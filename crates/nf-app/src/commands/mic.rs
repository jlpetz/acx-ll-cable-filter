//! `mic sessions` and `mic variant` (TODO 19): the virtual microphone driver's two microphones, which apps use which, and
//! the driver's variant (both microphones, the call one only, the low-latency one only). The work is nf-vmic's
//! (`sessions`, `variant`); this is the printing and the elevation.
//!
//! `mic variant X`, not elevated, first reads what's installed and the microphones Windows has (any user may: to be
//! confirmed live from a standard user's prompt), and does nothing if X is already the only variant there, working on the
//! driver store's newest AudioCodec package (one on an older package, such as the single-microphone driver's, which had
//! `ROOT\AudioCodec` too, is re-created), with each of X's microphones (by kind, whatever it's named). A microphone missing
//! there is one that package doesn't have (the single-microphone package alone in the driver store): it says so and exits 2,
//! with no prompt. Otherwise it relaunches itself elevated, one UAC prompt, as `mic variant X --elevated-helper --report
//! PIPE`, waits, and exits with the helper's code (3 if the prompt was declined). The helper reports each step through the
//! named pipe (report_pipe.rs) and this side prints them as they come. Elevated already (an administrator terminal), it
//! switches in-process. Either way the switch (nf-vmic's) holds a system-wide lock, checks a device it keeps the same way,
//! and succeeds only once the new device works on the chosen package and Windows has made its microphones; one Windows
//! must restart to finish exits 7, unchecked (ours, the owner's to confirm). The helper never opens or attaches a console
//! (main.rs); one that finds itself not elevated (UAC turned off) stops with exit code 4 instead of relaunching itself, as
//! setup-large-pages' helper does.

use std::fmt::Write as _;
use std::time::Duration;

use nf_mem::privilege::{self, ElevationError};
use nf_vmic::sessions::{self, CaptureEndpoint, Endpoint, Fit, Mic, Session};
use nf_vmic::variant::{self, Health, OurDevice, Outcome, SwitchError, Variant};
use nf_vmic::win_error::WinError;

use crate::cli::{HELPER_FLAG, MicVariantArgs, REPORT_FLAG};
use crate::exit::{Exit, Failure};
use crate::report_pipe::{ReportClient, ReportServer};

/// How long the caller waits for the helper's last lines after it exits.
const REPORT_GRACE: Duration = Duration::from_secs(2);

/// What the helper says when it starts without elevation.
const NOT_ELEVATED: &str = "The elevated switch started without elevation (is UAC turned off?). Run `noise-filter mic variant` \
     from an elevated (administrator) prompt instead.";

pub fn sessions() -> Result<(), Failure> {
    let endpoints = sessions::list().map_err(|e| Failure::error(format!("Couldn't read the audio sessions: {e}")))?;
    say!("{}", format_sessions(&endpoints).trim_end());
    Ok(())
}

/// Each endpoint, then each session on it with what suits the app.
fn format_sessions(endpoints: &[Endpoint]) -> String {
    let mut s = String::new();
    if endpoints.is_empty() {
        let _ = writeln!(
            s,
            "No AudioCodec Device microphone is active. `noise-filter mic variant` says whether the driver is installed, and \
             which microphones it has; the Sound panel's Recording tab may have one disabled."
        );
        return s;
    }
    let active: Vec<Mic> = endpoints.iter().map(|e| e.mic).collect();
    for e in endpoints {
        let _ = writeln!(s, "{}", e.name);
        if e.sessions.is_empty() {
            let _ = writeln!(s, "  no app has it open");
        }
        for session in &e.sessions {
            let _ = writeln!(s, "  {}, {}: {}", who(session), session.state.describe(), advice(session, e.mic, &active));
        }
    }
    let _ = writeln!(s, "\nAn app that records in exclusive mode (some DAWs) has no session, so it isn't listed.");
    s
}

/// "Discord.exe (pid 1234)", or why the app isn't known.
fn who(s: &Session) -> String {
    if s.system_sounds {
        return "Windows' system sounds".into();
    }
    if s.pid == 0 {
        return "a session of several processes".into();
    }
    match &s.image {
        Ok(_) => format!("{} (pid {})", s.exe().unwrap_or("?"), s.pid),
        Err(e) => format!("pid {} (its program couldn't be read: {e})", s.pid),
    }
}

