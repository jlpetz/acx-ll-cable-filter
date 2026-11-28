//! Presentation settings for the v2 example. Displaying one peak or sixteen channels never changes the signal.
use serde::{Deserialize, Serialize};
use windows_reactor::*;

use super::meters::{self, MeterMode, MeterPalette};
use super::{Mixer, Msg};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(super) struct Appearance {
    pub(super) theme: ThemeChoice,
    pub(super) compact: bool,
    pub(super) mode: MeterMode,
    pub(super) palette: MeterPalette,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme: ThemeChoice::System, compact: true, mode: MeterMode::Auto, palette: MeterPalette::BlueFloor }
    }
}

impl Appearance {
    pub(super) fn meter_mode(self) -> MeterMode {
        match self.mode {
            MeterMode::Auto if self.compact => MeterMode::Mono,
            MeterMode::Auto => MeterMode::Stereo,
            mode => mode,
        }
    }

    pub(super) fn window_theme(self) -> WindowTheme {
        match self.theme {
            ThemeChoice::System => WindowTheme::System,
            ThemeChoice::Light => WindowTheme::Light,
            ThemeChoice::Dark => WindowTheme::Dark,
        }
    }
}

fn choice<T: Copy + PartialEq + 'static, const N: usize>(
    cx: &ViewContext<Mixer>, label: &'static str, current: T, choices: [(T, &'static str); N], change: impl Fn(T) -> Appearance + 'static,
) -> View {
    let sender = cx.sender();
    RadioButtons::new()
        .max_columns(N as i32)
        .items_source(choices.iter().map(|(_, name)| *name))
        .selected_index(choices.iter().position(|(value, _)| *value == current))
        .on_selection_changed(move |selection: Option<usize>| {
            if let Some(&(value, _)) = selection.and_then(|i| choices.get(i))
                && value != current
            {
                sender.send(Msg::Appearance(change(value)));
            }
        })
        .slot(RadioButtonsSlot::Header, label)
}

pub(super) fn view(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let prefs = app.prefs;
    let theme = choice(
        cx,
        "Window theme",
        prefs.theme,
        [(ThemeChoice::System, "System"), (ThemeChoice::Light, "Light"), (ThemeChoice::Dark, "Dark")],
        move |theme| Appearance { theme, ..prefs },
    );
    let compact = ToggleSwitch::new()
        .is_on(prefs.compact)
        .on_toggled(cx.callback(move |compact| Msg::Appearance(Appearance { compact, ..prefs })))
        .slot(ToggleSwitchSlot::Header, "Compact spacing");
    let channels = choice(
        cx,
        "Meter display",
        prefs.mode,
        [(MeterMode::Auto, "Auto (follows spacing)"), (MeterMode::Mono, "Mono peak"), (MeterMode::Stereo, "Stereo pair")],
        move |mode| Appearance { mode, ..prefs },
    );
    let palette = choice(
        cx,
        "Meter colours",
        prefs.palette,
        [(MeterPalette::Classic, "Classic"), (MeterPalette::BlueFloor, "Blue at low levels")],
        move |palette| Appearance { palette, ..prefs },
    );
    // Include one hot channel outside the first stereo pair so its peak is discoverable even when detail is closed.
    let peaks = [-21.0, -19.0, -60.0, -38.0, -27.0, -12.0, -18.0, -24.0, -16.0, -45.0, -8.0, -14.0, -30.0, -4.0, 0.0, -41.0];
    let expanded = app.show_channels;
    let show_channels = Button::new()
        .horizontal_alignment(HorizontalAlignment::Left)
        .on_click(cx.callback(move |()| Msg::ShowChannels(!expanded)))
        .content(if expanded { "Collapse channel detail" } else { "Show all 16 channels" });
    let example = StackPanel::new().spacing(6.0).children((
        TextBlock::new().text("16-channel meter example").font_weight(FontWeight::SEMI_BOLD),
        meters::strip("16-channel example", &peaks, prefs.meter_mode(), prefs.palette),
        show_channels,
        if expanded { meters::channel_detail("16-channel example", &peaks, prefs.palette) } else { View::empty() },
    ));
    StackPanel::new().spacing(if prefs.compact { 8.0 } else { 14.0 }).children((
        theme,
        compact,
        channels,
        palette,
        TextBlock::new().text(meters::BAND_LEGEND).font_size(12.0).text_wrapping(TextWrapping::Wrap),
        Border::new().padding(10.0).background(ThemeBrush::CardBackground).corner_radius(6.0).content(example),
    ))
}
