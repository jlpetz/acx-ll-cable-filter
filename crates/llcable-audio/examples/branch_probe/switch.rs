use std::thread;
use std::time::Duration;

use llcable_audio::Session;

use super::signal::RATE;
use super::streams::{play, record};
use super::{Patch, check_dry, check_rnnoise};

pub fn run(patch: Patch, player: &str, seconds: usize) -> Result<(), String> {
    if seconds < 18 {
        return Err("Use at least 18 seconds for six live timing windows.".into());
    }
    let session = Session::start_with_initializer(patch.config(1., vec![]), llcable_effects::prepare_voice_thread)?;
    let a = record(patch.readers[0].clone(), patch.marks.clone(), seconds)?;
    let b = record(patch.readers[1].clone(), patch.marks.clone(), seconds)?;
    let player = play(player.to_owned(), patch.marks.clone(), seconds)?;
    thread::sleep(Duration::from_millis(1500));
    let mut epochs = Vec::new();
    for i in 0..6 {
        let wet = i % 2 == 1;
        if i > 0 {
            let effects = if wet { vec![llcable_effects::noise_reduction()] } else { vec![] };
            session.reconfigure(patch.config(1., effects))?;
        }
        // Keep control handover, effect warmup and audio already in the rings outside the per-condition statistics.
        // A's full-clip continuity check below includes all five transitions.
        thread::sleep(Duration::from_millis(300));
        let start = patch.marks.now();
        thread::sleep(Duration::from_secs(2));
        epochs.push((wet, start, patch.marks.now()));
    }
    let status = session.snapshot();
    let sent = player.finish(true)?;
    let (a, b) = (a.finish(true)?, b.finish(true)?);
    if !status.running || status.outputs_offline.contains(&true) || sent.frames == seconds * RATE {
        return Err(status.error.unwrap_or("A destination stopped or the source expired before the final window.".into()));
    }
    let periods = (sent.period, status.inputs[0].period_frames, a.period, b.period);
    println!("Continuous A/B. Playback/capture/A/B periods {periods:?}, source target {}, {} frames sent.", sent.fill, sent.frames);
    let mut result = check_dry("A including all transitions", &a, 1., sent.frames, true, &patch.marks);
    for (i, (wet, start, end)) in epochs.into_iter().enumerate() {
        println!("Window {}, B {}.", i + 1, if wet { "RNNoise" } else { "dry" });
        let (a, b) = (a.during(start, end), b.during(start, end));
        result = result.and(check_dry("A", &a, 1., sent.frames, false, &patch.marks));
        result = result.and(if wet { check_rnnoise(&b, b.frames.len()) } else { check_dry("B", &b, 1., sent.frames, false, &patch.marks) });
    }
    result
}
