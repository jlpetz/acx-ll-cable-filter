//! The settings file: the engine's [`EngineConfig`] plus the window's own fields, as JSON.
//!
//! - Loaded once, before the window opens. A file that doesn't parse, or can't be read, is renamed
//!   to `settings.json.bad` and the defaults are used, so the next save can't destroy what the user
//!   had. An unreadable file that can't be moved aside either (locked, or on a share that isn't
//!   there yet) turns saving off for the session instead ([`Loaded::read_only`]).
//! - Missing fields take their defaults (`#[serde(default)]` here and throughout `EngineConfig`) and
//!   unknown ones are ignored, so files written by older and newer versions keep loading. A value
//!   whose meaning changed is brought up to date on load by the file's `version` ([`upgrade_engine`]).
//! - Every value is clamped to its control's range on load ([`Settings::sanitize`]): a hand-edited
//!   file can't put a slider out of range.
//! - Saved by a background thread [`SAVE_DEBOUNCE`] after the last change (a slider drag is dozens
//!   of changes), through a temporary file that's flushed to the disk and then renamed, so neither a
//!   crash mid-write nor a power loss after it can truncate or zero it.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use nf_engine::{EffectConfig, EngineConfig, GateConfig, GateMode, GatePosition, HighBand, IoConfig, LatencyMode, ProcessingConfig};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

/// How long the settings must stay unchanged before they're written.
pub(crate) const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// The window's default client size, in DIPs (gui.md: about 520×760).
pub(crate) const DEFAULT_WIDTH: f64 = 520.0;
pub(crate) const DEFAULT_HEIGHT: f64 = 760.0;
/// The window's minimum size (its constraints), in DIPs.
pub(crate) const MIN_WIDTH: f64 = 440.0;
pub(crate) const MIN_HEIGHT: f64 = 480.0;
/// A remembered size larger than this is a bad file, not a monitor.
const MAX_SIDE: f64 = 8192.0;

/// Latency-test repetitions (the NumberBox's range).
pub(crate) const MIN_REPETITIONS: u32 = 1;
pub(crate) const MAX_REPETITIONS: u32 = 20;
const DEFAULT_REPETITIONS: u32 = 5;

/// A slider's range and step. The settings are clamped to it on load and the slider is built from it,
/// so the two can't disagree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Span {
    pub min: f32,
    pub max: f32,
    pub step: f32,
}

impl Span {
    /// `value` clamped into the span; NaN (which `clamp` would keep) becomes `fallback`, clamped too.
    pub fn clamp(self, value: f32, fallback: f32) -> f32 {
        let value = if value.is_nan() { fallback } else { value };
        if value.is_nan() { self.min } else { value.clamp(self.min, self.max) }
    }
}

/// "Max suppression" (the noise stage's floor). -60 is effectively unlimited.
pub(crate) const FLOOR_DB: Span = Span { min: -60.0, max: 0.0, step: 1.0 };
/// The static gate's threshold, in dBFS.
pub(crate) const THRESHOLD_DB: Span = Span { min: -80.0, max: -10.0, step: 1.0 };
/// The auto gate's margin above the tracked noise floor.
pub(crate) const MARGIN_DB: Span = Span { min: 0.0, max: 20.0, step: 0.5 };
pub(crate) const ATTACK_MS: Span = Span { min: 0.5, max: 50.0, step: 0.5 };
pub(crate) const RELEASE_MS: Span = Span { min: 20.0, max: 1000.0, step: 10.0 };
pub(crate) const HOLD_MS: Span = Span { min: 0.0, max: 500.0, step: 10.0 };
/// The gate's attenuation when closed.
pub(crate) const RANGE_DB: Span = Span { min: -80.0, max: 0.0, step: 1.0 };
/// Extra fill kept on the output cable.
pub(crate) const OUTPUT_MARGIN_MS: Span = Span { min: 0.0, max: 20.0, step: 0.5 };

/// The I/O switches: `IoConfig`'s booleans, each a ToggleSwitch in the diagnostics section (gui.md section 7). The latency
/// options come first, each with a line on what it does and costs; changing any restarts the streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoSwitch {
    ExclusiveOutput,
    ExclusiveMic,
    AdaptiveOutput,
    LowLatency,
    RawMic,
    KeepAlive,
}

impl IoSwitch {
    /// The latency options (on by default: the owner, 2026-11-02), in the order shown.
    pub const LATENCY: [Self; 3] = [Self::ExclusiveOutput, Self::ExclusiveMic, Self::AdaptiveOutput];
    /// The streams' other switches, in the order shown.
    pub const STREAMS: [Self; 3] = [Self::LowLatency, Self::RawMic, Self::KeepAlive];

    pub fn get(self, io: &IoConfig) -> bool {
        match self {
            Self::ExclusiveOutput => io.output_exclusive,
            Self::ExclusiveMic => io.mic_exclusive,
            Self::AdaptiveOutput => io.adaptive_output,
            Self::LowLatency => io.low_latency_periods,
            Self::RawMic => io.raw_mic,
            Self::KeepAlive => io.keep_alive_reference,
        }
    }

    pub fn set(self, io: &mut IoConfig, on: bool) {
        let field = match self {
            Self::ExclusiveOutput => &mut io.output_exclusive,
            Self::ExclusiveMic => &mut io.mic_exclusive,
            Self::AdaptiveOutput => &mut io.adaptive_output,
            Self::LowLatency => &mut io.low_latency_periods,
            Self::RawMic => &mut io.raw_mic,
            Self::KeepAlive => &mut io.keep_alive_reference,
        };
        *field = on;
    }

