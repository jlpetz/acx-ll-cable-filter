//! TODO 6's measurement of ONNX Runtime's heap traffic, without admin rights or external tools: the process's import address
//! tables, patched (tests and `bench` only; never the app's live path).
//!
//! **What it sees.** Rust's `System` allocator calls `HeapAlloc` directly. ONNX Runtime, linked statically, calls the C
//! runtime: its C++ `operator new` (msvcrt.lib's static part) and its CPU allocator (`_aligned_malloc`) reach `malloc`,
//! `free`, `calloc`, `realloc`, the `_aligned_*` family, `_recalloc` and the `_*_base` variants through the executable's
//! imports from `api-ms-win-crt-heap-l1-1-0.dll` (Rust's MSVC target links the C runtime as a DLL, and pyke's libraries
//! match it). [`install`] walks the import tables of the executable and of the C++ runtime DLLs ONNX Runtime uses
//! (`msvcp140.dll`, `msvcp140_1.dll`: their own allocations, for `std::` internals), matches those entries by name in any
//! imported DLL, and points each at a hook that counts and forwards to the original. It hooks `HeapAlloc`, `HeapReAlloc`
//! and `HeapFree` imports too, to count direct callers (Rust's among them).
//!
//! **The catch-all** ([`HeapCounts::ucrt_heap_allocs`] and the rest): `ucrtbase.dll`'s own imports of `HeapAlloc`,
//! `HeapReAlloc` and `HeapFree`, which every C runtime heap call reaches whoever made it (a caller outside the patched
//! import tables too). It is the probe's independent self-check: on a run the two counts must agree, and where they differ
//! the catch-all is the figure (a session's creation: some hundreds of allocations more, from callers outside the patched
//! tables). It stands in for the WPR/ETW cross-check until the owner runs that.
//!
//! **Counting** is per thread, in const-initialised thread-locals (no allocation, no lock), and only while that thread's
//! flag is on ([`counting`]). So a run's allocations are counted on the thread that runs it, while other threads (other
//! tests, ONNX Runtime's own) go uncounted.
//!
//! **Self-check:** building a session must show thousands of C runtime allocations on the building thread. None means the
//! probe is blind (a static C runtime or mimalloc inside the prebuilt library): the caller says so, and leaves WPR/ETW to
//! the owner, on a desktop with admin rights, as the cross-check.
//!
//! The patch is process-wide and permanent: an entry is swapped atomically (a pointer-sized aligned write), so a call
//! either takes the original or the hook, which forwards to it; a block freed through either path is freed by the original.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GetProcessHeap, HeapLock, HeapUnlock, HeapWalk, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, PROCESS_HEAP_ENTRY, VirtualProtect,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemServices::PROCESS_HEAP_ENTRY_BUSY;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
use windows::core::w;

/// One thread's counts while its flag was on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapCounts {
    /// C runtime calls: allocations (malloc, calloc, the aligned and base variants), frees, reallocations, and the bytes
    /// asked for by allocations and reallocations.
    pub crt_allocs: u64,
    pub crt_frees: u64,
    pub crt_reallocs: u64,
    pub crt_bytes: u64,
    /// Direct `HeapAlloc`, `HeapFree` and `HeapReAlloc` calls through a patched import (Rust's `System` allocator's).
    pub heap_allocs: u64,
    pub heap_frees: u64,
    pub heap_reallocs: u64,
    pub heap_bytes: u64,
    /// The catch-all (module notes): the C runtime's own `HeapAlloc`, `HeapFree` and `HeapReAlloc` calls, whoever called it.
    pub ucrt_heap_allocs: u64,
    pub ucrt_heap_frees: u64,
    pub ucrt_heap_reallocs: u64,
    pub ucrt_heap_bytes: u64,
}

impl HeapCounts {
    pub const ZERO: Self = Self {
        crt_allocs: 0,
        crt_frees: 0,
        crt_reallocs: 0,
        crt_bytes: 0,
        heap_allocs: 0,
        heap_frees: 0,
        heap_reallocs: 0,
        heap_bytes: 0,
        ucrt_heap_allocs: 0,
        ucrt_heap_frees: 0,
        ucrt_heap_reallocs: 0,
        ucrt_heap_bytes: 0,
    };

