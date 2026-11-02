//! The noise stage (TODO 3, 17): one CLAP-shaped stage that runs a noise model, then the floor mix and the gate
//! ([`NoisePost`]). The floor and gate are its params, over [`ChainControls`]' atomics; the stage's gain is its
//! `gain-adjustment-metering/0` and the model's VAD its `speech-probability/0` (the owner's TODO 17 decision: the floor and
//! gate inside the noise stage, an extension publishing speech probability).
//!
//! Models plug in through [`NoiseModel`], the internal model trait (not in the public spec): RNNoise
//! (`nf_stages::RnnoiseModel`), and model packs through ONNX Runtime (TODO 5: `nf_ort::OrtSpectralModel`, which owns its
//! STFT, session, tensors, state pairs and rate handling). Two adaptors layer around a model: [`Reblocked`] runs a model
//! of any hop in 480-sample hops, and [`Pipelined`] runs one on an inference worker thread, `depth` blocks behind (TODO 6,
//! the one exception to the real-time rule: the worker may allocate per run, measured; the audio thread doesn't).
//!
//! **Latency.** A model reports `latency_samples` (algorithmic: window + lookahead × hop, at 48 kHz) and its `hop`; its
//! stream offset is `latency_samples - hop`. A hop other than 480 is re-blocked ([`Reblocker`]), which adds `hop -
//! gcd(hop, 480)`. So the stage's latency is `L = (latency_samples - hop) + (hop - gcd(hop, 480))`, plus `depth` × 480 if
//! the model runs on a worker (TODO 6, [`Pipelined`]): RNNoise 480 + 0 = 480; a hop of 512 (FastEnhancer-48k, offset 512)
//! 992; UL-UNAS bin-mapped (hop 768, latency 1536) 768 + 672 = 1440, and 2400 on its worker at the default depth of 2.
//! `NoisePost`'s dry delay is `L`, so the floor lines up at any hop.
//!
//! **Per block:** sanitise the input into `clean` (NaN and ±inf to 0, the rest clamped to ±[`crate::dsp::INPUT_LIMIT`],
//! so no model sees a sample it could overflow); the model, straight into the output for hop 480 (RNNoise: bit for bit
//! what it did as a stage of its own), else through the re-blocker; zero non-finite output and clamp to ±1; then
//! `NoisePost` on `clean` and the output with the VAD. The VAD is the last hop's, and carries over a block with no hop
//! (hop > 480).
//!
//! **Reset:** when something was processed since the last one, the model, the re-blocker and `NoisePost`, in that order;
//! otherwise nothing (so the chain's resets don't spend RNNoise's spare state). Everything is sized in [`NoiseStage::new`],
//! so `activate` again never clears state. The one allocation on the audio thread is a model's: RNNoise's second real
//! reset before the next `activate` rebuilds its state in place (ten allocations, [`NoiseModel::reset`]).

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::chain::MAX_STAGE_LATENCY;
use crate::controls::ChainControls;
use crate::dsp::noise_post::NoisePost;
use crate::dsp::reblock::{Reblocker, reblock_delay};
use crate::dsp::sanitize_into;
use crate::stage::{
    Activation, AudioPorts, GainAdjustmentMetering, MONO_IN_OUT, ParamError, ParamFlags, ParamId, ParamInfo, ProcessIo, SpeechProbability,
    Stage, StageDescriptor, StageError, StageReport, Text, VENDOR, features, text,
};
use crate::{BLOCK, Block};

mod pipeline;
mod reblocked;

pub use pipeline::{
    DEFAULT_DEPTH, Executor, InferenceSnapshot, InferenceStats, MAX_DEPTH, MakeModel, NoHooks, Pipelined, READY_TIMEOUT, RING,
    WorkerCounters, WorkerGuard, WorkerHooks,
};
pub use reblocked::Reblocked;

/// What a model is, at the 48 kHz host rate. Its text is [`Text`]: literals for a built-in model, owned for a model pack
/// (its manifest's `id` and `name`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// Stable machine id, e.g. `"rnnoise"` (a pack's `id` later).
    pub id: Text,
    pub name: Text,
    /// Samples per `process_hop` call, at 48 kHz.
    pub hop: usize,
    /// Algorithmic latency at 48 kHz: window + lookahead × hop. The stream offset is `latency_samples - hop`.
    pub latency_samples: u32,
    /// `process_hop` returns a speech probability (else NaN).
    pub has_vad: bool,
}

