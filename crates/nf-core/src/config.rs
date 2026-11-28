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
    pub effect: EffectConfig,
}

/// The optional external CLAP effect after the noise stage. Parameter IDs belong to the selected plugin.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectConfig {
    pub path: Option<std::path::PathBuf>,
    pub plugin_id: Option<String>,
    pub bypass: bool,
    pub params: std::collections::BTreeMap<u32, f64>,
    /// Worker delay in 10 ms blocks. Zero selects from mic delivery; saved chain recordings carry the actual depth.
    pub worker_blocks: u8,
}

/// Device choices. Our cables can be saved by `acx-ll-cable:<cable id>/<role>` across reinstalls. Other choices, and files
/// from older releases, use the Windows endpoint ID.
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
    /// The noise filter.
    pub noise_enabled: bool,
    /// The noise filter's model: `"rnnoise"` (built in, the default) or a model pack's id (TODO 5: `"ulunas"`, for
    /// evaluation, in a build with the `onnx` feature), which follows the pack id rule ([`is_model_id`]). Changing it
    /// rebuilds the chain: an engine restart.
    pub noise_model: String,
    /// What a band-limited model's output does above its band (a 16 kHz model's 8 kHz): applied live.
    pub high_band: HighBand,
    /// How many blocks behind the audio thread a model pack's inference worker runs (1 to [`MAX_MODEL_DEPTH`]; TODO 6,
    /// `noise::Pipelined`), or [`AUTO_MODEL_DEPTH`] (the default): the engine picks it at each start from how the mic
    /// delivers ([`ProcessingConfig::model_depth_for`]). Each block adds 10 ms of latency; a wake that processes more blocks
    /// than this back to back (a 20 ms device period, a cable's burst, a late wake) costs a block of the unfiltered mic for
    /// each one over. RNNoise ignores it. Changing it, with a pack in use, is an engine restart.
    pub model_depth: u32,
    /// Maximum attenuation the noise stage may apply, in dB (negative). -60 is effectively unlimited.
    pub floor_db: f32,
    pub gate: GateConfig,
    /// Where the independent gate receives its audio in the processing chain.
    pub gate_position: GatePosition,
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
            noise_model: DEFAULT_NOISE_MODEL.to_owned(),
            high_band: HighBand::default(),
            model_depth: AUTO_MODEL_DEPTH,
            floor_db: -60.0,
            gate: GateConfig::default(),
            gate_position: GatePosition::default(),
            meters_enabled: true,
            latency_readout_enabled: true,
        }
    }
}

/// The built-in default noise model.
pub const DEFAULT_NOISE_MODEL: &str = "rnnoise";
/// [`ProcessingConfig::model_depth`]'s default: the engine picks the depth at each start (TODO 5).
pub const AUTO_MODEL_DEPTH: u32 = 0;
/// The depths it picks: one block for a mic that delivers a block a wake, two (`noise::DEFAULT_DEPTH`) for one that bursts
/// or hasn't been measured, and offline. And the limit (`noise::MAX_DEPTH`).
pub const STEADY_MODEL_DEPTH: u32 = 1;
pub const BURSTY_MODEL_DEPTH: u32 = crate::noise::DEFAULT_DEPTH;
pub const MAX_MODEL_DEPTH: u32 = crate::noise::MAX_DEPTH;

/// How a run's mic delivers its blocks, for the automatic model depth ([`ProcessingConfig::model_depth_for`]; TODO 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicDelivery {
    /// A block a wake: as the last run on this mic, opened the same way, measured it (`telemetry::WakeBlocks::delivery`), or
    /// unmeasured, the exclusive mic at a period of a block or less (its 3.33 ms packets make a block at every third).
    OneAtATime,
    /// Wakes of two or more blocks often enough that a worker one block behind would miss (the cables' shared mic: a fifth
    /// to a third of its wakes).
    Bursts,
    /// Not measured: a mic's first run in the process opened this way (but the exclusive mic's at a period of a block or
    /// less), or offline.
    Unknown,
}

impl MicDelivery {
    /// As the status says it: "the mic delivers a block at a time", "the mic delivers in bursts", "the mic's delivery isn't
    /// measured yet".
    pub fn text(self) -> &'static str {
        match self {
            MicDelivery::OneAtATime => "the mic delivers a block at a time",
            MicDelivery::Bursts => "the mic delivers in bursts",
            MicDelivery::Unknown => "the mic's delivery isn't measured yet",
        }
    }
}

