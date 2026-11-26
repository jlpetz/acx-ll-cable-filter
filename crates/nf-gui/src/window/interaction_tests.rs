use windows_reactor::test::{
    Command, EventId, EventPayload, NodeId, PropertyId, PropertyValue, Pump, QueuedEvent, RecordingRuntime, SlotId,
};

use super::*;
use crate::settings::Saver;

type WindowPump = Pump<RecordingRuntime>;

fn mounted_nodes(pump: &WindowPump) -> impl Iterator<Item = NodeId> + '_ {
    pump.runtime().commands().iter().flatten().filter_map(|command| match command {
        Command::Create { node, .. } if pump.runtime().node(*node).is_some() => Some(*node),
        _ => None,
    })
}

fn label_is(pump: &WindowPump, node: NodeId, text: &str) -> bool {
    matches!(pump.runtime().node(node).and_then(|n| n.property(PropertyId::TextBlockText)), Some(PropertyValue::Str(label)) if label == text)
}

fn named_slot(pump: &WindowPump, slot: SlotId, name: &str) -> NodeId {
    mounted_nodes(pump)
        .find(|node| pump.runtime().node(*node).unwrap().slot(slot).is_some_and(|child| label_is(pump, child, name)))
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn event(pump: &mut WindowPump, start: &Start, node: NodeId, event: EventId, payload: EventPayload) {
    let revision = pump.event_revision(node, event).expect("subscribed event");
    pump.queue_event(QueuedEvent::new(node, event, revision, payload));
    pump.dispatch_events().unwrap();
    pump.dispatch_components(32).unwrap();
    pump.update_view(View::component::<Window>(start.clone())).unwrap();
}

fn mount() -> (Start, WindowPump) {
    let start = Start {
        isolated: true,
        engine: SharedEngine(Arc::new(Engine::new())),
        settings: Settings::default(),
        warning: None,
        saver: Saver::disabled(Default::default()).handle(),
    };
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(View::component::<Window>(start.clone())).unwrap();
    (start, pump)
}

#[test]
fn window_sections_stay_open_independently_through_refreshes() {
    let (start, mut pump) = mount();
    let sections = ["Diagnostics", "Measure latency", "Gate settings", "Processing presets"].map(|name| {
        let id = named_slot(&pump, SlotId::ExpanderHeader, name);
        let content = pump.runtime().node(id).unwrap().slot(SlotId::ExpanderContent).expect("section content");
        assert!(!pump.runtime().node(content).unwrap().children().is_empty(), "{name} is empty");
        id
    });
    let prop = PropertyId::ExpanderIsExpanded;
    let mut expected = [false; 4];
    for (index, open) in [(0, true), (1, true), (2, true), (3, true), (0, false), (1, false), (0, true)] {
        let node = sections[index];
        pump.runtime_mut().record_property_observation(node, prop, PropertyValue::Bool(open)).unwrap();
        event(&mut pump, &start, node, EventId::ExpanderIsExpandedChanged, EventPayload::Bool(open));
        expected[index] = open;
        for (node, open) in sections.into_iter().zip(expected) {
            assert_eq!(pump.runtime().node(node).unwrap().property(prop), Some(&PropertyValue::Bool(open)));
        }
    }
}

#[test]
fn a_named_preset_restores_the_gate_selected_in_the_window() {
    let (start, mut pump) = mount();
    let name = named_slot(&pump, SlotId::TextBoxHeader, "Preset name");
    let gate = named_slot(&pump, SlotId::ComboBoxHeader, "Noise gate");
    let choose = |pump: &mut WindowPump, index| {
        pump.runtime_mut()
            .record_property_observation(gate, PropertyId::ComboBoxSelectedIndex, PropertyValue::SelectionIndex(Some(index)))
            .unwrap();
        event(pump, &start, gate, EventId::ComboBoxSelectionChanged, EventPayload::SelectionIndex(Some(index)))
    };
    let click = |pump: &mut WindowPump, label| {
        let node = mounted_nodes(pump)
            .find(|id| pump.runtime().node(*id).unwrap().children().iter().any(|&child| label_is(pump, child, label)))
            .unwrap_or_else(|| panic!("missing button {label}"));
        event(pump, &start, node, EventId::ButtonClick, EventPayload::Unit);
    };
    choose(&mut pump, 1);
    for typed in ["My", "My gate"] {
        pump.runtime_mut().record_property_observation(name, PropertyId::TextBoxText, PropertyValue::Str(typed.into())).unwrap();
        event(&mut pump, &start, name, EventId::TextBoxTextChanged, EventPayload::Str(typed.into()));
        assert_eq!(pump.runtime().node(name).unwrap().property(PropertyId::TextBoxText), Some(&PropertyValue::Str(typed.into())));
    }
    click(&mut pump, "Save preset");
    choose(&mut pump, 0);
    click(&mut pump, "Load");
    assert_eq!(
        pump.runtime().node(gate).unwrap().property(PropertyId::ComboBoxSelectedIndex),
        Some(&PropertyValue::SelectionIndex(Some(1)))
    );
}
