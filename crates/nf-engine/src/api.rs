//! nf-engine's public API: the only surface the GUI and the app use. Stable contract; the
//! implementation behind it lives in the crate's private modules.

use std::sync::Arc;
use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::sync::atomic::{AtomicBool, AtomicU64};

pub use nf_core::config::{
    AUTO_MODEL_DEPTH, BURSTY_MODEL_DEPTH, DEFAULT_NOISE_MODEL, DeviceSelection, EffectConfig, EngineConfig, GateConfig, GateMode,
    GatePosition, HighBand, IoConfig, MAX_MODEL_DEPTH, MicDelivery, OutputBackend, ProcessingConfig, STEADY_MODEL_DEPTH, is_model_id,
};
pub use nf_core::controls::{ChainParams, GateParams};
pub use nf_core::telemetry::{
    BudgetTotals, ECHO_GUARD_COOLING, ECHO_GUARD_HELD, ECHO_GUARD_TRIAL, ECHO_GUARD_WATCHING, EngineState, LatencyBudget, MeterSnapshot,
    OutputEvents, REFERENCE_TARGET_HIGH_MS, TelemetrySnapshot, WakeBlocks,
};

pub use crate::effect::{EffectInfo, attach_offline_effect};

/// Endpoint direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flow {
    Capture,
    Render,
}

/// One active audio endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// WASAPI endpoint ID for this installation.
    pub id: String,
    /// For our cable endpoints, the cable identity and role to save across reinstalls.
    pub cable_selection: Option<String>,
    /// Friendly name, e.g. "CABLE Input (VB-Audio Virtual Cable)".
    pub name: String,
    pub flow: Flow,
    /// Default device for the console/multimedia role.
    pub is_default: bool,
    /// Default device for the communications role (opening it can trigger ducking; TODO #8).
    pub is_default_communications: bool,
    /// The shared-mode mix format's rate and channel count.
    pub sample_rate: u32,
    pub channels: u16,
}

impl DeviceInfo {
    /// The selection to save. Ordinary endpoint IDs from existing files or the command line still match this device.
    pub fn selection(&self) -> &str {
        self.cable_selection.as_deref().unwrap_or(&self.id)
    }

    pub fn matches_selection(&self, selected: &str) -> bool {
        self.id == selected || self.selection() == selected
    }
}

/// An engine error, already worded for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

/// How one stream was opened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInfo {
    pub device_name: String,
    /// The device-side format (before conversion to the app's 48 kHz mono).
    pub sample_rate: u32,
    pub channels: u16,
    /// The engine period in ms (10 by default; smaller with IAudioClient3).
    pub period_ms: f32,
    /// Opened through IAudioClient3 at a smaller-than-default period.
    pub low_latency: bool,
    /// WASAPI converts the rate (the device isn't at 48 kHz).
    pub autoconvert: bool,
    /// RAW mode is in effect (mic only).
    pub raw: bool,
    /// In WASAPI exclusive mode (`IoConfig::mic_exclusive`, `output_exclusive`, granted: a refused one runs shared, with a
    /// note), in this sample format: "32-bit float", "24-bit" or "16-bit". `None` in shared mode. The period is then
    /// the exclusive buffer's, and no Windows effects run.
    pub exclusive: Option<&'static str>,
    /// Asked for in exclusive mode and refused: why, as a clause ("another app is using it, ..."). It runs in shared mode;
    /// the status's notes say so too, with the cost and the fix.
    pub exclusive_refused: Option<String>,
}

/// Slow-changing engine status, for the UI's status and diagnostics lines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineStatus {
    pub state: EngineState,
    /// The error that stopped the engine (or failed its last start), worded for the user. `None`
    /// while running: standing warnings are in `warnings` only.
    pub message: Option<String>,
    pub mic: Option<StreamInfo>,
    pub output: Option<StreamInfo>,
    pub reference: Option<StreamInfo>,
    /// "large pages (2 MiB)", "4 KB locked (no lock-pages privilege)", ...
    pub memory: String,
    /// The audio thread is registered with MMCSS "Pro Audio".
    pub mmcss: bool,
    /// The chain's current stream offset in ms (stages only; not device buffers).
    pub chain_latency_ms: f32,
    /// Warnings that hold while running (e.g. the mic is the output cable's own recording side, the
    /// echo reference was lost), worded for the user. Empty when all is well.
    pub warnings: Vec<String>,
    /// Notes that hold while running: how it runs where that isn't as asked but needs nothing done, worded for the user. Today
    /// a stream asked for in exclusive mode (the default) that runs shared: which, why, what it costs and how to fix it or
    /// stop it trying. Empty when all runs as asked. The window shows them calmer than the warnings, `run` as "note:".
    pub notes: Vec<String>,
    /// Stopped by a fault but armed: the engine can restart on settings or device changes.
    pub retrying: bool,
    /// Waiting specifically for a settings or device change. Otherwise an armed engine also retries on a timer.
    pub waiting_for_change: bool,
    /// What the user can do about the error in `message`, when the app can take them there.
    pub remedy: Option<Remedy>,
    /// The depth a model pack's inference worker runs at in this run, and how it was chosen (`None`: RNNoise, or stopped).
    pub model_depth: Option<ModelDepth>,
}

/// The depth a run's model pack runs at (`ProcessingConfig::model_depth`; TODO 5, 6), chosen at the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelDepth {
    pub depth: u32,
    /// Picked automatically (the default), from how the mic delivers: as its last run in this process, opened the same way,
    /// measured it (`TelemetrySnapshot::wake_blocks`), else a block at a time for the exclusive mic at a period of a block or
    /// less. `None`: the config set it.
    pub auto: Option<MicDelivery>,
}

impl ModelDepth {
    /// "depth 1 (auto: the mic delivers a block at a time)", or "depth 3 (set)".
    pub fn text(&self) -> String {
        match self.auto {
            Some(d) => format!("depth {} (auto: {})", self.depth, d.text()),
            None => format!("depth {} (set)", self.depth),
        }
    }
}

/// A fix for a failure that the app can take the user to (the window offers it as a button).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remedy {
    /// Windows' microphone privacy settings keep desktop apps from recording: the mic, or any recording
    /// endpoint (a virtual cable's recording side counts as a microphone).
    MicPrivacy,
}

impl Remedy {
    /// The button's text.
    pub fn label(self) -> &'static str {
        match self {
            Remedy::MicPrivacy => "Open microphone privacy settings",
        }
    }

    /// The Settings page that fixes it, for the shell to open (`start ms-settings:...` in a terminal).
    pub fn settings_uri(self) -> &'static str {
        match self {
            Remedy::MicPrivacy => "ms-settings:privacy-microphone",
        }
    }
}

/// The buffer fills the engine holds (engine.md "Targets"), for diagnostics. Zero while stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BufferTargets {
    /// Output render-buffer target at the start: one render period + one block + `io.output_margin_ms`. With
    /// `io.adaptive_output` it moves (telemetry's `output_target_ms` has the current one).
    pub output_ms: f32,
    /// The reference's starting lead target: one loopback period + 2 ms (0 without a reference). It can rise in the first
    /// seconds and when underflows persist, and come back down after five minutes under; telemetry's `reference_target_ms`
    /// has the current one (engine.md, "The reference").
    pub reference_ms: f32,
}

/// With `IoConfig::adaptive_output`, how long a run measures the delivery at the fixed output target before the target can
/// come down, in seconds (engine.md, "The output target").
pub const ADAPTIVE_OUTPUT_WINDOW_S: u64 = crate::out_target::WINDOW_S as u64;
/// The earliest the adaptive output target first comes down, in seconds: a window, and a second that confirms it (its
/// trial: out_target.rs).
pub const ADAPTIVE_OUTPUT_FIRST_S: u64 = 2 * ADAPTIVE_OUTPUT_WINDOW_S;
/// The longest the adaptive output target's walk down takes once it has started, in seconds (it moves a tenth of the way a
/// second, so the queue follows without overshooting).
pub const ADAPTIVE_OUTPUT_WALK_S: u64 = crate::out_target::RAMP_MAX_S as u64;
/// The longest a run's adaptive output target takes to settle when nothing runs it short, in seconds: the first window starts
/// a trial, which holds it for a second (`TelemetrySnapshot::output_target_on_trial`), and then it walks down (engine.md,
/// "The output target").
pub const ADAPTIVE_OUTPUT_SETTLE_S: u64 = crate::out_target::SETTLE_MAX_S as u64;

