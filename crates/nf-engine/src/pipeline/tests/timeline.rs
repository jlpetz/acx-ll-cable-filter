//! The reference timeline against bursty delivery (reference.rs, TODO 10): synthetic scenes modelled on the dev box's
//! cables, the live packet traces replayed, and an offline AEC3 check on both.
//!
//! Every run replays packet rows, as `live_probe.rs` logs them, through the real pipeline (nf-core's drift loop and slip
//! resampler on the output side) with index-coded samples, so each block shows which reference sample was served with
//! which mic sample: the echo path. The cables' mic delivers 0, 480 or 960 frames a 10 ms wake, its stamps in 0.5 ms steps
//! within about ±1 ms; the loopback delivers steadily or in bursts of up to 1440, its stamps within 0.05 ms. The scenes
//! add drift, stalls, losses, silence, late wakes, streams without stamps and stamps that are off or noisy; `NF_SEED`
//! plays them all with another seed. The ignored ones: the trace replays (`traces_hold_the_echo_path`), the offline AEC3
//! check (`aec3_converges_on_the_traces`, about 3 minutes) and a printed sweep of the scenes (`scene_sweep`).

use std::ops::Range;
use std::path::PathBuf;

use nf_core::stage::{Activation, AecTelemetry, ProcessIo, Stage};
use nf_stages::{Aec3Stage, synth};

use super::events::VSink;
use super::*;
use crate::reference::LEAD_CAP;

/// One logged packet (live_probe.rs): the wake it was read at and its stamp, in hns from the probe's start (the stamp
/// `NO_STAMP` when there was none; a stamp can be a little before the start), the stream (0 mic, 1 loopback), its frames and
/// device position. `code` is the stream index its samples are coded with: the device position in a trace; in a scene
/// the index they were tapped at, which counts what never came even where the device position doesn't.
#[derive(Clone, Copy, Debug)]
pub(super) struct Row {
    pub(super) now: f64,
    kind: u8,
    frames: usize,
    pos: u64,
    qpc: f64,
    code: u64,
}

const NO_STAMP: f64 = -1.0;

fn parse(text: &str) -> Vec<Row> {
    let field = |f: Option<&str>| f.and_then(|v| v.trim().parse::<f64>().ok());
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split(',');
            let (now, kind, frames, pos, qpc) = (field(f.next())?, field(f.next())?, field(f.next())?, field(f.next())?, field(f.next())?);
            Some(Row { now, kind: kind as u8, frames: frames as usize, pos: pos as u64, qpc, code: pos as u64 })
        })
        .collect()
}

/// Where the probe writes its traces and the replays read them: `%LOCALAPPDATA%\noise-filter\traces`.
fn trace_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join("noise-filter").join("traces"))
}

/// Every `*.csv` in the trace folder, by name; none when it's absent.
fn traces() -> Vec<(String, Vec<Row>)> {
    let Some(dir) = trace_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "csv")).collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let rows = parse(&std::fs::read_to_string(&p).ok()?);
            let name = p.file_stem()?.to_string_lossy().into_owned();
            (!rows.is_empty()).then_some((name, rows))
        })
        .collect()
}

/// The engine's pipeline for the dev box's cables: 10 ms periods everywhere.
pub(super) fn cable_pipe() -> Pipeline {
    let c = PipelineConfig { out_period: 480, out_margin: 96, ref_period: Some(480), max_packet: 4800 };
    Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new())
}

const T0: f64 = 1e10;

/// Replays `rows` as the audio loop runs: each wake queues its packets, drains the mic, drains the loopback when a block is
/// due, and processes every block. Device positions and codes are taken relative to each stream's first; `mic_of` and
/// `lb_of` give the samples at a code.
fn replay<P: BlockProcessor>(
    rows: &[Row], proc: &mut P, mic_of: &dyn Fn(u64, usize) -> Vec<f32>, lb_of: &dyn Fn(u64, usize) -> Vec<f32>,
) -> Pipeline {
    replay_into(cable_pipe(), rows, proc, mic_of, lb_of)
}

/// [`replay`] through a pipeline of the caller's (one with the chain recorder armed, say).
pub(super) fn replay_into<P: BlockProcessor>(
    mut p: Pipeline, rows: &[Row], proc: &mut P, mic_of: &dyn Fn(u64, usize) -> Vec<f32>, lb_of: &dyn Fn(u64, usize) -> Vec<f32>,
) -> Pipeline {
    let mut sink = VSink::new(480, 0.0, 0.37, T0);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
    let mut ticks = FakeTicks(0);
    let (mut m0, mut l0) = (None, None);
    let mut i = 0;
    while i < rows.len() {
        let now = rows[i].now;
        while let Some(r) = rows.get(i).filter(|r| r.now == now) {
            let (pos0, code0) = if r.kind == 0 { *m0.get_or_insert((r.pos, r.code)) } else { *l0.get_or_insert((r.pos, r.code)) };
            let q = (r.qpc != NO_STAMP).then_some((T0 + r.qpc) as u64);
            let (pos, code) = (r.pos - pos0, r.code - code0);
            if r.kind == 0 {
                mic.push(pos, q, mic_of(code, r.frames));
            } else {
                lb.push(pos, q, lb_of(code, r.frames));
            }
            i += 1;
        }
        let now = T0 + now;
        sink.advance_to(now);
        p.drain_mic(&mut mic, now).unwrap();
        if p.block_ready() {
            p.drain_reference(&mut lb, now).unwrap();
        }
        while p.process_block(proc, &mut sink, &mut ticks, &tel, false).unwrap() {}
    }
    p
}

/// Samples that carry their stream index plus one, so a zero is a sample the pipeline filled in.
pub(super) fn index_coded(pos: u64, n: usize) -> Vec<f32> {
    (pos + 1..pos + 1 + n as u64).map(|v| v as f32).collect()
}

/// Per block: the mic index of its first sample, the reference index lined up with it (both from the block's first real
/// sample; `None` when the whole block was filled in), and how many reference samples were filled in. Then the loopback's
/// drift against the mic (ppm), when it's known: the echo path moves at that rate.
#[derive(Default)]
struct LagSpy(Vec<(Option<f64>, Option<f64>, usize)>, f64);

/// The stream index of an index-coded block's first sample: from its last when that's real (a slip's interpolation
/// rings across a step from zeros, but always ends exactly on its last input), else from its first real one. Index codes
/// stay exact in f32 below 2^23 (2.9 minutes at 48 kHz).
fn first_index(b: &Block) -> Option<f64> {
    match b.last() {
        Some(&v) if v >= 0.5 => Some(f64::from(v) - 1.0 - (BLOCK - 1) as f64),
        _ => b.iter().position(|&v| v >= 0.5).map(|k| f64::from(b[k]) - 1.0 - k as f64),
    }
}

impl BlockProcessor for LagSpy {
    fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        self.0.push((first_index(mic), first_index(r), r.iter().filter(|&&v| v < 0.5).count()));
        *out = *mic;
        ChainReport::default()
    }
    fn aec_stats(&self) -> Option<AecStats> {
        None
    }
}

