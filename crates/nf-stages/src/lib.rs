//! nf-stages: the stages v0 ships (TODO #4, #10).
//! - [`RnnoiseStage`]: RNNoise through nnnoiseless, then nf-core's `NoisePost` (floor mix + gate).
//! - [`Aec3Stage`]: WebRTC AEC3 through sonora, at 48 kHz (`Rate48kHz`), with the reference dithered.
//! - [`build_chain`]: the v0 chain, built and activated.
//! - [`synth`]: deterministic test signals for the offline harness (`tests/`).
//! - [`bench`](mod@bench): the offline per-block timing that `tests/timing.rs` runs and
//!   `noise-filter bench` prints.
//!
//! Each stage's module notes cover its scale, latency, allocation and reset.

use std::sync::Arc;

use nf_core::chain::Chain;
use nf_core::controls::ChainControls;
use nf_core::stage::{Stage, StageError};
use nf_core::{BLOCK, SAMPLE_RATE};

mod aec3;
pub mod bench;
mod rnnoise;
pub mod synth;

pub use aec3::{AEC3_LATENCY, Aec3Stage};
pub use rnnoise::{RNNOISE_LATENCY, RnnoiseStage};

/// Builds this thread's easyfft state (FFT planners and scratch, which easyfft leaks per thread by
/// design) so it lands on the system heap rather than the locked pool. Call it on the audio thread
/// *before* `nf_mem::RtScope::enter()`; `RnnoiseStage::activate` still warms up, but then finds the
/// state built and allocates only a throwaway model state (review RT-1).
pub fn warm_up_fft_thread() {
    rnnoise::warm_up_thread();
}

/// Builds the v0 chain (AEC3 if `with_aec`, then RNNoise) and activates it.
///
/// Allocates: call it before real-time, on the thread that will run the chain. Activation warms up
/// easyfft's per-thread FFT state there, so `Chain::process` then doesn't allocate on that thread
/// (sonora's own per-block allocations aside, see [`Aec3Stage`]).
pub fn build_chain(controls: Arc<ChainControls>, with_aec: bool) -> Result<Chain, StageError> {
    let aec: Option<Box<dyn Stage>> =
        if with_aec { Some(Box::new(Aec3Stage::new(controls.clone())?)) } else { None };
    let noise: Box<dyn Stage> = Box::new(RnnoiseStage::new(controls.clone()));
    let mut chain = Chain::new(controls, aec, Some(noise));
    chain.activate()?;
    Ok(chain)
}

/// Rejects anything but the app's format: both stages are fixed at 48 kHz in 480-frame blocks.
fn check_format(name: &str, sample_rate: u32, block: usize) -> Result<(), StageError> {
    if sample_rate == SAMPLE_RATE && block == BLOCK {
        Ok(())
    } else {
        Err(StageError(format!("{name} runs at {SAMPLE_RATE} Hz in {BLOCK}-frame blocks, not {sample_rate} Hz in {block}")))
    }
}

/// NaN and ±inf become 0; everything else passes. A compare and a mask, so loops over it vectorise.
#[inline(always)]
fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}
