//! Select a destination to mix its sources, or switch to the matrix to patch several sends at once.
use windows_reactor::*;

use super::{DEST_INFO, DESTS, Mixer, Msg, SOURCE_INFO, SOURCES, Scope, Tap, meters};

const ALIASES: [&str; 3] = ["Recording · OBS", "Voice · Discord", "Local monitor"];
const PORT_TYPES: [&str; 3] = ["Physical microphone", "Virtual playback input", "Virtual playback input"];

fn title(label: &str, size: f64) -> TextBlock {
    TextBlock::new().text(label).font_size(size).font_weight(FontWeight::SEMI_BOLD).text_wrapping(TextWrapping::Wrap)
}

fn caption(label: impl Into<String>) -> TextBlock {
    TextBlock::new().text(label).font_size(12.0).opacity(0.80).text_wrapping(TextWrapping::Wrap)
}

fn destination(app: &Mixer, cx: &ViewContext<Mixer>, d: usize) -> View {
    let selected = app.focus_open && !app.all_sends && app.output == d;
    let meter = meters::strip(DESTS[d], &app.output_levels(d), app.prefs.meter_mode(), app.prefs.palette);
    let name = if app.muted[d] { format!("{} · Muted", ALIASES[d]) } else { ALIASES[d].into() };
    let inner =
        StackPanel::new().spacing(if app.prefs.compact { 2.0 } else { 4.0 }).children((title(DESTS[d], 16.0), caption(name), meter));
    let label = format!("Select {} destination mix for {}{}", DESTS[d], ALIASES[d], if selected { ", selected" } else { "" });
    let show_actions = app.output_actions == Some(d);
    // The button can consume a routed pointer press. Observe the secondary button in its hit-testable content first.
    let secondary = cx.sender();
    let hit = Border::new()
        .background(Color::transparent())
        .on_pointer_pressed(move |event: PointerEventInfo| {
            if event.is_right_button_pressed {
                secondary.send(Msg::OutputActions(Some(d)));
            }
        })
        .content(inner);
    let select = Button::new()
        .style(ButtonStyle::Subtle)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .automation_name(label)
        .on_click(cx.callback(move |()| Msg::PickOutput(d)))
        .content(hit)
        .tooltip(DEST_INFO[d]);
    let more = Button::new()
        .grid_column(1)
        .style(ButtonStyle::Subtle)
        .vertical_alignment(VerticalAlignment::Top)
        .automation_name(format!("More actions for {}", DESTS[d]))
        .on_click(cx.callback(move |()| Msg::OutputActions((!show_actions).then_some(d))))
        .content("…")
        .tooltip("Diagnostics and output effects");
    let face = Grid::new().columns([GridLength::STAR, GridLength::Auto]).children((select, more));
    let actions = if show_actions {
        StackPanel::new().orientation(Orientation::Horizontal).spacing(4.0).margin(Thickness::xy(8.0, 4.0)).children((
            Button::new()
                .automation_name(format!("Diagnostics for {}", DESTS[d]))
                .on_click(cx.callback(move |()| Msg::InspectOutput(d)))
                .content("Diagnostics"),
            Button::new()
                .automation_name(format!("Edit effects after mixing {}", DESTS[d]))
                .on_click(cx.callback(move |()| Msg::Edit(Scope::Output(d))))
                .content("Output effects"),
        ))
    } else {
        View::empty()
    };
    Border::new()
        .corner_radius(6.0)
        .padding(if app.prefs.compact { 3.0 } else { 6.0 })
        .border_thickness(if selected { 2.0 } else { 1.0 })
        .border_brush(if selected { ThemeBrush::Accent } else { ThemeBrush::CardStroke })
        .background(ThemeBrush::CardBackground)
        .content(StackPanel::new().children((face, actions)))
}

fn source_identity(app: &Mixer, cx: &ViewContext<Mixer>, s: usize) -> View {
    let meter = meters::strip(SOURCES[s], &app.input_levels(s), app.prefs.meter_mode(), app.prefs.palette);
    let content = StackPanel::new().spacing(2.0).children((title(SOURCES[s], 15.0), caption(PORT_TYPES[s]), meter));
    let label = format!("Edit shared effects for {}", SOURCES[s]);
    Button::new()
        .style(ButtonStyle::Subtle)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .automation_name(&label)
        .on_click(cx.callback(move |()| Msg::Edit(Scope::Source(s))))
        .content(content)
        .tooltip(SOURCE_INFO[s])
}

