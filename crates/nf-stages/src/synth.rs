//! Deterministic test signals for the offline harness (TODO #2): a speech-like voice, white and pink
//! noise, a synthetic room impulse response, and helpers to measure levels and mix at an SNR.
//!
//! Every function is a pure function of its arguments (a seed where there's randomness), so a failing
//! test reproduces exactly. Signals are at 48 kHz in ±1. None of this is real-time code.
//!
//! [`speech_like`] isn't speech, but it has the parts RNNoise and AEC3 key on: a harmonic source with
//! jitter, vibrato and phrase intonation; formants that glide between vowels within each syllable;
//! fricative onsets; syllable envelopes; and pauses between phrases, which give noise-only stretches.

use std::f64::consts::TAU;
use std::ops::Range;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// The rate every signal here is generated at.
pub const RATE: f64 = nf_core::SAMPLE_RATE as f64;

/// SplitMix64: small, fast and good enough for test signals; never use it for anything else.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x6A09_E667_F3BC_C908)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in [lo, hi).
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    /// Standard normal (Box-Muller; one of the pair is dropped, which keeps the state simple).
    pub fn gaussian(&mut self) -> f64 {
        let u = 1.0 - self.unit(); // (0, 1]: ln is finite
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

// ---- Levels and mixing ----------------------------------------------------------------------

/// RMS of `x` (0 for an empty slice).
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt() as f32
}

/// RMS over the given ranges of `x` together (ranges are clipped to `x`).
pub fn rms_over(x: &[f32], ranges: &[Range<usize>]) -> f32 {
    let (mut sum, mut n) = (0.0f64, 0usize);
    for r in ranges {
        let r = r.start.min(x.len())..r.end.min(x.len());
        n += r.len();
        sum += x[r].iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    }
    if n == 0 { 0.0 } else { (sum / n as f64).sqrt() as f32 }
}

/// An amplitude ratio in dB (-inf for 0).
pub fn db(ratio: f32) -> f32 {
    20.0 * ratio.log10()
}

/// `x` scaled by `gain`.
pub fn scaled(x: &[f32], gain: f32) -> Vec<f32> {
    x.iter().map(|&v| v * gain).collect()
}

/// The sample-wise sum, as long as the shorter input.
pub fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(&x, &y)| x + y).collect()
}

/// `x` delayed by `samples` (zeros in front), the same length as `x`.
pub fn delayed(x: &[f32], samples: usize) -> Vec<f32> {
    let n = samples.min(x.len());
    let mut out = vec![0.0; n];
    out.extend_from_slice(&x[..x.len() - n]);
    out
}

/// The gain that puts `noise` at `snr_db` below a signal whose (active) RMS is `signal_rms`.
pub fn gain_for_snr(signal_rms: f32, noise: &[f32], snr_db: f32) -> f32 {
    let n = rms(noise);
    if n > 0.0 { signal_rms / n * 10f32.powf(-snr_db / 20.0) } else { 0.0 }
}

/// `signal + noise`, with the noise scaled to `snr_db` below `signal_rms` (pass
/// [`Speech::active_rms`] for speech, so pauses don't count). Returns the mix and the scaled noise.
pub fn mix_at_snr(signal: &[f32], signal_rms: f32, noise: &[f32], snr_db: f32) -> (Vec<f32>, Vec<f32>) {
    let noise = scaled(noise, gain_for_snr(signal_rms, noise, snr_db));
    (add(signal, &noise), noise)
}

/// Rescales `x` in place to the given RMS (no-op if it's silent).
fn normalise(x: &mut [f32], target_rms: f32) {
    let r = rms(x);
    if r > 0.0 {
        let g = target_rms / r;
        x.iter_mut().for_each(|v| *v *= g);
    }
}

// ---- Noise ----------------------------------------------------------------------------------

/// Gaussian white noise at exactly `rms`.
pub fn white_noise(len: usize, rms: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut x: Vec<f32> = (0..len).map(|_| rng.gaussian() as f32).collect();
    normalise(&mut x, rms);
    x
}

