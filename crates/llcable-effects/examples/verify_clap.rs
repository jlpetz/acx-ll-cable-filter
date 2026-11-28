//! Exercise the v2 graph, public CLAP constructors and normal sibling `nf-fx-host.exe` lookup using the in-tree gain fixture.
//! Build `cargo build -p nf-fx --bin nf-fx-host --example gain` and
//! `cargo build -p llcable-effects --features clap --example verify_clap`. In a directory readable by the restricted worker,
//! stage `verify_clap.exe` next to `nf-fx-host.exe` and `gain.clap` (the built `gain.dll`). Run `verify_clap.exe gain.clap`.
//! From a separate directory containing the probe alone, `verify_clap.exe --missing-host <absolute path to gain.clap>` checks recovery.
//! No sound device or cable is opened.

use std::path::Path;
use std::thread;
use std::time::Duration;

use llcable_effects::{clap_voice, inspect_clap};
use llcable_routing::{Bus, Input, Router, Send, Tap};
use nf_core::{BLOCK, SAMPLE_RATE};

const ID: &str = "org.noise-filter.gain";
const PACKET: usize = 128;
const HOPS: usize = 24;

fn input(i: usize) -> [f32; 2] {
    // Binary fractions preserve exact gain values; different periods are long enough to distinguish frame offsets.
    let left = ((i * 13 % 503) as f32 - 251.) / 1024.;
    let right = ((i * 31 % 499) as f32 - 249.) / 1024.;
    [left, right]
}

fn verify_gain(path: &Path, stages: &[f64]) -> Result<(), Box<dyn std::error::Error>> {
    let effects = stages.iter().map(|&gain| clap_voice(path, Some(ID), &[(0, gain)])).collect::<Result<Vec<_>, _>>()?;
    let (effects, meters): (Vec<_>, Vec<_>) = effects.into_iter().map(|fx| fx.observe()).unzip();
    let source = Send::unity(Tap::Input(0));
    let mut graph =
        Router::new(SAMPLE_RATE, PACKET, 1, vec![Bus { sends: vec![source], effects: vec![] }, Bus { sends: vec![source], effects }])?;
    graph.prepare_thread();
    let lag = (1 + stages.len()) * BLOCK; // one adapter shared by adjacent models plus one worker response per stage
    let net_gain = stages.iter().product::<f64>() as f32;
    let mut i = 0usize;
    let mut verified = 0;
    while i < HOPS * BLOCK {
        let n = PACKET.min(HOPS * BLOCK - i);
        let packet: [_; PACKET] = std::array::from_fn(|j| input(i + j));
        graph.process(&[Input::Stereo(&packet[..n])])?;
        assert_eq!(graph.output(0).unwrap(), &packet[..n], "dry stereo split at {i}");
        for (j, &wet) in graph.output(1).unwrap().iter().enumerate() {
            let expected = if i + j < lag {
                0.
            } else {
                let [left, right] = input(i + j - lag);
                verified += 1;
                ((left + right) * 0.5) * net_gain
            };
            assert_eq!(wet, [expected; 2], "sample {}, gains {stages:?}, expected delay {lag}", i + j);
        }
        if (i + n) / BLOCK > i / BLOCK {
            // The graph's real-time process never waits. A synthetic producer must give the real helper time to return each block.
            thread::sleep(Duration::from_millis(25));
        }
        i += n;
    }
    println!("CLAP {stages:?}: {verified} exact wet frames, delay {lag} frames, {} exact dry frames", HOPS * BLOCK);
    assert!(meters.iter().all(|m| m.read().worker_misses == Some(0)), "paced gains should deliver each hop");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next().ok_or("Supply the path to the project's gain.clap")?;
    let (missing, path) = if first == "--missing-host" { (true, args.next().ok_or("Missing path to gain.clap")?) } else { (false, first) };
    let exe = std::env::current_exe()?;
    let helper = exe.with_file_name("nf-fx-host.exe");
    let path = Path::new(&path).canonicalize()?;
    if missing {
        if helper.exists() {
            return Err("Stage the probe alone for the missing-helper check".into());
        }
        let error = clap_voice(&path, Some(ID), &[]).err().ok_or("a CLAP loaded without the adjacent worker")?;
        if !error.contains("Start restricted effect helper") {
            return Err(format!("Expected an unavailable-helper error, got {error}").into());
        }
        println!("Missing helper reported at construction: {error}");
        return Ok(());
    }
    if !helper.is_file() {
        return Err("Stage the built nf-fx-host.exe alongside the probe before running".into());
    }
    let info = inspect_clap(&path, None)?;
    assert_eq!(info.id, ID);
    assert_eq!(info.name, "Gain");
    assert_eq!(info.params.len(), 1);
    assert_eq!((info.params[0].id, info.params[0].min, info.params[0].max), (0, 0., 2.));
    let error = clap_voice(&path, Some(ID), &[(99, 0.5)]).err().ok_or("unknown parameter 99 was accepted")?;
    assert!(error.contains("Invalid parameter 99"), "{error}");
    verify_gain(&path, &[0.5])?;
    verify_gain(&path, &[0.25, 0.5])
}
