//! The latency tests (engine.md "Latency test", TODO #13 layers 2-3).
//!
//! The engine test: the test thread plays a train of exponential sweeps into the probe device, records the output
//! cable's capture endpoint (QPC-stamped), and receives the audio thread's own mic blocks through
//! a tap ring (each with its first sample's QPC). Deconvolution finds each sweep in both
//! recordings; the stamps turn sample positions into QPC times, so
//! app = output − tap, input path = tap − probe, end to end = output − probe.
//!
//! The cable-only test ([`cable_latency_test`]) plays the same train with no engine, on the calling thread: probe play-out to
//! the arrival in the capture, which is one cable's own latency (or any playback-to-recording path's). Both tests share the
//! play-and-record loop ([`play_train`]), the arrival timing ([`Train::arrivals`]), the matching and the statistics.
//!
//! The tap ring is handed to the audio thread lock-free (a one-slot rtrb ring of producers) and
//! handed back the same way, so the audio thread never frees it (handover.rs).

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nf_core::latency::analysis::find_sweep_arrivals;
use nf_core::latency::sweep::{Sweep, exp_sweep, probe_train};
use nf_core::telemetry::EngineState;
use nf_core::{BLOCK, SAMPLE_RATE};
use rtrb::{Consumer, Producer, RingBuffer};
use wasapi::{DeviceEnumerator, Direction};

use crate::api::{
    CableLatencyReport, CableLatencyRequest, EngineError, LatencyMode, LatencyProgress, LatencyReport, LatencyTest, LatencyTestRequest,
    StreamInfo,
};
use crate::audio::{AudioShared, RENDER_BUFFER_HNS};
use crate::control::{Inner, claim_test, lock};
use crate::devices::resolve;
use crate::errors::{StreamRole, describe_com_failure, describe_hr, describe_start_failure};
use crate::handover::{self, End, Port};
use crate::pipeline::{RenderSink, TapItem, play_time_hns};
use crate::play_record::{CANCELLED, Player, Recorder, Recording};
use crate::sched::{Com, Mmcss, MmcssPriority};
use crate::stream::{CaptureStream, RenderStream, hr_of};
use crate::timing::{HNS_PER_S, interpolate_stamps};

/// Sweep parameters (engine.md): 100 Hz-16 kHz, 1 s, 10 ms fades, -12 dBFS; 0.5 s gaps.
const SWEEP_F1: f32 = 100.0;
const SWEEP_F2: f32 = 16_000.0;
const SWEEP_S: f32 = 1.0;
const SWEEP_FADE_S: f32 = 0.01;
const SWEEP_AMPLITUDE: f32 = 0.25;
const GAP_S: f32 = 0.5;
/// Recording continues this long after the train has played.
const TAIL_S: f64 = 1.5;
/// Arrivals below this confidence don't count.
const MIN_CONFIDENCE_DB: f32 = 15.0;
/// Repetitions further than this from the median app (or cable) latency are outliers.
const OUTLIER_MS: f64 = 50.0;
/// An arrival may come this early (5 ms, in hns) and still belong to its probe start.
const SLACK_HNS: f64 = 5.0 * 1e4;
/// A spread above this earns a warning.
const SPREAD_WARN_MS: f64 = 2.0;
/// The probe's render fill: enough to ride out a late wake-up.
const PROBE_FILL: usize = 1_920;
/// Silence written before the train.
const PROBE_PRE: usize = 960;

/// State shared between the test thread and its [`LatencyTest`] handles.
pub(crate) struct TestShared {
    progress: Mutex<LatencyProgress>,
    cancel: AtomicBool,
}

impl TestShared {
    fn new() -> Self {
        Self { progress: Mutex::new(LatencyProgress::Running { fraction: 0.0, stage: "Starting" }), cancel: AtomicBool::new(false) }
    }

    pub(crate) fn progress(&self) -> LatencyProgress {
        lock(&self.progress).clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    fn set(&self, p: LatencyProgress) {
        *lock(&self.progress) = p;
    }

    fn running(&self, fraction: f32, stage: &'static str) {
        self.set(LatencyProgress::Running { fraction: fraction.clamp(0.0, 1.0), stage });
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }
}

/// The control side of the tap hand-over (handover.rs): producers go in on `cmd` and come back on `ret`.
pub(crate) type TapPort = Port<Producer<TapItem>>;

/// The audio thread's side of the tap hand-over.
pub(crate) type TapEnd = End<Producer<TapItem>>;

/// One run's tap hand-over rings (allocated on the control thread).
pub(crate) fn tap_channel() -> (TapPort, TapEnd) {
    handover::channel()
}

enum TestError {
    Cancelled,
    Failed(String),
}

fn failed(msg: impl Into<String>) -> TestError {
    TestError::Failed(msg.into())
}

pub(crate) fn start(inner: &Arc<Inner>, request: LatencyTestRequest) -> Result<LatencyTest, EngineError> {
    if inner.telemetry.state() != EngineState::Running {
        return Err(EngineError("Start the filter first: the latency test measures the running engine.".into()));
    }
    let mic_id = lock(&inner.status).mic_id.clone();
    validate_request(&request, mic_id.as_deref()).map_err(EngineError)?;
    if !claim_test(inner) {
        return Err(EngineError("A latency test is already running.".into()));
    }
    let shared = Arc::new(TestShared::new());
    let (inner2, shared2) = (inner.clone(), shared.clone());
    match std::thread::Builder::new().name("nf-latency".into()).spawn(move || run_test(inner2, request, shared2)) {
        Ok(_) => Ok(LatencyTest { inner: shared }),
        Err(_) => {
            inner.test_running.store(false, Relaxed);
            Err(EngineError("The latency test couldn't start a thread.".into()))
        }
    }
}

/// Request checks that need no device.
pub(crate) fn validate_request(req: &LatencyTestRequest, mic_id: Option<&str>) -> Result<(), String> {
    if req.probe_device.is_empty() {
        return Err("Pick the playback device the test sweeps are played on.".into());
    }
    if req.capture_device.is_empty() {
        return Err("Pick the recording device that carries the filter's output (e.g. \"CABLE Output\").".into());
    }
    check_repetitions(req.repetitions)?;
    if Some(req.capture_device.as_str()) == mic_id {
        return Err("The capture device can't be the filter's own microphone: pick the output cable's recording side.".into());
    }
    Ok(())
}

fn check_repetitions(repetitions: u32) -> Result<(), String> {
    if (1..=100).contains(&repetitions) { Ok(()) } else { Err("Choose between 1 and 100 repetitions.".into()) }
}

/// Clears the test flag when the thread ends.
struct TestFlag(Arc<Inner>);

impl Drop for TestFlag {
    fn drop(&mut self) {
        self.0.test_running.store(false, Relaxed);
    }
}

/// Puts the engine back when the test ends, however it ends: passthrough as it was, and the tap
/// returned (and freed) here.
struct Restore<'a> {
    inner: &'a Inner,
    prev_passthrough: bool,
    tap_sent: bool,
}

