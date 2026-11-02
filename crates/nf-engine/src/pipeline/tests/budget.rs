//! The latency readout's budget (budget.rs; engine.md, "The latency budget") on the audio path: its parts add up to the
//! readout block for block, through drift, bursts, late wakes, a mic stall and the output's corrections; each part answers
//! to what it measures (a later wake raises the mic's delivery, held samples and later blocks of a wake wait, a fuller
//! output raises the queue, a device clock that holds more raises the render side, and the chain's offset sits beside the
//! readout); and the chain recorder gets each block's budget.
//!
//! The device is events.rs's `VSink`: it takes a period at each tick and its clock steps there, so it plays a sample as it
//! takes it. `Lagged` makes its clock report more held after the take, as a shared-mode engine and a device's buffer do.

use rtrb::RingBuffer;

use super::events::VSink;
use super::*;
use crate::recorder::{ChainItem, ChainTx};

const T0: f64 = 1e10;
const H: f64 = 1e7 / 48_000.0;
/// Means are taken from here on: at 300 ppm the wakes' phase against the device's ticks has swept a whole period by 33 s,
/// so the output fill's calibration has found the middle of the padding's steps (before that the queue and the render side
/// can trade up to half a period between them).
const SETTLED_S: f64 = 40.0;

/// `VSink` whose clock says it plays each sample `lag` frames after it would.
struct Lagged {
    sink: VSink,
    lag: f64,
}

impl RenderSink for Lagged {
    fn buffer_frames(&self) -> usize {
        self.sink.buffer_frames()
    }
    fn padding(&mut self) -> Result<usize, Hr> {
        self.sink.padding()
    }
    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        self.sink.write(mono)
    }
    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        self.sink.write_silence(frames)
    }
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        Ok(self.sink.clock()?.map(|c| ClockReading { pos_frames: c.pos_frames - self.lag, ..c }))
    }
}

/// Copies the mic, and reports a stream offset as a chain would.
struct Offset(u32);

impl BlockProcessor for Offset {
    fn process<T: Ticks>(&mut self, mic: &Block, _r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        *out = *mic;
        ChainReport::default()
    }
    fn aec_stats(&self) -> Option<AecStats> {
        None
    }
    fn latency(&self) -> u32 {
        self.0
    }
}

/// When a mic packet is ready, in ms after its last sample, by its number.
type Late = fn(u64) -> f64;

/// Delivered as it fills.
fn steady(_: u64) -> f64 {
    0.5
}
/// The cables' bursts at their most regular: every other packet a wake late, so the wakes get 0 and 960 frames in turn.
fn bursty(j: u64) -> f64 {
    if j.is_multiple_of(2) { 10.5 } else { 0.5 }
}
/// One packet in five a wake late: a wake with nothing, then one with two.
fn rare_bursts(j: u64) -> f64 {
    if j.is_multiple_of(5) { 10.5 } else { 0.5 }
}
/// Packets 5000-5003 (from 50 s) held back and delivered together, 40 ms on: the device runs dry, then the output overfills.
fn mic_stall(j: u64) -> f64 {
    if (5000..5004).contains(&j) { (5004 - j) as f64 * 10.0 + 0.5 } else { 0.5 }
}
/// The same stall at 200 s (packets 20000-20003), after the adaptive target has come down.
fn mic_stall_at_200(j: u64) -> f64 {
    if (20_000..20_004).contains(&j) { (20_004 - j) as f64 * 10.0 + 0.5 } else { 0.5 }
}

#[derive(Clone, Copy)]
struct Sim {
    /// The mic's packets: frames each, and when each is ready.
    packet: usize,
    late: Late,
    /// The engine wakes every `wake_ms`, `phase_ms` after each step plus up to `jitter_ms`, and reads what's ready.
    wake_ms: f64,
    phase_ms: f64,
    jitter_ms: f64,
    /// One wake (at `.0` s) this many ms late.
    late_wake: Option<(f64, f64)>,
    out_period: usize,
    /// Where the device's first tick falls in its period, as a fraction.
    device_phase: f64,
    margin: usize,
    lag: f64,
    chain: u32,
    /// The device's clock against the mic's.
    ppm: f64,
    seconds: f64,
    /// An experiment: the output target this many samples under the engine's (which isn't changed).
    target_less: usize,
    /// The adaptive output target (out_target.rs), from the engine's fixed one.
    adaptive: bool,
    /// Arm the chain recorder (its ring holds the whole run: 150 MB for four minutes, so the long runs go without).
    record: bool,
}

const BASE: Sim = Sim {
    packet: 480,
    late: steady,
    wake_ms: 10.0,
    phase_ms: 0.7,
    jitter_ms: 0.0,
    late_wake: None,
    out_period: 480,
    device_phase: 0.37,
    margin: 96,
    lag: 0.0,
    chain: 0,
    ppm: 300.0,
    seconds: 60.0,
    target_less: 0,
    adaptive: false,
    record: true,
};

/// One block's budget and readout, at its wake (s).
#[derive(Clone, Copy, Debug)]
struct Row {
    t_s: f64,
    b: LatencyBudget,
    readout: f32,
}

