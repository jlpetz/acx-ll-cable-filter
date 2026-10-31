//! Event simulations of the output and reference bookkeeping against a device that takes a period
//! at a time: late wakes, cable stalls, lost mic packets, wake jitter, and device periods other than
//! a block (the clocks-latency verification, 2026-10-26). `FakeSink` drains continuously, so its
//! padding never steps and it can't show these cases.
//!
//! Each run uses nf-core's real drift loop and resampler. The latency of a block is when the
//! device takes its first sample, less its capture time.

use super::*;

/// A render device on its own clock that takes `period` frames per tick, with its position stepped
/// at the tick (as shared-mode engines report it). While stalled it takes nothing and its clock
/// stops.
pub(super) struct VSink {
    period: usize,
    padding: usize,
    tick_hns: f64,
    next_tick: f64,
    last_tick: f64,
    now_hns: f64,
    ticks: u64,
    written: u64,
    taken: u64,
    silence: u64,
    dry: u64,
    stall: Option<(f64, f64)>,
}

impl VSink {
    pub(super) fn new(period: usize, ppm: f64, phase: f64, now: f64) -> Self {
        let tick_hns = period as f64 * 1e7 / (48_000.0 * (1.0 + ppm * 1e-6));
        Self {
            period,
            padding: 0,
            tick_hns,
            next_tick: now + phase * tick_hns,
            last_tick: now,
            now_hns: now,
            ticks: 0,
            written: 0,
            taken: 0,
            silence: 0,
            dry: 0,
            stall: None,
        }
    }

    pub(super) fn advance_to(&mut self, t: f64) {
        while self.next_tick <= t {
            let stalled = self.stall.is_some_and(|(a, b)| self.next_tick >= a && self.next_tick < b);
            if !stalled {
                let take = self.padding.min(self.period);
                self.dry += (self.period - take) as u64;
                self.padding -= take;
                self.taken += take as u64;
                self.ticks += 1;
                self.last_tick = self.next_tick;
            }
            self.next_tick += self.tick_hns;
        }
        self.now_hns = t;
    }

    /// When the device takes stream frame `index` (hns).
    fn take_time(&self, index: u64) -> f64 {
        let ahead = index as f64 - self.taken as f64;
        self.next_tick + ahead * self.tick_hns / self.period as f64
    }
}

impl RenderSink for VSink {
    fn buffer_frames(&self) -> usize {
        4800
    }
    fn padding(&mut self) -> Result<usize, Hr> {
        Ok(self.padding)
    }
    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        assert!(self.padding + mono.len() <= 4800, "must fit");
        self.padding += mono.len();
        self.written += mono.len() as u64;
        Ok(())
    }
    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        assert!(self.padding + frames <= 4800, "must fit");
        self.padding += frames;
        self.written += frames as u64;
        self.silence += frames as u64;
        Ok(())
    }
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        if self.ticks == 0 {
            return Ok(None);
        }
        let pos = ((self.ticks - 1) * self.period as u64) as f64;
        Ok(Some(ClockReading { pos_frames: pos, qpc_hns: self.last_tick, now_hns: self.now_hns }))
    }
}

#[derive(Clone, Copy, Debug)]
enum Event {
    None,
    /// One wake this many ms late; the packets that arrive meanwhile are read at that wake.
    LateWake(f64),
    /// The mic delivers nothing for this many ms and the packets are lost (its position jumps).
    MicLost(f64),
    /// The cable takes nothing, and its clock stops, for this many ms.
    CableStall(f64),
}

#[derive(Clone, Copy, Debug)]
struct Case {
    mic_packet: usize,
    out_period: usize,
    ppm: f64,
    /// Where the device's first tick falls in its period, as a fraction.
    phase: f64,
    /// Each wake is late by a uniform 0..jitter ms.
    jitter_ms: f64,
    event: Event,
}

