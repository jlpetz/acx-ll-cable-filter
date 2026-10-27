//! The stage interface: one processing step in the chain (echo cancel, noise filter, later effects).
//!
//! Shaped like a CLAP plugin (TODO #17) so stages can later be exported as `.clap` or hosted
//! side by side with third-party CLAP plugins, but called in-process through this trait with no FFI.
//! - `activate` is CLAP's `activate(sample_rate, min/max frames)`: it may allocate.
//! - `process` is CLAP's `process`: real-time, one [`BLOCK`]-frame block, no allocation, locks,
//!   system calls or logging.
//! - `latency` is CLAP's latency extension: the samples by which the output lags the input in the
//!   stream (a stage's stream offset, not its algorithmic latency, which adds one hop).
//! - A stage that needs the speaker reference declares a second input port (`needs_reference`),
//!   as a CLAP plugin would with a non-main input port.

use crate::{BLOCK, Block};

/// Where a stage sits in the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageRole {
    /// Echo cancellation: main input = mic, second input = speaker reference.
    EchoCancel,
    /// Noise suppression (a model plus its floor and gate).
    Noise,
    /// Post effects (a voice changer, EQ); none in v0.
    Effect,
}

/// What a stage is. Constant for its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageInfo {
    /// Stable machine id, e.g. `"rnnoise"`, `"aec3"`.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    pub role: StageRole,
    /// Takes the speaker reference on a second input port.
    pub needs_reference: bool,
    /// Too heavy to run on the audio thread (reserved for future ONNX models; v0 stages say false).
    pub needs_worker: bool,
}

/// One block's buffers. The input and reference are the stage's inputs; `output` is where it writes.
pub struct ProcessIo<'a> {
    pub input: &'a Block,
    /// The speaker reference, already on the mic's clock. `Some` only for stages with `needs_reference`.
    pub reference: Option<&'a Block>,
    pub output: &'a mut Block,
}

/// What a stage reports about the block it just processed. All fields are optional (`NaN` = none).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageReport {
    /// Speech probability 0..1 (RNNoise's VAD). Published for the gate and for later stages.
    pub speech_probability: f32,
    /// Attenuation the stage applied this block, in dB (<= 0): its output's level against its
    /// (latency-aligned) input's, so every part of the stage counts (for the noise stage: the model,
    /// the floor mix and the gate).
    pub gain_reduction_db: f32,
}

impl Default for StageReport {
    fn default() -> Self {
        Self { speech_probability: f32::NAN, gain_reduction_db: f32::NAN }
    }
}

/// Slower-changing statistics a stage can expose (read by the engine about once a second).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageStats {
    /// AEC: the estimated echo-path delay in ms.
    pub echo_delay_ms: f32,
    /// AEC: echo return loss enhancement in dB.
    pub erle_db: f32,
}

impl Default for StageStats {
    fn default() -> Self {
        Self { echo_delay_ms: f32::NAN, erle_db: f32::NAN }
    }
}

/// A stage failed to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageError(pub String);

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StageError {}

/// One processing step. `Send` so it can be built on one thread and run on the audio thread.
pub trait Stage: Send {
    fn info(&self) -> StageInfo;

    /// Prepares for processing at `sample_rate` with `block`-frame blocks. May allocate. Called
    /// before the first `process` and again after any configuration that needs it.
    fn activate(&mut self, sample_rate: u32, block: usize) -> Result<(), StageError>;

    /// Stream offset in samples: `output[n]` corresponds to `input[n - latency()]`.
    fn latency(&self) -> u32;

    /// Processes one block. Real-time: no allocation, locks, system calls or logging.
    fn process(&mut self, io: ProcessIo<'_>) -> StageReport;

    /// Clears internal state (called when the stage is switched back on).
    fn reset(&mut self);

    /// Slow statistics; the default has none.
    fn stats(&self) -> StageStats {
        StageStats::default()
    }
}

/// Compile-time check that the block size is what the stages assume.
const _: () = assert!(BLOCK == 480);
