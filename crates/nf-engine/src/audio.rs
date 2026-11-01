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
use nf_core::config::EngineConfig;
use nf_core::controls::ChainControls;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::slip::SlipResampler;
use nf_core::rt::Dither;
use nf_core::stage::Stage;
use nf_core::telemetry::Telemetry;
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_stages::{Aec3Stage, RnnoiseStage};
use rtrb::Producer;
use wasapi::{DeviceEnumerator, Direction, Role};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::api::{BufferTargets, StreamInfo};
use crate::devices::resolve;
use crate::errors::{Fault, StartFailure, StreamRole, Warning, is_virtual_cable};
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
    pub(crate) reference: Option<StreamInfo>,
    pub(crate) mic_id: Option<String>,
    pub(crate) output_id: Option<String>,
    pub(crate) reference_id: Option<String>,
    pub(crate) mmcss: bool,
    /// The AEC stage exists (AEC was on at start), so turning AEC on later needs no restart.
    pub(crate) aec_built: bool,
    pub(crate) warnings: Vec<Warning>,
    pub(crate) targets: BufferTargets,
}

/// Everything the audio thread is started with.
pub(crate) struct AudioArgs {
    pub(crate) config: EngineConfig,
    pub(crate) telemetry: Arc<Telemetry>,
    pub(crate) controls: Arc<ChainControls>,
    pub(crate) shared: Arc<AudioShared>,
    pub(crate) tap: TapEnd,
    /// The chain recorder's hand-over (recorder.rs); one armed before the start is in its command ring already.
    pub(crate) recorder: End<ChainTx>,
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
    let AudioArgs { config, telemetry, controls, shared, tap, mut recorder, ready, done } = args;
    // Declared first, so dropped last: both signal "gone" after every stream is released.
    let _done = done;
    let _exit = ExitFlag(shared.clone());
    match com_error {
        Some(code) => {
            let _ = ready.send(Err(StartFailure::Com(code)));
        }
        None => run(Run { config, telemetry, controls, shared, tap, recorder: &mut recorder, ready, mmcss }),
    }
    // A chain recorder the run never took (the start failed, or it stopped before its next wake) goes back, so a failed
    // start's caller can keep it for the next start (recorder::run_failed). Before `_done`: the caller has it once joined.
    handover::give_back(&mut recorder);
}

