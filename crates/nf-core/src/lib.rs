//! noise-filter's core: everything that doesn't touch Windows or a device.
//!
//! The app's internal format is fixed: [`SAMPLE_RATE`] (48 kHz), mono, `f32` in ±1, processed in
//! [`BLOCK`]-frame (10 ms) blocks. Devices are converted to it at the edge (`nf-engine`).
//!
//! Real-time rule (CLAUDE.md): anything called per block from the audio thread must not allocate, lock, log or make a
//! system call, and is O(1) per block: [`chain::Chain::process`]; a stage's `process` and `reset`, its extension getters
//! and values, `param_value` and `set_param` ([`stage`]); a [`noise::NoiseModel`]'s `process_hop` and `reset`; the `dsp`
//! types' `process` and `reset`. Construction, `activate` and state save and load may allocate. One exception, known and
//! bounded: a model's `reset` once its spare state is spent (RNNoise's second real reset before the next `activate`
//! rebuilds its state in place, ten allocations, which the engine's real-time pool serves; [`noise::NoiseModel::reset`]).
//! And CLAUDE.md's one planned exception: a model inside [`noise::Pipelined`] runs on its own inference worker thread, where
//! per-run allocations are allowed and measured (ONNX Runtime's, TODO 6); the audio thread's side of it allocates nothing
//! and makes one non-blocking system call a block, the worker's wake-up.
//!
//! The stage interface is CLAP-shaped (TODO 17, [`stage`]); the noise stage runs a model behind it ([`noise`]).

pub mod atomic;
pub mod chain;
pub mod config;
pub mod controls;
pub mod dsp;
pub mod latency;
pub mod noise;
pub mod rt;
pub mod stage;
pub mod telemetry;

/// The app's one sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Frames per processing block: 10 ms at [`SAMPLE_RATE`], the AEC3 frame and the default WASAPI period.
pub const BLOCK: usize = 480;
/// One mono block.
pub type Block = [f32; BLOCK];

/// Converts decibels to a linear amplitude factor.
#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db * 0.05)
}

/// Converts a linear amplitude to decibels, floored at -200 dB for zero.
#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= 1e-10 { -200.0 } else { 20.0 * lin.log10() }
}

/// Milliseconds to samples at [`SAMPLE_RATE`].
#[inline]
pub fn ms_to_samples(ms: f32) -> f32 {
    ms * (SAMPLE_RATE as f32 / 1000.0)
}
