//! Playing test signals and recording what comes back (engine.md "Latency test" and "Play and record").
//!
//! - [`Recorder`] reads a capture stream into one recording with (sample index → QPC) stamps, zero-filling the frames missing by
//!   device position, and counts those gaps.
//! - [`Player`] keeps a render stream topped up with a programme: silence, the signal, then silence; and a late part where the
//!   caller places it while the take runs ([`LatePart`]).
//! - [`play_record`] plays mono buffers into one or more render endpoints and records one capture endpoint meanwhile, blocking on
//!   the calling thread (COM MTA, MMCSS "Pro Audio" while streaming). `noise-filter quality` uses it.
//!
//! The latency tests (`latency_test.rs`) use the recorder and the player for their sweep trains.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nf_core::SAMPLE_RATE;
use wasapi::{DeviceEnumerator, Direction};

use crate::api::{EngineError, LatePart, LateStart, PlayRecordRequest, PlayRecordResult, PlayTrack, TrackRole};
use crate::devices::resolve;
use crate::errors::{StreamRole, describe_com_failure, describe_hr, describe_start_failure};
use crate::pipeline::{CaptureSource, ClockReading, Hr, RenderSink};
use crate::sched::{Com, Mmcss, MmcssPriority};
use crate::stream::{CaptureStream, RenderStream, hr_of};

/// What a cancelled test or recording fails with.
pub(crate) const CANCELLED: &str = "The test was cancelled.";
/// A jump in the capture's device position up to this (1 s) is zero-filled; a bigger one is a reset, counted but not filled.
const MAX_GAP_FILL: u64 = SAMPLE_RATE as u64;
/// `play_record`'s render buffers (200 ms) and the fill it keeps (100 ms): latency doesn't matter there, a late wake-up does.
const PLAY_BUFFER_HNS: i64 = 2_000_000;
const PLAY_FILL: usize = 4_800;
/// The lead and the tail at most, in seconds, and the longest recording (15 minutes).
const MAX_LEAD_TAIL_S: f32 = 10.0;
const MAX_RECORDING_S: f64 = 900.0;

/// One recording with its (sample index → QPC hns) stamps.
#[derive(Debug, Default)]
pub(crate) struct Recording {
    pub(crate) samples: Vec<f32>,
    pub(crate) stamps: Vec<(u64, f64)>,
}

/// Reads a capture stream into a [`Recording`].
pub(crate) struct Recorder {
    pub(crate) rec: Recording,
    packet: Vec<f32>,
    next_pos: Option<u64>,
    /// Jumps in the device position, and the frames zero-filled for them.
    pub(crate) gaps: u64,
    pub(crate) gap_frames: u64,
}

impl Recorder {
    /// For packets of up to `max_packet` frames, with room for `capacity` samples.
    pub(crate) fn new(max_packet: usize, capacity: usize) -> Self {
        Self {
            rec: Recording { samples: Vec::with_capacity(capacity), stamps: Vec::with_capacity(capacity / 64) },
            packet: vec![0.0; max_packet],
            next_pos: None,
            gaps: 0,
            gap_frames: 0,
        }
    }

    /// Reads every packet that is ready.
    pub(crate) fn read(&mut self, cap: &mut impl CaptureSource) -> Result<(), Hr> {
        while let Some(p) = cap.read_packet(&mut self.packet)? {
            if let Some(expect) = self.next_pos
                && p.device_pos > expect
            {
                let gap = p.device_pos - expect;
                self.gaps += 1;
                if gap <= MAX_GAP_FILL {
                    self.rec.samples.extend(std::iter::repeat_n(0.0, gap as usize));
                    self.gap_frames += gap;
                }
            }
            self.next_pos = Some(p.device_pos + p.frames as u64);
            if let Some(t) = p.qpc_hns {
                self.rec.stamps.push((self.rec.samples.len() as u64, t as f64));
            }
            self.rec.samples.extend_from_slice(&self.packet[..p.frames.min(self.packet.len())]);
        }
        Ok(())
    }
}

