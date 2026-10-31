//! The reference timeline (engine.md "The reference"): the loopback's samples on one timeline, read in step with the mic's
//! blocks so that the echo path holds still.
//!
//! AEC3 models the echo as a filter on the reference. It converges once and then holds, as long as the echo path (a mic
//! sample's index less that of the reference sample served with it) stays put. The path moves whenever a block consumes
//! other than what the two clocks call for. The first design held a FIFO at one loopback period + a block, and on the dev
//! box's cables, which deliver in bursts, it moved the path several times a second: a short FIFO was zero-filled up to the
//! target, a long one had whole blocks discarded, and its drift loop slipped after the swings (`doc/todo/10-aec.md`, "Fix").
//! So:
//! - **The read position is virtual.** Every block consumes its 480 samples (±1 for a slip) whether they've arrived or not.
//!   What's missing is served as zeros and owed: those samples are discarded when they come. The reference loses a few ms,
//!   but its timeline stays whole (counted as a reference gap).
//! - **The lead is read from the stamps.** The lead is the mic block's capture time less the tap time of the reference
//!   sample served with it, both from the packets' QPC stamps. It sees the two clocks and nothing of when packets are
//!   delivered, so a burst doesn't move it. A stream without stamps, or whose stamps scatter too widely (`StampMap`: a
//!   driver that stamps at delivery, or noise), is timed by when its packets were delivered instead, for the rest of the
//!   run (in the start-up the timing can still change either way, and the start-up starts over). That reading swings
//!   with the bursts and, delivered at the mic's wakes, can't see the drift: it moves by a whole packet once per beat of
//!   the two clocks. So then nothing slips, and each such step is put back in a jump of a packet, once a 3 s mean shows it.
//! - **A slow loop holds the lead at its target** ([`LeadLoop`]): it slips at the drift rate (a least-squares slope of the
//!   uncorrected lead over about 20 s, trusted after 10 s once its standard error is within 10 ppm), and corrects the
//!   offset only outside ±0.5 ms.
//! - **The target covers the delivery's swing.** A deeper reference costs nothing at the output. What it costs is the echo
//!   path's own delay: the loopback taps the mix ahead of the speakers, so the echo reaches the mic the output's latency
//!   (and the air's) after its reference, and the lead has to stay under that or the echo comes before its reference. A
//!   take's need is the least lead that would have served it whole. Only fresh takes count, made while the loopback is
//!   delivering (within two periods and 5 ms), and only once it delivers again: silence without the keep-alive, and a
//!   stall's first wakes (fresh, needing more each wake), say nothing about the delivery. And a need counts once three
//!   wakes have reached it (in the start-up, or within a second after it), so a lone late packet doesn't raise the target.
//!   The target starts at one loopback period + 2 ms, what steady delivery needs at the worst phase of the two clocks. For
//!   the first 2.5 s, while AEC3 converges anyway, it rises with the need, a jump each time. After that it rises only when
//!   underflows persist (6% of the takes for good, a seventh for 6 s, or 0.6 s in a row; timed by delivery 2.2 s in a
//!   row), to what the last 10 s needed + 3 ms: a jump costs AEC3 seconds, an owed block now and then next to nothing. A
//!   drifting bursty loopback's phase against the mic's wakes sets its need, and the start-up sees one phase: a raise at
//!   the first worse one covers it for good. Once the need has stayed 3 ms or more under the target for five minutes it
//!   comes back down, in one jump. It never passes [`LEAD_CAP`].
//! - **Jumps.** The first reading aligns the lead in one jump, either way. That reading is one sample of the stamps' jitter
//!   (or, unstamped, of the delivery's swing), so the start-up corrects nothing, and at its end one more jump puts the
//!   low-passed lead on its target. A step of the lead past 4 ms (or 2.5 times the stamps' jitter), seen in the median and
//!   in 7 of the last 9 readings, is a broken timeline (a stall longer than the buffers, a device reset, a stamp that
//!   jumped), and one jump undoes it; the drift fit starts over. Timed by delivery a break is a step past 30 ms, undone
//!   in a jump and centred 2.5 s later. A loopback that skipped samples (lost past the position fill, or silent without the
//!   keep-alive) needs none: its stamps, two in a row agreeing (or one, when the position jumped too), put zeros where the
//!   samples should have been.
//! - **Counted:** a block served zeros for samples that hadn't arrived is a reference gap; a jump after the start-up (a
//!   break, a change of target, a step of the count without stamps) is a reference coarse drop; and slips.
//!
//! Everything here runs on the audio thread: no allocation, locks or panics, O(1) per block.

use nf_core::dsp::slip::Slip;
use nf_core::{BLOCK, SAMPLE_RATE};

use crate::api::ReferenceFacts;
use crate::fifo::Fifo;
use crate::pipeline::{Counters, MAX_GAP_FILL, Packet};
use crate::timing::{HNS_PER_S, StampMap};

/// Reference FIFO capacity: 500 ms.
const FIFO: usize = 24_000;
/// 100 ns units per sample at the app's rate.
const HNS_PER_SAMPLE: f64 = HNS_PER_S / SAMPLE_RATE as f64;
/// The lead target's ceiling: 50 ms. It must stay under the echo path's own delay: the cables' is 60 ms, and their traces
/// needed up to 23 ms. Real speakers' output latency is often only 10-30 ms, but they deliver steadily, which needs about
/// a period. Nothing measures the echo path's delay yet (AEC3's delay estimate could), so a target over 30 ms is flagged
/// where it's shown (`nf_core::telemetry::REFERENCE_TARGET_HIGH_MS`).
pub(crate) const LEAD_CAP: usize = 2_400;
/// Raising the target: when the need comes within half this (1.5 ms) of it, the target goes to the need plus all of it
/// (3 ms), so a raise is a jump of at least 1.5 ms.
const RAISE_MARGIN: f64 = 144.0;
/// The start-up, from the alignment: 2.5 s in which the target follows the need up and the low-pass settles.
const START_BLOCKS: u64 = 250;
/// A need counts once the fresh takes of this many wakes have reached it: the third largest of the start-up's, or of a
/// second's (a stall's wakes don't count: `Needs::pending`). On the cables a bursty mic's late packets need 10 ms more
/// in one wake now and then, three times in a start-up.
const RECUR: usize = 3;
/// The most wakes a run between deliveries holds while the loopback is fresh (two periods and 5 ms of 10 ms wakes: 3).
const PENDING: usize = 4;
/// After the start-up the target rises only when underflows persist: a leaky count of the fresh takes that come up short
/// (decaying with a 10 s time constant) reaches `RAISE_SHORT`. That's 0.6 s of them in a row, a seventh of them for 6 s,
/// or 6% for good; a stall's takes (stale) don't count, nor do a few short ones now and then. The trade, from the offline
/// AEC3 check (`aec3_converges_on_the_traces`, timeline.rs): a jump costs AEC3 5-10 s of reconvergence (its ERLE fell from
/// 21 dB to 0 at a 3 ms raise), but owing blocks costs it for as long as they're owed, and a bursty loopback's phase
/// against the mic's wakes, which its drift moves, sets how many: the start-up sees one phase, and at the worst one (for
/// 30 s of every 100 at 100 ppm, 90 s of every 330 at 30 ppm) 13-20% of its blocks came up short, which cost AEC3 3-4 dB
/// of its ERLE throughout. Raised then, the target covers that phase for good: it comes down only after `LOWER_CHUNKS`.
const SHORT_DECAY: f64 = 1.0 - 1.0 / 1_000.0;
const RAISE_SHORT: f64 = 60.0;
/// Timed by delivery, each step of the count leaves the takes short until its jump of a packet, about a second later
/// (the low-pass's), so a raise waits for 2.2 s of them in a row (or a fifth for good) instead: the step's jump comes
/// first.
const RAISE_SHORT_UNSTAMPED: f64 = 200.0;
/// The need's record after the start-up: a value a second (100 blocks), the `RECUR`-th largest of its wakes' needs, kept
/// when at least half its takes were fresh. A raise goes to the most of the last 10 s (and this second's); the target
/// comes down to the most of the last 5 minutes + `RAISE_MARGIN` once that's `LOWER_BY` (3 ms) or more under it, so a
/// stretch of late delivery, or a start-up's bad luck, doesn't hold it up for good. Five minutes is longer than a drifting
/// loopback's good phase down to about 15 ppm, so its target isn't lowered and raised again every beat. Both start the
/// five minutes over.
const CHUNK_BLOCKS: u32 = 100;
const CHUNK_MIN_FRESH: u32 = 50;
const RAISE_CHUNKS: usize = 10;
const LOWER_CHUNKS: usize = 300;
const LOWER_BY: f64 = 144.0;
/// The loopback is stale, not delivering, once its last packet came more than two periods (or blocks: the takes come at
/// the mic's wakes) and this (5 ms) ago. A bursty loopback that delivers three packets every third wake is still fresh.
const STALE_SLACK: f64 = 240.0;
/// Breaks are judged on the median of the last 9 readings, and go by 7 of them: a step needs the median past it, and 7
/// readings past it, before the jump that undoes it (by their mean, less the outermost two: on the cables' bursty mic 5
/// readings came from 3 stamps, and left the lead up to 0.9 ms off for the loop to slip back).
const MEDIAN: usize = 9;
const BREAK_READINGS: usize = 7;
/// A loopback stamp further than this (4 ms, or 4 times the stamps' scatter if more) from its place on the timeline is
/// suspect. Confirmed, by the next stamp agreeing within `STAMP_AGREE_HNS` (1 ms) or by the device position jumping past
/// the position fill, a late one means samples that never came, and zeros put the packet where its stamp says; an early
/// one re-anchors the stamps (and the lead's median shows it, if it's a break). Unconfirmed, it's a stamp that was off,
/// and it's left out. The loopback's stamps jitter by about 0.05 ms on the cables.
const STAMP_GAP_HNS: f64 = 40_000.0;
const STAMP_GAP_SCATTERS: f64 = 4.0;
const STAMP_AGREE_HNS: f64 = 10_000.0;
/// Stamps further than this from now are ignored (500 ms, as `StampMap` does).
const MAX_STAMP_SKEW_HNS: f64 = 5e6;

