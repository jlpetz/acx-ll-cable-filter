//! Private, local control pipe. Both ends check the process at the other end before they accept a command or handle.

use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HLOCAL, WIN32_ERROR};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoCreateGuid, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE,
};
use windows::Win32::System::Threading::GetProcessId;
use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::core::{HSTRING, PCWSTR, w};

use super::*;

pub(super) fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn name(pid: u32, nonce: &str) -> String {
    format!(r"\\.\pipe\noise-filter-vmic-{pid}-{nonce}")
}

fn listener(name: &str) -> io::Result<File> {
    let name = HSTRING::from(name);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: fixed SDDL. Only administrators and the object owner may open this pipe, including a different UAC admin.
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:P(A;;GA;;;OW)(A;;GA;;;BA)"), SDDL_REVISION_1, &mut sd, None) }
        .map_err(io_error)?;
    // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW allocated this with LocalAlloc; we free it once.
    let _descriptor = unsafe { Owned::new(HLOCAL(sd.0)) };
    let attrs =
        SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
    // SAFETY: name and descriptor live through the call; only the first, local instance of this pipe is permitted.
    let pipe = unsafe {
        CreateNamedPipeW(
            &name,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_NOWAIT | PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            512,
            512,
            0,
            Some(&attrs),
        )
    };
    if pipe.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this is our newly created pipe.
    Ok(unsafe { File::from_raw_handle(pipe.0) })
}

fn elevate(pid: u32, nonce: &str) -> io::Result<OwnedHandle> {
    // A fixed sibling, never a PATH lookup or the external application's own executable.
    let client = std::env::current_exe()?;
    let exe = client.with_file_name("llcable-broker.exe");
    if !exe.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "install llcable-broker.exe beside the application to approve direct cable feeds",
        ));
    }
    let directory = HSTRING::from(exe.parent().ok_or(io::ErrorKind::InvalidInput)?);
    let exe = HSTRING::from(exe.as_path());
    let args = HSTRING::from(format!("{} {pid} {nonce}", helper::FLAG));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: 0,
        ..Default::default()
    };
    // SAFETY: a control thread, never the audio thread. Balance either successful initialization; an existing MTA stays in place.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    // SAFETY: every buffer lives through the call. The helper path is absolute and the arguments contain only a PID and nonce.
    let result = unsafe { ShellExecuteExW(&mut info) };
    if com.is_ok() {
        // SAFETY: balances the initialization on this same thread.
        unsafe { CoUninitialize() };
    }
    result.map_err(io_error)?;
    if info.hProcess.is_invalid() {
        return Err(io::ErrorKind::NotFound.into());
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS gave the caller an owned process handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) })
}

pub(super) fn launch() -> io::Result<Session> {
    // SAFETY: CoCreateGuid has no caller-owned pointers or apartment requirement.
    let nonce = format!("{:032x}", unsafe { CoCreateGuid() }.map_err(io_error)?.to_u128());
    let pid = std::process::id();
    let pipe = listener(&name(pid, &nonce))?;
    let child = elevate(pid, &nonce)?;
    accept(pipe, child)
}

fn accept(pipe: File, child: OwnedHandle) -> io::Result<Session> {
    let start = Instant::now();
    loop {
        // SAFETY: PIPE_NOWAIT; we own the server end and its only instance.
        match unsafe { ConnectNamedPipe(raw(&pipe), None) } {
            Ok(()) => {}
            Err(e) => match WIN32_ERROR::from_error(&e) {
                Some(ERROR_PIPE_CONNECTED) => break,
                Some(ERROR_PIPE_LISTENING) => {}
                _ => return Err(io_error(e)),
            },
        }
        let mut peer = 0;
        // SAFETY: a live server handle; positive only once a client has connected.
        if unsafe { GetNamedPipeClientProcessId(raw(&pipe), &mut peer) }.is_ok() && peer != 0 {
            break;
        }
        if !alive(&child) || start.elapsed() >= IO_LIMIT {
            return Err(io::Error::other("the elevated driver helper did not connect"));
        }
        std::thread::sleep(POLL);
    }
    let mut peer = 0;
    // SAFETY: the process handle returned by UAC remains open, pinning its identity while we check the kernel's actual pipe peer.
    unsafe {
        GetNamedPipeClientProcessId(raw(&pipe), &mut peer).map_err(io_error)?;
        if peer == 0 || peer != GetProcessId(raw(&child)) {
            return Err(denied());
        }
    }
    let session = Session { child, pipe: Mutex::new(Some(pipe)), connected: true.into(), leases: Mutex::default() };
    session.exchange(None)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_only_accepts_a_fixed_local_nonce() {
        assert!(valid_nonce("00112233445566778899aabbccddeeff"));
        for invalid in ["", "..", "00112233445566778899aabbccddeeff\\x", "00112233445566778899aabbccddee/g"] {
            assert!(!valid_nonce(invalid));
        }
    }

    /// The child is the same executable as the parent, as in the actual app. It receives no privilege elevation in this test.
    #[test]
    fn child_for_broker_test() {
        if let (Ok(pid), Ok(nonce)) = (std::env::var("NF_TEST_BROKER_PID"), std::env::var("NF_TEST_BROKER_NONCE")) {
            helper::serve(pid.parse().unwrap(), &nonce).unwrap();
        }
    }

    /// Explicit live test: consumes the driver's single writer, without starting UAC or an audio endpoint. Drop confirms the helper
    /// handles a second attachment and a closed parent channel. Run alone on an elevated, single-user dev box with its driver installed.
    #[test]
    #[ignore = "requires installed driver, elevated process, and sole signed-in interactive session"]
    fn broker_retains_device_and_transfers_fresh_rings() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let pid = std::process::id();
        // SAFETY: CoCreateGuid takes no arguments.
        let nonce = format!("{:032x}", unsafe { CoCreateGuid().unwrap() }.to_u128());
        let pipe = listener(&name(pid, &nonce)).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "broker::connection::tests::child_for_broker_test"])
            .env("NF_TEST_BROKER_PID", pid.to_string())
            .env("NF_TEST_BROKER_NONCE", nonce)
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let session = accept(pipe, child.into()).unwrap();
        let cables = crate::cables::present().unwrap();
        let id = crate::cables::choose_default(&cables).unwrap().cable.id;
        for _ in 0..2 {
            let dup = session.exchange(Some((START, id))).unwrap();
            // SAFETY: a fresh section handle transferred to this process by the authenticated running helper.
            let section = unsafe { Owned::new(HANDLE(dup as usize as _)) };
            let ring = RingMemory::shared_writer(*section, LIVE_CAPACITY).unwrap();
            assert_ne!(ring.header().state.load(Ordering::Acquire) & protocol::STATE_ATTACHED, 0);
            assert_eq!(session.exchange(Some((START, id))).unwrap_err().raw_os_error(), Some(ERROR_BUSY.0 as i32));
            let mut writer = crate::Writer::new(&ring);
            assert_eq!(writer.write_frame(), 0);
            assert_eq!(writer.produce(128, |_| [0.0; 2]), 128);
            session.exchange(Some((STOP, id))).unwrap();
            assert_eq!(ring.header().state.load(Ordering::Acquire) & protocol::STATE_ATTACHED, 0);
        }
        drop(session.pipe);
        // SAFETY: owned process handle and a bounded wait. EOF releases the authenticated connection.
        assert_eq!(unsafe { WaitForSingleObject(raw(&session.child), 3000) }, windows::Win32::Foundation::WAIT_OBJECT_0);
    }
}