/// Feeds a render stream its programme: `pre` frames of silence, the signal, then silence for as long as it's asked. With a
/// late part ([`LatePart`]), that part from its start (frames after `pre`), silence before and after it: the start is fixed
/// as the writer goes, once the caller has set it, or at the default when the writer gets there first.
pub(crate) struct Player<'a> {
    signal: &'a [f32],
    pre: usize,
    late: Option<LatePart<'a>>,
    /// Where the late part starts, once fixed.
    late_start: Option<LateStart>,
    written: u64,
    /// Frames played at the last top-up, by what had been written and what was still queued.
    played_estimate: f64,
}

/// What the programme holds from the writer's position: a signal's samples from there, or silence for that many frames
/// (`usize::MAX`: from there on).
enum Segment<'a> {
    Signal(&'a [f32]),
    Silence(usize),
}

impl<'a> Player<'a> {
    pub(crate) fn new(signal: &'a [f32], pre: usize) -> Self {
        Self::with_late(signal, pre, None)
    }

    pub(crate) fn with_late(signal: &'a [f32], pre: usize, late: Option<LatePart<'a>>) -> Self {
        Self { signal, pre, late, late_start: None, written: 0, played_estimate: 0.0 }
    }

    /// Frames handed to the stream so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Where the late part starts, once that's fixed.
    pub(crate) fn late_start(&self) -> Option<LateStart> {
        self.late_start
    }

    /// Where the programme's signals end: the first's end, or the late part's (from its latest start until it's fixed).
    pub(crate) fn end(&self) -> u64 {
        let first = (self.pre + self.signal.len()) as u64;
        match self.late {
            None => first,
            Some(late) => {
                let start = self.late_start.map_or_else(|| self.clamp_start(&late, late.latest), |s| s.frames);
                first.max(self.pre as u64 + start + late.samples.len() as u64)
            }
        }
    }

    /// A late start as it counts: from the first signal's end to the latest start.
    fn clamp_start(&self, late: &LatePart<'_>, frames: u64) -> u64 {
        let first = self.signal.len() as u64;
        frames.clamp(first, late.latest.max(first))
    }

    /// Fixes the late part's start, with the writer at `at`: once the caller has set it (where it said, or where the writer
    /// is if that's past it), or at the default once the writer gets there with none set (setting it, so a later set fails).
    fn place_late(&mut self, at: u64) {
        let (Some(late), None) = (self.late, self.late_start) else { return };
        let now = at.saturating_sub(self.pre as u64);
        let default = self.clamp_start(&late, late.default);
        let set = match late.get() {
            Some(v) => Some(v),
            None if now >= default => {
                if late.set(default) {
                    None
                } else {
                    late.get()
                }
            }
            None => return,
        };
        let asked = set.map(|v| self.clamp_start(&late, v));
        self.late_start = Some(LateStart { frames: asked.unwrap_or(default).max(now), set: asked });
    }

