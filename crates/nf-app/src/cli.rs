//! Hand-rolled argument parsing (no clap, to keep the binary small: app.md). [`parse`] is a pure
//! function of the arguments, so each command's grammar is unit-tested. Options take their value as
//! the next argument or after `=` (`--floor -20`, `--floor=-20`); a value may start with `-`.

use std::path::PathBuf;

use nf_engine::{GateMode, HighBand, LatencyMode};

use crate::quality::QualityTest;
use crate::quality::echo_delay::{DEFAULT_ECHO_DELAY_MS, EXCLUSIVE_MIC_ECHO_FLOOR_MS, MAX_ECHO_DELAY_MS};

/// What the command line asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// No arguments, or `gui`: the settings window.
    Gui,
    Help,
    Version,
    Devices,
    /// `devices --check`: a short passive run on the chosen devices, then a health report per stream (TODO 21).
    DeviceCheck(CheckArgs),
    Run(RunArgs),
    Latency(LatencyArgs),
    Quality(QualityArgs),
    Process(ProcessArgs),
    Bench(BenchArgs),
    /// `--sid` names the account to grant (the non-elevated side passes its own when it relaunches
    /// itself elevated, as the window's "Enable large pages" button does). `helper`: the hidden
    /// `--elevated-helper` flag that relaunch adds, so a helper that starts without elevation fails
    /// instead of relaunching itself again.
    SetupLargePages {
        sid: Option<String>,
        helper: bool,
    },
}

impl Command {
    /// Whether the command runs the engine or real-time code, and so wants the locked pool, the
    /// lock-pages privilege and the working-set minimum before any engine thread exists.
    pub fn needs_memory(&self) -> bool {
        match self {
            Command::Gui | Command::Run(_) | Command::DeviceCheck(_) | Command::Quality(_) | Command::Bench(_) => true,
            // The cable-only test runs no engine
            Command::Latency(l) => !l.cable_only,
            _ => false,
        }
    }
}

/// Processing options shared by `run`, `latency`, `quality` and `process`. `None` keeps the config's value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Processing {
    pub aec: Option<bool>,
    pub noise: Option<bool>,
    pub floor_db: Option<f32>,
    pub gate: Option<GateMode>,
    pub threshold_db: Option<f32>,
    /// `--model NAME`: the noise model, `rnnoise` or a model pack's id (checked for its syntax here, resolved at the start).
    pub model: Option<String>,
    /// `--high-band model|zero|pass`: what a band-limited model puts above its band.
    pub high_band: Option<HighBand>,
    /// `--model-depth N`: how many blocks behind the audio thread a model pack's inference worker runs (1-4).
    pub model_depth: Option<u32>,
}

/// The streams' options (`IoConfig`'s latency options), shared by `run`, `latency` and `quality`. `None` keeps the config's
/// value (on by default: the owner, 2026-11-02). The `--no-...` flags turn one off, as `--no-aec` does; the plain ones turn it
/// back on over a `--config` file that has it off. The last of a pair wins.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IoOptions {
    /// `--exclusive-output`, `--no-exclusive-output`: the output in WASAPI exclusive mode.
    pub output_exclusive: Option<bool>,
    /// `--exclusive-mic`, `--no-exclusive-mic`: the mic in WASAPI exclusive mode.
    pub mic_exclusive: Option<bool>,
    /// `--adaptive-output`, `--no-adaptive-output`: the output's target follows the delivery.
    pub adaptive_output: Option<bool>,
}

/// `run`'s options, which `latency` and `quality` take too.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineArgs {
    pub config: Option<PathBuf>,
    /// Device ids or name substrings, resolved against the device list at start.
    pub mic: Option<String>,
    pub output: Option<String>,
    pub reference: Option<String>,
    pub processing: Processing,
    pub io: IoOptions,
}

/// `run`: the engine's options, and how long to run (a soak: stop after that many seconds and print the summary).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunArgs {
    pub engine: EngineArgs,
    /// `--seconds N`: stop after N seconds; `None` runs until Ctrl+C.
    pub seconds: Option<u32>,
}

/// `devices --check`: `run`'s options (the devices to check; echo cancelling on unless `--no-aec`, so the speakers' loopback is
/// checked too) and how long to listen.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckArgs {
    pub engine: EngineArgs,
    pub seconds: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LatencyArgs {
    pub engine: EngineArgs,
    pub probe: String,
    pub capture: String,
    pub mode: LatencyMode,
    pub repetitions: u32,
    pub through_model: bool,
    /// `--cable-only`: the sweeps from the probe to the capture with no engine, so `engine`, `mode` and `through_model` are
    /// unused (the parser refuses them).
    pub cable_only: bool,
    /// `--settle`: wait for the adaptive output target to come down before the sweeps (up to `run::ADAPTIVE_WAIT_S`). Without
    /// it the test runs at once, at the target's fixed start.
    pub settle: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QualityArgs {
    pub engine: EngineArgs,
    /// The playback device that feeds the filter's mic, and the recording device that carries its output.
    pub probe: String,
    pub capture: String,
    pub test: QualityTest,
    /// The echo test's far-end device: it plays the far voice and is the echo reference.
    pub far: Option<String>,
    /// The echo test's echo delay in ms: its echo reaches the mic this long after the far end's loopback taps the far voice.
    pub echo_delay_ms: f32,
    pub seconds: f32,
    /// The noise below the speech's active level; `None` takes the test's default.
    pub snr_db: Option<f32>,
    /// 48 kHz recordings to use instead of the synthetic speech, noise and far voice.
    pub speech: Option<PathBuf>,
    pub noise: Option<PathBuf>,
    pub far_speech: Option<PathBuf>,
    /// Where to write the scene, the recording and the aligned recording as WAVs, and what the chain got and gave live.
    pub save: Option<PathBuf>,
    /// `--settle`: wait for the adaptive output target to come down before the scene, as `latency --settle` does.
    pub settle: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessArgs {
    pub input: PathBuf,
    pub output: PathBuf,
    pub reference: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub processing: Processing,
    /// Drop the chain's latency from the front and flush the tail, so `out[n]` lines up with `in[n]`.
    pub align: bool,
    /// Start the chain as the engine does (`nf_engine::warm_up_chain`: three dithered silent blocks first), so a chain
    /// recording that `quality --save` wrote reruns sample for sample.
    pub warm_up: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BenchArgs {
    pub input: Option<PathBuf>,
    pub seconds: f32,
    /// Time the whole chain with AEC3 in it.
    pub aec: bool,
    /// `--model NAME`: time a model pack (a build with `--features onnx`) beside RNNoise.
    pub model: Option<String>,
}

/// `run --seconds` at most: a day.
pub const MAX_RUN_SECONDS: u32 = 86_400;
/// `devices --check`'s default listening time and its range. The check keeps the chain recorder's per-block facts for the whole
/// time (about 0.6 MB a second).
pub const DEFAULT_CHECK_SECONDS: u32 = 15;
pub const MIN_CHECK_SECONDS: u32 = 5;
pub const MAX_CHECK_SECONDS: u32 = 120;
/// The latency test's default repetitions (the engine's `LatencyTestRequest` default).
pub const DEFAULT_REPETITIONS: u32 = 5;
/// The longest `--settle` waits for the adaptive output target: its window, its longest walk down, and 15 s.
pub const ADAPTIVE_WAIT_S: u64 = nf_engine::ADAPTIVE_OUTPUT_WINDOW_S + nf_engine::ADAPTIVE_OUTPUT_WALK_S + 15;
/// `quality`'s default scene length, and its range. The echo scene is one FFT convolution over its whole length, as `bench`'s
/// is, hence the same cap.
pub const DEFAULT_QUALITY_SECONDS: f32 = 30.0;
pub const MIN_QUALITY_SECONDS: f32 = 5.0;
pub const MAX_QUALITY_SECONDS: f32 = 300.0;
/// The echo test's shortest scene: its first far-only turn (0-3 s) is the echo canceller's to converge in, so the ERLE is
/// measured from the next one (9-12 s).
pub const MIN_ECHO_SECONDS: f32 = 12.0;
/// `quality --snr` starts at 0 dB: lower, a scene played with the filter off no longer correlates clearly enough with its clean
/// speech to be aligned (`quality::analysis::MIN_CORRELATION`).
pub const MIN_QUALITY_SNR_DB: f32 = 0.0;
pub const MAX_QUALITY_SNR_DB: f32 = 60.0;
/// `bench`'s default length, as `tests/timing.rs`.
pub const DEFAULT_BENCH_SECONDS: f32 = 30.0;
/// `bench --seconds` at most. The synthetic signals' echo is one FFT convolution over the whole
/// length, which peaks at about 1.2 GB at 300 s (and 8-10 GB at an hour).
pub const MAX_BENCH_SECONDS: f32 = 300.0;
/// The hidden flag the non-elevated `setup-large-pages` adds when it relaunches itself elevated
/// (defined in nf-mem, which the window's button shares). It's left out of `--help`.
pub use nf_mem::privilege::HELPER_FLAG;

/// A command line that doesn't parse, worded for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn usage(msg: impl Into<String>) -> UsageError {
    UsageError(msg.into())
}

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let Some((first, rest)) = args.split_first() else { return Ok(Command::Gui) };
    // `noise-filter <command> --help` asks for help, wherever it is.
    if args.iter().any(|a| a == "--help" || a == "-h" || a == "/?") {
        return Ok(Command::Help);
    }
    let mut p = Parser { args: rest, pos: 0, command: first.as_str() };
    let command = match first.as_str() {
        "gui" => Command::Gui,
        // `help run` too: there's one help text
        "help" => return Ok(Command::Help),
        "--version" | "-V" | "version" => Command::Version,
        "devices" => return parse_devices(&mut p),
        "run" => return parse_run(&mut p).map(Command::Run),
        "latency" => return parse_latency(&mut p).map(Command::Latency),
        "quality" => return parse_quality(&mut p).map(Command::Quality),
        "process" => return parse_process(&mut p).map(Command::Process),
        "bench" => return parse_bench(&mut p).map(Command::Bench),
        "setup-large-pages" => return parse_setup(&mut p),
        other if other.starts_with('-') => return Err(usage(format!("unknown option '{other}'"))),
        other => return Err(usage(format!("unknown command '{other}'"))),
    };
    p.finish()?;
    Ok(command)
}

/// One argument: an option (with its `=value`, if written that way) or a positional.
enum Arg<'a> {
    Flag(&'a str, Option<&'a str>),
    Positional(&'a str),
}

struct Parser<'a> {
    args: &'a [String],
    pos: usize,
    command: &'a str,
}

