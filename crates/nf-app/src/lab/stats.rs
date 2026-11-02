//! The Lab's statistics (TODO 14, "Statistics"): means over clips, and paired differences against a baseline with a 95%
//! confidence interval by Student's t, cluster-robust by speaker and noise file.
//!
//! **Why clusters.** The clips aren't independent: 357 of them are 17 speech segments from 10 speakers crossed with 7 noise
//! files at 3 SNRs, so a model that does well on one speaker's voice or one noise file does so on all that speaker's or that
//! noise's clips. A t-interval over the clips counts each of them as new evidence and comes out too narrow (by half or more on
//! the first run's noise-in-pauses difference). So each interval is the mean's cluster-robust one (the "CR1" sandwich
//! estimator; for a mean, with equal clusters, exactly the t-interval over the cluster means): by speaker (a speech take: a
//! VCTK take's segments share its speaker and its room) where one noise is in the table, and two-way by speaker and noise file
//! where the table pools noises (Cameron, Gelbach and Miller 2011: the speaker and noise variances added, their cells' taken
//! off), never narrower than either one-way clustering, with the fewer clusters less one as its degrees of freedom (Cameron and
//! Miller 2015's advice). The point estimate stays the mean over the clips. With 10 speakers and 7 noises the intervals are
//! wide, and only approximate with this few clusters (in the tests' simulation of the Lab's layout they held the true mean 94.7%
//! of the time, the t over clips 27%); more speakers will narrow them.
//!
//! **Why t, not a bootstrap.** The quantity is a mean of paired differences. The t-interval is exact for normal differences and
//! close to its nominal coverage for the bounded, mildly skewed MOS and dB differences by the central limit theorem. The
//! percentile bootstrap needs no normality but under-covers at the small end: its interval is about `sqrt((n - 1) / n)` times
//! the normal's quantile over t's as wide as the t's (at n = 10, 0.949 x 1.960 / 2.262: 18% too narrow), and with 7 to 10
//! clusters the small end is where the Lab is; it would also add a random seed to a report that is otherwise deterministic. A
//! two-way cluster bootstrap could come later, with more speakers.
//!
//! The t quantile comes from the t distribution's CDF (the regularised incomplete beta function, by its continued fraction)
//! inverted by bisection: to about 1e-12, at any degrees of freedom.

use std::collections::BTreeMap;

/// A mean and its confidence interval (`lo..=hi`), over `n` values in `clusters` clusters (the interval's unit: the fewer of
/// the speakers and the noises where both vary, `n` where each value is its own).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interval {
    pub n: usize,
    pub clusters: usize,
    pub mean: f64,
    pub lo: f64,
    pub hi: f64,
}

impl Interval {
    /// The interval holds zero: no difference shown.
    pub fn spans_zero(&self) -> bool {
        self.lo <= 0.0 && self.hi >= 0.0
    }
}

/// The confidence level the Lab reports.
pub const LEVEL: f64 = 0.95;

/// Which clusters a value belongs to: its speaker (the speech take) and its noise file.
pub type Key = (usize, usize);

/// The mean of the finite values of `x` (NaN when there are none), and how many there were.
pub fn mean(x: &[f64]) -> (f64, usize) {
    let (sum, n) = x.iter().filter(|v| v.is_finite()).fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    (if n == 0 { f64::NAN } else { sum / n as f64 }, n)
}

