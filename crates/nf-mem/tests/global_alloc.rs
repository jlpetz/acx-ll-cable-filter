//! `RtAllocator` as this test binary's real global allocator, over a 4 KB locked pool.
//!
//! The global counters are process-wide and libtest runs tests on parallel threads, so every test
//! takes `SERIAL` and the counter checks are deltas. Nothing inside a scope prints or formats
//! (assert messages are only built on failure, after the scope's allocations are counted).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use nf_mem::{PageKind, PagePolicy, RtAllocator, RtCounters, RtScope};

#[global_allocator]
static GLOBAL: RtAllocator = RtAllocator::new();

const MIB: usize = 1 << 20;
const POOL: usize = 2 * MIB;

static SERIAL: Mutex<()> = Mutex::new(());

/// Installs the pool once and serialises the tests.
fn setup() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let s = nf_mem::install_pool(PagePolicy::SmallLocked, POOL);
    assert_eq!(s.kind, PageKind::SmallLocked, "{}", s.describe());
    assert_eq!(s.bytes, POOL);
    g
}

fn delta(a: RtCounters, b: RtCounters) -> RtCounters {
    RtCounters {
        pool_allocs: b.pool_allocs - a.pool_allocs,
        pool_frees: b.pool_frees - a.pool_frees,
        fallback_allocs: b.fallback_allocs - a.fallback_allocs,
        pool_bytes_in_use: b.pool_bytes_in_use.wrapping_sub(a.pool_bytes_in_use),
        pool_peak_bytes: 0,
    }
}

#[test]
fn install_reports_the_pool() {
    let _g = setup();
    let s = nf_mem::pool_status().unwrap();
    assert_eq!(s.describe(), "4 KB locked (2 MiB)");
    // Later calls don't replace it.
    assert_eq!(nf_mem::install_pool(PagePolicy::SmallUnlocked, MIB), s);
}

#[test]
fn scoped_collections_live_in_the_pool_and_free_anywhere() {
    let _g = setup();
    let c0 = nf_mem::counters();
    let a0 = nf_mem::thread_scope_allocs();
    let f0 = nf_mem::thread_scope_fallbacks();

    let (v, s, m) = {
        let _scope = RtScope::enter();
        let mut v: Vec<u64> = Vec::with_capacity(1000);
        v.extend(0..1000);
        let s = String::from("pool");
        let mut m: HashMap<u32, u32> = HashMap::with_capacity(64);
        m.insert(1, 2);
        (v, s, m)
    };
    let c1 = nf_mem::counters();
    assert_eq!(nf_mem::thread_scope_allocs() - a0, 3);
    assert_eq!(nf_mem::thread_scope_fallbacks(), f0);
    let d = delta(c0, c1);
    assert_eq!((d.pool_allocs, d.pool_frees, d.fallback_allocs), (3, 0, 0));
    assert!(d.pool_bytes_in_use >= 8000 + 4);
    assert!(c1.pool_peak_bytes >= c1.pool_bytes_in_use);

    // Growing outside the scope moves the Vec to the system heap; the data comes along.
    let mut v = v;
    v.reserve(100_000);
    assert_eq!(v.iter().sum::<u64>(), 999 * 1000 / 2);
    let c2 = nf_mem::counters();
    assert_eq!(delta(c1, c2).pool_frees, 1);

    // The others are freed on another thread, routed by address.
    std::thread::spawn(move || {
        assert_eq!(s, "pool");
        assert_eq!(m[&1], 2);
    })
    .join()
    .unwrap();
    drop(v);
    let c3 = nf_mem::counters();
    let d = delta(c0, c3);
    assert_eq!((d.pool_allocs, d.pool_frees, d.pool_bytes_in_use), (3, 3, 0));
}

#[test]
fn a_full_pool_falls_back_to_the_system_heap() {
    let _g = setup();
    let c0 = nf_mem::counters();
    let f0 = nf_mem::thread_scope_fallbacks();
    let big = {
        let _scope = RtScope::enter();
        vec![7u8; 2 * POOL] // can't fit
    };
    assert_eq!(nf_mem::thread_scope_fallbacks() - f0, 1);
    let d = delta(c0, nf_mem::counters());
    assert_eq!((d.pool_allocs, d.fallback_allocs), (0, 1));
    assert!(big.iter().all(|&b| b == 7));
    drop(big);
    assert_eq!(delta(c0, nf_mem::counters()).pool_frees, 0);
}

#[test]
fn a_scoped_thread_churns_without_leaking() {
    let _g = setup();
    let c0 = nf_mem::counters();
    let (allocs, fallbacks) = std::thread::spawn(|| {
        let _scope = RtScope::enter();
        let mut keep: Vec<Box<[u8]>> = Vec::new();
        for i in 0..2000usize {
            let b = vec![i as u8; 1 + (i * 37) % 3000].into_boxed_slice();
            if i % 3 == 0 {
                keep.push(b);
            }
            if keep.len() > 40 {
                keep.clear();
            }
        }
        drop(keep);
        (nf_mem::thread_scope_allocs(), nf_mem::thread_scope_fallbacks())
    })
    .join()
    .unwrap();
    assert!(allocs >= 2000, "{allocs}");
    assert_eq!(fallbacks, 0);
    let d = delta(c0, nf_mem::counters());
    assert_eq!(d.pool_allocs, d.pool_frees);
    assert_eq!(d.pool_bytes_in_use, 0);
    assert_eq!(d.fallback_allocs, 0);
}

#[test]
fn a_panic_inside_a_scope_unwinds_and_restores_the_flag() {
    let _g = setup();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // keep the output quiet
    let r = std::panic::catch_unwind(|| {
        let _scope = RtScope::enter();
        let v: Vec<u8> = (0..100).collect();
        panic!("inside a scope, {}", v.len()); // the payload is allocated in the pool
    });
    std::panic::set_hook(hook);
    assert!(r.is_err());
    drop(r);
    // The scope was dropped during unwinding: this allocation is unscoped.
    let a0 = nf_mem::thread_scope_allocs();
    let v = vec![0u8; 10];
    assert_eq!(nf_mem::thread_scope_allocs(), a0);
    drop(v);
}