    /// The programme from `at` (see [`Segment`]). The late part's start is fixed by then, unless the writer is short of its
    /// default.
    fn segment(&self, at: u64) -> Segment<'a> {
        let at = at as usize;
        let (pre, first_end) = (self.pre, self.pre + self.signal.len());
        if at < pre {
            return Segment::Silence(pre - at);
        }
        if at < first_end {
            return Segment::Signal(&self.signal[at - pre..]);
        }
        let Some(late) = self.late else { return Segment::Silence(usize::MAX) };
        let Some(s) = self.late_start else {
            // Not placed yet: silence up to the default, where it's placed at the latest.
            return Segment::Silence((pre + self.clamp_start(&late, late.default) as usize).saturating_sub(at).max(1));
        };
        let start = pre + s.frames as usize;
        if at < start {
            Segment::Silence(start - at)
        } else if at < start + late.samples.len() {
            Segment::Signal(&late.samples[at - start..])
        } else {
            Segment::Silence(usize::MAX)
        }
    }

    /// Tops `out` up to `fill` frames queued, as far as its buffer allows. Returns the padding found before writing.
    pub(crate) fn top_up(&mut self, out: &mut impl RenderSink, fill: usize) -> Result<usize, Hr> {
        let padding = out.padding()?;
        self.played_estimate = self.written as f64 - padding as f64;
        let mut room = fill.saturating_sub(padding).min(out.buffer_frames().saturating_sub(padding));
        while room > 0 {
            self.place_late(self.written);
            let n = match self.segment(self.written) {
                Segment::Signal(x) => {
                    let n = room.min(x.len());
                    out.write(&x[..n])?;
                    n
                }
                Segment::Silence(run) => {
                    let n = room.min(run);
                    out.write_silence(n)?;
                    n
                }
            };
            self.written += n as u64;
            room -= n;
        }
        Ok(padding)
    }

    /// Frames played so far: the stream's clock when it runs, else the last top-up's estimate.
    pub(crate) fn played(&self, clock: Option<ClockReading>) -> f64 {
        clock.map_or(self.played_estimate, |c| c.pos_frames)
    }
}

/// Request checks that need no device.
pub(crate) fn validate_play_record(req: &PlayRecordRequest<'_>) -> Result<(), String> {
    if req.play.is_empty() {
        return Err("Give at least one playback device and the signal it plays.".into());
    }
    if req.play.iter().any(|t| t.device.is_empty()) {
        return Err("Pick each playback device.".into());
    }
    if req.play.iter().enumerate().any(|(i, t)| req.play[..i].iter().any(|u| u.device == t.device)) {
        return Err("Each playback device plays one signal; mix them first.".into());
    }
    if req.capture_device.is_empty() {
        return Err("Pick the recording device to record.".into());
    }
    for t in &req.play {
        if let Some(late) = &t.late
            && !(t.samples.len() as u64 <= late.default && late.default <= late.latest)
        {
            return Err("A track's late part starts after its first signal: by its default start, and its latest.".into());
        }
    }
    let longest = req.play.iter().map(longest_programme).max().unwrap_or(0);
    check_length(longest, req.lead_s, req.tail_s)
}

/// The longest a track's programme can be (after the lead): its signal, or its late part from its latest start.
fn longest_programme(t: &PlayTrack<'_>) -> usize {
    let late = t.late.map_or(0, |l| usize::try_from(l.latest).unwrap_or(usize::MAX).saturating_add(l.samples.len()));
    t.samples.len().max(late)
}

/// The lead and the tail are 0-10 s each, and the whole recording 15 minutes at most.
fn check_length(longest: usize, lead_s: f32, tail_s: f32) -> Result<(), String> {
    let seconds_ok = |s: f32| s.is_finite() && (0.0..=MAX_LEAD_TAIL_S).contains(&s);
    if !seconds_ok(lead_s) || !seconds_ok(tail_s) {
        return Err(format!("The lead and the tail are 0 to {MAX_LEAD_TAIL_S} s each."));
    }
    if longest as f64 / f64::from(SAMPLE_RATE) + f64::from(lead_s + tail_s) > MAX_RECORDING_S {
        return Err(format!("A recording is {} minutes at most.", MAX_RECORDING_S / 60.0));
    }
    Ok(())
}

/// The stream role a track's errors are worded for.
fn stream_role(role: TrackRole) -> StreamRole {
    match role {
        TrackRole::Probe => StreamRole::Probe,
        TrackRole::FarEnd => StreamRole::FarEnd,
    }
}