    /// The switch's header.
    pub fn label(self) -> &'static str {
        match self {
            Self::ExclusiveOutput => "Exclusive output",
            Self::ExclusiveMic => "Exclusive microphone",
            Self::AdaptiveOutput => "Adaptive output buffer",
            Self::LowLatency => "Low-latency periods (IAudioClient3)",
            Self::RawMic => "RAW mic",
            Self::KeepAlive => "Keep reference alive",
        }
    }

    /// The line under it, in plain words: what it does, what it costs, and when to turn it off. The figures are the dev
    /// VM's VB-CABLE A/B (TODO 13).
    pub fn description(self) -> Option<&'static str> {
        Some(match self {
            Self::ExclusiveOutput => {
                "Lowest latency (20-30 ms less on VB-CABLE): the filter plays straight into the cable's driver. Only the filter \
                 plays into the cable, and your apps still record its other side as usual. Turn off if the cable misbehaves."
            }
            Self::ExclusiveMic => {
                "Lowest latency. While the filter runs, other apps can't open your microphone directly; they use the filtered \
                 mic. Turn off if you record your raw microphone in another app. With echo cancelling on, if echo gets through \
                 or your voice crackles, try turning this off."
            }
            Self::AdaptiveOutput => {
                "Trims the output buffer to what your PC needs after four minutes of running (about 7 ms less on VB-CABLE). A \
                 rare hiccup can then cost a short glitch, three a run at most. Turn off for a recording that must have none."
            }
            // The mic's shared-mode options: the exclusive mic has its own small period and no Windows effects.
            Self::LowLatency | Self::RawMic => "For the microphone in shared mode (with the exclusive microphone off, or refused).",
            Self::KeepAlive => return None,
        })
    }
}

/// Under the latency options: what a refusal does.
pub(crate) const EXCLUSIVE_FALLBACK: &str =
    "A device that refuses exclusive mode runs in shared mode, and a note at the top says why and how to fix it.";

/// Version 1 introduced automatic model depth. Version 2 makes a selected gate run independently of the denoiser.
pub const SETTINGS_VERSION: u32 = 2;
/// The model depth every window saved before the automatic depth (88d1caa on): the default then, which nothing in the
/// window sets.
const OLD_DEFAULT_MODEL_DEPTH: u32 = 2;

/// Brings an engine config saved in a settings file of `version` up to this version's meaning (the CLI's `--config` reads
/// the window's file too). Before 1, a model depth of 2 was the default the window saved, not a choice: it becomes
/// automatic (`nf_engine::AUTO_MODEL_DEPTH`). A depth a later file sets, 2 included, is kept.
pub fn upgrade_engine(version: u32, engine: &mut EngineConfig) {
    if version < 1 && engine.processing.model_depth == OLD_DEFAULT_MODEL_DEPTH {
        engine.processing.model_depth = nf_engine::AUTO_MODEL_DEPTH;
    }
    if version < 2 && !engine.processing.noise_enabled {
        // Previously, switching the noise filter off bypassed its gate too. Preserve the sound of that saved route.
        engine.processing.gate.mode = GateMode::Off;
    }
}

/// The stages that shape the signal. A preset can be applied to a different cable or microphone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PipelinePreset {
    aec_enabled: bool,
    noise_enabled: bool,
    noise_model: String,
    high_band: HighBand,
    model_depth: u32,
    floor_db: f32,
    gate: GateConfig,
    gate_position: GatePosition,
    effect: EffectConfig,
}

impl PipelinePreset {
    pub fn capture(engine: &EngineConfig) -> Self {
        let p = &engine.processing;
        Self {
            aec_enabled: p.aec_enabled,
            noise_enabled: p.noise_enabled,
            noise_model: p.noise_model.clone(),
            high_band: p.high_band,
            model_depth: p.model_depth,
            floor_db: p.floor_db,
            gate: p.gate.clone(),
            gate_position: p.gate_position,
            effect: engine.effect.clone(),
        }
    }

    pub fn apply(self, engine: &mut EngineConfig) {
        let mut p = ProcessingConfig {
            aec_enabled: self.aec_enabled,
            noise_enabled: self.noise_enabled,
            noise_model: self.noise_model,
            high_band: self.high_band,
            model_depth: self.model_depth,
            floor_db: self.floor_db,
            gate: self.gate,
            gate_position: self.gate_position,
            meters_enabled: engine.processing.meters_enabled,
            latency_readout_enabled: engine.processing.latency_readout_enabled,
        };
        sanitize_processing(&mut p);
        engine.processing = p;
        engine.effect = self.effect;
    }
}

/// Everything the settings file holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// [`SETTINGS_VERSION`] when written by this version; a file without it is 0 (its own default, not the struct's).
    #[serde(default)]
    pub version: u32,
    /// What the engine runs with. Its `processing.meters_enabled` and
    /// `processing.latency_readout_enabled` are the "Show meters" and "Live readout" toggles: the
    /// engine skips that work when they're off, so they live in its config rather than here twice.
    pub engine: EngineConfig,
    pub ui: UiSettings,
    pub presets: BTreeMap<String, PipelinePreset>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { version: SETTINGS_VERSION, engine: EngineConfig::default(), ui: UiSettings::default(), presets: BTreeMap::new() }
    }
}

/// The latency test's loop (gui.md section 6). The engine's [`LatencyMode`] isn't serde, so the
/// file has its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TestMode {
    /// Probe into one virtual cable, capture the app's output from another.
    #[default]
    Virtual,
    /// Probe on the speakers, picked up by the mic.
    Acoustic,
}

