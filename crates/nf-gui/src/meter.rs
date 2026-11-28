//! A level meter's maths and its bar (gui.md section 4).
//!
//! The bar is a fixed-width track `Border` holding a left-aligned fill `Border`, so a tick changes
//! one `Width` (and the peak marker's margin), never a Grid's column definitions, which Reactor
//! rebuilds on every change (windows-reactor.md, "Meter bars from Borders"). Every width is clamped
//! and NaN-free: a builder asserts on anything else.

use llcable_control::voice::MeterSnapshot;
use windows_reactor::*;

use crate::widgets::MIN_TRACK;

/// The bar's scale: this level and below is empty, 0 dBFS is full.
pub(crate) const FLOOR_DBFS: f32 = -60.0;
/// The RMS fill turns yellow above this...
pub(crate) const HOT_DBFS: f32 = -12.0;
/// ...and red above this.
pub(crate) const OVER_DBFS: f32 = -3.0;
const BAR_HEIGHT: f64 = 10.0;
/// The peak marker's width.
pub(crate) const MARKER: f64 = 2.0;
/// A track wider than this is a bad input, not a monitor.
const MAX_TRACK: f64 = 4096.0;

const GREEN: Color = Color::rgb(0x3C, 0xB3, 0x71);
const YELLOW: Color = Color::rgb(0xE8, 0xB0, 0x00);
const RED: Color = Color::rgb(0xE0, 0x3C, 0x31);
/// Mid grey at a quarter opacity: visible on light and dark cards alike.
const TRACK: Color = Color::argb(0x40, 0x80, 0x80, 0x80);

/// Where `dbfs` sits on the scale: 0 at or below [`FLOOR_DBFS`] (and for NaN), 1 at 0 dBFS or above.
pub(crate) fn fraction(dbfs: f32) -> f64 {
    if dbfs.is_nan() || dbfs <= FLOOR_DBFS {
        0.0
    } else if dbfs >= 0.0 {
        1.0
    } else {
        f64::from((dbfs - FLOOR_DBFS) / -FLOOR_DBFS)
    }
}

/// A track width a builder accepts: finite, within [`MIN_TRACK`]..[`MAX_TRACK`].
pub(crate) fn sane_track(track: f64) -> f64 {
    if track.is_finite() { track.clamp(MIN_TRACK, MAX_TRACK) } else { MIN_TRACK }
}

/// The RMS fill's width on a `track`-wide bar: in 0..=track, never NaN.
pub(crate) fn fill_width(dbfs: f32, track: f64) -> f64 {
    let track = sane_track(track);
    (fraction(dbfs) * track).clamp(0.0, track)
}

/// The peak marker's left offset, so the marker stays inside the track; `None` (hidden) when the
/// peak is at or below the floor.
pub(crate) fn marker_offset(dbfs: f32, track: f64) -> Option<f64> {
    let room = sane_track(track) - MARKER;
    let f = fraction(dbfs);
    (f > 0.0).then(|| (f * room).clamp(0.0, room))
}

/// The RMS fill's colour band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Normal,
    Hot,
    Over,
}

pub(crate) fn level(rms_dbfs: f32) -> Level {
    if rms_dbfs > OVER_DBFS {
        Level::Over
    } else if rms_dbfs > HOT_DBFS {
        Level::Hot
    } else {
        Level::Normal
    }
}

fn colour(level: Level) -> Color {
    match level {
        Level::Normal => GREEN,
        Level::Hot => YELLOW,
        Level::Over => RED,
    }
}

/// The clip badge's fill when latched.
pub(crate) const CLIP_RED: Color = RED;