struct Out {
    rows: Vec<Row>,
    /// Blocks processed.
    blocks: usize,
    /// The output target after each block (samples), at its wake (s).
    targets: Vec<(f64, usize)>,
    p: Pipeline,
    tel: Telemetry,
    /// What the chain recorder got.
    recorded: Vec<ChainBlock>,
}

fn run(s: Sim) -> Out {
    let cfg = PipelineConfig { out_period: s.out_period, out_margin: s.margin, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    if s.target_less > 0 {
        p.out_target -= s.target_less;
        p.out_drift = DriftController::new(p.out_target as f32);
    }
    if s.adaptive {
        p = p.with_adaptive_target();
    }
    let (tx, mut rx) = RingBuffer::<ChainItem>::new(if s.record { (s.seconds * 100.0) as usize + 100 } else { 1 });
    if s.record {
        p.recorder = Some(ChainTx::new(tx));
    }
    let mut sink = Lagged { sink: VSink::new(s.out_period, s.ppm, s.device_phase, T0), lag: s.lag };
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let mut proc = Offset(s.chain);
    let mut ticks = FakeTicks(0);
    let (mut rows, mut blocks, mut targets) = (Vec::new(), 0, Vec::new());
    let (mut j, mut k, mut last) = (0u64, 1u64, f64::MIN);
    let mut late_wake = s.late_wake;
    let mut rng: u64 = 0x2545_f491_4f6c_dd1d;
    loop {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let jitter = (rng >> 11) as f64 / (1u64 << 53) as f64 * s.jitter_ms;
        let mut now = T0 + (k as f64 * s.wake_ms + s.phase_ms + jitter) * 1e4;
        k += 1;
        if now > T0 + s.seconds * 1e7 {
            break;
        }
        if let Some((at_s, ms)) = late_wake
            && now >= T0 + at_s * 1e7
        {
            now += ms * 1e4;
            late_wake = None;
        }
        // The wakes a late one ran past don't happen.
        if now <= last {
            continue;
        }
        last = now;
        sink.sink.advance_to(now);
        loop {
            let start = T0 + (j * s.packet as u64) as f64 * H;
            if start + s.packet as f64 * H + (s.late)(j) * 1e4 > now {
                break;
            }
            mic.push(j * s.packet as u64, Some(start as u64), vec![0.1; s.packet]);
            j += 1;
        }
        p.drain_mic(&mut mic, now).unwrap();
        while p.process_block(&mut proc, &mut sink, &mut ticks, &tel, true).unwrap() {
            blocks += 1;
            targets.push(((now - T0) / 1e7, p.out_target()));
            if let (Some(b), Some(readout)) = (p.last_budget, p.last_latency_ms) {
                rows.push(Row { t_s: (now - T0) / 1e7, b, readout });
            }
            if p.second_due() {
                p.publish_second(&tel, &proc, 0, 0, 0);
            }
        }
    }
    let mut recorded = Vec::new();
    while let Ok(item) = rx.pop() {
        recorded.push(item.block);
    }
    Out { rows, blocks, targets, p, tel, recorded }
}

/// The parts' means over the rows from `from_s` on.
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

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

/// Every block with a readout has a budget whose parts add up to it, whatever the delivery, the drift, the device's period
/// and the output's corrections (a late wake that ran the device dry, a mic stall's dry spell and then its overfull burst,
/// whole blocks dropped). The published means are the window's, and the chain recorder gets each block's budget.
#[test]
fn the_parts_add_up_to_the_readout_block_for_block() {
    #[rustfmt::skip]
    let cases: [(&str, Sim); 10] = [
        ("steady", BASE),
        ("steady, no drift", Sim { ppm: 0.0, ..BASE }),
        ("bursty", Sim { late: bursty, ..BASE }),
        ("rare bursts, -300 ppm", Sim { late: rare_bursts, ppm: -300.0, ..BASE }),
        ("half-block packets", Sim { packet: 240, wake_ms: 5.0, ..BASE }),
        ("a 1024-frame device", Sim { out_period: 1024, ..BASE }),
        ("a wake 15 ms late", Sim { late_wake: Some((30.0, 15.0)), ..BASE }),
        ("a wake 35 ms late: the device runs dry", Sim { late_wake: Some((30.0, 35.0)), ..BASE }),
        ("a mic stall: dry, then overfull", Sim { late: mic_stall, seconds: 70.0, ..BASE }),
        ("a lagging clock and a chain", Sim { lag: 700.0, chain: 912, ..BASE }),
    ];
    for (what, sim) in cases {
        let mut o = run(sim);
        assert!(o.rows.len() * 100 >= o.blocks * 98, "{what}: {} of {} blocks had a budget", o.rows.len(), o.blocks);
        for r in &o.rows {
            assert!(near(r.b.readout_ms(), r.readout, 1e-3), "{what}: at {} s, {:?} against {}", r.t_s, r.b, r.readout);
            assert!(near(r.b.total_ms(), r.readout + sim.chain as f32 / 48.0, 1e-3), "{what}: {:?}", r.b);
        }
        // The window's means, published: its last 100 blocks.
        o.p.publish_second(&o.tel, &Offset(sim.chain), 0, 0, 0);
        let last = &o.rows[o.rows.len() - 100..];
        let mean_readout = last.iter().map(|r| f64::from(r.readout)).sum::<f64>() / 100.0;
        let published = o.tel.budget_mean.load();
        assert!((f64::from(published.readout_ms()) - mean_readout).abs() < 1e-3, "{what}: {published:?} against {mean_readout}");
        assert_eq!(o.tel.budget_totals.load().blocks, o.rows.len() as u64, "{what}");
        let recorded: Vec<LatencyBudget> = o.recorded.iter().filter_map(|b| b.budget).collect();
        assert_eq!(recorded, o.rows.iter().map(|r| r.b).collect::<Vec<_>>(), "{what}: the recorder's");
        assert_eq!(o.recorded.len(), o.blocks, "{what}");
    }
}

/// Steady delivery, read at a wake 0.7 ms after each packet fills: the first sample is 10.7 ms old (the block's own 10 ms and
/// the wake's lateness), nothing waits, the queue is the output target (22 ms) and this device's render side half its period
/// (its clock steps as it takes a period, and the queue is calibrated to the middle of the steps). Wakes 5 ms later raise
/// the mic's delivery, and the readout, by 5 ms; the queue and the render side stay.
#[test]
fn a_later_wake_raises_the_mic_delivery() {
    let (early, early_readout) = means(&run(BASE).rows, SETTLED_S);
    assert!(near(early.mic_ms, 10.7, 0.01) && near(early.wait_ms, 0.0, 1e-6), "{early:?}");
    assert!(near(early.queue_ms, 22.0, 0.3) && near(early.render_ms, 5.0, 0.3), "{early:?}");
    assert!((early_readout - 37.7).abs() < 0.5, "{early_readout}");
    let (late, late_readout) = means(&run(Sim { phase_ms: 5.7, ..BASE }).rows, SETTLED_S);
    assert!(near(late.mic_ms - early.mic_ms, 5.0, 0.01), "{late:?} against {early:?}");
    assert!(near(late.queue_ms, early.queue_ms, 0.3) && near(late.render_ms, early.render_ms, 0.3), "{late:?} against {early:?}");
    assert!((late_readout - early_readout - 5.0).abs() < 0.5, "{late_readout} against {early_readout}");
}

/// A wake 15 ms late: its first block's sample is 15 ms older and finds 15 ms less queued (the device played on), the next
/// block (read at the same wake, 10 ms younger) 5 ms of each, and the readout doesn't move: the output's clock sets it.
#[test]
fn a_late_wake_moves_time_from_the_queue_to_the_mic() {
    let o = run(Sim { late_wake: Some((50.0, 15.0)), ..BASE });
    let (before, readout) = means(&o.rows.iter().filter(|r| r.t_s < 50.0).copied().collect::<Vec<_>>(), SETTLED_S);
    let at = o.rows.iter().position(|r| r.t_s >= 50.0).unwrap();
    let (first, second) = (o.rows[at], o.rows[at + 1]);
    assert_eq!(first.t_s, second.t_s, "both read at the late wake");
    assert!(near(first.b.mic_ms - before.mic_ms, 15.0, 0.01) && near(second.b.mic_ms - before.mic_ms, 5.0, 0.01), "{first:?} {second:?}");
    assert!(near(before.queue_ms - first.b.queue_ms, 15.0, 1.0) && near(before.queue_ms - second.b.queue_ms, 5.0, 1.0), "{first:?}");
    for r in [first, second] {
        assert!((f64::from(r.readout) - readout).abs() < 1.0 && near(r.b.render_ms, before.render_ms, 0.3), "{r:?}: {readout}");
    }
    assert_eq!((o.p.counters.render_underruns, o.p.counters.output_drops), (0, 0));
}

/// Wait: a block whose first samples came a wake before its last waits that wake; and a block behind another in the same
/// wake waits for the one before it (here 0.2 ms of chain time). Its sample is 10 ms younger, and the block before it is
/// queued ahead of it.
#[test]
fn held_samples_and_later_blocks_wait() {
    // Half-block packets at 5 ms wakes: each block's head waits for the next wake.
    let (b, _) = means(&run(Sim { packet: 240, wake_ms: 5.0, ..BASE }).rows, SETTLED_S);
    assert!(near(b.mic_ms, 5.7, 0.01) && near(b.wait_ms, 5.0, 0.01), "{b:?}");

    let mut p = pipe(false);
    let mut sink = FakeSink::new(4800);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut mic = FakeCapture::default();
    let now = sink.now_hns;
    // A burst: two packets at once, 20 and 10 ms old.
    mic.push(0, Some((now - 200_000.0) as u64), vec![0.1; 480]);
    mic.push(480, Some((now - 100_000.0) as u64), vec![0.1; 480]);
    p.drain_mic(&mut mic, now).unwrap();
    assert!(p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, true).unwrap());
    let first = p.last_budget.unwrap();
    // The chain took 0.2 ms over the first block (the device played nothing meanwhile).
    sink.now_hns += 2_000.0;
    assert!(p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, true).unwrap());
    let second = p.last_budget.unwrap();
    assert!(near(first.mic_ms, 20.0, 1e-4) && near(first.wait_ms, 0.0, 1e-4) && near(first.queue_ms, 22.0, 1e-3), "{first:?}");
    assert!(near(second.mic_ms, 10.0, 1e-4) && near(second.wait_ms, 0.2, 1e-4) && near(second.queue_ms, 32.0, 1e-3), "{second:?}");
    // This device plays as it takes (its clock runs smoothly): no render side.
    assert!(near(first.render_ms, 0.0, 1e-3) && near(second.render_ms, 0.0, 1e-3), "{first:?} {second:?}");
    assert!(near(second.readout_ms(), p.last_latency_ms.unwrap(), 1e-4));
}

