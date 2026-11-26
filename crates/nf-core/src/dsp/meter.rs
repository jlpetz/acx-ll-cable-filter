//! Peak and RMS metering with clip detection (TODO #12).

use crate::{BLOCK, Block, lin_to_db};

/// Independent accumulator lanes: one `ymm` register of f32 per reduction at x86-64-v3.
const LANES: usize = 8;
const _: () = assert!(BLOCK.is_multiple_of(LANES));

/// Mean of squares of a block (the gate's detector and the noise stage's suppression estimate),
/// in [`LANES`] independent lanes so it vectorises.
#[inline]
pub(crate) fn mean_square(x: &Block) -> f32 {
    let mut sum = [0.0f32; LANES];
    let (chunks, _) = x.as_chunks::<LANES>();
    for c in chunks {
        for (s, &v) in sum.iter_mut().zip(c) {
            *s += v * v;
        }
    }
    sum.iter().sum::<f32>() / BLOCK as f32
}

/// A clip: at least this many consecutive samples at or above [`CLIP_LEVEL`], or any sample at or
/// above full scale. A flat top of a few samples is what a clipped ADC produces.
pub const CLIP_RUN: usize = 3;
/// Samples at or above this magnitude count toward a clip run (-0.009 dBFS).
pub const CLIP_LEVEL: f32 = 0.999;
/// Peaks at or above this magnitude are "near clip" (-0.1 dBFS).
pub const NEAR_CLIP_LEVEL: f32 = 0.988_553_1;
/// Lower end of the numerical level display, -120 dBFS. Actual audio samples remain available below this level.
pub(crate) const METER_FLOOR: f32 = 1e-6;

/// One block's reading.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    /// Largest |sample|, linear.
    pub peak: f32,
    /// Mean of squares, linear.
    pub mean_square: f32,
    pub clipped: bool,
    pub near_clip: bool,
}

impl MeterReading {
    pub fn peak_dbfs(&self) -> f32 {
        lin_to_db(self.peak)
    }
    pub fn rms_dbfs(&self) -> f32 {
        lin_to_db(self.mean_square.sqrt())
    }
}

/// Display ballistics for a published meter, so a reader sampling the value (the GUI at 20 Hz, the
/// CLI at 1 Hz) still sees every block's peak: the peak holds for 1 s and then falls 20 dB/s, and the
/// RMS is averaged with a 300 ms time constant. When the signal stays below the numerical display range, release the
/// historical RMS after 300 ms and the held peak after 1 s. O(1) per block, no allocation.
///
/// Non-finite readings are skipped: a NaN never wins the peak or enters the average, and neither
/// does an infinity (it would hold forever, since `inf * decay` is `inf`). Both values flush to 0
/// below -200 dB, so a long silence never leaves them denormal.
#[derive(Clone, Debug, Default)]
pub struct MeterBallistics {
    peak: f32,
    hold_left: u32,
    mean_square: f32,
    primed: bool,
    below_floor_blocks: u32,
}

/// Blocks the peak holds before it falls (1 s).
const HOLD_BLOCKS: u32 = 100;
/// After this many consecutive below-range blocks, show the current RMS (300 ms).
const RMS_RELEASE_BLOCKS: u32 = 30;
/// Per-block peak fall: 20 dB/s at 100 blocks/s, 10^(-20/20/100).
const PEAK_DECAY: f32 = 0.977_237_2;
/// One-pole coefficient for a 300 ms RMS average at 10 ms blocks: 1 - exp(-10/300).
const RMS_ALPHA: f32 = 0.032_783_9;

impl MeterBallistics {
    pub const fn new() -> Self {
        Self { peak: 0.0, hold_left: 0, mean_square: 0.0, primed: false, below_floor_blocks: 0 }
    }

