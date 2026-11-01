//! `run`: the engine without the window. It prints the streams once started, then a status line a
//! second, until Ctrl+C (or the console closing) stops it cleanly.

use std::time::{Duration, Instant};

use nf_engine::{EngineConfig, EngineState, EngineStatus, REFERENCE_TARGET_HIGH_MS, StreamInfo, TelemetrySnapshot};

use super::{db, fixed, list_devices};
use crate::cli::EngineArgs;
use crate::exit::Failure;
use crate::{config, console};

pub fn run(args: &EngineArgs) -> Result<(), Failure> {
    let cfg = config::engine_config(args, list_devices)?;
    let engine = start(&cfg)?;
    say!("{LEGEND}");
    let first = engine.status();
    let (mut last_message, mut last_warnings) = (first.message, first.warnings);
    let mut ctrl_c = console::CtrlCWatch::default();
    while !console::sleep_unless_stopped(Duration::from_secs(1)) {
        let status = engine.status();
        if status.message != last_message {
            if let Some(m) = &status.message {
                say!("note: {m}");
            }
            last_message = status.message;
        }
        if status.warnings != last_warnings {
            for line in warning_changes(&last_warnings, &status.warnings) {
                say!("{line}");
            }
            last_warnings = status.warnings;
        }
        if let Some(w) = ctrl_c.check() {
            say!("{w}");
        }
        say!("{}", status_line(&engine.telemetry(), status.retrying));
    }
    engine.stop();
    say!("Stopped.");
    console::finished();
    Ok(())
}

/// The longest [`wait_for_adaptive_target`] waits: the adaptive target's window, its longest walk down, and 15 s.
pub const ADAPTIVE_WAIT_S: u64 = nf_engine::ADAPTIVE_OUTPUT_WINDOW_S + nf_engine::ADAPTIVE_OUTPUT_WALK_S + 15;

/// With `--adaptive-output` a run measures at the fixed output target for its first two minutes before the target can come
/// down, and then walks down to where it's going (a tenth of the way a second, up to a minute), so `latency` and `quality`
/// wait for it to settle (up to [`ADAPTIVE_WAIT_S`]) and measure where it settled. Returns true when Ctrl+C cancelled the wait.
pub fn wait_for_adaptive_target(engine: &nf_engine::Engine, cfg: &EngineConfig) -> bool {
    if !cfg.io.adaptive_output {
        return false;
    }
    let from = engine.telemetry().output_target_ms;
    say!(
        "The adaptive output target measures the delivery for {} s before it can come down, and then walks down for up to {} s: \
         waiting for it to settle (up to {ADAPTIVE_WAIT_S} s; Ctrl+C cancels).",
        nf_engine::ADAPTIVE_OUTPUT_WINDOW_S,
        nf_engine::ADAPTIVE_OUTPUT_WALK_S
    );
    let start = Instant::now();
    let mut watch = TargetWatch::new(from);
    while start.elapsed() < Duration::from_secs(ADAPTIVE_WAIT_S) {
        if console::sleep_unless_stopped(Duration::from_secs(1)) {
            return true;
        }
        if let Some(now) = watch.see(engine.telemetry().output_target_ms) {
            let secs = start.elapsed().as_secs();
            if now < from - 0.5 {
                say!("The output target came down from {} to {} ms ({secs} s).", fixed(f64::from(from), 1), fixed(f64::from(now), 1));
            } else {
                say!(
                    "The output target moved and is back at {} ms ({secs} s): it ran short (an underrun raises it).",
                    fixed(f64::from(now), 1)
                );
            }
            return false;
        }
    }
    say!(
        "The output target is {} ms after {ADAPTIVE_WAIT_S} s (it started at {} ms): the delivery needs it, or it ran short.",
        fixed(f64::from(engine.telemetry().output_target_ms), 1),
        fixed(f64::from(from), 1)
    );
    false
}

/// Seconds the adaptive target must hold still, after it has moved, to count as settled: its walk moves it every second
/// until it's there.
const TARGET_STILL_S: u32 = 3;

