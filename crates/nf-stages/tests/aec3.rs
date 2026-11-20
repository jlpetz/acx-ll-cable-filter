//! Offline harness, AEC3 stage (TODO #10): its stream offset, echo cancellation on a synthetic
//! room, double talk, reset and bad input. No devices.

mod common;

use common::*;
use nf_core::config::ProcessingConfig;
use nf_core::latency::analysis::gcc_phat;
use nf_core::stage::{Activation, AecTelemetry, MONO_WITH_REFERENCE, ProcessIo, Stage, StageRole, check_host_profile};
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_stages::harness::scenes::echo_room as room;
use nf_stages::synth::*;
use nf_stages::{AEC3_LATENCY, Aec3Stage};

const LAT: usize = AEC3_LATENCY as usize;

#[test]
fn stream_offset_is_aec3_latency() {
    // An impulse at several block phases, with a silent reference: it comes out AEC3_LATENCY later.
    for pos in [5 * BLOCK + 17, 20 * BLOCK + 301, 50 * BLOCK, 61 * BLOCK + 479] {
        let mut mic = vec![0.0f32; 80 * BLOCK];
        mic[pos] = 0.5;
        let y = run_stage(&mut aec3(), &mic, Some(&vec![0.0; mic.len()])).out;
        let at = peak_index(&y);
        assert_eq!(at - pos, LAT, "impulse at {pos}");
        assert!(y[at] > 0.35, "the impulse passes nearly whole: {}", y[at]);
    }
    // Broadband noise agrees to a fraction of a sample.
    let mic = white_noise(200 * BLOCK, 0.05, 3);
    let y = run_stage(&mut aec3(), &mic, Some(&vec![0.0; mic.len()])).out;
    let (d, conf) = gcc_phat(&mic[20 * BLOCK..], &y[20 * BLOCK..], 2000, 1.0).expect("correlates");
    assert!((d - LAT as f64).abs() < 0.1 && conf > 30.0, "noise: {d} at {conf} dB");
    assert_eq!(aec3().latency(), AEC3_LATENCY);
}

#[test]
fn echo_is_cancelled_by_15_db_after_3_s() {
    let len = 12 * SR;
    for (rt60, seed) in [(0.15, 1), (0.3, 2)] {
        let r = room(len, rt60, seed);
        let mic = add(&r.echo, &r.mic_noise);
        let run = run_stage(&mut aec3(), &mic, Some(&r.far.samples));
        let y = &run.out[LAT..];
        dump(&format!("aec-rt{rt60}-mic"), &mic);
        dump(&format!("aec-rt{rt60}-out"), &run.out);
        let span = 3 * SR..len - LAT;
        let erle = db(rms(&mic[span.clone()]) / rms(&y[span]));
        let s = run.aec.last().copied().expect("stats");
        eprintln!("RT60 {rt60}: ERLE after 3 s {erle:.1} dB; AEC3 reports ERLE {:.1} dB, delay {} ms", s.erle_db, s.echo_delay_ms);
        assert!(erle >= 15.0, "RT60 {rt60}: ERLE {erle:.1} dB");
        // AEC3's own figures: its linear filter's ERLE (the tail past its 52 ms filter limits it),
        // and its delay estimate in 4 ms steps, a block under the echo's bulk delay.
        assert!(s.erle_db > 6.0, "RT60 {rt60}: reported ERLE {}", s.erle_db);
        assert!((32.0..=44.0).contains(&s.echo_delay_ms), "RT60 {rt60}: reported delay {} ms", s.echo_delay_ms);
    }
}