pub(crate) fn play_record(req: &PlayRecordRequest<'_>) -> Result<PlayRecordResult, EngineError> {
    validate_play_record(req).map_err(EngineError)?;
    let _com = Com::mta().map_err(|c| EngineError(describe_com_failure(c)))?;
    let fs = f64::from(SAMPLE_RATE);
    let en = DeviceEnumerator::new().map_err(|e| EngineError(describe_hr(StreamRole::Probe, hr_of(&e))))?;
    let mut renders = Vec::with_capacity(req.play.len());
    // A device that isn't found has no name to give: its role says which one it is.
    for t in &req.play {
        let role = stream_role(t.role);
        let dev = resolve(&en, Some(&t.device), Direction::Render, role).map_err(|f| EngineError(describe_start_failure(&f)))?;
        let name = dev.get_friendlyname().unwrap_or_else(|_| t.device.clone());
        renders
            .push(RenderStream::open(&dev, PLAY_BUFFER_HNS).map_err(|e| EngineError(format!("{name}: {}", describe_hr(role, hr_of(&e)))))?);
    }
    let cap_dev = resolve(&en, Some(&req.capture_device), Direction::Capture, StreamRole::Capture)
        .map_err(|f| EngineError(describe_start_failure(&f)))?;
    let mut cap = CaptureStream::open_events(&cap_dev).map_err(|e| EngineError(describe_hr(StreamRole::Capture, hr_of(&e))))?;

    let lead = (f64::from(req.lead_s) * fs).round() as usize;
    let tail = (f64::from(req.tail_s) * fs).round() as usize;
    // The most the take can last: late parts from their latest starts.
    let total = lead + req.play.iter().map(longest_programme).max().unwrap_or(0) + tail;
    let mut players: Vec<Player<'_>> = req.play.iter().map(|t| Player::with_late(t.samples, lead, t.late)).collect();
    let mut rec = Recorder::new(cap.max_packet_frames(), total + 2 * SAMPLE_RATE as usize);
    let mut underruns = 0u64;
    // Real-time priority only while streaming (RT-2).
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    let roles: Vec<StreamRole> = req.play.iter().map(|t| stream_role(t.role)).collect();
    let tracks = Tracks { renders: &mut renders, roles: &roles, players: &mut players };
    let streamed = stream(tracks, &mut cap, &mut rec, Length { tail, most: total }, req.cancel, &mut underruns);
    for r in &renders {
        r.stop();
    }
    cap.stop();
    drop(mmcss);
    streamed?;
    Ok(PlayRecordResult {
        recording: rec.rec.samples,
        lead_frames: lead,
        capture_gaps: rec.gaps,
        gap_frames: rec.gap_frames,
        playback_underruns: underruns,
        playback: renders.iter().map(|r| r.info.stream_info()).collect(),
        capture: cap.info.stream_info(),
        late_starts: players.iter().map(Player::late_start).collect(),
    })
}

/// How long the take runs: until every track's programme (each player's end) has played, plus `tail` frames; `most` at the
/// longest (late parts from their latest starts).
struct Length {
    tail: usize,
    most: usize,
}

/// The render streams, each one's role (for its errors) and its player, in the request's order.
struct Tracks<'s, 'a> {
    renders: &'s mut [RenderStream],
    roles: &'s [StreamRole],
    players: &'s mut [Player<'a>],
}

