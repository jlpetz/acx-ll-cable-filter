# windows 0.62.2: Win32 system APIs for noise-filter

MMCSS, QPC, working set, large pages and locked memory, token privileges, the LSA grant, the elevated
child, thread priority and naming, FTZ/DAZ, and what to take from TMR.

Written 2026-10-26 from the crate sources. Nothing was compiled or run, so no snippet here has been
through `cargo check`. Facts marked **(MS docs)** come from Microsoft's documentation and are not in
any source read for this file. Everything else has a file:line.

Path prefixes (registry root `R` = `C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\`):

- `W/` = `R\windows-0.62.2\src\Windows\Win32\`
- `ext/` = `R\windows-0.62.2\src\extensions\Win32\`
- `core/` = `R\windows-core-0.62.2\src\`, `result/` = `R\windows-result-0.4.1\src\`, `strings/` = `R\windows-strings-0.5.1\src\`
- `stdarch/`, `std/` = `C:\Users\Administrator\.rustup\toolchains\nightly-x86_64-pc-windows-msvc\lib\rustlib\src\rust\library\{stdarch\crates\core_arch\src,std\src}\` (rustc 1.101.0-nightly 1a2c4797e 2026-10-21)
- `TMR/` = `C:\Users\Administrator\Downloads\TMR\TMR-APP\`, `NF/` = `C:\Users\Administrator\Downloads\noise-filter\`

**Versions.** `Cargo.lock` resolves `windows` 0.62.2 (`NF/Cargo.lock:636-637`), the same version
wasapi 0.26.0 uses (`R\wasapi-0.26.0\Cargo.toml:124-125`), so `HANDLE`, `HRESULT` and the rest are the
same types in both. windows-reactor 0.100.0 depends on windows-core **0.100.0**
(`R\windows-reactor-0.100.0\Cargo.toml:56-57`), which is a different crate. Its `HSTRING`, `Error`
and `HRESULT` can't be mixed with `windows::core::*` from 0.62.2.

---

## 1. Cargo features and module paths

| API | Rust path | Features | Source |
|---|---|---|---|
| `AvSetMmThreadCharacteristicsW`, `AvSetMmThreadPriority`, `AvRevertMmThreadCharacteristics`, `AVRT_PRIORITY_*` | `windows::Win32::System::Threading` | `Win32_System_Threading` | `W/System/Threading/mod.rs:34,109,118,2014-2019` |
| `SetThreadPriority`, `THREAD_PRIORITY_*`, `SetThreadDescription`, `GetCurrentThread`, `GetCurrentProcess`, `WaitForSingleObject`, `GetExitCodeProcess`, `INFINITE` | `windows::Win32::System::Threading` | `Win32_System_Threading` | `:1706,3452-3460,1676,704,679,1929,724,2145` |
| `OpenProcessToken` | `windows::Win32::System::Threading` | `Win32_System_Threading` **and** `Win32_Security` (`#[cfg]` at `:1197`) | `:1199` |
| `QueryPerformanceCounter/Frequency` | `windows::Win32::System::Performance` | `Win32_System_Performance` | `W/System/Performance/mod.rs:866,871` |
| `SetProcessWorkingSetSizeEx`, `GetProcessWorkingSetSizeEx`, `QUOTA_LIMITS_*` | `windows::Win32::System::Memory` | `Win32_System_Memory` | `W/System/Memory/mod.rs:471,126,1128-1131` |
| `VirtualAlloc`, `VirtualAlloc2`, `VirtualFree`, `VirtualLock`, `VirtualUnlock`, `GetLargePageMinimum`, `MEM_*`, `PAGE_READWRITE`, `MEM_EXTENDED_PARAMETER` | `windows::Win32::System::Memory` | `Win32_System_Memory` | `:501,506,531,541,571,105` |
| `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`, `MEM_EXTENDED_PARAMETER_TYPE_BITS` | `windows::Win32::System::SystemServices` | `Win32_System_SystemServices` (or hard-code 8 and 8) | `W/System/SystemServices/mod.rs:2830,2832` |
| `LookupPrivilegeValueW`, `AdjustTokenPrivileges`, `GetTokenInformation`, `IsValidSid`, `TOKEN_*`, `Token*` classes, `PSID`, `SE_LOCK_MEMORY_NAME` | `windows::Win32::Security` | `Win32_Security` | `W/Security/mod.rs:641,207,454,514` |
| `ConvertSidToStringSidW`, `ConvertStringSidToSidW` | `windows::Win32::Security::Authorization` | `Win32_Security_Authorization` | `W/Security/Authorization/mod.rs:339,368` |
| `LsaOpenPolicy`, `LsaAddAccountRights`, `LsaClose`, `LsaNtStatusToWinError`, `LSA_*`, `POLICY_*` | `windows::Win32::Security::Authentication::Identity` | `Win32_Security_Authentication_Identity` | `W/Security/Authentication/Identity/mod.rs:472,357,367,467` |
| `ShellExecuteExW`, `SHELLEXECUTEINFOW`, `SEE_MASK_*` | `windows::Win32::UI::Shell` | `Win32_UI_Shell` **and** `Win32_System_Registry` (`#[cfg]` at `W/UI/Shell/mod.rs:4360`) | `:4362` |
| `SW_HIDE`, `SW_SHOWNORMAL` | `windows::Win32::UI::WindowsAndMessaging` | `Win32_UI_WindowsAndMessaging` (or write the `i32` literal) | `W/UI/WindowsAndMessaging/mod.rs:6409,6428` |
| `CoInitializeEx`, `COINIT_*` | `windows::Win32::System::Com` | `Win32_System_Com` | `W/System/Com/mod.rs:334,1802-1804` |
| `CloseHandle`, `GetLastError`, `LocalFree`, `HANDLE`, `HLOCAL`, `HWND`, `LUID`, `NTSTATUS`, `WIN32_ERROR`, `ERROR_*`, `WAIT_*` | `windows::Win32::Foundation` | `Win32_Foundation` | `W/Foundation/mod.rs:2,27,38,5291,5488,5670,5814,6093,10548` |
| `Owned`, `Free`, `HSTRING`, `PCWSTR`, `PWSTR`, `w!`, `Error`, `Result`, `HRESULT`, `BOOL` | `windows::core` (= windows-core 0.62.2) | none | `core/handles.rs:5,16`, `core/windows.rs:12-15` |

Feature chains (`R\windows-0.62.2\Cargo.toml`): `Win32_Security_Authentication_Identity` turns on
`Win32_Security_Authentication` and then `Win32_Security` (`:512,511,509`). Every `Win32_System_*`
feature turns on `Win32_System` and `Win32` (`:561,376`). Default features are `["std"]` (`:711`).

**What our crates declare today, and the gaps:**

- `nf-engine` (`NF/crates/nf-engine/Cargo.toml`): Foundation, Media_Audio, System_Com,
  System_Threading, System_Performance. That covers MMCSS, QPC, `SetThreadPriority`,
  `SetThreadDescription` and power throttling. It has no `Win32_Security`, so no `OpenProcessToken`.
- `nf-mem` (`NF/crates/nf-mem/Cargo.toml`): Foundation, Security, System_Memory, System_Threading,
  System_SystemInformation, Security_Authentication_Identity. That covers VirtualAlloc2/Lock, the
  working set, the token and LSA. **Missing:** `Win32_Security_Authorization` (SID strings) and
  `Win32_System_SystemServices` (the two extended-parameter constants, or hard-code them). The
  elevated launcher, wherever it lives, also needs `Win32_UI_Shell` + `Win32_System_Registry` +
  `Win32_System_Com`.
- Features unify across a workspace build, but `cargo check -p nf-mem` compiles only with that
  crate's own list, so each crate must declare what it uses.

---

## 2. How 0.62.2 wraps the API (read this once)

- **Every function is `pub unsafe fn`.** That includes `GetLastError` (`W/Foundation/mod.rs:27`)
  and `GetCurrentProcess` (`W/System/Threading/mod.rs:679`).
- **Return shapes:**
  - When the C API returns `BOOL`, the wrapper usually returns `windows::core::Result<()>` through
    `BOOL::ok()` (`result/bool.rs:18-24`). On failure that is `Error::from_thread()` =
    `HRESULT_FROM_WIN32(GetLastError())` (`result/error.rs:118-120`, `result/hresult.rs:116-135`).
    The failure path doesn't allocate and makes no COM call.
  - Raw `BOOL` is kept for `GetProcessWorkingSetSizeEx` (`W/System/Memory/mod.rs:126`) and
    `IsValidSid` (`W/Security/mod.rs:514`). Call `.ok()` or `.as_bool()` on it.
  - `VirtualAlloc`/`VirtualAlloc2` return `*mut c_void`: null means failure, and you read
    `GetLastError` yourself.
  - `HANDLE`-returning calls return `Result<HANDLE>` and check `is_invalid()`
    (e.g. `W/System/Threading/mod.rs:109-116`).
  - HRESULT-returning calls (e.g. `SetThreadDescription`) use `.ok()`. On failure,
    `From<HRESULT> for Error` calls `GetErrorInfo` in oleaut32 (`result/error.rs:159-166,274-283`).
    That is a COM call, on the error path only.
  - `Lsa*` return a raw `NTSTATUS`. Use `is_ok()`, `is_err()` and `ok()` from
    `ext/Foundation/NTSTATUS.rs:3-33` (`ok()` goes the same HRESULT/`GetErrorInfo` way).
- **Getting the Win32 code back:** `WIN32_ERROR::from_error(&e) -> Option<WIN32_ERROR>`
  (`ext/Foundation/WIN32_ERROR.rs:17-24`), or compare `e.code() == HRESULT::from_win32(1314)`.
  `Error::message()`, `Display` and `Debug` all call `FormatMessageW` and allocate a `String`
  (`result/error.rs:131-138,203-222`, `result/hresult.rs:65-90`). So TMR's `format_win32_error` and
  the `Win32_System_Diagnostics_Debug` feature aren't needed.
- **String parameters** (`P: Param<PCWSTR>`) accept `PCWSTR` (including `w!("..")`), `PWSTR`,
  `&HSTRING`, `&BSTR` or `None` (`core/windows.rs:23-37`, `core/param.rs:16-28,67-76`). `w!` builds
  a static NUL-terminated UTF-16 literal at compile time (`strings/literals.rs:11`).
- **`HSTRING::from(&str | &Path | &OsStr)` allocates with `HeapAlloc(GetProcessHeap())`**
  (`strings/hstring_header.rs:26-27`), not through `#[global_allocator]`. A counting test allocator
  won't see it, so use `w!` on the audio thread. HSTRING is `Send + Sync` (`strings/hstring.rs:110-111`)
  and always NUL-terminated, even when empty (`strings/hstring.rs:52-76`).
- **RAII:** `windows::core::Owned<T: Free>` (`core/handles.rs:16-33`). The `Free` impls are:
  - `HANDLE` → `CloseHandle` (`W/Foundation/mod.rs:5297-5307`);
  - `HLOCAL` → `LocalFree` (`:5494`);
  - `LSA_HANDLE` → `LsaClose` (`W/Security/Authentication/Identity/mod.rs:3280-3289`).

  `HANDLE::is_invalid()` is `== 0 || == -1` (`W/Foundation/mod.rs:5293-5295`). **It returns true
  for `GetCurrentProcess()`'s valid pseudo-handle (-1).**
- **Send/Sync:** `HANDLE`, `HWND`, `PSID`, `PWSTR`, `PCWSTR`, `MEM_EXTENDED_PARAMETER`,
  `SHELLEXECUTEINFOW` and `LSA_UNICODE_STRING` hold raw pointers and have no `Send`/`Sync` impls
  (there is no `impl Send for HANDLE` anywhere in `windows-0.62.2/src`). `LSA_HANDLE(pub isize)` is
  `Send + Sync` automatically. Only add `unsafe impl Send` on a newtype where moving across threads
  is legal: a process handle, yes; an MMCSS handle, no.
- **Linking:** `windows_link::link!` emits `#[link(kind = "raw-dylib")]`
  (`R\windows-link-0.2.1\src\lib.rs:9,22`). No import libraries are needed. avrt.dll, advapi32.dll
  and shell32.dll become load-time imports.

---

## 3. MMCSS "Pro Audio"

```rust
// W/System/Threading/mod.rs
pub unsafe fn AvSetMmThreadCharacteristicsW<P0>(taskname: P0, taskindex: *mut u32) -> windows_core::Result<HANDLE>
where P0: windows_core::Param<windows_core::PCWSTR>;                                         // :109
pub unsafe fn AvSetMmThreadPriority(avrthandle: HANDLE, priority: AVRT_PRIORITY) -> windows_core::Result<()>; // :118
pub unsafe fn AvRevertMmThreadCharacteristics(avrthandle: HANDLE) -> windows_core::Result<()>; // :34
pub unsafe fn AvQuerySystemResponsiveness(avrthandle: HANDLE, systemresponsivenessvalue: *mut u32) -> windows_core::Result<()>; // :29
pub struct AVRT_PRIORITY(pub i32);                                                          // :2014
// AVRT_PRIORITY_VERYLOW -2, _LOW -1, _NORMAL 0, _HIGH 1, _CRITICAL 2                        // :2015-2019
```

Failure is an `Err` built from `GetLastError`, e.g. `ERROR_INVALID_TASK_NAME` 1550 or
`ERROR_INVALID_TASK_INDEX` 1551 (`W/Foundation/mod.rs:2987,2986`).

**Rules:**
- It registers the *calling* thread, so call it on the audio thread itself **(MS docs)**.
- `*taskindex` must be 0 on the first call **(MS docs)**. The parameter isn't an `Option`, so pass
  `&mut idx`.
- The returned value is an AVRT handle, not a kernel object. Release it with
  `AvRevertMmThreadCharacteristics` on the same thread before the thread exits **(MS docs)**.
  **Never `CloseHandle` it or wrap it in `Owned<HANDLE>`**, whose `Free` calls `CloseHandle`.
- wasapi 0.26.0 does no MMCSS registration (there's no `AvSetMm` in `R\wasapi-0.26.0\src`).

```rust
use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread,
    SetThreadPriority, AVRT_PRIORITY_CRITICAL, THREAD_PRIORITY_TIME_CRITICAL,
};

/// How the audio thread is scheduled. `!Send` (it holds a HANDLE), so it stays on the thread that made it.
pub enum RtSched { Mmcss { handle: HANDLE, task_index: u32 }, TimeCritical, Normal }

impl RtSched {
    /// First thing on the audio thread. Not on the hot path: the error branch may build an Error.
    pub fn enter() -> Self {
        let mut task_index = 0u32;
        // SAFETY: w! is a 'static NUL-terminated literal; task_index is a live local that starts at 0.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) => {
                // SAFETY: handle was just returned for this thread. CRITICAL is our choice (one RT thread).
                let _ = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) };
                RtSched::Mmcss { handle, task_index }
            }
            Err(_e) => {
                // Keep WIN32_ERROR::from_error(&_e) for the UI; don't format it here.
                // SAFETY: GetCurrentThread is a pseudo-handle that needs no closing.
                if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }.is_ok() {
                    RtSched::TimeCritical
                } else {
                    RtSched::Normal
                }
            }
        }
    }
}

impl Drop for RtSched {
    fn drop(&mut self) {
        if let RtSched::Mmcss { handle, .. } = self {
            // SAFETY: same thread (RtSched is !Send); the handle came from AvSetMmThreadCharacteristicsW.
            let _ = unsafe { AvRevertMmThreadCharacteristics(*handle) };
        }
    }
}
```

Order on the audio thread (our proposal): name it (via `std::thread::Builder`, §10), `RtSched::enter()`,
then set FTZ/DAZ (§11), then WASAPI COM init and the loop.

---

## 4. QueryPerformanceCounter and 100 ns units

```rust
// W/System/Performance/mod.rs
pub unsafe fn QueryPerformanceCounter(lpperformancecount: *mut i64) -> windows_core::Result<()>; // :866
pub unsafe fn QueryPerformanceFrequency(lpfrequency: *mut i64) -> windows_core::Result<()>;      // :871
```

- The frequency is fixed at boot **(MS docs)**, so read it once. std does the same
  (`std/sys/pal/windows/time.rs:33-61`). `std::time::Instant::now()` *is* QPC
  (`std/sys/pal/windows/time.rs:27-29`), so Instant and our readings share a clock, but Instant
  doesn't expose raw ticks.
- **WASAPI timestamps are already QPC in 100 ns units.** wasapi's `BufferInfo.timestamp` is
  documented "in 100-nanosecond units" (`R\wasapi-0.26.0\src\api.rs:2112-2113`) and filled by
  `IAudioCaptureClient::GetBuffer` (`api.rs:2215-2221`). Convert our ticks to 100 ns before
  subtracting.
- Cost: QPC normally doesn't enter the kernel **(MS docs, "Acquiring high-resolution time stamps")**,
  so once or twice per block on the audio thread is fine. The success path allocates nothing.
- The frequency is often 10 MHz on current Windows, which makes ticks equal to 100 ns, but don't
  rely on that.

```rust
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

#[derive(Clone, Copy, Debug)]
pub struct Qpc { freq: i64 }

impl Qpc {
    pub fn new() -> Self {
        let mut freq = 0i64;
        // SAFETY: valid out-pointer. Can't fail on XP or later (MS docs).
        unsafe { QueryPerformanceFrequency(&mut freq) }.expect("QueryPerformanceFrequency");
        Self { freq }
    }
    #[inline]
    pub fn ticks(&self) -> i64 {
        let mut t = 0i64;
        // SAFETY: valid out-pointer.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        t
    }
    /// Ticks to 100 ns without overflow: the remainder is < freq, so rem * 1e7 fits i64 for freq < 9.2e11.
    #[inline]
    pub fn to_hns(&self, ticks: i64) -> i64 {
        const HNS: i64 = 10_000_000;
        if self.freq == HNS { return ticks; }
        (ticks / self.freq) * HNS + (ticks % self.freq) * HNS / self.freq
    }
    #[inline]
    pub fn now_hns(&self) -> i64 { self.to_hns(self.ticks()) }
}
```

---

## 5. Working set: hard minimum

```rust
// W/System/Memory/mod.rs
pub unsafe fn SetProcessWorkingSetSizeEx(hprocess: HANDLE, dwminimumworkingsetsize: usize, dwmaximumworkingsetsize: usize,
    flags: SETPROCESSWORKINGSETSIZEEX_FLAGS) -> windows_core::Result<()>;                   // :471
pub unsafe fn GetProcessWorkingSetSizeEx(hprocess: HANDLE, lpminimumworkingsetsize: *mut usize,
    lpmaximumworkingsetsize: *mut usize, flags: *mut u32) -> windows_core::BOOL;            // :126  (raw BOOL)
pub struct SETPROCESSWORKINGSETSIZEEX_FLAGS(pub u32);                                       // :1188 (BitOr etc.)
// QUOTA_LIMITS_HARDWS_MIN_ENABLE 1, _MIN_DISABLE 2, _MAX_ENABLE 4, _MAX_DISABLE 8            // :1128-1131
```

- The `Get…Ex` `flags` output is a plain `u32` with the same bits.
- The non-Ex `SetProcessWorkingSetSize`/`GetProcessWorkingSetSize` live in `System::Threading`
  (`W/System/Threading/mod.rs:1661,906`), not `Memory`.
- `GetCurrentProcess()` (`:679`) returns a pseudo-handle with full access that needs no closing.
- `max` must be at least `min`. Raising the limits above their current values needs
  `SeIncreaseWorkingSetPrivilege`, which "all users generally have" **(MS docs)**.
  - The privilege may be present but disabled in the token. Whether it must be enabled is
    unverified here.
  - Enabling it with §7's `enable_privilege(SE_INC_WORKING_SET_NAME)` (`W/Security/mod.rs:1674`) is
    cheap insurance.
- `VirtualLock` is capped at the minimum working set minus a small overhead. Going past it fails
  with `ERROR_WORKING_SET_QUOTA` 1453 (`W/Foundation/mod.rs:4654`) **(MS docs)**. So raise the
  minimum first, then lock.
- Large pages are not part of the working set (`NF/doc/todo/6-inference-memory.md:21`). The minimum
  only has to cover the 4 KB locked ranges plus code and heaps.

```rust
use windows::Win32::System::Memory::{
    GetProcessWorkingSetSizeEx, SetProcessWorkingSetSizeEx, QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_ENABLE,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// Start-up only. Raises the hard minimum (and the soft maximum) by `extra` bytes; returns the new (min, max).
pub fn raise_working_set(extra: usize) -> windows::core::Result<(usize, usize)> {
    let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
    // SAFETY: pseudo-handle for this process; the out-pointers are live locals.
    unsafe {
        let me = GetCurrentProcess();
        GetProcessWorkingSetSizeEx(me, &mut min, &mut max, &mut flags).ok()?;
        let new_min = min.saturating_add(extra);
        let new_max = max.saturating_add(extra).max(new_min);
        SetProcessWorkingSetSizeEx(me, new_min, new_max, QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE)?;
        Ok((new_min, new_max))
    }
}
```

---

## 6. Large pages, VirtualLock, VirtualFree

```rust
// W/System/Memory/mod.rs
pub unsafe fn GetLargePageMinimum() -> usize;                                               // :105  (0 = unsupported, MS docs)
pub unsafe fn VirtualAlloc(lpaddress: Option<*const c_void>, dwsize: usize, flallocationtype: VIRTUAL_ALLOCATION_TYPE,
    flprotect: PAGE_PROTECTION_FLAGS) -> *mut c_void;                                         // :501
pub unsafe fn VirtualAlloc2(process: Option<HANDLE>, baseaddress: Option<*const c_void>, size: usize,
    allocationtype: VIRTUAL_ALLOCATION_TYPE, pageprotection: u32,                            // u32 here: pass PAGE_READWRITE.0
    extendedparameters: Option<&mut [MEM_EXTENDED_PARAMETER]>) -> *mut c_void;               // :506 (api-ms-win-core-memory-l1-1-6.dll)
pub unsafe fn VirtualFree(lpaddress: *mut c_void, dwsize: usize, dwfreetype: VIRTUAL_FREE_TYPE) -> windows_core::Result<()>; // :531
pub unsafe fn VirtualLock(lpaddress: *const c_void, dwsize: usize) -> windows_core::Result<()>;   // :541
pub unsafe fn VirtualUnlock(lpaddress: *const c_void, dwsize: usize) -> windows_core::Result<()>; // :571
```

**Constants:**
- `MEM_COMMIT` 0x1000 (`:895`), `MEM_RESERVE` 0x2000 (`:941`) and `MEM_LARGE_PAGES` 0x2000_0000
  (`:935`) are `VIRTUAL_ALLOCATION_TYPE` (`:1227`, has BitOr).
- `MEM_RELEASE` 0x8000 (`:939`) is `VIRTUAL_FREE_TYPE` (`:1263`).
- `PAGE_READWRITE` is `PAGE_PROTECTION_FLAGS(4)` (`:1034`).

**`MEM_EXTENDED_PARAMETER`** (`:902-924`, `Default` = zeroed):
- `{ Anonymous1: MEM_EXTENDED_PARAMETER_0 { _bitfield: u64 }, Anonymous2: MEM_EXTENDED_PARAMETER_1 }`,
  where `_1` is a union of `ULong64`, `Pointer`, `Size`, `Handle` and `ULong`.
- The type goes in the low `MEM_EXTENDED_PARAMETER_TYPE_BITS` = 8 bits
  (`W/System/SystemServices/mod.rs:2832`).
- Types: `MemExtendedParameterAddressRequirements` 1, `NumaNode` 2, `AttributeFlags` 5 (`:955-962`).
- Attribute values (u32): `MEM_EXTENDED_PARAMETER_NONPAGED` 2, `_NONPAGED_LARGE` 8,
  `_NONPAGED_HUGE` 16 (`W/System/SystemServices/mod.rs:2828-2830`).

**Refusal codes** (`W/Foundation/mod.rs`):

| Code | Constant | Meaning |
|---|---|---|
| 1314 | `ERROR_PRIVILEGE_NOT_HELD` (`:3748`) | SeLockMemoryPrivilege isn't **enabled** in our token. Being assigned isn't enough; enable it first (§7). |
| 1450 | `ERROR_NO_SYSTEM_RESOURCES` (`:3596`) | No contiguous physical memory: fragmentation (`TMR/src/memory/backend.rs:21-22`). |
| 1455 | `ERROR_COMMITMENT_LIMIT` (`:1570`) | Commit limit reached. |
| 8 | `ERROR_NOT_ENOUGH_MEMORY` (`:3473`) | Out of memory. |
| 87 | `ERROR_INVALID_PARAMETER` (`:2938`) | A bug in the request, e.g. a size that isn't a multiple of `GetLargePageMinimum()` **(MS docs)**. |

**Rules:**
- **Direct path only:** `MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES` in one call, as TMR does
  (`TMR/src/memory/backend.rs:241,345-354`). **Never** `MEM_RESERVE_PLACEHOLDER` or
  `MEM_REPLACE_PLACEHOLDER`: they bugcheck 0x139 on build 26100 (`NF/CLAUDE.md`).
- Allocate once at start-up, off the UI thread. Allocating large pages "can significantly affect
  system performance" **(MS docs)**.
- The memory comes back zeroed **(MS docs; TMR backend.rs:337)**.
- Large pages are non-pageable, so don't `VirtualLock` them.
- `MEM_RELEASE` takes size 0 and the base address (`TMR/src/memory/backend.rs:155-157`).
- **No silent fallback.** Return which page kind you got and why the large-page call was refused,
  so the UI can say so.
- `process: None` means the current process; TMR passes `None` (`backend.rs:347`).

```rust
use windows::Win32::Foundation::{GetLastError, ERROR_NO_SYSTEM_RESOURCES, ERROR_PRIVILEGE_NOT_HELD, WIN32_ERROR};
use windows::Win32::System::Memory::{
    GetLargePageMinimum, MemExtendedParameterAttributeFlags, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualLock,
    VirtualUnlock, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_TYPE, MEM_LARGE_PAGES, MEM_RELEASE,
    MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::SystemServices::{MEM_EXTENDED_PARAMETER_NONPAGED_LARGE, MEM_EXTENDED_PARAMETER_TYPE_BITS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind { Large2M, Locked4K }

#[derive(Debug, Clone, Copy)]
pub enum LargePageRefusal { Unsupported, NoPrivilege, NoContiguousMemory, Other(WIN32_ERROR) }

/// One owned private region.
pub struct Region { ptr: *mut u8, len: usize, kind: PageKind }
// SAFETY: Region is the unique owner of a private VirtualAlloc region; any thread may use or free it.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

/// TMR's create_extended_param (backend.rs:174-187).
fn ext_param(ty: MEM_EXTENDED_PARAMETER_TYPE, value: u64) -> MEM_EXTENDED_PARAMETER {
    let mut p = MEM_EXTENDED_PARAMETER::default();
    p.Anonymous1._bitfield = (ty.0 as u64) & ((1u64 << MEM_EXTENDED_PARAMETER_TYPE_BITS) - 1);
    p.Anonymous2.ULong64 = value; // writing a union field is safe
    p
}

/// Call after enable_privilege(SE_LOCK_MEMORY_NAME) returned Enabled.
pub fn alloc_large(len: usize) -> Result<Region, LargePageRefusal> {
    // SAFETY: no arguments.
    let page = unsafe { GetLargePageMinimum() };
    if page == 0 { return Err(LargePageRefusal::Unsupported); }
    let len = len.div_ceil(page) * page;
    let mut params = [ext_param(MemExtendedParameterAttributeFlags, MEM_EXTENDED_PARAMETER_NONPAGED_LARGE as u64)];
    // SAFETY: params holds plain values (no pointers) and lives across the call.
    let ptr = unsafe {
        VirtualAlloc2(None, None, len, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, PAGE_READWRITE.0, Some(&mut params[..]))
    };
    if ptr.is_null() {
        // SAFETY: read straight after the failed call.
        return Err(match unsafe { GetLastError() } {
            ERROR_PRIVILEGE_NOT_HELD => LargePageRefusal::NoPrivilege,
            ERROR_NO_SYSTEM_RESOURCES => LargePageRefusal::NoContiguousMemory,
            other => LargePageRefusal::Other(other),
        });
    }
    Ok(Region { ptr: ptr.cast(), len, kind: PageKind::Large2M })
}

/// 4 KB fallback, pinned. Call raise_working_set(len + slack) first, or VirtualLock fails with 1453.
pub fn alloc_locked_4k(len: usize) -> windows::core::Result<Region> {
    let len = len.next_multiple_of(4096);
    // SAFETY: a fresh private region, released on the error path.
    unsafe {
        let ptr = VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if ptr.is_null() { return Err(windows::core::Error::from_thread()); }
        if let Err(e) = VirtualLock(ptr, len) {
            let _ = VirtualFree(ptr, 0, MEM_RELEASE);
            return Err(e);
        }
        Ok(Region { ptr: ptr.cast(), len, kind: PageKind::Locked4K })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: we own the region; MEM_RELEASE takes size 0 and the base address.
        unsafe {
            if self.kind == PageKind::Locked4K { let _ = VirtualUnlock(self.ptr.cast(), self.len); }
            let _ = VirtualFree(self.ptr.cast(), 0, MEM_RELEASE);
        }
    }
}
```

`WIN32_ERROR` derives `PartialEq, Eq` (`W/Foundation/mod.rs:10546-10548`), so its constants work as
match patterns. If you ever pass `MemExtendedParameterAddressRequirements`, keep the
`MEM_ADDRESS_REQUIREMENTS` alive across the call. The parameter stores a raw pointer to it (TMR's
LIFETIME note, `backend.rs:316-357`). Large pages don't need it, because they are naturally aligned.

---

## 7. Token: enable a privilege, read elevation, get the user's SID string

```rust
// W/System/Threading/mod.rs:1197-1199 (needs Win32_Security too)
pub unsafe fn OpenProcessToken(processhandle: HANDLE, desiredaccess: TOKEN_ACCESS_MASK, tokenhandle: *mut HANDLE) -> windows_core::Result<()>;
// W/Security/mod.rs
pub unsafe fn LookupPrivilegeValueW<P0, P1>(lpsystemname: P0, lpname: P1, lpluid: *mut LUID) -> windows_core::Result<()>
where P0: Param<PCWSTR>, P1: Param<PCWSTR>;                                                  // :641
pub unsafe fn AdjustTokenPrivileges(tokenhandle: HANDLE, disableallprivileges: bool, newstate: Option<*const TOKEN_PRIVILEGES>,
    bufferlength: u32, previousstate: Option<*mut TOKEN_PRIVILEGES>, returnlength: Option<*mut u32>) -> windows_core::Result<()>; // :207
pub unsafe fn GetTokenInformation(tokenhandle: HANDLE, tokeninformationclass: TOKEN_INFORMATION_CLASS,
    tokeninformation: Option<*mut c_void>, tokeninformationlength: u32, returnlength: *mut u32) -> windows_core::Result<()>; // :454
```

**Types and constants** (`W/Security/mod.rs` unless noted):
- Access rights: `TOKEN_ACCESS_MASK(u32)` (`:1954`, BitOr `:1960`); `TOKEN_QUERY` 8 (`:2169`);
  `TOKEN_ADJUST_PRIVILEGES` 32 (`:1993`).
- Privilege structs: `TOKEN_PRIVILEGES { PrivilegeCount: u32, Privileges: [LUID_AND_ATTRIBUTES; 1] }`
  (`:2124`); `LUID_AND_ATTRIBUTES { Luid: LUID, Attributes: TOKEN_PRIVILEGES_ATTRIBUTES }` (`:1286`);
  `LUID { LowPart: u32, HighPart: i32 }` (`W/Foundation/mod.rs:5814`).
- `SE_PRIVILEGE_ENABLED` = `TOKEN_PRIVILEGES_ATTRIBUTES(2)` (`:1680`).
- Privilege names: `SE_LOCK_MEMORY_NAME: PCWSTR = w!("SeLockMemoryPrivilege")` (`:1676`);
  `SE_INC_WORKING_SET_NAME` (`:1674`).
- Information classes: `TokenUser` 1 (`:2269`), `TokenPrivileges` 3 (`:2254`),
  `TokenElevationType` 18 (`:2231`), `TokenElevation` 20 (`:2230`).
- User and SID: `TOKEN_USER { User: SID_AND_ATTRIBUTES { Sid: PSID, Attributes: u32 } }`
  (`:2204,1740`); `PSID(*mut c_void)` (`:1409`); `SECURITY_MAX_SID_SIZE` 68 (`:1579`).
- Elevation: `TOKEN_ELEVATION { TokenIsElevated: u32 }` (`:2044`); `TOKEN_ELEVATION_TYPE(i32)`
  (`:2049`), with `TokenElevationTypeDefault` 1, `Full` 2, `Limited` 3 (`:2232-2234`).
- `ERROR_NOT_ALL_ASSIGNED` 1300 (`W/Foundation/mod.rs:3454`).

**Pitfalls:**
- **`AdjustTokenPrivileges` returns `Ok(())` even when the token doesn't hold the privilege.** Then
  `GetLastError() == ERROR_NOT_ALL_ASSIGNED` **(MS docs; TMR/src/memory/privileges.rs:194-204)**.
  Read it straight after the call: a log call in between can overwrite it.
- `bufferlength` may be 0 when `previousstate` is `None` **(MS docs)**. TMR does this at
  `privileges.rs:188`.
- The size probe of `GetTokenInformation` returns `Err` (`ERROR_INSUFFICIENT_BUFFER` 122) by design.
  The buffer must be aligned for the struct you read out of it: TMR uses `Vec<u64>`
  (`privileges.rs:132-140`). For `TokenUser`, a fixed `[u64; 16]` (128 bytes, at least 16 + 68)
  needs no probe and no allocation.
- **The privilege appears in the token only after the next sign-in** (`NF/doc/todo/6-inference-memory.md:13`).
- A UAC-filtered admin token may not carry the privilege. This is unconfirmed (`doc/todo/6:16`).
  `TokenElevationType == Limited` identifies that case, so use it for the warning text.
- The dev box's built-in Administrator runs unfiltered by default **(MS docs, default policy)**, so
  the filtered case won't reproduce there.

```rust
use windows::core::{Owned, PCWSTR, PWSTR, Result};
use windows::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, HLOCAL, LUID};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation, TokenElevationType, TokenUser,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege { Enabled, NotAssigned }

/// Enables `name` (SE_LOCK_MEMORY_NAME, SE_INC_WORKING_SET_NAME) in our own token. Needs no admin.
pub fn enable_privilege(name: PCWSTR) -> Result<Privilege> {
    // SAFETY: the token is opened here and closed once by Owned; out-pointers are live locals;
    // GetLastError is read straight after AdjustTokenPrivileges with no call in between.
    unsafe {
        let mut h = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut h)?;
        let token = Owned::new(h);
        let mut luid = LUID::default();
        LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid)?;
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        AdjustTokenPrivileges(*token, false, Some(&tp), 0, None, None)?;
        Ok(if GetLastError() == ERROR_NOT_ALL_ASSIGNED { Privilege::NotAssigned } else { Privilege::Enabled })
    }
}

pub struct Elevation { pub elevated: bool, pub kind: TOKEN_ELEVATION_TYPE }

pub fn elevation() -> Result<Elevation> {
    // SAFETY: each out-buffer is a live local of the type that class returns, with its exact size.
    unsafe {
        let mut h = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h)?;
        let token = Owned::new(h);
        let mut len = 0u32;
        let mut e = TOKEN_ELEVATION::default();
        GetTokenInformation(*token, TokenElevation, Some((&raw mut e).cast()), size_of::<TOKEN_ELEVATION>() as u32, &mut len)?;
        let mut kind = TOKEN_ELEVATION_TYPE::default();
        GetTokenInformation(*token, TokenElevationType, Some((&raw mut kind).cast()), size_of::<TOKEN_ELEVATION_TYPE>() as u32, &mut len)?;
        Ok(Elevation { elevated: e.TokenIsElevated != 0, kind })
    }
}

/// "S-1-5-21-..." for the account this process runs as. Call it in the NON-elevated parent.
pub fn current_user_sid() -> Result<String> {
    // SAFETY: buf is 8-aligned and holds TOKEN_USER plus the largest SID (16 + 68 bytes); Sid points
    // into buf, which outlives its use; the string is LocalFree'd once by Owned<HLOCAL>.
    unsafe {
        let mut h = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h)?;
        let token = Owned::new(h);
        let mut buf = [0u64; 16];
        let mut len = 0u32;
        GetTokenInformation(*token, TokenUser, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)?;
        let _free = Owned::new(HLOCAL(s.0.cast()));
        Ok(s.to_string()?) // FromUtf16Error -> Error (result/error.rs:185)
    }
}
// LocalFree is used through Owned<HLOCAL>; the direct form is LocalFree(Some(HLOCAL(p))) (W/Foundation/mod.rs:38).
```

---

## 8. LSA: grant SeLockMemoryPrivilege to a SID (elevated child only)

```rust
// W/Security/Authentication/Identity/mod.rs
pub unsafe fn LsaOpenPolicy(systemname: Option<*const LSA_UNICODE_STRING>, objectattributes: *const LSA_OBJECT_ATTRIBUTES,
    desiredaccess: u32, policyhandle: *mut LSA_HANDLE) -> NTSTATUS;                          // :472
pub unsafe fn LsaAddAccountRights(policyhandle: LSA_HANDLE, accountsid: PSID, userrights: &[LSA_UNICODE_STRING]) -> NTSTATUS; // :357 (slice -> ptr + count)
pub unsafe fn LsaClose(objecthandle: LSA_HANDLE) -> NTSTATUS;                                // :367
pub unsafe fn LsaNtStatusToWinError(status: NTSTATUS) -> u32;                                // :467
pub unsafe fn LsaEnumerateAccountRights(policyhandle: LSA_HANDLE, accountsid: PSID,
    userrights: *mut *mut LSA_UNICODE_STRING, countofrights: *mut u32) -> NTSTATUS;          // :392 (optional read-back)
pub unsafe fn LsaFreeMemory(buffer: Option<*const c_void>) -> NTSTATUS;                      // :417
// W/Security/Authorization/mod.rs
pub unsafe fn ConvertStringSidToSidW<P0>(stringsid: P0, sid: *mut PSID) -> windows_core::Result<()> where P0: Param<PCWSTR>; // :368
pub unsafe fn ConvertSidToStringSidW(sid: PSID, stringsid: *mut PWSTR) -> windows_core::Result<()>;                         // :339
```

**Types:**
- `LSA_HANDLE(pub isize)` (`:3274`, `Default`; its `Free` calls `LsaClose`, `:3280`).
- `LSA_OBJECT_ATTRIBUTES` (`:3317`, `Default` zeroed). Its members are unused, so zero them
  **(MS docs)**.
- `LSA_UNICODE_STRING { Length: u16, MaximumLength: u16, Buffer: PWSTR }` (`:3506`). Lengths are in
  **bytes**, and `Length` excludes the NUL **(MS docs)**.
- `POLICY_CREATE_ACCOUNT: i32 = 16` (`:4403`) and `POLICY_LOOKUP_NAMES: i32 = 2048` (`:4449`) are
  `i32`, while `desiredaccess` is `u32`, so cast with `as u32`. **There is no `POLICY_ALL_ACCESS`
  in the crate.**
- `NTSTATUS(pub i32)` (`W/Foundation/mod.rs:6093`).

**Freeing:**
- The `PSID` from `ConvertStringSidToSidW` and the string from `ConvertSidToStringSidW` are
  LocalAlloc'd. Free them with `LocalFree` **(MS docs)**, e.g. through `Owned<HLOCAL>`.
- `IsValidSid(psid) -> BOOL` (`W/Security/mod.rs:514`).

**Requirements (MS docs):**
- The caller must be elevated.
- `LsaAddAccountRights` needs `POLICY_LOOKUP_NAMES`, plus `POLICY_CREATE_ACCOUNT` when the SID has
  no LSA account object yet.
- Not verified here: that re-adding an existing right returns success. Read back with
  `LsaEnumerateAccountRights` if it matters.

```rust
use windows::core::{Owned, HSTRING, PWSTR};
use windows::Win32::Foundation::{ERROR_INVALID_SID, HLOCAL, WIN32_ERROR};
use windows::Win32::Security::Authentication::Identity::{
    LsaAddAccountRights, LsaNtStatusToWinError, LsaOpenPolicy, LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{IsValidSid, PSID, SE_LOCK_MEMORY_NAME};

/// Elevated child only. Returns a Win32 code (0 = granted), used as the process exit code.
pub fn grant_lock_memory(sid_text: &str) -> u32 {
    let wide = HSTRING::from(sid_text);
    // SAFETY: the PSID is LocalAlloc'd by ConvertStringSidToSidW and freed once by Owned<HLOCAL>;
    // the policy handle is closed once by Owned<LSA_HANDLE>; `right` and the name it points to outlive the call.
    unsafe {
        let mut sid = PSID::default();
        if let Err(e) = ConvertStringSidToSidW(&wide, &mut sid) {
            return WIN32_ERROR::from_error(&e).map_or(ERROR_INVALID_SID.0, |w| w.0);
        }
        let _sid_mem = Owned::new(HLOCAL(sid.0));
        if !IsValidSid(sid).as_bool() { return ERROR_INVALID_SID.0; }

        let attrs = LSA_OBJECT_ATTRIBUTES::default();
        let mut policy = LSA_HANDLE::default();
        let st = LsaOpenPolicy(None, &attrs, (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32, &mut policy);
        if st.is_err() { return LsaNtStatusToWinError(st); }
        let policy = Owned::new(policy);

        let units = SE_LOCK_MEMORY_NAME.len(); // 21 UTF-16 units (wcslen)
        let right = LSA_UNICODE_STRING {
            Length: (units * 2) as u16,
            MaximumLength: ((units + 1) * 2) as u16,
            Buffer: PWSTR(SE_LOCK_MEMORY_NAME.0 as *mut u16), // LSA only reads it
        };
        LsaNtStatusToWinError(LsaAddAccountRights(*policy, sid, &[right]))
    }
}
```

**Which SID.** Grant to the interactive user's SID, which the **non-elevated** parent gets from
`current_user_sid()` (§7) and passes on the command line. Never use the elevated process's
`USERNAME`: a standard user who elevates with an admin's password would grant the admin
(`NF/doc/todo/6-inference-memory.md:14`). Don't restrict the SID prefix to `S-1-5-21-`. Entra ID
accounts have other prefixes (our note, not verified here).

---

## 9. The elevated child: ShellExecuteExW "runas"

```rust
// W/UI/Shell/mod.rs:4360-4362
#[cfg(feature = "Win32_System_Registry")]
pub unsafe fn ShellExecuteExW(pexecinfo: *mut SHELLEXECUTEINFOW) -> windows_core::Result<()>;
// x86_64 layout, :54303-54330 (Default = zeroed, Clone + Copy):
pub struct SHELLEXECUTEINFOW {
    pub cbSize: u32, pub fMask: u32, pub hwnd: HWND, pub lpVerb: PCWSTR, pub lpFile: PCWSTR,
    pub lpParameters: PCWSTR, pub lpDirectory: PCWSTR, pub nShow: i32, pub hInstApp: HINSTANCE,
    pub lpIDList: *mut c_void, pub lpClass: PCWSTR, pub hkeyClass: HKEY, pub dwHotKey: u32,
    pub Anonymous: SHELLEXECUTEINFOW_0, pub hProcess: HANDLE,
}
```

**Flags and constants:**
- `fMask` bits are plain `u32`: `SEE_MASK_NOCLOSEPROCESS` 0x40 (`:53636`), `SEE_MASK_NOASYNC` 0x100
  (`:53635`), `SEE_MASK_FLAG_NO_UI` 0x400 (`:53629`).
- `nShow` is `i32`, but `SW_*` are `SHOW_WINDOW_CMD(i32)` (`W/UI/WindowsAndMessaging/mod.rs:5953`;
  `SW_HIDE` 0, `SW_SHOWNORMAL` 1). Use `.0`, or a literal.

**Waiting for the child:**
- `WaitForSingleObject(hhandle: HANDLE, dwmilliseconds: u32) -> WAIT_EVENT`
  (`W/System/Threading/mod.rs:1929`).
- `INFINITE: u32` (`:2145`); `WAIT_OBJECT_0` and `WAIT_FAILED` (`W/Foundation/mod.rs:10486,10484`).
- `GetExitCodeProcess(hprocess: HANDLE, lpexitcode: *mut u32) -> Result<()>` (`:724`).
- `STILL_ACTIVE` is `NTSTATUS(0x103)` (`W/Foundation/mod.rs:9891`), so compare with `.0 as u32`.

**Behaviour:**
- If the user answers "No" at the UAC prompt, `ShellExecuteExW` fails with `ERROR_CANCELLED` 1223
  (`W/Foundation/mod.rs:1307`) **(MS docs)**.
- `hProcess` is null if no process was launched, even with `NOCLOSEPROCESS` **(MS docs)**.
- `std::process::Command` can't elevate. Only `ShellExecuteEx` with "runas" shows the UAC prompt
  **(MS docs)**.

**COM:** call `CoInitializeEx(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)` before
`ShellExecuteEx` **(MS docs)**.
- Signature: `CoInitializeEx(pvreserved: Option<*const c_void>, dwcoinit: COINIT) -> HRESULT`
  (`W/System/Com/mod.rs:334`). It returns `HRESULT`, not `Result`.
- It returns `S_FALSE` if COM is already initialised and `RPC_E_CHANGED_MODE` on an MTA thread such
  as a WASAPI thread (`W/Foundation/mod.rs:9911,6517`).
- So launch from a dedicated worker thread, which also does the blocking wait. That keeps the UI
  thread free and avoids sending a `HANDLE` across threads.

```rust
use windows::core::{w, Error, Owned, HSTRING, PCWSTR, Result};
use windows::Win32::Foundation::{E_FAIL, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

/// Blocks until the child exits: run it on its own std::thread, never the UI or audio thread.
/// Ok(child's exit code); Err with HRESULT_FROM_WIN32(ERROR_CANCELLED) if the user declined UAC.
pub fn run_self_elevated(args: &str) -> Result<u32> {
    let exe = HSTRING::from(std::env::current_exe()?.as_path()); // io::Error -> Error (result/error.rs:176)
    let params = HSTRING::from(args);                           // e.g. "--grant-lock-memory S-1-5-21-..."
    // SAFETY: COM init/uninit are paired on this thread; exe and params outlive the call; hProcess is
    // ours (SEE_MASK_NOCLOSEPROCESS) and closed once by Owned.
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            hwnd: HWND::default(), // or the settings window's HWND, if Reactor exposes one
            lpVerb: w!("runas"),
            lpFile: PCWSTR(exe.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: 0, // SW_HIDE
            ..Default::default()
        };
        let launched = ShellExecuteExW(&mut info);
        if com.is_ok() { CoUninitialize(); }
        launched?;
        if info.hProcess.is_invalid() { return Err(E_FAIL.into()); }
        let child = Owned::new(info.hProcess);
        if WaitForSingleObject(*child, INFINITE) != WAIT_OBJECT_0 { return Err(Error::from_thread()); }
        let mut code = 0u32;
        GetExitCodeProcess(*child, &mut code)?;
        Ok(code)
    }
}

// In nf-app's main(), before any window or engine starts:
// let args: Vec<String> = std::env::args().collect();
// if args.get(1).map(String::as_str) == Some("--grant-lock-memory") {
//     let code = args.get(2).map_or(87, |sid| grant_lock_memory(sid)); // 87 = ERROR_INVALID_PARAMETER
//     std::process::exit(code as i32);
// }
```

When the child returns 0, the UI must say "sign out and back in". Relaunching the app isn't enough
(`NF/doc/todo/6-inference-memory.md:13`).

---

## 10. Thread priority fallback, naming, EcoQoS

- `SetThreadPriority(hthread: HANDLE, npriority: THREAD_PRIORITY) -> Result<()>`
  (`W/System/Threading/mod.rs:1706`).
  - `THREAD_PRIORITY(i32)` (`:3452`): `TIME_CRITICAL` 15 (`:3460`), `HIGHEST` 2 (`:3455`),
    `ABOVE_NORMAL` 1 (`:3453`), `NORMAL` 0 (`:3459`).
  - Use it only as the MMCSS fallback (§3). In a normal-class process, `TIME_CRITICAL` is base
    priority 15, below MMCSS's real-time band **(MS docs)**.
- **Naming: std already does it.** `std::thread::Builder::new().name("nf-audio".into())` calls
  `SetThreadDescription(GetCurrentThread(), ..)` on the new thread (`std/thread/lifecycle.rs:148-149`
  → `std/sys/thread/windows.rs:99-115`).
  - For other threads, call it directly: `SetThreadDescription<P1: Param<PCWSTR>>(hthread: HANDLE,
    lpthreaddescription: P1) -> Result<()>` (`:1676`; HRESULT-based, so its error path does a COM
    call). `SetThreadDescription(GetCurrentThread(), w!("nf-main"))`.
  - `GetThreadDescription(hthread) -> Result<PWSTR>` (`:926`). Free the returned string with
    `LocalFree` **(MS docs)**.
- **EcoQoS opt-out (optional):**
  - Process: `SetProcessInformation(hprocess, ProcessPowerThrottling, ptr, size) -> Result<()>`
    (`:1636`, class `:3041`), with
    `PROCESS_POWER_THROTTLING_STATE { Version: u32, ControlMask: u32, StateMask: u32 }` (`:2804`):
    `Version = PROCESS_POWER_THROTTLING_CURRENT_VERSION` (1), `ControlMask = _EXECUTION_SPEED` (1),
    `StateMask = 0` (`:2799-2800`) **(MS docs semantics)**.
  - Thread: `SetThreadInformation` + `ThreadPowerThrottling` + `THREAD_POWER_THROTTLING_STATE`
    (`:1701,3532,3444,3440-3441`).
  - `doc/todo/6` says MMCSS-tagged threads are already exempt from the minimised-window QoS drop,
    so this is belt and braces.

---

## 11. FTZ/DAZ on x86-64

- **`core::arch::x86_64::{_mm_getcsr, _mm_setcsr}` are still in nightly 1.101, but deprecated:**
  `#[deprecated(since = "1.75.0", note = "see `_mm_getcsr` documentation - use inline assembly instead")]`
  (`stdarch/x86/sse.rs:1509-1513`, and `:1658-1662` for `_mm_setcsr`). `_MM_SET_FLUSH_ZERO_MODE` is
  deprecated too (`:1840-1845`).
- The constants aren't deprecated: `_MM_FLUSH_ZERO_ON` = 0x8000 (`:1732`) and `_MM_FLUSH_ZERO_MASK`
  (`:1729`). **There is no DAZ constant in `core::arch`**: no `_MM_DENORMALS_ZERO_*` anywhere in
  `stdarch/x86`. DAZ is MXCSR bit 6 (0x0040).
- **Rust's own documentation says changing these modes is UB.** The `_mm_setcsr` doc
  (`sse.rs:1541-1551`) says that changing the masking flags, the rounding mode or DAZ "leads to
  **immediate Undefined Behavior**", even if it is restored later. The sanctioned use is an asm
  block that restores MXCSR before the block ends. Inline asm removes the deprecation warning, not
  the rule.
- **What we have:** `nf-core` `rt::enable_flush_denormals()` / `rt::restore_mxcsr()`
  (`NF/crates/nf-core/src/rt.rs:7-35`) set FTZ|DAZ persistently with `stmxcsr`/`ldmxcsr` asm.
  - This is outside Rust's guarantees, and its SAFETY comment (`rt.rs:11-12`) doesn't say so.
  - The practical risk, as we read it, is that floats evaluated at compile time ignore FTZ/DAZ, so
    the same expression can give a different result when folded than at run time.
  - Accept that on the audio threads only. Tests that depend on flushing must `black_box` their
    operands, as `rt.rs:72-76` does.
- Nit: `restore_mxcsr` passes `options(preserves_flags)` (`rt.rs:31`), but loading `old` overwrites
  the MXCSR exception flags set since. The Rust reference counts those flags under
  `preserves_flags` (asm.rules.preserved-registers; local copy:
  `share/doc/rust/html/reference/inline-assembly.html`). It's harmless, and dropping the option
  there costs nothing.
- MXCSR is per thread, so set it on each audio thread after `RtSched::enter()`. It also governs
  VEX (AVX/AVX2) arithmetic (Intel SDM), so it covers nnnoiseless's and sonora's runtime-dispatched
  kernels.

---

## 12. What to take from TMR

TMR-APP is `MIT OR Apache-2.0`, copyright Jarrod Petz (`TMR/Cargo.toml:16-17`,
`TMR/LICENSE-MIT:3`). noise-filter uses the same licence (`NF/Cargo.toml`, `[workspace.package]`),
and the task says the owner is the same, so copying is fine. Keep a provenance comment ("adapted
from TMR-APP/src/memory/privileges.rs").

**`TMR/src/memory/privileges.rs`**

| Lines | Item | Verdict |
|---|---|---|
| 1-5 | `#![warn(clippy::undocumented_unsafe_blocks)]` header | **Copy** the convention. |
| 22-27, 419-443 | `PrivilegeState`, `check_privilege_state` | **Copy** if the GUI wants a read-only status. |
| 29-35 | `format_win32_error` | Not needed: `Error::message()` / `HRESULT::message()` (§2). |
| 37-114 | `check_large_page_privilege` | **Copy the token half (42-88).** Drop the 2 MB `VirtualAlloc` probe (90-112): it takes a contiguous page for nothing; the real allocation is the test. |
| 121-174 | `check_privilege_in_token` | **Copy**: the aligned `Vec<u64>` note (132-140) is the important part. The loop can be one slice over `addr_of!((*base).Privileges)`. |
| 180-205 | `enable_privilege_in_token` | **Copy**, generalised to any name (§7). |
| 207-236 | `is_elevated` | **Copy**; add `TokenElevationType` (§7). |
| 239-294 | `enable_large_page_privilege_enhanced` | **Don't.** It duplicates 180-205, asks for needless `TOKEN_DUPLICATE \| TOKEN_ASSIGN_PRIMARY`, and adds `SE_PRIVILEGE_ENABLED_BY_DEFAULT`, which isn't one of AdjustTokenPrivileges' NewState attributes **(MS docs)**. |
| 296-320 | `auto_grant_large_page_privilege` | **Don't.** It grants to the elevated process's `%USERDOMAIN%\%USERNAME%` (wrong account; caller-controlled env vars). |
| 323-416, 465-509 | `setup_large_pages_automatically`, `diagnose_and_setup_large_pages` | **Don't.** They mix checking with granting, run secedit when elevated, print emoji text, and say "Restart TMR" (344) where a sign-out is needed. |
| 446-463 | `test_large_page_allocation` | **Don't** (the probe again). |
| 511-720 | `secedit`, `grant_privilege_via_secedit`, `modify_security_policy`, `check_restart_needed`, `check_if_privilege_in_policy` | **Don't.** `LsaAddAccountRights` (§8) replaces exporting, editing and re-importing the whole security policy through temp files. |

**`TMR/src/memory/backend.rs`**

| Lines | Item | Verdict |
|---|---|---|
| 21-29, 37-59 | error constants, `describe_error`, `is_exhaustion` | **Copy** (or use `WIN32_ERROR` constants). |
| 31-35 | `NUMA_NODE_MANDATORY` | Not needed (single-node desktops). |
| 61-121 | `AllocError`, `Backend`, `BackendType`, `WindowsBackend` | **Don't**: TMR's revival seams. |
| 124-147 | `allocate` | **Don't**: silently downgrades to Regular when `use_large_pages` is false (128-142). |
| 149-165 | `free` | **Copy** (`MEM_RELEASE`, size 0). |
| 174-187 | `create_extended_param` | **Copy** (§6 `ext_param`). |
| 230-247, 280-288, 316-357 | size rounding, the attribute parameter, the `VirtualAlloc2` call and the `addr_req` LIFETIME rule | **Copy the call shape.** |
| 211-227 | `setup_large_pages_automatically()` in the allocation path | **Don't.** It runs secedit when elevated, and on failure it retries as Regular. |
| 255-273 | auto mode | **Don't**: 2 MB pages only for requests of 4 MB or more. |
| 364-392 | Any/Prefer retries huge → large → regular, logged at `debug` only | **Don't.** This is the silent Prefer fallback: the caller can't tell what it got. |
| 367-372 | NUMA retry | **Don't** (not needed). |

**Placeholder paths are not in backend.rs.** They're in `TMR/src/memory/stitched.rs`
(`MEM_RESERVE_PLACEHOLDER`, `MEM_REPLACE_PLACEHOLDER` and `MEM_PRESERVE_PLACEHOLDER` at `:69-70`
and `:412-455`; the bugcheck note is at `:101`). Copy nothing from stitched.rs.

---

## 13. Pitfall checklist

1. **Enabling isn't granting.** `AdjustTokenPrivileges` succeeds even when nothing was assigned;
   check `GetLastError() == ERROR_NOT_ALL_ASSIGNED` straight away. `MEM_LARGE_PAGES` returns 1314
   unless the privilege is *enabled* in our token, and a new grant shows up only after the next
   sign-in.
2. **Features and types.**
   - `ShellExecuteExW` needs `Win32_UI_Shell` + `Win32_System_Registry`.
   - The SID-string functions need `Win32_Security_Authorization`, which nf-mem lacks.
     `OpenProcessToken` needs `Win32_Security` as well as Threading.
   - Type mismatches: `VirtualAlloc2` takes protection as `u32` (`PAGE_READWRITE.0`); `POLICY_*`
     are `i32`; `GetProcessWorkingSetSizeEx` returns raw `BOOL`; `STILL_ACTIVE` is `NTSTATUS`;
     `SW_*` aren't `i32`.
3. **Hot-path costs.**
   - Success paths allocate nothing.
   - On failure, `BOOL` wrappers read `GetLastError` only, while `HRESULT`/`NTSTATUS` wrappers
     also call `GetErrorInfo` (COM).
   - Formatting an `Error` calls `FormatMessageW` and allocates.
   - `HSTRING::from` allocates on the process heap, invisible to a counting `#[global_allocator]`.
   - Use `w!` and keep raw codes on the audio thread.
4. **Handles.**
   - `HANDLE`, `PSID` and `PWSTR` are `!Send`/`!Sync`.
   - The MMCSS handle is per thread: revert it with `AvRevertMmThreadCharacteristics`, never
     `CloseHandle`/`Owned<HANDLE>`.
   - `HANDLE::is_invalid()` is true for `GetCurrentProcess()`.
   - SID strings and PSIDs from the conversion functions must be `LocalFree`'d.
5. **FTZ/DAZ** via asm (`nf-core/src/rt.rs`) is the working approach, but Rust documents a
   persistent MXCSR change as UB. Keep it to the audio threads, set it per thread, and `black_box`
   in tests.
6. **From TMR:** no secedit, no `USERNAME` grant, no silent fallback, no placeholder calls. Grant
   through LSA to the SID the non-elevated parent passes in, and report which page kind was
   allocated.
