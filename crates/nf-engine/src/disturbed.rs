//! Which of the latency test's sweeps the engine's own events disturbed (TODO 13). The tap carries each block's output and
//! thread facts (pipeline.rs's `TapItem`), so the test knows, block for block in the tap's own samples, where a mic glitch or
//! stall, an output underrun, deficit, coarse drop, re-anchor or target move came, and where the output queue stood against
//! its target. A sweep is disturbed when such an event came within [`MARGIN_MS`] of its samples at the app's input, or when
//! the output queue was still away from its level before one while the drift loop trimmed it back.
//!
//! Why the recovery counts: with today's defaults (exclusive mic and output, the adaptive target) four runs in eight of
//! 2026-11-23 had a wake held up 13-16 ms, a mic glitch and a late answer at it, and the output queue then 9-12 ms over its
//! target; the drift loop trimmed it at about 1.5 ms a second, slower as it neared its level (back within 1 ms of it after
//! 6.3-9.2 s, by this module's rule), and the sweeps meanwhile read 3.00 ms less each 2 s. The runs without one agreed within
//! 0.6 ms.
//!
//! Not seen: an event before the test's first tapped block, whose recovery runs into the sweeps (the CLI waits 2 s first).

use nf_core::telemetry::OutputEvents;
use nf_core::{BLOCK, SAMPLE_RATE};

use crate::api::{OutputFacts, SweepDisturbance, ThreadFacts};

/// An event this close to a sweep's samples at the app's input (before its first or after its last) disturbs it: an output
/// event moves the samples already queued (up to the readout's 25-65 ms) and those after, and a mic glitch's zeros land in
/// the blocks after it.
pub(crate) const MARGIN_MS: f64 = 100.0;
/// The queue's level before an event: the median over this many blocks before it (0.5 s), of those neither events nor
/// recovering from an earlier one; 0 (the target) without any.
const LEVEL_BLOCKS: usize = 50;
/// The queue is read as a mean over this many blocks from each one (100 ms): the fill steps a device period at a time, and a
/// recovery's queue would dip within reach of its level for a block at the bottom of a step.
const SMOOTH_BLOCKS: usize = 10;
/// The queue is back when its mean is this close to its level (ms): the runs' steady fill stays within 2.5 ms of its target
/// block to block, and its 100 ms mean within about 1.
const RECOVERED_MS: f32 = 1.0;

/// One tapped block's facts, as the block left them: the output's and the thread's (`Pipeline::last_output`, `last_thread`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TapFacts {
    pub(crate) output: OutputFacts,
    pub(crate) thread: ThreadFacts,
}

/// One sweep at the app's input: its repetition (from 1) and where its first sample is in the tap's samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SweepAt {
    pub(crate) repetition: u32,
    pub(crate) position: f64,
}

/// Each sweep's disturbance, in `sweeps`' order. Block `i` of `blocks` holds the tap's samples from `i * BLOCK`; `sweep_len` is
/// a sweep's length in samples.
pub(crate) fn disturbances(blocks: &[TapFacts], sweeps: &[SweepAt], sweep_len: usize) -> Vec<SweepDisturbance> {
    let events = block_events(blocks);
    let off = recovery(blocks, &events);
    let margin = MARGIN_MS * 1e-3 * f64::from(SAMPLE_RATE);
    sweeps
        .iter()
        .map(|s| {
            let from = ((s.position - margin) / BLOCK as f64).floor().max(0.0) as usize;
            let to = (((s.position + sweep_len as f64 + margin) / BLOCK as f64).ceil().max(0.0) as usize).min(blocks.len());
            let mut d = SweepDisturbance { repetition: s.repetition, ..SweepDisturbance::default() };
            for i in from..to.max(from) {
                add(&mut d, &events[i]);
                if off[i].abs() > d.queue_off_ms.abs() {
                    d.queue_off_ms = off[i];
                }
            }
            d
        })
        .collect()
}