impl<'a> Restore<'a> {
    fn engage(inner: &'a Inner, passthrough: bool) -> Self {
        let prev_passthrough = inner.controls.passthrough.load(Relaxed);
        if passthrough {
            inner.controls.passthrough.store(true, Relaxed);
        }
        Self { inner, prev_passthrough, tap_sent: false }
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.inner.controls.passthrough.store(self.prev_passthrough, Relaxed);
        if self.tap_sent {
            take_tap_back(self.inner);
        }
    }
}

/// Hands a tap producer to the live run. Returns that run's atomics.
fn send_tap(inner: &Inner, producer: Producer<TapItem>) -> Result<Arc<AudioShared>, TestError> {
    let shared = lock(&inner.current).clone().ok_or_else(|| failed("The filter isn't running."))?;
    shared.tap_stop.store(false, Relaxed);
    let mut tap = lock(&inner.tap);
    let port = tap.as_mut().ok_or_else(|| failed("The filter isn't running."))?;
    // A tap left over from an earlier test: drop it here.
    while port.ret.pop().is_ok() {}
    port.cmd.push(producer).map_err(|_| failed("The audio thread didn't take the test tap."))?;
    Ok(shared)
}

/// Asks the audio thread to stop tapping and waits (≤ 500 ms) for the producer to come back.
fn take_tap_back(inner: &Inner) {
    if let Some(s) = lock(&inner.current).as_ref() {
        s.tap_stop.store(true, Relaxed);
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        {
            let mut tap = lock(&inner.tap);
            match tap.as_mut() {
                Some(port) => {
                    if port.ret.pop().is_ok() {
                        return;
                    }
                }
                // The run ended; its thread released the tap when it exited.
                None => return,
            }
        }
        if Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run_test(inner: Arc<Inner>, req: LatencyTestRequest, shared: Arc<TestShared>) {
    let _flag = TestFlag(inner.clone());
    let mut restore = Restore::engage(&inner, !req.through_model);
    let outcome = measure(&inner, &req, &shared, &mut restore);
    // Restore before reporting, so "done" means the engine is back to normal.
    drop(restore);
    shared.set(match outcome {
        Ok(report) => LatencyProgress::Done(report),
        Err(TestError::Cancelled) => LatencyProgress::Cancelled,
        Err(TestError::Failed(msg)) => {
            log::warn!("latency test failed: {msg}");
            LatencyProgress::Failed(msg)
        }
    });
}

/// The sweep train both tests play, and how long they record.
struct Train {
    sweep: Sweep,
    signal: Vec<f32>,
    /// Each sweep's start in `signal`.
    starts: Vec<usize>,
    /// Frames to play after the probe's lead-in: the train, then the tail.
    total: usize,
}

impl Train {
    fn new(repetitions: usize) -> Self {
        let sweep = exp_sweep(SAMPLE_RATE, SWEEP_F1, SWEEP_F2, SWEEP_S, SWEEP_FADE_S, SWEEP_AMPLITUDE);
        let (signal, starts) = probe_train(&sweep, repetitions, GAP_S);
        let total = signal.len() + (TAIL_S * SAMPLE_RATE as f64) as usize;
        Self { sweep, signal, starts, total }
    }

    /// One repetition (gap, sweep, gap) in hns.
    fn period_hns(&self) -> f64 {
        let fs = SAMPLE_RATE as f64;
        (self.sweep.signal.len() as f64 + 2.0 * GAP_S as f64 * fs) / fs * HNS_PER_S
    }

    /// Each sweep's arrival in `rec`, mapped to QPC time by the recording's stamps.
    fn arrivals(&self, rec: &Recording) -> Vec<TimedArrival> {
        let fs = SAMPLE_RATE as f64;
        find_sweep_arrivals(&rec.samples, &self.sweep, self.starts.len(), self.sweep.signal.len())
            .iter()
            .filter_map(|a| {
                interpolate_stamps(&rec.stamps, a.position, fs).map(|t_hns| TimedArrival { t_hns, confidence_db: a.confidence_db })
            })
            .collect()
    }
}

/// What one play-out of the train gave: the capture, and each sweep start's play-out time where the probe's clock gave one.
struct Take {
    out: Recording,
    probe_t: Vec<Option<f64>>,
}

/// The probe's render stream and the capture stream.
fn open_streams(probe_id: &str, capture_id: &str) -> Result<(RenderStream, CaptureStream), TestError> {
    let en = DeviceEnumerator::new().map_err(|e| failed(describe_hr(StreamRole::Probe, hr_of(&e))))?;
    let probe_dev = resolve(&en, Some(probe_id), Direction::Render, StreamRole::Probe).map_err(|f| failed(describe_start_failure(&f)))?;
    let cap_dev =
        resolve(&en, Some(capture_id), Direction::Capture, StreamRole::Capture).map_err(|f| failed(describe_start_failure(&f)))?;
    let probe = RenderStream::open(&probe_dev, RENDER_BUFFER_HNS).map_err(|e| failed(describe_hr(StreamRole::Probe, hr_of(&e))))?;
    let cap = CaptureStream::open_events(&cap_dev).map_err(|e| failed(describe_hr(StreamRole::Capture, hr_of(&e))))?;
    Ok((probe, cap))
}

/// Plays the train on `probe` (after [`PROBE_PRE`] frames of silence) and records `cap` (QPC-stamped, gaps by device position
/// zero-filled) until the probe has played the train and the tail. `each_wake(fraction played)` runs at the top of every wake:
/// it checks for cancellation, drains whatever else is recording and publishes progress, and its error ends the take. Both
/// streams are stopped however it ends.
fn play_train(
    probe: &mut RenderStream, cap: &mut CaptureStream, train: &Train, mut each_wake: impl FnMut(f64) -> Result<(), TestError>,
) -> Result<Take, TestError> {
    let fs = SAMPLE_RATE as f64;
    let mut player = Player::new(&train.signal, PROBE_PRE);
    let mut rec = Recorder::new(cap.max_packet_frames(), train.total + 2 * SAMPLE_RATE as usize);
    let mut probe_t: Vec<Option<f64>> = vec![None; train.starts.len()];
    let probe_err = |hr| failed(describe_hr(StreamRole::Probe, hr));
    let cap_err = |hr| failed(describe_hr(StreamRole::Capture, hr));
    let mut take = || -> Result<(), TestError> {
        player.top_up(probe, PROBE_PRE).map_err(probe_err)?;
        cap.start().map_err(cap_err)?;
        probe.start().map_err(probe_err)?;
        let deadline = Instant::now() + Duration::from_secs_f64(train.total as f64 / fs + 10.0);
        let mut fraction = 0.0;
        loop {
            each_wake(fraction)?;
            if Instant::now() > deadline {
                return Err(failed("The probe device stopped playing; check that it's connected and not in use."));
            }
            cap.wait(10);

            // Keep the probe topped up: the train, then silence.
            player.top_up(probe, PROBE_FILL).map_err(probe_err)?;
            // Each sweep start's play-out time, re-estimated until it has played.
            let clock = probe.clock().map_err(probe_err)?;
            if let Some(c) = clock {
                for (t, &s) in probe_t.iter_mut().zip(&train.starts) {
                    let idx = (PROBE_PRE + s) as u64;
                    if idx < player.written() && c.pos_frames <= idx as f64 {
                        *t = Some(play_time_hns(c, idx, fs));
                    }
                }
            }
            let played = player.played(clock) - PROBE_PRE as f64;

            // The capture, gaps (by device position) zero-filled.
            rec.read(cap).map_err(cap_err)?;
            fraction = played / train.total as f64;
            if played >= train.total as f64 {
                return Ok(());
            }
        }
    };
    let result = take();
    probe.stop();
    cap.stop();
    result.map(|()| Take { out: rec.rec, probe_t })
}

fn measure(inner: &Inner, req: &LatencyTestRequest, shared: &TestShared, restore: &mut Restore<'_>) -> Result<LatencyReport, TestError> {
    let _com = Com::mta().map_err(|c| failed(describe_com_failure(c)))?;
    // Real-time priority only while streaming: dropped before the analysis (RT-2).
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    shared.running(0.0, "Preparing");
    let train = Train::new(req.repetitions as usize);
    let (mut probe, mut cap) = open_streams(&req.probe_device, &req.capture_device)?;

    // The tap ring covers the whole test: train + tail + 1 s. Touch every page now so the audio
    // thread never takes a page fault on it.
    let capacity = (train.total + SAMPLE_RATE as usize) / BLOCK + 1;
    let (mut tap_tx, mut tap_rx) = RingBuffer::<TapItem>::new(capacity);
    let blank = TapItem { qpc_hns: 0.0, samples: [0.0; BLOCK] };
    while tap_tx.push(blank).is_ok() {}
    while tap_rx.pop().is_ok() {}
    let run = send_tap(inner, tap_tx)?;
    restore.tap_sent = true;
    let mut tap = Recording { samples: Vec::with_capacity(capacity * BLOCK), stamps: Vec::with_capacity(capacity) };

    // The readout's budget over the test: its totals now and after the sweeps (published each second).
    let budget_before = inner.telemetry.snapshot().budget_totals;
    let take = play_train(&mut probe, &mut cap, &train, |fraction| {
        if shared.cancelled() {
            return Err(TestError::Cancelled);
        }
        let still_same_run = lock(&inner.current).as_ref().is_some_and(|s| Arc::ptr_eq(s, &run));
        if inner.telemetry.state() != EngineState::Running || !still_same_run {
            return Err(failed("The filter stopped or restarted during the test; run it again."));
        }
        drain_tap(&mut tap_rx, &mut tap);
        shared.running(0.9 * fraction as f32, "Measuring");
        Ok(())
    })?;
    drain_tap(&mut tap_rx, &mut tap);
    let budget = inner.telemetry.snapshot().budget_totals.means_since(&budget_before);
    // The analysis is seconds of CPU for a long test (FFTs of 10 M points and more): at MMCSS
    // "Pro Audio" priority (or the TIME_CRITICAL fallback) it would starve other apps' audio and
    // the UI. Back to normal priority first; nothing below needs it, nor COM's real-time state.
    drop(mmcss);
    if run.tap_overflows.load(Relaxed) > 0 {
        log::warn!("latency test: the tap ring overflowed");
    }

    shared.running(0.92, "Analysing");
    let tap_t = train.arrivals(&tap);
    let out_t = train.arrivals(&take.out);
    let mut report = build_report(req.repetitions, &take.probe_t, &tap_t, &out_t, train.period_hns());
    if report.valid < req.repetitions {
        // Which side lost the sweeps, and how loud each recording was: tells a silent or muted device
        // from a garbled one.
        report.warnings.push(found_line(req.repetitions, tap_t.len(), &tap.samples, out_t.len(), &take.out.samples));
    }

    let tel = inner.telemetry.snapshot();
    report.readout_ms_median = tel.latency_valid.then_some(tel.latency_ms_median);
    report.budget = budget;
    report.chain_ms = run.chain_latency.load(Relaxed) as f32 * 1000.0 / SAMPLE_RATE as f32;
    let st = lock(&inner.status).clone();
    report.conditions =
        conditions(st.mic.as_ref(), st.output.as_ref(), &probe.info.stream_info(), &cap.info.stream_info(), req.mode, !req.through_model);
    report.warnings.extend(mode_warnings(req, inner.controls.aec.enabled.load(Relaxed)));
    shared.running(1.0, "Done");
    Ok(report)
}

/// Request checks for the cable-only test that need no device.
pub(crate) fn validate_cable_request(req: &CableLatencyRequest<'_>) -> Result<(), String> {
    if req.probe_device.is_empty() {
        return Err("Pick the playback device the test sweeps are played on.".into());
    }
    if req.capture_device.is_empty() {
        return Err("Pick the recording device the sweeps arrive on: the probe cable's recording side (e.g. \"CABLE-A Output\").".into());
    }
    check_repetitions(req.repetitions)
}

pub(crate) fn cable_latency_test(req: &CableLatencyRequest<'_>) -> Result<CableLatencyReport, EngineError> {
    validate_cable_request(req).map_err(EngineError)?;
    measure_cable(req).map_err(|e| match e {
        TestError::Cancelled => EngineError(CANCELLED.into()),
        TestError::Failed(msg) => EngineError(msg),
    })
}

fn measure_cable(req: &CableLatencyRequest<'_>) -> Result<CableLatencyReport, TestError> {
    let _com = Com::mta().map_err(|c| failed(describe_com_failure(c)))?;
    // Real-time priority only while streaming, as in the engine test (RT-2).
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    let train = Train::new(req.repetitions as usize);
    let (mut probe, mut cap) = open_streams(&req.probe_device, &req.capture_device)?;
    let cancelled = || req.cancel.is_some_and(|c| c.load(Relaxed));
    let take = play_train(&mut probe, &mut cap, &train, |_| if cancelled() { Err(TestError::Cancelled) } else { Ok(()) })?;
    drop(mmcss);

    let out_t = train.arrivals(&take.out);
    let mut report = build_cable_report(req.repetitions, &take.probe_t, &out_t, train.period_hns());
    if report.valid < req.repetitions {
        report.warnings.push(format!(
            "Sweeps found: {} of {} in the capture (peak {}).",
            out_t.len(),
            req.repetitions,
            peak_text(&take.out.samples)
        ));
    }
    report.conditions = cable_conditions(&probe.info.stream_info(), &cap.info.stream_info());
    Ok(report)
}

/// "Sweeps found: 5 of 5 at the app's input (peak -12.0 dBFS), 0 of 5 in the output capture (peak -inf dBFS)."
fn found_line(reps: u32, tap_found: usize, tap: &[f32], out_found: usize, out: &[f32]) -> String {
    format!(
        "Sweeps found: {tap_found} of {reps} at the app's input (peak {}), {out_found} of {reps} in the output capture (peak {}).",
        peak_text(tap),
        peak_text(out)
    )
}

/// A recording's peak: "-12.0 dBFS", or "silent".
fn peak_text(x: &[f32]) -> String {
    let p = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    if p > 0.0 { format!("{:.1} dBFS", 20.0 * p.log10()) } else { "silent".to_owned() }
}

fn drain_tap(rx: &mut Consumer<TapItem>, rec: &mut Recording) {
    while let Ok(item) = rx.pop() {
        if item.qpc_hns.is_finite() {
            rec.stamps.push((rec.samples.len() as u64, item.qpc_hns));
        }
        rec.samples.extend_from_slice(&item.samples);
    }
}

/// An arrival mapped to QPC time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TimedArrival {
    pub(crate) t_hns: f64,
    pub(crate) confidence_db: f32,
}

/// The median (the mean of the middle two for an even count); `None` when empty.
pub(crate) fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    Some(if s.len() % 2 == 1 { s[m] } else { (s[m - 1] + s[m]) / 2.0 })
}

