//! Exclusive mode on the audio path (engine.md, "Exclusive mode"): the event-driven output (exclusive.rs's `EventRender`)
//! against a device on its own clock with a 2 ms period, answered at its events while the mic's packets come at theirs;
//! and the exclusive mic's small packets (3 ms and 3.33 ms), with the echo reference read against their stamps.
//!
//! The device takes the buffer handed for each of its period ticks (a stale one when none was), and its clock reads the
//! frames played at the tick. The thread answers each tick after `answer_ms` (and a jitter), unless it's stalled.

use super::*;
use crate::exclusive::{EventRender, PeriodDevice, RING_FRAMES};

const T0: f64 = 1e10;
const H: f64 = 1e7 / 48_000.0;
/// Means are taken from here on (the fill's calibration has its sawtooth by then).
const SETTLED_S: f64 = 20.0;

/// An exclusive render device with a `period`-frame buffer on its own clock.
struct XDev {
    period: usize,
    tick_hns: f64,
    next_tick: f64,
    last_tick: f64,
    ticks: u64,
    /// Buffers handed and not started yet (at most one: the device plays the other).
    queued: u32,
    stale: u64,
    now: f64,
}

impl XDev {
    fn new(period: usize, ppm: f64) -> Self {
        let tick_hns = period as f64 * H / (1.0 + ppm * 1e-6);
        Self { period, tick_hns, next_tick: f64::INFINITY, last_tick: 0.0, ticks: 0, queued: 0, stale: 0, now: 0.0 }
    }

    /// The stream starts at `t`: its first tick takes the pre-roll.
    fn start(&mut self, t: f64) {
        self.next_tick = t;
    }

    fn tick(&mut self) {
        if self.queued > 0 {
            self.queued -= 1;
        } else {
            self.stale += 1;
        }
        self.last_tick = self.next_tick;
        self.now = self.next_tick;
        self.ticks += 1;
        self.next_tick += self.tick_hns;
    }
}

impl PeriodDevice for XDev {
    fn period(&self) -> usize {
        self.period
    }
    fn hand(&mut self, frames: &[f32]) -> Result<(), Hr> {
        assert_eq!(frames.len(), self.period);
        assert!(self.queued == 0, "a buffer handed with the device's other one still queued: answered without an event");
        self.queued += 1;
        Ok(())
    }
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        Ok((self.ticks > 0).then(|| ClockReading {
            pos_frames: ((self.ticks - 1) * self.period as u64) as f64,
            qpc_hns: self.last_tick,
            now_hns: self.now,
        }))
    }
    fn now_hns(&mut self) -> f64 {
        self.now
    }
}

/// When a mic packet is ready, in ms after its last sample, by its number.
type Late = fn(u64) -> f64;

fn steady(_: u64) -> f64 {
    0.5
}
/// The cables' bursts: every other packet a wake (10 ms) late.
fn bursty(j: u64) -> f64 {
    if j.is_multiple_of(2) { 10.5 } else { 0.5 }
}
/// One packet in five a wake late.
fn rare_bursts(j: u64) -> f64 {
    if j.is_multiple_of(5) { 10.5 } else { 0.5 }
}
/// Packets 3000-3003 (from 30 s) held back and delivered together, 40 ms on: the ring runs dry, then the output overfills.
fn mic_stall(j: u64) -> f64 {
    if (3000..3004).contains(&j) { (3004 - j) as f64 * 10.0 + 0.5 } else { 0.5 }
}

#[derive(Clone, Copy)]
struct XSim {
    /// The mic's packets (frames) and when each is ready; the thread wakes on each (its event) and on the device's.
    packet: usize,
    late: Late,
    period: usize,
    ppm: f64,
    /// The thread answers a device tick this long after it, plus up to `jitter_ms`.
    answer_ms: f64,
    jitter_ms: f64,
    /// The thread doesn't run from `.0` s for `.1` ms.
    stall: Option<(f64, f64)>,
    /// A loopback (10 ms stamped packets, 20 ms ahead of the mic) for the echo reference.
    reference: bool,
    adaptive: bool,
    seconds: f64,
}

const BASE: XSim = XSim {
    packet: 480,
    late: steady,
    period: 96,
    ppm: 300.0,
    answer_ms: 0.05,
    jitter_ms: 0.2,
    stall: None,
    reference: false,
    adaptive: false,
    seconds: 40.0,
};