#[test]
fn double_talk_keeps_the_near_end() {
    // Far end throughout; a near-end talker joins after 6 s. The near end's share of the output is
    // the output with it minus the output without it (same far end, same echo).
    let len = 16 * SR;
    for seed in [1, 3] {
        let r = room(len, 0.15, seed);
        let near = speech_like(len, Voice::HIGH.at(-26.0), seed + 100);
        let mut near_s = near.samples.clone();
        near_s[..6 * SR].fill(0.0);
        let mic0 = add(&r.echo, &r.mic_noise);
        let mic1 = add(&mic0, &near_s);
        let y0 = run_stage(&mut aec3(), &mic0, Some(&r.far.samples)).out;
        let y1 = run_stage(&mut aec3(), &mic1, Some(&r.far.samples)).out;
        dump(&format!("aec-dt{seed}-mic"), &mic1);
        dump(&format!("aec-dt{seed}-out"), &y1);
        let share: Vec<f32> = y1.iter().zip(&y0).map(|(a, b)| a - b).collect();

        // Blocks where the near end talks, split by whether the far end does too.
        let (mut both, mut near_only) = (Vec::new(), Vec::new());
        for b in 6 * SR / BLOCK..(len - LAT) / BLOCK {
            let span = b * BLOCK..(b + 1) * BLOCK;
            if !near.syllables.iter().any(|s| s.start <= span.start && span.end <= s.end) {
                continue;
            }
            if r.far.near_syllable(span.start, 2400) || r.far.near_syllable(span.end, 2400) {
                both.push(span);
            } else if !r.far.near_syllable(span.start, 4800) {
                near_only.push(span);
            }
        }
        assert!(both.len() > 100 && near_only.len() > 10, "{} / {} blocks", both.len(), near_only.len());
        let kept = |spans: &[std::ops::Range<usize>]| db(rms_over(&share, &shifted(spans, LAT)) / rms_over(&near_s, spans));
        eprintln!("seed {seed}: near end kept {:.1} dB in double talk, {:.1} dB alone", kept(&both), kept(&near_only));
        assert!(kept(&both) > -6.0, "seed {seed}: double talk removed the near end: {:.1} dB", kept(&both));
        assert!(kept(&near_only) > -3.0, "seed {seed}: near end alone: {:.1} dB", kept(&near_only));
    }
}

#[test]
fn reset_flushes_held_mic_audio() {
    // Two identical stages hear loud mic audio, then stop. Without a reset, the audio AEC3 holds
    // (AEC3_LATENCY samples) comes out when it resumes; after one, only its filters' ringing does,
    // about 38 dB down (a second flush pair would only take that to 55).
    let loud = white_noise(4 * BLOCK, 0.3, 5);
    let prepared = || {
        let mut s = aec3();
        run_stage(&mut s, &loud, Some(&vec![0.0; loud.len()]));
        s
    };
    let (mut held, mut s) = (prepared(), prepared());
    let stale = run_stage(&mut held, &[0.0; BLOCK], Some(&[0.0; BLOCK])).out;
    s.reset();
    let after = run_stage(&mut s, &[0.0; BLOCK], Some(&[0.0; BLOCK])).out;
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak(&stale) > 0.5, "the held audio should show without a reset (peak {})", peak(&stale));
    let (by_peak, by_rms) = (db(peak(&stale) / peak(&after)), db(rms(&stale) / rms(&after)));
    assert!(by_peak >= 30.0 && by_rms >= 30.0, "reset flushed only {by_peak:.1} dB (peak), {by_rms:.1} dB (rms)");
}

#[test]
fn resetting_an_unused_stage_changes_nothing() {
    // The chain resets every stage on its first block; a stage that holds no audio mustn't flush.
    let mic = white_noise(30 * BLOCK, 0.1, 2);
    let far = white_noise(30 * BLOCK, 0.1, 3);
    let plain = run_stage(&mut aec3(), &mic, Some(&far)).out;
    let mut s = aec3();
    s.reset();
    s.reset();
    assert_eq!(run_stage(&mut s, &mic, Some(&far)).out, plain);
}

#[test]
fn reset_keeps_the_converged_echo_path() {
    let len = 10 * SR;
    let r = room(len, 0.15, 4);
    let mic = add(&r.echo, &r.mic_noise);
    let mut s = aec3();
    run_stage(&mut s, &mic[..6 * SR], Some(&r.far.samples[..6 * SR]));
    let converged = s.aec_stats();
    s.reset();
    // Right after the reset the echo is still cancelled, and AEC3's own ERLE and delay estimate
    // carry on where they were (a fresh AEC3 starts from 0 dB and takes seconds).
    let tail = 6 * SR..len;
    let run = run_stage(&mut s, &mic[tail.clone()], Some(&r.far.samples[tail.clone()]));
    let first = 0..SR - LAT;
    let erle = db(rms(&mic[tail.start..tail.start + first.end]) / rms(&run.out[LAT..first.end + LAT]));
    assert!(erle >= 15.0, "ERLE in the first second after reset {erle:.1} dB");
    let at_1s = run.aec[SR / BLOCK];
    assert!(at_1s.erle_db >= converged.erle_db - 3.0, "reported ERLE {:.1} -> {:.1} dB", converged.erle_db, at_1s.erle_db);
    assert_eq!(at_1s.echo_delay_ms, converged.echo_delay_ms);
    let fresh = run_stage(&mut aec3(), &mic[tail.clone()], Some(&r.far.samples[tail])).aec[SR / BLOCK];
    assert!(fresh.erle_db < converged.erle_db - 6.0, "a fresh AEC3 for comparison: {:.1} dB", fresh.erle_db);
}