impl LagSpy {
    /// The mic index and the echo path (mic index less reference index) of each served block from block `from` on.
    fn served(&self, from: usize) -> Vec<(f64, f64)> {
        self.0.iter().skip(from).filter_map(|&(m, r, _)| Some((m?, m? - r?))).collect()
    }

    /// Whether the echo path moved between two served blocks: by more than two samples off the clocks' line. Across
    /// blocks that weren't served, by more than four: the stamps that put the timeline back after them jitter by a sample
    /// or so each, and the slips went on meanwhile at the loop's drift estimate, which may be 10 ppm off.
    fn moved(&self, a: (f64, f64), b: (f64, f64)) -> bool {
        let dm = b.0 - a.0;
        let tolerance = if dm > 1.5 * BLOCK as f64 { 4.0 + 10e-6 * dm } else { 2.0 };
        (b.1 - a.1 + self.1 * 1e-6 * dm).abs() > tolerance
    }

    /// How far the echo path strays from the line the clocks draw (a loopback `ppm` fast takes `ppm` more of its
    /// samples per mic sample), from block `from` on: the most within any stretch between moves (each measured from its
    /// own start), and the last block's from the first's.
    fn wander(&self, from: usize, ppm: f64) -> (f64, f64) {
        let served = self.served(from);
        let Some(&first) = served.first() else { return (0.0, 0.0) };
        let off = |a: (f64, f64), b: (f64, f64)| (b.1 - a.1 + (b.0 - a.0) * ppm * 1e-6).abs();
        let (mut most, mut start) = (0.0f64, first);
        for w in served.windows(2) {
            if self.moved(w[0], w[1]) {
                start = w[1];
            }
            most = most.max(off(start, w[1]));
        }
        (most, served.last().map_or(0.0, |&l| off(first, l)))
    }
}

/// What a replay did to the echo path from block `from` on (3 s by default: the start-up is 2.5 s).
#[derive(Debug)]
struct PathStats {
    gaps: u64,
    drops: u64,
    slips: u64,
    /// Block-to-block changes of more than two samples (a slip is one; across unserved blocks, plus the drift).
    moves: usize,
    /// Blocks with no reference sample at all, and with some filled in.
    unserved: usize,
    filled: usize,
    lag_lo: f64,
    lag_hi: f64,
    target_ms: f32,
}

const SETTLED: usize = 300;

fn path_stats(p: &Pipeline, spy: &LagSpy, from: usize) -> PathStats {
    let served = spy.served(from);
    let after = spy.0.get(from..).unwrap_or_default();
    let (lag_lo, lag_hi) = served.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x.1), hi.max(x.1)));
    let c = p.counters;
    PathStats {
        gaps: c.reference_gaps,
        drops: c.reference_drops,
        slips: c.reference_slips,
        moves: served.windows(2).filter(|w| spy.moved(w[0], w[1])).count(),
        unserved: after.iter().filter(|x| x.1.is_none()).count(),
        filled: after.iter().filter(|x| x.2 > 0).count(),
        lag_lo,
        lag_hi,
        target_ms: p.ref_target() as f32 / 48.0,
    }
}

// ---- Synthetic scenes ----------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[((self.unit() * of.len() as f64) as usize).min(of.len() - 1)]
    }
}

/// How a stream's packets reach the wakes: a packet is ready this long after its last sample. `Steady`: 0.5 ms (plus up to
/// 0.3 ms). `Bursty`: as the cables, the mic 0.5 or 10.5 ms (0, 480 or 960 frames a wake), the loopback 0.5, 10.5 or 20.5
/// ms (up to 1440 a wake), each kept for a packet or two at random.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Delivery {
    Steady,
    Bursty,
}

/// What happens at the scene's `at_s`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Ev {
    None,
    /// One wake this many ms late: both streams queue.
    LateWake(f64),
    /// The mic delivers nothing for this many ms, then everything.
    MicStall(f64),
    /// The mic's packets of this many ms never come (its position jumps).
    MicLost(f64),
    LoopbackStall(f64),
    LoopbackLost(f64),
    /// The loopback delivers nothing for this many ms (far-end silence without the keep-alive), and its device position
    /// carries on from where it stopped.
    LoopbackSilent(f64),
    /// From then on every loopback packet is ready this many ms later.
    LoopbackLater(f64),
    /// One loopback packet's stamp this many ms off.
    LoopbackStampOff(f64),
    /// From then on the loopback delivers steadily.
    LoopbackSteady,
}

/// Which streams' packets carry stamps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Stamps {
    Both,
    MicOnly,
    LoopbackOnly,
    Neither,
}

/// The mic's stamps: the cables' (0.5 ms steps, -0.5 to +1 ms), uniform within ±this many ms, or the wake that
/// delivered the packet less its length (a driver that stamps at delivery).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MicStamps {
    Cables,
    Uniform(f64),
    AtDelivery,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Scene {
    mic: Delivery,
    lb: Delivery,
    /// The loopback's clock against the mic's.
    ppm: f64,
    /// When the loopback's first sample was tapped, against the mic's first (ms; negative: it ran first).
    lb_start_ms: f64,
    pub(super) stamps: Stamps,
    mic_stamps: MicStamps,
    pub(super) event: Ev,
    /// When the event happens (s).
    at_s: f64,
    seed: u64,
}

const SCENE_S: f64 = 40.0;
const EVENT_S: f64 = 20.0;
const PKT: f64 = 100_000.0;

