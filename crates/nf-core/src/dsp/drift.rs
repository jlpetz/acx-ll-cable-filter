//! Keeps a FIFO's fill near a target by deciding when to slip one sample (TODO #9).
//!
//! Contract:
//! - Called once per block with the FIFO's fill in samples, measured at the same point each block:
//!   - output side: the cable's render-buffer padding plus our pending samples;
//!   - reference side: the loopback FIFO's length.
//!
//!   Too full → `Slip::Drop`; too empty → `Slip::Insert` (see `Slip`'s meaning).
//! - A PI controller on the low-passed fill error estimates the rate difference (ratio, in ppm). An
//!   accumulator turns it into slips: add `ppm * 1e-6 * BLOCK` per block, and emit a slip each time
//!   it passes ±1. Limit to one slip per block and to about ±1000 ppm.
//! - The fill measurement is noisy (packet-sized steps). Low-pass it, with a time constant of about
//!   0.5-1 s.
//! - Tune so that with a simulated device ±300 ppm off, the fill settles within ±1 ms of the target
//!   within 30 s, the slip rate equals the drift, and there's no limit cycle worse than ±0.5 ms.
//! - `ppm()` reports the current estimate (positive = the FIFO fills faster than we drain it). For
//!   the output side, that's the cable consuming slower than the mic produces.
//! - Large errors (more than two blocks, e.g. at start-up) are the caller's job, by whole-block pad
//!   or discard. `update` keeps tracking without winding up.
//! - Tests: a simulated producer/consumer pair at ±50, ±300 and ±1000 ppm with packet jitter
//!   (fill measured in 480- or 128-sample steps); assert settling, bounded fill, and slip rate within
//!   5% of the drift.
//!
//! Tuning (block period 10 ms): the fill moves `0.048 (d - u)` samples per second for a drift `d`
//! and a correction `u` in ppm, so a PI with `Kp` ppm per sample of error and `Ki` ppm per sample
//! per second closes the loop `s² + 0.048 Kp s + 0.048 Ki`. `Kp = 5`, `Ki = 0.6` put it at
//! ωn ≈ 0.17 rad/s, ζ ≈ 0.71, behind a 1 s low-pass (pole at 1 rad/s). They were chosen with the
//! simulation in the tests (`tuning_sweep`, ignored) over both sides, ±50/300/1000 ppm, 480- and
//! 128-sample packets and half-block start offsets:
//! - the fill stays within ±1 ms of the target from 23 s after start in the worst case;
//! - the slip rate is within 1.3% of the drift;
//! - with 128-sample packets the fill then stays within 19 samples (±0.2 ms) peak to peak.
//!
//! The loop's bandwidth is set by measurement noise. 480-sample packets whose timing is uniform over
//! a whole packet (the worst case that still averages out) add about 200 samples of noise to every
//! reading. A wider loop settles faster but passes that noise on as fill wander: 0.3 rad/s
//! (`Kp = 10`, `Ki = 1.9`, 0.5 s filter) wandered ±1.2 ms; this wanders ±0.8 ms, and the ppm
//! readout by about ±10 ppm.
//!
//! `ppm()` is the integral term: the drift estimate, without the proportional term's correction of
//! the current error. The output is limited to ±1500 ppm (0.72 slips per block), which leaves a
//! 1000 ppm device room for the correction.
//!
//! The reading must not be quantised to packets of the block's own period without timing jitter.
//! With two 10 ms clocks, the quantisation error then moves by a whole packet once per beat period
//! (33 s at 300 ppm) instead of averaging out. The loop stays bounded and the slip rate still
//! averages to the drift, but the fill swings by up to a packet. The engine should interpolate the
//! device position (QPC) or otherwise measure below a packet.

use super::slip::Slip;
use crate::{BLOCK, SAMPLE_RATE};

/// Block period in seconds.
const BLOCK_S: f32 = BLOCK as f32 / SAMPLE_RATE as f32;
/// Fill low-pass time constant.
const FILTER_TAU_S: f32 = 1.0;
/// Proportional gain, ppm per sample of filtered error.
const KP: f32 = 5.0;
/// Integral gain, ppm per sample of filtered error per second.
const KI: f32 = 0.6;
/// Output and integrator limit, ppm.
const MAX_PPM: f32 = 1500.0;
/// The error the loop acts on is clamped to this (two blocks): beyond it, the caller pads or
/// discards whole blocks, and the integrator mustn't wind up meanwhile.
const MAX_ERROR: f32 = 2.0 * BLOCK as f32;

