# rlsf 0.2.3 and rtrb 0.4.1: API reference for noise-filter

Written 2026-10-26 from the crate sources. Nothing was compiled or run while writing it, so the
code here has not been through `cargo check`. Path prefixes used below:

- `rlsf/` = `C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\rlsf-0.2.3\`
- `rtrb/` = `...\index.crates.io-1949cf8c6b5b557f\rtrb-0.4.1\`
- `std/`, `core/`, `alloc/` = `C:\Users\Administrator\.rustup\toolchains\nightly-x86_64-pc-windows-msvc\lib\rustlib\src\rust\library\{std,core,alloc}\`
  (rustc 1.101.0-nightly 1a2c4797e 2026-10-21)

Workspace pins (`Cargo.toml`): `rlsf = "=0.2.3"`, `rtrb = "=0.4.1"` (rtrb with default features,
so `std` is on). `doc/todo/8-audio-io.md:35` and `doc/research-2026-10.md:100` still say rtrb
0.4.0; the pin is 0.4.1, and everything below is from 0.4.1.

All sizes assume x86_64 (`usize` = 8 bytes).

---

## 1. rlsf 0.2.3

### 1.1 What the crate gives us on Windows

- `#![no_std]` (`rlsf/src/lib.rs:2`). `std` is only linked for `cfg(test)` or feature `std`
  (`lib.rs:49-50`), and nothing outside the tests uses that feature. There is no `extern crate alloc`.
  **The `Tlsf` core never allocates.** Its state is inline arrays, and block headers are written
  into the pool memory you hand it.
- Public items: `Tlsf`, `GRANULARITY` (`lib.rs:15-18`), the `FlexTlsf`/`FlexSource`/
  `GlobalAllocAsFlexSource` family (`src/flex.rs`), and `int::BinInteger` (`src/int.rs:6`).
  `BlockInfo` is only there with feature `unstable` (`lib.rs:20`).
- **`GlobalTlsf`, `SmallGlobalTlsf` and `CAlloc` don't exist on Windows.** They're behind
  `cfg(any(all(wasm32, not(atomics)), unix, doc))` (`lib.rs:25-47`). docs.rs builds with `doc`, so
  it shows them anyway. That's a doc/target mismatch: on Windows we wrap `Tlsf` ourselves (section 2).
- Dependencies: `cfg-if`, `const-default` (no default features), `rustversion`; `libc` on unix
  only (`Cargo.toml.orig:18-24`).

### 1.2 The type and its const generics

```rust
// rlsf/src/tlsf.rs:71
pub struct Tlsf<'pool, FLBitmap, SLBitmap, const FLLEN: usize, const SLLEN: usize> {
    fl_bitmap: FLBitmap,
    sl_bitmap: [SLBitmap; FLLEN],
    first_free: [[Option<NonNull<FreeBlockHdr>>; SLLEN]; FLLEN],
    _phantom: PhantomData<&'pool ()>,
}
pub const GRANULARITY: usize = core::mem::size_of::<usize>() * 4;   // tlsf.rs:96 → 32 on x64
```

- `FLBitmap`, `SLBitmap: BinInteger`. This is a sealed trait, implemented for `u8..u128`,
  `usize`, `i8..i128` and `isize` (`int.rs:154-166`).
- Compile-time checks, evaluated in `new()` (`tlsf.rs:233-246`, `261-265`): `FLLEN != 0`,
  `SLLEN != 0`, `FLBitmap::BITS >= FLLEN`, `SLBitmap::BITS >= SLLEN`, and `SLLEN` must be a power of two.
- `FLLEN` is the number of power-of-two size classes, starting at `GRANULARITY`. `SLLEN` is the
  number of linear sub-lists per class.
- `'pool` is the lifetime of the memory handed to `insert_free_block`. Use `'static` for a pool
  that's never freed. The `_ptr` methods ignore it (they're `unsafe`).
- Size of the struct: `first_free` is `FLLEN*SLLEN*8` bytes. For the `<u32, u32, 28, 32>` used
  below that's 7168 B, plus 116 B of bitmaps (our arithmetic). That's fine in a `static`.

**Our choice: `Tlsf<'static, u32, u32, 28, 32>`.** A pool chunk can be up to `32 << 28` = 8 GiB,
and sizes inside each power-of-two class are 1/32 apart.

### 1.3 Methods (exact signatures)

| Signature | Where | Notes |
|---|---|---|
| `pub const fn new() -> Self` | tlsf.rs:214 | Empty, no pool. Also `Default` (195) and `ConstDefault` (204). Usable in a `static`. |
| `pub unsafe fn insert_free_block_ptr(&mut self, block: NonNull<[u8]>) -> Option<NonZeroUsize>` | tlsf.rs:464 | Adds a new pool. Returns the bytes used, counted from `block`'s start, or `None` if the block is too small. Never panics. |
| `pub fn insert_free_block(&mut self, block: &'pool mut [MaybeUninit<u8>]) -> impl Send + Sync` | tlsf.rs:734 | Safe wrapper. The return type is opaque ("yet to be determined", 702), so you can't read the pool length from it. Prefer the `_ptr` form. |
| `pub unsafe fn append_free_block_ptr(&mut self, block: NonNull<[u8]>) -> usize` | tlsf.rs:613 | Extends a pool. `block` must start exactly at an existing pool's end. It only coalesces when `5 + FLLEN >= 64`. Otherwise it falls back to inserting a separate pool (618-626). |
| `pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>>` | tlsf.rs:773 | O(1). `None` when nothing fits. Handles any power-of-two alignment. |
| `pub unsafe fn deallocate(&mut self, ptr: NonNull<u8>, align: usize)` | tlsf.rs:1002 | O(1). Safety (996-1000): `ptr` came from this `Tlsf`, **and `align` equals the original `Layout::align()`**. The header lookup depends on it (925-935). Passing the wrong align corrupts the heap. |
| `pub unsafe fn reallocate(&mut self, ptr: NonNull<u8>, new_layout: Layout) -> Option<NonNull<u8>>` | tlsf.rs:1183 | O(old_size): it copies when the block moves. Same safety as `deallocate`, with `new_layout.align()` equal to the original align. Tries in place first: shrink, grow into the next free block, then slide into the previous free block (1218-1473). Falls back to allocate + copy + free. **On `None`, the old block is untouched.** Every failure path returns before any unlink. |
| `pub unsafe fn allocation_usable_size(ptr: NonNull<u8>) -> usize` | tlsf.rs:1161 | Feature `unstable` only. |
| `pub unsafe fn iter_blocks(&self, pool: NonNull<[u8]>) -> impl Iterator<Item = BlockInfo<'_>> + Send + '_` | tlsf.rs:1520 | Feature `unstable` only. For diagnostics. |

