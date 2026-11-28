use windows_reactor::test::{Command, EventId, EventPayload, NodeId, PropertyId, PropertyValue, Pump, QueuedEvent, RecordingRuntime};

use super::*;

type Preview = Pump<RecordingRuntime>;

fn nodes(pump: &Preview) -> impl Iterator<Item = NodeId> + '_ {
    pump.runtime().commands().iter().flatten().filter_map(|cmd| match cmd {
        Command::Create { node, .. } if pump.runtime().node(*node).is_some() => Some(*node),
        _ => None,
    })
}

fn has_text(pump: &Preview, id: NodeId, prop: PropertyId, label: &str) -> bool {
    matches!(pump.runtime().node(id).and_then(|n| n.property(prop)), Some(PropertyValue::Str(text)) if text == label)
}

fn text_button(pump: &Preview, label: &str) -> NodeId {
    nodes(pump)
        .find(|&id| {
            pump.event_revision(id, EventId::ButtonClick).is_some()
                && pump.runtime().node(id).unwrap().children().iter().any(|c| has_text(pump, *c, PropertyId::TextBlockText, label))
        })
        .unwrap_or_else(|| panic!("missing {label} button"))
}

fn queue(pump: &mut Preview, id: NodeId, event: EventId, payload: EventPayload) {
    let revision = pump.event_revision(id, event).expect("current pointer subscription");
    pump.queue_event(QueuedEvent::new(id, event, revision, payload));
}

fn flush(pump: &mut Preview) {
    pump.dispatch_events().unwrap();
    pump.dispatch_components(32).unwrap();
    pump.update_view(View::component::<Mixer>(())).unwrap();
}

fn patchbay() -> Preview {
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(View::component::<Mixer>(())).unwrap();
    let button = text_button(&pump, "Wiring");
    queue(&mut pump, button, EventId::ButtonClick, EventPayload::Unit);
    flush(&mut pump);
    pump
}

fn prop(pump: &Preview, id: NodeId, key: PropertyId) -> f64 {
    match pump.runtime().node(id).and_then(|n| n.property(key)) {
        Some(PropertyValue::F64(value)) => *value,
        other => panic!("expected coordinate for {key:?}, got {other:?}"),
    }
}

fn cable_c_socket(pump: &Preview) -> NodeId {
    nodes(pump).find(|&id| has_text(pump, id, PropertyId::AutomationName, "LLCable-C In wiring socket")).unwrap()
}

/// Native pointer positions are relative to the captured socket even when the pointer lies over an output.
fn destination(pump: &Preview, socket: NodeId) -> (f64, f64) {
    let output = text_button(pump, "Headphones");
    (
        prop(pump, output, PropertyId::CanvasLeft) + BRANCH_WIDTH / 2.0 - prop(pump, socket, PropertyId::CanvasLeft),
        prop(pump, output, PropertyId::CanvasTop) + NODE_HEIGHT / 2.0 - prop(pump, socket, PropertyId::CanvasTop),
    )
}

fn pointer(pump: &mut Preview, node: NodeId, event: EventId, x: f64, y: f64) {
    let payload = PointerEventInfo {
        x,
        y,
        capture_succeeded: event == EventId::BorderPointerPressed,
        is_left_button_pressed: event != EventId::BorderPointerReleased,
        ..Default::default()
    };
    queue(pump, node, event, EventPayload::PointerEventInfo(payload));
}

fn check_paths(pump: &Preview, headphones: usize) {
    for (name, count) in [("LLCable-A Out", 1), ("LLCable-B Out", 1), ("Headphones", headphones)] {
        let button = text_button(pump, name);
        let label = format!("{name}. {name}. {count} connected sources. Select output.");
        assert!(has_text(pump, button, PropertyId::AutomationName, &label), "{label}");
    }
}

#[test]
fn quick_drag_through_reactors_release_capture_order_connects_only_once() {
    let mut pump = patchbay();
    for count in [2, 1] {
        let socket = cable_c_socket(&pump);
        let (x, y) = destination(&pump, socket);
        for _ in 0..count {
            pointer(&mut pump, socket, EventId::BorderPointerPressed, 13.0, 13.0);
            pointer(&mut pump, socket, EventId::BorderPointerMoved, x, y);
            // Native ReleasePointerCapture can enqueue the loss before the release. Rapid gestures can share one event batch.
            queue(&mut pump, socket, EventId::BorderPointerCaptureLost, EventPayload::Unit);
            pointer(&mut pump, socket, EventId::BorderPointerReleased, x, y);
        }
        flush(&mut pump);
        check_paths(&pump, 1);
    }
}

