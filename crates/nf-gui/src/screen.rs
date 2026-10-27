//! The window's size against the screen (gui/F6).
//!
//! - **At start-up** the remembered size is clamped to the primary monitor's work area (the screen
//!   less the taskbar). Reactor opens the window at its declared client size, so a size saved on a
//!   bigger monitor, or the 520×760 default on a 1080p laptop at 150% (a 1280×688 DIP work area),
//!   would otherwise put the bottom of the page under the taskbar.
//! - **When resized**, a side that fills the work area (maximised, snapped, or the clamped start-up
//!   height) isn't remembered: the next start would open a restored window that big. The other
//!   side still is, so a width chosen on a screen too short for the default height is kept.
//!
//! Reactor doesn't expose the window's HWND, so the window's own monitor isn't known: the primary
//! monitor is where a new window opens.

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use crate::settings::{MIN_HEIGHT, MIN_WIDTH};

/// The frame around the client area, in DIPs: the standard title bar (about 32) plus a border top
/// and bottom (8 each), and a border left and right.
const FRAME_WIDTH: f64 = 16.0;
const FRAME_HEIGHT: f64 = 48.0;
/// A reported size within this of the limit fills the work area. Larger than the DPI rounding of
/// the declared size (under 1 DIP), so the clamped start-up size counts as filling too.
const FILL_SLACK: f64 = 2.0;

/// The largest client size, in DIPs, that fits the primary monitor's work area with the frame.
/// `None` if Windows doesn't say.
///
/// After Reactor's start-up the process is per-monitor DPI aware, so the work area is in physical
/// pixels and the DPI is the monitor's own. In any other awareness both are scaled alike, so the
/// DIPs come out the same.
pub(crate) fn primary_limit() -> Option<(f64, f64)> {
    // SAFETY: plain queries on the monitor handle Windows returned; `info` is a MONITORINFO with
    // its size set, and the DPI outputs are locals.
    let (work, dpi_x, dpi_y) = unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (0, 0);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).ok()?;
        (info.rcWork, dpi_x, dpi_y)
    };
    to_dips(work, dpi_x, dpi_y).map(client_limit)
}

/// A rectangle in pixels at `dpi`, as a size in DIPs. `None` if it's empty or the DPI is zero.
fn to_dips(rect: RECT, dpi_x: u32, dpi_y: u32) -> Option<(f64, f64)> {
    let width = f64::from(rect.right) - f64::from(rect.left);
    let height = f64::from(rect.bottom) - f64::from(rect.top);
    (width > 0.0 && height > 0.0 && dpi_x > 0 && dpi_y > 0)
        .then(|| (width * 96.0 / f64::from(dpi_x), height * 96.0 / f64::from(dpi_y)))
}

/// The largest client size that fits a `work` area (DIPs) with the frame, but never below the
/// window's minimum size.
fn client_limit(work: (f64, f64)) -> (f64, f64) {
    ((work.0 - FRAME_WIDTH).max(MIN_WIDTH), (work.1 - FRAME_HEIGHT).max(MIN_HEIGHT))
}

/// The start-up client size: the remembered `size` within `limit`, when it's known. `size` is
/// sanitised (finite, at least the minimum), and so is the limit, so the result is too.
pub(crate) fn fit(size: (f64, f64), limit: Option<(f64, f64)>) -> (f64, f64) {
    match limit {
        Some((width, height)) => (size.0.min(width), size.1.min(height)),
        None => size,
    }
}

