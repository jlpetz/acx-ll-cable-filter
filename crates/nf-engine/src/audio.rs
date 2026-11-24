//! The audio thread (engine.md "The audio thread"): one MMCSS "Pro Audio" thread, woken by the
//! mic's capture events (and an exclusive output's), running capture → chain → render. It is kept
//! between runs ([`Worker`]): a restart or a retry hands it the next run instead of spawning a thread.
//!
//! Set-up (stages, streams, warm-up) happens on this thread inside an `RtScope`, so everything it
//! allocates comes from the locked pool. The loop itself never allocates, locks, logs, formats or
//! blocks except on the mic's event: commands arrive through [`AudioShared`]'s atomics and faults
//! leave through them as packed codes, worded later by the control thread.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::thread::JoinHandle;

use nf_core::chain::Chain;
use nf_core::config::{EngineConfig, MicDelivery};
use nf_core::controls::ChainControls;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::slip::SlipResampler;
use nf_core::noise::InferenceStats;
use nf_core::rt::Dither;
use nf_core::telemetry::Telemetry;
use nf_core::{BLOCK, SAMPLE_RATE};
use rtrb::Producer;
use wasapi::{DeviceEnumerator, Direction, Role};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::api::{BufferTargets, ModelDepth, StreamInfo};
use crate::devices::resolve;
use crate::errors::{Fault, StartFailure, StreamRole, Warning, is_virtual_cable};
use crate::exclusive::ExclusiveRefusal;
use crate::handover::{self, End};
use crate::latency_test::TapEnd;
use crate::pipeline::{CaptureSource, DITHER, DriftControl, Hr, Pipeline, PipelineConfig, Resampler, TapItem, Ticks};
use crate::recorder::ChainTx;
use crate::sched::{Com, Mmcss, MmcssPriority};
use crate::stream::{CaptureStream, KeepAlive, MicOptions, Output, hr_of};
use crate::timing::Qpc;

pub(crate) const CMD_RUN: u8 = 0;
pub(crate) const CMD_STOP: u8 = 1;

/// Event wait; this long without the mic's event counts as a stall.
const WAIT_MS: u32 = 100;
/// No mic packets for this long stops the run (100 ns units).
const STALL_LIMIT_HNS: i64 = 20_000_000;
/// Output and probe render-buffer capacity: 100 ms (latency is the padding we keep, not this).
pub(crate) const RENDER_BUFFER_HNS: i64 = 1_000_000;

/// One run's atomics, shared by the control thread and the audio thread.
#[derive(Debug, Default)]
pub(crate) struct AudioShared {
    /// [`CMD_RUN`] or [`CMD_STOP`], read once per wake-up.
    pub(crate) command: AtomicU8,
    /// The latency readout is on.
    pub(crate) readout: AtomicBool,
    /// The latency test wants its tap back.
    pub(crate) tap_stop: AtomicBool,
    /// A packed [`Fault`]; 0 while healthy.
    pub(crate) fault: AtomicU64,
    /// A packed [`Fault`] of the reference or keep-alive stream, which the run survived: it carries
    /// on without echo cancellation's reference (wasapi/B). 0 while the reference is fine.
    pub(crate) reference_fault: AtomicU64,
    /// Set when the run ends, however it ends (also when it unwinds).
    pub(crate) exited: AtomicBool,
    /// Event waits that timed out.
    pub(crate) stalls: AtomicU64,
    /// The chain's stream offset in samples (once a second).
    pub(crate) chain_latency: AtomicU32,
    pub(crate) tap_overflows: AtomicU64,
}

/// What a successful start reports.
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) mic: StreamInfo,
    pub(crate) output: StreamInfo,
    pub(crate) driver_ring: bool,
    pub(crate) reference: Option<StreamInfo>,
    pub(crate) mic_id: Option<String>,
    pub(crate) output_id: Option<String>,
    pub(crate) reference_id: Option<String>,
    /// The output was Windows' default playback device at the start (its warning, and shared mode where exclusive was
    /// asked: `output_exclusive`), so a change of default that ends it restarts the streams.
    pub(crate) output_default: bool,
    pub(crate) mmcss: bool,
    /// The AEC stage exists (AEC was on at start), so turning AEC on later needs no restart.
    pub(crate) aec_built: bool,
    pub(crate) warnings: Vec<Warning>,
    pub(crate) targets: BufferTargets,
    /// The model pack's depth this run chose (`None`: RNNoise).
    pub(crate) model_depth: Option<ModelDepth>,
}

/// Everything the audio thread is started with.
pub(crate) struct AudioArgs {
    pub(crate) config: EngineConfig,
    pub(crate) effect_stage: Option<Box<crate::effect::ExternalStage>>,
    pub(crate) telemetry: Arc<Telemetry>,
    pub(crate) controls: Arc<ChainControls>,
    pub(crate) shared: Arc<AudioShared>,
    pub(crate) tap: TapEnd,
    /// The chain recorder's hand-over (recorder.rs); one armed before the start is in its command ring already.
    pub(crate) recorder: End<ChainTx>,
    /// The flight recorder's (flight.rs): a ring of this run's own is in its command ring already while one is armed.
    pub(crate) flight: End<ChainTx>,
    /// The mic an earlier run in this process measured, as it opened then, and how it delivered (the control side's
    /// memory, for the automatic model depth: `choose_model_depth`).
    pub(crate) mic_delivery: Option<(MicOpening, MicDelivery)>,
    /// The exclusive period guard's floors (control.rs): the periods its streams stepped up to after late answers.
    pub(crate) floors: crate::exclusive::PeriodFloors,
    /// The one-shot "running" (or the failure) for `start()`.
    pub(crate) ready: SyncSender<Result<Opened, StartFailure>>,
    /// Dropped when the thread ends: the control thread waits on it to join with a timeout.
    pub(crate) done: Sender<()>,
}

/// The audio thread, kept between runs (RT-1, wasapi/A). Every new thread pays for easyfft's
/// per-thread FFT state, which easyfft leaks by design (about 22 KB), so restarts and the timed
/// retry hand their runs to this one thread instead of spawning one each. A run stuck in a driver
/// keeps its thread: the control side then gives up on this worker and spawns another.
pub(crate) struct Worker {
    jobs: Sender<AudioArgs>,
    thread: JoinHandle<()>,
}

impl Worker {
    pub(crate) fn spawn() -> std::io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("nf-audio".into()).spawn(move || worker_main(rx))?;
        Ok(Self { jobs, thread })
    }

    /// Hands the thread a run; gives the arguments back if the thread has gone.
    pub(crate) fn run(&self, args: AudioArgs) -> Result<(), Box<AudioArgs>> {
        self.jobs.send(args).map_err(|e| Box::new(e.0))
    }

    /// Ends the idle thread and waits for it (between runs it ends at once).
    pub(crate) fn shut_down(self) {
        drop(self.jobs);
        let _ = self.thread.join();
    }
}

