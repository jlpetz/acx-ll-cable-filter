//! The stage interface (TODO 17): one processing step in the chain (echo cancel, noise filter, later effects), shaped like a
//! CLAP plugin and called in-process through this trait, with no FFI (the owner, 2026-11-02: every stage CLAP-shaped).
//!
//! CLAP's model, and ours:
//! - **Descriptor** ([`StageDescriptor`]): `clap_plugin_descriptor`. Constant; any thread. Its feature tags give the
//!   stage's role ([`StageDescriptor::role`]). Its text, the ports' and the params' are [`Text`]: a literal for a built-in
//!   stage (so their tables stay `const`), or a string known only at run time (a model pack's manifest, later a
//!   third-party plugin's descriptor in the fx-host).
//! - **Audio ports** ([`ports`]): `clap.audio-ports`. Input 0 and output 0 are the main ports; the echo canceller takes
//!   the speaker reference on a non-main input, CLAP's sidechain convention. Constant (CLAP lets ports change only while
//!   inactive, and ours never do).
//! - **`activate`**: CLAP's `activate(sample_rate, min_frames, max_frames)`; it may allocate. Host-profile deviation: our
//!   host calls it on the thread that will process (the engine's audio thread, inside its real-time scope), and may call
//!   it again; calling it again implies a deactivate. Our stages refuse any format but [`Activation::HOST`].
//! - **`process`**: CLAP's `process`. Real-time: no allocation, locks, system calls or logging, O(1) per block; always
//!   [`BLOCK`] frames, mono. One system call is allowed: a noise stage whose model runs on an inference worker wakes it
//!   (`Thread::unpark`, which doesn't block; [`crate::noise::Pipelined`]).
//! - **`reset`**: CLAP's `reset` (audio thread). No locks or system calls, and no allocation but one known exception: a noise
//!   model whose fresh state allocates swaps in a spare built off the audio thread, and rebuilds in place only when every
//!   spare is spent (RNNoise: a third real reset within 100 ms of two others; [`crate::noise::NoiseModel::reset`]). A reset
//!   with nothing processed since the last one is a no-op. Deviation: CLAP's clears all processing state, but AEC3's
//!   flushes the audio it holds and keeps its converged echo path and delay estimate.
//! - **`latency`**: `clap.latency`. The stream offset, `output[n] ~ input[n - latency]` (not the algorithmic latency, which
//!   adds one hop of collecting). Valid after `activate`, fixed until the next; the chain caches it at activate.
//! - **Params** ([`params`]): `clap.params`. Whoever holds the stage sets values through [`Stage::set_param`], the
//!   in-process stand-in for `CLAP_EVENT_PARAM_VALUE`: while the stage is active, on the processing thread between blocks
//!   (where CLAP's in-events sit, never during `process`), taking effect at the next block. It takes `&mut self`, so that
//!   holds by construction. No other thread reaches a stage: our noise stage's params alias the shared
//!   [`crate::controls::ChainControls`], which the GUI writes (see [`params`]).
//! - **State** ([`state`]): `clap.state` save and load, by whoever holds the stage, never during `process`. Not real-time
//!   (they allocate), so not on the audio thread while it runs; nothing calls them live in v0.
//! - **Extensions** ([`ext`]): typed getters, CLAP's `get_extension`: `None` means the stage doesn't support it. Read on
//!   the audio thread after `process`.
//!
//! `doc/spec/host-profile-v0.md` is the published side of these rules.
//!
//! Not modelled (the `.clap` export maps each to a no-op or a constant): `start_processing`, `stop_processing`,
//! `deactivate`, the process status, `steady_time` and events. `Drop` is CLAP's `destroy`. Bypass is the host's (the
//! chain's slots, with their crossfades), so no stage declares an `IS_BYPASS` param.

use std::borrow::Cow;

use crate::{BLOCK, Block, SAMPLE_RATE};

