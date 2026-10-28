//! The router behind [`crate::RtAllocator`] (design: `doc/dev/api/rlsf-rtrb.md` §2).
//!
//! New memory comes from the allocator the calling thread's scope flag selects: the TLSF pool
//! inside an [`crate::RtScope`], the system heap outside one, or when the pool is absent or full.
//! Old memory goes back to its owner, found by address, so any thread may free or resize pool
//! blocks.
//!
//! Everything here runs inside `GlobalAlloc`, so it must never allocate, block, log or unwind.
//! The pool lock is a spin lock (std's `Mutex` "may allocate"); the only code under it is TLSF
//! (no_std, never allocates) and plain atomic loads and stores.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::{Cell, UnsafeCell};
use core::mem;
use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::alloc::System;
use std::thread::LocalKey;

use rlsf::Tlsf;
use windows::Win32::System::Memory::VirtualLock;

use crate::RtCounters;
use crate::region::win32_code;

/// x64: 32-byte granularity. FLLEN 28 allows 8 GiB chunks; SLLEN 32 keeps good-fit waste near 1/32.
type Pool = Tlsf<'static, u32, u32, 28, 32>;

/// The router [`crate::RtAllocator`] delegates to.
pub(crate) static ROUTER: Router = Router::new();

thread_local! {
    // All three are const-initialised and need no drop, so on windows-msvc each is a bare
    // `#[thread_local]` static: no lazy init, no destructor registration, no allocation
    // (rlsf-rtrb.md §2.1). Read them with `try_with`, which has no panic path.
    /// Whether this thread is inside an `RtScope`.
    static IN_SCOPE: Cell<bool> = const { Cell::new(false) };
    /// Allocations this thread made inside a scope (pool or fallback).
    static SCOPE_ALLOCS: Cell<u64> = const { Cell::new(0) };
    /// Of those, the ones served by the system heap.
    static SCOPE_FALLBACKS: Cell<u64> = const { Cell::new(0) };
}

#[inline(always)]
pub(crate) fn in_scope() -> bool {
    IN_SCOPE.try_with(Cell::get).unwrap_or(false)
}

/// Sets the calling thread's scope flag and returns the old value.
#[inline]
pub(crate) fn replace_scope_flag(on: bool) -> bool {
    IN_SCOPE.try_with(|c| c.replace(on)).unwrap_or(false)
}

#[inline]
pub(crate) fn set_scope_flag(on: bool) {
    let _ = IN_SCOPE.try_with(|c| c.set(on));
}

#[inline]
pub(crate) fn scope_allocs() -> u64 {
    SCOPE_ALLOCS.try_with(Cell::get).unwrap_or(0)
}

#[inline]
pub(crate) fn scope_fallbacks() -> u64 {
    SCOPE_FALLBACKS.try_with(Cell::get).unwrap_or(0)
}

#[inline(always)]
fn bump(key: &'static LocalKey<Cell<u64>>) {
    let _ = key.try_with(|c| c.set(c.get().wrapping_add(1)));
}

/// The address of this thread's flag: unique among live threads, and free to compute. 0 = unknown.
#[inline(always)]
fn thread_token() -> usize {
    IN_SCOPE.try_with(|c| ptr::from_ref(c).addr()).unwrap_or(0)
}

/// Unwinding out of a `GlobalAlloc` method is UB, so a panic under the router becomes a fast-fail
/// (`__fastfail`, no allocation). Defused with `mem::forget` on the normal path.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    #[inline]
    fn drop(&mut self) {
        std::process::abort()
    }
}

/// A test-and-test-and-set lock that knows its holder. A panic under the lock allocates, which
/// re-enters the router on the same thread; spinning there would hang the audio thread forever,
/// so that case aborts instead (a crash dump beats a silent hang).
struct SpinLock {
    locked: AtomicBool,
    /// `thread_token()` of the holder, 0 when free. Only compared against the caller's own token.
    owner: AtomicUsize,
}

struct SpinGuard<'a>(&'a SpinLock);

impl SpinLock {
    const fn new() -> Self {
        Self { locked: AtomicBool::new(false), owner: AtomicUsize::new(0) }
    }