impl<'a> Parser<'a> {
    fn next(&mut self) -> Option<Arg<'a>> {
        let arg = self.args.get(self.pos)?.as_str();
        self.pos += 1;
        Some(if let Some(long) = arg.strip_prefix("--").filter(|s| !s.is_empty()) {
            match long.split_once('=') {
                Some((name, value)) => Arg::Flag(&arg[..name.len() + 2], Some(value)),
                None => Arg::Flag(arg, None),
            }
        } else if arg.len() > 1 && arg.starts_with('-') && !is_number(arg) {
            Arg::Flag(arg, None)
        } else {
            Arg::Positional(arg)
        })
    }

    /// The option's value: after `=`, or the next argument whatever it looks like.
    fn value(&mut self, flag: &str, inline: Option<&'a str>) -> Result<&'a str, UsageError> {
        if let Some(v) = inline {
            return Ok(v);
        }
        let v = self.args.get(self.pos).ok_or_else(|| usage(format!("{flag} needs a value")))?;
        self.pos += 1;
        Ok(v.as_str())
    }

    /// Nothing may follow.
    fn finish(&mut self) -> Result<(), UsageError> {
        match self.next() {
            None => Ok(()),
            Some(Arg::Flag(f, _)) => Err(self.unknown(f)),
            Some(Arg::Positional(a)) => Err(usage(format!("'{}' takes no argument '{a}'", self.command))),
        }
    }

    fn unknown(&self, flag: &str) -> UsageError {
        usage(format!("'{}' has no option '{flag}'", self.command))
    }
}

fn is_number(s: &str) -> bool {
    s.parse::<f64>().is_ok()
}

/// A switch takes no value.
fn switch(flag: &str, inline: Option<&str>) -> Result<bool, UsageError> {
    match inline {
        None => Ok(true),
        Some(_) => Err(usage(format!("{flag} takes no value"))),
    }
}

fn number(flag: &str, v: &str, lo: f32, hi: f32, what: &str) -> Result<f32, UsageError> {
    match v.trim().parse::<f32>() {
        Ok(x) if x.is_finite() && (lo..=hi).contains(&x) => Ok(x),
        _ => Err(usage(format!("{flag} takes {what}, not '{v}'"))),
    }
}

fn text(flag: &str, v: &str) -> Result<String, UsageError> {
    let v = v.trim();
    if v.is_empty() { Err(usage(format!("{flag} needs a value"))) } else { Ok(v.to_owned()) }
}

/// A noise model's name: `rnnoise` or a model pack's id, by the pack id rule (its syntax only; whether it's installed, and
/// whether this build runs packs, is the start's to say).
fn model_name(flag: &str, v: &str) -> Result<String, UsageError> {
    let v = text(flag, v)?;
    if nf_engine::is_model_id(&v) {
        Ok(v)
    } else {
        Err(usage(format!("{flag} takes a model's name: rnnoise, or a model pack's id (lower-case letters, digits and '-'), not '{v}'")))
    }
}