/// Pink (1/f) noise at exactly `rms`: Paul Kellet's refined filter on Gaussian white noise
/// (within 0.05 dB of 1/f above about 10 Hz), run in for 0.1 s so it starts in steady state.
pub fn pink_noise(len: usize, rms: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut b = [0.0f64; 7];
    let mut step = |w: f64| {
        b[0] = 0.99886 * b[0] + w * 0.0555179;
        b[1] = 0.99332 * b[1] + w * 0.0750759;
        b[2] = 0.96900 * b[2] + w * 0.1538520;
        b[3] = 0.86650 * b[3] + w * 0.3104856;
        b[4] = 0.55000 * b[4] + w * 0.5329522;
        b[5] = -0.7616 * b[5] - w * 0.0168980;
        let y = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
        b[6] = w * 0.115926;
        y
    };
    for _ in 0..4800 {
        step(rng.gaussian());
    }
    let mut x: Vec<f32> = (0..len).map(|_| step(rng.gaussian()) as f32).collect();
    normalise(&mut x, rms);
    x
}

// ---- Speech-like voice ----------------------------------------------------------------------

/// A talker for [`speech_like`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Voice {
    /// Mean fundamental in Hz.
    pub f0_hz: f32,
    /// Formant frequencies relative to an adult male vocal tract.
    pub formant_scale: f32,
    /// RMS over the syllables, in dBFS.
    pub level_db: f32,
}

impl Voice {
    /// A low voice at -20 dBFS active level.
    pub const LOW: Voice = Voice { f0_hz: 115.0, formant_scale: 1.0, level_db: -20.0 };
    /// A high voice at -20 dBFS active level.
    pub const HIGH: Voice = Voice { f0_hz: 205.0, formant_scale: 1.17, level_db: -20.0 };

    /// The same voice at another active level.
    pub const fn at(self, level_db: f32) -> Voice {
        Voice { level_db, ..self }
    }
}

/// A speech-like signal and where its syllables are.
#[derive(Clone, Debug)]
pub struct Speech {
    pub samples: Vec<f32>,
    /// Sample ranges where a syllable sounds, in time order, non-overlapping.
    pub syllables: Vec<Range<usize>>,
}

impl Speech {
    /// RMS over the syllables: the active level that SNRs are quoted against.
    pub fn active_rms(&self) -> f32 {
        rms_over(&self.samples, &self.syllables)
    }

    /// Whether any syllable comes within `margin` samples of `i`.
    pub fn near_syllable(&self, i: usize, margin: usize) -> bool {
        self.syllables.iter().any(|r| i + margin >= r.start && i < r.end + margin)
    }

    /// Stretches at least `min_len` long, at least `margin` from every syllable: noise-only time.
    pub fn pauses(&self, min_len: usize, margin: usize) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut start = 0usize;
        let ends = self.syllables.iter().map(|r| (r.start, r.end)).chain(std::iter::once((self.samples.len() + margin, 0)));
        for (s, e) in ends {
            let (a, b) = (start, s.saturating_sub(margin).min(self.samples.len()));
            if b > a && b - a >= min_len {
                out.push(a..b);
            }
            start = e + margin;
        }
        out
    }

    /// Each syllable's middle, with `trim` removed at both ends (skips syllables shorter than that).
    pub fn cores(&self, trim: usize) -> Vec<Range<usize>> {
        self.syllables.iter().filter(|r| r.len() > 2 * trim).map(|r| r.start + trim..r.end - trim).collect()
    }
}

/// Formants (F1, F2, F3) in Hz for an adult male: /a/, /i/, /u/, /e/, /o/, /æ/.
#[rustfmt::skip]
const VOWELS: [[f64; 3]; 6] = [
    [730.0, 1090.0, 2440.0],
    [270.0, 2290.0, 3010.0],
    [300.0, 870.0, 2240.0],
    [530.0, 1840.0, 2480.0],
    [570.0, 840.0, 2410.0],
    [660.0, 1720.0, 2410.0],
];
const F4: f64 = 3400.0;
const BANDWIDTHS: [f64; 4] = [80.0, 100.0, 140.0, 220.0];
/// Harmonics stop below this (speech has little energy above, and it keeps synthesis cheap).
const MAX_HARMONIC_HZ: f64 = 7800.0;
/// Harmonic amplitudes are recomputed this often (the formants and f0 move slowly).
const CONTROL_STEP: usize = 32;

