//! Telemetry the audio thread publishes and the UI reads, through atomics only.
//!
//! The audio thread writes with relaxed stores (one block of skew between fields is harmless), and
//! the UI takes a [`TelemetrySnapshot`] about 20 times a second.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};

use crate::atomic::AtomicF32;

/// Engine run state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Stopped,
    Starting,
    Running,
    /// Stopped by an error; the engine's status holds the message.
    Error,
}

impl EngineState {
    fn to_u8(self) -> u8 {
        match self {
            EngineState::Stopped => 0,
            EngineState::Starting => 1,
            EngineState::Running => 2,
            EngineState::Error => 3,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => EngineState::Starting,
            2 => EngineState::Running,
            3 => EngineState::Error,
            _ => EngineState::Stopped,
        }
    }
}

/// One meter's atomics.
#[derive(Debug, Default)]
pub struct MeterCell {
    pub peak_dbfs: AtomicF32,
    pub rms_dbfs: AtomicF32,
}

/// A meter reading in dBFS.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterSnapshot {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
}

impl MeterCell {
    pub fn store(&self, peak_dbfs: f32, rms_dbfs: f32) {
        self.peak_dbfs.store(peak_dbfs);
        self.rms_dbfs.store(rms_dbfs);
    }
    pub fn load(&self) -> MeterSnapshot {
        MeterSnapshot { peak_dbfs: self.peak_dbfs.load(), rms_dbfs: self.rms_dbfs.load() }
    }
}

/// The shared telemetry block. One per engine; `Arc`-shared with the UI.
#[derive(Debug, Default)]
pub struct Telemetry {
    pub state: AtomicU8,
    /// Blocks processed since start.
    pub blocks: AtomicU64,
    pub input: MeterCell,
    pub output: MeterCell,
    /// Latched when the raw input clips; cleared by [`Telemetry::reset_clip`].
    pub clip_latched: AtomicBool,
    /// The raw input came within 0.1 dB of full scale in the last second.
    pub near_clip: AtomicBool,
    /// What the noise stage took off this block, measured on its output (floor and gate included), in dB (<= 0).
    pub noise_reduction_db: AtomicF32,
    pub speech_probability: AtomicF32,
    pub latency_valid: AtomicBool,
    pub latency_ms_median: AtomicF32,
    pub latency_ms_p95: AtomicF32,
    pub output_drift_ppm: AtomicF32,
    pub reference_drift_ppm: AtomicF32,
    pub output_fill_ms: AtomicF32,
    pub reference_fill_ms: AtomicF32,
    pub output_slips: AtomicU64,
    pub reference_slips: AtomicU64,
    /// Capture packets flagged as discontinuous, or missing by device position.
    pub capture_glitches: AtomicU64,
    /// Times the output cable's buffer ran dry.
    pub render_underruns: AtomicU64,
    /// Blocks where the reference had to be zero-filled.
    pub reference_gaps: AtomicU64,
    /// Whole-chain processing time per block over the last second, in µs.
    pub block_us_p50: AtomicF32,
    pub block_us_p99: AtomicF32,
    pub block_us_max: AtomicF32,
    pub aec_us_p99: AtomicF32,
    pub noise_us_p99: AtomicF32,
    pub aec_delay_ms: AtomicF32,
    pub aec_erle_db: AtomicF32,
    /// Allocations on the audio thread after warm-up that fell back to the system heap (must stay 0).
    pub rt_heap_allocations: AtomicU64,
    /// Allocations on the audio thread in the last second served from the locked pool (O(1) TLSF;
    /// informational: sonora 0.2.0 makes about 43 per block until its PR #40).
    pub rt_pool_allocations_per_s: AtomicU64,
    /// Mic event waits that timed out (100 ms with no packet).
    pub stalls: AtomicU64,
    /// Coarse corrections of the output, or the reference: times whole blocks were dropped to pull
    /// an overfull buffer back to its target (one per event, however many blocks it dropped).
    pub output_coarse_drops: AtomicU64,
    pub reference_coarse_drops: AtomicU64,
    /// Latency-test tap blocks lost because the tap ring was full.
    pub tap_overflows: AtomicU64,
    /// The fill targets the drift loops hold, in ms (0 while stopped or without a reference).
    pub output_target_ms: AtomicF32,
    pub reference_target_ms: AtomicF32,
    pub mic_period_ms: AtomicF32,
    pub output_period_ms: AtomicF32,
}