impl Processing {
    /// Takes one processing option. `Ok(false)`: not one of them.
    fn take<'a>(&mut self, p: &mut Parser<'a>, flag: &str, inline: Option<&'a str>) -> Result<bool, UsageError> {
        match flag {
            "--aec" => self.aec = Some(switch(flag, inline)?),
            "--no-aec" => self.aec = Some(!switch(flag, inline)?),
            "--ns" => self.noise = Some(switch(flag, inline)?),
            "--no-ns" => self.noise = Some(!switch(flag, inline)?),
            "--floor" => {
                let v = p.value(flag, inline)?;
                self.floor_db = Some(number(flag, v, -120.0, 0.0, "an attenuation limit from 0 to -120 dB (e.g. --floor -20)")?);
            }
            "--gate" => {
                self.gate = Some(match p.value(flag, inline)?.trim().to_ascii_lowercase().as_str() {
                    "off" => GateMode::Off,
                    "static" => GateMode::Static,
                    "auto" => GateMode::Auto,
                    other => return Err(usage(format!("--gate takes off, static or auto, not '{other}'"))),
                });
            }
            "--threshold" => {
                let v = p.value(flag, inline)?;
                self.threshold_db = Some(number(flag, v, -120.0, 0.0, "a level from 0 to -120 dBFS (e.g. --threshold -45)")?);
            }
            "--model" => self.model = Some(model_name(flag, p.value(flag, inline)?)?),
            "--model-depth" => {
                let v = p.value(flag, inline)?;
                let max = nf_engine::MAX_MODEL_DEPTH;
                let n = v.trim().parse::<u32>().ok().filter(|n| (1..=max).contains(n));
                self.model_depth =
                    Some(n.ok_or_else(|| usage(format!("--model-depth takes a whole number of blocks from 1 to {max}, not '{v}'")))?);
            }
            "--high-band" => {
                let v = p.value(flag, inline)?.trim().to_ascii_lowercase();
                self.high_band = Some(
                    HighBand::ALL
                        .into_iter()
                        .find(|h| h.name() == v)
                        .ok_or_else(|| usage(format!("--high-band takes model, zero or pass, not '{v}'")))?,
                );
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl IoOptions {
    /// Takes one of the streams' options. `Ok(false)`: not one of them.
    fn take(&mut self, flag: &str, inline: Option<&str>) -> Result<bool, UsageError> {
        match flag {
            "--exclusive-output" => self.output_exclusive = Some(switch(flag, inline)?),
            "--no-exclusive-output" => self.output_exclusive = Some(!switch(flag, inline)?),
            "--exclusive-mic" => self.mic_exclusive = Some(switch(flag, inline)?),
            "--no-exclusive-mic" => self.mic_exclusive = Some(!switch(flag, inline)?),
            "--adaptive-output" => self.adaptive_output = Some(switch(flag, inline)?),
            "--no-adaptive-output" => self.adaptive_output = Some(!switch(flag, inline)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl EngineArgs {
    /// Takes one of `run`'s options. `Ok(false)`: not one of them.
    fn take<'a>(&mut self, p: &mut Parser<'a>, flag: &str, inline: Option<&'a str>) -> Result<bool, UsageError> {
        match flag {
            "--config" => self.config = Some(PathBuf::from(text(flag, p.value(flag, inline)?)?)),
            "--mic" => self.mic = Some(text(flag, p.value(flag, inline)?)?),
            "--output" => self.output = Some(text(flag, p.value(flag, inline)?)?),
            "--reference" => self.reference = Some(text(flag, p.value(flag, inline)?)?),
            _ => return Ok(self.io.take(flag, inline)? || self.processing.take(p, flag, inline)?),
        }
        Ok(true)
    }
}

/// `--settle` waits for the adaptive output target: refused with `--no-adaptive-output`, which fixes it (a config file that
/// does is told at the start instead).
fn check_settle(settle: bool, engine: &EngineArgs) -> Result<(), UsageError> {
    if settle && engine.io.adaptive_output == Some(false) {
        return Err(usage("--settle waits for the adaptive output target, which --no-adaptive-output keeps fixed: drop one"));
    }
    Ok(())
}

/// A whole number of seconds from `lo` to `hi`.
fn seconds(flag: &str, v: &str, lo: u32, hi: u32) -> Result<u32, UsageError> {
    match v.trim().parse::<u32>() {
        Ok(n) if (lo..=hi).contains(&n) => Ok(n),
        _ => Err(usage(format!("{flag} takes a whole number of seconds from {lo} to {hi}, not '{v}'"))),
    }
}

fn parse_run(p: &mut Parser<'_>) -> Result<RunArgs, UsageError> {
    let mut r = RunArgs::default();
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag("--seconds", inline) => r.seconds = Some(seconds("--seconds", p.value("--seconds", inline)?, 1, MAX_RUN_SECONDS)?),
            Arg::Flag(flag, inline) => {
                if !r.engine.take(p, flag, inline)? {
                    return Err(p.unknown(flag));
                }
            }
            Arg::Positional(a) => return Err(usage(format!("'run' takes no argument '{a}'"))),
        }
    }
    Ok(r)
}

/// `devices` alone lists them; `devices --check` checks them, and takes `--seconds` and `run`'s options.
fn parse_devices(p: &mut Parser<'_>) -> Result<Command, UsageError> {
    let (mut check, mut secs, mut engine) = (false, None, EngineArgs::default());
    // The first option given that only the check uses
    let mut check_option: Option<&str> = None;
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag("--check", inline) => check = switch("--check", inline)?,
            Arg::Flag("--seconds", inline) => {
                secs = Some(seconds("--seconds", p.value("--seconds", inline)?, MIN_CHECK_SECONDS, MAX_CHECK_SECONDS)?);
                check_option.get_or_insert("--seconds");
            }
            Arg::Flag(flag, inline) => {
                if !engine.take(p, flag, inline)? {
                    return Err(p.unknown(flag));
                }
                check_option.get_or_insert(flag);
            }
            Arg::Positional(a) => return Err(usage(format!("'devices' takes no argument '{a}'"))),
        }
    }
    match (check, check_option) {
        (true, _) => Ok(Command::DeviceCheck(CheckArgs { engine, seconds: secs.unwrap_or(DEFAULT_CHECK_SECONDS) })),
        (false, None) => Ok(Command::Devices),
        (false, Some(flag)) => Err(usage(format!("{flag} is for 'devices --check': add --check, or drop it"))),
    }
}

fn parse_latency(p: &mut Parser<'_>) -> Result<LatencyArgs, UsageError> {
    let mut engine = EngineArgs::default();
    let (mut probe, mut capture, mut mode, mut reps, mut through_model, mut cable_only, mut settle) =
        (None, None, LatencyMode::VirtualLoop, DEFAULT_REPETITIONS, false, false, false);
    // The first option given that only the test through the filter uses
    let mut filter_option: Option<&str> = None;
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag(flag, inline) => match flag {
                "--probe" => probe = Some(text(flag, p.value(flag, inline)?)?),
                "--capture" => capture = Some(text(flag, p.value(flag, inline)?)?),
                "--mode" => {
                    mode = match p.value(flag, inline)?.trim().to_ascii_lowercase().as_str() {
                        "virtual" => LatencyMode::VirtualLoop,
                        "acoustic" => LatencyMode::Acoustic,
                        other => return Err(usage(format!("--mode takes virtual or acoustic, not '{other}'"))),
                    };
                    filter_option.get_or_insert(flag);
                }
                "--reps" => {
                    let v = p.value(flag, inline)?;
                    reps = match v.trim().parse::<u32>() {
                        Ok(n @ 1..=100) => n,
                        _ => return Err(usage(format!("--reps takes a count from 1 to 100, not '{v}'"))),
                    };
                }
                "--through-model" => {
                    through_model = switch(flag, inline)?;
                    filter_option.get_or_insert(flag);
                }
                "--cable-only" => cable_only = switch(flag, inline)?,
                "--settle" => {
                    settle = switch(flag, inline)?;
                    filter_option.get_or_insert(flag);
                }
                _ => {
                    if !engine.take(p, flag, inline)? {
                        return Err(p.unknown(flag));
                    }
                    filter_option.get_or_insert(flag);
                }
            },
            Arg::Positional(a) => return Err(usage(format!("'latency' takes no argument '{a}'"))),
        }
    }
    // Refused rather than ignored: a --mic or --aec given here would otherwise look as if it had been measured.
    if cable_only && let Some(flag) = filter_option {
        return Err(usage(format!("'latency --cable-only' times the probe-to-capture path alone, without the filter: drop {flag}")));
    }
    check_settle(settle, &engine)?;
    let probe = probe.ok_or_else(|| usage("'latency' needs --probe DEV: the playback device the sweeps are played on"))?;
    let capture = capture.ok_or_else(|| {
        usage(if cable_only {
            "'latency --cable-only' needs --capture DEV: the recording side of the probe's cable"
        } else {
            "'latency' needs --capture DEV: the recording device that carries the filter's output"
        })
    })?;
    Ok(LatencyArgs { engine, probe, capture, mode, repetitions: reps, through_model, cable_only, settle })
}

fn parse_quality(p: &mut Parser<'_>) -> Result<QualityArgs, UsageError> {
    let mut q = QualityArgs {
        engine: EngineArgs::default(),
        probe: String::new(),
        capture: String::new(),
        test: QualityTest::Noise,
        far: None,
        echo_delay_ms: DEFAULT_ECHO_DELAY_MS,
        seconds: DEFAULT_QUALITY_SECONDS,
        snr_db: None,
        speech: None,
        noise: None,
        far_speech: None,
        save: None,
        settle: false,
    };
    let (mut probe, mut capture, mut echo_delay) = (None, None, false);
    let path = |flag: &str, v: &str| text(flag, v).map(PathBuf::from);
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag(flag, inline) => match flag {
                "--probe" => probe = Some(text(flag, p.value(flag, inline)?)?),
                "--capture" => capture = Some(text(flag, p.value(flag, inline)?)?),
                "--test" => {
                    q.test = match p.value(flag, inline)?.trim().to_ascii_lowercase().as_str() {
                        "noise" => QualityTest::Noise,
                        "echo" => QualityTest::Echo,
                        other => return Err(usage(format!("--test takes noise or echo, not '{other}'"))),
                    }
                }
                "--far" => q.far = Some(text(flag, p.value(flag, inline)?)?),
                "--echo-delay" => {
                    let v = p.value(flag, inline)?;
                    q.echo_delay_ms = number(flag, v, 0.0, MAX_ECHO_DELAY_MS, "an echo delay from 0 to 300 ms (e.g. --echo-delay 25)")?;
                    echo_delay = true;
                }
                "--seconds" => {
                    let v = p.value(flag, inline)?;
                    q.seconds = number(flag, v, MIN_QUALITY_SECONDS, MAX_QUALITY_SECONDS, "a length from 5 to 300 s")?;
                }
                "--snr" => {
                    let v = p.value(flag, inline)?;
                    let what = "the noise's level under the speech, from 0 to 60 dB";
                    q.snr_db = Some(number(flag, v, MIN_QUALITY_SNR_DB, MAX_QUALITY_SNR_DB, what)?);
                }
                "--speech" => q.speech = Some(path(flag, p.value(flag, inline)?)?),
                "--noise" => q.noise = Some(path(flag, p.value(flag, inline)?)?),
                "--far-speech" => q.far_speech = Some(path(flag, p.value(flag, inline)?)?),
                "--save" => q.save = Some(path(flag, p.value(flag, inline)?)?),
                "--settle" => q.settle = switch(flag, inline)?,
                _ => {
                    if !q.engine.take(p, flag, inline)? {
                        return Err(p.unknown(flag));
                    }
                }
            },
            Arg::Positional(a) => return Err(usage(format!("'quality' takes no argument '{a}'"))),
        }
    }
    check_settle(q.settle, &q.engine)?;
    q.probe = probe
        .ok_or_else(|| usage("'quality' needs --probe DEV: the playback device that feeds the filter's mic (e.g. \"CABLE-A Input\")"))?;
    q.capture = capture.ok_or_else(|| {
        usage("'quality' needs --capture DEV: the recording device that carries the filter's output (e.g. \"CABLE Output\")")
    })?;
    match q.test {
        QualityTest::Echo if q.far.is_none() => Err(usage(
            "'quality --test echo' needs --far DEV: the far end's speakers, which play the far voice and give the filter its echo \
             reference (e.g. \"CABLE-B Input\")",
        )),
        QualityTest::Echo if q.engine.reference.is_some() => {
            Err(usage("'quality --test echo' takes its echo reference from --far: drop --reference"))
        }
        QualityTest::Echo if q.seconds < MIN_ECHO_SECONDS => Err(usage(format!(
            "'quality --test echo' needs --seconds {MIN_ECHO_SECONDS} or more: its first far-only turn is the echo canceller's to \
             converge in, and the ERLE is measured from the next one (9-12 s)"
        ))),
        QualityTest::Noise if q.far.is_some() => Err(usage("--far is for the echo test: add --test echo, or drop it")),
        QualityTest::Noise if q.far_speech.is_some() => Err(usage("--far-speech is for the echo test: add --test echo, or drop it")),
        QualityTest::Noise if echo_delay => Err(usage("--echo-delay is for the echo test: add --test echo, or drop it")),
        _ => Ok(q),
    }
}