struct Run<'a> {
    config: EngineConfig,
    telemetry: Arc<Telemetry>,
    controls: Arc<ChainControls>,
    shared: Arc<AudioShared>,
    tap: TapEnd,
    recorder: &'a mut End<ChainTx>,
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
    let Run { config, telemetry, controls, shared, mut tap, recorder, ready, mmcss } = r;
    let mut warnings = Vec::new();
    if let Some(code) = mmcss.error {
        warnings.push(Warning::Mmcss(code));
    }
    let (mut session, ids) = match open_session(&config, &controls, &mut warnings) {
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

    let margin_ms = if config.io.output_margin_ms.is_finite() { config.io.output_margin_ms.clamp(0.0, 100.0) } else { 0.0 };
    // The output's period: the shared engine's pass, or (exclusive) the device's buffer; the target follows it.
    let pcfg = PipelineConfig {
        out_period: session.out.period_frames(),
        out_margin: (margin_ms * SAMPLE_RATE as f32 / 1000.0).round() as usize,
        ref_period: session.lb.as_ref().map(|l| l.info.period_frames),
        max_packet: session.mic.max_packet_frames().max(session.lb.as_ref().map_or(0, |l| l.max_packet_frames())),
    };
    let (out_target, ref_target) = (pcfg.out_target(), pcfg.ref_target());
    let mut pipe: Pipeline = Pipeline::new(pcfg, DriftController::new(out_target as f32), SlipResampler::new(), SlipResampler::new());
    if config.io.adaptive_output {
        pipe = pipe.with_adaptive_target();
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
    telemetry.output_adaptive.store(config.io.adaptive_output, Relaxed);
    telemetry.output_target_ms.store(targets.output_ms);
    telemetry.reference_target_ms.store(targets.reference_ms);
    shared.chain_latency.store(session.chain.latency(), Relaxed);
    let opened = Opened {
        mic: session.mic.info.stream_info(),
        output: session.out.info().stream_info(),
        reference: session.lb.as_ref().map(|l| l.info.stream_info()),
        mic_id: ids.mic,
        output_id: ids.output,
        reference_id: session.lb.as_ref().and(ids.reference),
        mmcss: mmcss.registered(),
        aec_built: config.processing.aec_enabled,
        warnings,
        targets,
    };
    if ready.send(Ok(opened)).is_err() {
        // start() gave up waiting; it has already reported a timeout.
        return;
    }

    let mut ends = Ends { tap: &mut tap, recorder };
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
}

/// Resolves the endpoints, builds the stages and the chain (inside the caller's `RtScope`) and
/// opens every stream. The reference and keep-alive are optional: failing them is a warning.
/// Devices come first: resolving is cheap, so a start that can't run (a retry waiting for a device)
/// fails before building the stages.
fn open_session(config: &EngineConfig, controls: &Arc<ChainControls>, warnings: &mut Vec<Warning>) -> Result<(Session, Ids), StartFailure> {
    let aec_wanted = config.processing.aec_enabled;
    let en = DeviceEnumerator::new().map_err(|e| StartFailure::Enumerator(hr_of(&e)))?;
    let out_id = config.devices.output.as_deref().ok_or(StartFailure::NotFound(StreamRole::Output))?;
    let mic_dev = resolve(&en, config.devices.mic.as_deref(), Direction::Capture, StreamRole::Mic)?;
    let out_dev = resolve(&en, Some(out_id), Direction::Render, StreamRole::Output)?;
    let ids_out = out_dev.get_id().ok();
    let out_adapter = out_dev.get_interface_friendlyname().ok();
    let shared = same_adapter(mic_dev.get_interface_friendlyname().ok(), out_adapter.clone());
    if let Some(w) = adapter_check(config.devices.mic.is_none(), shared)? {
        warnings.push(w);
    }
    let out_name = out_dev.get_friendlyname().unwrap_or_default();
    let out_cable = is_virtual_cable(&out_name) || out_adapter.as_deref().is_some_and(is_virtual_cable);
    let default_out = en.get_default_device_for_role(&Direction::Render, &Role::Console).ok().and_then(|d| d.get_id().ok());
    if let Some(w) = output_check(default_out.is_some() && default_out == ids_out, out_cable, out_name) {
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

    let noise: Box<dyn Stage> = Box::new(RnnoiseStage::new(controls.clone()));
    let aec: Option<Box<dyn Stage>> =
        if aec_wanted { Some(Box::new(Aec3Stage::new(controls.clone()).map_err(|e| StartFailure::Stage(e.0))?)) } else { None };
    let mut chain = Chain::new(controls.clone(), aec, Some(noise));
    chain.activate().map_err(|e| StartFailure::Stage(e.0))?;

    let open = |role: StreamRole| move |e: wasapi::WasapiError| StartFailure::Open { role, hr: hr_of(&e) };
    // Exclusive mode where asked (engine.md, "Exclusive mode"): a device that refuses it runs shared, with a warning.
    let mic_options = MicOptions { low_latency: config.io.low_latency_periods, raw: config.io.raw_mic, exclusive: config.io.mic_exclusive };
    let (mic, mic_warning) = CaptureStream::open_mic(&mic_dev, mic_options).map_err(open(StreamRole::Mic))?;
    let (out, out_warning) = Output::open(&out_dev, config.io.output_exclusive, RENDER_BUFFER_HNS).map_err(open(StreamRole::Output))?;
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
    let ids = Ids { mic: mic_dev.get_id().ok(), output: ids_out, reference: ref_id };
    Ok((Session { mic, out, lb, ka, chain }, ids))
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
/// sound goes into the filtered mic. Both run, with a warning.
pub(crate) fn output_check(output_is_default: bool, output_is_cable: bool, name: String) -> Option<Warning> {
    match (output_is_default, output_is_cable) {
        (true, false) => Some(Warning::OutputIsSpeakers(name)),
        (true, true) => Some(Warning::CableIsDefaultPlayback(name)),
        (false, _) => None,
    }
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

/// Keep-alive first (so the loopback has data), then loopback, output and mic last. The reference
/// streams are optional, as they are mid-run (wasapi/B): if they won't start, they're dropped with
/// a warning and the filter runs without them.
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
    s.out.start().map_err(|hr| StartFailure::Open { role: StreamRole::Output, hr })?;
    s.mic.start().map_err(|hr| StartFailure::Open { role: StreamRole::Mic, hr })?;
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

/// Counts the mic's stalls: [`WAIT_MS`] without its event is one, whether the wait timed out (the thread waits on the mic
/// alone) or an exclusive output's events woke the thread meanwhile (every 2 ms: they'd never let the wait time out).
#[derive(Clone, Copy, Debug)]
struct StallWatch {
    /// The mic's last event, or the last stall counted (hns).
    since_hns: i64,
}

impl StallWatch {
    fn new(now_hns: i64) -> Self {
        Self { since_hns: now_hns }
    }

    /// A wake at `now_hns`, by the mic's event or not: whether it counts a stall.
    #[inline]
    fn wake(&mut self, mic_event: bool, now_hns: i64) -> bool {
        let stall = !mic_event && now_hns - self.since_hns >= i64::from(WAIT_MS) * 10_000;
        if mic_event || stall {
            self.since_hns = now_hns;
        }
        stall
    }
}

/// What woke the audio thread: the mic's event, the exclusive output's, or neither (the wait timed out).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Wake {
    mic: bool,
    out: bool,
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

/// The audio thread's ends of the run's hand-overs: the latency test's tap and the chain recorder.
struct Ends<'a> {
    tap: &'a mut TapEnd,
    recorder: &'a mut End<ChainTx>,
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
        if stalls.wake(wake.mic, now) {
            shared.stalls.fetch_add(1, Relaxed);
            tel.stalls.fetch_add(1, Relaxed);
        }

        // 1. Mic: every ready packet into the mic FIFO.
        match pipe.drain_mic(&mut s.mic, now as f64) {
            Ok(0) => {
                if now - last_packet > STALL_LIMIT_HNS {
                    return Some(Fault::MicStalled);
                }
            }
            Ok(_) => last_packet = now,
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
        // The tap and the recorder: taken before this wake's blocks (a recorder armed before the start, at the first wake,
        // so the run's first block is recorded), and handed back when done, to be freed on the control side.
        handover::service(&mut pipe.tap, ends.tap, shared.tap_stop.load(Relaxed), &mut returning);
        handover::service(&mut pipe.recorder, ends.recorder, false, &mut returning_recorder);

        // 3. Every complete block; 4. once a second, the slow statistics.
        let readout = shared.readout.load(Relaxed);
        loop {
            match pipe.process_block(&mut s.chain, &mut s.out, &mut ticks, tel, readout) {
                Ok(true) => {
                    if pipe.second_due() {
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
    /// default playback device, run with a warning.
    #[test]
    fn the_output_is_checked_against_the_default_playback_device() {
        let name = || "Speakers".to_owned();
        assert_eq!(output_check(true, false, name()), Some(Warning::OutputIsSpeakers("Speakers".into())));
        assert_eq!(output_check(true, true, name()), Some(Warning::CableIsDefaultPlayback("Speakers".into())));
        assert_eq!(output_check(false, true, name()), None);
        assert_eq!(output_check(false, false, name()), None, "not the default: may be a cable we don't know");
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
    /// gives back everything it took, so restarts don't leak the locked pool.
    #[test]
    fn a_warmed_thread_builds_the_noise_stage_without_leaking() {
        std::thread::spawn(|| {
            let _scope = prepare_rt_thread();
            let controls = Arc::new(ChainControls::new(&nf_core::config::ProcessingConfig::default()));
            for _ in 0..3 {
                let ((), allocs, frees) = crate::test_alloc::count(|| {
                    let mut s = RnnoiseStage::new(controls.clone());
                    s.activate(SAMPLE_RATE, BLOCK).unwrap();
                    drop(s);
                });
                assert!(allocs > 0);
                assert_eq!(allocs, frees, "a start's stage set-up leaks nothing on a warmed thread");
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
