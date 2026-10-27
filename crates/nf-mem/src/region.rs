//! The pool's memory: one private region, reserved and committed in a single call.
//!
//! - Large pages: TMR's direct path (`TMR-APP/src/memory/backend.rs`), `VirtualAlloc2` with
//!   `MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES`. **Never** a placeholder reserve or replace: a
//!   refused large-page replace followed by a free bugchecks build 26100 (0x139, CLAUDE.md).
//! - 4 KB pages: `VirtualAlloc`, then (locked) the working-set minimum is raised to cover them and
//!   `VirtualLock` pins them, which also faults every page in.
//!
//! Every refusal is classified into a reason for the user; nothing falls back silently.

use core::ptr::{self, NonNull};
use std::sync::Mutex;

use windows::Win32::Foundation::{GetLastError, WIN32_ERROR};
use windows::Win32::Security::SE_INC_WORKING_SET_NAME;
use windows::Win32::System::Memory::{
    GetLargePageMinimum, GetProcessWorkingSetSizeEx, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_LARGE_PAGES, MEM_RELEASE,
    MEM_RESERVE, MemExtendedParameterAttributeFlags, PAGE_READWRITE, QUOTA_LIMITS_HARDWS_MAX_DISABLE,
    QUOTA_LIMITS_HARDWS_MIN_ENABLE, SetProcessWorkingSetSizeEx, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualLock,
    VirtualUnlock,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::{PageKind, PagePolicy, PoolStatus};

/// x64 small-page size.
pub(crate) const PAGE: usize = 4096;
/// Working-set headroom on top of the locked bytes: `VirtualLock` is capped at the minimum working
/// set "minus a small overhead", and the process's own pages share that minimum.
const LOCK_MARGIN: usize = 1 << 20;
/// `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE` and `MEM_EXTENDED_PARAMETER_TYPE_BITS`
/// (`Win32::System::SystemServices`, not worth that feature for two constants).
const NONPAGED_LARGE: u64 = 8;
const TYPE_BITS: u32 = 8;

const MIB: usize = 1 << 20;

/// The Win32 code inside a `windows` error (or the raw HRESULT bits if it isn't one).
pub(crate) fn win32_code(e: &windows::core::Error) -> u32 {
    WIN32_ERROR::from_error(e).map_or(e.code().0 as u32, |w| w.0)
}

/// One owned private region. Released (and unlocked) on drop; the app's pool is leaked instead.
pub(crate) struct Region {
    ptr: NonNull<u8>,
    len: usize,
    locked: bool,
}

// SAFETY: a Region is the unique owner of a private VirtualAlloc region, which any thread may use
// or release.
unsafe impl Send for Region {}
// SAFETY: as above; `&Region` only exposes the base and length.
unsafe impl Sync for Region {}

impl Region {
    /// 4 KB pages, committed, not locked. `bytes` is rounded up to whole pages.
    pub(crate) fn small(bytes: usize) -> Result<Region, String> {
        let Some(len) = bytes.max(1).checked_next_multiple_of(PAGE) else {
            return Err(format!("pool size too large ({bytes} bytes)"));
        };
        // SAFETY: a fresh private region; no address hint.
        let p = unsafe { VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        match NonNull::new(p.cast::<u8>()) {
            Some(ptr) => Ok(Region { ptr, len, locked: false }),
            None => {
                // SAFETY: read straight after the failed call.
                let code = unsafe { GetLastError() }.0;
                Err(format!("couldn't reserve {} ({code})", mib(len)))
            }
        }
    }

    /// Large pages, reserved and committed in one `VirtualAlloc2` call (TMR's direct path).
    /// `bytes` is rounded up to `GetLargePageMinimum()`. The error is worded for the user.
    pub(crate) fn large(bytes: usize) -> Result<Region, String> {
        // SAFETY: no arguments.
        let page = unsafe { GetLargePageMinimum() };
        if page == 0 {
            return Err("large pages unsupported".into());
        }
        let Some(len) = bytes.max(1).checked_next_multiple_of(page) else {
            return Err(format!("pool size too large ({bytes} bytes)"));
        };
        let mut attr = MEM_EXTENDED_PARAMETER::default();
        attr.Anonymous1._bitfield = (MemExtendedParameterAttributeFlags.0 as u64) & ((1u64 << TYPE_BITS) - 1);
        attr.Anonymous2.ULong64 = NONPAGED_LARGE;
        let mut params = [attr];
        // SAFETY: `params` holds plain values (no pointers) and lives across the call. Reserve and
        // commit in one call; no placeholder flags.
        let p = unsafe {
            VirtualAlloc2(None, None, len, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, PAGE_READWRITE.0, Some(&mut params))
        };
        match NonNull::new(p.cast::<u8>()) {
            // Large pages are non-pageable and come back zeroed: nothing to lock or pre-fault.
            Some(ptr) => Ok(Region { ptr, len, locked: false }),
            None => {
                // SAFETY: read straight after the failed call.
                let code = unsafe { GetLastError() }.0;
                let filtered = code == 1314 && crate::privilege::is_uac_filtered();
                Err(large_refusal_reason(code, filtered))
            }
        }
    }

    /// Raises the working-set minimum to cover the region, then pins it with `VirtualLock`
    /// (which faults every page in). On failure the region stays usable, unlocked.
    pub(crate) fn lock(&mut self) -> Result<(), String> {
        if self.locked {
            return Ok(());
        }
        let raised = raise_working_set(self.len.saturating_add(LOCK_MARGIN));
        // SAFETY: the whole range is our own committed region.
        if let Err(e) = unsafe { VirtualLock(self.ptr.as_ptr().cast(), self.len) } {
            let lock = format!("couldn't lock the pages ({})", win32_code(&e));
            return Err(match raised {
                Ok(()) => lock,
                Err(ws) => format!("{ws}; {lock}"),
            });
        }
        self.locked = true;
        // VirtualLock already made every page resident; touch each one anyway so no first write
        // can fault later (cheap: 256 pages per MiB, at start-up).
        for off in (0..self.len).step_by(PAGE) {
            // SAFETY: `off < len`, inside our committed read-write region, which nothing uses yet.
            unsafe { ptr::write_volatile(self.ptr.as_ptr().add(off), 0) };
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_slice_ptr(&self) -> NonNull<[u8]> {
        NonNull::slice_from_raw_parts(self.ptr, self.len)
    }

    /// Keeps the region mapped for the rest of the process (the global pool).
    pub(crate) fn leak(self) -> NonNull<[u8]> {
        let s = self.as_slice_ptr();
        core::mem::forget(self);
        s
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: we own the region. MEM_RELEASE takes the base address and size 0. The
        // working-set minimum raised for it stays raised (harmless; only tests drop regions).
        unsafe {
            if self.locked {
                let _ = VirtualUnlock(self.ptr.as_ptr().cast(), self.len);
            }
            let _ = VirtualFree(self.ptr.as_ptr().cast(), 0, MEM_RELEASE);
        }
    }
}

/// Why a large-page request was refused, for the UI (`code` from `GetLastError`).
pub(crate) fn large_refusal_reason(code: u32, uac_filtered: bool) -> String {
    match code {
        1314 if uac_filtered => "no lock-pages privilege in this UAC-filtered token (1314)".into(),
        1314 => "no lock-pages privilege (1314)".into(),
        1450 => "no contiguous memory (1450)".into(),
        1455 => "commit limit reached (1455)".into(),
        8 | 14 => format!("out of memory ({code})"),
        _ => format!("large pages refused ({code})"),
    }
}

/// Reserves the pool's region for `policy`, with every fallback recorded in the status.
pub(crate) fn reserve(policy: PagePolicy, bytes: usize) -> (Option<Region>, PoolStatus) {
    let status = |kind, bytes, reasons: &[String]| PoolStatus {
        kind,
        bytes,
        fallback_reason: if reasons.is_empty() { None } else { Some(reasons.join("; ")) },
    };
    if bytes == 0 {
        return (None, status(PageKind::None, 0, &["no pool requested (size 0)".into()]));
    }
    let mut reasons = Vec::new();
    if policy == PagePolicy::Large {
        match Region::large(bytes) {
            Ok(r) => {
                let len = r.len();
                return (Some(r), status(PageKind::Large, len, &reasons));
            }
            Err(why) => reasons.push(why),
        }
    }
    let mut region = match Region::small(bytes) {
        Ok(r) => r,
        Err(why) => {
            reasons.push(why);
            return (None, status(PageKind::None, 0, &reasons));
        }
    };
    let kind = if policy == PagePolicy::SmallUnlocked {
        PageKind::SmallUnlocked
    } else {
        match region.lock() {
            Ok(()) => PageKind::SmallLocked,
            Err(why) => {
                reasons.push(why);
                PageKind::SmallUnlocked
            }
        }
    };
    let len = region.len();
    (Some(region), status(kind, len, &reasons))
}

/// Serialises read-modify-write of the process's working-set limits.
static WS_LOCK: Mutex<()> = Mutex::new(());

/// Raises the hard working-set minimum (and the soft maximum) by `extra` bytes.
pub(crate) fn raise_working_set(extra: usize) -> Result<(), String> {
    // Raising the limits needs SeIncreaseWorkingSetPrivilege, which users hold but may have
    // disabled; enabling it is cheap insurance, and a refusal shows up in the call below.
    let _ = crate::privilege::enable_privilege(SE_INC_WORKING_SET_NAME);
    let _g = WS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
    // SAFETY: GetCurrentProcess is a pseudo-handle that needs no closing; the out-pointers are
    // live locals.
    unsafe {
        let me = GetCurrentProcess();
        if !GetProcessWorkingSetSizeEx(me, &mut min, &mut max, &mut flags).as_bool() {
            return Err(format!("couldn't read the working-set limits ({})", GetLastError().0));
        }
        let new_min = min.saturating_add(extra);
        let new_max = max.saturating_add(extra).max(new_min);
        SetProcessWorkingSetSizeEx(me, new_min, new_max, QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE)
            .map_err(|e| format!("couldn't raise the working-set minimum to {} ({})", mib(new_min), win32_code(&e)))
    }
}

/// The working-set limits (minimum, maximum), for tests and diagnostics.
#[cfg(test)]
pub(crate) fn working_set_limits() -> (usize, usize) {
    let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
    // SAFETY: pseudo-handle; live out-pointers.
    let ok = unsafe { GetProcessWorkingSetSizeEx(GetCurrentProcess(), &mut min, &mut max, &mut flags) };
    assert!(ok.as_bool());
    (min, max)
}

fn mib(bytes: usize) -> String {
    if bytes.is_multiple_of(MIB) { format!("{} MiB", bytes / MIB) } else { format!("{:.1} MiB", bytes as f64 / MIB as f64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_all(r: &Region) {
        let base = r.as_slice_ptr().cast::<u8>().as_ptr();
        for off in (0..r.len()).step_by(PAGE) {
            // SAFETY: inside the committed region.
            unsafe {
                base.add(off).write(0x5A);
                assert_eq!(base.add(off).read(), 0x5A);
            }
        }
        // SAFETY: the last byte is inside the region.
        unsafe { base.add(r.len() - 1).write(1) };
    }

    #[test]
    fn small_regions_round_up_to_pages_and_are_writable() {
        for bytes in [1, PAGE, PAGE + 1, MIB + 1, 3 * MIB] {
            let r = Region::small(bytes).unwrap();
            assert_eq!(r.len(), bytes.next_multiple_of(PAGE));
            assert_eq!(r.as_slice_ptr().cast::<u8>().as_ptr().addr() % PAGE, 0);
            touch_all(&r);
        }
    }

    #[test]
    fn reserve_small_unlocked_1_to_4_mib() {
        for mibs in 1..=4 {
            let (r, s) = reserve(PagePolicy::SmallUnlocked, mibs * MIB);
            let r = r.unwrap();
            assert_eq!(s, PoolStatus { kind: PageKind::SmallUnlocked, bytes: mibs * MIB, fallback_reason: None });
            assert!(!r.locked);
            touch_all(&r);
        }
    }

    #[test]
    fn reserve_small_locked_1_to_4_mib() {
        for bytes in [MIB, 2 * MIB, 3 * MIB + 1, 4 * MIB] {
            let (min_before, _) = working_set_limits();
            let (r, s) = reserve(PagePolicy::SmallLocked, bytes);
            let r = r.unwrap();
            assert_eq!(s.kind, PageKind::SmallLocked, "{s:?}");
            assert_eq!(s.bytes, bytes.next_multiple_of(PAGE));
            assert_eq!(s.fallback_reason, None);
            assert!(r.locked);
            assert!(working_set_limits().0 >= min_before + r.len(), "the minimum must cover the locked pages");
            touch_all(&r);
        }
    }

    #[test]
    fn locking_twice_is_a_no_op() {
        let mut r = Region::small(MIB).unwrap();
        r.lock().unwrap();
        r.lock().unwrap();
        assert!(r.locked);
    }

    #[test]
    fn zero_bytes_means_no_pool() {
        for policy in [PagePolicy::Large, PagePolicy::SmallLocked, PagePolicy::SmallUnlocked] {
            let (r, s) = reserve(policy, 0);
            assert!(r.is_none());
            assert_eq!((s.kind, s.bytes), (PageKind::None, 0));
            assert!(s.fallback_reason.is_some());
        }
    }

    #[test]
    fn absurd_sizes_are_refused_with_a_reason() {
        let (r, s) = reserve(PagePolicy::SmallUnlocked, usize::MAX);
        assert!(r.is_none());
        assert_eq!(s.kind, PageKind::None);
        assert!(s.fallback_reason.unwrap().contains("too large"));
        // Bigger than the user address space: VirtualAlloc refuses at once, and the code is reported.
        let (r, s) = reserve(PagePolicy::SmallUnlocked, 1 << 48);
        assert!(r.is_none());
        assert!(s.fallback_reason.unwrap().starts_with("couldn't reserve"));
    }

    #[test]
    fn large_refusals_are_worded_for_the_user() {
        assert_eq!(large_refusal_reason(1314, false), "no lock-pages privilege (1314)");
        assert_eq!(large_refusal_reason(1314, true), "no lock-pages privilege in this UAC-filtered token (1314)");
        assert_eq!(large_refusal_reason(1450, false), "no contiguous memory (1450)");
        assert_eq!(large_refusal_reason(1455, false), "commit limit reached (1455)");
        assert_eq!(large_refusal_reason(8, false), "out of memory (8)");
        assert_eq!(large_refusal_reason(87, false), "large pages refused (87)");
    }

    #[test]
    fn working_set_minimum_can_be_raised() {
        let (min0, _) = working_set_limits();
        raise_working_set(MIB).unwrap();
        let (min1, max1) = working_set_limits();
        assert!(min1 >= min0 + MIB);
        assert!(max1 >= min1);
    }

    #[test]
    fn win32_codes_come_out_of_errors() {
        let e = windows::core::Error::from(WIN32_ERROR(1453));
        assert_eq!(win32_code(&e), 1453);
    }

    #[test]
    fn sizes_are_shown_in_mib() {
        assert_eq!(mib(32 * MIB), "32 MiB");
        assert_eq!(mib(MIB + MIB / 2), "1.5 MiB");
    }

    /// Allocates 2 MiB of large pages, so the owner runs it (CLAUDE.md):
    /// `cargo test -p nf-mem -- --ignored large_pages_region`.
    #[test]
    #[ignore = "allocates large pages; the owner runs it"]
    fn large_pages_region() {
        let _ = crate::privilege::enable_lock_memory_privilege();
        let (r, s) = reserve(PagePolicy::Large, 3 * MIB);
        eprintln!("{}", s.describe());
        let r = r.expect("some pool");
        match s.kind {
            PageKind::Large => {
                assert_eq!(r.len() % (2 * MIB), 0);
                assert!(r.len() >= 3 * MIB);
                assert_eq!(s.fallback_reason, None);
            }
            PageKind::SmallLocked | PageKind::SmallUnlocked => assert!(s.fallback_reason.is_some()),
            PageKind::None => unreachable!(),
        }
        touch_all(&r);
    }
}