/// Prefixes `$name` with our namespace and `$sep`: `"."` for ids (`noise-filter.denoise`), `":"` for feature tags
/// (`noise-filter:denoise`, CLAP's `$namespace:$feature` rule). The namespace's one literal is here; [`NAMESPACE`] is made
/// from it. Provisional: it becomes reverse-DNS before any `.clap` ships. Nothing persists these ids yet.
#[macro_export]
macro_rules! namespaced {
    ($sep:literal, $name:literal) => {
        concat!("noise-filter", $sep, $name)
    };
}

pub mod ext;
pub mod params;
pub mod ports;
pub mod state;

pub use ext::{AecStats, AecTelemetry, GainAdjustmentMetering, SpeechProbability};
pub use params::{ParamError, ParamFlags, ParamId, ParamInfo};
pub use ports::{AudioPortInfo, AudioPorts, MONO_IN_OUT, MONO_WITH_REFERENCE, PORT_MONO};

/// Our namespace for stage ids, feature tags and extension ids (provisional, see [`namespaced!`]).
pub const NAMESPACE: &str = namespaced!("", "");
/// The vendor every one of our stages declares.
pub const VENDOR: Text = text("noise-filter");

/// A name, id or tag a stage describes itself with: borrowed from a literal for our stages (const-constructible), owned
/// for one known only at run time.
pub type Text = Cow<'static, str>;

/// A [`Text`] from a literal, usable in a `const`.
pub const fn text(s: &'static str) -> Text {
    Cow::Borrowed(s)
}

/// Feature tags (`clap_plugin_descriptor.features`): CLAP's standard ones we use (`plugin-features.h`), and our role tags.
pub mod features {
    use super::{Text, text};

    pub const AUDIO_EFFECT: Text = text("audio-effect");
    pub const RESTORATION: Text = text("restoration");
    pub const GATE: Text = text("gate");
    pub const MONO: Text = text("mono");
    /// Ours: the stage cancels echo; it takes the speaker reference on its non-main input.
    pub const ECHO_CANCEL: Text = text(namespaced!(":", "echo-cancel"));
    /// Ours: the stage is a noise filter (a model, its floor and its gate).
    pub const DENOISE: Text = text(namespaced!(":", "denoise"));
}

/// Where a stage sits in the chain: the parsed form of its role tag ([`StageDescriptor::role`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageRole {
    /// Echo cancellation: main input = mic, non-main input = speaker reference.
    EchoCancel,
    /// Noise suppression (a model plus its floor and gate).
    Noise,
    /// Post effects (a voice changer, EQ); none in v0.
    Effect,
}

/// What a stage is (`clap_plugin_descriptor`). Constant for its lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageDescriptor {
    /// Stable machine id, e.g. `noise-filter.denoise` ([`namespaced!`]).
    pub id: Text,
    /// Display name.
    pub name: Text,
    pub vendor: Text,
    /// Empty until the project has a public home.
    pub url: Text,
    pub version: Text,
    pub description: Text,
    pub features: Cow<'static, [Text]>,
}

impl StageDescriptor {
    /// Whether it declares the feature tag `tag`.
    pub fn has_feature(&self, tag: &str) -> bool {
        self.features.iter().any(|f| f == tag)
    }

    /// The role its feature tags give: one role tag gives that role, none gives [`StageRole::Effect`], both is an error.
    pub fn role(&self) -> Result<StageRole, StageError> {
        match (self.has_feature(&features::ECHO_CANCEL), self.has_feature(&features::DENOISE)) {
            (true, true) => {
                Err(StageError(format!("stage {} declares two roles ({} and {})", self.id, features::ECHO_CANCEL, features::DENOISE)))
            }
            (true, false) => Ok(StageRole::EchoCancel),
            (false, true) => Ok(StageRole::Noise),
            (false, false) => Ok(StageRole::Effect),
        }
    }
}