`insert_free_block_ptr` details (`tlsf.rs:464-544`):
- **The input needs no alignment.** The start is rounded up to 32 and the length rounded down to a
  multiple of 32 (467-479).
- It needs at least `2 * GRANULARITY` = 64 bytes after rounding (503). Otherwise it returns `None`.
- A region larger than `MAX_POOL_SIZE = 1 << (5 + FLLEN)` (251-258) is cut into several adjacent
  pools, and one allocation can't span two of them.
- Each pool gets one free block, plus a 32-byte sentinel (a permanently used block) at its end
  (512-532).
- It writes headers only at the start and end of each chunk. Interior pages are first touched
  when blocks are split later, so pre-fault or lock the region.
- You can insert several non-adjacent regions; each becomes its own pool.

### 1.4 Block layout, size and alignment limits

- **Header:** a used block has a 16-byte header (`UsedBlockHdr` = `GRANULARITY/2`, 167-178).
  Blocks are 32-aligned, and block sizes are multiples of 32.
- **Alignment `<= 16`:** the payload sits at block + 16. Per-request overhead is 16 B, rounded up
  to 32. A 1-16 B request uses 32 B; a 17-48 B request uses 64 B.
- **Alignment `>= 32`:** padding of up to `align - 16` B, plus a back-pointer (`UsedBlockPad`)
  just before the payload (180-193, 871-873). Worst-case overhead is `align` bytes
  (`max_overhead`, 783-784).
- **Minimum free block:** 32 B (`FreeBlockHdr`).
- **Maximum alignment:** there's no explicit cap. A huge alignment just makes the search size too
  big, and `allocate` returns `None`.
- **Maximum block size:** `(GRANULARITY << FLLEN) - GRANULARITY` (doc, 69).
- **Largest request that can succeed:** this is smaller than the maximum block size, because of
  good-fit search.
  - `allocate` rounds the request up to the next list boundary (`map_ceil`, 293-318), and only
    searches lists whose *minimum* size is at least the request (881-911).
  - The largest search size (the request plus `max_overhead`, rounded to 32) is the minimum size
    of list `(FLLEN-1, SLLEN-1)`, which is `(32 << (FLLEN-1)) * (2 - 1/SLLEN)`.
  - A free block that's slightly bigger than a request but in the same list is **not** used.
- **Worked example (our params, fresh 64 MiB pool):**
  - The free block is 64 MiB - 32. It falls in list (20, 31), whose minimum size is 63 MiB.
  - So the largest single allocation is **63 MiB - 16 B**, not 64 MiB.
  - Size pools with headroom: about 1/SLLEN of the largest block, plus fragmentation.
- **Never returns memory:** there is no shrink or release. That's fine for a process-lifetime pool.

### 1.5 Threading, Send/Sync, panics

- `unsafe impl Send` and `unsafe impl Sync` for every `Tlsf<'_, ..>`, unconditionally (82-90).
  But every operation takes `&mut self`, so **concurrent use needs our own lock** around an
  `UnsafeCell<Tlsf>`. The README says the same: "It does not support concurrent access"
  (README.md, Drawbacks).
- **Panics in release builds:** none on valid use. `insert_free_block_ptr` and
  `append_free_block_ptr` are documented "never panics" (461, 612), and `allocate` returns `None`.
  - The hot indexing uses `get_unchecked` (792, 812).
  - The remaining bounds checks (378, 387, 888, 900) and the `assert!`s in `int.rs:80,85` stay in
    range by construction.
  - Heap corruption hits `unreachable_unchecked` (UB) in release, not a panic.
- **Debug builds:** rlsf has formatting `debug_assert!(false, "could not map size {}", size)`
  and similar (374, 410, 650, 683, 794, 902). The dev profile keeps debug assertions on for
  dependencies; `[profile.dev.package."*"]` only raises `opt-level`. So in `cargo test` and dev
  builds, heap corruption becomes a panic *inside the allocator*. See 2.2.

### 1.6 Standalone pool (e.g. a TLSF `OrtAllocator`, TODO #6)

```rust
use core::{alloc::Layout, ptr::NonNull};
use rlsf::Tlsf;

type Pool = Tlsf<'static, u32, u32, 28, 32>;

/// SAFETY: `base..base+len` is committed RW memory that is never freed or reused.
unsafe fn make_pool(base: NonNull<u8>, len: usize) -> Option<(Pool, usize)> {
    let mut tlsf = Pool::new();
    let n = unsafe { tlsf.insert_free_block_ptr(NonNull::slice_from_raw_parts(base, len)) }?.get();
    Some((tlsf, n))
}

fn demo(tlsf: &mut Pool) {
    let layout = Layout::from_size_align(480 * 4, 64).unwrap();
    if let Some(p) = tlsf.allocate(layout) {
        unsafe { tlsf.deallocate(p, layout.align()) }; // same align as allocated
    }
}
```

---

## 2. Router `#[global_allocator]` for Windows (nf-mem)

Design from `doc/todo/6-inference-memory.md` ("tract meanwhile"):
- a const thread-local flag picks the pool;
- frees are routed by address range;
- rlsf runs over the pinned region;
- it falls back to System, and counts, when the pool is full.

The checks below are against this toolchain's std source.

**Names in the real code (`crates/nf-mem/src/lib.rs`, 2026-10-26).** The skeletons in 2.4 and 2.5
were written before nf-mem existed, and its API took other names. Read them with this mapping:

| Skeleton (2.4, 2.5) | nf-mem |
|---|---|
| `nf_mem::Router` as the `#[global_allocator]` | `nf_mem::RtAllocator` (`RtAllocator::new()`, a `const fn`). The router itself is private (`router::Router`), one static `ROUTER` behind it. |
| `GLOBAL.install(region)` | `nf_mem::install_pool(PagePolicy, bytes) -> PoolStatus`, once at start-up; `pool_status()` reads it back |
| `set_rt_thread(true)` for a thread's whole life | `RtScope::enter()`: a guard that sets the flag, nests and restores it on drop. It is `!Send`. The flag is `IN_SCOPE`, not `RT_FLAG`. |
| `A.stats() -> AllocStats` (`rt_allocs`, `rt_frees`, `rt_system_frees`, `pool_fallbacks`) | `nf_mem::counters() -> RtCounters` (`pool_allocs`, `pool_frees`, `fallback_allocs`, `pool_bytes_in_use`, `pool_peak_bytes`), global |
| per-thread counts (2.2, "add more thread-locals") | `thread_scope_allocs()` and `thread_scope_fallbacks()`: the calling thread's allocations inside a scope, and those that fell back to the system heap |

The audio thread enters an `RtScope` for its whole life, so its stages are built in the pool. After
warm-up it takes both per-thread counts as its baseline and publishes their growth
(`crates/nf-engine/src/audio.rs`). The routing in 2.3 holds; the counter names in it are the
skeleton's.

### 2.1 The thread-local flag: verified allocation-free

`thread_local! { static RT_FLAG: Cell<bool> = const { Cell::new(false) }; }`

1. **windows-msvc uses native TLS.** `std/src/sys/thread_local/mod.rs:36-58` picks `native` under
   `target_thread_local`. `std/src/sys/thread_local/guard/windows.rs:111-118` branches on
   `cfg!(target_thread_local)` and notes that "`#[thread_local]` is unavailable on windows-gnu".
   So msvc has it. That's an inference from the comment; the target spec isn't in rust-src.
2. **`const` + `!needs_drop` compiles to a bare `#[thread_local] static VAL: T`.** It returns
   `&VAL` (`std/src/sys/thread_local/native/mod.rs:57-81`, the `else` arm at 71-77). There's no
   state machine, no lazy init, and no destructor registration. `Cell<bool>` doesn't need drop.
3. **For contrast, `const` + Drop uses `EagerStorage`.** Its first access calls
   `destructors::register` (`native/eager.rs`, `get_or_init_slow`). That pushes onto a
   `Vec<_, System>` (`destructors/list.rs:5-20`; it uses System, not the global allocator), and
   calls `guard::enable()`, which runs `FlsAlloc`/`FlsSetValue` and maybe CRT `atexit`
   (`guard/windows.rs:110-188`). **Lazy (non-const) init** goes through `LazyStorage`. Use neither
   inside the allocator.