/// How far off its target a stamped lead goes: offsets within ±0.5 ms (the dead band) are left alone, and once outside
/// one is corrected until back within 0.125 ms (the release); a step of 4 ms is a broken timeline, and a reading twice
/// that far from the low-pass isn't used. The cables' mic stamps are in 0.5 ms steps and jitter by about ±1 ms; after the
/// loop's low-pass their 1 s means wander by about ±0.15 ms. Bursts don't reach them.
const DEADBAND: f64 = 24.0;
const RELEASE: f64 = 6.0;
const BROKEN: f64 = 192.0;
/// A step is a break only past this many times the readings' jitter, too: trusted stamps jitter by up to 2.5 ms
/// (`StampMap`), and the median of 9 such readings then passes 6.25 ms about once a decade.
const BREAK_JITTERS: f64 = 2.5;
/// A lead timed by delivery swings by up to the cables' 30 ms with the bursts (a 1 s low-pass of it by ±2.5 ms), so a
/// step of its median 30 ms off is a broken timeline: one jump undoes it, and a centring `START_BLOCKS` later, by the slow
/// mean, if that's 1.25 ms off. Short of that, a slow mean of the lead more than 0.6 of a period (and 3 ms) off is a
/// step of the count, the drift's or the delivery's: one jump of whole packets (0.9 of a period the other way from the
/// last one).
const UNSTAMPED_BROKEN: f64 = 1_440.0;
const UNSTAMPED_SETTLE: f64 = 60.0;
const UNSTAMPED_STEP: f64 = 0.6;
const UNSTAMPED_BACK: f64 = 0.9;
/// The slow mean's time constant, timed by delivery: 3 s. The cables' bursts swing a 1 s low-pass of the delivery-timed
/// lead by ±2.5 ms, a 3 s one by about ±1.5 ms; a count step shows past 0.6 of a packet 2.7 s after it.
const SLOW_BLOCKS: u32 = 300;

/// When the mic block's first sample was captured (hns): by the mic's stamps, if it has them and they're trusted, and by
/// the wake that delivered its newest sample, the timing without stamps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MicTime {
    pub stamp: Option<f64>,
    pub delivered: f64,
    /// The mic stamps' scatter (hns; `StampMap::scatter_hns`).
    pub scatter: f64,
}

/// The reference stream's timeline, its FIFO, and the loop and target that hold its lead.
pub(crate) struct Reference {
    fifo: Fifo,
    /// Timeline index of the sample after the newest one delivered (position gaps zero-filled). The FIFO holds the last
    /// `fifo.len()` of them.
    end: i64,
    /// Timeline index of the next sample to serve. Past `end` while samples are owed (they're discarded as they come);
    /// before the FIFO's head after a jump back, or when the loopback started late (zeros until it gets there).
    read: i64,
    /// The device position the next packet should start at.
    next_pos: Option<u64>,
    /// Tap times of the timeline's samples, from the packet stamps.
    stamps: StampMap,
    /// The last packet's stamp, off its place on the timeline and unconfirmed: by how much (hns), its index and stamp.
    suspect: Option<(f64, u64, u64)>,
    /// The wake that delivered the last packet, and the wake now (hns): the timing without stamps, and freshness. And the
    /// time between the wakes that serve the reference (low-passed; 0 before two): the mic's period, or a block's.
    delivered_hns: Option<f64>,
    now_hns: f64,
    wake_gap: f64,
    /// The loopback's period, and how old its last delivery can be for it to be fresh (hns).
    period: f64,
    stale_hns: f64,
    /// The starting target (the least it comes down to), the target, and its ceiling, in samples.
    floor: f64,
    target: f64,
    cap: f64,
    /// The first reading has aligned the timeline.
    started: bool,
    /// The lead is timed by both streams' stamps (else by delivery).
    stamped: bool,
    /// What the delivery-timed lead reads over the stamped one, from when the stamps were given up (0 without stamps).
    offset: f64,
    /// While the stamps time the loop, the delivery-timed lead error's slow mean, for that switch.
    delivery_mean: SlowMean,
    /// Timed by delivery, the lead error's slow mean, which the centring and the count's steps go by: the bursts swing the
    /// 1 s low-pass by a few ms.
    slow: SlowMean,
    /// Timed by delivery, the way the last step of the count was put back (±1; 0 before one).
    last_step: f64,
    /// Blocks served since the alignment.
    blocks: u64,
    lp: LeadLoop,
    /// The last readings' lead errors, for the median.
    recent: Recent,
    /// The block at which the low-passed lead is centred on the target, if it's off: the start-up's end, and after a
    /// break without stamps.
    settle_at: u64,
    needs: Needs,
    /// What the last take did, for the chain recorder.
    facts: ReferenceFacts,
}

impl Reference {
    /// A timeline whose lead starts at `target` samples, for a loopback of `period` frames. Allocates the FIFO: not
    /// real-time.
    pub(crate) fn new(target: usize, period: usize) -> Self {
        let period = period.max(1) as f64;
        Self {
            fifo: Fifo::new(FIFO),
            end: 0,
            read: 0,
            next_pos: None,
            stamps: StampMap::new(SAMPLE_RATE),
            suspect: None,
            delivered_hns: None,
            now_hns: 0.0,
            wake_gap: 0.0,
            period,
            stale_hns: (2.0 * period.max(BLOCK as f64) + STALE_SLACK) * HNS_PER_SAMPLE,
            floor: target as f64,
            target: target as f64,
            cap: LEAD_CAP.max(target) as f64,
            started: false,
            stamped: false,
            offset: 0.0,
            delivery_mean: SlowMean::default(),
            slow: SlowMean::default(),
            last_step: 0.0,
            blocks: 0,
            lp: LeadLoop::new(),
            recent: Recent::new(),
            settle_at: START_BLOCKS,
            needs: Needs::new(),
            facts: ReferenceFacts::default(),
        }
    }

    /// Empties the FIFO (the stream was dropped mid-run).
    pub(crate) fn clear(&mut self) {
        self.fifo.clear();
    }

    /// Samples buffered.
    #[cfg(test)]
    pub(crate) fn fifo_len(&self) -> usize {
        self.fifo.len()
    }