/// Magnitude of an all-pole formant at `f`, normalised to 1 at DC.
fn formant_gain(f: f64, centre: f64, bandwidth: f64) -> f64 {
    let h = 0.5 * bandwidth;
    (centre * centre + h * h) / (((f - centre).powi(2) + h * h) * ((f + centre).powi(2) + h * h)).sqrt()
}

/// One syllable's plan.
struct Syllable {
    /// Fricative onset (samples), then the voiced part (samples).
    fricative: usize,
    voiced: usize,
    /// The vowel glides from `v0` to `v1`.
    v0: usize,
    v1: usize,
    /// f0 at the start and end, Hz.
    f0_start: f64,
    f0_end: f64,
    /// Relative loudness.
    gain: f64,
}

/// A speech-like signal `len` samples long (see the module notes), at `voice.level_db` RMS over its
/// syllables. Phrases of 3-6 syllables (120-320 ms each, 30-120 ms apart) alternate with pauses of
/// 350-800 ms; the signal starts with a pause.
pub fn speech_like(len: usize, voice: Voice, seed: u64) -> Speech {
    let mut rng = Rng::new(seed);
    let ms = |rng: &mut Rng, lo: f64, hi: f64| (rng.range(lo, hi) * 1e-3 * RATE) as usize;
    let f0 = f64::from(voice.f0_hz);

    // Plan the syllables.
    let mut plan = Vec::new();
    let mut t = ms(&mut rng, 250.0, 450.0);
    let mut vowel = rng.next_u64() as usize % VOWELS.len();
    'phrases: loop {
        let count = 3 + rng.next_u64() as usize % 4;
        for k in 0..count {
            let fricative = if rng.unit() < 0.45 { ms(&mut rng, 40.0, 90.0) } else { 0 };
            let voiced = ms(&mut rng, 120.0, 230.0);
            if t + fricative + voiced >= len {
                break 'phrases;
            }
            // Declination over the phrase, plus a random accent per syllable.
            let pos = k as f64 / count as f64;
            let accent = rng.range(-0.06, 0.08);
            let next = (vowel + 1 + rng.next_u64() as usize % (VOWELS.len() - 1)) % VOWELS.len();
            plan.push((t, Syllable {
                fricative,
                voiced,
                v0: vowel,
                v1: next,
                f0_start: f0 * (1.12 - 0.22 * pos + accent),
                f0_end: f0 * (1.08 - 0.22 * (pos + 1.0 / count as f64) + accent),
                gain: rng.range(0.6, 1.0),
            }));
            vowel = next;
            t += fricative + voiced + ms(&mut rng, 30.0, 120.0);
        }
        t += ms(&mut rng, 350.0, 800.0);
        if t >= len {
            break;
        }
    }

    let mut voiced_buf = vec![0.0f32; len];
    let mut fric_buf = vec![0.0f32; len];
    let mut syllables = Vec::with_capacity(plan.len());
    let mut fric_ranges = Vec::new();
    let mut voiced_ranges = Vec::new();
    let mut phase = 0.0f64;
    let mut amps: Vec<f64> = Vec::new();
    for (start, s) in &plan {
        let v_start = start + s.fricative;
        let v_end = v_start + s.voiced;
        syllables.push(*start..v_end);
        voiced_ranges.push(v_start..v_end);

        // Fricative onset: twice-differenced noise (+12 dB/octave, /s/-like), raised-cosine envelope.
        if s.fricative > 0 {
            fric_ranges.push(*start..v_start);
            let (mut x1, mut x2) = (0.0f64, 0.0f64);
            for i in 0..s.fricative {
                let w = rng.gaussian();
                let y = w - 2.0 * x1 + x2;
                (x2, x1) = (x1, w);
                let env = 0.5 - 0.5 * (TAU * (i as f64 + 0.5) / s.fricative as f64).cos();
                fric_buf[start + i] = (y * env) as f32;
            }
        }

        // Voiced part: harmonics of a jittered, vibrato'd f0, shaped by gliding formants.
        let attack = (0.02 * RATE) as usize;
        let release = (0.045 * RATE) as usize;
        let vib_rate = rng.range(4.5, 6.0);
        let mut drift = 0.0f64;
        for i in 0..s.voiced {
            let u = i as f64 / s.voiced as f64;
            if i % CONTROL_STEP == 0 {
                drift = (drift + 0.002 * rng.gaussian()).clamp(-0.015, 0.015);
            }
            let vibrato = 0.008 * (TAU * vib_rate * i as f64 / RATE).sin();
            let f = (s.f0_start + (s.f0_end - s.f0_start) * u) * (1.0 + vibrato + drift);
            if i % CONTROL_STEP == 0 {
                // Formants glide over the first 60% of the syllable, then hold.
                let g = (u / 0.6).min(1.0);
                let g = g * g * (3.0 - 2.0 * g);
                let scale = f64::from(voice.formant_scale);
                let fm: [f64; 4] = std::array::from_fn(|j| {
                    if j == 3 { F4 * scale } else { (VOWELS[s.v0][j] + (VOWELS[s.v1][j] - VOWELS[s.v0][j]) * g) * scale }
                });
                let count = (MAX_HARMONIC_HZ / f) as usize;
                amps.clear();
                amps.extend((1..=count).map(|k| {
                    let fk = k as f64 * f;
                    let tract: f64 = (0..4).map(|j| formant_gain(fk, fm[j], BANDWIDTHS[j])).product();
                    tract / k as f64 // -12 dB/octave source, +6 dB/octave lip radiation
                }));
            }
            phase = (phase + TAU * f / RATE) % TAU;
            // sin(kφ) by the Chebyshev recurrence: one sin_cos per sample.
            let (s1, c1) = phase.sin_cos();
            let two_c = 2.0 * c1;
            let (mut prev, mut cur) = (0.0f64, s1);
            let mut acc = 0.0;
            for &a in &amps {
                acc += a * cur;
                (prev, cur) = (cur, two_c * cur - prev);
            }
            let env = if i < attack {
                0.5 - 0.5 * (std::f64::consts::PI * i as f64 / attack as f64).cos()
            } else if i + release > s.voiced {
                0.5 - 0.5 * (std::f64::consts::PI * (s.voiced - i) as f64 / release as f64).cos()
            } else {
                1.0
            };
            voiced_buf[v_start + i] = (acc * env * s.gain) as f32;
        }
    }

    // Fricatives sit 12 dB below the vowels; then the whole thing goes to the requested level.
    let target = rms_over(&voiced_buf, &voiced_ranges) * 10f32.powf(-12.0 / 20.0);
    let fr = rms_over(&fric_buf, &fric_ranges);
    let fg = if fr > 0.0 { target / fr } else { 0.0 };
    let mut samples: Vec<f32> = voiced_buf.iter().zip(&fric_buf).map(|(&v, &f)| v + fg * f).collect();
    let active = rms_over(&samples, &syllables);
    if active > 0.0 {
        let g = 10f32.powf(voice.level_db / 20.0) / active;
        samples.iter_mut().for_each(|v| *v *= g);
    }
    Speech { samples, syllables }
}

