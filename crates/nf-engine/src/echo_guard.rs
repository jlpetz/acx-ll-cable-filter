//! The echo-path guard (engine.md, "The echo-path guard"; TODO 10 and 13). AEC3 cancels an echo only once it reaches the mic
//! after the reference sample served with it, so the reference's lead (the mic block's capture time less the tap time of the
//! reference served with it, reference.rs) has to be under the echo path's own delay: from the loopback's tap through the
//! speakers' output latency and the air to the mic. The lead is what the loopback's delivery needs: a loopback period and
//! some, 12-14 ms on the cables with the shared mic and 18 with the exclusive one, whose blocks come fresher. Most speakers'
//! echo comes later than that, but low-latency ones may not, and then the echo comes before its reference: AEC3 can't place
//! it (its delay estimate reads 0) and cancels little (TODO 10's sweep: an ERLE of 6-8 dB against 16-18).
//!
//! The guard watches for that once a second and, when it sees it, holds the mic back: the pipeline then processes each block
//! `hold` samples after it could, which gives the loopback that much longer to deliver, so the reference served with it is
//! that much newer (the effective lead is the reference loop's less the hold; the loop itself sees nothing). The hold is in
//! whole loopback periods: the loopback delivers a period at a time, so a hold of whole periods keeps the phase of the
//! blocks' takes against its packets, and with it the reference's spare (a first version held by the sample, 754 on 480-sample
//! packets, which brought one packet more or two by the take as the phase fell; live it brought one, and every take came up
//! 130 samples short until the reference's target rose 5.7 ms: TODO 10's detail). It costs its length in latency, so it comes
//! only for an echo seen that AEC3 can't place:
//! - **An echo:** the mic's energy envelope (a value a block) follows the far end's: their correlation over the last 5 s, at
//!   the best of a few lags either way, is [`ECHO_R`] or more. Headphones, or the near talker alone, show none.
//! - **That AEC3 can't place:** with the far end playing, AEC3's ERLE under [`LOW_ERLE_DB`] and its delay estimate at most a
//!   block of its own ([`PLACED_MS`]). It reads in 4 ms steps less its headroom, so a short echo that it does place reads 0
//!   as well: only the ERLE tells them apart.
//!
//! [`SUSPECT_S`] such seconds in a row (once the run is [`WARMUP_S`] old and the far end has played for [`CONVERGE_S`], so a
//! placeable echo has been cancelled by then) start a **trial** of a one-period hold. AEC3 needs [`SETTLE_S`] seconds of the
//! far end to find the echo again after the jump; the next [`JUDGE_S`] seconds of it judge. A second is good when AEC3's ERLE
//! is [`GAIN_DB`] over the suspect seconds' and at [`LOW_ERLE_DB`] or more, or when AEC3 has placed the echo (its delay
//! estimate over [`PLACED_MS`]: an echo before its reference always reads 0). Placing it can cost AEC3 its ERLE for a few
//! seconds: live, an echo 11.7 ms after its reference was cancelled at a delay of 0 at first, and 13 s later AEC3 moved its
//! delay to 8 ms and started its filter over, its ERLE down to 0, in the trial's judging (the first version took that for
//! failure). Half the judged seconds good keeps the hold. Otherwise the next trial holds a period more, as long as the
//! effective lead is still over [`TRIAL_LEAD`] (2 ms) and the hold within [`MAX_HOLD`]; past that the hold goes, and no
//! trial comes for [`COOLDOWN_S`]. Smallest first, so the hold is the least that works: with the exclusive mic's 18 ms lead
//! on 10 ms loopback periods, speakers whose echo comes 9-18 ms after the tap get 10 ms, sooner ones 20. A held run whose
//! echo is again one AEC3 can't cancel tries the next hold, and past the last lets it go; one whose echo goes away
//! ([`NO_ECHO_S`] seconds of the far end with none: headphones put on) lets it go.
//!
//! Pure, and O(1) a block and a second (the correlation: 7 lags over 500 blocks), on the audio thread: no allocation after
//! [`EchoGuard::new`].

use nf_core::stage::AecStats;
use nf_core::telemetry::{ECHO_GUARD_COOLING, ECHO_GUARD_HELD, ECHO_GUARD_TRIAL, ECHO_GUARD_WATCHING};
use nf_core::{BLOCK, Block, SAMPLE_RATE};

