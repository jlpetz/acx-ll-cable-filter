//! The host's streaming STFT and its weighted overlap-add inverse, over realfft (TODO 3: the STFT backend for spectral model
//! packs; the owner kept realfft, 2026-10-26). Concrete types, no trait: one FFT library is in use.
//!
//! - **Framing:** causal. [`Stft`] keeps the last `N` samples in a ring (zeros before the first sample) and makes a frame
//!   every `H` samples: [`Stft::push`] takes one hop and returns that frame's one-sided, unnormalised spectrum (`N/2 + 1`
//!   bins: `X[k] = Σ w[n] x[n] e^(-2πikn/N)`).
//! - **Inverse:** [`Istft::push`] takes a frame's spectrum, inverts it (realfft's inverse is unnormalised, so ×1/N), windows
//!   it with the synthesis window, overlap-adds it, and writes the hop that is now complete, divided per sample by the
//!   overlapped squared window: `out[n] = acc[n] / Σ_j w²[n + jH]` (WOLA, as torch.istft does; never one scalar normaliser,
//!   which is exact only where the window's overlap sums to a constant). With 50% overlap that is `w²[n] + w²[n + H]`.
//! - **Offset:** an unmodified spectrum comes back as the input `N - H` samples later, from the first sample on: the frames
//!   before it hold zeros, and zeros reconstruct as zeros.
//! - Bin 0's and bin `N/2`'s imaginary parts are zeroed before the inverse (realfft refuses others: a real signal's are 0).
//!   The inverse destroys its input, so it works on its own copy.
//! - Every buffer and both FFT plans are made in `new`; `push` and `reset` don't allocate (`process_with_scratch`).
//!
//! `periodic` Hann is torch's `hann_window(N)` default. The windows of a model's STFT and of its 48 kHz bin-mapped host STFT
//! (`N·r`) sample the same curve: `w48[r·m] = w16[m]` (`doc/spec/model-pack-v0.md`, "Derived at 48 kHz").

use std::sync::Arc;

pub use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// A Hann window of `n` points: `0.5 - 0.5·cos(2πi/n)` if `periodic` (torch's default), else over `n - 1`.
pub fn hann(n: usize, periodic: bool) -> Vec<f32> {
    let d = if periodic { n } else { n.saturating_sub(1) }.max(1) as f64;
    (0..n).map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / d).cos()) as f32).collect()
}

/// The WOLA envelope for one hop: `Σ_j w²[i + jH]` over the frames that overlap sample `i` of a hop, `i` in `0..H`.
pub fn wola_envelope(window: &[f32], hop: usize) -> Vec<f32> {
    let hop = hop.max(1);
    (0..hop).map(|i| window.iter().skip(i).step_by(hop).map(|w| w * w).sum()).collect()
}

/// A streaming forward STFT (see the module notes).
pub struct Stft {
    hop: usize,
    window: Box<[f32]>,
    /// The last `N` input samples, oldest first.
    ring: Box<[f32]>,
    /// The windowed frame (realfft's input, which it overwrites).
    frame: Box<[f32]>,
    spectrum: Box<[Complex32]>,
    scratch: Box<[Complex32]>,
    fft: Arc<dyn RealToComplex<f32>>,
}

impl Stft {
    /// An STFT with `window` (its length is the FFT size `N`, even) and a hop of `hop` samples (1 to `N`). Allocates.
    pub fn new(window: &[f32], hop: usize) -> Self {
        let n = window.len();
        assert!(n >= 2 && n.is_multiple_of(2) && (1..=n).contains(&hop), "an even window of 2 or more and a hop of 1 to N");
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        Self {
            hop,
            window: window.into(),
            ring: vec![0.0; n].into_boxed_slice(),
            frame: vec![0.0; n].into_boxed_slice(),
            spectrum: fft.make_output_vec().into_boxed_slice(),
            scratch: fft.make_scratch_vec().into_boxed_slice(),
            fft,
        }
    }

