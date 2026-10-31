# Engine spec (nf-engine, v0)

Read with `architecture.md`, and the API notes in `doc/dev/api/` (wasapi, win32-system, rlsf-rtrb,
sonora-nnnoiseless). The public API (`crates/nf-engine/src/api.rs`) is fixed; implement behind it.

## Modules
- `devices`: `list_devices()`, opening endpoints by id, the default-endpoint lookup, the
  `DeviceInfo` mix formats.
- `stream`: wrappers that open one stream and convert at the edge.
  - `CaptureStream`:
    - event-driven shared mode;
    - optional IAudioClient3 minimum period;
    - optional RAW (`StreamOption::Raw` when the device supports it);
    - autoconvert to 48 kHz when the mix rate isn't 48 kHz (no IAudioClient3 then);
    - downmix to mono by averaging channels;
    - handles BufferInfo flags: `silent` → zeros, `data_discontinuity` → glitch counter,
      `timestamp_error` → don't use that stamp;
    - exposes per packet: frames, device position (index) and QPC time (100 ns).
  - `RenderStream`:
    - event-driven or polled shared mode;
    - `padding()`, `write(mono)` upmixing to the device's channels;
    - `AudioClock` position for the play-out time.
  - `LoopbackStream`: shared-mode loopback capture of a render endpoint.
  - `KeepAlive`: a silent render stream on the reference endpoint at its default period, so its
    loopback never stops.
- `audio`: the audio thread (below).
- `control`: `Inner` (telemetry, status, the command channel, the audio-thread handle),
  `start/stop/apply/status`, and status messages worded for the user. The control thread owns:
  - a device-change listener (`DeviceEventCallbacks`) that restarts on removal, a followed default's
    change, an explicit reference's arrival, or a property change of a lost reference;
  - a timed retry with backoff (0.5 s doubling to 10 s) after a fault or a failed start with a
    transient cause. The backoff starts over only after a run has stayed up 30 s, so a mic that
    opens but sends nothing backs off instead of restarting every 2.5 s;
  - while a run carries on without its echo reference (a reference fault is non-fatal), a slow
    check that a loopback opens there again (15 s doubling to 5 min), which restarts to re-attach
    it: a format change or an exclusive-mode app sends no device event;
  - the log streak: a failure or fault repeated with the same message is logged at attempts 1, 2,
    4, 8...
- `latency_test`: below.
- `timing`: QPC helpers (`now_100ns()`, conversion to ms).

## The audio thread (one, MMCSS "Pro Audio", kept between runs)
The thread (`audio::Worker`) is spawned once and idles between runs: every start, restart and
retry hands it the next run. A new thread would pay easyfft's per-thread FFT state again, which
easyfft leaks by design (about 22 KB a thread), so a retry every 10 s would leak all day. When a
run doesn't stop within 1 s (stuck in a driver call), the control side gives that worker up to it
and the next start waits (`DriverBusy`, transient) until the stuck run has ended, then gets a new
thread.

Once, when the thread starts:
1. `initialize_mta` (a failure fails each run it's given);
2. `AvSetMmThreadCharacteristicsW("Pro Audio")` (record failure in the status and continue);
3. **no FTZ/DAZ** (Rust treats a persistent MXCSR change as UB): instead add `nf_core::rt::Dither` at
   1e-9 (about -180 dBFS) to every mic block and every reference block, zero-filled gaps included.
   That prevents sonora's denormal stall (sonora #34) and denormals in RNNoise's high-pass on exact
   digital silence;
4. `audio::prepare_rt_thread()`: `nf_stages::warm_up_fft_thread()` first, so easyfft's leaked state
   lands on the system heap, then `nf_mem::RtScope::enter()` for the thread's whole life. Keep the
   order: inside the scope the leak would take the locked pool.

For each run:
- Resolve the devices (cheap, so a run waiting for a device fails before building anything), then
  build the stages *inside the scope* (`RnnoiseStage::new`, `Aec3Stage::new` when AEC is on),
  `Chain::new` and `activate`.
- Open the mic, output, loopback and keep-alive streams.
- If `start()` has given up meanwhile (a driver slower than the start timeout), stop here: a newer
  run may already have the devices. The same check comes again before the streams start.
- Prefill the output to its target with zeros. Warm up with 3 silent blocks through the chain.
- Record `nf_mem::thread_scope_allocs()` as the baseline.
- Signal "running" to `start()` through a one-shot channel (or report the error).

Loop (wait on the mic's event handle, timeout 100 ms → treat as a stall: count it and continue;
after 2 s with no packets, report an error and stop):
1. **Mic:** drain every available capture packet into the mic FIFO (a fixed `Box<[f32]>` ring,
   about 200 ms), remembering the QPC time of the first un-consumed sample (from packet
   `timestamp` + offset/fs).
2. **Reference**, only on a wake that will process a block: top up the keep-alive and drain the
   loopback into the reference FIFO (about 500 ms). If it's empty for this block (no keep-alive
   and silence), zero-fill and count `reference_gaps`. A failing loopback or keep-alive drops the
   reference for the rest of the run (dithered zeros, a warning), and the mic path carries on.
