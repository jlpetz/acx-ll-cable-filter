//! noise-filter.exe (doc/design/app.md): the settings window by default, CLI commands otherwise
//! (`noise-filter --help`).
//!
//! # Window or console
//! This is a **windows-subsystem** exe that attaches to its parent's console for CLI commands
//! (`AttachConsole(ATTACH_PARENT_PROCESS)`, `console.rs`), the second option in app.md.
//! - **A double-click** (no arguments, or `gui`) never gets a console, on every Windows 11 build.
//!   TMR's console subsystem with the `consoleAllocationPolicy` manifest only does that from build
//!   26100; 22000-26099 ignore the manifest and flash a console, which app.md rules out.
//! - **From a terminal** a command prints to the terminal's console, and output redirected to a file
//!   or pipe stays redirected. `cargo run -- ...` waits for it, as does a script.
//! - **The cost:** an interactive cmd.exe or PowerShell doesn't wait for a windowed program. The
//!   prompt comes back at once, the output follows it, and `%ERRORLEVEL%` isn't the command's. In
//!   PowerShell, Ctrl+C doesn't reach `run` or `latency` either, because PSReadLine turns it into a
//!   keystroke while it reads the prompt (`run` and `latency` notice and say so,
//!   `console::CtrlCWatch`).
//!   **The fix is `noise-filter-cli.exe`** (launcher.rs), a console-subsystem launcher built next to
//!   this exe. It runs this exe with the same arguments and waits, so the shells wait, Ctrl+C
//!   reaches the command, and the exit code is right. Copied or renamed to `noise-filter.com`, it's
//!   what a plain `noise-filter` in a terminal runs (PATHEXT puts .COM before .EXE), while a
//!   double-click still opens this exe. Without it, `start /wait noise-filter ...` in cmd, or piping
//!   it in PowerShell, waits.
//! - **No console to attach to** (a shortcut or Win+R with arguments): the command opens one and
//!   waits for Enter before it closes; a panic there shows a message box first (logger.rs). The
//!   elevated `setup-large-pages` helper never opens one.
//! - **One window per sign-in session** (instance.rs): a second launch of the window brings the
//!   first one forward and exits, before it locks any memory.
//! - No delay-load or manifest is needed: Windows 11 is the floor (doc/todo/15-gui-tray.md), and
//!   Reactor's bootstrap DLL exists from build 22000.
//!
//! # Start-up order
//! The CPU gate first (cpu_check.rs): a CPU without x86-64-v3's features gets a message box and exit
//! code 5. `main` does only that, and the rest is `app_main`, kept out of it so that its prologue
//! doesn't run first. Then the panic hook and the log, then the console, then, for the window, its
//! single-instance claim. After that, for the commands that run the engine or real-time code
//! (`Command::needs_memory`), before any engine thread exists: the lock-pages privilege, the locked
//! pool (`RtAllocator` serves the audio thread from it) and the working-set minimum.

#![cfg_attr(not(test), windows_subsystem = "windows")]
#![warn(clippy::undocumented_unsafe_blocks)]

/// `println!` that ignores write errors: a closed pipe (`noise-filter devices | more`, quit early)
/// mustn't panic, which would abort.
macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout().lock(), $($t)*);
    }};
}

/// `eprintln!` that ignores write errors.
macro_rules! say_err {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($t)*);
    }};
}

mod account;
mod cli;
mod commands;
mod config;
mod console;
mod cpu_check;
mod exit;
mod health;
mod instance;
mod lab;
mod logger;
mod mutex;
mod quality;
mod report_pipe;
mod wav;

use std::ffi::CStr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use cli::Command;
use exit::{Exit, Failure};
use nf_mem::privilege::PrivilegeError;

/// Every allocation goes through the router: the system heap, except on a thread inside an
/// `RtScope` (the audio thread), which gets the locked pool.
#[global_allocator]
static GLOBAL: nf_mem::RtAllocator = nf_mem::RtAllocator::new();

/// The locked pool: AEC3's construction (about 300 KB) and its per-block allocations, RNNoise, the
/// audio thread's buffers, with room to spare (app.md).
const POOL_BYTES: usize = 32 << 20;
/// Added to the hard working-set minimum, so code, stacks and heaps stay resident. With the `onnx` feature, ONNX Runtime's code
/// and heap (about 20-40 MB) are in it too (TODO 5, 6).
#[cfg(not(feature = "onnx"))]
const WORKING_SET_BYTES: usize = 64 << 20;
#[cfg(feature = "onnx")]
const WORKING_SET_BYTES: usize = 128 << 20;

fn main() -> ExitCode {
    // Before anything else: everything after this may run x86-64-v3 code
    if let Some(code) = cpu_check::gate(Some(cpu_box)) {
        return ExitCode::from(code);
    }
    app_main()
}

/// The CPU gate's box: ANSI, for its ASCII text, because converting it to UTF-16 would take x86-64-v3 code, which the CPUs it is
/// for may not run (cpu_check.rs).
fn cpu_box(text: &CStr) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxA};
    use windows::core::PCSTR;
    let caption = c"noise-filter can't run on this CPU";
    // SAFETY: both strings are NUL-terminated and outlive the call; no owner window.
    unsafe { MessageBoxA(None, PCSTR(text.as_ptr().cast()), PCSTR(caption.as_ptr().cast()), MB_OK | MB_ICONERROR | MB_SETFOREGROUND) };
}