/// A margin 10 ms bigger raises the output target, the queue and the readout by 10 ms; nothing else moves.
#[test]
fn a_fuller_output_raises_the_queue() {
    let (usual, readout) = means(&run(BASE).rows, SETTLED_S);
    let (fuller, fuller_readout) = means(&run(Sim { margin: 96 + 480, ..BASE }).rows, SETTLED_S);
    assert!(near(fuller.queue_ms - usual.queue_ms, 10.0, 0.3), "{fuller:?} against {usual:?}");
    assert!(near(fuller.mic_ms, usual.mic_ms, 1e-3) && near(fuller.render_ms, usual.render_ms, 0.3), "{fuller:?} against {usual:?}");
    assert!((fuller_readout - readout - 10.0).abs() < 0.5, "{fuller_readout} against {readout}");
}

/// A device whose clock says it holds 10 ms more after taking a sample (an engine's mix buffer, a driver's FIFO) raises the
/// render side, and the readout, by 10 ms; the queue, which is what we hold, doesn't move.
#[test]
fn the_render_side_is_what_the_device_clock_holds() {
    let (plain, readout) = means(&run(BASE).rows, SETTLED_S);
    let (lagged, lagged_readout) = means(&run(Sim { lag: 480.0, ..BASE }).rows, SETTLED_S);
    assert!(near(lagged.render_ms - plain.render_ms, 10.0, 0.3), "{lagged:?} against {plain:?}");
    assert!(near(lagged.queue_ms, plain.queue_ms, 0.3) && near(lagged.mic_ms, plain.mic_ms, 1e-3), "{lagged:?} against {plain:?}");
    assert!((lagged_readout - readout - 10.0).abs() < 0.5, "{lagged_readout} against {readout}");
    // A 1024-frame device: half its period, 10.7 ms.
    let (long, _) = means(&run(Sim { out_period: 1024, ..BASE }).rows, SETTLED_S);
    assert!(near(long.render_ms, 1024.0 / 96.0, 0.3), "{long:?}");
}