/// Matches arrivals to repetitions and fills the report's statistics and their warnings.
///
/// Repetition `i`'s tap arrival is the first within [probe start − 5 ms, + half a period) of its
/// probe start (or the `i`th arrival when the probe clock gave no start time); its output arrival
/// is the first within [tap − 5 ms, + half a period) of that. Both need `confidence_db ≥ 15`; then
/// repetitions more than 50 ms from the median app latency are dropped.
pub(crate) fn build_report(
    repetitions: u32, probe: &[Option<f64>], tap: &[TimedArrival], out: &[TimedArrival], period_hns: f64,
) -> LatencyReport {
    struct Rep {
        app: f64,
        input: Option<f64>,
        e2e: Option<f64>,
        confidence: f32,
    }
    let half = period_hns / 2.0;
    let ms = |hns: f64| hns * 1e-4;
    let mut cands = Vec::new();
    for i in 0..repetitions as usize {
        let tp = probe.get(i).copied().flatten();
        let tap_i = match tp {
            Some(tp) => first_within(tap, tp, half),
            None => tap.get(i),
        };
        let Some(ta) = tap_i else { continue };
        let Some(oa) = first_within(out, ta.t_hns, half) else {
            continue;
        };
        if ta.confidence_db < MIN_CONFIDENCE_DB || oa.confidence_db < MIN_CONFIDENCE_DB {
            continue;
        }
        cands.push(Rep {
            app: ms(oa.t_hns - ta.t_hns),
            input: tp.map(|tp| ms(ta.t_hns - tp)),
            e2e: tp.map(|tp| ms(oa.t_hns - tp)),
            confidence: ta.confidence_db.min(oa.confidence_db),
        });
    }
    let kept = near_median(cands, |r| r.app);

    let app_ms: Vec<f64> = kept.iter().map(|r| r.app).collect();
    let inputs: Vec<f64> = kept.iter().filter_map(|r| r.input).collect();
    let e2e: Vec<f64> = kept.iter().filter_map(|r| r.e2e).collect();
    let spread = spread(&app_ms);
    let valid = kept.len() as u32;
    let mut warnings = Vec::new();
    if valid == 0 {
        warnings.push(
            "No sweep was found in both recordings. Check that the mic is the probe cable's output, that the capture device is the output cable's recording side, and that neither is muted.".to_owned(),
        );
    } else if valid < repetitions {
        warnings.push(format!("Only {valid} of {repetitions} sweeps were found clearly in both recordings; the result is less certain."));
    }
    if spread > SPREAD_WARN_MS {
        warnings.push(format!("The app latency varied by {spread:.1} ms between sweeps; repeat the test with less running on the PC."));
    }
    LatencyReport {
        repetitions,
        valid,
        app_ms_median: median(&app_ms).unwrap_or(f64::NAN),
        app_ms,
        app_ms_spread: spread,
        input_path_ms_median: median(&inputs),
        end_to_end_ms_median: median(&e2e),
        readout_ms_median: None,
        budget: None,
        chain_ms: 0.0,
        confidence_db_min: kept.iter().map(|r| r.confidence).fold(f32::NAN, f32::min),
        conditions: String::new(),
        warnings,
    }
}