fn parse_process(p: &mut Parser<'_>) -> Result<ProcessArgs, UsageError> {
    let (mut files, mut reference, mut config, mut processing) = (Vec::new(), None, None, Processing::default());
    let (mut align, mut warm_up) = (false, false);
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag(flag, inline) => match flag {
                "--ref" | "--reference" => reference = Some(PathBuf::from(text(flag, p.value(flag, inline)?)?)),
                "--config" => config = Some(PathBuf::from(text(flag, p.value(flag, inline)?)?)),
                "--align" => align = switch(flag, inline)?,
                "--warm-up" => warm_up = switch(flag, inline)?,
                _ => {
                    if !processing.take(p, flag, inline)? {
                        return Err(p.unknown(flag));
                    }
                }
            },
            Arg::Positional(a) => files.push(PathBuf::from(a)),
        }
    }
    let mut files = files.into_iter();
    match (files.next(), files.next(), files.next()) {
        (Some(input), Some(output), None) => Ok(ProcessArgs { input, output, reference, config, processing, align, warm_up }),
        (_, _, Some(extra)) => Err(usage(format!("'process' takes two files, IN.wav and OUT.wav; '{}' is a third", extra.display()))),
        _ => Err(usage("'process' needs IN.wav and OUT.wav")),
    }
}

fn parse_bench(p: &mut Parser<'_>) -> Result<BenchArgs, UsageError> {
    let mut b = BenchArgs { input: None, seconds: DEFAULT_BENCH_SECONDS, aec: false, model: None };
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag(flag, inline) => match flag {
                "--seconds" => {
                    let v = p.value(flag, inline)?;
                    b.seconds = number(flag, v, 2.0, MAX_BENCH_SECONDS, "a length from 2 to 300 s (1 s of it is warm-up)")?;
                }
                "--aec" => b.aec = switch(flag, inline)?,
                "--model" => b.model = Some(model_name(flag, p.value(flag, inline)?)?),
                _ => return Err(p.unknown(flag)),
            },
            Arg::Positional(a) if b.input.is_none() => b.input = Some(PathBuf::from(a)),
            Arg::Positional(a) => return Err(usage(format!("'bench' takes one file at most; '{a}' is a second"))),
        }
    }
    Ok(b)
}

fn parse_setup(p: &mut Parser<'_>) -> Result<Command, UsageError> {
    let (mut sid, mut helper) = (None, false);
    while let Some(arg) = p.next() {
        match arg {
            Arg::Flag(HELPER_FLAG, inline) => helper = switch(HELPER_FLAG, inline)?,
            Arg::Flag("--sid", inline) => {
                let v = text("--sid", p.value("--sid", inline)?)?;
                if !v.to_ascii_uppercase().starts_with("S-1-") {
                    return Err(usage(format!("--sid takes a SID such as S-1-5-21-..., not '{v}'")));
                }
                sid = Some(v);
            }
            Arg::Flag(flag, _) => return Err(p.unknown(flag)),
            Arg::Positional(a) => return Err(usage(format!("'setup-large-pages' takes no argument '{a}'"))),
        }
    }
    Ok(Command::SetupLargePages { sid, helper })
}