/// Samples a ms.
const MS: f64 = SAMPLE_RATE as f64 / 1000.0;
/// The envelopes' length: 5 s of blocks.
const ENV: usize = 500;
/// The lags tried, in blocks, either way: the echo up to 30 ms before its reference as served, or after.
const LAGS: i32 = 3;
/// A block's energy floor, dB: silence reads this.
const FLOOR_DB: f32 = -100.0;
/// The far end plays in a block whose reference RMS is over this (dBFS), and in a second with [`FAR_BLOCKS`] such blocks.
const FAR_DB: f32 = -50.0;
const FAR_BLOCKS: u32 = 30;
/// The envelopes' correlation that shows an echo, and the one under which a held run's echo has gone.
pub(crate) const ECHO_R: f32 = 0.4;
const NO_ECHO_R: f32 = 0.15;
/// AEC3's ERLE under this (dB), the far end playing: it isn't cancelling the echo. And its delay estimate over this (ms): it
/// has placed it (after its reference).
pub(crate) const LOW_ERLE_DB: f32 = 10.0;
pub(crate) const PLACED_MS: f32 = 4.0;
/// Seconds: the run's age before anything counts, and the far end's playing before then.
const WARMUP_S: u32 = 20;
const CONVERGE_S: u32 = 10;
/// Suspect seconds in a row that start a trial.
const SUSPECT_S: u32 = 5;
/// A hold goes no further once the effective lead is this or under (samples): 2 ms. A trial needs it 3 ms or more over that
/// to be worth one ([`MIN_HOLD`]). The most the pipeline holds: 30 ms.
pub(crate) const TRIAL_LEAD: f64 = 2.0 * MS;
pub(crate) const MIN_HOLD: f64 = 3.0 * MS;
pub(crate) const MAX_HOLD: usize = 1_440;
/// Seconds of the far end playing: for AEC3 to find the echo after the jump (a jump costs it 5-10 s: TODO 10), then to judge.
const SETTLE_S: u32 = 8;
const JUDGE_S: u32 = 8;
/// A trial's judged second is good with an ERLE this much over the suspect seconds' (and [`LOW_ERLE_DB`] or more).
const GAIN_DB: f32 = 4.0;
/// A trial the far end doesn't play enough for ends after this many seconds, and the next comes after [`RETRY_S`].
const TRIAL_MAX_S: u32 = 120;
const RETRY_S: u32 = 60;
/// After the last trial didn't help, or a hold let go that couldn't cancel the echo: no trial for 10 minutes.
const COOLDOWN_S: u32 = 600;
/// Held: seconds of the far end playing with no echo seen that let the hold go.
const NO_ECHO_S: u32 = 60;

/// Where the guard is, for the telemetry (`Telemetry::echo_guard`) and the control thread's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum GuardState {
    Watching = ECHO_GUARD_WATCHING,
    Trial = ECHO_GUARD_TRIAL,
    Held = ECHO_GUARD_HELD,
    Cooling = ECHO_GUARD_COOLING,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Watching,
    /// A trial of `periods` loopback periods: far-end seconds settled and judged, the good ones, the suspect seconds' mean
    /// ERLE (from before the first trial), and the trial's age (s).
    Trial {
        periods: usize,
        settled: u32,
        judged: u32,
        good: u32,
        baseline: f32,
        age: u32,
    },
    /// Held at `periods` loopback periods: far-end seconds with no echo in a row, and the ERLE the trials were judged against.
    Held {
        periods: usize,
        quiet: u32,
        baseline: f32,
    },
    /// Seconds left without a trial.
    Cooling(u32),
}

/// The most loopback periods of `period` samples worth holding under the reference's lead `target` (samples): until the
/// effective lead is [`TRIAL_LEAD`] or under, within [`MAX_HOLD`]; 0 when the lead is too short for a hold worth having
/// ([`MIN_HOLD`]).
pub(crate) fn most_periods(target: f64, period: usize) -> usize {
    let period = period.max(1);
    let fit = MAX_HOLD / period;
    if target - TRIAL_LEAD < MIN_HOLD || fit == 0 {
        return 0;
    }
    (((target - TRIAL_LEAD) / period as f64).ceil() as usize).clamp(1, fit)
}

/// The two envelopes, a block's energy (dB) each, over the last [`ENV`] blocks.
struct Envelopes {
    mic: Box<[f32; ENV]>,
    far: Box<[f32; ENV]>,
    /// The next slot, and how many hold a value.
    at: usize,
    len: usize,
}

