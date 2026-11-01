//! WASAPI exclusive mode, apart from WASAPI (engine.md, "Exclusive mode"; TODO 13): the period and format choices, the
//! fall-back to shared mode, and the event-driven output's ring ([`EventRender`]), so tests drive them with fakes.
//!
//! **The output is event-driven** ([`EventRender`]): WASAPI gives an event-driven exclusive render stream two buffers of
//! one period each, and at each of the device's events the client must hand over a whole period while the device plays
//! the other ("ping-pong"). The pipeline's output model needs a buffer it can fill to a level at its own blocks, so the
//! sink keeps a ring (100 ms) that the pipeline writes into exactly as it writes into a shared stream's buffer, and each
//! render event moves one period from the ring to the device. To the pipeline that is a device that takes a period at
//! each pass, as the shared-mode engine does, only with a 2 ms period instead of 10: the padding is the ring, the fill,
//! `FillOffset` (pipeline.rs) and the drift loop see the same sawtooth one period wide, and the deficit and drop
//! rules hold as they are. The device's two buffers (one to two periods ahead of its clock) are the render side.
//!
//! Polling with a buffer of a few periods would keep the one-event loop, but VB-CABLE (the probe of 2026-11-01) was only
//! seen to take event-driven exclusive streams, a polled exclusive buffer that runs dry replays stale audio from its
//! ring (there's no engine to play silence), and how its padding moves is the driver's affair. Event-driven, a short
//! ring is our own business: [`EventRender`] hands what it has and silence after it, and counts the silence so the
//! pipeline's stream indices (and the readout) stay right.
//!
//! The cost is a second deadline on the audio thread: a render event must be answered within a period. The thread
//! waits on both events and answers the render event first, again between the blocks of a wake (a block takes
//! 0.1-0.3 ms of the 2 ms), so its longest answer is one block's work.

use nf_core::{BLOCK, SAMPLE_RATE};

use crate::errors::{StreamRole, Warning, hr};
use crate::fifo::Fifo;
use crate::pipeline::{ClockReading, Hr, RenderSink};
use crate::timing::HNS_PER_S;

/// The smallest exclusive period the engine asks for, in frames at 48 kHz: 2 ms. The audio thread answers the render
/// event between blocks, so its longest answer is a block's work (0.1-0.3 ms, more under load); a period under 2 ms
/// leaves too little of it on a busy machine, and VB-CABLE's render side goes no lower anyway.
pub(crate) const MIN_PERIOD_FRAMES: usize = 96;

/// The event-driven output's ring, in frames (100 ms, as the shared output's buffer): the pipeline's fill lives here.
pub(crate) const RING_FRAMES: usize = 4_800;

/// The formats tried in exclusive mode, best first, at 48 kHz and the device's channel count: 32-bit float, then 24 bits
/// in a 32-bit container, then 16-bit (all VB-CABLE takes there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExclusiveFormat {
    F32,
    I24In32,
    I16,
}

impl ExclusiveFormat {
    pub(crate) const ALL: [Self; 3] = [Self::F32, Self::I24In32, Self::I16];

    /// Container bits, valid bits, and whether it's float.
    pub(crate) fn bits(self) -> (usize, usize, bool) {
        match self {
            Self::F32 => (32, 32, true),
            Self::I24In32 => (32, 24, false),
            Self::I16 => (16, 16, false),
        }
    }

    /// As the status and the reports name it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::F32 => "32-bit float",
            Self::I24In32 => "24-bit",
            Self::I16 => "16-bit",
        }
    }
}

/// Why a stream that was asked for in exclusive mode runs in shared mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExclusiveRefusal {
    /// The device takes none of [`ExclusiveFormat::ALL`] at 48 kHz.
    NoFormat,
    /// WASAPI refused it with this HRESULT: the device is in use, exclusive control isn't allowed, the format...
    Refused(Hr),
    /// Every period tried ([`exclusive_periods`], from the shortest to the longest, 100 ns) was refused, the last with
    /// this HRESULT.
    Periods { hr: Hr, shortest_hns: i64, longest_hns: i64 },
}

/// What a refused exclusive initialise leaves to try ([`first_exclusive`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// The period (or the buffer it makes) was refused: the next period.
    Period,
    /// The format was: the next format.
    Format,
    /// Anything else (the device in use, exclusive control not allowed, gone): no other try would do better.
    Stop,
}

