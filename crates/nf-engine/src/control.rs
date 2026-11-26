//! The control side (engine.md "control"): `start`/`stop`/`apply`/`status`, the control thread
//! with the device-change listener, and status messages worded for the user.
//!
//! Locks here are control-side only; the audio thread never touches them. `run` is held across a
//! (re)start, which waits up to a few seconds for the streams to open; everything the UI polls
//! (`status`, `apply`) takes only the short locks. Where two are held at once the order is
//! `desired` → `status` → `current`.
//!
//! Restarts are the control thread's: `apply` and device events only ask for one, and the control
//! thread decides when it's due, against the run as it is then (`restart_due`). Settings changes are
//! gathered into one restart, device events are debounced, and a fault arms a timed retry with
//! backoff (`Schedule`) that starts over only after a run has stayed up for [`HEALTHY`].
//!
//! Every run goes to the one audio thread ([`Worker`]), kept between runs; a run stuck in a driver
//! keeps it, and the next start gets a new one once the stuck run has ended.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{AcqRel, Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use nf_core::config::EngineConfig;
use nf_core::controls::ChainControls;
use nf_core::telemetry::{EngineState, Telemetry};
use wasapi::{DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, DeviceState, Direction, Role};

use crate::api::{BufferTargets, EngineError, EngineStatus, Remedy, StreamInfo};
use crate::audio::{AudioArgs, AudioShared, CMD_STOP, Opened, Worker};
use crate::devices::resolve;
use crate::errors::{
    Fault, StartFailure, StreamRole, describe_fault, describe_reference_lost, describe_start_failure, describe_warning, hr,
    no_output_message, remedy_for_fault, remedy_for_start, same_device_message,
};
use crate::exclusive::{PeriodFloor, PeriodFloors, next_floor};
use crate::flight::{self, Note};
use crate::latency_test::{TapPort, tap_channel};
use crate::recorder;
use crate::sched::Com;
use crate::stream::CaptureStream;

/// How long `start` waits for the streams to open.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `stop` waits for the audio thread (it checks its stop flag at least every 100 ms).
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// The control thread's health-check period.
const POLL: Duration = Duration::from_millis(200);
/// Device events arrive in bursts (one per role, plus state changes): restart once, after this.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Settings changes that need the streams rebuilt come in runs (a slider drag sends one per step):
/// they make one restart, this long after the latest...
const COALESCE: Duration = Duration::from_millis(200);
/// ...and at most this long after the first.
const COALESCE_MAX: Duration = Duration::from_secs(1);
/// The timed retry after a fault or a failed (re)start (wasapi/A): first after this, doubling...
const RETRY_FIRST: Duration = Duration::from_millis(500);
/// ...up to this. Well above START_TIMEOUT, so a device that keeps timing out doesn't hold the
/// control thread most of the time.
const RETRY_MAX: Duration = Duration::from_secs(10);
/// Device events held during a start, at most (they're re-judged when it finishes).
const MAX_DEFERRED: usize = 64;
/// The exclusive period guard (`PeriodGuard`): this many missed deadlines on a stream in [`STRIKE_WINDOW`] of one run step
/// it up a period. One is a click to put up with (a USB device plugged in, a driver's one-off); three in ten minutes is a
/// PC whose drivers hold the CPU past the period, and will keep doing it.
const STRIKES: usize = 3;
const STRIKE_WINDOW: Duration = Duration::from_secs(600);
/// A run's first seconds don't count against its periods: the start's own hold-ups (the model loading, the first page
/// faults) say nothing of the PC's steady state, and would only come back at a longer period.
const GRACE: Duration = Duration::from_secs(5);
/// A run that stays up this long was healthy: a fault after it starts the backoff (and the log's
/// failure streak) over. Before it, a run that opens and faults again (a mic that sends nothing, a
/// flapping USB link) keeps backing off instead of restarting every 2.5 s.
const HEALTHY: Duration = Duration::from_secs(30);
/// A run without its echo reference, with no device event to bring it back (a format change, an
/// exclusive-mode app on the speakers), checks on this timer whether a loopback opens there
/// again: first after this, doubling after each re-attach (a reference that keeps failing)...
const PROBE_FIRST: Duration = Duration::from_secs(15);
/// ...up to this.
const PROBE_MAX: Duration = Duration::from_secs(300);

/// Locks a control-side mutex, ignoring poisoning (the data are plain values that stay valid).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `status()` reports besides the telemetry, and what the listener and the latency test need.
#[derive(Clone, Debug, Default)]
pub(crate) struct StatusInfo {
    /// The error that stopped the engine (or its last start). `None` while running: the warnings
    /// are listed in `warnings`, so the window and the CLI don't show them twice.
    pub(crate) message: Option<String>,
    /// Warnings that hold while running, worded.
    pub(crate) warnings: Vec<String>,
    /// Notes that hold while running (`EngineStatus::notes`), worded.
    pub(crate) notes: Vec<String>,
    pub(crate) mic: Option<StreamInfo>,
    pub(crate) output: Option<StreamInfo>,
    pub(crate) driver_ring: bool,
    pub(crate) reference: Option<StreamInfo>,
    pub(crate) mmcss: bool,
    pub(crate) mic_id: Option<String>,
    pub(crate) output_id: Option<String>,
    pub(crate) reference_id: Option<String>,
    /// The output was Windows' default playback device at the start (`Opened::output_default`).
    pub(crate) output_default: bool,
    pub(crate) targets: BufferTargets,
    /// The live run lost its reference and carries on without it (wasapi/B).
    pub(crate) reference_lost: bool,
    /// The endpoint it lost, for re-attaching it.
    pub(crate) lost_reference_id: Option<String>,
    /// What the user can do about `message` (the window's button).
    pub(crate) remedy: Option<Remedy>,
    /// The run's model pack depth, and how it was chosen (`Opened::model_depth`).
    pub(crate) model_depth: Option<crate::api::ModelDepth>,
}

impl StatusInfo {
    /// Records that the run carries on without its reference (once): the warning, and no reference
    /// id, so the reference coming back re-attaches it (wasapi/C).
    fn lose_reference(&mut self, f: Fault) -> bool {
        if self.reference_lost {
            return false;
        }
        self.reference_lost = true;
        self.reference = None;
        self.lost_reference_id = self.reference_id.take();
        self.targets.reference_ms = 0.0;
        self.warnings.push(describe_reference_lost(f));
        true
    }
}

/// The user's intent and the live run's shape.
#[derive(Debug, Default)]
struct Desired {
    /// What the user asked to run; `None` once stopped.
    config: Option<EngineConfig>,
    /// The config the live audio thread runs with.
    running: Option<EngineConfig>,
    /// The live run built the AEC stage (and opened the reference).
    aec_built: bool,
    /// Stopped by a fault or a failed (re)start: retry on the next device event (and, for a
    /// transient cause, on the timed retry).
    waiting_for_device: bool,
    /// A start is opening streams: device events are held for it (wasapi/D).
    starting: bool,
    /// Device events that arrived during a start, re-judged once it finishes.
    deferred: Vec<DevEvent>,
    rt_alloc_logged: bool,
    /// When the live (or last) run started; cleared by a failed start.
    started_at: Option<Instant>,
    /// The current run of failures and faults with one message, and how many: the log gets the
    /// first, any change, and then attempts 2, 4, 8...
    streak: Option<(String, u32)>,
}

/// The live run on the audio thread. `done` disconnects when the run has ended and released its
/// streams.
struct AudioHandle {
    shared: Arc<AudioShared>,
    done: Receiver<()>,
}

#[derive(Default)]
struct RunState {
    audio: Option<AudioHandle>,
    /// The audio thread, idle between runs.
    worker: Option<Worker>,
    /// A run that didn't stop in time (stuck in a driver call), with the worker it holds.
    stuck: Option<Arc<AudioShared>>,
}

/// A device notification, as plain data.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DevEvent {
    Added(String),
    Removed(String),
    State(String, DeviceState),
    Default(Direction, Role, Option<String>),
    /// A property of the endpoint changed (its default format, its enhancements).
    Property(String),
}

enum Ctl {
    Device(DevEvent),
    /// Settings changed in a way that may need the streams rebuilt (judged when it's due).
    Restart,
    /// A `start` from the UI failed for a reason a later retry can fix.
    ArmRetry,
    Shutdown,
}

/// What a health check or a (re)start did, for the retry timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing changed.
    Idle,
    /// A run started.
    Started,
    /// A run stopped by a fault, or a (re)start failed; `transient` when a retry can fix it.
    Down { transient: bool },
}

/// A failed start: the worded message, and whether a retry can fix it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failed {
    msg: String,
    transient: bool,
}

/// Shared engine state.
pub(crate) struct Inner {
    pub(crate) telemetry: Arc<Telemetry>,
    pub(crate) controls: Arc<ChainControls>,
    pub(crate) effect: Mutex<Option<crate::effect::LoadedEffect>>,
    run: Mutex<RunState>,
    pub(crate) status: Mutex<StatusInfo>,
    desired: Mutex<Desired>,
    /// The live run's atomics.
    pub(crate) current: Mutex<Option<Arc<AudioShared>>>,
    /// The control side of the live run's tap hand-over (the latency test uses it).
    pub(crate) tap: Mutex<Option<TapPort>>,
    /// The chain recorder's: one armed for the next start, and the live run's hand-over (recorder.rs).
    pub(crate) recorder: Mutex<recorder::Slot>,
    readout: AtomicBool,
    ctl: Mutex<Option<Sender<Ctl>>>,
    pub(crate) test_running: AtomicBool,
    /// A chain recorder is armed (one at a time).
    pub(crate) recording: AtomicBool,
    /// The flight recorder's: the armed one, and the live run's hand-over (flight.rs).
    pub(crate) flight: Mutex<flight::Slot>,
    /// A flight recorder is armed (one at a time).
    pub(crate) flying: AtomicBool,
    /// The last mic a run measured, as it opened (its endpoint, mode, period and options), and how it delivered
    /// (`remember_mic_delivery`): what the next start's automatic model depth goes by when it opens the same mic the same
    /// way (TODO 5). In memory for the process.
    pub(crate) mic_delivery: Mutex<Option<(crate::audio::MicOpening, nf_core::config::MicDelivery)>>,
    /// The exclusive period guard's floors (`PeriodGuard`): the periods its streams stepped up to, by endpoint, for every
    /// later start. In memory for the process.
    pub(crate) period_floors: Mutex<PeriodFloors>,
}

impl Inner {
    pub(crate) fn new() -> Arc<Self> {
        let processing = nf_core::config::ProcessingConfig::default();
        Arc::new(Self {
            telemetry: Arc::new(Telemetry::new()),
            controls: Arc::new(ChainControls::new(&processing)),
            effect: Mutex::new(None),
            run: Mutex::new(RunState::default()),
            status: Mutex::new(StatusInfo::default()),
            desired: Mutex::new(Desired::default()),
            current: Mutex::new(None),
            tap: Mutex::new(None),
            recorder: Mutex::new(recorder::Slot::default()),
            readout: AtomicBool::new(processing.latency_readout_enabled),
            ctl: Mutex::new(None),
            test_running: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            flight: Mutex::new(flight::Slot::default()),
            flying: AtomicBool::new(false),
            mic_delivery: Mutex::new(None),
            period_floors: Mutex::new(PeriodFloors::default()),
        })
    }

    /// The control thread (device listener, health checks, restarts) starts with the first
    /// `start`, not with the engine, so an idle engine touches no audio API.
    fn ensure_control_thread(self: &Arc<Self>) {
        let mut ctl = lock(&self.ctl);
        if ctl.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let weak = Arc::downgrade(self);
        let listener_tx = tx.clone();
        match std::thread::Builder::new().name("nf-control".into()).spawn(move || control_main(weak, rx, listener_tx)) {
            Ok(_) => *ctl = Some(tx),
            Err(e) => log::error!("couldn't start the engine's control thread: {e}"),
        }
    }

