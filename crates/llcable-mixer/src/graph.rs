//! Direct manipulation of the same sends as the mixer. Each source has one trunk, split before or after its shared rack.
use std::collections::BTreeMap;

use windows_reactor::*;

use crate::patch::{Inventory, SourceSend, Stage};
use crate::window::{Edit, Mixer, Msg, Scope};

const PITCH: f64 = 158.0;
const WIDTH: f64 = 144.0;
const NODE: f64 = 32.0;
const STEP: f64 = 42.0;
const TOP: f64 = 4.0;

type Layer = Vec<View>;

fn wire(lines: &mut Layer, x: f64, y: f64, ex: f64, ey: f64) {
    if x != ex || y != ey {
        lines.push(Line::new().x1(x).y1(y).x2(ex).y2(ey).stroke(ThemeBrush::Accent).stroke_thickness(1.8).into());
    }
}
fn dot(items: &mut Layer, x: f64, y: f64) {
    items.push(
        Border::new()
            .width(8.0)
            .height(8.0)
            .corner_radius(4.0)
            .canvas_left(x - 4.0)
            .canvas_top(y - 4.0)
            .background(ThemeBrush::Accent)
            .into(),
    );
}
fn node(label: &str, desc: &str, x: f64, y: f64, on: bool, click: Callback<()>) -> View {
    Button::new()
        .width(WIDTH)
        .height(NODE)
        .canvas_left(x - WIDTH / 2.0)
        .canvas_top(y)
        .style(if on { ButtonStyle::Accent } else { ButtonStyle::Default })
        .on_click(click)
        .automation_name(desc)
        .content(TextBlock::new().font_size(12.0).text(label).text_trimming(TextTrimming::CharacterEllipsis))
        .tooltip(desc)
}
fn rack(cx: &ViewContext<Mixer>, lines: &mut Layer, nodes: &mut Layer, x: f64, top: f64, stages: &[Stage], target: (Scope, &str)) -> f64 {
    let mut y = top;
    for stage in stages {
        let (scope, out) = (target.0.clone(), target.1.to_owned());
        let end = y + STEP - NODE;
        wire(lines, x, y, x, end);
        let text = if stage.enabled { stage.title().to_owned() } else { format!("{} (off)", stage.title()) };
        nodes.push(node(&text, &format!("Edit {text}"), x, end, false, cx.callback(move |()| Msg::WireRack(scope.clone(), out.clone()))));
        y += STEP;
    }
    y
}
fn lane_x(i: usize) -> f64 {
    PITCH * (i as f64 + 0.5)
}

fn socket(cx: &ViewContext<Mixer>, source: &str, x: f64, outputs: &[(String, f64)], sink_y: f64) -> View {
    let y = TOP + NODE - 3.0;
    let left = x - 12.0;
    let (pressed, moved, released, lost, cancel) = (cx.sender(), cx.sender(), cx.sender(), cx.sender(), cx.sender());
    let key = source.to_owned();
    let finish = key.clone();
    let outputs = outputs.to_vec();
    Border::new()
        .width(24.0)
        .height(24.0)
        .canvas_left(left)
        .canvas_top(y)
        .background(Color::transparent())
        .capture_pointer_on_press(true)
        .automation_name(format!("Drag socket for {source}"))
        .on_pointer_pressed(move |p: PointerEventInfo| {
            if p.capture_succeeded && p.is_left_button_pressed {
                pressed.send(Msg::WireSource(Some(key.clone())));
                pressed.send(Msg::WireDrag(Some((left + p.x, y + p.y))));
            }
        })
        .on_pointer_moved(move |p: PointerEventInfo| {
            if p.is_left_button_pressed {
                moved.send(Msg::WireDrag(Some((left + p.x, y + p.y))));
            }
        })
        .on_pointer_released(move |p: PointerEventInfo| {
            let ey = y + p.y;
            let ex = left + p.x;
            if (sink_y - 9.0..=sink_y + NODE).contains(&ey)
                && let Some((out, _)) = outputs.iter().find(|(_, centre)| (ex - centre).abs() <= WIDTH / 2.0)
            {
                released.send(Msg::Connect(out.clone(), finish.clone()));
            }
            released.send(Msg::WireDrag(None));
        })
        .on_pointer_capture_lost(move || {
            lost.send(Msg::WireDrag(None));
        })
        .on_pointer_canceled(move || {
            cancel.send(Msg::WireDrag(None));
        })
        .content(Border::new().width(9.0).height(9.0).corner_radius(5.0).background(ThemeBrush::Accent))
}

