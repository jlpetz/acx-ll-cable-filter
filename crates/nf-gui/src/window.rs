//! The settings window (gui.md): one scrollable column, top to bottom: the header (title, status,
//! Start/Stop), the InfoBar, devices, processing, the live section (its own component, `live.rs`),
//! the latency test, and diagnostics.
//!
//! - **Controlled controls.** `update` stores every control's new value, or Reactor writes the old
//!   one back (windows-reactor.md 3.6). A changed engine config is applied at once
//!   (`Engine::apply` is cheap) and saved, debounced, by the settings thread.
//! - **Blocking calls** (`start`, `stop`, `list_devices`, `start_latency_test`, the elevated
//!   large-page setup) run in `spawn_background`, so [`Msg`] is `Send`. Their results come back as
//!   messages; an engine error becomes the InfoBar's text, never a panic.
//! - **The feed.** One re-armed waiter (tmr-gui's pattern) brings the engine's status, the running
//!   latency test's progress and any save error: at 2 Hz, or 5 Hz while a test runs. The 20 Hz
//!   telemetry is the live section's own waiter, so it doesn't re-render this whole view.
//! - **Two InfoBars.** The notice (errors and results; a fault that stops a run, once per fault)
//!   and, under it, the engine's warnings while it runs, which closes by itself when they clear.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nf_engine::{
    DeviceInfo, EffectConfig, EffectInfo, Engine, EngineConfig, EngineError, EngineState, EngineStatus, GateMode, LatencyProgress,
    LatencyReport, LatencyTest, LatencyTestRequest, OutputBackend, Remedy,
};
use nf_mem::privilege::{HELPER_NOT_ELEVATED, helper_args};
use windows_reactor::*;

use crate::devices::{self, DeviceChoices, Picker};
use crate::live::{Live, LiveInput};
use crate::settings::{
    ATTACK_MS, EXCLUSIVE_FALLBACK, FLOOR_DB, HOLD_MS, IoSwitch, MARGIN_DB, MAX_REPETITIONS, MIN_HEIGHT, MIN_REPETITIONS, MIN_WIDTH,
    OUTPUT_MARGIN_MS, RANGE_DB, RELEASE_MS, SaverHandle, Settings, THRESHOLD_DB, TestMode,
};
use crate::widgets::{
    MAX_CONTENT, PAGE_MARGIN, SECTION_SPACING, card, combo, described_switch, device_combo, secondary, selectable, slider, switch,
    track_width,
};
use crate::{SharedEngine, format, screen, waiting};

mod effect;

/// The status waiter's period: the status line and the diagnostics change slowly.
const STATUS_TICK: Duration = Duration::from_millis(500);
/// While a latency test runs, so its progress bar moves smoothly enough.
const TEST_TICK: Duration = Duration::from_millis(200);
/// The `rejected` result of a one-shot job: Reactor's task limit or queue was full.
const NOT_RUN: &str = "The window was too busy to start that. Try again.";

const GATE_MODES: [&str; 3] = ["Off", "Static", "Auto"];

fn gate_index(mode: GateMode) -> usize {
    match mode {
        GateMode::Off => 0,
        GateMode::Static => 1,
        GateMode::Auto => 2,
    }
}

fn gate_mode(index: usize) -> Option<GateMode> {
    match index {
        0 => Some(GateMode::Off),
        1 => Some(GateMode::Static),
        2 => Some(GateMode::Auto),
        _ => None,
    }
}

/// What [`crate::run`] hands the window.
#[derive(Clone, PartialEq)]
pub(crate) struct Start {
    pub engine: SharedEngine,
    pub settings: Settings,
    /// Why the settings are the defaults (an unreadable file), for the InfoBar.
    pub warning: Option<String>,
    pub saver: SaverHandle,
}

/// A start or stop under way on the thread pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Starting,
    Stopping,
}

/// What the status waiter brings.
pub(crate) struct Feed {
    status: EngineStatus,
    effect: Option<EffectInfo>,
    /// The running test's generation and progress, when a test was running as the waiter was armed.
    test: Option<(u64, LatencyProgress)>,
    save_error: Option<String>,
}

pub(crate) enum Msg {
    // Results from the thread pool
    Feed(Box<Feed>),
    Started(Result<(), EngineError>),
    Stopped,
    Devices(Result<Vec<DeviceInfo>, EngineError>),
    TestStarted(Result<LatencyTest, EngineError>),
    LargePages(Result<u32, String>),
    AllowDriver,
    DriverAllowed(Result<(), String>),
    // Header and devices
    StartStop,
    Refresh,
    Mic(Option<usize>),
    OutputBackend(Option<usize>),
    Output(Option<usize>),
    Reference(Option<usize>),
    // Processing
    Noise(bool),
    Aec(bool),
    Floor(f64),
    GateMode(Option<usize>),
    Threshold(f64),
    Margin(f64),
    Attack(f64),
    Release(f64),
    Hold(f64),
    Range(f64),
    Vad(bool),
    ChooseEffect,
    EffectChosen(Result<Option<PathBuf>, String>),
    RemoveEffect,
    BypassEffect(bool),
    EffectParam {
        path: PathBuf,
        plugin_id: String,
        id: u32,
        value: Option<f64>,
    },
    // The live section's toggles (its callbacks)
    ShowMeters(bool),
    ShowReadout(bool),
    // Latency test
    TestMode(Option<usize>),
    Probe(Option<usize>),
    Capture(Option<usize>),
    Repetitions(Option<f64>),
    /// Run, or cancel the running test.
    RunTest,
    // Diagnostics
    /// One of the I/O switches (the latency options, and the streams' others).
    Io(IoSwitch, bool),
    OutputMargin(f64),
    EnableLargePages,
    // Window
    CloseNotice,
    CloseWarnings,
    /// Open the Settings page that fixes the engine's error.
    OpenRemedy(Remedy),
    Size(WindowSize),
}

/// The InfoBar's content.
#[derive(Clone, Debug, PartialEq)]
struct Notice {
    severity: InfoBarSeverity,
    title: &'static str,
    message: String,
    /// About the engine not running (a start error, or a fault that stopped a run): cleared once
    /// it runs again, or the user stops it.
    about_run: bool,
}

impl Notice {
    fn new(severity: InfoBarSeverity, title: &'static str, message: impl Into<String>) -> Self {
        Self { severity, title, message: message.into(), about_run: false }
    }

    /// A [`Notice::about_run`] error.
    fn about_run(title: &'static str, message: impl Into<String>) -> Self {
        Self { about_run: true, ..Self::new(InfoBarSeverity::Error, title, message) }
    }
}

/// Whether Start/Stop stops: the engine runs, starts, or is armed to restart by itself after a
/// fault (gui/F8). Offering "Start" then would leave it to grab the devices again on its own.
fn stops(status: &EngineStatus) -> bool {
    matches!(status.state, EngineState::Running | EngineState::Starting) || status.retrying
}

/// Brings runtime faults to the InfoBar (gui/F10): a run stopped by a fault (a device unplugged, a
/// stream invalidated) is otherwise only the status line's secondary text. Raised once per fault,
/// not again for each failed retry or after the user closed it, and cleared when the engine runs
/// again.
#[derive(Debug, Default)]
struct FaultWatch {
    /// The current fault has its notice (or a start error is showing for it).
    raised: bool,
}

impl FaultWatch {
    /// Takes a new status. `job` is a start or stop under way: its own result is reported instead.
    fn observe(&mut self, status: &EngineStatus, job: Option<Job>, notice: &mut Option<Notice>) {
        let faulted = status.state == EngineState::Error || (status.state == EngineState::Stopped && status.retrying);
        if status.state == EngineState::Running {
            self.raised = false;
            // Recovered: by itself (a retry), or by a start
            if notice.as_ref().is_some_and(|n| n.about_run) {
                *notice = None;
            }
        } else if faulted {
            if job.is_none() && !self.raised {
                self.raised = true;
                *notice = Some(Notice::about_run("The filter stopped", format::fault(status)));
            }
        } else if status.state == EngineState::Stopped {
            // Stopped for good (the user's stop): the next fault is a new one
            self.raised = false;
        }
    }