#[test]
fn bad_input_is_sanitised() {
    let len = 6 * SR;
    let r = room(len, 0.15, 6);
    let mut mic = add(&r.echo, &r.mic_noise);
    let mut far = r.far.samples.clone();
    for i in (SR..2 * SR).step_by(331) {
        mic[i] = f32::NAN;
        far[i + 7] = f32::INFINITY;
    }
    mic[2 * SR] = f32::NEG_INFINITY;
    let run = run_stage(&mut aec3(), &mic, Some(&far));
    assert!(run.out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    let span = 4 * SR..len - LAT;
    let erle = db(rms(&mic[span.clone()]) / rms(&run.out[span.start + LAT..span.end + LAT]));
    assert!(erle >= 15.0, "NaNs mustn't poison the filters: ERLE {erle:.1} dB");

    // No reference at all reads as a silent one.
    let mut s = aec3();
    let mut y = [0.0f32; BLOCK];
    for x in white_noise(20 * BLOCK, 0.1, 1).as_chunks::<BLOCK>().0 {
        s.process(ProcessIo::new(&[x], &mut y));
        assert!(y.iter().all(|v| v.is_finite()));
    }
}

#[test]
fn descriptor_ports_and_format() {
    let mut s = Aec3Stage::new(controls(ProcessingConfig::default())).expect("build");
    assert_eq!(s.descriptor().id, "noise-filter.echo-cancel");
    assert_eq!(check_host_profile(&s), Ok(StageRole::EchoCancel));
    assert_eq!(*s.audio_ports(), MONO_WITH_REFERENCE, "the mic, then the speaker reference on a non-main port");
    assert!(s.activate(&Activation { sample_rate: 44_100, ..Activation::HOST }).is_err());
    assert!(s.activate(&Activation { sample_rate: SAMPLE_RATE, min_frames: 256, max_frames: 256 }).is_err());
    assert!(s.activate(&Activation::HOST).is_ok());
    let fresh = s.aec_stats();
    assert!(fresh.erle_db.is_nan() && fresh.echo_delay_ms.is_nan(), "nothing measured yet: {fresh:?}");
}

/// sonora's AEC path (the three-band split and merge, every biquad) uses `f32::mul_add`, which is one
/// FMA instruction at our x86-64-v3 baseline. In sonora 0.2.0 a build without FMA (the baseline lowered
/// to v2, say) made each one a call to the C runtime's `fmaf`, per sample: about 36 µs of AEC3's 91 µs
/// a block at v2 (doc/dev/api/hot-loop-tools.md, pitfall 3). sonora 0.3.0 writes them as plain
/// arithmetic where the build lacks FMA (`sonora_simd::NATIVE_FMA`, its PR #37), which changes AEC3's
/// output by rounding. sonora is built with the same flags as this test, which then doesn't build.
#[test]
fn the_build_has_fma_for_sonoras_mul_add() {
    const { assert!(cfg!(target_feature = "fma"), "no FMA: sonora's mul_add path is off (.cargo/config.toml)") }
}

/// The calling thread's MXCSR.
fn mxcsr() -> u32 {
    let mut v = 0u32;
    // SAFETY: `stmxcsr` stores the 32-bit MXCSR into `v`, a live local; it reads no memory and changes no flags.
    unsafe { std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut v, options(nostack, preserves_flags)) };
    v
}

/// sonora 0.3.0 sets FTZ and DAZ for the length of each `process_*` call (upstream's `DenormalDisabler`) and restores the
/// thread's MXCSR before the call returns. Nothing may leave them set (no persistent FTZ/DAZ: nf-engine's `audio.rs`), so
/// after the stage's blocks, on a speaking and then a silent reference, the thread's control bits are as they were and a
/// denormal survives.
#[test]
fn aec3_leaves_the_thread_s_mxcsr_as_it_found_it() {
    // Without the status flags, which ordinary arithmetic sets.
    const CONTROL: u32 = !0x3f;
    let before = mxcsr();
    let far = speech_like(100 * BLOCK, Voice::LOW, 7).samples;
    let mic = scaled(&delayed(&far, 1920), 0.4);
    let mut s = aec3();
    let built = mxcsr();
    run_stage(&mut s, &mic, Some(&far));
    run_stage(&mut s, &mic, Some(&vec![0.0; mic.len()]));
    let after = mxcsr();
    assert_eq!((built & CONTROL, after & CONTROL), (before & CONTROL, before & CONTROL), "MXCSR {before:#06x}, {built:#06x}, {after:#06x}");
    let tiny = std::hint::black_box(1e-38f32);
    assert!(std::hint::black_box(tiny * 0.01) > 0.0, "a denormal product flushed to 0");
}
