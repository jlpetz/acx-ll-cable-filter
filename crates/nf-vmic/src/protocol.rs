//! The driver's protocol, mirrored from `driver/src/Shared/nf_vmic.h`: the same constants, and the same layouts, checked against
//! the header's offsets at compile time. Change both together. Bump [`VERSION`] on a change to a layout or to a rule the app must
//! follow (ATTACH, the ring's header and the ring's rules for the app's side); [`Stats`] carries its size, checked with the
//! version ([`Device::stats`](crate::Device::stats)), and the driver's handling of its other capture streams can change without
//! one (the owner, 2026-11-23). Live ATTACH and its header use [`LIVE_VERSION`]; ordinary ATTACH and STATS retain [`VERSION`].

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64};

/// `NF_VMIC_MAGIC`: "NFVM" in memory order.
pub const MAGIC: u32 = 0x4D56_464E;
/// `NF_VMIC_VERSION`.
pub const VERSION: u32 = 1;
/// `NF_VMIC_LIVE_VERSION`: live ATTACH request and its ring. Ordinary feeds and STATS keep the v1 contract.
pub const LIVE_VERSION: u32 = 2;
/// `NF_VMIC_USER_PATH`: the control device, opened from user mode.
pub const USER_PATH: &str = r"\\.\NoiseFilterVmic";

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const METHOD_OUT_DIRECT: u32 = 2;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

/// winioctl.h's `CTL_CODE`.
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// `NF_VMIC_IOCTL_ATTACH`: input an [`AttachIn`], output the ring ([`ring_bytes`] long, starting on a page boundary); pending
/// while attached. [`ATTACH_REFUSALS`], [`ATTACH_FAILURES_AT_ONCE`] and [`ATTACH_ENDINGS`] list how it completes.
pub const IOCTL_ATTACH: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_OUT_DIRECT, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
/// `NF_VMIC_IOCTL_STATS`: no input; output a [`Stats`].
pub const IOCTL_STATS: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS);
const _: () = assert!(IOCTL_ATTACH == 0x0022_E002 && IOCTL_STATS == 0x0022_6004);

/// The ring's format: the capture endpoint's device format (spike check 3).
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// `NF_VMIC_FORMAT_FLOAT32`: IEEE 754 binary32, interleaved left, right.
pub const FORMAT_FLOAT32: u16 = 1;
pub const FRAME_BYTES: u32 = 8;
/// The header's size; the frames follow it.
pub const HEADER_BYTES: u32 = 256;
/// `NF_VMIC_DRIVER_LINE_OFFSET`: line 2, the driver's.
pub const DRIVER_LINE_OFFSET: u32 = 128;
/// `NF_VMIC_LINE_BYTES`: the header's four lines.
pub const LINE_BYTES: u32 = 64;
/// The ring's capacity: a power of two from 1024 frames (21.3 ms: two 480-frame packets, a shared stream's 10 ms period) to
/// 2^18 (5.46 s, 2 MiB). The capacity, and the fill the app keeps, must be at least one capture packet
/// ([`RingHeader::packet_frames`], [`Stats::reader_packet_frames`]), or every pass underruns.
pub const MIN_CAPACITY_FRAMES: u32 = 1024;
pub const MAX_CAPACITY_FRAMES: u32 = 1 << 18;
/// `NF_VMIC_MIN_RING_BYTES` and `NF_VMIC_MAX_RING_BYTES`: the ATTACH output buffer's limits.
pub const MIN_RING_BYTES: usize = ring_bytes(MIN_CAPACITY_FRAMES);
pub const MAX_RING_BYTES: usize = ring_bytes(MAX_CAPACITY_FRAMES);

/// ATTACH flag for a live microphone. Idle capture intervals discard queued audio on the next reader.
pub const ATTACH_LIVE: u32 = 1;

/// [`RingHeader::state`] bits: the driver published this ring.
pub const STATE_ATTACHED: u32 = 0x1;
/// The capture stream holding the reader slot read the ring at its latest pass (underrun or not).
pub const STATE_READING: u32 = 0x2;
/// That stream isn't 48 kHz float32 stereo: its packets are silence, the ring not read.
pub const STATE_FORMAT_MISMATCH: u32 = 0x4;
/// Live feed only: another recorder is running and can take the vacant slot on its next pass.
pub const STATE_LIVE_HANDOFF: u32 = 0x8;

/// [`Stats::flags`] bits. An app ring is attached (the `attached_*` fields are valid).
pub const STATS_ATTACHED: u32 = 0x1;
/// The PnP device is there ([`Stats::loopback`] is valid).
pub const STATS_LOOPBACK: u32 = 0x2;
/// The header's [`STATE_READING`] (the reader's latest pass).
///
/// With [`STATS_ATTACHED`], this and [`STATS_FORMAT_MISMATCH`] are the State bits the stream holding the reader slot wrote to
/// the header at its latest pass, and [`Stats::reader_packet_frames`] is the PacketFrames it wrote, so STATS and the header
/// agree. While no stream holds the slot all three are 0. A stream in another format holds the slot without reading:
/// FORMAT_MISMATCH, and its packet in its own format's frames (0 if the driver can't count its packet in frames).
pub const STATS_READING: u32 = 0x4;
/// The header's [`STATE_FORMAT_MISMATCH`] (the reader's latest pass).
pub const STATS_FORMAT_MISMATCH: u32 = 0x8;

/// [`Stats::stream`] indexes.
pub const KIND_RENDER: usize = 0;
pub const KIND_CAPTURE: usize = 1;
pub const KINDS: usize = 2;

/// The timer-lateness histogram: upper bounds of buckets 0-6 in 100 ns units; bucket 7 holds the rest (5 ms and over).
pub const LATENESS_BUCKETS: usize = 8;
pub const LATENESS_BOUNDS_HNS: [u64; LATENESS_BUCKETS - 1] = [500, 1_000, 2_500, 5_000, 10_000, 20_000, 50_000];
pub const LATENESS_LABELS: [&str; LATENESS_BUCKETS] =
    ["0-50 us", "50-100 us", "100-250 us", "250-500 us", "0.5-1 ms", "1-2 ms", "2-5 ms", "5 ms and over"];

