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
    /// The adaptive output target (out_target.rs) instead of the fixed one; with the old single-stage lowering (where a
    /// case starts once the target has come down: with the trial it takes a window longer to get there).
    adaptive: bool,
    single_stage: bool,
    /// The event happens at this time (s), and again every `repeat_s` (a `LateWake` only); the steady latency is the mean
    /// over the 10 s before it. The run lasts `run_s`.
    event_s: f64,
    repeat_s: Option<f64>,
    run_s: f64,
}

/// Faults are counted from this time (s), after the start-up.
const START_S: f64 = 1.0;
/// The event's time and the run's length (s), unless a case says otherwise.
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
    /// The output target just before the event and at the end (samples), and the adaptive target's strikes.
    target_before: usize,
    target_end: usize,
    strikes: u32,
}

fn run(c: Case) -> Outcome {
    let cfg = PipelineConfig { out_period: c.out_period, out_margin: 96, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    if c.adaptive {
        p = if c.single_stage { p.with_single_stage_target() } else { p.with_adaptive_target() };
    }
    let t0 = 1e10;
    let mut sink = VSink::new(c.out_period, c.ppm, c.phase, t0);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut ticks = FakeTicks(0);
    let pkt_hns = c.mic_packet as f64 * 1e7 / 48_000.0;
    let mut at_start = None;
    let ev_t = t0 + c.event_s * 1e7;
    let mut next_repeat = c.repeat_s.map(|r| ev_t + r * 1e7);
    let mut rng: u64 = 0x1234_5678_9abc_def1;
    let mut lat: Vec<(f64, f64)> = Vec::new();
    let mut at_event = None;
    let mut lost_until: Option<f64> = None;
    let mut j: u64 = 0;
    let mut wake = t0 + pkt_hns + 5_000.0;
    while wake < t0 + c.run_s * 1e7 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let u = (rng >> 11) as f64 / (1u64 << 53) as f64;
        let mut now = wake + u * c.jitter_ms * 1e4;
        if at_start.is_none() && now >= t0 + START_S * 1e7 {
            at_start = Some((sink.silence, sink.dry, p.counters.render_underruns));
        }
        if let (Some(at), Some(every), Event::LateWake(ms)) = (next_repeat, c.repeat_s, c.event)
            && at_event.is_some()
            && now >= at
        {
            now += ms * 1e4;
            next_repeat = Some(at + every * 1e7);
        }
        if at_event.is_none() && now >= ev_t {
            at_event = Some((sink.silence, sink.dry, p.counters.render_underruns, p.counters.output_drops, p.out_target()));
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
            // Once a second, as the audio loop does: where the adaptive target moves.
            if p.second_due() {
                p.publish_second(&tel, &CopyChain, 0, 0, 0);
            }
        }
        wake = t0 + (j + 1) as f64 * pkt_hns + 5_000.0;
    }
    let (z, d, u, dr, target_before) = at_event.unwrap();
    let (z0, d0, u0) = at_start.unwrap();
    let pre: Vec<f64> = lat.iter().filter(|(t, _)| *t > c.event_s - 10.0 && *t < c.event_s).map(|x| x.1).collect();
    let steady = pre.iter().sum::<f64>() / pre.len() as f64;
    let post: Vec<&(f64, f64)> = lat.iter().filter(|(t, _)| *t >= c.event_s).collect();
    let settle = post.iter().filter(|(_, l)| (l - steady).abs() > 1.0).map(|x| x.0).fold(c.event_s, f64::max) - c.event_s;
    let end: Vec<f64> = lat.iter().filter(|(t, _)| *t > c.run_s - 1.0).map(|x| x.1 - steady).collect();
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
        target_before,
        target_end: p.out_target(),
        strikes: p.adaptive.as_ref().map_or(0, AdaptiveTarget::strikes),
    }
}

fn case(event: Event, ppm: f64, phase: f64) -> Case {
    Case {
        mic_packet: 480,
        out_period: 480,
        ppm,
        phase,
        jitter_ms: 1.0,
        event,
        adaptive: false,
        single_stage: false,
        event_s: EVENT_S,
        repeat_s: None,
        run_s: RUN_S,
    }
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
        let c = Case { mic_packet: 960, jitter_ms: 8.0, ..case(Event::None, ppm, 0.37) };
        let o = run(c);
        assert_clean_start(&c, &o);
        assert_eq!((o.zeros_after, o.dry_after, o.underruns_after), (0, 0, 0), "{c:?}: {o:?}");
    }
}