/// Drift controller state.
#[derive(Clone, Debug)]
pub struct DriftController {
    target: f32,
    /// Low-passed fill, in samples.
    filtered: f32,
    /// Whether `filtered` has been seeded by a reading.
    primed: bool,
    /// The integral term in ppm: the rate-difference estimate.
    integral: f32,
    /// Slip accumulator, in samples (|acc| < 1 after each update).
    acc: f32,
    /// Filter coefficient per block.
    alpha: f32,
    kp: f32,
    ki: f32,
}

impl DriftController {
    /// A controller holding the fill at `target_samples`.
    pub fn new(target_samples: f32) -> Self {
        Self {
            target: target_samples,
            filtered: target_samples,
            primed: false,
            integral: 0.0,
            acc: 0.0,
            alpha: 1.0 - (-BLOCK_S / FILTER_TAU_S).exp(),
            kp: KP,
            ki: KI,
        }
    }

    /// One block's update; returns the slip to apply to this block.
    pub fn update(&mut self, fill_samples: f32) -> Slip {
        // A non-finite reading is skipped: the loop coasts on its last estimate.
        if fill_samples.is_finite() {
            if self.primed {
                self.filtered += self.alpha * (fill_samples - self.filtered);
            } else {
                self.filtered = fill_samples;
                self.primed = true;
            }
        }
        let raw = self.filtered - self.target;
        // Checked before the clamp: `NaN.max(x)` is `x`, which would read as a full-scale error.
        let e = if raw.is_nan() { 0.0 } else { raw.clamp(-MAX_ERROR, MAX_ERROR) };

        let p = self.kp * e;
        let i = self.integral + self.ki * BLOCK_S * e;
        // Anti-windup: integrate only while the output isn't pinned at the limit in e's direction.
        let pinned = (p + i > MAX_PPM && e > 0.0) || (p + i < -MAX_PPM && e < 0.0);
        if !pinned {
            self.integral = i.clamp(-MAX_PPM, MAX_PPM);
        }
        let u = (p + self.integral).clamp(-MAX_PPM, MAX_PPM);

        // |u| <= MAX_PPM adds at most 0.72 a block, so at most one slip per block.
        self.acc += u * 1e-6 * BLOCK as f32;
        if self.acc >= 1.0 {
            self.acc -= 1.0;
            Slip::Drop
        } else if self.acc <= -1.0 {
            self.acc += 1.0;
            Slip::Insert
        } else {
            Slip::None
        }
    }

    /// Current rate-difference estimate in ppm.
    pub fn ppm(&self) -> f32 {
        self.integral
    }

