use windows_reactor::test::{
    Command, EventId, EventPayload, NodeId, PropertyId, PropertyValue, Pump, QueuedEvent, RecordingRuntime, SlotId,
};

use super::*;

type PreviewPump = Pump<RecordingRuntime>;

fn text_is(pump: &PreviewPump, node: NodeId, property: PropertyId, text: &str) -> bool {
    matches!(pump.runtime().node(node).and_then(|n| n.property(property)), Some(PropertyValue::Str(s)) if s == text)
}

fn nodes(pump: &PreviewPump) -> impl Iterator<Item = NodeId> + '_ {
    pump.runtime()
        .commands()
        .iter()
        .flatten()
        .filter_map(|cmd| if let Command::Create { node, .. } = cmd { Some(*node) } else { None })
        .filter(|id| pump.runtime().node(*id).is_some())
}

/// Look up a mounted button by its accessible name, or by its visible content when it has a short text label.
fn button(pump: &PreviewPump, label: &str) -> NodeId {
    nodes(pump)
        .find(|id| {
            pump.event_revision(*id, EventId::ButtonClick).is_some()
                && (text_is(pump, *id, PropertyId::AutomationName, label)
                    || pump
                        .runtime()
                        .node(*id)
                        .is_some_and(|n| n.children().iter().any(|&c| text_is(pump, c, PropertyId::TextBlockText, label))))
        })
        .unwrap_or_else(|| panic!("No mounted button named {label}"))
}

fn click(pump: &mut PreviewPump, label: &str) {
    let id = button(pump, label);
    dispatch(pump, id, EventId::ButtonClick, EventPayload::Unit);
}

fn dispatch(pump: &mut PreviewPump, id: NodeId, event: EventId, payload: EventPayload) {
    let revision = pump.event_revision(id, event).unwrap();
    pump.queue_event(QueuedEvent::new(id, event, revision, payload));
    pump.dispatch_events().unwrap();
    pump.dispatch_components(32).unwrap();
    pump.update_view(View::component::<Preview>(Concept::Matrix)).unwrap();
}

#[test]
fn two_cables_can_mix_into_a_third_and_split_to_a_second_destination() {
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(View::component::<Preview>(Concept::Matrix)).unwrap();
    click(&mut pump, "Disconnect Hardware mic from Cable A Output");
    click(&mut pump, "Send Cable D Input to Cable A Output");
    button(&pump, "Disconnect Cable C Input from Cable A Output");
    button(&pump, "Disconnect Cable D Input from Cable A Output");
    click(&mut pump, "Send Cable C Input to Cable B Output");
    click(&mut pump, "Edit processing for the Cable C Input to Cable B Output send");
    click(&mut pump, "+ Add stage");
    click(&mut pump, "Add Equalizer");
    button(&pump, "Edit stage 1, Equalizer");
    click(&mut pump, "Edit processing for the Cable C Input to Cable A Output send");
    assert!(nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "Dry signal · no stages")));
}

#[test]
fn a_new_send_and_its_pan_stage_follow_the_user_between_layouts() {
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(View::component::<Preview>(Concept::Matrix)).unwrap();
    click(&mut pump, "Send Hardware mic to Headphones");
    button(&pump, "Disconnect Hardware mic from Headphones");
    click(&mut pump, "Edit processing for the Hardware mic to Headphones send");
    click(&mut pump, "+ Add stage");
    click(&mut pump, "Add Stereo pan");
    button(&pump, "Edit stage 1, Stereo pan");

    for view in ["Signal paths", "Mixer strips", "Routing matrix"] {
        click(&mut pump, view);
        button(&pump, "Edit stage 1, Stereo pan");
    }
    click(&mut pump, "Disconnect Hardware mic from Headphones");
    button(&pump, "Send Hardware mic to Headphones");
}

#[test]
fn a_clap_voice_effect_can_be_reordered_tuned_bypassed_and_removed_from_one_branch() {
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(View::component::<Preview>(Concept::Matrix)).unwrap();
    click(&mut pump, "+ Add stage");
    click(&mut pump, "Add Voice tone");
    button(&pump, "Edit stage 3, Voice tone");
    click(&mut pump, "Move stage earlier");
    button(&pump, "Edit stage 2, Voice tone");
    button(&pump, "Edit stage 3, Noise reduction");
    click(&mut pump, "Bypass");
    button(&pump, "Enable");
    assert!(nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "CLAP · example plug-in · Bypassed")));

    let slider = nodes(&pump)
        .find(|&id| {
            pump.runtime()
                .node(id)
                .and_then(|n| n.slot(SlotId::SliderHeader))
                .is_some_and(|header| text_is(&pump, header, PropertyId::TextBlockText, "Pitch  ·  -3 semitones"))
        })
        .expect("voice effect's pitch slider");
    pump.runtime_mut().record_property_observation(slider, PropertyId::SliderValue, PropertyValue::F64(-7.0)).unwrap();
    dispatch(&mut pump, slider, EventId::SliderValueChanged, EventPayload::F64(-7.0));
    assert!(nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "Pitch  ·  -7 semitones")));

    click(&mut pump, "Edit processing for the Hardware mic to Cable A Output send");
    assert!(nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "Dry signal · no stages")));
    click(&mut pump, "Edit processing for the Hardware mic to Cable B Output send");
    click(&mut pump, "Edit stage 2, Voice tone");
    button(&pump, "Enable");
    assert!(nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "Pitch  ·  -7 semitones")));
    click(&mut pump, "Remove");
    button(&pump, "Edit stage 1, Echo cancellation");
    button(&pump, "Edit stage 2, Noise reduction");
    assert!(!nodes(&pump).any(|n| text_is(&pump, n, PropertyId::TextBlockText, "Voice tone")));
}
