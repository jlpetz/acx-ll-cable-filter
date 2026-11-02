//! CPU per hop and per block, release, ignored (TODO 5's tests, item 5): `cargo test -p nf-ort --release --test ort --
//! --ignored timing --nocapture`. The same code `noise-filter bench --model ulunas` runs (`nf_ort::bench`), with RNNoise's
//! rows beside it, and then the worker fed as bursty wakes deliver, at real time (`nf_ort::bench::bursts`: depth 1 and 2,
//! wakes of 1, 2 and 3 blocks). The dev box is a VM over Remote Desktop: these figures say nothing about the owner's
//! desktops.

use std::any::Any;
use std::sync::Arc;

use nf_core::noise::{WorkerCounters, WorkerGuard, WorkerHooks};
use nf_ort::bench::{self, ProbeHooks};
use nf_stages::bench::Signals;

use crate::{common, rust_allocs};

/// The worker's own Rust allocations, by this binary's counting allocator (the app's hooks read nf-mem's pool counters).
struct Counted;

impl WorkerHooks for Counted {
    fn enter(&self) -> WorkerGuard {
        WorkerGuard::none()
    }

    fn steady(&self) -> Box<dyn Any> {
        Box::new(())
    }

    fn counters(&self) -> WorkerCounters {
        WorkerCounters { pool_allocs: rust_allocs(), ..WorkerCounters::default() }
    }
}

#[test]
#[ignore = "timing: release only, prints the rows"]
fn timing() {
    let Some(p) = common::pack() else { return };
    let probe = common::probe();
    let sig = Signals::synthetic(30.0);
    let r = bench::measure(&p, &sig, Some(rust_allocs), Arc::new(ProbeHooks(Arc::new(Counted)))).expect("bench");
    eprintln!("probe sees the C runtime: {}", probe.sees_the_crt());
    for row in &r.rows {
        eprintln!("{}", row.line());
    }
    let w = r.worker;
    eprintln!(
        "worker: {} jobs, {} runs, {} misses, {} late, {} gaps, {} errors; run µs last {:.0} p50 {:.0} p99 {:.0} max {:.0}; \
         worker Rust allocations {} (fallbacks {}), C heap calls {:?} ({:.1} a run)",
        w.jobs,
        w.runs,
        w.misses,
        w.late,
        w.gaps,
        w.errors,
        w.run_us_last,
        w.run_us_p50,
        w.run_us_p99,
        w.run_us_max,
        w.worker_pool_allocs,
        w.worker_fallbacks,
        w.worker_crt_heap_calls,
        w.worker_crt_heap_calls.unwrap_or(0) as f64 / w.runs.max(1) as f64
    );
    // The audio side of the worker row makes no allocation, and the worker none in Rust once ready.
    let audio_side = &r.rows[3];
    assert_eq!(audio_side.times.allocations, Some(0), "{}", audio_side.line());
    assert_eq!(audio_side.crt_calls.unwrap_or(0), 0, "{}", audio_side.line());
    assert_eq!((w.worker_pool_allocs, w.misses, w.gaps), (0, 0, 0), "{w:?}");
    // Bursty wakes at real time: a wake of k blocks costs about max(0, k - depth) misses (`bench::bursts`).
    for depth in [1u32, 2] {
        for burst in [1usize, 2, 3] {
            let row = bench::bursts(&p, &sig.noisy, Arc::new(Counted), depth, burst, 300).expect("bursts");
            eprintln!("{}", row.line());
            assert_eq!(row.stats.gaps, 0, "{}", row.line());
            if burst <= depth as usize {
                assert!(row.stats.misses <= 3, "a depth of at least the burst keeps up: {}", row.line());
            }
        }
    }
}