impl ModelInfo {
    /// The model's stream offset in its own framing: `latency_samples - hop` (0 if `latency_samples < hop`, which
    /// [`NoiseStage::new`] refuses).
    pub const fn model_offset(&self) -> usize {
        (self.latency_samples as usize).saturating_sub(self.hop)
    }

    /// The noise stage's latency around this model: its offset plus the re-blocking delay (module notes).
    pub const fn stage_latency(&self) -> usize {
        self.model_offset() + reblock_delay(self.hop)
    }
}

/// A noise model inside [`NoiseStage`]. `Send`: built on one thread, run on the audio thread.
pub trait NoiseModel: Send {
    /// Constant for the model's lifetime.
    fn info(&self) -> &ModelInfo;

    /// Gets ready to process: called by [`NoiseStage`]'s `activate`, on the processing thread. May allocate (a warm-up, a
    /// spare state).
    fn prepare(&mut self) -> Result<(), StageError>;

    /// Back to a fresh state. On the audio thread, so no locks or system calls, and no allocation, with one exception: a
    /// model that keeps a spare state (RNNoise) swaps it in at the first reset, and may rebuild in place at any further
    /// one until the next `prepare` (RNNoise: ten allocations, which the engine's real-time pool serves).
    fn reset(&mut self);

    /// Denoises one hop: `input` and `output` are each `hop` samples, at full scale ±1. The input is finite and within
    /// ±[`crate::dsp::INPUT_LIMIT`] (the stage zeros NaN and ±inf and clamps the rest). A hot mic can exceed ±1, so a model
    /// must stay finite up to that limit; the stage clamps the output to ±1. Returns the speech probability for the window
    /// this output came from, or NaN. Real-time, unless the model runs inside [`Pipelined`] on its inference worker, where it
    /// may allocate per call (TODO 6: measured, and off the audio thread).
    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32;

    /// What the hops since the last call did beyond their output: the inferences they ran, the time those took, and whether
    /// one failed (its output is then to be discarded: [`Pipelined`] substitutes for it and resets the model). Read after
    /// each hop or block, on the thread that ran it. Real-time. A model with nothing to report keeps the default.
    fn take_report(&mut self) -> HopReport {
        HopReport::default()
    }
}

/// What a model's hops did ([`NoiseModel::take_report`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HopReport {
    /// Inferences run (a model pack's session runs).
    pub runs: u32,
    /// Their total time, in µs.
    pub run_us: f32,
    /// One of them failed (an error, or a non-finite output): the output is the model's fallback, not its estimate.
    pub failed: bool,
}

impl HopReport {
    /// Adds `other`'s runs, time and failure to this one.
    #[inline]
    pub fn add(&mut self, other: HopReport) {
        self.runs += other.runs;
        self.run_us += other.run_us;
        self.failed |= other.failed;
    }
}

/// The noise stage's param ids, stable from now on (state blobs hold them).
pub mod param {
    use crate::stage::ParamId;

    pub const FLOOR: ParamId = 1;
    pub const GATE_MODE: ParamId = 2;
    pub const GATE_THRESHOLD: ParamId = 3;
    pub const GATE_AUTO_MARGIN: ParamId = 4;
    pub const GATE_ATTACK: ParamId = 5;
    pub const GATE_RELEASE: ParamId = 6;
    pub const GATE_HOLD: ParamId = 7;
    pub const GATE_RANGE: ParamId = 8;
    pub const GATE_USE_VAD: ParamId = 9;
}

/// The gate mode param's values, in order (`GateMode::to_u8`).
pub const GATE_MODE_NAMES: [&str; 3] = ["Off", "Static", "Auto"];

#[rustfmt::skip]
const fn p(
    id: ParamId, name: &'static str, module: &'static str, unit: &'static str, range: (f64, f64), default: f64, flags: ParamFlags,
) -> ParamInfo {
    ParamInfo { id, name: text(name), module: text(module), unit: text(unit), min: range.0, max: range.1, default, flags }
}

const NONE: ParamFlags = ParamFlags::NONE;

