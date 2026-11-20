//! WebRTC AEC3 through sonora 0.3.0, at 48 kHz mono. Facts below cite
//! doc/dev/api/sonora-nnnoiseless.md, Part 1 (written for 0.2.0; its 1.11 says what 0.3.0 changed).
//!
//! - **Config (1.4):** AEC3 on, sonora's noise suppressor and AGC off, and the high-pass on in
//!   full band. `MaxProcessingRate::Rate48kHz` matters: the default (32 kHz) caps the capture path
//!   at 16 kHz, and at 48 kHz I/O it allocates a full-band copy every frame.
//! - **Ports (TODO 17):** the mic on the main input, the speaker reference on input 1 (non-main, CLAP's sidechain), the
//!   echo-cancelled mic on the output. A host that connects no reference gets a silent one.
//! - **Per block (1.5):** render (the reference) first, then capture (the mic), 1:1. Both inputs are
//!   sanitised first (`nf_core::dsp::sanitize_into`): sonora's `f32::clamp` to ±1 lets a NaN through, and it would
//!   poison the filters.
//! - **Denormals (1.10, 1.11):** the reference is dithered at 1e-9 (about -180 dBFS) on every block, gaps
//!   included. A silent reference otherwise decays AEC3's filter states into subnormals (sonora
//!   #34): with sonora 0.2.0 an undithered zero reference cost 674 µs a block on the dev box against
//!   45 µs dithered (`tests/timing.rs`). The engine dithers the mic. sonora 0.3.0 sets FTZ and DAZ for each call
//!   and restores MXCSR on return, which takes the undithered row to 46 µs; the vendored 0.3.0 does that only with
//!   nf-stages' `sonora-denormal-guard` feature, off by default (TODO 10, vendor/README.md). `tests/mxcsr.rs` checks
//!   MXCSR inside and after sonora's calls in both builds.
//! - **Latency:** [`AEC3_LATENCY`], measured by the `stream_offset_is_aec3_latency` test. On voiced
//!   speech the high-pass delays the strong low harmonics, which pulls a plain cross-correlation
//!   about 30 samples late; GCC-PHAT, or an impulse, finds the stream offset.
//! - **Allocation (1.3, 1.7):** `new` builds the whole AEC3 (about 300 KB in hundreds of blocks):
//!   build it before real-time. `process` then allocates nothing (sonora 0.3.0, its PR #40; 0.2.0
//!   made about 43 small `Vec`s a block), from the first block on (`tests/rt_no_alloc.rs`).
//! - **Reset:** sonora has no reset, and rebuilding allocates as much as `new`, which the audio
//!   thread mustn't do (1.6). `reset` runs one dithered-silent render and capture pair through AEC3
//!   and discards the output. That flushes the mic audio AEC3 holds from before the stage stopped
//!   (its framing and band-split delay, [`AEC3_LATENCY`]), and keeps the converged echo path and
//!   delay estimate, so AEC3 doesn't reconverge after a latency test. A reset with nothing processed
//!   since the last one does nothing. This is our proposal; a clean start would mean building a new
//!   stage off the audio thread.
//! - **Telemetry:** `noise-filter.aec-telemetry/0` ([`AecTelemetry`]). sonora's `statistics()` is a cached reference, so
//!   reading it per block allocates nothing.

use std::borrow::Cow;
use std::sync::Arc;

