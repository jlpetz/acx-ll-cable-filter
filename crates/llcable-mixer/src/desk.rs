//! The control panel stays passive. Mixer and Wiring are two editors of the same saved patch.
#[cfg(test)]
mod metric_tests;
mod metrics;

use llcable_audio::Level;
use llcable_control::EndpointKind;
use nf_gui::mixer_meter::{self, MeterMode, MeterPalette};
use windows_reactor::*;

use crate::patch::{Destination, Inventory, Kind, Source, Stage, Theme};
use crate::window::{Edit, Mixer, Msg, Page, Scope, StageEdit};

fn title(label: impl Into<String>, size: f64) -> View {
    TextBlock::new().text(label).font_size(size).font_weight(FontWeight::SEMI_BOLD).text_wrapping(TextWrapping::Wrap).into()
}
fn caption(label: impl Into<String>) -> View {
    TextBlock::new().text(label).font_size(12.0).opacity(0.82).text_wrapping(TextWrapping::Wrap).into()
}
fn issue(text: impl Into<String>) -> View {
    Border::new()
        .horizontal_alignment(HorizontalAlignment::Left)
        .background(ThemeBrush::SystemCriticalBackground)
        .corner_radius(4.0)
        .padding(Thickness::xy(6.0, 2.0))
        .content(
            TextBlock::new()
                .text(format!("Error · {}", text.into()))
                .foreground(ThemeBrush::SystemCritical)
                .font_size(12.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .text_wrapping(TextWrapping::Wrap),
        )
}
fn feedback(message: &Result<String, String>) -> View {
    match message {
        Ok(text) => caption(text),
        Err(text) => issue(text),
    }
}
fn row(items: Vec<View>) -> View {
    StackPanel::new().orientation(Orientation::Horizontal).spacing(6.0).keyed_children(items.into_iter().enumerate())
}
fn col(items: Vec<View>) -> View {
    StackPanel::new().spacing(6.0).keyed_children(items.into_iter().enumerate())
}
fn card(body: View) -> View {
    Border::new()
        .corner_radius(7.0)
        .background(ThemeBrush::CardBackground)
        .border_thickness(1.0)
        .border_brush(ThemeBrush::CardStroke)
        .padding(9.0)
        .content(body)
}
fn button(cx: &ViewContext<Mixer>, name: impl Into<String>, accent: bool, enabled: bool, action: impl Fn() -> Msg + 'static) -> View {
    Button::new()
        .is_enabled(enabled)
        .style(if accent { ButtonStyle::Accent } else { ButtonStyle::Default })
        .on_click(cx.callback(move |()| action()))
        .content(name.into())
}
fn right(left: View, right: View) -> View {
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(6.0)
        .children((left, Border::new().grid_column(1).content(right)))
}
fn slider(cx: &ViewContext<Mixer>, label: String, value: i16, min: i16, max: i16, action: impl Fn(i16) -> Msg + 'static) -> View {
    let sender = cx.sender();
    Slider::new()
        .minimum(f64::from(min))
        .maximum(f64::from(max))
        .step_frequency(1.0)
        .value(f64::from(value))
        .on_value_changed(move |v: f64| {
            let n = v.round() as i16;
            if n != value {
                sender.send(action(n));
            }
        })
        .slot(SliderSlot::Header, label)
}
fn input_level<'a>(app: &'a Mixer, id: &str) -> Option<&'a Level> {
    app.channels.inputs.iter().position(|s| s == id).and_then(|i| app.status.as_ref()?.inputs.get(i)).or_else(|| {
        let owner = app.inventory.as_ref()?.sources.iter().find(|s| s.id == id && !id.starts_with("loopback:"))?.owner?;
        output_level(app, &owner.to_string())
    })
}
fn output_level<'a>(app: &'a Mixer, id: &str) -> Option<&'a Level> {
    app.channels.outputs.iter().position(|s| s == id).and_then(|i| app.status.as_ref()?.outputs.get(i))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FeedState {
    Active,
    Paused,
    Offline,
}