impl Envelopes {
    fn push(&mut self, mic: f32, far: f32) {
        self.mic[self.at] = mic;
        self.far[self.at] = far;
        self.at = (self.at + 1) % ENV;
        self.len = (self.len + 1).min(ENV);
    }

    /// The best correlation of the mic's envelope with the far end's at a lag of up to [`LAGS`] blocks either way (the mic's
    /// block `k` against the far end's `k - lag`); 0 for less than a second of them, or an envelope that doesn't move.
    fn correlation(&self) -> f32 {
        if self.len < 100 {
            return 0.0;
        }
        // Oldest first: slot `(start + i) % ENV` is the i-th.
        let start = (self.at + ENV - self.len) % ENV;
        let at = |v: &[f32; ENV], i: usize| v[(start + i) % ENV];
        let mut best = 0.0f32;
        for lag in -LAGS..=LAGS {
            let (from, to) = (lag.max(0) as usize, self.len - (-lag).max(0) as usize);
            let n = to.saturating_sub(from) as f64;
            if n < 50.0 {
                continue;
            }
            let (mut sm, mut sf, mut smm, mut sff, mut smf) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for i in from..to {
                let (m, f) = (f64::from(at(&self.mic, i)), f64::from(at(&self.far, (i as i64 - i64::from(lag)) as usize)));
                (sm, sf, smm, sff, smf) = (sm + m, sf + f, smm + m * m, sff + f * f, smf + m * f);
            }
            let (vm, vf) = (smm - sm * sm / n, sff - sf * sf / n);
            if vm > 1e-6 && vf > 1e-6 {
                best = best.max(((smf - sm * sf / n) / (vm * vf).sqrt()) as f32);
            }
        }
        best
    }
}

/// A block's energy, dB (RMS, full scale 1.0), floored.
fn block_db(b: &Block) -> f32 {
    let e = b.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>() / BLOCK as f64;
    if e > 0.0 { ((10.0 * e.log10()) as f32).max(FLOOR_DB) } else { FLOOR_DB }
}

/// The echo-path guard's state for one run.
pub(crate) struct EchoGuard {
    env: Envelopes,
    phase: Phase,
    /// Seconds seen, and those the far end played in.
    seconds: u32,
    far_seconds: u32,
    /// This second's blocks the far end played in.
    far_blocks: u32,
    /// Suspect seconds in a row, and their ERLE's sum.
    suspect: u32,
    erle_sum: f32,
    /// The last second's envelope correlation, for the telemetry.
    pub(crate) correlation: f32,
}

impl EchoGuard {
    /// Allocates the envelopes: not real-time.
    pub(crate) fn new() -> Self {
        Self {
            env: Envelopes { mic: Box::new([FLOOR_DB; ENV]), far: Box::new([FLOOR_DB; ENV]), at: 0, len: 0 },
            phase: Phase::Watching,
            seconds: 0,
            far_seconds: 0,
            far_blocks: 0,
            suspect: 0,
            erle_sum: 0.0,
            correlation: 0.0,
        }
    }

    pub(crate) fn state(&self) -> GuardState {
        match self.phase {
            Phase::Watching => GuardState::Watching,
            Phase::Trial { .. } => GuardState::Trial,
            Phase::Held { .. } => GuardState::Held,
            Phase::Cooling(_) => GuardState::Cooling,
        }
    }

    /// One block, as the chain got it: the mic, and the reference served with it.
    pub(crate) fn block(&mut self, mic: &Block, far: &Block) {
        let far_db = block_db(far);
        if far_db > FAR_DB {
            self.far_blocks += 1;
        }
        self.env.push(block_db(mic), far_db);
    }

    /// Starts a suspect run over.
    fn calm(&mut self) {
        (self.suspect, self.erle_sum) = (0, 0.0);
    }

