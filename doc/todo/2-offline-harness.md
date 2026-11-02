# 2. Offline harness: WAV in, WAV out, through the whole chain

## Why first
Every later decision needs numbers: model choice, latency, CPU, and whether a port is correct. The
offline harness gives them without audio devices. The dev box is Windows Server 2025 over Remote
Desktop, with no real mic or speakers, so the harness is the part of the work that can be built and
checked there.
- It runs under `cargo test`, which the project rules (`CLAUDE.md`) allow.
- `cargo bench` isn't on that list (check, clippy, build, test). Ask the owner once, or time
  frames inside an `#[ignore]`d test.
- Live runs are the owner's.
- The harness forces the arena's 4 KB test mode; it never takes the large-page path.

## What it runs
The same chain the live engine runs, through the same stage interface (#17), called block by block
exactly as the live engine calls it:
`AEC (optional, with a reference WAV) -> noise model -> floor/gate -> output WAV`.
- Input: a mic WAV, plus a reference (loopback) WAV for echo tests.
- Faster than real time; no threads or rings are needed for the offline path.

## Checks
1. **Latency conformance.**
   - Feed an impulse or a speech clip through, and cross-correlate input against output. This
     measures the **stream offset**: window − hop + lookahead.
   - That must equal `latency_samples − hop` exactly. `latency_samples` (spec #3) is the
     algorithmic latency, window + lookahead.
   - The remaining hop is the time spent collecting a frame. It exists only live (#13 layers 1-3).
   - Expected offsets: RNNoise 480 (of 960), UL-UNAS 256 (of 512 at 16 kHz), FastEnhancer-48k 512
     (of 1024), DFN3_ll 480 (of 960). AEC3 adds 430 (8.96 ms), measured, not ~4.5 ms
     (`nf_stages::AEC3_LATENCY`).
   - This catches framing and normalisation bugs. Known example: a scalar instead of a per-sample
     `w²` envelope in UL-UNAS synthesis caused audible periodic distortion in a third-party port.
2. **Parity.** Output must match each model's reference:
   - **UL-UNAS:** `audio/enhanced/0174.wav` from the repo (the offline model).
     - It uses `torch.stft` with `center=True`, so shift the streaming output by 256 samples
       (16 kHz) before comparing.
     - Reported errors are ~1.8e-7 (UL-UNAS PR #11 screenshot, ONNX vs offline) and 7.6e-6 max abs (a
       third-party proof of concept). Set the tolerance from our own run.
     - Alternatively, run `ulunas_onnx/stream/ulunas_stream.py` once; it writes `enh_onnx.wav`,
       which isn't in the repo.
   - **FastEnhancer:** its own reference outputs.
   - **RNNoise:** the C reference at tag v0.1.1, if needed. It has the same 2017 weights and no
     `delayed_X`; v0.2 and main use new weights and add a 480-sample frame of delay.
3. **CPU per frame.** p50, p99 and max per stage (AEC, model, gate), on the dev box and on the
   owner's desktops.

## Order
1. RNNoise (#4) alone.
2. Plus the floor and gate (#11).
3. Plus AEC3 (#10), on recorded mic and loopback pairs. The owner records them on a desktop with
   speakers.
4. Then each new model as it lands (#5).

## Built (2026-11-02)
The harness runs every stage through the CLAP-shaped interface (#17), all in plain `cargo test --workspace`. The
fixtures are synthetic and built in memory (`nf_stages::synth`); WAVs go only to temp test folders or to
`NF_STAGES_WAV_DIR`, for listening.
- **Code** (`crates/nf-stages/src/harness.rs`, not real-time code):
  - `run_chain`: moved from nf-app; `process` and `quality` use it;
  - `chain_for`: one stage in its role's slot, called as the engine calls it;
  - `run_stage` and `run_model`;
  - `impulse_offset`, `gcc_offset` (GCC-PHAT, max lag `2 × declared + 480`) and `model_offset` (hop by hop);
  - `parity`, the hook for model references later;
  - `scenes`: noisy speech, the echo room (RIR, 40 ms, ×0.5), double talk, impulses at block phases 0, 17, 301
    and 479, and dithered silence;
  - **the checks, callable outside the tests** (TODO 5's pack loader will run them on a pack), each returning what it
    measured or why it failed:
    - `check_latency(make, impulse_amp)`: an impulse at the four block phases (optional: a neural model may suppress
      a click) and GCC-PHAT on speech within 0.1 sample at > 25 dB, against `latency()`;
    - `check_model(make, impulse_amp)`: the same on a model hop by hop, against `latency_samples - hop`;
    - `check_profile(make)`: `check_host_profile`, activation (twice, the latency unmoved, at most 4096), each
      param's range, clamping and NaN refusal, the state round trip, and an idle reset changing nothing.
    Their own tests feed them stages and models that misreport by -1, +1 and a hop: both measurements catch each.
  - **`check_stage(make, &StageCheck { impulse_amp, parity })`: the one call a new stage or pack makes** (built
    2026-11-02): `check_profile`, then `check_latency` (the impulses at block phases 0, 17, 301 and 479 when the stage
    passes a click), then GCC-PHAT on 4 s of speech started at each of those block phases (within 0.1 sample at > 25 dB:
    the phase check for a stage that suppresses clicks), then 90 hostile blocks on every input (±1e4 square waves, NaN,
    ±inf, 1e17, ±`f32::MAX`, subnormals, NaN among samples, silence, a sine) with every output sample finite, then a
    reset and 2 s of noisy speech that must come through (at least 1% of its energy), then the parity hook: a
    `ParityCase { against, input, aux, expected, shift, tolerance }` run on a fresh stage and compared by `parity`,
    `Tolerance::Exact` (bit for bit) or `Within { max_abs, rms_db }`. It returns a `StageCheckReport` (the role, the
    latency report, the four phase offsets, the hostile blocks' peak, the parity), or the first failure, naming it. Its own
    test passes the pure-delay stage and catches a latency lie (-1, a block), a parity reference that differs, and a stage
    that lets a NaN through.
- **Check 1, latency conformance** (`tests/conformance.rs`, through `check_latency` and `check_model`):
  - the impulse offset equals `latency()` exactly at all four phases: RNNoise 480 (a 1e-6 impulse, floor off),
    AEC3 430 (0.5, silent reference);
  - GCC-PHAT on speech in pink noise at 30 dB: RNNoise 479.987 at 36.5 dB, AEC3 429.972 at 40.0 dB;
  - the RNNoise model hop by hop: 480 = `latency_samples - hop` (960 - 480);
  - the chain's measured offset equals `chain.latency()` for AEC on/off × noise on/off × passthrough on/off (0,
    480, 430, 910);
  - the re-blocker, with a pure-delay model at hops 160, 240, 256, 480, 512, 768, 960 and 1024, with and without
    a frame of lookahead, through a chain: the latency follows #3's formula, the output is the input delayed by it
    sample for sample, and a 0 dB floor gives exactly the delayed dry signal.
- **The one check on every stage** (`check_stage`, 2026-11-02): RNNoise (`tests/conformance.rs`): 480 at every phase
  (impulses at 1e-6; GCC-PHAT 479.983-479.996 at 36.5-36.8 dB), finite through the hostile blocks (peak 1.0, its
  clamp), bit-equal to nnnoiseless driven directly over 288 000 samples with the loud blocks; AEC3 (`src/aec3.rs`, which
  sees its dither seed): 430 at every phase, finite on both inputs, bit-equal to sonora driven directly over 4 s of echo
  with its reference; UL-UNAS (nf-ort's `tests/ort/latency.rs`), inline and on a waited-for worker, the floor off and
  then the high band passed (impulses at 0.5): 2400.000 at every phase, finite (peak 1.0), the worker bit for bit the
  stage inline.
- **Check 2, parity:**
  - the noise stage around RNNoise (floor off, gate off) is bit-equal to nnnoiseless driven directly (×32768 in,
    ÷32768 out, clamped): 288 000 samples, loud clipped blocks included;
  - AEC3 is bit-equal to sonora driven directly with the same sanitising and dither seed (a unit test in
    `src/aec3.rs`): 4 s, with NaN and infinite samples and blocks without a reference.
- **Check 3, CPU per frame** (`tests/timing.rs --release --ignored`, `noise-filter bench`; the dev VM, relative
  numbers only), p50 / p99 in µs:

  | Row | p50 | p99 |
  |---|---|---|
  | the noise stage around RNNoise | 34.1-34.3 | 43.5-59.6 |
  | the RNNoise model alone | 34.5-35.6 | 44.4-44.8 |
  | floor + gate (`NoisePost`, auto gate) | 0.4-0.5 | 0.7-0.8 |
  | AEC3 | 49.1-49.4 | 66.3-66.9 |
  | the chain, meters + RNNoise | 35.2-35.3 | 44.8-45.6 |
  | the chain, meters + AEC3 + RNNoise | 85.8-85.9 | 106.4 |

  Before the refactor, the same runs gave 34.1 (the stage), 37.8 and 90.3 (the chains): no regression. nf-app's
  bench test checks that the model and floor+gate rows allocate nothing once warm.
- **Check 4, the host profile and the interface:** `check_profile` on every stage; other formats refused; the noise
  params' defaults equal the config defaults; `set_param` clamps and reads back; the state round-trips and ignores
  unknown ids; each stage has its own extensions and not the other's.
- **Huge finite input:** a sample of about 1e17 or more used to overflow RNNoise's ×32768 to inf and abort inside
  easyfft (already so before this work). Stages now clamp their sanitised input to ±1e4; `f32::MAX` and 1e17 samples
  through the RNNoise stage and through `build_chain` with and without AEC3 come out finite (both tests abort
  without the clamp). The refactor guard still matches bit for bit.
- **Allocation tests:**
  - none for the noise stage around a pure-delay model at every hop, its resets, extensions and params, nor for
    the re-blocker (nf-core);
  - none for RNNoise's extensions and params, AEC3's telemetry, per block, nor for the RNNoise chain (nf-stages);
  - the AEC3 chain makes only sonora's own allocations.
- **On the fetched test audio** (`tests/test_audio.rs`, ignored; `NF_TEST_AUDIO` or the local cache):
  - the 10 `speech_*` takes, in cafe noise at 10 dB, through the chain: GCC-PHAT 479.99-480.00 against 480, the
    noise cut by 11.7-34.1 dB in pauses;
  - the ICASSP 2022 double-talk pair through AEC3: 429.91 against 430; AEC3 reports a 160 ms echo delay and ERLE
    6.4 dB.
- **Refactor guard** (`tests/refactor_parity.rs`, ignored, kept):
  - plays 20 s of toggled scenes through `build_chain` with and without AEC3, recording outputs, reports, meters
    and latencies; it uses only APIs a stage refactor leaves alone;
  - the golden was recorded on the tree before the stage interface, and the tree after matches it bit for bit, in
    debug and release.

## Next
1. Recorded mic + loopback pairs from the owner's desktop with speakers (order step 3), run as `test_audio.rs` runs the
   ICASSP pair.
2. Each new model as it lands (#5): `check_stage` (with `check_model` on the model alone), and its conformance pair through
   `parity`; UL-UNAS's `0174.wav` (shifted by 256 at 16 kHz) in an ignored test. Proposal (ours): the pack loader runs
   `check_stage` once per pack and caches the result by the model's SHA-256.
3. The timing rows on the owner's desktops (`noise-filter bench`).