/// The scenes' random seed: `default`, or `NF_SEED` to play them all with another (the tests passed with 51 odd seeds from
/// 1 to 101; an even one plays as the odd one above it).
fn seed(default: u64) -> u64 {
    std::env::var("NF_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Scene {
    pub(super) fn new(mic: Delivery, lb: Delivery, ppm: f64) -> Self {
        #[rustfmt::skip]
        let sc = Self {
            mic, lb, ppm, lb_start_ms: -3.7, stamps: Stamps::Both, mic_stamps: MicStamps::Cables, event: Ev::None, at_s: EVENT_S,
            seed: seed(0x5eed_ca11),
        };
        sc
    }
    fn with(self, event: Ev) -> Self {
        Self { event, ..self }
    }
    pub(super) fn at(self, event: Ev, at_s: f64) -> Self {
        Self { event, at_s, ..self }
    }
    pub(super) fn with_stamps(self, stamps: Stamps) -> Self {
        Self { stamps, ..self }
    }

    /// The scene's packets as a probe would log them, for `SCENE_S`.
    fn rows(&self) -> Vec<Row> {
        scene_rows(self, SCENE_S)
    }

    fn play(&self) -> (Pipeline, LagSpy) {
        self.play_for(SCENE_S)
    }

    fn play_for(&self, secs: f64) -> (Pipeline, LagSpy) {
        let mut spy = LagSpy(Vec::new(), self.ppm);
        let p = replay(&scene_rows(self, secs), &mut spy, &index_coded, &index_coded);
        (p, spy)
    }
}

/// One packet of a scene's stream: when it's ready, its stamp, the index it was tapped at and its device position; `None`
/// if it never comes.
type ScenePacket = Option<(f64, f64, u64, u64)>;

/// `sc`'s packets over `secs`, as a probe would log them.
pub(super) fn scene_rows(sc: &Scene, secs: f64) -> Vec<Row> {
    let mut rng = Rng(sc.seed | 1);
    let ev = sc.at_s * 1e7;
    let lpkt = PKT / (1.0 + sc.ppm * 1e-6);
    let lb0 = sc.lb_start_ms * 1e4;
    let packets = (secs * 1e7 / PKT) as usize;
    let mut stream = |n: usize, start: &dyn Fn(usize) -> f64, len: f64, bursty: &[f64], d: Delivery, mic: bool| {
        let mut out: Vec<ScenePacket> = Vec::with_capacity(n);
        let (mut lat, mut last, mut skipped, mut spiked) = (0.5, f64::MIN, 0u64, false);
        for k in 0..n {
            let t = start(k);
            let d = if !mic && sc.event == Ev::LoopbackSteady && t >= ev { Delivery::Steady } else { d };
            if d == Delivery::Bursty && rng.unit() < 0.5 {
                lat = rng.pick(bursty);
            }
            let base = if d == Delivery::Bursty { lat } else { 0.5 + 0.3 * rng.unit() };
            let mut ready = t + len + base * 1e4;
            let (stall, lost, silent) = match (sc.event, mic) {
                (Ev::MicStall(ms), true) | (Ev::LoopbackStall(ms), false) => (Some(ms), None, false),
                (Ev::MicLost(ms), true) | (Ev::LoopbackLost(ms), false) => (None, Some(ms), false),
                (Ev::LoopbackSilent(ms), false) => (None, Some(ms), true),
                _ => (None, None, false),
            };
            if let Some(ms) = stall
                && ready >= ev
                && ready < ev + ms * 1e4
            {
                ready = ev + ms * 1e4;
            }
            if let (Ev::LoopbackLater(ms), false) = (sc.event, mic)
                && t >= ev
            {
                ready += ms * 1e4;
            }
            ready = ready.max(last);
            last = ready;
            // The mic's stamps as `sc.mic_stamps` says (at delivery: filled in when read); the loopback's within 0.02 ms.
            let mut jitter = match (mic, sc.mic_stamps) {
                (true, MicStamps::Cables) => rng.pick(&[-0.5, 0.0, 0.5, 1.0]) * 1e4,
                (true, MicStamps::Uniform(ms)) => (2.0 * rng.unit() - 1.0) * ms * 1e4,
                (true, MicStamps::AtDelivery) => 0.0,
                (false, _) => (rng.unit() - 0.5) * 400.0,
            };
            if let (Ev::LoopbackStampOff(ms), false) = (sc.event, mic)
                && t >= ev
                && !spiked
            {
                (jitter, spiked) = (jitter + ms * 1e4, true);
            }
            let gone = lost.is_some_and(|ms| t >= ev && t < ev + ms * 1e4);
            if gone && silent {
                skipped += 1;
            }
            out.push((!gone).then_some((ready, t + jitter, k as u64 * 480, (k as u64 - skipped) * 480)));
        }
        out
    };
    let mic = stream(packets, &|k| k as f64 * PKT, PKT, &[0.5, 10.5], sc.mic, true);
    let lb = stream(packets + 10, &|k| lb0 + k as f64 * lpkt, lpkt, &[0.5, 10.5, 20.5], sc.lb, false);
    let mut rows = Vec::new();
    let (mut m, mut l) = (0, 0);
    let mut w = 1;
    while w <= packets {
        let mut now = w as f64 * PKT + 7_000.0 + 3_000.0 * rng.unit();
        if let Ev::LateWake(ms) = sc.event
            && now >= ev
            && now < ev + PKT
        {
            now += ms * 1e4;
            w += (ms * 1e4 / PKT) as usize;
        }
        for (kind, packets, next) in [(0u8, &mic, &mut m), (1u8, &lb, &mut l)] {
            while let Some(p) = packets.get(*next) {
                match p {
                    Some((ready, ..)) if *ready > now => break,
                    Some((_, stamp, code, pos)) => {
                        let stamped = match sc.stamps {
                            Stamps::Both => true,
                            Stamps::MicOnly => kind == 0,
                            Stamps::LoopbackOnly => kind == 1,
                            Stamps::Neither => false,
                        };
                        let stamp = if kind == 0 && sc.mic_stamps == MicStamps::AtDelivery { now - PKT } else { *stamp };
                        rows.push(Row { now, kind, frames: 480, pos: *pos, qpc: if stamped { stamp } else { NO_STAMP }, code: *code });
                    }
                    None => {}
                }
                *next += 1;
            }
        }
        w += 1;
    }
    rows
}

/// The slips the loopback's drift calls for after the start-up (2.5 s), give or take 5% and 40: the lead can sit just
/// inside the dead band (±24) and then, nudged out of it, be put back within its release (±6), or the other way.
fn drift_slips(ppm: f64, secs: f64) -> Range<f64> {
    let expect = ppm.abs() * 48_000.0 * 1e-6 * (secs - 2.5);
    (expect * 0.95 - 40.0).max(0.0)..expect * 1.05 + 40.0
}

/// The cables' delivery, steady or bursty on either side, at 0-300 ppm: after the start-up the echo path never moves but
/// by drift slips at the drift's rate (it stays within the dead band of the clocks' own line), and nothing jumps. A bursty
/// loopback raises the target in the start-up, to cover its 20-30 ms swing, and no further than the cap. Every block's
/// reference is then whole, but for a drifting bursty loopback's: its phase against the mic's wakes moves, and with it the
/// worst need, so when it reaches a phase the start-up didn't see, blocks come up short (owed: the timeline stays whole).
/// When that persists the target rises once, a counted jump, and covers that phase from then on.
#[test]
fn bursty_delivery_leaves_the_echo_path_alone() {
    for mic in [Delivery::Steady, Delivery::Bursty] {
        for lb in [Delivery::Steady, Delivery::Bursty] {
            for ppm in [0.0, 30.0, -100.0, 300.0] {
                let sc = Scene::new(mic, lb, ppm);
                let (p, spy) = sc.play();
                let s = path_stats(&p, &spy, SETTLED);
                let drifting = lb == Delivery::Bursty && ppm != 0.0;
                let (owed, raises) = if drifting { (100, 1) } else { (0, 0) };
                assert!(s.drops <= raises && s.moves == s.drops as usize && s.unserved == 0, "{sc:?}: {s:?}");
                assert!(s.filled <= owed, "{sc:?}: {s:?}");
                assert!(drift_slips(ppm, SCENE_S).contains(&(s.slips as f64)), "{sc:?}: {s:?}");
                // Until the drift fit is trusted (10 s) the offset correction alone follows the drift, ppm / 10 off.
                let (most, _) = spy.wander(SETTLED, ppm);
                assert!(most <= 30.0 + ppm.abs() / 10.0, "{sc:?}: strayed {most:.0} samples from the clocks' line");
                assert!(s.target_ms <= LEAD_CAP as f32 / 48.0, "{sc:?}: {s:?}");
                if lb == Delivery::Bursty {
                    assert!(s.target_ms > 20.0, "{sc:?}: {s:?}");
                }
            }
        }
    }
}

/// A bursty loopback drifting 300 or 100 ppm for 170 s: its phase against the mic's wakes sweeps five periods (or two),
/// and the need with it. At the first phase the start-up didn't cover, a few seconds of short blocks raise the target
/// once; from then on every block is whole, and nothing else jumps.
#[test]
fn a_drifting_bursty_loopback_raises_the_target_once() {
    for ppm in [300.0, -100.0] {
        let sc = Scene { seed: seed(0x0dd_ba11), ..Scene::new(Delivery::Bursty, Delivery::Bursty, ppm) };
        let (p, spy) = sc.play_for(170.0);
        let s = path_stats(&p, &spy, SETTLED);
        assert_eq!((s.moves, s.unserved, s.drops), (1, 0, 1), "{ppm} ppm: {s:?}");
        assert!(s.filled <= 150, "{ppm} ppm: {s:?}");
        let raised = spy.0.iter().rposition(|x| x.2 > 0).unwrap_or(0);
        assert!(raised < 6_000, "{ppm} ppm: still short at block {raised}");
        assert!(drift_slips(ppm, 170.0).contains(&(s.slips as f64)), "{ppm} ppm: {s:?}");
        assert!(spy.wander(SETTLED, ppm).0 <= 60.0, "{ppm} ppm: {s:?}");
        assert!(s.target_ms <= LEAD_CAP as f32 / 48.0, "{ppm} ppm: {s:?}");
    }
}

/// A loopback whose delivery turns 15 ms later for good comes up short on every block: the target rises once, a counted
/// jump, 0.6 s on if every take comes up short (steady), a few seconds if some do (bursty). The steady one's reference is
/// whole again; the bursty one now needs up to 45 ms, close to the cap, and the odd block is still owed.
#[test]
fn a_loopback_that_turns_later_raises_the_target_once() {
    for lb in [Delivery::Steady, Delivery::Bursty] {
        let sc = Scene::new(Delivery::Bursty, lb, 30.0).with(Ev::LoopbackLater(15.0));
        let (p, spy) = sc.play();
        let s = path_stats(&p, &spy, SETTLED);
        let late = path_stats(&p, &spy, ((EVENT_S + 12.0) * 100.0) as usize);
        assert_eq!((s.drops, s.moves), (1, 1), "{sc:?}: {s:?}");
        assert!(s.filled <= 100, "{sc:?}: {s:?}");
        let owed = if lb == Delivery::Steady { 0 } else { (spy.0.len() - (EVENT_S as usize + 12) * 100) / 10 };
        assert!(late.filled <= owed, "{sc:?}: {late:?}");
        assert!(s.target_ms <= LEAD_CAP as f32 / 48.0, "{sc:?}: {s:?}");
    }
}

/// Late wakes and stalls that the buffers ride out leave the echo path where it was, as do lost packets the position or
/// the loopback's stamps account for, and a loopback stamp that's off on its own (it's left out). A stalled loopback's late
/// blocks are served partly as zeros (owed: the late samples are discarded), and so are a lost loopback's. Far-end silence
/// without the keep-alive (the loopback delivers nothing, and its position carries on where it stopped) is all zeros, and
/// after it the next two stamps put the timeline back where the samples belong: no jump, and the target as it was. Only
/// the mic losing more than the 100 ms it zero-fills breaks the timeline: its stamps run on past its samples, and one
/// counted jump puts the reference back on them.
#[test]
fn events_cost_at_most_one_jump() {
    #[rustfmt::skip]
    let cases = [
        // (event, jumps, most blocks with filled-in samples)
        (Ev::LateWake(15.0), 0, 0),
        (Ev::LateWake(50.0), 0, 0),
        (Ev::MicStall(60.0), 0, 0),
        (Ev::MicLost(50.0), 0, 0),
        (Ev::MicLost(200.0), 1, 0),
        (Ev::LoopbackStall(50.0), 0, 6),
        (Ev::LoopbackLost(50.0), 0, 7),
        (Ev::LoopbackLost(200.0), 0, 22),
        (Ev::LoopbackSilent(200.0), 0, 22),
        (Ev::LoopbackSilent(3_000.0), 0, 303),
        (Ev::LoopbackSilent(10_000.0), 0, 1_003),
        (Ev::LoopbackStampOff(5.0), 0, 0),
        (Ev::LoopbackStampOff(-5.0), 0, 0),
        (Ev::LoopbackStampOff(30.0), 0, 0),
    ];
    let quiet = [Delivery::Steady, Delivery::Bursty].map(|lb| Scene::new(Delivery::Bursty, lb, 30.0).play().0.ref_target());
    for (ev, jumps, filled) in cases {
        for (lb, quiet) in [Delivery::Steady, Delivery::Bursty].into_iter().zip(quiet) {
            let sc = Scene::new(Delivery::Bursty, lb, 30.0).with(ev);
            let (p, spy) = sc.play();
            let s = path_stats(&p, &spy, SETTLED);
            assert_eq!(s.drops, jumps, "{sc:?}: {s:?}");
            assert!(s.filled <= filled && s.unserved <= filled, "{sc:?}: {s:?}");
            assert!(drift_slips(30.0, SCENE_S).contains(&(s.slips as f64)), "{sc:?}: {s:?}");
            assert_eq!(p.ref_target(), quiet, "{sc:?}: the target as without the event");
            // The mic's own loss moves the path, and the jump moves it back: two moves, the path as it was.
            assert_eq!(s.moves, 2 * jumps as usize, "{sc:?}: {s:?}");
            let (most, end) = spy.wander(SETTLED, 30.0);
            assert!(end <= 30.0 && (jumps > 0 || most <= 30.0), "{sc:?}: strayed {most:.0}, {end:.0} at the end");
        }
    }
}

/// Far-end silence without the keep-alive, 3 s or 10 s, drifting either way: no jump, the target as it was, and the echo
/// path where it was afterwards (the loop slipped on at its drift estimate meanwhile). (At -300 ppm the bursty loopback's
/// phase raises the target once with or without the silence.) Without stamps the silence's length can't be known but from
/// the delivery: a break, put back in a jump (and a centring), and the target as it was too.
#[test]
fn silence_without_the_keep_alive_leaves_the_target() {
    for ppm in [30.0, -300.0] {
        let quiet = Scene::new(Delivery::Bursty, Delivery::Bursty, ppm);
        let (q, spy) = quiet.play();
        let qs = path_stats(&q, &spy, SETTLED);
        for ms in [3_000.0, 10_000.0] {
            let sc = quiet.with(Ev::LoopbackSilent(ms));
            let (p, spy) = sc.play();
            let s = path_stats(&p, &spy, SETTLED);
            assert_eq!((s.drops, s.moves, p.ref_target()), (qs.drops, qs.moves, q.ref_target()), "{sc:?}: {s:?}");
            assert!(spy.wander(SETTLED, ppm).0 <= 60.0, "{sc:?}: {s:?}");
            let sc = Scene { stamps: Stamps::Neither, ..sc };
            let (p, _) = sc.play();
            let (q, _) = Scene { stamps: Stamps::Neither, ..quiet }.play();
            let more = p.counters.reference_drops - q.counters.reference_drops;
            assert!(more <= 2 && p.ref_target() == q.ref_target(), "{sc:?}: {:?}", p.counters);
        }
    }
}

/// A stall in the start-up, or far-end silence across it, leaves the target where the delivery alone puts it: a stalled
/// loopback's takes are stale, not a lead too short, and a need counts only once three takes have reached it.
#[test]
fn a_stall_in_the_start_up_doesnt_raise_the_target() {
    #[rustfmt::skip]
    let cases = [
        (Delivery::Steady, Ev::LoopbackStall(60.0), 0.5),
        (Delivery::Steady, Ev::LoopbackStall(100.0), 1.0),
        (Delivery::Bursty, Ev::LoopbackStall(100.0), 1.0),
        (Delivery::Steady, Ev::LoopbackSilent(3_000.0), 1.0),
        (Delivery::Bursty, Ev::LoopbackSilent(3_000.0), 1.0),
    ];
    for (d, ev, at_s) in cases {
        let quiet = Scene::new(d, d, 30.0);
        let (q, _) = quiet.play();
        let sc = quiet.at(ev, at_s);
        let (p, spy) = sc.play();
        let s = path_stats(&p, &spy, 500);
        assert_eq!(p.ref_target(), q.ref_target(), "{sc:?}: {s:?}");
        assert_eq!((s.drops, s.moves, s.unserved, s.filled), (0, 0, 0, 0), "{sc:?}: {s:?}");
    }
}

/// A loopback whose need falls for good (bursty for 10 s, steady from then on) gets its target back down, in one jump,
/// once the need has stayed 3 ms or more under it for five minutes: to the need + 3 ms, about the start's period + 2 ms
/// (steady delivery at the worst phase of the clocks needs about a period).
#[test]
fn the_target_comes_down_after_five_minutes() {
    let sc = Scene::new(Delivery::Steady, Delivery::Bursty, 30.0).at(Ev::LoopbackSteady, 10.0);
    let (p, spy) = sc.play_for(330.0);
    let s = path_stats(&p, &spy, SETTLED);
    assert_eq!((s.drops, s.moves, s.unserved, s.filled), (1, 1, 0, 0), "{s:?}");
    assert!(p.ref_target().abs_diff(cable_pipe().ref_target()) <= 96, "{s:?}");
    let (p, _) = sc.play_for(300.0);
    assert!(p.ref_target() > cable_pipe().ref_target() + 400, "not before five minutes: {}", p.ref_target());
}

/// Mic stamps that jitter by ±3 ms still time the lead: the loop slips at the drift (and corrects the jitter's slow wander
/// past the dead band now and then, a few dozen slips more) and nothing jumps. Stamps that scatter
/// wider (±5 ms), or that a driver puts on at delivery, are given up for the delivery's timing within the first seconds
/// (the start-up starts over), and then nothing slips, and nothing jumps but a step of the count at 300 ppm: no break
/// from a stamp that's off.
#[test]
fn stamps_that_scatter_are_given_up() {
    for ppm in [30.0, -300.0] {
        let sc = Scene { mic_stamps: MicStamps::Uniform(3.0), ..Scene::new(Delivery::Bursty, Delivery::Steady, ppm) };
        let (p, spy) = sc.play();
        let s = path_stats(&p, &spy, SETTLED);
        assert!(p.reference.stamped() && s.drops == 0 && s.moves == 0, "{sc:?}: {s:?}");
        let r = drift_slips(ppm, SCENE_S);
        assert!((r.start - 30.0..r.end + 30.0).contains(&(s.slips as f64)), "{sc:?}: {s:?}");
        for mic_stamps in [MicStamps::Uniform(5.0), MicStamps::AtDelivery] {
            let sc = Scene { mic_stamps, ..sc };
            let (p, spy) = sc.play();
            let s = path_stats(&p, &spy, 800);
            assert!(!p.reference.stamped(), "{sc:?}: {s:?}");
            assert!(s.drops <= u64::from(ppm.abs() > 100.0) && s.moves == s.drops as usize && s.slips == 0, "{sc:?}: {s:?}");
            assert!(s.unserved <= s.drops as usize, "{sc:?}: {s:?}");
        }
    }
}

/// Without stamps (on either stream or both) the lead is timed by delivery: the bursts swing it, and delivery at the mic's
/// wakes hides the drift until the count steps a whole packet, once per beat of the two clocks (33 s at 300 ppm). So
/// nothing slips, the served path holds still meanwhile (the true echo path drifts away from it at the clocks' rate), and
/// each step is put back in one jump of a packet: a jump back serves a block of zeros (what it would serve again was
/// served already), and nothing else is lost. Over 170 s at ±300 ppm, five or six jumps; at 30 ppm over 40 s, one at most
/// (when the clocks' phase starts near a step). A mic losing 200 ms is put back in a jump by the readings' median, and a
/// centring 2.5 s later: two at most.
#[test]
fn without_stamps_nothing_slips() {
    for stamps in [Stamps::Neither, Stamps::MicOnly, Stamps::LoopbackOnly] {
        for ppm in [-300.0, 300.0] {
            let sc = Scene { stamps, ..Scene::new(Delivery::Steady, Delivery::Steady, ppm) };
            let (p, spy) = sc.play_for(170.0);
            let s = path_stats(&p, &spy, SETTLED);
            assert!((5..=6).contains(&s.drops) && s.moves == s.drops as usize && s.slips == 0, "{sc:?}: {s:?}");
            assert!(s.unserved <= s.drops as usize && s.filled <= s.drops as usize, "{sc:?}: {s:?}");
            assert!(!p.reference.stamped() && spy.wander(SETTLED, 0.0).0 <= 2.0, "{sc:?}: {s:?}");
        }
    }
    for (mic, lb) in [(Delivery::Steady, Delivery::Steady), (Delivery::Bursty, Delivery::Steady), (Delivery::Bursty, Delivery::Bursty)] {
        let sc = Scene { stamps: Stamps::Neither, ..Scene::new(mic, lb, 30.0) };
        let (p, spy) = sc.play();
        let s = path_stats(&p, &spy, SETTLED);
        assert!(s.drops <= 1 && s.moves == s.drops as usize && s.unserved <= s.drops as usize && s.slips == 0, "{sc:?}: {s:?}");
        assert!(spy.wander(SETTLED, 0.0).0 <= 2.0, "{sc:?}: {s:?}");
        let sc = sc.with(Ev::MicLost(200.0));
        let (p, spy) = sc.play();
        let s = path_stats(&p, &spy, SETTLED);
        // The loss itself is a move, then each jump.
        assert!(s.drops <= 2 && s.moves == s.drops as usize + 1 && s.slips == 0, "{sc:?}: {s:?}");
    }
}

/// Every scene the tests above sample, printed; `--ignored --nocapture`.
#[test]
#[ignore]
fn scene_sweep() {
    for stamps in [Stamps::Both, Stamps::Neither] {
        for (mic, lb) in [(Delivery::Steady, Delivery::Steady), (Delivery::Bursty, Delivery::Steady), (Delivery::Bursty, Delivery::Bursty)]
        {
            for ppm in [0.0, 30.0, -100.0, 300.0] {
                #[rustfmt::skip]
                let events = [
                    Ev::None, Ev::LateWake(50.0), Ev::MicStall(60.0), Ev::MicLost(200.0), Ev::LoopbackStall(50.0),
                    Ev::LoopbackLost(200.0), Ev::LoopbackSilent(3_000.0), Ev::LoopbackLater(15.0), Ev::LoopbackStampOff(5.0),
                ];
                for ev in events {
                    let sc = Scene { stamps, ..Scene::new(mic, lb, ppm).with(ev) };
                    let (p, spy) = sc.play();
                    let s = path_stats(&p, &spy, SETTLED);
                    let (most, end) = spy.wander(SETTLED, ppm);
                    eprintln!(
                        "{stamps:?} {mic:?}/{lb:?} {ppm:5} ppm {ev:?}: gaps {}, drops {}, slips {} (drift {:.0}); after 3 s: moves {}, \
                         unserved {}, filled {}, off the clocks' line {most:.0} (end {end:.0}); target {:.1} ms",
                        s.gaps,
                        s.drops,
                        s.slips,
                        ppm.abs() * 0.048 * (SCENE_S - 2.5),
                        s.moves,
                        s.unserved,
                        s.filled,
                        s.target_ms
                    );
                }
            }
        }
    }
}

// ---- Live traces ---------------------------------------------------------------------------------------------------

/// Replays every trace in `%LOCALAPPDATA%\noise-filter\traces` with index-coded samples and prints what happened to the
/// echo path: the reference counters, the moves and the lag's range after 3 s, and its range each second. Asserts the
/// timeline goal: after 3 s no moves, every block's reference whole, and the target under its cap. Skipped when there
/// are no traces; `--ignored --nocapture`.
#[test]
#[ignore]
fn traces_hold_the_echo_path() {
    let all = traces();
    if all.is_empty() {
        eprintln!("no traces in {:?}", trace_dir());
        return;
    }
    for (name, rows) in &all {
        let mut spy = LagSpy::default();
        let p = replay(rows, &mut spy, &index_coded, &index_coded);
        let s = path_stats(&p, &spy, SETTLED);
        let per_s: Vec<String> = spy
            .0
            .chunks(100)
            .map(|sec| {
                let l: Vec<f64> = sec.iter().filter_map(|&(m, r, _)| Some(m? - r?)).collect();
                let (lo, hi) = l.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
                if l.is_empty() {
                    "-".into()
                } else if lo == hi {
                    format!("{lo:.0}")
                } else {
                    format!("{lo:.0}..{hi:.0}")
                }
            })
            .collect();
        eprintln!(
            "{name}: {} blocks; reference gaps {}, drops {}, slips {}; after 3 s: echo-path moves {}, unserved blocks {}, \
             partly filled {}, lag {:.0}..{:.0}; target {:.1} ms\n  lag per second: {}",
            spy.0.len(),
            s.gaps,
            s.drops,
            s.slips,
            s.moves,
            s.unserved,
            s.filled,
            s.lag_lo,
            s.lag_hi,
            s.target_ms,
            per_s.join(" ")
        );
        assert_eq!((s.moves, s.unserved, s.filled), (0, 0, 0), "{name}: {s:?}");
        assert!(s.target_ms <= LEAD_CAP as f32 / 48.0, "{name}: {s:?}");
    }
}

/// AEC3 behind the pipeline: the mic blocks the pipeline served, and what AEC3 made of them.
struct AecProc {
    stage: Aec3Stage,
    mic: Vec<f32>,
    out: Vec<f32>,
    stats: Vec<AecStats>,
}

impl AecProc {
    fn new() -> Self {
        let mut stage = Aec3Stage::new(Arc::new(ChainControls::new(&ProcessingConfig::default()))).unwrap();
        stage.activate(&Activation::HOST).unwrap();
        Self { stage, mic: Vec::new(), out: Vec::new(), stats: Vec::new() }
    }
}

impl BlockProcessor for AecProc {
    fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        self.stage.process(ProcessIo::new(&[mic, r], out));
        self.mic.extend_from_slice(mic);
        self.out.extend_from_slice(out);
        self.stats.push(self.stage.aec_stats());
        ChainReport::default()
    }
    fn aec_stats(&self) -> Option<AecStats> {
        Some(self.stage.aec_stats())
    }
}