fn feed_state(app: &Mixer, id: &str) -> FeedState {
    if id.parse().ok().and_then(|c| app.cable_enabled(c)) == Some(false) {
        return FeedState::Paused;
    }
    let offline = app.channels.outputs.iter().position(|s| s == id).and_then(|i| app.status.as_ref()?.outputs_offline.get(i));
    if offline == Some(&true) { FeedState::Offline } else { FeedState::Active }
}

fn meter(app: &Mixer, label: &str, level: Option<&Level>, channels: u16) -> View {
    let Some(level) = level else { return View::empty() };
    let mode = match app.settings.meter_mode {
        MeterMode::Auto if !app.settings.roomy => MeterMode::Mono,
        m => m,
    };
    let db = level.peak.map(|x| 20.0 * x.log10());
    mixer_meter::strip(label, &db[..if channels == 1 { 1 } else { 2 }], mode, app.settings.palette)
}
fn appearance(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let mut palettes = Vec::new();
    for (p, name) in [(MeterPalette::BlueFloor, "Blue at low levels"), (MeterPalette::Classic, "Classic green")] {
        palettes.push(button(cx, name, app.settings.palette == p, true, move || Msg::Palette(p)));
    }
    let mut channels = Vec::new();
    for (m, name) in [(MeterMode::Auto, "Automatic"), (MeterMode::Mono, "Mono peak"), (MeterMode::Stereo, "Stereo pair")] {
        channels.push(button(cx, name, app.settings.meter_mode == m, true, move || Msg::MeterMode(m)));
    }
    col(vec![
        title("Appearance", 22.0),
        card(col(vec![
            title("Window theme", 16.0),
            row([Theme::System, Theme::Dark, Theme::Light]
                .into_iter()
                .map(|t| button(cx, format!("{t:?}"), app.settings.theme == t, true, move || Msg::Theme(t)))
                .collect()),
        ])),
        card(col(vec![title("Meter colours", 16.0), row(palettes), caption(mixer_meter::BAND_LEGEND)])),
        card(col(vec![
            title("Channel display", 16.0),
            row(channels),
            caption("Automatic shows the loudest channel in compact mode and a stereo pair in roomy mode."),
        ])),
        card(col(vec![
            title("Spacing", 16.0),
            row(vec![
                button(cx, "Compact", !app.settings.roomy, true, || Msg::Roomy(false)),
                button(cx, "Roomy", app.settings.roomy, true, || Msg::Roomy(true)),
            ]),
        ])),
    ])
}

fn cable_controls(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let monitor = ToggleSwitch::new()
        .is_on(app.monitor.enabled())
        .automation_name("Monitor cables")
        .on_toggled(cx.callback(Msg::Monitor))
        .slots([
            SlotView::new(ToggleSwitchSlot::OnContent, "Monitor cables"),
            SlotView::new(ToggleSwitchSlot::OffContent, "Monitor cables"),
        ])
        .tooltip("Read each cable’s driver counters once a second while this panel is open.");
    let reset = button(cx, "Reset cable counters", false, app.monitor.enabled() && app.monitor.can_reset(), || Msg::ResetCableCounters)
        .tooltip("Start counting losses from zero in this panel. The driver keeps its totals.");
    let refresh = button(cx, "Refresh devices", false, !app.refreshing, || Msg::Refresh);
    let legacy = button(cx, "Open legacy app", false, true, || Msg::OpenLegacy);
    if app.width < 640.0 {
        col(vec![row(vec![monitor, reset]), row(vec![refresh, legacy])])
    } else {
        row(vec![monitor, reset, refresh, legacy])
    }
}

