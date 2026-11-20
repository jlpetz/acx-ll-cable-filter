//! Running a cell block by block, and what is measured of it: each stage's cost per block, and the output's exact zeros (as
//! f32, and as a 24- or 16-bit cable gets them), peak, bits, near-end SI-SDR and difference from today's dither, phase by
//! phase.

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
    /// The chain's and AEC3's mean cost in each second of the phase (every block, the first second's too), in µs: whether a
    /// stall builds, and whether it ends. AEC3's is empty without AEC3.
    pub chain_mean_per_s: Vec<f64>,
    pub aec3_mean_per_s: Vec<f64>,
}

/// A bit depth the output's exact zeros are counted at: the chain's f32 as it is, or as nf-engine's `convert.rs` quantises it
/// for an exclusive-mode cable in 24-bit (I24, I24In32) or 16-bit PCM: clamped to ±1, times full scale, rounded ties to even,
/// NaN to 0. Anything under half a step is then exact zero: about -144.5 dBFS at 24 bits, -96.3 at 16. In shared mode Windows
/// converts to the endpoint's format itself, which this doesn't model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    F32,
    I24,
    I16,
}

impl Depth {
    pub const ALL: [Depth; 3] = [Depth::F32, Depth::I24, Depth::I16];

    /// The suffix of this depth's JSON keys (none for f32).
    pub fn suffix(self) -> &'static str {
        match self {
            Depth::F32 => "",
            Depth::I24 => "_i24",
            Depth::I16 => "_i16",
        }
    }

    pub fn is_zero(self, x: f32) -> bool {
        let quantised = |full: f32| {
            let q = (x.clamp(-1.0, 1.0) * full).round_ties_even();
            q == 0.0 || q.is_nan()
        };
        match self {
            Depth::F32 => x == 0.0,
            Depth::I24 => quantised(8_388_608.0),
            Depth::I16 => quantised(32_768.0),
        }
    }
}

/// A phase's output's exact zeros at one depth.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Zeros {
    /// The share of the phase's output samples that are exact zeros (either sign).
    pub share: f64,
    /// From the phase's start to its first all-zero output block, and to the block from which every block to the phase's end
    /// is all zero, in ms; `None`: never.
    pub first_ms: Option<f64>,
    pub all_ms: Option<f64>,
}

/// What the output did over a phase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Output {
    /// The exact zeros at each [`Depth`], in [`Depth::ALL`]'s order.
    pub zeros: [Zeros; 3],
    /// The share of the phase's output samples that are subnormal.
    pub subnormal_share: f64,
    /// The largest |sample| in dBFS (`None`: all zero), over the phase and over its second half (past a chain's latency and
    /// the start of its tail: what a silent phase settles at).
    pub peak_dbfs: Option<f64>,
    pub late_peak_dbfs: Option<f64>,
    /// FNV-1a (64-bit) over the samples' f32 bits, little-endian: whether two runs' outputs (guard off and on) are the same
    /// bit for bit.
    pub fnv1a: u64,
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
    /// With the mic live: the output's SI-SDR against the near end's speech, in dB ([`si_sdr`]).
    pub si_sdr_db: Option<f64>,
    pub vs_today: Option<Diff>,
}

const MS_PER_BLOCK: f64 = BLOCK as f64 * 1000.0 / SAMPLE_RATE as f64;
const BLOCKS_PER_S: usize = SAMPLE_RATE as usize / BLOCK;

/// Each phase of `signal`'s results from `run`, a chain `latency` samples late; `today`, the output of the same cell with
/// today's dither, if it ran.
pub fn phases(signal: &Signal, run: &Run, has_aec: bool, has_noise: bool, latency: u32, today: Option<&[f32]>) -> Vec<PhaseResult> {
    let lat = latency as usize;
    signal
        .phases
        .iter()
        .map(|p| {
            let samples = p.start * BLOCK..p.end * BLOCK;
            // The output from the chain's first sample of the near end's on, against that speech.
            let from = samples.start.max(lat);
            let si = (!p.mic_silent && from < samples.end)
                .then(|| si_sdr(&run.out[from..samples.end], &signal.near[from - lat..samples.end - lat]));
            PhaseResult {
                phase: p.clone(),
                timing: timing(run, p, has_aec, has_noise),
                output: output(&run.out[samples.clone()]),
                si_sdr_db: si.flatten(),
                vs_today: today.map(|t| diff(&run.out[samples.clone()], &t[samples])),
            }
        })
        .collect()
}