impl TestMode {
    /// The mode ComboBox's items, in index order.
    pub const LABELS: [&'static str; 2] = ["Virtual cables", "Speakers and mic"];

    pub fn index(self) -> usize {
        match self {
            TestMode::Virtual => 0,
            TestMode::Acoustic => 1,
        }
    }

    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(TestMode::Virtual),
            1 => Some(TestMode::Acoustic),
            _ => None,
        }
    }

    pub fn engine(self) -> LatencyMode {
        match self {
            TestMode::Virtual => LatencyMode::VirtualLoop,
            TestMode::Acoustic => LatencyMode::Acoustic,
        }
    }
}

/// The window's own fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UiSettings {
    pub test_mode: TestMode,
    /// Render endpoint id the probe plays on; `None` until chosen.
    pub test_probe: Option<String>,
    /// Capture endpoint id carrying the app's output; `None` until chosen.
    pub test_capture: Option<String>,
    pub test_repetitions: u32,
    /// The window's last client size, in DIPs, used for the next start.
    pub window_width: f64,
    pub window_height: f64,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            test_mode: TestMode::Virtual,
            test_probe: None,
            test_capture: None,
            test_repetitions: DEFAULT_REPETITIONS,
            window_width: DEFAULT_WIDTH,
            window_height: DEFAULT_HEIGHT,
        }
    }
}

impl Settings {
    /// Upgrade still-active Windows IDs from an older settings file as soon as the device picker has its inventory.
    pub(crate) fn bind_cable_endpoints(&mut self, list: &[nf_engine::DeviceInfo]) -> bool {
        let d = &mut self.engine.devices;
        let mut changed = false;
        for selected in
            [&mut d.mic, &mut d.output, &mut d.reference, &mut self.ui.test_probe, &mut self.ui.test_capture].into_iter().flatten()
        {
            if let Some(binding) = list.iter().find(|d| d.id == *selected).and_then(|d| d.cable_selection.as_ref()) {
                selected.clone_from(binding);
                changed = true;
            }
        }
        changed
    }

    /// Clamps every value to its control's range, so no builder can assert on it and no slider snaps
    /// on first show. Blank device ids become `None` (the default, or not chosen).
    pub fn sanitize(&mut self) {
        let defaults = Settings::default();
        sanitize_processing(&mut self.engine.processing);
        let io = &mut self.engine.io;
        io.output_margin_ms = OUTPUT_MARGIN_MS.clamp(io.output_margin_ms, defaults.engine.io.output_margin_ms);

        let d = &mut self.engine.devices;
        for id in [&mut d.mic, &mut d.output, &mut d.reference, &mut self.ui.test_probe, &mut self.ui.test_capture] {
            if id.as_deref().is_some_and(|s| s.trim().is_empty()) {
                *id = None;
            }
        }

        let ui = &mut self.ui;
        ui.test_repetitions = ui.test_repetitions.clamp(MIN_REPETITIONS, MAX_REPETITIONS);
        ui.window_width = clamp_side(ui.window_width, MIN_WIDTH, DEFAULT_WIDTH);
        ui.window_height = clamp_side(ui.window_height, MIN_HEIGHT, DEFAULT_HEIGHT);
    }
}

fn sanitize_processing(p: &mut ProcessingConfig) {
    let dp = ProcessingConfig::default();
    p.floor_db = FLOOR_DB.clamp(p.floor_db, dp.floor_db);
    let (g, dg) = (&mut p.gate, &dp.gate);
    g.threshold_db = THRESHOLD_DB.clamp(g.threshold_db, dg.threshold_db);
    g.auto_margin_db = MARGIN_DB.clamp(g.auto_margin_db, dg.auto_margin_db);
    g.attack_ms = ATTACK_MS.clamp(g.attack_ms, dg.attack_ms);
    g.release_ms = RELEASE_MS.clamp(g.release_ms, dg.release_ms);
    g.hold_ms = HOLD_MS.clamp(g.hold_ms, dg.hold_ms);
    g.range_db = RANGE_DB.clamp(g.range_db, dg.range_db);
}

/// A window side for `client_size`, which asserts finite and > 0: NaN becomes `default`.
fn clamp_side(value: f64, min: f64, default: f64) -> f64 {
    if value.is_finite() { value.clamp(min, MAX_SIDE) } else { default }
}

/// Parses a settings file's bytes (a UTF-8 BOM, as Notepad writes, is allowed) and sanitises them.
pub(crate) fn parse(bytes: &[u8]) -> Result<Settings, serde_json::Error> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut settings: Settings = serde_json::from_slice(bytes)?;
    upgrade_engine(settings.version, &mut settings.engine);
    settings.version = settings.version.max(SETTINGS_VERSION);
    settings.sanitize();
    Ok(settings)
}

/// Where a file that didn't parse is kept: `settings.json` → `settings.json.bad`.
pub(crate) fn bad_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bad");
    path.with_file_name(name)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// What loading found: the settings to use, and a warning for the InfoBar when they're the defaults
/// because the file was unreadable.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub settings: Settings,
    pub warning: Option<String>,
    /// The file is there but couldn't be read, nor moved aside: a save would replace the user's
    /// settings with the defaults, so this session doesn't save ([`Saver::disabled`]).
    pub read_only: bool,
}

