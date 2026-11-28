//! A user-approved connection to the virtual cables. The elevated helper owns one handle and pending request per active cable.
//! The caller writes directly into the anonymous ring section supplied for each selected cable.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BUSY, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Owned;

use crate::device::io_error;
use crate::identity::CableId;
use crate::{Attachment, Device, RingMemory, protocol};

mod connection;
mod helper;

/// The production ring size, fixed so an ordinary caller cannot request unbounded locked memory through the helper.
pub const LIVE_CAPACITY: u32 = 8192;
const START: u8 = 1;
const STOP: u8 = 2;
const IO_LIMIT: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(10);
static APPROVED: Mutex<Option<Arc<Session>>> = Mutex::new(None);

fn raw(h: &impl AsRawHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn alive(h: &OwnedHandle) -> bool {
    // SAFETY: a process handle owned for this call. A zero timeout does not wait.
    unsafe { WaitForSingleObject(raw(h), 0) == WAIT_TIMEOUT }
}

fn available(pipe: &File) -> io::Result<usize> {
    let mut bytes = 0;
    // SAFETY: a connected pipe owned for this call and one writable count.
    unsafe { PeekNamedPipe(raw(pipe), None, 0, None, Some(&mut bytes), None) }.map_err(io_error)?;
    Ok(bytes as usize)
}

/// Fixed-size response. A Win32 error or a section newly duplicated into the receiving process, never a process or driver handle.
fn reply(pipe: &mut File, result: io::Result<u64>) -> io::Result<()> {
    let (code, handle) = match result {
        Ok(handle) => (0, handle),
        Err(e) => (e.raw_os_error().unwrap_or(31), 0), // ERROR_GEN_FAILURE
    };
    let mut packet = [0; 12];
    packet[..4].copy_from_slice(&i32::to_le_bytes(code));
    packet[4..].copy_from_slice(&handle.to_le_bytes());
    pipe.write_all(&packet)
}

struct Session {
    child: OwnedHandle,
    pipe: Mutex<Option<File>>,
    connected: AtomicBool,
    leases: Mutex<Leases>,
}

/// A removed cable can reappear while its previous writer still owns a mapping. Its later Drop must not
/// send a STOP for the replacement. A live token's allocation is unique for as long as either owner holds it.
#[derive(Default)]
struct Leases(BTreeMap<CableId, Arc<()>>);

impl Leases {
    fn replace(&mut self, id: CableId) -> Arc<()> {
        let token = Arc::new(());
        self.0.insert(id, Arc::clone(&token));
        token
    }

    fn release(&mut self, id: CableId, token: &Arc<()>) -> bool {
        if self.0.get(&id).is_some_and(|owner| Arc::ptr_eq(owner, token)) {
            self.0.remove(&id);
            true
        } else {
            false
        }
    }
}

impl Session {
    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) && alive(&self.child)
    }
    /// All exchanges occur on a control thread. An incomplete exchange closes the pipe so an unacknowledged START is detached.
    fn exchange(&self, command: Option<(u8, CableId)>) -> io::Result<u64> {
        let mut slot = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        let pipe = slot.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        let packet: io::Result<[u8; 12]> = (|| {
            if let Some((op, id)) = command {
                let mut request = [0; 17];
                request[0] = op;
                request[1..].copy_from_slice(&id.0.to_le_bytes());
                pipe.write_all(&request)?;
            }
            let start = Instant::now();
            while available(pipe)? < 12 {
                if !alive(&self.child) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                if start.elapsed() >= IO_LIMIT {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                std::thread::sleep(POLL);
            }
            let mut packet = [0; 12];
            pipe.read_exact(&mut packet)?;
            Ok(packet)
        })();
        let packet = packet.inspect_err(|_| {
            self.connected.store(false, Ordering::Release);
            *slot = None;
        })?;
        let code = i32::from_le_bytes(packet[..4].try_into().unwrap());
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(u64::from_le_bytes(packet[4..].try_into().unwrap()))
    }
}

/// Snapshot for the settings window, outside its audio path. Never waits for a UAC prompt.
pub fn authorized() -> bool {
    APPROVED.try_lock().is_ok_and(|slot| slot.as_ref().is_some_and(|s| s.connected()))
}

