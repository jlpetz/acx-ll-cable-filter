//! `devices --check`: the filter runs quietly on the chosen devices for a few seconds (15 by default), with the chain recorder
//! keeping each block's facts, and then says how each stream delivered (`crate::health`, TODO 21): steady, in bursts or late,
//! its drift and glitches, and what to try. Echo cancelling is on unless `--no-aec`, so the speakers' loopback is checked too.
//! It only reports: nothing is saved (no calibration yet). Exit 0 once it has run, 3 when Ctrl+C stopped it before 3 s.
//!
//! The figures are one run's: the blocks are the chain recording's (it ends with the run it began with), the streams as that
//! run opened them, and its counters as its last publication had them. After a restart they stay the first run's: the
//! next run's counters start again from 0.

use std::time::{Duration, Instant};

use nf_engine::{Engine, EngineStatus, TelemetrySnapshot};

use super::list_devices;
use crate::cli::CheckArgs;
use crate::exit::Failure;
use crate::{config, console, health};

/// The least of a check that's worth a report, in seconds.
const MIN_REPORT_S: f64 = 3.0;

pub fn check(args: &CheckArgs) -> Result<(), Failure> {
    let mut engine_args = args.engine.clone();
    // The speakers' loopback is a stream to check too, opened only for echo cancelling.
    engine_args.processing.aec.get_or_insert(true);
    let mut cfg = config::engine_config(&engine_args, list_devices)?;
    // The mic's delivery and the render side are the latency budget's: the readout has to be on.
    cfg.processing.latency_readout_enabled = true;
    let engine = Engine::new();
    // Armed before the start: the recording holds the run from its first block.
    let recorder =
        engine.record_chain(f64::from(args.seconds) + 5.0).map_err(|e| Failure::error(format!("Couldn't arm the chain recorder: {e}")))?;
    super::run::start_engine(&engine, &cfg)?;
    say!("Checking the devices for {} s: the filter runs as usual meanwhile. Ctrl+C stops it sooner.", args.seconds);
    let mut run = FirstRun::new(engine.status(), engine.telemetry());
    let started = Instant::now();
    let limit = Duration::from_secs(u64::from(args.seconds));
    let mut cancelled = false;
    let mut next_note = 5;
    while started.elapsed() < limit {
        if console::sleep_unless_stopped(limit.saturating_sub(started.elapsed()).min(Duration::from_secs(1))) {
            cancelled = true;
            break;
        }
        run.see(engine.status(), engine.telemetry());
        let s = started.elapsed().as_secs();
        if s >= next_note && s < u64::from(args.seconds) {
            say!("  {s} s...");
            next_note += 5;
        }
    }
    // The counters are published once a second: one more publication brings in the check's last moments.
    if !run.restarted
        && let Some(t) = super::run::next_publication(&engine)
    {
        run.see(engine.status(), t);
    }
    let seconds = started.elapsed().as_secs_f64();
    // Before the engine stops, so the recording ends with the check rather than with the run.
    let chain = recorder.stop();
    engine.stop();
    console::finished();
    if cancelled && seconds < MIN_REPORT_S {
        return Err(Failure::no_result("The check was cancelled before it had measured enough."));
    }
    if chain.blocks.is_empty() {
        return Err(Failure::error("The filter processed nothing during the check: no block reached it (see the notes above)."));
    }
    // The recording ends with the run it began with: after a restart (or a run that ended) the figures are that run's.
    let restarted = run.restarted || chain.run_ended;
    let figures = health::measure(&chain.blocks, &run.counters, &run.status, seconds, restarted);
    let verdicts = health::assess(&figures);
    say!("{}", health::format(&figures, &verdicts));
    Ok(())
}

/// The run the check began with, as the check saw it once a second: its streams (as it opened them) and its counters, held
/// once a restart is seen (fewer blocks or publications than before: the next run's count from 0), so the figures stay its.
struct FirstRun {
    status: EngineStatus,
    counters: TelemetrySnapshot,
    restarted: bool,
}

impl FirstRun {
    fn new(status: EngineStatus, counters: TelemetrySnapshot) -> Self {
        Self { status, counters, restarted: false }
    }

    /// One look at the engine: the run's counters move on until it restarts; its streams are kept as it opened them, a stream
    /// it hadn't reported yet filled in (a stopped engine reports none).
    fn see(&mut self, status: EngineStatus, t: TelemetrySnapshot) {
        if self.restarted || t.blocks < self.counters.blocks || t.publishes < self.counters.publishes {
            self.restarted = true;
            return;
        }
        self.counters = t;
        let EngineStatus { mic, output, reference, .. } = status;
        for (have, now) in [(&mut self.status.mic, mic), (&mut self.status.output, output), (&mut self.status.reference, reference)] {
            if have.is_none() {
                *have = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use nf_engine::StreamInfo;

    use super::*;

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

    /// The first run's streams and counters are kept: a later look fills in a stream it hadn't reported, a stopped engine's
    /// empty status changes nothing, and once a restart is seen (fewer blocks) the next run's counters and streams are ignored.
    #[test]
    fn the_check_keeps_its_first_runs_figures() {
        let opened = EngineStatus { mic: Some(stream("Mic")), output: Some(stream("Out")), ..Default::default() };
        let tel = |blocks, publishes, glitches| TelemetrySnapshot { blocks, publishes, capture_glitches: glitches, ..Default::default() };
        let mut run = FirstRun::new(opened.clone(), tel(10, 0, 0));
        run.see(EngineStatus { reference: Some(stream("Speakers")), ..opened.clone() }, tel(110, 1, 1));
        assert_eq!(run.status.reference.as_ref().map(|r| r.device_name.as_str()), Some("Speakers"));
        // Stopped (a fault): no streams, the counters as they were.
        run.see(EngineStatus::default(), tel(150, 1, 1));
        assert_eq!((run.status.mic.is_some(), run.counters.capture_glitches, run.restarted), (true, 1, false));
        // Restarted: its counters start again; the first run's stay, and so do its streams.
        run.see(EngineStatus { mic: Some(stream("Other mic")), ..Default::default() }, tel(20, 0, 0));
        run.see(opened, tel(300, 3, 5));
        assert!(run.restarted);
        assert_eq!((run.counters.blocks, run.counters.capture_glitches), (150, 1));
        assert_eq!(run.status.mic.as_ref().map(|m| m.device_name.as_str()), Some("Mic"));
        // A restart seen only by its publications (a first run shorter than the gap between looks).
        let mut short = FirstRun::new(EngineStatus::default(), tel(50, 2, 0));
        short.see(EngineStatus::default(), tel(90, 0, 0));
        assert!(short.restarted);
    }
}
