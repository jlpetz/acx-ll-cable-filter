use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use llcable_audio::{CableId, Session};
use nf_vmic::Device;
use nf_vmic::protocol::control_path;

use super::signal::RATE;
use super::streams::{Recording, play, record};
use super::{Patch, check_dry};

fn wait_for(description: &str, mut ready: impl FnMut() -> bool) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(12);
    while !ready() {
        if Instant::now() >= until {
            return Err(format!("Timed out waiting for {description}."));
        }
        thread::sleep(Duration::from_millis(40));
    }
    Ok(())
}

fn setup(exe: &str, op: &str, label: &str) -> Result<(), String> {
    let result = Command::new(exe).args([op, label]).output().map_err(|e| format!("Cable {op}: {e}"))?;
    if !result.status.success() {
        return Err(format!("Cable {op} {label}: {}, {}", result.status, String::from_utf8_lossy(&result.stderr).trim()));
    }
    Ok(())
}

fn listen(patch: &Patch) -> Result<Recording, String> {
    let recorder = record(patch.readers[1].clone(), patch.marks.clone(), 2)?;
    thread::sleep(Duration::from_millis(800));
    recorder.finish(true)
}

fn attach_count(id: CableId) -> Result<u64, String> {
    Device::open_path(&control_path(&id)).and_then(|device| device.stats()).map(|stats| stats.ring.attaches).map_err(|e| e.to_string())
}

pub fn run(patch: Patch, source_player: &str, b_label: &str, setup_exe: &str, seconds: usize) -> Result<(), String> {
    let b = patch.ids[1];
    let session = Session::start(patch.config(0.25, vec![]))?;
    let reader = record(patch.readers[0].clone(), patch.marks.clone(), seconds)?;
    let player = play(source_player.to_owned(), patch.marks.clone(), seconds)?;
    let attaches = attach_count(patch.ids[0])?;
    thread::sleep(Duration::from_millis(1500));
    let before = listen(&patch)?;
    session.set_cable_enabled(b, false)?;
    wait_for("B to detach", || llcable_sdk::diagnostics(b).is_ok_and(|d| !d.attached))?;
    session.reconfigure(patch.config(0.5, vec![]))?;
    let silent = listen(&patch)?;
    if silent.frames.len() < RATE / 2 || silent.frames.iter().any(|&f| f != [0.; 2]) {
        return Err("B continued to output samples after disabling its feed and changing its gain.".into());
    }
    session.set_cable_enabled(b, true)?;
    wait_for("B's edited feed", || llcable_sdk::diagnostics(b).is_ok_and(|d| d.attached))?;
    let edited = listen(&patch)?;
    // Close the B verification reader before asking Windows to disable its devnode. A's recorder stays open throughout.
    let disabled = setup(setup_exe, "disable", b_label).and_then(|()| wait_for("offline B", || session.snapshot().outputs_offline[1]));
    // Even an incomplete disable can leave a pending device change. Re-enable before propagating any error.
    setup(setup_exe, "enable", b_label)?;
    disabled?;
    wait_for("B's automatic reattachment", || llcable_sdk::diagnostics(b).is_ok_and(|d| d.attached))?;
    let returned = listen(&patch)?;
    let after_attaches = attach_count(patch.ids[0])?;
    let status = session.snapshot();
    let sent = player.finish(true)?;
    let a = reader.finish(true)?;
    if sent.frames == seconds * RATE {
        return Err("Source reached its duration limit before the control sequence ended. Increase the duration.".into());
    }
    println!("A attach count {attaches} -> {after_attaches}. B disabled output had {} silent frames.", silent.frames.len());
    let results = [
        check_dry("A across all edits", &a, 1., sent.frames, true, &patch.marks),
        check_dry("B before edit", &before, 0.25, sent.frames, false, &patch.marks),
        check_dry("B after edit", &edited, 0.5, sent.frames, false, &patch.marks),
        check_dry("B after device return", &returned, 0.5, sent.frames, false, &patch.marks),
    ];
    if after_attaches != attaches || !status.running || status.outputs_offline.contains(&true) {
        return Err(status.error.unwrap_or("A was reattached or a destination ended offline.".into()));
    }
    results.into_iter().collect()
}

