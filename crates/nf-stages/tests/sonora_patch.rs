//! The vendored sonora 0.2.0 (vendor/README.md, review performance/F1): its three-band filter bank
//! and cascaded biquads compute `a * b + c` with `madd`, which fuses only where the target has
//! FMA. At our x86-64-v2 baseline (.cargo/config.toml) upstream's `f32::mul_add` is a call to the
//! C runtime's `fmaf` per sample, about 36 µs of AEC3's 90 µs a block.
//!
//! A fused and an unfused multiply-add round differently, so the outputs tell which one ran. Each
//! test runs sonora's filter against a port with the same operation order, bit for bit: unfused
//! without FMA (the patch), fused with it. If the `[patch.crates-io]` entries go while the
//! baseline stays at v2, these fail: upstream's output is the fused one. If upstream changes the
//! filters (a new version), port them again, or drop this file with the patch (sonora PR #37).

use sonora::three_band_filter_bank::{FULL_BAND_SIZE, NUM_BANDS, SPLIT_BAND_SIZE, ThreeBandFilterBank};
use sonora_common_audio::cascaded_biquad_filter::{BiQuadCoefficients, CascadedBiQuadFilter};

/// What the patch's `madd` does on this build.
const FUSED: bool = cfg!(target_feature = "fma");

#[inline(always)]
fn madd(fused: bool, a: f32, b: f32, c: f32) -> f32 {
    if fused { a.mul_add(b, c) } else { a * b + c }
}

/// Deterministic test signal in ±0.5 (xorshift32).
fn signal(len: usize, seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as f32 / u32::MAX as f32 - 0.5
        })
        .collect()
}

fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|v| v.to_bits()).collect()
}

/// sonora's 48 kHz high-pass (sonora-0.2.0/src/high_pass_filter.rs), the biquads AEC3's capture
/// path runs, with sonora's literals (rounded to f32 as there).
#[allow(clippy::excessive_precision)]
const HPF_48K: [BiQuadCoefficients; 3] = [
    BiQuadCoefficients {
        b: [0.9213790163564168_f32, -1.8427552370064049_f32, 0.9213790163564168_f32],
        a: [-1.9604500061078971_f32, 0.9611862979079667_f32],
    },
    BiQuadCoefficients { b: [1.0, -1.9999789078432082_f32, 1.0], a: [-1.9923834169149972_f32, 0.9926001112941157_f32] },
    BiQuadCoefficients { b: [1.0, -1.9999632520325810_f32, 1.0], a: [-1.9983570340145236_f32, 0.9984928491805198_f32] },
];

/// The cascade in sonora's operation order: one section after another, direct form 1.
struct BiquadPort {
    sections: Vec<(BiQuadCoefficients, [f32; 2], [f32; 2])>,
}

impl BiquadPort {
    fn new(c: &[BiQuadCoefficients]) -> Self {
        Self { sections: c.iter().map(|&c| (c, [0.0; 2], [0.0; 2])).collect() }
    }

    fn process(&mut self, fused: bool, buf: &mut [f32]) {
        let m = |a, b, c| madd(fused, a, b, c);
        for (c, x, y) in &mut self.sections {
            for v in buf.iter_mut() {
                let tmp = *v;
                *v = m(c.b[0], tmp, m(c.b[1], x[0], m(c.b[2], x[1], m(-c.a[0], y[0], -c.a[1] * y[1]))));
                *x = [tmp, x[0]];
                *y = [*v, y[0]];
            }
        }
    }
}

#[test]
fn the_biquads_fuse_only_where_the_target_has_fma() {
    let x = signal(4 * 480, 1);
    // Both entry points: `process` (AEC3's decimator) and `process_in_place` (the high-pass).
    let mut sonora = CascadedBiQuadFilter::new(&HPF_48K);
    let mut in_place = CascadedBiQuadFilter::new(&HPF_48K);
    let (mut got, mut got_in_place) = (vec![0.0f32; x.len()], x.clone());
    for (xb, (yb, zb)) in x.chunks(480).zip(got.chunks_mut(480).zip(got_in_place.chunks_mut(480))) {
        sonora.process(xb, yb);
        in_place.process_in_place(zb);
    }
    let port = |fused| {
        let mut p = BiquadPort::new(&HPF_48K);
        let mut y = x.clone();
        for b in y.chunks_mut(480) {
            p.process(fused, b);
        }
        y
    };
    let (want, other) = (port(FUSED), port(!FUSED));
    assert_ne!(bits(&want), bits(&other), "the test signal should tell fused from unfused rounding");
    let what = if FUSED { "fused (an FMA build)" } else { "unfused: is vendor/sonora-common-audio-0.2.0 patched in?" };
    assert_eq!(bits(&got), bits(&want), "CascadedBiQuadFilter::process should be {what}");
    assert_eq!(bits(&got_in_place), bits(&want), "CascadedBiQuadFilter::process_in_place should be {what}");
}

