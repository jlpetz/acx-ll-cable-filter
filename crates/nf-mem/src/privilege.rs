//! The lock-pages privilege. Adapted from `TMR-APP/src/memory/privileges.rs` (the token half);
//! the grant uses `LsaAddAccountRights` instead of TMR's secedit round trip, and goes to the SID
//! the non-elevated parent passes in, never the elevated process's own account
//! (`doc/todo/6-inference-memory.md`).
//!
//! Nothing here runs on the audio path: these calls allocate and format freely.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    ERROR_CANCELLED, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, ERROR_NOT_ALL_ASSIGNED, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, HLOCAL,
    HWND, LUID, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authentication::Identity::{
    LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaNtStatusToWinError, LsaOpenPolicy,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, IsValidSid, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PSID, SE_LOCK_MEMORY_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_INFORMATION_CLASS,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenElevationType, TokenElevationTypeLimited, TokenUser,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::core::{HSTRING, Owned, PCWSTR, PWSTR, w};

use crate::region::win32_code;

/// Why the privilege couldn't be enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivilegeError {
    /// The account doesn't hold it (or the UAC-filtered token dropped it); run setup, then sign out and in.
    NotHeld,
    Other(String),
}

/// `context (code: system message)`, for errors shown to the user.
fn describe(context: &str, e: &windows::core::Error) -> String {
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() { format!("{context} ({})", win32_code(e)) } else { format!("{context} ({}: {msg})", win32_code(e)) }
}

/// This process's token, closed on drop.
fn open_token(access: TOKEN_ACCESS_MASK) -> Result<Owned<HANDLE>, String> {
    let mut h = HANDLE::default();
    // SAFETY: GetCurrentProcess is a pseudo-handle; `h` is a live out-pointer. On success we own
    // the token handle, and Owned closes it once.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), access, &mut h).map_err(|e| describe("couldn't open this process's token", &e))?;
        Ok(Owned::new(h))
    }
}

/// Reads a fixed-size token information class into `T`.
fn token_info<T: Default>(class: TOKEN_INFORMATION_CLASS) -> Result<T, String> {
    let token = open_token(TOKEN_QUERY)?;
    let mut out = T::default();
    let mut len = 0u32;
    // SAFETY: `out` is a live, aligned `T` of exactly the size passed; the caller pairs each class
    // with the struct it returns.
    unsafe {
        GetTokenInformation(*token, class, Some((&raw mut out).cast::<c_void>()), size_of::<T>() as u32, &mut len)
            .map_err(|e| describe("couldn't read this process's token", &e))?;
    }
    Ok(out)
}

/// Enables privilege `name` in this process's token. `Ok(false)`: the token doesn't hold it.
pub(crate) fn enable_privilege(name: PCWSTR) -> Result<bool, String> {
    let token = open_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?;
    let mut luid = LUID::default();
    // SAFETY: `name` is a NUL-terminated privilege name (the SE_*_NAME constants); `luid` is a
    // live out-pointer.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) }.map_err(|e| describe("unknown privilege", &e))?;
    let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
    // SAFETY: the token was opened with TOKEN_ADJUST_PRIVILEGES; `tp` outlives the call; no
    // previous state is requested, so the buffer length may be 0. GetLastError is read straight
    // after, with no call in between: AdjustTokenPrivileges succeeds even when nothing was
    // assigned and reports that only through the last error.
    unsafe {
        AdjustTokenPrivileges(*token, false, Some(&tp), 0, None, None)
            .map_err(|e| describe("couldn't adjust this process's privileges", &e))?;
        Ok(GetLastError() != ERROR_NOT_ALL_ASSIGNED)
    }
}

/// Enables `SeLockMemoryPrivilege` in this process's token (needs no admin if the account holds it).
pub fn enable_lock_memory_privilege() -> Result<(), PrivilegeError> {
    match enable_privilege(SE_LOCK_MEMORY_NAME) {
        Ok(true) => Ok(()),
        Ok(false) => Err(PrivilegeError::NotHeld),
        Err(e) => Err(PrivilegeError::Other(e)),
    }
}

