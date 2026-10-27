//! Exponential sine sweeps (Farina 2000) and probe trains for the latency test.
//!
//! Contract:
//! - `exp_sweep(fs, f1, f2, duration_s, fade_s, amplitude)`:
//!   `x(t) = A sin(2π f1 L (e^{t/L} - 1))`, with `L = T / ln(f2/f1)`, and raised-cosine fades of
//!   `fade_s` at both ends.
//!   - `inverse` is the time-reversed sweep with an amplitude envelope that falls 6 dB/octave
//!     (`e^{-t/L}`, reversed), normalised so that `signal ⊛ inverse` peaks at 1.0 (check by
//!     convolution in a test).
//!   - Defaults the engine will use: 100 Hz-16 kHz, 1.0 s, 10 ms fades, amplitude 0.25 (-12 dBFS).
//! - `probe_train(sweep, repetitions, gap_s)`: silence of `gap_s`, then the sweep, then silence of
//!   `gap_s`, repeated; returns the buffer and the start index of each sweep.
//! - Tests: the deconvolved peak sits at the sweep's start index (±0 samples) for an identity path,
//!   and the sweep's spectrum is flat within ±1 dB over 200 Hz-12 kHz after inverse filtering.
//!
//! The phase is computed in f64 (it reaches about 10⁵ rad, where an f32 ulp is 0.008 rad). `T` is
//! the sweep's length rounded to whole samples. `signal ⊛ inverse` is linear-phase and centred on
//! index `len - 1` (the sweep's group delay plus the inverse's is the same at every frequency), so
//! the normalisation sets that sample to 1 directly; a sweep that arrived at index `p` therefore
//! deconvolves to a peak at `p + len - 1`.

use std::f64::consts::PI;

/// A sweep and its inverse filter.
#[derive(Clone, Debug)]
pub struct Sweep {
    pub signal: Vec<f32>,
    pub inverse: Vec<f32>,
    pub sample_rate: u32,
    pub f1: f32,
    pub f2: f32,
}

/// Builds an exponential sine sweep and its inverse filter.
///
/// # Panics
/// Unless `0 < f1 < f2 < sample_rate / 2` and `duration_s > 0`. A negative `fade_s` is 0, and fades
/// longer than half the sweep are cut to half.
pub fn exp_sweep(sample_rate: u32, f1: f32, f2: f32, duration_s: f32, fade_s: f32, amplitude: f32) -> Sweep {
    let fs = f64::from(sample_rate);
    let (lo, hi) = (f64::from(f1), f64::from(f2));
    assert!(lo > 0.0 && lo < hi && hi < 0.5 * fs, "exp_sweep: need 0 < f1 < f2 < fs/2, got {f1}, {f2} at {sample_rate}");
    assert!(duration_s > 0.0, "exp_sweep: duration must be positive, got {duration_s}");

    let n = ((f64::from(duration_s) * fs).round() as usize).max(2);
    let l = n as f64 / fs / (hi / lo).ln(); // L = T / ln(f2/f1), seconds
    let k = 2.0 * PI * lo * l; // ω1·L
    let fade = ((f64::from(fade_s).max(0.0) * fs).round() as usize).min(n / 2);
    let amp = f64::from(amplitude);
    let signal: Vec<f32> = (0..n)
        .map(|i| {
            let edge = i.min(n - 1 - i);
            let w = if edge < fade { 0.5 - 0.5 * (PI * edge as f64 / fade as f64).cos() } else { 1.0 };
            let t = i as f64 / fs;
            (amp * w * (k * (t / l).exp_m1()).sin()) as f32
        })
        .collect();

    // inverse[i] = x[n-1-i] · e^{-t_i/L}: 0 dB at f2 down to f1/f2 at f1 (+6 dB/octave).
    let decay = -1.0 / (fs * l);
    let raw: Vec<f64> = (0..n).map(|i| f64::from(signal[n - 1 - i]) * (decay * i as f64).exp()).collect();
    // (signal ⊛ inverse)[n-1] = Σ_k x[k] · inverse[n-1-k].
    let centre: f64 = signal.iter().zip(raw.iter().rev()).map(|(&x, &f)| f64::from(x) * f).sum();
    let g = if centre > 0.0 { 1.0 / centre } else { 0.0 };
    let inverse = raw.iter().map(|&v| (v * g) as f32).collect();
    Sweep { signal, inverse, sample_rate, f1, f2 }
}