// ---- Room impulse response ------------------------------------------------------------------

/// A synthetic room impulse response `len` samples long, normalised to unit energy: the direct path
/// at index 0, eight early reflections over 2-15 ms, and a diffuse tail from 3 ms that decays by
/// 60 dB in `rt60_s`, lightly low-passed and about 3 dB below the direct path in energy (a desk:
/// speakers half a metre to a metre from the mic).
pub fn room_impulse_response(len: usize, rt60_s: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut h = vec![0.0f64; len];
    if len == 0 {
        return Vec::new();
    }
    h[0] = 1.0;
    let decay = |n: usize| 10f64.powf(-3.0 * n as f64 / RATE / f64::from(rt60_s));
    for _ in 0..8 {
        let n = (rng.range(2e-3, 15e-3) * RATE) as usize;
        let sign = if rng.unit() < 0.5 { -1.0 } else { 1.0 };
        if let Some(v) = h.get_mut(n) {
            *v += sign * rng.range(0.2, 0.55) * decay(n);
        }
    }
    let start = (3e-3 * RATE) as usize;
    let mut tail = vec![0.0f64; len];
    let mut lp = 0.0f64;
    for (n, t) in tail.iter_mut().enumerate().skip(start) {
        lp = 0.6 * lp + 0.4 * rng.gaussian(); // one-pole low-pass, about 4 kHz
        *t = lp * decay(n);
    }
    let e_tail: f64 = tail.iter().map(|v| v * v).sum();
    if e_tail > 0.0 {
        let g = (0.5 / e_tail).sqrt();
        h.iter_mut().zip(&tail).for_each(|(v, t)| *v += g * t);
    }
    let e: f64 = h.iter().map(|v| v * v).sum();
    let g = 1.0 / e.sqrt();
    h.iter().map(|v| (v * g) as f32).collect()
}

