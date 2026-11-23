//! Win32 resources owned by the control thread and child. Only the already-open event is touched by the audio callback.
use std::ffi::OsStr;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::System::Threading::*;

fn last(where_: &str) -> String {
    format!("{where_}: {}", io::Error::last_os_error())
}
fn ok(value: i32, where_: &str) -> Result<(), String> {
    if value == 0 { Err(last(where_)) } else { Ok(()) }
}
fn handle(raw: HANDLE, where_: &str) -> Result<OwnedHandle, String> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE { Err(last(where_)) } else { Ok(unsafe { OwnedHandle::from_raw_handle(raw) }) }
}
fn inherited_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: null_mut(), bInheritHandle: 1 }
}

pub(crate) struct Mapping {
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}
// The only typed view is of atomics; the OS retains the mapping as long as this object is alive.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}
impl Mapping {
    pub fn new() -> Result<Self, String> {
        let bytes = size_of::<super::Shared>() as u32;
        let h = handle(
            unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, &inherited_attributes(), PAGE_READWRITE, 0, bytes, null()) },
            "CreateFileMappingW",
        )?;
        Self::map(h)
    }
    pub fn map(handle: OwnedHandle) -> Result<Self, String> {
        unsafe {
            let size = size_of::<super::Shared>();
            let view = MapViewOfFile(handle.as_raw_handle(), FILE_MAP_ALL_ACCESS, 0, 0, size);
            if view.Value.is_null() {
                return Err(last("MapViewOfFile"));
            }
            let map = Self { handle, view };
            ok(VirtualLock(view.Value, size), "VirtualLock effect buffers")?;
            Ok(map)
        }
    }
    pub fn get(&self) -> &super::Shared {
        unsafe { &*self.view.Value.cast() }
    }
    pub fn handle(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
        }
    }
}