/// A least-squares line through a stream's stamps against the index its samples were tapped at: (hns at index 0, hns per
/// frame).
fn stamp_line(rows: &[Row], kind: u8) -> (f64, f64) {
    let pts: Vec<(f64, f64)> = rows.iter().filter(|r| r.kind == kind && r.qpc != NO_STAMP).map(|r| (r.code as f64, r.qpc)).collect();
    let n = pts.len() as f64;
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let b = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / pts.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
    (my - b * mx, b)
}

/// The same packets on the same clocks, delivered as soon as they're complete: one mic packet a wake (10 ms after its
/// first sample, plus 0.5 ms), and every loopback packet that has ended by then (with the keep-alive's: none is missing).
fn steady_rows(rows: &[Row]) -> Vec<Row> {
    let (m, l) = (stamp_line(rows, 0), stamp_line(rows, 1));
    let first = |kind: u8| rows.iter().find(|r| r.kind == kind).map_or(0, |r| r.code);
    let last = |kind: u8| rows.iter().filter(|r| r.kind == kind).map(|r| r.code).max().unwrap_or(0);
    let (m0, l0) = (first(0), first(1));
    let (mic_packets, lb_packets) = ((last(0) - m0) / 480 + 1, (last(1) - l0) / 480 + 1);
    let mut out = Vec::new();
    let mut k = 0u64;
    for j in 0..mic_packets {
        let pos = m0 + j * 480;
        let now = m.0 + m.1 * (pos + 480) as f64 + 5_000.0;
        out.push(Row { now, kind: 0, frames: 480, pos, qpc: m.0 + m.1 * pos as f64, code: pos });
        while k < lb_packets && l.0 + l.1 * (l0 + (k + 1) * 480) as f64 <= now {
            let pos = l0 + k * 480;
            out.push(Row { now, kind: 1, frames: 480, pos, qpc: l.0 + l.1 * pos as f64, code: pos });
            k += 1;
        }
    }
    out
}