    /// Takes one block's reading; returns the (peak, RMS) to display, in dBFS.
    pub fn update(&mut self, r: &MeterReading) -> (f32, f32) {
        let below_floor = (0.0..=METER_FLOOR).contains(&r.peak) && r.mean_square.is_finite();
        self.below_floor_blocks = if below_floor { (self.below_floor_blocks + 1).min(HOLD_BLOCKS) } else { 0 };
        if r.peak.is_finite() && (r.peak >= self.peak || self.below_floor_blocks == HOLD_BLOCKS) {
            self.peak = r.peak;
            self.hold_left = HOLD_BLOCKS;
        } else if self.hold_left > 0 {
            self.hold_left -= 1;
        } else {
            self.peak *= PEAK_DECAY;
            if self.peak < 1e-10 {
                self.peak = 0.0;
            }
        }
        if r.mean_square.is_finite() {
            if self.primed && self.below_floor_blocks < RMS_RELEASE_BLOCKS {
                self.mean_square += RMS_ALPHA * (r.mean_square - self.mean_square);
                if self.mean_square < 1e-20 {
                    self.mean_square = 0.0;
                }
            } else {
                self.mean_square = r.mean_square;
                self.primed = true;
            }
        }
        (lin_to_db(self.peak), lin_to_db(self.mean_square.sqrt()))
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Stateless apart from the clip run, which carries across block edges.
#[derive(Clone, Debug, Default)]
pub struct Meter {
    run: usize,
}

impl Meter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Measures a block. Eight independent lanes let LLVM vectorise both reductions. The peak uses
    /// a compare rather than `f32::max`, so it is a bare `maxps` with no NaN fix-up; a NaN sample
    /// never wins the compare, which is what `f32::max` did too.
    #[inline]
    pub fn measure(&mut self, x: &Block) -> MeterReading {
        let mut peak = [0.0f32; LANES];
        let mut sum = [0.0f32; LANES];
        let (chunks, _) = x.as_chunks::<LANES>(); // BLOCK is a multiple of LANES: no remainder
        for c in chunks {
            for (l, &v) in c.iter().enumerate() {
                let a = v.abs();
                peak[l] = if a > peak[l] { a } else { peak[l] };
                sum[l] += v * v;
            }
        }
        let peak = peak.iter().fold(0.0f32, |m, &v| if v > m { v } else { m });
        let mean_square = sum.iter().sum::<f32>() / BLOCK as f32;
        let mut clipped = peak >= 1.0;
        if peak >= CLIP_LEVEL {
            // Rare: only scan for a flat top when something came close.
            for v in x {
                if v.abs() >= CLIP_LEVEL {
                    self.run += 1;
                    if self.run >= CLIP_RUN {
                        clipped = true;
                    }
                } else {
                    self.run = 0;
                }
            }
        } else {
            self.run = 0;
        }
        MeterReading { peak, mean_square, clipped, near_clip: peak >= NEAR_CLIP_LEVEL }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_reads_minus_three_db_below_its_peak() {
        let mut x = [0.0f32; BLOCK];
        for (i, s) in x.iter_mut().enumerate() {
            *s = 0.9 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin();
        }
        let r = Meter::new().measure(&x);
        let expect = 20.0 * 0.9f32.log10() - 3.0103;
        assert!((r.rms_dbfs() - expect).abs() < 0.05, "{} vs {expect}", r.rms_dbfs());
        assert!((r.peak - 0.9).abs() < 1e-4 && !r.clipped && !r.near_clip);
    }

    #[test]
    fn full_scale_sample_is_a_clip() {
        let mut x = [0.0f32; BLOCK];
        x[100] = 1.0;
        assert!(Meter::new().measure(&x).clipped);
    }

    #[test]
    fn flat_top_is_a_clip_across_blocks() {
        let mut m = Meter::new();
        let mut a = [0.1f32; BLOCK];
        a[BLOCK - 2] = 1.0 - 1e-4;
        a[BLOCK - 1] = 1.0 - 1e-4;
        assert!(!m.measure(&a).clipped);
        let mut b = [0.1f32; BLOCK];
        b[0] = 1.0 - 1e-4;
        assert!(m.measure(&b).clipped, "run of 3 spanning the block edge");
    }

    fn reading(peak: f32, rms: f32) -> MeterReading {
        MeterReading { peak, mean_square: rms * rms, ..MeterReading::default() }
    }

    #[test]
    fn ballistics_hold_the_peak_for_a_second_then_fall_20_db_per_second() {
        let mut b = MeterBallistics::new();
        let (p, _) = b.update(&reading(0.5, 0.1)); // -6 dBFS
        assert!((p - -6.0206).abs() < 1e-3);
        // A quieter block doesn't lower it for 100 blocks (1 s) ...
        for k in 0..HOLD_BLOCKS {
            let (p, _) = b.update(&reading(0.01, 0.01));
            assert!((p - -6.0206).abs() < 1e-3, "held at block {k}: {p}");
        }
        // ... then it falls 0.2 dB a block, 20 dB a second.
        let mut last = -6.0206;
        for k in 0..100 {
            let (p, _) = b.update(&reading(0.001, 0.001));
            assert!((last - p - 0.2).abs() < 1e-3, "fall at block {k}: {last} -> {p}");
            last = p;
        }
        assert!((last - -26.02).abs() < 0.02, "{last}");
        // A new peak at or above the falling value takes over at once and restarts the hold.
        let (p, _) = b.update(&reading(0.1, 0.0));
        assert!((p - -20.0).abs() < 1e-3);
        for _ in 0..HOLD_BLOCKS {
            assert!((b.update(&reading(0.001, 0.001)).0 - -20.0).abs() < 1e-3);
        }
        assert!(b.update(&reading(0.001, 0.001)).0 < -20.1);
        // It falls all the way to the floor value and stays exactly 0, not denormal.
        for _ in 0..2000 {
            b.update(&reading(0.0, 0.0));
        }
        assert_eq!(b.peak, 0.0);
        assert_eq!(b.update(&reading(0.0, 0.0)).0, -200.0);
    }

    #[test]
    fn sustained_silence_releases_the_meters_and_the_next_signal_registers_immediately() {
        for (level, expected) in [(0.0, -200.0), (1e-9, -180.0)] {
            let mut b = MeterBallistics::new();
            b.update(&reading(1.0, 1.0));
            for block in 1..=HOLD_BLOCKS {
                let (peak, rms) = b.update(&reading(level, level));
                if block >= RMS_RELEASE_BLOCKS {
                    assert!((rms - expected).abs() < 0.01, "RMS after {block} blocks: {rms}");
                }
                if block < HOLD_BLOCKS {
                    assert_eq!(peak, 0.0);
                } else {
                    assert!((peak - expected).abs() < 0.01, "peak after {block} blocks: {peak}");
                }
            }
            let (peak, rms) = b.update(&reading(1e-5, 1e-5));
            assert!((peak + 100.0).abs() < 0.01 && rms > -120.0, "{peak}, {rms}");
        }
    }

    #[test]
    fn ballistics_average_the_rms_over_300_ms() {
        let mut b = MeterBallistics::new();
        // The first block primes the average directly.
        let (_, r) = b.update(&reading(0.1, 0.1));
        assert!((r - -20.0).abs() < 1e-3);
        // A step to -40 dB: after one time constant (30 blocks) the mean square has gone 1 - 1/e of
        // the way, in the power domain.
        let mut r = 0.0;
        for _ in 0..30 {
            r = b.update(&reading(0.01, 0.01)).1;
        }
        let (hi, lo) = (0.01f64, 0.0001f64);
        let expect = 10.0 * (lo + (hi - lo) * (-1.0f64).exp()).log10();
        assert!((f64::from(r) - expect).abs() < 0.05, "{r} vs {expect}");
        // One loud block barely moves it (the CLI and GUI see a steady bar, not one 10 ms window).
        let (_, r1) = b.update(&reading(0.5, 0.5));
        assert!(r1 - r < 10.0 && r1 > r);
        // Settles at the input level, and decays to an exact 0 in silence.
        for _ in 0..1000 {
            r = b.update(&reading(0.01, 0.01)).1;
        }
        assert!((r - -40.0).abs() < 0.01);
        for _ in 0..3000 {
            b.update(&reading(0.0, 0.0));
        }
        assert_eq!(b.mean_square, 0.0);
    }

    #[test]
    fn ballistics_skip_nan_and_infinite_readings() {
        let mut b = MeterBallistics::new();
        b.update(&reading(0.25, 0.1));
        for bad in [f32::NAN, f32::INFINITY] {
            let (p, r) = b.update(&MeterReading { peak: bad, mean_square: bad, ..MeterReading::default() });
            assert!(p.is_finite() && r.is_finite(), "{bad}: {p} {r}");
            assert!((p - -12.04).abs() < 0.01 && (r - -20.0).abs() < 1e-3, "{bad}: {p} {r}");
        }
        // The hold keeps counting through them, and the fall starts on time.
        for _ in 0..HOLD_BLOCKS - 2 {
            b.update(&reading(0.0, 0.1));
        }
        assert!(b.update(&reading(0.0, 0.1)).0 < -12.1);
        // A NaN before the first finite reading leaves the average unprimed.
        let mut b = MeterBallistics::new();
        b.update(&reading(f32::NAN, f32::NAN));
        assert_eq!(b.update(&reading(0.1, 0.1)).1, -20.0);
        b.reset();
        assert_eq!(b.update(&reading(0.0, 0.0)), (-200.0, -200.0));
    }

    #[test]
    fn ballistics_constants_match_their_definitions() {
        assert!((PEAK_DECAY - 10f32.powf(-20.0 / 20.0 / 100.0)).abs() < 1e-7);
        assert!((RMS_ALPHA - (1.0 - (-10.0f32 / 300.0).exp())).abs() < 1e-7);
    }

    #[test]
    fn nan_never_wins_the_peak_and_mean_square_matches() {
        let mut x = [0.25f32; BLOCK];
        x[7] = f32::NAN;
        x[300] = -0.5;
        let r = Meter::new().measure(&x);
        assert_eq!(r.peak, 0.5);
        let y: Block = std::array::from_fn(|i| ((i * 37) % 101) as f32 / 101.0 - 0.5);
        let r = Meter::new().measure(&y);
        let direct = y.iter().map(|v| f64::from(*v * *v)).sum::<f64>() / BLOCK as f64;
        assert!((f64::from(r.mean_square) - direct).abs() < 1e-6);
        assert_eq!(mean_square(&y), r.mean_square);
    }
}
