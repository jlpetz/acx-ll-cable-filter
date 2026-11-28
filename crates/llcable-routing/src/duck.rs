use serde::{Deserialize, Serialize};

use super::Input;

/// Peak-triggered ducking. The sidechain controls attenuation but is never mixed into the output by this effect.
/// Attack and release are full-range linear-gain ramp times. Hold bridges brief natural pauses in the key.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ducking {
    pub threshold_db: f32,
    pub attenuation_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for Ducking {
    fn default() -> Self {
        Self { threshold_db: -30.0, attenuation_db: 12.0, attack_ms: 10.0, hold_ms: 120.0, release_ms: 220.0 }
    }
}

pub(super) struct Ducker {
    threshold: f32,
    minimum: f64,
    attack: f64,
    release: f64,
    hold_frames: usize,
    remaining: usize,
    gain: f64,
}

impl Ducker {
    pub fn gain(&self) -> f32 {
        self.gain as f32
    }

    pub fn new(s: Ducking, rate: u32) -> Option<Self> {
        if !(-120.0..=0.0).contains(&s.threshold_db)
            || !(0.0..=120.0).contains(&s.attenuation_db)
            || [s.attack_ms, s.release_ms, s.hold_ms].iter().any(|x| !(0.0..=60_000.0).contains(x))
        {
            return None;
        }
        let frames = |ms: f32| (f64::from(ms) * f64::from(rate) / 1000.0).round() as usize;
        let minimum = 10f64.powf(-f64::from(s.attenuation_db) / 20.0);
        Some(Self {
            threshold: 10f32.powf(s.threshold_db / 20.0),
            minimum,
            attack: (1.0 - minimum) / frames(s.attack_ms).max(1) as f64,
            release: (1.0 - minimum) / frames(s.release_ms).max(1) as f64,
            hold_frames: frames(s.hold_ms),
            remaining: 0,
            gain: 1.0,
        })
    }

    fn frame(&mut self, key_peak: f32, audio: &mut [f32; 2]) {
        if key_peak >= self.threshold {
            self.remaining = self.hold_frames;
            self.gain = (self.gain - self.attack).max(self.minimum);
        } else if self.remaining > 0 {
            self.remaining -= 1;
            self.gain = (self.gain - self.attack).max(self.minimum);
        } else {
            self.gain = (self.gain + self.release).min(1.0);
        }
        let gain = self.gain as f32;
        audio[0] *= gain;
        audio[1] *= gain;
    }

    pub fn process(&mut self, key: Input<'_>, audio: &mut [[f32; 2]]) {
        match key {
            Input::Silence(_) if self.remaining == 0 && self.gain == 1.0 => {}
            Input::Silence(_) => audio.iter_mut().for_each(|frame| self.frame(0.0, frame)),
            Input::Mono(key) => audio.iter_mut().zip(key).for_each(|(frame, sample)| self.frame(sample.abs(), frame)),
            Input::Stereo(key) => audio.iter_mut().zip(key).for_each(|(frame, peak)| self.frame(peak[0].abs().max(peak[1].abs()), frame)),
        }
    }
}
