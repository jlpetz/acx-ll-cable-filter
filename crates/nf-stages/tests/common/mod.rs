//! Shared helpers for the offline harness tests: the stages as the tests build them, test-signal composition, and WAV dumps
//! for listening. Running stages and chains is `nf_stages::harness`'s.
//!
//! Each test binary uses a subset, hence the `dead_code` allowance.
#![allow(dead_code)]

use std::ops::Range;
use std::sync::Arc;

use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_core::noise::NoiseStage;
use nf_core::stage::{Activation, Stage};
use nf_core::{BLOCK, Block, SAMPLE_RATE};
#[allow(unused_imports)] // each test binary uses some of them
pub use nf_stages::harness::{Run, peak_index, run_chain as run_chain_from, run_stage};
use nf_stages::synth::{self, Voice};
use nf_stages::{Aec3Stage, rnnoise_stage};

pub const SR: usize = SAMPLE_RATE as usize;

pub fn controls(cfg: ProcessingConfig) -> Arc<ChainControls> {
    Arc::new(ChainControls::new(&cfg))
}

/// The noise stage around RNNoise, activated.
pub fn rnnoise(cfg: ProcessingConfig) -> NoiseStage {
    let mut s = rnnoise_stage(controls(cfg)).expect("build the RNNoise stage");
    s.activate(&Activation::HOST).expect("activate RNNoise");
    s
}

pub fn aec3() -> Aec3Stage {
    let mut s = Aec3Stage::new(controls(ProcessingConfig::default())).expect("build AEC3");
    s.activate(&Activation::HOST).expect("activate AEC3");
    s
}

pub fn block(x: &[f32]) -> &Block {
    x.try_into().expect("a whole block")
}

/// Runs the chain over `mic` and `reference`, from its first output sample.
pub fn run_chain(chain: &mut nf_core::chain::Chain, mic: &[f32], reference: &[f32]) -> Vec<f32> {
    run_chain_from(chain, mic, reference, 0)
}

/// `ranges` moved later by `by` samples (to index a delayed output with input-time ranges).
pub fn shifted(ranges: &[Range<usize>], by: usize) -> Vec<Range<usize>> {
    ranges.iter().map(|r| r.start + by..r.end + by).collect()
}

/// Ranges that start at or after `from` and end at or before `to`.
pub fn within(ranges: &[Range<usize>], from: usize, to: usize) -> Vec<Range<usize>> {
    ranges.iter().filter(|r| r.start >= from && r.end <= to).cloned().collect()
}

/// Talk spurts with long silent gaps: `spurts` × (`talk_s` of [`synth::speech_like`], then
/// `gap_s` of silence after its last syllable). The gaps give a gate time to close.
pub struct Talk {
    pub samples: Vec<f32>,
    pub syllables: Vec<Range<usize>>,
    /// From each spurt's last syllable to the next spurt.
    pub gaps: Vec<Range<usize>>,
}

pub fn talk_with_gaps(spurts: usize, talk_s: f32, gap_s: f32, voice: Voice, seed: u64) -> Talk {
    let mut t = Talk { samples: Vec::new(), syllables: Vec::new(), gaps: Vec::new() };
    for k in 0..spurts {
        let s = synth::speech_like((talk_s * SR as f32) as usize, voice, seed + k as u64);
        let off = t.samples.len();
        t.syllables.extend(shifted(&s.syllables, off));
        let last = s.syllables.last().map_or(off, |r| r.end + off);
        t.samples.extend_from_slice(&s.samples[..last - off]);
        let gap = (gap_s * SR as f32) as usize;
        t.samples.resize(last + gap, 0.0);
        t.gaps.push(last..last + gap);
    }
    let whole = t.samples.len().div_ceil(BLOCK) * BLOCK;
    t.samples.resize(whole, 0.0);
    t
}

/// Pads to whole blocks with zeros.
pub fn whole_blocks(mut x: Vec<f32>) -> Vec<f32> {
    x.resize(x.len().div_ceil(BLOCK) * BLOCK, 0.0);
    x
}

/// Writes `x` to `$NF_STAGES_WAV_DIR/<name>.wav` when that variable is set, to listen to a test.
pub fn dump(name: &str, x: &[f32]) {
    if let Some(dir) = std::env::var_os("NF_STAGES_WAV_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.wav"));
        if let Err(e) = synth::write_wav(&path, x) {
            eprintln!("couldn't write {}: {e}", path.display());
        }
    }
}

/// A stage as a trait object, for the harness's `make` closures.
pub fn boxed(s: impl Stage + 'static) -> Box<dyn Stage> {
    Box::new(s)
}