/// `play_record`'s loop: every render stream's programme played out and the tail after it, the capture read meanwhile.
fn stream(
    t: Tracks<'_, '_>, cap: &mut CaptureStream, rec: &mut Recorder, length: Length, cancel: Option<&AtomicBool>, underruns: &mut u64,
) -> Result<(), EngineError> {
    let track_err = |r: &RenderStream, role: StreamRole, hr: Hr| EngineError(format!("{}: {}", r.info.name, describe_hr(role, hr)));
    let cap_err = |hr: Hr| EngineError(describe_hr(StreamRole::Capture, hr));
    for ((p, r), &role) in t.players.iter_mut().zip(t.renders.iter_mut()).zip(t.roles) {
        p.top_up(r, PLAY_FILL).map_err(|hr| track_err(r, role, hr))?;
    }
    cap.start().map_err(cap_err)?;
    for (r, &role) in t.renders.iter().zip(t.roles) {
        r.start().map_err(|hr| track_err(r, role, hr))?;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(length.most as f64 / f64::from(SAMPLE_RATE) + 10.0);
    loop {
        if cancel.is_some_and(|c| c.load(Relaxed)) {
            return Err(EngineError(CANCELLED.into()));
        }
        if Instant::now() > deadline {
            return Err(EngineError("A playback device stopped playing; check that it's connected and not in use.".into()));
        }
        cap.wait(10);
        let mut least_played = f64::INFINITY;
        for ((p, r), &role) in t.players.iter_mut().zip(t.renders.iter_mut()).zip(t.roles) {
            let padding = p.top_up(r, PLAY_FILL).map_err(|hr| track_err(r, role, hr))?;
            // Found empty before its signal was all written: the device ran dry, and played silence for the rest of the wait.
            if padding == 0 && p.written() < p.end() {
                *underruns += 1;
            }
            let clock = r.clock().map_err(|hr| track_err(r, role, hr))?;
            least_played = least_played.min(p.played(clock));
        }
        rec.read(cap).map_err(cap_err)?;
        // Every track plays on until the longest programme and the tail have played (a late part counts from its latest
        // start until it's placed).
        let total = t.players.iter().map(Player::end).max().unwrap_or(0) as f64 + length.tail as f64;
        if least_played >= total {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::Release;

    use super::*;
    use crate::pipeline::Packet;

    #[derive(Default)]
    struct FakeCapture(VecDeque<(u64, Option<u64>, Vec<f32>)>);

    impl CaptureSource for FakeCapture {
        fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<Packet>, Hr> {
            let Some((pos, qpc, x)) = self.0.pop_front() else { return Ok(None) };
            let n = x.len().min(out.len());
            out[..n].copy_from_slice(&x[..n]);
            Ok(Some(Packet { frames: n, device_pos: pos, qpc_hns: qpc, discontinuity: false }))
        }
    }

    /// Takes what is written (silence as zeros); the test sets the padding before each top-up.
    struct FakeSink {
        written: Vec<f32>,
        padding: usize,
        buffer: usize,
    }

    impl RenderSink for FakeSink {
        fn buffer_frames(&self) -> usize {
            self.buffer
        }
        fn padding(&mut self) -> Result<usize, Hr> {
            Ok(self.padding)
        }
        fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
            self.written.extend_from_slice(mono);
            Ok(())
        }
        fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
            self.written.extend(std::iter::repeat_n(0.0, frames));
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            Ok(None)
        }
    }

    #[test]
    fn capture_gaps_are_zero_filled_by_position_and_counted() {
        let mut cap = FakeCapture::default();
        cap.0.push_back((1000, Some(5_000), vec![1.0; 4]));
        cap.0.push_back((1004, Some(5_400), vec![2.0; 4]));
        // Three frames missing
        cap.0.push_back((1011, None, vec![3.0; 2]));
        // A reset: counted, not filled
        cap.0.push_back((1013 + 2 * MAX_GAP_FILL, Some(9_000), vec![4.0; 1]));
        let mut r = Recorder::new(16, 64);
        r.read(&mut cap).unwrap();
        assert_eq!(r.rec.samples, [1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 0.0, 0.0, 0.0, 3.0, 3.0, 4.0]);
        assert_eq!(r.rec.stamps, [(0, 5_000.0), (4, 5_400.0), (13, 9_000.0)]);
        assert_eq!((r.gaps, r.gap_frames), (2, 3));
        // Nothing ready: nothing changes
        r.read(&mut cap).unwrap();
        assert_eq!(r.rec.samples.len(), 14);
    }

    #[test]
    fn the_player_writes_silence_the_signal_then_silence() {
        let signal = [0.5, 0.25, -0.25, -0.5, 0.75];
        let mut p = Player::new(&signal, 3);
        assert_eq!(p.end(), 8);
        let mut sink = FakeSink { written: Vec::new(), padding: 0, buffer: 100 };
        // Up to the fill: the lead-in and part of the signal in one top-up
        assert_eq!(p.top_up(&mut sink, 4), Ok(0));
        assert_eq!(sink.written, [0.0, 0.0, 0.0, 0.5]);
        // Three still queued: one more frame
        sink.padding = 3;
        assert_eq!(p.top_up(&mut sink, 4), Ok(3));
        assert_eq!(sink.written.len(), 5);
        assert_eq!(p.played(None), 1.0, "4 written, 3 queued");
        // Empty, with a large fill: the rest of the signal, then silence
        sink.padding = 0;
        p.top_up(&mut sink, 10).unwrap();
        assert_eq!(sink.written, [0.0, 0.0, 0.0, 0.5, 0.25, -0.25, -0.5, 0.75, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(p.written(), 15);
        // Never past the buffer, whatever the fill
        sink.buffer = 4;
        sink.padding = 2;
        p.top_up(&mut sink, 50).unwrap();
        assert_eq!(p.written(), 17);
        let c = ClockReading { pos_frames: 12.0, qpc_hns: 0.0, now_hns: 0.0 };
        assert_eq!(p.played(Some(c)), 12.0, "the clock wins when it runs");
    }

    #[test]
    fn play_record_requests_are_checked() {
        let x = vec![0.0f32; 480];
        let ok = PlayRecordRequest {
            play: vec![PlayTrack { device: "probe".into(), role: TrackRole::Probe, samples: &x, late: None }, PlayTrack {
                device: "far".into(),
                role: TrackRole::FarEnd,
                samples: &x,
                late: None,
            }],
            capture_device: "capture".into(),
            ..Default::default()
        };
        assert_eq!((ok.lead_s, ok.tail_s), (1.0, 2.0));
        assert!(validate_play_record(&ok).is_ok());
        let err = |r: &PlayRecordRequest<'_>| validate_play_record(r).unwrap_err();
        assert!(err(&PlayRecordRequest { play: Vec::new(), ..ok.clone() }).contains("at least one"));
        assert!(err(&PlayRecordRequest { capture_device: String::new(), ..ok.clone() }).contains("recording device"));
        let twice = PlayRecordRequest { play: vec![ok.play[0].clone(), ok.play[0].clone()], ..ok.clone() };
        assert!(err(&twice).contains("one signal"));
        assert!(err(&PlayRecordRequest { lead_s: -1.0, ..ok.clone() }).contains("lead"));
        assert!(err(&PlayRecordRequest { tail_s: f32::NAN, ..ok.clone() }).contains("tail"));
        assert!(check_length(897 * 48_000, 1.0, 2.0).is_ok());
        assert!(check_length(897 * 48_000 + 1, 1.0, 2.0).unwrap_err().contains("15 minutes"));
        // A late part: after the first signal, by its default and its latest; the take's length counts it from its latest
        let start = AtomicU64::new(LatePart::UNSET);
        let with_late = |default: u64, latest: u64, len: usize| {
            let mut r = ok.clone();
            r.play[1].late = Some(LatePart { samples: &x[..len], start: &start, default, latest });
            r
        };
        assert!(validate_play_record(&with_late(480, 480, 480)).is_ok());
        assert!(validate_play_record(&with_late(1_000, 5_000, 480)).is_ok());
        for bad in [with_late(479, 5_000, 480), with_late(2_000, 1_999, 480)] {
            assert!(err(&bad).contains("late part"), "{}", err(&bad));
        }
        let long = vec![0.0f32; 48_000];
        let mut r = with_late(480, 896 * 48_000 + 1, 1);
        r.play[1].late.as_mut().unwrap().samples = &long;
        assert!(err(&r).contains("15 minutes"), "the late part's latest end counts");
        assert_eq!(longest_programme(&r.play[1]), 897 * 48_000 + 1);
    }

    /// The late part: placed where the caller sets it, ahead of the writer; where the writer is when the set comes after it
    /// had passed that point; at the default when the writer gets there first, after which a set fails; clamped.
    #[test]
    fn the_late_part_starts_where_it_is_set_or_at_the_default() {
        let (first, late) = ([0.5f32, 0.5], [1.0f32, 2.0, 3.0]);
        let sink = || FakeSink { written: Vec::new(), padding: 0, buffer: 1_000 };
        let start = AtomicU64::new(LatePart::UNSET);
        let part = LatePart { samples: &late, start: &start, default: 6, latest: 9 };

        // Set before the writer gets there: 4 frames after the lead (pre 1)
        let mut p = Player::with_late(&first, 1, Some(part));
        assert_eq!(p.end(), 1 + 9 + 3, "until it's placed, from the latest start");
        let mut out = sink();
        p.top_up(&mut out, 2).unwrap();
        assert!(part.set(4) && !part.set(5), "set once");
        assert_eq!(part.get(), Some(4));
        p.top_up(&mut out, 20).unwrap();
        assert_eq!(out.written[..11], [0.0, 0.5, 0.5, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0, 0.0]);
        assert_eq!(p.late_start(), Some(LateStart { frames: 4, set: Some(4) }));
        assert_eq!(p.end(), 1 + 4 + 3);

        // Never set: the writer stops at the default, places it there, and a set after that fails
        start.store(LatePart::UNSET, Release);
        let mut p = Player::with_late(&first, 1, Some(part));
        let mut out = sink();
        p.top_up(&mut out, 5).unwrap();
        assert_eq!((p.written(), p.late_start()), (5, None), "short of the default (frame 7)");
        p.top_up(&mut out, 20).unwrap();
        assert_eq!(p.late_start(), Some(LateStart { frames: 6, set: None }));
        assert!(!part.set(3), "the player set the default");
        assert_eq!(out.written[..11], [0.0, 0.5, 0.5, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0]);

        // Set after the writer passed it: it starts where the writer is, later than asked
        start.store(LatePart::UNSET, Release);
        let mut p = Player::with_late(&first, 1, Some(part));
        let mut out = sink();
        p.top_up(&mut out, 6).unwrap();
        assert!(part.set(2));
        p.top_up(&mut out, 20).unwrap();
        assert_eq!(p.late_start(), Some(LateStart { frames: 5, set: Some(2) }));
        assert_eq!(out.written[..10], [0.0, 0.5, 0.5, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0]);

        // Out of range: clamped to the first signal's end and to the latest
        for (asked, at) in [(0, 2), (100, 9)] {
            start.store(LatePart::UNSET, Release);
            let mut p = Player::with_late(&first, 1, Some(part));
            assert!(part.set(asked));
            let mut out = sink();
            p.top_up(&mut out, 20).unwrap();
            assert_eq!(p.late_start(), Some(LateStart { frames: at, set: Some(at) }), "{asked}");
            assert_eq!(out.written[1 + at as usize..4 + at as usize], late);
        }
        assert!(!part.set(LatePart::UNSET) && part.get().is_some(), "UNSET can't be set");
        // Without a late part: nothing to place
        let p = Player::new(&first, 1);
        assert_eq!((p.late_start(), p.end()), (None, 3));
    }

    #[test]
    fn each_track_is_worded_for_its_role() {
        assert_eq!(stream_role(TrackRole::Probe), StreamRole::Probe);
        assert_eq!(stream_role(TrackRole::FarEnd), StreamRole::FarEnd);
        assert_eq!(TrackRole::default(), TrackRole::Probe);
    }

    #[test]
    fn play_record_checks_before_touching_a_device() {
        let err = play_record(&PlayRecordRequest::default()).unwrap_err();
        assert!(err.0.contains("at least one"), "{err}");
    }
}
