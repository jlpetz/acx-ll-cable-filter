//! nf-mem: memory the audio path can't lose to paging (TODO #6).
//!
//! - [`install_pool`] reserves one region at start-up: large pages (2 MiB multiples) when the
//!   lock-pages privilege is enabled, otherwise 4 KB pages pinned with `VirtualLock`. The status
//!   says which, and why.
//! - [`RtAllocator`] is the app's `#[global_allocator]`. Allocations made on a thread inside an
//!   [`RtScope`] come from the pool (TLSF, O(1)); all others go to the system heap. Frees are routed by
//!   address, so any thread may free pool memory.
//! - [`privilege`] enables `SeLockMemoryPrivilege` at run time, and grants it to a given SID from an
//!   elevated helper (the `setup-large-pages` command).
//! - [`set_working_set_minimum`] keeps code, stacks and heaps resident.
//!
//! Tests never take the large-page path (CLAUDE.md): they use [`PagePolicy::SmallUnlocked`] or
//! [`PagePolicy::SmallLocked`].
//!
//! Modules: `router` (the allocator, its lock and counters), `region` (reserving, locking and the
//! working set), [`privilege`].
#![warn(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout};
use std::marker::PhantomData;
use std::sync::OnceLock;

pub mod privilege;
mod region;
mod router;

/// What kind of memory to reserve for the pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagePolicy {
    /// Large pages; fall back to `SmallLocked` (with the reason recorded) if refused.
    Large,
    /// 4 KB pages, `VirtualLock`ed (after raising the working-set minimum to cover them).
    SmallLocked,
    /// 4 KB pages, not locked (tests).
    SmallUnlocked,
}

/// What the pool actually got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Large,
    SmallLocked,
    SmallUnlocked,
    /// No pool: everything goes to the system heap.
    None,
}

/// The outcome of [`install_pool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolStatus {
    pub kind: PageKind,
    pub bytes: usize,
    /// Why the requested policy wasn't met (e.g. "no lock-pages privilege (1314)",
    /// "no contiguous memory (1450)"), worded for the user.
    pub fallback_reason: Option<String>,
}

impl PoolStatus {
    /// One line for the UI, e.g. "large pages (16 MiB)" or "4 KB locked (16 MiB): no lock-pages privilege".
    pub fn describe(&self) -> String {
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        let kind = match self.kind {
            PageKind::Large => "large pages",
            PageKind::SmallLocked => "4 KB locked",
            PageKind::SmallUnlocked => "4 KB unlocked",
            PageKind::None => "system heap",
        };
        match &self.fallback_reason {
            Some(r) => format!("{kind} ({mib:.0} MiB): {r}"),
            None => format!("{kind} ({mib:.0} MiB)"),
        }
    }
}

static STATUS: OnceLock<PoolStatus> = OnceLock::new();

/// Reserves the pool and installs it in [`RtAllocator`]. Call once, at start-up, before any
/// [`RtScope`]. A second call returns the existing status.
///
/// `Large` falls back to 4 KB locked pages, and a failed lock to unlocked ones; each step's reason
/// is recorded. If even 4 KB pages can't be reserved the kind is [`PageKind::None`].
pub fn install_pool(policy: PagePolicy, bytes: usize) -> PoolStatus {
    STATUS
        .get_or_init(|| {
            let (region, status) = region::reserve(policy, bytes);
            let Some(region) = region else { return status };
            // SAFETY: the region is committed read-write memory that nothing else uses, and it is
            // leaked on success, so it stays mapped until the process exits.
            match unsafe { router::ROUTER.install(region.as_slice_ptr()) } {
                Some(_) => {
                    region.leak();
                    if matches!(status.kind, PageKind::Large | PageKind::SmallLocked) {
                        pin_control_block();
                    }
                    status
                }
                // Unreachable in practice (regions are at least a page, and this runs once);
                // the region is released on drop.
                None => PoolStatus { kind: PageKind::None, bytes: 0, fallback_reason: Some("the pool couldn't be set up".into()) },
            }
        })
        .clone()
}

/// Best effort: a pinned pool whose TLSF bookkeeping pages out could still fault under the lock.
/// It's a few pages; if the lock quota is short (large pages don't raise the working set), raise
/// it a little and retry. On failure those pages are only as pageable as the rest of the exe's
/// data, which the hard working-set minimum protects, so it isn't reported as a fallback.
fn pin_control_block() {
    if router::ROUTER.lock_control_block().is_err() {
        let _ = region::raise_working_set(64 * 1024);
        let _ = router::ROUTER.lock_control_block();
    }
}

/// The installed pool's status, if any.
pub fn pool_status() -> Option<PoolStatus> {
    STATUS.get().cloned()
}

/// Counters for the pool and the scopes.
///
/// A resize inside the pool counts as one free plus one allocation, so `pool_allocs - pool_frees`
/// is always the number of live pool blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtCounters {
    /// Allocations served from the pool.
    pub pool_allocs: u64,
    /// Pool blocks freed, on any thread.
    pub pool_frees: u64,
    /// Allocations made inside an `RtScope` that fell back to the system heap (pool full, or none).
    pub fallback_allocs: u64,
    /// Requested bytes of the live pool blocks (TLSF's per-block overhead isn't included).
    pub pool_bytes_in_use: u64,
    /// The high-water mark of `pool_bytes_in_use`.
    pub pool_peak_bytes: u64,
}

