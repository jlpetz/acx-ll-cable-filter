//! The capture thread writes each direct ring in the same packet. Each cable has a control thread that creates and
//! releases its thread-affine ATTACH request, including when a removed cable reappears or the user resumes it.
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use llcable_sdk::driver::{ReaderSample, STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_LIVE_HANDOFF, STATE_READING};
use llcable_sdk::{CableId, LIVE_CAPACITY, LiveFeed, Writer, attach_live_to};

use super::clock::{Drift, Frame, interpolate};
use super::{Meter, State};

const WATCH: Duration = Duration::from_millis(50);
const RETRY: Duration = Duration::from_millis(500);

pub(super) struct Control {
    pub id: CableId,
    // Odd = enabled. Every actual change invalidates an earlier attachment, even a rapid pause/resume during an open.
    revision: AtomicU64,
    quit: AtomicBool,
    worker: OnceLock<Thread>,
}

impl Control {
    pub fn new(id: CableId, enabled: bool) -> Self {
        Self { id, revision: u64::from(enabled).into(), quit: false.into(), worker: OnceLock::new() }
    }

    pub fn enabled(&self) -> bool {
        self.revision.load(Acquire) & 1 != 0
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut current = self.revision.load(Acquire);
        while (current & 1 != 0) != enabled {
            match self.revision.compare_exchange_weak(current, current.wrapping_add(1), AcqRel, Acquire) {
                Ok(_) => {
                    if let Some(t) = self.worker.get() {
                        t.unpark();
                    }
                    return;
                }
                Err(revision) => current = revision,
            }
        }
    }
}

/// Only `write` is lent to the capture thread. Construction and destruction stay on the control thread.
///
/// # Safety
/// `write` must be safe to execute on another thread with exclusive access to the value. It must leave ownership
/// of thread-affine resources intact, including on unwinding.
unsafe trait PacketWriter {
    /// False when this attachment has ended and needs to be released.
    fn write(&mut self, samples: &[Frame], meter: &Meter) -> bool;
}

// A single publisher owns the Box and withdraws it before dropping it. A null slot during publication means a packet
// is using it. Neither moving a LiveFeed to the capture thread for Drop nor calling its control operations there is safe.
struct Published<'a, T> {
    slot: &'a AtomicPtr<(u64, T)>,
    meter: &'a Meter,
    _owner: PhantomData<T>,
}

