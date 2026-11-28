use llcable_audio::Level;
use llcable_control::routing::StageReading;
use llcable_control::{CaptureEffects, Diagnostics};
use windows_reactor::test::{Command, NodeId, PropertyId, PropertyValue, Pump, RecordingRuntime, ThemeStyle};
use windows_reactor::{HorizontalAlignment, TextBlock, ThemeBrush, View};

use super::metrics::{cable_losses, effect, route_levels};
use super::{EffectsCheck, FeedState, windows_effects_result};

type Panel = Pump<RecordingRuntime>;

fn mount(view: View) -> Panel {
    let mut pump = Pump::new(RecordingRuntime::default());
    pump.mount_view(view).unwrap();
    pump
}

fn nodes(pump: &Panel) -> impl Iterator<Item = NodeId> + '_ {
    pump.runtime().commands().iter().flatten().filter_map(|command| match command {
        Command::Create { node, .. } if pump.runtime().node(*node).is_some() => Some(*node),
        _ => None,
    })
}

fn text_property(pump: &Panel, node: NodeId, property: PropertyId) -> Option<&str> {
    match pump.runtime().node(node)?.property(property)? {
        PropertyValue::Str(value) => Some(value),
        _ => None,
    }
}

fn cell(pump: &Panel, text: &str) -> NodeId {
    nodes(pump)
        .find(|&node| text_property(pump, node, PropertyId::TextBlockText) == Some(text))
        .unwrap_or_else(|| panic!("Missing displayed metric {text:?}"))
}

fn critical_style() -> ThemeStyle {
    // Reactor resolves a theme-resource foreground separately from a TextBlock's solid-colour property.
    let sample = mount(TextBlock::new().text("error").foreground(ThemeBrush::SystemCritical).into());
    sample.runtime().node(cell(&sample, "error")).unwrap().theme_style()
}

fn assert_alert(pump: &Panel, text: &str, subject: &str, metric: &str) {
    let node = cell(pump, text);
    assert_eq!(pump.runtime().node(node).unwrap().theme_style(), critical_style());
    let name = text_property(pump, node, PropertyId::AutomationName).expect("metric has an accessible name");
    for part in [subject, metric, "error", text.trim_start_matches("! ")] {
        assert!(name.to_lowercase().contains(&part.to_lowercase()), "{name:?} is missing {part:?}");
    }
}

#[test]
fn a_reset_clears_the_cable_alarm_and_preserves_the_device_totals() {
    let stats = Diagnostics { underrun_frames: 1_069, render_lost_packets: Some(2), ..Default::default() };
    let mut panel = mount(cable_losses("LLCable-B", &stats, (17, Some(1))));
    assert_alert(&panel, "! 17", "LLCable-B", "underrun");
    assert_alert(&panel, "! 1", "LLCable-B", "missing");
    panel.update_view(cable_losses("LLCable-B", &stats, (0, Some(0)))).unwrap();
    let critical = critical_style();
    for value in ["1,069", "2", "0"] {
        assert_ne!(panel.runtime().node(cell(&panel, value)).unwrap().theme_style(), critical);
    }
    assert!(!nodes(&panel).any(|id| panel.runtime().node(id).unwrap().theme_style() == critical));
}

