//! A spectral model pack as a noise model (TODO 3, 5): the host's STFT around the pack's session.
//!
//! Two framings of the same [`Spectral`] core:
//! - **[`OrtSpectralModel`]**, bin-mapped (live): at 48 kHz, an STFT of `n_fft·r` / `hop_length·r` (UL-UNAS, `r = 3`:
//!   1536/768), the same window shape. Per hop of 768:
//!   1. frame and transform (769 bins);
//!   2. the model's input is bins `0..n_fft/2` (0-255) divided by `r` and scaled by `input_scale`, in the layout's F-major
//!      order, re then im: the unnormalised DFT sums `r` times as many samples, so this matches the model's own STFT of the
//!      16 kHz signal (`doc/todo/3-model-spec.md`, P3). Its Nyquist bin `K = n_fft/2` (256) is real in the model's own STFT
//!      (an even `n_fft`), while the host's bin `K` is an ordinary complex bin: it gets `2·Re(X[K]) / r`, the term the
//!      decimated DFT folds onto `K` (`X[K] + conj X[2K - K]`), real as in training;
//!   3. one session run (`session.rs`);
//!   4. bins 0-255 out are the model's output ×`r` (÷`input_scale`);
//!   5. bins 256-768 come from the high-band policy (`nf_core::dsp::high_band`, read live from the controls), its gain from
//!      the model's own top band: bin 256 among them, as the model's real Nyquist output can't give the host bin's phase;
//!   6. inverse, window, overlap-add, divided by the envelope.
//!
//!   Its hop is 768 and its `latency_samples` 1536 (`(window + lookahead × hop) × r`), so its offset is 768; no VAD.
//! - **[`NativeSpectral`]** (offline only, deliberately not a `NoiseModel`): the pack's own STFT at its own rate (512/256),
//!   model in = X, out = E. For the parity checks against the reference implementation and the conformance pairs.
//!
//! **State carried between hops:** the session's state tensors (zeros at the start, the outputs copied back after each run),
//! the analysis ring and the overlap-add accumulator, and the high band's gain. `reset` zero-fills all of it; ONNX Runtime
//! isn't run.
//!
//! **Faults:** a run that errors, or whose output holds a non-finite value, zeros the state and the model's bins (the high
//! band still follows its policy), and is reported failed ([`NoiseModel::take_report`]): `Pipelined` substitutes for it and
//! restarts the model.
//!
//! **Warm-up** (`prepare`, on the thread that will run it): 32 frames of noise and 32 of silence, then everything zeroed. It
//! builds ONNX Runtime's arena chunk, its memory pattern and MLAS's per-thread buffers on that thread.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Instant;

use nf_core::controls::ChainControls;
use nf_core::dsp::high_band::{self, HighBand};
use nf_core::dsp::stft::{Complex32, Istft, Stft, hann};
use nf_core::noise::{HopReport, ModelInfo, NoiseModel};
use nf_core::stage::{StageError, Text};

use crate::pack::Pack;
use crate::session::{ModelSession, SessionOptions};

/// Warm-up frames of noise, then of silence.
const WARM_UP_FRAMES: usize = 32;

/// The STFT around a session, at `r` times the pack's rate.
pub struct Spectral {
    session: ModelSession,
    stft: Stft,
    istft: Istft,
    r: usize,
    /// The model's bins: `n_fft / 2 + 1`.
    bins: usize,
    /// Host spectrum → model input, and back.
    scale_in: f32,
    scale_out: f32,
    /// The model policy's band, in model bins, and its last gain.
    band: RangeInclusive<usize>,
    g_hb: f32,
    report: HopReport,
    /// Model bins below its Nyquist bin `K` that take the fold ([`Spectral::set_fold`]); 0 by default.
    fold: usize,
}