/// Loads `path`. A missing file is a first run (defaults, no warning). A file that doesn't parse, or
/// can't be read, is backed up as [`bad_path`] before the defaults are used. One that can't be read
/// nor backed up is left alone, and the session is [`Loaded::read_only`].
pub(crate) fn load(path: &Path) -> Loaded {
    let defaults = |warning: String| Loaded { settings: Settings::default(), warning: Some(warning), read_only: false };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if first_run(&e, path) => {
            return Loaded { settings: Settings::default(), warning: None, read_only: false };
        }
        Err(e) => {
            let bad = bad_path(path);
            return match std::fs::rename(path, &bad) {
                Ok(()) => defaults(format!(
                    "{} couldn't be read ({e}), so the defaults are in use. The old file was kept as {}.",
                    path.display(),
                    bad.display()
                )),
                // Still there and still unreadable: a save would destroy it
                Err(_) => Loaded {
                    settings: Settings::default(),
                    warning: Some(format!(
                        "{} couldn't be read ({e}), so the defaults are in use and changes won't be saved this \
                         session. Restart noise-filter once the file is readable.",
                        path.display()
                    )),
                    read_only: true,
                },
            };
        }
    };
    match parse(&bytes) {
        Ok(settings) => Loaded { settings, warning: None, read_only: false },
        Err(e) => {
            let bad = bad_path(path);
            let kept = match std::fs::rename(path, &bad) {
                Ok(()) => format!("The old file was kept as {}.", bad.display()),
                Err(re) => {
                    format!("It couldn't be kept as {} ({re}), and the next change will replace it.", bad.display())
                }
            };
            defaults(format!("{} isn't valid ({e}), so the defaults are in use. {kept}", path.display()))
        }
    }
}

/// Whether a failed read of `path` means there's no settings file yet: the file, or its folder, is
/// missing. std also calls an unreachable path NotFound (a redirected AppData on a share that isn't
/// up yet), but the user's file may well be there: that's an unreadable file. A missing folder
/// counts only if the folder above it (AppData) is there: Windows also says "path not found" for a
/// drive letter that isn't mapped yet (verify-gui F7).
fn first_run(e: &io::Error, path: &Path) -> bool {
    const FILE_NOT_FOUND: i32 = ERROR_FILE_NOT_FOUND.0 as i32;
    const PATH_NOT_FOUND: i32 = ERROR_PATH_NOT_FOUND.0 as i32;
    let home_exists = || path.parent().and_then(Path::parent).is_some_and(Path::is_dir);
    match e.raw_os_error() {
        Some(FILE_NOT_FOUND) => true,
        Some(PATH_NOT_FOUND) => home_exists(),
        Some(_) => false,
        None => e.kind() == io::ErrorKind::NotFound && home_exists(),
    }
}

/// Writes `settings` to `path` (creating its folder): to a temporary file first, flushed to the
/// disk, then renamed over the old one, so a crash or a power loss leaves either the old file or the
/// new one, whole.
pub(crate) fn write(path: &Path, settings: &Settings) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let temp = temp_path(path);
    write_synced(&temp, &json).and_then(|()| std::fs::rename(&temp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Writes `bytes` to `path` and flushes them to the disk (`FlushFileBuffers`) before returning.
/// Without the flush, NTFS can journal the rename before the data reaches the disk, and a power
/// loss then leaves a settings file of the right length full of zeros.
///
/// A few milliseconds on an SSD. It runs on the settings thread (or, rarely, the window's), never
/// on an audio thread.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(test)]
    tests::SYNCED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The last save error, for the window to show. Shared between the saver thread and the window's
/// status waiter.
#[derive(Debug, Default)]
pub(crate) struct SaveErrors(Mutex<Option<String>>);

impl SaveErrors {
    fn record(&self, result: io::Result<()>, path: &Path) {
        if let Err(e) = result {
            let message = format!("{} couldn't be saved: {e}", path.display());
            log::warn!("{message}");
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
        }
    }

    /// The error since the last call, if any.
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

enum Command {
    Save(Box<Settings>),
    /// Write what's pending now and end the thread.
    Finish,
}

/// The settings saver's thread. [`Saver::finish`] flushes it once the window has closed.
pub(crate) struct Saver {
    handle: SaverHandle,
    thread: Option<JoinHandle<()>>,
}

/// The window's end of the saver. Cheap to clone; two handles are equal when they share a saver.
#[derive(Clone, Debug)]
pub(crate) struct SaverHandle {
    /// `None` when the thread couldn't start: saves are then written synchronously (small files).
    commands: Option<Sender<Command>>,
    path: Arc<PathBuf>,
    errors: Arc<SaveErrors>,
    /// `false` for a [`Saver::disabled`] session: saves are dropped.
    enabled: bool,
}

impl PartialEq for SaverHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.errors, &other.errors)
    }
}

impl Saver {
    /// Starts the saver for `path`; each save is written `debounce` after the last change.
    pub fn spawn(path: PathBuf, debounce: Duration) -> Self {
        let path = Arc::new(path);
        let errors = Arc::new(SaveErrors::default());
        let (commands, received) = mpsc::channel();
        let thread = {
            let (path, errors) = (Arc::clone(&path), Arc::clone(&errors));
            std::thread::Builder::new().name("nf-settings".to_owned()).spawn(move || save_loop(&received, &path, debounce, &errors))
        };
        let (commands, thread) = match thread {
            Ok(thread) => (Some(commands), Some(thread)),
            Err(e) => {
                log::warn!("the settings saver's thread didn't start ({e}): saving synchronously");
                (None, None)
            }
        };
        Self { handle: SaverHandle { commands, path, errors, enabled: true }, thread }
    }

    /// A saver that never writes, for a session whose settings file couldn't be read
    /// ([`Loaded::read_only`]): the user's file stays as it is.
    pub fn disabled(path: PathBuf) -> Self {
        let (path, errors) = (Arc::new(path), Arc::new(SaveErrors::default()));
        Self { handle: SaverHandle { commands: None, path, errors, enabled: false }, thread: None }
    }

    pub fn handle(&self) -> SaverHandle {
        self.handle.clone()
    }

