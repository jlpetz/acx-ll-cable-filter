//! nf-engine's public API: the only surface the GUI and the app use. Stable contract; the
//! implementation behind it lives in the crate's private modules.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use nf_core::config::{DeviceSelection, EngineConfig, GateConfig, GateMode, IoConfig, ProcessingConfig};
pub use nf_core::telemetry::{EngineState, MeterSnapshot, TelemetrySnapshot};

/// Endpoint direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flow {
    Capture,
    Render,
}

/// One active audio endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// WASAPI endpoint id (stable across reboots); what [`DeviceSelection`] stores.
    pub id: String,
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
#[derive(Clone, Debug, PartialEq)]
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
    /// Stopped by a fault but armed: the engine retries by itself (timed backoff and device events).
    pub retrying: bool,
    /// What the user can do about the error in `message`, when the app can take them there.
    pub remedy: Option<Remedy>,
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
    /// Output render-buffer target: one render period + one block + `io.output_margin_ms`.
    pub output_ms: f32,
    /// Reference FIFO target: one loopback period + one block (0 without a reference).
    pub reference_ms: f32,
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
    /// The live readout (layer 1) during the test, for comparison.
    pub readout_ms_median: Option<f32>,
    /// The chain's own stream offset during the test (passthrough: the enabled stages' latency).
    pub chain_ms: f32,
    /// Lowest arrival confidence among valid repetitions, in dB.
    pub confidence_db_min: f32,
    /// Engine periods, RAW, autoconvert and the devices used, for the record.
    pub conditions: String,
    pub warnings: Vec<String>,
}

/// A running latency test's progress.
#[derive(Clone, Debug, PartialEq)]
pub enum LatencyProgress {
    Running { fraction: f32, stage: &'static str },
    Done(LatencyReport),
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
    /// 48 kHz mono in ±1.
    pub samples: &'a [f32],
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

    /// Starts a latency test. The engine must be running, with the mic set to the loop's input.
    pub fn start_latency_test(&self, request: LatencyTestRequest) -> Result<LatencyTest, EngineError> {
        crate::latency_test::start(&self.inner, request)
    }
}
