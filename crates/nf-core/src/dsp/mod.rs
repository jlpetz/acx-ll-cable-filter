//! Per-block DSP building blocks. Every `process` here is real-time: no allocation, locks or calls
//! out of the crate. Construction may allocate.

pub mod delay;
pub mod drift;
pub mod fade;
pub mod gate;
pub mod high_band;
pub mod meter;
pub mod mix;
pub mod noise_post;
pub mod reblock;
pub mod slip;
pub mod stft;

use crate::Block;

/// The largest magnitude a stage's sanitised input keeps: +80 dBFS. Far above any real signal (a hot mic or a float mix
/// clips at a few times full scale), far below what overflows a model: RNNoise scales by 32768, and a finite sample of
/// about 1e17 or more then overflows to inf, which becomes a NaN inside easyfft (an abort, or undefined behaviour in
/// release). A float capture buffer full of garbage has such exponents, and the engine only zeros non-finite samples.
pub const INPUT_LIMIT: f32 = 1e4;

/// NaN and ±inf become 0; everything else is clamped to ±[`INPUT_LIMIT`] (a no-op for real audio). Compares and selects,
/// so loops over it vectorise.
#[inline(always)]
pub fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-INPUT_LIMIT, INPUT_LIMIT) } else { 0.0 }
}

/// `src` sanitised ([`sanitize`]) into `dst`: what every stage does to its inputs first (a NaN or a huge sample reaches
/// undefined behaviour in easyfft, and a NaN poisons AEC3's filters). A kernel of its own, taking each block as its own
/// argument, so LLVM knows they don't overlap and vectorises without run-time checks.
#[inline]
pub fn sanitize_into(src: &Block, dst: &mut Block) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = sanitize(s);
    }
}
