use super::*;

pub(super) fn matrix(app: &Preview, cx: &ViewContext<Preview>) -> View {
    let mut cells = vec![small("Source ↓\nDestination →").vertical_alignment(VerticalAlignment::Center).into()];
    for (d, dest) in DESTS.iter().enumerate() {
        let label = format!("Edit the {dest} mix. {}", DEST_INFO[d]);
        cells.push(
            Button::new()
                .style(if app.scene.scope == Scope::Output(d) { ButtonStyle::Default } else { ButtonStyle::Subtle })
                .on_click(cx.callback(move |()| Msg::Select(Scope::Output(d))))
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .automation_name(&label)
                .grid_column(d as i32 + 1)
                .content(TextBlock::new().text(*dest).font_size(12.0).text_wrapping(TextWrapping::Wrap))
                .tooltip(label),
        );
    }

    for (s, source) in SOURCES.iter().enumerate() {
        let r = s as i32 + 1;
        let label = format!("Edit the shared {source} chain. {}", SOURCE_INFO[s]);
        cells.push(
            Button::new()
                .style(if app.scene.scope == Scope::Source(s) { ButtonStyle::Default } else { ButtonStyle::Subtle })
                .on_click(cx.callback(move |()| Msg::Select(Scope::Source(s))))
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .horizontal_content_alignment(HorizontalAlignment::Left)
                .automation_name(&label)
                .grid_row(r)
                .content(TextBlock::new().text(*source).text_wrapping(TextWrapping::Wrap).font_size(13.0))
                .tooltip(label),
        );
        for (d, dest) in DESTS.iter().enumerate() {
            let on = app.scene.connected(s, d);
            let label = if on { format!("Disconnect {source} from {dest}") } else { format!("Send {source} to {dest}") };
            cells.push(
                Button::new()
                    .style(if on { ButtonStyle::Accent } else { ButtonStyle::Default })
                    .horizontal_alignment(HorizontalAlignment::Stretch)
                    .height(38.0)
                    .grid_row(r)
                    .grid_column(d as i32 + 1)
                    .automation_name(&label)
                    .on_click(cx.callback(move |()| Msg::Send(s, d)))
                    .content(if on { "●  On" } else { "Off" })
                    .tooltip(label),
            );
        }
    }

    let footer_row = SOURCES.len() as i32 + 1;
    cells.push(small("In the mix").vertical_alignment(VerticalAlignment::Center).grid_row(footer_row).into());
    for (d, _) in DESTS.iter().enumerate() {
        let n = (0..SOURCES.len()).filter(|&s| app.scene.connected(s, d)).count();
        let count = if n == 1 { "1 source".to_owned() } else { format!("{n} sources") };
        cells.push(small(&count).horizontal_alignment(HorizontalAlignment::Center).grid_column(d as i32 + 1).grid_row(footer_row).into());
    }
    let table = Grid::new()
        .columns([GridLength::Star(1.45), GridLength::STAR, GridLength::STAR, GridLength::STAR])
        .rows([GridLength::Auto; SOURCES.len() + 2])
        .column_spacing(5.0)
        .row_spacing(9.0)
        .keyed_children(cells.into_iter().enumerate());

    let mut branches = Vec::new();
    for (s, source) in SOURCES.iter().enumerate() {
        for (d, dest) in DESTS.iter().enumerate() {
            if !app.scene.connected(s, d) {
                continue;
            }
            let selected = app.scene.scope == Scope::Send(s, d);
            let name = format!("{source}  →  {dest}");
            let label = format!("Edit processing for the {source} to {dest} send");
            let mark = if selected { "Editing" } else { "Edit  ›" };
            let content = Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(8.0)
                .children((TextBlock::new().text(name).font_size(14.0), small(mark).grid_column(1)));
            branches.push((
                (s * DESTS.len() + d) as u64,
                Button::new()
                    .style(if selected { ButtonStyle::Default } else { ButtonStyle::Subtle })
                    .horizontal_alignment(HorizontalAlignment::Stretch)
                    .horizontal_content_alignment(HorizontalAlignment::Stretch)
                    .automation_name(&label)
                    .on_click(cx.callback(move |()| Msg::Select(Scope::Send(s, d))))
                    .content(content)
                    .tooltip(label),
            ));
        }
    }
    let sends = if branches.is_empty() {
        muted("Turn on a send to edit its effects.").into()
    } else {
        StackPanel::new().spacing(3.0).keyed_children(branches)
    };
    column(vec![
        heading("Routing table").into(),
        muted("Switch on a send to connect a source. Sources in the same destination column are mixed.").into(),
        card(table),
        card(column(vec![heading("Send effects").into(), sends])),
    ])
}