pub(crate) fn after_refusal(code: Hr) -> Retry {
    match code {
        hr::AUDCLNT_E_INVALID_DEVICE_PERIOD
        | hr::AUDCLNT_E_BUFFER_SIZE_ERROR
        | hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
        | hr::E_INVALIDARG => Retry::Period,
        hr::AUDCLNT_E_UNSUPPORTED_FORMAT => Retry::Format,
        _ => Retry::Stop,
    }
}

/// A period of `frames` at 48 kHz in 100 ns units, rounded as Microsoft's samples (and wasapi's `calculate_period_100ns`) do.
pub(crate) fn frames_to_hns(frames: usize) -> i64 {
    (frames as f64 * HNS_PER_S / SAMPLE_RATE as f64 + 0.5) as i64
}

/// The exclusive periods to try, in 100 ns units, best first: [`exclusive_period`]'s choice, then the device's minimum
/// (from [`MIN_PERIOD_FRAMES`]: the probe saw VB-CABLE take its 3 ms, not 3.33), then its default, each once.
pub(crate) fn exclusive_periods(default_hns: i64, min_hns: i64) -> Vec<i64> {
    let mut periods = vec![frames_to_hns(exclusive_period(default_hns, min_hns))];
    for p in [min_hns.max(frames_to_hns(MIN_PERIOD_FRAMES)), default_hns] {
        if p > 0 && !periods.contains(&p) {
            periods.push(p);
        }
    }
    periods
}

/// The first exclusive stream `init` opens, trying each format (best first) at each period (best first): a refused period
/// goes on to the next period, a refused format (one `IsFormatSupported` took and the initialise didn't) to the next
/// format, and any other refusal ends it. Each try is on a fresh client (`init`'s affair: a refused initialise leaves its
/// client unusable). Not real-time.
pub(crate) fn first_exclusive<F, T>(
    formats: &[F], periods: &[i64], mut init: impl FnMut(&F, i64) -> Result<T, Hr>,
) -> Result<T, ExclusiveRefusal> {
    if formats.is_empty() {
        return Err(ExclusiveRefusal::NoFormat);
    }
    let (mut period_refused, mut format_refused) = (None, None);
    'formats: for f in formats {
        for &p in periods {
            match init(f, p) {
                Ok(t) => return Ok(t),
                Err(code) => match after_refusal(code) {
                    Retry::Period => period_refused = Some(code),
                    Retry::Format => {
                        format_refused = Some(code);
                        continue 'formats;
                    }
                    Retry::Stop => return Err(ExclusiveRefusal::Refused(code)),
                },
            }
        }
    }
    let (shortest_hns, longest_hns) = (periods.iter().copied().min().unwrap_or(0), periods.iter().copied().max().unwrap_or(0));
    Err(match period_refused {
        Some(code) => ExclusiveRefusal::Periods { hr: code, shortest_hns, longest_hns },
        None => ExclusiveRefusal::Refused(format_refused.unwrap_or(hr::AUDCLNT_E_UNSUPPORTED_FORMAT)),
    })
}

/// The exclusive period, in frames at 48 kHz, for a device whose default and minimum periods (`GetDevicePeriod`, 100 ns)
/// are these: the smallest period from the minimum (and from [`MIN_PERIOD_FRAMES`]) that divides the block and isn't
/// past the default, else that lower bound itself. A period that tiles the block makes the mic's packets end where its
/// blocks end, so a block is complete when its last sample's packet is (144-frame packets, 3 ms, finish a block up to
/// 2 ms after its end, 1 ms on average; 160-frame ones, 3.33 ms, never); on the output it puts five events in each of
/// a 2 ms period's blocks, and divisors of 480 are aligned for most drivers' 128-byte rule (any refusal of that is
/// retried at the size the driver gives).
pub(crate) fn exclusive_period(default_hns: i64, min_hns: i64) -> usize {
    // Periods in 100 ns units are rounded (512 frames reads 106 667): within a hundredth of a frame is that frame.
    let frames = |hns: i64, round_up: bool| {
        let x = hns.max(0) as f64 * SAMPLE_RATE as f64 / HNS_PER_S;
        (if round_up && (x - x.round()).abs() > 0.01 { x.ceil() } else { x.round() }) as usize
    };
    let lo = frames(min_hns, true).max(MIN_PERIOD_FRAMES);
    let default = if default_hns > 0 { frames(default_hns, false) } else { BLOCK };
    (lo..=default.max(lo)).find(|&p| BLOCK.is_multiple_of(p)).unwrap_or(lo)
}