/// A plain copy of [`Telemetry`] for the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TelemetrySnapshot {
    pub state: EngineState,
    pub blocks: u64,
    pub input: MeterSnapshot,
    pub output: MeterSnapshot,
    pub clip_latched: bool,
    pub near_clip: bool,
    pub noise_reduction_db: f32,
    pub speech_probability: f32,
    pub latency_valid: bool,
    pub latency_ms_median: f32,
    pub latency_ms_p95: f32,
    pub output_drift_ppm: f32,
    pub reference_drift_ppm: f32,
    pub output_fill_ms: f32,
    pub reference_fill_ms: f32,
    pub output_slips: u64,
    pub reference_slips: u64,
    pub capture_glitches: u64,
    pub render_underruns: u64,
    pub reference_gaps: u64,
    pub block_us_p50: f32,
    pub block_us_p99: f32,
    pub block_us_max: f32,
    pub aec_us_p99: f32,
    pub noise_us_p99: f32,
    pub aec_delay_ms: f32,
    pub aec_erle_db: f32,
    pub rt_heap_allocations: u64,
    pub rt_pool_allocations_per_s: u64,
    pub stalls: u64,
    pub output_coarse_drops: u64,
    pub reference_coarse_drops: u64,
    pub tap_overflows: u64,
    pub output_target_ms: f32,
    pub reference_target_ms: f32,
    pub mic_period_ms: f32,
    pub output_period_ms: f32,
}

impl Telemetry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_state(&self, s: EngineState) {
        self.state.store(s.to_u8(), Relaxed);
    }

    pub fn state(&self) -> EngineState {
        EngineState::from_u8(self.state.load(Relaxed))
    }

    /// Clears the clip latch (the UI's "reset" click).
    pub fn reset_clip(&self) {
        self.clip_latched.store(false, Relaxed);
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            state: self.state(),
            blocks: self.blocks.load(Relaxed),
            input: self.input.load(),
            output: self.output.load(),
            clip_latched: self.clip_latched.load(Relaxed),
            near_clip: self.near_clip.load(Relaxed),
            noise_reduction_db: self.noise_reduction_db.load(),
            speech_probability: self.speech_probability.load(),
            latency_valid: self.latency_valid.load(Relaxed),
            latency_ms_median: self.latency_ms_median.load(),
            latency_ms_p95: self.latency_ms_p95.load(),
            output_drift_ppm: self.output_drift_ppm.load(),
            reference_drift_ppm: self.reference_drift_ppm.load(),
            output_fill_ms: self.output_fill_ms.load(),
            reference_fill_ms: self.reference_fill_ms.load(),
            output_slips: self.output_slips.load(Relaxed),
            reference_slips: self.reference_slips.load(Relaxed),
            capture_glitches: self.capture_glitches.load(Relaxed),
            render_underruns: self.render_underruns.load(Relaxed),
            reference_gaps: self.reference_gaps.load(Relaxed),
            block_us_p50: self.block_us_p50.load(),
            block_us_p99: self.block_us_p99.load(),
            block_us_max: self.block_us_max.load(),
            aec_us_p99: self.aec_us_p99.load(),
            noise_us_p99: self.noise_us_p99.load(),
            aec_delay_ms: self.aec_delay_ms.load(),
            aec_erle_db: self.aec_erle_db.load(),
            rt_heap_allocations: self.rt_heap_allocations.load(Relaxed),
            rt_pool_allocations_per_s: self.rt_pool_allocations_per_s.load(Relaxed),
            stalls: self.stalls.load(Relaxed),
            output_coarse_drops: self.output_coarse_drops.load(Relaxed),
            reference_coarse_drops: self.reference_coarse_drops.load(Relaxed),
            tap_overflows: self.tap_overflows.load(Relaxed),
            output_target_ms: self.output_target_ms.load(),
            reference_target_ms: self.reference_target_ms.load(),
            mic_period_ms: self.mic_period_ms.load(),
            output_period_ms: self.output_period_ms.load(),
        }
    }
}