/// Explicit control-thread action. An approved session is reusable across Stop/Start. Device retries never launch this helper.
pub fn authorize() -> io::Result<()> {
    static PROMPTING: AtomicBool = AtomicBool::new(false);
    if authorized() {
        return Ok(());
    }
    if PROMPTING.swap(true, Ordering::AcqRel) {
        return Err(io::Error::from_raw_os_error(ERROR_BUSY.0 as i32));
    }
    // A second caller may have completed its prompt between the initial snapshot and the prompting guard.
    let result = if APPROVED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| s.connected()) {
        Ok(())
    } else {
        connection::launch().map(|session| *APPROVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(session)))
    };
    PROMPTING.store(false, Ordering::Release);
    result
}

/// A live writer. Only `ring` and `finished` belong in a real-time loop. Drop synchronously tears down this feed on its control path.
pub struct LiveFeed(Feed);

enum Feed {
    Direct(Attachment),
    Broker { ring: RingMemory, session: Arc<Session>, id: CableId, token: Arc<()> },
}

impl LiveFeed {
    pub fn ring(&self) -> &RingMemory {
        match &self.0 {
            Feed::Direct(a) => a.ring(),
            Feed::Broker { ring, .. } => ring,
        }
    }

    pub fn finished(&self) -> bool {
        match &self.0 {
            Feed::Direct(a) => a.finished(),
            Feed::Broker { ring, session, .. } => {
                ring.header().state.load(Ordering::Acquire) & protocol::STATE_ATTACHED == 0 || !session.connected()
            }
        }
    }
}

impl Drop for LiveFeed {
    fn drop(&mut self) {
        if let Feed::Broker { session, id, token, .. } = &self.0 {
            let mut leases = session.leases.lock().unwrap_or_else(|e| e.into_inner());
            if leases.release(*id, token) {
                let _ = session.exchange(Some((STOP, *id)));
            }
        }
    }
}

pub fn attach_live(capacity: u32) -> io::Result<LiveFeed> {
    let cables = crate::cables::present().map_err(io::Error::other)?;
    let cable = crate::cables::choose_default(&cables).map_err(|e| io::Error::from_raw_os_error(e.win32() as i32))?;
    attach_live_to(cable.cable.id, capacity)
}

/// Feed just this cable. Other cables can be attached independently to the same approved session.
pub fn attach_live_to(id: CableId, capacity: u32) -> io::Result<LiveFeed> {
    match Device::open_path(&protocol::control_path(&id)) {
        Ok(device) => return device.attach_live(RingMemory::new(capacity)?).map(|a| LiveFeed(Feed::Direct(a))).map_err(|e| e.error),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {}
        Err(e) => return Err(e),
    }
    if capacity != LIVE_CAPACITY {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let session = APPROVED.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|s| s.connected());
    let session = session.ok_or_else(denied)?;
    // Serialize START plus recording who owns it with the matching STOP. No control or pipe locks reach the audio thread.
    let mut leases = session.leases.lock().unwrap_or_else(|e| e.into_inner());
    let handle = session.exchange(Some((START, id)))?;
    // SAFETY: the authenticated helper transfers ownership of one real section in our handle table only in a successful START reply.
    let section = unsafe { Owned::new(HANDLE(handle as usize as _)) };
    let ring = RingMemory::shared_writer(*section, capacity).inspect_err(|_| {
        let _ = session.exchange(Some((STOP, id)));
    })?;
    let token = leases.replace(id);
    drop(leases);
    Ok(LiveFeed(Feed::Broker { ring, session, id, token }))
}

fn denied() -> io::Error {
    io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32)
}

/// Entry point for the dedicated broker executable. It loads no application settings, plug-ins or UI.
pub fn helper_entry() -> Option<u8> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != helper::FLAG {
        return None;
    }
    let result = match (args.next(), args.next(), args.next()) {
        (Some(pid), Some(nonce), None) => pid
            .to_str()
            .and_then(|p| p.parse::<u32>().ok())
            .zip(nonce.to_str())
            .filter(|(pid, nonce)| *pid != 0 && connection::valid_nonce(nonce))
            .ok_or_else(denied)
            .and_then(|(pid, nonce)| helper::serve(pid, nonce)),
        _ => Err(denied()),
    };
    Some(u8::from(result.is_err()))
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn a_removed_writers_drop_cannot_stop_its_replacement_or_another_cable() {
        let mut leases = Leases::default();
        let a = CableId(1);
        let b = CableId(2);
        let old = leases.replace(a);
        let other = leases.replace(b);
        let new = leases.replace(a);
        assert!(!leases.release(a, &old));
        assert!(!leases.release(b, &new));
        assert!(leases.release(a, &new));
        assert!(leases.release(b, &other));
        assert!(!leases.release(a, &new));
    }
}
