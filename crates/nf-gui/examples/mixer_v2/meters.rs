pub(super) use nf_gui::mixer_meter::{BAND_LEGEND, MeterMode, MeterPalette, channel_detail, strip};

/// Deterministic demo levels at the UI refresh rate. The mic pauses and the two distinct stereo sources have independent L/R variation.
pub(super) fn source_levels(source: usize, tick: u32) -> Vec<f32> {
    let phase = (tick % 160) as f32 * std::f32::consts::TAU / 160.0;
    match source {
        0 if tick % 160 >= 129 => vec![f32::NEG_INFINITY],
        0 => vec![-12.0 + 5.9 * (phase * 6.0).sin() + 4.3 * (phase * 13.0).sin()],
        1 => vec![-18.0 + 7.5 * (phase * 3.0).sin(), -14.5 + 8.0 * (phase * 3.0 + 1.3).sin()],
        2 => vec![-10.0 + 7.8 * (phase * 2.0 + 0.6).sin(), -16.0 + 5.2 * (phase * 4.0).sin()],
        _ => Vec::new(),
    }
}
