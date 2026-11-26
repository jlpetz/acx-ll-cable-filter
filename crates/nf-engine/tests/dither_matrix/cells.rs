//! The matrix's chains: what the engine can run, each gate mode where there's a noise stage, each chain built as the engine
//! builds it, with its noise model's hops timed.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use nf_core::chain::Chain;
use nf_core::config::{DEFAULT_NOISE_MODEL, GateConfig, GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::noise::{HopReport, ModelInfo, NoiseModel, NoiseStage};
use nf_core::stage::{Stage, StageError};
use nf_engine::models::{ModelThread, build_chain};
use nf_stages::Aec3Stage;

/// The model pack the UL-UNAS chains run.
pub const ULUNAS: &str = "ulunas";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Rnnoise,
    /// [`ULUNAS`], through ONNX Runtime (an `onnx` build).
    Ulunas,
}

/// A chain the engine can run: AEC3 on or off, and the noise stage's model, or none (the noise filter off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainKind {
    pub name: &'static str,
    pub aec: bool,
    pub model: Option<Model>,
}

/// Every chain, in the order the matrix runs them. "aec3" (the echo canceller with the noise filter off) is beyond the
/// matrix in `doc/todo/10-aec.md`: the app runs it, and it shows AEC3's tail without a noise stage after it.
#[rustfmt::skip]
pub const CHAINS: [ChainKind; 6] = [
    ChainKind { name: "bypass", aec: false, model: None },
    ChainKind { name: "aec3", aec: true, model: None },
    ChainKind { name: "rnnoise", aec: false, model: Some(Model::Rnnoise) },
    ChainKind { name: "rnnoise+aec3", aec: true, model: Some(Model::Rnnoise) },
    ChainKind { name: "ulunas", aec: false, model: Some(Model::Ulunas) },
    ChainKind { name: "ulunas+aec3", aec: true, model: Some(Model::Ulunas) },
];

/// The gate's modes, as the matrix names them.
pub const GATES: [GateMode; 3] = [GateMode::Off, GateMode::Static, GateMode::Auto];

pub fn gate_name(mode: GateMode) -> &'static str {
    match mode {
        GateMode::Off => "off",
        GateMode::Static => "static",
        GateMode::Auto => "auto",
    }
}

impl ChainKind {
    /// The gate's modes for this chain: all three around a noise stage (the gate is part of it); without one there is no gate,
    /// and the one cell runs with the default (off), named "-".
    pub fn gates(&self) -> &'static [GateMode] {
        if self.model.is_some() { &GATES } else { &[GateMode::Off] }
    }

    /// The gate's name in this chain's cells.
    pub fn gate_label(&self, mode: GateMode) -> &'static str {
        if self.model.is_some() { gate_name(mode) } else { "-" }
    }

    /// The app's default processing settings with AEC3, the noise filter, its model and the gate's mode as this cell says (the
    /// gate's other settings, the floor at -60 dB and the meters on are the defaults).
    pub fn config(&self, gate: GateMode) -> ProcessingConfig {
        let model = match self.model {
            Some(Model::Ulunas) => ULUNAS,
            _ => DEFAULT_NOISE_MODEL,
        };
        ProcessingConfig {
            aec_enabled: self.aec,
            noise_enabled: self.model.is_some(),
            noise_model: model.to_owned(),
            gate: GateConfig { mode: gate, ..GateConfig::default() },
            ..ProcessingConfig::default()
        }
    }

    /// Why this build can't run the chain, if it can't: UL-UNAS needs the `onnx` feature and the installed pack.
    pub fn unavailable(&self) -> Option<String> {
        match self.model {
            Some(Model::Ulunas) => ulunas_unavailable(),
            _ => None,
        }
    }
}

/// A chain to run, and the counter its noise model's hops add their time to, in ns (`None` with no noise stage running).
pub struct Built {
    pub chain: Chain,
    pub model_ns: Option<Arc<AtomicU64>>,
}

