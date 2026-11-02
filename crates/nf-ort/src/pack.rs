//! Model packs (`doc/spec/model-pack-v0.md`, draft): the manifest, its checks, and the verified bytes of the model.
//!
//! [`Pack::load`] reads `<dir>/manifest.json` (at most [`MAX_MANIFEST`]) and refuses, before reading any other file:
//! - an unknown spec major (`noise-filter.model-pack/0` only), an id off the rule (`[a-z0-9][a-z0-9-]{0,63}`), or a folder not
//!   named by the id;
//! - a path that could leave the pack (absolute, a drive, `..`, `.`, empty parts, backslashes, colons, control characters,
//!   device names, trailing dots or spaces), a path listed twice (case folded), or a symbolic link;
//! - a file without its size and SHA-256, a role other than `model`, `license` or `conformance`, and anything but exactly
//!   one `model` file, the one `graph.model` names;
//! - what the v0 host can't run: a `waveform` graph, a rate that doesn't divide 48 kHz, more than one channel, an STFT that
//!   isn't Hann with `n_fft == window_length`, `normalized`, an inverse other than `wola`, an input or output that isn't
//!   one `B,F,T,RI` frame of `float32`, a `ref` input, a state that doesn't start at zeros.
//!
//! Then each file's size is checked (which also caps the read) and its SHA-256 computed over the bytes read: at run time
//! ([`Verify::Runtime`], every engine start, on the inference worker) the model and the licence only; the conformance
//! files are for the fetch tool and the tests ([`Verify::All`]), as the spec's amendment 7 has it (at load the host checks
//! the hashes and the signature). The model's bytes are kept: the session is built from exactly those
//! (`commit_from_memory`), so nothing can be swapped between the check and the load.
//!
//! **External data.** ONNX Runtime resolves a relative external-data location of a model loaded from bytes against the
//! process's current folder (only `..` and absolute paths are refused), so "no base path" is no protection: the model's
//! protobuf is walked and a tensor whose data lives in another file is refused, at load and again just before the session
//! is built (`onnx_check.rs`). The session checks the graph's own inputs and outputs against the manifest (`session.rs`).
//! The hashes come from the cached manifest, which arrives with the model; pinning the repo manifest's model hash in the
//! host would make it a second anchor (not done in v0: packs are evaluation only, fetched by a developer).
//!
//! **Two kinds.** The checks every pack takes (`check_files`: the spec, the id, the paths, the files' sizes and hashes, one
//! model) are shared; then a noise model's (`check_noise`, through [`Pack::load`]) or a quality metric's
//! (`crate::metric::check_metric`, role `metric`), so neither kind loads as the other.
//!
//! SHA-256 is Windows CNG's (`BCryptHash` with the SHA-256 pseudo-handle): no hashing crate.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};

/// The spec this host reads.
pub const SPEC: &str = "noise-filter.model-pack/0";
const SPEC_PREFIX: &str = "noise-filter.model-pack/";
/// The manifest's size cap.
pub const MAX_MANIFEST: u64 = 1 << 20;
/// Any one file's (DeepFilterNet3_ll is about 39 MB).
pub const MAX_FILE: u64 = 256 << 20;

