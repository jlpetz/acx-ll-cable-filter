//! The latency test (engine.md "Latency test", TODO #13 layers 2-3).
//!
//! The test thread plays a train of exponential sweeps into the probe device, records the output
//! cable's capture endpoint (QPC-stamped), and receives the audio thread's own mic blocks through
//! a tap ring (each with its first sample's QPC). Deconvolution finds each sweep in both
//! recordings; the stamps turn sample positions into QPC times, so
//! app = output − tap, input path = tap − probe, end to end = output − probe.
//!
//! The tap ring is handed to the audio thread lock-free (a one-slot rtrb ring of producers) and
//! handed back the same way, so the audio thread never frees it.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nf_core::latency::analysis::{Arrival, find_sweep_arrivals};
use nf_core::latency::sweep::{exp_sweep, probe_train};
use nf_core::telemetry::EngineState;
use nf_core::{BLOCK, SAMPLE_RATE};
use rtrb::{Consumer, Producer, RingBuffer};
use wasapi::{DeviceEnumerator, Direction};

use crate::api::{EngineError, LatencyMode, LatencyProgress, LatencyReport, LatencyTest, LatencyTestRequest, StreamInfo};
use crate::audio::{AudioShared, RENDER_BUFFER_HNS};
use crate::control::{Inner, claim_test, lock};
use crate::devices::resolve;
use crate::errors::{StreamRole, describe_hr, describe_start_failure};
use crate::pipeline::{CaptureSource, RenderSink, TapItem, play_time_hns};
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
/// Repetitions further than this from the median app latency are outliers.
const OUTLIER_MS: f64 = 50.0;
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

/// The control side of the tap hand-over: producers go in on `cmd` and come back on `ret`.
pub(crate) struct TapPort {
    cmd: Producer<Producer<TapItem>>,
    ret: Consumer<Producer<TapItem>>,
}

#[cfg(test)]
impl TapPort {
    pub(crate) fn cmd_for_test(&mut self) -> &mut Producer<Producer<TapItem>> {
        &mut self.cmd
    }
    pub(crate) fn ret_for_test(&mut self) -> &mut Consumer<Producer<TapItem>> {
        &mut self.ret
    }
}

/// The audio thread's side of the tap hand-over.
pub(crate) struct TapEnd {
    pub(crate) cmd: Consumer<Producer<TapItem>>,
    pub(crate) ret: Producer<Producer<TapItem>>,
}

