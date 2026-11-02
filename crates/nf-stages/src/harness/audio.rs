//! The fetched test-audio set (TODO 14's catalogue, `tools/fetch_test_audio.py`) and the measures the ignored quality tests
//! print, shared by nf-stages' and nf-ort's tests (`tests/test_audio.rs`, nf-ort's `tests/ort/quality.rs`):
//! - [`audio_dir`] reads `NF_TEST_AUDIO`, or `%LOCALAPPDATA%\noise-filter\test-audio\wav48`, and says why when it's missing;
//!   nothing here writes there.
//! - [`pause_cut`]: how far the noise falls in the speech's pauses (blocks 40 dB under its active level), read from the output
//!   a latency later: full band, and below and above 8 kHz (a 16 kHz model's band edge) separately.
//! - [`speech_kept_db`]: the median, over the clean take's active 20 ms frames, of the least-squares gain of the output on the
//!   clean speech (aligned): how much of the speech survives (0 dB: all of it).
//!
//! Not real-time code.

use std::ops::Range;
use std::path::{Path, PathBuf};

use nf_core::{BLOCK, SAMPLE_RATE};
use realfft::RealFftPlanner;

use crate::synth::{db, rms, rms_over};

/// DEMAND's cafeteria, the noise every speech take has been mixed with.
pub const CAFE: &str = "noise_cafe_demand_pcafeter_ch01";
/// Freesound 448209, keyboard typing (CC0).
pub const KEYBOARD: &str = "noise_keyboard_448209";
/// Freesound 242042, a computer fan's loop (CC0).
pub const FAN: &str = "noise_fan_242042";
/// The split for [`pause_cut`]'s two bands: a 16 kHz model's band edge.
pub const SPLIT_HZ: f32 = 8000.0;
/// [`speech_kept_db`]'s frame: 20 ms.
pub const FRAME: usize = 960;

/// The set's 48 kHz folder, or `None` (with a message) when it isn't there.
pub fn audio_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("NF_TEST_AUDIO")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join("noise-filter").join("test-audio").join("wav48")));
    match dir {
        Some(d) if d.is_dir() => Some(d),
        _ => {
            eprintln!("skipped: no test audio (set NF_TEST_AUDIO, or run tools/fetch_test_audio.py)");
            None
        }
    }
}

/// A WAV, mono (channels averaged), in ±1 (integer PCM divided by 2^(bits-1)), and its rate.
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let mut r = hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = r.spec();
    let ch = usize::from(spec.channels.max(1));
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()
        }
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((samples.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect(), spec.sample_rate))
}

/// A 48 kHz take, mono, in ±1. Panics (naming the file) on anything else: the set is 48 kHz.
pub fn read(path: &Path) -> Vec<f32> {
    let (x, rate) = read_wav(path).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rate, SAMPLE_RATE, "{}", path.display());
    x
}

/// The set's `speech_*.wav` takes, sorted.
pub fn speech_takes(dir: &Path) -> Vec<PathBuf> {
    let mut takes: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|p: &PathBuf| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("speech_") && n.ends_with(".wav")))
        .collect();
    takes.sort();
    takes
}

/// `noise` looped or cut to `len` samples.
pub fn looped(noise: &[f32], len: usize) -> Vec<f32> {
    if noise.is_empty() { vec![0.0; len] } else { noise.iter().copied().cycle().take(len).collect() }
}

/// RMS over the blocks within 30 dB of the loudest: a take's active level, its pauses left out.
pub fn active_rms(x: &[f32]) -> f32 {
    let blocks: Vec<f32> = x.chunks(BLOCK).map(rms).collect();
    let loudest = blocks.iter().copied().fold(0.0f32, f32::max);
    let active: Vec<Range<usize>> =
        blocks.iter().enumerate().filter(|(_, r)| **r > loudest * 0.0316).map(|(b, _)| b * BLOCK..(b + 1) * BLOCK).collect();
    rms_over(x, &active)
}

/// The blocks of `speech` 40 dB under `level` whose output, `latency` later, is within `out_len`.
pub fn pauses(speech: &[f32], level: f32, latency: usize, out_len: usize) -> Vec<Range<usize>> {
    (0..speech.len() / BLOCK)
        .map(|b| b * BLOCK..(b + 1) * BLOCK)
        .filter(|r| rms(&speech[r.clone()]) < level * 0.01 && r.end + latency <= out_len)
        .collect()
}

/// [`pause_cut`]'s result: how far the noise fell in the pauses, in dB (positive: quieter), and how long they were.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PauseCut {
    pub full_db: f32,
    /// Below and above [`SPLIT_HZ`].
    pub low_db: f32,
    pub high_db: f32,
    pub seconds: f32,
}

