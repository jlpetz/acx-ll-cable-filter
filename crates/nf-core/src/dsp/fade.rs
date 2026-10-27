//! Crossfades for clean switching between processing paths (a stage toggled on or off).
//!
//! - [`crossfade`] (and the in-place `crossfade_into`) blends two paths with the same latency over
//!   a block: they carry the same signal, aligned, so a linear fade has no dip or comb.
//! - `dip_splice_into` joins two paths whose latencies differ (a stage's delay coming or going).
//!   Overlapping two copies of a signal offset by the latency would comb-filter (a 173 Hz tone
//!   offset by 10 ms half-cancels mid-fade), so the old path fades out first and the new one fades
//!   in after it, each over 120 samples (2.5 ms): a 5 ms dip, never an overlap.

use crate::{BLOCK, Block};

/// Each side of a dip splice fades over this many samples (2.5 ms).
pub(crate) const SPLICE_FADE: usize = 120;
const _: () = assert!(2 * SPLICE_FADE <= BLOCK);

/// `out = old * (1 - t) + new * t`, with `t` ramping linearly across the block. Linear, not
/// equal-power: the two paths carry the same (correlated) signal.
#[inline]
pub fn crossfade(old: &Block, new: &Block, out: &mut Block) {
    let step = 1.0 / BLOCK as f32;
    for i in 0..BLOCK {
        // `i as i32`: converts as a packed `cvtdq2ps`; usize -> f32 is scalar at x86-64-v2.
        let t = (i as i32 as f32 + 0.5) * step;
        out[i] = old[i] + (new[i] - old[i]) * t;
    }
}

/// [`crossfade`] in place: `io` holds the new path and receives the blend.
#[inline]
pub(crate) fn crossfade_into(old: &Block, io: &mut Block) {
    let step = 1.0 / BLOCK as f32;
    for (i, (o, &a)) in io.iter_mut().zip(old).enumerate() {
        let t = (i as i32 as f32 + 0.5) * step;
        *o = a + (*o - a) * t;
    }
}

/// A dip splice in place: `io` holds the new path. The first [`SPLICE_FADE`] samples are `old`
/// fading out, the next [`SPLICE_FADE`] the new path fading in, and the rest the new path untouched.
#[inline]
pub(crate) fn dip_splice_into(old: &Block, io: &mut Block) {
    let step = 1.0 / SPLICE_FADE as f32;
    let (out_part, rest) = io.split_at_mut(SPLICE_FADE);
    for (i, (o, &x)) in out_part.iter_mut().zip(&old[..SPLICE_FADE]).enumerate() {
        *o = x * (1.0 - (i as i32 as f32 + 0.5) * step);
    }
    for (i, o) in rest[..SPLICE_FADE].iter_mut().enumerate() {
        *o *= (i as i32 as f32 + 0.5) * step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_near_each_side() {
        let old = [1.0f32; BLOCK];
        let new = [0.0f32; BLOCK];
        let mut out = [0.0f32; BLOCK];
        crossfade(&old, &new, &mut out);
        assert!(out[0] > 0.99 && out[BLOCK - 1] < 0.01);
        let mut io = new;
        crossfade_into(&old, &mut io);
        assert_eq!(io, out, "in place matches");
    }

    #[test]
    fn a_dip_splice_never_overlaps_the_two_paths() {
        let old = [1.0f32; BLOCK];
        let mut io = [2.0f32; BLOCK];
        dip_splice_into(&old, &mut io);
        // Old alone, falling; then new alone, rising; then new untouched.
        assert!(io[..SPLICE_FADE].windows(2).all(|w| w[1] < w[0] && w[1] < 1.0));
        assert!(io[0] > 0.99 && io[SPLICE_FADE - 1] < 0.01);
        assert!(io[SPLICE_FADE] < 0.02 && (io[2 * SPLICE_FADE - 1] - 2.0).abs() < 0.01);
        assert!(io[SPLICE_FADE..2 * SPLICE_FADE].windows(2).all(|w| w[1] > w[0]));
        assert!(io[2 * SPLICE_FADE..].iter().all(|&v| v == 2.0));
    }
}