/// The quality test's echo scene (nf-app `quality/signals.rs`), for the offline check: 3 s turns of far only, double talk
/// and near only, over and over, far first. The far voice (high, -20 dBFS active level) is what the loopback taps; the mic
/// hears it through a room (RT60 0.15 s) 6 dB under the near voice (low, -20 dBFS), plus pink noise 60 dB under that.
/// `near`: whether the near voice talks (else the mic hears only the echo and the noise). `silent`: tap indices where the
/// far end plays nothing (its silence without the keep-alive).
struct EchoScene {
    far: Vec<f32>,
    /// The far voice through the room, by tap index.
    echo: Vec<f32>,
    near: Vec<f32>,
    noise: Vec<f32>,
}

const TURN: usize = 3 * 48_000;

/// Which turn sample `i` is in: 0 far only, 1 double talk, 2 near only.
fn turn_of(i: usize) -> usize {
    (i / TURN) % 3
}

/// Zeroes `x` outside the turns `keep` takes, with 10 ms raised-cosine fades inside the kept stretches' ends.
fn gate_turns(x: &mut [f32], keep: impl Fn(usize) -> bool) {
    const FADE: usize = 480;
    let len = x.len();
    let kept = |i: usize| i < len && keep(turn_of(i));
    let mut start = 0;
    while start < len {
        let on = kept(start);
        let mut end = start;
        while end < len && kept(end) == on {
            end += TURN - end % TURN;
        }
        let end = end.min(len);
        for (i, v) in x.iter_mut().enumerate().take(end).skip(start) {
            let edge = (i - start).min(end - 1 - i);
            let g = if !on {
                0.0
            } else if edge >= FADE {
                1.0
            } else {
                0.5 - 0.5 * (std::f64::consts::PI * (edge as f64 + 0.5) / FADE as f64).cos()
            };
            *v *= g as f32;
        }
        start = end;
    }
}