/// The mean of the finite values of `x` and its two-sided t-interval at `level`, cluster-robust by `keys` (a value's speaker
/// and noise; see the module notes): by speaker where one noise is among them, by noise where one speaker, two-way where
/// both vary. `None` with fewer than two clusters.
pub fn mean_ci(x: &[f64], keys: &[Key], level: f64) -> Option<Interval> {
    assert_eq!(x.len(), keys.len(), "a key a value");
    let v: Vec<(f64, Key)> = x.iter().zip(keys).filter(|(v, _)| v.is_finite()).map(|(v, k)| (*v, *k)).collect();
    let n = v.len();
    if n < 2 {
        return None;
    }
    let m = v.iter().map(|p| p.0).sum::<f64>() / n as f64;
    // The CR1 variance of the mean for a grouping, and its cluster count: G / (G - 1) times the sum over clusters of the
    // squared sum of their residuals, over n squared
    let variance = |group: &dyn Fn(Key) -> Key| -> (f64, usize) {
        let mut sums = BTreeMap::<Key, f64>::new();
        for &(x, k) in &v {
            *sums.entry(group(k)).or_default() += x - m;
        }
        let g = sums.len();
        let s2: f64 = sums.values().map(|s| s * s).sum();
        (if g < 2 { f64::NAN } else { g as f64 / (g - 1) as f64 * s2 / (n * n) as f64 }, g)
    };
    let (by_speaker, speakers) = variance(&|k| (k.0, 0));
    let (by_noise, noises) = variance(&|k| (0, k.1));
    let (var, clusters) = match (speakers >= 2, noises >= 2) {
        (true, true) => ((by_speaker + by_noise - variance(&|k| k).0).max(by_speaker).max(by_noise), speakers.min(noises)),
        (true, false) => (by_speaker, speakers),
        (false, true) => (by_noise, noises),
        (false, false) => return None,
    };
    let half = t_quantile(0.5 + level / 2.0, (clusters - 1) as f64) * var.sqrt();
    Some(Interval { n, clusters, mean: m, lo: m - half, hi: m + half })
}

/// `a[i] - b[i]` over the clips where both are finite, its mean and its cluster-robust t-interval at [`LEVEL`] (`keys`: each
/// clip's speaker and noise): the paired difference.
pub fn paired(a: &[f64], b: &[f64], keys: &[Key]) -> Option<Interval> {
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| if x.is_finite() && y.is_finite() { x - y } else { f64::NAN }).collect();
    mean_ci(&d, keys, LEVEL)
}

/// ln Γ(x) for x > 0 (Lanczos, g = 7, 9 terms: about 1e-15 relative).
fn ln_gamma(x: f64) -> f64 {
    #[rustfmt::skip]
    const C: [f64; 9] = [
        0.999_999_999_999_809_9, 676.520_368_121_885_1, -1_259.139_216_722_402_8, 771.323_428_777_653_1, -176.615_029_162_140_6,
        12.507_343_278_686_905, -0.138_571_095_265_720_12, 9.984_369_578_019_572e-6, 1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let t = x + 7.5;
    let s = C[1..].iter().enumerate().fold(C[0], |s, (i, c)| s + c / (x + 1.0 + i as f64));
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + s.ln()
}

/// The regularised incomplete beta function I_x(a, b) (its continued fraction by Lentz's method, on the side that converges).
fn inc_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - inc_beta(b, a, 1.0 - x);
    }
    const TINY: f64 = 1e-300;
    let (mut c, mut d) = (1.0f64, 1.0 - (a + b) * x / (a + 1.0));
    d = if d.abs() < TINY { TINY } else { d };
    d = 1.0 / d;
    let mut f = d;
    for m in 1..400 {
        let m = f64::from(m);
        for k in
            [m * (b - m) * x / ((a + 2.0 * m - 1.0) * (a + 2.0 * m)), -(a + m) * (a + b + m) * x / ((a + 2.0 * m) * (a + 2.0 * m + 1.0))]
        {
            d = 1.0 + k * d;
            d = if d.abs() < TINY { TINY } else { d };
            c = 1.0 + k / c;
            c = if c.abs() < TINY { TINY } else { c };
            d = 1.0 / d;
            f *= c * d;
        }
        if (c * d - 1.0).abs() < 1e-15 {
            break;
        }
    }
    front * f / a
}

/// Student's t CDF at `t` with `df` degrees of freedom.
pub fn t_cdf(t: f64, df: f64) -> f64 {
    let tail = 0.5 * inc_beta(df / 2.0, 0.5, df / (df + t * t));
    if t >= 0.0 { 1.0 - tail } else { tail }
}

