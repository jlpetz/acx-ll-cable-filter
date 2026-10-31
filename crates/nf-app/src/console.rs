//! The console of a windows-subsystem exe (main.rs says why noise-filter is one).
//!
//! A double-click starts the exe with no console, so the window opens alone on every Windows 11
//! build. A CLI command attaches to the console of the shell that started it
//! (`AttachConsole(ATTACH_PARENT_PROCESS)`) and points the standard handles that have nowhere to go
//! at it; output redirected to a file or pipe stays where it is. Started with arguments but no
//! console to attach to (a shortcut, Win+R), a command opens a console of its own and waits for
//! Enter before it closes, so its output can be read. The elevated large-page helper never does:
//! its caller reads only its exit code.
//!
//! Ctrl+C: [`handle_ctrl_c`] turns Ctrl+C, Ctrl+Break and closing the console into
//! [`stop_requested`], so `run`, `latency` and `quality` stop cleanly (`SetConsoleCtrlHandler`).
//! Ctrl+C only arrives if the shell waits for the command. PowerShell doesn't wait for a windowed
//! program: its prompt comes back, and while PSReadLine reads a line it turns the console's
//! processed input off, so Ctrl+C becomes a keystroke. The `noise-filter-cli` launcher (launcher.rs,
//! installable as `noise-filter.com`) is a console program that the shells do wait for.
//! [`CtrlCWatch`] notices the PowerShell case and says so once.

use std::io::BufRead;
use std::os::windows::io::IntoRawHandle;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, TRUE};
use windows::Win32::Storage::FileSystem::{FILE_TYPE, FILE_TYPE_DISK, FILE_TYPE_PIPE, FILE_TYPE_UNKNOWN, GetFileType};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, CONSOLE_MODE, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetStdHandle,
};
use windows::core::BOOL;

/// This process opened a console of its own ([`attach`]), which closes when the process ends. The
/// panic hook reads it.
static ALLOCATED: AtomicBool = AtomicBool::new(false);

/// Whether [`attach`] opened a console of its own.
pub fn allocated() -> bool {
    ALLOCATED.load(SeqCst)
}

/// What [`attach`] found or made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Console {
    /// Attached to the parent's console (a shell, or `cargo run`).
    pub attached: bool,
    /// Opened a console of its own, which closes when the process exits.
    pub allocated: bool,
}

/// Whether a command opens a console of its own: it found none to attach to, its output isn't
/// redirected, and it isn't the elevated helper.
pub fn should_allocate(attached: bool, stdout_redirected: bool, helper: bool) -> bool {
    !attached && !stdout_redirected && !helper
}

/// A file or a pipe: `> out.txt`, `| findstr`, a remote session. A console or NUL isn't.
pub fn is_redirected(file_type: FILE_TYPE) -> bool {
    matches!(file_type, FILE_TYPE_DISK | FILE_TYPE_PIPE)
}

/// Gives a CLI command a console to print to (see the module docs). `helper`: the elevated
/// large-page helper, which never opens one.
pub fn attach(helper: bool) -> Console {
    const STD: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    // Redirected handles, kept across the attach whatever it does to them.
    let redirected = STD.map(|which| std_handle(which).filter(|&(_, t)| is_redirected(t)).map(|(h, _)| h));
    // SAFETY: no pointers; it fails harmlessly when the parent has no console or has exited.
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok();
    // SAFETY: no arguments; it fails if this process already has a console.
    let allocated = should_allocate(attached, redirected[1].is_some(), helper) && unsafe { AllocConsole() }.is_ok();
    ALLOCATED.store(allocated, SeqCst);
    if attached || allocated {
        for (which, kept) in STD.into_iter().zip(redirected) {
            match kept {
                // SAFETY: `h` is the standard handle this process started with, still open.
                Some(h) => drop(unsafe { SetStdHandle(which, h) }),
                None => point_at_console(which),
            }
        }
        // SAFETY: a null handler with `false` restores normal Ctrl+C processing for this process
        // (a parent that ignored Ctrl+C passes that on).
        let _ = unsafe { SetConsoleCtrlHandler(None, false) };
    }
    Console { attached, allocated }
}

