//! Ignored tests on the fetched test-audio set (TODO 14's catalogue, `tools/fetch_test_audio.py`): real speech in real
//! noise through the chain, and a recorded echo pair through AEC3, each checking the stream offset by GCC-PHAT against
//! the declared latency and printing what the stage did. They read `NF_TEST_AUDIO`, or
//! `%LOCALAPPDATA%\noise-filter\test-audio\wav48`, skip with a message when it's missing, and never write there.
//! `cargo test -p nf-stages --release --test test_audio -- --ignored --nocapture`.

mod common;

use std::path::PathBuf;

use common::*;
use nf_core::BLOCK;
use nf_core::config::ProcessingConfig;
use nf_stages::harness::audio::{CAFE as NOISE, active_rms, audio_dir, pause_cut, pauses, read, speech_takes};
use nf_stages::harness::{chain_for, gcc_offset, max_lag_for, run_chain as run};
use nf_stages::synth::{db, mix_at_snr, rms};
use nf_stages::{AEC3_LATENCY, Aec3Stage, build_chain};
/// The echo pair: the mic (near end and echo) and its loopback reference.
const ECHO_MIC: &str = "aec_icassp2022_dt_00113069_mic";
const ECHO_REF: &str = "aec_icassp2022_dt_00113069_lpb";

#[test]
#[ignore = "reads the fetched test-audio set; run with --ignored"]
fn real_speech_in_cafe_noise_comes_out_at_the_chain_s_latency() {
    let Some(dir) = audio_dir() else { return };
    let noise = read(&dir.join(format!("{NOISE}.wav")));
    let takes: Vec<PathBuf> = speech_takes(&dir);
    assert!(!takes.is_empty(), "no speech_* takes in {}", dir.display());
    for take in takes {
        let speech = whole_blocks(read(&take));
        let n: Vec<f32> = noise.iter().copied().cycle().take(speech.len()).collect();
        let level = active_rms(&speech);
        let (mix, n) = mix_at_snr(&speech, level, &n, 10.0);
        let mut chain = build_chain(controls(ProcessingConfig::default()), false).expect("chain");
        let lat = chain.latency();
        let y = run(&mut chain, &mix, &[], 0);
        let name = take.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_owned();
        dump(&format!("take-{name}-cafe10-in"), &mix);
        dump(&format!("take-{name}-cafe10-out"), &y);
        let (d, conf) = gcc_offset(&mix, &y, max_lag_for(lat)).expect("correlates");
        // The pauses: blocks 40 dB under the take's active level, read from the output a latency later.
        let l = lat as usize;
        let p = pauses(&speech[..speech.len() - BLOCK], level, l, y.len());
        let cut = pause_cut(&n, &y, &p, l);
        eprintln!(
            "{name}: GCC-PHAT offset {d:.2} at {conf:.1} dB (chain latency {lat}); cafe noise cut by {:.1} dB in {:.1} s of pauses",
            cut.full_db, cut.seconds
        );
        assert!((d - f64::from(lat)).abs() < 0.5, "{name}: measured {d}, chain latency {lat}");
    }
}

#[test]
#[ignore = "reads the fetched test-audio set; run with --ignored"]
fn the_recorded_echo_pair_comes_out_at_aec3_s_latency() {
    let Some(dir) = audio_dir() else { return };
    let mic = whole_blocks(read(&dir.join(format!("{ECHO_MIC}.wav"))));
    let reference = read(&dir.join(format!("{ECHO_REF}.wav")));
    let c = controls(ProcessingConfig::default());
    let mut chain = chain_for(boxed(Aec3Stage::new(c.clone()).expect("AEC3")), c).expect("chain");
    assert_eq!(chain.latency(), AEC3_LATENCY);
    let y = run(&mut chain, &mic, &reference, 0);
    dump("take-echo-pair-in", &mic);
    dump("take-echo-pair-out", &y);
    let (d, conf) = gcc_offset(&mic, &y, max_lag_for(AEC3_LATENCY)).expect("correlates");
    let stats = chain.aec_stats().expect("AEC3 telemetry");
    let half = mic.len() / 2;
    let late = db(rms(&mic[half..]) / rms(&y[half..]));
    eprintln!(
        "echo pair: GCC-PHAT offset {d:.2} at {conf:.1} dB (declared {AEC3_LATENCY}); AEC3 reports delay {} ms, ERLE {:.1} dB; \
         the second half's level down {late:.1} dB",
        stats.echo_delay_ms, stats.erle_db
    );
    assert!((d - f64::from(AEC3_LATENCY)).abs() < 0.5, "measured {d}, declared {AEC3_LATENCY}");
}
