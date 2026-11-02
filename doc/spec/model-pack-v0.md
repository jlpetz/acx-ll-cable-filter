# noise-filter model pack v0 (draft, unpublished)

`"spec": "noise-filter.model-pack/0"`

A model pack is data, not code: an ONNX noise model plus what the host needs to run it, which the noise stage
(`noise-filter.denoise`, see `host-profile-v0.md`) loads. Built-in models that need custom DSP (RNNoise, DeepFilterNet3)
are Rust inside the noise stage and are not packs. The design and its sources are in `doc/todo/3-model-spec.md`.

Status: a draft. No parser exists yet; it comes with the first pack (TODO 5). The namespace and field names may change
until it is published.

## The pack
- A folder, or a zip of one, holding `manifest.json` (JSON), the ONNX file(s), licence texts and conformance audio.
- Paths in the manifest are relative to the pack: no `..`, no absolute paths, no symlinks.
- Packs live outside this repo.

## Manifest fields
### Identity
`spec`, `id`, `version` (semver), `name`, `authors[]`, `homepage`, `description`.

### Licences
`licenses`: `code`, `weights`, `data[]`, `attribution[]`, `patents[]`. These are separate fields because retrained weights
may need CC BY-SA if ShareAlike data was used, and some model families carry patent notes.

### Files
`files[]`: `path`, `sha256`, `role` (`model` | `license` | `conformance`).

### Role
`role`:
- `kind`: `denoise`, `dereverb`, or `aec` (loaded by the echo canceller once it takes neural models);
- `inputs`: `["mic"]` by default, or `["mic", "ref"]` for an `aec` pack only. In v0 the noise stage has one input and
  its models see only the mic, so a `denoise` or `dereverb` pack that declares `"ref"` is refused. A joint echo and
  noise model (one that needs the far end) is an `aec` pack.

### Signal
`signal`:
- `sample_rate`: 48000 or an integer divisor of it (8, 12, 16, 24 kHz). Any other rate (44.1 kHz) is refused.
- `channels`: 1.
- `input_scale`: what the host multiplies its ±1 samples by (1 for ±1, 32768 for i16 units).
- `band`: the upper edge, in Hz, of the band the model was trained on (default `sample_rate / 2`; e.g. 8000 for a 16 kHz
  model). Above it the host's high-band policy fills the spectrum (`doc/todo/3-model-spec.md`, "Rate handling").

The host gives the model finite samples within ±1e4 (+80 dBFS) before `input_scale`: it zeros NaN and ±inf and clamps
the rest. Real audio rarely passes a few times full scale, but a pack must stay finite (no NaN or inf out) up to that
bound.

### Graph
`graph`: `spectral` (the host does the STFT) or `waveform` (the STFT is inside the graph).
- `stft` (spectral packs; sherpa-onnx's key names where it has them): `n_fft`, `hop_length`, `window_length`,
  `window_type` (`hann` | `sqrt_hann` | `vorbis`), `periodic`, `normalized`, `normalization`, `center`, `pad_mode`.
  - `normalized` (default false): torch.stft's flag (DPDFNet's key): the forward frames are scaled by `1 / sqrt(n_fft)`,
    and the inverse scales them back.
  - `normalization`, how the inverse overlap-adds: `wola` (divide by the summed squared window, as torch.istft does;
    what plain Hann at 50% overlap needs) or `ola` (plain overlap-add, for an analysis and synthesis pair that already
    sums to one, such as sqrt-Hann at 50%).
  - `center` (default false) and `pad_mode` (`reflect` | `constant`; DPDFNet's key): how the model's offline reference
    framed its input. A streaming host frames causally either way; `conformance.align_samples` takes up the shift that
    `center` makes (`n_fft / 2` at the pack's rate).
  - Draft: check these values against the first packs' exporters before freezing them.
- `waveform` (waveform packs): `in_samples` (what one call reads, context included), `out_samples` (what one call
  writes: the hop), and an explicit `latency.samples`.

### Tensors
- `io.input`, `io.output`: `name`, `layout` (e.g. `B,F,T,RI`), `dtype`; the output's `kind` (`spectrum` | `real_mask` |
  `complex_mask` | `waveform`).
- `io.vad` (optional): a tensor holding the model's speech probability. It feeds the noise stage's
  `noise-filter.speech-probability/0` extension.
- `state[]`: `in`, `out`, `shape`, `dtype`, `init` (`zeros`, or given values). Pairs are listed explicitly, because the
  naming varies between models.

### Latency
- `latency.lookahead_frames` (spectral): STFT frames of lookahead.
- `latency.samples` (waveform): the **algorithmic** latency at the pack's rate: the stream offset (what the conformance
  pair shows) plus one hop (`out_samples`). Give the algorithmic figure, not the offset; they differ by a hop.

