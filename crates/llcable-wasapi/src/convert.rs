//! Format conversion at the device edge: interleaved device frames ↔ the app's mono f32.
//!
//! Capture downmixes by averaging the channels; render copies the mono sample into every channel.
//! The common cases (f32 or i16 with 1, 2, 4, 6 or 8 channels) have const-generic strides so the
//! loops have fixed shapes (doc/dev/api/hot-loop-tools.md R8); other layouts take a generic path.
//! Called once per packet on the audio thread: no allocation, no panics.
//!
//! Non-finite samples (NaN, ±inf from a faulty driver or APO, or a RAW float endpoint) become 0 at
//! both edges (DSP-7), so the meters, the tap, the bypass paths, the slip resampler and the stages
//! all see finite data, and nothing non-finite reaches the cable. Integer formats can't carry
//! them. Above-full-scale floats are legal and pass (downstream clips them).
//!
//! Integers are written at the scale they're read at (2^15, 2^23, 2^31 to full scale), rounded to
//! the nearest step and saturated at the top, so a 16-bit or 24-bit sample read and written again
//! comes back exactly: in exclusive mode a 16-bit mic passes a 16-bit cable unchanged in
//! passthrough. No dither: the shared-mode engine converts our float output to a 16-bit cable
//! without one as well, the rounding error (-101 dBFS RMS) is far under any mic's own noise, which
//! dithers the quantiser by itself, and a dither would put a -96 dBFS hiss on the gate's silence
//! (doc/todo/8-audio-io.md, "Exclusive mode").

/// A device sample encoding (little-endian, interleaved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    /// Packed 3-byte samples.
    I24,
    /// 24 valid bits in a 32-bit container: the sample in the high three bytes, the low byte padding. Written with the low
    /// byte zero (full scale is 0x7FFFFF00, as WAVEFORMATEXTENSIBLE's layout has it), read with it ignored. Treated as plain
    /// 32-bit integers before, it was written at 32-bit scale: +1.0 as 0x7FFFFFFF and the padding byte rarely zero.
    I24In32,
    /// 32-bit integers, all 32 bits valid.
    I32,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::I16 => 2,
            SampleFormat::I24 => 3,
            SampleFormat::F32 | SampleFormat::I24In32 | SampleFormat::I32 => 4,
        }
    }
}

const I16_SCALE: f32 = 1.0 / 32_768.0;
const I32_SCALE: f32 = 1.0 / 2_147_483_648.0;
/// Full scale for writing: the inverses of the read scales, so that integers round-trip.
const I16_FULL: f32 = 32_768.0;
const I24_FULL: f32 = 8_388_608.0;
const I32_FULL: f32 = 2_147_483_648.0;

/// Decode the captured packet into stereo. A mono capture is copied into both channels.
pub fn to_stereo(format: SampleFormat, channels: usize, bytes: &[u8], dst: &mut [[f32; 2]]) -> usize {
    if !(1..=2).contains(&channels) {
        return 0;
    }
    fn decode<const N: usize>(src: &[u8], channels: usize, dst: &mut [[f32; 2]], sample: impl Fn([u8; N]) -> f32) -> usize {
        let (samples, _) = src.as_chunks::<N>();
        let count = dst.len().min(samples.len() / channels);
        for (f, output) in samples.chunks_exact(channels).zip(dst) {
            let left = sample(f[0]);
            *output = if channels == 2 { [left, sample(f[1])] } else { [left; 2] };
        }
        count
    }
    match format {
        SampleFormat::F32 => decode(bytes, channels, dst, |b| finite_or_zero(f32::from_le_bytes(b))),
        SampleFormat::I16 => decode(bytes, channels, dst, |b| i16::from_le_bytes(b) as f32 * I16_SCALE),
        SampleFormat::I24 => decode::<3>(bytes, channels, dst, |b| i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 * I32_SCALE),
        SampleFormat::I24In32 => decode(bytes, channels, dst, |b| (i32::from_le_bytes(b) & !0xff) as f32 * I32_SCALE),
        SampleFormat::I32 => decode(bytes, channels, dst, |b| i32::from_le_bytes(b) as f32 * I32_SCALE),
    }
}

