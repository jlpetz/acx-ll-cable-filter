//! Arrival detection for the latency test.
//!
//! Contract (using realfft: FFT convolution / correlation, zero-padded to a power of two
//! ≥ len_a + len_b - 1):
//! - `find_sweep_arrivals(recording, sweep, repetitions, min_spacing)`:
//!   - deconvolve (`recording ⊛ sweep.inverse`), giving an impulse response that peaks where each
//!     sweep *started* in the recording, after removing the inverse filter's own length offset;
//!   - for each repetition, take the **first** peak within 6 dB of that repetition's maximum (the
//!     direct path, not a later reflection), searching windows at least `min_spacing` apart;
//!   - refine with parabolic interpolation on |h| (sub-sample);
//!   - `confidence_db` = the peak over the RMS of |h| in that repetition's window outside ±2 ms of the
//!     peak.
//!
//!   Returns at most `repetitions` arrivals in time order. A repetition with `confidence_db < 15` is
//!   still returned; the caller filters.
//! - `gcc_phat(a, b, max_lag, beta)`: the delay of `b` relative to `a` in samples (positive = b
//!   lags), using PHAT-β weighting (`|X|^β`; β ≈ 0.7-0.8 for speech), lags limited to ±max_lag,
//!   parabolic refinement. Returns `(lag, confidence_db)`, or `None` if either input is all zeros.
//! - `xcorr_delay(a, b, max_lag)`: plain normalised cross-correlation peak, for waveform-preserving
//!   paths; same return.
//! - Tests: synthetic delays of 0, 1, 480, 1234.37 (fractional, via windowed-sinc delay) samples are
//!   recovered within 0.05 samples at 40 dB SNR; with a -6 dB echo 5 ms later the first arrival is
//!   still chosen; gcc_phat on a vowel-like harmonic signal recovers the delay.
//!
//! How it does it:
//! - Each call plans its FFTs once and allocates its buffers once; nothing allocates in a loop.
//! - Windows: repetitions are found strongest first. Each pick excludes `min_spacing` either side
//!   for later picks, and its window is ±`min_spacing / 2` around it. The first arrival is searched
//!   from the window's start up to the strongest peak.
//! - The strongest-first search keeps the maximum of every 4096 samples of |h|: a pick
//!   scans those maxima and one chunk, and an exclusion rescans only the chunks it touched. So 100
//!   repetitions of a 200 s recording cost one pass over |h|, not one per repetition (review RT-2).
//!   Ties go to the later index, as `Iterator::max_by` did.
//! - Sub-sample refinement: parabolic interpolation, then Newton steps on the band-limited
//!   interpolation of the same sequence. Parabolic alone is biased towards the integer by up to
//!   ~0.045 samples for the default sweep's band and ~0.1 for a full-band PHAT peak; the Newton
//!   steps remove that.
//!   - `gcc_phat` and `xcorr_delay` evaluate it exactly from the kept spectrum (O(m) a step; m is
//!     small there).
//!   - `find_sweep_arrivals` evaluates it as the sinc sum over ±2048 samples around the
//!     peak (O(4097) a step instead of O(m), with m up to 2^24 for 100 repetitions). The tails left
//!     out move the estimate by about 1e-3 samples at 40 dB SNR; the test against the spectrum
//!     method holds them to that.
//! - `gcc_phat` and `xcorr_delay` report `confidence_db` as the peak over the RMS of the
//!   correlation in the searched lags outside ±2 ms of the peak (all lags, if that leaves none).
//!   `xcorr_delay` scales by the inputs' energies, so its peak is the correlation coefficient.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::sweep::Sweep;
use crate::SAMPLE_RATE;

/// Where one sweep arrived in a recording.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Arrival {
    /// Sub-sample index in the recording at which the sweep's first sample arrived.
    pub position: f64,
    /// |h| at the peak.
    pub peak: f32,
    /// Peak over the surrounding |h| RMS, in dB.
    pub confidence_db: f32,
}

/// The direct path is the first peak within this of the window's strongest.
const FIRST_WITHIN_DB: f32 = 6.0;
/// Half-width of the region around a peak left out of the confidence RMS.
const GUARD_MS: f64 = 2.0;
/// Newton evaluations at most per refined peak.
const NEWTON_STEPS: usize = 5;
/// The repetition search keeps the maximum of every this many samples of |h|.
const SEARCH_CHUNK: usize = 4096;
/// `find_sweep_arrivals` refines with the samples within this of the peak.
const REFINE_HALF: usize = 2048;

type C32 = Complex<f32>;

/// One FFT length's plans and scratch, for one call.
struct Fft {
    m: usize,
    r2c: Arc<dyn RealToComplex<f32>>,
    c2r: Arc<dyn ComplexToReal<f32>>,
    scratch: Vec<C32>,
}

impl Fft {
    /// Plans a power-of-two length of at least `n` (and at least 2).
    fn new(n: usize) -> Self {
        let m = n.max(2).next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(m);
        let c2r = planner.plan_fft_inverse(m);
        let scratch = vec![C32::new(0.0, 0.0); r2c.get_scratch_len().max(c2r.get_scratch_len())];
        Self { m, r2c, c2r, scratch }
    }

    fn time_vec(&self) -> Vec<f32> {
        vec![0.0; self.m]
    }

    fn spec_vec(&self) -> Vec<C32> {
        vec![C32::new(0.0, 0.0); self.m / 2 + 1]
    }

