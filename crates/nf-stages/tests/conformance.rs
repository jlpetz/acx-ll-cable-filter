//! TODO 2's harness checks on every built-in stage, through the CLAP-shaped interface (TODO 17), with no devices. The
//! checks any stage or pack must pass are `nf_stages::harness`'s (`check_latency`, `check_model`, `check_profile`); this
//! file runs them on ours, and adds what is ours alone:
//! - **check 1, latency conformance, to the sample:** each stage passes `check_latency` (its impulse offset is its
//!   declared `latency()` at four block phases, and GCC-PHAT on speech agrees to a tenth of a sample); the RNNoise model
//!   passes `check_model` hop by hop (`latency_samples - hop`); the chain's measured offset is `chain.latency()` in every
//!   on/off/passthrough combination; and the re-blocker's latency formula holds at every hop, through a chain, as the
//!   engine calls a stage;
//! - **check 2, parity:** the noise stage around RNNoise is bit-equal to nnnoiseless driven directly (AEC3's parity with
//!   sonora is a unit test in `src/aec3.rs`, which can see the stage's config and dither seed);
//! - **check 4, the host profile and the interface:** each stage passes `check_profile` (the profile, activation, the
//!   params' clamping, the state round trip, an idle reset); and ours refuse other formats, have their param defaults,
//!   tolerate unknown ids in a state, and have their own extensions.
//!
//! Check 3 (CPU per frame) is `tests/timing.rs` and `noise-filter bench`. Measured figures go to stderr (`--nocapture`).

mod common;

use std::sync::atomic::Ordering::Relaxed;

use common::*;
use nf_core::BLOCK;
use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::noise::{DelayModel, NoiseStage, PARAMS, param};
use nf_core::stage::{Activation, ParamError, Stage, StageRole, check_host_profile, state};
use nf_stages::harness::{self, chain_for, check_latency, check_model, check_profile, parity, scenes};
use nf_stages::synth::{Voice, white_noise};
use nf_stages::{AEC3_LATENCY, RNNOISE_INFO, RNNOISE_LATENCY, RnnoiseModel, build_chain};
use nnnoiseless::DenoiseState;

/// Floor off and gate off: the model alone.
fn model_only() -> ProcessingConfig {
    ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() }
}

/// A built-in stage: its name, a maker (built and activated), and the impulse amplitude its offset is measured at.
type Builtin = (&'static str, fn() -> Box<dyn Stage>, f32);

/// Every built-in stage: RNNoise at 1e-6 (below its silence threshold, so its gains stay at 1 and the framing shows alone),
/// AEC3 at 0.5 (with a silent reference: the checks give a second input silence).
fn stages() -> [Builtin; 2] {
    fn make_rnnoise() -> Box<dyn Stage> {
        boxed(rnnoise(model_only()))
    }
    fn make_aec3() -> Box<dyn Stage> {
        boxed(aec3())
    }
    [("RNNoise", make_rnnoise, 1e-6), ("AEC3", make_aec3, 0.5)]
}

// ---- Check 1: latency conformance -----------------------------------------------------------------------------------

#[test]
fn every_stage_passes_the_latency_check() {
    for (name, make, amp) in stages() {
        let r = check_latency(&make, Some(amp)).unwrap_or_else(|e| panic!("{e}"));
        eprintln!(
            "{name}: impulse offset {} at block phases 17, 301, 0 and 479; GCC-PHAT {:.3} samples at {:.1} dB",
            r.declared, r.gcc_offset, r.gcc_confidence_db
        );
    }
    assert_eq!(stages().iter().map(|s| (s.1)().latency()).collect::<Vec<_>>(), [RNNOISE_LATENCY, AEC3_LATENCY]);
}

#[test]
fn the_rnnoise_model_passes_the_latency_check_hop_by_hop() {
    let r = check_model(&|| Box::new(RnnoiseModel::new()), Some(1e-6)).unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "RNNoise model: offset {} hop by hop (latency_samples {} - hop {}); GCC-PHAT {:.3} at {:.1} dB",
        r.declared, RNNOISE_INFO.latency_samples, RNNOISE_INFO.hop, r.gcc_offset, r.gcc_confidence_db
    );
    assert_eq!(r.declared, 480);
}