    /// The lead target, in samples.
    #[cfg(test)]
    pub(crate) fn target(&self) -> f64 {
        self.target
    }

    pub(crate) fn target_ms(&self) -> f32 {
        (self.target * 1000.0 / SAMPLE_RATE as f64) as f32
    }

    /// The lead as the loop sees it (low-passed), in ms; 0 before the first packet.
    pub(crate) fn lead_ms(&self) -> f32 {
        if self.started { ((self.target + self.lp.error()) * 1000.0 / SAMPLE_RATE as f64) as f32 } else { 0.0 }
    }

    /// The loop's drift estimate: positive when the loopback's clock runs fast against the mic's.
    pub(crate) fn drift_ppm(&self) -> f32 {
        self.lp.ppm() as f32
    }

    /// Whether the stamps time the lead (else the delivery does); false before the first packet.
    pub(crate) fn stamped(&self) -> bool {
        self.started && self.stamped
    }

    /// What the last take did: where it read, what it served, the lead and the loop, and the jumps (the chain recorder's
    /// per-block facts).
    pub(crate) fn facts(&self) -> ReferenceFacts {
        self.facts
    }

    /// Notes a jump of the read position at this take.
    fn jumped(&mut self, event: u16, by: f64) {
        self.facts.events |= event;
        self.facts.jumped += by as i64;
    }

    /// The wake's time, before its packets are pushed.
    pub(crate) fn wake(&mut self, now_hns: f64) {
        let gap = now_hns - self.now_hns;
        if self.now_hns > 0.0 && gap > 0.0 {
            // A late wake moves it a little: a tenth.
            self.wake_gap = if self.wake_gap > 0.0 { self.wake_gap + 0.1 * (gap - self.wake_gap) } else { gap };
        }
        self.now_hns = now_hns;
    }

    /// Appends one loopback packet read at `now_hns`. Frames missing by device position (up to `MAX_GAP_FILL`) become
    /// zeros, as on the mic, so a lost packet doesn't shift the timeline. So, once confirmed, does a stamp more than
    /// `STAMP_GAP_HNS` later than the packet's place on the timeline: what came between never arrived (lost past the
    /// position fill, or silence without the keep-alive, when the loopback delivers nothing), and the zeros put the
    /// packet where its stamp says. A read that ran on meanwhile owes exactly those zeros, so after silence the timeline
    /// carries on without a jump. Confirmed by the next stamp, the zeros go in after the first packet, which is served a
    /// packet early; it's mostly owed by then.
    pub(crate) fn push(&mut self, p: &Packet, samples: &[f32], now_hns: f64) {
        // Frames missing by position past the fill: the stamp says whether they're real.
        let mut lost = None;
        if let Some(expect) = self.next_pos
            && p.device_pos > expect
        {
            if p.device_pos - expect <= MAX_GAP_FILL {
                self.append((p.device_pos - expect) as usize, None);
            } else {
                lost = Some(p.device_pos - expect);
            }
        }
        self.next_pos = Some(p.device_pos + samples.len() as u64);
        if let Some(ts) = p.qpc_hns
            && ((ts as f64) - now_hns).abs() < MAX_STAMP_SKEW_HNS
        {
            let at = self.end.max(0) as u64;
            let off = self.stamps.time_of(at).map(|due| ts as f64 - due);
            let threshold = STAMP_GAP_HNS.max(STAMP_GAP_SCATTERS * self.stamps.scatter_hns());
            match off {
                Some(off) if self.stamps.trusted() && off.abs() > threshold => {
                    if lost.is_some() || self.suspect.is_some_and(|(s, _, _)| (off - s).abs() <= STAMP_AGREE_HNS) {
                        self.suspect = None;
                        // At the loopback's rate: the stamps are QPC, and the drift over a long silence adds up. The
                        // position, when it agrees, counts the frames exactly.
                        let rate = 1.0 + self.lp.ppm() * 1e-6;
                        let by_stamp = off / HNS_PER_SAMPLE * rate;
                        let fill = match lost {
                            Some(frames) if (frames as f64 - by_stamp).abs() * HNS_PER_SAMPLE <= STAMP_AGREE_HNS => frames as f64,
                            _ => by_stamp,
                        };
                        if fill > 0.0 {
                            self.append(fill.round() as usize, None);
                        }
                        self.stamps.observe(self.end.max(0) as u64, ts, now_hns);
                    } else {
                        // Held: neither anchored on nor (until it's dismissed) counted in the scatter.
                        self.suspect = Some((off, at, ts));
                    }
                }
                _ => {
                    // A suspect this one doesn't confirm was a stamp that was off: it counts in the scatter.
                    if let Some((_, s_at, s_ts)) = self.suspect.take() {
                        self.stamps.note_only(s_at, s_ts, now_hns);
                    }
                    self.stamps.observe(at, ts, now_hns);
                }
            }
        }
        self.append(samples.len(), Some(samples));
        self.delivered_hns = Some(now_hns);
    }

    /// Appends `n` samples (zeros without `src`), less any that are owed: those were served as zeros already. More than
    /// the FIFO holds loses the oldest of them.
    fn append(&mut self, n: usize, src: Option<&[f32]>) {
        let skip = (self.read - self.end).max(0).min(n as i64) as usize;
        let lose = (n - skip).saturating_sub(FIFO);
        let keep = n - skip - lose;
        // Overflow drops the oldest: the head passes `read`, and the next take serves zeros for what was lost.
        self.fifo.discard(keep.saturating_sub(self.fifo.free()));
        match src.and_then(|s| s.get(skip + lose..n)) {
            Some(s) => self.fifo.push(s),
            None => self.fifo.push_zeros(keep),
        };
        self.end += n as i64;
    }

    /// Whether the loopback is delivering: its last packet came within `stale_hns` of this wake, or two wakes and 5 ms
    /// when the wakes are further apart than its periods.
    fn fresh(&self) -> bool {
        let stale = self.stale_hns.max(2.0 * self.wake_gap + STALE_SLACK * HNS_PER_SAMPLE);
        self.delivered_hns.is_some_and(|d| self.now_hns - d <= stale)
    }

    /// The lead now by the stamps, in samples: the mic block's capture time less the tap time of the sample at `read`.
    fn stamped_lead(&self, mic: MicTime) -> Option<f64> {
        let t_ref = self.stamps.time_of(0)? + self.read as f64 * HNS_PER_SAMPLE;
        Some((mic.stamp? - t_ref) / HNS_PER_SAMPLE)
    }

    /// The lead now by delivery: the mic block timed back from the wake that delivered its newest sample, the sample at
    /// `read` from the wake that delivered the loopback's. `None` before the first packet.
    fn delivery_lead(&self, mic: MicTime) -> Option<f64> {
        let t_ref = self.delivered_hns? - (self.end - self.read) as f64 * HNS_PER_SAMPLE;
        Some((mic.delivered - t_ref) / HNS_PER_SAMPLE)
    }

    /// Moves the read position by `by` samples: forward discards, back serves zeros until it reaches the FIFO again.
    fn move_read(&mut self, by: i64) {
        self.read += by;
        let head = self.end - self.fifo.len() as i64;
        if self.read > head {
            self.fifo.discard((self.read - head) as usize);
        }
    }

    /// A jump of `by` samples at the same target, by choice: the readings so far, and the delivery's means, move with it
    /// (the loop's own low-pass is the caller's).
    fn moved(&mut self, by: f64) {
        self.move_read(by as i64);
        self.recent.shift(-by);
        self.delivery_mean.value -= by;
        self.slow.value -= by;
    }

    /// A jump of `by` samples that undoes a break: the readings come back to where they were before it, which is where
    /// the loop's low-pass, coasting since, still is, give or take what the few readings after the step got wrong (about
    /// ±0.3 ms on the cables' mic). So the drift fit starts over (its estimate stays meanwhile): that residual would be a
    /// step in its line. Means that may have broken too start over: the delivery-timed one beside the stamps, and timed by
    /// delivery the slow one (the centring after the break goes by what it finds).
    fn undid(&mut self, by: f64) {
        self.move_read(by as i64);
        self.recent.fill(self.lp.error());
        self.lp.refit();
        self.delivery_mean.n = 0;
        if !self.stamped {
            self.slow.n = 0;
        }
    }

