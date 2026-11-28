use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use llcable_audio::Session;
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

pub fn run(patch: Patch, source_player: &str, b_label: &str, setup_exe: &str, seconds: usize) -> Result<(), String> {
    let b = patch.ids[1];
    let session = Session::start(patch.config(0.25, vec![]))?;
    let reader = record(patch.readers[0].clone(), patch.marks.clone(), seconds)?;
    let player = play(source_player.to_owned(), patch.marks.clone(), seconds)?;
    let count = || {
        Device::open_path(&control_path(&patch.ids[0]))
            .and_then(|device| device.stats())
            .map(|stats| stats.ring.attaches)
            .map_err(|e| e.to_string())
    };
    let attaches = count()?;
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
    let after_attaches = count()?;
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