impl Spectral {
    /// The core at `r` × the pack's rate (`r = 1`: its own). Builds the session on the calling thread.
    pub fn new(pack: &Pack, r: usize, opts: SessionOptions) -> Result<Self, String> {
        let st = pack.stft();
        let r = r.max(1);
        let window = hann(st.window_length * r, st.periodic);
        let session = ModelSession::new(pack, opts)?;
        let bins = st.n_fft / 2 + 1;
        let scale = pack.manifest.signal.input_scale;
        // The model policy's band: from 62.5% to 93.75% of the trained band (UL-UNAS: 5-7.5 kHz, bins 160-240).
        let hz_per_bin = pack.manifest.signal.sample_rate as f32 / st.n_fft as f32;
        let edge = pack.band_hz();
        let band = ((0.625 * edge / hz_per_bin).round() as usize)..=((0.9375 * edge / hz_per_bin).round() as usize).min(bins - 1);
        Ok(Self {
            session,
            stft: Stft::new(&window, st.hop_length * r),
            istft: Istft::new(&window, st.hop_length * r),
            r,
            bins,
            scale_in: scale / r as f32,
            scale_out: r as f32 / scale,
            band,
            g_hb: 0.0,
            report: HopReport::default(),
            fold: 0,
        })
    }

    /// An experiment for TODO 14's A/B, off by default (bins below `K` are mapped as they are; `K` always takes its fold):
    /// fold the host spectrum about the model's Nyquist bin `K = n_fft/2` for the top `bins` model bins below it too, `X[k]
    /// + conj X[2K - k]`. For a band-limited input that is exactly the native STFT's bin (the decimated DFT's identity: its
    /// other alias terms lie near 24 kHz), so it removes the top bins' mismatch the window's spread makes (P3); for full-band
    /// input it folds the host's content just above `K` into them, as a 16 kHz signal's own aliasing would. Only with
    /// `r > 1`.
    pub fn set_fold(&mut self, bins: usize) {
        self.fold = if self.r > 1 { bins.min(self.bins - 1) } else { 0 };
    }

    pub fn hop(&self) -> usize {
        self.stft.hop()
    }

    /// The model policy's band, in model bins.
    pub fn band(&self) -> RangeInclusive<usize> {
        self.band.clone()
    }

    /// One hop in, one hop out, the high band by `policy`. Real-time apart from the session's run (TODO 6).
    #[inline]
    pub fn process(&mut self, input: &[f32], output: &mut [f32], policy: HighBand) {
        let spec = self.stft.push(input);
        let bins = self.bins;
        let mix = self.session.mix_mut();
        for (m, x) in mix.as_chunks_mut::<2>().0.iter_mut().zip(&spec[..bins]) {
            m[0] = x.re * self.scale_in;
            m[1] = x.im * self.scale_in;
        }
        let top = bins - 1;
        if self.r > 1 {
            // Bins below K by the fold experiment, and K by its own fold: 2·Re(X[K]), real (module notes).
            for k in top - self.fold..=top {
                let (x, partner) = (spec[k], spec[2 * top - k].conj());
                mix[2 * k] = (x.re + partner.re) * self.scale_in;
                mix[2 * k + 1] = (x.im + partner.im) * self.scale_in;
            }
        }
        let t = Instant::now();
        let ran = self.session.run();
        let us = t.elapsed().as_secs_f32() * 1e6;
        let ok = ran.is_ok() && self.session.enh().iter().all(|v| v.is_finite());
        let y = self.istft.spectrum_mut();
        // The model's bins out: all of them at its own rate, those below K bin-mapped (K is the high band's).
        let model_bins = if self.r > 1 { top } else { bins };
        if ok {
            for (o, e) in y[..model_bins].iter_mut().zip(self.session.enh().as_chunks::<2>().0.iter()) {
                *o = Complex32::new(e[0] * self.scale_out, e[1] * self.scale_out);
            }
            if let Some(g) = high_band::band_gain(self.session.mix(), self.session.enh(), self.band.clone()) {
                self.g_hb = g;
            }
        } else {
            self.session.reset();
            y[..model_bins].fill(Complex32::new(0.0, 0.0));
        }
        if self.r > 1 {
            high_band::fill(policy, self.g_hb, &self.stft.spectrum()[top..], &mut y[top..]);
        }
        self.istft.synthesize(output);
        self.report.add(HopReport { runs: 1, run_us: us, failed: !ok });
    }

    /// The model's last input and output frames (interleaved re/im, `B,F,T,RI`).
    pub fn last_mix(&self) -> &[f32] {
        self.session.mix()
    }

    pub fn last_enh(&self) -> &[f32] {
        self.session.enh()
    }

    /// The high band's last gain.
    pub fn high_band_gain(&self) -> f32 {
        self.g_hb
    }

