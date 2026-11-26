//! Extensions: CLAP's `get_extension`, as typed getters on [`super::Stage`] that return `Option<&dyn Ext>` (`None` = not
//! supported). Each value is read on the audio thread after `process` and describes the block just output; reading one
//! allocates nothing and takes no lock.

/// `clap.gain-adjustment-metering/0` (a CLAP draft: the id may change before it's final).
pub const GAIN_ADJUSTMENT_METERING_ID: &str = "clap.gain-adjustment-metering/0";
/// Ours: the noise model's speech probability.
pub const SPEECH_PROBABILITY_ID: &str = crate::namespaced!(".", "speech-probability/0");
/// Ours: an echo canceller's estimates.
pub const AEC_TELEMETRY_ID: &str = crate::namespaced!(".", "aec-telemetry/0");

/// `clap.gain-adjustment-metering/0`: the gain the stage applied, in dB (<= 0).
///
/// Deviations from the draft, which the `.clap` export adapts:
/// - its `get()` is the current (last-sample) adjustment; ours is the block's output level against its latency-aligned
///   input's, so every part of the stage counts (the noise stage: the model, the floor mix and the gate);
/// - ours is NaN before the first block, after a reset, or when the aligned input is too quiet for a measurable ratio
///   (the export maps NaN to 0).
pub trait GainAdjustmentMetering {
    fn gain_adjustment_db(&self) -> f32;
}

/// Ours, `noise-filter.speech-probability/0`: the model's speech probability (0..1) for the window that produced the block
/// just output; NaN when unknown (nothing processed yet, or a reset since). A voice changer can gate on it.
pub trait SpeechProbability {
    fn speech_probability(&self) -> f32;
}

/// Ours, `noise-filter.aec-telemetry/0`: an echo canceller's estimates. Read once a second by the engine, and per block
/// while the chain recorder is armed.
pub trait AecTelemetry {
    fn aec_stats(&self) -> AecStats;
}

/// An echo canceller's estimates (NaN = unknown).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AecStats {
    /// The estimated echo-path delay in ms.
    pub echo_delay_ms: f32,
    /// Echo return loss enhancement in dB.
    pub erle_db: f32,
}

impl Default for AecStats {
    fn default() -> Self {
        Self { echo_delay_ms: f32::NAN, erle_db: f32::NAN }
    }
}