/// Prepares the calling thread for real-time work and enters its real-time scope (RT-1). easyfft
/// leaks its per-thread FFT planners and scratch by design, so they're built first, on the system
/// heap; inside the scope they'd take about 22 KB of the locked pool for good. The stages' own
/// warm-up then finds them built. Keep the order: both the audio thread and its test call this.
pub(crate) fn prepare_rt_thread() -> nf_mem::RtScope {
    nf_stages::warm_up_fft_thread();
    nf_mem::RtScope::enter()
}

/// Marks the run as exited when dropped (also during unwinding).
struct ExitFlag(Arc<AudioShared>);

impl Drop for ExitFlag {
    fn drop(&mut self) {
        self.0.exited.store(true, Relaxed);
    }
}

fn worker_main(jobs: Receiver<AudioArgs>) {
    // 1. COM, MTA, on this thread.
    let com = Com::mta();
    // 2. MMCSS "Pro Audio" (a failure is recorded and we carry on).
    let mmcss = Mmcss::enter(MmcssPriority::Critical);
    // 3. No FTZ/DAZ (a persistent MXCSR change is UB in Rust): the pipeline dithers the mic and
    //    the reference at about -180 dBFS instead, so nothing needs restoring at exit.
    // 4. easyfft's per-thread state, then the real-time allocator scope, for the thread's life.
    let scope = prepare_rt_thread();
    let com_error = com.as_ref().err().copied();
    while let Ok(args) = jobs.recv() {
        run_job(args, com_error, &mmcss);
    }
    drop(scope);
    drop(mmcss);
    drop(com);
}

fn run_job(args: AudioArgs, com_error: Option<i32>, mmcss: &Mmcss) {
    let AudioArgs { config, effect_stage, telemetry, controls, shared, tap, mut recorder, mut flight, mic_delivery, floors, ready, done } =
        args;
    // Declared first, so dropped last: both signal "gone" after every stream is released.
    let _done = done;
    let _exit = ExitFlag(shared.clone());
    match com_error {
        Some(code) => {
            let _ = ready.send(Err(StartFailure::Com(code)));
        }
        None => {
            let ends = Ends { tap, recorder: &mut recorder, flight: &mut flight };
            run(Run { config, effect_stage, telemetry, controls, shared, ends, mic_delivery, floors, ready, mmcss })
        }
    }
    // A chain recorder the run never took (the start failed, or it stopped before its next wake) goes back, so a failed
    // start's caller can keep it for the next start (recorder::run_failed). Before `_done`: the caller has it once joined.
    // The flight recorder's ring goes back too, to be freed on the control side (`flight::run_failed`).
    handover::give_back(&mut recorder);
    handover::give_back(&mut flight);
}

struct Run<'a> {
    config: EngineConfig,
    effect_stage: Option<Box<crate::effect::ExternalStage>>,
    telemetry: Arc<Telemetry>,
    controls: Arc<ChainControls>,
    shared: Arc<AudioShared>,
    ends: Ends<'a>,
    mic_delivery: Option<(MicOpening, MicDelivery)>,
    floors: crate::exclusive::PeriodFloors,
    ready: SyncSender<Result<Opened, StartFailure>>,
    mmcss: &'a Mmcss,
}

/// The streams and the chain of one run. Dropping it stops the streams in reverse start order
/// (mic, output, loopback, keep-alive) and then releases them in the same order.
struct Session {
    mic: CaptureStream,
    out: Output,
    lb: Option<CaptureStream>,
    ka: Option<KeepAlive>,
    chain: Chain,
    /// The noise model's inference worker's stats, when a model pack runs on one (TODO 6).
    inference: Option<Arc<InferenceStats>>,
    /// Its depth, and how it was chosen.
    model_depth: Option<ModelDepth>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.mic.stop();
        self.out.stop();
        if let Some(lb) = &self.lb {
            lb.stop();
        }
        if let Some(ka) = &self.ka {
            ka.stop();
        }
    }
}

/// QPC as the pipeline's tick source.
struct QpcTicks(Qpc);

impl Ticks for QpcTicks {
    #[inline]
    fn ticks(&mut self) -> u64 {
        self.0.ticks() as u64
    }
    #[inline]
    fn to_us(&self, ticks: u64) -> f32 {
        self.0.ticks_to_us(ticks)
    }
}

fn run(r: Run<'_>) {
    let Run { config, effect_stage, telemetry, controls, shared, mut ends, mic_delivery, floors, ready, mmcss } = r;
    let mut warnings = Vec::new();
    if let Some(code) = mmcss.error {
        warnings.push(Warning::Mmcss(code));
    }
    let (mut session, ids) = match open_session(&config, &controls, &shared, mic_delivery.as_ref(), &floors, &mut warnings) {
        Ok(s) => s,
        Err(f) => {
            let _ = ready.send(Err(f));
            return;
        }
    };
    // A driver that took longer than the start timeout: start() has given up and may already have
    // handed the devices to a newer run, so this one goes no further (wasapi, zombie runs).
    if shared.command.load(Relaxed) != CMD_RUN {
        return;
    }

    let direct = config.io.output_backend == nf_core::config::OutputBackend::DriverRing;
    // The direct driver route keeps one reader packet where it runs clean, with headroom only after the driver's underruns
    // (the owner, 2026-11-24; ring_headroom.rs): no margin, and no adaptive target, there.
    let margin_ms = if !direct && config.io.output_margin_ms.is_finite() { config.io.output_margin_ms.clamp(0.0, 100.0) } else { 0.0 };
    // The output's period: the shared engine's pass, or (exclusive) the device's buffer; the target follows it.
    let pcfg = PipelineConfig {
        out_period: session.out.period_frames(),
        out_margin: (margin_ms * SAMPLE_RATE as f32 / 1000.0).round() as usize,
        ref_period: session.lb.as_ref().map(|l| l.info.period_frames),
        max_packet: session.mic.max_packet_frames().max(session.lb.as_ref().map_or(0, |l| l.max_packet_frames())),
    };
    let (out_target, ref_target) = (pcfg.out_target(), pcfg.ref_target());
    let mut pipe: Pipeline = Pipeline::new(pcfg, DriftController::new(out_target as f32), SlipResampler::new(), SlipResampler::new());
    if direct {
        pipe.for_driver_ring();
    } else if config.io.adaptive_output {
        pipe = pipe.with_adaptive_target();
    }
    if config.io.echo_guard {
        pipe = pipe.with_echo_guard();
    }
    let mut targets = BufferTargets {
        output_ms: pipe.out_target() as f32 * 1000.0 / SAMPLE_RATE as f32,
        reference_ms: ref_target as f32 * 1000.0 / SAMPLE_RATE as f32,
    };

    // Prefill the output to its target, warm the chain up with 3 silent blocks (so the stages'
    // first-block set-up is behind us), then take the allocation baseline.
    if let Err(hr) = pipe.prefill(&mut session.out) {
        let _ = ready.send(Err(StartFailure::Open { role: StreamRole::Output, hr }));
        return;
    }
    warm_up(&mut session.chain);
    if let Some(mut stage) = effect_stage {
        let mic = MicOpening::of(ids.mic.as_deref(), &session.mic.info.stream_info());
        let blocks = if config.effect.worker_blocks == 0 { mic.effect_depth(mic_delivery.as_ref()) } else { config.effect.worker_blocks };
        if let Err(e) = stage.set_depth(blocks).and_then(|()| session.chain.attach_effect(stage)) {
            let _ = ready.send(Err(StartFailure::Stage(e.0)));
            return;
        }
    }
    let fallback_baseline = nf_mem::thread_scope_fallbacks();
    pipe.set_alloc_baseline(nf_mem::thread_scope_allocs());

    if shared.command.load(Relaxed) != CMD_RUN {
        return;
    }
    if let Err(f) = start_streams(&mut session, &mut warnings) {
        let _ = ready.send(Err(f));
        return;
    }
    // An exclusive output's first event, answered before the rest of the start (the status, the hand-over): it wants its
    // next period within one (a late first answer is caught and counted all the same: exclusive.rs).
    if let Err(hr) = session.out.service(false) {
        let _ = ready.send(Err(StartFailure::Open { role: StreamRole::Output, hr }));
        return;
    }
    if session.lb.is_none() && ref_target > 0 {
        // The reference wouldn't start: running without it.
        pipe.drop_reference();
        targets.reference_ms = 0.0;
    }

    telemetry.mic_period_ms.store(session.mic.info.period_ms);
    telemetry.output_period_ms.store(session.out.info().period_ms);
    telemetry.mic_exclusive.store(session.mic.info.exclusive.is_some(), Relaxed);
    telemetry.output_exclusive.store(session.out.is_exclusive(), Relaxed);
    telemetry.output_adaptive.store(!direct && config.io.adaptive_output, Relaxed);
    telemetry.output_target_ms.store(targets.output_ms);
    telemetry.reference_target_ms.store(targets.reference_ms);
    shared.chain_latency.store(session.chain.latency(), Relaxed);
    let opened = Opened {
        mic: session.mic.info.stream_info(),
        output: session.out.info().stream_info(),
        driver_ring: direct,
        reference: session.lb.as_ref().map(|l| l.info.stream_info()),
        mic_id: ids.mic,
        output_id: ids.output,
        reference_id: session.lb.as_ref().and(ids.reference),
        output_default: ids.output_default,
        mmcss: mmcss.registered(),
        aec_built: config.processing.aec_enabled,
        warnings,
        targets,
        model_depth: session.model_depth,
    };
    if ready.send(Ok(opened)).is_err() {
        // start() gave up waiting; it has already reported a timeout.
        return;
    }

    // The model pack's worker's misses, for the per-block facts (`ThreadFacts::model_misses`).
    pipe.set_inference(session.inference.clone());
    if let Some(f) = audio_loop(&mut session, &mut pipe, &telemetry, &shared, &mut ends, fallback_baseline) {
        shared.fault.store(f.pack(), Relaxed);
    }
    // `session` drops here: streams stop and are released in reverse order.
}

