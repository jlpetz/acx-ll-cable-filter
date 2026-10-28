//! Offline harness, RNNoise stage (TODO #2 checks 1, plus #4 and #11): latency conformance,
//! suppression, the floor and the gate, on `synth` signals. No devices.

mod common;

use common::*;
use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::latency::analysis::{gcc_phat, xcorr_delay};
use nf_core::stage::Stage;
use nf_core::{BLOCK, db_to_lin};
use nf_stages::RNNOISE_LATENCY;
use nf_stages::synth::*;

/// Floor off and gate off: the model alone.
fn model_only() -> ProcessingConfig {
    ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() }
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(f32::NAN)
}

#[test]
fn conformance_stream_offset_is_480_on_speech() {
    let len = 8 * SR;
    for (voice, seed) in [(Voice::LOW, 1), (Voice::HIGH, 2)] {
        let sp = speech_like(len, voice, seed);
        let (x, _) = mix_at_snr(&sp.samples, sp.active_rms(), &pink_noise(len, 1.0, 7), 30.0);
        let mut s = rnnoise(model_only());
        let y = run_stage(&mut s, &x, None).out;
        assert_eq!(s.latency(), RNNOISE_LATENCY);
        let (d, conf) = gcc_phat(&x, &y, 2000, 0.75).expect("the output correlates with the input");
        assert!((d - 480.0).abs() < 0.1, "{voice:?}: GCC-PHAT offset {d}");
        assert!(conf > 25.0, "{voice:?}: confidence {conf} dB");
        // Plain cross-correlation agrees to within the model's DC blocker's group delay, which
        // weighs in on voiced speech's strong low harmonics.
        let (d, _) = xcorr_delay(&x, &y, 2000).expect("correlates");
        assert!((d - 480.0).abs() < 1.0, "{voice:?}: xcorr offset {d}");
    }
}

#[test]
fn stationary_noise_is_cut_in_pauses_and_speech_keeps_its_level() {
    let len = 15 * SR;
    let sp = speech_like(len, Voice::LOW, 3);
    let lat = RNNOISE_LATENCY as usize;
    // Skip the first 2 s: the model learns the noise from the first phrases.
    let pauses = within(&sp.pauses(4800, 2400), 2 * SR, len - lat);
    let cores = within(&sp.cores(2400), 2 * SR, len - lat);
    assert!(pauses.len() >= 5 && cores.len() >= 20);
    for (name, noise) in [("white", white_noise(len, 1.0, 11)), ("pink", pink_noise(len, 1.0, 12))] {
        for snr in [5.0, 15.0] {
            let (x, n) = mix_at_snr(&sp.samples, sp.active_rms(), &noise, snr);
            // The shipped defaults: floor -60 dB (effectively none), gate off.
            let run = run_stage(&mut rnnoise(ProcessingConfig::default()), &x, None);
            let y = &run.out[lat..];
            dump(&format!("rnnoise-{name}-{snr}dB-in"), &x);
            dump(&format!("rnnoise-{name}-{snr}dB-out"), &run.out);

            let cut = db(rms_over(&n, &pauses) / rms_over(y, &pauses));
            assert!(cut >= 15.0, "{name} at {snr} dB SNR: noise cut by only {cut:.1} dB in pauses");
            let level = db(rms_over(y, &cores) / rms_over(&sp.samples, &cores));
            eprintln!("{name} at {snr} dB SNR: noise cut {cut:.1} dB in pauses, speech level {level:+.1} dB");
            assert!(level.abs() <= 3.0, "{name} at {snr} dB SNR: speech level moved {level:.1} dB");

            // The report: VAD and gain reduction tell speech from noise (block k covers input k-1).
            let blocks = |ranges: &[std::ops::Range<usize>]| -> Vec<usize> {
                ranges.iter().flat_map(|r| r.start / BLOCK + 2..r.end / BLOCK).collect()
            };
            let vad = |bs: &[usize]| median(bs.iter().map(|&b| run.reports[b].speech_probability).collect());
            let gr = |bs: &[usize]| median(bs.iter().map(|&b| run.reports[b].gain_reduction_db).collect());
            let (bp, bc) = (blocks(&pauses), blocks(&cores));
            assert!(vad(&bc) > 0.5 && vad(&bp) < 0.5, "{name}/{snr}: VAD speech {} noise {}", vad(&bc), vad(&bp));
            assert!(gr(&bp) < -15.0 && gr(&bc) > -3.0, "{name}/{snr}: reduction pauses {} speech {}", gr(&bp), gr(&bc));
        }
    }
}