/// The chain for `cfg`, built and activated on this thread as the engine builds it, its model inline as the offline tools run
/// it (the same blocks behind as on the engine's worker, so the same output), and AEC3's stage dithering its reference at
/// `aec_dither` ([`nf_stages::AEC3_REFERENCE_DITHER`] is the app's; 0 for the matrix's "bare" policy). Without AEC3 or a noise
/// stage running it is the engine's own (`nf_engine::models::build_chain`: RNNoise's stage in the noise slot, off). Otherwise
/// it is what `nf_stages::build_model_chain` builds, written out so AEC3's dither can be set: `Chain::with_noise_model` with
/// the AEC3 stage and the noise stage `models.rs` builds (its model wrapped in [`Timed`]), then
/// `activate`. `the_timed_chains_are_the_engine_s` checks the output is the engine's bit for bit at the app's dither.
pub fn build(kind: &ChainKind, cfg: &ProcessingConfig, aec_dither: f32) -> Result<Built, String> {
    let controls = Arc::new(ChainControls::new(cfg));
    if !kind.aec && kind.model.is_none() {
        let b = build_chain(controls, cfg, cfg.aec_enabled, ModelThread::Inline).map_err(|e| e.0)?;
        return Ok(Built { chain: b.chain, model_ns: None });
    }
    let ns = Arc::new(AtomicU64::new(0));
    let noise = match kind.model {
        // The noise filter off: the engine's noise slot (`models.rs`' `noise_stage` for the default model), off.
        None => Box::new(nf_stages::rnnoise_stage(controls.clone()).map_err(|e| e.0)?),
        Some(Model::Rnnoise) => {
            Box::new(NoiseStage::new(controls.clone(), Box::new(Timed::new(nf_stages::RnnoiseModel::new(), ns.clone()))).map_err(|e| e.0)?)
        }
        Some(Model::Ulunas) => Box::new(ulunas_stage(controls.clone(), cfg, ns.clone()).map_err(|e| e.0)?),
    };
    let aec: Option<Box<dyn Stage>> = if cfg.aec_enabled {
        Some(Box::new(Aec3Stage::with_reference_dither(controls.clone(), aec_dither).map_err(|e| e.0)?))
    } else {
        None
    };
    let mut chain = Chain::with_noise_model(controls, aec, Some(noise));
    chain.activate().map_err(|e| e.0)?;
    Ok(Built { chain, model_ns: kind.model.map(|_| ns) })
}

/// `nf_ort::noise_stage` for the installed pack, inline, at the depth `models.rs` gives it offline, with the model timed.
#[cfg(feature = "onnx")]
fn ulunas_stage(controls: Arc<ChainControls>, cfg: &ProcessingConfig, ns: Arc<AtomicU64>) -> Result<NoiseStage, StageError> {
    use nf_core::config::MicDelivery;
    use nf_core::noise::{Executor, Pipelined};
    let pack = nf_ort::Pack::installed(ULUNAS).map_err(|e| StageError(format!("noise model '{ULUNAS}': {}", e.0)))?;
    let make = nf_ort::model_maker(pack, controls.clone(), nf_ort::SessionOptions::default());
    let p = Pipelined::spawn(make, Executor::Inline, cfg.model_depth_for(MicDelivery::Unknown))?;
    NoiseStage::new(controls, Box::new(Timed::new(p, ns)))
}

#[cfg(not(feature = "onnx"))]
fn ulunas_stage(_controls: Arc<ChainControls>, _cfg: &ProcessingConfig, _ns: Arc<AtomicU64>) -> Result<NoiseStage, StageError> {
    Err(StageError(ulunas_unavailable().unwrap_or_default()))
}

#[cfg(feature = "onnx")]
fn ulunas_unavailable() -> Option<String> {
    nf_ort::Pack::installed(ULUNAS).err().map(|e| e.0)
}

#[cfg(not(feature = "onnx"))]
fn ulunas_unavailable() -> Option<String> {
    Some(format!("noise model '{ULUNAS}' needs a build with --features onnx"))
}

/// A noise model whose hops add their time (ns) to a counter the harness reads and clears each block: two clock reads a hop
/// (tens of ns), and otherwise the model itself.
pub struct Timed<M> {
    inner: M,
    ns: Arc<AtomicU64>,
}

impl<M: NoiseModel> Timed<M> {
    pub fn new(inner: M, ns: Arc<AtomicU64>) -> Self {
        Self { inner, ns }
    }
}

impl<M: NoiseModel> NoiseModel for Timed<M> {
    fn info(&self) -> &ModelInfo {
        self.inner.info()
    }

    fn prepare(&mut self) -> Result<(), StageError> {
        self.inner.prepare()
    }

    fn reset(&mut self) {
        self.inner.reset()
    }

    #[inline]
    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let t = Instant::now();
        let vad = self.inner.process_hop(input, output);
        self.ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        vad
    }

    fn take_report(&mut self) -> HopReport {
        self.inner.take_report()
    }
}
