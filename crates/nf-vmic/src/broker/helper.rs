//! Elevated side. Its entire input after authentication is START or STOP; its only transfer is an anonymous ring into the
//! kernel-identified parent process. No command can supply a path, a handle, a capacity or a duplication target.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{DuplicateHandle, ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandleEx, SECURITY_ANONYMOUS,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE, SetNamedPipeHandleState};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSListen, WTSQuerySessionInformationW,
    WTSUserName,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use super::*;

pub(super) const FLAG: &str = "--vmic-broker";

fn image(process: HANDLE) -> io::Result<Vec<u16>> {
    let mut path = vec![0; 32768];
    let mut len = path.len() as u32;
    // SAFETY: a queryable process and a writable buffer whose element count we supply.
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len) }.map_err(io_error)?;
    path.truncate(len as usize);
    Ok(path)
}

fn session_id(pid: u32) -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: one live output word.
    unsafe { ProcessIdToSessionId(pid, &mut session) }.map_err(io_error)?;
    Ok(session)
}

fn same_installation(caller: &Path, helper: &Path) -> io::Result<bool> {
    fn directory(exe: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(exe.parent().ok_or(io::ErrorKind::InvalidInput)?)
    }
    fn identity(file: &File) -> io::Result<FILE_ID_INFO> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: live handle to the opened Windows directory and a suitably sized, writable info structure.
        unsafe { GetFileInformationByHandleEx(raw(file), FileIdInfo, (&raw mut info).cast(), size_of::<FILE_ID_INFO>() as u32) }
            .map_err(io_error)?;
        Ok(info)
    }
    // Kernel paths may use different case, verbatim prefixes or aliases. Directory object identity also distinguishes
    // genuinely different directories, including those in case-sensitive Windows directories.
    let caller = directory(caller)?;
    let helper = directory(helper)?;
    Ok(identity(&caller)? == identity(&helper)?)
}

/// The microphone is machine-global. Grant access only while the requester is the sole logged-in interactive session.
fn check_sessions(own: u32) -> io::Result<()> {
    struct WtsBuffer(*mut core::ffi::c_void);
    impl Drop for WtsBuffer {
        fn drop(&mut self) {
            // SAFETY: the WTS call allocated this buffer, and ownership is unique.
            unsafe { WTSFreeMemory(self.0) };
        }
    }
    let mut count = 0;
    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    // SAFETY: local server. On success WTS returns a counted array valid until WTSFreeMemory.
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count) }.map_err(io_error)?;
    let _sessions = WtsBuffer(sessions.cast());
    if sessions.is_null() || count == 0 || own == 0 {
        return Err(denied());
    }
    let mut active = false;
    // SAFETY: WTS supplied `count` live WTS_SESSION_INFOW values.
    for s in unsafe { std::slice::from_raw_parts(sessions, count as usize) } {
        if s.SessionId == own {
            active = s.State == WTSActive;
        } else if s.SessionId != 0 && s.State != WTSListen {
            let mut user = PWSTR::null();
            let mut bytes = 0;
            // SAFETY: WTS supplies this buffer and its byte count; we only check that the first UTF-16 unit is nonzero.
            unsafe { WTSQuerySessionInformationW(None, s.SessionId, WTSUserName, &mut user, &mut bytes) }.map_err(io_error)?;
            let _user = WtsBuffer(user.0.cast());
            // SAFETY: an available first code unit is inside the WTS-owned buffer.
            if bytes >= 2 && !user.is_null() && unsafe { *user.0 } != 0 {
                return Err(denied());
            }
        }
    }
    if active { Ok(()) } else { Err(denied()) }
}

