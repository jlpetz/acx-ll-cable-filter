//! Running a cell block by block, and what is measured of it: each stage's cost per block, and the output's exact zeros, peak
//! and difference from today's dither, phase by phase.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use nf_core::{BLOCK, SAMPLE_RATE};
use nf_stages::bench::WARM_UP;

use crate::cells::Built;
use crate::signals::{DitherMode, Feed, Phase, Signal, warm_up};

/// One run's output and its per-block costs in ns: the whole `Chain::process_timed` call, the AEC slot, the noise slot and the
/// noise model's hops within it (0 where the stage didn't run).
pub struct Run {
    pub out: Vec<f32>,
    pub chain_ns: Vec<u64>,
    pub aec_ns: Vec<u64>,
    pub noise_ns: Vec<u64>,
    pub model_ns: Vec<u64>,
}

/// The engine's warm-up, then every block of `feed` through the chain, each timed alone (a stage's share by the chain's own
/// ticks, here ns from a monotonic clock). Everything is allocated before the first block; the loop only processes and
/// stores.
pub fn run(built: &mut Built, feed: &Feed, dither: DitherMode) -> Run {
    warm_up(&mut built.chain, dither);
    let n = feed.blocks();
    let mut r = Run {
        out: vec![0.0; n * BLOCK],
        chain_ns: Vec::with_capacity(n),
        aec_ns: Vec::with_capacity(n),
        noise_ns: Vec::with_capacity(n),
        model_ns: Vec::with_capacity(n),
    };
    let base = Instant::now();
    let ticks = || base.elapsed().as_nanos() as u64;
    for (b, out) in r.out.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        let (mic, reference) = feed.block(b);
        if let Some(m) = &built.model_ns {
            m.store(0, Relaxed);
        }
        let t = Instant::now();
        let report = built.chain.process_timed(mic, reference, out, ticks);
        r.chain_ns.push(t.elapsed().as_nanos() as u64);
        r.aec_ns.push(report.aec_ticks);
        r.noise_ns.push(report.noise_ticks);
        r.model_ns.push(built.model_ns.as_ref().map_or(0, |m| m.load(Relaxed)));
    }
    r
}

/// A row's figures in µs: nearest-rank p50 and p99 (as `nf_stages::bench::BlockTimes`), max and mean, over `n` blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

impl Stats {
    /// Of `us` (sorted here); `None` if empty.
    pub fn of(mut us: Vec<f64>) -> Option<Stats> {
        if us.is_empty() {
            return None;
        }
        us.sort_by(f64::total_cmp);
        let n = us.len();
        let at = |p: f64| us[((n as f64 * p / 100.0) as usize).min(n - 1)];
        Some(Stats { n, p50: at(50.0), p99: at(99.0), max: us[n - 1], mean: us.iter().sum::<f64>() / n as f64 })
    }
}

/// The stages' costs over a phase's timed blocks.
#[derive(Clone, Debug, Default)]
pub struct Timing {
    /// Blocks timed: the phase's, less any in the signal's first second ([`WARM_UP`], as the bench rows leave out).
    pub timed: usize,
    pub chain: Option<Stats>,
    pub aec3: Option<Stats>,
    /// The noise stage: its model, then the sanitising, the clamp, the floor mix and the gate.
    pub noise: Option<Stats>,
    pub model: Option<Stats>,
    /// The noise stage less its model's hops: the sanitising, the clamp, the floor and the gate (and the re-blocking for a hop
    /// other than a block).
    pub post: Option<Stats>,
    /// The chain's mean cost per second of the phase (every block, the first second's too), in µs.
    pub chain_mean_per_s: Vec<f64>,
}

/// What the output did over a phase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Output {
    /// Of the phase's output samples: exact zeros (either sign), and subnormals.
    pub zero_share: f64,
    pub subnormal_share: f64,
    /// From the phase's start to its first all-zero output block, and to the block from which every block to the phase's end
    /// is all zero, in ms; `None`: never.
    pub first_zero_ms: Option<f64>,
    pub all_zero_ms: Option<f64>,
    /// The largest |sample| in dBFS (`None`: all zero), over the phase and over its second half (past a chain's latency and
    /// the start of its tail: what a silent phase settles at).
    pub peak_dbfs: Option<f64>,
    pub late_peak_dbfs: Option<f64>,
}

/// How a phase's output differs from today's (the mic and the reference dithered) in the same cell otherwise.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diff {
    /// The largest |difference| and the difference's RMS over the phase, in dBFS (`None`: identical).
    pub max_dbfs: Option<f64>,
    pub rms_dbfs: Option<f64>,
    /// The share of samples that differ.
    pub differ_share: f64,
    /// The largest |difference| in each second of the phase (where it is: an onset, or throughout).
    pub max_dbfs_per_s: Vec<Option<f64>>,
}

