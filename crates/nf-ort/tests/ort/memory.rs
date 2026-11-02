//! Allocations (TODO 5's tests, item 6; TODO 6's measurement, `doc/todo/6-inference-memory.md`):
//! - the audio thread's side of the pipelined stage makes no Rust allocation and no C runtime or direct heap call per block;
//! - the heap probe sees ONNX Runtime's heap (the self-check: a session's creation makes thousands of C runtime
//!   allocations), its catch-all (ucrtbase's own heap calls) agrees with its per-caller hooks on a run, and the Rust side of
//!   a run makes none;
//! - building and warming up a session leaves the building thread's MXCSR as it was (no FTZ/DAZ: `session.rs`), checked in
//!   a process of its own, where that session is the first (ONNX Runtime sets the flags once a process);
//! - `todo6_measurement` (ignored; release): the figures for the doc, the steady state and its variants.

use std::sync::Arc;
use std::time::Instant;

use nf_core::BLOCK;
use nf_core::config::ProcessingConfig;
use nf_core::dsp::stft::{Stft, hann};
use nf_core::noise::{DEFAULT_DEPTH, Executor, NoHooks};
use nf_core::stage::{Activation, ProcessIo, Stage};
use nf_ort::heap_probe::{self, HeapCounts};
use nf_ort::{ModelSession, OrtSpectralModel, SessionOptions};
use nf_stages::harness::scenes;

use crate::common::{self, controls, read16};
use crate::rust_allocs;

/// The native model inputs of `x` (16 kHz), one frame per hop.
fn frames(x: &[f32]) -> Vec<Vec<f32>> {
    let mut s = Stft::new(&hann(512, true), 256);
    x.as_chunks::<256>().0.iter().map(|h| s.push(h).iter().flat_map(|c| [c.re, c.im]).collect()).collect()
}

#[test]
fn the_audio_side_makes_no_allocation_rust_or_c_runtime() {
    let Some(p) = common::pack() else { return };
    let probe = common::probe();
    assert!(probe.sees_the_crt(), "the probe hooks malloc and free: {probe:?}");
    let exec = Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: false };
    let (mut s, stats) =
        nf_ort::pack_stage(controls(ProcessingConfig::default()), p, exec, SessionOptions::default(), DEFAULT_DEPTH).expect("stage");
    s.activate(&Activation::HOST).unwrap();
    let x = scenes::noisy_speech(6 * 48_000, nf_stages::synth::Voice::LOW, 10.0, 7).mix;
    let mut out = [0.0f32; BLOCK];
    let blocks = x.as_chunks::<BLOCK>().0;
    // Warm: the first blocks, unmeasured.
    for b in &blocks[..50] {
        s.process(ProcessIo::new(&[b], &mut out));
    }
    let rust = rust_allocs();
    let start = Instant::now();
    let ((), heap) = heap_probe::counting(|| {
        for (k, b) in blocks[50..].iter().enumerate() {
            // About 4× real time, so the worker keeps up as it would live.
            while start.elapsed().as_micros() < 2500 * k as u128 {
                std::hint::spin_loop();
            }
            s.process(ProcessIo::new(&[b], &mut out));
            if k == 200 {
                s.reset();
            }
        }
    });
    let rust = rust_allocs() - rust;
    let st = stats.snapshot();
    eprintln!("audio side over {} blocks: {rust} Rust allocations, {heap:?}; the worker: {st:?}", blocks.len() - 50);
    assert_eq!(rust, 0, "Rust allocations on the audio side");
    assert_eq!((heap.crt_calls(), heap.heap_calls(), heap.ucrt_heap_calls()), (0, 0, 0), "heap calls on the audio side: {heap:?}");
    assert!(st.runs > 300, "the worker ran: {st:?}");
}