/// Which sides of a reported client size fill the work area, (across, down): a maximised or
/// snapped window (or the clamped start-up size). A side that fills isn't worth remembering.
pub(crate) fn fills(size: (f64, f64), limit: Option<(f64, f64)>) -> (bool, bool) {
    match limit {
        Some((width, height)) => (size.0 >= width - FILL_SLACK, size.1 >= height - FILL_SLACK),
        None => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DEFAULT_HEIGHT, DEFAULT_WIDTH};

    /// 1920×1080 at 150% with a 48 px taskbar: the review's laptop.
    fn laptop() -> (f64, f64) {
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1032 };
        client_limit(to_dips(work, 144, 144).unwrap())
    }

    #[test]
    fn the_work_area_is_converted_to_dips() {
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1032 };
        assert_eq!(to_dips(work, 144, 144), Some((1280.0, 688.0)));
        assert_eq!(to_dips(work, 96, 96), Some((1920.0, 1032.0)));
        // A second monitor's coordinates can be negative: only the size counts
        let left = RECT { left: -2560, top: -200, right: 0, bottom: 1240 };
        assert_eq!(to_dips(left, 96, 96), Some((2560.0, 1440.0)));
        assert_eq!(to_dips(RECT::default(), 96, 96), None);
        assert_eq!(to_dips(work, 0, 96), None);
    }

    #[test]
    fn the_default_size_fits_a_1080p_laptop_at_150_percent() {
        let limit = laptop();
        assert_eq!(limit, (1264.0, 640.0));
        let (width, height) = fit((DEFAULT_WIDTH, DEFAULT_HEIGHT), Some(limit));
        assert_eq!(width, DEFAULT_WIDTH, "only the height is cut");
        assert!(height + FRAME_HEIGHT <= 688.0, "{height}: the window's bottom is above the taskbar");
    }

    #[test]
    fn a_size_from_a_bigger_monitor_is_cut_to_the_screen() {
        assert_eq!(fit((2400.0, 1400.0), Some(laptop())), (1264.0, 640.0));
        assert_eq!(fit((600.0, 500.0), Some(laptop())), (600.0, 500.0), "a size that fits is kept");
        assert_eq!(fit((2400.0, 1400.0), None), (2400.0, 1400.0), "no work area: as stored");
    }

    #[test]
    fn a_tiny_screen_still_gets_the_minimum_size() {
        let limit = client_limit((600.0 * 96.0 / 144.0, 560.0 * 96.0 / 144.0));
        assert_eq!(limit, (MIN_WIDTH, MIN_HEIGHT));
        assert_eq!(fit((DEFAULT_WIDTH, DEFAULT_HEIGHT), Some(limit)), (MIN_WIDTH, MIN_HEIGHT));
    }

    #[test]
    fn maximised_and_snapped_sizes_fill_the_work_area() {
        let limit = Some(laptop());
        // Maximised: the whole work area less the title bar
        assert_eq!(fills((1280.0, 688.0 - 32.0), limit), (true, true));
        // Snapped to the left half: full height only
        assert_eq!(fills((640.0, 688.0 - 33.0), limit), (false, true));
        // The clamped start-up size, after DPI rounding: its height fills, its width doesn't
        let (width, height) = fit((DEFAULT_WIDTH, DEFAULT_HEIGHT), limit);
        assert_eq!(fills((width, height - 0.6), limit), (false, true));
        // verify-gui NEW-0: on that screen the user widens the window; the height still reads the
        // limit, but the width is theirs and is kept
        assert_eq!(fills((900.0, 640.0), Some((1264.0, 640.0))), (false, true));
        // A size the user picked
        assert_eq!(fills((DEFAULT_WIDTH, 600.0), limit), (false, false));
        assert_eq!(fills((1000.0, 620.0), limit), (false, false));
        assert_eq!(fills((3000.0, 3000.0), None), (false, false), "no work area: everything is remembered");
    }

    /// Calls the real queries (no window is opened). An RDP or headless session still has a
    /// primary monitor, but tolerate one that doesn't.
    #[test]
    fn the_primary_limit_is_sane_when_known() {
        if let Some((width, height)) = primary_limit() {
            assert!(width.is_finite() && height.is_finite(), "{width}×{height}");
            assert!(width >= MIN_WIDTH && height >= MIN_HEIGHT, "{width}×{height}");
        }
    }
}
