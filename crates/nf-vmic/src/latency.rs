//! The arithmetic of nf-vmic-test's `latency`: publication-to-receipt latency through the ring and the capture endpoint.
//!
//! One process writes the test signal into an attached ring and records the capture endpoint. The feed logs a [`Block`] for every
//! write it publishes: the frames, and the QPC just after it stored `write_frame`. The capture thread logs a [`Receipt`] for
//! every packet: the QPC when GetBuffer returned (the host's receipt time) and GetBuffer's device QPC position (the time the
//! endpoint gives for the packet's first frame, in 100 ns units), with the packet's signal frames as [`Run`]s of consecutive
//! frame numbers, each with its place in the packet ([`split_runs`] decodes each frame's code back to its frame number,
//! [`unwrap_code`]).
//!
//! A frame's host latency is its packet's receipt time minus the publication time of the block that held it: the whole packet
//! arrives at once. Its device latency is its own device time minus that publication: the packet's first frame's time plus the
//! frame's place in the packet at 48 kHz. Every frame counts once ([`latencies`] weights each (packet, block) pair by the frames
//! they share, and times the device's frames one by one, or on a long run in groups stamped at each group's centre,
//! [`device_group`]). The percentiles are nearest-rank over those frames ([`percentile`]). The device latency can come out
//! negative: the device time is when the frame was due on the endpoint's time line, and a driver fills a packet when it ends
//! (this one copies a packet from the ring at the pass that completes it, a period after its first frame was due).
//!
//! Everything the threads do per block or per packet here ([`Log::push`], [`split_runs`]) allocates nothing: the logs are
//! sized before the run starts. The analysis runs after the threads have stopped.

use crate::protocol::SAMPLE_RATE;
use crate::signal::{self, CODE_MODULUS};

/// 100 ns units per second.
pub const HNS_PER_SECOND: u64 = 10_000_000;

/// The most device samples [`latencies`] makes: it times the device's frames one by one up to this many frames (43.7 s), and in
/// groups beyond ([`device_group`]), so the analysis of a run as long as 600 s holds at most 32 MB of them.
pub const DEVICE_SAMPLES_MAX: u64 = 1 << 21;

/// A log of fixed capacity, allocated up front: [`Self::push`] never allocates; once the log is full it counts what it drops.
#[derive(Debug)]
pub struct Log<T> {
    items: Vec<T>,
    dropped: u64,
}

impl<T> Log<T> {
    /// A log with room for at least `capacity` entries.
    pub fn with_capacity(capacity: usize) -> Self {
        Self { items: Vec::with_capacity(capacity), dropped: 0 }
    }

    /// Appends `item` if there is room (no allocation: the length stays below the capacity reserved at the start), else
    /// counts it as dropped. Returns whether it was kept.
    pub fn push(&mut self, item: T) -> bool {
        if self.items.len() < self.items.capacity() {
            self.items.push(item);
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Entries refused because the log was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// One write the feed published: frames `[start, end)`, `qpc` (QueryPerformanceCounter ticks) read just after the store of
/// `write_frame` that published them, and the driver's `read_frame` the feed saw just before it wrote (the fill it topped up
/// from is `start - read_frame`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Block {
    pub qpc: u64,
    pub start: u64,
    pub end: u64,
    pub read_frame: u64,
}

impl Block {
    /// The fill the feed found before this write.
    pub fn fill_before(&self) -> u64 {
        self.start.saturating_sub(self.read_frame)
    }
}

/// One capture packet: `host_qpc` (ticks) read just after GetBuffer returned, GetBuffer's device QPC position `device_hns`
/// (100 ns units; `None` where GetBuffer flagged it AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR), its `frames`, and its runs of signal
/// frames: `runs` entries of the run log from `first_run`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    pub host_qpc: u64,
    pub device_hns: Option<u64>,
    pub frames: u32,
    pub first_run: u32,
    pub runs: u32,
}

/// Frames `[first, first + len)` of the signal, consecutive in one packet, the first of them `offset` frames into the packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub first: u64,
    pub len: u32,
    pub offset: u32,
}

