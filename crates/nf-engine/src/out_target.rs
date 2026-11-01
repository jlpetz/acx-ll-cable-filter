//! The adaptive output target (`IoConfig::adaptive_output`; engine.md, "The output target"; TODO 13's detail, "Budget").
//!
//! The drift loop holds the mean of the output fills read before each write at the target, so a block that comes later
//! than the others finds the fill that much under the mean: its **dip**. The lowest fill is the target less the largest
//! dip, and the device needs a period of it (plus the margin) at its lowest. So the target the delivery needs is the
//! **floor** (one device period + the margin) + the largest dip. The fixed target (one period + one block + the margin)
//! allows a dip of a block: the spare block that bursts use, and that a missed wake uses.
//!
//! - **Measured:** each ordinary block's dip under its own mean (a 1 s low-pass, as the drift loop's), on the frames
//!   written less the device's position now (the fill before its calibration to the padding: the calibration steps while
//!   it finds the device's sawtooth, over the first beat of the clocks, and those steps aren't lateness), or on the
//!   padding without a clock. After a correction (zeros, a drop) the mean starts again from the plain mean of its first
//!   [`WARM_UP`] blocks, which aren't counted. Each second's largest dip is kept for [`WINDOW_S`] seconds; the need is the
//!   largest over the window (a high percentile would let the rarest bursts through, and each one costs an underrun).
//! - **Lowered slowly:** once a whole window has passed since the last move was decided and the floor + the need +
//!   [`HEADROOM`] is at least [`HYSTERESIS`] under the goal, that is the new goal, and the target walks down to it a
//!   little each second ([`ramp_step`]: a tenth of the way, at most [`RAMP_MAX_STEP`], at least a sample; 9 ms takes
//!   [`RAMP_MAX_S`] seconds, the last 0.25 ms of it in the last 12). The drift loop follows by slips
//!   (`DriftController::set_target`), so nothing clicks, and the walk's slowing end lets it follow without overshooting
//!   (0.1 ms in the model of the loop). A goal set in one step drove the fill 1.4 ms under it for 10 s: the PI loop runs
//!   at its 1500 ppm limit, winds its integral up as it nears the goal and carries the fill past, at the moment the new
//!   target is least proven. A steady walk overshoots too when it stops (0.8 ms at 0.25 ms/s).
//! - **Raised at once:** an underrun (the device ran dry, or a deficit) puts it back to the fixed target, and the
//!   pipeline's zeros fill the device to it at that block, as they fill it to any target. An underrun under the fixed
//!   target is a **strike** (the fixed target might have covered it: a missed wake of 10-15 ms is what its spare block is
//!   for). Its need (its dip, and at least how far under the fixed target it ran) holds the target up for
//!   [`STRIKE_HOLD_S`] after the first strike, twice that after the second; after [`MAX_STRIKES`] the target stays fixed
//!   for the run. Without that memory, hiccups rarer than the window came back each time it had forgotten the last (a
//!   wake 15 ms late every 150 s underran 7 times in 1000 s, where the fixed target never did). With it, a run underruns
//!   at most [`MAX_STRIKES`] times more than with the fixed target: the lower target's price is that a rare hiccup the
//!   spare block would have covered costs one.
//! - It starts at the fixed target and never goes over it: the fixed target's spare block is what bursts need; a stall
//!   longer than that underruns once either way, and a target raised past it would carry the stall's length as latency
//!   for minutes. The deficit and drop thresholds follow it (they're the pipeline's, on its target).
//!
//! Real-time: fixed arrays, O(1) per block, O([`WINDOW_S`]) once a second; no allocation and no panic paths.

use nf_core::{BLOCK, SAMPLE_RATE};