3. While the mic FIFO holds ≥ 480:
   - Take one block.
   - **Reference slip:** `ref_slip = ref_drift.update(ref_fill)`, where the fill counts what the
     loopback has produced since its last packet (at most a period); pop
     `ref_slip.consumed_len()` samples and `SlipResampler::resample` them to 480.
   - `chain.process_timed(mic, ref, out, qpc_now)`.
   - **Output slip:** `out_slip = out_drift.update(output_fill)`, where the fill is frames written
     (and pending) minus the device position carried forward to now (at most a period), calibrated
     to the padding by a one-period window over the sawtooth the device's period steps make
     (`FillOffset`, pipeline.rs). It moves smoothly where the padding moves in packet steps, so the
     latency holds still as the two clocks beat. Resample to `out_slip.produced_len()`, and write
     to the render stream (it must fit: write as much as fits and keep the rest pending; never
     block).
   - **Coarse corrections** count this wake's backlog (the whole mic blocks still queued, written
     before the device needs more):
     - the first block sets the starting fill: zeros up to the target, not an underrun (the output
       starts before the mic);
     - padding under a period, and the fill with this block and the backlog still short of the
       target: an underrun. Zeros top the device up to the target once this wake's blocks are
       written; the loop restores the rest. A late wake whose blocks cover it gets no zeros;
     - more than two blocks over the target, whole blocks are dropped back to within half a block
       (one coarse drop per event). The reference FIFO does the same, leaving out what the mic
       backlog will take, and its first block aligns it from half a block over.
     - After a correction, `DriftController::reset` at the fill the next wake will read.
   - **Latency readout** (if enabled):
     `t_play = clock.qpc + (frames_written_total_before_this_block - clock.pos_frames) / fs_out`
     (the position from `AudioClock::get_position`, read once per block);
     `latency = t_play - t_capture_of_block_first_sample`.
     Push it (in ms) to `RollingStats<100>`.
   - **Telemetry:** meters (dBFS, through display ballistics: a 1 s peak hold falling at 20 dB/s,
     and a 300 ms power-domain RMS), clip latch (`clip_latched |= clipped`), near-clip, speech
     probability, and the noise stage's reduction (`noise_reduction_db`: its output against its
     latency-aligned input, the floor mix and gate included).
   - **Timing:** whole-block µs into `RollingStats<100>`; AEC and noise ticks.
4. **Once a second (every 100 blocks):**
   - publish p50/p99/max timings, latency median/p95, drift ppm, fills (ms), slip counts and AEC
     stats (`chain.stage_stats()`);
   - publish `rt_heap_allocations = thread_scope_fallbacks() - baseline` (must stay 0; log once,
     from the control thread, if it grows) and `rt_pool_allocations_per_s` (the per-second growth of
     `thread_scope_allocs()`; nonzero with AEC3, because sonora 0.2.0 allocates per block, served
     O(1) from the locked pool).

Never allocate, lock, log or make blocking calls inside the loop:
- Telemetry goes through atomics.
- Commands (stop, rebuild) arrive as an `AtomicBool`/`AtomicU8` checked once per wake-up.
- Config changes need no command: they live in `ChainControls`, written directly by `apply`.

**Targets:**
- Output render target = one render period + one block + `io.output_margin_ms`, measured before
  each block's write. (Whether one period + the margin is enough is to be measured on a real
  cable: it would take a block, 10 ms, off the latency.)
- Reference FIFO target = one loopback period + one block + 2 ms.
- Both are visible in telemetry.

**Stop:** set the flag and wait for the run to end (≤ 1 s); the run stops and releases its streams
in reverse order and the thread goes back to waiting. MMCSS and COM stay with the thread until the
engine is dropped.

## WASAPI hot-path rules (doc/dev/api/wasapi.md)
- `read_from_device` returns one packet per call: loop until it returns 0 frames. Read into
  preallocated buffers sized for the largest packet.
- The crate copies data even for `flags.silent`: zero-fill those packets yourself.
- `data_discontinuity` is never set in practice: detect gaps from `BufferInfo.index` jumps.
- Never use `read_from_device_to_deque` on the audio thread.
- The endpoint-loopback event doesn't fire during silence: poll the loopback and keep-alive streams
  from the mic's event loop. Don't wait on their events.
- wasapi logs at trace level on every read and write: the app sets `log::set_max_level(Info)` or
  below, so those calls cost only an atomic load. Never format errors on the audio thread: keep the
  raw HRESULT and report it through an atomic, worded later on the control thread.
- `set_ducking_preference(true)` *opts out* of ducking (the Windows parameter is `optout`).
- Order: `set_properties` (category, RAW) before initialise; `set_get_eventhandle` after
  initialise and before `start_stream`. After a failed initialise, get a fresh `AudioClient`.
- Every wasapi object is `!Send`: create, use and drop them on the audio thread (or the latency-test
  thread) after `initialize_mta()`. Only device id strings cross threads.