pub fn view(app: &Mixer, inv: &Inventory, cx: &ViewContext<Mixer>) -> View {
    let mut active: Vec<_> = inv
        .sources
        .iter()
        .filter(|s| {
            app.wire_source.as_ref() == Some(&s.id) || app.settings.mixes.values().any(|m| m.sends.iter().any(|v| v.source == s.id))
        })
        .collect();
    if active.is_empty() {
        active.extend(inv.sources.iter().take(1));
    }
    let mut groups = Vec::new();
    let mut count = 0;
    let mut shared_depth = 0;
    let mut send_depth = 0;
    for s in active {
        let mut sends: Vec<(&str, &SourceSend)> = app
            .settings
            .mixes
            .iter()
            .filter(|(id, _)| inv.outputs.iter().any(|c| c.id == **id))
            .filter_map(|(id, m)| m.sends.iter().find(|v| v.source == s.id).map(|v| (id.as_str(), v)))
            .collect();
        let len = app.settings.source_effects.get(&s.id).map_or(0, Vec::len);
        sends.sort_by_key(|(_, send)| send.depth(len));
        shared_depth = shared_depth.max(sends.iter().map(|(_, send)| send.depth(len)).max().unwrap_or(0));
        send_depth = send_depth.max(sends.iter().map(|(_, s)| s.effects.len()).max().unwrap_or(0));
        let width = sends.len().max(1);
        groups.push((s, sends, count, width));
        count += width;
    }
    let cols = count.max(inv.outputs.len()).max(1);
    let width = PITCH * cols as f64;
    let branch_top = TOP + NODE + 48.0 + (STEP + 4.0) * shared_depth as f64;
    let cross_top = branch_top + NODE + 12.0 + STEP * send_depth as f64;
    let mix_y = cross_top + 12.0 * inv.outputs.len() as f64 + 16.0;
    let post_depth = inv.outputs.iter().map(|c| app.settings.mixes.get(&c.id.clone()).map_or(0, |m| m.effects.len())).max().unwrap_or(0);
    let sink_y = mix_y + STEP * (post_depth + 1) as f64;
    // Order outputs by the mean positions of their feeding lanes. This also handles an unconnected destination.
    let mut positions: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for (_, sends, first, _) in &groups {
        for (i, (id, _)) in sends.iter().enumerate() {
            positions.entry(*id).or_default().push(lane_x(first + i));
        }
    }
    let mut outs: Vec<_> = inv.outputs.iter().collect();
    outs.sort_by(|a, b| {
        let average = |k: &str| positions.get(k).map(|v| v.iter().sum::<f64>() / v.len() as f64).unwrap_or(f64::MAX);
        average(&a.id).total_cmp(&average(&b.id))
    });
    let points: Vec<_> = outs.iter().enumerate().map(|(i, c)| (c.id.clone(), width * (i as f64 + 0.5) / outs.len() as f64)).collect();
    let mut lines = vec![];
    let mut nodes = vec![];
    let mut ports = vec![];
    let mut drag = vec![];
    for (s, sends, first, lanes) in &groups {
        let x = PITCH * (*first as f64 + *lanes as f64 / 2.0);
        let selected = app.wire_source.as_ref() == Some(&s.id);
        let key = s.id.clone();
        nodes.push(node(
            &s.label,
            &format!("Pick {} as wiring source", s.label),
            x,
            TOP,
            selected,
            cx.callback(move |()| Msg::WireSource((!selected).then(|| key.clone()))),
        ));
        ports.push(socket(cx, &s.id, x, &points, sink_y));
        if selected && let Some((ex, ey)) = app.wire_drag {
            wire(&mut drag, x, TOP + NODE, ex, ey);
        }
        if sends.is_empty() {
            continue;
        }
        let split_y = TOP + NODE + 18.0;
        wire(&mut lines, x, TOP + NODE, x, split_y);
        if sends.len() > 1 {
            dot(&mut nodes, x, split_y);
        }
        let shared = app.settings.source_effects.get(&s.id).map(Vec::as_slice).unwrap_or_default();
        let depth = sends.last().map_or(0, |(_, send)| send.depth(shared.len()));
        let mut points_at = vec![(x, split_y)];
        for j in 0..depth {
            // Keep the remaining series centred over the lanes which still use it.
            let start = sends.partition_point(|(_, v)| v.depth(shared.len()) <= j);
            let trunk_x = PITCH * ((*first + start) as f64 + (sends.len() - start) as f64 / 2.0);
            let (old_x, y) = points_at[j];
            wire(&mut lines, old_x, y, trunk_x, y);
            let bottom = rack(cx, &mut lines, &mut nodes, trunk_x, y, &shared[j..j + 1], (Scope::Source(s.id.clone()), sends[start].0));
            // Place the junction just below the effect, keeping the line clear of its face.
            let junction_y = bottom + 4.0;
            wire(&mut lines, trunk_x, bottom, trunk_x, junction_y);
            if sends.len() - start > 1 && sends[start].1.depth(shared.len()) == j + 1 {
                dot(&mut nodes, trunk_x, junction_y);
            }
            points_at.push((trunk_x, junction_y));
        }
        for (i, (out, send)) in sends.iter().enumerate() {
            let lx = lane_x(first + i);
            let (from_x, y) = points_at[send.depth(shared.len())];
            wire(&mut lines, from_x, y, lx, y);
            wire(&mut lines, lx, y, lx, branch_top);
            let info = format!(
                "{} → {}. Click to edit this send, or right-click to disconnect.",
                s.label,
                inv.outputs.iter().find(|c| c.id == *out).map(|c| c.name.clone()).unwrap_or_default()
            );
            let (k, o) = (s.id.clone(), out.to_string());
            let drag_scope = Scope::Send(o.clone(), k.clone());
            let remove = cx.sender();
            let output = o.clone();
            let content = Border::new()
                .background(Color::transparent())
                .on_pointer_pressed(move |p: PointerEventInfo| {
                    if p.is_right_button_pressed {
                        remove.send(Msg::Edit(Edit::Send(output.clone(), k.clone())));
                    }
                })
                .content(TextBlock::new().text(format!("{}  ·  {:+} dB", send.tap_name(shared), send.gain_db)));
            nodes.push(
                Button::new()
                    .width(WIDTH)
                    .height(NODE)
                    .canvas_left(lx - WIDTH / 2.0)
                    .canvas_top(branch_top)
                    .horizontal_content_alignment(HorizontalAlignment::Stretch)
                    .automation_name(&info)
                    .on_click(cx.callback(move |()| Msg::WireRack(drag_scope.clone(), o.clone())))
                    .content(content)
                    .tooltip(info),
            );
            let bottom =
                rack(cx, &mut lines, &mut nodes, lx, branch_top + NODE, &send.effects, (Scope::Send(out.to_string(), s.id.clone()), out));
            let (j, (_, ox)) = points.iter().enumerate().find(|(_, v)| v.0 == *out).expect("layout contains every connected cable");
            let y = cross_top + j as f64 * 12.0;
            wire(&mut lines, lx, bottom, lx, y);
            wire(&mut lines, lx, y, *ox, y);
            wire(&mut lines, *ox, y, *ox, mix_y);
        }
    }
    for ((id, x), c) in points.iter().zip(outs) {
        let mix = app.settings.mixes.get(id);
        let n = mix.map_or(0, |m| m.sends.len());
        let scope = Scope::Output(id.clone());
        let o = id.clone();
        if n > 0 {
            let mut y = mix_y;
            if n > 1 {
                let scope = scope.clone();
                let dest = o.clone();
                nodes.push(node(
                    &format!("Mix {n} sources"),
                    "Edit the output mix",
                    *x,
                    y,
                    false,
                    cx.callback(move |()| Msg::WireRack(scope.clone(), dest.clone())),
                ));
                y += NODE;
            }
            let bottom =
                rack(cx, &mut lines, &mut nodes, *x, y, mix.map(|m| m.effects.as_slice()).unwrap_or_default(), (scope.clone(), id));
            wire(&mut lines, *x, bottom, *x, sink_y);
        }
        let key = app.wire_source.clone();
        let name = c.name.clone();
        nodes.push(node(
            &name,
            &format!("{name}. Select a source then this output to connect it, or click to edit the mix."),
            *x,
            sink_y,
            false,
            cx.callback(move |()| match key.as_ref() {
                Some(key) => Msg::Connect(o.clone(), key.clone()),
                None => Msg::WireRack(scope.clone(), o.clone()),
            }),
        ));
    }
    // Keep the captured socket's native element alive when the temporary drag line appears/disappears.
    let keys = |tag: &'static str, views: Vec<View>| views.into_iter().enumerate().map(move |(i, v)| (format!("{tag}-{i}"), v));
    let graph = Canvas::new()
        .width(width)
        .height(sink_y + NODE + 4.0)
        .keyed_children(keys("wire", lines).chain(keys("drag", drag)).chain(keys("node", nodes)).chain(keys("socket", ports)));
    let available: Vec<_> = inv.sources.iter().map(|s| s.id.clone()).collect();
    let selected = available.iter().position(|s| Some(s) == app.wire_source.as_ref());
    let sender = cx.sender();
    let picker = ComboBox::new()
        .items_source(inv.sources.iter().map(|s| s.label.as_str()))
        .selected_index(selected)
        .on_selection_changed(move |choice: Option<usize>| {
            if choice != selected
                && let Some(s) = choice.and_then(|i| available.get(i))
            {
                sender.send(Msg::WireSource(Some(s.clone())));
            }
        })
        .slot(ComboBoxSlot::Header, "Source to connect");
    StackPanel::new().spacing(5.0).children((
        TextBlock::new()
            .text("Drag a source socket to an output, or select both ends. Click a send for its rack. Right-click a send to disconnect.")
            .text_wrapping(TextWrapping::Wrap),
        picker,
        ScrollViewer::new()
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
            .content(graph),
    ))
}