    /// Starts over from the next reading, in the start-up: its timing changed.
    fn restart(&mut self) {
        (self.started, self.blocks, self.target, self.offset, self.settle_at) = (false, 0, self.floor, 0.0, START_BLOCKS);
        self.lp = LeadLoop::new();
        self.needs = Needs::new();
    }

    /// Fills `out` with this block's reference input and returns its length (480 ± a slip) for the resampler. `mic` is
    /// the block's capture time. Counts gaps, jumps after the start-up, and slips.
    pub(crate) fn take(&mut self, mic: MicTime, out: &mut [f32], c: &mut Counters) -> usize {
        self.facts = ReferenceFacts { lead_reading_ms: f32::NAN, ..ReferenceFacts::default() };
        let stamps_ok = mic.stamp.is_some() && self.stamps.trusted();
        if self.started && stamps_ok != self.stamped {
            if self.blocks < START_BLOCKS {
                // In the start-up the timing can change either way (a stream's first stamps were flagged, or its scatter
                // has just shown): it starts over from this reading.
                self.restart();
                self.facts.events |= ReferenceFacts::RESTARTED;
            } else if self.stamped {
                // The stamps are given up for the rest of the run: delivery times the loop from now on, on the stamps'
                // scale, so nothing moves.
                self.offset = if self.delivery_mean.n > 0 { self.delivery_mean.value - self.lp.error() } else { 0.0 };
                self.recent.fill(self.lp.error());
                self.slow = SlowMean { value: self.lp.error(), n: 100 };
                self.stamped = false;
                self.facts.events |= ReferenceFacts::UNSTAMPED;
            }
        }
        if !self.started {
            self.stamped = stamps_ok;
        }
        let delivery = self.delivery_lead(mic);
        let reading = if self.stamped { self.stamped_lead(mic) } else { delivery.map(|d| d - self.offset) };
        let Some(raw) = reading else {
            // Nothing delivered yet: zeros, and the timeline starts with the first packet.
            if let Some(o) = out.get_mut(..BLOCK) {
                o.fill(0.0);
            }
            c.reference_gaps += 1;
            self.facts.events |= ReferenceFacts::NO_READING;
            self.facts.missing = BLOCK as u16;
            self.finish_facts(None, false);
            return BLOCK;
        };
        // A reading that isn't finite tells the loop nothing: it coasts on its own estimate.
        let reading = raw.is_finite().then_some(raw);
        let mut raw = if raw.is_finite() { raw } else { self.target + self.lp.error() };
        let fresh = self.fresh();
        if !self.started {
            self.started = true;
            let by = (raw - self.target).round();
            self.move_read(by as i64);
            self.jumped(ReferenceFacts::ALIGNED, by);
            raw -= by;
            self.lp = LeadLoop::starting_at(raw - self.target);
            self.recent.fill(raw - self.target);
            self.delivery_mean = delivery.map_or(SlowMean::default(), |d| SlowMean { value: d - by - self.target, n: 1 });
            self.slow = SlowMean { value: raw - self.target, n: 1 };
        }
        self.blocks += 1;
        let starting = self.blocks <= START_BLOCKS;
        let mut e = raw - self.target;
        let (off, settled) = if self.stamped { (self.lp.error(), RELEASE) } else { (self.slow.value, UNSTAMPED_SETTLE) };
        if self.blocks == self.settle_at && off.abs() > settled {
            // The low-pass has settled (the start-up's end, or after a break without stamps): one jump centres it rather
            // than a slow slew (or, without stamps, none). Timed by delivery, by the slow mean.
            let by = off.round();
            self.moved(by);
            self.jumped(ReferenceFacts::CENTRED, by);
            raw -= by;
            e -= by;
            self.lp.realigned(by);
            if !starting {
                c.reference_drops += 1;
            }
        }

        // Stamped, a break is a step past the readings' own jitter as well (the two streams' stamps, each about its
        // scatter over √2); a reading twice that far from the low-pass is left out of it.
        let broken = if self.stamped {
            let jitter = ((mic.scatter * mic.scatter + self.stamps.scatter_hns().powi(2)) / 2.0).sqrt() / HNS_PER_SAMPLE;
            BROKEN.max(BREAK_JITTERS * jitter)
        } else {
            UNSTAMPED_BROKEN
        };
        let mut slip = if !fresh {
            // The loopback isn't delivering (silence without the keep-alive, a stall): the reading only counts how long.
            // The loop coasts, slipping on at its drift.
            self.facts.events |= ReferenceFacts::COASTED;
            self.lp.coast()
        } else {
            self.recent.push(e);
            let step = self.recent.median() - self.lp.error();
            if step.abs() > broken && self.recent.count_past(self.lp.error(), step.signum() * broken) >= BREAK_READINGS {
                // A broken timeline, a step that survives the median: one jump undoes it (by the mean of the readings past
                // it, all after the step, less the outermost two). The readings carry on where they were, and so does the
                // low-pass, which coasted meanwhile.
                let by = self.recent.mean_past(self.lp.error(), step.signum() * broken).round();
                self.undid(by);
                self.jumped(ReferenceFacts::BROKEN | ReferenceFacts::COASTED, by);
                if !self.stamped {
                    self.settle_at = self.blocks + START_BLOCKS;
                }
                if !starting {
                    c.reference_drops += 1;
                }
                self.lp.coast()
            } else if (e - self.lp.error()).abs() > 2.0 * broken {
                // One reading that far off: a break may be under way, or a stamp is off. The loop coasts. (Nearer, it's
                // used: leaving out the tail of the jitter on one side would pull the low-pass to the other.)
                self.facts.events |= ReferenceFacts::COASTED;
                self.lp.coast()
            } else if self.stamped {
                if let Some(d) = delivery {
                    self.delivery_mean.push(d - self.target);
                }
                self.lp.update(e, raw, !starting)
            } else {
                // Timed by delivery: the low-passes alone, and no slips but at what a stamped reading estimated before. A
                // step of the count (the drift's, once per beat of the clocks, or the delivery's lateness) is put back
                // in whole packets, once the slow mean shows it; not while a centring is due (after a break): that puts
                // back what's left in one. The count steps through a transition as wide as the wakes' jitter, which the
                // drift takes seconds to cross, and the mean can sit at half a packet meanwhile: after a step, one back
                // must go further.
                self.lp.track(e);
                self.slow.push(e);
                let off = self.slow.value;
                let back = off.signum() == -self.last_step;
                let past = (if back { UNSTAMPED_BACK } else { UNSTAMPED_STEP } * self.period).max(RAISE_MARGIN);
                if !starting && self.blocks > self.settle_at && off.abs() > past {
                    let by = (off / self.period).round() * self.period;
                    self.moved(by);
                    self.jumped(ReferenceFacts::STEPPED, by);
                    self.lp.realigned(by);
                    self.last_step = by.signum();
                    c.reference_drops += 1;
                }
                self.lp.coast()
            }
        };
        let mut n = slip.consumed_len();
        let avail = self.end - self.read;
        if slip == Slip::Drop && avail >= BLOCK as i64 && avail < n as i64 {
            // A drop would leave the block one sample short: it waits a block instead.
            self.lp.defer(slip);
            slip = Slip::None;
            n = BLOCK;
        }

        // The need: the least lead at which this take would be served whole.
        let spare = (avail - n as i64) as f64;
        let need = self.target + self.lp.error() - spare;
        let raise_at = if self.stamped { RAISE_SHORT } else { RAISE_SHORT_UNSTAMPED };
        let b = Bounds { target: self.target, floor: self.floor, cap: self.cap, raise_at };
        let delivered = self.delivered_hns == Some(self.now_hns);
        let t = Take { need, fresh, delivered, short: spare < 0.0, wake: self.now_hns };
        if let Some(by) = self.needs.update(t, starting, b) {
            // The same lead error against a target `by` higher (or lower), with the read position `by` earlier (later).
            self.target += by;
            self.move_read(-(by as i64));
            self.jumped(ReferenceFacts::RETARGETED, -by);
            self.lp.retargeted(by);
            if !starting {
                c.reference_drops += 1;
            }
        }

        (self.facts.read_index, self.facts.buffered) = (self.read, self.end - self.read);
        let owed = self.serve(n, out);
        if owed > 0 {
            c.reference_gaps += 1;
        }
        if slip != Slip::None {
            c.reference_slips += 1;
        }
        (self.facts.consumed, self.facts.owed, self.facts.fresh) = (n as u16, owed as u16, fresh);
        self.finish_facts(reading, delivered);
        n
    }