impl ProcessingConfig {
    /// The model depth a start uses: [`ProcessingConfig::model_depth`] when it's set (clamped to 1..=[`MAX_MODEL_DEPTH`]);
    /// automatic, [`STEADY_MODEL_DEPTH`] for a mic that delivers a block at a time and [`BURSTY_MODEL_DEPTH`] otherwise
    /// (bursts, or not measured: the safe choice). The engine decides at each start and never mid-stream: the depth is the
    /// noise stage's latency, so changing it moves the output by a block (a click), and a start is where the chain is built.
    pub fn model_depth_for(&self, delivery: MicDelivery) -> u32 {
        match (self.model_depth, delivery) {
            (AUTO_MODEL_DEPTH, MicDelivery::OneAtATime) => STEADY_MODEL_DEPTH,
            (AUTO_MODEL_DEPTH, _) => BURSTY_MODEL_DEPTH,
            (d, _) => d.clamp(1, MAX_MODEL_DEPTH),
        }
    }

    /// The model depth is automatic ([`AUTO_MODEL_DEPTH`]).
    pub fn model_depth_auto(&self) -> bool {
        self.model_depth == AUTO_MODEL_DEPTH
    }
}

/// A model id: a model pack's id rule (`[a-z0-9][a-z0-9-]{0,63}`, `doc/spec/model-pack-v0.md`), which `"rnnoise"` follows too.
/// A pack's folder is named by it, so it can't hold a path.
pub fn is_model_id(id: &str) -> bool {
    let b = id.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=64).contains(&b.len()) && b.first().is_some_and(ok) && b.iter().all(|c| ok(c) || *c == b'-')
}

/// What the noise stage puts above a band-limited model's band (`doc/todo/3-model-spec.md`, "Rate handling"; the A/B is
/// TODO 14's). A full-band model (RNNoise) ignores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighBand {
    /// The model's own gain over its top band (5-7.5 kHz for a 16 kHz model), applied to everything above: full band out.
    #[default]
    Model,
    /// Nothing above the band: the output is band-limited, as a resampler path's would be (without the resampler).
    Zero,
    /// The input above the band, untouched (a diagnostic).
    Pass,
}

impl HighBand {
    pub const ALL: [HighBand; 3] = [HighBand::Model, HighBand::Zero, HighBand::Pass];

    pub fn to_u8(self) -> u8 {
        match self {
            HighBand::Model => 0,
            HighBand::Zero => 1,
            HighBand::Pass => 2,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => HighBand::Zero,
            2 => HighBand::Pass,
            _ => HighBand::Model,
        }
    }

    /// Its name on the command line and in the config: `model`, `zero` or `pass`.
    pub fn name(self) -> &'static str {
        match self {
            HighBand::Model => "model",
            HighBand::Zero => "zero",
            HighBand::Pass => "pass",
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

/// The four boundaries available in the current processing chain. The historic default uses the noise model's output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum GatePosition {
    #[default]
    AfterNoise = 0,
    BeforeAec = 1,
    BeforeNoise = 2,
    AfterEffect = 3,
}

impl GatePosition {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::BeforeAec,
            2 => Self::BeforeNoise,
            3 => Self::AfterEffect,
            _ => Self::AfterNoise,
        }
    }
}

/// The level gate (TODO 11).
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

/// Where the engine sends processed audio. Older settings keep their selected WASAPI playback device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputBackend {
    #[default]
    Wasapi,
    DriverRing,
}

