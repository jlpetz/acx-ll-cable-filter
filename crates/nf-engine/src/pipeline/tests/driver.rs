//! The direct driver route against the driver's capture passes (TODO 13; the owner, 2026-11-24): the readout's sample time
//! and delivery time against what the recording app's stamps and its packets' passes say, at the recording apps' periods
//! (IAudioClient3's 2.67 ms to the shared default 10 ms), and the ring's headroom: one reader packet where it runs clean,
//! raised by the driver's real underruns, lowered again after clean running.
//!
//! [`DriverSink`] runs the live ring as StreamEngine.cpp's AppRingCapturePass does: one pass a packet on the stream's time
//! line (each packet stamped at its start, the deadline of the pass before), each pass a little late (its timer and the ring
//! lock: [`Lateness`]), copying what the ring holds up to a packet and filling the rest with silence, counted (read_frame
//! stops at write_frame); a recording app's first pass discards what was queued (the live discard). The header's clock is
//! the last pass's read_frame and QPC. The mic delivers 10 ms packets on its own clock, a block a wake (jittered), or two
//! blocks every other wake (the VM's bursty mic), with late wakes where a scene asks.

use std::collections::VecDeque;

use super::*;
use crate::ring_headroom::{HOLD_S, MAX_FAILED, STEP};

const T0: f64 = 1e10;

/// How late each capture pass runs after its deadline.
#[derive(Clone, Copy, Debug)]
enum Lateness {
    /// Uniform from 0 to this many ms.
    Uniform(f64),
    /// As TODO 19's spike measured the driver's timers on the dev box under Verifier (2026-11-20: mean 310-353 us; max
    /// 0.8 ms over runs of minutes, 1.4-1.6 ms over an hour at 128 frames): 0.25 ms and an exponential tail of mean 0.08 ms,
    /// and one pass in a thousand 0.8 to 1.5 ms late. Mean about 0.33 ms, at most [`MEASURED_MAX_MS`].
    Measured,
}

const MEASURED_MAX_MS: f64 = 1.5;

impl Lateness {
    fn max_ms(self) -> f64 {
        match self {
            Lateness::Uniform(ms) => ms,
            Lateness::Measured => MEASURED_MAX_MS,
        }
    }
}

