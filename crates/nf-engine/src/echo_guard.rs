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
//! that much newer (the effective lead is the reference loop's less the hold; the loop itself sees nothing). The hold costs
//! its length in latency, so it comes only for an echo seen that AEC3 can't place:
//! - **An echo:** the mic's energy envelope (a value a block) follows the far end's: their correlation over the last 5 s, at
//!   the best of a few lags either way, is [`ECHO_R`] or more. Headphones, or the near talker alone, show none.
//! - **That AEC3 can't place:** with the far end playing, AEC3's ERLE under [`LOW_ERLE_DB`] and its delay estimate at most a
//!   block of its own (4 ms). It reads in 4 ms steps less its headroom, so a short echo that it does place reads 0 as well:
//!   only the ERLE tells them apart.
//!
//! [`SUSPECT_S`] such seconds in a row (once the run is [`WARMUP_S`] old and the far end has played for [`CONVERGE_S`], so a
//! placeable echo has been cancelled by then) start a **trial**: the hold that brings the effective lead to [`TRIAL_LEAD`]
//! (2 ms). AEC3 needs [`SETTLE_S`] seconds of the far end to find the echo again after the jump; the next [`JUDGE_S`] seconds
//! of it judge: an ERLE [`GAIN_DB`] over the suspect seconds' and at [`LOW_ERLE_DB`] or more in half of them keeps the hold;
//! otherwise it goes, and no trial comes for [`COOLDOWN_S`]. Kept, AEC3's delay estimate says how much of the hold the echo
//! didn't need: once, after [`TRIM_S`] seconds of it, the hold gives back all but [`TRIM_MARGIN_MS`] of that (the latency it
//! costs, 16 ms for a 2 ms lead under an 18 ms one, comes down to what the echo path needs). A held run whose echo is again
//! one AEC3 can't cancel goes back to the full hold, or, there already, lets it go; one whose echo goes away ([`NO_ECHO_S`]
//! seconds of the far end with none: headphones put on) lets it go.
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
/// AEC3's ERLE under this (dB), the far end playing: it isn't cancelling the echo. And its delay estimate at most this (ms):
/// it hasn't placed it.
pub(crate) const LOW_ERLE_DB: f32 = 10.0;
const PLACED_MS: f32 = 4.0;
/// Seconds: the run's age before anything counts, and the far end's playing before then.
const WARMUP_S: u32 = 20;
const CONVERGE_S: u32 = 10;
/// Suspect seconds in a row that start a trial.
const SUSPECT_S: u32 = 5;
/// The trial's effective lead (samples): 2 ms.
pub(crate) const TRIAL_LEAD: f64 = 2.0 * MS;
/// The least hold worth a trial (samples): 3 ms. The most the pipeline holds (30 ms).
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
/// After a trial that didn't help, or a hold let go that couldn't cancel the echo: no trial for 10 minutes.
const COOLDOWN_S: u32 = 600;
/// Held: seconds of AEC3 cancelling (the far end playing, its ERLE at [`LOW_ERLE_DB`] or more) whose delay estimates' median
/// trims the hold, by all of it but this margin (ms), if that's at least [`MIN_TRIM_MS`].
const TRIM_S: usize = 8;
const TRIM_MARGIN_MS: f32 = 4.0;
const MIN_TRIM_MS: f32 = 4.0;
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
    /// Far-end seconds settled and judged, the good ones, the suspect seconds' mean ERLE, and the trial's age (s).
    Trial {
        settled: u32,
        judged: u32,
        good: u32,
        baseline: f32,
        age: u32,
    },
    /// The effective lead held at (samples), whether trimmed, AEC3's delay estimates for the trim, and far-end seconds with no
    /// echo in a row.
    Held {
        lead: f64,
        trimmed: bool,
        delays: [f32; TRIM_S],
        n: usize,
        quiet: u32,
    },
    /// Seconds left without a trial.
    Cooling(u32),
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

    /// The second's end: AEC3's estimates now, and the reference's lead target (samples). Returns the effective lead to hold
    /// the reference at (samples), or `None` for no hold.
    pub(crate) fn second(&mut self, aec: Option<AecStats>, target: f64) -> Option<f64> {
        self.seconds = self.seconds.saturating_add(1);
        let far = std::mem::take(&mut self.far_blocks) >= FAR_BLOCKS;
        self.far_seconds += u32::from(far);
        self.correlation = self.env.correlation();
        let (erle, delay) = aec.map_or((f32::NAN, f32::NAN), |a| (a.erle_db, a.echo_delay_ms));
        let echo = self.correlation >= ECHO_R;
        // The far end playing, an echo, and AEC3 neither cancelling nor placing it.
        let suspect = far && echo && erle.is_finite() && erle < LOW_ERLE_DB && (delay.is_nan() || delay <= PLACED_MS);
        let warm = self.seconds >= WARMUP_S && self.far_seconds >= CONVERGE_S;
        if far {
            if suspect && warm {
                self.suspect += 1;
                self.erle_sum += erle;
            } else {
                (self.suspect, self.erle_sum) = (0, 0.0);
            }
        }
        let run = self.suspect;
        self.phase = match self.phase {
            Phase::Watching if run >= SUSPECT_S && target - TRIAL_LEAD >= MIN_HOLD => {
                let baseline = self.erle_sum / run as f32;
                (self.suspect, self.erle_sum) = (0, 0.0);
                Phase::Trial { settled: 0, judged: 0, good: 0, baseline, age: 0 }
            }
            Phase::Watching => Phase::Watching,
            Phase::Trial { settled, judged, good, baseline, age } => {
                let age = age + 1;
                let (settled, judged, good) = if !far {
                    (settled, judged, good)
                } else if settled < SETTLE_S {
                    (settled + 1, judged, good)
                } else {
                    let ok = erle.is_finite() && erle >= LOW_ERLE_DB && erle >= baseline + GAIN_DB;
                    (settled, judged + 1, good + u32::from(ok))
                };
                if judged >= JUDGE_S {
                    if 2 * good >= judged {
                        (self.suspect, self.erle_sum) = (0, 0.0);
                        Phase::Held { lead: TRIAL_LEAD, trimmed: false, delays: [0.0; TRIM_S], n: 0, quiet: 0 }
                    } else {
                        Phase::Cooling(COOLDOWN_S)
                    }
                } else if age >= TRIAL_MAX_S {
                    Phase::Cooling(RETRY_S)
                } else {
                    Phase::Trial { settled, judged, good, baseline, age }
                }
            }
            Phase::Held { lead, trimmed, mut delays, mut n, quiet } => {
                let quiet = if !far {
                    quiet
                } else if self.correlation < NO_ECHO_R {
                    quiet + 1
                } else {
                    0
                };
                if quiet >= NO_ECHO_S {
                    (self.suspect, self.erle_sum) = (0, 0.0);
                    Phase::Watching
                } else if run >= SUSPECT_S {
                    // AEC3 can't cancel it again: the full hold, or (there already) none.
                    (self.suspect, self.erle_sum) = (0, 0.0);
                    if lead > TRIAL_LEAD {
                        Phase::Held { lead: TRIAL_LEAD, trimmed: true, delays, n, quiet }
                    } else {
                        Phase::Cooling(COOLDOWN_S)
                    }
                } else {
                    let mut lead = lead;
                    let mut trimmed = trimmed;
                    if !trimmed && far && erle.is_finite() && erle >= LOW_ERLE_DB && delay.is_finite() {
                        delays[n] = delay;
                        n += 1;
                        if n == TRIM_S {
                            delays.sort_unstable_by(f32::total_cmp);
                            let spare = delays[TRIM_S / 2] - TRIM_MARGIN_MS;
                            if spare >= MIN_TRIM_MS {
                                lead = (lead + f64::from(spare) * MS).min(target);
                            }
                            trimmed = true;
                        }
                    }
                    Phase::Held { lead, trimmed, delays, n, quiet }
                }
            }
            Phase::Cooling(left) => {
                if left <= 1 {
                    (self.suspect, self.erle_sum) = (0, 0.0);
                    Phase::Watching
                } else {
                    Phase::Cooling(left - 1)
                }
            }
        };
        match self.phase {
            Phase::Trial { .. } => Some(TRIAL_LEAD),
            Phase::Held { lead, .. } => Some(lead),
            Phase::Watching | Phase::Cooling(_) => None,
        }
    }
}

