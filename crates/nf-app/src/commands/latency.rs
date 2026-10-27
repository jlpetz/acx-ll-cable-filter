//! `latency`: starts the engine with `run`'s options, runs the sweep test (TODO #13) and prints the
//! report. Exit 0 with at least one valid repetition, 3 with none or when cancelled, 2 if the test
//! couldn't run.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use nf_engine::{Flow, LatencyMode, LatencyProgress, LatencyReport, LatencyTestRequest};

use super::{fixed, list_devices};
use crate::cli::LatencyArgs;
use crate::config::{self, resolve_device};
use crate::console;
use crate::exit::Failure;

/// Time for the streams and the drift loops to settle before the sweeps.
const SETTLE: Duration = Duration::from_secs(2);
/// How often progress is read.
const POLL: Duration = Duration::from_millis(200);

pub fn latency(args: &LatencyArgs) -> Result<(), Failure> {
    let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    let cfg = config::engine_config(&args.engine, || Ok(devices.clone()))?;
    let probe = resolve_device(&devices, &args.probe, Flow::Render, "--probe").map_err(Failure::error)?;
    let capture = resolve_device(&devices, &args.capture, Flow::Capture, "--capture").map_err(Failure::error)?;
    let engine = super::run::start(&cfg)?;
    let result = measure(&engine, args, probe, capture);
    engine.stop();
    console::finished();
    result
}

fn measure(engine: &nf_engine::Engine, args: &LatencyArgs, probe: String, capture: String) -> Result<(), Failure> {
    if console::sleep_unless_stopped(SETTLE) {
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

/// The report, as the window shows it.
pub fn format_report(r: &LatencyReport) -> String {
    let ms = |v: f64| format!("{} ms", fixed(v, 2));
    let mut s = String::new();
    let _ = write!(s, "Latency: {} of {} repetitions valid", r.valid, r.repetitions);
    if r.valid > 0 {
        let _ = write!(s, " (lowest confidence {} dB)", fixed(f64::from(r.confidence_db_min), 1));
        let each: Vec<String> = r.app_ms.iter().map(|v| fixed(*v, 2)).collect();
        let _ = write!(
            s,
            "\n  app + output cable  {} median, spread {} [{}]",
            ms(r.app_ms_median),
            ms(r.app_ms_spread),
            each.join(", ")
        );
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
             chain offset        10.00 ms\n  \
             conditions: mic 10 ms\n  \
             warning: one repetition was weak"
        );
        let none = LatencyReport { repetitions: 5, app_ms_median: f64::NAN, confidence_db_min: f32::NAN, ..Default::default() };
        assert_eq!(format_report(&none), "Latency: 0 of 5 repetitions valid\n  chain offset        0.00 ms");
    }
}
