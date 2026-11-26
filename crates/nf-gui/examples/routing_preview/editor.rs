use super::*;

pub(super) fn editor(app: &Preview, cx: &ViewContext<Preview>) -> View {
    let scope = app.scene.scope;
    let stages = app.scene.stages();
    let header = Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(8.0).children((
        heading("Pipeline").vertical_alignment(VerticalAlignment::Center).grid_column(0),
        Border::new().grid_column(1).content(action(
            if app.catalog { "Close catalog" } else { "+ Add stage" },
            cx.callback(|()| Msg::Catalog),
            !app.catalog,
        )),
    ));
    let scope_label = StackPanel::new().spacing(2.0).children((
        TextBlock::new().text(scope.title()).font_size(16.0).font_weight(FontWeight::SEMI_BOLD).text_wrapping(TextWrapping::Wrap),
        muted(scope.description()),
    ));
    let stack = if stages.is_empty() {
        Border::new().padding(Thickness::new(0.0, 10.0, 0.0, 10.0)).content(muted("Dry signal · no stages"))
    } else {
        StackPanel::new().spacing(6.0).keyed_children(stages.iter().enumerate().map(|(index, stage)| {
            let selected = app.selected_stage == Some(stage.id);
            (stage.id, stage_row(stage, index, stages.len(), selected, cx))
        }))
    };
    card(column(vec![header, scope_label, if app.catalog { catalog(cx) } else { View::empty() }, stack]))
}

fn catalog(cx: &ViewContext<Preview>) -> View {
    let tiles = Kind::ALL.into_iter().enumerate().map(|(i, kind)| {
        let body = StackPanel::new().spacing(1.0).children((
            TextBlock::new().text(kind.name()).font_weight(FontWeight::SEMI_BOLD).font_size(13.0).text_wrapping(TextWrapping::Wrap),
            small(kind.family()),
        ));
        let pick = Button::new()
            .automation_name(format!("Add {}", kind.name()))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Left)
            .grid_row((i / 2) as i32)
            .grid_column((i % 2) as i32)
            .on_click(cx.callback(move |()| Msg::Add(kind)))
            .content(body);
        (i, pick)
    });
    Grid::new()
        .columns([GridLength::STAR; 2])
        .rows([GridLength::Auto; Kind::ALL.len().div_ceil(2)])
        .column_spacing(4.0)
        .row_spacing(4.0)
        .keyed_children(tiles)
}

fn stage_row(stage: &Stage, index: usize, count: usize, selected: bool, cx: &ViewContext<Preview>) -> View {
    let id = stage.id;
    let kind = stage.kind;
    let family = if stage.enabled { kind.family().to_owned() } else { format!("{} · Bypassed", kind.family()) };
    let title = StackPanel::new().grid_column(1).spacing(1.0).children((
        TextBlock::new().text(kind.name()).font_weight(FontWeight::SEMI_BOLD).font_size(14.0).text_wrapping(TextWrapping::Wrap),
        small(&family),
    ));
    let head = Grid::new().columns([GridLength::Auto, GridLength::STAR, GridLength::Auto]).column_spacing(10.0).children((
        small(&(index + 1).to_string()).vertical_alignment(VerticalAlignment::Center),
        title,
        small(if selected { "▴" } else { "▾" }).vertical_alignment(VerticalAlignment::Center).grid_column(2),
    ));
    let button = Button::new()
        .style(ButtonStyle::Subtle)
        .automation_name(format!("Edit stage {}, {}", index + 1, kind.name()))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .on_click(cx.callback(move |()| Msg::Edit(id)))
        .content(head);
    let mut parts = vec![button];
    if selected {
        let parameter = if matches!(kind, Kind::Echo) {
            let sender = cx.sender();
            let choices = ["Speakers (system playback)", "Headset playback", "Cable C playback"];
            ComboBox::new()
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .items_source(choices)
                .selected_index(Some(stage.value.clamp(0, 2) as usize))
                .on_selection_changed(Callback::new(move |index: Option<usize>| {
                    if let Some(index) = index.filter(|i| *i < choices.len()) {
                        sender.send(Msg::SetValue(id, index as i32));
                    }
                }))
                .slot(ComboBoxSlot::Header, "Echo reference (side input)")
        } else {
            let (low, high) = kind.range();
            Slider::new()
                .minimum(f64::from(low))
                .maximum(f64::from(high))
                .step_frequency(1.0)
                .value(f64::from(stage.value))
                .on_value_changed(cx.callback(move |v: f64| Msg::SetValue(id, v.round() as i32)))
                .slot(SliderSlot::Header, format!("{}  ·  {}", kind.param(), kind.format(stage.value)))
        };
        let move_button = |label: &'static str, hint: &'static str, step: i8, enabled| {
            Button::new()
                .width(32.0)
                .is_enabled(enabled)
                .automation_name(hint)
                .on_click(cx.callback(move |()| Msg::Move(id, step)))
                .content(label)
                .tooltip(hint)
        };
        let commands = StackPanel::new().orientation(Orientation::Horizontal).spacing(4.0).children((
            move_button("↑", "Move stage earlier", -1, index > 0),
            move_button("↓", "Move stage later", 1, index + 1 < count),
            action(if stage.enabled { "Bypass" } else { "Enable" }, cx.callback(move |()| Msg::Bypass(id)), false),
            action("Remove", cx.callback(move |()| Msg::Remove(id)), false),
        ));
        parts.push(Border::new().padding(Thickness::new(6.0, 2.0, 6.0, 6.0)).content(column(vec![parameter, commands])));
    }
    Border::new()
        .border_brush(if selected { ThemeBrush::Accent } else { ThemeBrush::CardStroke })
        .border_thickness(1.0)
        .corner_radius(6.0)
        .content(column(parts))
}
