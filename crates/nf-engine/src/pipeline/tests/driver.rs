//! The direct driver route against the driver's capture passes (TODO 13; the owner, 2026-11-24): the readout's sample time
//! and delivery time against what the recording app's stamps and its packets' passes say, at the recording apps' periods
//! (IAudioClient3's 2.67 ms to the shared default 10 ms), and the ring's headroom: one reader packet where it runs clean,
//! raised by the driver's real underruns, lowered again after clean running.
//!
//! [`DriverSink`] runs the live ring as StreamEngine.cpp's AppRingCapturePass does: one pass a packet on the stream's time
//! line (each packet stamped at its start, the deadline of the pass before), each pass a little late (its timer and the ring
//! lock), copying what the ring holds up to a packet and filling the rest with silence, counted (read_frame stops at
//! write_frame). The header's clock is the last pass's read_frame and QPC. The mic delivers 10 ms packets on its own clock,
//! a block a wake (jittered), or two blocks every other wake (the VM's bursty mic), with late wakes where a scene asks.

use std::collections::VecDeque;

use super::*;
use crate::ring_headroom::{HOLD_S, MAX, STEP};

const T0: f64 = 1e10;

/// The driver's live ring and its reader.
struct DriverSink {
    /// The reader's packet (frames); 0 while no app records.
    packet: usize,
    /// A pass has run since the reader started: the header's READING (until then the app sees no reader).
    reading: bool,
    write: u64,
    read: u64,
    /// The next pass's deadline and the time it actually runs (hns).
    next_deadline: f64,
    next_actual: f64,
    /// The last pass: read_frame after it and its QPC (the header's clock).
    last_pass: Option<(u64, f64)>,
    now: f64,
    /// Each pass runs up to this late (uniform), hns.
    eps_max_hns: f64,
    rng: u64,
    underrun_frames: u64,
    taken: u64,
    missing: u64,
    /// The frames whose times a readout predicted (index, sample time, delivery time), and each one's error against the pass
    /// that read it (predicted less true, hns): sample time, delivery time. Off for the allocation test.
    watch: Option<VecDeque<(u64, f64, f64)>>,
    errors: Vec<(f64, f64)>,
}

impl DriverSink {
    fn new(eps_max_ms: f64) -> Self {
        Self {
            packet: 0,
            reading: false,
            write: 0,
            read: 0,
            next_deadline: f64::INFINITY,
            next_actual: f64::INFINITY,
            last_pass: None,
            now: T0,
            eps_max_hns: eps_max_ms * 1e4,
            rng: 0x9e37_79b9_7f4a_7c15,
            underrun_frames: 0,
            taken: 0,
            missing: 0,
            watch: Some(VecDeque::new()),
            errors: Vec::new(),
        }
    }

    fn uniform(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn packet_hns(&self) -> f64 {
        self.packet as f64 * HNS_PER_SAMPLE
    }

    /// A recording app starts at `t` with `packet`-frame packets: the live discard (nothing queued before it plays), and its
    /// first pass a packet later.
    fn start_reader(&mut self, packet: usize, t: f64) {
        self.packet = packet;
        self.reading = false;
        self.read = self.write;
        self.next_deadline = t + self.packet_hns();
        self.next_actual = self.next_deadline + self.uniform() * self.eps_max_hns;
    }

    /// It stops: no passes, no reader.
    fn stop_reader(&mut self) {
        self.packet = 0;
        self.reading = false;
        self.next_deadline = f64::INFINITY;
        self.next_actual = f64::INFINITY;
    }

    /// Runs the passes due by `t`.
    fn advance_to(&mut self, t: f64) {
        while self.next_actual <= t {
            let p = self.packet as u64;
            let copy = (self.write - self.read).min(p);
            // This packet's stamp: its start, a packet before its deadline; ring frame `read + i` is stamped `i` frames on.
            let start = self.next_deadline - self.packet_hns();
            let (read, actual) = (self.read, self.next_actual);
            if let Some(w) = self.watch.as_mut() {
                while let Some(&(index, sample, delivery)) = w.front().filter(|f| f.0 < read + copy) {
                    w.pop_front();
                    // Written before a recording app stopped, and discarded at the next one's start: never delivered.
                    if index < read {
                        continue;
                    }
                    let true_sample = start + (index - read) as f64 * HNS_PER_SAMPLE;
                    self.errors.push((sample - true_sample, delivery - actual));
                }
            }
            self.underrun_frames += p - copy;
            self.read += copy;
            self.reading = true;
            self.last_pass = Some((self.read, actual));
            self.next_deadline += self.packet_hns();
            self.next_actual = self.next_deadline + self.uniform() * self.eps_max_hns;
        }
        self.now = t;
    }
}

impl RenderSink for DriverSink {
    fn driver_packet(&self) -> Option<usize> {
        Some(if self.reading { self.packet } else { 0 })
    }
    fn buffer_frames(&self) -> usize {
        8192
    }
    fn padding(&mut self) -> Result<usize, Hr> {
        Ok((self.write - self.read) as usize)
    }
    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        assert!((self.write - self.read) as usize + mono.len() <= 8192, "must fit");
        self.write += mono.len() as u64;
        Ok(())
    }
    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        assert!((self.write - self.read) as usize + frames <= 8192, "must fit");
        self.write += frames as u64;
        Ok(())
    }
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        Ok(self.last_pass.filter(|_| self.reading).map(|(pos, qpc)| ClockReading {
            pos_frames: pos as f64,
            qpc_hns: qpc,
            now_hns: self.now,
        }))
    }
    fn take_underruns(&mut self) -> u64 {
        self.missing = self.underrun_frames - self.taken;
        self.taken = self.underrun_frames;
        u64::from(self.missing > 0)
    }
    fn take_silence(&mut self) -> u64 {
        std::mem::take(&mut self.missing)
    }
    fn take_late(&mut self) -> u64 {
        0
    }
}

