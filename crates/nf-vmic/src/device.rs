//! The control device: open it, read its statistics, attach a ring.
//!
//! The handle is opened `FILE_FLAG_OVERLAPPED`, so every IOCTL on it carries an OVERLAPPED with its own event. ATTACH stays
//! pending for as long as the ring is attached: an [`Attachment`] owns the ring and the OVERLAPPED until the request has
//! completed, so neither can be freed while the driver or the I/O manager may still use them. Detaching is cancelling:
//! CancelIoEx, then one wait for the request to complete (the driver unpublishes the ring before it completes it, and
//! completion unlocks the pages). [`crate::protocol::ATTACH_REFUSALS`], [`crate::protocol::ATTACH_FAILURES_AT_ONCE`] and
//! [`crate::protocol::ATTACH_ENDINGS`] list how ATTACH completes.
//!
//! An OVERLAPPED that a pending request uses is written by the I/O manager when the request completes, behind Rust's back: it
//! is a heap allocation reached only through a raw pointer, and no reference to it is made while the request may be pending.
//!
//! Windows cancels the I/O a thread issued when the thread exits (inference from the Win32 documentation on CancelIo; not
//! re-read for this): an [`Attachment`] is `!Send`, to stay on the thread that made it, which should also be the one that
//! writes the ring and that lives as long as the attachment.

use std::io;
use std::marker::PhantomData;
use std::mem::size_of;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::Ordering::Acquire;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, Owned, PCWSTR};

use crate::cables;
use crate::memory::RingMemory;
use crate::protocol::{AttachIn, IOCTL_ATTACH, IOCTL_STATS, MAGIC, STATE_ATTACHED, Stats, VERSION};

/// How long a detach waits, once, for the driver to complete the request after the cancel before giving up on it (and leaking
/// the ring and the OVERLAPPED rather than freeing them under the driver).
pub const DETACH_WAIT_MS: u32 = 5_000;
/// How long an accepted ATTACH may take to show `STATE_ATTACHED` (the driver sets it before it leaves the request pending, so
/// in practice it is there at once).
const ATTACH_CONFIRM: Duration = Duration::from_secs(1);

/// A Win32 error as an `io::Error`, keeping the Win32 code where there is one (`raw_os_error`).
pub(crate) fn io_error(e: windows::core::Error) -> io::Error {
    match WIN32_ERROR::from_error(&e) {
        Some(w) => io::Error::from_raw_os_error(w.0 as i32),
        None => io::Error::other(e),
    }
}

fn win32(e: &windows::core::Error) -> Option<WIN32_ERROR> {
    WIN32_ERROR::from_error(e)
}

/// A manual-reset event for one OVERLAPPED, closed on drop.
fn new_event() -> io::Result<Owned<HANDLE>> {
    // SAFETY: an unnamed manual-reset event, not signalled, default security.
    let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(io_error)?;
    // SAFETY: a valid handle that nothing else owns; Owned closes it once.
    Ok(unsafe { Owned::new(h) })
}

/// Whether `event` (an OVERLAPPED's, manual-reset) is set: the I/O manager sets it when the request completes, after it has
/// written the OVERLAPPED, and nothing resets it.
fn is_set(event: &Owned<HANDLE>) -> bool {
    // SAFETY: our event handle; a zero timeout only tests it.
    (unsafe { WaitForSingleObject(**event, 0) }) == WAIT_OBJECT_0
}

/// A cable's control device (`\\.\AcxLlCable_<id>`, [`crate::protocol::control_path`]), opened for overlapped I/O, closed on
/// drop.
#[derive(Clone)]
pub struct Device {
    handle: Rc<Owned<HANDLE>>,
}

impl Device {
    /// Opens the control device at `path`, read and write. `ERROR_FILE_NOT_FOUND`: there is no such control device (no cable
    /// has that identity, or its device is disabled, removed or being updated, or the driver running isn't the per-cable one);
    /// `ERROR_ACCESS_DENIED`: full access requires SYSTEM or Administrators. A signed-in user can call [`Self::diagnostics`].
    pub fn open_path(path: &str) -> io::Result<Self> {
        Self::open_with_access(path, (GENERIC_READ | GENERIC_WRITE).0)
    }