impl Run {
    pub fn end(&self) -> u64 {
        self.first + u64::from(self.len)
    }
}

/// The frame number that carries code `k`: the latest one below `written` (the ring's `write_frame`, read after the packet
/// arrived: every frame in it was written before that). `None` when no frame below `written` carries it. Unambiguous while a
/// frame arrives less than 2^23 frames (174.8 s) after it was written, which a ring of at most 2^18 frames ensures.
pub fn unwrap_code(k: u32, written: u64) -> Option<u64> {
    let last = written.checked_sub(1)?;
    let back = last.wrapping_sub(u64::from(k)) & (CODE_MODULUS - 1);
    last.checked_sub(back)
}

/// What [`split_runs`] counted besides the runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCounts {
    /// Frames carrying a code.
    pub signal: u64,
    /// Silence: both samples' bits zero.
    pub silent: u64,
    /// Neither silence nor a code.
    pub corrupt: u64,
    /// Codes no frame below `written` carries (a frame from nowhere).
    pub unmatched: u64,
}

impl FrameCounts {
    pub fn add(&mut self, other: &FrameCounts) {
        self.signal += other.signal;
        self.silent += other.silent;
        self.corrupt += other.corrupt;
        self.unmatched += other.unmatched;
    }
}

/// Splits one packet's frames into runs of consecutive signal frames, decoding each frame's code back to its frame number
/// ([`unwrap_code`] against `written`), and hands each run to `emit` in order, with its first frame's place in the packet.
/// Silence and corrupt frames end a run. Allocates nothing.
pub fn split_runs(frames: impl IntoIterator<Item = [f32; 2]>, written: u64, mut emit: impl FnMut(Run)) -> FrameCounts {
    let mut counts = FrameCounts::default();
    let mut run: Option<Run> = None;
    for (offset, f) in (0u32..).zip(frames) {
        let Some(k) = signal::decode(f) else {
            if signal::is_silence(f) {
                counts.silent += 1;
            } else {
                counts.corrupt += 1;
            }
            if let Some(r) = run.take() {
                emit(r);
            }
            continue;
        };
        counts.signal += 1;
        if let Some(r) = &mut run
            && signal::code(r.end()) == k
        {
            r.len += 1;
            continue;
        }
        if let Some(r) = run.take() {
            emit(r);
        }
        match unwrap_code(k, written) {
            Some(n) => run = Some(Run { first: n, len: 1, offset }),
            None => counts.unmatched += 1,
        }
    }
    if let Some(r) = run {
        emit(r);
    }
    counts
}

/// Whether the runs, in arrival order, carry every frame once and in order: forward jumps (frames lost) and backward ones
/// (frames delivered again), with their sizes. A run that starts where the last ended is in order, silence between them or not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Continuity {
    pub gaps: u64,
    pub gap_frames: u64,
    pub repeats: u64,
    pub repeat_frames: u64,
}

pub fn continuity(runs: &[Run]) -> Continuity {
    let mut c = Continuity::default();
    for pair in runs.windows(2) {
        let (end, next) = (pair[0].end(), pair[1].first);
        if next > end {
            c.gaps += 1;
            c.gap_frames += next - end;
        } else if next < end {
            c.repeats += 1;
            c.repeat_frames += end - next;
        }
    }
    c
}

/// A latency (100 ns units) that `weight` frames had.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Weighted {
    pub value: i64,
    pub weight: u64,
}

/// `ticks` of a QPC of `frequency` in 100 ns units (rounded toward zero; signed, for differences).
pub fn ticks_to_hns(ticks: i64, frequency: u64) -> i64 {
    (i128::from(ticks) * i128::from(HNS_PER_SECOND) / i128::from(frequency.max(1))) as i64
}

/// A QPC reading in 100 ns units: the scale GetBuffer's device QPC position uses.
pub fn qpc_to_hns(qpc: u64, frequency: u64) -> i64 {
    (u128::from(qpc) * u128::from(HNS_PER_SECOND) / u128::from(frequency.max(1))) as i64
}