/// How the mic delivers in a stretch of a scene.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mic {
    /// A block a wake, each wake up to `jitter_ms` late.
    Steady,
    /// Two blocks every other wake (the first of them a block late).
    Bursty,
}

#[derive(Clone, Debug)]
struct Scene {
    /// The mic's clock against the stream's (the driver's passes run on QPC at the nominal rate).
    ppm: f64,
    jitter_ms: f64,
    eps_max_ms: f64,
    /// The mic's delivery from each time on (s), the first from the start.
    mic: Vec<(f64, Mic)>,
    /// Wakes this many ms late, at these times (s).
    late_wakes: Vec<(f64, f64)>,
    /// The recording apps: (start s, stop s, packet), one after another.
    readers: Vec<(f64, f64, usize)>,
    run_s: f64,
}

impl Scene {
    fn steady(packet: usize, run_s: f64) -> Self {
        Scene {
            ppm: 0.0,
            jitter_ms: 1.0,
            eps_max_ms: 0.3,
            mic: vec![(0.0, Mic::Steady)],
            late_wakes: Vec::new(),
            readers: vec![(0.5, f64::INFINITY, packet)],
            run_s,
        }
    }
}

/// Once a second: the time (s), the headroom and the target (frames).
#[derive(Clone, Copy, Debug)]
struct Second {
    t_s: f64,
    headroom: usize,
    target: usize,
}

struct Run {
    p: Pipeline,
    sink: DriverSink,
    tel: Telemetry,
    seconds: Vec<Second>,
    /// The times (s) of the blocks that counted an underrun, and the silence each found (frames).
    underruns: Vec<(f64, u32)>,
    /// Each block's readout and its delivery readout (ms), by its time (s).
    readouts: Vec<(f64, f32, f32)>,
}

impl Run {
    /// The readout's median over a stretch (s), and the delivery's.
    fn medians(&self, from_s: f64, to_s: f64) -> (f64, f64) {
        let mut r: Vec<f64> = self.readouts.iter().filter(|x| x.0 >= from_s && x.0 < to_s).map(|x| f64::from(x.1)).collect();
        let mut d: Vec<f64> = self.readouts.iter().filter(|x| x.0 >= from_s && x.0 < to_s).map(|x| f64::from(x.2)).collect();
        r.sort_by(f64::total_cmp);
        d.sort_by(f64::total_cmp);
        (r[r.len() / 2], d[d.len() / 2])
    }

    fn underruns_between(&self, from_s: f64, to_s: f64) -> usize {
        self.underruns.iter().filter(|u| u.0 >= from_s && u.0 < to_s).count()
    }

    fn headroom_at(&self, t_s: f64) -> usize {
        self.seconds.iter().rev().find(|s| s.t_s <= t_s).map_or(0, |s| s.headroom)
    }
}