    /// `spec = FFT(x zero-padded)`, using `time` (length m) as the input buffer.
    fn forward(&mut self, x: &[f32], time: &mut [f32], spec: &mut [C32]) -> Option<()> {
        let n = x.len().min(time.len());
        time[..n].copy_from_slice(&x[..n]);
        time[n..].fill(0.0);
        self.r2c.process_with_scratch(time, spec, &mut self.scratch).ok()
    }

    /// `time = IFFT(spec)` without the 1/m (fold it into `spec`). Destroys `spec`.
    fn inverse(&mut self, spec: &mut [C32], time: &mut [f32]) -> Option<()> {
        // c2r needs real DC and Nyquist bins, or it reports an error after transforming.
        if let Some(first) = spec.first_mut() {
            first.im = 0.0;
        }
        if let Some(last) = spec.last_mut() {
            last.im = 0.0;
        }
        self.c2r.process_with_scratch(spec, time, &mut self.scratch).ok()
    }
}

/// Linear convolution of `a` and `b` (length `a.len() + b.len() - 1`) by FFT. Empty if either is.
#[cfg(test)]
pub(crate) fn fft_convolve(a: &[f32], b: &[f32]) -> Vec<f32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let n = a.len() + b.len() - 1;
    let mut fft = Fft::new(n);
    let (mut time, mut sa, mut sb) = (fft.time_vec(), fft.spec_vec(), fft.spec_vec());
    let ok = fft.forward(a, &mut time, &mut sa).and_then(|()| fft.forward(b, &mut time, &mut sb));
    if ok.is_none() {
        return Vec::new();
    }
    let scale = 1.0 / fft.m as f32;
    for (x, y) in sa.iter_mut().zip(&sb) {
        *x = *x * *y * scale;
    }
    if fft.inverse(&mut sa, &mut time).is_none() {
        return Vec::new();
    }
    time.truncate(n);
    time
}

/// Vertex offset of the parabola through (-1, a), (0, b), (1, c), and its height. Offset 0 (and
/// height b) unless b is a strict local maximum.
fn parabolic(a: f32, b: f32, c: f32) -> (f64, f64) {
    let (a, b, c) = (f64::from(a), f64::from(b), f64::from(c));
    let d = a - 2.0 * b + c;
    if d < 0.0 {
        let p = (0.5 * (a - c) / d).clamp(-0.5, 0.5);
        (p, b - 0.25 * (a - c) * p)
    } else {
        (0.0, b)
    }
}

/// Value, first and second derivative at fractional index `tau` of the band-limited (periodic sinc)
/// interpolation of the real length-`m` sequence whose half spectrum is `spec`, scaled so that
/// `c2r(spec)` is the sequence. Exact at integers; O(m).
fn band_limited(spec: &[C32], m: usize, tau: f64) -> (f64, f64, f64) {
    let half = m / 2;
    let w1 = std::f64::consts::TAU / m as f64;
    let (s1, c1) = (w1 * tau).sin_cos();
    let (mut zr, mut zi) = (1.0f64, 0.0f64); // e^{j w_k tau}, by rotation
    let mut v = spec.first().map_or(0.0, |s| f64::from(s.re));
    let (mut d1, mut d2) = (0.0f64, 0.0f64);
    for (k, s) in spec.iter().enumerate().take(half).skip(1) {
        (zr, zi) = (zr * c1 - zi * s1, zr * s1 + zi * c1);
        let (sr, si) = (f64::from(s.re), f64::from(s.im));
        let re = sr * zr - si * zi; // Re(S e^{jθ})
        let im = sr * zi + si * zr; // Im(S e^{jθ})
        let w = k as f64 * w1;
        v += 2.0 * re;
        d1 -= 2.0 * w * im;
        d2 -= 2.0 * w * w * re;
    }
    if let Some(ny) = spec.get(half) {
        let (s, c) = (std::f64::consts::PI * tau).sin_cos();
        let pi = std::f64::consts::PI;
        let r = f64::from(ny.re);
        v += r * c;
        d1 -= r * pi * s;
        d2 -= r * pi * pi * c;
    }
    (v, d1, d2)
}

/// Value, first and second derivative at `i + u` (|u| <= 1) of the band-limited interpolation of
/// the circular sequence `x`, from the samples within [`REFINE_HALF`] of `i`: the truncated sum of
/// `x[i + k] sinc(u - k)`. One `sin_cos` per call: `sin(pi (u - k)) = (-1)^k sin(pi u)`.
fn sinc_local(x: &[f32], i: usize, u: f64) -> (f64, f64, f64) {
    use std::f64::consts::PI;
    let m = x.len();
    if m == 0 {
        return (0.0, 0.0, 0.0);
    }
    let half = REFINE_HALF.min((m - 1) / 2);
    let (s, c) = (PI * u).sin_cos();
    let (mut v, mut d1, mut d2) = (0.0f64, 0.0f64, 0.0f64);
    // Start at i - half (mod m) and walk forwards, wrapping once.
    let mut j = (i % m + m - half) % m;
    for k in 0..=2 * half {
        let xk = f64::from(x[j]);
        j = if j + 1 == m { 0 } else { j + 1 };
        if xk == 0.0 {
            continue;
        }
        let k = k as f64 - half as f64;
        let y = PI * (u - k);
        let odd = (k as i64) & 1 == 1;
        let (sy, cy) = if odd { (-s, -c) } else { (s, c) };
        // sinc(y) = sin y / y and its derivatives in y; Taylor near 0, where the closed forms cancel.
        let (f, f1, f2) = if y.abs() < 1e-2 {
            let y2 = y * y;
            (1.0 - y2 / 6.0 + y2 * y2 / 120.0, y * (-1.0 / 3.0 + y2 / 30.0), -1.0 / 3.0 + y2 / 10.0 - y2 * y2 / 168.0)
        } else {
            let inv = 1.0 / y;
            let f = sy * inv;
            let f1 = (cy - f) * inv;
            (f, f1, -f - 2.0 * f1 * inv) // from y f'' + 2 f' + y f = 0
        };
        v += xk * f;
        d1 += xk * PI * f1;
        d2 += xk * PI * PI * f2;
    }
    (v, d1, d2)
}

