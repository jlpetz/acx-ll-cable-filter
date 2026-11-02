//! Offsets to the sample (TODO 5's tests, item 3), through the harness's checks: each layer's `check_model` (native 256,
//! bin-mapped 768, re-blocked 1440, pipelined 2400 at the default depth of 2, 1920 at depth 1), the stage's `check_latency`
//! (2400) and `check_profile`, the chain in all 8 modes (0, 2400, 430, 2830), and the stage's limits. GCC-PHAT on speech
//! always; impulses at four hop or block phases too with the high band passed through (`HighBand::Pass`: a click's content
//! above 8 kHz reaches the output unmasked, where the model's band may suppress it), so a framing error that depends on the
//! phase would show.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::Chain;
use nf_core::config::{HighBand, ProcessingConfig};
use nf_core::noise::{DEFAULT_DEPTH, Executor, ModelInfo, NoHooks, NoiseModel, Pipelined, Reblocked};
use nf_core::stage::{Activation, ProcessIo, Stage, StageError, StageRole, text};
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_ort::{NativeSpectral, OrtSpectralModel, Pack, SessionOptions};
use nf_stages::harness::{
    ParityCase, StageCheck, Tolerance, check_latency, check_model, check_profile, check_stage, gcc_offset, max_lag_for, peak_index,
    run_chain, run_stage, scenes,
};
use nf_stages::synth::Voice;
use nf_stages::{AEC3_LATENCY, build_chain_with};

use crate::common::{self, controls, model_only};

/// UL-UNAS on its worker at the default depth: the stage's latency (1440 re-blocked, + 2 × 480).
const L: u32 = 2400;

/// The model alone, the high band passed through (the impulse checks).
fn passing() -> ProcessingConfig {
    ProcessingConfig { high_band: HighBand::Pass, ..model_only() }
}

/// The native framing as a model, for `check_model` (test-only: the native framing isn't a `NoiseModel` in the crate).
struct Native {
    inner: NativeSpectral,
    info: ModelInfo,
}

impl NoiseModel for Native {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    fn prepare(&mut self) -> Result<(), StageError> {
        Ok(())
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        self.inner.process_hop(input, output);
        f32::NAN
    }
}

fn native(p: &Pack) -> Box<dyn NoiseModel> {
    let inner = NativeSpectral::new(p, SessionOptions::default()).expect("native");
    let info = ModelInfo {
        id: text("ulunas-native"),
        name: text("native"),
        hop: inner.hop(),
        latency_samples: inner.latency_samples() as u32,
        has_vad: false,
    };
    Box::new(Native { inner, info })
}

fn bin_mapped_with(p: &Pack, cfg: ProcessingConfig) -> Box<dyn NoiseModel> {
    Box::new(OrtSpectralModel::new(p, controls(cfg), SessionOptions::default()).expect("model"))
}

fn bin_mapped(p: &Pack) -> Box<dyn NoiseModel> {
    bin_mapped_with(p, model_only())
}

fn piped(p: &Pack, cfg: ProcessingConfig, depth: u32) -> Box<dyn NoiseModel> {
    let q = p.clone();
    let make = Box::new(move || {
        Ok(Box::new(OrtSpectralModel::new(&q, controls(cfg), SessionOptions::default()).map_err(StageError)?) as Box<dyn NoiseModel>)
    });
    Box::new(Pipelined::spawn(make, Executor::Inline, depth).expect("pipelined"))
}

#[test]
fn every_layer_s_offset_is_declared_to_the_sample() {
    let Some(p) = common::pack() else { return };
    let report = |what: &str, r: Result<nf_stages::harness::LatencyReport, String>, want: u32| {
        let r = r.unwrap_or_else(|e| panic!("{what}: {e}"));
        eprintln!("{what}: declared {}, GCC-PHAT {:.3} at {:.1} dB", r.declared, r.gcc_offset, r.gcc_confidence_db);
        assert_eq!(r.declared, want, "{what}");
    };
    report("native 512/256", check_model(&|| native(&p), None), 256);
    report("bin-mapped 1536/768", check_model(&|| bin_mapped(&p), None), 768);
    report("re-blocked", check_model(&|| Box::new(Reblocked::new(bin_mapped(&p))), None), 1440);
    report("pipelined, depth 2", check_model(&|| piped(&p, model_only(), DEFAULT_DEPTH), None), L);
    report("pipelined, depth 1", check_model(&|| piped(&p, model_only(), 1), None), 1920);
}