/// A device time `half_frames` half frames after a packet's first frame, at 48 kHz, in 100 ns units rounded to nearest (half
/// frames, so that a group of frames can be stamped at its centre).
fn half_frames_to_hns(half_frames: u64) -> i64 {
    let den = 2 * u64::from(SAMPLE_RATE);
    ((u128::from(half_frames) * u128::from(HNS_PER_SECOND) + u128::from(den / 2)) / u128::from(den)) as i64
}

/// Frames per device sample for `frames` frames to time: 1 (each frame its own sample) up to [`DEVICE_SAMPLES_MAX`] frames, else
/// the smallest power of two that keeps the samples within it.
pub fn device_group(frames: u64) -> u32 {
    frames.div_ceil(DEVICE_SAMPLES_MAX).max(1).next_power_of_two() as u32
}

/// The latencies of a run, per receipt time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Latencies {
    /// Host receipt (the QPC when GetBuffer returned, the whole packet's) minus publication, a sample per (packet, block) pair.
    pub host: Vec<Weighted>,
    /// The frame's device time (GetBuffer's device QPC position, its packet's first frame's, plus the frame's place in the
    /// packet at 48 kHz) minus publication, for the receipts whose position is valid: a sample per `device_group` frames (or
    /// fewer, at the end of a (packet, block) pair), stamped at its frames' centre.
    pub device: Vec<Weighted>,
    /// Frames per device sample: 1 where each frame was timed alone ([`device_group`]). A group's frames are each within
    /// (`device_group` - 1) / 2 frames of its stamp, so each percentile is within that of the frame-by-frame one, and the
    /// mean within rounding.
    pub device_group: u32,
    /// Receipts at or after the window's start, and the signal frames they carried that were measured.
    pub packets: u64,
    pub frames: u64,
    /// Frames of the first `skip_blocks` blocks (the pre-fill written before ATTACH), not measured.
    pub skipped: u64,
    /// Frames no block holds (the feed never published them): a frame from nowhere.
    pub unpublished: u64,
}

/// Every received frame's latency, for both receipt times. `receipts` and `runs` are the capture thread's logs; `blocks` the
/// feed's (in order: each block starts where the last ended); `qpc_frequency` the QPC's. Receipts whose host time is before
/// `from_qpc` (the warm-up) aren't measured, nor are frames of the first `skip_blocks` blocks. The device's frames are timed
/// one by one, or in groups where the receipts hold more than [`DEVICE_SAMPLES_MAX`] frames ([`device_group`]).
pub fn latencies(blocks: &[Block], receipts: &[Receipt], runs: &[Run], qpc_frequency: u64, from_qpc: u64, skip_blocks: usize) -> Latencies {
    let frames = receipts.iter().filter(|r| r.host_qpc >= from_qpc).map(|r| u64::from(r.frames)).sum();
    latencies_grouped(blocks, receipts, runs, qpc_frequency, from_qpc, skip_blocks, device_group(frames))
}

/// [`latencies`], with the device's frames timed in groups of `group` (at least 1).
pub fn latencies_grouped(
    blocks: &[Block], receipts: &[Receipt], runs: &[Run], qpc_frequency: u64, from_qpc: u64, skip_blocks: usize, group: u32,
) -> Latencies {
    let group = group.max(1);
    let mut l = Latencies { device_group: group, ..Default::default() };
    let group = u64::from(group);
    for r in receipts.iter().filter(|r| r.host_qpc >= from_qpc) {
        l.packets += 1;
        let host_hns = qpc_to_hns(r.host_qpc, qpc_frequency);
        let first = (r.first_run as usize).min(runs.len());
        let last = (first + r.runs as usize).min(runs.len());
        for run in &runs[first..last] {
            let (mut n, end) = (run.first, run.end());
            // The first block that ends after n.
            let mut i = blocks.partition_point(|b| b.end <= n);
            while n < end {
                let Some(b) = blocks.get(i).filter(|b| b.start <= n) else {
                    // Frames before the next block (or past the last): never published.
                    let next = blocks.get(i).map_or(end, |b| b.start.min(end));
                    l.unpublished += next - n;
                    n = next;
                    continue;
                };
                let shared = b.end.min(end) - n;
                if i < skip_blocks {
                    l.skipped += shared;
                } else {
                    let published = qpc_to_hns(b.qpc, qpc_frequency);
                    l.host.push(Weighted { value: host_hns - published, weight: shared });
                    if let Some(device_hns) = r.device_hns {
                        // Frame n's place in its packet: the run's first frame's, plus n's place in the run.
                        let (mut offset, mut left) = (u64::from(run.offset) + (n - run.first), shared);
                        while left > 0 {
                            let k = left.min(group);
                            let at = device_hns as i64 + half_frames_to_hns(2 * offset + k - 1);
                            l.device.push(Weighted { value: at - published, weight: k });
                            (offset, left) = (offset + k, left - k);
                        }
                    }
                    l.frames += shared;
                }
                n += shared;
                i += 1;
            }
        }
    }
    l
}