pub fn counters() -> RtCounters {
    router::ROUTER.counters()
}

/// Allocations (pool or fallback) made by the calling thread inside an `RtScope` so far. The audio
/// thread reads it after warm-up and then once a second; growth means a real-time allocation.
/// Resizes count too.
pub fn thread_scope_allocs() -> u64 {
    router::scope_allocs()
}

/// Allocations made by the calling thread inside an `RtScope` that fell back to the system heap
/// (pool full or absent). The real-time hazard: the engine publishes its growth after warm-up.
pub fn thread_scope_fallbacks() -> u64 {
    router::scope_fallbacks()
}

/// While alive, the calling thread's allocations come from the pool. Nests; restores on drop.
///
/// `!Send`: dropping it restores the flag of the thread it's dropped on, so it must stay on the
/// thread that entered it.
#[must_use = "the scope ends when this is dropped"]
pub struct RtScope {
    prev: bool,
    _not_send: PhantomData<*const ()>,
}

impl RtScope {
    #[inline]
    pub fn enter() -> Self {
        Self { prev: router::replace_scope_flag(true), _not_send: PhantomData }
    }
}

impl Drop for RtScope {
    #[inline]
    fn drop(&mut self) {
        router::set_scope_flag(self.prev);
    }
}

/// The app's global allocator (installed in nf-app's main.rs).
pub struct RtAllocator;

impl RtAllocator {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RtAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every method delegates to the static router, which upholds GlobalAlloc's contract (see
// its impl): memory fits the layout, frees go to the owning allocator with the original layout,
// and it never allocates through the global allocator or unwinds.
unsafe impl GlobalAlloc for RtAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's GlobalAlloc contract, passed through.
        unsafe { router::ROUTER.alloc(layout) }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's GlobalAlloc contract, passed through.
        unsafe { router::ROUTER.alloc_zeroed(layout) }
    }
    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's GlobalAlloc contract, passed through; `ptr` came from the router.
        unsafe { router::ROUTER.dealloc(ptr, layout) }
    }
    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's GlobalAlloc contract, passed through; `ptr` came from the router.
        unsafe { router::ROUTER.realloc(ptr, layout, new_size) }
    }
}

/// Raises the process's working-set minimum by `extra_bytes` (hard minimum,
/// `QUOTA_LIMITS_HARDWS_MIN_ENABLE`), so code, stacks and heaps stay resident. No admin needed.
pub fn set_working_set_minimum(extra_bytes: usize) -> Result<(), String> {
    region::raise_working_set(extra_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    // The only test in this binary that touches the global pool. The static router isn't the
    // global allocator here (no #[global_allocator] in a library), so installing it is inert;
    // tests/global_alloc.rs covers it as the real allocator.
    #[test]
    fn install_pool_once_then_report_it() {
        let s = install_pool(PagePolicy::SmallUnlocked, MIB + 1);
        assert_eq!(s, PoolStatus { kind: PageKind::SmallUnlocked, bytes: MIB + 4096, fallback_reason: None });
        assert_eq!(pool_status(), Some(s.clone()));
        // A second call keeps the first pool.
        assert_eq!(install_pool(PagePolicy::SmallLocked, 4 * MIB), s);
        assert_eq!(counters(), RtCounters::default());
    }

    #[test]
    fn the_static_routers_control_block_can_be_pinned() {
        assert_eq!(router::ROUTER.lock_control_block(), Ok(()));
    }

    #[test]
    fn working_set_minimum_rises() {
        let (min0, _) = region::working_set_limits();
        set_working_set_minimum(2 * MIB).unwrap();
        assert!(region::working_set_limits().0 >= min0 + 2 * MIB);
    }

    #[test]
    fn scope_flags_and_counters_are_per_thread() {
        let before = (thread_scope_allocs(), thread_scope_fallbacks());
        {
            let _s = RtScope::enter();
            assert!(router::in_scope());
        }
        assert!(!router::in_scope());
        // A fresh thread starts at zero.
        let fresh = std::thread::spawn(|| (thread_scope_allocs(), thread_scope_fallbacks(), router::in_scope()));
        assert_eq!(fresh.join().unwrap(), (0, 0, false));
        // Nothing here allocated through a router.
        assert_eq!((thread_scope_allocs(), thread_scope_fallbacks()), before);
    }

    #[test]
    fn describe_reads_well() {
        let s = |kind, bytes, r: Option<&str>| PoolStatus { kind, bytes, fallback_reason: r.map(String::from) };
        assert_eq!(s(PageKind::Large, 32 * MIB, None).describe(), "large pages (32 MiB)");
        assert_eq!(
            s(PageKind::SmallLocked, 16 * MIB, Some("no lock-pages privilege (1314)")).describe(),
            "4 KB locked (16 MiB): no lock-pages privilege (1314)"
        );
        assert_eq!(s(PageKind::None, 0, None).describe(), "system heap (0 MiB)");
    }
}