/// The output target as the reports' conditions name it, from the target at the test's end
/// (`TelemetrySnapshot::output_target_ms`), whether it adapts, and the fixed one it starts at (`BufferTargets::output_ms`):
/// "output target 22.0 ms (fixed)"; adaptive, either "6.8 ms at the end (adaptive, down from 14.0 ms)" or "14.0 ms at the end
/// (adaptive, at the fixed target: it comes down only after 240 s of running, and goes back up to it after an underrun)". A
/// snapshot, so "at the end": a long test may have run part of its time at another target. The second is the neutral
/// reading of a target at the fixed one, which a run's first four minutes are (`latency` and `quality` measure at once
/// unless `--settle` waits), but so is a run that went back after an underrun (out_target.rs's strikes), whose delivery
/// needs the whole target, or whose `--settle` timed out. Empty without a target (stopped).
pub fn output_target_text(target_ms: f32, adaptive: bool, fixed_ms: f32) -> String {
    if target_ms.is_nan() || target_ms <= 0.0 {
        return String::new();
    }
    if !adaptive {
        return format!("output target {target_ms:.1} ms (fixed)");
    }
    let how = if fixed_ms > 0.0 && target_ms < fixed_ms - 0.05 {
        format!("down from {fixed_ms:.1} ms")
    } else {
        format!(
            "at the fixed target: it comes down only after {ADAPTIVE_OUTPUT_FIRST_S} s of running, and goes back up to it after \
             an underrun"
        )
    };
    format!("output target {target_ms:.1} ms at the end (adaptive, {how})")
}

/// The direct driver route's output target as the reports' conditions name it, from the snapshot at the test's end: the
/// reader's packet (`TelemetrySnapshot::output_period_ms`, the recording app's period) and the headroom over it
/// (`output_headroom_ms`): "output target one reader packet, 3.33 ms (the direct route's, with no headroom: it runs clean)",
/// or "output target 12.33 ms at the end: one reader packet, 3.33 ms, + 9.00 ms of headroom (the direct route's: raised by
/// the driver's underruns, lowered after clean running)". Empty while no app records (no packet).
pub fn ring_target_text(period_ms: f32, headroom_ms: f32) -> String {
    if period_ms.is_nan() || period_ms <= 0.0 {
        return String::new();
    }
    if headroom_ms.is_nan() || headroom_ms <= 0.0 {
        return format!("output target one reader packet, {period_ms:.2} ms (the direct route's, with no headroom: it runs clean)");
    }
    format!(
        "output target {:.2} ms at the end: one reader packet, {period_ms:.2} ms, + {headroom_ms:.2} ms of headroom (the direct \
         route's: raised by the driver's underruns, lowered after clean running)",
        period_ms + headroom_ms
    )
}

/// Which loop the latency test measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyMode {
    /// Probe into a virtual cable whose output is the app's mic (layer 2, TODO #13).
    VirtualLoop,
    /// Probe on the speakers, picked up by the physical mic (layer 3).
    Acoustic,
}

/// What to measure.
#[derive(Clone, Debug, PartialEq)]
pub struct LatencyTestRequest {
    pub mode: LatencyMode,
    /// Render endpoint the probe is played on: cable A's input (virtual) or the speakers (acoustic).
    pub probe_device: String,
    /// Capture endpoint carrying the app's output: the output cable's capture side (e.g. "CABLE Output").
    pub capture_device: String,
    /// Sweeps to play (each about 1 s plus gaps). 5 by default.
    pub repetitions: u32,
    /// Keep the noise model in the path (needs a speech probe; v0 measures in passthrough when false).
    pub through_model: bool,
}

impl Default for LatencyTestRequest {
    fn default() -> Self {
        Self {
            mode: LatencyMode::VirtualLoop,
            probe_device: String::new(),
            capture_device: String::new(),
            repetitions: 5,
            through_model: false,
        }
    }
}

/// The latency test's result. All times in ms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LatencyReport {
    pub repetitions: u32,
    /// Repetitions whose arrivals were found with enough confidence in both streams.
    pub valid: u32,
    /// Per valid repetition: the app's input tap to the output cable's capture (app + output cable).
    pub app_ms: Vec<f64>,
    pub app_ms_median: f64,
    /// Max minus min of `app_ms`.
    pub app_ms_spread: f64,
    /// Probe play-out to the app's input tap (the input cable or the acoustic path), if measurable.
    pub input_path_ms_median: Option<f64>,
    /// Probe play-out to the output capture: what an app reading the output cable experiences.
    pub end_to_end_ms_median: Option<f64>,
    /// The live readout (layer 1) during the test, for comparison. On the direct driver route to the sample time (the
    /// recording app's stamps, what the sweeps measure).
    pub readout_ms_median: Option<f32>,
    /// The direct driver route's live readout to the delivery time (the driver's pass that hands each packet over) over the
    /// same blocks, beside it: it's later by what's left of the reader's packet. `None` on the WASAPI route.
    pub delivery_ms_median: Option<f32>,
    /// The direct driver route's reader packet at the test's end, in ms (the recording app's period, which the readout's
    /// delivery and the target follow); `None` on the WASAPI route, or with no app recording.
    pub reader_period_ms: Option<f32>,
    /// The readout's budget during the test: each part's mean over the blocks from the sweeps' start to their end (engine.md,
    /// "The latency budget"). The readout leaves the chain's offset out; `app_ms` holds it.
    pub budget: Option<LatencyBudget>,
    /// The chain's own stream offset during the test (passthrough: the enabled stages' latency).
    pub chain_ms: f32,
    /// What the output side did over the test (its underruns, zeros, drops, slips and re-anchors): the tapped blocks' own
    /// counts added up, from the tap's first block to its last, as the per-sweep lines add theirs (`disturbances`); without
    /// them, telemetry's between its once-a-second snapshots at the test's start and end. A spread between sweeps that these
    /// don't explain is the devices' own (TODO 9). `None` until the sweeps have run.
    pub output: Option<OutputEvents>,
    /// The mic's glitches (packets flagged discontinuous or missing by device position, and the FIFO overflowing) and stalls (a
    /// wake that found nothing from the mic 100 ms after its last) over the same blocks. `None` until the sweeps have run.
    pub mic_glitches: Option<u64>,
    pub mic_stalls: Option<u64>,
    /// Per valid repetition, as `app_ms`: what disturbed it ([`SweepDisturbance`]; nothing when `!any()`). Empty when the test
    /// had no blocks' facts to tell by.
    pub disturbances: Vec<SweepDisturbance>,
    /// The undisturbed repetitions' median and spread (of `app_ms`); both `None` when none was undisturbed, or `disturbances`
    /// is empty. Beside the median of all, which mixes them with the disturbed ones.
    pub clean_ms_median: Option<f64>,
    pub clean_ms_spread: Option<f64>,
    /// Lowest arrival confidence among valid repetitions, in dB.
    pub confidence_db_min: f32,
    /// Engine periods, RAW, autoconvert and the devices used, for the record.
    pub conditions: String,
    pub warnings: Vec<String>,
}

impl LatencyReport {
    /// The valid repetitions the engine's own events disturbed, with their app latency (ms) and what disturbed them.
    pub fn disturbed(&self) -> impl Iterator<Item = (f64, &SweepDisturbance)> {
        self.app_ms.iter().copied().zip(&self.disturbances).filter(|(_, d)| d.any())
    }