/// Opens a stream exclusive-first: `exclusive` when `want` (and it works), else `shared`. A refused exclusive open falls
/// back to shared mode with the warning that says why; a failed shared open is the start's failure, as without the option.
pub(crate) fn exclusive_or_shared<T, E>(
    want: bool, role: StreamRole, device: &str, exclusive: impl FnOnce() -> Result<T, ExclusiveRefusal>,
    shared: impl FnOnce() -> Result<T, E>,
) -> Result<(T, Option<Warning>), E> {
    if !want {
        return shared().map(|s| (s, None));
    }
    match exclusive() {
        Ok(s) => Ok((s, None)),
        Err(why) => shared().map(|s| (s, Some(Warning::ExclusiveUnavailable { role, device: device.to_owned(), why }))),
    }
}

/// The device end of an event-driven exclusive render stream: a buffer of one period, handed over whole.
pub(crate) trait PeriodDevice {
    /// Frames per buffer.
    fn period(&self) -> usize;
    /// Hands the device its next buffer (`frames.len()` is the period). After a stream event, or once before the start.
    fn hand(&mut self, frames: &[f32]) -> Result<(), Hr>;
    /// The device's clock: frames it has played (the silence [`EventRender`] put in included). `Ok(None)` while it isn't
    /// running yet.
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr>;
    /// The time now (QPC, hns): what [`EventRender`] times its answers by.
    fn now_hns(&mut self) -> f64;
}

/// An answer this many periods after the last (or after the start) may be late: the device clock decides.
const LATE_GAP_PERIODS: f64 = 1.5;
/// The most a late answer counts as missed, in frames (10 s): a clock position far past what was handed is garbage, and
/// the counts it would add mustn't overflow (the run stops after 2 s without the mic anyway).
const MAX_LATE_FRAMES: f64 = 480_000.0;

/// An event-driven exclusive render stream as the pipeline's [`RenderSink`]: a ring the pipeline fills to its target, and a
/// period moved from it to the device at each event ([`Self::service`]). Its padding is the ring, so it looks like a
/// device that takes a period a pass. When the ring is short of a period at an event, the device gets what there is and
/// silence after it: counted as an underrun, and the silence is taken off the clock's position, so the pipeline's stream
/// indices stay the frames it wrote (the readout and the fill stay right after a dry spell, which the shared path's
/// clock can't promise: TODO 13's detail, "Budget"). An event answered late, after the device has played everything it
/// was handed, is an underrun too: the device played whatever its other buffer held meanwhile (a stale period, or more),
/// and those periods are taken off the clock as the silence is, so the indices follow a device whose stream moves on by
/// them (one that wrote the late buffer over the one playing instead would read that much high: unverified on VB-CABLE).
/// It takes both the clock's position past what was handed and a gap of 1.5 periods since the last answer, so a clock
/// that leads its buffers can't make every event look late. The gap is timed on the sink's own clock (QPC) from the
/// stream's start ([`Self::started`]), so the first answer, which waits on the rest of the start (the mic's start, the
/// status), is caught late as any other is. A device clock that can't say yet (its position reads 0 for a while after
/// the start) leaves the check to the next answer it can: a device that hadn't started has played nothing past what was
/// handed by then, and one whose clock lagged shows the stale periods then.
///
/// Real-time: no allocation, no locks, no panic paths; O(period) per event.
pub(crate) struct EventRender<D> {
    dev: D,
    ring: Fifo,
    capacity: usize,
    /// One period, as handed.
    buf: Box<[f32]>,
    period: usize,
    /// Frames handed to the device, its pre-roll and the silence included: the device stream's index of the next one.
    handed: u64,
    /// Silence put in where the ring was short: frames of the device's stream the pipeline didn't write.
    inserted: u64,
    /// Short or late events since the pipeline last asked ([`RenderSink::take_underruns`]).
    underruns: u64,
    /// Late events, and events in all, over the run.
    late: u64,
    events: u64,
    /// When the stream started or the last event was answered (QPC, hns); `None` before the start.
    last_answer_hns: Option<f64>,
    /// [`LATE_GAP_PERIODS`] in hns.
    late_gap_hns: f64,
    /// A long gap the device's clock couldn't check yet: the next answer it can checks it.
    late_unchecked: bool,
}

