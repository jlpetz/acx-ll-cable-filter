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
- `reference`: the reference's timeline (below, "The reference").
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
- `recorder`: the chain recorder (below): what the chain got and gave live, block by block.
- `handover`: handing a ring's producer to the audio thread and back, lock-free (the latency test's
  tap and the chain recorder).
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
   loopback onto the reference's timeline (a FIFO of about 500 ms; "The reference" below). A
   failing loopback or keep-alive drops the reference for the rest of the run (dithered zeros, a
   warning), and the mic path carries on.
   Then the hand-overs (`handover.rs`): the latency test's tap and the chain recorder are taken
   from their command rings, before this wake's blocks, and handed back when they're done.
3. While the mic FIFO holds ≥ 480:
   - Take one block.
   - **Reference:** the timeline serves 480 samples (±1 when its loop slips) for this block, with
     zeros for any that haven't arrived, and `SlipResampler::resample` makes them 480 ("The
     reference" below).
   - `chain.process_timed(mic, ref, out, qpc_now)`.
   - **Chain recorder**, while one is armed: the block's mic, reference and output, and its facts,
     into its ring ("The chain recorder" below).
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
       (one coarse drop per event).
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
- Reference lead target: one loopback period + 2 ms to start with; it can rise in the first 2.5 s
  and when underflows persist, up to 50 ms, and comes back down after five minutes under ("The
  reference" below).
- Both are visible in telemetry (`reference_fill_ms` is the reference's lead, against
  `reference_target_ms`).

**Stop:** set the flag and wait for the run to end (≤ 1 s); the run stops and releases its streams
in reverse order and the thread goes back to waiting. MMCSS and COM stay with the thread until the
engine is dropped.

## The reference (`reference.rs`, TODO 10)
AEC3 converges on the echo path, a mic sample's index less that of the reference sample served
with it, and holds as long as that stays put. The first design held a FIFO at one loopback period
+ a block, read by a PI loop on its fill. On the dev box's cables, whose mic delivers 0, 480 or 960
frames a wake and whose loopback delivers up to 1440, with a 10-30 ms swing between them, it moved
the path several times a second: a short FIFO zero-filled up to the target, an overfull one dropped
whole blocks, and the loop slipped after the swings (330 samples in 10 s against 9 ppm of real
drift). AEC3 kept 1-6 dB of its own ERLE. The timeline replaces it:
- **A virtual read position.** Each block consumes its 480 (±1) samples whether they've arrived or
  not. Missing ones are served as zeros and owed, discarded when they come, so the timeline stays
  whole. A reference gap counts a block that was served zeros that way.
- **The lead, from the stamps.** The loop's input is the lead: the mic block's capture time (its
  `StampMap`) less the loopback's tap time of the sample served with it (the loopback's own
  `StampMap`). It sees only the two clocks, so bursts don't reach it. On the cables the loopback's
  stamps jitter by 0.05 ms and the mic's are in 0.5 ms steps within ±1 ms. `StampMap` measures each
  stream's scatter (how far each stamp lands from where the one before put it); past 3.5 ms RMS, a
  driver that stamps at delivery or stamps that are noise, it stops trusting them for good.
- **Without trusted stamps** (on either stream) the lead is timed by when the packets were
  delivered, for the rest of the run (in the start-up the timing can still change either way, and
  the start-up starts over). Delivered at the mic's wakes, that reading swings with the bursts and
  moves with the drift only by a whole packet once per beat of the two clocks (33 s at 300 ppm,
  5.5 min at 30 ppm). So then nothing slips: the path drifts at the clocks' rate, and each step of
  the count is put back in a jump of a packet once a 3 s mean of the lead is 0.6 of a packet off
  (0.9 the other way from the last step: the mean can sit at half a packet while the count steps).
- **The loop.** Slips at the drift, a least-squares slope of the lead without our own moves over
  about 20 s (trusted after 10 s, once its standard error is within 10 ppm), plus an offset
  correction (10 ppm per sample) only outside a ±0.5 ms dead band, until back within 0.125 ms. At
  most one slip a block (1000 ppm). Blocks the loop coasts through (no fresh delivery, a reading
  left out) still count as time for the fit, so a silence's drift isn't a step in it.