fn source_send(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, d: usize) -> View {
    let send = app.scene.sends[s][d];
    let label = if send.connected {
        format!("Disconnect {} from {}", SOURCES[s], DESTS[d])
    } else {
        format!("Send {} to {}", SOURCES[s], DESTS[d])
    };
    let sender = cx.sender();
    ToggleButton::new()
        .min_width(54.0)
        .is_checked(send.connected)
        .automation_name(format!("{} to {} send", SOURCES[s], DESTS[d]))
        .on_is_checked_changed(move |state| {
            if state != send.connected {
                sender.send(Msg::ToggleSend(s, d));
            }
        })
        .content(if send.connected { "On" } else { "Off" })
        .tooltip(label)
}

fn tap_controls(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, d: usize) -> View {
    let send = app.scene.sends[s][d];
    let shared = app.scene.at(Scope::Source(s));
    if shared.is_empty() {
        return caption("Original signal").into();
    }
    let make_choice = |tap: Tap, name: &str| {
        let selected = send.tap == tap;
        let label = format!("Take {} for {} from {name}", SOURCES[s], DESTS[d]);
        Button::new()
            .style(if selected { ButtonStyle::Accent } else { ButtonStyle::Default })
            .automation_name(format!("{label}{}", if selected { ", selected" } else { "" }))
            .on_click(cx.callback(move |()| Msg::Tap(s, d, tap)))
            .content(TextBlock::new().text(name).font_size(12.0))
            .tooltip(label)
    };
    let buttons = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(4.0)
        .children((make_choice(Tap::Raw, "Original"), make_choice(Tap::End, "After FX")));
    if matches!(send.tap, Tap::Stage(_)) {
        StackPanel::new().spacing(2.0).children((buttons, caption(app.tap_name(s, d))))
    } else {
        buttons
    }
}

fn send_edit(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, d: usize) -> View {
    let send = app.scene.sends[s][d];
    let effects = app.scene.at(Scope::Send(s, d)).iter().filter(|stage| stage.enabled).count();
    let name = if effects == 0 { format!("{:+} dB  ›", send.gain_db) } else { format!("{:+} dB · {effects} FX  ›", send.gain_db) };
    Button::new()
        .style(ButtonStyle::Subtle)
        .automation_name(format!("Edit {} send level and effects for {}", SOURCES[s], DESTS[d]))
        .on_click(cx.callback(move |()| Msg::Edit(Scope::Send(s, d))))
        .content(name)
        .tooltip("Adjust send gain, tap and effects")
}

fn source_row(app: &Mixer, cx: &ViewContext<Mixer>, s: usize, d: usize) -> View {
    let row = Grid::new().columns([GridLength::STAR, GridLength::Auto]).rows([GridLength::Auto; 2]).column_spacing(6.0).children((
        source_identity(app, cx, s),
        Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Center).content(source_send(app, cx, s, d)),
        Border::new()
            .grid_row(1)
            .margin(Thickness::xy(10.0, 0.0))
            .vertical_alignment(VerticalAlignment::Center)
            .content(tap_controls(app, cx, s, d)),
        Border::new()
            .grid_row(1)
            .grid_column(1)
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Center)
            .content(send_edit(app, cx, s, d)),
    ));
    Border::new()
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(1.0)
        .corner_radius(6.0)
        .padding(if app.prefs.compact { 3.0 } else { 6.0 })
        .content(row)
}

