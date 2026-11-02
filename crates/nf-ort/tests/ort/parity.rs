//! Parity (TODO 5's tests, item 4):
//! - **P1:** the native framing against this test's own loop: an independent f64 STFT with causal framing, a second session
//!   built with ONNX Runtime's defaults and run the plain way (fresh tensors each frame, named inputs, the caches fed back),
//!   and an f64 WOLA inverse. Same model, so the bound is float rounding.
//! - **P2:** the conformance pairs, framed as torch frames them, against the upstream `enhanced_*.wav` as PCM16. The
//!   reference was probably made on CUDA by the offline (non-streaming) model, so the bound is loose, pinned from the first
//!   run.
//! - **P3:** the bin-mapped 48 kHz framing against the native one on `noisy_0174` upsampled ×3: the model's input and output
//!   frames, compared by band, with the default mapping (bin 256 real) and with the top 8 bins folded too.

use std::collections::HashMap;

use nf_core::config::HighBand;
use nf_core::dsp::stft::hann;
use nf_ort::conformance::{compare_pcm16, torch_framed};
use nf_ort::{NativeSpectral, OrtSpectralModel, Pack, SessionOptions};
use ort::session::Session;
use ort::value::Tensor;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex64;

use crate::common::{self, controls, read16};

/// P1's bound on the largest difference (pinned from the first run: see `doc/todo/5-candidate-models.md`).
const P1_MAX_ABS: f32 = 2e-5;
/// P2's bound on the RMS error against the reference, in dB, for a pack whose manifest gives none ("report only").
const P2_RMS_DB: f32 = -30.0;

/// The model over `x` through this test's own f64 STFT and a plainly run session (P1's reference).
fn reference(p: &Pack, x: &[f32]) -> Vec<f32> {
    let env = nf_ort::env::env().expect("env");
    let mut session = Session::builder(env).unwrap().commit_from_memory(&p.model_bytes).expect("a plain session");
    let (n, hop) = (512usize, 256usize);
    let w: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).collect();
    let mut planner = RealFftPlanner::<f64>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut ring = vec![0.0f64; n];
    let mut acc = vec![0.0f64; n];
    let env_w: Vec<f64> = (0..hop).map(|i| w[i] * w[i] + w[i + hop] * w[i + hop]).collect();
    let mut caches: HashMap<String, Vec<f32>> =
        p.manifest.state.iter().map(|s| (s.input.clone(), vec![0.0; s.shape.iter().product::<i64>() as usize])).collect();
    let mut y = vec![0.0f32; x.len().div_ceil(hop) * hop];
    let mut padded = x.to_vec();
    padded.resize(y.len(), 0.0);
    for (k, chunk) in padded.chunks_exact(hop).enumerate() {
        ring.copy_within(hop.., 0);
        for (r, &v) in ring[n - hop..].iter_mut().zip(chunk) {
            *r = f64::from(v);
        }
        let mut frame: Vec<f64> = ring.iter().zip(&w).map(|(a, b)| a * b).collect();
        let mut spec = fwd.make_output_vec();
        fwd.process(&mut frame, &mut spec).unwrap();
        let mix: Vec<f32> = spec.iter().flat_map(|c| [c.re as f32, c.im as f32]).collect();
        let mut inputs = vec![("mix".to_owned(), Tensor::from_array(([1usize, 257, 1, 2], mix)).unwrap().into_dyn())];
        for s in &p.manifest.state {
            let shape: Vec<usize> = s.shape.iter().map(|&d| d as usize).collect();
            inputs.push((s.input.clone(), Tensor::from_array((shape, caches[&s.input].clone())).unwrap().into_dyn()));
        }
        let out = session.run(inputs).expect("a plain run");
        let (_, enh) = out["enh"].try_extract_tensor::<f32>().unwrap();
        let mut e: Vec<Complex64> = enh.as_chunks::<2>().0.iter().map(|c| Complex64::new(f64::from(c[0]), f64::from(c[1]))).collect();
        for s in &p.manifest.state {
            caches.insert(s.input.clone(), out[s.output.as_str()].try_extract_tensor::<f32>().unwrap().1.to_vec());
        }
        e[0].im = 0.0;
        e[256].im = 0.0;
        let mut t = inv.make_output_vec();
        inv.process(&mut e, &mut t).unwrap();
        for i in 0..n {
            acc[i] += t[i] / n as f64 * w[i];
        }
        for i in 0..hop {
            y[k * hop + i] = (acc[i] / env_w[i]) as f32;
        }
        acc.copy_within(hop.., 0);
        acc[n - hop..].fill(0.0);
    }
    y.truncate(x.len());
    y
}

