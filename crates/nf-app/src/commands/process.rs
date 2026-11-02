//! `process IN.wav OUT.wav`: the same `Chain` and stages as the engine (`nf_stages::build_chain`), offline, run
//! block by block by TODO 2's harness (`nf_stages::harness::run_chain`). The input (and the
//! `--ref` reference) must be 48 kHz; more channels are averaged to mono. The output is 48 kHz mono
//! float, as long as the input. By default it is the stream the live app would send, so it lags the
//! input by the chain's latency, which is printed; `--align` removes that lag.
//!
//! The chain starts fresh, where the engine first runs three dithered silent blocks through it
//! (`nf_engine::warm_up_chain`). `--warm-up` does the same, so a chain recording from `quality --save`
//! (`chain_mic.wav`, `chain_ref.wav`, with `chain_config.json` as `--config`) reruns into exactly
//! `chain_out.wav`. The input isn't dithered here: the recording's blocks already are.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nf_core::SAMPLE_RATE;
use nf_core::controls::ChainControls;
use nf_engine::ProcessingConfig;
use nf_stages::harness::run_chain;

use super::fixed;
use crate::cli::ProcessArgs;
use crate::exit::Failure;
use crate::{config, wav};

pub fn process(args: &ProcessArgs) -> Result<(), Failure> {
    let done = process_files(args)?;
    for w in &done.warnings {
        say!("warning: {w}");
    }
    let seconds = done.frames as f64 / f64::from(SAMPLE_RATE);
    let speed = seconds / done.elapsed.as_secs_f64().max(1e-9);
    say!(
        "Wrote {}: {} s, 48 kHz mono float. Chain latency {} samples ({} ms){}. Processed in {} s ({}x real time).",
        args.output.display(),
        fixed(seconds, 2),
        done.latency,
        fixed(f64::from(done.latency) * 1000.0 / f64::from(SAMPLE_RATE), 2),
        if args.align { ", removed (--align)" } else { "" },
        fixed(done.elapsed.as_secs_f64(), 2),
        fixed(speed, 0),
    );
    Ok(())
}

/// What a `process` run did.
#[derive(Clone, Debug, PartialEq)]
pub struct Processed {
    /// Frames written (the input's length).
    pub frames: usize,
    /// The chain's stream offset in samples.
    pub latency: u32,
    pub elapsed: Duration,
    pub warnings: Vec<String>,
}

/// Reads, processes and writes; the testable body of `process`.
pub fn process_files(args: &ProcessArgs) -> Result<Processed, Failure> {
    let mut cfg = match &args.config {
        Some(path) => config::load(path)?.processing,
        None => ProcessingConfig::default(),
    };
    args.processing.apply(&mut cfg);
    let mut warnings = Vec::new();
    let input = wav::read_mono_48k(&args.input)?;
    if input.channels > 1 {
        warnings.push(format!("{} has {} channels: averaged to mono", args.input.display(), input.channels));
    }
    let reference = match &args.reference {
        Some(path) if cfg.aec_enabled => Some(wav::read_mono_48k(path)?.samples),
        Some(path) => {
            warnings.push(format!("{} is ignored: echo cancellation is off (add --aec)", path.display()));
            None
        }
        None => {
            if cfg.aec_enabled {
                warnings.push("no --ref: echo cancellation runs on a silent reference".to_owned());
            }
            None
        }
    };
    if let Some(r) = &reference
        && r.len() < input.samples.len()
    {
        warnings.push(format!("the reference is {} samples shorter than the input: padded with silence", input.samples.len() - r.len()));
    }

    let start = Instant::now();
    let controls = Arc::new(ChainControls::new(&cfg));
    let mut chain = nf_stages::build_chain(controls, cfg.aec_enabled)
        .map_err(|e| Failure::error(format!("Couldn't set up the processing chain: {e}")))?;
    if args.warm_up {
        nf_engine::warm_up_chain(&mut chain);
    }
    let latency = chain.latency();
    let skip = if args.align { latency as usize } else { 0 };
    let out = run_chain(&mut chain, &input.samples, reference.as_deref().unwrap_or(&[]), skip);
    let elapsed = start.elapsed();
    write_output(&args.output, &out)?;
    Ok(Processed { frames: out.len(), latency, elapsed, warnings })
}

fn write_output(path: &Path, out: &[f32]) -> Result<(), Failure> {
    wav::write_mono_f32(path, out)
}