/// The chain's offset is reported beside the readout, which doesn't hold it: the same run with a 10 ms chain has the same
/// readouts, and its first sample's whole way is 10 ms longer.
#[test]
fn the_chain_is_beside_the_readout() {
    let (plain, with) = (run(BASE), run(Sim { chain: 480, ..BASE }));
    assert_eq!(plain.rows.len(), with.rows.len());
    for (a, b) in plain.rows.iter().zip(&with.rows) {
        assert_eq!(a.readout, b.readout);
        assert!(near(b.b.chain_ms, 10.0, 1e-6) && near(b.b.total_ms() - a.b.total_ms(), 10.0, 1e-3), "{b:?}");
    }
}

/// The cables' bursts: the first sample is 5 ms older on average (blocks 20.7 and 10.7 ms old in turn), and the readout 5 ms
/// longer; the queue still holds the target on average, but a burst's first block finds a block less queued than its second
/// (the spare block in use), so mic delivery plus queue stays put block for block. Steady delivery never dips.
#[test]
fn bursts_raise_the_mic_delivery_and_dip_into_the_queue() {
    let (s, b) = (run(BASE), run(Sim { late: bursty, ..BASE }));
    let ((steady_mean, steady_readout), (bursty_mean, bursty_readout)) = (means(&s.rows, SETTLED_S), means(&b.rows, SETTLED_S));
    assert!(near(bursty_mean.mic_ms - steady_mean.mic_ms, 5.0, 0.01), "{bursty_mean:?} against {steady_mean:?}");
    assert!(near(bursty_mean.queue_ms, steady_mean.queue_ms, 0.3), "{bursty_mean:?} against {steady_mean:?}");
    assert!((bursty_readout - steady_readout - 5.0).abs() < 0.5, "{bursty_readout} against {steady_readout}");
    let late = |o: &Out| o.rows.iter().filter(|r| r.t_s >= SETTLED_S).copied().collect::<Vec<_>>();
    let lowest = |rows: &[Row]| rows.iter().map(|r| r.b.queue_ms).fold(f32::MAX, f32::min);
    let spread =
        |rows: &[Row], f: &dyn Fn(&Row) -> f32| rows.iter().map(f).fold(f32::MIN, f32::max) - rows.iter().map(f).fold(f32::MAX, f32::min);
    let (sr, br) = (late(&s), late(&b));
    assert!(lowest(&sr) > steady_mean.queue_ms - 1.0, "steady: {}", lowest(&sr));
    assert!(near(lowest(&br), bursty_mean.queue_ms - 5.0, 1.0), "bursty: {} against {bursty_mean:?}", lowest(&br));
    assert!(spread(&br, &|r| r.b.queue_ms) > 9.0 && spread(&br, &|r| r.b.mic_ms + r.b.queue_ms) < 1.0, "the queue takes up the bursts");
    for o in [&s, &b] {
        assert_eq!((o.p.counters.render_underruns, o.p.counters.output_drops), (0, 0));
    }
}

