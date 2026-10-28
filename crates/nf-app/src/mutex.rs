//! Named Win32 mutexes, shared between processes: the log's rotation lock (logger.rs) and the
//! one-window guard (instance.rs). A `Local\` name is one per sign-in session.
//!
//! A mutex belongs to the thread that acquired it: [`Held`] borrows the mutex and is `!Send`, so it
//! is released on that thread. When the owning process exits without releasing it, the next waiter
//! gets it ("abandoned"), which counts as acquired here.

use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_ABANDONED, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::{HSTRING, Owned};

/// A named mutex, its handle closed on drop.
pub struct NamedMutex(Owned<HANDLE>);

impl NamedMutex {
    /// Opens the mutex `name`, creating it if no process has it open. This doesn't acquire it.
    pub fn open(name: &str) -> windows::core::Result<Self> {
        let name = HSTRING::from(name);
        // SAFETY: default security, not initially owned; `name` is NUL-terminated and outlives the
        // call. On success the handle is ours, and Owned closes it once.
        unsafe {
            let h = CreateMutexW(None, false, &name)?;
            Ok(Self(Owned::new(h)))
        }
    }

    /// Waits up to `timeout` to own the mutex on this thread. `None`: another thread or process
    /// still holds it.
    pub fn acquire(&self, timeout: Duration) -> Option<Held<'_>> {
        self.wait(timeout).then_some(Held(self))
    }

    /// [`acquire`](Self::acquire), keeping the mutex with its ownership (released and closed on
    /// drop). `Err` gives the mutex back when the wait timed out.
    pub fn into_held(self, timeout: Duration) -> Result<HeldMutex, Self> {
        if self.wait(timeout) { Ok(HeldMutex(self)) } else { Err(self) }
    }

    fn wait(&self, timeout: Duration) -> bool {
        // INFINITE is u32::MAX; anything that long is a mistake here, so stay below it.
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
        // SAFETY: a valid mutex handle that `self` owns.
        let r = unsafe { WaitForSingleObject(*self.0, ms) };
        acquired(r)
    }

    fn release(&self) {
        // SAFETY: a valid mutex handle; called only by a Held or HeldMutex, which exist only while
        // this thread owns the mutex.
        let _ = unsafe { ReleaseMutex(*self.0) };
    }
}

/// A wait's result: signalled, or abandoned by an owner that exited, both give ownership.
fn acquired(r: WAIT_EVENT) -> bool {
    r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
}

/// Ownership of a borrowed [`NamedMutex`], released on drop.
pub struct Held<'a>(&'a NamedMutex);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A [`NamedMutex`] with its ownership: released, then closed, on drop (on the thread that
/// acquired it: `!Send`, as the handle is).
pub struct HeldMutex(NamedMutex);

impl Drop for HeldMutex {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A mutex name for `path`: `Local\noise-filter-<what>-<FNV-1a of the lower-cased path>`. A name
/// can't hold a backslash after the namespace, and the hash is the same in every process and build.
pub fn name_for_path(what: &str, path: &std::path::Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for unit in path.to_string_lossy().to_lowercase().encode_utf16() {
        for byte in unit.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!(r"Local\noise-filter-{what}-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn unique(name: &str) -> String {
        format!(r"Local\nf-app-test-{name}-{}", std::process::id())
    }

    #[test]
    fn a_held_mutex_keeps_others_out_until_released() {
        let name = unique("held");
        let mine = NamedMutex::open(&name).unwrap();
        let held = mine.acquire(Duration::ZERO).expect("free at first");
        let ask = name.clone();
        let (tell, got) = mpsc::channel();
        let (go, wait_go) = mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            // Another "process": its own handle on its own thread
            let theirs = NamedMutex::open(&ask).unwrap();
            tell.send(theirs.acquire(Duration::from_millis(50)).is_some()).unwrap();
            wait_go.recv().unwrap();
            tell.send(theirs.acquire(Duration::from_secs(5)).is_some()).unwrap();
        });
        assert!(!got.recv().unwrap(), "held elsewhere");
        drop(held);
        go.send(()).unwrap();
        assert!(got.recv().unwrap(), "released");
        other.join().unwrap();
    }

    #[test]
    fn an_owner_that_exits_without_releasing_hands_it_on() {
        let name = unique("abandoned");
        let ask = name.clone();
        std::thread::spawn(move || {
            let m = NamedMutex::open(&ask).unwrap();
            // The owner dies holding it: no release, and the handle stays open
            std::mem::forget(m.into_held(Duration::ZERO).ok().expect("free"));
        })
        .join()
        .unwrap();
        let m = NamedMutex::open(&name).unwrap();
        assert!(m.into_held(Duration::from_secs(5)).is_ok(), "abandoned counts as acquired");
    }

    #[test]
    fn path_names_are_stable_and_case_blind() {
        let a = name_for_path("log", std::path::Path::new(r"C:\Users\Me\AppData\Local\noise-filter\noise-filter.log"));
        let b = name_for_path("log", std::path::Path::new(r"c:\users\me\appdata\local\NOISE-FILTER\noise-filter.LOG"));
        assert_eq!(a, b);
        assert!(a.starts_with(r"Local\noise-filter-log-") && a.len() == r"Local\noise-filter-log-".len() + 16, "{a}");
        assert_eq!(a.matches('\\').count(), 1, "{a}");
        assert_ne!(a, name_for_path("log", std::path::Path::new(r"D:\other.log")));
    }
}
