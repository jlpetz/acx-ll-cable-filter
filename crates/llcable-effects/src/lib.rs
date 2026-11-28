//! Optional Rust voice effects for a 48 kHz [`llcable_routing::Router`]. Consecutive mono voice stages share one 480-frame
//! packet adapter. Each model retains its own algorithmic delay. A sidechain on a later stage starts a new group, keeping its
//! reference aligned to the packet supplied by the router. Intermediate taps read the same hop and phase as the group's stereo
//! output. An empty branch bypasses both the model and that buffering. The linked stereo gate can operate on a single frame with no lookahead.
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
    // Retain the clean result until the next hop. A mid-hop subscriber immediately sees the same phase as the final output.
    output: Block,
}

struct Voice {
    stages: Vec<VoiceStage>,
    active_stages: usize,
    input: Block,
    cursor: usize,
}

impl Voice {
    fn new(model: impl VoiceModel + 'static, mic_dither: bool) -> Self {
        let stage = VoiceStage {
            model: Box::new(model),
            mic_dither: mic_dither.then(|| Dither::new(0x2545_F491)),
            meter: None,
            output: [0.; BLOCK],
        };
        Self { stages: vec![stage], active_stages: 1, input: [0.0; BLOCK], cursor: 0 }
    }

    fn effect(model: impl VoiceModel + 'static) -> Effect {
        Effect::Custom(Box::new(Self::new(model, false)))
    }

    fn hop(&mut self) {
        let mut previous: Option<&Block> = None;
        for stage in &mut self.stages[..self.active_stages] {
            let peak = |block: &Block| block.iter().fold(0.0f32, |p, &sample| p.max(sample.abs()));
            let input_peak = stage.meter.as_ref().map(|_| peak(previous.unwrap_or(&self.input)));
            let source = if let Some(dither) = &mut stage.mic_dither {
                // Reuse the consumed input as scratch. The next model's denormal protection must not change an earlier tap.
                if let Some(previous) = previous {
                    self.input.copy_from_slice(previous);
                }
                dither.apply(&mut self.input, 1e-9);
                &self.input
            } else {
                previous.unwrap_or(&self.input)
            };
            stage.model.process(source, &mut stage.output);
            stage.output.iter_mut().for_each(|x| *x = sanitize(*x).clamp(-1.0, 1.0));
            if let Some(meter) = &stage.meter {
                meter.publish(StageReading { input_peak, output_peak: Some(peak(&stage.output)), ..stage.model.reading() });
            }
            previous = Some(&stage.output);
        }
    }