use nf_core::controls::ChainControls;
use nf_core::dsp::sanitize_into;
use nf_core::rt::Dither;
use nf_core::stage::{
    Activation, AecStats, AecTelemetry, AudioPorts, MONO_WITH_REFERENCE, ProcessIo, Stage, StageDescriptor, StageError, VENDOR, features,
    text,
};
use nf_core::{BLOCK, Block, SAMPLE_RATE};
use sonora::config::{EchoCanceller, HighPassFilter as HpfConfig, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

/// The stream offset, 430 samples (8.96 ms), measured with an impulse and a silent reference by the
/// `stream_offset_is_aec3_latency` test. It is AEC3's capture path (block framing and band
/// handling): the same with the high-pass off, and 0 with AEC3 off. sonora-nnnoiseless.md 1.8's
/// estimate from the code, about 4.5 ms, is too low.
pub const AEC3_LATENCY: u32 = 430;
/// The reference dither's amplitude: about -180 dBFS.
const REFERENCE_DITHER: f32 = 1e-9;
/// The reference dither's seed.
const DITHER_SEED: u32 = 0x5EED_AEC3;

/// What the AEC3 stage is.
pub static AEC3_DESCRIPTOR: StageDescriptor = StageDescriptor {
    id: text(nf_core::namespaced!(".", "echo-cancel")),
    name: text("Echo canceller (WebRTC AEC3)"),
    vendor: VENDOR,
    url: text(""),
    version: text(env!("CARGO_PKG_VERSION")),
    description: text("WebRTC's AEC3 echo canceller (sonora), with the speaker reference on its second input"),
    features: Cow::Borrowed(&[features::AUDIO_EFFECT, features::RESTORATION, features::MONO, features::ECHO_CANCEL]),
};

/// WebRTC AEC3 echo-cancel stage.
pub struct Aec3Stage {
    /// Held so AEC controls can be read per block; v0 has none beyond the chain's on/off.
    _controls: Arc<ChainControls>,
    apm: AudioProcessing,
    dither: Dither,
    /// Something was processed since `new` or the last `reset`.
    dirty: bool,
    // Blocks inline, and sanitised by a kernel taking each block as its own argument, so the loops
    // need no run-time overlap checks (as in nf-core's noise stage).
    /// This block's mic and reference, sanitised (and the reference dithered).
    mic: Block,
    reference: Block,
    /// Render output: a copy of the reference that nothing reads.
    scratch: Block,
}

impl Aec3Stage {
    /// Builds AEC3 (allocates heavily: do it before real-time).
    pub fn new(controls: Arc<ChainControls>) -> Result<Self, StageError> {
        let apm = build_apm();
        let rate = apm.proc_sample_rate_hz();
        if rate != SAMPLE_RATE as usize {
            return Err(StageError(format!("AEC3 came up at {rate} Hz internally, not {SAMPLE_RATE}")));
        }
        Ok(Self {
            _controls: controls,
            apm,
            dither: Dither::new(DITHER_SEED),
            dirty: false,
            mic: [0.0; BLOCK],
            reference: [0.0; BLOCK],
            scratch: [0.0; BLOCK],
        })
    }

    /// One render then one capture call: `self.reference` and `self.mic` must be filled.
    #[inline]
    fn run(&mut self, output: &mut Block) {
        // Both calls only fail on a format mismatch, which the fixed 48 kHz mono configs rule out.
        let r = self.apm.process_render_f32(&[&self.reference[..]], &mut [&mut self.scratch[..]]);
        debug_assert!(r.is_ok());
        let r = self.apm.process_capture_f32(&[&self.mic[..]], &mut [&mut output[..]]);
        debug_assert!(r.is_ok());
    }
}

/// 48 kHz mono, AEC3 on, NS and AGC off, high-pass on in full band (1.4).
pub(crate) fn build_apm() -> AudioProcessing {
    let config = Config {
        pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Pipeline::default() },
        echo_canceller: Some(EchoCanceller { enforce_high_pass_filtering: true, ..EchoCanceller::default() }),
        high_pass_filter: Some(HpfConfig { apply_in_full_band: true }),
        noise_suppression: None,
        gain_controller2: None,
        ..Config::default()
    };
    let mono48 = StreamConfig::new(SAMPLE_RATE, 1);
    AudioProcessing::builder().config(config).capture_config(mono48).render_config(mono48).build()
}

