//! View pieces shared by the window and the live section: cards, secondary text, sliders and
//! ComboBoxes.
//!
//! Every value that reaches a builder is clamped first: Reactor's builders `assert!` on NaN,
//! infinite or negative sizes, and a failed native set aborts the process (windows-reactor.md
//! pitfall 2).

use windows_reactor::*;

use crate::devices::DeviceChoices;
use crate::settings::{DEFAULT_WIDTH, Span};

/// The page's left and right margin.
pub(crate) const PAGE_MARGIN: f64 = 24.0;
/// Between sections.
pub(crate) const SECTION_SPACING: f64 = 16.0;
/// A card's inner padding, left and right.
pub(crate) const CARD_PADDING: f64 = 16.0;
/// The content column's widest: a wide window doesn't stretch the controls past reading width.
pub(crate) const MAX_CONTENT: f64 = 760.0;
/// Room kept for the ScrollViewer's bar when working out the meters' width.
const SCROLLBAR: f64 = 16.0;
/// The meter's narrowest track.
pub(crate) const MIN_TRACK: f64 = 120.0;
/// Secondary text: WinUI's theme brushes in Reactor 0.100.0 have no "secondary text", so it's the
/// primary colour dimmed.
const SECONDARY_OPACITY: f64 = 0.72;

/// The meters' track width for a window `window_width` DIPs wide: the card's inner width, in whole
/// DIPs (so a resize by a fraction doesn't re-render the meters). Always finite and at least
/// [`MIN_TRACK`].
pub(crate) fn track_width(window_width: f64) -> f64 {
    let window = if window_width.is_finite() { window_width } else { DEFAULT_WIDTH };
    let content = (window - 2.0 * PAGE_MARGIN - SCROLLBAR).min(MAX_CONTENT);
    // The card's padding and its 1 px border on each side
    (content - 2.0 * CARD_PADDING - 2.0).floor().max(MIN_TRACK)
}

/// A section's title.
pub(crate) fn section_title(title: &str) -> TextBlock {
    TextBlock::new().text(title).font_size(16.0).font_weight(FontWeight::SEMI_BOLD)
}

/// Dimmed, wrapped text for hints and notes.
pub(crate) fn secondary(text: impl Into<String>) -> TextBlock {
    TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).opacity(SECONDARY_OPACITY)
}

/// Wrapped text the user can select and copy (Reactor 0.100.0 has no clipboard API).
pub(crate) fn selectable(text: impl Into<String>) -> TextBlock {
    TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).is_text_selection_enabled(true)
}

/// A WinUI-style card: the card fill and stroke, rounded, with a title row. `extra` sits at the
/// title row's right (a section's ToggleSwitch).
pub(crate) fn card(title: &str, extra: Option<View>, body: impl Into<View>) -> View {
    let title_row: View = match extra {
        Some(extra) => Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(12.0).children((
            section_title(title).vertical_alignment(VerticalAlignment::Center).grid_column(0),
            Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Center).content(extra),
        )),
        None => section_title(title).into(),
    };
    Border::new()
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(1.0)
        .corner_radius(8.0)
        .padding(Thickness::new(CARD_PADDING, 12.0, CARD_PADDING, 14.0))
        .content(StackPanel::new().spacing(10.0).children((title_row, body.into())))
}

/// A section's show/hide switch, its label beside it in both states.
pub(crate) fn show_switch(label: &'static str, on: bool, on_toggled: Callback<bool>) -> View {
    ToggleSwitch::new()
        .is_on(on)
        .on_toggled(on_toggled)
        .slots([SlotView::new(ToggleSwitchSlot::OnContent, label), SlotView::new(ToggleSwitchSlot::OffContent, label)])
}

/// A ToggleSwitch with its header above.
pub(crate) fn switch(header: &'static str, on: bool, on_toggled: Callback<bool>) -> View {
    ToggleSwitch::new().is_on(on).on_toggled(on_toggled).slot(ToggleSwitchSlot::Header, header)
}

/// A slider over `span`, with its value in the header. `value` is clamped into the span first, so
/// a stray value can't make WinUI coerce it (and echo the coerced value back).
pub(crate) fn slider(header: String, span: Span, value: f32, enabled: bool, on_change: Callback<f64>) -> View {
    Slider::new()
        .minimum(f64::from(span.min))
        .maximum(f64::from(span.max))
        .value(f64::from(span.clamp(value, span.min)))
        .step_frequency(f64::from(span.step))
        .is_enabled(enabled)
        .on_value_changed(on_change)
        .slot(SliderSlot::Header, header)
}

/// A ComboBox over fixed items. `selected` must be in range: it's dropped otherwise rather than
/// handed to WinUI.
pub(crate) fn combo(
    header: &'static str,
    items: &[&'static str],
    selected: usize,
    enabled: bool,
    on_change: Callback<Option<usize>>,
) -> View {
    ComboBox::new()
        .items_source(items.iter().copied())
        .selected_index((selected < items.len()).then_some(selected))
        .is_enabled(enabled)
        .on_selection_changed(on_change)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .slot(ComboBoxSlot::Header, header)
}

/// A device ComboBox, keyed by its items: a changed list mounts a new native control, items first
/// and then the index, instead of replacing the items under a live selection.
pub(crate) fn device_combo(
    header: &'static str,
    choices: &DeviceChoices,
    placeholder: &str,
    enabled: bool,
    on_change: Callback<Option<usize>>,
) -> View {
    let selected = choices.selected.filter(|&i| i < choices.labels.len());
    let combo = ComboBox::new()
        .items_source(choices.labels.iter().map(String::as_str))
        .selected_index(selected)
        .placeholder_text(placeholder)
        .is_enabled(enabled)
        .on_selection_changed(on_change)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .slot(ComboBoxSlot::Header, header);
    View::keyed_fragment([(choices.key, combo)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{FLOOR_DB, MIN_WIDTH};

    #[test]
    fn the_track_fills_the_card_and_stays_sane() {
        assert_eq!(track_width(520.0), 520.0 - 48.0 - 16.0 - 32.0 - 2.0);
        assert_eq!(track_width(MIN_WIDTH), MIN_WIDTH - 98.0);
        assert_eq!(track_width(5000.0), MAX_CONTENT - 34.0, "capped at the content column");
        assert_eq!(track_width(520.6), track_width(520.0), "whole DIPs");
        for w in [0.0, -100.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let t = track_width(w);
            assert!(t.is_finite() && t >= MIN_TRACK, "{w} -> {t}");
        }
    }

    /// Building views runs the builders' assertions without WinUI.
    #[test]
    fn builders_accept_any_value() {
        let callback = || Callback::new(|_: f64| {});
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1e30, 1e30] {
            let _ = slider("x".to_owned(), FLOOR_DB, v, true, callback());
        }
        let pick = || Callback::new(|_: Option<usize>| {});
        let _ = combo("x", &["a", "b"], 7, true, pick());
        let bad = DeviceChoices { labels: vec!["a".to_owned()], ids: vec![None], selected: Some(3), key: 1 };
        let _ = device_combo("x", &bad, "p", true, pick());
        let _ = card("t", Some(show_switch("s", true, Callback::new(|_: bool| {}))), secondary("x"));
    }
}
