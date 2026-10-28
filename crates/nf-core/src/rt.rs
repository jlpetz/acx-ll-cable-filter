//! Real-time thread helpers that don't need Windows.

/// NOT USED by the engine: Rust's docs call a persistent MXCSR change undefined behaviour (LLVM assumes
/// the default floating-point environment), so the engine dithers the mic and the reference instead
/// ([`Dither`], about -180 dBFS), which also fixes sonora #34. Kept for experiments only.
///
/// Sets flush-to-zero and denormals-are-zero on the calling thread (x86-64 MXCSR bits 15 and 6),
/// and returns the previous MXCSR so it can be restored. Denormals in AEC3's filters on a silent
/// reference cost up to ~15 ms per 10 ms block on Intel (sonora #34); with FTZ/DAZ they cost nothing.
/// Call once at the start of the audio thread. No-op on other architectures (returns 0).
pub fn enable_flush_denormals() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        let mut old: u32 = 0;
        // SAFETY: stmxcsr/ldmxcsr only read and write this thread's MXCSR through a valid pointer to
        // a local; setting FTZ (bit 15) and DAZ (bit 6) is supported on every x86-64 CPU with SSE2.
        unsafe {
            core::arch::asm!("stmxcsr [{p}]", p = in(reg) &mut old, options(nostack, preserves_flags));
            let new = old | (1 << 15) | (1 << 6);
            core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &new, options(nostack));
        }
        old
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// Restores an MXCSR value returned by [`enable_flush_denormals`].
pub fn restore_mxcsr(old: u32) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: as above; `old` was read from this thread's MXCSR.
    unsafe {
        core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &old, options(nostack));
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = old;
}

/// Independent generator lanes in [`Dither`]: one xorshift step is a serial chain of six integer
/// ops, so a single stream can't vectorise (793 ns a block on the dev box); eight streams fill one
/// `ymm` register at x86-64-v3 and a block takes 60 vector steps (107 ns, `tests/timing.rs`).
const DITHER_LANES: usize = 8;

/// A tiny deterministic noise source (xorshift32 lanes) for dithering the mic and the AEC reference
/// at about -180 dBFS, which keeps AEC3's filters out of denormal range on exact digital silence
/// (sonora #34's other fix).
#[derive(Clone, Debug)]
pub struct Dither {
    /// Sample `i` of a call takes lane `i % 8`; never zero (xorshift's fixed point).
    state: [u32; DITHER_LANES],
}

impl Dither {
    pub const fn new(seed: u32) -> Self {
        let mut state = [0u32; DITHER_LANES];
        let mut i = 0;
        while i < DITHER_LANES {
            // murmur3's finaliser, a bijection: distinct lane seeds give distinct, decorrelated
            // starts; only an input of 0 maps to 0.
            let mut h = seed.wrapping_add((i as u32).wrapping_mul(0x9E37_79B9));
            h ^= h >> 16;
            h = h.wrapping_mul(0x85EB_CA6B);
            h ^= h >> 13;
            h = h.wrapping_mul(0xC2B2_AE35);
            h ^= h >> 16;
            state[i] = if h == 0 { 0x9E37_79B9 ^ (i as u32 + 1) } else { h };
            i += 1;
        }
        Self { state }
    }

    /// Adds ±`amplitude` uniform noise to every sample.
    #[inline]
    pub fn apply(&mut self, buf: &mut [f32], amplitude: f32) {
        // A lane's word read as i32 is uniform over ±2^31; signed so it converts with cvtdq2ps.
        let scale = amplitude * (1.0 / 2_147_483_648.0);
        let mut lanes = self.state;
        let (chunks, rest) = buf.as_chunks_mut::<DITHER_LANES>();
        for chunk in chunks {
            for (s, x) in chunk.iter_mut().zip(lanes.iter_mut()) {
                *x = xorshift32(*x);
                *s += (*x as i32) as f32 * scale;
            }
        }
        for (s, x) in rest.iter_mut().zip(lanes.iter_mut()) {
            *x = xorshift32(*x);
            *s += (*x as i32) as f32 * scale;
        }
        self.state = lanes;
    }
}

#[inline(always)]
fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftz_daz_round_trip() {
        let old = enable_flush_denormals();
        #[cfg(target_arch = "x86_64")]
        {
            let tiny = std::hint::black_box(1e-38f32);
            let product = std::hint::black_box(tiny) * std::hint::black_box(1e-3f32);
            assert_eq!(product, 0.0, "denormal result should flush to zero");
        }
        restore_mxcsr(old);
    }

    #[test]
    fn dither_is_tiny_and_bounded() {
        let mut d = Dither::new(1);
        let mut buf = [0.0f32; 480];
        d.apply(&mut buf, 1e-9);
        assert!(buf.iter().all(|s| s.abs() <= 1e-9 * 1.0001));
        assert!(buf.iter().any(|s| *s != 0.0));
    }

    #[test]
    fn dither_is_uniform_zero_mean_and_white() {
        let mut d = Dither::new(0); // a zero seed still gives live lanes
        let n = 480 * 200;
        let mut x = vec![0.0f32; n];
        for block in x.chunks_mut(480) {
            d.apply(block, 1.0);
        }
        let mean = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64;
        let var = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / n as f64;
        // Uniform on ±1: mean 0, variance 1/3
        assert!(mean.abs() < 0.01, "mean {mean}");
        assert!((var - 1.0 / 3.0).abs() < 0.01, "variance {var}");
        // Neighbours (different lanes) and samples 8 apart (the same lane) are uncorrelated
        for lag in [1, 7, 8, 16] {
            let c = x.iter().zip(&x[lag..]).map(|(&a, &b)| f64::from(a) * f64::from(b)).sum::<f64>() / (n - lag) as f64 / var;
            assert!(c.abs() < 0.02, "lag {lag}: correlation {c}");
        }
        assert!(x.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn dither_lanes_carry_on_across_whole_chunk_calls() {
        // Calls that end on whole chunks of 8 continue the same streams.
        let (mut a, mut b) = (Dither::new(7), Dither::new(7));
        let mut one = [0.0f32; 480];
        a.apply(&mut one, 1.0);
        let mut parts = [0.0f32; 480];
        let (p, q) = parts.split_at_mut(240);
        b.apply(p, 1.0);
        b.apply(q, 1.0);
        assert_eq!(one, parts);
        // Another seed is another stream; a short tail takes the first lanes
        let mut other = [0.0f32; 480];
        Dither::new(8).apply(&mut other, 1.0);
        assert_ne!(one, other);
        let mut tail = [0.0f32; 3];
        Dither::new(7).apply(&mut tail, 1.0);
        assert_eq!(tail, one[..3]);
    }
}