/// Watches the published output target, a reading a second: settled once it has moved and then held still for
/// [`TARGET_STILL_S`].
#[derive(Clone, Copy, Debug)]
struct TargetWatch {
    last: f32,
    moved: bool,
    still: u32,
}

impl TargetWatch {
    fn new(from: f32) -> Self {
        Self { last: from, moved: false, still: 0 }
    }

    /// This second's target (ms); the target once it has settled.
    fn see(&mut self, ms: f32) -> Option<f32> {
        if ms.is_nan() || ms <= 0.0 {
            // Stopped, or restarting: nothing to watch.
            return None;
        }
        if (ms - self.last).abs() > 1e-4 {
            (self.last, self.moved, self.still) = (ms, true, 0);
            return None;
        }
        self.still += 1;
        (self.moved && self.still >= TARGET_STILL_S).then_some(ms)
    }
}

/// What changed in the engine's standing warnings: each new one, then each one that has cleared.
pub fn warning_changes(old: &[String], new: &[String]) -> Vec<String> {
    let added = new.iter().filter(|w| !old.contains(w)).map(|w| format!("warning: {w}"));
    let cleared = old.iter().filter(|w| !new.contains(w)).map(|w| format!("cleared: {w}"));
    added.chain(cleared).collect()
}

/// Starts an engine with `cfg` (Ctrl+C armed first) and prints what it opened.
pub fn start(cfg: &EngineConfig) -> Result<nf_engine::Engine, Failure> {
    let engine = nf_engine::Engine::new();
    start_engine(&engine, cfg)?;
    Ok(engine)
}

/// [`start`] on an engine of the caller's (one with the chain recorder armed, say).
pub fn start_engine(engine: &nf_engine::Engine, cfg: &EngineConfig) -> Result<(), Failure> {
    console::handle_ctrl_c();
    say!("{}", describe_config(cfg));
    engine.start(cfg).map_err(|e| {
        // A fix the app can take the user to: in a terminal, the command that opens it.
        let hint = engine.status().remedy.map_or(String::new(), |r| format!("\nTo open the setting: start {}", r.settings_uri()));
        Failure::error(format!("Couldn't start the filter: {e}{hint}"))
    })?;
    say!("{}", describe_status(&engine.status()));
    Ok(())
}

/// What the status line's columns are.
pub const LEGEND: &str = "Each second: input and output peak/RMS dBFS; latency median/p95 (mic capture to cable play-out), \
     then its budget's means in ms [mic delivery+wait+output queue+render side] (the chain's offset, given at the start, \
     isn't in it), and with --adaptive-output the output queue's target now; CPU p99 per block; drift output/reference; \
     with an echo reference, its lead/target in ms (how long before the mic block it was tapped; 'by delivery' without \
     trusted stamps, 'high' over 30 ms, which a speaker's echo path may not cover); dropouts mic/output/reference, then, \
     once there are any, mic stalls (100 ms with no packet) and coarse corrections, output/reference (whole blocks dropped \
     to catch up; on the reference, jumps of its timeline); 'excl', then the streams in WASAPI exclusive mode (out, mic); \
     real-time heap allocations (pool allocations per second). Ctrl+C stops.";

