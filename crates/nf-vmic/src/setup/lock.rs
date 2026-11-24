//! The setup's lock (moved from variant.rs, where it guarded the variant switch; now every setup action takes it): a mutex
//! in a private namespace bounded by BUILTIN\Administrators (`CreateBoundaryDescriptorW` + `AddSIDToBoundaryDescriptor`, then
//! `CreatePrivateNamespaceW`, or `OpenPrivateNamespaceW` when another setup made it), whose security descriptor lets
//! Administrators and SYSTEM in. A process that isn't an elevated administrator's can neither make nor open it, so it can't
//! hold the lock to block the setup, as any process could a `Global\` mutex; it gets [`LockError::Denied`] itself. A second
//! setup waits up to [`LOCK_WAIT`], then stops; one whose holder's process ended mid-action (an abandoned mutex) goes on,
//! reading afresh.

use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, HLOCAL, LocalFree,
    WAIT_ABANDONED, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{
    CreateWellKnownSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, WELL_KNOWN_SID_TYPE,
    WinBuiltinAdministratorsSid,
};
use windows::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexExW, CreatePrivateNamespaceW,
    DeleteBoundaryDescriptor, MUTEX_MODIFY_STATE, OpenPrivateNamespaceW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{HSTRING, Owned};

use crate::win_error::{WinError, win32_code};

/// How long a setup action waits for another to end: one takes a few seconds, an update or a variant switch up to about 30.
pub const LOCK_WAIT: Duration = Duration::from_secs(60);

/// Why the lock wasn't taken. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockError {
    /// Another setup held it for the whole wait.
    Busy,
    /// The namespace or mutex refused this process: it isn't an elevated administrator's.
    Denied(WinError),
    Windows(WinError),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Busy => write!(
                f,
                "another setup action has held the setup's lock for {} s: let it end, then run this one again (it reads the devices \
                 afresh). Nothing was changed.",
                LOCK_WAIT.as_secs()
            ),
            LockError::Denied(e) => write!(
                f,
                "the setup's lock refused this process ({e}): it is for elevated administrators, so run it from an administrator \
                 prompt, or approve its UAC prompt. Nothing was changed."
            ),
            LockError::Windows(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for LockError {}

impl From<WinError> for LockError {
    fn from(e: WinError) -> Self {
        LockError::Windows(e)
    }
}

/// How a wait for the lock ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockWait {
    Taken,
    /// Its last holder's thread ended without releasing it: ours now, as if taken.
    Abandoned,
    /// Still held by another at the end of the wait.
    Busy,
    Failed,
}

fn lock_wait(event: WAIT_EVENT) -> LockWait {
    match event {
        e if e == WAIT_OBJECT_0 => LockWait::Taken,
        e if e == WAIT_ABANDONED => LockWait::Abandoned,
        e if e == WAIT_TIMEOUT => LockWait::Busy,
        _ => LockWait::Failed,
    }
}

/// Where a lock lives: a mutex in a private namespace (`CreatePrivateNamespaceW`), which only a caller within its boundary
/// may create or open (its token must hold the boundary's SID), so no other process can squat the name.
#[derive(Clone, Copy, Debug)]
pub struct LockSpace<'a> {
    /// The boundary descriptor's name: with [`sid`](Self::sid), what identifies the namespace.
    pub boundary: &'a str,
    /// The SID a caller's token must hold.
    pub sid: WELL_KNOWN_SID_TYPE,
    /// The namespace's and the mutex's security descriptor, in SDDL.
    pub sddl: &'a str,
    /// The start of the namespace's prefix in this process ([`fresh_alias`]): the mutex is `prefix\mutex`.
    pub alias: &'a str,
    pub mutex: &'a str,
}

/// The setup's lock: bounded by BUILTIN\Administrators (an elevated administrator, or SYSTEM, is within it), and open to
/// Administrators and SYSTEM, whichever account's setup made it.
pub const LOCK: LockSpace<'static> = LockSpace {
    boundary: "acx-ll-cable-setup",
    sid: WinBuiltinAdministratorsSid,
    sddl: "D:P(A;;GA;;;BA)(A;;GA;;;SY)",
    alias: "acx-ll-cable",
    mutex: "setup",
};