    /// The take's facts that read the state after it: the lead of the samples served (this take's `reading`, less the
    /// jumps made at it), the loop's lead, target and drift, the timing, and whether the loopback delivered at this wake.
    /// Before the first packet, where the read position is too.
    fn finish_facts(&mut self, reading: Option<f64>, delivered: bool) {
        let ms = |samples: f64| (samples * 1000.0 / SAMPLE_RATE as f64) as f32;
        let (lead, target, drift, stamped) = (self.lead_ms(), self.target_ms(), self.drift_ppm(), self.stamped());
        let f = &mut self.facts;
        f.lead_reading_ms = reading.map_or(f32::NAN, |r| ms(r - f.jumped as f64));
        if f.events & ReferenceFacts::NO_READING != 0 {
            (f.read_index, f.buffered) = (self.read, self.end - self.read);
        }
        (f.delivered, f.lead_ms, f.target_ms, f.drift_ppm, f.stamped) = (delivered, lead, target, drift, stamped);
    }

    /// Serves `n` samples from `read` into `out`: zeros for any before the FIFO's head (gone), the FIFO's samples, then
    /// zeros for any not yet delivered, which are owed. Returns how many were owed.
    fn serve(&mut self, n: usize, out: &mut [f32]) -> usize {
        let n = n.min(out.len());
        let len = self.fifo.len();
        // `read` is never past the head while the FIFO holds samples: `move_read` and `append` discard what's behind it.
        let head = self.end - len as i64;
        let before = (head - self.read).max(0).min(n as i64) as usize;
        self.facts.missing = before as u16;
        let real = (n - before).min(len);
        if let Some(z) = out.get_mut(..before) {
            z.fill(0.0);
        }
        if let Some(dst) = out.get_mut(before..before + real) {
            self.fifo.pop_into(dst);
        }
        if let Some(z) = out.get_mut(before + real..n) {
            z.fill(0.0);
        }
        self.read += n as i64;
        n - before - real
    }
}

/// The target's bounds when the need moves it: where it is, the least it comes down to, its ceiling, and when it rises.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    target: f64,
    floor: f64,
    cap: f64,
    /// The count of short takes that raises it.
    raise_at: f64,
}

/// One take, for the need's record: what it needed, whether the loopback was fresh (and delivered in this wake), whether it
/// was served zeros, and its wake.
#[derive(Clone, Copy, Debug)]
struct Take {
    need: f64,
    fresh: bool,
    delivered: bool,
    short: bool,
    wake: f64,
}

/// What the takes needed, for the target (the module doc): the start-up's need, and after it the leaky count of short
/// takes, this second's need and the seconds' before.
#[derive(Clone, Copy, Debug)]
struct Needs {
    start: TopK,
    short: f64,
    second: TopK,
    second_blocks: u32,
    second_fresh: u32,
    /// The wake the takes are in, and the most a fresh one of them needed: a need goes into the records once a wake, so
    /// that the takes reaching it are so many wakes.
    wake: f64,
    wake_need: f64,
    /// The needs of the wakes since the loopback last delivered (that one's too), not in the records yet: they go in when
    /// it delivers again, and are dropped if it goes stale first. A stall's first wakes are fresh, but what they needed
    /// (more each wake) says nothing of the delivery.
    pending: [f64; PENDING],
    pending_n: usize,
    /// The seconds' needs, a ring: `len` of them, the next going at `next`.
    past: [f64; LOWER_CHUNKS],
    len: usize,
    next: usize,
}

impl Needs {
    fn new() -> Self {
        let none = TopK::new();
        #[rustfmt::skip]
        let n = Self {
            start: none, short: 0.0, second: none, second_blocks: 0, second_fresh: 0, wake: f64::NEG_INFINITY,
            wake_need: f64::NEG_INFINITY, pending: [0.0; PENDING], pending_n: 0, past: [0.0; LOWER_CHUNKS], len: 0, next: 0,
        };
        n
    }

    /// One take's need, in the start-up or after it. Returns by how much to move the target, if at all.
    fn update(&mut self, t: Take, starting: bool, b: Bounds) -> Option<f64> {
        let Take { need, fresh, delivered, short, wake } = t;
        let raise = |level: f64| {
            let by = ((level + RAISE_MARGIN).min(b.cap) - b.target).round();
            (level > b.target - 0.5 * RAISE_MARGIN && by >= 1.0).then_some(by)
        };
        if wake != self.wake {
            // The last wake's need joins its run: a delivery now ends the run and it goes in, a stale wake drops it.
            let w = std::mem::replace(&mut self.wake_need, f64::NEG_INFINITY);
            if let Some(slot) = self.pending.get_mut(self.pending_n) {
                *slot = w;
                self.pending_n += 1;
            }
            if delivered || self.pending_n == PENDING {
                let run = self.pending;
                for &v in run.get(..self.pending_n).unwrap_or_default() {
                    if starting { self.start.insert(v) } else { self.second.insert(v) }
                }
                self.pending_n = 0;
            } else if !fresh {
                self.pending_n = 0;
            }
            self.wake = wake;
        }
        if fresh {
            self.wake_need = self.wake_need.max(need);
        }
        let mut by = None;
        if starting {
            by = raise(self.start.kth());
        } else {
            self.short *= SHORT_DECAY;
            if fresh {
                self.short += f64::from(u8::from(short));
                self.second_fresh += 1;
            }
            if self.short >= b.raise_at {
                by = raise(self.most(RAISE_CHUNKS).max(self.second.kth()));
            }
            self.second_blocks += 1;
            if self.second_blocks >= CHUNK_BLOCKS {
                if self.second_fresh >= CHUNK_MIN_FRESH {
                    self.remember(self.second.kth());
                }
                (self.second, self.second_blocks, self.second_fresh) = (TopK::new(), 0, 0);
                if by.is_none() && self.len == LOWER_CHUNKS {
                    let low = (self.most(LOWER_CHUNKS) + RAISE_MARGIN).max(b.floor);
                    let down = (low - b.target).round();
                    by = (down <= -LOWER_BY).then_some(down);
                }
            }
        }
        if by.is_some() {
            // The minute starts over, and so does the count.
            (self.short, self.len) = (0.0, 0);
        }
        by
    }

    fn remember(&mut self, need: f64) {
        if let Some(slot) = self.past.get_mut(self.next) {
            *slot = need;
        }
        self.next = (self.next + 1) % LOWER_CHUNKS;
        self.len = (self.len + 1).min(LOWER_CHUNKS);
    }

    /// The most of the last `k` seconds' needs (−∞ without any).
    fn most(&self, k: usize) -> f64 {
        (1..=k.min(self.len))
            .filter_map(|i| self.past.get((self.next + LOWER_CHUNKS - i) % LOWER_CHUNKS))
            .fold(f64::NEG_INFINITY, |a, &v| a.max(v))
    }
}

/// A mean of the values since it was set (up to `SLOW_BLOCKS` of them; `n` counts them), then a low-pass with that time
/// constant.
#[derive(Clone, Copy, Debug, Default)]
struct SlowMean {
    value: f64,
    n: u32,
}

impl SlowMean {
    fn push(&mut self, x: f64) {
        self.n = (self.n + 1).min(SLOW_BLOCKS);
        self.value += (x - self.value) / f64::from(self.n);
    }
}

/// The `RECUR` largest values seen, largest first (−∞ until there are as many).
#[derive(Clone, Copy, Debug)]
struct TopK([f64; RECUR]);

impl TopK {
    fn new() -> Self {
        Self([f64::NEG_INFINITY; RECUR])
    }

    fn insert(&mut self, mut x: f64) {
        // Carried down past the larger ones, swapping places with the first smaller one; NaN goes nowhere.
        for v in &mut self.0 {
            if x > *v {
                std::mem::swap(v, &mut x);
            }
        }
    }

