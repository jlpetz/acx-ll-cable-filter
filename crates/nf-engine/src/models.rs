//! The noise model by the config's name (TODO 5), and the one chain constructor: the engine's audio thread builds its chain
//! here ([`ModelThread::Worker`]), and so do `process`, `quality`'s offline reruns and `bench` ([`ModelThread::Inline`]), so the
//! live chain and an offline rerun can't drift apart (engine.md, "The chain recorder").
//!
//! - `"rnnoise"` (the default): today's stage, `nf_stages::rnnoise_stage`.
//! - Any other id is a model pack in `%LOCALAPPDATA%\noise-filter\models\<id>` (or `NF_MODELS`), in a build with the `onnx`
//!   feature (nf-ort; ONNX Runtime). Without it: "noise model 'X' needs a build with --features onnx". A missing pack says
//!   how to fetch it. A pack that fails to load fails the start: there's no silent fallback to RNNoise, since the user asked
//!   for that model.
//! - A pack's model runs on an inference worker live (`nf_core::noise::Pipelined`, TODO 6), `cfg.model_depth` blocks behind
//!   (automatic by default: the engine picks it at the start from how the mic delivers, `audio::choose_model_depth`, and
//!   builds with that; offline, where nothing delivers, automatic is the bursty depth, 2, as before: `MicDelivery::Unknown`),
//!   entered with [`EngineHooks`]: MMCSS "Pro Audio" at normal priority (its fallback, HIGHEST, under the audio thread's;
//!   a refusal is a start-up warning), then, once the pack is loaded, hashed and its session built and warmed up on the
//!   worker, nf-mem's real-time scope, so whatever its steady state allocates in Rust comes from the locked pool and is
//!   counted. While the worker builds, the audio thread waits for it (at most `READY_TIMEOUT`, 3 s), giving up when the run
//!   is stopped. Offline the model runs inline, the same number of blocks behind, so the output is the same bit for bit
//!   when the worker misses no deadline.
//!
//! A model change, or a pack's depth change, rebuilds the chain: an engine restart (`control::restart_needed`).

use std::any::Any;
use std::sync::Arc;

use nf_core::chain::Chain;
use nf_core::config::{DEFAULT_NOISE_MODEL, ProcessingConfig, is_model_id};
use nf_core::controls::ChainControls;
use nf_core::noise::{InferenceStats, NoiseStage, WorkerCounters, WorkerGuard, WorkerHooks};
use nf_core::stage::{Stage, StageError};

/// Whether the wait for a worker's model should end (the run was stopped).
pub type GiveUp = Arc<dyn Fn() -> bool + Send + Sync>;

/// Where a pack's model runs.
#[derive(Clone)]
pub enum ModelThread {
    /// On the calling thread, the same blocks behind (offline tools).
    Inline,
    /// On an inference worker (the engine's audio thread's chain). `give_up` is read every 100 ms while the worker builds
    /// its model: `true` ends the wait, and the start.
    Worker { give_up: Option<GiveUp> },
}

/// A chain, and its noise model's inference stats when that runs on a worker or inline through a pack.
pub struct BuiltChain {
    pub chain: Chain,
    pub inference: Option<Arc<InferenceStats>>,
}

/// The engine's worker set-up (module notes); its counters are nf-mem's for the thread.
#[derive(Clone, Default)]
pub struct EngineHooks {
    give_up: Option<GiveUp>,
}

impl EngineHooks {
    /// Hooks whose builder gives up waiting when `give_up` says so.
    pub fn new(give_up: Option<GiveUp>) -> Self {
        Self { give_up }
    }
}

impl WorkerHooks for EngineHooks {
    fn enter(&self) -> WorkerGuard {
        let mmcss = crate::sched::Mmcss::enter(crate::sched::MmcssPriority::Worker);
        let refused = mmcss.error;
        WorkerGuard { guard: Box::new(mmcss), refused }
    }

    fn steady(&self) -> Box<dyn Any> {
        Box::new(nf_mem::RtScope::enter())
    }

    fn counters(&self) -> WorkerCounters {
        WorkerCounters { pool_allocs: nf_mem::thread_scope_allocs(), fallbacks: nf_mem::thread_scope_fallbacks(), crt_heap_calls: None }
    }

    fn give_up(&self) -> bool {
        self.give_up.as_ref().is_some_and(|f| f())
    }
}

/// A noise stage, and its model's inference stats when it has them (a pack's).
pub type NoiseStageWithStats = (Box<dyn Stage>, Option<Arc<InferenceStats>>);
type BuiltModel = (Box<NoiseStage>, Option<Arc<InferenceStats>>);

/// Whether this build can run model packs (the `onnx` feature).
pub const ONNX: bool = cfg!(feature = "onnx");

/// Noise-model IDs and display names for this build: the built-in first, then the installed packs in ID order.
/// The catalog reads pack manifests and file metadata. Loading and verifying a selected model stays on the usual start path.
pub fn available_noise_models() -> Vec<(String, String)> {
    let built_in = (DEFAULT_NOISE_MODEL.to_owned(), "RNNoise".to_owned());
    #[cfg(feature = "onnx")]
    if let Some(root) = nf_ort::models_dir() {
        let packs = nf_ort::pack::noise_pack_names(&root);
        return std::iter::once(built_in).chain(packs.into_iter().filter(|(id, _)| id != DEFAULT_NOISE_MODEL)).collect();
    }
    vec![built_in]
}

/// The noise stage `cfg.noise_model` names, on `controls`, and its inference stats (a pack's).
pub fn noise_stage(controls: Arc<ChainControls>, cfg: &ProcessingConfig, thread: ModelThread) -> Result<NoiseStageWithStats, StageError> {
    let (noise, stats) = model_stage(controls, cfg, thread)?;
    Ok((noise, stats))
}