/// Refines the peak of |x| at index `i` (x circular of length m, `spec` its half spectrum as for
/// [`band_limited`]): parabolic first, then Newton on the band-limited interpolation, staying within
/// one sample of `i`. Returns the fractional index and |x| there.
fn refine_peak(x: &[f32], spec: &[C32], i: usize) -> (f64, f32) {
    let m = x.len();
    newton_peak(x, i, |tau| band_limited(spec, m, tau))
}

/// [`refine_peak`] from the samples near `i` alone ([`sinc_local`]): for long sequences.
fn refine_peak_local(x: &[f32], i: usize) -> (f64, f32) {
    newton_peak(x, i, |tau| sinc_local(x, i, tau - i as f64))
}

/// Parabolic, then Newton steps on `interp(tau) = (value, d1, d2)`, within one sample of `i`.
fn newton_peak(x: &[f32], i: usize, mut interp: impl FnMut(f64) -> (f64, f64, f64)) -> (f64, f32) {
    let m = x.len();
    let at = |j: usize| x.get(j % m).copied().unwrap_or(0.0);
    let b = at(i);
    let sign = if b < 0.0 { -1.0f32 } else { 1.0 };
    let (p, height) = parabolic(sign * at(i + m - 1), sign * b, sign * at(i + 1));
    let base = i as f64;
    let mut tau = base + p;
    let mut best = (tau, height);
    let s = f64::from(sign);
    for _ in 0..NEWTON_STEPS {
        let (v, d1, d2) = interp(tau);
        let (v, d1, d2) = (s * v, s * d1, s * d2);
        if d2.is_nan() || d2 >= 0.0 {
            break; // not at a maximum of sign·x: keep the last good estimate
        }
        best = (tau, v);
        let step = -d1 / d2;
        let next = tau + step;
        let off = (next - base).abs();
        if off.is_nan() || off > 1.0 || step.abs() < 1e-7 {
            break;
        }
        tau = next;
    }
    (best.0, best.1 as f32)
}

/// 20 log10(peak / rms), with the RMS taken over `values`; +inf if the RMS is 0 or there are none.
fn confidence_db(peak: f32, values: impl Iterator<Item = f32>) -> f32 {
    let (mut sum, mut n) = (0.0f64, 0usize);
    for v in values {
        sum += f64::from(v) * f64::from(v);
        n += 1;
    }
    if n == 0 || sum <= 0.0 {
        return f32::INFINITY;
    }
    let rms = (sum / n as f64).sqrt();
    (20.0 * (f64::from(peak) / rms).log10()) as f32
}

/// Finds each sweep's arrival in `recording` (see the module contract).
pub fn find_sweep_arrivals(recording: &[f32], sweep: &Sweep, repetitions: usize, min_spacing: usize) -> Vec<Arrival> {
    let inv = &sweep.inverse;
    if recording.is_empty() || inv.is_empty() || repetitions == 0 {
        return Vec::new();
    }
    let len = recording.len() + inv.len() - 1;
    let mut fft = Fft::new(len);
    let m = fft.m;
    let (mut time, mut spec) = (fft.time_vec(), fft.spec_vec());
    {
        let mut filter = fft.spec_vec();
        if fft.forward(recording, &mut time, &mut spec).is_none() || fft.forward(inv, &mut time, &mut filter).is_none() {
            return Vec::new();
        }
        let scale = 1.0 / m as f32;
        for (s, f) in spec.iter_mut().zip(&filter) {
            *s = *s * *f * scale;
        }
    }
    if fft.inverse(&mut spec, &mut time).is_none() {
        return Vec::new();
    }
    drop(spec);
    let h = &time[..];

    // h[offset + p] is the response for a sweep that started at recording index p.
    let offset = inv.len() - 1;
    let (lo, hi) = (offset, offset + recording.len()); // valid indices
    let guard = (GUARD_MS * 1e-3 * f64::from(sweep.sample_rate)).round() as usize;
    let excl = min_spacing.max(guard + 1);
    let half = (min_spacing / 2).max(guard + 1);
    let thr_ratio = 10f32.powf(-FIRST_WITHIN_DB / 20.0);

    // |h| over the valid range; picked regions are zeroed so later picks keep `excl` away. A NaN
    // anywhere would win every pick and stop the search at once, as it did before the chunked
    // search: nothing is found.
    let mut mag: Vec<f32> = h[lo..hi].iter().map(|v| v.abs()).collect();
    if mag.iter().any(|v| v.is_nan()) {
        return Vec::new();
    }
    let mut maxima = ChunkMax::new(&mag);
    let mut arrivals = Vec::with_capacity(repetitions);
    for _ in 0..repetitions {
        let Some((k, best)) = maxima.argmax(&mag) else {
            break;
        };
        if best <= 0.0 {
            break;
        }
        let peak_i = lo + k;
        let (start, end) = (k.saturating_sub(excl - 1), (k + excl).min(mag.len()));
        mag[start..end].fill(0.0);
        maxima.rescan(&mag, start..end);

        let (w0, w1) = (peak_i.saturating_sub(half).max(lo), (peak_i + half + 1).min(hi));
        let thr = h[peak_i].abs() * thr_ratio;
        let mut j = (w0..=peak_i).find(|&j| h[j].abs() >= thr).unwrap_or(peak_i);
        while j + 1 < w1 && h[j + 1].abs() >= h[j].abs() {
            j += 1;
        }
        let (pos, peak) = refine_peak_local(h, j);
        let around = (w0..w1).filter(|&i| i.abs_diff(j) > guard).map(|i| h[i]);
        arrivals.push(Arrival { position: pos - offset as f64, peak, confidence_db: confidence_db(peak, around) });
    }
    arrivals.sort_by(|a, b| a.position.total_cmp(&b.position));
    arrivals
}

