//! noise-filter-cli.exe: the console launcher for noise-filter.exe in terminals (app-privilege F1).
//!
//! noise-filter.exe is a windows-subsystem program (main.rs says why), and an interactive cmd.exe or
//! PowerShell doesn't wait for one. The prompt comes back at once and the output prints over it.
//! In PowerShell, PSReadLine then reads the prompt with the console's processed input off, so
//! Ctrl+C becomes a keystroke and never stops `run`. And `%ERRORLEVEL%` / `$LASTEXITCODE` isn't
//! the command's.
//!
//! This launcher is a console program, which both shells wait for. It runs the noise-filter.exe in
//! its own folder with the rest of its own command line, untouched, and its own standard handles
//! (`CreateProcessW`, inherited). It ignores Ctrl+C itself: the child, attached to the same console,
//! gets it and stops cleanly. It waits for the child and exits with the child's exit code.
//!
//! # Installing it as `noise-filter.com`
//! Copy (or rename) `noise-filter-cli.exe` to `noise-filter.com` in the folder that holds
//! `noise-filter.exe`. PATHEXT lists `.COM` before `.EXE`, and cmd and PowerShell both follow that
//! order, so a plain `noise-filter run ...` in a terminal then runs the launcher. A double-click, a
//! shortcut or the Start menu still opens noise-filter.exe. A `.com` file that holds a PE image runs
//! like an `.exe` (Visual Studio's `devenv.com` uses the same trick). Don't name it
//! `noise-filter.exe`: it would run itself, so it refuses.
//!
//! # The window, and a console of its own
//! - With no arguments, or `gui`, the child opens the settings window. The launcher starts it
//!   without its standard handles and returns at once, so the prompt comes back.
//! - Started with a console of its own (a double-click on the `.com`, Win+R) rather than a
//!   terminal's, a command's output would vanish when that console closes, so the launcher waits
//!   for Enter first.
//!
//! It's a separate, small program: no real-time allocator, no log.

#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, TRUE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{
    GetConsoleMode, GetConsoleProcessList, GetConsoleWindow, GetStdHandle, CONSOLE_MODE, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows::Win32::System::Environment::GetCommandLineW;
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetExitCodeProcess, INFINITE, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, PWSTR};

/// The program the launcher runs, in its own folder.
const TARGET: &str = "noise-filter.exe";
/// The exit code when the launcher itself fails (noise-filter's 2: an engine, device or file error).
const LAUNCH_FAILED: u32 = 2;

fn main() {
    let code = match launch() {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "noise-filter-cli: {e}");
            LAUNCH_FAILED
        }
    };
    // ExitProcess takes the u32 as is, so a crash code such as 0xC0000409 comes through too
    std::process::exit(code as i32);
}

fn launch() -> Result<u32, String> {
    let me = std::env::current_exe().map_err(|e| format!("couldn't find this program's path ({e})"))?;
    let target = target_for(&me)?;
    if !target.is_file() {
        return Err(format!("{} isn't there: noise-filter-cli runs the noise-filter.exe in its own folder", target.display()));
    }
    // SAFETY: GetCommandLineW takes no arguments. It returns this process's command line,
    // NUL-terminated, which lives as long as the process and which nothing here changes.
    let command_line = unsafe { GetCommandLineW() };
    // SAFETY: as above: a valid NUL-terminated string for the rest of the process.
    let tail = args_tail(unsafe { command_line.as_wide() });
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if opens_window(&args) {
        // The window needs no console: start it as a double-click would, and give the prompt back
        drop(spawn(&target, tail, false)?);
        return Ok(0);
    }
    // Before the child attaches: is this console the launcher's alone?
    let own_console = console_process_count() == 1;
    // SAFETY: the handler is an `extern "system"` fn of the signature the API takes, which lives as
    // long as the process; it only returns.
    unsafe { SetConsoleCtrlHandler(Some(leave_it_to_the_child), true) }
        .map_err(|e| format!("couldn't set a Ctrl+C handler ({e})"))?;
    let child = spawn(&target, tail, true)?;
    let code = wait(&child)?;
    if own_console && can_pause() {
        pause();
    }
    Ok(code)
}

/// Ctrl+C, Ctrl+Break and closing the console: the child, on the same console, gets the same event
/// and stops cleanly, so the launcher only stays out of the way and keeps waiting. Returning TRUE
/// (handled) for a close event still ends the launcher, which is fine: the child has its own
/// handler and grace period.
unsafe extern "system" fn leave_it_to_the_child(_event: u32) -> BOOL {
    TRUE
}

