//! Offline per-block timing of the stages and the chain: what `noise-filter bench` prints and
//! `tests/timing.rs` runs, so both measure the same code on the same signals.
//!
//! Each block is timed alone (a stage block is tens of µs, well above QPC's 0.1 µs tick) after
//! [`WARM_UP`] blocks. Not real-time code: signals and result buffers are allocated before timing
//! starts, and the timed closures only process. An optional counter (the app passes
//! `nf_mem::thread_scope_allocs`) is read after the warm-up and at the end, so a row also says how
//! many allocations the processing made once warm.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_core::stage::{ProcessIo, Stage, StageError};
use nf_core::{BLOCK, Block, SAMPLE_RATE, db_to_lin};

use crate::synth::{Voice, add, convolve, delayed, pink_noise, room_impulse_response, scaled, speech_like};
use crate::{Aec3Stage, RnnoiseStage, build_chain};

/// Blocks run before timing starts (1 s): first-touch pages, FFT set-up, AEC3's start-up.
pub const WARM_UP: usize = 100;

/// Reads the calling thread's allocation count, e.g. `nf_mem::thread_scope_allocs`.
pub type AllocCounter = fn() -> u64;

const SILENT: Block = [0.0; BLOCK];

/// The signals a run times, all the same length in whole blocks.
pub struct Signals {
    /// Near-end speech plus noise: RNNoise's input.
    pub noisy: Vec<f32>,
    /// `noisy` plus a room echo of `far`: AEC3's and the chain's mic.
    pub mic: Vec<f32>,
    /// The far end (the speaker reference).
    pub far: Vec<f32>,
}

impl Signals {
    /// `seconds` of the scene tests/timing.rs has always timed: a high voice at -26 dBFS and pink
    /// noise at -45 dBFS at the near end, plus a low voice's echo (RT60 0.15 s, 40 ms, -6 dB).
    pub fn synthetic(seconds: f32) -> Self {
        let len = whole_blocks(seconds);
        let near = speech_like(len, Voice::HIGH.at(-26.0), 3);
        let noise = pink_noise(len, db_to_lin(-45.0), 4);
        Self::with_echo(add(&near.samples, &noise), len)
    }

    /// `near` (a recording) as the near end, looped or cut to `seconds`, with the synthetic far
    /// end and its echo added for AEC3. An empty `near` is silence.
    pub fn with_near(near: &[f32], seconds: f32) -> Self {
        let len = whole_blocks(seconds);
        let noisy = if near.is_empty() { vec![0.0; len] } else { near.iter().copied().cycle().take(len).collect() };
        Self::with_echo(noisy, len)
    }

    fn with_echo(noisy: Vec<f32>, len: usize) -> Self {
        let far = speech_like(len, Voice::LOW, 1).samples;
        let rir = room_impulse_response(9600, 0.15, 2);
        let echo = convolve(&far, &rir);
        let echo = scaled(&delayed(echo.get(..len).unwrap_or(&echo), 1920), 0.5);
        let mic = add(&echo, &noisy);
        Self { noisy, mic, far }
    }

    /// Whole blocks in each signal.
    pub fn blocks(&self) -> usize {
        self.noisy.len().min(self.mic.len()).min(self.far.len()) / BLOCK
    }
}

/// Samples in `seconds`, rounded down to whole blocks (at least one).
fn whole_blocks(seconds: f32) -> usize {
    let samples = (f64::from(seconds.max(0.0)) * f64::from(SAMPLE_RATE)) as usize;
    (samples / BLOCK).max(1) * BLOCK
}

/// Block `b` of `x`, or silence past its end.
#[inline]
fn block(x: &[f32], b: usize) -> &Block {
    x.as_chunks::<BLOCK>().0.get(b).unwrap_or(&SILENT)
}

/// One row: the timed blocks' costs in µs, sorted.
#[derive(Clone, Debug)]
pub struct BlockTimes {
    pub name: &'static str,
    us: Vec<f64>,
    /// Allocations the processing made after the warm-up (`None` without a counter).
    pub allocations: Option<u64>,
}

impl BlockTimes {
    /// Timed blocks.
    pub fn blocks(&self) -> usize {
        self.us.len()
    }

