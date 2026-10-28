//! Zero-latency clock-drift correction: resample one block by ±1 sample when the drift controller
//! asks for it (TODO #9).
//!
//! Contract:
//! - `resample(input, out)`: `input.len()` and `out.len()` are each in `BLOCK-1 ..= BLOCK+1`.
//!   - When they're equal, copy (bit-exact).
//!   - Otherwise map output index `j` in `0..m` to input position `p = j * (n-1) / (m-1)`, so the
//!     first and last samples land exactly on input 0 and n-1 and consecutive blocks stay continuous.
//!   - Interpolate with 4-point cubic Hermite (Catmull-Rom). The left neighbour of input 0 comes
//!     from the previous call (keep the last two input samples); the right neighbour of input n-1
//!     is linearly extrapolated.
//! - Real-time: no allocation.
//! - Tests:
//!   - equal lengths are bit-exact;
//!   - a 1 kHz sine slipped once is continuous at both edges (no step > 2x the normal sample
//!     difference);
//!   - over 100 blocks with drops every 10, total output = 100*480 - 10;
//!   - THD of a 1 kHz sine in a slipped block stays below -60 dB.
//!
//! Catmull-Rom only needs one sample of history (input -1), so that is all that's kept. The
//! position `p` is computed in exact integer arithmetic, so the last output is input n-1 exactly.
//! Before the first call (and after `reset`) the left neighbour is linearly extrapolated too. Lengths outside the contract are handled without panicking (debug builds
//! assert): fewer than two samples on either side, or an input longer than `BLOCK + 1`, fall back
//! to nearest-sample mapping.

use crate::BLOCK;

/// What the drift controller asks for this block: the stream gains or loses one sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Slip {
    #[default]
    None,
    /// The stream loses one sample: a producer writes `BLOCK - 1`, or a consumer reads `BLOCK + 1`.
    Drop,
    /// The stream gains one sample: a producer writes `BLOCK + 1`, or a consumer reads `BLOCK - 1`.
    Insert,
}

impl Slip {
    /// Output length when producing from one `BLOCK` input (output side: app → cable).
    pub fn produced_len(self) -> usize {
        match self {
            Slip::None => BLOCK,
            Slip::Drop => BLOCK - 1,
            Slip::Insert => BLOCK + 1,
        }
    }
    /// Input length to consume to make one `BLOCK` output (reference side: loopback → app).
    pub fn consumed_len(self) -> usize {
        match self {
            Slip::None => BLOCK,
            Slip::Drop => BLOCK + 1,
            Slip::Insert => BLOCK - 1,
        }
    }
}

/// The longest input the cubic path takes.
const MAX_IN: usize = BLOCK + 1;
/// The padded input: one sample of history, the input, two extrapolated samples.
const EXT: usize = MAX_IN + 3;

/// Resampler state: the history for cubic interpolation across block edges.
#[derive(Clone, Debug)]
pub struct SlipResampler {
    /// The last input sample of the previous call: the left neighbour of this call's input 0.
    prev: f32,
    /// Whether `prev` holds a real sample.
    primed: bool,
}

impl Default for SlipResampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Catmull-Rom between `p1` (t = 0) and `p2` (t = 1). Exactly `p1` at t = 0.
#[inline]
fn catmull_rom([p0, p1, p2, p3]: [f32; 4], t: f32) -> f32 {
    let a = p2 - p0;
    let b = 2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3;
    let c = 3.0 * (p1 - p2) + p3 - p0;
    p1 + 0.5 * t * (a + t * (b + t * c))
}

impl SlipResampler {
    pub fn new() -> Self {
        Self { prev: 0.0, primed: false }
    }

    /// Resamples `input` (n samples) into `out` (m samples); see the module contract.
    pub fn resample(&mut self, input: &[f32], out: &mut [f32]) {
        let (n, m) = (input.len(), out.len());
        debug_assert!((BLOCK - 1..=BLOCK + 1).contains(&n), "input length {n}");
        debug_assert!((BLOCK - 1..=BLOCK + 1).contains(&m), "output length {m}");
        if n == m {
            out.copy_from_slice(input);
        } else if (2..=MAX_IN).contains(&n) && m >= 2 {
            self.cubic(input, out);
        } else {
            nearest(input, out);
        }
        if let Some(&last) = input.last() {
            self.prev = last;
            self.primed = true;
        }
    }

