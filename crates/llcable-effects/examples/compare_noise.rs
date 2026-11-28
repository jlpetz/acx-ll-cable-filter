//! Compare the two native denoisers on one recorded speech/fan mixture without opening a device.
//! Fetch the project's test audio first, then `cargo run --release -p llcable-effects --example compare_noise`.
//! `NF_TEST_AUDIO` overrides the default 48 kHz WAV folder. Packet timings measure this machine's wall clock.

use std::path::Path;
use std::time::{Duration, Instant};

use llcable_effects::{noise_reduction, sonora_noise_reduction};
use llcable_routing::{Bus, Effect, Input, Router, Send, Tap};
use nf_core::SAMPLE_RATE;
use nf_stages::harness::audio::{FAN, audio_dir, read_wav};
use nf_stages::synth::{db, rms};

const SR: usize = SAMPLE_RATE as usize;
const PACKET: usize = 128;
const LEAD: usize = 3 * SR;
const SPEECH: usize = 8 * SR;

fn take(dir: &Path, stem: &str, frames: usize) -> Result<Vec<f32>, String> {
    let (mut samples, rate) = read_wav(&dir.join(format!("{stem}.wav")))?;
    if rate != SAMPLE_RATE || samples.len() < frames {
        return Err(format!("{stem} needs at least {frames} samples at {SAMPLE_RATE} Hz"));
    }
    samples.truncate(frames);
    Ok(samples)
}

fn projection(clean: &[f32], samples: &[f32], step: usize) -> f64 {
    clean.iter().step_by(step).zip(samples.iter().step_by(step)).map(|(&x, &y)| f64::from(x) * f64::from(y)).sum()
}

fn compare(name: &str, effect: Effect, mix: &[f32], speech: &[f32]) -> Result<(), Box<dyn std::error::Error>> {
    let bus = Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![effect] };
    let mut router = Router::new(SAMPLE_RATE, PACKET, 1, vec![bus])?;
    router.prepare_thread();
    let mut output = Vec::with_capacity(mix.len());
    let mut timings = Vec::with_capacity(mix.len().div_ceil(PACKET));
    for packet in mix.chunks(PACKET) {
        let start = Instant::now();
        router.process(&[Input::Mono(packet)])?;
        timings.push(start.elapsed());
        output.extend(router.output(0).unwrap().iter().map(|f| f[0]));
    }

    // Ignore startup and let the noise estimator adapt before measuring a full second of fan alone.
    let noise_reduction = db(rms(&mix[SR..2 * SR]) / rms(&output[SR..2 * SR]));
    // Estimate delay on voice after onset. Decimate the correlation sum and test all lags from 0 to 40 ms.
    let max_lag = SR / 25;
    let start = LEAD + SR / 10;
    let end = mix.len() - max_lag;
    let clean = &speech[start - LEAD..end - LEAD];
    let lag = (0..=max_lag)
        .map(|lag| (lag, projection(clean, &output[start + lag..end + lag], 12)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0;
    let speech_db = 20.0 * (projection(clean, &output[start + lag..end + lag], 1) / projection(clean, &mix[start..end], 1)).abs().log10();
    let busy = 100.0 * timings.iter().sum::<Duration>().as_secs_f64() / (mix.len() as f64 / SR as f64);
    timings.sort_unstable();
    let percentile = |p: usize| timings[timings.len() * p / 100].as_secs_f64() * 1e6;
    println!(
        "{name}: fan reduction {noise_reduction:.1} dB, speech projection {speech_db:.1} dB, delay {lag} frames, \
         processing {busy:.2}% of real time, packet p50 {:.1} µs, p99 {:.1} µs",
        percentile(50),
        percentile(99)
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = audio_dir().ok_or("Run tools/fetch_test_audio.py to prepare the 48 kHz WAVs")?;
    let speech = take(&dir, "speech_tsp_ma01", SPEECH)?;
    let noise = take(&dir, FAN, LEAD + SPEECH)?;
    let speech_gain = 0.09 / rms(&speech);
    let noise_gain = 0.05 / rms(&noise);
    let mut mix: Vec<_> = noise.iter().map(|x| x * noise_gain).collect();
    for (mix, voice) in mix[LEAD..].iter_mut().zip(&speech) {
        *mix += speech_gain * voice;
    }
    compare("RNNoise", noise_reduction(), &mix, &speech)?;
    compare("Sonora NS Moderate", sonora_noise_reduction()?, &mix, &speech)
}
