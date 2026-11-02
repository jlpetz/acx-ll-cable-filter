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
//! - **Two windows before the first lowering:** until proven, a window that would lower the target starts a **trial**
//!   instead ([`AdaptiveTarget::on_trial`]): the target holds where it is (at the start, the fixed target) for a second
//!   window. If that window passes without an underrun and allows a lowering too, the target is proven and comes down on
//!   that window's need, as any lowering does; if its need doesn't allow one, the trial has failed, and the next window
//!   that allows one starts another. Proven, every later lowering goes at once. A strike unproves it (the next lowering
//!   is trialled again), and any underrun ends a trial. Why: a window is what has seen the delivery, and the first has seen
//!   the least. The 30-minute soak on the cables (TODO 13's detail, "Soak") walked from 14 to 9.9 ms in 9 s on its first
//!   window's need (heading for 6.8) and ran the device dry there at about 122 s, its queue at 10.6 ms: a block 8 ms or
//!   more later than the rest (the queue less the period the device holds; about 10 in the simulations), a missed wake.
//!   A first stop short of the fixed target doesn't cover that: in the soak's conditions one halfway (10.1-10.4 ms) or at
//!   a period + a block (12 ms) ran short with a block 10 ms late while it held (a block more than a block late, with the
//!   fill under a period, is a deficit), where the fixed target rode out blocks up to 13.5 ms late. Held at the fixed
//!   target, the trial rides out what the fixed target does, and a hiccup in it fails the trial rather than costing a
//!   strike (a wake 15 ms late every 150 s never lets the target down, where the single stage struck three times). The
//!   end point is the same, a window later: settled by [`SETTLE_MAX_S`] (`pipeline/tests/exclusive.rs`,
//!   `the_first_lowering_waits_for_a_second_window`). The price is the fixed target's latency for minutes 2 to 4 of a run
//!   (ours, 2026-11-02).
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
/// The longest a run's target takes to settle, in seconds, when nothing strikes and the need holds still: the window, the
/// trial's window, and the walk down (at most [`RAMP_MAX_S`]). A need that falls between windows (a dip leaving the window),
/// or a trial that fails, moves it later.
pub(crate) const SETTLE_MAX_S: usize = 2 * WINDOW_S + RAMP_MAX_S;
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
    /// A trial has passed since the start or the last strike, so every lowering goes at once; and a trial is on (the target
    /// holds for a window).
    proven: bool,
    trial: bool,
    /// Trials before the first lowering (always, but in the simulations that compare the old single stage).
    trials: bool,
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
            proven: false,
            trial: false,
            trials: true,
        }
    }

    /// The old single-stage lowering (every lowering at once, on its window alone, the first and after a strike too), for
    /// the simulations that compare the two.
    #[cfg(test)]
    pub(crate) fn single_stage(mut self) -> Self {
        self.trials = false;
        self
    }

    /// The target holds for the trial's window, which proves it: the lowering is still to come (`latency --settle` and
    /// `quality --settle` wait for it).
    pub(crate) fn on_trial(&self) -> bool {
        self.trial
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
        // A trial is over: only a whole window without an underrun passes one.
        self.trial = false;
        if self.target >= self.fixed {
            // What the fixed target gets anyway: no strike.
            return None;
        }
        self.strikes = self.strikes.saturating_add(1);
        // The next lowering is unproven again: a trial first.
        self.proven = false;
        // At least as far under the fixed target as it ran: the next lowering stops a headroom short of where this one ran
        // dry.
        let need = dip.max(self.target.saturating_sub(self.floor) as f32);
        self.strike_need = if self.strike_left_s > 0 { self.strike_need.max(need) } else { need };
        self.strike_left_s = STRIKE_HOLD_S.saturating_mul(1 << self.strikes.saturating_sub(1).min(8));
        self.target = self.fixed;
        Some(self.fixed)
    }

    /// Once a second: the second's largest dip into the window; a new goal when a whole window since the last move says
    /// the target can come down (until proven, a trial first, and the goal at its end: the module notes); and a step of the
    /// walk to the goal. Returns the target when it moved.
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
            let lower = wanted + HYSTERESIS <= self.goal;
            if self.trial {
                // The trial's window has passed without an underrun (one would have ended it). Passed if it allows a lowering
                // too, on its own need as any lowering is; failed if not, and the next window that allows one starts another.
                self.trial = false;
                if lower {
                    (self.goal, self.proven, self.since_move_s) = (wanted, true, 0);
                }
            } else if lower {
                if self.proven || !self.trials {
                    self.goal = wanted;
                } else {
                    self.trial = true;
                }
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

    fn target() -> AdaptiveTarget {
        AdaptiveTarget::new(FLOOR, FIXED)
    }

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

    /// The walk: a tenth of the way, 1 to 24 samples a second, never past the goal; the longest (9 ms) takes under a minute,
    /// and a run settles within five minutes of its start when nothing strikes.
    #[test]
    fn the_walk_slows_to_its_end() {
        let steps = [432, 240, 239, 35, 9, 1, 0].map(ramp_step);
        assert_eq!(steps, [24, 24, 23, 3, 1, 1, 0]);
        assert_eq!(RAMP_MAX_S, 53, "9 ms in 53 s");
        assert_eq!(SETTLE_MAX_S, 2 * WINDOW_S + RAMP_MAX_S, "two windows, and the walk down in at most 53 s");
    }

    /// Steady delivery: nothing moves for a window; then a trial holds the target at the fixed one for a second window;
    /// then it walks down to the floor + the largest dip + the headroom, never faster than the walk's limit and slowing to
    /// its end, and stays.
    #[test]
    fn steady_delivery_lowers_the_target_after_a_trial() {
        let mut a = target();
        assert_eq!(a.target(), FIXED, "starts at the fixed target");
        // 1 ms of jitter around the fill's mean.
        let jitter = |k: usize| if k % 7 == 0 { -48.0 } else { 8.0 };
        let early = blocks(&mut a, WINDOW_S - 1, jitter);
        assert!(early.is_empty() && !a.on_trial(), "{early:?}");
        let held = blocks(&mut a, WINDOW_S, jitter);
        assert!(held.is_empty() && a.on_trial() && a.goal() == FIXED, "a trial, at the fixed target: {held:?}");
        let end = FLOOR + 48 + HEADROOM;
        let moves = blocks(&mut a, WINDOW_S, jitter);
        assert!(!a.on_trial());
        assert!(near(a.goal(), end, 1), "to the floor + the dip + the headroom: {}", a.goal());
        assert_eq!(moves.last().copied(), Some(a.goal()), "walked all the way");
        assert!(moves.len() > 30 && moves.len() <= RAMP_MAX_S, "a walk: {moves:?}");
        let path: Vec<usize> = std::iter::once(FIXED).chain(moves.iter().copied()).collect();
        let steps: Vec<usize> = path.windows(2).map(|w| w[0] - w[1]).collect();
        assert!(steps.iter().all(|&s| (1..=RAMP_MAX_STEP).contains(&s)), "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] <= w[0]), "slowing: {steps:?}");
        assert!(a.need() > 47.0 && a.need() < 48.5, "{}", a.need());
        assert!(blocks(&mut a, 2 * WINDOW_S, jitter).is_empty(), "and stays");
        // A passed trial lowers on its own window's need, as any lowering does: a dip of 4 ms in the first window (at 60 s)
        // is past, and the target comes down to the end point; one in the trial's window (at 130 s) holds it 4 ms up, and
        // once the dip has left the window it comes the rest of the way at once (proven).
        let mut b = target();
        blocks(&mut b, 2 * WINDOW_S + RAMP_MAX_S, |k| if k == 6000 { -200.0 } else { 0.0 });
        assert_eq!((b.target(), b.on_trial()), (FLOOR + HEADROOM, false), "dip at 60 s");
        let mut b = target();
        blocks(&mut b, 2 * WINDOW_S + RAMP_MAX_S, |k| if k == 13_000 { -200.0 } else { 0.0 });
        assert!(near(b.target(), FLOOR + 200 + HEADROOM, 4) && !b.on_trial(), "dip at 130 s: {}", b.target());
        blocks(&mut b, WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(b.target(), FLOOR + HEADROOM, "dip at 130 s, then at once");
        // A trial whose own window doesn't allow a lowering fails (a block 9 ms late at 130 s: the target stays fixed), and the
        // next window that allows one, once the dip has left it, starts another; a window later the target comes down.
        let mut f = target();
        let moves = blocks(&mut f, 2 * WINDOW_S + 5, |k| if k == 13_000 { -440.0 } else { 0.0 });
        assert!(moves.is_empty() && !f.on_trial(), "failed: {moves:?}");
        blocks(&mut f, 10, |_| 0.0);
        assert!(f.on_trial(), "another trial once the dip has left the window");
        blocks(&mut f, WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!((f.target(), f.on_trial()), (FLOOR + HEADROOM, false));
        // The old single stage, for comparison: down after the first window.
        let mut c = target().single_stage();
        blocks(&mut c, WINDOW_S + RAMP_MAX_S, jitter);
        assert!(near(c.target(), end, 1) && !c.on_trial(), "{}", c.target());
    }

    /// The cables' bursts (blocks 10 ms late every other one) need 5 ms: lowered to the floor + 5 ms + the headroom, half
    /// a block under the fixed target. One wake in five missed (8 ms late against the mean) holds it higher. A start whose
    /// first fill is far off the mean doesn't count (the mean's warm-up).
    #[test]
    fn bursts_hold_the_target_up_by_their_lateness() {
        let settled = 2 * WINDOW_S + RAMP_MAX_S;
        let mut a = target();
        blocks(&mut a, settled, |k| if k % 2 == 0 { 240.0 } else { -240.0 });
        assert!(near(a.target(), FLOOR + 240 + HEADROOM, 2), "{}", a.target());
        let mut a = target();
        blocks(&mut a, settled, |k| if k % 5 == 0 { -384.0 } else { 96.0 });
        assert!(near(a.target(), FLOOR + 384 + HEADROOM, 4), "{}", a.target());
        let mut a = target();
        blocks(&mut a, settled, |k| if k == 0 { 2000.0 } else { 0.0 });
        assert!(near(a.target(), FLOOR + HEADROOM, 8), "{}: a first fill 40 ms off moves the warm-up's mean by 0.1 ms", a.target());
    }

    /// An underrun under the fixed target is a strike: back to the fixed target at once, unproven again (a window later a
    /// trial, and a window after that down to what the strike's need allows, no further), and its need holds the target
    /// up for 10 minutes, then it's forgotten. A second strike holds for 20 minutes, and the third keeps the target fixed
    /// for the run. An underrun at the fixed target isn't one; on trial, it ends the trial.
    #[test]
    fn strikes_hold_the_target_up_for_longer_each_time() {
        let mut a = target();
        blocks(&mut a, 2 * WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM);
        // A block 6 ms later than the others.
        assert_eq!(a.underrun(5000.0 - 288.0, true), Some(FIXED), "back to the fixed target");
        assert_eq!(a.strikes(), 1);
        let moves = blocks(&mut a, 2 * WINDOW_S - 1, |_| 0.0);
        assert!(moves.is_empty() && a.on_trial(), "a window, then a trial: {moves:?}");
        let held = FLOOR + 288 + HEADROOM;
        blocks(&mut a, 1 + RAMP_MAX_S, |_| 0.0);
        assert!(near(a.target(), held, 3) && !a.on_trial(), "{}", a.target());
        blocks(&mut a, STRIKE_HOLD_S - 2 * WINDOW_S - RAMP_MAX_S - 1, |_| 0.0);
        assert!(near(a.target(), held, 3), "held for 10 minutes: {}", a.target());
        // Proven by then: the next lowering goes at once.
        blocks(&mut a, RAMP_MAX_S + 1, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM, "then forgotten");
        // The second strike holds for 20 minutes; a third keeps it fixed.
        assert_eq!(a.underrun(5000.0 - 100.0, true), Some(FIXED));
        blocks(&mut a, 2 * STRIKE_HOLD_S - 1, |_| 0.0);
        assert!(a.target() > FLOOR + 100 + HEADROOM - 3 && a.target() < FIXED, "held for 20 minutes: {}", a.target());
        blocks(&mut a, WINDOW_S + RAMP_MAX_S + 1, |_| 0.0);
        assert_eq!(a.target(), FLOOR + HEADROOM);
        assert_eq!(a.underrun(5000.0 - 100.0, true), Some(FIXED));
        assert_eq!(a.strikes(), MAX_STRIKES);
        assert_eq!(blocks(&mut a, 3 * STRIKE_HOLD_S, |_| 0.0), Vec::<usize>::new(), "fixed for the run");
        // A block later than a whole block, at the fixed target: no strike; it stays there, and a window passes before it
        // can come down, then only as far as the dip allows (here not at all) until the dip leaves the window.
        let mut b = target();
        blocks(&mut b, 4, |_| 0.0);
        assert_eq!(b.underrun(5000.0 - 700.0, true), None, "at the fixed target already");
        assert_eq!(b.underrun(f64::NAN, true), None);
        assert_eq!((b.target(), b.strikes()), (FIXED, 0));
        assert_eq!(blocks(&mut b, WINDOW_S, |_| 0.0), Vec::<usize>::new(), "the dip holds it");
        blocks(&mut b, 2 * WINDOW_S + RAMP_MAX_S, |_| 0.0);
        assert_eq!(b.target(), FLOOR + HEADROOM, "and then it's gone");
        // An underrun at the fixed target just before the window ends puts the lowering off for another window.
        let mut c = target();
        blocks(&mut c, WINDOW_S - 1, |_| 0.0);
        assert_eq!(c.underrun(5000.0 - 100.0, true), None);
        assert_eq!(blocks(&mut c, 2, |_| 0.0), Vec::<usize>::new(), "no walk");
        assert_eq!((c.goal(), c.strikes(), c.on_trial()), (FIXED, 0, false));
        // An underrun on trial (at the fixed target, so no strike) ends the trial, and a window on another starts.
        let mut t = target();
        blocks(&mut t, WINDOW_S + 30, |_| 0.0);
        assert!(t.on_trial());
        assert_eq!(t.underrun(5000.0 - 100.0, true), None);
        assert!(!t.on_trial() && t.strikes() == 0);
        assert_eq!(blocks(&mut t, WINDOW_S, |_| 0.0), Vec::<usize>::new(), "a window on, another trial");
        assert!(t.on_trial());
    }

    /// A dip of 15 ms (which the fixed target covers; anything under it doesn't) every `every_s` seconds for two hours: the
    /// underruns, the strikes and the target at the end.
    fn hiccups(mut a: AdaptiveTarget, every_s: usize) -> (u32, u32, usize) {
        let mut underruns = 0;
        for s in 0..7200 {
            for b in 0..100 {
                if s % every_s == every_s - 1 && b == 50 {
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
        (underruns, a.strikes(), a.target())
    }

    /// Hiccups that come back less often than the window. Every 150 s, one lands in each trial's window and fails it, so the
    /// target never comes down and nothing underruns; the old single stage, lowered on each clean window, struck three times.
    /// Every 300 s (rarer than the two windows), the first after the target has come down underruns, and its need holds the
    /// target at the fixed one through the next hiccups for its hold; each time the hold runs out between hiccups the next
    /// strikes again, and the run's strikes stop at [`MAX_STRIKES`].
    #[test]
    fn rare_hiccups_strike_at_most_max_strikes_times() {
        assert_eq!(hiccups(target(), 150), (0, 0, FIXED), "every 150 s: no trial passes");
        assert_eq!(hiccups(target().single_stage(), 150), (MAX_STRIKES, MAX_STRIKES, FIXED), "the single stage");
        assert_eq!(hiccups(target(), 300), (MAX_STRIKES, MAX_STRIKES, FIXED), "every 300 s");
    }

    /// A switch between the clock's fill and the padding starts the mean again: the step between them isn't a dip.
    #[test]
    fn a_change_of_measure_starts_the_mean_again() {
        let mut a = target();
        blocks(&mut a, 10, |_| 0.0);
        for _ in 0..100 {
            a.observe(4000.0, false);
        }
        assert_eq!(a.need(), 0.0);
    }

    /// The per-block work allocates nothing.
    #[test]
    fn it_never_allocates() {
        let mut a = target();
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