/// Resolved endpoint ids, for the device-change listener.
struct Ids {
    mic: Option<String>,
    output: Option<String>,
    reference: Option<String>,
    /// The output is Windows' default playback device.
    output_default: bool,
}

/// Resolves the endpoints, opens every stream and builds the stages and the chain (inside the
/// caller's `RtScope`). The reference and keep-alive are optional: failing them is a warning.
/// Devices come first: resolving and opening are cheap, so a start that can't run (a retry waiting
/// for a device, a device in use) fails before building the stages. `mic_delivery`: what an earlier
/// run measured of the mic, for the automatic model depth.
fn open_session(
    config: &EngineConfig, controls: &Arc<ChainControls>, run_shared: &Arc<AudioShared>, mic_delivery: Option<&(MicOpening, MicDelivery)>,
    floors: &crate::exclusive::PeriodFloors, warnings: &mut Vec<Warning>,
) -> Result<(Session, Ids), StartFailure> {
    let aec_wanted = config.processing.aec_enabled;
    let en = DeviceEnumerator::new().map_err(|e| StartFailure::Enumerator(hr_of(&e)))?;
    let mic_dev = resolve(&en, config.devices.mic.as_deref(), Direction::Capture, StreamRole::Mic)?;
    let direct = config.io.output_backend == nf_core::config::OutputBackend::DriverRing;
    let out_dev = if direct {
        let from_ring = mic_dev.get_interface_friendlyname().ok().as_deref().is_some_and(nf_vmic::sessions::is_ours)
            || mic_dev.get_friendlyname().ok().as_deref().is_some_and(nf_vmic::sessions::is_ours);
        if from_ring {
            return if config.devices.mic.is_none() {
                Err(StartFailure::DefaultMicIsOutput("Noise Filter virtual microphone".into()))
            } else {
                Err(StartFailure::DriverRing {
                    message: "Choose a different input microphone: the current input records the driver's own output.".into(),
                    retry: false,
                })
            };
        }
        None
    } else {
        let out_id = config.devices.output.as_deref().ok_or(StartFailure::NotFound(StreamRole::Output))?;
        Some(resolve(&en, Some(out_id), Direction::Render, StreamRole::Output)?)
    };
    let ids_out = out_dev.as_ref().and_then(|d| d.get_id().ok());
    let out_adapter = out_dev.as_ref().and_then(|d| d.get_interface_friendlyname().ok());
    let shared = same_adapter(mic_dev.get_interface_friendlyname().ok(), out_adapter.clone());
    if let Some(w) = adapter_check(config.devices.mic.is_none(), shared)? {
        warnings.push(w);
    }
    let out_name = out_dev.as_ref().and_then(|d| d.get_friendlyname().ok()).unwrap_or_default();
    let out_cable = is_virtual_cable(&out_name) || out_adapter.as_deref().is_some_and(is_virtual_cable);
    let default_out = en.get_default_device_for_role(&Direction::Render, &Role::Console).ok().and_then(|d| d.get_id().ok());
    let out_default = default_out.is_some() && default_out == ids_out;
    if let Some(w) = output_check(out_default, out_cable, out_name.clone(), config.io.output_exclusive) {
        warnings.push(w);
    }

    let mut reference = None;
    if aec_wanted {
        match resolve(&en, config.devices.reference.as_deref(), Direction::Render, StreamRole::Reference) {
            Ok(d) => {
                let id = d.get_id().ok();
                if id.is_some() && id == ids_out {
                    // Our own output would come back as "echo".
                    return Err(StartFailure::OutputIsReference);
                }
                reference = Some((d, id));
            }
            Err(f) => warnings.push(Warning::ReferenceUnavailable(f)),
        }
    }

    let open = |role: StreamRole| move |e: wasapi::WasapiError| StartFailure::Open { role, hr: hr_of(&e) };
    // Exclusive mode where asked (the default; engine.md, "Exclusive mode"): a device that refuses it runs shared, with a
    // note (`Warning::is_note`). Never the output on the default playback device: the output check's warning says so.
    // The exclusive period guard's floors (control.rs): a stream that answered late at its period opens at a longer one,
    // or in shared mode once a block's period was too short as well; a note says so.
    use crate::exclusive::{PeriodFloor, PeriodFloors};
    let mic_floor = config.io.mic_exclusive.then(|| PeriodFloors::of(&floors.mic, mic_dev.get_id().ok().as_deref())).flatten();
    let out_floor = config.io.output_exclusive.then(|| PeriodFloors::of(&floors.output, ids_out.as_deref())).flatten();
    let at_least = |f: Option<PeriodFloor>| match f {
        Some(PeriodFloor::AtLeast(n)) => n,
        _ => 0,
    };
    let mic_options = MicOptions {
        low_latency: config.io.low_latency_periods,
        raw: config.io.raw_mic,
        exclusive: config.io.mic_exclusive && mic_floor != Some(PeriodFloor::Shared),
        exclusive_floor: at_least(mic_floor),
    };
    let (mic, mic_warning) = CaptureStream::open_mic(&mic_dev, mic_options).map_err(open(StreamRole::Mic))?;
    let out_exclusive = output_exclusive(config.io.output_exclusive && out_floor != Some(PeriodFloor::Shared), out_default);
    let (mut out, out_warning) = match out_dev {
        Some(device) => Output::open(&device, out_exclusive.is_ok_and(|e| e), RENDER_BUFFER_HNS, at_least(out_floor))
            .map_err(open(StreamRole::Output))?,
        None => (Output::Driver(crate::ring_output::RingOutput::open()?), None),
    };
    let mic_name = mic.info.stream_info().device_name;
    for (role, floor, device) in [(StreamRole::Mic, mic_floor, mic_name), (StreamRole::Output, out_floor, out_name)] {
        if let Some(floor) = floor {
            warnings.push(Warning::ExclusivePeriodRaised { role, device, floor });
        }
    }
    if let Err(why) = out_exclusive {
        out.withheld(why);
    }
    warnings.extend(mic_warning.into_iter().chain(out_warning));
    let (mut lb, mut ka, mut ref_id) = (None, None, None);
    if let Some((dev, id)) = reference {
        match CaptureStream::open_loopback(&dev) {
            Ok(l) => {
                lb = Some(l);
                ref_id = id;
                if config.io.keep_alive_reference {
                    match KeepAlive::open(&dev) {
                        Ok(k) => ka = Some(k),
                        Err(e) => warnings.push(Warning::KeepAliveUnavailable(hr_of(&e))),
                    }
                }
            }
            Err(e) => warnings.push(Warning::ReferenceUnavailable(StartFailure::Open { role: StreamRole::Reference, hr: hr_of(&e) })),
        }
    }
    let ids = Ids { mic: mic_dev.get_id().ok(), output: ids_out, reference: ref_id, output_default: out_default };

    // The chain, once the streams are open (so the model's depth can follow how the mic opened: `choose_model_depth`).
    // Built and activated here, inside the thread's real-time scope, by the one constructor the offline tools use too, so a
    // chain recording reruns offline exactly (engine.md, "The chain recorder"; the status gives the depth it ran at). A
    // model pack loads and starts on its inference worker meanwhile (TODO 6), this thread waiting for it at most 3 s and
    // giving up when the start is stopped (a start timeout or a stop: the command leaves CMD_RUN); a pack that fails to load
    // fails the start (the streams, opened but not started, are released).
    let model_depth = choose_model_depth(&config.processing, &MicOpening::of(ids.mic.as_deref(), &mic.info.stream_info()), mic_delivery);
    let processing = nf_core::config::ProcessingConfig {
        model_depth: model_depth.map_or(config.processing.model_depth, |d| d.depth),
        ..config.processing.clone()
    };
    let stopped = run_shared.clone();
    let give_up: crate::models::GiveUp = Arc::new(move || stopped.command.load(Relaxed) != CMD_RUN);
    let thread = crate::models::ModelThread::Worker { give_up: Some(give_up) };
    let crate::models::BuiltChain { chain, inference } =
        crate::models::build_chain(controls.clone(), &processing, aec_wanted, thread).map_err(|e| StartFailure::Stage(e.0))?;
    if let Some(code) = inference.as_ref().and_then(|s| s.snapshot().worker_sched_refused) {
        warnings.push(Warning::InferenceMmcss(code));
    }
    Ok((Session { mic, out, lb, ka, chain, inference, model_depth }, ids))
}