/// The `p` quantile of Student's t with `df` degrees of freedom (bisection on [`t_cdf`]).
pub fn t_quantile(p: f64, df: f64) -> f64 {
    if !(p > 0.0 && p < 1.0) || df.is_nan() || df <= 0.0 {
        return f64::NAN;
    }
    if p < 0.5 {
        return -t_quantile(1.0 - p, df);
    }
    if p == 0.5 {
        return 0.0;
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    while t_cdf(hi, df) < p {
        hi *= 2.0;
        if hi > 1e12 {
            return f64::INFINITY;
        }
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if t_cdf(mid, df) < p {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi - lo < 1e-13 * hi.max(1.0) {
            break;
        }
    }
    0.5 * (lo + hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each value its own cluster (independent values: the plain t-interval).
    fn independent(n: usize) -> Vec<Key> {
        (0..n).map(|i| (i, 0)).collect()
    }

    /// The t quantiles against the tables (two-sided 95%: the 0.975 quantile), and the normal's in the limit.
    #[test]
    fn t_quantiles_match_the_tables() {
        #[rustfmt::skip]
        let table = [
            (1.0, 12.706_204_736), (2.0, 4.302_652_730), (3.0, 3.182_446_305), (5.0, 2.570_581_836), (9.0, 2.262_157_163),
            (19.0, 2.093_024_054), (29.0, 2.045_229_642), (99.0, 1.984_216_952), (1e7, 1.959_964_2),
        ];
        for (df, q) in table {
            let got = t_quantile(0.975, df);
            assert!((got - q).abs() < 1e-6 * q, "df {df}: {got} vs {q}");
        }
        assert!((t_quantile(0.995, 9.0) - 3.249_835_542).abs() < 1e-6);
        assert!((t_quantile(0.025, 9.0) + 2.262_157_163).abs() < 1e-6, "symmetric");
        assert_eq!(t_quantile(0.5, 4.0), 0.0);
        assert!(t_quantile(1.0, 4.0).is_nan() && t_quantile(0.9, 0.0).is_nan());
        assert!((t_cdf(0.0, 3.0) - 0.5).abs() < 1e-15 && (t_cdf(2.262_157_163, 9.0) - 0.975).abs() < 1e-9);
        assert!((ln_gamma(5.0) - 24f64.ln()).abs() < 1e-13 && (ln_gamma(0.5) - std::f64::consts::PI.sqrt().ln()).abs() < 1e-13);
    }

    /// A paired difference with a known mean, each clip its own cluster: its interval holds it, it's the textbook one, and
    /// "no difference" is decided by whether it holds 0.
    #[test]
    fn paired_differences_on_independent_synthetic_scores() {
        // Ten clips: model A scores each clip's baseline + 0.3 ± a fixed jitter
        let base = [3.1, 2.7, 3.8, 3.3, 2.9, 3.5, 3.0, 3.6, 2.8, 3.4];
        let jitter = [0.05, -0.02, 0.01, -0.04, 0.03, 0.0, -0.01, 0.02, -0.03, -0.01];
        let a: Vec<f64> = base.iter().zip(jitter).map(|(b, j)| b + 0.3 + j).collect();
        let keys = independent(10);
        let d = paired(&a, &base, &keys).unwrap();
        assert_eq!((d.n, d.clusters), (10, 10));
        assert!((d.mean - 0.3).abs() < 1e-12, "{d:?}");
        // By hand: the jitter sums to 0 and its squares to 0.007, so its standard deviation is sqrt(0.007 / 9) and the
        // half-width 2.262157 times that over sqrt(10)
        let sd = (jitter.iter().map(|j| j * j).sum::<f64>() / 9.0).sqrt();
        assert!((sd - (0.007f64 / 9.0).sqrt()).abs() < 1e-12, "{sd}");
        let half = 2.262_157_163 * sd / 10f64.sqrt();
        assert!((d.hi - d.mean - half).abs() < 1e-9 && (d.mean - d.lo - half).abs() < 1e-9, "{d:?}");
        assert!(!d.spans_zero());
        // The other way round, the same interval negated
        let r = paired(&base, &a, &keys).unwrap();
        assert!((r.lo + d.hi).abs() < 1e-12 && (r.hi + d.lo).abs() < 1e-12);
        // Pure noise around 0: no difference
        let noisy: Vec<f64> = base.iter().zip(jitter).map(|(b, j)| b + 3.0 * j).collect();
        assert!(paired(&noisy, &base, &keys).unwrap().spans_zero());
        // NaN pairs are left out; fewer than two pairs give nothing
        let mut gappy = a.clone();
        gappy[3] = f64::NAN;
        assert_eq!(paired(&gappy, &base, &keys).unwrap().n, 9);
        assert_eq!(paired(&[1.0], &[0.0], &[(0, 0)]), None);
        assert_eq!(mean(&[1.0, f64::NAN, 3.0]), (2.0, 2));
        assert!(mean(&[]).0.is_nan());
    }

    /// Clustered by speaker: with equal clusters it's exactly the t-interval over the cluster means; the point estimate stays
    /// the mean over the values with unequal ones; one cluster gives nothing.
    #[test]
    fn one_way_clusters_are_the_t_interval_over_their_means() {
        // 5 speakers, 4 clips each (one noise): a speaker effect much bigger than the clips' own scatter
        let effect = [0.4, 0.1, -0.2, 0.3, 0.0];
        let scatter = [0.01, -0.02, 0.015, -0.005];
        let (mut x, mut keys) = (Vec::new(), Vec::new());
        for (s, e) in effect.iter().enumerate() {
            for j in scatter {
                x.push(0.2 + e + j);
                keys.push((s, 3));
            }
        }
        let clustered = mean_ci(&x, &keys, LEVEL).unwrap();
        let means: Vec<f64> = x.chunks(4).map(|c| c.iter().sum::<f64>() / 4.0).collect();
        let over_means = mean_ci(&means, &independent(5), LEVEL).unwrap();
        assert_eq!((clustered.n, clustered.clusters), (20, 5));
        assert!((clustered.lo - over_means.lo).abs() < 1e-12 && (clustered.hi - over_means.hi).abs() < 1e-12, "{clustered:?}");
        // Treated as 20 independent clips it's far narrower: the optimism the clusters remove
        let naive = mean_ci(&x, &independent(20), LEVEL).unwrap();
        assert!(naive.hi - naive.lo < 0.6 * (clustered.hi - clustered.lo), "{naive:?} {clustered:?}");
        // Unequal clusters: the mean stays the values'
        let (x3, k3) = ([1.0, 1.2, 1.1, 2.0, 0.5], [(0, 0), (0, 0), (0, 0), (1, 0), (2, 0)]);
        let i = mean_ci(&x3, &k3, LEVEL).unwrap();
        assert!((i.mean - 5.8 / 5.0).abs() < 1e-12 && i.clusters == 3, "{i:?}");
        assert_eq!(mean_ci(&[1.0, 2.0, 3.0], &[(7, 1); 3], LEVEL), None, "one speaker and one noise: no interval");
    }

    /// Two-way, by speaker and noise: a difference that varies by noise file gets an interval at least as wide as the noise's
    /// own clustering, which may hold 0 where the speaker clustering alone didn't; the degrees of freedom are the fewer
    /// clusters less one.
    #[test]
    fn two_way_clusters_count_the_noise_files_too() {
        let noise_effect = [0.3, -0.1, 0.25, -0.15, 0.05, 0.2, -0.2];
        let (mut x, mut keys) = (Vec::new(), Vec::new());
        for s in 0..10 {
            for (n, e) in noise_effect.iter().enumerate() {
                for snr in 0..3 {
                    x.push(0.05 + e + 0.01 * ((s * 7 + n * 3 + snr) % 5) as f64 - 0.02);
                    keys.push((s, n));
                }
            }
        }
        let two = mean_ci(&x, &keys, LEVEL).unwrap();
        let by_speaker = mean_ci(&x, &keys.iter().map(|k| (k.0, 0)).collect::<Vec<_>>(), LEVEL).unwrap();
        let by_noise = mean_ci(&x, &keys.iter().map(|k| (0, k.1)).collect::<Vec<_>>(), LEVEL).unwrap();
        assert_eq!((two.n, two.clusters, by_speaker.clusters, by_noise.clusters), (210, 7, 10, 7));
        assert!(!by_speaker.spans_zero() && two.spans_zero(), "{by_speaker:?} {two:?}");
        assert!(two.hi - two.lo >= by_noise.hi - by_noise.lo - 1e-12, "{two:?} {by_noise:?}");
        // The noise one-way, with equal clusters, is the t over the 7 noise means (df 6)
        let means: Vec<f64> = (0..7)
            .map(|n| {
                let v: Vec<f64> = x.iter().zip(&keys).filter(|(_, k)| k.1 == n).map(|(v, _)| *v).collect();
                v.iter().sum::<f64>() / v.len() as f64
            })
            .collect();
        let over_means = mean_ci(&means, &independent(7), LEVEL).unwrap();
        assert!((by_noise.hi - over_means.hi).abs() < 1e-12, "{by_noise:?} {over_means:?}");
    }

    /// A deterministic standard normal source (xorshift, Box-Muller).
    fn rng(seed: u64) -> impl FnMut() -> f64 {
        let mut state = seed;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        move || {
            let (u, v) = (uniform().max(1e-300), uniform());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// Coverage on synthetic scores: over 2000 deterministic samples of 10 independent normal differences (mean 0.2, sd 0.5),
    /// the 95% interval holds the true mean about 95% of the time.
    #[test]
    fn the_interval_covers_at_its_level() {
        let mut normal = rng(0x2545_f491_4f6c_dd1d);
        let trials = 2000;
        let keys = independent(10);
        let held = (0..trials)
            .filter(|_| {
                let d: Vec<f64> = (0..10).map(|_| 0.2 + 0.5 * normal()).collect();
                let i = mean_ci(&d, &keys, LEVEL).unwrap();
                i.lo <= 0.2 && 0.2 <= i.hi
            })
            .count();
        let rate = held as f64 / f64::from(trials);
        assert!((rate - 0.95).abs() < 0.015, "coverage {rate}");
    }

    /// Coverage in the Lab's own layout (10 speakers with 1 to 3 segments, 17 in all, x 7 noises x 3 SNRs) when the
    /// differences vary by speaker, by noise and by segment as much as by clip: the two-way interval holds the true mean about
    /// 95% of the time (94.7% over these 1000 trials), the one over clips as if independent 27%.
    #[test]
    fn clustered_intervals_cover_in_the_lab_s_layout() {
        let segments_per_speaker = [1, 1, 1, 1, 1, 1, 3, 3, 3, 2];
        let mut normal = rng(0x9e37_79b9_7f4a_7c15);
        let trials = 1000;
        let (mut clustered, mut naive) = (0usize, 0usize);
        for _ in 0..trials {
            let speaker: Vec<f64> = (0..10).map(|_| 0.1 * normal()).collect();
            let noise: Vec<f64> = (0..7).map(|_| 0.1 * normal()).collect();
            let (mut d, mut keys) = (Vec::new(), Vec::new());
            for (s, &segs) in segments_per_speaker.iter().enumerate() {
                for _ in 0..segs {
                    let segment = 0.05 * normal();
                    for (n, ne) in noise.iter().enumerate() {
                        for _ in 0..3 {
                            d.push(0.2 + speaker[s] + ne + segment + 0.1 * normal());
                            keys.push((s, n));
                        }
                    }
                }
            }
            assert_eq!(d.len(), 357);
            let holds = |i: Interval| i.lo <= 0.2 && 0.2 <= i.hi;
            clustered += usize::from(holds(mean_ci(&d, &keys, LEVEL).unwrap()));
            naive += usize::from(holds(mean_ci(&d, &independent(d.len()), LEVEL).unwrap()));
        }
        let (c, n) = (clustered as f64 / f64::from(trials), naive as f64 / f64::from(trials));
        eprintln!("coverage: clustered {c:.3}, as if independent {n:.3}");
        assert!(c > 0.93 && n < 0.4, "clustered {c}, naive {n}");
    }
}