/// How often a take looks for the namespace again when it went away between a create that found it and the open.
const NAMESPACE_TRIES: usize = 4;

/// A boundary descriptor, deleted on drop.
struct Boundary(HANDLE);

impl Drop for Boundary {
    fn drop(&mut self) {
        // SAFETY: our descriptor, deleted once.
        unsafe { DeleteBoundaryDescriptor(self.0) };
    }
}

/// An open private namespace, closed on drop but not destroyed (a setup waiting in it still uses it).
struct Namespace(HANDLE);

impl Drop for Namespace {
    fn drop(&mut self) {
        // SAFETY: our namespace handle, closed once; no PRIVATE_NAMESPACE_FLAG_DESTROY.
        let _ = unsafe { ClosePrivateNamespace(self.0, 0) };
    }
}

/// A security descriptor made from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn from_sddl(sddl: &str) -> Result<Self, WinError> {
        let sddl = HSTRING::from(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a NUL-terminated string; `sd` is a live out-pointer, freed by Drop.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
            .map_err(WinError::of("ConvertStringSecurityDescriptorToSecurityDescriptorW"))?;
        Ok(Self(sd))
    }

    /// Attributes that point at it: valid while it lives.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor ConvertStringSecurityDescriptorToSecurityDescriptorW allocated with LocalAlloc, freed once.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// A well-known SID, in a buffer aligned for one.
pub(crate) fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u32>, WinError> {
    let mut sid = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())];
    let mut size = (sid.len() * size_of::<u32>()) as u32;
    // SAFETY: no domain; the buffer holds `size` bytes and is aligned for a SID; `size` is a live in-out pointer.
    unsafe { CreateWellKnownSid(kind, None, Some(PSID(sid.as_mut_ptr().cast())), &mut size) }
        .map_err(WinError::of("CreateWellKnownSid"))?;
    Ok(sid)
}

/// A lock call that failed with `code`: access denied is a caller outside the boundary, or one the descriptor keeps out.
fn lock_error(call: &'static str, code: u32) -> LockError {
    let e = WinError { call, code };
    if code == ERROR_ACCESS_DENIED.0 { LockError::Denied(e) } else { LockError::Windows(e) }
}

