//! `run`: the engine without the window. It prints the streams once started, then a status line a
//! second, until Ctrl+C (or the console closing) stops it cleanly, or `--seconds` have passed (a soak). Then a summary of the
//! whole run: its dropouts, the output's events, the latency and its budget, the drift (a [`Tally`] of the status lines'
//! snapshots, and one more publication's after the loop, so the last moments count). A soak's exit code says whether it was
//! clean: nothing glitched and no real-time heap allocation (app.md, "Exit codes"); a run without `--seconds` exits 0. With
//! `--flight` the flight recorder is armed before the start (engine.md, "The flight recorder"): each dump it writes is noted
//! as it comes, and the summary lists them.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use nf_engine::{
    BudgetTotals, EngineConfig, EngineState, EngineStatus, FLIGHT_AUDIO_S, FlightConfig, FlightDump, FlightRecorder, FlightStats,
    LatencyBudget, OutputEvents, REFERENCE_TARGET_HIGH_MS, StreamInfo, TelemetrySnapshot, WakeBlocks,
};

use super::{budget_text, db, fixed, list_devices};
use crate::cli::{ADAPTIVE_WAIT_S, FlightArgs, RunArgs};
use crate::exit::Failure;
use crate::{config, console, logger};

pub fn run(args: &RunArgs) -> Result<(), Failure> {
    let cfg = config::engine_config(&args.engine, list_devices)?;
    let engine = nf_engine::Engine::new();
    // Armed before the start, so it holds the first run from its first block.
    let flight = args
        .flight
        .as_ref()
        .map(|f| arm_flight(&engine, f))
        .transpose()
        .map_err(|e| Failure::error(format!("Couldn't arm the flight recorder: {e}")))?;
    start_engine(&engine, &cfg)?;
    say!("{LEGEND}");
    if let Some(n) = args.seconds {
        say!("Running for {n} s (Ctrl+C stops it sooner), then a summary of the run.");
    }
    let first = engine.status();
    let (mut last_message, mut last_warnings, mut last_notes) = (first.message, first.warnings, first.notes);
    let mut ctrl_c = console::CtrlCWatch::default();
    let started = Instant::now();
    let limit = args.seconds.map(|n| Duration::from_secs(u64::from(n)));
    let mut tally = Tally::new(&engine.telemetry());
    let mut cut_short = false;
    let mut dumps_said = 0;
    loop {
        let wait = limit.map_or(Duration::from_secs(1), |l| l.saturating_sub(started.elapsed()).min(Duration::from_secs(1)));
        if console::sleep_unless_stopped(wait) {
            cut_short = limit.is_some_and(|l| started.elapsed() < l);
            break;
        }
        let status = engine.status();
        if status.message != last_message {
            if let Some(m) = &status.message {
                say!("note: {m}");
            }
            last_message = status.message;
        }
        if status.warnings != last_warnings {
            for line in changes("warning", &last_warnings, &status.warnings) {
                say!("{line}");
            }
            last_warnings = status.warnings;
        }
        if status.notes != last_notes {
            for line in changes("note", &last_notes, &status.notes) {
                say!("{line}");
            }
            last_notes = status.notes;
        }
        if let Some(w) = ctrl_c.check() {
            say!("{w}");
        }
        let t = engine.telemetry();
        // The clip latch is held until a line shows it (TODO 12).
        let clipped = engine.take_clip();
        tally.see(&t, clipped);
        say!("{}", status_line(&t, status.retrying, clipped));
        if let Some(f) = &flight {
            for d in f.recorder.stats().dumps.iter().skip(dumps_said) {
                say!("flight: {}", dump_line(d));
                dumps_said += 1;
            }
        }
        if limit.is_some_and(|l| started.elapsed() >= l) {
            break;
        }
    }
    // The counters are published once a second: one more publication brings the last moments' events in (a glitch in the
    // final second would be lost otherwise).
    if let Some(t) = next_publication(&engine) {
        tally.see(&t, engine.take_clip());
    }
    // Before the stop: the targets read 0 while stopped.
    let (end, status, fixed_target) = (engine.telemetry(), engine.status(), engine.buffer_targets().output_ms);
    engine.stop();
    say!("Stopped.");
    // After the stop: the triggers still in their post-rolls are dumped now, with what there is.
    let flight = flight.map(|f| (f.recorder.stop(), f.config));
    let summary = tally.summary(started.elapsed().as_secs_f64(), &end, &status, fixed_target);
    say!("{}", summary.text());
    if let Some((stats, cfg)) = &flight {
        say!("{}", flight_lines(stats, cfg));
    }
    console::finished();
    summary.result(cut_short, limit.is_some())
}

/// An armed flight recorder, and its settings (the folder it writes into, its limits).
pub(super) struct Flight {
    pub(super) recorder: FlightRecorder,
    pub(super) config: FlightConfig,
}

/// Arms the flight recorder on `engine` (before its start) with `--flight`'s folder (the log's `flight` folder by default)
/// and `--flight-audio`, and says so; else says why not (`run` stops, `devices --check` carries on without it).
pub(super) fn arm_flight(engine: &nf_engine::Engine, args: &FlightArgs) -> Result<Flight, nf_engine::EngineError> {
    let dir = args.dir.clone().unwrap_or_else(logger::flight_dir);
    let cfg = FlightConfig { audio_s: if args.audio { FLIGHT_AUDIO_S } else { 0.0 }, ..FlightConfig::new(&dir) };
    let recorder = engine.arm_flight_recorder(cfg.clone())?;
    let audio = if args.audio { format!(" and the last {} s of audio", fixed(cfg.audio_s, 0)) } else { String::new() };
    say!(
        "The flight recorder is on: it keeps the last {} s of every block's facts{audio}, and when something glitches writes \
         them into {} ({}; the newest {} kept there).",
        fixed(cfg.facts_s, 0),
        dir.display(),
        flight_limits(&cfg),
        cfg.keep_dumps
    );
    Ok(Flight { recorder, config: cfg })
}

/// The flight recorder's limits in words: "at most one dump every 60 s and 3 in all for each kind of trigger, 20 in all".
pub(super) fn flight_limits(cfg: &FlightConfig) -> String {
    format!(
        "at most one dump every {} s and {} in all for each kind of trigger, {} in all",
        fixed(cfg.min_interval_s, 0),
        cfg.max_dumps_per_kind,
        cfg.max_dumps
    )
}

/// One dump, in a line: when, what (and the other triggers it holds), where, and its reading.
pub(super) fn dump_line(d: &FlightDump) -> String {
    let also = if d.also.is_empty() { String::new() } else { format!(" (with {})", d.also.join(", ")) };
    format!("at {} s ({}), {}{also}: {} {}", fixed(d.at_s, 1), d.local_time, d.trigger, d.path.display(), d.verdict)
}