/// Faults are counted from this time (s), after the start-up.
const START_S: f64 = 1.0;
/// The event happens at this time (s); the steady latency is the mean over the 10 s before it.
const EVENT_S: f64 = 40.0;
const RUN_S: f64 = 100.0;

#[derive(Debug)]
struct Outcome {
    steady_ms: f64,
    /// The last time after the event that the latency was more than 1 ms off the steady value.
    settle_s: f64,
    /// Mean latency change over the last second.
    end_dev_ms: f64,
    /// Zeros, dry frames and underruns between `START_S` and the event.
    zeros_before: u64,
    zeros_after: u64,
    dry_before: u64,
    dry_after: u64,
    underruns_before: u64,
    underruns_after: u64,
    drops_after: u64,
}

fn run(c: Case) -> Outcome {
    let cfg = PipelineConfig { out_period: c.out_period, out_margin: 96, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    let t0 = 1e10;
    let mut sink = VSink::new(c.out_period, c.ppm, c.phase, t0);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut ticks = FakeTicks(0);
    let pkt_hns = c.mic_packet as f64 * 1e7 / 48_000.0;
    let mut at_start = None;
    let ev_t = t0 + EVENT_S * 1e7;
    let mut rng: u64 = 0x1234_5678_9abc_def1;
    let mut lat: Vec<(f64, f64)> = Vec::new();
    let mut at_event = None;
    let mut lost_until: Option<f64> = None;
    let mut j: u64 = 0;
    let mut wake = t0 + pkt_hns + 5_000.0;
    while wake < t0 + RUN_S * 1e7 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let u = (rng >> 11) as f64 / (1u64 << 53) as f64;
        let mut now = wake + u * c.jitter_ms * 1e4;
        if at_start.is_none() && now >= t0 + START_S * 1e7 {
            at_start = Some((sink.silence, sink.dry, p.counters.render_underruns));
        }
        if at_event.is_none() && now >= ev_t {
            at_event = Some((sink.silence, sink.dry, p.counters.render_underruns, p.counters.output_drops));
            match c.event {
                Event::LateWake(ms) => now += ms * 1e4,
                Event::MicLost(ms) => {
                    lost_until = Some(now + ms * 1e4);
                    now += ms * 1e4;
                }
                Event::CableStall(ms) => sink.stall = Some((now, now + ms * 1e4)),
                Event::None => {}
            }
        }
        sink.advance_to(now);
        while t0 + (j + 1) as f64 * pkt_hns <= now {
            let start = t0 + j as f64 * pkt_hns;
            let lost = lost_until.is_some_and(|until| start + pkt_hns > ev_t && start + pkt_hns <= until - 0.5 * pkt_hns);
            if !lost {
                mic.push(j * c.mic_packet as u64, Some(start as u64), vec![0.1; c.mic_packet]);
            }
            j += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        loop {
            let head = p.mic_abs - p.mic.len() as u64;
            let t_cap = p.mic_stamps.time_of(head);
            let before = sink.written + p.pending_len as u64;
            if !p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, false).unwrap() {
                break;
            }
            let after = sink.written + p.pending_len as u64;
            if after >= before + 479
                && let Some(tc) = t_cap
            {
                lat.push(((now - t0) / 1e7, (sink.take_time(after - 480) - tc) / 1e4));
            }
        }
        wake = t0 + (j + 1) as f64 * pkt_hns + 5_000.0;
    }
    let (z, d, u, dr) = at_event.unwrap();
    let (z0, d0, u0) = at_start.unwrap();
    let pre: Vec<f64> = lat.iter().filter(|(t, _)| *t > EVENT_S - 10.0 && *t < EVENT_S).map(|x| x.1).collect();
    let steady = pre.iter().sum::<f64>() / pre.len() as f64;
    let post: Vec<&(f64, f64)> = lat.iter().filter(|(t, _)| *t >= EVENT_S).collect();
    let settle = post.iter().filter(|(_, l)| (l - steady).abs() > 1.0).map(|x| x.0).fold(EVENT_S, f64::max) - EVENT_S;
    let end: Vec<f64> = lat.iter().filter(|(t, _)| *t > RUN_S - 1.0).map(|x| x.1 - steady).collect();
    Outcome {
        steady_ms: steady,
        settle_s: settle,
        end_dev_ms: end.iter().sum::<f64>() / end.len() as f64,
        zeros_before: z - z0,
        zeros_after: sink.silence - z,
        dry_before: d - d0,
        dry_after: sink.dry - d,
        underruns_before: u - u0,
        underruns_after: p.counters.render_underruns - u,
        drops_after: p.counters.output_drops - dr,
    }
}

