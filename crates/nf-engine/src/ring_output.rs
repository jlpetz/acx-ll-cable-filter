//! Live 48 kHz float32 stereo feed for the ACX capture endpoint. The capture reader owns the clock.

use std::io;
use std::sync::atomic::{Ordering, fence};

use nf_core::SAMPLE_RATE;
use nf_vmic::Writer;
use nf_vmic::broker::{self, LIVE_CAPACITY, LiveFeed};
use nf_vmic::protocol::{RingHeader, STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_LIVE_HANDOFF, STATE_READING};

use crate::errors::{StartFailure, hr};
use crate::pipeline::{ClockReading, DriverReader, Hr, RenderSink};
use crate::stream::OpenInfo;
use crate::timing::Qpc;

pub(crate) struct RingOutput {
    attachment: LiveFeed,
    qpc: Qpc,
    reader: ReaderLine,
    pub(crate) info: OpenInfo,
}

/// The header's reader line as the pipeline reads it once a block (TODO 13): the recording app's packet, whether the driver
/// moved the ring's read position other than by reading, and the frames it filled with silence, all as one capture pass
/// left them. A capture pass stores its state, packet, counters and read_frame inside the clock's sequence
/// (StreamEngine.cpp's AppRingCapturePass, AppRingStoreCounters), so read inside it they're that pass's together: a new
/// reader's packet, or its live discard, and the silence of its first passes come in the same read, never the silence
/// first and the reader a block later, which would count it as a real underrun (the owner, 2026-11-24: only real ones).
#[derive(Debug, Default)]
pub(crate) struct ReaderLine {
    /// The last packet read while READING, kept through a LIVE_HANDOFF.
    reader_packet: usize,
    /// What the last read gave.
    packet: usize,
    /// `read_frame - frames_copied` at the last read (`None` before it). The driver moves read_frame by what it copies, and
    /// otherwise only at a live discard (a recording app's first pass, LiveStreams 0 to 1) and a resync; a handoff's skip is
    /// in the reader's copy, not the ring.
    moved: Option<u64>,
    /// `underrun_frames` (this attachment's: reset at ATTACH) at the last read, and at the last [`Self::take_underruns`].
    underrun_frames: u64,
    taken: u64,
    /// The frames the last [`Self::take_underruns`] found, for [`Self::take_silence`].
    missing: u64,
}

impl ReaderLine {
    /// The reader at this block, from one read of the header. Where the read failed, the last block's packet with no news:
    /// its underruns and any restart come with the next read.
    pub(crate) fn read(&mut self, h: &RingHeader) -> DriverReader {
        let Some(line) = h.reader_sample() else { return DriverReader { packet: self.packet, restarted: false } };
        let restarted = self.moved.is_some_and(|m| m != line.moved);
        self.moved = Some(line.moved);
        self.underrun_frames = line.underrun_frames;
        self.packet = if line.state & STATE_READING != 0 {
            if line.packet != 0 {
                self.reader_packet = line.packet as usize;
            }
            self.reader_packet
        } else if line.state & STATE_LIVE_HANDOFF != 0 {
            self.reader_packet
        } else {
            0
        };
        DriverReader { packet: self.packet, restarted }
    }

    /// The frames the driver filled with silence up to the last [`Self::read`], less those already taken, as one underrun;
    /// its frames for [`Self::take_silence`].
    pub(crate) fn take_underruns(&mut self) -> u64 {
        self.missing = self.underrun_frames.saturating_sub(self.taken);
        self.taken = self.underrun_frames;
        u64::from(self.missing > 0)
    }

    pub(crate) fn take_silence(&mut self) -> u64 {
        std::mem::take(&mut self.missing)
    }
}

impl RingOutput {
    pub(crate) fn open(cable: &nf_vmic::identity::CableInstance) -> Result<Self, StartFailure> {
        let attachment = broker::attach_live_to(cable.id, LIVE_CAPACITY).map_err(open_error)?;
        Ok(Self {
            attachment,
            qpc: Qpc::new(),
            reader: ReaderLine::default(),
            info: OpenInfo {
                name: format!("LLCable-{} (direct driver ring)", cable.label),
                sample_rate: SAMPLE_RATE,
                channels: 2,
                period_frames: 0,
                period_ms: 0.0,
                low_latency: false,
                autoconvert: false,
                raw: false,
                exclusive: None,
                refused: None,
            },
        })
    }

