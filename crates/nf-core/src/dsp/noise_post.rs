//! What every noise stage does after its model: the floor mix, then the gate (TODO #11).
//! Shared so each future model (UL-UNAS, FastEnhancer, ...) gets the same floor and gate behaviour.
//!
//! Contract:
//! - `new(model_latency)`: a `DelayLine` sized for at least `model_latency` (allow up to 4096 so
//!   future models fit), set to `model_latency`.
//! - `process(dry, wet, controls, speech_probability)`:
//!   1. delay `dry` by the model latency (into an internal block);
//!   2. `FloorMix` with `controls.floor_db`;
//!   3. `Gate` with `controls.gate.load()` and `speech_probability`.
//!
//!   Return a `StageReport` with `speech_probability` and `gain_reduction_db` = what actually left
//!   the stage against what went in: `rms_db(output) - rms_db(dry_aligned)`, measured after the
//!   floor mix and the gate, so the floor's cap and the gate's gain are both in it. Clamped to
//!   [-120, 0]; silence on both sides, output above input, or a NaN level reads as 0 dB.
//! - `reset()` clears all three.

use crate::chain::MAX_STAGE_LATENCY;
use crate::controls::NoiseControls;
use crate::dsp::delay::DelayLine;
use crate::dsp::gate::Gate;
use crate::dsp::meter::mean_square;
use crate::dsp::mix::FloorMix;
use crate::stage::StageReport;
use crate::{BLOCK, Block, lin_to_db};

/// Floor mix plus gate, after a noise model.
#[derive(Clone, Debug)]
pub struct NoisePost {
    /// Delays the stage input by the model's latency, so dry lines up with the model's output.
    delay: DelayLine,
    mix: FloorMix,
    gate: Gate,
    dry_aligned: Box<Block>,
}

impl NoisePost {
    /// `model_latency`: the model's stream offset in samples (RNNoise: 480).
    pub fn new(model_latency: u32) -> Self {
        let latency = model_latency as usize;
        Self {
            delay: DelayLine::new(latency.max(MAX_STAGE_LATENCY), latency),
            mix: FloorMix::new(),
            gate: Gate::new(),
            dry_aligned: Box::new([0.0; BLOCK]),
        }
    }

    /// `dry`: this block's stage input. `wet`: the model's output, modified in place.
    pub fn process(&mut self, dry: &Block, wet: &mut Block, controls: &NoiseControls, speech_probability: f32) -> StageReport {
        self.delay.process(dry, &mut self.dry_aligned);
        let dry_db = lin_to_db(mean_square(&self.dry_aligned).sqrt());
        self.mix.process(&self.dry_aligned, wet, controls.floor_db.load());
        self.gate.process(wet, &controls.gate.load(), speech_probability);
        // Measured on the output, so the gate's gain is already in it. Silence on both sides reads
        // as no reduction (-200 - -200); output above input, or a NaN level, reads as 0 dB.
        let out = lin_to_db(mean_square(wet).sqrt()) - dry_db;
        let reduction = if out < 0.0 { out.max(-120.0) } else { 0.0 };
        StageReport { speech_probability, gain_reduction_db: reduction }
    }

    pub fn reset(&mut self) {
        self.delay.reset();
        self.mix.reset();
        self.gate.reset();
        self.dry_aligned.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GateConfig, GateMode, ProcessingConfig};
    use crate::controls::ChainControls;
    use crate::db_to_lin;

    fn controls(floor_db: f32, gate: GateConfig) -> ChainControls {
        let cfg = ProcessingConfig { floor_db, gate, ..ProcessingConfig::default() };
        ChainControls::new(&cfg)
    }

    fn tone(blk: usize) -> Block {
        std::array::from_fn(|i| 0.3 * ((blk * BLOCK + i) as f32 * 0.05).sin())
    }

    /// An identity "model" with `lat` samples of latency and a fixed gain.
    struct Model {
        line: DelayLine,
        gain: f32,
    }
    impl Model {
        fn new(lat: usize, gain: f32) -> Self {
            Self { line: DelayLine::new(lat, lat), gain }
        }
        fn run(&mut self, x: &Block) -> Block {
            let mut y = [0.0; BLOCK];
            self.line.process(x, &mut y);
            y.iter_mut().for_each(|v| *v *= self.gain);
            y
        }
    }

    #[test]
    fn dry_is_aligned_with_the_model_output() {
        // Model output silent, floor 0 dB: the output is the dry path alone, delayed by the latency.
        let c = controls(0.0, GateConfig::default());
        for lat in [0u32, 1, 480, 1000] {
            let mut post = NoisePost::new(lat);
            let mut out = Vec::new();
            for blk in 0..4 {
                let mut x = [0.0f32; BLOCK];
                if blk == 1 {
                    x[10] = 1.0;
                }
                let mut wet = [0.0f32; BLOCK];
                post.process(&x, &mut wet, &c.noise, f32::NAN);
                out.extend_from_slice(&wet);
            }
            let idx = out.iter().position(|&v| v != 0.0).expect("impulse comes through");
            assert_eq!(idx, BLOCK + 10 + lat as usize, "latency {lat}");
            assert_eq!(out[idx], 1.0);
        }
    }