#[test]
fn the_probe_sees_onnx_runtime_s_heap_and_a_run_s_rust_side_allocates_nothing() {
    let Some(p) = common::pack() else { return };
    assert!(common::probe().sees_the_crt());
    let t = Instant::now();
    let (s, created) = heap_probe::counting(|| ModelSession::new(&p, SessionOptions::default()).expect("session"));
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let mut s = s;
    eprintln!("session creation: {created:?} in {ms:.0} ms");
    assert!(created.crt_allocs > 1000, "the probe sees ONNX Runtime's allocations (else it is blind): {created:?}");
    let f = frames(&read16(&p, "conformance/noisy_0174.wav"));
    let ((), first) = heap_probe::counting(|| {
        s.mix_mut().copy_from_slice(&f[0]);
        s.run().unwrap();
    });
    eprintln!("the first run: {first:?}");
    for fr in &f[1..50] {
        s.mix_mut().copy_from_slice(fr);
        s.run().unwrap();
    }
    let rust = rust_allocs();
    let ((), steady) = heap_probe::counting(|| {
        for fr in &f[50..250] {
            s.mix_mut().copy_from_slice(fr);
            s.run().unwrap();
        }
    });
    let rust = rust_allocs() - rust;
    eprintln!("200 warm runs: {rust} Rust allocations; {steady:?} ({:.1} C runtime allocations a run)", steady.crt_allocs as f64 / 200.0);
    assert_eq!(rust, 0, "the Rust side of a run (ort's included) allocates nothing");
    // The self-check: the catch-all (every C runtime heap call, whoever made it) sees what the per-caller hooks see on a run.
    if common::probe().has_catch_all() {
        assert_eq!(
            (steady.ucrt_heap_allocs + steady.ucrt_heap_reallocs, steady.ucrt_heap_frees),
            (steady.crt_allocs + steady.crt_reallocs, steady.crt_frees),
            "the catch-all and the per-caller hooks disagree on a run: {steady:?}"
        );
        assert!(created.ucrt_heap_allocs >= created.crt_allocs, "the catch-all sees at least as much of a creation: {created:?}");
    }
    // TODO 6's figure, pinned from the first measurement (3,710 a run with ONNX Runtime 1.31.0): a bound that catches an
    // update or an option that makes a run heavier on the worker.
    assert!(steady.crt_allocs <= 200 * MAX_CRT_ALLOCS_PER_RUN, "{} C runtime allocations a run", steady.crt_allocs / 200);
}

/// The most C runtime allocations one UL-UNAS run may make on the worker (measured: 3,710).
const MAX_CRT_ALLOCS_PER_RUN: u64 = 5000;

/// The calling thread's MXCSR.
fn mxcsr() -> u32 {
    let mut v = 0u32;
    // SAFETY: `stmxcsr` stores the 32-bit MXCSR into `v`, a live local; it reads no memory and changes no flags.
    unsafe { std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut v, options(nostack, preserves_flags)) };
    v
}

/// MXCSR's control bits (FTZ 0x8000, rounding 0x6000, the exception masks 0x1f80, DAZ 0x40), without its status flags,
/// which ordinary arithmetic sets.
const MXCSR_CONTROL: u32 = !0x3f;

/// The check runs in a process of its own (this binary, running just the test below), so the session it builds is that
/// process's first: ONNX Runtime applies `set_denormal_as_zero` once a process, to the first session's thread.
#[test]
fn building_and_warming_a_session_leaves_the_thread_s_mxcsr_alone() {
    let Some(_) = common::pack() else { return };
    let out = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "memory::mxcsr_in_a_fresh_process", "--ignored", "--nocapture", "--test-threads", "1"])
        .output()
        .expect("the child");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    eprintln!("{}", text.lines().filter(|l| l.starts_with("MXCSR")).collect::<Vec<_>>().join("\n"));
    assert!(out.status.success() && text.contains("1 passed"), "the child process: {text}");
}