fn case(event: Event, ppm: f64, phase: f64) -> Case {
    Case { mic_packet: 480, out_period: 480, ppm, phase, jitter_ms: 1.0, event }
}

/// Nothing goes wrong before the event in any of these runs.
fn assert_clean_start(c: &Case, o: &Outcome) {
    assert_eq!((o.zeros_before, o.dry_before, o.underruns_before), (0, 0, 0), "{c:?}: {o:?}");
}

/// A wake up to 15 ms late is covered by the queued blocks: no zeros, no underrun, the device never
/// runs dry, and the latency doesn't move, at any drift (the calibration keeps its phase).
#[test]
fn a_late_wake_leaves_the_latency_where_it_was() {
    for ppm in [0.0, 5.0, 300.0] {
        for phase in [0.37, 0.8] {
            for ms in [5.0, 15.0] {
                let c = case(Event::LateWake(ms), ppm, phase);
                let o = run(c);
                assert_clean_start(&c, &o);
                assert_eq!((o.zeros_after, o.dry_after, o.underruns_after), (0, 0, 0), "{c:?}: {o:?}");
                assert!(o.settle_s < 0.5 && o.end_dev_ms.abs() < 0.5, "{c:?}: {o:?}");
            }
        }
    }
}

/// A wake late by more than the device holds lets it run dry once. The latency then comes back:
/// the calibration is re-anchored at the old phase instead of a fresh guess half a period off.
#[test]
fn after_running_dry_the_latency_comes_back() {
    for ppm in [0.0, 5.0, 300.0] {
        for phase in [0.37, 0.8] {
            for ms in [25.0, 35.0] {
                let c = case(Event::LateWake(ms), ppm, phase);
                let o = run(c);
                assert_clean_start(&c, &o);
                assert!(o.underruns_after <= 1, "{c:?}: {o:?}");
                assert!(o.end_dev_ms.abs() < 1.0 && o.settle_s < 40.0, "{c:?}: {o:?}");
            }
        }
    }
}

/// A cable that stops for up to 50 ms overfills; one coarse drop and the loop bring the latency
/// back, and the device never runs dry.
#[test]
fn a_cable_stall_is_recovered() {
    for ppm in [0.0, 5.0, 300.0] {
        for phase in [0.37, 0.8] {
            for ms in [15.0, 50.0] {
                let c = case(Event::CableStall(ms), ppm, phase);
                let o = run(c);
                assert_clean_start(&c, &o);
                assert_eq!((o.dry_after, o.underruns_after), (0, 0), "{c:?}: {o:?}");
                assert!(o.drops_after <= 1, "{c:?}: {o:?}");
                assert!(o.end_dev_ms.abs() < 1.0 && o.settle_s < 40.0, "{c:?}: {o:?}");
            }
        }
    }
}

/// Lost mic packets are zero-filled by position, so the output carries on at its latency.
#[test]
fn lost_mic_packets_leave_the_output_steady() {
    for ppm in [0.0, 300.0] {
        let c = case(Event::MicLost(50.0), ppm, 0.37);
        let o = run(c);
        assert_clean_start(&c, &o);
        assert!(o.end_dev_ms.abs() < 1.0 && o.settle_s < 40.0, "{c:?}: {o:?}");
    }
}

