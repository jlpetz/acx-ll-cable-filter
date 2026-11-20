//! TODO 10's dither and FTZ/DAZ matrix (`doc/todo/10-aec.md`, "Dither and FTZ/DAZ"): every chain the engine can run, fed as the
//! engine feeds it under three dither policies, on three signals; each stage timed per block, and the output's exact zeros,
//! peak and difference from today's dither measured, phase by phase. One JSON line per cell and a table, in
//! `target/dither-matrix/`. The matrix is ignored (a measurement, minutes long); `tools/dither_matrix.sh N` builds the two test
//! exes (sonora's FTZ/DAZ guard off and on, both `onnx`) and runs them alternately, N rounds, with the CPU logged. By hand:
//!
//! ```text
//! cargo test -p nf-engine --release --features onnx --test dither_matrix -- --ignored --nocapture --test-threads 1
//! cargo test -p nf-engine --release --features onnx,nf-stages/sonora-denormal-guard --test dither_matrix -- --ignored ...
//! ```
//!
//! - **Chains** ([`cells::CHAINS`]): bypass (AEC3 and the noise filter off, what the engine runs with both off: the noise
//!   slot's stage off), AEC3 alone (beyond the spec's list: the app runs it), RNNoise and UL-UNAS, each alone and after AEC3.
//!   Each with a noise stage runs with the gate off, static and auto (the defaults otherwise: floor -60 dB, meters on). Built
//!   as nf-engine's `models.rs` builds them, the model inline, in a fresh chain for every cell; the noise model's hops are timed
//!   by a wrapper whose output is the engine's bit for bit (`the_timed_chains_are_the_engine_s`).
//! - **Dither** ([`signals::DitherMode`]): today's ("mic+ref": the mic's block and then the reference's, one generator, as
//!   `pipeline.rs` does); "mic+ref/seed2", today's from other seeds (the control: on speech, what any -180 dBFS perturbation
//!   changes, to set the others' difference from today's against); "ref", the reference only (the mic clean); "none", the
//!   engine's dither off (the AEC3 stage still dithers its reference, at 1e-9 on every block); and with AEC3, "bare",
//!   nothing dithered anywhere (`Aec3Stage::with_reference_dither` at 0). The engine's three-block warm-up runs first,
//!   dithered the same way. The control runs on the two signals with speech only, "bare" in chains with AEC3 only.
//! - **Reference handling:** with AEC3 the reference is the far end, block-aligned with the mic (offline there is no loopback
//!   timeline or slip to resample); without it, zeros, as the engine serves when it opens no reference stream.
//! - **Signals** ([`signals::SignalKind`]), from two fetched takes (`NF_TEST_AUDIO` or the local cache) or else the synthetic
//!   voices: sustained digital silence (15 s); the timing scene (near-end speech, pink noise, the far end and its echo) for 6 s
//!   and then 15 s with the mic and the reference exact zero (the tails); and the scene for 6 s, then 10 s of the near end
//!   alone (the far end exact zero after activity: the reference-silence stall), 6 s of the far end with the mic exact zero
//!   (a muted mic) and 6 s of both.
//! - **Measures**, per phase: p50, p99, max and mean µs a block of the chain (the `process_timed` call), AEC3, the noise stage,
//!   its model and the rest of it (the floor, the gate, the sanitising and clamp), over the phase's blocks after the signal's
//!   first second ([`nf_stages::bench::WARM_UP`]), and the chain's and AEC3's mean per second (whether a stall ends); the
//!   output's exact zeros (either sign) as a share of its samples, the time from the phase's start to its first all-zero
//!   block and to all-zero output to its end (or never), each as f32 and as a 24-bit and a 16-bit cable gets it (`convert.rs`'
//!   rounding), its subnormals, its peak over the phase and over its second half (what a silent phase settles at, past the
//!   chain's latency), and an FNV-1a hash of its bits (guard off against on); with the mic live, its SI-SDR against the near
//!   end's speech; and against the same cell with today's dither, the largest difference, the difference's RMS and its
//!   largest in each second (dBFS), and the share of samples that differ (on speech, the mic dither's effect). UL-UNAS's hop
//!   is 768 samples, so 5 blocks in 8 run its model: its p50 is a block with a run.
//! - **Allocation:** as the timing tests: the signals, feeds and result buffers are made before a run, and the loop only
//!   processes and stores (UL-UNAS's ONNX Runtime calls make their known C heap calls, TODO 6).
//!
//! Environment: `NF_DITHER_MATRIX_OUT` (the folder; default the workspace's `target/dither-matrix`), `NF_DITHER_MATRIX_TAG`
//! (the files' stem; default `manual-guard-off` or `-on`), `NF_DITHER_MATRIX_SCALE` (the phases' length, 1 = full: 64 s of
//! audio a chain, gate and dither), comma lists for a subset (unset or empty: all): `NF_DITHER_MATRIX_CHAINS` (`bypass`,
//! `aec3`, `rnnoise`, `rnnoise+aec3`, `ulunas`, `ulunas+aec3`), `_GATES` (`off`, `static`, `auto`), `_DITHERS` (`mic+ref`,
//! `mic+ref/seed2`, `ref`, `none`, `bare`; run in that order, whatever the list's, and compared with `mic+ref` only when it
//! runs too) and `_SIGNALS` (`silence`, `speech-silence`, `ref-near`), and `NF_REQUIRE_MODELS=1` (fail rather than skip when
//! UL-UNAS can't run). `NF_DITHER_MATRIX_SOURCE`, read at build time (`tools/dither_matrix.sh` sets it), is the source the
//! exe was built from, in every line.