fn diagnostics(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let mut content = vec![
        title("Cable control panel", 22.0),
        cable_controls(app, cx),
        caption("Play to a cable’s In and record from its Out in any Windows app. The driver handles this while the mixer is stopped."),
    ];
    let Some(inv) = app.inventory.as_deref() else {
        content.push(device_discovery_status(app));
        return col(content);
    };
    if let Some(status) = &app.status {
        content.push(route_diagnostics(app, inv, status));
    }
    for c in &inv.cables {
        let mut items = vec![title(format!("LLCable-{}   ·   {}", c.label, c.health), 17.0)];
        for p in &c.ports {
            items.push(caption(format!(
                "{}   ·   {}{}",
                p.name,
                if p.role == EndpointKind::Speakers { "Playback" } else { "Recording" },
                if p.active { "" } else { " · Inactive" }
            )));
        }
        let diag = app.monitor.sample(c.id);
        if !app.monitor.enabled() && diag.is_some() {
            items.push(caption("Last reading · Monitoring off"));
        }
        match diag {
            Some(Ok(d)) => {
                let name = format!("LLCable-{}", c.label);
                let feed = if app.cable_enabled(c.id) == Some(false) {
                    "Direct feed paused"
                } else if d.attached {
                    "Direct feed attached"
                } else {
                    "Cable pass-through"
                };
                items.push(title(feed, 14.0));
                items.push(metrics::cable_properties(&name, d));
                if let Some(losses) = app.monitor.losses(c.id) {
                    items.push(metrics::cable_losses(&name, d, losses));
                }
            }
            Some(Err(e)) => items.push(issue(format!("Diagnostics unavailable · {e}"))),
            None if app.monitor.enabled() => items.push(caption("Reading driver statistics…")),
            None => {}
        }
        let id = c.id;
        if app.measuring.as_ref().is_some_and(|(c, _)| *c == id) {
            items.push(button(cx, "Cancel latency test", false, true, || Msg::CancelMeasurement));
        } else if [EndpointKind::Speakers, EndpointKind::Microphone]
            .into_iter()
            .all(|role| c.ports.iter().any(|p| p.active && p.role == role))
        {
            let attached = app.monitor.enabled() && matches!(diag, Some(Ok(d)) if d.attached);
            let available = !app.running() && !app.busy && app.measuring.is_none() && !attached;
            items.push(
                button(cx, "Measure cable latency", false, available, move || Msg::Measure(id))
                    .tooltip("Play three test sweeps through this cable. Stop its direct feed before measuring."),
            );
        }
        if let Some((_, report)) = app.measured.as_ref().filter(|(c, _)| *c == id) {
            items.push(feedback(report));
        }
        content.push(card(col(items)));
    }
    if inv.cables.is_empty() {
        content.push(caption("No LLCables found. Install or enable a cable, then refresh devices."));
    }
    col(content)
}

fn route_diagnostics(app: &Mixer, inv: &Inventory, status: &llcable_audio::RouteStatus) -> View {
    let mut running = vec![title("Live route diagnostics", 17.0), caption("Counts since this route started")];
    let inputs = app.channels.inputs.iter().zip(&status.inputs).map(|(id, level)| {
        let name = inv.sources.iter().find(|s| &s.id == id).map_or(id.as_str(), |s| s.label.as_str());
        ("Input", name, level, FeedState::Active)
    });
    let outputs = app.channels.outputs.iter().zip(&status.outputs).map(|(id, level)| {
        let name = inv.outputs.iter().find(|o| &o.id == id).map_or(id.as_str(), |o| o.name.as_str());
        ("Output", name, level, feed_state(app, id))
    });
    running.push(metrics::route_levels(&inputs.chain(outputs).collect::<Vec<_>>(), app.width));
    if !app.channels.meters.is_empty() {
        running.push(title("Effect diagnostics", 16.0));
    }
    for (stage, meter) in &app.channels.meters {
        running.push(metrics::effect(stage, meter.read()));
    }
    card(col(running))
}