/// The triggers the flight recorder's limits held back, by kind: "102 more triggers not dumped (102 of the echo reference;
/// at most ...)".
pub(super) fn held_back(stats: &FlightStats, cfg: &FlightConfig) -> String {
    let kinds: Vec<String> = stats.suppressed_by.iter().map(|(k, n)| format!("{n} of {k}")).collect();
    format!("{} not dumped ({}; {})", count(stats.suppressed, "more trigger", "more triggers"), kinds.join(", "), flight_limits(cfg))
}

/// The summary's flight recorder lines: its dumps, what its limits held back, the blocks it lost, and write errors.
pub fn flight_lines(stats: &FlightStats, cfg: &FlightConfig) -> String {
    let mut s = String::new();
    if stats.dumps.is_empty() {
        let _ = write!(s, "  {:<15}nothing triggered in {} blocks", "flight", stats.blocks);
    } else {
        let n = stats.dumps.len();
        let _ = write!(s, "  {:<15}{} in {}:", "flight", count(n as u64, "dump", "dumps"), cfg.dir.display());
        for d in &stats.dumps {
            let _ = write!(s, "\n  {:<15}- {}", "", dump_line(d));
        }
    }
    if stats.suppressed > 0 {
        let _ = write!(s, "\n  {:<15}{}", "", held_back(stats, cfg));
    }
    if stats.lost_blocks > 0 {
        let _ = write!(s, "\n  {:<15}{} lost (its ring was full)", "", count(stats.lost_blocks, "block", "blocks"));
    }
    for e in &stats.errors {
        let _ = write!(s, "\n  {:<15}a dump wasn't written: {e}", "");
    }
    s
}

/// The longest wait for the engine's next publication (it comes once a second).
const PUBLICATION_WAIT: Duration = Duration::from_millis(1500);

/// Waits for the engine's next once-a-second publication (up to [`PUBLICATION_WAIT`]) and returns the telemetry then: its
/// counters include everything up to about now. `None` when none came (the engine isn't running).
pub(super) fn next_publication(engine: &nf_engine::Engine) -> Option<TelemetrySnapshot> {
    let from = engine.telemetry().publishes;
    let since = Instant::now();
    while since.elapsed() < PUBLICATION_WAIT {
        std::thread::sleep(Duration::from_millis(20));
        let t = engine.telemetry();
        if t.publishes != from {
            return Some(t);
        }
    }
    None
}

/// "1 restart", "2 restarts".
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Seconds into a run after which the reference's gaps and jumps count as more than its start-up's (it aligns and centres its
/// timeline in the first 2.5 s, and may raise its target there: engine.md, "The reference").
const STARTUP_S: u32 = 5;

/// The counters a run's summary adds up, as one snapshot has them (each run's since its start).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub capture_glitches: u64,
    pub stalls: u64,
    pub reference_gaps: u64,
    pub reference_jumps: u64,
    pub reference_slips: u64,
    pub output: OutputEvents,
    /// The wakes that processed 1, 2, 3 and 4 or more blocks (TODO 6, 13), and a model pack's worker's misses.
    pub wakes: WakeBlocks,
    pub model_misses: u64,
}

impl Counts {
    fn of(t: &TelemetrySnapshot) -> Self {
        Self {
            capture_glitches: t.capture_glitches,
            stalls: t.stalls,
            reference_gaps: t.reference_gaps,
            reference_jumps: t.reference_coarse_drops,
            reference_slips: t.reference_slips,
            output: OutputEvents::of(t),
            wakes: t.wake_blocks,
            model_misses: t.inference_misses,
        }
    }

    /// From `before` to `after`; after a restart (the block count went down) the new run's from its start.
    fn between(before: &TelemetrySnapshot, after: &TelemetrySnapshot) -> Self {
        let (b, a) = (Self::of(before), Self::of(after));
        if after.blocks < before.blocks {
            return a;
        }
        Self {
            capture_glitches: a.capture_glitches.saturating_sub(b.capture_glitches),
            stalls: a.stalls.saturating_sub(b.stalls),
            reference_gaps: a.reference_gaps.saturating_sub(b.reference_gaps),
            reference_jumps: a.reference_jumps.saturating_sub(b.reference_jumps),
            reference_slips: a.reference_slips.saturating_sub(b.reference_slips),
            output: OutputEvents::between(before, after),
            wakes: a.wakes.since(&b.wakes),
            model_misses: a.model_misses.saturating_sub(b.model_misses),
        }
    }

    fn add(&mut self, d: &Counts) {
        self.capture_glitches += d.capture_glitches;
        self.stalls += d.stalls;
        self.reference_gaps += d.reference_gaps;
        self.reference_jumps += d.reference_jumps;
        self.reference_slips += d.reference_slips;
        self.wakes.add(&d.wakes);
        self.model_misses += d.model_misses;
        let (o, e) = (&mut self.output, &d.output);
        o.underruns += e.underruns;
        o.deficits += e.deficits;
        o.zero_frames += e.zero_frames;
        o.coarse_drops += e.coarse_drops;
        o.dropped_frames += e.dropped_frames;
        o.slips += e.slips;
        o.slip_net += e.slip_net;
        o.reanchors += e.reanchors;
    }

    fn less(&self, earlier: &Counts) -> Counts {
        Counts {
            reference_gaps: self.reference_gaps.saturating_sub(earlier.reference_gaps),
            reference_jumps: self.reference_jumps.saturating_sub(earlier.reference_jumps),
            ..*self
        }
    }
}

/// A run's once-a-second snapshots added up (the status lines'), across the engine's own restarts (a fault and its retry, a
/// device change): the counters, the latency budget's totals, each second's readout median and p95, and the seconds it
/// wasn't running or the input clipped.
#[derive(Clone, Debug)]
pub struct Tally {
    last: TelemetrySnapshot,
    seconds: u32,
    /// Seconds the engine wasn't running (stopped by a fault, retrying), and the runs that started over.
    not_running: u32,
    restarts: u32,
    counts: Counts,
    /// The counts at [`STARTUP_S`], for the reference's after its start-up.
    at_startup: Option<Counts>,
    budget: BudgetTotals,
    /// Each second's readout median and p95, and its budget's p95s (while the readout is valid), with the second's number.
    medians: Vec<f32>,
    p95s: Vec<(u32, f32)>,
    budget_p95s: Vec<LatencyBudget>,
    clipped: u32,
    /// The most real-time heap allocations a run published (they must stay 0).
    rt_heap: u64,
}

/// What a run comes to ([`Tally::summary`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub seconds: f64,
    pub running_s: u32,
    pub restarts: u32,
    pub counts: Counts,
    /// The reference's gaps and jumps after its start-up's [`STARTUP_S`] (`None` for a shorter run).
    pub reference_after_startup: Option<(u64, u64)>,
    pub budget: Option<LatencyBudget>,
    /// The seconds' readout medians (low, median, high), their p95s (median, worst and its second), and the median second's
    /// budget p95s.
    pub readout_medians: Option<(f32, f32, f32)>,
    pub readout_p95: Option<(f32, f32, u32)>,
    pub budget_p95: Option<LatencyBudget>,
    pub output_drift_ppm: f32,
    pub reference_drift_ppm: f32,
    pub reference: Option<(f32, f32, bool)>,
    pub target: String,
    pub clipped_s: u32,
    pub seen_s: u32,
    pub rt_heap: u64,
    /// The streams' devices, from the status.
    pub mic: String,
    pub output: String,
    /// A model pack's inference worker ran (at the end).
    pub model: bool,
}

