//! `latency`: starts the engine with `run`'s options, runs the sweep test (TODO #13) and prints the
//! report. Exit 0 with at least one valid repetition, 3 with none or when cancelled, 2 if the test
//! couldn't run.
//!
//! `latency --cable-only` runs no engine: the same sweeps from the probe straight to the capture (`nf_engine::cable_latency_test`,
//! on this thread), which times one cable alone, to tell its share of the latency from the app's.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use nf_engine::{CableLatencyReport, CableLatencyRequest, Flow, LatencyMode, LatencyProgress, LatencyReport, LatencyTestRequest};

use super::{budget_text, fixed, list_devices};
use crate::cli::LatencyArgs;
use crate::config::{self, resolve_device};
use crate::console;
use crate::exit::Failure;

/// Time for the streams and the drift loops to settle before the sweeps.
const SETTLE: Duration = Duration::from_secs(2);
/// How often progress is read.
const POLL: Duration = Duration::from_millis(200);

pub fn latency(args: &LatencyArgs) -> Result<(), Failure> {
    if args.cable_only {
        return cable_only(args);
    }
    let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    let cfg = config::engine_config(&args.engine, || Ok(devices.clone()))?;
    let probe = resolve_device(&devices, &args.probe, Flow::Render, "--probe").map_err(Failure::error)?;
    let capture = resolve_device(&devices, &args.capture, Flow::Capture, "--capture").map_err(Failure::error)?;
    let engine = super::run::start(&cfg)?;
    let result = measure(&engine, &cfg, args, probe, capture);
    engine.stop();
    console::finished();
    result
}

fn measure(
    engine: &nf_engine::Engine, cfg: &nf_engine::EngineConfig, args: &LatencyArgs, probe: String, capture: String,
) -> Result<(), Failure> {
    if console::sleep_unless_stopped(SETTLE) || super::run::wait_for_adaptive_target(engine, cfg, args.settle) {
        return Err(Failure::no_result("Cancelled before the test started."));
    }
    let request = LatencyTestRequest {
        mode: args.mode,
        probe_device: probe,
        capture_device: capture,
        repetitions: args.repetitions,
        through_model: args.through_model,
    };
    let test = engine.start_latency_test(request).map_err(|e| Failure::error(format!("The latency test didn't start: {e}")))?;
    say!(
        "Latency test: {} sweeps, {} loop. Ctrl+C cancels.",
        args.repetitions,
        if args.mode == LatencyMode::Acoustic { "acoustic" } else { "virtual" }
    );
    // Each sweep is about 1 s plus gaps; anything far past that is a stuck test.
    let deadline = Instant::now() + Duration::from_secs(60 + 5 * u64::from(args.repetitions));
    let (mut cancelled, mut shown) = (false, (String::new(), -1i32));
    let mut ctrl_c = console::CtrlCWatch::default();
    loop {
        if let Some(w) = ctrl_c.check() {
            say!("{w}");
        }
        match test.progress() {
            LatencyProgress::Running { fraction, stage } => {
                let tenth = (fraction.clamp(0.0, 1.0) * 10.0) as i32;
                if shown.0 != stage || shown.1 != tenth {
                    say!("  {stage}… {}%", tenth * 10);
                    shown = (stage.to_owned(), tenth);
                }
            }
            LatencyProgress::Done(report) => {
                say!("{}", format_report(&report));
                return if report.valid > 0 {
                    Ok(())
                } else {
                    Err(Failure::no_result("No repetition gave a valid result (see the warnings above)."))
                };
            }
            LatencyProgress::Failed(message) => return Err(Failure::error(format!("The latency test failed: {message}"))),
            LatencyProgress::Cancelled => return Err(Failure::no_result("The latency test was cancelled.")),
        }
        if !cancelled && (console::stop_requested() || Instant::now() > deadline) {
            if !console::stop_requested() {
                say!("The test is taking far longer than it should: cancelling it.");
            }
            test.cancel();
            cancelled = true;
        }
        std::thread::sleep(POLL);
    }
}

