//! The window's text, as pure functions of the engine's values: labels, the status line, the live
//! readout, diagnostics and the latency report.
//!
//! Numbers come from atomics the audio thread writes, so any of them can be NaN or infinite. They
//! print as "—" (or "-∞" for a silent level), never as "NaN", and -0.0 prints as "0".

use nf_engine::{
    EngineState, EngineStatus, LatencyBudget, LatencyReport, MeterSnapshot, REFERENCE_TARGET_HIGH_MS, StreamInfo, TelemetrySnapshot,
};

use crate::settings::{FLOOR_DB, TestMode};
use crate::window::Job;

/// Shown for a value that isn't available.
pub(crate) const DASH: &str = "—";
/// Levels at or below this are silence (the engine reports -200 dBFS for digital zero).
pub(crate) const SILENT_DB: f32 = -120.0;

/// `value` with `decimals` places: "—" unless finite, and never "-0" or "-0.0".
pub(crate) fn fixed(value: impl Into<f64>, decimals: usize) -> String {
    let value = value.into();
    if !value.is_finite() {
        return DASH.to_owned();
    }
    let text = format!("{value:.decimals$}");
    match text.strip_prefix('-') {
        Some(rest) if rest.bytes().all(|b| b == b'0' || b == b'.') => rest.to_owned(),
        _ => text,
    }
}

/// A signed value: "+12", "-40", and "0" for zero.
pub(crate) fn signed(value: impl Into<f64>, decimals: usize) -> String {
    let text = fixed(value, decimals);
    if text.bytes().next().is_some_and(|b| b.is_ascii_digit()) && text.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        format!("+{text}")
    } else {
        text
    }
}

/// A level in dB with one decimal: "-23.4", "-∞" for silence, "—" for NaN or +∞.
pub(crate) fn db(value: f32) -> String {
    if value <= SILENT_DB { "-∞".to_owned() } else { fixed(value, 1) }
}

/// A slider's header: "Attack: 2.0 ms", or "Threshold (dBFS): -50" with no unit.
pub(crate) fn labelled(name: &str, value: f32, decimals: usize, unit: &str) -> String {
    let value = fixed(value, decimals);
    if unit.is_empty() { format!("{name}: {value}") } else { format!("{name}: {value} {unit}") }
}

/// The "Max suppression" slider's header: "unlimited" at the slider's bottom (-60 dB).
pub(crate) fn suppression(floor_db: f32) -> String {
    if floor_db.is_nan() || floor_db <= FLOOR_DB.min + 0.5 {
        "Max suppression: unlimited".to_owned()
    } else {
        labelled("Max suppression", floor_db, 0, "dB")
    }
}

/// The engine's message, trimmed; `None` when there's nothing to say.
fn message(status: &EngineStatus) -> Option<&str> {
    status.message.as_deref().map(str::trim).filter(|m| !m.is_empty())
}

/// `text` ending in a full stop, so another sentence can follow it.
fn sentence(text: &str) -> String {
    if text.ends_with(['.', '!', '?']) { text.to_owned() } else { format!("{text}.") }
}

/// The header's status line: the state (or the job under way), then the engine's message. A
/// fault the engine will retry by itself reads "Waiting to retry", not "Stopped" (gui/F8).
pub(crate) fn status_line(status: &EngineStatus, job: Option<Job>) -> String {
    let state = match (job, status.state) {
        (Some(Job::Starting), _) | (None, EngineState::Starting) => "Starting…",
        (Some(Job::Stopping), _) => "Stopping…",
        (None, EngineState::Running) => "Running",
        (None, EngineState::Stopped | EngineState::Error) if status.retrying => "Waiting to retry",
        (None, EngineState::Stopped) => "Stopped",
        (None, EngineState::Error) => "Stopped by an error",
    };
    match message(status) {
        Some(message) => format!("{state} · {message}"),
        None => state.to_owned(),
    }
}

/// The InfoBar's text when a fault stops a run (gui/F10): what happened, and what happens next.
pub(crate) fn fault(status: &EngineStatus) -> String {
    let what = sentence(message(status).unwrap_or("An audio device stopped working"));
    if status.retrying {
        format!("{what} The filter starts again by itself when it can. Press Stop to keep it off.")
    } else {
        format!("{what} Press Start to try again.")
    }
}

