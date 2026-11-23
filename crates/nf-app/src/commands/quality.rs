//! `quality`: plays a test scene through the running filter and measures what comes back (TODO #14: Lab phase 0's live half).
//!
//! It builds the scene (`quality::signals`), starts the engine in-process with `run`'s options (for the echo test, with --far as
//! the echo reference and AEC on unless --no-aec), waits for it to run and settle, then plays the scene into --probe, and the
//! far voice into --far, while recording --capture (`nf_engine::play_record`, on this thread). Then it stops the engine and
//! reports the alignment and the levels (`quality::analysis`), the engine's own drift and echo estimates, and the dropouts: the
//! engine's counters over the take and the recording's own gaps. Exit 0 when the recording lined up with the scene, 3 when it
//! didn't (or the test was cancelled), 2 when something failed.
//!
//! The echo test places its far voice so that the echo reaches the mic `--echo-delay` after the far end's loopback taps it
//! (`quality::echo_delay`): the take starts with a marker on both devices, a thread beside `play_record` times it in the chain
//! recording as it grows ([`calibrate`]) and sets the far voice's start (an `nf_engine::LatePart`), and after the take the
//! chain recording says where the echo landed: the report's "echo delay" and "calibration" lines.
//!
//! The engine's chain recorder runs in every echo take, and with `--save`: armed before the start (`Engine::record_chain`), so
//! the recording holds the run from its first block. `--save` writes what the chain got and gave live beside the scene's WAVs:
//! `chain_mic.wav`, `chain_ref.wav`, `chain_out.wav`, a row a block in `chain_blocks.csv`, and the config in
//! `chain_config.json`. `process chain_mic.wav OUT.wav --ref chain_ref.wav --config chain_config.json --warm-up` reruns that
//! chain into `chain_out.wav` exactly (TODO 10).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, Instant};

use nf_core::{BLOCK, SAMPLE_RATE};
use nf_engine::{
    ChainRecorder, ChainRecording, DeviceInfo, Engine, EngineConfig, EngineState, EngineStatus, Flow, LatePart, LateStart, LatencyBudget,
    MAX_CHAIN_RECORDING_S, OutputEvents, PlayRecordRequest, PlayRecordResult, PlayTrack, REFERENCE_TARGET_HIGH_MS, TelemetrySnapshot,
    TrackRole,
};

use super::run::stream_text;
use super::{budget_text, db, fixed, list_devices};
use crate::cli::{EngineArgs, QualityArgs};
use crate::config::{self, resolve_device};
use crate::exit::Failure;
use crate::quality::QualityTest;
use crate::quality::analysis::{self, AlignmentFailure, Analysis, Levels, MIN_REGION_S, Metrics, SpeechLevels, WINDOW_S};
use crate::quality::echo_delay::{self, Calibration, EchoDelay, FarStart, MAX_RIG_OFFSET_MS, Marker, Plan, RigTrack};
use crate::quality::signals::{self, Scene};
use crate::{console, wav};

/// Once the engine runs, the time its streams and drift loops get to settle before the scene plays.
const SETTLE: Duration = Duration::from_millis(1500);
/// How long a just-started engine may take to report running.
const RUNNING_TIMEOUT: Duration = Duration::from_secs(5);
/// The report shows levels at or below this as -inf (as [`db`] does).
const FLOOR_DBFS: f32 = -120.0;
/// How often the echo test's calibration looks at the chain recording (the recorder drains its ring as often).
const CALIBRATION_POLL: Duration = Duration::from_millis(20);

pub fn quality(args: &QualityArgs) -> Result<(), Failure> {
    let user = signals::read_user_audio(args.speech.as_deref(), args.noise.as_deref(), args.far_speech.as_deref())?;
    let scene = signals::build(args.test, args.seconds, args.snr_db, args.echo_delay_ms, &user).map_err(Failure::error)?;
    say!("{}", scene.description);
    let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    let cfg = config::engine_config(&engine_args(args), || Ok(devices.clone()))?;
    let probe = resolve_device(&devices, &args.probe, Flow::Render, "--probe").map_err(Failure::error)?;
    let capture = resolve_device(&devices, &args.capture, Flow::Capture, "--capture").map_err(Failure::error)?;
    let far = args.far.as_deref().map(|f| resolve_device(&devices, f, Flow::Render, "--far")).transpose().map_err(Failure::error)?;
    check_devices(&cfg, &devices, &probe, &capture, far.as_deref()).map_err(Failure::usage)?;
    let route = route(&devices, &probe, far.as_deref(), &capture);

    let engine = Engine::new();
    let plan = (scene.test == QualityTest::Echo).then(|| Plan::new(f64::from(args.echo_delay_ms)));
    // Armed before the start, the chain recorder holds the run from its first block (its whole history, for a rerun). The echo
    // test times its marker in it, and measures where the echo landed.
    let recorder = if args.save.is_some() || plan.is_some() {
        let limit = recording_limit(&scene, plan.as_ref(), super::run::target_wait(&cfg, args.settle).waits());
        Some(engine.record_chain(limit).map_err(|e| Failure::error(format!("Couldn't arm the chain recorder: {e}")))?)
    } else {
        None
    };
    super::run::start_engine(&engine, &cfg, args.engine.allow_driver)?;
    // The depth a model pack ran at (automatic by default: chosen at the start), so that chain_config.json reruns as it ran.
    let mut ran = ran_config(&cfg, engine.status().model_depth);
    if let Some(fx) = engine.effect_info() {
        ran.effect.worker_blocks = fx.worker_blocks;
        ran.effect.plugin_id = Some(fx.plugin_id);
    }
    let route = Route { probe, capture, far, text: route };
    let take = play(&engine, &cfg, args.settle, &scene, plan.as_ref(), recorder.as_ref(), route);
    // Before the engine stops, so the recording ends with the take rather than with the run.
    let chain = recorder.map(ChainRecorder::stop);
    engine.stop();
    console::finished();
    let mut take = take?;
    say!("Analysing the recording...");
    if let (Some(echo), Some(chain)) = (take.echo.as_mut(), &chain) {
        echo.measure(chain, &scene);
    }
    let slip_from = take.echo.as_ref().and_then(|e| e.rig.as_ref()?.excluded_from());
    let result = analysis::analyse_with(&scene, &take.recorded.recording, take.scene_at, slip_from);
    say!("{}", format_report(scene.test, &result, &take));
    if let Some(dir) = &args.save {
        let far = take.echo.as_ref().and_then(|e| Some(e.plan.far_as_played(scene.far.as_deref()?, e.far_start?.frames)));
        let names = save(dir, &scene, far.as_deref(), &take.recorded.recording, result.as_ref().ok().map(|a| a.aligned.as_slice()))?;
        say!("Wrote {} to {}.", names.join(", "), dir.display());
        if let Some(chain) = &chain {
            let names = save_chain(dir, &ran, chain)?;
            say!("{}", chain_line(&names, chain));
        }
    }
    match result {
        Ok(_) => Ok(()),
        Err(f) => Err(Failure::no_result(format!(
            "{} Check that --probe feeds the filter's mic and that --capture records its output.",
            f.reason
        ))),
    }
}

