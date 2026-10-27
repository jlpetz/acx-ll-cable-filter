# Architecture (v0, 2026-10-26)

The base app: RNNoise is the only noise model, the AEC is WebRTC AEC3 (sonora), output goes to a
user-installed virtual cable, latency is measurable end to end, and a Reactor settings window is
included. It is built to take more noise models later without touching the engine.

## Crates
| Crate | What | Windows deps |
|---|---|---|
| `nf-core` | Constants, the stage interface, the chain, controls (atomics), config and telemetry types, DSP (meter, gate, floor mix, delay, slip resampler, drift controller, fades), and latency analysis (sweep, deconvolution, arrival detection, rolling stats). Pure Rust, unit-tested. | none |
| `nf-mem` | The locked memory pool (large pages by default, 4 KB + `VirtualLock` fallback), the lock-pages privilege (enable, elevated grant to a given SID), the working-set minimum, and `RtAllocator`, the global-allocator router. | yes |
| `nf-stages` | `RnnoiseStage` (nnnoiseless, with `NoisePost`: floor mix + gate) and `Aec3Stage` (sonora). Offline harness tests. | none |
| `nf-engine` | WASAPI devices and streams, the MMCSS audio thread, drift control, telemetry, the latency readout, the latency test runner, and the control thread. Public API in `api.rs`. | yes |
| `nf-gui` | The Reactor settings window and settings persistence. | via Reactor |
| `nf-app` | `noise-filter.exe`: the window by default; CLI subcommands `run`, `devices`, `latency`, `process`, `bench`, `setup-large-pages`. Installs `RtAllocator` as `#[global_allocator]`. | yes |

## Processing
- Fixed format inside the app: **48 kHz, mono, f32, 480-frame (10 ms) blocks** (`nf_core::BLOCK`).
  Devices are converted at the edge: channel downmix or upmix, plus WASAPI autoconvert when a
  device doesn't run at 48 kHz.
- **One audio thread** (MMCSS "Pro Audio", real-time allocator scope; no FTZ/DAZ, the mic and
  reference blocks are dithered at about -180 dBFS instead), kept between runs and driven by the
  mic's capture events. For each complete mic block:
  1. read the loopback reference (keep-alive render keeps it flowing) into the reference FIFO,
     slipped to the mic clock;
  2. `Chain::process`:
     - input meter (raw mic);
     - AEC stage (if enabled; mic plus reference);
     - noise stage (if enabled; RNNoise, then floor mix and gate inside it);
     - output meter;
  3. slip the output to the cable's clock and write it to the cable's render buffer;
  4. per-block timing, latency readout and telemetry atomics.
- Stage CPU use is small. On the dev VM at x86-64-v2 (relative numbers only), per block: RNNoise
  about 40 µs, AEC3 about 55 µs (with sonora's `mul_add` patched to plain arithmetic: 91 µs
  before), and the whole chain about 97 µs (doc/dev/build-report.md §9). The i9-14900K figure for
  AEC3 in the research is 45 µs. Inference stays on the audio thread; a worker thread is reserved
  for heavy future models (`StageInfo::needs_worker`).
- **Toggles** (nf-core `chain.rs` has the details):
  - Turning a stage on resets it and primes it with the last block's input, so its first block is
    processed audio. Latency 0 to the stage's goes in with a dip splice (5 ms), not a crossfade of
    two misaligned copies, which would comb.
  - Turning a stage off crossfades into a delay line of its latency and stops running it at once,
    so CPU is saved. The latency drops later, with a dip splice, at the first quiet block (or
    after 1.5 s, or when a latency test starts).
  - Measurement *passthrough* replaces each enabled stage with a delay line of its latency, so the
    measured delay equals the live configuration's delay; the switch is a crossfade.

## Stage interface (CLAP-shaped; whether it is exported as CLAP is TODO #17's open proposal)
- `info()` gives id, name, role, whether a second input port (the speaker reference) is needed, and
  whether a worker is needed.
- `activate(sample_rate, block)` may allocate. `process(io)` is real-time: no allocation, locks,
  syscalls or logging.
- `latency()` follows CLAP semantics: the samples by which output lags input in the stream (the
  stream offset, *not* the algorithmic latency, which is the offset plus one hop). RNNoise is 480.
  AEC3 is 430 (8.96 ms), measured with an impulse (`nf_stages::AEC3_LATENCY`), so the chain with both
  is 910 (18.96 ms).
- Controls are atomics shared through `Arc`, read once per block (as CLAP params would be).

## Clocks
- **The mic is the master clock.** The reference (loopback) and output (cable) each have their own
  clock.
- `DriftController` (a PI loop on the filtered FIFO fill) produces a ratio. `SlipResampler`
  realises it by emitting 479 or 481 samples instead of 480 in a block, using cubic Hermite
  interpolation over that block. That means no added latency and no audible pitch artifacts.

## Memory
- At start-up `nf-mem` reserves one pool: large pages (2 MiB multiples) if the lock-pages privilege
  is enabled, otherwise 4 KB pages locked with `VirtualLock`, with the reason recorded.
- It also sets a hard working-set minimum.
- `RtAllocator` serves allocations from the pool's TLSF on any thread inside an `RtScope` (the audio
  thread for its whole life, including stage construction and activation). Frees are routed by
  address.
- Allocations counted after warm-up show up as telemetry; tests assert zero.
- Tests never take the large-page path (CLAUDE.md).

## Latency
- **Readout (always on):** the capture QPC of a block's first sample against its predicted play-out
  time on the cable (`AudioClock`). Median and p95 over 1 s. It excludes the cable's internal pipe
  and the consuming app.
- **Test (virtual or acoustic):**
  - The app plays exponential sweeps into a probe device: cable A's input, or the speakers.
  - It taps its own mic input (with QPC) and captures the output cable's capture endpoint (with QPC).
  - Deconvolution gives each arrival. The tap-to-output difference is app + cable; probe-to-tap is
    the input path.
  - The chain is in passthrough, with AEC bypassed, unless "through model" is chosen (that needs a
    speech probe).

## Threads
| Thread | Priority | Role |
|---|---|---|
| UI (Reactor) | normal | window, settings; polls telemetry at 20 Hz |
| control | normal | commands, device changes, stream (re)builds, the timed retry, the lost-reference check |
| audio | MMCSS Pro Audio | capture → chain → render; one thread kept between runs |
| latency test | MMCSS Pro Audio while running | probe render, output capture, analysis afterwards |