    #[test]
    fn identity_model_passes_unchanged_and_reports_no_reduction() {
        let c = controls(-20.0, GateConfig::default()); // gate Off by default
        let mut post = NoisePost::new(480);
        let mut model = Model::new(480, 1.0);
        for blk in 0..6 {
            let x = tone(blk);
            let mut wet = model.run(&x);
            let expect = wet;
            let r = post.process(&x, &mut wet, &c.noise, 0.8);
            assert_eq!(r.speech_probability, 0.8);
            if blk >= 1 {
                // dry_aligned == wet, so the mix leaves it (to rounding) and nothing is reduced.
                for (a, b) in wet.iter().zip(&expect) {
                    assert!((a - b).abs() < 1e-6);
                }
                assert!(r.gain_reduction_db.abs() < 1e-3, "{}", r.gain_reduction_db);
            }
        }
    }

    #[test]
    fn suppression_and_floor_are_reported_and_mixed() {
        // The model cuts 20 dB; a -6 dB floor mixes the aligned dry back in, so what leaves the
        // stage is only 20 log10(0.501 + 0.499 * 0.1) = -5.2 dB down, and that is what's reported.
        let c = controls(-6.0, GateConfig::default());
        let mut post = NoisePost::new(480);
        let mut model = Model::new(480, 0.1);
        let lim = db_to_lin(-6.0);
        let mut aligned = DelayLine::new(480, 480);
        for blk in 0..5 {
            let x = tone(blk);
            let mut dry = [0.0f32; BLOCK];
            aligned.process(&x, &mut dry);
            let mut wet = model.run(&x);
            let before = wet;
            let r = post.process(&x, &mut wet, &c.noise, f32::NAN);
            if blk >= 1 {
                for i in 0..BLOCK {
                    assert_eq!(wet[i], lim * dry[i] + (1.0 - lim) * before[i]);
                }
                let expect = 20.0 * (lim + (1.0 - lim) * 0.1).log10();
                assert!((expect - -5.18).abs() < 0.01);
                assert!((r.gain_reduction_db - expect).abs() < 0.01, "{} vs {expect}", r.gain_reduction_db);
            }
        }
    }

    #[test]
    fn gate_reduction_is_in_the_measurement_and_the_total_is_clamped() {
        let gate = GateConfig { mode: GateMode::Static, threshold_db: -10.0, range_db: -40.0, release_ms: 1.0, ..GateConfig::default() };
        let c = controls(-200.0, gate);
        let mut post = NoisePost::new(0);
        let mut r = StageReport::default();
        for blk in 0..20 {
            let x = tone(blk);
            let mut wet = x.map(|v| v * 0.1); // -20 dB of "suppression", then gated closed
            r = post.process(&x, &mut wet, &c.noise, f32::NAN);
        }
        assert!((r.gain_reduction_db - -60.0).abs() < 0.05, "{}", r.gain_reduction_db);

        // A silent model output against a live input: far beyond -120, clamped.
        let mut post = NoisePost::new(0);
        let x = tone(0);
        let mut wet = [0.0f32; BLOCK];
        let r = post.process(&x, &mut wet, &c.noise, f32::NAN);
        assert_eq!(r.gain_reduction_db, -120.0);
        // The same silent model output with a -10 dB floor: the floor caps it at -10 dB (review
        // DSP-2: the old estimate said -120 here, and the GUI showed "-inf").
        let c10 = controls(-10.0, GateConfig::default());
        let mut post = NoisePost::new(0);
        let mut wet = [0.0f32; BLOCK];
        let r = post.process(&x, &mut wet, &c10.noise, f32::NAN);
        assert!((r.gain_reduction_db - -10.0).abs() < 0.01, "{}", r.gain_reduction_db);
        // Silence in, silence out: no reduction; a NaN in the signal: 0 dB, not NaN.
        let mut post = NoisePost::new(0);
        assert_eq!(post.process(&[0.0; BLOCK], &mut [0.0; BLOCK], &c.noise, f32::NAN).gain_reduction_db, 0.0);
        let mut wet = x;
        wet[3] = f32::NAN;
        assert_eq!(post.process(&x, &mut wet, &c.noise, f32::NAN).gain_reduction_db, 0.0);
        // Model louder than its input: reads as no reduction, never a gain.
        let c = controls(-200.0, GateConfig::default());
        let mut post = NoisePost::new(0);
        let mut wet = x.map(|v| v * 2.0);
        assert_eq!(post.process(&x, &mut wet, &c.noise, f32::NAN).gain_reduction_db, 0.0);
    }

    #[test]
    fn reset_clears_the_dry_history() {
        let c = controls(0.0, GateConfig::default());
        let mut post = NoisePost::new(480);
        post.process(&[1.0; BLOCK], &mut [0.0; BLOCK], &c.noise, f32::NAN);
        post.reset();
        let mut wet = [0.0f32; BLOCK];
        post.process(&[0.0; BLOCK], &mut wet, &c.noise, f32::NAN);
        assert!(wet.iter().all(|&v| v == 0.0), "no stale dry after reset");
    }
}
