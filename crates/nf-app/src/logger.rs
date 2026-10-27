//! The log file and the panic hook (app.md "Logging").
//!
//! Records go to `%LOCALAPPDATA%\noise-filter\noise-filter.log`, which rotates to `.old` at 1 MiB.
//! The level is capped at Info: wasapi logs at trace on every read, write and padding call. Only
//! non-real-time threads log (the engine never logs from its audio thread), so the logger may
//! allocate, format and take a lock. The window, the CLI and the elevated helper all append to the
//! same file, each line stamped with its process id.
//!
//! Since several processes share the file, rotation is a cross-process step. Under a named mutex,
//! a process re-reads the size on disk and rotates only if the file itself is full. If another
//! process rotated first, the file is small again and is just reopened, so `.old` (the history
//! before a glitch or crash) is never deleted by a second rotation. Before each line a process also
//! checks that the file on disk isn't shorter than its own count. If it is, someone else rotated it
//! and this process's handle points at `.old`, so it reopens the new file.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::mutex::NamedMutex;

/// The size at which the log rotates to `.old`.
pub const MAX_LOG_BYTES: u64 = 1 << 20;

/// How long a rotation waits for another process's rotation to finish (a rename or two, so
/// milliseconds). Past that it re-checks the size and goes ahead without the lock.
const ROTATE_WAIT: Duration = Duration::from_millis(500);

/// The log's usual place: `%LOCALAPPDATA%\noise-filter\noise-filter.log` (the temp folder if
/// `LOCALAPPDATA` isn't set).
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("noise-filter").join("noise-filter.log")
}

/// Installs the file logger at Info. Fails only if a logger is already set.
pub fn init(path: PathBuf) -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(FileLogger::new(path, MAX_LOG_BYTES)))?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(())
}

/// A `log::Log` that appends lines to a file and rotates it.
pub struct FileLogger {
    file: Mutex<LogFile>,
}

impl FileLogger {
    pub fn new(path: PathBuf, limit: u64) -> Self {
        Self { file: Mutex::new(LogFile::open(path, limit)) }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let line = format_line(now(), std::process::id(), thread.name().unwrap_or("-"), record.level(), record.target(), record.args());
        self.file.lock().unwrap_or_else(PoisonError::into_inner).append(&line);
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap_or_else(PoisonError::into_inner).file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// The open log, its size so far, and where it rotates to.
struct LogFile {
    path: PathBuf,
    old: PathBuf,
    file: Option<File>,
    /// The file's length as this process last saw it: its own lines, plus other processes' lines
    /// up to the last check.
    size: u64,
    limit: u64,
    /// The name of the cross-process rotation lock, derived from the path.
    lock: String,
}

impl LogFile {
    fn open(path: PathBuf, limit: u64) -> Self {
        let old = path.with_extension("log.old");
        let lock = crate::mutex::name_for_path("log", &path);
        let mut log = Self { path, old, file: None, size: 0, limit, lock };
        log.reopen();
        log
    }

    /// Appends `line`, rotating first if it would take the file past the limit. Errors are dropped:
    /// a log that can't be written mustn't stop the app.
    fn append(&mut self, line: &str) {
        let len = line.len() as u64;
        self.follow();
        if needs_rotation(self.size, len, self.limit) {
            self.rotate(len);
        }
        if let Some(f) = self.file.as_mut()
            && f.write_all(line.as_bytes()).is_ok()
        {
            self.size += len;
        }
    }

    /// Catches up with other processes. Their lines count towards the size. When the file at the
    /// path isn't the one this process holds open (their sizes differ: another process rotated it,
    /// so this handle points at `.old`, or at a deleted file after a second rotation), it is
    /// reopened. Comparing against this process's own count instead missed a rotation by a process
    /// that had written more since. A log that isn't open is retried.
    fn follow(&mut self) {
        let on_disk = std::fs::metadata(&self.path).map(|m| m.len());
        let held = self.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len());
        match (on_disk, held) {
            (Ok(len), Some(mine)) if len == mine => self.size = len,
            _ => self.reopen(),
        }
    }

    /// `noise-filter.log` becomes `noise-filter.log.old` (replacing the last one) and a new log
    /// starts, unless another process got there first. The size on disk, read under the
    /// cross-process lock, decides: a file another process has just rotated is small again, and is
    /// only reopened.
    fn rotate(&mut self, incoming: u64) {
        let mutex = NamedMutex::open(&self.lock).ok();
        let _held = mutex.as_ref().and_then(|m| m.acquire(ROTATE_WAIT));
        self.file = None;
        let on_disk = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if needs_rotation(on_disk, incoming, self.limit) {
            let _ = std::fs::remove_file(&self.old);
            let _ = std::fs::rename(&self.path, &self.old);
        }
        self.reopen();
    }

    /// Opens (or creates) the log at its path, and takes its real size.
    fn reopen(&mut self) {
        self.file = None;
        self.file = open_append(&self.path);
        self.size = self.file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
    }
}

/// Whether a file of `size` bytes rotates before `incoming` more. A line into an empty file always
/// goes in, however long.
fn needs_rotation(size: u64, incoming: u64, limit: u64) -> bool {
    size > 0 && size + incoming > limit
}

fn open_append(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Local wall-clock time, to the millisecond.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }
}

