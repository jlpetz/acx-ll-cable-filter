use windows_reactor::*;

pub(super) fn heading(label: &str) -> TextBlock {
    TextBlock::new().text(label).font_weight(FontWeight::SEMI_BOLD).font_size(18.0)
}

pub(super) fn muted(label: &str) -> TextBlock {
    TextBlock::new().text(label).font_size(13.0).opacity(0.76).text_wrapping(TextWrapping::Wrap)
}

pub(super) fn small(label: &str) -> TextBlock {
    TextBlock::new().text(label).font_size(12.0).opacity(0.77)
}

pub(super) fn row(items: Vec<View>) -> View {
    StackPanel::new().orientation(Orientation::Horizontal).spacing(6.0).keyed_children(items.into_iter().enumerate())
}

pub(super) fn column(items: Vec<View>) -> View {
    StackPanel::new().spacing(8.0).keyed_children(items.into_iter().enumerate())
}

pub(super) fn card(body: View) -> View {
    Border::new()
        .padding(13.0)
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(1.0)
        .corner_radius(8.0)
        .content(body)
}

pub(super) fn action(label: &str, callback: Callback<()>, accent: bool) -> View {
    Button::new().style(if accent { ButtonStyle::Accent } else { ButtonStyle::Default }).on_click(callback).content(label)
}
