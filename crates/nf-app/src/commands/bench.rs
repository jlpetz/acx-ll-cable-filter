//! `bench [IN.wav] [--seconds N] [--aec]`: offline per-block CPU of the noise stage around RNNoise, the
//! RNNoise model alone, the floor and gate after it, AEC3 (with a synthetic far end as its reference)
//! and the whole chain (with AEC3 under `--aec`). It is `nf_stages::bench`, the code `tests/timing.rs`
//! runs (TODO 2's check 3), timed inside an `RtScope` as the audio thread runs, so each row also counts
//! the allocations made after the warm-up (`nf_mem::thread_scope_allocs`). No devices.

use nf_stages::bench::{self, AllocCounter, BlockTimes, Signals};

use crate::cli::BenchArgs;
use crate::exit::Failure;
use crate::wav;

pub fn bench(args: &BenchArgs) -> Result<(), Failure> {
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
         and a chain without AEC3 make none; sonora 0.2.0 makes about 43 a block (served by the locked pool in the app)."
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

fn memory() -> String {
    nf_mem::pool_status().map_or_else(|| "system heap (no locked pool)".to_owned(), |s| s.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real global allocator (main.rs's `RtAllocator`) counts the scope's allocations, with no
    /// pool installed (they fall back to the system heap, and still count).
    #[test]
    fn bench_times_each_row_and_the_noise_rows_allocate_nothing_once_warm() {
        let dir = crate::test_dir("bench");
        let path = dir.join("near.wav");
        let near: Vec<f32> = (0..24_000).map(|i| ((i as f32) * 0.03).sin() * 0.1).collect();
        wav::write_mono_f32(&path, &near).unwrap();
        for (input, aec) in [(None, false), (Some(path.clone()), true)] {
            let rows = measure(&BenchArgs { input, seconds: 2.0, aec }).unwrap();
            let names: Vec<_> = rows.iter().map(|r| r.name).collect();
            let chain = if aec { "chain: meters + AEC3 + RNNoise" } else { "chain: meters + RNNoise" };
            let noise = ["RNNoise (noisy speech)", "RNNoise model alone", "floor + gate (NoisePost, auto gate)"];
            assert_eq!(names, [noise[0], noise[1], noise[2], "AEC3 (echo + near end + noise)", chain]);
            for r in &rows {
                // 2 s is 200 blocks, 100 of them warm-up
                assert_eq!(r.blocks(), 100, "{}", r.name);
                assert!(r.p50() > 0.0 && r.p50() <= r.p99() && r.p99() <= r.max(), "{}", r.line());
            }
            // The noise stage, its model alone, and its floor and gate: nothing once warm.
            for r in &rows[..3] {
                assert_eq!(r.allocations, Some(0), "{}", r.line());
            }
            if !aec {
                assert_eq!(rows[4].allocations, Some(0), "{}", rows[4].line());
            }
            // sonora 0.2.0 allocates per block; the counter sees it
            assert!(rows[3].allocations.is_some_and(|a| a > 0), "{}", rows[3].line());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let e = measure(&BenchArgs { input: Some("no-such-file.wav".into()), seconds: 2.0, aec: false }).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Error);
    }
}
