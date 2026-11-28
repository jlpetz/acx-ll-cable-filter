//! Interactive WinUI mixer design. Routes and meters use a deterministic local scene. Only appearance is persisted.
#![windows_subsystem = "windows"]

mod appearance;
mod desk;
mod diagnostics;
#[path = "../routing_preview/editor.rs"]
mod editor;
#[cfg(test)]
mod interaction_tests;
mod local;
mod meters;
#[path = "../routing_preview/scene.rs"]
mod scene;
#[path = "../routing_preview/widgets.rs"]
mod widgets;
mod wiring;

use std::time::Duration;

use appearance::Appearance;
use scene::*;
use widgets::*;
use windows_reactor::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Mixer,
    Wiring,
    Effects,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Diagnostics,
    Appearance,
}

enum Msg {
    Page(Page),
    Panel(Panel),
    Back,
    Output(usize),
    PickOutput(usize),
    OutputActions(Option<usize>),
    InspectOutput(usize),
    AllSends(bool),
    ToggleSend(usize, usize),
    ConnectSend(usize, usize),
    DisconnectSend(usize, usize),
    SelectWire(Option<(usize, usize)>),
    Tap(usize, usize, Tap),
    Gain(usize, usize, i32),
    Mute(usize),
    Edit(Scope),
    EditStage(Scope, u32),
    DeleteStage(Scope, u32),
    Catalog,
    Add(Kind),
    StageDetails(u32),
    SetValue(u32, i32),
    Move(u32, i8),
    Bypass(u32),
    Remove(u32),
    CopyStack,
    PasteStack,
    WireSource(Option<usize>),
    WireDrag(Option<(usize, f64, f64)>),
    Appearance(Appearance),
    ShowChannels(bool),
    DemoSignal(bool),
    OpenLive,
    Size(WindowSize),
    Tick,
}

type Preview = Mixer;

struct Mixer {
    scene: Scene,
    page: Page,
    panel: Option<Panel>,
    output: usize,
    focus_open: bool,
    output_actions: Option<usize>,
    all_sends: bool,
    prefs: Appearance,
    muted: [bool; DESTS.len()],
    tick: u32,
    demo_signal: bool,
    clock: Option<ComponentTask>,
    width: f64,
    wire_source: Option<usize>,
    selected_wire: Option<(usize, usize)>,
    wire_drag: Option<(usize, f64, f64)>,
    wire_gesture: wiring::Gesture,
    selected_stage: Option<u32>,
    catalog: bool,
    copied: Option<(String, Vec<Stage>)>,
    show_channels: bool,
    notice: Option<String>,
}

impl Mixer {
    fn new(prefs: Appearance) -> Self {
        let mut scene = Scene::empty(Scope::Source(0));
        scene.toggle(0, 0);
        scene.sends[0][0].tap = Tap::Raw;
        scene.toggle(0, 1);
        scene.add(Kind::Noise);
        Self {
            scene,
            page: Page::Mixer,
            panel: None,
            output: 1,
            focus_open: true,
            output_actions: None,
            all_sends: false,
            prefs,
            muted: [false; DESTS.len()],
            tick: 0,
            demo_signal: true,
            clock: None,
            width: 1000.0,
            wire_source: None,
            selected_wire: None,
            wire_drag: None,
            wire_gesture: Default::default(),
            selected_stage: None,
            catalog: false,
            copied: None,
            show_channels: false,
            notice: None,
        }
    }

