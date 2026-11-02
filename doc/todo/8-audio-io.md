# 8-9. Audio I/O, clocks and buffering

Facts and sources: `doc/research-2026-10.md` (Windows audio I/O).

## Streams
| Stream | Endpoint | Mode | Notes |
|---|---|---|---|
| Mic capture | chosen mic | event-driven exclusive mode by default (`--no-exclusive-mic` turns it off, below); in shared mode, IAudioClient3 minimum period if the driver allows, else 10 ms | RAW mode where `RawProcessingSupported` (shared mode); not the Communications category |
| Cable render | chosen virtual cable input (e.g. CABLE Input) | event-driven exclusive mode by default (`--no-exclusive-output`, below); polled shared mode, written at the mic's wakes, when off or refused | `SetDuckingPreference(TRUE)`; small period or RATEADJUST, not both |
| Reference capture | loopback of the chosen speakers | shared mode only | zero-fill through silence (no packets arrive); pre-volume; honour `flags.silent`; QPC timestamps |
| Keep-alive (optional) | the chosen speakers | silent render at the default period | keeps loopback packets flowing; never a small period (that would switch every app on the endpoint) |

## Requirement: MMCSS "Pro Audio" on every audio thread
- **What it is:** MMCSS (the Multimedia Class Scheduler Service) is how Windows schedules audio and
  video threads. A thread joins a task class with `AvSetMmThreadCharacteristicsW("Pro Audio")`.
  The "Pro Audio" class is in the High scheduling category, so it runs at priority 23-26
  (real-time class) for its scheduled share. The registry default for "Audio" is Medium (16-22).
  The `audio_thread_priority` crate only registers "Audio".
- **Why it's required:**
  - It keeps a capture, DSP or render pass from waiting behind a game's threads.
  - Windows gives a process whose window is minimised or hidden Low QoS unless it's audible or
    its threads are MMCSS-tagged (tagged threads get Deadline QoS). The owner's plan is to minimise
    the window, so without MMCSS the engine could be throttled.
  - MMCSS priority also weighs foreground state, and "Pro Audio" isn't Background Only, so the
    23-26 priority isn't guaranteed while minimised: check dropout counts with the window minimised
    (phase 1).
- **Applies to:** capture, DSP, render, any inference worker, and the fx-host's audio thread (#17).
  Revert with `AvRevertMmThreadCharacteristics` on exit.
- **Note:** MMCSS reserves 20% of CPU for other work by default (`SystemResponsiveness`).