/// What suits the app recording from `on`. `active`: our microphones active now.
fn advice(s: &Session, on: Mic, active: &[Mic]) -> String {
    let kind = s.kind();
    match sessions::fit(kind, on) {
        Fit::Suits => format!("{}; {} suits it.", kind.describe(), on.describe()),
        Fit::Better(mic) => {
            let mut text = format!("{}; {} suits it better ({}).", kind.describe(), mic.describe(), why(mic));
            if !active.contains(&mic) {
                text.push_str(" It isn't active now: `noise-filter mic variant` says which microphones the driver has.");
            }
            text
        }
        Fit::Unknown => match kind.suited_mic() {
            None => "unknown, no suggestion.".into(),
            Some(mic) => format!("{}; {} suits it.", kind.describe(), mic.describe()),
        },
    }
}

/// Why a microphone suits the apps it does.
fn why(mic: Mic) -> &'static str {
    match mic {
        Mic::Call => "its 5 ms default period halves the latency of an app that asks for none, as call apps do",
        Mic::LowLatency => "it keeps IAudioClient3's small periods, 2.67-10 ms, for apps that ask for them",
        Mic::Other => "",
    }
}

pub fn variant(args: &MicVariantArgs) -> Result<(), Failure> {
    let Some(target) = args.target else { return show() };
    match next_step(privilege::is_elevated(), args.helper) {
        Step::Refuse => {
            log::warn!("the mic variant helper started without elevation; not relaunching it");
            Err(Failure::not_elevated(NOT_ELEVATED))
        }
        Step::Switch => switch(target, args.report.as_deref()),
        Step::Elevate => elevate(target),
    }
}

/// What a `mic variant X` process does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Elevated: switch.
    Switch,
    /// Not elevated: relaunch elevated as the helper.
    Elevate,
    /// The helper, not elevated: stop, or it would relaunch itself without end.
    Refuse,
}

fn next_step(elevated: bool, helper: bool) -> Step {
    match (elevated, helper) {
        (true, _) => Step::Switch,
        (false, false) => Step::Elevate,
        (false, true) => Step::Refuse,
    }
}

fn read_devices() -> Result<Vec<OurDevice>, Failure> {
    variant::present().map_err(|e| Failure::error(format!("Couldn't read the devices: {e}")))
}

/// `mic variant`: which variant is installed, and the microphones Windows has.
fn show() -> Result<(), Failure> {
    say!("{}", format_variant(&read_devices()?, &sessions::capture_endpoints()).trim_end());
    Ok(())
}