/// How a run opened its mic, which is what sets how it delivers: its endpoint, exclusive or shared, its period (frames at
/// 48 kHz), IAudioClient3's small period and RAW mode. The automatic model depth's memory is keyed by it (`remembered` in
/// [`choose_model_depth`]): a measurement holds only for a mic opened the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MicOpening {
    pub(crate) id: Option<String>,
    pub(crate) exclusive: bool,
    pub(crate) period_frames: u32,
    pub(crate) low_latency: bool,
    pub(crate) raw: bool,
}

impl MicOpening {
    fn delivery(&self, remembered: Option<&(Self, MicDelivery)>) -> MicDelivery {
        match remembered.filter(|(then, _)| then.same(self)) {
            Some((_, measured)) => *measured,
            None if self.one_at_a_time() => MicDelivery::OneAtATime,
            None => MicDelivery::Unknown,
        }
    }

    pub(crate) fn effect_depth(&self, remembered: Option<&(Self, MicDelivery)>) -> u8 {
        let blocks_per_wake = self.period_frames.div_ceil(nf_core::BLOCK as u32);
        let delivery_floor = if self.delivery(remembered) == MicDelivery::OneAtATime { 1 } else { 2 };
        blocks_per_wake.max(delivery_floor).min(nf_fx::MAX_DEPTH as u32) as u8
    }

    pub(crate) fn of(id: Option<&str>, info: &StreamInfo) -> Self {
        Self {
            id: id.map(str::to_owned),
            exclusive: info.exclusive.is_some(),
            period_frames: (info.period_ms.max(0.0) * nf_core::SAMPLE_RATE as f32 / 1000.0).round() as u32,
            low_latency: info.low_latency,
            raw: info.raw,
        }
    }

    /// The same endpoint opened the same way (an endpoint without an id matches nothing).
    pub(crate) fn same(&self, other: &MicOpening) -> bool {
        self.id.is_some() && self == other
    }

    /// A block a wake by construction: the exclusive mic at a period of a block or less (each of its events brings at most
    /// one block's last sample; `exclusive::exclusive_period` tiles the block where the device allows). A longer exclusive
    /// period (a device whose minimum is over 10 ms) brings two blocks in some wakes, and a shared mic's delivery is the
    /// audio engine's and the driver's: those need a measurement.
    pub(crate) fn one_at_a_time(&self) -> bool {
        self.exclusive && self.period_frames > 0 && self.period_frames as usize <= nf_core::BLOCK
    }
}