/// One line of live telemetry. `retrying`: stopped by a fault, with the engine set to restart by
/// itself (`EngineStatus::retrying`).
pub fn status_line(t: &TelemetrySnapshot, retrying: bool) -> String {
    let state = match t.state {
        EngineState::Running => "running",
        EngineState::Starting => "starting",
        EngineState::Stopped | EngineState::Error if retrying => "retrying",
        EngineState::Stopped => "stopped",
        EngineState::Error => "error",
    };
    let clip = if t.clip_latched {
        " CLIP"
    } else if t.near_clip {
        " near-clip"
    } else {
        ""
    };
    let latency = if t.latency_valid {
        // The budget's means, compact (engine.md, "The latency budget").
        let b = &t.budget_mean;
        let part = |v: f32| fixed(f64::from(v), 1);
        format!(
            "{}/{} ms [{}+{}+{}+{}]",
            fixed(f64::from(t.latency_ms_median), 1),
            fixed(f64::from(t.latency_ms_p95), 1),
            part(b.mic_ms),
            part(b.wait_ms),
            part(b.queue_ms),
            part(b.render_ms)
        )
    } else {
        "--".into()
    };
    // The adaptive output target moves: shown with the budget, whose queue holds it.
    let target = if t.output_adaptive { format!(" target {}", fixed(f64::from(t.output_target_ms), 1)) } else { String::new() };
    let modes = match (t.output_exclusive, t.mic_exclusive) {
        (true, true) => "  excl out+mic",
        (true, false) => "  excl out",
        (false, true) => "  excl mic",
        (false, false) => "",
    };
    // The rarer counters, shown once they count something
    let mut rare = String::new();
    if t.stalls > 0 {
        rare.push_str(&format!(" stalls {}", t.stalls));
    }
    if t.output_coarse_drops > 0 || t.reference_coarse_drops > 0 {
        rare.push_str(&format!(" coarse {}/{}", t.output_coarse_drops, t.reference_coarse_drops));
    }
    let mut reference = String::new();
    if t.reference_target_ms > 0.0 {
        reference = format!("  ref {}/{} ms", fixed(f64::from(t.reference_fill_ms), 1), fixed(f64::from(t.reference_target_ms), 1));
        if !t.reference_stamped {
            reference.push_str(" by delivery");
        }
        if t.reference_target_ms > REFERENCE_TARGET_HIGH_MS {
            reference.push_str(" high");
        }
    }
    format!(
        "{state:<8} in {}/{} dBFS{clip}  out {}/{}  lat {latency}{target}  cpu {} µs  drift {}/{} ppm{reference}  drops {}/{}/{}{rare}{modes}  rt {} ({}/s)",
        db(t.input.peak_dbfs),
        db(t.input.rms_dbfs),
        db(t.output.peak_dbfs),
        db(t.output.rms_dbfs),
        fixed(f64::from(t.block_us_p99), 0),
        signed(t.output_drift_ppm),
        signed(t.reference_drift_ppm),
        t.capture_glitches,
        t.render_underruns,
        t.reference_gaps,
        t.rt_heap_allocations,
        t.rt_pool_allocations_per_s,
    )
}

/// `+12`, `-3`, `0`, `--`.
fn signed(v: f32) -> String {
    if !v.is_finite() {
        "--".into()
    } else if v.round() == 0.0 {
        "0".into()
    } else {
        format!("{v:+.0}")
    }
}

/// What `run` asked for, before the streams open.
pub fn describe_config(cfg: &EngineConfig) -> String {
    let p = &cfg.processing;
    let or_default = |d: &Option<String>| d.as_deref().filter(|s| !s.is_empty()).unwrap_or("the default").to_owned();
    let gate = match p.gate.mode {
        nf_engine::GateMode::Off => "off".to_owned(),
        nf_engine::GateMode::Static => format!("static at {} dBFS", fixed(f64::from(p.gate.threshold_db), 1)),
        nf_engine::GateMode::Auto => format!("auto, {} dB over the noise floor", fixed(f64::from(p.gate.auto_margin_db), 1)),
    };
    let io = &cfg.io;
    let asked: Vec<&str> =
        [(io.output_exclusive, "exclusive output"), (io.mic_exclusive, "exclusive mic"), (io.adaptive_output, "adaptive output target")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, what)| *what)
            .collect();
    let latency = if asked.is_empty() { String::new() } else { format!("; {}", asked.join(", ")) };
    format!(
        "Starting: mic {}, output {}{}; noise filter {}, floor {} dB, gate {gate}{latency}",
        or_default(&cfg.devices.mic),
        or_default(&cfg.devices.output),
        if p.aec_enabled { format!(", echo cancel on (reference {})", or_default(&cfg.devices.reference)) } else { String::new() },
        if p.noise_enabled { "on" } else { "off" },
        fixed(f64::from(p.floor_db), 0),
    )
}