/// The nearest-rank percentile of weighted samples sorted by value: the smallest value whose cumulative weight reaches
/// `per_mille`/1000 of `total` (the samples' summed weight). `per_mille` 1 to 1000: 500 is the median, 1000 the maximum.
/// `None` for no samples.
pub fn percentile(sorted: &[Weighted], total: u64, per_mille: u32) -> Option<i64> {
    if total == 0 {
        return None;
    }
    let per_mille = u128::from(per_mille.clamp(1, 1000));
    let rank = ((u128::from(total) * per_mille).div_ceil(1000)).max(1);
    let mut seen = 0u128;
    for s in sorted {
        seen += u128::from(s.weight);
        if seen >= rank {
            return Some(s.value);
        }
    }
    sorted.last().map(|s| s.value)
}

/// A distribution's summary (the values' unit is the samples').
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub min: i64,
    pub p50: i64,
    pub p90: i64,
    pub p99: i64,
    pub max: i64,
    /// The weighted mean, rounded toward zero.
    pub mean: i64,
}

/// Sorts `samples` by value and summarises them; `None` when they weigh nothing.
pub fn summarize(samples: &mut [Weighted]) -> Option<Summary> {
    samples.sort_unstable_by_key(|s| s.value);
    let total: u64 = samples.iter().map(|s| s.weight).sum();
    let sum: i128 = samples.iter().map(|s| i128::from(s.value) * i128::from(s.weight)).sum();
    let at = |per_mille| percentile(samples, total, per_mille);
    Some(Summary {
        count: total,
        min: samples.iter().find(|s| s.weight > 0)?.value,
        p50: at(500)?,
        p90: at(900)?,
        p99: at(990)?,
        max: at(1000)?,
        mean: (sum / i128::from(total)) as i64,
    })
}