    /// C runtime heap calls of any kind.
    pub fn crt_calls(&self) -> u64 {
        self.crt_allocs + self.crt_frees + self.crt_reallocs
    }

    /// Direct heap calls of any kind.
    pub fn heap_calls(&self) -> u64 {
        self.heap_allocs + self.heap_frees + self.heap_reallocs
    }

    /// The C runtime's own heap calls of any kind (the catch-all).
    pub fn ucrt_heap_calls(&self) -> u64 {
        self.ucrt_heap_allocs + self.ucrt_heap_frees + self.ucrt_heap_reallocs
    }

    /// These counts less `earlier`'s.
    pub fn since(&self, earlier: &HeapCounts) -> HeapCounts {
        HeapCounts {
            crt_allocs: self.crt_allocs.saturating_sub(earlier.crt_allocs),
            crt_frees: self.crt_frees.saturating_sub(earlier.crt_frees),
            crt_reallocs: self.crt_reallocs.saturating_sub(earlier.crt_reallocs),
            crt_bytes: self.crt_bytes.saturating_sub(earlier.crt_bytes),
            heap_allocs: self.heap_allocs.saturating_sub(earlier.heap_allocs),
            heap_frees: self.heap_frees.saturating_sub(earlier.heap_frees),
            heap_reallocs: self.heap_reallocs.saturating_sub(earlier.heap_reallocs),
            heap_bytes: self.heap_bytes.saturating_sub(earlier.heap_bytes),
            ucrt_heap_allocs: self.ucrt_heap_allocs.saturating_sub(earlier.ucrt_heap_allocs),
            ucrt_heap_frees: self.ucrt_heap_frees.saturating_sub(earlier.ucrt_heap_frees),
            ucrt_heap_reallocs: self.ucrt_heap_reallocs.saturating_sub(earlier.ucrt_heap_reallocs),
            ucrt_heap_bytes: self.ucrt_heap_bytes.saturating_sub(earlier.ucrt_heap_bytes),
        }
    }
}

thread_local! {
    // Const-initialised and without destructors: reading them never allocates, from any thread at any time.
    static ON: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<HeapCounts> = const { Cell::new(HeapCounts::ZERO) };
}

#[inline(always)]
fn note(f: impl FnOnce(&mut HeapCounts)) {
    let _ = ON.try_with(|on| {
        if on.get() {
            let _ = COUNTS.try_with(|c| {
                let mut v = c.get();
                f(&mut v);
                c.set(v);
            });
        }
    });
}

/// This thread's counts so far (only what was counted while its flag was on).
pub fn counts() -> HeapCounts {
    COUNTS.try_with(Cell::get).unwrap_or_default()
}

/// Turns this thread's counting on or off; returns what it was.
pub fn set_counting(on: bool) -> bool {
    ON.try_with(|c| c.replace(on)).unwrap_or(false)
}

/// Runs `f` with this thread's counting on, and returns what it counted.
pub fn counting<R>(f: impl FnOnce() -> R) -> (R, HeapCounts) {
    let before = counts();
    let was = set_counting(true);
    let r = f();
    set_counting(was);
    (r, counts().since(&before))
}

// ---- The hooks ----------------------------------------------------------------------------------

/// The hooked functions, by name; the last three are the catch-all's (ucrtbase's own imports of the heap functions).
const NAMES: [&str; 23] = [
    "malloc", "calloc", "realloc", "free", "_aligned_malloc", "_aligned_free", "_aligned_realloc", "_aligned_recalloc",
    "_aligned_offset_malloc", "_aligned_offset_realloc", "_aligned_offset_recalloc", "_recalloc", "_malloc_base", "_calloc_base",
    "_realloc_base", "_free_base", "_recalloc_base", "HeapAlloc", "HeapReAlloc", "HeapFree", "HeapAlloc", "HeapReAlloc", "HeapFree",
];
/// The hooks (`NAMES` indexes) the executable's and the C++ runtime's import tables take, and ucrtbase's.
const CALLER_HOOKS: std::ops::Range<usize> = 0..20;
const UCRT_HOOKS: std::ops::Range<usize> = 20..23;

