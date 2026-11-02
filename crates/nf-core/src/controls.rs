//! Live controls: atomics the control or UI thread writes and the audio thread reads once per block.
//!
//! Each stage holds an `Arc` to its controls and loads a plain `Copy` snapshot at the start of `process`. Relaxed loads are
//! enough: each value stands alone, and one block of skew between two of them is harmless. They are where the values of the
//! noise stage's CLAP params live (`noise::NoiseStage`, TODO 17): its params extension declares ids, ranges and flags over
//! these same atomics, and [`ChainControls::apply`] still writes them whole (without the params' clamping).

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8};

use crate::atomic::AtomicF32;
use crate::config::{GateConfig, GateMode, HighBand, ProcessingConfig};

/// Gate settings, readable per block as [`GateParams`]. The fields are the crate's, so the noise stage's params can read
/// and write them one by one.
#[derive(Debug)]
pub struct GateControls {
    /// [`GateMode::to_u8`].
    pub(crate) mode: AtomicU8,
    pub(crate) threshold_db: AtomicF32,
    pub(crate) auto_margin_db: AtomicF32,
    pub(crate) attack_ms: AtomicF32,
    pub(crate) release_ms: AtomicF32,
    pub(crate) hold_ms: AtomicF32,
    pub(crate) range_db: AtomicF32,
    pub(crate) use_vad: AtomicBool,
}

/// One block's view of [`GateControls`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
    /// [`HighBand::to_u8`]: what a band-limited model puts above its band, read by the model every hop.
    pub high_band: AtomicU8,
}

impl NoiseControls {
    #[inline]
    pub fn high_band(&self) -> HighBand {
        HighBand::from_u8(self.high_band.load(Relaxed))
    }
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
                high_band: AtomicU8::new(0),
            },
            meters_enabled: AtomicBool::new(true),
            passthrough: AtomicBool::new(false),
        };
        c.apply(cfg);
        c
    }

    /// The settings that change the chain's output, as they stand.
    #[inline]
    pub fn load(&self) -> ChainParams {
        ChainParams {
            aec_enabled: self.aec.enabled.load(Relaxed),
            noise_enabled: self.noise.enabled.load(Relaxed),
            passthrough: self.passthrough.load(Relaxed),
            floor_db: self.noise.floor_db.load(),
            gate: self.noise.gate.load(),
            high_band: self.noise.high_band(),
        }
    }

    /// Applies a whole processing config (the passthrough flag is left alone: the latency test owns it).
    pub fn apply(&self, cfg: &ProcessingConfig) {
        self.aec.enabled.store(cfg.aec_enabled, Relaxed);
        self.noise.enabled.store(cfg.noise_enabled, Relaxed);
        self.noise.floor_db.store(cfg.floor_db.min(0.0));
        self.noise.gate.apply(&cfg.gate);
        self.noise.high_band.store(cfg.high_band.to_u8(), Relaxed);
        self.meters_enabled.store(cfg.meters_enabled, Relaxed);
    }
}

/// One block's view of the [`ChainControls`] that change the chain's output (the meters don't): what the chain and its
/// stages read each block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChainParams {
    pub aec_enabled: bool,
    pub noise_enabled: bool,
    pub passthrough: bool,
    pub floor_db: f32,
    pub gate: GateParams,
    pub high_band: HighBand,
}

impl ChainParams {
    /// The same settings, bit for bit: as `==`, except that a NaN setting (a config can hold one) equals itself.
    pub fn same(&self, other: &Self) -> bool {
        let f = |a: f32, b: f32| a.to_bits() == b.to_bits();
        let (g, h) = (&self.gate, &other.gate);
        (self.aec_enabled, self.noise_enabled, self.passthrough, self.high_band)
            == (other.aec_enabled, other.noise_enabled, other.passthrough, other.high_band)
            && f(self.floor_db, other.floor_db)
            && (g.mode, g.use_vad) == (h.mode, h.use_vad)
            && f(g.threshold_db, h.threshold_db)
            && f(g.auto_margin_db, h.auto_margin_db)
            && f(g.attack_ms, h.attack_ms)
            && f(g.release_ms, h.release_ms)
            && f(g.hold_ms, h.hold_ms)
            && f(g.range_db, h.range_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot holds what the chain reads; a change of any of it shows, and a NaN setting still equals itself.
    #[test]
    fn a_snapshot_shows_each_change() {
        let c = ChainControls::new(&ProcessingConfig::default());
        let a = c.load();
        assert!(a.same(&c.load()));
        c.passthrough.store(true, Relaxed);
        assert!(!a.same(&c.load()) && c.load().passthrough);
        c.passthrough.store(false, Relaxed);
        c.noise.gate.threshold_db.store(-41.0);
        assert!(!a.same(&c.load()));
        c.noise.gate.threshold_db.store(f32::NAN);
        let nan = c.load();
        assert!(nan.same(&c.load()) && nan != c.load());
        c.meters_enabled.store(false, Relaxed);
        assert!(nan.same(&c.load()), "the meters don't change the output");
        c.apply(&ProcessingConfig { high_band: HighBand::Pass, ..ProcessingConfig::default() });
        assert_eq!((c.noise.high_band(), c.load().high_band), (HighBand::Pass, HighBand::Pass));
        assert!(!a.same(&c.load()));
    }
}