#[test]
fn a_minus_20_db_floor_leaves_noise_20_db_down() {
    // Four seconds of noisy speech, then six of the noise alone. RNNoise learns the noise from
    // speech: on stationary noise it has never heard speech over, it barely suppresses at all.
    let len = 10 * SR;
    let lat = RNNOISE_LATENCY as usize;
    let sp = speech_like(4 * SR, Voice::LOW, 5);
    let mut clean = sp.samples.clone();
    clean.resize(len, 0.0);
    let last = sp.syllables.last().expect("syllables").end;
    let noise_only = last + SR..len - lat;
    for (name, noise) in [("white", white_noise(len, 1.0, 21)), ("pink", pink_noise(len, 1.0, 22))] {
        let (x, _) = mix_at_snr(&clean, sp.active_rms(), &noise, 10.0);
        let free = run_stage(&mut rnnoise(model_only()), &x, None).out;
        let floored = run_stage(&mut rnnoise(ProcessingConfig { floor_db: -20.0, ..model_only() }), &x, None).out;
        dump(&format!("floor-{name}-in"), &x);
        dump(&format!("floor-{name}-out"), &floored);

        let out = |y: &[f32]| rms(&y[noise_only.start + lat..noise_only.end + lat]);
        let level = db(out(&floored) / rms(&x[noise_only.clone()]));
        assert!((level - -20.0).abs() <= 2.0, "{name}: noise-only output at {level:.1} dB");
        let model = db(out(&free) / rms(&x[noise_only.clone()]));
        eprintln!("{name}: noise-only output {level:.1} dB with the -20 dB floor, {model:.1} dB without");
        assert!(model < level - 10.0, "{name}: the model alone ({model:.1} dB) should go well below the floor");

        // The mechanism, sample for sample: the floor mixes in the input aligned with the model.
        let lim = db_to_lin(-20.0);
        let keep = 1.0 - lim;
        for i in lat..len {
            let expect = lim * x[i - lat] + keep * free[i];
            assert_eq!(floored[i].to_bits(), expect.to_bits(), "{name}: sample {i}");
        }
    }
}

/// The gap scenario (8 talk spurts in pink noise at -50 dBFS, 2 s gaps) through the stage with
/// `gate`.
struct Gated {
    /// The output moved back by the stage latency, so it lines up with the input.
    out: Vec<f32>,
    /// Each gap, from the spurt's last syllable to the next spurt.
    gaps: Vec<std::ops::Range<usize>>,
    /// Each gap interior's level against the noise there, in dB.
    levels: Vec<f32>,
    /// The speech cores' level against the clean speech, in dB.
    speech: f32,
    /// Each gap interior's median reported reduction, in dB.
    gr: Vec<f32>,
}

/// From 400 ms into the gap (the default hold and release, 250 ms, have run) to 50 ms before the
/// next spurt.
fn interior(gap: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    gap.start + 2 * SR / 5..gap.end - SR / 20
}

fn gated(gate: GateConfig) -> Gated {
    let talk = talk_with_gaps(8, 3.0, 2.0, Voice::LOW, 30);
    let lat = RNNOISE_LATENCY as usize;
    let n = pink_noise(talk.samples.len(), db_to_lin(-50.0), 8);
    let x = add(&talk.samples, &n);
    let run = run_stage(&mut rnnoise(ProcessingConfig { gate, ..ProcessingConfig::default() }), &x, None);
    let y = run.out[lat..].to_vec();
    let interiors: Vec<_> = talk.gaps.iter().map(interior).collect();
    let levels = interiors.iter().map(|r| db(rms(&y[r.clone()]) / rms(&n[r.clone()]))).collect();
    let cores =
        within(&talk.syllables.iter().filter(|r| r.len() > 4800).map(|r| r.start + 2400..r.end - 2400).collect::<Vec<_>>(), 0, y.len());
    let speech = db(rms_over(&y, &cores) / rms_over(&talk.samples, &cores));
    let gr = interiors
        .iter()
        .map(|r| median((r.start / BLOCK + 1..r.end / BLOCK).map(|b| run.reports[b].gain_reduction_db).collect()))
        .collect();
    Gated { out: y, gaps: talk.gaps, levels, speech, gr }
}