/// Everything after the CPU gate (the start-up order in the module doc). Never inlined into `main`: a large function's prologue
/// saves XMM registers with VEX moves, which a CPU without AVX can't run (cpu_check.rs).
#[inline(never)]
fn app_main() -> ExitCode {
    if let Some(code) = nf_fx::worker_entry() {
        return ExitCode::from(code);
    }
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let parsed = cli::parse(&args);
    let window = matches!(parsed, Ok(Command::Gui));
    // The large-page helper: its caller reads only its exit code, so it never opens a console. It's
    // marked by `--elevated-helper`. An elevated `--sid` run without the flag (a caller that predates
    // it) is treated the same way, as before.
    let helper = match &parsed {
        Ok(Command::SetupLargePages { sid, helper }) => *helper || (sid.is_some() && nf_mem::privilege::is_elevated()),
        _ => false,
    };
    // `mic variant`'s elevated helper reports through its caller's pipe (report_pipe.rs) and takes no console at all: attached
    // to its caller's, it would print its error a second time beside the caller's own.
    let detached = matches!(&parsed, Ok(Command::MicVariant(m)) if m.helper);

    let _ = logger::init(logger::default_path());
    logger::install_panic_hook(window);
    log::info!("noise-filter {} started (pid {}) with {args:?}", env!("CARGO_PKG_VERSION"), std::process::id());
    let console = if window || detached { console::Console::default() } else { console::attach(helper) };

    // One window per session, claimed before the pool is locked; kept until the process exits
    let _instance = if window {
        match instance::claim_window() {
            instance::Claim::First(guard) => guard,
            instance::Claim::Forwarded(pid) => {
                log::info!("the window is already open (pid {pid}): brought it forward");
                return finish(Exit::Ok, console);
            }
            instance::Claim::NoWindow => {
                log::warn!("another instance holds the window, but no window of it showed up");
                message_box(
                    "noise-filter is already running",
                    "noise-filter is already running, but its window didn't come up. If it doesn't appear, end \
                     noise-filter.exe in Task Manager and start it again.",
                );
                return finish(Exit::Error, console);
            }
        }
    } else {
        None
    };

    let exit = match parsed {
        Err(e) => {
            log::warn!("usage error: {e}");
            say_err!("error: {e}\n\nRun `noise-filter --help` for usage.");
            Exit::Usage
        }
        Ok(command) => {
            if command.needs_memory() {
                setup_memory();
            }
            let result = if window { run_window() } else { commands::run_cli(&command) };
            match result {
                Ok(()) => Exit::Ok,
                Err(f) => {
                    log::warn!("{:?}: {}", f.exit, f.message);
                    say_err!("error: {}", f.message);
                    f.exit
                }
            }
        }
    };
    finish(exit, console)
}

/// Logs the exit code, and waits for Enter before a console this process opened closes.
fn finish(exit: Exit, console: console::Console) -> ExitCode {
    log::info!("exit code {}", exit.code());
    log::logger().flush();
    if console.allocated {
        console::pause();
    }
    ExitCode::from(exit.code())
}

/// The privilege, the pool and the working set (app.md "Memory, in this order"). Nothing here is
/// fatal: without large pages the pool is 4 KB pages locked, and the reason is in the status.
fn setup_memory() {
    match nf_mem::privilege::enable_lock_memory_privilege() {
        Ok(()) => log::info!("the lock-pages privilege is enabled"),
        Err(PrivilegeError::NotHeld) => {
            log::info!("no lock-pages privilege: large pages need `noise-filter setup-large-pages` and a new sign-in")
        }
        Err(PrivilegeError::Other(e)) => log::warn!("couldn't enable the lock-pages privilege: {e}"),
    }
    let pool = nf_mem::install_pool(nf_mem::PagePolicy::Large, POOL_BYTES);
    log::info!("real-time pool: {}", pool.describe());
    if let Err(e) = nf_mem::set_working_set_minimum(WORKING_SET_BYTES) {
        log::warn!("couldn't raise the working-set minimum: {e}");
    }
}

/// The settings window, on this (STA) thread until it closes; it stops the engine itself.
fn run_window() -> Result<(), Failure> {
    let settings = nf_gui::default_settings_path().unwrap_or_else(|| PathBuf::from("settings.json"));
    nf_gui::run(Arc::new(nf_engine::Engine::new()), settings).map_err(|e| {
        // Reactor has already shown its own download prompt for a missing Windows App Runtime
        if !e.contains("WindowsAppRuntime") {
            message_box("noise-filter couldn't open its window", &e);
        }
        Failure::error(format!("The window couldn't open: {e}"))
    })
}

/// A modal error box, for what a windowed launch would otherwise never show.
pub fn message_box(caption: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW};
    use windows::core::HSTRING;
    let (text, caption) = (HSTRING::from(text), HSTRING::from(caption));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call; no owner window.
    unsafe { MessageBoxW(None, &text, &caption, MB_OK | MB_ICONERROR | MB_SETFOREGROUND) };
}

/// A fresh, empty folder for one test's files ([`TestDir`]).
#[cfg(test)]
fn test_dir(name: &str) -> TestDir {
    let dir = std::env::temp_dir().join("nf-app-tests").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp folder");
    TestDir(dir)
}

/// A test's folder in %TEMP%: removed when the test drops it, and the shared `nf-app-tests` folder too once it's empty (other
/// tests running meanwhile keep theirs), so test runs don't pile folders up there.
#[cfg(test)]
struct TestDir(PathBuf);

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if let Some(parent) = self.0.parent() {
            // Only once empty: another test's folder may still be in it.
            let _ = std::fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    /// A test's folder is there while the test holds it, and gone when it drops it.
    #[test]
    fn a_test_folder_is_removed_when_dropped() {
        let dir = super::test_dir("cleanup");
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        let path = dir.to_path_buf();
        assert!(path.join("f.txt").exists());
        drop(dir);
        assert!(!path.exists());
    }
}