/// The event logs' slots (`NF_VMIC_SKIP_LOG_ENTRIES`, `NF_VMIC_UNDERRUN_LOG_ENTRIES`): [`Stats::skip_log`] and
/// [`Stats::underrun_log`].
pub const SKIP_LOG_ENTRIES: usize = 64;
pub const UNDERRUN_LOG_ENTRIES: usize = 32;

/// [`Event::kind`]: the skip log's kinds (a stream never got some of the audio it was due), then the underrun log's (silence
/// with nothing lost). nf_vmic.h's "The event logs" has what each one's fields hold.
pub const EVENT_HANDOFF_SKIP: u16 = 1;
pub const EVENT_SECONDARY_SKIP: u16 = 2;
pub const EVENT_SECONDARY_JUMP: u16 = 3;
pub const EVENT_LATE_READ: u16 = 4;
pub const EVENT_RESYNC: u16 = 5;
pub const EVENT_LIVE_DISCARD: u16 = 6;
pub const EVENT_LOOPBACK_LEAD_SKIP: u16 = 7;
pub const EVENT_READER_UNDERRUN: u16 = 8;
pub const EVENT_SECONDARY_UNDERRUN: u16 = 9;
pub const EVENT_LOOPBACK_UNDERRUN: u16 = 10;

/// [`Event::flags`] bits. The stream held the reader slot at that pass.
pub const EVENTF_READER: u16 = 0x0001;
/// [`Event::ring_from`] and [`Event::ring_to`] hold the ring's frames.
pub const EVENTF_RING: u16 = 0x0002;
/// Part of the span was past the copy's end: the reader's writes there still add to the ring span.
pub const EVENTF_PENDING: u16 = 0x0004;
/// The ring span may be short: the driver's records of the copy ran out.
pub const EVENTF_UNTRACKED: u16 = 0x0008;
/// A resync or live discard to a `write_frame` behind `read_frame` (`ring_to` < `ring_from`).
pub const EVENTF_BACKWARD: u16 = 0x0010;
/// The stream had given the reader slot up (its Pause had begun).
pub const EVENTF_PAUSING: u16 = 0x0020;
/// A secondary underrun's silence: frames it had had (its place moved back), the copy's gap in the reader's time line, or
/// before the copy's valid start.
pub const EVENTF_BEHIND: u16 = 0x0100;
pub const EVENTF_GAP: u16 = 0x0200;
pub const EVENTF_OLD: u16 = 0x0400;

/// The ring's length for `capacity_frames` (`NF_VMIC_RING_BYTES`): the ATTACH output buffer's length, exactly. The buffer must
/// also start on a page boundary, as [`RingMemory`](crate::RingMemory)'s `VirtualAlloc` region does.
pub const fn ring_bytes(capacity_frames: u32) -> usize {
    HEADER_BYTES as usize + capacity_frames as usize * FRAME_BYTES as usize
}

/// Whether the driver takes a ring of `capacity_frames`: a power of two within the limits.
pub fn capacity_ok(capacity_frames: u32) -> bool {
    capacity_frames.is_power_of_two() && (MIN_CAPACITY_FRAMES..=MAX_CAPACITY_FRAMES).contains(&capacity_frames)
}

/// The bucket the driver counts a timer lateness of `hns` in (AppRing.cpp, `NfVmicLatenessBucket`).
pub fn lateness_bucket(hns: u64) -> usize {
    LATENESS_BOUNDS_HNS.iter().position(|&bound| hns < bound).unwrap_or(LATENESS_BUCKETS - 1)
}

/// A status ATTACH completes with: the driver's NTSTATUS, the Win32 error the app sees for it (DeviceIoControl's last error, or
/// GetOverlappedResult's: RtlNtStatusToDosError's mapping, which a test checks), and what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub ntstatus: u32,
    /// The NTSTATUS's name.
    pub name: &'static str,
    pub win32: u32,
    pub meaning: &'static str,
}

/// ATTACH's refusals: the driver completes the request without publishing the ring. KMDF pends every request it delivers to the
/// driver, so on the overlapped handle each comes back as ERROR_IO_PENDING from DeviceIoControl and then its status from
/// GetOverlappedResult ([`AttachError::pending`](crate::AttachError::pending)). The driver checks the input's magic and version
/// before any size, so a client of another version always gets ERROR_REVISION_MISMATCH.
#[rustfmt::skip]
pub const ATTACH_REFUSALS: [Outcome; 10] = [
    Outcome { ntstatus: 0xC000_0010, name: "STATUS_INVALID_DEVICE_REQUEST", win32: 1, meaning: "not from user mode" },
    Outcome { ntstatus: 0xC000_0059, name: "STATUS_REVISION_MISMATCH", win32: 1306,
              meaning: "the input's or the header's version isn't the driver's: it speaks another protocol version" },
    Outcome { ntstatus: 0xC000_0206, name: "STATUS_INVALID_BUFFER_SIZE", win32: 1784,
              meaning: "the ring's length is below MIN_RING_BYTES, above MAX_RING_BYTES, or not the header's total_bytes and \
                        ring_bytes(capacity_frames)" },
    Outcome { ntstatus: 0xC000_000D, name: "STATUS_INVALID_PARAMETER", win32: 87,
              meaning: "something else in the input or the header, or a ring that isn't one MDL of exactly the buffer" },
    Outcome { ntstatus: 0x8000_0002, name: "STATUS_DATATYPE_MISALIGNMENT", win32: 998,
              meaning: "the ring doesn't start on a page boundary" },
    Outcome { ntstatus: 0xC000_009A, name: "STATUS_INSUFFICIENT_RESOURCES", win32: 1450,
              meaning: "the driver couldn't map the ring into system space" },
    Outcome { ntstatus: 0xC000_02B6, name: "STATUS_DEVICE_REMOVED", win32: 1617,
              meaning: "the driver is going away: close every handle to the control device (an open handle keeps the driver loaded)" },
    Outcome { ntstatus: 0x8000_0011, name: "STATUS_DEVICE_BUSY", win32: 170,
              meaning: "another ring is attached (`stats` shows its process)" },
    FORMAT_REFUSAL,
    Outcome { ntstatus: 0xC000_0120, name: "STATUS_CANCELLED", win32: 995,
              meaning: "the request was cancelled, or the handle closed, before the ring was published" },
];