/// Seconds of dips the need is taken over, and how long after a move is decided before the next can be: long enough for a
/// drifting cable's bursts to pass through their phases against the device (33 s at 300 ppm) a few times.
pub(crate) const WINDOW_S: usize = 120;
/// Over the need, in samples (1 ms): the jitter the window may not have seen.
pub(crate) const HEADROOM: usize = 48;
/// A lowering smaller than this (1 ms) isn't worth a move.
pub(crate) const HYSTERESIS: usize = 48;
/// Blocks after the mean starts again before its dips count (3 s: three of its time constants).
pub(crate) const WARM_UP: u32 = 300;
/// How long the first strike's need holds the target up (10 minutes); each strike after it holds twice as long as the one
/// before.
pub(crate) const STRIKE_HOLD_S: usize = 600;
/// Strikes in a run before the target stays fixed for the rest of it.
pub(crate) const MAX_STRIKES: u32 = 3;
/// The walk down: at most this many samples a second (0.5 ms/s: 500 ppm of slips on top of the drift, a third of the
/// loop's limit), a tenth of the way left (an exponential approach, 10 s to 1/e), and at least one sample.
pub(crate) const RAMP_MAX_STEP: usize = 24;
const RAMP_FRACTION: usize = 10;
/// The longest walk, in seconds: from the fixed target to the floor + the headroom (a block less the headroom: 9 ms).
pub(crate) const RAMP_MAX_S: usize = ramp_seconds(BLOCK - HEADROOM);
/// The mean's time constant.
const MEAN_TAU_S: f64 = 1.0;

/// One second's step of the walk down, `left` samples from the goal: a tenth of the way, from one sample to
/// [`RAMP_MAX_STEP`], and never past the goal.
#[inline]
pub(crate) const fn ramp_step(left: usize) -> usize {
    let step = left / RAMP_FRACTION;
    let step = if step < 1 {
        1
    } else if step > RAMP_MAX_STEP {
        RAMP_MAX_STEP
    } else {
        step
    };
    if step < left { step } else { left }
}

/// Seconds the walk takes over `distance` samples.
const fn ramp_seconds(mut distance: usize) -> usize {
    let mut s = 0;
    while distance > 0 {
        distance -= ramp_step(distance);
        s += 1;
    }
    s
}

/// The output target, moving with the delivery.
#[derive(Clone, Debug)]
pub(crate) struct AdaptiveTarget {
    /// One period + the margin, and the fixed target (one period + one block + the margin), in samples.
    floor: usize,
    fixed: usize,
    /// The target now, and where it is walking down to (at or under it).
    target: usize,
    goal: usize,
    /// The measured fill's mean (once warm), its coefficient per block, the blocks left before dips count and their sum
    /// meanwhile (the mean's seed), and whether the fill measured is the clock's (a change of source starts it again).
    mean: Option<f64>,
    alpha: f64,
    warm: u32,
    warm_sum: f64,
    clocked: bool,
    /// This second's largest dip, and the last seconds' (a ring, oldest overwritten).
    second_max: f32,
    maxima: [f32; WINDOW_S],
    held: usize,
    next: usize,
    /// Seconds since the last move was decided (a lowering, or an underrun's raise).
    since_move_s: usize,
    /// Strikes so far, and the need they hold the target up by (samples) for the seconds left.
    strikes: u32,
    strike_need: f32,
    strike_left_s: usize,
}

