//! Live acceptance on virtual endpoints only. `branch_probe dry|rnnoise <source capture ID> <source playback ID> A B <seconds>`
//! sends a finite clip through the production Session and records its direct A and B outputs independently.
//! `switch` compares six alternating dry and RNNoise windows with both recording clients open. `continuity` takes the path of
//! `acx-ll-cable.exe` after the duration and temporarily disables and re-enables the designated B cable's devnode.
//! All times are host packet receipts. The input witness is a separate monitoring bus. A and B each subscribe directly to the source.
#[path = "branch_probe/continuity.rs"]
mod continuity;
#[path = "branch_probe/signal.rs"]
mod signal;
#[path = "branch_probe/streams.rs"]
mod streams;
#[path = "branch_probe/switch.rs"]
mod switch;

use llcable_audio::{RouteConfig, Session};
use llcable_routing::{Bus, Effect, Send, Tap};
use llcable_sdk::{CableId, EndpointKind};
use signal::{MAX_SECONDS, Marks, RATE, decode, marker_count};
use streams::{Recording, play, record};

struct Patch {
    input: String,
    ids: [CableId; 2],
    readers: [String; 2],
    marks: Marks,
}

impl Patch {
    fn new(input: &str, a: &str, b: &str) -> Result<Self, String> {
        let cables = llcable_sdk::cables().map_err(|e| e.to_string())?;
        let find = |name: &str| {
            let c = cables.iter().find(|c| c.label.eq_ignore_ascii_case(name)).ok_or_else(|| format!("Unknown cable {name}."))?;
            let port = c.ports.iter().find(|p| p.active && p.role == EndpointKind::Microphone).ok_or("Cable has no active Out.")?;
            Ok::<_, String>((c.id, port.id.clone()))
        };
        let ((a, reader_a), (b, reader_b)) = (find(a)?, find(b)?);
        Ok(Self { input: input.to_owned(), ids: [a, b], readers: [reader_a, reader_b], marks: Marks::new() })
    }

    fn config(&self, b_gain: f32, effects: Vec<Effect>) -> RouteConfig {
        let bus = |gain, effects| Bus { sends: vec![Send { from: Tap::Input(0), gain, pan: 0. }], effects };
        RouteConfig {
            inputs: vec![self.input.clone()],
            cables: self.ids.to_vec(),
            output_buses: vec![1, 2],
            playback: vec![],
            buses: vec![bus(1., vec![self.marks.witness()]), bus(1., vec![]), bus(b_gain, effects)],
        }
    }
}

fn times(name: &str, latencies: &mut [f64]) {
    if latencies.is_empty() {
        return;
    }
    latencies.sort_by(f64::total_cmp);
    let p = |pct| latencies[(latencies.len() * pct as usize).div_ceil(100).saturating_sub(1)];
    println!("{name} ({} markers, ms): median {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}", latencies.len(), p(50), p(95), p(99), p(100));
}

fn check_dry(name: &str, rec: &Recording, gain: f32, sent: usize, whole_clip: bool, marks: &Marks) -> Result<(), String> {
    let mut first = None;
    let mut last: Option<usize> = None;
    let (mut exact, mut invalid, mut gaps, mut repeats, mut silence, mut pending_silence) = (0, 0, 0, 0, 0, 0);
    let (mut leading_other, mut trailing_other) = (0, 0);
    let (mut total, mut route) = (Vec::new(), Vec::new());
    let mut start = 0;
    for &(end, received) in &rec.packets {
        for &[l, r] in &rec.frames[start..end] {
            if l == 0. && r == 0. {
                if last.is_some() {
                    pending_silence += 1;
                }
                continue;
            }
            let Some(code) = decode([l / gain, r / gain]).filter(|&n| n < sent) else {
                if last.is_some() {
                    trailing_other += 1;
                } else {
                    leading_other += 1;
                }
                continue;
            };
            invalid += trailing_other;
            trailing_other = 0;
            silence += pending_silence;
            pending_silence = 0;
            exact += 1;
            first.get_or_insert(code);
            if let Some(previous) = last {
                gaps += code.saturating_sub(previous + 1);
                repeats += (previous + 1).saturating_sub(code);
            }
            last = Some(code);
            if let Some((t, r)) = marks.latency(code, received) {
                total.push(t);
                route.push(r);
            }
        }
        start = end;
    }
    println!(
        "{name} gain {gain}: {exact} exact stereo frames, first {first:?}, last {last:?}, {invalid} invalid and {silence} silent inside clip, {gaps} skipped, {repeats} repeated, other frames before/after {leading_other}/{trailing_other}, period {}.",
        rec.period
    );
    times(&format!("{name} source submission to receipt"), &mut total);
    times(&format!("{name} router input to receipt"), &mut route);
    let enough = if whole_clip { exact == sent && first == Some(0) && last == sent.checked_sub(1) } else { exact >= RATE / 2 };
    let expected_markers = first.zip(last).map_or(0, |(f, l)| marker_count(f, l));
    if !enough || invalid + silence + gaps + repeats != 0 || route.len() != expected_markers {
        return Err(format!("{name} stereo continuity or marker coverage failed."));
    }
    Ok(())
}