/// The depth a run's model pack runs at (TODO 5): the config's when it sets one; automatic (the default), one block for a mic
/// that delivers a block at a time, else two (bursts, or not measured). How it delivers: as an earlier run in this process
/// measured it (`remembered`: the mic as it opened then, and its delivery), when it opened the same way; else a block at a
/// time for the exclusive mic at a period of a block or less ([`MicOpening::one_at_a_time`]: its 3.33 ms packets); else not
/// known. Chosen at the start, before the chain is built, and never changed mid-stream: the depth is the noise stage's
/// latency, so a change would move the output by a block, a click. A mic first measured in this run gets its depth at the
/// next start (any restart: a settings or device change, a model change). `None` for RNNoise, which has no worker.
pub(crate) fn choose_model_depth(
    p: &nf_core::config::ProcessingConfig, mic: &MicOpening, remembered: Option<&(MicOpening, MicDelivery)>,
) -> Option<ModelDepth> {
    if p.noise_model == nf_core::config::DEFAULT_NOISE_MODEL {
        return None;
    }
    let delivery = mic.delivery(remembered);
    Some(ModelDepth { depth: p.model_depth_for(delivery), auto: p.model_depth_auto().then_some(delivery) })
}

/// The interface friendly name two endpoints share ("VB-Audio Virtual Cable" for both sides of a
/// VB-Cable; CABLE-A and CABLE-B differ), when both read and match.
pub(crate) fn same_adapter(mic: Option<String>, out: Option<String>) -> Option<String> {
    match (mic, out) {
        (Some(a), Some(b)) if !a.trim().is_empty() && a == b => Some(a),
        _ => None,
    }
}

/// A mic on the output's own device (wasapi/E, gui/F1). On a virtual cable that's the cable's
/// recording side, carrying the filter's own output. Following the default (VB-Cable's installer,
/// or Windows promoting the next endpoint, made it the default), the engine waits for another
/// default, as for a missing mic; picked by hand, it runs with a warning. On a real sound card the
/// mic and speakers share a name too, but they're a real mic: nothing to say (the output check
/// covers speakers picked as the output).
pub(crate) fn adapter_check(following_default: bool, shared: Option<String>) -> Result<Option<Warning>, StartFailure> {
    match shared {
        Some(adapter) if is_virtual_cable(&adapter) => {
            if following_default {
                Err(StartFailure::DefaultMicIsOutput(adapter))
            } else {
                Ok(Some(Warning::MicSharesOutputDevice(adapter)))
            }
        }
        _ => Ok(None),
    }
}

/// The output against Windows' default playback device (gui/F1): the default and not a cable is
/// the speakers, so the cleaned mic plays out loud; the default and a cable means every app's
/// sound goes into the filtered mic. Both run, with a warning, and in shared mode: the warning
/// says so where exclusive mode was asked for (`exclusive_asked`; [`output_exclusive`]).
pub(crate) fn output_check(output_is_default: bool, output_is_cable: bool, name: String, exclusive_asked: bool) -> Option<Warning> {
    let exclusive_withheld = exclusive_asked;
    match (output_is_default, output_is_cable) {
        (true, false) => Some(Warning::OutputIsSpeakers { name, exclusive_withheld }),
        (true, true) => Some(Warning::CableIsDefaultPlayback { name, exclusive_withheld }),
        (false, _) => None,
    }
}

/// Whether to open the output in exclusive mode: as asked (`IoConfig::output_exclusive`), but never while it is Windows'
/// default playback device, where the filter would have it to itself and no other app could play sound (the call's own
/// sound included). There it runs shared, as it did before exclusive mode was the default, with the output check's
/// warning; `Err` says why, for the stream's readout. A change of default that ends it restarts the streams
/// (`control::device_event_needs_restart`), so fixing the default brings exclusive mode back.
pub(crate) fn output_exclusive(asked: bool, output_is_default: bool) -> Result<bool, ExclusiveRefusal> {
    if asked && output_is_default { Err(ExclusiveRefusal::DefaultPlayback) } else { Ok(asked) }
}

/// What the audio loop tops up on the reference endpoint: the keep-alive.
pub(crate) trait TopUp {
    fn top_up(&mut self) -> Result<(), Hr>;
}

impl TopUp for KeepAlive {
    #[inline]
    fn top_up(&mut self) -> Result<(), Hr> {
        KeepAlive::top_up(self)
    }
}

/// Tops up the keep-alive and drains the loopback, but only on a wake that will process a block:
/// the reference isn't used before then, so polling it on every wake of a 1 ms mic period only
/// multiplies WASAPI calls (performance/F3). Returns the failing stream's role and HRESULT.
pub(crate) fn service_reference<K: TopUp, C: CaptureSource, D: DriftControl, R: Resampler>(
    pipe: &mut Pipeline<D, R>, ka: Option<&mut K>, lb: Option<&mut C>, now_hns: f64,
) -> Result<(), (StreamRole, Hr)> {
    if !pipe.block_ready() {
        return Ok(());
    }
    if let Some(ka) = ka {
        ka.top_up().map_err(|hr| (StreamRole::KeepAlive, hr))?;
    }
    if let Some(lb) = lb {
        pipe.drain_reference(lb, now_hns).map_err(|hr| (StreamRole::Reference, hr))?;
    }
    Ok(())
}

/// Three dithered silent blocks through the chain, on this thread: before every run's first block (and `warm_up_chain`, for
/// an offline chain that should start from the same state).
pub(crate) fn warm_up(chain: &mut Chain) {
    let mut dither = Dither::new(0x1234_5678);
    let mut mic = [0.0f32; BLOCK];
    let mut reference = [0.0f32; BLOCK];
    let mut out = [0.0f32; BLOCK];
    for _ in 0..3 {
        mic.fill(0.0);
        reference.fill(0.0);
        dither.apply(&mut mic, DITHER);
        dither.apply(&mut reference, DITHER);
        chain.process(&mic, &reference, &mut out);
    }
}

/// How long a shared output's start waits for the mic's first packet (ms): a few of its periods. A mic that sends nothing by
/// then doesn't hold the start up any longer; the run's stall watch takes it from there.
const FIRST_PACKET_WAIT_MS: u32 = 100;

/// Keep-alive first (so the loopback has data), then loopback, then the output and the mic. The reference streams are
/// optional, as they are mid-run (wasapi/B): if they won't start, they're dropped with a warning and the filter runs without
/// them.
///
/// A shared output starts once the mic has its first packet (TODO 13, 2026-11-23). Started before the mic, its prefill
/// drained while the mic's first packet was on its way (14-16 ms on the dev box's cables), and the engine, reading a period
/// ahead, ran it dry: it played up to a period of silence the pipeline never wrote, so every later frame played that much
/// after the frames written said, and the readout read low by it for the whole run (7.7-8.1 ms in 7 runs of 8; the run whose
/// first packet came 5 ms in read right). The sound's own latency didn't move: the queue's target holds it. An exclusive
/// output counts its own silence (exclusive.rs) and wants its first event answered within a period: it starts as before.
fn start_streams(s: &mut Session, warnings: &mut Vec<Warning>) -> Result<(), StartFailure> {
    if let Err((role, hr)) = start_reference(s.ka.as_mut(), s.lb.as_ref()) {
        if let Some(ka) = s.ka.take() {
            ka.stop();
        }
        if let Some(lb) = s.lb.take() {
            lb.stop();
        }
        warnings.push(Warning::ReferenceUnavailable(StartFailure::Open { role, hr }));
    }
    if s.out.is_exclusive() {
        s.out.start().map_err(|hr| StartFailure::Open { role: StreamRole::Output, hr })?;
        s.mic.start().map_err(|hr| StartFailure::Open { role: StreamRole::Mic, hr })?;
    } else {
        s.mic.start().map_err(|hr| StartFailure::Open { role: StreamRole::Mic, hr })?;
        // The event stays set: the loop's first wait reads that packet at once.
        let _ = s.mic.wait_keeping_event(FIRST_PACKET_WAIT_MS);
        s.out.start().map_err(|hr| StartFailure::Open { role: StreamRole::Output, hr })?;
    }
    Ok(())
}