    /// The filtered fill, in samples (for telemetry).
    pub fn filtered_fill(&self) -> f32 {
        self.filtered
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn set_target(&mut self, target_samples: f32) {
        self.target = target_samples;
    }

    /// Restarts tracking from the current fill (after a stream restart or a coarse correction).
    /// The rate estimate restarts from 0 too. A non-finite fill leaves the filter to be seeded by
    /// the next reading.
    pub fn reset(&mut self, fill_samples: f32) {
        self.primed = fill_samples.is_finite();
        self.filtered = if self.primed { fill_samples } else { self.target };
        self.integral = 0.0;
        self.acc = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which FIFO the device is on.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Side {
        /// We write 480 ± 1 per block; the device (the cable) consumes packets.
        Output,
        /// The device (loopback) writes packets; we read 480 ± 1 per block.
        Reference,
    }

    /// A device on its own clock moving `packet`-sample packets, against our blocks on the mic
    /// clock. Time is in mic samples. A packet moves once the device has clocked all of it, at its
    /// nominal time plus uniform jitter of ±`jitter` packets.
    struct Sim {
        side: Side,
        packet: f64,
        /// Device samples per mic sample.
        rate: f64,
        jitter: f64,
        phase: f64,
        rng: u64,
        next_packet: u64,
        next_time: f64,
        t: f64,
        /// FIFO content in samples (may go negative: the sim has no underrun handling).
        fifo: f64,
        /// Our total written or read.
        ours: f64,
        fifo0: f64,
    }

    impl Sim {
        /// `ppm`: how much faster the FIFO fills than we drain it (the controller's sign).
        fn new(side: Side, ppm: f64, packet: usize, jitter: f64, fill: f64, seed: u64) -> Self {
            let rate = match side {
                Side::Output => 1.0 - ppm * 1e-6,
                Side::Reference => 1.0 + ppm * 1e-6,
            };
            let mut s = Self {
                side,
                packet: packet as f64,
                rate,
                jitter,
                phase: 0.0,
                rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                next_packet: 0,
                next_time: 0.0,
                t: 0.0,
                fifo: fill,
                ours: 0.0,
                fifo0: fill,
            };
            s.phase = s.uniform() * s.packet; // an arbitrary device phase
            s.next_time = s.packet_time(0);
            s
        }

        fn uniform(&mut self) -> f64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) as f64 / (1u64 << 53) as f64
        }

        fn packet_time(&mut self, l: u64) -> f64 {
            let j = (self.uniform() * 2.0 - 1.0) * self.jitter * self.packet;
            ((l + 1) as f64 * self.packet + j) / self.rate + self.phase
        }

        /// Advances one block and returns the reading: the FIFO just before we write or read.
        fn measure(&mut self) -> f64 {
            self.t += BLOCK as f64;
            while self.next_time <= self.t {
                match self.side {
                    Side::Output => self.fifo -= self.packet,
                    Side::Reference => self.fifo += self.packet,
                }
                self.next_packet += 1;
                self.next_time = self.packet_time(self.next_packet);
            }
            self.fifo
        }

        fn apply(&mut self, slip: Slip) {
            let n = match self.side {
                Side::Output => slip.produced_len(),
                Side::Reference => slip.consumed_len(),
            } as f64;
            self.ours += n;
            match self.side {
                Side::Output => self.fifo += n,
                Side::Reference => self.fifo -= n,
            }
        }

        /// The reading without quantisation noise: the fill if the device moved samples
        /// continuously, plus the quantisation's mean of half a packet (the reading still counts
        /// samples the device has clocked on the output side, and misses them on the reference
        /// side). This is what the controller holds at its target.
        fn smooth_reading(&self) -> f64 {
            let device = (self.t - self.phase) * self.rate;
            match self.side {
                Side::Output => self.fifo0 + self.ours - device + 0.5 * self.packet,
                Side::Reference => self.fifo0 + device - self.ours - 0.5 * self.packet,
            }
        }
    }

    struct Run {
        /// Largest |smooth reading - target| after `settle_s`.
        dev: f64,
        /// Peak-to-peak of the smooth reading after `settle_s`.
        ptp: f64,
        /// When the smooth reading last left ±1 ms of the target, in seconds.
        settled_at: f64,
        /// Largest |smooth reading - target| over the whole run.
        worst: f64,
        /// Net slips per sample after `settle_s`, as ppm.
        slip_ppm: f64,
        ppm_estimate: f32,
    }

    const TARGET: f32 = 960.0;
    const MS: f64 = SAMPLE_RATE as f64 / 1000.0;
    const BLOCKS_PER_S: f64 = SAMPLE_RATE as f64 / BLOCK as f64;
    const DRIFTS: [f64; 6] = [50.0, -50.0, 300.0, -300.0, 1000.0, -1000.0];

    struct Scenario {
        side: Side,
        ppm: f64,
        packet: usize,
        jitter: f64,
        start_offset: f64,
        settle_s: f64,
        total_s: f64,
        seed: u64,
    }

    impl Scenario {
        fn new(side: Side, ppm: f64, packet: usize, jitter: f64) -> Self {
            // Long enough after settling that a few samples of fill wander can't move the slip
            // count by 5%, even at 50 ppm (2.4 slips/s).
            let total_s = if ppm.abs() < 100.0 { 630.0 } else { 330.0 };
            Self { side, ppm, packet, jitter, start_offset: 0.0, settle_s: 30.0, total_s, seed: 1 }
        }

        fn run(&self) -> Run {
            self.run_with(DriftController::new(TARGET))
        }

        fn run_with(&self, mut ctl: DriftController) -> Run {
            let fill0 = f64::from(TARGET) + self.start_offset;
            let mut sim = Sim::new(self.side, self.ppm, self.packet, self.jitter, fill0, self.seed);
            let settle = (self.settle_s * BLOCKS_PER_S) as usize;
            let total = (self.total_s * BLOCKS_PER_S) as usize;
            let (mut dev, mut lo, mut hi, mut worst) = (0.0f64, f64::MAX, f64::MIN, 0.0f64);
            let (mut last_out, mut net) = (0usize, 0i64);
            for k in 0..total {
                let reading = sim.measure();
                let smooth = sim.smooth_reading();
                let d = (smooth - f64::from(TARGET)).abs();
                worst = worst.max(d);
                if d > MS {
                    last_out = k + 1;
                }
                let slip = ctl.update(reading as f32);
                if k >= settle {
                    dev = dev.max(d);
                    lo = lo.min(smooth);
                    hi = hi.max(smooth);
                    net += match slip {
                        Slip::Drop => 1,
                        Slip::Insert => -1,
                        Slip::None => 0,
                    };
                }
                sim.apply(slip);
            }
            Run {
                dev,
                ptp: hi - lo,
                settled_at: last_out as f64 / BLOCKS_PER_S,
                worst,
                slip_ppm: net as f64 / ((total - settle) * BLOCK) as f64 * 1e6,
                ppm_estimate: ctl.ppm(),
            }
        }