/// The models' cache: `NF_MODELS`, else `%LOCALAPPDATA%\noise-filter\models` (where `tools/fetch_models.py` puts them).
pub fn models_dir() -> Option<PathBuf> {
    std::env::var_os("NF_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join("noise-filter").join("models")))
}

/// The SHA-256 of `data`, lower-case hex.
pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut out = [0u8; 32];
    // SAFETY: the pseudo-handle needs no open or close; both slices are valid for the call.
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    if status.is_err() {
        return Err(format!("BCryptHash failed ({:#x})", status.0));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// The manifest (the fields the host reads; others, such as `source` and `upstream`, are the fetch tool's and ignored).
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub spec: String,
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub licenses: Licenses,
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub role: Role,
    pub signal: Signal,
    pub graph: Graph,
    pub io: Io,
    #[serde(default)]
    pub state: Vec<StatePair>,
    #[serde(default)]
    pub latency: Latency,
    #[serde(default)]
    pub conformance: Vec<Conformance>,
    /// A quality metric's pack (role `metric`): which metric it is, for its runner to check (`crate::metric`).
    #[serde(default)]
    pub metric: Option<MetricSpec>,
}

/// A metric pack's `metric` section.
#[derive(Clone, Debug, Deserialize)]
pub struct MetricSpec {
    /// The metric its runner computes: `dnsmos-p835` or `sigmos-v1`.
    pub kind: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Licenses {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub weights: String,
    #[serde(default)]
    pub patents: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub role: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Role {
    pub kind: String,
    #[serde(default = "mic_only")]
    pub inputs: Vec<String>,
}

fn mic_only() -> Vec<String> {
    vec!["mic".into()]
}

impl Default for Role {
    fn default() -> Self {
        Self { kind: "denoise".into(), inputs: mic_only() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Signal {
    pub sample_rate: u32,
    #[serde(default = "one")]
    pub channels: u32,
    #[serde(default = "unit")]
    pub input_scale: f32,
    /// The trained band's upper edge in Hz (default `sample_rate / 2`).
    pub band: Option<f32>,
}

fn one() -> u32 {
    1
}

fn unit() -> f32 {
    1.0
}

#[derive(Clone, Debug, Deserialize)]
pub struct Graph {
    pub kind: String,
    pub model: String,
    pub stft: Option<StftSpec>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StftSpec {
    pub n_fft: usize,
    pub hop_length: usize,
    pub window_length: usize,
    pub window_type: String,
    #[serde(default = "yes")]
    pub periodic: bool,
    #[serde(default)]
    pub normalized: bool,
    #[serde(default = "wola")]
    pub normalization: String,
    #[serde(default)]
    pub center: bool,
    #[serde(default)]
    pub pad_mode: Option<String>,
}

fn yes() -> bool {
    true
}

fn wola() -> String {
    "wola".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct Io {
    pub input: Tensor,
    pub output: Tensor,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tensor {
    pub name: String,
    pub layout: String,
    pub shape: Vec<i64>,
    pub dtype: String,
    pub kind: Option<String>,
    pub mask: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StatePair {
    #[serde(rename = "in")]
    pub input: String,
    #[serde(rename = "out")]
    pub output: String,
    pub shape: Vec<i64>,
    pub dtype: String,
    #[serde(default = "zeros")]
    pub init: String,
}

fn zeros() -> String {
    "zeros".into()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Latency {
    #[serde(default)]
    pub lookahead_frames: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Conformance {
    pub input: String,
    pub output: String,
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub pcm: String,
    #[serde(default)]
    pub align_samples: usize,
    #[serde(default)]
    pub skip_samples: usize,
    pub max_abs: Option<f32>,
    pub rms_db: Option<f32>,
}

/// Which of a pack's files a load reads and hashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    /// The model and the licence: what the runtime uses (every engine start).
    Runtime,
    /// Every file, the conformance pairs too (the tests, the conformance runner).
    All,
}

/// A checked pack: its folder, its manifest, and its model's verified bytes.
#[derive(Clone, Debug)]
pub struct Pack {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// The model file's bytes, whose size and SHA-256 matched the manifest.
    pub model_bytes: Vec<u8>,
}

/// Why a pack was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackError(pub String);

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PackError {}

/// The pack id rule: `[a-z0-9][a-z0-9-]{0,63}` (`nf_core::config::is_model_id`).
pub fn is_pack_id(id: &str) -> bool {
    nf_core::config::is_model_id(id)
}

/// Windows device names, which no part of a path may be.
const RESERVED: [&str; 26] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The `/`-separated parts of a pack path, refusing anything that could land outside the pack or that Windows reads
/// differently (the same rule as the fetch tool's `safe_parts`).
pub fn safe_parts(path: &str) -> Result<Vec<&str>, PackError> {
    let bad = || PackError(format!("unsafe path {path:?} in the manifest"));
    if path.is_empty() || path.len() > 400 {
        return Err(bad());
    }
    let parts: Vec<&str> = path.split('/').collect();
    for p in &parts {
        let stem = p.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
        if p.is_empty()
            || *p == "."
            || *p == ".."
            || p.chars().any(|c| c.is_control() || "<>:\"|?*\\".contains(c))
            || p.ends_with(['.', ' '])
            || RESERVED.contains(&stem.as_str())
        {
            return Err(bad());
        }
    }
    Ok(parts)
}

impl Pack {
    /// `models_dir()/<id>`, loaded and checked for the runtime ([`Verify::Runtime`]). A missing folder or manifest says how
    /// to fetch it.
    pub fn installed(id: &str) -> Result<Pack, PackError> {
        Self::installed_with(id, Verify::Runtime)
    }

    /// [`Pack::installed`], verifying `which` files.
    pub fn installed_with(id: &str, which: Verify) -> Result<Pack, PackError> {
        Self::load_with(&installed_dir(id)?, which)
    }

    /// Loads and checks the pack in `dir` for the runtime (see the module notes).
    pub fn load(dir: &Path) -> Result<Pack, PackError> {
        Self::load_with(dir, Verify::Runtime)
    }

    /// Loads and checks the pack in `dir`, reading and hashing `which` of its files.
    pub fn load_with(dir: &Path, which: Verify) -> Result<Pack, PackError> {
        Self::load_kind(dir, which, check_noise)
    }

    /// Loads the pack in `dir`: its manifest through the checks every pack takes and `kind`'s (a noise model's, or a quality
    /// metric's: `crate::metric`), then `which` of its files read and hashed.
    pub(crate) fn load_kind(dir: &Path, which: Verify, kind: fn(&Manifest) -> Result<(), String>) -> Result<Pack, PackError> {
        let raw = read_capped(&dir.join("manifest.json"), MAX_MANIFEST)?;
        let manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| PackError(format!("{}: manifest.json: {e}", dir.display())))?;
        let folder = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        check_files(&manifest, folder).and_then(|()| kind(&manifest)).map_err(|e| PackError(format!("pack {}: {e}", dir.display())))?;
        let mut model_bytes = None;
        for f in manifest.files.iter().filter(|f| which == Verify::All || f.role != "conformance") {
            let path = safe_parts(&f.path)?.iter().fold(dir.to_path_buf(), |p, part| p.join(part));
            let size = f.size.unwrap_or(0);
            let data = read_capped(&path, size)?;
            if data.len() as u64 != size {
                return Err(PackError(format!("{}: {} bytes, the manifest says {size}", path.display(), data.len())));
            }
            let sha = sha256_hex(&data).map_err(PackError)?;
            if Some(&sha) != f.sha256.as_ref() {
                return Err(PackError(format!(
                    "{}: SHA-256 {sha}, the manifest says {}: a broken or changed copy",
                    path.display(),
                    f.sha256.as_deref().unwrap_or("?")
                )));
            }
            if f.role == "model" {
                model_bytes = Some(data);
            }
        }
        let model_bytes: Vec<u8> = model_bytes.ok_or_else(|| PackError("no model file".into()))?;
        crate::onnx_check::refuse_external_data(&model_bytes).map_err(|e| PackError(format!("pack {}: {e}", dir.display())))?;
        Ok(Pack { dir: dir.to_path_buf(), manifest, model_bytes })
    }

    /// A file of the pack, by its manifest path (checked).
    pub fn file(&self, path: &str) -> Result<PathBuf, PackError> {
        if !self.manifest.files.iter().any(|f| f.path == path) {
            return Err(PackError(format!("{path} isn't in the pack's manifest")));
        }
        Ok(safe_parts(path)?.iter().fold(self.dir.clone(), |p, part| p.join(part)))
    }

    /// The pack's STFT (checked to exist for a spectral pack).
    pub fn stft(&self) -> &StftSpec {
        // `check` refuses a spectral pack without one.
        self.manifest.graph.stft.as_ref().unwrap_or_else(|| unreachable!("checked at load"))
    }

    /// `48000 / sample_rate`: the bin-mapped STFT's factor.
    pub fn rate_factor(&self) -> usize {
        (nf_core::SAMPLE_RATE / self.manifest.signal.sample_rate.max(1)) as usize
    }

    /// The trained band's upper edge, in Hz.
    pub fn band_hz(&self) -> f32 {
        let s = &self.manifest.signal;
        s.band.unwrap_or(s.sample_rate as f32 / 2.0)
    }
}

/// `models_dir()/<id>`, if it holds a manifest; else how to fetch it.
pub(crate) fn installed_dir(id: &str) -> Result<PathBuf, PackError> {
    if !is_pack_id(id) {
        return Err(PackError(format!("'{id}' isn't a model id ([a-z0-9][a-z0-9-]{{0,63}})")));
    }
    let root = models_dir().ok_or_else(|| PackError("neither NF_MODELS nor LOCALAPPDATA is set".into()))?;
    let dir = root.join(id);
    if !dir.join("manifest.json").is_file() {
        return Err(PackError(format!("the pack isn't installed in {}: python -I tools/fetch_models.py {id}", dir.display())));
    }
    Ok(dir)
}

/// Reads `path` if it is a regular file (not a link) of at most `cap` bytes.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, PackError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| PackError(format!("{}: {e}", path.display())))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(PackError(format!("{} isn't a regular file", path.display())));
    }
    if meta.len() > cap.min(MAX_FILE) {
        return Err(PackError(format!(
            "{} is {} bytes, more than the {} the manifest allows",
            path.display(),
            meta.len(),
            cap.min(MAX_FILE)
        )));
    }
    std::fs::read(path).map_err(|e| PackError(format!("{}: {e}", path.display())))
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A noise model pack's checks (module notes): every pack's, then the noise stage's. `folder` is the pack folder's name.
pub fn check(m: &Manifest, folder: &str) -> Result<(), String> {
    check_files(m, folder)?;
    check_noise(m)
}

/// The checks every pack takes, whatever its kind: the spec, the id and its folder, the files' paths, roles, sizes and
/// hashes, and the one model file.
pub(crate) fn check_files(m: &Manifest, folder: &str) -> Result<(), String> {
    match m.spec.strip_prefix(SPEC_PREFIX) {
        Some("0") => {}
        Some(major) => return Err(format!("spec major version {major} is unknown (this host reads {SPEC})")),
        None => return Err(format!("spec {:?} isn't {SPEC_PREFIX}<major>", m.spec)),
    }
    if !is_pack_id(&m.id) {
        return Err(format!("id {:?} isn't [a-z0-9][a-z0-9-]{{0,63}}", m.id));
    }
    if folder != m.id {
        return Err(format!("the folder is {folder:?} but the id is {:?}: they must be the same", m.id));
    }
    let mut seen: Vec<String> = Vec::new();
    for f in &m.files {
        let key = safe_parts(&f.path).map_err(|e| e.0)?.join("/").to_ascii_lowercase();
        if seen.contains(&key) || key == "manifest.json" {
            return Err(format!("{} is listed twice, or is the manifest", f.path));
        }
        seen.push(key);
        if !["model", "license", "conformance"].contains(&f.role.as_str()) {
            return Err(format!("{}: role {:?}", f.path, f.role));
        }
        match (f.size, f.sha256.as_deref()) {
            (Some(s), Some(h)) if (1..=MAX_FILE).contains(&s) && is_hex(h, 64) => {}
            _ => return Err(format!("{}: needs its size (1 to {MAX_FILE} bytes) and SHA-256 (64 lower-case hex digits)", f.path)),
        }
    }
    let models: Vec<&str> = m.files.iter().filter(|f| f.role == "model").map(|f| f.path.as_str()).collect();
    if models != [m.graph.model.as_str()] {
        return Err(format!("v0 takes exactly one model file, the one graph.model names ({:?}); the files list {models:?}", m.graph.model));
    }
    Ok(())
}

/// What the noise stage can run: a denoise or dereverb pack on the mic, spectral, one channel at a divisor of 48 kHz, its
/// STFT, I/O and state as the host frames them. (A quality metric's pack, role `metric`, fails here: it's no noise model.)
fn check_noise(m: &Manifest) -> Result<(), String> {
    if !["denoise", "dereverb"].contains(&m.role.kind.as_str()) || m.role.inputs != ["mic"] {
        return Err(format!(
            "role {:?} with inputs {:?}: the noise stage takes denoise or dereverb packs on the mic only",
            m.role.kind, m.role.inputs
        ));
    }
    let s = &m.signal;
    if s.sample_rate == 0 || !nf_core::SAMPLE_RATE.is_multiple_of(s.sample_rate) || s.channels != 1 {
        return Err(format!(
            "{} Hz, {} channels: the host takes one channel at 48 kHz or a whole divisor of it",
            s.sample_rate, s.channels
        ));
    }
    if !(s.input_scale.is_finite() && s.input_scale > 0.0) || s.band.is_some_and(|b| !(b > 0.0 && b <= s.sample_rate as f32 / 2.0)) {
        return Err(format!("input_scale {} or band {:?} out of range", s.input_scale, s.band));
    }
    match m.graph.kind.as_str() {
        "spectral" => {}
        "waveform" => return Err("waveform packs aren't supported by this host yet (TODO 3)".into()),
        k => return Err(format!("graph kind {k:?}")),
    }
    let st = m.graph.stft.as_ref().ok_or("a spectral pack needs graph.stft")?;
    if st.window_type != "hann" || st.n_fft != st.window_length || !st.n_fft.is_multiple_of(2) || !(1..=st.n_fft).contains(&st.hop_length) {
        return Err(format!(
            "the STFT {}/{}/{} {}: the host does Hann with n_fft = window_length, even",
            st.n_fft, st.hop_length, st.window_length, st.window_type
        ));
    }
    if st.normalized || st.normalization != "wola" {
        return Err("the host's inverse is WOLA, unnormalised (normalized false, normalization wola)".into());
    }
    let bins = (st.n_fft / 2 + 1) as i64;
    for (what, t) in [("input", &m.io.input), ("output", &m.io.output)] {
        if t.layout != "B,F,T,RI" || t.shape != [1, bins, 1, 2] || t.dtype != "float32" {
            return Err(format!(
                "io.{what}: {} {:?} {}: the host gives one B,F,T,RI frame of float32, [1, {bins}, 1, 2]",
                t.layout, t.shape, t.dtype
            ));
        }
    }
    match (m.io.output.kind.as_deref(), m.io.output.mask.as_deref()) {
        (Some("masked_spectrum"), Some("real" | "complex")) | (Some("spectrum"), None) => {}
        (k, mask) => return Err(format!("io.output kind {k:?} with mask {mask:?}: the host takes spectrum or masked_spectrum")),
    }
    for p in &m.state {
        if p.dtype != "float32" || p.init != "zeros" || p.shape.iter().any(|&d| d <= 0) {
            return Err(format!("state {} → {}: float32 starting at zeros, with a fixed shape", p.input, p.output));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_cngs_and_right() {
        assert_eq!(sha256_hex(b"").unwrap(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for good in ["model.onnx", "conformance/noisy_0174.wav", "LICENSE"] {
            assert!(safe_parts(good).is_ok(), "{good}");
        }
        for bad in ["", "../x", "/abs", "C:/x", "a\\b", "a/../b", "./a", "a//b", "con.wav", "Aux", "x.", "x ", "a:b", "a\u{1}"] {
            assert!(safe_parts(bad).is_err(), "{bad:?}");
        }
    }
}