/// Converts interleaved `channels`-channel frames in `src` to mono in `dst`, frame by frame, for
/// as many frames as both hold. Returns the frames converted.
pub fn to_mono(format: SampleFormat, channels: usize, src: &[u8], dst: &mut [f32]) -> usize {
    if channels == 0 {
        return 0;
    }
    let frames = (src.len() / (format.bytes() * channels)).min(dst.len());
    let dst = &mut dst[..frames];
    match (format, channels) {
        (SampleFormat::F32, 1) => downmix_f32::<1>(src, dst),
        (SampleFormat::F32, 2) => downmix_f32::<2>(src, dst),
        (SampleFormat::F32, 4) => downmix_f32::<4>(src, dst),
        (SampleFormat::F32, 6) => downmix_f32::<6>(src, dst),
        (SampleFormat::F32, 8) => downmix_f32::<8>(src, dst),
        (SampleFormat::I16, 1) => downmix_i16::<1>(src, dst),
        (SampleFormat::I16, 2) => downmix_i16::<2>(src, dst),
        (SampleFormat::F32, c) => downmix_generic::<4>(c, src, dst, f32::from_le_bytes),
        (SampleFormat::I16, c) => downmix_generic::<2>(c, src, dst, |b| i16::from_le_bytes(b) as f32 * I16_SCALE),
        (SampleFormat::I24, c) => downmix_generic::<3>(c, src, dst, |b| i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 * I32_SCALE),
        (SampleFormat::I24In32, c) => downmix_generic::<4>(c, src, dst, |b| (i32::from_le_bytes(b) & !0xFF) as f32 * I32_SCALE),
        (SampleFormat::I32, c) => downmix_generic::<4>(c, src, dst, |b| i32::from_le_bytes(b) as f32 * I32_SCALE),
    }
    frames
}

/// Writes mono `src` into interleaved `channels`-channel frames in `dst` (every channel gets the
/// sample), for as many frames as both hold. Integer formats are rounded to the nearest step and
/// clamped to full scale. Returns the frames written.
pub fn from_mono(format: SampleFormat, channels: usize, src: &[f32], dst: &mut [u8]) -> usize {
    if channels == 0 {
        return 0;
    }
    let frames = (dst.len() / (format.bytes() * channels)).min(src.len());
    let src = &src[..frames];
    match (format, channels) {
        (SampleFormat::F32, 1) => upmix_f32::<1>(src, dst),
        (SampleFormat::F32, 2) => upmix_f32::<2>(src, dst),
        (SampleFormat::F32, 4) => upmix_f32::<4>(src, dst),
        (SampleFormat::F32, 6) => upmix_f32::<6>(src, dst),
        (SampleFormat::F32, 8) => upmix_f32::<8>(src, dst),
        (SampleFormat::F32, c) => upmix_generic::<4>(c, src, dst, |x| finite_or_zero(x).to_le_bytes()),
        (SampleFormat::I16, 1) => upmix_i16::<1>(src, dst),
        (SampleFormat::I16, 2) => upmix_i16::<2>(src, dst),
        (SampleFormat::I16, c) => upmix_generic::<2>(c, src, dst, |x| i16_of(x).to_le_bytes()),
        (SampleFormat::I24, c) => upmix_generic::<3>(c, src, dst, |x| {
            let b = (scaled(x, I24_FULL) as i32).min(8_388_607).to_le_bytes();
            [b[0], b[1], b[2]]
        }),
        (SampleFormat::I24In32, c) => upmix_generic::<4>(c, src, dst, |x| ((scaled(x, I24_FULL) as i32).min(8_388_607) << 8).to_le_bytes()),
        (SampleFormat::I32, c) => upmix_generic::<4>(c, src, dst, |x| (scaled(x, I32_FULL) as i32).to_le_bytes()),
    }
    frames
}

/// Clamps to ±1. The bounds are constants, so clamp's bounds assert folds away; NaN passes
/// through to the saturating cast, which makes it 0.
#[inline(always)]
fn clip(x: f32) -> f32 {
    x.clamp(-1.0, 1.0)
}