impl Stage for Aec3Stage {
    fn descriptor(&self) -> &StageDescriptor {
        &AEC3_DESCRIPTOR
    }

    fn audio_ports(&self) -> &AudioPorts {
        &MONO_WITH_REFERENCE
    }

    fn activate(&mut self, activation: &Activation) -> Result<(), StageError> {
        activation.require_host(&AEC3_DESCRIPTOR.name)
    }

    fn latency(&self) -> u32 {
        AEC3_LATENCY
    }

    fn process(&mut self, io: ProcessIo<'_>) {
        self.dirty = true;
        match io.aux(1) {
            Some(r) => sanitize_into(r, &mut self.reference),
            None => self.reference.fill(0.0),
        }
        self.dither.apply(&mut self.reference[..], REFERENCE_DITHER);
        sanitize_into(io.main(), &mut self.mic);
        self.run(io.output);
    }

    fn reset(&mut self) {
        if !self.dirty {
            return; // holds no audio yet
        }
        self.dirty = false;
        self.reference.fill(0.0);
        self.dither.apply(&mut self.reference[..], REFERENCE_DITHER);
        self.mic.fill(0.0);
        let mut discard = [0.0f32; BLOCK];
        self.run(&mut discard);
    }

    fn aec_telemetry(&self) -> Option<&dyn AecTelemetry> {
        Some(self)
    }
}

impl AecTelemetry for Aec3Stage {
    /// AEC3's fullband ERLE (its linear filter's, so it understates the output's echo reduction) and
    /// its echo-delay estimate (4 ms steps; one step under the echo path's bulk delay in our tests).
    fn aec_stats(&self) -> AecStats {
        let s = self.apm.statistics();
        AecStats {
            echo_delay_ms: s.delay_ms.map_or(f32::NAN, |d| d as f32),
            erle_db: s.echo_return_loss_enhancement.map_or(f32::NAN, |e| e as f32),
        }
    }
}

#[cfg(test)]
mod tests {
    use nf_core::config::ProcessingConfig;
    use nf_core::stage::{StageRole, check_host_profile};

    use super::*;
    use crate::synth::{Voice, add, convolve, delayed, room_impulse_response, scaled, speech_like, white_noise};

    /// The stage's sanitising, written out: non-finite to 0, the rest clamped to ±INPUT_LIMIT.
    fn sanitized(x: &Block) -> Block {
        let limit = nf_core::dsp::INPUT_LIMIT;
        x.map(|v| if v.is_finite() { v.clamp(-limit, limit) } else { 0.0 })
    }