/// How the warnings InfoBar reads: a warning (something worth fixing), or only notes (how it runs: a stream asked for in
/// exclusive mode that runs shared, an ordinary thing now that exclusive mode is the default), shown calmer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Warning,
    Note,
}

/// The warnings InfoBar's tone, title and text: the engine's warnings while it runs, then its notes, one per line. `None`
/// when it isn't running or all is well.
pub(crate) fn warnings(status: &EngineStatus) -> Option<(Tone, &'static str, String)> {
    if status.state != EngineState::Running {
        return None;
    }
    let lines = |v: &[String]| v.iter().map(|w| w.trim().to_owned()).filter(|w| !w.is_empty()).collect::<Vec<_>>();
    let (warnings, notes) = (lines(&status.warnings), lines(&status.notes));
    let (tone, title) = match (warnings.len(), notes.len()) {
        (0, 0) => return None,
        (0, 1) => (Tone::Note, "Running with a note"),
        (0, _) => (Tone::Note, "Running with notes"),
        (1, _) => (Tone::Warning, "Running with a warning"),
        _ => (Tone::Warning, "Running with warnings"),
    };
    Some((tone, title, [warnings, notes].concat().join("\n")))
}

/// A meter's numbers: "RMS -23.4 · peak -12.1 dBFS".
pub(crate) fn meter_numbers(m: MeterSnapshot) -> String {
    format!("RMS {} · peak {} dBFS", db(m.rms_dbfs), db(m.peak_dbfs))
}

/// The output meter's extra line: "Reduction: -12.0 dB · Speech: 0.87". The reduction is what the
/// noise stage took off (its floor and the gate included).
pub(crate) fn reduction_and_speech(t: &TelemetrySnapshot) -> String {
    let speech = if t.speech_probability.is_nan() { f32::NAN } else { t.speech_probability.clamp(0.0, 1.0) };
    format!("Reduction: {} dB · Speech: {}", db(t.noise_reduction_db), fixed(speech, 2))
}

/// Readout line 1a: "Pipeline: 12.3 ms (p95 14.1)", or "Pipeline: —" while it isn't valid.
pub(crate) fn pipeline(t: &TelemetrySnapshot) -> String {
    if t.latency_valid && t.latency_ms_median.is_finite() {
        format!("Pipeline: {} ms (p95 {})", fixed(t.latency_ms_median, 1), fixed(t.latency_ms_p95, 1))
    } else {
        format!("Pipeline: {DASH}")
    }
}

/// A latency budget's parts (engine.md, "The latency budget"): "mic 16.2 + wait 0.1 + output queue 22.0 + render 23.4 ms,
/// chain 10.0 ms on top". The readout holds the four parts; the chain's offset isn't in it.
fn budget_parts(b: &LatencyBudget) -> String {
    format!(
        "mic {} + wait {} + output queue {} + render {} ms, chain {} ms on top",
        fixed(b.mic_ms, 1),
        fixed(b.wait_ms, 1),
        fixed(b.queue_ms, 1),
        fixed(b.render_ms, 1),
        fixed(b.chain_ms, 1)
    )
}

/// Readout line 1a's budget, under it: "Budget (means): mic 16.2 + wait 0.1 + output queue 22.0 + render 23.4 ms, chain
/// 10.0 ms on top", or "Budget: —" while the readout isn't valid. With the adaptive output target, the target the queue
/// holds now (it moves): "... · output target 9.5 ms (adaptive)".
pub(crate) fn budget(t: &TelemetrySnapshot) -> String {
    if t.latency_valid && t.budget_mean.readout_ms().is_finite() {
        let target =
            if t.output_adaptive { format!(" · output target {} ms (adaptive)", fixed(t.output_target_ms, 1)) } else { String::new() };
        format!("Budget (means): {}{target}", budget_parts(&t.budget_mean))
    } else {
        format!("Budget: {DASH}")
    }
}

