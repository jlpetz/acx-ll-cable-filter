//! One capture goes out dry, one feed has a gain stage, and a stream mix ducks stereo music when the voice is present.
//! Use the same graph for every packet. A Windows host writes each returned slice to its selected cable or audio device.

use llcable_routing::{BusSpec, Ducking, EffectSpec, GraphSpec, Input, Send, Tap};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mic = Tap::Input(0);
    let music = Tap::Input(1);
    let graph = GraphSpec {
        input_count: 2,
        buses: vec![
            BusSpec { sends: vec![Send::unity(mic)], effects: vec![] },
            BusSpec { sends: vec![Send::unity(Tap::Bus(0))], effects: vec![EffectSpec::Gain(0.25)] },
            BusSpec { sends: vec![Send::unity(music)], effects: vec![EffectSpec::Duck { key: mic, settings: Ducking::default() }] },
            BusSpec { sends: vec![Send::unity(Tap::Bus(1)), Send::unity(Tap::Bus(2))], effects: vec![] },
        ],
    };
    let mut router = graph.build(48_000, 480)?;
    let voice = [0.4; 128];
    let music = [[0.6, -0.2]; 128];
    router.process(&[Input::Mono(&voice), Input::Stereo(&music)])?;
    for (name, bus) in [("Dry voice", 0), ("Quiet voice", 1), ("Stream mix", 3)] {
        println!("{name}: {:?}", router.output(bus).unwrap().last().unwrap());
    }
    Ok(())
}