/// A device with a 1024-frame period (21.3 ms) runs clean at every phase against 10 ms blocks.
#[test]
fn a_long_device_period_runs_clean_at_every_phase() {
    for phase in [0.05, 0.25, 0.5, 0.75, 0.95] {
        let c = Case { out_period: 1024, ..case(Event::None, 0.0, phase) };
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

/// The adaptive output target (out_target.rs) in these scenes, with the event at 200 s, after the target has come down (it
/// walks down from 120 s, in the old single stage: what follows once it is down is the same in both, and the trial takes a
/// window longer to get there), against the fixed target in the same scenes:
/// - up to the event it runs clean, as the fixed target does, and at a lower latency wherever it came down (by 7-9 ms with
///   this delivery's 1 ms of jitter; the 1024-frame device and the 8 ms jitter keep it near the fixed target);
/// - at the event it underruns at most once more than the fixed target. That's the trade: a wake 10-15 ms late, which the
///   fixed target's spare block covers, runs the lowered target dry once. The strike puts the target back to the fixed one
///   at that block and holds it there (the need it showed: 10 minutes), so nothing more follows, and the latency is then the
///   fixed target's;
/// - the device never runs dry without an underrun counted, and the coarse drops are the fixed target's.
#[test]
fn the_adaptive_target_after_it_has_come_down() {
    let later = |c: Case| Case { event_s: 200.0, run_s: 260.0, ..c };
    let mut cases = Vec::new();
    for ppm in [0.0, 300.0] {
        for ev in [
            Event::None,
            Event::LateWake(10.0),
            Event::LateWake(15.0),
            Event::LateWake(35.0),
            Event::CableStall(50.0),
            Event::MicLost(50.0),
        ] {
            cases.push(later(case(ev, ppm, 0.37)));
        }
        cases.push(later(case(Event::LateWake(10.0), ppm, 0.05)));
        cases.push(later(Case { mic_packet: 960, jitter_ms: 8.0, ..case(Event::None, ppm, 0.8) }));
    }
    cases.push(later(Case { out_period: 1024, ..case(Event::None, 0.0, 0.5) }));
    let mut extra = Vec::new();
    for c in cases {
        let (fixed, adaptive) = (run(c), run(Case { adaptive: true, single_stage: true, ..c }));
        let tag = format!("{c:?}: adaptive {adaptive:?} against fixed {fixed:?}");
        assert_clean_start(&c, &fixed);
        assert_clean_start(&c, &adaptive);
        assert!(adaptive.target_before <= fixed.target_before, "{tag}");
        assert!(adaptive.steady_ms < fixed.steady_ms + 0.5, "{tag}");
        if c.mic_packet == 480 && c.out_period == 480 {
            assert!(adaptive.target_before < fixed.target_before - 288, "{tag}: came down");
            assert!(adaptive.steady_ms < fixed.steady_ms - 5.0, "{tag}: and the latency with it");
        }
        assert!(adaptive.underruns_after <= fixed.underruns_after + 1, "{tag}");
        assert!(adaptive.dry_after == 0 || adaptive.underruns_after > 0, "{tag}: dry without an underrun");
        assert!(adaptive.drops_after <= fixed.drops_after, "{tag}");
        if adaptive.underruns_after > fixed.underruns_after {
            assert_eq!((adaptive.strikes, adaptive.target_end), (1, fixed.target_end), "{tag}: a strike, back to the fixed target");
            extra.push(format!("{:?} {} ppm phase {}", c.event, c.ppm, c.phase));
        }
    }
    // The trade, as measured: the 15 ms late wakes, and the 10 ms one at the worst phase.
    assert!(extra.iter().any(|e| e.starts_with("LateWake(15.0)")), "{extra:?}");
    assert!(extra.iter().all(|e| e.starts_with("LateWake(15.0)") || e.starts_with("LateWake(10.0)")), "{extra:?}");
}

/// A wake 15 ms late every 150 s, which the fixed target's spare block covers each time: the fixed target never
/// underruns. Nor does the adaptive one: each hiccup lands in a trial's window (out_target.rs) and fails it, so the target
/// stays fixed. The old single stage, lowered on each window between hiccups, underran at the first after it had come down,
/// and the strike's need held it at the fixed target through the next for 10 minutes, then (the second strike) 20; the
/// third kept it fixed for the run: over 50 minutes 3 underruns, and none after the third. Without the strikes' memory (the
/// need forgotten with the two-minute window) it underran at every other hiccup: 7 times in 1000 s.
#[test]
fn rare_hiccups_cost_the_adaptive_target_at_most_three_underruns() {
    for ppm in [0.0, 300.0] {
        let c = Case { event_s: 150.0, repeat_s: Some(150.0), run_s: 3000.0, ..case(Event::LateWake(15.0), ppm, 0.37) };
        let (fixed, trials, single) = (run(c), run(Case { adaptive: true, ..c }), run(Case { adaptive: true, single_stage: true, ..c }));
        assert_eq!((fixed.underruns_before, fixed.underruns_after, fixed.dry_after), (0, 0, 0), "{ppm} ppm: {fixed:?}");
        assert_eq!(
            (trials.underruns_before, trials.underruns_after, trials.dry_after, trials.strikes, trials.target_end),
            (0, 0, 0, 0, fixed.target_end),
            "{ppm} ppm: no trial passes: {trials:?}"
        );
        assert_eq!(single.underruns_before, 0, "{ppm} ppm: {single:?}");
        assert!(single.underruns_after >= 1 && single.underruns_after <= 3, "{ppm} ppm: {single:?}");
        assert_eq!(u64::from(single.strikes), single.underruns_after, "{ppm} ppm: {single:?}");
        assert!(single.strikes < 3 || single.target_end == fixed.target_end, "{ppm} ppm: fixed after the third: {single:?}");
    }
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
                    let c = Case { mic_packet: mp, out_period: op, ..case(ev, ppm, phase) };
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
    fn aec_stats(&self) -> Option<AecStats> {
        None
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

/// [`the_adaptive_target_after_it_has_come_down`] and the repeated hiccups, printed: `--ignored --nocapture`.
#[test]
#[ignore]
fn adaptive_sweep() {
    for ppm in [0.0, 300.0] {
        for phase in [0.05, 0.37, 0.8] {
            for ev in [
                Event::None,
                Event::LateWake(10.0),
                Event::LateWake(15.0),
                Event::LateWake(35.0),
                Event::CableStall(50.0),
                Event::MicLost(50.0),
            ] {
                let c = Case { event_s: 200.0, run_s: 260.0, ..case(ev, ppm, phase) };
                let (f, a) = (run(c), run(Case { adaptive: true, ..c }));
                eprintln!(
                    "{ev:?} {ppm} ppm phase {phase}: steady {:.2} / {:.2} ms; underruns {} / {}, dry {} / {}, drops {} / {}; target {} -> {} (strikes {})",
                    f.steady_ms,
                    a.steady_ms,
                    f.underruns_after,
                    a.underruns_after,
                    f.dry_after,
                    a.dry_after,
                    f.drops_after,
                    a.drops_after,
                    a.target_before,
                    a.target_end,
                    a.strikes
                );
            }
        }
        let c = Case { event_s: 150.0, repeat_s: Some(150.0), run_s: 3000.0, ..case(Event::LateWake(15.0), ppm, 0.37) };
        let (f, a) = (run(c), run(Case { adaptive: true, ..c }));
        eprintln!("every 150 s, {ppm} ppm: fixed {f:?}\n    adaptive {a:?}");
    }
}

/// The wakes' block counts over `seconds` of mic packets of `packet` frames, packet `j` ready `late(j)` ms after its last
/// sample, the thread waking at each ready packet (those ready together come in one wake), counted as the audio loop counts
/// them (`Telemetry::count_wake_blocks`).
fn wake_blocks(packet: usize, late: fn(u64) -> f64, seconds: f64) -> nf_core::telemetry::WakeBlocks {
    let cfg = PipelineConfig { out_period: 480, out_margin: 96, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    let t0 = 1e10;
    let mut sink = VSink::new(480, 0.0, 0.37, t0);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut ticks = FakeTicks(0);
    let pkt_hns = packet as f64 * 1e7 / 48_000.0;
    let ready = |j: u64| t0 + (j + 1) as f64 * pkt_hns + late(j) * 1e4;
    let mut j = 0u64;
    loop {
        let now = ready(j);
        if now > t0 + seconds * 1e7 {
            break;
        }
        sink.advance_to(now);
        while ready(j) <= now {
            mic.push(j * packet as u64, Some((t0 + j as f64 * pkt_hns) as u64), vec![0.1; packet]);
            j += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        let mut blocks = 0;
        while p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, false).unwrap() {
            blocks += 1;
            if p.second_due() {
                p.publish_second(&tel, &CopyChain, 0, 0, 0);
            }
        }
        tel.count_wake_blocks(blocks);
    }
    tel.snapshot().wake_blocks
}

/// The automatic model depth (TODO 5) from fake delivery, as the engine chooses it at a start (`audio::choose_model_depth`)
/// from what the run before measured (`control::measured_delivery`), keyed by how the mic opened (`audio::MicOpening`):
/// steady 10 ms packets (a block a wake, a little late at random) and the exclusive mic's 3.33 ms packets (a block every
/// third) choose depth 1; the cables' bursts (every other packet a wake late, so pairs), a pair every ten packets, a 20 ms
/// device period (two blocks a wake) and an exclusive period of 512 frames (a device whose minimum is over a block: a pair
/// every 15 wakes or so) choose 2. Unmeasured, the exclusive mic at a period of a block or less is 1 and any other mic 2; a
/// measurement of another endpoint, or of the same one opened another way (exclusive or shared, another period, another
/// option), says nothing; too short a run, or an endpoint without an id, leaves nothing to remember; a depth that's set is
/// kept; and RNNoise has none.
#[test]
fn the_wakes_choose_the_model_depth() {
    use nf_core::config::MicDelivery::{self, Bursts, OneAtATime, Unknown};

    use crate::audio::{MicOpening, choose_model_depth};
    use crate::control::measured_delivery;
    fn jittered(j: u64) -> f64 {
        0.3 + (j.wrapping_mul(2_654_435_761) % 1000) as f64 / 1000.0
    }
    fn pairs(j: u64) -> f64 {
        if j.is_multiple_of(2) { 10.5 } else { 0.5 }
    }
    fn one_pair_in_ten(j: u64) -> f64 {
        if j.is_multiple_of(10) { 10.5 } else { 0.5 }
    }
    let pack = ProcessingConfig { noise_model: "ulunas".into(), ..ProcessingConfig::default() };
    assert!(pack.model_depth_auto(), "automatic by default");
    let shared = MicOpening { id: Some("mic".into()), exclusive: false, period_frames: 480, low_latency: false, raw: true };
    let exclusive = |period_frames| MicOpening { exclusive: true, period_frames, ..shared.clone() };
    let auto = |depth, d: MicDelivery| Some(crate::api::ModelDepth { depth, auto: Some(d) });
    type Late = fn(u64) -> f64;
    let cases: [(&str, usize, Late, MicOpening, MicDelivery, u32); 6] = [
        ("steady", 480, jittered, shared.clone(), OneAtATime, 1),
        ("the exclusive mic's packets", 160, jittered, exclusive(160), OneAtATime, 1),
        ("the cables' bursts", 480, pairs, shared.clone(), Bursts, 2),
        ("a pair every ten packets", 480, one_pair_in_ten, shared.clone(), Bursts, 2),
        ("a 20 ms period", 960, jittered, MicOpening { period_frames: 960, ..shared.clone() }, Bursts, 2),
        ("an exclusive period of 512 frames", 512, jittered, exclusive(512), Bursts, 2),
    ];
    for (what, packet, late, mic, delivery, depth) in cases {
        let wakes = wake_blocks(packet, late, 30.0);
        assert_eq!(wakes.delivery(), delivery, "{what}: {wakes:?}");
        let remembered = measured_delivery(mic.clone(), &wakes);
        assert_eq!(remembered, Some((mic.clone(), delivery)), "{what}");
        assert_eq!(choose_model_depth(&pack, &mic, remembered.as_ref()), auto(depth, delivery), "{what}: {wakes:?}");
        // Unmeasured: a block at a time only for the exclusive mic at a period of a block or less.
        let unmeasured = if mic.exclusive && mic.period_frames <= 480 { auto(1, OneAtATime) } else { auto(2, Unknown) };
        assert_eq!(choose_model_depth(&pack, &mic, None), unmeasured, "{what}");
        // Another endpoint, or this one opened another way, isn't what was measured.
        let others = [
            MicOpening { id: Some("another mic".into()), ..mic.clone() },
            MicOpening { id: None, ..mic.clone() },
            MicOpening { exclusive: !mic.exclusive, ..mic.clone() },
            MicOpening { period_frames: mic.period_frames / 2, ..mic.clone() },
            MicOpening { low_latency: !mic.low_latency, ..mic.clone() },
            MicOpening { raw: !mic.raw, ..mic.clone() },
        ];
        for other in others {
            assert_eq!(
                choose_model_depth(&pack, &other, remembered.as_ref()),
                choose_model_depth(&pack, &other, None),
                "{what}: {other:?}"
            );
        }
        // A depth that's set is used, said as set.
        let set = ProcessingConfig { model_depth: 3, ..pack.clone() };
        assert_eq!(choose_model_depth(&set, &mic, remembered.as_ref()), Some(crate::api::ModelDepth { depth: 3, auto: None }));
        assert_eq!(choose_model_depth(&ProcessingConfig::default(), &mic, remembered.as_ref()), None, "RNNoise");
        assert_eq!(measured_delivery(MicOpening { id: None, ..mic }, &wakes), None, "{what}: no id to know it by");
    }
    // Five seconds is too few wakes to say: nothing is remembered.
    let short = wake_blocks(480, pairs, 5.0);
    assert_eq!((short.delivery(), measured_delivery(shared.clone(), &short)), (Unknown, None));
    assert_eq!(choose_model_depth(&pack, &shared, None), auto(2, Unknown), "a shared mic not measured yet: the safe depth");
}
