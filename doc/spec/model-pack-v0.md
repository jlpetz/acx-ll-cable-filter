# noise-filter model pack v0 (draft, unpublished)

`"spec": "noise-filter.model-pack/0"`

A model pack is data, not code: an ONNX noise model plus what the host needs to run it, which the noise stage
(`noise-filter.denoise`, see `host-profile-v0.md`) loads. Built-in models that need custom DSP (RNNoise, DeepFilterNet3)
are Rust inside the noise stage and are not packs. The design and its sources are in `doc/todo/3-model-spec.md`.

Status: a draft. The parser is `nf_ort::pack`, with the first pack, UL-UNAS (`packs/ulunas/manifest.json`, TODO 5;
amendments from it are marked "(amended 2026-11-02)"). The namespace and field names may change until it is published.

## The pack
- A folder, or a zip of one, holding `manifest.json` (JSON), the ONNX file(s), licence texts and conformance audio.
- Paths in the manifest are relative to the pack, `/`-separated: no `..` or `.`, no absolute paths or drives, no empty
  parts, backslashes, colons, control characters, Windows device names or trailing dots and spaces, and no symlinks. Two
  paths that differ only in case are one path. (amended 2026-11-02)
- `id` matches `[a-z0-9][a-z0-9-]{0,63}`, and the pack's folder is named by it. (amended 2026-11-02)
- Packs live outside this repo. This repo holds a pack's manifest, which is also its fetch recipe (`tools/fetch_models.py`),
  and never its weights.

## Manifest fields
### Identity
`spec`, `id`, `version` (semver), `name`, `authors[]`, `homepage`, `description`. Optional `upstream`: `{repo, commit, paper}`,
where the files were published (read by people and the fetch tool; ignored by the host). (amended 2026-11-02)

### Licences
`licenses`: `code`, `weights`, `data[]`, `attribution[]`, `patents[]`. These are separate fields because retrained weights
may need CC BY-SA if ShareAlike data was used, and some model families carry patent notes.

### Files
`files[]`: `path`, `size`, `sha256`, `role` (`model` | `license` | `conformance`), optional `source`. (amended 2026-11-02)
- `size` (bytes) is required: the host checks it before hashing, and it caps the read.
- `sha256` is required (64 lower-case hex digits).
- `source`: `{url, git_commit, git_blob}`, where the file came from: `url` https only; `git_blob` the git object id
  (`sha1("blob <size>\0" + bytes)`), checked by the fetch tool. The host ignores it.
- `source.lfs: true` (amended 2026-11-02, for SIGMOS): a file stored with Git LFS, fetched from `media.githubusercontent.com`.
  Its blob in the commit's tree is its LFS pointer, so `git_blob` is the pointer's (`version https://git-lfs.github.com/spec/v1`,
  `oid sha256:<sha256>`, `size <size>`), which the fetch tool rebuilds from the download's SHA-256 and size and checks.
- Exactly one `model` file in v0, the one `graph.model` names.