/// Power below and above [`SPLIT_HZ`] over `ranges` of `x` (each range's own FFT, rectangular: Parseval's split).
fn band_power(x: &[f32], ranges: &[Range<usize>]) -> (f64, f64) {
    let mut planner = RealFftPlanner::<f64>::new();
    let (mut lo, mut hi) = (0.0f64, 0.0f64);
    for r in ranges {
        let r = r.start.min(x.len())..r.end.min(x.len());
        if r.is_empty() {
            continue;
        }
        let fft = planner.plan_fft_forward(r.len());
        let mut buf: Vec<f64> = x[r.clone()].iter().map(|&v| f64::from(v)).collect();
        let mut spec = fft.make_output_vec();
        let _ = fft.process(&mut buf, &mut spec);
        let hz = f64::from(SAMPLE_RATE) / r.len() as f64;
        for (k, c) in spec.iter().enumerate() {
            // One-sided: the edge bins count once, the rest twice.
            let w = if k == 0 || (r.len().is_multiple_of(2) && k == spec.len() - 1) { 1.0 } else { 2.0 };
            let p = w * c.norm_sqr();
            if (k as f64 * hz) < f64::from(SPLIT_HZ) {
                lo += p;
            } else {
                hi += p;
            }
        }
    }
    (lo, hi)
}

/// How far `noise` (as mixed) falls in `pauses` of the input, against `out` read `latency` later.
pub fn pause_cut(noise: &[f32], out: &[f32], pauses: &[Range<usize>], latency: usize) -> PauseCut {
    let later: Vec<Range<usize>> = pauses.iter().map(|r| r.start + latency..r.end + latency).collect();
    let full_db = db(rms_over(noise, pauses) / rms_over(out, &later));
    let ((nl, nh), (ol, oh)) = (band_power(noise, pauses), band_power(out, &later));
    let ratio_db = |a: f64, b: f64| (10.0 * (a / b).log10()) as f32;
    PauseCut {
        full_db,
        low_db: ratio_db(nl, ol),
        high_db: ratio_db(nh, oh),
        seconds: pauses.iter().map(|r| r.len()).sum::<usize>() as f32 / SAMPLE_RATE as f32,
    }
}

/// The median, over `clean`'s 20 ms frames within 20 dB of `level`, of the least-squares gain of `out` (read `latency`
/// later) on the clean speech, in dB. NaN when no frame qualifies.
pub fn speech_kept_db(clean: &[f32], out: &[f32], latency: usize, level: f32) -> f32 {
    let mut gains: Vec<f32> = (0..clean.len() / FRAME)
        .map(|f| f * FRAME..(f + 1) * FRAME)
        .filter(|r| r.end + latency <= out.len() && rms(&clean[r.clone()]) > level * 0.1)
        .filter_map(|r| {
            let (c, o) = (&clean[r.clone()], &out[r.start + latency..r.end + latency]);
            let cc: f64 = c.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
            let co: f64 = c.iter().zip(o).map(|(&a, &b)| f64::from(a) * f64::from(b)).sum();
            (cc > 0.0).then_some((co / cc) as f32)
        })
        .collect();
    if gains.is_empty() {
        return f32::NAN;
    }
    gains.sort_by(f32::total_cmp);
    db(gains[gains.len() / 2].max(1e-6))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Voice, pink_noise, speech_like, white_noise};

    #[test]
    fn the_measures_read_a_known_gain() {
        let len = 4 * SAMPLE_RATE as usize;
        let speech = speech_like(len, Voice::LOW, 3);
        let level = active_rms(&speech.samples);
        assert!(level > 0.0);
        // An output that is the speech at -6 dB, 480 samples late, with the noise 20 dB down.
        let noise = pink_noise(len, level * 0.3, 4);
        let lat = 480;
        let mut out = vec![0.0f32; len + lat];
        for i in 0..len {
            out[i + lat] = 0.5 * speech.samples[i] + 0.1 * noise[i];
        }
        let kept = speech_kept_db(&speech.samples, &out, lat, level);
        assert!((kept - db(0.5)).abs() < 0.5, "{kept}");
        let p = pauses(&speech.samples, level, lat, out.len());
        assert!(!p.is_empty());
        let cut = pause_cut(&noise, &out, &p, lat);
        assert!((cut.full_db - 20.0).abs() < 0.5 && (cut.low_db - 20.0).abs() < 0.5 && (cut.high_db - 20.0).abs() < 0.5, "{cut:?}");
        assert!(cut.seconds > 0.0);
    }

    #[test]
    fn the_band_split_is_parsevals() {
        let x = white_noise(48_000, 0.1, 9);
        let whole = 0..x.len();
        let (lo, hi) = band_power(&x, std::slice::from_ref(&whole));
        let total: f64 = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() * x.len() as f64;
        assert!(((lo + hi) / total - 1.0).abs() < 1e-9, "{lo} + {hi} vs {total}");
        // White noise: a third of its power is under 8 kHz.
        assert!((lo / (lo + hi) - 1.0 / 3.0).abs() < 0.02, "{}", lo / (lo + hi));
        assert_eq!(looped(&[1.0, 2.0], 5), [1.0, 2.0, 1.0, 2.0, 1.0]);
    }
}
