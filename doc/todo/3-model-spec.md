# 3. Model interface and the published model-pack spec

Status: design, fact-checked 2026-10-26 (plugin formats, ONNX metadata conventions, 16 vs 48 kHz).
Publishing the spec is the owner's tentative wish (#1). Built 2026-11-02: the noise stage's internal model
trait, its re-blocker, and spec v0 as a draft, `doc/spec/model-pack-v0.md` (see "Built" below).

## Why our own spec for models
- No standard describes streaming speech-enhancement models.
- Plugin formats (CLAP, VST3) can declare latency and take a sidechain, but they don't describe STFT
  settings, cache pairing, tensor layout, or a gain the host can floor. Voice apps (Discord, Teams)
  don't host plugins at all; DAWs, and OBS's pending VST3 host (sidechain from an audio source), can
  route a reference to a mic plugin only if the user sets it up.
- The closest convention is sherpa-onnx's `metadata_props` keys (GTCRN: `sample_rate`, `n_fft`,
  `hop_length`, `window_length`, `window_type`, `*_cache_shape`; DPDFNet adds `normalized`,
  `center`, `pad_mode` and the state sizes).
- FastEnhancer's export writes no metadata at all. UL-UNAS ships no metadata either.
- Post-processing effects (a voice changer) are CLAP stages (#17), not model packs.

## Two layers
Effects are CLAP plugins (owner, 2026-10-26), and the AEC and the noise filter are CLAP-shaped stages too
(owner, 2026-11-02, #17). This file covers the models *inside* the noise stage.
1. **Built-in models (Rust):** models that need custom DSP — RNNoise/`nnnoiseless`, DeepFilterNet3
   (three graphs plus ERB features and deep filtering in host code) — and later the hand-written
   ports (#6).
   - One internal model interface: sample rate, hop, `latency_samples`, `reset`, `process` (no
     allocation or locks in `process`).
   - Not in the public spec at first.
2. **Model packs (public, declarative, no code):** a folder or zip holding a manifest, the ONNX
   file(s), licence texts and checksums. The noise stage loads them.
   - Two kinds: `spectral` (the host does the STFT: UL-UNAS, GTCRN, DPDFNet, FastEnhancer `_spec`)
     and `waveform` (STFT inside the graph: FastEnhancer wav2wav).
   - They run through ort on a worker behind an adapter of the same interface. They allocate per
     call until #6's ORT strategy is in place.

## Manifest fields (draft v0)
- **Identity:** `spec` (format version), `id`, `version`, `name`, `authors`, `homepage`.
- **Provenance:**
  - separate code and weights licences;
  - a training-data licence list;
  - required attributions;
  - patent notes (e.g. China-only patents on the GTCRN family, #7).

  Retrained weights may need CC BY-SA if ShareAlike data was used, so this can't be one licence
  field.
- **Files:** `files[]` with sha256.
- **Role:** `kind` (`denoise`, `dereverb` for the noise stage; `aec` packs, with `inputs` `["mic", "ref"]`,
  are loaded by the AEC stage once it takes neural models, e.g. LocalVQE); `inputs` (`["mic"]` by default).
- **Signal:**
  - `sample_rate`, `channels`, `input_scale` (nnnoiseless wants ±32768, the ONNX models ±1), `band`.
  - For spectral packs: `n_fft`, `hop`, `win_length`, `window` (hann, sqrt_hann, vorbis),
    `periodic`, `center`, and the inverse-STFT normalisation. Plain Hann at 50% overlap needs
    division by the summed squared window.
  - For waveform packs: `in_samples`, `out_samples`.
- **Tensors:**
  - `io.input` / `io.output`: name, layout (`B,F,T,RI` for GTCRN/UL-UNAS vs `B,T,F,RI` for
    DPDFNet), and output kind (`spectrum`, `real_mask`, `complex_mask`, `waveform`).
  - `state[]`: in name, out name, shape, dtype and init (zeros or given values; DPDFNet's state
    starts non-zero). Pairs are listed explicitly, because naming varies (`x`→`x_out`,
    `cache_in_i`→`cache_out_i`).
- **Latency:** `lookahead_frames`.
  - `latency_samples` = algorithmic latency = window + lookahead × hop.
  - The stream offset `latency_samples − hop` is what #2 measures offline.
- **Controls (optional):** map a user setting to an input tensor (e.g. an attenuation limit).
- **Runtime:** minimum opset, and whether the graph uses FFT ops (ONNX has STFT since opset 17 but
  no ISTFT; tract registers DFT/STFT).
- **Conformance:** a golden input/output pair with tolerances.

Rules:
- Keep sherpa-onnx's key names where they exist.
- Read embedded `metadata_props` as a fallback (ort `Session::metadata()`; tract
  `onnx.metadata_props.<key>`), but the manifest wins. Upstream ONNX files then ship unmodified,
  with their checksums intact.
- **Security:** a "no-code" pack must reject custom-op domains or libraries, and external-data paths
  outside the pack. Check the ONNX external-data spec and ORT custom-op docs before freezing this.

## Host STFT backend
- v0 has no host STFT: RNNoise and AEC3 bring their own FFTs (nnnoiseless uses easyfft/rustfft at
  960 points; sonora has its own 128-point FFT). The only FFT in our code is the latency test's
  offline analysis (realfft), and realfft and rustfft are already linked through nnnoiseless, so it
  adds nothing.
- When the first spectral model pack lands, put the STFT behind a small nf-core trait (built 2026-11-02 without the trait:
  "Built" below), and benchmark
  realfft against PhastFT 0.4.1 (MIT/Apache, pure safe Rust, runtime SIMD via fearless_simd, has
  r2c, split re/im layout) at that model's sizes.
  - PhastFT is power-of-two only (non-power-of-two is listed as planned), so it can't do 960
    (RNNoise, DFN3) or 1536 (16 kHz models on the 48 kHz bin-mapped STFT).
  - Its README says RustFFT wins at small sizes and PhastFT at large, multithreaded ones, so expect
    realfft to win at 512-1024. Measure before switching.
  - Check whether PhastFT's planner-based calls allocate per call, which matters for the audio thread.
- **Measured 2026-10-26** (forward real FFT, best of 15, one thread; dev box under build load):
  - realfft: 512 349 ns, 960 858 ns, 1024 747 ns, 1536 1220 ns, 2048 1673 ns; primes 1009 / 1031:
    4048 / 6768 ns (the real non-power-of-two penalty is for large prime factors, not for 2^a 3^b 5^c);
  - PhastFT: 512 466 ns, 1024 955 ns, 2048 2010 ns;
  - a whole RNNoise frame: 51 us, of which its FFTs are about 2.6 us.
- **Decision (owner, 2026-10-26): keep realfft/rustfft.** It's faster at the small single-threaded sizes
  audio needs. Don't rebase nnnoiseless or sonora onto another FFT: the FFT is a few percent of a frame.
  Don't change a model's FFT size (960 → 1024): the network is trained on that bin grid, and 960/480
  is what lines up with 10 ms WASAPI periods and AEC3 frames.

## Rate handling by the host
- **Process at 48 kHz and always output full band.** An 8 kHz ceiling is audible: one MUSHRA test
  scored wideband 16.5-19.4 points below full band. Discord sends 48 kHz Opus, and libopus codes
  full band at voice bitrates. Opus also measures its input's bandwidth, so our high band changes
  what the far end receives.
- **sonora:** set `MaxProcessingRate::Rate48kHz`. The default (32 kHz) caps the whole chain at
  16 kHz whatever model runs. Rate48kHz brings in WebRTC's three-band filter bank (not
  perfect-reconstruction, ~9.5 dB round-trip SNR), so A/B it (#10).
- **44.1 kHz devices:** convert to 48 kHz at the edge (WASAPI `AUTOCONVERTPCM` + `SRC_DEFAULT_QUALITY`,
  or our resampler).
- **16 kHz spectral models** (UL-UNAS, FastEnhancer-16k):
  - Run a 48 kHz STFT with a 3× window (1536/768). Its bins 0-256 match the model's 512/256 bins.
  - Scale magnitudes by 1/3. This matters for UL-UNAS, whose features are unnormalised log-magnitude
    ERB.
  - Apply the model's output to bins 0-256. Fill bins 257-768 through a pluggable high-band policy:
    first a WebRTC-style gain from the model's 4-8 kHz gains, then per-ERB-band gains with a
    noise-floor tracker for fan hiss.
  - No resampler and no aliasing. Nobody has published or shipped exactly this, so A/B it in #14
    against RNNoise and a native 48 kHz model on keyboard-and-fan recordings.
  - Open: whether bins near 8 kHz need tapering, since the training data was anti-alias filtered
    there.
- **16 kHz time-domain models** use the three-band split, or a resampler. If resampling: once in
  each direction, a short (~1 ms) filter, no block-based FFT resamplers. Fold the drift ratio into
  that one resampler (#9).
- **Band-limited inputs** (16 kHz USB headsets, Bluetooth HFP): detect the effective input bandwidth
  and skip the high-band policy. Bandwidth extension is only an option for those.
- **Tiers:** native 48 kHz models (RNNoise now; FastEnhancer-48k or DFN3 if the weights clear, #7)
  for quality, 16 kHz plus the high-band policy otherwise. CPU doesn't separate the tiers (both cost
  a few % of a core); latency and licences do.

## Built (2026-11-02)
These are our decisions, within the owner's #17 decision (the floor and gate inside the noise stage).
- **The noise stage** (`nf_core::noise::NoiseStage`, the CLAP-shaped `noise-filter.denoise`) runs any model
  through the internal trait `NoiseModel`:
  - `info() -> &ModelInfo { id, name, hop, latency_samples, has_vad }`, at 48 kHz; `latency_samples` is
    algorithmic (window + lookahead × hop), so the stream offset is `latency_samples - hop`. `id` and `name` are
    `Cow<'static, str>`, so a pack's manifest can fill them at run time;
  - `prepare()`: on the processing thread, at activate; it may allocate;
  - `reset()` and `process_hop(input, output) -> VAD or NaN`: real-time. The input is finite and within ±1e4
    (`nf_core::dsp::INPUT_LIMIT`, +80 dBFS): it can exceed ±1 (a hot mic), so a model must stay finite up to that
    bound. `reset` doesn't allocate but in one known case: a model whose fresh state allocates keeps spares built off
    the audio thread (RNNoise: two, replaced by its spare maker within 100 ms, 2026-11-02; TODO 4), and rebuilds in place
    only when a reset finds them all spent (a third real reset within 100 ms of two others).
- **What the stage does around the model:**
  - it sanitises the input: NaN and ±inf to 0, the rest clamped to ±1e4 (a finite sample of about 1e17 overflowed
    RNNoise's ×32768 to inf and aborted inside easyfft, a hazard as old as v0);
  - it zeros non-finite output and clamps to ±1;
  - it runs the floor mix and the gate (`NoisePost`, its dry path delayed by the stage's whole latency);
  - it owns the params and the extensions;
  - it resets the model only when something was processed since the last reset.
- **RNNoise** is `nf_stages::RnnoiseModel`: hop 480, `latency_samples` 960, a VAD; ×32768 in, ÷32768 out; its spare
  state and easyfft warm-up are in `prepare`. `nf_stages::rnnoise_stage` is the v0 noise stage. Its output is bit
  for bit what the old `RnnoiseStage` gave (the refactor guard, #2).
- **The re-blocker** (`nf_core::dsp::reblock`) lets a model keep its own hop:
  - the output is primed with `hop - gcd(hop, 480)` zeros, the least that never runs dry, so that is the added
    delay; nothing for hops that divide 480 or are multiples of it;
  - fixed buffers, at most ⌈480/hop⌉ hops a block, no allocation;
  - so `L = (latency_samples - hop) + (hop - gcd(hop, 480))`: RNNoise 480; FastEnhancer-48k (hop 512) 992;
    UL-UNAS bin-mapped (hop 768, latency 1536) 1440. That is 10 ms extra at hop 512 and 14 ms at 768: input
    for #5 and #7's 10 ms-hop retrain;
  - the VAD is the last hop's, carried over a block with no hop. Its alignment for a hop other than 480 is
    approximate; no real model exercises it until #5;
  - a model needing more than `MAX_STAGE_LATENCY` (4096) is refused.
- **Checked** (#2): the latency formula at hops 160 to 1024 with and without lookahead, to the sample, through
  the chain; no allocation at any hop; the RNNoise model's offset hop by hop is 480 (`harness::check_model`, which
  TODO 5's loader can run on a pack).
- **Spec v0** is `doc/spec/model-pack-v0.md`, a draft: the manifest fields above, made concrete; the derived 48 kHz
  values (`hop`, `latency_samples`, `has_vad`) that fill `ModelInfo`, for spectral and waveform packs (a waveform
  pack's `latency.samples` is algorithmic, its hop is `out_samples`, an adapter's resampler adds its delay, and the
  rate must divide 48 kHz); a `"ref"` input for `aec` packs only (the noise stage's models see only the mic); the
  STFT's `normalized`, `normalization` (`wola` | `ola`), `center` and `pad_mode`; `band` as the trained band's upper
  edge; and the rules (the manifest wins, `metadata_props` as a fallback for keys it leaves out, checksums first, no
  custom ops or outside paths, an unknown major version refused, the pack through #2's checks). No parser yet.

## Built (2026-11-02, with TODO 5's first pack)
Our decisions, from the design step for TODO 5, 3 and 6 (`doc/todo/5-candidate-models.md`).
- **The host STFT** is `nf_core::dsp::stft`: concrete `Stft` and `Istft` types over realfft (no trait: one FFT library),
  periodic Hann, causal framing (zeros before the first sample), an analysis ring of N, a frame every H, the inverse ×1/N
  with bins 0 and N/2 made real, WOLA per sample (`out[n] = acc[n] / Σ w²[n + jH]`), stream offset N - H. Tested: identity
  reconstruction at 512/256, 1536/768, 960/480 and 1024/256; the envelope within [0.5, 1]; no allocation.
- **The high-band policy** is `nf_core::dsp::high_band`, live from `ChainControls.noise.high_band` (`ProcessingConfig.high_band`,
  `--high-band model|zero|pass`): `model` carries the model's own gain over 62.5-93.75% of its band (UL-UNAS: bins 160-240,
  5-7.5 kHz) to everything above, clamped to [0, 1], the last frame's when that band holds nothing; `zero` band-limits;
  `pass` passes the input.
- **The bin-mapped framing** is `nf_ort::OrtSpectralModel` (hop 768, `latency_samples` 1536, offset 768); the native one
  `nf_ort::NativeSpectral` (offline). The spec's amendments are in `doc/spec/model-pack-v0.md` (still a draft).
- **The manifest parser** is `nf_ort::pack`, with the conformance runner in `nf_ort::conformance` (torch's framing: center,
  reflect, the full envelope, trimmed).

## P3: bin-mapped against native (measured 2026-11-02, `nf-ort/tests/ort/parity.rs`)
`noisy_0174` upsampled ×3 by FFT zero-padding, with 2048 samples of -100 dBFS noise on each side (at 16 kHz, so
band-limited). Two lessons from the first runs:
- **The first causal frame isn't band-limited:** half zeros and half signal is a hard edge mid-window, whose energy above
  8 kHz aliases at 16 kHz. With the signal starting at sample 0, the mapping's error over the file was -83 dB below 4 kHz;
  padded, it is -138 dB. Live, that frame is the start-up region either way.
- **Digital silence sets the model apart:** UL-UNAS's features are `log10(max(|X|, 1e-12))`, so exact zeros at 16 kHz against
  the 48 kHz interpolation's ringing (around 1e-9) differ by decades, and the recurrent state diverged (-25 dB at the output,
  with the input matching). Padding with -100 dBFS noise removed it. Live input is dithered (±1e-9), so this is a test effect.

The model's input and output frames, error energy over native's per band:

| Band (model bins) | input, as designed | output, as designed | input, bin 256 real (default) | output, bin 256 real (default) | input, folded | output, folded |
|---|---|---|---|---|---|---|
| 0-4 kHz (0-127) | -137.8 dB | -38.9 dB | -137.8 | -46.4 | -137.8 | -57.5 |
| 4-7 kHz (128-223) | -109.4 | -52.2 | -109.4 | -58.4 | -109.4 | -76.6 |
| 7-7.75 kHz (224-247) | -89.8 | -45.8 | -89.8 | -52.5 | -89.8 | -74.8 |
| 7.75-8 kHz (248-255) | -27.4 | -34.9 | -27.4 | -34.8 | -89.9 | -78.1 |
| 8 kHz (256) | -4.4 | +1.3 | -81.0 | -25.1 | -81.0 | -63.1 |

- **As designed** (bins 0-256 ÷ 3) the input matches the design step's figures (-153/-113/-91 dB there, on its own framing;
  top 8 bins -17.5 dB, bin 256 -4 dB). But the top bins' mismatch reaches the whole output through the model's
  cross-frequency layers: -39 dB below 4 kHz, and bin 256's output is unrelated.
- **Why the top bins differ:** decimation's identity gives, for each frame, `X16[k] = (X48[k] + X48[k + 512] + X48[k + 1024]) /
  3`. For a band-limited input the middle term is near 24 kHz (nothing) and the last is `conj X48[512 - k]`: the window's spread
  just above 8 kHz, folded back. So at bin 256, `X16[256] = 2·Re(X48[256]) / 3`, not `X48[256] / 3`.
- **Bin 256 real, the default since the review** (ours): the model only ever saw a real Nyquist bin (the native STFT's, for
  an even `n_fft`), but the design fed it a complex host bin 256 (`mix[256].im` never 0). It now gets the fold's own term,
  `2·Re(X48[256]) / 3`, the decimated DFT's bin, and nothing else changes: the input matches there (-81.0 dB) and the
  model's output follows, -46.4 dB below 4 kHz instead of -38.9. The host's bin 256 out comes from the high-band policy
  with the bins above it (the model's real Nyquist output can't give the host bin's phase; `Zero` so leaves the output
  under 8 kHz).
- **The fold** (`Spectral::set_fold(8)`: bins 248-255 take `(X[k] + conj X[512 - k]) / 3` too) makes the input match at every
  bin and the output to -57 dB or better. For full-band input it folds the host's 8-8.25 kHz into 7.75-8 kHz, as a 16 kHz
  signal's own aliasing would. **Off by default** (ours): whether it sounds better is TODO 14's A/B, beside the high-band
  policy.
- Pinned (about 4 dB inside the figures): with the default mapping, input bands 0-2 under -130, -105 and -85 dB and bin 256
  under -75; output bands 0-4 under -42, -54, -48, -30 and -20; folded, input under -75 everywhere and output under -53
  (0-4 kHz), -70 (4-8 kHz) and -58 (bin 256).

## Next
1. TODO 14's A/B: the high-band policy and the fold, by ear and the Lab's scores, on the owner's mic.
2. Before freezing the spec: ORT's custom-op documentation (v0 registers no op library, so a custom domain fails the
   session). External data is refused by the host's own walk of the model (`nf_ort::onnx_check`, every place onnx.proto
   holds a tensor), not by construction: ONNX Runtime 1.31 resolves a model-from-memory's relative external-data path
   against the current folder (the verifier's check, 2026-11-02).
3. Waveform packs (FastEnhancer wav2wav): the resampler path is `doc/todo/5-candidate-models.md`'s "left for later".