    fn send(&self, msg: Ctl) {
        if let Some(tx) = lock(&self.ctl).as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// The processing settings, live: the chain reads these atomics every block. Callers hold the
    /// `desired` lock, so a start finishing can't put older values back over newer ones.
    fn apply_live_controls(&self, config: &EngineConfig) {
        let processing = &config.processing;
        self.controls.apply(processing);
        if let Some(effect) = lock(&self.effect).as_ref() {
            effect.apply(&config.effect);
        }
        let readout = processing.latency_readout_enabled;
        self.readout.store(readout, Relaxed);
        if let Some(s) = lock(&self.current).as_ref() {
            s.readout.store(readout, Relaxed);
        }
    }

    /// Whether a device notification should restart (or retry) the engine. During a start the
    /// event is held and judged once the start finishes, against what it opened (wasapi/D).
    fn device_event_wants_restart(&self, ev: &DevEvent) -> bool {
        let mut d = lock(&self.desired);
        if d.starting {
            if d.deferred.len() < MAX_DEFERRED {
                d.deferred.push(ev.clone());
            }
            return false;
        }
        let mut st = lock(&self.status);
        let view = RunView {
            wanted: d.config.as_ref(),
            running: d.running.is_some(),
            waiting: d.waiting_for_device,
            aec_built: d.aec_built,
            mic_id: st.mic_id.as_deref(),
            output_id: st.output_id.as_deref(),
            output_default: st.output_default,
            reference_id: st.reference_id.as_deref(),
            lost_reference_id: st.lost_reference_id.as_deref(),
        };
        let restart = device_event_needs_restart(ev, &view);
        if reference_gone(ev, &view) {
            // The audio thread drops it when its reads fail; say so now, and forget its id so its
            // return re-attaches it.
            let f = Fault::Stream { role: StreamRole::Reference, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED };
            if st.lose_reference(f) {
                log::warn!("the echo reference went away; carrying on without it");
                flight::note(self, Note::ReferenceLost(describe_reference_lost(f)));
            }
        }
        restart
    }

    /// Meant to run but stopped: a retry is due on device events and the timed retry.
    fn armed(&self) -> bool {
        let d = lock(&self.desired);
        d.config.is_some() && d.waiting_for_device && d.running.is_none()
    }

    /// Meant to run (running, starting or waiting to retry): not stopped by the user.
    fn wanted(&self) -> bool {
        lock(&self.desired).config.is_some()
    }

    /// Whether the run that just went down had been up for [`HEALTHY`]: if so, its fault starts a
    /// fresh backoff.
    fn was_healthy(&self) -> bool {
        lock(&self.desired).started_at.is_some_and(|t| t.elapsed() >= HEALTHY)
    }

    /// The live run carries on without its echo reference: the endpoint to check for it again
    /// (`Some(None)`: the default speakers).
    fn lost_reference(&self) -> Option<Option<String>> {
        let d = lock(&self.desired);
        let running = d.running.as_ref()?;
        let st = lock(&self.status);
        // A returned cable can have a different Windows ID. Probe its saved role both after a loss and if it was absent at start.
        if d.aec_built && st.reference_id.is_none() && crate::devices::is_cable_selection(running.devices.reference.as_deref()) {
            return Some(running.devices.reference.clone());
        }
        st.reference_lost.then(|| st.lost_reference_id.clone().or_else(|| running.devices.reference.clone()))
    }

    /// A (re)start that has come due: runs it if it's still needed (`restart_due`), with the wanted
    /// config. Errors land in the status. `stepped`: the exclusive period guard asked for it.
    fn restart(self: &Arc<Self>, forced: bool, stepped: bool) -> Outcome {
        let mut run = lock(&self.run);
        let wanted = {
            let d = lock(&self.desired);
            d.config.clone().filter(|_| restart_due(d.config.as_ref(), d.running.as_ref(), d.aec_built, forced))
        };
        let Some(cfg) = wanted else { return Outcome::Idle };
        let running = {
            let d = lock(&self.desired);
            if d.streak.is_none() {
                // Retries in a streak of failures go unlogged; `fail` logs what matters.
                log::info!("restarting the audio streams");
            }
            d.running.is_some()
        };
        let why = match (forced, running) {
            (true, true) if stepped => "the exclusive period guard",
            (true, _) => "a device change",
            (false, true) => "a settings change",
            (false, false) => "a retry",
        };
        flight::note(self, Note::Restarting(why));
        match start_locked(self, &mut run, &cfg) {
            Ok(()) => Outcome::Started,
            Err(f) => Outcome::Down { transient: f.transient },
        }
    }

    /// The exclusive period guard's look at the live run (`PeriodGuard`): a stream that kept missing its deadline gets a
    /// longer period (or shared mode) for the next start, logged and noted. `true` when one did: the run restarts.
    fn watch_periods(&self, guard: &mut PeriodGuard, now: Instant) -> bool {
        let steps = {
            let d = lock(&self.desired);
            let Some(run) = d.started_at.filter(|_| d.running.is_some()) else { return false };
            let st = lock(&self.status);
            let t = &self.telemetry;
            let mic = Watched::of(st.mic.as_ref(), st.mic_id.as_deref(), t.capture_glitches.load(Relaxed));
            let output = Watched::of(st.output.as_ref(), st.output_id.as_deref(), t.output_late_answers.load(Relaxed));
            guard.check(now, run, mic, output)
        };
        let mut floors = lock(&self.period_floors);
        let floors = &mut *floors;
        let mut stepped = false;
        for (role, step, slot) in [(StreamRole::Mic, steps.mic, &mut floors.mic), (StreamRole::Output, steps.output, &mut floors.output)] {
            let Some(step) = step else { continue };
            let what = match step.1 {
                PeriodFloor::AtLeast(n) => format!("an exclusive period of {:.2} ms or longer", frames_ms(n)),
                PeriodFloor::Shared => "shared mode".to_owned(),
            };
            let line = format!("{} kept missing its deadline at its exclusive period; restarting it at {what}", role.noun());
            log::warn!("{line}");
            flight::note(self, Note::PeriodRaised(line));
            *slot = Some(step);
            stepped = true;
        }
        stepped
    }

    /// The echo-path guard's moves (echo_guard.rs), from the telemetry the audio thread publishes each second: logged, and
    /// noted for the flight recorder. `last`: the state and hold seen before.
    fn watch_echo_guard(&self, last: &mut (u8, f32)) {
        let t = &self.telemetry;
        let state = t.echo_guard.load(Relaxed);
        let now = (state, t.echo_hold_ms.load());
        if let Some(line) = echo_guard_line(std::mem::replace(last, now), now) {
            log::info!("the echo-path guard: {line}");
            flight::note(self, Note::EchoGuard(line));
        }
    }

    /// Notices a run that stopped by itself (a fault, or the thread ending) or lost its reference,
    /// and logs real-time heap allocations once.
    fn check_health(self: &Arc<Self>) -> Outcome {
        let mut outcome = Outcome::Idle;
        let shared = lock(&self.current).clone();
        if let Some(shared) = shared {
            let fault = Fault::unpack(shared.fault.load(Relaxed));
            let died = shared.exited.load(Relaxed) && shared.command.load(Relaxed) != CMD_STOP;
            if fault.is_some() || died {
                let mut run = lock(&self.run);
                let same = run.audio.as_ref().is_some_and(|h| Arc::ptr_eq(&h.shared, &shared));
                if same {
                    stop_locked(self, &mut run);
                    let msg = match fault {
                        Some(f) => describe_fault(f),
                        None => describe_start_failure(&StartFailure::ThreadDied),
                    };
                    {
                        // The wait first, then the state: a status read in between never sees
                        // Error without the retry that goes with it.
                        let mut d = lock(&self.desired);
                        d.waiting_for_device = true;
                        if let Some(line) = streak_line(&mut d.streak, &msg, "audio stopped") {
                            log::warn!("{line}");
                        }
                    }
                    let remedy = fault.and_then(remedy_for_fault);
                    flight::note(self, Note::Fault(msg.clone()));
                    *lock(&self.status) = StatusInfo { message: Some(msg), remedy, ..StatusInfo::default() };
                    self.telemetry.set_state(EngineState::Error);
                    // A device that went away, is busy or is being reset (a format change, an
                    // exclusive-mode app) may send no event when it's back: retry on a timer too.
                    outcome = Outcome::Down { transient: fault.is_none_or(Fault::device_related) };
                }
            } else if let Some(f) = Fault::unpack(shared.reference_fault.load(Relaxed)) {
                let mut st = lock(&self.status);
                let live = lock(&self.current).as_ref().is_some_and(|c| Arc::ptr_eq(c, &shared));
                if live && st.lose_reference(f) {
                    log::warn!("the echo reference failed ({f:?}); carrying on without it");
                    flight::note(self, Note::ReferenceLost(describe_reference_lost(f)));
                }
            }
        }
        let n = self.telemetry.rt_heap_allocations.load(Relaxed);
        let mut d = lock(&self.desired);
        if n > 0 && !d.rt_alloc_logged {
            d.rt_alloc_logged = true;
            log::warn!("{n} allocation(s) on the audio thread fell back to the system heap after warm-up");
        }
        // A run that has stayed up ends the failure streak.
        if d.running.is_some() && d.started_at.is_some_and(|t| t.elapsed() >= HEALTHY) && d.streak.take().is_some() {
            log::info!("the audio streams are running steadily again");
        }
        outcome
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let ctl = self.ctl.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(tx) = ctl {
            let _ = tx.send(Ctl::Shutdown);
        }
        let run = self.run.get_mut().unwrap_or_else(|e| e.into_inner());
        let ended = match run.audio.take() {
            Some(h) => {
                h.shared.command.store(CMD_STOP, Relaxed);
                join_or_detach(h)
            }
            None => true,
        };
        if let Some(w) = run.worker.take() {
            // A stuck run keeps its thread; dropping the worker ends it once the run returns.
            if ended && run.stuck.is_none() {
                w.shut_down();
            }
        }
    }
}

/// The parts of the engine's state a device event is judged against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunView<'a> {
    pub(crate) wanted: Option<&'a EngineConfig>,
    pub(crate) running: bool,
    pub(crate) waiting: bool,
    pub(crate) aec_built: bool,
    pub(crate) mic_id: Option<&'a str>,
    pub(crate) output_id: Option<&'a str>,
    /// The output was Windows' default playback device at the start.
    pub(crate) output_default: bool,
    pub(crate) reference_id: Option<&'a str>,
    /// The reference the live run lost, if it did.
    pub(crate) lost_reference_id: Option<&'a str>,
}

/// Restart when the mic or the output goes away, when the default changes for a role we follow
/// (the mic when none is picked, the reference when AEC uses the default speakers), or when an
/// explicitly picked reference that isn't attached appears (it was missing at start, or was lost;
/// wasapi/C), or a property of a lost reference changes (its format or enhancements were changed,
/// which ends a loopback with no removal or arrival). Losing the reference alone doesn't restart:
/// the audio thread drops it and the filter carries on (wasapi/B). While waiting after a fault, any
/// arrival or default change is worth a retry. Also when the output becomes Windows' default
/// playback device or stops being it: the start's output check (its warning, and shared mode
/// where exclusive was asked: `audio::output_exclusive`) no longer holds, and an exclusive output
/// that became the default would keep every other app from playing sound.
pub(crate) fn device_event_needs_restart(ev: &DevEvent, v: &RunView<'_>) -> bool {
    let Some(cfg) = v.wanted else { return false };
    if v.waiting && !v.running {
        return match ev {
            DevEvent::Added(_) | DevEvent::Default(..) => true,
            DevEvent::State(_, s) => *s == DeviceState::Active,
            DevEvent::Removed(_) | DevEvent::Property(_) => false,
        };
    }
    if !v.running {
        return false;
    }
    let carries = |id: &str| [v.mic_id, v.output_id].contains(&Some(id));
    let reference_back = |id: &str| v.aec_built && cfg.devices.reference.as_deref() == Some(id) && v.reference_id != Some(id);
    match ev {
        DevEvent::Removed(id) => carries(id),
        DevEvent::State(id, s) => {
            if *s == DeviceState::Active {
                reference_back(id)
            } else {
                carries(id)
            }
        }
        DevEvent::Added(id) => reference_back(id),
        DevEvent::Property(id) => v.aec_built && v.lost_reference_id == Some(id.as_str()),
        DevEvent::Default(dir, role, id) => {
            if *role != Role::Console {
                return false;
            }
            match dir {
                Direction::Capture => cfg.devices.mic.is_none() && id.as_deref() != v.mic_id,
                Direction::Render => {
                    let output_default_moved = v.output_id.is_some_and(|o| (id.as_deref() == Some(o)) != v.output_default);
                    output_default_moved || (v.aec_built && cfg.devices.reference.is_none() && id.as_deref() != v.reference_id)
                }
            }
        }
    }
}