/// The noise stage's params. Their ranges are what the DSP accepts (wider than the GUI's spans, which stay the GUI's);
/// their defaults are `ProcessingConfig::default()`'s and `GateConfig::default()`'s (a test checks).
#[rustfmt::skip]
pub static PARAMS: [ParamInfo; 9] = [
    p(param::FLOOR, "Floor", "", "dB", (-120.0, 0.0), -60.0, NONE),
    p(param::GATE_MODE, "Gate mode", "Gate", "", (0.0, 2.0), 0.0, ParamFlags::STEPPED.union(ParamFlags::ENUM)),
    p(param::GATE_THRESHOLD, "Gate threshold", "Gate", "dBFS", (-120.0, 0.0), -50.0, NONE),
    p(param::GATE_AUTO_MARGIN, "Gate auto margin", "Gate", "dB", (0.0, 40.0), 6.0, NONE),
    p(param::GATE_ATTACK, "Gate attack", "Gate", "ms", (0.0, 1000.0), 2.0, NONE),
    p(param::GATE_RELEASE, "Gate release", "Gate", "ms", (0.0, 5000.0), 150.0, NONE),
    p(param::GATE_HOLD, "Gate hold", "Gate", "ms", (0.0, 5000.0), 100.0, NONE),
    p(param::GATE_RANGE, "Gate range", "Gate", "dB", (-120.0, 0.0), -40.0, NONE),
    p(param::GATE_USE_VAD, "Gate holds open on speech", "Gate", "", (0.0, 1.0), 0.0, ParamFlags::STEPPED),
];

/// The noise stage's descriptor, whichever model it runs.
pub static DESCRIPTOR: StageDescriptor = StageDescriptor {
    id: text(crate::namespaced!(".", "denoise")),
    name: text("Noise filter"),
    vendor: VENDOR,
    url: text(""),
    version: text(env!("CARGO_PKG_VERSION")),
    description: text("Speech noise suppression by a neural model, then an attenuation floor and a gate"),
    features: Cow::Borrowed(&[features::AUDIO_EFFECT, features::RESTORATION, features::GATE, features::MONO, features::DENOISE]),
};

/// The noise stage: a [`NoiseModel`], then the floor and the gate (module notes).
pub struct NoiseStage {
    /// The floor and the gate are read from `controls.noise` every block.
    controls: Arc<ChainControls>,
    model: Box<dyn NoiseModel>,
    /// The model publishes a speech probability (`ModelInfo::has_vad`).
    has_vad: bool,
    latency: u32,
    /// For a hop other than [`BLOCK`].
    reblock: Option<Reblocker>,
    post: NoisePost,
    // Inline rather than boxed, and filled by a kernel taking each block as its own argument: then LLVM knows nothing
    // overlaps and vectorises without run-time overlap checks.
    /// This block's input with non-finite samples zeroed: the model's input and the dry path.
    clean: Block,
    /// The last block's speech probability and gain, for the extensions.
    last: Published,
    /// The last hop's VAD.
    vad: f32,
    /// Something was processed since `new` or the last `reset`.
    dirty: bool,
}

impl NoiseStage {
    /// Wraps `model`, sizing everything for it: refuses a hop of 0, a `latency_samples` under the hop, and a stage latency
    /// over [`MAX_STAGE_LATENCY`]. Allocates.
    pub fn new(controls: Arc<ChainControls>, model: Box<dyn NoiseModel>) -> Result<Self, StageError> {
        let info = model.info();
        let fail = |why: String| Err(StageError(format!("noise model {}: {why}", info.id)));
        if info.hop == 0 {
            return fail("a hop of 0 samples".into());
        }
        if (info.latency_samples as usize) < info.hop {
            return fail(format!("latency_samples {} is under its hop {}", info.latency_samples, info.hop));
        }
        let latency = info.stage_latency();
        if latency > MAX_STAGE_LATENCY {
            return fail(format!("the stage latency {latency} would exceed {MAX_STAGE_LATENCY}"));
        }
        let (latency, hop, has_vad) = (latency as u32, info.hop, info.has_vad);
        Ok(Self {
            controls,
            model,
            has_vad,
            latency,
            reblock: (hop != BLOCK).then(|| Reblocker::new(hop)),
            post: NoisePost::new(latency),
            clean: [0.0; BLOCK],
            last: Published::default(),
            vad: f32::NAN,
            dirty: false,
        })
    }

    /// The model it runs.
    pub fn model_info(&self) -> &ModelInfo {
        self.model.info()
    }