/// The interactive user's SID as a string (S-1-5-21-...), from this process's token.
pub fn current_user_sid() -> Result<String, String> {
    let token = open_token(TOKEN_QUERY)?;
    // TOKEN_USER (16 B) plus the largest SID (SECURITY_MAX_SID_SIZE, 68 B) fits in 128 B, so no
    // size probe is needed. u64 elements keep the buffer aligned for TOKEN_USER.
    let mut buf = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: `buf` is live, 8-aligned and its size is passed; on success it starts with a
    // TOKEN_USER whose Sid points into `buf`, which outlives every use below. The string from
    // ConvertSidToStringSidW is LocalAlloc'd and freed once by Owned<HLOCAL>.
    unsafe {
        GetTokenInformation(*token, TokenUser, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len)
            .map_err(|e| describe("couldn't read this process's user", &e))?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(|e| describe("couldn't format the user's SID", &e))?;
        let _free = Owned::new(HLOCAL(s.0.cast()));
        s.to_string().map_err(|e| format!("the user's SID isn't valid UTF-16 ({e})"))
    }
}

/// Whether this process is elevated.
pub fn is_elevated() -> bool {
    token_info::<TOKEN_ELEVATION>(TokenElevation).is_ok_and(|e| e.TokenIsElevated != 0)
}

/// Whether this is the filtered half of a split admin token (UAC Admin Approval Mode), which may
/// lack privileges the account holds. Worded into the fallback reason.
pub(crate) fn is_uac_filtered() -> bool {
    token_info::<TOKEN_ELEVATION_TYPE>(TokenElevationType).is_ok_and(|t| t == TokenElevationTypeLimited)
}

/// A binary SID from its string form, LocalFree'd on drop.
struct Sid {
    psid: PSID,
    _mem: Owned<HLOCAL>,
}

fn parse_sid(sid: &str) -> Result<Sid, String> {
    let invalid = || format!("not a valid SID: {sid:?}");
    if sid.is_empty() || sid.contains('\0') {
        return Err(invalid());
    }
    let wide = HSTRING::from(sid);
    let mut psid = PSID::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `psid` is a live out-pointer. On
    // success the SID is LocalAlloc'd and freed once by Owned<HLOCAL>; `psid` is only used while
    // the returned Sid (which owns it) lives.
    unsafe {
        ConvertStringSidToSidW(&wide, &mut psid).map_err(|_| invalid())?;
        let mem = Owned::new(HLOCAL(psid.0));
        if !IsValidSid(psid).as_bool() {
            return Err(invalid());
        }
        Ok(Sid { psid, _mem: mem })
    }
}