fn stage_action(cx: &ViewContext<Mixer>, scope: &Scope, name: &str, enabled: bool, action: impl Fn() -> StageEdit + 'static) -> View {
    let scope = scope.clone();
    button(cx, name, false, enabled, move || Msg::Edit(Edit::Stage(scope.clone(), action())))
}
fn clap_controls(app: &Mixer, cx: &ViewContext<Mixer>, scope: &Scope, i: usize, s: &Stage) -> View {
    let pick = scope.clone();
    let mut controls =
        vec![button(cx, "Choose CLAP file…", false, !app.busy && !app.choosing, move || Msg::ChoosePlugin(pick.clone(), i))];
    if let Some(p) = &s.clap {
        controls.push(caption(format!("{} · isolated mono host", p.path.display())));
        for param in p.info.params.iter().filter(|p| p.min.is_finite() && p.max.is_finite() && p.min < p.max && p.default.is_finite()) {
            let value = p.params.get(&param.id).copied().unwrap_or(param.default).clamp(param.min, param.max);
            let (scope, id) = (scope.clone(), param.id);
            let sender = cx.sender();
            let field = NumberBox::new()
                .minimum(param.min)
                .maximum(param.max)
                .value(value)
                .on_value_changed(move |v: Option<f64>| {
                    if let Some(v) = v
                        && v.is_finite()
                        && v != value
                    {
                        sender.send(Msg::Edit(Edit::Stage(scope.clone(), StageEdit::Param(i, id, v))));
                    }
                })
                .slot(NumberBoxSlot::Header, param.name.as_str());
            controls.push(field);
        }
    }
    col(controls)
}
fn stage_stack(app: &Mixer, inv: &Inventory, cx: &ViewContext<Mixer>, scope: &Scope, stages: &[Stage]) -> View {
    let mut items = vec![title("Effect order", 16.0)];
    for (i, s) in stages.iter().enumerate() {
        let mut controls = vec![
            title(format!("{}   {}{}", i + 1, s.title(), if s.enabled { "" } else { " · bypassed" }), 15.0),
            row(vec![
                stage_action(cx, scope, "↑", i > 0, move || StageEdit::Move(i, -1)),
                stage_action(cx, scope, "↓", i + 1 < stages.len(), move || StageEdit::Move(i, 1)),
                stage_action(cx, scope, if s.enabled { "Bypass" } else { "Enable" }, true, move || StageEdit::Bypass(i)),
                stage_action(cx, scope, "Remove", true, move || StageEdit::Remove(i)),
            ]),
        ];
        if let Some((label, min, max)) = match s.kind {
            Kind::Gain => Some(("Gain", -60, 12)),
            Kind::Gate => Some(("Gate threshold", -90, 0)),
            Kind::Duck => Some(("Attenuation", 0, 60)),
            _ => None,
        } {
            let scope = scope.clone();
            controls.push(slider(cx, format!("{label} · {} dB", s.db), s.db, min, max, move |v| {
                Msg::Edit(Edit::Stage(scope.clone(), StageEdit::Value(i, v)))
            }));
        }
        if s.kind == Kind::Equalizer {
            for (band, name) in ["Low shelf · 180 Hz", "Mid bell · 1 kHz", "High shelf · 5 kHz"].into_iter().enumerate() {
                let target = scope.clone();
                let db = s.eq_db[band];
                controls.push(slider(cx, format!("{name} · {db:+} dB"), db, -18, 18, move |v| {
                    Msg::Edit(Edit::Stage(target.clone(), StageEdit::Band(i, band, v)))
                }));
            }
        }
        if matches!(s.kind, Kind::Duck | Kind::Aec) {
            let ids: Vec<_> = inv.sources.iter().map(|s| s.id.clone()).collect();
            let selected = ids.iter().position(|id| Some(id) == s.key.as_ref());
            let send = cx.sender();
            let target = scope.clone();
            let picker = ComboBox::new()
                .items_source(inv.sources.iter().map(|s| s.label.as_str()))
                .selected_index(selected)
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .on_selection_changed(move |choice: Option<usize>| {
                    if choice != selected
                        && let Some(key) = choice.and_then(|j| ids.get(j))
                    {
                        send.send(Msg::Edit(Edit::Stage(target.clone(), StageEdit::Key(i, key.clone()))));
                    }
                })
                .slot(ComboBoxSlot::Header, if s.kind == Kind::Aec { "Playback reference" } else { "Duck when this source is active" });
            controls.push(View::keyed_fragment([(
                format!("{:?}-{i}-{:?}", s.kind, inv.sources.iter().map(|s| &s.id).collect::<Vec<_>>()),
                picker,
            )]));
        }
        if matches!(s.kind, Kind::Aec | Kind::Rnnoise | Kind::Sonora) {
            controls.push(caption("Mono voice processing on this branch. Original sends retain their channels."));
        }
        if s.kind == Kind::Clap {
            controls.push(clap_controls(app, cx, scope, i, s));
        }
        items.push(card(col(controls)));
    }
    let send = cx.sender();
    let target = scope.clone();
    let add = ComboBox::new()
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .items_source(Kind::ALL.iter().map(|k| k.title()))
        .on_selection_changed(move |choice: Option<usize>| {
            if let Some(kind) = choice.and_then(|i| Kind::ALL.get(i)) {
                send.send(Msg::Edit(Edit::Stage(target.clone(), StageEdit::Add(*kind))));
            }
        })
        .slot(ComboBoxSlot::Header, "Add effect");
    items.push(View::keyed_fragment([(format!("add-{}", stages.len()), add)]));
    col(items)
}

