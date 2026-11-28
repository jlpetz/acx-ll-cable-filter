//! Optional Rust voice effects for a 48 kHz [`llcable_routing::Router`]. Consecutive mono voice stages share one 480-frame
//! packet adapter. Each model retains its own algorithmic delay. A sidechain on a later stage starts a new group, keeping its
//! reference aligned to the packet supplied by the router. A voice branch duplicates its output to stereo. An empty branch
//! bypasses both the model and that buffering. The linked stereo gate can operate on a single frame with no lookahead.
//! Construct and destroy effects off the audio callback. Call `router.prepare_thread()` on the thread where audio will run
//! before starting the stream, since RNNoise's FFT scratch belongs to that thread.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use llcable_routing::{Effect, Input, PacketEffect, StageMeter, StageReading, Tap};
use nf_core::config::{GateConfig, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::dsp::sanitize;
use nf_core::noise::NoiseModel;
use nf_core::rt::Dither;
use nf_core::stage::{Activation, AecTelemetry, ProcessIo, Stage};
use nf_core::{BLOCK, Block, SAMPLE_RATE, db_to_lin};
use nf_stages::{Aec3Stage, RnnoiseModel};
use sonora::config::{MaxProcessingRate, NoiseSuppression, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

mod eq;
pub use eq::{EqSettings, equalizer};

#[cfg(feature = "clap")]
mod clap;
#[cfg(feature = "clap")]
pub use clap::{EffectInfo, Parameter, clap_voice, inspect_clap};

trait VoiceModel: Send {
    fn prepare_thread(&mut self) {}
    fn ready_on_thread(&self) -> bool {
        true
    }
    fn reference(&mut self, _key: Input<'_>, _packet_offset: usize, _span: Range<usize>) {}
    fn reading(&self) -> StageReading {
        StageReading::default()
    }
    fn process(&mut self, input: &Block, out: &mut Block);
}

struct VoiceStage {
    model: Box<dyn VoiceModel>,
    mic_dither: Option<Dither>,
    meter: Option<StageMeter>,
}

struct Voice {
    stages: Vec<VoiceStage>,
    blocks: [Block; 2],
    active: usize,
    cursor: usize,
}

impl Voice {
    fn new(model: impl VoiceModel + 'static, mic_dither: bool) -> Self {
        let stage = VoiceStage { model: Box::new(model), mic_dither: mic_dither.then(|| Dither::new(0x2545_F491)), meter: None };
        Self { stages: vec![stage], blocks: [[0.0; BLOCK]; 2], active: 0, cursor: 0 }
    }

    fn effect(model: impl VoiceModel + 'static) -> Effect {
        Effect::Custom(Box::new(Self::new(model, false)))
    }

    fn hop(&mut self) {
        let [a, b] = &mut self.blocks;
        let (mut source, mut dest) = if self.active == 0 { (a, b) } else { (b, a) };
        for stage in &mut self.stages {
            let peak = |block: &Block| block.iter().fold(0.0f32, |p, &sample| p.max(sample.abs()));
            let input_peak = stage.meter.as_ref().map(|_| peak(source));
            // Keep the model out of subnormal arithmetic after speech, as in the legacy microphone chain.
            if let Some(dither) = &mut stage.mic_dither {
                dither.apply(source, 1e-9);
            }
            stage.model.process(source, dest);
            dest.iter_mut().for_each(|x| *x = sanitize(*x).clamp(-1.0, 1.0));
            if let Some(meter) = &stage.meter {
                meter.publish(StageReading { input_peak, output_peak: Some(peak(dest)), ..stage.model.reading() });
            }
            std::mem::swap(&mut source, &mut dest);
        }
        self.active ^= self.stages.len() % 2;
    }
}

impl PacketEffect for Voice {
    fn set_meter(&mut self, meter: &StageMeter) -> bool {
        // Construction attaches individual meters before adjacent voice stages share the adapter.
        self.stages[0].meter = Some(meter.clone());
        true
    }

    fn required_sample_rate(&self) -> Option<u32> {
        Some(SAMPLE_RATE)
    }

    fn prepare_thread(&mut self) {
        for stage in &mut self.stages {
            stage.model.prepare_thread();
        }
    }

    fn ready_on_thread(&self) -> bool {
        self.stages.iter().all(|stage| stage.model.ready_on_thread())
    }

    fn append_compatible(&mut self, next: &mut dyn PacketEffect) -> bool {
        if let Some(next) = (next as &mut dyn std::any::Any).downcast_mut::<Self>() {
            self.stages.append(&mut next.stages);
            true
        } else {
            false
        }
    }

    fn process(&mut self, mut frames: &mut [[f32; 2]], key: Option<Input<'_>>) {
        let mut offset = 0;
        while !frames.is_empty() {
            let n = frames.len().min(BLOCK - self.cursor);
            let (packet, rest) = frames.split_at_mut(n);
            let span = self.cursor..self.cursor + n;
            if let Some(key) = key {
                self.stages[0].model.reference(key, offset, span.clone());
            }
            // Drain the previous result and reuse each consumed slot for the next input hop.
            for (stereo, slot) in packet.iter_mut().zip(&mut self.blocks[self.active][span]) {
                let previous = std::mem::replace(slot, 0.5 * (sanitize(stereo[0]) + sanitize(stereo[1])));
                *stereo = [previous; 2];
            }
            self.cursor += n;
            offset += n;
            if self.cursor == BLOCK {
                self.hop();
                self.cursor = 0;
            }
            frames = rest;
        }
    }
}

struct Rnnoise {
    model: RnnoiseModel,
    speech_probability: f32,
}

thread_local! {
    static FFT_READY: Cell<bool> = const { Cell::new(false) };
}

/// Prepare the capture thread before starting a live-editable voice session. The FFT cache is per thread.
pub fn prepare_voice_thread() {
    if !FFT_READY.get() {
        nf_stages::warm_up_fft_thread();
        FFT_READY.set(true);
    }
}

impl VoiceModel for Rnnoise {
    fn prepare_thread(&mut self) {
        prepare_voice_thread();
    }

    fn ready_on_thread(&self) -> bool {
        FFT_READY.get()
    }

    fn process(&mut self, input: &Block, out: &mut Block) {
        self.speech_probability = self.model.process_hop(input, out);
    }

    fn reading(&self) -> StageReading {
        StageReading { speech_probability: Some(self.speech_probability), ..Default::default() }
    }
}

/// RNNoise through `nnnoiseless`, using the same float-to-model scaling as the legacy voice chain.
pub fn noise_reduction() -> Effect {
    Effect::Custom(Box::new(Voice::new(Rnnoise { model: RnnoiseModel::new(), speech_probability: f32::NAN }, true)))
}

struct WebRtc(AudioProcessing);

impl VoiceModel for WebRtc {
    fn process(&mut self, input: &Block, out: &mut Block) {
        // Builder and fixed-size buffers agree on mono at 48 kHz. Preserve one hop of matched dry audio on a library error.
        if self.0.process_capture_f32(&[input], &mut [out]).is_err() {
            out.copy_from_slice(input);
        }
    }
}

/// WebRTC noise suppression through Sonora, using its Moderate preset. AEC, automatic gain and all other APM processing are disabled.
pub fn sonora_noise_reduction() -> Result<Effect, String> {
    let mono48 = StreamConfig::new(SAMPLE_RATE, 1);
    let config = Config {
        pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
        noise_suppression: Some(NoiseSuppression::default()),
        ..Default::default()
    };
    let mut model = AudioProcessing::builder().config(config).capture_config(mono48).render_config(mono48).build();
    if model.proc_sample_rate_hz() != SAMPLE_RATE as usize {
        return Err(format!("WebRTC noise suppression initialized at {} Hz", model.proc_sample_rate_hz()));
    }
    model.process_capture_f32(&[&[0.0; BLOCK]], &mut [&mut [0.0; BLOCK]]).map_err(|e| e.to_string())?;
    Ok(Voice::effect(WebRtc(model)))
}

struct Echo {
    stage: Aec3Stage,
    reference: Block,
}

impl VoiceModel for Echo {
    fn reading(&self) -> StageReading {
        let stats = self.stage.aec_stats();
        StageReading { erle_db: Some(stats.erle_db), echo_delay_ms: Some(stats.echo_delay_ms), ..Default::default() }
    }

    fn reference(&mut self, key: Input<'_>, offset: usize, span: Range<usize>) {
        let end = offset + span.len();
        let out = &mut self.reference[span];
        match key {
            Input::Mono(samples) => out.copy_from_slice(&samples[offset..end]),
            Input::Stereo(samples) => {
                for (r, frame) in out.iter_mut().zip(&samples[offset..end]) {
                    *r = 0.5 * (sanitize(frame[0]) + sanitize(frame[1]));
                }
            }
            Input::Silence(_) => out.fill(0.0),
        }
    }

    fn process(&mut self, input: &Block, out: &mut Block) {
        self.stage.process(ProcessIo::new(&[input, &self.reference], out));
    }
}

/// Cancel speaker echo in a microphone branch with the legacy Sonora AEC3 implementation. `reference` carries what was played
/// on the speakers, aligned to the microphone's sample clock by the host. The mono voice path adds 480 packet-adapter frames
/// plus AEC3's 430-frame stream offset. Both AEC3 inputs use the legacy denormal protection.
pub fn echo_cancellation(reference: Tap) -> Result<Effect, String> {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let mut stage = Aec3Stage::new(controls).map_err(|e| e.to_string())?;
    stage.activate(&Activation::HOST).map_err(|e| e.to_string())?;
    let effect = Voice::new(Echo { stage, reference: [0.0; BLOCK] }, true);
    Ok(Effect::Sidechain { key: reference, effect: Box::new(effect) })
}

struct StereoGate {
    threshold: f32,
    close_threshold: f32,
    floor: f32,
    attack: f32,
    release: f32,
    hold: usize,
    remaining: usize,
    open: bool,
    gain: f32,
    meter: Option<StageMeter>,
}

impl PacketEffect for StereoGate {
    fn ready_on_thread(&self) -> bool {
        true
    }

    fn set_meter(&mut self, meter: &StageMeter) -> bool {
        self.meter = Some(meter.clone());
        true
    }

    fn required_sample_rate(&self) -> Option<u32> {
        Some(SAMPLE_RATE)
    }

    fn process(&mut self, frames: &mut [[f32; 2]], _: Option<Input<'_>>) {
        let peak = |frames: &[[f32; 2]]| frames.iter().fold(0.0f32, |p, frame| p.max(frame[0].abs()).max(frame[1].abs()));
        let input_peak = self.meter.as_ref().map(|_| peak(frames));
        for frame in &mut *frames {
            let level = frame[0].abs().max(frame[1].abs());
            if level >= if self.open { self.close_threshold } else { self.threshold } {
                self.open = true;
                self.remaining = self.hold;
            } else if self.remaining > 0 {
                self.remaining -= 1;
            } else {
                self.open = false;
            }
            self.gain = if self.open { (self.gain + self.attack).min(1.0) } else { (self.gain - self.release).max(self.floor) };
            frame[0] *= self.gain;
            frame[1] *= self.gain;
        }
        if let Some(meter) = &self.meter {
            meter.publish(StageReading {
                input_peak,
                output_peak: Some(peak(frames)),
                gain: Some(self.gain),
                gate_open: Some(self.open),
                ..Default::default()
            });
        }
    }
}

/// Linked, peak-triggered stereo gate. Uses the legacy gate's hold, attack, release and closed level defaults. The same gain
/// applies to left and right, including antiphase material. Accepts a threshold from −120 to 0 dBFS.
pub fn gate(threshold_db: f32) -> Result<Effect, String> {
    if !(-120.0..=0.0).contains(&threshold_db) {
        return Err("Gate threshold must be between -120 and 0 dBFS".into());
    }
    let defaults = GateConfig::default();
    let floor = db_to_lin(defaults.range_db);
    let frames = |ms: f32| (ms * SAMPLE_RATE as f32 / 1000.0).round().max(1.0);
    Ok(Effect::Custom(Box::new(StereoGate {
        threshold: db_to_lin(threshold_db),
        close_threshold: db_to_lin(threshold_db - 3.0),
        floor,
        attack: (1.0 - floor) / frames(defaults.attack_ms),
        release: (1.0 - floor) / frames(defaults.release_ms),
        hold: frames(defaults.hold_ms) as usize,
        remaining: 0,
        open: true,
        gain: 1.0,
        meter: None,
    })))
}

#[cfg(test)]
mod tests {
    use llcable_routing::{Bus, Router, Send};

    use super::*;

    // A sidechain input at the first model, a known one-hop delay (as for the isolated CLAP worker), then gain.
    // This checks signed, distinct samples, packet boundaries and delay even under three-stage framing.
    struct Reference(Block);
    impl VoiceModel for Reference {
        fn reference(&mut self, key: Input<'_>, offset: usize, span: Range<usize>) {
            let Input::Mono(key) = key else { panic!("expected mono key") };
            let n = span.len();
            self.0[span].copy_from_slice(&key[offset..offset + n]);
        }
        fn process(&mut self, input: &Block, out: &mut Block) {
            for ((out, x), reference) in out.iter_mut().zip(input).zip(&self.0) {
                *out = x + reference;
            }
        }
    }

    struct Delay(Block);
    impl VoiceModel for Delay {
        fn process(&mut self, input: &Block, out: &mut Block) {
            out.copy_from_slice(&self.0);
            self.0.copy_from_slice(input);
        }
    }

    struct Half;
    impl VoiceModel for Half {
        fn process(&mut self, input: &Block, out: &mut Block) {
            out.iter_mut().zip(input).for_each(|(y, &x)| *y = x * 0.5);
        }
    }

    #[test]
    fn adjacent_voice_models_share_framing_and_keep_intrinsic_delay_and_sidechain() {
        for later_reference in [false, true] {
            let mut effects = vec![
                Effect::Sidechain { key: Tap::Input(1), effect: Box::new(Voice::new(Reference([0.0; BLOCK]), false)) },
                Voice::effect(Delay([0.0; BLOCK])),
                Voice::effect(Half),
            ];
            if later_reference {
                // An explicitly keyed second stage must see the current key, even with earlier stages buffering its main input.
                effects.push(Effect::Sidechain { key: Tap::Input(1), effect: Box::new(Voice::new(Reference([0.0; BLOCK]), false)) });
            }
            let bus = Bus { sends: vec![Send::unity(Tap::Input(0))], effects };
            let mut router = Router::new(SAMPLE_RATE, 113, 2, vec![bus]).unwrap();
            let mic: Vec<_> = (0..3000).map(|i| (i % 79 - 40) as f32 / 256.0).collect();
            let key: Vec<_> = (0..3000).map(|i| (i % 61 - 27) as f32 / 256.0).collect();
            let mut i = 0;
            for (signal, reference) in mic.chunks(113).zip(key.chunks(113)) {
                router.process(&[Input::Mono(signal), Input::Mono(reference)]).unwrap();
                for &frame in router.output(0).unwrap() {
                    let delay = if later_reference { 3 * BLOCK } else { 2 * BLOCK };
                    let expected = if i >= delay { 0.5 * (mic[i - delay] + key[i - delay]) } else { 0.0 };
                    let expected = if later_reference && i >= BLOCK { expected + key[i - BLOCK] } else { expected };
                    assert_eq!(frame, [expected; 2], "sample {i}");
                    i += 1;
                }
            }
        }
    }
}