/// `--help`'s text.
pub fn help_text() -> String {
    format!(
        "noise-filter {version}: a low-latency mic noise filter and echo canceller

Usage:
  noise-filter [gui]             Open the settings window (the default)
  noise-filter devices           List the recording and playback devices
  noise-filter devices --check [--seconds N] [OPTIONS]
                                 Run the filter quietly for N s (default {check_s}, {check_lo}-{check_hi}) and say
                                 how each stream delivers: steady, in bursts or late, its drift and
                                 glitches, and what to try. Echo cancelling is on (so the speakers'
                                 loopback is checked too) unless --no-aec. The thresholds are
                                 provisional: they come from virtual cables, not real hardware yet
  noise-filter run [--seconds N] [OPTIONS]
                                 Run the filter without the window; Ctrl+C stops it. --seconds stops
                                 it after N s (a soak, up to a day; its exit code says whether it was
                                 clean). Either way it ends with a summary (dropouts, latency, drift)
  noise-filter latency --probe DEV --capture DEV [--mode virtual|acoustic] [--reps N]
                       [--through-model] [--settle] [OPTIONS]
                                 Measure the latency with sweeps and print the report
  noise-filter latency --cable-only --probe DEV --capture DEV [--reps N]
                                 The same sweeps through one cable alone, without the filter
  noise-filter quality --probe DEV --capture DEV [--test noise|echo] [--far DEV] [QUALITY]
                       [--settle] [OPTIONS]
                                 Play a test scene through the filter and measure what it did
  noise-filter process IN.wav OUT.wav [--ref REF.wav] [--align] [--warm-up] [--config FILE]
                       [PROCESSING]
                                 Filter a 48 kHz WAV offline; writes 48 kHz mono float.
                                 --warm-up starts the chain as the engine does, so quality
                                 --save's chain_mic.wav and chain_ref.wav rerun exactly
  noise-filter bench [IN.wav] [--seconds N] [--aec] [--model NAME]
                                 Time RNNoise, AEC3 and the chain per block (no devices);
                                 --model times a model pack beside RNNoise (an onnx build)
  noise-filter setup-large-pages [--sid SID]
                                 Grant the lock-pages privilege (asks for elevation)
  noise-filter --help | --version

OPTIONS (run, latency, quality):
  --config FILE      Settings JSON: the window's settings.json, or an engine config
  --mic DEV          Microphone (default: the system default)
  --output DEV       Playback device that gets the filtered mic, e.g. \"CABLE Input\" (required)
  --reference DEV    Speakers whose sound AEC removes (default: the default playback device)
  DEV is a device id or part of its name (exact name first, then the first match, any case).

LATENCY OPTIONS (run, latency, quality): the lowest latency is the default; each --no-...
turns one off, and the plain flag (--exclusive-output, --exclusive-mic, --adaptive-output)
turns it back on over a --config file that has it off. Your apps record the output cable's
other side, which stays shared whatever these say.
  --no-exclusive-output
                     Play into the output in shared mode, through Windows' audio engine.
                     Exclusive mode plays into the device's driver directly at its smallest
                     period (2 ms on VB-CABLE: 20-30 ms less latency there), and nothing else
                     can play into that device while the filter runs: fine for the output
                     cable, which only the filter plays into. Never while it is Windows'
                     default playback device (no other app could play sound): that runs
                     shared, with a warning. Turn it off if the output device misbehaves in
                     exclusive mode
  --no-exclusive-mic Record the microphone in shared mode. Exclusive mode (10-20 ms less latency
                     on VB-CABLE) keeps every other app from recording that microphone while
                     the filter runs: they use the filtered mic (the cable). Turn it off to
                     record the raw microphone in another app too (OBS, a DAW), or if it
                     misbehaves in exclusive mode. Exclusive, its buffer is one period (3.3 ms
                     on VB-CABLE, against shared mode's 40 ms): a filter thread held up longer
                     than that loses mic audio (a glitch). With echo cancellation on it
                     lengthens the reference's lead, so the canceller needs the echo to reach
                     the mic {echo_floor} ms or more after the speakers' sound is mixed (14 shared)
  --no-adaptive-output
                     Keep the output's buffer target fixed at a period + a block + the margin.
                     Adaptive, it walks down after two minutes to a period + the margin + the
                     lateness measured (about 7 ms less on VB-CABLE). An underrun puts it back
                     to the fixed one and holds it there for 10 minutes, 20 after a second;
                     after a third it stays fixed. So a rare hiccup the fixed target would
                     ride out can cost a glitch, up to three in a run: turn it off for a
                     recording that must have none
  A device that refuses exclusive mode runs in shared mode, with a note saying why, what it
  costs and how to fix it.
  --settle           (latency, quality) Wait for the adaptive output target to come down
                     before measuring: it can after two minutes of running, then walks for up
                     to a minute ({settle_s} s at most). Without it they measure at once, at its
                     fixed start, and the report's conditions say so

PROCESSING (run, latency, quality, process):
  --aec, --no-aec    Echo cancellation on or off (default off; on in quality's echo test)
  --ns, --no-ns      Noise filter on or off (default on)
  --floor DB         The most the noise filter may attenuate, 0 to -120 dB (default -60)
  --gate MODE        off, static or auto (default off)
  --threshold DB     The static gate's threshold in dBFS (implies --gate static)
  --model NAME       The noise model: rnnoise (the default), or an installed model pack's id,
                     such as ulunas (for evaluation: a build with --features onnx, and the pack
                     fetched with python -I tools/fetch_models.py ulunas). UL-UNAS is 16 kHz
                     and runs on a worker thread: 50 ms of stream latency against RNNoise's 10
  --high-band MODE   Above a 16 kHz model's band: model (its own top-band gain, the default),
                     zero (band-limited to 8 kHz) or pass (the input, untouched)
  --model-depth N    How many 10 ms blocks behind a model pack's worker runs, 1 to 4 (default
                     2). Each adds 10 ms; a wake that brings more blocks at once than this (a
                     20 ms device period, a cable's burst) plays the unfiltered mic for each
                     one over

QUALITY (the probe feeds the mic, e.g. \"CABLE-A Input\"; the capture records the output):
  --test noise|echo  Speech in noise (the default), or speech with a far voice's echo
  --far DEV          The echo test's far-end speakers: they play the far voice and are the
                     echo reference, e.g. \"CABLE-B Input\"
  --echo-delay MS    The echo test's echo path: its echo reaches the mic this long after
                     the far end's loopback taps the far voice, as a speaker's output
                     latency and the air would: 0 to 300 ms (default 25, over the {echo_floor} ms
                     the exclusive mic leaves the canceller). Each take first times its
                     devices' own offset with a short sweep on both, and reports the delay
                     it got
  --seconds N        The scene's length, 5 to 300 s (default 30; the echo test needs 12)
  --snr DB           The noise under the speech's level, 0 to 60 dB (default 10; echo test 60)
  --speech FILE, --noise FILE, --far-speech FILE
                     48 kHz WAVs to use instead of the synthetic speech, noise or far voice
  --save DIR         Write the scene, the recording and the aligned recording there as WAVs,
                     and what the chain got and gave live (chain_*.wav, chain_blocks.csv)

Exit codes: 0 OK, 1 usage error, 2 engine, device or file error, 3 no result (latency: no
valid repetition; quality: the recording didn't line up with the scene; latency, quality,
run --seconds or devices --check: cancelled), 4 the large-page helper started without
elevation, 5 this CPU lacks x86-64-v3 (AVX2, BMI1/2, FMA), which noise-filter is built for,
6 run --seconds: the soak wasn't clean (mic glitches or stalls, output underruns or coarse
drops, a fault that stopped it, or real-time heap allocations; the summary says which). A
run without --seconds exits 0.

In a terminal, run commands through noise-filter-cli, which sits next to noise-filter.exe:
  noise-filter-cli run --output \"CABLE Input\"
noise-filter.exe is a windowed program, and cmd.exe and PowerShell don't wait for it: the
prompt comes back at once, Ctrl+C may not reach `run`, and the exit code is lost.
noise-filter-cli is a console program that runs noise-filter.exe and waits for it. If you copy
noise-filter-cli.exe to noise-filter.com in the same folder, a plain `noise-filter ...` in a
terminal runs it (PATHEXT puts .COM before .EXE), and a double-click still opens the window.
Without it, use `start /wait noise-filter ...` in cmd, or pipe it in PowerShell (`| Out-Host`).
Log: %LOCALAPPDATA%\\noise-filter\\noise-filter.log",
        version = env!("CARGO_PKG_VERSION"),
        echo_floor = EXCLUSIVE_MIC_ECHO_FLOOR_MS,
        settle_s = ADAPTIVE_WAIT_S,
        check_s = DEFAULT_CHECK_SECONDS,
        check_lo = MIN_CHECK_SECONDS,
        check_hi = MAX_CHECK_SECONDS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, UsageError> {
        parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    fn err(args: &[&str]) -> String {
        match p(args) {
            Err(UsageError(m)) => m,
            Ok(c) => panic!("{args:?} parsed as {c:?}"),
        }
    }

    #[test]
    fn no_arguments_or_gui_open_the_window() {
        assert_eq!(p(&[]), Ok(Command::Gui));
        assert_eq!(p(&["gui"]), Ok(Command::Gui));
        assert_eq!(err(&["gui", "x"]), "'gui' takes no argument 'x'");
        assert_eq!(err(&["gui", "--aec"]), "'gui' has no option '--aec'");
    }

    #[test]
    fn help_and_version() {
        for args in
            [&["--help"][..], &["-h"], &["help"], &["help", "run"], &["/?"], &["run", "--output", "x", "--help"], &["process", "-h"]]
        {
            assert_eq!(p(args), Ok(Command::Help), "{args:?}");
        }
        assert_eq!(p(&["--version"]), Ok(Command::Version));
        assert_eq!(p(&["-V"]), Ok(Command::Version));
        assert_eq!(err(&["--version", "x"]), "'--version' takes no argument 'x'");
        let help = help_text();
        for command in [
            "devices", "run", "latency", "--cable-only", "quality", "--far-speech", "--echo-delay", "process", "bench",
            "setup-large-pages", "--version",
        ] {
            assert!(help.contains(command), "{command}");
        }
        assert!(help.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn unknown_commands_and_options_are_usage_errors() {
        assert_eq!(err(&["frobnicate"]), "unknown command 'frobnicate'");
        assert_eq!(err(&["--frobnicate"]), "unknown option '--frobnicate'");
        assert_eq!(err(&["devices", "--all"]), "'devices' has no option '--all'");
        assert_eq!(err(&["run", "--bogus"]), "'run' has no option '--bogus'");
        assert_eq!(err(&["run", "extra"]), "'run' takes no argument 'extra'");
    }

    #[test]
    fn run_takes_devices_config_and_processing() {
        let c = p(&[
            "run", "--config", "s.json", "--mic", "Headset Mic", "--output=CABLE Input", "--reference", "Speakers", "--aec", "--no-ns",
            "--floor", "-20", "--gate", "Auto", "--threshold=-45.5",
        ]);
        assert_eq!(
            c,
            Ok(Command::Run(RunArgs {
                engine: EngineArgs {
                    config: Some("s.json".into()),
                    mic: Some("Headset Mic".into()),
                    output: Some("CABLE Input".into()),
                    reference: Some("Speakers".into()),
                    processing: Processing {
                        aec: Some(true),
                        noise: Some(false),
                        floor_db: Some(-20.0),
                        gate: Some(GateMode::Auto),
                        threshold_db: Some(-45.5),
                        model: None,
                        high_band: None,
                        model_depth: None,
                    },
                    io: IoOptions::default(),
                },
                seconds: None,
            }))
        );
        assert_eq!(p(&["run"]), Ok(Command::Run(RunArgs::default())));
        // The last of a pair wins
        let Ok(Command::Run(RunArgs { engine: e, .. })) = p(&["run", "--aec", "--no-aec", "--no-ns", "--ns"]) else { panic!() };
        assert_eq!((e.processing.aec, e.processing.noise), (Some(false), Some(true)));
    }

    /// The latency options, on by default (the owner, 2026-11-02): each turned off by its `--no-...` flag, back on by the
    /// plain one (over a config file that has it off), or (unless given) the config's; the last of a pair wins. `latency`
    /// and `quality` take them too, `latency --cable-only` refuses them like the filter's other options, and `process`
    /// (no devices) doesn't take them.
    #[test]
    fn the_latency_options_turn_exclusive_mode_and_the_adaptive_target_off_and_on() {
        let Ok(Command::Run(RunArgs { engine: e, .. })) = p(&["run"]) else { panic!() };
        assert_eq!(e.io, IoOptions::default(), "nothing given: the config's (or the defaults)");
        let Ok(Command::Run(RunArgs { engine: e, .. })) =
            p(&["run", "--no-exclusive-output", "--no-exclusive-mic", "--no-adaptive-output"])
        else {
            panic!()
        };
        assert_eq!(e.io, IoOptions { output_exclusive: Some(false), mic_exclusive: Some(false), adaptive_output: Some(false) });
        let Ok(Command::Run(RunArgs { engine: e, .. })) = p(&["run", "--exclusive-output", "--exclusive-mic", "--adaptive-output"]) else {
            panic!()
        };
        assert_eq!(e.io, IoOptions { output_exclusive: Some(true), mic_exclusive: Some(true), adaptive_output: Some(true) });
        let Ok(Command::Run(RunArgs { engine: e, .. })) =
            p(&["run", "--no-exclusive-output", "--exclusive-output", "--adaptive-output", "--no-adaptive-output"])
        else {
            panic!()
        };
        assert_eq!(e.io, IoOptions { output_exclusive: Some(true), mic_exclusive: None, adaptive_output: Some(false) }, "the last wins");
        assert_eq!(err(&["run", "--exclusive-mic=yes"]), "--exclusive-mic takes no value");
        assert_eq!(err(&["run", "--no-exclusive-mic=1"]), "--no-exclusive-mic takes no value");
        let Ok(Command::Latency(l)) = p(&["latency", "--probe", "a", "--capture", "b", "--no-exclusive-output"]) else { panic!() };
        assert_eq!(l.engine.io.output_exclusive, Some(false));
        let Ok(Command::Quality(q)) = p(&["quality", "--probe", "a", "--capture", "b", "--no-adaptive-output", "--no-exclusive-mic"])
        else {
            panic!()
        };
        assert_eq!((q.engine.io.adaptive_output, q.engine.io.mic_exclusive), (Some(false), Some(false)));
        for flag in ["--exclusive-output", "--no-exclusive-mic", "--no-adaptive-output"] {
            let e = err(&["latency", "--cable-only", "--probe", "a", "--capture", "b", flag]);
            assert!(e.ends_with(&format!("drop {flag}")), "{e}");
        }
        assert_eq!(err(&["process", "a.wav", "b.wav", "--no-exclusive-output"]), "'process' has no option '--no-exclusive-output'");
        let help = help_text();
        for flag in [
            "--exclusive-output", "--exclusive-mic", "--adaptive-output", "--no-exclusive-output", "--no-exclusive-mic",
            "--no-adaptive-output",
        ] {
            assert!(help.contains(flag), "{flag}");
        }
        // Each says what it costs and when to turn it off.
        assert!(help.contains("the lowest latency is the default") && help.contains("other side, which stays shared"));
        assert!(help.contains("nothing else") && help.contains("misbehaves in exclusive mode"));
        assert!(help.contains("filtered mic") && help.contains("record the raw microphone in another app"));
        assert!(help.contains("loses mic audio") && help.contains("up to three in a run") && help.contains("must have none"));
        assert!(help.contains("a note saying why, what it") && help.contains("20 ms or more after"));
    }

    /// `--settle` (latency, quality) waits for the adaptive target before measuring; off unless given. It needs the adaptive
    /// target, so `--no-adaptive-output` refuses it, and `latency --cable-only` refuses it with the filter's options.
    #[test]
    fn settle_waits_for_the_adaptive_target_when_asked() {
        let Ok(Command::Latency(l)) = p(&["latency", "--probe", "a", "--capture", "b"]) else { panic!() };
        assert!(!l.settle, "off unless asked for: the test measures at once");
        let Ok(Command::Latency(l)) = p(&["latency", "--probe", "a", "--capture", "b", "--settle"]) else { panic!() };
        assert!(l.settle);
        let Ok(Command::Quality(q)) = p(&["quality", "--probe", "a", "--capture", "b"]) else { panic!() };
        assert!(!q.settle);
        let Ok(Command::Quality(q)) = p(&["quality", "--settle", "--probe", "a", "--capture", "b", "--adaptive-output"]) else { panic!() };
        assert!(q.settle);
        let refused = "--settle waits for the adaptive output target, which --no-adaptive-output keeps fixed: drop one";
        assert_eq!(err(&["latency", "--probe", "a", "--capture", "b", "--settle", "--no-adaptive-output"]), refused);
        assert_eq!(err(&["quality", "--probe", "a", "--capture", "b", "--no-adaptive-output", "--settle"]), refused);
        assert_eq!(
            err(&["latency", "--cable-only", "--probe", "a", "--capture", "b", "--settle"]).rsplit(": ").next(),
            Some("drop --settle")
        );
        assert_eq!(err(&["run", "--settle"]), "'run' has no option '--settle'");
        assert_eq!(err(&["quality", "--probe", "a", "--capture", "b", "--settle=1"]), "--settle takes no value");
        let help = help_text();
        assert!(help.contains("[--through-model] [--settle] [OPTIONS]") && help.contains("[--settle] [OPTIONS]"));
        assert!(help.contains(&format!("({ADAPTIVE_WAIT_S} s at most)")) && help.contains("measure at once"));
    }

    #[test]
    fn values_are_checked() {
        assert_eq!(err(&["run", "--floor"]), "--floor needs a value");
        assert!(err(&["run", "--floor", "20"]).starts_with("--floor takes an attenuation limit"));
        assert!(err(&["run", "--floor", "-200"]).starts_with("--floor takes"));
        assert!(err(&["run", "--floor", "NaN"]).starts_with("--floor takes"));
        assert!(err(&["run", "--floor", "loud"]).starts_with("--floor takes"));
        assert!(err(&["run", "--threshold", "3"]).starts_with("--threshold takes"));
        assert_eq!(err(&["run", "--gate", "maybe"]), "--gate takes off, static or auto, not 'maybe'");
        assert_eq!(err(&["run", "--aec=yes"]), "--aec takes no value");
        assert_eq!(err(&["run", "--mic", ""]), "--mic needs a value");
        assert_eq!(err(&["run", "--output="]), "--output needs a value");
        // A value may look like an option
        let Ok(Command::Run(RunArgs { engine: e, .. })) = p(&["run", "--output", "--weird name"]) else { panic!() };
        assert_eq!(e.output.as_deref(), Some("--weird name"));
        let Ok(Command::Run(RunArgs { engine: e, .. })) = p(&["run", "--floor", "0"]) else { panic!() };
        assert_eq!(e.processing.floor_db, Some(0.0));
    }

    #[test]
    fn latency_needs_a_probe_and_a_capture() {
        let Ok(Command::Latency(l)) = p(&[
            "latency", "--probe", "CABLE-A Input", "--capture", "CABLE-B Output", "--mic", "CABLE-A Output", "--output", "CABLE-B Input",
        ]) else {
            panic!()
        };
        assert_eq!((l.probe.as_str(), l.capture.as_str()), ("CABLE-A Input", "CABLE-B Output"));
        assert_eq!((l.mode, l.repetitions, l.through_model), (LatencyMode::VirtualLoop, DEFAULT_REPETITIONS, false));
        assert_eq!(l.engine.mic.as_deref(), Some("CABLE-A Output"));
        let Ok(Command::Latency(l)) =
            p(&["latency", "--probe", "Speakers", "--capture", "B", "--mode", "acoustic", "--reps=12", "--through-model", "--aec"])
        else {
            panic!()
        };
        assert_eq!((l.mode, l.repetitions, l.through_model, l.engine.processing.aec), (LatencyMode::Acoustic, 12, true, Some(true)));
        assert!(err(&["latency", "--capture", "B"]).starts_with("'latency' needs --probe"));
        assert!(err(&["latency", "--probe", "A"]).starts_with("'latency' needs --capture"));
        assert!(err(&["latency", "--probe", "A", "--capture", "B", "--reps", "0"]).starts_with("--reps takes a count"));
        assert!(err(&["latency", "--probe", "A", "--capture", "B", "--reps", "101"]).starts_with("--reps takes a count"));
        assert!(err(&["latency", "--probe", "A", "--capture", "B", "--mode", "x"]).starts_with("--mode takes"));
        assert_eq!(err(&["latency", "--probe", "A", "--capture", "B", "--align"]), "'latency' has no option '--align'");
    }

    #[test]
    fn latency_cable_only_takes_the_probe_the_capture_and_reps_alone() {
        let Ok(Command::Latency(l)) =
            p(&["latency", "--cable-only", "--probe", "CABLE-A Input", "--capture", "CABLE-A Output", "--reps=3"])
        else {
            panic!()
        };
        assert!(l.cable_only);
        assert_eq!((l.probe.as_str(), l.capture.as_str(), l.repetitions), ("CABLE-A Input", "CABLE-A Output", 3));
        assert_eq!((l.engine, l.mode, l.through_model, l.settle), (EngineArgs::default(), LatencyMode::VirtualLoop, false, false));
        let Ok(Command::Latency(l)) = p(&["latency", "--cable-only", "--probe", "a", "--capture", "b"]) else { panic!() };
        assert_eq!(l.repetitions, DEFAULT_REPETITIONS);
        let Ok(Command::Latency(l)) = p(&["latency", "--probe", "a", "--capture", "b"]) else { panic!() };
        assert!(!l.cable_only, "off unless asked for");
        // The filter's options are refused, before or after the flag: they would look as if they had been measured
        let filter_options: [&[&str]; 7] =
            [&["--mic", "x"], &["--output", "x"], &["--aec"], &["--floor", "-20"], &["--config", "s.json"], &["--mode", "acoustic"], &[
                "--through-model",
            ]];
        for opt in filter_options {
            for cable_first in [true, false] {
                let mut args = vec!["latency", "--probe", "a", "--capture", "b"];
                if cable_first {
                    args.push("--cable-only");
                }
                args.extend_from_slice(opt);
                if !cable_first {
                    args.push("--cable-only");
                }
                let expected = format!("'latency --cable-only' times the probe-to-capture path alone, without the filter: drop {}", opt[0]);
                assert_eq!(err(&args), expected);
            }
        }
        assert_eq!(
            err(&["latency", "--cable-only", "--probe", "a"]),
            "'latency --cable-only' needs --capture DEV: the recording side of the probe's cable"
        );
        assert!(err(&["latency", "--cable-only", "--capture", "b"]).starts_with("'latency' needs --probe"));
        assert_eq!(err(&["latency", "--cable-only=yes", "--probe", "a", "--capture", "b"]), "--cable-only takes no value");
    }

    #[test]
    fn quality_takes_a_scene_devices_and_runs_options() {
        let Ok(Command::Quality(q)) =
            p(&["quality", "--probe", "CABLE-A Input", "--capture", "CABLE Output", "--mic", "CABLE-A Output", "--output", "CABLE Input"])
        else {
            panic!()
        };
        let expected = QualityArgs {
            engine: EngineArgs { mic: Some("CABLE-A Output".into()), output: Some("CABLE Input".into()), ..Default::default() },
            probe: "CABLE-A Input".into(),
            capture: "CABLE Output".into(),
            test: QualityTest::Noise,
            far: None,
            echo_delay_ms: DEFAULT_ECHO_DELAY_MS,
            seconds: DEFAULT_QUALITY_SECONDS,
            snr_db: None,
            speech: None,
            noise: None,
            far_speech: None,
            save: None,
            settle: false,
        };
        assert_eq!(q, expected);
        let Ok(Command::Quality(q)) = p(&[
            "quality", "--probe", "A", "--capture", "B", "--test", "Echo", "--far=CABLE-B Input", "--seconds", "60", "--snr", "20",
            "--speech", "me.wav", "--noise", "fan.wav", "--far-speech", "you.wav", "--save", "out", "--no-aec", "--floor", "-30",
            "--echo-delay", "12.5",
        ]) else {
            panic!()
        };
        assert_eq!((q.test, q.far.as_deref(), q.seconds, q.snr_db), (QualityTest::Echo, Some("CABLE-B Input"), 60.0, Some(20.0)));
        assert_eq!(q.echo_delay_ms, 12.5);
        assert_eq!(q.speech, Some(PathBuf::from("me.wav")));
        assert_eq!(q.noise, Some(PathBuf::from("fan.wav")));
        assert_eq!(q.far_speech, Some(PathBuf::from("you.wav")));
        assert_eq!(q.save, Some(PathBuf::from("out")));
        assert_eq!((q.engine.processing.aec, q.engine.processing.floor_db), (Some(false), Some(-30.0)));
    }

    #[test]
    fn quality_refuses_what_its_test_cant_use() {
        assert!(err(&["quality", "--capture", "B"]).starts_with("'quality' needs --probe DEV"));
        assert!(err(&["quality", "--probe", "A"]).starts_with("'quality' needs --capture DEV"));
        let with = |extra: &[&str]| err(&[&["quality", "--probe", "A", "--capture", "B"][..], extra].concat());
        assert_eq!(with(&["--test", "music"]), "--test takes noise or echo, not 'music'");
        assert_eq!(with(&["--seconds", "4"]), "--seconds takes a length from 5 to 300 s, not '4'");
        assert!(with(&["--seconds", "301"]).starts_with("--seconds takes"));
        assert_eq!(with(&["--snr", "-1"]), "--snr takes the noise's level under the speech, from 0 to 60 dB, not '-1'");
        assert!(with(&["--snr", "61"]).starts_with("--snr takes"));
        assert_eq!(with(&["--speech"]), "--speech needs a value");
        assert_eq!(with(&["--save="]), "--save needs a value");
        assert_eq!(with(&["extra"]), "'quality' takes no argument 'extra'");
        assert_eq!(with(&["--reps", "3"]), "'quality' has no option '--reps'");
        // The echo test needs its far end, and takes its reference from it
        assert!(with(&["--test", "echo"]).starts_with("'quality --test echo' needs --far DEV"));
        assert_eq!(
            with(&["--test", "echo", "--far", "F", "--reference", "R"]),
            "'quality --test echo' takes its echo reference from --far: drop --reference"
        );
        // The far end belongs to the echo test
        assert_eq!(with(&["--far", "F"]), "--far is for the echo test: add --test echo, or drop it");
        assert_eq!(with(&["--far-speech", "you.wav"]), "--far-speech is for the echo test: add --test echo, or drop it");
        assert_eq!(with(&["--echo-delay", "25"]), "--echo-delay is for the echo test: add --test echo, or drop it");
        // The echo delay: 0 to 300 ms
        let echo = |d: &str| with(&["--test", "echo", "--far", "F", "--echo-delay", d]);
        assert_eq!(echo("301"), "--echo-delay takes an echo delay from 0 to 300 ms (e.g. --echo-delay 25), not '301'");
        assert!(echo("-1").starts_with("--echo-delay takes") && echo("x").starts_with("--echo-delay takes"));
        for (d, ms) in [("0", 0.0), ("300", 300.0)] {
            let Ok(Command::Quality(q)) =
                p(&["quality", "--probe", "A", "--capture", "B", "--test", "echo", "--far", "F", "--echo-delay", d])
            else {
                panic!()
            };
            assert_eq!(q.echo_delay_ms, ms);
        }
        // The echo test needs a far-only turn after the first
        let short = with(&["--test", "echo", "--far", "F", "--seconds", "11.9"]);
        assert!(short.starts_with("'quality --test echo' needs --seconds 12 or more"), "{short}");
        assert!(p(&["quality", "--probe", "A", "--capture", "B", "--test", "echo", "--far", "F", "--seconds", "12"]).is_ok());
        // The noise test may still run AEC on a reference of its own
        assert!(p(&["quality", "--probe", "A", "--capture", "B", "--reference", "R", "--aec"]).is_ok());
    }

    #[test]
    fn process_takes_two_files_and_processing() {
        let Ok(Command::Process(a)) = p(&["process", "in.wav", "--ref", "far.wav", "out.wav", "--aec", "--align", "--gate", "static"])
        else {
            panic!()
        };
        assert_eq!(a.input, PathBuf::from("in.wav"));
        assert_eq!(a.output, PathBuf::from("out.wav"));
        assert_eq!(a.reference, Some(PathBuf::from("far.wav")));
        assert_eq!((a.align, a.processing.aec, a.processing.gate), (true, Some(true), Some(GateMode::Static)));
        assert!(!a.warm_up);
        let Ok(Command::Process(a)) = p(&["process", "chain_mic.wav", "out.wav", "--warm-up", "--ref", "chain_ref.wav"]) else { panic!() };
        assert!(a.warm_up && !a.align);
        assert_eq!(err(&["process", "a.wav", "b.wav", "--warm-up=1"]), "--warm-up takes no value");
        assert_eq!(err(&["process", "in.wav"]), "'process' needs IN.wav and OUT.wav");
        assert_eq!(err(&["process"]), "'process' needs IN.wav and OUT.wav");
        assert!(err(&["process", "a.wav", "b.wav", "c.wav"]).contains("'c.wav' is a third"));
        assert_eq!(err(&["process", "a.wav", "b.wav", "--mic", "x"]), "'process' has no option '--mic'");
        // A file name may be "-"-free but numeric
        let Ok(Command::Process(a)) = p(&["process", "-1", "2"]) else { panic!() };
        assert_eq!((a.input, a.output), (PathBuf::from("-1"), PathBuf::from("2")));
    }

    #[test]
    fn bench_takes_an_optional_file_seconds_and_aec() {
        assert_eq!(p(&["bench"]), Ok(Command::Bench(BenchArgs { input: None, seconds: DEFAULT_BENCH_SECONDS, aec: false, model: None })));
        assert_eq!(
            p(&["bench", "voice.wav", "--seconds", "10", "--aec"]),
            Ok(Command::Bench(BenchArgs { input: Some("voice.wav".into()), seconds: 10.0, aec: true, model: None }))
        );
        let Ok(Command::Bench(b)) = p(&["bench", "--model", "ulunas"]) else { panic!() };
        assert_eq!(b.model.as_deref(), Some("ulunas"));
        assert!(err(&["bench", "--model", "UL.UNAS"]).starts_with("--model takes a model's name"));
        assert!(err(&["bench", "--seconds", "1"]).starts_with("--seconds takes a length from 2"));
        // An hour needed 8-10 GB for the synthetic signals (app-privilege F7): 300 s is the most
        assert_eq!(
            p(&["bench", "--seconds", "300"]),
            Ok(Command::Bench(BenchArgs { input: None, seconds: 300.0, aec: false, model: None }))
        );
        assert_eq!(err(&["bench", "--seconds", "301"]), "--seconds takes a length from 2 to 300 s (1 s of it is warm-up), not '301'");
        assert!(err(&["bench", "--seconds=3600"]).starts_with("--seconds takes a length from 2 to 300 s"));
        assert!(err(&["bench", "a.wav", "b.wav"]).contains("'b.wav' is a second"));
        assert_eq!(err(&["bench", "--no-ns"]), "'bench' has no option '--no-ns'");
    }

    /// `--model` and `--high-band` for every command that takes the processing options (TODO 5): the syntax is checked here,
    /// the model's presence at the start, so they parse in any build.
    #[test]
    fn the_noise_model_and_its_high_band_are_processing_options() {
        let commands: [&[&str]; 4] =
            [&["run"], &["latency", "--probe", "a", "--capture", "b"], &["quality", "--probe", "a", "--capture", "b"], &[
                "process", "i.wav", "o.wav",
            ]];
        for cmd in commands {
            let mut a: Vec<&str> = cmd.to_vec();
            a.extend(["--model", "ulunas", "--high-band", "Zero", "--model-depth", "3"]);
            let processing = match p(&a) {
                Ok(Command::Run(r)) => r.engine.processing,
                Ok(Command::Latency(l)) => l.engine.processing,
                Ok(Command::Quality(q)) => q.engine.processing,
                Ok(Command::Process(x)) => x.processing,
                other => panic!("{cmd:?}: {other:?}"),
            };
            assert_eq!((processing.model.as_deref(), processing.high_band), (Some("ulunas"), Some(HighBand::Zero)), "{cmd:?}");
            assert_eq!(processing.model_depth, Some(3), "{cmd:?}");
        }
        let Ok(Command::Run(r)) = p(&["run", "--model=rnnoise"]) else { panic!() };
        assert_eq!(r.engine.processing.model.as_deref(), Some("rnnoise"));
        for bad in ["../ulunas", "C:/x", "UL-UNAS", "a b"] {
            assert!(err(&["run", "--model", bad]).starts_with("--model takes a model's name"), "{bad}");
        }
        assert_eq!(err(&["run", "--high-band", "loud"]), "--high-band takes model, zero or pass, not 'loud'");
        assert_eq!(err(&["run", "--model"]), "--model needs a value");
        for bad in ["0", "5", "two", "-1", "1.5"] {
            assert_eq!(
                err(&["run", "--model-depth", bad]),
                format!("--model-depth takes a whole number of blocks from 1 to 4, not '{bad}'")
            );
        }
        assert!(
            help_text().contains("--model NAME") && help_text().contains("--high-band MODE") && help_text().contains("--model-depth N")
        );
    }

    #[test]
    fn setup_large_pages_takes_a_sid() {
        assert_eq!(p(&["setup-large-pages"]), Ok(Command::SetupLargePages { sid: None, helper: false }));
        assert_eq!(
            p(&["setup-large-pages", "--sid", "S-1-5-21-1-2-3-1001"]),
            Ok(Command::SetupLargePages { sid: Some("S-1-5-21-1-2-3-1001".into()), helper: false })
        );
        // The relaunched helper is marked by its own flag, not by --sid, a user option
        assert_eq!(
            p(&["setup-large-pages", "--sid", "S-1-5-21-1-2-3-1001", "--elevated-helper"]),
            Ok(Command::SetupLargePages { sid: Some("S-1-5-21-1-2-3-1001".into()), helper: true })
        );
        assert_eq!(err(&["setup-large-pages", "--elevated-helper=1"]), "--elevated-helper takes no value");
        assert!(!help_text().contains(HELPER_FLAG), "hidden from --help");
        assert!(err(&["setup-large-pages", "--sid", "Administrator"]).starts_with("--sid takes a SID"));
        assert_eq!(err(&["setup-large-pages", "--sid"]), "--sid needs a value");
        assert_eq!(err(&["setup-large-pages", "now"]), "'setup-large-pages' takes no argument 'now'");
    }

    /// `run --seconds N` stops after N seconds (a soak); without it, it runs until Ctrl+C.
    #[test]
    fn run_takes_seconds_for_a_soak() {
        let Ok(Command::Run(r)) = p(&["run", "--seconds", "3600", "--output", "CABLE Input"]) else { panic!() };
        assert_eq!((r.seconds, r.engine.output.as_deref()), (Some(3600), Some("CABLE Input")));
        assert_eq!(p(&["run", "--seconds=1"]), Ok(Command::Run(RunArgs { seconds: Some(1), ..Default::default() })));
        for bad in ["0", "86401", "1.5", "an hour"] {
            assert_eq!(
                err(&["run", "--seconds", bad]),
                format!("--seconds takes a whole number of seconds from 1 to 86400, not '{bad}'"),
                "{bad}"
            );
        }
        assert_eq!(err(&["run", "--seconds"]), "--seconds needs a value");
    }

    /// `devices` lists; `devices --check` takes `--seconds` (5-120, 15 by default) and `run`'s options, which the list refuses.
    #[test]
    fn devices_check_takes_seconds_and_runs_options() {
        assert_eq!(p(&["devices"]), Ok(Command::Devices));
        assert_eq!(p(&["devices", "--check"]), Ok(Command::DeviceCheck(CheckArgs { engine: EngineArgs::default(), seconds: 15 })));
        let Ok(Command::DeviceCheck(c)) = p(&["devices", "--mic", "USB", "--check", "--seconds", "30", "--no-aec", "--no-exclusive-mic"])
        else {
            panic!()
        };
        assert_eq!((c.seconds, c.engine.mic.as_deref(), c.engine.processing.aec), (30, Some("USB"), Some(false)));
        assert_eq!(c.engine.io.mic_exclusive, Some(false));
        assert_eq!(err(&["devices", "--seconds", "30"]), "--seconds is for 'devices --check': add --check, or drop it");
        assert_eq!(err(&["devices", "--mic", "USB"]), "--mic is for 'devices --check': add --check, or drop it");
        assert_eq!(err(&["devices", "--check", "--seconds", "4"]), "--seconds takes a whole number of seconds from 5 to 120, not '4'");
        assert_eq!(err(&["devices", "--check", "x"]), "'devices' takes no argument 'x'");
        assert_eq!(err(&["devices", "--check=yes"]), "--check takes no value");
        assert!(help_text().contains("devices --check") && help_text().contains("run [--seconds N]"));
    }

    #[test]
    fn only_engine_commands_want_the_pool() {
        let wants = |args: &[&str]| p(args).map(|c| c.needs_memory()).unwrap_or(false);
        assert!(wants(&[]) && wants(&["run"]) && wants(&["bench"]) && wants(&["latency", "--probe", "a", "--capture", "b"]));
        assert!(wants(&["quality", "--probe", "a", "--capture", "b"]));
        // The cable-only test runs no engine
        assert!(!wants(&["latency", "--cable-only", "--probe", "a", "--capture", "b"]));
        assert!(!wants(&["devices"]) && !wants(&["--help"]) && !wants(&["--version"]) && !wants(&["setup-large-pages"]));
        assert!(wants(&["devices", "--check"]), "the check runs the engine");
        assert!(!wants(&["process", "a.wav", "b.wav"]));
    }
}