    /// A start failed and its error is showing: the fault it leaves isn't raised again.
    fn start_failed(&mut self) {
        self.raised = true;
    }
}

/// The warnings InfoBar: the engine's warnings and notes while it runs ([`format::warnings`]).
/// Closing it hides that list; a changed list shows again, and so does the same one in a later
/// run.
#[derive(Debug, Default)]
struct WarningsBar {
    /// The list the user closed the bar on: the warnings, then the notes.
    dismissed: Vec<String>,
}

/// The engine's standing warnings, then its notes.
fn standing(status: &EngineStatus) -> Vec<String> {
    [status.warnings.as_slice(), status.notes.as_slice()].concat()
}

impl WarningsBar {
    fn observe(&mut self, status: &EngineStatus) {
        if status.state != EngineState::Running || (status.warnings.is_empty() && status.notes.is_empty()) {
            self.dismissed.clear();
        }
    }

    fn dismiss(&mut self, status: &EngineStatus) {
        self.dismissed = standing(status);
    }

    /// The bar's tone, title and text, when it's open.
    fn shown(&self, status: &EngineStatus) -> Option<(format::Tone, &'static str, String)> {
        format::warnings(status).filter(|_| standing(status) != self.dismissed)
    }
}

/// The device pickers' items, rebuilt whenever the device list or a stored device changes.
#[derive(Clone, Debug, Default, PartialEq)]
struct Choices {
    mic: DeviceChoices,
    output: DeviceChoices,
    reference: DeviceChoices,
    probe: DeviceChoices,
    capture: DeviceChoices,
}

impl Choices {
    fn new(list: &[DeviceInfo], settings: &Settings) -> Self {
        let d = &settings.engine.devices;
        let ui = &settings.ui;
        Self {
            mic: devices::choices(list, Picker::MIC, d.mic.as_deref()),
            output: devices::choices(list, Picker::OUTPUT, d.output.as_deref()),
            reference: devices::choices(
                list,
                Picker::reference(if settings.engine.io.output_backend == OutputBackend::Wasapi { d.output.as_deref() } else { None }),
                d.reference.as_deref(),
            ),
            probe: devices::choices(list, Picker::PROBE, ui.test_probe.as_deref()),
            capture: devices::choices(list, Picker::CAPTURE, ui.test_capture.as_deref()),
        }
    }
}

/// How a finished test ended.
#[derive(Clone, Debug, PartialEq)]
enum TestOutcome {
    Done(Box<LatencyReport>),
    Failed(String),
    Cancelled,
}

/// The latency test panel's state. Generic over the test's handle so the tests can drive it
/// without an engine.
struct TestPanel<H = LatencyTest> {
    /// `start_latency_test` is in flight.
    starting: bool,
    running: Option<H>,
    /// Counts the tests started, so progress read from an earlier one is ignored.
    generation: u64,
    fraction: f32,
    stage: &'static str,
    cancelling: bool,
    outcome: Option<TestOutcome>,
}

impl<H> Default for TestPanel<H> {
    fn default() -> Self {
        Self { starting: false, running: None, generation: 0, fraction: 0.0, stage: "", cancelling: false, outcome: None }
    }
}

impl<H> TestPanel<H> {
    fn busy(&self) -> bool {
        self.starting || self.running.is_some()
    }

    fn started(&mut self, result: Result<H, EngineError>) {
        self.starting = false;
        match result {
            Ok(test) => {
                self.generation += 1;
                self.running = Some(test);
                self.fraction = 0.0;
                self.stage = "Starting";
                self.cancelling = false;
                self.outcome = None;
            }
            Err(e) => self.outcome = Some(TestOutcome::Failed(e.0)),
        }
    }

    /// Takes a progress read for test `generation`; a stale one (an earlier test's) is dropped.
    fn progress(&mut self, generation: u64, progress: LatencyProgress) {
        if generation != self.generation || self.running.is_none() {
            return;
        }
        let outcome = match progress {
            LatencyProgress::Running { fraction, stage } => {
                self.fraction = fraction;
                self.stage = stage;
                return;
            }
            LatencyProgress::Done(report) => TestOutcome::Done(report),
            LatencyProgress::Failed(message) => TestOutcome::Failed(message),
            LatencyProgress::Cancelled => TestOutcome::Cancelled,
        };
        self.running = None;
        self.cancelling = false;
        self.outcome = Some(outcome);
    }
}

/// Why "Run test" is disabled, if it is.
fn test_blocker(state: EngineState, job: Option<Job>, probe: Option<&str>, capture: Option<&str>) -> Option<&'static str> {
    if state != EngineState::Running || job.is_some() {
        Some("Start the filter first: the test measures the running chain.")
    } else if probe.is_none() || capture.is_none() {
        Some("Choose where to play the probe and where to capture the app's output.")
    } else {
        None
    }
}

/// The ProgressBar's value: 0..=1, never NaN.
fn progress_value(fraction: f32) -> f64 {
    if fraction.is_nan() { 0.0 } else { f64::from(fraction.clamp(0.0, 1.0)) }
}

/// "Playing sweeps… 40%" under the progress bar.
fn stage_line(stage: &str, fraction: f32) -> String {
    let stage = if stage.trim().is_empty() { "Running" } else { stage };
    format!("{stage}… {}%", format::fixed(progress_value(fraction) * 100.0, 0))
}

/// The elevated large-page setup: `noise-filter.exe setup-large-pages --sid <this user>
/// --elevated-helper`, behind a UAC prompt. Blocks until the user answers and the helper exits.
fn setup_large_pages() -> Result<u32, String> {
    let sid = nf_mem::privilege::current_user_sid()?;
    nf_mem::privilege::run_elevated(&helper_args(&sid))
}

/// The fix to offer for the engine's error: only while it isn't running (a start or a run that failed).
fn remedy_offered(status: &EngineStatus) -> Option<Remedy> {
    status.remedy.filter(|_| !matches!(status.state, EngineState::Running | EngineState::Starting))
}

/// Opens a Settings page (an `ms-settings:` URI) through the shell.
fn open_settings(uri: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, w};
    let target = HSTRING::from(uri);
    // SAFETY: plain arguments; `target` and the literals are NUL-terminated and outlive the call.
    let code = unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL) }.0 as isize;
    // ShellExecute returns a value above 32 on success, an error code otherwise.
    if code > 32 {
        Ok(())
    } else {
        Err(format!("Windows couldn't open {uri} (error {code}). Open Settings > Privacy & security > Microphone yourself."))
    }
}

/// The notice for a helper that exited with `code` (not 0).
fn helper_failed(code: u32) -> String {
    match code {
        HELPER_NOT_ELEVATED => "The setup started without administrator rights (is UAC turned off?). Run \
             `noise-filter setup-large-pages` from an administrator prompt instead."
            .to_owned(),
        code => format!("The elevated setup exited with code {code}."),
    }
}

pub(crate) struct Window {
    engine: Arc<Engine>,
    saver: SaverHandle,
    settings: Settings,
    /// What was last handed to the saver, so an unchanged value isn't saved again.
    saved: Settings,
    /// The declared client size, constant for the window's life: a changed declaration resizes the
    /// window (windows-reactor.md 1.3). The remembered size, fitted to the screen at start-up.
    client: (f64, f64),
    /// The largest client size that fits the primary monitor's work area, if known
    /// ([`screen::primary_limit`]).
    limit: Option<(f64, f64)>,
    /// The window's latest width, for the meters' track.
    width: f64,
    status: EngineStatus,
    effect: Option<EffectInfo>,
    choosing_effect: bool,
    job: Option<Job>,
    faults: FaultWatch,
    warnings: WarningsBar,
    /// `None` until the first list arrives.
    devices: Option<Vec<DeviceInfo>>,
    refreshing: bool,
    choices: Choices,
    test: TestPanel,
    elevating: bool,
    granting_driver: bool,
    elevated: bool,
    large_pages_set_up: bool,
    notice: Option<Notice>,
    /// The status waiter in flight.
    feed: Option<ComponentTask>,
}