fn run(s: &Scene) -> Run {
    let cfg = PipelineConfig { out_period: BLOCK, out_margin: 0, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(0.0), SlipResampler::new(), SlipResampler::new());
    p.for_driver_ring();
    let mut sink = DriverSink::new(s.eps_max_ms);
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut ticks = FakeTicks(0);
    let pkt_hns = BLOCK as f64 * 1e7 / (48_000.0 * (1.0 + s.ppm * 1e-6));
    let mut rng: u64 = 0x1234_5678_9abc_def1;
    let (mut seconds, mut underruns, mut readouts) = (Vec::new(), Vec::new(), Vec::new());
    let mut late = s.late_wakes.clone();
    let mut readers: VecDeque<(f64, f64, usize)> = s.readers.iter().copied().collect();
    let mut reader_on = false;
    let mut j: u64 = 0;
    let mut wake = T0 + pkt_hns + 5_000.0;
    while wake < T0 + s.run_s * 1e7 {
        let t_s = (wake - T0) / 1e7;
        let delivery = s.mic.iter().rev().find(|m| m.0 <= t_s).map_or(Mic::Steady, |m| m.1);
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let u = (rng >> 11) as f64 / (1u64 << 53) as f64;
        let mut now = wake + u * s.jitter_ms * 1e4;
        if let Some(&(at, ms)) = late.first()
            && t_s >= at
        {
            now += ms * 1e4;
            late.remove(0);
        }
        // The recording app starts and stops when the scene says.
        if let Some(&(start, stop, packet)) = readers.front() {
            if !reader_on && t_s >= start {
                sink.advance_to(T0 + start * 1e7);
                sink.start_reader(packet, T0 + start * 1e7);
                reader_on = true;
            } else if reader_on && t_s >= stop {
                sink.advance_to(T0 + stop * 1e7);
                sink.stop_reader();
                reader_on = false;
                readers.pop_front();
            }
        }
        sink.advance_to(now);
        while T0 + (j + 1) as f64 * pkt_hns <= now {
            mic.push(j * BLOCK as u64, Some((T0 + j as f64 * pkt_hns) as u64), vec![0.1; BLOCK]);
            j += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        while p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, true).unwrap() {
            let o = p.last_output;
            if o.underrun {
                underruns.push((t_s, o.silence));
            }
            if let Some(b) = p.last_budget
                && o.target > 0
            {
                readouts.push((t_s, b.readout_ms(), b.delivered_ms()));
                // The block's first sample's times as the readout predicted them, against the pass that reads it.
                let clock = sink.clock().ok().flatten();
                if let (Some(w), Some(c)) = (sink.watch.as_mut(), clock) {
                    let sample = c.qpc_hns + (o.index as f64 - c.pos_frames) * HNS_PER_SAMPLE;
                    w.push_back((o.index, sample, driver_delivery_hns(c, o.index, o.packet as usize)));
                }
            }
            if p.second_due() {
                p.publish_second(&tel, &CopyChain, 0, 0, 0);
                seconds.push(Second { t_s, headroom: p.headroom(), target: p.out_target() });
            }
        }
        // Two blocks every other wake: the mic's driver holds the first back.
        let next = if delivery == Mic::Bursty && j.is_multiple_of(2) { j + 2 } else { j + 1 };
        wake = T0 + next as f64 * pkt_hns + 5_000.0;
    }
    Run { p, sink, tel, seconds, underruns, readouts }
}

/// A steady mic at every recording app's period, its clock on the stream's or 150 ppm off: no underrun after the start (its
/// first passes find the ring empty, before the starting fill), no headroom, the target one reader packet. The readout's
/// sample time is the recording app's stamp to within the passes' own lateness (0.3 ms here), and its delivery the pass that
/// reads the frame to within two passes' lateness; the delivery readout is later than the readout, block for block, by
/// what's left of the packet (a packet at most, so it follows the period).
#[test]
fn a_steady_mic_runs_clean_at_one_reader_packet() {
    for packet in [128usize, 160, 240, 480] {
        for ppm in [0.0, 150.0] {
            let scene = Scene { ppm, ..Scene::steady(packet, 30.0) };
            let r = run(&scene);
            let what = format!("{packet} frames, {ppm} ppm");
            assert_eq!(r.p.counters.render_underruns, 0, "{what}: {:?}", r.underruns);
            assert_eq!((r.p.headroom(), r.p.out_target()), (0, packet), "{what}");
            assert!(r.seconds.iter().all(|s| s.headroom == 0 && s.target == packet), "{what}");
            assert_eq!(r.p.counters.output_deficits, 0, "{what}");
            assert!(r.sink.errors.len() > 2_000, "{what}: {}", r.sink.errors.len());
            for &(sample, delivery) in &r.sink.errors {
                assert!((-1.0..=3_000.0 + 1.0).contains(&sample), "{what}: the sample time {sample} hns off");
                assert!(delivery.abs() <= 3_000.0 + 1.0, "{what}: the delivery {delivery} hns off");
            }
            let most = packet as f32 / 48.0 + 0.31;
            assert!(r.readouts.iter().all(|&(_, a, d)| d > a && d - a <= most), "{what}: the rest of a packet at most");
            assert!(f64::from(r.tel.latency_delivery_ms_median.load()) > f64::from(r.tel.latency_ms_median.load()), "{what}");
            assert!((f64::from(r.tel.output_period_ms.load()) - packet as f64 / 48.0).abs() < 1e-3, "{what}: the active period");
            assert_eq!(r.tel.output_headroom_ms.load(), 0.0, "{what}");
            assert_eq!(r.p.last_output.packet as usize, packet, "{what}");
        }
    }
}