fn us(ns: u64) -> f64 {
    ns as f64 / 1e3
}

fn per_second(ns: &[u64]) -> Vec<f64> {
    ns.chunks(BLOCKS_PER_S).map(|s| s.iter().map(|&v| us(v)).sum::<f64>() / s.len() as f64).collect()
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
        chain_mean_per_s: per_second(&run.chain_ns[p.start..p.end]),
        aec3_mean_per_s: if has_aec { per_second(&run.aec_ns[p.start..p.end]) } else { Vec::new() },
    }
}

/// FNV-1a, 64-bit, over `y`'s f32 bits (each sample's four bytes, little-endian).
pub fn fnv1a(y: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in y.iter().flat_map(|v| v.to_bits().to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `y`'s exact zeros at `depth`, and when they start.
fn zeros(y: &[f32], depth: Depth) -> Zeros {
    let count = y.iter().filter(|&&v| depth.is_zero(v)).count();
    let silent: Vec<bool> = y.chunks(BLOCK).map(|b| b.iter().all(|&v| depth.is_zero(v))).collect();
    let first = silent.iter().position(|&s| s);
    let all_from = match silent.iter().rposition(|&s| !s) {
        None => Some(0),
        Some(i) if i + 1 < silent.len() => Some(i + 1),
        Some(_) => None,
    };
    Zeros {
        share: count as f64 / y.len() as f64,
        first_ms: first.map(|b| b as f64 * MS_PER_BLOCK),
        all_ms: all_from.map(|b| b as f64 * MS_PER_BLOCK),
    }
}

/// The exact zeros (at each depth), subnormals, peak and bits of `y`, a phase's output.
pub fn output(y: &[f32]) -> Output {
    if y.is_empty() {
        return Output { fnv1a: fnv1a(y), ..Output::default() };
    }
    let subnormals = y.iter().filter(|v| v.is_subnormal()).count();
    let peak = |x: &[f32]| dbfs(f64::from(x.iter().fold(0.0f32, |m, v| m.max(v.abs()))));
    let blocks = y.len().div_ceil(BLOCK);
    Output {
        zeros: Depth::ALL.map(|d| zeros(y, d)),
        subnormal_share: subnormals as f64 / y.len() as f64,
        peak_dbfs: peak(y),
        late_peak_dbfs: peak(&y[(blocks / 2 * BLOCK).min(y.len())..]),
        fnv1a: fnv1a(y),
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

/// The scale-invariant SDR of `y` against `s` (the same length; `s` the near end's speech, aligned): the part of `y` along
/// `s` (at the best gain) over the rest (the noise and echo left, and any distortion), in dB. `None` with `s` silent; +inf
/// would be `y` exactly a multiple of `s`, so the rest is floored at 1e-30. A comparison between one chain's policies: a
/// phase response counts as distortion, so AEC3's high-pass (its low harmonics a little late, the module notes of nf-stages'
/// `aec3.rs`) takes its chains' figure well under the others' whatever the output sounds like.
pub fn si_sdr(y: &[f32], s: &[f32]) -> Option<f64> {
    let ss: f64 = s.iter().map(|&v| f64::from(v).powi(2)).sum();
    if ss == 0.0 {
        return None;
    }
    let ys: f64 = y.iter().zip(s).map(|(&a, &b)| f64::from(a) * f64::from(b)).sum();
    let g = ys / ss;
    let (mut target, mut rest) = (0.0f64, 0.0f64);
    for (&a, &b) in y.iter().zip(s) {
        let t = g * f64::from(b);
        target += t * t;
        rest += (f64::from(a) - t).powi(2);
    }
    Some(10.0 * (target / rest.max(1e-30)).log10())
}