/// Grants `SeLockMemoryPrivilege` to `sid` with `LsaAddAccountRights`. Needs elevation. Takes effect
/// at that account's next sign-in.
pub fn grant_lock_memory_privilege(sid: &str) -> Result<(), String> {
    let sid = parse_sid(sid)?;
    let attrs = LSA_OBJECT_ATTRIBUTES::default(); // unused members, zeroed (MS docs)
    let mut policy = LSA_HANDLE::default();
    // SAFETY: `attrs` and `policy` are live; on success we own the policy handle and Owned closes
    // it once (LsaClose).
    let policy = unsafe {
        let st = LsaOpenPolicy(None, &attrs, (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32, &mut policy);
        if st.is_err() {
            let code = LsaNtStatusToWinError(st);
            return Err(if code == 5 {
                "couldn't open the local security policy (5: access denied; this step must run elevated)".into()
            } else {
                format!("couldn't open the local security policy ({code})")
            });
        }
        Owned::new(policy)
    };
    // SAFETY: SE_LOCK_MEMORY_NAME is a static NUL-terminated literal.
    let units = unsafe { SE_LOCK_MEMORY_NAME.len() };
    let right = LSA_UNICODE_STRING {
        Length: (units * 2) as u16, // bytes, without the NUL
        MaximumLength: ((units + 1) * 2) as u16,
        Buffer: PWSTR(SE_LOCK_MEMORY_NAME.0.cast_mut()), // LSA only reads it
    };
    // SAFETY: the policy handle is open with POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES; the SID
    // and the right's buffer outlive the call.
    let code = unsafe { LsaNtStatusToWinError(LsaAddAccountRights(*policy, sid.psid, &[right])) };
    if code == 0 { Ok(()) } else { Err(format!("couldn't grant the lock-pages privilege ({code})")) }
}

/// The hidden flag that marks `setup-large-pages` relaunched elevated, so a helper that starts
/// without elevation (UAC off) exits with [`HELPER_NOT_ELEVATED`] instead of relaunching itself
/// (app-privilege F3). One definition for the app's CLI and the window's button: a mismatch would
/// bring the endless relaunch back.
pub const HELPER_FLAG: &str = "--elevated-helper";

/// The helper's exit code when it started without elevation (nf-app's `Exit::NotElevated`).
pub const HELPER_NOT_ELEVATED: u32 = 4;

/// The elevated helper's arguments: `setup-large-pages --sid <sid> --elevated-helper`.
pub fn helper_args(sid: &str) -> [&str; 4] {
    ["setup-large-pages", "--sid", sid, HELPER_FLAG]
}

/// Relaunches this exe elevated (UAC prompt, `runas`) with `args`, waits, and returns its exit code.
///
/// The helper starts in the exe's folder, never the caller's current directory. An elevated process
/// runs in another logon session, which doesn't see the caller's mapped network drives or `subst`
/// drives, so a current directory on one of those fails the launch with 267 ("The directory name
/// is invalid"). The exe's folder has to resolve elevated anyway, since the exe is started from it.
pub fn run_elevated(args: &[&str]) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("couldn't find this program's path ({e})"))?;
    let dir = helper_dir(&exe);
    let params = join_args(args);
    // A thread of our own: ShellExecuteEx wants an STA with COM initialised, which the caller's
    // thread may not be (a WASAPI thread is MTA), and the wait blocks.
    std::thread::Builder::new()
        .name("nf-elevate".into())
        .spawn(move || shell_runas(&exe, &params, dir.as_deref()))
        .map_err(|e| format!("couldn't start the elevation thread ({e})"))?
        .join()
        .map_err(|_| "the elevation thread panicked".to_string())?
}

/// The elevated helper's working directory: the exe's folder (see [`run_elevated`]).
fn helper_dir(exe: &Path) -> Option<PathBuf> {
    exe.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf)
}

/// What a failed elevated launch says, for the codes the user can act on. `None`: the system's own
/// message will do.
fn launch_error(code: u32, dir: Option<&Path>) -> Option<String> {
    const NOT_LOCAL: &str = "Elevated programs don't see mapped network drives or subst drives: copy noise-filter to a \
         local folder (such as C:\\Program Files\\noise-filter) and try again.";
    let folder = || dir.map_or_else(|| "noise-filter's folder".to_owned(), |d| d.display().to_string());
    match code {
        c if c == ERROR_CANCELLED.0 => Some(format!("the elevation prompt was declined ({c})")),
        c if c == ERROR_DIRECTORY.0 => {
            Some(format!("couldn't start the elevated helper ({c}): as an administrator, Windows can't reach {}. {NOT_LOCAL}", folder()))
        }
        c if c == ERROR_FILE_NOT_FOUND.0 || c == ERROR_PATH_NOT_FOUND.0 => Some(format!(
            "couldn't start the elevated helper ({c}): as an administrator, Windows can't find noise-filter in {}. {NOT_LOCAL}",
            folder()
        )),
        _ => None,
    }
}