/// `activate`'s arguments (CLAP's `activate(sample_rate, min_frames_count, max_frames_count)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    pub sample_rate: u32,
    pub min_frames: u32,
    pub max_frames: u32,
}

impl Activation {
    /// The v0 host's one format: 48 kHz in fixed 480-frame blocks.
    pub const HOST: Activation = Activation { sample_rate: SAMPLE_RATE, min_frames: BLOCK as u32, max_frames: BLOCK as u32 };

    /// Refuses anything but [`Activation::HOST`], which is all our stages run at. `name` is the stage's, for the message.
    pub fn require_host(&self, name: &str) -> Result<(), StageError> {
        if *self == Self::HOST {
            Ok(())
        } else {
            let Activation { sample_rate, min_frames, max_frames } = *self;
            Err(StageError(format!(
                "{name} runs at {SAMPLE_RATE} Hz in {BLOCK}-frame blocks, not {sample_rate} Hz in {min_frames}..{max_frames}"
            )))
        }
    }
}

/// Stands in for an input port the host didn't connect (it always connects the main one).
static SILENT: Block = [0.0; BLOCK];

/// One block's buffers (`clap_process`'s audio part): one input per declared input port, in port order, and the main
/// output. Always [`BLOCK`] frames, mono (stereo later means a `&[&Block]` per port).
pub struct ProcessIo<'a> {
    /// `inputs[0]` is the main input; `inputs[1]`, if any, the non-main one (the speaker reference, in the v0 host).
    pub inputs: &'a [&'a Block],
    /// The main output.
    pub output: &'a mut Block,
}

impl<'a> ProcessIo<'a> {
    pub fn new(inputs: &'a [&'a Block], output: &'a mut Block) -> Self {
        Self { inputs, output }
    }

    /// The main input (silence if the host passed none, which it never does).
    #[inline]
    pub fn main(&self) -> &'a Block {
        self.inputs.first().copied().unwrap_or(&SILENT)
    }

    /// Input port `port`, if the host connected it.
    #[inline]
    pub fn aux(&self, port: usize) -> Option<&'a Block> {
        self.inputs.get(port).copied()
    }
}

/// What the chain reports from the noise slot each block it runs live, read from the stage's extensions after `process`
/// ([`ext`]). `NaN` = the stage doesn't publish it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageReport {
    /// Speech probability 0..1 (the model's VAD): `speech-probability/0`.
    pub speech_probability: f32,
    /// Attenuation the stage applied this block, in dB (<= 0): its output's level against its (latency-aligned) input's,
    /// so every part of the stage counts (the model, the floor mix and the gate): `gain-adjustment-metering/0`.
    pub gain_reduction_db: f32,
}

impl Default for StageReport {
    fn default() -> Self {
        Self { speech_probability: f32::NAN, gain_reduction_db: f32::NAN }
    }
}

impl StageReport {
    /// What `stage`'s extensions say about the block it just processed (NaN where it has none). Real-time.
    #[inline]
    pub fn read(stage: &dyn Stage) -> Self {
        Self {
            speech_probability: stage.speech_probability().map_or(f32::NAN, |e| e.speech_probability()),
            gain_reduction_db: stage.gain_adjustment().map_or(f32::NAN, |e| e.gain_adjustment_db()),
        }
    }
}

/// A stage failed to start, or refused a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageError(pub String);

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StageError {}

/// One processing step, CLAP-shaped (see the module notes for each call's thread and real-time rules). `Send` so it can
/// be built on one thread and run on the audio thread.
pub trait Stage: Send {
    /// What the stage is (constant).
    fn descriptor(&self) -> &StageDescriptor;

    /// Its audio ports (constant).
    fn audio_ports(&self) -> &AudioPorts;

    /// Prepares for processing (may allocate). Called before the first `process`, on the processing thread, and possibly
    /// again (an implied deactivate); it must not change our stages' latency.
    fn activate(&mut self, activation: &Activation) -> Result<(), StageError>;