/// Each block's own events (a [`SweepDisturbance`] of one block, its repetition and queue unset): its mic glitches and stalls,
/// its output corrections, and a move of the target from the block before's.
fn block_events(blocks: &[TapFacts]) -> Vec<SweepDisturbance> {
    let mut prev_target = None;
    blocks
        .iter()
        .map(|b| {
            let (o, t) = (&b.output, &b.thread);
            let dropped = u64::from(o.dropped) + if o.skipped { BLOCK as u64 } else { 0 };
            let output = OutputEvents {
                underruns: u64::from(o.underrun),
                silence_frames: u64::from(o.silence),
                late_answers: u64::from(o.late),
                deficits: u64::from(o.zeros > 0),
                zero_frames: u64::from(o.zeros),
                coarse_drops: u64::from(dropped > 0),
                dropped_frames: dropped,
                reanchors: u64::from(o.reanchored),
                ..OutputEvents::default()
            };
            // A failed write leaves the block's output facts at their defaults (no target): it moves nothing.
            let moved = o.target > 0 && prev_target.is_some_and(|p| p != o.target);
            if o.target > 0 {
                prev_target = Some(o.target);
            }
            SweepDisturbance {
                mic_glitches: t.mic_glitches,
                mic_stalls: t.stalls,
                output,
                target_moves: u32::from(moved),
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
    let (o, b) = (&mut d.output, &e.output);
    o.underruns += b.underruns;
    o.silence_frames += b.silence_frames;
    o.late_answers += b.late_answers;
    o.deficits += b.deficits;
    o.zero_frames += b.zero_frames;
    o.coarse_drops += b.coarse_drops;
    o.dropped_frames += b.dropped_frames;
    o.reanchors += b.reanchors;
}

/// Per block, the output queue's distance from its level before the last event (ms, its 100 ms mean from the block on) while
/// it recovers; 0 elsewhere. A recovery starts after an event block, at the level of the blocks before it, carries on through
/// later events (at the same level), and ends at the first block whose mean is back within [`RECOVERED_MS`] of it. The event
/// blocks themselves are named for their events.
fn recovery(blocks: &[TapFacts], events: &[SweepDisturbance]) -> Vec<f32> {
    let ms_per_sample = 1000.0 / SAMPLE_RATE as f32;
    // The fill the drift loop read less the target it held; NaN for a failed write's block, which has neither.
    let excess: Vec<f32> = blocks
        .iter()
        .map(|b| if b.output.target > 0 { (b.output.fill - b.output.target as f32) * ms_per_sample } else { f32::NAN })
        .collect();
    let evented: Vec<bool> = events.iter().map(SweepDisturbance::any).collect();
    let mut off = vec![0.0f32; blocks.len()];
    let mut level: Option<f32> = None;
    for i in 0..blocks.len() {
        if evented[i] {
            if level.is_none() {
                level = Some(level_before(&excess, &evented, &off, i));
            }
            continue;
        }
        let Some(l) = level else { continue };
        let window = &excess[i..(i + SMOOTH_BLOCKS).min(excess.len())];
        let (sum, n) = window.iter().filter(|v| v.is_finite()).fold((0.0f32, 0u32), |(s, n), v| (s + v, n + 1));
        if n == 0 {
            continue;
        }
        let d = sum / n as f32 - l;
        if d.abs() > RECOVERED_MS {
            off[i] = d;
        } else {
            level = None;
        }
    }
    off
}

/// The queue's level before block `i`: the median excess over the last [`LEVEL_BLOCKS`] blocks before it that were neither
/// events nor recovering; 0 (the target) without any.
fn level_before(excess: &[f32], evented: &[bool], off: &[f32], i: usize) -> f32 {
    let mut v: Vec<f32> =
        (0..i).rev().filter(|&j| !evented[j] && off[j] == 0.0 && excess[j].is_finite()).map(|j| excess[j]).take(LEVEL_BLOCKS).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 }
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
                TapFacts { output: OutputFacts { fill, target, ..OutputFacts::default() }, thread: ThreadFacts::default() }
            })
            .collect()
    }

    fn sweep(repetition: u32, block: usize) -> SweepAt {
        SweepAt { repetition, position: (block * BLOCK) as f64 }
    }

    /// 1 s at 48 kHz: the test's sweep.
    const SWEEP: usize = 48_000;

    #[test]
    fn steady_blocks_disturb_nothing() {
        let blocks = steady(1_000, 736);
        let d = disturbances(&blocks, &[sweep(1, 50), sweep(2, 250), sweep(3, 450)], SWEEP);
        assert_eq!(d.len(), 3);
        for (i, s) in d.iter().enumerate() {
            assert_eq!(s.repetition, i as u32 + 1);
            assert!(!s.any(), "{s:?}");
            assert_eq!(s.text(), "");
        }
        // A sweep past the tap's last block has no blocks to be disturbed by.
        assert!(!disturbances(&[], &[sweep(1, 0)], SWEEP)[0].any());
        assert!(!disturbances(&blocks, &[sweep(1, 2_000)], SWEEP)[0].any());
    }

    /// The 2026-11-23 runs' pattern (spread-defaults-4.csv, block 258): a mic glitch at one block, a late answer with a
    /// period of silence at the next, the fill then 12 ms over the target, trimmed back at 1.5 ms a second. The sweep it came
    /// in names both events and the queue, the next two the queue alone, by less each time; the one before it is clean, as is
    /// the one after the queue is back.
    #[test]
    fn a_late_wake_disturbs_its_sweep_and_the_recovery_the_ones_after() {
        let mut blocks = steady(1_300, 736);
        blocks[258].thread.mic_glitches = 1;
        blocks[259].output = OutputFacts { underrun: true, late: true, silence: 160, ..blocks[259].output };
        // From block 259 the fill is 12 ms (576 frames) over, less 0.72 frames a block (1.5 ms a second) until it's back.
        for (i, b) in blocks.iter_mut().enumerate().skip(259) {
            b.output.fill += (576.0 - 0.72 * (i - 259) as f32).max(0.0);
        }
        // Sweeps every 2 s, from 0.3 s: one before the event, one just after it, two more, and one 7.7 s after it.
        let sweeps = [sweep(1, 30), sweep(2, 230), sweep(3, 430), sweep(4, 630), sweep(5, 1030)];
        let d = disturbances(&blocks, &sweeps, SWEEP);
        assert!(!d[0].any(), "before the event: {:?}", d[0]);
        assert_eq!((d[1].mic_glitches, d[1].output.underruns, d[1].output.late_answers, d[1].output.silence_frames), (1, 1, 1, 160));
        assert!((d[1].queue_off_ms - 12.0).abs() < 0.05, "{:?}", d[1]);
        assert_eq!(
            d[1].text(),
            "1 mic glitch; output: 1 underrun (3.3 ms of silence played, a late answer); the output queue up to 12.0 ms over its \
             level before the event, as the drift loop trims it back"
        );
        // Sweep 3's blocks start 1.6 s after the event (its margin before it): the queue 2.4 ms less, and no event of its own.
        assert!(!d[2].output.any_correction() && d[2].mic_glitches == 0);
        assert!((d[2].queue_off_ms - 9.57).abs() < 0.05, "{:?}", d[2]);
        assert_eq!(d[2].text(), "the output queue up to 9.6 ms over its level before the event, as the drift loop trims it back");
        assert!(d[3].queue_off_ms > 6.0 && d[3].queue_off_ms < 7.0, "{:?}", d[3]);
        // 576 frames at 0.72 a block: back within 1 ms after about 730 blocks (block 990); sweep 5's blocks start at 1,020.
        assert!(!d[4].any(), "after the recovery: {:?}", d[4]);
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
        // 5 ms over its level after the deficit, for 2 s, then back at it (still 1.2 ms under the target).
        for b in &mut blocks[101..300] {
            b.output.fill += 240.0;
        }
        let d = disturbances(&blocks, &[sweep(1, 150), sweep(2, 350)], 4_800);
        assert_eq!((d[0].output.deficits, d[0].output.zero_frames), (0, 0), "the deficit is 40 blocks before sweep 1's margin");
        assert!((d[0].queue_off_ms - 5.0).abs() < 0.1, "{:?}", d[0]);
        assert!(!d[1].any(), "back at its level, 1.2 ms under the target: {:?}", d[1]);
    }

    /// An event of each kind names itself; an event within 100 ms before a sweep's first sample or after its last counts, one
    /// further away doesn't; a target's move that the fill follows adds no recovery.
    #[test]
    fn each_event_and_the_margin() {
        let mut blocks = steady(400, 736);
        blocks[10].thread.stalls = 1;
        blocks[11].output.reanchored = true;
        blocks[12].output.dropped = 100;
        blocks[13].output.skipped = true;
        for b in &mut blocks[14..] {
            b.output.target = 640;
            b.output.fill -= 96.0;
        }
        // The sweep from block 20 to 30 (4,800 samples): its margin reaches back to block 10 and on to block 39.
        let d = disturbances(&blocks, &[sweep(1, 20)], 4_800);
        assert_eq!((d[0].mic_stalls, d[0].output.reanchors, d[0].output.coarse_drops, d[0].target_moves), (1, 1, 2, 1));
        assert_eq!(d[0].output.dropped_frames, 100 + 480);
        assert_eq!(d[0].queue_off_ms, 0.0, "the fill moved with its target");
        assert_eq!(d[0].text(), "1 mic stall; output: 2 coarse drops (12.1 ms dropped), 1 re-anchor; 1 output target move (adaptive)");
        // From block 25 the margin starts at block 15: the events are 110 ms and more before it.
        assert!(!disturbances(&blocks, &[sweep(1, 25)], 4_800)[0].any());
        // A glitch 90 ms after the sweep's last sample (block 39) counts; one 110 ms after it (block 41) doesn't.
        let mut blocks = steady(400, 736);
        blocks[39].thread.mic_glitches = 2;
        let d = disturbances(&blocks, &[sweep(1, 20)], 4_800);
        assert_eq!(d[0].mic_glitches, 2);
        assert_eq!(d[0].text(), "2 mic glitches");
        blocks[39].thread.mic_glitches = 0;
        blocks[41].thread.mic_glitches = 1;
        assert!(!disturbances(&blocks, &[sweep(1, 20)], 4_800)[0].any());
    }

    /// A failed write's block (its output facts at their defaults: no target) neither moves the target nor reads as a queue
    /// far from it, and a queue under its level after an event is named as under.
    #[test]
    fn a_failed_write_and_a_queue_under_its_level() {
        let mut blocks = steady(300, 736);
        blocks[50].output = OutputFacts::default();
        blocks[100].output.reanchored = true;
        for b in &mut blocks[101..140] {
            b.output.fill -= 200.0;
        }
        let d = disturbances(&blocks, &[sweep(1, 45), sweep(2, 120)], 480);
        assert!(!d[0].any(), "{:?}", d[0]);
        assert_eq!(d[1].output.reanchors, 0, "the re-anchor is outside sweep 2's margin");
        assert!((d[1].queue_off_ms + 4.2).abs() < 0.1, "{:?}", d[1]);
        assert_eq!(d[1].text(), "the output queue up to 4.2 ms under its level before the event, as the drift loop trims it back");
    }
}