/// The streams' modes, under the clocks: "Streams: mic shared, 10.0 ms period · output exclusive, 2.0 ms period · output
/// target fixed". Exclusive is WASAPI exclusive mode, as asked for and granted (a refused one runs shared, with a
/// note; an output on the default playback device runs shared, with that warning); the target is fixed (a period + a
/// block + the margin) or adaptive.
pub(crate) fn streams(t: &TelemetrySnapshot) -> String {
    let mode = |exclusive: bool| if exclusive { "exclusive" } else { "shared" };
    format!(
        "Streams: mic {}, {} ms period · output {}, {} ms period · output target {}",
        mode(t.mic_exclusive),
        fixed(t.mic_period_ms, 1),
        mode(t.output_exclusive),
        fixed(t.output_period_ms, 1),
        if t.output_adaptive { "adaptive" } else { "fixed" }
    )
}

/// A fill target: "—" when there's none (0 while stopped, or without a reference).
fn target(ms: f32) -> String {
    if ms > 0.0 { fixed(ms, 1) } else { DASH.to_owned() }
}

/// Readout line 1b: "Drift out/ref: +12/-40 ppm · Output fill: 15.0 ms (target 15.5) · Reference lead: 12.0 ms (target
/// 12.0)". The reference's is a lead (how long before the mic block its reference was tapped), not a fill; it says when
/// it's timed by delivery (no trusted stamps), and when its target is high: over `REFERENCE_TARGET_HIGH_MS` it may pass
/// the echo path's own delay, and the echo come before its reference.
pub(crate) fn clocks(t: &TelemetrySnapshot) -> String {
    let mut s = format!(
        "Drift out/ref: {}/{} ppm · Output fill: {} ms (target {}{}) · Reference lead: {} ms (target {})",
        signed(t.output_drift_ppm, 0),
        signed(t.reference_drift_ppm, 0),
        fixed(t.output_fill_ms, 1),
        target(t.output_target_ms),
        if t.output_adaptive { ", adaptive" } else { "" },
        fixed(t.reference_fill_ms, 1),
        target(t.reference_target_ms)
    );
    if t.reference_target_ms > 0.0 && !t.reference_stamped {
        s.push_str(", by delivery");
    }
    if t.reference_target_ms > REFERENCE_TARGET_HIGH_MS {
        s.push_str(", high for speakers");
    }
    // The echo-path guard's mic hold (nf-engine echo_guard.rs): the lead above is the echo path's, less it.
    if t.echo_hold_ms > 0.0 {
        let trial = if t.echo_guard == nf_engine::ECHO_GUARD_TRIAL { ", on a trial" } else { "" };
        s.push_str(&format!(" · Mic held for the echo: {} ms{trial}", fixed(t.echo_hold_ms, 1)));
    }
    s
}

/// Readout line 2a: "CPU per block: p50 45 µs, p99 80 µs (AEC p99 30, noise p99 40)".
pub(crate) fn cpu(t: &TelemetrySnapshot) -> String {
    format!(
        "CPU per block: p50 {} µs, p99 {} µs (AEC p99 {}, noise p99 {})",
        fixed(t.block_us_p50, 0),
        fixed(t.block_us_p99, 0),
        fixed(t.aec_us_p99, 0),
        fixed(t.noise_us_p99, 0)
    )
}

/// Readout line 2b: "Dropouts: capture 0, output 1, reference gaps 2, mic stalls 0 · Coarse drops
/// out/ref: 0/0 · Tap overflows: 0". A coarse drop is one correction that dropped whole blocks to
/// pull an overfull buffer back to its target (on the reference, a jump of its timeline); a tap
/// overflow is a block the latency test lost.
pub(crate) fn dropouts(t: &TelemetrySnapshot) -> String {
    format!(
        "Dropouts: capture {}, output {}, reference gaps {}, mic stalls {} · Coarse drops out/ref: {}/{} · Tap \
         overflows: {}",
        t.capture_glitches,
        t.render_underruns,
        t.reference_gaps,
        t.stalls,
        t.output_coarse_drops,
        t.reference_coarse_drops,
        t.tap_overflows
    )
}

/// Readout line 2c (gui.md section 5): "RT heap allocations: 0 (must stay 0) · Pool allocations:
/// 430/s". The pool's are the locked TLSF pool's, served in O(1); the heap's are fallbacks to the
/// system heap, which takes a lock.
pub(crate) fn allocations(t: &TelemetrySnapshot) -> String {
    format!("RT heap allocations: {} (must stay 0) · Pool allocations: {}/s", t.rt_heap_allocations, t.rt_pool_allocations_per_s)
}