/// The devices of ours there now, the microphones Windows has of ours (`mics`), and how to switch.
fn format_variant(devices: &[OurDevice], mics: &Result<Vec<CaptureEndpoint>, WinError>) -> String {
    let mut s = String::new();
    match devices {
        [] => {
            let ids: Vec<&str> = Variant::ALL.iter().map(|v| v.hardware_id()).collect();
            let _ = writeln!(s, "No AudioCodec device is installed (no device has {}).", ids.join(", "));
        }
        [d] => {
            let _ = writeln!(s, "Variant: {}, {} ({})", d.variant.arg(), d.variant.describe(), d.variant.hardware_id());
            let _ = writeln!(s, "  {}", d.describe());
            let _ = writeln!(s, "  ({})", d.package_note());
            if d.is_current() && d.health == Health::Working {
                // What Windows has of them, by kind: the newest package may still lack one (the single-microphone one's)
                match variant::check_kept(d.variant, d, mics) {
                    Ok(check) => {
                        let names = variant::quoted(check.found.iter().map(|f| f.name.as_str()));
                        let _ = writeln!(s, "  Microphones: {names}");
                        for line in variant::found_lines(&check).iter().filter(|l| l.starts_with("  warning: ")) {
                            let _ = writeln!(s, "{line}");
                        }
                    }
                    Err(SwitchError::PackageLacks { missing, .. }) => {
                        let _ =
                            writeln!(s, "  Missing: {}: {}", variant::mics_text(&missing), variant::package_advice(&missing, d.variant));
                    }
                    // Read below
                    Err(_) => {
                        let _ = writeln!(s, "  The variant's microphones: {}", d.variant.microphones_text());
                    }
                }
            } else if d.is_current() {
                let _ = writeln!(s, "  The variant's microphones, once it works: {}", d.variant.microphones_text());
            } else if matches!(d.newest, Ok(Some(_))) {
                // An older package (the single-microphone driver's, say) may not have them
                let _ = writeln!(s, "  Microphones with the newest package: {}", d.variant.microphones_text());
                let _ = writeln!(s, "  `noise-filter mic variant {}` re-creates the device with the newest package.", d.variant.arg());
            } else {
                let _ = writeln!(s, "  The variant's microphones: {}", d.variant.microphones_text());
            }
        }
        several => {
            let _ = writeln!(s, "{} AudioCodec devices are installed:", several.len());
            for d in several {
                let _ = writeln!(s, "  {}", d.describe());
                let _ = writeln!(s, "    ({})", d.package_note());
            }
        }
    }
    match mics {
        Ok(list) if list.is_empty() && devices.is_empty() => {}
        Ok(list) if list.is_empty() => {
            let _ = writeln!(s, "Windows has no AudioCodec microphone now.");
        }
        Ok(list) => {
            let names: Vec<String> = list
                .iter()
                .map(|e| if e.active { format!("\"{}\"", e.name) } else { format!("\"{}\" (disabled in the Sound panel)", e.name) })
                .collect();
            let _ = writeln!(s, "Windows has: {}", names.join(", "));
        }
        Err(e) => {
            let _ = writeln!(s, "(The microphones Windows has couldn't be read: {e})");
        }
    }
    let _ = writeln!(
        s,
        "Switch with `noise-filter mic variant both|call|low-latency`: both microphones, the call microphone only (a 5 ms \
         default period) or the low-latency one only. Windows asks for permission, as the switch re-creates the device."
    );
    s
}

/// Elevated: the switch itself, its steps to the caller's pipe (the helper) or to this console.
fn switch(target: Variant, report: Option<&str>) -> Result<(), Failure> {
    let mut pipe =
        report.and_then(|name| ReportClient::connect(name).map_err(|e| log::warn!("mic variant: no report to the caller: {e}")).ok());
    let mut step = |line: &str| {
        log::info!("mic variant: {line}");
        match pipe.as_mut() {
            Some(p) => p.line(line),
            None => say!("{line}"),
        }
    };
    let result = variant::switch(target, &mut step);
    match result {
        Ok(Outcome::Nothing) => Ok(()),
        Ok(Outcome::Switched) => {
            step("Done: the device works on the newest package, with its microphones.");
            Ok(())
        }
        Ok(Outcome::RestartPending { device }) => {
            let message = restart_message(target, &device);
            // As an error's: the caller reads it from the report, and gets the code
            if report.is_some() {
                step(&message);
            }
            Err(Failure::restart_pending(message))
        }
        Err(e) => {
            let message = format!("The switch to {} stopped: {e}", target.arg());
            // The helper's own error output goes nowhere: its caller reads it from the report
            if report.is_some() {
                step(&format!("error: {message}"));
            }
            Err(Failure::error(message))
        }
    }
}

/// A switch Windows must restart to finish (exit code 7; ours, the owner's to confirm).
fn restart_message(target: Variant, device: &str) -> String {
    format!(
        "The switch to {} isn't finished: Windows needs a restart to finish installing {device}. Restart Windows, then run \
         `noise-filter mic variant` to check its microphones.",
        target.arg()
    )
}

