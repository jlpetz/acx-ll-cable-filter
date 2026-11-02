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
    /// Open the output in WASAPI exclusive mode, event-driven at the device's smallest period (2 ms or more), falling back
    /// to shared mode with a note when the device refuses. It locks the endpoint: no other app can play into it, which
    /// suits the output cable (only the filter plays into it; the apps read its other side, which stays shared). On by
    /// default (the owner, 2026-11-02: the lowest latency by default; on VB-CABLE it took 30 ms off the readout).
    pub output_exclusive: bool,
    /// Open the mic in WASAPI exclusive mode, event-driven on its own small period, falling back to shared mode with a
    /// note. While the filter runs no other app can record from that mic directly: they use the filtered mic (the cable).
    /// On by default (the owner, 2026-11-02); off for someone who records the raw mic in another app as well.
    pub mic_exclusive: bool,
    /// Let the output's fill target follow the delivery it sees: down, slowly (after two minutes), to one period + the
    /// margin + the largest lateness measured; up at once after an underrun, which can cost up to three glitches a run
    /// that the fixed target (one period + one block + the margin) would have ridden out. On by default (the owner,
    /// 2026-11-02); off for a recording that must have none.
    pub adaptive_output: bool,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            low_latency_periods: true,
            raw_mic: true,
            output_margin_ms: 2.0,
            keep_alive_reference: true,
            output_exclusive: true,
            mic_exclusive: true,
            adaptive_output: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's decision (2026-11-02): the lowest-latency I/O by default, exclusive output and mic and the adaptive
    /// output target; the rest as before.
    #[test]
    fn the_lowest_latency_io_is_the_default() {
        let io = IoConfig::default();
        assert!(io.output_exclusive && io.mic_exclusive && io.adaptive_output);
        assert!(io.low_latency_periods && io.raw_mic && io.keep_alive_reference);
        assert_eq!(io.output_margin_ms, 2.0);
        assert_eq!(EngineConfig::default().io, io);
    }
}