    /// The nearest-rank percentile `p` (0..=100) in µs; NaN with no timed blocks.
    pub fn percentile(&self, p: f64) -> f64 {
        let n = self.us.len();
        if n == 0 {
            return f64::NAN;
        }
        let i = ((n as f64 * p / 100.0) as usize).min(n - 1);
        self.us.get(i).copied().unwrap_or(f64::NAN)
    }

    pub fn p50(&self) -> f64 {
        self.percentile(50.0)
    }

    pub fn p99(&self) -> f64 {
        self.percentile(99.0)
    }

    pub fn max(&self) -> f64 {
        self.us.last().copied().unwrap_or(f64::NAN)
    }

    pub fn mean(&self) -> f64 {
        if self.us.is_empty() { f64::NAN } else { self.us.iter().sum::<f64>() / self.us.len() as f64 }
    }

    /// The row as the timing test and the app print it.
    pub fn line(&self) -> String {
        let mut line = format!(
            "{:<40} p50 {:7.1} µs   p99 {:7.1}   max {:7.1}   (mean {:.1}, {} blocks)",
            self.name,
            self.p50(),
            self.p99(),
            self.max(),
            self.mean(),
            self.blocks()
        );
        if let Some(a) = self.allocations {
            let per_block = if self.us.is_empty() { 0.0 } else { a as f64 / self.us.len() as f64 };
            line.push_str(&format!("   allocs {a} ({per_block:.1}/block)"));
        }
        line
    }
}