#[test]
fn p1_the_native_framing_matches_an_independent_implementation() {
    let Some(p) = common::pack() else { return };
    for name in ["0174", "0119"] {
        let x = read16(&p, &format!("conformance/noisy_{name}.wav"));
        let ours = NativeSpectral::new(&p, SessionOptions::default()).expect("native").run(&x);
        let theirs = reference(&p, &x);
        let par = nf_stages::harness::parity(&ours, &theirs, 0);
        eprintln!("P1 noisy_{name}: max |diff| {:.3e}, RMS error {:.1} dB over {} samples", par.max_abs, par.rms_err_db, par.compared);
        assert!(par.max_abs <= P1_MAX_ABS, "noisy_{name}: {par:?}");
    }
}

#[test]
fn p2_the_conformance_pairs_match_the_upstream_reference() {
    let Some(p) = common::pack() else { return };
    for c in &p.manifest.conformance {
        let x = read16(&p, &c.input);
        let reference = read16(&p, &c.output);
        let y = torch_framed(&p, &x, SessionOptions::default()).expect("torch-framed");
        let r = compare_pcm16(&y[c.align_samples.min(y.len())..], &reference, c.skip_samples);
        eprintln!(
            "P2 {}: max {} LSB, RMS error {:.1} dB, {} of {} samples over one LSB",
            c.output, r.max_lsb, r.rms_err_db, r.over_one_lsb, r.compared
        );
        common::dump(&format!("p2-{}", c.output.replace('/', "-")), &y, 16_000);
        let rms = c.rms_db.unwrap_or(P2_RMS_DB);
        assert!(r.rms_err_db < rms, "{}: {r:?} against {rms} dB", c.output);
        if let Some(max) = c.max_abs {
            assert!(r.max_lsb as f32 / 32768.0 <= max, "{}: {r:?} against the manifest's max_abs {max}", c.output);
        }
    }
}

/// Error energy over reference energy, in dB, per band of model bins.
#[derive(Default)]
struct Bands {
    err: [f64; 5],
    refe: [f64; 5],
}

const BAND_NAMES: [&str; 5] =
    ["0-4 kHz (bins 0-127)", "4-7 kHz (128-223)", "7-7.75 kHz (224-247)", "7.75-8 kHz (248-255)", "8 kHz (bin 256)"];

fn band_of(k: usize) -> usize {
    match k {
        0..128 => 0,
        128..224 => 1,
        224..248 => 2,
        248..256 => 3,
        _ => 4,
    }
}

impl Bands {
    fn add(&mut self, ours: &[f32], native: &[f32]) {
        for (k, (a, b)) in ours.as_chunks::<2>().0.iter().zip(native.as_chunks::<2>().0.iter()).enumerate() {
            let (dr, di) = (f64::from(a[0] - b[0]), f64::from(a[1] - b[1]));
            let i = band_of(k);
            self.err[i] += dr * dr + di * di;
            self.refe[i] += f64::from(b[0]).powi(2) + f64::from(b[1]).powi(2);
        }
    }

    fn db(&self) -> [f64; 5] {
        std::array::from_fn(|i| 10.0 * (self.err[i] / self.refe[i].max(1e-30)).log10())
    }
}

