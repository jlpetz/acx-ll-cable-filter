//! Whether the driver's image is still loaded (TODO 19, the per-cable design, section 9, `update`): the driver service's state
//! from the service control manager. A kernel driver's service reads SERVICE_RUNNING while its image is loaded and
//! SERVICE_STOPPED once it has unloaded, which happens only when no device and no handle on a control device holds it (a
//! deleted control device lives until its last handle closes). Read-only; any user may ask.

use std::mem::size_of;
use std::time::Duration;

use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
};
use windows::core::{HSTRING, PCWSTR};

use super::Clock;
use crate::win_error::{WinError, win32_code};

/// The driver's service, as the INF names it (`AddService = AudioCodec`; D1 keeps it).
pub const SERVICE: &str = "AudioCodec";
/// How long `update` and the migration wait for the old image to unload once every node of ours is disabled.
pub const UNLOAD_WAIT: Duration = Duration::from_secs(10);
/// How often the wait looks.
const POLL: Duration = Duration::from_millis(250);

/// The service's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// SERVICE_STOPPED: the image isn't loaded.
    Stopped,
    /// Any other state (SERVICE_RUNNING, 4, while the image is loaded).
    Loaded(u32),
    /// No such service: no package of ours was ever installed on a device, so nothing is loaded.
    Absent,
}

impl State {
    pub fn unloaded(self) -> bool {
        matches!(self, State::Stopped | State::Absent)
    }
}

/// A service control manager handle, closed on drop.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        // SAFETY: our handle, closed once.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

/// The service `name`'s state now.
pub fn state(name: &str) -> Result<State, WinError> {
    // SAFETY: the local machine's active database; connect access only.
    let manager =
        Sc(unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }.map_err(WinError::of("OpenSCManagerW"))?);
    let name = HSTRING::from(name);
    // SAFETY: an open manager; a NUL-terminated name; query access only.
    let service = match unsafe { OpenServiceW(manager.0, &name, SERVICE_QUERY_STATUS) } {
        Ok(h) => Sc(h),
        Err(e) if win32_code(&e) == ERROR_SERVICE_DOES_NOT_EXIST.0 => return Ok(State::Absent),
        Err(e) => return Err(WinError::new("OpenServiceW", &e)),
    };
    let mut buf = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
    let mut needed = 0u32;
    // SAFETY: an open service; a buffer the size of the structure; `needed` is a live out-pointer.
    unsafe { QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed) }
        .map_err(WinError::of("QueryServiceStatusEx"))?;
    // SAFETY: the call filled the structure; read unaligned from the byte buffer.
    let status: SERVICE_STATUS_PROCESS = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Ok(if status.dwCurrentState == SERVICE_STOPPED { State::Stopped } else { State::Loaded(status.dwCurrentState.0) })
}

/// How a wait for the image to unload ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unload {
    /// Unloaded, after this long.
    Done(Duration),
    /// Still loaded at the deadline: its last state.
    StillLoaded(State),
    /// The state couldn't be read at the deadline: the last error.
    Unknown(WinError),
}

/// Up to `wait` on `clock` for the service to stop, reading it with `read` every [`POLL`]. A failed read is tried again.
pub fn wait_unloaded(wait: Duration, read: &mut dyn FnMut() -> Result<State, WinError>, clock: &mut dyn Clock) -> Unload {
    loop {
        let now = read();
        if let Ok(s) = now
            && s.unloaded()
        {
            return Unload::Done(clock.elapsed());
        }
        if clock.elapsed() >= wait {
            return match now {
                Ok(s) => Unload::StillLoaded(s),
                Err(e) => Unload::Unknown(e),
            };
        }
        clock.sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::tests::FakeClock;

    fn over(looks: &[Result<State, WinError>]) -> (Unload, Duration) {
        let mut i = 0;
        let mut read = || {
            i += 1;
            looks[(i - 1).min(looks.len() - 1)].clone()
        };
        let mut clock = FakeClock(Duration::ZERO);
        (wait_unloaded(Duration::from_secs(1), &mut read, &mut clock), clock.0)
    }

    #[test]
    fn the_wait_ends_when_the_service_stops() {
        assert_eq!(over(&[Ok(State::Stopped)]), (Unload::Done(Duration::ZERO), Duration::ZERO));
        assert_eq!(over(&[Ok(State::Absent)]).0, Unload::Done(Duration::ZERO));
        let (u, took) = over(&[Ok(State::Loaded(4)), Ok(State::Loaded(3)), Ok(State::Stopped)]);
        assert_eq!((u, took), (Unload::Done(POLL * 2), POLL * 2));
        let failed = WinError { call: "QueryServiceStatusEx", code: 5 };
        assert_eq!(over(&[Err(failed.clone()), Ok(State::Stopped)]).0, Unload::Done(POLL));
        // Still loaded at the deadline (a handle holds a deleted control device), or unreadable
        assert_eq!(over(&[Ok(State::Loaded(4))]), (Unload::StillLoaded(State::Loaded(4)), Duration::from_secs(1)));
        assert_eq!(over(&[Err(failed.clone())]).0, Unload::Unknown(failed));
    }

    /// Read-only: the state of a service that is always there, and of one that isn't.
    #[test]
    fn a_services_state_reads() {
        assert!(matches!(state("EventLog"), Ok(State::Loaded(_))));
        assert_eq!(state("NoSuchServiceAcxLlCableTest"), Ok(State::Absent));
    }
}