/// One phase's results.
#[derive(Clone, Debug)]
pub struct PhaseResult {
    pub phase: Phase,
    pub timing: Timing,
    pub output: Output,
    pub vs_today: Option<Diff>,
}

const MS_PER_BLOCK: f64 = BLOCK as f64 * 1000.0 / SAMPLE_RATE as f64;
const BLOCKS_PER_S: usize = SAMPLE_RATE as usize / BLOCK;

/// Each phase of `signal`'s results from `run`; `today`, the output of the same cell with today's dither, if it ran.
pub fn phases(signal: &Signal, run: &Run, has_aec: bool, has_noise: bool, today: Option<&[f32]>) -> Vec<PhaseResult> {
    signal
        .phases
        .iter()
        .map(|p| {
            let samples = p.start * BLOCK..p.end * BLOCK;
            PhaseResult {
                phase: p.clone(),
                timing: timing(run, p, has_aec, has_noise),
                output: output(&run.out[samples.clone()]),
                vs_today: today.map(|t| diff(&run.out[samples.clone()], &t[samples])),
            }
        })
        .collect()
}

fn us(ns: u64) -> f64 {
    ns as f64 / 1e3
}

fn timing(run: &Run, p: &Phase, has_aec: bool, has_noise: bool) -> Timing {
    let timed = p.start.max(WARM_UP).min(p.end)..p.end;
    let row = |ns: &[u64]| Stats::of(ns[timed.clone()].iter().map(|&v| us(v)).collect());
    let post: Vec<u64> = run.noise_ns.iter().zip(&run.model_ns).map(|(&n, &m)| n.saturating_sub(m)).collect();
    Timing {
        timed: timed.len(),
        chain: row(&run.chain_ns),
        aec3: if has_aec { row(&run.aec_ns) } else { None },
        noise: if has_noise { row(&run.noise_ns) } else { None },
        model: if has_noise { row(&run.model_ns) } else { None },
        post: if has_noise { row(&post) } else { None },
        chain_mean_per_s: run.chain_ns[p.start..p.end]
            .chunks(BLOCKS_PER_S)
            .map(|s| s.iter().map(|&v| us(v)).sum::<f64>() / s.len() as f64)
            .collect(),
    }
}

/// The exact zeros, subnormals and peak of `y`, a phase's output, and when it goes silent.
pub fn output(y: &[f32]) -> Output {
    if y.is_empty() {
        return Output::default();
    }
    let n = y.len() as f64;
    let zeros = y.iter().filter(|&&v| v == 0.0).count();
    let subnormals = y.iter().filter(|v| v.is_subnormal()).count();
    let silent: Vec<bool> = y.chunks(BLOCK).map(|b| b.iter().all(|&v| v == 0.0)).collect();
    let first = silent.iter().position(|&s| s);
    let last_live = silent.iter().rposition(|&s| !s);
    let all_from = match last_live {
        None => Some(0),
        Some(i) if i + 1 < silent.len() => Some(i + 1),
        Some(_) => None,
    };
    let peak = |x: &[f32]| dbfs(f64::from(x.iter().fold(0.0f32, |m, v| m.max(v.abs()))));
    Output {
        zero_share: zeros as f64 / n,
        subnormal_share: subnormals as f64 / n,
        first_zero_ms: first.map(|b| b as f64 * MS_PER_BLOCK),
        all_zero_ms: all_from.map(|b| b as f64 * MS_PER_BLOCK),
        peak_dbfs: peak(y),
        late_peak_dbfs: peak(&y[silent.len() / 2 * BLOCK..]),
    }
}

/// `x` (an amplitude, ≥ 0) in dBFS; `None` for 0.
fn dbfs(x: f64) -> Option<f64> {
    (x > 0.0).then(|| 20.0 * x.log10())
}

/// How `y` differs from `today`, sample by sample (the two are the same length).
pub fn diff(y: &[f32], today: &[f32]) -> Diff {
    let d = |(&a, &b): (&f32, &f32)| (f64::from(a) - f64::from(b)).abs();
    let max = |a: &[f32], b: &[f32]| a.iter().zip(b).map(d).fold(0.0f64, f64::max);
    let differ = y.iter().zip(today).filter(|(a, b)| a != b).count();
    let squares: f64 = y.iter().zip(today).map(|p| d(p).powi(2)).sum();
    let second = SAMPLE_RATE as usize;
    Diff {
        max_dbfs: dbfs(max(y, today)),
        rms_dbfs: if y.is_empty() { None } else { dbfs((squares / y.len() as f64).sqrt()) },
        differ_share: if y.is_empty() { 0.0 } else { differ as f64 / y.len() as f64 },
        max_dbfs_per_s: y.chunks(second).zip(today.chunks(second)).map(|(a, b)| dbfs(max(a, b))).collect(),
    }
}
