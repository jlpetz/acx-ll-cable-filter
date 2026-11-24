//! Which of the latency test's sweeps the engine's own events disturbed (TODO 13). The tap carries each block's output and
//! thread facts, the output's counts since the block before and the echo-path guard's hold (pipeline.rs's `TapItem`), so the
//! test knows, block for block in the tap's own samples, where a mic glitch or stall, an output underrun, deficit, coarse
//! drop, re-anchor, target move or hold move came (on the direct driver route, a move of the ring's headroom or of the
//! reader's packet: the owner, 2026-11-24), what the drift loop slipped, and where the output queue stood against its
//! target. A sweep is disturbed when:
//! - **an event** came in its window: from [`MARGIN_MS`] before its first sample at the app's input to its app latency and
//!   [`MARGIN_SLACK_MS`] after its last (at least [`MARGIN_MS`]);
//! - **the drift loop moved the path under it:** its net slips over the window at more than [`TRIM_SLIPS_PER_S`];
//! - **the queue was still recovering:** the filter's own estimate of its output queue (the fill less its target, a 100 ms
//!   mean) more than [`RECOVERED_MS`] from its level while the drift loop trims it back, after an event (the level the queue's
//!   before it), or from the tap's first block when the queue began [`FAR_MS`] or more from its target (something before the
//!   test moved it; the level the target). A recovery ends where the estimate is back and the slips have settled too.
//!
//! Why the recovery counts: with today's defaults (exclusive mic and output, the adaptive target) four runs in eight of
//! 2026-11-23 had the thread held up 13-16 ms, a mic glitch and a late answer at it, and the estimate then 9-13 ms over its
//! target; the drift loop trimmed it at its top rate (1.5 ms a second), slower as it neared its level, and the sweeps
//! meanwhile read 3.00 ms less each 2 s. The runs without one agreed within 0.6 ms. With the mic shared, one run had a late
//! answer at 0.2 s and another started 8 ms under its target with no event at all: both recoveries ran on through the sweeps
//! (the tap starts about 2 s in), which only the slips and the start's queue show.
//!
//! Known limits:
//! - after a late answer the estimate and the path no longer agree: the sweeps read 1-4 ms less against the readout than
//!   before it, through the recovery (how much depends on where the train is taken to start: the logs don't mark it), so
//!   the queue's distance from its level is the filter's own estimate, not the sweeps' displacement, and a sweep after the
//!   recovery may read off the ones before the event. The undisturbed sweeps on either side of an event are told apart
//!   (`SweepDisturbance::events_before`), and the report says when its undisturbed figure spans one;
//! - a path the drift loop moves at under [`TRIM_SLIPS_PER_S`] (a start-up's settling, the end of a recovery's overshoot)
//!   isn't seen: the clean runs' first sweeps read up to 0.9 ms under their last so;
//! - a recovery from before the test that began within [`FAR_MS`] of its target is seen by its slips alone;
//! - the devices' own moves (a cable's) are in no block's facts.

use nf_core::telemetry::OutputEvents;
use nf_core::{BLOCK, SAMPLE_RATE};

use crate::api::{OutputFacts, QueueRecovery, SweepDisturbance, ThreadFacts};

/// An event this close to a sweep's samples at the app's input (before its first, and at least this after its last) disturbs
/// it: a mic glitch's zeros land in the blocks after it, and the margin before covers a block's own timing.
pub(crate) const MARGIN_MS: f64 = 100.0;
/// After its last sample, an event up to the sweep's app latency and this much more disturbs it: an output event moves every
/// sample still queued, and the sweep's last samples are queued until its app latency has passed.
pub(crate) const MARGIN_SLACK_MS: f64 = 25.0;
/// The drift loop moves the path under a sweep when its net slips over the sweep's window come faster than this (samples a
/// second: 0.5 ms a second, 500 ppm). Past any crystal clock's drift (health's own "drifting" is 300 ppm), and over the
/// clean runs' (at most 17 in a sweep's 1.2 s, the defaults' start-up settling); the drift loop's top rate is 72 (1500 ppm),
/// which a recovery runs at for seconds.
pub(crate) const TRIM_SLIPS_PER_S: f64 = 24.0;
/// The queue's level before an event: the median over this many blocks before it (0.5 s), of those neither events nor
/// recovering from an earlier one; 0 (the target) without any. Also the tap's first blocks that tell a recovery under way.
const LEVEL_BLOCKS: usize = 50;
/// The queue is read as a mean over this many blocks from each one (100 ms): the fill steps a device period at a time, and a
/// recovery's queue would dip within reach of its level for a block at the bottom of a step.
const SMOOTH_BLOCKS: usize = 10;
/// The queue is back when its mean is this close to its level (ms): the runs' steady fill stays within 2.5 ms of its target
/// block to block, and its 100 ms mean within about 1.
const RECOVERED_MS: f32 = 1.0;
/// The queue is back only once the drift loop's net slips over this many blocks from there (a second) have settled under
/// [`TRIM_SLIPS_PER_S`] too: where a recovery's estimate was first back on 2026-11-23, the loop was still trimming at 4-41 a
/// second.
const SETTLE_BLOCKS: usize = 100;
/// A recovery under way at the tap's first block: the median of its first [`LEVEL_BLOCKS`] (those before its first event, at
/// least [`SMOOTH_BLOCKS`]) this far from the target (ms). A steady queue's median sits within about 1 ms of it, the defaults'
/// start-up's 2 s in within 1.1; the recoveries from before the test on 2026-11-23 were 5-6 ms out there.
const FAR_MS: f32 = 2.5;

