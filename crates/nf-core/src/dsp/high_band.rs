//! The high-band policy (TODO 3, "Rate handling"): what a band-limited spectral model's 48 kHz output holds above the
//! model's band. A 16 kHz model on the bin-mapped 1536/768 STFT fills bins 0-256 (0-8 kHz); bins 257-768 (8-24 kHz) come
//! from here, per [`HighBand`] (`ChainControls.noise.high_band`, read every hop):
//!
//! - [`HighBand::Model`]: one gain from the model's own top band, `g = sqrt(Σ|E|² / Σ|M|²)` over [`MODEL_BAND`] (bins
//!   160-240 at the model's rate: 5-7.5 kHz, below the edge where the training data rolled off), with `E` the model's
//!   output and `M` its input there, applied to the input above the band. For a real mask `E = m·M`, so `g` is the mask's
//!   energy-weighted RMS over that band. A frame whose band holds nothing keeps the last frame's `g`; `g` is clamped to
//!   [0, 1] (a denoiser's mask attenuates). No smoothing and no taper in v0: the A/B is TODO 14's.
//! - [`HighBand::Zero`]: nothing above the band (an 8 kHz band-limited output, as a resampler path would give).
//! - [`HighBand::Pass`]: the input above the band, untouched (a diagnostic).
//!
//! Real-time: no allocation, O(bins).

use std::ops::RangeInclusive;

use super::stft::Complex32;
pub use crate::config::HighBand;

/// The bins of a 512-point model spectrum (31.25 Hz each at 16 kHz) whose gain the model policy carries up: 5-7.5 kHz.
pub const MODEL_BAND: RangeInclusive<usize> = 160..=240;
/// Below this much input energy over the band, a frame keeps the last gain.
pub const MIN_BAND_ENERGY: f32 = 1e-20;

/// The model policy's gain for one frame, from interleaved re/im spectra (`B,F,T,RI` with one frame: bin `k` at `2k` and
/// `2k + 1`): `mix` the model's input, `enh` its output, over `band`. `None` when the band holds less than
/// [`MIN_BAND_ENERGY`] (the caller keeps its last gain).
#[inline]
pub fn band_gain(mix_ri: &[f32], enh_ri: &[f32], band: RangeInclusive<usize>) -> Option<f32> {
    let (lo, hi) = (2 * band.start(), 2 * band.end() + 2);
    let (m, e) = (mix_ri.get(lo..hi)?, enh_ri.get(lo..hi)?);
    let m2: f32 = m.iter().map(|v| v * v).sum();
    if m2.is_nan() || m2 < MIN_BAND_ENERGY {
        return None;
    }
    let e2: f32 = e.iter().map(|v| v * v).sum();
    let g = (e2 / m2).sqrt();
    if g.is_finite() { Some(g.clamp(0.0, 1.0)) } else { None }
}

/// Fills `out` (the host bins above the model's band) from `input` (the same bins of the host's input spectrum) by
/// `policy`, with the model policy's gain `g`. Bins of `out` past the end of `input` get 0 (the caller passes equal
/// lengths; no length panics: this runs on the inference worker, which has no panic paths).
#[inline]
pub fn fill(policy: HighBand, g: f32, input: &[Complex32], out: &mut [Complex32]) {
    let m = out.len().min(input.len());
    let (head, tail) = out.split_at_mut(m);
    match policy {
        HighBand::Model => {
            for (o, x) in head.iter_mut().zip(input) {
                *o = x * g;
            }
        }
        HighBand::Zero => head.fill(Complex32::new(0.0, 0.0)),
        HighBand::Pass => head.copy_from_slice(&input[..m]),
    }
    tail.fill(Complex32::new(0.0, 0.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ri(bins: usize, f: impl Fn(usize) -> (f32, f32)) -> Vec<f32> {
        (0..bins)
            .flat_map(|k| {
                let (a, b) = f(k);
                [a, b]
            })
            .collect()
    }

    #[test]
    fn the_model_gain_is_the_masks_energy_weighted_rms_over_its_top_band() {
        let mix = ri(257, |k| (1.0 + k as f32 * 0.01, -0.5));
        // A constant mask of 0.3 over the band (and 1 elsewhere, which mustn't count).
        let enh: Vec<f32> = mix.iter().enumerate().map(|(i, v)| if MODEL_BAND.contains(&(i / 2)) { v * 0.3 } else { *v }).collect();
        let g = band_gain(&mix, &enh, MODEL_BAND).unwrap();
        assert!((g - 0.3).abs() < 1e-6, "{g}");
        // Two halves at masks 0 and 1, equal energy: sqrt(1/2).
        let flat = ri(257, |_| (1.0, 0.0));
        let half: Vec<f32> = flat.iter().enumerate().map(|(i, v)| if i / 2 <= 200 { 0.0 } else { *v }).collect();
        let g = band_gain(&flat, &half, 161..=240).unwrap();
        assert!((g - 0.5f32.sqrt()).abs() < 1e-6, "{g}");
        // Clamped: a mask over 1 is a model error, not a gain to carry up.
        let loud: Vec<f32> = flat.iter().map(|v| v * 4.0).collect();
        assert_eq!(band_gain(&flat, &loud, MODEL_BAND), Some(1.0));
    }

    #[test]
    fn an_empty_band_or_bad_input_keeps_the_last_gain() {
        let zero = vec![0.0f32; 514];
        assert_eq!(band_gain(&zero, &zero, MODEL_BAND), None);
        let tiny = vec![1e-12f32; 514];
        assert_eq!(band_gain(&tiny, &tiny, MODEL_BAND), None);
        let mut nan = vec![1.0f32; 514];
        nan[400] = f32::NAN;
        assert_eq!(band_gain(&nan, &nan, MODEL_BAND), None);
        assert_eq!(band_gain(&[1.0; 10], &[1.0; 10], MODEL_BAND), None, "too short");
    }

    #[test]
    fn the_policies_fill_the_high_band() {
        let x: Vec<Complex32> = (0..512).map(|k| Complex32::new(k as f32, 1.0)).collect();
        let mut y = vec![Complex32::new(9.0, 9.0); 512];
        fill(HighBand::Model, 0.5, &x, &mut y);
        assert!(y.iter().zip(&x).all(|(a, b)| *a == b * 0.5));
        fill(HighBand::Pass, 0.5, &x, &mut y);
        assert_eq!(y, x);
        fill(HighBand::Zero, 0.5, &x, &mut y);
        assert!(y.iter().all(|v| *v == Complex32::new(0.0, 0.0)));
        // A shorter input: what it covers by the policy, zeros past it, and no panic.
        for policy in HighBand::ALL {
            let mut y = vec![Complex32::new(9.0, 9.0); 512];
            fill(policy, 1.0, &x[..100], &mut y);
            assert!(y[100..].iter().all(|v| *v == Complex32::new(0.0, 0.0)), "{policy:?}");
            assert_eq!(y[5], if policy == HighBand::Zero { Complex32::new(0.0, 0.0) } else { x[5] }, "{policy:?}");
        }
    }
}
