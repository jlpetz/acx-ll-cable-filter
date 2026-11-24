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
//!   the live discard). The pipeline knows a new recording by its packet, or by the ring's read position moving without a
//!   copy where it started within a block at the same packet, read with the underruns from one pass (pipeline.rs's
//!   `DriverReader`, ring_output.rs's `ReaderLine`). The raise keeps the fill where the underrun left it, rather than the
//!   drift loop slipping it back out and the ring running short again at the next late block.
//! - **Lowered after clean running:** once [`HOLD_S`] of blocks fed to a recording app (an idle ring's don't count) have run
//!   without an underrun since the last move, it halves, rounded down to [`STEP`] (a step or less goes to 0), and again after
//!   each further hold, back to one packet. The drift loop slips the fill down to each new target, a sample a block at most:
//!   nothing clicks.
//! - **A lowering that fails** (an underrun within a hold of it) goes back at once to the headroom it came down from (that
//!   ran clean for a hold), or by the driver's silence if that's more, and doubles the hold for the next, up to
//!   [`HOLD_MAX_S`]. After [`MAX_FAILED`] failed lowerings the headroom stays where it is: only an underrun moves it (up),
//!   until a recording app with another packet reads the ring (the slack one packet leaves goes with the packet), which
//!   starts the count again. So the lowerings cost at most [`MAX_FAILED`] underruns a run at a reader packet, beside the
//!   raises' own; the price is the headroom kept for the rest of the run where three lowerings have failed (ours, proposed,
//!   2026-11-24, as out_target.rs's strikes; for the owner's call). Without the bound, a mic whose need never goes away
//!   (the VM's, two blocks every other wake: `pipeline/tests/driver.rs`) lost its need at every lowering and underran
//!   within the hold, for the whole run: at 128 frames at about 70, 195, 440, 920 and 1890 s, then every 16 minutes, three
//!   underruns each (the raises a step at a time), 21 in 3000 s; at 160 frames one each. With it the same scene underruns
//!   six times in all at 128 frames (five at 160), none after 440 s.
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
/// Failed lowerings at a reader packet after which the headroom stays for the run (ours, proposed, 2026-11-24, as
/// out_target.rs's `MAX_STRIKES`).
pub(crate) const MAX_FAILED: u32 = 3;
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
    /// The last lowering, while it's within a hold (an underrun then is that lowering failing): the blocks fed since, and the
    /// headroom it came down from. `None` after a raise, or before any lowering.
    since_lowering: Option<(u32, usize)>,
    /// Lowerings that failed at this reader packet; at [`MAX_FAILED`] there are no more.
    failed: u32,
    /// The reader's packet the failures were at (frames; 0 before the first reader).
    packet: usize,
}

impl Default for RingHeadroom {
    fn default() -> Self {
        Self::new()
    }
}

