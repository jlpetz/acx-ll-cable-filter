# Engine spec (nf-engine, v0)

Read with `architecture.md`, and the API notes in `doc/dev/api/` (wasapi, win32-system, rlsf-rtrb,
sonora-nnnoiseless). The public API (`crates/nf-engine/src/api.rs`) is fixed; implement behind it.

## Modules
- `devices`: `list_devices()`, opening endpoints by id, the default-endpoint lookup, the
  `DeviceInfo` mix formats.
- `stream`: wrappers that open one stream and convert at the edge.
  - `CaptureStream`:
    - event-driven shared mode, or exclusive mode when `io.mic_exclusive` asks ("Exclusive mode" below);
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
  - `Output`: the output, a polled shared `RenderStream`, or with `io.output_exclusive` an event-driven exclusive stream
    (`ExclusiveRenderDevice` behind `exclusive::EventRender`), either way a `RenderSink` to the pipeline.
  - `LoopbackStream`: shared-mode loopback capture of a render endpoint.
  - `KeepAlive`: a silent render stream on the reference endpoint at its default period, so its
    loopback never stops. Not at IAudioClient3's small period (TODO 13's real-hardware notes): that switches every
    stream on the speakers, and shortens their own output path about as much as the loopback's packets.
- `exclusive`: exclusive mode apart from WASAPI: the period and format choices, the fall-back to shared mode, and the
  event-driven output's ring (below, "Exclusive mode").
- `out_target`: the adaptive output target (below, "The output target").
- `reference`: the reference's timeline (below, "The reference").
- `echo_guard`: the echo-path guard, the mic's hold for an echo that comes before its reference (below, "The echo-path
  guard").
- `audio`: the audio thread (below).
- `models`: the noise model by the config's name, and the one chain constructor (below, "The inference worker").
- `control`: `Inner` (telemetry, status, the command channel, the audio-thread handle),
  `start/stop/apply/status`, and status messages worded for the user. The control thread owns:
  - a device-change listener (`DeviceEventCallbacks`) that restarts on removal, a followed default's
    change, the default playback device moving to or from the output, an explicit reference's
    arrival, or a property change of a lost reference;
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
- `flight`: the flight recorder (below): the last seconds of the blocks' facts, written out when something goes wrong.
- `csv`: the recorders' blocks as CSV (`chain_blocks_csv`: `chain_blocks.csv`, and the flight recorder's rows).
- `handover`: handing a ring's producer to the audio thread and back, lock-free (the latency test's
  tap and the recorders).
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
- Resolve the devices (cheap, so a run waiting for a device fails before building anything).
- Open the mic, output, loopback and keep-alive streams (opened, not started).
- Build and activate the chain *inside the scope* with `models::build_chain` (AEC3 when AEC is on,
  then the noise stage around RNNoise or a model pack, a pack at the depth `choose_model_depth` picks from
  how the mic opened: "The inference worker" below; `Chain::activate` checks each stage against the host
  profile, activates it at 48 kHz in 480-frame blocks on this thread, and caches its latency). The offline
  tools use the same constructor, so the live chain and a rerun can't drift apart. RNNoise's `activate`
  starts its spare maker, a thread of the model's own at normal priority (no MMCSS: it does no audio),
  which keeps two pristine model states for its resets, so a reset on this thread doesn't allocate (TODO 4;
  nf-stages `rnnoise.rs`); the one known exception, a third real reset within 100 ms of two others, finds
  none, rebuilds the state in place (ten pool allocations) and is counted (`RnnoiseModel::rebuilds`).
  Dropping the chain stops and joins it, and what it held is freed by the dropper.
- If `start()` has given up meanwhile (a driver slower than the start timeout), stop here: a newer
  run may already have the devices. The same check comes again before the streams start.
- Prefill the output to its target with zeros. Warm up with 3 silent blocks through the chain.
- Record `nf_mem::thread_scope_allocs()` as the baseline.
- Signal "running" to `start()` through a one-shot channel (or report the error).

Loop (wait on the mic's event handle, and an exclusive output's with it (`WaitForMultipleObjects`, the output's first),
timeout 100 ms; 100 ms without the mic's event is a stall: count it and continue; after 2 s with no packets, report an
error and stop):
0. **An exclusive output's event**, when it fired (or has since: a zero-timeout check): move one period from its ring to
   the device ("Exclusive mode" below). Again after each block of the wake, so the longest answer is a block's work.
1. **Mic:** drain every available capture packet into the mic FIFO (a fixed `Box<[f32]>` ring,
   about 200 ms), remembering the QPC time of the first un-consumed sample (from packet
   `timestamp` + offset/fs).
2. **Reference**, only on a wake that will process a block: top up the keep-alive and drain the
   loopback onto the reference's timeline (a FIFO of about 500 ms; "The reference" below). A
   failing loopback or keep-alive drops the reference for the rest of the run (dithered zeros, a
   warning), and the mic path carries on.
   Then the hand-overs (`handover.rs`): the latency test's tap, the chain recorder and the flight
   recorder's ring are taken from their command rings, before this wake's blocks, and handed back
   when they're done.
3. While the mic FIFO holds ≥ 480:
   - Take one block.
   - **Reference:** the timeline serves 480 samples (±1 when its loop slips) for this block, with
     zeros for any that haven't arrived, and `SlipResampler::resample` makes them 480 ("The
     reference" below).
   - `chain.process_timed(mic, ref, out, qpc_now)`.
   - **Chain recorder** and **flight recorder**, while armed: the block's mic, reference and output,
     and its facts, into each one's ring ("The chain recorder", "The flight recorder" below).
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
     Push it (in ms) to `RollingStats<100>`, and its budget ("The latency budget" below). It times the block's first
     *output* sample, so the chain's offset isn't in it.
   - **Telemetry:** meters (dBFS, through display ballistics: a 1 s peak hold falling at 20 dB/s,
     and a 300 ms power-domain RMS), clip latch (`clip_latched |= clipped`; the window's reset clears it, and `run`'s
     status line takes it, `Engine::take_clip`, a swap, so each line shows a clip since the last: TODO 12), near-clip, speech
     probability, and the noise stage's reduction (`noise_reduction_db`: its output against its
     latency-aligned input, the floor mix and gate included).
   - **Timing:** whole-block µs into `RollingStats<100>`; AEC and noise ticks.