4. **std documents this as safe.**
   - `core/src/alloc/global.rs:165-176` says `std::thread_local` is "guaranteed to not use
     `#[global_allocator]`". This nightly repeats it at `core/src/alloc/mod.rs:513-515`.
   - `std/src/sys/thread_local/mod.rs:25-34` denies panicking lints, with the reason "TLS accesses
     must not call the global allocator, including via panic (#160930)".
5. **Use `try_with`, not `get`/`with`/`set`.**
   - `LocalKey::with` (`std/src/thread/local.rs:430-438`) and `LocalKey<Cell<T>>::get/set`
     (563, 530) have a cold `panic_access_error` path. It formats a message (388-393), and
     formatting allocates.
   - That path is dead for const `!Drop` keys, because `inner` never returns null.
   - `try_with` (465-471) returns `Result<R, AccessError>` and has no panic path at all:
     `RT_FLAG.try_with(Cell::get).unwrap_or(false)`.
6. **Not in these sources (Windows loader behaviour):** the EXE's implicit TLS block is allocated
   by the loader for every thread, including OS-created ones (WASAPI/COM pool threads), before
   any of our code runs on it. It doesn't use our allocator. A thread we didn't flag reads `false`
   and goes to System.

### 2.2 Re-entrancy and panic hazards

- **Unwinding out of a `GlobalAlloc` method is UB** (`core/src/alloc/global.rs:158-160`).
- **Panics allocate.** The panic handler builds the message in a `String` (`std/src/panicking.rs:620`),
  and boxes it (634). The hook and backtrace may allocate too. So a panic while we hold the pool
  lock re-enters `alloc`. On a flagged thread, that spins on our own lock forever. Panic sources:
  - rlsf `debug_assert!` (1.5);
  - our own overflow checks, indexing, `unwrap`, or `Layout::from_size_align(..).unwrap()`.
- **The skeleton's mitigations:**
  1. No panicking constructs under the lock. Use `wrapping_*`, and
     `Layout::from_size_align_unchecked`, which is sound because the `GlobalAlloc::realloc`
     contract guarantees validity.
  2. An abort-on-unwind guard around the pool calls. std uses the same trick in
     `abort_on_dtor_unwind`, `std/src/sys/thread_local/mod.rs:206-226`.
  3. A spin lock that records its owner. A thread that finds itself waiting on its own lock
     aborts instead of hanging.
  - `std::process::abort()` is `int 0x29` (`__fastfail`), with no allocation
    (`std/src/process.rs:2638`, `std/src/sys/pal/windows/mod.rs:269`). A crash dump beats a hung
    audio thread.
- **No calls that allocate or block inside the lock.** The only calls are TLSF ops, which are
  `no_std` and don't allocate.
  - Don't log, and don't use `std::sync::Mutex`, which "may allocate" (global.rs:166-170).
  - `std::thread::yield_now()` is a bare `SwitchToThread()` (`std/src/sys/thread/windows.rs:148-155`)
    and is allowed.
- **Priority inversion.** A non-RT thread that frees a pool block holds the lock for one O(1) TLSF
  op. If it's preempted there, the RT thread spins. `realloc` is worse: `Tlsf::reallocate` copies
  under the lock.
  - Keep the flag on RT threads and setup phases only.
  - Avoid non-RT frees of pool blocks in steady state.
- **Page faults under the lock.** TLSF writes headers into the pool. An unlocked page can take a
  hard fault while the lock is held. Use large pages or `VirtualLock`, and pre-fault before
  `install` (TODO #6).
- **System from an RT thread.** `HeapAlloc`/`HeapFree` take the process-heap lock. That's Windows
  behaviour and not in these sources. The skeleton counts every System free on a flagged thread
  (`rt_system_frees`) and every pool miss (`pool_fallbacks`).
- **Counters are diagnostics only.** The optimizer may elide allocations
  (`core/src/alloc/global.rs:129-148`).
  - The counters are global. Under parallel `cargo test` they're only meaningful if one test at a
    time sets the flag: one test per binary, or `--test-threads=1`.
  - For per-thread counts, add more const `Cell<u64>` thread-locals, which are equally
    allocation-free.
- **Toolchain note.** This nightly has `Allocator` stable since 1.100.0
  (`core/src/alloc/mod.rs:196-198`, `alloc/src/vec/mod.rs:1023`). On it, `System`'s `GlobalAlloc`
  comes from a blanket impl over the unstable `GlobalAllocator` trait
  (`core/src/alloc/global.rs:350-355`, `std/src/alloc.rs:315`).
  - Implementing `GlobalAlloc` directly for our type, as below, still works on stable 1.91,
    which is our `rust-version`.
  - Explicit `Vec<T, PoolAlloc>` would need rust-version >= 1.100.

### 2.3 Routing policy (our proposal, not from any source)

| Call | Flag clear | Flag set |
|---|---|---|
| `alloc` / `alloc_zeroed` | System | Pool. If the pool is empty or full: System, plus `pool_fallbacks`. Counts `rt_allocs`. |
| `dealloc` | Owner by address (pool range → TLSF, else System) | Same. Counts `rt_frees`, or `rt_system_frees` if the owner is System. |
| `realloc`, pool block | Move to System (alloc, copy, TLSF free) | `Tlsf::reallocate` in the pool. If it fails: move to System, plus `pool_fallbacks`. |
| `realloc`, System block | `System.realloc` | Move into the pool (alloc, copy, `System.dealloc`, counted). If the pool is full: `System.realloc`. |

The rule: **new memory comes from the allocator the current thread's flag selects, and old memory
goes back to its owner.** The address check is
`addr.wrapping_sub(start) < len`. `len` is loaded with Acquire before `start`, so a stale `start`
can't pair with a fresh `len`.

### 2.4 Skeleton (`crates/nf-mem/src/router.rs`)

The pre-implementation design. The real names differ: see the mapping at the start of §2.

```rust
//! Router global allocator: flagged (real-time) threads allocate from a TLSF pool over locked
//! memory; all other threads use System. Frees go to whichever allocator owns the address.
use core::alloc::{GlobalAlloc, Layout};
use core::cell::{Cell, UnsafeCell};
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use std::alloc::System;

use rlsf::Tlsf;

/// x64: GRANULARITY 32. FLLEN 28 → chunks up to 8 GiB. SLLEN 32 → good-fit waste ≤ ~1/32.
type Pool = Tlsf<'static, u32, u32, 28, 32>;

thread_local! {
    /// Const-initialised and !Drop: a bare #[thread_local] static on windows-msvc (2.1).
    static RT_FLAG: Cell<bool> = const { Cell::new(false) };
}

#[inline(always)]
fn rt_flag() -> bool {
    RT_FLAG.try_with(Cell::get).unwrap_or(false)
}

/// This thread's TLS address: a unique, allocation-free id for live threads. 0 = unknown.
#[inline(always)]
fn thread_token() -> usize {
    RT_FLAG.try_with(|c| ptr::from_ref(c).addr()).unwrap_or(0)
}

/// Flag the calling thread for its whole life (call first thing on each audio thread).
pub fn set_rt_thread(on: bool) {
    let _ = RT_FLAG.try_with(|c| c.set(on));
}

/// Scoped flag, e.g. for building a model or ring buffers on the control thread so their storage
/// lands in locked memory. !Send: it restores the flag of the thread that created it.
pub struct RtAllocScope {
    prev: bool,
    _not_send: PhantomData<*const ()>,
}
impl RtAllocScope {
    pub fn enter() -> Self {
        let prev = RT_FLAG.try_with(|c| c.replace(true)).unwrap_or(false);
        Self { prev, _not_send: PhantomData }
    }
}
impl Drop for RtAllocScope {
    fn drop(&mut self) {
        let _ = RT_FLAG.try_with(|c| c.set(self.prev));
    }
}

/// Unwinding out of GlobalAlloc is UB: turn any panic under our code into a fast-fail.
struct AbortOnUnwind;
impl Drop for AbortOnUnwind {
    #[inline]
    fn drop(&mut self) {
        std::process::abort()
    }
}

struct SpinLock {
    locked: AtomicBool,
    /// thread_token() of the holder, 0 when free. Only read to detect self-re-entry.
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
                // Only this thread ever writes `me`, so seeing it means we hold the lock: we were
                // re-entered from inside our own critical section (a panic). Spinning would hang.
                if me != 0 && self.owner.load(Relaxed) == me {
                    std::process::abort();
                }
                spins = spins.wrapping_add(1);
                if spins % 128 == 0 {
                    std::thread::yield_now(); // SwitchToThread; no allocation
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

#[repr(align(128))] // keep the counters off the lock's cache line
struct Counters {
    rt_allocs: AtomicU64,       // alloc/alloc_zeroed/realloc calls made with the flag set
    rt_frees: AtomicU64,        // pool frees made with the flag set
    rt_system_frees: AtomicU64, // System frees made with the flag set (heap calls on RT threads)
    pool_fallbacks: AtomicU64,  // flagged requests the pool couldn't serve (went to System)
    live: AtomicU64,            // pool bytes requested and not yet freed (written under the lock)
    peak: AtomicU64,            // high-water mark of `live`
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    pub rt_allocs: u64,
    pub rt_frees: u64,
    pub rt_system_frees: u64,
    pub pool_fallbacks: u64,
    pub pool_live_bytes: u64,
    pub pool_peak_bytes: u64,
}

pub struct Router {
    lock: SpinLock,
    start: AtomicUsize,
    len: AtomicUsize, // 0 until install(); never changes afterwards
    pool: UnsafeCell<Pool>,
    c: Counters,
}

// SAFETY: `pool` is only accessed with `lock` held; every other field is atomic.
unsafe impl Sync for Router {}

impl Router {
    pub const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            start: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            pool: UnsafeCell::new(Pool::new()),
            c: Counters {
                rt_allocs: AtomicU64::new(0),
                rt_frees: AtomicU64::new(0),
                rt_system_frees: AtomicU64::new(0),
                pool_fallbacks: AtomicU64::new(0),
                live: AtomicU64::new(0),
                peak: AtomicU64::new(0),
            },
        }
    }

    /// Hand the pool its memory. Returns the pool length, or None if already installed or the
    /// region is under 64 bytes.
    ///
    /// # Safety
    /// `region` is committed, read-write, pre-faulted (ideally locked or large pages), used for
    /// nothing else, and stays mapped until the process exits.
    pub unsafe fn install(&self, region: NonNull<[u8]>) -> Option<usize> {
        let _g = self.lock.lock();
        if self.len.load(Relaxed) != 0 {
            return None;
        }
        // SAFETY: lock held; region contract upheld by the caller.
        let n = unsafe { (*self.pool.get()).insert_free_block_ptr(region) }?.get();
        self.start.store(region.cast::<u8>().as_ptr().addr(), Relaxed);
        self.len.store(n, Release); // publish after `start`
        Some(n)
    }

    pub fn stats(&self) -> AllocStats {
        AllocStats {
            rt_allocs: self.c.rt_allocs.load(Relaxed),
            rt_frees: self.c.rt_frees.load(Relaxed),
            rt_system_frees: self.c.rt_system_frees.load(Relaxed),
            pool_fallbacks: self.c.pool_fallbacks.load(Relaxed),
            pool_live_bytes: self.c.live.load(Relaxed),
            pool_peak_bytes: self.c.peak.load(Relaxed),
        }
    }

    #[inline(always)]
    fn owns(&self, p: *mut u8) -> bool {
        let len = self.len.load(Acquire); // before `start` (pairs with install's Release)
        p.addr().wrapping_sub(self.start.load(Relaxed)) < len
    }

    // Both called with the lock held.
    #[inline]
    fn add_live(&self, n: usize) {
        let live = self.c.live.load(Relaxed).wrapping_add(n as u64);
        self.c.live.store(live, Relaxed);
        if live > self.c.peak.load(Relaxed) {
            self.c.peak.store(live, Relaxed);
        }
    }
    #[inline]
    fn sub_live(&self, n: usize) {
        self.c.live.store(self.c.live.load(Relaxed).wrapping_sub(n as u64), Relaxed);
    }

    /// Null if the pool isn't installed or can't fit `layout`.
    #[inline]
    fn pool_alloc(&self, layout: Layout) -> *mut u8 {
        if self.len.load(Relaxed) == 0 {
            return ptr::null_mut();
        }
        let bomb = AbortOnUnwind;
        let p = {
            let _g = self.lock.lock();
            // SAFETY: lock held → exclusive access.
            let pool = unsafe { &mut *self.pool.get() };
            match pool.allocate(layout) {
                Some(p) => {
                    self.add_live(layout.size());
                    p.as_ptr()
                }
                None => ptr::null_mut(),
            }
        };
        core::mem::forget(bomb);
        p
    }

    /// # Safety
    /// `p` is a live pool block allocated with `layout` (so `owns(p)` holds).
    #[inline]
    unsafe fn pool_free(&self, p: *mut u8, layout: Layout) {
        let bomb = AbortOnUnwind;
        {
            let _g = self.lock.lock();
            let pool = unsafe { &mut *self.pool.get() };
            // SAFETY: caller contract; GlobalAlloc passes the original layout, so align matches.
            unsafe { pool.deallocate(NonNull::new_unchecked(p), layout.align()) };
            self.sub_live(layout.size());
        }
        core::mem::forget(bomb);
    }
}

unsafe impl GlobalAlloc for Router {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !rt_flag() {
            return unsafe { System.alloc(layout) };
        }
        self.c.rt_allocs.fetch_add(1, Relaxed);
        let p = self.pool_alloc(layout);
        if !p.is_null() {
            return p;
        }
        self.c.pool_fallbacks.fetch_add(1, Relaxed);
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !rt_flag() {
            return unsafe { System.alloc_zeroed(layout) };
        }
        self.c.rt_allocs.fetch_add(1, Relaxed);
        let p = self.pool_alloc(layout);
        if !p.is_null() {
            // TLSF recycles memory; zero it here, outside the lock.
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
            return p;
        }
        self.c.pool_fallbacks.fetch_add(1, Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let rt = rt_flag();
        if self.owns(p) {
            if rt {
                self.c.rt_frees.fetch_add(1, Relaxed);
            }
            unsafe { self.pool_free(p, layout) }
        } else {
            if rt {
                self.c.rt_system_frees.fetch_add(1, Relaxed);
            }
            unsafe { System.dealloc(p, layout) }
        }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let rt = rt_flag();
        // SAFETY: GlobalAlloc::realloc guarantees new_size > 0 and no isize overflow when rounded.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if rt {
            self.c.rt_allocs.fetch_add(1, Relaxed);
        }
        if self.owns(p) {
            if rt {
                let bomb = AbortOnUnwind;
                let q = {
                    let _g = self.lock.lock();
                    let pool = unsafe { &mut *self.pool.get() };
                    // SAFETY: live pool block; new_layout.align() == original align.
                    let q = unsafe { pool.reallocate(NonNull::new_unchecked(p), new_layout) };
                    if q.is_some() {
                        self.sub_live(layout.size());
                        self.add_live(new_size);
                    }
                    q
                };
                core::mem::forget(bomb);
                if let Some(q) = q {
                    return q.as_ptr();
                }
                // Pool full; `p` is untouched (Tlsf::reallocate fails before changing anything).
                self.c.pool_fallbacks.fetch_add(1, Relaxed);
            }
            // Move out of the pool: flag clear, or the pool is full.
            let q = unsafe { System.alloc(new_layout) };
            if q.is_null() {
                return q; // `p` still valid, as GlobalAlloc requires
            }
            unsafe {
                ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                self.pool_free(p, layout);
            }
            q
        } else {
            if rt {
                // A System block resized on an RT thread: move it into the pool.
                let q = self.pool_alloc(new_layout);
                if !q.is_null() {
                    self.c.rt_system_frees.fetch_add(1, Relaxed);
                    unsafe {
                        ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                        System.dealloc(p, layout);
                    }
                    return q;
                }
                self.c.pool_fallbacks.fetch_add(1, Relaxed);
            }
            unsafe { System.realloc(p, layout, new_size) }
        }
    }
}
```

### 2.5 Wiring and tests

In the real code: `static GLOBAL: nf_mem::RtAllocator = nf_mem::RtAllocator::new();`
(`crates/nf-app/src/main.rs`), `nf_mem::install_pool(..)` at start-up, and `RtScope::enter()` on the
audio thread. The enforcement tests (`crates/{nf-core,nf-stages}/tests/rt_no_alloc.rs`,
`crates/nf-engine/src/test_alloc.rs`) install their own counting allocator; `crates/nf-mem/tests/global_alloc.rs`
installs `RtAllocator`.

```rust
// crates/nf-app/src/main.rs: exactly one #[global_allocator] per final binary.
#[global_allocator]
static GLOBAL: nf_mem::Router = nf_mem::Router::new();

fn main() {
    // Region from the TMR direct VirtualAlloc2 large-page path, or VirtualAlloc + VirtualLock.
    // Never MEM_REPLACE_PLACEHOLDER (CLAUDE.md).
    let region: core::ptr::NonNull<[u8]> = todo!("nf_mem large-page region, 64 MiB");
    unsafe { GLOBAL.install(region) }.expect("pool install");
    // audio thread body: nf_mem::set_rt_thread(true); ... loop ...
}
```

- The flag functions are free functions, so nf-engine doesn't need the `static`. Pass
  `&'static Router` (or a `fn() -> AllocStats`) to whoever displays stats.
- A library crate must not declare `#[global_allocator]`.
- **Enforcement test.** Give it its own integration-test binary (so its own `#[global_allocator]`),
  and use the 4 KB-page arena per CLAUDE.md. The pool may even stay uninstalled, because flagged
  allocations still count.

```rust
// crates/nf-engine/tests/no_alloc.rs
#[global_allocator]
static A: nf_mem::Router = nf_mem::Router::new();

#[test]
fn steady_state_allocates_nothing() {
    let mut chain = build_chain();                  // outside the flag
    let mut block = [0.0f32; 480];
    nf_mem::set_rt_thread(true);
    for _ in 0..16 { chain.process(&mut block); }   // warm-up may allocate
    let before = A.stats();
    for _ in 0..10_000 { chain.process(&mut block); }
    let after = A.stats();
    nf_mem::set_rt_thread(false);
    assert_eq!(after.rt_allocs, before.rt_allocs);
    assert_eq!(after.rt_system_frees, before.rt_system_frees);
    assert_eq!(after.rt_frees, before.rt_frees);
}
```

---

## 3. rtrb 0.4.1

### 3.1 Types and thread rules

- **`RingBuffer<T>`** (`rtrb/src/lib.rs:100`) holds:
  - `head` and `tail` as `CachePadded<AtomicUsize>`, which is `repr(align(128))` on x86_64
    (`src/cache_padded.rs:87-95`);
  - `flags: AtomicU8`;
  - `data_ptr: *mut T`;
  - `capacity`.

  The raw pointer makes it `!Send`/`!Sync`. You only reach it through `buffer()`.
- **`Producer<T>`** (305) and **`Consumer<T>`** (530):
  - `unsafe impl<T: Send> Send` (324, 560).
  - **Not `Sync`.** They hold `Cell` caches (312-319, 539-544). Not `Clone`.
  - Move each end into its thread. Don't share one through `Arc` or a `static`.
- Positions run over `0..2*capacity`, using compare-and-subtract with no modulo (182-236). The
  capacity is exact; it isn't rounded to a power of two.
- **Memory ordering:** an Acquire load of the other side's index, and a Release store of your own.
  Each side re-reads the other's index only when its cached view says "not enough" (490-505,
  778-793). On x86 these are plain `mov`s. It's wait-free.

### 3.2 Allocation behaviour: only at creation, and frees on the last drop

```rust
// lib.rs:150
#[must_use]
pub fn new(capacity: usize) -> (Producer<T>, Consumer<T>)
pub fn capacity(&self) -> usize                                  // RingBuffer, lib.rs:178
```

- `new` makes two allocations through the global allocator:
  - `Box<RingBuffer<T>>`, align 128 (155). That's about 384 B on x64 (our arithmetic).
  - `Vec::with_capacity(capacity)` (159), for `capacity * size_of::<T>()` bytes. It's skipped for
    ZSTs and capacity 0.
- After that, push, pop, chunks and slots **never allocate** (crate docs, lib.rs:7-9, which the
  code bears out). The exception is `T`'s own drops.
- **Panics:** `new` asserts `capacity.checked_mul(2).is_some()` for every `T` (151-154). The doc at
  128-129 mentions that limit only for ZSTs. `Vec::with_capacity` panics on byte overflow, and
  goes through `handle_alloc_error` (abort) on OOM.
- **Drop:** dropping the first handle sets `IS_ABANDONED`. **The second drop frees, on the
  dropping thread** (`arc_ring_buffer.rs:46-87`). `RingBuffer::drop` drops any queued `T`s, then
  frees the `Vec` and the `Box` (`lib.rs:239-259`). `Consumer::drop` first publishes `head` (547-556).
- **With the router**, storage goes where the creating thread's flag says: create under
  `RtAllocScope` for locked memory. It's freed by address on whichever thread drops last.

### 3.3 Producer (lib.rs, chunks.rs)

```rust
pub fn push(&mut self, value: T) -> Result<(), PushError<T>>                          // lib.rs:346
pub fn slots(&self) -> usize            // free slots; Acquire-loads head        lib.rs:377
pub fn cached_slots(&self) -> usize     // no atomic; may under-report           lib.rs:388
pub fn is_full(&self) -> bool                                                         // lib.rs:430
pub fn is_abandoned(&self) -> bool      // consumer dropped (Acquire)             lib.rs:477
pub fn buffer(&self) -> &RingBuffer<T>                                                // lib.rs:482
pub fn write_chunk(&mut self, n: usize) -> Result<WriteChunk<'_, T>, ChunkError>
    where T: Default                                                                  // chunks.rs:179
pub fn write_chunk_uninit(&mut self, n: usize) -> Result<WriteChunkUninit<'_, T>, ChunkError> // chunks.rs:217
// T: Copy
#[must_use] pub fn push_partial_slice<'a>(&mut self, slice: &'a [T]) -> (&'a [T], &'a [T]) // chunks.rs:281
pub fn push_entire_slice(&mut self, slice: &[T]) -> Result<(), ChunkError>             // chunks.rs:307
```

### 3.4 Consumer

```rust
pub fn pop(&mut self) -> Result<T, PopError>                                          // lib.rs:592
pub fn peek(&self) -> Result<&T, PeekError>                                           // lib.rs:639
pub fn slots(&self) -> usize            // readable slots; Acquire-loads tail     lib.rs:666
pub fn cached_slots(&self) -> usize                                                   // lib.rs:677
pub fn is_empty(&self) -> bool                                                        // lib.rs:719
pub fn is_abandoned(&self) -> bool      // producer dropped; queued items still readable  lib.rs:765
pub fn buffer(&self) -> &RingBuffer<T>                                                // lib.rs:770
pub fn read_chunk(&mut self, n: usize) -> Result<ReadChunk<'_, T>, ChunkError>        // chunks.rs:342
// T: Copy
#[must_use] pub fn pop_partial_slice<'a>(&mut self, slice: &'a mut [T]) -> (&'a mut [T], &'a mut [T]) // chunks.rs:408
#[must_use] pub fn pop_partial_slice_uninit<'a>(&mut self, slice: &'a mut [MaybeUninit<T>])
    -> (&'a mut [T], &'a mut [MaybeUninit<T>])                                        // chunks.rs:492
pub fn pop_entire_slice(&mut self, slice: &mut [T]) -> Result<(), ChunkError>          // chunks.rs:522
pub fn pop_entire_slice_uninit<'a>(&mut self, slice: &'a mut [MaybeUninit<T>])
    -> Result<&'a mut [T], ChunkError>                                                // chunks.rs:543
```

### 3.5 Chunks

```rust
pub struct WriteChunkUninit<'a, T> { /* first_ptr, first_len, second_ptr, second_len, producer: &'a Producer<T> */ } // chunks.rs:680
impl<T> WriteChunkUninit<'_, T> {
    pub fn as_mut_slices(&mut self) -> (&mut [MaybeUninit<T>], &mut [MaybeUninit<T>])  // 707
    pub unsafe fn commit(self, n: usize)        // first n must be initialised         727
    pub unsafe fn commit_all(self)              // all must be initialised             739
    pub fn fill_from_iter<I>(self, iter: I) -> usize where I: IntoIterator<Item = T>   // 800, safe, auto-commits
    #[must_use] pub fn len(&self) -> usize; #[must_use] pub fn is_empty(&self) -> bool // 827, 833
}
pub struct WriteChunk<'a, T>(WriteChunkUninit<'a, T>, PhantomData<T>);                // 568
impl<'a, T: Default> WriteChunk<'a, T> {
    pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T])                            // 615
    pub fn commit(self, n: usize); pub fn commit_all(self)                             // 635, 648
    #[must_use] pub fn len(&self) -> usize; #[must_use] pub fn is_empty(&self) -> bool // 657, 663
}
pub struct ReadChunk<'a, T> { /* ..., consumer: &'a Consumer<T> */ }                    // 857
impl<T> ReadChunk<'_, T> {
    #[must_use] pub fn as_slices(&self) -> (&[T], &[T])                                // 884
    #[must_use] pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T])                // 906
    pub fn commit(self, n: usize)   // drops (consumes) the first n                    969
    pub fn commit_all(self)                                                            // 977
    #[must_use] pub fn len(&self) -> usize; #[must_use] pub fn is_empty(&self) -> bool // 1024, 1030
}
impl<'a, T> IntoIterator for ReadChunk<'a, T> { type Item = T; type IntoIter = ReadChunkIntoIter<'a, T>; } // 1035
pub fn iterated(&self) -> usize   // ReadChunkIntoIter                                  // 1102
```

**Semantics:**
- **All or nothing.** `write_chunk_uninit(n)` and `read_chunk(n)` succeed only if `n` slots are
  available. Otherwise you get `Err(ChunkError::TooFewSlots(available))`. There's no panic, even
  for `n > capacity` (chunks.rs:217-231, 342-356).
- **Two slices:** the second is non-empty only at a wrap. The first is empty only when `n == 0`.
- **No `Drop` on `WriteChunkUninit` or `ReadChunk`.** Dropping one without committing publishes
  or consumes nothing (a peek).
  - `WriteChunk` *does* drop its default-initialised slots if you don't commit (570-577).
  - `ReadChunkIntoIter` consumes exactly what it iterated when dropped (1107-1119). If it's
    `mem::forget`-ed, the rest is leaked.
- **`write_chunk` writes `T::default()` into all `n` slots before you see them** (584-595). For
  `f32`, prefer `write_chunk_uninit` or `push_entire_slice`.
- **`commit(n)` with `n > len`** panics in debug builds and is capped in release (728, 970).
- **`CopyToUninit::copy_to_uninit(&self, dst: &'a mut [MaybeUninit<T>]) -> &'a mut [T]`**
  (lib.rs:807-832) uses `assert_eq!` on the lengths. A mismatch is a formatted panic, which
  allocates on the audio thread.
- `std::io::Write for Producer<u8>` and `std::io::Read for Consumer<u8>` return `WouldBlock`
  when full or empty (chunks.rs:1185-1216).

### 3.6 Errors

```rust
pub enum PushError<T> { Full(T) }          // lib.rs:872; Debug prints "Full(_)"
pub enum PopError { Empty }                // lib.rs:836
pub enum PeekError { Empty }               // lib.rs:854
pub enum ChunkError { TooFewSlots(usize) } // chunks.rs:1222; Copy, carries the available count
```

All four are `Display` and `std::error::Error` (with feature `std`). `PopError`, `PeekError` and
`ChunkError` are `Copy`.

### 3.7 Snippets for our threads

```rust
use rtrb::{chunks::ChunkError, Consumer, Producer, RingBuffer};

// Loopback reference: loopback capture thread → audio thread, mono f32 @ 48 kHz.
let (mut ref_tx, mut ref_rx) = {
    let _rt = nf_mem::RtAllocScope::enter();       // storage in the locked pool
    RingBuffer::<f32>::new(4 * 480)                // 40 ms
};

// Producer side: one WASAPI packet, all or nothing.
fn push_packet(tx: &mut Producer<f32>, pkt: &[f32], overruns: &mut u64) {
    if tx.push_entire_slice(pkt).is_err() { *overruns += 1; }
}

// Converting while writing (e.g. i16 → f32), no intermediate buffer. fill_from_iter commits
// exactly what it wrote, so a short iterator can't publish uninitialised slots.
fn push_i16(tx: &mut Producer<f32>, src: &[i16]) -> usize {
    match tx.write_chunk_uninit(src.len()) {
        Ok(chunk) => chunk.fill_from_iter(src.iter().map(|&s| s as f32 * (1.0 / 32768.0))),
        Err(ChunkError::TooFewSlots(_)) => 0,
    }
}

// Audio thread: one 480-frame block, zero copy, two slices at a wrap.
fn take_block(rx: &mut Consumer<f32>, mut f: impl FnMut(&[f32], &[f32])) -> bool {
    match rx.read_chunk(480) {
        Ok(chunk) => { let (a, b) = chunk.as_slices(); f(a, b); chunk.commit_all(); true }
        Err(ChunkError::TooFewSlots(_)) => false,  // underrun
    }
}
// Or copy into a fixed buffer:  rx.pop_entire_slice(&mut block[..])
// Drift controller input (fill level):  let fill = rx.slots();
// Drop one sample:  let _ = rx.pop();     Insert one:  duplicate locally, don't touch the ring.

// Telemetry: audio thread → GUI. Copy, no heap fields.
#[derive(Clone, Copy, Default)]
pub struct BlockStat { pub qpc_in: i64, pub qpc_out: i64, pub peak: f32, pub fill: u32 }
// audio:  if tx.push(stat).is_err() { dropped += 1; }   (never wait)
// GUI timer:  while let Ok(s) = rx.pop() { ... }
```

**Commands to the audio thread:**
- Use a `Copy` enum, e.g. `SetAtten(f32)`, `SetGate{..}`, `Bypass(bool)`.
- To swap heavy state, such as a new `Box<Stage>`, send it in on one ring and send the old one
  back on a second ring, so it's dropped on the control thread.
- A `Box` that the audio thread drops is a free on the RT thread: a heap call if it came from System.

### 3.8 rtrb pitfalls

- **Don't let the audio thread drop the last handle.** That frees the ring on the audio thread
  (3.2). Have the thread return its ends when it's joined, or keep the other end alive until after
  the join.
- **Non-`Copy` payloads run their destructors on the receiving thread.** That applies to `pop`,
  `ReadChunk::commit`, and to `PushError::Full(value)` handed back to the pusher when the ring is
  full.
- **`slots()` is a snapshot.** It can only grow until you act: the consumer's count grows as the
  producer writes, and vice versa. Fine as a fill-level reading for the drift PI loop.
- **`cached_slots()` can under-report.** Use it only as a fast path.
- **Debug-only panics:** commit-count checks, and `debug_assert!` on positions (lib.rs:184-230).
  The one panic that also fires in release is `copy_to_uninit`'s length `assert_eq!`. Keep
  slices exact; `push_entire_slice` and `pop_entire_slice` already are.

---

## 4. Docs that disagree with code, and things not in the sources

- **rlsf `GlobalTlsf`** shows on docs.rs but isn't compiled on Windows (1.1).
- **rlsf README's "maximum block size is 16 << FLLEN"** is the 32-bit figure. On x64 it's
  `(32 << FLLEN) - 32`, and the largest *request* is lower still (1.4).
- **rlsf `insert_free_block`** returns an opaque `impl Send + Sync` (tlsf.rs:702, 734).
- **rtrb `new` docs** describe the `usize::MAX / 2` capacity limit as ZST-only. The code asserts
  it for every `T` (lib.rs:128-129 vs 151-154).
- **Project docs** cite rtrb 0.4.0; the pin is `=0.4.1`.
- **This doc's own skeletons** (2.4, 2.5) use names nf-mem didn't keep (`Router`, `set_rt_thread`,
  `stats`): the real API is `RtAllocator`, `RtScope::enter()`, `counters()`, `thread_scope_allocs()`
  and `thread_scope_fallbacks()` (the mapping at the start of §2).
- **Not verifiable from rust-src:** that `x86_64-pc-windows-msvc` sets `target_thread_local`
  (inferred, 2.1), the loader's per-thread TLS allocation, the `HeapAlloc` locking, and
  `SwitchToThread` scheduling. All are Windows facts, outside these sources.