impl EchoScene {
    fn new(len: usize, near: bool, silent: Option<Range<usize>>) -> Self {
        let mut far = synth::speech_like(len, synth::Voice::HIGH, 22).samples;
        gate_turns(&mut far, |t| t != 2);
        if let Some(r) = silent {
            far[r.start.min(len)..r.end.min(len)].fill(0.0);
        }
        let mut talker = synth::speech_like(len, synth::Voice::LOW, 21).samples;
        gate_turns(&mut talker, |t| t != 0);
        let rir = synth::room_impulse_response(7200, 0.15, 24);
        let echo = synth::convolve(&far, &rir)[..len].to_vec();
        let near_rms = synth::rms(&talker);
        let echo = synth::scaled(&echo, near_rms * nf_core::db_to_lin(-6.0) / synth::rms(&echo).max(1e-9));
        let noise = synth::pink_noise(len, near_rms * nf_core::db_to_lin(-60.0), 23);
        Self { far, echo, near: if near { talker } else { vec![0.0; len] }, noise }
    }
}

/// What AEC3 did with one run: its own ERLE (the mean of its reports from 10 s on) and its delay estimate at the end; the
/// echo removed where the far voice sounds and the near one doesn't, from 9 s on (the median of the 20 ms frames', so a
/// moment of divergence doesn't decide it); and in the double talk from 12 s on, where the near voice sounds, its own
/// share's change (per 20 ms frame, the output's projection on it, as the quality test measures it). NaN without a near
/// voice.
#[derive(Clone, Copy, Debug)]
struct AecResult {
    erle: f32,
    delay: f32,
    far_only: f32,
    dt_near: f32,
}

