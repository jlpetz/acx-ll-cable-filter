//! `bench [IN.wav] [--seconds N] [--aec]`: offline per-block CPU of the noise stage around RNNoise, the
//! RNNoise model alone, the floor and gate after it, AEC3 (with a synthetic far end as its reference)
//! and the whole chain (with AEC3 under `--aec`). It is `nf_stages::bench`, the code `tests/timing.rs`
//! runs (TODO 2's check 3), timed inside an `RtScope` as the audio thread runs, so each row also counts
//! the allocations made after the warm-up (`nf_mem::thread_scope_allocs`). No devices.
//!
//! `--model ID` (a build with the `onnx` feature) times a model pack instead, beside RNNoise (`nf_ort::bench`): the ONNX
//! Runtime run and the model per hop, the stage per block with its model inline, and the audio thread's side per block with
//! the model on its worker (entered as the engine enters it, MMCSS and the real-time scope), with the heap probe installed
//! so each row also counts the C runtime's heap calls, and the worker's. Then the worker fed as bursty wakes deliver, at real
//! time: wakes of 1, 2 and 3 blocks at depth 1 and at the config's default, 2 (`nf_ort::bench::bursts`, 3 s each; TODO 6).

use nf_stages::bench::{self, AllocCounter, BlockTimes, Signals};

use crate::cli::BenchArgs;
use crate::exit::Failure;
use crate::wav;

pub fn bench(args: &BenchArgs) -> Result<(), Failure> {
    if let Some(id) = args.model.as_deref().filter(|m| *m != nf_engine::DEFAULT_NOISE_MODEL) {
        return bench_model(args, id);
    }
    let source = match &args.input {
        Some(path) => format!("{} as the near end", path.display()),
        None => "a synthetic voice over pink noise".to_owned(),
    };
    say!("Timing {} s of {source}, each block alone after a 1 s warm-up. Memory: {}.", args.seconds, memory());
    let rows = measure(args)?;
    for row in &rows {
        say!("{}", row.line());
    }
    say!(
        "allocs: allocations after the warm-up, inside the real-time scope. The noise stage, its model, its floor and gate, \
         AEC3 (sonora 0.3.0) and the chain make none."
    );
    Ok(())
}

/// The rows `bench` prints, timed on this thread.
pub fn measure(args: &BenchArgs) -> Result<Vec<BlockTimes>, Failure> {
    // The signals are made outside the scope: tens of MB that the pool isn't for.
    let signals = match &args.input {
        Some(path) => Signals::with_near(&wav::read_mono_48k(path)?.samples, args.seconds),
        None => Signals::synthetic(args.seconds),
    };
    let count: Option<AllocCounter> = Some(nf_mem::thread_scope_allocs);
    let fail = |e: nf_core::stage::StageError| Failure::error(format!("Couldn't set up a stage: {e}"));
    // The stages are built and run inside the scope, as on the audio thread, which first builds
    // easyfft's per-thread state outside it so it doesn't keep ~22 KB of the pool (review RT-1)
    nf_stages::warm_up_fft_thread();
    let _scope = nf_mem::RtScope::enter();
    Ok(vec![
        bench::rnnoise(&signals, count).map_err(fail)?,
        bench::rnnoise_model(&signals, count).map_err(fail)?,
        bench::noise_post(&signals, count),
        bench::aec3(&signals, count).map_err(fail)?,
        bench::chain(&signals, args.aec, count).map_err(fail)?,
    ])
}

#[cfg(not(feature = "onnx"))]
fn bench_model(_args: &BenchArgs, id: &str) -> Result<(), Failure> {
    Err(Failure::usage(format!("noise model '{id}' needs a build with --features onnx (this one has RNNoise only)")))
}