fn start_reference(ka: Option<&mut KeepAlive>, lb: Option<&CaptureStream>) -> Result<(), (StreamRole, Hr)> {
    if let Some(ka) = ka {
        ka.top_up().map_err(|hr| (StreamRole::KeepAlive, hr))?;
        ka.start().map_err(|hr| (StreamRole::KeepAlive, hr))?;
    }
    if let Some(lb) = lb {
        lb.start().map_err(|hr| (StreamRole::Reference, hr))?;
    }
    Ok(())
}

/// Counts the mic's stalls: [`WAIT_MS`] with nothing from the mic is one, whether the wait timed out (the thread waits on the
/// mic alone) or an exclusive output's events woke the thread meanwhile (every 2 ms: they'd never let the wait time out).
/// Nothing means neither the mic's event nor a packet: a thread held up longer than that, woken by the output's event (it's
/// reported first when both have fired), finds the mic's packets waiting, and that's no stall.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StallWatch {
    /// The mic's last event or packet, or the last stall counted (hns).
    since_hns: i64,
}

impl StallWatch {
    pub(crate) fn new(now_hns: i64) -> Self {
        Self { since_hns: now_hns }
    }

    /// A wake at `now_hns` that had something from the mic (its event, or packets) or not: whether it counts a stall.
    #[inline]
    pub(crate) fn wake(&mut self, from_mic: bool, now_hns: i64) -> bool {
        let stall = !from_mic && now_hns - self.since_hns >= i64::from(WAIT_MS) * 10_000;
        if from_mic || stall {
            self.since_hns = now_hns;
        }
        stall
    }
}

/// What woke the audio thread: the mic's event, the exclusive output's, or neither (the wait timed out).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Wake {
    pub(crate) mic: bool,
    pub(crate) out: bool,
}

/// One wake's mic, step 1 of the loop (the flight recorder's scenes run it too): the output's wake counted for the block's
/// facts, every ready mic packet into the mic FIFO (`drain_mic`), then the stall watch, told whether the wake had anything
/// from the mic. Returns the frames read and whether a stall was counted (the pipeline's count too).
#[inline]
pub(crate) fn mic_wake<C: CaptureSource, D: DriftControl, R: Resampler>(
    pipe: &mut Pipeline<D, R>, src: &mut C, stalls: &mut StallWatch, wake: Wake, now_hns: i64,
) -> Result<(usize, bool), Hr> {
    if wake.out {
        pipe.count_out_wake();
    }
    let frames = pipe.drain_mic(src, now_hns as f64)?;
    let stalled = stalls.wake(wake.mic || frames > 0, now_hns);
    if stalled {
        pipe.count_stall();
    }
    Ok((frames, stalled))
}

/// Waits up to `ms` for the mic's event, and for an exclusive output's as well (listed first: when both have fired it's
/// the one reported, as it has a period to be answered in; the mic's stays set for the next wait).
fn wait(mic: &CaptureStream, out: &Output, ms: u32) -> Wake {
    let (Some(out_event), Some(mic_event)) = (out.event(), mic.event()) else {
        return Wake { mic: mic.wait(ms), out: false };
    };
    // SAFETY: both are the streams' own event handles, which live as long as the streams, and so for the call.
    let r = unsafe { WaitForMultipleObjects(&[out_event, mic_event], false, ms) };
    Wake { out: r.0 == WAIT_OBJECT_0.0, mic: r.0 == WAIT_OBJECT_0.0 + 1 }
}

/// The audio thread's ends of the run's hand-overs: the latency test's tap, the chain recorder and the flight recorder.
struct Ends<'a> {
    tap: TapEnd,
    recorder: &'a mut End<ChainTx>,
    flight: &'a mut End<ChainTx>,
}

/// The loop. Returns the fault that ended it, or `None` when asked to stop.
fn audio_loop(
    s: &mut Session, pipe: &mut Pipeline, tel: &Telemetry, shared: &AudioShared, ends: &mut Ends<'_>, fallback_baseline: u64,
) -> Option<Fault> {
    let qpc = Qpc::new();
    let mut ticks = QpcTicks(qpc);
    let mut last_packet = qpc.now_hns();
    let mut stalls = StallWatch::new(last_packet);
    let output_fault = |hr| Some(Fault::Stream { role: StreamRole::Output, hr });
    let mut returning: Option<Producer<TapItem>> = None;
    let mut returning_recorder: Option<ChainTx> = None;
    let mut returning_flight: Option<ChainTx> = None;
    let mut reference_live = s.lb.is_some();
    loop {
        let wake = wait(&s.mic, &s.out, WAIT_MS);
        if shared.command.load(Relaxed) != CMD_RUN {
            return None;
        }
        let now = qpc.now_hns();
        // 0. An exclusive output's event first: the device wants its next period within one.
        if let Err(hr) = s.out.service(wake.out) {
            return output_fault(hr);
        }
        // 1. Mic: every ready packet into the mic FIFO, then the stall watch.
        match mic_wake(pipe, &mut s.mic, &mut stalls, wake, now) {
            Ok((frames, stalled)) => {
                if stalled {
                    shared.stalls.fetch_add(1, Relaxed);
                    tel.stalls.fetch_add(1, Relaxed);
                }
                if frames > 0 {
                    last_packet = now;
                } else if now - last_packet > STALL_LIMIT_HNS {
                    return Some(Fault::MicStalled);
                }
            }
            Err(hr) => return Some(Fault::Stream { role: StreamRole::Mic, hr }),
        }
        // 2. Reference, when a block is due: top up the keep-alive and drain the loopback (polled:
        //    its events stop during silence).
        if reference_live && let Err((role, hr)) = service_reference(pipe, s.ka.as_mut(), s.lb.as_mut(), now as f64) {
            // The reference is optional (wasapi/B): stop its streams once (they're released with
            // the session), carry on with dithered zeros for it, and tell the control thread.
            if let Some(ka) = &s.ka {
                ka.stop();
            }
            if let Some(lb) = &s.lb {
                lb.stop();
            }
            pipe.drop_reference();
            tel.reference_target_ms.store(0.0);
            shared.reference_fault.store(Fault::Stream { role, hr }.pack(), Relaxed);
            reference_live = false;
        }
        // The tap and the recorders: taken before this wake's blocks (a recorder armed before the start, at the first wake,
        // so the run's first block is recorded), and handed back when done, to be freed on the control side.
        handover::service(&mut pipe.tap, &mut ends.tap, shared.tap_stop.load(Relaxed), &mut returning);
        handover::service(&mut pipe.recorder, ends.recorder, false, &mut returning_recorder);
        handover::service(&mut pipe.flight, ends.flight, false, &mut returning_flight);

        // 3. Every complete block; 4. once a second, the slow statistics. The blocks a wake processed are counted (TODO 6:
        //    a model pack's worker misses one for each block past its depth that a wake brings back to back).
        let readout = shared.readout.load(Relaxed);
        let mut blocks = 0usize;
        loop {
            match pipe.process_block(&mut s.chain, &mut s.out, &mut ticks, tel, readout) {
                Ok(true) => {
                    blocks += 1;
                    if pipe.second_due() {
                        // Before publish_second, whose last step marks the second published.
                        tel.publish_inference(s.inference.as_deref());
                        pipe.publish_second(
                            tel,
                            &s.chain,
                            nf_mem::thread_scope_allocs(),
                            nf_mem::thread_scope_fallbacks(),
                            fallback_baseline,
                        );
                        shared.chain_latency.store(s.chain.latency(), Relaxed);
                        shared.tap_overflows.store(pipe.counters.tap_overflows, Relaxed);
                    }
                    // An exclusive output's event that fired during the block: answered before the next.
                    if let Err(hr) = s.out.service(false) {
                        return output_fault(hr);
                    }
                }
                Ok(false) => break,
                Err(hr) => return output_fault(hr),
            }
        }
        tel.count_wake_blocks(blocks);
    }
}