/// Each name's original, once seen.
static ORIG: [AtomicUsize; NAMES.len()] = [const { AtomicUsize::new(0) }; NAMES.len()];

/// Calls name `$i`'s original, of signature `$t`, with the hook's arguments.
macro_rules! call {
    ($i:expr, $t:ty, $($a:expr),*) => {
        // SAFETY: slot `$i` holds the original of the function whose signature `$t` is, stored before any import was
        // pointed at this hook, and the arguments are the hook's own, passed through unchanged.
        unsafe { std::mem::transmute::<usize, $t>(ORIG[$i].load(Relaxed))($($a),*) }
    };
}

type Size = usize;
type P = *mut c_void;

unsafe extern "C" fn h_malloc(n: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(0, unsafe extern "C" fn(Size) -> P, n)
}
unsafe extern "C" fn h_calloc(k: Size, n: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(1, unsafe extern "C" fn(Size, Size) -> P, k, n)
}
unsafe extern "C" fn h_realloc(p: P, n: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(2, unsafe extern "C" fn(P, Size) -> P, p, n)
}
unsafe extern "C" fn h_free(p: P) {
    if !p.is_null() {
        note(|c| c.crt_frees += 1);
    }
    call!(3, unsafe extern "C" fn(P), p)
}
unsafe extern "C" fn h_aligned_malloc(n: Size, a: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(4, unsafe extern "C" fn(Size, Size) -> P, n, a)
}
unsafe extern "C" fn h_aligned_free(p: P) {
    if !p.is_null() {
        note(|c| c.crt_frees += 1);
    }
    call!(5, unsafe extern "C" fn(P), p)
}
unsafe extern "C" fn h_aligned_realloc(p: P, n: Size, a: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(6, unsafe extern "C" fn(P, Size, Size) -> P, p, n, a)
}
unsafe extern "C" fn h_aligned_recalloc(p: P, k: Size, n: Size, a: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(7, unsafe extern "C" fn(P, Size, Size, Size) -> P, p, k, n, a)
}
unsafe extern "C" fn h_aligned_offset_malloc(n: Size, a: Size, o: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(8, unsafe extern "C" fn(Size, Size, Size) -> P, n, a, o)
}
unsafe extern "C" fn h_aligned_offset_realloc(p: P, n: Size, a: Size, o: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(9, unsafe extern "C" fn(P, Size, Size, Size) -> P, p, n, a, o)
}
unsafe extern "C" fn h_aligned_offset_recalloc(p: P, k: Size, n: Size, a: Size, o: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(10, unsafe extern "C" fn(P, Size, Size, Size, Size) -> P, p, k, n, a, o)
}
unsafe extern "C" fn h_recalloc(p: P, k: Size, n: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(11, unsafe extern "C" fn(P, Size, Size) -> P, p, k, n)
}
unsafe extern "C" fn h_malloc_base(n: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(12, unsafe extern "C" fn(Size) -> P, n)
}
unsafe extern "C" fn h_calloc_base(k: Size, n: Size) -> P {
    note(|c| (c.crt_allocs, c.crt_bytes) = (c.crt_allocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(13, unsafe extern "C" fn(Size, Size) -> P, k, n)
}
unsafe extern "C" fn h_realloc_base(p: P, n: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(n as u64)));
    call!(14, unsafe extern "C" fn(P, Size) -> P, p, n)
}
unsafe extern "C" fn h_free_base(p: P) {
    if !p.is_null() {
        note(|c| c.crt_frees += 1);
    }
    call!(15, unsafe extern "C" fn(P), p)
}
unsafe extern "C" fn h_recalloc_base(p: P, k: Size, n: Size) -> P {
    note(|c| (c.crt_reallocs, c.crt_bytes) = (c.crt_reallocs + 1, c.crt_bytes.saturating_add(k.saturating_mul(n) as u64)));
    call!(16, unsafe extern "C" fn(P, Size, Size) -> P, p, k, n)
}
unsafe extern "system" fn h_heap_alloc(h: HANDLE, f: u32, n: Size) -> P {
    note(|c| (c.heap_allocs, c.heap_bytes) = (c.heap_allocs + 1, c.heap_bytes.saturating_add(n as u64)));
    call!(17, unsafe extern "system" fn(HANDLE, u32, Size) -> P, h, f, n)
}
unsafe extern "system" fn h_heap_realloc(h: HANDLE, f: u32, p: P, n: Size) -> P {
    note(|c| (c.heap_reallocs, c.heap_bytes) = (c.heap_reallocs + 1, c.heap_bytes.saturating_add(n as u64)));
    call!(18, unsafe extern "system" fn(HANDLE, u32, P, Size) -> P, h, f, p, n)
}
unsafe extern "system" fn h_heap_free(h: HANDLE, f: u32, p: P) -> i32 {
    if !p.is_null() {
        note(|c| c.heap_frees += 1);
    }
    call!(19, unsafe extern "system" fn(HANDLE, u32, P) -> i32, h, f, p)
}
unsafe extern "system" fn h_ucrt_heap_alloc(h: HANDLE, f: u32, n: Size) -> P {
    note(|c| (c.ucrt_heap_allocs, c.ucrt_heap_bytes) = (c.ucrt_heap_allocs + 1, c.ucrt_heap_bytes.saturating_add(n as u64)));
    call!(20, unsafe extern "system" fn(HANDLE, u32, Size) -> P, h, f, n)
}
unsafe extern "system" fn h_ucrt_heap_realloc(h: HANDLE, f: u32, p: P, n: Size) -> P {
    note(|c| (c.ucrt_heap_reallocs, c.ucrt_heap_bytes) = (c.ucrt_heap_reallocs + 1, c.ucrt_heap_bytes.saturating_add(n as u64)));
    call!(21, unsafe extern "system" fn(HANDLE, u32, P, Size) -> P, h, f, p, n)
}
unsafe extern "system" fn h_ucrt_heap_free(h: HANDLE, f: u32, p: P) -> i32 {
    if !p.is_null() {
        note(|c| c.ucrt_heap_frees += 1);
    }
    call!(22, unsafe extern "system" fn(HANDLE, u32, P) -> i32, h, f, p)
}