/// sonora's three-band filter bank (sonora-0.2.0/src/three_band_filter_bank.rs), ported with its
/// operation order. Only `filter_core`'s parts 1 and 3 multiply-add.
mod three_band {
    const SQRT_3: f32 = 1.732_050_8;
    const STRIDE_LOG2: usize = 2;
    const STRIDE: usize = 1 << STRIDE_LOG2;
    const FILTER_SIZE: usize = 4;
    const MEMORY_SIZE: usize = FILTER_SIZE * STRIDE - 1;
    const BANDS: usize = 3;
    const SPLIT: usize = 160;
    const FULL: usize = 480;
    const NON_ZERO: usize = 10;
    const ZERO_1: usize = 3;
    const ZERO_2: usize = 9;

    #[rustfmt::skip]
    const FILTER_COEFFS: [[f32; FILTER_SIZE]; NON_ZERO] = [
        [-0.00047749, -0.00496888, 0.16547118,  0.00425496],
        [-0.00173287, -0.01585778, 0.14989004,  0.00994113],
        [-0.00304815, -0.02536082, 0.12154542,  0.01157993],
        [-0.00346946, -0.02587886, 0.04760441,  0.00607594],
        [-0.00154717, -0.01136076, 0.01387458,  0.00186353],
        [ 0.00186353,  0.01387458,-0.01136076, -0.00154717],
        [ 0.00607594,  0.04760441,-0.02587886, -0.00346946],
        [ 0.00983212,  0.08543175,-0.02982767, -0.00383509],
        [ 0.00994113,  0.14989004,-0.01585778, -0.00173287],
        [ 0.00425496,  0.16547118,-0.00496888, -0.00047749],
    ];

    #[rustfmt::skip]
    const DCT_MODULATION: [[f32; BANDS]; NON_ZERO] = [
        [ 2.0,     2.0,    2.0],
        [ SQRT_3,  0.0,   -SQRT_3],
        [ 1.0,    -2.0,    1.0],
        [-1.0,     2.0,   -1.0],
        [-SQRT_3,  0.0,    SQRT_3],
        [-2.0,    -2.0,   -2.0],
        [-SQRT_3,  0.0,    SQRT_3],
        [-1.0,     2.0,   -1.0],
        [ 1.0,    -2.0,    1.0],
        [ SQRT_3,  0.0,   -SQRT_3],
    ];

    #[allow(clippy::needless_range_loop)] // sonora's index arithmetic, kept to port it faithfully
    fn filter_core(fused: bool, f: &[f32; FILTER_SIZE], input: &[f32; SPLIT], in_shift: usize, output: &mut [f32; SPLIT], state: &mut [f32; MEMORY_SIZE]) {
        let m = |a, b, c| super::madd(fused, a, b, c);
        output.fill(0.0);
        for k in 0..in_shift {
            let j = MEMORY_SIZE + k - in_shift;
            output[k] = m(f[0], state[j], m(f[1], state[j - STRIDE], m(f[2], state[j - 2 * STRIDE], f[3] * state[j - 3 * STRIDE])));
        }
        for k in in_shift..FILTER_SIZE * STRIDE {
            let shift = k - in_shift;
            let limit = (1 + (shift >> STRIDE_LOG2)).min(FILTER_SIZE);
            for i in 0..limit {
                output[k] += input[shift - i * STRIDE] * f[i];
            }
            let j_base = MEMORY_SIZE + shift - limit * STRIDE;
            for i in limit..FILTER_SIZE {
                output[k] += state[j_base - (i - limit) * STRIDE] * f[i];
            }
        }
        for k in FILTER_SIZE * STRIDE..SPLIT {
            let base = k - in_shift;
            output[k] = m(f[0], input[base], m(f[1], input[base - STRIDE], m(f[2], input[base - 2 * STRIDE], f[3] * input[base - 3 * STRIDE])));
        }
        state.copy_from_slice(&input[SPLIT - MEMORY_SIZE..]);
    }

    /// The non-zero filter for polyphase `index`, if any.
    fn filter_index(index: usize) -> Option<usize> {
        match index {
            ZERO_1 | ZERO_2 => None,
            i if i < ZERO_1 => Some(i),
            i if i < ZERO_2 => Some(i - 1),
            i => Some(i - 2),
        }
    }