        /// The contract's checks: within ±1 ms of the target by 30 s and after, slip rate within 5%
        /// of the drift, peak to peak within `ptp_limit`, and the estimate within 10% plus
        /// `est_noise` ppm.
        fn check(&self, ptp_limit: f64, est_noise: f64) {
            let r = self.run();
            let tag = format!(
                "{:?} {} ppm, {}-sample packets, jitter {}, offset {}",
                self.side, self.ppm, self.packet, self.jitter, self.start_offset
            );
            assert!(r.settled_at <= 30.0, "{tag}: still outside ±1 ms at {} s", r.settled_at);
            assert!(r.dev <= MS, "{tag}: {} samples off after settling", r.dev);
            assert!(r.ptp <= ptp_limit, "{tag}: {} samples peak to peak", r.ptp);
            assert!((r.slip_ppm - self.ppm).abs() <= 0.05 * self.ppm.abs(), "{tag}: slips at {} ppm", r.slip_ppm);
            let est = f64::from(r.ppm_estimate);
            assert!((est - self.ppm).abs() <= 0.1 * self.ppm.abs() + est_noise, "{tag}: estimate {est}");
            assert!(r.worst <= 2.0 * BLOCK as f64, "{tag}: transient reached {} samples", r.worst);
        }
    }

    #[test]
    fn settles_with_128_sample_packets_and_no_limit_cycle() {
        // Steps of 128 against 480-sample blocks average out within a few blocks: no limit cycle
        // beyond ±0.5 ms (48 samples peak to peak), with or without timing jitter.
        for side in [Side::Output, Side::Reference] {
            for ppm in DRIFTS {
                for jitter in [0.0, 0.25] {
                    Scenario::new(side, ppm, 128, jitter).check(MS, 5.0);
                }
            }
        }
    }

    #[test]
    fn settles_with_dithered_480_sample_packets() {
        // Whole-packet steps with timing uniform over a packet: the worst noise that still averages
        // out. The fill wanders with the noise (not a limit cycle), within ±1 ms, and the integral
        // (the ppm readout) carries about ±10 ppm of it.
        for side in [Side::Output, Side::Reference] {
            for ppm in DRIFTS {
                for seed in [1, 2] {
                    Scenario { seed, ..Scenario::new(side, ppm, 480, 0.5) }.check(2.0 * MS, 20.0);
                }
            }
        }
    }

    #[test]
    fn a_start_up_offset_of_half_a_block_settles_in_30_s() {
        for side in [Side::Output, Side::Reference] {
            for ppm in [300.0, -300.0, 1000.0, -1000.0] {
                for start_offset in [240.0, -240.0] {
                    Scenario { start_offset, ..Scenario::new(side, ppm, 128, 0.25) }.check(MS, 5.0);
                }
            }
        }
    }

    #[test]
    fn large_errors_pin_the_output_without_winding_up() {
        // The caller hasn't corrected a large error yet: the output pins at the limit and the
        // integrator doesn't run past it.
        let mut ctl = DriftController::new(TARGET);
        let mut drops = 0;
        for _ in 0..10_000 {
            match ctl.update(TARGET + 1e6) {
                Slip::Drop => drops += 1,
                Slip::Insert => panic!("wrong direction"),
                Slip::None => {}
            }
        }
        assert!((0.0..=MAX_PPM).contains(&ctl.ppm()), "{}", ctl.ppm());
        // Pinned at MAX_PPM: 0.72 slips per block, never more than one.
        let rate = drops as f32 / 10_000.0;
        assert!((rate - MAX_PPM * 1e-6 * BLOCK as f32).abs() < 0.01, "{rate}");

        ctl.reset(TARGET);
        assert_eq!((ctl.ppm(), ctl.filtered_fill()), (0.0, TARGET));
        let slips = (0..1000).filter(|_| ctl.update(TARGET) != Slip::None).count();
        assert_eq!(slips, 0, "at the target with no drift estimate, nothing slips");
    }

