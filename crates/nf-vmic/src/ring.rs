//! The ring's arithmetic. Positions are absolute 64-bit frame counts from 0, never wrapped indexes; frame `n` lives in slot
//! `n & (capacity - 1)`.
//!
//! [`ReaderModel`] is the driver's capture pass (StreamEngine.cpp, `CCaptureStreamEngine::AppRingCapturePass`) in Rust, rule for
//! rule, so the tests can run [`Writer`](crate::Writer) against it without the driver.

use std::ops::Range;

/// The slot frame `frame` lives in.
pub fn slot(frame: u64, capacity: u32) -> usize {
    (frame & (u64::from(capacity) - 1)) as usize
}

/// The slots `count` frames from `frame` occupy (`count` at most the capacity): up to the ring's end, then from its start (the
/// second run is empty when they don't wrap).
pub fn runs(frame: u64, count: usize, capacity: u32) -> (Range<usize>, Range<usize>) {
    debug_assert!(count <= capacity as usize);
    let start = slot(frame, capacity);
    let first = count.min(capacity as usize - start);
    (start..start + first, 0..count - first)
}

/// What one capture pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Frames copied from the ring into the packet.
    pub copied: usize,
    /// Frames of the packet zero-filled (an underrun) after them.
    pub zeroed: usize,
    /// `write_frame` was out of range, so the reader re-based on it first.
    pub resynced: bool,
}

/// The driver's reader: its own position (never read back from the ring) and this attachment's counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderModel {
    pub read_frame: u64,
    pub frames_copied: u64,
    pub underrun_frames: u64,
    pub resyncs: u32,
    capacity: u32,
}

impl ReaderModel {
    /// A reader at frame 0, as the driver starts at ATTACH.
    pub fn new(capacity: u32) -> Self {
        Self { read_frame: 0, frames_copied: 0, underrun_frames: 0, resyncs: 0, capacity }
    }