/// Packets to log room for: `seconds` at 48 kHz in packets of at least `min_packet` frames, with a second's margin. Used to
/// size the logs before the run.
pub fn packets_for(seconds: u64, min_packet: u32) -> usize {
    let rate = u64::from(SAMPLE_RATE);
    ((seconds + 1) * rate).div_ceil(u64::from(min_packet.max(1))) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::frame;

    const fn run(first: u64, len: u32, offset: u32) -> Run {
        Run { first, len, offset }
    }

    #[test]
    fn a_log_never_grows() {
        let mut log = Log::with_capacity(3);
        let capacity = log.items.capacity();
        for i in 0..capacity + 5 {
            log.push(i);
        }
        assert_eq!((log.len(), log.dropped()), (capacity, 5));
        assert_eq!(log.items.capacity(), capacity, "no reallocation");
        assert_eq!(log.items()[..3], [0, 1, 2]);
    }

    #[test]
    fn codes_unwrap_to_the_latest_frame_written() {
        let m = CODE_MODULUS;
        assert_eq!(unwrap_code(0, 1), Some(0));
        assert_eq!(unwrap_code(5, 100), Some(5));
        assert_eq!(unwrap_code(99, 100), Some(99));
        // Not written yet: none below 100 carries code 100 (frame 100 is the next).
        assert_eq!(unwrap_code(100, 100), None);
        assert_eq!(unwrap_code(0, 0), None);
        // Past the code's wrap: frame m + 3 carries code 3, and so did frame 3, which is older.
        assert_eq!(unwrap_code(3, m + 10), Some(m + 3));
        assert_eq!(unwrap_code((m - 2) as u32, m + 10), Some(m - 2));
        // Every frame a ring could still hold unwraps to itself.
        for n in [0, 1, 1000, m - 1, m, m + 1, 5 * m + 12_345, u64::MAX / 2] {
            for lag in [1, 2, 480, 1 << 18] {
                if let Some(written) = n.checked_add(lag) {
                    assert_eq!(unwrap_code(signal::code(n), written), Some(n), "frame {n}, written {written}");
                }
            }
        }
    }

    fn runs_of(frames: Vec<[f32; 2]>, written: u64) -> (Vec<Run>, FrameCounts) {
        let mut runs = Vec::new();
        let counts = split_runs(frames, written, |r| runs.push(r));
        (runs, counts)
    }

    #[test]
    fn a_packet_splits_into_runs() {
        let silence = [0.0f32; 2];
        // Frames 100..110, three silent frames (an underrun), then 110..115: two runs, nothing lost.
        let mut frames: Vec<[f32; 2]> = (100..110).map(frame).collect();
        frames.extend([silence; 3]);
        frames.extend((110..115).map(frame));
        let (runs, counts) = runs_of(frames, 2000);
        assert_eq!(runs, [run(100, 10, 0), run(110, 5, 13)], "the second run starts 13 frames into the packet");
        assert_eq!(counts, FrameCounts { signal: 15, silent: 3, corrupt: 0, unmatched: 0 });
        assert_eq!(continuity(&runs), Continuity::default());

        // A jump inside a packet starts a new run; a corrupt frame ends one.
        let mut frames: Vec<[f32; 2]> = (0..4).map(frame).collect();
        frames.extend((10..12).map(frame));
        frames.push([0.25, 0.25]);
        frames.extend((12..14).map(frame));
        let (runs, counts) = runs_of(frames, 100);
        assert_eq!(runs, [run(0, 4, 0), run(10, 2, 4), run(12, 2, 7)]);
        assert_eq!((counts.signal, counts.corrupt), (8, 1));
        assert_eq!(continuity(&runs), Continuity { gaps: 1, gap_frames: 6, ..Default::default() });

        // Across the code's wrap the run goes on (frame numbers, not codes).
        let m = CODE_MODULUS;
        let (runs, _) = runs_of((m - 2..m + 2).map(frame).collect(), m + 100);
        assert_eq!(runs, [run(m - 2, 4, 0)]);

        // A code not written yet is unmatched, and starts no run.
        let (runs, counts) = runs_of(vec![frame(500)], 100);
        assert_eq!((runs.len(), counts.unmatched, counts.signal), (0, 1, 1));
    }

    #[test]
    fn repeats_are_seen() {
        let runs = [run(0, 10, 0), run(5, 10, 0), run(15, 1, 0), run(20, 1, 0)];
        assert_eq!(continuity(&runs), Continuity { gaps: 1, gap_frames: 4, repeats: 1, repeat_frames: 5 });
        assert_eq!(continuity(&[]), Continuity::default());
    }

    #[test]
    fn qpc_conversions() {
        // A 10 MHz QPC is already in 100 ns units.
        assert_eq!(qpc_to_hns(123_456_789, 10_000_000), 123_456_789);
        assert_eq!(ticks_to_hns(-5_000, 10_000_000), -5_000);
        // 3 MHz: 3 ticks a microsecond.
        assert_eq!(qpc_to_hns(3_000_000, 3_000_000), 10_000_000);
        assert_eq!(ticks_to_hns(30, 3_000_000), 100);
        assert_eq!(ticks_to_hns(-30, 3_000_000), -100);
        // No overflow at a large count (a year at 10 MHz is 3.2e14 ticks; u64::MAX ticks at 24 MHz).
        assert_eq!(qpc_to_hns(u64::MAX, 24_000_000), (u128::from(u64::MAX) * 10 / 24) as i64);
    }

    /// The weighted percentiles, against the same values spelled out frame by frame.
    #[test]
    fn percentiles_are_nearest_rank() {
        let samples = [Weighted { value: 30, weight: 1 }, Weighted { value: 10, weight: 5 }, Weighted { value: 20, weight: 4 }];
        let mut sorted = samples;
        sorted.sort_unstable_by_key(|s| s.value);
        // Spelled out: 10 x5, 20 x4, 30 x1 (10 frames). Rank ceil(p * 10).
        assert_eq!(percentile(&sorted, 10, 500), Some(10));
        assert_eq!(percentile(&sorted, 10, 501), Some(20));
        assert_eq!(percentile(&sorted, 10, 900), Some(20));
        assert_eq!(percentile(&sorted, 10, 901), Some(30));
        assert_eq!(percentile(&sorted, 10, 1000), Some(30));
        assert_eq!(percentile(&sorted, 10, 1), Some(10));
        assert_eq!(percentile(&sorted, 0, 500), None);
        let mut s = samples;
        let sum = summarize(&mut s).unwrap();
        assert_eq!(sum, Summary { count: 10, min: 10, p50: 10, p90: 20, p99: 30, max: 30, mean: (50 + 80 + 30) / 10 });
        assert_eq!(summarize(&mut []), None);
        assert_eq!(summarize(&mut [Weighted { value: 7, weight: 0 }]), None);

        // Against a plain sorted list of 1000 values 0..999: p50 is the 500th (499), p99 the 990th (989).
        let mut each: Vec<Weighted> = (0..1000).rev().map(|v| Weighted { value: v, weight: 1 }).collect();
        let s = summarize(&mut each).unwrap();
        assert_eq!((s.min, s.p50, s.p90, s.p99, s.max), (0, 499, 899, 989, 999));
        // Negative values sort first.
        let mut mixed = [Weighted { value: -40, weight: 2 }, Weighted { value: 15, weight: 2 }];
        let s = summarize(&mut mixed).unwrap();
        assert_eq!((s.min, s.p50, s.max, s.mean), (-40, -40, 15, -12));
    }

    /// Every measured frame's device latency, one by one, in floating point: the reference [`latencies`] is checked against.
    /// The QPC is 10 MHz (ticks are 100 ns units).
    fn device_one_by_one(blocks: &[Block], receipts: &[Receipt], runs: &[Run], skip_blocks: usize) -> Vec<i64> {
        let mut v = Vec::new();
        for r in receipts {
            let Some(device) = r.device_hns else { continue };
            for run in &runs[r.first_run as usize..][..r.runs as usize] {
                for i in 0..u64::from(run.len) {
                    let n = run.first + i;
                    let Some(b) = blocks.iter().position(|b| (b.start..b.end).contains(&n)) else { continue };
                    if b >= skip_blocks {
                        // Frame n is i frames after the run's first, which is run.offset frames into the packet.
                        let due = device as f64 + (u64::from(run.offset) + i) as f64 * 1e7 / 48_000.0;
                        v.push(due.round() as i64 - blocks[b].qpc as i64);
                    }
                }
            }
        }
        v.sort_unstable();
        v
    }

    /// Weighted samples spelled out one value per frame, sorted.
    fn spelled_out(samples: &[Weighted]) -> Vec<i64> {
        let mut v: Vec<i64> = samples.iter().flat_map(|s| std::iter::repeat_n(s.value, s.weight as usize)).collect();
        v.sort_unstable();
        v
    }

    /// Four blocks and three packets on a 10 MHz QPC: every frame's host latency is its packet's receipt time minus its block's
    /// publication; its device latency is its own device time (the packet's first frame's, plus its place in the packet) minus
    /// that publication. Frames are weighted; the pre-fill and the warm-up aren't measured.
    #[test]
    fn latencies_pair_frames_with_their_blocks() {
        let blocks = [
            // The pre-fill: frames 0..480, published at t=0 (before ATTACH).
            Block { qpc: 0, start: 0, end: 480, read_frame: 0 },
            Block { qpc: 100_000, start: 480, end: 960, read_frame: 480 },
            Block { qpc: 200_000, start: 960, end: 1200, read_frame: 720 },
            Block { qpc: 210_000, start: 1200, end: 1440, read_frame: 960 },
        ];
        let runs = [
            run(0, 480, 0),
            // The second packet starts with 3 frames of silence (an underrun), so its run starts 3 frames in.
            run(480, 477, 3),
            // The third packet: 957..1437, which spans the last three blocks.
            run(957, 480, 0),
        ];
        let receipts = [
            Receipt { host_qpc: 50_000, device_hns: Some(40_000), frames: 480, first_run: 0, runs: 1 },
            Receipt { host_qpc: 190_000, device_hns: Some(90_000), frames: 480, first_run: 1, runs: 1 },
            Receipt { host_qpc: 300_000, device_hns: Some(190_000), frames: 480, first_run: 2, runs: 1 },
        ];
        let l = latencies(&blocks, &receipts, &runs, 10_000_000, 0, 1);
        assert_eq!((l.packets, l.frames, l.skipped, l.unpublished, l.device_group), (3, 957, 480, 0, 1));
        let host = [(90_000, 477), (200_000, 3), (100_000, 240), (90_000, 237)];
        assert_eq!(l.host, host.map(|(value, weight)| Weighted { value, weight }));
        // Each frame alone: frame 480, 3 frames into its packet, was due 625 (3 x 208.33) after the packet's first frame; frame
        // 956, 479 frames in, 99,792 after it; the last frame, 1436, 479 frames into the third packet, likewise.
        assert_eq!(
            (l.device.len(), l.device[0], l.device[476]),
            (957, Weighted { value: -9_375, weight: 1 }, Weighted { value: -10_000 + 99_792, weight: 1 })
        );
        assert_eq!(l.device.last(), Some(&Weighted { value: 190_000 + 99_792 - 210_000, weight: 1 }));
        assert_eq!(spelled_out(&l.device), device_one_by_one(&blocks, &receipts, &runs, 1));
        let mut h = l.host.clone();
        let s = summarize(&mut h).unwrap();
        assert_eq!((s.count, s.p50, s.p90, s.max), (957, 90_000, 100_000, 200_000));

        // In groups as big as the pairs: one sample per (packet, block) pair, at its frames' centre: 241 frames in (3 + 238)
        // for the second packet's; 1, 122.5 and 361 frames in for the third's.
        let g = latencies_grouped(&blocks, &receipts, &runs, 10_000_000, 0, 1, 480);
        let device = [(40_208, 477), (90_208, 3), (15_521, 240), (55_208, 237)];
        assert_eq!((g.device_group, &g.host), (480, &l.host));
        assert_eq!(g.device, device.map(|(value, weight)| Weighted { value, weight }));

        // The warm-up: receipts before t=200,000 ticks aren't measured.
        let l = latencies(&blocks, &receipts, &runs, 10_000_000, 200_000, 1);
        assert_eq!((l.packets, l.frames, l.skipped), (1, 480, 0));
        // A 2 MHz QPC: the same ticks are five times the time.
        let l = latencies(&blocks, &receipts[1..2], &runs, 2_000_000, 0, 1);
        assert_eq!(l.host, [Weighted { value: 450_000, weight: 477 }]);
    }

    /// Grouped device samples against the frames one by one: every percentile within half a group, the mean within rounding.
    #[test]
    fn device_groups_stay_within_half_a_group() {
        // 40 packets of 480 frames, a block each, the device stamping each packet a varying time from its block's publication;
        // every fifth packet's run starts after 7 frames of silence.
        let mut blocks = vec![];
        let (mut receipts, mut runs) = (vec![], vec![]);
        let mut next = 0;
        for i in 0..40u64 {
            let offset = if i % 5 == 0 { 7 } else { 0 };
            let len = 480 - offset;
            blocks.push(Block { qpc: 1_000_000 * i, start: next, end: next + u64::from(len), read_frame: 0 });
            runs.push(run(next, len, offset));
            let device = 1_000_000 * i + 37_000 * (i % 7);
            receipts.push(Receipt { host_qpc: device + 200_000, device_hns: Some(device), frames: 480, first_run: i as u32, runs: 1 });
            next += u64::from(len);
        }
        let exact = device_one_by_one(&blocks, &receipts, &runs, 0);
        let mut one = latencies_grouped(&blocks, &receipts, &runs, 10_000_000, 0, 0, 1).device;
        assert_eq!(spelled_out(&one), exact);
        let one = summarize(&mut one).unwrap();
        for group in [2, 16, 64] {
            let mut g = latencies_grouped(&blocks, &receipts, &runs, 10_000_000, 0, 0, group).device;
            assert!(g.len() <= exact.len().div_ceil(group as usize) + receipts.len(), "group {group}: {} samples", g.len());
            let s = summarize(&mut g).unwrap();
            // Half a group, in 100 ns units, and a unit for rounding.
            let bound = (i64::from(group) - 1) * 10_000_000 / 96_000 + 1;
            for (a, b) in [(s.min, one.min), (s.p50, one.p50), (s.p90, one.p90), (s.p99, one.p99), (s.max, one.max)] {
                assert!((a - b).abs() <= bound, "group {group}: {a} against {b} (bound {bound})");
            }
            assert!((s.mean - one.mean).abs() <= 1, "group {group}: mean {} against {}", s.mean, one.mean);
            assert_eq!(s.count, one.count);
        }
    }

    #[test]
    fn frames_no_block_holds_are_counted() {
        let blocks = [Block { qpc: 0, start: 0, end: 100, read_frame: 0 }, Block { qpc: 10, start: 200, end: 300, read_frame: 100 }];
        let runs = [run(50, 300, 0)];
        let receipts = [Receipt { host_qpc: 1_000, device_hns: None, frames: 300, first_run: 0, runs: 1 }];
        let l = latencies(&blocks, &receipts, &runs, 10_000_000, 0, 0);
        // 50..100 in block 0, 100..200 in none, 200..300 in block 1, 300..350 past the last.
        assert_eq!((l.frames, l.unpublished), (150, 150));
        assert_eq!(l.host, [Weighted { value: 1_000, weight: 50 }, Weighted { value: 990, weight: 100 }]);
        // A packet whose device position GetBuffer flagged as an error counts for the host's time only.
        assert!(l.device.is_empty());
        // A receipt whose runs run past the log is clamped, not a panic.
        let bad = [Receipt { first_run: 1, runs: 5, ..receipts[0] }];
        assert_eq!(latencies(&blocks, &bad, &runs, 10_000_000, 0, 0).frames, 0);
    }

    #[test]
    fn device_groups_keep_the_samples_bounded() {
        assert_eq!((device_group(0), device_group(1), device_group(DEVICE_SAMPLES_MAX)), (1, 1, 1));
        assert_eq!((device_group(DEVICE_SAMPLES_MAX + 1), device_group(4 * DEVICE_SAMPLES_MAX)), (2, 4));
        // latency's default 10 s, frame by frame; its longest, 600 s, in groups of 16 (0.16 ms either side of a group's stamp).
        assert_eq!((device_group(10 * 48_000), device_group(600 * 48_000)), (1, 16));
        let hns = [0, 1, 2, 96_000].map(half_frames_to_hns);
        assert_eq!(hns, [0, 104, 208, 10_000_000], "half a frame is 104.17, a frame 208.33");
    }

    #[test]
    fn fill_and_sizes() {
        assert_eq!(Block { qpc: 0, start: 1000, end: 1480, read_frame: 520 }.fill_before(), 480);
        assert_eq!(Block { qpc: 0, start: 10, end: 20, read_frame: 30 }.fill_before(), 0);
        // 10 s of 128-frame packets with a second's margin: 528,000 frames.
        assert_eq!(packets_for(10, 128), 4_125);
        assert_eq!(packets_for(10, 100), 5_280);
        assert_eq!(packets_for(10, 480), 1_100);
    }
}