/// One stream's diagnostics: "Mic: USB Mic · 48000 Hz, 2 ch · 10.0 ms period · RAW".
pub(crate) fn stream(label: &str, s: Option<&StreamInfo>) -> String {
    let Some(s) = s else {
        return format!("{label}: not open");
    };
    let mut line = format!("{label}: {} · {} Hz, {} ch · {} ms period", s.device_name, s.sample_rate, s.channels, fixed(s.period_ms, 1));
    if s.low_latency {
        line.push_str(" (IAudioClient3)");
    }
    if s.autoconvert {
        line.push_str(" · resampled by Windows");
    }
    if s.raw {
        line.push_str(" · RAW");
    }
    if let Some(format) = s.exclusive {
        line.push_str(&format!(" · exclusive, {format}"));
    }
    if let Some(why) = &s.exclusive_refused {
        line.push_str(&format!(" · shared: exclusive refused ({why})"));
    }
    line
}

/// The diagnostics section's lines (gui.md section 7).
pub(crate) fn diagnostics(status: &EngineStatus) -> Vec<String> {
    let running = status.state == EngineState::Running;
    let memory = if status.memory.trim().is_empty() { DASH } else { status.memory.as_str() };
    let mmcss = match (running, status.mmcss) {
        (false, _) => DASH,
        (true, true) => "registered",
        (true, false) => "not registered",
    };
    let raw = match &status.mic {
        Some(mic) if mic.raw => "in effect",
        // Exclusive mode reaches the driver past Windows' effects: RAW has nothing to turn off.
        Some(mic) if mic.exclusive.is_some() => "not needed (exclusive mode)",
        Some(_) => "not in effect",
        None => DASH,
    };
    vec![
        format!("Memory: {memory}"),
        format!("MMCSS \"Pro Audio\": {mmcss}"),
        format!("RAW mic: {raw}"),
        stream("Mic", status.mic.as_ref()),
        stream("Output", status.output.as_ref()),
        stream("Echo reference", status.reference.as_ref()),
        if running { format!("Chain latency: {} ms", fixed(status.chain_latency_ms, 1)) } else { format!("Chain latency: {DASH}") },
    ]
}

fn ms(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() => format!("{} ms", fixed(v, 1)),
        _ => DASH.to_owned(),
    }
}

/// The latency test's result (gui.md section 6), one line each.
pub(crate) fn report(r: &LatencyReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(8 + r.warnings.len());
    if r.valid == 0 {
        lines.push(format!("No valid repetitions (0 of {}): no arrival was found with enough confidence.", r.repetitions));
    } else {
        lines.push(format!(
            "App (input tap → cable out): median {} ms, spread {} ms ({} of {} valid)",
            fixed(r.app_ms_median, 1),
            fixed(r.app_ms_spread, 1),
            r.valid,
            r.repetitions
        ));
    }
    lines.push(format!("Input path: {}", ms(r.input_path_ms_median)));
    lines.push(format!("End to end: {}", ms(r.end_to_end_ms_median)));
    lines.push(format!("Chain: {} ms", fixed(r.chain_ms, 1)));
    if let Some(readout) = r.readout_ms_median {
        lines.push(format!("Live readout during the test: {}", ms(Some(f64::from(readout)))));
    }
    if let Some(b) = &r.budget {
        lines.push(format!("Its budget (means over the test): {}", budget_parts(b)));
    }
    if let Some(e) = &r.output {
        // What the output side did meanwhile (TODO 9): a spread these don't explain is the devices' own.
        lines.push(format!("Output during the test: {}", e.text()));
    }
    if r.valid > 0 {
        lines.push(format!("Confidence: {} dB (lowest)", fixed(r.confidence_db_min, 1)));
    }
    lines.extend(r.warnings.iter().map(|w| format!("Warning: {w}")));
    if !r.conditions.trim().is_empty() {
        lines.push(format!("Conditions: {}", r.conditions));
    }
    lines
}

