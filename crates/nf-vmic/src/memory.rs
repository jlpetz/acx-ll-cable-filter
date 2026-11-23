//! The ring's memory and its writer.
//!
//! [`RingMemory`] is one page-aligned, zeroed allocation or shared section with line 0 of the header filled in: what ATTACH takes as its
//! output buffer. Once attached it is marked so: the driver starts every attachment at frame 0, so a ring goes back to the
//! driver only after [`RingMemory::reset`]. [`Writer`] writes frames from `write_frame` on into free slots only
//! (`[write_frame, read_frame + capacity)`), then publishes `write_frame` with release ordering. [`top_up`] is the pacing rule:
//! keep the fill (`write_frame - read_frame`) at a target by watching `read_frame`, so the writer follows the driver's QPC clock
//! rather than its own.

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualLock,
};
use windows::core::{Owned, PCWSTR};

use crate::protocol::{self, CHANNELS, FORMAT_FLOAT32, FRAME_BYTES, HEADER_BYTES, LINE_BYTES, MAGIC, RingHeader, SAMPLE_RATE, VERSION};
use crate::ring::slot;

/// A ring header and its frames, in a private reservation or a pagefile section. Both are page-aligned and released on drop.
///
/// While attached, an [`Attachment`](crate::Attachment) owns it, so it can't be freed under the driver.
pub struct RingMemory {
    base: NonNull<u8>,
    capacity: u32,
    /// [`Device::attach`](crate::Device::attach) sent it to the driver, which may have read it: cleared by [`Self::reset`].
    attached_before: bool,
    mapped: bool,
}