/// Times `f(block)` for blocks `WARM_UP..blocks`, each alone, after running the first `WARM_UP`
/// untimed. `count`, if given, is read after the warm-up and at the end.
pub fn time_blocks(name: &'static str, blocks: usize, count: Option<AllocCounter>, mut f: impl FnMut(usize)) -> BlockTimes {
    let warm = WARM_UP.min(blocks);
    for b in 0..warm {
        f(b);
    }
    let mut us = Vec::with_capacity(blocks - warm);
    let before = count.map(|c| c());
    for b in warm..blocks {
        let t = Instant::now();
        f(b);
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let allocations = count.zip(before).map(|(c, before)| c().wrapping_sub(before));
    us.sort_by(f64::total_cmp);
    BlockTimes { name, us, allocations }
}

fn controls(cfg: ProcessingConfig) -> Arc<ChainControls> {
    Arc::new(ChainControls::new(&cfg))
}

/// RNNoise with the default floor and gate, on the noisy near end.
pub fn rnnoise(sig: &Signals, count: Option<AllocCounter>) -> Result<BlockTimes, StageError> {
    let mut s = RnnoiseStage::new(controls(ProcessingConfig::default()));
    s.activate(SAMPLE_RATE, BLOCK)?;
    let mut out = [0.0f32; BLOCK];
    Ok(time_blocks("RNNoise (noisy speech)", sig.blocks(), count, |b| {
        black_box(s.process(ProcessIo { input: block(&sig.noisy, b), reference: None, output: &mut out }));
        black_box(&out);
    }))
}

/// AEC3 on the mic (echo, near end and noise) with the far end as its reference.
pub fn aec3(sig: &Signals, count: Option<AllocCounter>) -> Result<BlockTimes, StageError> {
    let mut s = Aec3Stage::new(controls(ProcessingConfig::default()))?;
    s.activate(SAMPLE_RATE, BLOCK)?;
    let mut out = [0.0f32; BLOCK];
    Ok(time_blocks("AEC3 (echo + near end + noise)", sig.blocks(), count, |b| {
        let reference = Some(block(&sig.far, b));
        black_box(s.process(ProcessIo { input: block(&sig.mic, b), reference, output: &mut out }));
        black_box(&out);
    }))
}

/// AEC3 with an exact-zero reference, which the stage dithers.
pub fn aec3_silent_reference(sig: &Signals, count: Option<AllocCounter>) -> Result<BlockTimes, StageError> {
    let mut s = Aec3Stage::new(controls(ProcessingConfig::default()))?;
    s.activate(SAMPLE_RATE, BLOCK)?;
    let mut out = [0.0f32; BLOCK];
    Ok(time_blocks("AEC3, silent reference (dithered)", sig.blocks(), count, |b| {
        black_box(s.process(ProcessIo { input: block(&sig.noisy, b), reference: Some(&SILENT), output: &mut out }));
        black_box(&out);
    }))
}

/// For comparison: sonora in the stage's config fed an exact-zero reference with no dither, after
/// 3 s of far-end speech so its filters hold energy that then decays (sonora #34's denormal stall,
/// if this CPU has it).
pub fn sonora_undithered(sig: &Signals, count: Option<AllocCounter>) -> BlockTimes {
    let mut apm = crate::aec3::build_apm();
    let mut scratch = [0.0f32; BLOCK];
    let mut out = [0.0f32; BLOCK];
    for b in 0..300.min(sig.blocks()) {
        let _ = apm.process_render_f32(&[&block(&sig.far, b)[..]], &mut [&mut scratch[..]]);
        let _ = apm.process_capture_f32(&[&block(&sig.mic, b)[..]], &mut [&mut out[..]]);
    }
    time_blocks("sonora, zero reference, no dither", sig.blocks(), count, |b| {
        let _ = apm.process_render_f32(&[&SILENT[..]], &mut [&mut scratch[..]]);
        let _ = apm.process_capture_f32(&[&block(&sig.noisy, b)[..]], &mut [&mut out[..]]);
        black_box(&out);
    })
}

/// The chain as the app builds it (meters on), with or without AEC3. Without it the input is the
/// noisy near end (no echo to cancel).
pub fn chain(sig: &Signals, with_aec: bool, count: Option<AllocCounter>) -> Result<BlockTimes, StageError> {
    let cfg = ProcessingConfig { aec_enabled: with_aec, ..ProcessingConfig::default() };
    let mut chain = build_chain(controls(cfg), with_aec)?;
    let mut out = [0.0f32; BLOCK];
    let (name, mic) = if with_aec { ("chain: meters + AEC3 + RNNoise", &sig.mic) } else { ("chain: meters + RNNoise", &sig.noisy) };
    Ok(time_blocks(name, sig.blocks(), count, |b| {
        black_box(chain.process(block(mic, b), block(&sig.far, b), &mut out));
        black_box(&out);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_over_the_sorted_times() {
        let t = BlockTimes { name: "t", us: (1..=100).map(f64::from).collect(), allocations: Some(5) };
        assert_eq!((t.p50(), t.p99(), t.max(), t.mean()), (51.0, 100.0, 100.0, 50.5));
        assert_eq!(t.percentile(0.0), 1.0);
        assert!(t.line().contains("allocs 5 (0.1/block)"), "{}", t.line());
        let empty = BlockTimes { name: "e", us: Vec::new(), allocations: None };
        assert!(empty.p50().is_nan() && empty.max().is_nan() && empty.mean().is_nan());
        assert!(!empty.line().contains("allocs"));
    }

    #[test]
    fn time_blocks_warms_up_then_times_the_rest_and_counts() {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn count() -> u64 {
            COUNT.load(std::sync::atomic::Ordering::Relaxed)
        }
        let mut seen = Vec::new();
        let t = time_blocks("t", WARM_UP + 7, Some(count), |b| {
            seen.push(b);
            if b >= WARM_UP {
                COUNT.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
            }
        });
        assert_eq!(seen, (0..WARM_UP + 7).collect::<Vec<_>>());
        assert_eq!((t.blocks(), t.allocations), (7, Some(14)));
        // Fewer blocks than the warm-up: nothing timed, nothing panics.
        assert_eq!(time_blocks("short", 3, None, |_| {}).blocks(), 0);
    }

    #[test]
    fn signals_are_whole_blocks_of_equal_length() {
        let s = Signals::with_near(&[0.5; 1000], 0.1);
        assert_eq!((s.noisy.len(), s.mic.len(), s.far.len()), (4800, 4800, 4800));
        assert_eq!(s.blocks(), 10);
        // The near end loops.
        assert!(s.noisy.iter().all(|&v| v == 0.5));
        assert_eq!(Signals::with_near(&[], 0.0).blocks(), 1);
        assert_eq!(block(&s.noisy, 99), &SILENT);
    }
}
