//! Native 48 kHz stereo capture and routing to direct cable feeds or WASAPI playback.
//! A session captures each selected source once. The first source clocks the graph. Other input clocks have bounded,
//! independently compensated buffers. All direct cable sinks receive their bus in the same wake, on the capture thread.
//! Each external playback device consumes from its own bounded lock-free queue on a separate MMCSS thread.

mod clock;
mod edits;
mod inputs;
mod outputs;
mod playback;
mod sinks;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clock::{Follower, Frame};
use llcable_routing::{Bus, Router};
pub use llcable_sdk::CableId;
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
    stop: AtomicBool,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

pub struct Session {
    thread: Option<JoinHandle<()>>,
    state: Arc<State>,
    inputs: Vec<String>,
    destinations: RwLock<Vec<outputs::Destination>>,
    format: edits::StreamFormat,
    edits: edits::Control,
}

impl Session {
    /// Opens on a worker thread and returns once all sources and cable attachments are ready.
    /// A regular Windows user can first call `llcable_sdk::authorize` in response to an explicit action.
    pub fn start(config: RouteConfig) -> Result<Self, String> {
        Self::start_with_initializer(config, || {})
    }

    /// Run an optional effect library's thread-local initialisation once before starting capture.
    pub fn start_with_initializer(config: RouteConfig, init: fn()) -> Result<Self, String> {
        Self::start_with_paused(config, init, &[])
    }

    /// Start with the selected direct destinations paused and unattached. IDs absent from this patch are ignored.
    /// Resume any selected cable with [`Self::set_cable_enabled`]; all initially enabled cables must attach during startup.
    pub fn start_with_paused(config: RouteConfig, init: fn(), paused: &[CableId]) -> Result<Self, String> {
        if !(1..=MAX_SOURCES).contains(&config.inputs.len()) {
            return Err(format!("Select between 1 and {MAX_SOURCES} recording sources."));
        }
        let state = Arc::new(State {
            inputs: (0..config.inputs.len()).map(|_| Meter::default()).collect(),
            stop: false.into(),
            running: false.into(),
            error: Mutex::default(),
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let (edits, audio) = edits::channel();
        let inputs = config.inputs.clone();
        let paused = paused.to_vec();
        let worker = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("LLCable routing".into())
            .spawn(move || {
                let failure = run(config, &worker, &tx, init, audio, &paused).err();
                if let Some(error) = failure {
                    *worker.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                    let _ = tx.try_send(Err(error));
                }
                worker.running.store(false, Relaxed);
            })
            .map_err(|e| e.to_string())?;
        match rx.recv().unwrap_or_else(|_| Err("The routing worker ended before startup.".into())) {
            Ok((format, destinations)) => {
                Ok(Self { thread: Some(thread), state, inputs, destinations: RwLock::new(destinations), format, edits })
            }
            Err(error) => {
                state.stop.store(true, Relaxed);
                let _ = thread.join();
                Err(error)
            }
        }
    }

    /// A live edit keeps the clock source and capture endpoints. Destination changes keep surviving attachments,
    /// playback workers and counters in place, including when their output indexes move.
    pub fn can_reconfigure(&self, config: &RouteConfig) -> bool {
        self.state.running.load(Relaxed) && self.inputs == config.inputs
    }

    /// Replace processing and routing at the next packet boundary. Call on a control thread.
    pub fn reconfigure(&self, config: RouteConfig) -> Result<(), String> {
        self.reconfigure_retaining(config, Vec::new())
    }

    /// For each `(old, new)` pair, keep that running rack (including its meters and DSP history), using the new bus's sends.
    /// The caller identifies unchanged racks; the host checks that their sidechain taps remain compatible.
    pub fn reconfigure_retaining(&self, config: RouteConfig, retained: Vec<(usize, usize)>) -> Result<(), String> {
        if !self.can_reconfigure(&config) {
            return Err("A live edit needs a running session with the same recording sources in the same order.".into());
        }
        self.edits.apply(&self.format, &self.destinations, config, retained)
    }

    /// Pauses or resumes one direct destination. Its attachment worker performs the detach or reopen asynchronously.
    pub fn set_cable_enabled(&self, id: CableId, enabled: bool) -> Result<(), String> {
        if !self.state.running.load(Relaxed) {
            return Err("The routing session has stopped.".into());
        }
        let destinations = self.destinations.read().unwrap_or_else(|e| e.into_inner());
        let control = destinations.iter().find_map(|c| c.cable(id)).ok_or_else(|| format!("Cable {id} is not a direct destination."))?;
        control.set_enabled(enabled);
        Ok(())
    }

    /// The user's requested state, including while an enabled destination waits for its device to return.
    pub fn cable_enabled(&self, id: CableId) -> Option<bool> {
        self.destinations.read().unwrap_or_else(|e| e.into_inner()).iter().find_map(|c| c.cable(id)).map(sinks::Control::enabled)
    }

    pub fn snapshot(&self) -> RouteStatus {
        let destinations = self.destinations.read().unwrap_or_else(|e| e.into_inner());
        RouteStatus {
            running: self.state.running.load(Relaxed) && self.thread.as_ref().is_some_and(|t| !t.is_finished()),
            inputs: self.state.inputs.iter().map(Meter::level).collect(),
            outputs: destinations.iter().map(|d| d.meter.level()).collect(),
            outputs_offline: destinations.iter().map(|d| d.meter.offline.load(Relaxed)).collect(),
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

type Started = Result<(edits::StreamFormat, Vec<outputs::Destination>), String>;

fn run(
    config: RouteConfig, state: &State, started: &mpsc::SyncSender<Started>, init: fn(), mut edits: edits::Audio, paused: &[CableId],
) -> Result<(), String> {
    let _com = Com::mta().map_err(|h| format!("Could not initialize the Windows audio thread ({h:#x})."))?;
    let (ids, origins) = inputs::resolve(&config.inputs, false)?;
    let destinations = outputs::Plan::prepare(&config, &[], &origins, paused)?;
    let mut sources = Vec::with_capacity(ids.len());
    for (id, meter) in ids.iter().zip(&state.inputs) {
        let capture = Capture::open(id).map_err(|e| format!("Could not open recording endpoint {id}: {e}"))?;
        meter.period.store(capture.info().period as u32, Relaxed);
        let buffer = vec![[0.; 2]; capture.capacity()].into_boxed_slice();
        sources.push(Source { capture, buffer, expected: None });
    }
    let max_frames = sources[0].capture.capacity();
    let mut router = Router::new(48_000, max_frames, sources.len(), config.buses).map_err(|e| e.to_string())?;
    init();
    router.prepare_thread();
    let format = edits::StreamFormat { max_frames, period: sources[0].capture.info().period, origins };
    let mut outputs = destinations.open(&format)?;
    let outputs::Plan { destinations, .. } = destinations;
    let n_inputs = sources.len();
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
    let _ = started.send(Ok((format, destinations)));
    let mut last_master = Instant::now();
    while !state.stop.load(Relaxed) {
        // SAFETY: every handle belongs to a live, started Capture on this thread; count is checked at startup.
        let wake = unsafe { WaitForMultipleObjects(&handles, false, 50) };
        if wake == WAIT_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        edits.apply_pending(&mut router, &mut outputs);
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
            for output in &mut outputs {
                output.write(&router);
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