    #[inline]
    fn lock(&self) -> SpinGuard<'_> {
        let me = thread_token();
        if self.locked.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
            self.lock_contended(me);
        }
        self.owner.store(me, Relaxed);
        SpinGuard(self)
    }

    #[cold]
    #[inline(never)]
    fn lock_contended(&self, me: usize) {
        let mut spins: u32 = 0;
        loop {
            while self.locked.load(Relaxed) {
                // Only this thread ever stores `me`, and it clears it before unlocking, so seeing
                // it means we already hold the lock: re-entered from our own critical section.
                if me != 0 && self.owner.load(Relaxed) == me {
                    std::process::abort();
                }
                spins = spins.wrapping_add(1);
                if spins % 128 == 0 {
                    // SwitchToThread: lets a preempted holder on this core run. No allocation.
                    std::thread::yield_now();
                } else {
                    core::hint::spin_loop();
                }
            }
            if self.locked.compare_exchange_weak(false, true, Acquire, Relaxed).is_ok() {
                return;
            }
        }
    }
}

impl Drop for SpinGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.owner.store(0, Relaxed);
        self.0.locked.store(false, Release);
    }
}

/// Global counters. The pool ones are written only under the lock, so plain loads and stores
/// suffice (no locked instructions); readers see Relaxed snapshots.
#[repr(align(128))] // off the lock's cache line
struct Counters {
    pool_allocs: AtomicU64,
    pool_frees: AtomicU64,
    /// Written outside the lock with `fetch_add`.
    fallback_allocs: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
}

/// A TLSF pool over one region, a lock, and the routing rules. `RtAllocator` uses the static
/// [`ROUTER`]; tests build their own over a small 4 KB-page region.
pub(crate) struct Router {
    lock: SpinLock,
    start: AtomicUsize,
    /// 0 until `install`; never changes afterwards.
    len: AtomicUsize,
    pool: UnsafeCell<Pool>,
    c: Counters,
}

// SAFETY: `pool` is only touched with `lock` held (or through `&mut self`); every other field is
// atomic. Tlsf itself is Send + Sync.
unsafe impl Sync for Router {}

