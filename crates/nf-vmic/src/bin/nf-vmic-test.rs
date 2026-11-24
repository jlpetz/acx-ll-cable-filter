//! nf-vmic-test: spike check 4's tool for the driver's app-fed capture ring (TODO 19; driver/README.md, "App feed").
//!
//! - `feed` attaches a ring and writes the test signal, paced by the driver's `read_frame`, until Ctrl+C.
//! - `verify` records "Microphone (AudioCodec Device)" (shared mode, float32 stereo 48 kHz, RAW where offered, or the processing
//!   mode and IAudioClient3 period asked for) while a feed runs, and passes only with proof that the app ring fed the recording:
//!   the driver's statistics just before and just after it (the same attachment, read throughout, its frames copied), and the
//!   signal bit-exact on both channels and in order, with silence only where the driver counted underruns, the reader's or a
//!   secondary stream's (one packet's slack at each end of the recording), and each gap the driver's own record of an allowed
//!   skip in its skip log, by the ring frames the codes either side name, with the log's skips and jumps adding up to the
//!   counters' rise.
//! - `stats` prints the driver's NF_VMIC_IOCTL_STATS: the ring's counters (its refusals and its secondary delivery among them),
//!   the loopback's, each stream kind's timer lateness histogram (spike check 5), and the skip and underrun logs.
//! - `kill-test` starts `feed` as a child and terminates it mid-stream K times (a capture stream reading its ring), checking
//!   from STATS that each attachment ended through the cancel path within a bound, and that the capture timer kept running.
//! - `open` opens an endpoint in one format (48 kHz stereo float32, or 24-in-32 PCM), exclusive or shared, prints what
//!   IsFormatSupported and Initialize returned by name, and holds the stream for N seconds: the format-refusal tests in either
//!   open order, and a small-period helper. A refusal is the driver's only where its RefusedCaptureOpens rose (exit 1); any
//!   other exits 4. A float render stream can play the test signal (`--signal`), so a float capture of the microphone, which
//!   decodes it and counts its gaps and repeats, checks the loopback bit-exact.
//! - `latency` attaches a ring, feeds it at a fill target and records the capture endpoint in the same process, timing each
//!   frame from the feed's publication to its receipt (the host's QPC when GetBuffer returned its packet, and the frame's
//!   device time: GetBuffer's device QPC position plus its place in the packet), and checks that its own stream read the
//!   ring rather than a secondary's late copy; [`nf_vmic::latency`] has the arithmetic.
//! - `stamps` records a capture endpoint and, after each GetBuffer, reads the driver's capture-timestamp trace from STATS, to
//!   set the device time the client sees beside the packet times the driver gave ACX (TODO 19).
//!
//! The spike's control device admits SYSTEM and Administrators only: run it elevated. The owner runs it (CLAUDE.md); no test
//! starts it. Its feed loop prints a status line a second, which formats and allocates: a test tool's liberty, with the ring's
//! 20 ms of fill (the default target) as the margin. `latency` prints nothing while it measures, and its feed and capture
//! threads allocate nothing per block or packet once they run: their logs are sized from `--seconds` before the start.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::BTreeSet;
use std::fmt::Display;
use std::marker::PhantomData;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{io, thread};

use nf_vmic::latency::{self, Block, Continuity, FrameCounts, Log, Receipt, Run, Summary, Weighted};
use nf_vmic::protocol::{
    self, CaptureTrace, EVENT_HANDOFF_SKIP, EVENT_LATE_READ, EVENT_LIVE_DISCARD, EVENT_LOOPBACK_LEAD_SKIP, EVENT_RESYNC,
    EVENT_SECONDARY_JUMP, EVENT_SECONDARY_SKIP, EVENTF_BACKWARD, EVENTF_BEHIND, EVENTF_GAP, EVENTF_OLD, EVENTF_PAUSING, EVENTF_PENDING,
    EVENTF_READER, EVENTF_UNTRACKED, Event, EventLog, KIND_CAPTURE, KIND_RENDER, LATENESS_LABELS, RingHeader, SKIP_LOG_ENTRIES,
    STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_READING, STATS_ATTACHED, STATS_FORMAT_MISMATCH, STATS_LOOPBACK, STATS_READING, Stats,
    StreamStats,
};
use nf_vmic::signal::{self, CheckReport, SequenceChecker};
use nf_vmic::{AttachError, Attachment, Device, RingMemory, Writer, top_up};
use wasapi::{
    AudioClient, AudioClientProperties, DeviceEnumerator, Direction, Handle, SampleType, StreamCategory, StreamMode, StreamOption,
    TimingMode, WasapiError, WaveFormat, calculate_period_100ns,
};
use windows::Win32::Foundation::{E_FAIL, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, FALSE, HANDLE, TRUE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    ENDPOINT_HARDWARE_SUPPORT_VOLUME, IAudioCaptureClient, IAudioRenderClient, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoTaskMemFree};
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW,
    SetWaitableTimer, TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{BOOL, HRESULT, Owned, PCWSTR, w};

/// The capture endpoint the driver's ring feeds (spike check 1's name).
const ENDPOINT: &str = "Microphone (AudioCodec Device)";
/// How long verify waits, once its stream runs, for the driver to report a capture stream reading the ring.
const READING_WAIT: Duration = Duration::from_secs(2);
/// How long verify waits after the recording, where no stream holds the reader slot (the last reader stopped as it ended), for
/// one to hold it again: its own stream still runs, and takes the slot at its next pass.
const VACANCY_WAIT: Duration = Duration::from_millis(200);
/// kill-test: how long a child may take to attach; the window over which its ring must be read before the kill; how long it
/// may take to exit after TerminateProcess, and its attachment to end after that; and how long the guard waits for it.
const ATTACH_WAIT: Duration = Duration::from_secs(5);
const READ_WINDOW: Duration = Duration::from_millis(200);
const EXIT_WAIT: Duration = Duration::from_secs(2);
const DETACH_WAIT: Duration = Duration::from_secs(1);
const GUARD_WAIT: Duration = Duration::from_secs(10);

const USAGE: &str = "\
nf-vmic-test: the driver's app-fed capture ring (TODO 19, spike check 4). Run it elevated.

  nf-vmic-test feed [--seconds N] [--capacity FRAMES] [--target FRAMES] [--start N] [--quiet]
      Attach a ring and write the test signal (frame n carries the code n mod 2^23, a different float32 on each channel),
      paced by the driver's read_frame, until Ctrl+C (or for N seconds; default 0, until stopped). The capacity is a power
      of two from 1024 to 262144 frames (default 8192). The target is the fill kept, in frames (default 960, 20 ms): at least
      one capture packet (480 frames for a shared stream's 10 ms period), or every pass underruns; the feed ends with an error
      once the reader's packet shows the target is below it. --start: the first frame's number. A refusal says why:
      ERROR_NOT_SUPPORTED (50) means a capture stream is open in a format other than 48 kHz float32 stereo.
  nf-vmic-test verify [--seconds N] [--device NAME] [--mode raw|default|communications] [--period-frames N] [--start N]
      Start `feed` first, in another window, and leave it running. Then record the capture endpoint (default \"Microphone
      (AudioCodec Device)\") in shared mode, float32 stereo 48 kHz, for N seconds (default 10), and check that the app ring
      fed it: the driver's statistics just before and after (the same attachment, read throughout, its frames copied), and
      the signal bit-exact on both channels and in order, with silence only where the driver underran (the reader, or a
      secondary stream; one packet's slack at each end). The recording starts once the stream delivers the signal (2 s at
      most). --mode: the processing mode the stream asks for (default: RAW where the endpoint offers it, else DEFAULT).
      --period-frames: open through IAudioClient3's InitializeSharedAudioStream at that engine period, in frames (a
      small-period helper, or a second client in another mode); the period granted is printed. --start: the feed's
      --start (default 0), so the codes name the ring's frames. Unmute the endpoint. The first capture stream to run
      reads the ring; the driver gives the others its audio as secondary streams. When the reader stops, another takes the
      slot with one skip; the rest play on with no repeat, getting silence where the new reader's packet is longer and
      skipping once where it is shorter. Each gap must be the driver's own record of one of those two skips: the codes
      either side name the ring's frames it lost, and the driver's skip log (in STATS, read before and after) must hold an
      entry for exactly those frames, or a chain of one stream's entries end to end (any stream's: verify doesn't know its
      own), each a handoff's skip or a skip to a shorter lag. A gap with no entry, or with one for its first frame that
      doesn't end with it, fails; so does one tied to a late read (a read past the copy's end: silence, and the frames the
      reader then wrote there never reach it), a jump in a stream's own time line, or a resync (the feed's write position
      invalid): the log's record of a loss, not a skip the ring's rules allow. A late read or a jump that no gap of the
      recording names is listed, diagnostic only. Where entries of more than one stream tie a gap, each is listed (stream,
      kind, frames, ring frames), and the gap takes their verdict where they agree. An entry still being filled in just
      after the recording is no gap's record (a stream's own entries for a gap are settled before it reads the code after
      it). INCONCLUSIVE only where what the log can't say would change the verdict: candidates for one gap judged
      differently, or an untracked entry (its ring frames may be short: the driver's records ran out) that may hold a
      gap's frames and would judge it otherwise; for a gap no chain ties, events of the recording no longer in the log (it
      keeps 64) or unreadable, any of which may be an allowed skip for it; and the log's skips and time-line jumps over the
      recording, which must add up to the driver's counters' rise, where events of it are no longer in the log. The
      capture stream's own losses (frames the engine dropped, discontinuities) count up to the recording's end (those
      before its first packet told apart): those of the drain after it (the final STATS read, the reader slot's wait) are
      listed, as information. Where no stream holds the reader slot just after the recording (the last reader stopped as
      it ended), verify waits up to 200 ms for one to (its own stream takes it at its next pass).
  nf-vmic-test stats [--time N]
      Print the driver's statistics, and its skip and underrun logs (newest last, times before the snapshot, each entry's
      stream by its serial). --time N: read STATS N times back to back instead and print how long each read took (min,
      median, p99, max in us): a read on a capture thread costs it that much of its buffer.
  nf-vmic-test kill-test [--kills K] [--min-ms MIN] [--max-ms MAX] [--device NAME] [--no-capture]
      K times (default 10): start `feed` as a child, check that a capture stream reads its ring, terminate the child MIN to
      MAX milliseconds after it attached (default 300 to 2000; MIN at least 200), and check that it exited within 2 s and its
      attachment ended through the cancel path within 1 s of that. This tool's own capture stream reads the endpoint
      throughout, and its timer must still run after the last kill. --no-capture leaves the reading to another capture
      client: without one the result is INCONCLUSIVE.
  nf-vmic-test open --device NAME [--render] (--exclusive | --shared) --format float|pcm24 [--period-frames N]
                    [--mode raw|default|communications] [--seconds N] [--signal [--start N]]
      Open the endpoint (capture, or render with --render) in that format, event-driven: 48 kHz stereo float32, or 24-in-32
      PCM (a 32-bit container with 24 valid bits). Print IsFormatSupported's result and Initialize's HRESULT by name (or
      that of the call after it that failed, up to Start), then hold the stream running for N seconds (default 10; Ctrl+C
      ends it sooner): render writes silence, or with --signal (float only) the test signal as feed writes it, from frame
      N (--start, default 0); capture counts packets and silent packets, and for float decodes the test signal and counts
      its codes, gaps and repeats (the loopback's check, with no app ring attached and a --signal render stream on the
      speakers). --period-frames: exclusive, the period (default: the device's default period); shared, an IAudioClient3
      engine period. --mode: the processing mode (default: DEFAULT). Shared mode converts nothing, so a format other than
      the endpoint's mix format is refused. The driver's statistics before and after are printed where
      its control device opens. Exit 0: it opened and ran. 1: the driver refused it (a capture stream while a ring is
      attached, in a format other than 48 kHz float32 stereo: its RefusedCaptureOpens rose), the expected result in some
      tests. 4: refused, but not shown to be by that rule: the outcome line names the call and its HRESULT, and says
      whether the counter rules the driver's ring rule out (another client holding the device, a bad period, the engine)
      or can't tell (no driver statistics, or a render endpoint). 3: it opened and started, then stopped running.
  nf-vmic-test latency [--seconds N] [--target FRAMES] [--capacity FRAMES] [--period-frames N] [--mode raw|default|communications]
                       [--device NAME] [--exclusive]
      In one process: attach a ring, write the test signal at the fill target (default 960 frames; capacity default 8192)
      as feed does, and record the capture endpoint (shared, float32 stereo 48 kHz; --mode and --period-frames as for
      verify; --exclusive: an exclusive event-driven stream, --period-frames its period, so the device times are the
      driver's own packet stamps rather than the shared engine's). After a 1 s warm-up, for N seconds (default 10, at most 600), time every frame from the store of write_frame
      that published it to its receipt: by the host's QPC when GetBuffer returned its packet, and by its device time
      (GetBuffer's device QPC position, its packet's first frame's, plus its place in the packet): p50, p90, p99 and max
      in ms for each, with the fill the feed found before each top-up, the underruns and the period granted. Stop any
      other feed first (the ring must be this process's), and close other apps that record the endpoint: a capture stream
      already running holds the ring's reader slot, and unless it shares this one's driver stream (shared, the same mode)
      this one gets the ring's audio late, as a secondary (a check fails).
  nf-vmic-test stamps [--packets N] [--resumes N] [--period-frames N] [--mode raw|default|communications] [--device NAME]
                      [--exclusive]
      Record the capture endpoint (as latency does; no ring needed) for N packets (default 200) past a 0.5 s warm-up, then
      stop and start the same client N times (--resumes, default 8), recording 4 packets after each: each Stop waits 30%
      to 170% of a packet after the last read (past 100%, the driver completes a packet the client hasn't read), each
      pause lasts 3 to 50 ms. After each GetBuffer it reads the driver's capture-timestamp trace from STATS, and prints
      the client's device time minus the packet start the driver last answered GetCapturePacket with, minus the
      completion time it last notified (any capture stream's: run one), and minus the query's own time, the calls per
      packet, the last rows, and each resume's first two packets beside the driver's notifications. A stamp off the
      driver's packet grid among a resume's leading packets, before the first the driver stamped after that Start (the
      shared engine's own, or one left unread at Stop), is Windows': left out and listed. PASS: every packet has a device
      time, each of the driver's stamps steps by its packet's length while the stream runs (by device position) and,
      across each resume, by at least the frames between; FAIL otherwise, or for any other stamp off the grid (in the
      first run, or after the driver's first packet since a Start: listed with its offset); INCONCLUSIVE when a resume's
      step couldn't be compared, or the grid couldn't be found from the first run's notifications (too few packets).
  nf-vmic-test idle-test [--device NAME]
      Start an exclusive recorder twice on a live ring. Check that samples queued before each open are discarded and new
      samples reach the recorder. Requires an idle virtual capture endpoint.

Exit codes: 0 pass, 1 fail, 2 a usage or set-up error, 3 inconclusive (open: 0 opened and ran, 1 refused by the
driver's app ring rule, 4 refused otherwise, 3 stopped).";

fn main() -> ExitCode {
    let Ok(args) = std::env::args_os().skip(1).map(|a| a.into_string()).collect::<Result<Vec<String>, _>>() else {
        eprintln!("nf-vmic-test: an argument isn't valid Unicode\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("feed") => feed(rest),
        Some("verify") => verify(rest),
        Some("stats") => stats(rest),
        Some("kill-test") => kill_test(rest),
        Some("open") => open(rest),
        Some("latency") => latency(rest),
        Some("stamps") => stamps(rest),
        Some("idle-test") => idle_test(rest),
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => Err(format!("which mode?\n\n{USAGE}")),
    };
    match result {
        Ok(Verdict::Pass) => ExitCode::SUCCESS,
        Ok(Verdict::Fail) => ExitCode::from(1),
        Ok(Verdict::Inconclusive) => ExitCode::from(3),
        Ok(Verdict::RefusedElsewhere) => ExitCode::from(4),
        Err(e) => {
            eprintln!("nf-vmic-test: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    /// Nothing failed, but what was checked doesn't prove the point (kill-test with nothing reading the ring).
    Inconclusive,
    /// open: the stream was refused, but not shown to be by the driver's app ring format rule (its RefusedCaptureOpens didn't
    /// rise, or can't be read): exit 4, so a test that expects that refusal can't pass on another.
    RefusedElsewhere,
}

/// A mode's options, as `parse` found them: `--name value` pairs and bare flags, each one the mode knows, each at most once.
#[derive(Debug)]
struct Opts<'a> {
    values: Vec<(&'a str, &'a str)>,
    flags: Vec<&'a str>,
}

impl<'a> Opts<'a> {
    /// `valued` options take the next argument as their value, which mustn't itself look like an option (`--`).
    fn parse(args: &'a [String], valued: &[&str], flags: &[&str]) -> Result<Self, String> {
        let mut o = Self { values: Vec::new(), flags: Vec::new() };
        let mut i = 0;
        while let Some(a) = args.get(i).map(String::as_str) {
            if o.flags.contains(&a) || o.values.iter().any(|(name, _)| *name == a) {
                return Err(format!("{a} is given twice"));
            }
            if flags.contains(&a) {
                o.flags.push(a);
                i += 1;
            } else if valued.contains(&a) {
                match args.get(i + 1).map(String::as_str) {
                    None => return Err(format!("{a} needs a value")),
                    Some(v) if v.starts_with("--") => return Err(format!("{a} needs a value, not the option {v}")),
                    Some(v) => o.values.push((a, v)),
                }
                i += 2;
            } else {
                return Err(format!("unknown option {a:?}\n\n{USAGE}"));
            }
        }
        Ok(o)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    fn value<T: FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        Ok(self.optional(name)?.unwrap_or(default))
    }

    /// The option's value, or `None` where it isn't given.
    fn optional<T: FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.values.iter().find(|(n, _)| *n == name) {
            None => Ok(None),
            Some((_, v)) => v.parse().map(Some).map_err(|_| format!("{name}: can't read {v:?}")),
        }
    }

    /// `--mode raw|default|communications`, or `default` where it isn't given.
    fn mode(&self, default: Mode) -> Result<Mode, String> {
        match self.optional::<String>("--mode")?.as_deref() {
            None => Ok(default),
            Some("raw") => Ok(Mode::Raw),
            Some("default") => Ok(Mode::Default),
            Some("communications") => Ok(Mode::Communications),
            Some(other) => Err(format!("--mode {other:?}: raw, default or communications")),
        }
    }

    /// `--period-frames N`: an engine period or an exclusive period, from 1 to a second's frames.
    fn period_frames(&self) -> Result<Option<u32>, String> {
        match self.optional::<u32>("--period-frames")? {
            Some(n) if n == 0 || n > protocol::SAMPLE_RATE => Err(format!("--period-frames {n}: from 1 to {}", protocol::SAMPLE_RATE)),
            other => Ok(other),
        }
    }
}

/// A verdict's checks, each printed with its numbers as it is made.
#[derive(Default)]
struct Checks {
    failed: u32,
    inconclusive: u32,
}

impl Checks {
    fn check(&mut self, ok: bool, name: &str, detail: impl Display) -> bool {
        self.judge(ok.into(), name, detail) == Outcome::Ok
    }

    /// Prints the check under a four-wide label, so the names line up: `ok`, `FAIL`, or `INC?` (inconclusive: its detail says
    /// why).
    fn judge(&mut self, outcome: Outcome, name: &str, detail: impl Display) -> Outcome {
        let label = match outcome {
            Outcome::Ok => "ok  ",
            Outcome::Inconclusive => "INC?",
            Outcome::Fail => "FAIL",
        };
        println!("  {label} {name}: {detail}");
        self.failed += u32::from(outcome == Outcome::Fail);
        self.inconclusive += u32::from(outcome == Outcome::Inconclusive);
        outcome
    }
}

/// A check's outcome: passed, failed, or neither (what the driver counts can't tie the recording's numbers to it either way).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Inconclusive,
    Fail,
}

impl From<bool> for Outcome {
    fn from(ok: bool) -> Self {
        if ok { Self::Ok } else { Self::Fail }
    }
}

/// One check of a verdict, made by a pure function (so the tests can make it too): its outcome, its name, its numbers.
#[derive(Debug)]
struct Check {
    outcome: Outcome,
    name: &'static str,
    detail: String,
}

/// The worse of two outcomes: a failure over an undecided one over a pass.
fn worse(a: Outcome, b: Outcome) -> Outcome {
    match (a, b) {
        (Outcome::Fail, _) | (_, Outcome::Fail) => Outcome::Fail,
        (Outcome::Inconclusive, _) | (_, Outcome::Inconclusive) => Outcome::Inconclusive,
        _ => Outcome::Ok,
    }
}

/// A verdict from its checks: FAIL if any failed, INCONCLUSIVE if none failed but one couldn't be decided, PASS otherwise.
fn verdict_of(checks: &[Check]) -> Verdict {
    if checks.iter().any(|c| c.outcome == Outcome::Fail) {
        Verdict::Fail
    } else if checks.iter().any(|c| c.outcome == Outcome::Inconclusive) {
        Verdict::Inconclusive
    } else {
        Verdict::Pass
    }
}

fn open_device() -> Result<Device, String> {
    let path = protocol::USER_PATH;
    Device::open().map_err(|e| match e.raw_os_error().map(|c| c as u32) {
        // The driver deletes the symbolic link with the control device, so this is also what an open gets while its PnP device
        // is gone (removed, disabled or being updated): nf_vmic.h, STATUS_DEVICE_REMOVED.
        Some(c) if c == ERROR_FILE_NOT_FOUND.0 => {
            format!("{path} isn't there ({e}): is the driver installed and started, in a build with the app feed, and its device enabled?")
        }
        Some(c) if c == ERROR_ACCESS_DENIED.0 => {
            format!("{path} refused access ({e}): the spike's control device admits SYSTEM and Administrators only; run elevated")
        }
        _ => format!("can't open {path}: {e}"),
    })
}

fn read_stats(device: &Device) -> Result<Stats, String> {
    device.stats().map_err(|e| match e.raw_os_error().map(|c| c as u32) {
        Some(c) if c == protocol::QUEUE_PURGED.win32 => format!("STATS: {e} ({})", protocol::QUEUE_PURGED.meaning),
        _ => format!("STATS: {e}"),
    })
}

/// Why an ATTACH failed, in words where the code says: every refusal and every failure at once nf_vmic.h lists, told apart by
/// whether the request went pending first.
fn attach_hint(e: &AttachError) -> String {
    match e.error.raw_os_error().and_then(|c| protocol::refusal(c as u32, e.pending)) {
        Some(o) => format!(" ({}: {})", o.name, o.meaning),
        None => String::new(),
    }
}

/// How an attached ring's request ended, in words where the code says.
fn end_text(e: &io::Error) -> String {
    match e.raw_os_error().and_then(|c| protocol::ending(c as u32)) {
        Some(o) => format!("{e} ({}: {})", o.name, o.meaning),
        None => e.to_string(),
    }
}

fn ms(frames: u64) -> f64 {
    frames as f64 * 1000.0 / f64::from(protocol::SAMPLE_RATE)
}

fn us(hns: u64) -> f64 {
    hns as f64 / 10.0
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}

/// The reader state as the header's State bits, from either the header or STATS (whose flags mirror them).
fn stats_state(s: &Stats) -> u32 {
    let mut state = 0;
    for (stats_bit, state_bit) in
        [(STATS_ATTACHED, STATE_ATTACHED), (STATS_READING, STATE_READING), (STATS_FORMAT_MISMATCH, STATE_FORMAT_MISMATCH)]
    {
        if s.flags & stats_bit != 0 {
            state |= state_bit;
        }
    }
    state
}

/// The ring's state (the header's State and PacketFrames, or STATS' flags and ReaderPacketFrames), in words.
fn state_text(state: u32, packet_frames: u32) -> String {
    if state & STATE_ATTACHED == 0 {
        "not attached".into()
    } else if state & STATE_READING != 0 {
        format!("attached, read in {packet_frames}-frame packets")
    } else if state & STATE_FORMAT_MISMATCH != 0 {
        let packet = if packet_frames != 0 { format!(", {packet_frames}-frame packets in its format") } else { String::new() };
        format!(
            "attached, FORMAT MISMATCH: the capture stream holding the ring isn't 48 kHz float32 stereo{packet} (its packets are \
             silence, the ring isn't read)"
        )
    } else if packet_frames != 0 {
        // nf_vmic.h: a stream holding the slot always wrote one of the two bits, so this isn't expected.
        format!("attached; a capture stream ({packet_frames}-frame packets) holds the ring but neither read it nor mismatched")
    } else {
        "attached, not read (no capture stream holds it)".into()
    }
}

/// STATS shows a ring attached and the capture stream holding it reading it (not a format mismatch).
fn is_reading(s: &Stats) -> bool {
    stats_state(s) == STATE_ATTACHED | STATE_READING
}

// ---- streams: formats, processing modes, HRESULTs, opening a client ----

/// The processing mode a stream asks for: AUDCLNT_STREAMOPTIONS_RAW, set before Initialize, or none (DEFAULT), or the
/// Communications category (as call apps open the mic), which Windows maps to the endpoint's COMMUNICATIONS mode where it has
/// one and to DEFAULT where it hasn't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// RAW where the endpoint takes it, else DEFAULT (verify's, latency's and kill-test's default).
    RawWhereOffered,
    Raw,
    Default,
    Communications,
}

/// A stream's sample format: 48 kHz stereo, eight bytes a frame (`protocol::FRAME_BYTES`) either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// IEEE float32: the ring's format, and the endpoints' device format (spike check 3).
    Float,
    /// Signed PCM with 24 valid bits in a 32-bit container (the sample in the high three bytes): the endpoints' second format.
    Pcm24,
}

impl Format {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "float" => Ok(Self::Float),
            "pcm24" => Ok(Self::Pcm24),
            _ => Err(format!("--format {s:?}: float or pcm24")),
        }
    }

    /// The format as a WAVEFORMATEXTENSIBLE, front left and right.
    fn wave(self) -> WaveFormat {
        let rate = protocol::SAMPLE_RATE as usize;
        match self {
            Self::Float => WaveFormat::new(32, 32, &SampleType::Float, rate, 2, None),
            Self::Pcm24 => WaveFormat::new(32, 24, &SampleType::Int, rate, 2, None),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Float => "48 kHz stereo float32",
            Self::Pcm24 => "48 kHz stereo 24-in-32 PCM",
        }
    }
}

/// How a stream is opened (always event-driven).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamSpec {
    exclusive: bool,
    format: Format,
    /// Exclusive: the period, in frames (default: the device's default period). Shared: the engine period asked of
    /// IAudioClient3::InitializeSharedAudioStream (default: IAudioClient::Initialize, at the engine's period).
    period_frames: Option<u32>,
    mode: Mode,
}

impl StreamSpec {
    /// verify's, kill-test's and latency's stream: shared float32 (the mix format, checked first), RAW where offered.
    const SHARED_FLOAT: Self = Self { exclusive: false, format: Format::Float, period_frames: None, mode: Mode::RawWhereOffered };

    fn describe(&self) -> String {
        let mode = match self.mode {
            Mode::RawWhereOffered => "RAW where offered",
            Mode::Raw => "RAW",
            Mode::Default => "DEFAULT mode",
            Mode::Communications => "the Communications category",
        };
        let period = match (self.exclusive, self.period_frames) {
            (true, Some(n)) => format!("a {n}-frame ({:.2} ms) period", ms(u64::from(n))),
            (true, None) => "the device's default period".into(),
            (false, Some(n)) => format!("an IAudioClient3 engine period of {n} frames ({:.2} ms)", ms(u64::from(n))),
            (false, None) => "the engine's period".into(),
        };
        format!("{}, {}, {mode}, {period}", if self.exclusive { "exclusive" } else { "shared" }, self.format.name())
    }
}

/// A shared stream's buffer: 100 ms (a reader that only has to keep up, not be quick).
const SHARED_BUFFER_HNS: i64 = 1_000_000;

/// QueryPerformanceCounter, now (ticks).
fn qpc_now() -> u64 {
    let mut t = 0i64;
    // SAFETY: writes the counter to `t`; it can't fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    t as u64
}

/// QueryPerformanceCounter's frequency (ticks a second; fixed at boot).
fn qpc_frequency() -> u64 {
    let mut f = 0i64;
    // SAFETY: writes the frequency to `f`; it can't fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    (f as u64).max(1)
}

/// HRESULTs by name: the ones WASAPI's calls return, and HRESULT_FROM_WIN32 and HRESULT_FROM_NT of the errors a driver's
/// refusal is likely to surface as.
mod hresult {
    use windows::Win32::Foundation::{
        E_ABORT, E_ACCESSDENIED, E_FAIL, E_HANDLE, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED,
        ERROR_BUSY, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_FUNCTION, ERROR_NO_SYSTEM_RESOURCES,
        ERROR_NOT_FOUND, ERROR_NOT_READY, ERROR_NOT_SUPPORTED, NTSTATUS, S_FALSE, S_OK, STATUS_DEVICE_BUSY, STATUS_DEVICE_REMOVED,
        STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER,
        STATUS_NOT_SUPPORTED, STATUS_UNSUCCESSFUL, WIN32_ERROR,
    };
    use windows::Win32::Media::Audio::{
        AUDCLNT_E_ALREADY_INITIALIZED, AUDCLNT_E_BUFDURATION_PERIOD_NOT_EQUAL, AUDCLNT_E_BUFFER_ERROR, AUDCLNT_E_BUFFER_OPERATION_PENDING,
        AUDCLNT_E_BUFFER_SIZE_ERROR, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_BUFFER_TOO_LARGE, AUDCLNT_E_CPUUSAGE_EXCEEDED,
        AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_ENDPOINT_CREATE_FAILED, AUDCLNT_E_ENGINE_FORMAT_LOCKED,
        AUDCLNT_E_ENGINE_PERIODICITY_LOCKED, AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED, AUDCLNT_E_EVENTHANDLE_NOT_SET,
        AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED, AUDCLNT_E_EXCLUSIVE_MODE_ONLY, AUDCLNT_E_INCORRECT_BUFFER_SIZE,
        AUDCLNT_E_INVALID_DEVICE_PERIOD, AUDCLNT_E_INVALID_SIZE, AUDCLNT_E_INVALID_STREAM_FLAG, AUDCLNT_E_NOT_INITIALIZED,
        AUDCLNT_E_NOT_STOPPED, AUDCLNT_E_OUT_OF_ORDER, AUDCLNT_E_RAW_MODE_UNSUPPORTED, AUDCLNT_E_RESOURCES_INVALIDATED,
        AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_E_THREAD_NOT_REGISTERED, AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_E_WRONG_ENDPOINT_TYPE,
        AUDCLNT_S_BUFFER_EMPTY, AUDCLNT_S_POSITION_STALLED, AUDCLNT_S_THREAD_ALREADY_REGISTERED,
    };
    use windows::core::HRESULT;

    #[rustfmt::skip]
    pub(super) const NAMES: [(HRESULT, &str); 50] = [
        (S_OK, "S_OK"),
        (S_FALSE, "S_FALSE"),
        (AUDCLNT_S_BUFFER_EMPTY, "AUDCLNT_S_BUFFER_EMPTY"),
        (AUDCLNT_S_THREAD_ALREADY_REGISTERED, "AUDCLNT_S_THREAD_ALREADY_REGISTERED"),
        (AUDCLNT_S_POSITION_STALLED, "AUDCLNT_S_POSITION_STALLED"),
        (AUDCLNT_E_NOT_INITIALIZED, "AUDCLNT_E_NOT_INITIALIZED"),
        (AUDCLNT_E_ALREADY_INITIALIZED, "AUDCLNT_E_ALREADY_INITIALIZED"),
        (AUDCLNT_E_WRONG_ENDPOINT_TYPE, "AUDCLNT_E_WRONG_ENDPOINT_TYPE"),
        (AUDCLNT_E_DEVICE_INVALIDATED, "AUDCLNT_E_DEVICE_INVALIDATED"),
        (AUDCLNT_E_NOT_STOPPED, "AUDCLNT_E_NOT_STOPPED"),
        (AUDCLNT_E_BUFFER_TOO_LARGE, "AUDCLNT_E_BUFFER_TOO_LARGE"),
        (AUDCLNT_E_OUT_OF_ORDER, "AUDCLNT_E_OUT_OF_ORDER"),
        (AUDCLNT_E_UNSUPPORTED_FORMAT, "AUDCLNT_E_UNSUPPORTED_FORMAT"),
        (AUDCLNT_E_INVALID_SIZE, "AUDCLNT_E_INVALID_SIZE"),
        (AUDCLNT_E_DEVICE_IN_USE, "AUDCLNT_E_DEVICE_IN_USE"),
        (AUDCLNT_E_BUFFER_OPERATION_PENDING, "AUDCLNT_E_BUFFER_OPERATION_PENDING"),
        (AUDCLNT_E_THREAD_NOT_REGISTERED, "AUDCLNT_E_THREAD_NOT_REGISTERED"),
        (AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED, "AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED"),
        (AUDCLNT_E_ENDPOINT_CREATE_FAILED, "AUDCLNT_E_ENDPOINT_CREATE_FAILED"),
        (AUDCLNT_E_SERVICE_NOT_RUNNING, "AUDCLNT_E_SERVICE_NOT_RUNNING"),
        (AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED, "AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED"),
        (AUDCLNT_E_EXCLUSIVE_MODE_ONLY, "AUDCLNT_E_EXCLUSIVE_MODE_ONLY"),
        (AUDCLNT_E_BUFDURATION_PERIOD_NOT_EQUAL, "AUDCLNT_E_BUFDURATION_PERIOD_NOT_EQUAL"),
        (AUDCLNT_E_EVENTHANDLE_NOT_SET, "AUDCLNT_E_EVENTHANDLE_NOT_SET"),
        (AUDCLNT_E_INCORRECT_BUFFER_SIZE, "AUDCLNT_E_INCORRECT_BUFFER_SIZE"),
        (AUDCLNT_E_BUFFER_SIZE_ERROR, "AUDCLNT_E_BUFFER_SIZE_ERROR"),
        (AUDCLNT_E_CPUUSAGE_EXCEEDED, "AUDCLNT_E_CPUUSAGE_EXCEEDED"),
        (AUDCLNT_E_BUFFER_ERROR, "AUDCLNT_E_BUFFER_ERROR"),
        (AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, "AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED"),
        (AUDCLNT_E_INVALID_DEVICE_PERIOD, "AUDCLNT_E_INVALID_DEVICE_PERIOD"),
        (AUDCLNT_E_INVALID_STREAM_FLAG, "AUDCLNT_E_INVALID_STREAM_FLAG"),
        (AUDCLNT_E_RESOURCES_INVALIDATED, "AUDCLNT_E_RESOURCES_INVALIDATED"),
        (AUDCLNT_E_RAW_MODE_UNSUPPORTED, "AUDCLNT_E_RAW_MODE_UNSUPPORTED"),
        (AUDCLNT_E_ENGINE_PERIODICITY_LOCKED, "AUDCLNT_E_ENGINE_PERIODICITY_LOCKED"),
        (AUDCLNT_E_ENGINE_FORMAT_LOCKED, "AUDCLNT_E_ENGINE_FORMAT_LOCKED"),
        (E_FAIL, "E_FAIL"),
        (E_ABORT, "E_ABORT"),
        (E_NOTIMPL, "E_NOTIMPL"),
        (E_NOINTERFACE, "E_NOINTERFACE"),
        (E_POINTER, "E_POINTER"),
        (E_UNEXPECTED, "E_UNEXPECTED"),
        (E_ACCESSDENIED, "E_ACCESSDENIED"),
        (E_HANDLE, "E_HANDLE"),
        (E_OUTOFMEMORY, "E_OUTOFMEMORY"),
        (E_INVALIDARG, "E_INVALIDARG"),
        (HRESULT::from_win32(ERROR_NOT_SUPPORTED.0), "HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)"),
        (HRESULT::from_win32(ERROR_BUSY.0), "HRESULT_FROM_WIN32(ERROR_BUSY)"),
        (HRESULT::from_win32(ERROR_GEN_FAILURE.0), "HRESULT_FROM_WIN32(ERROR_GEN_FAILURE)"),
        (HRESULT::from_win32(ERROR_NOT_FOUND.0), "HRESULT_FROM_WIN32(ERROR_NOT_FOUND)"),
        (HRESULT::from_nt(STATUS_NOT_SUPPORTED.0), "HRESULT_FROM_NT(STATUS_NOT_SUPPORTED)"),
    ];

    /// Win32 errors (facility 7) and NTSTATUSes (the FACILITY_NT bit) the table above doesn't name in full.
    #[rustfmt::skip]
    const WIN32: [(WIN32_ERROR, &str); 5] = [
        (ERROR_INVALID_FUNCTION, "ERROR_INVALID_FUNCTION"),
        (ERROR_FILE_NOT_FOUND, "ERROR_FILE_NOT_FOUND"),
        (ERROR_NOT_READY, "ERROR_NOT_READY"),
        (ERROR_DEVICE_NOT_CONNECTED, "ERROR_DEVICE_NOT_CONNECTED"),
        (ERROR_NO_SYSTEM_RESOURCES, "ERROR_NO_SYSTEM_RESOURCES"),
    ];
    #[rustfmt::skip]
    const NT: [(NTSTATUS, &str); 8] = [
        (STATUS_UNSUCCESSFUL, "STATUS_UNSUCCESSFUL"),
        (STATUS_NOT_SUPPORTED, "STATUS_NOT_SUPPORTED"),
        (STATUS_INVALID_PARAMETER, "STATUS_INVALID_PARAMETER"),
        (STATUS_INVALID_DEVICE_REQUEST, "STATUS_INVALID_DEVICE_REQUEST"),
        (STATUS_INVALID_DEVICE_STATE, "STATUS_INVALID_DEVICE_STATE"),
        (STATUS_INSUFFICIENT_RESOURCES, "STATUS_INSUFFICIENT_RESOURCES"),
        (STATUS_DEVICE_BUSY, "STATUS_DEVICE_BUSY"),
        (STATUS_DEVICE_REMOVED, "STATUS_DEVICE_REMOVED"),
    ];

    /// `hr` by name where it has one, always with its value, and the system's text for it where there's no name.
    pub(super) fn name(hr: HRESULT) -> String {
        let code = hr.0 as u32;
        if let Some((_, n)) = NAMES.iter().find(|(h, _)| *h == hr) {
            return format!("{n} (0x{code:08X})");
        }
        if code & 0xFFFF_0000 == 0x8007_0000
            && let Some((_, n)) = WIN32.iter().find(|(e, _)| e.0 == code & 0xFFFF)
        {
            return format!("HRESULT_FROM_WIN32({n}) (0x{code:08X})");
        }
        if hr.0 < 0
            && code & 0x1000_0000 != 0
            && let Some((_, n)) = NT.iter().find(|(s, _)| s.0 as u32 == code & !0x1000_0000)
        {
            return format!("HRESULT_FROM_NT({n}) (0x{code:08X})");
        }
        let message = hr.message();
        let message = message.trim();
        if message.is_empty() { format!("0x{code:08X}") } else { format!("0x{code:08X}: {message}") }
    }
}

/// The HRESULT behind a wasapi error; the crate's own errors (not a COM call's) as the nearest code.
fn hresult_of(e: &WasapiError) -> HRESULT {
    match e {
        WasapiError::Windows(w) | WasapiError::RegisterNotifications(w) => w.code(),
        WasapiError::UnsupportedFormat | WasapiError::UnsupportedSubformat(_) => AUDCLNT_E_UNSUPPORTED_FORMAT,
        _ => E_FAIL,
    }
}

/// A call that failed while a stream was being opened or started (anything up to and including Start), with its HRESULT: a
/// refusal, for `open`.
#[derive(Debug)]
struct Refusal {
    call: &'static str,
    hr: HRESULT,
    /// The crate's text, where the error wasn't a COM call's.
    text: Option<String>,
}

impl Refusal {
    fn at(call: &'static str, e: WasapiError) -> Self {
        let text = (!matches!(e, WasapiError::Windows(_))).then(|| e.to_string());
        Self { call, hr: hresult_of(&e), text }
    }

    /// A refusal from a COM call made directly (not through the wasapi crate).
    fn com(call: &'static str, e: &windows::core::Error) -> Self {
        Self { call, hr: e.code(), text: None }
    }
}

impl Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned {}", self.call, hresult::name(self.hr))?;
        match &self.text {
            Some(t) => write!(f, " ({t})"),
            None => Ok(()),
        }
    }
}

/// A stream initialised, and what it was granted.
struct Opened {
    client: AudioClient,
    exclusive: bool,
    raw: bool,
    /// Initialize or InitializeSharedAudioStream: the call that opened it.
    init_call: &'static str,
    /// The buffer, in frames (an exclusive event-driven stream's is its period).
    buffer_frames: u32,
    /// Shared: the engine's current period in this mode, in frames (IAudioClient3), where it says.
    engine_period: Option<u32>,
    /// The device's default period, in frames.
    default_period: u32,
    /// Shared: the IAudioClient3 engine period asked for.
    asked_period: Option<u32>,
    /// What happened on the way: a RAW refusal fallen back from, an exclusive buffer realigned.
    notes: Vec<String>,
}

impl Opened {
    /// The period granted, in frames: exclusive, the buffer; shared, the engine's period in this mode (the device's default
    /// period where IAudioClient3 can't say).
    fn period_frames(&self) -> u32 {
        if self.exclusive { self.buffer_frames } else { self.engine_period.unwrap_or(self.default_period) }
    }

    fn describe(&self) -> String {
        let raw = if self.raw { "RAW" } else { "DEFAULT mode" };
        let period = self.period_frames();
        let how = match (self.exclusive, self.asked_period, self.engine_period) {
            (true, _, _) => "the exclusive period".to_string(),
            (false, Some(asked), _) => format!("the engine period, IAudioClient3; {asked} asked"),
            (false, None, Some(_)) => "the engine period".to_string(),
            (false, None, None) => "the device's default period; IAudioClient3 didn't say the engine's".to_string(),
        };
        format!(
            "{}, {raw}, period {period} frames ({:.2} ms: {how}), buffer {} frames ({:.2} ms)",
            if self.exclusive { "exclusive" } else { "shared" },
            ms(u64::from(period)),
            self.buffer_frames,
            ms(u64::from(self.buffer_frames))
        )
    }
}

/// The endpoint called `name` in direction `dir` (its friendly name, as Sound settings shows it).
fn find_endpoint(name: &str, dir: Direction) -> Result<wasapi::Device, String> {
    let kind = if dir == Direction::Capture { "capture" } else { "render" };
    let enumerator = DeviceEnumerator::new().map_err(werr)?;
    let devices = enumerator.get_device_collection(&dir).map_err(werr)?;
    devices.get_device_with_name(name).map_err(|_| {
        let names: Vec<String> = (&devices).into_iter().filter_map(|d| d.ok()?.get_friendlyname().ok()).collect();
        format!("no {kind} endpoint {name:?}; there are: {}", names.join("; "))
    })
}

/// A format in words: rate, channels, and the sample type with its container and valid bits.
fn describe_format(f: &WaveFormat) -> String {
    let (bits, valid) = (f.get_bitspersample(), f.get_validbitspersample());
    let kind = match f.get_subformat() {
        Ok(SampleType::Float) => format!("{bits}-bit float"),
        Ok(SampleType::Int) if valid == bits || valid == 0 => format!("{bits}-bit PCM"),
        Ok(SampleType::Int) => format!("{valid}-in-{bits} PCM"),
        Err(_) => format!("{bits}-bit, not PCM or float"),
    };
    format!("{} Hz, {} ch, {kind}", f.get_samplespersec(), f.get_nchannels())
}

/// IsFormatSupported's HRESULT for `format` in that share mode, on a fresh client (no stream is made), and in shared mode the
/// closest format it offered instead (S_FALSE).
fn is_format_supported(device: &wasapi::Device, format: &WaveFormat, exclusive: bool) -> Result<(HRESULT, Option<String>), String> {
    let client = device.get_iaudioclient().map_err(werr)?;
    let mut closest: *mut WAVEFORMATEX = std::ptr::null_mut();
    let (mode, out) = if exclusive { (AUDCLNT_SHAREMODE_EXCLUSIVE, None) } else { (AUDCLNT_SHAREMODE_SHARED, Some(&raw mut closest)) };
    // SAFETY: a COM call on our client; `format` outlives it, and `closest` is a local the call may set to a CoTaskMemAlloc'd
    // format (shared mode only).
    let hr = unsafe { client.as_iaudioclient().IsFormatSupported(mode, format.as_waveformatex_ref(), out) };
    if closest.is_null() {
        return Ok((hr, None));
    }
    // SAFETY: WASAPI's own format, valid for its cbSize; read once, then freed once with the allocator it came from.
    let parsed = unsafe { WaveFormat::parse(closest) };
    // SAFETY: as above.
    unsafe { CoTaskMemFree(Some(closest.cast())) };
    Ok((hr, Some(parsed.map_or_else(|e| format!("unreadable ({e})"), |f| describe_format(&f)))))
}

/// The engine periods IAudioClient3 offers for `format` on `device`, in words, where it says.
fn engine_periods(device: &wasapi::Device, format: &WaveFormat) -> Option<String> {
    let p = device.get_iaudioclient().ok()?.get_shared_mode_engine_period(format).ok()?;
    Some(format!(
        "IAudioClient3's engine periods for {}: default {}, fundamental {}, from {} to {} frames",
        describe_format(format),
        p.default,
        p.fundamental,
        p.min,
        p.max
    ))
}

/// Opens a client on `device` as `spec` says: RAW (or not) first, then Initialize (exclusive, or shared at the engine's
/// period) or IAudioClient3's InitializeSharedAudioStream (shared, `period_frames`). Event-driven; nothing converted. The
/// first call that fails is the refusal; RAW where offered tries again without RAW first.
fn open_client(device: &wasapi::Device, dir: Direction, spec: &StreamSpec) -> Result<Opened, Refusal> {
    match spec.mode {
        Mode::Raw => init_client(device, dir, spec, true),
        Mode::Default | Mode::Communications => init_client(device, dir, spec, false),
        Mode::RawWhereOffered => init_client(device, dir, spec, true).or_else(|raw| {
            let mut o = init_client(device, dir, spec, false)?;
            o.notes.insert(0, format!("RAW refused ({raw}); DEFAULT mode instead, so the endpoint's effects, if any, run"));
            Ok(o)
        }),
    }
}

/// A fresh client, with RAW or the Communications category set (before Initialize, as IAudioClient2 requires) when asked.
fn new_client(device: &wasapi::Device, raw: bool, communications: bool) -> Result<AudioClient, Refusal> {
    let client = device.get_iaudioclient().map_err(|e| Refusal::at("IMMDevice::Activate", e))?;
    if raw {
        client
            .set_properties(AudioClientProperties::new().set_option(StreamOption::Raw))
            .map_err(|e| Refusal::at("SetClientProperties (RAW)", e))?;
    } else if communications {
        client
            .set_properties(AudioClientProperties::new().set_category(StreamCategory::Communications))
            .map_err(|e| Refusal::at("SetClientProperties (Communications)", e))?;
    }
    Ok(client)
}

fn init_client(device: &wasapi::Device, dir: Direction, spec: &StreamSpec, raw: bool) -> Result<Opened, Refusal> {
    let rate = i64::from(protocol::SAMPLE_RATE);
    let format = spec.format.wave();
    let communications = spec.mode == Mode::Communications;
    let mut client = new_client(device, raw, communications)?;
    let mut notes = Vec::new();
    let (default_hns, _) = client.get_device_period().map_err(|e| Refusal::at("GetDevicePeriod", e))?;
    let init_call = if spec.exclusive {
        let period_hns = spec.period_frames.map_or(default_hns, |n| calculate_period_100ns(i64::from(n), rate));
        if let Err(e) = client.initialize_client(&format, &dir, &StreamMode::EventsExclusive { period_hns }) {
            if hresult_of(&e) != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                return Err(Refusal::at("Initialize", e));
            }
            // Microsoft's recovery: the failed client's buffer size, on a fresh client.
            let frames = client.get_buffer_size().map_err(|e| Refusal::at("GetBufferSize", e))?;
            notes.push(format!("Initialize returned AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED; opened again at the aligned {frames} frames"));
            client = new_client(device, raw, communications)?;
            let period_hns = calculate_period_100ns(i64::from(frames), rate);
            client
                .initialize_client(&format, &dir, &StreamMode::EventsExclusive { period_hns })
                .map_err(|e| Refusal::at("Initialize", e))?;
        }
        "Initialize"
    } else if let Some(frames) = spec.period_frames {
        client
            .initialize_shared_audio_stream(&format, TimingMode::Events, frames)
            .map_err(|e| Refusal::at("InitializeSharedAudioStream", e))?;
        "InitializeSharedAudioStream"
    } else {
        let mode = StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: SHARED_BUFFER_HNS };
        client.initialize_client(&format, &dir, &mode).map_err(|e| Refusal::at("Initialize", e))?;
        "Initialize"
    };
    let buffer_frames = client.get_buffer_size().map_err(|e| Refusal::at("GetBufferSize", e))?;
    // Another client in this mode may hold the engine at another period: this is what applies.
    let engine_period = if spec.exclusive { None } else { client.get_current_shared_mode_engine_period().ok().map(|(_, f)| f) };
    Ok(Opened {
        client,
        exclusive: spec.exclusive,
        raw,
        init_call,
        buffer_frames,
        engine_period,
        default_period: ((default_hns.max(0) * rate + 5_000_000) / 10_000_000) as u32,
        asked_period: if spec.exclusive { None } else { spec.period_frames },
        notes,
    })
}

// ---- feed ----

/// Set by Ctrl+C or Ctrl+Break: feed stops, detaches and prints its counts.
static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_console_ctrl(ctrl_type: u32) -> BOOL {
    if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
        STOP.store(true, Relaxed);
        TRUE
    } else {
        FALSE
    }
}

/// Why feed's loop ended.
enum End {
    /// `--seconds` ran out, or Ctrl+C.
    Stopped,
    /// The driver completed the request.
    Driver(io::Error),
    /// The reader's packet (frames) is more than the target fill.
    TargetBelowPacket(u32),
}

/// The reader's packet when it is more than the fill `target` (so every pass underruns), from the header's State and
/// PacketFrames. Judged only while the stream holding the slot reads the ring: one in another format counts its packet in its
/// own format's frames.
fn packet_above_target(target: u64, state: u32, packet_frames: u32) -> Option<u32> {
    (state & STATE_ATTACHED != 0 && state & STATE_READING != 0 && u64::from(packet_frames) > target).then_some(packet_frames)
}

fn feed(args: &[String]) -> Result<Verdict, String> {
    let o = Opts::parse(args, &["--seconds", "--capacity", "--target", "--start"], &["--quiet"])?;
    let seconds: u64 = o.value("--seconds", 0)?;
    let capacity: u32 = o.value("--capacity", 8192)?;
    let target: u64 = o.value("--target", 960)?;
    let start: u64 = o.value("--start", 0)?;
    let quiet = o.flag("--quiet");
    if !protocol::capacity_ok(capacity) {
        let (min, max) = (protocol::MIN_CAPACITY_FRAMES, protocol::MAX_CAPACITY_FRAMES);
        return Err(format!("--capacity {capacity}: a power of two from {min} to {max}"));
    }
    if target == 0 || target > u64::from(capacity) {
        return Err(format!("--target {target}: from 1 to the capacity ({capacity}), and at least one capture packet"));
    }

    let device = open_device()?;
    let ring = RingMemory::new(capacity).map_err(|e| e.to_string())?;
    // Pre-filled to the target, so the first capture pass after ATTACH finds audio (the driver starts at frame 0).
    Writer::new(&ring).produce(target, |n| signal::frame(start.wrapping_add(n)));
    // SAFETY: the handler only stores to an atomic.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) }.is_err() {
        eprintln!("note: no Ctrl+C handler: Ctrl+C ends the feed without its summary (the driver still detaches the ring)");
    }
    let attachment = device.attach(ring).map_err(|e| format!("ATTACH refused: {}{}", e.error, attach_hint(&e)))?;
    if !quiet {
        let until = if seconds == 0 { "until Ctrl+C".to_string() } else { format!("for {seconds} s") };
        println!(
            "attached a {capacity}-frame ring; keeping {target} frames ({:.1} ms) in it; signal from frame {start}, {until}",
            ms(target)
        );
    }

    let _pro_audio = ProAudio::join();
    let tick = Tick::new();
    let header = attachment.ring().header();
    let mut writer = Writer::new(attachment.ring());
    let t0 = Instant::now();
    let (mut last_report, mut last_read) = (t0, writer.read_frame());
    let end = loop {
        if let Some(status) = attachment.ended() {
            break End::Driver(status);
        }
        // The reader's packet, once a capture stream reads the ring: a target below it underruns every pass. (A stream whose
        // format doesn't match isn't read at all, which the status line says.) PacketFrames first: the driver stores State
        // before it (AppRingStoreCounters; x64 keeps stores in order), so a new packet comes with its own stream's State or a
        // later one, never the stream before's READING.
        let packet = header.packet_frames.load(Acquire);
        if let Some(packet) = packet_above_target(target, header.state.load(Acquire), packet) {
            break End::TargetBelowPacket(packet);
        }
        let n = top_up(target, writer.fill(), writer.free());
        if n > 0 {
            writer.produce(n, |pos| signal::frame(start.wrapping_add(pos)));
        }
        let now = Instant::now();
        if STOP.load(Relaxed) || (seconds != 0 && now - t0 >= Duration::from_secs(seconds)) {
            break End::Stopped;
        }
        if !quiet && now - last_report >= Duration::from_secs(1) {
            let read = writer.read_frame();
            let rate = read.saturating_sub(last_read) as f64 / (now - last_report).as_secs_f64();
            print_feed_status(now - t0, &writer, header, rate);
            (last_report, last_read) = (now, read);
        }
        tick.wait();
    };
    let written = writer.write_frame();
    match end {
        End::Stopped => {}
        End::Driver(status) => {
            print_ring_counters(header);
            // Returning drops the attachment and then the device: every handle closes.
            eprintln!("the driver ended the attachment: {}", end_text(&status));
            return Ok(Verdict::Fail);
        }
        End::TargetBelowPacket(packet) => {
            print_ring_counters(header);
            // Returning detaches (the attachment's drop).
            return Err(format!(
                "--target {target} is below the reader's {packet}-frame capture packet, so every pass underruns: keep at least one \
                 packet (--target {}, two packets, leaves a margin)",
                2 * u64::from(packet)
            ));
        }
    }
    let (ring, status) = attachment.detach().map_err(|e| format!("detach: {e}"))?;
    let h = ring.header();
    println!("detached ({}); wrote {written} frames, the driver read {}", end_text(&status), h.read_frame.load(Acquire));
    print_ring_counters(h);
    Ok(Verdict::Pass)
}

fn print_feed_status(elapsed: Duration, w: &Writer<'_>, h: &RingHeader, read_rate: f64) {
    println!(
        "{:>7.1} s  written {}  read {}  fill {} ({:.1} ms)  reader {read_rate:.1} frames/s  underrun {}  resyncs {}  [{}]",
        elapsed.as_secs_f64(),
        w.write_frame(),
        w.read_frame(),
        w.fill(),
        ms(w.fill()),
        h.underrun_frames.load(Relaxed),
        h.resyncs.load(Relaxed),
        state_text(h.state.load(Relaxed), h.packet_frames.load(Relaxed)),
    );
}

fn print_ring_counters(h: &RingHeader) {
    println!(
        "the driver's counts for this ring: {} passes, {} frames copied, {} underrun frames, {} resyncs, {} mismatched packets",
        h.passes.load(Relaxed),
        h.frames_copied.load(Relaxed),
        h.underrun_frames.load(Relaxed),
        h.resyncs.load(Relaxed),
        h.mismatch_packets.load(Relaxed),
    );
}

/// A deterministic stale-microphone test: inject marked samples in the forbidden interval, and keep the producer stopped
/// until the first capture pass. A silent leading packet alone cannot pass this check: ReadFrame must skip the old prefix,
/// the driver's copied count must stay still, and the recorder must subsequently receive the new marker.
fn idle_test(args: &[String]) -> Result<Verdict, String> {
    let opts = Opts::parse(args, &["--device"], &[])?;
    let name: String = opts.value("--device", ENDPOINT.to_string())?;
    let _com = Com::mta()?;
    let device = open_device()?;
    let attachment = device.attach_live(RingMemory::new(8192).map_err(|e| e.to_string())?).map_err(|e| e.error.to_string())?;
    let mut writer = Writer::new(attachment.ring());
    let header = attachment.ring().header();
    let mut checks = Checks::default();
    for opening in 1..=2 {
        let idle = header.state.load(Acquire) == STATE_ATTACHED;
        if !checks.check(idle, "no active capture reader", format_args!("open {opening}")) {
            return Ok(Verdict::Fail);
        }
        let copied = header.frames_copied.load(Acquire);
        let queued = writer.produce(1536, |_| [-0.5; 2]);
        let end = writer.write_frame();
        let mut capture = Capture::open(&name, StreamSpec { exclusive: true, ..StreamSpec::SHARED_FLOAT })?;
        capture.start()?;
        let mut stale = 0;
        let mut observe = |f: [f32; 2]| stale += u32::from(f == [-0.5; 2]);
        let until = Instant::now() + READING_WAIT;
        while header.state.load(Acquire) & STATE_READING == 0 && Instant::now() < until {
            capture.read(&mut observe)?;
        }
        let consumed = writer.read_frame();
        let copied_now = header.frames_copied.load(Acquire);
        checks.check(
            queued == 1536 && consumed == end && copied_now == copied,
            "queued pre-open audio discarded",
            format_args!("open {opening}: {queued} queued, read position {consumed}/{end}, {} copied", copied_now - copied),
        );
        let fresh = [0.25; 2];
        writer.produce(1536, |_| fresh);
        let mut received = 0;
        let until = Instant::now() + READING_WAIT;
        while received < 1536 && Instant::now() < until {
            capture.read(|f| {
                received += u32::from(f == fresh);
                stale += u32::from(f == [-0.5; 2]);
            })?;
        }
        capture.stop();
        drop(capture);
        checks.check(
            received == 1536 && stale == 0,
            "only fresh mic samples delivered",
            format_args!("open {opening}: {received}/1536 fresh frames, {stale} stale"),
        );
    }
    Ok(if checks.failed == 0 { Verdict::Pass } else { Verdict::Fail })
}

// ---- verify ----

fn verify(args: &[String]) -> Result<Verdict, String> {
    let o = Opts::parse(args, &["--seconds", "--device", "--mode", "--period-frames", "--start"], &[])?;
    let seconds: u64 = o.value("--seconds", 10)?;
    let name: String = o.value("--device", ENDPOINT.to_string())?;
    let start: u64 = o.value("--start", 0)?;
    let spec = StreamSpec { period_frames: o.period_frames()?, mode: o.mode(Mode::RawWhereOffered)?, ..StreamSpec::SHARED_FLOAT };
    if seconds == 0 || seconds > 86_400 {
        return Err(format!("--seconds {seconds}: from 1 to 86400"));
    }
    // The driver's statistics are half the verdict: no driver, no verify.
    let device = open_device()?;
    if read_stats(&device)?.flags & STATS_ATTACHED == 0 {
        return Err("no app ring is attached: start `nf-vmic-test feed` first (elevated, in another window) and leave it running".into());
    }
    let _com = Com::mta()?;
    let mut capture = Capture::open(&name, spec)?;
    println!("recording {name:?} for {seconds} s: {}", capture.describe());
    capture.print_volume_notes();
    let _pro_audio = ProAudio::join();
    capture.start()?;
    let recorded = record(&mut capture, Duration::from_secs(seconds));
    capture.stop();
    let Recorded { before, after, checker, vacancy, losses } = recorded?;

    let r = checker.report();
    let (b, a) = (&before.ring, &after.ring);
    let mut c = Checks::default();
    println!("checks:");
    let seen = StreamSeen { losses, packet: capture.opened.period_frames(), start, vacancy };
    let checks = verify_checks(&before, &after, r, &seen);
    for check in &checks {
        c.judge(check.outcome, check.name, &check.detail);
    }
    let window = LogWindow::of(&before, &after);
    let ties = tie_gaps(&before, r, start, &window);
    for (i, tie) in ties.iter().enumerate() {
        println!("  gap {}: {}", i + 1, tie.text);
        for candidate in &tie.candidates {
            println!("      {candidate}");
        }
    }
    if r.gaps > r.gap_list.len() as u64 {
        println!("  ({} gaps more, not listed)", r.gaps - r.gap_list.len() as u64);
    }
    let in_window: Vec<&Event> = window.entries.iter().filter(|e| e.sequence > window.before && e.sequence <= window.after).collect();
    println!(
        "  the skip log over the recording: +{} events{}",
        window.after.saturating_sub(window.before),
        if window.lost != 0 { format!(", {} of them no longer in it", window.lost) } else { String::new() }
    );
    for e in in_window {
        println!("    {}", event_text(e, after.qpc_now, after.qpc_frequency));
    }
    let unnamed = unnamed_losses(&window, &ties);
    if !unnamed.is_empty() {
        let numbers: Vec<String> = unnamed.iter().map(|e| format!("#{}", e.sequence)).collect();
        println!(
            "  of those, the driver's records of a loss that no gap of this recording names (as a candidate, or as an entry whose \
             ring frames may be short that may hold its frames): {}, diagnostic only, not failed",
            numbers.join(", ")
        );
    }
    let not_reader = a.not_reader_packets.saturating_sub(b.not_reader_packets);
    println!(
        "  the driver over the recording: resyncs +{}, format mismatch +{} packets, passes of a stream that isn't the reader +{}{}",
        a.resyncs.saturating_sub(b.resyncs),
        a.mismatch_packets.saturating_sub(b.mismatch_packets),
        not_reader,
        if not_reader != 0 {
            " (two capture streams: the first to run reads the ring, the other gets its audio as a secondary)"
        } else {
            ""
        }
    );
    println!(
        "  secondary streams: +{} frames of the ring's audio, +{} zero-filled; refusals: ATTACH for a format +{}, capture opens +{}",
        after.secondary_frames_copied.saturating_sub(before.secondary_frames_copied),
        after.secondary_underrun_frames.saturating_sub(before.secondary_underrun_frames),
        a.format_rejects.saturating_sub(b.format_rejects),
        a.refused_capture_opens.saturating_sub(b.refused_capture_opens)
    );
    println!(
        "  reader handoffs +{}, skipping +{} frames of the ring's audio; streams playing on through them moved on to a shorter \
         lag +{}, skipping +{} frames; streams playing the copy jumped in their own time line +{}, passing over +{} frames the \
         copy held as each was logged",
        after.reader_handoffs.saturating_sub(before.reader_handoffs),
        after.handoff_skipped_frames.saturating_sub(before.handoff_skipped_frames),
        after.secondary_skips.saturating_sub(before.secondary_skips),
        after.secondary_skipped_frames.saturating_sub(before.secondary_skipped_frames),
        after.secondary_jumps.saturating_sub(before.secondary_jumps),
        after.secondary_jump_frames.saturating_sub(before.secondary_jump_frames)
    );
    for (index, left, right) in &r.first_corrupt {
        let (l, rt) = (f32::from_bits(*left), f32::from_bits(*right));
        println!("  corrupt at frame {index}: left {left:#010x} ({l}), right {right:#010x} ({rt})");
    }
    capture.print_counters();
    if losses.later.any() {
        println!(
            "  (those include what the stream's packets after the recording carried, unrecorded, which its check leaves out: \
             {} frames lost and {} discontinuities, in the drain for the final STATS read{})",
            losses.later.frames,
            losses.later.discontinuities,
            vacancy.map_or(String::new(), |d| format!(" and a {:.1} ms wait for the reader slot", d.as_secs_f64() * 1e3))
        );
    }
    let verdict = verdict_of(&checks);
    match verdict {
        Verdict::Pass => println!(
            "PASS: the app ring fed the recording, bit-exact on both channels, in order, nothing repeated, and nothing lost but \
             a reader handoff's skip or a skip to a shorter lag, each the driver's skip log's record of the ring's frames it \
             lost, to the frame"
        ),
        Verdict::Inconclusive => {
            let why: Vec<&str> = checks.iter().filter(|c| c.outcome == Outcome::Inconclusive).map(|c| inconclusive_why(c.name)).collect();
            println!(
                "INCONCLUSIVE: nothing failed, but {} of the checks couldn't be decided from the driver's skip log, where what it \
                 can't say would change the verdict: {}",
                c.inconclusive,
                why.join("; ")
            );
        }
        _ => println!("FAIL: {} of the checks", c.failed),
    }
    Ok(verdict)
}

/// What verify knows besides the statistics and the recording: what its own capture stream counted (frames the engine dropped
/// and discontinuities, before, in and after the recording: [`StreamLosses`]; its period in frames), the feed's first frame
/// (`--start`: frame n carries the code of n + start, so a code names its ring frame), and, where no stream held the reader
/// slot as the recording ended, how long it waited for one to ([`record`]).
#[derive(Clone, Copy, Debug, Default)]
struct StreamSeen {
    losses: StreamLosses,
    packet: u32,
    start: u64,
    vacancy: Option<Duration>,
}

/// The capture stream's check: frames the engine lost and discontinuities up to the measured recording's end fail it, as
/// before the fix below: the recording's own packets', and those of the packets before it (the wait for the signal and the
/// reader, the drain for the STATS read just before), told apart in the detail (ours: whether those before it are the
/// recording's is the owner's to say; the fix agreed is for its end). Those of the packets after it (the drain for the final
/// STATS read, and any wait for the reader slot) are outside the recording's frames, and only reported (the owner,
/// 2026-11-24, agreeing Astra's fix; [`StreamLosses`]). A loss is counted on the packet after it, in frames before that
/// packet: one counted on the recording's first packet is in frames the checker never saw, judged all the same, as
/// everything before it is; one counted on the first packet after the recording is in frames after its last, only reported.
fn capture_stream_check(l: &StreamLosses) -> Check {
    let judged = l.judged();
    let before = if l.earlier.any() {
        format!(
            " ({} and {} of them in the packets before it: the wait for the signal and the reader, and the drain for the STATS \
             read just before)",
            l.earlier.frames, l.earlier.discontinuities
        )
    } else {
        String::new()
    };
    let after = if l.later.any() {
        format!(
            "; after it, information only, not its: {} frames lost and {} discontinuities (the drain for the STATS read just \
             after, and any wait for the reader slot)",
            l.later.frames, l.later.discontinuities
        )
    } else {
        String::new()
    };
    Check {
        outcome: (!judged.any()).into(),
        name: "the capture stream",
        detail: format!(
            "{} frames lost by the engine, {} discontinuities after its first packet, up to the recording's end{before}{after}",
            judged.frames, judged.discontinuities
        ),
    }
}

/// Why a check of verify's came out INCONCLUSIVE, for the verdict's line.
fn inconclusive_why(name: &str) -> &'static str {
    match name {
        "bit-exact and in order" => {
            "bit-exact and in order, for gaps whose record the log can't settle (a gap's candidates judged differently, or an \
             entry whose ring frames may be short that may hold its frames and would judge it otherwise; or, for a gap no chain \
             ties, events no longer in the log that may be an allowed skip for it: the gaps above say which)"
        }
        "the skip log against the counters" => {
            "the skip log against the counters, as events of the recording are no longer in the log, or unreadable, so its \
             entries can't be added up"
        }
        _ => "another check, above",
    }
}

/// verify's checks of a recording (`r`) against the driver's statistics just before and just after it, and the capture stream's
/// own counts up to the recording's end. All must pass for proof that the app ring fed the recording. Each gap is tied to the
/// driver's skip log ([`tie_gaps`]) and must be a skip the ring's rules allow; with none failed, a gap the log can't decide
/// where that would change its verdict (candidates for it judged differently, an entry whose ring frames may be short that
/// may hold its frames and would judge it otherwise, or for a gap no chain ties, events of the recording no longer in the
/// log that may be an allowed skip for it) makes the verdict INCONCLUSIVE, and so does a window the log can't check the
/// counters over.
///
/// The recording may come from the reader or from a secondary stream (another capture stream ran first): the silence allowed is
/// what the driver zero-filled for either (UnderrunFrames and SecondaryUnderrunFrames over the recording), and the slack at the
/// edges is a packet of the larger of the reader's and this stream's.
fn verify_checks(before: &Stats, after: &Stats, r: &CheckReport, seen: &StreamSeen) -> Vec<Check> {
    let packet = u64::from(before.reader_packet_frames.max(after.reader_packet_frames).max(seen.packet));
    let (b, a) = (&before.ring, &after.ring);
    let same = after.attached_pid == before.attached_pid && a.attaches == b.attaches;
    let copied = a.frames_copied.saturating_sub(b.frames_copied);
    let reader_underrun = a.underrun_frames.saturating_sub(b.underrun_frames);
    let secondary_underrun = after.secondary_underrun_frames.saturating_sub(before.secondary_underrun_frames);
    let underrun = reader_underrun + secondary_underrun;
    let underrun_text = format!("{underrun} frames (the reader {reader_underrun}, secondary streams {secondary_underrun})");
    let silence = r.zeros_before_signal + r.zeros_inside + r.zeros_after_signal;
    // Each gap is the driver's own record of it (the owner, 2026-11-24: "build it rather than marking things inconclusive"):
    // the codes either side name the ring's frames it lost, and the skip log must hold an entry for exactly those frames, or
    // a chain of one stream's entries end to end ([`tie_gaps`]), and they must be skips the ring's rules allow (a taker's
    // handoff skip, a skip to a shorter lag). So a gap is tied to the skip that made it with any number of streams, where the
    // driver-wide counters (two streams playing on through one handoff to a shorter packet each skip, the counter holding
    // both) could only say it might be among them; the tie is to an entry for exactly those frames, any stream's (verify
    // doesn't know its own). A gap past the recording's gap list (more than signal::GAP_RECORDS) can't be tied: it fails.
    // Any repeat fails.
    let window = LogWindow::of(before, after);
    let ties = tie_gaps(before, r, seen.start, &window);
    let untied = r.gaps.saturating_sub(ties.len() as u64);
    let gaps = ties.iter().map(|t| t.outcome).chain((untied != 0).then_some(Outcome::Fail)).fold(Outcome::Ok, worse);
    // What fails the check whatever the gaps.
    let others: Vec<&str> =
        [(r.signal_frames == 0, "the missing signal"), (r.repeats > 0, "the repeats"), (r.corrupt_frames > 0, "the corrupt frames")]
            .into_iter()
            .filter_map(|(bad, what)| bad.then_some(what))
            .collect();
    let in_order = if others.is_empty() { gaps } else { Outcome::Fail };
    let count = |o: Outcome| ties.iter().filter(|t| t.outcome == o).count();
    let fails_on = if others.is_empty() { String::new() } else { format!(", but the check fails on {}", others.join(" and ")) };
    let gaps_text = match (r.gaps, gaps) {
        (0, _) => String::new(),
        (_, Outcome::Ok) => {
            format!("; every gap is the skip log's record of a skip the ring's rules allow, to the frame (listed below){fails_on}")
        }
        (_, Outcome::Inconclusive) => format!(
            "; {} gaps tied to allowed skips in the skip log, {} it can't decide (listed below){fails_on}",
            count(Outcome::Ok),
            count(Outcome::Inconclusive)
        ),
        (_, Outcome::Fail) => format!(
            "; {} gaps not the skip log's record of a skip the ring's rules allow{} (listed below)",
            count(Outcome::Fail) as u64 + untied,
            if untied != 0 { format!(", {untied} of them past the {} the recording lists", signal::GAP_RECORDS) } else { String::new() }
        ),
    };
    let vacancy_text = match seen.vacancy {
        None => String::new(),
        Some(d) if is_reading(after) => format!(
            " (no stream held the reader slot as the recording ended, the last reader having stopped: one did {:.1} ms later)",
            d.as_secs_f64() * 1e3
        ),
        Some(d) => format!(" (no stream held the reader slot as the recording ended, nor {:.0} ms later)", d.as_secs_f64() * 1e3),
    };
    vec![
        Check {
            outcome: is_reading(before).into(),
            name: "just before the recording",
            detail: format!("process {}'s ring {}", before.attached_pid, state_text(stats_state(before), before.reader_packet_frames)),
        },
        Check {
            outcome: (is_reading(after) && same).into(),
            name: "just after it",
            detail: format!(
                "process {}'s ring {}; attaches +{}{vacancy_text}",
                after.attached_pid,
                state_text(stats_state(after), after.reader_packet_frames),
                a.attaches.saturating_sub(b.attaches)
            ),
        },
        Check {
            outcome: (r.frames > 0 && copied * 10 >= r.frames * 9).into(),
            name: "the driver copied the recording",
            detail: format!(
                "{copied} frames copied from the ring while {} were recorded ({:.1}%; 90% needed)",
                r.frames,
                percent(copied, r.frames)
            ),
        },
        Check {
            outcome: (r.frames > 0 && r.signal_frames * 10 >= r.frames * 9).into(),
            name: "signal",
            detail: format!(
                "{} of {} frames carry a code ({:.1}%; 90% needed)",
                r.signal_frames,
                r.frames,
                percent(r.signal_frames, r.frames)
            ),
        },
        Check {
            outcome: (r.zeros_inside <= underrun + packet).into(),
            name: "silence inside the signal",
            detail: format!(
                "{} frames in {} runs; the driver underran {underrun_text} over the recording (one {packet}-frame packet more \
                 allowed)",
                r.zeros_inside, r.zero_runs_inside
            ),
        },
        // Silence at the edges too: `before` shows the ring read, so the only silence the recording may hold is an underrun.
        // The statistics' window and the recording's are offset by the engine's pipeline, so an underrun just before `before`
        // can still arrive in the recording: one packet's slack at each end. Silence nothing counted (another capture stream
        // holding the reader slot for part of the recording, say) fails here.
        Check {
            outcome: (silence <= underrun + 2 * packet).into(),
            name: "all the silence",
            detail: format!(
                "{silence} frames ({} before the signal, {} inside, {} after); the driver underran {underrun_text} over the \
                 recording (a {packet}-frame packet more allowed at each end)",
                r.zeros_before_signal, r.zeros_inside, r.zeros_after_signal
            ),
        },
        Check {
            outcome: in_order,
            name: "bit-exact and in order",
            detail: format!(
                "gaps {} ({} frames lost), repeats {} ({} frames), corrupt frames {}; codes {:?} to {:?}, {} steps of one: a gap \
                 is allowed only as the skip log's record, to the frame, of a reader handoff's skip or a skip to a shorter \
                 lag{gaps_text}",
                r.gaps, r.gap_frames, r.repeats, r.repeat_frames, r.corrupt_frames, r.first_code, r.last_code, r.steps_ok
            ),
        },
        log_counts_check(before, after, &window),
        capture_stream_check(&seen.losses),
    ]
}

// ---- the skip log against a recording (verify) ----

/// The skip log over a recording: the entries of the copies STATS gave just before and just after it (after's where both hold
/// one: a late read's ring span may have grown), oldest first; the log's counts at the two (`before`, `after`); how many of
/// the events between were in neither copy (`lost`: more of them than the log keeps, or an entry unreadable), so their entries
/// can't be read and the window can't be judged by them; and the entries read from before's copy alone that were still being
/// filled in there (`stale`: after's no longer holds them, so their ring span may have grown since).
#[derive(Debug, Default)]
struct LogWindow {
    entries: Vec<Event>,
    before: u64,
    after: u64,
    lost: u64,
    stale: Vec<u64>,
}

impl LogWindow {
    fn of(before: &Stats, after: &Stats) -> Self {
        let (b, a): (EventLog, EventLog) = (before.skip_events(), after.skip_events());
        let only_before: Vec<Event> = b.entries.iter().filter(|e| !a.entries.iter().any(|x| x.sequence == e.sequence)).copied().collect();
        let stale = only_before.iter().filter(|e| e.flags & EVENTF_PENDING != 0).map(|e| e.sequence).collect();
        let mut entries = a.entries.clone();
        entries.extend(only_before);
        entries.sort_by_key(|e| e.sequence);
        // A log that went back (its device added again) holds none of the window's events.
        let window = if a.logged >= b.logged { a.logged - b.logged } else { a.logged.max(1) };
        let held = entries.iter().filter(|e| e.sequence > b.logged && e.sequence <= a.logged).count() as u64;
        Self { lost: window.saturating_sub(held) + b.unreadable + a.unreadable, entries, before: b.logged, after: a.logged, stale }
    }

    /// An entry still being filled in as the recording ended (pending in the copy STATS gave just after it): the record of no
    /// gap of the recording. A stream's own entries for a gap are settled before it reads the code after the gap: each entry's
    /// claim ends at or before that code's place on the stream's time line, the stream reads it only once the copy holds it,
    /// and the reader's write that brought the copy's end past it settled every claim it passed (AppRingHistoryWrite,
    /// NfVmicAppRingSettleClaims; ours, from the review of 2026-11-24; tools/test_app_ring_model.py asserts it). So where a
    /// pending entry would hold a gap's frames, the gap's record is elsewhere, and verify neither ties a gap to one nor waits
    /// on one. A settled claim is freed and never extended, so an entry that settled stays settled.
    fn settling(&self, e: &Event) -> bool {
        e.flags & EVENTF_PENDING != 0 && !self.stale.contains(&e.sequence)
    }

    /// An entry whose ring span may still be short for a gap of the recording (the true span holds the one logged: the driver
    /// only adds frames to its end, or maps fewer of its start): untracked (the driver's records of the copy, or of the spans
    /// being filled in, ran out, so it stays short), or pending as read only before the recording (it may have grown since).
    fn may_grow(&self, e: &Event) -> bool {
        e.flags & EVENTF_UNTRACKED != 0 || (e.flags & EVENTF_PENDING != 0 && self.stale.contains(&e.sequence))
    }
}

/// A recording's gap tied to the skip log: the outcome; in words the ring's frames it lost, the silence between its codes and
/// the verdict's reason (`text`); every candidate for its record, a line each, where there is more than one or the log can't
/// yet settle one, and each entry still being filled in whose ring frames meet its own, as no candidate (`candidates`); and
/// the entries the candidates name, by number (`entries`), so verify can list the log's records of a loss that no gap names.
#[derive(Debug)]
struct GapTie {
    outcome: Outcome,
    text: String,
    candidates: Vec<String>,
    entries: Vec<u64>,
}

/// The skip log's kinds a gap in a recording of the app ring can be: each takes ring frames from a capture stream.
const RING_SKIPS: [u16; 6] =
    [EVENT_HANDOFF_SKIP, EVENT_SECONDARY_SKIP, EVENT_SECONDARY_JUMP, EVENT_LATE_READ, EVENT_RESYNC, EVENT_LIVE_DISCARD];

/// The reader's ring-wide events: what they skip, every stream skips (the copy holds the ring's frames after them).
fn ring_wide(kind: u16) -> bool {
    kind == EVENT_RESYNC || kind == EVENT_LIVE_DISCARD
}

/// The skips the ring's rules allow a recording: a taker's handoff skip (the owner, 2026-11-20) and a skip to a shorter lag
/// (the owner, 2026-11-23). The skip log's other kinds are the driver's record of a loss ([`loss`]) or of frames no stream
/// got (a resync, a live discard): a gap tied to them fails, as it did before the log, when no counter held it (the slot held
/// while the reader's timer stopped, 2026-11-23: verify's FAIL found it).
fn allowed(kind: u16) -> bool {
    kind == EVENT_HANDOFF_SKIP || kind == EVENT_SECONDARY_SKIP
}

/// The skip log's records of a loss in one capture stream: a late read (the audio the reader then wrote there never reached
/// it) and a jump in its own time line (a late timer's glitch adjustment). The owner, 2026-11-24: a gap of the recording tied
/// to one fails (a demonstrated loss); one that no gap of the recording names (a late timer or a late read with no gap
/// observed in this recording: another stream's, say, or outside the recording) is diagnostic only, listed and not failed
/// ([`unnamed_losses`]).
fn loss(kind: u16) -> bool {
    kind == EVENT_LATE_READ || kind == EVENT_SECONDARY_JUMP
}

/// `e`'s logged ring frames lie within `[from, to]`, or it has none: only then may it hold some of a gap's frames from `from`
/// on as their record, or a part of it, its true span holding the logged one ([`LogWindow::may_grow`]).
fn may_hold(e: &Event, from: u64, to: u64) -> bool {
    e.ring().is_none_or(|(a, b)| b <= a || (a >= from && b <= to))
}

/// The ring frame nearest `near` whose code is `code`, for a feed whose frame n carries the code of n + `start` (its --start).
fn ring_frame(code: u32, start: u64, near: u64) -> u64 {
    let m = signal::CODE_MODULUS;
    let want = (u64::from(code) + m - start % m) % m;
    let ahead = (want + m - near % m) % m;
    if ahead < m / 2 { near.wrapping_add(ahead) } else { near.wrapping_sub(m - ahead) }
}

/// An entry whose ring frames may be short ([`LogWindow::may_grow`]) that may hold a gap's frames ([`may_hold`]) though no
/// chain that ties the gap holds it: what it would make of the gap as its record, or a part of it: pass it (an allowed skip),
/// or fail it (a loss; or an allowed skip with an entry of a loss of its stream's, or of the reader's frames no stream got,
/// within the gap's frames, which would be part of the same record).
struct Rival<'e> {
    entry: &'e Event,
    pass: bool,
    fail: bool,
}

/// Each listed gap of the recording `r` tied to the skip log (verify's rule; tools/test_app_ring_model.py's `ties` and
/// `judge`). The codes either side of a gap name the ring frames it lost: the first code's frame is the one nearest the
/// reader's position just before, and each gap's due code that plus its steps on from the first ([`signal::Gap::offset`]:
/// silence takes none, so no amount of it moves the mapping). Its candidates are the skip log's entries of this attachment
/// with a ring span: one whose span starts at the gap's first frame, then the same stream's following on (or the reader's
/// ring-wide ones), to the gap's end. verify doesn't know its own stream, so the entries are any stream's: a tie proves the
/// driver logged exactly those ring frames as lost, and every chain that ties is a candidate for the gap's record. An entry
/// still being filled in as the recording ended is no gap's record ([`LogWindow::settling`]): it is left out, and listed
/// where its ring frames meet the gap's.
///
/// The owner, 2026-11-24: INCONCLUSIVE only when the ambiguity changes the verdict. A gap chains tie takes their verdict
/// where they agree, each listed where there are several ([`judge_ties`]): Ok where each holds only skips the ring's rules
/// allow ([`allowed`]), Fail where each holds a loss (a late read or a jump: a demonstrated loss, [`loss`]) or frames no
/// stream got (a resync, a live discard), Inconclusive only where they are judged differently (in practice, different
/// streams'), or where an entry whose ring frames may be short may hold the gap's frames instead and would judge it
/// otherwise ([`Rival`]: listed with them). A gap no chain ties fails, unless what the log can't yet say could pass it
/// ([`judge_untied`]). Events no longer in the log weigh only on a gap no chain ties (ours): with any, the log against the
/// counters is INCONCLUSIVE ([`log_counts_check`]), so a recording can't pass on a tie they might have contradicted; and a
/// tie to a loss fails, the driver's record of those very frames.
fn tie_gaps(before: &Stats, r: &CheckReport, start: u64, window: &LogWindow) -> Vec<GapTie> {
    // No code, no gap (the checker sets the two together).
    let Some(first_code) = r.first_code else { return Vec::new() };
    let first = ring_frame(first_code, start, before.attached_read_frame);
    let attach = before.ring.attaches as u32;
    let ours: Vec<&Event> = window.entries.iter().filter(|e| RING_SKIPS.contains(&e.kind) && e.attach == attach).collect();
    let (settling, ours): (Vec<&Event>, Vec<&Event>) = ours.into_iter().partition(|e| window.settling(e));
    let spans: Vec<&Event> = ours.iter().copied().filter(|e| e.ring().is_some_and(|(lo, hi)| hi > lo)).collect();
    // Entries whose true ring span may reach further than the one logged (or that have none yet): frames no chain reaches,
    // or the frames of a gap a chain ties, may be theirs.
    let unsettled: Vec<&Event> = ours.iter().copied().filter(|e| window.may_grow(e)).collect();
    r.gap_list
        .iter()
        .map(|g| {
            let lo = first.wrapping_add(g.offset);
            let hi = lo.wrapping_add(u64::from(g.got.wrapping_sub(g.due)) % signal::CODE_MODULUS);
            let mut short: Vec<(Vec<&Event>, u64)> = Vec::new();
            let mut tied: Vec<Vec<&Event>> = Vec::new();
            for head in spans.iter().filter(|e| e.ring_from == lo) {
                let mut chain = vec![*head];
                let mut at = head.ring_to;
                while at < hi {
                    let next = spans.iter().find(|e| {
                        e.ring_from == at
                            && (e.stream == head.stream || ring_wide(e.kind) || ring_wide(head.kind))
                            && !chain.iter().any(|c| c.sequence == e.sequence)
                    });
                    let Some(&next) = next else { break };
                    chain.push(next);
                    at = next.ring_to;
                }
                if at == hi {
                    tied.push(chain);
                } else {
                    short.push((chain, at));
                }
            }
            let what = format!(
                "ring frames [{lo}, {hi}) ({} frames, the code after them the recording's frame {}{})",
                hi.wrapping_sub(lo),
                g.index,
                if g.silence != 0 { format!(", {} frames of silence before the code after it", g.silence) } else { String::new() }
            );
            let (outcome, why, mut candidates, entries) = if tied.is_empty() {
                judge_untied(&short, &unsettled, (lo, hi), window)
            } else {
                let in_tie = |e: &Event| tied.iter().flatten().any(|c| c.sequence == e.sequence);
                let loss_with = |e: &Event| {
                    spans.iter().any(|s| {
                        s.sequence != e.sequence && !allowed(s.kind) && (s.stream == e.stream || ring_wide(s.kind)) && may_hold(s, lo, hi)
                    })
                };
                let rivals: Vec<Rival<'_>> = unsettled
                    .iter()
                    .copied()
                    .filter(|e| !in_tie(e) && may_hold(e, lo, hi))
                    .map(|e| Rival { entry: e, pass: allowed(e.kind), fail: !allowed(e.kind) || loss_with(e) })
                    .collect();
                let (outcome, why, candidates) = judge_ties(&tied, &rivals);
                let named = tied.iter().flatten().map(|e| e.sequence).chain(rivals.iter().map(|r| r.entry.sequence)).collect();
                (outcome, why, candidates, named)
            };
            for &e in settling.iter().filter(|e| e.ring().is_some_and(|(a, b)| a < hi && b > lo)) {
                candidates.push(format!(
                    "not a candidate: {} meets its frames, but was still being filled in as the recording ended, so it is no \
                     gap's record (a stream's own entries for a gap are settled before it reads the code after it)",
                    entry_brief(e)
                ));
            }
            GapTie { outcome, text: format!("{what}: {why}"), candidates, entries }
        })
        .collect()
}

/// A gap that chains of the skip log's entries tie exactly ([`tie_gaps`]), judged by them and by the entries whose ring
/// frames may be short that may hold its frames instead (`rivals`, [`Rival`]): one chain and no rival, the chain's verdict
/// and reason; otherwise every one listed (a chain's entries' streams, kinds, frames and ring frames, and its verdict; a
/// rival's entry, and what it would make of the gap), and their verdict where they agree, the log not saying whose the gap
/// was but the verdict the same either way; Inconclusive only where they don't (the owner, 2026-11-24).
fn judge_ties(tied: &[Vec<&Event>], rivals: &[Rival<'_>]) -> (Outcome, String, Vec<String>) {
    let judged: Vec<(Outcome, String)> = tied.iter().map(|c| judge_chain(c)).collect();
    if let ([(outcome, why)], []) = (&judged[..], rivals) {
        return (*outcome, why.clone(), Vec::new());
    }
    let mut candidates: Vec<String> = judged
        .iter()
        .map(|(o, why)| format!("candidate: {why}{}", if *o == Outcome::Ok { ": a skip the ring's rules allow" } else { "" }))
        .collect();
    candidates.extend(rivals.iter().map(|r| {
        let would = match (r.pass, r.fail) {
            (true, true) => "pass it, or fail it with the entry of a loss (or of frames no stream got) within its frames",
            (true, false) => "pass it",
            _ => "fail it",
        };
        format!(
            "may be a candidate: {} may hold its frames (its ring frames may be short, or it has none): as its record it would \
             {would}",
            entry_brief(r.entry)
        )
    }));
    let tied_outcome =
        judged.iter().map(|(o, _)| *o).reduce(|a, b| if a == b { a } else { Outcome::Inconclusive }).unwrap_or(Outcome::Fail);
    let rival_differs = match tied_outcome {
        Outcome::Ok => rivals.iter().any(|r| r.fail),
        Outcome::Fail => rivals.iter().any(|r| r.pass),
        Outcome::Inconclusive => false,
    };
    let head = match &judged[..] {
        [(_, why)] => why.clone(),
        _ => {
            let streams: Vec<String> = tied.iter().map(|c| c[0].stream).collect::<BTreeSet<u32>>().iter().map(u32::to_string).collect();
            format!("{} candidates for exactly its frames, of streams {}", tied.len(), streams.join(", "))
        }
    };
    let more = match rivals.len() {
        0 => String::new(),
        1 => ", and an entry whose ring frames may be short that may hold them".to_string(),
        n => format!(", and {n} entries whose ring frames may be short that may hold them"),
    };
    let why = match (tied_outcome, rival_differs) {
        (Outcome::Ok | Outcome::Fail, true) => format!(
            "{head}{more} (listed below): {} may be its record instead, and would {} it: the log doesn't say which, and the \
             verdict depends on it: inconclusive",
            if rivals.len() == 1 { "that entry" } else { "one of them" },
            if tied_outcome == Outcome::Ok { "fail" } else { "pass" }
        ),
        (Outcome::Ok, false) => format!(
            "{head}{more} (listed below), every one a skip the ring's rules allow: the log doesn't say whose the gap was, but the \
             verdict is the same either way"
        ),
        (Outcome::Fail, false) => format!(
            "{head}{more} (listed below), every one the driver's record of a loss or of frames no stream got: the log doesn't \
             say whose the gap was, but it fails either way"
        ),
        (Outcome::Inconclusive, _) => format!(
            "{head}{more} (listed below), judged differently: the log doesn't say whose the gap was, and the verdict depends on \
             it: inconclusive"
        ),
    };
    (if rival_differs { Outcome::Inconclusive } else { tied_outcome }, why, candidates)
}

/// A gap no chain of the skip log's entries ties exactly ([`tie_gaps`]): `short` holds the chains that start at its first
/// frame and reach only part of the way, `unsettled` the entries whose ring span may be short ([`LogWindow::may_grow`]),
/// `(lo, hi)` its ring frames. It fails (no entry for it, or one for its first frame that ends elsewhere), unless what the
/// log can't yet say could pass it (the owner, 2026-11-24: INCONCLUSIVE only when that would change the verdict): a chain
/// that holds only allowed skips so far and may still reach its end (an entry in it may be short, or an unsettled entry of
/// an allowed kind, of its stream and within the rest of the gap's frames, may take it on), an unsettled entry of an allowed
/// kind that may hold the gap's frames, or events missing from the log ([`LogWindow::lost`]), any of which may be an allowed
/// skip that is its record. Where a chain already holds a loss or frames no stream got, or only a loss could complete one
/// or hold the gap's frames, it fails either way. Returns the outcome, the reason, the candidates where the log can't yet
/// settle one (or there are several), and the entries named.
fn judge_untied(
    short: &[(Vec<&Event>, u64)], unsettled: &[&Event], (lo, hi): (u64, u64), window: &LogWindow,
) -> (Outcome, String, Vec<String>, Vec<u64>) {
    let lost = window.lost;
    let mut may_pass = lost != 0;
    let mut open = lost != 0;
    // What an allowed skip that may be short may hold (`pass_rest`: the rest of a chain's frames; `pass_all`: all of the
    // gap's), and why a gap the log can't yet settle fails either way (`held`: what a chain already holds; `loss_rest`,
    // `loss_all`: only a loss may hold the rest, or all of its frames).
    let (mut pass_rest, mut pass_all) = (false, false);
    let mut held: BTreeSet<&str> = BTreeSet::new();
    let (mut loss_rest, mut loss_all) = (false, false);
    let mut candidates = Vec::new();
    let mut entries: Vec<u64> = Vec::new();
    for (chain, at) in short {
        let head = chain[0];
        let (own, why) = judge_chain(chain);
        let grows = chain.iter().any(|e| window.may_grow(e));
        let next: Vec<&Event> = unsettled
            .iter()
            .copied()
            .filter(|e| {
                (e.stream == head.stream || ring_wide(e.kind) || ring_wide(head.kind))
                    && !chain.iter().any(|c| c.sequence == e.sequence)
                    && may_hold(e, *at, hi)
            })
            .collect();
        let passes = own == Outcome::Ok && (grows || next.iter().any(|e| allowed(e.kind)));
        may_pass |= passes;
        pass_rest |= passes;
        if grows || !next.is_empty() {
            open = true;
            if own == Outcome::Fail {
                let no_stream = chain.iter().any(|e| ring_wide(e.kind));
                held.insert(if no_stream { "frames no stream got" } else { "the driver's record of a loss" });
            } else if !passes {
                loss_rest = true;
            }
        }
        entries.extend(chain.iter().chain(&next).map(|e| e.sequence));
        let state = if grows { ", an entry of it untracked or pending as last read (its ring frames may be short)" } else { "" };
        let then = if next.is_empty() {
            String::new()
        } else {
            format!("; {} may take it on", next.iter().map(|&e| entry_brief(e)).collect::<Vec<_>>().join(" or "))
        };
        candidates.push(format!("candidate: {why}: reaches only to {at}{state}{then}"));
    }
    let heads: Vec<&Event> = unsettled.iter().copied().filter(|e| !entries.contains(&e.sequence) && may_hold(e, lo, hi)).collect();
    let heads_pass = heads.iter().any(|e| allowed(e.kind));
    may_pass |= heads_pass;
    pass_all |= heads_pass;
    open |= !heads.is_empty();
    loss_all |= !heads.is_empty() && !heads_pass;
    for &e in &heads {
        candidates.push(format!("candidate: {} may hold its frames (its ring frames may be short, or it has none yet)", entry_brief(e)));
        entries.push(e.sequence);
    }
    let base = match short.first() {
        None if !open => "no entry in the skip log for it".to_string(),
        None => "no entry in the skip log starts at its first frame".to_string(),
        Some((chain, at)) => {
            let list = chain.iter().map(|&e| entry_brief(e)).collect::<Vec<_>>().join(", then ");
            format!("the skip log's {list} for its first frame reach to {at}, not its end")
        }
    };
    let frames = |rest: bool, all: bool| match (rest, all) {
        (true, true) => "the rest of its frames, or all of them",
        (true, false) => "the rest of its frames",
        _ => "its frames",
    };
    let (outcome, tail) = if may_pass && lost != 0 {
        (
            Outcome::Inconclusive,
            format!(
                ", but {lost} of the log's events over the recording are no longer in it (it keeps {SKIP_LOG_ENTRIES}), and one \
                 of them may be an allowed skip that is its record: inconclusive"
            ),
        )
    } else if may_pass {
        (
            Outcome::Inconclusive,
            format!(
                ", but an allowed skip whose ring frames may be short (untracked: the driver's records ran out; or pending as \
                 last read, before the recording) may hold {}, and would pass it (listed below): inconclusive",
                frames(pass_rest, pass_all)
            ),
        )
    } else if open {
        let mut why: Vec<String> = Vec::new();
        if !held.is_empty() {
            why.push(format!(
                "a chain for its first frame already holds {}, whatever an entry whose ring frames may be short adds to it",
                held.into_iter().collect::<Vec<_>>().join(" and ")
            ));
        }
        if loss_rest || loss_all {
            why.push(format!(
                "an entry whose ring frames may be short could hold {} only as the driver's record of a loss",
                frames(loss_rest, loss_all)
            ));
        }
        (Outcome::Fail, format!("; {} (listed below): it fails either way", why.join("; and ")))
    } else {
        (Outcome::Fail, String::new())
    };
    // One short chain the log has settled: the reason names it, and there is nothing more to list.
    if !open && short.len() < 2 {
        candidates.clear();
    }
    (outcome, format!("{base}{tail}"), candidates, entries)
}

/// A chain of entries that ties a gap (or reaches part of the way), judged, with its entries in words: Ok only if every entry
/// is a skip the ring's rules allow ([`allowed`]); a loss ([`loss`]) or frames no stream got (a resync, a live discard) fail it.
fn judge_chain(chain: &[&Event]) -> (Outcome, String) {
    let list = chain.iter().map(|&e| entry_brief(e)).collect::<Vec<_>>().join(", then ");
    if chain.iter().any(|e| e.kind == EVENT_RESYNC) {
        (Outcome::Fail, format!("the skip log's {list}: the feed's write position was invalid, and those frames reached no stream"))
    } else if chain.iter().any(|e| e.kind == EVENT_LIVE_DISCARD) {
        (Outcome::Fail, format!("the skip log's {list}: a live feed's first pass discarded them, and they reached no stream"))
    } else if let Some(e) = chain.iter().find(|e| !allowed(e.kind)) {
        (
            Outcome::Fail,
            format!(
                "the skip log's {list}: the driver's record of a loss (a {}), not a skip the ring's rules allow (a taker's \
                 handoff skip, or a skip to a shorter lag)",
                e.kind_name()
            ),
        )
    } else {
        (Outcome::Ok, format!("the skip log's {list}"))
    }
}

/// An entry in a few words: its number, kind, stream, frames and ring frames (and whether they may be short).
fn entry_brief(e: &Event) -> String {
    let ring = match e.ring() {
        Some((lo, hi)) => format!("ring [{lo}, {hi})"),
        None => "no ring frames".to_string(),
    };
    let state = match (e.flags & EVENTF_PENDING != 0, e.flags & EVENTF_UNTRACKED != 0) {
        (true, true) => ", pending, untracked",
        (true, false) => ", pending",
        (false, true) => ", untracked",
        (false, false) => "",
    };
    format!("#{} {} (stream {}, {} frames, {ring}{state})", e.sequence, e.kind_name(), e.stream, e.frames)
}

/// The skip log's records of a loss over the recording ([`loss`]: a late read, a jump in a stream's own time line) that no
/// gap of it names ([`GapTie::entries`]: no chain that ties or reaches into a gap holds it, and it is no entry whose ring
/// frames may be short that may hold a gap's). Diagnostic only (the owner, 2026-11-24): verify lists them and fails nothing
/// on them. Whose they are, the log doesn't say: another stream's, or of frames outside the recording, or with no ring frames
/// (the reader underran there too: silence, nothing lost), or, where a gap failed for want of its record, maybe part of it.
fn unnamed_losses<'w>(window: &'w LogWindow, ties: &[GapTie]) -> Vec<&'w Event> {
    window
        .entries
        .iter()
        .filter(|e| e.sequence > window.before && e.sequence <= window.after && loss(e.kind))
        .filter(|e| !ties.iter().any(|t| t.entries.contains(&e.sequence)))
        .collect()
}

/// The cross-check: the skip log's skips over the recording add up to the counters' rise, kind by kind, in number and in
/// frames (STATS copies the two at the same instant, so they agree exactly): the time-line jumps too, by the device's counters
/// of them (the owner, 2026-11-24). INCONCLUSIVE where the window's events weren't all read.
fn log_counts_check(before: &Stats, after: &Stats, window: &LogWindow) -> Check {
    let name = "the skip log against the counters";
    if window.lost != 0 {
        return Check {
            outcome: Outcome::Inconclusive,
            name,
            detail: format!(
                "+{} events over the recording, {} of them no longer in the log (it keeps {}): the counters can't be checked by it",
                window.after.saturating_sub(window.before),
                window.lost,
                SKIP_LOG_ENTRIES
            ),
        };
    }
    let rise = |a: u64, b: u64| a.saturating_sub(b);
    #[rustfmt::skip]
    let kinds = [
        (EVENT_HANDOFF_SKIP, "reader handoffs", rise(after.reader_handoffs, before.reader_handoffs),
         Some(rise(after.handoff_skipped_frames, before.handoff_skipped_frames))),
        (EVENT_SECONDARY_SKIP, "skips to a shorter lag", rise(after.secondary_skips, before.secondary_skips),
         Some(rise(after.secondary_skipped_frames, before.secondary_skipped_frames))),
        (EVENT_SECONDARY_JUMP, "time-line jumps", rise(after.secondary_jumps, before.secondary_jumps),
         Some(rise(after.secondary_jump_frames, before.secondary_jump_frames))),
        (EVENT_LOOPBACK_LEAD_SKIP, "loopback lead skips", rise(after.loopback_lead_skips, before.loopback_lead_skips),
         Some(rise(after.loopback_lead_skipped_frames, before.loopback_lead_skipped_frames))),
        (EVENT_RESYNC, "resyncs", rise(after.ring.resyncs, before.ring.resyncs), None),
    ];
    let mut ok = true;
    let parts: Vec<String> = kinds
        .iter()
        .map(|&(kind, what, count, frames)| {
            let logged: Vec<&Event> =
                window.entries.iter().filter(|e| e.kind == kind && e.sequence > window.before && e.sequence <= window.after).collect();
            let logged_frames: u64 = logged.iter().map(|e| u64::from(e.frames)).sum();
            let agree = logged.len() as u64 == count && frames.is_none_or(|f| f == logged_frames);
            ok &= agree;
            match frames {
                Some(f) => format!("{what} {} logged, +{count} counted ({logged_frames} and {f} frames)", logged.len()),
                None => format!("{what} {} logged, +{count} counted", logged.len()),
            }
        })
        .collect();
    Check { outcome: ok.into(), name, detail: parts.join("; ") }
}

/// An entry as `stats` and verify print it: its number, its time before the snapshot (`now`), kind, stream and packet, its
/// frames and spans, and its flags.
fn event_text(e: &Event, now: u64, frequency: u64) -> String {
    // The driver reads QpcNow last (Control.cpp), so no entry is after it; one that is (a driver that read it first) shows 0.
    let ago = if frequency == 0 { 0.0 } else { (now.wrapping_sub(e.qpc) as i64).max(0) as f64 / frequency as f64 };
    let ring = match e.ring() {
        Some((lo, hi)) if e.flags & EVENTF_BACKWARD != 0 => format!(", ring {lo} back to {hi}"),
        Some((lo, hi)) => format!(", ring [{lo}, {hi}) ({} frames)", hi.saturating_sub(lo)),
        None => String::new(),
    };
    let attach = if e.attach != 0 { format!(", attachment {}", e.attach) } else { String::new() };
    let flags: Vec<&str> = [
        (EVENTF_READER, "reader"),
        (EVENTF_PAUSING, "pausing"),
        (EVENTF_PENDING, "pending"),
        (EVENTF_UNTRACKED, "untracked"),
        (EVENTF_BEHIND, "behind"),
        (EVENTF_GAP, "copy gap"),
        (EVENTF_OLD, "old"),
    ]
    .into_iter()
    .filter_map(|(bit, word)| (e.flags & bit != 0).then_some(word))
    .collect();
    format!(
        "#{} -{ago:.3} s {}: stream {} ({}-frame packets{}), {} frames, span {} + {}{ring}{attach}",
        e.sequence,
        e.kind_name(),
        e.stream,
        e.packet_frames,
        if flags.is_empty() { String::new() } else { format!("; {}", flags.join(", ")) },
        e.frames,
        e.span_from,
        e.span_frames
    )
}

/// What [`record`] returns: STATS just before and just after the recording, the checker over its frames, how long it waited
/// after it for a stream to hold the reader slot (where none did as it ended), and the capture stream's own losses split at
/// the recording's start and end ([`StreamLosses`]).
struct Recorded {
    before: Stats,
    after: Stats,
    checker: SequenceChecker,
    vacancy: Option<Duration>,
    losses: StreamLosses,
}

/// A capture stream's own counts ([`Capture`]): frames the engine lost (its device position skipped them: the reader was
/// late) and discontinuities after its first packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Losses {
    frames: u64,
    discontinuities: u64,
}

impl Losses {
    /// What these counts added since `earlier`'s.
    fn since(self, earlier: Self) -> Self {
        Self {
            frames: self.frames.saturating_sub(earlier.frames),
            discontinuities: self.discontinuities.saturating_sub(earlier.discontinuities),
        }
    }

    fn any(self) -> bool {
        self != Self::default()
    }
}

/// The capture stream's losses around a recording: those its packets before the recording carried (the wait for the signal
/// and the reader, and the drain for the STATS read just before), those the recording's own packets carried, and those of the
/// packets after it (the drain for the STATS read just after, and any wait for a stream to hold the reader slot). Those after
/// it aren't the recording's (the owner, 2026-11-24, agreeing Astra's fix: the counts taken as the measured recording ends,
/// so a loss in the drain after it can't fail a clean recording), and are information; those before it are judged with it,
/// as they were before the fix, but told apart ([`capture_stream_check`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StreamLosses {
    earlier: Losses,
    recording: Losses,
    later: Losses,
}

impl StreamLosses {
    /// From the stream's running counts as the recording started (`start`), as it ended (`end`), and at the last read (`last`).
    fn of(start: Losses, end: Losses, last: Losses) -> Self {
        Self { earlier: start, recording: end.since(start), later: last.since(end) }
    }

    /// What the capture stream's check judges: the losses up to the recording's end.
    fn judged(&self) -> Losses {
        Losses {
            frames: self.earlier.frames + self.recording.frames,
            discontinuities: self.earlier.discontinuities + self.recording.discontinuities,
        }
    }
}

/// Waits (draining the stream, up to [`READING_WAIT`]) for STATS to show a capture stream reading the ring and for the stream to
/// deliver the signal, then records for `duration`: STATS just before and just after, and the checker over the frames between.
/// (The wait for the signal skips whatever the engine delivers before the driver's first packets reach it.) No recording if
/// neither came. Where just after it the same ring is attached but no stream holds the reader slot (the last reader stopped
/// as the recording ended: three.sh's A2-default, 2026-11-24, the last of three recorders), it drains the stream, unrecorded,
/// and reads STATS again until one does, up to [`VACANCY_WAIT`]: this stream still runs and takes the slot at its next pass.
/// The later STATS is the recording's `after` (a wider window takes in nothing the recording's frames could need). The
/// stream's own losses are split where the measured recording starts and ends ([`StreamLosses`]): what the drains after it
/// carried isn't the recording's.
fn record(capture: &mut Capture, duration: Duration) -> Result<Recorded, String> {
    let t0 = Instant::now();
    let mut signal_seen = false;
    let before = loop {
        capture.read(|f| signal_seen |= signal::decode(f).is_some())?;
        let s = stats_while_draining(capture, |f| signal_seen |= signal::decode(f).is_some())?;
        if (signal_seen && is_reading(&s)) || t0.elapsed() >= READING_WAIT {
            break s;
        }
    };
    let mut checker = SequenceChecker::new();
    let start = capture.losses();
    if signal_seen && is_reading(&before) {
        let t1 = Instant::now();
        while t1.elapsed() < duration {
            capture.read(|f| checker.feed(f))?;
        }
    } else {
        let wait = READING_WAIT.as_secs();
        let what = if signal_seen { "the driver reported no capture stream reading the ring" } else { "the stream delivered no signal" };
        eprintln!("{what} within {wait} s of the stream's start: not recorded");
    }
    // The measured recording ends here: the drains below are for STATS and the reader slot, and record nothing.
    let end = capture.losses();
    let mut after = stats_while_draining(capture, |_| {})?;
    let mut vacancy = None;
    if slot_vacant(&before, &after) {
        let t = Instant::now();
        while slot_vacant(&before, &after) && t.elapsed() < VACANCY_WAIT {
            capture.read(|_| {})?;
            after = stats_while_draining(capture, |_| {})?;
        }
        vacancy = Some(t.elapsed());
    }
    Ok(Recorded { before, after, checker, vacancy, losses: StreamLosses::of(start, end, capture.losses()) })
}

/// STATS, read on a helper thread (its own handle) while this thread keeps draining the stream into `on_frame`. A STATS
/// read can take tens of milliseconds now and then on the dev box under Driver Verifier (2026-11-24: about 1% of reads over
/// 1 ms, up to 36 ms, with no stream running; a rejected control code on the same handle never over 0.4 ms, so it's our
/// handler's), longer than a 128-frame shared client's buffer: read on the capture thread, it cost every recording 104 frames
/// at its start (p65 and its rerun, 10 of 12 verifies).
fn stats_while_draining(capture: &mut Capture, mut on_frame: impl FnMut([f32; 2])) -> Result<Stats, String> {
    std::thread::scope(|s| {
        let reader = s.spawn(|| open_device().and_then(|d| read_stats(&d)));
        while !reader.is_finished() {
            capture.read(&mut on_frame)?;
        }
        reader.join().map_err(|_| "the STATS thread panicked".to_string())?
    })
}

/// `after` shows `before`'s attachment still published with no stream holding the reader slot.
fn slot_vacant(before: &Stats, after: &Stats) -> bool {
    after.flags & STATS_ATTACHED != 0
        && after.attached_pid == before.attached_pid
        && after.ring.attaches == before.ring.attaches
        && stats_state(after) == STATE_ATTACHED
        && after.reader_packet_frames == 0
}

/// COM in the multithreaded apartment for the calling thread, uninitialised on drop. Made first, so it drops after the
/// wasapi objects.
struct Com(PhantomData<*const ()>);

impl Com {
    fn mta() -> Result<Self, String> {
        let hr = wasapi::initialize_mta();
        if hr.is_ok() { Ok(Self(PhantomData)) } else { Err(format!("COM (MTA) refused: {hr:?}")) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

/// MMCSS "Pro Audio" for the calling thread, reverted on drop. Best effort: without it the thread runs at normal priority.
struct ProAudio {
    handle: Option<HANDLE>,
    _not_send: PhantomData<*const ()>,
}

impl ProAudio {
    fn join() -> Self {
        let mut task_index = 0u32;
        // SAFETY: a static task name; `task_index` outlives the call.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
        if handle.is_none() {
            eprintln!("note: MMCSS \"Pro Audio\" refused; this thread runs at normal priority");
        }
        Self { handle, _not_send: PhantomData }
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        if let Some(h) = self.handle {
            // SAFETY: the same thread (ProAudio is !Send); `h` came from AvSetMmThreadCharacteristicsW.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// A 1 ms wait: a high-resolution waitable timer (Windows 10 1803 and up), or a plain sleep without one.
struct Tick(Option<Owned<HANDLE>>);

impl Tick {
    fn new() -> Self {
        // SAFETY: an unnamed timer, default security.
        let Ok(h) = (unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) })
        else {
            return Self(None);
        };
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        Self(Some(unsafe { Owned::new(h) }))
    }

    fn wait(&self) {
        if let Some(timer) = &self.0 {
            // 1 ms from now (negative: relative, in 100 ns units).
            let due = -10_000i64;
            // SAFETY: our timer; `due` outlives the call; no completion routine.
            if unsafe { SetWaitableTimer(**timer, &due, 0, None, None, false) }.is_ok() {
                // SAFETY: our timer.
                let _ = unsafe { WaitForSingleObject(**timer, 1_000) };
                return;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// The endpoint's master volume (scalar), mute, and which of them its hardware (the driver's nodes) implements.
#[derive(Clone, Copy, Debug)]
struct Volume {
    level: f32,
    muted: bool,
    /// ENDPOINT_HARDWARE_SUPPORT_* bits (0 where the endpoint doesn't say).
    hardware: u32,
}

/// An event-driven capture stream (`open_client`): verify's, kill-test's and latency's (shared float32 stereo 48 kHz, the
/// endpoint's mix format: no conversion), or `open`'s, in the format and mode asked for. It reads packets straight from the
/// engine's buffer through IAudioCaptureClient (GetBuffer's device QPC position, and the QPC just after the call returned).
/// `!Send` (the COM objects): made, used and dropped on one thread.
struct Capture {
    opened: Opened,
    capture: IAudioCaptureClient,
    event: Handle,
    volume: Option<Volume>,
    next_index: Option<u64>,
    frames: u64,
    packets: u64,
    silent_packets: u64,
    /// Frames the stream's device position skipped (the engine dropped them: this reader was late).
    lost_frames: u64,
    discontinuities: u64,
    longest_wait: Duration,
}

/// One capture packet, as GetBuffer handed it out (the bytes are the engine's, until ReleaseBuffer).
struct Packet<'a> {
    bytes: &'a [u8],
    frames: u32,
    /// AUDCLNT_BUFFERFLAGS_SILENT: the packet is silence, whatever its bytes.
    silent: bool,
    /// AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY: Windows says this packet doesn't follow on from the one before.
    discontinuity: bool,
    /// GetBuffer's device QPC position, in 100 ns units: the time the endpoint gives for the packet's first frame (`None`
    /// where it flagged AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR).
    qpc_hns: Option<u64>,
    /// QueryPerformanceCounter, read just after GetBuffer returned (ticks).
    host_qpc: u64,
    /// GetBuffer's device position: the stream frame of the packet's first frame.
    position: u64,
}

impl Packet<'_> {
    /// The frames as float32 stereo (a float stream's).
    fn float_frames(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        let (chunks, _) = self.bytes.as_chunks::<8>();
        chunks.iter().map(|&[a, b, c, d, e, f, g, h]| [f32::from_le_bytes([a, b, c, d]), f32::from_le_bytes([e, f, g, h])])
    }
}

fn werr(e: wasapi::WasapiError) -> String {
    e.to_string()
}

impl Capture {
    /// verify's, kill-test's and latency's stream on the capture endpoint `name`: shared float32, as `spec` says (its mode and
    /// IAudioClient3 period), after a check that the endpoint mixes at 48 kHz float32 stereo (bit-exact needs it).
    fn open(name: &str, spec: StreamSpec) -> Result<Self, String> {
        let device = find_endpoint(name, Direction::Capture)?;
        let mix = device.get_iaudioclient().map_err(werr)?.get_mixformat().map_err(werr)?;
        let float = matches!(mix.get_subformat(), Ok(SampleType::Float));
        if mix.get_samplespersec() != protocol::SAMPLE_RATE || mix.get_nchannels() != 2 || mix.get_bitspersample() != 32 || !float {
            return Err(format!("{name:?} mixes at {}: bit-exact needs a 48 kHz float32 stereo device format", describe_format(&mix)));
        }
        if spec.period_frames.is_some()
            && let Some(periods) = engine_periods(&device, &spec.format.wave())
        {
            println!("note: {periods}");
        }
        let volume = endpoint_volume(&device);
        let opened = open_client(&device, Direction::Capture, &spec).map_err(|r| format!("{name:?} refused the stream: {r}"))?;
        for note in &opened.notes {
            eprintln!("note: {note}");
        }
        Self::new(opened, volume).map_err(|r| format!("{name:?}: {r}"))
    }

    /// The capture side of an opened client: its event, and its IAudioCaptureClient.
    fn new(opened: Opened, volume: Option<Volume>) -> Result<Self, Refusal> {
        let event = opened.client.set_get_eventhandle().map_err(|e| Refusal::at("SetEventHandle", e))?;
        // SAFETY: a COM call on an initialised client (open_client), which hands out its capture interface, reference-counted.
        let capture = unsafe { opened.client.as_iaudioclient().GetService::<IAudioCaptureClient>() }
            .map_err(|e| Refusal::com("GetService(IAudioCaptureClient)", &e))?;
        Ok(Self {
            opened,
            capture,
            event,
            volume,
            next_index: None,
            frames: 0,
            packets: 0,
            silent_packets: 0,
            lost_frames: 0,
            discontinuities: 0,
            longest_wait: Duration::ZERO,
        })
    }

    fn describe(&self) -> String {
        let volume = match self.volume {
            Some(v) => format!("endpoint volume {:.0}%, {}", v.level * 100.0, if v.muted { "MUTED" } else { "unmuted" }),
            None => "endpoint volume unknown".to_string(),
        };
        format!("float32 stereo 48 kHz, {}; {volume}", self.opened.describe())
    }

    /// The volume settings that matter here: mute, which the driver has no node for (so Windows applies it to the samples), and
    /// the level, which the driver's volume node takes without applying it to the app ring's samples.
    fn print_volume_notes(&self) {
        let Some(v) = self.volume else {
            println!("note: the endpoint's volume and mute are unknown");
            return;
        };
        if v.muted {
            eprintln!("warning: the endpoint is muted, which can zero the stream: unmute it (Sound settings, the microphone's properties)");
        }
        if v.hardware & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0 {
            println!(
                "note: the endpoint's level is {:.0}%, set on the driver's volume node, which the driver doesn't apply to the app \
                 ring's samples: the level shouldn't matter, but a result that isn't bit-exact with it below 100% would point there",
                v.level * 100.0
            );
        } else if v.level < 1.0 {
            eprintln!(
                "warning: the endpoint's level is {:.0}% and it reports no hardware volume, so Windows may scale the samples itself: \
                 set it to 100%",
                v.level * 100.0
            );
        }
    }

    fn start(&self) -> Result<(), String> {
        self.start_stream().map_err(|r| r.to_string())
    }

    /// Start, its failure a refusal (for `open`: a refusal may surface only once the stream starts).
    fn start_stream(&self) -> Result<(), Refusal> {
        self.opened.client.start_stream().map_err(|e| Refusal::at("Start", e))
    }

    fn stop(&self) {
        let _ = self.opened.client.stop_stream();
    }

    /// Waits for the stream's event (2 s at most), then hands every packet that's ready to `on_packet`, straight from the
    /// engine's buffer, with the QPC read just after its GetBuffer returned. Counts the packets, their frames, the silent ones,
    /// frames the engine dropped (the device position skipped them) and discontinuities. Allocates nothing but an error's text.
    fn read_packets(&mut self, mut on_packet: impl FnMut(&Packet<'_>)) -> Result<(), String> {
        let t = Instant::now();
        self.event.wait_for_event(2_000).map_err(|_| "no capture event for 2 s: is the endpoint's stream running?".to_string())?;
        self.longest_wait = self.longest_wait.max(t.elapsed());
        loop {
            let (mut data, mut frames, mut flags, mut position, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64, 0u64);
            // SAFETY: a COM call on our initialised capture client; the out-pointers are locals that outlive it.
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, Some(&mut position), Some(&mut qpc)) }
                .map_err(|e| format!("GetBuffer returned {}", hresult::name(e.code())))?;
            let host_qpc = qpc_now();
            if frames == 0 {
                // AUDCLNT_S_BUFFER_EMPTY. Releasing 0 frames is always allowed (wasapi's read_from_device does the same).
                // SAFETY: as above.
                let _ = unsafe { self.capture.ReleaseBuffer(0) };
                return Ok(());
            }
            self.packets += 1;
            self.frames += u64::from(frames);
            if let Some(expect) = self.next_index
                && position > expect
            {
                self.lost_frames += position - expect;
            }
            // The stream's first packet may carry the discontinuity flag (the engine's start, not a glitch: inference from
            // reports of it, not checked on this endpoint), so only later ones count.
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && self.next_index.is_some() {
                self.discontinuities += 1;
            }
            self.next_index = Some(position + u64::from(frames));
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
            let qpc_hns = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0).then_some(qpc);
            self.silent_packets += u64::from(silent);
            // SAFETY: GetBuffer handed out `frames` frames at `data`, each FRAME_BYTES long (both formats' block align: two
            // 32-bit samples), the engine's until ReleaseBuffer below; nothing writes them meanwhile.
            let bytes = unsafe { std::slice::from_raw_parts(data, frames as usize * protocol::FRAME_BYTES as usize) };
            on_packet(&Packet { bytes, frames, silent, discontinuity, qpc_hns, host_qpc, position });
            // SAFETY: releases all of the packet just read; `bytes` isn't used past here.
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer returned {}", hresult::name(e.code())))?;
        }
    }

    /// [`Self::read_packets`], frame by frame as float32 stereo: a silent packet as that many frames of silence.
    fn read(&mut self, mut sink: impl FnMut([f32; 2])) -> Result<(), String> {
        self.read_packets(|p| {
            if p.silent {
                (0..p.frames).for_each(|_| sink([0.0; 2]));
            } else {
                p.float_frames().for_each(&mut sink);
            }
        })
    }

    /// Its losses so far: frames the engine lost, and discontinuities after its first packet.
    fn losses(&self) -> Losses {
        Losses { frames: self.lost_frames, discontinuities: self.discontinuities }
    }

    fn print_counters(&self) {
        println!(
            "capture: {} packets, {} frames, {} silent packets, {} frames lost by the engine, {} discontinuities, longest wait {:.1} ms",
            self.packets,
            self.frames,
            self.silent_packets,
            self.lost_frames,
            self.discontinuities,
            self.longest_wait.as_secs_f64() * 1000.0
        );
    }
}

/// An event-driven render stream that plays silence (`open --render`). `!Send`, like [`Capture`].
struct Render {
    opened: Opened,
    render: IAudioRenderClient,
    event: Handle,
    packets: u64,
    frames: u64,
    longest_wait: Duration,
    /// The next frame of the test signal to write (a float stream's `--signal`), or `None`: silence.
    signal: Option<u64>,
}

impl Render {
    fn new(opened: Opened, signal: Option<u64>) -> Result<Self, Refusal> {
        let event = opened.client.set_get_eventhandle().map_err(|e| Refusal::at("SetEventHandle", e))?;
        // SAFETY: a COM call on an initialised client (open_client), which hands out its render interface, reference-counted.
        let render = unsafe { opened.client.as_iaudioclient().GetService::<IAudioRenderClient>() }
            .map_err(|e| Refusal::com("GetService(IAudioRenderClient)", &e))?;
        Ok(Self { opened, render, event, packets: 0, frames: 0, longest_wait: Duration::ZERO, signal })
    }

    /// Fills what the buffer has free with silence, or the test signal (`signal`, float only: open_args checks): an exclusive
    /// stream's whole buffer (its period), a shared one's free space. A failure names its call (a refusal, where it is the
    /// fill before the start).
    fn fill(&mut self) -> Result<(), Refusal> {
        let o = &self.opened;
        let free = if o.exclusive {
            o.buffer_frames
        } else {
            o.buffer_frames.saturating_sub(o.client.get_current_padding().map_err(|e| Refusal::at("GetCurrentPadding", e))?)
        };
        if free == 0 {
            return Ok(());
        }
        // SAFETY: a COM call on our initialised render client, for no more than the free frames.
        let data = unsafe { self.render.GetBuffer(free) }.map_err(|e| Refusal::com("GetBuffer", &e))?;
        let flags = match self.signal {
            Some(first) => {
                // SAFETY: GetBuffer handed out `free` frames at `data` for us to write until ReleaseBuffer, and a float stereo
                // frame (the only format --signal opens) is FRAME_BYTES bytes.
                let bytes = unsafe { std::slice::from_raw_parts_mut(data, free as usize * protocol::FRAME_BYTES as usize) };
                write_signal(bytes, first);
                self.signal = Some(first + u64::from(free));
                0
            }
            // Marked silent: the engine treats the frames as silence, so nothing is written.
            None => AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
        };
        // SAFETY: releases the frames just obtained, written above or marked silent.
        unsafe { self.render.ReleaseBuffer(free, flags) }.map_err(|e| Refusal::com("ReleaseBuffer", &e))?;
        self.packets += 1;
        self.frames += u64::from(free);
        Ok(())
    }

    /// Waits for the stream's event (2 s at most), then fills the buffer.
    fn wait_and_fill(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.event.wait_for_event(2_000).map_err(|_| "no render event for 2 s: is the endpoint's stream running?".to_string())?;
        self.longest_wait = self.longest_wait.max(t.elapsed());
        self.fill().map_err(|r| r.to_string())
    }

    /// What the stream writes, for its status lines.
    fn what(&self) -> &'static str {
        if self.signal.is_some() { "of the test signal" } else { "of silence" }
    }
}

/// The test signal's frames from frame `first` into `dest`, float32 stereo little-endian, as many whole frames as it holds.
fn write_signal(dest: &mut [u8], first: u64) {
    for (n, f) in (first..).zip(dest.as_chunks_mut::<8>().0) {
        let [left, right] = signal::frame(n);
        f[..4].copy_from_slice(&left.to_le_bytes());
        f[4..].copy_from_slice(&right.to_le_bytes());
    }
}

/// The endpoint's master volume (scalar), mute and hardware support, where it reports them.
fn endpoint_volume(device: &wasapi::Device) -> Option<Volume> {
    // SAFETY: COM is initialised on this thread (the caller holds a Com); the interface is released when it drops.
    unsafe {
        let volume: IAudioEndpointVolume = device.as_immdevice().Activate(CLSCTX_ALL, None).ok()?;
        Some(Volume {
            level: volume.GetMasterVolumeLevelScalar().ok()?,
            muted: volume.GetMute().ok()?.as_bool(),
            hardware: volume.QueryHardwareSupport().unwrap_or(0),
        })
    }
}

// ---- stats ----

fn stats(args: &[String]) -> Result<Verdict, String> {
    let o = Opts::parse(args, &["--time"], &[])?;
    let device = open_device()?;
    let reads: usize = o.value("--time", 0)?;
    if reads == 0 {
        print_stats(&read_stats(&device)?);
        return Ok(Verdict::Pass);
    }
    let mut us: Vec<f64> = Vec::with_capacity(reads);
    for _ in 0..reads {
        let t = Instant::now();
        read_stats(&device)?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    us.sort_by(f64::total_cmp);
    let at = |q: f64| us[((us.len() - 1) as f64 * q).round() as usize];
    println!(
        "{reads} STATS reads of {} bytes: min {:.1} us, median {:.1}, p99 {:.1}, max {:.1}",
        std::mem::size_of::<Stats>(),
        at(0.0),
        at(0.5),
        at(0.99),
        at(1.0)
    );
    Ok(Verdict::Pass)
}

fn print_stats(s: &Stats) {
    let mhz = s.qpc_frequency as f64 / 1e6;
    println!("driver statistics (protocol v{}), QPC {mhz:.3} MHz", s.version);
    if s.flags & STATS_ATTACHED != 0 {
        let fill = s.attached_write_frame.wrapping_sub(s.attached_read_frame) as i64;
        println!(
            "app ring: process {}'s, {} frames, {}; read {}, write {} (fill {fill} at the last pass)",
            s.attached_pid,
            s.attached_capacity_frames,
            state_text(stats_state(s), s.reader_packet_frames),
            s.attached_read_frame,
            s.attached_write_frame
        );
    } else {
        println!("app ring: none attached");
    }
    let r = &s.ring;
    println!(
        "  attaches {}, detaches {} (cancel {}, cleanup {}, removal {}); ATTACHes refused: busy {}, invalid {}, format {} (a capture \
         stream open in another format)",
        r.attaches, r.detaches, r.detach_cancel, r.detach_cleanup, r.detach_removal, r.busy_rejects, r.invalid_rejects, r.format_rejects
    );
    println!("  capture stream creations refused {} (a ring attached, the stream in another format)", r.refused_capture_opens);
    println!(
        "  passes {}, frames copied {}, underruns {} packets / {} frames, resyncs {}, format mismatch {} packets",
        r.passes, r.frames_copied, r.underrun_packets, r.underrun_frames, r.resyncs, r.mismatch_packets
    );
    println!(
        "  streams that aren't the reader: {} passes; given the ring's audio {} frames, zero-filled {} frames",
        r.not_reader_packets, s.secondary_frames_copied, s.secondary_underrun_frames
    );
    println!(
        "  reader handoffs {} (a stream took the slot another had held), skipping {} frames of the ring's audio",
        s.reader_handoffs, s.handoff_skipped_frames
    );
    println!(
        "  streams playing on through handoffs: moved on to a shorter reader packet's lag {}, skipping {} frames of the ring's audio",
        s.secondary_skips, s.secondary_skipped_frames
    );
    if s.flags & STATS_LOOPBACK != 0 {
        // The device's counts, as its skip log is (valid with STATS_LOOPBACK): the app ring's line, before the loopback's.
        println!(
            "  streams playing the copy whose own time line jumped later (a late timer's glitch adjustment) {}, passing over {} \
             frames the copy held as each was logged (a part past its end then is in the entries' ring frames only)",
            s.secondary_jumps, s.secondary_jump_frames
        );
        let l = &s.loopback;
        println!(
            "loopback: zero-filled packets: idle {}, format mismatch {}, underrun {} ({} frames), overrun {}",
            l.idle_packets, l.mismatch_packets, l.underrun_packets, l.underrun_frames, l.overrun_packets
        );
        println!(
            "  capture anchors {}, render restarts {}, gap fills {}, late packets {}, lost packets {}, second render stream {}",
            l.capture_anchors, l.render_restarts, l.render_gap_fills, l.render_late_packets, l.render_lost_packets, l.render_not_owner
        );
        println!("  stream creations refused {} (a format other than one open on the other endpoint)", l.refused_opens);
        println!(
            "  capture reads moved on to a shorter render lead {}, skipping {} frames of the render audio",
            s.loopback_lead_skips, s.loopback_lead_skipped_frames
        );
    } else {
        println!("loopback: no device");
    }
    print_timer("render", &s.stream[KIND_RENDER], s.qpc_frequency);
    print_timer("capture", &s.stream[KIND_CAPTURE], s.qpc_frequency);
    if s.flags & STATS_LOOPBACK != 0 {
        print_log("skip log (every event a capture stream never got some of its audio in)", &s.skip_events(), s);
        print_log("underrun log (silence with nothing lost, a run an entry)", &s.underrun_events(), s);
    }
}

/// An event log, newest last: each entry with its time before the snapshot and its stream's serial.
fn print_log(what: &str, log: &EventLog, s: &Stats) {
    let kept = if log.entries.len() as u64 == log.logged { String::new() } else { format!(", the latest {} kept", log.entries.len()) };
    let unreadable = if log.unreadable != 0 { format!(", {} unreadable", log.unreadable) } else { String::new() };
    println!("{what}: {} events since the device was added{kept}{unreadable}", log.logged);
    for e in &log.entries {
        println!("  {}", event_text(e, s.qpc_now, s.qpc_frequency));
    }
}

/// The mean working time of a timer pass in microseconds, from the QPC ticks the passes spent.
fn work_us(ticks: u64, passes: u64, frequency: u64) -> f64 {
    if passes == 0 || frequency == 0 { 0.0 } else { ticks as f64 * 1e6 / frequency as f64 / passes as f64 }
}

fn print_timer(kind: &str, t: &StreamStats, frequency: u64) {
    let mean = if t.timer_passes == 0 { 0.0 } else { us(t.lateness_sum_hns) / t.timer_passes as f64 };
    println!(
        "{kind} timer: {} passes in {} runs; glitch adjustments {} ({:.1} ms in all); early {}; lateness mean {mean:.1} us, max {:.1} us; \
         work {:.2} us a pass",
        t.timer_passes,
        t.runs,
        t.glitch_passes,
        us(t.glitch_hns) / 1000.0,
        t.early_passes,
        us(t.lateness_max_hns),
        work_us(t.work_qpc_ticks, t.timer_passes, frequency)
    );
    for (label, count) in LATENESS_LABELS.iter().zip(t.lateness_buckets) {
        println!("  {label:>14}  {count:>12}  {:>8.4}%", percent(count, t.timer_passes));
    }
}

// ---- kill-test ----

/// What kill-test's capture thread saw.
#[derive(Debug, Default)]
struct CaptureSummary {
    frames: u64,
    signal_frames: u64,
    lost_frames: u64,
    discontinuities: u64,
    longest_wait: Duration,
    error: Option<String>,
}

/// Reads the endpoint until `stop`, counting frames (kill-test's "the capture keeps running"). Reports on `ready` once streaming.
fn capture_thread(name: String, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) -> CaptureSummary {
    let _com = match Com::mta() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return CaptureSummary::default();
        }
    };
    let mut capture = match Capture::open(&name, StreamSpec::SHARED_FLOAT).and_then(|c| c.start().map(|()| c)) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return CaptureSummary::default();
        }
    };
    let _ = ready.send(Ok(()));
    let _pro_audio = ProAudio::join();
    let mut summary = CaptureSummary::default();
    while !stop.load(Relaxed) {
        if let Err(e) = capture.read(|f| {
            if !signal::is_silence(f) {
                summary.signal_frames += 1;
            }
        }) {
            summary.error = Some(e);
            break;
        }
    }
    capture.stop();
    summary.frames = capture.frames;
    summary.lost_frames = capture.lost_frames;
    summary.discontinuities = capture.discontinuities;
    summary.longest_wait = capture.longest_wait;
    summary
}

/// Polls STATS until `done` holds or `timeout` passes.
fn wait_for(device: &Device, timeout: Duration, done: impl Fn(&Stats) -> bool) -> Result<Option<Stats>, String> {
    let t0 = Instant::now();
    loop {
        let s = read_stats(device)?;
        if done(&s) {
            return Ok(Some(s));
        }
        if t0.elapsed() >= timeout {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// A job object that terminates its processes when its last handle closes: kill-test's `feed` children go in it, so none
/// outlives kill-test with its ring attached (which would refuse every later ATTACH) however kill-test ends. That covers what
/// [`ChildGuard`]'s Drop can't: a panic, which aborts in the release build (the workspace's `panic = "abort"`, so no Drop runs),
/// and kill-test itself being terminated. The handle isn't inheritable, so only this process holds it.
struct KillJob(Owned<HANDLE>);

impl KillJob {
    fn new() -> Result<Self, String> {
        // SAFETY: an unnamed job with default security.
        let h = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(|e| format!("can't create a job for the children: {e}"))?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let job = Self(unsafe { Owned::new(h) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: our job; `limits` is this class's structure, and its size is passed with it.
        unsafe {
            SetInformationJobObject(*job.0, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size_of_val(&limits) as u32)
        }
        .map_err(|e| format!("can't make the children's job kill them on close: {e}"))?;
        Ok(job)
    }

    fn assign(&self, child: &Child) -> windows::core::Result<()> {
        // SAFETY: our job, and the child's process handle, which std keeps open while `child` lives.
        unsafe { AssignProcessToJobObject(*self.0, HANDLE(child.as_raw_handle())) }
    }
}

/// A `feed` child, in kill-test's [`KillJob`], terminated and waited for (bounded) on drop: every return, `?` and `continue` ends
/// it before the next starts, and the job ends it if kill-test aborts or is terminated. (Between its start and its assignment
/// to the job, a few microseconds, only the guard covers it.) Ctrl+C reaches it too (the same console), and it stops on that.
struct ChildGuard {
    child: Child,
    pid: u32,
}

impl ChildGuard {
    fn spawn(exe: &Path, job: &KillJob) -> Result<Self, String> {
        let child = Command::new(exe)
            .args(["feed", "--seconds", "0", "--quiet"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| format!("can't start the feed child: {e}"))?;
        let guard = Self { pid: child.id(), child };
        // On failure the guard's drop terminates the child and waits for it.
        job.assign(&guard.child).map_err(|e| format!("can't put the feed child (process {}) in kill-test's job: {e}", guard.pid))?;
        Ok(guard)
    }

    /// TerminateProcess.
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// The child's exit status once it has exited, polling for up to `timeout`.
    fn exited_within(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let t0 = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            if t0.elapsed() >= timeout {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Harmless if it has exited. The wait is bounded: a child whose exit waits in the kernel for a cancel that never
        // completes can't be waited out here.
        let _ = self.child.kill();
        if !matches!(self.exited_within(GUARD_WAIT), Ok(Some(_))) {
            eprintln!("warning: the feed child (process {}) still runs {} s after TerminateProcess", self.pid, GUARD_WAIT.as_secs());
        }
    }
}

fn kill_test(args: &[String]) -> Result<Verdict, String> {
    let o = Opts::parse(args, &["--kills", "--min-ms", "--max-ms", "--device"], &["--no-capture"])?;
    let kills: u32 = o.value("--kills", 10)?;
    let min_ms: u64 = o.value("--min-ms", 300)?;
    let max_ms: u64 = o.value("--max-ms", 2_000)?;
    let name: String = o.value("--device", ENDPOINT.to_string())?;
    let no_capture = o.flag("--no-capture");
    let window_ms = READ_WINDOW.as_millis() as u64;
    if !(1..=10_000).contains(&kills) || min_ms < window_ms || min_ms > max_ms || max_ms > 600_000 {
        return Err(format!(
            "--kills from 1 to 10000; --min-ms from {window_ms} (the reading check's window) to --max-ms; --max-ms at most 600000"
        ));
    }
    let device = open_device()?;
    let first = read_stats(&device)?;
    if first.flags & STATS_ATTACHED != 0 {
        return Err(format!("process {} has a ring attached already: stop it first", first.attached_pid));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let reader = if no_capture {
        println!("--no-capture: the reading checks need another capture client on {name:?}; without one the result is INCONCLUSIVE");
        None
    } else {
        let (tx, rx) = mpsc::channel();
        let (stop2, name2) = (Arc::clone(&stop), name.clone());
        let handle = thread::spawn(move || capture_thread(name2, stop2, tx));
        rx.recv().map_err(|_| "the capture thread ended before it started".to_string())??;
        println!("capturing {name:?} throughout");
        Some(handle)
    };

    let exe = std::env::current_exe().map_err(|e| format!("this tool's path: {e}"))?;
    // Made before the first child and dropped after the last guard (each drops inside the loop).
    let job = KillJob::new()?;
    let (mut failures, mut unread) = (0u32, 0u32);
    for k in 1..=kills {
        let start = read_stats(&device)?;
        let mut child = ChildGuard::spawn(&exe, &job)?;
        let pid = child.pid;
        if wait_for(&device, ATTACH_WAIT, |s| s.flags & STATS_ATTACHED != 0 && s.attached_pid == pid)?.is_none() {
            let exit = match child.exited_within(Duration::ZERO) {
                Ok(Some(status)) => format!(" (it exited: {status})"),
                _ => String::new(),
            };
            let now = read_stats(&device)?;
            let format_rejects = now.ring.format_rejects.saturating_sub(start.ring.format_rejects);
            let why = if format_rejects != 0 {
                format!("; the driver refused an ATTACH for the format (+{format_rejects}): {}", protocol::FORMAT_REFUSAL.meaning)
            } else {
                String::new()
            };
            println!("kill {k}/{kills}: FAIL: the child (process {pid}) didn't attach within {} s{exit}{why}", ATTACH_WAIT.as_secs());
            failures += 1;
            continue;
        }
        // Spread over the range, the same each run; the reading check takes the hold's last READ_WINDOW.
        let hold_ms = min_ms + (u64::from(k) * 7_919) % (max_ms - min_ms + 1);
        thread::sleep(Duration::from_millis(hold_ms - window_ms));
        let a = read_stats(&device)?;
        thread::sleep(READ_WINDOW);
        let b = read_stats(&device)?;

        child.kill().map_err(|e| format!("can't terminate process {pid}: {e}"))?;
        let killed = Instant::now();
        let exited = child.exited_within(EXIT_WAIT).map_err(|e| format!("process {pid}: {e}"))?;
        let exit_ms = killed.elapsed().as_millis();
        let (gone, detach_ms) = match exited {
            Some(_) => {
                let t = Instant::now();
                let gone = wait_for(&device, DETACH_WAIT, |s| s.flags & STATS_ATTACHED == 0)?;
                (gone, t.elapsed().as_millis())
            }
            None => (None, 0),
        };
        let after = match &gone {
            Some(s) => *s,
            None => read_stats(&device)?,
        };
        let seen =
            KillSeen { pid, start: &start, a: &a, b: &b, after: &after, exited: exited.is_some(), detached: gone.is_some(), no_capture };
        let v = judge_kill(&seen);
        println!(
            "kill {k}/{kills}: process {pid} held {hold_ms} ms; {} frames read in its last {window_ms} ms ({}); exited {} ms after \
             TerminateProcess, detached {} ms after that; attaches +{}, detaches +{} (cancel +{}, cleanup +{}, removal +{}): {}{}",
            v.read,
            state_text(stats_state(&b), b.reader_packet_frames),
            if exited.is_some() { exit_ms.to_string() } else { "not".into() },
            if gone.is_some() { detach_ms.to_string() } else { "not".into() },
            v.attaches,
            v.detaches,
            v.cancel,
            v.cleanup,
            v.removal,
            if !v.ok {
                "FAIL"
            } else if v.unread {
                "INCONCLUSIVE"
            } else {
                "ok"
            },
            if v.problems.is_empty() { String::new() } else { format!(" ({})", v.problems.join("; ")) }
        );
        failures += u32::from(!v.ok);
        unread += u32::from(v.unread);
        if after.flags & STATS_ATTACHED != 0 {
            println!("the ring is still attached: stopping here (every later child would be refused)");
            break;
        }
    }

    // After the last kill, the capture stream's timer still runs.
    let t1 = read_stats(&device)?;
    thread::sleep(READ_WINDOW);
    let t2 = read_stats(&device)?;
    let timer = t2.stream[KIND_CAPTURE].timer_passes.saturating_sub(t1.stream[KIND_CAPTURE].timer_passes);
    stop.store(true, Relaxed);
    let summary = reader
        .map(|h| h.join().unwrap_or_else(|_| CaptureSummary { error: Some("the capture thread panicked".into()), ..Default::default() }));
    let last = read_stats(&device)?;
    let mut c = Checks::default();
    println!("after the kills:");
    c.check(failures == 0, "the kills", format!("{kills} planned, {failures} failed"));
    c.check(last.flags & STATS_ATTACHED == 0, "no ring left attached", state_text(stats_state(&last), last.reader_packet_frames));
    let timer_runs = match &summary {
        Some(s) => {
            let timer_ok =
                c.check(timer > 0, "the capture timer still runs", format!("+{timer} passes over {window_ms} ms after the last kill"));
            c.check(
                s.error.is_none() && s.lost_frames == 0 && s.discontinuities == 0,
                "the capture stream",
                format!(
                    "{} frames ({} not silence), {} lost by the engine, {} discontinuities after its first packet, longest wait {:.1} ms{}",
                    s.frames,
                    s.signal_frames,
                    s.lost_frames,
                    s.discontinuities,
                    s.longest_wait.as_secs_f64() * 1000.0,
                    s.error.as_deref().map(|e| format!("; ERROR: {e}")).unwrap_or_default()
                ),
            );
            timer_ok
        }
        None => {
            println!("  note the capture timer: +{timer} passes over {window_ms} ms after the last kill (another client's stream, if any)");
            timer > 0
        }
    };
    let (f, l) = (&first.ring, &last.ring);
    println!(
        "attaches +{}, detaches +{} (cancel +{}, cleanup +{}, removal +{}); refused: ATTACH for a format +{}, capture opens +{}",
        l.attaches.saturating_sub(f.attaches),
        l.detaches.saturating_sub(f.detaches),
        l.detach_cancel.saturating_sub(f.detach_cancel),
        l.detach_cleanup.saturating_sub(f.detach_cleanup),
        l.detach_removal.saturating_sub(f.detach_removal),
        l.format_rejects.saturating_sub(f.format_rejects),
        l.refused_capture_opens.saturating_sub(f.refused_capture_opens)
    );
    println!(
        "streams that aren't the reader: +{} passes, given the ring's audio +{} frames, zero-filled +{} frames",
        l.not_reader_packets.saturating_sub(f.not_reader_packets),
        last.secondary_frames_copied.saturating_sub(first.secondary_frames_copied),
        last.secondary_underrun_frames.saturating_sub(first.secondary_underrun_frames)
    );
    let verdict = kill_test_verdict(c.failed, unread, timer_runs);
    match verdict {
        Verdict::Fail | Verdict::RefusedElsewhere => println!("FAIL: {} of the checks", c.failed),
        Verdict::Inconclusive => println!(
            "INCONCLUSIVE: nothing read the ring in {unread} of the kills, or captured the endpoint after them (--no-capture and no \
             other capture client): those kills weren't mid-stream"
        ),
        Verdict::Pass => {
            println!("PASS: {kills} kills mid-stream, each ended by the cancel path within the bounds; the capture timer kept running")
        }
    }
    Ok(verdict)
}

/// What kill-test saw of one kill: STATS before the child started (`start`), at the start and the end of the read window just
/// before TerminateProcess (`a`, `b`), and after it (`after`: once STATS showed no ring attached, or a last snapshot if not).
struct KillSeen<'s> {
    pid: u32,
    start: &'s Stats,
    a: &'s Stats,
    b: &'s Stats,
    after: &'s Stats,
    /// The child exited within [`EXIT_WAIT`] of TerminateProcess.
    exited: bool,
    /// STATS showed no ring attached within [`DETACH_WAIT`] of its exit.
    detached: bool,
    /// `--no-capture`: the reading is another capture client's to do.
    no_capture: bool,
}

/// One kill's verdict, and its numbers.
#[derive(Debug)]
struct KillVerdict {
    /// Every check passed; with `--no-capture`, every one but the reading.
    ok: bool,
    /// `--no-capture` and nothing read the ring: the kill isn't shown to be mid-stream (INCONCLUSIVE, not a failure).
    unread: bool,
    /// Frames the driver copied from the ring over the read window.
    read: u64,
    /// The detach counters' growth from `b` to `after`, and the attach and detach counts' from `start`.
    cancel: u64,
    cleanup: u64,
    removal: u64,
    attaches: u64,
    detaches: u64,
    problems: Vec<String>,
}

/// A kill passes when the child's ring was being read just before it (attached under the child's pid, READING, frames copied
/// over the window), the child exited within the bound and its attachment was gone within the next, and the attachment ended
/// through the cancel path: DetachCancel +1, DetachCleanup and DetachRemoval unchanged, and exactly one attach and one detach.
fn judge_kill(k: &KillSeen<'_>) -> KillVerdict {
    let read = k.b.ring.frames_copied.saturating_sub(k.a.ring.frames_copied);
    let reading = [k.a, k.b].iter().all(|s| is_reading(s) && s.attached_pid == k.pid) && read > 0;
    let (rb, ra, r0) = (&k.b.ring, &k.after.ring, &k.start.ring);
    let cancel = ra.detach_cancel.saturating_sub(rb.detach_cancel);
    let cleanup = ra.detach_cleanup.saturating_sub(rb.detach_cleanup);
    let removal = ra.detach_removal.saturating_sub(rb.detach_removal);
    let attaches = ra.attaches.saturating_sub(r0.attaches);
    let detaches = ra.detaches.saturating_sub(r0.detaches);
    let by_cancel = cancel == 1 && cleanup == 0 && removal == 0;
    let one_attachment = attaches == 1 && detaches == 1;
    let mut problems = Vec::new();
    if !reading {
        problems.push(if k.no_capture { "not read (nothing captures the endpoint?)" } else { "its ring wasn't being read" }.to_string());
    }
    if !k.exited {
        problems.push(format!("still running {} s after TerminateProcess", EXIT_WAIT.as_secs()));
    } else if !k.detached {
        problems.push(format!("still attached {} s after the child's exit", DETACH_WAIT.as_secs()));
    }
    if !by_cancel {
        problems.push("not ended by exactly one cancel".to_string());
    }
    if !one_attachment {
        problems.push("not exactly one attach and one detach".to_string());
    }
    KillVerdict {
        ok: k.exited && k.detached && by_cancel && one_attachment && (reading || k.no_capture),
        unread: !reading && k.no_capture,
        read,
        cancel,
        cleanup,
        removal,
        attaches,
        detaches,
        problems,
    }
}

/// kill-test's verdict: FAIL if any of its checks failed (the kills' among them); otherwise INCONCLUSIVE if a kill wasn't
/// shown mid-stream, or nothing showed the capture timer running after the last kill (`--no-capture` with no other capture
/// client); otherwise PASS.
fn kill_test_verdict(failed_checks: u32, unread_kills: u32, timer_runs: bool) -> Verdict {
    if failed_checks != 0 {
        Verdict::Fail
    } else if unread_kills != 0 || !timer_runs {
        Verdict::Inconclusive
    } else {
        Verdict::Pass
    }
}

// ---- open ----

/// open's options.
#[derive(Debug, PartialEq, Eq)]
struct OpenArgs {
    name: String,
    render: bool,
    spec: StreamSpec,
    seconds: u64,
    /// A render stream's test signal: the first frame's number (`--signal`, `--start`); `None` writes silence.
    signal: Option<u64>,
}

fn open_args(args: &[String]) -> Result<OpenArgs, String> {
    let valued = ["--device", "--format", "--period-frames", "--mode", "--seconds", "--start"];
    let o = Opts::parse(args, &valued, &["--render", "--exclusive", "--shared", "--signal"])?;
    let name: String = o.optional("--device")?.ok_or("--device NAME is required: the endpoint's name, as Sound settings shows it")?;
    let exclusive = match (o.flag("--exclusive"), o.flag("--shared")) {
        (true, false) => true,
        (false, true) => false,
        _ => return Err("one of --exclusive and --shared is required".into()),
    };
    let format = Format::parse(&o.optional::<String>("--format")?.ok_or("--format float|pcm24 is required")?)?;
    let seconds: u64 = o.value("--seconds", 10)?;
    if seconds == 0 || seconds > 86_400 {
        return Err(format!("--seconds {seconds}: from 1 to 86400"));
    }
    let spec = StreamSpec { exclusive, format, period_frames: o.period_frames()?, mode: o.mode(Mode::Default)? };
    let render = o.flag("--render");
    let start: Option<u64> = o.optional("--start")?;
    let signal = match (o.flag("--signal"), start) {
        (false, None) => None,
        (false, Some(_)) => return Err("--start is --signal's: the test signal's first frame".into()),
        (true, _) if !render || format != Format::Float => return Err("--signal needs --render and --format float".into()),
        (true, start) => Some(start.unwrap_or(0)),
    };
    Ok(OpenArgs { name, render, spec, seconds, signal })
}

/// How long open's stream ran once started, the events it served (each waited for and its buffer read or filled), and why it
/// stopped if it stopped before its time.
#[derive(Debug, Default)]
struct OpenRun {
    elapsed: Duration,
    events: u64,
    error: Option<String>,
}

fn open(args: &[String]) -> Result<Verdict, String> {
    let a = open_args(args)?;
    let (dir, kind) = if a.render { (Direction::Render, "render") } else { (Direction::Capture, "capture") };
    // The driver's view, where its control device opens (elevated, the driver loaded): it counts its refusals.
    let driver = match Device::open() {
        Ok(d) => Some(d),
        Err(e) => {
            println!("note: no driver statistics: {} didn't open ({e})", protocol::USER_PATH);
            None
        }
    };
    let stats = || driver.as_ref().and_then(|d| d.stats().ok());
    let before = stats();
    let _com = Com::mta()?;
    let endpoint = find_endpoint(&a.name, dir)?;
    print_endpoint(&endpoint, &a.name, kind, &a.spec);
    println!("asking for {}", a.spec.describe());
    let share = if a.spec.exclusive { "exclusive" } else { "shared" };
    match is_format_supported(&endpoint, &a.spec.format.wave(), a.spec.exclusive) {
        Ok((hr, closest)) => println!(
            "IsFormatSupported ({share}, {}): {}{}",
            a.spec.format.name(),
            hresult::name(hr),
            closest.map(|c| format!("; the closest it offers: {c}")).unwrap_or_default()
        ),
        Err(e) => println!("IsFormatSupported: not asked ({e})"),
    }
    // SAFETY: the handler only stores to an atomic.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) }.is_err() {
        eprintln!("note: no Ctrl+C handler: Ctrl+C ends the run without its summary");
    }
    // The refusal's attribution counts from here: the open and start calls only.
    let before_open = stats();
    let run = open_client(&endpoint, dir, &a.spec).and_then(|opened| {
        for note in &opened.notes {
            println!("note: {note}");
        }
        println!("{}: S_OK; {}", opened.init_call, opened.describe());
        if a.render { run_render(opened, a.seconds, a.signal) } else { run_capture(opened, a.spec.format, a.seconds) }
    });
    let after = stats();
    if let Err(r) = &run {
        println!("{r}");
    }
    print_driver_change(before.as_ref(), after.as_ref());
    let run = match run {
        Ok(run) => run,
        Err(r) => {
            let (verdict, outcome) = refusal_outcome(&r, kind, &refuser(a.render, before_open.as_ref(), after.as_ref()));
            println!("{outcome}");
            return Ok(verdict);
        }
    };
    let secs = run.elapsed.as_secs_f64();
    match run.error {
        None => {
            println!("OUTCOME: OPENED: the {kind} stream ran for {secs:.1} s");
            Ok(Verdict::Pass)
        }
        Some(e) if run.events == 0 => {
            println!("OUTCOME: OPENED and STARTED, but STOPPED before it delivered anything: {e}");
            Ok(Verdict::Inconclusive)
        }
        Some(e) => {
            println!("OUTCOME: OPENED, then STOPPED after {secs:.1} s and {} events: {e}", run.events);
            Ok(Verdict::Inconclusive)
        }
    }
}

/// Who refused open's stream, as the driver's counters say (RefusedCaptureOpens for the app ring's rule, the loopback's
/// RefusedOpens for its own): read just before the open and just after the call that failed.
#[derive(Debug, PartialEq, Eq)]
enum Refuser {
    /// RefusedCaptureOpens rose by this much: the driver refused a capture stream because a ring was attached and the stream's
    /// format wasn't 48 kHz float32 stereo.
    Driver(u64),
    /// The loopback's RefusedOpens rose by this much: the driver refused the stream because the other endpoint has a stream
    /// open in another format.
    Loopback(u64),
    /// Neither rose: something other than the driver's two rules refused it.
    NotTheDriver,
    /// The statistics can't say, for this reason.
    Unknown(&'static str),
}

fn refuser(render: bool, before: Option<&Stats>, after: Option<&Stats>) -> Refuser {
    let (Some(b), Some(a)) = (before, after) else {
        return Refuser::Unknown("no driver statistics from before the open and after it");
    };
    // The capture side checks the app ring's rule first (CaptureStreamOpen), then the loopback's; a render stream meets only
    // the loopback's.
    let ring = if render { 0 } else { a.ring.refused_capture_opens.saturating_sub(b.ring.refused_capture_opens) };
    let both = a.flags & b.flags & STATS_LOOPBACK != 0;
    let loopback = if both { a.loopback.refused_opens.saturating_sub(b.loopback.refused_opens) } else { 0 };
    match (ring, loopback) {
        (0, 0) => Refuser::NotTheDriver,
        (0, n) => Refuser::Loopback(n),
        (n, _) => Refuser::Driver(n),
    }
}

/// open's verdict and outcome line for a refusal: exit 1 only where the driver's counters show one of its format rules (the
/// app ring's or the loopback's) refused the stream; any other refusal exits 4.
fn refusal_outcome(r: &Refusal, kind: &str, who: &Refuser) -> (Verdict, String) {
    match who {
        Refuser::Driver(n) => (
            Verdict::Fail,
            format!(
                "OUTCOME: REFUSED BY THE DRIVER (RefusedCaptureOpens +{n}: a ring is attached and the stream isn't 48 kHz float32 \
                 stereo): {r}; the {kind} stream never ran"
            ),
        ),
        Refuser::Loopback(n) => (
            Verdict::Fail,
            format!(
                "OUTCOME: REFUSED BY THE DRIVER (the loopback's RefusedOpens +{n}: the other endpoint has a stream open in another \
                 format): {r}; the {kind} stream never ran"
            ),
        ),
        Refuser::NotTheDriver => (
            Verdict::RefusedElsewhere,
            format!(
                "OUTCOME: REFUSED, not by the driver's format rules (RefusedCaptureOpens and the loopback's RefusedOpens +0): {r}; \
                 the {kind} stream never ran. Something else refused it: another client holding the device \
                 (AUDCLNT_E_DEVICE_IN_USE), the period asked for, the engine, or, on a device of the driver other than the first \
                 installed, its loopback rule (STATS carries only the first device's loopback counters)"
            ),
        ),
        Refuser::Unknown(why) => {
            (Verdict::RefusedElsewhere, format!("OUTCOME: REFUSED, by whom unknown ({why}): {r}; the {kind} stream never ran"))
        }
    }
}

/// The endpoint's mix format and device periods, and in shared mode IAudioClient3's engine periods for the format asked for.
fn print_endpoint(device: &wasapi::Device, name: &str, kind: &str, spec: &StreamSpec) {
    let client = device.get_iaudioclient().ok();
    let mix = client.as_ref().and_then(|c| c.get_mixformat().ok()).map_or_else(|| "unknown".to_string(), |m| describe_format(&m));
    let period = client.as_ref().and_then(|c| c.get_device_period().ok()).map_or_else(
        || "unknown".to_string(),
        |(default, min)| format!("{:.2} ms default, {:.2} ms minimum", default as f64 / 1e4, min as f64 / 1e4),
    );
    println!("{kind} endpoint {name:?}: mix format {mix}; device period {period}");
    if !spec.exclusive
        && let Some(periods) = engine_periods(device, &spec.format.wave())
    {
        println!("  {periods}");
    }
}

/// What changed in the driver's statistics while open's stream was tried, where its control device opened (`before` read).
fn print_driver_change(before: Option<&Stats>, after: Option<&Stats>) {
    let Some(b) = before else { return };
    let Some(a) = after else {
        println!("the driver: its statistics didn't read after the stream");
        return;
    };
    let (rb, ra) = (&b.ring, &a.ring);
    let now = if a.flags & STATS_ATTACHED != 0 {
        format!("process {}'s ring {}", a.attached_pid, state_text(stats_state(a), a.reader_packet_frames))
    } else {
        "no ring attached".to_string()
    };
    println!(
        "the driver: capture opens refused +{} (a ring attached, the stream in another format), ATTACHes refused for a format +{}, \
         attaches +{}, capture timer passes +{}; now {now}",
        ra.refused_capture_opens.saturating_sub(rb.refused_capture_opens),
        ra.format_rejects.saturating_sub(rb.format_rejects),
        ra.attaches.saturating_sub(rb.attaches),
        a.stream[KIND_CAPTURE].timer_passes.saturating_sub(b.stream[KIND_CAPTURE].timer_passes)
    );
}

/// What a 24-in-32 PCM capture held: frames all zero, frames with sound, and samples whose padding byte (the low byte, zero
/// in a well-formed 24-in-32 sample) isn't zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PcmCounts {
    silent_frames: u64,
    sound_frames: u64,
    nonzero_padding: u64,
}

impl PcmCounts {
    /// Counts a packet's frames, eight bytes each: two little-endian 32-bit samples.
    fn count(&mut self, bytes: &[u8]) {
        for f in bytes.as_chunks::<8>().0 {
            if f.iter().all(|&b| b == 0) {
                self.silent_frames += 1;
            } else {
                self.sound_frames += 1;
            }
            self.nonzero_padding += u64::from(f[0] != 0) + u64::from(f[4] != 0);
        }
    }
}

/// open's capture: reads for `seconds` (or until Ctrl+C), counting what the packets carry, with a status line a second. A call
/// that fails before the stream runs (SetEventHandle, GetService, Start) is a refusal.
fn run_capture(opened: Opened, format: Format, seconds: u64) -> Result<OpenRun, Refusal> {
    let mut capture = Capture::new(opened, None)?;
    capture.start_stream()?;
    let _pro_audio = ProAudio::join();
    let mut checker = SequenceChecker::new();
    let mut pcm = PcmCounts::default();
    let (t0, mut last) = (Instant::now(), Instant::now());
    let (mut error, mut events) = (None, 0);
    while t0.elapsed() < Duration::from_secs(seconds) && !STOP.load(Relaxed) {
        let r = capture.read_packets(|p| match format {
            Format::Float if p.silent => (0..p.frames).for_each(|_| checker.feed([0.0; 2])),
            Format::Float => p.float_frames().for_each(|f| checker.feed(f)),
            Format::Pcm24 if p.silent => pcm.silent_frames += u64::from(p.frames),
            Format::Pcm24 => pcm.count(p.bytes),
        });
        if let Err(e) = r {
            error = Some(e);
            break;
        }
        events += 1;
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            let carried = match format {
                Format::Float => {
                    let r = checker.report();
                    let silence = r.zeros_before_signal + r.zeros_inside + r.zeros_after_signal;
                    format!("{} carry a code, {silence} silent, {} neither", r.signal_frames, r.corrupt_frames)
                }
                Format::Pcm24 => format!("{} with sound, {} all zero", pcm.sound_frames, pcm.silent_frames),
            };
            println!(
                "{:>7.1} s  {} packets, {} frames ({carried}), {} silent packets",
                t0.elapsed().as_secs_f64(),
                capture.packets,
                capture.frames,
                capture.silent_packets
            );
        }
    }
    let elapsed = t0.elapsed();
    capture.stop();
    capture.print_counters();
    match format {
        Format::Float => {
            let r = checker.report();
            println!("decoded marker range across all packets: {:?}", r.code_range);
            println!(
                "the test signal: {} of {} frames carry a code (codes {:?} to {:?}, {} steps of one); silence {} before, {} inside \
                 ({} runs), {} after; gaps {} ({} frames), repeats {}, frames neither silence nor a code {}",
                r.signal_frames,
                r.frames,
                r.first_code,
                r.last_code,
                r.steps_ok,
                r.zeros_before_signal,
                r.zeros_inside,
                r.zero_runs_inside,
                r.zeros_after_signal,
                r.gaps,
                r.gap_frames,
                r.repeats,
                r.corrupt_frames
            );
            // The first few: their samples, and (before the first code) the code there and each channel's ratio to it, so a
            // gain (a render engine's fade-in) shows as a ratio both channels share.
            for &(index, left, right) in &r.first_corrupt {
                let got = [f32::from_bits(left), f32::from_bits(right)];
                let due = r
                    .first_code
                    .zip(r.first_code_index)
                    .and_then(|(k, at)| at.checked_sub(index).map(|back| u64::from(k).wrapping_sub(back)));
                match due {
                    Some(n) => {
                        let want = signal::frame(n);
                        println!(
                            "  frame {index}, neither: {:e} {:e}; code {} there: {:e} {:e} (ratios {:.6} {:.6})",
                            got[0],
                            got[1],
                            signal::code(n),
                            want[0],
                            want[1],
                            got[0] / want[0],
                            got[1] / want[1]
                        );
                    }
                    None => println!("  frame {index}, neither: {:e} {:e}", got[0], got[1]),
                }
            }
        }
        Format::Pcm24 => println!(
            "24-in-32 PCM: {} frames with sound, {} all zero; {} samples with a non-zero padding byte",
            pcm.sound_frames, pcm.silent_frames, pcm.nonzero_padding
        ),
    }
    Ok(OpenRun { elapsed, events, error })
}

/// open's render: writes silence, or the test signal from frame `signal`, for `seconds` (or until Ctrl+C), the first buffer
/// before the start. A call that fails before the stream runs (SetEventHandle, GetService, the first fill, Start) is a refusal.
fn run_render(opened: Opened, seconds: u64, signal: Option<u64>) -> Result<OpenRun, Refusal> {
    let mut render = Render::new(opened, signal)?;
    // An event-driven stream wants its first buffer before it starts.
    render.fill()?;
    render.opened.client.start_stream().map_err(|e| Refusal::at("Start", e))?;
    let _pro_audio = ProAudio::join();
    let (t0, mut last) = (Instant::now(), Instant::now());
    let (mut error, mut events) = (None, 0);
    while t0.elapsed() < Duration::from_secs(seconds) && !STOP.load(Relaxed) {
        if let Err(e) = render.wait_and_fill() {
            error = Some(e);
            break;
        }
        events += 1;
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            println!("{:>7.1} s  {} buffers, {} frames {}", t0.elapsed().as_secs_f64(), render.packets, render.frames, render.what());
        }
    }
    let elapsed = t0.elapsed();
    let _ = render.opened.client.stop_stream();
    println!(
        "render: {} buffers, {} frames {} written{}, longest wait {:.1} ms",
        render.packets,
        render.frames,
        render.what(),
        // The range only where frames were written: none (a stream that stopped at once) has no last frame.
        signal.filter(|_| render.frames > 0).map(|first| format!(" (frames {first} to {})", first + render.frames - 1)).unwrap_or_default(),
        render.longest_wait.as_secs_f64() * 1000.0
    );
    Ok(OpenRun { elapsed, events, error })
}

// ---- latency ----

/// latency: the warm-up once the capture stream runs, before frames are measured; the longest measurement; how long the
/// capture thread may take to open its stream.
const LATENCY_WARMUP: Duration = Duration::from_secs(1);
const LATENCY_MAX_SECONDS: u64 = 600;
const CAPTURE_OPEN_WAIT: Duration = Duration::from_secs(10);
/// The capture thread's states ([`LatencyShared::state`]).
const OPENING: u32 = 0;
const RUNNING: u32 = 1;
const FAILED: u32 = 2;

/// What latency's feed (the main thread, which owns the attachment) and its capture thread share.
struct LatencyShared {
    /// Set by the feed once the measurement ends: the capture thread stops.
    stop: AtomicBool,
    /// OPENING, RUNNING (since `started_qpc`) or FAILED (the thread's result says why).
    state: AtomicU32,
    started_qpc: AtomicU64,
}

/// What latency's capture thread recorded.
struct CaptureLog {
    receipts: Log<Receipt>,
    runs: Log<Run>,
    counts: FrameCounts,
    describe: String,
    packets: u64,
    silent_packets: u64,
    lost_frames: u64,
    discontinuities: u64,
    longest_wait: Duration,
    /// Where another stream held the ring's reader slot just before this one started: that stream's packet, in frames (0 where
    /// it holds the slot in a format the driver can't count in frames).
    held_before: Option<u32>,
    /// Why it stopped before the feed told it to, if it did.
    error: Option<String>,
}

/// Whether the ring's reader slot is held, from the header's `state` and `packet_frames`: the holder's packet in frames where
/// it is. The holder writes READING or FORMAT_MISMATCH at every pass, and the driver clears both (and PacketFrames) when it
/// stops.
fn slot_holder(state: u32, packet_frames: u32) -> Option<u32> {
    (state & (STATE_READING | STATE_FORMAT_MISMATCH) != 0 || packet_frames != 0).then_some(packet_frames)
}

/// latency's capture thread: opens the stream, sizes its logs, and records every packet until the feed says stop. Nothing
/// allocates per packet once the stream runs.
fn latency_capture(name: &str, spec: StreamSpec, seconds: u64, header: &RingHeader, shared: &LatencyShared) -> Result<CaptureLog, String> {
    let failed = |e: String| {
        shared.state.store(FAILED, Release);
        e
    };
    let _com = Com::mta().map_err(failed)?;
    let mut capture = Capture::open(name, spec).map_err(failed)?;
    // Sized before the start: every packet of the run at half the granted period (32 frames at the least), four runs each.
    let packets = latency::packets_for(seconds + LATENCY_WARMUP.as_secs() + 1, (capture.opened.period_frames() / 2).clamp(32, 480));
    let mut receipts = Log::with_capacity(packets);
    let mut runs = Log::with_capacity(4 * packets);
    let mut counts = FrameCounts::default();
    // This stream can't hold the slot before it runs: a holder now is another stream, which this one would follow as a
    // secondary (unless they share one driver stream, the same mode in shared mode, which the window's checks tell apart).
    let packet = header.packet_frames.load(Acquire);
    let held_before = slot_holder(header.state.load(Acquire), packet);
    capture.start().map_err(failed)?;
    shared.started_qpc.store(qpc_now(), Relaxed);
    shared.state.store(RUNNING, Release);
    let _pro_audio = ProAudio::join();
    let mut error = None;
    while !shared.stop.load(Relaxed) {
        let r = capture.read_packets(|p| {
            // Every frame of the packet was written before this load (the driver copied each only after it read a write_frame
            // past it), so `written` bounds their frame numbers.
            let written = header.write_frame.load(Acquire);
            let first_run = runs.len() as u32;
            let mut kept = 0u32;
            if p.silent {
                counts.silent += u64::from(p.frames);
            } else {
                counts.add(&latency::split_runs(p.float_frames(), written, |run| kept += u32::from(runs.push(run))));
            }
            receipts.push(Receipt { host_qpc: p.host_qpc, device_hns: p.qpc_hns, frames: p.frames, first_run, runs: kept });
        });
        if let Err(e) = r {
            shared.state.store(FAILED, Release);
            error = Some(e);
            break;
        }
    }
    capture.stop();
    Ok(CaptureLog {
        receipts,
        runs,
        counts,
        describe: capture.describe(),
        packets: capture.packets,
        silent_packets: capture.silent_packets,
        lost_frames: capture.lost_frames,
        discontinuities: capture.discontinuities,
        longest_wait: capture.longest_wait,
        held_before,
        error,
    })
}

/// Why latency's feed loop ended.
enum LatencyEnd {
    /// The window was measured, or Ctrl+C cut it short.
    Done,
    Driver(io::Error),
    TargetBelowPacket(u32),
    CaptureFailed,
}

/// The measured window: from the first QPC after the warm-up to the feed's stop, with STATS and this ring's underrun count at
/// each end.
struct Window {
    from_qpc: u64,
    to_qpc: u64,
    start: Stats,
    end: Stats,
    underrun_start: u64,
    underrun_end: u64,
}

/// latency's feed, on the thread that owns the attachment: tops the ring up to `target` as feed does, logging each block with
/// the QPC just after its publication, until the window (`seconds` after the warm-up) is over. Nothing allocates per block.
fn latency_feed(
    attachment: &Attachment, device: &Device, target: u64, seconds: u64, frequency: u64, shared: &LatencyShared, blocks: &mut Log<Block>,
) -> Result<(LatencyEnd, Option<Window>), String> {
    let _pro_audio = ProAudio::join();
    let tick = Tick::new();
    let ring = attachment.ring();
    let header = ring.header();
    let capacity = u64::from(ring.capacity());
    let mut writer = Writer::new(ring);
    let (warmup, length) = (LATENCY_WARMUP.as_secs() * frequency, seconds * frequency);
    let t0 = Instant::now();
    let mut started: Option<(u64, Stats, u64)> = None;
    let end = loop {
        if let Some(status) = attachment.ended() {
            break LatencyEnd::Driver(status);
        }
        // As in feed: PacketFrames first, then State.
        let packet = header.packet_frames.load(Acquire);
        if let Some(packet) = packet_above_target(target, header.state.load(Acquire), packet) {
            break LatencyEnd::TargetBelowPacket(packet);
        }
        let read = writer.read_frame();
        let start = writer.write_frame();
        let fill = start.saturating_sub(read);
        let n = top_up(target, fill, capacity.saturating_sub(fill));
        if n > 0 {
            writer.produce(n, signal::frame);
            // Just after produce's store of write_frame, which published the block.
            blocks.push(Block { qpc: qpc_now(), start, end: writer.write_frame(), read_frame: read });
        }
        if STOP.load(Relaxed) {
            break LatencyEnd::Done;
        }
        match shared.state.load(Acquire) {
            FAILED => break LatencyEnd::CaptureFailed,
            OPENING if t0.elapsed() >= CAPTURE_OPEN_WAIT => break LatencyEnd::CaptureFailed,
            RUNNING => {
                let now = qpc_now();
                match &started {
                    // One STATS read at the window's start (and one after its end): not per block.
                    None if now >= shared.started_qpc.load(Relaxed) + warmup => {
                        started = Some((now, read_stats(device)?, header.underrun_frames.load(Relaxed)));
                    }
                    Some((from, ..)) if now >= from + length => break LatencyEnd::Done,
                    _ => {}
                }
            }
            _ => {}
        }
        tick.wait();
    };
    let window = match started {
        Some((from_qpc, start, underrun_start)) => {
            let (to_qpc, underrun_end) = (qpc_now(), header.underrun_frames.load(Relaxed));
            Some(Window { from_qpc, to_qpc, start, end: read_stats(device)?, underrun_start, underrun_end })
        }
        None => None,
    };
    Ok((end, window))
}

/// One stamps packet: GetBuffer's times and device position beside the driver's capture-timestamp trace, read from STATS
/// just after. `segment` counts the Stop/Start cycles before it (0: the first run).
#[derive(Clone, Copy)]
struct StampRow {
    host_qpc: u64,
    device_hns: Option<u64>,
    position: u64,
    frames: u32,
    segment: u32,
    /// One of a resume's leading packets, before the first the driver stamped after that Start, whose stamp isn't on the
    /// driver's packet grid ([`classify_stamps`]), so not checked. Shared (measured 2026-11-23): the engine hands a restarted
    /// client a packet before the device's next one, stamped about when it gives it. Exclusive (Astra's trace, 2026-11-23):
    /// a packet the driver completed after the client's last read but before Stop returned is handed out just after Start,
    /// stamped then, not with the driver's time; the next packet, the driver's, can be stamped earlier than it (TODO 22's
    /// question).
    windows_stamped: bool,
    /// A packet the driver completed before the Stop, handed out after Start with the driver's stamp (a shared engine's
    /// buffered packets; measured 2026-11-23): on the time line before the pause, so checked with the run before.
    late: bool,
    /// A stamp off the driver's packet grid where no resume explains it (in the first run, or in a later one after the
    /// first packet the driver stamped since its Start; Astra's review, 2026-11-24): how far it is from the nearest packet
    /// start the driver had notified, at or before its latest notification (100 ns units, later positive; past the latest,
    /// from it). Fails stamps, and stays in the steps checked (nothing shows it isn't the driver's). Only where the packet
    /// length is known: with no grid to place a stamp, it isn't evidence against the driver.
    unexplained: Option<i64>,
    /// Windows flagged the packet AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY (a client that fell behind, or a stream's start).
    discontinuity: bool,
    trace: CaptureTrace,
}

/// How long stamps waits after its last read before each Stop, in turn, in percent of a packet: Astra's 3-8 ms at 480 frames
/// (the review and its trace, 2026-11-23), then past one notification for sure, so a packet the client hasn't read is left
/// at Stop (under two, so an exclusive stream's two-packet buffer doesn't overrun).
const RESUME_WAITS_PERCENT: [u64; 8] = [30, 40, 50, 60, 70, 80, 130, 170];
/// How long each resume holds the stream stopped (ms), in turn.
const RESUME_PAUSES_MS: [u64; 8] = [3, 4, 5, 6, 7, 8, 20, 50];
/// One of stamps' Stop/Starts: the wait after the last read, the pause, whether a packet the driver completed was left
/// unread when Stop returned (the trace's latest notification isn't the client's last packet's), that notification's time
/// (100 ns units), and the QPC just before Start.
struct Resume {
    wait_hns: u64,
    pause_ms: u64,
    pending: bool,
    stop_notify_hns: i64,
    start_qpc: u64,
}
/// Packets recorded after each resume.
const RESUME_PACKETS: usize = 4;
/// How far a stamp's step may stray from its packet's length: the QPC's tick and the rounding of its conversion.
const STAMP_SLACK_HNS: i64 = 100;

/// What [`check_steps`] found.
#[derive(Debug, Default, PartialEq, Eq)]
struct StepCheck {
    /// Steps between the driver's stamps checked: in a run, packets whose positions follow on; across a Stop/Start, the
    /// last packet before it and the first after it (positions moving on by at least the first one's frames).
    checked: usize,
    /// Of them, steps across a Stop/Start (each resume's at most once: the verdict wants one per resume).
    across: usize,
    /// Inside one run: stamps that didn't step by the first packet's length (index of the second row).
    wrong: Vec<usize>,
    /// Across a Stop/Start: stamps that stepped by less than the frames between (a resumed stamp overlapping).
    overlapping: Vec<usize>,
    /// Packets with no device time (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR).
    no_time: usize,
    /// Steps not checked: in a run, the positions didn't follow on (frames lost); across a resume, the position went back.
    gaps: usize,
    /// A resume's leading packets whose stamp the driver didn't give ([`StampRow::windows_stamped`]): left out.
    windows_stamped: usize,
    /// Stamps off the driver's grid that no resume explains ([`StampRow::unexplained`]): the row's index and its offset
    /// from the grid (100 ns units). Each fails; it is checked in the steps too.
    unexplained: Vec<(usize, i64)>,
    /// Steps into a packet Windows flagged as a discontinuity inside a run: not checked, and listed apart (index of the
    /// second row). Across a resume the step is checked, flagged or not (a resume is a discontinuity).
    flagged: Vec<usize>,
    /// Stamps earlier than the packet before's, any packet's (index of the second row): the driver's own fail as `wrong` or
    /// `overlapping`; one after a packet Windows stamped is what a client saw, listed (TODO 22).
    backward: Vec<usize>,
}

/// Each of the driver's stamps must step by exactly the frames before it while the stream runs (the device position says
/// which packets follow on), and across a Stop/Start by at least the frames between the last packet before it and the
/// first after (by the pause too, as the time line paused with it; the frames between include any packet left out). A
/// packet from before the Stop handed out after Start ([`StampRow::late`]) is on the time line of the run before. Only a
/// resume's leading packets Windows stamped are left out; an off-grid stamp anywhere else is listed, to fail.
fn check_steps(rows: &[StampRow]) -> StepCheck {
    let line = |r: &StampRow| r.segment - u32::from(r.late && r.segment > 0);
    let mut c = StepCheck {
        no_time: rows.iter().filter(|r| r.device_hns.is_none()).count(),
        windows_stamped: rows.iter().filter(|r| r.windows_stamped).count(),
        unexplained: rows.iter().enumerate().filter_map(|(i, r)| Some((i, r.unexplained?))).collect(),
        ..Default::default()
    };
    let hns = |frames: u64| (frames * 10_000_000 / u64::from(protocol::SAMPLE_RATE)) as i64;
    for (i, w) in rows.windows(2).enumerate() {
        if let (Some(ta), Some(tb)) = (w[0].device_hns, w[1].device_hns)
            && tb < ta
        {
            c.backward.push(i + 1);
        }
    }
    let kept: Vec<(usize, &StampRow)> = rows.iter().enumerate().filter(|(_, r)| !r.windows_stamped).collect();
    for w in kept.windows(2) {
        let ((_, a), (i, b)) = (w[0], w[1]);
        let (Some(ta), Some(tb)) = (a.device_hns, b.device_hns) else { continue };
        let step = tb as i64 - ta as i64;
        if line(a) != line(b) {
            let Some(between) = b.position.checked_sub(a.position).filter(|&d| d >= u64::from(a.frames)) else {
                c.gaps += 1;
                continue;
            };
            c.checked += 1;
            c.across += 1;
            if step < hns(between) - STAMP_SLACK_HNS {
                c.overlapping.push(i);
            }
            continue;
        }
        if b.position != a.position + u64::from(a.frames) {
            c.gaps += 1;
            continue;
        }
        c.checked += 1;
        if b.discontinuity {
            c.flagged.push(i);
        } else if (step - hns(u64::from(a.frames))).abs() > STAMP_SLACK_HNS {
            c.wrong.push(i);
        }
    }
    c
}

/// stamps: what capture time the client sees for each packet, beside what the driver told ACX (TODO 19, the capture
/// timestamps reading a packet late), and whether the stamps step as the audio does, across Stop/Start too (Astra's review).
/// No ring needed: the stream's silence carries the same stamps.
fn stamps(args: &[String]) -> Result<Verdict, String> {
    let valued = ["--packets", "--period-frames", "--mode", "--device", "--resumes"];
    let o = Opts::parse(args, &valued, &["--exclusive", "--verbose"])?;
    let packets: usize = o.value("--packets", 200)?;
    if !(1..=100_000).contains(&packets) {
        return Err(format!("--packets {packets}: from 1 to 100000"));
    }
    let resumes: usize = o.value("--resumes", 8)?;
    if resumes > 1_000 {
        return Err(format!("--resumes {resumes}: from 0 to 1000"));
    }
    let name: String = o.value("--device", ENDPOINT.to_string())?;
    let spec = StreamSpec {
        exclusive: o.flag("--exclusive"),
        period_frames: o.period_frames()?,
        mode: o.mode(Mode::RawWhereOffered)?,
        ..StreamSpec::SHARED_FLOAT
    };
    let device = open_device()?;
    let frequency = read_stats(&device)?.qpc_frequency;
    let _com = Com::mta()?;
    let mut capture = Capture::open(&name, spec)?;
    println!("recording {name:?}: {}", capture.describe());
    let mut rows = Vec::with_capacity(packets + resumes * RESUME_PACKETS);
    let mut done: Vec<Resume> = Vec::with_capacity(resumes);
    // The last packet read, recorded or not (a drain reads on past the rows wanted): its receipt, device time and frames.
    let mut last_read: Option<(u64, Option<u64>, u32)> = None;
    let mut error = None;
    let warm_until = Instant::now() + Duration::from_millis(500);
    for segment in 0..=resumes {
        if segment > 0 {
            // The same client, stopped and started again without a Reset: its device position carries on. The wait runs from
            // the last read's receipt; then whether the driver notified a packet after the client's last one.
            let k = (segment - 1) % RESUME_WAITS_PERCENT.len();
            let (receipt, last_hns, frames) = last_read.ok_or("stamps: no packet read before a resume")?;
            let wait_hns = u64::from(frames) * 10_000_000 / u64::from(protocol::SAMPLE_RATE) * RESUME_WAITS_PERCENT[k] / 100;
            let until = receipt + wait_hns * frequency / 10_000_000;
            while qpc_now() < until {
                std::hint::spin_loop();
            }
            capture.stop();
            let latest = latency::qpc_to_hns(read_stats(&device)?.capture_trace.last_notify_qpc, frequency);
            let pending = last_hns.is_some_and(|d| (latest - d as i64).abs() > STAMP_SLACK_HNS);
            std::thread::sleep(Duration::from_millis(RESUME_PAUSES_MS[k]));
            done.push(Resume { wait_hns, pause_ms: RESUME_PAUSES_MS[k], pending, stop_notify_hns: latest, start_qpc: qpc_now() });
        }
        capture.start()?;
        let want = rows.len() + if segment == 0 { packets } else { RESUME_PACKETS };
        while rows.len() < want && error.is_none() {
            capture.read_packets(|p| {
                last_read = Some((p.host_qpc, p.qpc_hns, p.frames));
                if (segment == 0 && Instant::now() < warm_until) || rows.len() >= want || error.is_some() {
                    return;
                }
                match device.stats() {
                    Ok(s) => rows.push(StampRow {
                        host_qpc: p.host_qpc,
                        device_hns: p.qpc_hns,
                        position: p.position,
                        frames: p.frames,
                        segment: segment as u32,
                        windows_stamped: false,
                        late: false,
                        unexplained: None,
                        discontinuity: p.discontinuity,
                        trace: s.capture_trace,
                    }),
                    Err(e) => error = Some(format!("STATS: {e}")),
                }
            })?;
        }
        if error.is_some() {
            break;
        }
    }
    capture.stop();
    if let Some(e) = error {
        return Err(e);
    }
    let grid = classify_stamps(&mut rows, &done, frequency);
    print_stamps(&rows, frequency);
    match grid {
        Some(p) => println!("the driver's packet grid, from the first run's notifications: {:.4} ms", p as f64 / 10_000.0),
        None => println!(
            "the driver's packet grid couldn't be found from the first run's notifications (too few rows, or all read after one \
             notification): a stamp is the driver's only at its latest notification or the one at Stop, and no other is judged"
        ),
    }
    if o.flag("--verbose") {
        // Each resume's rows and the one before: run, device position, frames, device time and receipt (ms from the first).
        let hns = |qpc: u64| latency::qpc_to_hns(qpc, frequency) as f64;
        let base = rows.first().and_then(|r| r.device_hns).unwrap_or(0) as f64;
        for (i, r) in rows.iter().enumerate() {
            if r.segment > 0 || rows.get(i + 1).is_some_and(|n| n.segment > 0) {
                let dev = r.device_hns.map_or("none".to_string(), |d| format!("{:.4}", (d as f64 - base) / 10_000.0));
                let rec = (hns(r.host_qpc) - base) / 10_000.0;
                let drv = (hns(r.trace.last_notify_qpc) - base) / 10_000.0;
                println!(
                    "    row {i:>4} run {:>2} position {:>9} frames {:>4} device {dev:>12} receipt {rec:>12.4} driver's latest {drv:>12.4}{}",
                    r.segment,
                    r.position,
                    r.frames,
                    if r.discontinuity { " DISCONTINUITY" } else { "" }
                );
            }
        }
    }
    print_resumes(&rows, &done, frequency);
    let c = check_steps(&rows);
    let (pending, backs) = (done.iter().filter(|r| r.pending).count(), c.backward.iter().filter(|&&i| rows[i - 1].windows_stamped).count());
    println!(
        "steps: {} of the driver's checked ({} across a resume, of {} resumes), {} in a run that weren't a packet's length, \
         {} across a resume short of the frames between, {} packets without a device time, {} not checked (positions not \
         following on), {} of a resume's leading packets stamped by Windows, not the driver (left out), {} off the driver's \
         packet grid where no resume explains it, {} stamps earlier than the packet before",
        c.checked,
        c.across,
        done.len(),
        c.wrong.len(),
        c.overlapping.len(),
        c.no_time,
        c.gaps,
        c.windows_stamped,
        c.unexplained.len(),
        c.backward.len()
    );
    // A step's two packets: the row before (`kept`: the last the driver stamped, as check_steps compares) and this one.
    let at = |i: usize, kept: bool| {
        let a = rows[..i].iter().rev().find(|r| !(kept && r.windows_stamped)).unwrap_or(&rows[i - 1]);
        let b = &rows[i];
        let step = b.device_hns.unwrap_or(0) as f64 - a.device_hns.unwrap_or(0) as f64;
        let frames = b.position.saturating_sub(a.position);
        format!("    packet {i} (run {}): stepped {:.4} ms after {frames} frames", b.segment, step / 10_000.0)
    };
    for &i in c.wrong.iter().chain(&c.overlapping).take(12) {
        println!("{}", at(i, true));
    }
    for &i in c.flagged.iter().take(12) {
        println!("{} (flagged a discontinuity: not checked)", at(i, true));
    }
    for &(i, off) in c.unexplained.iter().take(12) {
        let r = &rows[i];
        let place = if r.segment == 0 { "in the first run" } else { "after the driver's first packet since Start" };
        println!(
            "    packet {i} (run {}): stamped {:+.4} ms off the driver's packet grid, {place}: unexplained",
            r.segment,
            off as f64 / 10_000.0
        );
    }
    for &i in c.backward.iter().filter(|&&i| rows[i - 1].windows_stamped).take(12) {
        println!("{} (the packet before stamped by Windows: a step back the client saw)", at(i, false));
    }
    if c.windows_stamped > 0 {
        println!(
            "note: {} of a resume's leading packets carried a stamp Windows gave, not the driver; {backs} of them later than \
             the packet after, so the client saw its stamps step back (not the driver's; TODO 22's question)",
            c.windows_stamped
        );
    }
    let (verdict, line) = stamps_verdict(&c, rows.len(), done.len(), pending, grid.is_some());
    println!("{line}");
    Ok(verdict)
}

/// stamps' verdict and its line, from [`check_steps`]' counts over `packets` rows and `resumes` resumes (`pending` of them
/// with a packet left unread at Stop), and whether [`classify_stamps`] found the driver's packet grid (with none, no stamp
/// off the latest notification could be judged: INCONCLUSIVE at best).
fn stamps_verdict(c: &StepCheck, packets: usize, resumes: usize, pending: usize, grid: bool) -> (Verdict, String) {
    let failures: Vec<String> = [
        (c.no_time, "packets without a device time"),
        (c.wrong.len(), "steps in a run that weren't a packet's length"),
        (c.overlapping.len(), "steps across a resume short of the frames between"),
        (c.unexplained.len(), "stamps off the driver's packet grid that no resume explains"),
    ]
    .into_iter()
    .filter(|&(n, _)| n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    if !failures.is_empty() {
        (Verdict::Fail, format!("FAIL: the driver's stamps don't follow the audio: {}", failures.join(", ")))
    } else if !grid {
        let line = "INCONCLUSIVE: the driver's packet grid couldn't be found from the first run, so a stamp off its latest \
                    notification couldn't be told the driver's or Windows' (record more packets: --packets)";
        (Verdict::Inconclusive, line.to_string())
    } else if c.across < resumes {
        let line = format!(
            "INCONCLUSIVE: {} of {resumes} resumes' steps compared (the rest: positions going back, or no packet the driver stamped)",
            c.across
        );
        (Verdict::Inconclusive, line)
    } else if c.checked + 1 < packets / 2 {
        (
            Verdict::Inconclusive,
            format!("INCONCLUSIVE: too few packets followed on to check ({} of {})", c.checked, packets.saturating_sub(1)),
        )
    } else {
        let line = format!(
            "PASS: every stamp the driver gave stepped by its packet's length, and across each of the {resumes} resumes ({pending} \
             with a packet left unread at Stop) by at least the frames between; every stamp off its packet grid was one of a \
             resume's leading packets"
        );
        (Verdict::Pass, line)
    }
}

/// Whose stamp each packet carries, by the driver's packet grid (its packet length from the first run's notifications: the
/// trace's packet numbers and times, read just after each GetBuffer). A stamp the driver gave starts one of its packets: on
/// the grid of the row's latest notification, at or before it; or, after a resume, on the grid from before the Stop, at or
/// before the notification then (a packet completed before the Stop and handed out after Start: `late`). Any other stamp
/// among a resume's leading packets, before the first the driver stamped after that Start, Windows gave: the shared engine's
/// own packet, or one left unread at Stop and stamped again at Start (measured 2026-11-23; the latest notification alone
/// missed a shared engine's buffered packets, a packet or two behind it). Anywhere else, in the first run or after the
/// driver's first packet since a Start, nothing explains it (`unexplained`): Astra's review (2026-11-24) had one stamp 1 ms
/// off in a steady run, with no resume, left out as Windows' and passed. The packet length found, in 100 ns units: with none
/// (one row in the first run, or its rows all read after one notification) a stamp is the driver's only at the latest
/// notification, or at the one at Stop, and a stamp elsewhere is no evidence against the driver: none is `unexplained`, the
/// rest of a resume's leading packets are still left out, and the verdict can't be a PASS ([`stamps_verdict`]).
fn classify_stamps(rows: &mut [StampRow], done: &[Resume], frequency: u64) -> Option<i64> {
    let hns = |qpc: u64| latency::qpc_to_hns(qpc, frequency);
    let mut lengths: Vec<i64> = rows
        .windows(2)
        .filter(|w| w[0].segment == 0 && w[1].segment == 0)
        .filter_map(|w| {
            let packets = w[1].trace.last_notify_packet.checked_sub(w[0].trace.last_notify_packet).filter(|&n| n > 0)?;
            Some((hns(w[1].trace.last_notify_qpc) - hns(w[0].trace.last_notify_qpc)) / packets as i64)
        })
        .collect();
    lengths.sort_unstable();
    let packet = lengths.get(lengths.len() / 2).copied().filter(|&p| p > 0);
    // At or before `reference` by a whole number of packets (with no packet length known, at it).
    let on_grid = |stamp: i64, reference: i64| {
        let back = reference - stamp;
        back >= -STAMP_SLACK_HNS
            && match packet {
                Some(p) => {
                    let r = back.rem_euclid(p);
                    r <= STAMP_SLACK_HNS || p - r <= STAMP_SLACK_HNS
                }
                None => back <= STAMP_SLACK_HNS,
            }
    };
    // How far a stamp is from the nearest start of a `p`-long packet at or before `reference`, later positive; past `reference`,
    // from it.
    let offset = |stamp: i64, reference: i64, p: i64| {
        if stamp < reference {
            let r = (reference - stamp) % p;
            if 2 * r > p { p - r } else { -r }
        } else {
            stamp - reference
        }
    };
    // The run each row is in, and whether the driver stamped a packet in it since its Start (`late` ones are from before).
    let mut run = (u32::MAX, false);
    for r in rows.iter_mut() {
        if run.0 != r.segment {
            run = (r.segment, false);
        }
        let Some(d) = r.device_hns.map(|d| d as i64) else { continue };
        let latest = hns(r.trace.last_notify_qpc);
        let stop = r.segment.checked_sub(1).and_then(|k| done.get(k as usize)).map(|x| x.stop_notify_hns);
        r.late = stop.is_some_and(|s| d <= s + STAMP_SLACK_HNS && on_grid(d, s));
        let current = stop.is_none_or(|s| d > s + STAMP_SLACK_HNS) && on_grid(d, latest);
        let off = !r.late && !current;
        r.windows_stamped = off && stop.is_some() && !run.1;
        r.unexplained = packet.filter(|_| off && !r.windows_stamped).map(|p| offset(d, latest, p));
        run.1 |= current;
    }
    packet
}

/// stamps' resumes, the first 16: the wait after the last read, the pause, whether a packet was left unread at Stop, then
/// the first two packets after Start as the client got them (receipt and stamp, ms from just before Start), whose stamp
/// each was, and the step between them: the client's packets apart from the driver's notifications (Astra, 2026-11-23).
fn print_resumes(rows: &[StampRow], done: &[Resume], frequency: u64) {
    if done.is_empty() {
        return;
    }
    let hns = |qpc: u64| latency::qpc_to_hns(qpc, frequency);
    println!("resumes (ms from just before Start; pending: a packet the driver completed, unread by the client when Stop returned)");
    for (k, r) in done.iter().enumerate().take(16) {
        let from = |t: i64| (t - hns(r.start_qpc)) as f64 / 10_000.0;
        let first: Vec<&StampRow> = rows.iter().filter(|x| x.segment == k as u32 + 1).take(2).collect();
        let packet = |x: &StampRow| {
            let stamp = x.device_hns.map_or("none".to_string(), |d| format!("{:+.3}", from(d as i64)));
            let whose = if x.windows_stamped {
                format!("Windows', the driver's latest {:+.3}", from(hns(x.trace.last_notify_qpc)))
            } else if x.late {
                "the driver's, from before Stop".to_string()
            } else if let Some(off) = x.unexplained {
                format!("unexplained, {:+.3} off the driver's grid", off as f64 / 10_000.0)
            } else {
                "the driver's".to_string()
            };
            format!("receipt {:+.3} stamp {stamp} ({whose})", from(hns(x.host_qpc)))
        };
        let step = match first.as_slice() {
            [a, b] => match (a.device_hns, b.device_hns) {
                (Some(ta), Some(tb)) => format!(", step {:+.3}", (tb as f64 - ta as f64) / 10_000.0),
                _ => String::new(),
            },
            _ => String::new(),
        };
        println!(
            "  resume {:>2}: waited {:.2}, paused {}, {}; first {}; second {}{step}",
            k + 1,
            r.wait_hns as f64 / 10_000.0,
            r.pause_ms,
            if r.pending { "pending" } else { "none pending" },
            first.first().map_or("none".to_string(), |x| packet(x)),
            first.get(1).map_or("none".to_string(), |x| packet(x)),
        );
    }
    if done.len() > 16 {
        println!("  and {} more", done.len() - 16);
    }
}

/// stamps' report: each difference's min, median and max, then the last rows as they came.
fn print_stamps(rows: &[StampRow], frequency: u64) {
    let hns = |qpc: u64| latency::qpc_to_hns(qpc, frequency);
    let ms = |h: i64| h as f64 / 10_000.0;
    let spread = |name: &str, mut v: Vec<i64>| {
        v.sort_unstable();
        match (v.first(), v.get(v.len() / 2), v.last()) {
            (Some(&lo), Some(&mid), Some(&hi)) => println!("  {name:<58} {:>8.3} {:>8.3} {:>8.3}", ms(lo), ms(mid), ms(hi)),
            _ => println!("  {name:<58} (no packets with a device time)"),
        }
    };
    let stamped: Vec<(&StampRow, i64)> = rows.iter().filter_map(|r| r.device_hns.map(|d| (r, d as i64))).collect();
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else { return };
    let span = rows.len().saturating_sub(1).max(1) as f64;
    let per = |f: fn(&CaptureTrace) -> u64| (f(&last.trace) - f(&first.trace)) as f64 / span;
    println!(
        "{} packets ({} with a device time), {} frames each at first; per packet the driver answered {:.2} GetCapturePacket, {:.2} \
         GetPresentationPosition, {:.2} GetCurrentPacket",
        rows.len(),
        stamped.len(),
        first.frames,
        per(|t| t.packet_queries),
        per(|t| t.position_queries),
        per(|t| t.current_packet_queries),
    );
    println!("  {:<58} {:>8} {:>8} {:>8}", "ms (the client's device time minus ...)", "min", "median", "max");
    spread(
        "the latest query's QPCPacketStart (the driver's packet start)",
        stamped.iter().map(|(r, d)| d - hns(r.trace.last_query_packet_qpc)).collect(),
    );
    spread("the latest notify's QPCCompletion", stamped.iter().map(|(r, d)| d - hns(r.trace.last_notify_qpc)).collect());
    spread("the latest query's own clock reading", stamped.iter().map(|(r, d)| d - hns(r.trace.last_query_qpc)).collect());
    spread("(the client's receipt minus its device time)", stamped.iter().map(|(r, d)| hns(r.host_qpc) - d).collect());
    let same = rows.iter().filter(|r| r.trace.last_query_packet == r.trace.last_notify_packet).count();
    println!("  the latest query named the latest notified packet at {same} of {} packets", rows.len());
    println!(
        "  the last rows: device ms, query packet / its start ms, notify packet / its QPC ms, receipt ms (all from the first device time)"
    );
    let base = stamped.first().map_or(0, |&(_, d)| d);
    for r in rows.iter().rev().take(8).rev() {
        let at = |qpc: u64| ms(hns(qpc) - base);
        println!(
            "    {:>10} {:>7} / {:>10.3} {:>7} / {:>10.3} {:>10.3}",
            r.device_hns.map_or("error".to_string(), |d| format!("{:.3}", ms(d as i64 - base))),
            r.trace.last_query_packet,
            at(r.trace.last_query_packet_qpc),
            r.trace.last_notify_packet,
            at(r.trace.last_notify_qpc),
            at(r.host_qpc),
        );
    }
}

fn latency(args: &[String]) -> Result<Verdict, String> {
    let valued = ["--seconds", "--target", "--capacity", "--period-frames", "--mode", "--device"];
    let o = Opts::parse(args, &valued, &["--exclusive"])?;
    let seconds: u64 = o.value("--seconds", 10)?;
    let target: u64 = o.value("--target", 960)?;
    let capacity: u32 = o.value("--capacity", 8192)?;
    let name: String = o.value("--device", ENDPOINT.to_string())?;
    let spec = StreamSpec {
        exclusive: o.flag("--exclusive"),
        period_frames: o.period_frames()?,
        mode: o.mode(Mode::RawWhereOffered)?,
        ..StreamSpec::SHARED_FLOAT
    };
    if !(1..=LATENCY_MAX_SECONDS).contains(&seconds) {
        return Err(format!("--seconds {seconds}: from 1 to {LATENCY_MAX_SECONDS}"));
    }
    if !protocol::capacity_ok(capacity) {
        let (min, max) = (protocol::MIN_CAPACITY_FRAMES, protocol::MAX_CAPACITY_FRAMES);
        return Err(format!("--capacity {capacity}: a power of two from {min} to {max}"));
    }
    if target == 0 || target > u64::from(capacity) {
        return Err(format!("--target {target}: from 1 to the capacity ({capacity}), and at least one capture packet"));
    }
    let device = open_device()?;
    let first = read_stats(&device)?;
    if first.flags & STATS_ATTACHED != 0 {
        return Err(format!("process {}'s ring is attached: stop that feed first (latency attaches its own)", first.attached_pid));
    }
    let frequency = qpc_frequency();
    // The feed's log, sized before the start: at most one block a tick (1 ms or more), twice over.
    let mut blocks = Log::with_capacity(((seconds + LATENCY_WARMUP.as_secs() + 2) * 2_000) as usize);
    let ring = RingMemory::new(capacity).map_err(|e| e.to_string())?;
    // Pre-filled to the target as feed does: block 0, published before ATTACH, so not measured.
    Writer::new(&ring).produce(target, signal::frame);
    blocks.push(Block { qpc: qpc_now(), start: 0, end: target, read_frame: 0 });
    // SAFETY: the handler only stores to an atomic.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) }.is_err() {
        eprintln!("note: no Ctrl+C handler: Ctrl+C ends the run without its summary (the driver still detaches the ring)");
    }
    let attachment = device.attach(ring).map_err(|e| format!("ATTACH refused: {}{}", e.error, attach_hint(&e)))?;
    println!(
        "attached a {capacity}-frame ring, keeping {target} frames ({:.2} ms) in it; recording {name:?} for {seconds} s after a {} s \
         warm-up (nothing is printed meanwhile)",
        ms(target),
        LATENCY_WARMUP.as_secs()
    );
    let shared = LatencyShared { stop: AtomicBool::new(false), state: AtomicU32::new(OPENING), started_qpc: AtomicU64::new(0) };
    let header = attachment.ring().header();
    let (fed, captured) = thread::scope(|s| {
        let capture = s.spawn(|| latency_capture(&name, spec, seconds, header, &shared));
        let fed = latency_feed(&attachment, &device, target, seconds, frequency, &shared, &mut blocks);
        shared.stop.store(true, Relaxed);
        (fed, capture.join().unwrap_or_else(|_| Err("the capture thread panicked".to_string())))
    });
    let (ring, status) = attachment.detach().map_err(|e| format!("detach: {e}"))?;
    println!("detached ({})", end_text(&status));
    print_ring_counters(ring.header());
    let (end, window) = fed?;
    let log = captured?;
    match &end {
        LatencyEnd::Done => {}
        LatencyEnd::Driver(status) => println!("the driver ended the attachment: {}", end_text(status)),
        LatencyEnd::TargetBelowPacket(packet) => println!(
            "--target {target} is below the reader's {packet}-frame capture packet, so every pass underruns: keep at least one \
             packet (--target {packet}) or two (--target {})",
            2 * u64::from(*packet)
        ),
        LatencyEnd::CaptureFailed => println!(
            "the capture stream failed: {}",
            log.error.as_deref().unwrap_or("it didn't start within the wait (or its thread ended)")
        ),
    }
    println!("the capture stream: {}", log.describe);
    let Some(w) = window else {
        println!("FAIL: the capture stream never ran past the {} s warm-up: nothing was measured", LATENCY_WARMUP.as_secs());
        return Ok(Verdict::Fail);
    };
    let report = latency_report(blocks.items(), log.receipts.items(), log.runs.items(), frequency, w.from_qpc, w.to_qpc);
    print_latency(&report, &log, &w, frequency, target);
    let mut c = Checks::default();
    println!("checks:");
    let dropped = [blocks.dropped(), log.receipts.dropped(), log.runs.dropped()];
    let stream = LatencyStream {
        lost_frames: log.lost_frames,
        discontinuities: log.discontinuities,
        held_before: log.held_before,
        not_reader_packets: w.end.ring.not_reader_packets.saturating_sub(w.start.ring.not_reader_packets),
    };
    for check in latency_checks(&report, &log.counts, dropped, &stream) {
        c.judge(check.outcome, check.name, check.detail);
    }
    if c.failed == 0 && matches!(end, LatencyEnd::Done) {
        println!("PASS: measured; the latencies above are the result");
        Ok(Verdict::Pass)
    } else {
        println!("FAIL: {} of the checks, or the run ended early", c.failed);
        Ok(Verdict::Fail)
    }
}

/// latency's results, from the logs.
#[derive(Debug)]
struct LatencyReport {
    /// The window's frames: measured, skipped (the pre-fill), never published.
    measured: latency::Latencies,
    host: Option<Summary>,
    device: Option<Summary>,
    /// The fill the feed found before each top-up in the window, in frames.
    fill: Option<Summary>,
    blocks: u64,
    continuity: Continuity,
}

/// The latencies of the frames received from `from_qpc` on (block 0, the pre-fill, not measured), the fill the feed found before
/// each of its blocks between `from_qpc` and `to_qpc`, and whether the window's runs carried every frame once and in order.
fn latency_report(blocks: &[Block], receipts: &[Receipt], runs: &[Run], frequency: u64, from_qpc: u64, to_qpc: u64) -> LatencyReport {
    let mut measured = latency::latencies(blocks, receipts, runs, frequency, from_qpc, 1);
    let host = latency::summarize(&mut measured.host);
    let device = latency::summarize(&mut measured.device);
    let mut fills: Vec<Weighted> = blocks
        .iter()
        .skip(1)
        .filter(|b| (from_qpc..=to_qpc).contains(&b.qpc))
        .map(|b| Weighted { value: b.fill_before() as i64, weight: 1 })
        .collect();
    let fill = latency::summarize(&mut fills);
    let first = receipts.partition_point(|r| r.host_qpc < from_qpc);
    let from_run = receipts.get(first).map_or(runs.len(), |r| (r.first_run as usize).min(runs.len()));
    let continuity = latency::continuity(&runs[from_run..]);
    LatencyReport { measured, host, device, fill, blocks: fills.len() as u64, continuity }
}

/// What latency's checks need to know of its capture stream, besides its frames.
#[derive(Debug, Default)]
struct LatencyStream {
    lost_frames: u64,
    discontinuities: u64,
    /// [`CaptureLog::held_before`].
    held_before: Option<u32>,
    /// Passes of capture streams other than the reader over the window (STATS' NotReaderPackets).
    not_reader_packets: u64,
}

/// Whether the ring's audio reached this process's stream as the reader's, not as a secondary's (which plays the reader's copy
/// the reader's packet plus 1 ms and a frame late). No pass of another stream over the window proves it: every pass was the
/// reader's, this stream's among them. With such passes, the slot free just before this stream started says its first pass
/// took it; held, this stream may be one of them.
fn reader_check(s: &LatencyStream) -> Check {
    let (ok, detail) = match (s.held_before, s.not_reader_packets) {
        (_, 0) => (true, "no pass of a stream other than the reader over the window: this one read the ring".to_string()),
        (None, n) => (
            true,
            format!(
                "the reader slot was free just before this stream started, so its first pass took it; another stream passed +{n} \
                 times as a secondary"
            ),
        ),
        (Some(packet), n) => (
            false,
            format!(
                "another capture stream ({packet}-frame packets) held the reader slot when this one started, and streams other than \
                 the reader passed +{n} times: this one may be one of them, a secondary that plays the reader's copy the reader's \
                 packet plus 1 ms and a frame late, so the latencies above may be a secondary's. Close other apps that record the \
                 endpoint (and any verify or open left running), then run it again"
            ),
        ),
    };
    Check { outcome: ok.into(), name: "this process's stream reads the ring", detail }
}

/// latency's checks: something measured, every frame the feed's and in order, the logs big enough, the stream unbroken, and
/// the stream the ring's reader. The latencies themselves are the result, not judged.
fn latency_checks(r: &LatencyReport, counts: &FrameCounts, dropped: [u64; 3], stream: &LatencyStream) -> Vec<Check> {
    let (m, c) = (&r.measured, &r.continuity);
    let (lost_frames, discontinuities) = (stream.lost_frames, stream.discontinuities);
    vec![
        Check {
            outcome: (m.frames > 0).into(),
            name: "frames measured",
            detail: format!("{} frames in {} packets after the warm-up ({} of the pre-fill not measured)", m.frames, m.packets, m.skipped),
        },
        Check {
            outcome: (c.gaps == 0 && c.repeats == 0).into(),
            name: "every frame once and in order",
            detail: format!("gaps {} ({} frames lost), repeats {} ({} frames)", c.gaps, c.gap_frames, c.repeats, c.repeat_frames),
        },
        Check {
            outcome: (counts.corrupt == 0 && counts.unmatched == 0 && m.unpublished == 0).into(),
            name: "every frame the feed's",
            detail: format!(
                "{} frames neither silence nor a code, {} codes not written yet, {} frames no block published",
                counts.corrupt, counts.unmatched, m.unpublished
            ),
        },
        Check {
            outcome: dropped.iter().all(|&d| d == 0).into(),
            name: "the logs held everything",
            detail: format!("dropped: {} blocks, {} packets, {} runs", dropped[0], dropped[1], dropped[2]),
        },
        Check {
            outcome: (lost_frames == 0 && discontinuities == 0).into(),
            name: "the capture stream",
            detail: format!("{lost_frames} frames lost by the engine, {discontinuities} discontinuities after its first packet"),
        },
        reader_check(stream),
    ]
}

/// A latency summary in ms (the samples in 100 ns units).
fn latency_text(s: &Summary) -> String {
    let ms = |hns: i64| hns as f64 / 10_000.0;
    format!(
        "p50 {:.3} ms, p90 {:.3} ms, p99 {:.3} ms, max {:.3} ms (min {:.3} ms, mean {:.3} ms) over {} frames",
        ms(s.p50),
        ms(s.p90),
        ms(s.p99),
        ms(s.max),
        ms(s.min),
        ms(s.mean),
        s.count
    )
}

fn print_latency(r: &LatencyReport, log: &CaptureLog, w: &Window, frequency: u64, target: u64) {
    let seconds = (w.to_qpc.saturating_sub(w.from_qpc)) as f64 / frequency as f64;
    let (s0, s1) = (&w.start, &w.end);
    let timer = |s: &Stats| s.stream[KIND_CAPTURE];
    let passes = timer(s1).timer_passes.saturating_sub(timer(s0).timer_passes);
    let lateness = timer(s1).lateness_sum_hns.saturating_sub(timer(s0).lateness_sum_hns);
    let mean_us = if passes == 0 { 0.0 } else { us(lateness) / passes as f64 };
    let work = timer(s1).work_qpc_ticks.saturating_sub(timer(s0).work_qpc_ticks);
    println!("measured {seconds:.2} s, QPC {:.3} MHz", frequency as f64 / 1e6);
    println!(
        "the driver: the reader's packet {} frames; capture timer +{passes} passes, lateness mean {mean_us:.1} us over the window \
         (max {:.1} us since the driver loaded), work {:.2} us a pass; passes of a stream that isn't the reader +{}",
        s1.reader_packet_frames,
        us(timer(s1).lateness_max_hns),
        work_us(work, passes, frequency),
        s1.ring.not_reader_packets.saturating_sub(s0.ring.not_reader_packets)
    );
    println!(
        "the ring over the window: underrun {} frames ({} packets with one), resyncs +{}; fill after the driver's latest pass {} \
         frames",
        w.underrun_end.saturating_sub(w.underrun_start),
        s1.ring.underrun_packets.saturating_sub(s0.ring.underrun_packets),
        s1.ring.resyncs.saturating_sub(s0.ring.resyncs),
        s1.attached_write_frame.wrapping_sub(s1.attached_read_frame) as i64
    );
    match &r.fill {
        Some(f) => println!(
            "the feed: {} top-ups to {target} frames; the fill it found before one: min {}, p50 {}, p90 {}, max {} frames (p50 {:.2} ms)",
            r.blocks,
            f.min,
            f.p50,
            f.p90,
            f.max,
            ms(f.p50.max(0) as u64)
        ),
        None => println!("the feed: no top-ups in the window"),
    }
    let counts = &log.counts;
    let timestamp_errors = log.receipts.items().iter().filter(|r| r.device_hns.is_none()).count();
    println!(
        "received (the whole run): {} packets ({} silent, {timestamp_errors} with a timestamp error), frames: {} carry a code, {} \
         silent, {} neither; longest wait {:.1} ms",
        log.packets,
        log.silent_packets,
        counts.signal,
        counts.silent,
        counts.corrupt,
        log.longest_wait.as_secs_f64() * 1000.0
    );
    let none = "nothing measured".to_string();
    println!("publication to host receipt (the QPC when GetBuffer returned): {}", r.host.as_ref().map_or(none.clone(), latency_text));
    let group = r.measured.device_group;
    let timed = if group <= 1 {
        "each frame timed alone".to_string()
    } else {
        format!("frames timed in groups of {group}, each within {:.2} ms of its group's centre", ms(u64::from(group - 1)) / 2.0)
    };
    println!(
        "publication to the frame's device time (GetBuffer's device QPC position, its packet's first frame's, plus its place in the \
         packet; {timed}): {}",
        r.device.as_ref().map_or(none, latency_text)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(position: u64, device_ms: f64, segment: u32) -> StampRow {
        StampRow {
            host_qpc: 0,
            device_hns: Some((device_ms * 10_000.0).round() as u64),
            position,
            frames: 480,
            segment,
            windows_stamped: false,
            late: false,
            unexplained: None,
            discontinuity: false,
            trace: CaptureTrace::default(),
        }
    }

    /// A stamps row with `frames` frames and the driver's latest notification read after it (QPC at 10 MHz: 100 ns ticks).
    fn traced(position: u64, frames: u32, device_ms: f64, segment: u32, notify: (u64, f64)) -> StampRow {
        let mut r = row(position, device_ms, segment);
        r.frames = frames;
        (r.trace.last_notify_packet, r.trace.last_notify_qpc) = (notify.0, (notify.1 * 10_000.0).round() as u64);
        r
    }

    fn resume(stop_notify_ms: f64) -> Resume {
        Resume { wait_hns: 0, pause_ms: 0, pending: true, stop_notify_hns: (stop_notify_ms * 10_000.0).round() as i64, start_qpc: 0 }
    }

    /// Whose stamp, by the driver's packet grid (measured 2026-11-23). Exclusive, Astra's trace: the packet left unread at
    /// Stop comes back stamped at Start, off the grid. Shared at 5 ms (the Call Microphone): the engine hands out the
    /// driver's packets from before the Stop after Start, a packet or two behind its latest, then the driver's new ones.
    #[test]
    fn stamps_tell_the_drivers_from_windows_by_its_packet_grid() {
        let mut rows = vec![
            traced(0, 480, 0.0, 0, (10, 0.0)),
            traced(480, 480, 10.0, 0, (11, 10.0)),
            traced(960, 480, 20.0, 0, (12, 20.0)),
            traced(1440, 480, 72.93, 1, (13, 30.0)),
            traced(1920, 480, 71.42, 1, (14, 71.42)),
            traced(2400, 480, 81.42, 1, (15, 81.42)),
        ];
        classify_stamps(&mut rows, &[resume(30.0)], 10_000_000);
        let whose: Vec<(bool, bool)> = rows.iter().map(|r| (r.windows_stamped, r.late)).collect();
        assert_eq!(whose, [(false, false), (false, false), (false, false), (true, false), (false, false), (false, false)]);
        let c = check_steps(&rows);
        assert_eq!((c.checked, c.across, c.wrong.len(), c.overlapping.len(), c.backward.clone()), (4, 1, 0, 0, vec![4]));

        // Shared at 5 ms: two packets notified after the last read before Stop (15 and 20 ms), handed out after Start with
        // their own stamps, the second read after the new run's first notification; then the new run's packets.
        let mut rows = vec![
            traced(0, 240, 0.0, 0, (100, 0.0)),
            traced(240, 240, 5.0, 0, (101, 5.0)),
            traced(480, 240, 10.0, 0, (102, 10.0)),
            traced(720, 240, 15.0, 1, (104, 20.0)),
            traced(960, 240, 20.0, 1, (105, 46.88)),
            traced(1200, 240, 46.88, 1, (105, 46.88)),
            traced(1440, 240, 51.88, 1, (106, 51.88)),
        ];
        classify_stamps(&mut rows, &[resume(20.0)], 10_000_000);
        let whose: Vec<(bool, bool)> = rows.iter().map(|r| (r.windows_stamped, r.late)).collect();
        assert_eq!(whose[3..], [(false, true), (false, true), (false, false), (false, false)]);
        let c = check_steps(&rows);
        assert_eq!((c.windows_stamped, c.checked, c.across, c.wrong.len(), c.overlapping.len()), (0, 6, 1, 0, 0));
    }

    /// A stamp off the driver's grid is Windows' only among a resume's leading packets, before the first the driver stamped
    /// after that Start; anywhere else it fails, listed with its offset (Astra's review, 2026-11-24).
    #[test]
    fn stamps_fail_a_stamp_off_the_grid_that_no_resume_explains() {
        let verdict = |rows: &[StampRow], resumes: usize| stamps_verdict(&check_steps(rows), rows.len(), resumes, 0, true).0;
        // Astra's case: a steady 10 ms stream, no resume, one stamp 1 ms late. It was left out as Windows' and passed.
        let steady = |shift: f64| -> Vec<StampRow> {
            (0..10u64)
                .map(|n| {
                    let at = 100.0 + 10.0 * n as f64;
                    traced(480 * n, 480, at + if n == 4 { shift } else { 0.0 }, 0, (n, at))
                })
                .collect()
        };
        let mut rows = steady(1.0);
        assert_eq!(classify_stamps(&mut rows, &[], 10_000_000), Some(100_000), "the 10 ms grid");
        assert!(rows.iter().all(|r| !r.windows_stamped));
        let c = check_steps(&rows);
        assert_eq!((c.windows_stamped, c.unexplained.clone(), c.wrong.clone()), (0, vec![(4, 10_000)], vec![4, 5]));
        assert_eq!(verdict(&rows, 0), Verdict::Fail);
        // 1 ms early: on no packet start either, 1 ms before the latest notification's.
        let mut rows = steady(-1.0);
        classify_stamps(&mut rows, &[], 10_000_000);
        assert_eq!(check_steps(&rows).unexplained, vec![(4, -10_000)]);
        // Where the steps can't catch it (frames lost on both sides, so neither step is checked), the listing still fails it.
        let mut rows = steady(1.0);
        for r in &mut rows[4..] {
            r.position += 480;
        }
        for r in &mut rows[5..] {
            r.position += 480;
        }
        classify_stamps(&mut rows, &[], 10_000_000);
        let c = check_steps(&rows);
        assert_eq!((c.gaps, c.wrong.len(), c.unexplained.clone()), (2, 0, vec![(4, 10_000)]));
        assert_eq!(verdict(&rows, 0), Verdict::Fail);
        // Undisplaced, it passes.
        let mut rows = steady(0.0);
        classify_stamps(&mut rows, &[], 10_000_000);
        assert_eq!(verdict(&rows, 0), Verdict::Pass);

        // After a resume: the exclusive trace's restamped packet, but after the driver's first packet since Start (1.51 ms
        // past its latest notification), fails; as the resume's first packet (the test above) it is Windows'.
        let first_run = [traced(0, 480, 0.0, 0, (10, 0.0)), traced(480, 480, 10.0, 0, (11, 10.0)), traced(960, 480, 20.0, 0, (12, 20.0))];
        let mut rows = first_run.to_vec();
        rows.extend([
            traced(1440, 480, 71.42, 1, (14, 71.42)),
            traced(1920, 480, 82.93, 1, (15, 81.42)),
            traced(2400, 480, 91.42, 1, (16, 91.42)),
        ]);
        classify_stamps(&mut rows, &[resume(30.0)], 10_000_000);
        let whose: Vec<(bool, bool, Option<i64>)> = rows[3..].iter().map(|r| (r.windows_stamped, r.late, r.unexplained)).collect();
        assert_eq!(whose, [(false, false, None), (false, false, Some(15_100)), (false, false, None)]);
        let c = check_steps(&rows);
        assert_eq!((c.across, c.windows_stamped, c.unexplained.clone()), (1, 0, vec![(4, 15_100)]));
        assert_eq!(verdict(&rows, 1), Verdict::Fail);
        // The resume's leading packets may hold the driver's from before the Stop and then Windows' (the Call Microphone,
        // Astra's validation run, 2026-11-24: resume 7's first packet the driver's from before Stop, its second Windows').
        let mut rows =
            vec![traced(0, 240, 0.0, 0, (100, 0.0)), traced(240, 240, 5.0, 0, (101, 5.0)), traced(480, 240, 10.0, 0, (102, 10.0))];
        rows.extend([
            traced(720, 240, 15.0, 1, (104, 20.0)),
            traced(960, 240, 42.75, 1, (104, 20.0)),
            traced(1200, 240, 46.88, 1, (105, 46.88)),
            traced(1440, 240, 51.88, 1, (106, 51.88)),
        ]);
        classify_stamps(&mut rows, &[resume(20.0)], 10_000_000);
        let whose: Vec<(bool, bool, Option<i64>)> = rows[3..].iter().map(|r| (r.windows_stamped, r.late, r.unexplained)).collect();
        assert_eq!(whose, [(false, true, None), (true, false, None), (false, false, None), (false, false, None)]);
        let c = check_steps(&rows);
        assert_eq!((c.checked, c.across, c.windows_stamped, c.unexplained.len(), c.wrong.len()), (5, 1, 1, 0, 0));
        assert_eq!(verdict(&rows, 1), Verdict::Pass);
        // A stamp off the grid after the new run's first driver packet (1.37 ms past its latest notification): unexplained.
        rows[4..].copy_from_slice(&[
            traced(960, 240, 46.88, 1, (105, 46.88)),
            traced(1200, 240, 48.25, 1, (105, 46.88)),
            traced(1440, 240, 51.88, 1, (106, 51.88)),
        ]);
        classify_stamps(&mut rows, &[resume(20.0)], 10_000_000);
        assert!(rows.iter().all(|r| !r.windows_stamped));
        assert_eq!(check_steps(&rows).unexplained, vec![(5, 13_700)]);
        assert_eq!(verdict(&rows, 1), Verdict::Fail);
    }

    /// With no packet length from the first run (one row: --packets 1; or its rows all read after one notification), a stamp
    /// off the latest notification can't be placed: not unexplained, no FAIL for it, and the verdict INCONCLUSIVE at best (the
    /// fix's review, 2026-11-24). A step that is wrong still fails.
    #[test]
    fn stamps_are_inconclusive_without_the_drivers_packet_grid() {
        let judge = |rows: &mut [StampRow], done: &[Resume]| {
            let grid = classify_stamps(rows, done, 10_000_000);
            let c = check_steps(rows);
            (grid, c.unexplained.clone(), stamps_verdict(&c, rows.len(), done.len(), 0, grid.is_some()).0)
        };
        // Shared at 5 ms (the Call Microphone): the client's packets a packet or two behind the latest notification, all read
        // after the same one. Before, the first two were unexplained and failed.
        let mut rows = [traced(0, 240, 0.0, 0, (100, 10.0)), traced(240, 240, 5.0, 0, (100, 10.0)), traced(480, 240, 10.0, 0, (100, 10.0))];
        assert_eq!(judge(&mut rows, &[]), (None, vec![], Verdict::Inconclusive));
        assert!(rows.iter().all(|r| !r.windows_stamped && !r.late));
        assert_eq!(check_steps(&rows).checked, 2);
        // The same with a packet number for each: the 5 ms grid, and PASS.
        for (n, r) in rows.iter_mut().enumerate() {
            (r.trace.last_notify_packet, r.trace.last_notify_qpc) = (100 + n as u64, 50_000 * n as u64);
        }
        assert_eq!(judge(&mut rows, &[]), (Some(50_000), vec![], Verdict::Pass));
        // One row in the first run, then a resume: its first packet off the notification at Stop is still left out as
        // Windows', and nothing else is judged off the grid.
        let mut rows = [
            traced(0, 480, 0.0, 0, (10, 0.0)),
            traced(480, 480, 22.93, 1, (11, 10.0)),
            traced(960, 480, 21.42, 1, (12, 21.42)),
            traced(1440, 480, 31.42, 1, (13, 31.42)),
        ];
        assert_eq!(judge(&mut rows, &[resume(10.0)]), (None, vec![], Verdict::Inconclusive));
        assert_eq!(rows.iter().map(|r| r.windows_stamped).collect::<Vec<_>>(), [false, true, false, false]);
        // A step in a run that isn't its packet's length fails, grid or not.
        rows[3].device_hns = Some(320_000);
        assert_eq!(judge(&mut rows, &[resume(10.0)]).2, Verdict::Fail);
    }

    /// stamps' verdict (Astra's review and recheck, 2026-11-23): a run's stamps step by exactly a packet; across a resume
    /// they may step by more than the frames between (the pause) but not less, each resume compared once; a position that
    /// doesn't follow on in a run isn't checked; a missing device time fails; a packet Windows stamped is left out.
    #[test]
    fn stamps_step_by_their_packets_and_never_overlap_across_a_resume() {
        let steady = [row(0, 0.0, 0), row(480, 10.0, 0), row(960, 20.0, 0)];
        assert_eq!(check_steps(&steady), StepCheck { checked: 2, ..Default::default() });
        // The bug: the second resumed packet only 1.32 ms after the first.
        let short = [row(0, 0.0, 0), row(480, 15.0, 1), row(960, 16.32, 1)];
        let c = check_steps(&short);
        assert_eq!((c.checked, c.across, c.wrong.clone(), c.overlapping.clone()), (2, 1, vec![2], vec![]));
        // A resume's own step: the pause on top is fine, less than a packet overlaps.
        assert!(check_steps(&[row(0, 0.0, 0), row(480, 13.0, 1)]).overlapping.is_empty());
        assert_eq!(check_steps(&[row(0, 0.0, 0), row(480, 6.0, 1)]).overlapping, vec![1]);
        // Frames lost in a run: not checked. No device time: counted.
        assert_eq!(check_steps(&[row(0, 0.0, 0), row(1440, 30.0, 0)]).gaps, 1);
        let mut none = row(480, 10.0, 0);
        none.device_hns = None;
        assert_eq!(check_steps(&[row(0, 0.0, 0), none]).no_time, 1);
        // Across a resume the position may move on past the first packet (a shared engine's skip, flagged a discontinuity):
        // still compared, by the frames between. The saved shared run (round 3, 160 frames) compared none of its eight.
        let (mut before, mut skipped) = (row(0, 0.0, 0), row(320, 7.05, 1));
        (before.frames, skipped.frames, skipped.discontinuity) = (160, 160, true);
        let c = check_steps(&[before, skipped]);
        assert_eq!((c.checked, c.across, c.overlapping.len(), c.flagged.len()), (1, 1, 0, 0));
        // Under the 320 frames' 6.67 ms: overlapping.
        skipped.device_hns = Some(60_000);
        assert_eq!(check_steps(&[before, skipped]).overlapping, vec![1]);
        // A position going back across a resume can't be compared.
        assert_eq!(check_steps(&[row(960, 0.0, 0), row(480, 30.0, 1)]), StepCheck { gaps: 1, ..Default::default() });
        // Exclusive, Astra's trace: the packet left unread at Stop, handed out just after Start with a stamp Windows gave
        // (72.93 ms on), then the driver's next 1.51 ms earlier. Left out; the step across the resume is the driver's two
        // stamps, 71.42 ms for 960 frames; the step back the client saw is listed.
        let mut pending = row(480, 72.93, 1);
        pending.windows_stamped = true;
        let c = check_steps(&[row(0, 0.0, 0), pending, row(960, 71.42, 1), row(1440, 81.42, 1)]);
        assert_eq!((c.windows_stamped, c.checked, c.across, c.gaps, c.wrong.len()), (1, 2, 1, 0, 0));
        assert_eq!((c.overlapping.len(), c.backward.clone()), (0, vec![2]));
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const VALUED: [&str; 2] = ["--seconds", "--device"];
    const FLAGS: [&str; 1] = ["--quiet"];

    fn parse(list: &[&str]) -> Result<(Option<u64>, Option<String>, bool), String> {
        let a = args(list);
        let o = Opts::parse(&a, &VALUED, &FLAGS)?;
        let seconds = o.values.iter().any(|(n, _)| *n == "--seconds").then(|| o.value("--seconds", 0u64)).transpose()?;
        let device = o.values.iter().any(|(n, _)| *n == "--device").then(|| o.value("--device", String::new())).transpose()?;
        Ok((seconds, device, o.flag("--quiet")))
    }

    #[test]
    fn options_and_flags() {
        assert_eq!(parse(&[]), Ok((None, None, false)));
        assert_eq!(parse(&["--seconds", "5", "--quiet"]), Ok((Some(5), None, true)));
        assert_eq!(parse(&["--quiet", "--device", "Mic (X)", "--seconds", "0"]), Ok((Some(0), Some("Mic (X)".into()), true)));
        // A value may start with one dash, or be empty; a default applies only to an option not given.
        assert_eq!(parse(&["--device", "-x"]), Ok((None, Some("-x".into()), false)));
        assert_eq!(parse(&["--device", ""]), Ok((None, Some(String::new()), false)));
        let a = args(&[]);
        assert_eq!(Opts::parse(&a, &VALUED, &FLAGS).unwrap().value("--seconds", 7u64), Ok(7));
    }

    #[test]
    fn an_option_name_is_never_a_value() {
        // The old parser took "--seconds" as the device's value and then indexed past the end.
        assert!(parse(&["--device", "--seconds"]).unwrap_err().contains("needs a value, not the option --seconds"));
        // And "--quiet" as the target's, setting quiet.
        assert!(parse(&["--seconds", "--quiet"]).unwrap_err().contains("not the option --quiet"));
        assert!(parse(&["--seconds"]).unwrap_err().contains("needs a value"));
        assert!(parse(&["--seconds", "5", "--device"]).unwrap_err().contains("needs a value"));
    }

    #[test]
    fn bad_arguments_are_errors_not_panics() {
        for list in [
            &["--bogus"][..],
            &["5"],
            &["--seconds", "5", "--seconds", "6"],
            &["--quiet", "--quiet"],
            &["--seconds", "five"],
            &["--seconds", "-1"],
            &["--seconds", "99999999999999999999999"],
            &["-"],
            &["--"],
            &[""],
            &["--seconds", "5", "extra"],
        ] {
            assert!(parse(list).is_err(), "{list:?}");
        }
    }

    // ---- the verdicts, on synthetic statistics ----

    use nf_vmic::protocol::{EVENTF_RING, LoopbackStats, RingStats};
    use nf_vmic::signal::Gap;

    const PID: u32 = 4242;
    /// Ten seconds at 48 kHz.
    const FRAMES: u64 = 480_000;

    /// STATS with process PID's ring attached and read in 480-frame packets, the ring's counters as given.
    fn reading(ring: RingStats) -> Stats {
        Stats {
            flags: STATS_ATTACHED | STATS_LOOPBACK | STATS_READING,
            attached_pid: PID,
            attached_capacity_frames: 8192,
            reader_packet_frames: 480,
            ring,
            ..Default::default()
        }
    }

    fn counters(attaches: u64, frames_copied: u64, underrun_frames: u64) -> RingStats {
        RingStats { attaches, frames_copied, underrun_frames, ..Default::default() }
    }

    /// A clean recording of FRAMES frames: every one a code, in order.
    fn clean_recording() -> CheckReport {
        CheckReport {
            frames: FRAMES,
            signal_frames: FRAMES,
            first_code: Some(0),
            last_code: Some(FRAMES as u32 - 1),
            steps_ok: FRAMES - 1,
            ..Default::default()
        }
    }

    /// The names of the checks that didn't pass: a failed one's name, an inconclusive one's with ": inconclusive" (so a test
    /// expecting either tells them apart).
    fn failed(checks: &[Check]) -> Vec<String> {
        checks
            .iter()
            .filter_map(|c| match c.outcome {
                Outcome::Ok => None,
                Outcome::Fail => Some(c.name.to_string()),
                Outcome::Inconclusive => Some(format!("{}: inconclusive", c.name)),
            })
            .collect()
    }

    /// verify's checks for a recording `r` between `before` and `after`, with nothing lost and no discontinuity.
    fn verify_failures(before: &Stats, after: &Stats, r: &CheckReport) -> Vec<String> {
        failed(&verify_checks(before, after, r, &StreamSeen::default()))
    }

    /// The attachment the synthetic statistics' ring is (`counters(3, ..)`).
    const ATTACH: u32 = 3;

    /// A skip-log entry of this attachment: number `sequence`, `kind`, stream `stream`, `frames` counted, the ring's frames
    /// `[lo, hi)`.
    fn entry(sequence: u64, kind: u16, stream: u32, frames: u32, (lo, hi): (u64, u64)) -> Event {
        Event {
            sequence,
            kind,
            stream,
            frames,
            attach: ATTACH,
            packet_frames: 240,
            flags: EVENTF_RING,
            ring_from: lo,
            ring_to: hi,
            ..Default::default()
        }
    }

    /// `s` with `events` in its skip log, each in its slot as STATS copies them; the log's count the newest's.
    fn logged(mut s: Stats, events: &[Event]) -> Stats {
        for e in events {
            s.skip_log.0[((e.sequence - 1) % SKIP_LOG_ENTRIES as u64) as usize] = *e;
            s.skip_events_logged = s.skip_events_logged.max(e.sequence);
        }
        s
    }

    /// A clean recording but for its gaps, each the ring's frames `[lo, hi)` it lost (the feed from frame 0: a code is its
    /// ring frame, below 2^23).
    fn gapped(spans: &[(u64, u64)]) -> CheckReport {
        CheckReport {
            gaps: spans.len() as u64,
            gap_frames: spans.iter().map(|(lo, hi)| hi - lo).sum(),
            gap_list: spans.iter().map(|&(lo, hi)| Gap { index: lo, offset: lo, due: lo as u32, got: hi as u32, silence: 0 }).collect(),
            first_code_index: Some(0),
            steps_ok: FRAMES - 1 - spans.len() as u64,
            ..clean_recording()
        }
    }

    #[test]
    fn stats_flags_map_to_the_headers_state() {
        let with = |flags| Stats { flags, ..Default::default() };
        let s = with(STATS_ATTACHED | STATS_LOOPBACK | STATS_READING);
        assert_eq!(stats_state(&s), STATE_ATTACHED | STATE_READING);
        assert!(is_reading(&s));
        for flags in [
            STATS_ATTACHED | STATS_READING | STATS_FORMAT_MISMATCH,
            STATS_ATTACHED | STATS_FORMAT_MISMATCH,
            STATS_ATTACHED | STATS_LOOPBACK,
            STATS_READING,
            STATS_LOOPBACK | STATS_READING,
            0,
        ] {
            assert!(!is_reading(&with(flags)), "flags {flags:#x}");
        }
        assert_eq!(stats_state(&with(STATS_ATTACHED | STATS_FORMAT_MISMATCH)), STATE_ATTACHED | STATE_FORMAT_MISMATCH);
        // The bits are protocol.rs's (nf_vmic.h's): STATS_READING is 0x4, STATE_READING 0x2.
        assert_eq!((STATS_ATTACHED, STATS_LOOPBACK, STATS_READING, STATS_FORMAT_MISMATCH), (0x1, 0x2, 0x4, 0x8));
        assert_eq!((STATE_ATTACHED, STATE_READING, STATE_FORMAT_MISMATCH), (0x1, 0x2, 0x4));
    }

    #[test]
    fn verify_passes_a_clean_recording() {
        let before = reading(counters(3, 1_000_000, 50));
        let after = reading(counters(3, 1_000_000 + FRAMES + 960, 50));
        let checks = verify_checks(&before, &after, &clean_recording(), &StreamSeen::default());
        assert_eq!(checks.len(), 9);
        assert_eq!(failed(&checks), Vec::<&str>::new(), "{checks:?}");
        // Silence the driver counted as underruns passes, inside the signal or at its edges.
        let after = reading(counters(3, 1_000_000 + FRAMES, 50 + 3_000));
        let r = CheckReport {
            signal_frames: FRAMES - 3_000,
            zeros_before_signal: 400,
            zeros_inside: 2_000,
            zero_runs_inside: 4,
            zeros_after_signal: 600,
            steps_ok: FRAMES - 3_001,
            ..clean_recording()
        };
        assert_eq!(verify_failures(&before, &after, &r), Vec::<&str>::new());
    }

    #[test]
    fn verify_fails_without_proof() {
        let before = reading(counters(3, 1_000_000, 0));
        let after = reading(counters(3, 1_000_000 + FRAMES, 0));
        let r = clean_recording();

        let mismatch = Stats { flags: STATS_ATTACHED | STATS_FORMAT_MISMATCH, ..before };
        assert_eq!(verify_failures(&mismatch, &after, &r), ["just before the recording"]);
        let not_attached = Stats { flags: STATS_LOOPBACK, attached_pid: 0, ..before };
        // (And the attachment after isn't the one before.)
        assert_eq!(verify_failures(&not_attached, &after, &r), ["just before the recording", "just after it"]);
        assert_eq!(verify_failures(&before, &Stats { attached_pid: PID + 1, ..after }, &r), ["just after it"]);
        assert_eq!(verify_failures(&before, &reading(counters(4, 1_000_000 + FRAMES, 0)), &r), ["just after it"]);
        assert_eq!(verify_failures(&before, &Stats { flags: STATS_ATTACHED, ..after }, &r), ["just after it"]);

        // The driver copied 89% of the frames recorded.
        assert_eq!(verify_failures(&before, &reading(counters(3, 1_000_000 + FRAMES * 89 / 100, 0)), &r), [
            "the driver copied the recording"
        ]);
        // Signal in 89% of the frames, the rest silence the driver counted.
        let underran = reading(counters(3, 1_000_000 + FRAMES, FRAMES * 11 / 100));
        let r89 = CheckReport { signal_frames: FRAMES * 89 / 100, zeros_inside: FRAMES * 11 / 100, zero_runs_inside: 1, ..r.clone() };
        assert_eq!(verify_failures(&before, &underran, &r89), ["signal"]);
        // Silence inside the signal past the underruns and a packet.
        let inside = CheckReport { signal_frames: FRAMES - 481, zeros_inside: 481, zero_runs_inside: 1, ..r.clone() };
        assert_eq!(verify_failures(&before, &after, &inside), ["silence inside the signal"]);
        // Silence at the edges nothing counted: another stream held the reader slot for the first 10% of the recording (the
        // signal check alone lets 10% through).
        let edges = CheckReport { signal_frames: FRAMES * 9 / 10, zeros_before_signal: FRAMES / 10, ..r.clone() };
        assert_eq!(verify_failures(&before, &after, &edges), ["all the silence"]);
        let trailing = CheckReport { signal_frames: FRAMES - 961, zeros_after_signal: 961, ..r.clone() };
        assert_eq!(verify_failures(&before, &after, &trailing), ["all the silence"]);
        let both = CheckReport { signal_frames: FRAMES - 960, zeros_before_signal: 480, zeros_after_signal: 480, ..r.clone() };
        assert_eq!(verify_failures(&before, &after, &both), Vec::<&str>::new(), "a packet's slack at each end");

        // One corrupt frame, a gap with no entry, a repeat.
        let corrupt = CheckReport { signal_frames: FRAMES - 1, corrupt_frames: 1, ..r.clone() };
        let bad = ["bit-exact and in order"];
        assert_eq!(verify_failures(&before, &after, &corrupt), bad);
        assert_eq!(verify_failures(&before, &after, &gapped(&[(1_000, 1_005)])), bad);
        assert_eq!(verify_failures(&before, &after, &CheckReport { repeats: 1, ..r.clone() }), bad);
        // A gap past those the recording lists can't be tied.
        assert_eq!(verify_failures(&before, &after, &CheckReport { gaps: 1, gap_frames: 5, ..r.clone() }), bad);
        // A handoff the driver logged allows its gap: the ring's frames it lost, exactly; any other gap still fails.
        let skip = entry(1, EVENT_HANDOFF_SKIP, 7, 449, (1_000, 1_449));
        let handed = logged(Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..after }, &[skip]);
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(1_000, 1_449)])), Vec::<&str>::new(), "its gap");
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(1_000, 1_450)])), bad, "a frame more");
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(1_000, 1_448)])), bad, "a frame less");
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(1_001, 1_449)])), bad, "from the next frame");
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(5_000, 5_449)])), bad, "as many frames, elsewhere");
        assert_eq!(verify_failures(&before, &handed, &gapped(&[(1_000, 1_449), (6_000, 6_449)])), bad, "two gaps, one entry");
        let repeat = CheckReport { repeats: 1, ..gapped(&[(1_000, 1_449)]) };
        assert_eq!(verify_failures(&before, &handed, &repeat), bad);
        // Another attachment's entry for the same frames ties nothing.
        let elsewhere = logged(Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..after }, &[Event { attach: 2, ..skip }]);
        assert_eq!(verify_failures(&before, &elsewhere, &gapped(&[(1_000, 1_449)])), bad);
        let two = logged(handed, &[entry(2, EVENT_HANDOFF_SKIP, 8, 332, (6_000, 6_332))]);
        let two = Stats { reader_handoffs: 2, handoff_skipped_frames: 781, ..two };
        assert_eq!(verify_failures(&before, &two, &gapped(&[(1_000, 1_449), (6_000, 6_332)])), Vec::<&str>::new(), "two handoffs");
        // The counters without the log's entry, or the log without the counters: the cross-check fails.
        let unlogged = Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..after };
        assert_eq!(verify_failures(&before, &unlogged, &r), ["the skip log against the counters"]);
        assert_eq!(verify_failures(&before, &logged(after, &[skip]), &r), ["the skip log against the counters"]);
        let frames_off = Stats { handoff_skipped_frames: 450, ..handed };
        assert_eq!(verify_failures(&before, &frames_off, &r), ["the skip log against the counters"]);
        // One discontinuity, or frames the engine lost, in the recording's packets.
        let seen = |frames, discontinuities| StreamSeen {
            losses: StreamLosses { recording: Losses { frames, discontinuities }, ..Default::default() },
            packet: 480,
            ..Default::default()
        };
        assert_eq!(failed(&verify_checks(&before, &after, &r, &seen(0, 1))), ["the capture stream"]);
        assert_eq!(failed(&verify_checks(&before, &after, &r, &seen(10, 0))), ["the capture stream"]);
        // Nothing recorded.
        let none = verify_failures(&before, &before, &CheckReport::default());
        assert_eq!(none, ["the driver copied the recording", "signal", "bit-exact and in order"]);
    }

    /// One kill's statistics: none attached before the child started; attached under PID and read in the window (1,000 then
    /// 10,600 frames copied); after, detached by one cancel.
    fn kill_stats() -> [Stats; 4] {
        let r0 = RingStats { attaches: 5, detaches: 5, detach_cancel: 3, detach_cleanup: 1, detach_removal: 1, ..Default::default() };
        let start = Stats { flags: STATS_LOOPBACK, ring: r0, ..Default::default() };
        let a = reading(RingStats { attaches: 6, frames_copied: 1_000, ..r0 });
        let b = reading(RingStats { attaches: 6, frames_copied: 10_600, ..r0 });
        let after = Stats {
            flags: STATS_LOOPBACK,
            ring: RingStats { attaches: 6, detaches: 6, detach_cancel: 4, frames_copied: 11_000, ..r0 },
            ..Default::default()
        };
        [start, a, b, after]
    }

    fn judge(s: &[Stats; 4], exited: bool, detached: bool, no_capture: bool) -> KillVerdict {
        judge_kill(&KillSeen { pid: PID, start: &s[0], a: &s[1], b: &s[2], after: &s[3], exited, detached, no_capture })
    }

    #[test]
    fn a_kill_through_the_cancel_path_passes() {
        let v = judge(&kill_stats(), true, true, false);
        assert!(v.ok && !v.unread && v.problems.is_empty(), "{v:?}");
        assert_eq!((v.read, v.cancel, v.cleanup, v.removal, v.attaches, v.detaches), (9_600, 1, 0, 0, 1, 1));
        // --no-capture with another client reading: the same.
        let v = judge(&kill_stats(), true, true, true);
        assert!(v.ok && !v.unread, "{v:?}");
    }

    #[test]
    fn a_kill_fails_off_the_cancel_path_or_out_of_bounds() {
        let with_after = |f: fn(&mut RingStats)| {
            let mut s = kill_stats();
            f(&mut s[3].ring);
            s
        };
        let cases: [(&str, [Stats; 4], bool, bool); 8] = [
            (
                "the handle's cleanup instead of the cancel",
                with_after(|r| {
                    r.detach_cancel -= 1;
                    r.detach_cleanup += 1
                }),
                true,
                true,
            ),
            ("cancel +2", with_after(|r| r.detach_cancel += 1), true, true),
            ("removal +1", with_after(|r| r.detach_removal += 1), true, true),
            ("attaches +0", with_after(|r| r.attaches -= 1), true, true),
            ("attaches +2", with_after(|r| r.attaches += 1), true, true),
            ("detaches +0", with_after(|r| r.detaches -= 1), true, true),
            ("exited later than 2 s", kill_stats(), false, false),
            ("still attached 1 s after its exit", kill_stats(), true, false),
        ];
        for (name, s, exited, detached) in cases {
            let v = judge(&s, exited, detached, false);
            assert!(!v.ok && !v.problems.is_empty(), "{name}: {v:?}");
            // With --no-capture too: only the reading is another client's to show.
            assert!(!judge(&s, exited, detached, true).ok, "{name}, --no-capture");
        }
    }

    #[test]
    fn a_kill_not_mid_stream_fails_or_is_inconclusive() {
        let unread = |f: fn(&mut [Stats; 4])| {
            let mut s = kill_stats();
            f(&mut s);
            s
        };
        for (name, s) in [
            ("no frames copied in the window", unread(|s| s[2].ring.frames_copied = s[1].ring.frames_copied)),
            ("a format mismatch", unread(|s| s[1].flags = STATS_ATTACHED | STATS_FORMAT_MISMATCH)),
            ("another process's ring", unread(|s| s[2].attached_pid = PID + 1)),
            ("not attached at the window's start", unread(|s| s[1].flags = STATS_LOOPBACK)),
        ] {
            let v = judge(&s, true, true, false);
            assert!(!v.ok && !v.unread, "{name}: {v:?}");
            // --no-capture: unread, not failed (the verdict is INCONCLUSIVE).
            let v = judge(&s, true, true, true);
            assert!(v.ok && v.unread, "{name}, --no-capture: {v:?}");
        }
    }

    #[test]
    fn the_kill_tests_verdict() {
        assert_eq!(kill_test_verdict(0, 0, true), Verdict::Pass);
        assert_eq!(kill_test_verdict(1, 0, true), Verdict::Fail);
        assert_eq!(kill_test_verdict(2, 3, false), Verdict::Fail);
        assert_eq!(kill_test_verdict(0, 1, true), Verdict::Inconclusive);
        assert_eq!(kill_test_verdict(0, 0, false), Verdict::Inconclusive);
    }

    #[test]
    fn a_target_below_the_readers_packet_ends_the_feed() {
        let read = STATE_ATTACHED | STATE_READING;
        assert_eq!(packet_above_target(960, read, 480), None);
        assert_eq!(packet_above_target(480, read, 480), None, "one packet is enough");
        assert_eq!(packet_above_target(479, read, 480), Some(480));
        assert_eq!(packet_above_target(100, read, 1_440), Some(1_440));
        // Judged only while the slot's stream reads the ring: not with none (0 frames), nor in another format's frames.
        assert_eq!(packet_above_target(100, STATE_ATTACHED, 0), None);
        assert_eq!(packet_above_target(100, STATE_ATTACHED | STATE_FORMAT_MISMATCH, 1_323), None);
        assert_eq!(packet_above_target(100, 0, 480), None);
    }

    /// A secondary stream's recording: its silence is what the driver zero-filled for it (SecondaryUnderrunFrames), and the
    /// slack at the edges is a packet of its own period when that is the larger.
    #[test]
    fn verify_allows_a_secondary_streams_underruns() {
        let before = Stats { secondary_underrun_frames: 100, ..reading(counters(3, 1_000_000, 0)) };
        let after = Stats { secondary_underrun_frames: 100 + 2_000, ..reading(counters(3, 1_000_000 + FRAMES, 0)) };
        let r = CheckReport { signal_frames: FRAMES - 2_000, zeros_inside: 2_000, zero_runs_inside: 3, ..clean_recording() };
        assert_eq!(verify_failures(&before, &after, &r), Vec::<&str>::new());
        // The same silence with no secondary underruns counted fails.
        let after_none = Stats { secondary_underrun_frames: 100, ..after };
        assert_eq!(verify_failures(&before, &after_none, &r), ["silence inside the signal", "all the silence"]);
        // A 128-frame reader and this stream at 480 frames: one of this stream's packets of slack at each end.
        let small = |s: Stats| Stats { reader_packet_frames: 128, ..s };
        let (b, a) = (small(reading(counters(3, 1_000_000, 0))), small(reading(counters(3, 1_000_000 + FRAMES, 0))));
        let edges = CheckReport { signal_frames: FRAMES - 960, zeros_before_signal: 480, zeros_after_signal: 480, ..clean_recording() };
        assert_eq!(failed(&verify_checks(&b, &a, &edges, &StreamSeen::default())), ["all the silence"], "the reader's 128 only");
        let own = StreamSeen { packet: 480, ..Default::default() };
        assert_eq!(failed(&verify_checks(&b, &a, &edges, &own)), Vec::<&str>::new());
    }

    /// Three capture streams and one handoff (Astra's review, 2026-11-23; run A of nf-scratch's three.sh): a 160-frame reader
    /// stops, a 480-frame stream takes the slot and a 240-frame one plays on as another stream. The taker loses its skip (its
    /// lag at most: 209 frames), tied to its entry; the one that plays on gets the difference in the readers' packets as
    /// silence, counted, with no gap and no repeat.
    #[test]
    fn verify_judges_each_stream_of_a_three_stream_handoff() {
        let before = Stats { reader_packet_frames: 160, ..reading(counters(3, 1_000_000, 0)) };
        let taker_skip = entry(1, EVENT_HANDOFF_SKIP, 8, 209, (300_000, 300_209));
        let after = logged(
            Stats {
                reader_packet_frames: 480,
                reader_handoffs: 1,
                handoff_skipped_frames: 209,
                secondary_underrun_frames: 320,
                ..reading(counters(3, 1_000_000 + FRAMES, 0))
            },
            &[taker_skip],
        );
        assert_eq!(verify_failures(&before, &after, &gapped(&[(300_000, 300_209)])), Vec::<&str>::new(), "the taker");
        let on = CheckReport { signal_frames: FRAMES - 320, zeros_inside: 320, zero_runs_inside: 1, ..clean_recording() };
        let seen = StreamSeen { packet: 240, ..Default::default() };
        assert_eq!(failed(&verify_checks(&before, &after, &on, &seen)), Vec::<&str>::new(), "the stream that plays on");
        // The driver before the fix: the stream that played on had the 320 frames again, and lost what the new reader's first
        // packet left out of the copy (151 frames, say), with nothing logged for either.
        let old = CheckReport { repeats: 1, repeat_frames: 320, ..gapped(&[(300_100, 300_251)]) };
        let old_after = Stats { secondary_underrun_frames: 0, ..after };
        assert_eq!(failed(&verify_checks(&before, &old_after, &old, &seen)), ["bit-exact and in order"]);
        // Its gap alone fails too: no entry is for it.
        let gap_only = gapped(&[(300_100, 300_251)]);
        assert_eq!(failed(&verify_checks(&before, &old_after, &gap_only, &seen)), ["bit-exact and in order"]);
        // The 320 frames fit in the one-packet slack (480) even with nothing counted, so the case above can't tell. More
        // silence than the slack passes only where the driver counted it: 320 frames and 481 more, 801, fail uncounted.
        let more = CheckReport { signal_frames: FRAMES - 801, zeros_inside: 801, zero_runs_inside: 1, ..clean_recording() };
        let counted = |frames| Stats { secondary_underrun_frames: frames, ..after };
        assert_eq!(failed(&verify_checks(&before, &counted(0), &more, &seen)), ["silence inside the signal"]);
        assert_eq!(failed(&verify_checks(&before, &counted(320), &more, &seen)), ["silence inside the signal"]);
        assert_eq!(failed(&verify_checks(&before, &counted(801), &more, &seen)), Vec::<&str>::new());
        // The live run on a66d8ab (A1, 2026-11-23): the stopping reader held the slot while its timer stopped, so the stream that
        // played on read past the copy's end and lost the 55 frames the taker then wrote there. Its silence was counted (430:
        // the 320, and the 110 past the copy's end), its gap nowhere: verify failed it, which found the bug (m_AppNoSlot). The
        // skip log now has it as a late read, its ring span the taker's writes there: tied, and still failed, a loss the
        // ring's rules don't allow (the owner allowed a taker's skip and a skip to a shorter lag). Without the entry it fails
        // as no entry's.
        let late = entry(2, EVENT_LATE_READ, 9, 110, (300_209, 300_264));
        let live = CheckReport { signal_frames: FRAMES - 430, zeros_inside: 430, zero_runs_inside: 1, ..gapped(&[(300_209, 300_264)]) };
        let with_late = logged(counted(430), &[late]);
        assert_eq!(failed(&verify_checks(&before, &with_late, &live, &seen)), ["bit-exact and in order"]);
        let ties = tie_gaps(&before, &live, 0, &LogWindow::of(&before, &with_late));
        assert_eq!(ties[0].outcome, Outcome::Fail);
        assert!(
            ties[0]
                .text
                .contains("#2 late read (stream 9, 110 frames, ring [300209, 300264)): the driver's record of a loss (a late read)"),
            "{}",
            ties[0].text
        );
        assert_eq!(failed(&verify_checks(&before, &counted(430), &live, &seen)), ["bit-exact and in order"]);
        let ties = tie_gaps(&before, &live, 0, &LogWindow::of(&before, &counted(430)));
        assert!(ties[0].text.ends_with("no entry in the skip log for it"), "{}", ties[0].text);
        // A skip to a shorter lag logged in the window (another stream's, say) ties a gap of exactly its frames, and no other.
        let other = entry(2, EVENT_SECONDARY_SKIP, 10, 80, (310_000, 310_080));
        let skipped_too = logged(Stats { secondary_skips: 1, secondary_skipped_frames: 80, ..after }, &[other]);
        let gap = |lo, hi| CheckReport { signal_frames: on.signal_frames, zeros_inside: 320, zero_runs_inside: 1, ..gapped(&[(lo, hi)]) };
        assert_eq!(failed(&verify_checks(&before, &skipped_too, &gap(310_000, 310_081), &seen)), ["bit-exact and in order"]);
        assert_eq!(failed(&verify_checks(&before, &skipped_too, &gap(310_000, 310_080), &seen)), Vec::<&str>::new());
    }

    /// The other way (run B of nf-scratch's three.sh; the owner, 2026-11-23): a 480-frame reader stops, a 160-frame stream takes
    /// the slot and a 240-frame one plays on. The taker loses its skip (449 frames here, its lag of 529 at most); the one that
    /// plays on moves on to the new reader's lag at once and loses the difference in the readers' packets, 320 frames, once,
    /// with no silence and no repeat. Each gap is tied to its own entry in the skip log, exactly.
    #[test]
    fn verify_judges_each_stream_of_a_handoff_to_a_shorter_packet() {
        let before = reading(counters(3, 1_000_000, 0));
        let counted = Stats {
            reader_packet_frames: 160,
            reader_handoffs: 1,
            handoff_skipped_frames: 449,
            secondary_skips: 1,
            secondary_skipped_frames: 320,
            ..reading(counters(3, 1_000_000 + FRAMES, 0))
        };
        let taker = entry(1, EVENT_HANDOFF_SKIP, 8, 449, (300_000, 300_449));
        let on = entry(2, EVENT_SECONDARY_SKIP, 9, 320, (300_100, 300_420));
        let after = logged(counted, &[taker, on]);
        let seen = StreamSeen { packet: 240, ..Default::default() };
        let judge = |after: &Stats, r: &CheckReport| failed(&verify_checks(&before, after, r, &seen));
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_449)])), Vec::<&str>::new(), "the taker");
        assert_eq!(judge(&after, &gapped(&[(300_100, 300_420)])), Vec::<&str>::new(), "the stream that plays on");
        assert_eq!(judge(&after, &clean_recording()), Vec::<&str>::new(), "no gap at all (the stopping reader's own recording)");
        // Off by a frame, two gaps for one skip, both skips' frames in one gap, a repeat: all fail.
        let bad = ["bit-exact and in order"];
        assert_eq!(judge(&after, &gapped(&[(300_100, 300_421)])), bad, "a frame more");
        assert_eq!(judge(&after, &gapped(&[(300_100, 300_419)])), bad, "a frame less");
        assert_eq!(judge(&after, &gapped(&[(300_100, 300_260), (300_260, 300_420)])), bad, "two gaps, one skip");
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_769)])), bad, "the taker's and the bystander's frames together");
        assert_eq!(judge(&after, &CheckReport { repeats: 1, repeat_frames: 320, ..clean_recording() }), bad);
        // The same gap with the skip counted but not logged: no entry ties it, and the log and the counters disagree.
        assert_eq!(judge(&counted, &gapped(&[(300_100, 300_420)])), ["bit-exact and in order", "the skip log against the counters"]);
        // A skip that passed over nothing the copy held: an entry with no ring span, which ties no gap.
        let empty = Stats { secondary_skips: 1, secondary_skipped_frames: 0, handoff_skipped_frames: 0, reader_handoffs: 0, ..counted };
        let empty = logged(empty, &[Event { sequence: 1, flags: 0, frames: 0, ring_from: 0, ring_to: 0, ..on }]);
        assert_eq!(judge(&empty, &gapped(&[(300_100, 300_101)])), bad);
        assert_eq!(judge(&empty, &clean_recording()), Vec::<&str>::new());
    }

    /// Two recorders playing on through one handoff to a shorter packet (Astra's four-stream model, 2026-11-24): each skips
    /// once (334 and 352 frames here) and the counters rise by both (SecondarySkips +2, 686 frames), so neither gap matched them
    /// alone: verify before the skip log was INCONCLUSIVE. Each gap is its own stream's entry: a PASS each. A gap from one's
    /// first frame with the other's length, or both skips' frames, fails.
    #[test]
    fn verify_ties_each_of_two_streams_skips_to_its_own_entry() {
        let before = reading(counters(3, 1_000_000, 0));
        let taker = entry(1, EVENT_HANDOFF_SKIP, 8, 449, (300_000, 300_449));
        let first = entry(2, EVENT_SECONDARY_SKIP, 9, 334, (300_090, 300_424));
        let second = entry(3, EVENT_SECONDARY_SKIP, 10, 352, (300_070, 300_422));
        let counted = Stats {
            reader_packet_frames: 160,
            reader_handoffs: 1,
            handoff_skipped_frames: 449,
            secondary_skips: 2,
            secondary_skipped_frames: 334 + 352,
            ..reading(counters(3, 1_000_000 + FRAMES, 0))
        };
        let after = logged(counted, &[taker, first, second]);
        let checks = |r: &CheckReport| verify_checks(&before, &after, r, &StreamSeen { packet: 240, ..Default::default() });
        for span in [(300_090, 300_424), (300_070, 300_422), (300_000, 300_449)] {
            let c = checks(&gapped(&[span]));
            assert_eq!(verdict_of(&c), Verdict::Pass, "{span:?}: {c:?}");
        }
        let bad = ["bit-exact and in order"];
        assert_eq!(failed(&checks(&gapped(&[(300_090, 300_442)]))), bad, "the first's start, the second's length");
        assert_eq!(failed(&checks(&gapped(&[(300_090, 300_776)]))), bad, "both skips' frames in one gap");
        // A repeat or a corrupt frame fails, whatever the gaps; the detail says what failed it.
        for (r, what) in [
            (CheckReport { repeats: 1, repeat_frames: 10, ..gapped(&[(300_090, 300_424)]) }, "the repeats"),
            (CheckReport { corrupt_frames: 1, ..gapped(&[(300_090, 300_424)]) }, "the corrupt frames"),
        ] {
            let c = checks(&r);
            assert_eq!(failed(&c), bad);
            let detail = &c.iter().find(|c| c.name == "bit-exact and in order").unwrap().detail;
            assert!(detail.contains("every gap is the skip log's record of a skip the ring's rules allow"), "{detail}");
            assert_eq!(others_named(&r), [what]);
        }
        // Another check failing makes the verdict FAIL.
        let one = StreamLosses { recording: Losses { discontinuities: 1, ..Default::default() }, ..Default::default() };
        let c = verify_checks(&before, &after, &gapped(&[(300_090, 300_424)]), &StreamSeen { losses: one, ..Default::default() });
        assert_eq!((failed(&c), verdict_of(&c)), (vec!["the capture stream".to_string()], Verdict::Fail));
    }

    /// What fails "bit-exact and in order" whatever the gaps, as verify_checks names it.
    fn others_named(r: &CheckReport) -> Vec<&'static str> {
        [(r.signal_frames == 0, "the missing signal"), (r.repeats > 0, "the repeats"), (r.corrupt_frames > 0, "the corrupt frames")]
            .into_iter()
            .filter_map(|(bad, what)| bad.then_some(what))
            .collect()
    }

    /// A gap may be a chain of one stream's entries end to end, and a recording may hold a gap of each kind (three.sh's B runs:
    /// a stream played on through one handoff, skipping, and took the slot at the next). Another stream's entry doesn't extend
    /// the chain. A chain that holds a late read (a skip to a shorter lag, then its late read: the copy didn't hold the whole
    /// span yet) ties, and fails: the late read is a loss the ring's rules don't allow. An entry still being filled in just
    /// after the recording is no gap's record (a stream's own entries for a gap are settled before it reads the code after
    /// it: LogWindow::settling), so a chain doesn't take one in, whatever its kind: the gap fails as no entry's, the entry
    /// listed as no candidate.
    #[test]
    fn verify_ties_a_gap_to_a_chain_of_one_streams_entries() {
        let before = reading(counters(3, 1_000_000, 0));
        let skip = entry(1, EVENT_SECONDARY_SKIP, 9, 320, (300_000, 300_320));
        let late = entry(2, EVENT_LATE_READ, 9, 240, (300_320, 300_400));
        let other = entry(3, EVENT_LATE_READ, 10, 240, (300_320, 300_500));
        let handoff = entry(4, EVENT_HANDOFF_SKIP, 9, 209, (400_000, 400_209));
        // Two allowed skips of one stream's end to end (synthetic: a chain of the two kinds the rules allow).
        let first = entry(5, EVENT_SECONDARY_SKIP, 11, 100, (450_000, 450_100));
        let then = entry(6, EVENT_HANDOFF_SKIP, 11, 50, (450_100, 450_150));
        let counted = Stats {
            reader_handoffs: 2,
            handoff_skipped_frames: 259,
            secondary_skips: 2,
            secondary_skipped_frames: 420,
            ..reading(counters(3, 1_000_000 + FRAMES, 0))
        };
        let after = logged(counted, &[skip, late, other, handoff, first, then]);
        let judge = |after: &Stats, r: &CheckReport| failed(&verify_checks(&before, after, r, &StreamSeen::default()));
        let bad = ["bit-exact and in order"];
        let text = |after: &Stats, spans: &[(u64, u64)]| {
            tie_gaps(&before, &gapped(spans), 0, &LogWindow::of(&before, after)).into_iter().map(|t| t.text).collect::<Vec<_>>()
        };
        assert_eq!(judge(&after, &gapped(&[(450_000, 450_150)])), Vec::<&str>::new(), "a chain of allowed skips");
        assert_eq!(judge(&after, &gapped(&[(450_000, 450_150), (400_000, 400_209)])), Vec::<&str>::new(), "a gap of each kind");
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_320)])), Vec::<&str>::new(), "the skip alone");
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_400)])), bad, "a skip and its late read: tied, and a loss");
        let [t] = &text(&after, &[(300_000, 300_400)])[..] else { panic!() };
        assert!(
            t.contains(
                "#1 skip to a shorter lag (stream 9, 320 frames, ring [300000, 300320)), then #2 late read (stream 9, 240 frames, \
                 ring [300320, 300400))"
            ),
            "{t}"
        );
        assert!(t.contains("the driver's record of a loss (a late read)"), "{t}");
        assert_eq!(judge(&after, &gapped(&[(300_320, 300_500)])), bad, "the other stream's late read alone");
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_500)])), bad, "another stream's entry");
        // The late read still being filled in just after the recording: no gap's record, so the chain stops at the skip, and
        // the gap fails, whether the late read would reach its end or not; it is listed, as no candidate.
        let pending = logged(after, &[Event { flags: EVENTF_RING | EVENTF_PENDING, ..late }]);
        let window = LogWindow::of(&before, &pending);
        for hi in [300_400, 300_480] {
            assert_eq!(judge(&pending, &gapped(&[(300_000, hi)])), bad, "{hi}");
            let [t] = &tie_gaps(&before, &gapped(&[(300_000, hi)]), 0, &window)[..] else { panic!() };
            assert!(t.text.ends_with("ring [300000, 300320)) for its first frame reach to 300320, not its end"), "{}", t.text);
            assert_eq!(t.candidates.len(), 1, "{:?}", t.candidates);
            assert!(
                t.candidates[0].starts_with(
                    "not a candidate: #2 late read (stream 9, 240 frames, ring [300320, 300400), pending) meets its frames, but was \
                     still being filled in as the recording ended"
                ),
                "{:?}",
                t.candidates
            );
            assert_eq!(t.entries, [1], "the late read is no gap's");
        }
        assert_eq!(judge(&after, &gapped(&[(300_000, 300_480)])), bad, "settled: its frames are final");
        // The skip itself still being filled in: it may yet reach further, but not as this gap's record, which would have been
        // settled before the stream read the code after it. No entry for the gap: it fails (it was inconclusive).
        let one_skip =
            Stats { reader_handoffs: 0, handoff_skipped_frames: 0, secondary_skips: 1, secondary_skipped_frames: 320, ..counted };
        let open = logged(one_skip, &[Event { flags: EVENTF_RING | EVENTF_PENDING, ..skip }]);
        assert_eq!(judge(&open, &gapped(&[(300_000, 300_400)])), bad, "no gap's record");
        let [t] = &text(&open, &[(300_000, 300_400)])[..] else { panic!() };
        assert!(t.ends_with("no entry in the skip log for it"), "{t}");
        // Exactly the gap's frames, and still pending: no candidate all the same.
        assert_eq!(judge(&open, &gapped(&[(300_000, 300_320)])), bad, "pending, though it ties");
        // A resync's frames reached no stream: tied, and failed (the feed's write position was invalid).
        let resync = entry(7, EVENT_RESYNC, 7, 500, (500_000, 500_500));
        let resynced = logged(Stats { ring: RingStats { resyncs: 1, ..counted.ring }, ..after }, &[resync]);
        assert_eq!(judge(&resynced, &gapped(&[(500_000, 500_500)])), bad);
        let [t] = &text(&resynced, &[(500_000, 500_500)])[..] else { panic!() };
        assert!(t.contains("#7 resync (stream 7, 500 frames, ring [500000, 500500)): the feed's write position was invalid"), "{t}");
        // A jump in a stream's own time line, logged and counted (the device's counters): tied, and failed.
        let jump = entry(7, EVENT_SECONDARY_JUMP, 9, 192, (600_000, 600_192));
        let jumped = logged(Stats { secondary_jumps: 1, secondary_jump_frames: 192, ..after }, &[jump]);
        assert_eq!(judge(&jumped, &gapped(&[(600_000, 600_192)])), bad);
    }

    /// verify doesn't know its own stream: an entry of any stream's for exactly the gap's frames ties it, and where several
    /// streams' entries do (two streams' identical skips: the app ring model has them, each an allowed skip), every candidate
    /// is listed, with its stream, kind, frames and ring frames. The owner, 2026-11-24: INCONCLUSIVE only when the ambiguity
    /// changes the verdict. Candidates that agree give their verdict (two allowed skips pass the gap; two late reads, or a
    /// late read and a jump, fail it); only candidates judged differently (an allowed skip and a late read for the same frames)
    /// make it inconclusive.
    #[test]
    fn verify_names_every_streams_entry_for_a_gaps_frames() {
        let before = reading(counters(3, 1_000_000, 0));
        let counted = Stats { secondary_skips: 2, secondary_skipped_frames: 640, ..reading(counters(3, 1_000_000 + FRAMES, 0)) };
        let s2 = entry(1, EVENT_SECONDARY_SKIP, 12, 320, (4_789, 5_109));
        let s3 = entry(2, EVENT_SECONDARY_SKIP, 13, 320, (4_789, 5_109));
        let after = logged(counted, &[s2, s3]);
        let r = gapped(&[(4_789, 5_109)]);
        let judge = |after: &Stats| failed(&verify_checks(&before, after, &r, &StreamSeen::default()));
        let tie = |after: &Stats| tie_gaps(&before, &r, 0, &LogWindow::of(&before, after)).remove(0);
        assert_eq!(judge(&after), Vec::<&str>::new(), "two allowed skips: it passes either way");
        let t = tie(&after);
        assert_eq!(t.outcome, Outcome::Ok);
        assert!(t.text.contains("2 candidates for exactly its frames, of streams 12, 13"), "{}", t.text);
        assert!(t.text.contains("the log doesn't say whose the gap was, but the verdict is the same either way"), "{}", t.text);
        assert_eq!(t.candidates, [
            "candidate: the skip log's #1 skip to a shorter lag (stream 12, 320 frames, ring [4789, 5109)): a skip the ring's rules \
             allow",
            "candidate: the skip log's #2 skip to a shorter lag (stream 13, 320 frames, ring [4789, 5109)): a skip the ring's rules \
             allow",
        ]);
        assert_eq!(t.entries, [1, 2]);
        // Two losses for the same frames, of different kinds and streams: it fails either way, both listed.
        let losses = Stats { secondary_skips: 0, secondary_skipped_frames: 0, secondary_jumps: 1, secondary_jump_frames: 320, ..counted };
        let losses = logged(losses, &[Event { kind: EVENT_LATE_READ, ..s2 }, Event { kind: EVENT_SECONDARY_JUMP, ..s3 }]);
        assert_eq!(judge(&losses), ["bit-exact and in order"]);
        let t = tie(&losses);
        assert!(
            t.text.contains("every one the driver's record of a loss or of frames no stream got") && t.text.contains("it fails either way")
        );
        assert!(t.candidates[0].contains("#1 late read (stream 12, 320 frames, ring [4789, 5109)): the driver's record of a loss"));
        assert!(t.candidates[1].contains("#2 time-line jump (stream 13, 320 frames, ring [4789, 5109)): the driver's record of a loss"));
        // An allowed skip and a late read: whose the gap was decides it, and the log doesn't say. Inconclusive, both listed.
        let late = Stats { secondary_skips: 1, secondary_skipped_frames: 320, ..counted };
        let late = logged(late, &[s2, Event { kind: EVENT_LATE_READ, ..s3 }]);
        assert_eq!(judge(&late), ["bit-exact and in order: inconclusive"]);
        let t = tie(&late);
        assert!(t.text.contains("judged differently: the log doesn't say whose the gap was, and the verdict depends on it"), "{}", t.text);
        assert!(
            t.candidates[0].ends_with("(stream 12, 320 frames, ring [4789, 5109)): a skip the ring's rules allow"),
            "{:?}",
            t.candidates
        );
        assert!(t.candidates[1].contains("#2 late read (stream 13, 320 frames, ring [4789, 5109)): the driver's record of a loss"));
        // One entry alone: its verdict, no list.
        let one = logged(Stats { secondary_skips: 1, secondary_skipped_frames: 320, ..counted }, &[s2]);
        let t = tie(&one);
        assert_eq!((t.outcome, t.candidates.len()), (Outcome::Ok, 0));
    }

    /// The owner, 2026-11-24: a late read or a jump in the skip log that no gap of the recording names (another stream's, or
    /// frames outside the recording) is diagnostic only: listed, and nothing fails on it. A gap tied to one fails.
    #[test]
    fn verify_lists_a_loss_no_gap_names_and_passes() {
        let before = reading(counters(3, 1_000_000, 0));
        let late = entry(1, EVENT_LATE_READ, 9, 110, (300_209, 300_264));
        let jump = entry(2, EVENT_SECONDARY_JUMP, 10, 192, (310_000, 310_192));
        // A late read with no ring frames (the reader underran there too: silence, nothing lost) and one still pending.
        let silent = Event { flags: 0, ring_from: 0, ring_to: 0, ..entry(3, EVENT_LATE_READ, 9, 96, (0, 0)) };
        let pending = Event { flags: EVENTF_PENDING, ring_from: 0, ring_to: 0, ..entry(4, EVENT_LATE_READ, 11, 48, (0, 0)) };
        let counted = Stats {
            secondary_underrun_frames: 110 + 96 + 48,
            secondary_jumps: 1,
            secondary_jump_frames: 192,
            ..reading(counters(3, 1_000_000 + FRAMES, 0))
        };
        let after = logged(counted, &[late, jump, silent, pending]);
        let window = LogWindow::of(&before, &after);
        // A clean recording: every check passes, and all four are listed as no gap's.
        let r = clean_recording();
        assert_eq!(verify_failures(&before, &after, &r), Vec::<&str>::new());
        let ties = tie_gaps(&before, &r, 0, &window);
        assert_eq!(unnamed_losses(&window, &ties).iter().map(|e| e.sequence).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // A recording with another stream's allowed skip as its gap: it passes, and the losses are still no gap's.
        let skip = entry(5, EVENT_HANDOFF_SKIP, 8, 449, (1_000, 1_449));
        let handed = logged(Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..after }, &[skip]);
        let r = gapped(&[(1_000, 1_449)]);
        assert_eq!(verify_failures(&before, &handed, &r), Vec::<&str>::new());
        let handed_window = LogWindow::of(&before, &handed);
        let ties = tie_gaps(&before, &r, 0, &handed_window);
        assert_eq!(unnamed_losses(&handed_window, &ties).iter().map(|e| e.sequence).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // The late read's frames as the recording's gap: a demonstrated loss, it fails, and the entry is the gap's.
        let r = gapped(&[(300_209, 300_264)]);
        assert_eq!(verify_failures(&before, &after, &r), ["bit-exact and in order"]);
        let ties = tie_gaps(&before, &r, 0, &window);
        assert_eq!(unnamed_losses(&window, &ties).iter().map(|e| e.sequence).collect::<Vec<_>>(), [2, 3, 4]);
        // A gap no entry ties. The pending late read is no gap's record (still being filled in just after the recording):
        // the gap fails as no entry's, and the late read is still no gap's.
        let r = gapped(&[(400_000, 400_048)]);
        let ties = tie_gaps(&before, &r, 0, &window);
        assert_eq!(ties[0].outcome, Outcome::Fail, "{}", ties[0].text);
        assert!(ties[0].text.ends_with("no entry in the skip log for it") && ties[0].candidates.is_empty(), "{:?}", ties[0]);
        assert_eq!(unnamed_losses(&window, &ties).iter().map(|e| e.sequence).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // The same late read untracked (the driver's claims ran out, so it has no ring frames and never will): it may hold the
        // gap's frames, but only as a loss, so the gap fails either way, and the entry is named by it, not listed as no gap's.
        let untracked = logged(after, &[Event { flags: EVENTF_UNTRACKED, ..pending }]);
        let window = LogWindow::of(&before, &untracked);
        let ties = tie_gaps(&before, &r, 0, &window);
        assert_eq!(ties[0].outcome, Outcome::Fail, "{}", ties[0].text);
        assert!(
            ties[0].text.ends_with(
                "no entry in the skip log starts at its first frame; an entry whose ring frames may be short could hold its frames \
                 only as the driver's record of a loss (listed below): it fails either way"
            ),
            "{}",
            ties[0].text
        );
        assert!(ties[0].candidates[0].contains("#4 late read (stream 11, 48 frames, no ring frames, untracked) may hold its frames"));
        assert_eq!(unnamed_losses(&window, &ties).iter().map(|e| e.sequence).collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// The owner, 2026-11-24: INCONCLUSIVE only when the ambiguity changes the verdict, a gap a chain ties exactly as well as
    /// one none does (the review of 2853a23). An untracked entry (its ring frames may be short: the driver's records ran
    /// out) whose logged ring frames lie within a tied gap's, or that has none, may be the gap's record instead of the tie:
    /// listed with it, and the gap inconclusive where it would judge it otherwise (a late read beside an allowed skip; an
    /// allowed skip beside a late read; an allowed skip with a loss of its stream's in the gap's frames, which would be part
    /// of the same record). The same verdict either way stands. One still being filled in just after the recording is no
    /// gap's record (LogWindow::settling): the tie stands, the entry listed as no candidate. One whose logged frames reach
    /// outside the gap's can't be its record, nor part of it.
    #[test]
    fn verify_weighs_an_untracked_entry_against_a_tie() {
        let before = reading(counters(3, 1_000_000, 0));
        let skip = entry(1, EVENT_SECONDARY_SKIP, 12, 320, (4_789, 5_109));
        let counted = Stats { secondary_skips: 1, secondary_skipped_frames: 320, ..reading(counters(3, 1_000_000 + FRAMES, 0)) };
        let r = gapped(&[(4_789, 5_109)]);
        let tie = |events: &[Event]| tie_gaps(&before, &r, 0, &LogWindow::of(&before, &logged(counted, events))).remove(0);
        let judge = |events: &[Event]| failed(&verify_checks(&before, &logged(counted, events), &r, &StreamSeen::default()));
        let late = |flags: u16, ring: (u64, u64)| Event {
            flags,
            ring_from: ring.0,
            ring_to: ring.1,
            ..entry(2, EVENT_LATE_READ, 13, 320, (0, 0))
        };
        assert_eq!(judge(&[skip]), Vec::<&str>::new(), "the tie alone");
        // An untracked late read of another stream's, with no ring frames: it may be the gap's record, and would fail it.
        let no_span = late(EVENTF_UNTRACKED, (0, 0));
        assert_eq!(judge(&[skip, no_span]), ["bit-exact and in order: inconclusive"]);
        let t = tie(&[skip, no_span]);
        assert_eq!(t.outcome, Outcome::Inconclusive);
        assert!(
            t.text.ends_with(
                "ring [4789, 5109)), and an entry whose ring frames may be short that may hold them (listed below): that entry may \
                 be its record instead, and would fail it: the log doesn't say which, and the verdict depends on it: inconclusive"
            ),
            "{}",
            t.text
        );
        assert_eq!(t.candidates, [
            "candidate: the skip log's #1 skip to a shorter lag (stream 12, 320 frames, ring [4789, 5109)): a skip the ring's rules \
             allow",
            "may be a candidate: #2 late read (stream 13, 320 frames, no ring frames, untracked) may hold its frames (its ring \
             frames may be short, or it has none): as its record it would fail it",
        ]);
        assert_eq!(t.entries, [1, 2], "the late read is named by the gap");
        // Its logged ring frames within the gap's: the same. Reaching outside them: it can't be the gap's record.
        assert_eq!(judge(&[skip, late(EVENTF_RING | EVENTF_UNTRACKED, (4_900, 5_109))]), ["bit-exact and in order: inconclusive"]);
        assert_eq!(judge(&[skip, late(EVENTF_RING | EVENTF_UNTRACKED, (5_000, 5_200))]), Vec::<&str>::new(), "past its end");
        assert_eq!(judge(&[skip, late(EVENTF_RING | EVENTF_UNTRACKED, (5_109, 5_200))]), Vec::<&str>::new(), "just after it");
        // Still being filled in just after the recording: no gap's record. The tie stands, the entry listed as no candidate.
        let pending = late(EVENTF_RING | EVENTF_PENDING, (4_900, 5_000));
        assert_eq!(judge(&[skip, pending]), Vec::<&str>::new(), "pending");
        let t = tie(&[skip, pending]);
        assert_eq!((t.outcome, t.entries.clone()), (Outcome::Ok, vec![1]));
        assert!(t.candidates[0].starts_with("not a candidate: #2 late read"), "{:?}", t.candidates);
        // Pending with no ring frames yet: nothing to list.
        let t = tie(&[skip, late(EVENTF_PENDING, (0, 0))]);
        assert_eq!((t.outcome, t.candidates.len()), (Outcome::Ok, 0));
        // An untracked allowed skip of another stream's: the same verdict either way, listed, and the tie stands.
        let other = Event { flags: EVENTF_UNTRACKED, ring_from: 0, ring_to: 0, ..entry(2, EVENT_SECONDARY_SKIP, 14, 300, (0, 0)) };
        let both = Stats { secondary_skips: 2, secondary_skipped_frames: 620, ..counted };
        let after = logged(both, &[skip, other]);
        assert_eq!(failed(&verify_checks(&before, &after, &r, &StreamSeen::default())), Vec::<&str>::new());
        let t = tie_gaps(&before, &r, 0, &LogWindow::of(&before, &after)).remove(0);
        assert_eq!((t.outcome, t.candidates.len()), (Outcome::Ok, 2));
        assert!(t.text.contains("every one a skip the ring's rules allow"), "{}", t.text);
        // ... but with a late read of its stream's within the gap's frames, the two may be the gap's record, and would fail it.
        let its_late = Event { stream: 14, ..late(EVENTF_RING, (5_000, 5_109)) };
        let after = logged(both, &[skip, other, Event { sequence: 3, ..its_late }]);
        let t = tie_gaps(&before, &r, 0, &LogWindow::of(&before, &after)).remove(0);
        assert_eq!(t.outcome, Outcome::Inconclusive, "{}", t.text);
        assert!(t.candidates[1].ends_with(
            "as its record it would pass it, or fail it with the entry of a loss (or of frames no stream got) within its frames"
        ));
        // A tied late read, and an untracked allowed skip that may hold its frames: it may pass, so inconclusive too.
        let tied_late = Event { kind: EVENT_LATE_READ, ..skip };
        let after = logged(Stats { secondary_skips: 1, secondary_skipped_frames: 300, ..counted }, &[tied_late, other]);
        assert_eq!(failed(&verify_checks(&before, &after, &r, &StreamSeen::default())), ["bit-exact and in order: inconclusive"]);
        let t = tie_gaps(&before, &r, 0, &LogWindow::of(&before, &after)).remove(0);
        assert!(t.text.contains("that entry may be its record instead, and would pass it"), "{}", t.text);
        // Without it, the tie to a late read fails.
        let after = logged(Stats { secondary_skips: 0, secondary_skipped_frames: 0, ..counted }, &[tied_late]);
        assert_eq!(failed(&verify_checks(&before, &after, &r, &StreamSeen::default())), ["bit-exact and in order"]);
    }

    /// INCONCLUSIVE only where the skip log can't say: more events over the recording than it keeps, so some of the window's
    /// are in neither copy, or a slot unreadable. A gap whose entry is still there is tied all the same; with every event read,
    /// a gap with no entry fails. A chain that falls short at a loss is inconclusive too while events are missing (one may be
    /// another stream's allowed skip for exactly the gap's frames, its record), and fails with all read. An entry pending in
    /// before's copy alone (after's no longer holds it) may have grown since: a chain short at it is inconclusive; the same
    /// entry pending in after's copy is no gap's record (LogWindow::settling), and the gap fails.
    #[test]
    fn verify_is_inconclusive_only_where_the_log_cant_say() {
        let skip = |n: u64| entry(n, EVENT_SECONDARY_SKIP, 9, 10, (n * 1_000, n * 1_000 + 10));
        let skips = |n: u64, frames_copied| {
            let s = Stats { secondary_skips: n, secondary_skipped_frames: 10 * n, ..reading(counters(3, frames_copied, 0)) };
            logged(s, &(1..=n).map(skip).collect::<Vec<_>>())
        };
        let before = skips(10, 1_000_000);
        // The after copies hold what the driver's slots would: every event so far, the latest 64 kept.
        let wrapped = skips(80, 1_000_000 + FRAMES);
        let seen = StreamSeen::default();
        let judge = |after: &Stats, r: &CheckReport| failed(&verify_checks(&before, after, r, &seen));
        let unsure = "bit-exact and in order: inconclusive";
        let unsure_counts = "the skip log against the counters: inconclusive";
        // 70 events, 64 kept: events 11 to 16 are in neither copy.
        assert_eq!(LogWindow::of(&before, &wrapped).lost, 6);
        assert_eq!(judge(&wrapped, &gapped(&[(20_000, 20_010)])), [unsure_counts], "its entry is still there");
        assert_eq!(judge(&wrapped, &gapped(&[(12_000, 12_010)])), [unsure, unsure_counts]);
        assert_eq!(judge(&wrapped, &gapped(&[(5_000, 5_010)])), [unsure_counts], "before's copy has it");
        // A chain that falls short where the rest may be among the events no longer in the log: inconclusive too.
        assert_eq!(judge(&wrapped, &gapped(&[(20_000, 20_020)])), [unsure, unsure_counts]);
        // 60 events: every one read. A gap with no entry fails, as does one an entry starts but doesn't end.
        let whole = skips(70, 1_000_000 + FRAMES);
        assert_eq!(LogWindow::of(&before, &whole).lost, 0);
        assert_eq!(judge(&whole, &gapped(&[(3_500, 3_510)])), ["bit-exact and in order"]);
        assert_eq!(judge(&whole, &gapped(&[(20_000, 20_020)])), ["bit-exact and in order"]);
        // A slot holding another number than its own can't be read: the same.
        let mut torn = whole;
        torn.skip_log.0[20].sequence = 999;
        assert_eq!(judge(&torn, &gapped(&[(3_500, 3_510)])), [unsure, unsure_counts]);
        // A chain short at a late read: inconclusive while events are missing, failed with every one read.
        let lossy = |mut s: Stats| {
            s.skip_log.0[29] = Event { kind: EVENT_LATE_READ, ..skip(30) };
            s
        };
        let gap = gapped(&[(30_000, 30_020)]);
        assert_eq!(judge(&lossy(wrapped), &gap), [unsure, unsure_counts]);
        let [t] = &tie_gaps(&before, &gap, 0, &LogWindow::of(&before, &lossy(wrapped)))[..] else { panic!() };
        assert!(t.text.ends_with("and one of them may be an allowed skip that is its record: inconclusive"), "{}", t.text);
        // (Its counters as the late read's: one skip fewer.)
        let all_read = Stats { secondary_skips: 69, secondary_skipped_frames: 690, ..lossy(whole) };
        assert_eq!(judge(&all_read, &gap), ["bit-exact and in order"]);
        // An entry pending in before's copy alone, after's having wrapped past it with every event read (64 of them).
        let pending_at = |n: u64, s: Stats| {
            let mut s = s;
            s.skip_log.0[(n as usize - 1) % SKIP_LOG_ENTRIES].flags |= EVENTF_PENDING;
            s
        };
        let early = pending_at(5, skips(5, 1_000_000));
        let past = skips(69, 1_000_000 + FRAMES);
        let window = LogWindow::of(&early, &past);
        assert_eq!((window.lost, window.stale.clone()), (0, vec![5]));
        let gap = gapped(&[(5_000, 5_020)]);
        assert_eq!(failed(&verify_checks(&early, &past, &gap, &seen)), [unsure], "it may have grown");
        let [t] = &tie_gaps(&early, &gap, 0, &window)[..] else { panic!() };
        assert!(t.text.contains("may hold the rest of its frames, and would pass it"), "{}", t.text);
        // The same entry pending in after's copy: no gap's record, so no chain, and the gap fails.
        let (early, late) = (skips(4, 1_000_000), pending_at(5, skips(10, 1_000_000 + FRAMES)));
        assert_eq!(failed(&verify_checks(&early, &late, &gap, &seen)), ["bit-exact and in order"]);
        let [t] = &tie_gaps(&early, &gap, 0, &LogWindow::of(&early, &late))[..] else { panic!() };
        assert!(t.text.ends_with("no entry in the skip log for it") && t.candidates[0].starts_with("not a candidate: #5"), "{t:?}");
    }

    /// An entry the driver marked UNTRACKED (its records of the copy, or of the spans being filled in, ran out) may have a ring
    /// span short at its start, or none: no entry then starts at the gap's first frame, but the log can't rule that one out.
    /// Inconclusive, not failed, where it would pass the gap (an allowed skip); without the flag, the same gap fails. An
    /// untracked late read fails it either way (the owner, 2026-11-24: INCONCLUSIVE only when the ambiguity changes the
    /// verdict).
    #[test]
    fn verify_is_inconclusive_for_a_gap_an_untracked_entry_may_hold() {
        let before = reading(counters(3, 1_000_000, 0));
        let counted = Stats { secondary_skips: 1, secondary_skipped_frames: 320, ..reading(counters(3, 1_000_000 + FRAMES, 0)) };
        let skip = entry(1, EVENT_SECONDARY_SKIP, 9, 320, (300_040, 300_320));
        let judge =
            |e: Event| failed(&verify_checks(&before, &logged(counted, &[e]), &gapped(&[(300_000, 300_320)]), &StreamSeen::default()));
        assert_eq!(judge(skip), ["bit-exact and in order"], "an entry for other frames");
        let untracked = Event { flags: EVENTF_RING | EVENTF_UNTRACKED, ..skip };
        assert_eq!(judge(untracked), ["bit-exact and in order: inconclusive"], "its start may be short");
        let no_span = Event { flags: EVENTF_UNTRACKED, ring_from: 0, ring_to: 0, ..skip };
        assert_eq!(judge(no_span), ["bit-exact and in order: inconclusive"], "no ring span at all");
        // One far from the gap's frames holds none of them.
        let far = Event { ring_from: 900_000, ring_to: 900_320, ..untracked };
        assert_eq!(judge(far), ["bit-exact and in order"]);
        // An untracked entry that ties it exactly is judged as any other.
        let exact = Event { ring_from: 300_000, ..untracked };
        assert_eq!(judge(exact), Vec::<&str>::new());
        // The same untracked entry a late read: whatever frames it holds, it is a loss, so the gap fails either way.
        let no_skip = Stats { secondary_skips: 0, secondary_skipped_frames: 0, ..counted };
        let late = |e: Event| logged(no_skip, &[Event { kind: EVENT_LATE_READ, ..e }]);
        let gap = gapped(&[(300_000, 300_320)]);
        for e in [untracked, no_span] {
            assert_eq!(failed(&verify_checks(&before, &late(e), &gap, &StreamSeen::default())), ["bit-exact and in order"], "{e:?}");
        }
        let ties = tie_gaps(&before, &gap, 0, &LogWindow::of(&before, &late(untracked)));
        assert!(ties[0].text.contains("it fails either way"), "{}", ties[0].text);
    }

    /// Each gap's ring frames come from its steps on from the first code, not its place in the recording: silence past 2^22
    /// frames before a gap (a long soak's counted underruns) moves its index that far from its frames, and it still ties.
    #[test]
    fn a_gap_after_long_silence_still_names_its_frames() {
        // A long soak: 50,000,000 frames of signal, and 5,000,000 of the reader's underrun silence before the gap.
        let (signal, silence) = (50_000_000, 5_000_000);
        let before = reading(counters(3, 1_000_000, 0));
        let skip = entry(1, EVENT_HANDOFF_SKIP, 8, 449, (30_000_000, 30_000_449));
        let ring = counters(3, 1_000_000 + signal + silence, silence);
        let after = logged(Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..reading(ring) }, &[skip]);
        let gap =
            Gap { index: 30_000_000 + silence, offset: 30_000_000, due: 30_000_000 % (1 << 23), got: 30_000_449 % (1 << 23), silence: 0 };
        let r = CheckReport {
            frames: signal + silence,
            signal_frames: signal,
            zeros_inside: silence,
            zero_runs_inside: 1,
            gaps: 1,
            gap_frames: 449,
            gap_list: vec![gap],
            ..clean_recording()
        };
        assert_eq!(failed(&verify_checks(&before, &after, &r, &StreamSeen::default())), Vec::<&str>::new());
        // Its index alone, the silence taken as steps, would name frames 5,000,000 on: no entry there.
        let drifted = CheckReport { gap_list: vec![Gap { offset: gap.index, ..gap }], ..r.clone() };
        assert_eq!(failed(&verify_checks(&before, &after, &drifted, &StreamSeen::default())), ["bit-exact and in order"]);
    }

    /// The codes name the ring's frames: nearest the reader's position, with the feed's --start, through the codes' wrap and
    /// past 2^23 frames.
    #[test]
    fn codes_name_ring_frames() {
        let m = signal::CODE_MODULUS;
        assert_eq!(ring_frame(5, 0, 3 * m), 3 * m + 5);
        assert_eq!(ring_frame(5, 0, 3 * m + 10), 3 * m + 5);
        assert_eq!(ring_frame((m - 3) as u32, 0, 3 * m + 10), 3 * m - 3);
        assert_eq!(ring_frame(0, m - 100, 1_200), 100, "the code 0 is ring frame 100 from --start 2^23 - 100");
        assert_eq!(ring_frame(1_000, 0, 0), 1_000);
        // A recording from a feed started at 2^23 - 100: its codes wrap, its gap still ties.
        let start = m - 100;
        let before = Stats { attached_read_frame: 1_200, ..reading(counters(3, 1_000_000, 0)) };
        let skip = entry(1, EVENT_HANDOFF_SKIP, 8, 449, (1_000, 1_449));
        let after =
            logged(Stats { reader_handoffs: 1, handoff_skipped_frames: 449, ..reading(counters(3, 1_000_000 + FRAMES, 0)) }, &[skip]);
        let code = |ring: u64| ((ring + start) % m) as u32;
        let r = CheckReport {
            gap_list: vec![Gap { index: 1_000, offset: 1_000, due: code(1_000), got: code(1_449), silence: 0 }],
            first_code: Some(code(0)),
            ..gapped(&[(1_000, 1_449)])
        };
        let with_start = StreamSeen { start, ..Default::default() };
        assert_eq!(failed(&verify_checks(&before, &after, &r, &with_start)), Vec::<&str>::new());
        assert_eq!(failed(&verify_checks(&before, &after, &r, &StreamSeen::default())), ["bit-exact and in order"], "without --start");
    }

    /// The last recorder running (three.sh's A2-default, 2026-11-24): the reader stopped as the recording ended, so no stream
    /// held the slot when verify looked. It waits for its own stream's next pass, and passes once one holds the slot.
    #[test]
    fn verify_waits_out_a_vacant_reader_slot() {
        let before = reading(counters(3, 1_000_000, 0));
        let after = reading(counters(3, 1_000_000 + FRAMES, 0));
        let vacant = Stats { flags: STATS_ATTACHED | STATS_LOOPBACK, reader_packet_frames: 0, ..after };
        assert!(slot_vacant(&before, &vacant) && !slot_vacant(&before, &after));
        assert!(!slot_vacant(&before, &Stats { ring: counters(4, 0, 0), ..vacant }), "another attachment");
        assert!(!slot_vacant(&before, &Stats { flags: STATS_LOOPBACK, ..vacant }), "none attached");
        assert!(!slot_vacant(&before, &Stats { flags: STATS_ATTACHED | STATS_FORMAT_MISMATCH, ..vacant }), "a stream in another format");
        let waited = StreamSeen { vacancy: Some(Duration::from_millis(4)), ..Default::default() };
        let c = verify_checks(&before, &after, &clean_recording(), &waited);
        assert_eq!(failed(&c), Vec::<&str>::new());
        let detail = &c.iter().find(|c| c.name == "just after it").unwrap().detail;
        assert!(detail.contains("the last reader having stopped: one did 4.0 ms later"), "{detail}");
        // Still vacant after the wait: it fails.
        let c = verify_checks(&before, &vacant, &clean_recording(), &StreamSeen { vacancy: Some(VACANCY_WAIT), ..Default::default() });
        assert_eq!(failed(&c), ["just after it"]);
    }

    /// The owner, 2026-11-24 (agreeing Astra's fix): the capture stream's own losses are taken as the measured recording ends.
    /// What the packets after it carried (the drain for the final STATS read and the wait for the reader slot) is reported as
    /// information and fails nothing; a loss up to its end fails the check: in the recording's packets, or in those before it
    /// (the wait for the signal, the drain for the STATS read just before), judged with it as before the fix (ours: the fix
    /// was agreed for the end only) and told apart in the detail.
    #[test]
    fn the_capture_streams_losses_after_the_recording_are_information() {
        let at = |frames, discontinuities| Losses { frames, discontinuities };
        let before = reading(counters(3, 1_000_000, 0));
        let after = reading(counters(3, 1_000_000 + FRAMES, 0));
        let judge = |losses: StreamLosses| {
            failed(&verify_checks(&before, &after, &clean_recording(), &StreamSeen { losses, ..Default::default() }))
        };
        let detail = |losses: StreamLosses| capture_stream_check(&losses).detail;
        // The stream's running counts: none as the recording starts or ends, 104 frames and a discontinuity by the last read
        // (the engine dropped them in the drain for the final STATS read).
        let after_it = StreamLosses::of(at(0, 0), at(0, 0), at(104, 1));
        assert_eq!(after_it, StreamLosses { earlier: at(0, 0), recording: at(0, 0), later: at(104, 1) });
        assert_eq!(judge(after_it), Vec::<&str>::new(), "a clean recording passes");
        assert_eq!(
            detail(after_it),
            "0 frames lost by the engine, 0 discontinuities after its first packet, up to the recording's end; after it, \
             information only, not its: 104 frames lost and 1 discontinuities (the drain for the STATS read just after, and any \
             wait for the reader slot)"
        );
        // Before it (the wait for the signal) only: judged with it, as before the fix, and told apart.
        let before_it = StreamLosses::of(at(96, 1), at(96, 1), at(96, 1));
        assert_eq!(before_it, StreamLosses { earlier: at(96, 1), ..Default::default() });
        assert_eq!(judge(before_it), ["the capture stream"]);
        assert_eq!(
            detail(before_it),
            "96 frames lost by the engine, 1 discontinuities after its first packet, up to the recording's end (96 and 1 of them \
             in the packets before it: the wait for the signal and the reader, and the drain for the STATS read just before)"
        );
        // In the recording's packets: it fails, with what came before told apart and what came after reported.
        let inside = StreamLosses::of(at(48, 0), at(48 + 104, 1), at(48 + 104 + 10, 1));
        assert_eq!(inside, StreamLosses { earlier: at(48, 0), recording: at(104, 1), later: at(10, 0) });
        assert_eq!((inside.judged(), judge(inside)), (at(152, 1), vec!["the capture stream".to_string()]));
        let d = detail(inside);
        assert!(d.starts_with("152 frames lost by the engine, 1 discontinuities after its first packet, up to the recording's end"), "{d}");
        assert!(
            d.contains("(48 and 0 of them in the packets before it") && d.contains("not its: 10 frames lost and 0 discontinuities"),
            "{d}"
        );
        // Nothing lost anywhere: no note.
        assert_eq!(
            detail(StreamLosses::default()),
            "0 frames lost by the engine, 0 discontinuities after its first packet, up to the recording's end"
        );
    }

    /// The device's time-line jump counters (the owner, 2026-11-24: two totals, appended to STATS) against its skip log: the
    /// SECONDARY_JUMP entries over the recording are SecondaryJumps' rise, their frames SecondaryJumpFrames'. Jumps no gap of the
    /// recording names fail nothing (diagnostic only); a log and counters that disagree fail the cross-check.
    #[test]
    fn the_skip_logs_jumps_add_up_to_the_counters() {
        let before = Stats { secondary_jumps: 5, secondary_jump_frames: 1_000, ..reading(counters(3, 1_000_000, 0)) };
        let after = |jumps: u64, frames: u64| Stats {
            secondary_jumps: 5 + jumps,
            secondary_jump_frames: 1_000 + frames,
            ..reading(counters(3, 1_000_000 + FRAMES, 0))
        };
        let jump = |n: u64, frames: u32, lo: u64| entry(n, EVENT_SECONDARY_JUMP, 9, frames, (lo, lo + u64::from(frames)));
        let both = [jump(1, 192, 700_000), jump(2, 96, 800_000)];
        let r = clean_recording();
        let logged_after = |jumps, frames| logged(after(jumps, frames), &both);
        assert_eq!(verify_failures(&before, &logged_after(2, 288), &r), Vec::<&str>::new());
        let check = log_counts_check(&before, &logged_after(2, 288), &LogWindow::of(&before, &logged_after(2, 288)));
        assert!(check.detail.contains("time-line jumps 2 logged, +2 counted (288 and 288 frames)"), "{}", check.detail);
        let cross = ["the skip log against the counters"];
        assert_eq!(verify_failures(&before, &logged_after(1, 192), &r), cross, "a jump logged, not counted");
        assert_eq!(verify_failures(&before, &logged_after(3, 288), &r), cross, "a jump counted, not logged");
        assert_eq!(verify_failures(&before, &logged_after(2, 289), &r), cross, "a frame off");
        assert_eq!(verify_failures(&before, &after(2, 288), &r), cross, "counted, the log empty");
    }

    /// The skip log as read from its slots: oldest first, the latest it keeps, and a slot not holding its own number
    /// unreadable.
    #[test]
    fn event_logs_read_from_their_slots() {
        let jump = |n: u64| entry(n, EVENT_SECONDARY_JUMP, 9, 10, (n, n + 1));
        let s = logged(Stats::default(), &(1..=70).map(jump).collect::<Vec<_>>());
        let log = s.skip_events();
        assert_eq!((log.logged, log.unreadable, log.entries.len()), (70, 0, SKIP_LOG_ENTRIES));
        assert_eq!((log.entries[0].sequence, log.entries[63].sequence), (7, 70));
        let mut torn = s;
        torn.skip_log.0[3].sequence = 4;
        assert_eq!(torn.skip_events().unreadable, 1, "slot 3 holds 68, not 4");
        let empty = Stats::default().skip_events();
        assert_eq!((empty.logged, empty.unreadable, empty.entries.len()), (0, 0, 0));
        let text = event_text(&Event { qpc: 7_000_000, flags: EVENTF_RING | EVENTF_PENDING, ..jump(5) }, 10_000_000, 10_000_000);
        assert_eq!(
            text,
            "#5 -0.300 s time-line jump: stream 9 (240-frame packets; pending), 10 frames, span 0 + 0, ring [5, 6) (1 frames), attachment 3"
        );
    }

    #[test]
    fn hresults_have_names() {
        use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_IN_USE;
        assert_eq!(hresult::name(AUDCLNT_E_UNSUPPORTED_FORMAT), "AUDCLNT_E_UNSUPPORTED_FORMAT (0x88890008)");
        assert_eq!(hresult::name(AUDCLNT_E_DEVICE_IN_USE), "AUDCLNT_E_DEVICE_IN_USE (0x8889000A)");
        assert_eq!(hresult::name(HRESULT(0)), "S_OK (0x00000000)");
        assert_eq!(hresult::name(HRESULT(1)), "S_FALSE (0x00000001)");
        // A driver's STATUS_NOT_SUPPORTED as the I/O manager's Win32 error, or as the NTSTATUS itself.
        assert_eq!(hresult::name(HRESULT(0x8007_0032_u32 as i32)), "HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED) (0x80070032)");
        assert_eq!(hresult::name(HRESULT(0xD000_00BB_u32 as i32)), "HRESULT_FROM_NT(STATUS_NOT_SUPPORTED) (0xD00000BB)");
        // The fallback tables: a Win32 error and an NTSTATUS that NAMES doesn't hold.
        assert_eq!(hresult::name(HRESULT(0x8007_048F_u32 as i32)), "HRESULT_FROM_WIN32(ERROR_DEVICE_NOT_CONNECTED) (0x8007048F)");
        assert_eq!(hresult::name(HRESULT(0x9000_0011_u32 as i32)), "HRESULT_FROM_NT(STATUS_DEVICE_BUSY) (0x90000011)");
        assert_eq!(hresult::name(E_FAIL), "E_FAIL (0x80004005)");
        // Anything else: its value, and the system's text where it has one.
        assert!(hresult::name(HRESULT(0x8899_1234_u32 as i32)).starts_with("0x88991234"));
        // Every name is the constant's own, once.
        for (i, (hr, name)) in hresult::NAMES.iter().enumerate() {
            assert!(hresult::NAMES[i + 1..].iter().all(|(h, _)| h != hr), "{name} twice");
        }
        let r = Refusal { call: "Initialize", hr: AUDCLNT_E_UNSUPPORTED_FORMAT, text: None };
        assert_eq!(r.to_string(), "Initialize returned AUDCLNT_E_UNSUPPORTED_FORMAT (0x88890008)");
        let r = Refusal::at("Initialize", WasapiError::UnsupportedFormat);
        assert!(r.to_string().starts_with("Initialize returned AUDCLNT_E_UNSUPPORTED_FORMAT (0x88890008) ("), "{r}");
    }

    #[test]
    fn the_formats_are_eight_bytes_a_frame() {
        let f = Format::Float.wave();
        assert_eq!((f.get_samplespersec(), f.get_nchannels(), f.get_bitspersample(), f.get_validbitspersample()), (48_000, 2, 32, 32));
        assert!(matches!(f.get_subformat(), Ok(SampleType::Float)));
        let p = Format::Pcm24.wave();
        assert_eq!((p.get_bitspersample(), p.get_validbitspersample(), p.get_dwchannelmask()), (32, 24, 0b11));
        assert!(matches!(p.get_subformat(), Ok(SampleType::Int)));
        for w in [f, p] {
            assert_eq!(w.get_blockalign(), protocol::FRAME_BYTES);
        }
        assert_eq!(describe_format(&Format::Pcm24.wave()), "48000 Hz, 2 ch, 24-in-32 PCM");
        assert_eq!(describe_format(&Format::Float.wave()), "48000 Hz, 2 ch, 32-bit float");
        assert_eq!((Format::parse("float"), Format::parse("pcm24")), (Ok(Format::Float), Ok(Format::Pcm24)));
        assert!(Format::parse("pcm16").is_err());
    }

    #[test]
    fn open_needs_a_device_a_share_mode_and_a_format() {
        let open = |list: &[&str]| open_args(&args(list));
        let a = open(&["--device", "Mic (X)", "--exclusive", "--format", "pcm24"]).unwrap();
        let spec = StreamSpec { exclusive: true, format: Format::Pcm24, period_frames: None, mode: Mode::Default };
        assert_eq!(a, OpenArgs { name: "Mic (X)".into(), render: false, spec, seconds: 10, signal: None });
        let a = open(&[
            "--render", "--shared", "--device", "Speakers", "--format", "float", "--period-frames", "128", "--mode", "raw", "--seconds",
            "3",
        ])
        .unwrap();
        let spec = StreamSpec { exclusive: false, format: Format::Float, period_frames: Some(128), mode: Mode::Raw };
        assert_eq!(a, OpenArgs { name: "Speakers".into(), render: true, spec, seconds: 3, signal: None });
        // The test signal, from frame 0 or from --start.
        let signal = |extra: &[&str]| {
            let list = [&["--render", "--shared", "--device", "S", "--format", "float"][..], extra].concat();
            open(&list).map(|a| a.signal)
        };
        assert_eq!(signal(&["--signal"]), Ok(Some(0)));
        assert_eq!(signal(&["--signal", "--start", "2097152"]), Ok(Some(2_097_152)));
        for (list, why) in [
            (&["--exclusive", "--format", "float"][..], "--device NAME is required"),
            (&["--device", "M", "--format", "float"], "one of --exclusive and --shared"),
            (&["--device", "M", "--exclusive", "--shared", "--format", "float"], "one of --exclusive and --shared"),
            (&["--device", "M", "--exclusive"], "--format float|pcm24 is required"),
            (&["--device", "M", "--exclusive", "--format", "int16"], "float or pcm24"),
            (&["--device", "M", "--exclusive", "--format", "float", "--mode", "comms"], "raw, default or communications"),
            (&["--device", "M", "--exclusive", "--format", "float", "--period-frames", "0"], "--period-frames 0"),
            (&["--device", "M", "--exclusive", "--format", "float", "--period-frames", "48001"], "--period-frames 48001"),
            (&["--device", "M", "--exclusive", "--format", "float", "--seconds", "0"], "--seconds 0"),
            (&["--device", "M", "--shared", "--format", "float", "--signal"], "--signal needs --render and --format float"),
            (&["--render", "--device", "M", "--shared", "--format", "pcm24", "--signal"], "--signal needs --render"),
            (&["--render", "--device", "M", "--shared", "--format", "float", "--start", "5"], "--start is --signal's"),
            (&["--render", "--device", "M", "--shared", "--format", "float", "--signal", "--start", "x"], "--start"),
        ] {
            let e = open(list).unwrap_err();
            assert!(e.contains(why), "{list:?}: {e}");
        }
    }

    #[test]
    fn modes_default_per_command() {
        let a = args(&[]);
        let o = Opts::parse(&a, &["--mode", "--period-frames"], &[]).unwrap();
        assert_eq!((o.mode(Mode::RawWhereOffered), o.period_frames()), (Ok(Mode::RawWhereOffered), Ok(None)));
        let a = args(&["--mode", "default", "--period-frames", "160"]);
        let o = Opts::parse(&a, &["--mode", "--period-frames"], &[]).unwrap();
        assert_eq!((o.mode(Mode::RawWhereOffered), o.period_frames()), (Ok(Mode::Default), Ok(Some(160))));
        let a = args(&["--mode", "communications"]);
        let o = Opts::parse(&a, &["--mode", "--period-frames"], &[]).unwrap();
        assert_eq!(o.mode(Mode::RawWhereOffered), Ok(Mode::Communications));
    }

    /// open's render `--signal` writes the frames a float capture decodes (Packet::float_frames, the same bytes back), through
    /// the codes' wrap, and only whole frames.
    #[test]
    fn a_render_stream_writes_the_test_signal() {
        let mut bytes = vec![0xAA_u8; 5 * 8 + 3];
        write_signal(&mut bytes, signal::CODE_MODULUS - 2);
        let packet = Packet { bytes: &bytes, frames: 5, silent: false, discontinuity: false, qpc_hns: None, host_qpc: 0, position: 0 };
        let codes: Vec<_> = packet.float_frames().map(signal::decode).collect();
        let top = signal::CODE_MODULUS as u32;
        assert_eq!(codes, [Some(top - 2), Some(top - 1), Some(0), Some(1), Some(2)]);
        assert_eq!(bytes[40..], [0xAA; 3], "no part of a frame");
        let mut checker = SequenceChecker::new();
        packet.float_frames().for_each(|f| checker.feed(f));
        assert!(checker.is_clean());
    }

    #[test]
    fn pcm_counts_find_silence_and_padding() {
        let mut c = PcmCounts::default();
        // A silent frame; a frame with sound and zero padding (0x7FFFFF00, positive full scale, then -1 as 0xFFFFFF00); a frame
        // whose left sample's padding byte isn't zero.
        let mut bytes = vec![0u8; 8];
        bytes.extend(0x7FFF_FF00_u32.to_le_bytes());
        bytes.extend(0xFFFF_FF00_u32.to_le_bytes());
        bytes.extend(0x0000_0101_u32.to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        c.count(&bytes);
        assert_eq!(c, PcmCounts { silent_frames: 1, sound_frames: 2, nonzero_padding: 1 });
    }

    /// Synthetic logs on a 10 MHz QPC: the pre-fill, then a 480-frame block every 10 ms, and a packet per block received 10 ms
    /// after it, the device stamping the packet's first frame at the block's publication.
    fn latency_logs(blocks_n: u64) -> (Vec<Block>, Vec<Receipt>, Vec<Run>) {
        let mut blocks = vec![Block { qpc: 0, start: 0, end: 480, read_frame: 0 }];
        let (mut receipts, mut runs) = (Vec::new(), Vec::new());
        for i in 0..blocks_n {
            let t = 100_000 * (i + 1);
            if i > 0 {
                blocks.push(Block { qpc: t, start: 480 * i, end: 480 * (i + 1), read_frame: 480 * i });
            }
            runs.push(Run { first: 480 * i, len: 480, offset: 0 });
            receipts.push(Receipt { host_qpc: t + 100_000, device_hns: Some(t), frames: 480, first_run: i as u32, runs: 1 });
        }
        (blocks, receipts, runs)
    }

    #[test]
    fn latency_reports_the_logs() {
        let (blocks, receipts, runs) = latency_logs(100);
        let r = latency_report(&blocks, &receipts, &runs, 10_000_000, 1_000_000, u64::MAX);
        // Receipts from 1,000,000 ticks: packets 8 to 99 (92), every frame received 10 ms after its block; by the device, frame
        // j of a packet is j / 48 kHz after it (0 to 9.98 ms, each frame timed alone).
        assert_eq!((r.measured.packets, r.measured.frames, r.measured.skipped), (92, 92 * 480, 0));
        let host = r.host.unwrap();
        assert_eq!((host.p50, host.p99, host.max, host.count), (100_000, 100_000, 100_000, 92 * 480));
        let device = r.device.unwrap();
        // The median is frame 239's (the 240th of 480 offsets, each 92 times): 239 x 208.33 = 49,791.7.
        assert_eq!((device.min, device.p50, device.max, device.count, r.measured.device_group), (0, 49_792, 99_792, 92 * 480, 1));
        // Blocks 9 to 99 in the window, each topping up from an empty ring.
        let fill = r.fill.unwrap();
        assert_eq!((r.blocks, fill.min, fill.max), (91, 0, 0));
        assert_eq!(r.continuity, Continuity::default());
        let clean = LatencyStream::default();
        let checks = latency_checks(&r, &FrameCounts::default(), [0; 3], &clean);
        assert_eq!(failed(&checks), Vec::<&str>::new(), "{checks:?}");

        // From the start the pre-fill is skipped, not measured.
        let r0 = latency_report(&blocks, &receipts, &runs, 10_000_000, 0, u64::MAX);
        assert_eq!((r0.measured.packets, r0.measured.skipped), (100, 480));

        // A lost run, a corrupt frame, a dropped log entry, the engine's lost frames: each fails its check.
        let mut gap = runs.clone();
        gap[50].first += 10;
        let r = latency_report(&blocks, &receipts, &gap, 10_000_000, 1_000_000, u64::MAX);
        assert_eq!(failed(&latency_checks(&r, &FrameCounts::default(), [0; 3], &clean)), ["every frame once and in order"]);
        let r = latency_report(&blocks, &receipts, &runs, 10_000_000, 1_000_000, u64::MAX);
        let corrupt = FrameCounts { corrupt: 1, ..Default::default() };
        assert_eq!(failed(&latency_checks(&r, &corrupt, [0; 3], &clean)), ["every frame the feed's"]);
        assert_eq!(failed(&latency_checks(&r, &FrameCounts::default(), [0, 1, 0], &clean)), ["the logs held everything"]);
        let lost = LatencyStream { lost_frames: 5, ..Default::default() };
        assert_eq!(failed(&latency_checks(&r, &FrameCounts::default(), [0; 3], &lost)), ["the capture stream"]);
        let secondary = LatencyStream { held_before: Some(480), not_reader_packets: 1_000, ..Default::default() };
        assert_eq!(failed(&latency_checks(&r, &FrameCounts::default(), [0; 3], &secondary)), ["this process's stream reads the ring"]);
        // Nothing received after the window's start.
        let r = latency_report(&blocks, &receipts, &runs, 10_000_000, u64::MAX, u64::MAX);
        assert_eq!(failed(&latency_checks(&r, &FrameCounts::default(), [0; 3], &clean)), ["frames measured"]);
        assert!(r.host.is_none() && r.device.is_none());
    }

    /// Whether latency's stream read the ring: proven by no other stream's passes; taken from a free slot at the start where
    /// there were some; in doubt (a failure) where another stream held the slot and others passed.
    #[test]
    fn latency_knows_whether_its_stream_is_the_reader() {
        let ok = |held_before, not_reader_packets| {
            reader_check(&LatencyStream { held_before, not_reader_packets, ..Default::default() }).outcome == Outcome::Ok
        };
        assert!(ok(None, 0));
        assert!(ok(None, 500), "the slot was free: this stream took it, another passed as a secondary");
        assert!(ok(Some(480), 0), "the same driver stream as the holder (one mode, shared), or the holder stopped first");
        assert!(!ok(Some(480), 500));
        assert!(!ok(Some(0), 1), "a holder in a format the driver can't count in frames");
        // The header's state at the start: held while the holder reads or mismatches, or reports a packet.
        assert_eq!(slot_holder(STATE_ATTACHED, 0), None);
        assert_eq!(slot_holder(STATE_ATTACHED | STATE_READING, 480), Some(480));
        assert_eq!(slot_holder(STATE_ATTACHED | STATE_FORMAT_MISMATCH, 0), Some(0));
        assert_eq!(slot_holder(STATE_ATTACHED, 128), Some(128));
    }

    /// open's refusals: exit 1 only where RefusedCaptureOpens or the loopback's RefusedOpens rose over the open; any other
    /// refusal exits 4.
    #[test]
    fn a_refusal_is_the_drivers_only_where_its_counter_rose() {
        let with = |refused_capture_opens, refused_opens| Stats {
            flags: STATS_LOOPBACK,
            ring: RingStats { refused_capture_opens, ..Default::default() },
            loopback: LoopbackStats { refused_opens, ..Default::default() },
            ..Default::default()
        };
        let (b, rose, same, lb) = (with(7, 3), with(8, 3), with(7, 3), with(7, 4));
        assert_eq!(refuser(false, Some(&b), Some(&rose)), Refuser::Driver(1));
        assert_eq!(refuser(false, Some(&b), Some(&same)), Refuser::NotTheDriver);
        assert_eq!(refuser(false, Some(&b), Some(&lb)), Refuser::Loopback(1));
        assert_eq!(refuser(true, Some(&b), Some(&lb)), Refuser::Loopback(1), "a render stream meets the loopback's rule");
        assert_eq!(refuser(true, Some(&b), Some(&rose)), Refuser::NotTheDriver, "the ring's rule is the capture side's only");
        let no_device = Stats { flags: 0, ..lb };
        assert_eq!(refuser(false, Some(&b), Some(&no_device)), Refuser::NotTheDriver, "no loopback counters without the device");
        assert!(matches!(refuser(false, None, Some(&rose)), Refuser::Unknown(_)));
        assert!(matches!(refuser(false, Some(&b), None), Refuser::Unknown(_)));

        let r = Refusal { call: "Initialize", hr: AUDCLNT_E_UNSUPPORTED_FORMAT, text: None };
        let (v, line) = refusal_outcome(&r, "capture", &Refuser::Driver(1));
        assert_eq!(v, Verdict::Fail);
        assert!(line.starts_with("OUTCOME: REFUSED BY THE DRIVER (RefusedCaptureOpens +1"), "{line}");
        assert!(line.contains("Initialize returned AUDCLNT_E_UNSUPPORTED_FORMAT"), "{line}");
        let in_use = Refusal { call: "Initialize", hr: windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_IN_USE, text: None };
        let (v, line) = refusal_outcome(&in_use, "capture", &Refuser::NotTheDriver);
        assert_eq!(v, Verdict::RefusedElsewhere);
        let expect = "OUTCOME: REFUSED, not by the driver's format rules (RefusedCaptureOpens and the loopback's RefusedOpens +0): \
                      Initialize returned AUDCLNT_E_DEVICE_IN_USE";
        assert!(line.starts_with(expect), "{line}");
        let (v, line) = refusal_outcome(&r, "render", &Refuser::Loopback(1));
        assert_eq!(v, Verdict::Fail);
        assert!(line.starts_with("OUTCOME: REFUSED BY THE DRIVER (the loopback's RefusedOpens +1"), "{line}");
        let start = Refusal { call: "Start", hr: E_FAIL, text: None };
        let (v, line) = refusal_outcome(&start, "render", &Refuser::Unknown("why"));
        assert_eq!(
            (v, line.as_str()),
            (
                Verdict::RefusedElsewhere,
                "OUTCOME: REFUSED, by whom unknown (why): Start returned E_FAIL (0x80004005); the render stream never ran"
            )
        );
    }
}