impl Window {
    /// Arms the status waiter unless one is in flight. Called after every message: a delivered feed
    /// re-arms here, so there's one waiter at a time, and a chain that ended (a rejected spawn)
    /// restarts at the next event rather than never.
    fn ensure_feed(&mut self, context: &ComponentContext<Self>) {
        if waiting(self.feed.as_ref()) {
            return;
        }
        let engine = Arc::clone(&self.engine);
        let errors = self.saver.errors();
        let test = self.test.running.clone().map(|t| (self.test.generation, t));
        let tick = if test.is_some() { TEST_TICK } else { STATUS_TICK };
        self.feed = Some(context.spawn_background(move |cancel| {
            std::thread::sleep(tick);
            if cancel.is_cancelled() {
                // The window has closed (or the task was dropped): the result is discarded, so don't
                // touch an engine that `run` may be stopping.
                return Msg::Feed(Box::new(Feed { status: EngineStatus::default(), effect: None, test: None, save_error: None }));
            }
            let test = test.map(|(generation, t)| (generation, t.progress()));
            Msg::Feed(Box::new(Feed { status: engine.status(), effect: engine.effect_info(), test, save_error: errors.take() }))
        }));
    }

    fn take_feed(&mut self, feed: Feed) {
        self.take_status(feed.status);
        self.effect = feed.effect;
        if self.settings.engine.effect.plugin_id.is_none()
            && let Some(id) = self.active_effect().map(|fx| fx.plugin_id.clone())
        {
            // The engine chose the initial effect. Persist that identity before saving any parameter IDs or reopening a
            // reordered bundle. The executing effect already has this ID, so no control message or restart is needed.
            self.settings.engine.effect.plugin_id = Some(id);
            self.persist();
        }
        if let Some((generation, progress)) = feed.test {
            self.test.progress(generation, progress);
        }
        if let Some(error) = feed.save_error {
            self.notice = Some(Notice::new(InfoBarSeverity::Error, "Settings weren't saved", error));
        }
    }

    /// A new engine status: a fault reaches the InfoBar, and the warnings bar follows.
    fn take_status(&mut self, status: EngineStatus) {
        self.status = status;
        self.faults.observe(&self.status, self.job, &mut self.notice);
        self.warnings.observe(&self.status);
    }

    fn refresh_choices(&mut self) {
        self.choices = match &self.devices {
            Some(list) => Choices::new(list, &self.settings),
            None => Choices::default(),
        };
    }

    fn refresh_devices(&mut self, context: &ComponentContext<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        context.spawn_background_with_rejection(
            |_| Msg::Devices(nf_engine::list_devices()),
            Msg::Devices(Err(EngineError(NOT_RUN.to_owned()))),
        );
    }

    /// A control changed the engine's config: apply it (cheap) and save it.
    fn configure(&mut self, change: impl FnOnce(&mut EngineConfig)) {
        let mut config = self.settings.engine.clone();
        change(&mut config);
        if config == self.settings.engine {
            return;
        }
        let devices_changed =
            config.devices != self.settings.engine.devices || config.io.output_backend != self.settings.engine.io.output_backend;
        self.settings.engine = config;
        self.engine.apply(&self.settings.engine);
        if devices_changed {
            self.refresh_choices();
        }
        self.persist();
    }

    fn persist(&mut self) {
        if self.settings != self.saved {
            self.saved.clone_from(&self.settings);
            self.saver.save(&self.settings);
        }
    }

    fn start_stop(&mut self, context: &ComponentContext<Self>) {
        if self.job.is_some() {
            return;
        }
        let engine = Arc::clone(&self.engine);
        // Retrying counts as on: Stop disarms it (`engine.stop` clears the config it would retry)
        if stops(&self.status) {
            if let Some(test) = &self.test.running {
                test.cancel();
                self.test.cancelling = true;
            }
            self.job = Some(Job::Stopping);
            context.spawn_background_with_rejection(
                move |_| {
                    engine.stop();
                    Msg::Stopped
                },
                Msg::Stopped,
            );
        } else if self.settings.engine.io.output_backend == OutputBackend::DriverRing && !self.elevated && !nf_vmic::broker::authorized() {
            self.notice = Some(Notice::new(
                InfoBarSeverity::Warning,
                "Allow direct output first",
                "Under Devices, choose Allow direct output, approve the driver helper for this session, then press Start.",
            ));
        } else if self.settings.engine.io.output_backend == OutputBackend::Wasapi && self.settings.engine.devices.output.is_none() {
            self.notice = Some(Notice::new(
                InfoBarSeverity::Warning,
                "Choose an output first",
                "Pick the virtual cable your apps use as their mic under Devices, then press Start.",
            ));
        } else {
            self.job = Some(Job::Starting);
            let config = self.settings.engine.clone();
            context.spawn_background_with_rejection(
                move |_| Msg::Started(engine.start(&config)),
                Msg::Started(Err(EngineError(NOT_RUN.to_owned()))),
            );
        }
    }

    fn started(&mut self, result: Result<(), EngineError>) {
        self.job = None;
        match result {
            Ok(()) => {
                if self.notice.as_ref().is_some_and(|n| n.about_run) {
                    self.notice = None;
                }
            }
            Err(e) => {
                self.notice = Some(Notice::about_run("Couldn't start", e.0));
                self.faults.start_failed();
            }
        }
        self.take_status(self.engine.status());
    }

    /// The user's stop has finished.
    fn stopped(&mut self) {
        self.job = None;
        self.take_status(self.engine.status());
        // A fault's notice said the filter would restart by itself: not any more
        if self.status.state == EngineState::Stopped && !self.status.retrying && self.notice.as_ref().is_some_and(|n| n.about_run) {
            self.notice = None;
        }
    }

    fn devices_listed(&mut self, result: Result<Vec<DeviceInfo>, EngineError>) {
        self.refreshing = false;
        match result {
            Ok(list) => {
                if self.settings.bind_cable_endpoints(&list) {
                    self.engine.apply(&self.settings.engine);
                    self.persist();
                }
                self.devices = Some(list);
            }
            Err(e) => {
                self.notice = Some(Notice::new(InfoBarSeverity::Error, "Couldn't list the audio devices", e.0));
                // Unlock the pickers anyway: the stored devices show as missing
                self.devices.get_or_insert_with(Vec::new);
            }
        }
        self.refresh_choices();
    }

    fn run_or_cancel_test(&mut self, context: &ComponentContext<Self>) {
        if let Some(test) = &self.test.running {
            if !self.test.cancelling {
                test.cancel();
                self.test.cancelling = true;
            }
            return;
        }
        let ui = &self.settings.ui;
        if self.test.starting || test_blocker(self.status.state, self.job, ui.test_probe.as_deref(), ui.test_capture.as_deref()).is_some() {
            return;
        }
        let (Some(probe), Some(capture)) = (ui.test_probe.clone(), ui.test_capture.clone()) else {
            return;
        };
        let request = LatencyTestRequest {
            mode: ui.test_mode.engine(),
            probe_device: probe,
            capture_device: capture,
            repetitions: ui.test_repetitions,
            through_model: false,
        };
        self.test.starting = true;
        self.test.outcome = None;
        let engine = Arc::clone(&self.engine);
        context.spawn_background_with_rejection(
            move |_| Msg::TestStarted(engine.start_latency_test(request)),
            Msg::TestStarted(Err(EngineError(NOT_RUN.to_owned()))),
        );
    }

