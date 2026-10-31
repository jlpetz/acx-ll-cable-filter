# 10. Echo cancellation (WebRTC AEC3 via `sonora`)

Facts and sources: `doc/research-2026-10.md` (Echo cancellation).

## Choice
- **`sonora`:** a pure-Rust port of WebRTC's audio processing (BSD-3), with Windows CI. It runs AEC3 at 48 kHz in 10 ms frames; about 45 µs per frame on an i9-14900K once denormals are handled.
- **Ruled out:**
  - tonarino `webrtc-audio-processing`: doesn't build on MSVC (PR #102 open).
  - SpeexDSP: no delay estimator, linear only.
  - Voice Capture DSP: at most 22.05 kHz.
  - the `aec3` crate (RubyBit/aec3-rs 0.4.0; MIT OR BSD-3-Clause, with a LICENSE GitHub can't classify): its README calls it a work in progress, with an API expected to change.

## Getting a real-time-safe build
- **0.2.0 vs main:** the release (2026-07-29) is the M145 port. Main has further upstream fixes up to M156, including render-buffer headroom on underruns, which matters for loopback gaps, and sonora #37 (fmaf ignoring FTZ under `+crt-static`).
- **Open PRs:**
  - sonora #38: denormal stall on a silent reference, stacked on sonora #36.
  - sonora #40: allocation-free processing.
  - sonora #41: custom AEC3 config. One downstream user needed it for double-talk tuning; it also exposes `num_filters` for a wider delay window.
- **`mul_add` (sonora #37) no longer matters to us (2026-10-28):** the build moved to x86-64-v3, where each `mul_add` is one FMA instruction, so the exes call no `fmaf`. The vendored 0.2.0 with the AEC path's `mul_add`s as plain arithmetic (2026-10-26, AEC3 91 → 55 µs a block at v2) went with the move; crates.io's 0.2.0 takes about 50 µs at v3 (`doc/dev/build-report.md` §11).
- **Plan:** vendor a fork of main with sonora #38 and #40 (and #41) merged. Until then, FTZ/DAZ on the DSP thread plus ~1e-9 dither on the reference, which zero-filled gaps make mandatory.
- **One thread:** sonora's API takes `&mut self` for both render and capture, so one DSP thread owns the instance and feeds the reference frame before the matching mic frame.
- **Full band:** set `MaxProcessingRate::Rate48kHz`. sonora's default (`Rate32kHz`, as in upstream WebRTC) caps the whole capture path at 16 kHz whatever noise model runs.
  - Rate48kHz routes audio through WebRTC's three-band filter bank (0-8/8-16/16-24 kHz). It's not perfect-reconstruction (~9.5 dB round-trip SNR), so A/B it against 32 kHz in #14.
  - AEC3 runs its linear filter on the bottom band only. Above 8 kHz it applies one scalar gain plus comfort noise, so leftover far-end speech above 8 kHz is handled coarsely, whatever the noise model's rate.

## As a stage (proposal, #17, awaiting the owner)
- A CLAP-shaped stage (#17): main input = mic, second (non-main) input port = speaker reference,
  main output = echo-cancelled mic.
- The engine resamples the reference to the mic clock (#9) and feeds it in the same 10 ms block as
  the mic.
- Latency is reported through the latency extension: 430 samples (8.96 ms) at 48 kHz, measured with an impulse and GCC-PHAT (`nf_stages::AEC3_LATENCY`, `crates/nf-stages/tests/aec3.rs`). The earlier ~4.5 ms was a code-reading estimate and too low (`doc/dev/api/sonora-nnnoiseless.md` 1.8).

## Behaviour and UX
- **Order:** AEC → noise model → floor/gate. Nothing nonlinear or time-varying runs before AEC.
- **Residual echo:** the noise model keeps leftover far-end echo, because it's speech. So AEC3's residual-echo suppressor, which sees the reference, stays enabled and does that job. A joint mic+reference neural model is the higher-quality later option (see Later).
- **One frame clock:** all stages share the 10 ms frame, with no re-buffering between them. That's the useful part of the "single pass" idea; separate FFTs per stage cost microseconds.
- **Opt-in "using speakers" mode,** off for headphones. With closed headphones there's little echo, so AEC there would mostly risk double-talk artifacts (our inference, unverified).
- **Warnings and guards:**
  - Block the virtual cable, and our own monitor output, from being picked as the reference.
  - Warn when the noise filter is on without AEC while on speakers: Discord's own AEC then sees an echo path through a time-varying denoiser.
- **Stats:** show AEC3's delay estimate and ERLE.
- **Delay window:** the default delay-estimation window is about 608 ms (our calculation from the code). Bluetooth or USB speakers with large output latency may need a bigger window (custom config, sonora #41).
- **Reconvergence:** volume changes (loopback is pre-volume), render-side enhancement APOs and stalls all trigger it; one report took about a minute after a heavy stall.

## First live test (2026-11-01)
`quality --test echo` over the dev VM's VB-CABLE cables (#14's detail file has the runs): AEC3 removed only 1-5 dB of echo a
far-only turn (7-23 dB with the base cable as the far end), where the same scene offline removes 21-53 dB, with a 96 ms
lead and 30 ppm of drift too. The engine counted 5-8 reference gaps a second whichever cable carried the reference: its
reference FIFO (target one loopback period + one block + 96 samples, 22 ms) found less than a block at a mic wake. Each gap
zero-fills to the target and resets the drift loop, and the coarse correction later discards the excess. Both move the
reference against the mic, so AEC3's linear filter never holds (its delay estimate wandered from 96 to 116 ms in one take).

Next:
1. Log the loopback packets (count, size, QPC) against the mic wakes on the cables, to see whether the loopback comes in
   bursts (VB-CABLE's statistics show 1024-frame buffers) or stops.
2. Proposed fix: a late loopback packet should be waited for, not zero-filled. The reference's latency costs nothing at the
   output, only its lead over the echo, so a deeper target (raised on an underflow, as a jitter buffer would) keeps the
   timeline whole. It has to stay inside the echo path's own delay (loopback taps at mix time, ahead of the speakers).
3. Then rerun the echo test, and on real speakers in phase 2.

## Later
- **Optional Windows path:** open the mic in Communications mode, check `IAudioEffectsManager` for AEC/NS, and set the reference with `IAcousticEchoCancellationControl` (22621+; E_NOINTERFACE when unsupported).
  - Where the OEM supplies no Communications-mode processing, this path gets Voice Clarity (Windows 11 client editions).
  - Whether Voice Clarity accepts a chosen reference through `IAcousticEchoCancellationControl` is unverified. Without that, its reference is the default render device.
- **Neural AEC (v2 experiment):** LocalVQE (Apache-2.0, 16 kHz, GGML C++). v1.4-AEC has 203K params; the current v1.3 is a joint AEC+NS model with 4.8M params.
