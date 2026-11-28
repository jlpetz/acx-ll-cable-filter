//! The patchbay uses the mixer's sends directly. Source and output groups have one lead each;
//! only a dot connects crossing wires. A busy graph can scroll horizontally without shrinking the controls.
use std::cell::Cell;
use std::rc::Rc;

use windows_reactor::*;

use super::{DESTS, Mixer, Msg, SOURCES, Scene, Scope, Stage};

#[path = "wire_layout.rs"]
mod geometry;
use geometry::{Layout, STAGE_STEP, boundary_y};

const BRANCH_WIDTH: f64 = 156.0;
const GAP: f64 = 22.0;
const NODE_HEIGHT: f64 = 42.0;
const SOURCE_Y: f64 = 10.0;
const DRAG_PORT: f64 = 26.0;

#[derive(Clone, Copy, PartialEq)]
enum Press {
    Captured(usize),
    ReleasedCapture(usize),
}

/// Event callbacks can run in the same batch, before the component publishes its next view.
#[derive(Clone, Default)]
pub(super) struct Gesture(Rc<Cell<Option<Press>>>);

impl Gesture {
    pub(super) fn reset(&self) {
        self.0.set(None);
    }

    fn owns(&self, source: usize) -> bool {
        matches!(self.0.get(), Some(Press::Captured(s) | Press::ReleasedCapture(s)) if s == source)
    }
}

fn step_name(stage: &Stage) -> String {
    if stage.enabled { stage.kind.name().into() } else { format!("{} (off)", stage.kind.name()) }
}

fn chain_name(stages: &[Stage]) -> String {
    stages.iter().map(step_name).collect::<Vec<_>>().join(" → ")
}

fn output_at(outputs: &[f64], row_y: f64, x: f64, y: f64) -> Option<usize> {
    if (row_y - 8.0..=row_y + NODE_HEIGHT).contains(&y) {
        outputs.iter().position(|out| (x - out).abs() <= BRANCH_WIDTH / 2.0)
    } else {
        None
    }
}

fn wire(items: &mut Vec<View>, x: f64, y: f64, to_x: f64, to_y: f64) {
    lead(items, (x, y), (to_x, to_y), false);
}

fn lead(items: &mut Vec<View>, from: (f64, f64), to: (f64, f64), selected: bool) {
    let (x, y) = from;
    let (to_x, to_y) = to;
    if (x, y) != (to_x, to_y) {
        items.push(
            Line::new().x1(x).y1(y).x2(to_x).y2(to_y).stroke(ThemeBrush::Accent).stroke_thickness(if selected { 4.0 } else { 2.0 }).into(),
        );
    }
}

fn node(label: &str, detail: &str, x: f64, y: f64, active: bool, callback: Callback<()>) -> View {
    Button::new()
        .width(BRANCH_WIDTH)
        .height(NODE_HEIGHT)
        .canvas_left(x - BRANCH_WIDTH / 2.0)
        .canvas_top(y)
        .style(if active { ButtonStyle::Accent } else { ButtonStyle::Default })
        .automation_name(format!("{label}. {detail}"))
        .on_click(callback)
        .content(TextBlock::new().text(label).text_trimming(TextTrimming::CharacterEllipsis))
        .tooltip(detail)
}