    /// Back to the start: the state, the rings and the gain. Real-time; no run.
    pub fn reset(&mut self) {
        self.session.reset();
        self.stft.reset();
        self.istft.reset();
        self.g_hb = 0.0;
        self.report = HopReport::default();
    }

    /// The warm-up (module notes), then a reset.
    pub fn warm_up(&mut self) {
        let hop = self.hop();
        let mut seed = 0x9E37_79B9u32;
        let mut x = vec![0.0f32; hop];
        let mut y = vec![0.0f32; hop];
        for f in 0..2 * WARM_UP_FRAMES {
            for v in &mut x {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *v = if f < WARM_UP_FRAMES { (seed as f32 / u32::MAX as f32 - 0.5) * 0.06 } else { 0.0 };
            }
            self.process(&x, &mut y, HighBand::Model);
        }
        self.reset();
    }

    fn take_report(&mut self) -> HopReport {
        std::mem::take(&mut self.report)
    }
}

/// A spectral pack on the bin-mapped 48 kHz STFT, as a [`NoiseModel`] (see the module notes).
pub struct OrtSpectralModel {
    core: Spectral,
    info: ModelInfo,
    controls: Arc<ChainControls>,
    prepared: bool,
}

impl OrtSpectralModel {
    /// The pack's model, its session built on the calling thread (the worker), reading the high-band policy from
    /// `controls`.
    pub fn new(pack: &Pack, controls: Arc<ChainControls>, opts: SessionOptions) -> Result<Self, String> {
        let r = pack.rate_factor();
        let core = Spectral::new(pack, r, opts)?;
        let st = pack.stft();
        let m = &pack.manifest;
        let latency_samples = ((st.window_length + m.latency.lookahead_frames * st.hop_length) * r) as u32;
        let name = if m.name.is_empty() { m.id.clone() } else { m.name.clone() };
        let info = ModelInfo { id: Text::Owned(m.id.clone()), name: Text::Owned(name), hop: core.hop(), latency_samples, has_vad: false };
        Ok(Self { core, info, controls, prepared: false })
    }

    /// The spectral core (its last frames, its gain).
    pub fn core(&self) -> &Spectral {
        &self.core
    }

    /// [`Spectral::set_fold`] (an experiment; off by default).
    pub fn set_fold(&mut self, bins: usize) {
        self.core.set_fold(bins);
    }
}

impl NoiseModel for OrtSpectralModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    /// The warm-up, on the calling thread (the worker), once.
    fn prepare(&mut self) -> Result<(), StageError> {
        if !self.prepared {
            self.core.warm_up();
            self.prepared = true;
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        self.core.process(input, output, self.controls.noise.high_band());
        f32::NAN
    }

    fn take_report(&mut self) -> HopReport {
        self.core.take_report()
    }
}

/// A spectral pack at its own rate and STFT (offline only; see the module notes).
pub struct NativeSpectral {
    core: Spectral,
    /// Its own algorithmic latency, at its rate.
    latency_samples: usize,
}

impl NativeSpectral {
    pub fn new(pack: &Pack, opts: SessionOptions) -> Result<Self, String> {
        let st = pack.stft();
        let latency_samples = st.window_length + pack.manifest.latency.lookahead_frames * st.hop_length;
        Ok(Self { core: Spectral::new(pack, 1, opts)?, latency_samples })
    }

    pub fn hop(&self) -> usize {
        self.core.hop()
    }

    /// Window + lookahead × hop, at the pack's rate; the stream offset is this less a hop.
    pub fn latency_samples(&self) -> usize {
        self.latency_samples
    }

    /// One hop at the pack's rate.
    pub fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> HopReport {
        self.core.process(input, output, HighBand::Zero);
        self.core.take_report()
    }

    /// The whole of `x` (zero-padded to whole hops), as long as `x`.
    pub fn run(&mut self, x: &[f32]) -> Vec<f32> {
        let hop = self.hop();
        let mut padded = x.to_vec();
        padded.resize(x.len().div_ceil(hop) * hop, 0.0);
        let mut y = vec![0.0f32; padded.len()];
        for (i, o) in padded.chunks_exact(hop).zip(y.chunks_exact_mut(hop)) {
            self.process_hop(i, o);
        }
        y.truncate(x.len());
        y
    }

    pub fn core(&self) -> &Spectral {
        &self.core
    }

    pub fn reset(&mut self) {
        self.core.reset();
    }
}