impl<T> Drop for Published<'_, T> {
    fn drop(&mut self) {
        self.meter.offline.store(true, Release);
        loop {
            let ptr = self.slot.swap(ptr::null_mut(), Acquire);
            if !ptr.is_null() {
                self.meter.period.store(0, Relaxed);
                self.meter.update(&[]);
                // SAFETY: the only publisher takes back the allocation from `manage`. A packet that took it first
                // returns it with Release when its exclusive borrow ends. No new borrow can take a withdrawn pointer.
                drop(unsafe { Box::from_raw(ptr) });
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn write<T: PacketWriter>(slot: &AtomicPtr<(u64, T)>, control: &Control, meter: &Meter, samples: &[Frame]) {
    let revision = control.revision.load(Acquire);
    if revision & 1 == 0 || meter.offline.load(Acquire) {
        return;
    }
    let ptr = slot.swap(ptr::null_mut(), Acquire);
    if ptr.is_null() {
        return;
    }
    // Always release the borrow, including when a caller unwinds. The control thread may already be waiting to detach.
    struct Return<'a, T>(&'a AtomicPtr<T>, *mut T);
    impl<T> Drop for Return<'_, T> {
        fn drop(&mut self) {
            self.0.store(self.1, Release);
        }
    }
    let _borrow = Return(slot, ptr);
    // SAFETY: the acquiring swap uniquely checked out the live allocation. Its publisher cannot reclaim it until Return
    // runs. PacketWriter permits this call on the capture thread and the reference does not escape it.
    let (attached_at, writer) = unsafe { &mut *ptr };
    if *attached_at == revision && !writer.write(samples, meter) {
        meter.offline.store(true, Release);
    }
}

fn manage<T: PacketWriter>(
    control: &Control, meter: &Meter, slot: &AtomicPtr<(u64, T)>, ready: mpsc::SyncSender<Result<(), String>>,
    mut attach: impl FnMut(CableId) -> std::io::Result<T>,
) {
    let _ = control.worker.set(thread::current());
    let mut startup = Some(ready);
    while !control.quit.load(Acquire) {
        let revision = control.revision.load(Acquire);
        if revision & 1 == 0 {
            if let Some(ready) = startup.take() {
                meter.offline.store(true, Release);
                let _ = ready.send(Ok(()));
            }
            thread::park();
            continue;
        }
        match attach(control.id) {
            Ok(writer) => {
                if control.revision.load(Acquire) != revision || control.quit.load(Acquire) {
                    continue;
                }
                slot.store(Box::into_raw(Box::new((revision, writer))), Release);
                let published = Published { slot, meter, _owner: PhantomData };
                meter.offline.store(false, Release);
                if let Some(ready) = startup.take() {
                    let _ = ready.send(Ok(()));
                }
                while !meter.offline.load(Acquire) && control.revision.load(Acquire) == revision && !control.quit.load(Acquire) {
                    thread::park_timeout(WATCH);
                }
                drop(published);
            }
            Err(error) => {
                if let Some(ready) = startup.take() {
                    let _ = ready.send(Err(format!("Could not attach to cable {}: {error}", control.id)));
                    return;
                }
            }
        }
        if control.revision.load(Acquire) == revision && !control.quit.load(Acquire) {
            thread::park_timeout(RETRY);
        }
    }
}

pub(super) struct Sink {
    index: usize,
    state: Arc<State>,
    slot: Arc<AtomicPtr<(u64, CableWriter)>>,
    worker: Option<JoinHandle<()>>,
}

impl Sink {
    pub fn open(index: usize, state: &Arc<State>) -> Result<Self, String> {
        let slot = Arc::new(AtomicPtr::default());
        let worker_slot = Arc::clone(&slot);
        let worker_state = Arc::clone(state);
        let (ready, wait) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("LLCable attachment".into())
            .spawn(move || manage(&worker_state.cables[index], &worker_state.outputs[index], &worker_slot, ready, CableWriter::attach))
            .map_err(|e| e.to_string())?;
        let sink = Self { index, state: Arc::clone(state), slot, worker: Some(worker) };
        wait.recv().map_err(|_| "The cable attachment worker ended during startup.".to_owned())??;
        Ok(sink)
    }

    pub fn write(&self, samples: &[Frame], meter: &Meter) {
        write(&self.slot, &self.state.cables[self.index], meter, samples);
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.state.cables[self.index].quit.store(true, Release);
        if let Some(t) = self.worker.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

struct CableWriter {
    feed: LiveFeed,
    active: bool,
    drift: Drift,
    previous_reader: usize,
    line: ReaderSample,
}

impl CableWriter {
    fn attach(id: CableId) -> std::io::Result<Self> {
        let feed = attach_live_to(id, LIVE_CAPACITY)?;
        Ok(Self { feed, active: false, drift: Drift::default(), previous_reader: 0, line: ReaderSample::default() })
    }
}

// SAFETY: ring() borrows the mapping and finished() only checks the ATTACH event or broker process. The sole writer
// accesses the mapping's Cell reservation exclusively. The issuing thread retains the device, request, mapping and broker
// lease until that borrow ends, then performs all cancellation, unmapping and STOP operations itself.
unsafe impl PacketWriter for CableWriter {
    fn write(&mut self, samples: &[Frame], meter: &Meter) -> bool {
        let ring = self.feed.ring();
        let h = ring.header();
        let state = h.state.load(Acquire);
        if state & STATE_ATTACHED == 0 || state & STATE_FORMAT_MISMATCH != 0 || self.feed.finished() {
            return false;
        }
        meter.update(samples);
        let line = h.reader_sample().unwrap_or(self.line);
        let moved = line.moved != self.line.moved;
        let missing = line.underrun_frames.saturating_sub(self.line.underrun_frames);
        self.line = line;
        let measured = line.packet as usize;
        let reader = if line.state & STATE_READING != 0 {
            measured
        } else if line.state & STATE_LIVE_HANDOFF != 0 {
            self.previous_reader
        } else {
            0
        };
        self.previous_reader = reader;
        meter.period.store(reader as u32, Relaxed);
        if !self.active || moved {
            meter.startup.fetch_add(missing, Relaxed);
        } else {
            meter.underrun.fetch_add(missing, Relaxed);
        }
        if reader == 0 {
            self.active = false;
            return true;
        }
        if reader > ring.capacity() as usize {
            return false;
        }
        let mut writer = Writer::new(ring);
        let n = samples.len();
        let period = n.max(reader);
        if !self.active || moved {
            let prime = (period as u64).saturating_sub(writer.fill());
            writer.produce(prime, |_| [0.; 2]);
            self.active = true;
            self.drift = Drift::default();
        }
        let fill = writer.fill() as usize;
        let adjustment = self.drift.correction(fill, period / 3, 2 * period, n);
        let send = n.saturating_add_signed(-(adjustment as isize)).max(1);
        let start = writer.write_frame();
        let wrote = if send == n || n <= 1 || send <= 1 {
            writer.produce(send as u64, |pos| samples[((pos - start) as usize).min(n - 1)])
        } else {
            writer.produce(send as u64, |pos| {
                let offset = (pos - start) as usize * (n - 1);
                let lo = offset / (send - 1);
                interpolate(samples[lo], samples[(lo + 1).min(n - 1)], (offset % (send - 1)) as f32 / (send - 1) as f32)
            })
        };
        meter.lost.fetch_add(send as u64 - wrote, Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::thread::ThreadId;
    use std::time::Instant;

    use super::*;

    struct FakeFeed<'a> {
        owner: ThreadId,
        dropped: &'a AtomicUsize,
        present: &'a AtomicBool,
    }

    // SAFETY: cross-thread packet access touches only the meter and an atomic device-presence flag.
    unsafe impl PacketWriter for FakeFeed<'_> {
        fn write(&mut self, samples: &[Frame], meter: &Meter) -> bool {
            if !self.present.load(Acquire) {
                return false;
            }
            meter.update(samples);
            true
        }
    }

    impl Drop for FakeFeed<'_> {
        fn drop(&mut self) {
            assert_eq!(self.owner, thread::current().id());
            self.dropped.fetch_add(1, Release);
        }
    }

    struct Stop<'a>(&'a Control);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.quit.store(true, Release);
            self.0.worker.get().unwrap().unpark();
        }
    }

    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "cable worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn initially_paused_cable_attaches_on_resume_recovers_after_removal_and_releases_on_pause() {
        let control = Control::new(CableId(5), false);
        let (slot, meter) = (AtomicPtr::default(), Meter::default());
        let (present, failed, dropped) = (AtomicBool::new(true), AtomicBool::new(false), AtomicUsize::new(0));
        let attempts = AtomicUsize::new(0);
        let (ready, started) = mpsc::sync_channel(1);
        thread::scope(|scope| {
            scope.spawn(|| {
                manage(&control, &meter, &slot, ready, |id| {
                    assert_eq!(id, CableId(5));
                    attempts.fetch_add(1, Release);
                    if !present.load(Acquire) {
                        failed.store(true, Release);
                        return Err(std::io::ErrorKind::NotFound.into());
                    }
                    Ok(FakeFeed { owner: thread::current().id(), dropped: &dropped, present: &present })
                });
            });
            started.recv().unwrap().unwrap();
            let _stop = Stop(&control);
            let packet = || write(&slot, &control, &meter, &[[0.25, -0.75]]);
            assert!(meter.offline.load(Acquire));
            assert_eq!(attempts.load(Acquire), 0);
            control.set_enabled(true);
            until(|| !meter.offline.load(Acquire));
            packet();
            assert_eq!(meter.level().peak, [0.25, 0.75]);

            present.store(false, Release);
            packet();
            assert!(meter.offline.load(Acquire));
            until(|| failed.load(Acquire));
            assert_eq!(dropped.load(Acquire), 1);
            present.store(true, Release);
            until(|| !meter.offline.load(Acquire));
            packet();
            assert_eq!(meter.level().frames, 2);

            control.set_enabled(false);
            until(|| dropped.load(Acquire) == 2);
            assert!(meter.offline.load(Acquire));
            packet();
            assert_eq!(meter.level().peak, [0., 0.]);
            assert_eq!(meter.level().frames, 2);
            control.set_enabled(true);
            until(|| !meter.offline.load(Acquire));
            packet();
            assert_eq!(meter.level().frames, 3);
        });
        assert_eq!(dropped.load(Acquire), 3);
        assert!(slot.load(Acquire).is_null());
    }

    #[test]
    fn withdrawing_a_busy_writer_waits_for_its_packet_before_dropping_on_the_owner() {
        struct Busy<'a> {
            feed: FakeFeed<'a>,
            entered: &'a AtomicBool,
            finish: &'a AtomicBool,
        }
        // SAFETY: the test holds the borrower on two atomic flags before accessing the exclusively borrowed fake feed.
        unsafe impl PacketWriter for Busy<'_> {
            fn write(&mut self, samples: &[Frame], meter: &Meter) -> bool {
                self.entered.store(true, Release);
                until(|| self.finish.load(Acquire));
                self.feed.write(samples, meter)
            }
        }
        let control = Control::new(CableId(8), true);
        let (slot, meter) = (AtomicPtr::default(), Meter::default());
        let (entered, finish) = (AtomicBool::new(false), AtomicBool::new(false));
        let (present, dropped) = (AtomicBool::new(true), AtomicUsize::new(0));
        let (ready, started) = mpsc::sync_channel(1);
        thread::scope(|scope| {
            scope.spawn(|| {
                manage(&control, &meter, &slot, ready, |_| {
                    Ok(Busy {
                        feed: FakeFeed { owner: thread::current().id(), dropped: &dropped, present: &present },
                        entered: &entered,
                        finish: &finish,
                    })
                });
            });
            started.recv().unwrap().unwrap();
            let _stop = Stop(&control);
            let packet = scope.spawn(|| write(&slot, &control, &meter, &[[1., -1.]]));
            until(|| entered.load(Acquire));
            control.set_enabled(false);
            until(|| meter.offline.load(Acquire));
            assert_eq!(dropped.load(Acquire), 0);
            finish.store(true, Release);
            packet.join().unwrap();
            until(|| dropped.load(Acquire) == 1);
            assert_eq!(meter.level().frames, 1);
            assert_eq!(meter.level().peak, [0., 0.]);
        });
    }
}
