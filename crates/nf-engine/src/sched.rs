//! Per-thread set-up for engine threads: COM in the multithreaded apartment, and MMCSS "Pro Audio".
//!
//! Both guards are `!Send`: they register the calling thread and must be undone on it.

use std::marker::PhantomData;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AVRT_PRIORITY, AVRT_PRIORITY_CRITICAL, AVRT_PRIORITY_NORMAL, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    AvSetMmThreadPriority, GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY, THREAD_PRIORITY_NORMAL,
    THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::core::w;

/// COM initialised as MTA on this thread; uninitialised on drop. Declare it first in a thread
/// function so it drops after every wasapi object.
pub(crate) struct Com {
    _not_send: PhantomData<*const ()>,
}

impl Com {
    /// `Err(hresult)` when COM can't be initialised as MTA (e.g. `RPC_E_CHANGED_MODE` on an STA
    /// thread). `S_FALSE` (already MTA) is success and is balanced by the drop like `S_OK`.
    pub(crate) fn mta() -> Result<Self, i32> {
        let hr = wasapi::initialize_mta();
        if hr.is_ok() { Ok(Self { _not_send: PhantomData }) } else { Err(hr.0) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

/// Which priority the thread asks for inside MMCSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MmcssPriority {
    /// The audio thread: the one thread whose deadline matters most.
    Critical,
    /// Helpers (the latency test).
    Normal,
}

/// The calling thread's MMCSS "Pro Audio" registration, reverted on drop. When registration fails,
/// the thread runs at TIME_CRITICAL instead, and the drop puts its previous priority back (RT-2: the
/// latency test drops the guard before its long analysis, which must not run at priority 15).
pub(crate) struct Mmcss {
    handle: Option<HANDLE>,
    /// The Win32 error when registration failed (the thread then runs at TIME_CRITICAL).
    pub(crate) error: Option<u32>,
    /// The fallback's priority to restore on drop.
    restore: Option<THREAD_PRIORITY>,
    _not_send: PhantomData<*const ()>,
}

impl Mmcss {
    /// Registers the calling thread. Never fails: on refusal it records the error and raises the
    /// thread's normal priority instead, so the caller can report it and carry on.
    pub(crate) fn enter(priority: MmcssPriority) -> Self {
        let mut task_index = 0u32;
        // SAFETY: `w!` is a 'static NUL-terminated literal; `task_index` is a live local that starts
        // at 0, as the first call requires.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) => {
                let p: AVRT_PRIORITY = match priority {
                    MmcssPriority::Critical => AVRT_PRIORITY_CRITICAL,
                    MmcssPriority::Normal => AVRT_PRIORITY_NORMAL,
                };
                // SAFETY: `handle` was just returned for this thread.
                let _ = unsafe { AvSetMmThreadPriority(handle, p) };
                Self { handle: Some(handle), error: None, restore: None, _not_send: PhantomData }
            }
            Err(e) => {
                // Keep the code; the control thread words it.
                let code = e.code().0 as u32 & 0xFFFF;
                Self { handle: None, error: Some(code), restore: raise_to_time_critical(), _not_send: PhantomData }
            }
        }
    }

    pub(crate) fn registered(&self) -> bool {
        self.handle.is_some()
    }
}

/// The fallback when MMCSS refuses: TIME_CRITICAL. Returns the priority to restore (NORMAL if the
/// current one can't be read), or `None` when the raise itself failed.
fn raise_to_time_critical() -> Option<THREAD_PRIORITY> {
    // SAFETY: GetCurrentThread returns a pseudo-handle that needs no closing; both calls only act on
    // the calling thread.
    unsafe {
        let thread = GetCurrentThread();
        // THREAD_PRIORITY_ERROR_RETURN (MAXLONG) when it can't be read.
        let prev = match GetThreadPriority(thread) {
            p if p == i32::MAX => THREAD_PRIORITY_NORMAL,
            p => THREAD_PRIORITY(p),
        };
        SetThreadPriority(thread, THREAD_PRIORITY_TIME_CRITICAL).ok().map(|()| prev)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // SAFETY: same thread (the guard is !Send); `h` came from AvSetMmThreadCharacteristicsW.
            // It's an AVRT handle, not a kernel object, so it's never CloseHandle'd.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
        if let Some(p) = self.restore.take() {
            // SAFETY: same thread (the guard is !Send); the pseudo-handle needs no closing.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), p) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priority() -> i32 {
        // SAFETY: reads the calling thread's priority through its pseudo-handle.
        unsafe { GetThreadPriority(GetCurrentThread()) }
    }

    /// RT-2: the fallback (no MMCSS) raises the thread to TIME_CRITICAL, and dropping the guard
    /// puts the previous priority back. Runs on its own short-lived thread.
    #[test]
    fn the_fallback_priority_is_restored_on_drop() {
        std::thread::spawn(|| {
            let before = priority();
            let guard = Mmcss { handle: None, error: Some(1552), restore: raise_to_time_critical(), _not_send: PhantomData };
            assert_eq!(priority(), THREAD_PRIORITY_TIME_CRITICAL.0);
            assert!(!guard.registered());
            drop(guard);
            assert_eq!(priority(), before);
        })
        .join()
        .unwrap();
    }

    /// Whichever way registration goes, the guard leaves the thread at its old priority.
    #[test]
    fn entering_and_leaving_mmcss_leaves_the_priority_as_it_was() {
        std::thread::spawn(|| {
            let before = priority();
            drop(Mmcss::enter(MmcssPriority::Normal));
            assert_eq!(priority(), before);
        })
        .join()
        .unwrap();
    }
}