/// A usable standard handle and its type: `None` when there's none (a windowed program starts with
/// none) or it isn't open.
fn std_handle(which: STD_HANDLE) -> Option<(HANDLE, FILE_TYPE)> {
    // SAFETY: GetStdHandle takes no pointers. GetFileType only queries the handle; one that isn't
    // open gives FILE_TYPE_UNKNOWN.
    unsafe {
        let h = GetStdHandle(which).ok().filter(|h| !h.is_invalid())?;
        let t = GetFileType(h);
        (t != FILE_TYPE_UNKNOWN).then_some((h, t))
    }
}

/// Points standard handle `which` at the console if it has nowhere to go.
fn point_at_console(which: STD_HANDLE) {
    if std_handle(which).is_some() {
        return;
    }
    let device = if which == STD_INPUT_HANDLE { "CONIN$" } else { "CONOUT$" };
    // Read and write: std's console writes call GetConsoleMode, which needs read access.
    if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(device) {
        // Leaked on purpose: a standard handle lives as long as the process.
        let h = HANDLE(f.into_raw_handle());
        // SAFETY: `h` is an open handle this process owns.
        let _ = unsafe { SetStdHandle(which, h) };
    }
}

/// Before a console this process opened closes with it: wait for Enter.
pub fn pause() {
    say!("\nPress Enter to close this window.");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

static STOP: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);

/// How long closing the console waits for [`finished`]: Windows ends the process about 5 s after
/// the close event.
const CLOSE_GRACE: Duration = Duration::from_millis(4000);

unsafe extern "system" fn on_console_event(kind: u32) -> BOOL {
    STOP.store(true, SeqCst);
    if matches!(kind, CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
        // The process ends when this returns: give the main thread time to stop the engine.
        let step = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        while !FINISHED.load(SeqCst) && waited < CLOSE_GRACE {
            std::thread::sleep(step);
            waited += step;
        }
    }
    TRUE
}

/// Routes Ctrl+C, Ctrl+Break and closing the console to [`stop_requested`].
pub fn handle_ctrl_c() -> bool {
    // SAFETY: the handler is an `extern "system"` fn of the signature the API takes, which lives as
    // long as the process; it only touches atomics and sleeps.
    unsafe { SetConsoleCtrlHandler(Some(on_console_event), true) }.is_ok()
}

/// Ctrl+C was pressed (or the console is closing).
pub fn stop_requested() -> bool {
    STOP.load(SeqCst)
}

/// The flag behind [`stop_requested`], for the engine's blocking tests to watch (`cancel` in their requests).
pub fn stop_flag() -> &'static AtomicBool {
    &STOP
}

/// The command has finished its clean-up: a closing console may end the process now.
pub fn finished() {
    FINISHED.store(true, SeqCst);
}

/// Sleeps up to `d`, returning early (true) once a stop is requested.
pub fn sleep_unless_stopped(d: Duration) -> bool {
    let step = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < d {
        if stop_requested() {
            return true;
        }
        std::thread::sleep(step.min(d - slept));
        slept += step;
    }
    stop_requested()
}

/// Whether the console's input mode lets Ctrl+C through as a signal. `None` when standard input
/// isn't a console (redirected, or none), which says nothing either way.
fn processed_input() -> Option<bool> {
    let (h, _) = std_handle(STD_INPUT_HANDLE)?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: `h` is this process's open standard input; `mode` is a live out-pointer. A handle that
    // isn't a console fails harmlessly.
    unsafe { GetConsoleMode(h, &mut mode) }.ok()?;
    Some(ctrl_c_is_a_signal(mode))
}