/// The refusal of a ring the capture endpoint can't play: a capture stream is open on it in a format other than 48 kHz float32
/// stereo (a client that opened it in 24-in-32 PCM, say). The driver leaves that client alone and counts the refusal
/// ([`RingStats::format_rejects`]). While a ring is attached the driver refuses the opposite order too: a capture stream created
/// in another format ([`RingStats::refused_capture_opens`]; its client sees the failure from `IAudioClient::Initialize`).
#[rustfmt::skip]
pub const FORMAT_REFUSAL: Outcome = Outcome {
    ntstatus: 0xC000_00BB, name: "STATUS_NOT_SUPPORTED", win32: 50,
    meaning: "a capture stream is open in a format other than 48 kHz float32 stereo, so the ring couldn't be played: the driver \
              leaves that client running; feed through the app's WASAPI output instead (24-in-32 PCM into the speaker endpoint, \
              the loopback path), or close that client and attach again",
};

/// ATTACH failures DeviceIoControl returns at once, from before the driver saw the request (it never went pending): the I/O
/// manager's checks, or a purged queue. ERROR_NOACCESS and ERROR_NO_SYSTEM_RESOURCES are refusals too, from GetOverlappedResult:
/// which call returned the code tells them apart ([`refusal`]). The I/O manager's allocations and locks can fail with other codes.
/// The NTSTATUS behind 998 here is the probe's exception (inference from how the I/O manager probes a buffer, not checked).
#[rustfmt::skip]
pub const ATTACH_FAILURES_AT_ONCE: [Outcome; 4] = [
    Outcome { ntstatus: 0xC000_0022, name: "STATUS_ACCESS_DENIED", win32: 5,
              meaning: "the I/O manager's access check: the handle wasn't opened with read and write access" },
    Outcome { ntstatus: 0xC000_0005, name: "STATUS_ACCESS_VIOLATION", win32: 998,
              meaning: "the I/O manager couldn't read the input, or probe the ring's memory for writing" },
    Outcome { ntstatus: 0xC000_009A, name: "STATUS_INSUFFICIENT_RESOURCES", win32: 1450,
              meaning: "the I/O manager couldn't allocate or lock what the request needs" },
    QUEUE_PURGED,
];

/// How an attached ring's request ends. Either way the driver unpublishes the ring before it completes the request, and never
/// touches the ring after.
#[rustfmt::skip]
pub const ATTACH_ENDINGS: [Outcome; 2] = [
    Outcome { ntstatus: 0xC000_0120, name: "STATUS_CANCELLED", win32: 995,
              meaning: "cancelled: CancelIoEx, the thread that issued it or its process exiting, or the handle closed" },
    Outcome { ntstatus: 0xC000_02B6, name: "STATUS_DEVICE_REMOVED", win32: 1617,
              meaning: "the driver is going away: close every handle to the control device (an open handle keeps the driver loaded)" },
];

/// Every request on a handle opened before its control device was deleted (the device went away), from then on: DeviceIoControl
/// fails at once, the queue being purged. The driver deletes the symbolic link with the device, so a new handle opens on the next
/// control device, once a PnP device is back (ERROR_FILE_NOT_FOUND until then). The framework destroys the deleted one only once
/// the last handle to it closes, and until then it keeps the driver loaded.
#[rustfmt::skip]
pub const QUEUE_PURGED: Outcome = Outcome {
    ntstatus: 0xC000_0184, name: "STATUS_INVALID_DEVICE_STATE", win32: 22,
    meaning: "this handle is on a control device that was deleted (the driver went away; its queue is purged): close it and open \
              a new one (an open handle keeps the old device, and the driver, loaded)",
};

/// What an ATTACH that failed with Win32 error `win32` was refused for. `pending`: the request went pending first, so the code
/// came from GetOverlappedResult, and is looked up in [`ATTACH_REFUSALS`] first; otherwise DeviceIoControl returned it at once,
/// and [`ATTACH_FAILURES_AT_ONCE`] comes first. Either way the other table is the fallback.
pub fn refusal(win32: u32, pending: bool) -> Option<&'static Outcome> {
    let (first, second): (&'static [Outcome], &'static [Outcome]) =
        if pending { (&ATTACH_REFUSALS, &ATTACH_FAILURES_AT_ONCE) } else { (&ATTACH_FAILURES_AT_ONCE, &ATTACH_REFUSALS) };
    first.iter().chain(second).find(|o| o.win32 == win32)
}

/// How an attached ring's request that completed with Win32 error `win32` ended (one of [`ATTACH_ENDINGS`]).
pub fn ending(win32: u32) -> Option<&'static Outcome> {
    ATTACH_ENDINGS.iter().find(|o| o.win32 == win32)
}

/// `NF_VMIC_RING_HEADER`, at the start of the ring: four 64-byte lines, then the frames. Line 0 is the app's, set before ATTACH
/// (the driver copies it once and never reads it again); line 1 is the app's `write_frame`; line 2 is the driver's. The fields
/// either side writes while the other may look are atomics.
#[repr(C)]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub header_bytes: u32,
    pub capacity_frames: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: u16,
    pub frame_bytes: u32,
    pub flags: u32,
    pub total_bytes: u64,
    pub reserved0: [u8; 24],
    /// Frames written. The app stores it with release ordering, after the frames.
    pub write_frame: AtomicU64,
    pub reserved1: [u8; 56],
    /// The next frame the driver reads. The driver stores it with release ordering, after its copy.
    pub read_frame: AtomicU64,
    /// This attachment's frames copied into capture packets.
    pub frames_copied: AtomicU64,
    /// Frames the driver zero-filled because the ring didn't hold them yet.
    pub underrun_frames: AtomicU64,
    /// Re-syncs: `write_frame` went backwards or more than a ring ahead.
    pub resyncs: AtomicU32,
    /// Packets zero-filled because the capture stream wasn't 48 kHz float32 stereo.
    pub mismatch_packets: AtomicU32,
    /// `STATE_*` bits.
    pub state: AtomicU32,
    /// The packet of the capture stream holding the reader slot, in frames (0: none; see [`STATS_READING`]).
    pub packet_frames: AtomicU32,
    /// Capture passes of the stream holding the reader slot, FORMAT_MISMATCH ones included: a rising count says a stream holds
    /// the slot, not that it reads ([`Self::frames_copied`] measures reading).
    pub passes: AtomicU64,
    /// The driver's QPC at the latest of those passes.
    pub last_pass_qpc: AtomicU64,
    /// Even sequence for a coherent read of the live driver clock; odd while a reader updates it. Zero for ordinary attachments.
    pub clock_sequence: AtomicU64,
    pub reserved3: [u8; 64],
}