/// One meter's bar, `track` DIPs wide: the RMS fill and a thin peak marker over it.
pub(crate) fn bar(m: MeterSnapshot, track: f64) -> View {
    let track = sane_track(track);
    let marker = marker_offset(m.peak_dbfs, track);
    Border::new()
        .width(track)
        .height(BAR_HEIGHT)
        .horizontal_alignment(HorizontalAlignment::Left)
        .corner_radius(BAR_HEIGHT / 2.0)
        .background(TRACK)
        .content(
            Grid::new().children((
                Border::new()
                    .width(fill_width(m.rms_dbfs, track))
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .corner_radius(BAR_HEIGHT / 2.0)
                    .background(colour(level(m.rms_dbfs))),
                Border::new()
                    .width(MARKER)
                    .margin(Thickness::new(marker.unwrap_or(0.0), 0.0, 0.0, 0.0))
                    .opacity(if marker.is_some() { 1.0 } else { 0.0 })
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .background(ThemeBrush::PrimaryText),
            )),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ODD: [f32; 8] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -200.0, -1e30, 1e30, f32::MIN_POSITIVE, -0.0];

    #[test]
    fn the_scale_maps_minus_60_to_0_dbfs_onto_0_to_1() {
        assert_eq!(fraction(-60.0), 0.0);
        assert_eq!(fraction(-30.0), 0.5);
        assert_eq!(fraction(-15.0), 0.75);
        assert_eq!(fraction(0.0), 1.0);
        assert_eq!(fraction(6.0), 1.0, "over full scale is full");
        assert_eq!(fraction(-75.0), 0.0);
        assert_eq!(fraction(-200.0), 0.0, "the engine's silence");
        assert_eq!(fraction(f32::NAN), 0.0);
        assert_eq!(fraction(f32::NEG_INFINITY), 0.0);
        assert_eq!(fraction(f32::INFINITY), 1.0);
    }

    #[test]
    fn the_fill_is_a_fraction_of_the_track() {
        assert_eq!(fill_width(-30.0, 300.0), 150.0);
        assert_eq!(fill_width(0.0, 300.0), 300.0);
        assert_eq!(fill_width(-60.0, 300.0), 0.0);
    }

    #[test]
    fn widths_are_always_valid_for_a_builder() {
        for track in [300.0, 0.0, -5.0, f64::NAN, f64::INFINITY, 1e12] {
            for db in ODD.into_iter().chain([-60.0, -30.0, 0.0]) {
                let w = fill_width(db, track);
                assert!(w.is_finite() && w >= 0.0 && w <= sane_track(track), "{db} on {track}: {w}");
                if let Some(x) = marker_offset(db, track) {
                    assert!(x.is_finite() && x >= 0.0 && x + MARKER <= sane_track(track), "{db} on {track}: {x}");
                }
            }
        }
    }

    #[test]
    fn the_marker_hides_below_the_floor_and_stays_inside() {
        assert_eq!(marker_offset(-60.0, 300.0), None);
        assert_eq!(marker_offset(f32::NAN, 300.0), None);
        assert_eq!(marker_offset(0.0, 300.0), Some(300.0 - MARKER));
        assert_eq!(marker_offset(-30.0, 302.0), Some(150.0));
    }

    #[test]
    fn the_fill_colour_bands() {
        assert_eq!(level(-40.0), Level::Normal);
        assert_eq!(level(-12.0), Level::Normal, "yellow is above -12");
        assert_eq!(level(-11.9), Level::Hot);
        assert_eq!(level(-3.0), Level::Hot, "red is above -3");
        assert_eq!(level(-2.9), Level::Over);
        assert_eq!(level(f32::NAN), Level::Normal);
        assert_eq!(level(f32::INFINITY), Level::Over);
    }

    /// The bar's builders assert on bad widths and margins: none of these may panic.
    #[test]
    fn bars_build_for_any_reading() {
        for track in [300.0, f64::NAN, -1.0, f64::INFINITY] {
            for peak in ODD {
                for rms in ODD {
                    let _ = bar(MeterSnapshot { peak_dbfs: peak, rms_dbfs: rms }, track);
                }
            }
        }
    }
}