    /// The app latency (ms) of the valid repetitions nothing disturbed: `clean_ms_median`'s; none when `disturbances` is empty.
    pub fn undisturbed(&self) -> impl Iterator<Item = f64> {
        self.app_ms.iter().copied().zip(&self.disturbances).filter(|(_, d)| !d.any()).map(|(a, _)| a)
    }

    /// The undisturbed repetitions in the stretches between the engine's events (`SweepDisturbance::events_before`), each
    /// stretch's repetitions in order: one stretch when no event came between any two of them. Across an event the output path
    /// may have moved for good (after a late answer the sweeps read 1-4 ms less against the readout: TODO 13).
    pub fn undisturbed_stretches(&self) -> Vec<Vec<u32>> {
        let mut stretches: Vec<(u32, Vec<u32>)> = Vec::new();
        for d in self.disturbances.iter().filter(|d| !d.any()) {
            match stretches.last_mut() {
                Some((before, reps)) if *before == d.events_before => reps.push(d.repetition),
                _ => stretches.push((d.events_before, vec![d.repetition])),
            }
        }
        stretches.into_iter().map(|(_, reps)| reps).collect()
    }

    /// When the undisturbed repetitions span the engine's events: "in 2 stretches between the engine's events (sweeps 1, 2 |
    /// 4, 5): the output path may differ from one to the next". `None` in one stretch (or none).
    pub fn stretches_text(&self) -> Option<String> {
        let stretches = self.undisturbed_stretches();
        (stretches.len() > 1).then(|| {
            let each: Vec<String> = stretches.iter().map(|s| repetitions_text(s)).collect();
            format!(
                "in {} stretches between the engine's events (sweeps {}): the output path may differ from one to the next",
                stretches.len(),
                each.join(" | ")
            )
        })
    }
}

/// Repetitions in a few characters: "3", "1, 2", "4-7" (three or more in a row), "1, 4-7, 9".
fn repetitions_text(reps: &[u32]) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &r in reps {
        match runs.last_mut() {
            Some((_, last)) if r == *last + 1 => *last = r,
            _ => runs.push((r, r)),
        }
    }
    let mut parts = Vec::new();
    for (a, b) in runs {
        match b - a {
            0 => parts.push(a.to_string()),
            1 => parts.extend([a.to_string(), b.to_string()]),
            _ => parts.push(format!("{a}-{b}")),
        }
    }
    parts.join(", ")
}

/// What disturbed one sweep of the latency test (TODO 13), from the tap's blocks' own facts (disturbed.rs):
/// - the engine's own events in its window, from 100 ms before its first sample reached the app's input to its app latency
///   and 25 ms after its last (100 ms at least): a mic glitch or stall, an output correction, a move of the output target or
///   of the echo-path guard's hold (an output event moves the samples queued then and after; a mic glitch's zeros land in the
///   next blocks);
/// - the drift loop moving the path under it: its net slips over the window at more than 0.5 ms a second, past any crystal
///   clock's drift (the loop's top rate is 1.5 ms a second);
/// - the filter's own estimate of its output queue still away from its level while the drift loop trims it back, after such
///   an event, or from the test's start when it began 2.5 ms or more from its target (something before the test moved it).
///
/// With today's defaults a late wake left the estimate 9-13 ms over its level and the loop trimming for 6-9 s and more; the
/// sweeps meanwhile read up to 8 ms high (TODO 13's 2026-11-23 runs). After the event the estimate and the path no longer
/// agreed (the sweeps read 1-4 ms less against the readout than before it), so sweeps on either side of an event may
/// differ even once the queue is back: `events_before` tells them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SweepDisturbance {
    /// The repetition, from 1, in the train's order.
    pub repetition: u32,
    /// Mic glitches and stalls in the sweep's window.
    pub mic_glitches: u32,
    pub mic_stalls: u32,
    /// The output's counts in the sweep's window, as the engine counted them: its corrections (underruns, an exclusive device's
    /// silence, late answers, deficits, coarse drops, re-anchors), which disturb it, and its slips, which disturb it only as
    /// `trim_samples`: following the clocks' drift is the drift loop's ordinary work.
    pub output: OutputEvents,
    /// Times the output target moved in the sweep's window (adaptive: down a step, or back up after an underrun), and the
    /// echo-path guard's hold (each move a jump in the audio's delay).
    pub target_moves: u32,
    pub hold_moves: u32,
    /// The direct driver route's moves in the window: of the headroom over one reader packet (raised after an underrun,
    /// lowered after clean running: ring_headroom.rs), and of the reader's packet (another recording app took the ring). Each
    /// moves the target; neither counts in `target_moves`.
    pub headroom_moves: u32,
    pub packet_moves: u32,
    /// The drift loop's net slips over the window (inserted less dropped: the samples the path grew by), when they came at more
    /// than 0.5 ms a second; 0 otherwise.
    pub trim_samples: i64,
    /// Whether the output queue was recovering in the window, and from what.
    pub queue: QueueRecovery,
    /// While it recovered: the filter's own estimate of its queue (the fill the drift loop reads less its target, a 100 ms
    /// mean) at its farthest from its level, in ms (over when positive). Not the sweep's displacement: after a late answer the
    /// estimate and the path disagree.
    pub queue_off_ms: f32,
    /// The tapped blocks with an event before the sweep's window: sweeps with different counts are on either side of one.
    pub events_before: u32,
}

/// Whether a sweep's output queue was recovering, and from what ([`SweepDisturbance::queue`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueRecovery {
    #[default]
    None,
    /// From an event in the test: the level is the queue's before it.
    AfterEvent,
    /// From the test's start, which found the queue 2.5 ms or more from its target (an event before the test, or an
    /// unsettled start): the level is the target.
    FromStart,
}

impl SweepDisturbance {
    /// Anything at all: an event, the drift loop moving the path, or the queue recovering.
    pub fn any(&self) -> bool {
        self.mic_glitches > 0
            || self.mic_stalls > 0
            || self.output.any_correction()
            || self.target_moves > 0
            || self.hold_moves > 0
            || self.headroom_moves > 0
            || self.packet_moves > 0
            || self.trim_samples != 0
            || self.queue != QueueRecovery::None
    }

    /// What disturbed it, in one phrase: "1 mic glitch; output: 1 underrun (3.3 ms of silence played, a late answer); the
    /// filter's own estimate of its output queue up to 11.8 ms over its level before the event, as the drift loop trims it
    /// back (86 samples dropped net over the sweep, 1.8 ms)". Empty when nothing did.
    pub fn text(&self) -> String {
        let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        let mic: Vec<String> = [(self.mic_glitches, "mic glitch", "mic glitches"), (self.mic_stalls, "mic stall", "mic stalls")]
            .iter()
            .filter(|(n, ..)| *n > 0)
            .map(|&(n, one, many)| plural(n, one, many))
            .collect();
        if !mic.is_empty() {
            parts.push(mic.join(", "));
        }
        if self.output.any_correction() {
            parts.push(format!("output: {}", self.output.corrections_text()));
        }
        if self.target_moves > 0 {
            parts.push(format!("{} (adaptive)", plural(self.target_moves, "output target move", "output target moves")));
        }
        if self.headroom_moves > 0 {
            parts.push(format!(
                "{} (direct route: raised after an underrun, lowered after clean running)",
                plural(self.headroom_moves, "move of the ring's headroom", "moves of the ring's headroom")
            ));
        }
        if self.packet_moves > 0 {
            parts.push(format!(
                "{} (direct route: another recording app's period)",
                plural(self.packet_moves, "change of the reader's packet", "changes of the reader's packet")
            ));
        }
        if self.hold_moves > 0 {
            parts.push(format!("{} of the echo-path guard's hold", plural(self.hold_moves, "move", "moves")));
        }
        let trim = (self.trim_samples != 0).then(|| {
            let way = if self.trim_samples < 0 { "dropped" } else { "inserted" };
            let n = self.trim_samples.unsigned_abs();
            format!("{n} samples {way} net over the sweep, {:.1} ms", n as f64 * 1000.0 / f64::from(nf_core::SAMPLE_RATE))
        });
        let level = match self.queue {
            QueueRecovery::None => None,
            QueueRecovery::AfterEvent => Some("its level before the event"),
            QueueRecovery::FromStart => Some("its target from the test's start (something before the test moved it)"),
        };
        match (level, trim) {
            (Some(level), trim) => {
                let (by, way) = (self.queue_off_ms.abs(), if self.queue_off_ms >= 0.0 { "over" } else { "under" });
                let base = format!(
                    "the filter's own estimate of its output queue up to {by:.1} ms {way} {level}, as the drift loop trims it back"
                );
                parts.push(match trim {
                    Some(t) => format!("{base} ({t})"),
                    None => base,
                });
            }
            (None, Some(t)) => parts.push(format!("the drift loop moving the output path under it ({t})")),
            (None, None) => {}
        }
        parts.join("; ")
    }
}