/// `NF_VMIC_ATTACH_IN`: ATTACH's input buffer. Magic and Version are its first 8 bytes, and the ring header's, in every version:
/// ATTACH checks the input's before any size, and the header's before its other fields.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachIn {
    pub magic: u32,
    pub version: u32,
    pub in_bytes: u32,
    pub flags: u32,
}

impl AttachIn {
    pub const fn new() -> Self {
        Self { magic: MAGIC, version: VERSION, in_bytes: size_of::<Self>() as u32, flags: 0 }
    }
}

impl Default for AttachIn {
    fn default() -> Self {
        Self::new()
    }
}

/// `NF_VMIC_STREAM_STATS`: one stream kind's timer, cumulative since the driver loaded.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Passes started by the stream's timer.
    pub timer_passes: u64,
    /// Passes run at once because the next was already due (each is a glitch adjustment of the stream's time line).
    pub glitch_passes: u64,
    /// The sum of those adjustments, 100 ns units.
    pub glitch_hns: u64,
    /// The largest timer lateness (the pass's QPC time minus the time the timer was armed for), 100 ns units.
    pub lateness_max_hns: u64,
    /// The sum of the latenesses (the mean is this over `timer_passes`).
    pub lateness_sum_hns: u64,
    /// Timer passes that ran before their time (rounding; expected 0).
    pub early_passes: u64,
    /// Pause-to-Run transitions.
    pub runs: u64,
    /// QPC ticks the timer passes spent working (the callback's first clock read to the end of its work, glitch passes run
    /// at once included): over `timer_passes` and the QPC frequency, the mean CPU time of a pass.
    pub work_qpc_ticks: u64,
    /// Timer passes by lateness ([`LATENESS_BOUNDS_HNS`]).
    pub lateness_buckets: [u64; LATENESS_BUCKETS],
}

/// `NF_VMIC_RING_STATS`: the app ring, cumulative since the driver loaded.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingStats {
    pub attaches: u64,
    pub detaches: u64,
    /// Ended by a cancel: CancelIoEx, or the issuing thread or process exiting.
    pub detach_cancel: u64,
    /// Ended by the handle's cleanup (closed with the attach still pending).
    pub detach_cleanup: u64,
    /// Ended because the device went away (the request completes with STATUS_DEVICE_REMOVED).
    pub detach_removal: u64,
    /// ATTACHes refused because a ring was attached already.
    pub busy_rejects: u64,
    /// ATTACHes refused by validation.
    pub invalid_rejects: u64,
    /// Capture passes that found a ring attached.
    pub passes: u64,
    pub frames_copied: u64,
    pub underrun_frames: u64,
    /// Passes with at least one underrun frame.
    pub underrun_packets: u64,
    pub resyncs: u64,
    /// Passes zero-filled because the capture stream wasn't 48 kHz float32 stereo.
    pub mismatch_packets: u64,
    /// Passes of a capture stream that isn't the reader (one reader at a time; [`Stats::secondary_frames_copied`] counts what
    /// such streams were given of the ring's audio).
    pub not_reader_packets: u64,
    /// ATTACHes refused with [`FORMAT_REFUSAL`]: a capture stream was open in a format other than 48 kHz float32 stereo.
    pub format_rejects: u64,
    /// Capture stream creations the driver refused because a ring was attached and the stream's format wasn't 48 kHz float32
    /// stereo.
    pub refused_capture_opens: u64,
}

/// `NF_VMIC_LOOPBACK_STATS`: the render-to-capture loopback ring's counters (Public.h, `LOOPBACK_RING`), widened.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopbackStats {
    pub idle_packets: u64,
    pub mismatch_packets: u64,
    pub underrun_packets: u64,
    pub underrun_frames: u64,
    pub overrun_packets: u64,
    pub capture_anchors: u64,
    pub render_restarts: u64,
    pub render_gap_fills: u64,
    pub render_late_packets: u64,
    pub render_lost_packets: u64,
    pub render_not_owner: u64,
    /// Stream creations the loopback refused: a format other than one open on the other circuit, or (not expected) no free
    /// format slot on its own. Like every loopback counter, the first-installed device's only.
    pub refused_opens: u64,
}

/// `NF_VMIC_CAPTURE_TRACE` (TODO 19, the capture-timestamp trace): what capture streams told ACX of their packets' times, the
/// latest of any capture stream. QPCs in ticks ([`Stats::qpc_frequency`]); each field is read whole, the set not as one.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureTrace {
    /// EvtAcxStreamGetCapturePacket calls.
    pub packet_queries: u64,
    /// The latest one's clock reading.
    pub last_query_qpc: u64,
    /// Its answer: the packet number and the packet's QPCPacketStart.
    pub last_query_packet: u64,
    pub last_query_packet_qpc: u64,
    /// The latest AcxRtStreamNotifyPacketComplete's packet and QPCCompletion.
    pub last_notify_packet: u64,
    pub last_notify_qpc: u64,
    /// EvtAcxStreamGetPresentationPosition calls.
    pub position_queries: u64,
    /// EvtAcxStreamGetCurrentPacket calls.
    pub current_packet_queries: u64,
}

/// `NF_VMIC_EVENT`: one entry of an event log (nf_vmic.h, "The event logs").
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// Its number in its log, from 1; 0: an unused slot.
    pub sequence: u64,
    /// The driver's QPC at the pass that logged it (a run's first pass).
    pub qpc: u64,
    /// `EVENT_*`.
    pub kind: u16,
    /// `EVENTF_*`.
    pub flags: u16,
    /// The capture stream's serial: from 1, in creation order since the driver loaded.
    pub stream: u32,
    /// Its packet, in its format's frames.
    pub packet_frames: u32,
    /// By kind: the frames its counter counted, or the silence it got.
    pub frames: u32,
    /// The attachment ([`RingStats::attaches`] when logged, its low 32 bits); 0: the loopback's.
    pub attach: u32,
    /// `[span_from, span_from + span_frames)`: frames on the driver's QPC time line.
    pub span_frames: u32,
    pub span_from: u64,
    /// `[ring_from, ring_to)`: the ring's frames, with [`EVENTF_RING`] (the app ring's positions, or the loopback ring's
    /// frames).
    pub ring_from: u64,
    pub ring_to: u64,
}