/// `x` clipped to ±1, at `full` to full scale, rounded to the nearest step (ties to even: one
/// rounding instruction at x86-64-v3). The caller's saturating cast takes +full scale to the top
/// step, and NaN to 0.
#[inline(always)]
fn scaled(x: f32, full: f32) -> f32 {
    (clip(x) * full).round_ties_even()
}

#[inline(always)]
fn i16_of(x: f32) -> i16 {
    scaled(x, I16_FULL) as i16
}

/// NaN and ±inf become 0; everything else passes. A compare and a branch per sample in the
/// (already scalar) conversion loops: negligible next to the conversion itself.
#[inline(always)]
fn finite_or_zero(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

fn downmix_f32<const C: usize>(src: &[u8], dst: &mut [f32]) {
    let (samples, _) = src.as_chunks::<4>();
    let (frames, _) = samples.as_chunks::<C>();
    let scale = 1.0 / C as f32;
    for (o, f) in dst.iter_mut().zip(frames) {
        let mut acc = 0.0f32;
        for s in f {
            acc += f32::from_le_bytes(*s);
        }
        // Checked on the sum: a NaN or inf in any channel (or an overflowing sum) zeroes the frame.
        *o = finite_or_zero(if C == 1 { acc } else { acc * scale });
    }
}

fn downmix_i16<const C: usize>(src: &[u8], dst: &mut [f32]) {
    let (samples, _) = src.as_chunks::<2>();
    let (frames, _) = samples.as_chunks::<C>();
    let scale = I16_SCALE / C as f32;
    for (o, f) in dst.iter_mut().zip(frames) {
        let mut acc = 0.0f32;
        for s in f {
            acc += i16::from_le_bytes(*s) as f32;
        }
        *o = acc * scale;
    }
}

fn downmix_generic<const B: usize>(channels: usize, src: &[u8], dst: &mut [f32], decode: impl Fn([u8; B]) -> f32) {
    let (samples, _) = src.as_chunks::<B>();
    let scale = 1.0 / channels as f32;
    for (o, f) in dst.iter_mut().zip(samples.chunks_exact(channels)) {
        let mut acc = 0.0f32;
        for s in f {
            acc += decode(*s);
        }
        // As in `downmix_f32` (integers are always finite; the select costs nothing there).
        *o = finite_or_zero(acc * scale);
    }
}

fn upmix_f32<const C: usize>(src: &[f32], dst: &mut [u8]) {
    let (samples, _) = dst.as_chunks_mut::<4>();
    let (frames, _) = samples.as_chunks_mut::<C>();
    for (f, &x) in frames.iter_mut().zip(src) {
        *f = [finite_or_zero(x).to_le_bytes(); C];
    }
}

/// 16-bit, the format exclusive mode gets on VB-CABLE: the common layouts with fixed shapes.
fn upmix_i16<const C: usize>(src: &[f32], dst: &mut [u8]) {
    let (samples, _) = dst.as_chunks_mut::<2>();
    let (frames, _) = samples.as_chunks_mut::<C>();
    for (f, &x) in frames.iter_mut().zip(src) {
        *f = [i16_of(x).to_le_bytes(); C];
    }
}

fn upmix_generic<const B: usize>(channels: usize, src: &[f32], dst: &mut [u8], encode: impl Fn(f32) -> [u8; B]) {
    let (samples, _) = dst.as_chunks_mut::<B>();
    for (f, &x) in samples.chunks_exact_mut(channels).zip(src) {
        let b = encode(x);
        for s in f {
            *s = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_capture_keeps_channels_and_mono_duplicates_with_sanitization() {
        let mut dst = [[3.; 2]; 3];
        let bytes: Vec<_> = [0.75f32, -0.5, 0.25, f32::NAN].into_iter().flat_map(f32::to_le_bytes).collect();
        assert_eq!(to_stereo(SampleFormat::F32, 2, &bytes, &mut dst), 2);
        assert_eq!(dst, [[0.75, -0.5], [0.25, 0.], [3., 3.]]);
        let bytes: Vec<_> = [0x4000_00ff_i32, 0xc000_0011_u32 as i32].into_iter().flat_map(i32::to_le_bytes).collect();
        assert_eq!(to_stereo(SampleFormat::I24In32, 1, &bytes, &mut dst), 2);
        assert_eq!(dst, [[0.5, 0.5], [-0.5, -0.5], [3., 3.]]);
    }

    fn f32_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn f32_stereo_averages() {
        let src = f32_bytes(&[1.0, 0.0, 0.5, 0.5, -1.0, 0.25]);
        let mut dst = [9.0f32; 4];
        assert_eq!(to_mono(SampleFormat::F32, 2, &src, &mut dst), 3);
        assert_eq!(&dst[..3], &[0.5, 0.5, -0.375]);
        assert_eq!(dst[3], 9.0, "past the frames: untouched");
    }

    #[test]
    fn f32_mono_is_exact() {
        let v = [0.1f32, -0.7, 1e-30, 3.5];
        let mut dst = [0.0f32; 4];
        assert_eq!(to_mono(SampleFormat::F32, 1, &f32_bytes(&v), &mut dst), 4);
        assert_eq!(dst, v);
    }

    #[test]
    fn every_channel_count_averages() {
        for ch in 1..=10usize {
            let frames = 5;
            let mut v = Vec::new();
            for f in 0..frames {
                for c in 0..ch {
                    v.push((f * 10 + c) as f32);
                }
            }
            let mut dst = vec![0.0f32; frames];
            assert_eq!(to_mono(SampleFormat::F32, ch, &f32_bytes(&v), &mut dst), frames);
            for (f, o) in dst.iter().enumerate() {
                let expect = (0..ch).map(|c| (f * 10 + c) as f32).sum::<f32>() / ch as f32;
                assert!((o - expect).abs() < 1e-5, "ch {ch} frame {f}: {o} vs {expect}");
            }
        }
    }

    #[test]
    fn integer_formats_scale_to_unit_range() {
        // i16 stereo: full-scale negative and half positive.
        let src: Vec<u8> = [i16::MIN, 16_384].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut dst = [0.0f32; 1];
        to_mono(SampleFormat::I16, 2, &src, &mut dst);
        assert!((dst[0] - (-1.0 + 0.5) / 2.0).abs() < 1e-6);
        // i16 mono, 3 channels (generic path).
        let src: Vec<u8> = [16_384i16, 16_384, 16_384].iter().flat_map(|v| v.to_le_bytes()).collect();
        to_mono(SampleFormat::I16, 3, &src, &mut dst);
        assert!((dst[0] - 0.5).abs() < 1e-6);
        // Packed 24-bit mono: 0x400000 = 0.5.
        let src = [0x00u8, 0x00, 0x40, 0x00, 0x00, 0xC0];
        let mut two = [0.0f32; 2];
        assert_eq!(to_mono(SampleFormat::I24, 1, &src, &mut two), 2);
        assert!((two[0] - 0.5).abs() < 1e-6 && (two[1] + 0.5).abs() < 1e-6);
        // 32-bit int.
        let src = (i32::MAX / 2 + 1).to_le_bytes();
        to_mono(SampleFormat::I32, 1, &src, &mut dst);
        assert!((dst[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn short_inputs_convert_only_whole_frames() {
        let src = f32_bytes(&[1.0, 1.0, 1.0]); // 1.5 stereo frames
        let mut dst = [0.0f32; 4];
        assert_eq!(to_mono(SampleFormat::F32, 2, &src, &mut dst), 1);
        assert_eq!(to_mono(SampleFormat::F32, 0, &src, &mut dst), 0);
        let mut out = [0u8; 7];
        assert_eq!(from_mono(SampleFormat::F32, 2, &[0.5, 0.5], &mut out), 0);
    }

    #[test]
    fn upmix_copies_into_every_channel() {
        for ch in [1usize, 2, 3, 4, 6, 8] {
            let src = [0.25f32, -0.5, 1.0];
            let mut dst = vec![0u8; src.len() * ch * 4];
            assert_eq!(from_mono(SampleFormat::F32, ch, &src, &mut dst), 3);
            let back: Vec<f32> = dst.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            for (f, frame) in back.chunks_exact(ch).enumerate() {
                assert!(frame.iter().all(|&v| v == src[f]), "ch {ch}");
            }
        }
    }

    /// DSP-7: NaN and inf never get past the capture edge, on any channel layout, nor out to an
    /// f32 device; above-full-scale floats pass.
    #[test]
    fn non_finite_samples_become_zero_at_both_edges() {
        let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        for ch in [1usize, 2, 3, 4, 5, 6, 8] {
            for &b in &bad {
                // Frame 0 has one bad channel; frame 1 is fine and above full scale.
                let mut v = vec![0.5f32; 2 * ch];
                v[ch - 1] = b;
                for s in &mut v[ch..] {
                    *s = 1.5;
                }
                let mut dst = [9.0f32; 2];
                assert_eq!(to_mono(SampleFormat::F32, ch, &f32_bytes(&v), &mut dst), 2);
                assert_eq!(dst[0], 0.0, "ch {ch}, {b}");
                assert!((dst[1] - 1.5).abs() < 1e-6, "ch {ch}: {}", dst[1]);
            }
            // A sum that overflows to inf.
            let v = vec![f32::MAX; ch];
            let mut dst = [9.0f32; 1];
            to_mono(SampleFormat::F32, ch, &f32_bytes(&v), &mut dst);
            assert!(dst[0].is_finite(), "ch {ch}: {}", dst[0]);
        }
        for ch in [1usize, 2, 3, 6] {
            let src = [f32::NAN, f32::INFINITY, 2.0, -0.25];
            let mut dst = vec![0u8; src.len() * ch * 4];
            from_mono(SampleFormat::F32, ch, &src, &mut dst);
            let back: Vec<f32> = dst.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            for (f, frame) in back.chunks_exact(ch).enumerate() {
                let expect = [0.0, 0.0, 2.0, -0.25][f];
                assert!(frame.iter().all(|&v| v == expect), "ch {ch} frame {f}: {frame:?}");
            }
        }
    }

    #[test]
    fn integer_upmix_clamps_and_round_trips() {
        let src = [2.0f32, -2.0, 0.5, f32::NAN];
        let mut dst = vec![0u8; src.len() * 2 * 2];
        from_mono(SampleFormat::I16, 2, &src, &mut dst);
        let v: Vec<i16> = dst.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        assert_eq!(v, vec![32_767, 32_767, -32_768, -32_768, 16_384, 16_384, 0, 0]);
        // Every layout clamps alike (the fixed shapes and the generic path).
        for ch in [1usize, 2, 3] {
            let mut dst = vec![0u8; src.len() * ch * 2];
            from_mono(SampleFormat::I16, ch, &src, &mut dst);
            let v: Vec<i16> = dst.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
            let expect: Vec<i16> = [32_767, -32_768, 16_384, 0].iter().flat_map(|&x| std::iter::repeat_n(x, ch)).collect();
            assert_eq!(v, expect, "{ch} ch");
        }
        let mut b24 = [0u8; 6];
        from_mono(SampleFormat::I24, 1, &[1.0, -1.0], &mut b24);
        assert_eq!(b24, [0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x80], "24-bit: the top step, and -full scale exactly");

        for fmt in [SampleFormat::I16, SampleFormat::I24, SampleFormat::I24In32, SampleFormat::I32, SampleFormat::F32] {
            let src = [0.5f32, -0.25, 0.0];
            let mut bytes = vec![0u8; src.len() * fmt.bytes() * 3];
            from_mono(fmt, 3, &src, &mut bytes);
            let mut back = [0.0f32; 3];
            to_mono(fmt, 3, &bytes, &mut back);
            for (a, b) in src.iter().zip(back) {
                assert!((a - b).abs() < 1e-4, "{fmt:?}: {a} vs {b}");
            }
        }
    }

    /// 24-in-32 (our driver's integer format; most hardware's): the 24-bit value in the high three bytes and the padding
    /// byte zero, so +1.0 is 0x7FFFFF00 (it was 0x7FFFFFFF, at 32-bit scale) and +0.1 has no stray low byte (it was
    /// 0x0CCCCCD0); every 24-bit value read and written again comes back exactly; a padding byte read is ignored.
    #[test]
    fn twenty_four_in_thirty_two_keeps_the_padding_byte_zero() {
        let mut b = [0u8; 12];
        from_mono(SampleFormat::I24In32, 1, &[1.0, -1.0, 0.1], &mut b);
        let v: Vec<u32> = b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        assert_eq!(v[..2], [0x7FFF_FF00, 0x8000_0000]);
        assert_eq!(v[2], ((0.1f32 * I24_FULL).round_ties_even() as u32) << 8);
        assert_eq!(v[2] & 0xFF, 0, "no stray low byte");
        let all24: Vec<i32> = (-8_388_608..8_388_608).step_by(991).chain([8_388_607]).collect();
        for ch in [1usize, 2] {
            let bytes: Vec<u8> = all24.iter().flat_map(|v| std::iter::repeat_n((v << 8).to_le_bytes(), ch)).flatten().collect();
            let mut mono = vec![0.0f32; all24.len()];
            to_mono(SampleFormat::I24In32, ch, &bytes, &mut mono);
            let mut out = vec![0u8; bytes.len()];
            from_mono(SampleFormat::I24In32, ch, &mono, &mut out);
            assert!(out == bytes, "{ch} ch: a 24-in-32 sample changed on the way through");
        }
        let mut noisy = [0u8; 4];
        noisy.copy_from_slice(&((1_000i32 << 8) | 0x7F).to_le_bytes());
        let mut back = [0.0f32];
        to_mono(SampleFormat::I24In32, 1, &noisy, &mut back);
        assert_eq!(back[0], (1_000i32 << 8) as f32 * I32_SCALE, "the padding byte is ignored");
    }

    /// Exclusive mode's 16-bit path (VB-CABLE takes nothing else there): every 16-bit value read from a device and written
    /// back comes back exactly, at 1, 2 and 3 channels, so a 16-bit mic passes a 16-bit cable unchanged in passthrough; a
    /// float between the steps lands on the nearest, half a step off at most. 24-bit round-trips exactly too.
    #[test]
    fn sixteen_bit_round_trips_exactly() {
        let all: Vec<i16> = (i16::MIN..=i16::MAX).collect();
        for ch in [1usize, 2, 3] {
            let bytes: Vec<u8> = all.iter().flat_map(|v| std::iter::repeat_n(v.to_le_bytes(), ch)).flatten().collect();
            let mut mono = vec![0.0f32; all.len()];
            assert_eq!(to_mono(SampleFormat::I16, ch, &bytes, &mut mono), all.len());
            let mut out = vec![0u8; bytes.len()];
            assert_eq!(from_mono(SampleFormat::I16, ch, &mono, &mut out), all.len());
            assert!(out == bytes, "{ch} ch: a 16-bit sample changed on the way through");
        }
        for k in 0..10_000 {
            let x = (k as f32 / 10_000.0) * 1.9 - 0.95 + 1.3e-6;
            let mut b = [0u8; 2];
            from_mono(SampleFormat::I16, 1, &[x], &mut b);
            let back = f32::from(i16::from_le_bytes(b)) * I16_SCALE;
            assert!((back - x).abs() <= 0.5 * I16_SCALE + 1e-9, "{x}: {back}");
        }
        let all24: Vec<i32> = (-8_388_608..8_388_608).step_by(997).chain([8_388_607]).collect();
        let bytes: Vec<u8> = all24
            .iter()
            .flat_map(|v| {
                let b = v.to_le_bytes();
                [b[0], b[1], b[2]]
            })
            .collect();
        let mut mono = vec![0.0f32; all24.len()];
        to_mono(SampleFormat::I24, 1, &bytes, &mut mono);
        let mut out = vec![0u8; bytes.len()];
        from_mono(SampleFormat::I24, 1, &mono, &mut out);
        assert!(out == bytes, "24-bit");
    }
}