    fn enable_large_pages(&mut self, context: &ComponentContext<Self>) {
        if self.elevating {
            return;
        }
        self.elevating = true;
        context.spawn_background_with_rejection(|_| Msg::LargePages(setup_large_pages()), Msg::LargePages(Err(NOT_RUN.to_owned())));
    }

    fn large_pages_done(&mut self, result: Result<u32, String>) {
        self.elevating = false;
        self.notice = Some(match result {
            Ok(0) => {
                self.large_pages_set_up = true;
                Notice::new(InfoBarSeverity::Success, "Large pages are set up", "Sign out and back in to apply.")
            }
            Ok(code) => Notice::new(InfoBarSeverity::Error, "Large-page setup failed", helper_failed(code)),
            Err(e) => Notice::new(InfoBarSeverity::Error, "Large-page setup didn't run", e),
        });
    }

    /// The window was resized: the meters follow, and the size is remembered for the next start.
    fn resized(&mut self, size: WindowSize) {
        // A minimised window can report a tiny (or no) size: keep only real ones
        if !size.width.is_finite() || !size.height.is_finite() || size.width < MIN_WIDTH - 1.0 || size.height < MIN_HEIGHT - 1.0 {
            return;
        }
        self.width = size.width;
        // A side that fills the screen (maximised, snapped, or the start-up height cut to the
        // screen): the next start would open a restored window that big, so that side keeps the
        // last size the user chose; the other side is remembered (gui/F6)
        let (wide, tall) = screen::fills((size.width, size.height), self.limit);
        let ui = &mut self.settings.ui;
        let width = if wide { ui.window_width } else { size.width.round() };
        let height = if tall { ui.window_height } else { size.height.round() };
        // DPI rounding moves the reported size by a fraction: ignore it rather than save every start
        if (ui.window_width - width).abs() > 2.0 || (ui.window_height - height).abs() > 2.0 {
            ui.window_width = width;
            ui.window_height = height;
            self.settings.sanitize();
            self.persist();
        }
    }
}

impl Component for Window {
    type Input = Start;
    type Message = Msg;

    fn create(start: &Start, context: &ComponentContext<Self>) -> Self {
        let engine = Arc::clone(&start.engine.0);
        let mut settings = start.settings.clone();
        settings.sanitize();
        // The engine starts from the stored settings (it's idle; apply is cheap)
        engine.apply(&settings.engine);
        let status = engine.status();
        // The remembered size can be from a bigger monitor, and the default is too tall for a 1080p
        // laptop at 150%: fit it to the screen (gui/F6). The stored size is kept for a bigger one.
        let limit = screen::primary_limit();
        let client = screen::fit((settings.ui.window_width, settings.ui.window_height), limit);
        let mut window = Self {
            saver: start.saver.clone(),
            saved: settings.clone(),
            client,
            limit,
            width: client.0,
            settings,
            engine,
            status,
            effect: None,
            choosing_effect: false,
            job: None,
            faults: FaultWatch::default(),
            warnings: WarningsBar::default(),
            devices: None,
            refreshing: false,
            choices: Choices::default(),
            test: TestPanel::default(),
            elevating: false,
            granting_driver: false,
            elevated: nf_mem::privilege::is_elevated(),
            large_pages_set_up: false,
            notice: start.warning.clone().map(|w| Notice::new(InfoBarSeverity::Warning, "Settings reset", w)),
            feed: None,
        };
        window.refresh_devices(context);
        window.ensure_feed(context);
        window
    }

    fn update(&mut self, message: Msg, context: &ComponentContext<Self>) {
        match message {
            Msg::Feed(feed) => self.take_feed(*feed),
            Msg::Started(result) => self.started(result),
            Msg::Stopped => self.stopped(),
            Msg::Devices(result) => self.devices_listed(result),
            Msg::TestStarted(result) => self.test.started(result),
            Msg::LargePages(result) => self.large_pages_done(result),
            Msg::AllowDriver if !self.granting_driver => {
                self.granting_driver = true;
                context.spawn_background_with_rejection(
                    |_| Msg::DriverAllowed(nf_vmic::broker::authorize().map_err(|e| e.to_string())),
                    Msg::DriverAllowed(Err(NOT_RUN.to_owned())),
                );
            }
            Msg::AllowDriver => {}
            Msg::DriverAllowed(result) => {
                self.granting_driver = false;
                self.notice = Some(match result {
                    Ok(()) => Notice::new(
                        InfoBarSeverity::Success,
                        "Driver connection approved",
                        "Direct output is available for this app session.",
                    ),
                    Err(e) => Notice::new(InfoBarSeverity::Error, "Couldn't authorize direct output", e),
                });
            }
            Msg::StartStop => self.start_stop(context),
            Msg::Refresh => self.refresh_devices(context),
            // `None` means WinUI cleared the selection (its items changed): keep the stored device,
            // and the next view puts the selection back.
            Msg::Mic(index) => {
                if let Some(id) = index.and_then(|i| self.choices.mic.id_at(i)) {
                    self.configure(|c| c.devices.mic = id);
                }
            }
            Msg::OutputBackend(Some(i)) if !self.test.busy() => match i {
                0 => self.configure(|c| c.io.output_backend = OutputBackend::DriverRing),
                1 => self.configure(|c| c.io.output_backend = OutputBackend::Wasapi),
                _ => {}
            },
            Msg::OutputBackend(_) => {}
            Msg::Output(index) => {
                if let Some(id) = index.and_then(|i| self.choices.output.id_at(i)) {
                    self.configure(|c| c.devices.output = id);
                }
            }
            Msg::Reference(index) => {
                if let Some(id) = index.and_then(|i| self.choices.reference.id_at(i)) {
                    self.configure(|c| c.devices.reference = id);
                }
            }
            Msg::Noise(on) => self.configure(|c| c.processing.noise_enabled = on),
            Msg::Aec(on) => self.configure(|c| c.processing.aec_enabled = on),
            Msg::Floor(v) => self.configure(|c| c.processing.floor_db = FLOOR_DB.clamp(v as f32, c.processing.floor_db)),
            Msg::GateMode(index) => {
                if let Some(mode) = index.and_then(gate_mode) {
                    self.configure(|c| c.processing.gate.mode = mode);
                }
            }
            Msg::Threshold(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.threshold_db = THRESHOLD_DB.clamp(v as f32, g.threshold_db);
            }),
            Msg::Margin(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.auto_margin_db = MARGIN_DB.clamp(v as f32, g.auto_margin_db);
            }),
            Msg::Attack(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.attack_ms = ATTACK_MS.clamp(v as f32, g.attack_ms);
            }),
            Msg::Release(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.release_ms = RELEASE_MS.clamp(v as f32, g.release_ms);
            }),
            Msg::Hold(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.hold_ms = HOLD_MS.clamp(v as f32, g.hold_ms);
            }),
            Msg::Range(v) => self.configure(|c| {
                let g = &mut c.processing.gate;
                g.range_db = RANGE_DB.clamp(v as f32, g.range_db);
            }),
            Msg::Vad(on) => self.configure(|c| c.processing.gate.use_vad = on),
            Msg::ChooseEffect => self.choose_effect(context),
            Msg::EffectChosen(result) => {
                self.choosing_effect = false;
                match result {
                    Ok(Some(path)) if self.settings.engine.effect.path.as_ref() != Some(&path) => {
                        self.configure(|c| c.effect = EffectConfig { path: Some(path), bypass: false, ..Default::default() });
                    }
                    Ok(_) => {}
                    Err(e) => self.notice = Some(Notice::new(InfoBarSeverity::Error, "Couldn't choose an effect", e)),
                }
            }
            Msg::RemoveEffect => self.configure(|c| c.effect = EffectConfig::default()),
            Msg::BypassEffect(on) => self.configure(|c| c.effect.bypass = on),
            Msg::EffectParam { path, plugin_id, id, value } => self.set_effect_param(&path, &plugin_id, id, value),
            Msg::ShowMeters(on) => self.configure(|c| c.processing.meters_enabled = on),
            Msg::ShowReadout(on) => self.configure(|c| c.processing.latency_readout_enabled = on),
            Msg::TestMode(index) => {
                if let Some(mode) = index.and_then(TestMode::from_index) {
                    self.settings.ui.test_mode = mode;
                    self.persist();
                }
            }
            Msg::Probe(index) => {
                if let Some(id) = index.and_then(|i| self.choices.probe.id_at(i)) {
                    self.settings.ui.test_probe = id;
                    self.refresh_choices();
                    self.persist();
                }
            }
            Msg::Capture(index) => {
                if let Some(id) = index.and_then(|i| self.choices.capture.id_at(i)) {
                    self.settings.ui.test_capture = id;
                    self.refresh_choices();
                    self.persist();
                }
            }
            // A cleared box (`None`) keeps the old count, which the next view puts back
            Msg::Repetitions(Some(v)) if v.is_finite() => {
                self.settings.ui.test_repetitions = v.round().clamp(f64::from(MIN_REPETITIONS), f64::from(MAX_REPETITIONS)) as u32;
                self.persist();
            }
            Msg::Repetitions(_) => {}
            Msg::RunTest => self.run_or_cancel_test(context),
            Msg::Io(switch, on) => self.configure(|c| switch.set(&mut c.io, on)),
            Msg::OutputMargin(v) => self.configure(|c| c.io.output_margin_ms = OUTPUT_MARGIN_MS.clamp(v as f32, c.io.output_margin_ms)),
            Msg::EnableLargePages => self.enable_large_pages(context),
            // InfoBar.IsOpen has no feedback: clear it, so the next notice is a change Reactor applies
            Msg::CloseNotice => self.notice = None,
            Msg::OpenRemedy(remedy) => {
                if let Err(e) = open_settings(remedy.settings_uri()) {
                    self.notice = Some(Notice::new(InfoBarSeverity::Error, "Couldn't open Settings", e));
                }
            }
            // Also raised when the bar closes because the warnings cleared: then there's nothing to hide
            Msg::CloseWarnings => {
                if self.warnings.shown(&self.status).is_some() {
                    self.warnings.dismiss(&self.status);
                }
            }
            Msg::Size(size) => self.resized(size),
        }
        self.ensure_feed(context);
    }

    fn view(&self, _start: &Start, context: &mut ViewContext<Self>) -> View {
        context.window_title("noise-filter");
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(WindowTheme::System)
                // Sanitised on load: finite, at least the minimum
                .client_size(self.client.0, self.client.1)
                .constraints(WindowConstraints { min_width: Some(MIN_WIDTH), min_height: Some(MIN_HEIGHT), ..Default::default() }),
        );
        context.on_window_size(context.callback(Msg::Size));
        let page = StackPanel::new()
            .margin(Thickness::new(PAGE_MARGIN, 20.0, PAGE_MARGIN, 28.0))
            .max_width(MAX_CONTENT)
            .spacing(SECTION_SPACING)
            .children((
                self.header(context),
                self.notice_bar(context),
                self.warnings_bar(context),
                self.devices_card(context),
                self.processing_card(context),
                self.effect_card(context),
                View::component::<Live>(self.live_input(context)),
                self.test_section(context),
                self.diagnostics_section(context),
            ));
        ScrollViewer::new().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(page)
    }
}

