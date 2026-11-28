//! Native 48 kHz stereo capture and routing to direct cable feeds or WASAPI playback.
//! A session captures each selected source once. The first source clocks the graph. Other input clocks have bounded,
//! independently compensated buffers. All direct cable sinks receive their bus in the same wake, on the capture thread.
//! Each external playback device consumes from its own bounded lock-free queue on a separate MMCSS thread.

mod clock;
mod inputs;
mod playback;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clock::{Drift, Follower, Frame, interpolate};
use llcable_routing::{Bus, Router};
pub use llcable_sdk::CableId;
use llcable_sdk::driver::{ReaderSample, STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_LIVE_HANDOFF, STATE_READING};
use llcable_sdk::{LIVE_CAPACITY, LiveFeed, Writer, attach_live_to};
use llcable_wasapi::sched::{Com, Mmcss, MmcssPriority};
use llcable_wasapi::{Capture, Packet};
use windows::Win32::Foundation::{HANDLE, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};

const MAX_SOURCES: usize = 64; // WaitForMultipleObjects' maximum.

pub struct RouteConfig {
    /// One WASAPI recording endpoint ID or persisted `acx-ll-cable:<id>/<role>` per captured source.
    /// Prefix a render endpoint with `loopback:` to capture the playback mix, including a speaker reference for AEC.
    pub inputs: Vec<String>,
    /// One cable attachment per destination. Cable identities survive removal and reinstallation.
    pub cables: Vec<CableId>,
    /// Input indexes refer to `inputs`. Bus indexes refer to earlier entries here.
    pub buses: Vec<Bus>,
    /// Which bus each cable receives, in `cables` order. Multiple cables may subscribe to the same bus.
    pub output_buses: Vec<usize>,
    /// External WASAPI playback endpoint and its graph bus. These sinks follow the direct cables in [`RouteStatus::outputs`].
    pub playback: Vec<(String, usize)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Level {
    /// Linear amplitude. Both channels retain their individual readings.
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    pub dropped_frames: u64,
    /// The actual period of the recording endpoint or attached cable reader, zero until a reader arrives.
    pub period_frames: u32,
    /// Frames the driver supplied as silence after the producer established its starting fill.
    pub underrun_frames: u64,
    /// Silence from starting a fresh reader on an empty live ring, including a restart after a discard.
    pub startup_silent_frames: u64,
    pub frames: u64,
}

#[derive(Clone, Debug, Default)]
pub struct RouteStatus {
    pub running: bool,
    pub inputs: Vec<Level>,
    pub outputs: Vec<Level>,
    pub outputs_offline: Vec<bool>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Meter {
    peak: [AtomicU32; 2],
    rms: [AtomicU32; 2],
    lost: AtomicU64,
    period: AtomicU32,
    underrun: AtomicU64,
    startup: AtomicU64,
    frames: AtomicU64,
    offline: AtomicBool,
}

impl Meter {
    fn update(&self, packet: &[Frame]) {
        let mut peak = [0f32; 2];
        let mut square = [0f64; 2];
        for frame in packet {
            for c in 0..2 {
                let x = frame[c];
                peak[c] = peak[c].max(x.abs());
                square[c] += f64::from(x) * f64::from(x);
            }
        }
        for c in 0..2 {
            self.peak[c].store(peak[c].to_bits(), Relaxed);
            let rms = (square[c] / packet.len().max(1) as f64).sqrt() as f32;
            self.rms[c].store(rms.to_bits(), Relaxed);
        }
        self.frames.fetch_add(packet.len() as u64, Relaxed);
    }