/// The streams, memory and chain once started.
pub fn describe_status(s: &EngineStatus) -> String {
    let mut lines = vec![stream_line("Mic", s.mic.as_ref()), stream_line("Output", s.output.as_ref())];
    if s.reference.is_some() {
        lines.push(stream_line("Reference", s.reference.as_ref()));
    }
    lines.push(format!(
        "{:<10} {}; MMCSS {}; chain {} ms",
        "Engine",
        if s.memory.is_empty() { "memory unknown" } else { &s.memory },
        if s.mmcss { "Pro Audio" } else { "unavailable" },
        fixed(f64::from(s.chain_latency_ms), 2)
    ));
    if let Some(m) = &s.message {
        lines.push(format!("note: {m}"));
    }
    lines.extend(warning_changes(&[], &s.warnings));
    lines.join("\n")
}

fn stream_line(label: &str, s: Option<&StreamInfo>) -> String {
    match s {
        Some(s) => format!("{label:<10} {}", stream_text(s)),
        None => format!("{label:<10} --"),
    }
}

/// "CABLE Input (48000 Hz, 2 ch, period 10.00 ms, RAW)"; "CABLE Input (48000 Hz, 2 ch, period 2.00 ms, exclusive 16-bit)".
pub fn stream_text(s: &StreamInfo) -> String {
    let mut flags: String = [(s.low_latency, ", low-latency period"), (s.raw, ", RAW"), (s.autoconvert, ", rate converted")]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, f)| *f)
        .collect();
    if let Some(format) = s.exclusive {
        flags.push_str(&format!(", exclusive {format}"));
    }
    if s.exclusive_refused.is_some() {
        // Why is in the warning.
        flags.push_str(", exclusive refused");
    }
    format!("{} ({} Hz, {} ch, period {} ms{flags})", s.device_name, s.sample_rate, s.channels, fixed(f64::from(s.period_ms), 2))
}

#[cfg(test)]
mod tests {
    /// The adaptive target's wait: nothing until it has moved, then settled once it holds still for 3 s; a walk's steps
    /// (every second) keep it waiting, and a stopped engine's 0 doesn't count.
    #[test]
    fn the_adaptive_wait_settles_after_the_walk() {
        let mut w = super::TargetWatch::new(22.0);
        let seen: Vec<Option<f32>> = [22.0, 22.0, 22.0, 21.5, 21.0, 20.6, 0.0, 20.6, 20.6, 20.6, 20.6].iter().map(|&t| w.see(t)).collect();
        assert_eq!(seen, [None, None, None, None, None, None, None, None, None, Some(20.6), Some(20.6)]);
    }

    use nf_engine::{GateMode, MeterSnapshot, ProcessingConfig};

    use super::*;

    #[test]
    fn the_status_line_shows_levels_latency_cpu_drift_drops_and_allocations() {
        let t = TelemetrySnapshot {
            state: EngineState::Running,
            input: MeterSnapshot { peak_dbfs: -12.34, rms_dbfs: -30.0 },
            output: MeterSnapshot { peak_dbfs: -200.0, rms_dbfs: f32::NAN },
            clip_latched: true,
            latency_valid: true,
            latency_ms_median: 21.26,
            latency_ms_p95: 23.0,
            budget_mean: nf_engine::LatencyBudget { mic_ms: 10.71, wait_ms: 0.04, chain_ms: 10.0, queue_ms: 6.02, render_ms: 4.96 },
            block_us_p99: 131.4,
            output_drift_ppm: 12.4,
            reference_drift_ppm: -3.2,
            capture_glitches: 1,
            render_underruns: 2,
            reference_gaps: 3,
            rt_heap_allocations: 0,
            rt_pool_allocations_per_s: 4300,
            ..Default::default()
        };
        assert_eq!(
            status_line(&t, false),
            "running  in -12.3/-30.0 dBFS CLIP  out -inf/--  lat 21.3/23.0 ms [10.7+0.0+6.0+5.0]  cpu 131 µs  drift +12/-3 ppm  \
             drops 1/2/3  rt 0 (4300/s)"
        );
        assert!(LEGEND.contains("[mic delivery+wait+output queue+render side]"));
        let idle = TelemetrySnapshot { state: EngineState::Error, near_clip: true, output_drift_ppm: f32::NAN, ..Default::default() };
        let line = status_line(&idle, false);
        assert!(line.starts_with("error    in 0.0/0.0 dBFS near-clip"), "{line}");
        assert!(line.contains("lat --") && line.contains("drift --/0 ppm"), "{line}");
        // A fault the engine is retrying by itself says so
        assert!(status_line(&idle, true).starts_with("retrying in 0.0/0.0 dBFS"), "{}", status_line(&idle, true));
        let stopped = TelemetrySnapshot { state: EngineState::Stopped, ..Default::default() };
        assert!(status_line(&stopped, true).starts_with("retrying "));
        assert!(status_line(&stopped, false).starts_with("stopped "));
        assert!(status_line(&t, true).starts_with("running "), "a running engine isn't retrying");
    }