fn source(app: &Mixer, inv: &Inventory, cx: &ViewContext<Mixer>, c: &Destination, s: &Source) -> View {
    let id = c.id.to_string();
    let name = c.name.clone();
    let send = app.settings.mixes.get(&id).and_then(|m| m.sends.iter().find(|v| v.source == s.id));
    let active = send.is_some();
    let (dest, key) = (id.clone(), s.id.clone());
    let toggle = button(cx, if active { "Connected" } else { "Connect" }, active, !app.busy && !c.receives_itself(s), move || {
        Msg::Edit(Edit::Send(dest.clone(), key.clone()))
    });
    let mut body = vec![right(title(&s.label, 15.0), toggle), meter(app, &s.label, input_level(app, &s.id), s.channels)];
    if c.receives_itself(s) {
        body.push(caption(format!("Connecting this source to {name} would create a feedback loop.")));
    }
    if let Some(send) = send {
        let shared = app.settings.source_effects.get(&s.id).map(Vec::as_slice).unwrap_or_default();
        let key = s.id.clone();
        let scope = Scope::Source(s.id.clone());
        let select = scope.clone();
        let branch = Scope::Send(id.clone(), s.id.clone());
        let edit_branch = branch.clone();
        let show = app.send_detail.as_ref() == Some(&key);
        body.push(row(vec![
            button(cx, format!("{:+} dB · {}", send.gain_db, send.tap_name(shared)), show, true, move || Msg::SendDetail(key.clone())),
            button(cx, "Source effects", app.scope.as_ref() == Some(&scope), true, move || Msg::Scope(select.clone())),
        ]));
        if show {
            body.push(button(cx, "Send effects", app.scope.as_ref() == Some(&branch), true, move || Msg::Scope(edit_branch.clone())));
            let (out, k) = (id.clone(), s.id.clone());
            body.push(slider(cx, format!("Send level · {:+} dB", send.gain_db), send.gain_db, -60, 12, move |v| {
                Msg::Edit(Edit::Level(out.clone(), k.clone(), v))
            }));
            let (out, k) = (id.clone(), s.id.clone());
            body.push(slider(
                cx,
                format!(
                    "Balance · {}",
                    if send.balance == 0 {
                        "Centre".into()
                    } else {
                        format!("{}% {}", send.balance.abs(), if send.balance < 0 { "left" } else { "right" })
                    }
                ),
                send.balance,
                -100,
                100,
                move |v| Msg::Edit(Edit::Balance(out.clone(), k.clone(), v)),
            ));
            let k = s.id.clone();
            let mut labels = vec!["Original signal".to_owned()];
            labels.extend(shared.iter().enumerate().map(|(i, stage)| format!("After {}. {}", i + 1, stage.title())));
            labels.push("After entire source rack".into());
            let last = labels.len() - 1;
            let chosen = send.boundary.map(|n| n.min(shared.len())).unwrap_or(if send.after_effects { last } else { 0 });
            let sender = cx.sender();
            let picker = ComboBox::new()
                .items_source(labels.iter().map(String::as_str))
                .selected_index(Some(chosen))
                .on_selection_changed(move |choice: Option<usize>| {
                    if let Some(i) = choice.filter(|&i| i != chosen && i <= last) {
                        sender.send(Msg::Edit(Edit::Tap(id.clone(), k.clone(), (i < last).then_some(i))));
                    }
                })
                .slot(ComboBoxSlot::Header, "Take this send from");
            body.push(View::keyed_fragment([(labels.join("\0"), picker)]));
        }
        if app.scope.as_ref() == Some(&branch) {
            body.push(stage_stack(app, inv, cx, &branch, &send.effects));
        }
        if app.scope.as_ref() == Some(&scope) {
            body.push(stage_stack(app, inv, cx, &scope, app.settings.source_effects.get(&s.id).map(Vec::as_slice).unwrap_or_default()));
        }
    }
    card(col(body))
}