#[test]
fn p3_the_bin_mapped_framing_matches_the_native_one_below_7_7_khz() {
    let Some(p) = common::pack() else { return };
    // Near-silence on both sides first (white noise at -100 dBFS, at 16 kHz: band-limited): then every causal frame, the
    // first ones too, holds a band-limited signal at both rates (a frame that is half zeros and half signal has a hard edge
    // mid-window, whose spectrum above 8 kHz aliases at 16 kHz), and the FFT interpolation's wrap-around joins like to like.
    // Not exact zeros: the model's features are log10(max(|X|, 1e-12)), so digital silence at 16 kHz against the 48 kHz
    // interpolation's ringing (1e-9 or so) differs by decades and sets the recurrent state apart (live input is dithered).
    let pad = |n: usize, seed: u64| nf_stages::synth::white_noise(n, 1e-5, seed);
    let mut x16 = pad(2048, 5);
    x16.extend(read16(&p, "conformance/noisy_0174.wav"));
    x16.extend(pad(2048, 6));
    let x48 = common::upsample3(&x16);
    use nf_core::noise::NoiseModel;
    let mut results = Vec::new();
    for fold in [0usize, 8] {
        let mut native = NativeSpectral::new(&p, SessionOptions::default()).expect("native");
        let c = controls(nf_core::config::ProcessingConfig { high_band: HighBand::Zero, ..crate::common::model_only() });
        let mut mapped = OrtSpectralModel::new(&p, c, SessionOptions::default()).expect("bin-mapped");
        mapped.set_fold(fold);
        let (mut mix, mut enh) = (Bands::default(), Bands::default());
        let (mut y16, mut y48) = (vec![0.0f32; 256], vec![0.0f32; 768]);
        let mut out16 = Vec::with_capacity(x16.len());
        for (h16, h48) in x16.as_chunks::<256>().0.iter().zip(x48.as_chunks::<768>().0) {
            native.process_hop(h16, &mut y16);
            mapped.process_hop(h48, &mut y48);
            out16.extend_from_slice(&y16);
            mix.add(mapped.core().last_mix(), native.core().last_mix());
            enh.add(mapped.core().last_enh(), native.core().last_enh());
        }
        let (m, e) = (mix.db(), enh.db());
        for i in 0..5 {
            eprintln!("P3 fold {fold}: {:<22} model input {:7.1} dB   model output {:7.1} dB", BAND_NAMES[i], m[i], e[i]);
        }
        results.push((m, e));
    }
    let ((m, e), (fm, fe)) = (results[0], results[1]);
    // Pinned from the run of 2026-11-02 with the Nyquist bin mapped real (`2·Re(X[256]) / 3`, `spectral.rs`), about 4 dB
    // inside the figures (doc/todo/3-model-spec.md, P3): the input matches below 7.75 kHz and at bin 256 (-137.8, -109.4,
    // -89.8 and -81.0 dB; the top 8 bins -27.4), and so the model's output to -46.4 dB below 4 kHz, -58.4 at 4-7 kHz,
    // -52.5 at 7-7.75 kHz, -34.8 in the top 8 bins and -25.1 at bin 256 (with bin 256 complex, as first designed: -38.9 dB
    // below 4 kHz and +1.3 at bin 256). Folded (the A/B option), both match everywhere.
    assert!(m[0] < -130.0 && m[1] < -105.0 && m[2] < -85.0 && m[4] < -75.0, "the input matches but in the top 8 bins: {m:?}");
    assert!(e[0] < -42.0 && e[1] < -54.0 && e[2] < -48.0 && e[3] < -30.0 && e[4] < -20.0, "the output: {e:?}");
    assert!(fm.iter().all(|&d| d < -75.0), "folded, the input matches at every bin: {fm:?}");
    assert!(fe[0] < -53.0 && fe[1..4].iter().all(|&d| d < -70.0) && fe[4] < -58.0, "and so does the output: {fe:?}");
    // The window's taper is the same curve at both rates.
    assert_eq!(hann(1536, true)[3 * 100], hann(512, true)[100]);
}
