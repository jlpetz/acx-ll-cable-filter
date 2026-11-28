use super::*;

fn mixer() -> Mixer {
    Mixer::new(Appearance::default())
}

fn assert_silent(levels: &[f32]) {
    assert!(!levels.is_empty());
    assert!(levels.iter().all(|&peak| peak == f32::NEG_INFINITY), "{levels:?}");
}

#[test]
fn compact_spacing_folds_auto_meters_and_honours_explicit_stereo() {
    use meters::{MeterMode, MeterPalette};
    let compact = Appearance::default();
    assert!(compact.compact);
    assert_eq!(compact.palette, MeterPalette::BlueFloor);
    let normal = Appearance { compact: false, ..compact };
    assert_eq!(normal.meter_mode(), MeterMode::Stereo);
    assert_eq!(compact.meter_mode(), MeterMode::Mono);
    assert_eq!(Appearance { mode: MeterMode::Stereo, ..compact }.meter_mode(), MeterMode::Stereo);
}

#[test]
fn closing_the_output_inspector_and_opening_another_outputs_actions_preserves_every_send() {
    let mut app = mixer();
    let before = app.output_levels(1);
    app.apply(Msg::PickOutput(1));
    assert!(!app.focus_open);
    app.apply(Msg::OutputActions(Some(0)));
    assert_eq!(app.output_actions, Some(0));
    assert!(!app.focus_open);
    app.apply(Msg::InspectOutput(0));
    assert!(matches!(app.panel, Some(Panel::Diagnostics)));
    assert_eq!(app.output, 0);
    assert!(app.output_actions.is_none());
    app.apply(Msg::Back);
    app.apply(Msg::PickOutput(1));
    assert!(app.focus_open);
    app.apply(Msg::AllSends(true));
    app.apply(Msg::PickOutput(1));
    assert!(app.focus_open && !app.all_sends);
    assert_eq!(app.output_levels(1), before);
}

#[test]
fn silence_reaches_all_measurements_and_restore_preserves_the_patch() {
    let mut app = mixer();
    app.apply(Msg::ConnectSend(1, 1));
    let before = app.output_levels(1);
    app.apply(Msg::DemoSignal(false));
    app.apply(Msg::Tick);
    for s in 0..SOURCES.len() {
        assert_silent(&app.input_levels(s));
    }
    assert_silent(&app.mix_levels_before_output(1));
    assert_silent(&app.output_levels(1));
    assert_eq!(app.tick, 0);
    app.apply(Msg::DemoSignal(true));
    assert_eq!(app.output_levels(1), before);
}

#[test]
fn removing_the_selected_wire_or_stage_only_edits_the_requested_path() {
    let mut app = mixer();
    let source = Scope::Source(0);
    let branch = Scope::Send(0, 1);
    let noise = app.scene.at(source)[0].id;
    app.apply(Msg::Edit(branch));
    app.apply(Msg::Add(Kind::Gain));
    let gain = app.selected_stage.unwrap();
    app.apply(Msg::Page(Page::Wiring));
    app.apply(Msg::SelectWire(Some((0, 1))));
    app.apply(Msg::DeleteStage(branch, noise));
    assert_eq!(app.scene.at(source)[0].id, noise);
    app.apply(Msg::DeleteStage(branch, gain));
    assert!(app.scene.at(branch).is_empty());
    assert!(matches!(app.page, Page::Wiring));
    app.apply(Msg::DisconnectSend(0, 1));
    app.apply(Msg::DisconnectSend(0, 1));
    assert!(app.selected_wire.is_none());
    assert!(!app.scene.connected(0, 1));
    assert!(app.scene.connected(0, 0));
    app.apply(Msg::EditStage(source, noise));
    assert!(matches!(app.page, Page::Effects));
    assert_eq!(app.selected_stage, Some(noise));
}

#[test]
fn mixer_and_wiring_share_the_same_sends() {
    let mut app = mixer();
    assert_eq!(app.output, 1);
    assert!(app.scene.connected(0, 0) && app.scene.connected(0, 1));
    assert_eq!(app.scene.sends[0][0].tap, Tap::Raw);
    assert!(!app.scene.connected(1, 1));
    let dry = app.output_levels(0);

    app.apply(Msg::ToggleSend(1, 1));
    app.apply(Msg::Gain(1, 1, -6));
    app.apply(Msg::Page(Page::Wiring));
    assert!(app.scene.connected(1, 1));
    assert_eq!(app.scene.sends[1][1].gain_db, -6);
    assert_eq!(app.output_levels(0), dry);
    app.apply(Msg::ToggleSend(1, 1));
    app.apply(Msg::Page(Page::Mixer));
    assert!(!app.scene.connected(1, 1));
    assert!(app.scene.connected(0, 1));
    assert_eq!(app.scene.sends[0][1].gain_db, 0);
}

#[test]
fn removing_a_shared_stage_moves_only_the_send_tapped_there() {
    let mut app = mixer();
    let shared = Scope::Source(0);
    app.apply(Msg::Edit(shared));
    let noise = app.scene.stages().last().expect("initial shared noise reduction").id;
    app.apply(Msg::Add(Kind::Gain));
    let gain = app.scene.at(shared).last().unwrap().id;
    app.apply(Msg::Tap(0, 1, Tap::Stage(gain)));
    app.apply(Msg::SetValue(gain, -9));
    app.apply(Msg::Bypass(gain));

    let stage = app.scene.at(shared).last().unwrap();
    assert_eq!(stage.value, -9);
    assert!(!stage.enabled);
    assert_eq!(app.scene.sends[0][0].tap, Tap::Raw);
    assert_eq!(app.scene.sends[0][1].tap, Tap::Stage(gain));
    app.apply(Msg::Remove(gain));
    assert_eq!(app.scene.sends[0][1].tap, Tap::Stage(noise));
    assert_eq!(app.scene.sends[0][0].tap, Tap::Raw);
    assert!(app.scene.connected(0, 0) && app.scene.connected(0, 1));
}

#[test]
fn output_meters_follow_the_route_mute_and_source_silence() {
    let mut app = mixer();
    let initial = app.output_levels(0);
    let other = app.output_levels(1);
    assert!(initial.iter().any(|db| db.is_finite()));
    assert_silent(&app.output_levels(2));

    app.apply(Msg::Mute(0));
    assert_silent(&app.output_levels(0));
    assert_eq!(app.output_levels(1), other);
    app.apply(Msg::Mute(0));
    assert_eq!(app.output_levels(0), initial);

    app.apply(Msg::ToggleSend(0, 0));
    assert_silent(&app.output_levels(0));
    assert_eq!(app.output_levels(1), other);
    app.apply(Msg::ToggleSend(0, 0));
    app.tick = 140; // Mic is in its silent interval while the two stereo sources still have a signal.
    assert_silent(&app.output_levels(0));
    assert_silent(&app.output_levels(1));
    app.apply(Msg::ToggleSend(1, 0));
    assert!(app.output_levels(0).iter().any(|db| db.is_finite()));
    assert_silent(&app.output_levels(1));
}

#[test]
fn inspecting_a_different_output_leaves_the_mix_intact() {
    let mut app = mixer();
    let first = app.output_levels(0);
    let second = app.output_levels(1);
    app.apply(Msg::Page(Page::Wiring));
    app.apply(Msg::Panel(Panel::Diagnostics));
    assert!(matches!(app.panel, Some(Panel::Diagnostics)));
    app.apply(Msg::Output(0));
    assert_eq!(app.output, 0);
    assert_eq!(app.output_levels(0), first);
    assert_eq!(app.output_levels(1), second);
}
