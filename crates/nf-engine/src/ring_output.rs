//! Live 48 kHz float32 stereo feed for the ACX capture endpoint. The capture reader owns the clock.

use std::cell::Cell;
use std::io;
use std::sync::atomic::{Ordering, fence};

use nf_core::SAMPLE_RATE;
use nf_vmic::Writer;
use nf_vmic::broker::{self, LIVE_CAPACITY, LiveFeed};
use nf_vmic::protocol::{STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_LIVE_HANDOFF, STATE_READING};

use crate::errors::{StartFailure, hr};
use crate::pipeline::{ClockReading, Hr, RenderSink};
use crate::stream::OpenInfo;
use crate::timing::Qpc;

pub(crate) struct RingOutput {
    attachment: LiveFeed,
    qpc: Qpc,
    last_reader_packet: Cell<usize>,
    last_missing: u64,
    missing: u64,
    pub(crate) info: OpenInfo,
}

impl RingOutput {
    pub(crate) fn open() -> Result<Self, StartFailure> {
        let attachment = broker::attach_live(LIVE_CAPACITY).map_err(open_error)?;
        Ok(Self {
            attachment,
            qpc: Qpc::new(),
            last_reader_packet: Cell::new(0),
            last_missing: 0,
            missing: 0,
            info: OpenInfo {
                name: "Noise Filter virtual microphone (direct driver ring)".into(),
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

    pub(crate) fn packet(&self) -> usize {
        let header = self.attachment.ring().header();
        let state = header.state.load(Ordering::Acquire);
        if state & STATE_READING != 0 {
            let packet = header.packet_frames.load(Ordering::Relaxed) as usize;
            if packet != 0 {
                self.last_reader_packet.set(packet);
            }
            self.last_reader_packet.get()
        } else if state & STATE_LIVE_HANDOFF != 0 {
            self.last_reader_packet.get()
        } else {
            0
        }
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
        // is the pass's own QPC, taken a little after it (the timer's lateness and the ring lock). The pipeline times frame
        // `pos + j` at `qpc + j` frames (its sample time) and at the pass that completes its packet (its delivery).
        // TODO 13's open point: the deadline itself in the header would take that lateness out (a ring header change: a
        // protocol version bump by the owner's rule of 2026-11-23).
        Ok(Some(ClockReading { pos_frames: pos as f64, qpc_hns: self.qpc.at_hns(qpc as i64) as f64, now_hns: self.qpc.now_hns() as f64 }))
    }

    /// The frames the driver filled with silence since the last call (the header's `underrun_frames`: a pass found less than
    /// a packet in the ring, and read_frame stopped at write_frame), as one underrun, its frames for `take_silence`. The
    /// pipeline reads the reader's packet first: a new recording's first passes find the ring empty (the live discard) and
    /// count before its starting fill.
    fn take_underruns(&mut self) -> u64 {
        let count = self.attachment.ring().header().underrun_frames.load(Ordering::Relaxed);
        self.missing = count.saturating_sub(self.last_missing);
        self.last_missing = count;
        u64::from(self.missing > 0)
    }

    fn take_silence(&mut self) -> u64 {
        std::mem::take(&mut self.missing)
    }

    fn take_late(&mut self) -> u64 {
        0
    }
}
