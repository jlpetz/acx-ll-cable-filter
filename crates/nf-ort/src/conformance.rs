//! A pack's conformance pair (`doc/spec/model-pack-v0.md`, "Conformance"): the model run at its own rate through its own
//! STFT, framed as the reference was, so no 48 kHz reference is needed.
//!
//! [`torch_framed`] frames as `torch.stft(center=True, pad_mode='reflect')` does: the input reflect-padded by `n_fft / 2` at
//! both ends (the edge sample not repeated), a frame every hop from the padded start, `1 + len / hop` of them. The model runs
//! frame by frame with its state carried, as the streaming export runs; the inverse is `torch.istft(center=True,
//! length=len)`'s: overlap-add of the windowed inverse frames divided per sample by the overlapped squared window (the full
//! envelope), then trimmed by `n_fft / 2`. With `center` false it frames from sample 0 with no padding.
//!
//! [`compare_pcm16`] compares an output with a PCM16 reference as `soundfile.write` would have written it: ×32767, rounded,
//! clipped (reading divides by 32768, so about one least-significant bit is the floor of any tolerance).

use nf_core::dsp::stft::hann;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex32;

use crate::pack::Pack;
use crate::session::{ModelSession, SessionOptions};

/// `x` reflect-padded by `pad` at both ends (torch's `reflect`: `x[pad], ..., x[1], x[0], x[1], ...`).
pub fn reflect_pad(x: &[f32], pad: usize) -> Vec<f32> {
    let n = x.len();
    if n < 2 {
        let mut v = vec![x.first().copied().unwrap_or(0.0); pad];
        v.extend_from_slice(x);
        v.resize(n + 2 * pad, x.first().copied().unwrap_or(0.0));
        return v;
    }
    let at = |i: isize| -> f32 {
        // Reflect about 0 and n - 1 until in range.
        let period = 2 * (n as isize - 1);
        let mut j = i.rem_euclid(period);
        if j >= n as isize {
            j = period - j;
        }
        x[j as usize]
    };
    (-(pad as isize)..(n + pad) as isize).map(at).collect()
}

/// The pack's model over `x` (at its own rate), framed as its reference was (module notes).
pub fn torch_framed(pack: &Pack, x: &[f32], opts: SessionOptions) -> Result<Vec<f32>, String> {
    let st = pack.stft();
    let (n, hop) = (st.n_fft, st.hop_length);
    let w = hann(st.window_length, st.periodic);
    let pad = if st.center { n / 2 } else { 0 };
    let padded = if pad > 0 { reflect_pad(x, pad) } else { x.to_vec() };
    if padded.len() < n {
        return Err(format!("{} samples: shorter than one frame", x.len()));
    }
    let frames = 1 + (padded.len() - n) / hop;
    let mut session = ModelSession::new(pack, opts)?;
    let scale = pack.manifest.signal.input_scale;
    let mut planner = RealFftPlanner::<f32>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let (mut buf, mut spec) = (fwd.make_input_vec(), fwd.make_output_vec());
    let mut out_frame = inv.make_output_vec();
    let total = (frames - 1) * hop + n;
    let (mut y, mut env) = (vec![0.0f64; total], vec![0.0f64; total]);
    for t in 0..frames {
        let s = t * hop;
        for ((b, &v), &wv) in buf.iter_mut().zip(&padded[s..s + n]).zip(&w) {
            *b = v * wv;
        }
        fwd.process(&mut buf, &mut spec).map_err(|e| e.to_string())?;
        for (m, c) in session.mix_mut().as_chunks_mut::<2>().0.iter_mut().zip(&spec) {
            m[0] = c.re * scale;
            m[1] = c.im * scale;
        }
        session.run()?;
        for (c, e) in spec.iter_mut().zip(session.enh().as_chunks::<2>().0.iter()) {
            *c = Complex32::new(e[0] / scale, e[1] / scale);
        }
        spec[0].im = 0.0;
        if let Some(last) = spec.last_mut() {
            last.im = 0.0;
        }
        inv.process(&mut spec, &mut out_frame).map_err(|e| e.to_string())?;
        for i in 0..n {
            let wv = f64::from(w[i]);
            y[s + i] += f64::from(out_frame[i]) / n as f64 * wv;
            env[s + i] += wv * wv;
        }
    }
    Ok((pad..pad + x.len()).map(|i| if i < total && env[i] > 1e-11 { (y[i] / env[i]) as f32 } else { 0.0 }).collect())
}

/// An output against a PCM16 reference, as PCM16.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pcm16Parity {
    pub compared: usize,
    /// The largest difference, in least-significant bits.
    pub max_lsb: i32,
    /// The error's RMS against the reference's, in dB.
    pub rms_err_db: f32,
    /// Samples that differ by more than one LSB.
    pub over_one_lsb: usize,
}

/// `out[n + skip]` (±1 floats) as `soundfile.write` would write them (×32767, rounded, clipped) against `reference[n +
/// skip]` (read as ×32768 floats: the file's integers).
pub fn compare_pcm16(out: &[f32], reference: &[f32], skip: usize) -> Pcm16Parity {
    let n = out.len().min(reference.len());
    let (mut max_lsb, mut over, mut e2, mut r2) = (0i32, 0usize, 0.0f64, 0.0f64);
    let range = skip.min(n)..n;
    for i in range.clone() {
        let a = (f64::from(out[i]) * 32767.0).round().clamp(-32768.0, 32767.0) as i32;
        let b = (f64::from(reference[i]) * 32768.0).round() as i32;
        let d = (a - b).abs();
        max_lsb = max_lsb.max(d);
        if d > 1 {
            over += 1;
        }
        e2 += f64::from(d) * f64::from(d);
        r2 += f64::from(b) * f64::from(b);
    }
    let rms_err_db = if e2 == 0.0 { f32::NEG_INFINITY } else { (10.0 * (e2 / r2.max(1e-30)).log10()) as f32 };
    Pcm16Parity { compared: range.len(), max_lsb, rms_err_db, over_one_lsb: over }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflect_padding_is_torchs() {
        assert_eq!(reflect_pad(&[1.0, 2.0, 3.0, 4.0], 2), [3.0, 2.0, 1.0, 2.0, 3.0, 4.0, 3.0, 2.0]);
        assert_eq!(reflect_pad(&[1.0, 2.0], 3), [2.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0]);
        assert_eq!(reflect_pad(&[5.0], 1), [5.0, 5.0, 5.0]);
    }

    #[test]
    fn pcm16_comparison_counts_lsbs() {
        let r: Vec<f32> = [100i32, -200, 32767, 0].iter().map(|&v| v as f32 / 32768.0).collect();
        let exact: Vec<f32> = [100i32, -200, 32767, 0].iter().map(|&v| v as f32 / 32767.0).collect();
        let p = compare_pcm16(&exact, &r, 0);
        assert_eq!((p.compared, p.max_lsb, p.over_one_lsb), (4, 0, 0));
        let off: Vec<f32> = exact.iter().map(|v| v + 3.0 / 32767.0).collect();
        assert_eq!(compare_pcm16(&off, &r, 1).max_lsb, 3, "clipped at full scale: 32767 + 3 stays 32767");
    }
}