/// After the output ran dry, a device whose clock counted the silence it played (as this one's does: it ticks on, taking
/// nothing) reads that much ahead of what it took. The readout drops by the silence and stays there for the rest of the run
/// (this 40 ms stall: from 37.7 to 16.1 ms), though the queue and so the real latency are back where they were; the budget
/// shows the drop all on the render side (5.0 to -16.1 ms). A longer dry spell can put the play-out before the capture,
/// and then there's no readout at all (a 60 ms stall at 50 s: none for the 20 s left). Whether WASAPI's clock counts an
/// underrun's silence is unverified (TODO 13's detail, "Budget").
#[test]
fn after_a_dry_spell_the_device_clock_shows_on_the_render_side() {
    let o = run(Sim { late: mic_stall, seconds: 70.0, ..BASE });
    let rows = |from: f64, to: f64| o.rows.iter().filter(|r| r.t_s >= from && r.t_s < to).copied().collect::<Vec<_>>();
    let ((before, before_readout), (after, after_readout)) = (means(&rows(40.0, 50.0), 0.0), means(&rows(62.0, 70.0), 0.0));
    assert!(near(after.mic_ms, before.mic_ms, 0.01) && near(after.queue_ms, before.queue_ms, 1.0), "{after:?} against {before:?}");
    let drop = before_readout - after_readout;
    assert!(drop > 5.0, "{after_readout} against {before_readout}");
    assert!((f64::from(before.render_ms - after.render_ms) - drop).abs() < 1.0, "{after:?} against {before:?}: {drop}");
    assert_eq!((o.p.counters.render_underruns, o.p.counters.output_drops), (1, 1), "{:?}", o.p.counters);
}

/// The output target a block lower (one period + the margin, 12 ms; an experiment, the engine's isn't changed): steady
/// delivery runs clean through every phase of the device against the wakes (300 ppm sweeps them in 33 s), with 10.5 ms
/// left in the queue at its lowest. Bursts dip into the queue by how much later than the mean their blocks come: one wake in
/// five missed takes it to nothing and the device runs dry at the worst phases, where the engine's target (the spare
/// block) runs clean. `a_block_lower_target_sweep` prints more.
#[test]
fn the_spare_block_is_what_bursts_need() {
    let lower = |late: Late, less: usize| run(Sim { late, target_less: less, jitter_ms: 1.0, ..BASE });
    let lowest = |o: &Out| o.rows.iter().filter(|r| r.t_s >= 5.0).map(|r| r.b.queue_ms).fold(f32::MAX, f32::min);
    let steady_lower = lower(steady, 480);
    assert_eq!(steady_lower.p.counters.render_underruns, 0);
    assert!(lowest(&steady_lower) > 10.0, "{}", lowest(&steady_lower));
    let rare_lower = lower(rare_bursts, 480);
    assert!(rare_lower.p.counters.render_underruns > 0, "{:?}", rare_lower.p.counters);
    let rare = lower(rare_bursts, 0);
    assert_eq!(rare.p.counters.render_underruns, 0);
    assert!(lowest(&rare) > 9.0, "{}", lowest(&rare));
}

/// [`the_spare_block_is_what_bursts_need`] across delivery, the target, the device's phase and the drift (0 and 30 ppm hold
/// one phase for the run; 300 ppm sweeps them all): the faults and the queue's lowest and mean. `--ignored --nocapture`.
#[test]
#[ignore]
fn a_block_lower_target_sweep() {
    for (name, late) in [("steady", steady as Late), ("bursty", bursty), ("rare bursts", rare_bursts)] {
        for less in [0, 480] {
            for phase in [0.05, 0.37, 0.6, 0.9] {
                for ppm in [0.0, 30.0, 300.0] {
                    let o = run(Sim { late, target_less: less, device_phase: phase, ppm, jitter_ms: 1.0, ..BASE });
                    let low = o.rows.iter().filter(|r| r.t_s >= 5.0).map(|r| r.b.queue_ms).fold(f32::MAX, f32::min);
                    let (b, readout) = means(&o.rows, SETTLED_S);
                    eprintln!(
                        "{name:<12} target -{less:<3} phase {phase:.2} {ppm:>5} ppm: underruns {} drops {}; \
                         queue lowest {low:.1} mean {:.1}; readout {readout:.1}",
                        o.p.counters.render_underruns, o.p.counters.output_drops, b.queue_ms
                    );
                }
            }
        }
    }
}