/// The same offsets by impulses at hop phases 17, hop / 2, 0 and hop - 1, of 0.5 and of 0.05, the high band passed through:
/// each layer's, and the stage's at block phases 17, 301, 0 and 479, inline and on a waited-for worker.
#[test]
fn impulses_at_every_phase_come_out_at_the_declared_offset() {
    let Some(p) = common::pack() else { return };
    for amp in [0.5f32, 0.05] {
        let report = |what: &str, r: Result<nf_stages::harness::LatencyReport, String>, want: u32| {
            let r = r.unwrap_or_else(|e| panic!("{what}, impulse {amp}: {e}"));
            eprintln!("{what}, impulse {amp}: declared {}, GCC-PHAT {:.3} at {:.1} dB", r.declared, r.gcc_offset, r.gcc_confidence_db);
            assert_eq!((r.declared, r.impulse_amp), (want, Some(amp)), "{what}");
        };
        report("bin-mapped", check_model(&|| bin_mapped_with(&p, passing()), Some(amp)), 768);
        report("re-blocked", check_model(&|| Box::new(Reblocked::new(bin_mapped_with(&p, passing()))), Some(amp)), 1440);
        report("pipelined", check_model(&|| piped(&p, passing(), DEFAULT_DEPTH), Some(amp)), L);
        for thread in [false, true] {
            let make = || common::boxed(common::stage(&p, passing(), thread));
            report(if thread { "the stage, worker" } else { "the stage, inline" }, check_latency(&make, Some(amp)), L);
        }
    }
}

#[test]
fn the_stage_passes_the_latency_and_profile_checks() {
    let Some(p) = common::pack() else { return };
    for thread in [false, true] {
        let make = || common::boxed(common::stage(&p, model_only(), thread));
        let r = check_latency(&make, None).unwrap_or_else(|e| panic!("thread {thread}: {e}"));
        eprintln!("UL-UNAS stage (worker {thread}): latency {}, GCC-PHAT {:.3} at {:.1} dB", r.declared, r.gcc_offset, r.gcc_confidence_db);
        assert_eq!(r.declared, L);
        assert_eq!(check_profile(&make), Ok(StageRole::Noise));
    }
}

/// UL-UNAS's stage through TODO 2's reusable check (`harness::check_stage`), inline and on a waited-for worker: the profile,
/// its 2400 at every block phase by GCC-PHAT on speech (and by impulses with the high band passed through, where a click
/// isn't the model's to suppress), finite through the hostile blocks and speech after them, and the parity hook: the stage on
/// its worker bit for bit the stage inline (what TODO 6's pipeline promises offline), on 4 s of noisy speech.
#[test]
fn the_stage_passes_the_stage_check_inline_and_on_its_worker() {
    let Some(p) = common::pack() else { return };
    let x = scenes::noisy_speech(4 * SAMPLE_RATE as usize, Voice::HIGH, 10.0, 6).mix;
    for (cfg, amp) in [(model_only(), None), (passing(), Some(0.5))] {
        let inline = run_stage(&mut common::stage(&p, cfg.clone(), false), &x, None).out;
        for thread in [false, true] {
            let parity =
                ParityCase { against: "the stage inline", input: &x, aux: None, expected: &inline, shift: 0, tolerance: Tolerance::Exact };
            let make = || common::boxed(common::stage(&p, cfg.clone(), thread));
            let r = check_stage(&make, &StageCheck { impulse_amp: amp, parity: Some(parity) })
                .unwrap_or_else(|e| panic!("worker {thread}, impulses {amp:?}: {e}"));
            eprintln!(
                "UL-UNAS stage (worker {thread}, impulses {amp:?}): {} at phases {:?}, hostile peak {}",
                r.latency.declared,
                r.phases.iter().map(|q| format!("{}: {:.3}", q.0, q.1)).collect::<Vec<_>>(),
                r.stress_peak
            );
            assert_eq!((r.role, r.latency.declared), (StageRole::Noise, L));
        }
    }
}