/// One tapped block's facts, as the block left them: the output's and the thread's (`Pipeline::last_output`, `last_thread`),
/// the output's counts since the block before (its corrections, exactly as the engine counts them, and its slips), and the
/// echo-path guard's hold (samples).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TapFacts {
    pub(crate) output: OutputFacts,
    pub(crate) thread: ThreadFacts,
    pub(crate) events: OutputEvents,
    pub(crate) hold: u32,
}

/// One sweep at the app's input: its repetition (from 1), where its first sample is in the tap's samples, and its app latency
/// (ms), which sets its window's end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SweepAt {
    pub(crate) repetition: u32,
    pub(crate) position: f64,
    pub(crate) app_ms: f64,
}

/// The tapped blocks' events over the test: the output's (as the per-sweep counts add them, slips included), the mic's
/// glitches and its stalls.
pub(crate) fn totals(blocks: &[TapFacts]) -> (OutputEvents, u64, u64) {
    blocks.iter().fold((OutputEvents::default(), 0u64, 0u64), |(mut e, g, s), b| {
        e.add(&b.events);
        (e, g + u64::from(b.thread.mic_glitches), s + u64::from(b.thread.stalls))
    })
}

/// Each sweep's disturbance, in `sweeps`' order. Block `i` of `blocks` holds the tap's samples from `i * BLOCK`; `sweep_len` is
/// a sweep's length in samples.
pub(crate) fn disturbances(blocks: &[TapFacts], sweeps: &[SweepAt], sweep_len: usize) -> Vec<SweepDisturbance> {
    let events = block_events(blocks);
    let evented: Vec<bool> = events.iter().map(SweepDisturbance::any).collect();
    let rec = recovery(blocks, &events, &evented);
    let fs = f64::from(SAMPLE_RATE);
    let before = MARGIN_MS * 1e-3 * fs;
    sweeps
        .iter()
        .map(|s| {
            let after = MARGIN_MS.max(s.app_ms + MARGIN_SLACK_MS) * 1e-3 * fs;
            let from = (((s.position - before) / BLOCK as f64).floor().max(0.0) as usize).min(blocks.len());
            let to = (((s.position + sweep_len as f64 + after) / BLOCK as f64).ceil().max(0.0) as usize).clamp(from, blocks.len());
            let mut d = SweepDisturbance {
                repetition: s.repetition,
                events_before: u32::try_from(evented[..from].iter().filter(|&&e| e).count()).unwrap_or(u32::MAX),
                ..SweepDisturbance::default()
            };
            for (e, r) in events[from..to].iter().zip(&rec[from..to]) {
                add(&mut d, e);
                if r.kind != QueueRecovery::None {
                    if d.queue == QueueRecovery::None || r.off_ms.abs() > d.queue_off_ms.abs() {
                        d.queue_off_ms = r.off_ms;
                    }
                    // From the test's start when any of its blocks' recovery was.
                    if d.queue != QueueRecovery::FromStart {
                        d.queue = r.kind;
                    }
                }
            }
            let window_s = ((to - from) * BLOCK) as f64 / fs;
            if window_s > 0.0 && d.output.slip_net.unsigned_abs() as f64 > TRIM_SLIPS_PER_S * window_s {
                d.trim_samples = d.output.slip_net;
            }
            d
        })
        .collect()
}