#[test]
fn drag_lead_moves_across_paints_and_survives_capture_loss_before_release() {
    let mut pump = patchbay();
    let socket = cable_c_socket(&pump);
    let (x, y) = destination(&pump, socket);
    pointer(&mut pump, socket, EventId::BorderPointerPressed, 13.0, 13.0);
    flush(&mut pump);
    let (local_x, local_y) = (47.0, 59.0);
    pointer(&mut pump, socket, EventId::BorderPointerMoved, local_x, local_y);
    flush(&mut pump);
    let target_x = prop(&pump, socket, PropertyId::CanvasLeft) + local_x;
    let target_y = prop(&pump, socket, PropertyId::CanvasTop) + local_y;
    assert!(nodes(&pump).any(|id| {
        let node = pump.runtime().node(id).unwrap();
        node.property(PropertyId::LineX2) == Some(&PropertyValue::F64(target_x))
            && node.property(PropertyId::LineY2) == Some(&PropertyValue::F64(target_y))
    }));
    queue(&mut pump, socket, EventId::BorderPointerCaptureLost, EventPayload::Unit);
    flush(&mut pump);
    check_paths(&pump, 0);
    pointer(&mut pump, socket, EventId::BorderPointerReleased, x, y);
    flush(&mut pump);
    check_paths(&pump, 1);
}

#[test]
fn cancel_or_release_outside_leaves_all_sends_intact() {
    let mut pump = patchbay();
    let socket = cable_c_socket(&pump);
    let (x, y) = destination(&pump, socket);
    for canceled in [true, false] {
        pointer(&mut pump, socket, EventId::BorderPointerPressed, 13.0, 13.0);
        pointer(&mut pump, socket, EventId::BorderPointerMoved, x, y);
        if canceled {
            queue(&mut pump, socket, EventId::BorderPointerCanceled, EventPayload::Unit);
        }
        pointer(&mut pump, socket, EventId::BorderPointerReleased, x, if canceled { y } else { -100.0 });
        queue(&mut pump, socket, EventId::BorderPointerCaptureLost, EventPayload::Unit);
        flush(&mut pump);
        check_paths(&pump, 0);
    }
}

fn named(pump: &Preview, label: &str) -> NodeId {
    nodes(pump).find(|&id| has_text(pump, id, PropertyId::AutomationName, label)).unwrap_or_else(|| panic!("missing control {label}"))
}

fn click_text(pump: &mut Preview, label: &str) {
    let id = text_button(pump, label);
    queue(pump, id, EventId::ButtonClick, EventPayload::Unit);
    flush(pump);
}

fn click_named(pump: &mut Preview, label: &str) {
    let id = named(pump, label);
    queue(pump, id, EventId::ButtonClick, EventPayload::Unit);
    flush(pump);
}

#[test]
fn send_ring_is_centered_and_has_room_inside_the_padded_native_button() {
    let pump = patchbay();
    let button = named(&pump, "Select wire. Hardware mic → LLCable-A Out");
    let hit = pump.runtime().node(button).unwrap().children()[0];
    let ring = pump.runtime().node(hit).unwrap().children()[0];
    let diameter = prop(&pump, ring, PropertyId::Width);
    assert_eq!(diameter, prop(&pump, ring, PropertyId::Height));
    assert!(prop(&pump, hit, PropertyId::Width) >= diameter);
    assert!(prop(&pump, button, PropertyId::Width) - prop(&pump, hit, PropertyId::Width) >= 32.0);
    assert!(prop(&pump, button, PropertyId::Height) - prop(&pump, hit, PropertyId::Height) >= 12.0);
    assert_eq!(
        pump.runtime().node(button).unwrap().property(PropertyId::ButtonHorizontalContentAlignment),
        Some(&PropertyValue::HorizontalAlignment(HorizontalAlignment::Center))
    );
    assert_eq!(
        pump.runtime().node(ring).unwrap().property(PropertyId::HorizontalAlignment),
        Some(&PropertyValue::HorizontalAlignment(HorizontalAlignment::Center))
    );
}