## Device rules
- `output` must be set: refuse to start with a clear message ("Pick the virtual cable to send the
  cleaned mic to").
- `output` must not be the same endpoint as `reference` (that would loop our own output into the
  echo reference): refuse with a message.
- A mic on the output's own device, when that device is a virtual cable (VB-Audio, Voicemeeter,
  VAC, by name): following the default, wait for another default (it's the cable's recording
  side: the filter would hear itself); picked by hand, a warning. A real card's mic and speakers
  share a device name too, and that's fine.
- The output being Windows' default playback device: a warning, worded for the speakers (it
  doesn't look like a cable: the cleaned mic plays out loud) or for a cable (every app's sound goes
  into the filtered mic).
- The reference is only needed when AEC is enabled at start or later. Open the loopback whenever a
  reference device is set and AEC is enabled. If AEC is switched on later, rebuild the streams; a
  short restart is acceptable.
- Call `set_ducking_preference(true)` on every session we open.
- Format: request 48 kHz f32 at the device's channel count. With the mix rate at 48 kHz, use it
  directly (low-latency path allowed). Otherwise use autoconvert (no IAudioClient3).

## Latency test (`latency_test.rs`)
Preconditions: the engine is running. `probe_device` and `capture_device` exist, and
`capture_device` isn't the mic.

1. **Setup:**
   - Set `controls.passthrough = true` (unless `through_model`).
   - Ask the audio thread, through an atomic flag plus a pre-allocated rtrb ring of
     (QPC-stamped mic blocks), to tap its mic input. Each item is the block's first-sample QPC and
     480 samples. Capacity covers the whole test: repetitions × (sweep + 2 gaps) + 1 s.
2. **Test thread** (MMCSS "Pro Audio" while running):
   - opens a render stream on `probe_device` and a capture stream on `capture_device`;
   - builds `exp_sweep(48000, 100, 16000, 1.0, 0.01, 0.25)` and
     `probe_train(sweep, repetitions, 0.5)`;
   - writes the train to the probe device, keeping its buffer topped up, and records each sweep
     start's play-out QPC (from the probe stream's AudioClock);
   - captures `capture_device` (QPC-stamped) into a Vec, while draining the tap ring into a Vec;
   - stops when the train has played plus 1.5 s;
   - publishes progress fractions while running.
3. **Analysis** (this thread, after recording):
   - `find_sweep_arrivals` on the tap stream and on the output capture stream. Map each arrival's
     sample position to QPC with that stream's (sample index → QPC) stamps (linear interpolation
     between packets).
   - `app_ms[i] = t_out[i] - t_tap[i]`;
     `input_path_ms[i] = t_tap[i] - t_probe_start[i]`;
     `end_to_end_ms[i] = t_out[i] - t_probe_start[i]`.
   - Keep repetitions whose arrivals have `confidence_db ≥ 15` in both streams and lie within
     ±50 ms of the median.
   - Fill `LatencyReport`. Its `conditions` lists the engine periods, RAW, autoconvert and the
     device names.
   - Add a warning when `valid < repetitions`, or when the spread is > 2 ms.
4. **Restore** the passthrough and tap flags (also on cancel or error).

Mode differences: Acoustic means the probe plays on the speakers. Warn if AEC is on (it's bypassed
in passthrough anyway) and suggest a quiet room. The maths is the same.

**Cable only** (`cable_latency_test`, no engine): the same train and the same play-and-record loop, blocking the calling thread
(COM MTA; MMCSS while streaming, dropped before the analysis). Each sweep start's play-out time (the probe's AudioClock) to its
arrival in the capture (QPC stamps), matched, filtered and summarised as above, into a `CableLatencyReport` with both streams'
formats and periods.

## Play and record (`play_record.rs`)
`play_record` plays 48 kHz mono buffers into one or more render endpoints (each after a lead of silence, about 1 s) and records
one capture endpoint until the longest has played plus a tail (about 2 s), blocking the calling thread (COM MTA; MMCSS while
streaming). The renders get 200 ms buffers kept 100 ms full; capture gaps by device position are zero-filled and counted, and
a render found empty before its track is written is an underrun. Each track has a role (`TrackRole`: the probe, or an echo
test's far end) that its errors are worded for. A running `Engine` carries on meanwhile: `noise-filter quality` measures it that
way. The latency tests share its recorder and player. Both blocking calls (this and `cable_latency_test`) want a worker thread:
on an STA (UI) thread COM is single-threaded already, and they fail, saying so.

## Testing (no devices; CLAUDE.md)
- Unit-test:
  - the FIFO ring;
  - the slip/drift integration, with a simulated device clock;
  - the latency-readout arithmetic;
  - the QPC interpolation;
  - the report statistics.
- `#[ignore]` tests may enumerate devices; never open streams in tests.
- The audio loop's logic should be split so its per-block bookkeeping can be driven by a fake
  stream in a test: a trait over capture/render that the WASAPI wrappers implement.
