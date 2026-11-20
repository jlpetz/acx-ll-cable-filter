//! nf-vmic-test: spike check 4's tool for the driver's app-fed capture ring (TODO 19; driver/README.md, "App feed").
//!
//! - `feed` attaches a ring and writes the test signal, paced by the driver's `read_frame`, until Ctrl+C.
//! - `verify` records "Microphone (AudioCodec Device)" (shared mode, float32 stereo 48 kHz, RAW where offered) while a feed
//!   runs, and passes only with proof that the app ring fed the recording: the driver's statistics just before and just after
//!   it (the same attachment, read throughout, its frames copied), and the signal bit-exact on both channels and in order, with
//!   silence only where the driver counted underruns (one packet's slack at each end of the recording).
//! - `stats` prints the driver's NF_VMIC_IOCTL_STATS: the ring's counters, the loopback's, and each stream kind's timer
//!   lateness histogram (spike check 5).
//! - `kill-test` starts `feed` as a child and terminates it mid-stream K times (a capture stream reading its ring), checking
//!   from STATS that each attachment ended through the cancel path within a bound, and that the capture timer kept running.
//!
//! The spike's control device admits SYSTEM and Administrators only: run it elevated. The owner runs it (CLAUDE.md); no test
//! starts it. Its feed loop prints a status line a second, which formats and allocates: a test tool's liberty, with the ring's
//! 20 ms of fill (the default target) as the margin.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::fmt::Display;
use std::marker::PhantomData;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{io, thread};

use nf_vmic::protocol::{
    self, KIND_CAPTURE, KIND_RENDER, LATENESS_LABELS, RingHeader, STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_READING, STATS_ATTACHED,
    STATS_FORMAT_MISMATCH, STATS_LOOPBACK, STATS_READING, Stats, StreamStats,
};
use nf_vmic::signal::{self, CheckReport, SequenceChecker};
use nf_vmic::{AttachError, Device, RingMemory, Writer, top_up};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioClientProperties, DeviceEnumerator, Direction, Handle, SampleType, StreamMode, StreamOption,
    WaveFormat,
};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, FALSE, HANDLE, TRUE};
use windows::Win32::Media::Audio::ENDPOINT_HARDWARE_SUPPORT_VOLUME;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW,
    SetWaitableTimer, TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{BOOL, Owned, PCWSTR, w};

/// The capture endpoint the driver's ring feeds (spike check 1's name).
const ENDPOINT: &str = "Microphone (AudioCodec Device)";
/// How long verify waits, once its stream runs, for the driver to report a capture stream reading the ring.
const READING_WAIT: Duration = Duration::from_secs(2);
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
      once the reader's packet shows the target is below it. --start: the first frame's number.
  nf-vmic-test verify [--seconds N] [--device NAME]
      Start `feed` first, in another window, and leave it running. Then record the capture endpoint (default \"Microphone
      (AudioCodec Device)\") in shared mode, float32 stereo 48 kHz, RAW where it's offered, for N seconds (default 10), and
      check that the app ring fed it: the driver's statistics just before and after (the same attachment, read throughout,
      its frames copied), and the signal bit-exact on both channels and in order, with silence only where the driver
      underran (one packet's slack at each end). The recording starts once the stream delivers the signal (2 s at most).
      Unmute the endpoint, and close other apps that record it (the first stream to run holds the ring).
  nf-vmic-test stats
      Print the driver's statistics.
  nf-vmic-test kill-test [--kills K] [--min-ms MIN] [--max-ms MAX] [--device NAME] [--no-capture]
      K times (default 10): start `feed` as a child, check that a capture stream reads its ring, terminate the child MIN to
      MAX milliseconds after it attached (default 300 to 2000; MIN at least 200), and check that it exited within 2 s and its
      attachment ended through the cancel path within 1 s of that. This tool's own capture stream reads the endpoint
      throughout, and its timer must still run after the last kill. --no-capture leaves the reading to another capture
      client: without one the result is INCONCLUSIVE.

Exit codes: 0 pass, 1 fail, 2 a usage or set-up error, 3 inconclusive.";

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
        match self.values.iter().find(|(n, _)| *n == name) {
            None => Ok(default),
            Some((_, v)) => v.parse().map_err(|_| format!("{name}: can't read {v:?}")),
        }
    }
}

