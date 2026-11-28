//! Standalone Cargo workspace. Enumerate installed cables and split one stereo packet without starting audio or loading effects.
use llcable_routing::{BusSpec, EffectSpec, GraphSpec, Input, Send, Tap};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for cable in llcable_sdk::cables()? {
        println!("LLCable-{}: {} endpoints, {}", cable.label, cable.ports.len(), cable.health);
    }
    let graph = GraphSpec {
        input_count: 1,
        buses: vec![BusSpec { sends: vec![Send::unity(Tap::Input(0))], effects: vec![] }, BusSpec {
            sends: vec![Send::unity(Tap::Bus(0))],
            effects: vec![EffectSpec::Gain(0.25)],
        }],
    };
    let mut router = graph.build(48_000, 128)?;
    let stereo = [[0.5, -0.25]; 128];
    router.process(&[Input::Stereo(&stereo)])?;
    assert_eq!(router.output(0).unwrap(), &stereo);
    assert_eq!(router.output(1).unwrap(), &[[0.125, -0.0625]; 128]);
    println!("Both stereo outputs match.");
    Ok(())
}