4. **Once a second (every 100 blocks):**
   - publish p50/p99/max timings, latency median/p95 and its budget (each part's mean and p95, and
     the run's totals), drift ppm, fills (ms), slip counts and AEC stats (`chain.aec_stats()`, AEC3's
     `aec-telemetry/0` extension), and the output's events (`OutputEvents`, TODO 9: underruns and the silence an exclusive
     output's device played in our stream where its ring ran short, deficits and the zeros they wrote, coarse drops and the
     samples dropped, slips and their net, the fill calibration's re-anchors; `OutputEvents::between` gives a span's, which
     the quality and latency reports and `run`'s summary print, `OutputEvents::text` in one phrase); last, a publication
     count (`Telemetry::publishes`, `Release`), so a reader can wait for the next second's counters (`run`'s summary and
     `devices --check` do, before they stop);
   - publish `rt_heap_allocations = thread_scope_fallbacks() - baseline` (must stay 0; log once,
     from the control thread, if it grows) and `rt_pool_allocations_per_s` (the per-second growth of
     `thread_scope_allocs()`; 0 with the built-in stages since sonora 0.3.0, whose AEC3 allocates
     nothing per block; 0.2.0's made about 43 a block, served O(1) from the locked pool).

Never allocate, lock, log or make blocking calls inside the loop:
- Telemetry goes through atomics.
- Commands (stop, rebuild) arrive as an `AtomicBool`/`AtomicU8` checked once per wake-up.
- Config changes need no command: they live in `ChainControls`, written directly by `apply`.

**Targets:**
- Output render target = one render period + one block + `io.output_margin_ms`, measured before
  each block's write: 22 ms at a shared 10 ms period, 14 ms at an exclusive 2 ms one (the default). (Whether one
  period + the margin is enough is to be measured on a real cable: it would take a block, 10 ms, off the latency. In the
  event simulation it is for steady delivery and isn't for a bursty mic's missed wakes: TODO 13's detail, "Budget".)
  With `io.adaptive_output` (the default) it moves: "The output target" below.
- Reference lead target: one loopback period + 2 ms to start with; it can rise in the first 2.5 s
  and when underflows persist, up to 50 ms, and comes back down after five minutes under ("The
  reference" below).
- Both are visible in telemetry (`reference_fill_ms` is the reference's lead, against
  `reference_target_ms`; `output_fill_ms` against `output_target_ms`, both published each second).

**Stop:** set the flag and wait for the run to end (≤ 1 s); the run stops and releases its streams
in reverse order and the thread goes back to waiting. MMCSS and COM stay with the thread until the
engine is dropped.

## The inference worker (`models.rs`, `nf_core::noise::Pipelined`; TODO 5, 6)
`ProcessingConfig.noise_model` names the noise model: `"rnnoise"` (built in, the default) or a model pack's id, which needs a
build with the `onnx` feature (nf-ort, ONNX Runtime 1.31). `models::build_chain(controls, &processing, with_aec, thread)` is
the one constructor: the audio thread builds its chain with `ModelThread::Worker` inside its real-time scope (`open_session`),
and `process`, `quality`'s reruns (through `process --warm-up`) and `bench` with `ModelThread::Inline`. A missing pack fails
the start with how to fetch it (`python -I tools/fetch_models.py ID`); a pack that fails its checks fails the start too
(`StartFailure::Stage`); there's no silent fallback to RNNoise. Changing the model, or a pack's depth
(`ProcessingConfig.model_depth`, `--model-depth`), is a restart (`control::restart_needed`); the high-band policy
(`ProcessingConfig.high_band`) is live, like the floor and gate.

**The depth is automatic by default** (`model_depth` 0, `AUTO_MODEL_DEPTH`; TODO 5, ours, from the live A/B: depth 2 costs
10 ms, and depth 1 ran clean with the exclusive mic). `open_session` opens the streams first and then builds the chain, at the
depth `audio::choose_model_depth` picks: 1 when the mic delivers a block a wake, 2 when it bursts or hasn't been measured.
How it delivers: as an earlier run in this process measured it, when it opened the same way (`audio::MicOpening`: the
endpoint, exclusive or shared, the period, IAudioClient3's small period, RAW); unmeasured, a block a wake for the exclusive
mic at a period of a block or less (its 3.33 ms packets make a block at every third; a device whose exclusive minimum is
over a block, 512 frames say, brings two blocks in some wakes), and not known for any other. The measure is the run's own
wake counts (`Telemetry::wake_blocks`, `WakeBlocks::delivery`): from 1,000 wakes on, a block at a time when a worker one
block behind would have missed at most one block in 500 (one every 5 s at most), else bursts. The control side keeps the
last run's (the mic as it opened, and its delivery: `Inner::mic_delivery`), exclusive or shared, taken as each run ends
(`stop_locked`, before a restart's reset of the counters). So an exclusive mic that measured bursts (a long period, or a
thread often held up) gets 2 at its next start, and a change of how the mic opens (an I/O setting) starts unmeasured. The
depth is decided only at a start, never mid-stream: it is the noise stage's latency, so a change would move the output by
a block (a click), and a restart is a gap of its own, so the engine doesn't restart for it. A mic first measured in a run
gets its depth at the next start (any restart: a model change, a settings or device change). `--model-depth N` (or a config's 1-4) forces it; `--model-depth auto` puts it back. The status says which and why
(`EngineStatus::model_depth`, `ModelDepth::text`: "depth 1 (auto: the mic delivers a block at a time)"; `run`'s Engine
line), and `quality --save` writes the depth the start chose into `chain_config.json`, so the rerun matches. Offline
(`process`, `bench`) nothing delivers, and auto is 2, as before.

A pack's model runs on its own thread, `nf-inference`, because ONNX Runtime allocates from the C runtime's heap on every run
(about 3,700 allocations: `doc/todo/6-inference-memory.md`): CLAUDE.md's one exception to the real-time rule.
- **Set-up, on the worker:** `models::EngineHooks::enter` (MMCSS "Pro Audio" at `MmcssPriority::Worker`: normal priority,
  under the audio thread's Critical; refused, it falls back to HIGHEST, under the audio thread's own TIME_CRITICAL
  fallback, and the start carries a warning, `Warning::InferenceMmcss`); the pack loaded, checked and hashed (the model and
  the licence only; on the worker, so not on the audio thread or in its pool) and its session built from the verified
  bytes; `prepare` (32 frames of noise and 32 of silence, then everything zeroed, so the arena, the memory pattern and MLAS's
  per-thread buffers exist on the thread that runs); then `EngineHooks::steady` (`nf_mem::RtScope`, so whatever its steady
  state allocates in Rust comes from the locked pool and is counted: nothing, measured) and "ready". `open_session` waits
  up to 3 s for it (`READY_TIMEOUT`, under the 5 s start timeout; about 120 ms on the dev VM), looking every 100 ms at the
  run's command, so a stop or a start timeout ends the wait; a slow build fails the start with the model named.
- **Per block, on the audio thread** (`Pipelined::process_hop`, inside the noise stage), `D` = the depth (automatic: 1 or 2):
  take the result of the call `D` back (older ones ahead of it count as `late`, a later one at the front means it never
  came); push this block's job (a full ring is a `gap`, and the next job that gets in resets the model); `Thread::unpark`
  the worker (no allocation, no blocking: the host profile's one allowed system call); output that result, or the
  substitute (the clean input delayed by the same offset, 2400 samples for UL-UNAS, × its gain-matched `g`) and count a
  `miss`. Both rings are rtrb, 32 blocks each way (more than the mic FIFO's 20, so a stall's backlog never gaps), moved by
  value. Measured: 4.2 µs p50, 5.1 µs p99 a block, no allocation and no C heap call.
- **The deadline is the call `D` later, not a block's time.** This loop processes every complete block of a wake back to
  back, so a wake of `k` blocks (a 20 ms device period, VB-CABLE's bursts, a late wake, a stall's backlog) gives the worker
  microseconds for the jobs pushed earlier in it: with the worker caught up before the wake, it costs `max(0, k - D)`
  misses. Depth 2 absorbs two-block wakes (common on the cables: TODO 8's detail), at 10 ms more latency than depth 1
  (UL-UNAS: 60 ms live against 50); measured on the dev VM, wakes of 2 lose 49% of their blocks at depth 1 and none at 2,
  wakes of 3 lose 66% and 33% (`doc/todo/6-inference-memory.md`, "The pipeline's depth", with the expected rate per device
  mode). The loop counts each wake's blocks (`Telemetry::wake_blocks`: wakes of 1, 2, 3 and 4+), so a soak says what each
  depth would miss.
- **The start-up rule:** after any reset of the model (a stage reset, a gap, a failed run) the first `D` + ⌈1440/480⌉ = 5
  calls play the substitute (silence after a stage reset: `g` = 0), counting no miss. That is the stage's start-up region,
  which the chain doesn't play at a toggle-on: a stage whose latency is over a block stays on the slot's delay line (the same
  latency) for its start-up blocks and then crossfades in (`chain.rs`, `Mode::Starting`), so a switch-on has no silence and
  no dip. A toggle-on (reset, prime, process in one block) never counts a miss, and the inline executor follows the same
  rule: offline equals live bit for bit whenever the worker misses no deadline.
- **Telemetry:** `InferenceStats` (runs, misses, late, gaps, errors, the run time's p50/p99/max over the last 100, the worker's
  pool allocations and fallbacks, a refused MMCSS) is copied into `Telemetry` once a second (`publish_inference`, before
  the second is marked published); `run`'s status line shows `model misses N run P µs bursts B` when a worker exists, and
  a soak's summary has a `wakes` line.
- **Stopping:** the session drops with the chain: stop flag, unpark, join; the session is dropped on the worker.
- **Linking:** pyke's static ONNX Runtime always includes DirectML: `DirectML.dll`, `d3d12.dll` and `dxgi.dll` are
  delay-loaded (build.rs), and nothing registers that provider. Its C++ static initialisers run before `main`, ahead of the
  CPU gate, and it needs AVX2; hence the feature, off by default. Under it the hard working-set minimum is 128 MiB (64
  without).
- **The chain recorder** doesn't log the substitute's blocks yet, so a run with misses doesn't rerun exactly.

## Exclusive mode (`exclusive.rs`, `stream.rs`; TODO 13)
Each side on its own, both **on by default** (the owner, 2026-11-02: "by default apply the lowest processing default ...
some users may want to toggle it off for recording streams etc. Though as long as the final output is not exclusive that
shouldn't be a problem most of the time"; the final output, the cable's recording side that the apps read, stays shared):
`io.output_exclusive` (`--no-exclusive-output` turns it off) and `io.mic_exclusive` (`--no-exclusive-mic`).
The aim is the render side and the mic's delivery: in shared mode the audio engine's pass sits between us and the device
on each side (the cables' render side read 26 ms, the mic 16), and exclusive mode reaches the driver directly.
- **Opening:** the first of 32-bit float, 24 bits in 32, 16-bit that the device takes at 48 kHz and its mix's channel
  count (`is_supported_exclusive_with_quirks`, which also tries a plain `WAVEFORMATEX` and other channel masks), and
  `EventsExclusive` at `exclusive_period`: the smallest period from the device's minimum (`GetDevicePeriod`; never under
  2 ms) that divides the block, up to its default, else that minimum. VB-CABLE (the probe of 2026-11-01): 16-bit only,
  2 ms on its render side (96 frames), 3 ms on its capture side, which becomes 3.33 ms (160 frames: three to a block,
  where 144-frame packets would finish a block 1 ms late on average). A refused period (`AUDCLNT_E_INVALID_DEVICE_PERIOD`,
  `AUDCLNT_E_BUFFER_SIZE_ERROR`, `E_INVALIDARG`) tries the device's minimum next (VB-CABLE's capture side: the 3 ms the
  probe saw it take, should it refuse 3.33) and then its default; a format `IsFormatSupported` took and the initialise
  refused tries the next format (`exclusive::first_exclusive`). Each try is on a fresh client, and a driver that wants its
  buffer aligned (`AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`) is retried once at the size its failed client gives. When the check
  takes no format at all, every format is tried at the initialise anyway (`exclusive::formats_to_try`): the check can't say
  why it refuses (wasapi's quirks retry folds it into "unsupported", and Microsoft documents no other answer), while the
  initialise refuses a device another app holds, or one Windows keeps from exclusive control, with its own code, so the
  note names the real cause rather than "no format" (three initialises more for a device that really takes none).
- **Falling back:** a refusal (no format, the device in use, exclusive control not allowed, every period tried) opens the
  stream in shared mode as without the option, with `Warning::ExclusiveUnavailable`, which is a **note**
  (`Warning::is_note`, `EngineStatus::notes`), not a warning: with exclusive mode the default, a refusal is ordinary. It
  says which stream, why (`describe_refusal`: another app holds it, `AUDCLNT_E_DEVICE_IN_USE`; exclusive control turned
  off for the device, `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`; no format, `AUDCLNT_E_UNSUPPORTED_FORMAT`; the short period,
  `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`, `AUDCLNT_E_INVALID_DEVICE_PERIOD`, `AUDCLNT_E_BUFFER_SIZE_ERROR`; for the periods,
  "it refused every period tried, 2.00 ms to 10.00 ms (error ...)"; anything else by its code), that it works as usual with
  more latency (the A/B on the cables: 20-30 ms for the output, 10-20 for the mic, depending on the other stream's mode),
  and what to do: to get exclusive mode, close the app that holds it and restart the filter (or start the filter before
  that app next time), or tick "Allow applications to take exclusive control of this device" (Sound control panel, the
  device's Properties > Advanced) and restart; to stop it trying, turn the stream's option off (the window's Diagnostics >
  Latency switch, or `--no-exclusive-output` / `--no-exclusive-mic`: the engine doesn't know which front end shows it, so
  it names both). The window shows notes informational, `run` as "note:" lines. A shared open that fails then is the
  start's failure, as before: `AUDCLNT_E_DEVICE_IN_USE` there means another app holds the endpoint exclusively, most
  often now another copy of the filter (the window and a CLI command on the same device), and its message says so, with
  what unticking "Allow applications to take exclusive control" would cost (the filter's own streams shared too).
  `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED` has its own wording (only an exclusive open should get it).
- **Never on the default playback device** (`audio::output_exclusive`): an output that is Windows' default playback
  device opens shared even when exclusive is asked, as it did before exclusive was the default. Held exclusively, no other
  app could play sound at all (the call's own sound included), where shared it at least plays (into the mic, for a cable,
  which the warning is about). Its warning (`OutputIsSpeakers`, `CableIsDefaultPlayback`, with `exclusive_withheld`) adds
  that it runs shared until the default is fixed, and the stream's readout says why (`ExclusiveRefusal::DefaultPlayback`).
  A change of default to or from the output restarts the streams (`device_event_needs_restart`, `RunView::output_default`),
  so fixing the default brings exclusive mode back, and an exclusive output made the default gives it up.
- **The output is event-driven** (`EventRender`): WASAPI gives it two buffers of one period, and at each device event the
  client hands over a whole period while the device plays the other. The pipeline writes into a ring (100 ms) as it
  writes into a shared stream's buffer, and each event moves a period from the ring to the device. The ring is the
  padding, so to the pipeline it is a device that takes a period a pass, as the shared engine is, with a 2 ms period:
  the fill, `FillOffset`'s sawtooth (one period wide: the device holds one to two periods), the drift loop, the deficit
  and drop rules and the readout hold as they are, and the target follows the period. Polling with a buffer of a few
  periods would keep the one-event loop, but VB-CABLE was seen to take event-driven exclusive streams only, a polled
  exclusive buffer that runs dry replays stale audio, and how its padding moves is the driver's affair.
- **A short ring** at an event hands what it has and silence after it: an underrun the sink reports
  (`RenderSink::take_underruns`), and the silence is taken off the clock's position, so the pipeline's stream indices stay
  the frames it wrote and the readout stays right after a dry spell (which the shared path's clock can't promise). The
  pipeline counts the sink's underruns once a block however many events ran short, as the shared path counts a dry
  spell: a 40 ms mic stall, a dozen short 2 ms events, is one (not before the first block; the adaptive target rises).
  **A late answer** (the clock past everything handed, and 1.5 periods since the last answer: both, so a clock that leads
  its buffers can't make every event late) is an underrun too. More than 2 periods since the last answer proves it late
  (the device wants the next buffer within a period of its event, and every event is answered once), and the clock,
  extrapolated to now, counts its stale periods; between 1.5 and 2 only the position the device itself reported can show
  it, not one extrapolated: extrapolating a stalled clock's reading by up to a period counted a late answer that wasn't
  (TODO 13's soak 3, VB-CABLE's render clock stalling at about two minutes). Late answers are taken apart from the short
  ring's underruns (`RenderSink::take_late`, the `output_late_answers` counter, chain_blocks.csv's `out_late`), and they
  don't strike the adaptive target: a deeper queue can't make the thread answer sooner, a bigger device period can. The
  device played whatever its other buffer held, and
  those periods are taken off the clock the same way, as for a device whose stream moves on by them (one that writes the
  late buffer over the one playing instead would read a period high per late answer; unverified on VB-CABLE). The gap is
  timed on QPC from the stream's start (`EventRender::started`), so the first answer, which waits on the rest of the
  start, is caught late as any other is; counted from the device clock's first reading it was missed, and the stale
  periods then read as 2 ms each off the readout and the render side for the whole run. The start also answers the first
  event as soon as the streams are started, before the status is built. A device clock that can't say yet (its
  position reads 0 for a while after the start) leaves the check to the next answer it can, whatever that one's gap. The counts saturate and a late answer counts at most
  10 s of stale periods, so a garbage position can't overflow them.
- **The thread's second deadline:** a render event must be answered within a period (2 ms). The wait lists the output's
  event first, and the loop answers it at the top of each wake and after each block (0.1-0.3 ms of work), so the longest
  answer is one block's work plus the wake's own (reading the mic, servicing the reference). A stall is now timed by the
  mic (`StallWatch`: 100 ms with neither its event nor a packet), since the output's wake the thread every 2 ms. A wake
  the output's event made that finds the mic's packets waiting is no stall: a thread held up past 100 ms finds both events
  fired, and the wait reports the output's (`audio::mic_wake`; `pipeline/tests/flight.rs`).
- **The mic** in exclusive mode is an event-driven exclusive `CaptureStream`: a packet a period, read into the mic FIFO
  as any packet is; its stamps and device positions are GetBuffer's as in shared mode, so the reference's lead (read
  against the mic's capture stamps), the stamps' trust, the budget's mic delivery and the gap filling hold as they are.
  No RAW (exclusive mode has no Windows effects) and no IAudioClient3. The budget splits differently: a block's first
  sample is delivered with its 3.33 ms packet and waits for the other two, so mic delivery reads about 3.6 ms and the
  wait about 6.7; together they're the block and the driver's lateness, as before.
- **The mic's deadline:** an event-driven exclusive capture buffer is one period (WASAPI double-buffers it in the
  driver), so a packet not read within about a period (3.33 ms) is overwritten: mic audio lost, a capture glitch (the
  next packet's discontinuity or position gap, zero-filled). Shared mode's 40 ms capture buffer rides out the same
  delay. The thread reads the mic at each wake, and its work between the mic's events is a block (0.1-0.3 ms) and the
  output's answers, so what can miss it is the thread itself being held up (preempted, a page fault, a slow driver
  call): MMCSS's priority is what keeps it on time, the window minimised included.
- **The period guard** (`control::PeriodGuard`, `exclusive::next_floor`): both deadlines are the PC's as much as ours. A
  PC whose drivers hold the CPU past a period (DPC latency: some network, GPU and ACPI drivers hold it for milliseconds,
  and MMCSS can't preempt a DPC) makes the thread answer late, or lose the mic's packet, however light its work. So a
  stream that misses three deadlines in ten minutes of one run (the output's late answers, `output_late_answers`; the
  exclusive mic's capture glitches; the run's first 5 s don't count, as the start's own hold-ups would only come back)
  steps up to the next period that tiles the block (2, 2.5, 3.33, 5, 10 ms), by one forced restart (the flight
  recorder's notes say "the exclusive period guard"), and stays there for later starts on that endpoint while the process
  runs (`Inner::period_floors`, by endpoint id; `exclusive_periods` tries nothing under the floor, and a floor past the
  default is tried alone). Past a block's period the stream runs shared. Each start under a floor carries
  `Warning::ExclusivePeriodRaised`, a note that says which stream, at what, and points at LatencyMon. One click doesn't
  move it (a USB device plugged in, a driver's one-off), and neither do a short ring's underruns (the adaptive target's
  affair). Item 2 of the real-hardware list (the owner's go, 2026-11-03); untested on real hardware, where a PC with bad
  DPC latency is the test.
- **Formats at the edge** (convert.rs): 16-bit and 24-bit are written at the scale they're read at, rounded to the
  nearest step: a 16-bit mic passes a 16-bit cable unchanged in passthrough. No dither: the shared engine converts our
  float to a 16-bit cable without one too, the rounding error (-101 dBFS RMS) is under any mic's own noise, and a dither
  would put a -96 dBFS hiss on the gate's silence.
- **What it costs others:** an exclusive stream has the endpoint to itself. On the output cable that's nothing (only the
  filter plays into it, and the apps read the cable's other side, which stays shared); on the mic, no other app can record
  from that mic while the filter runs, so they should use the filtered mic, and someone who records the raw mic in another
  app too (OBS, a DAW) turns it off. A start whose device is held by another app (exclusive or shared) falls back, and the
  next start tries again. The options' help and the window's switches say both, and `--help` and the window's mic line
  the mic's deadline and the echo floor. Two copies of the filter on the same mic or cable (the window and a CLI command)
  can't both run now: the second's shared open fails on the first's exclusive stream, and is retried.
- **Volume (unverified):** an exclusive stream bypasses the audio engine, so the endpoint's volume (CABLE Input's playback
  slider, the mic's Levels slider) should apply only where the device has a hardware volume control; where the volume is
  the engine's software one it would stop applying. Unchecked on VB-CABLE and on a real mic (a live check is due before
  the help and the window's lines say so).
- **The echo floor:** the exclusive mic's blocks come fresher, so the reference's lead rose to 18.3 ms on the cables
  (12-14 shared), and the shortest echo AEC3 can cancel to about 20 ms (TODO 10's detail). Speakers whose output latency
  is under that would echo through; the echo-path guard (below) holds the mic back for them, only as long as their echo
  needs. `quality --test echo`'s default delay (25 ms) is over it.
- Telemetry `mic_exclusive`, `output_exclusive` (set at the start, cleared at the stop); `StreamInfo::exclusive` names
  the format, and `StreamInfo::exclusive_refused` why a stream asked for in exclusive mode runs shared (the reports'
  conditions say it with the stream).

## The output target (`out_target.rs`, TODO 13)
The fixed target: one period + one block + `io.output_margin_ms`. The block is spare for steady delivery and is what
bursts use: the drift loop holds the mean of the fills read before each write at the target, so a block that comes
later than the others finds that much less queued. With `io.adaptive_output` (on by default, the owner, 2026-11-02;
`--no-adaptive-output` keeps it fixed) the target follows what the delivery needs:
- **Measured:** each ordinary block's dip, how far its fill (before the calibration to the padding: frames written less
  the device position now, or the padding without a clock) is under its 1 s mean; each second's largest, kept for 120 s.
  The need is the largest over that window: a high percentile would let the rarest bursts through, and each costs an
  underrun. After a correction the mean starts again from the plain mean of 300 blocks, which aren't counted.
- **Lowered slowly, after a trial:** once 120 s have passed since the last move was decided, the target can go to the
  floor (one period + the margin: what the device's period requires) + the need + 1 ms, when that's at least 1 ms lower.
  Until proven (the run's start, and after a strike), a window that allows it starts a **trial** instead: the target
  holds where it is (the fixed target) for a second window (`AdaptiveTarget::on_trial`, published as
  `output_target_on_trial`). If that one passes without an underrun and allows a lowering too, the target is proven and
  comes down on its need; if not, the trial has failed and the next window that allows a lowering starts another.
  Proven, each lowering goes at once. Why (ours, 2026-11-02): the first window has seen the least of the delivery. The
  soak on the cables walked from 14 to 9.9 ms in 9 s on its first window's need, heading for 6.8, and ran dry there with
  the queue at 10.6 ms, so on a block 8 ms or more later than the rest (about 10 in the simulations: a missed wake; TODO
  13's detail, "Soak"); its end point then ran 17 minutes clean. A first stop short of the fixed target doesn't cover
  that: in the soak's conditions one halfway (10.1-10.4 ms) or at a period + a block (12 ms) ran short with a block 10 ms
  late while it held (a block more than a block late, with the fill under a period, is a deficit), where the fixed target
  rode out blocks up to 13.5 ms late. Held at the fixed target, the trial rides out what the fixed target does, and a
  hiccup in it fails the trial instead of costing a strike. The end point is the same, a window later;
  the price is the fixed target's latency for minutes 2 to 4. The target walks down a tenth of the way each second (at
  most 0.5 ms a second, at least a sample: 9 ms takes 53 s, all but the last 0.25 ms in the first 41), through
  `DriftController::set_target`, and the loop slips the fill after it, so nothing clicks. A walk, not a step: set in one
  step, the PI loop ran at its 1500 ppm limit, wound its integral up as the fill neared the target and carried it past,
  1.4 ms under the new target for 10 s (under the floor, in exclusive mode), just when the target is least proven; a
  steady walk overshoots when it stops (0.8 ms at 0.25 ms/s). The walk's slowing end lets the loop's integral wind down
  with it: in the simulations the queue's 1 s means stay above where the walk ends (within 0.1 ms; they run up to 1 ms
  ahead of or behind the moving target meanwhile).
- **Raised at once:** an underrun (the device or the exclusive ring ran dry, or a deficit) puts it back to the fixed
  target, and the deficit's zeros fill the device to it at that block. Under the fixed target that's a **strike**: the
  fixed target might have covered it. Its need (its dip, and at least how far under the fixed target it ran) holds the
  target up for 10 minutes after the first strike, 20 after the second, and after the third it stays fixed for the run.
  An underrun at the fixed target (a stall longer than the spare block) isn't one: it puts the next lowering a window off.
  It never goes over the fixed target: a stall longer than the spare block underruns once either way, and a target
  raised past it would carry the stall as latency for minutes.
- **The trade:** under the fixed target, a rare hiccup that its spare block would have covered (a missed wake, 10-15 ms
  late) runs the device dry. The strikes bound it: at most three underruns a run more than the fixed target's. Without
  them the need was forgotten with the window, and hiccups rarer than it came back each time (a wake 15 ms late every
  150 s underran 7 times in 1000 s where the fixed target never did; with the trials it never lets the target down, as
  each lands in a trial's window).
- The deficit and drop thresholds are on the target, so they follow it; `output_target_ms` is published each second,
  and `run`'s status line, the window's budget and fill lines and the reports' conditions show it.
- In the first 240 s of a run it is the fixed target (the window and the trial), then it walks down (up to 53 s:
  `ADAPTIVE_OUTPUT_SETTLE_S`, 293 s, when nothing strikes). `latency` and `quality` measure at once by default (at the
  fixed start, which they say before measuring), so a lab run doesn't wait five minutes; `--settle` waits for it to settle
  (to move and then hold still for 3 s, not on trial; up to 308 s) before they measure. Their
  conditions give the target as a snapshot at the test's end (`output_target_text`): "6.8 ms at the end (adaptive, down
  from 14.0 ms)", or, at the fixed one, the neutral "14.0 ms at the end (adaptive, at the fixed target: it comes down only
  after 240 s of running, and goes back up to it after an underrun)", since a run's first four minutes, a strike, a
  delivery that needs the whole target and a `--settle` that timed out all read the same there.

The first lowering in the simulations (`pipeline/tests/exclusive.rs`, `the_first_lowering_waits_for_a_second_window`,
the soak's conditions: both exclusive, packets up to 1.8 ms late): both end at about 6.8 ms, within the jitter their
windows saw, the trial a window later. A block 11 ms late at 129 s (the old single stage 9 s into its walk, at 9.7 ms, as
the soak's was at 9.9) runs the single stage dry; the trial, at 14 ms then, rides it out and fails on it, another trial
passes once it has left the window, and the target ends where it would have; a block 13 ms late in the middle of the
trial is ridden out too. Swept by hand (2026-11-02): the trial covers a block up to 13.5 ms late at 129, 150, 180 and
230 s (14 ran it dry at two of them); the single stage 9 s into its walk up to 10.5; a first stop halfway (10.1 ms) or at
a period + a block (12 ms), each tried before the trial, ran short at 10. `budget.rs`'s
`an_underrun_raises_the_adaptive_target_at_once` is the same on a 5 ms shared device: a wake 13 ms late at 200 s runs the
single stage's lowered target dry, and the trial, at the fixed target then, rides it out and ends at the same target. The
traces' shared-mic delivery (`timeline.rs`, the ignored `traces_and_the_adaptive_target`) settles at 16.4-19.9 ms with the
trial and 16.7-20.0 with the single stage, neither underrunning.

In the event simulations, with the events at 200 s, after it has come down (`pipeline/tests/events.rs`,
`the_adaptive_target_after_it_has_come_down`, and `budget.rs`'s scenes; `events.rs`'s ignored `adaptive_sweep` prints
them; these use the old single stage, `Pipeline::with_single_stage_target`, which gets down a window sooner than the
trial: what follows once it is down is the same): steady delivery with 1 ms of wake jitter brings a 10 ms device's 22 ms down to about 13.5-14 and the latency by
8 ms (37.7 to 29.0 ms in `budget.rs`), the cables' bursts to about 18 (3.4 ms off), and one wake in five missed keeps it
at 21-22, all with no underrun at any phase. At the event: a wake 15 ms late, which the fixed target covers, runs it dry
once in half the phases and drifts tried (and one wake 10 ms late, at the worst phase and 300 ppm), and the strike puts it back to the fixed
target; a wake 35 ms late, a 50 ms mic loss and a 50 ms cable stall cost what they cost the fixed target (one underrun,
one drop; the dry spells 400 frames longer). A wake 15 ms late every 150 s for 50 minutes: the fixed target never
underruns, the adaptive one three times (the strikes) and is fixed from the third. With the exclusive output
(`pipeline/tests/exclusive.rs`) and steady 3.33 ms packets it comes down from 14 ms to 5, the queue never under it.

## The latency budget (`budget.rs`, TODO 13)
Each block's readout split along its first sample's way, into parts that add up to it exactly, block by block
(`nf_core::telemetry::LatencyBudget`; `pipeline/tests/budget.rs` checks the sum and what moves each part):

| Part | From, to | How |
|---|---|---|
| Mic delivery | the first sample's capture time (the mic's stamps) to the wake that read it | measured (`WakeLog`) |
| Wait | that wake (its time, taken before the mic is read) to the block's write (the output clock's reading, taken as it's written) | measured |
| Output queue | what's queued ahead of the block's first output sample as it's written | measured (the output fill) |
| Render side | the moment the queue would have drained to the play-out the AudioClock predicts | derived: the rest |
| Chain | the chain's stream offset | reported by the stages; not in the readout |

- **Mic delivery** is at least the packet (10 ms for a block-sized one), plus how late the device and its driver delivered
  it and the thread's wake-up. Each wake's first sample and QPC time go into `WakeLog` (256 wakes, trimmed as blocks take
  their samples). A full log counts a new wake's samples as the newest held (its time earlier than theirs: delivery reads
  short and wait long by as much; it takes a period under 0.8 ms with the FIFO full).
- **Wait** is all the audio thread does between the wake and the block's write. First the wake's own work: reading the mic's
  packets, servicing the reference (the keep-alive's top-up and, with AEC on, the loopback's drain) and the tap's and the
  recorder's hand-overs. Then the blocks before this one in the same wake, whole (each one's chain, write, recording and
  statistics), and this block up to its write (its reference, its chain, the output's padding). And the wakes between, when
  a block's first samples came a wake before its last.
- **Output queue:** the output fill as the drift loop reads it (frames up to the sample less the device position now,
  calibrated to the padding by `FillOffset`; the padding and pending samples before a calibration), with this block's
  corrections in it (zeros written, pending samples dropped).
- **The queue's mean is the output target** whatever the delivery: the drift loop holds the mean of the fills read before
  each write. A block that comes late finds less queued (the device played on meanwhile), so mic delivery + queue holds
  still block for block, and the readout with it: it's set by the output's clock.
- **Render side:** half the device period for a device that takes a period at a time and whose clock plays a sample as it
  takes it (the queue is calibrated to the middle of the padding's steps), plus whatever the shared-mode engine and the
  device hold after taking a sample, as their clock reports it. In exclusive mode the queue is the ring, and the render
  side the device's two buffers (one and a half periods: 3 ms at 2 ms) plus what the driver holds past its clock. A device whose clock counts the silence it played after
  running dry reads that far ahead from then on: the readout drops by the silence for the rest of the run, and the render
  side shows it (it can go negative; TODO 13's detail, "Budget").
- **The chain isn't in the readout:** the block's first output sample carries the input from the chain's offset before the
  block's first input sample. The first sample's own way through the app is the readout plus the chain's offset
  (`LatencyBudget::total_ms`); the latency test's app figure holds both.
- **Cost:** per block a wake-log lookup (amortised O(1)), a few subtractions and five `RollingStats<100>` pushes; per
  second five means and five percentiles of 100 values. No allocation, no panic path.
- **Where it shows:** telemetry's `budget_mean` and `budget_p95` (over the readout's window, its last 100 blocks; valid with
  `latency_valid`; the p95s are each part's own and don't add up) and `budget_totals` (the run's sums and blocks, so two
  snapshots give the means between them); `ChainBlock::budget` and six columns of `chain_blocks.csv`; the quality report's
  and the latency report's "latency budget" line (the means over the take or the test); `run`'s status line
  (`lat 61.3/66.0 ms [16.2+0.1+22.0+23.0]`); the GUI's readout ("Budget (means): ...") and its latency report.

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
  cables' echo path is 60 ms; a bursty cable's lead ends at 23-33 ms. Nothing in the engine measures
  the echo path's delay, so a target over 30 ms (`REFERENCE_TARGET_HIGH_MS`) is flagged where it's
  shown (the GUI readout, `run`'s status line, the quality report's warnings). The quality echo
  test puts its echo `--echo-delay` (25 ms by default) after the reference's tap, measures where it
  landed in its chain recording, and warns when AEC3 got it less than 2 ms after its reference.
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
  Both lead and target are published as the echo path sees them, less the echo-path guard's hold (below).

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

## The echo-path guard (`echo_guard.rs`; TODO 10, 13)
AEC3 cancels an echo only once it reaches the mic after the reference sample served with it, so the reference's lead
(above) has to be under the echo path's own delay: from the loopback's tap through the speakers' output latency and the
air. The lead is what the loopback's delivery needs, 12-14 ms on the cables with the shared mic and 18 with the exclusive
one (the echo floor, "Exclusive mode"). Most speakers' echo comes later than that; low-latency ones may not, and then the
echo comes first and AEC3 can't place it (its delay estimate reads 0, its ERLE 6-8 dB against 15-19: TODO 10's sweep and
the live takes). `io.echo_guard` (on by default, ours; `--no-echo-guard`) watches for that, once a second on the audio
thread, and holds the mic back when it sees it:
- **The hold:** each block is processed `hold` samples after it could be, and the reference loop reads it as captured that
  much later, so it serves the newer reference the wait let the loopback deliver: the lead the echo path sees is the
  loop's less the hold. The loop's need, target and readings are unchanged by it (`take_reference` shifts the block's
  capture time; `Reference::held` moves the read position with a growing hold, the jump the facts show as `held`). A
  growing hold writes the extra as silence to the device at once (its next block comes that much later; left pending
  until then, a 20 ms hold over the exclusive output's 14 ms queue played 6.9 ms of silence of its own); a shrinking one
  drops that much of the oldest mic audio (`mic_discarded`). Each is one short gap. The backlog the output's corrections
  count leaves the held samples out.
- **Whole loopback periods,** at most 30 ms (`echo_guard::most_periods`). The loopback delivers a period at a time, so a
  hold of whole periods keeps the phase of the blocks' takes against its packets, and with it the reference's spare. A
  first version held by the sample (754, 15.7 ms, on the cables' 480-sample packets), which brings one packet more or two
  by the take as the phase falls; live it brought one, every take came up 130 samples short (62-129 reference gaps, which
  wrecked AEC3's convergence in one take), and the reference's target rose 5.7 ms before the gaps stopped.
- **What it watches:** an echo (the mic's energy envelope, a value a block, correlated with the far end's at 0.4 or more
  over the last 5 s, at the best of ±3 blocks: headphones and the near talker alone show none) that AEC3 can't place
  (with the far end playing, its ERLE under 10 dB and its delay estimate at most 4 ms; it reads in 4 ms steps less its
  headroom, so a short echo it does place reads 0 too, and only the ERLE tells them apart). Only seconds AEC3 ran on
  count (switched off live, it's not watched), and only once the run is 20 s old with 10 s of the far end playing, so
  an echo AEC3 can place has been cancelled by then.
- **The trials, smallest first:** five such seconds in a row start a trial of one loopback period. AEC3 gets 8 seconds of
  the far end to find the echo again (a jump costs it 5-10 s), and the next 8 judge it: a second is good when AEC3's ERLE
  is 4 dB over the suspect seconds' and 10 dB or more, or when AEC3 has placed the echo (its delay estimate over 4 ms;
  an echo before its reference always reads 0). Placing it can cost AEC3 its ERLE for a while: live, an echo 11.7 ms
  after its reference was cancelled at a delay of 0 at first, and 13 s later AEC3 moved its delay to 8 ms and started its
  filter over, its ERLE at 0, in the judging; the first version took that for failure. Half the judged seconds good keep
  the hold. Otherwise the next trial holds a period more, while the effective lead is still over 2 ms (and the lead 3 ms
  over that to start with); past the last the hold goes, and no trial comes for ten minutes (a minute when the far end
  didn't play enough to judge in two). So the hold is the least that works: with the exclusive mic's 18 ms lead on 10 ms
  loopback periods, speakers whose echo comes 9-18 ms after the tap get 10 ms, sooner ones 20. A held run whose echo is
  again one AEC3 can't cancel (its target rose, the speakers changed) tries the next hold, and past the last lets it go;
  a minute of the far end with no echo seen (headphones put on) lets it go.
- **Seen:** telemetry's `echo_hold_ms` (the hold, as the next block takes it up), `echo_guard` (watching, trial, held,
  cooling; 0 when it isn't running) and `echo_correlation`; the chain recording's `echo_hold` (its lead and target are
  the loop's: the quality test's echo delay takes the hold off); `run`'s status line ("held 10.0 trial"), the quality
  report's engine line and the window's reference line; and the control thread logs each move and notes it for the
  flight recorder.
- **Offline** (`pipeline/tests/timeline.rs`, `the_echo_path_guard_holds_the_mic_for_an_early_echo`, the quality test's
  echo scene through AEC3, its ERLE over the last 20 s): steady 10 ms delivery (a 12 ms lead), an echo 5 ms after its tap
  6.9 dB without the guard and 19.3 with it (10 ms), as good as a 40 ms echo's 19.4, which gets no hold; the loopback 8 ms
  later (a 20.9 ms lead), an echo at 15 ms 6.9 and 14.3 dB (10 ms), at 5 ms 5.5 and 13.5 dB (the first trial failed, the
  second held 20 ms).
- **Live** on the cables (the exclusive mic's 18 ms lead, `quality --test echo --echo-delay 5`, 150 s): far-only turns
  58.8 dB median with the guard (20 ms held, 0 gaps, 0 underruns) against 22.3 without it; 25 ms, no trial. TODO 10's
  detail has the takes.

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
  into the filtered mic), and shared mode even where exclusive is asked ("Exclusive mode", "Never
  on the default playback device"). The default moving to or from the output restarts the streams.
- The reference is only needed when AEC is enabled at start or later. Open the loopback whenever a
  reference device is set and AEC is enabled. If AEC is switched on later, rebuild the streams; a
  short restart is acceptable.
- Call `set_ducking_preference(true)` on every session we open.
- Format: request 48 kHz f32 at the device's channel count. With the mix rate at 48 kHz, use it
  directly (low-latency path allowed). Otherwise use autoconvert (no IAudioClient3). In exclusive mode, 48 kHz in the
  best format the device takes (32-bit float, 24-in-32, 16-bit), or shared mode ("Exclusive mode").

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

A track can carry a second signal placed while the take runs (`PlayTrack::late`, a `LatePart`): its start, in frames after the
lead, is an `AtomicU64` set once by a compare-exchange from `LatePart::UNSET`. The caller sets it from another thread when it
knows (`LatePart::set`); the track's player, which writes 100 ms ahead, reads it at each top-up, writes silence after the first
signal meanwhile, and sets `default` itself if it gets there first (a later set then fails). A start the writer has already
passed starts the part where the writer is. Starts are clamped to the first signal's end..=`latest`, the take runs until every
track's programme has played (a late part counts from its latest start until it's placed) and the tail, and
`PlayRecordResult::late_starts` says where each part went and whether the caller's start or the default placed it. `quality`'s
echo test places its far voice so, once it has timed a marker in the chain recording (app.md).

## The chain recorder (`recorder.rs`, TODO 10 and 14)
What the chain was fed live and what it gave, block by block, so that a live result can be told apart from the chain's own
behaviour and rerun offline exactly (`Engine::record_chain`; `noise-filter quality --save` writes it):
- **On the audio thread**, after each block's chain call and its write (`Pipeline::record`, whether or not the write failed),
  one item into a pre-allocated rtrb ring (400
  blocks, 4 s, about 2.4 MB; its pages are touched when it's armed): the mic block and the reference block exactly as the chain
  got them (the mic after the dither; the reference after the timeline, the slip resampler and the dither), the chain's output
  block (before the output's drift loop resamples it for the device), and the block's facts, `ChainBlock`:
  - the run's block index, the mic block's capture QPC (by its stamps, NaN without), whether the mic's stamps are trusted, the
    mic stream index of its first sample, the wake that delivered it, and the mic samples still queued (a burst or a late wake);
  - the mic's moves since the block before: samples its FIFO discarded (it overflowed: the loop hadn't run for 200 ms), and device
    position it skipped without zero-filling (a gap over 100 ms; the stream index doesn't count it);
  - AEC3's own delay and ERLE estimates after the block (cached by sonora: a reference, no work);
  - the block's latency budget ("The latency budget"), while the readout is on and the block had one;
  - what the output side did, `OutputFacts` (TODO 9; a few stores a block, always): the device-stream index the block's first
    sample went to, the device's padding and our pending samples before the write, the fill the drift loop read and its
    target, the slip (-1, 0, +1), zeros written (a deficit; not the first block's starting fill), pending samples dropped,
    whether the block itself was dropped (skipped), an underrun, a re-anchor, the silence an exclusive device played since
    the block before (`RenderSink::take_silence`; its clock leaves it out, so the fill doesn't see it and every later sample
    plays that much later until the loop slips it back), and the device's clock reading the block used (position in our
    stream's frames and its QPC time; `nf_engine::clock_drift` fits each clock's drift from those and the mic's stamps, for
    `devices --check`). Block to block the index moves by the last
    block's length (480 and its slip, or nothing when it was skipped) plus this block's zeros less its dropped samples
    (`pipeline/tests/budget.rs`, `the_output_facts_follow_the_stream_block_for_block`, through bursts, a dry device, a mic
    stall's dry spell and overfull burst, a 1024-frame device and a lagging clock; the facts add up to the counters). So the
    chain's output maps onto the device's stream sample for sample, and a move of the output path's delay that the facts
    don't show happened after the device took the samples;
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
  `ChainRecorder::stop` stops it and waits for the producer to come back. The recording so far sits behind a mutex that the
  drain thread holds while it appends: `ChainRecorder::copy_from(n)` copies the blocks from the nth on while it runs (the
  quality echo test's calibration reads its marker so, every 20 ms). Neither thread is the audio thread, which only pushes into
  the ring.
- **Rerunning it offline:** the engine builds its chain with `models::build_chain` (the noise model of the run's config; a
  pack's inline offline, as many blocks behind as on its worker) and warms it up with three dithered
  silent blocks (`audio::warm_up`, public as `nf_engine::warm_up_chain`) before the first block, and the recorded mic and
  reference are dithered already. So a chain built with the run's processing settings, warmed up and fed the recording block by
  block reproduces its output sample for sample (`the_live_chain_reruns_offline_sample_for_sample`, AEC3 and RNNoise behind
  bursty delivery with a loopback stall; without the warm-up it doesn't). `noise-filter process --warm-up` does this with the
  saved WAVs. A recording armed while running, or with blocks lost, starts from a state the rerun doesn't have; one whose
  settings changed matches a rerun with one config only up to the change (`ChainRecording::settings_changed_at`).
  `reproducible` checks all three.
- **The thread's facts** (`ThreadFacts`, the last of `ChainBlock`'s; the flight recorder's, below): the wake that processed
  the block, the longest gap between wakes since the block before's, the wakes (and those the exclusive output's event made)
  and stalls since then, the mic's glitches, the block's own time and a model pack's misses. `chain_blocks.csv` has them as
  its last columns.

## The flight recorder (`flight.rs`, TODO 13 and 21)
The last seconds of every block's facts, kept in memory and written out when something goes wrong, so a soak or a user's run
shows what ran dry (`Engine::arm_flight_recorder`; `run --flight` and `devices --check`: app.md, "The flight recorder"). The
two 30-minute soaks on the dev VM's cables each had one output underrun near two minutes in, and `run` kept no per-block log.
- **On the audio thread, nothing new.** It pushes the chain recorder's item (`ChainItem`: the block's facts, `ChainBlock`, and
  its mic, reference and output) into a ring of the flight recorder's own (`Pipeline::flight`), beside the chain recorder's
  when both are armed: one item copied into a pre-allocated rtrb ring a block (about 6 KB; a full ring loses the block,
  counted), what the chain recorder costs. The facts it needs are those the thread writes anyway, and the thread's own
  (`ThreadFacts`, below) are a few stores a block, armed or not, the block's time being the clock reading its timing takes
  anyway. The audio thread only flags; the control side detects. The no-allocation test runs with both rings armed.
- **The thread's facts** (`ChainBlock::thread`, `ThreadFacts`; in `chain_blocks.csv` too): the wake that processed the block
  (QPC), the longest gap between the thread's wakes since the block before's (0 for a later block of the same wake; `drain_mic`
  is called once a wake), the wakes since the block before's and those of them the exclusive output's event made
  (`out_wakes`; 0 with a shared output, `Pipeline::count_out_wake`), the stalls (wakes with nothing from the mic, neither its
  event nor a packet, 100 ms after its last; `Pipeline::count_stall`, both from the loop's mic step, `audio::mic_wake`), the
  mic's glitches, the block's own time (from taking it off the mic FIFO to its output written; the recorders' copies not),
  and a model pack's misses (its worker's counter, one relaxed load a block). With the mic's delivery (`mic_wake_hns`,
  `mic_queued`) and the output's padding they tell a thread that was held up (a long gap between wakes with no stall, and
  the mic's samples waiting when it woke: whole blocks queued, several blocks at one wake, or a glitch, an exclusive mic's
  one-period buffer overrunning) from a device that stopped delivering (wakes on time, or stalls, and a gap between the
  mic's delivering wakes) from an output that ran dry (the thread and the mic on time, the device's padding gone). That
  takes an exclusive output: its events wake the thread every few ms whatever the mic does. With a shared output the thread
  waits on the mic's events alone, and a thread held up 60 ms and a mic's driver that held its packets back 60 ms and then
  delivered them at once give the same facts (`out_wakes` 0 tells the reading so).
- **Its rings follow the engine.** The chain recorder's recording ends with its run; the flight recorder gives each run a ring
  of its own while it's armed, made and its pages touched on the control side as the start makes the run's hand-over rings
  (`flight::run_channel`, beside the chain recorder's), put in the run's command ring so the run's first wake takes it
  before its first block, its consumer handed to the recorder's thread with the run's number since arming. A start that
  fails hands its ring back untaken (`run_job`), freed with the port; a run that ends drops its producer with its pipeline
  (the run is over), and the thread drops the ring once it's empty. Armed during a live run, the live run's ring goes to its
  command ring and is taken at the next wake. Stopped, the thread drops its consumers, the live run hands its ring back at
  its next wake (the stop waits up to 500 ms for it, as the chain recorder's does) and it's freed on the control side. One
  flight recorder at a time; it runs beside a chain recorder (`devices --check` arms both).
- **The drain side** (`flight::Core`, pure; its thread, `nf-flight`, normal priority, every 20 ms): a ring of the last
  `facts_s` + `post_roll_s` of blocks (10 + 3 s by default; an entry is a block's facts, its run, its sequence number since
  arming, the blocks its ring lost before it, and the triggers it showed, about 0.3 KB: 0.4 MB), and when `audio_s` asks
  (0 by default; `run --flight-audio`: 5 s) a ring of `audio_s` + `post_roll_s` of audio (5.8 KB a block: 4.6 MB at 5 s),
  both allocated when it's armed and never grown (`the_recorders_memory_is_bounded`), and the control side's last 64 notes.
- **Triggers**, from each block's facts, per run (`flight::Watch`):
  - the output: an underrun, a deficit (zeros written), a coarse drop (pending samples dropped, or the block skipped); never
    at a run's first block, which sets the starting fill (its facts count none there);
  - the mic: a stall (100 ms with nothing from it), its FIFO overflowing (`mic_discarded`), a position skip
    (`mic_skipped`), a glitch (`ThreadFacts::mic_glitches`; the run's first packet is never one, though many drivers flag
    the first packet after a start discontinuous: `drain_mic`);
  - the audio thread, from the run's second second on (its first is the streams starting): a wake 40 ms or more after the
    one before with no wait timing out (its waits time out at 100 ms; an exclusive output's events wake it every few ms), a
    block that took 5 ms or more (half a block);
  - the echo reference, after its start-up (260 blocks from its alignment, as `devices --check` counts it; its alignment,
    centring and raises there are by design): a jump of its timeline (broken, stepped, retargeted or centred), a burst of
    short takes (5 of the last 100 blocks' takes owed samples, 5% of a second; the next burst once a whole second has had
    none), and the reference lost (a block without it after one with it);
  - a model pack's miss (its worker's result not there in time);
  and from the control side (`flight::note`), a fault that stopped the audio (`check_health`, worded as the status words it).
  The control side also notes, for the summary, a failed start, a restart and why (a device change, a settings change, a
  retry), a lost reference and a stop.
- **A dump.** A trigger starts one and waits out the post-roll (`post_roll_s` of blocks after it, or that long and 0.5 s more
  by the clock when no blocks come: the audio stopped). A later trigger goes into a pending dump that holds its kind, or
  whose trigger came half a second (50 blocks) or less before it, whatever its kind: the same incident (a stall's first
  block has its underrun, skip and glitch; a held thread's, its underrun and late wake). The most serious of those leads
  the dump (the output, then the mic, the thread, a fault, the reference, the model), its window re-centred on it, if its
  kind may have a dump. Any other trigger opens a dump of its own, with its whole pre- and post-roll, overlapping another
  kind's if need be (soak 2's reference bursts and the underrun 18 s later; or an underrun 2.9 s into a burst's post-roll):
  up to one pending a kind. A dump is cut as its post-roll's last block comes in (`Core::feed`), the ring then holding
  exactly its window, however many blocks a drain brings at once. Its window (`facts_s` before the trigger to the
  post-roll's end; the audio's `audio_s` before it) goes into a folder of its own in `dir`, `flight-YYYYMMDD-HHMMSS-<trigger>`
  (local time; `-2` and on for a name taken). The wall clock is the system's now less how long ago the trigger was by QPC.
  Written on the recorder's thread (a write slower than its ring's 4 s would lose blocks, counted). `FlightDump` has its
  headline (`trigger`) and the other triggers it holds by name (`also`), and `run`'s line names both.
  - `flight_blocks.csv`, a row a block: `run` (since arming, from 1; each start takes the next, a failed one too), `t_s`
    (seconds since arming, by the block's wake), `trigger` (the block's triggers, `|`-joined: `output-underrun`,
    `output-deficit`, `output-drop`, `mic-stall`, `mic-overflow`, `mic-skip`, `mic-glitch`, `late-wake`, `slow-block`,
    `reference-lost`, `reference-jump`, `reference-short`, `model-miss`), `lost` (blocks its ring had lost before it, in its
    run), then `chain_blocks.csv`'s columns (`nf_engine::CHAIN_BLOCK_COLUMNS`, written by `nf_engine::chain_blocks_csv`;
    csv.rs lists them): the chain recorder's facts above, and the thread's last, `wake_hns, wake_gap_ms, wakes, out_wakes,
    stalls, mic_glitches, process_us, model_misses`.
  - `summary.txt`: what triggered, when (local time, seconds since arming, the block and the run), the window and the audio,
    the other triggers in the dump, the window's triggers it doesn't hold (before its trigger, or another dump's after it),
    the triggers the rate limit held back since the last dump by kind, a reading of the facts,
    the facts around it in words (the output's padding, fill, target, zeros, drops and silence; the thread's usual and
    longest gap between wakes in the 200 ms before, its stalls, what woke it (the mic alone, or the output's events too),
    its slowest block; the mic's usual and longest gap
    between deliveries, its queue and its first sample's age; the reference's lead, target, freshness, short takes and jumps;
    lost blocks), the control side's notes, and the files.
  - `mic.wav`, `ref.wav`, `out.wav` (48 kHz mono float: the chain's input and output, as the chain recorder keeps them)
    when audio is kept.
  The reading (`flight::read`) is a heuristic, in this order: a fault, as worded; a gap between the thread's wakes in the
  200 ms before the trigger of three times its usual and 15 ms more, with no stall, is the thread held up when the mic's
  samples were waiting as it woke (else the devices' events came late), but with a shared output (no `out_wakes` in the
  run) the thread held up or the mic's driver holding its samples back, which it says it can't tell apart; stalls, or a
  gap between the mic's deliveries of three times its usual and 20 ms more, is the mic stopping; a block of 5 ms or more
  is the processing falling behind; otherwise, with both on time, it's the device the trigger names (the output running
  dry or overfilling, the loopback late or broken, the mic losing samples, the model's worker late).
- **Rate-limited and bounded:** at most one dump a `min_interval_s` (60 s) and `max_dumps_per_kind` (3) an arming for each
  kind of trigger (the output, the mic, the thread, a fault, the reference, the model), so a misbehaving loopback (the
  second soak's 1,109 short takes over minutes, from 92 s and again at 1234 s) can't use up what an output underrun needs,
  18 s later or 13 minutes later (`a_kind_that_keeps_misbehaving_cant_crowd_out_another_later`); at most `max_dumps` (20)
  in all, more than the six kinds' 3 each, so with the defaults it bounds the disk without crowding a kind out. The rest
  are counted (`FlightStats::suppressed`, by kind in `suppressed_by` and the next dump's summary). On disk, after each dump,
  the oldest dump folders in `dir` past `keep_dumps` (20) are deleted, only folders named exactly as the recorder names
  them (`flight-`, 8 digits, `-`, 6 digits, `-`, a trigger's name, and `-2` to `-999`) with a `summary.txt`: about 0.4 MB
  a dump with the facts alone, 5 MB with 5 s of audio.
- **Tests** (no devices): flight.rs (a window with its pre- and post-roll and audio; the rate limit by kind, a kind's cap
  and in all; soak 2's shape, the reference's bursts for minutes and underruns at 110 s and 900 s, both dumped; a trigger of
  another kind late in a post-roll dumped on its own, and one within half a second leading the dump; a dump cut whole
  whatever the drain's batches; the memory bounded, two dumps pending at once; the reference's start-up; a fault dumped by
  the clock when no blocks come, and a stop's flush; the reading of a held thread (with an exclusive output, and with a
  shared one), a quiet mic and a dry output; the files, the retention and its names; and the threads end to end with fake
  runs: across a restart, a fault noted, armed while running); `pipeline/tests/flight.rs`, scenes through the pipeline, the
  loop's mic step (`audio::mic_wake`) and the core, the thread's waits simulated as the loop's (the output's event reported
  first): a mic that stops for 150 ms at 20 s (one dump, its trigger the first block after the stall with its deficit,
  stall, skip and glitch, its window the 10 s before and the 3 s after, read as the mic stopping, and written out); a
  thread held 120 ms with an exclusive output (no stall, read as the thread); a mic's driver holding its packets back 60 ms
  with it (the mic); and, with a shared output, a thread held 60 ms and a driver holding back 60 ms (the same facts and
  reading, saying it can't tell); and the no-allocation test with both rings.

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
- The output path's delay: the cables' bursts at 300 ppm move it by nothing but slips for two minutes (no zeros, drops,
  underruns or re-anchors; the readout's 1 s means within a millisecond:
  `the_cables_bursts_move_the_output_path_by_its_slips_alone`). The 9-13 ms steps the dev VM's quality takes saw (TODO 9)
  were silence inserted between the filter's output and the test's capture: the cable's, unless the next take's capture gaps
  show the test's recorder filled a position jump (TODO 8's detail, "The output path's 9-13 ms steps").
- The inference worker (`nf_core::noise::Pipelined`, nf-core's unit tests): the latency formula, inline equal to a waited-for
  worker bit for bit through resets, a model that sleeps 15 ms every 100th hop on an unwaited worker (misses counted, the output
  finite and aligned between them, recovery), old-epoch results dropped, a full ring's gap and resync, a failed run's restart,
  and no allocation on the calling thread (`nf-core/tests/rt_no_alloc.rs`, a stalling worker included). With the pack fetched,
  nf-ort's tests run it end to end (`crates/nf-ort/tests/ort/`).
- Exclusive mode the same way: `EventRender` over a `PeriodDevice` (the WASAPI one is `ExclusiveRenderDevice`), the
  period and format choices, the retries and the fall-back as pure functions; `pipeline/tests/exclusive.rs` drives a
  2 ms device on its own clock against steady and bursty mic delivery, a thread stalled mid-run and before its first
  answer, a mic stall's dry spell, and 3 ms and 3.33 ms mic packets.