    pub(crate) fn check_attached(&self) -> Result<(), Hr> {
        let header = self.attachment.ring().header();
        let state = header.state.load(Ordering::Acquire);
        if state & STATE_ATTACHED == 0 || self.attachment.finished() {
            return Err(hr::AUDCLNT_E_DEVICE_INVALIDATED);
        }
        if state & STATE_FORMAT_MISMATCH != 0 {
            return Err(hr::AUDCLNT_E_UNSUPPORTED_FORMAT);
        }
        if state & STATE_READING != 0 && header.packet_frames.load(Ordering::Relaxed) > LIVE_CAPACITY {
            return Err(hr::AUDCLNT_E_INVALID_DEVICE_PERIOD);
        }
        Ok(())
    }

    /// The recording app reading the ring at this block, and whether the driver set its fill afresh ([`ReaderLine::read`]).
    pub(crate) fn reader(&mut self) -> DriverReader {
        self.reader.read(self.attachment.ring().header())
    }
}

fn open_error(error: io::Error) -> StartFailure {
    let code = error.raw_os_error();
    let detail = match code {
        Some(2 | 3 | 22 | 1617) => "The virtual driver is unavailable. Enable the Noise Filter device or select WASAPI output.".into(),
        Some(5) => "Approve direct output for this session or select WASAPI output.".into(),
        Some(170) => "Another app is feeding the virtual microphone. Stop that feed and try again.".into(),
        Some(50) => "A recorder is using an incompatible capture format. Close it or use WASAPI output.".into(),
        Some(87 | 1306) => {
            "The installed driver does not support this live ring protocol. Update the driver or select WASAPI output.".into()
        }
        _ => error.to_string(),
    };
    StartFailure::DriverRing {
        message: format!("Could not open the direct driver ring. {detail}"),
        retry: matches!(code, Some(2 | 3 | 22 | 170 | 1617)),
    }
}

impl RenderSink for RingOutput {
    fn buffer_frames(&self) -> usize {
        LIVE_CAPACITY as usize
    }