/// The maximum of each [`SEARCH_CHUNK`] samples of a non-negative, NaN-free sequence.
struct ChunkMax(Vec<f32>);

impl ChunkMax {
    fn new(x: &[f32]) -> Self {
        Self(x.chunks(SEARCH_CHUNK).map(chunk_max).collect())
    }

    /// The largest value and its index (the last, on ties); `None` if `x` is empty.
    fn argmax(&self, x: &[f32]) -> Option<(usize, f32)> {
        let (c, &best) = self.0.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        let from = c * SEARCH_CHUNK;
        let chunk = x.get(from..x.len().min(from + SEARCH_CHUNK))?;
        let k = chunk.iter().rposition(|&v| v == best)?;
        Some((from + k, best))
    }

    /// Recomputes the maxima of the chunks that `changed` touches.
    fn rescan(&mut self, x: &[f32], changed: std::ops::Range<usize>) {
        if changed.is_empty() {
            return;
        }
        for c in changed.start / SEARCH_CHUNK..=(changed.end - 1) / SEARCH_CHUNK {
            let from = c * SEARCH_CHUNK;
            if let (Some(m), Some(chunk)) = (self.0.get_mut(c), x.get(from..x.len().min(from + SEARCH_CHUNK))) {
                *m = chunk_max(chunk);
            }
        }
    }
}

fn chunk_max(c: &[f32]) -> f32 {
    c.iter().fold(0.0f32, |m, &v| if v > m { v } else { m })
}

/// GCC-PHAT-β delay of `b` relative to `a`.
pub fn gcc_phat(a: &[f32], b: &[f32], max_lag: usize, beta: f32) -> Option<(f64, f32)> {
    let beta = if beta.is_finite() { beta.clamp(0.0, 1.0) } else { 1.0 };
    correlate(a, b, max_lag, Some(beta))
}

/// Normalised cross-correlation delay of `b` relative to `a`.
pub fn xcorr_delay(a: &[f32], b: &[f32], max_lag: usize) -> Option<(f64, f32)> {
    correlate(a, b, max_lag, None)
}