fn check_rnnoise(rec: &Recording, expected_frames: usize) -> Result<(), String> {
    let mono = rec.frames.iter().all(|f| f[0].is_finite() && f[0] == f[1]);
    let active = rec.frames.iter().filter(|f| f[0].abs() > 1e-7).count();
    println!("B with RNNoise: {} frames above -140 dBFS, identical finite channels {mono}, period {}.", active, rec.period);
    if rec.frames.len() < RATE / 2 || !mono || active < expected_frames / 10 {
        return Err("B's RNNoise recording was short, silent, nonfinite or stereo.".into());
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [mode, capture, player, a, b, seconds, extra @ ..] = args.as_slice() else {
        return Err("Usage: branch_probe dry|rnnoise|switch|continuity <virtual source capture ID> <virtual source playback ID> <A label> <B label> <seconds> [setup executable for continuity]".into());
    };
    let seconds: usize = seconds.parse().map_err(|_| "Invalid duration.")?;
    if !(2..=MAX_SECONDS).contains(&seconds) {
        return Err(format!("Use 2 to {MAX_SECONDS} seconds."));
    }
    let patch = Patch::new(capture, a, b)?;
    if mode == "continuity" {
        let [exe] = extra else { return Err("Continuity needs the path to acx-ll-cable.exe to cycle B's device.".into()) };
        return continuity::run(patch, player, b, exe, seconds);
    }
    if !extra.is_empty() {
        return Err("Unexpected argument after duration.".into());
    }
    let wet = match mode.as_str() {
        "dry" => false,
        "rnnoise" => true,
        "switch" => return switch::run(patch, player, seconds),
        _ => return Err("Choose dry, rnnoise, switch or continuity.".into()),
    };
    let effects = if wet { vec![llcable_effects::noise_reduction()] } else { vec![] };
    let session = Session::start(patch.config(1., effects))?;
    let read_a = record(patch.readers[0].clone(), patch.marks.clone(), seconds)?;
    let read_b = record(patch.readers[1].clone(), patch.marks.clone(), seconds)?;
    let sent = play(player.clone(), patch.marks.clone(), seconds)?.finish(false)?;
    let status = session.snapshot();
    let (a, b) = (read_a.finish(true)?, read_b.finish(true)?);
    println!(
        "B {mode}: {} sent, source playback period {} / target fill {}, source capture period {}, A/B router drops {:?}, underrun {:?}, startup {:?}.",
        sent.frames,
        sent.period,
        sent.fill,
        status.inputs[0].period_frames,
        status.outputs.iter().map(|o| o.dropped_frames).collect::<Vec<_>>(),
        status.outputs.iter().map(|o| o.underrun_frames).collect::<Vec<_>>(),
        status.outputs.iter().map(|o| o.startup_silent_frames).collect::<Vec<_>>(),
    );
    if !status.running || status.outputs_offline.contains(&true) {
        return Err(status.error.unwrap_or("A destination went offline.".into()));
    }
    let output_a = check_dry("A", &a, 1., sent.frames, true, &patch.marks);
    let output_b = if wet { check_rnnoise(&b, sent.frames) } else { check_dry("B", &b, 1., sent.frames, true, &patch.marks) };
    output_a.and(output_b)
}