### Controls (optional)
`controls[]`: `name`, `tensor`, `min`, `max`, `default`. Each maps a user setting to an input tensor, e.g. an attenuation
limit.

### Runtime
`runtime`: `min_opset`, `fft_ops`, `providers` (`["cpu"]`).

### Conformance
`conformance`: `input`, `output` (paths in the pack), `align_samples`, `max_abs`, `rms_db`.

## Derived at 48 kHz
The host runs every pack at its 48 kHz profile. With `r = 48000 / sample_rate` (an integer: see `signal.sample_rate`):
- `hop = hop_length × r` for a spectral pack, `out_samples × r` for a waveform pack;
- `latency_samples = (window_length + lookahead_frames × hop_length) × r` for a spectral pack, `latency.samples × r` for
  a waveform pack; plus the adapter's own rate-conversion delay, if it has one, in 48 kHz samples (a resampler around a
  16 kHz waveform pack; none for a spectral pack on the bin-mapped 48 kHz STFT);
- `has_vad` = whether `io.vad` is present.

These fill the noise stage's internal model description, `nf_core::noise::ModelInfo { id, name, hop, latency_samples,
has_vad }`, with the manifest's `id` and `name` (owned strings: `ModelInfo` takes run-time text). The stage then reports
its latency as the stream offset plus the re-blocking delay:

`L = (latency_samples - hop) + (hop - gcd(hop, 480))`

| Model | hop | latency_samples | L |
|---|---|---|---|
| RNNoise (built in) | 480 | 960 | 480 |
| FastEnhancer-48k | 512 | 1024 | 992 |
| UL-UNAS, bin-mapped (1536/768 STFT) | 768 | 1536 | 1440 |

A model run on a worker thread (TODO 6) will add 480 to `L`.

## Rules
- The manifest wins over the ONNX file's embedded `metadata_props`, so upstream ONNX files ship unmodified, with their
  checksums intact. A key the manifest leaves out may come from `metadata_props` under sherpa-onnx's name (`sample_rate`,
  `n_fft`, `hop_length`, `window_length`, `window_type`, and DPDFNet's `normalized`, `center`, `pad_mode`); read through
  ort's `Session::metadata()`. A key in both takes the manifest's value.
- Verify every `sha256` before loading anything.
- Reject custom-op domains and external-data paths outside the pack. Check the ONNX external-data spec and ORT's
  custom-op documentation before freezing this rule.
- Reject an unknown spec major version.
- The host reports `L` as above, and runs the pack through the harness (TODO 2's checks 1 and 2, in
  `nf_stages::harness`):
  - `check_model`: the model's offset, driven hop by hop, equals `latency_samples - hop` (GCC-PHAT on speech to a tenth
    of a sample, and an impulse to the sample where the model passes one); `check_latency`: the noise stage's equals
    `L`; `check_profile`: the stage around it fits the host profile;
  - `parity`: the output matches `conformance.output` after `align_samples`, within `max_abs` and `rms_db`.