/// The cable-only test's statistics. Repetition `i`'s arrival is the first within [probe start − 5 ms, + half a period) of its
/// probe start, with `confidence_db ≥ 15`; then repetitions more than 50 ms from the median are dropped. A repetition without a
/// probe start time (the clock not running yet) has nothing to be timed against.
pub(crate) fn build_cable_report(repetitions: u32, probe: &[Option<f64>], out: &[TimedArrival], period_hns: f64) -> CableLatencyReport {
    let half = period_hns / 2.0;
    let mut cands: Vec<(f64, f32)> = Vec::new();
    for i in 0..repetitions as usize {
        let Some(tp) = probe.get(i).copied().flatten() else { continue };
        let Some(a) = first_within(out, tp, half) else { continue };
        if a.confidence_db >= MIN_CONFIDENCE_DB {
            cands.push(((a.t_hns - tp) * 1e-4, a.confidence_db));
        }
    }
    let kept = near_median(cands, |c| c.0);
    let cable_ms: Vec<f64> = kept.iter().map(|c| c.0).collect();
    let spread = spread(&cable_ms);
    let valid = kept.len() as u32;
    let mut warnings = Vec::new();
    if probe.iter().take(repetitions as usize).all(Option::is_none) {
        warnings.push(
            "The probe device's clock gave no play-out times, so no sweep could be timed. Check that it plays (not muted or disabled)."
                .to_owned(),
        );
    } else if valid == 0 {
        warnings.push(
            "No sweep was found in the capture. Check that the capture device is the probe cable's recording side, and that neither \
             is muted."
                .to_owned(),
        );
    } else if valid < repetitions {
        warnings.push(format!("Only {valid} of {repetitions} sweeps were found clearly in the capture; the result is less certain."));
    }
    if spread > SPREAD_WARN_MS {
        warnings.push(format!("The cable latency varied by {spread:.1} ms between sweeps; repeat the test with less running on the PC."));
    }
    CableLatencyReport {
        repetitions,
        valid,
        cable_ms_median: median(&cable_ms).unwrap_or(f64::NAN),
        cable_ms,
        cable_ms_spread: spread,
        confidence_db_min: kept.iter().map(|c| c.1).fold(f32::NAN, f32::min),
        conditions: String::new(),
        warnings,
    }
}