/// The shared correlator: `c[τ] = Σ a[n] b[n+τ]`, weighted by `1 / |G|^β` (PHAT-β) or by the
/// inputs' energies (`beta = None`), searched over `-max_lag..=max_lag` (and the lags the lengths
/// allow).
fn correlate(a: &[f32], b: &[f32], max_lag: usize, beta: Option<f32>) -> Option<(f64, f32)> {
    let energy = |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    let (ea, eb) = (energy(a), energy(b));
    if !(ea > 0.0 && eb > 0.0) {
        return None; // empty, all zeros, or NaN
    }
    let mut fft = Fft::new(a.len() + b.len() - 1);
    let m = fft.m;
    let (mut time, mut sa, mut sb) = (fft.time_vec(), fft.spec_vec(), fft.spec_vec());
    fft.forward(a, &mut time, &mut sa)?;
    fft.forward(b, &mut time, &mut sb)?;
    for (x, y) in sa.iter().zip(sb.iter_mut()) {
        *y = x.conj() * *y; // G = conj(A) B: peaks at the lag by which b trails a
    }
    let inv_m = 1.0 / m as f64;
    match beta {
        Some(beta) => {
            let gmax2 = sb.iter().fold(0.0f32, |acc, g| acc.max(g.norm_sqr()));
            if gmax2.is_nan() || gmax2 <= 0.0 {
                return None;
            }
            let hb = 0.5 * f64::from(beta);
            let floor = 1e-6 * f64::from(gmax2).powf(hb); // keeps empty bins from blowing up
            for g in sb.iter_mut() {
                let mag2 = f64::from(g.norm_sqr());
                let w = if beta == 1.0 { mag2.sqrt() } else { mag2.powf(hb) };
                *g *= (inv_m / (w + floor)) as f32;
            }
        }
        None => {
            let k = (inv_m / (ea * eb).sqrt()) as f32;
            for g in sb.iter_mut() {
                *g *= k;
            }
        }
    }
    sa.copy_from_slice(&sb); // keep H for the refinement; c2r destroys `sb`
    fft.inverse(&mut sb, &mut time)?;

    let lo = -(max_lag.min(a.len() - 1) as isize);
    let hi = max_lag.min(b.len() - 1) as isize;
    let idx = |lag: isize| if lag >= 0 { lag as usize } else { (m as isize + lag) as usize };
    let mut best = (0isize, -1.0f32);
    for lag in lo..=hi {
        let v = time[idx(lag)].abs();
        if v > best.1 {
            best = (lag, v);
        }
    }
    let (lag, peak) = best;
    if peak.is_nan() || peak <= 0.0 {
        return None;
    }
    let i = idx(lag);
    let (pos, peak) = refine_peak(&time, &sa, i);
    let delay = lag as f64 + (pos - i as f64);

    let guard = (GUARD_MS * 1e-3 * f64::from(SAMPLE_RATE)).round() as usize;
    let outside = |l: &isize| l.abs_diff(lag) > guard;
    let conf = if (lo..=hi).any(|l| outside(&l)) {
        confidence_db(peak, (lo..=hi).filter(outside).map(|l| time[idx(l)]))
    } else {
        // The window is all guard: fall back to every lag of the (circular) correlation.
        let circular = |j: usize| j.abs_diff(i).min(m - j.abs_diff(i));
        confidence_db(peak, (0..m).filter(|&j| circular(j) > guard).map(|j| time[j]))
    };
    Some((delay, conf))
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;
    use crate::latency::sweep::{exp_sweep, probe_train};

    /// Deterministic noise source (xorshift64*), uniform in ±1.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
            (v >> 11) as f64 / (1u64 << 52) as f64 - 1.0
        }
        fn noise(&mut self, n: usize, rms: f64) -> Vec<f32> {
            let k = rms * 3f64.sqrt();
            (0..n).map(|_| (k * self.next()) as f32).collect()
        }
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// `x` delayed by `d` samples (fractional) through a Blackman-windowed sinc, `extra` samples
    /// longer. Linear phase: the kernel is symmetric about `d`.
    fn delayed(x: &[f32], d: f64, extra: usize) -> Vec<f32> {
        const K: i64 = 48;
        let (di, frac) = (d.floor() as i64, d - d.floor());
        let kernel: Vec<f64> = (-K..=K)
            .map(|m| {
                let u = m as f64 - frac;
                let sinc = if u == 0.0 { 1.0 } else { (PI * u).sin() / (PI * u) };
                let w = 0.42 + 0.5 * (PI * u / (K + 1) as f64).cos() + 0.08 * (2.0 * PI * u / (K + 1) as f64).cos();
                sinc * w
            })
            .collect();
        let n = x.len() + extra;
        (0..n as i64)
            .map(|t| {
                let mut acc = 0.0;
                for (kk, &h) in kernel.iter().enumerate() {
                    let src = t - di - (kk as i64 - K);
                    if src >= 0 && (src as usize) < x.len() {
                        acc += h * f64::from(x[src as usize]);
                    }
                }
                acc as f32
            })
            .collect()
    }

    fn add(a: &mut [f32], b: &[f32], gain: f32) {
        for (x, &y) in a.iter_mut().zip(b) {
            *x += gain * y;
        }
    }

    /// The default probe: 100 Hz-16 kHz, 1 s, 10 ms fades, -12 dBFS; two repetitions, 0.2 s gaps.
    fn probe() -> (Sweep, Vec<f32>, Vec<usize>) {
        let sweep = exp_sweep(48_000, 100.0, 16_000.0, 1.0, 0.01, 0.25);
        let (train, starts) = probe_train(&sweep, 2, 0.2);
        (sweep, train, starts)
    }

    #[test]
    fn identity_path_lands_on_the_sweep_starts() {
        let (sweep, train, starts) = probe();
        let spacing = sweep.signal.len() + 2 * 9600;
        let arr = find_sweep_arrivals(&train, &sweep, 2, spacing);
        assert_eq!(arr.len(), 2);
        for (a, &s) in arr.iter().zip(&starts) {
            assert!((a.position - s as f64).abs() < 1e-3, "{} vs {s}", a.position);
            assert!((a.peak - 1.0).abs() < 1e-3, "gain {}", a.peak);
            assert!(a.confidence_db > 40.0, "{}", a.confidence_db);
        }
    }

    #[test]
    fn fractional_delays_are_recovered_within_005_samples_at_40_db_snr() {
        let (sweep, train, starts) = probe();
        let spacing = sweep.signal.len() + 2 * 9600;
        let noise_rms = rms(&sweep.signal) * 0.01; // 40 dB below the sweep
        for (seed, d) in [0.0, 1.0, 480.0, 1234.37, 0.5, 77.81].into_iter().enumerate() {
            let mut rec = delayed(&train, d, 1500);
            let n = Rng(seed as u64 + 1).noise(rec.len(), noise_rms);
            add(&mut rec, &n, 1.0);
            let arr = find_sweep_arrivals(&rec, &sweep, 2, spacing);
            assert_eq!(arr.len(), 2);
            for (a, &s) in arr.iter().zip(&starts) {
                let err = a.position - (s as f64 + d);
                assert!(err.abs() < 0.05, "delay {d}: error {err}");
                assert!(a.confidence_db > 30.0, "delay {d}: confidence {}", a.confidence_db);
                assert!((a.peak - 1.0).abs() < 0.01, "delay {d}: gain {}", a.peak);
            }
        }
    }

    #[test]
    fn the_first_arrival_wins_over_a_later_echo() {
        let (sweep, train, starts) = probe();
        let spacing = sweep.signal.len() + 2 * 9600;
        let echo = 240.0; // 5 ms
        // (direct gain, echo gain, expected path): a -6 dB echo; an echo 3 dB louder than the
        // direct path (still within 6 dB, so the direct path is first); and one 10 dB louder,
        // where the direct path is too weak to count and the echo is the arrival.
        for (g_direct, g_echo, expect) in [(1.0f32, 0.5f32, 0.0), (0.7, 1.0, 0.0), (0.3, 1.0, echo)] {
            let d = 300.25;
            let mut rec = delayed(&train, d, 2000);
            for v in rec.iter_mut() {
                *v *= g_direct;
            }
            add(&mut rec, &delayed(&train, d + echo, 2000), g_echo);
            let n = Rng(9).noise(rec.len(), rms(&sweep.signal) * 0.01);
            add(&mut rec, &n, 1.0);
            let arr = find_sweep_arrivals(&rec, &sweep, 2, spacing);
            assert_eq!(arr.len(), 2);
            for (a, &s) in arr.iter().zip(&starts) {
                let err = a.position - (s as f64 + d + expect);
                assert!(err.abs() < 0.05, "direct {g_direct}, echo {g_echo}: error {err}");
            }
        }
    }

    #[test]
    fn a_missing_repetition_comes_back_with_low_confidence() {
        let (sweep, mut train, starts) = probe();
        let n = sweep.signal.len();
        train[starts[1]..starts[1] + n].fill(0.0); // the second sweep never arrived
        let noise = Rng(3).noise(train.len(), rms(&sweep.signal) * 0.01);
        add(&mut train, &noise, 1.0);
        let arr = find_sweep_arrivals(&train, &sweep, 2, n + 2 * 9600);
        assert_eq!(arr.len(), 2);
        let (good, bad): (Vec<&Arrival>, Vec<&Arrival>) = arr.iter().partition(|a| a.confidence_db >= 15.0);
        assert_eq!(good.len(), 1);
        assert!((good[0].position - starts[0] as f64).abs() < 0.05);
        assert!(bad[0].confidence_db < 15.0, "{}", bad[0].confidence_db);
    }

    #[test]
    fn degenerate_inputs_give_nothing() {
        let (sweep, train, _) = probe();
        assert!(find_sweep_arrivals(&[], &sweep, 2, 1000).is_empty());
        assert!(find_sweep_arrivals(&train, &sweep, 0, 1000).is_empty());
        assert!(find_sweep_arrivals(&vec![0.0; 1000], &sweep, 2, 1000).is_empty());
        assert_eq!(gcc_phat(&[0.0; 64], &[1.0; 64], 10, 0.8), None);
        assert_eq!(gcc_phat(&[1.0; 64], &[], 10, 0.8), None);
        assert_eq!(xcorr_delay(&[], &[1.0; 64], 10), None);
        assert_eq!(xcorr_delay(&[1.0; 64], &[0.0; 64], 10), None);
    }

    #[test]
    fn convolution_matches_the_direct_sum() {
        let a = [1.0f32, -2.0, 0.5, 3.0];
        let b = [0.25f32, 1.0, -1.0];
        let c = fft_convolve(&a, &b);
        assert_eq!(c.len(), 6);
        for (n, &v) in c.iter().enumerate() {
            let direct: f32 = (0..a.len()).filter(|&k| n >= k && n - k < b.len()).map(|k| a[k] * b[n - k]).sum();
            assert!((v - direct).abs() < 1e-5, "{n}: {v} vs {direct}");
        }
        assert!(fft_convolve(&[], &b).is_empty());
    }

    #[test]
    fn band_limited_interpolation_is_exact_at_integers() {
        let x: Vec<f32> = (0..64).map(|i| ((i * 13) % 7) as f32 - 3.0).collect();
        let mut fft = Fft::new(64);
        let (mut t, mut s) = (fft.time_vec(), fft.spec_vec());
        fft.forward(&x, &mut t, &mut s).unwrap();
        let s: Vec<C32> = s.iter().map(|v| *v * (1.0 / 64.0)).collect();
        for i in [0usize, 1, 17, 63] {
            let (v, _, _) = band_limited(&s, 64, i as f64);
            assert!((v - f64::from(x[i])).abs() < 1e-4, "{i}: {v}");
        }
        // A pure tone's derivative is known exactly.
        let w = 2.0 * PI * 5.0 / 64.0;
        let tone: Vec<f32> = (0..64).map(|i| (w * i as f64).cos() as f32).collect();
        let (mut s2, mut t2) = (fft.spec_vec(), fft.time_vec());
        fft.forward(&tone, &mut t2, &mut s2).unwrap();
        let s2: Vec<C32> = s2.iter().map(|v| *v * (1.0 / 64.0)).collect();
        let (v, d1, d2) = band_limited(&s2, 64, 2.3);
        assert!((v - (w * 2.3).cos()).abs() < 1e-5);
        assert!((d1 + w * (w * 2.3).sin()).abs() < 1e-5);
        assert!((d2 + w * w * (w * 2.3).cos()).abs() < 1e-5);
    }

    #[test]
    fn gcc_phat_and_xcorr_recover_broadband_delays() {
        let n = 24_000;
        let src = Rng(42).noise(n + 3000, 0.1);
        for (seed, d) in [0.0, 1.0, 480.0, 1234.37, 37.5, 2.25].into_iter().enumerate() {
            let a: Vec<f32> = src[..n].to_vec();
            let b_full = delayed(&src, d, 0);
            let mut b = b_full[..n].to_vec();
            let mut a_noisy = a.clone();
            add(&mut b, &Rng(100 + seed as u64).noise(n, 0.001), 1.0); // 40 dB SNR
            add(&mut a_noisy, &Rng(200 + seed as u64).noise(n, 0.001), 1.0);
            for beta in [1.0f32, 0.75, 0.0] {
                let (lag, conf) = gcc_phat(&a_noisy, &b, 2400, beta).expect("found");
                assert!((lag - d).abs() < 0.05, "gcc β {beta}, delay {d}: {lag}");
                assert!(conf > 20.0, "gcc β {beta}, delay {d}: confidence {conf}");
            }
            let (lag, conf) = xcorr_delay(&a_noisy, &b, 2400).expect("found");
            assert!((lag - d).abs() < 0.05, "xcorr, delay {d}: {lag}");
            assert!(conf > 20.0, "xcorr, delay {d}: confidence {conf}");
        }
    }

    #[test]
    fn negative_lags_and_the_lag_limit() {
        let n = 8192;
        let src = Rng(7).noise(n + 600, 0.1);
        // b leads a by 100.5: a = b delayed.
        let a = delayed(&src, 100.5, 0)[..n].to_vec();
        let b = src[..n].to_vec();
        let (lag, _) = gcc_phat(&a, &b, 400, 0.8).unwrap();
        assert!((lag + 100.5).abs() < 0.05, "{lag}");
        let (lag, _) = xcorr_delay(&a, &b, 400).unwrap();
        assert!((lag + 100.5).abs() < 0.05, "{lag}");
        // With the true lag outside ±max_lag, the answer stays inside it, with low confidence.
        let (lag, conf) = gcc_phat(&a, &b, 50, 0.8).unwrap();
        assert!(lag.abs() <= 51.0, "{lag}");
        assert!(conf < 15.0, "{conf}");
    }

    /// A vowel-like source: f0 = 120 Hz with 5 Hz, ±2% vibrato, harmonics to 5 kHz weighted by /a/
    /// formants (730, 1090, 2440 Hz), a syllable-rate envelope, and a little aspiration noise.
    fn vowel(n: usize, seed: u64) -> Vec<f32> {
        let fs = 48_000.0;
        let formants = [(730.0, 90.0, 1.0), (1090.0, 110.0, 0.5), (2440.0, 170.0, 0.25)];
        let mut rng = Rng(seed);
        let phases: Vec<f64> = (0..42).map(|_| PI * rng.next()).collect();
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / fs;
            let f0 = 120.0 * (1.0 + 0.02 * (2.0 * PI * 5.0 * t).sin());
            phase += 2.0 * PI * f0 / fs;
            let mut v = 0.0;
            for (k, ph) in phases.iter().enumerate() {
                let h = (k + 1) as f64;
                let f = h * f0;
                if f > 5000.0 {
                    break;
                }
                let amp: f64 = formants.iter().map(|&(fc, bw, g)| g / (1.0 + ((f - fc) / bw).powi(2))).sum::<f64>() + 0.02;
                v += amp * (h * phase + ph).sin();
            }
            let env = 0.6 + 0.4 * (2.0 * PI * 3.0 * t).sin();
            out.push((0.05 * env * v + 0.0005 * rng.next()) as f32);
        }
        out
    }

    #[test]
    fn gcc_phat_recovers_the_delay_of_a_vowel() {
        let n = 48_000;
        let src = vowel(n + 3000, 5);
        let sig_rms = rms(&src);
        for (seed, d) in [480.0, 1234.37, 3.6].into_iter().enumerate() {
            let a = src[..n].to_vec();
            let mut b = delayed(&src, d, 0)[..n].to_vec();
            let mut a = a;
            add(&mut a, &Rng(300 + seed as u64).noise(n, sig_rms * 0.01), 1.0);
            add(&mut b, &Rng(400 + seed as u64).noise(n, sig_rms * 0.01), 1.0);
            let (lag, conf) = gcc_phat(&a, &b, 2400, 0.75).expect("found");
            // Not a pitch period (400 samples) off, and sub-sample accurate.
            assert!((lag - d).abs() < 0.05, "delay {d}: {lag} (confidence {conf})");
            assert!(conf > 15.0, "delay {d}: confidence {conf}");
        }
    }

    // ---- Long recordings (review RT-2) ------------------------------------------------------

    #[test]
    fn the_chunked_search_picks_what_a_full_scan_picks() {
        // Random levels with ties and long zero runs, picked strongest first with exclusions, against
        // the full scan per pick it replaced (`max_by` keeps the last of equal maxima).
        let mut rng = Rng(77);
        for (n, excl) in [(50_000usize, 3000usize), (SEARCH_CHUNK * 3, 1), (100, 7), (SEARCH_CHUNK + 1, 9000)] {
            let mut x: Vec<f32> = (0..n).map(|_| ((rng.next().abs() * 40.0) as i32) as f32).collect();
            x[n / 3..n / 2].fill(0.0);
            let mut naive = x.clone();
            let mut fast = x.clone();
            let mut maxima = ChunkMax::new(&fast);
            for pick in 0..30 {
                let expect = naive.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(k, &v)| (k, v));
                let got = maxima.argmax(&fast);
                assert_eq!(got, expect, "n {n}, pick {pick}");
                let Some((k, best)) = got else { break };
                if best <= 0.0 {
                    break;
                }
                let (a, b) = (k.saturating_sub(excl - 1), (k + excl).min(n));
                naive[a..b].fill(0.0);
                fast[a..b].fill(0.0);
                maxima.rescan(&fast, a..b);
            }
        }
        assert_eq!(ChunkMax::new(&[]).argmax(&[]), None);
    }

    /// The deconvolved response of `rec` and its half spectrum, as `find_sweep_arrivals` builds it
    /// (and as it used to keep it for the spectrum refinement).
    fn deconvolved(rec: &[f32], sweep: &Sweep) -> (Vec<f32>, Vec<C32>) {
        let inv = &sweep.inverse;
        let mut fft = Fft::new(rec.len() + inv.len() - 1);
        let (mut time, mut spec, mut filter) = (fft.time_vec(), fft.spec_vec(), fft.spec_vec());
        fft.forward(rec, &mut time, &mut spec).unwrap();
        fft.forward(inv, &mut time, &mut filter).unwrap();
        let scale = 1.0 / fft.m as f32;
        for (s, f) in spec.iter_mut().zip(&filter) {
            *s = *s * *f * scale;
        }
        let kept = spec.clone();
        fft.inverse(&mut spec, &mut time).unwrap();
        (time, kept)
    }

    #[test]
    fn local_refinement_matches_the_spectrum_method() {
        let (sweep, train, starts) = probe();
        let noise_rms = rms(&sweep.signal) * 0.01; // 40 dB SNR, as in the accuracy test
        let mut worst = 0.0f64;
        for (seed, d) in [0.0, 0.5, 77.81, 1234.37].into_iter().enumerate() {
            let mut rec = delayed(&train, d, 1500);
            let n = Rng(seed as u64 + 50).noise(rec.len(), noise_rms);
            add(&mut rec, &n, 1.0);
            let (h, spec) = deconvolved(&rec, &sweep);
            let offset = sweep.inverse.len() - 1;
            for &s in &starts {
                let i = offset + s + d.round() as usize;
                let (exact, pe) = refine_peak(&h, &spec, i);
                let (local, pl) = refine_peak_local(&h, i);
                worst = worst.max((exact - local).abs());
                assert!((exact - local).abs() < 2e-3, "delay {d}: spectrum {exact}, local {local}");
                assert!((pe - pl).abs() < 1e-3, "delay {d}: heights {pe} vs {pl}");
            }
        }
        eprintln!("local vs spectrum refinement: worst difference {worst:.2e} samples");
        // The sinc sum itself, on a burst that dies out well inside the window: value and both
        // derivatives match the exact (spectrum) interpolation, at integers and between them.
        let m = 8192;
        let burst: Vec<f32> = (0..m)
            .map(|n| {
                let t = n as f64 - 4000.3;
                ((-(t / 150.0).powi(2)).exp() * (0.9 * t).cos()) as f32
            })
            .collect();
        let mut fft = Fft::new(m);
        let (mut t, mut sp) = (fft.time_vec(), fft.spec_vec());
        fft.forward(&burst, &mut t, &mut sp).unwrap();
        let sp: Vec<C32> = sp.iter().map(|v| *v * (1.0 / m as f32)).collect();
        for (i, u) in [(4000usize, 0.0), (4000, 0.37), (4001, -0.81), (3990, 0.5)] {
            let (v, d1, d2) = sinc_local(&burst, i, u);
            let (ve, d1e, d2e) = band_limited(&sp, m, i as f64 + u);
            assert!(
                (v - ve).abs() < 1e-5 && (d1 - d1e).abs() < 1e-5 && (d2 - d2e).abs() < 1e-5,
                "at {i}{u:+}: {v} {d1} {d2} vs {ve} {d1e} {d2e}"
            );
        }
        assert_eq!(sinc_local(&burst, 4000, 0.0).0, f64::from(burst[4000]), "exact at an integer");
    }

    #[test]
    fn a_hundred_short_repetitions_are_all_found() {
        // 100 repetitions of a 0.1 s sweep, 50 ms gaps: the chunked search through every pick.
        let sweep = exp_sweep(48_000, 200.0, 16_000.0, 0.1, 0.005, 0.25);
        let (train, starts) = probe_train(&sweep, 100, 0.05);
        let d = 333;
        let mut rec = vec![0.0f32; d];
        rec.extend_from_slice(&train);
        let n = Rng(11).noise(rec.len(), rms(&sweep.signal) * 0.01);
        add(&mut rec, &n, 1.0);
        let arr = find_sweep_arrivals(&rec, &sweep, 100, sweep.signal.len());
        assert_eq!(arr.len(), 100);
        for (a, &s) in arr.iter().zip(&starts) {
            assert!((a.position - (s + d) as f64).abs() < 0.05, "{} vs {}", a.position, s + d);
            assert!(a.confidence_db > 25.0, "{}", a.confidence_db);
        }
    }

    #[test]
    #[ignore = "timing: run with --release"]
    fn a_hundred_repetitions_of_the_real_probe_finish_quickly() {
        // The latency test's maximum: 100 repetitions of the 1 s probe with 0.5 s gaps, 200 s
        // (9.6 M samples), at the engine's min_spacing. `cargo test -p nf-core --release --lib --
        // --ignored a_hundred_repetitions --nocapture`.
        let sweep = exp_sweep(48_000, 100.0, 16_000.0, 1.0, 0.01, 0.25);
        let (train, starts) = probe_train(&sweep, 100, 0.5);
        let d = 1234;
        let mut rec = vec![0.0f32; d];
        rec.extend_from_slice(&train);
        let n = Rng(12).noise(rec.len(), rms(&sweep.signal) * 0.01);
        add(&mut rec, &n, 1.0);
        let t = std::time::Instant::now();
        let arr = find_sweep_arrivals(&rec, &sweep, 100, sweep.signal.len());
        let took = t.elapsed();
        eprintln!("100 repetitions, {} samples: {took:?}", rec.len());
        assert_eq!(arr.len(), 100);
        for (a, &s) in arr.iter().zip(&starts) {
            assert!((a.position - (s + d) as f64).abs() < 0.05, "{} vs {}", a.position, s + d);
            assert!(a.confidence_db > 30.0, "{}", a.confidence_db);
        }
        assert!(took.as_secs_f64() < 10.0, "took {took:?}");

        // For the record: one refinement the old way (a pass over the whole half spectrum per
        // Newton step), against the local one.
        let (h, spec) = deconvolved(&rec, &sweep);
        let i = sweep.inverse.len() - 1 + starts[50] + d;
        let t = std::time::Instant::now();
        let (exact, _) = refine_peak(&h, &spec, i);
        let spectrum = t.elapsed();
        let t = std::time::Instant::now();
        let (local, _) = refine_peak_local(&h, i);
        let near = t.elapsed();
        eprintln!(
            "one peak refined: spectrum {spectrum:?} (x100 repetitions), local {near:?}; positions differ by {:.1e}",
            (exact - local).abs()
        );
    }
}