fn right_click_content(pump: &mut Preview, label: &str) {
    let button = named(pump, label);
    let inner = pump
        .runtime()
        .node(button)
        .unwrap()
        .children()
        .iter()
        .copied()
        .find(|&id| pump.event_revision(id, EventId::BorderPointerPressed).is_some())
        .expect("hit target inside the native button");
    queue(
        pump,
        inner,
        EventId::BorderPointerPressed,
        EventPayload::PointerEventInfo(PointerEventInfo { is_right_button_pressed: true, ..Default::default() }),
    );
    flush(pump);
}

#[test]
fn split_menu_removes_one_named_branch_and_leaves_shared_effects_in_place() {
    let mut pump = patchbay();
    let menu = pump
        .runtime()
        .commands()
        .iter()
        .flatten()
        .find_map(|cmd| match cmd {
            Command::SetOwnedMenu { owner, items: Some(items), .. }
                if items
                    .iter()
                    .any(|item| matches!(item,MenuItem::Item {label,..} if label == "Disconnect Hardware mic → LLCable-A Out")) =>
            {
                Some(*owner)
            }
            _ => None,
        })
        .expect("mounted split junction menu");
    queue(&mut pump, menu, EventId::OwnedMenuItemInvoked, EventPayload::String("Disconnect Hardware mic → LLCable-A Out".into()));
    flush(&mut pump);
    named(&pump, "Select wire. Hardware mic → LLCable-B Out");
    named(&pump, "Hardware mic stage 1. Noise reduction");
    assert!(!nodes(&pump).any(|id| has_text(&pump, id, PropertyId::AutomationName, "Select wire. Hardware mic → LLCable-A Out")));
}

#[test]
fn right_click_on_a_wire_away_from_its_handle_selects_only_that_send() {
    let mut pump = patchbay();
    let layout = Layout::new(&Mixer::new(crate::Appearance::default()).scene, 926.0);
    let &(d, _, lane) = layout.sources[0].sends.iter().find(|edge| edge.0 == 0).unwrap();
    let surface = named(&pump, "Connection hit surface");
    let event = PointerEventInfo {
        x: (lane + layout.outputs[d]) / 2.0 + 3.0,
        y: (layout.exit_y + layout.mix_y) / 2.0,
        is_right_button_pressed: true,
        ..Default::default()
    };
    queue(&mut pump, surface, EventId::BorderPointerPressed, EventPayload::PointerEventInfo(event));
    flush(&mut pump);
    click_text(&mut pump, "Disconnect");
    named(&pump, "Select wire. Hardware mic → LLCable-B Out");
    assert!(!nodes(&pump).any(|id| has_text(&pump, id, PropertyId::AutomationName, "Select wire. Hardware mic → LLCable-A Out")));
}

#[test]
fn each_branch_stage_has_its_own_node_and_removal_and_right_click_targets_one_wire() {
    let mut pump = patchbay();
    let send = "Select wire. Hardware mic → LLCable-A Out";
    click_named(&mut pump, send);
    click_text(&mut pump, "Edit send");
    for add in ["Add Gain", "Add Noise gate"] {
        click_text(&mut pump, "+ Add stage");
        click_named(&mut pump, add);
    }
    click_text(&mut pump, "Bypass");
    click_text(&mut pump, "Wiring");
    named(&pump, "Hardware mic → LLCable-A Out stage 1. Gain");
    named(&pump, "Hardware mic → LLCable-A Out stage 2. Noise gate (off)");
    click_named(&mut pump, "Remove Hardware mic → LLCable-A Out stage 1. Gain");
    right_click_content(&mut pump, "Hardware mic → LLCable-A Out stage 1. Noise gate (off)");
    text_button(&pump, "Enable");
    click_text(&mut pump, "Wiring");
    named(&pump, "Hardware mic stage 1. Noise reduction");

    right_click_content(&mut pump, send);
    click_text(&mut pump, "Disconnect");
    named(&pump, "Select wire. Hardware mic → LLCable-B Out");
    assert!(!nodes(&pump).any(|id| has_text(&pump, id, PropertyId::AutomationName, send)));
}