fn aec_result(a: &AecProc, sc: &EchoScene, echo_at: &dyn Fn(usize) -> f32) -> AecResult {
    const FRAME: usize = 960;
    let lat = nf_stages::AEC3_LATENCY as usize;
    let n = a.out.len().saturating_sub(lat).min(a.mic.len());
    let erle: Vec<f32> = a.stats.iter().skip(1000).map(|s| s.erle_db).filter(|v| v.is_finite()).collect();
    let energy = |f: &dyn Fn(usize) -> f64, r: Range<usize>| r.map(|i| f(i).powi(2)).sum::<f64>();
    let echo = |i: usize| f64::from(echo_at(i));
    let near = |i: usize| f64::from(sc.near.get(i).copied().unwrap_or(0.0));
    let (mic, out) = (|i: usize| f64::from(a.mic[i]), |i: usize| f64::from(a.out[i + lat]));
    // A frame "sounds" within 20 dB of the loudest frame of its kind.
    let loudest = |f: &dyn Fn(usize) -> f64| (0..n / FRAME).map(|k| energy(f, k * FRAME..(k + 1) * FRAME)).fold(0.0, f64::max);
    let (echo_top, near_top) = (loudest(&echo), loudest(&near));
    let db = |a: f64, b: f64| if b > 0.0 { (10.0 * (a / b).log10()) as f32 } else { f32::NAN };
    let (mut far_only, mut clean, mut share) = (Vec::new(), 0.0, 0.0);
    for k in 0..n / FRAME {
        let r = k * FRAME..(k + 1) * FRAME;
        let (echoing, talking) = (energy(&echo, r.clone()) > 0.01 * echo_top, near_top > 0.0 && energy(&near, r.clone()) > 0.01 * near_top);
        if r.start >= 9 * 48_000 && echoing && energy(&near, r.clone()) == 0.0 {
            far_only.push(db(energy(&mic, r.clone()), energy(&out, r.clone())));
        }
        if r.start >= 12 * 48_000 && turn_of(r.start) == 1 && talking {
            let cc = energy(&near, r.clone());
            let g = r.map(|i| out(i) * near(i)).sum::<f64>() / cc;
            (clean, share) = (clean + cc, share + g * g * cc);
        }
    }
    far_only.sort_by(f32::total_cmp);
    AecResult {
        erle: erle.iter().sum::<f32>() / erle.len().max(1) as f32,
        delay: a.stats.last().map_or(f32::NAN, |s| s.echo_delay_ms),
        far_only: far_only.get(far_only.len() / 2).copied().unwrap_or(f32::NAN),
        dt_near: db(share, clean),
    }
}

/// Replays `rows` with the echo scene's audio through the pipeline into AEC3, the echo `echo_ms` behind the loopback's
/// tap (by the rows' own stamp lines, so the clocks' drift is in it). Returns the pipeline and the result.
fn aec_run(rows: &[Row], sc: &EchoScene, echo_ms: f64) -> (Pipeline, AecResult) {
    let (m, l) = (stamp_line(rows, 0), stamp_line(rows, 1));
    let m0 = rows.iter().find(|r| r.kind == 0).map_or(0, |r| r.code);
    let l0 = rows.iter().find(|r| r.kind == 1).map_or(0, |r| r.code);
    // Mic sample `i` (from the first) holds the echo of the far sample the loopback tapped `echo_ms` before it.
    let echo_at = |i: usize| -> f32 {
        let t = m.0 + m.1 * (m0 as usize + i) as f64 - echo_ms * 1e4;
        let r = (t - l.0) / l.1 - l0 as f64;
        let k = r.floor();
        let f = (r - k) as f32;
        let at = |j: f64| if j >= 0.0 { sc.echo.get(j as usize).copied().unwrap_or(0.0) } else { 0.0 };
        at(k) + (at(k + 1.0) - at(k)) * f
    };
    let mic_of = |pos: u64, n: usize| -> Vec<f32> {
        (pos as usize..pos as usize + n)
            .map(|i| echo_at(i) + sc.near.get(i).copied().unwrap_or(0.0) + sc.noise.get(i).copied().unwrap_or(0.0))
            .collect()
    };
    let lb_of =
        |pos: u64, n: usize| -> Vec<f32> { (pos..pos + n as u64).map(|i| sc.far.get(i as usize).copied().unwrap_or(0.0)).collect() };
    let mut a = AecProc::new();
    let p = replay(rows, &mut a, &mic_of, &lb_of);
    let r = aec_result(&a, sc, &echo_at);
    (p, r)
}