    /// The new counters appear once they count something: mic stalls, and coarse drops on the
    /// output and the reference.
    #[test]
    fn stalls_and_coarse_drops_show_once_there_are_any() {
        let quiet = TelemetrySnapshot { state: EngineState::Running, capture_glitches: 1, ..Default::default() };
        let line = status_line(&quiet, false);
        assert!(line.contains("drops 1/0/0  rt 0") && !line.contains("stalls") && !line.contains("coarse"), "{line}");
        let t = TelemetrySnapshot { stalls: 2, ..quiet };
        assert!(status_line(&t, false).contains("drops 1/0/0 stalls 2  rt 0"), "{}", status_line(&t, false));
        let t = TelemetrySnapshot { reference_coarse_drops: 4, ..quiet };
        assert!(status_line(&t, false).contains("drops 1/0/0 coarse 0/4  rt 0"), "{}", status_line(&t, false));
        let t = TelemetrySnapshot { stalls: 1, output_coarse_drops: 3, reference_coarse_drops: 5, ..quiet };
        assert!(status_line(&t, false).contains("drops 1/0/0 stalls 1 coarse 3/5  rt 0"), "{}", status_line(&t, false));
        assert!(LEGEND.contains("stalls") && LEGEND.contains("coarse"));
    }

    /// With a reference, its lead and target follow the drift, and say when the delivery times it or the target is high.
    #[test]
    fn the_reference_lead_shows_with_a_reference() {
        let quiet = TelemetrySnapshot { state: EngineState::Running, ..Default::default() };
        assert!(!status_line(&quiet, false).contains("ref "), "{}", status_line(&quiet, false));
        let t = TelemetrySnapshot { reference_fill_ms: 23.36, reference_target_ms: 23.4, reference_stamped: true, ..quiet };
        assert!(status_line(&t, false).contains("drift 0/0 ppm  ref 23.4/23.4 ms  drops 0/0/0"), "{}", status_line(&t, false));
        let t = TelemetrySnapshot { reference_target_ms: 31.0, reference_stamped: false, ..t };
        assert!(status_line(&t, false).contains("ref 23.4/31.0 ms by delivery high  drops"), "{}", status_line(&t, false));
        assert!(LEGEND.contains("lead/target"));
    }