/// The first arrival in [t − 5 ms, t + half): the direct path, not a later reflection nor the next repetition.
fn first_within(arrivals: &[TimedArrival], t: f64, half: f64) -> Option<&TimedArrival> {
    arrivals.iter().find(|a| a.t_hns >= t - SLACK_HNS && a.t_hns < t + half)
}

/// The candidates whose `key` is within [`OUTLIER_MS`] of the candidates' median.
fn near_median<T>(cands: Vec<T>, key: impl Fn(&T) -> f64) -> Vec<T> {
    let all: Vec<f64> = cands.iter().map(&key).collect();
    match median(&all) {
        Some(m) => cands.into_iter().filter(|c| (key(c) - m).abs() <= OUTLIER_MS).collect(),
        None => Vec::new(),
    }
}

/// Max minus min; 0 when empty.
fn spread(v: &[f64]) -> f64 {
    if v.is_empty() { 0.0 } else { v.iter().copied().fold(f64::MIN, f64::max) - v.iter().copied().fold(f64::MAX, f64::min) }
}

/// Warnings that depend on the mode, not the result.
pub(crate) fn mode_warnings(req: &LatencyTestRequest, aec_enabled: bool) -> Vec<String> {
    let mut w = Vec::new();
    if req.mode == LatencyMode::Acoustic {
        if aec_enabled {
            w.push("Echo cancellation was bypassed during the test (passthrough keeps its delay).".to_owned());
        }
        w.push("Acoustic results need a quiet room: background noise shows up as missed or uncertain sweeps.".to_owned());
    }
    if req.through_model {
        w.push(
            "Measuring through the noise filter needs a speech probe (not in this version): the filter may have removed the sweeps."
                .to_owned(),
        );
    }
    w
}