    fn process_packet(
        &mut self, mut frames: &mut [[f32; 2]], key: Option<Input<'_>>, mut emit: impl FnMut(&[VoiceStage], usize, Range<usize>),
    ) {
        let mut offset = 0;
        while !frames.is_empty() {
            let n = frames.len().min(BLOCK - self.cursor);
            let (packet, rest) = frames.split_at_mut(n);
            let span = self.cursor..self.cursor + n;
            if let Some(key) = key {
                self.stages[0].model.reference(key, offset, span.clone());
            }
            let (last, intermediates) = self.stages[..self.active_stages].split_last().expect("a voice group has an active stage");
            emit(intermediates, offset, span.clone());
            for ((stereo, input), &output) in packet.iter_mut().zip(&mut self.input[span.clone()]).zip(&last.output[span]) {
                *input = 0.5 * (sanitize(stereo[0]) + sanitize(stereo[1]));
                *stereo = [output; 2];
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
            self.active_stages = self.stages.len();
            true
        } else {
            false
        }
    }

    fn process(&mut self, frames: &mut [[f32; 2]], key: Option<Input<'_>>) {
        self.process_packet(frames, key, |_, _, _| {});
    }

    fn supports_stage_taps(&self) -> bool {
        true
    }

    fn limit_stages(&mut self, active: usize) {
        assert!((1..=self.active_stages).contains(&active), "only an active voice prefix can survive a live edit");
        // The old suffix stays allocated until the group is dropped off the audio thread.
        self.active_stages = active;
    }

    fn process_tapped(&mut self, frames: &mut [[f32; 2]], key: Option<Input<'_>>, output: &mut dyn FnMut(usize, usize, &[f32])) {
        self.process_packet(frames, key, |stages, offset, span| {
            for (i, stage) in stages.iter().enumerate() {
                output(i, offset, &stage.output[span.clone()]);
            }
        });
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
    fn mid_hop_taps_and_prefix_retirement_preserve_samples_dither_phase_and_meters() {
        let sizes = [313, 200, 1100, 129, 789, 17, 333, 849];
        let length: usize = sizes.iter().sum();
        let mic: Vec<_> = (0..length).map(|i| (i as i32 % 71 - 40) as f32 * 1e-9).collect();
        let key: Vec<_> = (0..length).map(|i| (i as i32 % 29 - 13) as f32 * 1e-9).collect();

        // Each stage has its own dither generator. The prefix tap contains only stage one's dither, including after
        // the downstream stage has consumed the same hop. Index the expected traces by *delivery* frame.
        let mut generator1 = Dither::new(0x2545_F491);
        let mut generator2 = generator1.clone();
        let mut prefix = vec![0.0; length + 2 * BLOCK];
        let mut delayed = prefix.clone();
        for (block, mut hop) in mic.as_chunks::<BLOCK>().0.iter().copied().enumerate() {
            let first = block * BLOCK;
            generator1.apply(&mut hop, 1e-9);
            for (sample, reference) in hop.iter_mut().zip(&key[first..]) {
                *sample += reference;
            }
            prefix[first + BLOCK..first + 2 * BLOCK].copy_from_slice(&hop);
            generator2.apply(&mut hop, 1e-9);
            delayed[first + 2 * BLOCK..first + 3 * BLOCK].copy_from_slice(&hop);
        }

        let mut fused = Voice::new(Reference([0.; BLOCK]), true);
        let mut delay = Voice::new(Delay([0.; BLOCK]), true);
        let mut last = Voice::new(Half, false);
        let meter = StageMeter::default();
        let tail_meter = StageMeter::default();
        fused.set_meter(&meter);
        last.set_meter(&tail_meter);
        assert!(fused.append_compatible(&mut delay));
        assert!(fused.append_compatible(&mut last));
        let mut first = 0;
        let mut frozen = None;
        for (index, n) in sizes.into_iter().enumerate() {
            let mut packet: Vec<_> = mic[first..first + n].iter().map(|&x| [x; 2]).collect();
            let reference = Some(Input::Mono(&key[first..first + n]));
            if index == 5 {
                assert_ne!(first % BLOCK, 0);
                fused.limit_stages(1);
                frozen = Some(tail_meter.read());
            }
            if index == 2 || index == 4 {
                // The first subscription starts 33 frames into an already computed hop; a single packet can cross multiple hops.
                let mut next_offset = [0; 2];
                fused.process_tapped(&mut packet, reference, &mut |stage, offset, samples| {
                    assert_eq!(offset, next_offset[stage]);
                    let start = first + offset;
                    let expected = if stage == 0 { &prefix } else { &delayed };
                    assert_eq!(samples, &expected[start..start + samples.len()]);
                    next_offset[stage] += samples.len();
                });
                assert_eq!(next_offset, [n; 2]);
            } else {
                fused.process(&mut packet, reference);
            }
            for (offset, frame) in packet.iter().enumerate() {
                let i = first + offset;
                assert_eq!(*frame, [if index < 5 { delayed[i] * 0.5 } else { prefix[i] }; 2], "frame {i}");
            }
            first += n;
            if first >= BLOCK {
                let start = first / BLOCK * BLOCK;
                let expected_peak = prefix[start..start + BLOCK].iter().fold(0.0f32, |p, s| p.max(s.abs()));
                assert_eq!(meter.read().output_peak, Some(expected_peak));
            }
            if let Some(reading) = frozen {
                assert_eq!(tail_meter.read(), reading);
            }
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