/// A namespace prefix this process hasn't used yet (`acx-ll-cable-0`): Windows refuses a process a second open of a namespace
/// under a prefix it has open (ERROR_DUP_NAME), as two takes in one process would be (the tests' threads).
fn fresh_alias(start: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("{start}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The last error, for a call that failed.
fn last_error() -> u32 {
    // SAFETY: no arguments; the callers read it straight after the failed call.
    unsafe { GetLastError() }.0
}

impl Namespace {
    /// Creates the namespace, or opens it if it's there (another setup's).
    fn open(space: &LockSpace, alias: &str, sd: &Descriptor) -> Result<Self, LockError> {
        let name = HSTRING::from(space.boundary);
        // SAFETY: a NUL-terminated name; no flags. Null on failure.
        let mut boundary = Boundary(unsafe { CreateBoundaryDescriptorW(&name, 0) });
        if boundary.0.is_invalid() {
            return Err(WinError { call: "CreateBoundaryDescriptorW", code: last_error() }.into());
        }
        let mut sid = well_known_sid(space.sid)?;
        // SAFETY: our descriptor, which the call may reallocate through the pointer; a SID that outlives the call (it's copied).
        unsafe { AddSIDToBoundaryDescriptor(&mut boundary.0, PSID(sid.as_mut_ptr().cast())) }
            .map_err(WinError::of("AddSIDToBoundaryDescriptor"))?;
        let alias = HSTRING::from(alias);
        let attributes = sd.attributes();
        let descriptor = boundary.0.0.cast_const();
        for _ in 0..NAMESPACE_TRIES {
            // SAFETY: attributes whose descriptor lives in `sd`; our boundary descriptor; a NUL-terminated prefix. Null on failure.
            let created = unsafe { CreatePrivateNamespaceW(Some(&attributes), descriptor, &alias) };
            if !created.is_invalid() {
                return Ok(Self(created));
            }
            let code = last_error();
            if code != ERROR_ALREADY_EXISTS.0 {
                return Err(lock_error("CreatePrivateNamespaceW", code));
            }
            // SAFETY: as above.
            let opened = unsafe { OpenPrivateNamespaceW(descriptor, &alias) };
            if !opened.is_invalid() {
                return Ok(Self(opened));
            }
            let code = last_error();
            // Gone between the two calls (its last holder closed it): create it again
            if code != ERROR_FILE_NOT_FOUND.0 && code != ERROR_PATH_NOT_FOUND.0 {
                return Err(lock_error("OpenPrivateNamespaceW", code));
            }
        }
        Err(WinError { call: "OpenPrivateNamespaceW", code: ERROR_FILE_NOT_FOUND.0 }.into())
    }
}

/// The lock, a mutex in its private namespace; released when dropped.
pub struct SetupLock {
    mutex: Owned<HANDLE>,
    /// Kept open while the mutex is held, so a setup that comes meanwhile opens this namespace rather than making another.
    _namespace: Namespace,
}

impl SetupLock {
    /// Takes the mutex in `space`, waiting up to `wait` for another holder.
    pub fn take(space: &LockSpace, wait: Duration, say: &mut dyn FnMut(&str)) -> Result<Self, LockError> {
        let sd = Descriptor::from_sddl(space.sddl)?;
        let alias = fresh_alias(space.alias);
        let namespace = Namespace::open(space, &alias, &sd)?;
        let name = HSTRING::from(format!(r"{alias}\{}", space.mutex));
        // Enough to wait on it and release it
        let access = SYNCHRONIZATION_SYNCHRONIZE.0 | MUTEX_MODIFY_STATE.0;
        let attributes = sd.attributes();
        // SAFETY: attributes whose descriptor lives in `sd`; a NUL-terminated name in the namespace just opened; not owned on
        // creation, so it is waited for below whoever made it.
        let handle =
            unsafe { CreateMutexExW(Some(&attributes), &name, 0, access) }.map_err(|e| lock_error("CreateMutexExW", win32_code(&e)))?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let mutex = unsafe { Owned::new(handle) };
        // SAFETY: our handle; no wait.
        let mut event = unsafe { WaitForSingleObject(*mutex, 0) };
        if event == WAIT_TIMEOUT {
            say(&format!("Another setup action is running: waiting for it to end (up to {} s)", wait.as_secs()));
            // SAFETY: as above.
            event = unsafe { WaitForSingleObject(*mutex, u32::try_from(wait.as_millis()).unwrap_or(u32::MAX - 1)) };
        }
        match lock_wait(event) {
            LockWait::Taken => Ok(Self { mutex, _namespace: namespace }),
            LockWait::Abandoned => {
                say("(The last setup's process ended in the middle of its action: this one reads the devices afresh.)");
                Ok(Self { mutex, _namespace: namespace })
            }
            LockWait::Busy => Err(LockError::Busy),
            LockWait::Failed => Err(WinError { call: "WaitForSingleObject", code: last_error() }.into()),
        }
    }
}

impl Drop for SetupLock {
    fn drop(&mut self) {
        // SAFETY: our handle, to a mutex this thread owns (take returns one only then). The fields drop after this: the mutex's
        // handle is closed, then the namespace's.
        let _ = unsafe { ReleaseMutex(*self.mutex) };
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Security::{
        CheckTokenMembership, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, ImpersonateLoggedOnUser, RevertToSelf, SID_AND_ATTRIBUTES,
        TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, WinAuthenticatedUserSid, WinWorldSid,
    };
    use windows::Win32::System::Threading::{CREATE_MUTEX_INITIAL_OWNER, GetCurrentProcess, OpenProcessToken};
    use windows::core::BOOL;

    use super::*;

    #[test]
    fn a_wait_for_the_lock_ends_as_windows_says() {
        assert_eq!(lock_wait(WAIT_OBJECT_0), LockWait::Taken);
        assert_eq!(lock_wait(WAIT_ABANDONED), LockWait::Abandoned);
        assert_eq!(lock_wait(WAIT_TIMEOUT), LockWait::Busy);
        assert_eq!(lock_wait(windows::Win32::Foundation::WAIT_FAILED), LockWait::Failed);
    }

    /// A lock of the test's own, so a real setup is never in the way: a boundary named for this process and `which`, with the
    /// Everyone SID (every token has it) and a descriptor that lets everyone in, so the test needn't be elevated. The setup's
    /// own ([`LOCK`]) differs in those alone.
    fn test_lock(which: &str) -> String {
        format!("acx-ll-cable-test-{which}-{}", std::process::id())
    }

    fn space(boundary: &str) -> LockSpace<'_> {
        // The alias is the boundary's name: the tests run on threads of one process, each with its own namespace
        LockSpace { boundary, sid: WinWorldSid, sddl: "D:P(A;;GA;;;WD)", alias: boundary, mutex: "setup" }
    }

    /// Takes the lock on another thread, as a second setup would: whether it got it, and what it said.
    fn take_elsewhere(boundary: &str, wait: Duration) -> (Result<(), LockError>, Vec<String>) {
        let boundary = boundary.to_string();
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            let taken = SetupLock::take(&space(&boundary), wait, &mut |l| lines.push(l.to_string())).map(drop);
            (taken, lines)
        })
        .join()
        .unwrap()
    }

    /// Astra's review: two switches at once could both create a device. Held, another thread waits and gives up; released, it
    /// takes it; abandoned by a thread that ended holding it, the next takes it and says so.
    #[test]
    fn the_lock_lets_one_setup_in_at_a_time() {
        let boundary = test_lock("one-at-a-time");
        let held = SetupLock::take(&space(&boundary), Duration::ZERO, &mut |_| {}).unwrap();
        let (taken, lines) = take_elsewhere(&boundary, Duration::from_millis(50));
        assert_eq!(taken, Err(LockError::Busy));
        assert_eq!(lines, ["Another setup action is running: waiting for it to end (up to 0 s)"]);
        drop(held);
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO), (Ok(()), vec![]));
        let b = boundary.clone();
        std::thread::spawn(move || std::mem::forget(SetupLock::take(&space(&b), Duration::ZERO, &mut |_| {}).unwrap())).join().unwrap();
        let (taken, lines) = take_elsewhere(&boundary, Duration::ZERO);
        assert_eq!(taken, Ok(()));
        assert_eq!(lines, ["(The last setup's process ended in the middle of its action: this one reads the devices afresh.)"]);
    }

    /// The setup that made the namespace ends while another waits in it: the next still finds the waiter's lock (the namespace
    /// lives while a holder has it open), not a new one beside it.
    #[test]
    fn the_lock_outlives_the_setup_that_made_its_namespace() {
        use std::sync::mpsc;
        let boundary = test_lock("outlives");
        let first = SetupLock::take(&space(&boundary), Duration::ZERO, &mut |_| {}).unwrap();
        let (waiting, waits) = mpsc::channel::<()>();
        let (taken, has_it) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let b = boundary.clone();
        let second = std::thread::spawn(move || {
            let lock = SetupLock::take(&space(&b), Duration::from_secs(10), &mut |_| waiting.send(()).unwrap()).unwrap();
            taken.send(()).unwrap();
            released.recv().unwrap();
            drop(lock);
        });
        waits.recv().unwrap();
        drop(first);
        has_it.recv().unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO).0, Err(LockError::Busy), "the second setup holds it");
        release.send(()).unwrap();
        second.join().unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO).0, Ok(()));
    }

    /// Astra's review: any process could squat a `Global\` name. Neither a plain mutex of the name nor a namespace of the same
    /// boundary name with another SID (one a squatter could make) is the lock.
    #[test]
    fn a_squatted_name_isnt_the_lock() {
        let boundary = test_lock("squatted");
        let plain = HSTRING::from(format!(r"Local\{boundary}"));
        // SAFETY: no attributes; a NUL-terminated name; owned on creation.
        let squatter = unsafe { CreateMutexExW(None, &plain, CREATE_MUTEX_INITIAL_OWNER, SYNCHRONIZATION_SYNCHRONIZE.0) }.unwrap();
        // SAFETY: a valid handle that nothing else owns.
        let _squatter = unsafe { Owned::new(squatter) };
        let alias = format!("{boundary}-squatter");
        let other_sid = LockSpace { sid: WinAuthenticatedUserSid, alias: &alias, ..space(&boundary) };
        let squatted = SetupLock::take(&other_sid, Duration::ZERO, &mut |_| {}).unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO), (Ok(()), vec![]));
        drop(squatted);
    }

    /// This process's token with BUILTIN\Administrators deny-only, as a standard user's or an unelevated administrator's has it,
    /// and its privileges dropped.
    fn token_without_administrators() -> Owned<HANDLE> {
        let mut token = HANDLE::default();
        // SAFETY: this process's pseudo handle; `token` is a live out-pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE, &mut token) }.unwrap();
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let token = unsafe { Owned::new(token) };
        let mut admins = well_known_sid(WinBuiltinAdministratorsSid).unwrap();
        let disable = [SID_AND_ATTRIBUTES { Sid: PSID(admins.as_mut_ptr().cast()), Attributes: 0 }];
        let mut restricted = HANDLE::default();
        // SAFETY: our token; a SID that outlives the call; `restricted` is a live out-pointer.
        unsafe { CreateRestrictedToken(*token, DISABLE_MAX_PRIVILEGE, Some(&disable), None, None, &mut restricted) }.unwrap();
        // SAFETY: as above.
        unsafe { Owned::new(restricted) }
    }

    /// The setup's own lock refuses a caller without Administrators enabled, at once and by name: a thread impersonating this
    /// process's token with Administrators deny-only. Refused at the namespace (its boundary, or, if a setup made it meanwhile,
    /// its descriptor).
    #[test]
    fn the_setups_lock_refuses_a_caller_that_isnt_an_elevated_administrator() {
        let refused = std::thread::spawn(|| {
            let token = token_without_administrators();
            // SAFETY: a valid token, for this thread alone, reverted below.
            unsafe { ImpersonateLoggedOnUser(*token) }.unwrap();
            let refused = SetupLock::take(&LOCK, Duration::ZERO, &mut |_| {}).map(drop);
            // SAFETY: no arguments; ends this thread's impersonation.
            unsafe { RevertToSelf() }.unwrap();
            refused
        })
        .join()
        .unwrap();
        match &refused {
            Err(LockError::Denied(e)) => assert!(matches!(e.call, "CreatePrivateNamespaceW" | "OpenPrivateNamespaceW"), "{e}"),
            _ => panic!("{refused:?}"),
        }
        let text = refused.unwrap_err().to_string();
        assert!(text.starts_with("the setup's lock refused this process (") && text.contains("ERROR_ACCESS_DENIED (5)"), "{text}");
    }

    /// Whether this thread's token has BUILTIN\Administrators enabled.
    fn elevated() -> bool {
        let mut admins = well_known_sid(WinBuiltinAdministratorsSid).unwrap();
        let mut member = BOOL(0);
        // SAFETY: no token, so this thread's (or the process's); a SID that outlives the call; `member` is a live out-pointer.
        unsafe { CheckTokenMembership(None, PSID(admins.as_mut_ptr().cast()), &mut member) }.unwrap();
        member.as_bool()
    }

    /// Elevated, this process takes the setup's own lock, a second take is Busy, and once released it can be taken again. It
    /// holds the real lock for a moment, changing nothing: a setup that comes meanwhile waits for it.
    #[test]
    fn the_setups_lock_lets_an_elevated_administrator_in() {
        if !elevated() {
            eprintln!("skipped: this process isn't an elevated administrator's, so the setup's lock refuses it (the test above)");
            return;
        }
        let take = || std::thread::spawn(|| SetupLock::take(&LOCK, Duration::ZERO, &mut |_| {}).map(drop)).join().unwrap();
        let held = match SetupLock::take(&LOCK, Duration::ZERO, &mut |_| {}) {
            Ok(lock) => lock,
            Err(LockError::Busy) => {
                eprintln!("skipped: a setup holds the lock now");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert_eq!(take(), Err(LockError::Busy), "held");
        drop(held);
        match take() {
            Ok(()) => {}
            Err(LockError::Busy) => eprintln!("note: a setup took the lock as soon as it was released"),
            Err(e) => panic!("{e}"),
        }
        let text = LockError::Busy.to_string();
        assert!(text.starts_with("another setup action has held the setup's lock for 60 s") && text.ends_with("Nothing was changed."));
    }
}