#[derive(Clone, Copy, Debug)]
struct Row {
    t_s: f64,
    b: LatencyBudget,
    readout: f32,
    /// The block's first sample's capture time by the stamps, against the truth.
    capture_err_hns: f64,
}

/// Copies the mic and records each block's first mic and reference samples (index-coded, plus one).
struct Spy(Vec<(f32, f32)>);

impl BlockProcessor for Spy {
    fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        self.0.push((mic[0], r[0]));
        *out = *mic;
        ChainReport::default()
    }
    fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
        (None, None)
    }
}

struct XOut {
    rows: Vec<Row>,
    blocks: usize,
    /// The output target after each block (samples), at its time (s).
    targets: Vec<(f64, usize)>,
    /// The silence the sink put in before the first block (the output runs before the mic: not an underrun).
    start_silence: u64,
    p: Pipeline,
    sink: EventRender<XDev>,
    spy: Spy,
    tel: Telemetry,
}

fn idx1(start: u64, n: usize) -> Vec<f32> {
    (start + 1..start + 1 + n as u64).map(|v| v as f32).collect()
}

fn run(s: XSim) -> XOut {
    let cfg = PipelineConfig { out_period: s.period, out_margin: 96, ref_period: s.reference.then_some(480), max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    if s.adaptive {
        p = p.with_adaptive_target();
    }
    let mut sink = EventRender::new(XDev::new(s.period, s.ppm), RING_FRAMES);
    p.prefill(&mut sink).unwrap();
    sink.preroll().unwrap();
    sink.device_mut().start(T0);
    sink.device_mut().now = T0;
    sink.started();
    let tel = Telemetry::new();
    let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
    let mut spy = Spy(Vec::new());
    let mut ticks = FakeTicks(0);
    let (mut rows, mut blocks, mut start_silence, mut targets) = (Vec::new(), 0usize, None, Vec::new());
    let ready = |j: u64| T0 + ((j + 1) * s.packet as u64) as f64 * H + (s.late)(j) * 1e4;
    let (mut j, mut k) = (0u64, 0u64);
    let mut answer: Option<f64> = None;
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let lb_t0 = T0 - 20e4;
    let stalled = |t: f64| {
        s.stall.map_or(t, |(at, ms)| if t >= T0 + at * 1e7 && t < T0 + at * 1e7 + ms * 1e4 { T0 + at * 1e7 + ms * 1e4 } else { t })
    };
    let end = T0 + s.seconds * 1e7;
    loop {
        let (tick, mic_at) = (sink.device().next_tick, stalled(ready(j)));
        let now = tick.min(mic_at).min(answer.unwrap_or(f64::INFINITY));
        if now > end {
            break;
        }
        if now == tick {
            // The device starts its next buffer and signals; an event already pending merges with it (auto-reset).
            sink.device_mut().tick();
            if answer.is_none() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let u = (rng >> 11) as f64 / (1u64 << 53) as f64;
                answer = Some(stalled(now + (s.answer_ms + u * s.jitter_ms) * 1e4));
            }
            continue;
        }
        // The thread is awake: the device's event first (as the audio loop answers it), then the mic.
        sink.device_mut().now = now;
        if answer == Some(now) {
            sink.service().unwrap();
            answer = None;
        }
        while ready(j) <= now {
            mic.push(j * s.packet as u64, Some((T0 + (j * s.packet as u64) as f64 * H) as u64), idx1(j * s.packet as u64, s.packet));
            j += 1;
        }
        while s.reference && lb_t0 + ((k + 1) * 480) as f64 * H <= now {
            lb.push(k * 480, Some((lb_t0 + (k * 480) as f64 * H) as u64), idx1(k * 480, 480));
            k += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        if s.reference && p.block_ready() {
            p.drain_reference(&mut lb, now).unwrap();
        }
        loop {
            let head = p.mic_abs - p.mic.len() as u64;
            let t_cap = p.mic_stamps.time_of(head);
            let silence = sink.counts().0;
            if !p.process_block(&mut spy, &mut sink, &mut ticks, &tel, true).unwrap() {
                break;
            }
            start_silence.get_or_insert(silence);
            blocks += 1;
            targets.push(((now - T0) / 1e7, p.out_target()));
            if let (Some(b), Some(readout), Some(tc)) = (p.last_budget, p.last_latency_ms, t_cap) {
                let truth = T0 + head as f64 * H;
                rows.push(Row { t_s: (now - T0) / 1e7, b, readout, capture_err_hns: tc - truth });
            }
            if p.second_due() {
                p.publish_second(&tel, &spy, 0, 0, 0);
            }
        }
    }
    XOut { rows, blocks, targets, start_silence: start_silence.unwrap_or(0), p, sink, spy, tel }
}

fn means(rows: &[Row], from_s: f64) -> (LatencyBudget, f64) {
    let kept: Vec<&Row> = rows.iter().filter(|r| r.t_s >= from_s).collect();
    let n = kept.len() as f64;
    let mean = |f: &dyn Fn(&LatencyBudget) -> f32| (kept.iter().map(|r| f64::from(f(&r.b))).sum::<f64>() / n) as f32;
    let b = LatencyBudget {
        mic_ms: mean(&|b| b.mic_ms),
        wait_ms: mean(&|b| b.wait_ms),
        chain_ms: mean(&|b| b.chain_ms),
        queue_ms: mean(&|b| b.queue_ms),
        render_ms: mean(&|b| b.render_ms),
    };
    (b, kept.iter().map(|r| f64::from(r.readout)).sum::<f64>() / n)
}

fn lowest_queue(rows: &[Row], from_s: f64) -> f32 {
    rows.iter().filter(|r| r.t_s >= from_s).map(|r| r.b.queue_ms).fold(f32::MAX, f32::min)
}

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

/// The engine's mic delivery against a 2 ms exclusive device: steady, the cables' bursts and one wake in five missed, each
/// at 0 and 300 ppm (300 sweeps the wakes through the device's phases). The target is a period + a block + the margin,
/// 14 ms: nothing underruns (no short ring, no late event, no stale buffer), the parts add up to the readout block for
/// block, the queue's mean is the target, and the render side is the device's buffers: one and a half periods (3 ms; the
/// shared output's was half a 10 ms period plus what the engine holds). A burst's block finds less queued, its lowest 5
/// or 8 ms under the mean, still 4 ms or more over a period.
#[test]
fn a_2_ms_exclusive_output_runs_clean_under_steady_and_bursty_delivery() {
    // How much later than steady delivery the blocks come on average, and the latest against that mean.
    let cases = [("steady", steady as Late, 0.0f32, 0.0f32), ("bursty", bursty, 5.0, 5.0), ("rare bursts", rare_bursts, 2.0, 8.0)];
    for (what, late, later, dip) in cases {
        for ppm in [0.0, 300.0] {
            let o = run(XSim { late, ppm, ..BASE });
            let tag = format!("{what}, {ppm} ppm");
            assert_eq!(o.p.out_target(), 96 + 480 + 96, "{tag}");
            let c = o.p.counters;
            let (inserted, late_events, events) = o.sink.counts();
            let silence = inserted - o.start_silence;
            assert_eq!((c.render_underruns, c.output_drops, silence, late_events, o.sink.device().stale), (0, 0, 0, 0, 0), "{tag}: {c:?}");
            assert!(events as f64 > o.blocks as f64 * 4.9, "{tag}: an event a period: {events} for {} blocks", o.blocks);
            assert!(o.rows.len() * 100 >= o.blocks * 98, "{tag}: {} of {} blocks had a budget", o.rows.len(), o.blocks);
            for r in &o.rows {
                assert!(near(r.b.readout_ms(), r.readout, 1e-3), "{tag}: {r:?}");
            }
            let (b, readout) = means(&o.rows, SETTLED_S);
            assert!(near(b.queue_ms, 14.0, 0.4), "{tag}: {b:?}");
            // Between one and two periods; their middle once the drift has swept the wakes through the device's phases
            // (at 300 ppm a 2 ms period's phase sweeps in 7 s). With no drift the phase stays where it started.
            assert!(b.render_ms > 1.9 && b.render_ms < 4.1, "{tag}: {b:?}");
            if ppm != 0.0 {
                assert!(near(b.render_ms, 3.0, 0.4), "{tag}: {b:?}");
            }
            let mic = 10.5 + later;
            assert!(near(b.mic_ms + b.wait_ms, mic, 0.3), "{tag}: {b:?}");
            assert!((readout - f64::from(mic + 14.0 + b.render_ms)).abs() < 0.7, "{tag}: readout {readout}");
            let low = lowest_queue(&o.rows, SETTLED_S);
            assert!(low > 14.0 - dip - 1.2 && low > 4.0, "{tag}: lowest {low}");
        }
    }
}

/// The thread stalled for 5 ms (it answers no event for two and a half periods): the device replays a stale buffer, the
/// sink counts its late event as an underrun (one), and the readout stays where it was (the indices don't move: the
/// device's stream index of each frame handed is the one it would have had).
#[test]
fn a_late_answer_is_counted_and_the_readout_stays() {
    let o = run(XSim { stall: Some((20.0, 5.0)), seconds: 40.0, ..BASE });
    let (_, late_events, _) = o.sink.counts();
    assert!(o.sink.device().stale >= 1 && late_events >= 1, "stale {} late {late_events}", o.sink.device().stale);
    assert!(o.p.counters.render_underruns >= 1 && o.p.counters.render_underruns <= 3, "{:?}", o.p.counters);
    let before: Vec<Row> = o.rows.iter().filter(|r| r.t_s > 10.0 && r.t_s < 20.0).copied().collect();
    let (_, before) = means(&before, 0.0);
    // The stale periods delayed what followed them; the drift loop slips the fill back to the target.
    let (_, after) = means(&o.rows, 32.0);
    assert!((after - before).abs() < 1.0, "{before} -> {after}");
}

/// The thread stalled for 5 ms at the start, before its first answer (in the engine, the rest of the start's work): the
/// device plays its pre-roll and then two stale periods, the sink counts the late first answer (timed from the start), and
/// the stale periods are out of the stream's indices from the first block on, so the readout and the render side are a
/// clean start's (counted from the clock alone they'd read 4 ms low for the run, the render side under nothing). Before
/// the first block it's the start's, not an underrun.
#[test]
fn a_late_first_answer_leaves_the_readout_right() {
    for ppm in [0.0, 300.0] {
        let (clean, late) = (run(XSim { ppm, ..BASE }), run(XSim { ppm, stall: Some((0.0, 5.0)), ..BASE }));
        assert_eq!((late.sink.device().stale, late.sink.counts().1), (2, 1), "{ppm} ppm: two stale periods, one late answer");
        assert_eq!(late.start_silence, 2 * 96, "{ppm} ppm");
        assert_eq!(late.p.counters.render_underruns, 0, "{ppm} ppm: {:?}", late.p.counters);
        assert_eq!(late.p.counters.output_silence_frames, 0, "{ppm} ppm: nor is its silence counted");
        let ((cb, cr), (lb, lr)) = (means(&clean.rows, SETTLED_S), means(&late.rows, SETTLED_S));
        assert!((lr - cr).abs() < 0.3 && near(lb.render_ms, cb.render_ms, 0.3), "{ppm} ppm: {lb:?} {lr} against {cb:?} {cr}");
        assert!(lb.render_ms > 1.9, "{ppm} ppm: {lb:?}");
    }
}

/// A mic stall (40 ms without packets, then all of them at once): the ring runs short at a dozen device events, which is
/// one underrun, counted at the block after as the shared path counts its dry spell (not one an event), and its silence counted
/// in the output's events. The silence is
/// out of the stream's indices, so the readout comes back to where it was once the burst's excess is dropped.
#[test]
fn a_dry_spell_is_one_underrun() {
    for ppm in [0.0, 300.0] {
        let o = run(XSim { late: mic_stall, ppm, ..BASE });
        let silence = o.sink.counts().0 - o.start_silence;
        assert!(silence > 10 * 96, "{ppm} ppm: {silence} frames of silence");
        assert_eq!(o.p.counters.render_underruns, 1, "{ppm} ppm: {:?}", o.p.counters);
        // The output's events count the device's silence, every frame of it (TODO 9).
        assert_eq!(o.p.counters.output_silence_frames, silence, "{ppm} ppm: {:?}", o.p.counters);
        assert!(o.p.counters.output_drops <= 1, "{ppm} ppm: {:?}", o.p.counters);
        let rows = |from: f64, to: f64| o.rows.iter().filter(|r| r.t_s >= from && r.t_s < to).copied().collect::<Vec<_>>();
        let ((_, before), (after_b, after)) = (means(&rows(20.0, 30.0), 0.0), means(&rows(36.0, 40.0), 0.0));
        assert!((after - before).abs() < 1.0 && after_b.render_ms > 1.5, "{ppm} ppm: {before} -> {after} ({after_b:?})");
    }
}

/// The exclusive mic's packets: 144 frames (3 ms, VB-CABLE's capture minimum) don't tile the block, 160 (3.33 ms, the
/// engine's choice) do. Either way every block is whole and in order, and its first sample's capture time is exact by the
/// stamps. Its mic delivery is the rest of the packet its first sample is in, and the wake's lag (with 144 frames a block
/// starts at the packet's start, two thirds or a third into it, in turn: 2 ms on average; with 160, at the start:
/// 3.33 ms), and the wait the rest of the block: 144-frame packets finish a block 1 ms late on average (2 ms at most),
/// 160-frame ones on time. With a loopback 20 ms ahead, the reference's lead (which it reads against the mic's stamps)
/// settles on its target and the echo path holds still.
#[test]
fn the_exclusive_mic_s_small_packets_assemble_blocks_with_their_stamps() {
    for (packet, mic_ms, extra_ms) in [(144usize, 2.0f32, 1.0f32), (160, 10.0 / 3.0, 0.0)] {
        let mut o = run(XSim { packet, late: |_| 0.3, reference: true, seconds: 30.0, ..BASE });
        let tag = format!("{packet}-frame packets");
        assert_eq!((o.p.counters.capture_glitches, o.p.counters.render_underruns), (0, 0), "{tag}: {:?}", o.p.counters);
        // Whole, in order, from the first sample.
        for (b, &(m, _)) in o.spy.0.iter().enumerate() {
            assert_eq!(m, (b * 480 + 1) as f32, "{tag}: block {b}");
        }
        assert!(o.rows.iter().all(|r| r.capture_err_hns.abs() < 2.0), "{tag}: a capture time off its sample");
        let (b, _) = means(&o.rows, SETTLED_S);
        assert!(near(b.mic_ms, mic_ms + 0.3, 0.05), "{tag}: {b:?}");
        assert!(near(b.mic_ms + b.wait_ms, 10.3 + extra_ms, 0.15), "{tag}: {b:?}");
        // The echo reference: on its stamps, at its target, the path still after the start-up.
        o.p.publish_second(&o.tel, &o.spy, 0, 0, 0);
        assert!(o.tel.reference_stamped.load(Relaxed), "{tag}");
        assert!(near(o.tel.reference_fill_ms.load(), o.tel.reference_target_ms.load(), 0.5), "{tag}");
        let path: Vec<f32> = o.spy.0.iter().skip(500).map(|&(m, r)| m - r).collect();
        assert!(path.windows(2).all(|w| (w[1] - w[0]).abs() <= 1.0), "{tag}: the echo path moved");
        assert_eq!((o.p.counters.reference_gaps, o.p.counters.reference_drops), (0, 0), "{tag}: {:?}", o.p.counters);
    }
}

/// Exclusive on both sides, 160-frame mic packets and a 2 ms output: 10.3 ms to the write, 14 ms queued, 3 ms after it.
/// With the adaptive target on and steady delivery, after the two-minute window the target walks down to the floor (a
/// period + the margin, 4 ms) + the wake jitter seen + 1 ms, the queue follows it without going under it (a target set in
/// one step drove the queue 1.4 ms under it for 10 s, under the floor), and still nothing underruns.
#[test]
fn exclusive_on_both_sides_and_the_adaptive_target() {
    let both = XSim { packet: 160, late: |_| 0.3, ..BASE };
    let (b, readout) = means(&run(both).rows, SETTLED_S);
    assert!((readout - 27.3).abs() < 0.6, "{readout}: {b:?}");
    for ppm in [0.0, 300.0] {
        let o = run(XSim { adaptive: true, ppm, seconds: 240.0, ..both });
        let target_ms = o.p.out_target() as f32 / 48.0;
        assert!(target_ms > 4.9 && target_ms < 6.5, "{ppm} ppm: {target_ms}");
        assert!(near(o.tel.output_target_ms.load(), target_ms, 1e-3), "published");
        let (late, late_readout) = means(&o.rows, 200.0);
        assert!(near(late.queue_ms, target_ms, 0.3), "{ppm} ppm: {late:?}");
        if ppm != 0.0 {
            assert!((late_readout - (readout - f64::from(14.0 - target_ms))).abs() < 0.8, "{late_readout} against {readout}");
        }
        // From the lowering on, the queue's 1 s means follow the walk, within 1.5 ms (behind it as it starts, ahead of it
        // as it slows: the loop's lag), and never go under where it ends.
        let (mut lowest, mut behind, mut ahead) = (f32::MAX, f32::MIN, f32::MIN);
        for second in 120..240 {
            let rows: Vec<Row> = o.rows.iter().filter(|r| r.t_s >= second as f64 && r.t_s < (second + 1) as f64).copied().collect();
            let (m, _) = means(&rows, 0.0);
            let target_then = o.targets.iter().rev().find(|(t, _)| *t <= second as f64 + 1.0).map_or(0.0, |x| x.1 as f32 / 48.0);
            (behind, ahead) = (behind.max(m.queue_ms - target_then), ahead.max(target_then - m.queue_ms));
            lowest = lowest.min(m.queue_ms);
        }
        assert!(behind < 1.5 && ahead < 1.5, "{ppm} ppm: the queue {behind} ms behind the walk at most, {ahead} ahead");
        assert!(lowest > target_ms - 0.15, "{ppm} ppm: the queue's lowest 1 s mean {lowest} against {target_ms}");
        let under = o.targets.iter().map(|x| x.1).collect::<Vec<_>>();
        assert!(under.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] <= 24), "{ppm} ppm: a walk down");
        let (inserted, late_events, _) = o.sink.counts();
        assert_eq!((o.p.counters.render_underruns, inserted - o.start_silence, late_events), (0, 0, 0), "{:?}", o.p.counters);
    }
}