    /// Stream offset in samples: `output[n]` corresponds to `input[n - latency()]`. Valid after `activate`.
    fn latency(&self) -> u32;

    /// Processes one block. Real-time: no allocation, locks, system calls or logging.
    fn process(&mut self, io: ProcessIo<'_>);

    /// Clears the processing state (the host resets a stage it switches back on). On the audio thread (the module notes for
    /// its one allocation exception); a no-op when nothing was processed since the last one.
    fn reset(&mut self);

    /// The parameters (`clap.params`); none by default.
    fn params(&self) -> &[ParamInfo] {
        &[]
    }

    /// A parameter's current value; `None` for an unknown id. Real-time safe.
    fn param_value(&self, _id: ParamId) -> Option<f64> {
        None
    }

    /// Sets a parameter: rejects NaN, clamps to the range, truncates a stepped value, and takes effect at the next block.
    /// Called by whoever holds the stage: while it is active, on the processing thread between blocks (CLAP's in-events),
    /// so it must not allocate or lock. Values from other threads reach a stage through its host, never through here (the
    /// module notes).
    fn set_param(&mut self, id: ParamId, _value: f64) -> Result<(), ParamError> {
        Err(ParamError::UnknownId(id))
    }

    /// Appends the stage's state (`clap.state`): by default its parameters ([`state::save_params`]).
    fn save_state(&self, out: &mut Vec<u8>) {
        state::save_params(self, out);
    }

    /// Restores a state from [`Stage::save_state`]: by default its parameters ([`state::load_params`]).
    fn load_state(&mut self, data: &[u8]) -> Result<(), StageError> {
        state::load_params(self, data)
    }

    /// `clap.gain-adjustment-metering/0` (draft).
    fn gain_adjustment(&self) -> Option<&dyn GainAdjustmentMetering> {
        None
    }

    /// Ours: `noise-filter.speech-probability/0`.
    fn speech_probability(&self) -> Option<&dyn SpeechProbability> {
        None
    }

    /// Ours: `noise-filter.aec-telemetry/0`.
    fn aec_telemetry(&self) -> Option<&dyn AecTelemetry> {
        None
    }
}

/// Checks that `stage` fits the v0 host profile, and returns its role:
/// - its role tags (at most one);
/// - its ports: input 0 and the one output are main and mono; an echo-cancel stage has exactly one non-main mono input
///   (the speaker reference), any other stage at most one; port ids are unique per direction;
/// - its params: unique ids, `min < max` and both finite, the default in range, a stepped param's bounds and default
///   integral, and an enum param stepped too (CLAP: `IS_ENUM` requires `IS_STEPPED`).
///
/// Not real-time ([`crate::chain::Chain::activate`] and the harness call it).
pub fn check_host_profile(stage: &dyn Stage) -> Result<StageRole, StageError> {
    let d = stage.descriptor();
    let role = d.role()?;
    let fail = |what: String| Err(StageError(format!("stage {}: {what}", d.id)));
    let ports = stage.audio_ports();
    let mono = |p: &AudioPortInfo| p.channel_count == 1 && p.port_type == PORT_MONO;
    match ports.inputs.first() {
        Some(p) if p.main && mono(p) => {}
        _ => return fail("input 0 must be the main mono input".into()),
    }
    match &ports.outputs[..] {
        [p] if p.main && mono(p) => {}
        _ => return fail(format!("needs exactly one output, main and mono (it has {})", ports.outputs.len())),
    }
    let aux = &ports.inputs[1..];
    if let Some(p) = aux.iter().find(|p| p.main || !mono(p)) {
        return fail(format!("input {} ({}) must be non-main and mono", p.id, p.name));
    }
    match (role, aux.len()) {
        (StageRole::EchoCancel, 1) | (StageRole::Noise | StageRole::Effect, 0 | 1) => {}
        (StageRole::EchoCancel, n) => return fail(format!("an echo canceller needs one non-main input for the reference, not {n}")),
        (_, n) => return fail(format!("takes at most one non-main input, not {n}")),
    }
    for list in [&ports.inputs[..], &ports.outputs[..]] {
        if list.iter().enumerate().any(|(i, p)| list[..i].iter().any(|q| q.id == p.id)) {
            return fail("port ids must be unique".into());
        }
    }
    let params = stage.params();
    for (i, p) in params.iter().enumerate() {
        if params[..i].iter().any(|q| q.id == p.id) {
            return fail(format!("param id {} is declared twice", p.id));
        }
        if !(p.min.is_finite() && p.max.is_finite() && p.min < p.max) {
            return fail(format!("param {} ({}): the range {}..{} must be finite and increasing", p.id, p.name, p.min, p.max));
        }
        if !(p.min..=p.max).contains(&p.default) {
            return fail(format!("param {} ({}): the default {} is outside {}..{}", p.id, p.name, p.default, p.min, p.max));
        }
        if p.flags.contains(ParamFlags::STEPPED) && [p.min, p.max, p.default].iter().any(|v| v.fract() != 0.0) {
            return fail(format!("param {} ({}): a stepped param's range and default must be integral", p.id, p.name));
        }
        if p.flags.contains(ParamFlags::ENUM) && !p.flags.contains(ParamFlags::STEPPED) {
            return fail(format!("param {} ({}): an enum param must be stepped too", p.id, p.name));
        }
    }
    Ok(role)
}

/// Compile-time check that the block size is what the stages assume.
const _: () = assert!(BLOCK == 480);

#[cfg(test)]
mod tests {
    use super::*;