/// The mic's events in a few words: "no mic glitches", "1 mic glitch", "3 mic glitches", "no mic glitches, 1 mic stall".
pub fn mic_events_text(glitches: u64, stalls: u64) -> String {
    let glitches = match glitches {
        0 => "no mic glitches".to_owned(),
        1 => "1 mic glitch".to_owned(),
        n => format!("{n} mic glitches"),
    };
    match stalls {
        0 => glitches,
        1 => format!("{glitches}, 1 mic stall"),
        n => format!("{glitches}, {n} mic stalls"),
    }
}

/// A running latency test's progress.
#[derive(Clone, Debug, PartialEq)]
pub enum LatencyProgress {
    Running { fraction: f32, stage: &'static str },
    Done(Box<LatencyReport>),
    Failed(String),
    Cancelled,
}

/// Handle to a latency test started with [`Engine::start_latency_test`].
#[derive(Clone)]
pub struct LatencyTest {
    pub(crate) inner: Arc<crate::latency_test::TestShared>,
}

impl LatencyTest {
    pub fn progress(&self) -> LatencyProgress {
        self.inner.progress()
    }
    pub fn cancel(&self) {
        self.inner.cancel()
    }
}

/// What the cable-only latency test measures: the sweep train played on `probe_device` and found in `capture_device`, with no
/// engine in between. One virtual cable (its input as the probe, its output as the capture), or any playback-to-recording path.
#[derive(Clone, Debug)]
pub struct CableLatencyRequest<'a> {
    pub probe_device: String,
    pub capture_device: String,
    /// Sweeps to play (each about 1 s plus gaps). 5 by default.
    pub repetitions: u32,
    /// Set (e.g. by Ctrl+C) to stop the test early; it then fails with "The test was cancelled."
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for CableLatencyRequest<'_> {
    fn default() -> Self {
        Self { probe_device: String::new(), capture_device: String::new(), repetitions: 5, cancel: None }
    }
}

/// The cable-only latency test's result. All times in ms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CableLatencyReport {
    pub repetitions: u32,
    /// Repetitions whose arrival was found with enough confidence.
    pub valid: u32,
    /// Per valid repetition: the probe's play-out (its AudioClock) to the arrival in the capture (its packets' QPC stamps).
    pub cable_ms: Vec<f64>,
    pub cable_ms_median: f64,
    /// Max minus min of `cable_ms`.
    pub cable_ms_spread: f64,
    /// Lowest arrival confidence among valid repetitions, in dB.
    pub confidence_db_min: f32,
    /// Both streams' formats and periods, for the record.
    pub conditions: String,
    pub warnings: Vec<String>,
}

/// What a [`PlayTrack`]'s device is to the test, for the wording of its errors ("The probe device ...").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackRole {
    /// The device that feeds the filter's mic.
    #[default]
    Probe,
    /// The far end's speakers in an echo test.
    FarEnd,
}

/// One playback endpoint and what it plays.
#[derive(Clone, Debug)]
pub struct PlayTrack<'a> {
    /// Endpoint id.
    pub device: String,
    pub role: TrackRole,
    /// 48 kHz mono in ±1, played from the end of the lead.
    pub samples: &'a [f32],
    /// A second signal, placed while the take runs ([`LatePart`]); `None` for none.
    pub late: Option<LatePart<'a>>,
}

/// A track's second signal, whose start the caller sets while the take runs: `noise-filter quality`'s echo test places the far
/// voice this way, once it has timed a marker in the track's first signal (`samples`). Until the part starts, the track plays
/// silence after its first signal.
#[derive(Clone, Copy, Debug)]
pub struct LatePart<'a> {
    /// 48 kHz mono in ±1.
    pub samples: &'a [f32],
    /// Where it starts, in frames after the lead: [`LatePart::UNSET`] until it's set, and it's set once ([`LatePart::set`],
    /// a compare-exchange from UNSET). The caller sets it when it knows. The track's player sets `default` if it gets there
    /// first. A start the player has already written past starts the part where the player is (later than asked), and
    /// [`PlayRecordResult::late_starts`] says where it went. Starts are clamped to the first signal's end..=`latest`.
    pub start: &'a AtomicU64,
    /// The start the player takes when it gets there with none set: at or after the first signal's end, and at most `latest`.
    pub default: u64,
    /// The latest start that counts (a later one is taken as this).
    pub latest: u64,
}

impl LatePart<'_> {
    /// `start` before anything has set it.
    pub const UNSET: u64 = u64::MAX;

    /// Sets the part's start, `frames` after the lead, unless it's set already (by an earlier call, or by the player at
    /// `default`): true when this call set it.
    pub fn set(&self, frames: u64) -> bool {
        frames != Self::UNSET && self.start.compare_exchange(Self::UNSET, frames, AcqRel, Acquire).is_ok()
    }

    /// The start set so far, if any.
    pub fn get(&self) -> Option<u64> {
        Some(self.start.load(Acquire)).filter(|&v| v != Self::UNSET)
    }
}

/// Where a track's [`LatePart`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LateStart {
    /// Frames after the lead.
    pub frames: u64,
    /// The start the caller set (clamped as [`LatePart`] says), or `None` when the player got to the default first. Less
    /// than `frames`: it was set after the player had written past it, and the part started that much late.
    pub set: Option<u64>,
}

/// What [`play_record`] plays and records.
#[derive(Clone, Debug)]
pub struct PlayRecordRequest<'a> {
    /// The tracks, all started together after the lead. Each device plays one.
    pub play: Vec<PlayTrack<'a>>,
    /// The capture endpoint recorded meanwhile (id).
    pub capture_device: String,
    /// Silence every track plays first, in seconds: the streams settle. 1 s by default.
    pub lead_s: f32,
    /// The recording runs on this long after the longest track ends, for the path's latency and tails. 2 s by default.
    pub tail_s: f32,
    /// Set (e.g. by Ctrl+C) to stop early; it then fails with "The test was cancelled."
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for PlayRecordRequest<'_> {
    fn default() -> Self {
        Self { play: Vec::new(), capture_device: String::new(), lead_s: 1.0, tail_s: 2.0, cancel: None }
    }
}

/// What [`play_record`] recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayRecordResult {
    /// The capture as 48 kHz mono, from the capture's start (just before the tracks start); frames missing by device
    /// position are zeros.
    pub recording: Vec<f32>,
    /// The lead in frames: each track's samples start this far into its stream.
    pub lead_frames: usize,
    /// Jumps in the capture's device position, and the frames zero-filled for them (a jump over 1 s isn't filled).
    pub capture_gaps: u64,
    pub gap_frames: u64,
    /// Times a playback stream was found empty before its track had all been written (it ran dry).
    pub playback_underruns: u64,
    /// The playback streams, in the request's order, and the capture stream.
    pub playback: Vec<StreamInfo>,
    pub capture: StreamInfo,
    /// Where each track's [`LatePart`] started, in the request's order (`None` for a track without one).
    pub late_starts: Vec<Option<LateStart>>,
}

/// The longest chain recording, in seconds (15 minutes, as [`play_record`]'s).
pub const MAX_CHAIN_RECORDING_S: f64 = 900.0;