/// The driver's live ring and its reader.
struct DriverSink {
    /// The reader's packet (frames); 0 while no app records.
    packet: usize,
    /// A pass has run since the reader started: the header's READING (until then the app sees no reader).
    reading: bool,
    write: u64,
    read: u64,
    /// The frames copied into packets (the header's frames_copied), and those the live discards dropped.
    copied: u64,
    discarded: u64,
    /// `read - copied` at the last [`RenderSink::driver_reader`], and whether a move in it is reported as a restart (off: as
    /// the pipeline saw the driver before it read the move, for the scene that shows what the restart is for).
    moved: Option<u64>,
    report_restarts: bool,
    /// The next pass's deadline and the time it actually runs (hns).
    next_deadline: f64,
    next_actual: f64,
    /// The last pass: read_frame after it and its QPC (the header's clock).
    last_pass: Option<(u64, f64)>,
    now: f64,
    lateness: Lateness,
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
    fn new(lateness: Lateness) -> Self {
        Self {
            packet: 0,
            reading: false,
            write: 0,
            read: 0,
            copied: 0,
            discarded: 0,
            moved: None,
            report_restarts: true,
            next_deadline: f64::INFINITY,
            next_actual: f64::INFINITY,
            last_pass: None,
            now: T0,
            lateness,
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

    /// The next pass's lateness (hns).
    fn late_hns(&mut self) -> f64 {
        let ms = match self.lateness {
            Lateness::Uniform(ms) => self.uniform() * ms,
            Lateness::Measured => {
                if self.uniform() < 1e-3 {
                    0.8 + 0.7 * self.uniform()
                } else {
                    (0.25 - 0.08 * (1.0 - self.uniform()).ln()).min(MEASURED_MAX_MS)
                }
            }
        };
        ms * 1e4
    }

    fn packet_hns(&self) -> f64 {
        self.packet as f64 * HNS_PER_SAMPLE
    }

    /// A recording app starts at `t` with `packet`-frame packets: its first pass a packet later, which discards what's queued
    /// (the live discard: nothing written before it plays).
    fn start_reader(&mut self, packet: usize, t: f64) {
        self.packet = packet;
        self.reading = false;
        self.next_deadline = t + self.packet_hns();
        self.next_actual = self.next_deadline + self.late_hns();
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
            if !self.reading {
                self.discarded += self.write - self.read;
                self.read = self.write;
            }
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
            self.copied += copy;
            self.reading = true;
            self.last_pass = Some((self.read, actual));
            self.next_deadline += self.packet_hns();
            self.next_actual = self.next_deadline + self.late_hns();
        }
        self.now = t;
    }
}

impl RenderSink for DriverSink {
    fn driver_reader(&mut self) -> Option<DriverReader> {
        let moved = self.read - self.copied;
        let restarted = self.report_restarts && self.moved.is_some_and(|m| m != moved);
        self.moved = Some(moved);
        Some(DriverReader { packet: if self.reading { self.packet } else { 0 }, restarted })
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
    lateness: Lateness,
    /// The mic's delivery from each time on (s), the first from the start.
    mic: Vec<(f64, Mic)>,
    /// Wakes this many ms late, at these times (s).
    late_wakes: Vec<(f64, f64)>,
    /// The recording apps: (start s, stop s, packet), one after another.
    readers: Vec<(f64, f64, usize)>,
    /// Whether the sink reports the ring's restarts (off for the scene that shows what they're for).
    report_restarts: bool,
    run_s: f64,
}

impl Scene {
    fn steady(packet: usize, run_s: f64) -> Self {
        Scene {
            ppm: 0.0,
            jitter_ms: 1.0,
            lateness: Lateness::Uniform(0.3),
            mic: vec![(0.0, Mic::Steady)],
            late_wakes: Vec::new(),
            readers: vec![(0.5, f64::INFINITY, packet)],
            report_restarts: true,
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

    /// The headroom's moves, second by second: (time s, the headroom after).
    fn moves(&self) -> Vec<(f64, usize)> {
        self.seconds.windows(2).filter(|w| w[0].headroom != w[1].headroom).map(|w| (w[1].t_s, w[1].headroom)).collect()
    }
}

fn run(s: &Scene) -> Run {
    let cfg = PipelineConfig { out_period: BLOCK, out_margin: 0, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(0.0), SlipResampler::new(), SlipResampler::new());
    p.for_driver_ring();
    let mut sink = DriverSink::new(s.lateness);
    sink.report_restarts = s.report_restarts;
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
        // The recording apps start and stop when the scene says, in order: a stop and the next start can both fall between
        // two wakes, so that no block sees the ring without a reader.
        while let Some(&(start, stop, packet)) = readers.front() {
            if !reader_on && t_s >= start {
                sink.advance_to(T0 + start * 1e7);
                sink.start_reader(packet, T0 + start * 1e7);
                reader_on = true;
            } else if reader_on && t_s >= stop {
                sink.advance_to(T0 + stop * 1e7);
                sink.stop_reader();
                reader_on = false;
                readers.pop_front();
            } else {
                break;
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

/// The sample time's and the delivery's errors against the passes, each within the lateness's bounds: the sample time late
/// by the clock's pass's lateness (from 0 to its most), the delivery off by the clock's pass's less the reading one's (or
/// early by the reading one's, where the reading found it overdue). Returns the mean sample time error (ms).
fn check_errors(r: &Run, lateness: Lateness, what: &str) -> f64 {
    let late_hns = lateness.max_ms() * 1e4;
    assert!(r.sink.errors.len() > 2_000, "{what}: {}", r.sink.errors.len());
    for &(sample, delivery) in &r.sink.errors {
        assert!((-1.0..=late_hns + 1.0).contains(&sample), "{what}: the sample time {sample} hns off");
        assert!(delivery.abs() <= late_hns + 1.0, "{what}: the delivery {delivery} hns off");
    }
    r.sink.errors.iter().map(|e| e.0).sum::<f64>() / r.sink.errors.len() as f64 * 1e-4
}

/// A steady mic at every recording app's period, its clock on the stream's or 150 ppm off: no underrun after the start (its
/// first passes find the ring empty, before the starting fill), no headroom, the target one reader packet. The readout's
/// sample time is the recording app's stamp read late by the clock's pass's own lateness (0 to 0.3 ms here: the error is
/// that), and its delivery the pass that reads the frame to within the passes' lateness (the clock's pass's less the
/// reading one's); the delivery readout is later than the readout, block for block, by what's left of the packet (a
/// packet at most, so it follows the period).
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
            let mean = check_errors(&r, scene.lateness, &what);
            assert!((0.12..0.18).contains(&mean), "{what}: the sample time {mean} ms late on average (the lateness's mean)");
            let most = packet as f32 / 48.0 + 0.31;
            assert!(r.readouts.iter().all(|&(_, a, d)| d > a && d - a <= most), "{what}: the rest of a packet at most");
            assert!(f64::from(r.tel.latency_delivery_ms_median.load()) > f64::from(r.tel.latency_ms_median.load()), "{what}");
            assert!((f64::from(r.tel.output_period_ms.load()) - packet as f64 / 48.0).abs() < 1e-3, "{what}: the active period");
            assert_eq!(r.tel.output_headroom_ms.load(), 0.0, "{what}");
            assert_eq!(r.p.last_output.packet as usize, packet, "{what}");
        }
    }
}

/// The same with the passes as late as the driver's timers were on the dev box under Verifier ([`Lateness::Measured`]:
/// mean about 0.33 ms, up to 1.5): still clean at one packet at every period, and the sample time reads late by the clock's
/// pass's lateness, 0.33 ms on average, up to the tail's 0.8 to 1.5 ms. So the live check's sweep less the readout (and
/// the chain) comes out at about -0.3 ms, not 0 (nf-scratch/latency/direct.sh).
#[test]
fn the_measured_pass_lateness_still_runs_clean_and_reads_that_much_late() {
    for packet in [128usize, 160, 240, 480] {
        let scene = Scene { lateness: Lateness::Measured, ..Scene::steady(packet, 30.0) };
        let r = run(&scene);
        let what = format!("{packet} frames");
        assert_eq!(r.p.counters.render_underruns, 0, "{what}: {:?}", r.underruns);
        assert_eq!((r.p.headroom(), r.p.out_target(), r.p.counters.output_deficits), (0, packet, 0), "{what}");
        let mean = check_errors(&r, scene.lateness, &what);
        assert!((0.30..0.36).contains(&mean), "{what}: the sample time {mean} ms late on average");
        let latest = r.sink.errors.iter().map(|e| e.0).fold(0.0, f64::max) * 1e-4;
        assert!(latest > 0.7, "{what}: the tail shows ({latest} ms)");
        let most = packet as f32 / 48.0 + MEASURED_MAX_MS as f32 + 0.01;
        assert!(r.readouts.iter().all(|&(_, a, d)| d > a && d - a <= most), "{what}: the rest of a packet, and a late pass");
    }
}

/// The VM's bursty mic (two blocks every other wake, the first a block late) against IAudioClient3's short packets: one
/// packet of fill can't ride that out, so the ring runs short, and the headroom rises by what the driver filled with
/// silence: two or three underruns, to 2 to 5 ms (at 128 frames three, to 5 ms; at 160 two, to 2 ms); then it runs clean.
/// Once the mic delivers steadily again, clean running brings the headroom back down to nothing, a halving a hold, in two
/// or three holds from the last raise, with no lowering failing, and the readout back where a clean run's is.
#[test]
fn a_bursty_mic_raises_the_headroom_and_clean_running_lowers_it_again() {
    for packet in [128usize, 160] {
        let hold = f64::from(HOLD_S);
        let calm = 40.0;
        let scene = Scene { mic: vec![(0.0, Mic::Bursty), (calm, Mic::Steady)], ..Scene::steady(packet, calm + 6.0 * hold) };
        let r = run(&scene);
        let what = format!("{packet} frames");
        let bursty = r.underruns_between(0.0, calm);
        assert!((2..=3).contains(&bursty), "{what}: {bursty} underruns: {:?}", r.underruns);
        assert_eq!(r.underruns_between(20.0, calm), 0, "{what}: clean once raised: {:?}", r.underruns);
        let raised = r.headroom_at(calm - 1.0);
        assert!((2 * STEP..=5 * STEP).contains(&raised), "{what}: {raised}");
        let last_raise = r.underruns.last().unwrap().0;
        assert_eq!(r.underruns_between(calm, f64::INFINITY), 0, "{what}: no lowering failed: {:?}", r.underruns);
        assert_eq!(r.p.headroom(), 0, "{what}: back to one packet: {:?}", r.moves());
        assert_eq!(r.p.out_target(), packet, "{what}");
        // The headroom came down in halvings, the first a hold after the last raise and each a hold after the one before.
        let downs: Vec<(f64, usize)> = r.moves().into_iter().filter(|m| m.0 > calm).collect();
        assert!((2..=3).contains(&downs.len()) && downs.windows(2).all(|w| w[1].1 < w[0].1), "{what}: {:?}", r.moves());
        assert!((downs[0].0 - last_raise - hold).abs() < 1.5, "{what}: a hold after the last raise: {:?}", r.moves());
        assert!(downs.windows(2).all(|w| (w[1].0 - w[0].0 - hold).abs() < 1.5), "{what}: a hold apart: {:?}", r.moves());
        assert_eq!(r.headroom_at(last_raise + downs.len() as f64 * hold + 1.5), 0, "{what}: {:?}", r.moves());
        // The raised headroom shows in the readout while it's up, and is gone from it at the end.
        let (high, _) = r.medians(calm - 20.0, calm);
        let (end, _) = r.medians(calm + 6.0 * hold - 20.0, calm + 6.0 * hold);
        let clean = run(&Scene::steady(packet, 30.0)).medians(5.0, 30.0).0;
        assert!(high > end + 1.5, "{what}: {high} raised against {end} at the end");
        // Back to one packet: the latency a clean run holds, to within the fill calibration's phase (half a packet: it's
        // never seeded afresh, pipeline.rs's FillOffset).
        assert!((end - clean).abs() < packet as f64 / 96.0 + 0.3, "{what}: {end} at the end against a clean run's {clean}");
        assert_eq!(r.p.counters.output_deficits, 0, "{what}: the driver's silence, no zeros of ours");
    }
}

/// A mic whose need never goes away (the bursty mic for the whole run): each lowering takes the headroom under what the
/// bursts need, so it fails within its hold, and goes back at once to where it came from (one underrun, not one a step).
/// After [`MAX_FAILED`] the headroom stays for the run (ring_headroom.rs; ours, proposed): the run underruns at its start
/// and once a failed lowering, none after the third, where without the bound it went on doing so every 16 minutes at
/// most (at 128 frames 21 underruns in 3000 s, three a lowering).
#[test]
fn a_mic_whose_need_never_goes_away_costs_three_failed_lowerings_at_most() {
    let hold = f64::from(HOLD_S);
    for packet in [128usize, 160] {
        let run_s = 1_500.0;
        let r = run(&Scene { mic: vec![(0.0, Mic::Bursty)], ..Scene::steady(packet, run_s) });
        let what = format!("{packet} frames: {:?}, {:?}", r.underruns, r.moves());
        let start = r.underruns_between(0.0, 20.0);
        assert!((2..=3).contains(&start), "{what}");
        let raised = r.headroom_at(20.0);
        assert!((2 * STEP..=5 * STEP).contains(&raised), "{what}");
        // Each lowering a doubled hold after the raise before it, and each failing: an underrun within a few seconds, the
        // headroom back where it was.
        let moves = r.moves();
        let downs: Vec<usize> = (0..moves.len()).filter(|&k| k > 0 && moves[k].1 < moves[k - 1].1).collect();
        assert_eq!(downs.len(), MAX_FAILED as usize, "{what}");
        let mut wait = hold;
        for &k in &downs {
            let (at, prev) = (moves[k].0, moves[k - 1]);
            assert!((at - prev.0 - wait).abs() < 1.5 && prev.1 == raised, "{what}: lowered {wait} s after the last raise");
            assert_eq!(r.underruns_between(at - 1.0, at + 5.0), 1, "{what}: one underrun at {at}");
            assert_eq!(moves.get(k + 1).map(|m| m.1), Some(raised), "{what}: back where it came from");
            wait *= 2.0;
        }
        assert_eq!(r.underruns.len(), start + MAX_FAILED as usize, "{what}");
        // The third failure and the 8-hold wait the next lowering would have had both inside the run: none came.
        let third = moves[downs[downs.len() - 1]].0;
        assert!(third + wait < run_s && r.underruns_between(third + 5.0, f64::INFINITY) == 0, "{what}");
        assert_eq!((r.p.headroom(), r.p.out_target()), (raised, packet + raised), "{what}: kept for the run");
    }
}

/// A recording app that stops and another that starts within one block (5 ms) at the same packet: no block sees the ring
/// without a reader, but the new one's first pass discards what was queued (the live discard) and finds the ring empty.
/// The pipeline sees the ring's read position move without a copy (`DriverReader::restarted`) and starts its fill again,
/// so that silence is the new start's, not an underrun: none counted, no headroom. Without the restart it was taken for a
/// real underrun (the control: at 128 frames three counted and 5 ms of headroom, at 160 one and 4 ms).
#[test]
fn a_recording_app_restarting_within_a_block_is_a_new_start() {
    for packet in [128usize, 160] {
        for report_restarts in [true, false] {
            let readers = vec![(0.5, 10.012, packet), (10.017, f64::INFINITY, packet)];
            let r = run(&Scene { readers, report_restarts, ..Scene::steady(packet, 20.0) });
            let what = format!("{packet} frames, restarts reported: {report_restarts}: {:?}, {:?}", r.underruns, r.moves());
            assert!(r.sink.discarded > packet as u64, "{what}: the second app's first pass discarded {}", r.sink.discarded);
            assert!(r.seconds.iter().all(|s| s.target > 0), "{what}: no block saw the ring without a reader");
            if report_restarts {
                assert_eq!((r.p.counters.render_underruns, r.p.headroom()), (0, 0), "{what}");
                assert_eq!((r.p.out_target(), r.p.counters.output_deficits), (packet, 0), "{what}");
            } else {
                assert!(r.p.counters.render_underruns > 0 && r.p.headroom() > 0, "{what}: the control");
            }
        }
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
    let mut sink = DriverSink::new(Lateness::Uniform(0.3));
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