    /// A configurable stand-in: its descriptor, ports and params are whatever a test gives it.
    struct Probe {
        descriptor: StageDescriptor,
        ports: AudioPorts,
        params: Vec<ParamInfo>,
    }

    impl Stage for Probe {
        fn descriptor(&self) -> &StageDescriptor {
            &self.descriptor
        }
        fn audio_ports(&self) -> &AudioPorts {
            &self.ports
        }
        fn activate(&mut self, a: &Activation) -> Result<(), StageError> {
            a.require_host("probe")
        }
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, io: ProcessIo<'_>) {
            *io.output = *io.main();
        }
        fn reset(&mut self) {}
        fn params(&self) -> &[ParamInfo] {
            &self.params
        }
    }

    /// A descriptor with these tags, its text owned: built at run time, as a loaded plugin's would be.
    fn descriptor(features: &[Text]) -> StageDescriptor {
        StageDescriptor {
            id: Text::Owned("test.probe".into()),
            name: text("probe"),
            vendor: VENDOR,
            url: text(""),
            version: Text::Owned(String::from("0")),
            description: text(""),
            features: features.to_vec().into(),
        }
    }

    fn probe(features: &[Text], ports: AudioPorts) -> Probe {
        Probe { descriptor: descriptor(features), ports, params: Vec::new() }
    }

    const fn param(id: ParamId, min: f64, max: f64, default: f64, flags: ParamFlags) -> ParamInfo {
        ParamInfo { id, name: text("p"), module: text(""), unit: text(""), min, max, default, flags }
    }

    /// Ports built at run time.
    fn ports(inputs: &[AudioPortInfo], outputs: &[AudioPortInfo]) -> AudioPorts {
        AudioPorts { inputs: inputs.to_vec().into(), outputs: outputs.to_vec().into() }
    }

    fn stereo_in() -> AudioPorts {
        let stereo = AudioPortInfo { id: 0, name: text("in"), main: true, channel_count: 2, port_type: Text::Owned("stereo".into()) };
        ports(&[stereo], &MONO_IN_OUT.outputs)
    }

    fn two_outputs() -> AudioPorts {
        ports(&MONO_IN_OUT.inputs, &[AudioPortInfo::main_mono(0, "a"), AudioPortInfo::aux_mono(1, "b")])
    }

