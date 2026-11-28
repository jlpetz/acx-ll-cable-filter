use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use llcable_wasapi::sched::{Com, Mmcss, MmcssPriority};
use llcable_wasapi::{Capture, Playback};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::signal::{MAX_SECONDS, Marks, RATE, frame};

pub struct Worker<T> {
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<T, String>>>,
}

type Ready = mpsc::SyncSender<Result<(), String>>;

impl<T: Send + 'static> Worker<T> {
    fn start(task: impl FnOnce(&AtomicBool, &Ready) -> Result<T, String> + Send + 'static) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        let quit = Arc::<AtomicBool>::default();
        let flag = quit.clone();
        let thread = thread::spawn(move || {
            let result = task(&flag, &tx);
            if let Err(e) = &result {
                let _ = tx.try_send(Err(e.clone()));
            }
            result
        });
        let worker = Self { quit, thread: Some(thread) };
        rx.recv().map_err(|_| "Probe worker ended before opening its endpoint.")??;
        Ok(worker)
    }

    pub fn finish(mut self, stop: bool) -> Result<T, String> {
        self.quit.store(stop, Relaxed);
        self.thread.take().unwrap().join().map_err(|_| "Probe worker panicked.")?
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.quit.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wait(handle: HANDLE) -> Result<bool, String> {
    // SAFETY: the owning WASAPI stream remains open throughout the wait.
    match unsafe { WaitForSingleObject(handle, 500) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(std::io::Error::last_os_error().to_string()),
    }
}

pub struct Recording {
    pub frames: Vec<[f32; 2]>,
    // Exclusive readers deliver a whole packet at once. The timestamp is taken immediately after Capture::read.
    pub packets: Vec<(usize, u64)>,
    pub period: usize,
}

impl Recording {
    /// Offline copy of complete packets received within a control epoch, with offsets relative to the copy.
    pub fn during(&self, from: u64, until: u64) -> Self {
        let first = self.packets.partition_point(|&(_, t)| t < from);
        let end = self.packets.partition_point(|&(_, t)| t < until);
        let frame_offset = |i: usize| i.checked_sub(1).map_or(0, |p| self.packets[p].0);
        let start_frame = frame_offset(first);
        Self {
            frames: self.frames[start_frame..frame_offset(end)].to_vec(),
            packets: self.packets[first..end].iter().map(|&(pos, t)| (pos - start_frame, t)).collect(),
            period: self.period,
        }
    }
}

pub fn record(id: String, marks: Marks, seconds: usize) -> Result<Worker<Recording>, String> {
    Worker::start(move |stop, ready| {
        let _com = Com::mta().map_err(|e| format!("COM: {e:#x}"))?;
        let mut device = Capture::open(&id)?;
        let info = device.info();
        let mut buffer = vec![[0.; 2]; device.capacity()];
        let limit = (seconds + 5) * RATE;
        let mut recording = Recording {
            frames: Vec::with_capacity(limit),
            packets: Vec::with_capacity(limit.div_ceil(info.period.max(1)) * 2),
            period: info.period,
        };
        device.start().map_err(|e| e.to_string())?;
        let _ = ready.send(Ok(()));
        let mmcss = Mmcss::enter(MmcssPriority::Normal);
        while !stop.load(Relaxed) {
            if !wait(device.event())? {
                return Err("A verification reader stopped receiving events.".into());
            }
            while let Some(packet) = device.read(&mut buffer).map_err(|e| e.to_string())? {
                let now = marks.now();
                if recording.frames.len() + packet.frames > limit || recording.packets.len() == recording.packets.capacity() {
                    return Err("Verification recording exceeded its preallocated duration.".into());
                }
                recording.frames.extend_from_slice(&buffer[..packet.frames]);
                recording.packets.push((recording.frames.len(), now));
                if info.exclusive {
                    break;
                }
            }
        }
        drop(mmcss);
        device.stop();
        Ok(recording)
    })
}

pub struct Sent {
    pub frames: usize,
    pub period: usize,
    pub fill: usize,
}

pub fn play(id: String, marks: Marks, seconds: usize) -> Result<Worker<Sent>, String> {
    Worker::start(move |stop, ready| {
        let _com = Com::mta().map_err(|e| format!("COM: {e:#x}"))?;
        let mut device = Playback::open(&id)?;
        let period = device.period();
        let fill = device.capacity().min(2 * period);
        let mut buffer = vec![[0.; 2]; device.capacity()];
        device.write(&buffer[..fill]).map_err(|e| e.to_string())?;
        device.start().map_err(|e| e.to_string())?;
        let _ = ready.send(Ok(()));
        let mmcss = Mmcss::enter(MmcssPriority::Normal);
        let mut written = fill;
        let mut signal_end = RATE + seconds.min(MAX_SECONDS) * RATE;
        // One second of silence to stabilize the client and its rings, then a finite coded clip and 0.4 s of drain.
        while written < signal_end + 2 * RATE / 5 {
            if !wait(device.event())? {
                return Err("The virtual source player stopped receiving events.".into());
            }
            if stop.load(Relaxed) {
                signal_end = signal_end.min(written.max(RATE));
            }
            let n = device.needed().map_err(|e| e.to_string())?;
            for (i, f) in buffer[..n].iter_mut().enumerate() {
                let pos = written + i;
                *f = if (RATE..signal_end).contains(&pos) { frame(pos - RATE) } else { [0.; 2] };
            }
            device.write(&buffer[..n]).map_err(|e| e.to_string())?;
            marks.submit(written.clamp(RATE, signal_end) - RATE, (written + n).clamp(RATE, signal_end) - RATE);
            written += n;
        }
        drop(mmcss);
        device.stop();
        Ok(Sent { frames: signal_end - RATE, period, fill })
    })
}
