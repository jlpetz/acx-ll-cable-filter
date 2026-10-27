//! One settings window per sign-in session (gui/F5).
//!
//! A second window would run a second engine into the same cable, so callers would hear a doubled,
//! comb-filtered voice, and two capture streams would hold the mic. Both windows would also save to
//! the same settings.json. The window takes the named mutex `Local\noise-filter.gui` (one per
//! sign-in session) in `main`, before the locked pool exists, and owns it until the process exits.
//! A second launch that finds it owned brings the first window forward and exits.
//!
//! The first window is found with `EnumWindows`: a visible top-level window titled "noise-filter"
//! whose process runs the same exe as this one (`QueryFullProcessImageNameW` for both, so the two
//! paths have the same form). The title alone isn't enough, since an Explorer window on a folder
//! named "noise-filter" has it too. A noise-filter.exe in another folder (a second install, a dev
//! build) still counts, if no exact match exists.
//! The first instance may still be opening its window, or closing: the second waits a few seconds
//! for the window, or for the mutex to be released, before telling the user.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, HWND, LPARAM, TRUE, WIN32_ERROR};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow,
    ShowWindow,
};
use windows::core::{BOOL, Owned, PWSTR};

use crate::mutex::{HeldMutex, NamedMutex};

/// The window's mutex: one per sign-in session.
pub const MUTEX_NAME: &str = r"Local\noise-filter.gui";
/// nf-gui's window title (`window_title("noise-filter")`).
pub const WINDOW_TITLE: &str = "noise-filter";
/// How long a second launch waits for the first one's window (it may be opening) or for the first
/// one to exit (it may be closing).
const PATIENCE: Duration = Duration::from_secs(5);
/// How often it looks again meanwhile.
const STEP: Duration = Duration::from_millis(100);

/// What a launch of the window found.
pub enum Claim {
    /// This is the only window: keep the guard until the process exits. `None`: the mutex couldn't
    /// be made at all (logged), so the window opens unguarded rather than not at all.
    First(Option<HeldMutex>),
    /// Another window is open, and was brought forward (its process id).
    Forwarded(u32),
    /// Another instance holds the mutex, but no window of it showed up in time.
    NoWindow,
}

/// Claims the window for this process, or hands over to the one that has it.
pub fn claim_window() -> Claim {
    claim(MUTEX_NAME, WINDOW_TITLE, PATIENCE)
}

fn claim(name: &str, title: &str, patience: Duration) -> Claim {
    let mut mutex = match NamedMutex::open(name) {
        Ok(m) => Some(m),
        // An elevated instance made it, and this one may not open it: it's running all the same
        Err(e) if WIN32_ERROR::from_error(&e) == Some(ERROR_ACCESS_DENIED) => None,
        Err(e) => {
            log::warn!("couldn't make the single-window mutex ({e}); opening the window unguarded");
            return Claim::First(None);
        }
    };
    let deadline = Instant::now() + patience;
    let mut wait = Duration::ZERO;
    loop {
        if let Some(m) = mutex.take() {
            match m.into_held(wait) {
                Ok(held) => return Claim::First(Some(held)),
                Err(m) => mutex = Some(m),
            }
        }
        if let Some((hwnd, pid)) = find_window(title) {
            bring_forward(hwnd);
            return Claim::Forwarded(pid);
        }
        if Instant::now() >= deadline {
            return Claim::NoWindow;
        }
        // Wait on the mutex itself (the first instance may be exiting), or just sleep without one
        wait = STEP;
        if mutex.is_none() {
            std::thread::sleep(STEP);
        }
    }
}

/// A top-level window that might be the first instance's.
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    hwnd: isize,
    pid: u32,
    exe: Option<PathBuf>,
}

/// The visible window titled `title` of another process running noise-filter: the same exe first,
/// then one of the same file name.
fn find_window(title: &str) -> Option<(HWND, u32)> {
    let ours = process_exe(None)?;
    let candidates: Vec<Candidate> = titled_windows(title)
        .into_iter()
        .filter(|&(_, pid)| pid != std::process::id())
        .map(|(hwnd, pid)| Candidate { hwnd: hwnd.0 as isize, pid, exe: process_exe(Some(pid)) })
        .collect();
    pick(&candidates, &ours).map(|c| (HWND(c.hwnd as *mut _), c.pid))
}

/// The best match for `ours` among `candidates`: the same path (any case), else the same file name.
fn pick<'a>(candidates: &'a [Candidate], ours: &Path) -> Option<&'a Candidate> {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    let file = |p: &Path| p.file_name().map(|f| f.to_string_lossy().to_lowercase());
    candidates
        .iter()
        .find(|c| c.exe.as_deref().is_some_and(|e| lower(e) == lower(ours)))
        .or_else(|| candidates.iter().find(|c| c.exe.as_deref().is_some_and(|e| file(e).is_some() && file(e) == file(ours))))
}

