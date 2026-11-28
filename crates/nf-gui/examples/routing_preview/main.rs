//! Native, in-memory design study. The three views edit the same example; no devices, plugins or settings files are opened.
#![windows_subsystem = "windows"]

mod editor;
#[cfg(test)]
mod interaction_tests;
mod paths;
mod routes;
mod scene;
#[cfg(test)]
mod scene_tests;
mod strips;
mod widgets;

use scene::*;
use widgets::*;
use windows_reactor::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Concept {
    Paths,
    Matrix,
    Mixer,
}

impl Concept {
    const ALL: [(Self, &str); 3] = [(Self::Paths, "Signal paths"), (Self::Matrix, "Routing matrix"), (Self::Mixer, "Mixer strips")];
}

enum Msg {
    Concept(Concept),
    Select(Scope),
    Send(usize, usize),
    Sends(usize),
    Catalog,
    Add(Kind),
    StageDetails(u32),
    Move(u32, i8),
    Remove(u32),
    Bypass(u32),
    SetValue(u32, i32),
    SendOptions,
    SetTap(usize, usize, Tap),
    SendGain(usize, usize, i32),
    StackActions,
    SaveStack,
    LoadStack,
    Reset,
    Size(WindowSize),
    ShowPane(bool),
}

struct Preview {
    concept: Concept,
    scene: Scene,
    sends_open: Option<usize>,
    selected_stage: Option<u32>,
    catalog: bool,
    send_options: bool,
    stack_actions: bool,
    saved_stack: Option<(String, Vec<Stage>)>,
    width: f64,
    editor_pane: bool,
}

impl Preview {
    fn new(concept: Concept) -> Self {
        Self {
            concept,
            scene: Scene::default(),
            sends_open: None,
            selected_stage: None,
            catalog: false,
            send_options: false,
            stack_actions: false,
            saved_stack: None,
            width: 960.0,
            editor_pane: false,
        }
    }

    fn select(&mut self, scope: Scope) {
        self.scene.scope = scope;
        self.selected_stage = None;
        self.catalog = false;
        self.send_options = false;
        self.stack_actions = false;
        self.editor_pane = true;
    }

    fn receive(&mut self, message: Msg) {
        match message {
            Msg::Concept(choice) => {
                self.concept = choice;
                self.editor_pane = false;
            }
            Msg::Select(scope) => self.select(scope),
            Msg::Send(s, d) => {
                let scope = self.scene.scope;
                self.scene.toggle(s, d);
                if self.scene.scope != scope {
                    self.select(self.scene.scope);
                    self.editor_pane = false;
                }
            }
            Msg::Sends(s) => self.sends_open = (self.sends_open != Some(s)).then_some(s),
            Msg::Catalog => self.catalog = !self.catalog,
            Msg::Add(kind) => {
                self.selected_stage = Some(self.scene.add(kind));
                self.catalog = false;
            }
            Msg::StageDetails(id) => self.selected_stage = (self.selected_stage != Some(id)).then_some(id),
            Msg::Move(id, step) => self.scene.move_stage(id, step),
            Msg::Remove(id) => {
                self.scene.remove(id);
                self.selected_stage = None;
            }
            Msg::Bypass(id) => {
                if let Some(stage) = self.scene.stage_mut(id) {
                    stage.enabled = !stage.enabled;
                }
            }
            Msg::SetValue(id, value) => {
                if let Some(stage) = self.scene.stage_mut(id) {
                    let (lo, hi) = stage.kind.range();
                    stage.value = value.clamp(lo, hi);
                }
            }
            Msg::SendOptions => self.send_options = !self.send_options,
            Msg::SetTap(s, d, tap) => {
                if self.scene.scope == Scope::Send(s, d) && self.scene.taps(s).iter().any(|(t, _)| *t == tap) {
                    self.scene.sends[s][d].tap = tap;
                }
            }
            Msg::SendGain(s, d, value) => {
                if self.scene.scope == Scope::Send(s, d) {
                    self.scene.sends[s][d].gain_db = value.clamp(-48, 12);
                }
            }
            Msg::StackActions => self.stack_actions = !self.stack_actions,
            Msg::SaveStack => self.saved_stack = Some((self.scene.scope.title(), self.scene.stages().to_vec())),
            Msg::LoadStack => {
                if let Some((_, saved)) = &self.saved_stack {
                    self.scene.load_stack(saved);
                    self.selected_stage = None;
                    self.catalog = false;
                }
            }
            Msg::Reset => {
                let width = self.width;
                *self = Self::new(self.concept);
                self.width = width;
            }
            Msg::Size(size) if size.width.is_finite() && size.width > 0.0 => self.width = size.width,
            Msg::Size(_) => {}
            Msg::ShowPane(editor) => self.editor_pane = editor,
        }
    }