/// What the echo reference's timeline did at one block ([`ChainBlock::reference`]; engine.md, "The reference"). Indices and
/// counts are in samples at 48 kHz.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReferenceFacts {
    /// The timeline's index of the first reference sample served with the block. The timeline counts the loopback's samples
    /// from its first packet, frames missing by device position and losses its stamps showed included. The block's
    /// [`ChainBlock::mic_index`] less this is the echo path as the engine holds it. Block to block it moves only by what
    /// the facts show: on the mic's side by [`ChainBlock::mic_discarded`], on the reference's by a slip (`consumed` other
    /// than 480) and a jump (`jumped`).
    pub read_index: i64,
    /// This block's lead reading: the mic block's capture time less the tap time of the first reference sample served with it,
    /// in ms, by the streams' stamps (or by delivery when `stamped` is false), after any jump at this block. NaN without a
    /// reading (nothing delivered yet, or a reading that wasn't a number).
    pub lead_reading_ms: f32,
    /// The lead as the loop holds it (its 1 s low-pass; telemetry's `reference_fill_ms`) and its target, in ms.
    pub lead_ms: f32,
    pub target_ms: f32,
    /// The loop's drift estimate: positive when the loopback's clock runs fast against the mic's.
    pub drift_ppm: f32,
    /// The stamps time the lead (else the delivery does).
    pub stamped: bool,
    /// The loopback had delivered within two periods and 5 ms (else the loop coasted on its drift), and it delivered at
    /// this block's wake.
    pub fresh: bool,
    pub delivered: bool,
    /// Samples delivered past the read position when the block was served; negative while samples are owed.
    pub buffered: i64,
    /// Samples the block took from the timeline: 480, or 479 and 481 at a slip (resampled to 480); 0 before the first
    /// packet (the block is zeros, and the timeline starts with that packet).
    pub consumed: u16,
    /// Zeros served for samples that hadn't arrived: owed, and discarded when they come (a reference gap).
    pub owed: u16,
    /// Zeros served for samples the timeline doesn't hold: before the loopback's first sample, after a jump back, or the
    /// whole block before the first packet.
    pub missing: u16,
    /// How far the read position jumped at this block, forward (positive) or back, by the events below.
    pub jumped: i64,
    /// What happened at this block: the `ReferenceFacts::*` event bits.
    pub events: u16,
}

impl ReferenceFacts {
    /// Nothing delivered yet: the block is zeros.
    pub const NO_READING: u16 = 1 << 0;
    /// The first reading aligned the timeline on the target (a jump).
    pub const ALIGNED: u16 = 1 << 1;
    /// The low-passed lead was centred on the target in one jump: at the start-up's end, or after a break timed by delivery.
    pub const CENTRED: u16 = 1 << 2;
    /// A broken timeline (a step of the lead past 4 ms, or 30 ms timed by delivery) undone in one jump.
    pub const BROKEN: u16 = 1 << 3;
    /// Timed by delivery: a step of the count put back in a jump of whole packets.
    pub const STEPPED: u16 = 1 << 4;
    /// The lead target moved, and the read position with it.
    pub const RETARGETED: u16 = 1 << 5;
    /// In the start-up, the timing changed (a stream's stamps came or went): the start-up started over.
    pub const RESTARTED: u16 = 1 << 6;
    /// The stamps were given up for the rest of the run: delivery times the lead from this block on.
    pub const UNSTAMPED: u16 = 1 << 7;
    /// The loop coasted on its drift estimate: the loopback wasn't fresh, or the reading was too far off to use.
    pub const COASTED: u16 = 1 << 8;
    /// The echo-path guard's hold grew (nf-engine `echo_guard.rs`): the read position moved on with it, the lead the loop reads
    /// unmoved. (A hold that shrinks drops mic samples instead: `ChainBlock::mic_discarded`.)
    pub const HELD: u16 = 1 << 9;

    #[rustfmt::skip]
    const NAMES: [(u16, &'static str); 10] = [
        (Self::NO_READING, "no-reading"), (Self::ALIGNED, "aligned"), (Self::CENTRED, "centred"), (Self::BROKEN, "broken"),
        (Self::STEPPED, "stepped"), (Self::RETARGETED, "retargeted"), (Self::RESTARTED, "restarted"),
        (Self::UNSTAMPED, "unstamped"), (Self::COASTED, "coasted"), (Self::HELD, "held"),
    ];

    /// The names of the events at this block ("aligned", "coasted", ...), in the order of their bits.
    pub fn event_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        Self::NAMES.iter().filter(|(bit, _)| self.events & bit != 0).map(|&(_, name)| name)
    }
}

/// What the output side did at one block ([`ChainBlock::output`]; engine.md, "The audio thread": the output slip and the coarse
/// corrections). Counts are samples at 48 kHz. The output path's delay moves only by what these show (slips, zeros, drops, and
/// an exclusive device's `silence`) and by what the device and its driver do after taking the samples, which the engine doesn't
/// see (TODO 9).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputFacts {
    /// The stream index where the block's first sample went: the frames written before it, the prefill and our pending
    /// samples included. Block to block it moves by the last block's length (479, 480 or 481, or 0 when it was `skipped`) plus
    /// this block's `zeros` less its `dropped`; for a skipped block, where it would have gone. An exclusive output's device
    /// stream has its `silence` too: there the block went that far on, summed over the run.
    pub index: u64,
    /// The device's padding when the block was written (before any of it), and our samples pending behind it.
    pub padding: u32,
    pub pending: u32,
    /// The fill the drift loop read (samples, calibrated to the padding: telemetry's `output_fill_ms`), and the target it holds.
    pub fill: f32,
    pub target: u32,
    /// The block's slip: -1 (a sample dropped), 0, or +1 (one inserted).
    pub slip: i8,
    /// Zeros written at this block to top the device up to the target (a deficit; the first block's starting fill isn't one).
    pub zeros: u32,
    /// Pending samples dropped at this block (written before it, not yet given to the device): in a coarse drop, or because
    /// they overflowed.
    pub dropped: u32,
    /// The block itself was dropped in a coarse drop: none of it went to the device (its 480 samples count in the output's
    /// dropped samples).
    pub skipped: bool,
    /// An underrun counted at this block: the device found dry, a deficit, or an exclusive output's ring short since the last block.
    pub underrun: bool,
    /// Of it: an exclusive output's device event answered late since the last block (the thread held up past the
    /// device's deadline), not its ring short.
    pub late: bool,
    /// The fill's calibration to the padding started over at this block (the device ran dry, or its clock jumped).
    pub reanchored: bool,
    /// Silence an exclusive output's device played in our stream since the block before, where its ring ran short at an
    /// event or an event was answered late (frames; always with `underrun`). Every later sample plays that much later, until
    /// the drift loop slips it back out. 0 for a shared output.
    pub silence: u32,
    /// The device's clock as the block read it: its position in our stream's frames (an exclusive device's `silence` left
    /// out), and the QPC time that position belongs to (100 ns units). NaN without a reading (its first few ms). Against the
    /// mic's index and stamps they give the clocks' drift ([`clock_drift`]).
    pub clock_pos: f64,
    pub clock_qpc_hns: f64,
    /// The direct driver route: the recording app's packet the block was fed to (the ring's reader's, in frames), and the
    /// headroom over it in the target (frames: nf-engine `ring_headroom.rs`). Both 0 on the WASAPI route.
    pub packet: u32,
    pub headroom: u32,
}

impl Default for OutputFacts {
    fn default() -> Self {
        Self {
            index: 0,
            padding: 0,
            pending: 0,
            fill: 0.0,
            target: 0,
            slip: 0,
            zeros: 0,
            dropped: 0,
            skipped: false,
            underrun: false,
            late: false,
            reanchored: false,
            silence: 0,
            clock_pos: f64::NAN,
            clock_qpc_hns: f64::NAN,
            packet: 0,
            headroom: 0,
        }
    }
}