/// The hook for name `k`.
fn hook(k: usize) -> usize {
    match k {
        0 => h_malloc as *const () as usize,
        1 => h_calloc as *const () as usize,
        2 => h_realloc as *const () as usize,
        3 => h_free as *const () as usize,
        4 => h_aligned_malloc as *const () as usize,
        5 => h_aligned_free as *const () as usize,
        6 => h_aligned_realloc as *const () as usize,
        7 => h_aligned_recalloc as *const () as usize,
        8 => h_aligned_offset_malloc as *const () as usize,
        9 => h_aligned_offset_realloc as *const () as usize,
        10 => h_aligned_offset_recalloc as *const () as usize,
        11 => h_recalloc as *const () as usize,
        12 => h_malloc_base as *const () as usize,
        13 => h_calloc_base as *const () as usize,
        14 => h_realloc_base as *const () as usize,
        15 => h_free_base as *const () as usize,
        16 => h_recalloc_base as *const () as usize,
        17 => h_heap_alloc as *const () as usize,
        18 => h_heap_realloc as *const () as usize,
        19 => h_heap_free as *const () as usize,
        20 => h_ucrt_heap_alloc as *const () as usize,
        21 => h_ucrt_heap_realloc as *const () as usize,
        _ => h_ucrt_heap_free as *const () as usize,
    }
}

// ---- Installing ---------------------------------------------------------------------------------