impl Router {
    pub(crate) const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            start: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            pool: UnsafeCell::new(Pool::new()),
            c: Counters {
                pool_allocs: AtomicU64::new(0),
                pool_frees: AtomicU64::new(0),
                fallback_allocs: AtomicU64::new(0),
                live: AtomicU64::new(0),
                peak: AtomicU64::new(0),
            },
        }
    }

    /// Hands the pool its memory. Returns the bytes TLSF manages (counted from the region's
    /// start), or `None` if a pool is already installed or the region is under 64 bytes.
    ///
    /// # Safety
    /// `region` is committed read-write memory used for nothing else, and stays mapped for as
    /// long as this router can be called (for [`ROUTER`], until the process exits).
    pub(crate) unsafe fn install(&self, region: NonNull<[u8]>) -> Option<usize> {
        let _g = self.lock.lock();
        if self.len.load(Relaxed) != 0 {
            return None;
        }
        // SAFETY: the lock is held, so this is the only access to the pool; the region contract
        // is the caller's. insert_free_block_ptr never panics.
        let n = unsafe { (*self.pool.get()).insert_free_block_ptr(region) }?.get();
        self.start.store(region.cast::<u8>().as_ptr().addr(), Relaxed);
        self.len.store(n, Release); // after `start`: `owns` loads `len` first
        Some(n)
    }

    /// Pins the router's own pages: the TLSF control block (~7 KB of bitmaps and free lists) and
    /// the counters, which every pool operation touches under the lock. For [`ROUTER`] they sit
    /// in the exe's data section, pageable unless locked. Returns the Win32 code on failure.
    pub(crate) fn lock_control_block(&'static self) -> Result<(), u32> {
        // SAFETY: `self` is a live static, so the whole range is committed read-write memory for
        // the rest of the process; VirtualLock only pins it.
        unsafe { VirtualLock(ptr::from_ref(self).cast(), size_of::<Self>()) }.map_err(|e| win32_code(&e))
    }

    pub(crate) fn counters(&self) -> RtCounters {
        RtCounters {
            pool_allocs: self.c.pool_allocs.load(Relaxed),
            pool_frees: self.c.pool_frees.load(Relaxed),
            fallback_allocs: self.c.fallback_allocs.load(Relaxed),
            pool_bytes_in_use: self.c.live.load(Relaxed),
            pool_peak_bytes: self.c.peak.load(Relaxed),
        }
    }

    /// Whether `p` lies in the pool's region. Branch-free; false while no pool is installed.
    #[inline(always)]
    pub(crate) fn owns(&self, p: *const u8) -> bool {
        let len = self.len.load(Acquire); // pairs with install's Release; `start` is then current
        p.addr().wrapping_sub(self.start.load(Relaxed)) < len
    }

    // The note_* helpers run with the lock held: single writer, so load + store is enough.
    #[inline(always)]
    fn note_alloc(&self, size: usize) {
        let c = &self.c;
        c.pool_allocs.store(c.pool_allocs.load(Relaxed).wrapping_add(1), Relaxed);
        let live = c.live.load(Relaxed).wrapping_add(size as u64);
        c.live.store(live, Relaxed);
        c.peak.store(c.peak.load(Relaxed).max(live), Relaxed);
    }

    #[inline(always)]
    fn note_free(&self, size: usize) {
        let c = &self.c;
        c.pool_frees.store(c.pool_frees.load(Relaxed).wrapping_add(1), Relaxed);
        c.live.store(c.live.load(Relaxed).wrapping_sub(size as u64), Relaxed);
    }

    /// A scoped allocation the pool couldn't serve.
    #[inline]
    fn note_fallback(&self) {
        self.c.fallback_allocs.fetch_add(1, Relaxed);
        bump(&SCOPE_FALLBACKS);
    }

    /// Null if no pool is installed or nothing fits.
    #[inline]
    fn pool_alloc(&self, layout: Layout) -> *mut u8 {
        if self.len.load(Relaxed) == 0 {
            return ptr::null_mut();
        }
        let bomb = AbortOnUnwind;
        let p = {
            let _g = self.lock.lock();
            // SAFETY: the lock is held, so this is the only reference to the pool.
            let pool = unsafe { &mut *self.pool.get() };
            match pool.allocate(layout) {
                Some(p) => {
                    self.note_alloc(layout.size());
                    p.as_ptr()
                }
                None => ptr::null_mut(),
            }
        };
        mem::forget(bomb);
        p
    }

    /// # Safety
    /// `p` is a live pool block allocated with `layout`'s alignment and size.
    #[inline]
    unsafe fn pool_free(&self, p: *mut u8, layout: Layout) {
        let bomb = AbortOnUnwind;
        {
            let _g = self.lock.lock();
            // SAFETY: the lock is held, so this is the only reference to the pool.
            let pool = unsafe { &mut *self.pool.get() };
            // SAFETY: caller contract. TLSF finds the block header from the alignment, so it must
            // be the original one, which GlobalAlloc's contract guarantees.
            unsafe { pool.deallocate(NonNull::new_unchecked(p), layout.align()) };
            self.note_free(layout.size());
        }
        mem::forget(bomb);
    }

    /// Resizes a pool block inside the pool. `None` leaves `p` untouched (rlsf returns before any
    /// change on every failure path).
    ///
    /// # Safety
    /// `p` is a live pool block allocated with `layout`; `new_layout` has the same alignment.
    #[inline]
    unsafe fn pool_realloc(&self, p: *mut u8, layout: Layout, new_layout: Layout) -> *mut u8 {
        let bomb = AbortOnUnwind;
        let q = {
            let _g = self.lock.lock();
            // SAFETY: the lock is held, so this is the only reference to the pool.
            let pool = unsafe { &mut *self.pool.get() };
            // SAFETY: caller contract: a live pool block, and the original alignment.
            match unsafe { pool.reallocate(NonNull::new_unchecked(p), new_layout) } {
                Some(q) => {
                    // Counted as a free plus an allocation, so allocs - frees = live blocks.
                    self.note_free(layout.size());
                    self.note_alloc(new_layout.size());
                    q.as_ptr()
                }
                None => ptr::null_mut(),
            }
        };
        mem::forget(bomb);
        q
    }
}