    fn info_of(id: ParamId) -> Result<&'static ParamInfo, ParamError> {
        PARAMS.iter().find(|p| p.id == id).ok_or(ParamError::UnknownId(id))
    }
}

impl Stage for NoiseStage {
    fn descriptor(&self) -> &StageDescriptor {
        &DESCRIPTOR
    }

    fn audio_ports(&self) -> &AudioPorts {
        &MONO_IN_OUT
    }

    /// Checks the format, then prepares the model on the calling thread (RNNoise: rebuilds a used spare, warms up easyfft).
    fn activate(&mut self, activation: &Activation) -> Result<(), StageError> {
        activation.require_host(&DESCRIPTOR.name)?;
        self.model.prepare()
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn process(&mut self, io: ProcessIo<'_>) {
        self.dirty = true;
        sanitize_into(io.main(), &mut self.clean);
        let out = io.output;
        match self.reblock.as_mut() {
            None => self.vad = self.model.process_hop(&self.clean, out),
            Some(r) => {
                let (model, vad) = (&mut self.model, &mut self.vad);
                r.process(&self.clean, out, |i, o| *vad = model.process_hop(i, o));
            }
        }
        clamp_output(out);
        self.last = Published(self.post.process(&self.clean, out, &self.controls.noise, self.vad));
    }

    fn reset(&mut self) {
        if !self.dirty {
            return; // already fresh: keep RNNoise's spare
        }
        self.dirty = false;
        self.model.reset();
        if let Some(r) = self.reblock.as_mut() {
            r.reset();
        }
        self.post.reset();
        self.vad = f32::NAN;
        self.last = Published::default();
    }

    fn params(&self) -> &[ParamInfo] {
        &PARAMS
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        let n = &self.controls.noise;
        let g = &n.gate;
        Some(match id {
            param::FLOOR => f64::from(n.floor_db.load()),
            param::GATE_MODE => f64::from(g.mode.load(Relaxed)),
            param::GATE_THRESHOLD => f64::from(g.threshold_db.load()),
            param::GATE_AUTO_MARGIN => f64::from(g.auto_margin_db.load()),
            param::GATE_ATTACK => f64::from(g.attack_ms.load()),
            param::GATE_RELEASE => f64::from(g.release_ms.load()),
            param::GATE_HOLD => f64::from(g.hold_ms.load()),
            param::GATE_RANGE => f64::from(g.range_db.load()),
            param::GATE_USE_VAD => f64::from(u8::from(g.use_vad.load(Relaxed))),
            _ => return None,
        })
    }

    fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), ParamError> {
        let v = Self::info_of(id)?.accept(value)?;
        let n = &self.controls.noise;
        let g = &n.gate;
        let f = v as f32;
        match id {
            param::FLOOR => n.floor_db.store(f),
            param::GATE_MODE => g.mode.store(v as u8, Relaxed),
            param::GATE_THRESHOLD => g.threshold_db.store(f),
            param::GATE_AUTO_MARGIN => g.auto_margin_db.store(f),
            param::GATE_ATTACK => g.attack_ms.store(f),
            param::GATE_RELEASE => g.release_ms.store(f),
            param::GATE_HOLD => g.hold_ms.store(f),
            param::GATE_RANGE => g.range_db.store(f),
            param::GATE_USE_VAD => g.use_vad.store(v != 0.0, Relaxed),
            _ => return Err(ParamError::UnknownId(id)),
        }
        Ok(())
    }

    fn gain_adjustment(&self) -> Option<&dyn GainAdjustmentMetering> {
        Some(&self.last)
    }

    fn speech_probability(&self) -> Option<&dyn SpeechProbability> {
        if self.has_vad { Some(&self.last) } else { None }
    }
}

/// The last block's speech probability and gain: what the extensions read. A type of its own, so the stage's
/// `Stage::speech_probability` getter and the extension's value never share a receiver.
#[derive(Clone, Copy, Debug, Default)]
struct Published(StageReport);

impl GainAdjustmentMetering for Published {
    fn gain_adjustment_db(&self) -> f32 {
        self.0.gain_reduction_db
    }
}

impl SpeechProbability for Published {
    fn speech_probability(&self) -> f32 {
        self.0.speech_probability
    }
}