    fn main_reference() -> AudioPorts {
        ports(&[AudioPortInfo::main_mono(0, "mic"), AudioPortInfo::main_mono(1, "ref")], &MONO_IN_OUT.outputs)
    }

    fn same_ids() -> AudioPorts {
        ports(&[AudioPortInfo::main_mono(0, "mic"), AudioPortInfo::aux_mono(0, "ref")], &MONO_IN_OUT.outputs)
    }

    fn three_inputs() -> AudioPorts {
        let ins = [AudioPortInfo::main_mono(0, "mic"), AudioPortInfo::aux_mono(1, "ref"), AudioPortInfo::aux_mono(2, "ref 2")];
        ports(&ins, &MONO_IN_OUT.outputs)
    }

    #[test]
    fn the_namespace_has_one_literal() {
        assert_eq!(NAMESPACE, "noise-filter");
        assert_eq!(features::ECHO_CANCEL, format!("{NAMESPACE}:echo-cancel"));
        assert_eq!(features::DENOISE, format!("{NAMESPACE}:denoise"));
        assert_eq!(ext::SPEECH_PROBABILITY_ID, format!("{NAMESPACE}.speech-probability/0"));
        assert_eq!(ext::AEC_TELEMETRY_ID, format!("{NAMESPACE}.aec-telemetry/0"));
    }

    #[test]
    fn the_role_comes_from_one_role_tag() {
        use features::*;
        assert_eq!(descriptor(&[AUDIO_EFFECT, MONO, ECHO_CANCEL]).role(), Ok(StageRole::EchoCancel));
        assert_eq!(descriptor(&[AUDIO_EFFECT, RESTORATION, GATE, DENOISE]).role(), Ok(StageRole::Noise));
        assert_eq!(descriptor(&[AUDIO_EFFECT, MONO]).role(), Ok(StageRole::Effect));
        assert_eq!(descriptor(&[]).role(), Ok(StageRole::Effect));
        let e = descriptor(&[ECHO_CANCEL, DENOISE]).role().unwrap_err();
        assert!(e.0.contains("two roles"), "{e}");
        // A tag from another namespace isn't ours; an owned tag counts like a literal one.
        assert_eq!(descriptor(&[text("other:denoise")]).role(), Ok(StageRole::Effect));
        assert_eq!(descriptor(&[Text::Owned(DENOISE.to_string())]).role(), Ok(StageRole::Noise));
        assert!(descriptor(&[MONO]).has_feature("mono") && !descriptor(&[MONO]).has_feature("gate"));
    }

    #[test]
    fn the_host_profile_takes_our_layouts() {
        use features::*;
        assert_eq!(check_host_profile(&probe(&[ECHO_CANCEL], MONO_WITH_REFERENCE.clone())), Ok(StageRole::EchoCancel));
        assert_eq!(check_host_profile(&probe(&[DENOISE], MONO_IN_OUT.clone())), Ok(StageRole::Noise));
        assert_eq!(check_host_profile(&probe(&[], MONO_IN_OUT.clone())), Ok(StageRole::Effect));
        // Any non-echo stage may take one sidechain (the host feeds it the reference).
        assert_eq!(check_host_profile(&probe(&[DENOISE], MONO_WITH_REFERENCE.clone())), Ok(StageRole::Noise));
    }

    #[test]
    fn the_host_profile_rejects_other_layouts() {
        use features::*;
        let reject = |s: Probe, why: &str| {
            let e = check_host_profile(&s).unwrap_err();
            assert!(e.0.contains(why), "{e} (wanted {why:?})");
        };
        reject(probe(&[ECHO_CANCEL], MONO_IN_OUT.clone()), "needs one non-main input");
        reject(probe(&[DENOISE], stereo_in()), "main mono input");
        reject(probe(&[DENOISE], two_outputs()), "exactly one output");
        reject(probe(&[ECHO_CANCEL], main_reference()), "non-main and mono");
        reject(probe(&[ECHO_CANCEL], same_ids()), "unique");
        reject(probe(&[DENOISE, ECHO_CANCEL], MONO_WITH_REFERENCE.clone()), "two roles");
        reject(probe(&[], three_inputs()), "at most one non-main input");
        reject(probe(&[ECHO_CANCEL], three_inputs()), "not 2");
        reject(probe(&[], ports(&[], &MONO_IN_OUT.outputs)), "input 0");
    }