    /// Writes any pending change and waits for the thread to end.
    pub fn finish(mut self) {
        if let Some(commands) = &self.handle.commands {
            let _ = commands.send(Command::Finish);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl SaverHandle {
    /// Queues `settings` to be written once changes stop for the debounce time. Does nothing in a
    /// [`Saver::disabled`] session.
    pub fn save(&self, settings: &Settings) {
        if !self.enabled {
            return;
        }
        let queued = self.commands.as_ref().is_some_and(|c| c.send(Command::Save(Box::new(settings.clone()))).is_ok());
        if !queued {
            self.errors.record(write(&self.path, settings), &self.path);
        }
    }

    pub fn errors(&self) -> Arc<SaveErrors> {
        Arc::clone(&self.errors)
    }
}

/// Trailing debounce: hold the newest settings until `debounce` passes with no newer ones.
fn save_loop(received: &Receiver<Command>, path: &Path, debounce: Duration, errors: &SaveErrors) {
    let mut pending: Option<Box<Settings>> = None;
    loop {
        let next = match pending {
            Some(_) => received.recv_timeout(debounce),
            None => received.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(Command::Save(settings)) => pending = Some(settings),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(settings) = pending.take() {
                    errors.record(write(path, &settings), path);
                }
            }
            Ok(Command::Finish) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(settings) = pending.take() {
                    errors.record(write(path, &settings), path);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::time::Instant;

    use nf_engine::GateMode;

    use super::*;

    /// Counts the writes [`write_synced`] flushed, across every test.
    pub(super) static SYNCED: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, empty folder per test (no tempfile crate), removed when the test drops it.
    fn temp_dir(name: &str) -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nf-gui-test-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// A test's folder in %TEMP%, removed on drop so test runs don't pile them up there.
    struct TempDir(PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn changed() -> Settings {
        let mut s = Settings::default();
        s.engine.devices.mic = Some("{0.0.1.00000000}.{mic}".to_owned());
        s.engine.devices.output = Some("{0.0.0.00000000}.{cable}".to_owned());
        s.engine.processing.aec_enabled = true;
        s.engine.processing.floor_db = -24.0;
        s.engine.processing.gate.mode = GateMode::Auto;
        s.engine.processing.gate.auto_margin_db = 8.5;
        s.engine.processing.meters_enabled = false;
        s.engine.io.raw_mic = false;
        s.engine.io.output_margin_ms = 4.5;
        s.ui.test_mode = TestMode::Acoustic;
        s.ui.test_probe = Some("{speakers}".to_owned());
        s.ui.test_repetitions = 9;
        s.ui.window_width = 600.0;
        s.ui.window_height = 900.0;
        s
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = changed();
        let json = serde_json::to_vec_pretty(&s).unwrap();
        assert_eq!(parse(&json).unwrap(), s);
        assert_eq!(parse(&serde_json::to_vec(&Settings::default()).unwrap()).unwrap(), Settings::default());
    }

    #[test]
    fn an_old_bypassed_gate_stays_off_and_a_new_gate_only_profile_survives() {
        let gate_in = |v| {
            let json = format!(r#"{{"version":{v},"engine":{{"processing":{{"noise_enabled":false,"gate":{{"mode":"Auto"}}}}}}}}"#);
            parse(json.as_bytes()).unwrap().engine.processing.gate.mode
        };
        assert_eq!(gate_in(0), GateMode::Off);
        assert_eq!(gate_in(1), GateMode::Off);
        assert_eq!(gate_in(2), GateMode::Auto);
    }

    #[test]
    fn named_preset_restores_stage_settings_while_preserving_route_and_monitoring() {
        let mut settings = changed();
        settings.engine.processing.noise_enabled = false;
        settings.engine.processing.gate_position = GatePosition::BeforeNoise;
        settings.engine.effect.path = Some("gain.clap".into());
        settings.engine.effect.plugin_id = Some("example.gain".to_owned());
        settings.engine.effect.params.insert(3, 0.25);
        settings.presets.insert("Quiet studio".to_owned(), PipelinePreset::capture(&settings.engine));
        let restored = parse(&serde_json::to_vec(&settings).unwrap()).unwrap();
        let mut destination = EngineConfig::default();
        destination.devices.output = Some("another speaker".into());
        destination.processing.latency_readout_enabled = false;
        let route = destination.devices.clone();
        let io = destination.io.clone();
        restored.presets["Quiet studio"].clone().apply(&mut destination);
        assert_eq!(PipelinePreset::capture(&destination), restored.presets["Quiet studio"]);
        assert_eq!((destination.devices, destination.io), (route, io));
        assert!(destination.processing.meters_enabled);
        assert!(!destination.processing.latency_readout_enabled);
    }

    #[test]
    fn old_settings_keep_wasapi_and_direct_output_and_effect_settings_survive_a_relaunch() {
        let mut settings = parse(br#"{"engine":{"devices":{"output":"cable"}}}"#).unwrap();
        assert_eq!(settings.engine.io.output_backend, nf_engine::OutputBackend::Wasapi);
        assert_eq!(settings.engine.effect, nf_engine::EffectConfig::default());
        settings.engine.io.output_backend = nf_engine::OutputBackend::DriverRing;
        settings.engine.effect = nf_engine::EffectConfig {
            path: Some("C:/Effects/Studio Gain.clap".into()),
            plugin_id: Some("test.gain".into()),
            bypass: true,
            params: [(7, 0.25)].into(),
            worker_blocks: 2,
        };
        assert_eq!(parse(&serde_json::to_vec_pretty(&settings).unwrap()).unwrap(), settings);
    }

    /// The I/O switches: each reads and writes its own field, the latency options are on by default (the owner, 2026-11-02),
    /// and a change goes through the settings file and back.
    #[test]
    fn the_io_switches_round_trip_through_the_settings_file() {
        let all: Vec<IoSwitch> = IoSwitch::LATENCY.iter().chain(&IoSwitch::STREAMS).copied().collect();
        let defaults = IoConfig::default();
        for s in IoSwitch::LATENCY {
            assert!(s.get(&defaults), "{s:?} on by default");
            assert!(s.description().is_some_and(|d| d.contains("Turn off")), "{s:?} says when to turn it off");
        }
        for s in &all {
            for on in [false, true] {
                let mut settings = Settings::default();
                s.set(&mut settings.engine.io, on);
                assert_eq!(s.get(&settings.engine.io), on, "{s:?}");
                // Only its own field
                for other in all.iter().filter(|o| *o != s) {
                    assert_eq!(other.get(&settings.engine.io), other.get(&defaults), "{s:?} moved {other:?}");
                }
                let back = parse(&serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
                assert_eq!(back, settings, "{s:?} = {on}");
            }
        }
        let labels: Vec<&str> = all.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), labels.iter().collect::<std::collections::HashSet<_>>().len(), "one label each");
        let mic = IoSwitch::ExclusiveMic.description().unwrap();
        assert!(mic.starts_with("Lowest latency. While the filter runs, other apps can't open your microphone directly"));
        assert!(mic.contains("raw microphone"));
        // Its two other costs (the longer echo floor, and the one-period buffer's glitches) as what to try.
        assert!(mic.ends_with("With echo cancelling on, if echo gets through or your voice crackles, try turning this off."));
    }

    /// The latency options came in with off as their default (e074ff7) and went on (the owner, 2026-11-02): a file that
    /// doesn't name them gets them on, and one that saved them off keeps them off.
    #[test]
    fn older_files_get_the_latency_options_on_and_a_saved_off_stays_off() {
        let old = br#"{"engine": {"devices": {"output": "cable"}, "io": {"raw_mic": false, "output_margin_ms": 3.0}}, "ui": {}}"#;
        let io = parse(old).unwrap().engine.io;
        assert!(io.output_exclusive && io.mic_exclusive && io.adaptive_output);
        assert!(!io.raw_mic && io.output_margin_ms == 3.0);
        let off = br#"{"engine": {"io": {"output_exclusive": false, "mic_exclusive": false, "adaptive_output": false}}}"#;
        let io = parse(off).unwrap().engine.io;
        assert!(!io.output_exclusive && !io.mic_exclusive && !io.adaptive_output);
        let some = br#"{"engine": {"io": {"mic_exclusive": false}}}"#;
        let io = parse(some).unwrap().engine.io;
        assert!(io.output_exclusive && !io.mic_exclusive && io.adaptive_output);
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        assert_eq!(parse(b"{}").unwrap(), Settings::default());
        // An older file: only some of the engine's fields, none of the window's.
        let old = br#"{"engine": {"processing": {"aec_enabled": true, "gate": {"mode": "Static"}}, "devices": {"output": "cable"}}}"#;
        let s = parse(old).unwrap();
        let mut expected = Settings::default();
        expected.engine.processing.aec_enabled = true;
        expected.engine.processing.gate.mode = GateMode::Static;
        expected.engine.devices.output = Some("cable".to_owned());
        assert_eq!(s, expected);
        // A newer file: unknown fields are ignored.
        let newer = br#"{"version": 7, "engine": {"processing": {"model": "fast"}}, "ui": {"tray": true, "test_repetitions": 3}}"#;
        let s = parse(newer).unwrap();
        assert_eq!(s.ui.test_repetitions, 3);
        assert_eq!(s.engine, EngineConfig::default());
        assert_eq!(s.version, 7, "kept, so a save doesn't make it older");
    }

    /// The automatic model depth (TODO 5): a file from before it (no version) holds the depth the window saved by default,
    /// 2, which becomes automatic; any other depth there, and any depth in a file of this version, 2 included, is kept. The
    /// window saves the version.
    #[test]
    fn an_older_file_s_saved_depth_becomes_automatic() {
        let depth = |json: &str| parse(json.as_bytes()).unwrap().engine.processing.model_depth;
        assert_eq!(depth(r#"{"engine": {"processing": {"model_depth": 2}}}"#), nf_engine::AUTO_MODEL_DEPTH);
        assert_eq!(depth(r#"{"engine": {"processing": {"model_depth": 1}}}"#), 1);
        assert_eq!(depth(r#"{"engine": {"processing": {"model_depth": 3}}}"#), 3);
        assert_eq!(depth(r#"{"version": 1, "engine": {"processing": {"model_depth": 2}}}"#), 2);
        assert_eq!(depth("{}"), nf_engine::AUTO_MODEL_DEPTH);
        let saved: serde_json::Value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(saved["version"], SETTINGS_VERSION, "{saved}");
    }

    #[test]
    fn a_bom_is_allowed() {
        assert_eq!(parse(b"\xEF\xBB\xBF{}").unwrap(), Settings::default());
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let json = br#"{
            "engine": {
                "processing": {"floor_db": -500, "gate": {"threshold_db": 3, "attack_ms": 0, "release_ms": 1e9, "hold_ms": -1, "range_db": 10, "auto_margin_db": 99}},
                "io": {"output_margin_ms": 100},
                "devices": {"mic": "  ", "reference": ""}
            },
            "ui": {"test_repetitions": 0, "window_width": 1e12, "window_height": 10, "test_probe": ""}
        }"#;
        let s = parse(json).unwrap();
        let p = &s.engine.processing;
        assert_eq!((p.floor_db, p.gate.threshold_db, p.gate.attack_ms), (-60.0, -10.0, 0.5));
        assert_eq!((p.gate.release_ms, p.gate.hold_ms, p.gate.range_db, p.gate.auto_margin_db), (1000.0, 0.0, 0.0, 20.0));
        assert_eq!(s.engine.io.output_margin_ms, 20.0);
        assert_eq!((&s.engine.devices.mic, &s.engine.devices.reference, &s.ui.test_probe), (&None, &None, &None));
        assert_eq!(s.ui.test_repetitions, MIN_REPETITIONS);
        assert_eq!((s.ui.window_width, s.ui.window_height), (MAX_SIDE, MIN_HEIGHT));
        let s = parse(br#"{"ui": {"test_repetitions": 400}}"#).unwrap();
        assert_eq!(s.ui.test_repetitions, MAX_REPETITIONS);
    }

    #[test]
    fn nan_becomes_the_default() {
        let mut s = Settings::default();
        s.engine.processing.floor_db = f32::NAN;
        s.engine.processing.gate.attack_ms = f32::NAN;
        s.ui.window_width = f64::NAN;
        s.ui.window_height = f64::INFINITY;
        s.sanitize();
        assert_eq!(s, Settings::default());
        assert_eq!(Span { min: 1.0, max: 2.0, step: 1.0 }.clamp(f32::NAN, f32::NAN), 1.0);
    }

    #[test]
    fn the_defaults_are_within_every_span() {
        let mut s = Settings::default();
        s.sanitize();
        assert_eq!(s, Settings::default(), "sanitising the defaults changes nothing");
        for span in [FLOOR_DB, THRESHOLD_DB, MARGIN_DB, ATTACK_MS, RELEASE_MS, HOLD_MS, RANGE_DB, OUTPUT_MARGIN_MS] {
            // Slider::step_frequency asserts > 0; minimum < maximum keeps the slider usable.
            assert!(span.step > 0.0 && span.min < span.max, "{span:?}");
        }
    }

    #[test]
    fn test_mode_indices_match_the_labels() {
        for (i, _) in TestMode::LABELS.iter().enumerate() {
            assert_eq!(TestMode::from_index(i).map(TestMode::index), Some(i));
        }
        assert_eq!(TestMode::from_index(TestMode::LABELS.len()), None);
        assert_eq!(TestMode::Acoustic.engine(), LatencyMode::Acoustic);
        assert_eq!(TestMode::Virtual.engine(), LatencyMode::VirtualLoop);
    }

    #[test]
    fn a_missing_file_is_a_first_run() {
        let dir = temp_dir("missing");
        let loaded = load(&dir.join("settings.json"));
        assert_eq!((loaded.settings, loaded.warning), (Settings::default(), None));
    }

    #[test]
    fn a_bad_file_is_kept_as_bad_and_the_defaults_used() {
        let dir = temp_dir("bad");
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ \"engine\": { not json").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, Settings::default());
        let warning = loaded.warning.expect("a warning");
        assert!(warning.contains("settings.json.bad"), "{warning}");
        assert!(!path.exists(), "the bad file was moved aside");
        assert_eq!(std::fs::read(bad_path(&path)).unwrap(), b"{ \"engine\": { not json");

        // A wrong type is as bad as broken syntax, and an older .bad is replaced.
        std::fs::write(&path, br#"{"engine": {"processing": {"floor_db": "loud"}}}"#).unwrap();
        assert!(load(&path).warning.is_some());
        assert!(std::fs::read_to_string(bad_path(&path)).unwrap().contains("loud"));
    }

    #[test]
    fn bad_path_appends_to_the_file_name() {
        assert_eq!(bad_path(Path::new(r"C:\a\settings.json")), PathBuf::from(r"C:\a\settings.json.bad"));
    }

    #[test]
    fn write_then_load_round_trips_and_creates_the_folder() {
        let dir = temp_dir("write");
        let path = dir.join("noise-filter").join("settings.json");
        write(&path, &changed()).unwrap();
        let loaded = load(&path);
        assert_eq!((loaded.settings, loaded.warning), (changed(), None));
        assert!(!temp_path(&path).exists(), "the temporary file was renamed");
        // Overwriting an existing file works (rename replaces on Windows).
        write(&path, &Settings::default()).unwrap();
        assert_eq!(load(&path).settings, Settings::default());
    }

    #[test]
    fn the_saver_writes_the_last_change_once_changes_stop() {
        let dir = temp_dir("debounce");
        let path = dir.join("settings.json");
        let saver = Saver::spawn(path.clone(), Duration::from_millis(150));
        let handle = saver.handle();
        let mut s = Settings::default();
        let started = Instant::now();
        for reps in 1..=10 {
            s.ui.test_repetitions = reps;
            handle.save(&s);
        }
        // A burst of changes: nothing is written until the debounce has passed.
        if started.elapsed() < Duration::from_millis(100) {
            assert!(!path.exists(), "written during the burst");
        }
        // A stall longer than the debounce mid-burst could write an earlier value first: wait for the last
        let deadline = Instant::now() + Duration::from_secs(5);
        while load(&path).settings.ui.test_repetitions != 10 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(load(&path).settings.ui.test_repetitions, 10);
        assert_eq!(handle.errors().take(), None);
        saver.finish();
    }

    #[test]
    fn finish_flushes_a_pending_save() {
        let dir = temp_dir("finish");
        let path = dir.join("settings.json");
        let saver = Saver::spawn(path.clone(), Duration::from_secs(60));
        saver.handle().save(&changed());
        saver.finish();
        assert_eq!(load(&path).settings, changed());
    }

    #[test]
    fn save_errors_are_reported_once() {
        let dir = temp_dir("error");
        // A folder where the file should be: the rename fails.
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&path).unwrap();
        let saver = Saver::spawn(path, Duration::from_millis(10));
        let handle = saver.handle();
        handle.save(&Settings::default());
        saver.finish();
        let errors = handle.errors();
        assert!(errors.take().is_some_and(|e| e.contains("couldn't be saved")));
        assert_eq!(errors.take(), None);
        // After the thread has gone, a save is written synchronously (and fails the same way here).
        handle.save(&Settings::default());
        assert!(errors.take().is_some());
    }

    /// gui/F7: a file another program holds without read sharing can't be read or renamed. The
    /// defaults are used, but nothing may write over the user's file.
    #[test]
    fn a_locked_file_is_left_alone_and_nothing_is_saved() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = temp_dir("locked");
        let path = dir.join("settings.json");
        let theirs = serde_json::to_vec_pretty(&changed()).unwrap();
        std::fs::write(&path, &theirs).unwrap();
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        let loaded = load(&path);
        drop(lock);
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.read_only, "saving is off for the session");
        let warning = loaded.warning.expect("a warning");
        assert!(warning.contains("couldn't be read") && warning.contains("won't be saved"), "{warning}");
        assert!(!bad_path(&path).exists());

        // The session's saver drops every save, even once the file is readable again
        let saver = Saver::disabled(path.clone());
        let handle = saver.handle();
        handle.save(&Settings::default());
        saver.finish();
        handle.save(&Settings::default());
        assert_eq!(std::fs::read(&path).unwrap(), theirs, "the user's settings are intact");
        assert!(!temp_path(&path).exists());
        assert_eq!(handle.errors().take(), None);
    }

    /// gui/F7: only a missing file or folder is a first run. An unreachable share or drive is
    /// NotFound to std as well, but the user's file may be there: it's treated as unreadable.
    #[test]
    fn only_a_missing_file_or_folder_is_a_first_run() {
        let dir = temp_dir("first-run");
        let path = dir.join("noise-filter").join("settings.json");
        let loaded = load(&path);
        assert_eq!((loaded.warning, loaded.read_only), (None, false), "the folder doesn't exist yet");
        for missing in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
            assert!(first_run(&io::Error::from_raw_os_error(missing.0 as i32), &path), "{missing:?}");
        }
        assert!(first_run(&io::Error::from(io::ErrorKind::NotFound), &path));
        // ERROR_INVALID_DRIVE, ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME: std's NotFound too
        for unreachable in [15, 53, 67] {
            let e = io::Error::from_raw_os_error(unreachable);
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
            assert!(!first_run(&e, &path), "{e}");
        }
        assert!(!first_run(&io::Error::from(io::ErrorKind::PermissionDenied), &path));
        // An unmapped drive letter reads as "path not found" too (std::fs::read on Q:\ gives 3),
        // but AppData itself is missing then: an unreadable file, not a first run.
        let unmapped = dir.join("no-such-appdata").join("noise-filter").join("settings.json");
        assert!(!first_run(&io::Error::from_raw_os_error(ERROR_PATH_NOT_FOUND.0 as i32), &unmapped));
        let loaded = load(&unmapped);
        assert!(loaded.warning.is_some() && loaded.read_only, "{loaded:?}");
    }

    /// gui/F7: an unreadable file that can be moved aside is kept as `.bad`, like one that doesn't
    /// parse, and saving stays on. (A folder where the file should be: reading it is refused.)
    #[test]
    fn an_unreadable_file_is_kept_as_bad() {
        let dir = temp_dir("unreadable");
        let path = dir.join("settings.json");
        std::fs::create_dir_all(path.join("inside")).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(!loaded.read_only);
        let warning = loaded.warning.expect("a warning");
        assert!(warning.contains("couldn't be read") && warning.contains("settings.json.bad"), "{warning}");
        assert!(!path.exists() && bad_path(&path).join("inside").is_dir(), "moved aside whole");
        // Saving works again: the path is free
        write(&path, &changed()).unwrap();
        assert_eq!(load(&path).settings, changed());
    }

    /// gui/F9: the temporary file is flushed before it's renamed into place.
    #[test]
    fn writes_are_flushed_before_the_rename() {
        let dir = temp_dir("synced");
        let path = dir.join("settings.json");
        let before = SYNCED.load(Ordering::Relaxed);
        write(&path, &changed()).unwrap();
        assert!(SYNCED.load(Ordering::Relaxed) > before, "write_synced ran");
        assert_eq!(load(&path).settings, changed());

        // A temporary file left by a crash is replaced, not appended to
        std::fs::write(temp_path(&path), b"\0\0\0\0 garbage from before the crash").unwrap();
        write(&path, &Settings::default()).unwrap();
        let loaded = load(&path);
        assert_eq!((loaded.settings, loaded.warning, loaded.read_only), (Settings::default(), None, false));
        assert!(!temp_path(&path).exists());
    }

    /// gui/F9: a write that fails before the rename leaves the old file as it was.
    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let dir = temp_dir("failed-write");
        let path = dir.join("settings.json");
        write(&path, &changed()).unwrap();
        // A folder where the temporary file goes: creating it fails
        std::fs::create_dir_all(temp_path(&path)).unwrap();
        assert!(write(&path, &Settings::default()).is_err());
        assert_eq!(load(&path).settings, changed());
    }

    #[test]
    fn handles_compare_by_saver() {
        let dir = temp_dir("eq");
        let a = Saver::spawn(dir.join("a.json"), SAVE_DEBOUNCE);
        let b = Saver::spawn(dir.join("b.json"), SAVE_DEBOUNCE);
        assert_eq!(a.handle(), a.handle());
        assert_ne!(a.handle(), b.handle());
        a.finish();
        b.finish();
    }
}