/// The latency test's help text for each mode (gui.md section 6).
pub(crate) fn test_help(mode: TestMode) -> &'static str {
    match mode {
        TestMode::Virtual => {
            "Set Microphone to CABLE-A Output. Play the probe on CABLE-A Input. Capture CABLE Output (the cable set as \
             Output above). Needs two virtual cables (e.g. VB-CABLE plus CABLE-A, or two Voicemeeter inputs)."
        }
        TestMode::Acoustic => "Play the probe on your speakers, with the mic as Microphone. Quiet room, normal volume.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_handles_rounding_signs_and_non_finite_values() {
        assert_eq!(fixed(12.34f32, 1), "12.3");
        assert_eq!(fixed(-23.46f64, 1), "-23.5");
        assert_eq!(fixed(-0.0f32, 1), "0.0");
        assert_eq!(fixed(-0.04f32, 1), "0.0", "rounds to zero: no minus");
        assert_eq!(fixed(-0.4f32, 0), "0");
        assert_eq!(fixed(-0.6f32, 0), "-1");
        assert_eq!(fixed(f32::NAN, 1), DASH);
        assert_eq!(fixed(f64::INFINITY, 1), DASH);
        assert_eq!(fixed(5u32, 0), "5");
    }

    #[test]
    fn signed_values_carry_a_plus() {
        assert_eq!(signed(12.4f32, 0), "+12");
        assert_eq!(signed(-40.0f32, 0), "-40");
        assert_eq!(signed(0.2f32, 0), "0");
        assert_eq!(signed(-0.2f32, 0), "0");
        assert_eq!(signed(0.05f32, 1), "+0.1");
        assert_eq!(signed(f32::NAN, 0), DASH);
    }

    #[test]
    fn db_labels() {
        assert_eq!(db(-23.44), "-23.4");
        assert_eq!(db(0.0), "0.0");
        assert_eq!(db(-0.0), "0.0");
        assert_eq!(db(3.0), "3.0");
        assert_eq!(db(-200.0), "-∞", "the engine's silence");
        assert_eq!(db(f32::NEG_INFINITY), "-∞");
        assert_eq!(db(SILENT_DB), "-∞");
        assert_eq!(db(-119.9), "-119.9");
        assert_eq!(db(f32::NAN), DASH);
        assert_eq!(db(f32::INFINITY), DASH);
    }

    #[test]
    fn slider_labels() {
        assert_eq!(labelled("Attack", 2.0, 1, "ms"), "Attack: 2.0 ms");
        assert_eq!(labelled("Threshold (dBFS)", -50.0, 0, ""), "Threshold (dBFS): -50");
        assert_eq!(suppression(-60.0), "Max suppression: unlimited");
        assert_eq!(suppression(-59.7), "Max suppression: unlimited", "the slider's bottom, give or take a step's rounding");
        assert_eq!(suppression(f32::NAN), "Max suppression: unlimited");
        assert_eq!(suppression(-24.0), "Max suppression: -24 dB");
        assert_eq!(suppression(0.0), "Max suppression: 0 dB");
    }

    fn status(state: EngineState, message: Option<&str>) -> EngineStatus {
        EngineStatus { state, message: message.map(str::to_owned), ..EngineStatus::default() }
    }

    #[test]
    fn the_status_line_shows_the_job_then_the_message() {
        assert_eq!(status_line(&status(EngineState::Stopped, None), None), "Stopped");
        assert_eq!(status_line(&status(EngineState::Stopped, None), Some(Job::Starting)), "Starting…");
        assert_eq!(status_line(&status(EngineState::Running, None), Some(Job::Stopping)), "Stopping…");
        assert_eq!(status_line(&status(EngineState::Starting, Some(" ")), None), "Starting…");
        assert_eq!(status_line(&status(EngineState::Running, Some("Reference device lost")), None), "Running · Reference device lost");
        assert_eq!(
            status_line(&status(EngineState::Error, Some("The mic was unplugged")), None),
            "Stopped by an error · The mic was unplugged"
        );
    }

    /// gui/F8: an engine armed to restart by itself isn't "Stopped".
    #[test]
    fn a_retrying_engine_is_waiting_to_retry() {
        let retrying = EngineStatus { retrying: true, ..status(EngineState::Error, Some("The mic was unplugged")) };
        assert_eq!(status_line(&retrying, None), "Waiting to retry · The mic was unplugged");
        let stopped = EngineStatus { state: EngineState::Stopped, ..retrying.clone() };
        assert_eq!(status_line(&stopped, None), "Waiting to retry · The mic was unplugged");
        // The retry itself, and the user's stop, read as usual
        let starting = EngineStatus { state: EngineState::Starting, ..retrying.clone() };
        assert_eq!(status_line(&starting, None), "Starting… · The mic was unplugged");
        assert!(status_line(&retrying, Some(Job::Stopping)).starts_with("Stopping…"));
    }

    /// gui/F10: a fault's InfoBar says what happened and what happens next.
    #[test]
    fn the_fault_text() {
        let retrying = EngineStatus { retrying: true, ..status(EngineState::Error, Some("The mic was unplugged")) };
        assert_eq!(fault(&retrying), "The mic was unplugged. The filter starts again by itself when it can. Press Stop to keep it off.");
        let final_ = status(EngineState::Error, Some("The output cable's format isn't supported."));
        assert_eq!(fault(&final_), "The output cable's format isn't supported. Press Start to try again.");
        assert_eq!(fault(&status(EngineState::Error, Some("  "))), "An audio device stopped working. Press Start to try again.");
    }

    #[test]
    fn warnings_show_while_running_one_per_line() {
        let mut s = status(EngineState::Running, None);
        assert_eq!(warnings(&s), None, "all is well");
        s.warnings = vec!["The echo reference was lost".to_owned()];
        assert_eq!(warnings(&s), Some((Tone::Warning, "Running with a warning", "The echo reference was lost".to_owned())));
        s.warnings.push(" ".to_owned());
        s.warnings.push("MMCSS isn't available".to_owned());
        let both = "The echo reference was lost\nMMCSS isn't available";
        assert_eq!(warnings(&s), Some((Tone::Warning, "Running with warnings", both.to_owned())));
        // The notes come after the warnings, and alone they're a note (an exclusive stream that runs shared)
        s.notes = vec!["The microphone (Mic) runs in shared mode: ...".to_owned()];
        assert_eq!(warnings(&s), Some((Tone::Warning, "Running with warnings", format!("{both}\n{}", s.notes[0]))));
        s.warnings.clear();
        assert_eq!(warnings(&s), Some((Tone::Note, "Running with a note", s.notes[0].clone())));
        s.notes.push("The output device (CABLE Input) runs in shared mode: ...".to_owned());
        assert!(matches!(warnings(&s), Some((Tone::Note, "Running with notes", _))));
        s.state = EngineState::Error;
        assert_eq!(warnings(&s), None, "only while running");
    }

    fn telemetry() -> TelemetrySnapshot {
        TelemetrySnapshot {
            state: EngineState::Running,
            latency_valid: true,
            latency_ms_median: 12.34,
            latency_ms_p95: 14.06,
            budget_mean: LatencyBudget { mic_ms: 16.24, wait_ms: 0.08, chain_ms: 10.0, queue_ms: 22.04, render_ms: 23.44 },
            output_drift_ppm: 12.2,
            reference_drift_ppm: -40.4,
            output_fill_ms: 15.0,
            reference_fill_ms: 12.0,
            block_us_p50: 45.2,
            block_us_p99: 80.4,
            aec_us_p99: 30.0,
            noise_us_p99: 41.0,
            capture_glitches: 1,
            render_underruns: 2,
            reference_gaps: 3,
            rt_heap_allocations: 0,
            rt_pool_allocations_per_s: 4300,
            stalls: 4,
            output_coarse_drops: 5,
            reference_coarse_drops: 6,
            tap_overflows: 7,
            output_target_ms: 15.5,
            reference_target_ms: 12.0,
            reference_stamped: true,
            noise_reduction_db: -12.0,
            speech_probability: 0.874,
            input: MeterSnapshot { peak_dbfs: -12.06, rms_dbfs: -23.4 },
            ..TelemetrySnapshot::default()
        }
    }

    #[test]
    fn the_readout_lines() {
        let t = telemetry();
        assert_eq!(pipeline(&t), "Pipeline: 12.3 ms (p95 14.1)");
        assert_eq!(budget(&t), "Budget (means): mic 16.2 + wait 0.1 + output queue 22.0 + render 23.4 ms, chain 10.0 ms on top");
        assert_eq!(clocks(&t), "Drift out/ref: +12/-40 ppm · Output fill: 15.0 ms (target 15.5) · Reference lead: 12.0 ms (target 12.0)");
        // Timed by delivery, and a target that may pass a speaker's echo path, are said.
        let odd = TelemetrySnapshot { reference_stamped: false, reference_target_ms: 31.0, ..t };
        assert!(clocks(&odd).ends_with("(target 31.0), by delivery, high for speakers"), "{}", clocks(&odd));
        assert_eq!(cpu(&t), "CPU per block: p50 45 µs, p99 80 µs (AEC p99 30, noise p99 41)");
        assert_eq!(
            dropouts(&t),
            "Dropouts: capture 1, output 2, reference gaps 3, mic stalls 4 · Coarse drops out/ref: 5/6 · \
             Tap overflows: 7"
        );
        assert_eq!(allocations(&t), "RT heap allocations: 0 (must stay 0) · Pool allocations: 4300/s");
        assert_eq!(meter_numbers(t.input), "RMS -23.4 · peak -12.1 dBFS");
        assert_eq!(reduction_and_speech(&t), "Reduction: -12.0 dB · Speech: 0.87");
        let invalid = TelemetrySnapshot { latency_valid: false, ..t };
        assert_eq!(pipeline(&invalid), "Pipeline: —");
        assert_eq!(budget(&invalid), "Budget: —");
        let nan = TelemetrySnapshot {
            latency_ms_median: f32::NAN,
            speech_probability: f32::NAN,
            output_drift_ppm: f32::NAN,
            output_target_ms: f32::NAN,
            ..t
        };
        assert_eq!(pipeline(&nan), "Pipeline: —");
        assert_eq!(reduction_and_speech(&nan), "Reduction: -12.0 dB · Speech: —");
        assert!(clocks(&nan).starts_with("Drift out/ref: —/-40 ppm"));
        assert!(clocks(&nan).contains("Output fill: 15.0 ms (target —)"), "{}", clocks(&nan));
        let wild = TelemetrySnapshot { speech_probability: 7.0, ..t };
        assert_eq!(reduction_and_speech(&wild), "Reduction: -12.0 dB · Speech: 1.00");
        // No reference: no target for it
        let no_reference = TelemetrySnapshot { reference_target_ms: 0.0, reference_fill_ms: 0.0, ..t };
        let line = clocks(&no_reference);
        assert!(line.ends_with("Reference lead: 0.0 ms (target —)"), "{line}");
        // The latency options: the streams' modes, and an adaptive target with the budget and the fill.
        let t = TelemetrySnapshot { mic_period_ms: 10.0, output_period_ms: 10.0, ..t };
        assert_eq!(streams(&t), "Streams: mic shared, 10.0 ms period · output shared, 10.0 ms period · output target fixed");
        let x = TelemetrySnapshot { output_exclusive: true, output_period_ms: 2.0, output_adaptive: true, output_target_ms: 5.0, ..t };
        assert_eq!(streams(&x), "Streams: mic shared, 10.0 ms period · output exclusive, 2.0 ms period · output target adaptive");
        assert!(budget(&x).ends_with("chain 10.0 ms on top · output target 5.0 ms (adaptive)"), "{}", budget(&x));
        assert!(clocks(&x).contains("Output fill: 15.0 ms (target 5.0, adaptive)"), "{}", clocks(&x));
        let m = TelemetrySnapshot { mic_exclusive: true, mic_period_ms: 10.0 / 3.0, ..t };
        assert!(streams(&m).starts_with("Streams: mic exclusive, 3.3 ms period"), "{}", streams(&m));
    }

    fn stream_info() -> StreamInfo {
        StreamInfo {
            device_name: "USB Mic".to_owned(),
            sample_rate: 44_100,
            channels: 1,
            period_ms: 2.67,
            low_latency: true,
            autoconvert: true,
            raw: true,
            exclusive: None,
            exclusive_refused: None,
        }
    }

    #[test]
    fn stream_and_diagnostics_lines() {
        assert_eq!(stream("Mic", None), "Mic: not open");
        assert_eq!(
            stream("Mic", Some(&stream_info())),
            "Mic: USB Mic · 44100 Hz, 1 ch · 2.7 ms period (IAudioClient3) · resampled by Windows · RAW"
        );
        let plain = StreamInfo { low_latency: false, autoconvert: false, raw: false, period_ms: 10.0, ..stream_info() };
        assert_eq!(stream("Output", Some(&plain)), "Output: USB Mic · 44100 Hz, 1 ch · 10.0 ms period");
        let excl = StreamInfo { sample_rate: 48_000, channels: 2, period_ms: 2.0, exclusive: Some("16-bit"), ..plain.clone() };
        assert_eq!(stream("Output", Some(&excl)), "Output: USB Mic · 48000 Hz, 2 ch · 2.0 ms period · exclusive, 16-bit");
        let refused = StreamInfo { exclusive_refused: Some("another app is using it".into()), ..plain.clone() };
        assert!(stream("Output", Some(&refused)).ends_with("10.0 ms period · shared: exclusive refused (another app is using it)"));

        let stopped = diagnostics(&EngineStatus::default());
        assert_eq!(stopped[0], "Memory: —");
        assert_eq!(stopped[1], "MMCSS \"Pro Audio\": —");
        assert_eq!(stopped[2], "RAW mic: —");
        assert_eq!(stopped[6], "Chain latency: —", "no chain while stopped");
        let running = EngineStatus {
            state: EngineState::Running,
            mic: Some(stream_info()),
            memory: "large pages (16 MiB)".to_owned(),
            mmcss: true,
            chain_latency_ms: 10.0,
            ..EngineStatus::default()
        };
        let lines = diagnostics(&running);
        assert_eq!(lines[..3], ["Memory: large pages (16 MiB)", "MMCSS \"Pro Audio\": registered", "RAW mic: in effect"]);
        assert_eq!(lines[4], "Output: not open");
        assert_eq!(lines[6], "Chain latency: 10.0 ms");
        let exclusive = StreamInfo { raw: false, exclusive: Some("16-bit"), ..stream_info() };
        let lines = diagnostics(&EngineStatus { mic: Some(exclusive), ..running });
        assert_eq!(lines[2], "RAW mic: not needed (exclusive mode)");
        assert!(lines[3].ends_with(" · exclusive, 16-bit"), "{}", lines[3]);
    }

    #[test]
    fn the_report_lines() {
        let r = LatencyReport {
            repetitions: 5,
            valid: 4,
            app_ms: vec![12.0, 12.4, 12.8, 12.2],
            app_ms_median: 12.3,
            app_ms_spread: 0.8,
            input_path_ms_median: Some(3.24),
            end_to_end_ms_median: None,
            readout_ms_median: Some(12.1),
            budget: Some(LatencyBudget { mic_ms: 10.5, wait_ms: 0.0, chain_ms: 10.0, queue_ms: 22.0, render_ms: 5.0 }),
            output: Some(nf_engine::OutputEvents {
                underruns: 1,
                deficits: 1,
                zero_frames: 480,
                slips: 4,
                slip_net: -2,
                ..Default::default()
            }),
            chain_ms: 10.0,
            confidence_db_min: 18.04,
            conditions: "mic 10 ms, RAW".to_owned(),
            warnings: vec!["One repetition was too quiet".to_owned()],
        };
        assert_eq!(report(&r), [
            "App (input tap → cable out): median 12.3 ms, spread 0.8 ms (4 of 5 valid)",
            "Input path: 3.2 ms",
            "End to end: —",
            "Chain: 10.0 ms",
            "Live readout during the test: 12.1 ms",
            "Its budget (means over the test): mic 10.5 + wait 0.0 + output queue 22.0 + render 5.0 ms, chain 10.0 ms on top",
            "Output during the test: 1 underrun, 1 deficit (10.0 ms of zeros); 4 slips (net -2 samples)",
            "Confidence: 18.0 dB (lowest)",
            "Warning: One repetition was too quiet",
            "Conditions: mic 10 ms, RAW",
        ]);
        let none = LatencyReport { repetitions: 5, ..LatencyReport::default() };
        let lines = report(&none);
        assert!(lines[0].starts_with("No valid repetitions (0 of 5)"));
        assert!(!lines.iter().any(|l| l.starts_with("Confidence") || l.starts_with("Conditions") || l.starts_with("Live readout")));
    }

    #[test]
    fn each_mode_has_its_help() {
        assert!(test_help(TestMode::Virtual).starts_with("Set Microphone to CABLE-A Output."));
        assert!(test_help(TestMode::Acoustic).contains("Quiet room"));
    }
}
