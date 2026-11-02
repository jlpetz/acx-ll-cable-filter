//! nf-ort's integration tests, one binary (ONNX Runtime is linked statically: one test binary links it once, not once per
//! file). They need the UL-UNAS pack fetched (`python -I tools/fetch_models.py ulunas`) and skip with a message without it;
//! with `NF_REQUIRE_MODELS=1` a missing pack fails them instead.
//!
//! - `pack`: loading and its refusals; the session's signature check.
//! - `latency`: the offsets to the sample (each layer's `check_model`, the stage's `check_latency` and `check_profile`), the
//!   chain in every mode, the stage's limits.
//! - `parity`: P1 (against an independent f64 implementation), P2 (the conformance pairs), P3 (bin-mapped against native).
//! - `memory`: the audio side's allocations (none, Rust or C runtime), the heap probe's self-check, and TODO 6's measurement
//!   (ignored).
//! - `timing` (ignored, release) and `quality` (ignored, the fetched test audio).
//!
//! This binary's global allocator counts allocations per thread, as nf-core's `rt_no_alloc` does.

mod common;
mod latency;
mod memory;
mod pack;
mod parity;
mod quality;
mod timing;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    // Const-initialised and without a destructor, so reading it never allocates.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

/// The calling thread's Rust allocations so far.
pub fn rust_allocs() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s guarantees hold; the only
// addition is a thread-local counter that doesn't allocate.
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