/// The record of how the test ran: engine periods, RAW, autoconvert and the devices.
pub(crate) fn conditions(
    mic: Option<&StreamInfo>, output: Option<&StreamInfo>, probe: &StreamInfo, capture: &StreamInfo, mode: LatencyMode, passthrough: bool,
) -> String {
    let mut parts = vec![format!(
        "{} ({})",
        match mode {
            LatencyMode::VirtualLoop => "virtual loop",
            LatencyMode::Acoustic => "acoustic",
        },
        if passthrough { "passthrough" } else { "through the model" }
    )];
    if let Some(m) = mic {
        parts.push(describe_stream("mic", m));
    }
    if let Some(o) = output {
        parts.push(describe_stream("output", o));
    }
    parts.push(describe_stream("probe", probe));
    parts.push(describe_stream("capture", capture));
    parts.join("; ")
}

/// The cable-only test's record: both streams.
pub(crate) fn cable_conditions(probe: &StreamInfo, capture: &StreamInfo) -> String {
    format!("cable only; {}; {}", describe_stream("probe", probe), describe_stream("capture", capture))
}

fn describe_stream(label: &str, s: &StreamInfo) -> String {
    let mut flags = String::new();
    if s.low_latency {
        flags.push_str(", IAudioClient3");
    }
    if s.raw {
        flags.push_str(", RAW");
    }
    if s.autoconvert {
        flags.push_str(", autoconvert");
    }
    format!("{label}: {} ({} Hz, {} ch, {:.2} ms period{flags})", s.device_name, s.sample_rate, s.channels, s.period_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(t_ms: f64, conf: f32) -> TimedArrival {
        TimedArrival { t_hns: t_ms * 1e4, confidence_db: conf }
    }

    const PERIOD_HNS: f64 = 2.0 * HNS_PER_S;

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn report_from_clean_arrivals() {
        // Probes every 2 s; input path 12 ms; app 31, 31.5, 30.5 ms.
        let probe: Vec<Option<f64>> = (0..3).map(|i| Some((1000.0 + 2000.0 * i as f64) * 1e4)).collect();
        let tap: Vec<_> = (0..3).map(|i| arr(1012.0 + 2000.0 * i as f64, 40.0)).collect();
        let app = [31.0, 31.5, 30.5];
        let out: Vec<_> = (0..3).map(|i| arr(1012.0 + 2000.0 * i as f64 + app[i], 30.0 + i as f32)).collect();
        let r = build_report(3, &probe, &tap, &out, PERIOD_HNS);
        assert_eq!((r.repetitions, r.valid), (3, 3));
        for (a, b) in r.app_ms.iter().zip(app) {
            assert!((a - b).abs() < 1e-9);
        }
        assert!((r.app_ms_median - 31.0).abs() < 1e-9);
        assert!((r.app_ms_spread - 1.0).abs() < 1e-9);
        assert!((r.input_path_ms_median.unwrap() - 12.0).abs() < 1e-9);
        assert!((r.end_to_end_ms_median.unwrap() - 43.0).abs() < 1e-9);
        assert_eq!(r.confidence_db_min, 30.0);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn weak_and_outlying_repetitions_are_dropped() {
        let probe: Vec<Option<f64>> = (0..5).map(|i| Some(2000.0 * i as f64 * 1e4)).collect();
        let tap: Vec<_> = (0..5).map(|i| arr(2000.0 * i as f64 + 10.0, if i == 1 { 9.0 } else { 30.0 })).collect();
        // Repetition 3's output arrival is a reflection 120 ms late.
        let app = [20.0, 20.0, 21.0, 140.0, 22.5];
        let out: Vec<_> = (0..5).map(|i| arr(2000.0 * i as f64 + 10.0 + app[i], 25.0)).collect();
        let r = build_report(5, &probe, &tap, &out, PERIOD_HNS);
        assert_eq!(r.valid, 3, "rep 1 is weak, rep 3 an outlier");
        assert_eq!(r.app_ms, vec![20.0, 21.0, 22.5]);
        assert!((r.app_ms_spread - 2.5).abs() < 1e-9);
        assert!(r.warnings.iter().any(|w| w.starts_with("Only 3 of 5")), "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("varied by 2.5 ms")), "{:?}", r.warnings);
    }

    #[test]
    fn missing_arrivals_are_matched_by_time_not_order() {
        let probe: Vec<Option<f64>> = (0..3).map(|i| Some(2000.0 * i as f64 * 1e4)).collect();
        // The tap missed sweep 0; the output missed sweep 2.
        let tap = vec![arr(2010.0, 30.0), arr(4010.0, 30.0)];
        let out = vec![arr(30.0, 30.0), arr(2035.0, 30.0)];
        let r = build_report(3, &probe, &tap, &out, PERIOD_HNS);
        assert_eq!(r.valid, 1);
        assert_eq!(r.app_ms, vec![25.0]);
        assert!((r.input_path_ms_median.unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn without_probe_times_order_is_used_and_paths_are_unknown() {
        let probe = vec![None, None];
        let tap = vec![arr(10.0, 30.0), arr(2010.0, 30.0)];
        let out = vec![arr(40.0, 30.0), arr(2041.0, 30.0)];
        let r = build_report(2, &probe, &tap, &out, PERIOD_HNS);
        assert_eq!(r.valid, 2);
        assert!((r.app_ms_median - 30.5).abs() < 1e-9);
        assert_eq!(r.input_path_ms_median, None);
        assert_eq!(r.end_to_end_ms_median, None);
    }

    /// Which side lost the sweeps, in the report's words.
    #[test]
    fn the_found_line_names_each_side() {
        let line = found_line(5, 5, &[0.0, 0.25, -0.5], 0, &[0.0; 4]);
        assert_eq!(line, "Sweeps found: 5 of 5 at the app's input (peak -6.0 dBFS), 0 of 5 in the output capture (peak silent).");
    }

    #[test]
    fn nothing_found() {
        let r = build_report(5, &[None; 5], &[], &[], PERIOD_HNS);
        assert_eq!(r.valid, 0);
        assert!(r.app_ms_median.is_nan() && r.confidence_db_min.is_nan());
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].starts_with("No sweep was found"));
    }

    #[test]
    fn cable_report_from_clean_arrivals() {
        // Probes every 2 s; the cable takes 55, 56.5 and 55.5 ms.
        let probe: Vec<Option<f64>> = (0..3).map(|i| Some((1000.0 + 2000.0 * i as f64) * 1e4)).collect();
        let cable = [55.0, 56.5, 55.5];
        let out: Vec<_> = (0..3).map(|i| arr(1000.0 + 2000.0 * i as f64 + cable[i], 40.0 - i as f32)).collect();
        let r = build_cable_report(3, &probe, &out, PERIOD_HNS);
        assert_eq!((r.repetitions, r.valid), (3, 3));
        for (a, b) in r.cable_ms.iter().zip(cable) {
            assert!((a - b).abs() < 1e-9, "{:?}", r.cable_ms);
        }
        assert!((r.cable_ms_median - 55.5).abs() < 1e-9);
        assert!((r.cable_ms_spread - 1.5).abs() < 1e-9);
        assert_eq!(r.confidence_db_min, 38.0);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn cable_report_drops_weak_late_early_and_untimed_repetitions() {
        let mut probe: Vec<Option<f64>> = (0..6).map(|i| Some(2000.0 * i as f64 * 1e4)).collect();
        // Repetition 4's probe time is unknown, so its arrival can't be timed.
        probe[4] = None;
        let cable = [60.0, 60.0, 61.0, 190.0, 60.0, 63.0];
        let conf = [30.0, 9.0, 30.0, 30.0, 30.0, 30.0];
        let mut out: Vec<_> = (0..6).map(|i| arr(2000.0 * i as f64 + cable[i], conf[i])).collect();
        // An arrival 6 ms early belongs to no probe.
        out[5] = arr(2000.0 * 5.0 - 6.0, 30.0);
        let r = build_cable_report(6, &probe, &out, PERIOD_HNS);
        // 1 is weak, 3 an outlier (190 ms against a median of 61), 4 untimed, 5 early.
        assert_eq!(r.cable_ms, vec![60.0, 61.0]);
        assert_eq!(r.valid, 2);
        assert!(r.warnings.iter().any(|w| w.starts_with("Only 2 of 6")), "{:?}", r.warnings);
        assert!(!r.warnings.iter().any(|w| w.contains("varied")), "a 1 ms spread is fine: {:?}", r.warnings);
        let r = build_cable_report(2, &probe, &[arr(60.0, 30.0), arr(2075.0, 30.0)], PERIOD_HNS);
        assert!(r.warnings.iter().any(|w| w.contains("varied by 15.0 ms")), "{:?}", r.warnings);
    }

    #[test]
    fn cable_report_says_why_nothing_was_timed() {
        let r = build_cable_report(3, &[None; 3], &[arr(50.0, 40.0)], PERIOD_HNS);
        assert_eq!(r.valid, 0);
        assert!(r.cable_ms_median.is_nan() && r.confidence_db_min.is_nan());
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("clock gave no play-out times"), "{:?}", r.warnings);
        let r = build_cable_report(3, &[Some(0.0); 3], &[], PERIOD_HNS);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].starts_with("No sweep was found in the capture."), "{:?}", r.warnings);
    }

    #[test]
    fn cable_requests_and_conditions() {
        let ok = CableLatencyRequest { probe_device: "cableA".into(), capture_device: "cableA-out".into(), ..Default::default() };
        assert_eq!(ok.repetitions, 5);
        assert!(validate_cable_request(&ok).is_ok());
        assert!(validate_cable_request(&CableLatencyRequest { probe_device: String::new(), ..ok.clone() }).is_err());
        assert!(validate_cable_request(&CableLatencyRequest { capture_device: String::new(), ..ok.clone() }).is_err());
        assert!(validate_cable_request(&CableLatencyRequest { repetitions: 101, ..ok.clone() }).is_err());
        // Checked before any device is touched
        let err = cable_latency_test(&CableLatencyRequest { repetitions: 0, ..ok.clone() }).unwrap_err();
        assert_eq!(err.0, "Choose between 1 and 100 repetitions.");

        let s = |name: &str| StreamInfo {
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms: 10.0,
            low_latency: false,
            autoconvert: false,
            raw: false,
        };
        assert_eq!(
            cable_conditions(&s("CABLE-A Input"), &s("CABLE-A Output")),
            "cable only; probe: CABLE-A Input (48000 Hz, 2 ch, 10.00 ms period); capture: CABLE-A Output (48000 Hz, 2 ch, 10.00 ms period)"
        );
    }

    /// Both tests' train: 2 s a repetition and a 1.5 s tail, and its own signal, stamped from QPC 0, puts each sweep at its
    /// start's time.
    #[test]
    fn the_train_times_its_own_sweeps() {
        let t = Train::new(3);
        assert_eq!(t.starts.len(), 3);
        assert!((t.period_hns() - 2.0 * HNS_PER_S).abs() < 1.0);
        assert_eq!(t.signal.len(), 3 * 96_000);
        assert_eq!(t.total, t.signal.len() + 72_000);
        let rec = Recording { samples: t.signal.clone(), stamps: vec![(0, 0.0)] };
        let a = t.arrivals(&rec);
        assert_eq!(a.len(), 3);
        for (a, &s) in a.iter().zip(&t.starts) {
            assert!((a.t_hns - s as f64 * HNS_PER_S / 48_000.0).abs() < 10.0, "{a:?} vs {s}");
            assert!(a.confidence_db > 40.0, "{a:?}");
        }
    }

    #[test]
    fn request_rules() {
        let ok = LatencyTestRequest { probe_device: "cableA".into(), capture_device: "cableB-out".into(), ..Default::default() };
        assert!(validate_request(&ok, Some("cableA-out")).is_ok());
        assert!(validate_request(&LatencyTestRequest { probe_device: String::new(), ..ok.clone() }, None).is_err());
        assert!(validate_request(&LatencyTestRequest { capture_device: String::new(), ..ok.clone() }, None).is_err());
        assert!(validate_request(&LatencyTestRequest { repetitions: 0, ..ok.clone() }, None).is_err());
        let own_mic = validate_request(&ok, Some("cableB-out")).unwrap_err();
        assert!(own_mic.contains("own microphone"), "{own_mic}");
    }

    #[test]
    fn mode_warnings_and_conditions() {
        let acoustic = LatencyTestRequest { mode: LatencyMode::Acoustic, ..Default::default() };
        let w = mode_warnings(&acoustic, true);
        assert_eq!(w.len(), 2);
        assert!(w[0].contains("bypassed") && w[1].contains("quiet room"));
        assert!(mode_warnings(&LatencyTestRequest::default(), true).is_empty());
        assert_eq!(mode_warnings(&LatencyTestRequest { through_model: true, ..Default::default() }, false).len(), 1);

        let s = |name: &str, ll, raw, ac| StreamInfo {
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms: 10.0,
            low_latency: ll,
            autoconvert: ac,
            raw,
        };
        let c = conditions(
            Some(&s("CABLE-A Output", true, true, false)),
            Some(&s("CABLE-B Input", false, false, true)),
            &s("CABLE-A Input", false, false, false),
            &s("CABLE-B Output", false, false, false),
            LatencyMode::VirtualLoop,
            true,
        );
        assert_eq!(
            c,
            "virtual loop (passthrough); mic: CABLE-A Output (48000 Hz, 2 ch, 10.00 ms period, IAudioClient3, RAW); \
             output: CABLE-B Input (48000 Hz, 2 ch, 10.00 ms period, autoconvert); \
             probe: CABLE-A Input (48000 Hz, 2 ch, 10.00 ms period); capture: CABLE-B Output (48000 Hz, 2 ch, 10.00 ms period)"
        );
    }

    #[test]
    fn progress_and_cancel() {
        let s = TestShared::new();
        assert!(matches!(s.progress(), LatencyProgress::Running { fraction, .. } if fraction == 0.0));
        s.running(1.5, "Measuring");
        assert_eq!(s.progress(), LatencyProgress::Running { fraction: 1.0, stage: "Measuring" });
        assert!(!s.cancelled());
        s.cancel();
        assert!(s.cancelled());
    }

    #[test]
    fn tap_hand_over_round_trip() {
        let (mut port, mut end) = tap_channel();
        let (tx, _rx) = RingBuffer::<TapItem>::new(4);
        assert!(port.cmd.push(tx).is_ok());
        let p = end.cmd.pop().expect("the audio side receives it");
        assert!(end.ret.push(p).is_ok());
        assert!(port.ret.pop().is_ok(), "and the control side gets it back");
    }

    #[test]
    fn test_needs_a_running_engine() {
        let engine = crate::Engine::new();
        let req = LatencyTestRequest { probe_device: "a".into(), capture_device: "b".into(), ..Default::default() };
        let err = engine.start_latency_test(req).err().unwrap();
        assert!(err.0.starts_with("Start the filter first"));
    }
}