/// Whether the event takes away the running reference (and not the mic or output with it).
pub(crate) fn reference_gone(ev: &DevEvent, v: &RunView<'_>) -> bool {
    let id = match ev {
        DevEvent::Removed(id) => id,
        DevEvent::State(id, s) if *s != DeviceState::Active => id,
        _ => return false,
    };
    v.running && v.reference_id == Some(id.as_str()) && ![v.mic_id, v.output_id].contains(&Some(id.as_str()))
}

/// Whether applying `new` over the running config needs the streams rebuilt: devices or I/O
/// changed, AEC turned on without its stage and reference (engine.md "Device rules"), or another noise model, or another
/// depth for a model pack's worker (its stage is built at the start: TODO 5, 6).
pub(crate) fn restart_needed(running: &EngineConfig, aec_built: bool, new: &EngineConfig) -> bool {
    let (r, n) = (&running.processing, &new.processing);
    running.devices != new.devices
        || running.io != new.io
        || running.effect.path != new.effect.path
        || running.effect.plugin_id != new.effect.plugin_id
        || running.effect.worker_blocks != new.effect.worker_blocks
        || (n.aec_enabled && !aec_built)
        || r.noise_model != n.noise_model
        || (n.noise_model != nf_core::config::DEFAULT_NOISE_MODEL && r.model_depth != n.model_depth)
}

/// The devices, the I/O or the AEC switch differ: what decides which streams a start opens.
pub(crate) fn io_changed(a: &EngineConfig, b: &EngineConfig) -> bool {
    a.devices != b.devices || a.io != b.io || a.processing.aec_enabled != b.processing.aec_enabled
}

/// Whether an `apply` asks the control thread for a restart (gui/F2): the devices, I/O or AEC
/// switch changed from the previous wanted config (which also catches a change made while the
/// streams reopen), or the live run doesn't match the new config. While waiting after a failure
/// only the first counts, so a processing-only change (floor, gate, meters) never retries. The
/// control thread judges the restart again when it's due (`restart_due`), so turning AEC off, a
/// bypass, doesn't restart either.
pub(crate) fn apply_wants_restart(prev: &EngineConfig, new: &EngineConfig, running: Option<&EngineConfig>, aec_built: bool) -> bool {
    io_changed(prev, new)
        || prev.effect.worker_blocks != new.effect.worker_blocks
        || prev.effect.path != new.effect.path
        || prev.effect.plugin_id != new.effect.plugin_id
        || running.is_some_and(|r| restart_needed(r, aec_built, new))
}

/// Whether a (re)start that has come due should run, judged against the run as it is then: the
/// engine is meant to run and isn't (a retry), or the live run needs rebuilding for the wanted
/// config, or a device event asked for it (`forced`).
pub(crate) fn restart_due(wanted: Option<&EngineConfig>, running: Option<&EngineConfig>, aec_built: bool, forced: bool) -> bool {
    match (wanted, running) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(w), Some(r)) => forced || restart_needed(r, aec_built, w),
    }
}

/// What the log says of a move of the echo-path guard (echo_guard.rs), from its state and hold before and now
/// (`Telemetry::echo_guard`, `echo_hold_ms`); `None` for nothing to say (a start, a stop, or no move).
pub(crate) fn echo_guard_line((was, was_ms): (u8, f32), (is, ms): (u8, f32)) -> Option<String> {
    use nf_core::telemetry::{ECHO_GUARD_COOLING as COOLING, ECHO_GUARD_HELD as HELD, ECHO_GUARD_TRIAL as TRIAL};
    Some(match (was, is) {
        (w, TRIAL) if w != TRIAL => {
            format!("an echo reaches the mic before its reference, and AEC3 can't cancel it: the mic waits {ms:.1} ms, on a trial")
        }
        (TRIAL, HELD) => format!("the trial cancelled the echo: the mic keeps waiting {ms:.1} ms"),
        (TRIAL, TRIAL) if (ms - was_ms).abs() >= 0.05 => {
            format!("the trial didn't cancel the echo: the mic waits {ms:.1} ms now (it was {was_ms:.1}), on a trial")
        }
        (TRIAL, TRIAL) | (TRIAL, 0) | (HELD, 0) => return None,
        (TRIAL, _) => "the trial didn't cancel the echo: the mic's wait goes, and the next trial waits".to_owned(),
        (HELD, HELD) if (ms - was_ms).abs() >= 0.05 => format!("the mic's wait is now {ms:.1} ms (it was {was_ms:.1})"),
        (HELD, COOLING) => "the wait no longer cancels the echo: it goes, and the next trial waits ten minutes".to_owned(),
        (HELD, h) if h != HELD => "the echo has gone: the mic's wait goes".to_owned(),
        _ => return None,
    })
}

/// Frames at 48 kHz in ms.
fn frames_ms(frames: usize) -> f64 {
    frames as f64 * 1000.0 / f64::from(nf_core::SAMPLE_RATE)
}

/// An exclusive stream as the period guard watches it: its endpoint, its period (frames) and its count of missed
/// deadlines (the output's late answers; the mic's lost samples, `capture_glitches`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Watched {
    pub(crate) id: String,
    pub(crate) frames: usize,
    pub(crate) count: u64,
}

impl Watched {
    /// The stream if it runs exclusive and has an endpoint id to remember it by.
    fn of(info: Option<&StreamInfo>, id: Option<&str>, count: u64) -> Option<Self> {
        let info = info.filter(|i| i.exclusive.is_some())?;
        let frames = (f64::from(info.period_ms) * f64::from(nf_core::SAMPLE_RATE) / 1000.0).round() as usize;
        Some(Self { id: id?.to_owned(), frames, count })
    }
}

/// The exclusive period guard (engine.md, "Exclusive mode"). An exclusive stream has a period to answer its device in: the
/// output's next buffer, the mic's before the device overwrites it. A PC whose drivers hold the CPU longer than that (DPC
/// latency: some network, GPU and ACPI drivers do, for milliseconds) clicks at every hold-up, however light our own work
/// is. A stream that misses [`STRIKES`] deadlines in [`STRIKE_WINDOW`] of one run (not counting its first [`GRACE`]) steps
/// up to the next period that tiles the block (`exclusive::next_floor`: 2, 2.5, 3.33, 5, 10 ms), by one restart, and stays
/// there for later starts in the process; past a block's period it runs shared. Once per stream per run. Pure, for tests:
/// fed the run, the streams and their counters.
#[derive(Debug, Default)]
pub(crate) struct PeriodGuard {
    /// The run watched (when it started), and when its strikes start counting.
    run: Option<(Instant, Instant)>,
    mic: Strikes,
    output: Strikes,
}

#[derive(Debug, Default)]
struct Strikes {
    /// The count last seen.
    seen: u64,
    /// When the strikes inside the window came.
    at: std::collections::VecDeque<Instant>,
    /// Stepped this run already: the restart is on its way.
    stepped: bool,
}

impl Strikes {
    /// The count now: whether the stream has struck out (once).
    fn check(&mut self, now: Instant, count: u64, counting: bool) -> bool {
        let new = count.saturating_sub(self.seen);
        self.seen = count;
        if !counting || self.stepped {
            return false;
        }
        for _ in 0..new.min(STRIKES as u64) {
            self.at.push_back(now);
        }
        while self.at.front().is_some_and(|&t| now.duration_since(t) > STRIKE_WINDOW) {
            self.at.pop_front();
        }
        self.stepped = self.at.len() >= STRIKES;
        self.stepped
    }
}

impl PeriodGuard {
    /// One look at the live run (`started`: when it did, its identity): the floors to set for the streams that struck out.
    pub(crate) fn check(&mut self, now: Instant, started: Instant, mic: Option<Watched>, output: Option<Watched>) -> PeriodFloors {
        if self.run.is_none_or(|(s, _)| s != started) {
            *self = Self { run: Some((started, now + GRACE)), ..Self::default() };
        }
        let counting = self.run.is_some_and(|(_, from)| now >= from);
        let step = |s: &mut Strikes, w: Option<Watched>| {
            let w = w?;
            s.check(now, w.count, counting).then(|| (w.id, next_floor(w.frames)))
        };
        PeriodFloors { mic: step(&mut self.mic, mic), output: step(&mut self.output, output) }
    }
}

/// When the control thread next (re)starts, and why.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    /// A restart asked for by settings changes or device events.
    restart_at: Option<Instant>,
    /// When the current run of settings changes began (it's cut off at COALESCE_MAX).
    first: Option<Instant>,
    /// A device event asked for it: restart even if the config matches the run.
    forced: bool,
    /// The timed retry, while armed (wasapi/A).
    retry_at: Option<Instant>,
    /// The retry's current step; kept across a run that went down before it was healthy.
    backoff: Option<Duration>,
    /// The exclusive period guard asked for the restart (`period_step`).
    stepped: bool,
}

impl Schedule {
    /// A settings change that may need the streams rebuilt: one restart, COALESCE after the
    /// latest change but no later than COALESCE_MAX after the first (gui/F2).
    pub(crate) fn settings_changed(&mut self, now: Instant) {
        let first = *self.first.get_or_insert(now);
        let t = (now + COALESCE).min(first + COALESCE_MAX);
        self.restart_at = Some(self.restart_at.map_or(t, |a| a.max(t)));
    }

    /// A device event that wants a restart: DEBOUNCE after the first of a burst.
    pub(crate) fn device_event(&mut self, now: Instant) {
        self.forced = true;
        self.first.get_or_insert(now);
        self.restart_at.get_or_insert(now + DEBOUNCE);
    }

    /// The exclusive period guard stepped a stream up: a forced restart, as for a device event.
    pub(crate) fn period_step(&mut self, now: Instant) {
        self.stepped = true;
        self.device_event(now);
    }

    /// Whether the restart [`Schedule::take_due`] gave was the period guard's (asked once).
    pub(crate) fn take_stepped(&mut self) -> bool {
        std::mem::take(&mut self.stepped)
    }

    /// The run went down, or a (re)start failed: a transient cause retries after the backoff,
    /// which doubles each time up to RETRY_MAX; any other waits for a device event or a change.
    /// `fresh`: the run that went down had been healthy, so the backoff starts over.
    pub(crate) fn down(&mut self, now: Instant, transient: bool, fresh: bool) {
        if fresh {
            self.backoff = None;
        }
        if !transient {
            self.retry_at = None;
            return;
        }
        let b = self.backoff.map_or(RETRY_FIRST, |b| (b * 2).min(RETRY_MAX));
        self.backoff = Some(b);
        self.retry_at = Some(now + b);
    }

    /// Running: no timed retry, but the backoff stays until the run proves healthy.
    pub(crate) fn running(&mut self) {
        self.retry_at = None;
    }

    /// Stopped by the user: no timed retry, and the backoff starts over.
    pub(crate) fn disarm(&mut self) {
        self.backoff = None;
        self.retry_at = None;
    }

    fn next(&self) -> Option<Instant> {
        [self.restart_at, self.retry_at].into_iter().flatten().min()
    }