fn silent(segment: &Recording, label: &str) -> Result<(), String> {
    println!("{label}: {} frames recorded.", segment.frames.len());
    if segment.frames.len() < RATE / 2 || segment.frames.iter().any(|&frame| frame != [0.; 2]) {
        return Err(format!("Expected at least half a second of exact silence on {label}."));
    }
    Ok(())
}

fn clocked_levels(recording: &Recording) -> Result<[f64; 2], String> {
    // The playback endpoint may change the level or sample rate. Compare the two gains on this same output.
    let mut sum = [0_f64; 2];
    let mut peak = 0_f32;
    let mut matching = 0;
    for &[l, r] in &recording.frames {
        matching += usize::from(l > 0. && r < 0.);
        sum[0] += f64::from(l);
        sum[1] -= f64::from(r);
        peak = peak.max(l.abs()).max(r.abs());
    }
    let n = recording.frames.len();
    let mean = sum.map(|s| s / n.max(1) as f64);
    println!("WASAPI playback: {matching}/{n} frames with the expected stereo polarity, channel means {mean:?}, peak {peak:.4}.");
    if n < RATE / 2 || matching * 100 < n * 95 || !peak.is_finite() || mean.iter().any(|&m| !m.is_finite() || m < 0.001) {
        return Err("The WASAPI destination did not receive the coded stereo signal.".into());
    }
    Ok(mean)
}