#[test]
fn the_chain_s_measured_offset_is_its_reported_latency_in_every_mode() {
    for aec in [false, true] {
        for noise in [false, true] {
            for passthrough in [false, true] {
                let c = controls(ProcessingConfig { aec_enabled: aec, noise_enabled: noise, ..model_only() });
                c.passthrough.store(passthrough, Relaxed);
                let mut chain = build_chain(c, true).expect("chain");
                let at = 6 * BLOCK + 301;
                // Through RNNoise, an impulse under its silence threshold; through AEC3 alone or a delay line, a loud one.
                let amp = if noise && !passthrough { 1e-6 } else { 0.5 };
                let mut mic = vec![0.0f32; 16 * BLOCK];
                mic[at] = amp;
                let y = run_chain(&mut chain, &mic, &vec![0.0; mic.len()]);
                let measured = peak_index(&y).checked_sub(at);
                let what = format!("AEC {aec}, noise {noise}, passthrough {passthrough}");
                eprintln!("chain, {what}: measured {measured:?}, reported {}", chain.latency());
                assert_eq!(measured, Some(chain.latency() as usize), "{what}");
                let expect = if aec { AEC3_LATENCY } else { 0 } + if noise { RNNOISE_LATENCY } else { 0 };
                assert_eq!(chain.latency(), expect, "{what}");
            }
        }
    }
}

#[test]
fn the_reblocker_s_latency_holds_through_the_chain_at_every_hop() {
    // A pure-delay model at each hop, with and without a frame of lookahead, alone in the noise slot as the engine runs
    // it: the latency follows `(latency_samples - hop) + (hop - gcd(hop, 480))`, the output is the input delayed by it
    // sample for sample, and a 0 dB floor gives exactly the delayed dry signal.
    let x = white_noise(60 * BLOCK, 0.1, 3);
    for hop in [160usize, 240, 256, 480, 512, 768, 960, 1024] {
        for lookahead in [0usize, 1] {
            let lat = ((2 + lookahead) * hop) as u32;
            let expect = (lat as usize - hop) + (hop - nf_core::dsp::reblock::gcd(hop, BLOCK));
            let run = |floor_db: f32| {
                let c = controls(ProcessingConfig { floor_db, ..ProcessingConfig::default() });
                let stage = NoiseStage::new(c.clone(), Box::new(DelayModel::new(hop, lat))).expect("stage");
                let mut chain = chain_for(Box::new(stage), c).expect("chain");
                assert_eq!(chain.latency() as usize, expect, "hop {hop}, lookahead {lookahead}");
                run_chain(&mut chain, &x, &[])
            };
            let y = run(-100.0);
            let p = parity(&y, &x, expect);
            assert_eq!((p.first_mismatch, p.compared), (None, x.len() - expect), "hop {hop}, lookahead {lookahead}");
            assert!(y[..expect].iter().all(|&v| v == 0.0));
            assert_eq!(run(0.0), y, "hop {hop}, lookahead {lookahead}: the floor's dry path is aligned");
        }
    }
    eprintln!("re-blocker: hops 160, 240, 256, 480, 512, 768, 960, 1024 (lookahead 0 and 1): output = input delayed by L");
}

// ---- Check 2: parity --------------------------------------------------------------------------------------------------

#[test]
fn the_rnnoise_stage_is_nnnoiseless_driven_directly() {
    // Floor off, gate off: the stage's output is nnnoiseless's on the input ×32768, ÷32768 and clamped, bit for bit. Loud
    // blocks (×4 full scale) take the clamp too.
    let len = 6 * SR;
    let mut x = scenes::noisy_speech(len, Voice::HIGH, 10.0, 4).mix;
    for (i, v) in x[2 * SR..2 * SR + 4 * BLOCK].iter_mut().enumerate() {
        *v = if (i / 60) % 2 == 0 { 4.0 } else { -4.0 };
    }
    let staged = run_stage(&mut rnnoise(model_only()), &x, None).out;
    let mut state = DenoiseState::new();
    let (mut i16_in, mut i16_out) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
    let mut direct = Vec::with_capacity(len);
    for b in x.as_chunks::<BLOCK>().0 {
        for (d, &v) in i16_in.iter_mut().zip(b) {
            *d = v * 32768.0;
        }
        state.process_frame(&mut i16_out, &i16_in);
        direct.extend(i16_out.iter().map(|v| (v * (1.0 / 32768.0)).clamp(-1.0, 1.0)));
    }
    let p = parity(&staged, &direct, 0);
    eprintln!("RNNoise stage vs nnnoiseless: {} samples, max |diff| {}, first mismatch {:?}", p.compared, p.max_abs, p.first_mismatch);
    assert_eq!((p.first_mismatch, p.compared), (None, len));
    assert!(staged.iter().any(|v| v.abs() == 1.0), "the loud blocks reached the clamp");
}

