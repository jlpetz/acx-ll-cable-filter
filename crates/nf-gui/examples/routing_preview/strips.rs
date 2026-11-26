use super::*;

/// Each strip is an output mix. A lit source button feeds this output, independently of every other strip.
pub(super) fn mixer(app: &Preview, cx: &ViewContext<Preview>) -> View {
    let sends_to = match app.scene.scope {
        Scope::Output(d) | Scope::Send(_, d) => Some(d),
        Scope::Source(_) => None,
    };
    let strips = Grid::new().columns([GridLength::STAR; 3]).column_spacing(8.0).keyed_children(
        DESTS
            .iter()
            .enumerate()
            .map(|(d, _)| (d, Border::new().grid_column(d as i32).content(output_strip(app, cx, d, sends_to == Some(d))))),
    );
    let mut body = vec![heading("Destination mixes").into(), muted("Choose the sources to mix at each destination.").into(), strips];
    if let Some(d) = sends_to {
        let sends: Vec<View> = SOURCES
            .iter()
            .enumerate()
            .filter(|(s, _)| app.scene.connected(*s, d))
            .map(|(s, name)| action(name, cx.callback(move |()| Msg::Select(Scope::Send(s, d))), app.scene.scope == Scope::Send(s, d)))
            .collect();
        body.push(card(column(vec![
            heading(&format!("Send effects to {}", DESTS[d])).into(),
            if sends.is_empty() { muted("Connect a source to this destination to edit its send effects.").into() } else { row(sends) },
        ])));
    }
    let sources = SOURCES
        .iter()
        .enumerate()
        .map(|(s, name)| action(name, cx.callback(move |()| Msg::Select(Scope::Source(s))), app.scene.scope == Scope::Source(s)))
        .collect();
    body.push(card(column(vec![heading("Shared source effects").into(), row(sources)])));
    column(body)
}

fn output_strip(app: &Preview, cx: &ViewContext<Preview>, d: usize, selected: bool) -> View {
    let inputs = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d)).count();
    let signal = if inputs == 0 { 0.0 } else { [0.78, 0.69, 0.63][d] };
    let peak = if inputs == 0 { "−∞" } else { ["−13.2", "−18.6", "−22.2"][d] };
    let count = match inputs {
        0 => "No sources".to_owned(),
        1 => "1 source".to_owned(),
        n => format!("{n} sources"),
    };
    let mut body = vec![
        TextBlock::new()
            .text(DESTS[d])
            .font_size(16.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .text_wrapping(TextWrapping::Wrap)
            .tooltip(DEST_INFO[d]),
        column(vec![
            meter(signal),
            TextBlock::new().text(format!("{peak} dBFS")).font_size(16.0).automation_name(format!("{} peak, {peak} dBFS", DESTS[d])).into(),
        ]),
        small(&count).into(),
    ];
    for (s, name) in SOURCES.iter().enumerate() {
        let connected = app.scene.connected(s, d);
        let sender = cx.sender();
        body.push(
            ToggleButton::new()
                .is_checked(connected)
                .on_is_checked_changed(move |checked| {
                    if checked != connected {
                        sender.send(Msg::Send(s, d));
                    }
                })
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .automation_name(format!("Send {name} to {}", DESTS[d]))
                .content(TextBlock::new().text(*name).font_size(12.0).text_wrapping(TextWrapping::Wrap))
                .tooltip(format!("{} → {}", SOURCE_INFO[s], DEST_INFO[d])),
        );
    }
    let output = Scope::Output(d);
    body.push(action("Edit mix", cx.callback(move |()| Msg::Select(output)), app.scene.scope == output));
    Border::new()
        .padding(Thickness::xy(10.0, 12.0))
        .corner_radius(8.0)
        .background(ThemeBrush::CardBackground)
        .border_thickness(1.0)
        .border_brush(if selected { ThemeBrush::Accent } else { ThemeBrush::CardStroke })
        .content(column(body))
}