/// Not elevated: relaunches itself elevated as the helper, prints its report, and exits with its code.
fn elevate(target: Variant) -> Result<(), Failure> {
    let devices = read_devices()?;
    match nothing_to_do(&devices, target, &sessions::capture_endpoints()) {
        Kept::Nothing(lines) => {
            for line in lines {
                say!("{line}");
            }
            return Ok(());
        }
        Kept::Lacks(e) => return Err(Failure::error(format!("The switch to {} stopped: {e}", target.arg()))),
        Kept::Unchecked(e) => say!("note: the microphones couldn't be read here ({e}); the elevated switch reads them again."),
        Kept::No => {}
    }
    if let Some(note) = upgrade_note(&devices, target) {
        say!("{note}");
    }
    say!(
        "Switching the AudioCodec driver to {} ({}). Windows will ask for permission now (a UAC prompt): removing and creating \
         a device needs an administrator. The microphones are gone for a few seconds, and apps recording from them lose them.",
        target.describe(),
        target.hardware_id()
    );
    let report = ReportServer::start(|line| say!("{line}"))
        .map_err(|e| {
            log::warn!("mic variant: no report pipe: {e}");
            say!("note: the elevated steps can't show here ({e}); the log has them.");
        })
        .ok();
    let mut args = vec!["mic", "variant", target.arg(), HELPER_FLAG];
    if let Some(r) = &report {
        args.extend([REPORT_FLAG, r.name()]);
    }
    let launched = privilege::run_elevated_checked(&args);
    if let Some(r) = report {
        r.finish(REPORT_GRACE);
    }
    let code = match launched {
        Ok(code) => code,
        Err(ElevationError::Declined) => {
            return Err(Failure::no_result("The permission prompt was declined (ERROR_CANCELLED, 1223): nothing changed."));
        }
        Err(ElevationError::Failed(why)) => return Err(Failure::error(format!("Couldn't start the elevated switch: {why}"))),
    };
    log::info!("the elevated mic variant switch exited with {code}");
    helper_result(code)?;
    // What anyone can now read
    if let Ok(devices) = variant::present() {
        say!("{}", format_variant(&devices, &sessions::capture_endpoints()).trim_end());
    }
    Ok(())
}

/// Whether a switch to `target` has nothing to do, before any prompt ([`nothing_to_do`]).
#[derive(Debug, PartialEq)]
enum Kept {
    /// The target is already the only variant there, on the newest package, with its microphones: what to say.
    Nothing(Vec<String>),
    /// It would be, but Windows lacks one of its microphones: its package doesn't have it, which a switch can't mend.
    Lacks(SwitchError),
    /// It would be, but the microphones couldn't be read here: the elevated switch checks them.
    Unchecked(WinError),
    /// The switch has something to do.
    No,
}

/// The not-elevated side's check: nothing to do only when the plan keeps everything and the device kept has the variant's
/// microphones (`mics`, as Windows has them; the single-microphone package alone in the driver store has no Call Microphone).
fn nothing_to_do(devices: &[OurDevice], target: Variant, mics: &Result<Vec<CaptureEndpoint>, WinError>) -> Kept {
    let plan = variant::plan(devices, target);
    let Some(k) = plan.keep.filter(|_| plan.is_nothing()) else { return Kept::No };
    match variant::check_kept(target, &devices[k], mics) {
        Ok(check) => {
            let mut lines = vec![format!("Already {} ({}) on the newest package: nothing to do.", target.describe(), target.hardware_id())];
            lines.extend(variant::found_lines(&check));
            Kept::Nothing(lines)
        }
        Err(SwitchError::NoEndpoints(e)) => Kept::Unchecked(e),
        Err(e) => Kept::Lacks(e),
    }
}

/// Why a switch to the variant already there goes ahead: its device works, but not on the newest package (an upgrade).
fn upgrade_note(devices: &[OurDevice], target: Variant) -> Option<String> {
    let d = devices.iter().find(|d| d.variant == target && d.health == Health::Working && !d.is_current())?;
    Some(format!("{} is {} already, but {}: the switch re-creates it with the newest.", d.instance_id, target.describe(), d.package_note()))
}

/// The helper's exit code as this process's result (the same code, where it's one of ours).
fn helper_result(code: u32) -> Result<(), Failure> {
    let log = || crate::logger::default_path().display().to_string();
    match code {
        0 => Ok(()),
        c if c == u32::from(Exit::Usage.code()) => Err(Failure::usage("The elevated switch didn't accept its arguments (exit code 1).")),
        c if c == u32::from(Exit::Error.code()) => Err(Failure::error(format!(
            "The elevated switch failed (exit code 2): its steps are above, and in the log ({}; the log of the account that \
             approved the prompt, if that was another one).",
            log()
        ))),
        c if c == u32::from(Exit::NotElevated.code()) => Err(Failure::not_elevated(NOT_ELEVATED)),
        c if c == u32::from(Exit::RestartPending.code()) => Err(Failure::restart_pending(
            "The elevated switch isn't finished (exit code 7): Windows needs a restart to finish installing the new device. \
             Restart Windows, then run `noise-filter mic variant` to check its microphones.",
        )),
        c => Err(Failure::error(format!("The elevated switch stopped unexpectedly (exit code 0x{c:X}). The log may say why: {}", log()))),
    }
}