/// The VM's bursty mic (two blocks every other wake, the first a block late) against IAudioClient3's short packets: one
/// packet of fill can't ride that out, so the ring runs short, and the headroom rises by what the driver filled with
/// silence, a few underruns at most, within its cap; then it runs clean. Once the mic delivers steadily again, clean running
/// brings the headroom back down to nothing, a halving a hold, with no lowering failing, and the readout back where a clean
/// run's is.
#[test]
fn a_bursty_mic_raises_the_headroom_and_clean_running_lowers_it_again() {
    for packet in [128usize, 160] {
        let hold = f64::from(HOLD_S);
        let calm = 40.0;
        let scene = Scene { mic: vec![(0.0, Mic::Bursty), (calm, Mic::Steady)], ..Scene::steady(packet, calm + 6.0 * hold) };
        let r = run(&scene);
        let what = format!("{packet} frames");
        let bursty = r.underruns_between(0.0, calm);
        assert!((1..=6).contains(&bursty), "{what}: {bursty} underruns: {:?}", r.underruns);
        assert_eq!(r.underruns_between(20.0, calm), 0, "{what}: clean once raised: {:?}", r.underruns);
        let raised = r.headroom_at(calm - 1.0);
        assert!((STEP..=MAX).contains(&raised), "{what}: {raised}");
        assert_eq!(r.underruns_between(calm, f64::INFINITY), 0, "{what}: no lowering failed: {:?}", r.underruns);
        assert_eq!(r.p.headroom(), 0, "{what}: back to one packet: {:?}", r.seconds.iter().map(|s| s.headroom).collect::<Vec<_>>());
        assert_eq!(r.p.out_target(), packet, "{what}");
        // The headroom came down in halvings, each a hold after the last move.
        let moves: Vec<(f64, usize)> =
            r.seconds.windows(2).filter(|w| w[0].headroom != w[1].headroom).map(|w| (w[1].t_s, w[1].headroom)).collect();
        let downs: Vec<&(f64, usize)> = moves.iter().filter(|m| m.0 > calm).collect();
        assert!(downs.len() >= 2 && downs.windows(2).all(|w| w[1].1 < w[0].1), "{what}: {moves:?}");
        assert!(downs.windows(2).all(|w| (w[1].0 - w[0].0 - hold).abs() < 1.5), "{what}: a hold apart: {moves:?}");
        // The raised headroom shows in the readout while it's up, and is gone from it at the end.
        let (high, _) = r.medians(calm - 20.0, calm);
        let (end, _) = r.medians(calm + 6.0 * hold - 20.0, calm + 6.0 * hold);
        let clean = run(&Scene::steady(packet, 30.0)).medians(5.0, 30.0).0;
        assert!(high > end + 2.0, "{what}: {high} raised against {end} at the end");
        // Back to one packet: the latency a clean run holds, to within the fill calibration's phase (half a packet: it's
        // never seeded afresh, pipeline.rs's FillOffset).
        assert!((end - clean).abs() < packet as f64 / 96.0 + 0.3, "{what}: {end} at the end against a clean run's {clean}");
        assert_eq!(r.p.counters.output_deficits, 0, "{what}: the driver's silence, no zeros of ours");
    }
}