    fn open_with_access(path: &str, access: u32) -> io::Result<Self> {
        let path = HSTRING::from(path);
        // SAFETY: `path` is NUL-terminated and outlives the call; no security attributes, no template.
        let h = unsafe { CreateFileW(&path, access, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAG_OVERLAPPED, None) }
            .map_err(io_error)?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        Ok(Self { handle: Rc::new(unsafe { Owned::new(h) }) })
    }

    /// Opens the default cable's control device: the one labelled A, else the only cable ([`cables::choose_default`] over
    /// [`cables::present`]). Interim (TODO 19, N0): until the broker and the app name the cable they feed (B1, E1), they feed
    /// this one, as they fed the one control device before. `ERROR_FILE_NOT_FOUND` with no cable,
    /// `ERROR_AMBIGUOUS_SYSTEM_DEVICE` with several and none labelled A; otherwise as [`Device::open_path`].
    pub fn open_default() -> io::Result<Self> {
        Self::open_path(&cables::default_control_path()?)
    }

    /// The driver's statistics (NF_VMIC_IOCTL_STATS), checked for the protocol's magic, version and size. `ERROR_BAD_COMMAND`:
    /// this handle's control device was deleted ([`crate::protocol::QUEUE_PURGED`]): drop this Device and open a new one.
    pub fn stats(&self) -> io::Result<Stats> {
        let (stats, returned): (Stats, _) = self.read_reply(IOCTL_STATS)?;
        if returned != size_of::<Stats>() || stats.magic != MAGIC || stats.version != VERSION || stats.stats_bytes as usize != returned {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "incompatible engineering statistics layout"));
        }
        Ok(stats)
    }

    pub fn diagnostics(id: crate::identity::CableId) -> io::Result<crate::protocol::Diagnostics> {
        use crate::protocol::{DiagnosticReply, IOCTL_DIAGNOSTICS, control_path};
        let device = Self::open_with_access(&control_path(&id), GENERIC_READ.0)?;
        let (reply, returned): (DiagnosticReply, _) = device.read_reply(IOCTL_DIAGNOSTICS)?;
        reply.decode(returned).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "incompatible cable diagnostics layout"))
    }

    // Called only with repr(C) integer-only ABI replies. No audio thread issues diagnostic requests.
    fn read_reply<T: Default>(&self, code: u32) -> io::Result<(T, usize)> {
        let mut stats = T::default();
        let event = new_event()?;
        let mut overlapped = OVERLAPPED { hEvent: *event, ..Default::default() };
        // Raw pointers only from here until the request has completed: the system writes both behind Rust's back.
        let (stats_ptr, overlapped_ptr) = (&raw mut stats, &raw mut overlapped);
        let mut bytes = 0u32;
        // SAFETY: our handle. `stats` and `overlapped` outlive the request: it either fails here at once, or completes before
        // GetOverlappedResult (which waits: it doesn't return while the request is pending) returns.
        let r = unsafe {
            DeviceIoControl(
                **self.handle,
                code,
                None,
                0,
                Some(stats_ptr.cast()),
                size_of::<T>() as u32,
                Some(&raw mut bytes),
                Some(overlapped_ptr),
            )
        };
        if let Err(e) = r
            && win32(&e) != Some(ERROR_IO_PENDING)
        {
            return Err(io_error(e));
        }
        // SAFETY: our handle and this request's OVERLAPPED; waits for it to complete.
        unsafe { GetOverlappedResult(**self.handle, overlapped_ptr, &raw mut bytes, true) }.map_err(io_error)?;
        Ok((stats, bytes as usize))
    }

    /// Hands `ring` to the driver (NF_VMIC_IOCTL_ATTACH). On success the request stays pending and the driver reads the ring
    /// from the capture stream's timer. On failure the error says why ([`crate::protocol::refusal`] explains the Win32 codes, with
    /// [`AttachError::pending`]), and the ring comes back as [`AttachError`] describes.
    ///
    /// A ring that was attached before ([`RingMemory::attached_before`]) is refused (`ErrorKind::InvalidInput`, the ring handed
    /// back) until [`RingMemory::reset`]: the driver starts every attachment at frame 0.
    pub fn attach(&self, ring: RingMemory) -> Result<Attachment, AttachError> {
        self.attach_with_flags(ring, 0)
    }

    /// Live microphone feed: after the last reader stops, the next reader starts at fresh audio. An older driver
    /// refuses this flag, rather than replaying queued microphone samples from before the recording started.
    pub fn attach_live(&self, ring: RingMemory) -> Result<Attachment, AttachError> {
        self.attach_with_flags(ring, crate::protocol::ATTACH_LIVE)
    }

    fn attach_with_flags(&self, mut ring: RingMemory, flags: u32) -> Result<Attachment, AttachError> {
        if ring.attached_before() {
            let error = io::Error::new(
                io::ErrorKind::InvalidInput,
                "this ring was attached before: reset() it first (the driver starts every attachment at frame 0)",
            );
            return Err(AttachError { error, ring: Some(ring), pending: false });
        }
        let event = match new_event() {
            Ok(e) => e,
            Err(error) => return Err(AttachError { error, ring: Some(ring), pending: false }),
        };
        let overlapped = NonNull::from(Box::leak(Box::new(OVERLAPPED { hEvent: *event, ..Default::default() })));
        let version = if flags == 0 { VERSION } else { crate::protocol::LIVE_VERSION };
        ring.set_attach_version(version);
        let input = AttachIn { version, flags, ..AttachIn::new() };
        // SAFETY: our handle. The input is copied into the system buffer during the call. The ring and the OVERLAPPED belong to
        // the Attachment made below, which keeps both until the request has completed; if the call fails at once, the request
        // never started and both come straight back.
        let r = unsafe {
            DeviceIoControl(
                **self.handle,
                IOCTL_ATTACH,
                Some((&raw const input).cast()),
                size_of::<AttachIn>() as u32,
                Some(ring.as_mut_ptr().cast()),
                ring.bytes() as u32,
                None,
                Some(overlapped.as_ptr()),
            )
        };
        let error = match r {
            // The driver never completes an accepted ATTACH until a detach: completed at once is a refusal, or a driver that
            // isn't ours (which may have written the ring: marked).
            Ok(()) => {
                ring.mark_attached();
                Some(io::Error::other("the driver completed the ATTACH at once, with success"))
            }
            Err(e) if win32(&e) == Some(ERROR_IO_PENDING) => None,
            Err(e) => Some(io_error(e)),
        };
        if let Some(error) = error {
            // SAFETY: the request isn't pending (it failed or completed at once), so nothing writes the OVERLAPPED any more; the
            // Box leaked above, freed once.
            drop(unsafe { Box::from_raw(overlapped.as_ptr()) });
            return Err(AttachError { error, ring: Some(ring), pending: false });
        }
        // Pending: from here the driver may publish the ring, read it and write its line, so it is marked for good. (KMDF pends
        // every request it delivers, so the driver's refusals come this way too: from ended() below.)
        ring.mark_attached();
        let mut attachment =
            Attachment { device: self.clone(), ring: Some(ring), overlapped, event, abandoned: false, _not_send: PhantomData };
        let deadline = Instant::now() + ATTACH_CONFIRM;
        loop {
            if attachment.ring().header().state.load(Acquire) & STATE_ATTACHED != 0 {
                return Ok(attachment);
            }
            if let Some(error) = attachment.ended() {
                return Err(AttachError { error, ring: attachment.ring.take(), pending: true });
            }
            if Instant::now() >= deadline {
                let error = io::Error::new(io::ErrorKind::TimedOut, "the driver left ATTACH pending without publishing the ring");
                // Err: the cancel's one wait timed out and the ring is leaked (Drop then neither waits nor frees).
                let ring = attachment.cancel_and_wait().ok().and_then(|_| attachment.ring.take());
                return Err(AttachError { error, ring, pending: true });
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Why [`Device::attach`] failed, and the ring when it is safe to have back.
///
/// `ring`: `Some` when the request never started, or has completed. If it went pending first (the driver may have published
/// it), the ring is marked [`RingMemory::attached_before`]: [`RingMemory::reset`] it before attaching it again. `None`: the
/// request didn't complete within [`DETACH_WAIT_MS`] of its cancel, so the ring was leaked rather than freed under the driver.
#[derive(Debug)]
pub struct AttachError {
    pub error: io::Error,
    pub ring: Option<RingMemory>,
    /// The request went pending before it failed, so the driver saw it and `error` is GetOverlappedResult's (KMDF pends every
    /// request it delivers: every driver refusal comes this way). `false`: DeviceIoControl failed at once, before the driver saw
    /// it (the I/O manager's checks, or a purged queue), or attach refused the ring before any I/O.
    pub pending: bool,
}

/// An attached ring: the pending ATTACH, and everything it uses (the ring, the OVERLAPPED and its event) until it completes.
///
/// Dropping it detaches (cancel, then one wait of up to [`DETACH_WAIT_MS`]). If the request still hasn't completed by then, the
/// ring and the OVERLAPPED are leaked rather than freed while the driver may still use them; a drop after a [`Self::detach`] or
/// an attach whose wait timed out like that doesn't wait again.
pub struct Attachment {
    device: Device,
    ring: Option<RingMemory>,
    /// The request's OVERLAPPED: a Box's allocation, reached only through this pointer.
    overlapped: NonNull<OVERLAPPED>,
    /// The OVERLAPPED's event. The request holds the event object itself, not this handle, so closing the handle is safe
    /// whatever the request does.
    event: Owned<HANDLE>,
    /// A cancel's wait timed out: the request may still be pending, so the ring is leaked and the OVERLAPPED must be.
    abandoned: bool,
    _not_send: PhantomData<*const ()>,
}

impl Attachment {
    /// The attached ring (write it with a [`Writer`](crate::Writer)).
    pub fn ring(&self) -> &RingMemory {
        self.ring.as_ref().expect("an Attachment holds its ring until it ends")
    }

    /// Nonblocking completion check for a live audio writer. No error allocation or IOCTL.
    pub fn finished(&self) -> bool {
        is_set(&self.event)
    }

    /// `Some(status)` once the request has completed: the driver ended the attachment ([`crate::protocol::ending`] explains the
    /// status: `ERROR_DEVICE_REMOVED` means close every handle). `None` while the ring is live.
    pub fn ended(&self) -> Option<io::Error> {
        if self.abandoned || !is_set(&self.event) {
            return None;
        }
        let mut bytes = 0u32;
        // SAFETY: our handle and this request's OVERLAPPED; its event is set, so the request has completed and this doesn't wait.
        match unsafe { GetOverlappedResult(**self.device.handle, self.overlapped.as_ptr(), &raw mut bytes, false) } {
            Ok(()) => Some(io::Error::other("the driver completed the attachment with success")),
            Err(e) => Some(io_error(e)),
        }
    }

    /// Detaches: cancels the request, waits once for the driver to complete it, and gives the ring back (marked
    /// [`RingMemory::attached_before`]) with the request's final status (`ERROR_OPERATION_ABORTED`, 995, for a cancel).
    /// Err: the request didn't complete within [`DETACH_WAIT_MS`]; the ring is leaked.
    pub fn detach(mut self) -> io::Result<(RingMemory, io::Error)> {
        let status = self.cancel_and_wait()?;
        let ring = self.ring.take().expect("an Attachment holds its ring until it ends");
        Ok((ring, status))
    }

    /// Cancels the request (harmless if it has completed already) and waits once, up to [`DETACH_WAIT_MS`], for it to complete.
    /// Ok: its final status. Err: still pending, so the attachment is abandoned: the ring is leaked here and the OVERLAPPED in
    /// Drop.
    fn cancel_and_wait(&mut self) -> io::Result<io::Error> {
        if self.abandoned {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the attachment was abandoned after an earlier wait"));
        }
        // SAFETY: our handle; cancels only this OVERLAPPED's request (ERROR_NOT_FOUND, ignored, if it has completed).
        let _ = unsafe { CancelIoEx(**self.device.handle, Some(self.overlapped.as_ptr())) };
        let mut bytes = 0u32;
        // SAFETY: our handle and this request's OVERLAPPED; waits up to DETACH_WAIT_MS for it to complete.
        let r = unsafe { GetOverlappedResultEx(**self.device.handle, self.overlapped.as_ptr(), &raw mut bytes, DETACH_WAIT_MS, false) };
        let timed_out = matches!(&r, Err(e) if win32(e).is_some_and(|w| w.0 == WAIT_TIMEOUT.0 || w == ERROR_IO_INCOMPLETE));
        // Completed means the event is set, whatever the call said: anything else is treated as still pending.
        if timed_out || !is_set(&self.event) {
            self.abandoned = true;
            // The driver may still read and write the ring: it stays allocated.
            std::mem::forget(self.ring.take());
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the driver didn't complete the attachment within {DETACH_WAIT_MS} ms of the cancel: its ring and OVERLAPPED \
                     are leaked"
                ),
            ));
        }
        match r {
            Ok(()) => Ok(io::Error::other("the driver completed the attachment with success")),
            Err(e) => Ok(io_error(e)),
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Still holding the ring and not abandoned: not detached yet, so detach now (the one wait; its status goes unread).
        if self.ring.is_some() && !self.abandoned {
            let _ = self.cancel_and_wait();
        }
        if !self.abandoned {
            // SAFETY: the request has completed (detach, attach or the wait above saw its event set), so the I/O manager no
            // longer writes the OVERLAPPED; the Box leaked in attach, freed once.
            drop(unsafe { Box::from_raw(self.overlapped.as_ptr()) });
        }
        // The ring, if still here, drops after this: the request has completed, so freeing it is safe.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses the installed driver and an interactive logon. No endpoint is opened and no valid attachment is submitted.
    #[test]
    #[ignore = "live driver access check; requires an elevated interactive logon and at least one enabled cable"]
    fn interactive_read_only_diagnostics_and_feeder_rights() {
        use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION};
        use windows::Win32::Security::{
            CheckTokenMembership, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, ImpersonateLoggedOnUser, PSID, RevertToSelf,
            SID_AND_ATTRIBUTES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, WinBuiltinAdministratorsSid, WinInteractiveSid,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        use windows::core::BOOL;

        use crate::protocol::control_path;
        use crate::setup::lock::well_known_sid;
        use crate::setup::plan::Health;

        let cables = cables::present().unwrap().into_iter().filter(|c| c.health == Health::Working).collect::<Vec<_>>();
        assert!(!cables.is_empty(), "this test needs a working cable");
        // Check the full statistics path in its own authorized handle, then drop it before restricting this thread.
        for c in &cables {
            let authorized = Device::open_path(&control_path(&c.cable.id)).expect("run this test elevated");
            authorized.stats().unwrap_or_else(|e| panic!("{}: authorized engineering stats: {e}", c.cable.label));
        }
        // All remaining handles are opened after impersonating. The process token and other threads are untouched.
        let mut process_token = HANDLE::default();
        // SAFETY: the pseudo handle refers to this process. The output is an owned token handle.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE, &mut process_token) }.unwrap();
        // SAFETY: OpenProcessToken succeeded and transferred this handle to the caller.
        let process_token = unsafe { Owned::new(process_token) };
        let mut admins = well_known_sid(WinBuiltinAdministratorsSid).unwrap();
        let disabled = [SID_AND_ATTRIBUTES { Sid: PSID(admins.as_mut_ptr().cast()), Attributes: 0 }];
        let mut restricted = HANDLE::default();
        // SAFETY: the SID and token are live during this call. The returned token belongs to this thread.
        unsafe { CreateRestrictedToken(*process_token, DISABLE_MAX_PRIVILEGE, Some(&disabled), None, None, &mut restricted) }.unwrap();
        // SAFETY: CreateRestrictedToken succeeded and transferred the new token handle to the caller.
        let restricted = unsafe { Owned::new(restricted) };
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                // SAFETY: this scope began impersonation on this thread. Revert even if an assertion failed.
                unsafe { RevertToSelf() }.expect("restore the test thread token");
            }
        }
        // SAFETY: the restricted token is live, unshared, and impersonated only on this thread.
        unsafe { ImpersonateLoggedOnUser(*restricted) }.unwrap();
        let _revert = Revert;
        for (sid, expected) in [(WinBuiltinAdministratorsSid, false), (WinInteractiveSid, true)] {
            let mut sid = well_known_sid(sid).unwrap();
            let mut member = BOOL(0);
            // SAFETY: a null token tests this thread's effective token, using a live well-known SID and output.
            unsafe { CheckTokenMembership(None, PSID(sid.as_mut_ptr().cast()), &mut member) }.unwrap();
            assert_eq!(member.as_bool(), expected, "requires an interactive, non-administrative effective token");
        }
        for c in cables {
            let path = control_path(&c.cable.id);
            let label = c.cable.label;
            let denied = Some(ERROR_ACCESS_DENIED.0 as i32);
            assert_eq!(Device::open_path(&path).err().and_then(|e| e.raw_os_error()), denied, "{label}: writable open");
            Device::diagnostics(c.cable.id).unwrap_or_else(|e| panic!("{label}: read-only diagnostics: {e}"));
            let reader = Device::open_with_access(&path, GENERIC_READ.0).unwrap();
            let (legacy, bytes) = reader.read_reply::<[u64; 5]>(crate::protocol::IOCTL_DIAGNOSTICS).unwrap();
            assert_eq!((bytes, legacy[0] as u32, legacy[0] >> 32), (40, 1, 40), "{label}: v1 panel reply");
            // Deliberately omit both the valid header and the ring. Should authorization ever regress, this cannot install a feed.
            let attach = reader.read_reply::<u32>(IOCTL_ATTACH);
            assert_eq!(attach.err().and_then(|e| e.raw_os_error()), denied, "{label}: attachment IOCTL");
            let engineering = reader.stats().err();
            assert_eq!(engineering.as_ref().and_then(io::Error::raw_os_error), denied, "{label}: engineering STATS error {engineering:?}");
            let old = reader.read_reply::<Stats>(0x0022_6004).err();
            assert_eq!(
                old.as_ref().and_then(io::Error::raw_os_error),
                Some(ERROR_INVALID_FUNCTION.0 as i32),
                "{label}: retired STATS {old:?}"
            );
            eprintln!("Cable {label}: engineering stats readable when authorized; diagnostics only under the restricted interactive token");
        }
    }

    /// attach refuses a ring marked attached-before without any I/O (so a Device that isn't the driver's will do), and hands it
    /// back; reset() makes it acceptable again.
    #[test]
    fn a_used_ring_is_refused_until_reset() {
        // A Device whose handle is an event: never the driver, and never asked anything (the refusal comes first).
        let device = Device { handle: Rc::new(new_event().unwrap()) };
        let mut ring = RingMemory::new(1024).unwrap();
        ring.mark_attached();
        let e = device.attach(ring).err().expect("refused");
        assert_eq!((e.error.kind(), e.pending), (io::ErrorKind::InvalidInput, false));
        let mut ring = e.ring.expect("handed back");
        assert!(ring.attached_before());
        ring.reset();
        assert!(!ring.attached_before());
        // Reset, it gets as far as the IOCTL, which an event handle refuses at once: the ring comes back, not marked (the
        // request never started).
        let e = device.attach(ring).err().expect("an event isn't the driver");
        assert_ne!(e.error.kind(), io::ErrorKind::InvalidInput, "{}", e.error);
        assert!(!e.pending, "refused at once");
        assert!(!e.ring.expect("handed back").attached_before());
    }
}