mod cells;
mod measure;
mod report;
mod signals;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use cells::{CHAINS, ChainKind, GATES, gate_name};
use nf_core::BLOCK;
use nf_core::config::GateMode;
use nf_core::controls::ChainControls;
use nf_engine::models::{ModelThread, build_chain};
use signals::{DitherMode, Feed, Signal, SignalKind, Speech};

/// The matrix's selection and output, from the environment (module notes).
struct Options {
    out: PathBuf,
    tag: String,
    scale: f32,
    chains: Vec<ChainKind>,
    gates: Vec<GateMode>,
    dithers: Vec<DitherMode>,
    signals: Vec<SignalKind>,
    require_models: bool,
}

/// The items of `all` that `var` names (comma-separated), in `all`'s order (so today's dither runs before the others), or all
/// of them when it's unset or empty; an unknown name panics with the list.
fn pick<T: Copy>(var: &str, all: &[T], name: impl Fn(T) -> &'static str) -> Vec<T> {
    let Some(list) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) else { return all.to_vec() };
    let wanted: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    for s in &wanted {
        if !all.iter().any(|&t| name(t) == *s) {
            let names: Vec<&str> = all.iter().map(|&t| name(t)).collect();
            panic!("{var}: no '{s}' (one of {})", names.join(", "));
        }
    }
    all.iter().copied().filter(|&t| wanted.contains(&name(t))).collect()
}

impl Options {
    fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let guard = if nf_stages::SONORA_DENORMAL_GUARD { "on" } else { "off" };
        Self {
            out: var("NF_DITHER_MATRIX_OUT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/dither-matrix")),
            tag: var("NF_DITHER_MATRIX_TAG").unwrap_or_else(|| format!("manual-guard-{guard}")),
            scale: var("NF_DITHER_MATRIX_SCALE").map_or(1.0, |s| s.parse().expect("NF_DITHER_MATRIX_SCALE: a number")),
            chains: pick("NF_DITHER_MATRIX_CHAINS", &CHAINS, |c| c.name),
            gates: pick("NF_DITHER_MATRIX_GATES", &GATES, gate_name),
            dithers: pick("NF_DITHER_MATRIX_DITHERS", &DitherMode::ALL, DitherMode::name),
            signals: pick("NF_DITHER_MATRIX_SIGNALS", &SignalKind::ALL, SignalKind::name),
            require_models: var("NF_REQUIRE_MODELS").as_deref() == Some("1"),
        }
    }
}