// SAFETY: every method returns memory that fits the layout from either System or the pool (TLSF
// honours any power-of-two alignment), frees go to the allocator that owns the address with the
// original layout, and nothing here allocates through the global allocator or unwinds (panics
// under the lock abort).
unsafe impl GlobalAlloc for Router {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !in_scope() {
            // SAFETY: the caller's layout contract, passed through.
            return unsafe { System.alloc(layout) };
        }
        bump(&SCOPE_ALLOCS);
        let p = self.pool_alloc(layout);
        if !p.is_null() {
            return p;
        }
        self.note_fallback();
        // SAFETY: the caller's layout contract, passed through.
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !in_scope() {
            // SAFETY: the caller's layout contract, passed through.
            return unsafe { System.alloc_zeroed(layout) };
        }
        bump(&SCOPE_ALLOCS);
        let p = self.pool_alloc(layout);
        if !p.is_null() {
            // TLSF recycles blocks, so zero here (outside the lock).
            // SAFETY: `p` is a fresh pool block of at least `layout.size()` bytes.
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
            return p;
        }
        self.note_fallback();
        // SAFETY: the caller's layout contract, passed through.
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if self.owns(p) {
            // SAFETY: `p` is in the pool's region, so it's a live pool block from `layout`.
            unsafe { self.pool_free(p, layout) }
        } else {
            // SAFETY: not ours, so it came from System with this layout.
            unsafe { System.dealloc(p, layout) }
        }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let scoped = in_scope();
        if scoped {
            bump(&SCOPE_ALLOCS);
        }
        // SAFETY: GlobalAlloc::realloc guarantees new_size > 0 and that rounding it up to
        // `layout.align()` doesn't overflow isize, so this layout is valid.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let keep = layout.size().min(new_size);