// ---- view pieces ----------------------------------------------------------------------------

impl Window {
    fn header(&self, context: &ViewContext<Self>) -> View {
        let (label, style) = match (self.job, stops(&self.status)) {
            (Some(Job::Starting), _) => ("Starting…", ButtonStyle::Default),
            (Some(Job::Stopping), _) => ("Stopping…", ButtonStyle::Default),
            (None, true) => ("Stop", ButtonStyle::Default),
            (None, false) => ("Start", ButtonStyle::Accent),
        };
        let status = format::status_line(&self.status, self.job);
        Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(16.0).children((
            StackPanel::new().grid_column(0).spacing(2.0).children((
                TextBlock::new().text("noise-filter").font_size(28.0).font_weight(FontWeight::SEMI_BOLD),
                secondary(status),
                if stops(&self.status) {
                    secondary("Minimize to keep filtering. Closing the window stops the filter.").into()
                } else {
                    View::empty()
                },
            )),
            Button::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .min_width(104.0)
                .style(style)
                .is_enabled(self.job.is_none())
                .on_click(context.callback(|()| Msg::StartStop))
                .content(label),
        ))
    }

    /// The notice, and under it the button for a fix the app can take the user to (Reactor's InfoBar has
    /// no action button of its own).
    fn notice_bar(&self, context: &ViewContext<Self>) -> View {
        let (severity, title, message) = match &self.notice {
            Some(n) => (n.severity, n.title, n.message.clone()),
            None => (InfoBarSeverity::Informational, "", String::new()),
        };
        let remedy = remedy_offered(&self.status);
        StackPanel::new().spacing(8.0).children((
            InfoBar::new()
                .severity(severity)
                .title(title)
                .message(message)
                .is_open(self.notice.is_some())
                .is_closable(true)
                .on_closed(context.callback(|()| Msg::CloseNotice)),
            match remedy {
                Some(r) => Button::new().on_click(context.callback(move |()| Msg::OpenRemedy(r))).content(r.label()),
                None => View::empty(),
            },
        ))
    }

    /// The engine's warnings and notes while it runs; closed when they clear (or the user closes it). Notes alone (a stream
    /// that runs shared, not exclusive) are informational.
    fn warnings_bar(&self, context: &ViewContext<Self>) -> View {
        let shown = self.warnings.shown(&self.status);
        let open = shown.is_some();
        let (tone, title, message) = shown.unwrap_or((format::Tone::Warning, "", String::new()));
        let severity = match tone {
            format::Tone::Warning => InfoBarSeverity::Warning,
            format::Tone::Note => InfoBarSeverity::Informational,
        };
        InfoBar::new()
            .severity(severity)
            .title(title)
            .message(message)
            .is_open(open)
            .is_closable(true)
            .on_closed(context.callback(|()| Msg::CloseWarnings))
            .into()
    }

    fn devices_card(&self, context: &ViewContext<Self>) -> View {
        let loaded = self.devices.is_some();
        // Changing a device mid-test would break the test's loop
        let enabled = loaded && !self.test.busy();
        let ring = self.settings.engine.io.output_backend == OutputBackend::DriverRing;
        let placeholder = |p: &'static str| if loaded { p } else { "Loading devices…" };
        card(
            "Devices",
            None,
            StackPanel::new().spacing(12.0).children((
                device_combo("Microphone", &self.choices.mic, placeholder("Choose a microphone"), enabled, context.callback(Msg::Mic)),
                secondary(
                    "To use a plain cable, play to its Speakers endpoint and record from a microphone on the same cable. \
                     You can leave this app closed or run filters on a different cable.",
                ),
                combo(
                    "Output route",
                    &["Direct to driver", "WASAPI playback device"],
                    usize::from(!ring),
                    !self.test.busy(),
                    context.callback(Msg::OutputBackend),
                ),
                if ring {
                    StackPanel::new().spacing(6.0).children((
                        secondary(
                            "Direct output feeds ACX-LL-Cable A, or the only installed cable. \
                             In your recording or call app, select a microphone on that cable.",
                        ),
                        if self.elevated || nf_vmic::broker::authorized() {
                            View::empty()
                        } else {
                            Button::new()
                                .is_enabled(!self.granting_driver)
                                .on_click(context.callback(|()| Msg::AllowDriver))
                                .content(if self.granting_driver { "Waiting for approval…" } else { "Allow direct output…" })
                        },
                    ))
                } else {
                    device_combo(
                        "Output (virtual cable)",
                        &self.choices.output,
                        placeholder("Choose the cable's playback endpoint"),
                        enabled,
                        context.callback(Msg::Output),
                    )
                },
                device_combo(
                    "Echo reference (speakers)",
                    &self.choices.reference,
                    placeholder("Choose your speakers"),
                    enabled,
                    context.callback(Msg::Reference),
                ),
                Button::new().is_enabled(!self.refreshing).on_click(context.callback(|()| Msg::Refresh)).content(if self.refreshing {
                    "Refreshing…"
                } else {
                    "Refresh devices"
                }),
            )),
        )
    }

