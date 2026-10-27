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
- **Detector:** RNNoise's VAD probability, or the model's gain energy. The gate is the noise stage's last step,
  after AEC and the model and before any effects (#17). With a third-party denoiser, a level-based
  gate runs right after it.

## Input level meter (owner's request)
- **What it shows:**
  - raw input peak and RMS in dBFS, with peak hold, measured before any processing;
  - a clip light that latches at about −0.1 dBFS, or when several samples in a row sit near full
    scale (a flat top means the hardware clipped). Click to reset.
- **Caveat:** in shared mode Windows' mic level slider and any driver effects run before the app
  sees the audio, so the meter shows what the app receives. That is also what matters for clipping.
- **Cost:** the DSP cost is one pass per block; the real cost is GUI redraws. Toggle on/off, about
  20 Hz.
  - The engine stops sending meter values when the settings window closes (its process
    disconnects from the pipe).
  - Pausing while minimised has no Reactor 0.100.0 event (#15), so for now that relies on the
    toggle.
- **Plumbing:** the audio thread publishes peak/RMS in atomics. The engine's control thread samples
  them at about 20 Hz and sends them over the named pipe (#15). The settings window receives them
  through tmr-gui's re-armed `spawn_background` waiter.
- **Phase 1, before the GUI exists:** a console readout (peak/RMS dBFS, clip latch) from the same
  atomics.
- **Later:** output level and gain reduction, which make tuning the floor and gate easier.