    /// The FFT size `N`.
    pub fn n(&self) -> usize {
        self.window.len()
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Bins per frame: `N/2 + 1`.
    pub fn bins(&self) -> usize {
        self.spectrum.len()
    }

    /// Takes the next `hop` samples and returns the frame that ends with them. Real-time.
    #[inline]
    pub fn push(&mut self, input: &[f32]) -> &[Complex32] {
        let (n, hop) = (self.ring.len(), self.hop.min(input.len()));
        self.ring.copy_within(hop.., 0);
        self.ring[n - hop..].copy_from_slice(&input[..hop]);
        for ((f, &x), &w) in self.frame.iter_mut().zip(self.ring.iter()).zip(self.window.iter()) {
            *f = x * w;
        }
        // Sizes are the plan's own, so it can't fail.
        let _ = self.fft.process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch);
        &self.spectrum
    }

    /// The last frame's spectrum.
    pub fn spectrum(&self) -> &[Complex32] {
        &self.spectrum
    }

    /// Back to silence. Real-time.
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.spectrum.fill(Complex32::new(0.0, 0.0));
    }
}

/// A streaming WOLA inverse STFT (see the module notes).
pub struct Istft {
    hop: usize,
    /// The synthesis window ×1/N (realfft's inverse is unnormalised).
    window: Box<[f32]>,
    /// 1 / the WOLA envelope, per sample of a hop.
    inv_envelope: Box<[f32]>,
    /// Overlap-add accumulator: the next `N` output samples, oldest first.
    acc: Box<[f32]>,
    /// The inverse's input (a copy: it's destroyed) and output.
    spectrum: Box<[Complex32]>,
    frame: Box<[f32]>,
    scratch: Box<[Complex32]>,
    ifft: Arc<dyn ComplexToReal<f32>>,
}