impl Tally {
    /// From the snapshot before the first status line (just after the start).
    pub fn new(first: &TelemetrySnapshot) -> Self {
        Self {
            last: *first,
            seconds: 0,
            not_running: 0,
            restarts: 0,
            counts: Counts::default(),
            at_startup: None,
            budget: BudgetTotals::default(),
            medians: Vec::new(),
            p95s: Vec::new(),
            budget_p95s: Vec::new(),
            clipped: 0,
            rt_heap: 0,
        }
    }

    /// One second's snapshot, and whether the input clipped since the last (the clip latch, taken).
    pub fn see(&mut self, t: &TelemetrySnapshot, clipped: bool) {
        self.seconds += 1;
        let restarted = t.blocks < self.last.blocks;
        if restarted {
            self.restarts += 1;
        }
        self.counts.add(&Counts::between(&self.last, t));
        let base = if restarted { BudgetTotals::default() } else { self.last.budget_totals };
        let b = &t.budget_totals;
        if b.blocks > base.blocks {
            self.budget.blocks += b.blocks - base.blocks;
            self.budget.mic_ms += b.mic_ms - base.mic_ms;
            self.budget.wait_ms += b.wait_ms - base.wait_ms;
            self.budget.chain_ms += b.chain_ms - base.chain_ms;
            self.budget.queue_ms += b.queue_ms - base.queue_ms;
            self.budget.render_ms += b.render_ms - base.render_ms;
        }
        if t.state != EngineState::Running {
            self.not_running += 1;
        } else if t.latency_valid {
            self.medians.push(t.latency_ms_median);
            self.p95s.push((self.seconds, t.latency_ms_p95));
            self.budget_p95s.push(t.budget_p95);
        }
        if self.seconds == STARTUP_S {
            self.at_startup = Some(self.counts);
        }
        self.clipped += u32::from(clipped);
        self.rt_heap = self.rt_heap.max(t.rt_heap_allocations);
        self.last = *t;
    }

    /// The run's summary: `seconds` it ran, its last snapshot `end` (before the stop), its status, and the output's fixed
    /// target (`BufferTargets::output_ms`).
    pub fn summary(&self, seconds: f64, end: &TelemetrySnapshot, status: &EngineStatus, fixed_target_ms: f32) -> Summary {
        let mut medians = self.medians.clone();
        medians.sort_by(f32::total_cmp);
        let mid = |v: &[f32]| v.get(v.len() / 2).copied();
        let readout_medians = match (medians.first(), mid(&medians), medians.last()) {
            (Some(&lo), Some(m), Some(&hi)) => Some((lo, m, hi)),
            _ => None,
        };
        let mut p95: Vec<f32> = self.p95s.iter().map(|p| p.1).collect();
        p95.sort_by(f32::total_cmp);
        let worst = self.p95s.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1));
        let readout_p95 = mid(&p95).zip(worst).map(|(m, (at, w))| (m, w, at));
        // Each part's median second.
        let part = |f: fn(&LatencyBudget) -> f32| {
            let mut v: Vec<f32> = self.budget_p95s.iter().map(f).collect();
            v.sort_by(f32::total_cmp);
            mid(&v).unwrap_or(f32::NAN)
        };
        let budget_p95 = (!self.budget_p95s.is_empty()).then(|| LatencyBudget {
            mic_ms: part(|b| b.mic_ms),
            wait_ms: part(|b| b.wait_ms),
            chain_ms: part(|b| b.chain_ms),
            queue_ms: part(|b| b.queue_ms),
            render_ms: part(|b| b.render_ms),
        });
        let device = |s: Option<&StreamInfo>| s.map_or_else(|| "--".to_owned(), |s| s.device_name.clone());
        Summary {
            seconds,
            running_s: self.seconds - self.not_running,
            restarts: self.restarts,
            counts: self.counts,
            reference_after_startup: self.at_startup.map(|s| {
                let after = self.counts.less(&s);
                (after.reference_gaps, after.reference_jumps)
            }),
            budget: self.budget.means_since(&BudgetTotals::default()),
            readout_medians,
            readout_p95,
            budget_p95,
            output_drift_ppm: end.output_drift_ppm,
            reference_drift_ppm: end.reference_drift_ppm,
            reference: (end.reference_target_ms > 0.0).then_some((end.reference_fill_ms, end.reference_target_ms, end.reference_stamped)),
            target: nf_engine::output_target_text(end.output_target_ms, end.output_adaptive, fixed_target_ms),
            clipped_s: self.clipped,
            seen_s: self.seconds,
            rt_heap: self.rt_heap,
            mic: device(status.mic.as_ref()),
            output: device(status.output.as_ref()),
            model: end.inference_active,
        }
    }
}

impl Summary {
    /// What glitched, as phrases: mic glitches and stalls, output underruns and coarse drops, and a fault that stopped the
    /// filter (seconds not running, or a restart). Empty for a clean run. Reference gaps and jumps aren't here: the start-up
    /// has a few by design, and they touch the echo canceller's reference, not the audio.
    pub fn glitches(&self) -> Vec<String> {
        let c = &self.counts;
        let mut g = Vec::new();
        let mut add = |n: u64, one: &str, many: &str| {
            if n > 0 {
                g.push(count(n, one, many));
            }
        };
        add(c.capture_glitches, "mic glitch", "mic glitches");
        add(c.stalls, "mic stall", "mic stalls");
        add(c.output.underruns, "output underrun", "output underruns");
        add(c.output.coarse_drops, "output coarse drop", "output coarse drops");
        add(u64::from(self.seen_s - self.running_s), "s not running (a fault)", "s not running (a fault)");
        add(u64::from(self.restarts), "restart", "restarts");
        g
    }

    /// What wasn't clean: what glitched, and real-time heap allocations (the real-time rule broken: they must stay 0).
    pub fn problems(&self) -> Vec<String> {
        let mut p = self.glitches();
        if self.rt_heap > 0 {
            p.push(count(self.rt_heap, "real-time heap allocation", "real-time heap allocations"));
        }
        p
    }

    /// The exit. A soak (`soak`: `--seconds`): 6 when it wasn't clean (even when Ctrl+C cut it short), else 3 when Ctrl+C cut it
    /// short, else OK. A run without `--seconds` is someone using the filter, not a test: OK whatever its summary says.
    pub fn result(&self, cut_short: bool, soak: bool) -> Result<(), Failure> {
        if !soak {
            return Ok(());
        }
        let p = self.problems();
        if !p.is_empty() {
            return Err(Failure::glitched(format!("The soak wasn't clean: {} (the summary above).", p.join(", "))));
        }
        if cut_short {
            return Err(Failure::no_result(format!(
                "Stopped after {} s, before --seconds ran out: no glitch until then.",
                fixed(self.seconds, 0)
            )));
        }
        Ok(())
    }

