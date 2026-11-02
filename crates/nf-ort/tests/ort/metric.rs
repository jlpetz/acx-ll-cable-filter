//! The quality metrics' runners (TODO 14): DNSMOS P.835 and SIGMOS through ONNX Runtime. Neither source publishes reference
//! outputs (DNS-Challenge 591184a and SIG-Challenge bf45251 ship the models and their scripts only), so these pin what the
//! ported runners gave on fixed signals when they were written (a recorded expected value: a change in the framing, the
//! resampler or the calibration moves them), and check that the scores order as the metrics promise: clean speech over the
//! same speech in noise, and a model's cleaned output between them where the noise is concerned. They need the metric packs
//! (`python -I tools/fetch_models.py dnsmos sigmos`) and skip without them (`NF_REQUIRE_MODELS=1` fails instead).

use nf_core::SAMPLE_RATE;
use nf_ort::metric::{self, Dnsmos, Sigmos};
use nf_ort::{Pack, PackError};
use nf_stages::harness::scenes;
use nf_stages::synth::Voice;

use crate::common;

fn require(e: &str) {
    if std::env::var("NF_REQUIRE_MODELS").as_deref() == Ok("1") {
        panic!("NF_REQUIRE_MODELS=1: {e}");
    }
    eprintln!("skipped: {e}");
}

fn dnsmos() -> Option<Dnsmos> {
    Dnsmos::new().map_err(|e| require(&e)).ok()
}

fn sigmos() -> Option<Sigmos> {
    Sigmos::new().map_err(|e| require(&e)).ok()
}

/// 12 s of a synthetic voice in pink noise at `snr_db`, and the voice alone, 48 kHz (deterministic).
fn scene(snr_db: f32) -> (Vec<f32>, Vec<f32>) {
    let s = scenes::noisy_speech(12 * SAMPLE_RATE as usize, Voice::LOW.at(-20.0), snr_db, 11);
    (s.mix, s.speech.samples)
}

#[test]
fn metric_packs_load_as_metrics_and_never_as_noise_models() {
    let Ok(d) = metric::load(Dnsmos::ID, Dnsmos::KIND) else { return require("the dnsmos pack isn't installed") };
    assert_eq!((d.manifest.signal.sample_rate, d.manifest.io.input.name.as_str()), (16_000, "input_1"));
    let e: PackError = metric::load(Dnsmos::ID, Sigmos::KIND).unwrap_err();
    assert!(e.0.contains("not \"sigmos-v1\""), "{e}");
    let e = Pack::installed(Dnsmos::ID).unwrap_err();
    assert!(e.0.contains("role"), "a metric pack isn't a noise model: {e}");
    if common::pack().is_some() {
        let e = metric::load(common::ID, Dnsmos::KIND).unwrap_err();
        assert!(e.0.contains("role"), "a noise pack isn't a metric: {e}");
    }
}

/// DNSMOS on fixed synthetic scenes: the recorded scores, and the noise's order (BAK and OVRL fall as the noise rises;
/// clean speech scores highest).
#[test]
fn dnsmos_scores_a_fixed_scene_as_recorded() {
    let Some(mut m) = dnsmos() else { return };
    let (noisy, clean) = scene(5.0);
    let (quiet, _) = scene(20.0);
    let [n, q, c] = [&noisy, &quiet, &clean].map(|x| m.score_48k(x).expect("scores"));
    for (what, s) in [("5 dB", &n), ("20 dB", &q), ("clean", &c)] {
        eprintln!(
            "DNSMOS {what}: SIG {:.4} BAK {:.4} OVRL {:.4} (raw {:.4} {:.4} {:.4}), {} windows",
            s.sig, s.bak, s.ovrl, s.sig_raw, s.bak_raw, s.ovrl_raw, s.windows
        );
        assert_eq!(s.windows, 3, "12 s: int(12 - 9.01) + 1");
    }
    assert!(c.bak > q.bak && q.bak > n.bak && c.ovrl > n.ovrl, "{c:?} {q:?} {n:?}");
    // Recorded 2026-11-02 (dev VM, ONNX Runtime 1.31, Level 3, deterministic compute), with resampy 0.4's resampling (the
    // exact-sinc one before it gave up to 0.026 different: BAK 2.9297 at 20 dB). The synthetic voice isn't speech to DNSMOS
    // (its SIG falls without the noise), so these pin the runner, not a judgement.
    #[rustfmt::skip]
    let recorded = [(&n, [3.1235, 1.5480, 1.6716]), (&q, [3.3074, 2.9034, 2.5022]), (&c, [2.4477, 3.8224, 2.1849])];
    for (s, [sig, bak, ovrl]) in recorded {
        assert!((s.sig - sig).abs() < 2e-3 && (s.bak - bak).abs() < 2e-3 && (s.ovrl - ovrl).abs() < 2e-3, "{s:?} vs {sig} {bak} {ovrl}");
    }
    // resampy 0.2.2's filter instead of 0.4's: what the choice of resampy does to the scores (up to 0.03, recorded likewise)
    #[rustfmt::skip]
    let older = [(&noisy, &n, [-0.0110, 0.0052, -0.0041]), (&quiet, &q, [0.0055, 0.0296, 0.0128]), (&clean, &c, [0.0005, -0.0057, 0.0009])];
    for (x, s, [sig, bak, ovrl]) in older {
        let old = m.score_16k(&metric::resample::decimate3_with(&metric::resample::KAISER_BEST_0_2, x)).unwrap();
        let d = [old.sig - s.sig, old.bak - s.bak, old.ovrl - s.ovrl];
        eprintln!("  with resampy 0.2.2's: SIG {:+.4} BAK {:+.4} OVRL {:+.4}", d[0], d[1], d[2]);
        assert!((d[0] - sig).abs() < 2e-3 && (d[1] - bak).abs() < 2e-3 && (d[2] - ovrl).abs() < 2e-3, "{d:?}");
    }
    // 16 kHz in directly: the same as 48 kHz through the resampler, by construction
    let direct = m.score_16k(&metric::resample::decimate3(&noisy)).unwrap();
    assert_eq!(direct, n);
    assert!(m.score_48k(&vec![0.0; 48_000]).is_err(), "silence has no score");
}