- **The target.** One loopback period + 2 ms: what steady delivery needs at the worst phase of the
  clocks. A take's need is the least lead that would have served it whole. Only fresh takes count
  (the loopback delivered within two periods and 5 ms), and only once the loopback delivers again: a
  stall's first wakes are fresh, but need more each wake, and silence without the keep-alive says
  nothing about the delivery. A need counts once three wakes have reached it. In the first 2.5 s
  (AEC3 hasn't converged) the target rises, a jump each time, to that need plus 3 ms. After that
  only when underflows persist: a leaky count of short takes (10 s time constant) reaching 60, 6% of
  them for good or 0.6 s in a row (timed by delivery 200: a step of the count is put back first),
  to what the last 10 s needed plus 3 ms. That matters for a drifting bursty loopback: its phase
  against the mic's wakes sets its need, the start-up sees one phase, and at the worst one 13-20%
  of its blocks were short for 30 s of every 100 (at 100 ppm), costing AEC3 3-4 dB throughout; one
  raise covers that phase for good. A jump costs AEC3 5-10 s; a few owed blocks in a hundred about
  1 dB. The target comes back down, in one jump, once the need has stayed 3 ms or more under it for
  five minutes (longer than a drifting loopback's good phase down to about 15 ppm), and it's capped
  at 50 ms. The cap must stay under the echo path's own delay: the loopback taps the mix ahead of
  the speakers, so the echo arrives the output's latency (and the air's) after its reference, and a
  lead beyond that puts the echo before its reference (AEC3's delay estimate then reads 0). The
  cables' echo path is 60 ms; a bursty cable's lead ends at 23-33 ms. Nothing measures the echo
  path's delay yet, so a target over 30 ms (`REFERENCE_TARGET_HIGH_MS`) is flagged where it's
  shown (the GUI readout, `run`'s status line, the quality report's warnings).
- **Jumps**, counted as reference coarse drops after the start-up: the alignment at the first
  reading (either way: discarding, or zeros before the loopback's first sample), a centring at the
  start-up's end when the low-passed lead is off by more than 0.125 ms, a change of target, and a
  broken timeline. A break is a step of the lead past 4 ms (or 2.5 times the stamps' jitter) from
  the low-pass, in the median of the last 9 readings and in 7 of them: the mic losing more than the
  100 ms it zero-fills, a device reset, a stamp that jumped. One jump undoes it, by the mean of the
  readings past it, and the drift fit starts over (the readings after a step get it right to about
  ±0.3 ms on the cables' bursty mic, and the residual would be a step in its line). A reading
  twice that far from the low-pass is left out of it. Timed by delivery a break is a step past
  30 ms, undone in one jump and centred 2.5 s later. Loopback packets lost past the position fill,
  and silence without the keep-alive, need no jump: a loopback stamp more than 4 ms (or 4 times its
  scatter) from its place on the timeline is suspect, and once the next stamp agrees (or the device
  position jumped by about as much) zeros go in where the samples should have been: by the
  position's count when it agrees, else by the stamps at the loopback's rate. A stamp that's off on
  its own is left out.
- Telemetry: `reference_fill_ms` is the low-passed lead, `reference_target_ms` its target,
  `reference_stamped` whether the stamps time it, `reference_drift_ppm` the loop's drift estimate.

Replayed through the pipeline, the four 30 s traces from the cables (`live_probe.rs`;
`pipeline/tests/timeline.rs`) went from 206-283 gaps, 205-282 drops and up to 2021 slips to 0-3
gaps (in the start-up), no drops, 1-2 slips, and no move of the path and no block short after 3 s,
at a lead of 13.5-23.4 ms. Behind AEC3 offline (the quality test's turns, the echo path 25-100 ms
behind the loopback), AEC3's own ERLE is within 3 dB of the same audio delivered steadily wherever
the lead fits in the echo path, and the echo removed where only the far end talks the same; a bursty
loopback's 27 ms lead in a 25 ms echo path isn't causal (AEC3's delay estimate reads 0 and its ERLE
falls 6 dB). The synthetic scenes there test bursts, drift up to ±300 ppm, stalls, losses, silence
without the keep-alive, late wakes, a late loopback, streams without stamps and stamps that are off
or noisy; `NF_SEED` plays them with another seed.

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

## The chain recorder (`recorder.rs`, TODO 10 and 14)
What the chain was fed live and what it gave, block by block, so that a live result can be told apart from the chain's own
behaviour and rerun offline exactly (`Engine::record_chain`; `noise-filter quality --save` writes it):
- **On the audio thread**, after each block's chain call (`Pipeline::record`), one item into a pre-allocated rtrb ring (400
  blocks, 4 s, about 2.4 MB; its pages are touched when it's armed): the mic block and the reference block exactly as the chain
  got them (the mic after the dither; the reference after the timeline, the slip resampler and the dither), the chain's output
  block (before the output's drift loop resamples it for the device), and the block's facts, `ChainBlock`:
  - the run's block index, the mic block's capture QPC (by its stamps, NaN without), whether the mic's stamps are trusted, the
    mic stream index of its first sample, the wake that delivered it, and the mic samples still queued (a burst or a late wake);
  - the mic's moves since the block before: samples its FIFO discarded (it overflowed: the loop hadn't run for 200 ms), and device
    position it skipped without zero-filling (a gap over 100 ms; the stream index doesn't count it);
  - AEC3's own delay and ERLE estimates after the block (cached by sonora: a reference, no work);
  - the chain's live settings after the block (`nf_core::controls::ChainParams`: on/off, passthrough, floor, gate; a dozen
    relaxed loads), so a change during the recording (an `Engine::apply`, a latency test) shows;
  - the reference's take, `ReferenceFacts`, which `Reference::take` fills in as it goes (a few stores a block, always): the
    timeline index it read from (the mic index less it is the echo path as the engine holds it), this block's lead reading and
    the loop's low-passed lead, the target, the drift, whether the stamps timed it, whether the loopback was fresh and
    delivered at this wake, the samples buffered past the read position, the samples consumed (480, or 479 and 481 at a slip),
    the zeros served owed and missing, and how far and why the read position jumped (aligned, centred, broken, stepped,
    retargeted; and restarted, unstamped, coasted, no reading). Block to block the read index moves by what the last block
    consumed plus this block's jumps, and the mic index by 480 plus what the mic FIFO discarded, nothing else
    (`pipeline/tests/recorder.rs` checks both over bursts, stalls, a mic stall past its FIFO, a broken timeline and delivery
    timing).
  A full ring loses the block: no allocation, lock or panic path, the indices skip it, and the audio thread counts it
  (`ChainTx`). Each item carries the count before it, and the recorder's thread reads the count as it stops, so blocks lost
  after the last one recorded count too (`lost_blocks`). The no-allocation test runs with the recorder armed, its ring full for
  a while.
- **The hand-over** is the latency test's tap's, made generic (`handover.rs`): each run has a port (control side) and an end
  (audio thread) of two one-slot rings, for the producer going in and coming back (a `Handed`: the tap's producer, or the
  recorder's `ChainTx`, its producer and the lost count). A producer whose consumer has gone is handed back, never dropped on
  the audio thread, and freed on the control side (by the stop that waits for it, up to 500 ms; else at the next arm or when
  the run stops).
- **Arming** (`recorder::arm`), one recorder at a time. With a live run the producer goes to its command ring and the run
  takes it at its next wake. Without one (stopped, starting, waiting to retry) it waits in `Inner::recorder`, and the next
  start puts it in its run's command ring before handing the run over, so the run's first wake takes it before the first
  block: the recording then holds the chain's whole history after the warm-up (`ChainRecording::reproducible`). One armed while
  a start is opening its streams goes to the run once it's up (its first blocks may come first). A start that fails passes it
  on: its run, ending without having taken it, hands it back (`handover::give_back` in `run_job`, before the start's join
  returns), and the start keeps it for the next (`recorder::run_failed`), unless it was stopped meanwhile. Only a start that
  got no audio thread, or whose thread hung or died, loses it (the recorder then records nothing more).
- **Draining:** the recorder's own thread (normal priority) moves the ring into memory every 20 ms, up to `max_seconds` (at most
  15 minutes; about 0.6 MB a second), and ends when stopped, at the limit (`truncated`), or once the producer has gone: the
  run ended (a stop or a restart; `run_ended`). The recording doesn't follow a restart: the new run's chain starts afresh.
  `ChainRecorder::stop` stops it and waits for the producer to come back.
- **Rerunning it offline:** the engine builds its chain as `nf_stages::build_chain` does and warms it up with three dithered
  silent blocks (`audio::warm_up`, public as `nf_engine::warm_up_chain`) before the first block, and the recorded mic and
  reference are dithered already. So a chain built with the run's processing settings, warmed up and fed the recording block by
  block reproduces its output sample for sample (`the_live_chain_reruns_offline_sample_for_sample`, AEC3 and RNNoise behind
  bursty delivery with a loopback stall; without the warm-up it doesn't). `noise-filter process --warm-up` does this with the
  saved WAVs. A recording armed while running, or with blocks lost, starts from a state the rerun doesn't have; one whose
  settings changed matches a rerun with one config only up to the change (`ChainRecording::settings_changed_at`).
  `reproducible` checks all three.

## Testing (no devices; CLAUDE.md)
- Unit-test:
  - the FIFO ring;
  - the slip/drift integration, with a simulated device clock;
  - the latency-readout arithmetic;
  - the QPC interpolation;
  - the report statistics.
- `#[ignore]` tests may enumerate devices; never open streams in tests. One exception, outside
  every plain `cargo test`: the live probe (`live_probe.rs`), built only with the `live-probe`
  feature, ignored, and doing nothing unless `NF_LIVE_PROBE` is set, which the owner runs to trace a
  device's delivery for the reference's replays.
- The audio loop's logic should be split so its per-block bookkeeping can be driven by a fake
  stream in a test: a trait over capture/render that the WASAPI wrappers implement.