    fn header(&self, cx: &ViewContext<Self>) -> View {
        let title = StackPanel::new().spacing(1.0).children((
            TextBlock::new().text("Routing & effects").font_size(28.0).font_weight(FontWeight::SEMI_BOLD),
            muted("Design preview  ·  Simulated audio and meters"),
        ));
        let reset = Button::new().style(ButtonStyle::Subtle).on_click(cx.callback(|()| Msg::Reset)).content("Reset example");
        let title_row = Grid::new().columns([GridLength::STAR, GridLength::Auto]).children((
            Border::new().content(title),
            Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Center).content(reset),
        ));
        let navigation = row(Concept::ALL
            .into_iter()
            .map(|(choice, label)| action(label, cx.callback(move |()| Msg::Concept(choice)), self.concept == choice))
            .collect());
        column(vec![title_row, navigation, muted("For example, play into LLCable-C In and record from LLCable-A Out.").into()])
    }

    fn routing(&self, cx: &ViewContext<Self>) -> View {
        match self.concept {
            Concept::Paths => paths::paths(self, cx),
            Concept::Matrix => routes::matrix(self, cx),
            Concept::Mixer => strips::mixer(self, cx),
        }
    }

    fn send_controls(&self, cx: &ViewContext<Self>, s: usize, d: usize) -> View {
        let send = &self.scene.sends[s][d];
        let choices = self.scene.taps(s);
        let selected = choices.iter().position(|(tap, _)| *tap == send.tap);
        let summary = selected.map_or("After all source stages", |i| choices[i].1.as_str());
        let action_label = format!("{}   ·   {summary}   ·   {:+} dB", if self.send_options { "▾  Send" } else { "▸  Send" }, send.gain_db);
        let button = Button::new()
            .style(ButtonStyle::Subtle)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Left)
            .on_click(cx.callback(|()| Msg::SendOptions))
            .content(TextBlock::new().text(action_label).text_wrapping(TextWrapping::Wrap));
        if !self.send_options {
            return button;
        }
        let choice_ids: Vec<_> = choices.iter().map(|(tap, _)| *tap).collect();
        let sender = cx.sender();
        let picker = ComboBox::new()
            .items_source(choices.iter().map(|(_, title)| title.as_str()))
            .selected_index(selected)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .on_selection_changed(move |index: Option<usize>| {
                if let Some(&tap) = index.and_then(|i| choice_ids.get(i)) {
                    sender.send(Msg::SetTap(s, d, tap));
                }
            })
            .slot(ComboBoxSlot::Header, "Take audio from");
        let gain = Slider::new()
            .minimum(-48.0)
            .maximum(12.0)
            .value(f64::from(send.gain_db))
            .step_frequency(1.0)
            .on_value_changed(cx.callback(move |v: f64| Msg::SendGain(s, d, v.round() as i32)))
            .slot(SliderSlot::Header, format!("Gain into {}   ·   {:+} dB", DESTS[d], send.gain_db));
        let key = format!("{s}-{d}-{:?}", choices);
        card(column(vec![button, View::keyed_fragment([(key, picker)]), gain]))
    }

    fn inspector(&self, cx: &ViewContext<Self>) -> View {
        let mut items = vec![editor::editor(self, cx, &Kind::ALL)];
        if let Scope::Send(s, d) = self.scene.scope {
            items.push(self.send_controls(cx, s, d));
        }
        let copy = Button::new().style(ButtonStyle::Subtle).on_click(cx.callback(|()| Msg::StackActions)).content(if self.stack_actions {
            "▾  Reuse this pipeline"
        } else {
            "▸  Reuse this pipeline"
        });
        items.push(copy);
        if self.stack_actions {
            let mut buttons = vec![action("Copy stages", cx.callback(|()| Msg::SaveStack), false)];
            if let Some((from, _)) = &self.saved_stack {
                buttons.push(action("Replace with copy", cx.callback(|()| Msg::LoadStack), false));
                items.push(small(&format!("Copied from {from}")).into());
            }
            items.push(row(buttons));
        }
        column(items)
    }
}

impl Component for Preview {
    type Input = Concept;
    type Message = Msg;

    fn create(concept: &Concept, _: &ComponentContext<Self>) -> Self {
        Self::new(*concept)
    }

    fn update(&mut self, message: Msg, _: &ComponentContext<Self>) {
        self.receive(message);
    }

    fn view(&self, _: &Concept, cx: &mut ViewContext<Self>) -> View {
        cx.window_title("ACX-LL-Cable · Routing design preview");
        cx.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(WindowTheme::System)
                .client_size(960.0, 660.0)
                .constraints(WindowConstraints { min_width: Some(510.0), min_height: Some(400.0), ..Default::default() }),
        );
        cx.on_window_size(cx.callback(Msg::Size));
        let content = if self.width < 840.0 {
            let back = if self.editor_pane { "‹  Show routing" } else { "Edit selected pipeline  ›" };
            let target = !self.editor_pane;
            let switch = action(back, cx.callback(move |()| Msg::ShowPane(target)), false);
            column(vec![switch, if self.editor_pane { self.inspector(cx) } else { self.routing(cx) }])
        } else {
            Grid::new().columns([GridLength::Star(1.3), GridLength::STAR]).column_spacing(22.0).children((
                Border::new().vertical_alignment(VerticalAlignment::Top).content(self.routing(cx)),
                Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Top).content(self.inspector(cx)),
            ))
        };
        let layout =
            StackPanel::new().margin(Thickness::xy(22.0, 16.0)).max_width(1150.0).spacing(18.0).children((self.header(cx), content));
        ScrollViewer::new().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(layout)
    }
}

fn meter(level: f64) -> View {
    let strip = |level: f64| {
        let cells = (0..14).map(|n| {
            let colour = if f64::from(n) + 0.5 < level * 14.0 {
                if n > 10 { Color::rgb(227, 184, 81) } else { Color::rgb(59, 177, 120) }
            } else {
                Color::argb(65, 128, 128, 128)
            };
            (n as usize, Border::new().grid_column(n).height(5.0).corner_radius(1.0).background(colour))
        });
        Grid::new().columns([GridLength::STAR; 14]).column_spacing(1.0).keyed_children(cells)
    };
    StackPanel::new().spacing(2.0).children((strip(level), strip((level - 0.10).max(0.0))))
}

fn main() {
    let concept = match std::env::args().nth(1).as_deref() {
        Some("matrix") => Concept::Matrix,
        Some("mixer") => Concept::Mixer,
        _ => Concept::Paths,
    };
    if let Err(error) = App::run_component::<Preview>(concept) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
