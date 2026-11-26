use super::*;

/// A source owns its shared chain. The indented lines are its individually processed sends into destination mixes.
pub(super) fn paths(app: &Preview, cx: &ViewContext<Preview>) -> View {
    let sources = StackPanel::new().spacing(8.0).keyed_children((0..SOURCES.len()).map(|s| (s, source_card(app, cx, s))));
    let mixes = Grid::new().columns([GridLength::STAR; 3]).column_spacing(6.0).keyed_children(DESTS.iter().enumerate().map(|(d, dest)| {
        let inputs = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d)).count();
        let count = if inputs == 1 { "1 source".to_owned() } else { format!("{inputs} sources") };
        let scope = Scope::Output(d);
        let button = Button::new()
            .style(if app.scene.scope == scope { ButtonStyle::Accent } else { ButtonStyle::Default })
            .grid_column(d as i32)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .automation_name(format!("Edit {dest} mix with {count}"))
            .on_click(cx.callback(move |()| Msg::Select(scope)))
            .content(StackPanel::new().spacing(1.0).children((
                TextBlock::new().text(*dest).text_wrapping(TextWrapping::Wrap).font_weight(FontWeight::SEMI_BOLD),
                small(&count),
            )))
            .tooltip(DEST_INFO[d]);
        (d, button)
    }));
    column(vec![
        heading("Sources and sends").into(),
        muted("Select a source for shared processing, or a branch for effects on one send.").into(),
        sources,
        heading("Destination mixes").into(),
        mixes,
    ])
}

fn source_card(app: &Preview, cx: &ViewContext<Preview>, s: usize) -> View {
    let source = Scope::Source(s);
    let editing = app.sends_open == Some(s);
    let shared = app.scene.at(source);
    let subhead = if shared.is_empty() {
        SOURCE_INFO[s].to_owned()
    } else {
        format!("Shared · {}", shared.iter().map(|stage| stage.kind.name()).collect::<Vec<_>>().join(" → "))
    };
    let head = Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(8.0).children((
        Button::new()
            .style(if app.scene.scope == source { ButtonStyle::Default } else { ButtonStyle::Subtle })
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Left)
            .automation_name(format!("Edit shared effects for {}", SOURCES[s]))
            .on_click(cx.callback(move |()| Msg::Select(source)))
            .content(StackPanel::new().spacing(1.0).children((
                TextBlock::new().text(SOURCES[s]).font_weight(FontWeight::SEMI_BOLD).font_size(15.0),
                small(&subhead).text_trimming(TextTrimming::CharacterEllipsis),
            )))
            .tooltip(format!("{}. Select to edit shared effects.", SOURCE_INFO[s])),
        Button::new()
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name(format!("{} sends for {}", if editing { "Close" } else { "Add or edit" }, SOURCES[s]))
            .on_click(cx.callback(move |()| Msg::Sends(s)))
            .content(if editing { "Done" } else { "Edit sends" }),
    ));
    let destinations: Vec<_> = (0..DESTS.len()).filter(|&d| app.scene.connected(s, d)).collect();
    let branches = if destinations.is_empty() {
        small("No connected sends").margin(Thickness::new(14.0, 0.0, 0.0, 0.0)).into()
    } else {
        let last = destinations.last().copied();
        StackPanel::new().keyed_children(destinations.into_iter().map(|d| (d, send_branch(app, cx, s, d, Some(d) == last))))
    };
    let mut contents = vec![head, branches];
    if editing {
        let controls =
            Grid::new().columns([GridLength::STAR; 3]).column_spacing(4.0).keyed_children(DESTS.iter().enumerate().map(|(d, dest)| {
                let on = app.scene.connected(s, d);
                let label = if on { format!("Disconnect {} from {dest}", SOURCES[s]) } else { format!("Send {} to {dest}", SOURCES[s]) };
                let button = Button::new()
                    .grid_column(d as i32)
                    .style(if on { ButtonStyle::Accent } else { ButtonStyle::Default })
                    .horizontal_alignment(HorizontalAlignment::Stretch)
                    .automation_name(&label)
                    .on_click(cx.callback(move |()| Msg::Send(s, d)))
                    .content(
                        TextBlock::new()
                            .text(format!("{} {dest}", if on { "✓" } else { "+" }))
                            .font_size(13.0)
                            .text_wrapping(TextWrapping::Wrap),
                    )
                    .tooltip(label);
                (d, button)
            }));
        contents.push(controls);
    }
    card(StackPanel::new().spacing(4.0).keyed_children(contents.into_iter().enumerate()))
}

fn send_branch(app: &Preview, cx: &ViewContext<Preview>, s: usize, d: usize, last: bool) -> View {
    let scope = Scope::Send(s, d);
    let selected = app.scene.scope == scope;
    let stages = app.scene.at(scope);
    let local_fx = match stages {
        [] => "No send FX".to_owned(),
        [stage] if !stage.enabled => format!("{} · bypassed", stage.kind.name()),
        [stage] => stage.kind.name().to_owned(),
        [first, rest @ ..] => format!("{} +{}", first.kind.name(), rest.len()),
    };
    let send = &app.scene.sends[s][d];
    let summary = if send.tap == Tap::Raw { format!("Dry tap · {local_fx}") } else { local_fx };
    // Both strokes meet at the centre of the row. The final branch stops there.
    let connector = Grid::new().rows([GridLength::STAR; 2]).children((
        Border::new()
            .width(1.0)
            .grid_row_span(if last { 1 } else { 2 })
            .horizontal_alignment(HorizontalAlignment::Left)
            .background(ThemeBrush::CardStroke),
        Border::new().grid_row(1).height(1.0).vertical_alignment(VerticalAlignment::Top).background(ThemeBrush::CardStroke),
    ));
    let label = Grid::new().columns([GridLength::Star(1.2), GridLength::STAR]).column_spacing(6.0).children((
        StackPanel::new().spacing(0.0).children((
            TextBlock::new().text(DESTS[d]).font_size(14.0).font_weight(FontWeight::SEMI_BOLD),
            small(DEST_INFO[d]).text_trimming(TextTrimming::CharacterEllipsis),
        )),
        small(&summary)
            .grid_column(1)
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Center)
            .text_trimming(TextTrimming::CharacterEllipsis),
    ));
    Grid::new().margin(Thickness::new(16.0, 0.0, 0.0, 0.0)).columns([GridLength::Pixel(14.0), GridLength::STAR]).children((
        connector,
        Button::new()
            .grid_column(1)
            .style(if selected { ButtonStyle::Default } else { ButtonStyle::Subtle })
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .automation_name(format!("Edit send from {} to {}", SOURCES[s], DESTS[d]))
            .on_click(cx.callback(move |()| Msg::Select(scope)))
            .content(label)
            .tooltip(format!("{} → {}. {summary}", SOURCES[s], DESTS[d])),
    ))
}
