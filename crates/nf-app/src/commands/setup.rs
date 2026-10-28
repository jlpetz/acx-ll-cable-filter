//! `setup-large-pages [--sid SID]`: grants `SeLockMemoryPrivilege`, which large pages need.
//!
//! Not elevated, it relaunches itself elevated (a UAC prompt) as
//! `setup-large-pages --sid <this user's SID> --elevated-helper` and reports the helper's exit
//! code. The SID is the non-elevated user's, so an over-the-shoulder admin prompt still grants the
//! account that runs the app. The window's "Enable large pages" button runs the same helper.
//!
//! Elevated, it grants the privilege through LSA to `--sid`. Without `--sid` (someone ran it from an
//! administrator terminal), it grants the user signed in to this desktop, which may not be the
//! elevated account (account.rs), and says which account it chose.
//!
//! The hidden `--elevated-helper` flag marks the relaunched helper. A helper that finds itself not
//! elevated (with UAC turned off, "runas" starts it as the same user) fails with
//! [`Exit::NotElevated`] instead of relaunching itself again, without end. `--sid` alone isn't the
//! marker: it's a user option, and a non-elevated `setup-large-pages --sid S-1-...` still asks for
//! elevation.

use nf_mem::privilege::{self, helper_args};

use crate::account::{self, Grantee};
use crate::exit::{Exit, Failure};

const SIGN_OUT: &str = "Sign out of Windows and back in, then restart noise-filter.";

/// What the helper says when it starts without elevation.
const NOT_ELEVATED: &str = "The large-page helper started without elevation (is UAC turned off?). \
     Run `noise-filter setup-large-pages` from an elevated (administrator) prompt instead.";

pub fn setup_large_pages(sid: Option<&str>, helper: bool) -> Result<(), Failure> {
    match next_step(privilege::is_elevated(), helper) {
        Step::Refuse => {
            log::warn!("the large-page helper started without elevation; not relaunching it");
            Err(Failure::not_elevated(NOT_ELEVATED))
        }
        Step::Grant => grant(sid),
        Step::Elevate => elevate(sid),
    }
}

/// What a `setup-large-pages` process does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Elevated: grant the privilege.
    Grant,
    /// Not elevated: relaunch elevated as the helper.
    Elevate,
    /// The helper, not elevated: stop, or it would relaunch itself without end.
    Refuse,
}

fn next_step(elevated: bool, helper: bool) -> Step {
    match (elevated, helper) {
        (true, _) => Step::Grant,
        (false, false) => Step::Elevate,
        (false, true) => Step::Refuse,
    }
}

fn own_sid() -> Result<String, Failure> {
    privilege::current_user_sid().map_err(|e| Failure::error(format!("Couldn't read this user's SID: {e}")))
}

/// Elevated: grants `sid`, or the desktop's user if there's no `sid`.
fn grant(sid: Option<&str>) -> Result<(), Failure> {
    let sid = match sid {
        Some(s) => s.to_owned(),
        None => {
            let grantee = account::choose_grantee(own_sid()?, account::interactive_user_sid());
            if let Some(note) = grantee_note(&grantee, &account::describe) {
                log::info!("{note}");
                say!("{note}");
            }
            grantee.sid().to_owned()
        }
    };
    let who = account::describe(&sid);
    privilege::grant_lock_memory_privilege(&sid)
        .map_err(|e| Failure::error(format!("Couldn't grant the lock-pages privilege to {who}: {e}")))?;
    log::info!("granted SeLockMemoryPrivilege to {who}");
    say!("The lock-pages privilege is granted to {who}.\n{SIGN_OUT}");
    Ok(())
}

/// What to tell the user about the account an elevated run without `--sid` chose. `None` when it's
/// simply this prompt's own account. `describe` names a SID ([`account::describe`]).
fn grantee_note(grantee: &Grantee, describe: &dyn Fn(&str) -> String) -> Option<String> {
    match grantee {
        Grantee::Same(_) => None,
        Grantee::DesktopUser { sid, elevated } => Some(format!(
            "note: this prompt runs as {}, but {} is signed in to this desktop and runs noise-filter, so the privilege goes to \
             that account. Pass --sid to choose another.",
            describe(elevated),
            describe(sid)
        )),
        Grantee::Fallback { sid, why } => Some(format!(
            "warning: couldn't tell who is signed in to this desktop ({why}), so the privilege goes to this prompt's account, {}. \
             If noise-filter runs as another user, run `noise-filter setup-large-pages` from that user's normal (non-admin) \
             terminal instead: it asks for elevation itself.",
            describe(sid)
        )),
    }
}