impl RingMemory {
    /// A zeroed ring of `capacity_frames` (a power of two within the protocol's limits), header line 0 set and `write_frame` 0.
    pub fn new(capacity_frames: u32) -> io::Result<Self> {
        if !protocol::capacity_ok(capacity_frames) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a ring holds a power of two of frames from {} to {}, not {capacity_frames}",
                    protocol::MIN_CAPACITY_FRAMES,
                    protocol::MAX_CAPACITY_FRAMES
                ),
            ));
        }
        let bytes = protocol::ring_bytes(capacity_frames);
        // SAFETY: a fresh private reservation and commit at an address of the system's choosing; nothing else refers to it.
        // VirtualAlloc returns committed pages zeroed.
        let p = unsafe { VirtualAlloc(None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        let base = NonNull::new(p.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        Ok(Self::initialize(base, capacity_frames, false))
    }

    /// Creates the broker's anonymous section. The broker initializes and attaches it before allowing the app to map a view.
    pub(crate) fn shared(capacity: u32) -> io::Result<(Self, Owned<HANDLE>)> {
        if !protocol::capacity_ok(capacity) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let bytes = protocol::ring_bytes(capacity);
        // SAFETY: pagefile backing with a fixed, validated length and no globally accessible name.
        let section = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, bytes as u32, PCWSTR::null()) }
            .map_err(crate::device::io_error)?;
        // SAFETY: CreateFileMappingW transferred ownership of this handle.
        let section = unsafe { Owned::new(section) };
        let base = Self::map_section(*section, capacity)?;
        Ok((Self::initialize(base, capacity, true), section))
    }

    /// The app's view of an authenticated broker section. Its immutable header was initialized before transfer.
    pub(crate) fn shared_writer(section: HANDLE, capacity: u32) -> io::Result<Self> {
        let ring = Self { base: Self::map_section(section, capacity)?, capacity, attached_before: true, mapped: true };
        // SAFETY: our live section view spans exactly `bytes`. Lock the writer's own working-set view before the audio loop.
        unsafe { VirtualLock(ring.base.as_ptr().cast(), ring.bytes()) }.map_err(crate::device::io_error)?;
        Ok(ring)
    }

    fn map_section(section: HANDLE, capacity: u32) -> io::Result<NonNull<u8>> {
        // SAFETY: the caller owns a readable/writable section handle. Windows verifies the requested length fits its section.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, protocol::ring_bytes(capacity)) };
        NonNull::new(view.Value.cast()).ok_or_else(io::Error::last_os_error)
    }

    fn initialize(base: NonNull<u8>, capacity_frames: u32, mapped: bool) -> Self {
        // SAFETY: `base` is page-aligned (more than RingHeader's 8), writable and at least HEADER_BYTES long; all-zero bytes are a
        // valid RingHeader (integers, atomics and byte arrays); and no other reference to the memory exists yet.
        let h = unsafe { &mut *base.as_ptr().cast::<RingHeader>() };
        h.magic = MAGIC;
        h.version = VERSION;
        h.header_bytes = HEADER_BYTES;
        h.capacity_frames = capacity_frames;
        h.sample_rate = SAMPLE_RATE;
        h.channels = CHANNELS;
        h.sample_format = FORMAT_FLOAT32;
        h.frame_bytes = FRAME_BYTES;
        h.flags = 0;
        h.total_bytes = protocol::ring_bytes(capacity_frames) as u64;
        Self { base, capacity: capacity_frames, attached_before: false, mapped }
    }

    /// Whether the ring went to the driver before (its ATTACH went pending). [`Device::attach`](crate::Device::attach) refuses
    /// such a ring until [`Self::reset`]: the driver starts every attachment at frame 0, so an old `write_frame` would replay
    /// old frames (or re-sync, past a capacity).
    pub fn attached_before(&self) -> bool {
        self.attached_before
    }

    pub(crate) fn mark_attached(&mut self) {
        self.attached_before = true;
    }

    pub(crate) fn set_attach_version(&mut self, version: u32) {
        // SAFETY: attach calls this only on fresh or reset memory, before the driver can access the header.
        unsafe { (*self.base.as_ptr().cast::<RingHeader>()).version = version };
    }

    /// Makes the ring as good as new for another ATTACH: lines 1 to 3 of the header zeroed (`write_frame` 0, the driver's line
    /// cleared), line 0 kept, the mark cleared. The frames stay as they are: none before `write_frame` is ever read. `&mut`: no
    /// attachment or [`Writer`] holds it.
    pub fn reset(&mut self) {
        // SAFETY: bytes LINE_BYTES to HEADER_BYTES are inside the allocation and hold only integers, atomics and byte arrays
        // (all-zero is valid for each). `&mut self`: no reference into the header exists, and no request holds the ring (an
        // Attachment owns it while one may).
        unsafe { self.base.as_ptr().add(LINE_BYTES as usize).write_bytes(0, (HEADER_BYTES - LINE_BYTES) as usize) };
        self.attached_before = false;
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The ring's length: ATTACH's output length.
    pub fn bytes(&self) -> usize {
        protocol::ring_bytes(self.capacity)
    }

    pub fn header(&self) -> &RingHeader {
        // SAFETY: `base` holds an initialised RingHeader for as long as `self` lives. Once attached, the driver writes only line 2,
        // which is atomics, and the app writes only `write_frame`, an atomic, so a shared reference stays valid.
        unsafe { &*self.base.as_ptr().cast::<RingHeader>() }
    }

    /// The start of the region, for ATTACH's output buffer.
    pub(crate) fn as_mut_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// The first slot. Writes go through this pointer (no reference to the frames is ever made while the driver may read them).
    fn slots(&self) -> *mut [f32; 2] {
        // SAFETY: HEADER_BYTES is inside the allocation (it is HEADER_BYTES + capacity * 8 long), and 256 keeps 4-byte alignment.
        unsafe { self.base.as_ptr().add(HEADER_BYTES as usize).cast::<[f32; 2]>() }
    }

    /// The frames, for a reader in this process: the tests' [`ReaderModel`](crate::ring::ReaderModel), with no driver attached.
    #[cfg(test)]
    pub(crate) fn frames_for_test(&self) -> &[[f32; 2]] {
        // SAFETY: `capacity` slots from `slots()` are inside the allocation; the tests write only between their reads.
        unsafe { std::slice::from_raw_parts(self.slots(), self.capacity as usize) }
    }
}

impl std::fmt::Debug for RingMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingMemory")
            .field("capacity", &self.capacity)
            .field("attached_before", &self.attached_before)
            .finish_non_exhaustive()
    }
}