/// What the audio thread itself did around one block ([`ChainBlock::thread`]; engine.md, "The flight recorder"): when it woke,
/// how long since it had woken before, the waits that timed out and the mic's glitches since the block before, and how long
/// the block took it. With the mic's delivery ([`ChainBlock::mic_wake_hns`], [`ChainBlock::mic_queued`]) and the output's
/// padding they tell a thread that was held up (a long gap between wakes, the mic's samples waiting when it woke) from a
/// device that stopped delivering (wakes on time, or waits timing out, and no packets) from an output that ran dry (the thread
/// and the mic on time, the device's padding gone). A few stores a block, always (the block's time is the clock reading its
/// timing takes anyway).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ThreadFacts {
    /// The QPC time (100 ns units) of the wake that processed the block; 0 when unknown.
    pub wake_hns: f64,
    /// The longest time between two of the thread's wakes since the block before's wake, the one that ended at this block's
    /// included, in ms; 0 for a later block of the same wake.
    pub wake_gap_ms: f32,
    /// The thread's wakes since the block before's, this block's own included: 0 for a later block of the same wake, more
    /// than 1 when wakes in between brought less than a block.
    pub wakes: u32,
    /// Of those wakes, the ones an exclusive output's event made (reported first when both have fired). Always 0 with a
    /// shared output: the thread then waits on the mic's events alone, and a thread held up can't be told from a mic's driver
    /// that held its samples back and delivered them at once.
    pub out_wakes: u32,
    /// Stalls since the block before: wakes that found nothing from the mic (neither its event nor a packet) 100 ms or more
    /// after its last (a wait timing out; with an exclusive output's events waking the thread, once in each 100 ms).
    pub stalls: u32,
    /// Mic glitches since the block before: packets flagged discontinuous or missing by device position, and the FIFO
    /// overflowing (telemetry's `capture_glitches`).
    pub mic_glitches: u32,
    /// The block's own time on the audio thread, in µs: from taking it off the mic FIFO to its output written (the chain's
    /// processing included, the recorders' copies not).
    pub process_us: f32,
    /// Blocks a model pack's inference worker had no result for in time (the substitute played) since the block before; 0
    /// without a pack.
    pub model_misses: u32,
}

/// A clock's drift against the mic's, fitted over a chain recording's blocks ([`clock_drift`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftFit {
    /// Positive when the clock runs fast against the mic's (it plays or captures more samples in the mic's second).
    pub ppm: f64,
    /// About two standard errors of `ppm` (a 95% interval), from the fits' residuals: the stamps' scatter, or the
    /// delivery's.
    pub uncertainty_ppm: f64,
    /// The span fitted, in seconds.
    pub seconds: f64,
}

/// How the mic's own clock was read for [`ClockDrift`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicTiming {
    /// By its packets' QPC stamps (`ChainBlock::capture_qpc_hns`).
    Stamps,
    /// By when its packets were delivered (`ChainBlock::mic_wake_hns`): without trusted stamps, noisier.
    Delivery,
}

/// The output's and the reference's clocks against the mic's, measured over a chain recording's blocks rather than read from
/// the drift loops, whose estimates take a minute or more to settle (the output's loop is a PI controller with a natural
/// frequency of about 0.17 rad/s; bursty delivery gives it a start-up transient of hundreds of ppm).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockDrift {
    /// How the mic's clock was read; `None` when it couldn't be (no stamps and no wakes, or too short a span).
    pub mic: Option<MicTiming>,
    /// The output device's clock, from its position readings ([`OutputFacts::clock_pos`]).
    pub output: Option<DriftFit>,
    /// The echo reference's (the speakers' loopback), from its timeline's index and the tap times its stamped lead readings
    /// give ([`ReferenceFacts`]); `None` without a reference or its stamps.
    pub reference: Option<DriftFit>,
}

/// The clocks' drift over `blocks` (a chain recording's, in order; leave out a start-up that shouldn't count). Each clock is
/// a straight line of time against its sample index: the mic's from its stamps (or its delivery), the output's from its
/// position readings, the loopback's from its timeline and stamps; each line is fitted by least squares with its own
/// intercept on each stretch between the steps that break it (a mic position skip, an output underrun or re-anchor, a
/// reference restart), so a glitch moves no slope. Needs three seconds of each; `None` for a clock it can't fit.
pub fn clock_drift(blocks: &[ChainBlock]) -> ClockDrift {
    crate::clock_fit::clock_drift(blocks)
}

/// One block the chain processed, as the chain recorder saw it ([`Engine::record_chain`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChainBlock {
    /// The run's block number: 0 is its first block after the warm-up. Consecutive, unless the recorder's ring was full.
    pub index: u64,
    /// The QPC time (100 ns units) of the mic block's first sample by the mic's packet stamps; NaN without them.
    pub capture_qpc_hns: f64,
    /// The mic's stamps are trusted (they time the reference's lead).
    pub mic_stamped: bool,
    /// The mic stream's index of the block's first sample: the samples pushed into the mic FIFO before it, so frames missing
    /// by device position up to 100 ms count (as the zeros pushed for them) and a bigger gap doesn't ([`Self::mic_skipped`]).
    /// It moves by 480 a block, plus [`Self::mic_discarded`].
    pub mic_index: u64,
    /// The QPC time (100 ns units) of the wake that delivered the mic's newest packet: the block's time by delivery.
    pub mic_wake_hns: f64,
    /// Mic samples still queued after this block: a backlog of whole blocks is a late wake or a burst.
    pub mic_queued: u32,
    /// Mic samples the FIFO discarded, oldest first, since the block before: it overflowed (the engine didn't run for
    /// 200 ms). This block's `mic_index` is that much past the last block's plus 480.
    pub mic_discarded: u64,
    /// Frames the mic's device position skipped since the block before that weren't filled with zeros (a gap over 100 ms;
    /// negative if the position went back). `mic_index` doesn't count them: the echo path moves by them in time, and the
    /// reference's timeline follows by a jump when the stamps show it.
    pub mic_skipped: i64,
    /// What the reference's timeline did at this block; `None` without a reference (or after it failed and was dropped).
    pub reference: Option<ReferenceFacts>,
    /// The echo-path guard's mic hold at this block, in samples (0 without one; engine.md, "The echo-path guard"). The
    /// reference's lead and target in [`Self::reference`] are its loop's, which reads the block as captured this much later:
    /// the lead the echo path sees is theirs less this.
    pub echo_hold: u32,
    /// AEC3's own estimates after this block: the echo delay it found and its ERLE (NaN while it has none, or without AEC).
    pub aec_delay_ms: f32,
    pub aec_erle_db: f32,
    /// The chain's live settings as they stood after this block (on/off, passthrough, floor, gate): an [`Engine::apply`] or a
    /// latency test that changed them shows here, and a rerun with one config matches only while they don't change.
    pub settings: ChainParams,
    /// The block's latency readout split into where its first sample spent the time (engine.md, "The latency budget"): its
    /// `readout_ms()` is the readout. `None` while the readout is off, without the mic's stamps or the output's clock (its
    /// first few ms), or for a block the output dropped.
    pub budget: Option<LatencyBudget>,
    /// What the output side did at this block: where it went in the device's stream, the fill and target, and its slip, zeros,
    /// drops, underrun and re-anchor.
    pub output: OutputFacts,
    /// What the audio thread did around this block: its wake, the gap before it, stalls, mic glitches and model misses since
    /// the block before, and the block's processing time.
    pub thread: ThreadFacts,
}

/// The columns of [`chain_blocks_csv`], comma-separated (engine.md, "The chain recorder" and "The flight recorder"; `quality
/// --save`'s `chain_blocks.csv`).
pub const CHAIN_BLOCK_COLUMNS: &str = crate::csv::COLUMNS;

/// A chain recording's blocks as CSV: [`CHAIN_BLOCK_COLUMNS`], then a row a block. Times are QPC in 100 ns units; a number
/// that's unknown is empty, and so are the reference's columns without a reference and the budget's without a budget.
pub fn chain_blocks_csv(blocks: &[ChainBlock]) -> String {
    crate::csv::chain_blocks(blocks)
}