/// What [`install`] patched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeStatus {
    /// `module: dll!name` for each import pointed at a hook.
    pub patched: Vec<String>,
    /// Imports left alone: their original differs from the same name's first (a second implementation).
    pub skipped: Vec<String>,
}

impl ProbeStatus {
    /// Whether the C runtime's `malloc` and `free` (as ONNX Runtime calls them) are hooked.
    pub fn sees_the_crt(&self) -> bool {
        ["!malloc", "!free"].iter().all(|n| self.patched.iter().any(|p| p.ends_with(n)))
    }

    /// Whether the catch-all is in: ucrtbase's own `HeapAlloc` and `HeapFree` imports.
    pub fn has_catch_all(&self) -> bool {
        ["!HeapAlloc", "!HeapFree"].iter().all(|n| self.patched.iter().any(|p| p.starts_with("ucrtbase:") && p.ends_with(n)))
    }
}

static STATUS: Mutex<Option<ProbeStatus>> = Mutex::new(None);
/// [`install`] ran and hooked the C runtime's `malloc` and `free`: readable from any thread without a lock or allocation.
static SEES_THE_CRT: AtomicBool = AtomicBool::new(false);

/// Whether the probe is installed and sees the C runtime (lock-free; for a worker's counters).
pub fn installed() -> bool {
    SEES_THE_CRT.load(Relaxed)
}

/// Patches the executable's and the C++ runtime's imports (module notes), once; later calls return the first's status.
pub fn install() -> Result<ProbeStatus, String> {
    let mut status = STATUS.lock().map_err(|_| "the probe's lock is poisoned".to_owned())?;
    if let Some(s) = status.as_ref() {
        return Ok(s.clone());
    }
    let mut s = ProbeStatus::default();
    // SAFETY: GetModuleHandleW returns a loaded module's base (it isn't unloaded: the exe, and DLLs it imports from).
    let modules = unsafe {
        [
            ("exe", GetModuleHandleW(None), CALLER_HOOKS),
            ("msvcp140", GetModuleHandleW(w!("msvcp140.dll")), CALLER_HOOKS),
            ("msvcp140_1", GetModuleHandleW(w!("msvcp140_1.dll")), CALLER_HOOKS),
            ("ucrtbase", GetModuleHandleW(w!("ucrtbase.dll")), UCRT_HOOKS),
        ]
    };
    for (name, m, hooks) in modules {
        if let Ok(m) = m {
            // SAFETY: `m` is a mapped PE image (above); `patch_module` reads its headers within the image.
            unsafe { patch_module(m.0 as *const u8, name, hooks, &mut s)? };
        }
    }
    SEES_THE_CRT.store(s.sees_the_crt(), Relaxed);
    *status = Some(s.clone());
    Ok(s)
}

/// The installed probe's status, if [`install`] ran.
pub fn status() -> Option<ProbeStatus> {
    STATUS.lock().ok().and_then(|s| s.clone())
}

/// Reads a `T` at `base + off`, unaligned.
///
/// # Safety
/// `base + off .. + size_of::<T>()` must be readable.
unsafe fn at<T: Copy>(base: *const u8, off: usize) -> T {
    unsafe { base.add(off).cast::<T>().read_unaligned() }
}

/// The NUL-terminated ASCII string at `p` (at most 256 bytes).
///
/// # Safety
/// `p` must point to a NUL-terminated string within mapped memory.
unsafe fn cstr<'a>(p: *const u8) -> &'a [u8] {
    let mut n = 0;
    while n < 256 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    unsafe { std::slice::from_raw_parts(p, n) }
}

