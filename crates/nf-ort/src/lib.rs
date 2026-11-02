//! nf-ort: model packs through ONNX Runtime (TODO 3, 5, 6). The only crate that links ONNX Runtime (1.31.0, pyke's static
//! build through ort `=2.0.0-rc.15`); nf-engine and nf-app take it only with their `onnx` feature, off by default.
//!
//! - [`pack`]: the manifest (`doc/spec/model-pack-v0.md`), its checks, and the model's verified bytes.
//! - [`env`]: the process's one ONNX Runtime environment.
//! - [`session`]: a pack's session with its I/O preallocated.
//! - [`spectral`]: [`OrtSpectralModel`], a spectral pack on the bin-mapped 48 kHz STFT as a `NoiseModel`, and
//!   [`NativeSpectral`], the same at the pack's own rate (offline: parity and conformance).
//! - [`conformance`]: the conformance pair, framed as its reference was.
//! - [`heap_probe`]: TODO 6's count of ONNX Runtime's heap calls, per thread (tests and `bench` only).
//! - [`bench`]: the per-hop and per-block timing `noise-filter bench --model` prints and the ignored timing test runs.
//! - [`metric`]: the quality metrics (DNSMOS P.835, SIGMOS) as packs of role `metric`, run offline by `noise-filter lab`
//!   (TODO 14), and their 48-to-16 kHz resampler.
//!
//! [`noise_stage`] puts them together: the noise stage around a pack, its model on an inference worker (live) or inline
//! (offline), `depth` blocks behind either way (`nf_core::noise::Pipelined`). The pack is loaded and hashed where the model
//! is built (on the worker, live: not on the audio thread, and not in its real-time pool).
//!
//! UL-UNAS, the first pack, is for evaluation only: RNNoise stays the default, and shipping waits on the owner's answer on
//! the patent question (TODO 5).

use std::sync::Arc;

use nf_core::controls::ChainControls;
use nf_core::noise::{Executor, InferenceStats, MakeModel, NoiseModel, NoiseStage, Pipelined};
use nf_core::stage::StageError;

pub mod bench;
pub mod conformance;
pub mod env;
pub mod heap_probe;
pub mod metric;
pub mod onnx_check;
pub mod pack;
pub mod session;
pub mod spectral;

pub use pack::{Pack, PackError, models_dir};
pub use session::{ModelSession, SessionOptions};
pub use spectral::{NativeSpectral, OrtSpectralModel, Spectral};

/// The noise stage around the installed pack `id` (from [`models_dir`]), its model `depth` blocks behind: the pack is
/// loaded, checked and its session built, prepared and warmed up where `exec` runs it (a worker's own thread, or here
/// inline). Returns the stage and its model's [`InferenceStats`]. A failure names the model.
pub fn noise_stage(
    controls: Arc<ChainControls>, id: &str, exec: Executor, depth: u32,
) -> Result<(NoiseStage, Arc<InferenceStats>), StageError> {
    let (c, owned) = (controls.clone(), id.to_owned());
    let make: MakeModel = Box::new(move || {
        let pack = Pack::installed(&owned).map_err(|e| StageError(e.0))?;
        build_model(&pack, c, SessionOptions::default())
    });
    stage_around(controls, make, exec, depth).map_err(|e| StageError(format!("noise model '{id}': {}", e.0)))
}

/// [`noise_stage`] for a loaded `pack`, with `opts` (tests and the bench).
pub fn pack_stage(
    controls: Arc<ChainControls>, pack: Pack, exec: Executor, opts: SessionOptions, depth: u32,
) -> Result<(NoiseStage, Arc<InferenceStats>), StageError> {
    stage_around(controls.clone(), model_maker(pack, controls, opts), exec, depth)
}

fn stage_around(
    controls: Arc<ChainControls>, make: MakeModel, exec: Executor, depth: u32,
) -> Result<(NoiseStage, Arc<InferenceStats>), StageError> {
    let p = Pipelined::spawn(make, exec, depth)?;
    let stats = p.stats();
    Ok((NoiseStage::new(controls, Box::new(p))?, stats))
}

/// What builds a loaded pack's model, wherever it runs (`Pipelined::spawn`'s `make`).
pub fn model_maker(pack: Pack, controls: Arc<ChainControls>, opts: SessionOptions) -> MakeModel {
    Box::new(move || build_model(&pack, controls, opts))
}

fn build_model(pack: &Pack, controls: Arc<ChainControls>, opts: SessionOptions) -> Result<Box<dyn NoiseModel>, StageError> {
    let m = OrtSpectralModel::new(pack, controls, opts).map_err(StageError)?;
    log::info!(
        "noise model {}: {} ({} Hz pack, bin-mapped ×{}), ONNX Runtime 1.31",
        pack.manifest.id,
        pack.manifest.name,
        pack.manifest.signal.sample_rate,
        pack.rate_factor()
    );
    Ok(Box::new(m))
}