/// Pairs CoInitializeEx with CoUninitialize on this thread.
struct ComApartment(bool);

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: CoInitializeEx succeeded on this thread (S_OK or S_FALSE), so this balances it.
            unsafe { CoUninitialize() };
        }
    }
}

fn shell_runas(exe: &Path, params: &str, dir: Option<&Path>) -> Result<u32, String> {
    let exe_w = HSTRING::from(exe);
    let params_w = HSTRING::from(params);
    let dir_w = dir.map(HSTRING::from);
    // SAFETY: no reserved pointer; this thread is ours and hasn't chosen an apartment.
    let _com = ComApartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // FLAG_NO_UI: the error comes back here (worded by `launch_error`) instead of shell32 also
        // showing a dialog. The UAC prompt isn't an error dialog, and still shows.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND::default(),
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        lpDirectory: dir_w.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        nShow: 0, // SW_HIDE: the child is a console subcommand with nothing to show
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised with its size; the strings it points to outlive the
    // call. With SEE_MASK_NOCLOSEPROCESS the process handle (if any) is ours to close.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| launch_error(win32_code(&e), dir).unwrap_or_else(|| describe("couldn't start the elevated helper", &e)))?;
    if info.hProcess.is_invalid() {
        return Err("the elevated helper didn't start".into());
    }
    // SAFETY: a real process handle (checked above), owned by us and closed once by Owned.
    let child = unsafe { Owned::new(info.hProcess) };
    // SAFETY: a valid process handle; INFINITE: the helper only grants a right and exits.
    if unsafe { WaitForSingleObject(*child, INFINITE) } != WAIT_OBJECT_0 {
        // SAFETY: read straight after the failed wait.
        return Err(format!("couldn't wait for the elevated helper ({})", unsafe { GetLastError() }.0));
    }
    let mut code = 0u32;
    // SAFETY: a valid process handle; `code` is a live out-pointer.
    unsafe { GetExitCodeProcess(*child, &mut code) }.map_err(|e| describe("couldn't read the helper's exit code", &e))?;
    Ok(code)
}

/// Joins `args` into one command line that `CommandLineToArgvW` (and Rust's `std::env::args`)
/// splits back into the same strings.
fn join_args(args: &[&str]) -> String {
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        push_quoted(&mut out, arg);
    }
    out
}

