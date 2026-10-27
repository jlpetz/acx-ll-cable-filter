//! QPC helpers and sample-index → QPC mapping.
//!
//! WASAPI's capture stamps (`BufferInfo::timestamp`) and `AudioClock` positions are QPC in 100 ns
//! units ("hns"), so everything here works in hns. QPC is one timebase for the whole machine, so
//! capture and play-out stamps subtract directly.

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 100 ns units per second.
pub(crate) const HNS_PER_S: f64 = 10_000_000.0;

/// The QPC clock, with its frequency read once (it's fixed at boot).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Qpc {
    freq: i64,
}

impl Qpc {
    pub(crate) fn new() -> Self {
        let mut freq = 0i64;
        // SAFETY: a valid out-pointer to a live local. The call can't fail on XP or later; if it
        // somehow did, `freq` stays 0 and the fallback below keeps every conversion finite.
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        Self { freq: if freq > 0 { freq } else { 10_000_000 } }
    }

    /// Raw QPC ticks. No allocation and (normally) no kernel transition.
    #[inline]
    pub(crate) fn ticks(&self) -> i64 {
        let mut t = 0i64;
        // SAFETY: a valid out-pointer to a live local.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        t
    }

    #[inline]
    pub(crate) fn now_hns(&self) -> i64 {
        ticks_to_hns(self.ticks(), self.freq)
    }

    /// Converts a tick difference to microseconds.
    #[inline]
    pub(crate) fn ticks_to_us(&self, ticks: u64) -> f32 {
        (ticks as f64 * 1e6 / self.freq as f64) as f32
    }
}

/// Ticks to 100 ns without overflow: the remainder is below `freq`, so `rem * 1e7` fits an i64 for
/// any real frequency.
#[inline]
pub(crate) fn ticks_to_hns(ticks: i64, freq: i64) -> i64 {
    const HNS: i64 = 10_000_000;
    if freq == HNS {
        return ticks;
    }
    if freq <= 0 {
        return ticks;
    }
    (ticks / freq) * HNS + (ticks % freq) * HNS / freq
}

/// Stamps further than this from "now" are rejected (Chromium's sanity check).
const MAX_STAMP_AGE_HNS: f64 = 500.0 * 1e4;

/// Maps a stream's absolute sample index to its capture time, anchored at the latest trusted
/// packet stamp. Each new stamp re-anchors the map, so the device's clock drift never accumulates.
#[derive(Clone, Debug)]
pub(crate) struct StampMap {
    anchor_index: u64,
    anchor_hns: f64,
    hns_per_sample: f64,
    valid: bool,
}

impl StampMap {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self { anchor_index: 0, anchor_hns: 0.0, hns_per_sample: HNS_PER_S / sample_rate as f64, valid: false }
    }

    /// Offers the stamp of the packet whose first sample has absolute index `index`. Rejected when
    /// it's more than 500 ms from `now_hns` or goes backwards (Chromium's checks); returns whether
    /// it was taken.
    pub(crate) fn observe(&mut self, index: u64, stamp_hns: u64, now_hns: f64) -> bool {
        let t = stamp_hns as f64;
        if (t - now_hns).abs() > MAX_STAMP_AGE_HNS {
            return false;
        }
        if self.valid && (t < self.anchor_hns || index < self.anchor_index) {
            return false;
        }
        self.anchor_index = index;
        self.anchor_hns = t;
        self.valid = true;
        true
    }

    /// The capture time of absolute sample `index`, if any stamp has been taken.
    #[inline]
    pub(crate) fn time_of(&self, index: u64) -> Option<f64> {
        self.valid.then(|| self.time_of_unchecked(index))
    }

    #[inline]
    fn time_of_unchecked(&self, index: u64) -> f64 {
        let offset = index as f64 - self.anchor_index as f64;
        self.anchor_hns + offset * self.hns_per_sample
    }
}

