# 4. RNNoise baseline (`nnnoiseless`)

Status: chosen as the baseline (owner, 2026-10-26). It runs live in v0, and since 2026-11-02 it is the first model
through the offline harness (#2), as a model inside the CLAP-shaped noise stage (#17, #3). The research behind the
choice is `doc/research-2026-10.md` (Noise models); the API facts are `doc/dev/api/sonora-nnnoiseless.md`, Part 2.

## Built (2026-11-02)
- **A model, not a stage:** `nf_stages::RnnoiseModel` implements nf-core's `NoiseModel`:
  - hop 480, `latency_samples` 960 (the 960 window, no lookahead), a VAD;
  - `nf_stages::rnnoise_stage` wraps it in `NoiseStage` (`noise-filter.denoise`), which sanitises, clamps, and runs
    the floor and gate.
- **Unchanged behaviour:**
  - the scale is ×32768 in and ÷32768 out;
  - the stream offset is 480;
  - the pristine spare state is swapped in at the first reset, and a second reset rebuilds it in place (ten
    allocations);
  - the stage's activate warms up easyfft on the processing thread;
  - the output is bit for bit the old `RnnoiseStage`'s (#2's refactor guard).
- **Measured** (#2's harness):
  - the impulse offset is 480 at every block phase; GCC-PHAT on speech gives 479.987;
  - the model's offset hop by hop is 480 = 960 - 480;
  - bit-equal to nnnoiseless driven directly;
  - on the 10 fetched speech takes in cafe noise at 10 dB SNR: the offset is 479.99-480.00, and the noise is cut by
    11.7-34.1 dB in pauses.
- **CPU** (dev VM, release, p50): the model alone 34.5-35.6 µs a block, the whole noise stage 34.1-34.3 µs, the floor
  and gate 0.4-0.5 µs.
- **Allocation:** none per block once warm. The exception is a reset: the first after an activate swaps in the spare
  (none), and each later one until the next activate rebuilds the state in place, ten allocations on the audio thread
  from the engine's real-time pool. That is each passthrough end and each noise toggle after the first, until the next
  start (as before #17; now stated in the stage interface's rules).
- **Huge input:** a finite sample of about 1e17 or more overflowed the ×32768 to inf, and easyfft aborted on the NaN
  it became (undefined behaviour in release; already so before #17). The noise stage now clamps its input to ±1e4
  first. Checked with `f32::MAX` and 1e17 samples, through the stage and the chain.

## Next
- Stays the bring-up model and later the eco mode. The candidates (#5) go through the same harness and the same
  noise stage, as packs (`doc/spec/model-pack-v0.md`) or built in.
- It can't load RNNoise 0.2 models (nnnoiseless 0.5.2).
- A follow-up if the pool's ten allocations at a reset ever matter: rebuild a spent spare off the audio thread (the
  control thread could hand a fresh `DenoiseState` over), so no reset allocates.