pub(crate) struct Wake(pub OwnedHandle);
impl Wake {
    pub fn new() -> Result<Self, String> {
        handle(unsafe { CreateEventW(&inherited_attributes(), 0, 0, null()) }, "CreateEventW").map(Self)
    }
    pub fn signal(&self) {
        unsafe {
            SetEvent(self.0.as_raw_handle());
        }
    }
    pub fn wait(&self, ms: u32) -> Result<(), String> {
        match unsafe { WaitForSingleObject(self.0.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
            _ => Err(last("WaitForSingleObject")),
        }
    }
}

fn token(access: u32) -> Result<OwnedHandle, String> {
    let mut token = null_mut();
    unsafe {
        ok(OpenProcessToken(GetCurrentProcess(), access, &mut token), "OpenProcessToken")?;
    }
    handle(token, "OpenProcessToken")
}
fn sid(kind: WELL_KNOWN_SID_TYPE) -> Result<[u32; 17], String> {
    let mut sid = [0; 17];
    let mut size = size_of_val(&sid) as u32;
    unsafe {
        ok(CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &mut size), "CreateWellKnownSid")?;
    }
    Ok(sid)
}
fn integrity(token: HANDLE) -> Result<u32, String> {
    // TOKEN_MANDATORY_LABEL (pointer-aligned), followed by a SID of at most SECURITY_MAX_SID_SIZE.
    let mut data = [0usize; 32];
    let mut len = 0;
    unsafe {
        ok(
            GetTokenInformation(token, TokenIntegrityLevel, data.as_mut_ptr().cast(), size_of_val(&data) as u32, &mut len),
            "Read token integrity",
        )?;
        let label = &*data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        if count == 0 {
            return Err("Worker integrity label has no RID".into());
        }
        Ok(*GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)))
    }
}
fn admin_member() -> Result<bool, String> {
    let mut admin = sid(WinBuiltinAdministratorsSid)?;
    let mut yes = 0;
    unsafe {
        ok(CheckTokenMembership(null_mut(), admin.as_mut_ptr().cast(), &mut yes), "Check administrative group")?;
    }
    Ok(yes != 0)
}
pub fn verify_worker_token() -> Result<(), String> {
    if integrity(token(TOKEN_QUERY)?.as_raw_handle())? > SECURITY_MANDATORY_MEDIUM_RID as u32 || admin_member()? {
        return Err("Effect helper has an elevated token".into());
    }
    Ok(())
}
fn restricted_token() -> Result<OwnedHandle, String> {
    let parent = token(TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT)?;
    let mut admin = sid(WinBuiltinAdministratorsSid)?;
    let deny = SID_AND_ATTRIBUTES { Sid: admin.as_mut_ptr().cast(), Attributes: 0 };
    let mut child = null_mut();
    unsafe {
        ok(
            CreateRestrictedToken(parent.as_raw_handle(), DISABLE_MAX_PRIVILEGE, 1, &deny, 0, null(), 0, null(), &mut child),
            "Restrict effect helper token",
        )?;
        let child = handle(child, "Restricted token")?;
        if integrity(child.as_raw_handle())? > SECURITY_MANDATORY_MEDIUM_RID as u32 {
            let mut medium = sid(WinMediumLabelSid)?;
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES { Sid: medium.as_mut_ptr().cast(), Attributes: SE_GROUP_INTEGRITY as u32 },
            };
            ok(
                SetTokenInformation(
                    child.as_raw_handle(),
                    TokenIntegrityLevel,
                    (&raw const label).cast(),
                    (size_of_val(&label) as u32) + GetLengthSid(label.Label.Sid),
                ),
                "Lower effect helper integrity",
            )?;
        }
        // An elevated token's default DACL may grant only SYSTEM and the Administrators group. The latter is deny-only
        // in the effect process. Give its own user access to the objects the Windows loader creates under this token.
        let mut user = [0usize; 32];
        let mut len = 0;
        ok(
            GetTokenInformation(child.as_raw_handle(), TokenUser, user.as_mut_ptr().cast(), size_of_val(&user) as u32, &mut len),
            "Read effect user SID",
        )?;
        let user = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let mut acl_storage = [0usize; 32];
        let acl = acl_storage.as_mut_ptr().cast::<ACL>();
        ok(InitializeAcl(acl, size_of_val(&acl_storage) as u32, ACL_REVISION), "Initialize effect default ACL")?;
        ok(AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, user), "Grant effect user access to its own objects")?;
        let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
        ok(
            SetTokenInformation(child.as_raw_handle(), TokenDefaultDacl, (&raw const dacl).cast(), size_of_val(&dacl) as u32),
            "Set effect default ACL",
        )?;
        Ok(child)
    }
}

struct Attributes(Vec<usize>);
impl Attributes {
    fn new() -> Result<Self, String> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(last("Size process attributes"));
        }
        let mut buf = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        unsafe {
            ok(InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), 2, 0, &mut bytes), "Initialize process attributes")?;
        }
        Ok(Self(buf))
    }
    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
    fn set(&mut self, kind: u32, handles: &[HANDLE]) -> Result<(), String> {
        unsafe {
            ok(
                UpdateProcThreadAttribute(self.list(), 0, kind as usize, handles.as_ptr().cast(), size_of_val(handles), null_mut(), null()),
                "Set child process attributes",
            )
        }
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.list());
        }
    }
}

