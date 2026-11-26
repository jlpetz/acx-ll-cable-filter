use super::scene::{Kind, Scene, Scope, Stage, Tap};

fn kinds(stages: &[Stage]) -> Vec<Kind> {
    stages.iter().map(|stage| stage.kind).collect()
}

#[test]
fn a_dry_send_and_a_processed_copy_can_share_an_input_while_a_second_source_joins_the_mix() {
    let mut scene = Scene::default();
    let (mic, playback) = (0, 1);
    let (obs, discord) = (0, 1);
    assert!(scene.connected(mic, obs) && scene.connected(mic, discord) && scene.connected(playback, obs));
    assert!(!scene.connected(playback, discord));

    scene.scope = Scope::Source(mic);
    scene.add(Kind::Compressor);
    assert_eq!(scene.sends[mic][obs].tap, Tap::Raw);
    assert_eq!(scene.sends[mic][discord].tap, Tap::End);

    scene.scope = Scope::Send(mic, discord);
    scene.add(Kind::Voice);
    scene.scope = Scope::Output(obs);
    scene.add(Kind::Gain);
    assert_eq!(kinds(scene.at(Scope::Source(mic))), [Kind::Compressor]);
    assert_eq!(kinds(scene.at(Scope::Send(mic, discord))), [Kind::Echo, Kind::Noise, Kind::Voice]);
    assert!(scene.at(Scope::Send(mic, obs)).is_empty());
    assert!(scene.at(Scope::Send(playback, obs)).is_empty());
    assert_eq!(kinds(scene.at(Scope::Output(obs))), [Kind::Gain]);
    assert!(scene.at(Scope::Output(discord)).is_empty());
    assert_eq!(scene.sends[playback][obs].gain_db, -9);
}

#[test]
fn a_tap_follows_its_stage_and_falls_back_to_the_preceding_boundary_after_removal() {
    let mut scene = Scene::default();
    scene.scope = Scope::Source(0);
    let noise = scene.add(Kind::Noise);
    let eq = scene.add(Kind::Equalizer);
    let pan = scene.add(Kind::Pan);
    scene.sends[0][1].tap = Tap::Stage(eq);
    scene.move_stage(eq, 1);
    assert_eq!(scene.stages().iter().map(|s| s.id).collect::<Vec<_>>(), [noise, pan, eq]);
    assert_eq!(scene.sends[0][1].tap, Tap::Stage(eq));
    assert_eq!(scene.taps(0).last().unwrap().0, Tap::Stage(eq));

    scene.remove(eq);
    assert_eq!(scene.sends[0][1].tap, Tap::Stage(pan));
    scene.remove(pan);
    assert_eq!(scene.sends[0][1].tap, Tap::Stage(noise));
    scene.remove(noise);
    assert_eq!(scene.sends[0][1].tap, Tap::Raw);
    assert_eq!(scene.sends[0][0].tap, Tap::Raw);
    assert_eq!(scene.sends[1][0].tap, Tap::End);
}

#[test]
fn disconnecting_and_reconnecting_a_send_preserves_its_tap_gain_and_pipeline() {
    let mut scene = Scene::default();
    scene.scope = Scope::Source(0);
    let tap = Tap::Stage(scene.add(Kind::Echo));
    scene.scope = Scope::Send(0, 1);
    scene.sends[0][1].tap = tap;
    scene.sends[0][1].gain_db = -6;
    let gate = scene.add(Kind::Gate);
    scene.stage_mut(gate).unwrap().value = -42;

    scene.toggle(0, 1);
    assert!(!scene.connected(0, 1));
    assert_eq!(scene.scope, Scope::Output(1));
    assert!(scene.connected(0, 0));
    scene.toggle(0, 1);
    assert!(scene.connected(0, 1));
    assert_eq!((scene.sends[0][1].gain_db, scene.sends[0][1].tap), (-6, tap));
    scene.scope = Scope::Send(0, 1);
    let restored = scene.stage_mut(gate).unwrap();
    assert_eq!((restored.kind, restored.value, restored.enabled), (Kind::Gate, -42, true));
}

#[test]
fn loading_a_pipeline_preset_creates_independent_stage_instances_and_keeps_routing() {
    let mut scene = Scene::default();
    scene.scope = Scope::Output(0);
    let bass = scene.add(Kind::Equalizer);
    scene.stage_mut(bass).unwrap().value = 3;
    let second = scene.add(Kind::Equalizer);
    let bypassed = scene.stage_mut(second).unwrap();
    bypassed.value = -5;
    bypassed.enabled = false;
    let preset = scene.stages().to_vec();
    let routes = |scene: &Scene| scene.sends.map(|destinations| destinations.map(|send| (send.connected, send.gain_db, send.tap)));
    let before = routes(&scene);

    scene.scope = Scope::Send(0, 1);
    let replaced = scene.stages()[0].id;
    scene.load_stack(&preset);
    let copies = scene.stages().to_vec();
    assert_eq!(kinds(&copies), [Kind::Equalizer, Kind::Equalizer]);
    assert!(copies.iter().all(|copy| preset.iter().all(|original| original.id != copy.id)));
    assert_ne!(copies[0].id, copies[1].id);
    assert!(scene.stage_mut(replaced).is_none());
    assert_eq!(copies.iter().map(|s| (s.value, s.enabled)).collect::<Vec<_>>(), [(3, true), (-5, false)]);
    scene.stage_mut(copies[0].id).unwrap().value = 10;
    assert_eq!(preset[0].value, 3);
    assert_eq!(scene.at(Scope::Output(0))[0].value, 3);
    assert_eq!(routes(&scene), before);
}