/// The cable-only test: no engine, the sweeps blocking this thread (Ctrl+C cancels them).
fn cable_only(args: &LatencyArgs) -> Result<(), Failure> {
    console::handle_ctrl_c();
    let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    let probe = resolve_device(&devices, &args.probe, Flow::Render, "--probe").map_err(Failure::error)?;
    let capture = resolve_device(&devices, &args.capture, Flow::Capture, "--capture").map_err(Failure::error)?;
    let name = |id: &str| devices.iter().find(|d| d.id == id).map_or_else(|| id.to_owned(), |d| d.name.clone());
    say!(
        "Latency test, cable only: {} sweeps from {} to {}, without the filter (about {} s). Ctrl+C cancels.",
        args.repetitions,
        name(&probe),
        name(&capture),
        2 * args.repetitions + 2
    );
    let request = CableLatencyRequest {
        probe_device: probe,
        capture_device: capture,
        repetitions: args.repetitions,
        cancel: Some(console::stop_flag()),
    };
    // The test blocks this thread: say now if Ctrl+C can't reach it.
    if let Some(w) = console::CtrlCWatch::default().check() {
        say!("{w}");
    }
    let result = nf_engine::cable_latency_test(&request);
    console::finished();
    match result {
        Ok(report) => {
            say!("{}", format_cable_report(&report));
            if report.valid > 0 { Ok(()) } else { Err(Failure::no_result("No repetition gave a valid result (see the warnings above).")) }
        }
        Err(_) if console::stop_requested() => Err(Failure::no_result("The latency test was cancelled.")),
        Err(e) => Err(Failure::error(format!("The latency test failed: {e}"))),
    }
}

/// The cable-only report. Against the engine test's report: the mic cable's figure compares with its input path, and the output
/// cable's with its "app + output cable" share, which holds it (TODO #13's detail, "Cable-only mode").
pub fn format_cable_report(r: &CableLatencyReport) -> String {
    let ms = |v: f64| format!("{} ms", fixed(v, 2));
    let mut s = String::new();
    let _ = write!(s, "Latency (cable only): {} of {} repetitions valid", r.valid, r.repetitions);
    if r.valid > 0 {
        let _ = write!(s, " (lowest confidence {} dB)", fixed(f64::from(r.confidence_db_min), 1));
        let each: Vec<String> = r.cable_ms.iter().map(|v| fixed(*v, 2)).collect();
        let _ =
            write!(s, "\n  probe to capture    {} median, spread {} [{}]", ms(r.cable_ms_median), ms(r.cable_ms_spread), each.join(", "));
        let _ = write!(
            s,
            "\n  compare with        the engine test's input path (the mic cable), or its app + output cable (the output cable's \
             share is in it)"
        );
    }
    if !r.conditions.is_empty() {
        let _ = write!(s, "\n  conditions: {}", r.conditions);
    }
    for w in &r.warnings {
        let _ = write!(s, "\n  warning: {w}");
    }
    s
}

