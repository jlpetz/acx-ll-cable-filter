//! A playback device owns its clock and may stall or disappear. Its queue is the only boundary crossed by the graph.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use llcable_wasapi::Playback;
use llcable_wasapi::sched::{Com, Mmcss, MmcssPriority};
use rtrb::{Consumer, Producer, RingBuffer};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::System::Threading::WaitForSingleObject;

use super::Meter;
use super::clock::{Follower, Frame};

pub(super) struct External {
    tx: Producer<Frame>,
    quit: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl External {
    pub fn open(id: String, meter: Arc<Meter>, max_input: usize, input_period: usize) -> Result<Self, String> {
        let capacity = (8 * max_input).max(8192);
        let (tx, rx) = RingBuffer::new(capacity);
        let (ready, wait) = mpsc::sync_channel(1);
        let quit = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&quit);
        let worker = thread::Builder::new()
            .name("LLCable playback".into())
            .spawn(move || {
                let setup = || -> Result<_, String> {
                    let com = Com::mta().map_err(|h| format!("Could not initialize the playback thread ({h:#x})."))?;
                    let stream = Playback::open(&id)?;
                    Ok((com, stream))
                };
                let (_com, mut stream) = match setup() {
                    Ok(pair) => pair,
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                let result = play(&mut stream, rx, &done, &meter, max_input, input_period, &ready);
                if let Err(error) = result {
                    meter.offline.store(true, Relaxed);
                    meter.update(&[]);
                    // Ends the worker after its MMCSS guard has been dropped. Other routes continue.
                    let _ = ready.try_send(Err(error));
                }
            })
            .map_err(|e| e.to_string())?;
        let mut external = Self { tx, quit, worker: Some(worker) };
        if let Err(error) = wait.recv().unwrap_or_else(|_| Err("The playback worker ended during startup.".into())) {
            external.close();
            return Err(error);
        }
        Ok(external)
    }
    pub fn write(&mut self, packet: &[Frame], meter: &Meter) {
        if meter.offline.load(Relaxed) {
            return;
        }
        meter.update(packet);
        let (_, remaining) = self.tx.push_partial_slice(packet);
        meter.lost.fetch_add(remaining.len() as u64, Relaxed);
    }
    fn close(&mut self) {
        self.quit.store(true, Relaxed);
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

impl Drop for External {
    fn drop(&mut self) {
        self.close();
    }
}

fn play(
    stream: &mut Playback, mut queue: Consumer<Frame>, quit: &AtomicBool, meter: &Meter, max_input: usize, input_period: usize,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let cap = stream.capacity();
    let mut input = vec![[0.; 2]; max_input];
    let mut out = vec![[0.; 2]; cap];
    let mut clock = Follower::new(max_input.max(cap), input_period.max(stream.period()));
    meter.period.store(stream.period() as u32, Relaxed);
    let n = stream.needed().map_err(|e| e.to_string())?;
    stream.write(&out[..n]).map_err(|e| e.to_string())?;
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    stream.start().map_err(|e| e.to_string())?;
    let _ = ready.send(Ok(()));
    let mut last = Instant::now();
    while !quit.load(Relaxed) {
        // SAFETY: the event is owned by this stream and remains live throughout the wait.
        let wake = unsafe { WaitForSingleObject(stream.event(), 40) };
        if wake == WAIT_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // Drain only what was present at this wake, so a fast producer cannot hold the rendering thread in the drain loop.
        let mut available = queue.slots();
        while available > 0 {
            let n = available.min(input.len());
            let (got, _) = queue.pop_partial_slice(&mut input[..n]);
            if got.is_empty() {
                break;
            }
            let before = clock.lost;
            clock.push(got);
            meter.lost.fetch_add(clock.lost - before, Relaxed);
            available -= got.len();
        }
        let n = stream.needed().map_err(|e| e.to_string())?;
        if n > 0 {
            let before = clock.lost;
            clock.render(&mut out[..n]);
            meter.underrun.fetch_add(clock.lost - before, Relaxed);
            stream.write(&out[..n]).map_err(|e| e.to_string())?;
            last = Instant::now();
        } else if last.elapsed() > Duration::from_secs(2) {
            return Err("Playback endpoint stopped consuming samples.".into());
        }
    }
    drop(mmcss);
    stream.stop();
    Ok(())
}