    /// The summary's lines.
    pub fn text(&self) -> String {
        let ms = |v: f32| fixed(f64::from(v), 1);
        let c = &self.counts;
        let mut s =
            format!("Summary: {} s, {} of them running; mic {}, output {}", fixed(self.seconds, 0), self.running_s, self.mic, self.output);
        if self.restarts > 0 {
            let _ = write!(s, "; the filter restarted {} times", self.restarts);
        }
        let after = self
            .reference_after_startup
            .map_or(String::new(), |(gaps, jumps)| format!(" ({gaps} and {jumps} after the first {STARTUP_S} s)"));
        let _ = write!(
            s,
            "
  {:<15}mic glitches {}, mic stalls {}, output underruns {}; reference gaps {} and jumps {}{after}",
            "dropouts", c.capture_glitches, c.stalls, c.output.underruns, c.reference_gaps, c.reference_jumps
        );
        let _ = write!(
            s,
            "
  {:<15}{}",
            "output events",
            c.output.text()
        );
        if let Some((lo, m, hi)) = self.readout_medians {
            let _ = write!(
                s,
                "
  {:<15}readout {} ms (the seconds' medians {} to {})",
                "latency",
                ms(m),
                ms(lo),
                ms(hi)
            );
            if let Some((p, worst, at)) = self.readout_p95 {
                let _ = write!(s, ", p95 {} ms (the worst second's {}, at {at} s)", ms(p), ms(worst));
            }
        } else {
            let _ = write!(
                s,
                "
  {:<15}no readout (off, or no clock yet)",
                "latency"
            );
        }
        if let Some(b) = &self.budget {
            let _ = write!(
                s,
                "
  {:<15}{}, means over the run",
                "budget",
                budget_text(b)
            );
        }
        if let Some(b) = &self.budget_p95 {
            let _ = write!(
                s,
                "
  {:<15}mic delivery {}, wait {}, output queue {}, render side {} ms (the median second's)",
                "budget p95s",
                ms(b.mic_ms),
                ms(b.wait_ms),
                ms(b.queue_ms),
                ms(b.render_ms)
            );
        }
        let ppm = |v: f32| if v.is_finite() { format!("{v:+.1}") } else { "--".into() };
        let _ = write!(
            s,
            "
  {:<15}output {} ppm",
            "drift",
            ppm(self.output_drift_ppm)
        );
        if let Some((lead, target, stamped)) = self.reference {
            let _ = write!(
                s,
                ", reference {} ppm; the reference's lead {} ms{} (target {}), {} slips",
                ppm(self.reference_drift_ppm),
                ms(lead),
                if stamped { "" } else { " by delivery" },
                ms(target),
                c.reference_slips
            );
        }
        if !self.target.is_empty() {
            let _ = write!(
                s,
                "
  {:<15}{}",
                "output", self.target
            );
        }
        let _ = write!(
            s,
            "
  {:<15}clipped in {} of {} s; real-time heap allocations {}",
            "input", self.clipped_s, self.seen_s, self.rt_heap
        );
        let w = &c.wakes;
        let model = if self.model { format!("; the model missed {}", c.model_misses) } else { String::new() };
        let _ = write!(
            s,
            "
  {:<15}{} with 1 block, {} with 2, {} with 3, {} with 4 or more; a model pack's worker would miss about {} blocks at \
             --model-depth 1, {} at 2, {} at 3{model}",
            "wakes",
            w.0[0],
            w.0[1],
            w.0[2],
            w.0[3],
            w.misses_at(1),
            w.misses_at(2),
            w.misses_at(3)
        );
        let p = self.problems();
        let result = if p.is_empty() {
            format!("clean: no glitch and no real-time heap allocation in {} s", fixed(self.seconds, 0))
        } else {
            format!("not clean: {}", p.join(", "))
        };
        let _ = write!(
            s,
            "
  {:<15}{result}",
            "result"
        );
        s
    }
}

/// Before `latency` or `quality` measures: with the adaptive output target (the default) it measures the delivery at the
/// fixed target for a run's first two minutes before it can come down, holds there for two more to confirm it (a trial),
/// and then walks down to where it's going, a tenth of the way a second (`nf_engine::ADAPTIVE_OUTPUT_SETTLE_S` in all).
/// With `settle` (`--settle`) this waits for it to settle (up to [`ADAPTIVE_WAIT_S`]), so the test measures where it
/// settled; without, it says the test measures at the fixed start, and returns at once (the owner, 2026-11-02: the lab
/// commands measure what the product does by default, and needn't wait three minutes for it).
/// Returns true when Ctrl+C cancelled the wait.
pub fn wait_for_adaptive_target(engine: &nf_engine::Engine, cfg: &EngineConfig, settle: bool) -> bool {
    let from = engine.telemetry().output_target_ms;
    match target_wait(cfg, settle) {
        TargetWait::Fixed => return false,
        TargetWait::NothingToSettle => {
            say!("--settle: the output target is fixed (the config turns the adaptive target off), so there's nothing to wait for.");
            return false;
        }
        TargetWait::AtFixedStart => {
            say!("{}", at_fixed_start(from));
            return false;
        }
        TargetWait::Settle => {}
    }
    say!(
        "The adaptive output target measures the delivery for {} s before it can come down, holds for another {} s to \
         confirm it, and then walks down: waiting for it to settle (up to {ADAPTIVE_WAIT_S} s; Ctrl+C cancels).",
        nf_engine::ADAPTIVE_OUTPUT_WINDOW_S,
        nf_engine::ADAPTIVE_OUTPUT_WINDOW_S
    );
    let start = Instant::now();
    let mut watch = TargetWatch::new(from);
    while start.elapsed() < Duration::from_secs(ADAPTIVE_WAIT_S) {
        if console::sleep_unless_stopped(Duration::from_secs(1)) {
            return true;
        }
        let t = engine.telemetry();
        if let Some(now) = watch.see(t.output_target_ms, t.output_target_on_trial) {
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

/// What a lab test does about the output target before it measures ([`target_wait`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetWait {
    /// Fixed (`--no-adaptive-output`, or a config): nothing to say or wait for.
    Fixed,
    /// Fixed, and `--settle` given: say there's nothing to wait for.
    NothingToSettle,
    /// Adaptive, no `--settle`: measure at once, at the target's fixed start, and say so.
    AtFixedStart,
    /// Adaptive with `--settle`: wait for it to come down (up to [`ADAPTIVE_WAIT_S`]).
    Settle,
}

impl TargetWait {
    /// Whether the test waits for the target (the chain recorder's limit allows for the wait).
    pub fn waits(self) -> bool {
        self == TargetWait::Settle
    }
}

/// What `latency` or `quality` does about the output target with `cfg` and `--settle`.
pub fn target_wait(cfg: &EngineConfig, settle: bool) -> TargetWait {
    match (cfg.io.adaptive_output, settle) {
        (false, false) => TargetWait::Fixed,
        (false, true) => TargetWait::NothingToSettle,
        (true, false) => TargetWait::AtFixedStart,
        (true, true) => TargetWait::Settle,
    }
}

/// What a test that doesn't wait for the adaptive output target measures: the target at its fixed start, `ms`.
fn at_fixed_start(ms: f32) -> String {
    format!(
        "The adaptive output target is at its fixed start ({} ms) and can come down only after {} s of running: measuring now, \
         at the fixed start. --settle waits for it first (up to {ADAPTIVE_WAIT_S} s).",
        fixed(f64::from(ms), 1),
        nf_engine::ADAPTIVE_OUTPUT_FIRST_S
    )
}

/// Seconds the adaptive target must hold still, after it has moved, to count as settled: its walk moves it every second
/// until it's there.
const TARGET_STILL_S: u32 = 3;

/// Watches the published output target, a reading a second: settled once it has moved and then held still for
/// [`TARGET_STILL_S`], but not while it holds for a trial (`on_trial`: its lowering is still to come; after a strike's raise,
/// the next lowering is trialled again).
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

    /// This second's target (ms) and whether it is on trial; the target once it has settled.
    fn see(&mut self, ms: f32, on_trial: bool) -> Option<f32> {
        if ms.is_nan() || ms <= 0.0 {
            // Stopped, or restarting: nothing to watch.
            return None;
        }
        if (ms - self.last).abs() > 1e-4 {
            (self.last, self.moved, self.still) = (ms, true, 0);
            return None;
        }
        self.still += 1;
        (self.moved && !on_trial && self.still >= TARGET_STILL_S).then_some(ms)
    }
}

/// What changed in the engine's standing warnings or notes (`kind`: "warning" or "note"): each new one, then each one that
/// has cleared.
pub fn changes(kind: &str, old: &[String], new: &[String]) -> Vec<String> {
    let added = new.iter().filter(|w| !old.contains(w)).map(|w| format!("{kind}: {w}"));
    let cleared = old.iter().filter(|w| !new.contains(w)).map(|w| format!("cleared: {w}"));
    added.chain(cleared).collect()
}

/// Starts `engine` with `cfg` (Ctrl+C armed first) and prints what it opened: the caller makes the engine, so it can arm the
/// chain recorder first.
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
pub const LEGEND: &str = "Each second: input and output peak/RMS dBFS (the input before any processing; CLIP: it clipped \
     since the last line, near-clip: within 0.1 dB of full scale); latency median/p95 (mic capture to cable play-out), \
     then its budget's means in ms [mic delivery+wait+output queue+render side] (the chain's offset, given at the start, \
     isn't in it), and with the adaptive output target (unless --no-adaptive-output) the output queue's target now; CPU \
     p99 per block; drift output/reference; with an echo reference, its lead/target in ms (how long before the mic block \
     it was tapped; 'by delivery' without trusted stamps, 'high' over 30 ms, which a speaker's echo path may not cover); \
     dropouts mic/output/reference, then, once there are any, mic stalls (100 ms with no packet) and coarse corrections, \
     output/reference (whole blocks dropped to catch up; on the reference, jumps of its timeline); 'excl', then the streams \
     in WASAPI exclusive mode (out, mic); with a model pack (--model), its inference worker's misses (blocks it was late \
     for, which played the delayed input instead), its run time's p99 and the wakes that brought more than one block (each \
     block past --model-depth in one is a miss); real-time heap allocations (pool allocations per second). Ctrl+C stops.";

/// One line of live telemetry. `retrying`: stopped by a fault, with the engine set to restart by
/// itself (`EngineStatus::retrying`). `clipped`: the input clipped since the last line (the clip latch, taken: TODO 12).
pub fn status_line(t: &TelemetrySnapshot, retrying: bool, clipped: bool) -> String {
    let state = match t.state {
        EngineState::Running => "running",
        EngineState::Starting => "starting",
        EngineState::Stopped | EngineState::Error if retrying => "retrying",
        EngineState::Stopped => "stopped",
        EngineState::Error => "error",
    };
    let clip = if clipped {
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
    // A model pack's inference worker (TODO 6): its misses (blocks it was too late for) and its run time.
    let model = if t.inference_active {
        format!(
            "  model misses {} run {} µs bursts {}",
            t.inference_misses,
            fixed(f64::from(t.inference_run_us_p99), 0),
            t.wake_blocks.bursts()
        )
    } else {
        String::new()
    };
    let mut reference = String::new();
    if t.reference_target_ms > 0.0 {
        reference = format!("  ref {}/{} ms", fixed(f64::from(t.reference_fill_ms), 1), fixed(f64::from(t.reference_target_ms), 1));
        if !t.reference_stamped {
            reference.push_str(" by delivery");
        }
        if t.reference_target_ms > REFERENCE_TARGET_HIGH_MS {
            reference.push_str(" high");
        }
        // The echo-path guard's mic hold: the lead and target above are the echo path's, less it.
        if t.echo_hold_ms > 0.0 {
            let trial = if t.echo_guard == nf_engine::ECHO_GUARD_TRIAL { " trial" } else { "" };
            let _ = write!(reference, " held {}{trial}", fixed(f64::from(t.echo_hold_ms), 1));
        }
    }
    format!(
        "{state:<8} in {}/{} dBFS{clip}  out {}/{}  lat {latency}{target}  cpu {} µs  drift {}/{} ppm{reference}  \
         drops {}/{}/{}{rare}{modes}{model}  rt {} ({}/s)",
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
    let mode = |exclusive: bool| if exclusive { "exclusive" } else { "shared" };
    let latency = format!(
        "; {} output, {} mic, {} output target",
        mode(io.output_exclusive),
        mode(io.mic_exclusive),
        if io.adaptive_output { "adaptive" } else { "fixed" }
    );
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
    // A model pack's worker depth, and why (TODO 5: automatic by default).
    let depth = s.model_depth.map_or(String::new(), |d| format!("; model {}", d.text()));
    lines.push(format!(
        "{:<10} {}; MMCSS {}; chain {} ms{depth}",
        "Engine",
        if s.memory.is_empty() { "memory unknown" } else { &s.memory },
        if s.mmcss { "Pro Audio" } else { "unavailable" },
        fixed(f64::from(s.chain_latency_ms), 2)
    ));
    if let Some(m) = &s.message {
        lines.push(format!("note: {m}"));
    }
    lines.extend(changes("warning", &[], &s.warnings));
    lines.extend(changes("note", &[], &s.notes));
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
    /// The summary's flight recorder lines: each dump (when, what, where, its reading), what the rate limit held back, lost
    /// blocks and write errors; or that nothing triggered.
    #[test]
    fn the_summary_lists_the_flight_recorders_dumps() {
        use std::path::Path;

        use super::{FlightConfig, FlightDump, FlightStats, flight_lines};
        let dir = Path::new("C:\\flight");
        let cfg = FlightConfig::new(dir);
        let quiet = flight_lines(&FlightStats { blocks: 180_000, ..Default::default() }, &cfg);
        assert_eq!(quiet, "  flight         nothing triggered in 180000 blocks");
        let dump = FlightDump {
            path: dir.join("flight-20261102-140312-output-underrun"),
            trigger: "an output underrun".into(),
            also: vec!["output-deficit", "late-wake"],
            at_s: 110.04,
            local_time: "2026-11-02 14:03:12.345".into(),
            verdict: "The audio thread was held up.".into(),
        };
        let stats = FlightStats {
            dumps: vec![dump],
            suppressed: 3,
            suppressed_by: vec![("the output", 1), ("the echo reference", 2)],
            lost_blocks: 1,
            errors: vec!["disk full".into()],
            blocks: 9,
        };
        let lines: Vec<String> = flight_lines(&stats, &cfg).lines().map(str::to_owned).collect();
        assert_eq!(lines, [
            "  flight         1 dump in C:\\flight:",
            "                 - at 110.0 s (2026-11-02 14:03:12.345), an output underrun (with output-deficit, late-wake): \
                 C:\\flight\\flight-20261102-140312-output-underrun The audio thread was held up.",
            "                 3 more triggers not dumped (1 of the output, 2 of the echo reference; at most one dump every 60 s \
                 and 3 in all for each kind of trigger, 20 in all)",
            "                 1 block lost (its ring was full)",
            "                 a dump wasn't written: disk full",
        ]);
    }

    /// The lab tests measure what the product does by default, at once: the adaptive target at its fixed start, which they
    /// say; `--settle` waits for it (and only then does the chain recorder allow for the wait); a fixed target has nothing
    /// to wait for.
    #[test]
    fn the_lab_tests_wait_for_the_adaptive_target_only_with_settle() {
        use super::{TargetWait, target_wait};
        let mut cfg = super::EngineConfig::default();
        assert_eq!(target_wait(&cfg, false), TargetWait::AtFixedStart, "the default: adaptive, measured at once");
        assert_eq!(target_wait(&cfg, true), TargetWait::Settle);
        cfg.io.adaptive_output = false;
        assert_eq!(target_wait(&cfg, false), TargetWait::Fixed);
        assert_eq!(target_wait(&cfg, true), TargetWait::NothingToSettle);
        let waits: Vec<bool> = [TargetWait::Fixed, TargetWait::NothingToSettle, TargetWait::AtFixedStart, TargetWait::Settle]
            .iter()
            .map(|w| w.waits())
            .collect();
        assert_eq!(waits, [false, false, false, true]);
        assert_eq!(
            super::at_fixed_start(14.0),
            "The adaptive output target is at its fixed start (14.0 ms) and can come down only after 240 s of running: measuring \
             now, at the fixed start. --settle waits for it first (up to 308 s)."
        );
    }

    /// The adaptive target's wait: nothing until it has moved, then settled once it holds still for 3 s; a walk's steps
    /// (every second) keep it waiting, and so does a trial (its lowering is still to come); a stopped engine's 0 doesn't
    /// count.
    #[test]
    fn the_adaptive_wait_settles_after_the_walk() {
        // The run's trial holds the target at the fixed one: not moved, not settled; then the walk, and 3 s still.
        let mut w = super::TargetWatch::new(22.0);
        let trial = [false, true, true, true, true, true, false, false, false, false, false, false, false];
        let at = [22.0, 22.0, 22.0, 22.0, 0.0, 22.0, 21.5, 21.0, 20.6, 20.6, 20.6, 20.6, 20.6];
        let seen: Vec<Option<f32>> = at.iter().zip(trial).map(|(&t, on)| w.see(t, on)).collect();
        assert_eq!(seen[..11], [None; 11], "the trial, then the walk");
        assert_eq!(seen[11..], [Some(20.6), Some(20.6)]);
        // A target that has moved and is on trial (as after a strike's raise) isn't settled either.
        let mut w = super::TargetWatch::new(22.0);
        let trial = [false, false, false, true, true, true, true, true, true, true, true, true, false, false, false, false];
        let at = [22.0, 22.0, 22.0, 21.5, 21.0, 20.6, 20.6, 20.6, 20.6, 20.6, 0.0, 20.6, 20.0, 20.0, 20.0, 20.0];
        let seen: Vec<Option<f32>> = at.iter().zip(trial).map(|(&t, on)| w.see(t, on)).collect();
        assert_eq!(seen[..14], [None; 14], "held on trial for longer than 3 s isn't the end");
        assert_eq!(seen[14..], [None, Some(20.0)]);
        let mut w = super::TargetWatch::new(22.0);
        let seen: Vec<Option<f32>> =
            [22.0, 22.0, 22.0, 21.5, 21.0, 20.6, 0.0, 20.6, 20.6, 20.6, 20.6].iter().map(|&t| w.see(t, false)).collect();
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
            status_line(&t, false, true),
            "running  in -12.3/-30.0 dBFS CLIP  out -inf/--  lat 21.3/23.0 ms [10.7+0.0+6.0+5.0]  cpu 131 µs  drift +12/-3 ppm  \
             drops 1/2/3  rt 0 (4300/s)"
        );
        // The latch shows on the line that took it (a clip since the last line), not on every line after.
        assert!(status_line(&t, false, false).starts_with("running  in -12.3/-30.0 dBFS  out"), "{}", status_line(&t, false, false));
        assert!(LEGEND.contains("[mic delivery+wait+output queue+render side]") && LEGEND.contains("CLIP: it clipped since the last line"));
        let idle = TelemetrySnapshot { state: EngineState::Error, near_clip: true, output_drift_ppm: f32::NAN, ..Default::default() };
        let line = status_line(&idle, false, false);
        assert!(line.starts_with("error    in 0.0/0.0 dBFS near-clip"), "{line}");
        assert!(line.contains("lat --") && line.contains("drift --/0 ppm"), "{line}");
        // A fault the engine is retrying by itself says so
        assert!(status_line(&idle, true, false).starts_with("retrying in 0.0/0.0 dBFS"), "{}", status_line(&idle, true, false));
        let stopped = TelemetrySnapshot { state: EngineState::Stopped, ..Default::default() };
        assert!(status_line(&stopped, true, false).starts_with("retrying "));
        assert!(status_line(&stopped, false, false).starts_with("stopped "));
        assert!(status_line(&t, true, false).starts_with("running "), "a running engine isn't retrying");
    }

    /// The new counters appear once they count something: mic stalls, and coarse drops on the
    /// output and the reference.
    #[test]
    fn stalls_and_coarse_drops_show_once_there_are_any() {
        let quiet = TelemetrySnapshot { state: EngineState::Running, capture_glitches: 1, ..Default::default() };
        let line = status_line(&quiet, false, false);
        assert!(line.contains("drops 1/0/0  rt 0") && !line.contains("stalls") && !line.contains("coarse"), "{line}");
        let t = TelemetrySnapshot { stalls: 2, ..quiet };
        assert!(status_line(&t, false, false).contains("drops 1/0/0 stalls 2  rt 0"), "{}", status_line(&t, false, false));
        let t = TelemetrySnapshot { reference_coarse_drops: 4, ..quiet };
        assert!(status_line(&t, false, false).contains("drops 1/0/0 coarse 0/4  rt 0"), "{}", status_line(&t, false, false));
        let t = TelemetrySnapshot { stalls: 1, output_coarse_drops: 3, reference_coarse_drops: 5, ..quiet };
        assert!(status_line(&t, false, false).contains("drops 1/0/0 stalls 1 coarse 3/5  rt 0"), "{}", status_line(&t, false, false));
        // A model pack on its inference worker (TODO 5, 6): its misses and run time, before the allocations.
        let wakes = nf_engine::WakeBlocks([500, 7, 2, 1]);
        let t =
            TelemetrySnapshot { inference_active: true, inference_misses: 3, inference_run_us_p99: 1104.4, wake_blocks: wakes, ..quiet };
        assert!(
            status_line(&t, false, false).contains("drops 1/0/0  model misses 3 run 1104 µs bursts 10  rt 0"),
            "{}",
            status_line(&t, false, false)
        );
        assert!(!status_line(&quiet, false, false).contains("model"), "RNNoise has no worker");
        assert!(LEGEND.contains("its inference worker's misses") && !LEGEND.contains("  "), "{LEGEND}");
        assert!(LEGEND.contains("stalls") && LEGEND.contains("coarse"));
    }

    /// With a reference, its lead and target follow the drift, and say when the delivery times it or the target is high.
    #[test]
    fn the_reference_lead_shows_with_a_reference() {
        let quiet = TelemetrySnapshot { state: EngineState::Running, ..Default::default() };
        assert!(!status_line(&quiet, false, false).contains("ref "), "{}", status_line(&quiet, false, false));
        let t = TelemetrySnapshot { reference_fill_ms: 23.36, reference_target_ms: 23.4, reference_stamped: true, ..quiet };
        assert!(
            status_line(&t, false, false).contains("drift 0/0 ppm  ref 23.4/23.4 ms  drops 0/0/0"),
            "{}",
            status_line(&t, false, false)
        );
        let t = TelemetrySnapshot { reference_target_ms: 31.0, reference_stamped: false, ..t };
        assert!(status_line(&t, false, false).contains("ref 23.4/31.0 ms by delivery high  drops"), "{}", status_line(&t, false, false));
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
        let line = status_line(&t, false, false);
        assert!(line.contains("lat 18.3/19.0 ms [10.3+0.0+5.0+3.0] target 5.0  cpu"), "{line}");
        assert!(line.contains("drops 0/0/0  excl out+mic  rt 0"), "{line}");
        let shared = TelemetrySnapshot { output_adaptive: false, output_exclusive: false, ..t };
        let line = status_line(&shared, false, false);
        assert!(line.contains("[10.3+0.0+5.0+3.0]  cpu") && line.contains("drops 0/0/0  excl mic  rt"), "{line}");
        assert!(!status_line(&TelemetrySnapshot { mic_exclusive: false, ..shared }, false, false).contains("excl"));
        assert!(LEGEND.contains("--no-adaptive-output") && LEGEND.contains("'excl'"));
    }

    /// One second of a running engine: `s` seconds in (100 blocks a second), its budget's totals so far (1 + 6 + 6 + 4 ms a
    /// block) and its readout, as `f` changes them.
    fn second(s: u64, f: impl Fn(&mut TelemetrySnapshot)) -> TelemetrySnapshot {
        let n = 100 * s;
        let mut t = TelemetrySnapshot {
            state: EngineState::Running,
            blocks: n,
            latency_valid: s > 0,
            latency_ms_median: 17.0,
            latency_ms_p95: 18.0,
            budget_p95: nf_engine::LatencyBudget { mic_ms: 1.5, wait_ms: 6.5, chain_ms: 10.0, queue_ms: 7.0, render_ms: 4.5 },
            budget_totals: BudgetTotals {
                blocks: n,
                mic_ms: n as f64,
                wait_ms: 6.0 * n as f64,
                chain_ms: 10.0 * n as f64,
                queue_ms: 6.0 * n as f64,
                render_ms: 4.0 * n as f64,
            },
            output_slips: s,
            output_slip_net: -(s as i64),
            reference_target_ms: 18.0,
            reference_fill_ms: 18.1,
            reference_stamped: true,
            output_target_ms: 14.0,
            output_adaptive: true,
            ..Default::default()
        };
        f(&mut t);
        t
    }

    /// A soak's tally: the counters summed over its seconds, the budget's means over the run, the seconds' readout medians and
    /// p95s (the worst second named), the reference's gaps after its start-up, the clipped seconds; and its exit: clean, or
    /// cut short by Ctrl+C (3).
    #[test]
    fn a_run_tallies_its_seconds_into_a_summary() {
        let mut tally = Tally::new(&second(0, |_| {}));
        for s in 1..=10u64 {
            tally.see(
                &second(s, |t| {
                    // Two gaps in the start-up, one more at 8 s; one bad second's p95.
                    t.reference_gaps = if s >= 8 { 3 } else { 2 };
                    if s == 7 {
                        t.latency_ms_p95 = 25.0;
                    }
                }),
                s == 3,
            );
        }
        let end = second(10, |t| t.reference_gaps = 3);
        let status = EngineStatus { output: Some(stream("CABLE Input")), ..Default::default() };
        let sum = tally.summary(10.02, &end, &status, 14.0);
        assert_eq!((sum.counts.output.slips, sum.counts.output.slip_net, sum.counts.reference_gaps), (10, -10, 3));
        assert_eq!(sum.reference_after_startup, Some((1, 0)), "one gap after the first 5 s");
        assert_eq!((sum.readout_medians, sum.readout_p95), (Some((17.0, 17.0, 17.0)), Some((18.0, 25.0, 7))));
        let b = sum.budget.unwrap();
        assert!((b.mic_ms - 1.0).abs() < 1e-6 && (b.readout_ms() - 17.0).abs() < 1e-4, "{b:?}");
        assert_eq!((sum.clipped_s, sum.seen_s, sum.running_s, sum.restarts), (1, 10, 10, 0));
        assert!(sum.glitches().is_empty() && sum.result(false, true).is_ok());
        assert_eq!(sum.result(true, true).unwrap_err().exit, crate::exit::Exit::NoResult, "a soak cut short");
        assert!(sum.result(true, false).is_ok(), "a run without --seconds stopped with Ctrl+C");
        let text = sum.text();
        for part in [
            "Summary: 10 s, 10 of them running; mic --, output CABLE Input",
            "dropouts       mic glitches 0, mic stalls 0, output underruns 0; reference gaps 3 and jumps 0 (1 and 0 after the first 5 s)",
            "output events  no underruns, zeros, drops or re-anchors; 10 slips (net -10 samples)",
            "latency        readout 17.0 ms (the seconds' medians 17.0 to 17.0), p95 18.0 ms (the worst second's 25.0, at 7 s)",
            "budget         mic delivery 1.0 + wait 6.0 + output queue 6.0 + render side 4.0 = readout 17.0 ms",
            "budget p95s    mic delivery 1.5, wait 6.5, output queue 7.0, render side 4.5 ms (the median second's)",
            "drift          output +0.0 ppm, reference +0.0 ppm; the reference's lead 18.1 ms (target 18.0), 0 slips",
            "output         output target 14.0 ms at the end (adaptive, at the fixed target",
            "input          clipped in 1 of 10 s; real-time heap allocations 0",
            "result         clean: no glitch and no real-time heap allocation in 10 s",
        ] {
            assert!(text.contains(part), "{part}\n{text}");
        }
    }

    /// Glitches decide a soak's exit (6, even when Ctrl+C cut it short): mic glitches and stalls, output underruns and coarse
    /// drops, and a fault (seconds not running, a restart); and so do real-time heap allocations. The counters are summed
    /// across the restart, which starts them afresh. A run without `--seconds` exits 0 whatever happened.
    #[test]
    fn a_run_that_glitched_exits_with_6() {
        let mut tally = Tally::new(&second(0, |_| {}));
        tally.see(&second(1, |t| t.render_underruns = 2), false);
        tally.see(&second(2, |t| t.render_underruns = 2), false);
        // A fault: stopped for a second, then a fresh run whose counters start again.
        tally.see(&second(2, |t| t.state = EngineState::Error), false);
        tally.see(&second(1, |t| t.capture_glitches = 1), false);
        let sum = tally.summary(4.0, &second(1, |_| {}), &EngineStatus::default(), 14.0);
        assert_eq!((sum.counts.output.underruns, sum.counts.capture_glitches, sum.restarts, sum.running_s), (2, 1, 1, 3));
        assert_eq!(sum.glitches(), ["1 mic glitch", "2 output underruns", "1 s not running (a fault)", "1 restart"]);
        for cut_short in [false, true] {
            let f = sum.result(cut_short, true).unwrap_err();
            assert_eq!(f.exit, crate::exit::Exit::Glitched);
            assert!(f.message.starts_with("The soak wasn't clean: 1 mic glitch, 2 output underruns"), "{}", f.message);
            assert!(sum.result(cut_short, false).is_ok(), "not a soak");
        }
        assert!(sum.text().contains("result         not clean: 1 mic glitch, 2 output underruns"), "{}", sum.text());
        // Real-time heap allocations alone: not clean either.
        let mut heap = Tally::new(&second(0, |_| {}));
        heap.see(&second(1, |t| t.rt_heap_allocations = 3), false);
        let sum = heap.summary(1.0, &second(1, |_| {}), &EngineStatus::default(), 14.0);
        assert!(sum.glitches().is_empty() && sum.problems() == ["3 real-time heap allocations"], "{:?}", sum.problems());
        assert_eq!(sum.result(false, true).unwrap_err().exit, crate::exit::Exit::Glitched);
        assert!(sum.text().contains("result         not clean: 3 real-time heap allocations"), "{}", sum.text());
        // Reference gaps alone aren't glitches: the start-up has some.
        let mut quiet = Tally::new(&second(0, |_| {}));
        quiet.see(&second(1, |t| t.reference_gaps = 3), false);
        assert!(quiet.summary(1.0, &second(1, |_| {}), &EngineStatus::default(), 14.0).glitches().is_empty());
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

    #[test]
    fn warnings_are_printed_when_they_change() {
        let s = |v: &[&str]| v.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert!(changes("warning", &s(&["a"]), &s(&["a"])).is_empty());
        assert_eq!(changes("warning", &[], &s(&["the echo reference was lost"])), ["warning: the echo reference was lost"]);
        assert_eq!(changes("warning", &s(&["a", "b"]), &s(&["b", "c"])), ["warning: c", "cleared: a"]);
        assert_eq!(changes("warning", &s(&["a"]), &[]), ["cleared: a"]);
        // The notes (a stream that runs shared, not exclusive) the same way, as "note:"
        assert_eq!(changes("note", &[], &s(&["The microphone runs in shared mode"])), ["note: The microphone runs in shared mode"]);
    }

    #[test]
    fn the_start_lines_say_what_runs() {
        let mut cfg = EngineConfig::default();
        cfg.devices.output = Some("{cable}".into());
        assert_eq!(
            describe_config(&cfg),
            "Starting: mic the default, output {cable}; noise filter on, floor -60 dB, gate off; exclusive output, exclusive mic, \
             adaptive output target"
        );
        cfg.processing = ProcessingConfig { aec_enabled: true, noise_enabled: false, floor_db: -20.0, ..Default::default() };
        cfg.processing.gate.mode = GateMode::Static;
        cfg.processing.gate.threshold_db = -45.0;
        cfg.io.mic_exclusive = false;
        cfg.io.adaptive_output = false;
        assert_eq!(
            describe_config(&cfg),
            "Starting: mic the default, output {cable}, echo cancel on (reference the default); noise filter off, floor -20 dB, gate \
             static at -45.0 dBFS; exclusive output, shared mic, fixed output target"
        );
        cfg.io.output_exclusive = false;
        assert!(describe_config(&cfg).ends_with("gate static at -45.0 dBFS; shared output, shared mic, fixed output target"));
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
            notes: vec!["The microphone (Mic) runs in shared mode: ...".into()],
            ..Default::default()
        };
        assert_eq!(
            describe_status(&s),
            "Mic        Mic (48000 Hz, 2 ch, period 3.33 ms, low-latency period, RAW)\nOutput     --\n\
             Engine     large pages (32 MiB); MMCSS Pro Audio; chain 10.00 ms\nnote: the reference couldn't be opened\n\
             warning: the mic is the output cable's own recording side\nnote: The microphone (Mic) runs in shared mode: ..."
        );
        // A model pack's depth, and how it was chosen (TODO 5).
        let auto = nf_engine::ModelDepth { depth: 1, auto: Some(nf_engine::MicDelivery::OneAtATime) };
        let pack = EngineStatus { model_depth: Some(auto), chain_latency_ms: 40.0, ..s.clone() };
        assert!(
            describe_status(&pack).contains("chain 40.00 ms; model depth 1 (auto: the mic delivers a block at a time)\n"),
            "{}",
            describe_status(&pack)
        );
        let set = EngineStatus { model_depth: Some(nf_engine::ModelDepth { depth: 3, auto: None }), ..pack };
        assert!(describe_status(&set).contains("; model depth 3 (set)\n"), "{}", describe_status(&set));
    }
}
