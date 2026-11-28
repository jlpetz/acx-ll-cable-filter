//! A packet-size independent stereo three-band EQ. Coefficients follow the [Audio EQ Cookbook](https://www.w3.org/TR/audio-eq-cookbook/).
use std::f64::consts::{SQRT_2, TAU};

use llcable_routing::{Effect, Input, PacketEffect};
use nf_core::SAMPLE_RATE;

/// Band gains in dB, inclusive range -18 to +18. The default is flat. Each band at 0 dB is bypassed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EqSettings {
    /// 180 Hz low shelf, shelf slope 1.
    pub low_db: f32,
    /// 1 kHz peaking band, Q = 1.
    pub mid_db: f32,
    /// 5 kHz high shelf, shelf slope 1.
    pub high_db: f32,
}

enum Shape {
    LowShelf,
    Peak,
    HighShelf,
}

struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z1: [f64; 2],
    z2: [f64; 2],
}

impl Biquad {
    fn new(shape: Shape, hz: f64, db: f32) -> Self {
        let gain = 10_f64.powf(f64::from(db) / 40.);
        let (sin, cos) = (TAU * hz / f64::from(SAMPLE_RATE)).sin_cos();
        let sum = gain + 1.;
        let diff = gain - 1.;
        let shelf_width = SQRT_2 * gain.sqrt() * sin;
        let (num, den) = match shape {
            Shape::Peak => {
                let alpha = sin / 2.;
                ([1. + alpha * gain, -2. * cos, 1. - alpha * gain], [1. + alpha / gain, -2. * cos, 1. - alpha / gain])
            }
            Shape::LowShelf => {
                ([gain * (sum - diff * cos + shelf_width), 2. * gain * (diff - sum * cos), gain * (sum - diff * cos - shelf_width)], [
                    sum + diff * cos + shelf_width,
                    -2. * (diff + sum * cos),
                    sum + diff * cos - shelf_width,
                ])
            }
            Shape::HighShelf => {
                ([gain * (sum + diff * cos + shelf_width), -2. * gain * (diff + sum * cos), gain * (sum + diff * cos - shelf_width)], [
                    sum - diff * cos + shelf_width,
                    2. * (diff - sum * cos),
                    sum - diff * cos - shelf_width,
                ])
            }
        };
        Self { b: num.map(|v| v / den[0]), a: [den[1] / den[0], den[2] / den[0]], z1: [0.; 2], z2: [0.; 2] }
    }

    fn process(&mut self, packet: &mut [[f32; 2]]) {
        let [b0, b1, b2] = self.b;
        let [a1, a2] = self.a;
        for frame in packet {
            for (c, sample) in frame.iter_mut().enumerate() {
                let x = f64::from(*sample);
                let y = b0 * x + self.z1[c];
                self.z1[c] = b1 * x - a1 * y + self.z2[c];
                self.z2[c] = b2 * x - a2 * y;
                *sample = y as f32;
            }
        }
        // Clear inaudible tails before they enter denormal arithmetic in sustained digital silence.
        for c in 0..2 {
            if self.z1[c].abs().max(self.z2[c].abs()) < 1e-30 {
                self.z1[c] = 0.;
                self.z2[c] = 0.;
            }
        }
    }
}

struct Equalizer {
    bands: Vec<Biquad>,
}

impl PacketEffect for Equalizer {
    fn ready_on_thread(&self) -> bool {
        true
    }

    fn required_sample_rate(&self) -> Option<u32> {
        Some(SAMPLE_RATE)
    }
    fn process(&mut self, stereo: &mut [[f32; 2]], _: Option<Input<'_>>) {
        for band in &mut self.bands {
            band.process(stereo);
        }
    }
}

/// Native stereo EQ with independent channel histories and no lookahead or packet buffering. Its output retains floating-point headroom.
pub fn equalizer(settings: EqSettings) -> Result<Effect, String> {
    let config =
        [(Shape::LowShelf, 180., settings.low_db), (Shape::Peak, 1000., settings.mid_db), (Shape::HighShelf, 5000., settings.high_db)];
    let mut bands = Vec::new();
    for (shape, hz, gain) in config {
        if !(-18.0..=18.0).contains(&gain) {
            return Err(format!("The {hz} Hz EQ gain must be between -18 and +18 dB."));
        }
        if gain != 0. {
            bands.push(Biquad::new(shape, hz, gain));
        }
    }
    Ok(Effect::Custom(Box::new(Equalizer { bands })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(settings: EqSettings) -> Box<dyn PacketEffect> {
        let Effect::Custom(eq) = equalizer(settings).unwrap() else { unreachable!() };
        eq
    }

    #[test]
    fn ranges_are_enforced_and_flat_bypasses_each_sample_bit_for_bit() {
        for bad in [f32::NAN, f32::INFINITY, -18.01, 18.01] {
            assert!(equalizer(EqSettings { low_db: bad, ..EqSettings::default() }).is_err());
        }
        let mut eq = eq(EqSettings::default());
        let mut packet = [[-0., 0.75], [1.5, -1.25], [f32::MIN_POSITIVE, f32::from_bits(7)]];
        let original = packet.map(|f| f.map(f32::to_bits));
        eq.process(&mut packet, None);
        assert_eq!(original, packet.map(|f| f.map(f32::to_bits)));
    }

    #[test]
    fn boost_and_cut_hit_the_prescribed_response() {
        fn measured(settings: EqSettings, hz: f64) -> f64 {
            let mut eq = eq(settings);
            let source: Vec<_> = (0..24_000).map(|t| [(0.1 * (TAU * hz * t as f64 / 48000.).sin()) as f32, 0.]).collect();
            let mut output = source.clone();
            for packet in output.chunks_mut(128) {
                eq.process(packet, None);
            }
            let power = |s: &[[f32; 2]]| s[12_000..].iter().map(|f| f64::from(f[0]).powi(2)).sum::<f64>();
            10. * (power(&output) / power(&source)).log10()
        }
        for gain in [-18., 12., 18.] {
            let low = EqSettings { low_db: gain, ..Default::default() };
            let mid = EqSettings { mid_db: gain, ..Default::default() };
            let high = EqSettings { high_db: gain, ..Default::default() };
            for (settings, hz, want) in [
                (low, 20., gain),
                (low, 180., gain / 2.),
                (mid, 1000., gain),
                (high, 5000., gain / 2.),
                (high, 20000., gain),
                (low, 10000., 0.),
                (high, 100., 0.),
            ] {
                let db = measured(settings, hz);
                assert!((db - f64::from(want)).abs() < 0.06, "{settings:?} at {hz} Hz: {db} dB, expected {want}");
            }
        }
    }

    #[test]
    fn independent_channels_are_stable_at_extremes_and_decay_to_zero() {
        for low_db in [-18., 18.] {
            for mid_db in [-18., 18.] {
                for high_db in [-18., 18.] {
                    let mut eq = eq(EqSettings { low_db, mid_db, high_db });
                    let mut samples = vec![[0.; 2]; 48_000];
                    samples[0][1] = 1.;
                    for packet in samples.chunks_mut(96) {
                        eq.process(packet, None);
                    }
                    assert!(samples[0][1].abs() > 0., "no added packet of delay");
                    assert!(samples.iter().all(|s| s[0] == 0. && s[1].abs() < 12.));
                    assert_eq!(samples.last(), Some(&[0., 0.]));
                }
            }
        }
    }
}