    /// How long the control thread may sleep.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        self.next().map_or(POLL, |t| t.saturating_duration_since(now).min(POLL))
    }

    /// When a (re)start is due: clears it and says whether it was forced.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<bool> {
        if self.next()? > now {
            return None;
        }
        self.restart_at = None;
        self.retry_at = None;
        self.first = None;
        Some(std::mem::take(&mut self.forced))
    }
}

/// The device rules that need no device: an output is chosen, and it isn't the reference.
pub(crate) fn validate(config: &EngineConfig) -> Result<(), String> {
    if config.io.output_backend == nf_core::config::OutputBackend::DriverRing {
        return Ok(());
    }
    let Some(out) = config.devices.output.as_deref().filter(|s| !s.is_empty()) else {
        return Err(no_output_message().to_owned());
    };
    if config.devices.reference.as_deref() == Some(out) {
        return Err(same_device_message().to_owned());
    }
    Ok(())
}

/// Clears the per-run telemetry (the clip latch stays until the user resets it).
fn reset_telemetry(t: &Telemetry) {
    let clip = t.clip_latched.load(Relaxed);
    let fresh = Telemetry::new();
    let s = fresh.snapshot();
    t.blocks.store(s.blocks, Relaxed);
    t.input.store(-200.0, -200.0);
    t.output.store(-200.0, -200.0);
    t.near_clip.store(false, Relaxed);
    // NaN reads as "—": nothing reported yet (DSP-4).
    t.noise_reduction_db.store(f32::NAN);
    t.speech_probability.store(f32::NAN);
    t.latency_valid.store(false, Relaxed);
    t.latency_ms_median.store(0.0);
    t.latency_ms_p95.store(0.0);
    t.latency_delivery_ms_median.store(0.0);
    t.latency_delivery_ms_p95.store(0.0);
    t.budget_mean.store(&s.budget_mean);
    t.budget_p95.store(&s.budget_p95);
    t.budget_totals.store(&s.budget_totals);
    t.output_drift_ppm.store(0.0);
    t.reference_drift_ppm.store(0.0);
    t.output_fill_ms.store(0.0);
    t.reference_fill_ms.store(0.0);
    for c in [
        &t.output_slips, &t.reference_slips, &t.capture_glitches, &t.render_underruns, &t.reference_gaps, &t.stalls,
        &t.output_coarse_drops, &t.reference_coarse_drops, &t.tap_overflows, &t.output_deficits, &t.output_zero_frames,
        &t.output_dropped_frames, &t.output_reanchors, &t.output_silence_frames, &t.output_late_answers, &t.publishes, &t.inference_runs,
        &t.inference_misses, &t.inference_late, &t.inference_gaps, &t.inference_errors, &t.inference_worker_allocs,
        &t.inference_worker_fallbacks, &t.wake_blocks[0], &t.wake_blocks[1], &t.wake_blocks[2], &t.wake_blocks[3],
    ] {
        c.store(0, Relaxed);
    }
    t.output_slip_net.store(0, Relaxed);
    for f in [
        &t.block_us_p50, &t.block_us_p99, &t.block_us_max, &t.aec_us_p99, &t.noise_us_p99, &t.aec_delay_ms, &t.aec_erle_db,
        &t.inference_run_us_p50, &t.inference_run_us_p99, &t.inference_run_us_max,
    ] {
        f.store(0.0);
    }
    t.inference_active.store(false, Relaxed);
    t.rt_heap_allocations.store(0, Relaxed);
    t.rt_pool_allocations_per_s.store(0, Relaxed);
    t.mic_period_ms.store(0.0);
    t.output_period_ms.store(0.0);
    t.output_headroom_ms.store(0.0);
    clear_targets(t);
    t.clip_latched.store(clip, Relaxed);
}

/// The fill targets read 0 while stopped.
fn clear_targets(t: &Telemetry) {
    t.output_target_ms.store(0.0);
    t.reference_target_ms.store(0.0);
    t.reference_stamped.store(false, Relaxed);
    for f in [&t.mic_exclusive, &t.output_exclusive, &t.output_adaptive, &t.output_target_on_trial] {
        f.store(false, Relaxed);
    }
    t.echo_hold_ms.store(0.0);
    t.echo_guard.store(0, Relaxed);
    t.echo_correlation.store(0.0);
}

pub(crate) fn start(inner: &Arc<Inner>, config: &EngineConfig) -> Result<(), EngineError> {
    validate(config).map_err(EngineError)?;
    inner.ensure_control_thread();
    let mut run = lock(&inner.run);
    lock(&inner.desired).config = Some(config.clone());
    start_locked(inner, &mut run, config).map_err(|f| {
        if f.transient {
            // The control thread retries on a timer as well as on device events.
            inner.send(Ctl::ArmRetry);
        }
        EngineError(f.msg)
    })
}

/// Stops any run, then starts one with `config` and waits for it to report. Errors are recorded
/// in the status and returned worded. Either way, what changed while the streams opened (the
/// wanted config, device events) is judged again at the end (wasapi/D, gui/F3).
fn start_locked(inner: &Arc<Inner>, run: &mut RunState, config: &EngineConfig) -> Result<(), Failed> {
    stop_locked(inner, run);
    reset_telemetry(&inner.telemetry);
    inner.telemetry.set_state(EngineState::Starting);
    *lock(&inner.status) = StatusInfo::default();

    let readout = {
        let mut d = lock(&inner.desired);
        d.starting = true;
        let wanted = d.config.as_ref().unwrap_or(config);
        inner.apply_live_controls(wanted);
        wanted.processing.latency_readout_enabled
    };
    // A run that timed out may still be stuck in a driver call, holding the device and its thread:
    // a new start would only queue behind it.
    if let Some(s) = &run.stuck {
        if !s.exited.load(Relaxed) {
            return Err(fail(inner, &StartFailure::DriverBusy, config));
        }
        run.stuck = None;
    }
    let external = crate::effect::LoadedEffect::open(&config.effect, inner.controls.clone())
        .map_err(|e| fail(inner, &StartFailure::Stage(e), config))?;
    let (loaded_effect, effect_stage) = external.unzip();
    let shared = Arc::new(AudioShared::default());
    shared.readout.store(readout, Relaxed);
    let (port, end) = tap_channel();
    // A chain recorder armed before this start goes in now, so the run's first wake takes it before its first block; and an
    // armed flight recorder's ring for this run.
    let (rec_port, rec_end) = recorder::run_channel(inner);
    let (flight_port, flight_end) = flight::run_channel(inner);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::channel();
    // Keep the desired config durable for every retry. Only the audio worker gets this run's concrete endpoint IDs.
    let mut opened_config = config.clone();
    let devices = &mut opened_config.devices;
    let output = devices.output.as_mut().filter(|_| config.io.output_backend == nf_core::config::OutputBackend::Wasapi);
    let reference = devices.reference.as_mut().filter(|_| config.processing.aec_enabled);
    crate::devices::resolve_selections([devices.mic.as_mut(), output, reference].into_iter().flatten());
    let args = AudioArgs {
        config: opened_config,
        effect_stage,
        telemetry: inner.telemetry.clone(),
        controls: inner.controls.clone(),
        shared: shared.clone(),
        tap: end,
        recorder: rec_end,
        flight: flight_end,
        mic_delivery: lock(&inner.mic_delivery).clone(),
        floors: lock(&inner.period_floors).clone(),
        ready: ready_tx,
        done: done_tx,
    };
    let Some(worker) = hand_over(run.worker.take(), args) else {
        return Err(fail(inner, &StartFailure::Spawn, config));
    };
    let handle = AudioHandle { shared: shared.clone(), done: done_rx };
    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(opened)) => {
            run.worker = Some(worker);
            run.audio = Some(handle);
            *lock(&inner.current) = Some(shared);
            *lock(&inner.effect) = loaded_effect;
            *lock(&inner.tap) = Some(port);
            recorder::run_started(inner, rec_port);
            flight::run_started(inner, flight_port);
            let aec_built = opened.aec_built;
            *lock(&inner.status) = status_from(opened);
            let (stale, deferred) = settle_start(inner, config, Some(aec_built));
            inner.telemetry.set_state(EngineState::Running);
            resend(inner, stale, deferred);
            Ok(())
        }
        // A chain recorder armed for this start comes back from the ended run for the next (`recorder::run_failed`).
        Ok(Err(f)) => {
            settle_worker(run, worker, handle);
            recorder::run_failed(inner, rec_port);
            flight::run_failed(flight_port);
            Err(fail(inner, &f, config))
        }
        Err(RecvTimeoutError::Timeout) => {
            handle.shared.command.store(CMD_STOP, Relaxed);
            settle_worker(run, worker, handle);
            recorder::run_failed(inner, rec_port);
            flight::run_failed(flight_port);
            Err(fail(inner, &StartFailure::Timeout, config))
        }
        Err(RecvTimeoutError::Disconnected) => {
            settle_worker(run, worker, handle);
            recorder::run_failed(inner, rec_port);
            flight::run_failed(flight_port);
            Err(fail(inner, &StartFailure::ThreadDied, config))
        }
    }
}

/// Gives a run to the audio thread, spawning one if there's none or the old one has gone. `None`
/// when no thread could be had.
fn hand_over(worker: Option<Worker>, args: AudioArgs) -> Option<Worker> {
    let spawn = || match Worker::spawn() {
        Ok(w) => Some(w),
        Err(e) => {
            log::error!("couldn't spawn the audio thread: {e}");
            None
        }
    };
    let worker = match worker {
        Some(w) => w,
        None => spawn()?,
    };
    match worker.run(args) {
        Ok(()) => Some(worker),
        Err(args) => {
            // The thread has gone (it panicked): one fresh thread.
            worker.shut_down();
            let fresh = spawn()?;
            fresh.run(*args).ok()?;
            Some(fresh)
        }
    }
}

/// After a run that failed to start: keeps its worker if the run has ended, else gives the worker
/// up to the stuck run (it ends with it) and remembers the run, so the next start waits for it.
fn settle_worker(run: &mut RunState, worker: Worker, h: AudioHandle) {
    let shared = h.shared.clone();
    if join_or_detach(h) {
        run.worker = Some(worker);
    } else {
        drop(worker);
        run.stuck = Some(shared);
    }
}

/// Records a start failure: Error state, the worded message, and a retry on the next device event
/// (and on a settings change that may fix it, or one made during this start).
fn fail(inner: &Inner, f: &StartFailure, config: &EngineConfig) -> Failed {
    let msg = describe_start_failure(f);
    // The wait first, then the state (gui: a status read in between never sees Error without its
    // retry).
    let (stale, deferred) = settle_start(inner, config, None);
    if let Some(line) = streak_line(&mut lock(&inner.desired).streak, &msg, "audio start failed") {
        log::warn!("{line}");
    }
    flight::note(inner, Note::StartFailed(msg.clone()));
    {
        let mut st = lock(&inner.status);
        st.message = Some(msg.clone());
        st.remedy = remedy_for_start(f);
    }
    inner.telemetry.set_state(EngineState::Error);
    resend(inner, stale, deferred);
    Failed { msg, transient: f.transient() }
}

/// The log line for one more failure or fault worded `msg`, if it gets one: the first of a streak
/// and any change of message, then attempts 2, 4, 8 and so on. A device missing all day retries
/// every 10 s; logging each would fill the 1 MiB log in hours and push out what started it.
pub(crate) fn streak_line(streak: &mut Option<(String, u32)>, msg: &str, what: &str) -> Option<String> {
    match streak {
        Some((m, n)) if m == msg => {
            *n += 1;
            n.is_power_of_two().then(|| format!("{what} again ({n} times in a row): {msg}"))
        }
        _ => {
            *streak = Some((msg.to_owned(), 1));
            Some(format!("{what}: {msg}"))
        }
    }
}

