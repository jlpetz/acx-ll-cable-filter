//! The real-time rule (CLAUDE.md) for the stages: after `build_chain` (which warms up the calling
//! thread), the RNNoise chain's per-block processing makes no heap allocation, and an AEC3 chain
//! makes only sonora 0.2.0's own (sonora-nnnoiseless.md 1.7; upstream PR #40 removes them).
//!
//! A counting global allocator (this test binary only) counts per thread, so tests running in
//! parallel don't disturb each other. Same pattern as nf-core's `tests/rt_no_alloc.rs`.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::Ordering::Relaxed;

use common::*;
use nf_core::chain::Chain;
use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::stage::{ProcessIo, Stage};
use nf_core::{BLOCK, Block};
use nf_stages::build_chain;
use nf_stages::synth::*;

struct Counting;

thread_local! {
    // Const-initialised and without a destructor, so reading it never allocates.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees hold; the only addition is a thread-local counter that doesn't allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded as received; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: as above; `ptr` came from this allocator, which is `System` underneath.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations made on this thread while `f` runs.
fn allocations_in(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

/// Noisy speech, and a far end with its echo, as whole blocks (built before counting starts).
struct Signals {
    mic: Vec<f32>,
    reference: Vec<f32>,
}

fn signals(seconds: usize) -> Signals {
    let len = seconds * SR;
    let far = speech_like(len, Voice::LOW, 1);
    let near = speech_like(len, Voice::HIGH, 2);
    let echo = scaled(&delayed(&far.samples, 1920), 0.4);
    let noise = pink_noise(len, 0.003, 3);
    Signals { mic: add(&add(&near.samples, &echo), &noise), reference: far.samples }
}

impl Signals {
    fn block(&self, b: usize) -> (&Block, &Block) {
        let n = self.mic.len() / BLOCK;
        let r = (b % n) * BLOCK..(b % n + 1) * BLOCK;
        (block(&self.mic[r.clone()]), block(&self.reference[r]))
    }
}

/// Per-block allocation counts of `chain` over `blocks` blocks, with `before(b)` run first each block.
fn per_block(chain: &mut Chain, s: &Signals, blocks: std::ops::Range<usize>, mut before: impl FnMut(usize)) -> Vec<usize> {
    let mut out = [0.0f32; BLOCK];
    let mut counts = Vec::with_capacity(blocks.len());
    for b in blocks {
        before(b);
        let (m, r) = s.block(b);
        counts.push(allocations_in(|| {
            let report = chain.process(m, r, &mut out);
            std::hint::black_box((report, &out));
        }));
    }
    counts
}

#[test]
fn the_counter_sees_allocations() {
    assert!(allocations_in(|| drop(std::hint::black_box(vec![1u8; 64]))) >= 1);
}

#[test]
fn the_rnnoise_chain_does_not_allocate_after_warm_up() {
    let s = signals(6);
    let c = controls(ProcessingConfig::default());
    let mut chain = build_chain(c.clone(), false).expect("chain");
    let gates = [GateMode::Off, GateMode::Static, GateMode::Auto]
        .map(|mode| GateConfig { mode, threshold_db: -45.0, attack_ms: 3.0, ..GateConfig::default() });
    // Every gate mode, floor moves (ramps), meters on and off: still nothing allocates.
    let counts = per_block(&mut chain, &s, 0..900, |b| {
        c.noise.gate.apply(&gates[b / 50 % 3]);
        c.noise.floor_db.store(-60.0 + (b / 70 % 4) as f32 * 12.0);
        c.meters_enabled.store(b % 13 != 0, Relaxed);
    });
    let total: usize = counts.iter().sum();
    assert_eq!(total, 0, "allocations in the RNNoise chain: {:?}", counts.iter().enumerate().filter(|c| *c.1 > 0).collect::<Vec<_>>());
}

#[test]
fn rnnoise_resets_allocate_only_once_the_spare_is_used() {
    // Switching the noise stage on resets it (the pristine spare: no allocation). Leaving
    // passthrough resets it again; with the spare used, that builds a fresh state.
    let s = signals(4);
    let c = controls(ProcessingConfig::default());
    let mut chain = build_chain(c.clone(), false).expect("chain");
    let counts = per_block(&mut chain, &s, 0..120, |b| match b {
        10 => c.noise.enabled.store(false, Relaxed),
        20 => c.noise.enabled.store(true, Relaxed), // reset 1: swaps the spare in
        40 => c.passthrough.store(true, Relaxed),
        50 => c.passthrough.store(false, Relaxed), // reset 2: no spare left
        70 => c.passthrough.store(true, Relaxed),
        80 => c.passthrough.store(false, Relaxed), // reset 3
        _ => {}
    });
    eprintln!("allocations in reset blocks: 20: {}, 50: {}, 80: {}", counts[20], counts[50], counts[80]);
    assert_eq!(counts[20], 0, "the first reset swaps in the pre-built spare");
    for b in [50, 80] {
        assert!((1..=12).contains(&counts[b]), "block {b}: {} allocations (one DenoiseState::new is 10)", counts[b]);
    }
    let others: usize = counts.iter().enumerate().filter(|(b, _)| ![50, 80].contains(b)).map(|(_, &n)| n).sum();
    assert_eq!(others, 0, "only the later reset blocks allocate: {counts:?}");
    // activate rebuilds the spare (off the audio path), and the next reset is free again.
    chain.activate().expect("re-activate");
    let counts = per_block(&mut chain, &s, 120..140, |b| match b {
        125 => c.noise.enabled.store(false, Relaxed),
        130 => c.noise.enabled.store(true, Relaxed),
        _ => {}
    });
    assert_eq!(counts.iter().sum::<usize>(), 0, "{counts:?}");
}

#[test]
fn an_aec3_chain_allocates_only_inside_sonora() {
    let s = signals(6);
    let cfg = ProcessingConfig { aec_enabled: true, ..ProcessingConfig::default() };
    let mut chain = build_chain(controls(cfg.clone()), true).expect("chain");
    // The same blocks through AEC3 alone: the chain (and RNNoise in it) must add nothing.
    let mut aec = aec3();
    let mut out = [0.0f32; BLOCK];
    let warm = 50;
    let _ = per_block(&mut chain, &s, 0..warm, |_| {});
    for b in 0..warm {
        let (m, r) = s.block(b);
        aec.process(ProcessIo { input: m, reference: Some(r), output: &mut out });
    }
    let chain_counts = per_block(&mut chain, &s, warm..warm + 400, |_| {});
    let aec_counts: Vec<usize> = (warm..warm + 400)
        .map(|b| {
            let (m, r) = s.block(b);
            allocations_in(|| {
                aec.process(ProcessIo { input: m, reference: Some(r), output: &mut out });
            })
        })
        .collect();
    let mean = chain_counts.iter().sum::<usize>() as f64 / chain_counts.len() as f64;
    let max = chain_counts.iter().copied().max().unwrap_or(0);
    eprintln!("AEC3 chain: {mean:.1} allocations per block on average, {max} at most (sonora 0.2.0)");
    assert_eq!(chain_counts, aec_counts, "the chain and RNNoise add allocations of their own");
    // A guard, not a target: sonora's known per-block Vecs are ~43 (a Rate32kHz config would add
    // a full-band copy per frame).
    assert!(max <= 100, "{max} allocations in one block");
}

#[test]
fn activating_on_another_thread_leaves_the_first_block_to_allocate() {
    // Why the engine activates on the audio thread: easyfft sets up its FFTs per thread.
    let s = signals(1);
    let mut chain = build_chain(controls(ProcessingConfig::default()), false).expect("chain");
    let counts = std::thread::spawn(move || per_block(&mut chain, &s, 0..3, |_| {})).join().expect("thread");
    assert!(counts[0] > 0, "the first block on a new thread sets up easyfft");
    assert_eq!(&counts[1..], &[0, 0]);
}
