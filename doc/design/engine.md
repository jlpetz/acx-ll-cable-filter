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
    loopback never stops.
- `exclusive`: exclusive mode apart from WASAPI: the period and format choices, the fall-back to shared mode, and the
  event-driven output's ring (below, "Exclusive mode").
- `out_target`: the adaptive output target (below, "The output target").
- `reference`: the reference's timeline (below, "The reference").
- `audio`: the audio thread (below).
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
  build and activate the chain *inside the scope* with `nf_stages::build_chain` (AEC3 when AEC is on,
  then the noise stage around RNNoise; `Chain::activate` checks each stage against the host profile,
  activates it at 48 kHz in 480-frame blocks on this thread, and caches its latency). The offline tools
  use the same constructor, so the live chain and a rerun can't drift apart.
- Open the mic, output, loopback and keep-alive streams.
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
     Push it (in ms) to `RollingStats<100>`, and its budget ("The latency budget" below). It times the block's first
     *output* sample, so the chain's offset isn't in it.
   - **Telemetry:** meters (dBFS, through display ballistics: a 1 s peak hold falling at 20 dB/s,
     and a 300 ms power-domain RMS), clip latch (`clip_latched |= clipped`), near-clip, speech
     probability, and the noise stage's reduction (`noise_reduction_db`: its output against its
     latency-aligned input, the floor mix and gate included).
   - **Timing:** whole-block µs into `RollingStats<100>`; AEC and noise ticks.
4. **Once a second (every 100 blocks):**
   - publish p50/p99/max timings, latency median/p95 and its budget (each part's mean and p95, and
     the run's totals), drift ppm, fills (ms), slip counts and AEC stats (`chain.aec_stats()`, AEC3's
     `aec-telemetry/0` extension);
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
  its buffers can't make every event late) is an underrun too: the device played whatever its other buffer held, and
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
  mic's events (`StallWatch`: 100 ms without one), since the output's wake the thread every 2 ms.
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
  is under that echo through; `quality --test echo`'s default delay (25 ms) is over it.
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
- **Lowered slowly:** once 120 s have passed since the last move was decided, the goal is the floor (one period + the
  margin: what the device's period requires) + the need + 1 ms, when that's at least 1 ms lower. The target walks down to
  it a tenth of the way each second (at most 0.5 ms a second, at least a sample: 9 ms takes 53 s, all but the last
  0.25 ms in the first 41), through `DriftController::set_target`, and the loop slips the fill after it, so nothing
  clicks. A walk, not a step: set in one step, the PI loop ran at its 1500 ppm limit, wound its integral up as the fill
  neared the target and carried it past, 1.4 ms under the new target for 10 s (under the floor, in exclusive mode),
  just when the target is least proven; a steady walk overshoots when it stops (0.8 ms at 0.25 ms/s). The walk's slowing
  end lets the loop's integral wind down with it: in the simulations the queue's 1 s means stay above where the walk ends
  (within 0.1 ms; they run up to 1 ms ahead of or behind the moving target meanwhile).
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
  150 s underran 7 times in 1000 s where the fixed target never did).
- The deficit and drop thresholds are on the target, so they follow it; `output_target_ms` is published each second,
  and `run`'s status line, the window's budget and fill lines and the reports' conditions show it.
- In the first 120 s of a run it is the fixed target, and then it walks for up to a minute. `latency` and `quality`
  measure at once by default (at the fixed start, which they say before measuring), so a lab run doesn't wait three
  minutes; `--settle` waits for it to settle (to move and then hold still for 3 s; up to 188 s) before they measure. Their
  conditions give the target as a snapshot at the test's end (`output_target_text`): "6.8 ms at the end (adaptive, down
  from 14.0 ms)", or, at the fixed one, the neutral "14.0 ms at the end (adaptive, at the fixed target: it comes down only
  after 120 s of running, and goes back up to it after an underrun)", since a run's first two minutes, a strike, a
  delivery that needs the whole target and a `--settle` that timed out all read the same there.

In the event simulations, with the events at 200 s, after it has come down (`pipeline/tests/events.rs`,
`the_adaptive_target_after_it_has_come_down`, and `budget.rs`'s scenes; `events.rs`'s ignored `adaptive_sweep` prints
them): steady delivery with 1 ms of wake jitter brings a 10 ms device's 22 ms down to about 13.5-14 and the latency by
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
- **Rerunning it offline:** the engine builds its chain with `nf_stages::build_chain` and warms it up with three dithered
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
- Exclusive mode the same way: `EventRender` over a `PeriodDevice` (the WASAPI one is `ExclusiveRenderDevice`), the
  period and format choices, the retries and the fall-back as pure functions; `pipeline/tests/exclusive.rs` drives a
  2 ms device on its own clock against steady and bursty mic delivery, a thread stalled mid-run and before its first
  answer, a mic stall's dry spell, and 3 ms and 3.33 ms mic packets.