/// The end of a start of `config`: `Some(aec_built)` when it runs, `None` when it failed. Records
/// the run (or the wait), and reconciles it with what changed while the streams opened (wasapi/D,
/// gui/F3). Running, the latest processing goes live (the readout flag too, now that the run's
/// atomics exist), and a device, I/O or AEC change means another restart; failed, a change that
/// may fix it means a retry. Returns that, and the device events held back meanwhile.
fn settle_start(inner: &Inner, config: &EngineConfig, aec_built: Option<bool>) -> (bool, Vec<DevEvent>) {
    let mut d = lock(&inner.desired);
    d.starting = false;
    let stale = match aec_built {
        Some(built) => {
            let mut running = config.clone();
            if let Some(effect) = lock(&inner.effect).as_ref() {
                effect.pin_id(&mut running.effect);
                if let Some(wanted) = &mut d.config {
                    effect.pin_id(&mut wanted.effect);
                }
            }
            d.running = Some(running.clone());
            d.aec_built = built;
            d.waiting_for_device = false;
            d.started_at = Some(Instant::now());
            d.config.as_ref().is_some_and(|w| {
                inner.apply_live_controls(w);
                restart_needed(&running, built, w)
            })
        }
        None => {
            d.waiting_for_device = true;
            d.started_at = None;
            d.config.as_ref().is_some_and(|w| apply_wants_restart(config, w, None, false))
        }
    };
    (stale, std::mem::take(&mut d.deferred))
}

/// Hands what a start held back to the control thread: a restart to judge, and the device events.
fn resend(inner: &Inner, stale: bool, deferred: Vec<DevEvent>) {
    if stale {
        inner.send(Ctl::Restart);
    }
    for ev in deferred {
        inner.send(Ctl::Device(ev));
    }
}

fn status_from(o: Opened) -> StatusInfo {
    let (notes, warnings): (Vec<_>, Vec<_>) = o.warnings.iter().partition(|w| w.is_note());
    StatusInfo {
        message: None,
        warnings: warnings.into_iter().map(describe_warning).collect(),
        notes: notes.into_iter().map(describe_warning).collect(),
        mic: Some(o.mic),
        output: Some(o.output),
        driver_ring: o.driver_ring,
        reference: o.reference,
        mmcss: o.mmcss,
        mic_id: o.mic_id,
        output_id: o.output_id,
        reference_id: o.reference_id,
        output_default: o.output_default,
        targets: o.targets,
        reference_lost: false,
        lost_reference_id: None,
        remedy: None,
        model_depth: o.model_depth,
    }
}

/// Stops the live run, if any: sets its stop flag and joins it (≤ 1 s). The thread stops and
/// releases its own streams, in reverse order, and reverts MMCSS.
fn stop_locked(inner: &Inner, run: &mut RunState) {
    if run.audio.is_some() {
        remember_mic_delivery(inner);
    }
    if let Some(h) = run.audio.take() {
        h.shared.command.store(CMD_STOP, Relaxed);
        let shared = h.shared.clone();
        if !join_or_detach(h) {
            // Stuck in a driver call: the worker stays with the run and ends after it.
            run.worker = None;
            run.stuck = Some(shared);
        }
    }
    *lock(&inner.current) = None;
    *lock(&inner.effect) = None;
    *lock(&inner.tap) = None;
    recorder::run_stopped(inner);
    flight::run_stopped(inner);
    lock(&inner.desired).running = None;
    clear_targets(&inner.telemetry);
}

/// Before a run's status and counters go (at its stop, or a restart's): how its mic delivered, as it opened, for the next
/// start's automatic model depth (TODO 5; `audio::choose_model_depth`), once its wakes say (`WakeBlocks::delivery`: 10 s or
/// more). Exclusive or shared: an exclusive mic whose period is over a block, or whose thread is often held up, can bring
/// two blocks in a wake too. The counters are the run's own (reset at each start), as its last once-a-second publication
/// had them.
fn remember_mic_delivery(inner: &Inner) {
    let opening = {
        let st = lock(&inner.status);
        st.mic.as_ref().map(|info| crate::audio::MicOpening::of(st.mic_id.as_deref(), info))
    };
    if let Some(entry) = opening.and_then(|o| measured_delivery(o, &inner.telemetry.snapshot().wake_blocks)) {
        *lock(&inner.mic_delivery) = Some(entry);
    }
}

/// What a run's wakes say of its mic, to remember: the mic as it opened, and its delivery, once the wakes say (and the
/// endpoint has an id to know it by again). Pure, for tests.
pub(crate) fn measured_delivery(
    mic: crate::audio::MicOpening, wakes: &nf_core::telemetry::WakeBlocks,
) -> Option<(crate::audio::MicOpening, nf_core::config::MicDelivery)> {
    let delivery = wakes.delivery();
    (mic.id.is_some() && delivery != nf_core::config::MicDelivery::Unknown).then_some((mic, delivery))
}

/// Waits up to STOP_TIMEOUT for the run to end and release its streams; `false` if it didn't.
fn join_or_detach(h: AudioHandle) -> bool {
    match h.done.recv_timeout(STOP_TIMEOUT) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => {
            log::error!("the audio thread didn't stop within {STOP_TIMEOUT:?}; leaving it to finish");
            false
        }
    }
}

pub(crate) fn stop(inner: &Arc<Inner>) {
    flight::note(inner, Note::Stopped);
    let mut run = lock(&inner.run);
    {
        // Not meant to run any more: the control thread's retry disarms (it checks `armed`).
        let mut d = lock(&inner.desired);
        d.config = None;
        d.waiting_for_device = false;
        d.streak = None;
    }
    stop_locked(inner, &mut run);
    inner.telemetry.set_state(EngineState::Stopped);
    *lock(&inner.status) = StatusInfo::default();
}

pub(crate) fn apply(inner: &Arc<Inner>, config: &EngineConfig) {
    let restart = {
        let mut guard = lock(&inner.desired);
        let mut new = config.clone();
        if let Some(effect) = lock(&inner.effect).as_ref() {
            effect.pin_id(&mut new.effect);
        }
        // Processing changes need no command: the chain reads these atomics every block.
        inner.apply_live_controls(&new);
        let d = &mut *guard;
        match d.config.as_mut() {
            // Not meant to run: nothing to restart.
            None => false,
            Some(wanted) => {
                let prev = std::mem::replace(wanted, new);
                apply_wants_restart(&prev, wanted, d.running.as_ref(), d.aec_built)
            }
        }
    };
    if restart {
        // The control thread restarts (gathering a run of changes into one), so this returns at once.
        inner.send(Ctl::Restart);
    }
}

pub(crate) fn status(inner: &Arc<Inner>) -> EngineStatus {
    let mut st = lock(&inner.status).clone();
    if st.driver_ring
        && let Some(output) = &mut st.output
    {
        output.period_ms = inner.telemetry.output_period_ms.load();
    }
    let chain_latency_ms =
        lock(&inner.current).as_ref().map_or(0.0, |s| s.chain_latency.load(Relaxed) as f32 * 1000.0 / nf_core::SAMPLE_RATE as f32);
    EngineStatus {
        state: inner.telemetry.state(),
        message: st.message,
        mic: st.mic,
        output: st.output,
        reference: st.reference,
        memory: memory_line(),
        mmcss: st.mmcss,
        chain_latency_ms,
        warnings: st.warnings,
        notes: st.notes,
        retrying: inner.armed(),
        remedy: st.remedy,
        model_depth: st.model_depth,
    }
}

pub(crate) fn buffer_targets(inner: &Arc<Inner>) -> BufferTargets {
    let st = lock(&inner.status);
    let mut targets = st.targets;
    if st.driver_ring {
        targets.output_ms = inner.telemetry.output_target_ms.load();
    }
    targets
}

/// The pool's status line, e.g. "large pages (32 MiB)".
fn memory_line() -> String {
    nf_mem::pool_status().map_or_else(|| "system heap (no locked pool)".to_owned(), |s| s.describe())
}