    #[test]
    fn the_host_profile_checks_the_params() {
        let with = |params: &[ParamInfo]| check_host_profile(&Probe { params: params.to_vec(), ..probe(&[], MONO_IN_OUT.clone()) });
        let none = ParamFlags::NONE;
        assert!(with(&[param(1, -10.0, 0.0, -3.0, none), param(2, 0.0, 2.0, 1.0, ParamFlags::STEPPED)]).is_ok());
        let fails = |params: &[ParamInfo], why: &str| {
            let e = with(params).unwrap_err();
            assert!(e.0.contains(why), "{e} (wanted {why:?})");
        };
        fails(&[param(1, 0.0, 1.0, 0.0, none), param(1, 0.0, 1.0, 0.0, none)], "declared twice");
        fails(&[param(1, 1.0, 1.0, 1.0, none)], "finite and increasing");
        fails(&[param(1, 0.0, f64::INFINITY, 0.0, none)], "finite and increasing");
        fails(&[param(1, f64::NAN, 1.0, 0.0, none)], "finite and increasing");
        fails(&[param(1, 0.0, 1.0, 2.0, none)], "outside");
        fails(&[param(1, 0.0, 1.0, f64::NAN, none)], "outside");
        fails(&[param(1, 0.0, 2.5, 1.0, ParamFlags::STEPPED)], "integral");
        fails(&[param(1, 0.0, 2.0, 0.5, ParamFlags::STEPPED)], "integral");
        fails(&[param(1, 0.0, 2.0, 0.0, ParamFlags::ENUM)], "enum param must be stepped");
        assert!(with(&[param(1, 0.0, 2.0, 0.0, ParamFlags::ENUM.union(ParamFlags::STEPPED))]).is_ok());
    }

    #[test]
    fn activation_refuses_other_formats() {
        assert!(Activation::HOST.require_host("x").is_ok());
        for a in [Activation { sample_rate: 44_100, ..Activation::HOST }, Activation { min_frames: 1, ..Activation::HOST }, Activation {
            max_frames: 512,
            ..Activation::HOST
        }] {
            let e = a.require_host("x").unwrap_err();
            assert!(e.0.starts_with("x runs at 48000 Hz in 480-frame blocks"), "{e}");
        }
    }

    #[test]
    fn process_io_gives_the_ports_in_order() {
        let (a, b) = ([1.0f32; BLOCK], [2.0f32; BLOCK]);
        let mut out = [0.0f32; BLOCK];
        let ins = [&a, &b];
        let io = ProcessIo::new(&ins, &mut out);
        assert_eq!((io.main()[0], io.aux(1).map(|r| r[0]), io.aux(2)), (1.0, Some(2.0), None));
        let io = ProcessIo::new(&[], &mut out);
        assert_eq!(io.main(), &[0.0; BLOCK], "no input reads as silence");
    }

    #[test]
    fn the_defaults_have_no_params_and_no_extensions() {
        let mut s = probe(&[], MONO_IN_OUT.clone());
        assert!(s.params().is_empty() && s.param_value(1).is_none());
        assert_eq!(s.set_param(1, 0.0), Err(ParamError::UnknownId(1)));
        assert!(s.gain_adjustment().is_none() && s.speech_probability().is_none() && s.aec_telemetry().is_none());
        let mut blob = Vec::new();
        s.save_state(&mut blob);
        assert_eq!(blob.len(), 12, "the header and a count of zero");
        assert!(s.load_state(&blob).is_ok());
    }
}