    /// The latency options in effect: the adaptive target after the budget (it moves), and the streams in exclusive mode.
    #[test]
    fn the_status_line_shows_the_adaptive_target_and_exclusive_streams() {
        let t = TelemetrySnapshot {
            state: EngineState::Running,
            latency_valid: true,
            latency_ms_median: 18.3,
            latency_ms_p95: 19.0,
            budget_mean: nf_engine::LatencyBudget { mic_ms: 10.3, wait_ms: 0.0, chain_ms: 10.0, queue_ms: 5.04, render_ms: 3.0 },
            output_target_ms: 5.0,
            output_adaptive: true,
            output_exclusive: true,
            mic_exclusive: true,
            ..Default::default()
        };
        let line = status_line(&t, false);
        assert!(line.contains("lat 18.3/19.0 ms [10.3+0.0+5.0+3.0] target 5.0  cpu"), "{line}");
        assert!(line.contains("drops 0/0/0  excl out+mic  rt 0"), "{line}");
        let shared = TelemetrySnapshot { output_adaptive: false, output_exclusive: false, ..t };
        let line = status_line(&shared, false);
        assert!(line.contains("[10.3+0.0+5.0+3.0]  cpu") && line.contains("drops 0/0/0  excl mic  rt"), "{line}");
        assert!(!status_line(&TelemetrySnapshot { mic_exclusive: false, ..shared }, false).contains("excl"));
        assert!(LEGEND.contains("--adaptive-output") && LEGEND.contains("'excl'"));
    }

    #[test]
    fn warnings_are_printed_when_they_change() {
        let s = |v: &[&str]| v.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert!(warning_changes(&s(&["a"]), &s(&["a"])).is_empty());
        assert_eq!(warning_changes(&[], &s(&["the echo reference was lost"])), ["warning: the echo reference was lost"]);
        assert_eq!(warning_changes(&s(&["a", "b"]), &s(&["b", "c"])), ["warning: c", "cleared: a"]);
        assert_eq!(warning_changes(&s(&["a"]), &[]), ["cleared: a"]);
    }

    #[test]
    fn the_start_lines_say_what_runs() {
        let mut cfg = EngineConfig::default();
        cfg.devices.output = Some("{cable}".into());
        assert_eq!(describe_config(&cfg), "Starting: mic the default, output {cable}; noise filter on, floor -60 dB, gate off");
        cfg.processing = ProcessingConfig { aec_enabled: true, noise_enabled: false, floor_db: -20.0, ..Default::default() };
        cfg.processing.gate.mode = GateMode::Static;
        cfg.processing.gate.threshold_db = -45.0;
        assert_eq!(
            describe_config(&cfg),
            "Starting: mic the default, output {cable}, echo cancel on (reference the default); noise filter off, floor -20 dB, gate static at -45.0 dBFS"
        );
        cfg.io.output_exclusive = true;
        cfg.io.adaptive_output = true;
        assert!(describe_config(&cfg).ends_with("gate static at -45.0 dBFS; exclusive output, adaptive output target"));
        let out = StreamInfo {
            device_name: "CABLE Input".into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms: 2.0,
            low_latency: false,
            autoconvert: false,
            raw: false,
            exclusive: Some("16-bit"),
            exclusive_refused: None,
        };
        assert_eq!(stream_text(&out), "CABLE Input (48000 Hz, 2 ch, period 2.00 ms, exclusive 16-bit)");
        let refused = StreamInfo { exclusive: None, period_ms: 10.0, exclusive_refused: Some("another app is using it".into()), ..out };
        assert_eq!(stream_text(&refused), "CABLE Input (48000 Hz, 2 ch, period 10.00 ms, exclusive refused)");

        let s = EngineStatus {
            mic: Some(StreamInfo {
                device_name: "Mic".into(),
                sample_rate: 48_000,
                channels: 2,
                period_ms: 3.33,
                low_latency: true,
                autoconvert: false,
                raw: true,
                exclusive: None,
                exclusive_refused: None,
            }),
            memory: "large pages (32 MiB)".into(),
            mmcss: true,
            chain_latency_ms: 10.0,
            message: Some("the reference couldn't be opened".into()),
            warnings: vec!["the mic is the output cable's own recording side".into()],
            ..Default::default()
        };
        assert_eq!(
            describe_status(&s),
            "Mic        Mic (48000 Hz, 2 ch, period 3.33 ms, low-latency period, RAW)\nOutput     --\n\
             Engine     large pages (32 MiB); MMCSS Pro Audio; chain 10.00 ms\nnote: the reference couldn't be opened\n\
             warning: the mic is the output cable's own recording side"
        );
    }
}
