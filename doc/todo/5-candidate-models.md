# 5. Candidate models: UL-UNAS through ONNX Runtime (for evaluation)

Status: UL-UNAS built in as the first model pack, for evaluation only (2026-11-02, uncommitted when written). RNNoise stays
the default; the patent question stays with the owner (TODO 5's entry): nothing here ships UL-UNAS, and its pack is fetched
by a developer, never bundled. The research is `doc/research-2026-10.md` ("Noise models", "UL-UNAS streaming ONNX"); the
spec is `doc/spec/model-pack-v0.md`; the memory side is `doc/todo/6-inference-memory.md`.

## What was built
- **The pack:** `packs/ulunas/manifest.json` (the manifest is also the fetch recipe; no weights or audio in the repo, and
  `.gitignore` now refuses `*.onnx`, `*.ort` and `*.tar` too). `python -I tools/fetch_models.py [--dir DIR] [--pin] [ID ...]`
  fetches it into `%LOCALAPPDATA%\noise-filter\models\ulunas\` (`NF_MODELS` overrides), checks every file's size, SHA-256
  and git blob id, never executes, imports or parses a download, and copies the manifest last. Tests:
  `python -I tools/test_fetch_models.py` (16, offline).
- **nf-ort** (the only crate linking ONNX Runtime): the pack loader and its refusals, the one environment, the session with
  preallocated I/O, `OrtSpectralModel` (bin-mapped 48 kHz) and `NativeSpectral` (offline), the conformance runner, the heap
  probe, the bench. Its integration tests are one binary, `crates/nf-ort/tests/ort/`.
- **nf-core:** the host STFT (`dsp::stft`), the high-band policy (`dsp::high_band`, `ProcessingConfig.high_band`, live),
  `noise::Reblocked`, and `noise::Pipelined`, the inference worker (TODO 6's exception; `doc/design/engine.md`, "The inference
  worker").
- **Selection:** `ProcessingConfig.noise_model` (default `"rnnoise"`), `--model NAME` and `--high-band model|zero|pass` on
  `run`, `latency`, `quality` and `process`, and `bench --model NAME`. `nf_engine::models::build_chain` is the one chain
  constructor (the audio thread's with the model on its worker; the offline tools' inline). A model change restarts the
  engine. A missing pack says how to fetch it; a build without the `onnx` feature says it needs one; a pack that fails to
  load fails the start (no silent fallback to RNNoise).
- **The `onnx` feature** (nf-engine, nf-app), off by default: the default exe is unchanged. The owner builds the evaluation
  variant with `cargo build --release --features onnx`.

## Decisions (ours, from the design step; the owner's are marked)
- **Pin `ort = "=2.0.0-rc.15"`** (still the newest on crates.io on 2026-11-02; ONNX Runtime 1.31.0, ort-sys rc.15). Its
  minimum Rust is 1.92, so the workspace's `rust-version` went from 1.91 to 1.92. Features: `default-features = false`,
  `std`, `download-binaries`, `tls-native`, `api-31`; no `ndarray`, `tracing` or `copy-dylibs`.
- **The binary:** ort-sys downloads pyke's static build at build time and checks it against its own table:
  `x86_64-pc-windows-msvc+directml.tar.lzma2`, SHA-256 `f99aaef23f12986abb05924c944d8433521263ad756b462df762a5a982c98405`,
  cached in `%LOCALAPPDATA%\ort.pyke.io\dfbin\x86_64-pc-windows-msvc\<hash>\` (a 360 MB `onnxruntime.lib` and
  `DirectML.dll`). rc.15 has no CPU-only Windows build. Our fetch tool handles data only.
- **DirectML, D3D12 and DXGI are delay-loaded** (nf-ort's, nf-engine's and nf-app's build.rs; nf-app's only for the
  `noise-filter` binary, so the launcher doesn't warn). `dumpbin /dependents` on an nf-ort test exe and on the onnx app lists
  them as delay-load dependencies only; nothing in ONNX Runtime 1.31 imports `dxcore.dll`. A new run-time dependency
  remains: `MSVCP140.dll` (and `_1`), the C++ runtime, which the VC++ redistributable provides (TODO 18).
- **`ulunas_stream_simple.onnx`, not `ulunas_stream.onnx`:** static shapes, 697 nodes against 5,482 (2,412 of them
  `Constant`), and ONNX Runtime's cost is per node.
- **No resampler:** the bin-mapped 48 kHz STFT (1536/768, bins 0-255 ÷ 3, bin 256 as `2·Re(X[256]) / 3`, real as the
  model's own Nyquist bin is; host bins 256 and up from the high-band policy). No added latency, no aliasing in the band, a
  whole-sample latency, full band in one STFT.
- **The model runs on a worker, never the audio thread** (CLAUDE.md's exception): each block's result is used `depth` blocks
  later, live and offline alike. **Depth 2 by default** (after the review: ours, `doc/todo/6-inference-memory.md`, "The
  pipeline's depth"): the engine processes a wake's blocks back to back, so at depth 1 every two-block wake (VB-CABLE's
  bursts, a 20 ms device period, a late wake) missed a block. Cost: 2 × 480 samples. `--model-depth 1-4` sets it.
- **Latency per layer:** RNNoise 480 (20 ms live with 10 ms blocks); UL-UNAS on the audio thread would be 1440 (forbidden);
  **on the worker 2400** at depth 2 (60 ms live; 1920 and 50 ms at depth 1); the chain with AEC3 and UL-UNAS 2830.
- **Model files** load from memory, read and hashed on the worker (not the audio thread or its pool; the conformance files
  only at fetch and test time): the host hashes the bytes it hands to `commit_from_memory`, so nothing can be swapped
  between the check and the load. A tensor whose data lives in another file is refused before ONNX Runtime sees the model
  (`onnx_check.rs`): ONNX Runtime would resolve it against the current folder, so "no base path" wasn't a protection.

## Measured (2026-11-02, the dev VM: Windows Server 2025 over Remote Desktop, 8 vCPUs; these figures say nothing about the owner's desktops)

### Offsets, to the sample (`tests/ort/latency.rs`)
| Layer | Declared | GCC-PHAT on speech |
|---|---|---|
| native 512/256 (offline, `check_model`) | 256 | 256.001 at 34.2 dB |
| bin-mapped 1536/768 | 768 | 768.000 at 44.8 dB |
| `Reblocked` | 1440 | 1440.000 at 46.3 dB |
| `Pipelined`, depth 2 (the default) | 2400 | 2400.000 at 48.2 dB |
| `Pipelined`, depth 1 | 1920 | 1920.000 at 47.4 dB |
| the stage (`check_latency`), inline and on a worker | 2400 | 2400.000 at 49.4 dB |

`check_profile` passes. With the high band passed through (`HighBand::Pass`: a click's content above 8 kHz reaches the
output unmasked), impulses of 0.5 and 0.05 at hop phases 17, hop/2, 0 and hop - 1 come out at the declared offset for the
bin-mapped model (768), `Reblocked` (1440) and `Pipelined` (2400), and at block phases 17, 301, 0 and 479 for the stage,
inline and on a waited-for worker (2400): no phase-dependent framing error. Without it the impulse check is skipped (the
model may suppress a click in its band). The chain in all 8 modes (AEC on/off × noise on/off × passthrough) measures 0, 0,
2400, 2400, 430, 430, 2830 and 2830, each its reported latency (the first run's figures at depth 1: 1920 and 2350, GCC
2349.970 at 43.9 dB). Square waves at ±1e4, NaN, ±inf and 1e17 come out finite, the model's part within ±1, and speech
comes through after a reset.

### Parity (`tests/ort/parity.rs`)
- **P1** (native framing against an independent f64 STFT, a plainly run session with ONNX Runtime's default options, and an
  f64 WOLA inverse): noisy_0174 max 1.06e-5, RMS error -94.8 dB; noisy_0119 max 5.7e-7, -122.9 dB. The bound is pinned at
  2e-5 (the design guessed 1e-5).
- **P2** (the conformance pairs framed as torch frames them, against upstream's `enhanced_*.wav` as PCM16): 0174 max 4 LSB,
  RMS error -64.1 dB, 909 of 160,000 samples over one LSB; 0119 max 1 LSB, -66.1 dB. Far tighter than the "around -40 dB"
  expected of a CUDA-made, offline-model reference. Pinned in the manifest: `max_abs` 2e-4 (6.5 LSB), `rms_db` -60.
- **P3:** see `doc/todo/3-model-spec.md` ("P3"): with bin 256 mapped real (after the review) the input matches below 7.75
  kHz and at bin 256, and the model's output matches native to -46.4 dB below 4 kHz (-38.9 with bin 256 complex, as first
  designed); folding the top 8 bins too would take it to -57.5 dB or better.

### CPU (release, `cargo test -p nf-ort --release --test ort -- --ignored timing --nocapture`; `bench --model ulunas` prints the same rows)
The 30 s timing rows (the second run, after the review's fixes):

| Row | p50 µs | p99 | max | Rust allocs | C heap calls |
|---|---|---|---|---|---|
| ONNX Runtime run, per hop | 968 | 1051 | 1120 | 0 | 7420 a run |
| model per hop (STFT, run, inverse) | 980 | 1030 | 1296 | 0 | 7420 a hop |
| stage per block, model inline | 959 (mean 613) | 1027 | 1083 | 0 | |
| stage per block, audio side, model on its worker (4× real time) | 4.2 | 5.1 | 11.9 | 0 | 0 |
| RNNoise stage per block | 37.8 | 47.1 | 51.3 | 0 | |

For the worker's headroom quote the longer runs: TODO 6's 10,000-run steady state gave p50 950-969 µs, **p99 1,053-1,961,
p99.9 1,106-2,869 and max 1,449-5,405** over its two runs (the VM's scheduling moves the tail), all far under the
deadline (a block, 10 ms, `depth` calls later). The worker over the timing row's 30 s: 1874 runs, 0 misses, run p50 1034 µs,
p99 1220, max 1812; **0 Rust allocations** after it was ready (the first report's 2 were the model info's clone sent with
"ready": TODO 6); 7420 C heap calls a run. So UL-UNAS costs about 6% of a core (0.97 ms × 62.5 hops a second), and the audio
thread a few µs a block. In an earlier run with both threads at normal priority the audio side once took 1.68 ms (the woken
worker on its core, presumably); in the app the worker runs at MMCSS Normal under the audio thread's Critical, and without
MMCSS at HIGHEST, under the audio thread's TIME_CRITICAL fallback.

Fed as bursty wakes at real time (`nf_ort::bench::bursts`, 300 blocks each): wakes of 1 block lose none at depth 1 or 2;
wakes of 2 lose 49.3% at depth 1 and none at depth 2; wakes of 3 lose 66.0% at depth 1 and 32.7% at depth 2 (TODO 6,
"The pipeline's depth").

### Quality on the fetched test audio (`tests/ort/quality.rs`, ignored; each of the 10 `speech_*` takes at 10 dB SNR)
Mean noise cut in the pauses (dB, the noise as mixed against the output a latency later):

| Noise | RNNoise | UL-UNAS (high band: model) |
|---|---|---|
| DEMAND cafe | 19.9 | 22.7 |
| Freesound keyboard (448209) | 18.4 | 29.9 |
| Freesound fan (242042) | 26.1 | 29.2 |

(The second run, at depth 2 with bin 256 mapped real; the first, at depth 1, gave the same but the fan's 29.3.)
- Per take, UL-UNAS cut more on 27 of 30 (RNNoise more on p225 and p237 in the cafe, and p237 with the fan).
- Above 8 kHz UL-UNAS's cut is often under its cut below (cafe fb07: 12.3 against 20.7 dB), and under RNNoise's on 5 of
  30 takes: one gain from 5-7.5 kHz is a blunt tool for the high band (TODO 14's A/B).
- Speech kept (the median per-20 ms least-squares gain on the clean take): -0.1 to -1.2 dB for UL-UNAS and -0.1 to -1.3 dB
  for RNNoise on every take.
- GCC-PHAT on every take: the chain's latency (480.00, 2400.00) within 0.01 sample at 41-53 dB.
- These are level measures, not listening: the WAVs go to `NF_STAGES_WAV_DIR` for the owner's ears.

## Open
- Listening (the owner), and TODO 14's A/B: the high-band policy (`model`, `zero`, `pass`), and the top-bin fold
  (`Spectral::set_fold(8)`, off by default; `doc/todo/3-model-spec.md`, P3).
- The patent question (the owner, TODO 5's entry).
- ONNX Runtime's per-run heap traffic (about 3,710 allocations a run: TODO 6) argues for the hand port (zero allocations,
  1440 samples on the audio thread) if UL-UNAS stays.
- In onnx builds ONNX Runtime's C++ static initialisers run before `main`, ahead of the CPU gate. `check_cpu_gate_asm.py`
  passes the onnx build's Rust asm but can't see into the prebuilt library, and the library needs AVX2.
- A live run with misses can't be rerun exactly: the chain recorder doesn't log the substitute's blocks yet.
- The depth on real hardware: a soak's "wakes" line (`run --seconds`) says what depths 1-3 would miss there.

## Left for later (the design step's list)
- Shipping: the patent answer, the NOTICE (TODO 18), bundled packs or a download; the GUI's model selector (TODO 15).
- Large-page arenas for ONNX Runtime: an env `OrtAllocator` over TLSF in the nf-mem pool, `with_external_initializer` for the
  weights, an `operator new` shim (TODO 6).
- Keeping the session across restarts (each restart rebuilds it: about 120 ms with the warm-up on the dev VM).
- A hand port with zero allocations, which can run on the audio thread (L = 1440).
- TODO 7's 10 ms-hop retrain.
- A resampler path for waveform packs: a 3:1 polyphase linear-phase Kaiser FIR of odd length, about 145 taps (1.5 ms,
  about 50 dB) to 241 taps (2.5 ms, about 80 dB), so the delay is a whole sample at both rates.
- A VAD from the mask; the chain recorder logging misses, so offline reruns stay exact; pack zips; caching conformance
  results; freezing the spec; the `.clap` export.

## Live A/B on the cables (2026-11-02)
`quality --test noise --seconds 30 --model rnnoise|ulunas` on the dev VM's cables, the defaults (exclusive mic and output),
the onnx release build (88d1caa). WAVs in the VM's `Downloads\nf-quality\ab-*` for listening.

| Scene | RNNoise: noise in pauses (200 ms clear) | UL-UNAS | Speech kept |
|---|---|---|---|
| Synthetic voice, pink noise 10 dB under | -26.5 dB (-27.3) | -34.5 dB (-46.1) | -0.3 dB both |
| VCTK p226, keyboard (Freesound 448209) 10 dB under | -24.5 dB (-28.5) | -32.9 dB (-34.6) | -0.4 dB both |

- **Latency:** the app path (readout + chain) 34.4 ms with RNNoise, 74.6 ms with UL-UNAS at depth 2 (the chain 50 ms), 64.5
  ms at depth 1 (the chain 40 ms). Depth 1 ran clean here (no dropouts, the same noise cut) because the exclusive mic
  delivers a block at a time; with the shared mic's bursts it would miss (the fix pass's table). Proposal: pick the depth
  from the mic's delivery (1 when steady), not a fixed 2.
- So UL-UNAS cuts 8-19 dB more noise in pauses for the same speech level, at 30-40 ms more latency. Latency is the owner's
  first criterion: RNNoise stays the default; UL-UNAS (if the patent question allows) as a stronger mode.
