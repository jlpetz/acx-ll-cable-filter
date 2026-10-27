//! The user's settings, persisted by the GUI as JSON and applied to the engine.
//!
//! Missing fields take their defaults (`#[serde(default)]`), so old settings files keep loading.

use serde::{Deserialize, Serialize};

/// Everything the engine needs to run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub devices: DeviceSelection,
    pub processing: ProcessingConfig,
    pub io: IoConfig,
}

/// Endpoint ids (WASAPI device ids). `None` = the system default for that role.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSelection {
    /// Capture endpoint: the physical mic (or a cable's output, for the virtual latency loop).
    pub mic: Option<String>,
    /// Render endpoint the cleaned signal goes to: a virtual cable's input (e.g. "CABLE Input").
    /// `None` means not chosen; the engine won't start without one (it never picks a default here).
    pub output: Option<String>,
    /// Render endpoint whose loopback is the echo reference (the speakers). `None` = default render.
    pub reference: Option<String>,
}

/// What the chain does; all of it is applied live through `controls::ChainControls`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessingConfig {
    /// Echo cancellation ("using speakers"). Off by default: headphones need none (TODO #10).
    pub aec_enabled: bool,
    /// The noise filter (RNNoise in v0).
    pub noise_enabled: bool,
    /// Maximum attenuation the noise stage may apply, in dB (negative). -60 is effectively unlimited.
    pub floor_db: f32,
    pub gate: GateConfig,
    /// Compute and publish the input and output meters.
    pub meters_enabled: bool,
    /// Compute and publish the per-block latency readout.
    pub latency_readout_enabled: bool,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            aec_enabled: false,
            noise_enabled: true,
            floor_db: -60.0,
            gate: GateConfig::default(),
            meters_enabled: true,
            latency_readout_enabled: true,
        }
    }
}

/// How the gate decides it's open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateMode {
    /// No gate.
    #[default]
    Off,
    /// Open above `threshold_db`.
    Static,
    /// Open above the tracked noise floor plus `auto_margin_db`.
    Auto,
}

impl GateMode {
    pub fn to_u8(self) -> u8 {
        match self {
            GateMode::Off => 0,
            GateMode::Static => 1,
            GateMode::Auto => 2,
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => GateMode::Static,
            2 => GateMode::Auto,
            _ => GateMode::Off,
        }
    }
}

/// The gate that ends the noise stage (TODO #11).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    pub mode: GateMode,
    /// Static mode: the open threshold, in dBFS (RMS of the block).
    pub threshold_db: f32,
    /// Auto mode: how far above the tracked noise floor the gate opens, in dB.
    pub auto_margin_db: f32,
    /// Time to open fully, in ms.
    pub attack_ms: f32,
    /// Time to close fully after the hold, in ms.
    pub release_ms: f32,
    /// Time the gate stays open after the signal drops below the threshold, in ms.
    pub hold_ms: f32,
    /// Attenuation when closed, in dB (negative).
    pub range_db: f32,
    /// Also hold the gate open while the noise model reports speech.
    pub use_vad: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            mode: GateMode::Off,
            threshold_db: -50.0,
            auto_margin_db: 6.0,
            attack_ms: 2.0,
            release_ms: 150.0,
            hold_ms: 100.0,
            range_db: -40.0,
            // Off: RNNoise's VAD reads 0.5 or more on 87-100% of blocks of pure pink noise at -50 and
            // -40 dBFS, and on nearly every pause at 0 dB SNR, which kept the Auto gate open on
            // noise-only stretches (verify 2026-10-26).
            use_vad: false,
        }
    }
}

/// Device-side choices. Changing any of these restarts the streams.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IoConfig {
    /// Try IAudioClient3's smallest shared-mode period on the mic (falls back to the default).
    pub low_latency_periods: bool,
    /// Open the mic in RAW mode where the device supports it (no OEM effects ahead of AEC3).
    pub raw_mic: bool,
    /// Extra render-buffer fill kept on the output cable, in ms, on top of one block and one period.
    pub output_margin_ms: f32,
    /// Keep a silent render stream open on the reference endpoint so its loopback never stops.
    pub keep_alive_reference: bool,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self { low_latency_periods: true, raw_mic: true, output_margin_ms: 2.0, keep_alive_reference: true }
    }
}