impl Istft {
    /// The inverse for frames made with `window` (also the synthesis window) every `hop` samples. Allocates.
    pub fn new(window: &[f32], hop: usize) -> Self {
        let n = window.len();
        assert!(n >= 2 && n.is_multiple_of(2) && (1..=n).contains(&hop), "an even window of 2 or more and a hop of 1 to N");
        let ifft = RealFftPlanner::<f32>::new().plan_fft_inverse(n);
        let inv_n = 1.0 / n as f32;
        let inv_envelope: Box<[f32]> = wola_envelope(window, hop).iter().map(|&e| if e > 1e-12 { 1.0 / e } else { 0.0 }).collect();
        Self {
            hop,
            window: window.iter().map(|w| w * inv_n).collect(),
            inv_envelope,
            acc: vec![0.0; n].into_boxed_slice(),
            spectrum: ifft.make_input_vec().into_boxed_slice(),
            frame: ifft.make_output_vec().into_boxed_slice(),
            scratch: ifft.make_scratch_vec().into_boxed_slice(),
            ifft,
        }
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Bins per frame: `N/2 + 1`.
    pub fn bins(&self) -> usize {
        self.spectrum.len()
    }

    /// The stream offset of [`Stft`] then [`Istft`]: `N - H`.
    pub fn offset(&self) -> usize {
        self.acc.len() - self.hop
    }

    /// Inverts one frame's `spectrum` (`N/2 + 1` bins), overlap-adds it, and writes the `hop` samples now complete into
    /// `out`. Real-time.
    #[inline]
    pub fn push(&mut self, spectrum: &[Complex32], out: &mut [f32]) {
        let bins = self.spectrum.len();
        let n = spectrum.len().min(bins);
        self.spectrum[..n].copy_from_slice(&spectrum[..n]);
        self.spectrum[n..].fill(Complex32::new(0.0, 0.0));
        self.synthesize(out);
    }

    /// The spectrum the next [`Istft::synthesize`] inverts, to write in place (any imaginary parts at bins 0 and `N/2` are
    /// zeroed there).
    pub fn spectrum_mut(&mut self) -> &mut [Complex32] {
        &mut self.spectrum
    }

    /// [`Istft::push`] on the spectrum written through [`Istft::spectrum_mut`]. Real-time.
    #[inline]
    pub fn synthesize(&mut self, out: &mut [f32]) {
        let (n, hop) = (self.acc.len(), self.hop);
        if let Some(first) = self.spectrum.first_mut() {
            first.im = 0.0;
        }
        if let Some(last) = self.spectrum.last_mut() {
            last.im = 0.0;
        }
        // Sizes are the plan's own and the edge bins are real, so it can't fail.
        let _ = self.ifft.process_with_scratch(&mut self.spectrum, &mut self.frame, &mut self.scratch);
        for ((a, &y), &w) in self.acc.iter_mut().zip(self.frame.iter()).zip(self.window.iter()) {
            *a += y * w;
        }
        for ((o, &a), &e) in out.iter_mut().zip(self.acc.iter()).zip(self.inv_envelope.iter()) {
            *o = a * e;
        }
        self.acc.copy_within(hop.., 0);
        self.acc[n - hop..].fill(0.0);
    }

    /// Back to silence. Real-time.
    pub fn reset(&mut self) {
        self.acc.fill(0.0);
        self.spectrum.fill(Complex32::new(0.0, 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(len: usize) -> Vec<f32> {
        (0..len).map(|i| ((i * 7919) % 2003) as f32 / 2003.0 - 0.5 + 0.3 * (i as f32 * 0.013).sin()).collect()
    }

    /// The identity mask gives the input back `N - H` later, from the first sample, at the model's size and at its 48 kHz
    /// bin-mapped size.
    #[test]
    fn an_identity_mask_reconstructs_perfectly_at_n_minus_h() {
        for (n, hop) in [(512usize, 256usize), (1536, 768), (960, 480), (1024, 256)] {
            let w = hann(n, true);
            let (mut stft, mut istft) = (Stft::new(&w, hop), Istft::new(&w, hop));
            assert_eq!((stft.bins(), istft.bins(), istft.offset()), (n / 2 + 1, n / 2 + 1, n - hop));
            let x = signal(40 * n);
            let mut y = vec![0.0f32; x.len()];
            for (i, o) in x.chunks_exact(hop).zip(y.chunks_exact_mut(hop)) {
                let spec = stft.push(i).to_vec();
                istft.push(&spec, o);
            }
            let d = n - hop;
            let worst = (0..y.len()).map(|t| (y[t] - if t >= d { x[t - d] } else { 0.0 }).abs()).fold(0.0f32, f32::max);
            assert!(worst < 2e-6, "{n}/{hop}: worst error {worst}");
        }
    }

    #[test]
    fn the_envelope_of_periodic_hann_at_half_overlap_stays_within_a_half_and_one() {
        for n in [512usize, 1536] {
            let e = wola_envelope(&hann(n, true), n / 2);
            assert_eq!(e.len(), n / 2);
            assert!(e.iter().all(|&v| (0.5 - 1e-6..=1.0 + 1e-6).contains(&v)), "{n}: {:?}", (e[0], e[n / 4]));
            assert!(
                (e[0] - 1.0).abs() < 1e-6 && (e[n / 4] - 0.5).abs() < 1e-6,
                "sin⁴ + cos⁴: 1 at the edge, 0.5 a quarter of the window in"
            );
        }
        // The 48 kHz window samples the 16 kHz one's curve: w48[3m] = w16[m].
        let (w16, w48) = (hann(512, true), hann(1536, true));
        assert!((0..512).all(|m| (w48[3 * m] - w16[m]).abs() < 1e-6));
        assert_eq!(hann(4, false), [0.0, 0.75, 0.75, 0.0]);
    }

    #[test]
    fn the_spectrum_is_unnormalised_and_reset_clears_the_state() {
        let n = 512;
        let mut s = Stft::new(&vec![1.0; n], n);
        let dc = s.push(&vec![0.25; n])[0];
        assert!((dc.re - 0.25 * n as f32).abs() < 1e-3 && dc.im.abs() < 1e-6, "{dc}");
        let w = hann(n, true);
        let (mut stft, mut istft) = (Stft::new(&w, n / 2), Istft::new(&w, n / 2));
        let x = signal(8 * n);
        let run = |stft: &mut Stft, istft: &mut Istft| -> Vec<f32> {
            let mut y = vec![0.0f32; x.len()];
            for (i, o) in x.chunks_exact(n / 2).zip(y.chunks_exact_mut(n / 2)) {
                let spec = stft.push(i).to_vec();
                istft.push(&spec, o);
            }
            y
        };
        let first = run(&mut stft, &mut istft);
        stft.reset();
        istft.reset();
        assert_eq!(run(&mut stft, &mut istft), first);
    }
}