/// Maps a (fractional) sample position to QPC hns using `(sample index, hns)` stamps, interpolating
/// linearly between the two surrounding stamps (that follows the device's real rate) and
/// extrapolating at the nominal rate `fs` beyond the ends. Stamps must increase in index;
/// out-of-order ones are skipped. `None` without stamps.
pub(crate) fn interpolate_stamps(stamps: &[(u64, f64)], position: f64, fs: f64) -> Option<f64> {
    let first = stamps.first()?;
    let hns_per_sample = HNS_PER_S / fs;
    // The last stamp at or before `position` (stamps are sorted by index).
    let after = stamps.partition_point(|s| (s.0 as f64) <= position);
    if after == 0 {
        return Some(first.1 + (position - first.0 as f64) * hns_per_sample);
    }
    let lo = stamps.get(after - 1)?;
    match stamps.get(after) {
        Some(hi) if hi.0 > lo.0 => {
            let frac = (position - lo.0 as f64) / (hi.0 - lo.0) as f64;
            Some(lo.1 + (hi.1 - lo.1) * frac)
        }
        _ => Some(lo.1 + (position - lo.0 as f64) * hns_per_sample),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_convert_without_overflow() {
        assert_eq!(ticks_to_hns(12_345, 10_000_000), 12_345);
        assert_eq!(ticks_to_hns(3_000_000, 3_000_000), 10_000_000);
        // 30 days of uptime at 24 MHz.
        let freq = 24_000_000i64;
        let ticks = freq * 86_400 * 30 + freq / 2;
        assert_eq!(ticks_to_hns(ticks, freq), 10_000_000 * 86_400 * 30 + 5_000_000);
    }

    #[test]
    fn qpc_is_monotonic_and_positive() {
        let q = Qpc::new();
        let a = q.now_hns();
        let b = q.now_hns();
        assert!(a > 0 && b >= a);
        assert!(q.ticks_to_us(0) == 0.0);
    }

    #[test]
    fn stamp_map_follows_the_latest_stamp() {
        let mut m = StampMap::new(48_000);
        assert_eq!(m.time_of(0), None);
        let now = 1e9;
        assert!(m.observe(0, 1e9 as u64, now));
        // 480 samples = 10 ms = 100_000 hns.
        assert_eq!(m.time_of(480), Some(1e9 + 100_000.0));
        // A new stamp re-anchors: the device runs 1% fast.
        assert!(m.observe(4800, (1e9 + 990_000.0) as u64, now));
        assert_eq!(m.time_of(4800), Some(1e9 + 990_000.0));
        assert_eq!(m.time_of(5280), Some(1e9 + 1_090_000.0));
        // Samples before the anchor map backwards from it.
        assert_eq!(m.time_of(4320), Some(1e9 + 890_000.0));
    }

    #[test]
    fn stamp_map_rejects_stale_and_backward_stamps() {
        let mut m = StampMap::new(48_000);
        let now = 2e9;
        // 600 ms in the past: rejected.
        assert!(!m.observe(0, (now - 6e6) as u64, now));
        assert_eq!(m.time_of(0), None);
        assert!(m.observe(0, now as u64, now));
        // Earlier than the anchor: rejected, the anchor stays.
        assert!(!m.observe(480, (now - 10.0) as u64, now));
        assert_eq!(m.time_of(0), Some(now));
    }

    #[test]
    fn interpolation_between_and_beyond_stamps() {
        assert_eq!(interpolate_stamps(&[], 10.0, 48_000.0), None);
        // A device running 0.1% slow: 480 samples take 100_100 hns between stamps.
        let stamps = [(0u64, 1000.0), (480, 1000.0 + 100_100.0), (960, 1000.0 + 200_200.0)];
        assert_eq!(interpolate_stamps(&stamps, 0.0, 48_000.0), Some(1000.0));
        assert_eq!(interpolate_stamps(&stamps, 480.0, 48_000.0), Some(101_100.0));
        let mid = interpolate_stamps(&stamps, 240.0, 48_000.0).unwrap();
        assert!((mid - (1000.0 + 50_050.0)).abs() < 1e-6);
        // Beyond the last stamp: the nominal rate.
        let after = interpolate_stamps(&stamps, 1440.0, 48_000.0).unwrap();
        assert!((after - (1000.0 + 200_200.0 + 100_000.0)).abs() < 1e-6);
        // Before the first: the nominal rate, backwards.
        let before = interpolate_stamps(&stamps[1..], 0.0, 48_000.0).unwrap();
        assert!((before - (101_100.0 - 100_000.0)).abs() < 1e-6);
        // One stamp: the nominal rate both ways.
        assert_eq!(interpolate_stamps(&stamps[..1], 48.0, 48_000.0), Some(1000.0 + 10_000.0));
    }

    #[test]
    fn interpolation_skips_a_duplicate_index() {
        let stamps = [(0u64, 0.0), (480, 100_000.0), (480, 100_500.0)];
        let t = interpolate_stamps(&stamps, 600.0, 48_000.0).unwrap();
        assert!((t - (100_500.0 + 25_000.0)).abs() < 1e-6);
    }
}