    /// The `RECUR`-th largest: a value at least that many reached.
    fn kth(&self) -> f64 {
        self.0[RECUR - 1]
    }
}

/// The last `MEDIAN` lead errors, for their median.
#[derive(Clone, Copy, Debug)]
struct Recent {
    v: [f64; MEDIAN],
    next: usize,
}

impl Recent {
    fn new() -> Self {
        Self { v: [0.0; MEDIAN], next: 0 }
    }

    fn fill(&mut self, x: f64) {
        self.v = [x; MEDIAN];
    }

    fn push(&mut self, x: f64) {
        if let Some(slot) = self.v.get_mut(self.next) {
            *slot = x;
        }
        self.next = (self.next + 1) % MEDIAN;
    }

    fn shift(&mut self, by: f64) {
        for v in &mut self.v {
            *v += by;
        }
    }

    /// Sorted on the stack (total_cmp: a total order, so the sort can't panic).
    fn median(&self) -> f64 {
        let mut s = self.v;
        s.sort_unstable_by(f64::total_cmp);
        s[MEDIAN / 2]
    }

    /// How many of the values are more than `by` past `from` (in `by`'s direction).
    fn count_past(&self, from: f64, by: f64) -> usize {
        self.v.iter().filter(|&&v| (v - from - by) * by.signum() > 0.0).count()
    }

    /// The mean of the values more than `by` past `from` (in `by`'s direction), less `from`, leaving out the least and
    /// the most of them when there are three or more (a stamp that's off); 0 when there are none.
    fn mean_past(&self, from: f64, by: f64) -> f64 {
        let mut s = [0.0; MEDIAN];
        let mut n = 0;
        for &v in &self.v {
            if (v - from - by) * by.signum() > 0.0
                && let Some(slot) = s.get_mut(n)
            {
                *slot = v - from;
                n += 1;
            }
        }
        let past = s.get_mut(..n).unwrap_or_default();
        past.sort_unstable_by(f64::total_cmp);
        let mid = if n >= 3 { past.get(1..n - 1).unwrap_or_default() } else { past };
        if mid.is_empty() { 0.0 } else { mid.iter().sum::<f64>() / mid.len() as f64 }
    }
}

/// Holds the lead at its target with slips: at the drift rate, from a least-squares slope of the lead without our own
/// corrections, plus an offset correction only while the low-passed lead is outside a dead band. Delivery can't reach a
/// stamped lead, and the stamps' jitter (about ±1 ms raw, ±0.15 ms after the 1 s low-pass) stays inside the band, so the
/// slips follow the clocks alone.
#[derive(Clone, Copy, Debug)]
struct LeadLoop {
    /// The low-passed lead error, in samples.
    filt: f64,
    fit: SlopeFit,
    /// Our own moves of the lead (slips, chosen jumps), added back for the fit: what it sees is the clocks.
    own: f64,
    /// The drift estimate: the fit's slope, or the last one while a new fit isn't trusted yet.
    ppm: f64,
    /// Correcting the offset: from outside the dead band until back within its release.
    correcting: bool,
    /// Slip accumulator, in samples (|acc| < 1 after each step).
    acc: f64,
    /// Blocks since the fit's last value.
    coasted: u32,
}

/// The lead's low-pass coefficient per block: a 1 s time constant (1 - e^-0.01).
const ALPHA: f64 = 0.009_950_166_250_831_947;
/// The offset correction in ppm per sample: an offset decays with a time constant of 1 / (10 * 0.048) ≈ 2 s, damped
/// (ζ ≈ 0.7) behind the 1 s low-pass. Until the drift fit is trusted it alone follows the drift, `ppm / KC` off.
const KC: f64 = 10.0;
/// The slip rate's limit: 0.48 a block, so at most one slip per block.
const MAX_PPM: f64 = 1_000.0;

impl LeadLoop {
    fn new() -> Self {
        Self { filt: 0.0, fit: SlopeFit::default(), own: 0.0, ppm: 0.0, correcting: false, acc: 0.0, coasted: 0 }
    }

    /// A fresh loop from a lead error `e` (the alignment).
    fn starting_at(e: f64) -> Self {
        Self { filt: e, ..Self::new() }
    }

    /// One block's update from the lead error `e` and the lead `reading`; returns the slip to apply. The offset is
    /// corrected only when `correct` (not in the start-up).
    fn update(&mut self, e: f64, reading: f64, correct: bool) -> Slip {
        self.track(e);
        self.fit.push(reading + self.own, self.coasted + 1);
        self.coasted = 0;
        if let Some(s) = self.fit.slope() {
            self.ppm = s * 1e6 / BLOCK as f64;
        }
        let off = self.filt.abs();
        if off > DEADBAND && correct {
            self.correcting = true;
        } else if off < RELEASE {
            self.correcting = false;
        }
        let correction = if self.correcting { KC * self.filt } else { 0.0 };
        self.step(self.ppm + correction)
    }

    /// The low-pass alone.
    fn track(&mut self, e: f64) {
        self.filt += ALPHA * (e - self.filt);
    }

    /// One block on the drift estimate alone.
    fn coast(&mut self) -> Slip {
        self.coasted = self.coasted.saturating_add(1);
        self.step(self.ppm)
    }

    fn step(&mut self, ppm: f64) -> Slip {
        // Constant bounds, so clamp's assert can't fire; NaN slips nothing.
        let u = if ppm.is_finite() { ppm.clamp(-MAX_PPM, MAX_PPM) } else { 0.0 };
        self.acc += u * 1e-6 * BLOCK as f64;
        if self.acc >= 1.0 {
            self.acc -= 1.0;
            self.own += 1.0;
            Slip::Drop
        } else if self.acc <= -1.0 {
            self.acc += 1.0;
            self.own -= 1.0;
            Slip::Insert
        } else {
            Slip::None
        }
    }

    /// The slip `update` returned wasn't applied: it's owed to the next block.
    fn defer(&mut self, slip: Slip) {
        match slip {
            Slip::Drop => (self.acc, self.own) = (self.acc + 1.0, self.own - 1.0),
            Slip::Insert => (self.acc, self.own) = (self.acc - 1.0, self.own + 1.0),
            Slip::None => {}
        }
    }

    /// The read position moved forward by `by` samples by choice: the lead fell by `by`, the clocks didn't.
    fn realigned(&mut self, by: f64) {
        self.own += by;
        self.filt -= by;
    }

    /// The target rose by `by` (or fell, negative) and the read position went back (on) as far: the lead moved with it,
    /// the clocks didn't.
    fn retargeted(&mut self, by: f64) {
        self.own -= by;
    }

    /// The fit starts over; the drift estimate stays until the new one is trusted.
    fn refit(&mut self) {
        (self.fit, self.coasted) = (SlopeFit::default(), 0);
    }

    fn error(&self) -> f64 {
        self.filt
    }

    fn ppm(&self) -> f64 {
        self.ppm
    }
}

/// The drift fit's forgetting factor per block (a 20 s window), and what it needs before it's trusted: the weight of 10 s
/// of blocks in a row, (1 - λ^1000) / (1 - λ), and a slope whose standard error (from the fit's own residuals) is within
/// 10 ppm. The cables' mic stamps leave the slope within about ±10 ppm at 10 s and ±3 ppm at 20 s: their 1 s means wander
/// by about ±7 samples, so a 5 s fit was off by 20-30 ppm. Stamps jittering by ±3 ms took it 60 ppm off at 10 s, which the
/// standard error shows, and keeps out until it's down.
const FIT_LAMBDA: f64 = 0.9995;
#[cfg(test)]
const FIT_MIN: u32 = 1_000;
const FIT_MIN_WEIGHT: f64 = 787.0;
const FIT_MAX_SE: f64 = 10e-6 * BLOCK as f64;

/// An exponentially weighted least-squares line through values a whole number of blocks apart, O(1) per value. Time is
/// counted back from the newest, so the sums stay bounded, and blocks left out (the loop coasted) still count as time: a
/// silence's drift isn't a step.
#[derive(Clone, Copy, Debug, Default)]
struct SlopeFit {
    w: f64,
    t: f64,
    tt: f64,
    y: f64,
    ty: f64,
    yy: f64,
}