    /// The stage is sonora driven directly: the same config, the reference sanitised and dithered with the same seed, the
    /// mic sanitised, render then capture. Bit for bit, through NaNs, infinities and blocks without a reference port.
    #[test]
    fn the_stage_is_sonora_driven_directly() {
        let len = 4 * 48_000;
        let mut far = speech_like(len, Voice::LOW, 7).samples;
        let echo = scaled(&delayed(&convolve(&far, &room_impulse_response(4800, 0.15, 8))[..len], 1920), 0.5);
        let mut mic = add(&add(&echo, &speech_like(len, Voice::HIGH.at(-30.0), 9).samples), &white_noise(len, 1e-3, 10));
        for i in (1000..len - 20).step_by(4801) {
            mic[i] = f32::NAN;
            far[i + 3] = f32::INFINITY;
            mic[i + 9] = f32::NEG_INFINITY;
        }
        let mut stage = Aec3Stage::new(Arc::new(ChainControls::new(&ProcessingConfig::default()))).expect("stage");
        stage.activate(&Activation::HOST).expect("activate");
        let mut apm = build_apm();
        let mut dither = Dither::new(DITHER_SEED);
        let (mut scratch, mut direct, mut y) = ([0.0f32; BLOCK], [0.0f32; BLOCK], [0.0f32; BLOCK]);
        for (b, (mi, ri)) in mic.as_chunks::<BLOCK>().0.iter().zip(far.as_chunks::<BLOCK>().0).enumerate() {
            // Now and then no reference port at all: that reads as a silent reference.
            let mut r = if b % 50 == 7 {
                stage.process(ProcessIo::new(&[mi], &mut y));
                [0.0; BLOCK]
            } else {
                stage.process(ProcessIo::new(&[mi, ri], &mut y));
                sanitized(ri)
            };
            dither.apply(&mut r, REFERENCE_DITHER);
            apm.process_render_f32(&[&r[..]], &mut [&mut scratch[..]]).expect("render");
            apm.process_capture_f32(&[&sanitized(mi)[..]], &mut [&mut direct[..]]).expect("capture");
            let first = y.iter().zip(&direct).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "block {b}: the stage differs from sonora");
        }
        assert!(stage.aec_stats().erle_db.is_finite(), "AEC3 converged enough to report");
    }

    /// AEC3 through TODO 2's reusable check (`harness::check_stage`): the profile, its 430 at every block phase (impulses at
    /// 0.5 on a silent reference, GCC-PHAT on speech), finite through the hostile blocks on both inputs and the mic coming
    /// through after them, and bit-equal to sonora driven directly (the same config and dither seed) on 4 s of echo.
    #[test]
    fn aec3_passes_the_stage_check() {
        use crate::harness::{ParityCase, StageCheck, Tolerance, check_stage};
        let len = 4 * 48_000;
        let far = speech_like(len, Voice::LOW, 7).samples;
        let echo = scaled(&delayed(&convolve(&far, &room_impulse_response(4800, 0.15, 8))[..len], 1920), 0.5);
        let mic = add(&add(&echo, &speech_like(len, Voice::HIGH.at(-30.0), 9).samples), &white_noise(len, 1e-3, 10));
        let mut apm = build_apm();
        let mut dither = Dither::new(DITHER_SEED);
        let (mut scratch, mut out) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        let mut direct = Vec::with_capacity(len);
        for (mi, ri) in mic.as_chunks::<BLOCK>().0.iter().zip(far.as_chunks::<BLOCK>().0) {
            let mut r = sanitized(ri);
            dither.apply(&mut r, REFERENCE_DITHER);
            apm.process_render_f32(&[&r[..]], &mut [&mut scratch[..]]).expect("render");
            apm.process_capture_f32(&[&sanitized(mi)[..]], &mut [&mut out[..]]).expect("capture");
            direct.extend_from_slice(&out);
        }
        let parity = ParityCase {
            against: "sonora driven directly",
            input: &mic,
            aux: Some(&far),
            expected: &direct,
            shift: 0,
            tolerance: Tolerance::Exact,
        };
        let make =
            || -> Box<dyn Stage> { Box::new(Aec3Stage::new(Arc::new(ChainControls::new(&ProcessingConfig::default()))).expect("stage")) };
        let r = check_stage(&make, &StageCheck { impulse_amp: Some(0.5), parity: Some(parity) }).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((r.role, r.latency.declared, r.phases.len()), (StageRole::EchoCancel, AEC3_LATENCY, 4));
        assert_eq!(r.parity.map(|p| (p.compared, p.first_mismatch)), Some((len, None)));
    }

    #[test]
    fn it_fits_the_host_profile_as_the_echo_canceller() {
        let s = Aec3Stage::new(Arc::new(ChainControls::new(&ProcessingConfig::default()))).expect("stage");
        assert_eq!(check_host_profile(&s), Ok(StageRole::EchoCancel));
        assert_eq!(s.descriptor().id, "noise-filter.echo-cancel");
        assert_eq!(s.audio_ports().inputs[1].name, "Speaker reference");
        assert!(s.aec_telemetry().is_some() && s.speech_probability().is_none() && s.gain_adjustment().is_none());
        assert!(s.params().is_empty());
    }
}