/// The offline AEC3 check (TODO 10): each trace's delivery, and the synthetic scenes' (bursty; a stall in the start-up; 3 s
/// of far-end silence without the keep-alive), replayed with the echo scene through AEC3, against the same audio delivered
/// steadily (by the same stamps' lines, every packet as soon as it's complete), with the echo path's delay at 25, 40, 60
/// and 100 ms. Twice: with the near voice silent, which shows AEC3 converging on the reference, and with it talking in its
/// turns, as the quality test plays it. AEC3's own divergences in double talk (now and then, for live and steady delivery
/// alike) make the second run's figures move by tens of dB from one delivery to the other, so it's printed, not asserted.
/// Prints AEC3's own ERLE and delay, the echo removed where only the far end talks, and in double talk the near voice's
/// change. Asserts that where the lead fits in the echo path (the target 3 ms or more under its delay), without the near
/// voice, the live AEC3 ERLE is within 3 dB of the steady one. `--ignored --nocapture`; `NF_AEC_ONLY=<part of a name>`
/// runs only those.
#[test]
#[ignore]
fn aec3_converges_on_the_traces() {
    use Delivery::{Bursty as B, Steady as S};
    let mut runs: Vec<(String, Vec<Row>, Option<Range<usize>>)> = traces().into_iter().map(|(n, r)| (n, r, None)).collect();
    #[rustfmt::skip]
    let scenes = [
        ("synthetic bursty, 30 ppm", Scene::new(B, B, 30.0), None),
        ("synthetic bursty, 300 ppm", Scene::new(B, B, 300.0), None),
        ("synthetic steady, a 100 ms loopback stall at 1 s", Scene::new(S, S, 30.0).at(Ev::LoopbackStall(100.0), 1.0), None),
        ("synthetic bursty, a 100 ms loopback stall at 1 s", Scene::new(B, B, 30.0).at(Ev::LoopbackStall(100.0), 1.0), None),
        ("synthetic bursty, 3 s far-end silence at 20 s", Scene::new(B, B, 30.0).at(Ev::LoopbackSilent(3000.0), 20.0),
            Some(20 * 48_000..23 * 48_000)),
    ];
    for (name, sc, silent) in scenes {
        runs.push((name.into(), sc.rows(), silent));
    }
    let only = std::env::var("NF_AEC_ONLY").unwrap_or_default();
    let mut failed = Vec::new();
    for (name, rows, silent) in runs.iter().filter(|r| r.0.contains(only.as_str())) {
        let len = (rows.last().map_or(0.0, |r| r.now / 1e7).ceil() as usize + 1) * 48_000;
        let steady = steady_rows(rows);
        eprintln!("{name}:");
        for near in [false, true] {
            let sc = EchoScene::new(len, near, silent.clone());
            for echo_ms in [25.0, 40.0, 60.0, 100.0] {
                let (p, live) = aec_run(rows, &sc, echo_ms);
                let (_, st) = aec_run(&steady, &sc, echo_ms);
                let target = p.ref_target() as f64 / 48.0;
                let c = p.counters;
                let dt = if near { format!("; double talk: near {:+5.1} dB ({:+5.1})", live.dt_near, st.dt_near) } else { String::new() };
                eprintln!(
                    "  {} echo {echo_ms:3.0} ms, target {target:4.1} ms: AEC3 ERLE {:5.1} dB (steady {:5.1}), delay {:3.0} ms ({:3.0}); \
                     far only {:5.1} dB ({:5.1}){dt}; gaps {}, drops {}, slips {}",
                    if near { "talking" } else { "silent " },
                    live.erle,
                    st.erle,
                    live.delay,
                    st.delay,
                    live.far_only,
                    st.far_only,
                    c.reference_gaps,
                    c.reference_drops,
                    c.reference_slips
                );
                if !near && target + 3.0 <= echo_ms && live.erle < st.erle - 3.0 {
                    failed.push(format!("{name}, echo {echo_ms} ms: {live:?} against {st:?}"));
                }
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// The traces' mic delivery (the cables' shared mic, each trace looped to 330 s) against the adaptive output target on a
/// shared 10 ms device, with the trial and in the old single stage (out_target.rs): where each goes and whether either runs
/// dry. Prints; ignored (it needs the traces); `--ignored --nocapture`.
#[test]
#[ignore]
fn traces_and_the_adaptive_target() {
    let all = traces();
    if all.is_empty() {
        eprintln!("no traces in {:?}", trace_dir());
    }
    for (name, rows) in all {
        let mic: Vec<&Row> = rows.iter().filter(|r| r.kind == 0).collect();
        let (Some(first), Some(last)) = (mic.first(), mic.last()) else { continue };
        let pos0 = first.pos;
        let frames = last.pos + last.frames as u64 - pos0;
        let span = frames as f64 * 1e7 / 48_000.0;
        for single in [false, true] {
            let c = PipelineConfig { out_period: 480, out_margin: 96, ref_period: None, max_packet: 4800 };
            let p = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
            let mut p = if single { p.with_single_stage_target() } else { p.with_adaptive_target() };
            let mut sink = VSink::new(480, 0.0, 0.37, T0);
            p.prefill(&mut sink).unwrap();
            let tel = Telemetry::new();
            let mut capture = FakeCapture::default();
            let mut ticks = FakeTicks(0);
            let mut targets = Vec::new();
            'run: for k in 0u64.. {
                let shift = k as f64 * span;
                let mut i = 0;
                while i < mic.len() {
                    let now = mic[i].now;
                    while let Some(r) = mic.get(i).filter(|r| r.now == now) {
                        let q = (r.qpc != NO_STAMP).then_some((T0 + r.qpc + shift) as u64);
                        capture.push(r.pos - pos0 + k * frames, q, vec![0.1; r.frames]);
                        i += 1;
                    }
                    let t = T0 + now - first.now + shift;
                    if t - T0 > 330e7 {
                        break 'run;
                    }
                    sink.advance_to(t);
                    p.drain_mic(&mut capture, t).unwrap();
                    while p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, false).unwrap() {
                        if p.second_due() {
                            p.publish_second(&tel, &CopyChain, 0, 0, 0);
                            targets.push(p.out_target());
                        }
                    }
                }
            }
            let at = |s: usize| targets.get(s).map_or(0.0, |&t| t as f64 / 48.0);
            eprintln!(
                "{name} {}: target {:.1} ms at 119 s, {:.1} at 180, {:.1} at 239, {:.1} at 300, {:.1} at 329; {} underruns, need {:.1} ms",
                if single { "single stage" } else { "with a trial" },
                at(119),
                at(180),
                at(239),
                at(300),
                at(329),
                p.counters.render_underruns,
                p.adaptive.as_ref().map_or(0.0, |a| f64::from(a.need()) / 48.0)
            );
        }
    }
}
