//! The direct driver route's fill headroom (TODO 13; the owner, 2026-11-24: one reader packet stays the fill target where it
//! runs clean, bounded headroom when the driver reports real underruns, reduced again after a stretch of clean running, no
//! permanent extra latency for everyone).
//!
//! The drift loop holds the ring's fill before each write at the output target: one reader packet + this headroom. A capture
//! pass needs a whole packet in the ring, so the fill's lowest point (just before a write) leaves about half a packet of
//! lateness a block can come with before a pass runs short (2.67 ms packets: 1.3 to 4 ms, by the phase of the passes against
//! the blocks): a steady mic rides that out, a mic that delivers its blocks two to a wake (the first a block late) doesn't.
//!
//! - **Raised at once** after a real underrun by the frames the driver filled with silence since the block before (the ring
//!   header's `underrun_frames`: read_frame stops at write_frame, so the recording app got that much silence and every later
//!   sample comes that much later), rounded up to [`STEP`] (at least a step), to at most [`MAX`]. Real: while a recording app
//!   reads the ring, after the block that set its starting fill (the first passes of a new recording find the ring empty:
//!   the live discard). The raise keeps the fill where the underrun left it, rather than the drift loop slipping it back out
//!   and the ring running short again at the next late block.
//! - **Lowered after clean running:** once [`HOLD_S`] of blocks fed to a recording app (an idle ring's don't count) have run
//!   without an underrun since the last move, it halves, rounded down to [`STEP`] (a step or less goes to 0), and again after
//!   each further hold, back to one packet. The drift loop slips the fill down to each new target, a sample a block at most:
//!   nothing clicks.
//! - **A lowering that fails** (an underrun within a hold of it) doubles the hold for the next, up to [`HOLD_MAX_S`]: hiccups
//!   that come more often than the hold keep the headroom up rather than costing an underrun at every lowering. The hold
//!   never goes back down in a run: the next lowering is at most [`HOLD_MAX_S`] away, so the headroom always comes down.
//!
//! The numbers (ours, proposed, 2026-11-24): the cap is a block (WASAPI's fixed target keeps one spare: a mic's two blocks a
//! wake, a missed wake) and the WASAPI route's default output margin (2 ms), so the direct route never holds more than the
//! WASAPI route's fixed target at the reader's period; the holds are the adaptive WASAPI target's order of magnitude
//! (out_target.rs: a 120 s window, strikes held 10 minutes and more), shorter at first since the route starts low.
//!
//! Real-time: O(1) a block, no allocation, no panic paths.

use nf_core::{BLOCK, SAMPLE_RATE};

/// The quantum (1 ms): a raise rounds the driver's missing frames up to it, a lowering rounds down to it.
pub(crate) const STEP: usize = 48;
/// The most headroom, in frames (12 ms): a block and the WASAPI route's default output margin.
pub(crate) const MAX: usize = BLOCK + 96;
/// Clean running a lowering waits for at first, in seconds, and the longest it waits after lowerings that failed.
pub(crate) const HOLD_S: u32 = 60;
pub(crate) const HOLD_MAX_S: u32 = 960;
/// Blocks a second.
const BLOCKS_PER_S: u32 = SAMPLE_RATE / BLOCK as u32;

/// The headroom over one reader packet, and when it moves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RingHeadroom {
    /// The headroom now, in frames.
    headroom: usize,
    /// Clean blocks since the last underrun or move.
    clean: u32,
    /// The clean blocks a lowering waits for now.
    hold: u32,
    /// Blocks fed since the last lowering, while it's within a hold (an underrun then is that lowering failing); `None`
    /// after a raise, or before any lowering.
    since_lowering: Option<u32>,
}

impl Default for RingHeadroom {
    fn default() -> Self {
        Self::new()
    }
}

impl RingHeadroom {
    pub(crate) const fn new() -> Self {
        Self { headroom: 0, clean: 0, hold: HOLD_S * BLOCKS_PER_S, since_lowering: None }
    }