/// Device-side choices. Changing any of these restarts the streams.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IoConfig {
    pub output_backend: OutputBackend,
    /// Persistent driver cable identity. Earlier settings choose A, or the only installed cable.
    pub direct_cable: Option<String>,
    /// Try IAudioClient3's smallest shared-mode period on the mic (falls back to the default).
    pub low_latency_periods: bool,
    /// Open the mic in RAW mode where the device supports it (no OEM effects ahead of AEC3).
    pub raw_mic: bool,
    /// Extra render-buffer fill kept on the output cable, in ms, on top of one block and one period. The WASAPI route's
    /// only: the direct driver route keeps one reader packet in the ring where that runs clean, with headroom on top that
    /// rises after the driver's underruns and comes down after clean running (the owner, 2026-11-24; nf-engine
    /// `ring_headroom.rs`).
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
    /// Let the output's fill target follow the delivery it sees: down, slowly (after four minutes), to one period + the
    /// margin + the largest lateness measured; up at once after an underrun, which can cost up to three glitches a run
    /// that the fixed target (one period + one block + the margin) would have ridden out. On by default (the owner,
    /// 2026-11-02); off for a recording that must have none. The WASAPI route's only: the direct driver route's target is
    /// one reader packet and the ring's own headroom (nf-engine `ring_headroom.rs`), whatever this says.
    pub adaptive_output: bool,
    /// The echo-path guard (nf-engine `echo_guard.rs`): with echo cancelling on, when an echo seen comes before its reference
    /// (speakers whose output latency is under the reference's lead), hold the mic back until the reference is in time,
    /// on a trial first, and only as long as needed. Costs the hold in latency (whole loopback periods: 10-20 ms on most
    /// speakers) only then. On by default (ours, 2026-11-03).
    pub echo_guard: bool,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            output_backend: OutputBackend::Wasapi,
            direct_cable: None,
            low_latency_periods: true,
            raw_mic: true,
            output_margin_ms: 2.0,
            keep_alive_reference: true,
            output_exclusive: true,
            mic_exclusive: true,
            adaptive_output: true,
            echo_guard: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The automatic depth (TODO 5): one block for a mic that delivers a block at a time, two for one that bursts or hasn't
    /// been measured; a depth that's set is used whatever the delivery, within its limits.
    #[test]
    fn the_automatic_depth_follows_the_mic_s_delivery() {
        let auto = ProcessingConfig::default();
        assert!(auto.model_depth_auto());
        let picks = [MicDelivery::OneAtATime, MicDelivery::Bursts, MicDelivery::Unknown].map(|d| auto.model_depth_for(d));
        assert_eq!(picks, [1, 2, 2]);
        for (set, used) in [(1, 1), (2, 2), (3, 3), (4, 4), (9, MAX_MODEL_DEPTH)] {
            let p = ProcessingConfig { model_depth: set, ..ProcessingConfig::default() };
            assert!(!p.model_depth_auto());
            for d in [MicDelivery::OneAtATime, MicDelivery::Bursts, MicDelivery::Unknown] {
                assert_eq!(p.model_depth_for(d), used, "{set} with {d:?}");
            }
        }
        let saved: ProcessingConfig = serde_json::from_str(r#"{"model_depth": 1}"#).unwrap();
        assert_eq!(saved.model_depth_for(MicDelivery::Bursts), 1, "a depth a settings file sets is kept");
    }

    /// The owner's decision (2026-11-02): the lowest-latency I/O by default, exclusive output and mic and the adaptive
    /// output target; the rest as before.
    #[test]
    fn the_lowest_latency_io_is_the_default() {
        let io = IoConfig::default();
        assert!(io.output_exclusive && io.mic_exclusive && io.adaptive_output && io.echo_guard);
        assert!(io.low_latency_periods && io.raw_mic && io.keep_alive_reference);
        assert_eq!(io.output_margin_ms, 2.0);
        assert_eq!(EngineConfig::default().io, io);
    }

    #[test]
    fn rnnoise_is_the_default_model_and_ids_follow_the_pack_rule() {
        let p = ProcessingConfig::default();
        assert_eq!((p.noise_model.as_str(), p.high_band, p.model_depth), ("rnnoise", HighBand::Model, AUTO_MODEL_DEPTH));
        let old: ProcessingConfig = serde_json::from_str(r#"{"noise_model": "ulunas"}"#).unwrap();
        assert_eq!(old.model_depth, AUTO_MODEL_DEPTH, "an older settings file gets the automatic depth");
        for good in ["rnnoise", "ulunas", "a", "fast-enhancer-48k", &"x".repeat(64)] {
            assert!(is_model_id(good), "{good}");
        }
        for bad in ["", "-x", "UL", "a b", "a/b", "..", "a.b", &"x".repeat(65), "\u{fc}"] {
            assert!(!is_model_id(bad), "{bad}");
        }
        for h in HighBand::ALL {
            assert_eq!(HighBand::from_u8(h.to_u8()), h);
        }
        assert_eq!(HighBand::ALL.map(HighBand::name), ["model", "zero", "pass"]);
    }
}