/// Capture the pointer on the bottom socket. Native buttons on the same card provide keyboard and click-to-connect access.
fn drag_socket(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, layout: &Layout) -> View {
    let x = layout.sources[s].x - DRAG_PORT / 2.0;
    let y = SOURCE_Y + NODE_HEIGHT - 4.0;
    let send = cx.sender();
    let update = cx.sender();
    let finish = cx.sender();
    let outputs = layout.outputs;
    let output_y = layout.output_y;
    let pressed = app.wire_gesture.clone();
    let moved = pressed.clone();
    let released = pressed.clone();
    let lost = pressed.clone();
    let canceled = pressed.clone();
    let clear_lost = cx.sender();
    let clear_cancel = cx.sender();
    let centre = (DRAG_PORT - 10.0) / 2.0;
    Border::new()
        .canvas_left(x)
        .canvas_top(y)
        .width(DRAG_PORT)
        .height(DRAG_PORT)
        .background(Color::argb(0, 0, 0, 0))
        .automation_name(format!("{} wiring socket", SOURCES[s]))
        .capture_pointer_on_press(true)
        .on_pointer_pressed(move |event: PointerEventInfo| {
            if event.capture_succeeded && event.is_left_button_pressed {
                pressed.0.set(Some(Press::Captured(s)));
                send.send(Msg::WireDrag(Some((s, x + event.x, y + event.y))));
            } else if pressed.owns(s) {
                pressed.reset();
                send.send(Msg::WireDrag(None));
            }
        })
        .on_pointer_moved(move |event: PointerEventInfo| {
            if moved.0.get() == Some(Press::Captured(s)) && event.is_left_button_pressed {
                update.send(Msg::WireDrag(Some((s, x + event.x, y + event.y))));
            }
        })
        .on_pointer_released(move |event: PointerEventInfo| {
            if released.owns(s) {
                released.reset();
                if let Some(d) = output_at(&outputs, output_y, x + event.x, y + event.y) {
                    finish.send(Msg::ConnectSend(s, d));
                }
                finish.send(Msg::WireDrag(None));
            }
        })
        .on_pointer_capture_lost(move || {
            // Reactor 0.100 calls ReleasePointerCapture before queueing PointerReleased. Hide the lead immediately
            // on loss, and let the matching release report its final position. Cancellation and navigation disarm it.
            if lost.0.get() == Some(Press::Captured(s)) {
                lost.0.set(Some(Press::ReleasedCapture(s)));
                clear_lost.send(Msg::WireDrag(None));
            }
        })
        .on_pointer_canceled(move || {
            if canceled.owns(s) {
                canceled.reset();
                clear_cancel.send(Msg::WireDrag(None));
            }
        })
        .content(Canvas::new().children((
            Border::new().canvas_left(centre).canvas_top(centre).width(10.0).height(10.0).corner_radius(5.0).background(ThemeBrush::Accent),
        )))
        .tooltip(format!("Drag from {} to an output", SOURCES[s]))
}

/// The pointer target must be inside the control's content. A WinUI button consumes the press before it can bubble to a parent border.
fn secondary_press(content: View, width: f64, action: Callback<()>) -> View {
    Border::new()
        .width(width)
        .height(22.0)
        .background(Color::transparent())
        .on_pointer_pressed(move |event: PointerEventInfo| {
            if event.is_right_button_pressed {
                let _ = action.call(());
            }
        })
        .content(content)
}

fn stage_node(cx: &ViewContext<Mixer>, scope: Scope, position: usize, stage: Stage, x: f64, y: f64) -> View {
    let id = stage.id;
    let name = step_name(&stage);
    let label = format!("{} stage {}. {name}", scope.title(), position + 1);
    let actions = Button::new()
        .automation_name(format!("Remove {label}"))
        .on_click(cx.callback(move |()| Msg::DeleteStage(scope, id)))
        .content(format!("Remove {}", stage.kind.name()));
    let open = cx.callback(move |()| Msg::EditStage(scope, id));
    let text = TextBlock::new()
        .text(name)
        .font_size(13.0)
        .vertical_alignment(VerticalAlignment::Center)
        .opacity(if stage.enabled { 1.0 } else { 0.72 })
        .text_trimming(TextTrimming::CharacterEllipsis);
    SplitButton::new()
        .width(BRANCH_WIDTH)
        .height(NODE_HEIGHT)
        .canvas_left(x - BRANCH_WIDTH / 2.0)
        .canvas_top(y)
        .automation_name(&label)
        .on_click(open.clone())
        .content(secondary_press(text.into(), BRANCH_WIDTH - 50.0, open))
        .flyout_with(Flyout::rich(actions).placement(FlyoutPlacement::Bottom))
}

