//! `mic sessions` and `mic variant` (TODO 19): the virtual microphone driver's two microphones, which apps use which, and
//! the driver's variant (both microphones, the call one only, the low-latency one only). The work is nf-vmic's
//! (`sessions`, `variant`); this is the printing and the elevation.
//!
//! `mic variant` acts on one cable (TODO 19, the per-cable design): A, until N1 gives it `--cable`. The switch is nf-vmic's
//! (`variant::switch`, on `setup`): it touches that cable's nodes alone, re-creates its node with the next instance number
//! and the package the cables run, and checks that cable's endpoints by its instance ID.
//!
//! `mic variant X`, not elevated, first reads the cables and their endpoints (any user may: to be confirmed live from a
//! standard user's prompt). The cable already X, working, with its endpoints: it does nothing. One that can't be switched
//! (absent, a setup error, or the old driver's devices waiting for the migration) stops with exit 2 and no prompt. Otherwise
//! it relaunches itself elevated, one UAC prompt, as `mic variant X --elevated-helper --report PIPE`, waits, and exits with
//! the helper's code (3 if the prompt was declined). The helper reports each step through the named pipe (report_pipe.rs)
//! and this side prints them as they come. Elevated already (an administrator terminal), it switches in-process. Either way
//! the switch holds the setup's lock, and succeeds only once the new node works on the chosen package and Windows has made
//! its endpoints; one Windows must restart to finish exits 7, unchecked (the owner, 2026-11-23). The helper never opens or
//! attaches a console (main.rs); one that finds itself not elevated (UAC turned off) stops with exit code 4 instead of
//! relaunching itself, as setup-large-pages' helper does.

use std::fmt::Write as _;
use std::time::Duration;

use nf_mem::privilege::{self, ElevationError};
use nf_vmic::identity::Label;
use nf_vmic::sessions::{self, Endpoint, Fit, Mic, Session};
use nf_vmic::setup;
use nf_vmic::variant::{self, Outcome, Precheck, Variant};

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
            "No ACX-LL-Cable microphone is active. `noise-filter mic variant` says whether the driver is installed, and \
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

/// The cable `mic variant` acts on: A, until N1 gives it `--cable` (TODO 19, the per-cable design, section 10).
fn cable() -> Label {
    Label::a()
}

/// `mic variant`: the cable's variant, its device and its endpoints.
fn show() -> Result<(), Failure> {
    let shown = variant::present(&cable()).map_err(|e| Failure::error(format!("Couldn't read the cables: {e}")))?;
    say!("{}", variant::format_shown(&shown).trim_end());
    Ok(())
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
    match variant::switch(&cable(), target, &mut step) {
        Ok(Outcome::Nothing) => Ok(()),
        Ok(Outcome::Switched) => {
            step("Done: the cable works on the package the cables run, with its endpoints.");
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

/// A switch Windows must restart to finish (exit code 7; the owner, 2026-11-23).
fn restart_message(target: Variant, device: &str) -> String {
    format!(
        "The switch to {} isn't finished: Windows needs a restart to finish installing {device}. Restart Windows, then run \
         `noise-filter mic variant` to check its endpoints.",
        target.arg()
    )
}

/// Not elevated: looks first (nothing to do, or something that can't be done, needs no prompt), then relaunches itself
/// elevated as the helper, prints its report, and exits with its code.
fn elevate(target: Variant) -> Result<(), Failure> {
    let r = setup::read_report().map_err(|e| Failure::error(format!("Couldn't read the cables: {e}")))?;
    match variant::precheck(&cable(), target, &r.store.entries, &r.nodes, &r.endpoints) {
        Precheck::Nothing(lines) => {
            for line in lines {
                say!("{line}");
            }
            return Ok(());
        }
        Precheck::Refused(e) => return Err(Failure::error(format!("The switch to {} stopped: {e}", target.arg()))),
        Precheck::Unchecked(e) => say!("note: the endpoints couldn't be read here ({e}); the elevated switch reads them again."),
        Precheck::Switch(Some(note)) => say!("{note}"),
        Precheck::Switch(None) => {}
    }
    say!(
        "Switching {} to {} ({}). Windows will ask for permission now (a UAC prompt): re-creating a cable's device needs an \
         administrator. Its endpoints are new afterwards; the other cables are left alone.",
        cable().friendly_name(),
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
    if let Ok(shown) = variant::present(&cable()) {
        say!("{}", variant::format_shown(&shown).trim_end());
    }
    Ok(())
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
             Restart Windows, then run `noise-filter mic variant` to check its endpoints.",
        )),
        c => Err(Failure::error(format!("The elevated switch stopped unexpectedly (exit code 0x{c:X}). The log may say why: {}", log()))),
    }
}

#[cfg(test)]
mod tests {
    use nf_vmic::sessions::SessionState;
    use nf_vmic::variant::{CALL_MICROPHONE, MICROPHONE};
    use nf_vmic::win_error::WinError;

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
        assert!(format_sessions(&[]).starts_with("No ACX-LL-Cable microphone is active."));
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
        let renamed = endpoint("Studio mic (ACX-LL-Cable A)", vec![session("zoom.exe", SessionState::Active)]);
        assert!(format_sessions(&[renamed]).contains("  zoom.exe (pid 1234), active: a call app; the Call Microphone suits it."));
    }

    #[test]
    fn a_switch_windows_must_restart_to_finish_isnt_done() {
        let text = restart_message(Variant::Call, r"ROOT\MEDIA\0004");
        assert_eq!(
            text,
            r"The switch to call isn't finished: Windows needs a restart to finish installing ROOT\MEDIA\0004. Restart Windows, then run `noise-filter mic variant` to check its endpoints."
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