fn mixer(app: &Mixer, inv: &Inventory, cx: &ViewContext<Mixer>) -> View {
    let mut left = vec![right(
        title("Destination mixes", 21.0),
        button(cx, "Presets", app.presets_open, true, {
            let open = app.presets_open;
            move || Msg::Presets(!open)
        }),
    )];
    if app.presets_open {
        let names: Vec<_> = app.settings.presets.keys().cloned().collect();
        let selected = names.iter().position(|n| n == app.preset_name.trim());
        let choices = names.clone();
        let sender = cx.sender();
        let mut controls = vec![title("Routing presets", 16.0)];
        if !names.is_empty() {
            let list = ComboBox::new()
                .items_source(names.iter().map(String::as_str))
                .placeholder_text("Saved presets")
                .selected_index(selected)
                .on_selection_changed(move |i: Option<usize>| {
                    if selected != i
                        && let Some(name) = i.and_then(|i| choices.get(i))
                    {
                        sender.send(Msg::PresetName(name.clone()));
                    }
                })
                .slot(ComboBoxSlot::Header, "Choose preset");
            controls.push(View::keyed_fragment([(names.join("\0"), list)]));
        }
        controls.push(
            TextBox::new()
                .text(app.preset_name.as_str())
                .on_text_changed(cx.callback(Msg::PresetName))
                .slot(TextBoxSlot::Header, "Preset name"),
        );
        controls.push(row(vec![
            button(cx, if selected.is_some() { "Replace" } else { "Save" }, false, !app.preset_name.trim().is_empty(), || Msg::SavePreset),
            button(cx, "Load", false, selected.is_some() && !app.busy && !app.choosing, || Msg::LoadPreset),
            button(cx, "Remove", false, selected.is_some() && !app.busy, || Msg::DeletePreset),
        ]));
        left.push(card(col(controls)));
    }
    let mut right_pane = Vec::new();
    for c in &inv.outputs {
        let id = c.id.to_string();
        let name = c.name.clone();
        let active = app.settings.focused.as_ref() == Some(&id);
        let mix = app.settings.mixes.get(&id);
        let count = mix.map_or(0, |m| m.sends.len());
        let feed = feed_state(app, &id);
        let status = match feed {
            FeedState::Paused => caption("Feed paused"),
            FeedState::Offline => issue("Device offline"),
            FeedState::Active if mix.is_some_and(|m| m.muted) => caption("Muted"),
            FeedState::Active => caption(format!("{count} connected {}", if count == 1 { "source" } else { "sources" })),
        };
        let context = cx.sender();
        let context_id = id.clone();
        let face = Border::new()
            .background(Color::transparent())
            .on_pointer_pressed(move |p: PointerEventInfo| {
                if p.is_right_button_pressed {
                    context.send(Msg::InspectCable(context_id.clone()));
                }
            })
            .content(col(vec![title(&name, 16.0), status, meter(app, &name, output_level(app, &id), 2)]));
        let choose = id.clone();
        left.push(
            Button::new()
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .horizontal_content_alignment(HorizontalAlignment::Stretch)
                .automation_name(match feed {
                    FeedState::Paused => format!("Select {name} destination mix, feed paused"),
                    FeedState::Offline => format!("Select {name} destination mix, device offline"),
                    FeedState::Active => format!("Select {name} destination mix"),
                })
                .style(if active { ButtonStyle::Accent } else { ButtonStyle::Default })
                .on_click(cx.callback(move |()| Msg::Select(choose.clone())))
                .content(face)
                .tooltip("Select or collapse this mix. Right-click for driver diagnostics."),
        );
        if active {
            right_pane.push(title(format!("What should {name} hear?"), 19.0));
            let scope = Scope::Output(id.clone());
            let edit = scope.clone();
            let mute = id.clone();
            let mut actions = vec![
                button(cx, "Output effects", app.scope.as_ref() == Some(&scope), true, move || Msg::Scope(edit.clone())),
                button(cx, if mix.is_some_and(|m| m.muted) { "Unmute output" } else { "Mute output" }, false, !app.busy, move || {
                    Msg::Edit(Edit::Mute(mute.clone()))
                }),
            ];
            if let Some(cable) = c.cable
                && let Some(enabled) = app.cable_enabled(cable)
            {
                let available = !app.busy && app.status.as_ref().is_some_and(|s| s.running);
                actions.push(
                    button(cx, if enabled { "Pause feed" } else { "Resume feed" }, false, available, move || {
                        Msg::CableEnabled(cable, !enabled)
                    })
                    .tooltip(if enabled {
                        "Release this cable for Windows playback."
                    } else {
                        "Feed this mix to the cable again."
                    }),
                );
            }
            right_pane.push(row(actions));
            if app.scope.as_ref() == Some(&scope) {
                right_pane.push(card(stage_stack(app, inv, cx, &scope, mix.map(|m| m.effects.as_slice()).unwrap_or_default())));
            }
            for s in &inv.sources {
                right_pane.push(source(app, inv, cx, c, s));
            }
            if let Some(m) = mix {
                for missing in m.sends.iter().filter(|v| !inv.sources.iter().any(|s| s.id == v.source)) {
                    let key = missing.source.clone();
                    let out = id.clone();
                    right_pane.push(card(right(
                        caption(inv.label(&key)),
                        button(cx, "Disconnect", false, true, move || Msg::Edit(Edit::Send(out.clone(), key.clone()))),
                    )));
                }
            }
        }
    }
    if left.len() == 1 {
        left.push(caption("No output devices found. Enable a cable or playback device, then refresh."));
    }
    for id in app.settings.mixes.keys().filter(|id| !inv.outputs.iter().any(|c| c.id == **id)) {
        let key = id.clone();
        left.push(card(col(vec![
            caption(format!("Unavailable cable {id}")),
            button(cx, "Forget this mix", false, true, move || Msg::Edit(Edit::Forget(key.clone()))),
        ])));
    }
    if right_pane.is_empty() {
        return col(left);
    }
    if app.width < 730.0 {
        return col(vec![col(left), col(right_pane)]);
    }
    Grid::new()
        .columns([GridLength::Star(0.85), GridLength::Star(1.3)])
        .column_spacing(16.0)
        .children((col(left), Border::new().grid_column(1).content(col(right_pane))))
}