    pub fn reset(&mut self) {
        self.prev = 0.0;
        self.primed = false;
    }

    /// The cubic path: `2 <= n <= MAX_IN`, `m >= 2`, `n != m`.
    ///
    /// With `den = m - 1` and `δ = n - m`, `p(j) = j (n-1) / den = j + q + t` where
    /// `q = floor(j δ / den)` and `t = (j δ - q den) / den`. `q` changes at most `|δ|` times (once
    /// or twice in contract), so the block splits into runs of constant `q`. In each run the four
    /// taps are contiguous slices of the padded input and `t` comes from the index, so the loop
    /// vectorises; the integer forms keep `p` exact, so the last output is input n-1 exactly.
    fn cubic(&self, input: &[f32], out: &mut [f32]) {
        let n = input.len();
        let (&[x0, x1, ..], &[.., b, a]) = (input, input) else { return };
        // ext[k + 1] = x[k] for k in -1 ..= n + 1.
        let mut ext = [0.0f32; EXT];
        let Some(body) = ext.get_mut(1..=n) else { return };
        body.copy_from_slice(input);
        ext[0] = if self.primed { self.prev } else { 2.0 * x0 - x1 };
        if let Some([e1, e2]) = ext.get_mut(n + 1..=n + 2) {
            *e1 = 2.0 * a - b; // linear extrapolation past x[n-1]
            *e2 = 3.0 * a - 2.0 * b;
        }

        let m = out.len();
        let den = (m - 1) as i64;
        let delta = n as i64 - m as i64;
        let inv_den = 1.0 / den as f32;
        // Every division and slice below is checked (`checked_*`, `get`) rather than panicking;
        // none of the fallbacks can trigger for the lengths `resample` lets through.
        let mut j0 = 0usize;
        while j0 < m {
            let Some(q) = (j0 as i64 * delta).checked_div_euclid(den) else { break };
            // The first j past j0 whose floor(j δ / den) differs from q (always > j0).
            let next = if delta > 0 { ((q + 1) * den + delta - 1).checked_div(delta) } else { (q * den).checked_div(delta).map(|v| v + 1) };
            let Some(next) = next else { break };
            let j1 = (next.max(j0 as i64 + 1) as usize).min(m);
            let (base, len) = ((j0 as i64 + q) as usize, j1 - j0); // ext[base] = x[j0 + q - 1]
            let tap = |k: usize| ext.get(base + k..base + k + len);
            let (Some(o), Some(w0), Some(w1), Some(w2), Some(w3)) = (out.get_mut(j0..j1), tap(0), tap(1), tap(2), tap(3)) else {
                break;
            };
            let t0 = (j0 as i64 * delta - q * den) as f32; // exact: |values| < 2^24
            let dt = delta as f32;
            let taps = w0.iter().zip(w1).zip(w2).zip(w3);
            for (k, (y, (((&p0, &p1), &p2), &p3))) in o.iter_mut().zip(taps).enumerate() {
                // `k as i32`: a packed int -> float conversion (usize -> f32 is scalar below AVX-512).
                *y = catmull_rom([p0, p1, p2, p3], (t0 + k as i32 as f32 * dt) * inv_den);
            }
            j0 = j1;
        }
    }
}