impl SlopeFit {
    /// The value `y`, `k` blocks after the last.
    fn push(&mut self, y: f64, k: u32) {
        // Every older point moves k blocks into the past (t → t - k), and all decay by λ^k; the new one is at t = 0.
        let k = f64::from(k.max(1));
        let d = FIT_LAMBDA.powf(k);
        self.tt = d * (self.tt - 2.0 * k * self.t + k * k * self.w);
        self.ty = d * (self.ty - k * self.y);
        self.t = d * (self.t - k * self.w);
        self.yy = d * self.yy + y * y;
        self.y = d * self.y + y;
        self.w = d * self.w + 1.0;
    }

    /// The slope per block, once the values' weight is `FIT_MIN_WEIGHT` (10 s of them; after a long gap, again) and its
    /// standard error is within `FIT_MAX_SE`.
    fn slope(&self) -> Option<f64> {
        if self.w < FIT_MIN_WEIGHT {
            return None;
        }
        // The weighted sums about the means.
        let sxx = self.tt - self.t * self.t / self.w;
        let sxy = self.ty - self.t * self.y / self.w;
        let syy = self.yy - self.y * self.y / self.w;
        if sxx <= 0.0 {
            return None;
        }
        let slope = sxy / sxx;
        let se2 = (syy - slope * sxy).max(0.0) / (self.w - 2.0) / sxx;
        (se2 <= FIT_MAX_SE * FIT_MAX_SE).then_some(slope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pos: u64, qpc: Option<f64>, frames: usize) -> Packet {
        Packet { frames, device_pos: pos, qpc_hns: qpc.map(|q| q as u64), discontinuity: false }
    }

    /// Index-coded samples, plus one so that a zero is a filled-in sample.
    fn idx(pos: u64, n: usize) -> Vec<f32> {
        (pos + 1..pos + 1 + n as u64).map(|v| v as f32).collect()
    }

    struct Rng(u64);

    impl Rng {
        /// Uniform in [-0.5, 0.5).
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        }
    }

    const T: f64 = 1e10;
    const PKT: f64 = 100_000.0;

    #[test]
    fn the_fit_finds_a_slope_through_jitter() {
        let mut f = SlopeFit::default();
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for k in 0..3000 {
            // 30 ppm (0.0144 samples a block) under ±48 samples of jitter, the cables' mic stamps.
            f.push(1000.0 + 0.0144 * k as f64 + 96.0 * rng.next(), 1);
            if k + 1 < FIT_MIN as usize {
                assert_eq!(f.slope(), None);
            }
        }
        let ppm = f.slope().unwrap() * 1e6 / 480.0;
        assert!((ppm - 30.0).abs() < 5.0, "{ppm}");
        // Values 2 blocks apart, on the same line: the same slope per block. After a minute without any, not trusted.
        let mut g = SlopeFit::default();
        for k in (0..30_000).step_by(2) {
            g.push(1000.0 + 0.0144 * k as f64, 2);
        }
        assert!((g.slope().unwrap() - 0.0144).abs() < 1e-9);
        g.push(0.0, 6_000);
        assert_eq!(g.slope(), None);
    }

    /// The loop slips at the drift alone: none for jitter, the drift's rate for drift, and the lead held within the dead
    /// band once the fit is in.
    #[test]
    fn the_loop_slips_at_the_drift_rate() {
        for ppm in [0.0, 30.0, -300.0, 500.0] {
            let mut lp = LeadLoop::new();
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
            let mut slips = 0u64;
            let mut worst: f64 = 0.0;
            for k in 0..12_000 {
                // The lead the clocks make, less our own slips; the reading adds ±48 samples of jitter.
                let lead = ppm * 1e-6 * 480.0 * k as f64 - lp.own;
                let reading = lead + 96.0 * rng.next();
                if lp.update(reading, reading, true) != Slip::None {
                    slips += 1;
                }
                if k >= 2_000 {
                    worst = worst.max(lead.abs());
                }
            }
            // Within the drift, give or take where in the dead band (±24) the lead ends up.
            let expect = ppm.abs() * 1e-6 * 48_000.0 * 120.0;
            assert!((slips as f64 - expect).abs() <= expect * 0.02 + 30.0, "{ppm} ppm: {slips} slips vs {expect:.0}");
            assert!(worst < 60.0, "{ppm} ppm: the lead went {worst:.0} samples off");
            assert!((lp.ppm() - ppm).abs() < 0.1 * ppm.abs() + 5.0, "{ppm} ppm: estimated {}", lp.ppm());
        }
    }

    /// A short FIFO is served what it holds and zeros; the late samples are discarded when they arrive, so the next block
    /// carries on where the timeline is. After a jump back, zeros come first.
    #[test]
    fn owed_samples_are_discarded_when_they_arrive() {
        let mut r = Reference::new(576, 480);
        let mut out = [0.0f32; BLOCK + 1];
        r.push(&packet(0, None, 480), &idx(0, 480), 0.0);
        r.move_read(180);
        assert_eq!(r.serve(480, &mut out), 180, "300 served, 180 owed");
        assert_eq!((out[0], out[299]), (181.0, 480.0));
        assert!(out[300..480].iter().all(|&v| v == 0.0));
        // The next packet's first 180 are owed and dropped: the next block starts at 660.
        r.push(&packet(480, None, 480), &idx(480, 480), 0.0);
        assert_eq!(r.fifo_len(), 300);
        assert_eq!(r.serve(300, &mut out), 0);
        assert_eq!(out[0], 661.0);
        // A jump back past the FIFO's head: zeros, then the FIFO.
        r.push(&packet(960, None, 480), &idx(960, 480), 0.0);
        r.move_read(-100);
        assert_eq!(r.serve(480, &mut out), 0);
        assert!(out[..100].iter().all(|&v| v == 0.0));
        assert_eq!(out[100], 961.0);
        // More zeros than the FIFO holds, none owed: it keeps the newest of them, and the timeline's indices.
        r.append(FIFO + 1000, None);
        r.push(&packet(1440 + (FIFO + 1000) as u64, None, 480), &idx(5000, 480), 0.0);
        assert_eq!(r.fifo_len(), FIFO);
        r.move_read(r.end - 480 - r.read);
        assert_eq!(r.serve(480, &mut out), 0);
        assert_eq!(out[0], 5001.0);
    }

    #[test]
    fn lost_loopback_packets_are_zero_filled_by_position() {
        let mut r = Reference::new(576, 480);
        r.push(&packet(0, None, 480), &idx(0, 480), 0.0);
        // 480 frames lost.
        r.push(&packet(960, None, 480), &idx(960, 480), 0.0);
        assert_eq!((r.end, r.fifo_len()), (1440, 1440));
        // A position going backwards, or forward by more than 100 ms, is left alone.
        r.push(&packet(0, None, 480), &idx(0, 480), 0.0);
        r.push(&packet(100_000, None, 480), &idx(0, 480), 0.0);
        assert_eq!(r.end, 2400);
    }

    /// A loopback stamp that's off on its own (5 ms late or early) puts nothing in; two in a row agreeing that 240
    /// samples never came put them in, as does one with the device position past the position fill.
    #[test]
    fn a_stamp_gap_needs_confirming() {
        let run = |offs: &[f64], jump_at: Option<usize>| {
            let mut r = Reference::new(576, 480);
            let mut pos = 0u64;
            for (k, off) in offs.iter().enumerate() {
                if jump_at == Some(k) {
                    pos += 10_000;
                }
                let t = T + k as f64 * PKT + off;
                r.push(&packet(pos, Some(t), 480), &idx(pos, 480), t + PKT);
                pos += 480;
            }
            r.end - 480 * offs.len() as i64
        };
        assert_eq!(run(&[0.0, 0.0, 50_000.0, 0.0, 0.0], None), 0, "late on its own");
        assert_eq!(run(&[0.0, 0.0, -50_000.0, 0.0, 0.0], None), 0, "early on its own");
        assert_eq!(run(&[0.0, 0.0, 50_000.0, 50_000.0, 50_000.0], None), 240, "confirmed by the next");
        assert_eq!(run(&[0.0, 0.0, -50_000.0, -50_000.0, -50_000.0], None), 0, "early, confirmed: re-anchored, nothing in");
        // The position jumped 10_000 frames, and the stamp says by how much time: 208.3 ms.
        assert_eq!(run(&[0.0, 0.0, 2_083_333.3, 2_083_333.3], Some(2)), 10_000, "confirmed by the position");
    }