fn matrix(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let tight = app.prefs.compact || app.width < 800.0;
    let mut cells = vec![caption("Sources ↓\nOutputs →").into()];
    for (d, &output) in DESTS.iter().enumerate() {
        cells.push(
            title(output, if tight { 13.0 } else { 15.0 })
                .grid_column(d as i32 + 1)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .automation_name(format!("Output column, {output}"))
                .into(),
        );
    }
    for (s, &source) in SOURCES.iter().enumerate() {
        let row = s as i32 + 1;
        cells.push(
            title(source, if tight { 13.0 } else { 15.0 })
                .grid_row(row)
                .vertical_alignment(VerticalAlignment::Center)
                .automation_name(format!("Source row, {source}"))
                .tooltip(SOURCE_INFO[s]),
        );
        for d in 0..DESTS.len() {
            cells.push(
                Border::new()
                    .grid_row(row)
                    .grid_column(d as i32 + 1)
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .content(source_send(app, cx, s, d)),
            );
        }
    }
    Grid::new()
        .columns([GridLength::Star(1.25), GridLength::STAR, GridLength::STAR, GridLength::STAR])
        .rows([GridLength::Auto; SOURCES.len() + 1])
        .column_spacing(if tight { 4.0 } else { 9.0 })
        .row_spacing(if tight { 4.0 } else { 9.0 })
        .keyed_children(cells.into_iter().enumerate())
}

fn focused(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let d = app.output;
    let sender = cx.sender();
    let muted = app.muted[d];
    let mute = ToggleButton::new()
        .is_checked(muted)
        .automation_name(format!("Mute {}", DESTS[d]))
        .on_is_checked_changed(move |state| {
            if state != muted {
                sender.send(Msg::Mute(d));
            }
        })
        .content(if muted { "Unmute" } else { "Mute" });
    let prompt = Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(7.0).children((
        title(&format!("What should {} hear?", DESTS[d]), 18.0).vertical_alignment(VerticalAlignment::Center),
        Border::new().grid_column(1).content(mute),
    ));
    let connected = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d));
    let rows = StackPanel::new()
        .spacing(if app.prefs.compact { 3.0 } else { 8.0 })
        .keyed_children(connected.map(|s| (s, source_row(app, cx, s, d))));
    let disconnected: Vec<_> = (0..SOURCES.len()).filter(|&s| !app.scene.connected(s, d)).collect();
    let add = if disconnected.is_empty() {
        View::empty()
    } else {
        // Lined-up rows keep every source name intact at narrow widths and connect a send in one click.
        let choices = StackPanel::new().spacing(2.0).keyed_children(disconnected.into_iter().map(|s| {
            let add = Button::new()
                .style(ButtonStyle::Subtle)
                .horizontal_alignment(HorizontalAlignment::Left)
                .automation_name(format!("Connect {} to {}", SOURCES[s], DESTS[d]))
                .on_click(cx.callback(move |()| Msg::ConnectSend(s, d)))
                .content(format!("+  {}", SOURCES[s]));
            (s, add)
        }));
        StackPanel::new().spacing(3.0).children((caption("Add a source"), choices))
    };
    StackPanel::new().spacing(if app.prefs.compact { 6.0 } else { 10.0 }).children((prompt, rows, add))
}

pub(super) fn view(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let all_sends = app.all_sends;
    let sender = cx.sender();
    let switch = ToggleButton::new()
        .grid_column(1)
        .is_checked(all_sends)
        .automation_name("Show all sends")
        .on_is_checked_changed(move |on| {
            if all_sends != on {
                sender.send(Msg::AllSends(on));
            }
        })
        .content("All sends")
        .tooltip("Show every source-to-output send in one matrix");
    let header = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children((title("Destination mixes", 20.0).vertical_alignment(VerticalAlignment::Center), switch));
    let outputs = StackPanel::new()
        .spacing(if app.prefs.compact { 5.0 } else { 9.0 })
        .keyed_children((0..DESTS.len()).map(|d| (d, destination(app, cx, d))));
    let upper = StackPanel::new().spacing(if app.prefs.compact { 7.0 } else { 12.0 }).children((header, outputs));
    if !all_sends && !app.focus_open {
        return upper;
    }
    let detail =
        if all_sends { StackPanel::new().spacing(10.0).children((title("All sends", 18.0), matrix(app, cx))) } else { focused(app, cx) };
    if app.width < 750.0 {
        return StackPanel::new().spacing(16.0).children((upper, detail));
    }
    Grid::new()
        .columns([GridLength::Star(0.9), GridLength::Star(1.2)])
        .column_spacing(18.0)
        .children((upper, Border::new().grid_column(1).content(detail)))
}