    #[test]
    fn block_period_packets_without_jitter_stay_bounded() {
        // The case the module docs warn about: the reading moves by a whole packet once per beat
        // period. The loop must stay bounded and slip at the drift rate on average.
        for ppm in [300.0, -300.0, 1000.0, -1000.0] {
            let r = Scenario { total_s: 1230.0, ..Scenario::new(Side::Output, ppm, 480, 0.02) }.run();
            assert!(r.worst <= 2.0 * BLOCK as f64, "{ppm} ppm: excursion {}", r.worst);
            assert!((r.slip_ppm - ppm).abs() <= 0.05 * ppm.abs(), "{ppm} ppm: slips at {}", r.slip_ppm);
        }
    }

    #[test]
    fn set_target_moves_the_fill_and_nan_readings_are_skipped() {
        let mut sim = Sim::new(Side::Output, 100.0, 128, 0.25, f64::from(TARGET), 9);
        let mut ctl = DriftController::new(TARGET);
        for _ in 0..3000 {
            let f = sim.measure();
            sim.apply(ctl.update(f as f32));
        }
        ctl.set_target(TARGET + 96.0);
        assert_eq!(ctl.target(), TARGET + 96.0);
        let mut smooth = 0.0;
        for k in 0..4000 {
            let f = sim.measure();
            smooth = sim.smooth_reading();
            let f = if k % 50 == 0 { f32::NAN } else { f as f32 };
            sim.apply(ctl.update(f));
        }
        let off = smooth - f64::from(TARGET + 96.0);
        assert!(off.abs() < MS, "{off}");
        assert!((ctl.ppm() - 100.0).abs() < 10.0, "{}", ctl.ppm());
    }

    #[test]
    fn the_first_reading_seeds_the_filter() {
        let mut ctl = DriftController::new(TARGET);
        ctl.update(TARGET + 30.0);
        assert_eq!(ctl.filtered_fill(), TARGET + 30.0);
        ctl.reset(f32::NAN);
        ctl.update(TARGET - 5.0);
        assert_eq!(ctl.filtered_fill(), TARGET - 5.0, "a NaN reset leaves it unseeded");
        // A NaN target can't turn into a full-scale correction.
        let mut ctl = DriftController::new(f32::NAN);
        assert!((0..1000).all(|_| ctl.update(TARGET) == Slip::None));
    }

    impl DriftController {
        fn tuned(target: f32, kp: f32, ki: f32, tau: f32) -> Self {
            let mut c = Self::new(target);
            c.kp = kp;
            c.ki = ki;
            c.alpha = 1.0 - (-BLOCK_S / tau).exp();
            c
        }
    }

    /// How the gains were chosen: the worst case of each metric over every scenario, per gain set.
    /// Run with `cargo test -p nf-core --release -- --ignored tuning_sweep --nocapture`.
    #[test]
    #[ignore = "tuning aid: prints a table"]
    fn tuning_sweep() {
        let sets = [(10.0, 1.9, 0.5), (KP, KI, FILTER_TAU_S), (5.0, 0.5, 1.0), (4.0, 0.45, 1.0), (6.0, 0.6, 1.0)];
        for (kp, ki, tau) in sets {
            let (mut fine_ptp, mut dev, mut settled, mut slip_err) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for (packet, jitter) in [(480usize, 0.5), (128, 0.0), (128, 0.25)] {
                for side in [Side::Output, Side::Reference] {
                    for ppm in DRIFTS {
                        for start_offset in [0.0, 240.0, -240.0] {
                            for seed in [1u64, 2, 3] {
                                let s = Scenario { start_offset, seed, ..Scenario::new(side, ppm, packet, jitter) };
                                let r = s.run_with(DriftController::tuned(TARGET, kp, ki, tau));
                                if packet == 128 {
                                    fine_ptp = fine_ptp.max(r.ptp);
                                }
                                dev = dev.max(r.dev);
                                settled = settled.max(r.settled_at);
                                slip_err = slip_err.max((r.slip_ppm - ppm).abs() / ppm.abs());
                            }
                        }
                    }
                }
            }
            eprintln!(
                "kp {kp} ki {ki} tau {tau}: 128-packet ptp {fine_ptp:.1}, max |fill - target| after 30 s {dev:.1}, \
                 within ±1 ms from {settled:.1} s, slip error {:.2}%",
                slip_err * 100.0
            );
        }
    }
}