/// The exclusive output, the adaptive target, the mic and the reference over 2000 wakes: nothing allocates.
#[test]
fn the_exclusive_path_never_allocates() {
    let cfg = PipelineConfig { out_period: 96, out_margin: 96, ref_period: Some(480), max_packet: 960 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new())
        .with_adaptive_target();
    let mut sink = EventRender::new(XDev::new(96, 50.0), RING_FRAMES);
    p.prefill(&mut sink).unwrap();
    sink.preroll().unwrap();
    sink.device_mut().start(T0);
    sink.device_mut().now = T0;
    sink.started();
    let tel = Telemetry::new();
    let mut chain = Chain::new(Arc::new(ChainControls::new(&ProcessingConfig::default())), None, None);
    chain.activate().unwrap();
    let mut mic = GenCapture { pos: 0, ready: false, stamp_hns: 0.0 };
    let mut lb = GenCapture { pos: 0, ready: false, stamp_hns: 0.0 };
    let mut ticks = FakeTicks(0);
    let mut one = |p: &mut Pipeline, sink: &mut EventRender<XDev>, w: usize| {
        // Five device events a block, the mic's packet at the fifth.
        for _ in 0..5 {
            let t = sink.device().next_tick;
            sink.device_mut().tick();
            sink.device_mut().now = t + 500.0;
            sink.service().unwrap();
        }
        let now = sink.device().now;
        mic.ready = true;
        mic.stamp_hns = now - 100_000.0;
        lb.ready = true;
        lb.stamp_hns = now - 100_000.0;
        p.drain_mic(&mut mic, now).unwrap();
        p.drain_reference(&mut lb, now).unwrap();
        while p.process_block(&mut chain, sink, &mut ticks, &tel, true).unwrap() {
            if p.second_due() {
                p.publish_second(&tel, &chain, 0, 0, 0);
            }
        }
        w
    };
    for w in 0..20 {
        one(&mut p, &mut sink, w);
    }
    let ((), allocs, frees) = crate::test_alloc::count(|| {
        for w in 0..2000 {
            one(&mut p, &mut sink, w);
        }
    });
    assert_eq!((allocs, frees), (0, 0));
    assert_eq!(p.counters.render_underruns, 0, "{:?}", p.counters);
}
