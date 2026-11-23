# Architecture (v0, 2026-10-26)

The base app: RNNoise is the only noise model, the AEC is WebRTC AEC3 (sonora), output goes to a
user-installed virtual cable, latency is measurable end to end, and a Reactor settings window is
included. It is built to take more noise models later without touching the engine.

## Crates
| Crate | What | Windows deps |
|---|---|---|
| `nf-core` | Constants, the CLAP-shaped stage interface, the chain, the noise stage and its model trait (with `Reblocked` and `Pipelined`, the inference worker), controls (atomics), config and telemetry types, DSP (meter, gate, floor mix, delay, slip resampler, drift controller, fades, the host STFT, the high-band policy), and latency analysis (sweep, deconvolution, arrival detection, rolling stats). Pure Rust, unit-tested. | none |
| `nf-mem` | The locked memory pool (large pages by default, 4 KB + `VirtualLock` fallback), the lock-pages privilege (enable, elevated grant to a given SID), the working-set minimum, and `RtAllocator`, the global-allocator router. | yes |
| `nf-stages` | `RnnoiseModel` (nnnoiseless) in nf-core's noise stage (`rnnoise_stage`), `Aec3Stage` (sonora), `build_chain`, and TODO 2's offline harness (`harness`, `synth`, `bench`) with its tests. | none |
| `nf-ort` | Model packs through ONNX Runtime (TODO 5, evaluation only): the manifest and its checks, the session with preallocated I/O, `OrtSpectralModel` on the bin-mapped 48 kHz STFT, the conformance runner, the heap probe and the pack's bench. The only crate that links ONNX Runtime; nf-engine and nf-app take it with their `onnx` feature, off by default. | yes (CNG, the probe) |
| `nf-engine` | WASAPI devices and streams, the MMCSS audio thread, drift control, telemetry, the latency readout, the latency test runner, and the control thread. Public API in `api.rs`. | yes |
| `nf-gui` | The Reactor settings window and settings persistence. | via Reactor |
| `nf-app` | `noise-filter.exe`: the window by default; CLI subcommands `run`, `devices`, `latency`, `quality`, `process`, `bench`, `setup-large-pages`. Installs `RtAllocator` as `#[global_allocator]`. | yes |

## Processing
- Fixed format inside the app: **48 kHz, mono, f32, 480-frame (10 ms) blocks** (`nf_core::BLOCK`).
  Devices are converted at the edge: channel downmix or upmix, plus WASAPI autoconvert when a
  device doesn't run at 48 kHz.