/// The faults: underruns and coarse drops.
fn faults(o: &Out) -> (u64, u64) {
    (o.p.counters.render_underruns, o.p.counters.output_drops)
}

/// The adaptive output target (out_target.rs) in this file's scenes, with their events at 200 s, after it has come down
/// (it walks down from 120 s), against the fixed target in the same scenes. Its budget adds up block for block as the
/// fixed one's does, it is at or under the fixed target before the event (wherever the delivery lets it), and it
/// underruns at most once more than the fixed target and drops no more. The once more is the trade: a wake 15 ms late,
/// which the fixed target's spare block covers, runs the lowered target dry (and the strike puts it back to the fixed
/// target; events.rs has the strikes over many hiccups).
#[test]
fn the_adaptive_target_in_these_scenes_after_it_has_come_down() {
    let long = Sim { seconds: 260.0, record: false, ..BASE };
    #[rustfmt::skip]
    let cases: [(&str, Sim); 13] = [
        ("steady", long),
        ("steady, no drift", Sim { ppm: 0.0, ..long }),
        ("bursty", Sim { late: bursty, ..long }),
        ("rare bursts, -300 ppm", Sim { late: rare_bursts, ppm: -300.0, ..long }),
        ("half-block packets", Sim { packet: 240, wake_ms: 5.0, ..long }),
        ("a 1024-frame device", Sim { out_period: 1024, ..long }),
        ("a wake 15 ms late", Sim { late_wake: Some((200.0, 15.0)), ..long }),
        ("a wake 35 ms late: the device runs dry", Sim { late_wake: Some((200.0, 35.0)), ..long }),
        ("a mic stall: dry, then overfull", Sim { late: mic_stall_at_200, seconds: 270.0, ..long }),
        ("a lagging clock and a chain", Sim { lag: 700.0, chain: 912, ..long }),
        ("steady, wake jitter", Sim { jitter_ms: 1.0, ..long }),
        ("bursty, wake jitter", Sim { late: bursty, jitter_ms: 1.0, ..long }),
        ("rare bursts, wake jitter", Sim { late: rare_bursts, jitter_ms: 1.0, ..long }),
    ];
    let mut extra = Vec::new();
    for (what, sim) in cases {
        let (fixed, adaptive) = (run(sim), run(Sim { adaptive: true, ..sim }));
        let (f, a) = (faults(&fixed), faults(&adaptive));
        assert!(a.0 <= f.0 + 1 && a.1 <= f.1, "{what}: adaptive {a:?} against fixed {f:?}");
        if a.0 > f.0 {
            extra.push(what);
        }
        for r in &adaptive.rows {
            assert!(near(r.b.readout_ms(), r.readout, 1e-3), "{what}: {r:?}");
        }
        let before = |o: &Out| o.targets.iter().rev().find(|(t, _)| *t < 199.0).map_or(0, |x| x.1);
        assert!(before(&adaptive) <= before(&fixed), "{what}: never over the fixed target");
        if sim.out_period == 480 && !what.starts_with("rare bursts") {
            assert!(before(&adaptive) < before(&fixed) - 96, "{what}: down by 2 ms or more: {}", before(&adaptive));
        }
    }
    assert_eq!(extra, ["a wake 15 ms late"], "the trade, as measured");
}

/// Steady delivery (1 ms of wake jitter): for its two-minute window the target is the fixed one (22 ms); then it walks
/// down (a tenth of the way a second, at most 0.5 ms) to the floor (a period + the margin, 12 ms) + the dips it saw +
/// 1 ms, the drift loop slips the queue down after it without going under where it ends (a target set in one step took
/// the queue 1.4 ms under it), and the readout comes down by as much. Nothing underruns.
#[test]
fn the_adaptive_target_comes_down_after_steady_delivery() {
    let sim = Sim { jitter_ms: 1.0, seconds: 240.0, record: false, ..BASE };
    for ppm in [0.0, 300.0] {
        let (fixed, o) = (run(Sim { ppm, ..sim }), run(Sim { adaptive: true, ppm, ..sim }));
        let at = |t: f64| o.targets.iter().rev().find(|(s, _)| *s <= t).map_or(0, |x| x.1);
        assert_eq!(at(119.0), 1056, "the fixed target for the window");
        let target_ms = o.p.out_target() as f32 / 48.0;
        assert!(target_ms > 13.0 && target_ms < 15.5, "{ppm} ppm: {target_ms}");
        let first = at(121.5);
        assert!(first < 1056 && first > o.p.out_target() + 240, "{ppm} ppm: a walk, not a step: {first}");
        assert_eq!(at(180.0), o.p.out_target(), "{ppm} ppm: there within a minute");
        let targets: Vec<usize> = o.targets.iter().map(|x| x.1).collect();
        assert!(targets.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] <= 24), "{ppm} ppm: down only, 0.5 ms a second at most");
        let lowest = (120..240)
            .map(|s| {
                let rows: Vec<Row> = o.rows.iter().filter(|r| r.t_s >= f64::from(s) && r.t_s < f64::from(s + 1)).copied().collect();
                means(&rows, 0.0).0.queue_ms
            })
            .fold(f32::MAX, f32::min);
        assert!(lowest > target_ms - 0.15, "{ppm} ppm: the queue's lowest 1 s mean {lowest} against {target_ms}");
        let ((late, readout), (_, fixed_readout)) = (means(&o.rows, 200.0), means(&fixed.rows, 200.0));
        assert!(near(late.queue_ms, target_ms, 0.4), "{ppm} ppm: {late:?}");
        if ppm != 0.0 {
            assert!((readout - (fixed_readout - f64::from(22.0 - target_ms))).abs() < 0.6, "{readout} against {fixed_readout}");
        }
        assert_eq!(faults(&o), (0, 0));
        assert!(near(o.tel.output_target_ms.load(), target_ms, 1e-3), "published");
    }
}