/// How each gap closes, from the gate's gain block by block: `on` against `off`, the same run
/// ungated (the gate acts after the model and the floor mix, and the model never sees its output).
/// Per gap, the time from the gap's start to the end of the first block within 0.5 dB of
/// `range_db`, in ms, and how many blocks after it, up to the interior's end, are further off.
fn closing(on: &Gated, off: &Gated, range_db: f32) -> Vec<Option<(f32, usize)>> {
    let gain = |b: usize| {
        let r = b * BLOCK..(b + 1) * BLOCK;
        db(rms(&on.out[r.clone()]) / rms(&off.out[r]))
    };
    let closed = |b: usize| (gain(b) - range_db).abs() <= 0.5;
    on.gaps
        .iter()
        .map(|g| {
            let end = interior(g).end / BLOCK;
            let first = (g.start / BLOCK + 1..end).find(|&b| closed(b))?;
            let ms = ((first + 1) * BLOCK - g.start) as f32 * 1000.0 / SR as f32;
            Some((ms, (first..end).filter(|&b| !closed(b)).count()))
        })
        .collect()
}

#[test]
fn the_static_gate_closes_in_gaps_at_its_release_time_and_leaves_speech_alone() {
    // The default hold (100 ms) and release (150 ms): the release is the time from open to range.
    let gate = GateConfig { mode: GateMode::Static, threshold_db: -45.0, ..GateConfig::default() };
    let off = gated(GateConfig { mode: GateMode::Off, ..gate.clone() });
    let on = gated(gate.clone());
    let closes = closing(&on, &off, gate.range_db);
    eprintln!("static gate, default release: closed after {closes:.0?} (ms, blocks reopened)");
    eprintln!("    reported reduction in gaps {:.1?} -> {:.1?} dB, speech {:.2} -> {:.2} dB", off.gr, on.gr, off.speech, on.speech);
    let due = gate.hold_ms + gate.release_ms;
    for (k, c) in closes.iter().enumerate() {
        let (ms, reopened) = c.unwrap_or_else(|| panic!("gap {k}: the gate never reached its range"));
        // The level falls below the threshold at the syllable's tail; the gain is read per block.
        assert!((due - 30.0..=due + 50.0).contains(&ms), "gap {k}: at range after {ms:.0} ms, not hold + release ({due} ms)");
        assert_eq!(reopened, 0, "gap {k}: reopened in the pause");
    }
    for (k, (a, b)) in off.levels.iter().zip(&on.levels).enumerate() {
        assert!((b - a - gate.range_db).abs() <= 2.0, "gap {k}: the gate took {:.1} dB, not its 40 dB range", b - a);
    }
    for (k, (a, b)) in off.gr.iter().zip(&on.gr).enumerate() {
        assert!((b - a - gate.range_db).abs() <= 3.0, "gap {k}: reported reduction {a:.1} -> {b:.1} dB");
    }
    let (s0, s1) = (off.speech, on.speech);
    assert!((s1 - s0).abs() <= 0.5 && s1.abs() <= 3.0, "speech {s0:.2} -> {s1:.2} dB");
}

#[test]
fn the_auto_gate_closes_in_gaps_at_the_default_margin() {
    // Every default: a 6 dB margin over the auto floor, which follows RNNoise's smoothed residual.
    // RNNoise's residual stays up for a second or so after a spurt, so the close comes later
    // than the static gate's in some gaps, but it comes in every one, and stays.
    let gate = GateConfig { mode: GateMode::Auto, ..GateConfig::default() };
    assert_eq!(gate.auto_margin_db, 6.0, "this test is about the default margin");
    let off = gated(GateConfig { mode: GateMode::Off, ..gate.clone() });
    let on = gated(gate.clone());
    let closes = closing(&on, &off, gate.range_db);
    let extra: Vec<f32> = off.levels.iter().zip(&on.levels).map(|(a, b)| b - a).collect();
    eprintln!("auto gate, default margin: closed after {closes:.0?} (ms, blocks reopened)");
    eprintln!("    gap interiors {extra:.1?} dB, speech {:.2} -> {:.2} dB", off.speech, on.speech);
    for (k, c) in closes.iter().enumerate() {
        let (ms, reopened) = c.unwrap_or_else(|| panic!("gap {k}: the auto gate never closed ({extra:.1?} dB)"));
        assert!(ms <= 1500.0, "gap {k}: the auto gate took {ms:.0} ms to close");
        assert_eq!(reopened, 0, "gap {k}: reopened in the pause");
    }
    assert!(on.levels.iter().zip(&off.levels).all(|(b, a)| b <= &(a + 0.5)), "the gate never adds level");
    let (s0, s1) = (off.speech, on.speech);
    assert!((s1 - s0).abs() <= 0.5 && s1.abs() <= 3.0, "speech {s0:.2} -> {s1:.2} dB");
}
