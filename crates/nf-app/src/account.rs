//! Accounts for `setup-large-pages` (app-privilege F2): who is signed in to this desktop, and an
//! account's name for a SID.
//!
//! An elevated prompt can run as another account than the desktop's user: a standard user's UAC
//! prompt asks for an administrator's password ("over the shoulder"), and the elevated process is
//! that administrator. The privilege must go to the account that runs noise-filter, which is the
//! desktop's: the user Remote Desktop Services records for this session (`WTSQuerySessionInformationW`,
//! then `LookupAccountNameW`). That needs no access to another process. The fallback is the shell's
//! (Explorer's) token, which an over-the-shoulder admin usually can't open: Explorer's DACL admits
//! its own user, and an elevated token's SeDebugPrivilege is disabled (verify app-privilege F2).

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountNameW, LookupAccountSidW, PSID, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{HSTRING, Owned, PCWSTR, PWSTR};

/// The SID (S-1-5-21-...) of the user signed in to this desktop: the session's user, else the
/// shell's token's user. `Err` says why neither could be found (a service's session 0, Server Core
/// with no one signed in).
pub fn interactive_user_sid() -> Result<String, String> {
    session_user_sid().or_else(|why| shell_user_sid().map_err(|e| format!("{why}; {e}")))
}

/// The user Remote Desktop Services records for this session, as a SID.
fn session_user_sid() -> Result<String, String> {
    let user = session_string(WTSUserName)?;
    if user.is_empty() {
        return Err("no user is signed in to this session".into());
    }
    let domain = session_string(WTSDomainName).unwrap_or_default();
    let account = if domain.is_empty() { user } else { format!("{domain}\\{user}") };
    account_sid(&account)
}

/// One string about this session.
fn session_string(class: WTS_INFO_CLASS) -> Result<String, String> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: the out-pointers are live locals. On success `buf` is a NUL-terminated string that
    // Remote Desktop Services allocated; it is read, then freed once with WTSFreeMemory.
    unsafe {
        WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), WTS_CURRENT_SESSION, class, &mut buf, &mut bytes)
            .map_err(|e| format!("couldn't ask Windows who is signed in to this session ({})", win32_code(&e)))?;
        let s = buf.to_string();
        WTSFreeMemory(buf.0.cast());
        s.map_err(|e| format!("the session's user name isn't valid UTF-16 ({e})"))
    }
}

/// `DOMAIN\user` (or `user`) as a SID string.
fn account_sid(account: &str) -> Result<String, String> {
    let name = HSTRING::from(account);
    // The largest SID is 68 bytes; u64 elements keep it aligned.
    let mut sid = [0u64; 16];
    let mut sid_len = size_of_val(&sid) as u32;
    let mut domain = [0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: `name` is NUL-terminated and outlives the call; the SID and domain buffers are live
    // with their sizes passed (bytes for the SID, characters for the domain). On success the SID
    // in `sid` is valid for ConvertSidToStringSidW, whose string is LocalAlloc'd and freed once by
    // Owned<HLOCAL>.
    unsafe {
        let psid = PSID(sid.as_mut_ptr().cast());
        LookupAccountNameW(PCWSTR::null(), &name, Some(psid), &mut sid_len, Some(PWSTR(domain.as_mut_ptr())), &mut domain_len, &mut kind)
            .map_err(|e| format!("couldn't find the account {account} ({})", win32_code(&e)))?;
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(psid, &mut s).map_err(|e| format!("couldn't format the account's SID ({})", win32_code(&e)))?;
        let _free = Owned::new(HLOCAL(s.0.cast()));
        s.to_string().map_err(|e| format!("the account's SID isn't valid UTF-16 ({e})"))
    }
}

/// The shell's (Explorer's) token's user, as a SID.
fn shell_user_sid() -> Result<String, String> {
    // SAFETY: no arguments; it returns null when there is no shell window.
    let shell = unsafe { GetShellWindow() };
    if shell.is_invalid() {
        return Err("no desktop shell is running".into());
    }
    let mut pid = 0u32;
    // SAFETY: `shell` is a window handle (a stale one makes the call return 0); `pid` is a live
    // out-pointer.
    unsafe { GetWindowThreadProcessId(shell, Some(&mut pid)) };
    if pid == 0 {
        return Err("couldn't find the desktop shell's process".into());
    }
    let mut token = HANDLE::default();
    // SAFETY: plain arguments; on success the process handle is ours and Owned closes it once.
    // OpenProcessToken needs only PROCESS_QUERY_LIMITED_INFORMATION; `token` is a live out-pointer,
    // and the token is owned and closed once by Owned.
    let token = unsafe {
        let process = Owned::new(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|e| format!("couldn't open the desktop shell's process ({})", win32_code(&e)))?,
        );
        OpenProcessToken(*process, TOKEN_QUERY, &mut token)
            .map_err(|e| format!("couldn't read the desktop shell's account ({})", win32_code(&e)))?;
        Owned::new(token)
    };
    token_user_sid(*token)
}

/// A token's user as a SID string.
fn token_user_sid(token: HANDLE) -> Result<String, String> {
    // TOKEN_USER (16 B) plus the largest SID (68 B) fits in 128 B. u64 elements keep it aligned.
    let mut buf = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: `buf` is live, 8-aligned and its size is passed. On success it starts with a
    // TOKEN_USER whose Sid points into `buf`, which outlives every use below. The string from
    // ConvertSidToStringSidW is LocalAlloc'd and freed once by Owned<HLOCAL>.
    unsafe {
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast::<c_void>()), size_of_val(&buf) as u32, &mut len)
            .map_err(|e| format!("couldn't read the account from its token ({})", win32_code(&e)))?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(|e| format!("couldn't format the account's SID ({})", win32_code(&e)))?;
        let _free = Owned::new(HLOCAL(s.0.cast()));
        s.to_string().map_err(|e| format!("the account's SID isn't valid UTF-16 ({e})"))
    }
}

