//! What the tests share: the pack (or a skip), the controls, the probe, signals and runners.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_core::noise::{DEFAULT_DEPTH, Executor, NoHooks, NoiseStage};
use nf_core::stage::{Activation, Stage};
use nf_ort::heap_probe::{self, ProbeStatus};
use nf_ort::pack::Verify;
use nf_ort::{Pack, SessionOptions};

pub const ID: &str = "ulunas";
pub const SR16: usize = 16_000;

/// The fetched pack with every file verified (its conformance pairs too), or `None` with a message (a failure with
/// `NF_REQUIRE_MODELS=1`).
pub fn pack() -> Option<Pack> {
    static PACK: OnceLock<Result<Pack, String>> = OnceLock::new();
    match PACK.get_or_init(|| Pack::installed_with(ID, Verify::All).map_err(|e| e.0)) {
        Ok(p) => Some(p.clone()),
        Err(e) if std::env::var("NF_REQUIRE_MODELS").as_deref() == Ok("1") => panic!("NF_REQUIRE_MODELS=1: {e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

pub fn controls(cfg: ProcessingConfig) -> Arc<ChainControls> {
    Arc::new(ChainControls::new(&cfg))
}

/// The floor off (-100 dB) and everything else default: the model's output alone, for parity and latency.
pub fn model_only() -> ProcessingConfig {
    ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() }
}

/// The noise stage around the pack, its model inline or on a waited-for worker (offline: bit-identical) at the default
/// depth, activated.
pub fn stage(pack: &Pack, cfg: ProcessingConfig, thread: bool) -> NoiseStage {
    let exec = if thread { Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: true } } else { Executor::Inline };
    let (mut s, _) =
        nf_ort::pack_stage(controls(cfg), pack.clone(), exec, SessionOptions::default(), DEFAULT_DEPTH).expect("the pack's stage");
    s.activate(&Activation::HOST).expect("activate");
    s
}

/// The heap probe, installed once for the binary.
pub fn probe() -> ProbeStatus {
    static STATUS: OnceLock<ProbeStatus> = OnceLock::new();
    STATUS.get_or_init(|| heap_probe::install().expect("the heap probe")).clone()
}

/// A 16 kHz PCM16 conformance file of the pack, in ±1 (÷32768).
pub fn read16(pack: &Pack, rel: &str) -> Vec<f32> {
    let path = pack.file(rel).expect("in the manifest");
    let (x, rate) = nf_stages::harness::audio::read_wav(&path).expect("a WAV");
    assert_eq!(rate as usize, SR16, "{}", path.display());
    x
}

/// `x` (16 kHz) upsampled ×3 by FFT zero-padding (exact for a periodic, band-limited signal), in f64.
pub fn upsample3(x: &[f32]) -> Vec<f32> {
    use realfft::RealFftPlanner;
    let n = x.len();
    let mut p = RealFftPlanner::<f64>::new();
    let (f, i) = (p.plan_fft_forward(n), p.plan_fft_inverse(3 * n));
    let mut buf: Vec<f64> = x.iter().map(|&v| f64::from(v)).collect();
    let mut spec = f.make_output_vec();
    f.process(&mut buf, &mut spec).expect("fft");
    let mut big = i.make_input_vec();
    big[..spec.len()].copy_from_slice(&spec);
    // The 16 kHz Nyquist bin is shared by its two halves: halve it in the larger spectrum.
    if n.is_multiple_of(2) {
        big[n / 2] *= 0.5;
    }
    let mut out = i.make_output_vec();
    i.process(&mut big, &mut out).expect("ifft");
    out.iter().map(|&v| (v / n as f64) as f32).collect()
}

/// Writes `x` at `rate` to `$NF_STAGES_WAV_DIR/<name>.wav`, when that's set.
pub fn dump(name: &str, x: &[f32], rate: u32) {
    if let Some(dir) = std::env::var_os("NF_STAGES_WAV_DIR") {
        let path = Path::new(&dir).join(format!("{name}.wav"));
        let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let write = || -> Result<(), hound::Error> {
            let mut w = hound::WavWriter::create(&path, spec)?;
            for &s in x {
                w.write_sample(s)?;
            }
            w.finalize()
        };
        if let Err(e) = write() {
            eprintln!("couldn't write {}: {e}", path.display());
        }
    }
}

/// A fresh temporary folder for this test.
pub fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nf-ort-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// The stage as a trait object.
pub fn boxed(s: NoiseStage) -> Box<dyn Stage> {
    Box::new(s)
}