fn model_stage(controls: Arc<ChainControls>, cfg: &ProcessingConfig, thread: ModelThread) -> Result<BuiltModel, StageError> {
    let id = cfg.noise_model.as_str();
    if id == DEFAULT_NOISE_MODEL {
        return Ok((Box::new(nf_stages::rnnoise_stage(controls)?), None));
    }
    if !is_model_id(id) {
        return Err(StageError(format!("'{id}' isn't a noise model id ([a-z0-9][a-z0-9-]{{0,63}})")));
    }
    pack_stage(controls, id, cfg.model_depth_for(nf_core::config::MicDelivery::Unknown), thread)
}

#[cfg(feature = "onnx")]
fn pack_stage(controls: Arc<ChainControls>, id: &str, depth: u32, thread: ModelThread) -> Result<BuiltModel, StageError> {
    use nf_core::noise::Executor;
    let exec = match thread {
        ModelThread::Inline => Executor::Inline,
        ModelThread::Worker { give_up } => Executor::Thread { hooks: Arc::new(EngineHooks::new(give_up)), wait_for_results: false },
    };
    let (stage, stats) = nf_ort::noise_stage(controls, id, exec, depth)?;
    Ok((Box::new(stage), Some(stats)))
}

#[cfg(not(feature = "onnx"))]
fn pack_stage(_controls: Arc<ChainControls>, id: &str, _depth: u32, _thread: ModelThread) -> Result<BuiltModel, StageError> {
    Err(StageError(format!("noise model '{id}' needs a build with --features onnx (this one has RNNoise only)")))
}

/// The chain for `cfg` (AEC3 if `with_aec`, then the noise stage `cfg.noise_model` names), built and activated on the calling
/// thread. The one constructor for the engine and the offline tools (module notes).
pub fn build_chain(
    controls: Arc<ChainControls>, cfg: &ProcessingConfig, with_aec: bool, thread: ModelThread,
) -> Result<BuiltChain, StageError> {
    // Keep the built-in ready for a hot toggle. Disabled model packs need no install, load or inference worker.
    let (noise, inference) = if cfg.noise_enabled || cfg.noise_model == DEFAULT_NOISE_MODEL {
        let (noise, inference) = model_stage(controls.clone(), cfg, thread)?;
        (Some(noise), inference)
    } else {
        (None, None)
    };
    let chain = nf_stages::build_model_chain(controls, with_aec, noise)?;
    Ok(BuiltChain { chain, inference })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rnnoise_is_built_in_and_other_ids_need_a_pack() {
        let cfg = ProcessingConfig::default();
        let c = Arc::new(ChainControls::new(&cfg));
        let b = build_chain(c.clone(), &cfg, false, ModelThread::Inline).unwrap();
        assert_eq!(b.chain.latency(), nf_stages::RNNOISE_LATENCY);
        assert!(b.inference.is_none());
        let bad = ProcessingConfig { noise_model: "../x".into(), ..cfg.clone() };
        assert!(build_chain(c.clone(), &bad, false, ModelThread::Inline).err().unwrap().0.contains("isn't a noise model id"));
        let other = ProcessingConfig { noise_model: "no-such-pack".into(), ..cfg };
        for thread in [ModelThread::Inline, ModelThread::Worker { give_up: None }] {
            let e = build_chain(c.clone(), &other, false, thread).err().unwrap().0;
            if ONNX {
                assert!(e.contains("isn't installed") && e.contains("fetch_models.py no-such-pack"), "{e}");
            } else {
                assert!(e.contains("needs a build with --features onnx"), "{e}");
            }
        }
    }

    #[test]
    fn a_gate_only_chain_can_start_with_an_uninstalled_noise_model() {
        use nf_core::config::{GateConfig, GateMode};
        use nf_core::{BLOCK, Block};
        let cfg = ProcessingConfig {
            noise_enabled: false,
            noise_model: "no-such-pack".into(),
            gate: GateConfig { mode: GateMode::Static, threshold_db: -20.0, range_db: -30.0, release_ms: 0.0, ..GateConfig::default() },
            ..ProcessingConfig::default()
        };
        for thread in [ModelThread::Inline, ModelThread::Worker { give_up: None }] {
            let mut b = build_chain(Arc::new(ChainControls::new(&cfg)), &cfg, false, thread).unwrap();
            assert!(b.inference.is_none());
            assert_eq!(b.chain.latency(), 0);
            assert!(!b.chain.runs_a_stage());
            let mut out: Block = [0.0; BLOCK];
            let report = b.chain.process(&[0.01; BLOCK], &[0.0; BLOCK], &mut out);
            assert!(report.noise.is_none() && (report.gate_reduction_db.unwrap() + 30.0).abs() < 0.001);
        }
    }

    /// The worker's hooks: MMCSS at the worker's priority (a refusal reported, not a failure), the real-time scope from its
    /// steady state on, and the give-up the audio thread passes.
    #[test]
    fn the_engine_s_worker_hooks() {
        std::thread::spawn(|| {
            let h = EngineHooks::default();
            assert!(!h.give_up());
            let entered = h.enter();
            assert_eq!(entered.refused.is_some(), entered.guard.downcast_ref::<crate::sched::Mmcss>().is_some_and(|m| !m.registered()));
            // (This test binary's allocator isn't nf-mem's, so the scope's counting is the app's to show.)
            let scope = h.steady();
            assert!(scope.downcast_ref::<nf_mem::RtScope>().is_some(), "the steady state runs in the real-time scope");
            drop(scope);
            drop(entered);
        })
        .join()
        .unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let h = EngineHooks::new(Some(Arc::new(move || s.load(std::sync::atomic::Ordering::Relaxed))));
        assert!(!h.give_up());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(h.give_up());
    }
}