#[cfg(test)]
mod tests {
    use nf_vmic::sessions::SessionState;
    use nf_vmic::variant::{CALL_MICROPHONE, Candidate, MICROPHONE};

    use super::*;

    // No device, session or UAC prompt here: the printing and the decisions only.

    fn session(image: &str, state: SessionState) -> Session {
        Session { pid: 1234, image: Ok(image.into()), state, system_sounds: false }
    }

    fn endpoint(name: &str, sessions: Vec<Session>) -> Endpoint {
        Endpoint { id: "{0.0.1.00000000}.{x}".into(), name: name.into(), mic: sessions::mic_of(name), sessions }
    }

    #[test]
    fn sessions_are_listed_with_what_suits_each_app() {
        let call = endpoint(CALL_MICROPHONE, vec![
            session(r"C:\x\Discord.exe", SessionState::Active),
            session(r"C:\x\reaper.exe", SessionState::Inactive),
            session(r"C:\x\notepad.exe", SessionState::Expired),
        ]);
        let low = endpoint(MICROPHONE, vec![session("ms-teams.exe", SessionState::Active), session("chrome.exe", SessionState::Active)]);
        let text = format_sessions(&[call.clone(), low]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CALL_MICROPHONE);
        assert_eq!(lines[1], "  Discord.exe (pid 1234), active: a call app; the Call Microphone suits it.");
        assert!(
            lines[2].starts_with("  reaper.exe (pid 1234), inactive: a low-latency recorder; the Microphone suits it better (it keeps"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[3], "  notepad.exe (pid 1234), expired: unknown, no suggestion.");
        assert_eq!(lines[4], MICROPHONE);
        assert!(
            lines[5].starts_with("  ms-teams.exe (pid 1234), active: a call app; the Call Microphone suits it better (its 5 ms"),
            "{}",
            lines[5]
        );
        assert!(
            lines[6].starts_with("  chrome.exe (pid 1234), active: a browser (web calls, probably); the Call Microphone suits it better")
        );
        assert!(!text.contains("isn't active now"), "both microphones are there");
        assert!(text.trim_end().ends_with("has no session, so it isn't listed."));

        // The call microphone alone: a recorder's better microphone isn't there
        let text = format_sessions(&[call]);
        assert!(
            text.contains("the Microphone suits it better") && text.contains("It isn't active now: `noise-filter mic variant`"),
            "{text}"
        );
        // Nothing open, nothing of ours, and sessions without a known program
        let empty = endpoint(MICROPHONE, vec![]);
        assert!(format_sessions(&[empty]).contains("  no app has it open"));
        assert!(format_sessions(&[]).starts_with("No AudioCodec Device microphone is active."));
        let odd = vec![
            Session { pid: 0, image: Ok(String::new()), state: SessionState::Active, system_sounds: false },
            Session { pid: 9, image: Err(WinError { call: "OpenProcess", code: 87 }), state: SessionState::Active, system_sounds: false },
            Session { pid: 0, image: Ok(String::new()), state: SessionState::Inactive, system_sounds: true },
        ];
        let text = format_sessions(&[endpoint(MICROPHONE, odd)]);
        assert!(text.contains("  a session of several processes, active: unknown, no suggestion."), "{text}");
        assert!(text.contains("  pid 9 (its program couldn't be read: OpenProcess failed: ERROR_INVALID_PARAMETER (87)"), "{text}");
        assert!(text.contains("  Windows' system sounds, inactive: unknown, no suggestion."), "{text}");
        // A renamed endpoint: what suits the app, without saying it's the wrong one
        let renamed = endpoint("Studio mic (AudioCodec Device)", vec![session("zoom.exe", SessionState::Active)]);
        assert!(format_sessions(&[renamed]).contains("  zoom.exe (pid 1234), active: a call app; the Call Microphone suits it."));
    }

    /// The driver store's newest package in these tests: oem12.inf, 1.0.0.0.
    fn newest() -> Candidate {
        Candidate {
            inf: r"C:\Windows\INF\oem12.inf".into(),
            original: Ok("audiocodec.inf".into()),
            provider: "VS_Microsoft".into(),
            description: "AudioCodec Device".into(),
            date: 116_444_736_000_000_000,
            version: 1 << 48,
        }
    }

    /// A device on the newest package.
    fn dev(variant: Variant, health: Health) -> OurDevice {
        OurDevice {
            instance_id: r"ROOT\MEDIA\0003".into(),
            variant,
            health,
            inf: Some("oem12.inf".into()),
            driver_version: Some("1.0.0.0".into()),
            newest: Ok(Some(newest())),
        }
    }

    /// A device on an older package: the single-microphone driver, which had `ROOT\AudioCodec` too.
    fn old(variant: Variant) -> OurDevice {
        OurDevice { inf: Some("oem3.inf".into()), driver_version: Some("0.9.0.300".into()), ..dev(variant, Health::Working) }
    }

    /// One of our endpoints on the device [`dev`] makes (its filter path has the instance ID in lower case), of the kind its
    /// name says.
    fn ep(name: &str, active: bool) -> CaptureEndpoint {
        ep_as(name, sessions::mic_of(name), active)
    }

    fn ep_as(name: &str, mic: Mic, active: bool) -> CaptureEndpoint {
        CaptureEndpoint {
            id: format!("{{0.0.1.00000000}}.{{{name}}}"),
            name: name.into(),
            active,
            mic,
            device: Some(r"root\media\0003".into()),
        }
    }

    #[test]
    fn the_variant_reads_as_its_devices() {
        let no_mics = Ok(vec![]);
        let none = format_variant(&[], &no_mics);
        assert!(none.starts_with(
            r"No AudioCodec device is installed (no device has ROOT\AudioCodec, ROOT\AudioCodecCall, ROOT\AudioCodecLowLatency)."
        ));
        assert!(!none.contains("Windows has"), "{none}");
        let call_mic = Ok(vec![ep(CALL_MICROPHONE, true)]);
        let one = format_variant(&[dev(Variant::Call, Health::Working)], &call_mic);
        let lines: Vec<&str> = one.lines().collect();
        assert_eq!(lines[0], r"Variant: call, the call microphone only (ROOT\AudioCodecCall)");
        assert_eq!(lines[1], r"  ROOT\MEDIA\0003: the call microphone only (ROOT\AudioCodecCall), working, driver oem12.inf 1.0.0.0");
        assert_eq!(lines[2], "  (it runs the newest AudioCodec package in the driver store)");
        assert_eq!(lines[3], "  Microphones: \"Call Microphone (AudioCodec Device)\"");
        assert_eq!(lines[4], "Windows has: \"Call Microphone (AudioCodec Device)\"");
        let both = format_variant(&[dev(Variant::Both, Health::Working)], &Ok(vec![ep(MICROPHONE, true), ep(CALL_MICROPHONE, false)]));
        assert!(both.contains("Microphones: \"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\""));
        assert!(
            both.contains(
                "\n  warning: \"Call Microphone (AudioCodec Device)\" is disabled in the Sound panel, so apps don't see it: enable it"
            ),
            "{both}"
        );
        assert!(
            both.contains(
                "Windows has: \"Microphone (AudioCodec Device)\", \"Call Microphone (AudioCodec Device)\" (disabled in the Sound panel)"
            ),
            "{both}"
        );
        // Not working: what it would have
        let off = format_variant(&[dev(Variant::Call, Health::Problem(22))], &no_mics);
        assert!(off.contains("  The variant's microphones, once it works: \"Call Microphone (AudioCodec Device)\""), "{off}");
        let two = format_variant(&[dev(Variant::Both, Health::Working), dev(Variant::LowLatency, Health::Problem(28))], &no_mics);
        assert!(two.starts_with("2 AudioCodec devices are installed:\n"), "{two}");
        assert!(two.contains("no driver installed (Code 28)"));
        assert!(two.contains("\n    (it runs the newest AudioCodec package in the driver store)\n"), "{two}");
        assert!(two.contains("Windows has no AudioCodec microphone now."), "{two}");
        let unread = format_variant(
            &[dev(Variant::Call, Health::Working)],
            &Err(WinError { call: "CoCreateInstance(MMDeviceEnumerator)", code: 5 }),
        );
        assert!(unread.contains("(The microphones Windows has couldn't be read: CoCreateInstance(MMDeviceEnumerator) failed"), "{unread}");
        for text in [none, one, both, two, unread] {
            assert!(text.contains("Switch with `noise-filter mic variant both|call|low-latency`"));
        }
    }

    /// Astra's review: the single-microphone driver read as `both`, with a Call Microphone it doesn't have.
    #[test]
    fn a_device_on_an_older_package_says_so() {
        let text = format_variant(&[old(Variant::Both)], &Ok(vec![ep(MICROPHONE, true)]));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], r"Variant: both, both microphones (ROOT\AudioCodec)");
        assert!(
            lines[2].starts_with(r"  (it doesn't run the newest AudioCodec package in the driver store, C:\Windows\INF\oem12.inf"),
            "{}",
            lines[2]
        );
        assert_eq!(
            lines[3],
            "  Microphones with the newest package: \"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\""
        );
        assert_eq!(lines[4], "  `noise-filter mic variant both` re-creates the device with the newest package.");
        assert_eq!(lines[5], "Windows has: \"Microphone (AudioCodec Device)\"");
        // Without a newer package to install, no suggestion to
        let none = OurDevice { newest: Ok(None), ..old(Variant::Both) };
        let text = format_variant(&[none], &Ok(vec![]));
        assert!(text.contains(r"(no AudioCodec package in the driver store has ROOT\AudioCodec)"), "{text}");
        assert!(text.contains("  The variant's microphones: ") && !text.contains("with the newest package"), "{text}");

