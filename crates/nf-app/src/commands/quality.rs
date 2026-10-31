//! `quality`: plays a test scene through the running filter and measures what comes back (TODO #14: Lab phase 0's live half).
//!
//! It builds the scene (`quality::signals`), starts the engine in-process with `run`'s options (for the echo test, with --far as
//! the echo reference and AEC on unless --no-aec), waits for it to run and settle, then plays the scene into --probe, and the
//! far voice into --far, while recording --capture (`nf_engine::play_record`, on this thread). Then it stops the engine and
//! reports the alignment and the levels (`quality::analysis`), the engine's own drift and echo estimates, and the dropouts: the
//! engine's counters over the take and the recording's own gaps. Exit 0 when the recording lined up with the scene, 3 when it
//! didn't (or the test was cancelled), 2 when something failed.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use nf_core::SAMPLE_RATE;
use nf_engine::{
    DeviceInfo, Engine, EngineConfig, EngineState, Flow, PlayRecordRequest, PlayRecordResult, PlayTrack, TelemetrySnapshot, TrackRole,
};

use super::run::stream_text;
use super::{db, fixed, list_devices};
use crate::cli::{EngineArgs, QualityArgs};
use crate::config::{self, resolve_device};
use crate::exit::Failure;
use crate::quality::QualityTest;
use crate::quality::analysis::{self, AlignmentFailure, Analysis, Levels, MIN_REGION_S, Metrics, SpeechLevels, WINDOW_S};
use crate::quality::signals::{self, Scene};
use crate::{console, wav};

/// Once the engine runs, the time its streams and drift loops get to settle before the scene plays.
const SETTLE: Duration = Duration::from_millis(1500);
/// How long a just-started engine may take to report running.
const RUNNING_TIMEOUT: Duration = Duration::from_secs(5);
/// The report shows levels at or below this as -inf (as [`db`] does).
const FLOOR_DBFS: f32 = -120.0;

pub fn quality(args: &QualityArgs) -> Result<(), Failure> {
    let user = signals::read_user_audio(args.speech.as_deref(), args.noise.as_deref(), args.far_speech.as_deref())?;
    let scene = signals::build(args.test, args.seconds, args.snr_db, &user).map_err(Failure::error)?;
    say!("{}", scene.description);
    let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    let cfg = config::engine_config(&engine_args(args), || Ok(devices.clone()))?;
    let probe = resolve_device(&devices, &args.probe, Flow::Render, "--probe").map_err(Failure::error)?;
    let capture = resolve_device(&devices, &args.capture, Flow::Capture, "--capture").map_err(Failure::error)?;
    let far = args.far.as_deref().map(|f| resolve_device(&devices, f, Flow::Render, "--far")).transpose().map_err(Failure::error)?;
    check_devices(&cfg, &devices, &probe, &capture, far.as_deref()).map_err(Failure::usage)?;
    let route = route(&devices, &probe, far.as_deref(), &capture);

    let engine = super::run::start(&cfg)?;
    let take = play(&engine, &cfg, &scene, Route { probe, capture, far, text: route });
    engine.stop();
    console::finished();
    let take = take?;
    say!("Analysing the recording...");
    let result = analysis::analyse(&scene, &take.recorded.recording, take.recorded.lead_frames);
    say!("{}", format_report(scene.test, &result, &take));
    if let Some(dir) = &args.save {
        let names = save(dir, &scene, &take.recorded.recording, result.as_ref().ok().map(|a| a.aligned.as_slice()))?;
        say!("Wrote {} to {}.", names.join(", "), dir.display());
    }
    match result {
        Ok(_) => Ok(()),
        Err(f) => Err(Failure::no_result(format!(
            "{} Check that --probe feeds the filter's mic and that --capture records its output.",
            f.reason
        ))),
    }
}

/// `run`'s options as the quality test starts the engine with them: the echo test takes its reference from --far, with AEC on
/// unless --no-aec was given (which measures the path without it, for comparison).
fn engine_args(args: &QualityArgs) -> EngineArgs {
    let mut e = args.engine.clone();
    if args.test == QualityTest::Echo {
        e.reference.clone_from(&args.far);
        e.processing.aec.get_or_insert(true);
    }
    e
}