#[test]
#[ignore = "TODO 10's dither matrix: a measurement, minutes long; release, --test-threads 1 (tools/dither_matrix.sh)"]
fn dither_matrix() {
    let o = Options::from_env();
    assert!(o.scale > 0.0, "NF_DITHER_MATRIX_SCALE must be positive");
    if cfg!(debug_assertions) {
        eprintln!("debug build: our crates run unoptimised, so the timings mean little (use --release)");
    }
    let speech = Speech::load();
    let info = report::RunInfo {
        tag: o.tag.clone(),
        guard: nf_stages::SONORA_DENORMAL_GUARD,
        onnx: nf_engine::models::ONNX,
        release: !cfg!(debug_assertions),
        scale: o.scale,
        speech: speech.source.clone(),
        source: option_env!("NF_DITHER_MATRIX_SOURCE"),
    };
    eprintln!("dither matrix {}: sonora's FTZ/DAZ guard {}, onnx {}, scale {}", o.tag, info.guard, info.onnx, o.scale);
    let signals: Vec<Signal> = o.signals.iter().map(|&k| Signal::new(k, &speech, o.scale)).collect();
    let mut rep = report::Report::create(&o.out, info).expect("the output folder");
    let started = Instant::now();
    let mut cells = 0;
    for kind in &o.chains {
        if let Some(why) = kind.unavailable() {
            assert!(!o.require_models, "NF_REQUIRE_MODELS=1: {}: {why}", kind.name);
            eprintln!("skipped {}: {why}", kind.name);
            rep.skipped(kind.name, &why);
            continue;
        }
        for &gate in kind.gates().iter().filter(|g| kind.model.is_none() || o.gates.contains(g)) {
            let cfg = kind.config(gate);
            for sig in &signals {
                // Today's output, for the other policies' difference from it.
                let mut today: Option<Vec<f32>> = None;
                for &dither in o.dithers.iter().filter(|d| d.runs(kind.aec, sig.kind)) {
                    let t0 = Instant::now();
                    let mut built = cells::build(kind, &cfg, dither.stage_dither()).unwrap_or_else(|e| panic!("{}: {e}", kind.name));
                    let latency = built.chain.latency();
                    let feed = Feed::new(sig, dither, kind.aec);
                    let t1 = Instant::now();
                    let run = measure::run(&mut built, &feed, dither);
                    let run_s = t1.elapsed().as_secs_f64();
                    drop(built);
                    let phases = measure::phases(sig, &run, kind.aec, kind.model.is_some(), latency, today.as_deref());
                    let cell = report::Cell {
                        chain: kind.name,
                        gate: kind.gate_label(gate),
                        dither: dither.name(),
                        seeds: dither.seeds(),
                        stage_ref_dither: kind.aec.then(|| dither.stage_dither()),
                        signal: sig.kind.name(),
                        latency,
                        blocks: sig.blocks(),
                        setup_s: (t1 - t0).as_secs_f64(),
                        run_s,
                    };
                    rep.cell(&cell, &phases).expect("the JSON lines");
                    eprintln!(
                        "{:<13} gate {:<6} {:<14} dither {:<13} {:6.2} s (setup {:.2} s)",
                        cell.chain, cell.gate, cell.signal, cell.dither, run_s, cell.setup_s
                    );
                    cells += 1;
                    if dither == DitherMode::MicAndReference {
                        today = Some(run.out);
                    }
                }
            }
        }
    }
    let table = rep.finish().expect("the table");
    eprintln!("\n{table}");
    eprintln!("{cells} cells in {:.1} s; output in {}", started.elapsed().as_secs_f64(), o.out.display());
    assert!(cells > 0, "the selection ran no cells (NF_DITHER_MATRIX_CHAINS, _GATES, _DITHERS, _SIGNALS)");
}

/// The harness's chains, built with the AEC3 stage's own dither at the app's amplitude and their noise model wrapped to time
/// its hops, give the engine's chain's output bit for bit (`nf_engine::models::build_chain`, the model inline), and the
/// wrapper counts time: on a second of the timing scene with AEC3 and the auto gate, for AEC3 alone, RNNoise and (an `onnx`
/// build with the pack installed) UL-UNAS.
#[test]
fn the_timed_chains_are_the_engine_s() {
    let sig = nf_stages::bench::Signals::synthetic(1.0);
    for kind in CHAINS.iter().filter(|k| k.aec) {
        if let Some(why) = kind.unavailable() {
            assert!(std::env::var("NF_REQUIRE_MODELS").as_deref() != Ok("1"), "NF_REQUIRE_MODELS=1: {why}");
            eprintln!("skipped {}: {why}", kind.name);
            continue;
        }
        let cfg = kind.config(GateMode::Auto);
        let mut ours = cells::build(kind, &cfg, nf_stages::AEC3_REFERENCE_DITHER).expect("ours");
        let mut engine = build_chain(Arc::new(ChainControls::new(&cfg)), &cfg, cfg.aec_enabled, ModelThread::Inline).expect("engine").chain;
        assert_eq!(ours.chain.latency(), engine.latency());
        let (mut a, mut b) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        let mut model_ns = 0;
        for i in 0..sig.blocks() {
            let mic = &sig.mic.as_chunks::<BLOCK>().0[i];
            let far = &sig.far.as_chunks::<BLOCK>().0[i];
            ours.chain.process(mic, far, &mut a);
            engine.process(mic, far, &mut b);
            assert!(a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()), "{}: block {i} differs", kind.name);
            model_ns += ours.model_ns.as_ref().map_or(0, |m| m.swap(0, Relaxed));
        }
        assert_eq!(model_ns > 0, kind.model.is_some(), "{}: the model's hops were timed", kind.name);
    }
}