/// Not elevated: relaunches itself as the elevated helper, for `sid` or this user.
fn elevate(sid: Option<&str>) -> Result<(), Failure> {
    let sid = match sid {
        Some(s) => s.to_owned(),
        None => own_sid()?,
    };
    say!("Asking Windows for elevation to grant the lock-pages privilege to {}…", account::describe(&sid));
    let code = privilege::run_elevated(&helper_args(&sid)).map_err(Failure::error)?;
    log::info!("the elevated large-page setup exited with {code}");
    match helper_result(code) {
        Ok(()) => {
            say!("Large pages are set up.\n{SIGN_OUT}");
            Ok(())
        }
        Err(why) => Err(Failure::error(format!(
            "The elevated setup failed (exit code {code}: {why}). The log has the details: {}",
            crate::logger::default_path().display()
        ))),
    }
}

/// What the elevated helper's exit code means.
pub fn helper_result(code: u32) -> Result<(), &'static str> {
    match code {
        0 => Ok(()),
        c if c == u32::from(Exit::Usage.code()) => Err("it didn't accept its arguments"),
        c if c == u32::from(Exit::Error.code()) => Err("Windows refused the grant"),
        c if c == u32::from(Exit::NotElevated.code()) => {
            Err("it started without elevation; is UAC turned off? Run `noise-filter setup-large-pages` from an elevated prompt")
        }
        _ => Err("it stopped unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helpers_exit_codes_are_explained() {
        assert_eq!(helper_result(0), Ok(()));
        assert_eq!(helper_result(1), Err("it didn't accept its arguments"));
        assert_eq!(helper_result(2), Err("Windows refused the grant"));
        assert!(helper_result(4).unwrap_err().starts_with("it started without elevation"));
        assert_eq!(helper_result(0xC000_0409), Err("it stopped unexpectedly"));
    }

    /// app-privilege F3: a helper that isn't elevated stops, with no relaunch loop. A user's own
    /// non-elevated run, with or without --sid, still asks for elevation, and the relaunch marks
    /// itself as the helper.
    #[test]
    fn a_helper_that_isnt_elevated_doesnt_relaunch_itself() {
        assert_eq!(next_step(false, true), Step::Refuse);
        assert_eq!(next_step(false, false), Step::Elevate);
        assert_eq!(next_step(true, true), Step::Grant);
        assert_eq!(next_step(true, false), Step::Grant);
        let args = helper_args("S-1-5-21-1-2-3-1001");
        assert_eq!(args, ["setup-large-pages", "--sid", "S-1-5-21-1-2-3-1001", "--elevated-helper"]);
        // The relaunched command line parses back as the helper
        let parsed = crate::cli::parse(&args.map(String::from));
        assert_eq!(parsed, Ok(crate::cli::Command::SetupLargePages { sid: Some(args[2].into()), helper: true }));
        // Refusing touches neither LSA nor UAC: it's only the failure (checked where the test
        // process isn't elevated; elevated, the command would grant, which tests never do)
        if !privilege::is_elevated() {
            let e = setup_large_pages(Some(args[2]), true).unwrap_err();
            assert_eq!((e.exit, e.message.as_str()), (Exit::NotElevated, NOT_ELEVATED));
        }
    }

    /// app-privilege F2: the account chosen without --sid is named, with a note when it isn't the
    /// prompt's own account and a warning when the desktop's user couldn't be found.
    #[test]
    fn the_chosen_account_is_named() {
        let name = |sid: &str| format!("PC\\user{} ({sid})", sid.rsplit('-').next().unwrap_or(""));
        assert_eq!(grantee_note(&Grantee::Same("S-1-5-21-1-2-3-1001".into()), &name), None);
        let note = grantee_note(&Grantee::DesktopUser { sid: "S-1-5-21-1-2-3-1001".into(), elevated: "S-1-5-21-1-2-3-500".into() }, &name)
            .unwrap();
        assert!(note.starts_with("note: this prompt runs as PC\\user500 (S-1-5-21-1-2-3-500), but PC\\user1001"), "{note}");
        let warning =
            grantee_note(&Grantee::Fallback { sid: "S-1-5-21-1-2-3-500".into(), why: "no desktop shell is running".into() }, &name)
                .unwrap();
        assert!(warning.starts_with("warning: couldn't tell who is signed in to this desktop (no desktop shell is running)"), "{warning}");
        assert!(warning.contains("PC\\user500 (S-1-5-21-1-2-3-500)") && warning.contains("normal (non-admin) terminal"), "{warning}");
    }
}