fn now() -> Stamp {
    // SAFETY: no arguments; it only returns the local time.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    Stamp {
        year: t.wYear,
        month: t.wMonth,
        day: t.wDay,
        hour: t.wHour,
        minute: t.wMinute,
        second: t.wSecond,
        millis: t.wMilliseconds,
    }
}

/// One log line: `2026-10-26 14:03:12.345 [4242 nf-control] WARN nf_engine::control: message`.
pub fn format_line(stamp: Stamp, pid: u32, thread: &str, level: log::Level, target: &str, message: &fmt::Arguments<'_>) -> String {
    format!("{stamp} [{pid} {thread}] {level} {target}: {message}\n")
}

/// Logs a panic and where it happened (with a backtrace, for the dump analysis), and prints it to
/// the console if there is one. If the process is about to end and nothing would keep the panic on
/// screen, it also shows the panic and the log's path in a message box first. That covers the
/// window, and a CLI command that opened its own console (Win+R, a shortcut), since that console
/// closes with the process. See [`shows_panic_box`].
pub fn install_panic_hook(window: bool) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let text = panic_text(thread.name().unwrap_or("unnamed"), info.location(), info.payload());
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!(target: "panic", "{text}\n{backtrace}");
        log::logger().flush();
        let log_path = default_path();
        let _ = writeln!(std::io::stderr(), "{text}");
        let allocated = crate::console::allocated();
        if allocated {
            let _ = writeln!(std::io::stderr(), "The log has the details: {}", log_path.display());
        }
        if shows_panic_box(window, allocated, cfg!(panic = "abort"), thread.name() == Some("main")) {
            crate::message_box("noise-filter has stopped", &format!("{text}\n\nThe log has the details: {}", log_path.display()));
        }
    }));
}

/// Whether a panic is also shown in a message box. That happens when nothing else would keep it
/// on screen (the window, or a console this process opened, which closes with it) and the process
/// is about to end: every panic with `panic = "abort"` (release), otherwise one on the main thread,
/// since an unwinding panic on another thread only ends that thread.
pub fn shows_panic_box(window: bool, console_allocated: bool, aborts: bool, main_thread: bool) -> bool {
    (window || console_allocated) && (aborts || main_thread)
}