        // The switch says why it goes ahead
        let note = upgrade_note(&[old(Variant::Both)], Variant::Both).unwrap();
        assert!(
            note.starts_with(
                r"ROOT\MEDIA\0003 is both microphones already, but it doesn't run the newest AudioCodec package in the driver store"
            ),
            "{note}"
        );
        assert!(note.ends_with(": the switch re-creates it with the newest."), "{note}");
        assert_eq!(upgrade_note(&[dev(Variant::Both, Health::Working)], Variant::Both), None);
        assert_eq!(upgrade_note(&[old(Variant::Both)], Variant::Call), None, "another variant: a plain switch");
        assert_eq!(upgrade_note(&[OurDevice { health: Health::Problem(22), ..old(Variant::Both) }], Variant::Both), None);
        assert!(!variant::plan(&[old(Variant::Both)], Variant::Both).is_nothing(), "the caller elevates for it");
        assert_eq!(nothing_to_do(&[old(Variant::Both)], Variant::Both, &Ok(vec![ep(MICROPHONE, true)])), Kept::No);
    }

    /// Astra's review: with the single-microphone package alone in the driver store, its device runs the newest package, so
    /// `mic variant both` said there was nothing to do and `mic variant` listed both microphones.
    #[test]
    fn a_device_without_the_variants_microphones_says_so() {
        let single = Ok(vec![ep(MICROPHONE, true)]);
        let text = format_variant(&[dev(Variant::Both, Health::Working)], &single);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[2], "  (it runs the newest AudioCodec package in the driver store)");
        assert!(
            lines[3].starts_with(
                "  Missing: \"Call Microphone (AudioCodec Device)\": the package it runs doesn't have it (the single-microphone \
                 driver's has the Microphone alone). Add the package with the three variants to the driver store"
            ),
            "{}",
            lines[3]
        );
        assert!(
            lines[3].ends_with("then run `noise-filter mic variant both`: it re-creates the device with that package."),
            "{}",
            lines[3]
        );
        assert_eq!(lines[4], "Windows has: \"Microphone (AudioCodec Device)\"");

        // No prompt for it: the switch can't mend it, so it says so and fails
        let Kept::Lacks(e) = nothing_to_do(&[dev(Variant::Both, Health::Working)], Variant::Both, &single) else { panic!() };
        assert!(e.to_string().starts_with(r"ROOT\MEDIA\0003 works on the driver store's newest AudioCodec package"), "{e}");
        // The low-latency variant has what it needs
        let Kept::Nothing(lines) = nothing_to_do(&[dev(Variant::LowLatency, Health::Working)], Variant::LowLatency, &single) else {
            panic!()
        };
        assert_eq!(lines, [
            r"Already the low-latency microphone only (ROOT\AudioCodecLowLatency) on the newest package: nothing to do.",
            "  the Microphone: \"Microphone (AudioCodec Device)\"",
        ]);
        // Unread here: the elevated switch checks
        let unread = Err(WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 });
        assert!(matches!(nothing_to_do(&[dev(Variant::Both, Health::Working)], Variant::Both, &unread), Kept::Unchecked(_)));
        // Something to do
        assert_eq!(nothing_to_do(&[dev(Variant::Both, Health::Working)], Variant::Call, &single), Kept::No);
        assert_eq!(nothing_to_do(&[], Variant::Call, &single), Kept::No);
    }

    /// Astra's review: Windows' numbered names and the user's own never counted. By kind, with the name Windows has.
    #[test]
    fn the_microphones_are_shown_by_the_names_they_have() {
        let named =
            Ok(vec![ep_as("Studio mic (2- AudioCodec Device)", Mic::LowLatency, true), ep("Call Microphone (2- AudioCodec Device)", true)]);
        let text = format_variant(&[dev(Variant::Both, Health::Working)], &named);
        assert!(
            text.contains("\n  Microphones: \"Studio mic (2- AudioCodec Device)\" and \"Call Microphone (2- AudioCodec Device)\"\n"),
            "{text}"
        );
        let Kept::Nothing(lines) = nothing_to_do(&[dev(Variant::Both, Health::Working)], Variant::Both, &named) else { panic!() };
        assert_eq!(&lines[1..], [
            "  the Microphone: \"Studio mic (2- AudioCodec Device)\"",
            "  the Call Microphone: \"Call Microphone (2- AudioCodec Device)\"",
        ]);
    }

    #[test]
    fn a_switch_windows_must_restart_to_finish_isnt_done() {
        let text = restart_message(Variant::Call, r"ROOT\MEDIA\0004");
        assert_eq!(
            text,
            r"The switch to call isn't finished: Windows needs a restart to finish installing ROOT\MEDIA\0004. Restart Windows, then run `noise-filter mic variant` to check its microphones."
        );
        let e = helper_result(7).unwrap_err();
        assert_eq!(e.exit, Exit::RestartPending);
        assert!(e.message.starts_with("The elevated switch isn't finished (exit code 7): Windows needs a restart"), "{}", e.message);
    }

    /// As setup-large-pages' (app-privilege F3): a helper that isn't elevated stops; a user's own run asks for elevation.
    #[test]
    fn a_helper_that_isnt_elevated_doesnt_relaunch_itself() {
        assert_eq!(next_step(false, true), Step::Refuse);
        assert_eq!(next_step(false, false), Step::Elevate);
        assert_eq!(next_step(true, true), Step::Switch);
        assert_eq!(next_step(true, false), Step::Switch);
        // Refusing touches neither devices nor UAC (checked where the test isn't elevated; elevated, it would switch, which
        // tests never do)
        if !privilege::is_elevated() {
            let args = MicVariantArgs { target: Some(Variant::Call), helper: true, report: None };
            let e = variant(&args).unwrap_err();
            assert_eq!((e.exit, e.message.as_str()), (Exit::NotElevated, NOT_ELEVATED));
        }
    }

    #[test]
    fn the_helpers_exit_codes_become_ours() {
        assert_eq!(helper_result(0), Ok(()));
        assert_eq!(helper_result(1).unwrap_err().exit, Exit::Usage);
        let e = helper_result(2).unwrap_err();
        assert_eq!(e.exit, Exit::Error);
        assert!(e.message.starts_with("The elevated switch failed (exit code 2): its steps are above"), "{}", e.message);
        assert_eq!(helper_result(4).unwrap_err(), Failure::not_elevated(NOT_ELEVATED));
        let e = helper_result(0xC000_0409).unwrap_err();
        assert_eq!(e.exit, Exit::Error);
        assert!(e.message.starts_with("The elevated switch stopped unexpectedly (exit code 0xC0000409)"), "{}", e.message);
    }
}
