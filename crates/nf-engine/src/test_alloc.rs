//! Test builds only: a global allocator that counts the calling thread's allocations and frees
//! while counting is on, so tests can assert the audio path's real-time rule (CLAUDE.md).
//!
//! The thread-locals are `const`-initialised `Cell`s, which compile to plain `#[thread_local]`
//! statics with no lazy set-up, so the allocator never allocates itself.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static FREES: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

#[inline]
fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    let on = COUNTING.try_with(Cell::get).unwrap_or(false);
    if on {
        let _ = counter.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: every call forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees carry over; the counting touches only const thread-locals and can't unwind.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(&ALLOCS);
        // SAFETY: forwarded as is (the caller upholds GlobalAlloc::alloc's contract).
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(&FREES);
        // SAFETY: `ptr` came from `System` through this allocator, with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump(&ALLOCS);
        // SAFETY: forwarded as is.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(&ALLOCS);
        // SAFETY: forwarded as is; `ptr` came from `System` with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Runs `f` and returns its result with the (allocations, frees) this thread made meanwhile.
pub(crate) fn count<R>(f: impl FnOnce() -> R) -> (R, u64, u64) {
    let a0 = ALLOCS.with(Cell::get);
    let f0 = FREES.with(Cell::get);
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, ALLOCS.with(Cell::get) - a0, FREES.with(Cell::get) - f0)
}

#[test]
fn the_counter_sees_allocations() {
    let ((), allocs, frees) = count(|| {
        let v = std::hint::black_box(vec![1u8; 64]);
        drop(v);
    });
    assert_eq!((allocs, frees), (1, 1));
    let ((), allocs, _) = count(|| ());
    assert_eq!(allocs, 0);
}