/// Linear convolution, `x.len() + h.len() - 1` long (empty if either is), by FFT in f64.
pub fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    if x.is_empty() || h.is_empty() {
        return Vec::new();
    }
    let n = x.len() + h.len() - 1;
    let m = n.next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(m);
    let c2r = planner.plan_fft_inverse(m);
    let spectrum = |v: &[f32]| {
        let mut t = vec![0.0f64; m];
        t.iter_mut().zip(v).for_each(|(d, &s)| *d = f64::from(s));
        let mut s = vec![Complex::new(0.0, 0.0); m / 2 + 1];
        r2c.process(&mut t, &mut s).expect("lengths come from the plan");
        s
    };
    let mut a = spectrum(x);
    let b = spectrum(h);
    let scale = 1.0 / m as f64;
    a.iter_mut().zip(&b).for_each(|(p, q)| *p = *p * *q * scale);
    // The product of two real signals' spectra has real DC and Nyquist bins; make it exact.
    if let Some(v) = a.first_mut() {
        v.im = 0.0;
    }
    if let Some(v) = a.last_mut() {
        v.im = 0.0;
    }
    let mut out = vec![0.0f64; m];
    c2r.process(&mut a, &mut out).expect("lengths come from the plan");
    out[..n].iter().map(|&v| v as f32).collect()
}

// ---- Listening ------------------------------------------------------------------------------