        if self.owns(p) {
            if scoped {
                // SAFETY: `p` is a live pool block from `layout`; same alignment.
                let q = unsafe { self.pool_realloc(p, layout, new_layout) };
                if !q.is_null() {
                    return q;
                }
                self.note_fallback();
            }
            // Out of the pool: outside a scope, or the pool is full.
            // SAFETY: `new_layout` is valid (above).
            let q = unsafe { System.alloc(new_layout) };
            if q.is_null() {
                return q; // `p` stays valid, as GlobalAlloc requires
            }
            // SAFETY: both blocks hold at least `keep` bytes and are distinct allocations; `p` is
            // a live pool block from `layout`.
            unsafe {
                ptr::copy_nonoverlapping(p, q, keep);
                self.pool_free(p, layout);
            }
            q
        } else {
            if scoped {
                // A system block resized inside a scope moves into the pool.
                let q = self.pool_alloc(new_layout);
                if !q.is_null() {
                    // SAFETY: both blocks hold at least `keep` bytes and are distinct; `p` came
                    // from System with `layout`.
                    unsafe {
                        ptr::copy_nonoverlapping(p, q, keep);
                        System.dealloc(p, layout);
                    }
                    return q;
                }
                self.note_fallback();
            }
            // SAFETY: `p` came from System with `layout`; the caller's new_size contract.
            unsafe { System.realloc(p, layout, new_size) }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::RtScope;
    use crate::region::Region;

    /// A router over its own 4 KB-page region. Field order matters: the router goes first.
    struct TestPool {
        router: Box<Router>,
        _region: Region,
        len: usize,
    }

    fn test_pool(bytes: usize) -> TestPool {
        let region = Region::small(bytes).expect("VirtualAlloc for the test pool");
        let router = Box::new(Router::new());
        // SAFETY: the region outlives the router (field order) and nothing else uses it.
        let len = unsafe { router.install(region.as_slice_ptr()) }.expect("install");
        TestPool { router, _region: region, len }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    fn alloc(r: &Router, l: Layout) -> *mut u8 {
        // SAFETY: tests only use non-zero sizes.
        let p = unsafe { r.alloc(l) };
        assert!(!p.is_null());
        assert_eq!(p.addr() % l.align(), 0, "misaligned {l:?}");
        p
    }

    fn free(r: &Router, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `r` with `l`.
        unsafe { r.dealloc(p, l) }
    }

    fn fill(p: *mut u8, n: usize, seed: u8) {
        for i in 0..n {
            // SAFETY: `p` holds at least `n` bytes.
            unsafe { p.add(i).write(seed.wrapping_add(i as u8)) };
        }
    }

    fn check(p: *const u8, n: usize, seed: u8) {
        for i in 0..n {
            // SAFETY: `p` holds at least `n` bytes.
            let b = unsafe { p.add(i).read() };
            assert_eq!(b, seed.wrapping_add(i as u8), "byte {i} of {n}");
        }
    }

    #[test]
    fn install_reports_length_and_refuses_a_second_pool() {
        let t = test_pool(64 * 1024);
        assert_eq!(t.len, 64 * 1024);
        let other = Region::small(4096).unwrap();
        // SAFETY: refused before touching the region.
        assert_eq!(unsafe { t.router.install(other.as_slice_ptr()) }, None);
        assert!(!t.router.owns(other.as_slice_ptr().cast::<u8>().as_ptr()));
    }

    #[test]
    fn outside_a_scope_everything_goes_to_the_system_heap() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let before = (scope_allocs(), scope_fallbacks());
        let l = layout(100, 8);
        let p = alloc(r, l);
        assert!(!r.owns(p));
        fill(p, 100, 1);
        check(p, 100, 1);
        free(r, p, l);
        assert_eq!(r.counters(), RtCounters::default());
        assert_eq!((scope_allocs(), scope_fallbacks()), before);
    }

    #[test]
    fn inside_a_scope_allocations_come_from_the_pool() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let (a0, f0) = (scope_allocs(), scope_fallbacks());
        let l = layout(1000, 16);
        let p = {
            let _s = RtScope::enter();
            alloc(r, l)
        };
        assert!(r.owns(p));
        assert_eq!(scope_allocs(), a0 + 1);
        assert_eq!(scope_fallbacks(), f0);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.fallback_allocs), (1, 0, 0));
        assert_eq!((c.pool_bytes_in_use, c.pool_peak_bytes), (1000, 1000));
        fill(p, 1000, 7);
        check(p, 1000, 7);
        // Freed outside the scope: still routed to the pool by address.
        free(r, p, l);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees), (1, 1));
        assert_eq!((c.pool_bytes_in_use, c.pool_peak_bytes), (0, 1000));
    }

    #[test]
    fn alloc_zeroed_zeroes_recycled_pool_memory() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let _s = RtScope::enter();
        let l = layout(512, 64);
        for _ in 0..3 {
            let p = alloc(r, l);
            fill(p, 512, 0xA5);
            free(r, p, l);
            // SAFETY: non-zero size.
            let z = unsafe { r.alloc_zeroed(l) };
            assert!(r.owns(z));
            for i in 0..512 {
                // SAFETY: `z` holds 512 bytes.
                assert_eq!(unsafe { z.add(i).read() }, 0);
            }
            free(r, z, l);
        }
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use), (6, 6, 0));
    }

    #[test]
    fn scopes_nest_and_restore() {
        assert!(!in_scope());
        {
            let _outer = RtScope::enter();
            assert!(in_scope());
            {
                let _inner = RtScope::enter();
                assert!(in_scope());
            }
            assert!(in_scope(), "dropping the inner scope must keep the outer one");
        }
        assert!(!in_scope());
        // Out-of-order drops restore what each saw on entry.
        let a = RtScope::enter();
        let b = RtScope::enter();
        drop(a);
        assert!(!in_scope());
        drop(b);
        assert!(in_scope());
        set_scope_flag(false);
    }

    #[test]
    fn scope_is_per_thread() {
        let _s = RtScope::enter();
        assert!(!std::thread::spawn(in_scope).join().unwrap());
        assert!(in_scope());
    }

    #[test]
    fn pool_memory_can_be_freed_on_another_thread() {
        let t = test_pool(256 * 1024);
        let r = &*t.router;
        let l = layout(4096, 4096);
        let p = {
            let _s = RtScope::enter();
            alloc(r, l)
        };
        assert!(r.owns(p));
        fill(p, 4096, 3);
        // Raw pointers aren't Send, so wrap it to hand it over.
        struct SendPtr(*mut u8);
        // SAFETY: the block is plain bytes owned by this test; only one thread uses it at a time.
        unsafe impl Send for SendPtr {}
        let sp = SendPtr(p);
        std::thread::scope(|s| {
            s.spawn(move || {
                let sp = sp;
                assert!(!in_scope());
                check(sp.0, 4096, 3);
                free(r, sp.0, l);
            });
        });
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use), (1, 1, 0));

        // And the other way: a worker allocates in its own scope, this thread frees.
        let q = std::thread::scope(|s| {
            s.spawn(|| {
                let _s = RtScope::enter();
                let q = alloc(r, l);
                fill(q, 4096, 9);
                SendPtr(q)
            })
            .join()
            .unwrap()
        });
        assert!(r.owns(q.0));
        check(q.0, 4096, 9);
        free(r, q.0, l);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use), (2, 2, 0));
    }

    #[test]
    fn realloc_inside_the_pool_keeps_the_data() {
        let t = test_pool(256 * 1024);
        let r = &*t.router;
        let _s = RtScope::enter();
        let a0 = scope_allocs();
        let l = layout(64, 8);
        let p = alloc(r, l);
        fill(p, 64, 11);
        // SAFETY: `p` came from `r` with `l`; non-zero new size.
        let q = unsafe { r.realloc(p, l, 8192) };
        assert!(r.owns(q));
        check(q, 64, 11);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use), (2, 1, 8192));
        let l2 = layout(8192, 8);
        fill(q, 8192, 12);
        // SAFETY: as above; shrinking.
        let s = unsafe { r.realloc(q, l2, 100) };
        assert!(r.owns(s));
        check(s, 100, 12);
        free(r, s, layout(100, 8));
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use), (3, 3, 0));
        assert_eq!(c.pool_peak_bytes, 8192);
        assert_eq!(scope_allocs(), a0 + 3);
    }

    #[test]
    fn realloc_outside_a_scope_moves_a_pool_block_to_the_system_heap() {
        let t = test_pool(256 * 1024);
        let r = &*t.router;
        let l = layout(300, 32);
        let p = {
            let _s = RtScope::enter();
            alloc(r, l)
        };
        fill(p, 300, 21);
        let (a0, f0) = (scope_allocs(), scope_fallbacks());
        // SAFETY: `p` came from `r` with `l`.
        let q = unsafe { r.realloc(p, l, 5000) };
        assert!(!r.owns(q));
        assert_eq!(q.addr() % 32, 0);
        check(q, 300, 21);
        assert_eq!((scope_allocs(), scope_fallbacks()), (a0, f0), "not a scoped allocation");
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use, c.fallback_allocs), (1, 1, 0, 0));
        free(r, q, layout(5000, 32));
    }

    #[test]
    fn realloc_inside_a_scope_moves_a_system_block_into_the_pool() {
        let t = test_pool(256 * 1024);
        let r = &*t.router;
        let l = layout(200, 64);
        let p = alloc(r, l);
        assert!(!r.owns(p));
        fill(p, 200, 33);
        let _s = RtScope::enter();
        // SAFETY: `p` came from `r` with `l`.
        let q = unsafe { r.realloc(p, l, 1000) };
        assert!(r.owns(q));
        assert_eq!(q.addr() % 64, 0);
        check(q, 200, 33);
        free(r, q, layout(1000, 64));
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use, c.fallback_allocs), (1, 1, 0, 0));
    }

    #[test]
    fn realloc_that_outgrows_the_pool_falls_back() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let _s = RtScope::enter();
        let f0 = scope_fallbacks();
        let l = layout(1024, 16);
        let p = alloc(r, l);
        assert!(r.owns(p));
        fill(p, 1024, 44);
        // SAFETY: `p` came from `r` with `l`.
        let q = unsafe { r.realloc(p, l, 1 << 20) };
        assert!(!r.owns(q));
        check(q, 1024, 44);
        assert_eq!(scope_fallbacks(), f0 + 1);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.pool_frees, c.pool_bytes_in_use, c.fallback_allocs), (1, 1, 0, 1));
        // A system block that can't fit stays in the system heap.
        // SAFETY: `q` came from `r` with this layout.
        let s = unsafe { r.realloc(q, layout(1 << 20, 16), 2 << 20) };
        assert!(!r.owns(s));
        check(s, 1024, 44);
        assert_eq!(scope_fallbacks(), f0 + 2);
        free(r, s, layout(2 << 20, 16));
        assert_eq!(r.counters().fallback_allocs, 2);
    }

    #[test]
    fn a_full_pool_falls_back_and_recovers() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let _s = RtScope::enter();
        let (a0, f0) = (scope_allocs(), scope_fallbacks());
        let l = layout(4000, 16);
        let mut pool_blocks = Vec::new();
        let spill = loop {
            let p = alloc(r, l);
            if !r.owns(p) {
                break p;
            }
            pool_blocks.push(p);
            assert!(pool_blocks.len() <= 16, "a 64 KiB pool can't hold 17 blocks of 4000 B");
        };
        let n = pool_blocks.len() as u64;
        assert!(n >= 14, "only {n} blocks fit");
        assert_eq!(scope_allocs(), a0 + n + 1);
        assert_eq!(scope_fallbacks(), f0 + 1);
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.fallback_allocs, c.pool_bytes_in_use), (n, 1, n * 4000));
        free(r, spill, l);
        for p in pool_blocks.drain(..) {
            free(r, p, l);
        }
        let c = r.counters();
        assert_eq!((c.pool_frees, c.pool_bytes_in_use, c.pool_peak_bytes), (n, 0, n * 4000));
        // The space is reusable.
        let p = alloc(r, l);
        assert!(r.owns(p));
        free(r, p, l);
    }

    #[test]
    fn without_a_pool_every_scoped_allocation_is_a_fallback() {
        let r = Box::new(Router::new());
        let _s = RtScope::enter();
        let (a0, f0) = (scope_allocs(), scope_fallbacks());
        let l = layout(64, 8);
        let p = alloc(&r, l);
        // SAFETY: non-zero size.
        let z = unsafe { r.alloc_zeroed(l) };
        // SAFETY: `p` came from `r` with `l`.
        let q = unsafe { r.realloc(p, l, 128) };
        assert!(!r.owns(z) && !r.owns(q));
        assert_eq!((scope_allocs(), scope_fallbacks()), (a0 + 3, f0 + 3));
        let c = r.counters();
        assert_eq!((c.pool_allocs, c.fallback_allocs), (0, 3));
        free(&r, z, l);
        free(&r, q, layout(128, 8));
    }

    #[test]
    fn huge_alignment_falls_back_instead_of_failing() {
        let t = test_pool(64 * 1024);
        let r = &*t.router;
        let _s = RtScope::enter();
        let l = layout(16, 1 << 20);
        let p = alloc(r, l);
        assert!(!r.owns(p));
        free(r, p, l);
        assert_eq!(r.counters().fallback_allocs, 1);
    }

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    struct Live {
        p: *mut u8,
        l: Layout,
        seed: u8,
    }

    fn random_layout(rng: &mut Rng) -> Layout {
        let size = match rng.below(10) {
            0 => 1 + rng.below(64 * 1024),
            1..=3 => 1 + rng.below(4096),
            _ => 1 + rng.below(256),
        } as usize;
        layout(size, 1 << rng.below(13)) // 1..=4096
    }

    #[test]
    fn random_sizes_and_alignments_match_a_reference_model() {
        let t = test_pool(1 << 20);
        let r = &*t.router;
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut live: Vec<Live> = Vec::new();
        let mut scope: Option<RtScope> = None;
        let (mut allocs, mut frees, mut bytes, mut peak, mut fallbacks) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let (a0, f0) = (scope_allocs(), scope_fallbacks());
        let mut scoped_calls = 0u64;
        let mut seed = 0u8;

        for _ in 0..20_000 {
            if rng.below(64) == 0 {
                scope = if scope.is_some() { None } else { Some(RtScope::enter()) };
            }
            let scoped = scope.is_some();
            assert_eq!(in_scope(), scoped);
            let op = rng.below(10);
            if live.is_empty() || op < 4 {
                let l = random_layout(&mut rng);
                let zeroed = rng.below(4) == 0;
                // SAFETY: non-zero size.
                let p = unsafe { if zeroed { r.alloc_zeroed(l) } else { r.alloc(l) } };
                assert!(!p.is_null());
                assert_eq!(p.addr() % l.align(), 0);
                if zeroed {
                    check_zero(p, l.size());
                }
                if scoped {
                    scoped_calls += 1;
                }
                if r.owns(p) {
                    assert!(scoped, "pool memory handed out outside a scope");
                    allocs += 1;
                    bytes += l.size() as u64;
                } else if scoped {
                    fallbacks += 1;
                }
                seed = seed.wrapping_add(1);
                fill(p, l.size(), seed);
                live.push(Live { p, l, seed });
            } else if op < 7 {
                let i = rng.below(live.len() as u64) as usize;
                let b = live.swap_remove(i);
                check(b.p, b.l.size(), b.seed);
                if r.owns(b.p) {
                    frees += 1;
                    bytes -= b.l.size() as u64;
                }
                free(r, b.p, b.l);
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let b = &mut live[i];
                let new_size = random_layout(&mut rng).size();
                let was_pool = r.owns(b.p);
                // SAFETY: `b.p` came from `r` with `b.l`; non-zero new size.
                let q = unsafe { r.realloc(b.p, b.l, new_size) };
                assert!(!q.is_null());
                assert_eq!(q.addr() % b.l.align(), 0);
                check(q, b.l.size().min(new_size), b.seed);
                let now_pool = r.owns(q);
                if scoped {
                    scoped_calls += 1;
                    if !now_pool {
                        fallbacks += 1;
                    }
                } else {
                    assert!(!now_pool, "a realloc outside a scope must leave the pool");
                }
                if was_pool {
                    frees += 1;
                    bytes -= b.l.size() as u64;
                }
                if now_pool {
                    allocs += 1;
                    bytes += new_size as u64;
                }
                b.p = q;
                b.l = layout(new_size, b.l.align());
                fill(q, new_size, b.seed);
            }
            peak = peak.max(bytes);
            let c = r.counters();
            assert_eq!(
                (c.pool_allocs, c.pool_frees, c.pool_bytes_in_use, c.pool_peak_bytes, c.fallback_allocs),
                (allocs, frees, bytes, peak, fallbacks)
            );
        }
        assert_eq!(scope_allocs() - a0, scoped_calls);
        assert_eq!(scope_fallbacks() - f0, fallbacks);
        assert!(allocs > 1000 && fallbacks > 10, "allocs {allocs}, fallbacks {fallbacks}");

        // Live pool blocks never overlap.
        let mut spans: Vec<(usize, usize)> = live.iter().filter(|b| r.owns(b.p)).map(|b| (b.p.addr(), b.p.addr() + b.l.size())).collect();
        spans.sort_unstable();
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlap: {:x?}", w);
        }
        drop(scope);
        for b in live.drain(..) {
            check(b.p, b.l.size(), b.seed);
            free(r, b.p, b.l);
        }
        let c = r.counters();
        assert_eq!(c.pool_allocs, c.pool_frees);
        assert_eq!(c.pool_bytes_in_use, 0);
    }

    fn check_zero(p: *const u8, n: usize) {
        for i in 0..n {
            // SAFETY: `p` holds at least `n` bytes.
            assert_eq!(unsafe { p.add(i).read() }, 0);
        }
    }

    #[test]
    fn threads_contending_for_the_pool_keep_it_consistent() {
        let t = test_pool(4 << 20);
        let r = &*t.router;
        struct SendPtr(*mut u8, Layout);
        // SAFETY: plain bytes; ownership moves with the value.
        unsafe impl Send for SendPtr {}
        let orphans: Mutex<Vec<SendPtr>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for k in 0..4u64 {
                let orphans = &orphans;
                s.spawn(move || {
                    let _scope = RtScope::enter();
                    let mut rng = Rng(0x1234_5678 + k);
                    let mut mine: Vec<(*mut u8, Layout, u8)> = Vec::with_capacity(32);
                    for i in 0..20_000u32 {
                        if mine.len() < 32 && (mine.is_empty() || rng.below(2) == 0) {
                            let l = layout(1 + rng.below(2048) as usize, 1 << rng.below(7));
                            let p = alloc(r, l);
                            let seed = i as u8;
                            fill(p, l.size().min(64), seed);
                            mine.push((p, l, seed));
                        } else {
                            let j = rng.below(mine.len() as u64) as usize;
                            let (p, l, seed) = mine.swap_remove(j);
                            check(p, l.size().min(64), seed);
                            if rng.below(16) == 0 {
                                orphans.lock().unwrap().push(SendPtr(p, l));
                            } else {
                                free(r, p, l);
                            }
                        }
                    }
                    for (p, l, _) in mine {
                        free(r, p, l);
                    }
                });
            }
        });
        // Blocks handed over by the workers, freed here.
        for SendPtr(p, l) in orphans.into_inner().unwrap() {
            free(r, p, l);
        }
        let c = r.counters();
        assert_eq!(c.pool_allocs, c.pool_frees);
        assert_eq!(c.pool_bytes_in_use, 0);
        assert!(c.pool_allocs > 10_000);
    }

    #[test]
    fn lock_is_reusable_and_clears_its_owner() {
        let l = SpinLock::new();
        for _ in 0..3 {
            let g = l.lock();
            assert_eq!(l.owner.load(Relaxed), thread_token());
            assert!(l.locked.load(Relaxed));
            drop(g);
            assert_eq!(l.owner.load(Relaxed), 0);
            assert!(!l.locked.load(Relaxed));
        }
        assert_ne!(thread_token(), 0);
        let other = std::thread::spawn(thread_token).join().unwrap();
        assert_ne!(other, thread_token());
    }
}