/// What [`ChainRecorder::stop`] gives: every block the chain processed while it recorded, the chain's input and output.
/// `mic`, `reference` and `output` are 48 kHz mono, 480 samples for each entry of `blocks`, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChainRecording {
    /// The mic blocks as the chain got them: with the engine's dither (about -180 dBFS) on each block a stage was to run on
    /// (`Chain::runs_a_stage`, read before the block), and as captured, undithered, in bypass and in measurement passthrough
    /// (TODO 10).
    pub mic: Vec<f32>,
    /// The reference blocks as the chain got them: the timeline's samples resampled to the block, dithered.
    pub reference: Vec<f32>,
    /// The chain's output blocks, before the output's drift loop resamples them for the device.
    pub output: Vec<f32>,
    pub blocks: Vec<ChainBlock>,
    /// Blocks the recorder's ring had no room for, up to the end of the recording (the audio thread counts them): missing from
    /// it, not zero-filled. The indices skip those before the last block recorded.
    pub lost_blocks: u64,
    /// Stopped at its length limit.
    pub truncated: bool,
    /// The run ended (stopped, or restarted after a fault, a device change or a settings change that rebuilds the streams)
    /// before the recorder was stopped: the recording ends with it.
    pub run_ended: bool,
}

impl ChainRecording {
    /// The recording starts at the run's first block, after the engine's warm-up: armed before the start.
    pub fn from_first_block(&self) -> bool {
        self.blocks.first().is_some_and(|b| b.index == 0)
    }

    /// The first block whose chain settings differ from the block before's ([`ChainBlock::settings`]): an [`Engine::apply`] or
    /// a latency test changed them during the recording.
    pub fn settings_changed_at(&self) -> Option<u64> {
        self.blocks.windows(2).find(|w| !w[1].settings.same(&w[0].settings)).map(|w| w[1].index)
    }

    /// The chain's whole history is here: from the run's first block, none lost, the settings unchanged. Then a chain started
    /// as the engine starts it ([`warm_up_chain`]) with those settings and fed `mic` and `reference` block by block
    /// reproduces `output` (`noise-filter process --warm-up` with the run's processing settings).
    pub fn reproducible(&self) -> bool {
        !self.blocks.is_empty() && self.from_first_block() && self.lost_blocks == 0 && self.settings_changed_at().is_none()
    }

    /// The recording's length in seconds.
    pub fn seconds(&self) -> f64 {
        self.mic.len() as f64 / f64::from(nf_core::SAMPLE_RATE)
    }
}

/// A chain recording in progress ([`Engine::record_chain`]). Its own thread moves the audio thread's blocks into memory
/// while it runs. Dropping it stops it and discards what it recorded.
pub struct ChainRecorder {
    pub(crate) inner: crate::recorder::Handle,
}

impl ChainRecorder {
    /// Blocks recorded so far.
    pub fn blocks(&self) -> u64 {
        self.inner.blocks()
    }

    /// Blocks lost so far because the recorder's ring was full.
    pub fn lost_blocks(&self) -> u64 {
        self.inner.lost_blocks()
    }

    /// A copy of the blocks recorded so far from the `from`th on (0 is the recording's first block; none past its end), as
    /// [`ChainRecorder::stop`] would give them, with its counts and flags as they stand: a look at the recording while it
    /// runs (`noise-filter quality` times its echo test's marker so). About 6 KB a block; the recording carries on.
    pub fn copy_from(&self, from: usize) -> ChainRecording {
        self.inner.copy_from(from)
    }

    /// Stops recording (the recording ends at about this moment) and returns it. It waits up to about half a second for
    /// the audio thread to hand the ring back.
    pub fn stop(self) -> ChainRecording {
        self.inner.stop()
    }
}

/// The flight recorder's audio when it's kept, in seconds before a trigger: `run --flight-audio`'s (about 0.6 MB a second
/// in memory, the post-roll's included).
pub const FLIGHT_AUDIO_S: f64 = 5.0;

/// The flight recorder's settings ([`Engine::arm_flight_recorder`]; engine.md, "The flight recorder"). [`FlightConfig::new`]
/// gives the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct FlightConfig {
    /// Where the dumps go, a folder each (`flight-<local time>-<trigger>`); made if it isn't there.
    pub dir: std::path::PathBuf,
    /// The per-block facts kept before a trigger, in seconds (1-60; default 10).
    pub facts_s: f64,
    /// The audio kept before a trigger (the chain's mic, reference and output, as the chain recorder keeps them), in seconds
    /// (0-30); 0, the default, keeps none.
    pub audio_s: f64,
    /// How long a dump waits after its trigger, recording, before it's written, in seconds (0.1-10; default 3).
    pub post_roll_s: f64,
    /// The least time between two dumps for the same kind of trigger (the output, the mic, the audio thread, the echo
    /// reference, the model, a fault), in seconds (0-3600; default 60). Triggers in between are counted, not dumped.
    pub min_interval_s: f64,
    /// The most dumps one arming writes for each kind of trigger (at least 1; default 3), so a kind that keeps misbehaving
    /// (the second soak's loopback, for minutes) can't use up the dumps another needs later.
    pub max_dumps_per_kind: u32,
    /// The most dumps one arming writes in all (at least 1; default 20: with the defaults, more than the six kinds' 3 each
    /// can use, so it bounds the disk without crowding a kind out).
    pub max_dumps: u32,
    /// The most dump folders kept in `dir`: after each dump the oldest beyond this are deleted (at least 1; default 20). Only
    /// folders named as the recorder names them, with a `summary.txt`, are touched.
    pub keep_dumps: usize,
}

impl FlightConfig {
    /// The defaults, writing into `dir`: 10 s of facts before a trigger, no audio, 3 s after it, at most one dump a minute
    /// and 3 an arming for each kind of trigger, 20 in all, and the newest 20 dumps kept in `dir`.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            facts_s: 10.0,
            audio_s: 0.0,
            post_roll_s: 3.0,
            min_interval_s: 60.0,
            max_dumps_per_kind: 3,
            max_dumps: 20,
            keep_dumps: 20,
        }
    }
}

/// One dump the flight recorder wrote ([`FlightStats::dumps`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FlightDump {
    /// The dump's folder: `flight_blocks.csv`, `summary.txt`, and the WAVs when audio was kept.
    pub path: std::path::PathBuf,
    /// What triggered it, in words ("an output underrun"): the most serious of the triggers that came together (the output's,
    /// then the mic's, the thread's, a fault, the reference's, the model's).
    pub trigger: String,
    /// The other triggers it holds, by their names (`output-deficit`, `reference-short`: engine.md lists them), once each:
    /// those at the same blocks, and in its post-roll.
    pub also: Vec<&'static str>,
    /// When, in seconds after the recorder was armed, and by the wall clock (local time, as the app's log has it).
    pub at_s: f64,
    pub local_time: String,
    /// The summary's reading of the facts around it, in one sentence.
    pub verdict: String,
}

/// What a flight recorder has done so far ([`FlightRecorder::stats`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlightStats {
    /// The dumps written, in order.
    pub dumps: Vec<FlightDump>,
    /// Triggers that made no dump of their own: within `min_interval_s` of their kind's last dump, past its
    /// `max_dumps_per_kind`, or past `max_dumps`. (Those in a pending dump's post-roll of a kind it holds, or within half a
    /// second of its trigger, are in that dump.)
    pub suppressed: u64,
    /// The same by kind, those with any, in words ("the echo reference", 94).
    pub suppressed_by: Vec<(&'static str, u64)>,
    /// Blocks the recorder has seen, and blocks lost because its ring was full.
    pub blocks: u64,
    pub lost_blocks: u64,
    /// Dumps that couldn't be written, worded.
    pub errors: Vec<String>,
}