### Role
`role`:
- `kind`: `denoise`, `dereverb`, or `aec` (loaded by the echo canceller once it takes neural models); or `metric` (amended
  2026-11-02): a quality metric (DNSMOS, SIGMOS) that `noise-filter lab` runs offline on whole clips, never a stage (see
  "Metric packs");
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
`graph` is an object (amended 2026-11-02: v0's text left the nesting open): `graph.kind`, `spectral` (the host does the STFT) or
`waveform` (the STFT is inside the graph); `graph.model`, the ONNX file's path; and `graph.stft` (spectral) or
`graph.waveform` (waveform). The v0 host runs spectral packs only.
- `stft` (spectral packs; sherpa-onnx's key names where it has them): `n_fft`, `hop_length`, `window_length`,
  `window_type` (`hann` | `sqrt_hann` | `vorbis`), `periodic`, `normalized`, `normalization`, `center`, `pad_mode`.
  - `normalized` (default false): torch.stft's flag (DPDFNet's key): the forward frames are scaled by `1 / sqrt(n_fft)`,
    and the inverse scales them back.
  - `normalization`, how the inverse overlap-adds: `wola` (divide by the summed squared window, as torch.istft does;
    what plain Hann at 50% overlap needs) or `ola` (plain overlap-add, for an analysis and synthesis pair that already
    sums to one, such as sqrt-Hann at 50%).
  - `center` (default false) and `pad_mode` (`reflect` | `constant`; DPDFNet's key): how the model's offline reference
    framed its input. A streaming host frames causally either way; the conformance runner frames as the reference did, so
    no shift is left to take up (amended 2026-11-02; v0's first text had `conformance.align_samples` take up `center`'s
    `n_fft / 2`).
  - Draft: check these values against the first packs' exporters before freezing them.
- `waveform` (waveform packs): `in_samples` (what one call reads, context included), `out_samples` (what one call
  writes: the hop), and an explicit `latency.samples`.

### Tensors
- `io.input`, `io.output`: `name`, `layout` (e.g. `B,F,T,RI`), `shape`, `dtype`; the output's `kind` (`spectrum` |
  `masked_spectrum` | `real_mask` | `complex_mask` | `waveform`).
  - `shape` (amended 2026-11-02, required): the exact shape of one call; one frame per call (`T = 1`) for a spectral pack.
    The host checks every name, dtype and shape against the graph's declared inputs and outputs when it builds the session,
    and refuses a graph input or output the manifest doesn't name.
  - `masked_spectrum` (amended 2026-11-02), with `mask: real | complex`: the model has applied its mask already and outputs
    the spectrum. For a real mask the host can recover it as `m = Re(E·conj(X)) / |X|²`; the high-band policy uses the
    output's energy against the input's over the model's top band. The v0 host takes `spectrum` and `masked_spectrum`.
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
`runtime`: `min_opset`, `fft_ops`, `providers` (`["cpu"]`); `engines` (e.g. `["onnxruntime>=1.17"]`) and `known_bad` (e.g.
`["tract: mis-carries the GRU state (tract #2629)"]`), advisory only (amended 2026-11-02).

### Conformance
`conformance` is a list (amended 2026-11-02) of `{input, output, reference, pcm, align_samples, skip_samples, max_abs,
rms_db}`:
- the pair runs at `signal.sample_rate` through the pack's own STFT, framed as the reference was (`graph.stft.center` and
  `pad_mode`), so no 48 kHz reference is needed and `align_samples` is 0 for a reference made that way;
- `reference`: `offline` (the non-streaming model made it) or `streaming`;
- `pcm`: `pcm16` when the reference is 16-bit (the comparison is then in its integers, as `soundfile.write` wrote them,
  ×32767 and rounded: about one least-significant bit is the floor of any tolerance), else `float`;
- `skip_samples`: samples left out at the start;
- `max_abs` and `rms_db`: the tolerances (`null`: report only, until the first run pins them);
- it runs at install or test time and may be cached per set of hashes. At load the host checks only the hashes and the
  signature: the pair takes about 0.7 s.

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

A model run on a worker thread (TODO 6) adds `depth` × 480 to `L`, its result used `depth` blocks later (amended
2026-11-02: the v0 host runs every pack on one, `nf_core::noise::Pipelined`, at the host's depth, not a pack field: automatic,
1 when the mic delivers a block a wake and 2 otherwise, or as set): UL-UNAS bin-mapped is 2400 (1920 at depth 1).

### Spectral packs at 48 kHz, made concrete (amended 2026-11-02)
With `r = 48000 / sample_rate`:
- the host STFT is `n_fft·r` / `hop_length·r` / `window_length·r`, the same window shape and `periodic` flag (so the 48 kHz
  window samples the pack's window's curve: `w48[r·m] = w[m]`);
- the model's bins `0..n_fft/2` are the host's bins `0..n_fft/2` divided by `r` (the unnormalised DFT sums `r` times as many
  samples), and scaled by `input_scale`; its output is multiplied by `r` (and divided by `input_scale`);
- the model's Nyquist bin `K = n_fft/2`, real in its own STFT (an even `n_fft`), is `2·Re(X[K]) / r` (the decimated DFT's
  term, `X[K] + conj X[2K - K]`), real as in training; the host's bin `K`, an ordinary complex bin, comes back from the
  high-band policy with the bins above it (amended after the review: the first draft passed bin `K` through complex);