/// 20 ms mic packets with 0-8 ms of wake jitter: no false underruns or zeros.
#[test]
fn two_blocks_per_wake_with_jitter_never_pad() {
    for ppm in [0.0, 300.0] {
        let c = Case { mic_packet: 960, out_period: 480, ppm, phase: 0.37, jitter_ms: 8.0, event: Event::None };
        let o = run(c);
        assert_clean_start(&c, &o);
        assert_eq!((o.zeros_after, o.dry_after, o.underruns_after), (0, 0, 0), "{c:?}: {o:?}");
    }
}

/// A device with a 1024-frame period (21.3 ms) runs clean at every phase against 10 ms blocks.
#[test]
fn a_long_device_period_runs_clean_at_every_phase() {
    for phase in [0.05, 0.25, 0.5, 0.75, 0.95] {
        let c = Case { mic_packet: 480, out_period: 1024, ppm: 0.0, phase, jitter_ms: 1.0, event: Event::None };
        let o = run(c);
        assert_clean_start(&c, &o);
        assert_eq!((o.zeros_after, o.dry_after, o.underruns_after), (0, 0, 0), "{c:?}: {o:?}");
    }
}

/// With no drift the readings never sweep the sawtooth, so where the latency settles within one
/// device period depends on the session's phase (the padding at the wake is what's held, as before
/// the interpolation). It stays within a period across phases.
#[test]
fn the_steady_latency_varies_by_less_than_a_period_with_the_phase() {
    let steady: Vec<f64> = [0.05, 0.37, 0.8].iter().map(|&ph| run(case(Event::None, 0.0, ph)).steady_ms).collect();
    let spread = steady.iter().cloned().fold(f64::MIN, f64::max) - steady.iter().cloned().fold(f64::MAX, f64::min);
    assert!(spread < 10.0, "{steady:?}");
}

/// The full sweep the tests above sample; `--ignored --nocapture` prints every outcome.
#[test]
#[ignore]
fn event_sweep() {
    for (mp, op) in [(480usize, 480usize), (960, 480), (480, 441), (480, 1024)] {
        for ev in [
            Event::None,
            Event::LateWake(5.0),
            Event::LateWake(15.0),
            Event::LateWake(25.0),
            Event::LateWake(35.0),
            Event::LateWake(50.0),
            Event::MicLost(50.0),
            Event::MicLost(100.0),
            Event::CableStall(15.0),
            Event::CableStall(35.0),
            Event::CableStall(50.0),
        ] {
            for ppm in [0.0, 5.0, 50.0, 300.0] {
                for phase in [0.05, 0.37, 0.8] {
                    let c = Case { mic_packet: mp, out_period: op, ppm, phase, jitter_ms: 1.0, event: ev };
                    eprintln!("{c:?}\n    {:?}", run(c));
                }
            }
        }
    }
}

/// Records the first mic and reference sample of each block (each carries its stream's index).
struct IdxSpy(Vec<(f32, f32)>);

impl BlockProcessor for IdxSpy {
    fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        self.0.push((mic[0], r[0]));
        *out = *mic;
        ChainReport::default()
    }
    fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
        (None, None)
    }
}

fn idx(start: u64, n: usize) -> Vec<f32> {
    (start..start + n as u64).map(|v| v as f32).collect()
}

#[derive(Debug)]
struct RefOutcome {
    /// Seconds until the echo path (mic index minus reference index) is within a millisecond of its
    /// steady value for good.
    start_settle_s: f64,
    /// The largest block-to-block change of the echo path after the event, in samples (a slip is
    /// one; a discard or a gap is a block).
    post_step: f64,
    drops_after: u64,
    gaps_after: u64,
}