    fn edit(&mut self, scope: Scope) {
        self.page = Page::Effects;
        self.panel = None;
        self.scene.scope = scope;
        if let Scope::Send(_, d) | Scope::Output(d) = scope {
            self.output = d;
            self.focus_open = true;
        }
        self.selected_stage = None;
        self.catalog = false;
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Page(page) => {
                self.page = page;
                self.panel = None;
            }
            Msg::Panel(panel) => self.panel = (self.panel != Some(panel)).then_some(panel),
            Msg::Back => self.panel = None,
            Msg::Output(d) | Msg::InspectOutput(d) => {
                self.output = d;
                self.focus_open = true;
                self.output_actions = None;
                if matches!(msg, Msg::InspectOutput(_)) {
                    self.panel = Some(Panel::Diagnostics);
                }
            }
            Msg::PickOutput(d) => {
                self.focus_open = self.all_sends || self.output != d || !self.focus_open;
                self.output = d;
                self.all_sends = false;
                self.output_actions = None;
            }
            Msg::OutputActions(d) => self.output_actions = d,
            Msg::AllSends(show) => {
                self.all_sends = show;
                self.output_actions = None;
            }
            Msg::ToggleSend(s, d) => self.scene.toggle(s, d),
            Msg::ConnectSend(s, d) => self.scene.sends[s][d].connected = true,
            Msg::DisconnectSend(s, d) => {
                if self.scene.connected(s, d) {
                    self.scene.toggle(s, d);
                }
            }
            Msg::SelectWire(wire) => {
                self.selected_wire = wire.filter(|&(s, d)| self.scene.connected(s, d));
                self.wire_source = None;
            }
            Msg::Tap(s, d, tap) => {
                if self.scene.connected(s, d) && self.scene.taps(s).iter().any(|(t, _)| *t == tap) {
                    self.scene.sends[s][d].tap = tap;
                }
            }
            Msg::Gain(s, d, db) => {
                if self.scene.connected(s, d) {
                    self.scene.sends[s][d].gain_db = db.clamp(-48, 12);
                }
            }
            Msg::Mute(d) => self.muted[d] = !self.muted[d],
            Msg::Edit(scope) => self.edit(scope),
            Msg::EditStage(scope, id) => {
                if self.scene.at(scope).iter().any(|stage| stage.id == id) {
                    self.edit(scope);
                    self.selected_stage = Some(id);
                }
            }
            Msg::DeleteStage(scope, id) => {
                self.scene.remove_from(scope, id);
                if self.selected_stage == Some(id) {
                    self.selected_stage = None;
                }
            }
            Msg::Catalog => self.catalog = !self.catalog,
            Msg::Add(kind) => {
                self.selected_stage = Some(self.scene.add(kind));
                self.catalog = false;
            }
            Msg::StageDetails(id) => self.selected_stage = (self.selected_stage != Some(id)).then_some(id),
            Msg::SetValue(id, value) => {
                if let Some(stage) = self.scene.stage_mut(id) {
                    let (min, max) = stage.kind.range();
                    stage.value = value.clamp(min, max);
                }
            }
            Msg::Move(id, step) => self.scene.move_stage(id, step),
            Msg::Bypass(id) => {
                if let Some(stage) = self.scene.stage_mut(id) {
                    stage.enabled = !stage.enabled;
                }
            }
            Msg::Remove(id) => {
                self.scene.remove(id);
                self.selected_stage = None;
            }
            Msg::CopyStack => self.copied = Some((self.scene.scope.title(), self.scene.stages().to_vec())),
            Msg::PasteStack => {
                if let Some((_, stages)) = &self.copied {
                    self.scene.load_stack(stages);
                    self.selected_stage = None;
                    self.catalog = false;
                }
            }
            Msg::WireSource(s) => {
                self.wire_source = s;
                self.selected_wire = None;
            }
            Msg::WireDrag(drag) => self.wire_drag = drag,
            Msg::Appearance(prefs) => {
                if self.prefs != prefs {
                    self.prefs = prefs;
                    self.notice = local::save_appearance(prefs).err();
                }
            }
            Msg::ShowChannels(expanded) => self.show_channels = expanded,
            Msg::DemoSignal(on) => self.demo_signal = on,
            Msg::OpenLive => self.notice = local::open_live().err(),
            Msg::Size(size) if size.width.is_finite() && size.width > 0.0 => self.width = size.width,
            Msg::Size(_) => {}
            Msg::Tick => {
                self.clock = None;
                if self.demo_signal {
                    self.tick = self.tick.wrapping_add(1);
                }
            }
        }
        if self.page != Page::Wiring || self.panel.is_some() {
            self.wire_source = None;
            self.wire_drag = None;
            self.wire_gesture.reset();
            self.selected_wire = None;
        } else if self.selected_wire.is_some_and(|(s, d)| !self.scene.connected(s, d)) {
            self.selected_wire = None;
        }
        if self.page != Page::Mixer || self.panel.is_some() {
            self.output_actions = None;
        }
    }

    fn arm_clock(&mut self, cx: &ComponentContext<Self>) {
        let show_meters = matches!((self.page, self.panel), (Page::Mixer, None) | (_, Some(Panel::Diagnostics)));
        if self.demo_signal
            && show_meters
            && !self.clock.as_ref().is_some_and(|t| matches!(t.status(), ComponentTaskStatus::Running | ComponentTaskStatus::Queued))
        {
            self.clock = Some(cx.spawn_background(|_| {
                std::thread::sleep(Duration::from_millis(200));
                Msg::Tick
            }));
        }
    }

    fn input_levels(&self, s: usize) -> Vec<f32> {
        let mut peaks = meters::source_levels(s, self.tick);
        if !self.demo_signal {
            peaks.fill(f32::NEG_INFINITY);
        }
        peaks
    }

    /// The demo sums aligned sample peaks in the linear domain. Noise suppression and echo cancellation have no synthetic transfer curve.
    fn mix_levels_before_output(&self, d: usize) -> Vec<f32> {
        let mut sum = [0.0; 2];
        for s in 0..SOURCES.len() {
            let send = self.scene.sends[s][d];
            if !send.connected {
                continue;
            }
            let shared = self.scene.source_stages_for_send(s, d);
            let input = self.input_levels(s);
            for (ch, amplitude) in sum.iter_mut().enumerate() {
                let db = model_level(input[ch.min(input.len() - 1)], shared);
                let db = model_level(db, self.scene.at(Scope::Send(s, d))) + send.gain_db as f32;
                *amplitude += 10.0_f32.powf(db / 20.0);
            }
        }
        sum.into_iter().map(|a| 20.0 * a.log10()).collect()
    }

    fn output_levels(&self, d: usize) -> Vec<f32> {
        if self.muted[d] {
            return vec![f32::NEG_INFINITY; 2];
        }
        self.mix_levels_before_output(d).into_iter().map(|db| model_level(db, self.scene.at(Scope::Output(d)))).collect()
    }

    fn tap_name(&self, s: usize, d: usize) -> String {
        self.scene.taps(s).into_iter().find(|(tap, _)| *tap == self.scene.sends[s][d].tap).map_or_else(String::new, |(_, name)| name)
    }

    fn send_options(&self, cx: &ViewContext<Self>, s: usize, d: usize) -> View {
        let choices = self.scene.taps(s);
        let ids: Vec<_> = choices.iter().map(|(tap, _)| *tap).collect();
        let send = self.scene.sends[s][d];
        let sender = cx.sender();
        let tap = ComboBox::new()
            .items_source(choices.iter().map(|(_, label)| label.as_str()))
            .selected_index(ids.iter().position(|tap| *tap == send.tap))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .on_selection_changed(move |i: Option<usize>| {
                if let Some(&tap) = i.and_then(|i| ids.get(i)) {
                    sender.send(Msg::Tap(s, d, tap));
                }
            })
            .slot(ComboBoxSlot::Header, "Take signal from");
        let slider = Slider::new()
            .minimum(-48.0)
            .maximum(12.0)
            .step_frequency(1.0)
            .value(f64::from(send.gain_db))
            .on_value_changed(cx.callback(move |v: f64| Msg::Gain(s, d, v.round() as i32)))
            .slot(SliderSlot::Header, format!("Send gain  ·  {:+} dB", send.gain_db));
        card(column(vec![View::keyed_fragment([(format!("{s}-{d}-{choices:?}"), tap)]), slider]))
    }

    fn effects(&self, cx: &ViewContext<Self>) -> View {
        let mut scopes = Vec::new();
        for (s, name) in SOURCES.iter().enumerate() {
            scopes.push((Scope::Source(s), format!("{name}  ·  shared source")));
            for (d, output) in DESTS.iter().enumerate().filter(|(d, _)| self.scene.connected(s, *d)) {
                scopes.push((Scope::Send(s, d), format!("{name} → {output}  ·  this send")));
            }
        }
        scopes.extend(DESTS.iter().enumerate().map(|(d, name)| (Scope::Output(d), format!("{name}  ·  after mix"))));
        let ids: Vec<_> = scopes.iter().map(|(scope, _)| *scope).collect();
        let active = self.scene.scope;
        let sender = cx.sender();
        let picker = ComboBox::new()
            .items_source(scopes.iter().map(|(_, name)| name.as_str()))
            .selected_index(ids.iter().position(|scope| *scope == active))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .on_selection_changed(move |i: Option<usize>| {
                if let Some(&scope) = i.and_then(|i| ids.get(i))
                    && scope != active
                {
                    sender.send(Msg::Edit(scope));
                }
            })
            .slot(ComboBoxSlot::Header, "Edit effects on");
        let kinds: Vec<_> = Kind::ALL.into_iter().filter(|k| matches!(k, Kind::Echo | Kind::Noise | Kind::Gate | Kind::Gain)).collect();
        let mut body = vec![View::keyed_fragment([(format!("{scopes:?}"), picker)]), editor::editor(self, cx, &kinds)];
        if let Scope::Send(s, d) = active {
            body.push(self.send_options(cx, s, d));
        }
        let mut reuse = vec![action("Copy pipeline", cx.callback(|()| Msg::CopyStack), false)];
        if let Some((from, _)) = &self.copied {
            reuse.push(action("Replace with copy", cx.callback(|()| Msg::PasteStack), false));
            body.push(small(&format!("Copied pipeline from {from}")).into());
        }
        body.push(row(reuse));
        column(body)
    }

    fn header(&self, cx: &ViewContext<Self>) -> View {
        let nav = row([(Page::Mixer, "Mixer"), (Page::Wiring, "Wiring"), (Page::Effects, "Effects")]
            .into_iter()
            .map(|(page, name)| action(name, cx.callback(move |()| Msg::Page(page)), self.page == page && self.panel.is_none()))
            .collect());
        let tools = row([(Panel::Diagnostics, "Diagnostics"), (Panel::Appearance, "Appearance")]
            .into_iter()
            .map(|(panel, name)| action(name, cx.callback(move |()| Msg::Panel(panel)), self.panel == Some(panel)))
            .collect());
        let bar = Grid::new()
            .columns([GridLength::STAR, GridLength::Auto])
            .column_spacing(8.0)
            .children((nav, Border::new().grid_column(1).content(tools)));
        let title = TextBlock::new().text("Cable mixer").font_size(28.0).font_weight(FontWeight::SEMI_BOLD);
        let simulated = self.demo_signal;
        let sender = cx.sender();
        let signal_switch = ToggleSwitch::new()
            .is_on(simulated)
            .automation_name("Demo signal")
            .on_toggled(move |on| {
                if on != simulated {
                    sender.send(Msg::DemoSignal(on));
                }
            })
            .slots([
                SlotView::new(ToggleSwitchSlot::OnContent, "Demo signal on"),
                SlotView::new(ToggleSwitchSlot::OffContent, "Demo signal off"),
            ])
            .tooltip("Drive the simulated meters, or switch off for digital silence.");
        let identity = Grid::new().columns([GridLength::STAR, GridLength::Auto]).children((
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(10.0)
                .children((title, muted("V2 prototype").vertical_alignment(VerticalAlignment::Center))),
            Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Center).content(signal_switch),
        ));
        StackPanel::new().margin(Thickness::new(20.0, 12.0, 20.0, 8.0)).spacing(8.0).children((identity, bar))
    }
}