fn wiring(app: &Mixer, inv: &Inventory, cx: &ViewContext<Mixer>) -> View {
    let mut cells = vec![caption("Sources ↓   Outputs →")];
    for (i, out) in inv.outputs.iter().enumerate() {
        cells.push(Border::new().grid_column(i as i32 + 1).content(title(&out.name, 13.0)));
    }
    for (j, source) in inv.sources.iter().enumerate() {
        let r = j as i32 + 1;
        cells.push(Border::new().grid_row(r).content(title(&source.label, 13.0)));
        for (i, out) in inv.outputs.iter().enumerate() {
            let active = app.settings.mixes.get(&out.id).is_some_and(|m| m.sends.iter().any(|v| v.source == source.id));
            let (id, key) = (out.id.clone(), source.id.clone());
            let label = format!("{} {} {}", if active { "Disconnect" } else { "Connect" }, source.label, out.name);
            let switch = Button::new()
                .style(if active { ButtonStyle::Accent } else { ButtonStyle::Default })
                .is_enabled(!app.busy && !out.receives_itself(source))
                .automation_name(&label)
                .on_click(cx.callback(move |()| Msg::Edit(Edit::Send(id.clone(), key.clone()))))
                .content(if active { "On" } else { "Off" })
                .tooltip(label);
            cells.push(Border::new().grid_row(r).grid_column(i as i32 + 1).content(switch));
        }
    }
    let columns = std::iter::once(GridLength::Pixel(168.0)).chain(inv.outputs.iter().map(|_| GridLength::Pixel(105.0)));
    let matrix = Grid::new()
        .columns(columns)
        .rows(vec![GridLength::Auto; inv.sources.len() + 1])
        .row_spacing(4.0)
        .column_spacing(4.0)
        .keyed_children(cells.into_iter().enumerate());
    col(vec![
        title("Signal wiring", 22.0),
        crate::graph::view(app, inv, cx),
        title("Send matrix", 18.0),
        ScrollViewer::new()
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
            .content(matrix),
    ])
}