impl Event {
    /// Its ring span, where it has one.
    pub fn ring(&self) -> Option<(u64, u64)> {
        (self.flags & EVENTF_RING != 0).then_some((self.ring_from, self.ring_to))
    }

    /// Its kind's name, as the tool prints it.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            EVENT_HANDOFF_SKIP => "handoff skip",
            EVENT_SECONDARY_SKIP => "skip to a shorter lag",
            EVENT_SECONDARY_JUMP => "time-line jump",
            EVENT_LATE_READ => "late read",
            EVENT_RESYNC => "resync",
            EVENT_LIVE_DISCARD => "live discard",
            EVENT_LOOPBACK_LEAD_SKIP => "loopback lead skip",
            EVENT_READER_UNDERRUN => "reader underrun",
            EVENT_SECONDARY_UNDERRUN => "secondary underrun",
            EVENT_LOOPBACK_UNDERRUN => "loopback underrun",
            _ => "unknown",
        }
    }
}

/// One event log's slots, as STATS copies them: event n in slot (n - 1) % N.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventSlots<const N: usize>(pub [Event; N]);

impl<const N: usize> Default for EventSlots<N> {
    fn default() -> Self {
        Self([Event::default(); N])
    }
}

/// An event log read from its slots: its entries oldest first, and how many of the latest it should hold weren't readable
/// (a slot whose sequence isn't one of them, or isn't where it says: not expected, since STATS copies under the log's lock).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventLog {
    pub entries: Vec<Event>,
    /// The log's count when copied: the newest's sequence.
    pub logged: u64,
    pub unreadable: u64,
}

impl EventLog {
    /// The log's entries from `slots`, with `logged` its count: each of the latest `slots.len()` events in its own slot.
    pub fn read(slots: &[Event], logged: u64) -> Self {
        let n = slots.len() as u64;
        let oldest = logged.saturating_sub(n) + 1;
        let mut entries: Vec<Event> = (oldest..=logged)
            .filter_map(|seq| {
                let e = slots[((seq - 1) % n) as usize];
                (e.sequence == seq).then_some(e)
            })
            .collect();
        entries.sort_by_key(|e| e.sequence);
        let held = logged - (oldest - 1);
        Self { unreadable: held - entries.len() as u64, entries, logged }
    }
}

/// `NF_VMIC_STATS`: STATS's output.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub magic: u32,
    pub version: u32,
    pub stats_bytes: u32,
    /// `STATS_*` bits.
    pub flags: u32,
    pub qpc_frequency: u64,
    /// The snapshot's QPC, read last: every count and event-log entry in it is earlier.
    pub qpc_now: u64,
    /// The attached app's process (0: none attached).
    pub attached_pid: u32,
    pub attached_capacity_frames: u32,
    /// The driver's read position in the attached ring.
    pub attached_read_frame: u64,
    /// The `write_frame` the latest capture pass read (as the app wrote it: untrusted).
    pub attached_write_frame: u64,
    /// The packet of the capture stream holding the reader slot, in frames (0: none). Reported whenever a stream holds the slot,
    /// reading or not ([`STATS_READING`]).
    pub reader_packet_frames: u32,
    pub reserved0: u32,
    pub ring: RingStats,
    pub loopback: LoopbackStats,
    /// Frames of the ring's audio delivered from the driver's copy: to capture streams other than the reader, and to a new
    /// reader for the time the copy already held.
    pub secondary_frames_copied: u64,
    /// Frames zero-filled in their place: the ring's audio for the position wasn't there, or the stream had had it already (its
    /// place in the copy moved back, at a handoff to a reader with a longer packet: it never plays a frame twice).
    pub secondary_underrun_frames: u64,
    /// Times a capture stream that was playing the driver's copy took the reader slot another stream had held (a handoff).
    pub reader_handoffs: u64,
    /// The copy's frames such a stream never delivered: between its place in the copy and its first packet as the reader (a
    /// handoff's one-time skip, its lag at most; the last reader's underrun silence there included). A stream that plays on
    /// skips only behind a new reader with a shorter packet, counted apart ([`Stats::secondary_skipped_frames`]).
    pub handoff_skipped_frames: u64,
    /// Indexed by [`KIND_RENDER`] and [`KIND_CAPTURE`].
    pub stream: [StreamStats; KINDS],
    pub capture_trace: CaptureTrace,
    /// Times a capture stream playing the driver's copy moved on at once to a shorter reader packet's lag (a handoff to a reader
    /// with a shorter packet), skipping the difference once (the owner, 2026-11-23: lower latency over a one-time skip). The
    /// taker's skip is [`Stats::handoff_skipped_frames`].
    pub secondary_skips: u64,
    /// The copy's frames those skips passed over (the readers' packets' difference at most; the last reader's underrun silence
    /// there included, the copy's recorded gaps not): a recording that played on through such a handoff has a gap of exactly this.
    pub secondary_skipped_frames: u64,
    /// Times a loopback capture read moved on at once to a shorter render lead, skipping the difference once (valid with
    /// [`STATS_LOOPBACK`], as [`Stats::loopback`] is).
    pub loopback_lead_skips: u64,
    /// The render audio the loopback ring held between those reads at the two leads (the difference in leads at most).
    pub loopback_lead_skipped_frames: u64,
    /// The event logs' counts (the newest's sequence each), since the app feed's device was added. Valid with
    /// [`STATS_LOOPBACK`], as the logs are (the device's, as [`Stats::loopback`] is).
    pub skip_events_logged: u64,
    pub underrun_events_logged: u64,
    /// [`SKIP_LOG_ENTRIES`], [`UNDERRUN_LOG_ENTRIES`] and the size of an [`Event`], as the driver has them.
    pub skip_log_entries: u32,
    pub underrun_log_entries: u32,
    pub event_bytes: u32,
    pub reserved1: u32,
    /// Every event in which a capture stream never got some of the audio it was due: skips, jumps, late reads, resyncs.
    pub skip_log: EventSlots<SKIP_LOG_ENTRIES>,
    /// The silence streams got with nothing lost, a run an entry.
    pub underrun_log: EventSlots<UNDERRUN_LOG_ENTRIES>,
    /// Times a capture stream playing the driver's copy found its own time line moved later (a late timer's glitch adjustment),
    /// each an [`EVENT_SECONDARY_JUMP`] entry in the skip log, and the copy's frames they passed over (the entries' frames,
    /// added up): the device's, as the logs are (valid with [`STATS_LOOPBACK`]; the owner, 2026-11-24). The frames are what the
    /// copy held there as each jump was logged, as a skip's are counted (`secondary_skipped_frames`): where a jump's span ran
    /// past the copy's end, the audio the reader later wrote there joins only the entry's ring frames, not its frames or these.
    pub secondary_jumps: u64,
    pub secondary_jump_frames: u64,
}

