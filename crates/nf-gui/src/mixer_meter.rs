//! Sample-peak displays for the live mixer and its design preview. Levels are in dBFS and the channel selector changes only their presentation.
use serde::{Deserialize, Serialize};
use windows_reactor::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterMode {
    #[default]
    Auto,
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterPalette {
    Classic,
    #[default]
    BlueFloor,
}

pub const BAND_LEGEND: &str =
    "Peak scale −60 to 0 dBFS. Green below −12, yellow from −12, amber from −6, red from −3. CLIP at 0. Optional blue below −36.";
const FLOOR_DB: i32 = -60;
const STEP_DB: i32 = 3;
const SEGMENTS: usize = (-FLOOR_DB / STEP_DB) as usize;
const TRACK: Color = Color::argb(52, 128, 128, 128);

fn colour(start_db: i32, palette: MeterPalette) -> Color {
    match start_db {
        ..-36 if palette == MeterPalette::BlueFloor => Color::rgb(0x3B, 0xA4, 0xD9),
        ..-12 => Color::rgb(0x32, 0xB2, 0x77),
        -12..-6 => Color::rgb(0xDC, 0xB9, 0x31),
        -6..-3 => Color::rgb(0xF0, 0x92, 0x31),
        _ => Color::rgb(0xDE, 0x52, 0x4E),
    }
}

fn peak_text(db: f32) -> String {
    if db == f32::NEG_INFINITY {
        "−∞ dBFS".into()
    } else if !db.is_finite() {
        "—".into()
    } else if db >= 0.0 {
        format!("{:.1} CLIP", db.abs())
    } else if db > -0.05 {
        ">−0.1 dBFS".into()
    } else {
        format!("−{:.1} dBFS", -db)
    }
}

fn lane(label: &str, name: &str, db: f32, palette: MeterPalette) -> View {
    let segments = Grid::new().columns([GridLength::STAR; SEGMENTS]).column_spacing(1.0).vertical_alignment(VerticalAlignment::Center);
    let bar = segments.keyed_children((0..SEGMENTS).map(|cell| {
        let start_db = FLOOR_DB + cell as i32 * STEP_DB;
        let lit = db.is_finite() && db > FLOOR_DB as f32 && db >= start_db as f32;
        (
            cell,
            Border::new().grid_column(cell as i32).height(7.0).corner_radius(1.0).background(if lit {
                colour(start_db, palette)
            } else {
                TRACK
            }),
        )
    }));
    let peak = peak_text(db);
    Grid::new().columns([GridLength::Pixel(29.0), GridLength::STAR, GridLength::Pixel(71.0)]).column_spacing(4.0).children((
        TextBlock::new().text(name).font_size(11.0).opacity(0.80),
        Border::new().grid_column(1).vertical_alignment(VerticalAlignment::Center).content(bar),
        TextBlock::new().grid_column(2).horizontal_alignment(HorizontalAlignment::Right).text(&peak).font_size(11.0).automation_name(
            format!("{label}, {name}, sample peak {peak}{}", if db.is_finite() && db >= 0.0 { " dBFS, full scale reached" } else { "" }),
        ),
    ))
}

fn highest(peaks: &[f32]) -> f32 {
    peaks.iter().copied().filter(|&db| db.is_finite() || db == f32::NEG_INFINITY).reduce(f32::max).unwrap_or(f32::NAN)
}

/// Mono presentation shows the loudest channel so an overload on either side survives collapsing the view.
/// A stereo presentation leaves a real mono signal as one lane. For wider inputs it shows the first pair and the peak across all channels.
pub fn strip(label: &str, peaks: &[f32], mode: MeterMode, palette: MeterPalette) -> View {
    match (peaks, mode) {
        ([], _) => TextBlock::new().text("No channels").font_size(12.0).into(),
        ([mono], _) => lane(label, "M", *mono, palette),
        (_, MeterMode::Mono) => lane(label, "Max", highest(peaks), palette),
        ([left, right], _) => {
            StackPanel::new().spacing(2.0).children((lane(label, "L", *left, palette), lane(label, "R", *right, palette)))
        }
        (_, _) => StackPanel::new().spacing(2.0).children((
            lane(label, "Ch 1", peaks[0], palette),
            lane(label, "Ch 2", peaks[1], palette),
            TextBlock::new().font_size(11.0).text(format!("{} channels · Highest peak {}", peaks.len(), peak_text(highest(peaks)))),
        )),
    }
}

/// Expanded channel list for the appearance panel. Numbered channels do not assume a surround or speaker layout.
pub fn channel_detail(label: &str, peaks: &[f32], palette: MeterPalette) -> View {
    StackPanel::new()
        .spacing(2.0)
        .keyed_children(peaks.iter().enumerate().map(|(index, &peak)| (index, lane(label, &(index + 1).to_string(), peak, palette))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_scale_or_above_indicates_clipping() {
        for db in [f32::NEG_INFINITY, -75.0, -3.0, -0.01, f32::NAN, f32::INFINITY] {
            assert!(!peak_text(db).contains("CLIP"));
        }
        assert_eq!(peak_text(-0.01), ">−0.1 dBFS");
        assert!(peak_text(0.0).contains("CLIP"));
        assert!(peak_text(2.0).contains("CLIP"));
        assert_eq!(peak_text(f32::NEG_INFINITY), "−∞ dBFS");
    }

    #[test]
    fn combined_and_compact_meters_account_for_every_channel() {
        let mut surround = [-30.0; 16];
        surround[15] = 0.3;
        assert_eq!(highest(&surround), 0.3);
        assert_eq!(highest(&[f32::INFINITY, -12.0, f32::NAN]), -12.0);
        assert_eq!(highest(&[f32::NEG_INFINITY, f32::NEG_INFINITY]), f32::NEG_INFINITY);
    }
}