impl<D: PeriodDevice> EventRender<D> {
    pub(crate) fn new(dev: D, capacity: usize) -> Self {
        let period = dev.period().max(1);
        Self {
            dev,
            ring: Fifo::new(capacity.max(period)),
            capacity: capacity.max(period),
            buf: vec![0.0; period].into_boxed_slice(),
            period,
            handed: 0,
            inserted: 0,
            underruns: 0,
            late: 0,
            events: 0,
            last_answer_hns: None,
            late_gap_hns: LATE_GAP_PERIODS * period as f64 * HNS_PER_S / SAMPLE_RATE as f64,
            late_unchecked: false,
        }
    }

    pub(crate) fn device(&self) -> &D {
        &self.dev
    }

    #[cfg(test)]
    pub(crate) fn device_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    /// The device's period, in frames.
    pub(crate) fn period(&self) -> usize {
        self.period
    }

    /// The first buffer, before the stream starts (WASAPI wants it filled first): the ring's first period, the
    /// pipeline's prefill.
    pub(crate) fn preroll(&mut self) -> Result<(), Hr> {
        self.hand_next()
    }

    /// The stream has just started: the first event's answer is timed from now.
    pub(crate) fn started(&mut self) {
        self.last_answer_hns = Some(self.dev.now_hns());
    }

    /// One device event: hands the device its next buffer. Late when it comes 1.5 periods after the last answer (or the
    /// start) and the device has played everything it was handed: the stale periods it played meanwhile are counted out
    /// of its stream.
    pub(crate) fn service(&mut self) -> Result<(), Hr> {
        self.events = self.events.saturating_add(1);
        let now = self.dev.now_hns();
        let gap = self.last_answer_hns.map_or(0.0, |t| now - t);
        self.last_answer_hns = Some(now);
        let long_gap = gap > self.late_gap_hns;
        if long_gap || self.late_unchecked {
            // A clock that can't say yet (it reads 0 for a while after the start, or erred: the hand-over reports a dead
            // stream) leaves the check to the next answer it can, whatever that one's gap: the stale periods are still
            // past what was handed then.
            self.late_unchecked = false;
            match self.dev.clock() {
                Ok(Some(c)) => self.count_stale(c),
                _ => self.late_unchecked = true,
            }
        }
        self.hand_next()
    }

    /// A late answer's stale periods, when the device's clock is past everything handed: the period playing now and any
    /// before it since the last one handed, none of them ours.
    fn count_stale(&mut self, c: ClockReading) {
        let period = self.period as f64;
        let past = c.position_now(period) - self.handed as f64;
        if past >= 0.0 {
            // `min` before the cast (NaN fails the test above), and saturating sums: no overflow on a garbage position.
            let periods = ((past.min(MAX_LATE_FRAMES) / period) as u64).saturating_add(1);
            let stale = periods.saturating_mul(self.period as u64);
            self.inserted = self.inserted.saturating_add(stale);
            self.handed = self.handed.saturating_add(stale);
            self.late = self.late.saturating_add(1);
            self.underruns = self.underruns.saturating_add(1);
        }
    }

    fn hand_next(&mut self) -> Result<(), Hr> {
        let n = self.ring.len().min(self.period);
        if let Some(head) = self.buf.get_mut(..n) {
            self.ring.pop_into(head);
        }
        if let Some(tail) = self.buf.get_mut(n..) {
            tail.fill(0.0);
        }
        self.dev.hand(&self.buf)?;
        self.handed = self.handed.saturating_add(self.period as u64);
        if n < self.period {
            self.inserted = self.inserted.saturating_add((self.period - n) as u64);
            self.underruns = self.underruns.saturating_add(1);
        }
        Ok(())
    }

    /// Silence put in where the ring ran short, and late events, over the run; and the events answered.
    #[cfg(test)]
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        (self.inserted, self.late, self.events)
    }
}

impl<D: PeriodDevice> RenderSink for EventRender<D> {
    fn buffer_frames(&self) -> usize {
        self.capacity
    }