impl RingHeadroom {
    pub(crate) const fn new() -> Self {
        Self { headroom: 0, clean: 0, hold: HOLD_S * BLOCKS_PER_S, since_lowering: None, failed: 0, packet: 0 }
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

    /// Lowerings that failed at this reader packet.
    #[cfg(test)]
    pub(crate) fn failed(&self) -> u32 {
        self.failed
    }

    /// A recording app reads the ring with `packet`-frame packets (not 0). One with another packet than the last's starts the
    /// failed lowerings' count again: the slack one packet of fill leaves goes with the packet, so a headroom the last one
    /// kept is tried lower again at this one, the hold as it stands.
    #[inline]
    pub(crate) fn reader(&mut self, packet: usize) {
        if packet == 0 || packet == self.packet {
            return;
        }
        if self.packet != 0 {
            self.failed = 0;
        }
        self.packet = packet;
    }

    /// A real underrun at this block: the driver filled `missing` frames with silence since the block before. Returns the
    /// new headroom when it moved (not at the cap).
    #[inline]
    pub(crate) fn underrun(&mut self, missing: u64) -> Option<usize> {
        // A lowering failing: back to where it came down from at least, and the next one waits longer (or never comes).
        let mut floor = 0;
        if let Some((blocks, from)) = self.since_lowering
            && blocks < self.hold
        {
            self.hold = self.hold.saturating_mul(2).min(HOLD_MAX_S * BLOCKS_PER_S);
            self.failed = self.failed.saturating_add(1);
            floor = from;
        }
        self.since_lowering = None;
        self.clean = 0;
        let missing = usize::try_from(missing).unwrap_or(usize::MAX).min(MAX);
        let raise = missing.max(1).div_ceil(STEP) * STEP;
        let headroom = self.headroom.saturating_add(raise).max(floor).min(MAX);
        (headroom != self.headroom).then(|| {
            self.headroom = headroom;
            headroom
        })
    }

    /// A block fed to a recording app without an underrun. Returns the new headroom when it came down.
    #[inline]
    pub(crate) fn clean_block(&mut self) -> Option<usize> {
        if let Some((b, _)) = self.since_lowering.as_mut() {
            *b = b.saturating_add(1);
        }
        if self.headroom == 0 || self.failed >= MAX_FAILED {
            return None;
        }
        self.clean = self.clean.saturating_add(1);
        if self.clean < self.hold {
            return None;
        }
        self.clean = 0;
        self.since_lowering = Some((0, self.headroom));
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

    /// An underrun within a hold of a lowering puts the headroom back where the lowering came from (or up by the silence, if
    /// that's more) and doubles the hold; one later than that is an ordinary raise.
    #[test]
    fn a_lowering_that_fails_goes_back_and_waits_longer_the_next_time() {
        let mut h = RingHeadroom::new();
        h.reader(128);
        h.underrun(480);
        assert_eq!(h.frames(), 480);
        run_clean(&mut h, HOLD);
        assert_eq!(h.frames(), 240);
        run_clean(&mut h, 10);
        assert_eq!(h.underrun(20), Some(480), "back to where it came down from, not 240 + a step");
        assert_eq!((h.hold_s(), h.failed()), (2 * HOLD_S, 1));
        run_clean(&mut h, 2 * HOLD);
        assert_eq!(h.frames(), 240);
        assert_eq!(h.underrun(300), Some(240 + 336), "the silence when it's more than the way back");
        assert_eq!((h.hold_s(), h.failed()), (4 * HOLD_S, 2));
        // Each later lowering a hold after the one before (an underrun between two is the last one failing), down to nothing.
        assert_eq!(run_clean(&mut h, 4 * 4 * HOLD), vec![288, 144, 48, 0]);
        // At nothing, a hold after the lowering to it: an underrun then is an ordinary raise, by the silence.
        run_clean(&mut h, 4 * HOLD);
        assert_eq!(h.underrun(10), Some(48));
        assert_eq!((h.hold_s(), h.failed()), (4 * HOLD_S, 2));
        // Within the hold of the lowering to nothing, it is that lowering failing: back to a step.
        assert_eq!(run_clean(&mut h, 4 * HOLD), vec![0]);
        run_clean(&mut h, 4 * HOLD - 1);
        assert_eq!(h.underrun(1), Some(48));
        assert_eq!((h.hold_s(), h.failed()), (8 * HOLD_S, 3));
    }

    /// After [`MAX_FAILED`] failed lowerings the headroom stays for the run, raised by any later underrun but never lowered;
    /// the hold stops at its most. A recording app with the same packet doesn't change that; one with another packet starts
    /// the count again, and lowerings resume after the hold as it stands.
    #[test]
    fn three_failed_lowerings_keep_it_until_another_packet() {
        let mut h = RingHeadroom::new();
        h.reader(128);
        h.underrun(200);
        let mut hold = HOLD;
        for k in 1..=MAX_FAILED {
            assert_eq!(run_clean(&mut h, hold), vec![96], "lowering {k}");
            run_clean(&mut h, 100);
            assert_eq!(h.underrun(5), Some(240), "lowering {k} failed: back up");
            hold = (2 * hold).min(HOLD_MAX_S * BLOCKS_PER_S);
            assert_eq!((h.hold_s() * BLOCKS_PER_S, h.failed()), (hold, k));
        }
        assert!(run_clean(&mut h, 20 * HOLD).is_empty(), "no lowering any more");
        assert_eq!(h.frames(), 240);
        assert_eq!(h.underrun(48), Some(288), "an underrun still raises it");
        assert!(run_clean(&mut h, 20 * HOLD).is_empty());
        // The same packet again (a recording app stopped, another started): still kept.
        h.reader(128);
        assert!(run_clean(&mut h, 20 * HOLD).is_empty());
        // Another packet: lowerings resume, the hold as it stood.
        h.reader(480);
        assert_eq!(h.failed(), 0);
        assert!(run_clean(&mut h, hold - 1).is_empty());
        assert_eq!(h.clean_block(), Some(144));
        // At most the bound again at this packet.
        for _ in 0..MAX_FAILED {
            run_clean(&mut h, 10);
            h.underrun(1);
            run_clean(&mut h, HOLD_MAX_S * BLOCKS_PER_S);
        }
        assert_eq!(h.failed(), MAX_FAILED);
        let kept = h.frames();
        assert!(run_clean(&mut h, 20 * HOLD).is_empty() && h.frames() == kept && kept > 0, "{h:?}");
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
