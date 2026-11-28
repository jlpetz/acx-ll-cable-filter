//! `cargo run -p llcable-audio --example route -- --input <ID-or-cable-label> --output A --output B:0.25 --seconds 15`.
//! Extra `--input` arguments are mixed in order. The first input clocks the patch. A recording client on each destination
//! is needed to read its live ring. `--authorize` uses the optional `llcable-broker.exe` installed beside the example.
//! To check the actual recorded samples, add `--verify-playback <virtual source cable's playback endpoint ID>`.
//! A shared playback output instead uses `--playback-output <render ID> <gain>`; pass its paired capture ID
//! with `--verify-output <ID>` when checking that route on a virtual device.
#[path = "route/verify.rs"]
mod verify;
use std::time::Duration;

use llcable_audio::{RouteConfig, Session};
use llcable_routing::{Bus, Send, Tap};
use llcable_sdk::{CableInfo, EndpointKind};

fn main() -> Result<(), String> {
    let inventory = llcable_sdk::cables().map_err(|e| e.to_string())?;
    let cable = |name: &str| -> Result<&CableInfo, String> {
        inventory
            .iter()
            .find(|c| c.label.eq_ignore_ascii_case(name) || name.parse().ok() == Some(c.id))
            .ok_or_else(|| format!("Unknown cable {name}"))
    };
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut playback_outputs = Vec::new();
    let mut verify_outputs = Vec::new();
    let mut seconds = 10;
    let mut verify_playback = None;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        match option.as_str() {
            "--input" => {
                let name = args.next().ok_or("Missing --input endpoint ID or cable label.")?;
                inputs.push(if let Ok(c) = cable(&name) {
                    format!("acx-ll-cable:{}/{}", c.id, EndpointKind::Microphone.arg())
                } else {
                    name
                });
            }
            "--output" => {
                let value = args.next().ok_or("Missing --output cable label.")?;
                let (name, gain) = value.split_once(':').unwrap_or((&value, "1"));
                outputs.push((cable(name)?.id, gain.parse::<f32>().map_err(|_| "Invalid gain.")?));
            }
            "--seconds" => seconds = args.next().ok_or("Missing duration.")?.parse::<u64>().map_err(|_| "Invalid duration.")?,
            "--playback-output" => {
                let id = args.next().ok_or("Missing playback endpoint ID.")?;
                let gain = args.next().ok_or("Missing playback gain.")?.parse::<f32>().map_err(|_| "Invalid gain.")?;
                playback_outputs.push((id, gain));
            }
            "--verify-output" => verify_outputs.push(args.next().ok_or("Missing playback destination's capture endpoint ID.")?),
            "--authorize" => llcable_sdk::authorize().map_err(|e| e.to_string())?,
            "--verify-playback" => verify_playback = Some(args.next().ok_or("Missing virtual playback endpoint ID.")?),
            _ => {
                return Err(format!("Unknown option {option}. Use --input ID-or-label --output label[:gain] [--seconds N] [--authorize]."));
            }
        }
    }
    if verify_playback.is_some()
        && (inputs.len() != 1
            || !(2..=120).contains(&seconds)
            || verify_outputs.len() != playback_outputs.len()
            || outputs.iter().map(|(_, g)| g).chain(playback_outputs.iter().map(|(_, g)| g)).any(|g| !g.is_finite() || *g <= 0.))
    {
        return Err("Sample verification needs one source, positive output gains, and a duration from 2 to 120 seconds.".into());
    }
    let buses: Vec<_> = outputs
        .iter()
        .map(|(_, g)| g)
        .chain(playback_outputs.iter().map(|(_, g)| g))
        .map(|gain| Bus { sends: (0..inputs.len()).map(|i| Send { from: Tap::Input(i), gain: *gain, pan: 0. }).collect(), effects: vec![] })
        .collect();
    let mut session = Session::start(RouteConfig {
        inputs,
        output_buses: (0..outputs.len()).collect(),
        cables: outputs.iter().map(|t| t.0).collect(),
        playback: playback_outputs.iter().enumerate().map(|(i, (id, _))| (id.clone(), outputs.len() + i)).collect(),
        buses,
    })?;
    if let Some(playback) = verify_playback {
        let mut recordings = outputs
            .iter()
            .map(|(id, gain)| {
                let port = inventory
                    .iter()
                    .find(|c| c.id == *id)
                    .and_then(|c| c.ports.iter().find(|p| p.role == EndpointKind::Microphone && p.active))
                    .ok_or("A destination has no active low-latency Out. Enable it or select a cable with that role.")?;
                Ok((port.id.clone(), *gain))
            })
            .collect::<Result<Vec<_>, String>>()?;
        recordings.extend(verify_outputs.into_iter().zip(playback_outputs.iter().map(|(_, g)| *g)));
        verify::run(&playback, &recordings, seconds)?;
        let status = session.snapshot();
        if !status.running || status.outputs_offline.contains(&true) {
            return Err(status.error.unwrap_or("A destination stopped.".into()));
        }
        return Ok(());
    }
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        let s = session.snapshot();
        println!("inputs {:?} outputs {:?}", s.inputs, s.outputs);
        if !s.running {
            return Err(s.error.unwrap_or_else(|| "The routing worker stopped.".into()));
        }
        if s.outputs_offline.contains(&true) {
            return Err("A cable has detached.".into());
        }
    }
    session.stop();
    Ok(())
}