mod checks {
    use super::*;
    use crate::measure::{Depth, Diff, Stats, diff, fnv1a, output, si_sdr};

    /// The policies: today's dithers both from one generator; the control the same from another; the reference only leaves
    /// the mic clean and gives the reference today's values; none and bare leave both as they were (bare also sets the AEC3
    /// stage's dither to 0). Without a reference stream the reference is (dithered) zeros. And the third signal's phases.
    #[test]
    fn the_feed_dithers_as_each_policy_says() {
        let speech = Speech { near: vec![0.25; 4800], far: vec![-0.5; 4800], source: "test".into() };
        let sig = Signal::new(SignalKind::ReferenceThenNear, &speech, 0.05);
        assert_eq!(sig.phases.iter().map(|p| (p.name, p.blocks(), p.mic_silent, p.ref_silent)).collect::<Vec<_>>(), [
            ("scene", 30, false, false),
            ("near-only", 50, false, true),
            ("far-only", 30, true, false),
            ("both", 30, false, false)
        ]);
        let today = Feed::new(&sig, DitherMode::MicAndReference, true);
        let control = Feed::new(&sig, DitherMode::Reseeded, true);
        let clean = Feed::new(&sig, DitherMode::ReferenceOnly, true);
        let none = Feed::new(&sig, DitherMode::None, true);
        let bare = Feed::new(&sig, DitherMode::Bare, true);
        assert_eq!((&none.mic, &none.reference), (&sig.mic, &sig.far));
        assert_eq!((&bare.mic, &bare.reference), (&sig.mic, &sig.far));
        assert_eq!(clean.mic, sig.mic, "the mic clean");
        assert_eq!(clean.reference, today.reference, "the reference gets today's dither");
        let s = |p: usize| sig.phases[p].start * BLOCK..sig.phases[p].end * BLOCK;
        // Where the input is silent, the control's dither is another of the same amplitude.
        let other = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| x != y && x.abs() <= signals::DITHER && y.abs() <= signals::DITHER);
        assert!(other(&control.mic[s(2)], &today.mic[s(2)]) && other(&control.reference[s(1)], &today.reference[s(1)]));
        assert!(sig.far[s(1)].iter().all(|&v| v == 0.0) && sig.mic[s(1)].iter().any(|&v| v != 0.0), "the near end alone");
        assert!(sig.mic[s(2)].iter().all(|&v| v == 0.0) && sig.far[s(2)].iter().any(|&v| v != 0.0), "the far end alone");
        assert!(today.reference[s(1)].iter().all(|&v| v != 0.0 && v.abs() <= signals::DITHER), "today's reference is dithered");
        assert!(today.mic[s(2)].iter().all(|&v| v != 0.0 && v.abs() <= signals::DITHER), "today's mic is dithered");
        assert!(sig.near[s(2)].iter().all(|&v| v == 0.0) && sig.near[s(1)].iter().all(|&v| v == 0.25), "the near end's speech");
        let no_reference = Feed::new(&sig, DitherMode::ReferenceOnly, false);
        assert!(no_reference.reference.iter().all(|&v| v != 0.0 && v.abs() <= signals::DITHER));
        // Which policies run where, and AEC3's own dither.
        assert!(!DitherMode::Bare.runs(false, SignalKind::SpeechThenSilence) && DitherMode::Bare.runs(true, SignalKind::Silence));
        assert!(!DitherMode::Reseeded.runs(true, SignalKind::Silence) && DitherMode::Reseeded.runs(false, SignalKind::ReferenceThenNear));
        assert_eq!((DitherMode::Bare.stage_dither(), DitherMode::None.stage_dither()), (0.0, nf_stages::AEC3_REFERENCE_DITHER));
        assert_ne!(DitherMode::Reseeded.seeds(), DitherMode::MicAndReference.seeds());
    }

    #[test]
    fn a_phase_s_zeros_and_when_they_start() {
        let mut y = vec![1e-9f32; 10 * BLOCK];
        y[2 * BLOCK..3 * BLOCK].fill(0.0); // a first all-zero block, then live again
        y[6 * BLOCK..].fill(0.0); // all zero from block 6
        y[5 * BLOCK] = f32::MIN_POSITIVE / 2.0;
        y[0] = 0.5; // the peak, in the first half
        let o = output(&y);
        let z = o.zeros[0];
        assert_eq!((z.first_ms, z.all_ms), (Some(20.0), Some(60.0)));
        assert!((z.share - 5.0 / 10.0).abs() < 1e-12, "{}", z.share);
        assert!((o.subnormal_share - 1.0 / (10 * BLOCK) as f64).abs() < 1e-12);
        assert!((o.peak_dbfs.unwrap() + 6.02).abs() < 0.01 && (o.late_peak_dbfs.unwrap() + 180.0).abs() < 0.01, "{o:?}");
        // Quantised, -180 dBFS is zero: every block but the first (its 0.5) is silent at 24 and 16 bits.
        for z in &o.zeros[1..] {
            assert_eq!((z.first_ms, z.all_ms), (Some(10.0), Some(10.0)));
            assert!((z.share - (1.0 - 1.0 / (10 * BLOCK) as f64)).abs() < 1e-12);
        }
        let never = output(&[1e-9; BLOCK]);
        assert_eq!((never.zeros[0].first_ms, never.zeros[0].all_ms, never.zeros[0].share), (None, None, 0.0));
        assert_eq!(never.zeros[1].share, 1.0);
        let silent = output(&[0.0, -0.0, 0.0]);
        assert_eq!(
            (silent.zeros[0].first_ms, silent.zeros[0].all_ms, silent.peak_dbfs, silent.zeros[0].share),
            (Some(0.0), Some(0.0), None, 1.0)
        );
        // Half a step rounds to even (0), a little more to a step; NaN is 0 as the saturating cast makes it.
        for (depth, full) in [(Depth::I24, 8_388_608.0f32), (Depth::I16, 32_768.0)] {
            assert!(depth.is_zero(0.5 / full) && depth.is_zero(-0.5 / full) && depth.is_zero(f32::NAN));
            assert!(!depth.is_zero(0.51 / full) && !depth.is_zero(1.5 / full) && !depth.is_zero(-1.0));
        }
        assert!(Depth::F32.is_zero(-0.0) && !Depth::F32.is_zero(f32::MIN_POSITIVE / 2.0) && !Depth::F32.is_zero(f32::NAN));
        // The bits' hash: FNV-1a's published value for no input, and -0.0 isn't 0.0.
        assert_eq!(fnv1a(&[]), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a(&[0.0]), fnv1a(&[-0.0]));
        assert_eq!(output(&y).fnv1a, fnv1a(&y));
    }

    #[test]
    fn differences_percentiles_and_si_sdr() {
        let same = Diff { max_dbfs: None, rms_dbfs: None, differ_share: 0.0, max_dbfs_per_s: vec![None] };
        assert_eq!(diff(&[0.0, 1.0], &[-0.0, 1.0]), same);
        let d = diff(&[0.0, 1.0, 0.5, 0.25], &[0.1, 1.0, 0.5, 0.25]);
        assert!((d.max_dbfs.unwrap() + 20.0).abs() < 1e-4 && d.differ_share == 0.25, "{d:?}");
        assert!((d.rms_dbfs.unwrap() + 26.02).abs() < 0.01 && d.max_dbfs_per_s == [d.max_dbfs], "{d:?}");
        // A second of the same, then a difference: per second, where it is.
        let (mut a, b) = (vec![0.0f32; 2 * 48_000], vec![0.0f32; 2 * 48_000]);
        a[60_000] = 1e-3;
        assert_eq!(diff(&a, &b).max_dbfs_per_s.len(), 2);
        assert!(diff(&a, &b).max_dbfs_per_s[0].is_none() && (diff(&a, &b).max_dbfs_per_s[1].unwrap() + 60.0).abs() < 1e-3);
        let s = Stats::of((1..=100).map(f64::from).rev().collect()).unwrap();
        assert_eq!((s.n, s.p50, s.p99, s.max, s.mean), (100, 51.0, 100.0, 100.0, 50.5));
        assert!(Stats::of(Vec::new()).is_none());
        // SI-SDR: at any gain the same; an orthogonal error at a tenth of the power is 10 dB; no speech, none.
        let sp = [1.0f32, -1.0, 1.0, -1.0];
        assert!(si_sdr(&sp.map(|v| 0.3 * v), &sp).unwrap() > 290.0);
        let e = [1.0f32, 1.0, -1.0, -1.0].map(|v| v * 0.1f32.sqrt());
        let y: Vec<f32> = sp.iter().zip(&e).map(|(a, b)| a + b).collect();
        assert!((si_sdr(&y, &sp).unwrap() - 10.0).abs() < 1e-5, "{:?}", si_sdr(&y, &sp));
        assert_eq!(si_sdr(&sp, &[0.0; 4]), None);
    }
}