/// An armed flight recorder ([`Engine::arm_flight_recorder`]). Its own thread keeps the last seconds of every block's facts
/// (and audio, if asked) and writes them out when something goes wrong. Dropping it stops it, as [`FlightRecorder::stop`]
/// does.
pub struct FlightRecorder {
    pub(crate) inner: crate::flight::Handle,
}

impl FlightRecorder {
    /// The dumps and counts so far (the dump being written may not be in yet).
    pub fn stats(&self) -> FlightStats {
        self.inner.stats()
    }

    /// Stops it: the triggers still in their post-rolls are dumped now, with what there is. It waits for those writes, and up
    /// to about half a second for the audio thread to hand the live run's ring back.
    pub fn stop(self) -> FlightStats {
        self.inner.stop()
    }
}

/// Runs the engine's start-up warm-up on `chain`: three silent blocks, dithered as the engine dithers, through the chain on
/// the calling thread. The engine does this before every run's first block (after the stages' first-block set-up), so a
/// chain built and activated as the engine builds it (`models::build_chain` with the run's processing settings), warmed
/// up with this and fed a [`ChainRecording`]'s `mic` and `reference` reproduces its `output`.
pub fn warm_up_chain(chain: &mut nf_core::chain::Chain) {
    crate::audio::warm_up(chain)
}

/// Lists the active capture and render endpoints.
pub fn list_devices() -> Result<Vec<DeviceInfo>, EngineError> {
    crate::devices::list_devices()
}

/// Runs the cable-only latency test on the calling thread and blocks until it's done (about 2 s a repetition). It needs no
/// [`Engine`]: COM (MTA) is initialised on this thread for the test, and the thread runs at MMCSS "Pro Audio" while streaming.
/// Call it from a worker thread: on an STA (UI) thread COM is single-threaded already, and it fails.
pub fn cable_latency_test(request: &CableLatencyRequest<'_>) -> Result<CableLatencyReport, EngineError> {
    crate::latency_test::cable_latency_test(request)
}

/// Plays the tracks and records the capture endpoint meanwhile, on the calling thread, blocking until the longest track and
/// the tail have played. COM (MTA) is initialised on this thread for it, and the thread runs at MMCSS "Pro Audio" while
/// streaming. Call it from a worker thread: on an STA (UI) thread COM is single-threaded already, and it fails. A running
/// [`Engine`] carries on meanwhile, which is how `noise-filter quality` measures it.
pub fn play_record(request: &PlayRecordRequest<'_>) -> Result<PlayRecordResult, EngineError> {
    crate::play_record::play_record(request)
}

/// The engine. `Send + Sync`: the UI holds it in an `Arc` and calls it from its own thread. Every
/// method returns quickly except `start`, which waits up to a few seconds for the streams to open.
pub struct Engine {
    pub(crate) inner: Arc<crate::control::Inner>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An idle engine (no streams open).
    pub fn new() -> Self {
        Self { inner: crate::control::Inner::new() }
    }

    /// Opens the streams and starts processing with `config`. Stops a running engine first.
    pub fn start(&self, config: &EngineConfig) -> Result<(), EngineError> {
        crate::control::start(&self.inner, config)
    }

    /// Stops processing and closes the streams.
    pub fn stop(&self) {
        crate::control::stop(&self.inner)
    }

    /// Applies a changed config: processing changes take effect at the next block and never restart
    /// anything; device, I/O or AEC-on changes restart the streams if running (a run of changes,
    /// such as a slider drag, makes one restart). While stopped by a fault and retrying, only
    /// device, I/O or AEC changes trigger an early retry.
    pub fn apply(&self, config: &EngineConfig) {
        crate::control::apply(&self.inner, config)
    }

    /// The external effect in the active chain, including any blocks that fell back to aligned dry audio.
    pub fn effect_info(&self) -> Option<EffectInfo> {
        crate::control::lock(&self.inner.effect).as_ref().map(crate::effect::LoadedEffect::snapshot)
    }

    pub fn telemetry(&self) -> TelemetrySnapshot {
        self.inner.telemetry.snapshot()
    }

    pub fn status(&self) -> EngineStatus {
        crate::control::status(&self.inner)
    }

    /// The output and reference fill targets of the running streams (compare with
    /// `TelemetrySnapshot::output_fill_ms` and `reference_fill_ms`).
    pub fn buffer_targets(&self) -> BufferTargets {
        crate::control::buffer_targets(&self.inner)
    }

    /// Clears the input clip latch.
    pub fn reset_clip(&self) {
        self.inner.telemetry.reset_clip()
    }

    /// Whether the raw input clipped since the latch was last cleared (or taken), clearing it: a console line a second shows
    /// each clip once (TODO 12).
    pub fn take_clip(&self) -> bool {
        self.inner.telemetry.take_clip()
    }

    /// Starts a latency test. The engine must be running, with the mic set to the loop's input.
    pub fn start_latency_test(&self, request: LatencyTestRequest) -> Result<LatencyTest, EngineError> {
        crate::latency_test::start(&self.inner, request, 0)
    }

    /// [`Self::start_latency_test`] with its capture opened at IAudioClient3's `capture_period_frames`, shared, in the
    /// endpoint's DEFAULT mode (0: its default period, as `start_latency_test`). On the direct driver route the test's
    /// capture is the recording app that reads the ring, so this sets the reader's packet: a short one (128 or 160 frames)
    /// needs the capture to ask for it itself (TODO 13). The capture endpoint must take the period: the test fails, naming
    /// the periods it takes, where it doesn't.
    pub fn start_latency_test_at(&self, request: LatencyTestRequest, capture_period_frames: u32) -> Result<LatencyTest, EngineError> {
        crate::latency_test::start(&self.inner, request, capture_period_frames)
    }

    /// Arms the chain recorder (engine.md, "The chain recorder"): every block the chain processes, its mic and reference
    /// input as the chain gets them and its output, with the block's capture time and what the reference's timeline did,
    /// for up to `max_seconds` (at most [`MAX_CHAIN_RECORDING_S`]). About 0.6 MB a second.
    ///
    /// Armed before [`Engine::start`] (or while the engine is stopped, or waiting to retry), the next run that starts records
    /// from its first block, after the warm-up, so the recording holds the chain's whole history
    /// ([`ChainRecording::reproducible`]). A start that fails hands it on to the next (unless it got no audio thread, or the
    /// thread hung or died: then it records nothing more). Armed while running, the live run starts recording at its next
    /// wake. The recording ends with the run: a stop, or a restart (a fault, a device change, a settings change that rebuilds
    /// the streams). One recorder at a time.
    pub fn record_chain(&self, max_seconds: f64) -> Result<ChainRecorder, EngineError> {
        crate::recorder::arm(&self.inner, max_seconds).map(|inner| ChainRecorder { inner })
    }

    /// Arms the flight recorder (engine.md, "The flight recorder"): the last `facts_s` seconds of every block's facts (and
    /// `audio_s` of its audio) kept in memory, and written into `dir` when something goes wrong: an output underrun, deficit
    /// or coarse drop; a mic stall, glitch, skip or overflow; the audio thread waking late or taking too long on a block; an
    /// echo reference that jumps, runs short in a burst or is lost; a model pack's miss; or a fault that stops the audio. Each
    /// dump waits `post_roll_s` after its trigger and holds a CSV of the blocks, a summary in words, and the audio if kept.
    ///
    /// Unlike the chain recorder it follows the engine across its restarts (each run gets a ring of its own) and keeps
    /// recording after each dump, until stopped. Armed before [`Engine::start`] it records from the first run's first block;
    /// armed while running, from the next wake. On the audio thread it costs what the chain recorder costs: one block's
    /// facts and audio copied into a pre-allocated ring a block (about 6 KB). One flight recorder at a time; it runs beside a
    /// chain recorder.
    pub fn arm_flight_recorder(&self, config: FlightConfig) -> Result<FlightRecorder, EngineError> {
        crate::flight::arm(&self.inner, config).map(|inner| FlightRecorder { inner })
    }
}