#[cfg(test)]
mod tests {
    use rtrb::RingBuffer;

    use super::*;
    use crate::latency_test::tap_channel;

    /// The latency test's tap goes in and comes back through the generic hand-over (handover.rs), with the run's stop flag.
    #[test]
    fn the_tap_goes_in_and_comes_back() {
        let (mut port, mut end) = tap_channel();
        let shared = AudioShared::default();
        let (mut slot, mut returning) = (None, None);
        let (tx, rx) = RingBuffer::<TapItem>::new(4);
        assert!(port.cmd.push(tx).is_ok());
        handover::service(&mut slot, &mut end, shared.tap_stop.load(Relaxed), &mut returning);
        assert!(slot.is_some() && returning.is_none(), "taken");
        // The test asks for it back.
        shared.tap_stop.store(true, Relaxed);
        handover::service(&mut slot, &mut end, shared.tap_stop.load(Relaxed), &mut returning);
        assert!(slot.is_none() && returning.is_none());
        assert!(port.ret.pop().is_ok(), "returned to the control side");
        drop(rx);
    }

    /// wasapi/E, gui/F1: both sides of one VB-Cable share their interface name. Following the
    /// default, that's a wait (the filter would hear itself); picked by hand, a warning. A real
    /// card's mic and speakers share a name too, and that's fine.
    #[test]
    fn a_mic_on_the_output_device_waits_or_warns() {
        let cable = || Some("VB-Audio Virtual Cable".to_owned());
        assert_eq!(same_adapter(cable(), cable()).as_deref(), Some("VB-Audio Virtual Cable"));
        assert_eq!(same_adapter(Some("VB-Audio Cable A".into()), cable()), None);
        assert_eq!(same_adapter(None, cable()), None, "unreadable: no claim");
        assert_eq!(same_adapter(Some(" ".into()), Some(" ".into())), None);
        assert_eq!(adapter_check(true, None), Ok(None));
        assert_eq!(adapter_check(false, None), Ok(None));
        let shared = same_adapter(cable(), cable());
        assert_eq!(adapter_check(true, shared.clone()), Err(StartFailure::DefaultMicIsOutput("VB-Audio Virtual Cable".into())));
        assert!(adapter_check(true, shared.clone()).unwrap_err().transient(), "waits like a missing mic");
        assert_eq!(adapter_check(false, shared), Ok(Some(Warning::MicSharesOutputDevice("VB-Audio Virtual Cable".into()))));
        let card = Some("Realtek(R) Audio".to_owned());
        assert_eq!(adapter_check(true, card.clone()), Ok(None), "a headset or onboard card isn't a loop");
        assert_eq!(adapter_check(false, card), Ok(None));
    }

    /// gui/F1: the default speakers picked as the output, or the output cable left as Windows'
    /// default playback device, run with a warning, and in shared mode: exclusive there would keep
    /// every other app from playing sound. Anywhere else the output is exclusive as asked.
    #[test]
    fn the_output_is_checked_against_the_default_playback_device() {
        let name = || "Speakers".to_owned();
        for asked in [false, true] {
            let speakers = Warning::OutputIsSpeakers { name: name(), exclusive_withheld: asked };
            assert_eq!(output_check(true, false, name(), asked), Some(speakers));
            let cable = Warning::CableIsDefaultPlayback { name: name(), exclusive_withheld: asked };
            assert_eq!(output_check(true, true, name(), asked), Some(cable));
            assert_eq!(output_check(false, true, name(), asked), None);
            assert_eq!(output_check(false, false, name(), asked), None, "not the default: may be a cable we don't know");
        }
        assert_eq!(output_exclusive(true, true), Err(ExclusiveRefusal::DefaultPlayback), "the default playback device: shared");
        assert_eq!(output_exclusive(true, false), Ok(true));
        assert_eq!(output_exclusive(false, true), Ok(false), "not asked: nothing withheld");
        assert_eq!(output_exclusive(false, false), Ok(false));
    }

    /// One mic packet of `frames` per wake.
    struct Packets {
        frames: usize,
        pos: u64,
        ready: bool,
    }

    impl CaptureSource for Packets {
        fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<crate::pipeline::Packet>, Hr> {
            if !std::mem::take(&mut self.ready) {
                return Ok(None);
            }
            out[..self.frames].fill(0.0);
            let p = crate::pipeline::Packet { frames: self.frames, device_pos: self.pos, qpc_hns: None, discontinuity: false };
            self.pos += self.frames as u64;
            Ok(Some(p))
        }
    }

    /// A loopback that counts its reads, and can fail.
    #[derive(Default)]
    struct CountingLoopback {
        reads: usize,
        fail: Option<Hr>,
    }

    impl CaptureSource for CountingLoopback {
        fn read_packet(&mut self, _out: &mut [f32]) -> Result<Option<crate::pipeline::Packet>, Hr> {
            self.reads += 1;
            self.fail.map_or(Ok(None), Err)
        }
    }

    #[derive(Default)]
    struct CountingKeepAlive {
        top_ups: usize,
        fail: Option<Hr>,
    }

    impl TopUp for CountingKeepAlive {
        fn top_up(&mut self) -> Result<(), Hr> {
            self.top_ups += 1;
            self.fail.map_or(Ok(()), Err)
        }
    }