fn connect_parent(pipe: &File, expected: u32) -> io::Result<(OwnedHandle, u32)> {
    let mut peer = 0;
    // SAFETY: read-only peer query on our connected local pipe.
    unsafe { GetNamedPipeServerProcessId(raw(pipe), &mut peer) }.map_err(io_error)?;
    if peer == 0 || peer != expected {
        return Err(denied());
    }
    // SAFETY: the PID is the actual pipe creator. Opening a process handle pins this PID through every later duplication.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | PROCESS_SYNCHRONIZE, false, peer) }
        .map_err(io_error)?;
    // SAFETY: OpenProcess handed us ownership.
    let process = unsafe { OwnedHandle::from_raw_handle(h.0) };
    let session = session_id(peer)?;
    let caller = PathBuf::from(OsString::from_wide(&image(h)?));
    let helper_path = std::env::current_exe()?;
    // Only a peer from this installation directory; both absolute image paths are supplied by Windows.
    if session != session_id(std::process::id())? || !alive(&process) || !same_installation(&caller, &helper_path)? {
        return Err(denied());
    }
    check_sessions(session)?;
    Ok((process, session))
}

fn start_feed(parent: &OwnedHandle, active: &mut BTreeMap<CableId, Attachment>, id: CableId) -> io::Result<u64> {
    if active.contains_key(&id) {
        return Err(io::Error::from_raw_os_error(ERROR_BUSY.0 as i32));
    }
    if !crate::cables::present().map_err(io::Error::other)?.iter().any(|c| c.cable.id == id) {
        return Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND.0 as i32));
    }
    let (ring, section) = RingMemory::shared(LIVE_CAPACITY)?;
    let attachment = Device::open_path(&protocol::control_path(&id))?.attach_live(ring).map_err(|e| e.error)?;
    let mut remote = HANDLE::default();
    // SAFETY: fresh anonymous pagefile section only; verified, live recipient. Limit access to the two view rights and disallow inheritance.
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            *section,
            raw(parent),
            &mut remote,
            FILE_MAP_READ.0 | FILE_MAP_WRITE.0,
            false,
            Default::default(),
        )
    }
    .map_err(io_error)?;
    active.insert(id, attachment);
    Ok(remote.0 as usize as u64)
}

pub(super) fn serve(pid: u32, nonce: &str) -> io::Result<()> {
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags((SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS).0)
        .open(connection::name(pid, nonce))?;
    // SAFETY: never let a caller that stops draining the pipe block this privileged loop or its session checks.
    unsafe { SetNamedPipeHandleState(raw(&pipe), Some(&(PIPE_READMODE_BYTE | PIPE_NOWAIT)), None, None) }.map_err(io_error)?;
    let (parent, session) = match connect_parent(&pipe, pid) {
        Ok(pair) => pair,
        Err(e) => return reply(&mut pipe, Err(e)),
    };
    reply(&mut pipe, Ok(0))?;
    let mut active = BTreeMap::new();
    let mut checked = Instant::now();
    while alive(&parent) {
        if checked.elapsed() >= Duration::from_millis(200) {
            check_sessions(session)?;
            active.retain(|_, a: &mut Attachment| !a.finished());
            checked = Instant::now();
        }
        let pending = match available(&pipe) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e),
        };
        if pending < 17 {
            std::thread::sleep(POLL);
            continue;
        }
        let mut command = [0; 17];
        pipe.read_exact(&mut command)?;
        let id = CableId(u128::from_le_bytes(command[1..].try_into().unwrap()));
        let result = match command[0] {
            START => {
                if let Err(e) = check_sessions(session) {
                    // Revoke the whole approved session. Even the reply cannot leave a previous cable feeding.
                    drop(active);
                    reply(&mut pipe, Err(e))?;
                    return Ok(());
                }
                // A newly enabled/re-added cable can arrive before the next sweep.
                if active.get(&id).is_some_and(Attachment::finished) {
                    active.remove(&id);
                }
                start_feed(&parent, &mut active, id)
            }
            STOP => match active.remove(&id) {
                Some(attachment) => attachment.detach().map(|_| 0),
                None => Ok(0),
            },
            _ => Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA.0 as i32)),
        };
        reply(&mut pipe, result)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_image_directory_matches_windows_verbatim_and_rejects_a_different_directory() {
        let image = std::env::current_exe().unwrap();
        let verbatim = std::fs::canonicalize(&image).unwrap().with_file_name("llcable-broker.exe");
        assert!(same_installation(&image, &verbatim).unwrap());
        let unrelated = image.parent().unwrap().parent().unwrap().join("llcable-broker.exe");
        assert!(!same_installation(&image, &unrelated).unwrap());
    }
}