impl Drop for RingMemory {
    fn drop(&mut self) {
        // SAFETY: `base` is the start of our own reservation, released once here. No request holds it: an Attachment owns the
        // ring while the request is pending and gives it up (or leaks it) only after the request has completed.
        if self.mapped {
            // SAFETY: the caller owns the view created by MapViewOfFile.
            let _ = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base.as_ptr().cast() }) };
        } else {
            // SAFETY: base is the private reservation returned by VirtualAlloc.
            let _ = unsafe { VirtualFree(self.base.as_ptr().cast(), 0, MEM_RELEASE) };
        }
    }
}

/// The app's side of the ring: writes frames from `write_frame` on and publishes them.
pub struct Writer<'r> {
    ring: &'r RingMemory,
    write_frame: u64,
}

impl<'r> Writer<'r> {
    /// Takes up from the header's `write_frame`: 0 for a fresh or reset ring, the pre-fill an earlier writer left before ATTACH,
    /// or where the attachment's previous writer stopped.
    pub fn new(ring: &'r RingMemory) -> Self {
        Self { ring, write_frame: ring.header().write_frame.load(Relaxed) }
    }

    pub fn write_frame(&self) -> u64 {
        self.write_frame
    }

    /// The driver's position. Acquire: once this is seen, the driver has finished with the slots before it.
    pub fn read_frame(&self) -> u64 {
        self.ring.header().read_frame.load(Acquire)
    }

    /// Frames written and not yet read (0 if the driver were ahead, which only a broken writer can cause).
    pub fn fill(&self) -> u64 {
        self.write_frame.saturating_sub(self.read_frame())
    }

    /// Slots free for writing: the capacity less the fill.
    pub fn free(&self) -> u64 {
        u64::from(self.ring.capacity()).saturating_sub(self.fill())
    }

    /// Writes up to `count` frames, no more than are free, frame `n` (absolute position) being `f(n)`; then publishes them.
    /// Returns how many it wrote. Doesn't allocate.
    pub fn produce(&mut self, count: u64, mut f: impl FnMut(u64) -> [f32; 2]) -> u64 {
        let n = count.min(self.free());
        let slots = self.ring.slots();
        let capacity = self.ring.capacity();
        for pos in self.write_frame..self.write_frame + n {
            // SAFETY: slot() is below the capacity, so inside the allocation. The slot is free: pos < read_frame + capacity
            // (n <= free), and the driver reads only [read_frame, write_frame), which stops before `self.write_frame` <= pos.
            unsafe { slots.add(slot(pos, capacity)).write(f(pos)) };
        }
        self.write_frame += n;
        self.ring.header().write_frame.store(self.write_frame, Release);
        n
    }
}