    /// One pass into `packet` (the capture packet: its length is the packet's frames), with `write_frame` as the driver read it
    /// and `slots` the ring's frames. The rules, as nf_vmic.h states them:
    /// - fill = write_frame - read_frame (unsigned); over the capacity (backwards, or more than a ring ahead): re-sync,
    ///   read_frame = write_frame, so the ring reads as empty;
    /// - copy min(fill, packet) frames from read_frame, zero the rest of the packet, and advance read_frame by the frames copied.
    pub fn pass(&mut self, write_frame: u64, slots: &[[f32; 2]], packet: &mut [[f32; 2]]) -> Pass {
        let mut fill = write_frame.wrapping_sub(self.read_frame);
        let resynced = fill > u64::from(self.capacity);
        if resynced {
            self.read_frame = write_frame;
            fill = 0;
            self.resyncs += 1;
        }
        let copied = fill.min(packet.len() as u64) as usize;
        let (first, second) = runs(self.read_frame, copied, self.capacity);
        let split = first.len();
        packet[..split].copy_from_slice(&slots[first]);
        packet[split..copied].copy_from_slice(&slots[second]);
        packet[copied..].fill([0.0; 2]);
        let zeroed = packet.len() - copied;
        self.read_frame += copied as u64;
        self.frames_copied += copied as u64;
        self.underrun_frames += zeroed as u64;
        Pass { copied, zeroed, resynced }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring of `capacity` slots holding frame numbers `[from, to)` (each slot's left channel is its frame number).
    fn ring_with(capacity: u32, from: u64, to: u64) -> Vec<[f32; 2]> {
        let mut slots = vec![[-1.0; 2]; capacity as usize];
        for n in from..to {
            slots[slot(n, capacity)] = [n as f32, n as f32];
        }
        slots
    }

    fn numbers(packet: &[[f32; 2]]) -> Vec<f32> {
        packet.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn slots_and_runs() {
        assert_eq!(slot(0, 256), 0);
        assert_eq!(slot(255, 256), 255);
        assert_eq!(slot(256, 256), 0);
        assert_eq!(slot(u64::MAX, 256), 255);
        assert_eq!(runs(10, 20, 256), (10..30, 0..0));
        assert_eq!(runs(250, 20, 256), (250..256, 0..14));
        assert_eq!(runs(256 * 7 + 255, 1, 256), (255..256, 0..0));
        assert_eq!(runs(5, 0, 256), (5..5, 0..0));
    }

    #[test]
    fn a_full_packet_advances_by_the_packet() {
        let slots = ring_with(256, 0, 100);
        let mut r = ReaderModel::new(256);
        let mut packet = [[0.0; 2]; 32];
        let p = r.pass(100, &slots, &mut packet);
        assert_eq!(p, Pass { copied: 32, zeroed: 0, resynced: false });
        assert_eq!(numbers(&packet), (0..32).map(|n| n as f32).collect::<Vec<_>>());
        assert_eq!(r.read_frame, 32);
    }

    #[test]
    fn the_copy_splits_at_the_wrap() {
        // Frames 1000..1100 in a 256-frame ring: 1000 is slot 232, so a 48-frame packet wraps after 24.
        let slots = ring_with(256, 1000, 1100);
        let mut r = ReaderModel { read_frame: 1000, ..ReaderModel::new(256) };
        let mut packet = [[0.0; 2]; 48];
        assert_eq!(r.pass(1100, &slots, &mut packet).copied, 48);
        assert_eq!(numbers(&packet), (1000..1048).map(|n| n as f32).collect::<Vec<_>>());
    }

    #[test]
    fn an_underrun_inserts_silence_and_drops_nothing() {
        let slots = ring_with(256, 0, 20);
        let mut r = ReaderModel::new(256);
        let mut packet = [[9.0; 2]; 32];
        assert_eq!(r.pass(20, &slots, &mut packet), Pass { copied: 20, zeroed: 12, resynced: false });
        assert!(packet[20..].iter().all(|f| *f == [0.0, 0.0]));
        // The reader stops at write_frame: the app's next frames follow on, nothing skipped.
        assert_eq!(r.read_frame, 20);
        assert_eq!(r.underrun_frames, 12);
        let slots = ring_with(256, 20, 60);
        assert_eq!(r.pass(60, &slots, &mut packet).copied, 32);
        assert_eq!(packet[0][0], 20.0);
    }

    #[test]
    fn an_empty_ring_is_all_silence() {
        let slots = ring_with(256, 0, 0);
        let mut r = ReaderModel::new(256);
        let mut packet = [[9.0; 2]; 16];
        assert_eq!(r.pass(0, &slots, &mut packet), Pass { copied: 0, zeroed: 16, resynced: false });
        assert_eq!(r.read_frame, 0);
    }

    #[test]
    fn a_full_ring_is_not_a_resync() {
        let slots = ring_with(256, 0, 256);
        let mut r = ReaderModel::new(256);
        let mut packet = [[0.0; 2]; 16];
        assert!(!r.pass(256, &slots, &mut packet).resynced);
        assert_eq!(r.read_frame, 16);
    }

    #[test]
    fn more_than_a_ring_ahead_resyncs_to_empty() {
        let slots = ring_with(256, 1000, 1257);
        let mut r = ReaderModel::new(256);
        let mut packet = [[9.0; 2]; 16];
        assert_eq!(r.pass(257, &slots, &mut packet), Pass { copied: 0, zeroed: 16, resynced: true });
        assert_eq!((r.read_frame, r.resyncs), (257, 1));
    }

    #[test]
    fn write_frame_going_back_resyncs() {
        let slots = ring_with(256, 0, 100);
        let mut r = ReaderModel { read_frame: 100, ..ReaderModel::new(256) };
        let mut packet = [[0.0; 2]; 16];
        // 90 - 100 wraps to a huge fill: the same single comparison catches it.
        assert!(r.pass(90, &slots, &mut packet).resynced);
        assert_eq!(r.read_frame, 90);
        // A write_frame from nowhere (garbage) as well.
        assert!(r.pass(u64::MAX - 5, &slots, &mut packet).resynced);
    }
}