/// Visible top-level windows whose title is exactly `title`, with their process ids.
fn titled_windows(title: &str) -> Vec<(HWND, u32)> {
    struct Search<'a> {
        title: Vec<u16>,
        found: &'a mut Vec<(HWND, u32)>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Search` that `titled_windows` passed, alive for the whole
        // EnumWindows call, and used by this callback only (EnumWindows calls it on this thread).
        let search = unsafe { &mut *(lparam.0 as *mut Search<'_>) };
        let mut buf = [0u16; 64];
        // SAFETY: `hwnd` is a window EnumWindows gave; the buffer is live and its length is
        // passed. For another process's window, GetWindowText reads the cached caption without
        // sending it a message, so a hung window can't block this.
        let (visible, n) = unsafe { (IsWindowVisible(hwnd).as_bool(), GetWindowTextW(hwnd, &mut buf)) };
        if visible && usize::try_from(n).is_ok_and(|n| buf[..n] == search.title[..]) {
            let mut pid = 0u32;
            // SAFETY: as above; `pid` is a live out-pointer.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            search.found.push((hwnd, pid));
        }
        TRUE
    }
    let mut found = Vec::new();
    let mut search = Search { title: title.encode_utf16().collect(), found: &mut found };
    // SAFETY: `visit` has the signature EnumWindows takes; the LPARAM points at `search`, which
    // outlives the call. EnumWindows doesn't return until it has visited every window.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM((&raw mut search) as isize)) };
    found
}

/// The full Win32 path of process `pid`'s exe (this process's for `None`).
fn process_exe(pid: Option<u32>) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: OpenProcess takes plain arguments, and its handle is ours, closed once by Owned;
    // GetCurrentProcess is a pseudo-handle that needs no closing. The buffer is live and its
    // length (in characters) is passed; on success `len` is the characters written.
    unsafe {
        let owned;
        let process: HANDLE = match pid {
            Some(pid) => {
                owned = Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
                *owned
            }
            None => GetCurrentProcess(),
        };
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()?;
    }
    Some(PathBuf::from(String::from_utf16_lossy(buf.get(..len as usize)?)))
}

/// Restores a minimised window (a maximised one stays maximised) and brings it to the front. A
/// launch from Explorer or the Start menu may take the foreground; from elsewhere Windows may only
/// flash its taskbar button.
fn bring_forward(hwnd: HWND) {
    // SAFETY: plain calls on a window handle; a window that has closed meanwhile makes them fail
    // harmlessly.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn c(hwnd: isize, exe: Option<&str>) -> Candidate {
        Candidate { hwnd, pid: hwnd as u32, exe: exe.map(PathBuf::from) }
    }

    /// The title alone isn't enough (an Explorer window on a "noise-filter" folder has it): the
    /// process's exe decides, the same path first.
    #[test]
    fn the_window_is_matched_by_its_exe() {
        let ours = Path::new(r"C:\Program Files\noise-filter\noise-filter.exe");
        let explorer = c(1, Some(r"C:\Windows\explorer.exe"));
        let other_copy = c(2, Some(r"D:\dev\target\release\noise-filter.exe"));
        let same = c(3, Some(r"c:\program files\NOISE-FILTER\noise-filter.exe"));
        let unknown = c(4, None);
        assert_eq!(pick(&[explorer.clone(), other_copy.clone(), same.clone()], ours), Some(&same));
        assert_eq!(pick(&[explorer.clone(), other_copy.clone()], ours), Some(&other_copy));
        assert_eq!(pick(&[explorer.clone(), unknown], ours), None);
        assert_eq!(pick(&[], ours), None);
    }

    #[test]
    fn this_process_has_an_exe_and_no_noise_filter_window() {
        let me = process_exe(None).expect("this exe's path");
        assert!(me.is_absolute() && me.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")), "{}", me.display());
        assert_eq!(process_exe(Some(std::process::id())), Some(me));
        // A test binary isn't noise-filter.exe, so no real window (or Explorer folder) matches
        assert_eq!(find_window(WINDOW_TITLE), None);
        assert_eq!(find_window("nf-app test: no window has this title"), None);
    }

    /// gui/F5: the first launch owns the mutex; a second one, finding no window, waits and says so;
    /// one that is still waiting when the first exits takes over.
    #[test]
    fn a_second_launch_waits_for_the_first_then_takes_over() {
        let name = format!(r"Local\nf-app-test-instance-{}", std::process::id());
        let title = "nf-app test: no window has this title";
        let Claim::First(Some(first)) = claim(&name, title, Duration::ZERO) else { panic!("the first claim owns it") };
        let (n, t) = (name.clone(), title.to_owned());
        let started = Instant::now();
        let second = std::thread::spawn(move || matches!(claim(&n, &t, Duration::from_millis(300)), Claim::NoWindow));
        assert!(second.join().unwrap(), "no window, so the second launch gives up");
        assert!(started.elapsed() >= Duration::from_millis(300));
        let (tell, got) = mpsc::channel();
        let (n, t) = (name.clone(), title.to_owned());
        let third = std::thread::spawn(move || {
            let claimed = matches!(claim(&n, &t, Duration::from_secs(10)), Claim::First(Some(_)));
            tell.send(claimed).unwrap();
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(first); // the first instance exits
        assert!(got.recv_timeout(Duration::from_secs(5)).unwrap(), "the waiting launch becomes the window");
        third.join().unwrap();
    }
}