/// The control thread: owns the device enumerator and its notification registration (both
/// `!Send`, so made here), runs health checks and performs the (gathered, debounced, retried)
/// restarts.
fn control_main(weak: Weak<Inner>, rx: Receiver<Ctl>, tx: Sender<Ctl>) {
    // Declared first, so COM is uninitialised after the registration and enumerator are gone.
    let com = Com::mta();
    let en = com.as_ref().ok().and_then(|_| DeviceEnumerator::new().ok());
    let registration = en.as_ref().and_then(|en| register_listener(en, &tx));
    if registration.is_none() {
        log::warn!("device-change notifications are unavailable; the filter retries on a timer after a fault");
    }
    drop(tx);
    let mut sched = Schedule::default();
    let mut guard = PeriodGuard::default();
    let mut echo_seen = (0u8, 0.0f32);
    // The lost-reference check (wasapi/B): when it's next due, and its current step.
    let mut probe_at: Option<Instant> = None;
    let mut probe_step = PROBE_FIRST;
    loop {
        let msg = rx.recv_timeout(sched.wait(Instant::now()));
        let Some(inner) = weak.upgrade() else { break };
        let now = Instant::now();
        match msg {
            Ok(Ctl::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Ctl::Restart) => sched.settings_changed(now),
            Ok(Ctl::ArmRetry) => sched.down(now, true, false),
            Ok(Ctl::Device(ev)) => {
                if inner.device_event_wants_restart(&ev) {
                    sched.device_event(now);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Outcome::Down { transient } = inner.check_health() {
            sched.down(Instant::now(), transient, inner.was_healthy());
        } else if inner.watch_periods(&mut guard, Instant::now()) {
            sched.period_step(Instant::now());
        }
        inner.watch_echo_guard(&mut echo_seen);
        if let Some(forced) = sched.take_due(Instant::now()) {
            let stepped = sched.take_stepped();
            match inner.restart(forced, stepped) {
                Outcome::Started => sched.running(),
                // A failed start continues the streak.
                Outcome::Down { transient } => sched.down(Instant::now(), transient, false),
                Outcome::Idle => {}
            }
        }
        match inner.lost_reference() {
            None => probe_at = None,
            Some(id) => {
                let now = Instant::now();
                let at = *probe_at.get_or_insert(now + probe_step);
                if now >= at {
                    if en.as_ref().is_some_and(|en| loopback_opens(en, id.as_deref())) {
                        log::info!("the echo reference opens again; restarting to attach it");
                        sched.device_event(now);
                        probe_step = (probe_step * 2).min(PROBE_MAX);
                    }
                    probe_at = Some(now + probe_step);
                }
            }
        }
        if !inner.wanted() {
            sched.disarm();
            probe_step = PROBE_FIRST;
        } else if !inner.armed() {
            sched.running();
        }
        drop(inner);
    }
    drop(registration);
    drop(en);
    drop(com);
}

/// Whether a loopback stream opens on the reference now (it's released at once). On the control
/// thread, which has COM and its own enumerator.
fn loopback_opens(en: &DeviceEnumerator, id: Option<&str>) -> bool {
    let mut selected = id.map(str::to_owned);
    crate::devices::resolve_selections(&mut selected);
    resolve(en, selected.as_deref(), Direction::Render, StreamRole::Reference).is_ok_and(|d| CaptureStream::open_loopback(&d).is_ok())
}

/// Forwards every endpoint notification to the control thread. The callbacks run on a Windows
/// audio thread: they only send, and make no COM calls.
fn register_listener(en: &DeviceEnumerator, tx: &Sender<Ctl>) -> Option<DeviceEventRegistration> {
    let mut cb = DeviceEventCallbacks::new();
    let t = tx.clone();
    cb.set_device_added_callback(move |id| {
        let _ = t.send(Ctl::Device(DevEvent::Added(id)));
    });
    let t = tx.clone();
    cb.set_device_removed_callback(move |id| {
        let _ = t.send(Ctl::Device(DevEvent::Removed(id)));
    });
    let t = tx.clone();
    cb.set_device_state_callback(move |id, state| {
        let _ = t.send(Ctl::Device(DevEvent::State(id, state)));
    });
    let t = tx.clone();
    cb.set_default_device_callback(move |dir, role, id| {
        let _ = t.send(Ctl::Device(DevEvent::Default(dir, role, id)));
    });
    let t = tx.clone();
    cb.set_property_value_callback(move |id, _key| {
        let _ = t.send(Ctl::Device(DevEvent::Property(id)));
    });
    en.register_notification_callback(cb).ok()
}

/// A latency test can't start while another runs; the flag is cleared when its thread ends.
pub(crate) fn claim_test(inner: &Inner) -> bool {
    !inner.test_running.swap(true, AcqRel)
}

#[cfg(test)]
mod tests {
    use nf_core::config::DeviceSelection;

    use super::*;

    fn cfg(mic: Option<&str>, output: Option<&str>, reference: Option<&str>, aec: bool) -> EngineConfig {
        let mut c = EngineConfig {
            devices: DeviceSelection { mic: mic.map(Into::into), output: output.map(Into::into), reference: reference.map(Into::into) },
            ..EngineConfig::default()
        };
        c.processing.aec_enabled = aec;
        c
    }

    #[test]
    fn device_rules() {
        assert_eq!(validate(&cfg(None, None, None, false)), Err(no_output_message().to_owned()));
        assert_eq!(validate(&cfg(None, Some(""), None, false)), Err(no_output_message().to_owned()));
        assert_eq!(validate(&cfg(None, Some("cable"), Some("cable"), true)), Err(same_device_message().to_owned()));
        assert!(validate(&cfg(None, Some("cable"), Some("speakers"), true)).is_ok());
        assert!(validate(&cfg(Some("mic"), Some("cable"), None, true)).is_ok());
        let mut direct = cfg(Some("mic"), None, Some("speakers"), true);
        direct.io.output_backend = nf_core::config::OutputBackend::DriverRing;
        assert!(validate(&direct).is_ok());
        direct.devices.output = direct.devices.reference.clone(); // A saved WASAPI selection is inactive on a direct feed.
        assert!(validate(&direct).is_ok());
    }

    #[test]
    fn engine_refuses_to_start_without_an_output() {
        let engine = crate::Engine::new();
        let err = engine.start(&EngineConfig::default()).unwrap_err();
        assert_eq!(err.0, "Pick the virtual cable to send the cleaned mic to.");
        let err = engine.start(&cfg(None, Some("x"), Some("x"), false)).unwrap_err();
        assert!(err.0.contains("same device"));
        // Nothing was started: stop and apply are harmless.
        engine.stop();
        engine.apply(&cfg(None, Some("x"), None, true));
        assert_eq!(engine.telemetry().state, EngineState::Stopped);
        assert!(lock(&engine.inner.ctl).is_none(), "no control thread for an idle engine");
    }

    #[test]
    fn apply_updates_the_live_controls() {
        let engine = crate::Engine::new();
        let mut c = cfg(None, Some("x"), None, false);
        c.processing.noise_enabled = false;
        c.processing.floor_db = -20.0;
        c.processing.latency_readout_enabled = false;
        engine.apply(&c);
        let ctl = &engine.inner.controls;
        assert!(!ctl.noise.enabled.load(Relaxed));
        assert_eq!(ctl.noise.floor_db.load(), -20.0);
        assert!(!engine.inner.readout.load(Relaxed));
    }

    #[test]
    fn effect_selection_restarts_and_its_live_controls_do_not() {
        let base = cfg(None, Some("cable"), None, false);
        let mut wet = base.clone();
        wet.effect.path = Some("gain.clap".into());
        assert!(restart_needed(&base, false, &wet));
        assert!(apply_wants_restart(&base, &wet, None, false));
        let mut same = wet.clone();
        same.effect.bypass = true;
        same.effect.params.insert(4, 0.75);
        assert!(!apply_wants_restart(&wet, &same, Some(&wet), false));
        same.effect.plugin_id = Some("another.effect.in.the.bundle".into());
        assert!(restart_needed(&wet, false, &same));
    }

    #[test]
    fn what_needs_a_restart() {
        let base = cfg(None, Some("cable"), None, false);
        let mut same = base.clone();
        same.processing.floor_db = -12.0;
        same.processing.gate.threshold_db = -40.0;
        assert!(!restart_needed(&base, false, &same), "processing is live");
        let mut io = base.clone();
        io.io.output_margin_ms = 5.0;
        assert!(restart_needed(&base, false, &io));
        let dev = cfg(Some("other mic"), Some("cable"), None, false);
        assert!(restart_needed(&base, false, &dev));
        let mut aec = base.clone();
        aec.processing.aec_enabled = true;
        assert!(restart_needed(&base, false, &aec), "AEC on needs its stage and reference");
        assert!(!restart_needed(&base, true, &aec), "already built: just the switch");
        let mut aec_off = aec.clone();
        aec_off.processing.aec_enabled = false;
        assert!(!restart_needed(&aec, true, &aec_off), "turning AEC off is a bypass");
        let mut model = base.clone();
        model.processing.noise_model = "ulunas".into();
        assert!(restart_needed(&base, false, &model), "another noise model is another stage");
        assert!(!io_changed(&base, &model), "but not other streams");
        let mut hb = base.clone();
        hb.processing.high_band = nf_core::config::HighBand::Zero;
        assert!(!restart_needed(&base, false, &hb), "the high band is live");
        let mut deeper = model.clone();
        deeper.processing.model_depth = 3;
        assert!(restart_needed(&model, false, &deeper), "a pack's worker at another depth is another stage");
        let mut rnnoise_deeper = base.clone();
        rnnoise_deeper.processing.model_depth = 3;
        assert!(!restart_needed(&base, false, &rnnoise_deeper), "RNNoise has no worker");
    }

    fn view<'a>(c: &'a EngineConfig, running: bool, waiting: bool) -> RunView<'a> {
        RunView {
            wanted: Some(c),
            running,
            waiting,
            aec_built: c.processing.aec_enabled,
            mic_id: Some("mic1"),
            output_id: Some("cable"),
            output_default: false,
            reference_id: c.processing.aec_enabled.then_some("spk1"),
            lost_reference_id: None,
        }
    }

    #[test]
    fn device_events_that_restart() {
        let followed = cfg(None, Some("cable"), None, true);
        let v = view(&followed, true, false);
        let removed = |id: &str| DevEvent::Removed(id.into());
        assert!(device_event_needs_restart(&removed("mic1"), &v));
        assert!(device_event_needs_restart(&removed("cable"), &v));
        // Losing the reference alone doesn't stop the mic path (wasapi/B): it's marked gone instead.
        assert!(!device_event_needs_restart(&removed("spk1"), &v));
        assert!(reference_gone(&removed("spk1"), &v));
        assert!(reference_gone(&DevEvent::State("spk1".into(), DeviceState::Unplugged), &v));
        assert!(!reference_gone(&removed("mic1"), &v) && !reference_gone(&DevEvent::State("spk1".into(), DeviceState::Active), &v));
        assert!(!device_event_needs_restart(&removed("unrelated"), &v));
        assert!(device_event_needs_restart(&DevEvent::State("cable".into(), DeviceState::Unplugged), &v));
        assert!(!device_event_needs_restart(&DevEvent::State("cable".into(), DeviceState::Active), &v));
        assert!(!device_event_needs_restart(&DevEvent::Added("new".into()), &v));
        // Default changes: only the console role, only for a followed default, only if it moved.
        let def = |dir, role, id: &str| DevEvent::Default(dir, role, Some(id.into()));
        assert!(device_event_needs_restart(&def(Direction::Capture, Role::Console, "mic2"), &v));
        assert!(!device_event_needs_restart(&def(Direction::Capture, Role::Console, "mic1"), &v));
        assert!(!device_event_needs_restart(&def(Direction::Capture, Role::Communications, "mic2"), &v));
        assert!(device_event_needs_restart(&def(Direction::Render, Role::Console, "spk2"), &v));
        assert!(device_event_needs_restart(&DevEvent::Default(Direction::Capture, Role::Console, None), &v));
        // An explicitly picked mic doesn't follow the default; no AEC means no reference to follow.
        let explicit = cfg(Some("mic1"), Some("cable"), None, false);
        let v = view(&explicit, true, false);
        assert!(!device_event_needs_restart(&def(Direction::Capture, Role::Console, "mic2"), &v));
        assert!(!device_event_needs_restart(&def(Direction::Render, Role::Console, "spk2"), &v));
        // The output becoming Windows' default playback device, or ceasing to be it: the start's output check (its warning,
        // and the shared mode exclusive mode gives way to there) no longer holds. Not on the console role: nothing.
        assert!(device_event_needs_restart(&def(Direction::Render, Role::Console, "cable"), &v), "became the default");
        assert!(!device_event_needs_restart(&def(Direction::Render, Role::Communications, "cable"), &v));
        let on_default = RunView { output_default: true, ..v };
        assert!(!device_event_needs_restart(&def(Direction::Render, Role::Console, "cable"), &on_default), "still the default");
        assert!(device_event_needs_restart(&def(Direction::Render, Role::Console, "spk2"), &on_default), "no longer the default");
        assert!(device_event_needs_restart(&DevEvent::Default(Direction::Render, Role::Console, None), &on_default));
        let no_id = RunView { output_id: None, ..v };
        assert!(!device_event_needs_restart(&DevEvent::Default(Direction::Render, Role::Console, None), &no_id), "unknown output");
    }

    #[test]
    fn device_events_while_waiting_or_stopped() {
        let c = cfg(Some("mic1"), Some("cable"), None, false);
        let waiting = view(&c, false, true);
        assert!(device_event_needs_restart(&DevEvent::Added("mic1".into()), &waiting));
        assert!(device_event_needs_restart(&DevEvent::State("mic1".into(), DeviceState::Active), &waiting));
        assert!(!device_event_needs_restart(&DevEvent::State("mic1".into(), DeviceState::Disabled), &waiting));
        assert!(!device_event_needs_restart(&DevEvent::Removed("x".into()), &waiting));
        let stopped = RunView { wanted: None, ..waiting };
        assert!(!device_event_needs_restart(&DevEvent::Added("mic1".into()), &stopped));
        let idle = view(&c, false, false);
        assert!(!device_event_needs_restart(&DevEvent::Removed("mic1".into()), &idle));
    }

    #[test]
    fn telemetry_reset_keeps_the_clip_latch() {
        let t = Telemetry::new();
        t.blocks.store(99, Relaxed);
        t.render_underruns.store(3, Relaxed);
        t.clip_latched.store(true, Relaxed);
        t.latency_valid.store(true, Relaxed);
        reset_telemetry(&t);
        let s = t.snapshot();
        assert_eq!((s.blocks, s.render_underruns, s.latency_valid), (0, 0, false));
        assert!(s.clip_latched);
    }

    #[test]
    fn status_lists_the_warnings_and_the_notes_once() {
        let info = StreamInfo {
            device_name: "Mic".into(),
            sample_rate: 48_000,
            channels: 1,
            period_ms: 10.0,
            low_latency: false,
            autoconvert: false,
            raw: true,
            exclusive: None,
            exclusive_refused: None,
        };
        let o = Opened {
            mic: info.clone(),
            output: info,
            driver_ring: false,
            reference: None,
            mic_id: Some("m".into()),
            output_id: Some("o".into()),
            reference_id: None,
            output_default: true,
            mmcss: false,
            aec_built: false,
            warnings: vec![
                crate::errors::Warning::Mmcss(1552),
                crate::errors::Warning::ExclusiveUnavailable {
                    role: StreamRole::Mic,
                    device: "Mic".into(),
                    why: crate::exclusive::ExclusiveRefusal::Refused(crate::errors::hr::AUDCLNT_E_DEVICE_IN_USE),
                },
                crate::errors::Warning::KeepAliveUnavailable(-1),
            ],
            targets: BufferTargets { output_ms: 22.0, reference_ms: 0.0 },
            model_depth: None,
        };
        let s = status_from(o);
        assert_eq!(s.warnings.len(), 2, "one entry per warning: {:?}", s.warnings);
        assert!(s.warnings[0].contains("Pro Audio") && s.warnings[1].contains("keep-alive"));
        // An exclusive stream that runs shared is a note (exclusive mode is the default: a refusal is ordinary), listed once.
        assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
        assert!(s.notes[0].starts_with("The microphone (Mic) runs in shared mode: another app is using it"), "{}", s.notes[0]);
        assert_eq!(s.message, None, "the warnings aren't repeated in the message");
        assert_eq!(s.targets.output_ms, 22.0);
        assert_eq!(s.mic_id.as_deref(), Some("m"));
        assert!(s.output_default, "kept for the device events");
    }

    #[test]
    fn only_one_latency_test_at_a_time() {
        let inner = Inner::new();
        assert!(claim_test(&inner));
        assert!(!claim_test(&inner));
        inner.test_running.store(false, Relaxed);
        assert!(claim_test(&inner));
    }

    /// wasapi/C: an explicitly picked reference that wasn't there at start (or was lost) is
    /// attached when it appears, as an arrival or a state change to Active.
    #[test]
    fn an_explicit_reference_is_attached_when_it_appears() {
        let c = cfg(Some("mic1"), Some("cable"), Some("hp"), true);
        let missing = RunView { reference_id: None, ..view(&c, true, false) };
        assert!(device_event_needs_restart(&DevEvent::Added("hp".into()), &missing));
        assert!(device_event_needs_restart(&DevEvent::State("hp".into(), DeviceState::Active), &missing));
        assert!(!device_event_needs_restart(&DevEvent::Added("other".into()), &missing));
        // Already attached: nothing to do.
        let attached = RunView { reference_id: Some("hp"), ..missing };
        assert!(!device_event_needs_restart(&DevEvent::Added("hp".into()), &attached));
        // No AEC stage built: no reference wanted.
        let no_aec = RunView { aec_built: false, ..missing };
        assert!(!device_event_needs_restart(&DevEvent::Added("hp".into()), &no_aec));
        // Following the default: the Default(Render) event attaches it.
        let followed = cfg(Some("mic1"), Some("cable"), None, true);
        let lost = RunView { reference_id: None, ..view(&followed, true, false) };
        assert!(device_event_needs_restart(&DevEvent::Default(Direction::Render, Role::Console, Some("spk2".into())), &lost));
    }

    /// gui/F2: decided against the previous wanted config; processing-only changes never restart,
    /// running or waiting; turning AEC off (a bypass) asks, but isn't due.
    #[test]
    fn what_an_apply_asks_for() {
        let base = cfg(None, Some("cable"), None, false);
        let mut floor = base.clone();
        floor.processing.floor_db = -20.0;
        floor.processing.gate.threshold_db = -45.0;
        floor.processing.meters_enabled = !base.processing.meters_enabled;
        // Running, or waiting after a failure: no restart for processing.
        assert!(!apply_wants_restart(&base, &floor, Some(&base), false));
        assert!(!apply_wants_restart(&base, &floor, None, false));
        // A margin step or another mic, running or waiting.
        let mut margin = base.clone();
        margin.io.output_margin_ms += 1.0;
        assert!(apply_wants_restart(&base, &margin, Some(&base), false));
        assert!(apply_wants_restart(&base, &margin, None, false));
        // The same config again while the run is still on an older one (a change made during a
        // restart): the run decides.
        assert!(apply_wants_restart(&margin, &margin, Some(&base), false));
        // AEC on without its stage: asked and due. AEC off with it built: asked, not due.
        let mut aec = base.clone();
        aec.processing.aec_enabled = true;
        assert!(apply_wants_restart(&base, &aec, Some(&base), false));
        assert!(restart_due(Some(&aec), Some(&base), false, false));
        assert!(apply_wants_restart(&aec, &base, Some(&aec), true));
        assert!(!restart_due(Some(&base), Some(&aec), true, false), "a bypass");
        // When due: stopped by the user never; meant to run and not running always; a device
        // event forces it.
        assert!(!restart_due(None, Some(&base), false, true));
        assert!(restart_due(Some(&base), None, false, false));
        assert!(!restart_due(Some(&floor), Some(&base), false, false));
        assert!(restart_due(Some(&base), Some(&base), false, true));
    }

    /// gui/F2: a slider drag of 12 steps 50 ms apart makes one restart, 200 ms after the last step
    /// (at most 1 s after the first); device events are debounced and forced.
    #[test]
    fn settings_changes_are_gathered_into_one_restart() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        let mut due = Vec::new();
        for k in 0..40u64 {
            let now = t0 + Duration::from_millis(50 * k);
            if k < 12 {
                s.settings_changed(now);
            }
            if let Some(forced) = s.take_due(now) {
                due.push((k, forced));
            }
        }
        // The 12th step is at 550 ms; the restart is due at 750 ms (k = 15).
        assert_eq!(due, vec![(15, false)]);
        // A drag that goes on: cut off 1 s after its first step.
        let mut s = Schedule::default();
        let mut first_due = None;
        for k in 0..60u64 {
            let now = t0 + Duration::from_millis(50 * k);
            s.settings_changed(now);
            if first_due.is_none() && s.take_due(now).is_some() {
                first_due = Some(k);
            }
        }
        assert_eq!(first_due, Some(20));
        // Device events: 300 ms after the first of a burst, forced; the sleep is bounded by it.
        let mut s = Schedule::default();
        s.device_event(t0);
        s.device_event(t0 + Duration::from_millis(100));
        assert_eq!(s.wait(t0 + Duration::from_millis(250)), Duration::from_millis(50));
        assert_eq!(s.take_due(t0 + Duration::from_millis(299)), None);
        assert_eq!(s.take_due(t0 + Duration::from_millis(300)), Some(true));
        assert_eq!(s.take_due(t0 + Duration::from_secs(5)), None, "once");
        assert_eq!(s.wait(t0), POLL);
        // The period guard's restart: forced, as a device event's, and said once.
        s.period_step(t0);
        assert!(s.take_due(t0).is_none());
        assert_eq!(s.take_due(t0 + DEBOUNCE), Some(true));
        assert!(s.take_stepped());
        assert!(!s.take_stepped());
    }

    /// The exclusive period guard: three missed deadlines in ten minutes of one run step a stream up once (the next period
    /// that tiles the block, by its endpoint); the run's first seconds, strikes that fall out of the window, a shared
    /// stream and another run don't count.
    #[test]
    fn the_period_guard_steps_a_stream_up_after_three_strikes() {
        use crate::exclusive::PeriodFloor::{AtLeast, Shared};
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let w = |id: &str, frames, count| Some(Watched { id: id.into(), frames, count });
        let mut g = PeriodGuard::default();
        let none = PeriodFloors::default();
        // Two late answers in the grace, then one at 10 s and one at 11 min (by when the first has left the window), then
        // two more: the third in the window steps the output up.
        assert_eq!(g.check(at(1), t0, w("mic", 160, 0), w("out", 96, 2)), none, "the grace");
        assert_eq!(g.check(at(10), t0, w("mic", 160, 0), w("out", 96, 3)), none);
        assert_eq!(g.check(at(670), t0, w("mic", 160, 0), w("out", 96, 4)), none);
        assert_eq!(g.check(at(671), t0, w("mic", 160, 0), w("out", 96, 5)), none, "the first fell out of the window");
        let steps = g.check(at(672), t0, w("mic", 160, 0), w("out", 96, 6));
        assert_eq!(steps, PeriodFloors { mic: None, output: Some(("out".into(), AtLeast(120))) });
        assert_eq!(g.check(at(673), t0, w("mic", 160, 0), w("out", 96, 9)), none, "once per run");
        // The mic: three lost-sample glitches in one look; at a block's period, shared mode next.
        let steps = g.check(at(674), t0, w("mic", 480, 3), w("out", 96, 9));
        assert_eq!(steps, PeriodFloors { mic: Some(("mic".into(), Shared)), output: None });
        // A new run starts over (its counters were reset), with its own grace.
        let t1 = at(700);
        assert_eq!(g.check(at(701), t1, w("mic", 160, 5), w("out", 120, 5)), none, "the new run's grace");
        assert_eq!(g.check(at(710), t1, w("mic", 160, 6), None), none, "the output runs shared: not watched");
        assert_eq!(g.check(at(711), t1, w("mic", 160, 8), None), PeriodFloors { mic: Some(("mic".into(), AtLeast(240))), output: None });
    }

    /// The echo-path guard's moves, as the log words them: a trial, kept or not; a trim; the wait let go; and nothing at a
    /// run's start or stop.
    #[test]
    fn the_echo_path_guard_s_moves_are_logged() {
        use nf_core::telemetry::{ECHO_GUARD_COOLING as C, ECHO_GUARD_HELD as H, ECHO_GUARD_TRIAL as T, ECHO_GUARD_WATCHING as W};
        let line = |a, b| echo_guard_line(a, b);
        assert_eq!(line((0, 0.0), (W, 0.0)), None, "a start");
        assert_eq!(line((W, 0.0), (W, 0.0)), None);
        let trial = line((W, 0.0), (T, 10.0)).unwrap();
        assert_eq!(trial, "an echo reaches the mic before its reference, and AEC3 can't cancel it: the mic waits 10.0 ms, on a trial");
        assert_eq!(line((T, 10.0), (T, 10.0)), None);
        let up = "the trial didn't cancel the echo: the mic waits 20.0 ms now (it was 10.0), on a trial";
        assert_eq!(line((T, 10.0), (T, 20.0)).unwrap(), up);
        assert_eq!(line((T, 10.0), (H, 10.0)).unwrap(), "the trial cancelled the echo: the mic keeps waiting 10.0 ms");
        assert!(line((T, 10.0), (C, 0.0)).unwrap().starts_with("the trial didn't cancel the echo"));
        assert_eq!(line((H, 16.3), (H, 8.3)).unwrap(), "the mic's wait is now 8.3 ms (it was 16.3)");
        assert_eq!(line((H, 8.3), (H, 8.3)), None);
        assert!(line((H, 8.3), (C, 0.0)).unwrap().contains("ten minutes"));
        assert_eq!(line((H, 8.3), (W, 0.0)).unwrap(), "the echo has gone: the mic's wait goes");
        assert_eq!(line((H, 8.3), (0, 0.0)), None, "a stop");
        assert_eq!(line((C, 0.0), (W, 0.0)), None);
    }

    /// The guard watches a stream that runs exclusive and has an id, at its period in frames.
    #[test]
    fn the_period_guard_watches_exclusive_streams() {
        let info = |exclusive, period_ms| StreamInfo { exclusive, period_ms, ..StreamInfo::default() };
        let watched = Watched::of(Some(&info(Some("32-bit float"), 10.0 / 3.0)), Some("mic"), 4);
        assert_eq!(watched, Some(Watched { id: "mic".into(), frames: 160, count: 4 }));
        assert_eq!(Watched::of(Some(&info(Some("24-bit"), 2.0)), Some("out"), 0).map(|w| w.frames), Some(96));
        assert_eq!(Watched::of(Some(&info(None, 10.0)), Some("mic"), 4), None, "shared");
        assert_eq!(Watched::of(Some(&info(Some("16-bit"), 2.0)), None, 4), None, "no id");
        assert_eq!(Watched::of(None, Some("mic"), 4), None);
    }

    /// wasapi/A: a transient fault retries on a timer with backoff (0.5 s doubling to 10 s); a
    /// lasting one waits for a device event. A run that comes up and goes down again before it was
    /// healthy keeps the backoff growing (a mic that opens but sends nothing); a healthy run's
    /// fault, or a stop, starts it over.
    #[test]
    fn the_timed_retry_backs_off() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        let mut now = t0;
        let mut steps = Vec::new();
        for k in 0..8 {
            s.down(now, true, false);
            let next = s.next().unwrap();
            steps.push((next - now).as_millis());
            assert_eq!(s.take_due(next - Duration::from_millis(1)), None);
            assert_eq!(s.take_due(next), Some(false));
            now = next;
            if k % 2 == 1 {
                // Every other retry starts a run that faults again within seconds.
                s.running();
                assert_eq!(s.next(), None);
            }
        }
        assert_eq!(steps, vec![500, 1000, 2000, 4000, 8000, 10_000, 10_000, 10_000]);
        s.down(now, true, true);
        assert_eq!(s.next(), Some(now + RETRY_FIRST), "starts over after a healthy run");
        s.down(now, false, false);
        assert_eq!(s.next(), None, "a lasting cause waits for an event");
        s.down(now, true, false);
        s.disarm();
        s.down(now, true, false);
        assert_eq!(s.next(), Some(now + RETRY_FIRST), "starts over after a stop");
    }

    /// verify-wasapi: a device missing all day retries every 10 s; the log gets the first failure,
    /// any change of message, and then attempts 2, 4, 8... only.
    #[test]
    fn a_streak_of_failures_logs_sparsely() {
        let mut streak = None;
        let mut logged = Vec::new();
        for k in 1..=100u32 {
            let msg = if k == 50 { "other" } else { "missing" };
            if let Some(line) = streak_line(&mut streak, msg, "audio start failed") {
                logged.push((k, line));
            }
        }
        let at: Vec<u32> = logged.iter().map(|(k, _)| *k).collect();
        // 50 changes the message and 51 changes it back: each starts a streak.
        assert_eq!(at, vec![1, 2, 4, 8, 16, 32, 50, 51, 52, 54, 58, 66, 82]);
        assert_eq!(logged[0].1, "audio start failed: missing");
        assert_eq!(logged[1].1, "audio start failed again (2 times in a row): missing");
    }

    /// Runs a fake live run (no devices): a run that has already ended stands in for the audio
    /// thread's.
    fn fake_run(inner: &Inner, c: &EngineConfig, shared: &Arc<AudioShared>) {
        let (done_tx, done) = mpsc::channel::<()>();
        drop(done_tx);
        lock(&inner.run).audio = Some(AudioHandle { shared: shared.clone(), done });
        *lock(&inner.current) = Some(shared.clone());
        let mut d = lock(&inner.desired);
        d.config = Some(c.clone());
        d.running = Some(c.clone());
        d.aec_built = c.processing.aec_enabled;
        inner.telemetry.set_state(EngineState::Running);
    }

    #[test]
    fn a_missing_cable_reference_is_retried_by_role_even_when_its_windows_id_changes() {
        let selection = crate::cable_endpoint_selection(nf_vmic::identity::CableId(5), nf_vmic::cables::EndpointKind::Speakers);
        let config = cfg(None, Some("output"), Some(&selection), true);
        let inner = Inner::new();
        fake_run(&inner, &config, &Arc::new(AudioShared::default()));
        assert_eq!(inner.lost_reference(), Some(Some(selection.clone())), "unavailable at first start");
        lock(&inner.status).reference_id = Some("old Windows ID".into());
        assert_eq!(inner.lost_reference(), None);
        inner.device_event_wants_restart(&DevEvent::Removed("old Windows ID".into()));
        assert_eq!(inner.lost_reference(), Some(Some(selection)), "removed while running");
    }

    /// wasapi/A: a fault that ends the run (with no device event to follow) stops it, arms the
    /// retry and says so; what a retry can fix is told apart from what it can't.
    #[test]
    fn a_fault_arms_the_retry() {
        let c = cfg(Some("mic1"), Some("cable"), None, false);
        let cases = [
            (Some(Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED }), true),
            (Some(Fault::Stream { role: StreamRole::Output, hr: hr::AUDCLNT_E_DEVICE_IN_USE }), true),
            (Some(Fault::MicStalled), true),
            (Some(Fault::Stream { role: StreamRole::Mic, hr: hr::E_FAIL }), false),
            (None, true),
        ];
        for (fault, transient) in cases {
            let inner = Inner::new();
            let shared = Arc::new(AudioShared::default());
            match fault {
                Some(f) => shared.fault.store(f.pack(), Relaxed),
                // The thread ended without being asked to (it panicked).
                None => shared.exited.store(true, Relaxed),
            }
            fake_run(&inner, &c, &shared);
            assert_eq!(inner.check_health(), Outcome::Down { transient }, "{fault:?}");
            assert_eq!(inner.telemetry.state(), EngineState::Error);
            let st = status(&inner);
            assert!(st.retrying && st.message.is_some(), "{st:?}");
            assert!(lock(&inner.current).is_none());
            // Stopping by hand disarms it.
            stop(&inner);
            assert!(!status(&inner).retrying && !inner.armed());
        }
    }

    /// wasapi/B: a reference fault the run survived becomes a warning (once), and the reference id
    /// is forgotten so that its return re-attaches it; the run carries on.
    #[test]
    fn a_lost_reference_is_a_warning() {
        let c = cfg(Some("mic1"), Some("cable"), Some("hp"), true);
        let inner = Inner::new();
        let shared = Arc::new(AudioShared::default());
        fake_run(&inner, &c, &shared);
        {
            let mut st = lock(&inner.status);
            st.reference_id = Some("hp".into());
            st.targets = BufferTargets { output_ms: 22.0, reference_ms: 22.0 };
        }
        assert_eq!(inner.check_health(), Outcome::Idle);
        assert!(status(&inner).warnings.is_empty());
        let f = Fault::Stream { role: StreamRole::Reference, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED };
        shared.reference_fault.store(f.pack(), Relaxed);
        for _ in 0..3 {
            assert_eq!(inner.check_health(), Outcome::Idle);
        }
        let st = status(&inner);
        assert_eq!(st.state, EngineState::Running);
        assert_eq!(st.warnings.len(), 1, "{:?}", st.warnings);
        assert!(st.warnings[0].starts_with("Echo cancellation lost its reference"));
        assert_eq!(st.message, None, "listed once, in warnings");
        assert!(!st.retrying);
        assert_eq!(buffer_targets(&inner).reference_ms, 0.0);
        // Its return now restarts to attach it, and so does a change to it with no arrival (a
        // format change, enhancements toggled); the control thread also checks it on a timer.
        assert!(inner.device_event_wants_restart(&DevEvent::Added("hp".into())));
        assert!(inner.device_event_wants_restart(&DevEvent::Property("hp".into())));
        assert!(!inner.device_event_wants_restart(&DevEvent::Property("mic1".into())));
        assert_eq!(inner.lost_reference(), Some(Some("hp".into())));
        // The removal event (if it comes first) is the same warning, not a restart, and not twice.
        let inner = Inner::new();
        fake_run(&inner, &c, &Arc::new(AudioShared::default()));
        lock(&inner.status).reference_id = Some("hp".into());
        assert!(!inner.device_event_wants_restart(&DevEvent::Removed("hp".into())));
        assert_eq!(status(&inner).warnings.len(), 1);
        assert!(!inner.device_event_wants_restart(&DevEvent::Removed("hp".into())));
        assert_eq!(status(&inner).warnings.len(), 1);
        // Property changes of devices the run doesn't use, or while it has its reference, don't
        // restart.
        let inner = Inner::new();
        fake_run(&inner, &c, &Arc::new(AudioShared::default()));
        lock(&inner.status).reference_id = Some("hp".into());
        assert!(!inner.device_event_wants_restart(&DevEvent::Property("hp".into())));
        assert_eq!(inner.lost_reference(), None);
    }

    /// A start that Windows' microphone privacy settings refused says so in the status, with the way to
    /// the setting; the next start clears it.
    #[test]
    fn a_start_refused_by_the_privacy_settings_offers_the_setting() {
        let c = cfg(Some("mic1"), Some("cable"), None, false);
        let inner = Inner::new();
        lock(&inner.desired).config = Some(c.clone());
        let f = fail(&inner, &StartFailure::Open { role: StreamRole::Mic, hr: hr::E_ACCESSDENIED }, &c);
        assert!(!f.transient, "waits for the user, not a timer");
        let st = status(&inner);
        assert_eq!(st.remedy, Some(Remedy::MicPrivacy));
        assert!(st.message.is_some_and(|m| m.contains("Privacy")));
        fail(&inner, &StartFailure::NotFound(StreamRole::Mic), &c);
        assert_eq!(status(&inner).remedy, None);
    }

    /// verify-gui NEW-2: a failed start records the wait before it publishes Error, so a status read
    /// never sees Error without the retry; a healthy run's fault starts a fresh backoff.
    #[test]
    fn a_failed_start_is_waiting_by_the_time_it_reads_error() {
        let c = cfg(Some("mic1"), Some("cable"), None, false);
        let inner = Inner::new();
        lock(&inner.desired).config = Some(c.clone());
        let f = fail(&inner, &StartFailure::NotFound(StreamRole::Mic), &c);
        assert!(f.transient);
        let st = status(&inner);
        assert_eq!(st.state, EngineState::Error);
        assert!(st.retrying, "{st:?}");
        assert!(!inner.was_healthy(), "a failed start isn't a healthy run");
        lock(&inner.desired).started_at = Some(Instant::now() - HEALTHY);
        assert!(inner.was_healthy());
    }

    /// wasapi/D, gui/F3: while a start opens the streams, a device event is held and handed back
    /// once it finishes, a device or AEC change made meanwhile asks for another restart, and the
    /// live controls end up with the latest processing.
    #[test]
    fn changes_during_a_start_are_judged_when_it_finishes() {
        let started = cfg(Some("mic1"), Some("cable"), None, false);
        for (outcome, change_aec) in [(Some(false), true), (Some(false), false), (None, false)] {
            let inner = Inner::new();
            {
                let mut d = lock(&inner.desired);
                d.config = Some(started.clone());
                d.starting = true;
            }
            // During the start: the user ticks AEC (or picks another mic) and moves the floor, and
            // the default mic changes.
            let mut later = started.clone();
            if change_aec {
                later.processing.aec_enabled = true;
            } else {
                later.devices.mic = Some("mic2".into());
            }
            later.processing.floor_db = -33.0;
            apply(&inner, &later);
            let ev = DevEvent::Default(Direction::Capture, Role::Console, Some("mic3".into()));
            assert!(!inner.device_event_wants_restart(&ev), "held, not judged mid-start");
            // A stale start would put the old floor back.
            inner.controls.apply(&started.processing);
            let (stale, deferred) = settle_start(&inner, &started, outcome);
            assert!(stale, "{outcome:?} aec {change_aec}");
            assert_eq!(deferred, vec![ev]);
            assert!(!lock(&inner.desired).starting);
            if outcome.is_some() {
                assert_eq!(inner.controls.noise.floor_db.load(), -33.0, "the latest processing is live");
                assert!(!inner.armed());
            } else {
                assert!(inner.armed(), "a failed start waits");
            }
        }
        // Nothing changed meanwhile: nothing to redo.
        let inner = Inner::new();
        lock(&inner.desired).config = Some(started.clone());
        assert_eq!(settle_start(&inner, &started, Some(false)), (false, vec![]));
    }

    #[test]
    fn telemetry_reset_clears_the_new_fields() {
        let t = Telemetry::new();
        t.speech_probability.store(0.9);
        t.noise_reduction_db.store(-6.0);
        t.stalls.store(4, Relaxed);
        t.output_coarse_drops.store(2, Relaxed);
        t.reference_coarse_drops.store(1, Relaxed);
        t.tap_overflows.store(7, Relaxed);
        t.output_target_ms.store(22.0);
        t.reference_target_ms.store(22.0);
        reset_telemetry(&t);
        let s = t.snapshot();
        assert!(s.speech_probability.is_nan() && s.noise_reduction_db.is_nan());
        assert_eq!((s.stalls, s.output_coarse_drops, s.reference_coarse_drops, s.tap_overflows), (0, 0, 0, 0));
        assert_eq!((s.output_target_ms, s.reference_target_ms), (0.0, 0.0));
    }
}
