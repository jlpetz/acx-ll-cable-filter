//! Shared helpers for the offline harness tests: run a stage or chain block by block, build
//! controls, compose test signals, and dump WAVs for listening.
//!
//! Each test binary uses a subset, hence the `dead_code` allowance.
#![allow(dead_code)]

use std::ops::Range;
use std::sync::Arc;

use nf_core::chain::Chain;
use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_core::stage::{ProcessIo, Stage, StageReport, StageStats};
use nf_core::{BLOCK, Block, SAMPLE_RATE};
use nf_stages::synth::{self, Voice};
use nf_stages::{Aec3Stage, RnnoiseStage};

pub const SR: usize = SAMPLE_RATE as usize;

pub fn controls(cfg: ProcessingConfig) -> Arc<ChainControls> {
    Arc::new(ChainControls::new(&cfg))
}

pub fn rnnoise(cfg: ProcessingConfig) -> RnnoiseStage {
    let mut s = RnnoiseStage::new(controls(cfg));
    s.activate(SAMPLE_RATE, BLOCK).expect("activate RNNoise");
    s
}

pub fn aec3() -> Aec3Stage {
    let mut s = Aec3Stage::new(controls(ProcessingConfig::default())).expect("build AEC3");
    s.activate(SAMPLE_RATE, BLOCK).expect("activate AEC3");
    s
}

pub fn block(x: &[f32]) -> &Block {
    x.try_into().expect("a whole block")
}

/// A stage's output, its per-block reports, and its stats after each block.
pub struct Run {
    pub out: Vec<f32>,
    pub reports: Vec<StageReport>,
    pub stats: Vec<StageStats>,
}

/// Runs `stage` over `input` (and `reference`, block-aligned) as the chain would.
pub fn run_stage(stage: &mut dyn Stage, input: &[f32], reference: Option<&[f32]>) -> Run {
    assert_eq!(input.len() % BLOCK, 0, "whole blocks only");
    let blocks = input.len() / BLOCK;
    let mut run = Run { out: Vec::with_capacity(input.len()), reports: Vec::with_capacity(blocks), stats: Vec::new() };
    let mut y = [0.0f32; BLOCK];
    for (b, x) in input.as_chunks::<BLOCK>().0.iter().enumerate() {
        let r = reference.map(|r| block(&r[b * BLOCK..(b + 1) * BLOCK]));
        run.reports.push(stage.process(ProcessIo { input: x, reference: r, output: &mut y }));
        run.stats.push(stage.stats());
        run.out.extend_from_slice(&y);
    }
    run
}

/// Runs the chain over `mic` and `reference`.
pub fn run_chain(chain: &mut Chain, mic: &[f32], reference: &[f32]) -> Vec<f32> {
    assert_eq!(mic.len() % BLOCK, 0, "whole blocks only");
    let mut out = Vec::with_capacity(mic.len());
    let mut y = [0.0f32; BLOCK];
    for (m, r) in mic.as_chunks::<BLOCK>().0.iter().zip(reference.as_chunks::<BLOCK>().0) {
        chain.process(m, r, &mut y);
        out.extend_from_slice(&y);
    }
    out
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

/// The index of the largest |x|.
pub fn peak_index(x: &[f32]) -> usize {
    x.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map_or(0, |(i, _)| i)
}
