//! sonora's FTZ/DAZ guard and the thread's MXCSR (TODO 10). sonora 0.3.0 as released sets MXCSR's FTZ and DAZ for the length
//! of each `process_*` call and restores the thread's word on return; the vendored copy (vendor/README.md) does so only with
//! nf-stages' `sonora-denormal-guard` feature. Both builds run these tests, and each checks its own behaviour: without the
//! guard MXCSR is unchanged inside a call and after it; with it, FTZ and DAZ are set inside and the word is restored after.
//! Neither may leave them set (no persistent FTZ/DAZ: nf-engine's `audio.rs`).
//!
//! A recording global allocator (this test binary only) reads MXCSR inside a sonora call: a call in another stream format than
//! the last one re-initialises sonora inside the guard, and that allocates. It records per thread, so tests running in
//! parallel don't disturb each other. No devices.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use nf_core::BLOCK;
use nf_stages::synth::*;
use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, Error, StreamConfig};

/// Whether this build has sonora's guard.
const GUARD: bool = cfg!(feature = "sonora-denormal-guard");
/// MXCSR's FTZ (bit 15) and DAZ (bit 6).
const FTZ_DAZ: u32 = 0x8040;
/// MXCSR's control bits (FTZ, rounding, the exception masks, DAZ), without its status flags, which ordinary arithmetic sets.
const CONTROL: u32 = !0x3f;

/// The calling thread's MXCSR.
#[inline(always)]
fn mxcsr() -> u32 {
    let mut v = 0u32;
    // SAFETY: `stmxcsr` stores the 32-bit MXCSR into `v`, a live local; it reads no memory and changes no flags.
    unsafe { std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut v, options(nostack, preserves_flags)) };
    v
}

/// What the allocator saw on this thread while armed.
#[derive(Clone, Copy, Debug)]
struct Seen {
    /// Calls into the allocator.
    calls: usize,
    /// The AND and the OR of the MXCSR control bits those calls ran under: equal when every call saw the same word.
    and: u32,
    or: u32,
}

const NOTHING: Seen = Seen { calls: 0, and: !0, or: 0 };

thread_local! {
    // Const-initialised and without a destructor, so reading them never allocates.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static SEEN: Cell<Seen> = const { Cell::new(NOTHING) };
}

fn record() {
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        let word = mxcsr() & CONTROL;
        let _ = SEEN.try_with(|s| {
            let Seen { calls, and, or } = s.get();
            s.set(Seen { calls: calls + 1, and: and & word, or: or | word });
        });
    }
}

struct Recording;

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s guarantees hold; the only
// addition is reading MXCSR into a thread-local, which doesn't allocate.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: forwarded as received; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record();
        // SAFETY: as above; `ptr` came from this allocator, which is `System` underneath.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record();
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

/// Runs `f` with the allocator recording on this thread; returns its result and what the allocator saw.
fn seen_in<R>(f: impl FnOnce() -> R) -> (R, Seen) {
    SEEN.set(NOTHING);
    ARMED.set(true);
    let r = f();
    ARMED.set(false);
    (r, SEEN.get())
}

/// Inside each of sonora's four entry points MXCSR has FTZ and DAZ set exactly when the build has the guard, its other control
/// bits as the caller left them, and after each call the thread's word is as it was. Each call switches the stream format
/// (48 and 16 kHz in turn), which makes sonora re-initialise inside the call, and allocate: that is where the probe reads.
#[test]
fn sonora_sets_ftz_and_daz_inside_its_calls_only_with_the_guard() {
    let at48 = StreamConfig::new(48_000, 1);
    let at16 = StreamConfig::new(16_000, 1);
    let config = Config { echo_canceller: Some(EchoCanceller::default()), ..Config::default() };
    let mut apm = AudioProcessing::builder().config(config).capture_config(at48).render_config(at48).build();
    let before = mxcsr() & CONTROL;
    assert_eq!(before & FTZ_DAZ, 0, "the test thread starts with FTZ or DAZ set: {before:#06x}");
    let inside = if GUARD { before | FTZ_DAZ } else { before };
    let check = |call: &str, (r, seen): (Result<(), Error>, Seen)| {
        assert!(r.is_ok(), "{call}: {r:?}");
        assert!(seen.calls > 0, "{call}: nothing allocated inside the call, so the probe read nothing");
        assert_eq!((seen.and, seen.or), (inside, inside), "{call}: MXCSR inside {seen:x?}, before {before:#06x}, guard {GUARD}");
        let after = mxcsr() & CONTROL;
        assert_eq!(after, before, "{call}: MXCSR after {after:#06x}, before {before:#06x}");
    };
    let (src16, mut out16) = (vec![0.01f32; 160], vec![0.0f32; 160]);
    let (src48, mut out48) = (vec![100i16; 480], vec![0i16; 480]);
    check("process_capture_f32", seen_in(|| apm.process_capture_f32_with_config(&[&src16[..]], &at16, &at16, &mut [&mut out16[..]])));
    check("process_capture_i16", seen_in(|| apm.process_capture_i16_with_config(&src48, &at48, &at48, &mut out48)));
    check("process_render_f32", seen_in(|| apm.process_render_f32_with_config(&[&src16[..]], &at16, &at16, &mut [&mut out16[..]])));
    check("process_render_i16", seen_in(|| apm.process_render_i16_with_config(&src48, &at48, &at48, &mut out48)));
}

/// The AEC3 stage as the chain runs it, with the guard or without: after building it and after its blocks, on a speaking and
/// then a silent reference, the thread's control bits are as they were and a denormal survives.
#[test]
fn aec3_leaves_the_thread_s_mxcsr_as_it_found_it() {
    let before = mxcsr();
    let far = speech_like(100 * BLOCK, Voice::LOW, 7).samples;
    let mic = scaled(&delayed(&far, 1920), 0.4);
    let mut s = aec3();
    let built = mxcsr();
    run_stage(&mut s, &mic, Some(&far));
    run_stage(&mut s, &mic, Some(&vec![0.0; mic.len()]));
    let after = mxcsr();
    assert_eq!((built & CONTROL, after & CONTROL), (before & CONTROL, before & CONTROL), "MXCSR {before:#06x}, {built:#06x}, {after:#06x}");
    let tiny = std::hint::black_box(1e-38f32);
    assert!(std::hint::black_box(tiny * 0.01) > 0.0, "a denormal product flushed to 0");
}