    fn processing_card(&self, context: &ViewContext<Self>) -> View {
        let p = &self.settings.engine.processing;
        let g = &p.gate;
        let noise = p.noise_enabled;
        // The gate ends the noise stage: with the filter off it's bypassed too
        let gate_on = noise && g.mode != GateMode::Off;
        // Keyed by mode: each mode gets its own native slider, so switching ranges can't make WinUI
        // coerce one mode's value into the other's range.
        let level = match g.mode {
            GateMode::Auto => (
                "margin",
                slider(
                    format::labelled("Margin above noise (dB)", g.auto_margin_db, 1, ""),
                    MARGIN_DB,
                    g.auto_margin_db,
                    gate_on,
                    context.callback(Msg::Margin),
                ),
            ),
            GateMode::Off | GateMode::Static => (
                "threshold",
                slider(
                    format::labelled("Threshold (dBFS)", g.threshold_db, 0, ""),
                    THRESHOLD_DB,
                    g.threshold_db,
                    gate_on,
                    context.callback(Msg::Threshold),
                ),
            ),
        };
        let bypassed: View =
            if noise { View::empty() } else { secondary("The noise filter is off: suppression and the gate are bypassed.").into() };
        card(
            "Processing",
            None,
            StackPanel::new().spacing(10.0).children((
                CheckBox::new().is_checked(noise).on_is_checked_changed(context.callback(Msg::Noise)).content("Noise filter (RNNoise)"),
                StackPanel::new().children((
                    CheckBox::new()
                        .is_checked(p.aec_enabled)
                        .on_is_checked_changed(context.callback(Msg::Aec))
                        .content("Echo cancellation (using speakers)"),
                    secondary("Leave off with headphones.").margin(Thickness::new(28.0, 0.0, 0.0, 0.0)),
                )),
                bypassed,
                slider(format::suppression(p.floor_db), FLOOR_DB, p.floor_db, noise, context.callback(Msg::Floor)),
                TextBlock::new().text("Gate").font_weight(FontWeight::SEMI_BOLD).margin(Thickness::new(0.0, 6.0, 0.0, 0.0)),
                combo("Mode", &GATE_MODES, gate_index(g.mode), noise, context.callback(Msg::GateMode)),
                View::keyed_fragment([level]),
                slider(format::labelled("Attack", g.attack_ms, 1, "ms"), ATTACK_MS, g.attack_ms, gate_on, context.callback(Msg::Attack)),
                slider(
                    format::labelled("Release", g.release_ms, 0, "ms"),
                    RELEASE_MS,
                    g.release_ms,
                    gate_on,
                    context.callback(Msg::Release),
                ),
                slider(format::labelled("Hold", g.hold_ms, 0, "ms"), HOLD_MS, g.hold_ms, gate_on, context.callback(Msg::Hold)),
                slider(format::labelled("Range (dB)", g.range_db, 0, ""), RANGE_DB, g.range_db, gate_on, context.callback(Msg::Range)),
                CheckBox::new()
                    .is_checked(g.use_vad)
                    .is_enabled(gate_on)
                    .on_is_checked_changed(context.callback(Msg::Vad))
                    .content("Hold open on speech (VAD)"),
            )),
        )
    }

    fn live_input(&self, context: &ViewContext<Self>) -> LiveInput {
        let p = &self.settings.engine.processing;
        LiveInput {
            engine: SharedEngine(Arc::clone(&self.engine)),
            show_meters: p.meters_enabled,
            show_readout: p.latency_readout_enabled,
            direct: self.settings.engine.io.output_backend == OutputBackend::DriverRing,
            track: track_width(self.width),
            on_show_meters: context.callback(Msg::ShowMeters),
            on_show_readout: context.callback(Msg::ShowReadout),
        }
    }

    fn test_section(&self, context: &ViewContext<Self>) -> View {
        let ui = &self.settings.ui;
        let t = &self.test;
        let busy = t.busy();
        let loaded = self.devices.is_some();
        let placeholder = |p: &'static str| if loaded { p } else { "Loading devices…" };
        let blocker = test_blocker(self.status.state, self.job, ui.test_probe.as_deref(), ui.test_capture.as_deref());
        let (label, enabled, style) = match (&t.running, t.starting) {
            (Some(_), _) => (if t.cancelling { "Cancelling…" } else { "Cancel" }, !t.cancelling, ButtonStyle::Default),
            (None, true) => ("Starting…", false, ButtonStyle::Accent),
            (None, false) => ("Run test", blocker.is_none(), ButtonStyle::Accent),
        };
        let progress: View = if t.running.is_some() {
            ProgressBar::new()
                .minimum(0.0)
                .maximum(1.0)
                .value(progress_value(t.fraction))
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(1)
                .into()
        } else {
            View::empty()
        };
        let line: View = match (&t.running, blocker) {
            (Some(_), _) => secondary(stage_line(t.stage, t.fraction)).into(),
            (None, Some(why)) if !t.starting => secondary(why).into(),
            _ => View::empty(),
        };
        let body = StackPanel::new().spacing(12.0).children((
            combo("Mode", &TestMode::LABELS, ui.test_mode.index(), !busy, context.callback(Msg::TestMode)),
            secondary(format::test_help(ui.test_mode)),
            device_combo(
                "Play probe on",
                &self.choices.probe,
                placeholder("Choose a playback device"),
                loaded && !busy,
                context.callback(Msg::Probe),
            ),
            device_combo(
                "Capture app output from",
                &self.choices.capture,
                placeholder("Choose a recording device"),
                loaded && !busy,
                context.callback(Msg::Capture),
            ),
            NumberBox::new()
                .minimum(f64::from(MIN_REPETITIONS))
                .maximum(f64::from(MAX_REPETITIONS))
                .value(f64::from(ui.test_repetitions))
                .is_enabled(!busy)
                .on_value_changed(context.callback(Msg::Repetitions))
                .horizontal_alignment(HorizontalAlignment::Left)
                .min_width(160.0)
                .slot(NumberBoxSlot::Header, "Repetitions"),
            Grid::new().columns([GridLength::Auto, GridLength::STAR]).column_spacing(12.0).children((
                Button::new()
                    .grid_column(0)
                    .min_width(104.0)
                    .style(style)
                    .is_enabled(enabled)
                    .on_click(context.callback(|()| Msg::RunTest))
                    .content(label),
                progress,
            )),
            line,
            self.test_outcome(),
        ));
        Expander::new()
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .slots([SlotView::new(ExpanderSlot::Header, "Measure latency"), SlotView::new(ExpanderSlot::Content, body)])
    }

    fn test_outcome(&self) -> View {
        match &self.test.outcome {
            None => View::empty(),
            Some(TestOutcome::Cancelled) => secondary("The test was cancelled.").into(),
            Some(TestOutcome::Failed(e)) => selectable(format!("The test failed: {e}")).into(),
            Some(TestOutcome::Done(report)) => StackPanel::new()
                .spacing(2.0)
                .keyed_children(format::report(report).into_iter().enumerate().map(|(i, line)| (i, selectable(line)))),
        }
    }