/// The hold for an effective `lead` under the reference's lead `target` (samples): the difference, whole, from 0 to
/// [`MAX_HOLD`].
pub(crate) fn hold_for(lead: Option<f64>, target: f64) -> usize {
    lead.map_or(0, |l| (target - l).round().clamp(0.0, MAX_HOLD as f64) as usize)
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

    /// Runs seconds `from..from + secs` with the far end playing, AEC3's estimates `stats` and an 18.3 ms lead target (the
    /// exclusive mic's on the cables); each second's lead.
    fn run(g: &mut EchoGuard, from: u32, secs: u32, echo: bool, stats: Option<AecStats>) -> Vec<Option<f64>> {
        (from..from + secs)
            .map(|s| {
                feed(g, s, true, echo);
                g.second(stats, 18.3 * MS)
            })
            .collect()
    }

    /// The second (0-based) the first trial starts at with the far end playing throughout: the warm-up's last, then the
    /// rest of the suspect run.
    const TRIAL_AT: u32 = WARMUP_S - 1 + SUSPECT_S - 1;
    /// The second the trial is judged at: settled and judged, all with the far end.
    const JUDGED_AT: u32 = TRIAL_AT + SETTLE_S + JUDGE_S;

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

    /// An echo AEC3 can't place: after the warm-up, five suspect seconds start a trial at a 2 ms lead; an ERLE up from 6 to
    /// 16 dB keeps it; AEC3's delay estimate of 12 ms then trims it by 8 ms, once.
    #[test]
    fn a_non_causal_echo_is_held_then_trimmed() {
        let mut g = EchoGuard::new();
        let leads = run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
        assert!(leads[..TRIAL_AT as usize].iter().all(Option::is_none), "{leads:?}");
        assert_eq!((leads[TRIAL_AT as usize], g.state()), (Some(TRIAL_LEAD), GuardState::Trial));
        let leads = run(&mut g, TRIAL_AT + 1, JUDGED_AT - TRIAL_AT, true, aec(16.0, 12.0));
        assert!(leads.iter().all(|l| *l == Some(TRIAL_LEAD)), "{leads:?}");
        assert_eq!(g.state(), GuardState::Held);
        let leads = run(&mut g, JUDGED_AT + 1, TRIM_S as u32, true, aec(16.0, 12.0));
        assert_eq!(leads[..TRIM_S - 1], [Some(TRIAL_LEAD); TRIM_S - 1]);
        assert_eq!(leads[TRIM_S - 1], Some(TRIAL_LEAD + 8.0 * MS));
        let leads = run(&mut g, JUDGED_AT + 1 + TRIM_S as u32, 30, true, aec(16.0, 4.0));
        assert!(leads.iter().all(|l| *l == Some(10.0 * MS)), "trimmed once: {leads:?}");
        assert_eq!(hold_for(Some(10.0 * MS), 18.3 * MS), 398, "8.3 ms");
        assert_eq!(hold_for(None, 18.3 * MS), 0);
        assert_eq!(hold_for(Some(TRIAL_LEAD), 100.0 * MS), MAX_HOLD);
    }

    /// No echo (headphones, the near talker alone): no trial, however low AEC3's ERLE. An echo AEC3 cancels, or places (its
    /// delay estimate past a block), or no AEC3, or a lead too short to be worth a hold: none either.
    #[test]
    fn no_trial_without_an_echo_or_for_one_aec3_cancels() {
        let none = |echo, stats| {
            let mut g = EchoGuard::new();
            run(&mut g, 0, 120, echo, stats).iter().all(Option::is_none)
        };
        assert!(none(false, aec(1.0, 0.0)), "no echo");
        assert!(none(true, aec(15.0, 0.0)), "cancelled");
        assert!(none(true, aec(5.0, 8.0)), "placed (a poor room)");
        assert!(none(true, None), "no AEC3");
        let mut g = EchoGuard::new();
        let short: Vec<_> = (0..60)
            .map(|s| {
                feed(&mut g, s, true, true);
                g.second(aec(6.0, 0.0), 4.0 * MS)
            })
            .collect();
        assert!(short.iter().all(Option::is_none), "a 4 ms target");
    }

    /// A trial that doesn't raise AEC3's ERLE lets the hold go, and the next waits ten minutes; the far end's silence counts
    /// neither for a trial nor against it.
    #[test]
    fn a_trial_that_doesnt_help_cools_down() {
        let mut g = EchoGuard::new();
        run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
        for s in TRIAL_AT + 1..TRIAL_AT + 11 {
            feed(&mut g, s, false, false);
            assert_eq!(g.second(aec(6.0, 0.0), 18.3 * MS), Some(TRIAL_LEAD), "the trial waits for the far end");
        }
        let n = SETTLE_S + JUDGE_S;
        let leads = run(&mut g, TRIAL_AT + 11, n, true, aec(7.0, 0.0));
        assert_eq!((leads[n as usize - 2], leads[n as usize - 1]), (Some(TRIAL_LEAD), None), "judged: it didn't help");
        assert_eq!(g.state(), GuardState::Cooling);
        let from = TRIAL_AT + 11 + n;
        let leads = run(&mut g, from, COOLDOWN_S + SUSPECT_S, true, aec(6.0, 0.0));
        let first = leads.iter().position(Option::is_some);
        assert_eq!(first, Some((COOLDOWN_S + SUSPECT_S - 1) as usize), "ten minutes, then five suspect seconds");
    }

    /// Held: the echo going away (headphones put on) lets the hold go after a minute of the far end; the echo becoming one
    /// AEC3 can't cancel again takes back the trim, and then lets the hold go.
    #[test]
    fn a_held_run_lets_the_hold_go() {
        let held = || {
            let mut g = EchoGuard::new();
            run(&mut g, 0, TRIAL_AT + 1, true, aec(6.0, 0.0));
            let leads = run(&mut g, TRIAL_AT + 1, JUDGED_AT - TRIAL_AT + TRIM_S as u32, true, aec(16.0, 12.0));
            assert_eq!(leads.last(), Some(&Some(10.0 * MS)), "trimmed");
            g
        };
        let from = JUDGED_AT + 1 + TRIM_S as u32;
        let mut g = held();
        let leads = run(&mut g, from, 70, false, aec(1.0, 0.0));
        let gone = leads.iter().position(Option::is_none).unwrap();
        assert!((NO_ECHO_S as usize - 1..NO_ECHO_S as usize + 6).contains(&gone), "about a minute: {gone}");
        assert_eq!(g.state(), GuardState::Watching);
        let mut g = held();
        let leads = run(&mut g, from, 2 * SUSPECT_S, true, aec(6.0, 0.0));
        assert_eq!(leads[SUSPECT_S as usize - 1], Some(TRIAL_LEAD), "back to the full hold");
        assert_eq!(leads[2 * SUSPECT_S as usize - 1], None, "and then none");
        assert_eq!(g.state(), GuardState::Cooling);
    }
}