/// Ctrl+C is a signal only with ENABLE_PROCESSED_INPUT. Without it, the console delivers it as a
/// keystroke to whoever is reading input.
pub fn ctrl_c_is_a_signal(mode: CONSOLE_MODE) -> bool {
    mode.contains(ENABLE_PROCESSED_INPUT)
}

/// What `run`, `latency` and `quality` print, once, when Ctrl+C can't reach them.
pub const CTRL_C_BLOCKED: &str = "warning: Ctrl+C can't reach noise-filter from this prompt. The shell didn't wait for it \
     (PowerShell doesn't wait for a windowed program) and has taken the console's input back. To stop it cleanly, close \
     this terminal window or tab. Next time, start it as `noise-filter-cli` (or as noise-filter.com, see --help): \
     the shell then waits, and Ctrl+C works.";

/// Notices, once, that Ctrl+C has stopped reaching this process: a check of the console's input mode
/// on the main thread, a cheap call made about once a second.
#[derive(Debug, Default)]
pub struct CtrlCWatch {
    warned: bool,
}

impl CtrlCWatch {
    /// The warning, the first time the console's input mode is found turning Ctrl+C into a key.
    pub fn check(&mut self) -> Option<&'static str> {
        self.saw(processed_input())
    }

    fn saw(&mut self, processed: Option<bool>) -> Option<&'static str> {
        if self.warned || processed != Some(false) {
            return None;
        }
        self.warned = true;
        Some(CTRL_C_BLOCKED)
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_REMOTE};
    use windows::Win32::System::Console::{ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT};

    use super::*;

    /// PSReadLine reading the prompt clears ENABLE_PROCESSED_INPUT (app-privilege F1): that's
    /// warned about once, and a console that delivers Ctrl+C, or no console, isn't.
    #[test]
    fn a_console_that_swallows_ctrl_c_is_warned_about_once() {
        assert!(ctrl_c_is_a_signal(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT));
        assert!(!ctrl_c_is_a_signal(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT));
        assert!(!ctrl_c_is_a_signal(CONSOLE_MODE(0)));
        let mut watch = CtrlCWatch::default();
        assert_eq!(watch.saw(None), None, "stdin redirected: nothing to say");
        assert_eq!(watch.saw(Some(true)), None, "cmd.exe, or a shell that waits");
        assert_eq!(watch.saw(Some(false)), Some(CTRL_C_BLOCKED));
        assert_eq!(watch.saw(Some(false)), None, "only once");
        assert!(CTRL_C_BLOCKED.contains("noise-filter-cli"));
        // The live check runs anywhere (a test's stdin is usually not a console)
        let _ = CtrlCWatch::default().check();
    }

    #[test]
    fn nothing_is_allocated_before_attach() {
        assert!(!allocated());
    }

    #[test]
    fn a_console_is_opened_only_with_nowhere_else_to_print() {
        // A shell's console was attached
        assert!(!should_allocate(true, false, false));
        // Win+R or a shortcut with arguments: no console, no redirection
        assert!(should_allocate(false, false, false));
        // `> out.txt` from a script with no console: the output has a place
        assert!(!should_allocate(false, true, false));
        // The elevated helper: never
        assert!(!should_allocate(false, false, true));
    }

    #[test]
    fn redirection_is_a_file_or_a_pipe() {
        for (t, expected) in
            [(FILE_TYPE_DISK, true), (FILE_TYPE_PIPE, true), (FILE_TYPE_CHAR, false), (FILE_TYPE_UNKNOWN, false), (FILE_TYPE_REMOTE, false)]
        {
            assert_eq!(is_redirected(t), expected, "{t:?}");
        }
    }

    #[test]
    fn sleeping_returns_after_the_time_without_a_stop() {
        let t = std::time::Instant::now();
        assert!(!sleep_unless_stopped(Duration::from_millis(120)));
        assert!(t.elapsed() >= Duration::from_millis(100));
    }
}
