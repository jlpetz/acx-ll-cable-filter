//! The noise floor as a dry/wet mix (TODO #11): DeepFilterNet's attenuation-limit formula.
//!
//! Contract:
//! - `wet = lim * dry + (1 - lim) * wet`, with `lim = db_to_lin(floor_db)`. `floor_db <= -100` means
//!   no floor (`lim = 0`, fast path: leave `wet` untouched).
//! - `dry` must already be delay-aligned with `wet` (`NoisePost` delays it by the model's latency).
//! - When `floor_db` changes, ramp `lim` linearly from the previous block's value across this block;
//!   no step.
//! - Tests: no-floor is bit-exact; a 0 dB floor gives dry; -20 dB gives the exact mix; the ramp is
//!   monotonic.
//!
//! The first block after `new` or `reset` takes its `lim` directly (there is no previous value to
//! ramp from). `floor_db` above 0 is treated as 0 dB, and NaN as no floor.

use crate::{BLOCK, Block, db_to_lin};

/// `floor_db` at or below this means no floor.
const NO_FLOOR_DB: f32 = -100.0;

/// Floor mix state (the last `lim`, for ramping).
#[derive(Clone, Debug)]
pub struct FloorMix {
    /// `lim` at the end of the last block; `None` before the first block.
    lim: Option<f32>,
    /// The last `floor_db` seen and its `lim`, so `powf` runs only when the control moves.
    cached_db: f32,
    cached_lim: f32,
}

impl Default for FloorMix {
    fn default() -> Self {
        Self::new()
    }
}

/// `lim` for a floor in dB: 0 for no floor, at most 1.
fn lim_for(floor_db: f32) -> f32 {
    if floor_db > NO_FLOOR_DB { db_to_lin(floor_db.min(0.0)) } else { 0.0 }
}

impl FloorMix {
    pub fn new() -> Self {
        Self { lim: None, cached_db: NO_FLOOR_DB, cached_lim: 0.0 }
    }

    /// Mixes the aligned dry signal into `wet` in place.
    #[inline]
    pub fn process(&mut self, dry_aligned: &Block, wet: &mut Block, floor_db: f32) {
        // Compare bits so a NaN control caches too instead of recomputing every block.
        if floor_db.to_bits() != self.cached_db.to_bits() {
            self.cached_db = floor_db;
            self.cached_lim = lim_for(floor_db);
        }
        let target = self.cached_lim;
        let start = self.lim.unwrap_or(target);
        self.lim = Some(target);

        if start == target {
            if target == 0.0 {
                return; // no floor: wet untouched, bit-exact
            }
            let keep = 1.0 - target;
            for (w, &d) in wet.iter_mut().zip(dry_aligned) {
                *w = target * d + keep * *w;
            }
        } else {
            // Index-derived ramp (vectorises, no drift); the last sample lands on `target`. The
            // index is converted as i32: a packed `cvtdq2ps`, where usize -> f32 is scalar at x86-64-v2.
            let step = (target - start) * (1.0 / BLOCK as f32);
            for (i, (w, &d)) in wet.iter_mut().zip(dry_aligned).enumerate() {
                let lim = start + (i as i32 + 1) as f32 * step;
                *w = lim * d + (1.0 - lim) * *w;
            }
        }
    }

    pub fn reset(&mut self) {
        self.lim = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> (Block, Block) {
        let dry = std::array::from_fn(|i| (i as f32 * 0.013).sin() * 0.7);
        let wet = std::array::from_fn(|i| (i as f32 * 0.071).cos() * 0.2);
        (dry, wet)
    }

    #[test]
    fn no_floor_is_bit_exact() {
        let (dry, wet0) = signals();
        let mut m = FloorMix::new();
        for db in [-100.0, -150.0, f32::NEG_INFINITY, f32::NAN] {
            let mut wet = wet0;
            m.process(&dry, &mut wet, db);
            assert_eq!(wet.map(f32::to_bits), wet0.map(f32::to_bits), "floor {db}");
        }
    }

    #[test]
    fn zero_db_floor_gives_dry() {
        let (dry, mut wet) = signals();
        FloorMix::new().process(&dry, &mut wet, 0.0);
        assert_eq!(wet, dry);
        // Above 0 dB is treated as 0 dB, not as a negative wet weight.
        let (_, mut wet) = signals();
        FloorMix::new().process(&dry, &mut wet, 6.0);
        assert_eq!(wet, dry);
    }

    #[test]
    fn minus_twenty_db_is_the_exact_mix() {
        let (dry, wet0) = signals();
        let mut wet = wet0;
        let mut m = FloorMix::new();
        m.process(&dry, &mut wet, -20.0);
        let lim = db_to_lin(-20.0);
        assert!((lim - 0.1).abs() < 1e-6);
        for i in 0..BLOCK {
            assert_eq!(wet[i], lim * dry[i] + (1.0 - lim) * wet0[i], "sample {i}");
        }
        // Holding the floor keeps the same exact mix (no ramp).
        let mut again = wet0;
        m.process(&dry, &mut again, -20.0);
        assert_eq!(again, wet);
    }

    #[test]
    fn a_floor_change_ramps_monotonically_from_the_last_value() {
        // dry = 1, wet = 0, so the output is lim itself.
        let dry = [1.0f32; BLOCK];
        let mut m = FloorMix::new();
        let mut out = [0.0f32; BLOCK];
        m.process(&dry, &mut out, -40.0);
        let lim0 = db_to_lin(-40.0);
        assert!(out.iter().all(|&v| (v - lim0).abs() < 1e-7));

        for (db, rising) in [(-6.0, true), (-200.0, false), (-20.0, true)] {
            let prev = out[BLOCK - 1];
            let mut ramp = [0.0f32; BLOCK];
            m.process(&dry, &mut ramp, db);
            let end = lim_for(db);
            assert!((ramp[BLOCK - 1] - end).abs() < 1e-6, "ends at the new lim");
            // The first step is one step from the previous block's end: no jump.
            let step = (end - prev) / BLOCK as f32;
            assert!((ramp[0] - (prev + step)).abs() < 1e-6);
            for w in ramp.windows(2) {
                assert!(if rising { w[1] >= w[0] } else { w[1] <= w[0] }, "monotonic");
            }
            out = ramp;
        }
        // Ramping down to no floor still mixes (towards wet); the block after takes the fast path.
        let mut wet = [0.5f32; BLOCK];
        m.process(&dry, &mut wet, -200.0);
        assert!(wet[0] > 0.5 && (wet[BLOCK - 1] - 0.5).abs() < 1e-6);
        let mut wet = [0.5f32; BLOCK];
        m.process(&dry, &mut wet, -200.0);
        assert!(wet.iter().all(|&v| v == 0.5));
    }

    #[test]
    fn reset_takes_the_next_floor_directly() {
        let dry = [1.0f32; BLOCK];
        let mut m = FloorMix::new();
        m.process(&dry, &mut [0.0; BLOCK], -6.0);
        m.reset();
        let mut out = [0.0f32; BLOCK];
        m.process(&dry, &mut out, -20.0);
        assert!(out.iter().all(|&v| v == db_to_lin(-20.0)));
    }
}