/// The native flyout lists the actual sends at this junction. Choosing one only removes that branch.
fn junction(cx: &ViewContext<Mixer>, label: &str, sends: &[(usize, usize)], x: f64, y: f64) -> View {
    let entries: Vec<_> = sends.iter().map(|&(s, d)| (format!("Disconnect {} → {}", SOURCES[s], DESTS[d]), (s, d))).collect();
    let menu = entries.iter().enumerate().map(|(i, (name, _))| MenuItem::item(i, name.clone())).collect::<Vec<_>>();
    let sender = cx.sender();
    let button = Button::new()
        .style(ButtonStyle::Default)
        .width(62.0)
        .height(28.0)
        .automation_name(format!("{label} junction. Choose a connection to disconnect"))
        .content(TextBlock::new().font_size(12.0).text(label))
        .menu(Menu::new(menu, move |choice: String| {
            if let Some((_, (s, d))) = entries.iter().find(|(name, _)| *name == choice) {
                sender.send(Msg::DisconnectSend(*s, *d));
            }
        }));
    Border::new().canvas_left(x - 31.0).canvas_top(y - 14.0).corner_radius(4.0).background(ThemeBrush::SolidBackground).content(button)
}

fn send_control(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, d: usize, x: f64, y: f64) -> View {
    let selected = app.selected_wire == Some((s, d));
    let db = app.scene.sends[s][d].gain_db;
    let label = format!("Select wire. {} → {}", SOURCES[s], DESTS[d]);
    let select = cx.callback(move |()| Msg::SelectWire(Some((s, d))));
    // WinUI's native button reserves horizontal padding. Give the hit target its own width and centre the glyph inside it.
    let (width, hit_width, content): (f64, f64, View) = if db == 0 {
        (
            56.0,
            20.0,
            Border::new()
                .width(10.0)
                .height(10.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .corner_radius(5.0)
                .border_thickness(2.0)
                .border_brush(ThemeBrush::Accent)
                .background(ThemeBrush::SolidBackground)
                .into(),
        )
    } else {
        (
            80.0,
            46.0,
            TextBlock::new()
                .text(format!("{db:+} dB"))
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        )
    };
    Button::new()
        .width(width)
        .height(36.0)
        .canvas_left(x - width / 2.0)
        .canvas_top(y - 18.0)
        .style(if selected { ButtonStyle::Accent } else { ButtonStyle::Subtle })
        .horizontal_content_alignment(HorizontalAlignment::Center)
        .vertical_content_alignment(VerticalAlignment::Center)
        .automation_name(&label)
        .on_click(select.clone())
        .content(secondary_press(content, hit_width, select))
        .tooltip(format!("{label}. Show send controls and disconnect."))
}

#[derive(Clone, Copy)]
struct Segment {
    send: (usize, usize),
    from: (f64, f64),
    to: (f64, f64),
}

fn wire_at(segments: &[Segment], x: f64, y: f64) -> Option<(usize, usize)> {
    let mut hit = None;
    let mut radius = 7.0;
    for segment in segments {
        let (a, b) = segment.from;
        let (dx, dy) = (segment.to.0 - a, segment.to.1 - b);
        let squared = dx * dx + dy * dy;
        if squared == 0.0 {
            continue;
        }
        let t = (((x - a) * dx + (y - b) * dy) / squared).clamp(0.0, 1.0);
        let distance = (x - a - dx * t).hypot(y - b - dy * t);
        if distance <= radius {
            hit = Some(segment.send);
            radius = distance;
        }
    }
    hit
}

fn source_nodes(
    app: &Mixer, cx: &ViewContext<Mixer>, layout: &Layout, s: usize, wires: &mut Vec<View>, nodes: &mut Vec<View>,
    segments: &mut Vec<Segment>,
) {
    let group = &layout.sources[s];
    let shared = app.scene.at(Scope::Source(s));
    let selected = app.wire_source == Some(s);
    nodes.push(node(
        SOURCES[s],
        &format!("Choose {} to connect or disconnect an output", SOURCES[s]),
        group.x,
        SOURCE_Y,
        selected,
        cx.callback(move |()| Msg::WireSource((!selected).then_some(s))),
    ));
    if group.sends.is_empty() && shared.is_empty() {
        wire(wires, group.x, SOURCE_Y + NODE_HEIGHT, group.x, SOURCE_Y + NODE_HEIGHT + DRAG_PORT / 2.0 - 4.0);
        return;
    }
    let mut from = group.x;
    wire(wires, from, SOURCE_Y + NODE_HEIGHT, from, boundary_y(0));
    for boundary in 0..=shared.len() {
        let y = boundary_y(boundary);
        let taps: Vec<_> = group.sends.iter().filter(|edge| edge.1 == boundary).collect();
        let following: Vec<_> = group.sends.iter().filter(|edge| edge.1 > boundary).map(|e| e.2).chain(group.tail).collect();
        let next = shared.get(boundary).map(|_| (following[0] + following[following.len() - 1]) / 2.0);
        let (mut left, mut right) = (from, from);
        for x in taps.iter().map(|edge| edge.2).chain(next) {
            left = left.min(x);
            right = right.max(x);
        }
        wire(wires, left, y, right, y);
        if taps.len() + usize::from(next.is_some()) > 1 {
            let outgoing: Vec<_> = group.sends.iter().filter(|edge| edge.1 >= boundary).map(|edge| (s, edge.0)).collect();
            nodes.push(junction(cx, "Split", &outgoing, from, y));
        }
        for &&(d, _, lane) in &taps {
            let stages = app.scene.at(Scope::Send(s, d));
            let selected = app.selected_wire == Some((s, d));
            let mut lead_y = y;
            for (i, stage) in stages.iter().enumerate() {
                let stage_y = layout.branch_y + i as f64 * STAGE_STEP;
                lead(wires, (lane, lead_y), (lane, stage_y), selected);
                segments.push(Segment { send: (s, d), from: (lane, lead_y), to: (lane, stage_y) });
                nodes.push(stage_node(cx, Scope::Send(s, d), i, *stage, lane, stage_y));
                lead_y = stage_y + NODE_HEIGHT;
            }
            lead(wires, (lane, lead_y), (lane, layout.exit_y), selected);
            segments.push(Segment { send: (s, d), from: (lane, lead_y), to: (lane, layout.exit_y) });
            nodes.push(send_control(app, cx, s, d, lane, layout.exit_y - 17.0));
        }
        if let Some(x) = next {
            let stage_y = y + 18.0;
            wire(wires, x, y, x, stage_y);
            nodes.push(stage_node(cx, Scope::Source(s), boundary, shared[boundary], x, stage_y));
            wire(wires, x, stage_y + NODE_HEIGHT, x, boundary_y(boundary + 1));
            from = x;
        }
    }
}

fn output_wires(app: &Mixer, layout: &Layout, wires: &mut Vec<View>, segments: &mut Vec<Segment>) {
    let mut paths = Vec::new();
    for (s, group) in layout.sources.iter().enumerate() {
        for &(d, _, x) in &group.sends {
            paths.push((x, layout.outputs[d], (s, d)));
        }
    }
    // Keep the selected wire continuous at every crossing.
    paths.sort_by_key(|&(_, _, send)| app.selected_wire == Some(send));
    let lines: Vec<_> = paths.iter().map(|&(from, to, _)| (from, to)).collect();
    let height = layout.mix_y - layout.exit_y;
    for (i, &(from, to, send)) in paths.iter().enumerate() {
        let point = |t: f64| (from + (to - from) * t, layout.exit_y + height * t);
        for (lo, hi) in geometry::wire_spans(from, to, height, &lines[i + 1..]) {
            lead(wires, point(lo), point(hi), app.selected_wire == Some(send));
            segments.push(Segment { send, from: point(lo), to: point(hi) });
        }
    }
}

pub(super) fn view(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let layout = Layout::new(&app.scene, (app.width - 74.0).clamp(330.0, 1240.0));
    let mut wires = Vec::new();
    let mut nodes = Vec::new();
    let mut segments = Vec::new();
    for s in 0..SOURCES.len() {
        source_nodes(app, cx, &layout, s, &mut wires, &mut nodes, &mut segments);
    }
    output_wires(app, &layout, &mut wires, &mut segments);
    for (d, &name) in DESTS.iter().enumerate() {
        let x = layout.outputs[d];
        let incoming = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d)).count();
        let output_stages = app.scene.at(Scope::Output(d));
        if incoming > 0 {
            let end = if output_stages.is_empty() { layout.output_y } else { layout.post_mix_y };
            wire(&mut wires, x, layout.mix_y, x, end);
        }
        if incoming > 1 {
            let sends: Vec<_> = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d)).map(|s| (s, d)).collect();
            nodes.push(junction(cx, &format!("Mix {incoming}"), &sends, x, layout.mix_y));
        }
        for (i, stage) in output_stages.iter().enumerate() {
            let y = layout.post_mix_y + i as f64 * STAGE_STEP;
            nodes.push(stage_node(cx, Scope::Output(d), i, *stage, x, y));
            let next = if i + 1 == output_stages.len() { layout.output_y } else { y + STAGE_STEP };
            wire(&mut wires, x, y + NODE_HEIGHT, x, next);
        }
        let source = app.wire_source;
        let editing = source.map(|s| if app.scene.connected(s, d) { "Disconnect" } else { "Connect" });
        let label = editing.map_or_else(|| name.to_owned(), |action| format!("{action} · {name}"));
        let detail = if let (Some(action), Some(s)) = (editing, source) {
            format!("{action} {} and {name}", SOURCES[s])
        } else if output_stages.is_empty() {
            format!("{name}. {incoming} connected sources. Select output.")
        } else {
            format!("{name}. After mixing: {}. Select output.", chain_name(output_stages))
        };
        nodes.push(node(
            &label,
            &detail,
            x,
            layout.output_y,
            app.output == d,
            cx.callback(move |()| source.map_or(Msg::Output(d), |s| Msg::ToggleSend(s, d))),
        ));
    }
    if let Some((s, x, y)) = app.wire_drag {
        wire(&mut wires, layout.sources[s].x, SOURCE_Y + NODE_HEIGHT + DRAG_PORT / 2.0 - 4.0, x, y);
    }
    // Keep the captured native element keyed by source even when connections and their generated line segments change.
    let drawing = wires.into_iter().enumerate().map(|(i, element)| (format!("wire-{i}"), element));
    let cards = nodes.into_iter().enumerate().map(|(i, element)| (format!("node-{i}"), element));
    let ports = (0..SOURCES.len()).map(|s| (format!("port-{s}"), drag_socket(app, cx, s, &layout)));
    let height = layout.output_y + NODE_HEIGHT + 8.0;
    // A transparent layer above the drawn leads and below interactive cards receives clicks in canvas coordinates.
    // Shared trunks and junctions deliberately have no single-send hit target.
    let sender = cx.sender();
    let surface = Border::new()
        .background(Color::transparent())
        .width(layout.width)
        .height(height)
        .automation_name("Connection hit surface")
        .on_pointer_pressed(move |event: PointerEventInfo| {
            if (event.is_left_button_pressed || event.is_right_button_pressed)
                && let Some(send) = wire_at(&segments, event.x, event.y)
            {
                sender.send(Msg::SelectWire(Some(send)));
            }
        });
    let hit_layer = std::iter::once(("wire-hit-surface".into(), surface.into()));
    let graph = Canvas::new().width(layout.width).height(height).keyed_children(drawing.chain(hit_layer).chain(cards).chain(ports));
    let scrolling = ScrollViewer::new()
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .content(graph);
    let help = app.wire_source.map_or_else(
        || "Drag a source socket to an output, or select both ends. Click a wire for send controls. Gaps mark crossing wires.".into(),
        |s| format!("{} selected. Choose an output to connect or disconnect. Select the source again to cancel.", SOURCES[s]),
    );
    let scope = app.wire_source.map_or(Scope::Output(app.output), Scope::Source);
    let label = format!("Edit {} effects", scope.title());
    let controls: View = if let Some((s, d)) = app.selected_wire {
        StackPanel::new().spacing(4.0).children((
            TextBlock::new().text(format!("{} → {}  ·  {}", SOURCES[s], DESTS[d], app.tap_name(s, d))).font_weight(FontWeight::SEMI_BOLD),
            StackPanel::new().orientation(Orientation::Horizontal).spacing(6.0).children((
                Button::new().on_click(cx.callback(move |()| Msg::Edit(Scope::Send(s, d)))).content("Edit send"),
                Button::new().on_click(cx.callback(move |()| Msg::DisconnectSend(s, d))).content("Disconnect"),
                Button::new().on_click(cx.callback(|()| Msg::SelectWire(None))).content("Done"),
            )),
        ))
    } else {
        TextBlock::new().text(help).text_wrapping(TextWrapping::Wrap).into()
    };
    StackPanel::new().spacing(10.0).children((
        controls,
        scrolling,
        Button::new().on_click(cx.callback(move |()| Msg::Edit(scope))).content(label),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_patch_has_separate_send_lanes_and_no_crossings_into_outputs() {
        let scene = Scene::default();
        let layout = Layout::new(&scene, 625.0);
        let mut lanes: Vec<_> = layout.sources.iter().flat_map(|group| group.sends.iter().copied()).collect();
        assert_eq!(lanes.len(), 3);
        lanes.sort_by(|a, b| a.2.total_cmp(&b.2));
        for pair in lanes.windows(2) {
            assert!(pair[1].2 - pair[0].2 >= BRANCH_WIDTH);
            // A diagonal between adjacent rows crosses another only if the horizontal order is reversed.
            assert!(layout.outputs[pair[0].0] <= layout.outputs[pair[1].0]);
        }
    }

    #[test]
    fn dry_and_filtered_outputs_sit_directly_under_their_own_taps() {
        let mixer = Mixer::new(super::super::Appearance::default());
        let layout = Layout::new(&mixer.scene, 900.0);
        let sends = &layout.sources[0].sends;
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0], (0, 0, layout.outputs[0]));
        assert_eq!(sends[1], (1, 1, layout.outputs[1]));
        assert!(layout.outputs[2] >= layout.outputs[1] + BRANCH_WIDTH);
    }

    #[test]
    fn post_mix_effect_sits_between_the_junction_and_actual_drop_target() {
        let mut scene = Scene::default();
        let plain = Layout::new(&scene, 625.0);
        scene.scope = Scope::Output(0);
        scene.add(super::super::Kind::Gain);
        let layout = Layout::new(&scene, 625.0);
        assert!(layout.mix_y < layout.post_mix_y);
        assert!(layout.post_mix_y + NODE_HEIGHT < layout.output_y);
        let x = layout.outputs[0];
        assert_eq!(output_at(&layout.outputs, layout.output_y, x, plain.output_y + NODE_HEIGHT / 2.0), None);
        for (d, x) in layout.outputs.iter().enumerate() {
            assert_eq!(output_at(&layout.outputs, layout.output_y, *x, layout.output_y + NODE_HEIGHT / 2.0), Some(d));
        }
    }
}

#[cfg(test)]
#[path = "wiring_pointer_tests.rs"]
mod pointer_tests;