/// The config as the run ran it: a model pack's automatic depth replaced by the one the start chose (`EngineStatus::model_depth`),
/// which an offline rerun (`process --config`) must use to match; with RNNoise, or a depth that was set, `cfg` as it is.
fn ran_config(cfg: &EngineConfig, depth: Option<nf_engine::ModelDepth>) -> EngineConfig {
    let mut ran = cfg.clone();
    if let Some(d) = depth {
        ran.processing.model_depth = d.depth;
    }
    ran
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
    let mic = cfg.devices.mic.as_deref();
    let output = (cfg.io.output_backend == nf_engine::OutputBackend::Wasapi).then_some(cfg.devices.output.as_deref()).flatten();
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

/// The chain recorder's limit: the take (the settle, the adaptive target's wait when `waits` (`--settle` with the adaptive
/// target), the lead, the echo test's calibration, the scene and the tail) and a minute to spare, within the engine's 15
/// minutes.
fn recording_limit(scene: &Scene, plan: Option<&Plan>, waits: bool) -> f64 {
    let r = PlayRecordRequest::default();
    let calibration = plan.map_or(0.0, |p| p.latest.max(p.scene_at) as f64 / f64::from(SAMPLE_RATE));
    let wait = if waits { crate::cli::ADAPTIVE_WAIT_S as f64 } else { 0.0 };
    let take =
        scene.input.len() as f64 / f64::from(SAMPLE_RATE) + calibration + SETTLE.as_secs_f64() + wait + f64::from(r.lead_s + r.tail_s);
    (take + 60.0).min(MAX_CHAIN_RECORDING_S)
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
    /// Where the scene's input starts in the recording: the lead, and the echo test's calibration before it.
    pub scene_at: usize,
    pub dropouts: Dropouts,
    pub engine: EngineReadings,
    /// The latency readout's budget over the take: each part's mean (engine.md, "The latency budget"); `None` with the
    /// readout off.
    pub budget: Option<LatencyBudget>,
    /// The echo test's echo delay: how it was placed and where it landed.
    pub echo: Option<EchoTake>,
    /// How the filter's streams ran (exclusive mode and its format, the periods) and its output target at the take's end.
    pub conditions: String,
    pub warnings: Vec<String>,
}

/// The echo test's echo delay (`quality::echo_delay`): the take's plan, what the calibration did, where the far voice started,
/// and where the echo landed by the take's chain recording.
#[derive(Clone, Debug, PartialEq)]
pub struct EchoTake {
    pub plan: Plan,
    /// Where the marker is looked for in the chain recording (`echo_delay::search_from`: from the take's start).
    pub search_from: usize,
    /// The marker as the calibration found it and the start it asked for the far voice, or why there was none.
    pub calibration: Result<(Calibration, FarStart), String>,
    /// Where the far voice started (`play_record`'s late start; `None` if it never did).
    pub far_start: Option<LateStart>,
    /// The echo's delay measured in the chain recording (filled in once the recorder has stopped), or why it wasn't.
    pub delay: Result<EchoDelay, String>,
    /// Both devices' content against what they played, from the marker on (`echo_delay::track_rig`): a rig slip, if any.
    pub rig: Option<RigTrack>,
}

impl EchoTake {
    /// Measures where the echo landed, in the take's whole chain recording: from where the calibration found the marker (the
    /// same recording: its copies were of this one's first blocks), or where it is in the whole recording when the calibration
    /// didn't find it in time; and the echo in the far-only turns after it.
    fn measure(&mut self, chain: &ChainRecording, scene: &Scene) {
        let marker = match &self.calibration {
            Ok((c, _)) => Ok(*c),
            Err(_) => match echo_delay::find_marker(chain, &self.plan.marker, self.search_from) {
                Marker::Found(c) => Ok(c),
                Marker::Failed(why) => Err(why),
                Marker::NotYet { .. } => Err("the marker isn't in the chain recording".to_owned()),
            },
        };
        self.rig = match (&marker, self.far_start) {
            (Ok(cal), Some(start)) => Some(echo_delay::track_rig(chain, scene, &self.plan, cal, start.frames)),
            _ => None,
        };
        let slip_from = self.rig.as_ref().and_then(RigTrack::excluded_from);
        self.delay = match (marker, self.far_start) {
            (_, None) => Err("the far voice didn't play".into()),
            (Ok(cal), Some(start)) => echo_delay::measure(chain, scene, &self.plan, &cal, start.frames, slip_from),
            (Err(why), _) => Err(why),
        };
    }

    /// The report's lines: the delay asked and measured, the calibration, and the rig's content against what it played.
    fn lines(&self) -> Vec<String> {
        let ms = |v: f64| fixed(v, 1);
        let mut delay = format!("{:<26} asked {} ms, ", "echo delay", ms(self.plan.delay_ms));
        match &self.delay {
            Ok(d) => {
                let turns = if d.turns == d.turns_held { format!("{}", d.turns) } else { format!("{} of {}", d.turns, d.turns_held) };
                let plural = if d.turns_held == 1 { "turn" } else { "turns" };
                let when = if d.seen_ms < 0.0 { format!("{} ms before", ms(-d.seen_ms)) } else { format!("{} ms after", ms(d.seen_ms)) };
                let _ = write!(
                    delay,
                    "measured {} ms after the reference's tap ({turns} far-only {plural}, {} to {} ms); AEC3 sees it {when} the \
                     reference it's given (the lead {} ms)",
                    ms(d.measured_ms),
                    ms(d.low_ms),
                    ms(d.high_ms),
                    ms(d.lead_ms)
                );
                if !d.slipped.is_empty() {
                    let each: Vec<String> = d.slipped.iter().map(|(at, v)| format!("{} ms at {:.0} s", ms(*v), at)).collect();
                    let _ = write!(delay, "; after the rig slipped {}", each.join(", "));
                }
            }
            Err(why) => {
                let _ = write!(delay, "not measured: {why}");
            }
        }
        let played = match self.far_start {
            Some(s) => {
                let a = self.plan.advance_ms(s.frames);
                if a < 0.0 {
                    format!("the far voice played {} ms behind its echo", ms(-a))
                } else {
                    format!("the far voice played {} ms ahead of its echo", ms(a))
                }
            }
            None => "the far voice didn't play".into(),
        };
        let calibration = match &self.calibration {
            Ok((c, _)) => {
                let side = if c.rig_ms < 0.0 { format!("{} ms before", ms(-c.rig_ms)) } else { format!("{} ms after", ms(c.rig_ms)) };
                format!(
                    "{:<26} on these devices an echo with no delay of its own reaches the mic {side} its reference's tap (the \
                     marker, at a lead of {} ms{}), so {played}",
                    "calibration",
                    ms(c.lead_ms),
                    if c.stamped { "" } else { " by delivery" }
                )
            }
            Err(why) => format!("{:<26} none ({why}): {played}, as for devices that add no delay", "calibration"),
        };
        let mut lines = vec![delay, calibration];
        if let Some(r) = &self.rig {
            lines.push(format!("{:<26} {} (against what each played)", "rig", echo_delay::rig_line(r)));
        }
        lines
    }

    /// What the report warns about: the measured delay's own warnings, and a calibration that didn't place the far voice
    /// where it asked.
    fn warnings(&self) -> Vec<String> {
        let mut w = self.rig.as_ref().map(echo_delay::slip_warnings).unwrap_or_default();
        w.extend(self.delay.as_ref().map(echo_delay::warnings).unwrap_or_default());
        match (&self.calibration, self.far_start) {
            (Err(why), _) => w.push(format!(
                "The echo delay wasn't calibrated ({why}), so the echo came where the devices put it: the measured delay is the \
                 one AEC3 had."
            )),
            (Ok((c, asked)), Some(s)) => {
                if asked.clamped {
                    w.push(format!(
                        "On these devices an echo with no delay of its own reaches the mic {} ms from its reference's tap, past the \
                         {MAX_RIG_OFFSET_MS:.0} ms either way a take makes up for: the far voice started as near as it could, so the \
                         delay isn't the one asked.",
                        fixed(c.rig_ms, 1)
                    ));
                }
                match s.set {
                    None => w.push(
                        "The calibration came too late: the far voice had started at its default (as for devices that add no \
                         delay) before the marker was found."
                            .to_owned(),
                    ),
                    Some(set) if s.frames > set => w.push(format!(
                        "The calibration came {} ms too late for the far voice's start, which started that much later than it \
                         should have: its echo came that much sooner.",
                        fixed((s.frames - set) as f64 * 1000.0 / f64::from(SAMPLE_RATE), 1)
                    )),
                    Some(_) => {}
                }
            }
            (Ok(_), None) => {}
        }
        w
    }
}

/// One take: the settle (and with `settle`, the adaptive output target's wait), then the scene played and recorded.
fn play(
    engine: &Engine, cfg: &EngineConfig, settle: bool, scene: &Scene, plan: Option<&Plan>, recorder: Option<&ChainRecorder>, route: Route,
) -> Result<Take, Failure> {
    wait_running(engine)?;
    if console::sleep_unless_stopped(SETTLE) || super::run::wait_for_adaptive_target(engine, cfg, settle) {
        return Err(Failure::no_result("Cancelled before the test started."));
    }
    let before = engine.telemetry();
    // The echo test: the marker on both tracks, the scene's input after it on the probe, and the far voice where the
    // calibration puts it (its default until then).
    let probe_track = plan.map_or(Cow::Borrowed(&scene.input[..]), |p| Cow::Owned(p.probe_track(&scene.input)));
    let start = AtomicU64::new(LatePart::UNSET);
    let late = plan.zip(scene.far.as_deref()).map(|(p, far)| LatePart {
        samples: far,
        start: &start,
        default: p.default_start(),
        latest: p.latest,
    });
    let mut play = vec![PlayTrack { device: route.probe, role: TrackRole::Probe, samples: &probe_track, late: None }];
    let far_track = plan.zip(route.far).zip(late);
    if let Some(((p, device), late)) = far_track {
        play.push(PlayTrack { device, role: TrackRole::FarEnd, samples: &p.marker, late: Some(late) });
    }
    let request = PlayRecordRequest { play, capture_device: route.capture, cancel: Some(console::stop_flag()), ..Default::default() };
    let seconds = |frames: u64| fixed(frames as f64 / f64::from(SAMPLE_RATE), 1);
    let calibration = plan.map_or(String::new(), |p| format!(" and {} s of calibration", seconds(p.scene_at)));
    say!(
        "Playing the scene: {} s, after {} s of silence{calibration}, recording {} s past its end. Ctrl+C cancels.",
        seconds(scene.input.len() as u64),
        fixed(f64::from(request.lead_s), 0),
        fixed(f64::from(request.tail_s), 0)
    );
    // The take blocks this thread: say now if Ctrl+C can't reach it.
    if let Some(w) = console::CtrlCWatch::default().check() {
        say!("{w}");
    }
    // Echo cancellation's reference is the only timing of the far end there is: without it, the far voice plays at its default.
    let calibrate = match (plan, recorder, late) {
        (Some(p), Some(r), Some(l)) if cfg.processing.aec_enabled => Some((p, r, l)),
        _ => None,
    };
    // The marker plays after the lead: it's looked for only from just before then in the chain recording, which holds the run.
    let lead = (f64::from(request.lead_s) * f64::from(SAMPLE_RATE)).round() as usize;
    let search_from = echo_delay::search_from(recorder.map_or(0, |r| r.blocks() as usize * BLOCK), lead);
    let done = AtomicBool::new(false);
    let (recorded, calibrated) = std::thread::scope(|s| {
        let done = &done;
        let helper = calibrate.map(|(p, r, l)| s.spawn(move || self::calibrate(r, p, &l, search_from, done)));
        let recorded = nf_engine::play_record(&request);
        done.store(true, Release);
        (recorded, helper.map(|h| h.join().unwrap_or_else(|_| Err("its thread failed".into()))))
    });
    let recorded = recorded.map_err(|e| {
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
    if cfg.processing.aec_enabled && after.reference_target_ms > REFERENCE_TARGET_HIGH_MS {
        warnings.push(format!(
            "The echo reference's lead target rose to {} ms: an echo that comes back sooner than that (speakers with a short \
             output latency) arrives before its reference, and AEC3 can't cancel it (its delay estimate then reads about 0).",
            fixed(f64::from(after.reference_target_ms), 1)
        ));
    }
    let readings = EngineReadings::of(cfg, &before, &after);
    let budget = after.budget_totals.means_since(&before.budget_totals);
    let echo = plan.map(|p| EchoTake {
        plan: p.clone(),
        search_from,
        calibration: calibrated.unwrap_or_else(|| {
            Err(if cfg.processing.aec_enabled {
                "no far end".into()
            } else {
                "echo cancellation is off, so nothing times the far end".into()
            })
        }),
        far_start: recorded.late_starts.get(1).copied().flatten(),
        delay: Err("no chain recording".into()),
        rig: None,
    });
    let scene_at = recorded.lead_frames + plan.map_or(0, |p| p.scene_at as usize);
    let conditions = conditions(&engine.status(), &after, engine.buffer_targets().output_ms);
    let dropouts = Dropouts::between(&before, &after, &recorded);
    Ok(Take { dropouts, recorded, scene_at, engine: readings, budget, echo, conditions, warnings })
}

/// The filter's streams as they ran (shared or exclusive, and its format; the periods; RAW) and its output target at the
/// take's end, fixed or adaptive (against `fixed_ms`, the fixed one it starts at: at it, or down from it):
/// "filter mic CABLE-A Output (48000 Hz, 2 ch, period 3.33 ms, exclusive 16-bit); output CABLE Input (...); output target
/// 14.0 ms (fixed)".
pub fn conditions(status: &EngineStatus, t: &TelemetrySnapshot, fixed_ms: f32) -> String {
    let stream = |label: &str, s: Option<&nf_engine::StreamInfo>| match s {
        Some(s) => format!("{label} {}", stream_text(s)),
        None => format!("{label} --"),
    };
    let mut parts = vec![format!("filter {}", stream("mic", status.mic.as_ref())), stream("output", status.output.as_ref())];
    let target = nf_engine::output_target_text(t.output_target_ms, t.output_adaptive, fixed_ms);
    if !target.is_empty() {
        parts.push(target);
    }
    parts.join("; ")
}

/// The echo test's calibration, beside the take: every [`CALIBRATION_POLL`] it copies what the chain recorder has added, and
/// once the marker has arrived in both the filter's mic and its reference (looked for from `from` on, `echo_delay::search_from`)
/// it sets the far voice's start for the rig's offset (`Plan::far_start`). It gives up when the far end's player has taken its
/// default start (the marker came too late), when the take ends, or when the marker can't be found.
fn calibrate(
    recorder: &ChainRecorder, plan: &Plan, late: &LatePart<'_>, from: usize, done: &AtomicBool,
) -> Result<(Calibration, FarStart), String> {
    let mut chain = ChainRecording::default();
    let mut found = Marker::NotYet { mic: false, reference: false };
    loop {
        // Read before the look: a marker found in this look counts even if the player gets there meanwhile (its set fails then,
        // and the report says so).
        let too_late = late.get().is_some() || done.load(Acquire);
        let more = recorder.copy_from(chain.blocks.len());
        if !more.blocks.is_empty() {
            append(&mut chain, more);
            found = echo_delay::find_marker(&chain, &plan.marker, from);
        }
        match &found {
            Marker::Found(c) => {
                let start = plan.far_start(c.rig_ms);
                // False when the player took its default first: the far start says so.
                let _ = late.set(start.frames);
                return Ok((*c, start));
            }
            Marker::Failed(why) => return Err(why.clone()),
            Marker::NotYet { mic, reference } if too_late => {
                return Err(match (mic, reference) {
                    (false, true) => "the marker reached the echo reference but not the filter's mic in time: check that --probe feeds \
                                      the mic"
                        .into(),
                    (true, false) => "the marker reached the filter's mic but not its echo reference in time".into(),
                    _ => "the marker didn't reach the filter's mic and its echo reference in time".into(),
                });
            }
            Marker::NotYet { .. } => {}
        }
        std::thread::sleep(CALIBRATION_POLL);
    }
}

/// `more` (a copy of the blocks after `chain`'s) appended to `chain`.
fn append(chain: &mut ChainRecording, more: ChainRecording) {
    chain.mic.extend_from_slice(&more.mic);
    chain.reference.extend_from_slice(&more.reference);
    chain.output.extend_from_slice(&more.output);
    chain.blocks.extend_from_slice(&more.blocks);
    chain.lost_blocks = more.lost_blocks;
    chain.truncated = more.truncated;
    chain.run_ended = more.run_ended;
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

/// What the engine ran, and its own estimates at the end of the take: the drift loops'; the echo reference's lead and the
/// target its loop holds (how long before the mic block the reference served with it was tapped), whether the streams'
/// stamps timed it, and its slips and jumps over the take (a jump moves the echo path, and AEC3 takes seconds to follow);
/// and AEC3's (the echo delay it found, and its ERLE). They show what a measured ERLE went with.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EngineReadings {
    pub aec: bool,
    pub noise_filter: bool,
    pub output_drift_ppm: f32,
    pub reference_drift_ppm: f32,
    pub reference_lead_ms: f32,
    pub reference_target_ms: f32,
    pub reference_stamped: bool,
    pub reference_slips: u64,
    pub reference_jumps: u64,
    pub aec_delay_ms: f32,
    pub aec_erle_db: f32,
    /// The echo-path guard's mic hold at the take's end (ms; 0 without one), and whether it was still on its trial.
    pub echo_hold_ms: f32,
    pub echo_trial: bool,
    /// What the output side did over the take (TODO 9): a move of the output path's delay that these don't show happened in
    /// the devices, after the filter's output took the samples.
    pub output: OutputEvents,
}

impl EngineReadings {
    fn of(cfg: &EngineConfig, before: &TelemetrySnapshot, t: &TelemetrySnapshot) -> Self {
        Self {
            aec: cfg.processing.aec_enabled,
            noise_filter: cfg.processing.noise_enabled,
            output_drift_ppm: t.output_drift_ppm,
            reference_drift_ppm: t.reference_drift_ppm,
            reference_lead_ms: t.reference_fill_ms,
            reference_target_ms: t.reference_target_ms,
            reference_stamped: t.reference_stamped,
            reference_slips: t.reference_slips.saturating_sub(before.reference_slips),
            reference_jumps: t.reference_coarse_drops.saturating_sub(before.reference_coarse_drops),
            aec_delay_ms: t.aec_delay_ms,
            aec_erle_db: t.aec_erle_db,
            echo_hold_ms: t.echo_hold_ms,
            echo_trial: t.echo_guard == nf_engine::ECHO_GUARD_TRIAL,
            output: OutputEvents::between(before, t),
        }
    }

    /// "output events              no underruns, zeros, drops or re-anchors; 37 slips (net -5 samples) over the take".
    fn output_line(&self) -> String {
        format!("{:<26} {} over the take", "output events", self.output.text())
    }

    /// "output drift +1.2 ppm, reference drift -0.4 ppm, lead 23.4 ms (target 23.4), 2 slips and 0 jumps over the take;
    /// AEC3's estimates: echo delay 40.0 ms, ERLE 31.2 dB". The lead says "by delivery" when the stamps didn't time it.
    fn line(&self) -> String {
        let ppm = |v: f32| if v.is_finite() { format!("{v:+.1}") } else { "--".into() };
        let mut s = format!("{:<26} output drift {} ppm", "engine", ppm(self.output_drift_ppm));
        if self.aec {
            let _ = write!(
                s,
                ", reference drift {} ppm, lead {} ms{} (target {}), {} slips and {} jumps over the take; AEC3's estimates: echo delay \
                 {} ms, ERLE {} dB",
                ppm(self.reference_drift_ppm),
                fixed(f64::from(self.reference_lead_ms), 1),
                if self.reference_stamped { "" } else { " by delivery" },
                fixed(f64::from(self.reference_target_ms), 1),
                self.reference_slips,
                self.reference_jumps,
                fixed(f64::from(self.aec_delay_ms), 1),
                fixed(f64::from(self.aec_erle_db), 1)
            );
            if self.echo_hold_ms > 0.0 {
                let trial = if self.echo_trial { ", on its trial" } else { "" };
                let _ = write!(s, "; the mic held {} ms for the echo (the echo-path guard{trial})", fixed(f64::from(self.echo_hold_ms), 1));
            }
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

/// Writes the scene's signals, the far voice as played (the echo test), the recording and (when it lined up) the aligned
/// recording into `dir` as 48 kHz mono float WAVs, and returns their names.
fn save(dir: &Path, scene: &Scene, far: Option<&[f32]>, recording: &[f32], aligned: Option<&[f32]>) -> Result<Vec<&'static str>, Failure> {
    std::fs::create_dir_all(dir).map_err(|e| Failure::error(format!("Couldn't create {}: {e}", dir.display())))?;
    let mut files: Vec<(&'static str, &[f32])> = vec![("input.wav", &scene.input), ("clean.wav", &scene.clean)];
    // As played against input.wav: the far voice moved by the advance the take used, as `process --ref` would get it from a
    // loopback that the probe's cable delivers in step with (the echo delay AEC3 had live is in chain_ref.wav).
    if let Some(far) = far {
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

/// The chain recording's files, beside the take's WAVs.
const CHAIN_FILES: [&str; 5] = ["chain_mic.wav", "chain_ref.wav", "chain_out.wav", "chain_blocks.csv", "chain_config.json"];

/// Writes the chain recording into `dir`: what the chain got (mic and reference) and gave, as 48 kHz mono float WAVs, a CSV
/// row a block (`nf_engine::chain_blocks_csv`, its columns `CHAIN_BLOCK_COLUMNS`: engine.md, "The chain recorder"), and the
/// engine config the take ran with (`process --config` reads it). Returns the files' names.
fn save_chain(dir: &Path, cfg: &EngineConfig, chain: &ChainRecording) -> Result<Vec<&'static str>, Failure> {
    let [mic, reference, out, blocks, config] = CHAIN_FILES;
    wav::write_mono_f32(&dir.join(mic), &chain.mic)?;
    wav::write_mono_f32(&dir.join(reference), &chain.reference)?;
    wav::write_mono_f32(&dir.join(out), &chain.output)?;
    let write = |name: &str, text: String| {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|e| Failure::error(format!("Couldn't write {}: {e}", path.display())))
    };
    write(blocks, nf_engine::chain_blocks_csv(&chain.blocks))?;
    let json = serde_json::to_string_pretty(cfg).map_err(|e| Failure::error(format!("Couldn't write {config}: {e}")))?;
    write(config, json)?;
    Ok(CHAIN_FILES.to_vec())
}

/// What was saved of the chain, in one line: how much, from where in the run, what was lost or cut, and how to rerun it.
fn chain_line(names: &[&str], chain: &ChainRecording) -> String {
    let files = match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => names.join(""),
    };
    let Some(first) = chain.blocks.first() else {
        return format!("Wrote {files}, but the chain recorder got no blocks: the filter's run never reached it.");
    };
    let mut s = format!(
        "Wrote {files}: what the chain got and gave live, {} blocks ({} s) from {}",
        chain.blocks.len(),
        fixed(chain.seconds(), 1),
        if first.index == 0 { "the run's first block".to_owned() } else { format!("block {} of the run", first.index) }
    );
    match chain.lost_blocks {
        0 => s.push_str(", none lost"),
        n => {
            let _ = write!(s, "; {n} blocks lost to a full ring (missing from the files)");
        }
    }
    if chain.truncated {
        s.push_str("; cut at the recorder's limit");
    }
    if chain.run_ended {
        s.push_str("; the filter's run ended before the take did");
    }
    let changed = chain.settings_changed_at();
    if let Some(at) = changed {
        let _ = write!(s, "; the chain's settings changed at block {at}");
    }
    if chain.reproducible() {
        s.push_str(
            ". In that folder `noise-filter process chain_mic.wav chain_rerun.wav --ref chain_ref.wav --config chain_config.json \
             --warm-up` reruns it into chain_out.wav exactly.",
        );
    } else if chain.from_first_block() && chain.lost_blocks == 0 && changed.is_some() {
        s.push_str(". A rerun with chain_config.json matches chain_out.wav only up to that block.");
    } else {
        s.push_str(". A rerun of it won't match chain_out.wav exactly: it doesn't hold the chain's whole history.");
    }
    s
}

/// The report: the alignment, each region's levels, the engine's readings, the dropouts, the streams, notes and warnings.
pub fn format_report(test: QualityTest, result: &Result<Analysis, AlignmentFailure>, take: &Take) -> String {
    let seconds = |samples: i64| fixed(samples as f64 / f64::from(SAMPLE_RATE), 4);
    // The scene's start in the recording: after the lead, and the echo test's calibration.
    let scene_at = take.scene_at;
    let mut s = String::new();
    let mut notes = Vec::new();
    let mut warnings = take.warnings.clone();
    match result {
        Ok(a) => {
            let al = &a.alignment;
            let ms = |samples: f64| fixed((samples - scene_at as f64) * 1000.0 / f64::from(SAMPLE_RATE), 2);
            let _ = write!(
                s,
                "Quality, {} test: the recording lines up {} s in ({} ms after the scene's start), correlation {} (confidence {} \
                 dB)",
                test.name(),
                seconds(al.delay),
                fixed(al.path_ms(scene_at), 1),
                fixed(f64::from(al.correlation), 2),
                fixed(f64::from(al.confidence_db), 1)
            );
            let drift = if al.drift_ppm.is_finite() { format!(", drift {:+.1} ppm", al.drift_ppm) } else { String::new() };
            let _ = write!(
                s,
                "\n  {:<26} {} of {} windows of {WINDOW_S:.0} s lined up, the lowest at {}; delay {} to {} ms after the scene's \
                 start{drift}",
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
    let _ = write!(s, "\n  {}", take.engine.output_line());
    if let Some(echo) = &take.echo {
        for line in echo.lines() {
            let _ = write!(s, "\n  {line}");
        }
        warnings.extend(echo.warnings());
    }
    if let Some(b) = &take.budget {
        let _ = write!(s, "\n  {:<26} {}, means over the take", "latency budget", budget_text(b));
    }
    let _ = write!(s, "\n  {}", take.dropouts.line());
    let played: Vec<String> = take.recorded.playback.iter().map(stream_text).collect();
    let _ = write!(s, "\n  played on {}; recorded from {}", played.join(" and "), stream_text(&take.recorded.capture));
    if !take.conditions.is_empty() {
        let _ = write!(s, "\n  conditions: {}", take.conditions);
    }
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
        Metrics::Echo { far_only, far_only_turns, slipped_turns, slipped_from, converging, near_only, double_talk, both_talking } => {
            let measure = if engine.noise_filter { "drop" } else { "ERLE" };
            let mut lines = vec![row("echo, far end alone", "input", &far_only.levels, measure, fall(&far_only.levels))];
            if let Some(from) = slipped_from {
                let each: Vec<String> = slipped_turns.iter().map(fall).collect();
                lines.push(format!(
                    "{:<26} {} dB, left out of the figure above (and double talk from then on): the rig slipped (see the warning)",
                    format!("  from {:.0} s", *from as f64 / f64::from(SAMPLE_RATE)),
                    if each.is_empty() { "--".to_owned() } else { each.join(", ") }
                ));
            }
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
    use nf_engine::{CHAIN_BLOCK_COLUMNS, ChainBlock, ChainParams, DeviceSelection, OutputFacts, ReferenceFacts, StreamInfo, ThreadFacts};

    use super::*;
    use crate::cli::Processing;
    use crate::quality::analysis::{Alignment, EchoLevels};
    use crate::quality::echo_delay::DEFAULT_ECHO_DELAY_MS;
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
            echo_delay_ms: DEFAULT_ECHO_DELAY_MS,
            seconds: 30.0,
            snr_db: None,
            speech: None,
            noise: None,
            far_speech: None,
            save: None,
            settle: false,
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
            exclusive: None,
            exclusive_refused: None,
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
                late_starts: vec![None; playback.len()],
            },
            scene_at: 48_000,
            dropouts: Dropouts { capture_glitches: 2, capture_gaps: 1, gap_frames: 480, ..Default::default() },
            engine,
            budget: None,
            echo: None,
            conditions: "filter mic CABLE-A Output (48000 Hz, 2 ch, period 10.00 ms); output CABLE Input (48000 Hz, 2 ch, period \
                         10.00 ms); output target 22.0 ms (fixed)"
                .into(),
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

    /// The conditions line: the filter's streams as they ran, exclusive ones with their format, and its output target,
    /// fixed or adaptive.
    #[test]
    fn the_conditions_say_how_the_filter_s_streams_ran() {
        let excl = StreamInfo { exclusive: Some("16-bit"), period_ms: 2.0, ..stream("CABLE Input") };
        let status = EngineStatus { mic: Some(stream("CABLE-A Output")), output: Some(excl), ..Default::default() };
        let t = TelemetrySnapshot { output_target_ms: 5.0, output_adaptive: true, ..Default::default() };
        assert_eq!(
            conditions(&status, &t, 14.0),
            "filter mic CABLE-A Output (48000 Hz, 2 ch, period 10.00 ms); output CABLE Input (48000 Hz, 2 ch, period 2.00 ms, \
             exclusive 16-bit); output target 5.0 ms at the end (adaptive, down from 14.0 ms)"
        );
        // A take that didn't --settle (or went back after an underrun): at the fixed target, and it says what that means.
        let start = conditions(&status, &TelemetrySnapshot { output_target_ms: 14.0, ..t }, 14.0);
        assert!(start.ends_with(
            "output target 14.0 ms at the end (adaptive, at the fixed target: it comes down only after 240 s of running, and goes \
             back up to it after an underrun)"
        ));
        let none = conditions(&EngineStatus::default(), &TelemetrySnapshot { output_target_ms: 22.0, ..Default::default() }, 22.0);
        assert_eq!(none, "filter mic --; output --; output target 22.0 ms (fixed)");
        assert_eq!(conditions(&EngineStatus::default(), &TelemetrySnapshot::default(), 0.0), "filter mic --; output --", "stopped");
    }

    #[test]
    fn the_report_shows_the_alignment_each_region_the_engine_the_dropouts_and_the_streams() {
        let metrics = Metrics::Noise {
            pauses: levels(9.84, -30.0, -51.25),
            steady_pauses: levels(4.2, -30.0, -52.5),
            speech: speech(8.1, -18.52, -18.9, -18.94, -52.04),
        };
        let ok = Ok(Analysis {
            alignment: ALIGNMENT,
            aligned: Vec::new(),
            metrics,
            delay_steps: Vec::new(),
            warnings: vec!["a short region".into()],
        });
        let readings = EngineReadings { noise_filter: true, output_drift_ppm: 1.24, ..Default::default() };
        assert_eq!(
            format_report(QualityTest::Noise, &ok, &take(&["CABLE-A Input"], readings)),
            "Quality, noise test: the recording lines up 1.1830 s in (183.0 ms after the scene's start), correlation 0.97 (confidence \
             39.2 dB)\n  \
             alignment                  14 of 14 windows of 2 s lined up, the lowest at 0.94; delay 182.99 to 183.02 ms after the \
             scene's start, \
             drift +0.1 ppm\n  \
             noise in pauses            input -30.0 dBFS, output -51.2 dBFS: change -21.2 dB over 9.8 s\n  \
             \x20 200 ms from speech       input -30.0 dBFS, output -52.5 dBFS: change -22.5 dB over 4.2 s\n  \
             speech in its cores        clean -18.5 dBFS, output -18.9 dBFS (speech -18.9, rest -52.0): change -0.4 dB over 8.1 s\n  \
             engine                     output drift +1.2 ppm\n  \
             output events              no underruns, zeros, drops or re-anchors; no slips over the take\n  \
             dropouts                   filter: mic glitches 2, output underruns 0, reference gaps 0, stalls 0; test streams: \
             capture gaps 1 (480 frames), playback underruns 0\n  \
             played on CABLE-A Input (48000 Hz, 2 ch, period 10.00 ms); recorded from CABLE Output (48000 Hz, 2 ch, period 10.00 ms)\n  \
             conditions: filter mic CABLE-A Output (48000 Hz, 2 ch, period 10.00 ms); output CABLE Input (48000 Hz, 2 ch, period \
             10.00 ms); output target 22.0 ms (fixed)\n  \
             warning: The filter restarted during the test, so its dropout counts cover only the time since.\n  \
             warning: a short region"
        );

        let metrics = Metrics::Echo {
            far_only: EchoLevels { levels: levels(4.0, -24.5, -55.0), noise_dbfs: -84.1 },
            far_only_turns: vec![levels(1.3, -24.5, -52.0), levels(1.4, -24.5, -60.0), levels(1.3, -24.6, -54.6)],
            slipped_turns: Vec::new(),
            slipped_from: None,
            converging: levels(1.2, -24.4, -36.7),
            near_only: speech(3.2, -18.0, -18.3, -18.31, -50.0),
            double_talk: speech(1.4, -18.4, f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY),
            both_talking: speech(0.4, -18.2, -21.0, -21.3, -32.6),
        };
        let echo = Ok(Analysis {
            alignment: ALIGNMENT,
            aligned: Vec::new(),
            metrics: metrics.clone(),
            delay_steps: Vec::new(),
            warnings: Vec::new(),
        });
        let aec = EngineReadings {
            aec: true,
            noise_filter: false,
            output_drift_ppm: -2.0,
            reference_drift_ppm: 0.44,
            reference_lead_ms: 23.36,
            reference_target_ms: 23.4,
            reference_stamped: true,
            reference_slips: 2,
            reference_jumps: 0,
            aec_delay_ms: 40.0,
            aec_erle_db: 31.25,
            echo_hold_ms: 0.0,
            echo_trial: false,
            output: OutputEvents { slips: 7, slip_net: -1, ..Default::default() },
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
            "\n  engine                     output drift -2.0 ppm, reference drift +0.4 ppm, lead 23.4 ms (target 23.4), 2 slips and 0 \
             jumps over the take; AEC3's estimates: echo delay 40.0 ms, ERLE 31.2 dB\n",
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
        let Metrics::Echo { far_only_turns, converging, near_only, double_talk, both_talking, .. } = metrics.clone() else { panic!() };
        let silent = Metrics::Echo {
            far_only: EchoLevels { levels: levels(4.0, -24.5, f32::NEG_INFINITY), noise_dbfs: -84.1 },
            far_only_turns,
            slipped_turns: Vec::new(),
            slipped_from: None,
            converging,
            near_only,
            double_talk,
            both_talking,
        };
        let echo =
            Ok(Analysis { alignment: ALIGNMENT, aligned: Vec::new(), metrics: silent, delay_steps: Vec::new(), warnings: Vec::new() });
        let report = format_report(QualityTest::Echo, &echo, &take(&["A", "B"], aec));
        assert!(report.contains("output -inf dBFS: ERLE over 95.5 dB over 4.0 s"), "{report}");
        // After a rig slip: the turns from its turn on, listed apart
        let Metrics::Echo { far_only, far_only_turns, converging, near_only, double_talk, both_talking, .. } = metrics else { panic!() };
        let slipped = Metrics::Echo {
            far_only,
            far_only_turns: far_only_turns[..1].to_vec(),
            slipped_turns: vec![levels(1.3, -24.5, -27.0), levels(1.3, -24.5, -26.5)],
            slipped_from: Some(18 * 48_000),
            converging,
            near_only,
            double_talk,
            both_talking,
        };
        let echo =
            Ok(Analysis { alignment: ALIGNMENT, aligned: Vec::new(), metrics: slipped, delay_steps: Vec::new(), warnings: Vec::new() });
        let report = format_report(QualityTest::Echo, &echo, &take(&["A", "B"], aec));
        assert!(
            report
                .contains("\n    from 18 s                2.5, 2.0 dB, left out of the figure above (and double talk from then on): the rig slipped (see the warning)\n"),
            "{report}"
        );
        assert!(report.contains("\n    each far-only turn       27.5 dB: median 27.5, lowest 27.5;"), "{report}");

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

    /// The latency readout's budget over the take, after the engine's readings, when the readout ran.
    #[test]
    fn the_report_shows_the_latency_budget_over_the_take() {
        let readings = EngineReadings { output_drift_ppm: 1.24, ..Default::default() };
        let failed = Err(AlignmentFailure { alignment: None, reason: "silent".into() });
        let b = LatencyBudget { mic_ms: 16.24, wait_ms: 0.08, chain_ms: 10.0, queue_ms: 22.04, render_ms: 23.44 };
        let report = format_report(QualityTest::Noise, &failed, &Take { budget: Some(b), ..take(&["A"], readings) });
        assert!(
            report.contains(
                "\n  engine                     output drift +1.2 ppm\n  \
                 output events              no underruns, zeros, drops or re-anchors; no slips over the take\n  \
                 latency budget             mic delivery 16.2 + wait 0.1 + output queue 22.0 + render side 23.4 = readout 61.8 ms; \
                 the chain's 10.0 ms on top: 71.8 ms, means over the take\n  dropouts"
            ),
            "{report}"
        );
        assert!(!format_report(QualityTest::Noise, &failed, &take(&["A"], readings)).contains("latency budget"));
    }

    fn calibration(rig_ms: f64) -> Calibration {
        Calibration {
            mic_at: 117_400.0,
            ref_at: 120_000.0,
            mic_confidence_db: 52.0,
            ref_confidence_db: 61.0,
            lead_ms: 14.06,
            stamped: true,
            rig_ms,
        }
    }

    fn delay(measured_ms: f64, seen_ms: f64) -> EchoDelay {
        EchoDelay {
            asked_ms: 25.0,
            measured_ms,
            seen_ms,
            lead_ms: measured_ms - seen_ms,
            low_ms: measured_ms - 0.1,
            high_ms: measured_ms + 0.2,
            turns: 6,
            turns_held: 6,
            slipped: Vec::new(),
        }
    }

    /// The echo delay's lines, after the engine's: asked and measured (and AEC3's lag behind its reference, and the lead),
    /// and the calibration: the devices' own offset and how far ahead of its echo the far voice played; with the warnings a
    /// non-causal echo, a missing calibration or a late one earn.
    #[test]
    fn the_report_gives_the_echo_delay_and_its_calibration() {
        let plan = Plan::new(25.0);
        let asked = plan.far_start(-40.0);
        let echo = EchoTake {
            plan: plan.clone(),
            search_from: 0,
            calibration: Ok((calibration(-40.0), asked)),
            far_start: Some(LateStart { frames: asked.frames, set: Some(asked.frames) }),
            delay: Ok(delay(25.04, 11.0)),
            rig: None,
        };
        let failed = Err(AlignmentFailure { alignment: None, reason: "silent".into() });
        let report = |e: EchoTake| {
            let mut t = take(&["A", "B"], EngineReadings::default());
            t.warnings.clear();
            format_report(QualityTest::Echo, &failed, &Take { echo: Some(e), ..t })
        };
        let r = report(echo.clone());
        assert!(
            r.contains(
                "\n  echo delay                 asked 25.0 ms, measured 25.0 ms after the reference's tap (6 far-only turns, 24.9 to \
                 25.2 ms); AEC3 sees it 11.0 ms after the reference it's given (the lead 14.0 ms)\n  \
                 calibration                on these devices an echo with no delay of its own reaches the mic 40.0 ms before its \
                 reference's tap (the marker, at a lead of 14.1 ms), so the far voice played 65.0 ms ahead of its echo\n  dropouts"
            ),
            "{r}"
        );
        assert!(!r.contains("warning:"), "{r}");
        // The echo before its reference: said in the line and warned about
        let r = report(EchoTake { delay: Ok(delay(5.0, -9.0)), ..echo.clone() });
        assert!(r.contains("AEC3 sees it 9.0 ms before the reference it's given (the lead 14.0 ms)"), "{r}");
        assert!(r.contains("warning: AEC3 got the echo 9.0 ms before the reference it's given"), "{r}");
        assert!(r.contains("warning: The echo came 5.0 ms after its reference's tap, -20.0 ms off the 25.0 ms asked"), "{r}");
        // Devices that put the echo after its tap: the far voice behind its echo
        let late_rig = plan.far_start(40.0);
        let r = report(EchoTake {
            calibration: Ok((calibration(40.0), late_rig)),
            far_start: Some(LateStart { frames: late_rig.frames, set: Some(late_rig.frames) }),
            ..echo.clone()
        });
        assert!(
            r.contains(
                "40.0 ms after its reference's tap (the marker, at a lead of 14.1 ms), so the far voice played 15.0 ms behind its echo"
            ),
            "{r}"
        );
        // No calibration: the default, said and warned about; nothing measured
        let r = report(EchoTake {
            calibration: Err("echo cancellation is off, so nothing times the far end".into()),
            far_start: Some(LateStart { frames: plan.default_start(), set: None }),
            delay: Err("the filter has no echo reference, so nothing times the far end".into()),
            ..echo.clone()
        });
        assert!(
            r.contains(
                "echo delay                 asked 25.0 ms, not measured: the filter has no echo reference, so nothing times the far end\n  \
                 calibration                none (echo cancellation is off, so nothing times the far end): the far voice played 25.0 \
                 ms ahead of its echo, as for devices that add no delay"
            ),
            "{r}"
        );
        assert!(r.contains("warning: The echo delay wasn't calibrated (echo cancellation is off"), "{r}");
        // Found too late for the player, and set after the player had passed it
        let r = report(EchoTake { far_start: Some(LateStart { frames: plan.default_start(), set: None }), ..echo.clone() });
        assert!(r.contains("warning: The calibration came too late: the far voice had started at its default"), "{r}");
        let r = report(EchoTake { far_start: Some(LateStart { frames: asked.frames + 480, set: Some(asked.frames) }), ..echo.clone() });
        assert!(r.contains("warning: The calibration came 10.0 ms too late for the far voice's start"), "{r}");
        assert!(r.contains("so the far voice played 55.0 ms ahead of its echo"), "{r}");
        // Devices past what a take makes up for
        let r = report(EchoTake { calibration: Ok((calibration(-300.0), plan.far_start(-300.0))), ..echo });
        assert!(r.contains("warning: On these devices an echo with no delay of its own reaches the mic -300.0 ms from its"), "{r}");
        assert!(r.contains("past the 250 ms either way a take makes up for"), "{r}");
    }

    /// A take whose far end slipped 70 ms at 24 s: the rig line says which side and when, the delay line lists the turns after
    /// it apart, and the warning says the turns from its turn on are the rig's.
    #[test]
    fn the_report_says_when_the_rig_slipped() {
        use crate::quality::echo_delay::{RigPoint, Side, Slip};
        let plan = Plan::new(25.0);
        let asked = plan.far_start(-40.0);
        let point = |at: i64, offset: f64| RigPoint { at, offset, peak: 0.99 };
        let rig = RigTrack {
            mic: (0..100).map(|k| point(k * 24_000, 0.0)).collect(),
            reference: (0..100).map(|k| point(k * 24_000, if k < 54 { 0.4 } else { 3_360.4 })).collect(),
            slips: vec![Slip { side: Side::Reference, after: 24 * 48_000, at: 27 * 48_000, samples: 3_360.0, confirmed: true }],
            lost: Vec::new(),
        };
        assert_eq!(rig.excluded_from(), Some(24 * 48_000));
        let mut d = delay(25.04, 11.0);
        d.slipped = vec![(27.0, -44.9), (36.0, -45.0)];
        let echo = EchoTake {
            plan: plan.clone(),
            search_from: 0,
            calibration: Ok((calibration(-40.0), asked)),
            far_start: Some(LateStart { frames: asked.frames, set: Some(asked.frames) }),
            delay: Ok(d),
            rig: Some(rig),
        };
        let failed = Err(AlignmentFailure { alignment: None, reason: "silent".into() });
        let mut t = take(&["A", "B"], EngineReadings::default());
        t.warnings.clear();
        let r = format_report(QualityTest::Echo, &failed, &Take { echo: Some(echo), ..t });
        assert!(
            r.contains(
                "24.9 to 25.2 ms); AEC3 sees it 11.0 ms after the reference it's given (the lead 14.0 ms); after the rig slipped -44.9 \
                 ms at 27 s, -45.0 ms at 36 s\n"
            ),
            "{r}"
        );
        assert!(
            r.contains(
                "\n  rig                        the mic's 100 windows, within 0.0 ms, the reference's 100 windows, within 0.0 ms between \
                 slips; the reference slipped +70.0 ms between 24.0 and 27.0 s (against what each played)\n"
            ),
            "{r}"
        );
        assert!(
            r.contains(
                "warning: The rig slipped: the far end's loopback (the far voice in the filter's reference) by +70.0 ms between 24.0 \
                 and 27.0 s into the scene (the mic held), against what it played"
            ),
            "{r}"
        );
        assert!(
            r.contains(
                "so the far-only turns from 27 s on are left out of the echo figures, and double talk from then on: the rig's failure"
            ),
            "{r}"
        );
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
        let before = TelemetrySnapshot { reference_slips: 10, reference_coarse_drops: 1, output_slips: 4, ..Default::default() };
        let t = TelemetrySnapshot {
            output_slips: 9,
            output_slip_net: 3,
            output_coarse_drops: 1,
            output_dropped_frames: 480,
            output_drift_ppm: 3.0,
            reference_drift_ppm: -1.0,
            reference_fill_ms: 12.5,
            reference_target_ms: 12.0,
            reference_stamped: false,
            reference_slips: 13,
            reference_coarse_drops: 2,
            aec_delay_ms: 52.0,
            aec_erle_db: 12.0,
            ..Default::default()
        };
        let r = EngineReadings::of(&cfg, &before, &t);
        assert_eq!(r, EngineReadings {
            aec: true,
            noise_filter: true,
            output_drift_ppm: 3.0,
            reference_drift_ppm: -1.0,
            reference_lead_ms: 12.5,
            reference_target_ms: 12.0,
            reference_stamped: false,
            reference_slips: 3,
            reference_jumps: 1,
            aec_delay_ms: 52.0,
            aec_erle_db: 12.0,
            echo_hold_ms: 0.0,
            echo_trial: false,
            output: OutputEvents { coarse_drops: 1, dropped_frames: 480, slips: 5, slip_net: 3, ..Default::default() },
        });
        assert_eq!(r.output_line(), "output events              1 coarse drop (10.0 ms dropped); 5 slips (net +3 samples) over the take");
        assert!(r.line().contains("lead 12.5 ms by delivery (target 12.0), 3 slips and 1 jumps over the take"), "{}", r.line());
        let off = EngineReadings { output_drift_ppm: f32::NAN, ..Default::default() };
        assert_eq!(off.line(), format!("{:<26} output drift -- ppm", "engine"));
        let held = EngineReadings { echo_hold_ms: 10.0, echo_trial: true, ..r };
        assert!(
            held.line().ends_with("ERLE 12.0 dB; the mic held 10.0 ms for the echo (the echo-path guard, on its trial)"),
            "{}",
            held.line()
        );
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
        use nf_stages::harness::run_chain;
        use nf_stages::synth::{Voice, delayed, speech_like};

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
        let noise = |snr| signals::build(QualityTest::Noise, 30.0, Some(snr), DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let (n10, n0) = (noise(10.0), noise(0.0));
        let echo = signals::build(QualityTest::Echo, 30.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
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
        let long = signals::build(QualityTest::Noise, 300.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
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

    fn chain_block(index: u64, reference: Option<ReferenceFacts>) -> ChainBlock {
        ChainBlock {
            index,
            capture_qpc_hns: 1e10 + index as f64 * 1e5,
            mic_stamped: true,
            mic_index: index * 480,
            mic_wake_hns: 1e10 + index as f64 * 1e5 + 2e5,
            mic_queued: 0,
            mic_discarded: if index == 1 { 960 } else { 0 },
            mic_skipped: if index == 1 { -48 } else { 0 },
            reference,
            echo_hold: 0,
            aec_delay_ms: f32::NAN,
            aec_erle_db: 3.25,
            settings: ChainParams { passthrough: index >= 1, ..ChainParams::default() },
            budget: (index == 0).then_some(LatencyBudget {
                mic_ms: 19.5,
                wait_ms: 0.125,
                chain_ms: 10.0,
                queue_ms: 21.25,
                render_ms: 22.0,
            }),
            output: OutputFacts {
                index: 1056 + index * 480,
                padding: 960,
                pending: 0,
                fill: 1019.75,
                target: 1056,
                slip: if index == 1 { -1 } else { 0 },
                zeros: if index == 1 { 96 } else { 0 },
                dropped: 0,
                skipped: false,
                underrun: index == 1,
                late: false,
                reanchored: false,
                silence: if index == 1 { 46 } else { 0 },
                clock_pos: if index == 0 { 96.0 } else { f64::NAN },
                clock_qpc_hns: if index == 0 { 10_000_180_000.0 } else { f64::NAN },
            },
            thread: ThreadFacts {
                wake_hns: 1e10 + index as f64 * 1e5 + 2e5,
                wake_gap_ms: if index == 0 { 10.0 } else { 0.0 },
                wakes: u32::from(index == 0),
                out_wakes: 0,
                stalls: 0,
                mic_glitches: u32::from(index == 1),
                process_us: 87.5,
                model_misses: 0,
            },
        }
    }

    /// `--save`'s chain files: the three WAVs as recorded, a CSV row a block (the reference's columns empty without one, an
    /// unknown number empty), and the config, which `process --config` reads back.
    #[test]
    fn saving_the_chain_writes_its_files_and_a_row_a_block() {
        let dir = crate::test_dir("quality-chain");
        let facts = ReferenceFacts {
            read_index: -96,
            lead_reading_ms: 14.25,
            lead_ms: 14.0,
            target_ms: 14.1,
            drift_ppm: -1.5,
            stamped: true,
            fresh: true,
            delivered: false,
            buffered: -12,
            consumed: 481,
            owed: 12,
            missing: 0,
            jumped: 96,
            events: ReferenceFacts::ALIGNED | ReferenceFacts::COASTED,
        };
        let chain = ChainRecording {
            mic: (0..960).map(|i| i as f32 * 1e-3).collect(),
            reference: vec![0.125; 960],
            output: vec![-0.5; 960],
            blocks: vec![chain_block(0, Some(facts)), chain_block(1, None)],
            ..Default::default()
        };
        let mut cfg = EngineConfig::default();
        cfg.processing.aec_enabled = true;
        cfg.processing.noise_enabled = false;
        let names = save_chain(&dir, &cfg, &chain).unwrap();
        assert_eq!(names, CHAIN_FILES);
        let read = |name: &str| wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        assert_eq!(
            (read("chain_mic.wav"), read("chain_ref.wav"), read("chain_out.wav")),
            (chain.mic.clone(), chain.reference, chain.output)
        );
        let csv = std::fs::read_to_string(dir.join("chain_blocks.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        let columns = lines[0].split(',').count();
        assert_eq!(lines[0], CHAIN_BLOCK_COLUMNS);
        assert!(lines.iter().all(|l| l.split(',').count() == columns), "{csv}");
        assert_eq!(
            lines[1],
            "0,10000000000.0,1,0,10000200000.0,0,0,0,-96,96,14.250,14.000,14.100,-1.50,1,1,0,-12,481,12,0,96,aligned|coasted,0,,3.25,0,\
             19.500,0.125,10.000,21.250,22.000,62.875,1056,960,0,1019.8,1056,0,0,0,0,0,0,0,0,96.0,10000180000.0,\
             10000200000.0,10.000,1,0,0,0,87.5,0"
        );
        assert_eq!(
            lines[2],
            "1,10000100000.0,1,480,10000300000.0,0,960,-48,,,,,,,,,,,,,,,,0,,3.25,1,,,,,,,1536,960,0,1019.8,1056,-1,96,0,0,1,0,0,46,,,\
             10000300000.0,0.000,0,0,0,1,87.5,0"
        );
        let back = crate::config::load(&dir.join("chain_config.json")).unwrap();
        assert_eq!(back, cfg, "process --config reads it");
        let _ = std::fs::remove_dir_all(dir);
        // A model pack's automatic depth is saved as the depth the start chose, so the rerun matches; RNNoise's is left as is.
        let pack = EngineConfig {
            processing: nf_engine::ProcessingConfig { noise_model: "ulunas".into(), ..Default::default() },
            ..Default::default()
        };
        let chose = nf_engine::ModelDepth { depth: 1, auto: Some(nf_engine::MicDelivery::OneAtATime) };
        assert_eq!(ran_config(&pack, Some(chose)).processing.model_depth, 1);
        assert_eq!(ran_config(&cfg, None), cfg);
    }

    #[test]
    fn the_chain_line_says_what_was_saved_and_how_to_rerun_it() {
        let blocks = |from: u64, n: u64| (from..from + n).map(|i| chain_block(i, None)).collect::<Vec<_>>();
        let rec = |from, n, lost| ChainRecording {
            mic: vec![0.0; n as usize * 480],
            blocks: blocks(from, n),
            lost_blocks: lost,
            ..Default::default()
        };
        let steady = |mut r: ChainRecording| {
            for b in &mut r.blocks {
                b.settings = ChainParams::default();
            }
            r
        };
        let line = chain_line(&CHAIN_FILES, &steady(rec(0, 250, 0)));
        assert!(
            line.starts_with(
                "Wrote chain_mic.wav, chain_ref.wav, chain_out.wav, chain_blocks.csv and chain_config.json: what the chain got and \
                 gave live, 250 blocks (2.5 s) from the run's first block, none lost. In that folder `noise-filter process"
            ),
            "{line}"
        );
        assert!(line.ends_with("--warm-up` reruns it into chain_out.wav exactly."), "{line}");
        let line = chain_line(&CHAIN_FILES, &steady(rec(0, 250, 3)));
        assert!(line.contains("from the run's first block; 3 blocks lost to a full ring") && line.contains("won't match"), "{line}");
        // `chain_block` turns passthrough on from block 1.
        let line = chain_line(&CHAIN_FILES, &rec(0, 250, 0));
        assert!(line.contains("none lost; the chain's settings changed at block 1. A rerun with chain_config.json matches"), "{line}");
        let late = ChainRecording { truncated: true, run_ended: true, ..steady(rec(12, 10, 0)) };
        let line = chain_line(&CHAIN_FILES, &late);
        assert!(line.contains("from block 12 of the run, none lost; cut at the recorder's limit; the filter's run ended"), "{line}");
        assert!(line.ends_with("it doesn't hold the chain's whole history."), "{line}");
        assert!(chain_line(&CHAIN_FILES, &ChainRecording::default()).contains("got no blocks"));
    }

    /// The chain recorder covers the take: the settle, the adaptive target's wait (with `--settle`), the lead, the
    /// echo test's calibration, the scene and the tail, and a minute more.
    #[test]
    fn the_recorder_covers_the_take() {
        let scene = signals::build(QualityTest::Noise, 30.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let plain = recording_limit(&scene, None, false);
        assert!((plain - (30.0 + 1.5 + 3.0 + 60.0)).abs() < 0.1, "{plain}");
        assert!(plain <= MAX_CHAIN_RECORDING_S);
        let plan = Plan::new(25.0);
        let with = recording_limit(&scene, Some(&plan), false);
        assert!((with - plain - plan.latest as f64 / 48_000.0).abs() < 1e-9, "{with}");
        let adaptive = recording_limit(&scene, None, true);
        assert!((adaptive - plain - crate::cli::ADAPTIVE_WAIT_S as f64).abs() < 1e-9, "{adaptive}");
    }

    #[test]
    fn saving_writes_the_scene_and_the_recordings() {
        let dir = crate::test_dir("quality-save");
        let out = dir.join("take");
        let scene = signals::build(QualityTest::Echo, 5.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let recording = vec![0.25f32; 1000];
        let plan = Plan::new(25.0);
        let far = plan.far_as_played(scene.far.as_deref().unwrap(), plan.far_start(-40.0).frames);
        let names = save(&out, &scene, Some(&far), &recording, Some(&scene.clean)).unwrap();
        assert_eq!(names, ["input.wav", "clean.wav", "far.wav", "recording.wav", "aligned.wav"]);
        let read = |name: &str| wav::read_mono_48k(&out.join(name)).unwrap().samples;
        assert_eq!(read("recording.wav"), recording);
        assert_eq!(read("far.wav"), far);
        // 65 ms ahead of its echo in input.wav
        assert_eq!(far[..far.len() - 3_120], scene.far.as_deref().unwrap()[3_120..]);
        assert_eq!(read("aligned.wav"), scene.clean);
        let noise = signals::build(QualityTest::Noise, 5.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let names = save(&out, &noise, None, &recording, None).unwrap();
        assert_eq!(names, ["input.wav", "clean.wav", "recording.wav"], "no far end, and no alignment to save");
        let _ = std::fs::remove_dir_all(dir);
    }
}