    fn diagnostics_section(&self, context: &ViewContext<Self>) -> View {
        let io = &self.settings.engine.io;
        let facts = format::diagnostics(&self.status).into_iter().enumerate().map(|(i, line)| (i, selectable(line)));
        let note: View = if self.large_pages_set_up { secondary("Sign out and back in to apply.").into() } else { View::empty() };
        // Each switch keyed by itself, with its line under it where it has one
        let io_switch = |s: IoSwitch| {
            let view = match s.description() {
                Some(d) => described_switch(s.label(), d, s.get(io), io_callback(context, s)),
                None => switch(s.label(), s.get(io), io_callback(context, s)),
            };
            (s as usize, view)
        };
        let wasapi = io.output_backend == OutputBackend::Wasapi;
        let latency = IoSwitch::LATENCY.into_iter().filter(|s| wasapi || matches!(s, IoSwitch::ExclusiveMic)).map(io_switch);
        let streams = IoSwitch::STREAMS.map(io_switch);
        let heading =
            |text: &'static str| TextBlock::new().text(text).font_weight(FontWeight::SEMI_BOLD).margin(Thickness::new(0.0, 6.0, 0.0, 0.0));
        let body = StackPanel::new().spacing(12.0).children((
            StackPanel::new().spacing(2.0).keyed_children(facts),
            heading("Latency"),
            StackPanel::new().spacing(12.0).keyed_children(latency),
            secondary(EXCLUSIVE_FALLBACK),
            heading("Streams"),
            StackPanel::new().spacing(12.0).keyed_children(streams),
            if wasapi { slider(
                format::labelled("Output margin (ms)", io.output_margin_ms, 1, ""),
                OUTPUT_MARGIN_MS,
                io.output_margin_ms,
                true,
                context.callback(Msg::OutputMargin),
            ) } else { View::empty() },
            StackPanel::new().spacing(4.0).children((
                Button::new()
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .is_enabled(!self.elevating)
                    .on_click(context.callback(|()| Msg::EnableLargePages))
                    .content(if self.elevating { "Waiting for the setup…" } else { "Enable large pages…" })
                    .tooltip("Grants this account the lock-pages privilege (an administrator must approve), so the audio memory can use large pages."),
                note,
            )),
        ));
        Expander::new()
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .slots([SlotView::new(ExpanderSlot::Header, "Diagnostics"), SlotView::new(ExpanderSlot::Content, body)])
    }
}