    pub struct Port {
        analysis: [[f32; MEMORY_SIZE]; NON_ZERO],
        synthesis: [[f32; MEMORY_SIZE]; NON_ZERO],
    }

    impl Port {
        pub fn new() -> Self {
            Self { analysis: [[0.0; MEMORY_SIZE]; NON_ZERO], synthesis: [[0.0; MEMORY_SIZE]; NON_ZERO] }
        }

        pub fn analysis(&mut self, fused: bool, input: &[f32; FULL], output: &mut [[f32; SPLIT]; BANDS]) {
            for band in output.iter_mut() {
                band.fill(0.0);
            }
            for down in 0..BANDS {
                let sub: [f32; SPLIT] = std::array::from_fn(|k| input[(BANDS - 1) - down + BANDS * k]);
                for in_shift in 0..STRIDE {
                    let Some(fi) = filter_index(down + in_shift * BANDS) else { continue };
                    let mut out = [0.0f32; SPLIT];
                    filter_core(fused, &FILTER_COEFFS[fi], &sub, in_shift, &mut out, &mut self.analysis[fi]);
                    for (o, &mod_val) in output.iter_mut().zip(&DCT_MODULATION[fi]) {
                        for (o, &v) in o.iter_mut().zip(&out) {
                            *o += mod_val * v;
                        }
                    }
                }
            }
        }

        pub fn synthesis(&mut self, fused: bool, input: &[[f32; SPLIT]; BANDS], output: &mut [f32; FULL]) {
            output.fill(0.0);
            for up in 0..BANDS {
                for in_shift in 0..STRIDE {
                    let Some(fi) = filter_index(up + in_shift * BANDS) else { continue };
                    let mut sub = [0.0f32; SPLIT];
                    for (x, &mod_val) in input.iter().zip(&DCT_MODULATION[fi]) {
                        for (s, &v) in sub.iter_mut().zip(x) {
                            *s += mod_val * v;
                        }
                    }
                    let mut out = [0.0f32; SPLIT];
                    filter_core(fused, &FILTER_COEFFS[fi], &sub, in_shift, &mut out, &mut self.synthesis[fi]);
                    for (k, &v) in out.iter().enumerate() {
                        output[up + BANDS * k] += BANDS as f32 * v;
                    }
                }
            }
        }
    }
}

type Bands = [[f32; SPLIT_BAND_SIZE]; NUM_BANDS];

/// Analysis then synthesis, frame by frame, through `split` and `merge`; returns the bands and the
/// resynthesis, each concatenated.
fn split_and_merge(x: &[f32], mut split: impl FnMut(&[f32; FULL_BAND_SIZE], &mut Bands), mut merge: impl FnMut(&Bands, &mut [f32; FULL_BAND_SIZE])) -> (Vec<f32>, Vec<f32>) {
    let (mut bands, mut full) = (Vec::new(), Vec::new());
    for frame in x.as_chunks::<FULL_BAND_SIZE>().0 {
        let mut b: Bands = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
        let mut y = [0.0f32; FULL_BAND_SIZE];
        split(frame, &mut b);
        merge(&b, &mut y);
        bands.extend(b.iter().flatten().copied());
        full.extend_from_slice(&y);
    }
    (bands, full)
}

#[test]
fn the_three_band_filter_bank_fuses_only_where_the_target_has_fma() {
    const _: () = assert!(FULL_BAND_SIZE == 480 && NUM_BANDS == 3 && SPLIT_BAND_SIZE == 160);
    let x = signal(4 * FULL_BAND_SIZE, 2);
    let port = |fused| {
        let p = std::cell::RefCell::new(three_band::Port::new());
        split_and_merge(&x, |f, b| p.borrow_mut().analysis(fused, f, b), |b, y| p.borrow_mut().synthesis(fused, b, y))
    };
    let fb = std::cell::RefCell::new(ThreeBandFilterBank::new());
    let (bands, full) = split_and_merge(&x, |f, b| fb.borrow_mut().analysis(f, b), |b, y| fb.borrow_mut().synthesis(b, y));
    let ((want_bands, want_full), (other_bands, _)) = (port(FUSED), port(!FUSED));
    assert_ne!(bits(&want_bands), bits(&other_bands), "the test signal should tell fused from unfused rounding");
    let what = if FUSED { "fused (an FMA build)" } else { "unfused: is vendor/sonora-0.2.0 patched in?" };
    assert_eq!(bits(&bands), bits(&want_bands), "ThreeBandFilterBank::analysis should be {what}");
    assert_eq!(bits(&full), bits(&want_full), "ThreeBandFilterBank::synthesis should be {what}");
}
