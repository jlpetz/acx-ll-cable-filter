//! Offline harness, the v0 chain from `build_chain`: its reported latency against what passthrough
//! and live processing measure (TODO #13: the measured delay must equal the live configuration's).

mod common;

use std::sync::atomic::Ordering::Relaxed;

use common::*;
use nf_core::BLOCK;
use nf_core::config::ProcessingConfig;
use nf_core::latency::analysis::gcc_phat;
use nf_stages::synth::*;
use nf_stages::{AEC3_LATENCY, RNNOISE_LATENCY, build_chain};

const BOTH: u32 = AEC3_LATENCY + RNNOISE_LATENCY;

fn both_on() -> ProcessingConfig {
    ProcessingConfig { aec_enabled: true, noise_enabled: true, ..ProcessingConfig::default() }
}

#[test]
fn reported_latency_follows_the_enabled_stages() {
    let c = controls(both_on());
    let chain = build_chain(c.clone(), true).expect("chain");
    assert_eq!(chain.latency(), 910);
    assert_eq!(chain.latency(), BOTH);
    c.aec.enabled.store(false, Relaxed);
    assert_eq!(chain.latency(), RNNOISE_LATENCY);
    c.noise.enabled.store(false, Relaxed);
    assert_eq!(chain.latency(), 0);
    c.aec.enabled.store(true, Relaxed);
    assert_eq!(chain.latency(), AEC3_LATENCY);
    // Without an AEC stage the AEC switch adds nothing.
    let c = controls(both_on());
    assert_eq!(build_chain(c, false).expect("chain").latency(), RNNOISE_LATENCY);
}

#[test]
fn huge_finite_mic_samples_come_out_finite() {
    // The engine only zeros non-finite capture samples, so a garbage float buffer can hold 1e17 or f32::MAX. RNNoise's
    // ×32768 overflowed those to inf, and the NaN it became aborted inside easyfft; the noise stage now clamps its input
    // first. With and without AEC3, the noise filter switched off and on again (its reset and priming see them too).
    for with_aec in [false, true] {
        let c = controls(both_on());
        let mut chain = build_chain(c.clone(), with_aec).expect("chain");
        let mut mic = speech_like(3 * SR, Voice::HIGH, 3).samples;
        for (k, i) in (BLOCK..mic.len()).step_by(997).enumerate() {
            mic[i] = [1e17, f32::MAX, -f32::MAX, 3e38][k % 4];
        }
        let far = white_noise(mic.len(), 0.05, 2);
        let mut y = run_chain(&mut chain, &mic[..SR], &far[..SR]);
        c.noise.enabled.store(false, Relaxed);
        y.extend(run_chain(&mut chain, &mic[SR..2 * SR], &far[SR..2 * SR]));
        c.noise.enabled.store(true, Relaxed);
        y.extend(run_chain(&mut chain, &mic[2 * SR..], &far[2 * SR..]));
        assert!(y.iter().all(|v| v.is_finite()), "with_aec {with_aec}");
    }
}

#[test]
fn passthrough_delays_an_impulse_by_the_reported_latency() {
    for (with_aec, cfg) in [(true, both_on()), (false, ProcessingConfig::default())] {
        let c = controls(cfg);
        c.passthrough.store(true, Relaxed);
        let mut chain = build_chain(c, with_aec).expect("chain");
        let mut mic = vec![0.0f32; 12 * BLOCK];
        let at = 3 * BLOCK + 123; // after the switch-in blocks
        mic[at] = 0.8;
        let reference = white_noise(mic.len(), 0.1, 1); // passthrough ignores it
        let y = run_chain(&mut chain, &mic, &reference);
        let peak = peak_index(&y);
        assert_eq!(peak - at, chain.latency() as usize, "with_aec {with_aec}");
        assert_eq!(y[peak], 0.8, "passthrough is a pure delay");
        assert!(y.iter().enumerate().all(|(i, &v)| i == peak || v == 0.0));
    }
}

#[test]
fn live_processing_delays_speech_by_the_reported_latency() {
    // Through the models (silent reference), GCC-PHAT on speech finds the reported offset. Plain
    // cross-correlation doesn't here: AEC3's high-pass delays voiced speech's low harmonics by ~30
    // samples, which pulls a waveform correlation late.
    let len = 8 * SR;
    let sp = speech_like(len, Voice::LOW, 1);
    let (x, _) = mix_at_snr(&sp.samples, sp.active_rms(), &pink_noise(len, 1.0, 7), 30.0);
    let silent = vec![0.0f32; len];
    for aec_on in [true, false] {
        let c = controls(ProcessingConfig { aec_enabled: aec_on, floor_db: -100.0, ..both_on() });
        let mut chain = build_chain(c, true).expect("chain");
        let y = run_chain(&mut chain, &x, &silent);
        let (d, conf) = gcc_phat(&x, &y, 2000, 0.75).expect("correlates");
        assert!((d - f64::from(chain.latency())).abs() < 0.1, "AEC {aec_on}: measured {d}, reported {}", chain.latency());
        assert!(conf > 25.0, "confidence {conf}");
    }
}

#[test]
fn both_stages_together_cancel_echo_and_noise() {
    // Echo of a far-end talker plus pink noise at the mic: AEC3 removes the echo, RNNoise the noise.
    let len = 10 * SR;
    let far = speech_like(len, Voice::LOW, 2);
    let rir = room_impulse_response(9600, 0.15, 3);
    let echo = scaled(&delayed(&convolve(&far.samples, &rir)[..len], 1920), 0.5);
    let noise = pink_noise(len, 10f32.powf(-50.0 / 20.0), 4);
    let mic = add(&echo, &noise);
    let mut chain = build_chain(controls(both_on()), true).expect("chain");
    let y = run_chain(&mut chain, &mic, &far.samples);
    dump("chain-mic", &mic);
    dump("chain-out", &y);
    let lat = BOTH as usize;
    let span = 3 * SR..len - lat;
    let cut = db(rms(&mic[span.clone()]) / rms(&y[span.start + lat..span.end + lat]));
    assert!(cut >= 20.0, "echo plus noise down by only {cut:.1} dB");
    let aec = chain.aec_stats().expect("AEC stats");
    assert!(aec.erle_db > 6.0 && (32.0..=44.0).contains(&aec.echo_delay_ms), "{aec:?}");
    assert_eq!(build_chain(controls(both_on()), false).expect("chain").aec_stats(), None, "no AEC slot, no AEC stats");
}