/// noise-filter.exe in the launcher's folder. A launcher that is itself named noise-filter.exe
/// would run itself without end, so that's an error.
fn target_for(me: &Path) -> Result<PathBuf, String> {
    let named_like_target = me.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(TARGET));
    if named_like_target {
        return Err(format!(
            "this launcher is named {TARGET} and would run itself: name it noise-filter-cli.exe or noise-filter.com, next to {TARGET}"
        ));
    }
    Ok(me.with_file_name(TARGET))
}

/// The command line after the program name, as the shell wrote it, so the child splits it into the
/// same arguments. The program name follows `CommandLineToArgvW`'s rule for it: up to the closing
/// quote if it starts with one (no escapes), else up to the first space or tab.
fn args_tail(line: &[u16]) -> &[u16] {
    const QUOTE: u16 = b'"' as u16;
    let blank = |c: u16| c == u16::from(b' ') || c == u16::from(b'\t');
    let end = if line.first() == Some(&QUOTE) {
        line[1..].iter().position(|&c| c == QUOTE).map_or(line.len(), |i| i + 2)
    } else {
        line.iter().position(|&c| blank(c)).unwrap_or(line.len())
    };
    let rest = &line[end..];
    let start = rest.iter().position(|&c| !blank(c)).unwrap_or(rest.len());
    &rest[start..]
}

/// Whether noise-filter opens its window for these arguments (none, or `gui` alone; `cli.rs`).
fn opens_window(args: &[OsString]) -> bool {
    match args {
        [] => true,
        [only] => only == "gui",
        _ => false,
    }
}

/// The child's command line: the quoted target, then `tail` as it came, NUL-terminated.
fn child_command_line(target: &Path, tail: &[u16]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::with_capacity(target.as_os_str().len() + tail.len() + 4);
    line.push(u16::from(b'"'));
    line.extend(target.as_os_str().encode_wide());
    line.push(u16::from(b'"'));
    if !tail.is_empty() {
        line.push(u16::from(b' '));
        line.extend_from_slice(tail);
    }
    line.push(0);
    line
}

/// Starts `target` with `tail` as its arguments. `with_std`: hand it this process's standard
/// handles (inheritable copies), so its output goes where the launcher's does, a console, file or
/// pipe. Returns the process handle.
fn spawn(target: &Path, tail: &[u16], with_std: bool) -> Result<Owned<HANDLE>, String> {
    let app = HSTRING::from(target);
    let mut line = child_command_line(target, tail);
    let mut si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    // Kept open until the child has its own copies
    let mut copies: Vec<Owned<HANDLE>> = Vec::new();
    if with_std {
        si.dwFlags = STARTF_USESTDHANDLES;
        let [input, output, error] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|which| {
            inheritable_std_handle(which).map_or(HANDLE::default(), |h| {
                let raw = *h;
                copies.push(h);
                raw
            })
        });
        (si.hStdInput, si.hStdOutput, si.hStdError) = (input, output, error);
    }
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `app` and `line` are NUL-terminated UTF-16 that outlive the call, and `line` is
    // writable as CreateProcessW requires; `si` is initialised with its size, and its handles are
    // open (or null) until after the call; `pi` is a live out-pointer. On success its two handles
    // are ours: the thread's is closed here, the process's is returned owned.
    unsafe {
        CreateProcessW(
            &app,
            Some(PWSTR(line.as_mut_ptr())),
            None,
            None,
            with_std,
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .map_err(|e| format!("couldn't start {} ({e})", target.display()))?;
        drop(Owned::new(pi.hThread));
        Ok(Owned::new(pi.hProcess))
    }
}

/// An inheritable copy of standard handle `which`, or `None` if there isn't one.
fn inheritable_std_handle(which: STD_HANDLE) -> Option<Owned<HANDLE>> {
    // SAFETY: GetStdHandle takes no pointers. DuplicateHandle copies an open handle of this process
    // within it (GetCurrentProcess is a pseudo-handle); `copy` is a live out-pointer, and the copy
    // is ours, closed once by Owned.
    unsafe {
        let h = GetStdHandle(which).ok().filter(|h| !h.is_invalid())?;
        let mut copy = HANDLE::default();
        DuplicateHandle(GetCurrentProcess(), h, GetCurrentProcess(), &mut copy, 0, true, DUPLICATE_SAME_ACCESS).ok()?;
        Some(Owned::new(copy))
    }
}

/// Waits for the child and returns its exit code.
fn wait(child: &Owned<HANDLE>) -> Result<u32, String> {
    // SAFETY: a valid process handle that we own; INFINITE: the command decides how long it runs.
    if unsafe { WaitForSingleObject(**child, INFINITE) } != WAIT_OBJECT_0 {
        return Err("couldn't wait for noise-filter.exe".into());
    }
    let mut code = 0u32;
    // SAFETY: a valid process handle; `code` is a live out-pointer.
    unsafe { GetExitCodeProcess(**child, &mut code) }.map_err(|e| format!("couldn't read noise-filter.exe's exit code ({e})"))?;
    Ok(code)
}