/// Each block's own events (a [`SweepDisturbance`] of one block, its repetition and queue unset): its mic glitches and stalls,
/// its output's counts (slips included: they make no event), and a move of the target or the echo-path hold from the block
/// before's. On the direct driver route the target is one reader packet + the ring's headroom (ring_headroom.rs), so a move
/// of it is the headroom's or the packet's (or both), named as those: no adaptive target runs there.
fn block_events(blocks: &[TapFacts]) -> Vec<SweepDisturbance> {
    let mut prev_target = None;
    let mut prev_hold = None;
    // The direct route's last fed block: its reader's packet and the headroom.
    let mut prev_ring: Option<(u32, u32)> = None;
    blocks
        .iter()
        .map(|b| {
            // A failed write leaves the block's output facts at their defaults (no target): it moves nothing. So does a block
            // the direct route discarded with no app recording (no target either).
            let target = b.output.target;
            let target_moved = target > 0 && prev_target.is_some_and(|p| p != target);
            if target > 0 {
                prev_target = Some(target);
            }
            let ring = (target > 0 && b.output.packet > 0).then_some((b.output.packet, b.output.headroom));
            let (packet_moved, headroom_moved) = match (prev_ring, ring) {
                (Some((p0, h0)), Some((p1, h1))) => (p0 != p1, h0 != h1),
                _ => (false, false),
            };
            if ring.is_some() {
                prev_ring = ring;
            }
            let hold_moved = prev_hold.is_some_and(|p| p != b.hold);
            prev_hold = Some(b.hold);
            SweepDisturbance {
                mic_glitches: b.thread.mic_glitches,
                mic_stalls: b.thread.stalls,
                output: b.events,
                target_moves: u32::from(target_moved && ring.is_none()),
                hold_moves: u32::from(hold_moved),
                headroom_moves: u32::from(headroom_moved),
                packet_moves: u32::from(packet_moved),
                ..SweepDisturbance::default()
            }
        })
        .collect()
}

/// Adds one block's events to a sweep's.
fn add(d: &mut SweepDisturbance, e: &SweepDisturbance) {
    d.mic_glitches = d.mic_glitches.saturating_add(e.mic_glitches);
    d.mic_stalls = d.mic_stalls.saturating_add(e.mic_stalls);
    d.target_moves = d.target_moves.saturating_add(e.target_moves);
    d.hold_moves = d.hold_moves.saturating_add(e.hold_moves);
    d.headroom_moves = d.headroom_moves.saturating_add(e.headroom_moves);
    d.packet_moves = d.packet_moves.saturating_add(e.packet_moves);
    d.output.add(&e.output);
}

/// One block's part in a recovery: whether one was under way (and which), and the queue's distance from its level (ms, its
/// 100 ms mean from the block on; 0 where unknown).
#[derive(Clone, Copy, Debug, Default)]
struct BlockRecovery {
    kind: QueueRecovery,
    off_ms: f32,
}

/// Per block, the output queue's recovery. One starts after an event block, at the level of the blocks before it, or at the
/// tap's first block when its first blocks' median is [`FAR_MS`] or more from the target (at the target's level; then an
/// event in it carries on at that level, as the queue before the event was still recovering); it carries
/// on through later events (at the same level), and ends at the first block whose mean is back within [`RECOVERED_MS`] of its
/// level with the drift loop's slips over the next second settled. The event blocks themselves are named for their events.
fn recovery(blocks: &[TapFacts], events: &[SweepDisturbance], evented: &[bool]) -> Vec<BlockRecovery> {
    let ms_per_sample = 1000.0 / SAMPLE_RATE as f32;
    // The fill the drift loop read less the target it held; NaN for a failed write's block, which has neither.
    let excess: Vec<f32> = blocks
        .iter()
        .map(|b| if b.output.target > 0 { (b.output.fill - b.output.target as f32) * ms_per_sample } else { f32::NAN })
        .collect();
    // The drift loop's net slips from each block to the end: a stretch's is a difference.
    let mut slips_from = vec![0i64; blocks.len() + 1];
    for i in (0..blocks.len()).rev() {
        slips_from[i] = slips_from[i + 1] + events[i].output.slip_net;
    }
    let settled = |i: usize| {
        let end = (i + SETTLE_BLOCKS).min(blocks.len());
        let s = ((end - i) * BLOCK) as f64 / f64::from(SAMPLE_RATE);
        s == 0.0 || ((slips_from[i] - slips_from[end]).unsigned_abs() as f64) < TRIM_SLIPS_PER_S * s
    };
    let mut rec = vec![BlockRecovery::default(); blocks.len()];
    // A recovery under way: its kind and level.
    let mut under_way: Option<(QueueRecovery, f32)> = None;
    // The tap's first blocks, before its first event: a queue far from its target there was moved before the test.
    let first: Vec<f32> =
        (0..blocks.len()).take_while(|&j| !evented[j]).filter(|&j| excess[j].is_finite()).map(|j| excess[j]).take(LEVEL_BLOCKS).collect();
    if first.len() >= SMOOTH_BLOCKS && median(first).is_some_and(|m| m.abs() >= FAR_MS) {
        under_way = Some((QueueRecovery::FromStart, 0.0));
    }
    for i in 0..blocks.len() {
        if evented[i] {
            if under_way.is_none() {
                under_way = Some((QueueRecovery::AfterEvent, level_before(&excess, evented, &rec, i)));
            }
            continue;
        }
        let Some((kind, level)) = under_way else { continue };
        let window = &excess[i..(i + SMOOTH_BLOCKS).min(excess.len())];
        let (sum, n) = window.iter().filter(|v| v.is_finite()).fold((0.0f32, 0u32), |(s, n), v| (s + v, n + 1));
        let d = if n > 0 { sum / n as f32 - level } else { 0.0 };
        if n > 0 && d.abs() <= RECOVERED_MS && settled(i) {
            under_way = None;
        } else {
            rec[i] = BlockRecovery { kind, off_ms: d };
        }
    }
    rec
}

