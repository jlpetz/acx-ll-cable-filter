//! nf-stages: the stages v0 ships (TODO 4, 10), on nf-core's CLAP-shaped stage interface (TODO 17).
//! - [`rnnoise_stage`]: nf-core's noise stage (`NoiseStage`: the floor mix and the gate after the model, as params) around
//!   [`RnnoiseModel`], RNNoise through nnnoiseless.
//! - [`Aec3Stage`]: WebRTC AEC3 through sonora, at 48 kHz (`Rate48kHz`), with the reference dithered, on a non-main input.
//! - [`build_chain`]: the v0 chain, built and activated: what the engine runs and the offline tools rerun;
//!   [`build_chain_with`] the same around another noise stage (a model pack's, nf-ort, through nf-engine's `models.rs`).
//! - [`harness`]: TODO 2's offline harness: run a stage or the chain block by block as the engine does, measure stream
//!   offsets and parity, and the test scenes.
//! - [`synth`]: deterministic test signals for the harness (`tests/`).
//! - [`bench`](mod@bench): the offline per-block timing that `tests/timing.rs` runs and
//!   `noise-filter bench` prints.
//!
//! Each stage's module notes cover its scale, latency, allocation and reset.

use std::sync::Arc;

use nf_core::chain::Chain;
use nf_core::controls::ChainControls;
use nf_core::stage::{Stage, StageError};

mod aec3;
pub mod bench;
pub mod harness;
mod rnnoise;
pub mod synth;

pub use aec3::{AEC3_DESCRIPTOR, AEC3_LATENCY, AEC3_REFERENCE_DITHER, Aec3Stage};
pub use rnnoise::{MAKER_POLL, RNNOISE_INFO, RNNOISE_LATENCY, RnnoiseModel, SPARES, rnnoise_stage};

/// Whether sonora's FTZ/DAZ guard is built in (this crate's `sonora-denormal-guard` feature, TODO 10), for a harness in a crate
/// that can't see this crate's features (nf-engine's `tests/dither_matrix`).
pub const SONORA_DENORMAL_GUARD: bool = cfg!(feature = "sonora-denormal-guard");

/// Builds this thread's easyfft state (FFT planners and scratch, which easyfft leaks per thread by
/// design) so it lands on the system heap rather than the locked pool. Call it on the audio thread
/// *before* `nf_mem::RtScope::enter()`; the noise stage's `activate` still warms up, but then finds the
/// state built and allocates only a throwaway model state (review RT-1).
pub fn warm_up_fft_thread() {
    rnnoise::warm_up_thread();
}

/// Builds the v0 chain (AEC3 if `with_aec`, then the noise stage around RNNoise) and activates it.
///
/// The engine builds its chain with this too, so the live chain and an offline rerun of a chain recording can't drift
/// apart (engine.md, "The chain recorder").
///
/// Allocates: call it before real-time, on the thread that will run the chain. Activation warms up
/// easyfft's per-thread FFT state there, so `Chain::process` then doesn't allocate on that thread,
/// with AEC3 in it or not (see [`Aec3Stage`]).
pub fn build_chain(controls: Arc<ChainControls>, with_aec: bool) -> Result<Chain, StageError> {
    let noise: Box<dyn Stage> = Box::new(rnnoise_stage(controls.clone())?);
    build_chain_with(controls, with_aec, noise)
}

/// [`build_chain`] around `noise` (built on `controls`) in the noise slot: AEC3 first if `with_aec`, then the chain is
/// activated on the calling thread. nf-engine's `models::build_chain` is the one caller that picks the noise stage by the
/// config's model.
pub fn build_chain_with(controls: Arc<ChainControls>, with_aec: bool, noise: Box<dyn Stage>) -> Result<Chain, StageError> {
    let aec: Option<Box<dyn Stage>> = if with_aec { Some(Box::new(Aec3Stage::new(controls.clone())?)) } else { None };
    let mut chain = Chain::new(controls, aec, Some(noise));
    chain.activate()?;
    Ok(chain)
}