pub fn destinations(patch: Patch, source_player: &str, seconds: usize, external: Option<(&str, &str)>) -> Result<(), String> {
    if seconds < 6 {
        return Err("Use at least six seconds to cover adding and removing a destination.".into());
    }
    let [a, b] = patch.ids;
    let layout = |include_b, playback_gain| {
        let mut config = patch.config(0.25, vec![]);
        if include_b {
            // B is added at the front, so A's meter and live writer must follow its ID across both index changes.
            config.cables.swap(0, 1);
            config.output_buses.swap(0, 1);
            if let Some((playback, _)) = external {
                use llcable_routing::{Bus, Send, Tap};
                config.playback.push((playback.to_owned(), config.buses.len()));
                config.buses.push(Bus { sends: vec![Send { from: Tap::Input(0), gain: playback_gain, pan: 0. }], ..Bus::default() });
            }
        } else {
            config.cables.truncate(1);
            config.output_buses.truncate(1);
            config.buses.truncate(2);
        }
        config
    };
    let session = Session::start(layout(false, 1.0))?;
    let reader_a = record(patch.readers[0].clone(), patch.marks.clone(), seconds)?;
    let reader_b = record(patch.readers[1].clone(), patch.marks.clone(), seconds)?;
    let external_reader = external.map(|(_, id)| record(id.to_owned(), patch.marks.clone(), seconds)).transpose()?;
    let player = play(source_player.to_owned(), patch.marks.clone(), seconds)?;
    let attaches = attach_count(a)?;
    if session.cable_enabled(b).is_some() || llcable_sdk::diagnostics(b).map_err(|e| e.to_string())?.attached {
        return Err("B is already configured or has an active feed at the start of the test.".into());
    }
    // Playback submits one second of startup silence. Keep the source active across both edits, and leave more than half
    // a second of signal in each steady window to distinguish a real feed and a real release from an idle source.
    thread::sleep(Duration::from_millis(1500));
    let window = || {
        let start = patch.marks.now();
        thread::sleep(Duration::from_millis(800));
        (start, patch.marks.now())
    };
    let before = window();
    let a_frames = session.snapshot().outputs[0].frames;
    if let Some((_, recording_id)) = external {
        // The new cable is prepared successfully before the playback worker rejects this recording-only endpoint.
        // Rollback must release B and leave the original A graph and its live stream intact.
        let mut invalid = layout(true, 1.0);
        invalid.playback[0].0 = recording_id.to_owned();
        let error = session.reconfigure(invalid).err().ok_or("A recording-only endpoint was accepted as a playback destination.")?;
        if session.snapshot().outputs.len() != 1
            || session.cable_enabled(b).is_some()
            || llcable_sdk::diagnostics(b).map_err(|e| e.to_string())?.attached
        {
            return Err("The refused edit changed the running graph or left its new cable attached.".into());
        }
        println!("Refused playback edit released its tentative cable: {error}");
    }
    session.reconfigure_retaining(layout(true, 1.0), vec![(0, 0), (1, 1)])?;
    wait_for("B's newly added feed", || llcable_sdk::diagnostics(b).is_ok_and(|d| d.attached))?;
    // A brief settling interval applies to B's windows. A's full-clip check includes the transition samples.
    thread::sleep(Duration::from_millis(100));
    let reference = if external.is_some() {
        let reference = window();
        session.reconfigure_retaining(layout(true, 0.25), vec![(0, 0), (1, 1), (2, 2)])?;
        thread::sleep(Duration::from_millis(100));
        Some(reference)
    } else {
        None
    };
    let middle = window();
    let added = session.snapshot();
    if added.outputs.len() != 2 + usize::from(external.is_some())
        || added.outputs_offline.contains(&true)
        || added.outputs[1].frames < a_frames
        || session.cable_enabled(b) != Some(true)
    {
        return Err("Adding destinations lost A's accumulated meter or failed to publish all active outputs.".into());
    }
    session.reconfigure_retaining(layout(false, 1.0), vec![(0, 0), (1, 1)])?;
    wait_for("B's removal to release its feed", || llcable_sdk::diagnostics(b).is_ok_and(|d| !d.attached))?;
    thread::sleep(Duration::from_millis(100));
    let after = window();
    let status = session.snapshot();
    let after_attaches = attach_count(a)?;
    let sent = player.finish(true)?;
    let (rec_a, rec_b) = (reader_a.finish(true)?, reader_b.finish(true)?);
    let rec_external = external_reader.map(|r| r.finish(true)).transpose()?;
    if sent.frames == seconds * RATE {
        return Err("Source expired before the edits finished. Increase the duration.".into());
    }
    if !status.running
        || status.outputs_offline != [false]
        || status.outputs.len() != 1
        || status.outputs[0].frames < added.outputs[1].frames
        || session.cable_enabled(b).is_some()
        || after_attaches != attaches
    {
        return Err(status.error.unwrap_or_else(|| "Removing B lost A's feed, meter, or attachment.".into()));
    }
    let mut result = check_dry("Continuous output through both edits", &rec_a, 1., sent.frames, true, &patch.marks);
    let (start, end) = middle;
    result = result.and(check_dry("New output while configured", &rec_b.during(start, end), 0.25, sent.frames, false, &patch.marks));
    if let (Some(recording), Some((first, last))) = (&rec_external, reference) {
        let unity = clocked_levels(&recording.during(first, last))?;
        let quarter = clocked_levels(&recording.during(start, end))?;
        let ratios = [quarter[0] / unity[0], quarter[1] / unity[1]];
        println!("WASAPI quarter/unity gain measured on the same endpoint: {ratios:.4?}.");
        if ratios.iter().any(|r| !(0.22..=0.28).contains(r)) {
            return Err("The WASAPI gain change did not reach both recording channels.".into());
        }
    }
    for (phase, (start, end)) in [("before add", before), ("after removal", after)] {
        result = result.and(silent(&rec_b.during(start, end), &format!("Second cable {phase}")));
        if let Some(recording) = &rec_external {
            result = result.and(silent(&recording.during(start, end), &format!("WASAPI output {phase}")));
        }
    }
    println!("Continuous output's driver attach count {attaches} -> {after_attaches}.");
    result
}