/// Bursts hold the target up by what they need, and nothing underruns at any phase (300 ppm sweeps the wakes through
/// the device's phases every 33 s): the cables' bursts (every other block a wake late) need 5 ms and bring it down to
/// about 18 ms; one wake in five missed needs 8 ms, which leaves the spare block about where it was.
#[test]
fn bursts_hold_the_adaptive_target_at_what_they_need() {
    for (what, late, lo, hi) in [("bursty", bursty as Late, 17.0f32, 20.0f32), ("rare bursts", rare_bursts, 20.5, 22.01)] {
        let o = run(Sim { late, jitter_ms: 1.0, adaptive: true, seconds: 300.0, record: false, ..BASE });
        let target_ms = o.p.out_target() as f32 / 48.0;
        assert!(target_ms > lo && target_ms < hi, "{what}: {target_ms}");
        assert_eq!(faults(&o), (0, 0), "{what}");
        let low = o.rows.iter().filter(|r| r.t_s >= 180.0).map(|r| r.b.queue_ms).fold(f32::MAX, f32::min);
        assert!(low > 10.5, "{what}: the queue's lowest {low} ms, over the period");
    }
}

/// Once the target has come down (steady delivery against a 5 ms device: from 17 ms to about 9), a wake 13 ms late, which
/// the fixed target's spare block covers at every phase, runs the device dry once; the target goes back to the fixed one
/// at that block (a strike), the zeros fill the device to it, and it stays there (the strike's need holds it for 10
/// minutes): nothing underruns again.
#[test]
fn an_underrun_raises_the_adaptive_target_at_once() {
    let sim = Sim { out_period: 240, jitter_ms: 1.0, late_wake: Some((240.0, 13.0)), seconds: 300.0, record: false, ..BASE };
    let fixed_target = 240 + 480 + 96;
    let (fixed, o) = (run(sim), run(Sim { adaptive: true, ..sim }));
    assert_eq!(faults(&fixed), (0, 0), "the spare block covers it");
    assert_eq!(faults(&o), (1, 0), "the lowered target doesn't");
    let at = |t: f64| o.targets.iter().rev().find(|(s, _)| *s <= t).map_or(0, |x| x.1);
    assert!(at(239.0) < fixed_target - 240, "down before: {}", at(239.0));
    let up = o.targets.iter().find(|(s, _)| *s >= 240.0).map_or(0, |x| x.1);
    assert_eq!(up, fixed_target, "up at the block that ran dry");
    let (after, _) = means(&o.rows, 250.0);
    assert!(after.queue_ms >= fixed_target as f32 / 48.0 - 0.5, "{after:?}");
    assert_eq!(o.p.out_target(), fixed_target, "held");
    assert_eq!(o.p.adaptive.as_ref().map(|a| a.strikes()), Some(1));
}