/// Mic and loopback with 10 ms packets; at 30 s one wake is `late_ms` late (both streams queue
/// meanwhile). `lead_ms`: how long the loopback ran before the mic's first packet.
fn ref_run(late_ms: f64, lead_ms: f64, ref_ppm: f64, phase: f64) -> RefOutcome {
    let c = PipelineConfig { out_period: 480, out_margin: 96, ref_period: Some(480), max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    let t0 = 1e10;
    let mut sink = VSink::new(480, 0.0, 0.37, t0);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut lb = FakeCapture::default();
    let mut spy = IdxSpy(Vec::new());
    let mut ticks = FakeTicks(0);
    let pkt = 100_000.0;
    let lpkt = pkt / (1.0 + ref_ppm * 1e-6);
    let lb_t0 = t0 - lead_ms * 1e4 + phase * pkt;
    let (mut j, mut k) = (0u64, 0u64);
    let ev_t = t0 + 30e7;
    let mut at_event = None;
    let mut align: Vec<(f64, f64)> = Vec::new();
    let mut wake = t0 + pkt + 5_000.0;
    while wake < t0 + 60e7 {
        let mut now = wake;
        if at_event.is_none() && now >= ev_t {
            at_event = Some((p.counters.reference_gaps, p.counters.reference_drops));
            now += late_ms * 1e4;
        }
        sink.advance_to(now);
        while t0 + (j + 1) as f64 * pkt <= now {
            mic.push(j * 480, Some((t0 + j as f64 * pkt) as u64), idx(j * 480, 480));
            j += 1;
        }
        while lb_t0 + (k + 1) as f64 * lpkt <= now {
            lb.push(k * 480, Some((lb_t0 + k as f64 * lpkt) as u64), idx(k * 480, 480));
            k += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        if p.block_ready() {
            p.drain_reference(&mut lb, now).unwrap();
        }
        let before = spy.0.len();
        while p.process_block(&mut spy, &mut sink, &mut ticks, &tel, false).unwrap() {}
        for &(m, r) in &spy.0[before..] {
            align.push(((now - t0) / 1e7, (m - r) as f64));
        }
        wake = t0 + (j + 1) as f64 * pkt + 5_000.0;
    }
    let (gaps, drops) = at_event.unwrap();
    let pre: Vec<f64> = align.iter().filter(|(t, _)| *t > 20.0 && *t < 30.0).map(|x| x.1).collect();
    let steady = pre.iter().sum::<f64>() / pre.len() as f64;
    let start_settle_s = align.iter().filter(|(t, d)| *t < 30.0 && (d - steady).abs() > 48.0).map(|x| x.0).fold(0.0, f64::max);
    let post_step = align.windows(2).filter(|w| w[0].0 > 29.0).map(|w| (w[1].1 - w[0].1).abs()).fold(0.0, f64::max);
    RefOutcome { start_settle_s, post_step, drops_after: p.counters.reference_drops - drops, gaps_after: p.counters.reference_gaps - gaps }
}

/// clocks-latency/F2: a wake up to 50 ms late doesn't discard reference blocks the rest of the
/// wake's mic backlog needs, so the echo path doesn't jump and no gap follows.
#[test]
fn a_late_wake_keeps_the_echo_path() {
    for late in [15.0, 25.0, 50.0] {
        for ppm in [0.0, 100.0] {
            for phase in [0.2, 0.7] {
                let o = ref_run(late, 0.0, ppm, phase);
                assert_eq!((o.drops_after, o.gaps_after), (0, 0), "late {late} ms, {ppm} ppm, phase {phase}: {o:?}");
                assert!(o.post_step <= 2.0, "late {late} ms, {ppm} ppm, phase {phase}: {o:?}");
            }
        }
    }
}

/// A loopback that started up to 100 ms before the mic is aligned at the first block, not slipped
/// for 20-30 s.
#[test]
fn the_reference_aligns_at_the_start() {
    for lead in [15.0, 40.0, 100.0] {
        let o = ref_run(0.0, lead, 0.0, 0.2);
        assert!(o.start_settle_s < 5.0, "lead {lead} ms: {o:?}");
    }
}