/// Non-finite model output becomes 0, the rest is clamped to ±1 (models don't clamp).
#[inline]
fn clamp_output(out: &mut Block) {
    for v in out.iter_mut() {
        *v = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
    }
}

/// A pure delay as a model, for the harness's re-blocker and latency checks: its output is its input `latency_samples -
/// hop` samples later, in its own hop framing. With [`DelayModel::with_vad`] it reports each hop's peak |x| (at most 1)
/// as its speech probability, so a VAD's alignment and carry-over can be checked too.
#[derive(Clone, Debug)]
pub struct DelayModel {
    info: ModelInfo,
    /// The last `latency_samples - hop` inputs, a ring.
    ring: Box<[f32]>,
    pos: usize,
}

impl DelayModel {
    pub fn new(hop: usize, latency_samples: u32) -> Self {
        let offset = (latency_samples as usize).saturating_sub(hop);
        let info = ModelInfo { id: text("delay"), name: text("Pure delay (a test model)"), hop, latency_samples, has_vad: false };
        Self { info, ring: vec![0.0; offset].into_boxed_slice(), pos: 0 }
    }

    pub fn with_vad(mut self) -> Self {
        self.info.has_vad = true;
        self
    }
}

impl NoiseModel for DelayModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    fn prepare(&mut self) -> Result<(), StageError> {
        Ok(())
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }

    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let n = self.ring.len();
        if n == 0 {
            output.copy_from_slice(input);
        } else {
            for (o, &x) in output.iter_mut().zip(input) {
                *o = self.ring[self.pos];
                self.ring[self.pos] = x;
                self.pos = if self.pos + 1 == n { 0 } else { self.pos + 1 };
            }
        }
        if self.info.has_vad { input.iter().fold(0.0f32, |m, v| m.max(v.abs())).min(1.0) } else { f32::NAN }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GateConfig, GateMode, ProcessingConfig};
    use crate::dsp::INPUT_LIMIT;
    use crate::stage::{StageRole, check_host_profile, state};

    fn controls(floor_db: f32) -> Arc<ChainControls> {
        Arc::new(ChainControls::new(&ProcessingConfig { floor_db, ..ProcessingConfig::default() }))
    }

    fn delay_stage(hop: usize, latency_samples: u32, floor_db: f32) -> NoiseStage {
        let mut s = NoiseStage::new(controls(floor_db), Box::new(DelayModel::new(hop, latency_samples))).expect("stage");
        s.activate(&Activation::HOST).expect("activate");
        s
    }

    fn run(s: &mut NoiseStage, x: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (xi, yo) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0) {
            s.process(ProcessIo::new(&[xi], yo));
        }
        y
    }

    fn ramp(len: usize) -> Vec<f32> {
        (0..len).map(|i| ((i * 7919) % 2000) as f32 / 2000.0 - 0.5).collect()
    }

    #[test]
    fn it_fits_the_host_profile_as_the_noise_role() {
        let s = delay_stage(480, 960, -60.0);
        assert_eq!(check_host_profile(&s), Ok(StageRole::Noise));
        assert_eq!(s.descriptor().id, "noise-filter.denoise");
        assert_eq!(s.descriptor().version, env!("CARGO_PKG_VERSION"));
        assert!(s.gain_adjustment().is_some() && s.aec_telemetry().is_none());
        assert!(s.speech_probability().is_none(), "the delay model has no VAD");
        let v = NoiseStage::new(controls(-60.0), Box::new(DelayModel::new(480, 960).with_vad())).expect("stage");
        assert!(v.speech_probability().is_some());
    }

    #[test]
    fn other_formats_are_refused() {
        let mut s = delay_stage(480, 960, -60.0);
        assert!(s.activate(&Activation { sample_rate: 44_100, ..Activation::HOST }).is_err());
        assert!(s.activate(&Activation { max_frames: 512, ..Activation::HOST }).is_err());
        assert!(s.activate(&Activation::HOST).is_ok());
    }

    #[test]
    fn bad_models_are_refused() {
        let new = |hop, lat| NoiseStage::new(controls(-60.0), Box::new(DelayModel::new(hop, lat))).map(|_| ());
        assert!(new(0, 960).unwrap_err().0.contains("hop of 0"));
        assert!(new(480, 479).unwrap_err().0.contains("under its hop"));
        assert!(new(1024, 1024 + 4096).unwrap_err().0.contains("exceed"));
        assert!(new(480, 480 + 4096).is_ok(), "exactly the limit");
    }

    /// The re-blocker's hops, with and without a frame of lookahead: the latency follows the formula, the output is the
    /// input delayed by it sample for sample (floor off), and a 0 dB floor gives exactly the delayed dry signal.
    #[test]
    fn the_latency_follows_the_formula_at_any_hop() {
        for hop in [160usize, 240, 256, 480, 512, 768, 960, 1024] {
            for lookahead in [0usize, 1] {
                let lat = (2 * hop + lookahead * hop) as u32;
                let expect = (lat as usize - hop) + (hop - crate::dsp::reblock::gcd(hop, BLOCK));
                let x = ramp(40 * BLOCK);
                let mut s = delay_stage(hop, lat, -100.0);
                assert_eq!(s.latency() as usize, expect, "hop {hop}, lookahead {lookahead}");
                let y = run(&mut s, &x);
                for n in 0..y.len() {
                    let want = if n >= expect { x[n - expect] } else { 0.0 };
                    assert_eq!(y[n], want, "hop {hop}, lookahead {lookahead}: sample {n}");
                }
                // A 0 dB floor is all dry: the dry path is delayed by the same L.
                let mut dry = delay_stage(hop, lat, 0.0);
                assert_eq!(run(&mut dry, &x), y, "hop {hop}, lookahead {lookahead}: the floor's dry path is aligned");
            }
        }
        let info = |hop, latency_samples| ModelInfo { id: text(""), name: text(""), hop, latency_samples, has_vad: false };
        assert_eq!(info(512, 1024).stage_latency(), 992);
        assert_eq!(info(768, 1536).stage_latency(), 1440);
    }

    #[test]
    fn bad_samples_are_zeroed_and_the_output_is_clamped() {
        let mut s = delay_stage(480, 480, -100.0); // no delay: what goes in comes out
        let mut x = vec![0.5f32; BLOCK];
        x[1] = f32::NAN;
        x[2] = f32::INFINITY;
        x[3] = 3.0;
        x[4] = -7.0;
        let y = run(&mut s, &x);
        assert_eq!(&y[..6], &[0.5, 0.0, 0.0, 1.0, -1.0, 0.5]);
        // A finite but huge sample reaches the model bounded at ±INPUT_LIMIT, and the dry path too: a 0 dB floor is all dry,
        // and the floor mix isn't clamped (the engine clips its output).
        let mut x = vec![0.25f32; BLOCK];
        (x[1], x[2], x[3], x[4]) = (1e17, f32::MAX, -f32::MAX, -2e4);
        let l = INPUT_LIMIT;
        assert_eq!(&run(&mut delay_stage(480, 480, 0.0), &x)[..6], &[0.25, l, l, -l, -l, 0.25], "the dry path, bounded");
        let y = run(&mut delay_stage(480, 480, -120.0), &x);
        assert!(y[1..5].iter().all(|v| (1.0..1.01).contains(&v.abs())), "the model's output, clamped: {:?}", &y[..6]);
    }

    #[test]
    fn the_vad_comes_from_the_last_hop_and_carries_over() {
        // Hop 960: a hop every other block. The VAD (the hop's peak) changes on the blocks that run one, and holds between.
        let mut s = NoiseStage::new(controls(-100.0), Box::new(DelayModel::new(960, 1920).with_vad())).expect("stage");
        s.activate(&Activation::HOST).unwrap();
        let mut out = [0.0f32; BLOCK];
        let mut seen = Vec::new();
        for b in 0..6 {
            let x = [0.1 * (b + 1) as f32; BLOCK];
            s.process(ProcessIo::new(&[&x], &mut out));
            seen.push(s.speech_probability().map(|e| e.speech_probability()).unwrap());
        }
        assert!(seen[0].is_nan(), "no hop yet: unknown");
        assert_eq!(&seen[1..], &[0.2, 0.2, 0.4, 0.4, 0.6], "{seen:?}");
        s.reset();
        assert!(s.speech_probability().unwrap().speech_probability().is_nan(), "a reset forgets it");
    }

    #[test]
    fn reset_with_nothing_processed_changes_nothing_and_a_real_one_is_fresh() {
        let x = ramp(12 * BLOCK);
        for hop in [480usize, 512] {
            let fresh = run(&mut delay_stage(hop, 2 * hop as u32, -20.0), &x);
            let mut s = delay_stage(hop, 2 * hop as u32, -20.0);
            s.reset();
            s.reset();
            assert_eq!(run(&mut s, &x), fresh, "hop {hop}: unused resets");
            s.reset();
            assert_eq!(run(&mut s, &x), fresh, "hop {hop}: after a reset");
            s.activate(&Activation::HOST).unwrap();
            assert_ne!(run(&mut s, &x), fresh, "hop {hop}: activate again keeps the state");
        }
    }

    #[test]
    fn param_defaults_are_the_config_defaults() {
        let s = NoiseStage::new(controls(ProcessingConfig::default().floor_db), Box::new(DelayModel::new(480, 960))).unwrap();
        let cfg = ProcessingConfig::default();
        let g = GateConfig::default();
        assert_eq!(cfg.gate, g);
        let expect = [
            f64::from(cfg.floor_db),
            f64::from(g.mode.to_u8()),
            f64::from(g.threshold_db),
            f64::from(g.auto_margin_db),
            f64::from(g.attack_ms),
            f64::from(g.release_ms),
            f64::from(g.hold_ms),
            f64::from(g.range_db),
            f64::from(u8::from(g.use_vad)),
        ];
        for (p, want) in PARAMS.iter().zip(expect) {
            assert_eq!(p.default, want, "{} default", p.name);
            assert_eq!(s.param_value(p.id), Some(want), "{} as the controls hold it", p.name);
        }
        let ids: Vec<ParamId> = s.params().iter().map(|p| p.id).collect();
        assert_eq!(ids, (1..=9).collect::<Vec<_>>(), "stable ids");
        assert_eq!(GATE_MODE_NAMES.map(|n| n.len()), [3, 6, 4]);
        assert_eq!([GateMode::Off, GateMode::Static, GateMode::Auto].map(GateMode::to_u8), [0, 1, 2]);
    }

    #[test]
    fn set_param_clamps_and_reaches_the_controls() {
        let c = controls(-60.0);
        let mut s = NoiseStage::new(c.clone(), Box::new(DelayModel::new(480, 960))).unwrap();
        s.set_param(param::FLOOR, -300.0).unwrap();
        s.set_param(param::GATE_MODE, 2.9).unwrap();
        s.set_param(param::GATE_THRESHOLD, -41.5).unwrap();
        s.set_param(param::GATE_RELEASE, 1e9).unwrap();
        s.set_param(param::GATE_USE_VAD, 1.0).unwrap();
        assert_eq!(s.param_value(param::FLOOR), Some(-120.0));
        assert_eq!(s.param_value(param::GATE_MODE), Some(2.0));
        assert_eq!(s.param_value(param::GATE_RELEASE), Some(5000.0));
        let p = c.load();
        assert_eq!((p.floor_db, p.gate.mode, p.gate.threshold_db, p.gate.use_vad), (-120.0, GateMode::Auto, -41.5, true));
        assert_eq!(s.set_param(param::GATE_HOLD, f64::NAN), Err(ParamError::NotANumber(param::GATE_HOLD)));
        assert_eq!(s.set_param(10, 0.0), Err(ParamError::UnknownId(10)));
        assert_eq!(s.param_value(0), None);
        // The config path writes the same atomics, unclamped (TODO 15 decides whether the GUI goes through params).
        c.noise.floor_db.store(-200.0);
        assert_eq!(s.param_value(param::FLOOR), Some(-200.0));
    }

    #[test]
    fn the_state_round_trips_through_the_params() {
        let mut a = NoiseStage::new(controls(-60.0), Box::new(DelayModel::new(480, 960))).unwrap();
        a.set_param(param::FLOOR, -18.0).unwrap();
        a.set_param(param::GATE_MODE, 1.0).unwrap();
        a.set_param(param::GATE_HOLD, 250.0).unwrap();
        let mut blob = Vec::new();
        a.save_state(&mut blob);
        assert_eq!(&blob[..4], &state::MAGIC);
        let mut b = NoiseStage::new(controls(-60.0), Box::new(DelayModel::new(480, 960))).unwrap();
        b.load_state(&blob).unwrap();
        for p in &PARAMS {
            assert_eq!(b.param_value(p.id), a.param_value(p.id), "{}", p.name);
        }
    }
}