#[cfg(test)]
mod tests {
    use nf_core::latency::analysis::gcc_phat;
    use nf_stages::synth::{Voice, add, delayed, pink_noise, rms, scaled, speech_like};

    use super::*;
    use crate::cli::Processing;
    use crate::exit::Exit;

    fn args(dir: &Path, processing: Processing) -> ProcessArgs {
        ProcessArgs {
            input: dir.join("in.wav"),
            output: dir.join("out.wav"),
            reference: None,
            config: None,
            processing,
            align: false,
            warm_up: false,
        }
    }

    fn read(path: &Path) -> Vec<f32> {
        let r = hound::WavReader::open(path).unwrap();
        assert_eq!(r.spec(), wav::SPEC_F32, "the output is 48 kHz mono float");
        r.into_samples::<f32>().map(Result::unwrap).collect()
    }

    /// 3 s of a speech-like voice over pink noise.
    fn noisy_speech() -> Vec<f32> {
        let len = 3 * SAMPLE_RATE as usize + 123; // not whole blocks
        add(&speech_like(len, Voice::HIGH.at(-26.0), 7).samples, &pink_noise(len, 0.003, 8))
    }

    #[test]
    fn with_both_stages_off_the_output_is_the_input() {
        let dir = crate::test_dir("process-bypass");
        let x = noisy_speech();
        wav::write_mono_f32(&dir.join("in.wav"), &x).unwrap();
        let done = process_files(&args(&dir, Processing { noise: Some(false), ..Default::default() })).unwrap();
        assert_eq!((done.frames, done.latency), (x.len(), 0));
        assert_eq!(read(&dir.join("out.wav")), x);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rnnoise_output_lags_by_its_latency_unless_aligned() {
        let dir = crate::test_dir("process-rnnoise");
        let x = noisy_speech();
        wav::write_mono_f32(&dir.join("in.wav"), &x).unwrap();
        let mut a = args(&dir, Processing::default());
        let done = process_files(&a).unwrap();
        assert_eq!((done.frames, done.latency), (x.len(), nf_stages::RNNOISE_LATENCY));
        assert!(done.warnings.is_empty(), "{:?}", done.warnings);
        let raw = read(&dir.join("out.wav"));
        assert_eq!(raw.len(), x.len());
        assert!(raw.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        // The model changed the signal, and the stream lags by 480 samples
        assert_ne!(raw, x);
        let (lag, _) = gcc_phat(&x, &raw, 2000, 0.75).unwrap();
        assert!((lag - 480.0).abs() < 1.0, "lag {lag}");

        a.align = true;
        let done = process_files(&a).unwrap();
        assert_eq!(done.frames, x.len());
        let aligned = read(&dir.join("out.wav"));
        let (lag, _) = gcc_phat(&x, &aligned, 2000, 0.75).unwrap();
        assert!(lag.abs() < 1.0, "aligned lag {lag}");
        // Aligned is the raw stream moved 480 samples earlier
        assert_eq!(aligned[..x.len() - 480], raw[480..]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stereo_int_input_is_averaged_and_the_settings_apply() {
        let dir = crate::test_dir("process-stereo");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(dir.join("in.wav"), spec).unwrap();
        for i in 0..4800 {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        // A config file turns the noise filter off; the command line can turn it back on
        std::fs::write(dir.join("cfg.json"), r#"{"processing": {"noise_enabled": false}}"#).unwrap();
        let mut a = args(&dir, Processing::default());
        a.config = Some(dir.join("cfg.json"));
        let done = process_files(&a).unwrap();
        assert_eq!((done.frames, done.latency), (4800, 0));
        assert_eq!(done.warnings, vec![format!("{} has 2 channels: averaged to mono", dir.join("in.wav").display())]);
        let out = read(&dir.join("out.wav"));
        assert!((out[10] - ((10.0f32 * 0.05).sin() * 8000.0).trunc() / 32768.0).abs() < 1e-6);
        a.processing.noise = Some(true);
        assert_eq!(process_files(&a).unwrap().latency, nf_stages::RNNOISE_LATENCY);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_reference_cancels_its_echo() {
        let dir = crate::test_dir("process-aec");
        let len = 6 * SAMPLE_RATE as usize;
        let far = speech_like(len, Voice::LOW, 11).samples;
        // The mic hears only the far end, 20 ms later at -6 dB
        let mic = scaled(&delayed(&far, 960), 0.5);
        wav::write_mono_f32(&dir.join("in.wav"), &mic).unwrap();
        wav::write_mono_f32(&dir.join("far.wav"), &far[..len - 1000]).unwrap();
        let mut a = args(&dir, Processing { aec: Some(true), noise: Some(false), ..Default::default() });
        // Without the reference AEC3 has nothing to cancel
        let done = process_files(&a).unwrap();
        assert_eq!(done.latency, nf_stages::AEC3_LATENCY);
        assert_eq!(done.warnings, vec!["no --ref: echo cancellation runs on a silent reference".to_owned()]);
        let tail = |x: &[f32]| rms(&x[3 * SAMPLE_RATE as usize..]);
        let unreferenced = tail(&read(&dir.join("out.wav")));
        a.reference = Some(dir.join("far.wav"));
        let done = process_files(&a).unwrap();
        assert_eq!(done.warnings, vec!["the reference is 1000 samples shorter than the input: padded with silence".to_owned()]);
        let cancelled = tail(&read(&dir.join("out.wav")));
        let reduction_db = 20.0 * (unreferenced / cancelled.max(1e-12)).log10();
        assert!(reduction_db > 15.0, "echo reduced by only {reduction_db:.1} dB");
        // A reference without --aec is ignored, with a warning
        a.processing.aec = Some(false);
        let done = process_files(&a).unwrap();
        assert_eq!(done.latency, 0);
        assert!(done.warnings[0].ends_with("is ignored: echo cancellation is off (add --aec)"), "{:?}", done.warnings);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `--warm-up` starts the chain as the engine does: the output is what a chain warmed up by
    /// `nf_engine::warm_up_chain` and fed the input block by block gives, sample for sample, and
    /// not what a fresh chain gives (AEC3 and RNNoise both hold the warm-up's state).
    #[test]
    fn warm_up_starts_the_chain_as_the_engine_does() {
        let dir = crate::test_dir("process-warm-up");
        let far = speech_like(2 * SAMPLE_RATE as usize, Voice::LOW, 11).samples;
        let mic = add(&scaled(&delayed(&far, 960), 0.5), &noisy_speech()[..far.len()]);
        wav::write_mono_f32(&dir.join("in.wav"), &mic).unwrap();
        wav::write_mono_f32(&dir.join("far.wav"), &far).unwrap();
        let mut a = args(&dir, Processing { aec: Some(true), ..Default::default() });
        a.reference = Some(dir.join("far.wav"));
        a.warm_up = true;
        process_files(&a).unwrap();
        let warm = read(&dir.join("out.wav"));
        let cfg = ProcessingConfig { aec_enabled: true, ..Default::default() };
        let mut chain = nf_stages::build_chain(Arc::new(ChainControls::new(&cfg)), true).unwrap();
        nf_engine::warm_up_chain(&mut chain);
        assert_eq!(warm, run_chain(&mut chain, &mic, &far, 0), "the engine's start, sample for sample");
        a.warm_up = false;
        process_files(&a).unwrap();
        assert_ne!(read(&dir.join("out.wav")), warm, "a fresh chain starts elsewhere");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_inputs_fail_with_a_file_error() {
        let dir = crate::test_dir("process-bad");
        let a = args(&dir, Processing::default());
        assert_eq!(process_files(&a).unwrap_err().exit, Exit::Error);
        let spec = hound::WavSpec { channels: 1, sample_rate: 16_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        hound::WavWriter::create(dir.join("in.wav"), spec).unwrap().finalize().unwrap();
        let e = process_files(&a).unwrap_err();
        assert!(e.message.contains("16000 Hz"), "{}", e.message);
        // An output folder that doesn't exist
        wav::write_mono_f32(&dir.join("in.wav"), &[0.0; 100]).unwrap();
        let e = process_files(&ProcessArgs { output: dir.join("no").join("such").join("out.wav"), ..a }).unwrap_err();
        assert!(e.message.starts_with("Couldn't write"), "{}", e.message);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_input_gives_an_empty_output() {
        let dir = crate::test_dir("process-empty");
        wav::write_mono_f32(&dir.join("in.wav"), &[]).unwrap();
        let mut a = args(&dir, Processing::default());
        a.align = true;
        assert_eq!(process_files(&a).unwrap().frames, 0);
        assert!(read(&dir.join("out.wav")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