#[cfg(feature = "onnx")]
fn bench_model(args: &BenchArgs, id: &str) -> Result<(), Failure> {
    use std::sync::Arc;
    let pack = nf_ort::Pack::installed(id).map_err(|e| Failure::error(e.0))?;
    let probe = nf_ort::heap_probe::install().map_err(Failure::error)?;
    let signals = match &args.input {
        Some(path) => Signals::with_near(&wav::read_mono_48k(path)?.samples, args.seconds),
        None => Signals::synthetic(args.seconds),
    };
    say!(
        "Timing {} ({}) over {} s, after a 1 s warm-up; {}. Memory: {}. Heap probe: {}.",
        pack.manifest.name,
        nf_ort::env::build_info(),
        args.seconds,
        "the worker row paced at 4x real time",
        memory(),
        if probe.sees_the_crt() { "on" } else { "blind (no C runtime imports to hook)" }
    );
    let hooks = Arc::new(nf_ort::bench::ProbeHooks(Arc::new(nf_engine::models::EngineHooks::default())));
    nf_stages::warm_up_fft_thread();
    let report = {
        let _scope = nf_mem::RtScope::enter();
        nf_ort::bench::measure(&pack, &signals, Some(nf_mem::thread_scope_allocs), hooks).map_err(Failure::error)?
    };
    for row in &report.rows {
        say!("{}", row.line());
    }
    let w = report.worker;
    say!(
        "worker: {} runs, {} misses, {} late, {} gaps, {} errors; run p50 {:.0} µs, p99 {:.0}, max {:.0}; its Rust allocations {} \
         (fallbacks {}); its C heap calls {} ({:.1} a run)",
        w.runs,
        w.misses,
        w.late,
        w.gaps,
        w.errors,
        w.run_us_p50,
        w.run_us_p99,
        w.run_us_max,
        w.worker_pool_allocs,
        w.worker_fallbacks,
        w.worker_crt_heap_calls.map_or("--".into(), |c| c.to_string()),
        w.worker_crt_heap_calls.unwrap_or(0) as f64 / w.runs.max(1) as f64,
    );
    say!(
        "allocs: Rust allocations after the warm-up on this thread; C heap calls: the C runtime's (ONNX Runtime's), on this \
         thread. The audio side of the worker row makes neither: ONNX Runtime's per-run heap calls are the worker's (TODO 6)."
    );
    say!("The worker fed as bursty wakes deliver, at real time (a wake of k blocks costs about k - depth misses):");
    for depth in [nf_engine::STEADY_MODEL_DEPTH, nf_engine::BURSTY_MODEL_DEPTH] {
        for burst in [1, 2, 3] {
            let worker = Arc::new(nf_engine::models::EngineHooks::default());
            let row = nf_ort::bench::bursts(&pack, &signals.noisy, worker, depth, burst, 300).map_err(Failure::error)?;
            say!("  {}", row.line());
        }
    }
    Ok(())
}

fn memory() -> String {
    nf_mem::pool_status().map_or_else(|| "system heap (no locked pool)".to_owned(), |s| s.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real global allocator (main.rs's `RtAllocator`) counts the scope's allocations, with no
    /// pool installed (they fall back to the system heap, and still count).
    #[test]
    fn bench_times_each_row_and_no_row_allocates_once_warm() {
        let dir = crate::test_dir("bench");
        let path = dir.join("near.wav");
        let near: Vec<f32> = (0..24_000).map(|i| ((i as f32) * 0.03).sin() * 0.1).collect();
        wav::write_mono_f32(&path, &near).unwrap();
        for (input, aec) in [(None, false), (Some(path.clone()), true)] {
            let rows = measure(&BenchArgs { input, seconds: 2.0, aec, model: None }).unwrap();
            let names: Vec<_> = rows.iter().map(|r| r.name).collect();
            let chain = if aec { "chain: meters + AEC3 + RNNoise" } else { "chain: meters + RNNoise" };
            let noise = ["RNNoise (noisy speech)", "RNNoise model alone", "floor + gate (NoisePost, auto gate)"];
            assert_eq!(names, [noise[0], noise[1], noise[2], "AEC3 (echo + near end + noise)", chain]);
            for r in &rows {
                // 2 s is 200 blocks, 100 of them warm-up
                assert_eq!(r.blocks(), 100, "{}", r.name);
                assert!(r.p50() > 0.0 && r.p50() <= r.p99() && r.p99() <= r.max(), "{}", r.line());
            }
            // The noise stage, its model alone, its floor and gate, AEC3 and the chain, with AEC3 or without: nothing once
            // warm (sonora 0.3.0; 0.2.0 made about 43 a block).
            for r in &rows {
                assert_eq!(r.allocations, Some(0), "{}", r.line());
            }
        }
        let _ = std::fs::remove_dir_all(dir);
        // And the counter sees the scope's allocations: sonora 0.2.0's AEC3 row showed it until 0.3.0 allocated nothing.
        let seen = {
            let _scope = nf_mem::RtScope::enter();
            let a0 = nf_mem::thread_scope_allocs();
            drop(std::hint::black_box(vec![0u8; 64]));
            nf_mem::thread_scope_allocs() - a0
        };
        assert_eq!(seen, 1, "the scope's counter missed an allocation");
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let e = measure(&BenchArgs { input: Some("no-such-file.wav".into()), seconds: 2.0, aec: false, model: None }).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Error);
        let e = bench(&BenchArgs { input: None, seconds: 2.0, aec: false, model: Some("no-such-pack".into()) }).unwrap_err();
        assert!(e.message.contains("no-such-pack"), "{}", e.message);
    }
}