    /// The ring: what the device hasn't taken yet.
    fn padding(&mut self) -> Result<usize, Hr> {
        Ok(self.ring.len())
    }

    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        self.ring.push(mono);
        Ok(())
    }

    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        self.ring.push_zeros(frames);
        Ok(())
    }

    /// The device's clock in the pipeline's frames: the silence put in is taken off its position.
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        let inserted = self.inserted as f64;
        Ok(self.dev.clock()?.map(|c| ClockReading { pos_frames: c.pos_frames - inserted, ..c }))
    }

    fn take_underruns(&mut self) -> u64 {
        std::mem::take(&mut self.underruns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::describe_warning;

    /// VB-CABLE's render side (the probe: 2 ms minimum, 10 ms default) gets 2 ms; its capture side's 3 ms becomes 3.33 ms,
    /// which tiles the block; HDAudio's 2.9 ms minimum gets 3.33 ms too; and a device whose minimum is past every divisor
    /// up to its default gets its minimum.
    #[test]
    fn the_period_is_the_smallest_that_tiles_the_block() {
        assert_eq!(exclusive_period(100_000, 20_000), 96, "VB-CABLE's render side: 2 ms");
        assert_eq!(exclusive_period(100_000, 30_000), 160, "VB-CABLE's capture side: 3 ms -> 3.33 ms");
        assert_eq!(exclusive_period(100_000, 29_025), 160);
        assert_eq!(exclusive_period(100_000, 10_000), 96, "never under 2 ms");
        assert_eq!(exclusive_period(100_000, 0), 96);
        assert_eq!(exclusive_period(100_000, 50_000), 240);
        assert_eq!(exclusive_period(100_000, 100_000), 480);
        assert_eq!(exclusive_period(106_667, 106_667), 512, "past the block: the minimum");
        assert_eq!(exclusive_period(0, 20_000), 96, "no default: up to a block");
        assert_eq!(exclusive_period(30_000, 25_000), 120, "the default caps the search");
        assert_eq!(exclusive_period(26_000, 26_000), 125, "nothing tiles below the default: the minimum");
    }

    /// The periods tried: the choice, the device's minimum and its default, each once and never under 2 ms. VB-CABLE's
    /// capture side: 3.33 ms, then the 3 ms the probe saw it take, then 10 ms; its render side: 2 ms, then 10.
    #[test]
    fn the_periods_tried_are_the_choice_the_minimum_and_the_default() {
        assert_eq!(exclusive_periods(100_000, 30_000), [33_333, 30_000, 100_000]);
        assert_eq!(exclusive_periods(100_000, 20_000), [20_000, 100_000]);
        assert_eq!(exclusive_periods(100_000, 10_000), [20_000, 100_000], "never under 2 ms");
        assert_eq!(exclusive_periods(100_000, 29_025), [33_333, 29_025, 100_000]);
        assert_eq!(exclusive_periods(0, 20_000), [20_000]);
        assert_eq!(frames_to_hns(160), 33_333);
        assert_eq!(frames_to_hns(144), 30_000);
    }

    /// Each format at each period: a refused period goes on to the next, a refused format to the next format, anything else
    /// stops; all periods refused says so with the periods tried, and no format at all is `NoFormat`.
    #[test]
    fn a_refused_period_or_format_tries_the_next() {
        let periods = [33_333, 30_000, 100_000];
        let mut tried = Vec::new();
        // The 3.33 ms period refused, the 3 ms one taken.
        let r = first_exclusive(&["16-bit"], &periods, |f, p| {
            tried.push((*f, p));
            if p == 33_333 { Err(hr::AUDCLNT_E_INVALID_DEVICE_PERIOD) } else { Ok(p) }
        });
        assert_eq!(r, Ok(30_000));
        assert_eq!(tried, [("16-bit", 33_333), ("16-bit", 30_000)]);
        // A format the initialise refuses: the next format, from the first period.
        let mut tried = Vec::new();
        let r = first_exclusive(&["f32", "16-bit"], &periods, |f, p| {
            tried.push((*f, p));
            if *f == "f32" { Err(hr::AUDCLNT_E_UNSUPPORTED_FORMAT) } else { Ok((*f, p)) }
        });
        assert_eq!(r, Ok(("16-bit", 33_333)));
        assert_eq!(tried, [("f32", 33_333), ("16-bit", 33_333)]);
        // Every period refused for every format: the periods tried, and the last refusal.
        let mut n = 0;
        let r = first_exclusive(&["f32", "16-bit"], &periods, |_, _| -> Result<(), Hr> {
            n += 1;
            Err(if n % 2 == 0 { hr::E_INVALIDARG } else { hr::AUDCLNT_E_BUFFER_SIZE_ERROR })
        });
        assert_eq!(n, 6);
        assert_eq!(r, Err(ExclusiveRefusal::Periods { hr: hr::E_INVALIDARG, shortest_hns: 30_000, longest_hns: 100_000 }));
        // In use: no other try.
        let mut n = 0;
        let r = first_exclusive(&["f32", "16-bit"], &periods, |_, _| -> Result<(), Hr> {
            n += 1;
            Err(hr::AUDCLNT_E_DEVICE_IN_USE)
        });
        assert_eq!((r, n), (Err(ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE)), 1));
        let r = first_exclusive(&["f32"], &periods, |_, _| -> Result<(), Hr> { Err(hr::AUDCLNT_E_UNSUPPORTED_FORMAT) });
        assert_eq!(r, Err(ExclusiveRefusal::Refused(hr::AUDCLNT_E_UNSUPPORTED_FORMAT)));
        let none: [&str; 0] = [];
        assert_eq!(first_exclusive(&none, &periods, |_, _| Ok(())), Err(ExclusiveRefusal::NoFormat));
    }

    #[test]
    fn formats_are_tried_best_first() {
        assert_eq!(ExclusiveFormat::ALL, [ExclusiveFormat::F32, ExclusiveFormat::I24In32, ExclusiveFormat::I16]);
        assert_eq!(ExclusiveFormat::I24In32.bits(), (32, 24, false));
        assert_eq!(ExclusiveFormat::I16.label(), "16-bit");
    }

    /// A refused exclusive open gives the shared stream and a warning that names the device and why; one that works, or
    /// isn't asked for, no warning; and a shared open that fails after a refusal fails the start, as it would without the
    /// option.
    #[test]
    fn a_refused_exclusive_open_falls_back_to_shared_with_a_warning() {
        let shared = || Ok::<_, Hr>("shared");
        let granted = || Ok::<_, ExclusiveRefusal>("exclusive");
        let in_use = || Err::<&str, _>(ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE));
        assert_eq!(exclusive_or_shared(false, StreamRole::Output, "CABLE Input", granted, shared), Ok(("shared", None)));
        assert_eq!(exclusive_or_shared(true, StreamRole::Output, "CABLE Input", granted, shared), Ok(("exclusive", None)));
        let (s, w) = exclusive_or_shared(true, StreamRole::Output, "CABLE Input", in_use, shared).unwrap();
        assert_eq!(s, "shared");
        let w = w.expect("a warning");
        assert_eq!(w, Warning::ExclusiveUnavailable {
            role: StreamRole::Output,
            device: "CABLE Input".into(),
            why: ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE)
        });
        let text = describe_warning(&w);
        assert!(text.starts_with("The output device (CABLE Input) couldn't be opened in exclusive mode"), "{text}");
        assert!(text.contains("another app") && text.contains("shared mode"), "{text}");
        let (_, w) = exclusive_or_shared(true, StreamRole::Mic, "Mic", || Err::<&str, _>(ExclusiveRefusal::NoFormat), shared).unwrap();
        let text = describe_warning(&w.unwrap());
        assert!(text.starts_with("The microphone (Mic) couldn't be opened in exclusive mode") && text.contains("16-bit"), "{text}");
        let failed = exclusive_or_shared(true, StreamRole::Output, "x", in_use, || Err::<&str, Hr>(hr::AUDCLNT_E_DEVICE_INVALIDATED));
        assert_eq!(failed, Err(hr::AUDCLNT_E_DEVICE_INVALIDATED));
    }

    /// A device on its own clock: it takes the next buffer handed at each period tick (a stale one when there's none:
    /// counted), and its position runs smoothly through the period.
    struct TickDevice {
        period: usize,
        queued: std::collections::VecDeque<Vec<f32>>,
        played: Vec<f32>,
        ticks: u64,
        stale: u64,
        now: f64,
        tick_hns: f64,
        t0: f64,
        /// Added to the clock's position (a garbage reading).
        pos_offset: f64,
        /// The clock reads `None` through this many ticks (a position that reads 0 after the start).
        blind_ticks: u64,
    }

    impl TickDevice {
        fn new(period: usize) -> Self {
            Self {
                period,
                queued: Default::default(),
                played: Vec::new(),
                ticks: 0,
                stale: 0,
                now: 0.0,
                tick_hns: period as f64 * 1e7 / 48_000.0,
                t0: 0.0,
                pos_offset: 0.0,
                blind_ticks: 0,
            }
        }
        /// The next tick: the device starts the next buffer.
        fn tick(&mut self) {
            match self.queued.pop_front() {
                Some(b) => self.played.extend(b),
                None => {
                    self.stale += 1;
                    self.played.extend(std::iter::repeat_n(f32::NAN, self.period));
                }
            }
            self.now = self.t0 + self.ticks as f64 * self.tick_hns;
            self.ticks += 1;
        }
    }

    impl PeriodDevice for TickDevice {
        fn period(&self) -> usize {
            self.period
        }
        fn hand(&mut self, frames: &[f32]) -> Result<(), Hr> {
            self.queued.push_back(frames.to_vec());
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            // Started the current buffer at its tick: its position is the buffers before it.
            Ok((self.ticks > self.blind_ticks).then(|| ClockReading {
                pos_frames: ((self.ticks - 1) * self.period as u64) as f64 + self.pos_offset,
                qpc_hns: self.t0 + (self.ticks - 1) as f64 * self.tick_hns,
                now_hns: self.now,
            }))
        }
        fn now_hns(&mut self) -> f64 {
            self.now
        }
    }

    /// The ring is the padding, written as a shared buffer is; each event hands the device a period, in order; a ring
    /// short of a period hands what it has and silence (an underrun, its silence off the clock); an event the device has
    /// run past is late (an underrun). Nothing allocates per event.
    #[test]
    fn the_ring_feeds_the_device_a_period_per_event() {
        let mut r = EventRender::new(TickDevice::new(96), RING_FRAMES);
        assert_eq!((r.buffer_frames(), r.period()), (RING_FRAMES, 96));
        let ramp: Vec<f32> = (0..480).map(|v| v as f32).collect();
        r.write(&ramp).unwrap();
        assert_eq!(r.padding(), Ok(480));
        r.preroll().unwrap();
        assert_eq!(r.padding(), Ok(384));
        // Five events on time: the first four come from the ring.
        for _ in 0..4 {
            r.device_mut().tick();
            r.service().unwrap();
        }
        assert_eq!((r.padding(), r.take_underruns()), (Ok(0), 0));
        // The ring is empty at the fifth: silence, counted, and off the clock.
        r.device_mut().tick();
        r.service().unwrap();
        assert_eq!(r.take_underruns(), 1);
        assert_eq!(r.take_underruns(), 0, "taken once");
        assert_eq!(r.counts(), (96, 0, 5));
        r.write(&ramp[..50]).unwrap();
        r.device_mut().tick();
        r.service().unwrap();
        assert_eq!((r.take_underruns(), r.counts().0), (1, 96 + 46), "a short ring: what there is, then silence");
        // Played so far: the ramp, a period of silence, 50 frames and 46 of silence.
        r.device_mut().tick();
        r.device_mut().tick();
        let played = r.device().played.clone();
        assert_eq!(&played[..480], &ramp[..]);
        assert!(played[480..576].iter().all(|&v| v == 0.0));
        assert_eq!(&played[576..626], &ramp[..50]);
        // The clock in the pipeline's frames: the silence isn't the pipeline's.
        let dev = r.device_mut().clock().unwrap().unwrap().pos_frames;
        let ours = r.clock().unwrap().unwrap().pos_frames;
        assert_eq!(dev - ours, (96 + 46) as f64);
        // Late: two ticks without an event; the device replayed a stale buffer meanwhile.
        r.write(&ramp).unwrap();
        r.device_mut().tick();
        r.device_mut().tick();
        r.service().unwrap();
        assert_eq!((r.device().stale, r.take_underruns()), (3, 1), "late: three stale periods");
        assert_eq!(r.counts().1, 1);
        assert_eq!(r.counts().0, 96 + 46 + 3 * 96, "and out of the clock: the next frame handed is still the pipeline's next");
        let dev = r.device_mut().clock().unwrap().unwrap().pos_frames;
        assert_eq!(dev - r.clock().unwrap().unwrap().pos_frames, (96 + 46 + 3 * 96) as f64);
        // On time again: not late.
        for _ in 0..3 {
            r.device_mut().tick();
            r.service().unwrap();
        }
        assert_eq!((r.take_underruns(), r.counts().1), (0, 1));
        // Steady events, nothing short or late, and no allocation in the sink.
        let mut d = EventRender::new(NullDevice(96), RING_FRAMES);
        d.write(&ramp).unwrap();
        let ((), allocs, frees) = crate::test_alloc::count(|| {
            for k in 0..1000 {
                if k % 5 == 0 {
                    d.write(&ramp).unwrap();
                }
                d.service().unwrap();
                let _ = d.clock();
            }
        });
        assert_eq!((allocs, frees), (0, 0));
        assert_eq!(d.take_underruns(), 0);
    }

    /// A device that takes its buffers and keeps no clock.
    struct NullDevice(usize);

    impl PeriodDevice for NullDevice {
        fn period(&self) -> usize {
            self.0
        }
        fn hand(&mut self, _: &[f32]) -> Result<(), Hr> {
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            Ok(None)
        }
        fn now_hns(&mut self) -> f64 {
            0.0
        }
    }

    /// The first event's answer waits on the rest of the start: timed from the start, it's caught late (the device played
    /// its pre-roll and two stale periods), and the stale periods are counted out of its stream as any late answer's are.
    /// A device clock that can't say yet leaves it to the next answer it can: a device that hadn't started isn't late, and
    /// one whose clock lagged its start is.
    #[test]
    fn a_late_first_answer_is_caught() {
        let mut r = EventRender::new(TickDevice::new(96), RING_FRAMES);
        r.write(&[0.5; 480]).unwrap();
        r.preroll().unwrap();
        r.started();
        for _ in 0..3 {
            r.device_mut().tick();
        }
        r.service().unwrap();
        assert_eq!((r.device().stale, r.counts().1, r.take_underruns()), (2, 1, 1));
        assert_eq!(r.counts().0, 2 * 96, "two stale periods off the clock");
        let dev = r.device_mut().clock().unwrap().unwrap().pos_frames;
        assert_eq!(dev - r.clock().unwrap().unwrap().pos_frames, 192.0);
        // Before its clock runs, a long gap isn't late.
        let mut q = EventRender::new(TickDevice::new(96), RING_FRAMES);
        q.write(&[0.5; 480]).unwrap();
        q.preroll().unwrap();
        q.started();
        q.device_mut().now = 1e6;
        q.service().unwrap();
        assert_eq!((q.counts().1, q.take_underruns()), (0, 0));
        // It starts now: on time from here, so not late when its clock comes to life.
        q.device_mut().t0 = 1e6;
        for _ in 0..2 {
            q.device_mut().tick();
            q.service().unwrap();
        }
        assert_eq!((q.device().stale, q.counts().1, q.take_underruns()), (0, 0, 0));
        // A clock that reads nothing for its first three ticks: the late first answer is caught at the next.
        let mut b = EventRender::new(TickDevice::new(96), RING_FRAMES);
        b.device_mut().blind_ticks = 3;
        b.write(&[0.5; 480]).unwrap();
        b.preroll().unwrap();
        b.started();
        for _ in 0..3 {
            b.device_mut().tick();
        }
        b.service().unwrap();
        assert_eq!(b.counts().1, 0, "not yet");
        b.device_mut().tick();
        b.service().unwrap();
        assert_eq!((b.device().stale, b.counts().1, b.counts().0, b.take_underruns()), (2, 1, 192, 1));
    }

    /// A garbage clock position (far past anything handed, or not a number) can't overflow the counts or panic: a late
    /// answer counts at most 10 s of stale periods, and a NaN position none.
    #[test]
    fn a_garbage_clock_position_is_bounded() {
        for offset in [1e30, f64::INFINITY, f64::NAN] {
            let mut r = EventRender::new(TickDevice::new(96), RING_FRAMES);
            r.preroll().unwrap();
            r.started();
            r.device_mut().pos_offset = offset;
            for _ in 0..3 {
                r.device_mut().tick();
            }
            for _ in 0..3 {
                r.device_mut().tick();
                r.device_mut().now += 1e6;
                r.service().unwrap();
            }
            let (inserted, late, events) = r.counts();
            assert_eq!(events, 3);
            assert!(inserted <= 3 * (480_000 + 2 * 96), "{offset}: {inserted}");
            assert_eq!(late == 0, offset.is_nan(), "{offset}: {late}");
            let _ = r.clock();
        }
    }
}