/// The queue's level before block `i`: the median excess over the last [`LEVEL_BLOCKS`] blocks before it that were neither
/// events nor recovering; 0 (the target) without any.
fn level_before(excess: &[f32], evented: &[bool], rec: &[BlockRecovery], i: usize) -> f32 {
    let v: Vec<f32> = (0..i)
        .rev()
        .filter(|&j| !evented[j] && rec[j].kind == QueueRecovery::None && excess[j].is_finite())
        .map(|j| excess[j])
        .take(LEVEL_BLOCKS)
        .collect();
    median(v).unwrap_or(0.0)
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` steady blocks: the fill at the target give or take a sawtooth of 0.5 ms (its median and its 100 ms means at the
    /// target), nothing happening.
    fn steady(n: usize, target: u32) -> Vec<TapFacts> {
        (0..n)
            .map(|i| {
                let fill = target as f32 + [0.0, 24.0, 0.0, -24.0][i % 4];
                TapFacts { output: OutputFacts { fill, target, ..OutputFacts::default() }, ..TapFacts::default() }
            })
            .collect()
    }

    /// Block `i` slipped one sample (`net` -1 dropped, +1 inserted).
    fn slip(b: &mut TapFacts, net: i8) {
        b.output.slip = net;
        b.events.slips = 1;
        b.events.slip_net = i64::from(net);
    }

    /// A sweep at `block`, with an app latency of 40 ms (its window: 100 ms before it, 100 ms after it).
    fn sweep(repetition: u32, block: usize) -> SweepAt {
        SweepAt { repetition, position: (block * BLOCK) as f64, app_ms: 40.0 }
    }

    /// 1 s at 48 kHz: the test's sweep.
    const SWEEP: usize = 48_000;

    /// A late wake at block `at` as the 2026-11-23 runs had it (spread-defaults-4.csv, block 258): a mic glitch at one block,
    /// a late answer with a period of silence at the next, the fill then 12 ms (576 frames) over the target and trimmed back
    /// at 0.72 a block (1.5 ms a second, the loop's top rate), a sample dropped every block while it's over (the drift loop
    /// trimmed on for a second and more after the estimate was back on 2026-11-23).
    fn late_wake(blocks: &mut [TapFacts], at: usize) {
        blocks[at].thread.mic_glitches = 1;
        blocks[at + 1].output = OutputFacts { underrun: true, late: true, silence: 160, ..blocks[at + 1].output };
        blocks[at + 1].events = OutputEvents { underruns: 1, late_answers: 1, silence_frames: 160, ..OutputEvents::default() };
        let mut over = 576.0f32;
        for (k, b) in blocks.iter_mut().enumerate().skip(at + 1) {
            b.output.fill += over.max(0.0);
            if over > 0.0 && k > at + 1 {
                slip(b, -1);
            }
            over -= 0.72;
        }
    }

    #[test]
    fn steady_blocks_disturb_nothing() {
        let blocks = steady(1_000, 736);
        let d = disturbances(&blocks, &[sweep(1, 50), sweep(2, 250), sweep(3, 450)], SWEEP);
        assert_eq!(d.len(), 3);
        for (i, s) in d.iter().enumerate() {
            assert_eq!(s.repetition, i as u32 + 1);
            assert!(!s.any(), "{s:?}");
            assert_eq!(s.text(), "");
            assert_eq!(s.events_before, 0);
        }
        // A sweep past the tap's last block has no blocks to be disturbed by.
        assert!(!disturbances(&[], &[sweep(1, 0)], SWEEP)[0].any());
        assert!(!disturbances(&blocks, &[sweep(1, 2_000)], SWEEP)[0].any());
        // A clock drift the drift loop follows (300 ppm: 14 slips a second), the fill at its target: no trimming.
        let mut drift = steady(1_000, 736);
        for b in drift.iter_mut().step_by(7) {
            slip(b, -1);
        }
        let d = disturbances(&drift, &[sweep(1, 50), sweep(2, 250)], SWEEP);
        assert!(!d[0].any() && !d[1].any(), "{d:?}");
        assert_eq!(d[0].output.slips, 17, "its slips counted, as no event");
    }

    /// The 2026-11-23 runs' pattern: the sweep the late wake came in names both events and the queue, the ones after the queue
    /// and the slips, by less each time; the one before it is clean. With the estimate back but the loop still trimming, a
    /// sweep is still disturbed by the slips; once they settle, it's clean, but on the far side of the event.
    #[test]
    fn a_late_wake_disturbs_its_sweep_and_the_recovery_the_ones_after() {
        let mut blocks = steady(1_600, 736);
        late_wake(&mut blocks, 258);
        // Sweeps every 2 s, from 0.3 s: one before the event, one just after it, two more, then two more as the loop ends.
        let sweeps = [sweep(1, 30), sweep(2, 230), sweep(3, 430), sweep(4, 630), sweep(5, 1030), sweep(6, 1430)];
        let d = disturbances(&blocks, &sweeps, SWEEP);
        assert!(!d[0].any(), "before the event: {:?}", d[0]);
        assert_eq!(d[0].events_before, 0);
        assert_eq!((d[1].mic_glitches, d[1].output.underruns, d[1].output.late_answers, d[1].output.silence_frames), (1, 1, 1, 160));
        assert_eq!(d[1].queue, QueueRecovery::AfterEvent);
        assert!((d[1].queue_off_ms - 12.0).abs() < 0.05, "{:?}", d[1]);
        assert_eq!(d[1].trim_samples, -80, "{:?}", d[1]);
        assert_eq!(
            d[1].text(),
            "1 mic glitch; output: 1 underrun (3.3 ms of silence played, a late answer); the filter's own estimate of its output \
             queue up to 12.0 ms over its level before the event, as the drift loop trims it back (80 samples dropped net over \
             the sweep, 1.7 ms)"
        );
        // Sweep 3's blocks start 1.6 s after the event (its margin before it): the queue 2.4 ms less, and no event of its own.
        assert!(!d[2].output.any_correction() && d[2].mic_glitches == 0);
        assert!((d[2].queue_off_ms - 9.57).abs() < 0.05, "{:?}", d[2]);
        assert_eq!(d[2].events_before, 2, "the glitch's block and the late answer's");
        assert!(d[3].queue_off_ms > 6.0 && d[3].queue_off_ms < 7.0, "{:?}", d[3]);
        // 576 frames at 0.72 a block: the estimate back within 1 ms from block 991, but the loop still slipping a sample a
        // block to block 1,059, so the recovery ends only 24 blocks before its slips do: sweep 5 (its window from block 1,020)
        // is still disturbed, by the slips and the recovery's end.
        assert_eq!(d[4].queue, QueueRecovery::AfterEvent, "{:?}", d[4]);
        assert!(d[4].queue_off_ms.abs() < 1.0 && d[4].trim_samples == -40, "{:?}", d[4]);
        assert!(
            d[4].text().ends_with("as the drift loop trims it back (40 samples dropped net over the sweep, 0.8 ms)"),
            "{}",
            d[4].text()
        );
        // Sweep 6 is clean, after the event: the report says when the clean ones span one.
        assert!(!d[5].any(), "after the recovery: {:?}", d[5]);
        assert_eq!(d[5].events_before, 2);
    }

    /// A recovery under way before the tap's first block (spread-micshared-6.csv: a late answer 1.8 s before the tap, the
    /// estimate 6 ms over its target there and trimmed at the loop's top rate): no event in the tap, but the sweeps are named
    /// for the queue from the test's start and the slips; and a start within 2.5 ms of its target, a run's start-up settling,
    /// is not. A recovery seen only by its slips (one that began nearer its target) is named for them.
    #[test]
    fn a_recovery_under_way_at_the_first_block() {
        let mut blocks = steady(1_000, 736);
        let mut over = 288.0f32;
        for b in &mut blocks {
            b.output.fill += over.max(0.0);
            if over > 0.0 {
                slip(b, -1);
            }
            over -= 0.72;
        }
        let d = disturbances(&blocks, &[sweep(1, 60), sweep(2, 260), sweep(3, 460)], SWEEP);
        assert_eq!(d[0].queue, QueueRecovery::FromStart);
        assert!((d[0].queue_off_ms - 5.2).abs() < 0.05, "{:?}", d[0]);
        assert_eq!(d[0].events_before, 0);
        assert_eq!(
            d[0].text(),
            "the filter's own estimate of its output queue up to 5.2 ms over its target from the test's start (something before \
             the test moved it), as the drift loop trims it back (120 samples dropped net over the sweep, 2.5 ms)"
        );
        // 288 frames at 0.72 a block: the estimate back within 1 ms from block 335, but the slips go on to block 400: the
        // recovery ends at block 377.
        assert!(d[1].queue == QueueRecovery::FromStart && d[1].trim_samples < 0, "{:?}", d[1]);
        assert!(!d[2].any(), "{:?}", d[2]);
        // An event in its first 0.5 s (a re-anchor at block 30): the recovery goes on at the target's level, not at the
        // queue's 5.8 ms over it before the event, so it doesn't end with the queue still that far over.
        blocks[30].events.reanchors = 1;
        let d = disturbances(&blocks, &[sweep(1, 60), sweep(2, 260), sweep(3, 460)], SWEEP);
        assert_eq!((d[0].queue, d[1].queue, d[1].events_before), (QueueRecovery::FromStart, QueueRecovery::FromStart, 1), "{d:?}");
        assert!(!d[2].any(), "{:?}", d[2]);

        // A start 1.2 ms under the target, the drift loop raising it at 12 a second: as the defaults' start-up, nothing.
        let mut settling = steady(1_000, 736);
        for (i, b) in settling.iter_mut().enumerate() {
            b.output.fill -= (58.0 - 0.12 * i as f32).max(0.0);
            if i < 480 && i % 8 == 0 {
                slip(b, 1);
            }
        }
        assert!(disturbances(&settling, &[sweep(1, 60), sweep(2, 260)], SWEEP).iter().all(|d| !d.any()));

        // 2 ms under the target at the start, and raised at 0.7 ms a second: the slips alone tell it.
        let mut raised = steady(1_000, 736);
        for (i, b) in raised.iter_mut().enumerate() {
            b.output.fill -= (96.0 - 0.36 * i as f32).max(0.0);
            if i < 266 && i % 3 == 0 {
                slip(b, 1);
            }
        }
        let d = disturbances(&raised, &[sweep(1, 60), sweep(2, 260)], SWEEP);
        assert_eq!((d[0].queue, d[0].trim_samples), (QueueRecovery::None, 40), "{:?}", d[0]);
        assert_eq!(d[0].text(), "the drift loop moving the output path under it (40 samples inserted net over the sweep, 0.8 ms)");
        assert!(!d[1].any(), "{:?}", d[1]);
    }

    /// The level is the queue's own before the event, not the target: a run whose steady fill sits 1.2 ms under its target
    /// (spread-defaults-4.csv before its event) has recovered when it's back there.
    #[test]
    fn the_queue_recovers_to_its_own_level() {
        let mut blocks = steady(600, 736);
        for b in &mut blocks {
            b.output.fill -= 58.0;
        }
        blocks[100].output.zeros = 240;
        blocks[100].output.underrun = true;
        blocks[100].events = OutputEvents { underruns: 1, deficits: 1, zero_frames: 240, ..OutputEvents::default() };
        // 5 ms over its level after the deficit, for 2 s, then back at it (still 1.2 ms under the target).
        for b in &mut blocks[101..300] {
            b.output.fill += 240.0;
        }
        let d = disturbances(&blocks, &[sweep(1, 150), sweep(2, 350)], 4_800);
        assert_eq!((d[0].output.deficits, d[0].output.zero_frames), (0, 0), "the deficit is 40 blocks before sweep 1's margin");
        assert!((d[0].queue_off_ms - 5.0).abs() < 0.1, "{:?}", d[0]);
        assert!(!d[1].any(), "back at its level, 1.2 ms under the target: {:?}", d[1]);
        assert_eq!(d[1].events_before, 1);
    }

    /// An event of each kind names itself, as the engine counted it (a drain of three blocks is one coarse drop); an event
    /// within 100 ms before a sweep's first sample counts, one further away doesn't; after its last sample, the window runs to
    /// its app latency and 25 ms more (at least 100 ms); a target's move that the fill follows adds no recovery, nor does the
    /// echo-path hold's.
    #[test]
    fn each_event_and_the_margin() {
        let mut blocks = steady(400, 736);
        blocks[10].thread.stalls = 1;
        blocks[11].output.reanchored = true;
        blocks[11].events.reanchors = 1;
        for (i, b) in blocks[12..15].iter_mut().enumerate() {
            b.output.skipped = true;
            b.events = OutputEvents { coarse_drops: u64::from(i == 0), dropped_frames: 480, ..OutputEvents::default() };
        }
        for b in &mut blocks[15..] {
            b.output.target = 640;
            b.output.fill -= 96.0;
        }
        for b in &mut blocks[16..] {
            b.hold = 240;
        }
        // The sweep from block 20 to 30 (4,800 samples): its margin reaches back to block 10 and on to block 39.
        let d = disturbances(&blocks, &[sweep(1, 20)], 4_800);
        assert_eq!((d[0].mic_stalls, d[0].output.reanchors, d[0].output.coarse_drops, d[0].target_moves), (1, 1, 1, 1));
        assert_eq!((d[0].hold_moves, d[0].output.dropped_frames), (1, 3 * 480));
        assert_eq!(d[0].queue, QueueRecovery::None, "the fill moved with its target");
        assert_eq!(
            d[0].text(),
            "1 mic stall; output: 1 coarse drop (30.0 ms dropped), 1 re-anchor; 1 output target move (adaptive); 1 move of the \
             echo-path guard's hold"
        );
        // From block 27 the margin starts at block 17: the events are 110 ms and more before it (a block each: the drain's
        // three blocks are each an event block, as each dropped its samples).
        let d = disturbances(&blocks, &[sweep(1, 27)], 4_800)[0];
        assert!(!d.any(), "{d:?}");
        assert_eq!(d.events_before, 7);
        // A glitch 90 ms after the sweep's last sample (block 39) counts; one 110 ms after it (block 41) doesn't.
        let mut blocks = steady(400, 736);
        blocks[39].thread.mic_glitches = 2;
        let d = disturbances(&blocks, &[sweep(1, 20)], 4_800);
        assert_eq!(d[0].mic_glitches, 2);
        assert_eq!(d[0].text(), "2 mic glitches");
        blocks[39].thread.mic_glitches = 0;
        blocks[41].thread.mic_glitches = 1;
        assert!(!disturbances(&blocks, &[sweep(1, 20)], 4_800)[0].any());
        // With an app latency of 120 ms its last samples are queued until then: the window runs on to 145 ms after it.
        let slow = SweepAt { app_ms: 120.0, ..sweep(1, 20) };
        assert_eq!(disturbances(&blocks, &[slow], 4_800)[0].mic_glitches, 1);
        blocks[41].thread.mic_glitches = 0;
        blocks[45].thread.mic_glitches = 1;
        assert!(!disturbances(&blocks, &[slow], 4_800)[0].any(), "150 ms after it");
    }

    /// The direct driver route (TODO 13; the owner, 2026-11-24): a move of the ring's headroom is an event for the sweeps
    /// around it, as is a new reader packet, each named as such rather than as the adaptive target's move; the queue then
    /// recovers to its level as after any event. Blocks discarded while no app records (no target) move nothing, and a
    /// recording app that comes back with the same packet and headroom isn't a move.
    #[test]
    fn the_direct_routes_headroom_and_packet_moves_disturb_their_sweeps() {
        let ring = |packet: u32, headroom: u32, n: usize| -> Vec<TapFacts> {
            let mut v = steady(n, packet + headroom);
            for b in &mut v {
                (b.output.packet, b.output.headroom) = (packet, headroom);
            }
            v
        };
        // 128-frame packets: the headroom down from 240 to 96 at block 200 (the fill 144 over the new target, trimmed back at
        // 0.72 a block), and a 160-frame reader from block 600.
        let mut blocks = ring(128, 240, 200);
        let mut lowered = ring(128, 96, 400);
        let mut over = 144.0f32;
        for b in &mut lowered {
            b.output.fill += over.max(0.0);
            if over > 0.0 {
                slip(b, -1);
            }
            over -= 0.72;
        }
        blocks.extend(lowered);
        blocks.extend(ring(160, 96, 300));
        let sweeps = [sweep(1, 40), sweep(2, 190), sweep(3, 450), sweep(4, 595), sweep(5, 750)];
        let d = disturbances(&blocks, &sweeps, 4_800);
        assert!(!d[0].any(), "{:?}", d[0]);
        assert_eq!((d[1].headroom_moves, d[1].target_moves, d[1].packet_moves), (1, 0, 0), "{:?}", d[1]);
        assert_eq!(d[1].queue, QueueRecovery::AfterEvent);
        assert!(
            d[1].text().starts_with("1 move of the ring's headroom (direct route: raised after an underrun, lowered after clean running)"),
            "{}",
            d[1].text()
        );
        assert!(!d[2].any(), "recovered: {:?}", d[2]);
        assert_eq!((d[3].packet_moves, d[3].headroom_moves, d[3].target_moves), (1, 0, 0), "{:?}", d[3]);
        assert_eq!(d[3].text(), "1 change of the reader's packet (direct route: another recording app's period)");
        assert!(!d[4].any(), "{:?}", d[4]);
        assert_eq!(d[4].events_before, 2);
        // Idle blocks between two recordings with the same packet and headroom: nothing moved.
        let mut idle = ring(128, 96, 100);
        idle.extend((0..100).map(|_| TapFacts::default()));
        idle.extend(ring(128, 96, 100));
        assert!(
            disturbances(&idle, &[sweep(1, 60), sweep(2, 150)], 4_800)
                .iter()
                .all(|d| d.headroom_moves + d.packet_moves + d.target_moves == 0)
        );
    }

    /// A failed write's block (its output facts at their defaults: no target) neither moves the target nor reads as a queue
    /// far from it, and a queue under its level after an event is named as under.
    #[test]
    fn a_failed_write_and_a_queue_under_its_level() {
        let mut blocks = steady(300, 736);
        blocks[50].output = OutputFacts::default();
        blocks[100].output.reanchored = true;
        blocks[100].events.reanchors = 1;
        for b in &mut blocks[101..140] {
            b.output.fill -= 200.0;
        }
        let d = disturbances(&blocks, &[sweep(1, 45), sweep(2, 120)], 480);
        assert!(!d[0].any(), "{:?}", d[0]);
        assert_eq!(d[1].output.reanchors, 0, "the re-anchor is outside sweep 2's margin");
        assert!((d[1].queue_off_ms + 4.2).abs() < 0.1, "{:?}", d[1]);
        assert_eq!(
            d[1].text(),
            "the filter's own estimate of its output queue up to 4.2 ms under its level before the event, as the drift loop trims \
             it back"
        );
    }

    /// The test's totals are the blocks' counts added up: the per-sweep lines' and the events line agree.
    #[test]
    fn the_totals_add_the_blocks_up() {
        let mut blocks = steady(300, 736);
        late_wake(&mut blocks, 100);
        blocks[200].thread.stalls = 1;
        let (e, glitches, stalls) = totals(&blocks);
        assert_eq!((e.underruns, e.late_answers, e.silence_frames, glitches, stalls), (1, 1, 160, 1, 1));
        assert_eq!(e.slip_net, -198);
        assert_eq!(e.slips, 198);
    }

    /// The rule on a saved run's blocks (`noise-filter latency --save-blocks FILE`, which records from the engine's start: the
    /// CSV has no mark of the tap's start nor the sweeps', so the tap is taken to start at block `NF_TAP_FROM`, 230 by default,
    /// and sweep `i` 0.52 s + 2 `i` s after it: the probe's 20 ms of silence and the train's first gap). Each block's counts are
    /// rebuilt from its facts (a coarse drop where a block drops after one that didn't). Ignored (it reads a file):
    /// `NF_BLOCKS_CSV=FILE NF_APP_MS=46.47,43.47,40.47,37.91,36.22 cargo test -p nf-engine --lib a_saved_runs_sweeps -- --ignored
    /// --nocapture`, the app latencies the run's report gave. On 2026-11-23's spread-defaults-1..8 it names what TODO 13's replay
    /// of the rule did.
    #[test]
    #[ignore]
    fn a_saved_runs_sweeps() {
        let Ok(path) = std::env::var("NF_BLOCKS_CSV") else { return };
        let from: usize = std::env::var("NF_TAP_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(230);
        let app: Vec<f64> = std::env::var("NF_APP_MS").unwrap().split(',').map(|v| v.trim().parse().unwrap()).collect();
        let csv = std::fs::read_to_string(&path).unwrap();
        let mut rows = csv.lines();
        let header: Vec<&str> = rows.next().unwrap().split(',').collect();
        let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
        let cols = [
            "out_fill", "out_target", "out_slip", "out_zeros", "out_dropped", "out_skipped", "out_underrun", "out_late", "out_reanchored",
            "out_silence", "mic_glitches", "stalls", "echo_hold",
        ]
        .map(col);
        // The direct route's packet and headroom: in CSVs from 2026-11-24 on (0 in older ones, as on the WASAPI route).
        let ring = ["out_packet", "out_headroom"].map(|name| header.iter().position(|h| *h == name));
        let mut dropping = false;
        let blocks: Vec<TapFacts> = rows
            .skip(from)
            .map(|line| {
                let f: Vec<&str> = line.split(',').collect();
                let [fill, target, slip, zeros, dropped, skipped, underrun, late, reanchored, silence, glitches, stalls, hold] =
                    cols.map(|i| f[i].parse::<f64>().unwrap_or(if f[i] == "true" { 1.0 } else { 0.0 }));
                let [packet, headroom] = ring.map(|i| i.and_then(|i| f.get(i)?.parse::<u32>().ok()).unwrap_or(0));
                let dropped_frames = (dropped + 480.0 * skipped) as u64;
                let drop_starts = dropped_frames > 0 && !dropping;
                dropping = dropped_frames > 0;
                let events = OutputEvents {
                    underruns: underrun as u64,
                    silence_frames: silence as u64,
                    late_answers: late as u64,
                    deficits: u64::from(zeros > 0.0),
                    zero_frames: zeros as u64,
                    coarse_drops: u64::from(drop_starts),
                    dropped_frames,
                    slips: u64::from(slip != 0.0),
                    slip_net: slip as i64,
                    reanchors: reanchored as u64,
                };
                TapFacts {
                    output: OutputFacts {
                        fill: fill as f32,
                        target: target as u32,
                        slip: slip as i8,
                        packet,
                        headroom,
                        ..OutputFacts::default()
                    },
                    thread: ThreadFacts { mic_glitches: glitches as u32, stalls: stalls as u32, ..ThreadFacts::default() },
                    events,
                    hold: hold as u32,
                }
            })
            .collect();
        let sweeps: Vec<SweepAt> = (0..app.len())
            .map(|i| SweepAt { repetition: i as u32 + 1, position: (0.52 + 2.0 * i as f64) * 48_000.0, app_ms: app[i] })
            .collect();
        for (d, a) in disturbances(&blocks, &sweeps, SWEEP).iter().zip(&app) {
            let why = if d.any() { d.text() } else { "undisturbed".to_owned() };
            eprintln!("sweep {} ({a:.2} ms, {} event blocks before it): {why}", d.repetition, d.events_before);
        }
    }
}