#[test]
fn wide_and_narrow_route_tables_keep_the_exact_counts_and_distinguish_startup_silence_from_loss() {
    let mic = Level { frames: 4_294_967_296, dropped_frames: 23, underrun_frames: 3, startup_silent_frames: 128, ..Default::default() };
    let cable = Level { frames: 19_284, ..Default::default() };
    let other = Level { frames: 57_421, ..Default::default() };
    let rows = [
        ("Input", "Studio mic", &mic, FeedState::Active),
        ("Output", "LLCable-A", &cable, FeedState::Paused),
        ("Output", "LLCable-B", &other, FeedState::Offline),
    ];
    let mut panel = mount(route_levels(&rows, 1000.0));
    for (width, wide) in [(1000.0, true), (560.0, false)] {
        panel.update_view(route_levels(&rows, width)).unwrap();
        let tables: Vec<_> = nodes(&panel)
            .filter_map(|n| {
                let grid = panel.runtime().node(n)?;
                match grid.property(PropertyId::GridColumns)? {
                    PropertyValue::GridLengths(columns) => Some(columns.len()),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(tables.len() == 1, wide, "{width} DIP grid column counts: {tables:?}");
        assert_eq!(tables.iter().map(|columns| columns - 1).sum::<usize>(), rows.len());
        assert_alert(&panel, "! 23", "Studio mic", "drop");
        assert_alert(&panel, "! 3", "Studio mic", "underrun");
        let paused = cell(&panel, "Output · LLCable-A · Paused");
        assert_ne!(panel.runtime().node(paused).unwrap().theme_style(), critical_style());
        assert_eq!(text_property(&panel, paused, PropertyId::AutomationName), Some("Output · LLCable-A, feed paused"));
        assert_eq!(panel.runtime().node(cell(&panel, "! Output · LLCable-B · Offline")).unwrap().theme_style(), critical_style());
        for value in ["128", "4,294,967,296", "19,284", "57,421"] {
            let n = panel.runtime().node(cell(&panel, value)).unwrap();
            assert_eq!(n.property(PropertyId::HorizontalAlignment), Some(&PropertyValue::HorizontalAlignment(HorizontalAlignment::Right)));
            assert_ne!(n.theme_style(), critical_style());
        }
    }
}

#[test]
fn an_effect_worker_miss_is_visible_and_named_for_its_stage() {
    let missed = StageReading { worker_misses: Some(12_345), ..Default::default() };
    let panel = mount(effect("Voice effect", missed));
    assert_alert(&panel, "! 12,345", "Voice effect", "miss");
}

#[test]
fn windows_effects_show_each_endpoint_and_mode_even_if_one_query_fails() {
    let check = EffectsCheck::Complete(vec![
        (
            "LLCable-B Out".into(),
            Ok(CaptureEffects { ordinary: Ok(vec![]), communications: Ok(vec!["Noise suppression".into(), "Echo cancellation".into()]) }),
        ),
        (
            "LLCable-B Out-5ms".into(),
            Ok(CaptureEffects { ordinary: Ok(vec!["Equalizer".into()]), communications: Err(std::io::Error::other("Unsupported mode")) }),
        ),
    ]);
    let panel = mount(windows_effects_result(&check));
    for label in [
        "LLCable-B Out",
        "LLCable-B Out-5ms",
        "Standard · No effects reported",
        "Communications · Noise suppression, Echo cancellation",
        "Standard · Equalizer",
        "Error · Communications · Unavailable · Unsupported mode",
    ] {
        cell(&panel, label);
    }
}

#[test]
#[ignore = "checks the installed Windows cable endpoints through the mixer"]
fn check_windows_effects_from_cable_cards() {
    use std::time::{Duration, Instant};

    use windows_reactor::test::{EventId, EventPayload, QueuedEvent};

    use crate::window::{Mixer, Page, recording_ports};

    fn wait(panel: &mut Panel, ready: impl Fn(&Panel) -> bool) {
        let start = Instant::now();
        while !ready(panel) {
            panel.dispatch_components(32).unwrap();
            assert!(start.elapsed() < Duration::from_secs(15), "Timed out waiting for the cable panel");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let cables = llcable_control::cables().unwrap();
    let mut panel = mount(View::component::<Mixer>(Page::Cables));
    let rows = |panel: &Panel| {
        nodes(panel)
            .filter_map(|id| text_property(panel, id, PropertyId::TextBlockText))
            .filter(|s| {
                let mode = s.strip_prefix("Error · ").unwrap_or(s);
                mode.starts_with("Standard · ")
                    || mode.starts_with("Communications · ")
                    || mode.starts_with("Windows effects unavailable · ")
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut total = 0;
    for cable in cables.iter().filter(|c| recording_ports(c).next().is_some()) {
        let action = format!("Check Windows effects for LLCable-{}", cable.label);
        let button = |p: &Panel| nodes(p).find(|&id| text_property(p, id, PropertyId::AutomationName) == Some(&action));
        wait(&mut panel, |p| button(p).is_some());
        assert_eq!(rows(&panel).len(), total, "Effects should appear only after checking a cable");
        let id = button(&panel).unwrap();
        let revision = panel.event_revision(id, EventId::ButtonClick).unwrap();
        panel.queue_event(QueuedEvent::new(id, EventId::ButtonClick, revision, EventPayload::Unit));
        panel.dispatch_events().unwrap();
        wait(&mut panel, |p| {
            recording_ports(cable)
                .all(|port| nodes(p).any(|id| text_property(p, id, PropertyId::TextBlockText) == Some(port.name.as_str())))
        });
        let previous = total;
        total += 2 * recording_ports(cable).count();
        let displayed = rows(&panel);
        assert_eq!(displayed.len(), total, "Unexpected Windows effects response for LLCable-{}", cable.label);
        assert!(!displayed.iter().any(|s| s.starts_with("Error · ")), "{displayed:?}");
        println!("LLCable-{}: {:?}", cable.label, &displayed[previous..]);
    }
    assert!(total > 0, "Install a cable with an active recording port to run this check");
}
