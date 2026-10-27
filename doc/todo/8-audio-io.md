# 8-9. Audio I/O, clocks and buffering

Facts and sources: `doc/research-2026-10.md` (Windows audio I/O).

## Streams
| Stream | Endpoint | Mode | Notes |
|---|---|---|---|
| Mic capture | chosen mic | event-driven shared mode; IAudioClient3 minimum period if the driver allows, else 10 ms; exclusive as an advanced option | RAW mode where `RawProcessingSupported`; not the Communications category |
| Cable render | chosen virtual cable input (e.g. CABLE Input) | event-driven shared mode | `SetDuckingPreference(TRUE)`; small period or RATEADJUST, not both |
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