    fn padding(&mut self) -> Result<usize, Hr> {
        self.check_attached()?;
        Ok(Writer::new(self.attachment.ring()).fill() as usize)
    }

    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        let mut writer = Writer::new(self.attachment.ring());
        let start = writer.write_frame();
        let n = writer.produce(mono.len() as u64, |pos| [mono[(pos - start) as usize]; 2]);
        if n == mono.len() as u64 { Ok(()) } else { Err(hr::AUDCLNT_E_BUFFER_SIZE_ERROR) }
    }

    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        let n = Writer::new(self.attachment.ring()).produce(frames as u64, |_| [0.0; 2]);
        if n == frames as u64 { Ok(()) } else { Err(hr::AUDCLNT_E_BUFFER_SIZE_ERROR) }
    }

    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        let h = self.attachment.ring().header();
        // Read once. If the device updated during this sample the pipeline uses padding for the block.
        let seq = h.clock_sequence.load(Ordering::Acquire);
        if seq == 0 || seq & 1 != 0 {
            return Ok(None);
        }
        let pos = h.read_frame.load(Ordering::Relaxed);
        let qpc = h.last_pass_qpc.load(Ordering::Relaxed);
        let packet = h.packet_frames.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if packet == 0 || h.clock_sequence.load(Ordering::Relaxed) != seq {
            return Ok(None);
        }
        // The last completed pass consumed the prefix ending just before `pos`. The next packet starts at `pos`, and its stamp
        // (the recording app's sample time of frame `pos`) is that pass's deadline (StreamEngine.cpp's PacketStartQpc); `qpc`
        // is the pass's own QPC, taken a little after it (the timer's lateness and the ring lock: about 0.33 ms on the dev box
        // under Verifier, up to 1.5: pipeline.rs's `play_time_hns`). The pipeline times frame `pos + j` at `qpc + j` frames
        // (its sample time) and at the pass that completes its packet (its delivery).
        // TODO 13's open point: the deadline itself in the header would take that lateness out (a ring header change: a
        // protocol version bump by the owner's rule of 2026-11-23).
        Ok(Some(ClockReading { pos_frames: pos as f64, qpc_hns: self.qpc.at_hns(qpc as i64) as f64, now_hns: self.qpc.now_hns() as f64 }))
    }

    /// The frames the driver filled with silence since the last call (the header's `underrun_frames`: a pass found less than
    /// a packet in the ring, and read_frame stopped at write_frame), as one underrun, its frames for `take_silence`: as the
    /// block's [`Self::reader`] read them, with the reader, so a new recording's first passes (the ring empty after the live
    /// discard) count with its start, before its starting fill.
    fn take_underruns(&mut self) -> u64 {
        self.reader.take_underruns()
    }

    fn take_silence(&mut self) -> u64 {
        self.reader.take_silence()
    }

    fn take_late(&mut self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    use nf_vmic::RingMemory;

    use super::*;

    /// A capture pass as the driver stores its line (AppRingCapturePass): the sequence odd, the counters and state, read_frame,
    /// the sequence even. `discard` moves read_frame first, as a live discard or a resync does.
    fn pass(h: &RingHeader, packet: u32, copy: u64, under: u64, discard: u64) {
        h.clock_sequence.fetch_add(1, Relaxed);
        h.frames_copied.fetch_add(copy, Relaxed);
        h.underrun_frames.fetch_add(under, Relaxed);
        h.state.store(STATE_ATTACHED | STATE_READING, Relaxed);
        h.packet_frames.store(packet, Relaxed);
        h.read_frame.fetch_add(discard + copy, Relaxed);
        h.clock_sequence.fetch_add(1, Relaxed);
    }

    /// The reader line from one read of the header: the packet, a restart where read_frame moved without a copy, and the
    /// silence since the last read, taken once; a read that overlaps a pass gives the last block's packet and no news, which
    /// the next read brings; a stop (outside the sequence) is no reader, a live handoff the last reader's packet.
    #[test]
    fn the_reader_line_is_read_as_one_pass_left_it() {
        let ring = RingMemory::new(1024).unwrap();
        let h = ring.header();
        let mut line = ReaderLine::default();
        let read = |line: &mut ReaderLine| {
            let r = line.read(h);
            let n = line.take_underruns();
            (r.packet, r.restarted, n, line.take_silence())
        };
        assert_eq!(read(&mut line), (0, false, 0, 0), "attached, no pass yet");
        // A reader's first pass: the ring empty, a packet of silence, with the packet that read it.
        pass(h, 128, 0, 128, 0);
        assert_eq!(read(&mut line), (128, false, 1, 128));
        pass(h, 128, 128, 0, 0);
        pass(h, 128, 100, 28, 0);
        assert_eq!(read(&mut line), (128, false, 1, 28), "copies, and a pass short by 28");
        assert_eq!(read(&mut line), (128, false, 0, 0), "nothing new");
        // A pass in progress: the last block's packet, and its news at the next read.
        h.clock_sequence.fetch_add(1, Relaxed);
        h.underrun_frames.fetch_add(480, Relaxed);
        h.packet_frames.store(480, Relaxed);
        assert_eq!(read(&mut line), (128, false, 0, 0), "a read overlapping a pass");
        h.clock_sequence.fetch_add(1, Relaxed);
        assert_eq!(read(&mut line), (480, false, 1, 480), "the handoff's packet with its silence");
        // A new recording's live discard (read_frame to write_frame, past 600 queued frames) and its first pass's silence.
        pass(h, 480, 0, 480, 600);
        assert_eq!(read(&mut line), (480, true, 1, 480), "a restart at the same packet");
        pass(h, 480, 480, 0, 0);
        assert_eq!(read(&mut line), (480, false, 0, 0));
        // The reader stops with another stream to take over (LIVE_HANDOFF: its packet kept), then with none.
        h.state.store(STATE_ATTACHED | STATE_LIVE_HANDOFF, Relaxed);
        h.packet_frames.store(0, Relaxed);
        assert_eq!(read(&mut line).0, 480);
        h.state.store(STATE_ATTACHED, Relaxed);
        assert_eq!(read(&mut line).0, 0);
    }
}
