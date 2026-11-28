//! Acceptance through real endpoint I/O, never invoked by cargo test. A virtual source's playback receives a repeated,
//! asymmetric stereo sequence, representable even in 16-bit PCM. Each recording must contain its own expected gain and order.
use std::time::{Duration, Instant};

use llcable_wasapi::Capture;
use llcable_wasapi::sched::{Com, Mmcss, MmcssPriority};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};

const CYCLE: usize = 251;

fn pattern(t: usize) -> [f32; 2] {
    let x = t % CYCLE;
    [(x + 1) as f32 / 1024., -(((67 * x + 19) % CYCLE + 1) as f32) / 1024.]
}

fn compare(samples: &[[f32; 2]], gain: f32, seconds: u64) -> Result<(usize, usize, usize), String> {
    let mut checked = 0;
    let mut exact = 0;
    let mut in_order = 0;
    let mut previous = None;
    for &[l, r] in samples {
        // Lock to the transmitted pattern, ignoring initial silence / the shared playback engine's fade-in.
        let phase = (l * 1024. / gain).round() as i32 - 1;
        let matches = if (0..CYCLE as i32).contains(&phase) {
            let expected = pattern(phase as usize);
            l == expected[0] * gain && r == expected[1] * gain
        } else {
            false
        };
        if matches || checked > 0 {
            checked += 1;
            if matches {
                exact += 1;
                if Some(phase as usize) == previous {
                    in_order += 1;
                }
                previous = Some((phase as usize + 1) % CYCLE);
            } else {
                previous = None;
            }
        }
    }
    if checked < (seconds as usize - 1) * 48_000 || exact * 100 < checked * 99 || in_order * 100 < checked * 99 {
        return Err(format!(
            "Expected gain {gain}: {checked} recorded frames from first match, {exact} exact stereo samples, {in_order} consecutive."
        ));
    }
    Ok((checked, exact, in_order))
}

struct Reader {
    capture: Capture,
    buffer: Box<[[f32; 2]]>,
    recording: Vec<[f32; 2]>,
}

pub(super) fn run(playback: &str, outputs: &[(String, f32)], seconds: u64) -> Result<(), String> {
    let _com = Com::mta().map_err(|h| format!("COM initialization: {h:#x}"))?;
    let device = DeviceEnumerator::new().and_then(|e| e.get_device(playback)).map_err(|e| e.to_string())?;
    if device.get_direction() != Direction::Render {
        return Err("--verify-playback needs the virtual source's playback endpoint ID.".into());
    }
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    client
        .initialize_client(&format, &Direction::Render, &StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 500_000 })
        .map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    let mut bytes = vec![0; 8 * client.get_buffer_size().map_err(|e| e.to_string())? as usize];
    let mut cursor = 0usize;
    let mut top_up = || -> Result<(), String> {
        let count = client.get_available_space_in_frames().map_err(|e| e.to_string())? as usize;
        for frame in bytes[..count * 8].as_chunks_mut::<8>().0 {
            let [l, r] = pattern(cursor);
            frame[..4].copy_from_slice(&l.to_le_bytes());
            frame[4..].copy_from_slice(&r.to_le_bytes());
            cursor += 1;
        }
        render.write_to_device(count, &bytes[..count * 8], None).map_err(|e| e.to_string())
    };
    let mut readers = Vec::new();
    for (id, _) in outputs {
        let capture = Capture::open(id)?;
        let capacity = capture.capacity();
        let recording = Vec::with_capacity(48_000 * (seconds as usize + 1));
        readers.push(Reader { capture, buffer: vec![[0.; 2]; capacity].into_boxed_slice(), recording });
    }
    let handles: Vec<_> = readers.iter().map(|r| r.capture.event()).collect();
    if !(1..=64).contains(&handles.len()) {
        return Err("Select between 1 and 64 verification destinations.".into());
    }
    top_up()?;
    for reader in &readers {
        reader.capture.start().map_err(|e| e.to_string())?;
    }
    client.start_stream().map_err(|e| e.to_string())?;
    let mmcss = Mmcss::enter(MmcssPriority::Normal);
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        // SAFETY: every handle is owned by a running capture until the loop finishes.
        let wake = unsafe { WaitForMultipleObjects(&handles, false, 5) };
        if wake == WAIT_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        top_up()?;
        for (i, reader) in readers.iter_mut().enumerate() {
            if reader.capture.info().exclusive
                && wake.0 != WAIT_OBJECT_0.0 + i as u32
                && unsafe { WaitForSingleObject(handles[i], 0) } != WAIT_OBJECT_0
            {
                continue;
            }
            while let Some(p) = reader.capture.read(&mut reader.buffer).map_err(|e| e.to_string())? {
                if reader.recording.len() + p.frames <= reader.recording.capacity() {
                    reader.recording.extend_from_slice(&reader.buffer[..p.frames]);
                }
                if reader.capture.info().exclusive {
                    break;
                }
            }
        }
    }
    drop(mmcss);
    client.stop_stream().map_err(|e| e.to_string())?;
    for (reader, (_, gain)) in readers.iter().zip(outputs) {
        reader.capture.stop();
        let (frames, exact, ordered) = compare(&reader.recording, *gain, seconds)?;
        println!("Gain {gain}: {exact}/{frames} exact stereo frames, {ordered} consecutive.");
    }
    Ok(())
}
