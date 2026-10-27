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
