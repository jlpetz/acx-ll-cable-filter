//! The CLI commands (app.md "Commands"). Each returns `Ok` or a [`Failure`] carrying its exit code;
//! `main` prints the failure. The window (`gui`) is in main.rs.

mod bench;
mod check;
mod devices;
mod lab;
mod latency;
mod mic;
mod process;
mod quality;
mod remap;
mod run;
mod setup;

use crate::cli::{Command, help_text};
use crate::exit::Failure;

/// Runs a CLI command (anything but the window).
pub fn run_cli(command: &Command) -> Result<(), Failure> {
    match command {
        Command::Help => {
            say!("{}", help_text());
            Ok(())
        }
        Command::Version => {
            say!("noise-filter {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Devices => devices::devices(),
        Command::RemapDevices { map, config } => remap::run(map, config.as_deref()),
        Command::DeviceCheck(args) => check::check(args),
        Command::Run(args) => run::run(args),
        Command::Latency(args) => latency::latency(args),
        Command::Quality(args) => quality::quality(args),
        Command::Process(args) => process::process(args),
        Command::Bench(args) => bench::bench(args),
        Command::Lab(args) => lab::lab(args),
        Command::SetupLargePages { sid, helper } => setup::setup_large_pages(sid.as_deref(), *helper),
        Command::MicSessions => mic::sessions(),
        Command::MicVariant(args) => mic::variant(args),
        Command::Gui => Err(Failure::usage("the window isn't a CLI command")),
    }
}

/// The device list, with the engine's error as text (for `config::engine_config`).
fn list_devices() -> Result<Vec<nf_engine::DeviceInfo>, String> {
    nf_engine::list_devices().map_err(|e| e.0)
}

/// `-23.4`, `-inf` at or below -120 dB, `--` for none (NaN).
fn db(v: f32) -> String {
    if v.is_nan() {
        "--".into()
    } else if v <= -120.0 {
        "-inf".into()
    } else {
        format!("{v:.1}")
    }
}

/// `v` to `places` decimals, `--` if it isn't finite.
pub(crate) fn fixed(v: f64, places: usize) -> String {
    if v.is_finite() { format!("{v:.places$}") } else { "--".into() }
}

/// A latency budget in one line (engine.md, "The latency budget"): "mic delivery 16.2 + wait 0.1 + output queue 22.0 + render
/// side 23.4 = readout 61.7 ms; the chain's 10.0 ms on top: 71.7 ms". The readout holds the four parts; the chain's offset
/// comes on top of it for the first sample's whole way. The direct driver route's readout ends at the sample time, what the
/// sweeps measure, and the driver's delivery comes after it (the owner, 2026-11-24): "mic delivery 10.4 + wait 0.1 + ring
/// queue 1.5 = readout 12.0 ms to the sample time, delivered 2.2 ms after it: 14.2 ms; the chain's 10.0 ms on top: 22.0 ms".
fn budget_text(b: &nf_engine::LatencyBudget) -> String {
    let ms = |v: f32| fixed(f64::from(v), 1);
    if b.direct() {
        return format!(
            "mic delivery {} + wait {} + ring queue {} = readout {} ms to the sample time, delivered {} ms after it: {} ms; the \
             chain's {} ms on top: {} ms",
            ms(b.mic_ms),
            ms(b.wait_ms),
            ms(b.queue_ms),
            ms(b.readout_ms()),
            ms(b.delivery_ms),
            ms(b.delivered_ms()),
            ms(b.chain_ms),
            ms(b.total_ms())
        );
    }
    format!(
        "mic delivery {} + wait {} + output queue {} + render side {} = readout {} ms; the chain's {} ms on top: {} ms",
        ms(b.mic_ms),
        ms(b.wait_ms),
        ms(b.queue_ms),
        ms(b.render_ms),
        ms(b.readout_ms()),
        ms(b.chain_ms),
        ms(b.total_ms())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_numbers_read_well() {
        assert_eq!(db(-23.44), "-23.4");
        assert_eq!(db(0.0), "0.0");
        assert_eq!(db(-120.0), "-inf");
        assert_eq!(db(f32::NEG_INFINITY), "-inf");
        assert_eq!(db(f32::NAN), "--");
        assert_eq!(fixed(1.234, 2), "1.23");
        assert_eq!(fixed(f64::NAN, 1), "--");
        assert_eq!(fixed(f64::INFINITY, 1), "--");
    }

    #[test]
    fn a_budget_reads_as_its_parts_the_readout_and_the_chain_on_top() {
        let b =
            nf_engine::LatencyBudget { mic_ms: 16.24, wait_ms: 0.08, chain_ms: 10.0, queue_ms: 22.04, render_ms: 23.44, delivery_ms: 0.0 };
        assert_eq!(
            budget_text(&b),
            "mic delivery 16.2 + wait 0.1 + output queue 22.0 + render side 23.4 = readout 61.8 ms; the chain's 10.0 ms on top: 71.8 ms"
        );
        // The direct driver route: to the sample time, and the delivery after it.
        let d = nf_engine::LatencyBudget { mic_ms: 10.4, wait_ms: 0.1, chain_ms: 10.0, queue_ms: 1.5, render_ms: 0.0, delivery_ms: 2.2 };
        assert_eq!(
            budget_text(&d),
            "mic delivery 10.4 + wait 0.1 + ring queue 1.5 = readout 12.0 ms to the sample time, delivered 2.2 ms after it: 14.2 ms; \
             the chain's 10.0 ms on top: 22.0 ms"
        );
        let none = nf_engine::LatencyBudget { mic_ms: f32::NAN, ..b };
        assert!(budget_text(&none).starts_with("mic delivery -- + wait 0.1") && budget_text(&none).contains("readout -- ms"));
    }

    #[test]
    fn the_window_isnt_run_as_a_cli_command() {
        assert_eq!(run_cli(&Command::Gui).unwrap_err().exit, crate::exit::Exit::Usage);
    }
}
