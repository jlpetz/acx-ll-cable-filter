//! WAV in and out for `process` and `bench` (doc/dev/api/realfft-hound.md section 2). Offline only:
//! hound does blocking I/O.

use std::path::Path;

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use nf_core::SAMPLE_RATE;

use crate::exit::Failure;

/// A file read as the app's format.
#[derive(Clone, Debug, PartialEq)]
pub struct Mono {
    /// 48 kHz mono in ±1: channels averaged, ints scaled by 2^-(bits-1).
    pub samples: Vec<f32>,
    /// The file's channel count, before the downmix.
    pub channels: u16,
}

/// Reads a 48 kHz WAV (16/24/32-bit int, 8-bit, or 32-bit float, any channel count) as mono.
pub fn read_mono_48k(path: &Path) -> Result<Mono, Failure> {
    let fail = |e: String| Failure::error(format!("{}: {e}", path.display()));
    let mut r = WavReader::open(path).map_err(|e| fail(e.to_string()))?;
    let spec = r.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(fail(format!(
            "it is {} Hz, and noise-filter works at {SAMPLE_RATE} Hz only; resample it first (e.g. ffmpeg -i IN -ar 48000 OUT.wav)",
            spec.sample_rate
        )));
    }
    let channels = spec.channels;
    if channels == 0 {
        return Err(fail("it has no channels".into()));
    }
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => r.samples::<f32>().collect::<Result<_, _>>(),
        (SampleFormat::Int, bits @ 8..=32) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()
        }
        (format, bits) => return Err(fail(format!("{bits}-bit {format:?} samples aren't supported"))),
    }
    .map_err(|e| fail(e.to_string()))?;
    Ok(Mono { samples: downmix(&interleaved, usize::from(channels)), channels })
}

/// The average of each frame's `channels` samples (a trailing partial frame is dropped).
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        0 => Vec::new(),
        1 => interleaved.to_vec(),
        n => {
            let scale = 1.0 / n as f32;
            interleaved.chunks_exact(n).map(|frame| frame.iter().sum::<f32>() * scale).collect()
        }
    }
}

/// The app's output format: 48 kHz mono 32-bit float.
pub const SPEC_F32: WavSpec = WavSpec { channels: 1, sample_rate: SAMPLE_RATE, bits_per_sample: 32, sample_format: SampleFormat::Float };

/// Writes `x` as a 48 kHz mono float WAV.
pub fn write_mono_f32(path: &Path, x: &[f32]) -> Result<(), Failure> {
    let fail = |e: hound::Error| Failure::error(format!("Couldn't write {}: {e}", path.display()));
    let mut w = WavWriter::create(path, SPEC_F32).map_err(fail)?;
    for &s in x {
        w.write_sample(s).map_err(fail)?;
    }
    // Not left to Drop, which ignores errors
    w.finalize().map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, spec: WavSpec, frames: &[f32]) {
        let mut w = WavWriter::create(path, spec).unwrap();
        for &s in frames {
            match (spec.sample_format, spec.bits_per_sample) {
                (SampleFormat::Float, _) => w.write_sample(s).unwrap(),
                (SampleFormat::Int, 16) => w.write_sample((s * 32767.0).round() as i16).unwrap(),
                (SampleFormat::Int, bits) => w.write_sample((s * ((1i64 << (bits - 1)) - 1) as f32).round() as i32).unwrap(),
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn float_mono_round_trips_exactly() {
        let dir = crate::test_dir("wav-f32");
        let path = dir.join("x.wav");
        let x: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        write_mono_f32(&path, &x).unwrap();
        let m = read_mono_48k(&path).unwrap();
        assert_eq!((m.samples, m.channels), (x, 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ints_of_every_width_scale_to_unit_range_and_channels_average() {
        let dir = crate::test_dir("wav-int");
        for bits in [8u16, 16, 24, 32] {
            let path = dir.join(format!("i{bits}.wav"));
            let spec = WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: bits, sample_format: SampleFormat::Int };
            // Left 0.5, right -0.25, for 10 frames
            write(&path, spec, &[0.5, -0.25].repeat(10));
            let m = read_mono_48k(&path).unwrap();
            assert_eq!((m.samples.len(), m.channels), (10, 2), "{bits}");
            let tolerance = if bits == 8 { 1.0 / 64.0 } else { 1e-4 };
            assert!(m.samples.iter().all(|&v| (v - 0.125).abs() < tolerance), "{bits}: {:?}", m.samples);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_rates_and_formats_are_refused_with_the_reason() {
        let dir = crate::test_dir("wav-refused");
        let path = dir.join("cd.wav");
        write(&path, WavSpec { channels: 1, sample_rate: 44_100, bits_per_sample: 16, sample_format: SampleFormat::Int }, &[0.0; 10]);
        let e = read_mono_48k(&path).unwrap_err();
        assert!(e.message.contains("44100 Hz") && e.message.contains("48000 Hz only"), "{}", e.message);
        let e = read_mono_48k(&dir.join("missing.wav")).unwrap_err();
        assert!(e.message.contains("missing.wav"), "{}", e.message);
        std::fs::write(dir.join("junk.wav"), b"not a wav file at all").unwrap();
        assert!(read_mono_48k(&dir.join("junk.wav")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn downmix_averages_whole_frames() {
        assert_eq!(downmix(&[1.0, 3.0, 5.0, 7.0, 9.0], 2), vec![2.0, 6.0]);
        assert_eq!(downmix(&[1.0, 2.0, 3.0], 3), vec![2.0]);
        assert_eq!(downmix(&[1.0, 2.0], 1), vec![1.0, 2.0]);
        assert!(downmix(&[1.0], 0).is_empty());
    }
}