/// The Win32 error code in a windows-rs error, for messages.
fn win32_code(e: &windows::core::Error) -> u32 {
    windows::Win32::Foundation::WIN32_ERROR::from_error(e).map_or(e.code().0 as u32, |w| w.0)
}

/// `DOMAIN\user` for a SID, or `None` if Windows doesn't know it (or the SID isn't valid).
pub fn account_name(sid: &str) -> Option<String> {
    if sid.is_empty() || sid.contains('\0') {
        return None;
    }
    let wide = HSTRING::from(sid);
    let mut psid = PSID::default();
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the SID it gives is LocalAlloc'd and
    // freed once by Owned<HLOCAL>, after its last use. The name buffers are live and their lengths
    // (in characters) are passed; on success the lengths are the characters written, without the NUL.
    unsafe {
        ConvertStringSidToSidW(&wide, &mut psid).ok()?;
        let _free = Owned::new(HLOCAL(psid.0));
        LookupAccountSidW(
            PCWSTR::null(),
            psid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
        .ok()?;
    }
    let name = String::from_utf16_lossy(name.get(..name_len as usize)?);
    let domain = String::from_utf16_lossy(domain.get(..domain_len as usize)?);
    Some(if domain.is_empty() { name } else { format!("{domain}\\{name}") })
}

/// "PC\alice (S-1-5-21-...)", or the SID alone when it has no name.
pub fn describe(sid: &str) -> String {
    match account_name(sid) {
        Some(name) => format!("{name} ({sid})"),
        None => sid.to_owned(),
    }
}

/// Who an elevated `setup-large-pages` without `--sid` grants the privilege to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grantee {
    /// The elevated prompt runs as the desktop's user (UAC's Admin Approval Mode): that account.
    Same(String),
    /// The prompt runs as another account (an over-the-shoulder admin): the desktop's user, `sid`,
    /// is the one that runs noise-filter. `elevated` is the prompt's own account.
    DesktopUser { sid: String, elevated: String },
    /// The desktop's user couldn't be found: the prompt's own account, with a warning.
    Fallback { sid: String, why: String },
}

impl Grantee {
    pub fn sid(&self) -> &str {
        match self {
            Grantee::Same(sid) | Grantee::DesktopUser { sid, .. } | Grantee::Fallback { sid, .. } => sid,
        }
    }
}

/// Picks the account: the desktop's user when there is one, else this process's.
pub fn choose_grantee(own: String, desktop: Result<String, String>) -> Grantee {
    match desktop {
        Ok(sid) if sid.eq_ignore_ascii_case(&own) => Grantee::Same(own),
        // A shell running as a service account (a kiosk or an odd setup) isn't a person to grant
        Ok(sid) if is_service_account(&sid) => Grantee::Fallback { sid: own, why: format!("the desktop shell runs as {sid}") },
        Ok(sid) => Grantee::DesktopUser { sid, elevated: own },
        Err(why) => Grantee::Fallback { sid: own, why },
    }
}

/// LocalSystem, LocalService and NetworkService.
fn is_service_account(sid: &str) -> bool {
    matches!(sid.to_ascii_uppercase().as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// app-privilege F2: an elevated prompt running as another admin grants the desktop's user,
    /// not itself; with no desktop user to be found it falls back to itself, with a warning.
    #[test]
    fn the_grant_goes_to_the_desktops_user() {
        let alice = "S-1-5-21-1-2-3-1001".to_owned();
        let admin = "S-1-5-21-1-2-3-500".to_owned();
        assert_eq!(choose_grantee(alice.clone(), Ok(alice.to_lowercase())), Grantee::Same(alice.clone()));
        let g = choose_grantee(admin.clone(), Ok(alice.clone()));
        assert_eq!(g, Grantee::DesktopUser { sid: alice.clone(), elevated: admin.clone() });
        assert_eq!(g.sid(), alice);
        let g = choose_grantee(admin.clone(), Err("no desktop shell is running".into()));
        assert_eq!(g, Grantee::Fallback { sid: admin.clone(), why: "no desktop shell is running".into() });
        assert_eq!(g.sid(), admin);
        let g = choose_grantee(admin.clone(), Ok("S-1-5-18".into()));
        assert!(matches!(&g, Grantee::Fallback { sid, .. } if *sid == admin), "{g:?}");
    }

    /// Read-only lookups on this machine: the shell's user (if there is a shell) and this user's
    /// name.
    #[test]
    fn the_desktop_user_and_account_names_are_read() {
        match interactive_user_sid() {
            Ok(sid) => assert!(sid.starts_with("S-1-"), "{sid}"),
            Err(why) => assert!(!why.is_empty()),
        }
        let me = nf_mem::privilege::current_user_sid().unwrap();
        // The session's user is this test's user (it runs in the signed-in session, unelevated or
        // through Admin Approval Mode).
        if let Ok(sid) = session_user_sid() {
            assert!(sid.eq_ignore_ascii_case(&me), "{sid} vs {me}");
        }
        let name = account_name(&me).expect("this user has a name");
        assert!(!name.is_empty() && !name.starts_with("S-1-"), "{name}");
        assert!(describe(&me).ends_with(&format!("({me})")));
        // BUILTIN\Administrators always resolves; nonsense doesn't
        assert!(account_name("S-1-5-32-544").is_some_and(|n| n.contains('\\')));
        assert_eq!(account_name("not a sid"), None);
        assert_eq!(account_name(""), None);
        assert_eq!(describe("S-1-5-21-0-0-0-4242"), "S-1-5-21-0-0-0-4242");
    }
}