/// The pacing rule: the frames that bring the fill up to `target`, no more than are free.
pub fn top_up(target: u64, fill: u64, free: u64) -> u64 {
    target.saturating_sub(fill).min(free)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::ReaderModel;
    use crate::signal::{SequenceChecker, frame};

    #[test]
    fn a_fresh_ring_is_what_the_driver_checks() {
        let ring = RingMemory::new(4096).unwrap();
        assert!(!ring.attached_before());
        assert!(ring.as_mut_ptr().addr().is_multiple_of(4096), "page-aligned");
        let h = ring.header();
        assert_eq!((h.magic, h.version, h.header_bytes, h.capacity_frames), (MAGIC, VERSION, 256, 4096));
        assert_eq!((h.sample_rate, h.channels, h.sample_format, h.frame_bytes, h.flags), (48_000, 2, 1, 8, 0));
        assert_eq!(h.total_bytes, 256 + 4096 * 8);
        assert_eq!(ring.bytes(), 256 + 4096 * 8);
        assert_eq!((h.write_frame.load(Relaxed), h.read_frame.load(Relaxed), h.state.load(Relaxed)), (0, 0, 0));
        assert!(RingMemory::new(1000).is_err());
        assert!(RingMemory::new(512).is_err(), "below the minimum");
    }

    #[test]
    fn a_shared_writer_sees_the_existing_ring_and_keeps_the_section_alive() {
        let (broker, handle) = RingMemory::shared(1024).unwrap();
        let writer = RingMemory::shared_writer(*handle, 1024).unwrap();
        drop(handle);
        assert!(writer.attached_before());
        assert_eq!(writer.header().magic, MAGIC);
        assert_eq!(Writer::new(&writer).produce(7, frame), 7);
        assert_eq!(broker.header().write_frame.load(Acquire), 7);
        assert_eq!(broker.frames_for_test()[6], frame(6));
        drop(writer);
        assert_eq!(broker.header().write_frame.load(Acquire), 7);
    }

    /// reset() zeroes lines 1 to 3 (write_frame and the driver's line), keeps line 0, and clears the mark.
    #[test]
    fn reset_makes_a_used_ring_new() {
        let mut ring = RingMemory::new(1024).unwrap();
        Writer::new(&ring).produce(700, frame);
        ring.mark_attached();
        // What a driver leaves in its line, and stray bytes in lines 1 and 3.
        let h = ring.header();
        h.read_frame.store(600, Relaxed);
        h.frames_copied.store(600, Relaxed);
        h.state.store(protocol::STATE_ATTACHED, Relaxed);
        // SAFETY: inside the header (reserved bytes of lines 1 and 3); no reference to them is alive.
        unsafe {
            ring.as_mut_ptr().add(100).write(0xAA);
            ring.as_mut_ptr().add(255).write(0xBB);
        }
        assert!(ring.attached_before());
        ring.reset();
        assert!(!ring.attached_before());
        let h = ring.header();
        assert_eq!((h.magic, h.version, h.capacity_frames, h.total_bytes), (MAGIC, VERSION, 1024, protocol::ring_bytes(1024) as u64));
        // SAFETY: the header's 256 bytes, read while nothing writes them.
        let bytes = unsafe { std::slice::from_raw_parts(ring.as_mut_ptr(), HEADER_BYTES as usize) };
        assert!(bytes[LINE_BYTES as usize..].iter().all(|&b| b == 0));
        assert_eq!(Writer::new(&ring).write_frame(), 0);
    }

    #[test]
    fn the_writer_never_overwrites_unread_frames() {
        let ring = RingMemory::new(1024).unwrap();
        let mut w = Writer::new(&ring);
        assert_eq!(w.produce(5000, frame), 1024);
        assert_eq!((w.fill(), w.free()), (1024, 0));
        assert_eq!(w.produce(1, frame), 0);
        assert_eq!(ring.header().write_frame.load(Relaxed), 1024);
    }

    /// The writer against the model reader over the same memory: many wraps, a pacing target, packets that don't divide the
    /// ring, and a writer that pauses (underruns). Every frame arrives once, in order, with silence where the writer was late.
    #[test]
    fn writer_and_reader_through_wraps_and_underruns() {
        let ring = RingMemory::new(1024).unwrap();
        let mut w = Writer::new(&ring);
        let mut reader = ReaderModel::new(1024);
        let mut checker = SequenceChecker::new();
        let mut packet = [[0.0f32; 2]; 480];
        let mut silent = 0;
        for pass in 0..2000u32 {
            // The writer stalls for 3 passes in every 100.
            if !(50..53).contains(&(pass % 100)) {
                w.produce(top_up(1000, w.fill(), w.free()), frame);
            }
            let write_frame = ring.header().write_frame.load(Acquire);
            let p = reader.pass(write_frame, ring.frames_for_test(), &mut packet);
            assert!(!p.resynced);
            silent += p.zeroed;
            // The driver publishes its position after the copy.
            ring.header().read_frame.store(reader.read_frame, Release);
            packet.iter().for_each(|f| checker.feed(*f));
        }
        assert!(checker.is_clean(), "{:?}", checker.report());
        assert!(silent > 0, "the stalls underran");
        let r = checker.report();
        assert_eq!((r.zeros_before_signal, r.zeros_inside, r.zeros_after_signal), (0, silent as u64, 0));
        // 2000 passes of 480 frames: round the 1024-frame ring over 900 times.
        assert!(reader.read_frame > 900 * 1024);
        assert_eq!(reader.frames_copied, w.write_frame() - w.fill());
    }

    #[test]
    fn top_up_keeps_the_target() {
        assert_eq!(top_up(960, 0, 8192), 960);
        assert_eq!(top_up(960, 500, 8192), 460);
        assert_eq!(top_up(960, 960, 8192), 0);
        assert_eq!(top_up(960, 2000, 0), 0);
        assert_eq!(top_up(960, 100, 300), 300);
    }
}