/// `repetitions` sweeps, each preceded and followed by `gap_s` of silence. Returns the buffer and
/// each sweep's start index.
pub fn probe_train(sweep: &Sweep, repetitions: usize, gap_s: f32) -> (Vec<f32>, Vec<usize>) {
    let gap = (f64::from(gap_s).max(0.0) * f64::from(sweep.sample_rate)).round() as usize;
    let n = sweep.signal.len();
    let period = 2 * gap + n;
    let mut buf = vec![0.0f32; period * repetitions];
    let mut starts = Vec::with_capacity(repetitions);
    for (r, chunk) in buf.chunks_exact_mut(period.max(1)).enumerate().take(repetitions) {
        chunk[gap..gap + n].copy_from_slice(&sweep.signal);
        starts.push(r * period + gap);
    }
    (buf, starts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::analysis::{fft_convolve, find_sweep_arrivals};
    use realfft::RealFftPlanner;
    use realfft::num_complex::Complex;

    fn default_sweep() -> Sweep {
        exp_sweep(48_000, 100.0, 16_000.0, 1.0, 0.01, 0.25)
    }

    #[test]
    fn shape_and_fades() {
        let s = default_sweep();
        assert_eq!((s.signal.len(), s.inverse.len()), (48_000, 48_000));
        assert_eq!((s.sample_rate, s.f1, s.f2), (48_000, 100.0, 16_000.0));
        assert_eq!(s.signal[0], 0.0);
        assert!(s.signal[s.signal.len() - 1].abs() < 1e-6);
        let peak = s.signal.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= 0.25 && peak > 0.249, "{peak}");
        // The instantaneous frequency doubles every L ln 2: count zero crossings near the start
        // (about 2 f1 per second) and near the end (about 2 f2).
        let crossings = |x: &[f32]| x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let l = 1.0 / (160f64).ln();
        let early = crossings(&s.signal[480..4800]) as f64 / 0.09; // 10-100 ms
        let f_early = 100.0 * ((0.01 + 0.1) / 2.0 / l).exp();
        assert!((early / 2.0 / f_early - 1.0).abs() < 0.1, "{} vs {f_early}", early / 2.0);
        let late = crossings(&s.signal[47_000..47_500]) as f64 / (500.0 / 48_000.0);
        let f_late = 100.0 * (47_250.0 / 48_000.0 / l).exp(); // about 14.8 kHz
        assert!((late / 2.0 / f_late - 1.0).abs() < 0.05, "{} vs {f_late}", late / 2.0);
    }

    #[test]
    fn signal_convolved_with_inverse_peaks_at_one_in_the_centre() {
        let s = default_sweep();
        let n = s.signal.len();
        let h = fft_convolve(&s.signal, &s.inverse);
        let (imax, vmax) = h.iter().enumerate().fold((0, 0.0f32), |b, (i, &v)| if v.abs() > b.1 { (i, v.abs()) } else { b });
        assert_eq!(imax, n - 1);
        assert!((vmax - 1.0).abs() < 1e-4, "{vmax}");
        // Linear phase: the neighbours are symmetric, so the refined peak is exactly on n - 1.
        assert!((h[n - 2] - h[n]).abs() < 1e-4, "{} vs {}", h[n - 2], h[n]);
    }

    #[test]
    fn inverse_filtered_spectrum_is_flat_over_200_hz_to_12_khz() {
        let s = default_sweep();
        let h = fft_convolve(&s.signal, &s.inverse);
        let m = h.len().next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(m);
        let mut x = r2c.make_input_vec();
        x[..h.len()].copy_from_slice(&h);
        let mut spec = r2c.make_output_vec();
        r2c.process(&mut x, &mut spec).unwrap();
        let bin_hz = 48_000.0 / m as f64;
        let db: Vec<f64> = spec
            .iter()
            .enumerate()
            .filter(|(k, _)| (200.0..=12_000.0).contains(&(*k as f64 * bin_hz)))
            .map(|(_, c): (usize, &Complex<f32>)| 10.0 * f64::from(c.norm_sqr()).log10())
            .collect();
        let mean = db.iter().sum::<f64>() / db.len() as f64;
        let (lo, hi) = db.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        assert!(hi - mean <= 1.0 && mean - lo <= 1.0, "mean {mean:.2} dB, range {lo:.2}..{hi:.2}");
    }

    #[test]
    fn probe_train_layout() {
        let s = exp_sweep(48_000, 100.0, 16_000.0, 0.1, 0.005, 0.5);
        let (buf, starts) = probe_train(&s, 3, 0.05);
        let (n, gap) = (4800, 2400);
        assert_eq!(buf.len(), 3 * (n + 2 * gap));
        assert_eq!(starts, [gap, 3 * gap + n, 5 * gap + 2 * n]);
        for &st in &starts {
            assert_eq!(&buf[st..st + n], &s.signal[..]);
            assert!(buf[st - gap..st].iter().all(|&v| v == 0.0));
        }
        assert!(buf[starts[2] + n..].iter().all(|&v| v == 0.0));
        let (empty, none) = probe_train(&s, 0, 0.05);
        assert!(empty.is_empty() && none.is_empty());
    }

    #[test]
    fn the_identity_path_deconvolves_to_the_start_indices() {
        let s = default_sweep();
        let (train, starts) = probe_train(&s, 3, 0.25);
        let arr = find_sweep_arrivals(&train, &s, 3, s.signal.len() + 2 * 12_000);
        assert_eq!(arr.len(), 3);
        for (a, &st) in arr.iter().zip(&starts) {
            assert_eq!(a.position.round() as usize, st, "±0 samples");
            assert!((a.position - st as f64).abs() < 1e-3, "{}", a.position);
        }
    }

    #[test]
    #[should_panic(expected = "f1 < f2")]
    fn rejects_an_inverted_band() {
        exp_sweep(48_000, 1000.0, 100.0, 1.0, 0.01, 0.25);
    }

    #[test]
    #[should_panic(expected = "f1 < f2")]
    fn rejects_a_band_past_nyquist() {
        exp_sweep(48_000, 100.0, 24_000.0, 1.0, 0.01, 0.25);
    }
}