// ---- Check 4: the host profile and the interface ---------------------------------------------------------------------

#[test]
fn every_stage_passes_the_profile_check_in_its_role() {
    let roles: Vec<_> = stages().iter().map(|(name, make, ..)| check_profile(make).unwrap_or_else(|e| panic!("{name}: {e}"))).collect();
    assert_eq!(roles, [StageRole::Noise, StageRole::EchoCancel]);
    let rn = rnnoise(model_only());
    let ae = aec3();
    assert_eq!(check_host_profile(&rn), Ok(StageRole::Noise));
    assert_eq!(check_host_profile(&ae), Ok(StageRole::EchoCancel));
    for s in [&rn as &dyn Stage, &ae] {
        let d = s.descriptor();
        assert!(d.id.starts_with("noise-filter.") && d.vendor == "noise-filter" && d.version == env!("CARGO_PKG_VERSION"), "{d:?}");
        assert!(d.has_feature("audio-effect") && d.has_feature("mono"), "{d:?}");
    }
    // Each in the other's slot is refused.
    let c = controls(ProcessingConfig::default());
    let wrong = nf_core::chain::Chain::new(c.clone(), Some(boxed(rnnoise(model_only()))), None).activate();
    assert!(wrong.is_err_and(|e| e.0.contains("not one for the EchoCancel slot")));
    let wrong = nf_core::chain::Chain::new(c, None, Some(boxed(aec3()))).activate();
    assert!(wrong.is_err_and(|e| e.0.contains("not one for the Noise slot")));
}

#[test]
fn other_formats_are_refused() {
    for (name, make, _) in stages() {
        let mut s = make();
        for bad in [
            Activation { sample_rate: 44_100, ..Activation::HOST },
            Activation { sample_rate: 16_000, ..Activation::HOST },
            Activation { min_frames: 1, ..Activation::HOST },
            Activation { max_frames: 512, ..Activation::HOST },
        ] {
            assert!(s.activate(&bad).is_err(), "{name}: {bad:?}");
        }
        assert!(s.activate(&Activation::HOST).is_ok(), "{name}");
    }
}

#[test]
fn the_noise_stage_s_param_defaults_are_the_config_defaults() {
    let s = rnnoise(ProcessingConfig::default());
    let (cfg, g) = (ProcessingConfig::default(), GateConfig::default());
    #[rustfmt::skip]
    let expect = [
        (param::FLOOR, f64::from(cfg.floor_db)), (param::GATE_MODE, f64::from(g.mode.to_u8())),
        (param::GATE_THRESHOLD, f64::from(g.threshold_db)), (param::GATE_AUTO_MARGIN, f64::from(g.auto_margin_db)),
        (param::GATE_ATTACK, f64::from(g.attack_ms)), (param::GATE_RELEASE, f64::from(g.release_ms)),
        (param::GATE_HOLD, f64::from(g.hold_ms)), (param::GATE_RANGE, f64::from(g.range_db)),
        (param::GATE_USE_VAD, f64::from(u8::from(g.use_vad))),
    ];
    assert_eq!(s.params(), PARAMS.as_slice());
    for (id, want) in expect {
        let info = s.params().iter().find(|p| p.id == id).expect("declared");
        assert_eq!(info.default, want, "{} default", info.name);
        assert_eq!(s.param_value(id), Some(want), "{}: what a default config stores", info.name);
    }
    assert!(aec3().params().is_empty(), "AEC3 has no params in v0");
}

#[test]
fn set_param_clamps_and_reads_back_and_takes_effect() {
    let c = controls(model_only());
    let mut s = rnnoise_stage_with(c.clone());
    s.set_param(param::FLOOR, -500.0).unwrap();
    assert_eq!(s.param_value(param::FLOOR), Some(-120.0));
    s.set_param(param::GATE_MODE, 1.6).unwrap();
    assert_eq!((s.param_value(param::GATE_MODE), c.load().gate.mode), (Some(1.0), GateMode::Static), "truncated");
    s.set_param(param::GATE_AUTO_MARGIN, 55.0).unwrap();
    assert_eq!(s.param_value(param::GATE_AUTO_MARGIN), Some(40.0));
    assert_eq!(s.set_param(param::GATE_RANGE, f64::NAN), Err(ParamError::NotANumber(param::GATE_RANGE)));
    assert_eq!(s.set_param(0, 1.0), Err(ParamError::UnknownId(0)));
    // A 0 dB floor set through the params makes the output the delayed input: it took effect at the next block.
    s.set_param(param::GATE_MODE, 0.0).unwrap();
    s.set_param(param::FLOOR, 0.0).unwrap();
    let x = white_noise(10 * BLOCK, 0.1, 9);
    let y = run_stage(&mut s, &x, None).out;
    assert_eq!(parity(&y, &x, RNNOISE_LATENCY as usize).first_mismatch, None);
}

