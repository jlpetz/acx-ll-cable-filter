# 3. Model interface and the published model-pack spec

Status: design, fact-checked 2026-10-26 (plugin formats, ONNX metadata conventions, 16 vs 48 kHz).
Publishing the spec is the owner's tentative wish (#1).

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
Effects are CLAP plugins (owner, 2026-10-26). Making the AEC and noise filter CLAP-shaped stages too
is our proposal, awaiting the owner (#17). This file covers the models *inside* the noise stage.
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
- When the first spectral model pack lands, put the STFT behind a small nf-core trait, and benchmark
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
