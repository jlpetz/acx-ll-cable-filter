//! Live controls: atomics the control or UI thread writes and the audio thread reads once per block.
//!
//! These play the part CLAP parameters would: each stage holds an `Arc` to its controls and loads a
//! plain `Copy` snapshot at the start of `process`. Relaxed loads are enough: each value stands
//! alone, and one block of skew between two of them is harmless.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

use crate::atomic::AtomicF32;
use crate::config::{GateConfig, GateMode, ProcessingConfig};

/// Gate settings, readable per block as [`GateParams`].
#[derive(Debug)]
pub struct GateControls {
    mode: AtomicU8,
    threshold_db: AtomicF32,
    auto_margin_db: AtomicF32,
    attack_ms: AtomicF32,
    release_ms: AtomicF32,
    hold_ms: AtomicF32,
    range_db: AtomicF32,
    use_vad: AtomicBool,
}

/// One block's view of [`GateControls`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateParams {
    pub mode: GateMode,
    pub threshold_db: f32,
    pub auto_margin_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub hold_ms: f32,
    pub range_db: f32,
    pub use_vad: bool,
}

impl GateControls {
    pub fn new(cfg: &GateConfig) -> Self {
        let c = Self {
            mode: AtomicU8::new(0),
            threshold_db: AtomicF32::new(0.0),
            auto_margin_db: AtomicF32::new(0.0),
            attack_ms: AtomicF32::new(0.0),
            release_ms: AtomicF32::new(0.0),
            hold_ms: AtomicF32::new(0.0),
            range_db: AtomicF32::new(0.0),
            use_vad: AtomicBool::new(false),
        };
        c.apply(cfg);
        c
    }

    pub fn apply(&self, cfg: &GateConfig) {
        self.mode.store(cfg.mode.to_u8(), Relaxed);
        self.threshold_db.store(cfg.threshold_db);
        self.auto_margin_db.store(cfg.auto_margin_db);
        self.attack_ms.store(cfg.attack_ms.max(0.0));
        self.release_ms.store(cfg.release_ms.max(0.0));
        self.hold_ms.store(cfg.hold_ms.max(0.0));
        self.range_db.store(cfg.range_db.min(0.0));
        self.use_vad.store(cfg.use_vad, Relaxed);
    }

    #[inline]
    pub fn load(&self) -> GateParams {
        GateParams {
            mode: GateMode::from_u8(self.mode.load(Relaxed)),
            threshold_db: self.threshold_db.load(),
            auto_margin_db: self.auto_margin_db.load(),
            attack_ms: self.attack_ms.load(),
            release_ms: self.release_ms.load(),
            hold_ms: self.hold_ms.load(),
            range_db: self.range_db.load(),
            use_vad: self.use_vad.load(Relaxed),
        }
    }
}

/// The noise stage's controls: on/off, the attenuation floor and its gate.
#[derive(Debug)]
pub struct NoiseControls {
    pub enabled: AtomicBool,
    /// Maximum attenuation in dB (negative); see [`crate::dsp::mix::FloorMix`].
    pub floor_db: AtomicF32,
    pub gate: GateControls,
}

/// The echo canceller's controls.
#[derive(Debug)]
pub struct AecControls {
    pub enabled: AtomicBool,
}

/// Everything the chain reads per block. Shared as `Arc<ChainControls>`.
#[derive(Debug)]
pub struct ChainControls {
    pub aec: AecControls,
    pub noise: NoiseControls,
    pub meters_enabled: AtomicBool,
    /// Measurement passthrough: each enabled stage is replaced by a delay line of its latency, so a
    /// probe survives (no denoiser removes it, no AEC cancels it) and the delay stays the live one.
    pub passthrough: AtomicBool,
}

impl ChainControls {
    pub fn new(cfg: &ProcessingConfig) -> Self {
        let c = Self {
            aec: AecControls { enabled: AtomicBool::new(false) },
            noise: NoiseControls {
                enabled: AtomicBool::new(false),
                floor_db: AtomicF32::new(0.0),
                gate: GateControls::new(&cfg.gate),
            },
            meters_enabled: AtomicBool::new(true),
            passthrough: AtomicBool::new(false),
        };
        c.apply(cfg);
        c
    }

    /// Applies a whole processing config (the passthrough flag is left alone: the latency test owns it).
    pub fn apply(&self, cfg: &ProcessingConfig) {
        self.aec.enabled.store(cfg.aec_enabled, Relaxed);
        self.noise.enabled.store(cfg.noise_enabled, Relaxed);
        self.noise.floor_db.store(cfg.floor_db.min(0.0));
        self.noise.gate.apply(&cfg.gate);
        self.meters_enabled.store(cfg.meters_enabled, Relaxed);
    }
}