fn rnnoise_stage_with(c: std::sync::Arc<nf_core::controls::ChainControls>) -> NoiseStage {
    let mut s = nf_stages::rnnoise_stage(c).expect("stage");
    s.activate(&Activation::HOST).expect("activate");
    s
}

#[test]
fn the_state_round_trips_and_tolerates_unknown_ids() {
    let mut a = rnnoise(ProcessingConfig::default());
    a.set_param(param::FLOOR, -24.0).unwrap();
    a.set_param(param::GATE_MODE, 2.0).unwrap();
    a.set_param(param::GATE_USE_VAD, 1.0).unwrap();
    let mut blob = Vec::new();
    a.save_state(&mut blob);
    // Append an entry with an id no stage has (count + 1): a later version's param, ignored.
    let count = u32::from_le_bytes(blob[8..12].try_into().unwrap());
    blob[8..12].copy_from_slice(&(count + 1).to_le_bytes());
    blob.extend(777u32.to_le_bytes());
    blob.extend(1.5f64.to_le_bytes());
    let mut b = rnnoise(ProcessingConfig::default());
    b.load_state(&blob).unwrap();
    for p in &PARAMS {
        assert_eq!(b.param_value(p.id), a.param_value(p.id), "{}", p.name);
    }
    let mut bad = blob.clone();
    bad[4] = 9;
    assert!(b.load_state(&bad).is_err(), "an unknown version");
    // AEC3's state: no params, and it round-trips as such.
    let mut e = aec3();
    let mut empty = Vec::new();
    e.save_state(&mut empty);
    assert_eq!((&empty[..4], empty.len()), (&state::MAGIC[..], 12));
    assert!(e.load_state(&empty).is_ok() && e.load_state(&blob).is_ok(), "unknown ids are ignored");
}

#[test]
fn each_stage_has_its_own_extensions_and_not_the_other_s() {
    let rn = rnnoise(model_only());
    let ae = aec3();
    assert!(rn.speech_probability().is_some() && rn.gain_adjustment().is_some() && rn.aec_telemetry().is_none());
    assert!(ae.aec_telemetry().is_some() && ae.speech_probability().is_none() && ae.gain_adjustment().is_none());
    // A model without a VAD gives a noise stage without the speech-probability extension (its gain stays).
    let d = NoiseStage::new(controls(model_only()), Box::new(DelayModel::new(480, 960))).expect("stage");
    assert!(d.speech_probability().is_none() && d.gain_adjustment().is_some());
}

#[test]
fn the_harness_runs_a_stage_as_the_chain_does() {
    // One stage alone in its slot (chain_for) gives what the stage gives called directly, once both have run a block:
    // the chain's first block switches nothing on, so neither resets.
    let x = scenes::noisy_speech(3 * SR, Voice::HIGH, 15.0, 8).mix;
    let direct = run_stage(&mut rnnoise(model_only()), &x, None).out;
    let c = controls(model_only());
    let mut chain = chain_for(boxed(nf_stages::rnnoise_stage(c.clone()).expect("stage")), c).expect("chain");
    assert_eq!(harness::run_chain(&mut chain, &x, &[], 0), direct);
    let room = scenes::echo_room(3 * SR, 0.15, 2);
    let mic = room.mic();
    let direct = run_stage(&mut aec3(), &mic, Some(&room.far.samples)).out;
    let c = controls(ProcessingConfig { aec_enabled: false, ..ProcessingConfig::default() });
    let mut chain = chain_for(boxed(nf_stages::Aec3Stage::new(c.clone()).expect("AEC3")), c.clone()).expect("chain");
    assert!(c.aec.enabled.load(Relaxed), "chain_for switches its slot on");
    assert_eq!(harness::run_chain(&mut chain, &mic, &room.far.samples, 0), direct);
}
