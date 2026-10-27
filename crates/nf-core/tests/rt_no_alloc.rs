//! Enforces the real-time rule (CLAUDE.md): after construction and warm-up, every per-block call
//! that runs on the audio thread makes no heap allocation. A counting global allocator (this test
//! binary only) counts allocations per thread, so tests running in parallel don't disturb each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::Chain;
use nf_core::config::{GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::dsp::delay::DelayLine;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::fade::crossfade;
use nf_core::dsp::gate::Gate;
use nf_core::dsp::meter::Meter;
use nf_core::dsp::mix::FloorMix;
use nf_core::dsp::noise_post::NoisePost;
use nf_core::dsp::slip::{Slip, SlipResampler};
use nf_core::latency::stats::RollingStats;
use nf_core::stage::{ProcessIo, Stage, StageError, StageInfo, StageReport, StageRole};
use nf_core::{BLOCK, Block};

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

fn tone(blk: usize) -> Block {
    std::array::from_fn(|i| 0.2 * ((blk * BLOCK + i) as f32 * 0.03).sin())
}

#[test]
fn the_counter_sees_allocations() {
    assert!(allocations_in(|| drop(std::hint::black_box(vec![1u8; 64]))) >= 1);
}

#[test]
fn dsp_blocks_do_not_allocate() {
    let cfg = ProcessingConfig::default();
    let controls = ChainControls::new(&cfg);
    let mut gate = Gate::new();
    let mut mix = FloorMix::new();
    let mut post = NoisePost::new(480);
    let mut delay = DelayLine::new(4096, 480);
    let mut meter = Meter::new();
    let mut slip = SlipResampler::new();
    let mut drift = DriftController::new(960.0);
    let mut stats = RollingStats::<100>::new();
    let mut out = [0.0f32; BLOCK];
    let mut long = [0.0f32; BLOCK + 1];
    let mut short = [0.0f32; BLOCK - 1];
    let input_long = [0.1f32; BLOCK + 1];
    let input_short = [0.1f32; BLOCK - 1];

    let n = allocations_in(|| {
        for blk in 0..200 {
            let x = tone(blk);
            // Every gate mode, and parameter changes that rebuild the coefficient tables.
            let mut p = controls.noise.gate.load();
            p.mode = [GateMode::Off, GateMode::Static, GateMode::Auto][blk % 3];
            p.attack_ms = 1.0 + (blk % 7) as f32;
            p.release_ms = 50.0 + (blk % 5) as f32;
            p.range_db = -30.0 - (blk % 4) as f32;
            let mut g = x;
            gate.process(&mut g, &p, if blk % 2 == 0 { 0.9 } else { f32::NAN });

            let mut w = x;
            mix.process(&tone(blk + 1), &mut w, -20.0 - (blk % 3) as f32);
            let mut w = x;
            controls.noise.floor_db.store(-10.0 - (blk % 9) as f32);
            post.process(&tone(blk + 1), &mut w, &controls.noise, 0.5);

            delay.process(&x, &mut out);
            delay.set_delay(blk * 13 % 4096);
            meter.measure(&x);
            let old = out;
            crossfade(&old, &x, &mut out);

            slip.resample(&x, &mut long);
            slip.resample(&x, &mut short);
            slip.resample(&input_long, &mut out);
            slip.resample(&input_short, &mut out);
            slip.resample(&x, &mut out);

            let s = drift.update(960.0 + (blk % 50) as f32);
            let _ = (s.produced_len(), s.consumed_len(), drift.ppm(), drift.filtered_fill());
            stats.push(blk as f32);
            let _ = (stats.percentile(0.95), stats.median(), stats.max());
        }
    });
    assert_eq!(n, 0, "allocations on the audio path");
    let _ = Slip::None;
}

/// A stage that only scales (it must not allocate either, or the chain test would blame the chain).
struct Scale(f32);

impl Stage for Scale {
    fn info(&self) -> StageInfo {
        StageInfo { id: "scale", name: "scale", role: StageRole::Noise, needs_reference: false, needs_worker: false }
    }
    fn activate(&mut self, _sample_rate: u32, _block: usize) -> Result<(), StageError> {
        Ok(())
    }
    fn latency(&self) -> u32 {
        480
    }
    fn process(&mut self, io: ProcessIo<'_>) -> StageReport {
        for (o, i) in io.output.iter_mut().zip(io.input) {
            *o = *i * self.0;
        }
        StageReport::default()
    }
    fn reset(&mut self) {}
}

#[test]
fn the_chain_does_not_allocate_through_toggles_and_passthrough() {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let mut chain = Chain::new(controls.clone(), Some(Box::new(Scale(0.9))), Some(Box::new(Scale(0.5))));
    chain.activate().expect("activate");
    let reference = [0.0f32; BLOCK];
    let mut out = [0.0f32; BLOCK];
    chain.process(&tone(0), &reference, &mut out); // warm-up

    let n = allocations_in(|| {
        let mut ticks = 0u64;
        for blk in 1..300 {
            controls.aec.enabled.store(blk % 40 < 25, Relaxed);
            controls.noise.enabled.store(blk % 30 < 20, Relaxed);
            controls.passthrough.store(blk % 70 > 50, Relaxed);
            controls.meters_enabled.store(blk % 11 != 0, Relaxed);
            let r = chain.process_timed(&tone(blk), &reference, &mut out, || {
                ticks += 1;
                ticks
            });
            let _ = (r, chain.latency());
        }
    });
    assert_eq!(n, 0, "allocations in Chain::process");
}