pub(crate) struct Process {
    process: Arc<OwnedHandle>,
    stop: Arc<Wake>,
}
// Windows copies the parent's *process* error mode before the child's loader or our Rust entry point runs.
// Serialize this narrow scope across concurrent launches; leave every existing error-mode bit intact.
static SPAWN_ERROR_MODE: Mutex<()> = Mutex::new(());
const HIDDEN_ERRORS: u32 = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;
struct ErrorMode(u32);
impl ErrorMode {
    fn inherit_quietly() -> Self {
        let mode = unsafe { GetErrorMode() };
        unsafe {
            SetErrorMode(mode | HIDDEN_ERRORS);
        }
        Self(mode)
    }
}
impl Drop for ErrorMode {
    fn drop(&mut self) {
        unsafe {
            SetErrorMode(self.0);
        }
    }
}
pub(crate) fn verify_quiet_mode() -> Result<(), String> {
    if unsafe { GetErrorMode() } & HIDDEN_ERRORS != HIDDEN_ERRORS {
        return Err("Unattended Windows error mode is missing".into());
    }
    Ok(())
}
impl Process {
    pub fn start(exe: &Path, map: &Mapping, wake: &Wake, done: &Wake) -> Result<Self, String> {
        let mut app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        if app[..app.len() - 1].contains(&0) {
            return Err("Invalid helper executable path".into());
        }
        let args = format!(
            "nf-fx-host --nf-fx-worker {} {} {}",
            map.handle() as usize,
            wake.0.as_raw_handle() as usize,
            done.0.as_raw_handle() as usize
        );
        let mut cmd: Vec<u16> = OsStr::new(&args).encode_wide().chain([0]).collect();
        let token = restricted_token()?;
        unsafe {
            let job = handle(CreateJobObjectW(null(), null()), "Create effect job")?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = 1;
            ok(
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of_val(&limits) as u32,
                ),
                "Set job limits",
            )?;
            let mut attrs = Attributes::new()?;
            let inherit = [map.handle(), wake.0.as_raw_handle(), done.0.as_raw_handle()];
            let jobs = [job.as_raw_handle()];
            attrs.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherit)?;
            attrs.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = attrs.list();
            let mut pi = PROCESS_INFORMATION::default();
            let _guard = SPAWN_ERROR_MODE.lock().unwrap_or_else(|e| e.into_inner());
            let _mode = ErrorMode::inherit_quietly();
            verify_quiet_mode()?;
            ok(
                CreateProcessAsUserW(
                    token.as_raw_handle(),
                    app.as_mut_ptr(),
                    cmd.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                    null(),
                    null(),
                    &startup.StartupInfo,
                    &mut pi,
                ),
                "Start restricted effect helper",
            )?;
            drop(_mode);
            drop(_guard);
            drop(handle(pi.hThread, "Child thread")?);
            let process = Arc::new(handle(pi.hProcess, "Child process")?);
            let stop = Arc::new(Wake(handle(CreateEventW(null(), 1, 0, null()), "Create effect shutdown event")?));
            let watcher = process.clone();
            let shutdown = stop.clone();
            // The watcher owns the kill-on-close job before the audio callback can run. Give a cooperative plugin time
            // to stop_processing, deactivate and destroy itself. A hung plugin gets the same bounded grace interval.
            std::thread::Builder::new()
                .name("effect-reaper".into())
                .spawn(move || {
                    let handles = [watcher.as_raw_handle(), shutdown.0.as_raw_handle()];
                    if WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) == WAIT_OBJECT_0 + 1 {
                        WaitForSingleObject(watcher.as_raw_handle(), 250);
                    }
                    drop(job);
                })
                .map_err(|e| format!("Start effect cleanup thread: {e}"))?;
            Ok(Self { process, stop })
        }
    }
    pub fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) != WAIT_TIMEOUT }
    }
    pub fn wait_for_response(&self, event: &Wake, ms: u32) -> Result<(), String> {
        let handles = [event.0.as_raw_handle(), self.process.as_raw_handle()];
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err("CLAP effect timed out during offline processing".into()),
            n if n == WAIT_OBJECT_0 + 1 => Err("CLAP effect exited during offline processing".into()),
            _ => Err(last("Wait for effect response")),
        }
    }
    pub fn stopping(&self) {
        self.stop.signal();
    }
}

pub(crate) struct ProAudio(HANDLE);
impl ProAudio {
    pub fn enter() -> Result<Self, String> {
        let mut task = 0;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(windows_sys::w!("Pro Audio"), &mut task) };
        if handle.is_null() { Err(last("Register effect worker with Pro Audio")) } else { Ok(Self(handle)) }
    }
}
impl Drop for ProAudio {
    fn drop(&mut self) {
        unsafe {
            AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

#[cfg(test)]
impl Process {
    pub(super) fn terminate_test_worker(&self) {
        assert_ne!(unsafe { TerminateProcess(self.process.as_raw_handle(), 17) }, 0);
        assert_eq!(unsafe { WaitForSingleObject(self.process.as_raw_handle(), 2000) }, WAIT_OBJECT_0);
    }
}