    /// The lead comes from the stamps: delivering the same packets late or in a burst doesn't change it.
    #[test]
    fn the_stamped_lead_ignores_delivery() {
        let mic = MicTime { stamp: Some(T + 5.0 * PKT), delivered: T + 5.0 * PKT, scatter: 0.0 };
        let lead = |late: f64, burst: bool| {
            let mut r = Reference::new(576, 480);
            for k in 0..4u64 {
                let at = if burst { T + 4.0 * PKT } else { T + (k + 1) as f64 * PKT } + late;
                r.push(&packet(k * 480, Some(T + k as f64 * PKT), 480), &idx(k * 480, 480), at);
            }
            r.stamped_lead(mic).unwrap()
        };
        // Sample 0 was tapped at T, so the mic block five packets later leads it by 2400.
        for (late, burst) in [(0.0, false), (200_000.0, false), (0.0, true)] {
            assert!((lead(late, burst) - 2400.0).abs() < 1e-6, "{late} {burst}");
        }
        // By delivery: the newest sample (1920) taken as tapped when it was read.
        let mut r = Reference::new(576, 480);
        r.push(&packet(0, None, 1920), &idx(0, 1920), T + 4.0 * PKT);
        assert!((r.delivery_lead(mic).unwrap() - (480.0 + 1920.0)).abs() < 1e-6);
        assert_eq!(r.stamped_lead(mic), None, "no loopback stamps");
        assert_eq!(Reference::new(576, 480).delivery_lead(mic), None, "nothing delivered");
    }

    /// A drop that would leave the block a sample short waits a block, rather than serving a zero.
    #[test]
    fn a_drop_at_a_packet_boundary_waits_a_block() {
        let mut r = Reference::new(576, 480);
        let mut c = Counters::default();
        let mut out = [0.0f32; BLOCK + 1];
        // Past the start-up, at the target (by delivery: the mic block's time puts the reading at 576), with exactly a
        // block buffered and the loop about to drop.
        r.push(&packet(0, None, 480), &idx(0, 480), 0.0);
        (r.started, r.blocks) = (true, START_BLOCKS + 10);
        (r.lp.acc, r.lp.ppm) = (0.9999, 1000.0);
        let mic = |r: &Reference| MicTime { stamp: None, delivered: (576.0 - (r.end - r.read) as f64) * HNS_PER_SAMPLE, scatter: 0.0 };
        assert_eq!(r.take(mic(&r), &mut out, &mut c), BLOCK);
        assert_eq!((c.reference_slips, c.reference_gaps, out[0], out[479]), (0, 0, 1.0, 480.0), "deferred");
        r.push(&packet(480, None, 960), &idx(480, 960), 0.0);
        assert_eq!(r.take(mic(&r), &mut out, &mut c), BLOCK + 1);
        assert_eq!((c.reference_slips, out[0], out[480]), (1, 481.0, 961.0), "applied a block later");
    }

    /// The need counts once three wakes' fresh takes reach it (two takes in a wake are one), and only once the loopback
    /// has delivered again: a stall's wakes, fresh until it goes stale, don't count. Nor do stale takes count towards a
    /// raise after the start-up, which needs short takes to persist; and the target comes down after five minutes 3 ms
    /// under it.
    #[test]
    fn the_need_moves_the_target() {
        let b = Bounds { target: 576.0, floor: 576.0, cap: 2400.0, raise_at: RAISE_SHORT };
        // (need, fresh, wake, delivered in it)
        let run = |n: &mut Needs, takes: &[(f64, bool, f64, bool)]| {
            takes
                .iter()
                .map(|&(need, fresh, wake, delivered)| n.update(Take { need, fresh, delivered, short: true, wake }, true, b))
                .collect::<Vec<_>>()
        };
        let mut n = Needs::new();
        #[rustfmt::skip]
        let stall = [
            (500.0, true, 1.0, true), (500.0, true, 2.0, true),
            // A stall: three fresh wakes, more each, then stale.
            (900.0, true, 3.0, true), (1400.0, true, 4.0, false), (1900.0, true, 5.0, false), (2400.0, false, 6.0, false),
            (500.0, true, 7.0, true), (500.0, true, 8.0, true),
        ];
        assert!(run(&mut n, &stall).iter().all(Option::is_none), "a stall raises nothing");
        #[rustfmt::skip]
        let late = [
            // A late mic packet's second block needs 10 ms more, in three wakes (one with two such takes).
            (1000.0, true, 9.0, true), (1000.0, true, 9.0, true), (500.0, true, 10.0, true), (1000.0, true, 11.0, false),
            (500.0, true, 12.0, true), (1000.0, true, 13.0, true),
        ];
        assert!(run(&mut n, &late).iter().all(Option::is_none), "two wakes");
        assert_eq!(run(&mut n, &[(500.0, true, 14.0, true)]), [Some(1000.0 + 144.0 - 576.0)], "the third wake, at the next");
        // After the start-up, short stale takes don't count towards a raise.
        let mut n = Needs::new();
        for k in 0..3000 {
            assert_eq!(n.update(Take { need: 3000.0, fresh: false, delivered: false, short: true, wake: f64::from(k) }, false, b), None);
        }
        // 0.6 s of short fresh takes raise it, to the third largest need + 3 ms.
        let mut raised = None;
        for k in 0..300 {
            let t = Take { need: 700.0 + f64::from(k % 7), fresh: true, delivered: true, short: true, wake: f64::from(3000 + k) };
            if let Some(by) = n.update(t, false, b) {
                raised = Some((k, by));
                break;
            }
        }
        assert_eq!(raised, Some((61, 706.0 + 144.0 - 576.0)));
        // Five minutes of needs 4 ms under the target lower it to them + 3 ms.
        let b = Bounds { target: 1200.0, ..b };
        let mut n = Needs::new();
        let mut lowered = None;
        for k in 0..31_000 {
            let t = Take { need: 1200.0 - 192.0 - 144.0, fresh: true, delivered: true, short: false, wake: f64::from(k) };
            if let Some(by) = n.update(t, false, b) {
                lowered = Some((k, by));
                break;
            }
        }
        assert_eq!(lowered, Some((29_999, -192.0)));
    }

    /// The loopback is fresh for two periods and 5 ms after its last packet (two 10 ms wakes), or two wakes and 5 ms when
    /// the wakes are further apart.
    #[test]
    fn freshness_follows_the_periods_and_the_wakes() {
        let mut r = Reference::new(576, 480);
        assert!(!r.fresh(), "nothing delivered");
        r.wake(T);
        r.push(&packet(0, None, 480), &idx(0, 480), T);
        for (at, fresh) in [(10.0, true), (20.0, true), (30.0, false)] {
            r.wake(T + at * 1e4);
            assert_eq!(r.fresh(), fresh, "{at} ms");
        }
        // 20 ms wakes: a packet two wakes back is still fresh.
        let mut r = Reference::new(576, 480);
        for k in 0..20 {
            r.wake(T + f64::from(k) * 2e5);
        }
        r.push(&packet(0, None, 480), &idx(0, 480), r.now_hns);
        for (wakes, fresh) in [(1.0, true), (2.0, true), (3.0, false)] {
            r.wake(T + (19.0 + wakes) * 2e5);
            assert_eq!(r.fresh(), fresh, "{wakes} wakes");
        }
    }

    #[test]
    fn the_top_k_and_the_median_ignore_outliers() {
        let mut t = TopK::new();
        for x in [5.0, f64::NAN, 1.0, 9.0, 3.0, 7.0] {
            t.insert(x);
        }
        assert_eq!(t.0, [9.0, 7.0, 5.0]);
        let mut r = Recent::new();
        r.fill(0.0);
        for x in [500.0, -500.0, 500.0, 500.0, 500.0] {
            r.push(x);
        }
        assert_eq!(r.median(), 0.0, "four of nine");
        r.push(500.0);
        assert_eq!(r.median(), 500.0, "five");
    }
}