/// A switch's callback. Each arm's mapper captures nothing, so it keeps its identity across renders (Reactor's
/// `callback`: a capturing closure's would change at each one).
fn io_callback(context: &ViewContext<Window>, switch: IoSwitch) -> Callback<bool> {
    match switch {
        IoSwitch::ExclusiveOutput => context.callback(|on| Msg::Io(IoSwitch::ExclusiveOutput, on)),
        IoSwitch::ExclusiveMic => context.callback(|on| Msg::Io(IoSwitch::ExclusiveMic, on)),
        IoSwitch::AdaptiveOutput => context.callback(|on| Msg::Io(IoSwitch::AdaptiveOutput, on)),
        IoSwitch::LowLatency => context.callback(|on| Msg::Io(IoSwitch::LowLatency, on)),
        IoSwitch::RawMic => context.callback(|on| Msg::Io(IoSwitch::RawMic, on)),
        IoSwitch::KeepAlive => context.callback(|on| Msg::Io(IoSwitch::KeepAlive, on)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's ask (2026-10-31): a start that the microphone privacy settings refused offers a button to
    /// them, until the engine runs again.
    #[test]
    fn a_blocked_mic_offers_the_privacy_settings() {
        let failed = EngineStatus { state: EngineState::Error, remedy: Some(Remedy::MicPrivacy), ..EngineStatus::default() };
        assert_eq!(remedy_offered(&failed), Some(Remedy::MicPrivacy));
        let running = EngineStatus { state: EngineState::Running, ..failed.clone() };
        assert_eq!(remedy_offered(&running), None);
        assert_eq!(remedy_offered(&EngineStatus::default()), None);
        assert_eq!(Remedy::MicPrivacy.label(), "Open microphone privacy settings");
    }

    /// app-privilege F3: the window's relaunch is marked as the helper, so with UAC off it stops
    /// with code 4 instead of relaunching itself, and the notice explains that code.
    #[test]
    fn the_large_page_helper_is_marked_and_its_refusal_explained() {
        assert_eq!(helper_args("S-1-5-21-1"), ["setup-large-pages", "--sid", "S-1-5-21-1", "--elevated-helper"]);
        assert!(helper_failed(HELPER_NOT_ELEVATED).contains("UAC"));
        assert_eq!(helper_failed(2), "The elevated setup exited with code 2.");
    }

    #[test]
    fn gate_modes_map_both_ways() {
        for (i, label) in GATE_MODES.iter().enumerate() {
            let mode = gate_mode(i).unwrap_or_else(|| panic!("{label}"));
            assert_eq!(gate_index(mode), i);
        }
        assert_eq!(gate_mode(GATE_MODES.len()), None);
        assert_eq!(GATE_MODES[gate_index(GateMode::default())], "Off");
    }

    #[test]
    fn the_test_needs_a_running_engine_and_both_devices() {
        let running = EngineState::Running;
        assert_eq!(test_blocker(running, None, Some("p"), Some("c")), None);
        assert!(test_blocker(EngineState::Stopped, None, Some("p"), Some("c")).is_some_and(|w| w.starts_with("Start the filter")));
        assert!(test_blocker(EngineState::Error, None, Some("p"), Some("c")).is_some());
        assert!(test_blocker(running, Some(Job::Stopping), Some("p"), Some("c")).is_some(), "stopping");
        assert!(test_blocker(running, None, None, Some("c")).is_some_and(|w| w.starts_with("Choose")));
        assert!(test_blocker(running, None, Some("p"), None).is_some());
    }

    #[test]
    fn progress_values_are_clamped() {
        assert_eq!(progress_value(0.4), f64::from(0.4f32));
        assert_eq!(progress_value(-1.0), 0.0);
        assert_eq!(progress_value(3.0), 1.0);
        assert_eq!(progress_value(f32::NAN), 0.0);
        assert_eq!(stage_line("Playing sweeps", 0.4), "Playing sweeps… 40%");
        assert_eq!(stage_line("", f32::NAN), "Running… 0%");
    }

    #[test]
    fn progress_follows_the_running_test_and_ignores_earlier_ones() {
        let mut panel: TestPanel<()> = TestPanel { starting: true, ..TestPanel::default() };
        panel.started(Ok(()));
        assert!(panel.busy() && !panel.starting);
        assert_eq!((panel.generation, panel.stage), (1, "Starting"));
        panel.progress(1, LatencyProgress::Running { fraction: 0.4, stage: "Playing sweeps" });
        assert_eq!((panel.fraction, panel.stage), (0.4, "Playing sweeps"));
        panel.progress(0, LatencyProgress::Cancelled);
        assert!(panel.running.is_some() && panel.outcome.is_none(), "an earlier test's end is ignored");
        let report = LatencyReport { repetitions: 5, valid: 5, ..LatencyReport::default() };
        panel.progress(1, LatencyProgress::Done(Box::new(report.clone())));
        assert!(!panel.busy());
        assert_eq!(panel.outcome, Some(TestOutcome::Done(Box::new(report))));
        panel.progress(1, LatencyProgress::Failed("late".to_owned()));
        assert!(matches!(panel.outcome, Some(TestOutcome::Done(_))), "nothing after the end");

        // The next test starts clean, and a cancel ends it
        panel.starting = true;
        panel.started(Ok(()));
        assert_eq!((panel.generation, panel.fraction, &panel.outcome), (2, 0.0, &None));
        panel.cancelling = true;
        panel.progress(2, LatencyProgress::Cancelled);
        assert_eq!((panel.cancelling, &panel.outcome), (false, &Some(TestOutcome::Cancelled)));
    }

    #[test]
    fn a_failed_start_is_reported_and_leaves_no_test() {
        let mut panel: TestPanel<()> = TestPanel { starting: true, ..TestPanel::default() };
        panel.started(Err(EngineError("The engine isn't running".to_owned())));
        assert!(!panel.busy());
        assert_eq!(panel.outcome, Some(TestOutcome::Failed("The engine isn't running".to_owned())));
        assert_eq!(panel.generation, 0);
    }

    #[test]
    fn progress_without_a_running_test_is_ignored() {
        let mut panel: TestPanel<()> = TestPanel::default();
        panel.progress(0, LatencyProgress::Cancelled);
        assert_eq!(panel.outcome, None);
        panel.progress(0, LatencyProgress::Running { fraction: 0.5, stage: "x" });
        assert_eq!(panel.fraction, 0.0);
    }

    use EngineState as S;

    fn engine_status(state: EngineState, retrying: bool) -> EngineStatus {
        EngineStatus { state, retrying, message: Some("The mic was unplugged".to_owned()), ..EngineStatus::default() }
    }

    /// gui/F8: while the engine is armed to restart by itself, the button stops it.
    #[test]
    fn a_retrying_engine_offers_stop() {
        assert!(stops(&engine_status(S::Running, false)));
        assert!(stops(&engine_status(S::Starting, false)));
        assert!(stops(&engine_status(S::Error, true)), "armed to restart: Stop disarms it");
        assert!(stops(&engine_status(S::Stopped, true)));
        assert!(!stops(&engine_status(S::Error, false)), "stopped for good: Start");
        assert!(!stops(&engine_status(S::Stopped, false)));
    }

    /// gui/F10: a fault that stops a run reaches the InfoBar, once, and clears when it runs again.
    #[test]
    fn a_fault_reaches_the_infobar_once_and_clears_on_recovery() {
        let mut watch = FaultWatch::default();
        let mut notice = None;
        watch.observe(&engine_status(S::Running, false), None, &mut notice);
        assert_eq!(notice, None);

        // The mic is unplugged mid-run
        let fault = engine_status(S::Error, true);
        watch.observe(&fault, None, &mut notice);
        let raised = notice.clone().expect("raised");
        assert_eq!((raised.severity, raised.title, raised.about_run), (InfoBarSeverity::Error, "The filter stopped", true));
        assert!(raised.message.starts_with("The mic was unplugged.") && raised.message.contains("Press Stop"));

        // The user closes it: failed retries don't bring it back
        notice = None;
        for state in [S::Starting, S::Error, S::Starting, S::Error] {
            watch.observe(&engine_status(state, true), None, &mut notice);
        }
        assert_eq!(notice, None);

        // A retry succeeds: a fault notice still open clears, any other notice stays
        notice = Some(raised);
        watch.observe(&engine_status(S::Running, false), None, &mut notice);
        assert_eq!(notice, None, "recovered by itself");
        let other = Notice::new(InfoBarSeverity::Success, "Large pages are set up", "Sign out and back in to apply.");
        notice = Some(other.clone());
        watch.observe(&engine_status(S::Running, false), None, &mut notice);
        assert_eq!(notice, Some(other));

        // The next fault is raised again
        watch.observe(&fault, None, &mut notice);
        assert_eq!(notice.map(|n| n.title), Some("The filter stopped"));
    }

    #[test]
    fn a_start_or_stop_under_way_reports_its_own_result() {
        let mut watch = FaultWatch::default();
        let mut notice = None;
        watch.observe(&engine_status(S::Error, false), Some(Job::Starting), &mut notice);
        assert_eq!(notice, None, "the start's own result says why");

        // A failed start's error is showing: the fault it leaves isn't raised over it
        notice = Some(Notice::about_run("Couldn't start", "The device is in use"));
        watch.start_failed();
        watch.observe(&engine_status(S::Error, true), None, &mut notice);
        assert_eq!(notice.as_ref().map(|n| n.title), Some("Couldn't start"));

        // The user stops it; a fault in a later run is new
        watch.observe(&engine_status(S::Stopped, false), Some(Job::Stopping), &mut notice);
        watch.observe(&engine_status(S::Stopped, false), None, &mut notice);
        watch.observe(&engine_status(S::Error, false), None, &mut notice);
        let raised = notice.expect("raised");
        assert_eq!(raised.title, "The filter stopped");
        assert!(raised.message.ends_with("Press Start to try again."), "{}", raised.message);
    }

    #[test]
    fn the_warnings_bar_follows_the_warnings() {
        let mut bar = WarningsBar::default();
        let lost = "The echo reference was lost".to_owned();
        let mut status = EngineStatus { state: S::Running, warnings: vec![lost.clone()], ..EngineStatus::default() };
        bar.observe(&status);
        assert_eq!(bar.shown(&status), Some((format::Tone::Warning, "Running with a warning", lost.clone())));

        // Closed by the user: hidden while the same list holds, shown again when it changes
        bar.dismiss(&status);
        bar.observe(&status);
        assert_eq!(bar.shown(&status), None);
        status.warnings.push("MMCSS isn't available".to_owned());
        bar.observe(&status);
        assert_eq!(bar.shown(&status).map(|(_, title, _)| title), Some("Running with warnings"));

        // They clear: the bar closes, and the same warning coming back later shows again
        bar.dismiss(&status);
        status.warnings.clear();
        bar.observe(&status);
        assert_eq!(bar.shown(&status), None);
        status.warnings = vec![lost];
        bar.observe(&status);
        assert!(bar.shown(&status).is_some());

        // A note (a stream that runs shared, not exclusive) alone: informational; closed, it stays closed until the list
        // changes, as the warnings do
        status.warnings.clear();
        status.notes = vec!["The microphone (Mic) runs in shared mode: ...".to_owned()];
        bar.observe(&status);
        assert_eq!(bar.shown(&status).map(|(tone, title, _)| (tone, title)), Some((format::Tone::Note, "Running with a note")));
        bar.dismiss(&status);
        bar.observe(&status);
        assert_eq!(bar.shown(&status), None);
        status.warnings = vec!["MMCSS isn't available".to_owned()];
        assert!(bar.shown(&status).is_some_and(|(tone, _, text)| tone == format::Tone::Warning && text.ends_with("shared mode: ...")));

        // Only while running
        status.state = S::Stopped;
        bar.observe(&status);
        assert_eq!(bar.shown(&status), None);
    }

    #[test]
    fn choices_follow_the_settings() {
        let list = vec![nf_engine::DeviceInfo {
            id: "cable-in".to_owned(),
            cable_selection: None,
            name: "CABLE Input".to_owned(),
            flow: nf_engine::Flow::Render,
            is_default: false,
            is_default_communications: false,
            sample_rate: 48_000,
            channels: 2,
        }];
        let mut s = Settings::default();
        let c = Choices::new(&list, &s);
        assert_eq!((c.output.selected, c.reference.labels.len()), (None, 2), "the cable is a reference choice until it's the output");
        s.engine.devices.output = Some("cable-in".to_owned());
        s.ui.test_capture = Some("gone".to_owned());
        let c = Choices::new(&list, &s);
        assert_eq!(c.output.selected, Some(0));
        assert_eq!(c.reference.labels, ["Default speakers"]);
        assert_eq!(c.capture.labels, ["(missing) gone"]);
        s.engine.io.output_backend = OutputBackend::DriverRing;
        let ring = Choices::new(&list, &s);
        assert_eq!(ring.reference.labels, ["Default speakers", "CABLE Input"], "a saved WASAPI output never hides the ring's reference");
        assert_eq!(ring.output.selected, Some(0), "keep the endpoint for a switch back to WASAPI");
    }
}