#[test]
#[ignore = "run by building_and_warming_a_session_leaves_the_thread_s_mxcsr_alone, in a process of its own"]
fn mxcsr_in_a_fresh_process() {
    let Some(p) = common::pack() else { return };
    let before = mxcsr();
    let mut model = OrtSpectralModel::new(&p, controls(ProcessingConfig::default()), SessionOptions::default()).expect("model");
    let built = mxcsr();
    nf_core::noise::NoiseModel::prepare(&mut model).expect("the warm-up");
    let warmed = mxcsr();
    eprintln!("MXCSR before {before:#06x}, after the session {built:#06x}, after the warm-up {warmed:#06x}");
    assert_eq!((built & MXCSR_CONTROL, warmed & MXCSR_CONTROL), (before & MXCSR_CONTROL, before & MXCSR_CONTROL));
    // And a denormal survives on this thread: no FTZ or DAZ.
    let tiny = std::hint::black_box(1e-38f32);
    assert!(std::hint::black_box(tiny * 0.01) > 0.0, "a denormal product flushed to 0");
}

/// Per-run statistics over `n` runs after `warm` (this thread counting).
struct Steady {
    allocs: Vec<u64>,
    frees: Vec<u64>,
    bytes: Vec<u64>,
    /// The catch-all's allocations (and reallocations) and frees, summed.
    ucrt: (u64, u64),
    heap: u64,
    rust: u64,
    us: Vec<f64>,
    cycles: Vec<u64>,
    faults_per_1000: f64,
    private_before: usize,
    private_after: usize,
    blocks_before: (u64, u64),
    blocks_after: (u64, u64),
}

fn steady(s: &mut ModelSession, f: &[Vec<f32>], warm: usize, n: usize) -> Steady {
    for i in 0..warm {
        s.mix_mut().copy_from_slice(&f[i % f.len()]);
        s.run().unwrap();
    }
    let (faults0, private_before) = heap_probe::process_memory();
    let blocks_before = heap_probe::live_heap_blocks();
    let mut st = Steady {
        allocs: Vec::with_capacity(n),
        frees: Vec::with_capacity(n),
        bytes: Vec::with_capacity(n),
        ucrt: (0, 0),
        heap: 0,
        rust: 0,
        us: Vec::with_capacity(n),
        cycles: Vec::with_capacity(n),
        faults_per_1000: 0.0,
        private_before,
        private_after: 0,
        blocks_before,
        blocks_after: (0, 0),
    };
    let rust0 = rust_allocs();
    for i in 0..n {
        s.mix_mut().copy_from_slice(&f[(warm + i) % f.len()]);
        let (c0, t) = (heap_probe::thread_cycles(), Instant::now());
        let ((), h): ((), HeapCounts) = heap_probe::counting(|| s.run().unwrap());
        st.us.push(t.elapsed().as_secs_f64() * 1e6);
        st.cycles.push(heap_probe::thread_cycles() - c0);
        st.allocs.push(h.crt_allocs);
        st.frees.push(h.crt_frees);
        st.bytes.push(h.crt_bytes);
        st.ucrt.0 += h.ucrt_heap_allocs + h.ucrt_heap_reallocs;
        st.ucrt.1 += h.ucrt_heap_frees;
        st.heap += h.heap_calls();
    }
    st.rust = rust_allocs() - rust0;
    let (faults1, private_after) = heap_probe::process_memory();
    st.faults_per_1000 = f64::from(faults1 - faults0) * 1000.0 / n as f64;
    st.private_after = private_after;
    st.blocks_after = heap_probe::live_heap_blocks();
    st.us.sort_by(f64::total_cmp);
    st.cycles.sort_unstable();
    st
}