/// AEC3 then UL-UNAS on its (waited-for) worker, each on or off, through the models or in passthrough: the measured offset
/// is the reported latency. Through UL-UNAS, by GCC-PHAT on speech (an impulse may be suppressed); else by an impulse.
#[test]
fn the_chain_s_measured_offset_is_its_reported_latency_in_every_mode() {
    let Some(p) = common::pack() else { return };
    let speech = scenes::noisy_speech(6 * SAMPLE_RATE as usize, Voice::LOW, 30.0, 1).mix;
    for aec in [false, true] {
        for noise in [false, true] {
            for passthrough in [false, true] {
                let c = controls(ProcessingConfig { aec_enabled: aec, noise_enabled: noise, ..model_only() });
                c.passthrough.store(passthrough, Relaxed);
                let exec = Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: true };
                let (stage, _) = nf_ort::pack_stage(c.clone(), p.clone(), exec, SessionOptions::default(), DEFAULT_DEPTH).expect("stage");
                let mut chain: Chain = build_chain_with(c, true, Box::new(stage)).expect("chain");
                let what = format!("AEC {aec}, noise {noise}, passthrough {passthrough}");
                let reported = chain.latency();
                let expect = if aec { AEC3_LATENCY } else { 0 } + if noise { L } else { 0 };
                assert_eq!(reported, expect, "{what}");
                if noise && !passthrough {
                    let y = run_chain(&mut chain, &speech, &vec![0.0; speech.len()], 0);
                    let (d, conf) = gcc_offset(&speech, &y, max_lag_for(reported)).expect("correlates");
                    eprintln!("chain, {what}: GCC-PHAT {d:.3} at {conf:.1} dB, reported {reported}");
                    assert!((d - f64::from(reported)).abs() < 0.1 && conf > 25.0, "{what}: {d} at {conf}");
                } else {
                    let at = 8 * BLOCK + 301;
                    let mut mic = vec![0.0f32; 24 * BLOCK];
                    mic[at] = 0.5;
                    let y = run_chain(&mut chain, &mic, &vec![0.0; mic.len()], 0);
                    let measured = peak_index(&y).checked_sub(at);
                    eprintln!("chain, {what}: measured {measured:?}, reported {reported}");
                    assert_eq!(measured, Some(reported as usize), "{what}");
                }
            }
        }
    }
}

/// ±1e4 square waves (the stage's input limit), NaN, ±inf and 1e17: the output stays finite, the model's within ±1 (the
/// floor mix adds the dry path's ±1e4 × the floor's gain on top, unclamped: the engine clips its output), and it recovers.
#[test]
fn the_stage_stays_finite_and_clamped_at_its_limits() {
    let Some(p) = common::pack() else { return };
    let mut s = common::stage(&p, model_only(), true);
    let bound = 1.0 + nf_core::dsp::INPUT_LIMIT * nf_core::db_to_lin(model_only().floor_db);
    let mut out = [0.0f32; BLOCK];
    let mut worst = 0.0f32;
    for b in 0..200 {
        let x: [f32; BLOCK] = std::array::from_fn(|i| match b % 4 {
            0 => {
                if (i / 24) % 2 == 0 {
                    1e4
                } else {
                    -1e4
                }
            }
            1 => f32::NAN,
            2 => {
                if i % 7 == 0 {
                    f32::INFINITY
                } else {
                    1e17
                }
            }
            _ => (i as f32 * 0.05).sin() * 0.3,
        });
        s.process(ProcessIo::new(&[&x], &mut out));
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= bound), "block {b}: {:?}", out.iter().fold(0.0f32, |m, v| m.max(v.abs())));
        worst = out.iter().fold(worst, |m, v| m.max(v.abs()));
    }
    eprintln!("limits: the largest output sample {worst}");
    // And it is still a working stage afterwards: a reset, then speech comes through.
    s.reset();
    let x = scenes::noisy_speech(2 * SAMPLE_RATE as usize, Voice::LOW, 20.0, 3).mix;
    let mut energy = 0.0f64;
    for blk in x.as_chunks::<BLOCK>().0 {
        s.process(ProcessIo::new(&[blk], &mut out));
        energy += out.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    }
    assert!(energy > 1.0, "speech comes through after the abuse: {energy}");
    let mut fresh = common::stage(&p, ProcessingConfig::default(), false);
    assert!(fresh.activate(&Activation { sample_rate: 44_100, ..Activation::HOST }).is_err());
}