impl Stats {
    /// The skip log, oldest first.
    pub fn skip_events(&self) -> EventLog {
        EventLog::read(&self.skip_log.0, self.skip_events_logged)
    }

    /// The underrun log, oldest first.
    pub fn underrun_events(&self) -> EventLog {
        EventLog::read(&self.underrun_log.0, self.underrun_events_logged)
    }
}

// The header's offsets (nf_vmic.h's layout tables and C_ASSERTs).
#[rustfmt::skip]
const _: () = {
    assert!(size_of::<AttachIn>() == 16);
    assert!(offset_of!(AttachIn, version) == 4);

    assert!(size_of::<RingHeader>() == HEADER_BYTES as usize);
    assert!(offset_of!(RingHeader, magic) == 0);
    assert!(offset_of!(RingHeader, version) == 4);
    assert!(offset_of!(RingHeader, header_bytes) == 8);
    assert!(offset_of!(RingHeader, capacity_frames) == 12);
    assert!(offset_of!(RingHeader, sample_rate) == 16);
    assert!(offset_of!(RingHeader, channels) == 20);
    assert!(offset_of!(RingHeader, sample_format) == 22);
    assert!(offset_of!(RingHeader, frame_bytes) == 24);
    assert!(offset_of!(RingHeader, flags) == 28);
    assert!(offset_of!(RingHeader, total_bytes) == 32);
    assert!(offset_of!(RingHeader, reserved0) == 40);
    assert!(offset_of!(RingHeader, write_frame) == 64);
    assert!(offset_of!(RingHeader, reserved1) == 72);
    assert!(offset_of!(RingHeader, read_frame) == 128 && DRIVER_LINE_OFFSET == 128);
    assert!(offset_of!(RingHeader, frames_copied) == 136);
    assert!(offset_of!(RingHeader, underrun_frames) == 144);
    assert!(offset_of!(RingHeader, resyncs) == 152);
    assert!(offset_of!(RingHeader, mismatch_packets) == 156);
    assert!(offset_of!(RingHeader, state) == 160);
    assert!(offset_of!(RingHeader, packet_frames) == 164);
    assert!(offset_of!(RingHeader, passes) == 168);
    assert!(offset_of!(RingHeader, last_pass_qpc) == 176);
    assert!(offset_of!(RingHeader, clock_sequence) == 184);
    assert!(offset_of!(RingHeader, reserved3) == (DRIVER_LINE_OFFSET + LINE_BYTES) as usize);
    assert!(MIN_RING_BYTES == 8_448 && MAX_RING_BYTES == 2_097_408);

    assert!(size_of::<StreamStats>() == 128);
    assert!(offset_of!(StreamStats, lateness_max_hns) == 24);
    assert!(offset_of!(StreamStats, runs) == 48);
    assert!(offset_of!(StreamStats, lateness_buckets) == 64);

    assert!(size_of::<RingStats>() == 128);
    assert!(offset_of!(RingStats, passes) == 56);
    assert!(offset_of!(RingStats, frames_copied) == 64);
    assert!(offset_of!(RingStats, not_reader_packets) == 104);
    assert!(offset_of!(RingStats, format_rejects) == 112);
    assert!(offset_of!(RingStats, refused_capture_opens) == 120);

    assert!(size_of::<LoopbackStats>() == 96);
    assert!(offset_of!(LoopbackStats, render_not_owner) == 80);
    assert!(offset_of!(LoopbackStats, refused_opens) == 88);

    assert!(size_of::<CaptureTrace>() == 64);
    assert!(offset_of!(CaptureTrace, last_notify_qpc) == 40);
    assert!(size_of::<Event>() == 64);
    assert!(offset_of!(Event, qpc) == 8);
    assert!(offset_of!(Event, kind) == 16);
    assert!(offset_of!(Event, flags) == 18);
    assert!(offset_of!(Event, stream) == 20);
    assert!(offset_of!(Event, packet_frames) == 24);
    assert!(offset_of!(Event, frames) == 28);
    assert!(offset_of!(Event, attach) == 32);
    assert!(offset_of!(Event, span_frames) == 36);
    assert!(offset_of!(Event, span_from) == 40);
    assert!(offset_of!(Event, ring_from) == 48);
    assert!(offset_of!(Event, ring_to) == 56);
    assert!(offset_of!(Stats, skip_events_logged) == 672);
    assert!(offset_of!(Stats, underrun_events_logged) == 680);
    assert!(offset_of!(Stats, skip_log_entries) == 688);
    assert!(offset_of!(Stats, underrun_log_entries) == 692);
    assert!(offset_of!(Stats, event_bytes) == 696);
    assert!(offset_of!(Stats, skip_log) == 704);
    assert!(offset_of!(Stats, underrun_log) == 704 + 64 * SKIP_LOG_ENTRIES);
    assert!(offset_of!(Stats, underrun_log) == 4_800);
    assert!(offset_of!(Stats, secondary_jumps) == 6_848);
    assert!(offset_of!(Stats, secondary_jump_frames) == 6_856);
    assert!(size_of::<Stats>() == 6_864);
    assert!(offset_of!(Stats, capture_trace) == 576);
    assert!(offset_of!(Stats, secondary_skips) == 640);
    assert!(offset_of!(Stats, secondary_skipped_frames) == 648);
    assert!(offset_of!(Stats, loopback_lead_skips) == 656);
    assert!(offset_of!(Stats, loopback_lead_skipped_frames) == 664);
    assert!(offset_of!(Stats, qpc_frequency) == 16);
    assert!(offset_of!(Stats, attached_pid) == 32);
    assert!(offset_of!(Stats, attached_read_frame) == 40);
    assert!(offset_of!(Stats, attached_write_frame) == 48);
    assert!(offset_of!(Stats, reader_packet_frames) == 56);
    assert!(offset_of!(Stats, ring) == 64);
    assert!(offset_of!(Stats, loopback) == 192);
    assert!(offset_of!(Stats, secondary_frames_copied) == 288);
    assert!(offset_of!(Stats, secondary_underrun_frames) == 296);
    assert!(offset_of!(Stats, reader_handoffs) == 304);
    assert!(offset_of!(Stats, handoff_skipped_frames) == 312);
    assert!(offset_of!(Stats, stream) == 320);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacities() {
        assert!(capacity_ok(1024) && capacity_ok(4096) && capacity_ok(1 << 18));
        assert!(!capacity_ok(512) && !capacity_ok(256) && !capacity_ok(1 << 19) && !capacity_ok(3000) && !capacity_ok(0));
        assert_eq!(ring_bytes(1024), 256 + 8192);
        assert_eq!(ring_bytes(MAX_CAPACITY_FRAMES), 256 + 2 * 1024 * 1024);
        // nf_vmic.h's comments: 8,448 and 2,097,408.
        assert_eq!((MIN_RING_BYTES, MAX_RING_BYTES), (8_448, 2_097_408));
        // The smallest ring holds two 10 ms shared-mode packets.
        const { assert!(MIN_CAPACITY_FRAMES >= 2 * 480) };
    }

    #[test]
    fn lateness_buckets_match_the_driver() {
        // Each bound opens the next bucket; 0 and anything past 5 ms land at the ends.
        assert_eq!(lateness_bucket(0), 0);
        assert_eq!(lateness_bucket(499), 0);
        assert_eq!(lateness_bucket(500), 1);
        assert_eq!(lateness_bucket(9_999), 4);
        assert_eq!(lateness_bucket(10_000), 5);
        assert_eq!(lateness_bucket(49_999), 6);
        assert_eq!(lateness_bucket(50_000), 7);
        assert_eq!(lateness_bucket(u64::MAX), 7);
    }

    #[test]
    fn attach_in_is_the_drivers() {
        let a = AttachIn::new();
        assert_eq!((a.magic, a.version, a.in_bytes, a.flags), (0x4D56_464E, 1, 16, 0));
        // "NFVM" in memory order.
        assert_eq!(&MAGIC.to_le_bytes(), b"NFVM");
    }

    /// Every outcome's Win32 error is what RtlNtStatusToDosError makes of its NTSTATUS (ntdll, no device), and its NTSTATUS is
    /// the one its name says (the windows crate's constant).
    #[test]
    fn outcomes_map_as_windows_does() {
        use windows::Win32::Foundation::{
            NTSTATUS, RtlNtStatusToDosError, STATUS_ACCESS_DENIED, STATUS_ACCESS_VIOLATION, STATUS_CANCELLED, STATUS_DATATYPE_MISALIGNMENT,
            STATUS_DEVICE_BUSY, STATUS_DEVICE_REMOVED, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_BUFFER_SIZE,
            STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED,
            STATUS_REVISION_MISMATCH,
        };
        let named = [
            ("STATUS_ACCESS_DENIED", STATUS_ACCESS_DENIED),
            ("STATUS_ACCESS_VIOLATION", STATUS_ACCESS_VIOLATION),
            ("STATUS_INVALID_DEVICE_REQUEST", STATUS_INVALID_DEVICE_REQUEST),
            ("STATUS_REVISION_MISMATCH", STATUS_REVISION_MISMATCH),
            ("STATUS_INVALID_BUFFER_SIZE", STATUS_INVALID_BUFFER_SIZE),
            ("STATUS_INVALID_PARAMETER", STATUS_INVALID_PARAMETER),
            ("STATUS_DATATYPE_MISALIGNMENT", STATUS_DATATYPE_MISALIGNMENT),
            ("STATUS_INSUFFICIENT_RESOURCES", STATUS_INSUFFICIENT_RESOURCES),
            ("STATUS_DEVICE_REMOVED", STATUS_DEVICE_REMOVED),
            ("STATUS_DEVICE_BUSY", STATUS_DEVICE_BUSY),
            ("STATUS_NOT_SUPPORTED", STATUS_NOT_SUPPORTED),
            ("STATUS_CANCELLED", STATUS_CANCELLED),
            ("STATUS_INVALID_DEVICE_STATE", STATUS_INVALID_DEVICE_STATE),
        ];
        for o in ATTACH_REFUSALS.iter().chain(&ATTACH_FAILURES_AT_ONCE).chain(&ATTACH_ENDINGS) {
            let (_, status) = named.iter().find(|(n, _)| *n == o.name).unwrap_or_else(|| panic!("{} isn't listed here", o.name));
            assert_eq!(o.ntstatus, status.0 as u32, "{}", o.name);
            // SAFETY: a pure ntdll function on a value.
            let win32 = unsafe { RtlNtStatusToDosError(NTSTATUS(o.ntstatus as i32)) };
            assert_eq!(o.win32, win32, "{} maps to {win32}", o.name);
        }
        // Within each table every Win32 error is its own, so the client can tell them apart. 998 and 1450 are in both: which call
        // returned them tells them apart. STATUS_CANCELLED is both a refusal and an ending, which the client tells apart by whether
        // the ring was published.
        for (table, pending) in [(&ATTACH_REFUSALS[..], true), (&ATTACH_FAILURES_AT_ONCE[..], false)] {
            for (i, o) in table.iter().enumerate() {
                assert!(table[i + 1..].iter().all(|p| p.win32 != o.win32), "{}", o.name);
                assert_eq!(refusal(o.win32, pending), Some(o));
            }
        }
        let name = |win32, pending| refusal(win32, pending).map(|o| o.name);
        assert_eq!((name(998, true), name(998, false)), (Some("STATUS_DATATYPE_MISALIGNMENT"), Some("STATUS_ACCESS_VIOLATION")));
        // The other table is the fallback.
        assert_eq!(
            (name(1306, false), name(5, true), name(22, true)),
            (Some("STATUS_REVISION_MISMATCH"), Some("STATUS_ACCESS_DENIED"), Some("STATUS_INVALID_DEVICE_STATE"))
        );
        assert_eq!((ending(995).map(|o| o.name), ending(1617).map(|o| o.name)), (Some("STATUS_CANCELLED"), Some("STATUS_DEVICE_REMOVED")));
        assert_eq!((refusal(2, true), refusal(2, false), ending(87)), (None, None, None));
        // The format refusal: ERROR_NOT_SUPPORTED, from GetOverlappedResult (a refusal goes pending first), listed right after
        // STATUS_DEVICE_BUSY as nf_vmic.h lists it; never an ending.
        assert_eq!((FORMAT_REFUSAL.win32, refusal(50, true)), (50, Some(&FORMAT_REFUSAL)));
        let busy = ATTACH_REFUSALS.iter().position(|o| o.name == "STATUS_DEVICE_BUSY").unwrap();
        assert_eq!(ATTACH_REFUSALS[busy + 1], FORMAT_REFUSAL);
        assert_eq!(ending(50), None);
    }

    /// The new counters sit where nf_vmic.h's Reserved fields were.
    #[test]
    fn the_counters_replace_reserved_fields() {
        assert_eq!((offset_of!(RingStats, format_rejects), offset_of!(RingStats, refused_capture_opens)), (112, 120));
        assert_eq!((offset_of!(Stats, secondary_frames_copied), offset_of!(Stats, secondary_underrun_frames)), (288, 296));
        assert_eq!((offset_of!(Stats, reader_handoffs), offset_of!(Stats, handoff_skipped_frames)), (304, 312));
        assert_eq!(offset_of!(Stats, handoff_skipped_frames) + 8, offset_of!(Stats, stream));
    }

    /// The skip counters of 2026-11-23 are appended: every earlier offset stands, and the size grows by their 32 bytes (no
    /// version bump: the owner's rule of 2026-11-23 bumps only for ATTACH, the ring's header or rules the app must follow).
    #[test]
    fn the_skip_counters_are_appended() {
        assert_eq!(offset_of!(Stats, capture_trace) + size_of::<CaptureTrace>(), offset_of!(Stats, secondary_skips));
        assert_eq!((offset_of!(Stats, secondary_skips), offset_of!(Stats, secondary_skipped_frames)), (640, 648));
        assert_eq!((offset_of!(Stats, loopback_lead_skips), offset_of!(Stats, loopback_lead_skipped_frames)), (656, 664));
        assert_eq!(offset_of!(Stats, loopback_lead_skipped_frames) + 8, offset_of!(Stats, skip_events_logged));
        assert_eq!(VERSION, 1);
    }

    /// The event logs of 2026-11-24 are appended in turn (diagnostic fields: no version bump, the owner's rule), the skip log
    /// on a 64-byte line, each entry a line, the underrun log after it.
    #[test]
    fn the_event_logs_are_appended() {
        assert_eq!(offset_of!(Stats, skip_events_logged), 672);
        assert_eq!(offset_of!(Stats, skip_log) % 64, 0);
        assert_eq!(offset_of!(Stats, skip_log) + SKIP_LOG_ENTRIES * size_of::<Event>(), offset_of!(Stats, underrun_log));
        assert_eq!(offset_of!(Stats, underrun_log) + UNDERRUN_LOG_ENTRIES * size_of::<Event>(), offset_of!(Stats, secondary_jumps));
        assert_eq!((size_of::<Event>(), VERSION), (64, 1));
        // A copy of the default is all zero: no slot holds an entry, both logs empty.
        let s = Stats::default();
        assert!(s.skip_events().entries.is_empty() && s.underrun_events().entries.is_empty());
    }

    /// The time-line jump counters of 2026-11-24 (the owner: two totals per device, so the log's cross-check covers jumps) are
    /// appended after the logs: every earlier offset stands, and the size grows by their 16 bytes, with no version bump
    /// (diagnostic fields: the owner's rule of 2026-11-23).
    #[test]
    fn the_jump_counters_are_appended() {
        assert_eq!(offset_of!(Stats, underrun_log) + UNDERRUN_LOG_ENTRIES * size_of::<Event>(), 6_848);
        assert_eq!((offset_of!(Stats, secondary_jumps), offset_of!(Stats, secondary_jump_frames)), (6_848, 6_856));
        assert_eq!(offset_of!(Stats, secondary_jump_frames) + 8, size_of::<Stats>());
        assert_eq!((size_of::<Stats>(), VERSION), (6_864, 1));
    }

    /// A log read from its slots: the latest it keeps, oldest first; a slot not holding the number its place says is
    /// unreadable (the driver copies under the log's lock, so not expected); an empty log is empty.
    #[test]
    fn an_event_log_is_read_from_its_slots() {
        let mut slots = [Event::default(); 4];
        for n in 1..=6u64 {
            slots[((n - 1) % 4) as usize] = Event { sequence: n, kind: EVENT_LATE_READ, ..Default::default() };
        }
        let log = EventLog::read(&slots, 6);
        assert_eq!(log.entries.iter().map(|e| e.sequence).collect::<Vec<_>>(), [3, 4, 5, 6]);
        assert_eq!(log.unreadable, 0);
        slots[1].sequence = 2;
        assert_eq!(EventLog::read(&slots, 6).unreadable, 1, "slot 1 should hold 6");
        assert_eq!(EventLog::read(&slots[..], 0), EventLog::default());
        let short = EventLog::read(&[Event { sequence: 1, ..Default::default() }, Event::default()], 2);
        assert_eq!((short.entries.len(), short.unreadable), (1, 1), "event 2's slot is empty");
        assert_eq!(Event { kind: EVENT_SECONDARY_SKIP, ..Default::default() }.kind_name(), "skip to a shorter lag");
        assert_eq!(Event { flags: EVENTF_RING, ring_from: 3, ring_to: 9, ..Default::default() }.ring(), Some((3, 9)));
        assert_eq!(Event { ring_from: 3, ring_to: 9, ..Default::default() }.ring(), None);
    }
}