/// A verdict's checks, each printed with its numbers as it is made.
#[derive(Default)]
struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, ok: bool, name: &str, detail: impl Display) -> bool {
        println!("  {} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
        ok
    }
}

/// One check of a verdict, made by a pure function (so the tests can make it too): whether it passed, its name, its numbers.
#[derive(Debug)]
struct Check {
    ok: bool,
    name: &'static str,
    detail: String,
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

// ---- verify ----

fn verify(args: &[String]) -> Result<Verdict, String> {
    let o = Opts::parse(args, &["--seconds", "--device"], &[])?;
    let seconds: u64 = o.value("--seconds", 10)?;
    let name: String = o.value("--device", ENDPOINT.to_string())?;
    if seconds == 0 || seconds > 86_400 {
        return Err(format!("--seconds {seconds}: from 1 to 86400"));
    }
    // The driver's statistics are half the verdict: no driver, no verify.
    let device = open_device()?;
    if read_stats(&device)?.flags & STATS_ATTACHED == 0 {
        return Err("no app ring is attached: start `nf-vmic-test feed` first (elevated, in another window) and leave it running".into());
    }
    let _com = Com::mta()?;
    let mut capture = Capture::open(&name)?;
    println!("recording {name:?} for {seconds} s: {}", capture.describe());
    capture.print_volume_notes();
    let _pro_audio = ProAudio::join();
    capture.start()?;
    let recorded = record(&device, &mut capture, Duration::from_secs(seconds));
    capture.stop();
    let (before, after, checker) = recorded?;

    let r = checker.report();
    let (b, a) = (&before.ring, &after.ring);
    let mut c = Checks::default();
    println!("checks:");
    for check in verify_checks(&before, &after, r, capture.lost_frames, capture.discontinuities) {
        c.check(check.ok, check.name, check.detail);
    }
    let not_reader = a.not_reader_packets.saturating_sub(b.not_reader_packets);
    println!(
        "  the driver over the recording: resyncs +{}, format mismatch +{} packets, a second capture stream +{} packets{}",
        a.resyncs.saturating_sub(b.resyncs),
        a.mismatch_packets.saturating_sub(b.mismatch_packets),
        not_reader,
        if not_reader != 0 { " (another app records the endpoint and may hold the ring: close it)" } else { "" }
    );
    for (index, left, right) in &r.first_corrupt {
        let (l, rt) = (f32::from_bits(*left), f32::from_bits(*right));
        println!("  corrupt at frame {index}: left {left:#010x} ({l}), right {right:#010x} ({rt})");
    }
    capture.print_counters();
    if c.failed == 0 {
        println!("PASS: the app ring fed the recording, bit-exact on both channels, in order, nothing lost or repeated");
        Ok(Verdict::Pass)
    } else {
        println!("FAIL: {} of the checks", c.failed);
        Ok(Verdict::Fail)
    }
}

/// verify's checks of a recording (`r`) against the driver's statistics just before and just after it, and the capture stream's
/// own counts. All must pass for proof that the app ring fed the recording.
fn verify_checks(before: &Stats, after: &Stats, r: &CheckReport, lost_frames: u64, discontinuities: u64) -> Vec<Check> {
    let packet = u64::from(before.reader_packet_frames.max(after.reader_packet_frames));
    let (b, a) = (&before.ring, &after.ring);
    let same = after.attached_pid == before.attached_pid && a.attaches == b.attaches;
    let copied = a.frames_copied.saturating_sub(b.frames_copied);
    let underrun = a.underrun_frames.saturating_sub(b.underrun_frames);
    let silence = r.zeros_before_signal + r.zeros_inside + r.zeros_after_signal;
    vec![
        Check {
            ok: is_reading(before),
            name: "just before the recording",
            detail: format!("process {}'s ring {}", before.attached_pid, state_text(stats_state(before), before.reader_packet_frames)),
        },
        Check {
            ok: is_reading(after) && same,
            name: "just after it",
            detail: format!(
                "process {}'s ring {}; attaches +{}",
                after.attached_pid,
                state_text(stats_state(after), after.reader_packet_frames),
                a.attaches.saturating_sub(b.attaches)
            ),
        },
        Check {
            ok: r.frames > 0 && copied * 10 >= r.frames * 9,
            name: "the driver copied the recording",
            detail: format!(
                "{copied} frames copied from the ring while {} were recorded ({:.1}%; 90% needed)",
                r.frames,
                percent(copied, r.frames)
            ),
        },
        Check {
            ok: r.frames > 0 && r.signal_frames * 10 >= r.frames * 9,
            name: "signal",
            detail: format!(
                "{} of {} frames carry a code ({:.1}%; 90% needed)",
                r.signal_frames,
                r.frames,
                percent(r.signal_frames, r.frames)
            ),
        },
        Check {
            ok: r.zeros_inside <= underrun + packet,
            name: "silence inside the signal",
            detail: format!(
                "{} frames in {} runs; the driver underran {underrun} frames over the recording (one {packet}-frame packet more \
                 allowed)",
                r.zeros_inside, r.zero_runs_inside
            ),
        },
        // Silence at the edges too: `before` shows the ring read, so the only silence the recording may hold is an underrun.
        // The statistics' window and the recording's are offset by the engine's pipeline, so an underrun just before `before`
        // can still arrive in the recording: one packet's slack at each end. Silence nothing counted (another capture stream
        // holding the reader slot for part of the recording, say) fails here.
        Check {
            ok: silence <= underrun + 2 * packet,
            name: "all the silence",
            detail: format!(
                "{silence} frames ({} before the signal, {} inside, {} after); the driver underran {underrun} frames over the \
                 recording (a {packet}-frame packet more allowed at each end)",
                r.zeros_before_signal, r.zeros_inside, r.zeros_after_signal
            ),
        },
        Check {
            ok: r.is_clean(),
            name: "bit-exact and in order",
            detail: format!(
                "gaps {} ({} frames lost), repeats {} ({} frames), corrupt frames {}; codes {:?} to {:?}, {} steps of one",
                r.gaps, r.gap_frames, r.repeats, r.repeat_frames, r.corrupt_frames, r.first_code, r.last_code, r.steps_ok
            ),
        },
        Check {
            ok: lost_frames == 0 && discontinuities == 0,
            name: "the capture stream",
            detail: format!("{lost_frames} frames lost by the engine, {discontinuities} discontinuities after its first packet"),
        },
    ]
}

/// Waits (draining the stream, up to [`READING_WAIT`]) for STATS to show a capture stream reading the ring and for the stream to
/// deliver the signal, then records for `duration`: STATS just before and just after, and the checker over the frames between.
/// (The wait for the signal skips whatever the engine delivers before the driver's first packets reach it.) No recording if
/// neither came.
fn record(device: &Device, capture: &mut Capture, duration: Duration) -> Result<(Stats, Stats, SequenceChecker), String> {
    let t0 = Instant::now();
    let mut signal_seen = false;
    let before = loop {
        capture.read(|f| signal_seen |= signal::decode(f).is_some())?;
        let s = read_stats(device)?;
        if (signal_seen && is_reading(&s)) || t0.elapsed() >= READING_WAIT {
            break s;
        }
    };
    let mut checker = SequenceChecker::new();
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
    let after = read_stats(device)?;
    Ok((before, after, checker))
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

/// A shared-mode, event-driven capture stream, float32 stereo 48 kHz (the endpoint's mix format: no conversion), RAW where the
/// endpoint takes it (no effects). `!Send` (the wasapi objects): made, used and dropped on one thread.
struct Capture {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    buf: Vec<u8>,
    raw: bool,
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

fn werr(e: wasapi::WasapiError) -> String {
    e.to_string()
}

impl Capture {
    fn open(name: &str) -> Result<Self, String> {
        let enumerator = DeviceEnumerator::new().map_err(werr)?;
        let devices = enumerator.get_device_collection(&Direction::Capture).map_err(werr)?;
        let device = devices.get_device_with_name(name).map_err(|_| {
            let names: Vec<String> = (&devices).into_iter().filter_map(|d| d.ok()?.get_friendlyname().ok()).collect();
            format!("no capture endpoint {name:?}; there are: {}", names.join("; "))
        })?;
        let mix = device.get_iaudioclient().map_err(werr)?.get_mixformat().map_err(werr)?;
        let float = matches!(mix.get_subformat(), Ok(SampleType::Float));
        if mix.get_samplespersec() != protocol::SAMPLE_RATE || mix.get_nchannels() != 2 || mix.get_bitspersample() != 32 || !float {
            return Err(format!(
                "{name:?} mixes at {} Hz, {} channels, {}-bit {}: bit-exact needs a 48 kHz float32 stereo device format",
                mix.get_samplespersec(),
                mix.get_nchannels(),
                mix.get_bitspersample(),
                if float { "float" } else { "integer" },
            ));
        }
        let volume = endpoint_volume(&device);
        let format = WaveFormat::new(32, 32, &SampleType::Float, protocol::SAMPLE_RATE as usize, 2, None);
        // RAW first; a refused initialise leaves the client unusable, so the fallback takes a fresh one.
        let (client, raw) = match Self::client(&device, &format, true) {
            Ok(c) => (c, true),
            Err(e) => {
                eprintln!("note: RAW refused ({e}); the endpoint's effects, if any, run");
                (Self::client(&device, &format, false).map_err(werr)?, false)
            }
        };
        let event = client.set_get_eventhandle().map_err(werr)?;
        let capture = client.get_audiocaptureclient().map_err(werr)?;
        let buffer_frames = client.get_buffer_size().map_err(werr)? as usize;
        Ok(Self {
            client,
            capture,
            event,
            // A packet never exceeds the buffer; twice is the engine's margin (stream.rs).
            buf: vec![0; 2 * buffer_frames * protocol::FRAME_BYTES as usize],
            raw,
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

    fn client(device: &wasapi::Device, format: &WaveFormat, raw: bool) -> Result<AudioClient, wasapi::WasapiError> {
        let mut client = device.get_iaudioclient()?;
        if raw {
            client.set_properties(AudioClientProperties::new().set_option(StreamOption::Raw))?;
        }
        // 100 ms of buffer: this reader only has to keep up, not be quick.
        client.initialize_client(format, &Direction::Capture, &StreamMode::EventsShared {
            autoconvert: false,
            buffer_duration_hns: 1_000_000,
        })?;
        Ok(client)
    }

    fn describe(&self) -> String {
        let raw = if self.raw { "RAW" } else { "not RAW" };
        match self.volume {
            Some(v) => format!(
                "float32 stereo 48 kHz, shared, {raw}; endpoint volume {:.0}%, {}",
                v.level * 100.0,
                if v.muted { "MUTED" } else { "unmuted" }
            ),
            None => format!("float32 stereo 48 kHz, shared, {raw}; endpoint volume unknown"),
        }
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
        self.client.start_stream().map_err(werr)
    }

    fn stop(&self) {
        let _ = self.client.stop_stream();
    }

    /// Waits for the stream's event (2 s at most), then hands every frame that's ready to `sink`.
    fn read(&mut self, mut sink: impl FnMut([f32; 2])) -> Result<(), String> {
        let t = Instant::now();
        self.event.wait_for_event(2_000).map_err(|_| "no capture event for 2 s: is the endpoint's stream running?".to_string())?;
        self.longest_wait = self.longest_wait.max(t.elapsed());
        loop {
            let (frames, info) = self.capture.read_from_device(&mut self.buf).map_err(werr)?;
            if frames == 0 {
                return Ok(());
            }
            let frames = u64::from(frames);
            self.packets += 1;
            self.frames += frames;
            if let Some(expect) = self.next_index
                && info.index > expect
            {
                self.lost_frames += info.index - expect;
            }
            // The stream's first packet may carry the discontinuity flag (the engine's start, not a glitch: inference from
            // reports of it, not checked on this endpoint), so only later ones count.
            if info.flags.data_discontinuity && self.next_index.is_some() {
                self.discontinuities += 1;
            }
            self.next_index = Some(info.index + frames);
            if info.flags.silent {
                // The crate copies the buffer even for a silent packet; the flag says it's silence.
                self.silent_packets += 1;
                (0..frames).for_each(|_| sink([0.0; 2]));
                continue;
            }
            let (chunks, _) = self.buf[..frames as usize * protocol::FRAME_BYTES as usize].as_chunks::<8>();
            for &[a, b, c, d, e, f, g, h] in chunks {
                sink([f32::from_le_bytes([a, b, c, d]), f32::from_le_bytes([e, f, g, h])]);
            }
        }
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
    Opts::parse(args, &[], &[])?;
    print_stats(&read_stats(&open_device()?)?);
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
        "  attaches {}, detaches {} (cancel {}, cleanup {}, removal {}), refused busy {}, refused invalid {}",
        r.attaches, r.detaches, r.detach_cancel, r.detach_cleanup, r.detach_removal, r.busy_rejects, r.invalid_rejects
    );
    println!(
        "  passes {}, frames copied {}, underruns {} packets / {} frames, resyncs {}, format mismatch {} packets, second reader {} packets",
        r.passes, r.frames_copied, r.underrun_packets, r.underrun_frames, r.resyncs, r.mismatch_packets, r.not_reader_packets
    );
    if s.flags & STATS_LOOPBACK != 0 {
        let l = &s.loopback;
        println!(
            "loopback: zero-filled packets: idle {}, format mismatch {}, underrun {} ({} frames), overrun {}",
            l.idle_packets, l.mismatch_packets, l.underrun_packets, l.underrun_frames, l.overrun_packets
        );
        println!(
            "  capture anchors {}, render restarts {}, gap fills {}, late packets {}, lost packets {}, second render stream {}",
            l.capture_anchors, l.render_restarts, l.render_gap_fills, l.render_late_packets, l.render_lost_packets, l.render_not_owner
        );
    } else {
        println!("loopback: no device");
    }
    print_timer("render", &s.stream[KIND_RENDER]);
    print_timer("capture", &s.stream[KIND_CAPTURE]);
}

fn print_timer(kind: &str, t: &StreamStats) {
    let mean = if t.timer_passes == 0 { 0.0 } else { us(t.lateness_sum_hns) / t.timer_passes as f64 };
    println!(
        "{kind} timer: {} passes in {} runs; glitch adjustments {} ({:.1} ms in all); early {}; lateness mean {mean:.1} us, max {:.1} us",
        t.timer_passes,
        t.runs,
        t.glitch_passes,
        us(t.glitch_hns) / 1000.0,
        t.early_passes,
        us(t.lateness_max_hns)
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
    let mut capture = match Capture::open(&name).and_then(|c| c.start().map(|()| c)) {
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
            println!("kill {k}/{kills}: FAIL: the child (process {pid}) didn't attach within {} s{exit}", ATTACH_WAIT.as_secs());
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
        "attaches +{}, detaches +{} (cancel +{}, cleanup +{}, removal +{})",
        l.attaches.saturating_sub(f.attaches),
        l.detaches.saturating_sub(f.detaches),
        l.detach_cancel.saturating_sub(f.detach_cancel),
        l.detach_cleanup.saturating_sub(f.detach_cleanup),
        l.detach_removal.saturating_sub(f.detach_removal)
    );
    let verdict = kill_test_verdict(c.failed, unread, timer_runs);
    match verdict {
        Verdict::Fail => println!("FAIL: {} of the checks", c.failed),
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

#[cfg(test)]
mod tests {
    use super::*;

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

    use nf_vmic::protocol::RingStats;

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

    /// The names of the checks that failed.
    fn failed(checks: &[Check]) -> Vec<&'static str> {
        checks.iter().filter(|c| !c.ok).map(|c| c.name).collect()
    }

    /// verify's checks for a recording `r` between `before` and `after`, with nothing lost and no discontinuity.
    fn verify_failures(before: &Stats, after: &Stats, r: &CheckReport) -> Vec<&'static str> {
        failed(&verify_checks(before, after, r, 0, 0))
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
        let checks = verify_checks(&before, &after, &clean_recording(), 0, 0);
        assert_eq!(checks.len(), 8);
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

        // One corrupt frame, a gap, a repeat.
        let corrupt = CheckReport { signal_frames: FRAMES - 1, corrupt_frames: 1, ..r.clone() };
        assert_eq!(verify_failures(&before, &after, &corrupt), ["bit-exact and in order"]);
        assert_eq!(verify_failures(&before, &after, &CheckReport { gaps: 1, gap_frames: 5, ..r.clone() }), ["bit-exact and in order"]);
        assert_eq!(verify_failures(&before, &after, &CheckReport { repeats: 1, ..r.clone() }), ["bit-exact and in order"]);
        // One discontinuity, or frames the engine lost.
        assert_eq!(failed(&verify_checks(&before, &after, &r, 0, 1)), ["the capture stream"]);
        assert_eq!(failed(&verify_checks(&before, &after, &r, 10, 0)), ["the capture stream"]);
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
}