/// Points the imports of the PE32+ image at `base` that `hooks` (indexes into `NAMES`) name at those hooks.
///
/// # Safety
/// `base` must be a loaded image's base address.
unsafe fn patch_module(base: *const u8, module: &str, hooks: std::ops::Range<usize>, s: &mut ProbeStatus) -> Result<(), String> {
    unsafe {
        if at::<u16>(base, 0) != 0x5A4D {
            return Err(format!("{module}: no MZ header"));
        }
        let nt = at::<u32>(base, 0x3C) as usize;
        if at::<u32>(base, nt) != 0x0000_4550 || at::<u16>(base, nt + 24) != 0x20B {
            return Err(format!("{module}: not a PE32+ image"));
        }
        // The optional header's data directories start 112 bytes in; the import directory is entry 1.
        let imports = at::<u32>(base, nt + 24 + 112 + 8) as usize;
        if imports == 0 {
            return Ok(());
        }
        let mut desc = imports;
        loop {
            let (int, name, iat) =
                (at::<u32>(base, desc) as usize, at::<u32>(base, desc + 12) as usize, at::<u32>(base, desc + 16) as usize);
            if name == 0 && iat == 0 {
                break;
            }
            let dll = String::from_utf8_lossy(cstr(base.add(name))).to_ascii_lowercase();
            if int != 0 {
                let mut i = 0usize;
                loop {
                    let thunk = at::<u64>(base, int + 8 * i);
                    if thunk == 0 {
                        break;
                    }
                    if thunk & (1 << 63) == 0 {
                        let f = cstr(base.add(thunk as u32 as usize + 2));
                        if let Some(k) = hooks.clone().find(|&k| NAMES[k].as_bytes() == f) {
                            let slot = base.add(iat + 8 * i) as *mut usize;
                            patch(slot, k, &format!("{module}: {dll}!{}", NAMES[k]), s)?;
                        }
                    }
                    i += 1;
                }
            }
            desc += 20;
        }
    }
    Ok(())
}

/// Points one import slot at hook `k`.
///
/// # Safety
/// `slot` must be an import address table entry of a loaded image.
unsafe fn patch(slot: *mut usize, k: usize, what: &str, s: &mut ProbeStatus) -> Result<(), String> {
    let cur = unsafe { slot.read_volatile() };
    let h = hook(k);
    if cur == h {
        return Ok(());
    }
    if let Err(first) = ORIG[k].compare_exchange(0, cur, Relaxed, Relaxed)
        && first != cur
    {
        s.skipped.push(what.to_owned());
        return Ok(());
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the slot is 8 bytes inside the image; making it writable and back changes nothing else's meaning.
    unsafe {
        VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut old).map_err(|e| format!("{what}: VirtualProtect: {e}"))?;
        // A pointer-sized aligned store: atomic on x86-64, so a concurrent call takes either function.
        (*(slot as *const AtomicUsize)).store(h, Relaxed);
        let mut back = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot as *const c_void, 8, old, &mut back);
    }
    s.patched.push(what.to_owned());
    Ok(())
}

// ---- Process-wide figures -----------------------------------------------------------------------

/// The process's page faults (soft and hard) and private bytes.
pub fn process_memory() -> (u32, usize) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: the struct is the size it says; the pseudo-handle needs no closing.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) };
    if ok.as_bool() { (c.PageFaultCount, c.PrivateUsage) } else { (0, 0) }
}

/// The process heap's busy blocks and their bytes (`HeapWalk`, the heap locked meanwhile): net figures, for leaks.
pub fn live_heap_blocks() -> (u64, u64) {
    // SAFETY: the process heap lives as long as the process; it is locked for the walk and unlocked after; the entry
    // starts zeroed, as HeapWalk's first call wants.
    unsafe {
        let Ok(heap) = GetProcessHeap() else { return (0, 0) };
        if HeapLock(heap).is_err() {
            return (0, 0);
        }
        let mut e = PROCESS_HEAP_ENTRY::default();
        let (mut n, mut bytes) = (0u64, 0u64);
        while HeapWalk(heap, &mut e).is_ok() {
            if u32::from(e.wFlags) & PROCESS_HEAP_ENTRY_BUSY != 0 {
                n += 1;
                bytes += u64::from(e.cbData);
            }
        }
        let _ = HeapUnlock(heap);
        (n, bytes)
    }
}

/// The calling thread's CPU cycles so far.
pub fn thread_cycles() -> u64 {
    let mut c = 0u64;
    // SAFETY: the pseudo-handle is this thread; `c` is a valid out-pointer.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}