/// A wake 10 ms late at a 2.67 ms packet runs the ring short; the headroom keeps what that cost (the silence, rounded up to
/// a step), so the same hiccup again inside the hold is ridden out, or costs at most the rest of a packet more (the block's
/// phase against the passes differs: a packet at most); a third is ridden out. After a hold of clean running it comes down.
/// The underruns' silence is counted as the driver filled it.
#[test]
fn a_late_wake_costs_an_underrun_and_the_headroom_rides_out_the_next() {
    let late = vec![(20.0, 10.0), (35.0, 10.0), (50.0, 10.0)];
    let scene = Scene { late_wakes: late, ..Scene::steady(128, 50.0 + 2.5 * f64::from(HOLD_S)) };
    let r = run(&scene);
    assert!((1..=2).contains(&r.underruns.len()), "{:?}", r.underruns);
    let (at, silence) = r.underruns[0];
    assert!((20.0..20.1).contains(&at) && silence > 0, "{:?}", r.underruns);
    assert_eq!(r.headroom_at(30.0), (silence as usize).div_ceil(STEP) * STEP, "the silence, rounded up to a step");
    if let Some(&(at, more)) = r.underruns.get(1) {
        assert!((35.0..35.1).contains(&at) && (more as usize) < 128 + STEP, "{:?}", r.underruns);
    }
    assert_eq!(r.underruns_between(45.0, f64::INFINITY), 0, "{:?}", r.underruns);
    let raised = r.headroom_at(59.0);
    let silence: u64 = r.underruns.iter().map(|u| u64::from(u.1)).sum();
    assert_eq!(r.p.counters.output_silence_frames, silence);
    assert!(r.p.headroom() < raised, "lowered after a hold of clean running: {} from {raised}", r.p.headroom());
}

/// A recording app that starts after the engine, stops, and another that starts with another period: neither start counts
/// an underrun (its first passes find the ring empty), the idle stretch feeds nothing and counts as no clean running, and
/// the target follows each reader's packet with the headroom the run has needed carried over.
#[test]
fn recording_apps_come_and_go_without_counted_underruns() {
    let scene =
        Scene { readers: vec![(2.0, 12.0, 480), (15.0, f64::INFINITY, 160)], late_wakes: vec![(6.0, 30.0)], ..Scene::steady(480, 30.0) };
    let r = run(&scene);
    assert_eq!(r.underruns.len(), 1, "the late wake's alone: {:?}", r.underruns);
    let raised = r.headroom_at(11.0);
    assert!(raised > 0);
    // Idle: no reader, no target, nothing fed, and the headroom kept.
    assert!(r.seconds.iter().filter(|s| s.t_s > 12.2 && s.t_s < 15.0).all(|s| s.target == 0 && s.headroom == raised), "{:?}", r.seconds);
    assert_eq!((r.p.out_target(), r.p.headroom()), (160 + raised, raised));
    assert_eq!(r.p.last_output.packet, 160);
    assert_eq!(r.p.counters.output_deficits, 0);
}

/// The direct route's per-block path allocates nothing (CLAUDE.md's real-time rule): the ring's passes, its underruns, the
/// headroom's moves and the readout's two times.
#[test]
fn the_direct_route_never_allocates() {
    let cfg = PipelineConfig { out_period: BLOCK, out_margin: 0, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(0.0), SlipResampler::new(), SlipResampler::new());
    p.for_driver_ring();
    let mut sink = DriverSink::new(0.3);
    sink.watch = None;
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    mic.packets.reserve(64);
    let mut ticks = FakeTicks(0);
    let pkt = 100_000.0;
    sink.start_reader(128, T0);
    let mut j: u64 = 0;
    let mut step = |p: &mut Pipeline, sink: &mut DriverSink, mic: &mut FakeCapture, k: u64| {
        let now = T0 + (k + 1) as f64 * pkt + 5_000.0;
        sink.advance_to(now);
        // Every 40th wake missed (the next brings two blocks): the ring runs short and the headroom moves.
        if k % 40 == 39 {
            return;
        }
        while T0 + (j + 1) as f64 * pkt <= now {
            let qpc_hns = Some((T0 + j as f64 * pkt) as u64);
            // No samples: the packet's are the buffer's own (no allocation for them, nor a free).
            mic.packets.push_back((Packet { frames: BLOCK, device_pos: j * BLOCK as u64, qpc_hns, discontinuity: false }, Vec::new()));
            j += 1;
        }
        p.drain_mic(mic, now).unwrap();
        while p.process_block(&mut CopyChain, sink, &mut ticks, &tel, true).unwrap() {
            if p.second_due() {
                p.publish_second(&tel, &CopyChain, 0, 0, 0);
            }
        }
    };
    for k in 0..500 {
        step(&mut p, &mut sink, &mut mic, k);
    }
    let ((), allocs, frees) = crate::test_alloc::count(|| {
        for k in 500..3_000 {
            step(&mut p, &mut sink, &mut mic, k);
        }
    });
    assert_eq!((allocs, frees), (0, 0));
    assert!(p.counters.render_underruns > 0 && p.headroom() > 0, "the path with its underruns: {:?}", p.counters);
}
