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
use std::sync::atomic::Ordering::Acquire;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, Owned, PCWSTR};

use crate::memory::RingMemory;
use crate::protocol::{AttachIn, IOCTL_ATTACH, IOCTL_STATS, MAGIC, STATE_ATTACHED, Stats, USER_PATH, VERSION};

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

/// The control device `\\.\NoiseFilterVmic`, opened for overlapped I/O, closed on drop.
pub struct Device {
    handle: Owned<HANDLE>,
}

impl Device {
    /// Opens the control device. `ERROR_FILE_NOT_FOUND`: there is no control device (the driver, with its app feed, isn't
    /// running, or its PnP device is gone); `ERROR_ACCESS_DENIED`: the spike's device admits SYSTEM and Administrators only, so
    /// the caller isn't elevated.
    pub fn open() -> io::Result<Self> {
        let path = HSTRING::from(USER_PATH);
        // SAFETY: `path` is NUL-terminated and outlives the call; no security attributes, no template.
        let h = unsafe {
            CreateFileW(
                &path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(io_error)?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        Ok(Self { handle: unsafe { Owned::new(h) } })
    }

    /// The driver's statistics (NF_VMIC_IOCTL_STATS), checked for the protocol's magic, version and size. `ERROR_BAD_COMMAND`:
    /// this handle's control device was deleted ([`crate::protocol::QUEUE_PURGED`]): drop this Device and open a new one.
    pub fn stats(&self) -> io::Result<Stats> {
        let mut stats = Stats::default();
        let event = new_event()?;
        let mut overlapped = OVERLAPPED { hEvent: *event, ..Default::default() };
        // Raw pointers only from here until the request has completed: the system writes both behind Rust's back.
        let (stats_ptr, overlapped_ptr) = (&raw mut stats, &raw mut overlapped);
        let mut bytes = 0u32;
        // SAFETY: our handle. `stats` and `overlapped` outlive the request: it either fails here at once, or completes before
        // GetOverlappedResult (which waits: it doesn't return while the request is pending) returns.
        let r = unsafe {
            DeviceIoControl(
                *self.handle,
                IOCTL_STATS,
                None,
                0,
                Some(stats_ptr.cast()),
                size_of::<Stats>() as u32,
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
        unsafe { GetOverlappedResult(*self.handle, overlapped_ptr, &raw mut bytes, true) }.map_err(io_error)?;
        if bytes as usize != size_of::<Stats>()
            || stats.magic != MAGIC
            || stats.version != VERSION
            || stats.stats_bytes as usize != size_of::<Stats>()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the driver's statistics aren't this tool's ({bytes} bytes, magic {:#x}, version {}, size {}): rebuild one",
                    stats.magic, stats.version, stats.stats_bytes
                ),
            ));
        }
        Ok(stats)
    }

    /// Hands `ring` to the driver (NF_VMIC_IOCTL_ATTACH). On success the request stays pending and the driver reads the ring
    /// from the capture stream's timer. On failure the error says why ([`crate::protocol::refusal`] explains the Win32 codes, with
    /// [`AttachError::pending`]), and the ring comes back as [`AttachError`] describes.
    ///
    /// A ring that was attached before ([`RingMemory::attached_before`]) is refused (`ErrorKind::InvalidInput`, the ring handed
    /// back) until [`RingMemory::reset`]: the driver starts every attachment at frame 0.
    pub fn attach(&self, mut ring: RingMemory) -> Result<Attachment<'_>, AttachError> {
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
        let input = AttachIn::new();
        // SAFETY: our handle. The input is copied into the system buffer during the call. The ring and the OVERLAPPED belong to
        // the Attachment made below, which keeps both until the request has completed; if the call fails at once, the request
        // never started and both come straight back.
        let r = unsafe {
            DeviceIoControl(
                *self.handle,
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
        let mut attachment = Attachment { device: self, ring: Some(ring), overlapped, event, abandoned: false, _not_send: PhantomData };
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
pub struct Attachment<'d> {
    device: &'d Device,
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

impl Attachment<'_> {
    /// The attached ring (write it with a [`Writer`](crate::Writer)).
    pub fn ring(&self) -> &RingMemory {
        self.ring.as_ref().expect("an Attachment holds its ring until it ends")
    }

    /// `Some(status)` once the request has completed: the driver ended the attachment ([`crate::protocol::ending`] explains the
    /// status: `ERROR_DEVICE_REMOVED` means close every handle). `None` while the ring is live.
    pub fn ended(&self) -> Option<io::Error> {
        if self.abandoned || !is_set(&self.event) {
            return None;
        }
        let mut bytes = 0u32;
        // SAFETY: our handle and this request's OVERLAPPED; its event is set, so the request has completed and this doesn't wait.
        match unsafe { GetOverlappedResult(*self.device.handle, self.overlapped.as_ptr(), &raw mut bytes, false) } {
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
        let _ = unsafe { CancelIoEx(*self.device.handle, Some(self.overlapped.as_ptr())) };
        let mut bytes = 0u32;
        // SAFETY: our handle and this request's OVERLAPPED; waits up to DETACH_WAIT_MS for it to complete.
        let r = unsafe { GetOverlappedResultEx(*self.device.handle, self.overlapped.as_ptr(), &raw mut bytes, DETACH_WAIT_MS, false) };
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

impl Drop for Attachment<'_> {
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

    /// attach refuses a ring marked attached-before without any I/O (so a Device that isn't the driver's will do), and hands it
    /// back; reset() makes it acceptable again.
    #[test]
    fn a_used_ring_is_refused_until_reset() {
        // A Device whose handle is an event: never the driver, and never asked anything (the refusal comes first).
        let device = Device { handle: new_event().unwrap() };
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