    /// The headroom now, in frames.
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.headroom
    }

    /// The clean running the next lowering waits for, in seconds.
    #[cfg(test)]
    pub(crate) fn hold_s(&self) -> u32 {
        self.hold / BLOCKS_PER_S
    }

    /// A real underrun at this block: the driver filled `missing` frames with silence since the block before. Returns the
    /// new headroom when it moved (not at the cap).
    #[inline]
    pub(crate) fn underrun(&mut self, missing: u64) -> Option<usize> {
        if self.since_lowering.is_some_and(|b| b < self.hold) {
            self.hold = self.hold.saturating_mul(2).min(HOLD_MAX_S * BLOCKS_PER_S);
        }
        self.since_lowering = None;
        self.clean = 0;
        let missing = usize::try_from(missing).unwrap_or(usize::MAX).min(MAX);
        let raise = missing.max(1).div_ceil(STEP) * STEP;
        let headroom = self.headroom.saturating_add(raise).min(MAX);
        (headroom != self.headroom).then(|| {
            self.headroom = headroom;
            headroom
        })
    }

    /// A block fed to a recording app without an underrun. Returns the new headroom when it came down.
    #[inline]
    pub(crate) fn clean_block(&mut self) -> Option<usize> {
        if let Some(b) = self.since_lowering.as_mut() {
            *b = b.saturating_add(1);
        }
        if self.headroom == 0 {
            return None;
        }
        self.clean = self.clean.saturating_add(1);
        if self.clean < self.hold {
            return None;
        }
        self.clean = 0;
        self.since_lowering = Some(0);
        self.headroom = self.headroom / 2 / STEP * STEP;
        Some(self.headroom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: u32 = HOLD_S * BLOCKS_PER_S;

    /// `n` clean blocks; the headroom after each lowering among them.
    fn run_clean(h: &mut RingHeadroom, n: u32) -> Vec<usize> {
        (0..n).filter_map(|_| h.clean_block()).collect()
    }

    /// Clean from the start: no headroom, and nothing ever moves.
    #[test]
    fn a_clean_run_keeps_one_packet() {
        let mut h = RingHeadroom::new();
        assert_eq!(h.frames(), 0);
        assert!(run_clean(&mut h, 10 * HOLD).is_empty());
        assert_eq!(h.frames(), 0);
    }

    /// A raise is the driver's silence rounded up to a step, at least a step, and stops at the cap; another underrun adds to
    /// it.
    #[test]
    fn an_underrun_raises_by_what_the_driver_filled() {
        let mut h = RingHeadroom::new();
        assert_eq!(h.underrun(1), Some(STEP), "a frame: a step");
        assert_eq!(h.underrun(0), Some(2 * STEP), "a count that rose with no frames named: a step");
        assert_eq!(h.underrun(416), Some(2 * STEP + 432), "416 frames: 9 steps");
        assert_eq!(h.underrun(4_800), Some(MAX), "a 100 ms stall: the cap");
        assert_eq!(h.underrun(u64::MAX), None, "at the cap: no move");
        assert_eq!(h.frames(), MAX);
    }

    /// After a hold of clean running it halves, to a step, and again after each hold, to nothing; an underrun meanwhile starts
    /// the hold again.
    #[test]
    fn clean_running_brings_it_back_down() {
        let mut h = RingHeadroom::new();
        h.underrun(500);
        assert_eq!(h.frames(), 528);
        assert!(run_clean(&mut h, HOLD - 1).is_empty());
        assert_eq!(h.clean_block(), Some(240), "528 / 2 = 264, down to a step: 240");
        // An underrun just after a hold has passed since the lowering isn't that lowering failing.
        assert!(run_clean(&mut h, HOLD - 1).is_empty());
        assert_eq!(h.clean_block(), Some(96));
        assert_eq!(run_clean(&mut h, 3 * HOLD), vec![48, 0], "then a step, then nothing, and it stays there");
        assert_eq!(h.hold_s(), HOLD_S, "no lowering failed");
        // An underrun halfway through a hold starts it again.
        h.underrun(100);
        assert_eq!(h.frames(), 144);
        run_clean(&mut h, HOLD / 2);
        h.underrun(1);
        assert_eq!(h.frames(), 192);
        assert!(run_clean(&mut h, HOLD - 1).is_empty(), "a whole hold from the second underrun");
        assert_eq!(h.clean_block(), Some(96));
    }

    /// An underrun within a hold of a lowering doubles the hold, up to its most; one later than that doesn't.
    #[test]
    fn a_lowering_that_fails_waits_longer_the_next_time() {
        let mut h = RingHeadroom::new();
        h.underrun(480);
        let mut hold = HOLD;
        for _ in 0..6 {
            run_clean(&mut h, hold);
            assert_eq!(h.frames(), 240);
            run_clean(&mut h, 10);
            assert_eq!(h.underrun(200), Some(480), "back up by the shortfall");
            hold = (2 * hold).min(HOLD_MAX_S * BLOCKS_PER_S);
            assert_eq!(h.hold_s() * BLOCKS_PER_S, hold);
        }
        assert_eq!(h.hold_s(), HOLD_MAX_S, "doubled up to its most");
        // There it stays, and the headroom still comes down, a step each hold.
        assert_eq!(run_clean(&mut h, 2 * hold), vec![240, 96]);
        h.underrun(10);
        assert_eq!((h.frames(), h.hold_s()), (144, HOLD_MAX_S));
    }

    /// The per-block calls allocate nothing (the audio thread's).
    #[test]
    fn it_never_allocates() {
        let mut h = RingHeadroom::new();
        let ((), allocs, frees) = crate::test_alloc::count(|| {
            for k in 0..200_000u32 {
                if k % 7_001 == 0 {
                    let _ = h.underrun(u64::from(k % 600));
                } else {
                    let _ = h.clean_block();
                }
            }
        });
        assert_eq!((allocs, frees), (0, 0));
    }
}