    fn level(&self) -> Level {
        let read = |x: &AtomicU32| f32::from_bits(x.load(Relaxed));
        Level {
            peak: self.peak.each_ref().map(read),
            rms: self.rms.each_ref().map(read),
            dropped_frames: self.lost.load(Relaxed),
            period_frames: self.period.load(Relaxed),
            underrun_frames: self.underrun.load(Relaxed),
            startup_silent_frames: self.startup.load(Relaxed),
            frames: self.frames.load(Relaxed),
        }
    }
}

struct State {
    inputs: Vec<Meter>,
    outputs: Vec<Meter>,
    stop: AtomicBool,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

pub struct Session {
    thread: Option<JoinHandle<()>>,
    state: Arc<State>,
}

impl Session {
    /// Opens on a worker thread and returns once all sources and cable attachments are ready.
    /// A regular Windows user can first call `llcable_sdk::authorize` in response to an explicit action.
    pub fn start(config: RouteConfig) -> Result<Self, String> {
        if !(1..=MAX_SOURCES).contains(&config.inputs.len())
            || (config.cables.is_empty() && config.playback.is_empty())
            || config.cables.len() != config.output_buses.len()
        {
            return Err("Select at least one recording source and one destination. Each cable needs one output bus.".into());
        }
        if config.cables.iter().enumerate().any(|(i, c)| config.cables[..i].contains(c)) {
            return Err("Each destination cable can receive one bus. Combine its sources into that bus.".into());
        }
        let state = Arc::new(State {
            inputs: (0..config.inputs.len()).map(|_| Meter::default()).collect(),
            outputs: (0..config.cables.len() + config.playback.len()).map(|_| Meter::default()).collect(),
            stop: false.into(),
            running: false.into(),
            error: Mutex::default(),
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("LLCable routing".into())
            .spawn(move || {
                let failure = run(config, &worker, &tx).err();
                if let Some(error) = failure {
                    *worker.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                    let _ = tx.try_send(Err(error));
                }
                worker.running.store(false, Relaxed);
            })
            .map_err(|e| e.to_string())?;
        let mut session = Self { thread: Some(thread), state };
        if let Err(error) = rx.recv().unwrap_or_else(|_| Err("The routing worker ended before startup.".into())) {
            session.stop();
            return Err(error);
        }
        Ok(session)
    }

    pub fn snapshot(&self) -> RouteStatus {
        RouteStatus {
            running: self.state.running.load(Relaxed) && self.thread.as_ref().is_some_and(|t| !t.is_finished()),
            inputs: self.state.inputs.iter().map(Meter::level).collect(),
            outputs: self.state.outputs.iter().map(Meter::level).collect(),
            outputs_offline: self.state.outputs.iter().map(|m| m.offline.load(Relaxed)).collect(),
            error: self.state.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    pub fn stop(&mut self) {
        self.state.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Source {
    capture: Capture,
    buffer: Box<[Frame]>,
    expected: Option<u64>,
}

fn event_ready(handle: HANDLE, index: usize, wake: WAIT_EVENT) -> bool {
    // The multi-object wait already consumed the winning auto-reset event. Other exclusive streams may also have
    // fired; consume their individual event before GetBuffer, which requires a complete packet in exclusive mode.
    wake.0 == WAIT_OBJECT_0.0 + index as u32 || unsafe { WaitForSingleObject(handle, 0) == WAIT_OBJECT_0 }
}

impl Source {
    fn read(&mut self, meter: &Meter) -> Result<Option<Packet>, String> {
        let packet = self.capture.read(&mut self.buffer).map_err(|e| e.to_string())?;
        if let Some(p) = packet {
            if let (Some(position), Some(expected)) = (p.position, self.expected) {
                meter.lost.fetch_add(position.saturating_sub(expected), Relaxed);
            }
            self.expected = p.position.map(|position| position.saturating_add(p.frames as u64));
            meter.update(&self.buffer[..p.frames]);
        }
        Ok(packet)
    }
}

struct Sink {
    feed: LiveFeed,
    bus: usize,
    active: bool,
    drift: Drift,
    previous_reader: usize,
    line: ReaderSample,
}

impl Sink {
    fn write(&mut self, samples: &[Frame], meter: &Meter) {
        if meter.offline.load(Relaxed) {
            return;
        }
        let ring = self.feed.ring();
        let h = ring.header();
        let state = h.state.load(std::sync::atomic::Ordering::Acquire);
        if state & STATE_ATTACHED == 0 || state & STATE_FORMAT_MISMATCH != 0 || self.feed.finished() {
            meter.offline.store(true, Relaxed);
            meter.update(&[]);
            return;
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
            return;
        }
        if reader > ring.capacity() as usize {
            meter.offline.store(true, Relaxed);
            return;
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
    }
}

fn run(config: RouteConfig, state: &Arc<State>, started: &mpsc::SyncSender<Result<(), String>>) -> Result<(), String> {
    let _com = Com::mta().map_err(|h| format!("Could not initialize the Windows audio thread ({h:#x})."))?;
    let (ids, origins) = inputs::resolve(&config.inputs, false)?;
    let (play_ids, play_origins) = if config.playback.is_empty() {
        (vec![], vec![])
    } else {
        inputs::resolve(&config.playback.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(), true)?
    };
    let mut target_ids: Vec<_> = config.cables.iter().map(ToString::to_string).collect();
    target_ids.extend(play_origins.into_iter().flatten());
    if target_ids.iter().enumerate().any(|(i, id)| target_ids[..i].contains(id)) {
        return Err("This cable is already selected as a destination. Choose one output route per cable.".into());
    }
    let indices: Vec<_> = config.output_buses.iter().copied().chain(config.playback.iter().map(|(_, i)| *i)).collect();
    inputs::check_loops(&origins, &target_ids, &indices, &config.buses)?;
    let mut sources = Vec::with_capacity(ids.len());
    for (id, meter) in ids.iter().zip(&state.inputs) {
        let capture = Capture::open(id).map_err(|e| format!("Could not open recording endpoint {id}: {e}"))?;
        meter.period.store(capture.info().period as u32, Relaxed);
        let buffer = vec![[0.; 2]; capture.capacity()].into_boxed_slice();
        sources.push(Source { capture, buffer, expected: None });
    }
    let max_frames = sources[0].capture.capacity();
    let mut router = Router::new(48_000, max_frames, sources.len(), config.buses).map_err(|e| e.to_string())?;
    router.prepare_thread();
    let mut sinks = Vec::with_capacity(config.cables.len());
    for (id, bus) in config.cables.into_iter().zip(config.output_buses) {
        let feed = attach_live_to(id, LIVE_CAPACITY).map_err(|e| format!("Could not attach to cable {id}: {e}"))?;
        sinks.push(Sink { feed, bus, active: false, drift: Drift::default(), previous_reader: 0, line: ReaderSample::default() });
    }
    let n_inputs = sources.len();
    let mut external = Vec::new();
    for (i, (id, (_, bus))) in play_ids.into_iter().zip(config.playback).enumerate() {
        external.push(playback::External::open(id, bus, Arc::clone(state), sinks.len() + i, max_frames, sources[0].capture.info().period)?);
    }
    let handles: Vec<_> = sources.iter().map(|s| s.capture.event()).collect();
    let (master, rest) = sources.split_first_mut().unwrap();
    let mut followers: Vec<_> = rest
        .iter()
        .map(|s| {
            let queue = Follower::new(max_frames.max(s.capture.capacity()), s.capture.info().period.max(master.capture.info().period));
            (queue, vec![[0.; 2]; max_frames].into_boxed_slice())
        })
        .collect();
    let mmcss = Mmcss::enter(MmcssPriority::Critical);
    // Start secondary endpoints first. Their first buffers can arrive before the master wakes.
    for s in rest.iter().chain(std::iter::once(&*master)) {
        s.capture.start().map_err(|e| e.to_string())?;
    }
    state.running.store(true, Relaxed);
    let _ = started.send(Ok(()));
    let mut last_master = Instant::now();
    while !state.stop.load(Relaxed) {
        // SAFETY: every handle belongs to a live, started Capture on this thread; count is checked at startup.
        let wake = unsafe { WaitForMultipleObjects(&handles, false, 50) };
        if wake == WAIT_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        for (i, ((source, (queue, _)), meter)) in rest.iter_mut().zip(&mut followers).zip(&state.inputs[1..]).enumerate() {
            if source.capture.info().exclusive && !event_ready(handles[i + 1], i + 1, wake) {
                continue;
            }
            while let Some(p) = source.read(meter)? {
                let before = queue.lost;
                queue.push(&source.buffer[..p.frames]);
                meter.lost.fetch_add(queue.lost - before, Relaxed);
                if source.capture.info().exclusive {
                    break;
                }
            }
        }
        if last_master.elapsed() >= Duration::from_secs(2) {
            return Err("The clock source stopped delivering audio.".into());
        }
        if master.capture.info().exclusive && !event_ready(handles[0], 0, wake) {
            continue;
        }
        while let Some(p) = master.read(&state.inputs[0])? {
            last_master = Instant::now();
            let n = p.frames;
            let mut views: [&[Frame]; MAX_SOURCES] = [&[]; MAX_SOURCES];
            views[0] = &master.buffer[..n];
            for (((queue, buffer), view), meter) in followers.iter_mut().zip(&mut views[1..n_inputs]).zip(&state.inputs[1..]) {
                let before = queue.lost;
                queue.render(&mut buffer[..n]);
                meter.lost.fetch_add(queue.lost - before, Relaxed);
                *view = &buffer[..n];
            }
            router.process_stereo(&views[..n_inputs]).map_err(|e| e.to_string())?;
            for (sink, meter) in sinks.iter_mut().zip(&state.outputs) {
                sink.write(router.output(sink.bus).unwrap(), meter);
            }
            for (target, meter) in external.iter_mut().zip(&state.outputs[sinks.len()..]) {
                target.write(router.output(target.bus).unwrap(), meter);
            }
            if master.capture.info().exclusive {
                break;
            }
        }
    }
    drop(mmcss);
    for source in &sources {
        source.capture.stop();
    }
    Ok(())
}
