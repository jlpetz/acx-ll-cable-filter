//! The real-time rule (CLAUDE.md) for the stages: after `build_chain` (which warms up the calling
//! thread), the RNNoise chain's per-block processing makes no heap allocation, and an AEC3 chain
//! makes only sonora 0.2.0's own (sonora-nnnoiseless.md 1.7; upstream PR #40 removes them). The
//! per-block reads of the stages' extensions and params (the speech probability, the gain, AEC3's
//! telemetry, `param_value` and `set_param`) make none either.
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
use nf_core::stage::{Activation, ProcessIo, Stage};
use nf_core::{BLOCK, Block};
use nf_stages::synth::*;
use nf_stages::{build_chain, rnnoise_stage};

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
    let gates = [GateMode::Off, GateMode::Static, GateMode::Auto].map(|mode| GateConfig {
        mode,
        threshold_db: -45.0,
        attack_ms: 3.0,
        ..GateConfig::default()
    });
    // Every gate mode, floor moves (ramps), meters on and off: still nothing allocates.
    let counts = per_block(&mut chain, &s, 0..900, |b| {
        c.noise.gate.apply(&gates[b / 50 % 3]);
        c.noise.floor_db.store(-60.0 + (b / 70 % 4) as f32 * 12.0);
        c.meters_enabled.store(b % 13 != 0, Relaxed);
    });
    let total: usize = counts.iter().sum();
    assert_eq!(total, 0, "allocations in the RNNoise chain: {:?}", counts.iter().enumerate().filter(|c| *c.1 > 0).collect::<Vec<_>>());
}

/// Gives RNNoise's spare maker time to replace a spare: it looks every 100 ms, and a reset never waits for it.
fn let_the_maker_run() {
    std::thread::sleep(nf_stages::MAKER_POLL + std::time::Duration::from_millis(50));
}

#[test]
fn rnnoise_resets_never_allocate() {
    // Switching the noise stage on resets it, and so does leaving passthrough: each swaps in a pristine spare that the
    // spare maker built off this thread, and the maker frees the used state there too. Twelve resets, then two back to
    // back (two spares), and a re-activate: none allocates on the processing thread.
    let s = signals(4);
    let c = controls(ProcessingConfig::default());
    let mut chain = build_chain(c.clone(), false).expect("chain");
    // Every 30 blocks from block 10: off (the noise stage, or passthrough on, in turn), and back on 10 blocks later: a reset
    // at blocks 20, 50, ..., 350.
    let counts = per_block(&mut chain, &s, 0..370, |b| match b % 30 {
        10 if b < 360 => {
            let_the_maker_run();
            if b / 30 % 2 == 0 {
                c.noise.enabled.store(false, Relaxed);
            } else {
                c.passthrough.store(true, Relaxed);
            }
        }
        20 if b < 360 => {
            c.noise.enabled.store(true, Relaxed);
            c.passthrough.store(false, Relaxed);
        }
        _ => {}
    });
    assert_eq!(
        counts.iter().sum::<usize>(),
        0,
        "allocations at blocks {:?}",
        counts.iter().enumerate().filter(|c| *c.1 > 0).collect::<Vec<_>>()
    );
    // Two real resets in a row, the maker given no time between them: two spares.
    let_the_maker_run();
    let counts = per_block(&mut chain, &s, 370..410, |b| match b {
        375 | 385 => c.passthrough.store(true, Relaxed),
        380 | 390 => c.passthrough.store(false, Relaxed),
        _ => {}
    });
    assert_eq!(counts.iter().sum::<usize>(), 0, "{counts:?}");
    // activate keeps the maker; the next reset is free again.
    let_the_maker_run();
    chain.activate().expect("re-activate");
    let counts = per_block(&mut chain, &s, 410..430, |b| match b {
        415 => c.noise.enabled.store(false, Relaxed),
        420 => c.noise.enabled.store(true, Relaxed),
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
        aec.process(ProcessIo::new(&[m, r], &mut out));
    }
    let chain_counts = per_block(&mut chain, &s, warm..warm + 400, |_| {});
    let aec_counts: Vec<usize> = (warm..warm + 400)
        .map(|b| {
            let (m, r) = s.block(b);
            allocations_in(|| {
                aec.process(ProcessIo::new(&[m, r], &mut out));
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

#[test]
fn the_extensions_and_params_do_not_allocate_per_block() {
    // What the engine and a future host read and write each block: the noise stage's speech probability, its gain and
    // its params, and AEC3's telemetry (sonora hands out a cached reference).
    let s = signals(2);
    let mut noise =
        rnnoise(ProcessingConfig { gate: GateConfig { mode: GateMode::Auto, ..GateConfig::default() }, ..ProcessingConfig::default() });
    let mut aec = aec3();
    let mut out = [0.0f32; BLOCK];
    for b in 0..20 {
        let (m, r) = s.block(b);
        noise.process(ProcessIo::new(&[m], &mut out));
        aec.process(ProcessIo::new(&[m, r], &mut out));
    }
    let counts: Vec<usize> = (20..200)
        .map(|b| {
            let (m, r) = s.block(b);
            noise.process(ProcessIo::new(&[m], &mut out));
            aec.process(ProcessIo::new(&[m, r], &mut out));
            allocations_in(|| {
                let sp = noise.speech_probability().map_or(f32::NAN, |e| e.speech_probability());
                let gr = noise.gain_adjustment().map_or(f32::NAN, |e| e.gain_adjustment_db());
                let stats = aec.aec_telemetry().map(|t| t.aec_stats());
                let id = (b % 9) as u32 + 1;
                let v = noise.param_value(id).unwrap_or(0.0);
                let set = (noise.set_param(id, v), noise.set_param(id, f64::NAN), noise.set_param(42, 1.0));
                let _ = std::hint::black_box((sp, gr, stats, set));
            })
        })
        .collect();
    assert_eq!(counts.iter().sum::<usize>(), 0, "{counts:?}");
}

#[test]
fn resetting_an_unused_noise_stage_keeps_its_spare() {
    // The stage resets its model only when it processed something since its last reset, so unused resets cost nothing
    // and leave the pristine spare for the first real one.
    let s = signals(1);
    let mut stage = rnnoise_stage(controls(ProcessingConfig::default())).expect("stage");
    stage.activate(&Activation::HOST).expect("activate");
    let mut out = [0.0f32; BLOCK];
    let mut process = |stage: &mut dyn Stage, b: usize| stage.process(ProcessIo::new(&[s.block(b).0], &mut out));
    let unused = allocations_in(|| {
        stage.reset();
        stage.reset();
    });
    process(&mut stage, 0);
    let first = allocations_in(|| stage.reset());
    let_the_maker_run();
    process(&mut stage, 1);
    let second = allocations_in(|| stage.reset());
    assert_eq!((unused, first, second), (0, 0, 0), "unused resets, then a spare each (the maker replaced the first)");
}