- host bins above the model's band come from the host's high-band policy (not a pack field);
- the model's top bins below `K` differ from its own STFT's by the window's spread across its Nyquist
  (`doc/todo/3-model-spec.md`, P3); folding them too is an A/B option, off in v0.

## Metric packs (amended 2026-11-02)
A pack of `role.kind` `metric` holds a no-reference quality metric's ONNX model (`packs/dnsmos`, `packs/sigmos`; TODO 14). It
shares "The pack", "Files" and "Security" with the noise packs (the same checks, the same fetch tool, the same refusal of
external data), and nothing else: `nf_ort::metric::check_metric` takes `graph.kind` `metric`, `signal.sample_rate` 16000 or
48000 with one channel, `io.input` and `io.output` as named `float32` tensors whose dimensions are positive or `-1` (dynamic:
a batch, or a clip's frame count), and a `metric` section, `{"kind": "dnsmos-p835" | "sigmos-v1"}`, naming the runner, which
checks it. The framing, windowing and calibration are the runner's (each metric's own reference script, ported), not the
manifest's. The noise stage refuses a metric pack (its role), and the metric runners a noise pack.

## Security (amended 2026-11-02)
- The host hashes the very bytes it hands to ONNX Runtime (`commit_from_memory`), so nothing can be swapped between the check
  and the load. At run time it reads and hashes the model and the licence only; the conformance files are checked at fetch
  and test time.
- **External data is refused by the host, before ONNX Runtime sees the model** (corrected after the review). The first
  draft said an in-memory model has no base path, so an external-data reference can't resolve. False: ONNX Runtime 1.31
  resolves a relative external-data location of a model loaded from bytes against the process's current folder, refusing
  only `..` and absolute paths (the verifier read a file from the current folder into an initializer that way). So the host
  walks the model's protobuf (`nf_ort::onnx_check`) and refuses any `TensorProto` with `external_data` entries or a
  `data_location` other than `DEFAULT`, wherever onnx.proto puts one: graph initializers and sparse initializers, node
  attributes (`t`, `tensors`, `sparse_tensor`, `sparse_tensors`) and their subgraphs to any depth, training graphs and
  functions; a malformed message, a group and nesting past 100 are refused too. It runs at load and again just before the
  session is built.
- The hashes come from the cached manifest, which arrives with the model. Pinning the repo manifest's model hash in the
  host would be a second anchor; not in v0, where packs are evaluation only, fetched by a developer.
- No op library is registered, so a custom-op domain fails session creation.
- The fetch tool never executes, imports or parses a download: it checks sizes, SHA-256s and git blob ids only, and refuses
  a file that isn't fully pinned before downloading it (unless `--pin`).

## Rules
- The manifest wins over the ONNX file's embedded `metadata_props`, so upstream ONNX files ship unmodified, with their
  checksums intact. A key the manifest leaves out may come from `metadata_props` under sherpa-onnx's name (`sample_rate`,
  `n_fft`, `hop_length`, `window_length`, `window_type`, and DPDFNet's `normalized`, `center`, `pad_mode`); read through
  ort's `Session::metadata()`. A key in both takes the manifest's value.
- Verify every `sha256` before loading anything.
- Reject custom-op domains (by construction: no op library) and every external-data reference (by the host's walk of the
  model: "Security"). Check ORT's custom-op documentation before freezing this rule.
- Reject an unknown spec major version.
- The host reports `L` as above, and runs the pack through the harness (TODO 2's checks 1 and 2, in
  `nf_stages::harness`; `check_stage` makes them one call, amended 2026-11-02: the profile, the offset at four block
  phases, finite output through hostile input, and a parity hook):
  - `check_model`: the model's offset, driven hop by hop, equals `latency_samples - hop` (GCC-PHAT on speech to a tenth
    of a sample, and an impulse to the sample where the model passes one); `check_latency`: the noise stage's equals
    `L`; `check_profile`: the stage around it fits the host profile;
  - `parity`: the output matches `conformance.output` after `align_samples`, within `max_abs` and `rms_db`.