/// The output's facts per block (`OutputFacts`, TODO 9) follow the device stream exactly: block to block its index moves by
/// the last block's length (480 and its slip, or nothing when it was dropped whole) plus this block's zeros less the pending
/// samples it dropped, and the facts add up to the run's counters (the output's events), through bursts, a device that runs
/// dry, a mic stall's dry spell and its overfull burst, a device period other than a block, and a lagging clock.
#[test]
fn the_output_facts_follow_the_stream_block_for_block() {
    #[rustfmt::skip]
    let cases: [(&str, Sim); 6] = [
        ("steady", BASE),
        ("bursty", Sim { late: bursty, ..BASE }),
        ("a wake 35 ms late: the device runs dry", Sim { late_wake: Some((30.0, 35.0)), ..BASE }),
        ("a mic stall: dry, then overfull", Sim { late: mic_stall, seconds: 70.0, ..BASE }),
        ("a 1024-frame device", Sim { out_period: 1024, ..BASE }),
        ("a lagging clock", Sim { lag: 700.0, ..BASE }),
    ];
    for (what, sim) in cases {
        let o = run(sim);
        assert_eq!(o.recorded.len(), o.blocks, "{what}");
        for w in o.recorded.windows(2) {
            let (a, b) = (&w[0].output, &w[1].output);
            let len = if a.skipped { 0 } else { 480 + i64::from(a.slip) };
            let expect = a.index as i64 + len + i64::from(b.zeros) - i64::from(b.dropped);
            assert_eq!(b.index as i64, expect, "{what}: block {} after {a:?}: {b:?}", w[1].index);
        }
        let facts = o.recorded.iter().map(|b| b.output);
        let (mut zeros, mut deficits, mut dropped, mut slip_net, mut underruns, mut reanchors) = (0u64, 0u64, 0u64, 0i64, 0u64, 0u64);
        for f in facts {
            zeros += u64::from(f.zeros);
            deficits += u64::from(f.zeros > 0);
            dropped += u64::from(f.dropped) + if f.skipped { 480 } else { 0 };
            slip_net += i64::from(f.slip);
            underruns += u64::from(f.underrun);
            reanchors += u64::from(f.reanchored);
            assert_eq!(f.target as usize, o.p.out_target(), "{what}: the fixed target");
        }
        let c = &o.p.counters;
        assert_eq!(
            (zeros, deficits, dropped, slip_net, underruns, reanchors),
            (c.output_zero_frames, c.output_deficits, c.output_dropped_frames, c.output_slip_net, c.render_underruns, c.output_reanchors),
            "{what}: {c:?}"
        );
        let corrected = c.output_deficits + c.output_drops + c.render_underruns + c.output_reanchors;
        match what {
            "steady" | "bursty" | "a 1024-frame device" | "a lagging clock" => assert_eq!(corrected, 0, "{what}: {c:?}"),
            _ => assert!(corrected > 0, "{what}: {c:?}"),
        }
    }
}

/// The cables' bursts (0 and 960 frames a wake in turn) move the output path's delay by nothing but the drift loop's slips:
/// over two minutes at 300 ppm no zeros, drops, underruns or re-anchors after the start, and the readout's 1 s means hold
/// within a millisecond once the calibration has settled. So the engine's output side can't make the 9-12 ms steps the
/// quality takes saw (TODO 9: they were silence inserted after the filter's output, the cable's or our test recorder's, which the readout,
/// timed by the output's own clock, doesn't see).
#[test]
fn the_cables_bursts_move_the_output_path_by_its_slips_alone() {
    let o = run(Sim { late: bursty, seconds: 120.0, ..BASE });
    let c = &o.p.counters;
    assert_eq!((c.output_deficits, c.output_drops, c.render_underruns, c.output_reanchors), (0, 0, 0, 0), "{c:?}");
    assert!(c.output_slips > 0, "300 ppm needs slips: {c:?}");
    let mut means = Vec::new();
    for s in (SETTLED_S as usize)..120 {
        let r: Vec<f64> = o.rows.iter().filter(|r| r.t_s >= s as f64 && r.t_s < (s + 1) as f64).map(|r| f64::from(r.readout)).collect();
        means.push(r.iter().sum::<f64>() / r.len() as f64);
    }
    let (lo, hi) = means.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &m| (lo.min(m), hi.max(m)));
    assert!(hi - lo < 1.0, "the readout's 1 s means moved {:.2} ms: {means:?}", hi - lo);
}

/// `devices --check`'s drift (TODO 21): fitted over a 15 s check's blocks after its first second (`clock_drift`), it comes
/// within a few ppm of the device's true drift, steady or with the cables' bursts and wakes up to 3 ms late, where the drift
/// loop's own estimate is still far off at 15 s (here, at 0 ppm with the bursts, -196; at +100, -289 on the loop's sign: its
/// integral settles in 30-60 s, and bursts give it a start-up transient of hundreds of ppm). Without the mic's stamps its
/// delivery times it, less precisely.
#[test]
fn a_short_check_measures_the_drift() {
    for (late, jitter_ms, name) in [(steady as Late, 0.0, "steady"), (bursty as Late, 3.0, "bursty, jittered")] {
        for ppm in [0.0, 100.0, -100.0, 300.0, -300.0] {
            let o = run(Sim { late, jitter_ms, ppm, seconds: 16.0, ..BASE });
            let blocks = &o.recorded[100..];
            let d = crate::api::clock_drift(blocks);
            assert_eq!(d.mic, Some(crate::api::MicTiming::Stamps), "{name}, {ppm}");
            let fit = d.output.unwrap_or_else(|| panic!("{name}, {ppm}: no fit"));
            assert!((fit.ppm - ppm).abs() < 5.0 && fit.uncertainty_ppm < 10.0, "{name}, {ppm} ppm: {fit:?}");
            assert!(fit.seconds > 14.0, "{fit:?}");
            assert_eq!(d.reference, None, "no reference here");
            // Without the stamps: by the mic's delivery.
            let unstamped: Vec<ChainBlock> =
                blocks.iter().map(|b| ChainBlock { mic_stamped: false, capture_qpc_hns: f64::NAN, ..*b }).collect();
            let d = crate::api::clock_drift(&unstamped);
            let fit = d.output.unwrap();
            assert_eq!(d.mic, Some(crate::api::MicTiming::Delivery));
            assert!((fit.ppm - ppm).abs() < 30.0 && fit.uncertainty_ppm < 100.0, "{name}, {ppm} ppm, by delivery: {fit:?}");
        }
    }
}