fn model_level(input: f32, chain: &[Stage]) -> f32 {
    chain.iter().filter(|stage| stage.enabled).fold(input, |db, stage| match stage.kind {
        Kind::Gain => db + stage.value as f32,
        Kind::Gate if db < stage.value as f32 => f32::NEG_INFINITY,
        _ => db,
    })
}

impl Component for Mixer {
    type Input = ();
    type Message = Msg;

    fn create(_: &(), cx: &ComponentContext<Self>) -> Self {
        let mut app = Self::new(local::load_appearance());
        app.arm_clock(cx);
        app
    }

    fn update(&mut self, message: Msg, cx: &ComponentContext<Self>) {
        self.apply(message);
        self.arm_clock(cx);
    }

    fn view(&self, _: &(), cx: &mut ViewContext<Self>) -> View {
        cx.window_title("ACX-LL-Cable · Mixer v2");
        cx.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(self.prefs.window_theme())
                .client_size(1000.0, 660.0)
                .constraints(WindowConstraints { min_width: Some(565.0), min_height: Some(420.0), ..Default::default() }),
        );
        cx.on_window_size(cx.callback(Msg::Size));
        let body = if let Some(panel) = self.panel {
            let (label, content) = match panel {
                Panel::Appearance => ("Appearance", appearance::view(self, cx)),
                Panel::Diagnostics => ("Diagnostics", diagnostics::view(self, cx)),
            };
            column(vec![
                Grid::new()
                    .columns([GridLength::STAR, GridLength::Auto])
                    .children((heading(label), Border::new().grid_column(1).content(action("Back", cx.callback(|()| Msg::Back), false)))),
                content,
            ])
        } else {
            match self.page {
                Page::Mixer => desk::view(self, cx),
                Page::Wiring => wiring::view(self, cx),
                Page::Effects => self.effects(cx),
            }
        };
        let mut parts = Vec::new();
        if let Some(msg) = &self.notice {
            parts.push(muted(msg).into());
        }
        parts.push(body);
        let body =
            StackPanel::new().spacing(8.0).margin(Thickness::new(20.0, 6.0, 20.0, 18.0)).keyed_children(parts.into_iter().enumerate());
        Grid::new().rows([GridLength::Auto, GridLength::STAR]).children((
            self.header(cx),
            ScrollViewer::new().grid_row(1).vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(body),
        ))
    }
}

fn main() {
    if let Err(error) = App::run_component::<Mixer>(()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