/// How many processes share this console (0 without one).
fn console_process_count() -> u32 {
    let mut pids = [0u32; 4];
    // SAFETY: the buffer is live and its length is passed; the call only writes into it.
    unsafe { GetConsoleProcessList(&mut pids) }
}

/// A pause can be answered: the console has a window (one made with CREATE_NO_WINDOW has none, and
/// would wait for ever) and standard input is that console.
fn can_pause() -> bool {
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: no pointers but `mode`, a live out-pointer; a standard input that isn't a console
    // fails harmlessly.
    unsafe { !GetConsoleWindow().is_invalid() && GetStdHandle(STD_INPUT_HANDLE).is_ok_and(|h| GetConsoleMode(h, &mut mode).is_ok()) }
}

/// Before a console of the launcher's own closes: wait for Enter.
fn pause() {
    let _ = writeln!(std::io::stdout().lock(), "\nPress Enter to close this window.");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn tail(line: &str) -> String {
        String::from_utf16(args_tail(&wide(line))).unwrap()
    }

    #[test]
    fn the_tail_is_the_command_line_after_the_program_name_as_written() {
        assert_eq!(tail(r#""C:\Program Files\nf\noise-filter.com" run --output "CABLE Input""#), r#"run --output "CABLE Input""#);
        assert_eq!(tail(r#"noise-filter  	 devices"#), "devices");
        assert_eq!(tail("noise-filter"), "");
        assert_eq!(tail(r#""noise filter.com""#), "");
        assert_eq!(tail(""), "");
        // Quotes, backslashes and odd spacing inside the arguments are kept exactly
        assert_eq!(tail(r#"nf.com process "a b.wav" C:\out\ --floor=-20  x"#), r#"process "a b.wav" C:\out\ --floor=-20  x"#);
        // argv[0] ends at its closing quote even with no space after it, as CommandLineToArgvW has it
        assert_eq!(tail(r#""nf.com"run"#), "run");
        // An unterminated quote takes the whole line
        assert_eq!(tail(r#""nf.com run"#), "");
    }

    #[test]
    fn only_no_arguments_or_gui_alone_open_the_window() {
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(opens_window(&a(&[])));
        assert!(opens_window(&a(&["gui"])));
        assert!(!opens_window(&a(&["gui", "x"])), "a usage error, printed to the terminal");
        assert!(!opens_window(&a(&["run"])));
        assert!(!opens_window(&a(&["--help"])));
    }

    #[test]
    fn the_target_is_noise_filter_exe_next_to_the_launcher() {
        assert_eq!(target_for(Path::new(r"C:\nf\noise-filter.com")).unwrap(), PathBuf::from(r"C:\nf\noise-filter.exe"));
        assert_eq!(target_for(Path::new(r"C:\nf\noise-filter-cli.exe")).unwrap(), PathBuf::from(r"C:\nf\noise-filter.exe"));
        assert!(target_for(Path::new(r"C:\nf\Noise-Filter.EXE")).unwrap_err().contains("would run itself"));
    }

    #[test]
    fn the_child_line_quotes_the_target_and_keeps_the_tail() {
        let line = child_command_line(Path::new(r"C:\Program Files\nf\noise-filter.exe"), &wide(r#"run --mic "A B""#));
        assert_eq!(String::from_utf16(&line).unwrap(), "\"C:\\Program Files\\nf\\noise-filter.exe\" run --mic \"A B\"\0");
        let line = child_command_line(Path::new(r"C:\nf\noise-filter.exe"), &[]);
        assert_eq!(String::from_utf16(&line).unwrap(), "\"C:\\nf\\noise-filter.exe\"\0");
    }

    /// The whole path but the target: a console child gets the arguments as written, the
    /// launcher's standard handles, and the launcher returns its exit code. cmd.exe stands in for
    /// noise-filter.exe (tests never run the app).
    #[test]
    fn the_childs_arguments_and_exit_code_come_through() {
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot")).join(r"System32\cmd.exe");
        let child = spawn(&cmd, &wide("/d /c exit 7"), true).unwrap();
        assert_eq!(wait(&child).unwrap(), 7);
        let child = spawn(&cmd, &wide(r#"/d /c "exit 3""#), true).unwrap();
        assert_eq!(wait(&child).unwrap(), 3);
        // Without the standard handles too (the window's way)
        let child = spawn(&cmd, &wide("/d /c exit 0"), false).unwrap();
        assert_eq!(wait(&child).unwrap(), 0);
        assert!(spawn(Path::new(r"C:\no\such\noise-filter.exe"), &[], true).unwrap_err().starts_with("couldn't start"));
    }

    #[test]
    fn console_queries_run_anywhere() {
        let _ = console_process_count();
        let _ = can_pause();
    }
}
