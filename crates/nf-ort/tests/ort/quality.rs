//! Quality on the fetched test audio, ignored (TODO 5's tests, item 7): each `speech_*` take at 10 dB SNR in DEMAND's cafe,
//! Freesound's keyboard (448209) and fan (242042), through the chain with RNNoise and with UL-UNAS (high band: the model
//! policy). It prints the noise's cut in the pauses (below and above 8 kHz too) and the speech kept, dumps WAVs to
//! `NF_STAGES_WAV_DIR`, and asserts loose bounds that catch a broken pipeline: GCC-PHAT at the chain's latency within 0.5
//! samples, a cut of 6 dB or more on average per noise, speech of -6 dB or more. The owner judges by ear.
//! `cargo test -p nf-ort --release --test ort -- --ignored quality --nocapture`.

use std::sync::Arc;

use nf_core::config::ProcessingConfig;
use nf_core::noise::{DEFAULT_DEPTH, Executor, NoHooks};
use nf_ort::SessionOptions;
use nf_stages::harness::audio::{
    CAFE, FAN, KEYBOARD, active_rms, audio_dir, looped, pause_cut, pauses, read, speech_kept_db, speech_takes,
};
use nf_stages::harness::{gcc_offset, max_lag_for, run_chain};
use nf_stages::synth::mix_at_snr;
use nf_stages::{build_chain, build_chain_with};

use crate::common::{self, controls, dump};

#[test]
#[ignore = "reads the fetched test-audio set and the pack; run with --ignored"]
fn quality_against_rnnoise_on_the_test_audio() {
    let Some(p) = common::pack() else { return };
    let Some(dir) = audio_dir() else { return };
    let takes = speech_takes(&dir);
    assert!(!takes.is_empty(), "no speech_* takes in {}", dir.display());
    let mut summary = Vec::new();
    for noise_name in [CAFE, KEYBOARD, FAN] {
        let noise = read(&dir.join(format!("{noise_name}.wav")));
        let mut cuts = [(0.0f32, 0usize); 2];
        for take in &takes {
            let mut speech = read(take);
            speech.resize(speech.len().div_ceil(480) * 480, 0.0);
            let level = active_rms(&speech);
            let (mix, n) = mix_at_snr(&speech, level, &looped(&noise, speech.len()), 10.0);
            let name = take.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_owned();
            for (m, model) in ["rnnoise", "ulunas"].iter().enumerate() {
                let c = controls(ProcessingConfig::default());
                let mut chain = if *model == "rnnoise" {
                    build_chain(c, false).expect("chain")
                } else {
                    let exec = Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: true };
                    let (stage, _) =
                        nf_ort::pack_stage(c.clone(), p.clone(), exec, SessionOptions::default(), DEFAULT_DEPTH).expect("stage");
                    build_chain_with(c, false, Box::new(stage)).expect("chain")
                };
                let lat = chain.latency() as usize;
                let y = run_chain(&mut chain, &mix, &[], 0);
                let mut longer = y.clone();
                longer.extend(run_chain(&mut chain, &vec![0.0; lat + 480], &[], 0));
                dump(&format!("q-{name}-{noise_name}-{model}"), &y, 48_000);
                let (d, conf) = gcc_offset(&mix, &y, max_lag_for(lat as u32)).expect("correlates");
                let pz = pauses(&speech, level, lat, longer.len());
                let cut = pause_cut(&n, &longer, &pz, lat);
                let kept = speech_kept_db(&speech, &longer, lat, level);
                eprintln!(
                    "{name:<18} {noise_name:<32} {model:<8} GCC {d:8.2} at {conf:4.1} dB (latency {lat}); pause cut {:5.1} dB (<8 kHz {:5.1}, \
                     >8 kHz {:5.1}) over {:.1} s; speech kept {kept:5.1} dB",
                    cut.full_db, cut.low_db, cut.high_db, cut.seconds
                );
                assert!((d - lat as f64).abs() < 0.5, "{name}, {noise_name}, {model}: GCC {d}, latency {lat}");
                assert!(kept > -6.0, "{name}, {noise_name}, {model}: speech kept {kept} dB");
                cuts[m].0 += cut.full_db;
                cuts[m].1 += 1;
            }
        }
        let mean = cuts.map(|(s, k)| s / k.max(1) as f32);
        summary.push(format!("{noise_name}: mean pause cut RNNoise {:.1} dB, UL-UNAS {:.1} dB", mean[0], mean[1]));
        assert!(mean.iter().all(|&m| m >= 6.0), "{noise_name}: mean cuts {mean:?}");
    }
    for s in summary {
        eprintln!("{s}");
    }
}