fn pct<T: Copy>(v: &[T], p: f64) -> T {
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn mmm(v: &[u64]) -> String {
    let mut s = v.to_vec();
    s.sort_unstable();
    format!("{}/{}/{}", s[0], pct(&s, 0.5), s[s.len() - 1])
}

fn print(name: &str, st: &Steady) {
    let n = st.us.len() as f64;
    eprintln!(
        "{name:<34} CRT allocs/frees per run (min/median/max) {} / {}, bytes per run median {}; the catch-all {:.1} / {:.1} a run; \
         direct heap calls {:.2}/run; Rust {:.2}/run; soft faults {:.1} per 1000 runs; private {:.1} → {:.1} MB; heap blocks {} → {}; \
         run µs p50 {:.1} p99 {:.1} p99.9 {:.1} max {:.1}; cycles p50 {}",
        mmm(&st.allocs),
        mmm(&st.frees),
        pct(
            &{
                let mut b = st.bytes.clone();
                b.sort_unstable();
                b
            },
            0.5
        ),
        st.ucrt.0 as f64 / n,
        st.ucrt.1 as f64 / n,
        st.heap as f64 / n,
        st.rust as f64 / n,
        st.faults_per_1000,
        st.private_before as f64 / 1e6,
        st.private_after as f64 / 1e6,
        st.blocks_before.0,
        st.blocks_after.0,
        pct(&st.us, 0.5),
        pct(&st.us, 0.99),
        pct(&st.us, 0.999),
        st.us[st.us.len() - 1],
        pct(&st.cycles, 0.5),
    );
}

/// TODO 6's figures (`doc/todo/6-inference-memory.md`, "Measured"): `cargo test -p nf-ort --release --test ort -- --ignored
/// todo6 --nocapture --test-threads 1`.
#[test]
#[ignore = "TODO 6's measurement: release, one test thread"]
fn todo6_measurement() {
    let Some(p) = common::pack() else { return };
    let probe = common::probe();
    eprintln!(
        "probe: {} imports patched, {} skipped; sees the C runtime: {}; the catch-all (ucrtbase's heap imports): {}",
        probe.patched.len(),
        probe.skipped.len(),
        probe.sees_the_crt(),
        probe.has_catch_all()
    );
    for l in &probe.patched {
        eprintln!("  {l}");
    }
    eprintln!("{}", nf_ort::env::build_info());
    let speech = frames(&read16(&p, "conformance/noisy_0174.wav"));
    let silence = vec![vec![0.0f32; 514]; 64];
    let dithered = frames(&scenes::dithered_silence(64 * 256 + 512, 3));
    // Session creation and the first run.
    let t = Instant::now();
    let (mut s, created) = heap_probe::counting(|| ModelSession::new(&p, SessionOptions::default()).unwrap());
    eprintln!(
        "session creation: {} CRT allocs, {} frees, {} bytes, {} direct heap calls, {:.1} ms; the catch-all: {} allocs, {} reallocs, \
         {} frees, {} bytes",
        created.crt_allocs,
        created.crt_frees,
        created.crt_bytes,
        created.heap_calls(),
        t.elapsed().as_secs_f64() * 1e3,
        created.ucrt_heap_allocs,
        created.ucrt_heap_reallocs,
        created.ucrt_heap_frees,
        created.ucrt_heap_bytes
    );
    let ((), first) = heap_probe::counting(|| {
        s.mix_mut().copy_from_slice(&speech[0]);
        s.run().unwrap();
    });
    eprintln!(
        "first run: {} CRT allocs, {} frees, {} bytes; the catch-all {} allocs, {} frees",
        first.crt_allocs, first.crt_frees, first.crt_bytes, first.ucrt_heap_allocs, first.ucrt_heap_frees
    );
    print("default (speech), 10000 runs", &steady(&mut s, &speech, 1000, 10_000));
    drop(s);
    let variants: [(&str, SessionOptions); 6] = [
        ("memory pattern off", SessionOptions { memory_pattern: false, ..SessionOptions::default() }),
        ("arena off", SessionOptions { arena: false, ..SessionOptions::default() }),
        ("optimisation level 1", SessionOptions { opt_level: 1, ..SessionOptions::default() }),
        ("optimisation level 2", SessionOptions { opt_level: 2, ..SessionOptions::default() }),
        ("outputs not preallocated", SessionOptions { preallocate_outputs: false, ..SessionOptions::default() }),
        ("default (again)", SessionOptions::default()),
    ];
    for (name, o) in variants {
        let mut s = ModelSession::new(&p, o).unwrap();
        print(name, &steady(&mut s, &speech, 500, 3000));
    }
    for (name, f) in [("exact digital silence", &silence), ("dithered silence (±1e-9)", &dithered)] {
        let mut s = ModelSession::new(&p, SessionOptions::default()).unwrap();
        print(name, &steady(&mut s, f, 500, 3000));
    }
}