/// One run's hand-over rings (allocated on the control thread).
pub(crate) fn tap_channel() -> (TapPort, TapEnd) {
    let (cmd_tx, cmd_rx) = RingBuffer::new(1);
    let (ret_tx, ret_rx) = RingBuffer::new(1);
    (TapPort { cmd: cmd_tx, ret: ret_rx }, TapEnd { cmd: cmd_rx, ret: ret_tx })
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
    if req.repetitions == 0 || req.repetitions > 100 {
        return Err("Choose between 1 and 100 repetitions.".into());
    }
    if Some(req.capture_device.as_str()) == mic_id {
        return Err("The capture device can't be the filter's own microphone: pick the output cable's recording side.".into());
    }
    Ok(())
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

/// One recording with its (sample index → QPC hns) stamps.
#[derive(Default)]
struct Recording {
    samples: Vec<f32>,
    stamps: Vec<(u64, f64)>,
}

fn measure(inner: &Inner, req: &LatencyTestRequest, shared: &TestShared, restore: &mut Restore<'_>) -> Result<LatencyReport, TestError> {
    let _com = Com::mta().map_err(|c| failed(format!("Windows audio couldn't be initialised (error 0x{:08X}).", c as u32)))?;
    // Real-time priority only while streaming: dropped before the analysis (RT-2).
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    shared.running(0.0, "Preparing");
    let fs = SAMPLE_RATE as f64;
    let reps = req.repetitions as usize;
    let sweep = exp_sweep(SAMPLE_RATE, SWEEP_F1, SWEEP_F2, SWEEP_S, SWEEP_FADE_S, SWEEP_AMPLITUDE);
    let (train, starts) = probe_train(&sweep, reps, GAP_S);

    let en = DeviceEnumerator::new().map_err(|e| failed(describe_hr(StreamRole::Probe, hr_of(&e))))?;
    let probe_dev =
        resolve(&en, Some(&req.probe_device), Direction::Render, StreamRole::Probe).map_err(|f| failed(describe_start_failure(&f)))?;
    let cap_dev =
        resolve(&en, Some(&req.capture_device), Direction::Capture, StreamRole::Capture).map_err(|f| failed(describe_start_failure(&f)))?;
    let mut probe = RenderStream::open(&probe_dev, RENDER_BUFFER_HNS).map_err(|e| failed(describe_hr(StreamRole::Probe, hr_of(&e))))?;
    let mut cap = CaptureStream::open_events(&cap_dev).map_err(|e| failed(describe_hr(StreamRole::Capture, hr_of(&e))))?;
    let probe_err = |hr| failed(describe_hr(StreamRole::Probe, hr));
    let cap_err = |hr| failed(describe_hr(StreamRole::Capture, hr));

    // The tap ring covers the whole test: train + tail + 1 s. Touch every page now so the audio
    // thread never takes a page fault on it.
    let total = train.len() + (TAIL_S * fs) as usize;
    let capacity = (total + SAMPLE_RATE as usize) / BLOCK + 1;
    let (mut tap_tx, mut tap_rx) = RingBuffer::<TapItem>::new(capacity);
    let blank = TapItem { qpc_hns: 0.0, samples: [0.0; BLOCK] };
    while tap_tx.push(blank).is_ok() {}
    while tap_rx.pop().is_ok() {}
    let run = send_tap(inner, tap_tx)?;
    restore.tap_sent = true;

    let mut tap = Recording { samples: Vec::with_capacity(capacity * BLOCK), stamps: Vec::with_capacity(capacity) };
    let out_cap = total + 2 * SAMPLE_RATE as usize;
    let mut out = Recording { samples: Vec::with_capacity(out_cap), stamps: Vec::with_capacity(out_cap / 64) };
    let mut packet = vec![0.0f32; cap.max_packet_frames()];
    let mut next_pos: Option<u64> = None;

    probe.write_silence(PROBE_PRE).map_err(probe_err)?;
    let mut written = PROBE_PRE as u64;
    let mut train_pos = 0usize;
    let mut probe_t: Vec<Option<f64>> = vec![None; reps];
    cap.start().map_err(cap_err)?;
    probe.start().map_err(probe_err)?;
    let deadline = Instant::now() + Duration::from_secs_f64(total as f64 / fs + 10.0);

    loop {
        if shared.cancelled() {
            probe.stop();
            cap.stop();
            return Err(TestError::Cancelled);
        }
        let still_same_run = lock(&inner.current).as_ref().is_some_and(|s| Arc::ptr_eq(s, &run));
        if inner.telemetry.state() != EngineState::Running || !still_same_run {
            return Err(failed("The filter stopped or restarted during the test; run it again."));
        }
        if Instant::now() > deadline {
            return Err(failed("The probe device stopped playing; check that it's connected and not in use."));
        }
        cap.wait(10);

        // Keep the probe topped up: the train, then silence.
        let padding = probe.padding().map_err(probe_err)?;
        let room = PROBE_FILL.saturating_sub(padding).min(probe.buffer_frames().saturating_sub(padding));
        if room > 0 {
            if train_pos < train.len() {
                let n = room.min(train.len() - train_pos);
                probe.write(&train[train_pos..train_pos + n]).map_err(probe_err)?;
                train_pos += n;
                written += n as u64;
            } else {
                probe.write_silence(room).map_err(probe_err)?;
                written += room as u64;
            }
        }
        // Each sweep start's play-out time, re-estimated until it has played.
        let clock = probe.clock().map_err(probe_err)?;
        let played = match clock {
            Some(c) => {
                for (t, &s) in probe_t.iter_mut().zip(&starts) {
                    let idx = (PROBE_PRE + s) as u64;
                    if idx < written && c.pos_frames <= idx as f64 {
                        *t = Some(play_time_hns(c, idx, fs));
                    }
                }
                c.pos_frames - PROBE_PRE as f64
            }
            None => written as f64 - (padding + PROBE_PRE) as f64,
        };

        // The output cable's capture, gaps (by device position) zero-filled.
        while let Some(p) = cap.read_packet(&mut packet).map_err(cap_err)? {
            if let Some(expect) = next_pos
                && p.device_pos > expect
                && p.device_pos - expect <= SAMPLE_RATE as u64
            {
                let gap = (p.device_pos - expect) as usize;
                out.samples.extend(std::iter::repeat_n(0.0, gap));
            }
            next_pos = Some(p.device_pos + p.frames as u64);
            if let Some(t) = p.qpc_hns {
                out.stamps.push((out.samples.len() as u64, t as f64));
            }
            out.samples.extend_from_slice(&packet[..p.frames.min(packet.len())]);
        }
        drain_tap(&mut tap_rx, &mut tap);

        shared.running(0.9 * (played / total as f64) as f32, "Measuring");
        if played >= total as f64 {
            break;
        }
    }
    probe.stop();
    cap.stop();
    drain_tap(&mut tap_rx, &mut tap);
    // The analysis is seconds of CPU for a long test (FFTs of 10 M points and more): at MMCSS
    // "Pro Audio" priority (or the TIME_CRITICAL fallback) it would starve other apps' audio and
    // the UI. Back to normal priority first; nothing below needs it, nor COM's real-time state.
    drop(mmcss);
    if run.tap_overflows.load(Relaxed) > 0 {
        log::warn!("latency test: the tap ring overflowed");
    }

    shared.running(0.92, "Analysing");
    let min_spacing = sweep.signal.len();
    let timed = |rec: &Recording, arrivals: Vec<Arrival>| -> Vec<TimedArrival> {
        arrivals
            .iter()
            .filter_map(|a| {
                interpolate_stamps(&rec.stamps, a.position, fs).map(|t_hns| TimedArrival { t_hns, confidence_db: a.confidence_db })
            })
            .collect()
    };
    let tap_t = timed(&tap, find_sweep_arrivals(&tap.samples, &sweep, reps, min_spacing));
    let out_t = timed(&out, find_sweep_arrivals(&out.samples, &sweep, reps, min_spacing));
    let period_hns = (sweep.signal.len() as f64 + 2.0 * GAP_S as f64 * fs) / fs * HNS_PER_S;
    let mut report = build_report(req.repetitions, &probe_t, &tap_t, &out_t, period_hns);
    if report.valid < req.repetitions {
        // Which side lost the sweeps, and how loud each recording was: tells a silent or muted device
        // from a garbled one.
        report.warnings.push(found_line(req.repetitions, tap_t.len(), &tap.samples, out_t.len(), &out.samples));
    }

    let tel = inner.telemetry.snapshot();
    report.readout_ms_median = tel.latency_valid.then_some(tel.latency_ms_median);
    report.chain_ms = run.chain_latency.load(Relaxed) as f32 * 1000.0 / SAMPLE_RATE as f32;
    let st = lock(&inner.status).clone();
    report.conditions =
        conditions(st.mic.as_ref(), st.output.as_ref(), &probe.info.stream_info(), &cap.info.stream_info(), req.mode, !req.through_model);
    report.warnings.extend(mode_warnings(req, inner.controls.aec.enabled.load(Relaxed)));
    shared.running(1.0, "Done");
    Ok(report)
}

/// "Sweeps found: 5 of 5 at the app's input (peak -12.0 dBFS), 0 of 5 in the output capture (peak -inf dBFS)."
fn found_line(reps: u32, tap_found: usize, tap: &[f32], out_found: usize, out: &[f32]) -> String {
    let peak = |x: &[f32]| {
        let p = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        if p > 0.0 { format!("{:.1} dBFS", 20.0 * p.log10()) } else { "silent".to_owned() }
    };
    format!(
        "Sweeps found: {tap_found} of {reps} at the app's input (peak {}), {out_found} of {reps} in the output capture (peak {}).",
        peak(tap),
        peak(out)
    )
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
    let slack = 5.0 * 1e4;
    let ms = |hns: f64| hns * 1e-4;
    let mut cands = Vec::new();
    for i in 0..repetitions as usize {
        let tp = probe.get(i).copied().flatten();
        let tap_i = match tp {
            Some(tp) => tap.iter().find(|a| a.t_hns >= tp - slack && a.t_hns < tp + half),
            None => tap.get(i),
        };
        let Some(ta) = tap_i else { continue };
        let Some(oa) = out.iter().find(|a| a.t_hns >= ta.t_hns - slack && a.t_hns < ta.t_hns + half) else {
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
    let all: Vec<f64> = cands.iter().map(|r| r.app).collect();
    let kept: Vec<Rep> = match median(&all) {
        Some(m) => cands.into_iter().filter(|r| (r.app - m).abs() <= OUTLIER_MS).collect(),
        None => Vec::new(),
    };

    let app_ms: Vec<f64> = kept.iter().map(|r| r.app).collect();
    let inputs: Vec<f64> = kept.iter().filter_map(|r| r.input).collect();
    let e2e: Vec<f64> = kept.iter().filter_map(|r| r.e2e).collect();
    let spread = if app_ms.is_empty() {
        0.0
    } else {
        app_ms.iter().copied().fold(f64::MIN, f64::max) - app_ms.iter().copied().fold(f64::MAX, f64::min)
    };
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
        chain_ms: 0.0,
        confidence_db_min: kept.iter().map(|r| r.confidence).fold(f32::NAN, f32::min),
        conditions: String::new(),
        warnings,
    }
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