fn push_quoted(out: &mut String, arg: &str) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are escaped, and so is the quote.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are escaped.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    // Read-only calls only. Never grant_lock_memory_privilege or run_elevated here.

    #[test]
    fn elevation_reads_without_failing() {
        let elevated = is_elevated();
        let filtered = is_uac_filtered();
        // A filtered token is by definition not elevated.
        assert!(!(elevated && filtered));
    }

    #[test]
    fn current_user_sid_is_a_sid_string() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(sid.split('-').skip(1).all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())), "{sid}");
        // It round-trips through the parser grant_lock_memory_privilege uses.
        parse_sid(&sid).unwrap();
    }

    #[test]
    fn enabling_lock_pages_is_ok_or_not_held() {
        match enable_lock_memory_privilege() {
            Ok(()) | Err(PrivilegeError::NotHeld) => {}
            Err(PrivilegeError::Other(e)) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn enabling_the_working_set_privilege_works() {
        // Every account holds SeIncreaseWorkingSetPrivilege by default.
        assert_eq!(enable_privilege(windows::Win32::Security::SE_INC_WORKING_SET_NAME), Ok(true));
    }

    #[test]
    fn unknown_privileges_are_errors() {
        assert!(enable_privilege(w!("SeNoSuchPrivilege")).is_err());
    }

    #[test]
    fn bad_sids_are_rejected() {
        for bad in ["", "S-1", "not a sid", "S-1-5-21-1\0", "S-1-5-x"] {
            assert!(parse_sid(bad).is_err(), "{bad:?}");
        }
        parse_sid("S-1-5-32-544").unwrap(); // BUILTIN\Administrators
        parse_sid("S-1-12-1-1-2-3-4").unwrap(); // an Entra ID shape: no S-1-5-21 restriction
    }

    #[test]
    fn args_are_quoted_for_command_line_to_argv() {
        assert_eq!(join_args(&[]), "");
        assert_eq!(join_args(&["setup-large-pages", "--sid", "S-1-5-21-1-2-3-1001"]), "setup-large-pages --sid S-1-5-21-1-2-3-1001");
        assert_eq!(join_args(&["a b", ""]), r#""a b" """#);
        assert_eq!(join_args(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(join_args(&[r"C:\dir with space\"]), r#""C:\dir with space\\""#);
        assert_eq!(join_args(&[r"C:\plain\path"]), r"C:\plain\path");
        assert_eq!(join_args(&[r#"a\\"b"#]), r#""a\\\\\"b""#);
    }

    /// app-privilege F6: the helper starts in the exe's folder, which resolves elevated, never in
    /// the caller's current directory (a mapped drive there failed the launch with 267).
    #[test]
    fn the_helper_starts_in_the_exes_folder() {
        assert_eq!(helper_dir(Path::new(r"C:\Program Files\nf\noise-filter.exe")), Some(PathBuf::from(r"C:\Program Files\nf")));
        assert_eq!(helper_dir(Path::new("noise-filter.exe")), None, "no folder: ShellExecuteEx's default");
        let exe = std::env::current_exe().unwrap();
        let dir = helper_dir(&exe).unwrap();
        assert_eq!(Some(dir.as_path()), exe.parent());
        // Whatever the caller's current directory is, it isn't used
        let elsewhere = std::env::temp_dir();
        assert_ne!(dir, elsewhere);
    }

    #[test]
    fn launch_errors_the_user_can_act_on_are_worded() {
        let dir = Path::new(r"Z:\tools\nf");
        let e = launch_error(267, Some(dir)).unwrap();
        assert!(e.starts_with(r"couldn't start the elevated helper (267): as an administrator, Windows can't reach Z:\tools\nf."), "{e}");
        assert!(e.contains("mapped network drives") && e.contains("local folder"), "{e}");
        assert!(launch_error(267, None).unwrap().contains("noise-filter's folder"));
        assert!(launch_error(3, Some(dir)).unwrap().contains(r"can't find noise-filter in Z:\tools\nf"));
        assert!(launch_error(2, Some(dir)).is_some());
        assert_eq!(launch_error(1223, None).as_deref(), Some("the elevation prompt was declined (1223)"));
        assert_eq!(launch_error(5, Some(dir)), None, "the system's message");
    }

    /// What the elevated child will see: the line split by `CommandLineToArgvW`.
    fn split(line: &str) -> Vec<String> {
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let wide = HSTRING::from(format!("x.exe {line}"));
        let mut n = 0i32;
        // SAFETY: `wide` is NUL-terminated; the returned array is one LocalAlloc block, freed once
        // by Owned<HLOCAL> after its strings are copied out.
        unsafe {
            let argv = CommandLineToArgvW(&wide, &mut n);
            assert!(!argv.is_null());
            let _free = Owned::new(HLOCAL(argv.cast()));
            (1..n as usize).map(|i| (*argv.add(i)).to_string().unwrap()).collect()
        }
    }

    #[test]
    fn quoted_args_round_trip_through_command_line_to_argv() {
        let cases: &[&[&str]] = &[
            &["setup-large-pages", "--sid", "S-1-5-21-1-2-3-1001"],
            &["a b", "", "c"],
            &[r#"say "hi""#, r"C:\dir with space\", r"C:\plain\path"],
            &[r#"a\\"b"#, "\t", r#"\""#, r"\\", "trailing\\"],
        ];
        for args in cases {
            assert_eq!(split(&join_args(args)), *args, "{}", join_args(args));
        }
    }
}