    /// The second's end: AEC3's estimates now, the reference's lead target and the loopback's period (samples). Returns the
    /// hold to keep, in samples: whole periods, 0 for none.
    pub(crate) fn second(&mut self, aec: Option<AecStats>, target: f64, period: usize) -> usize {
        self.seconds = self.seconds.saturating_add(1);
        let far = std::mem::take(&mut self.far_blocks) >= FAR_BLOCKS;
        self.far_seconds += u32::from(far);
        self.correlation = self.env.correlation();
        let (erle, delay) = aec.map_or((f32::NAN, f32::NAN), |a| (a.erle_db, a.echo_delay_ms));
        let echo = self.correlation >= ECHO_R;
        let placed = delay > PLACED_MS;
        // The far end playing, an echo, and AEC3 neither cancelling nor placing it.
        let suspect = far && echo && erle.is_finite() && erle < LOW_ERLE_DB && !placed;
        let warm = self.seconds >= WARMUP_S && self.far_seconds >= CONVERGE_S;
        if far {
            if suspect && warm {
                self.suspect += 1;
                self.erle_sum += erle;
            } else {
                self.calm();
            }
        }
        let (run, most) = (self.suspect, most_periods(target, period));
        let trial = |periods, baseline| Phase::Trial { periods, settled: 0, judged: 0, good: 0, baseline, age: 0 };
        self.phase = match self.phase {
            Phase::Watching if run >= SUSPECT_S && most > 0 => {
                let baseline = self.erle_sum / run as f32;
                self.calm();
                trial(1, baseline)
            }
            Phase::Watching => Phase::Watching,
            Phase::Trial { periods, settled, judged, good, baseline, age } => {
                let age = age + 1;
                let (settled, judged, good) = if !far {
                    (settled, judged, good)
                } else if settled < SETTLE_S {
                    (settled + 1, judged, good)
                } else {
                    let cancels = erle.is_finite() && erle >= LOW_ERLE_DB && erle >= baseline + GAIN_DB;
                    (settled, judged + 1, good + u32::from(cancels || placed))
                };
                if judged >= JUDGE_S && 2 * good >= judged {
                    self.calm();
                    Phase::Held { periods, quiet: 0, baseline }
                } else if judged >= JUDGE_S {
                    // Not enough: a period more, while one is worth trying.
                    self.calm();
                    if periods < most { trial(periods + 1, baseline) } else { Phase::Cooling(COOLDOWN_S) }
                } else if age >= TRIAL_MAX_S {
                    Phase::Cooling(RETRY_S)
                } else {
                    Phase::Trial { periods, settled, judged, good, baseline, age }
                }
            }
            Phase::Held { periods, quiet, baseline } => {
                let quiet = if !far {
                    quiet
                } else if self.correlation < NO_ECHO_R {
                    quiet + 1
                } else {
                    0
                };
                if quiet >= NO_ECHO_S {
                    self.calm();
                    Phase::Watching
                } else if run >= SUSPECT_S {
                    // AEC3 can't cancel it again (the reference's target rose, or the speakers changed): a period more, or
                    // past the last, none.
                    self.calm();
                    if periods < most { trial(periods + 1, baseline) } else { Phase::Cooling(COOLDOWN_S) }
                } else {
                    Phase::Held { periods, quiet, baseline }
                }
            }
            Phase::Cooling(left) => {
                if left <= 1 {
                    self.calm();
                    Phase::Watching
                } else {
                    Phase::Cooling(left - 1)
                }
            }
        };
        match self.phase {
            Phase::Trial { periods, .. } | Phase::Held { periods, .. } => periods * period.max(1),
            Phase::Watching | Phase::Cooling(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aec(erle_db: f32, echo_delay_ms: f32) -> Option<AecStats> {
        Some(AecStats { echo_delay_ms, erle_db })
    }

    /// A second of blocks: the far end playing (a speech-like envelope, syllables of 200 ms loud and soft) or silent, and
    /// the mic hearing its echo (`echo`) or a talker of its own rhythm.
    fn feed(g: &mut EchoGuard, second: u32, far: bool, echo: bool) {
        for k in 0..100u32 {
            let level = [0.3f32, 0.05, 0.12][(((second * 100 + k) / 20) % 3) as usize];
            let far_blk: Block = if far { [level; BLOCK] } else { [0.0; BLOCK] };
            let mic_blk: Block = if echo {
                far_blk.map(|x| 0.5 * x + 1e-4)
            } else {
                [[0.02f32, 0.2, 0.08, 0.15][(((second * 100 + k) / 13) % 4) as usize]; BLOCK]
            };
            g.block(&mic_blk, &far_blk);
        }
    }

    /// The exclusive mic's lead target on the cables (18.3 ms), on 10 ms loopback periods.
    const TARGET: f64 = 18.3 * MS;
    const P: usize = 480;

    /// Runs seconds `from..from + secs` with the far end playing and AEC3's estimates `stats`; each second's hold.
    fn run(g: &mut EchoGuard, from: u32, secs: u32, echo: bool, stats: Option<AecStats>) -> Vec<usize> {
        (from..from + secs)
            .map(|s| {
                feed(g, s, true, echo);
                g.second(stats, TARGET, P)
            })
            .collect()
    }

    /// The second (0-based) the first trial starts at with the far end playing throughout: the warm-up's last, then the
    /// rest of the suspect run. A trial is judged that many far-end seconds after it starts.
    const TRIAL_AT: u32 = WARMUP_S - 1 + SUSPECT_S - 1;
    const TRIAL_S: u32 = SETTLE_S + JUDGE_S;

    /// The envelopes: an echo correlates, an independent talker doesn't; an echo two blocks before its reference does too.
    #[test]
    fn the_envelopes_tell_an_echo_from_none() {
        let mut g = EchoGuard::new();
        (0..5).for_each(|s| feed(&mut g, s, true, true));
        assert!(g.env.correlation() > 0.9, "{}", g.env.correlation());
        let mut g = EchoGuard::new();
        (0..5).for_each(|s| feed(&mut g, s, true, false));
        assert!(g.env.correlation() < ECHO_R, "{}", g.env.correlation());
        let mut e = Envelopes { mic: Box::new([FLOOR_DB; ENV]), far: Box::new([FLOOR_DB; ENV]), at: 0, len: 0 };
        let level = |k: usize| [-10.0f32, -30.0, -20.0][(k / 20) % 3] + (k % 7) as f32;
        (0..400).for_each(|k| e.push(level(k + 2) - 6.0, level(k)));
        assert!(e.correlation() > 0.9, "{}", e.correlation());
    }

    /// The holds worth trying: whole periods until the effective lead is 2 ms or under, within 30 ms; none for a lead too
    /// short to be worth one.
    #[test]
    fn the_holds_tried_are_whole_periods() {
        assert_eq!(most_periods(TARGET, P), 2, "18.3 ms: 10, then 20 (-1.7 ms)");
        assert_eq!(most_periods(12.0 * MS, P), 1, "12 ms: 10 (2 ms)");
        assert_eq!(most_periods(12.1 * MS, P), 2);
        assert_eq!(most_periods(TARGET, 144), 6, "3 ms packets: up to 18 ms");
        assert_eq!(most_periods(100.0 * MS, P), 3, "30 ms at most");
        assert_eq!(most_periods(4.9 * MS, P), 0);
        assert_eq!(most_periods(5.0 * MS, P), 1);
        assert_eq!(most_periods(TARGET, 2048), 0, "a period past the most hold");
    }

    /// An echo AEC3 can't place: after the warm-up, five suspect seconds start a one-period trial; an ERLE up from 6 to 16 dB
    /// keeps it.
    #[test]
    fn a_non_causal_echo_is_held() {
        let mut g = EchoGuard::new();
        let holds = run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
        assert!(holds[..TRIAL_AT as usize].iter().all(|&h| h == 0), "{holds:?}");
        assert_eq!((holds[TRIAL_AT as usize], g.state()), (P, GuardState::Trial));
        let holds = run(&mut g, TRIAL_AT + 1, TRIAL_S, true, aec(16.0, 0.0));
        assert!(holds.iter().all(|&h| h == P), "{holds:?}");
        assert_eq!(g.state(), GuardState::Held);
        assert!(run(&mut g, TRIAL_AT + 1 + TRIAL_S, 300, true, aec(16.0, 0.0)).iter().all(|&h| h == P), "it stays");
    }

    /// The live case (TODO 10's detail): AEC3 places the echo in the trial (its delay estimate 0 to 8 ms) and starts its filter
    /// over, its ERLE at 0 meanwhile. Placed is good: the hold stays.
    #[test]
    fn aec3_placing_the_echo_keeps_the_hold() {
        let mut g = EchoGuard::new();
        run(&mut g, 0, TRIAL_AT + 1, true, aec(7.0, 0.0));
        run(&mut g, TRIAL_AT + 1, SETTLE_S + 2, true, aec(10.0, 0.0));
        let holds = run(&mut g, TRIAL_AT + 3 + SETTLE_S, JUDGE_S, true, aec(0.0, 8.0));
        assert_eq!(holds.last(), Some(&P));
        assert_eq!(g.state(), GuardState::Held);
        assert!(run(&mut g, TRIAL_AT + 3 + TRIAL_S, 30, true, aec(2.0, 8.0)).iter().all(|&h| h == P), "placed: not suspect");
    }

    /// A one-period trial that doesn't help is followed by a two-period one; that one helps and stays.
    #[test]
    fn a_trial_that_doesnt_help_tries_a_period_more() {
        let mut g = EchoGuard::new();
        run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
        let holds = run(&mut g, TRIAL_AT + 1, TRIAL_S, true, aec(7.0, 0.0));
        assert_eq!((holds[TRIAL_S as usize - 2], holds[TRIAL_S as usize - 1]), (P, 2 * P), "judged: a period more");
        assert_eq!(g.state(), GuardState::Trial);
        let holds = run(&mut g, TRIAL_AT + 1 + TRIAL_S, TRIAL_S, true, aec(15.0, 0.0));
        assert!(holds.iter().all(|&h| h == 2 * P), "{holds:?}");
        assert_eq!(g.state(), GuardState::Held);
    }

    /// No echo (headphones, the near talker alone): no trial, however low AEC3's ERLE. An echo AEC3 cancels, or places (its
    /// delay estimate past a block), or no AEC3, or a lead too short to be worth a hold: none either.
    #[test]
    fn no_trial_without_an_echo_or_for_one_aec3_cancels() {
        let none = |echo, stats| {
            let mut g = EchoGuard::new();
            run(&mut g, 0, 120, echo, stats).iter().all(|&h| h == 0)
        };
        assert!(none(false, aec(1.0, 0.0)), "no echo");
        assert!(none(true, aec(15.0, 0.0)), "cancelled");
        assert!(none(true, aec(5.0, 8.0)), "placed (a poor room)");
        assert!(none(true, None), "no AEC3");
        let mut g = EchoGuard::new();
        let short: Vec<_> = (0..60)
            .map(|s| {
                feed(&mut g, s, true, true);
                g.second(aec(6.0, 0.0), 4.0 * MS, P)
            })
            .collect();
        assert!(short.iter().all(|&h| h == 0), "a 4 ms target");
    }

    /// Trials that don't help let the hold go after the last worth trying, and the next waits ten minutes; the far end's
    /// silence counts neither for a trial nor against it.
    #[test]
    fn trials_that_dont_help_cool_down() {
        let mut g = EchoGuard::new();
        run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
        for s in TRIAL_AT + 1..TRIAL_AT + 11 {
            feed(&mut g, s, false, false);
            assert_eq!(g.second(aec(6.0, 0.0), TARGET, P), P, "the trial waits for the far end");
        }
        let holds = run(&mut g, TRIAL_AT + 11, 2 * TRIAL_S, true, aec(7.0, 0.0));
        let n = 2 * TRIAL_S as usize;
        assert_eq!((holds[n - 2], holds[n - 1]), (2 * P, 0), "two periods didn't help either");
        assert_eq!(g.state(), GuardState::Cooling);
        let from = TRIAL_AT + 11 + 2 * TRIAL_S;
        let holds = run(&mut g, from, COOLDOWN_S + SUSPECT_S, true, aec(6.0, 0.0));
        let first = holds.iter().position(|&h| h > 0);
        assert_eq!(first, Some((COOLDOWN_S + SUSPECT_S - 1) as usize), "ten minutes, then five suspect seconds");
    }

    /// Held: the echo going away (headphones put on) lets the hold go after a minute of the far end; the echo becoming one
    /// AEC3 can't cancel again tries a period more, and after the last, none.
    #[test]
    fn a_held_run_lets_the_hold_go() {
        let held = || {
            let mut g = EchoGuard::new();
            run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
            run(&mut g, TRIAL_AT + 1, TRIAL_S, true, aec(16.0, 0.0));
            assert_eq!(g.state(), GuardState::Held);
            g
        };
        let from = TRIAL_AT + 1 + TRIAL_S;
        let mut g = held();
        let holds = run(&mut g, from, 70, false, aec(1.0, 0.0));
        let gone = holds.iter().position(|&h| h == 0).unwrap();
        assert!((NO_ECHO_S as usize - 1..NO_ECHO_S as usize + 6).contains(&gone), "about a minute: {gone}");
        assert_eq!(g.state(), GuardState::Watching);
        let mut g = held();
        let holds = run(&mut g, from, SUSPECT_S, true, aec(6.0, 0.0));
        assert_eq!((holds[0], holds[SUSPECT_S as usize - 1], g.state()), (P, 2 * P, GuardState::Trial), "a period more");
        let holds = run(&mut g, from + SUSPECT_S, TRIAL_S, true, aec(6.0, 0.0));
        assert_eq!((holds[0], *holds.last().unwrap(), g.state()), (2 * P, 0, GuardState::Cooling), "and then none");
    }
}