/// Out-of-contract fallback: nearest input sample for each output (silence for an empty input).
#[cold]
fn nearest(input: &[f32], out: &mut [f32]) {
    let (n, m) = (input.len(), out.len());
    for (j, o) in out.iter_mut().enumerate() {
        let i = if m > 1 { (j * n.saturating_sub(1) + (m - 1) / 2) / (m - 1) } else { 0 };
        *o = input.get(i).copied().unwrap_or(0.0);
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    fn sine(start: usize, len: usize, f: f64) -> Vec<f32> {
        (start..start + len).map(|k| (0.8 * (2.0 * PI * f * k as f64 / 48_000.0).sin()) as f32).collect()
    }

    #[test]
    fn equal_lengths_are_bit_exact() {
        let mut s = SlipResampler::new();
        for n in [BLOCK - 1, BLOCK, BLOCK + 1] {
            let x: Vec<f32> = (0..n).map(|i| ((i * 7919) % 1000) as f32 * 1e-3 - 0.5).collect();
            let mut y = vec![0.0f32; n];
            s.resample(&x, &mut y);
            assert_eq!(x.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), y.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn ends_land_exactly_and_a_ramp_is_reproduced() {
        // Catmull-Rom (and the linear extrapolation) reproduce a straight line exactly, so the output
        // is the ramp sampled at p = j (n-1) / (m-1).
        for (n, m) in [(481, 480), (479, 480), (480, 479), (480, 481), (479, 481), (481, 479)] {
            let mut s = SlipResampler::new();
            let before: Vec<f32> = (0..BLOCK).map(|k| (k as f32 - BLOCK as f32) * 0.001).collect();
            s.resample(&before, &mut [0.0; BLOCK]); // history continues the ramp
            let x: Vec<f32> = (0..n).map(|k| k as f32 * 0.001).collect();
            let mut y = vec![0.0f32; m];
            s.resample(&x, &mut y);
            assert_eq!(y[0], x[0]);
            assert_eq!(y[m - 1], x[n - 1]);
            for (j, v) in y.iter().enumerate() {
                let p = j as f64 * (n - 1) as f64 / (m - 1) as f64;
                assert!((*v as f64 - p * 0.001).abs() < 2e-6, "({n}, {m}) j {j}: {v} vs {}", p * 0.001);
            }
        }
    }

    /// Feeds a continuous 1 kHz sine through blocks with the given input/output lengths and returns
    /// the output stream.
    fn stream(lengths: &[(usize, usize)]) -> Vec<f32> {
        let mut s = SlipResampler::new();
        let mut pos = 0;
        let mut out = Vec::new();
        for &(n, m) in lengths {
            let x = sine(pos, n, 1000.0);
            pos += n;
            let mut y = vec![0.0f32; m];
            s.resample(&x, &mut y);
            out.extend_from_slice(&y);
        }
        out
    }

    #[test]
    fn a_slipped_sine_is_continuous_at_both_edges() {
        let normal = 0.8 * (2.0 * PI * 1000.0 / 48_000.0) as f32; // max |x[k+1] - x[k]|
        for slip in [(481, 480), (479, 480), (480, 479), (480, 481)] {
            let y = stream(&[(480, 480), (480, 480), slip, (480, 480), (480, 480)]);
            let edges = [2 * BLOCK, 2 * BLOCK + slip.1];
            for (k, w) in y.windows(2).enumerate() {
                let step = (w[1] - w[0]).abs();
                assert!(step <= 2.0 * normal, "{slip:?}: step {step} at {k}");
            }
            for e in edges {
                let step = (y[e] - y[e - 1]).abs();
                assert!(step <= normal * 1.01, "{slip:?}: edge step {step} at {e}");
            }
        }
    }

    #[test]
    fn drops_every_ten_blocks_remove_ten_samples() {
        let mut s = SlipResampler::new();
        let input = sine(0, 100 * BLOCK, 1000.0);
        let mut total = 0;
        let mut out = Vec::new();
        for (blk, x) in input.as_chunks::<BLOCK>().0.iter().enumerate() {
            let slip = if blk % 10 == 9 { Slip::Drop } else { Slip::None };
            let mut y = vec![0.0f32; slip.produced_len()];
            s.resample(x, &mut y);
            total += y.len();
            out.extend_from_slice(&y);
        }
        assert_eq!(total, 100 * BLOCK - 10);
        let normal = 0.8 * (2.0 * PI * 1000.0 / 48_000.0) as f32;
        assert!(out.windows(2).all(|w| (w[1] - w[0]).abs() <= 2.0 * normal));

        // The reference side: consuming 481 on a drop reads ten more samples for the same output.
        let mut s = SlipResampler::new();
        let input = sine(0, 100 * BLOCK + 10, 1000.0);
        let (mut pos, mut produced) = (0, 0);
        for blk in 0..100 {
            let slip = if blk % 10 == 9 { Slip::Drop } else { Slip::None };
            let n = slip.consumed_len();
            let mut y = [0.0f32; BLOCK];
            s.resample(&input[pos..pos + n], &mut y);
            pos += n;
            produced += y.len();
        }
        assert_eq!((pos, produced), (100 * BLOCK + 10, 100 * BLOCK));
    }

    /// Residual of `y` after a least-squares fit of a sine at angular frequency `w` (per sample),
    /// relative to the fitted sine's energy, in dB.
    fn thd_db(y: &[f32], w: f64) -> f64 {
        let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (j, &v) in y.iter().enumerate() {
            let (s, c) = (w * j as f64).sin_cos();
            ss += s * s;
            sc += s * c;
            cc += c * c;
            ys += v as f64 * s;
            yc += v as f64 * c;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((ys * cc - yc * sc) / det, (yc * ss - ys * sc) / det);
        let (mut sig, mut res) = (0.0, 0.0);
        for (j, &v) in y.iter().enumerate() {
            let (s, c) = (w * j as f64).sin_cos();
            let fit = a * s + b * c;
            sig += fit * fit;
            res += (v as f64 - fit).powi(2);
        }
        10.0 * (res / sig).log10()
    }

    #[test]
    fn thd_of_a_slipped_1khz_sine_is_below_minus_60_db() {
        let w = 2.0 * PI * 1000.0 / 48_000.0;
        for (n, m) in [(481, 480), (479, 480), (480, 479), (480, 481)] {
            let mut s = SlipResampler::new();
            let prev = sine(0, BLOCK, 1000.0);
            s.resample(&prev, &mut vec![0.0; BLOCK]);
            let x = sine(BLOCK, n, 1000.0);
            let mut y = vec![0.0f32; m];
            s.resample(&x, &mut y);
            // In output samples the sine runs at w (n-1)/(m-1).
            let thd = thd_db(&y, w * (n - 1) as f64 / (m - 1) as f64);
            assert!(thd < -60.0, "({n}, {m}): {thd:.1} dB");
        }
    }

    #[test]
    fn out_of_contract_fallback_is_safe() {
        // The fallback itself (called directly: debug builds assert in `resample`).
        let mut y = [9.0f32; 4];
        nearest(&[], &mut y);
        assert_eq!(y, [0.0; 4]);
        nearest(&[1.0, 2.0, 3.0], &mut y);
        assert_eq!(y, [1.0, 2.0, 2.0, 3.0]);
        let mut one = [0.0f32; 1];
        nearest(&[5.0, 6.0], &mut one);
        assert_eq!(one, [5.0]);
    }

    /// The direct form: each output's position stepped one at a time.
    fn reference(prev: Option<f32>, x: &[f32], m: usize) -> Vec<f32> {
        let n = x.len();
        let at = |k: isize| -> f32 {
            if k < 0 {
                prev.unwrap_or(2.0 * x[0] - x[1])
            } else if (k as usize) < n {
                x[k as usize]
            } else {
                let over = (k as usize - (n - 1)) as f32;
                x[n - 1] + over * (x[n - 1] - x[n - 2])
            }
        };
        (0..m)
            .map(|j| {
                let num = j * (n - 1);
                let (i, r) = ((num / (m - 1)) as isize, num % (m - 1));
                catmull_rom([at(i - 1), at(i), at(i + 1), at(i + 2)], r as f32 / (m - 1) as f32)
            })
            .collect()
    }

    #[test]
    fn runs_match_the_direct_per_sample_form() {
        let mut lengths = vec![(481, 480), (479, 480), (480, 479), (480, 481), (479, 481), (481, 479)];
        lengths.extend([(2, 481), (481, 2), (3, 7), (300, 470), (481, 120), (5, 5 * 96)]); // outside the contract
        for (n, m) in lengths {
            let x: Vec<f32> = (0..n).map(|k| ((k * 7919) % 613) as f32 / 613.0 - 0.5).collect();
            for primed in [false, true] {
                let s = SlipResampler { prev: 0.3, primed };
                let mut y = vec![0.0f32; m];
                s.cubic(&x, &mut y);
                let r = reference(primed.then_some(0.3), &x, m);
                for (j, (a, b)) in y.iter().zip(&r).enumerate() {
                    assert!((a - b).abs() < 1e-5, "({n}, {m}) j {j}: {a} vs {b}");
                }
                assert_eq!(y[m - 1], x[n - 1], "({n}, {m}) ends exactly on the last input");
            }
        }
    }

    #[test]
    fn reset_forgets_the_history() {
        let mut a = SlipResampler::new();
        a.resample(&[1.0; BLOCK], &mut [0.0; BLOCK]);
        a.reset();
        let mut b = SlipResampler::new();
        let x = sine(0, BLOCK - 1, 1000.0);
        let (mut ya, mut yb) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        a.resample(&x, &mut ya);
        b.resample(&x, &mut yb);
        assert_eq!(ya, yb);
    }
}