/// A clip under 9.01 s is repeated until it's long enough, as the script does: a 4 s clip scores as its threefold repeat.
#[test]
fn dnsmos_repeats_a_short_clip() {
    let Some(mut m) = dnsmos() else { return };
    let (noisy, _) = scene(10.0);
    let short = metric::resample::decimate3(&noisy[..4 * SAMPLE_RATE as usize]);
    let mut repeated = short.clone();
    while repeated.len() < Dnsmos::window_len() {
        repeated.extend_from_within(..);
    }
    let (a, b) = (m.score_16k(&short).unwrap(), m.score_16k(&repeated).unwrap());
    assert_eq!(a, b);
    assert_eq!(a.windows, 7, "repeated to 16 s: int(16 - 9.01) + 1");
}

/// SIGMOS on the same scenes: the recorded scores, and NOISE (its noisiness dimension) falls as the noise rises.
#[test]
fn sigmos_scores_a_fixed_scene_as_recorded() {
    let Some(mut m) = sigmos() else { return };
    let (noisy, clean) = scene(5.0);
    let (quiet, _) = scene(20.0);
    let [n, q, c] = [&noisy, &quiet, &clean].map(|x| m.score(x).expect("scores"));
    for (what, s) in [("5 dB", &n), ("20 dB", &q), ("clean", &c)] {
        let v = s.values();
        let parts: Vec<String> = metric::SigmosScore::NAMES.iter().zip(v).map(|(k, v)| format!("{k} {v:.4}")).collect();
        eprintln!("SIGMOS {what}: {}", parts.join(", "));
    }
    assert!(c.noise > q.noise && q.noise > n.noise, "{c:?} {q:?} {n:?}");
    #[rustfmt::skip]
    let recorded = [
        (&n, [3.0612, 4.6396, 1.7364, 1.0079, 4.3792, 2.4496, 1.3676]),
        (&q, [3.7431, 4.8864, 3.7915, 1.2021, 4.7814, 3.3565, 1.8857]),
        (&c, [3.6356, 3.9082, 2.9731, 4.0802, 4.2824, 3.3084, 2.8004]),
    ];
    for (s, want) in recorded {
        for (k, (got, want)) in metric::SigmosScore::NAMES.iter().zip(s.values().into_iter().zip(want)) {
            assert!((got - want).abs() < 2e-3, "{k}: {got} vs {want}");
        }
    }
}

/// The UL-UNAS pack's conformance clip (real 16 kHz speech from the DNS set, its noisy input and upstream's enhanced output):
/// DNSMOS ranks the enhanced output's BAK over the noisy input's, and the clean speech's over both.
#[test]
fn dnsmos_ranks_a_real_clip_s_clean_enhanced_and_noisy() {
    let (Some(mut m), Some(p)) = (dnsmos(), common::pack()) else { return };
    let [noisy, enhanced, clean] =
        ["conformance/noisy_0174.wav", "conformance/enhanced_0174.wav", "conformance/clean_0174.wav"].map(|f| common::read16(&p, f));
    let [n, e, c] = [&noisy, &enhanced, &clean].map(|x| m.score_16k(x).expect("scores"));
    eprintln!("DNSMOS 0174: noisy {n:?}\n  enhanced {e:?}\n  clean {c:?}");
    assert!(c.bak > n.bak && e.bak > n.bak && e.ovrl > n.ovrl, "{n:?} {e:?} {c:?}");
}

/// Writes a fixed 48 kHz signal, its SIGMOS features and its DNSMOS resampling as raw little-endian f32 into
/// `NF_METRIC_DUMP`, for `tools/check_metric_features.py`, which recomputes both with numpy: the STFT as sigmos.py frames it,
/// the resampling as librosa 0.8.1 and resampy 0.4 do it (and as resampy 0.2.2 did, for scale). Ignored: `NF_METRIC_DUMP=DIR cargo test -p nf-ort --test ort dump_metric_features --
/// --ignored`, then `python -I tools/check_metric_features.py DIR`.
#[test]
#[ignore]
fn dump_metric_features() {
    let Some(dir) = std::env::var_os("NF_METRIC_DUMP") else { return };
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let (mut x, _) = scene(10.0);
    // Not a multiple of 3, so librosa pads the resampling with a zero
    x.truncate(5 * SAMPLE_RATE as usize + 124);
    let (features, frames) = Sigmos::features(&x);
    let raw = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    std::fs::write(dir.join("signal.f32"), raw(&x)).unwrap();
    std::fs::write(dir.join("sigmos_features.f32"), raw(&features)).unwrap();
    std::fs::write(dir.join("dnsmos_16k.f32"), raw(&metric::resample::decimate3(&x))).unwrap();
    eprintln!("{} samples, {frames} SIGMOS frames, into {}", x.len(), dir.display());
}
