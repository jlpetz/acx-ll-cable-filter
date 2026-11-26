# 11-12. Noise floor, gate and input meter

## Noise floor (maximum attenuation)
The floor clamps how much the model may cut, e.g. to −20 dB. This is the same idea as DeepFilterNet's
attenuation limit and Poise's `atten_lim_db`. The shipped interfaces output spectra or audio, not
masks, so the approach depends on the model:
- **Spectrum-output models** (UL-UNAS `enh`, FastEnhancer `spec_out`):
  - recover the per-bin gain as |enh|/|mix|. That is exact for UL-UNAS's real sigmoid mask; check
    it for FastEnhancer.
  - clamp the gain to the floor and re-apply it, or mix in delay-matched dry signal.
- **RNNoise via `nnnoiseless`** returns audio and a VAD value only; its band gains are internal. A
  floor needs a small patch to the crate, or the dry mix.
- **Cost:** it works inside the same frames, so no added latency. It also hides artifacts:
  GTCRN, on whose backbone UL-UNAS is built, leaves musical noise on noise-only stretches
  (gtcrn #76).

## Gate
- **Floor:** static (the user sets a threshold), or auto (track the noise floor; tunable rise and
  fall rates).
- **Envelope:** attack/release and hold, no lookahead, so no added latency.
- The gate is off by default. Its default position is after the denoiser's floor mix and before the external effect.
  It can also run on its own, before AEC, before denoising, or after the effect. It measures the level at its selected position.
  Speech detection can hold it open where the upstream denoiser supplies a probability aligned to the same block.
- Try quiet word endings and pauses with AGC2 both on and off to hear whether the gain brings room noise back up.

Settings saved by an older window keep their old sound. If the denoiser used to be off, its gate was bypassed too.
Gate-only processing is explicit in a new bare engine JSON config with e.g. `"gate_position":"AfterNoise"` alongside
`"noise_enabled":false` and `"gate":{"mode":"Static"}` inside `processing`. Old unversioned chain recordings omitted
that position. The CLI also accepts `--no-ns --gate static` to request a gate-only render.

## Input level meter (owner's request)
- **What it shows:**
  - raw input peak and RMS in dBFS, with peak hold, measured before any processing;
  - a clip light that latches at about −0.1 dBFS, or when several samples in a row sit near full
    scale (a flat top means the hardware clipped). Click to reset.
- **Caveat:** in shared mode Windows' mic level slider and any driver effects run before the app
  sees the audio, so the meter shows what the app receives. That is also what matters for clipping.
- **Cost:** the DSP cost is one pass per block; the real cost is GUI redraws. Toggle on/off, about
  20 Hz.
  - Closing the current window stops its engine and meter updates. TODO 16 covers the later tray process.
  - Pausing while minimised has no Reactor 0.100.0 event (#15), so for now that relies on the
    toggle.
- **Current window:** the audio thread publishes peak/RMS in atomics. The window reads the engine's telemetry
  in-process with a re-armed `spawn_background` waiter. With the meters on it refreshes about 20 times a second.
  It shows both input and output, gain reduction and speech probability. The input clip badge resets on click.
- **Phase 1, before the GUI exists:** a console readout (peak/RMS dBFS, clip latch) from the same
  atomics. Built (2026-11-02): `run`'s status line has shown the input's and the output's peak/RMS since v0; its `CLIP` is
  now the latch taken by each line (`Engine::take_clip`, a swap), so it shows on the line after a clip and clears, where it
  stayed on every line after the first clip before; `near-clip` is the last second within 0.1 dB of full scale; `run`'s
  summary counts the seconds that clipped. The window's latch and its reset are unchanged.
