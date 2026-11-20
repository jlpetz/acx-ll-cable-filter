//! The arithmetic of nf-vmic-test's `latency`: publication-to-receipt latency through the ring and the capture endpoint.
//!
//! One process writes the test signal into an attached ring and records the capture endpoint. The feed logs a [`Block`] for every
//! write it publishes: the frames, and the QPC just after it stored `write_frame`. The capture thread logs a [`Receipt`] for
//! every packet: the QPC when GetBuffer returned (the host's receipt time) and GetBuffer's device QPC position (the time the
//! endpoint gives for the packet's first frame, in 100 ns units), with the packet's signal frames as [`Run`]s of consecutive
//! frame numbers ([`split_runs`] decodes each frame's code back to its frame number, [`unwrap_code`]).
//!
//! Both receipt times are the packet's: a frame's latency is its packet's receipt time minus the publication time of the block
//! that held it, and every frame counts once ([`latencies`] weights each (packet, block) pair by the frames they share). The
//! percentiles are nearest-rank over those frames ([`percentile`]). The device time can come out below the publication time:
//! a driver may stamp a packet with the time its first frame was due rather than the time it filled the packet.
//!
//! Everything the threads do per block or per packet here ([`Log::push`], [`split_runs`]) allocates nothing: the logs are
//! sized before the run starts. The analysis runs after the threads have stopped.

use crate::signal::{self, CODE_MODULUS};

/// 100 ns units per second.
pub const HNS_PER_SECOND: u64 = 10_000_000;

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

/// Frames `[first, first + len)` of the signal, consecutive in one packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub first: u64,
    pub len: u32,
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
/// ([`unwrap_code`] against `written`), and hands each run to `emit` in order. Silence and corrupt frames end a run. Allocates
/// nothing.
pub fn split_runs(frames: impl IntoIterator<Item = [f32; 2]>, written: u64, mut emit: impl FnMut(Run)) -> FrameCounts {
    let mut counts = FrameCounts::default();
    let mut run: Option<Run> = None;
    for f in frames {
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
            Some(n) => run = Some(Run { first: n, len: 1 }),
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

/// The latencies of a run, per receipt time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Latencies {
    /// Host receipt (the QPC when GetBuffer returned) minus publication.
    pub host: Vec<Weighted>,
    /// Device receipt (GetBuffer's device QPC position) minus publication, for the receipts whose position is valid.
    pub device: Vec<Weighted>,
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
/// `from_qpc` (the warm-up) aren't measured, nor are frames of the first `skip_blocks` blocks.
pub fn latencies(blocks: &[Block], receipts: &[Receipt], runs: &[Run], qpc_frequency: u64, from_qpc: u64, skip_blocks: usize) -> Latencies {
    let mut l = Latencies::default();
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
                        l.device.push(Weighted { value: device_hns as i64 - published, weight: shared });
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
    let rate = u64::from(crate::protocol::SAMPLE_RATE);
    ((seconds + 1) * rate).div_ceil(u64::from(min_packet.max(1))) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::frame;

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
        assert_eq!(runs, [Run { first: 100, len: 10 }, Run { first: 110, len: 5 }]);
        assert_eq!(counts, FrameCounts { signal: 15, silent: 3, corrupt: 0, unmatched: 0 });
        assert_eq!(continuity(&runs), Continuity::default());

        // A jump inside a packet starts a new run; a corrupt frame ends one.
        let mut frames: Vec<[f32; 2]> = (0..4).map(frame).collect();
        frames.extend((10..12).map(frame));
        frames.push([0.25, 0.25]);
        frames.extend((12..14).map(frame));
        let (runs, counts) = runs_of(frames, 100);
        assert_eq!(runs, [Run { first: 0, len: 4 }, Run { first: 10, len: 2 }, Run { first: 12, len: 2 }]);
        assert_eq!((counts.signal, counts.corrupt), (8, 1));
        assert_eq!(continuity(&runs), Continuity { gaps: 1, gap_frames: 6, ..Default::default() });

        // Across the code's wrap the run goes on (frame numbers, not codes).
        let m = CODE_MODULUS;
        let (runs, _) = runs_of((m - 2..m + 2).map(frame).collect(), m + 100);
        assert_eq!(runs, [Run { first: m - 2, len: 4 }]);

        // A code not written yet is unmatched, and starts no run.
        let (runs, counts) = runs_of(vec![frame(500)], 100);
        assert_eq!((runs.len(), counts.unmatched, counts.signal), (0, 1, 1));
    }

    #[test]
    fn repeats_are_seen() {
        let runs = [Run { first: 0, len: 10 }, Run { first: 5, len: 10 }, Run { first: 15, len: 1 }, Run { first: 20, len: 1 }];
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

    /// Two blocks and two packets on a 10 MHz QPC: every frame's latency is its packet's receipt time minus its block's
    /// publication, frames weighted; the pre-fill and the warm-up aren't measured.
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
            Run { first: 0, len: 480 },
            // The second packet: 960..1440, which spans the last two blocks.
            Run { first: 480, len: 480 },
            Run { first: 960, len: 480 },
        ];
        let receipts = [
            Receipt { host_qpc: 50_000, device_hns: Some(40_000), frames: 480, first_run: 0, runs: 1 },
            Receipt { host_qpc: 190_000, device_hns: Some(90_000), frames: 480, first_run: 1, runs: 1 },
            Receipt { host_qpc: 300_000, device_hns: Some(190_000), frames: 480, first_run: 2, runs: 1 },
        ];
        let l = latencies(&blocks, &receipts, &runs, 10_000_000, 0, 1);
        assert_eq!((l.packets, l.frames, l.skipped, l.unpublished), (3, 960, 480, 0));
        assert_eq!(l.host, [Weighted { value: 90_000, weight: 480 }, Weighted { value: 100_000, weight: 240 }, Weighted {
            value: 90_000,
            weight: 240
        },]);
        assert_eq!(l.device, [Weighted { value: -10_000, weight: 480 }, Weighted { value: -10_000, weight: 240 }, Weighted {
            value: -20_000,
            weight: 240
        },]);
        let mut host = l.host.clone();
        let s = summarize(&mut host).unwrap();
        assert_eq!((s.count, s.p50, s.p90, s.max), (960, 90_000, 100_000, 100_000));

        // The warm-up: receipts before t=200,000 ticks aren't measured.
        let l = latencies(&blocks, &receipts, &runs, 10_000_000, 200_000, 1);
        assert_eq!((l.packets, l.frames, l.skipped), (1, 480, 0));
        // A 2 MHz QPC: the same ticks are five times the time.
        let l = latencies(&blocks, &receipts[1..2], &runs, 2_000_000, 0, 1);
        assert_eq!(l.host, [Weighted { value: 450_000, weight: 480 }]);
    }

    #[test]
    fn frames_no_block_holds_are_counted() {
        let blocks = [Block { qpc: 0, start: 0, end: 100, read_frame: 0 }, Block { qpc: 10, start: 200, end: 300, read_frame: 100 }];
        let runs = [Run { first: 50, len: 300 }];
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
    fn fill_and_sizes() {
        assert_eq!(Block { qpc: 0, start: 1000, end: 1480, read_frame: 520 }.fill_before(), 480);
        assert_eq!(Block { qpc: 0, start: 10, end: 20, read_frame: 30 }.fill_before(), 0);
        // 10 s of 128-frame packets with a second's margin: 528,000 frames.
        assert_eq!(packets_for(10, 128), 4_125);
        assert_eq!(packets_for(10, 100), 5_280);
        assert_eq!(packets_for(10, 480), 1_100);
    }
}