/// The devices must make a loop through the filter: the probe isn't its output, the capture isn't its mic, the far end is
/// neither the probe nor the output, and echo cancellation doesn't take the probe (the scene itself) as its reference.
fn check_devices(cfg: &EngineConfig, devices: &[DeviceInfo], probe: &str, capture: &str, far: Option<&str>) -> Result<(), String> {
    let (mic, output) = (cfg.devices.mic.as_deref(), cfg.devices.output.as_deref());
    if output == Some(probe) {
        return Err("--probe is the filter's own output: play into the cable its mic records instead (e.g. \"CABLE-A Input\")".into());
    }
    if mic == Some(capture) {
        return Err("--capture is the filter's own mic: record the output cable's recording side instead (e.g. \"CABLE Output\")".into());
    }
    if let Some(far) = far {
        if far == probe {
            return Err("--far must be another device than --probe: the far voice reaches the mic only as the scene's echo".into());
        }
        if output == Some(far) {
            return Err("--far is the filter's own output: give the far end's speakers instead (e.g. \"CABLE-B Input\")".into());
        }
    }
    if cfg.processing.aec_enabled {
        let default_playback = devices.iter().find(|d| d.flow == Flow::Render && d.is_default).map(|d| d.id.as_str());
        let fix = "it would take the scene itself as the far end and cancel it. Give the far end's speakers as --reference, or turn \
                   echo cancellation off (--no-aec)";
        match cfg.devices.reference.as_deref() {
            Some(r) if r == probe => return Err(format!("Echo cancellation's reference is --probe: {fix}")),
            None if default_playback == Some(probe) => {
                return Err(format!("Echo cancellation takes its reference from the default playback device, which is --probe: {fix}"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// "playing on CABLE-A Input and CABLE-B Input, recording CABLE Output", by the devices' names.
fn route(devices: &[DeviceInfo], probe: &str, far: Option<&str>, capture: &str) -> String {
    let name = |id: &str| devices.iter().find(|d| d.id == id).map_or_else(|| id.to_owned(), |d| d.name.clone());
    let far = far.map_or(String::new(), |f| format!(" and {}", name(f)));
    format!("playing on {}{far}, recording {}", name(probe), name(capture))
}

/// The take's devices (ids), and the route by their names, for the messages.
struct Route {
    probe: String,
    capture: String,
    far: Option<String>,
    text: String,
}

/// What one take gave.
pub struct Take {
    pub recorded: PlayRecordResult,
    pub dropouts: Dropouts,
    pub engine: EngineReadings,
    pub warnings: Vec<String>,
}

fn play(engine: &Engine, cfg: &EngineConfig, scene: &Scene, route: Route) -> Result<Take, Failure> {
    wait_running(engine)?;
    if console::sleep_unless_stopped(SETTLE) {
        return Err(Failure::no_result("Cancelled before the test started."));
    }
    let before = engine.telemetry();
    let mut play = vec![PlayTrack { device: route.probe, role: TrackRole::Probe, samples: &scene.input }];
    if let (Some(device), Some(samples)) = (route.far, scene.far.as_deref()) {
        play.push(PlayTrack { device, role: TrackRole::FarEnd, samples });
    }
    let request = PlayRecordRequest { play, capture_device: route.capture, cancel: Some(console::stop_flag()), ..Default::default() };
    say!(
        "Playing the scene: {} s, after {} s of silence, recording {} s past its end. Ctrl+C cancels.",
        fixed(scene.input.len() as f64 / f64::from(SAMPLE_RATE), 1),
        fixed(f64::from(request.lead_s), 0),
        fixed(f64::from(request.tail_s), 0)
    );
    // The take blocks this thread: say now if Ctrl+C can't reach it.
    if let Some(w) = console::CtrlCWatch::default().check() {
        say!("{w}");
    }
    let recorded = nf_engine::play_record(&request).map_err(|e| {
        if console::stop_requested() {
            Failure::no_result("The quality test was cancelled.")
        } else {
            Failure::error(format!("The quality test couldn't play or record ({}): {e}", route.text))
        }
    })?;
    let after = engine.telemetry();
    if after.state != EngineState::Running {
        let why = engine.status().message.map_or(String::new(), |m| format!(": {m}"));
        return Err(Failure::error(format!("The filter stopped during the test{why}")));
    }
    let mut warnings = Vec::new();
    if after.blocks < before.blocks {
        warnings.push("The filter restarted during the test, so its dropout counts cover only the time since.".to_owned());
    }
    let readings = EngineReadings::of(cfg, &after);
    Ok(Take { dropouts: Dropouts::between(&before, &after, &recorded), recorded, engine: readings, warnings })
}

/// `run::start` returns once the streams are open; this checks the engine says so.
fn wait_running(engine: &Engine) -> Result<(), Failure> {
    let deadline = Instant::now() + RUNNING_TIMEOUT;
    loop {
        match engine.telemetry().state {
            EngineState::Running => return Ok(()),
            EngineState::Starting if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let why = engine.status().message.map_or(String::new(), |m| format!(": {m}"));
                return Err(Failure::error(format!("The filter isn't running{why}")));
            }
        }
    }
}

/// What the engine ran, and its own estimates at the end of the take: the drift loops', and AEC3's (the echo delay it found,
/// and its ERLE), which show what a measured ERLE went with.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EngineReadings {
    pub aec: bool,
    pub noise_filter: bool,
    pub output_drift_ppm: f32,
    pub reference_drift_ppm: f32,
    pub aec_delay_ms: f32,
    pub aec_erle_db: f32,
}

impl EngineReadings {
    fn of(cfg: &EngineConfig, t: &TelemetrySnapshot) -> Self {
        Self {
            aec: cfg.processing.aec_enabled,
            noise_filter: cfg.processing.noise_enabled,
            output_drift_ppm: t.output_drift_ppm,
            reference_drift_ppm: t.reference_drift_ppm,
            aec_delay_ms: t.aec_delay_ms,
            aec_erle_db: t.aec_erle_db,
        }
    }

    /// "output drift +1.2 ppm, reference drift -0.4 ppm; AEC3's estimates: echo delay 40.0 ms, ERLE 31.2 dB".
    fn line(&self) -> String {
        let ppm = |v: f32| if v.is_finite() { format!("{v:+.1}") } else { "--".into() };
        let mut s = format!("{:<26} output drift {} ppm", "engine", ppm(self.output_drift_ppm));
        if self.aec {
            let _ = write!(
                s,
                ", reference drift {} ppm; AEC3's estimates: echo delay {} ms, ERLE {} dB",
                ppm(self.reference_drift_ppm),
                fixed(f64::from(self.aec_delay_ms), 1),
                fixed(f64::from(self.aec_erle_db), 1)
            );
        }
        s
    }
}

/// The dropouts during a take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropouts {
    /// The engine's counters over the take: mic glitches, output underruns, reference gaps and mic stalls.
    pub capture_glitches: u64,
    pub render_underruns: u64,
    pub reference_gaps: u64,
    pub stalls: u64,
    /// The test's own streams: jumps in the recording (and the frames zero-filled for them), and playback running dry.
    pub capture_gaps: u64,
    pub gap_frames: u64,
    pub playback_underruns: u64,
}

impl Dropouts {
    /// The engine's counters from `before` to `after` (they're published once a second, so the tail covers the scene's end),
    /// and the recording's.
    pub fn between(before: &TelemetrySnapshot, after: &TelemetrySnapshot, recorded: &PlayRecordResult) -> Self {
        Self {
            capture_glitches: after.capture_glitches.saturating_sub(before.capture_glitches),
            render_underruns: after.render_underruns.saturating_sub(before.render_underruns),
            reference_gaps: after.reference_gaps.saturating_sub(before.reference_gaps),
            stalls: after.stalls.saturating_sub(before.stalls),
            capture_gaps: recorded.capture_gaps,
            gap_frames: recorded.gap_frames,
            playback_underruns: recorded.playback_underruns,
        }
    }

    fn line(&self) -> String {
        format!(
            "{:<27}filter: mic glitches {}, output underruns {}, reference gaps {}, stalls {}; test streams: capture gaps {} ({} frames), \
             playback underruns {}",
            "dropouts",
            self.capture_glitches,
            self.render_underruns,
            self.reference_gaps,
            self.stalls,
            self.capture_gaps,
            self.gap_frames,
            self.playback_underruns
        )
    }
}

/// Writes the scene's signals, the recording and (when it lined up) the aligned recording into `dir` as 48 kHz mono float
/// WAVs, and returns their names.
fn save(dir: &Path, scene: &Scene, recording: &[f32], aligned: Option<&[f32]>) -> Result<Vec<&'static str>, Failure> {
    std::fs::create_dir_all(dir).map_err(|e| Failure::error(format!("Couldn't create {}: {e}", dir.display())))?;
    let mut files: Vec<(&'static str, &[f32])> = vec![("input.wav", &scene.input), ("clean.wav", &scene.clean)];
    if let Some(far) = &scene.far {
        files.push(("far.wav", far));
    }
    files.push(("recording.wav", recording));
    if let Some(a) = aligned {
        files.push(("aligned.wav", a));
    }
    for (name, x) in &files {
        wav::write_mono_f32(&dir.join(name), x)?;
    }
    Ok(files.into_iter().map(|(name, _)| name).collect())
}

/// The report: the alignment, each region's levels, the engine's readings, the dropouts, the streams, notes and warnings.
pub fn format_report(test: QualityTest, result: &Result<Analysis, AlignmentFailure>, take: &Take) -> String {
    let seconds = |samples: i64| fixed(samples as f64 / f64::from(SAMPLE_RATE), 4);
    let lead = take.recorded.lead_frames;
    let mut s = String::new();
    let mut notes = Vec::new();
    let mut warnings = take.warnings.clone();
    match result {
        Ok(a) => {
            let al = &a.alignment;
            let ms = |samples: f64| fixed((samples - lead as f64) * 1000.0 / f64::from(SAMPLE_RATE), 2);
            let _ = write!(
                s,
                "Quality, {} test: the recording lines up {} s in ({} ms after the lead), correlation {} (confidence {} dB)",
                test.name(),
                seconds(al.delay),
                fixed(al.path_ms(lead), 1),
                fixed(f64::from(al.correlation), 2),
                fixed(f64::from(al.confidence_db), 1)
            );
            let drift = if al.drift_ppm.is_finite() { format!(", drift {:+.1} ppm", al.drift_ppm) } else { String::new() };
            let _ = write!(
                s,
                "\n  {:<26} {} of {} windows of {WINDOW_S:.0} s lined up, the lowest at {}; delay {} to {} ms after the lead{drift}",
                "alignment",
                al.windows_aligned,
                al.windows,
                fixed(f64::from(al.correlation_low), 2),
                ms(al.delay_low),
                ms(al.delay_high)
            );
            for line in metric_lines(&a.metrics, &take.engine) {
                let _ = write!(s, "\n  {line}");
            }
            if test == QualityTest::Echo && take.engine.noise_filter {
                notes.push(
                    "The noise filter was on, so the echo's drop is the whole chain's echo reduction, not the echo canceller's ERLE \
                     alone (--no-ns measures that)."
                        .to_owned(),
                );
            }
            warnings.extend(a.warnings.iter().cloned());
        }
        Err(f) => {
            let _ = write!(s, "Quality, {} test: the recording didn't line up with the scene", test.name());
            if let Some(al) = &f.alignment {
                let _ = write!(
                    s,
                    " (its windows' median correlation is {}, at about {} s in)",
                    fixed(f64::from(al.correlation), 2),
                    seconds(al.delay)
                );
            }
        }
    }
    let _ = write!(s, "\n  {}", take.engine.line());
    let _ = write!(s, "\n  {}", take.dropouts.line());
    let played: Vec<String> = take.recorded.playback.iter().map(stream_text).collect();
    let _ = write!(s, "\n  played on {}; recorded from {}", played.join(" and "), stream_text(&take.recorded.capture));
    for n in &notes {
        let _ = write!(s, "\n  note: {n}");
    }
    for w in &warnings {
        let _ = write!(s, "\n  warning: {w}");
    }
    s
}

fn metric_lines(m: &Metrics, engine: &EngineReadings) -> Vec<String> {
    match m {
        Metrics::Noise { pauses, steady_pauses, speech } => {
            let mut lines = vec![row("noise in pauses", "input", pauses, "change", change(pauses))];
            if steady_pauses.seconds >= MIN_REGION_S {
                lines.push(row("  200 ms from speech", "input", steady_pauses, "change", change(steady_pauses)));
            }
            lines.push(speech_row("speech in its cores", speech));
            lines
        }
        Metrics::Echo { far_only, far_only_turns, converging, near_only, double_talk, both_talking } => {
            let measure = if engine.noise_filter { "drop" } else { "ERLE" };
            let mut lines = vec![row("echo, far end alone", "input", &far_only.levels, measure, fall(&far_only.levels))];
            if !far_only_turns.is_empty() {
                let mut drops: Vec<f32> = far_only_turns.iter().map(|t| -t.change_db()).collect();
                let each: Vec<String> = far_only_turns.iter().map(fall).collect();
                drops.sort_by(f32::total_cmp);
                let median =
                    if drops.len() % 2 == 1 { drops[drops.len() / 2] } else { (drops[drops.len() / 2 - 1] + drops[drops.len() / 2]) / 2.0 };
                lines.push(format!(
                    "{:<26} {} dB: median {}, lowest {}; the first turn (converging) {} dB",
                    "  each far-only turn",
                    each.join(", "),
                    fixed(f64::from(median), 1),
                    fixed(f64::from(drops[0]), 1),
                    fall(converging)
                ));
            }
            lines.push(format!(
                "{:<26} {} dBFS: the most the {measure} can show while the noise passes is {} dB",
                "  the scene's noise there",
                db(far_only.noise_dbfs),
                difference(far_only.levels.reference_dbfs, far_only.noise_dbfs, false)
            ));
            lines.push(speech_row("near speech alone", near_only));
            lines.push(speech_row("near speech, double talk", double_talk));
            lines.push(speech_row("  both mid-syllable", both_talking));
            lines
        }
    }
}

/// "noise in pauses            input -30.0 dBFS, output -51.2 dBFS: change -21.2 dB over 9.8 s".
fn row(label: &str, reference: &str, l: &Levels, measure: &str, value: String) -> String {
    format!(
        "{label:<26} {reference} {} dBFS, output {} dBFS: {measure} {value} dB over {} s",
        db(l.reference_dbfs),
        db(l.output_dbfs),
        fixed(l.seconds, 1)
    )
}

/// "speech in its cores        clean -18.5 dBFS, output -18.9 dBFS (speech -18.9, rest -52.0): change -0.4 dB over 8.1 s".
fn speech_row(label: &str, l: &SpeechLevels) -> String {
    format!(
        "{label:<26} clean {} dBFS, output {} dBFS (speech {}, rest {}): change {} dB over {} s",
        db(l.clean_dbfs),
        db(l.output_dbfs),
        db(l.speech_dbfs),
        db(l.rest_dbfs),
        difference(l.speech_dbfs, l.clean_dbfs, true),
        fixed(l.seconds, 1)
    )
}

/// A region's change, output over reference: `-21.2`, `+0.4`.
fn change(l: &Levels) -> String {
    difference(l.output_dbfs, l.reference_dbfs, true)
}

/// A region's drop, reference over output: `30.5`.
fn fall(l: &Levels) -> String {
    difference(l.reference_dbfs, l.output_dbfs, false)
}

/// `a - b` in dB, as the report shows it beside levels printed by [`db`] (one rule for both): a bound when a level is at or
/// below -120 dBFS, which shows as -inf (`over 95.5`, `under -101.6`); `--` when there's nothing to compare. `signed` puts a
/// sign on it.
fn difference(a: f32, b: f32, signed: bool) -> String {
    let num = |v: f32| if signed { format!("{v:+.1}") } else { format!("{v:.1}") };
    if a.is_nan() || b.is_nan() || (a <= FLOOR_DBFS && b <= FLOOR_DBFS) {
        "--".into()
    } else if b <= FLOOR_DBFS {
        format!("over {}", num(a - FLOOR_DBFS))
    } else if a <= FLOOR_DBFS {
        format!("under {}", num(FLOOR_DBFS - b))
    } else {
        num(a - b)
    }
}

#[cfg(test)]
mod tests {
    use nf_engine::{DeviceSelection, StreamInfo};

    use super::*;
    use crate::cli::Processing;
    use crate::quality::analysis::{Alignment, EchoLevels};
    use crate::quality::signals::UserAudio;

    fn args(test: QualityTest, far: Option<&str>, aec: Option<bool>) -> QualityArgs {
        QualityArgs {
            engine: EngineArgs {
                mic: Some("CABLE-A Output".into()),
                output: Some("CABLE Input".into()),
                processing: Processing { aec, ..Default::default() },
                ..Default::default()
            },
            probe: "CABLE-A Input".into(),
            capture: "CABLE Output".into(),
            test,
            far: far.map(Into::into),
            seconds: 30.0,
            snr_db: None,
            speech: None,
            noise: None,
            far_speech: None,
            save: None,
        }
    }

    #[test]
    fn the_echo_test_runs_aec_on_the_far_end_unless_told_not_to() {
        let e = engine_args(&args(QualityTest::Echo, Some("CABLE-B Input"), None));
        assert_eq!((e.reference.as_deref(), e.processing.aec), (Some("CABLE-B Input"), Some(true)));
        let e = engine_args(&args(QualityTest::Echo, Some("CABLE-B Input"), Some(false)));
        assert_eq!((e.reference.as_deref(), e.processing.aec), (Some("CABLE-B Input"), Some(false)), "--no-aec, for comparison");
        let noise = args(QualityTest::Noise, None, None);
        assert_eq!(engine_args(&noise), noise.engine, "the noise test runs as given");
    }

    fn device(id: &str, name: &str, flow: Flow, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            flow,
            is_default,
            is_default_communications: false,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn the_devices_must_loop_through_the_filter() {
        let list = [device("{probe}", "CABLE-A Input", Flow::Render, false), device("{spk}", "Speakers", Flow::Render, true)];
        let devices = DeviceSelection { mic: Some("{mic}".into()), output: Some("{out}".into()), reference: None };
        let mut cfg = EngineConfig { devices, ..Default::default() };
        assert_eq!(check_devices(&cfg, &list, "{probe}", "{cap}", Some("{far}")), Ok(()));
        let check = |cfg: &EngineConfig, probe, capture, far| check_devices(cfg, &list, probe, capture, far).unwrap_err();
        assert!(check(&cfg, "{out}", "{cap}", None).starts_with("--probe is the filter's own output"));
        assert!(check(&cfg, "{probe}", "{mic}", None).starts_with("--capture is the filter's own mic"));
        assert!(check(&cfg, "{probe}", "{cap}", Some("{probe}")).starts_with("--far must be another device"));
        assert!(check(&cfg, "{probe}", "{cap}", Some("{out}")).starts_with("--far is the filter's own output"));
        // Echo cancellation on, with the probe as its reference, given or by default: it would cancel the scene
        cfg.processing.aec_enabled = true;
        assert_eq!(check_devices(&cfg, &list, "{probe}", "{cap}", None), Ok(()), "the default playback device is the speakers");
        cfg.devices.reference = Some("{probe}".into());
        assert!(check(&cfg, "{probe}", "{cap}", None).starts_with("Echo cancellation's reference is --probe"));
        cfg.devices.reference = None;
        let probe_is_default = [device("{probe}", "CABLE-A Input", Flow::Render, true)];
        let e = check_devices(&cfg, &probe_is_default, "{probe}", "{cap}", None).unwrap_err();
        assert!(e.starts_with("Echo cancellation takes its reference from the default playback device, which is --probe"), "{e}");
        cfg.processing.aec_enabled = false;
        assert_eq!(check_devices(&cfg, &probe_is_default, "{probe}", "{cap}", None), Ok(()), "no echo cancellation, no reference");
        // The default mic: nothing to compare the capture with
        cfg.devices.mic = None;
        assert_eq!(check_devices(&cfg, &list, "{probe}", "{mic}", None), Ok(()));
    }

    #[test]
    fn the_route_names_the_devices() {
        let list = [
            device("{a}", "CABLE-A Input", Flow::Render, false),
            device("{b}", "CABLE-B Input", Flow::Render, false),
            device("{c}", "CABLE Output", Flow::Capture, false),
        ];
        assert_eq!(route(&list, "{a}", Some("{b}"), "{c}"), "playing on CABLE-A Input and CABLE-B Input, recording CABLE Output");
        assert_eq!(route(&list, "{a}", None, "{gone}"), "playing on CABLE-A Input, recording {gone}");
    }

    fn stream(name: &str) -> StreamInfo {
        StreamInfo {
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms: 10.0,
            low_latency: false,
            autoconvert: false,
            raw: false,
        }
    }

    fn take(playback: &[&str], engine: EngineReadings) -> Take {
        Take {
            recorded: PlayRecordResult {
                recording: Vec::new(),
                lead_frames: 48_000,
                capture_gaps: 1,
                gap_frames: 480,
                playback_underruns: 0,
                playback: playback.iter().map(|n| stream(n)).collect(),
                capture: stream("CABLE Output"),
            },
            dropouts: Dropouts { capture_glitches: 2, capture_gaps: 1, gap_frames: 480, ..Default::default() },
            engine,
            warnings: vec!["The filter restarted during the test, so its dropout counts cover only the time since.".into()],
        }
    }

    fn levels(seconds: f64, reference_dbfs: f32, output_dbfs: f32) -> Levels {
        Levels { seconds, reference_dbfs, output_dbfs }
    }

    fn speech(seconds: f64, clean_dbfs: f32, output_dbfs: f32, speech_dbfs: f32, rest_dbfs: f32) -> SpeechLevels {
        SpeechLevels { seconds, clean_dbfs, output_dbfs, speech_dbfs, rest_dbfs }
    }

    const ALIGNMENT: Alignment = Alignment {
        delay: 56_784,
        delay_exact: 56_784.2,
        correlation: 0.968,
        correlation_low: 0.941,
        confidence_db: 39.24,
        windows: 14,
        windows_aligned: 14,
        delay_low: 56_783.5,
        delay_high: 56_785.0,
        drift_ppm: 0.12,
    };

    #[test]
    fn the_report_shows_the_alignment_each_region_the_engine_the_dropouts_and_the_streams() {
        let metrics = Metrics::Noise {
            pauses: levels(9.84, -30.0, -51.25),
            steady_pauses: levels(4.2, -30.0, -52.5),
            speech: speech(8.1, -18.52, -18.9, -18.94, -52.04),
        };
        let ok = Ok(Analysis { alignment: ALIGNMENT, aligned: Vec::new(), metrics, warnings: vec!["a short region".into()] });
        let readings = EngineReadings { noise_filter: true, output_drift_ppm: 1.24, ..Default::default() };
        assert_eq!(
            format_report(QualityTest::Noise, &ok, &take(&["CABLE-A Input"], readings)),
            "Quality, noise test: the recording lines up 1.1830 s in (183.0 ms after the lead), correlation 0.97 (confidence 39.2 dB)\n  \
             alignment                  14 of 14 windows of 2 s lined up, the lowest at 0.94; delay 182.99 to 183.02 ms after the lead, \
             drift +0.1 ppm\n  \
             noise in pauses            input -30.0 dBFS, output -51.2 dBFS: change -21.2 dB over 9.8 s\n  \
             \x20 200 ms from speech       input -30.0 dBFS, output -52.5 dBFS: change -22.5 dB over 4.2 s\n  \
             speech in its cores        clean -18.5 dBFS, output -18.9 dBFS (speech -18.9, rest -52.0): change -0.4 dB over 8.1 s\n  \
             engine                     output drift +1.2 ppm\n  \
             dropouts                   filter: mic glitches 2, output underruns 0, reference gaps 0, stalls 0; test streams: \
             capture gaps 1 (480 frames), playback underruns 0\n  \
             played on CABLE-A Input (48000 Hz, 2 ch, period 10.00 ms); recorded from CABLE Output (48000 Hz, 2 ch, period 10.00 ms)\n  \
             warning: The filter restarted during the test, so its dropout counts cover only the time since.\n  \
             warning: a short region"
        );

        let metrics = Metrics::Echo {
            far_only: EchoLevels { levels: levels(4.0, -24.5, -55.0), noise_dbfs: -84.1 },
            far_only_turns: vec![levels(1.3, -24.5, -52.0), levels(1.4, -24.5, -60.0), levels(1.3, -24.6, -54.6)],
            converging: levels(1.2, -24.4, -36.7),
            near_only: speech(3.2, -18.0, -18.3, -18.31, -50.0),
            double_talk: speech(1.4, -18.4, f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY),
            both_talking: speech(0.4, -18.2, -21.0, -21.3, -32.6),
        };
        let echo = Ok(Analysis { alignment: ALIGNMENT, aligned: Vec::new(), metrics: metrics.clone(), warnings: Vec::new() });
        let aec = EngineReadings {
            aec: true,
            noise_filter: false,
            output_drift_ppm: -2.0,
            reference_drift_ppm: 0.44,
            aec_delay_ms: 40.0,
            aec_erle_db: 31.25,
        };
        let report = format_report(QualityTest::Echo, &echo, &take(&["CABLE-A Input", "CABLE-B Input"], aec));
        for line in [
            "\n  echo, far end alone        input -24.5 dBFS, output -55.0 dBFS: ERLE 30.5 dB over 4.0 s\n",
            "\n    each far-only turn       27.5, 35.5, 30.0 dB: median 30.0, lowest 27.5; the first turn (converging) 12.3 dB\n",
            "\n    the scene's noise there  -84.1 dBFS: the most the ERLE can show while the noise passes is 59.6 dB\n",
            "\n  near speech alone          clean -18.0 dBFS, output -18.3 dBFS (speech -18.3, rest -50.0): change -0.3 dB over 3.2 s\n",
            // A level shown as -inf: the change is a bound, not a number next to it
            "\n  near speech, double talk   clean -18.4 dBFS, output -inf dBFS (speech -inf, rest -inf): change under -101.6 dB \
             over 1.4 s\n",
            "\n    both mid-syllable        clean -18.2 dBFS, output -21.0 dBFS (speech -21.3, rest -32.6): change -3.1 dB over 0.4 s\n",
            "\n  engine                     output drift -2.0 ppm, reference drift +0.4 ppm; AEC3's estimates: echo delay 40.0 ms, \
             ERLE 31.2 dB\n",
        ] {
            assert!(report.contains(line), "{line}\nnot in\n{report}");
        }
        assert!(report.contains("played on CABLE-A Input (48000 Hz, 2 ch, period 10.00 ms) and CABLE-B Input"), "{report}");
        assert!(!report.contains("note:"), "{report}");
        // With the noise filter on, the drop is the chain's, and it's said
        let report = format_report(QualityTest::Echo, &echo, &take(&["A", "B"], EngineReadings { noise_filter: true, ..aec }));
        assert!(report.contains("input -24.5 dBFS, output -55.0 dBFS: drop 30.5 dB over 4.0 s"), "{report}");
        assert!(report.contains("\n  note: The noise filter was on, so the echo's drop is the whole chain's"), "{report}");
        // The output silent where only the far end talks: a bound again
        let Metrics::Echo { far_only_turns, converging, near_only, double_talk, both_talking, .. } = metrics else { panic!() };
        let silent = Metrics::Echo {
            far_only: EchoLevels { levels: levels(4.0, -24.5, f32::NEG_INFINITY), noise_dbfs: -84.1 },
            far_only_turns,
            converging,
            near_only,
            double_talk,
            both_talking,
        };
        let echo = Ok(Analysis { alignment: ALIGNMENT, aligned: Vec::new(), metrics: silent, warnings: Vec::new() });
        let report = format_report(QualityTest::Echo, &echo, &take(&["A", "B"], aec));
        assert!(report.contains("output -inf dBFS: ERLE over 95.5 dB over 4.0 s"), "{report}");

        let failed = Err(AlignmentFailure { alignment: Some(Alignment { correlation: 0.04, ..ALIGNMENT }), reason: "weak".into() });
        let report = format_report(QualityTest::Noise, &failed, &take(&["CABLE-A Input"], readings));
        assert!(
            report.starts_with(
                "Quality, noise test: the recording didn't line up with the scene (its windows' median correlation is 0.04, at about \
                 1.1830 s in)\n  engine"
            ),
            "{report}"
        );
        let silent = Err(AlignmentFailure { alignment: None, reason: "silent".into() });
        let report = format_report(QualityTest::Echo, &silent, &take(&["A"], readings));
        assert!(report.starts_with("Quality, echo test: the recording didn't line up with the scene\n"), "{report}");
    }

    #[test]
    fn differences_follow_the_levels_shown() {
        assert_eq!(difference(-51.25, -30.0, true), "-21.2");
        assert_eq!(difference(-18.0, -18.4, true), "+0.4");
        assert_eq!(difference(-24.5, -55.0, false), "30.5");
        // A level shown as -inf (at or below -120 dBFS) makes the difference a bound
        assert_eq!(difference(-24.5, f32::NEG_INFINITY, false), "over 95.5");
        assert_eq!(difference(-24.5, -130.0, false), "over 95.5");
        assert_eq!(difference(f32::NEG_INFINITY, -18.4, true), "under -101.6");
        assert_eq!(difference(f32::NEG_INFINITY, f32::NEG_INFINITY, true), "--");
        assert_eq!(difference(f32::NAN, -18.4, true), "--");
    }

    #[test]
    fn dropouts_are_the_counters_growth_over_the_take() {
        let before = TelemetrySnapshot { capture_glitches: 3, render_underruns: 1, reference_gaps: 0, stalls: 5, ..Default::default() };
        let after = TelemetrySnapshot { capture_glitches: 4, render_underruns: 1, reference_gaps: 2, stalls: 2, ..Default::default() };
        let t = take(&["A"], EngineReadings::default());
        let d = Dropouts::between(&before, &after, &t.recorded);
        assert_eq!(d, Dropouts {
            capture_glitches: 1,
            render_underruns: 0,
            reference_gaps: 2,
            stalls: 0,
            capture_gaps: 1,
            gap_frames: 480,
            playback_underruns: 0
        });
    }

    #[test]
    fn the_engine_readings_come_from_the_config_and_the_telemetry() {
        let mut cfg = EngineConfig::default();
        cfg.processing.aec_enabled = true;
        let t = TelemetrySnapshot {
            output_drift_ppm: 3.0,
            reference_drift_ppm: -1.0,
            aec_delay_ms: 52.0,
            aec_erle_db: 12.0,
            ..Default::default()
        };
        let r = EngineReadings::of(&cfg, &t);
        assert_eq!(r, EngineReadings {
            aec: true,
            noise_filter: true,
            output_drift_ppm: 3.0,
            reference_drift_ppm: -1.0,
            aec_delay_ms: 52.0,
            aec_erle_db: 12.0
        });
        let off = EngineReadings { output_drift_ppm: f32::NAN, ..Default::default() };
        assert_eq!(off.line(), format!("{:<26} output drift -- ppm", "engine"));
    }

    /// `x` read at `n - d(n)` (4-point Lagrange interpolation): a path whose delay is `d(n)` samples at output sample `n`.
    fn delayed_by(x: &[f32], d: impl Fn(usize) -> f64) -> Vec<f32> {
        let at = |i: i64| usize::try_from(i).ok().and_then(|i| x.get(i)).map_or(0.0, |&v| f64::from(v));
        (0..x.len())
            .map(|n| {
                let t = n as f64 - d(n);
                let (i, u) = (t.floor() as i64, t - t.floor());
                let (a, b, c, e) = (at(i - 1), at(i), at(i + 1), at(i + 2));
                let v = -u * (u - 1.0) * (u - 2.0) / 6.0 * a + (u + 1.0) * (u - 1.0) * (u - 2.0) / 2.0 * b
                    - (u + 1.0) * u * (u - 2.0) / 2.0 * c
                    + (u + 1.0) * u * (u - 1.0) / 6.0 * e;
                v as f32
            })
            .collect()
    }

    /// The alignment's figures through the real chain, offline, with the path's delay drifting, wandering and stepping, and
    /// for recordings that don't hold the scene: the record behind `MIN_CORRELATION`. It prints a table, and checks that the
    /// threshold separates the two with room to spare. Ignored (about a minute): the owner runs it to re-derive the threshold,
    /// `cargo test -p nf-app the_alignment_threshold -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn the_alignment_threshold_holds_through_the_chain() {
        use std::sync::Arc;

        use nf_core::controls::ChainControls;
        use nf_engine::ProcessingConfig;
        use nf_stages::synth::{Voice, delayed, speech_like};

        use super::super::process::run_chain;
        use crate::quality::analysis::{MAX_PATH_DELAY_S, MIN_CORRELATION, analyse};

        const FS: usize = 48_000;
        let chain = |mic: &[f32], reference: &[f32], aec: bool, ns: bool| {
            let cfg = ProcessingConfig { aec_enabled: aec, noise_enabled: ns, ..Default::default() };
            run_chain(&mut nf_stages::build_chain(Arc::new(ChainControls::new(&cfg)), aec).unwrap(), mic, reference, 0)
        };
        let recording = |processed: &[f32], path: usize| {
            let mut r = vec![0.0; FS + path];
            r.extend_from_slice(processed);
            r.extend(std::iter::repeat_n(0.0, 2 * FS));
            r
        };
        let noise = |snr| signals::build(QualityTest::Noise, 30.0, Some(snr), &UserAudio::default()).unwrap();
        let (n10, n0) = (noise(10.0), noise(0.0));
        let echo = signals::build(QualityTest::Echo, 30.0, None, &UserAudio::default()).unwrap();
        // The mic hears the echo 40 ms after the reference plays it, as through a cable
        let mic = delayed(&echo.input, 1_920);
        let far = echo.far.as_deref().unwrap();
        let aligned_cases: Vec<(&str, &Scene, Vec<f32>)> = vec![
            ("noise 10 dB, filter off", &n10, n10.input.clone()),
            ("noise 0 dB, filter off", &n0, n0.input.clone()),
            ("noise 10 dB, RNNoise", &n10, chain(&n10.input, &[], false, true)),
            ("noise 0 dB, RNNoise", &n0, chain(&n0.input, &[], false, true)),
            ("echo, AEC off", &echo, mic.clone()),
            ("echo, AEC3", &echo, chain(&mic, far, true, false)),
            ("echo, AEC3 + RNNoise", &echo, chain(&mic, far, true, true)),
        ];
        let half = 15.0 * FS as f64;
        let impairments: [(&str, &dyn Fn(usize) -> f64); 6] = [
            ("still", &|_| 0.0),
            ("20 ppm", &|n| n as f64 * 20e-6),
            ("100 ppm", &|n| n as f64 * 100e-6),
            ("wander 1 ms / 10 s", &|n| 48.0 * (1.0 + (std::f64::consts::TAU * n as f64 / (10.0 * FS as f64)).sin())),
            ("wander 1 ms / 30 s", &|n| 48.0 * (1.0 + (std::f64::consts::TAU * n as f64 / (30.0 * FS as f64)).sin())),
            ("step 1 ms", &|n| if (n as f64) < half { 0.0 } else { 48.0 }),
        ];
        let mut lowest_aligned = f32::INFINITY;
        println!("{:<24} {:<20} {:>6} {:>6} {:>7} {:>8} {:>8}", "case", "path", "corr", "low", "windows", "ppm", "moved");
        for (name, scene, processed) in &aligned_cases {
            for (what, d) in impairments {
                let rec = recording(&delayed_by(processed, d), 9_000);
                let a = analyse(scene, &rec, FS).unwrap_or_else(|e| panic!("{name}, {what}: {}", e.reason)).alignment;
                println!(
                    "{name:<24} {what:<20} {:>6.3} {:>6.3} {:>4}/{:<2} {:>8.1} {:>6.2}ms",
                    a.correlation,
                    a.correlation_low,
                    a.windows_aligned,
                    a.windows,
                    a.drift_ppm,
                    a.delay_spread_ms()
                );
                lowest_aligned = lowest_aligned.min(a.correlation);
            }
        }
        // A long take drifting: 15 ms over 300 s, which one lag for the whole take lost
        let long = signals::build(QualityTest::Noise, 300.0, None, &UserAudio::default()).unwrap();
        let processed = delayed_by(&chain(&long.input, &[], false, true), |n| n as f64 * 50e-6);
        let a = analyse(&long, &recording(&processed, 9_000), FS).expect("the long take lines up").alignment;
        println!(
            "{:<24} {:<20} {:>6.3} {:>6.3} {:>4}/{:<2} {:>8.1} {:>6.2}ms",
            "noise 10 dB 300 s, RNNoise",
            "50 ppm",
            a.correlation,
            a.correlation_low,
            a.windows_aligned,
            a.windows,
            a.drift_ppm,
            a.delay_spread_ms()
        );
        lowest_aligned = lowest_aligned.min(a.correlation);

        let other = speech_like(30 * FS, Voice::HIGH, 77).samples;
        let misaligned: Vec<(&str, &Scene, Vec<f32>)> = vec![
            ("another talker", &n10, recording(&other, 9_000)),
            ("another talker, RNNoise", &n10, recording(&chain(&other, &[], false, true), 9_000)),
            ("the noise alone", &n10, recording(&n10.noise, 9_000)),
            ("the echo alone", &echo, recording(echo.echo.as_deref().unwrap(), 9_000)),
            ("the scene 4 s off", &n10, recording(&n10.input, (MAX_PATH_DELAY_S * FS as f64) as usize + 2 * FS)),
        ];
        let mut highest_misaligned = 0.0f32;
        for (name, scene, rec) in &misaligned {
            let e = analyse(scene, rec, FS).expect_err(name);
            let c = e.alignment.map_or(0.0, |a| a.correlation);
            println!("{name:<45} {c:>6.3}  ({})", e.alignment.map_or(0, |a| a.windows));
            highest_misaligned = highest_misaligned.max(c);
        }
        println!("lowest aligned {lowest_aligned:.3}, highest misaligned {highest_misaligned:.3}, threshold {MIN_CORRELATION}");
        assert!(lowest_aligned > 1.5 * MIN_CORRELATION && highest_misaligned < 0.5 * MIN_CORRELATION);
    }

    #[test]
    fn saving_writes_the_scene_and_the_recordings() {
        let dir = crate::test_dir("quality-save");
        let out = dir.join("take");
        let scene = signals::build(QualityTest::Echo, 5.0, None, &UserAudio::default()).unwrap();
        let recording = vec![0.25f32; 1000];
        let names = save(&out, &scene, &recording, Some(&scene.clean)).unwrap();
        assert_eq!(names, ["input.wav", "clean.wav", "far.wav", "recording.wav", "aligned.wav"]);
        let read = |name: &str| wav::read_mono_48k(&out.join(name)).unwrap().samples;
        assert_eq!(read("recording.wav"), recording);
        assert_eq!(read("far.wav"), *scene.far.as_ref().unwrap());
        assert_eq!(read("aligned.wav"), scene.clean);
        let noise = signals::build(QualityTest::Noise, 5.0, None, &UserAudio::default()).unwrap();
        let names = save(&out, &noise, &recording, None).unwrap();
        assert_eq!(names, ["input.wav", "clean.wav", "recording.wav"], "no far end, and no alignment to save");
        let _ = std::fs::remove_dir_all(dir);
    }
}
