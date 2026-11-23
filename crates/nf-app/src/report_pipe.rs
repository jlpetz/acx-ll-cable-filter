//! An elevated helper's report: a named pipe that the process asking for elevation makes, through which the helper sends
//! its steps as lines of text while it works (`mic variant`). The elevated process can't count on its caller's console,
//! and this way the steps go wherever the caller's output goes (a terminal, a file, later the window).
//!
//! The pipe is `\\.\pipe\noise-filter-report-<pid>-<random>`, inbound, one instance (FILE_FLAG_FIRST_PIPE_INSTANCE: a name
//! someone else already holds fails the create), local clients only. Its default security descriptor lets its creator,
//! Administrators and SYSTEM write to it, so the elevated helper may (the same user elevated, or an administrator over the
//! shoulder). The helper opens only a name of that form, never a file, and with SECURITY_IDENTIFICATION, so whoever made the
//! pipe can't act as the elevated helper through it.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, SECURITY_IDENTIFICATION};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::HSTRING;

/// Every report pipe's name starts with this.
pub const PREFIX: &str = r"\\.\pipe\noise-filter-report-";

/// Whether `name` is a report pipe's: the prefix, then the pid and random part (digits, hex and `-`).
pub fn is_report_pipe(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

/// The caller's end: the pipe, and a thread that hands each line the helper writes to a callback.
pub struct ReportServer {
    name: String,
    done: mpsc::Receiver<()>,
}

impl ReportServer {
    /// Makes the pipe and starts its thread, which waits for the helper and passes each line it writes to `on_line`.
    pub fn start(on_line: impl FnMut(&str) + Send + 'static) -> Result<Self, String> {
        let name = format!("{PREFIX}{}-{:016x}", std::process::id(), RandomState::new().hash_one(std::process::id()));
        let wide = HSTRING::from(&name);
        // SAFETY: a NUL-terminated name that outlives the call; default security; the handle returned is ours.
        let handle = unsafe {
            CreateNamedPipeW(
                &wide,
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                64 * 1024,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            // SAFETY: read straight after the failed call.
            let code = unsafe { GetLastError() }.0;
            return Err(format!("CreateNamedPipeW failed: {}", nf_vmic::win_error::code_text(code)));
        }
        // SAFETY: a valid handle that nothing else owns; the File closes it once.
        let pipe = unsafe { File::from_raw_handle(handle.0) };
        let (tx, done) = mpsc::channel();
        std::thread::Builder::new()
            .name("nf-report".into())
            .spawn(move || {
                serve(pipe, on_line);
                let _ = tx.send(());
            })
            .map_err(|e| format!("couldn't start the report's thread ({e})"))?;
        Ok(Self { name, done })
    }

    /// The pipe's name, for the helper's command line.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Once the helper has exited: waits up to `grace` for its last lines. A helper that never connected (the prompt was
    /// declined, or it failed first) leaves the thread waiting for one, so this connects and hangs up itself, which ends it;
    /// when the helper did connect, it took the pipe's only instance and this open fails, harmlessly. `false`: the thread
    /// hadn't finished.
    pub fn finish(self, grace: Duration) -> bool {
        drop(OpenOptions::new().write(true).open(&self.name));
        self.done.recv_timeout(grace).is_ok()
    }
}

/// Waits for a client and passes on its lines until it hangs up.
fn serve(pipe: File, mut on_line: impl FnMut(&str)) {
    // Whatever this says (connected, a client that came first, or one already gone), reading tells: with no client it fails.
    // SAFETY: our pipe handle, open for as long as `pipe` lives; no OVERLAPPED, so the call waits.
    let _ = unsafe { ConnectNamedPipe(HANDLE(pipe.as_raw_handle()), None) };
    for line in BufReader::new(pipe).split(b'\n') {
        let Ok(line) = line else { break };
        on_line(String::from_utf8_lossy(&line).trim_end_matches('\r'));
    }
}

/// The helper's end.
#[derive(Debug)]
pub struct ReportClient(File);

impl ReportClient {
    /// Opens the caller's pipe `name`, which must be a report pipe's ([`is_report_pipe`]).
    pub fn connect(name: &str) -> Result<Self, String> {
        if !is_report_pipe(name) {
            return Err(format!("'{name}' isn't a report pipe's name"));
        }
        OpenOptions::new()
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(name)
            .map(Self)
            .map_err(|e| format!("couldn't open the report pipe {name} ({e})"))
    }

    /// Sends one line, at once (one write; the File isn't buffered). A caller that has gone away is ignored.
    pub fn line(&mut self, text: &str) {
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        let _ = self.0.write_all(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    // In-process only: a pipe of our own, no device and no elevation.

    #[test]
    fn lines_pass_from_the_helper_to_the_caller() {
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&got);
        let server = ReportServer::start(move |line| sink.lock().unwrap().push(line.to_owned())).unwrap();
        assert!(is_report_pipe(server.name()), "{}", server.name());
        let mut client = ReportClient::connect(server.name()).unwrap();
        client.line("Looking for the AudioCodec device");
        client.line(r"  ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working");
        client.line("");
        client.line("Done: é");
        drop(client);
        assert!(server.finish(Duration::from_secs(5)));
        let got = got.lock().unwrap();
        assert_eq!(*got, [
            "Looking for the AudioCodec device",
            r"  ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working",
            "",
            "Done: é"
        ]);
    }

    #[test]
    fn a_helper_that_never_connects_doesnt_hold_the_caller() {
        let server = ReportServer::start(|line| panic!("no helper, but got {line:?}")).unwrap();
        let started = std::time::Instant::now();
        assert!(server.finish(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn only_a_report_pipe_is_opened() {
        for bad in [r"C:\Windows\System32\drivers\etc\hosts", r"\\.\pipe\other", PREFIX, r"\\.\pipe\noise-filter-report-1\..\x"] {
            assert!(!is_report_pipe(bad), "{bad}");
            assert!(ReportClient::connect(bad).unwrap_err().contains("isn't a report pipe"), "{bad}");
        }
        assert!(is_report_pipe(r"\\.\pipe\noise-filter-report-1234-00ff00ff00ff00ff"));
        // A report pipe's name that nobody made: the open fails, and says which pipe
        let e = ReportClient::connect(r"\\.\pipe\noise-filter-report-1-0").err().unwrap();
        assert!(e.starts_with(r"couldn't open the report pipe \\.\pipe\noise-filter-report-1-0"), "{e}");
    }
}
