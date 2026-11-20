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
//! - **Dither** ([`signals::DitherMode`]): today's (the mic's block and then the reference's, one generator, as `pipeline.rs`
//!   does), the reference only (the mic clean), and none. The AEC3 stage's own reference dither stays in all three: it is the
//!   stage's, not the engine's. The engine's three-block warm-up runs first, dithered the same way.
//! - **Reference handling:** with AEC3 the reference is the far end, block-aligned with the mic (offline there is no loopback
//!   timeline or slip to resample); without it, zeros, as the engine serves when it opens no reference stream.
//! - **Signals** ([`signals::SignalKind`]), from two fetched takes (`NF_TEST_AUDIO` or the local cache) or else the synthetic
//!   voices: sustained digital silence (15 s); the timing scene (near-end speech, pink noise, the far end and its echo) for 6 s
//!   and then 15 s with the mic and the reference exact zero (the tails); and 4 s silent, 6 s of the far end with the mic exact
//!   zero (a muted mic), then 6 s with the near end too.
//! - **Measures**, per phase: p50, p99, max and mean µs a block of the chain (the `process_timed` call), AEC3, the noise stage,
//!   its model and the rest of it (the floor, the gate, the sanitising and clamp), over the phase's blocks after the signal's
//!   first second ([`nf_stages::bench::WARM_UP`]), and the chain's mean per second; the output's exact zeros (either sign) and
//!   subnormals as shares of its samples, the time from the phase's start to its first all-zero block and to all-zero output to
//!   its end (or never), and its peak over the phase and over its second half (what a silent phase settles at, past the
//!   chain's latency); and against the same cell with today's dither, the largest difference, the difference's RMS and its
//!   largest in each second (dBFS), and the share of samples that differ (on speech, the mic dither's effect). UL-UNAS's hop
//!   is 768 samples, so 5 blocks in 8 run its model: its p50 is a block with a run.
//! - **Allocation:** as the timing tests: the signals, feeds and result buffers are made before a run, and the loop only
//!   processes and stores (UL-UNAS's ONNX Runtime calls make their known C heap calls, TODO 6).
//!
//! Environment: `NF_DITHER_MATRIX_OUT` (the folder; default the workspace's `target/dither-matrix`), `NF_DITHER_MATRIX_TAG`
//! (the files' stem; default `manual-guard-off` or `-on`), `NF_DITHER_MATRIX_SCALE` (the phases' length, 1 = full: 52 s of
//! audio a chain, gate and dither), comma lists for a subset: `NF_DITHER_MATRIX_CHAINS` (`bypass`, `aec3`, `rnnoise`,
//! `rnnoise+aec3`, `ulunas`, `ulunas+aec3`), `_GATES` (`off`, `static`, `auto`), `_DITHERS` (`mic+ref`, `ref`, `none`; run in
//! that order, whatever the list's, and `ref` and `none` are compared with `mic+ref` only when it runs too) and `_SIGNALS`
//! (`silence`, `speech-silence`, `ref-near`), and `NF_REQUIRE_MODELS=1` (fail rather than skip when UL-UNAS can't run).

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
/// of them when it's unset; an unknown name panics with the list.
fn pick<T: Copy>(var: &str, all: &[T], name: impl Fn(T) -> &'static str) -> Vec<T> {
    let Ok(list) = std::env::var(var) else { return all.to_vec() };
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
                for &dither in &o.dithers {
                    let t0 = Instant::now();
                    let mut built = cells::build(kind, &cfg).unwrap_or_else(|e| panic!("{}: {e}", kind.name));
                    let latency = built.chain.latency();
                    let feed = Feed::new(sig, dither, kind.aec);
                    let t1 = Instant::now();
                    let run = measure::run(&mut built, &feed, dither);
                    let run_s = t1.elapsed().as_secs_f64();
                    drop(built);
                    let phases = measure::phases(sig, &run, kind.aec, kind.model.is_some(), today.as_deref());
                    let cell = report::Cell {
                        chain: kind.name,
                        gate: kind.gate_label(gate),
                        dither: dither.name(),
                        signal: sig.kind.name(),
                        latency,
                        blocks: sig.blocks(),
                        setup_s: (t1 - t0).as_secs_f64(),
                        run_s,
                    };
                    rep.cell(&cell, &phases).expect("the JSON lines");
                    eprintln!(
                        "{:<13} gate {:<6} {:<14} dither {:<7} {:6.2} s (setup {:.2} s)",
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
}

/// The harness's chains, their noise model wrapped to time its hops, give the engine's chain's output bit for bit
/// (`nf_engine::models::build_chain`, the model inline), and the wrapper counts time: on a second of the timing scene with
/// AEC3 and the auto gate, for RNNoise and (an `onnx` build with the pack installed) UL-UNAS.
#[test]
fn the_timed_chains_are_the_engine_s() {
    let sig = nf_stages::bench::Signals::synthetic(1.0);
    for kind in CHAINS.iter().filter(|k| k.model.is_some() && k.aec) {
        if let Some(why) = kind.unavailable() {
            assert!(std::env::var("NF_REQUIRE_MODELS").as_deref() != Ok("1"), "NF_REQUIRE_MODELS=1: {why}");
            eprintln!("skipped {}: {why}", kind.name);
            continue;
        }
        let cfg = kind.config(GateMode::Auto);
        let mut ours = cells::build(kind, &cfg).expect("ours");
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
        assert!(model_ns > 0, "{}: the model's hops were timed", kind.name);
    }
}

mod checks {
    use super::*;
    use crate::measure::{Diff, Stats, diff, output};

    /// The policies: today's dithers both from one generator; the reference only leaves the mic clean and gives the reference
    /// today's values; none leaves both as they were. Without a reference stream the reference is (dithered) zeros.
    #[test]
    fn the_feed_dithers_as_each_policy_says() {
        let speech = Speech { near: vec![0.25; 4800], far: vec![-0.5; 4800], source: "test".into() };
        let sig = Signal::new(SignalKind::ReferenceThenNear, &speech, 0.05);
        assert_eq!(sig.phases.iter().map(|p| (p.name, p.blocks(), p.mic_silent)).collect::<Vec<_>>(), [
            ("quiet", 20, true),
            ("far-only", 30, true),
            ("near", 30, false)
        ]);
        let today = Feed::new(&sig, DitherMode::MicAndReference, true);
        let clean = Feed::new(&sig, DitherMode::ReferenceOnly, true);
        let none = Feed::new(&sig, DitherMode::None, true);
        assert_eq!(none.mic, sig.mic);
        assert_eq!(none.reference, sig.far);
        assert_eq!(clean.mic, sig.mic, "the mic clean");
        assert_eq!(clean.reference, today.reference, "the reference gets today's dither");
        let quiet = 0..20 * BLOCK;
        assert!(sig.mic[quiet.clone()].iter().chain(&sig.far[quiet.clone()]).all(|&v| v == 0.0), "the quiet phase is exact zero");
        assert!(today.mic[quiet.clone()].iter().all(|&v| v != 0.0 && v.abs() <= signals::DITHER), "today's mic is dithered");
        let far_only = 20 * BLOCK..50 * BLOCK;
        assert!(sig.mic[far_only.clone()].iter().all(|&v| v == 0.0) && sig.far[far_only].iter().any(|&v| v != 0.0));
        let no_reference = Feed::new(&sig, DitherMode::ReferenceOnly, false);
        assert!(no_reference.reference.iter().all(|&v| v != 0.0 && v.abs() <= signals::DITHER));
    }

    #[test]
    fn a_phase_s_zeros_and_when_they_start() {
        let mut y = vec![1e-9f32; 10 * BLOCK];
        y[2 * BLOCK..3 * BLOCK].fill(0.0); // a first all-zero block, then live again
        y[6 * BLOCK..].fill(0.0); // all zero from block 6
        y[5 * BLOCK] = f32::MIN_POSITIVE / 2.0;
        y[0] = 0.5; // the peak, in the first half
        let o = output(&y);
        assert_eq!((o.first_zero_ms, o.all_zero_ms), (Some(20.0), Some(60.0)));
        assert!((o.zero_share - 5.0 / 10.0).abs() < 1e-12, "{}", o.zero_share);
        assert!((o.subnormal_share - 1.0 / (10 * BLOCK) as f64).abs() < 1e-12);
        assert!((o.peak_dbfs.unwrap() + 6.02).abs() < 0.01 && (o.late_peak_dbfs.unwrap() + 180.0).abs() < 0.01, "{o:?}");
        let never = output(&[1e-9; BLOCK]);
        assert_eq!((never.first_zero_ms, never.all_zero_ms, never.zero_share), (None, None, 0.0));
        let silent = output(&[0.0, -0.0, 0.0]);
        assert_eq!((silent.first_zero_ms, silent.all_zero_ms, silent.peak_dbfs, silent.zero_share), (Some(0.0), Some(0.0), None, 1.0));
    }

    #[test]
    fn differences_and_percentiles() {
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
    }
}