- **One audio thread** (MMCSS "Pro Audio", real-time allocator scope; no FTZ/DAZ of our own (sonora 0.3.0's guard, both for the length of each of its calls, is built only with nf-stages' `sonora-denormal-guard`, off by default: TODO 10, `vendor/README.md`, `tests/mxcsr.rs`), the
  reference blocks, and the mic's while a stage runs, are dithered at about -180 dBFS instead; in bypass and in measurement passthrough silence stays exact), kept between runs and driven by the
  mic's capture events. For each complete mic block:
  1. read the loopback reference (keep-alive render keeps it flowing) into the reference FIFO,
     slipped to the mic clock;
  2. `Chain::process`:
     - input meter (raw mic);
     - AEC stage (if enabled; mic plus reference);
     - noise stage (if enabled; the model (RNNoise), then floor mix and gate inside it);
     - output meter;
  3. slip the output to the cable's clock and write it to the cable's render buffer;
  4. per-block timing, latency readout and telemetry atomics.
- **CPU baseline:** x86-64-v3 (AVX2, BMI1/2, FMA; `.cargo/config.toml`, since 2026-10-28). Both
  exes check the CPU first thing and refuse one without it with a message and exit code 5
  (`crates/nf-app/src/cpu_check.rs`), rather than fault on an illegal instruction.
- Stage CPU use is small. On the dev VM at x86-64-v3 (relative numbers only), per block: RNNoise
  about 36 µs, AEC3 about 50 µs and the whole chain about 85 µs (doc/dev/build-report.md §11). At
  x86-64-v2 they were 40, 54 and 97 µs, with sonora's `mul_add` patched to plain arithmetic (AEC3
  took 91 µs before the patch). The i9-14900K figure for AEC3 in the research is 45 µs. RNNoise's
  inference stays on the audio thread. A model pack (ONNX Runtime, which allocates every run) runs
  on an inference worker inside the noise stage, `depth` blocks behind (2 by default: two-block
  wakes are common, and the engine processes a wake's blocks back to back), about 1 ms a run for
  UL-UNAS (TODO 6; engine.md, "The inference worker").
- **Toggles** (nf-core `chain.rs` has the details):
  - Turning a stage on resets it and primes it with the last block's input, so its first block is
    processed audio. A stage whose latency is over a block (a model pack on its worker) plays its
    delay line through the rest of its start-up and then crossfades in. Latency 0 to the stage's
    goes in with a dip splice (5 ms), not a crossfade of two misaligned copies, which would comb.
  - Turning a stage off crossfades into a delay line of its latency and stops running it at once,
    so CPU is saved. The latency drops later, with a dip splice, at the first quiet block (or
    after 1.5 s, or when a latency test starts).
  - Measurement *passthrough* replaces each enabled stage with a delay line of its latency, so the
    measured delay equals the live configuration's delay; the switch is a crossfade.

## Stage interface (CLAP-shaped, TODO 17: the owner, 2026-11-02)
Every stage, ours included, follows CLAP's model through the Rust trait `nf_core::stage::Stage`, in-process
with no FFI. `doc/spec/host-profile-v0.md` is the published side; `.clap` export and an fx-host for
third-party plugins come later.
- **Descriptor** (`descriptor()`): id, name, vendor, version, description and feature tags. The role comes
  from a tag: `noise-filter:echo-cancel`, `noise-filter:denoise`, or neither (an effect). The namespace
  `noise-filter` is provisional (one macro, `nf_core::namespaced!`).
- **Ports** (`audio_ports()`): a main mono input and output at index 0. The echo canceller takes the
  speaker reference on a non-main input (CLAP's sidechain); the chain gives the reference to any stage
  with a second input, the same block in every slot (not delayed by upstream slots, so the engine's lead is
  the AEC slot's alone). Descriptor, port and param text is `Cow<'static, str>`: literals for our stages,
  owned for what is only known at run time (a model pack, later a third-party plugin).
- **`activate(&Activation)`**: always `Activation::HOST` (48 kHz, 480-frame blocks). It may allocate, and
  runs on the audio thread inside its real-time scope (a deviation from CLAP's main thread, for easyfft).
  `Chain::activate` first checks each stage against the host profile and its slot (`check_host_profile`).
- **`process(ProcessIo { inputs, output })`** and **`reset()`** are real-time: no allocation, locks,
  syscalls or logging. A reset with nothing processed since the last one is a no-op. RNNoise's reset swaps in
  a pristine state that a thread of its own built (two kept, replaced within 100 ms; TODO 4). One known
  exception: a third real reset within 100 ms of two others finds none and rebuilds the state in place (ten
  allocations from the engine's real-time pool; counted).
- **`latency()`** follows CLAP semantics: the samples by which output lags input in the stream (the
  stream offset, *not* the algorithmic latency, which is the offset plus one hop). The chain caches it at
  activate. RNNoise is 480. AEC3 is 430 (8.96 ms), measured with an impulse (`nf_stages::AEC3_LATENCY`),
  so the chain with both is 910 (18.96 ms).
- **Params** (`params()`, `param_value`, `set_param`): the noise stage's floor and gate, ids 1-9, over the
  same atomics `ChainControls` holds (`ChainControls::apply` still writes them whole, from any thread).
  `set_param` takes `&mut self`: whoever holds the stage calls it, on the processing thread between blocks
  while it is active (CLAP's in-events); no other thread reaches a stage. **State** (`save_state`,
  `load_state`): the params in a small binary blob. Bypass (on, off, passthrough) is the host's, not a param.
- **Extensions** are typed getters returning `Option<&dyn Ext>`: `gain_adjustment()`
  (`clap.gain-adjustment-metering/0`), `speech_probability()` and `aec_telemetry()` (ours). The chain's
  per-block `ChainReport::noise` and `Chain::aec_stats` read them.
- **The noise stage** (`nf_core::noise::NoiseStage`) is one stage for every noise model: it sanitises the
  input (NaN and ±inf to 0, the rest clamped to ±1e4 so no model overflows), runs a `NoiseModel`
  (RNNoise now, ONNX adapters in TODO 5), clamps, then the floor mix and the gate (`NoisePost`). A model
  with a hop other than 480 is re-blocked, at `hop - gcd(hop, 480)` more latency
  (`doc/spec/model-pack-v0.md`).

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