/// The report, as the window shows it.
pub fn format_report(r: &LatencyReport) -> String {
    let ms = |v: f64| format!("{} ms", fixed(v, 2));
    let mut s = String::new();
    let _ = write!(s, "Latency: {} of {} repetitions valid", r.valid, r.repetitions);
    if r.valid > 0 {
        let _ = write!(s, " (lowest confidence {} dB)", fixed(f64::from(r.confidence_db_min), 1));
        let each: Vec<String> = r.app_ms.iter().map(|v| fixed(*v, 2)).collect();
        let _ = write!(s, "\n  app + output cable  {} median, spread {} [{}]", ms(r.app_ms_median), ms(r.app_ms_spread), each.join(", "));
    }
    if let Some(v) = r.input_path_ms_median {
        let _ = write!(s, "\n  input path          {} median (probe play-out to the app's input)", ms(v));
    }
    if let Some(v) = r.end_to_end_ms_median {
        let _ = write!(s, "\n  end to end          {} median (probe play-out to the output capture)", ms(v));
    }
    if let Some(v) = r.readout_ms_median {
        let _ = write!(s, "\n  live readout        {} median", ms(f64::from(v)));
    }
    if let Some(b) = &r.budget {
        let _ = write!(s, "\n  latency budget      {}, means over the test", budget_text(b));
    }
    if let Some(e) = &r.output {
        // A spread between the sweeps that these don't explain is the devices' own (TODO 9).
        let _ = write!(s, "\n  output events       {} over the test", e.text());
    }
    let _ = write!(s, "\n  chain offset        {}", ms(f64::from(r.chain_ms)));
    if !r.conditions.is_empty() {
        let _ = write!(s, "\n  conditions: {}", r.conditions);
    }
    for w in &r.warnings {
        let _ = write!(s, "\n  warning: {w}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_lists_each_measure_it_has() {
        let r = LatencyReport {
            repetitions: 5,
            valid: 4,
            app_ms: vec![12.3, 12.35, 12.4, 12.31],
            app_ms_median: 12.33,
            app_ms_spread: 0.1,
            input_path_ms_median: Some(3.2),
            end_to_end_ms_median: None,
            readout_ms_median: Some(12.0),
            budget: Some(nf_engine::LatencyBudget { mic_ms: 10.5, wait_ms: 0.0, chain_ms: 10.0, queue_ms: 22.0, render_ms: 5.0 }),
            output: Some(nf_engine::OutputEvents { slips: 3, slip_net: 1, ..Default::default() }),
            chain_ms: 10.0,
            confidence_db_min: 31.26,
            conditions: "mic 10 ms".into(),
            warnings: vec!["one repetition was weak".into()],
        };
        assert_eq!(
            format_report(&r),
            "Latency: 4 of 5 repetitions valid (lowest confidence 31.3 dB)\n  \
             app + output cable  12.33 ms median, spread 0.10 ms [12.30, 12.35, 12.40, 12.31]\n  \
             input path          3.20 ms median (probe play-out to the app's input)\n  \
             live readout        12.00 ms median\n  \
             latency budget      mic delivery 10.5 + wait 0.0 + output queue 22.0 + render side 5.0 = readout 37.5 ms; the chain's \
             10.0 ms on top: 47.5 ms, means over the test\n  \
             output events       no underruns, zeros, drops or re-anchors; 3 slips (net +1 samples) over the test\n  \
             chain offset        10.00 ms\n  \
             conditions: mic 10 ms\n  \
             warning: one repetition was weak"
        );
        let none = LatencyReport { repetitions: 5, app_ms_median: f64::NAN, confidence_db_min: f32::NAN, ..Default::default() };
        assert_eq!(format_report(&none), "Latency: 0 of 5 repetitions valid\n  chain offset        0.00 ms");
    }

    #[test]
    fn the_cable_only_report_is_labelled_and_lists_each_repetition() {
        let r = CableLatencyReport {
            repetitions: 5,
            valid: 3,
            cable_ms: vec![55.1, 56.25, 55.6],
            cable_ms_median: 55.6,
            cable_ms_spread: 1.15,
            confidence_db_min: 42.04,
            conditions: "cable only; probe: CABLE-A Input (48000 Hz, 2 ch, 10.00 ms period)".into(),
            warnings: vec!["Only 3 of 5 sweeps were found clearly in the capture; the result is less certain.".into()],
        };
        assert_eq!(
            format_cable_report(&r),
            "Latency (cable only): 3 of 5 repetitions valid (lowest confidence 42.0 dB)\n  \
             probe to capture    55.60 ms median, spread 1.15 ms [55.10, 56.25, 55.60]\n  \
             compare with        the engine test's input path (the mic cable), or its app + output cable (the output cable's \
             share is in it)\n  \
             conditions: cable only; probe: CABLE-A Input (48000 Hz, 2 ch, 10.00 ms period)\n  \
             warning: Only 3 of 5 sweeps were found clearly in the capture; the result is less certain."
        );
        let none = CableLatencyReport { repetitions: 2, cable_ms_median: f64::NAN, confidence_db_min: f32::NAN, ..Default::default() };
        assert_eq!(format_cable_report(&none), "Latency (cable only): 0 of 2 repetitions valid");
    }
}