    fn test_pipe() -> Pipeline {
        let cfg = PipelineConfig { out_period: 480, out_margin: 96, ref_period: Some(480), max_packet: 960 };
        Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new())
    }

    /// performance/F3: with a 48-frame (1 ms) mic period, the reference is serviced on the one wake
    /// in ten that completes a block, not on every wake.
    #[test]
    fn the_reference_is_serviced_only_on_block_wakes() {
        let mut pipe = test_pipe();
        let mut mic = Packets { frames: 48, pos: 0, ready: false };
        let (mut ka, mut lb) = (CountingKeepAlive::default(), CountingLoopback::default());
        let mut blocks = 0;
        for _ in 0..100 {
            mic.ready = true;
            pipe.drain_mic(&mut mic, 0.0).unwrap();
            service_reference(&mut pipe, Some(&mut ka), Some(&mut lb), 0.0).unwrap();
            // Stand in for process_block: take the block.
            if pipe.block_ready() {
                blocks += 1;
                let mut blk = [0.0f32; BLOCK];
                pipe.pop_mic_for_test(&mut blk);
            }
        }
        assert_eq!(blocks, 10);
        assert_eq!((ka.top_ups, lb.reads), (10, 10), "once per block, not per wake");
    }

    /// wasapi/B: a failing loopback or keep-alive is reported with its role, for the loop to drop
    /// the reference and carry on, rather than ending the run.
    #[test]
    fn reference_faults_name_their_stream() {
        let hr = crate::errors::hr::AUDCLNT_E_DEVICE_INVALIDATED;
        let mut pipe = test_pipe();
        let mut mic = Packets { frames: 480, pos: 0, ready: true };
        pipe.drain_mic(&mut mic, 0.0).unwrap();
        let mut lb = CountingLoopback { fail: Some(hr), ..Default::default() };
        let r = service_reference(&mut pipe, Some(&mut CountingKeepAlive::default()), Some(&mut lb), 0.0);
        assert_eq!(r, Err((StreamRole::Reference, hr)));
        let mut ka = CountingKeepAlive { fail: Some(hr), ..Default::default() };
        let r = service_reference(&mut pipe, Some(&mut ka), Some(&mut CountingLoopback::default()), 0.0);
        assert_eq!(r, Err((StreamRole::KeepAlive, hr)));
        let r = service_reference::<CountingKeepAlive, CountingLoopback, _, _>(&mut pipe, None, None, 0.0);
        assert_eq!(r, Ok(()));
    }

    /// RT-1: once a thread has built easyfft's per-thread state (what the audio thread now does
    /// before entering its real-time scope), building, activating and dropping the noise stage
    /// gives back everything it took, so restarts don't leak the locked pool. Its activate starts
    /// RNNoise's spare maker, a thread, and a thread frees a few of the allocations its spawn made
    /// here (its boxed start-up) as it ends, which this thread's count can't see: they're what a
    /// bare spawn and join leaves, counted the same way. (nf-stages' `tests/rt_pool.rs` checks the
    /// pool itself, process-wide: back in full after each start, resets and all.)
    #[test]
    fn a_warmed_thread_builds_the_noise_stage_without_leaking() {
        std::thread::spawn(|| {
            let _scope = prepare_rt_thread();
            let controls = Arc::new(ChainControls::new(&nf_core::config::ProcessingConfig::default()));
            let ((), spawn_allocs, spawn_frees) = crate::test_alloc::count(|| {
                if let Ok(t) = std::thread::Builder::new().name("nf-test-spawn".into()).spawn(|| ()) {
                    let _ = t.join();
                }
            });
            let elsewhere = spawn_allocs - spawn_frees;
            for _ in 0..3 {
                let ((), allocs, frees) = crate::test_alloc::count(|| {
                    use nf_core::stage::{Activation, Stage};
                    let mut s = nf_stages::rnnoise_stage(controls.clone()).unwrap();
                    s.activate(&Activation::HOST).unwrap();
                    drop(s);
                });
                assert!(allocs > 0);
                assert_eq!(allocs, frees + elsewhere, "a start's stage set-up leaks nothing on a warmed thread");
            }
        })
        .join()
        .unwrap();
    }

    /// A stall is 100 ms without the mic's event: each timed-out wait when the thread waits on the mic alone, and once in
    /// each 100 ms when an exclusive output's events wake it every 2 ms meanwhile; the mic's events (3.33 ms packets) between
    /// the output's count none, and a mic event 99 ms after the last none either.
    #[test]
    fn stalls_are_timed_by_the_mic_s_events() {
        let ms = |t: f64| (t * 1e4) as i64;
        let mut w = StallWatch::new(0);
        let timeouts: Vec<bool> = (1..=5).map(|k| w.wake(false, ms(100.05 * k as f64))).collect();
        assert_eq!(timeouts, [true; 5], "each timed-out wait");
        let mut w = StallWatch::new(0);
        let n = (1..=500).filter(|&k| w.wake(false, ms(2.0 * k as f64))).count();
        assert_eq!(n, 10, "1 s of the output's events without the mic's: ten stalls");
        let mut w = StallWatch::new(0);
        let mut events: Vec<(f64, bool)> = (1..=500).map(|k| (2.0 * k as f64, false)).collect();
        events.extend((1..=300).map(|k| (10.0 / 3.0 * k as f64, true)));
        events.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!(events.iter().all(|&(t, mic)| !w.wake(mic, ms(t))), "the mic delivers: no stall");
        let mut w = StallWatch::new(0);
        assert!(!w.wake(true, ms(99.0)) && !w.wake(false, ms(198.0)) && w.wake(false, ms(199.5)));
    }

    /// The loop's mic step: a wake by the exclusive output's event (counted for the block's facts) that finds the mic's packets
    /// waiting is no stall, though the mic's event wasn't the one reported (a thread held up 120 ms finds both fired); one
    /// that finds nothing from the mic 100 ms after its last is, counted for the block's facts too.
    #[test]
    fn a_wake_that_finds_the_mics_packets_is_no_stall() {
        let mut pipe = test_pipe();
        let mut mic = Packets { frames: 480, pos: 0, ready: true };
        let mut stalls = StallWatch::new(0);
        let out = Wake { mic: false, out: true };
        assert_eq!(mic_wake(&mut pipe, &mut mic, &mut stalls, out, 1_200_000).unwrap(), (480, false), "held 120 ms: packets");
        assert_eq!(mic_wake(&mut pipe, &mut mic, &mut stalls, out, 2_200_000).unwrap(), (0, true), "100 ms of nothing");
        assert_eq!(mic_wake(&mut pipe, &mut mic, &mut stalls, Wake::default(), 2_250_000).unwrap(), (0, false));
        assert_eq!(pipe.wake_counts(), (2, 1));
    }

    #[test]
    fn shared_state_starts_healthy() {
        let s = AudioShared::default();
        assert_eq!(s.command.load(Relaxed), CMD_RUN);
        assert_eq!(s.fault.load(Relaxed), 0);
        assert_eq!(s.reference_fault.load(Relaxed), 0);
        let flag = ExitFlag(Arc::new(AudioShared::default()));
        let inner = flag.0.clone();
        drop(flag);
        assert!(inner.exited.load(Relaxed));
    }
}