/// "panic on thread 'main' at src\x.rs:1:2: message".
pub fn panic_text(thread: &str, location: Option<&std::panic::Location<'_>>, payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    match location {
        Some(l) => format!("panic on thread '{thread}' at {}:{}:{}: {message}", l.file(), l.line(), l.column()),
        None => format!("panic on thread '{thread}': {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_carry_time_process_thread_level_and_target() {
        let stamp = Stamp { year: 2026, month: 10, day: 6, hour: 9, minute: 3, second: 7, millis: 45 };
        let line = format_line(stamp, 4242, "nf-control", log::Level::Warn, "nf_engine::control", &format_args!("restart failed: {}", 7));
        assert_eq!(line, "2026-10-06 09:03:07.045 [4242 nf-control] WARN nf_engine::control: restart failed: 7\n");
        let t = now();
        assert!(t.year >= 2024 && (1..=12).contains(&t.month) && t.millis < 1000, "{t}");
    }

    #[test]
    fn the_log_appends_and_rotates_to_old() {
        let dir = crate::test_dir("logger");
        let path = dir.join("sub").join("noise-filter.log");
        let mut log = LogFile::open(path.clone(), 100);
        log.append(&"a".repeat(60));
        log.append(&"b".repeat(30));
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 90);
        // Past the limit: the old file moves aside and a new one starts with this line
        log.append(&"c".repeat(20));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "c".repeat(20));
        assert_eq!(std::fs::read_to_string(dir.join("sub").join("noise-filter.log.old")).unwrap().len(), 90);
        // A second rotation replaces the old one
        log.append(&"d".repeat(90));
        assert_eq!(std::fs::read_to_string(dir.join("sub").join("noise-filter.log.old")).unwrap(), "c".repeat(20));
        // A reopened log continues from its size
        drop(log);
        let mut log = LogFile::open(path.clone(), 100);
        assert_eq!(log.size, 90);
        log.append("e");
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 91);
        // A line longer than the limit still goes in
        log.append(&"f".repeat(300));
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 300);
        drop(log);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two processes share the log (app-privilege F4). One rotates. The other, whose count is stale
    /// and whose handle now points at `.old`, mustn't rotate again and delete the history.
    #[test]
    fn a_second_process_doesnt_rotate_away_the_firsts_rotation() {
        let dir = crate::test_dir("logger-shared");
        let path = dir.join("noise-filter.log");
        let old = dir.join("noise-filter.log.old");
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let mut a = LogFile::open(path.clone(), 100);
        a.append(&"a".repeat(60));
        let mut b = LogFile::open(path.clone(), 100);
        assert_eq!(b.size, 60);
        b.append(&"b".repeat(30));
        // B fills the file and rotates it
        b.append(&"c".repeat(20));
        assert_eq!(read(&old), format!("{}{}", "a".repeat(60), "b".repeat(30)));
        assert_eq!(read(&path), "c".repeat(20));
        // A counted only its own 60 bytes, and 60 + 50 is past the limit. It follows the new file
        // instead of rotating it, which would delete .old and push B's line into it
        a.append(&"d".repeat(50));
        assert_eq!(read(&old), format!("{}{}", "a".repeat(60), "b".repeat(30)), "the history survives");
        assert_eq!(read(&path), format!("{}{}", "c".repeat(20), "d".repeat(50)));
        assert_eq!((a.size, b.size), (70, 20));
        // B catches up with A's line before deciding: 70 + 40 rotates once, both lines going to .old
        b.append(&"e".repeat(40));
        assert_eq!(read(&old), format!("{}{}", "c".repeat(20), "d".repeat(50)));
        assert_eq!(read(&path), "e".repeat(40));
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Both processes decided to rotate at the same moment. The second, under the lock, sees the
    /// small new file and only reopens it.
    #[test]
    fn a_rotation_rechecks_the_size_on_disk() {
        let dir = crate::test_dir("logger-recheck");
        let path = dir.join("noise-filter.log");
        let old = dir.join("noise-filter.log.old");
        let mut a = LogFile::open(path.clone(), 100);
        let mut b = LogFile::open(path.clone(), 100);
        a.append(&"a".repeat(95));
        b.follow();
        assert_eq!(b.size, 95);
        a.append(&"x".repeat(10)); // rotates: .old holds the 95 a's
        b.rotate(10); // decided on the stale 95, so it must not rotate again
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "a".repeat(95));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x".repeat(10));
        assert_eq!(b.size, 10);
        assert!(needs_rotation(95, 10, 100) && !needs_rotation(0, 300, 100) && !needs_rotation(90, 10, 100));
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_deleted_log_is_recreated() {
        let dir = crate::test_dir("logger-deleted");
        let path = dir.join("noise-filter.log");
        let mut log = LogFile::open(path.clone(), 100);
        log.append("one\n");
        drop(log.file.take());
        std::fs::remove_file(&path).unwrap();
        log.append("two\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        drop(log);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A panic in a command that opened its own console is shown before that console closes with
    /// the process (app-privilege F5), the same as one in the window.
    #[test]
    fn panics_get_a_box_only_when_nothing_else_would_show_them() {
        // (window, console allocated, panic = abort, main thread)
        assert!(shows_panic_box(true, false, true, false), "the window, release: any thread");
        assert!(shows_panic_box(false, true, true, false), "Win+R console, release: any thread");
        assert!(shows_panic_box(false, true, false, true), "Win+R console, debug: the main thread ends the process");
        assert!(!shows_panic_box(false, true, false, false), "debug: another thread's panic doesn't end it");
        assert!(!shows_panic_box(false, false, true, true), "a terminal's console stays open");
        assert!(shows_panic_box(true, false, false, true));
    }

    #[test]
    fn the_logger_honours_the_max_level() {
        use log::Log;
        let dir = crate::test_dir("logger-level");
        let logger = FileLogger::new(dir.join("l.log"), MAX_LOG_BYTES);
        // The global max level is Off until `init`, and tests never call it
        let off = log::max_level();
        let record = log::Record::builder().level(log::Level::Error).target("t").args(format_args!("x")).build();
        logger.log(&record);
        logger.flush();
        let written = std::fs::read_to_string(dir.join("l.log")).unwrap_or_default();
        assert_eq!(written.is_empty(), off < log::Level::Error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn panic_text_reads_str_and_string_payloads() {
        let here = std::panic::Location::caller();
        let s: Box<dyn std::any::Any + Send> = Box::new("boom");
        let text = panic_text("audio", Some(here), s.as_ref());
        assert!(text.starts_with("panic on thread 'audio' at ") && text.ends_with(": boom"), "{text}");
        let s: Box<dyn std::any::Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_text("main", None, s.as_ref()), "panic on thread 'main': owned");
        let s: Box<dyn std::any::Any + Send> = Box::new(7u32);
        assert_eq!(panic_text("main", None, s.as_ref()), "panic on thread 'main': (no message)");
    }

    #[test]
    fn the_default_path_is_under_localappdata() {
        let p = default_path();
        assert!(p.ends_with(r"noise-filter\noise-filter.log"), "{}", p.display());
    }
}