/// Writes `x` as a mono 48 kHz 32-bit float WAV, to listen to what a test fed and got back.
pub fn write_wav(path: &std::path::Path, x: &[f32]) -> Result<(), hound::Error> {
    let spec =
        hound::WavSpec { channels: 1, sample_rate: nf_core::SAMPLE_RATE, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in x {
        w.write_sample(s)?;
    }
    w.finalize() // Drop would finalize too, but swallow the error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trips() {
        let dir = std::env::temp_dir().join(format!("nf-stages-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("t.wav");
        let x = white_noise(1000, 0.2, 1);
        write_wav(&path, &x).expect("write");
        let mut r = hound::WavReader::open(&path).expect("open");
        assert_eq!(r.spec().sample_rate, 48_000);
        let y: Vec<f32> = r.samples::<f32>().map(|s| s.expect("sample")).collect();
        assert_eq!(x, y);
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rng_is_deterministic_and_roughly_uniform_and_normal() {
        let (mut a, mut b) = (Rng::new(5), Rng::new(5));
        assert!((0..100).all(|_| a.next_u64() == b.next_u64()));
        let mut r = Rng::new(1);
        let u: Vec<f64> = (0..100_000).map(|_| r.unit()).collect();
        assert!(u.iter().all(|v| (0.0..1.0).contains(v)));
        let mean = u.iter().sum::<f64>() / u.len() as f64;
        assert!((mean - 0.5).abs() < 0.01);
        let g: Vec<f64> = (0..100_000).map(|_| r.gaussian()).collect();
        let m = g.iter().sum::<f64>() / g.len() as f64;
        let var = g.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / g.len() as f64;
        assert!(m.abs() < 0.02 && (var - 1.0).abs() < 0.03, "mean {m} var {var}");
    }

    #[test]
    fn noise_has_the_requested_level_and_slope() {
        let w = white_noise(1 << 16, 0.1, 3);
        let p = pink_noise(1 << 16, 0.1, 3);
        assert!((rms(&w) - 0.1).abs() < 1e-5 && (rms(&p) - 0.1).abs() < 1e-5);
        assert_eq!(w, white_noise(1 << 16, 0.1, 3));
        assert_ne!(w, white_noise(1 << 16, 0.1, 4));
        // Pink: about -3 dB per octave. Compare the energy in 250-500 Hz with 4-8 kHz (4 octaves).
        let band = |x: &[f32], lo: f64, hi: f64| {
            let m = x.len();
            let mut planner = RealFftPlanner::<f64>::new();
            let fft = planner.plan_fft_forward(m);
            let mut t: Vec<f64> = x.iter().map(|&v| f64::from(v)).collect();
            let mut s = fft.make_output_vec();
            fft.process(&mut t, &mut s).expect("fft");
            let k = |f: f64| (f / RATE * m as f64) as usize;
            s[k(lo)..k(hi)].iter().map(|c| c.norm_sqr()).sum::<f64>()
        };
        let slope = 10.0 * (band(&p, 4000.0, 8000.0) / band(&p, 250.0, 500.0)).log10();
        assert!((slope - 0.0).abs() < 1.5, "pink: equal energy per octave, got {slope} dB"); // per octave band: flat
        let slope_w = 10.0 * (band(&w, 4000.0, 8000.0) / band(&w, 250.0, 500.0)).log10();
        assert!((slope_w - 12.04).abs() < 0.5, "white: 16x the bandwidth, got {slope_w} dB");
    }

    #[test]
    fn speech_has_syllables_pauses_and_the_requested_level() {
        let len = 10 * 48_000;
        let s = speech_like(len, Voice::LOW, 1);
        assert_eq!(s.samples.len(), len);
        assert!(s.syllables.len() >= 20, "{} syllables", s.syllables.len());
        assert!(s.syllables.windows(2).all(|w| w[0].end <= w[1].start));
        assert!((db(s.active_rms()) - -20.0).abs() < 0.01);
        let peak = s.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.9, "peak {peak}");
        // Outside syllables it's exactly silent, and there are long pauses to measure noise in.
        assert!(s.samples.iter().enumerate().all(|(i, &v)| v == 0.0 || s.near_syllable(i, 0)));
        let pauses = s.pauses(9600, 480);
        assert!(pauses.len() >= 5, "{pauses:?}");
        assert!(pauses.iter().all(|p| (p.start..p.end).all(|i| !s.near_syllable(i, 480))));
        assert_eq!(s.samples, speech_like(len, Voice::LOW, 1).samples);
        let h = speech_like(len, Voice::HIGH.at(-30.0), 2);
        assert!((db(h.active_rms()) - -30.0).abs() < 0.01);
    }

    #[test]
    fn rir_is_normalised_and_decays() {
        let h = room_impulse_response(9600, 0.1, 4);
        let e: f32 = h.iter().map(|v| v * v).sum();
        assert!((e - 1.0).abs() < 1e-4);
        let peak = h.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i);
        assert_eq!(peak, Some(0), "the direct path is the strongest");
        let e_late: f32 = h[4800..].iter().map(|v| v * v).sum();
        assert!(e_late < 1e-3, "100 ms in, 60 dB down: {e_late}");
    }

    #[test]
    fn convolution_matches_the_direct_sum() {
        let x = white_noise(1000, 0.3, 1);
        let h = room_impulse_response(77, 0.01, 2);
        let y = convolve(&x, &h);
        assert_eq!(y.len(), 1076);
        for (n, &v) in y.iter().enumerate() {
            let direct: f64 = (0..h.len()).filter(|&k| n >= k && n - k < x.len()).map(|k| f64::from(h[k]) * f64::from(x[n - k])).sum();
            assert!((f64::from(v) - direct).abs() < 1e-5, "{n}");
        }
        assert!(convolve(&[], &h).is_empty());
    }

    #[test]
    fn mixing_hits_the_snr() {
        let s = speech_like(5 * 48_000, Voice::LOW, 3);
        let n = pink_noise(s.samples.len(), 1.0, 9);
        let (mix, noise) = mix_at_snr(&s.samples, s.active_rms(), &n, 15.0);
        assert!((db(s.active_rms() / rms(&noise)) - 15.0).abs() < 0.01);
        assert_eq!(mix.len(), s.samples.len());
        assert_eq!(delayed(&[1.0, 2.0, 3.0], 1), vec![0.0, 1.0, 2.0]);
        assert_eq!(delayed(&[1.0, 2.0], 5), vec![0.0, 0.0]);
    }
}