fn device_discovery_status(app: &Mixer) -> View {
    if app.refreshing { caption("Finding audio devices…") } else { View::empty() }
}

pub fn view(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let mut nav = Vec::new();
    for (page, label) in [(Page::Cables, "Cables"), (Page::Mixer, "Mixer"), (Page::Wiring, "Wiring"), (Page::Appearance, "Appearance")] {
        nav.push(button(cx, label, app.page == page, true, move || Msg::Page(page)));
    }
    let mut header = vec![row(nav)];
    let route_failure = app
        .status
        .as_ref()
        .filter(|_| !app.busy)
        .and_then(|s| s.error.as_deref().or_else(|| (!s.running).then_some("The routing worker ended unexpectedly.")));
    if app.page == Page::Mixer || app.page == Page::Wiring || app.running() {
        let running = app.running();
        let dirty = app.changed();
        let mut actions = vec![button(
            cx,
            if running { "Apply changes" } else { "Start audio" },
            !running || dirty,
            !app.busy && app.measuring.is_none() && app.inventory.is_some() && (!running || dirty || route_failure.is_some()),
            || Msg::Start,
        )];
        if running {
            actions.push(button(cx, "Stop audio", false, !app.busy, || Msg::Stop));
        }
        let needs_feed = app
            .inventory
            .iter()
            .flat_map(|inv| &inv.outputs)
            .any(|o| o.cable.is_some() && app.settings.mixes.get(&o.id).is_some_and(|m| !m.sends.is_empty()));
        if needs_feed && !app.elevated && !llcable_control::authorized() {
            actions.push(button(cx, "Authorize direct feed…", false, !app.granting && !app.busy, || Msg::Authorize));
        }
        if app.page != Page::Cables {
            actions.push(button(cx, "Refresh devices", false, !app.refreshing, || Msg::Refresh));
        }
        header.push(row(actions));
        header.push(if app.busy {
            caption("Updating audio…")
        } else if let Some(reason) = route_failure {
            issue(format!("Route stopped · {reason}"))
        } else if running && dirty {
            caption("Audio running · Unapplied changes")
        } else if running {
            caption("Audio running")
        } else {
            caption("Audio stopped")
        });
    }
    if let Some(error) = &app.discovery_error {
        header.push(issue(error));
    }
    if let Some(message) = &app.notice
        && !matches!(message, Err(e) if Some(e.as_str()) == route_failure || Some(e) == app.discovery_error.as_ref())
    {
        header.push(feedback(message));
    }
    let page = match app.page {
        Page::Appearance => appearance(app, cx),
        Page::Cables => diagnostics(app, cx),
        p => match app.inventory.as_deref() {
            Some(inv) if p == Page::Mixer => mixer(app, inv, cx),
            Some(inv) => wiring(app, inv, cx),
            None => device_discovery_status(app),
        },
    };
    Grid::new()
        .rows([GridLength::Auto, GridLength::STAR])
        .row_spacing(if app.settings.roomy { 18.0 } else { 10.0 })
        .margin(if app.settings.roomy { 20.0 } else { 12.0 })
        .children((col(header), ScrollViewer::new().grid_row(1).vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(page)))
}