## Threads
- **Capture thread:** MMCSS "Pro Audio" via `AvSetMmThreadCharacteristicsW` (the `audio_thread_priority` crate only registers "Audio"). Copies only.
- **DSP thread:** runs AEC → model → gate on one 10 ms frame clock, with no re-buffering between stages. A model whose time per call can't be bounded runs on its own worker. With the noise filter off, the model is bypassed and its delay removed (a brief jump on toggle is fine).
- **Render thread:** feeds the cable from a ring buffer.
- **Rings:** SPSC (rtrb 0.4.0). No allocation, locks or logging on audio threads; `assert_no_alloc` in debug builds.
- **Control thread:** rebuilds streams on device changes (wasapi `DeviceEventCallbacks`). All COM stays in the engine crate, and audio threads run in the MTA.
- **QoS:** a process whose window is minimised or hidden gets Low QoS unless it's audible or its threads are MMCSS-tagged. Every audio thread joins MMCSS "Pro Audio", and the engine runs as its own process (#6, #15).
- **Formats:** process at 48 kHz. Convert 44.1 kHz devices at the edge (`AUTOCONVERTPCM` + `SRC_DEFAULT_QUALITY`, or our resampler). Detect band-limited inputs (16 kHz USB headsets, Bluetooth HFP) for the high-band policy (#3).

## Clocks (#9)
- **Three independent clocks:** mic, speakers (which clock the reference) and the cable. Without correction the rings over- or underrun within minutes, and the echo canceller keeps re-aligning.
- **Control loop:** a PI controller on ring fill drives a rubato 5 `Async` resampler (`set_resample_ratio_relative`), or `Slip` (crossfaded single-frame insert or drop, no delay, up to ~0.8% drift).
  - For 16 kHz models via the resampler path, fold the ratio into the 48↔16 kHz resampler. Resample at most once in each direction, with a short (~1 ms) filter; no block-based FFT resamplers (they add a block of delay).
  - Resample the reference to the mic clock the same way.
- **Target fill:** a small one, not a big safety margin. A big margin is a common source of hidden latency (our estimate: 20-50 ms; #13 will measure it).
- **Measure first:** real drift (ppm over 30+ minutes) on the owner's device pairs, to size the controller.

## Hop vs period
- With WASAPI periods of `p`: capture-to-output = `N + L + p − gcd(p, H)`.
- 10 ms-hop models (RNNoise, DFN3) align with 10 ms periods.
- UL-UNAS (16 ms hop) and FastEnhancer-48k (512-sample hop) pay up to ~8-9 ms unless the period divides the hop.

## Exclusive mode (built 2026-11-01; the default since 2026-11-02; engine.md, "Exclusive mode")
A probe on the dev VM (2026-11-01, not in the tree) found every VB-CABLE endpoint takes exclusive mode, 16-bit only (32-bit
float and 24-in-32 refused), event-driven at periods down to 2 ms (render) and 3 ms (capture), 10 ms by default. The options
`io.output_exclusive` and `io.mic_exclusive` use it, both on by default (the owner, 2026-11-02: the lowest latency by
default; `--no-exclusive-output`, `--no-exclusive-mic` and the window's switches turn them off, e.g. to record the raw mic
in another app). The cable's recording side, which the apps read, stays shared either way.
- **Why:** shared mode puts the audio engine's pass between the app and the driver on each side. On the cables the render
  side (our buffer to the output's play-out) was 26 ms and the mic's delivery 16, against a block's 10. Exclusive mode
  reaches the driver directly, at its own small period.
- **Event-driven, not polled:** WASAPI's event-driven exclusive render gives two buffers of one period; at each event the
  app hands over a whole period while the device plays the other. The engine keeps its own ring (100 ms) ahead of that and
  moves a period at each event, so the output model (fill, drift loop, `FillOffset`, deficit and drop rules, readout,
  budget) holds with a 2 ms period. Polled exclusive would keep the single mic-driven loop, but it's the mode VB-CABLE wasn't
  seen to take, a polled exclusive buffer that runs dry replays stale audio (no engine plays silence), and its padding is
  the driver's to report. The cost: the audio thread answers the output's event within a period, so it waits on both
  events and answers the output's first, and again after each block (a block is 0.1-0.3 ms of work).
- **Formats:** 32-bit float, then 24 bits in 32, then 16-bit, at 48 kHz (exclusive mode can't resample). 16-bit and 24-bit
  are written rounded, at the scale they're read at, so they round-trip exactly; no dither (the shared engine converts to a
  16-bit cable without one too, the rounding is -101 dBFS RMS, under any mic's own noise, and a dither would hiss under the
  gate's silence).
- **Periods:** the smallest from the device's minimum (never under 2 ms) that divides the 10 ms block, else the minimum:
  2 ms out, 3.33 ms in on VB-CABLE. A mic packet that tiles the block finishes the block when its last sample is captured;
  144-frame (3 ms) packets would finish one 1 ms late on average. The probe only tried 3 ms on the capture side, so a
  refused period tries the device's minimum (3 ms there) and then its default before giving up, and a format the
  initialise refuses tries the next format.
- **Trade-offs:**
  - Exclusive mode locks the endpoint. The output cable only has the filter playing into it, so locking its input side
    costs nothing (the apps read its other side). The mic is another matter: **no other app can record from that mic while
    the filter runs**; they should use the filtered mic (the cable) anyway. The help and the window's switch say so, and
    say to turn it off to record the raw mic in another app too.
  - **Never on the default playback device:** an output that is Windows' default playback device (the speakers picked by
    mistake, or VB-CABLE's installer leaving CABLE Input the default) is opened shared even when exclusive is asked:
    held exclusively, no other app could play sound at all (the call's own sound included). Its warning says so, and a
    change of default to or from the output restarts the streams, so fixing the default brings exclusive mode back.
  - **Two copies of the filter:** with both exclusive by default, the window and a CLI `run`/`latency`/`quality` on the
    same mic or cable can't both run: the second's shared open fails on the first's exclusive stream
    (`AUDCLNT_E_DEVICE_IN_USE`), and its start error names another copy of the filter as the likeliest holder. The lab
    commands want the window's filter stopped first.
  - No Windows effects on an exclusive stream (no APOs, so RAW has nothing to do), and no IAudioClient3.
  - **Volume (unverified):** an exclusive stream bypasses the audio engine, so the endpoint's volume slider (CABLE Input's
    playback level, the mic's Levels slider) should apply only where the device has a hardware volume control; on a
    device whose volume is the engine's software one, it would stop applying. Unchecked on VB-CABLE and on a real mic;
    the next live run should check it before the help and the window's lines say so.
  - A device in use by another app, with exclusive control turned off in its properties, or refusing every period tried,
    falls back to shared mode with a note (not a warning: a refusal is ordinary now that exclusive is the default) that
    says which stream, why (the periods tried, for those), what shared mode costs, how to fix it and where to turn the
    option off (the window's switch, the CLI's flag). When the format check takes nothing, the initialise is tried anyway,
    so "in use" and "not allowed" come back as themselves.
  - **The output's deadline:** a late answer (the thread held off for over a period, 2 ms) lets the device replay a stale
    buffer: a glitch, counted as an underrun (once a block, as a dry spell is). How the driver's stream position treats it
    is unverified on VB-CABLE: the engine assumes the stream moved on. The first answer, after the start's other work, is
    the likeliest late one; it's timed from the start, so it's counted and its stale periods kept out of the readout.
  - **The mic's deadline:** an exclusive capture buffer is one period (3.33 ms), where shared mode's is 40 ms. A thread
    held up longer than about a period loses mic audio: a capture glitch, zero-filled. Shared mode rides out the same
    delay. MMCSS's priority is what keeps the thread on time; a live run should watch the capture glitches, with the
    window minimised too.
  - A wake every 2 ms instead of every 10 (a few µs of WASAPI calls each).
- **What a live run should show:** the budget's render side down from 26 ms by the engine's share (to about 3 ms, the
  device's two buffers, plus whatever VB-CABLE holds past its clock), the queue's target at 14 ms instead of 22, and, with
  the exclusive mic, the mic's delivery and wait adding to about a block if VB-CABLE's bursts were its schedule against the
  engine's (#13's detail, "Budget", has the expected figures). Whether VB-CABLE's own transit to the consuming app (the
  52-62 ms the cable-only test measured) changes is for the latency test's "app + output cable" to say.

## Delivery on VB-CABLE (measured 2026-11-01)
On the dev VM (EC2, Windows Server 2025) VB-CABLE delivers in bursts: its capture side gives 0, 480 or 960 frames per
10 ms event (often a fifth to a third each), the data 19-21 ms old when read (10 ms for a packet that has just filled);
its loopback is sometimes steady, sometimes 0-1440 a wake. The traces are in `%LOCALAPPDATA%\noise-filter\traces`
(`live_probe.rs`, built with `--features live-probe`).
- **Not the VM's timer.** The system timer ran at its default 15.6 ms, with nothing asking for finer; holding 0.5 ms
  from another process (`NtSetTimerResolution`) left the pattern and the age unchanged.
- **Most likely VB-CABLE's own schedule** beating against the audio engine's: a virtual cable has no hardware clock, so
  the driver moves audio on its own timer (its statistics show 128-1024-sample buffers) while the engine reads each
  endpoint every 10 ms, and two unlocked 10 ms schedules give 0, 1 or 2 packets a wake as their phases drift (the pattern
  changes from run to run). A sound card's DMA usually runs in step with the engine, so real devices should deliver
  steadily: checking that on real hardware is open. Bluetooth and some USB devices are bursty too, so the engine keeps
  handling bursts either way.
- **What it costs:** about 6 ms of mic age on average beyond a block, and the output target's spare block (10 ms) exists
  to absorb it (#13's budget). Our own driver (#19) can deliver on the engine's schedule instead.