impl AdaptiveTarget {
    /// `floor`: one device period + the margin; `fixed`: the engine's fixed target (where it starts), in samples.
    pub(crate) fn new(floor: usize, fixed: usize) -> Self {
        let fixed = fixed.max(floor);
        Self {
            floor,
            fixed,
            target: fixed,
            goal: fixed,
            mean: None,
            alpha: 1.0 - (-(BLOCK as f64 / SAMPLE_RATE as f64) / MEAN_TAU_S).exp(),
            warm: WARM_UP,
            warm_sum: 0.0,
            clocked: false,
            second_max: 0.0,
            maxima: [0.0; WINDOW_S],
            held: 0,
            next: 0,
            since_move_s: 0,
            strikes: 0,
            strike_need: 0.0,
            strike_left_s: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> usize {
        self.target
    }

    #[cfg(test)]
    pub(crate) fn goal(&self) -> usize {
        self.goal
    }

    /// Strikes so far in the run: underruns under the fixed target.
    #[cfg(test)]
    pub(crate) fn strikes(&self) -> u32 {
        self.strikes
    }

    /// The largest dip over the window and this second, in samples (0 before any).
    pub(crate) fn need(&self) -> f32 {
        self.maxima.iter().take(self.held).copied().fold(self.second_max, f32::max)
    }

    /// An ordinary block's fill, read before its write (samples), by the device's clock (`clocked`: frames written less
    /// its position now) or by its padding: its dip under the mean is measured.
    #[inline]
    pub(crate) fn observe(&mut self, fill: f64, clocked: bool) {
        if let Some(dip) = self.dip(fill, clocked) {
            self.note(dip as f32);
        }
    }

    /// This block's dip, once the mean is warm; the mean takes the block either way.
    #[inline]
    fn dip(&mut self, fill: f64, clocked: bool) -> Option<f64> {
        if !fill.is_finite() {
            return None;
        }
        if clocked != self.clocked {
            self.clocked = clocked;
            self.reseed();
        }
        if self.warm > 0 {
            // Warming up: the plain mean of these blocks seeds the low-pass, with no start-up transient in it.
            self.warm_sum += fill;
            self.warm -= 1;
            if self.warm == 0 {
                self.mean = Some(self.warm_sum / f64::from(WARM_UP));
            }
            return None;
        }
        let mean = self.mean.map_or(fill, |m| m + self.alpha * (fill - m));
        self.mean = Some(mean);
        Some(mean - fill)
    }

    #[inline]
    fn note(&mut self, dip: f32) {
        if dip > self.second_max {
            self.second_max = dip;
        }
    }

    /// A correction moved the fill by whole blocks (zeros, or a drop): the mean starts again.
    #[inline]
    pub(crate) fn reseed(&mut self) {
        self.mean = None;
        self.warm = WARM_UP;
        self.warm_sum = 0.0;
    }

    /// An underrun at a block whose fill was `fill`: the target goes back to the fixed one at once, the dip goes into the
    /// window, and the mean starts again (the zeros that follow move the fill). Under the fixed target it's a strike: its
    /// need holds the target up for the strike's hold, and the last strike keeps it fixed. Returns the target when it moved.
    pub(crate) fn underrun(&mut self, fill: f64, clocked: bool) -> Option<usize> {
        // Without a warm mean, as late as the target allowed for.
        let dip = self.dip(fill, clocked).map_or(self.target as f64, |d| d.max(0.0)) as f32;
        self.note(dip);
        self.reseed();
        self.since_move_s = 0;
        self.goal = self.fixed;
        if self.target >= self.fixed {
            // What the fixed target gets anyway: no strike.
            return None;
        }
        self.strikes = self.strikes.saturating_add(1);
        // At least as far under the fixed target as it ran: the next lowering stops a headroom short of where this one ran
        // dry.
        let need = dip.max(self.target.saturating_sub(self.floor) as f32);
        self.strike_need = if self.strike_left_s > 0 { self.strike_need.max(need) } else { need };
        self.strike_left_s = STRIKE_HOLD_S.saturating_mul(1 << self.strikes.saturating_sub(1).min(8));
        self.target = self.fixed;
        Some(self.fixed)
    }

    /// Once a second: the second's largest dip into the window; a new goal when a whole window since the last move says
    /// the target can come down; and a step of the walk to the goal. Returns the target when it moved.
    pub(crate) fn second(&mut self) -> Option<usize> {
        if let Some(slot) = self.maxima.get_mut(self.next) {
            *slot = self.second_max;
        }
        self.next = (self.next + 1) % WINDOW_S;
        self.held = (self.held + 1).min(WINDOW_S);
        self.second_max = 0.0;
        self.since_move_s = self.since_move_s.saturating_add(1);
        if self.strike_left_s > 0 {
            self.strike_left_s -= 1;
            if self.strike_left_s == 0 {
                self.strike_need = 0.0;
            }
        }
        if self.strikes < MAX_STRIKES && self.since_move_s >= WINDOW_S {
            let need = self.need().max(self.strike_need).max(0.0).ceil() as usize;
            let wanted = (self.floor + need + HEADROOM).min(self.fixed);
            if wanted + HYSTERESIS <= self.goal {
                self.goal = wanted;
                self.since_move_s = 0;
            }
        }
        (self.target > self.goal).then(|| {
            self.target -= ramp_step(self.target - self.goal);
            self.target
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: usize = 480 + 96;
    const FIXED: usize = 480 + 480 + 96;

    /// `seconds` of blocks whose fill (by the clock) is 5000 plus `offset(k)`; the targets it moved to.
    fn blocks(a: &mut AdaptiveTarget, seconds: usize, offset: impl Fn(usize) -> f64) -> Vec<usize> {
        let mut moves = Vec::new();
        for s in 0..seconds {
            for b in 0..100 {
                a.observe(5000.0 + offset(s * 100 + b), true);
            }
            if let Some(t) = a.second() {
                moves.push(t);
            }
        }
        moves
    }

    fn near(a: usize, b: usize, tol: usize) -> bool {
        a.abs_diff(b) <= tol
    }

    /// The walk: a tenth of the way, 1 to 24 samples a second, never past the goal; the longest (9 ms) takes under a minute.
    #[test]
    fn the_walk_slows_to_its_end() {
        let steps = [432, 240, 239, 35, 9, 1, 0].map(ramp_step);
        assert_eq!(steps, [24, 24, 23, 3, 1, 1, 0]);
        assert_eq!(RAMP_MAX_S, 53, "9 ms in 53 s");
    }

    /// Steady delivery: nothing moves for a window, then the target walks down to the floor + the largest dip + the
    /// headroom, never faster than the walk's limit and slowing to its end, and stays.
    #[test]
    fn steady_delivery_lowers_the_target_after_a_window() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        assert_eq!(a.target(), FIXED, "starts at the fixed target");
        // 1 ms of jitter around the fill's mean.
        let jitter = |k: usize| if k % 7 == 0 { -48.0 } else { 8.0 };
        let early = blocks(&mut a, WINDOW_S - 1, jitter);
        assert!(early.is_empty(), "{early:?}");
        let moves = blocks(&mut a, 3 * WINDOW_S, jitter);
        assert!(near(a.goal(), FLOOR + 48 + HEADROOM, 1), "to the floor + the dip + the headroom: {}", a.goal());
        assert_eq!(moves.last().copied(), Some(a.goal()), "walked all the way");
        assert!(moves.len() > 30 && moves.len() <= RAMP_MAX_S, "a walk: {moves:?}");
        let path: Vec<usize> = std::iter::once(FIXED).chain(moves.iter().copied()).collect();
        let steps: Vec<usize> = path.windows(2).map(|w| w[0] - w[1]).collect();
        assert!(steps.iter().all(|&s| (1..=RAMP_MAX_STEP).contains(&s)), "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] <= w[0]), "slowing: {steps:?}");
        assert!(a.need() > 47.0 && a.need() < 48.5, "{}", a.need());
    }

    /// The cables' bursts (blocks 10 ms late every other one) need 5 ms: lowered to the floor + 5 ms + the headroom, half
    /// a block under the fixed target. One wake in five missed (8 ms late against the mean) holds it higher. A start whose
    /// first fill is far off the mean doesn't count (the mean's warm-up).
    #[test]
    fn bursts_hold_the_target_up_by_their_lateness() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut a, 2 * WINDOW_S, |k| if k % 2 == 0 { 240.0 } else { -240.0 });
        assert!(near(a.target(), FLOOR + 240 + HEADROOM, 2), "{}", a.target());
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut a, 2 * WINDOW_S, |k| if k % 5 == 0 { -384.0 } else { 96.0 });
        assert!(near(a.target(), FLOOR + 384 + HEADROOM, 4), "{}", a.target());
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut a, WINDOW_S + RAMP_MAX_S, |k| if k == 0 { 2000.0 } else { 0.0 });
        assert!(near(a.target(), FLOOR + HEADROOM, 8), "{}: a first fill 40 ms off moves the warm-up's mean by 0.1 ms", a.target());
    }

    /// An underrun under the fixed target is a strike: back to the fixed target at once, and its need holds the target up
    /// for 10 minutes (a window later it comes down only as far as that need allows), then it's forgotten. A second strike
    /// holds for 20 minutes, and the third keeps the target fixed for the run. An underrun at the fixed target isn't one.
    #[test]
    fn strikes_hold_the_target_up_for_longer_each_time() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut a, WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM);
        // A block 6 ms later than the others.
        assert_eq!(a.underrun(5000.0 - 288.0, true), Some(FIXED), "back to the fixed target");
        assert_eq!(a.strikes(), 1);
        let moves = blocks(&mut a, WINDOW_S - 1, |_| 0.0);
        assert!(moves.is_empty(), "{moves:?}");
        // A window on, down to what the strike's dip needs, and no further while its hold lasts.
        blocks(&mut a, RAMP_MAX_S, |_| 0.0);
        assert!(near(a.target(), FLOOR + 288 + HEADROOM, 3), "{}", a.target());
        blocks(&mut a, STRIKE_HOLD_S - WINDOW_S - RAMP_MAX_S - 1, |_| 0.0);
        assert!(near(a.target(), FLOOR + 288 + HEADROOM, 3), "held for 10 minutes: {}", a.target());
        blocks(&mut a, WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM, "then forgotten");
        // The second strike holds for 20 minutes; a third keeps it fixed.
        assert_eq!(a.underrun(5000.0 - 100.0, true), Some(FIXED));
        blocks(&mut a, 2 * STRIKE_HOLD_S - 1, |_| 0.0);
        assert!(a.target() > FLOOR + 100 + HEADROOM - 3 && a.target() < FIXED, "held for 20 minutes: {}", a.target());
        blocks(&mut a, WINDOW_S + RAMP_MAX_S + 1, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM);
        assert_eq!(a.underrun(5000.0 - 100.0, true), Some(FIXED));
        assert_eq!(a.strikes(), MAX_STRIKES);
        assert_eq!(blocks(&mut a, 3 * STRIKE_HOLD_S, |_| 0.0), [], "fixed for the run");
        // A block later than a whole block, at the fixed target: no strike; it stays there, and a window passes before it
        // can come down, then only as far as the dip allows (here not at all) until the dip leaves the window.
        let mut b = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut b, 4, |_| 0.0);
        assert_eq!(b.underrun(5000.0 - 700.0, true), None, "at the fixed target already");
        assert_eq!(b.underrun(f64::NAN, true), None);
        assert_eq!((b.target(), b.strikes()), (FIXED, 0));
        assert_eq!(blocks(&mut b, WINDOW_S, |_| 0.0), [], "the dip holds it");
        blocks(&mut b, WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(b.target(), FLOOR + HEADROOM, "and then it's gone");
        // An underrun at the fixed target just before the window ends puts the lowering off for another window.
        let mut c = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut c, WINDOW_S - 1, |_| 0.0);
        assert_eq!(c.underrun(5000.0 - 100.0, true), None);
        assert_eq!(blocks(&mut c, 2, |_| 0.0), [], "no walk");
        assert_eq!((c.goal(), c.strikes()), (FIXED, 0));
    }

    /// A hiccup that comes back less often than the window (a dip of 15 ms every 150 s, which the fixed target covers):
    /// the first after the target has come down underruns, and its need holds the target at the fixed one through the
    /// next hiccups for its hold; each time the hold runs out between hiccups the next strikes again, and the run's
    /// strikes stop at [`MAX_STRIKES`].
    #[test]
    fn rare_hiccups_strike_at_most_max_strikes_times() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        let mut underruns = 0;
        for s in 0..7200 {
            for b in 0..100 {
                if s % 150 == 149 && b == 50 {
                    // The fixed target covers it; anything under it doesn't.
                    if a.target() < FIXED {
                        underruns += 1;
                        a.underrun(5000.0 - 720.0, true);
                    } else {
                        a.observe(5000.0 - 720.0, true);
                    }
                } else {
                    a.observe(5000.0, true);
                }
            }
            a.second();
        }
        assert_eq!((underruns, a.strikes()), (MAX_STRIKES, MAX_STRIKES));
        assert_eq!(a.target(), FIXED);
    }

    /// A switch between the clock's fill and the padding starts the mean again: the step between them isn't a dip.
    #[test]
    fn a_change_of_measure_starts_the_mean_again() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        blocks(&mut a, 10, |_| 0.0);
        for _ in 0..100 {
            a.observe(4000.0, false);
        }
        assert_eq!(a.need(), 0.0);
    }

    /// The per-block work allocates nothing.
    #[test]
    fn it_never_allocates() {
        let mut a = AdaptiveTarget::new(FLOOR, FIXED);
        let ((), allocs, frees) = crate::test_alloc::count(|| {
            for k in 0..60_000usize {
                a.observe(FIXED as f64 + (k % 13) as f64, k % 5000 != 0);
                if k % 100 == 99 {
                    let _ = a.second();
                }
                if k == 40_000 {
                    let _ = a.underrun(0.0, true);
                }
            }
        });
        assert_eq!((allocs, frees), (0, 0));
    }
}
