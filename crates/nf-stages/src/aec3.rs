//! WebRTC AEC3 through sonora 0.2.0, at 48 kHz mono. Facts below cite
//! doc/dev/api/sonora-nnnoiseless.md, Part 1.
//!
//! - **Config (1.4):** AEC3 on, sonora's noise suppressor and AGC off, and the high-pass on in
//!   full band. `MaxProcessingRate::Rate48kHz` matters: the default (32 kHz) caps the capture path
//!   at 16 kHz, and at 48 kHz I/O it allocates a full-band copy every frame.
//! - **Per block (1.5):** render (the reference) first, then capture (the mic), 1:1. Both inputs are
//!   sanitised first: sonora's `f32::clamp` to ±1 lets a NaN through, and it would poison the filters.
//! - **Denormals (1.10):** the reference is dithered at 1e-9 (about -180 dBFS) on every block, gaps
//!   included. A silent reference otherwise decays AEC3's filter states into subnormals (sonora
//!   #34): on the dev box an undithered zero reference costs 690 µs a block against 87 µs dithered
//!   (`tests/timing.rs`). The engine dithers the mic.
//! - **Latency:** [`AEC3_LATENCY`], measured by the `stream_offset_is_aec3_latency` test. On voiced
//!   speech the high-pass delays the strong low harmonics, which pulls a plain cross-correlation
//!   about 30 samples late; GCC-PHAT, or an impulse, finds the stream offset.
//! - **Allocation (1.3, 1.7):** `new` builds the whole AEC3 (about 300 KB in hundreds of blocks):
//!   build it before real-time. `process` allocates about 43 small `Vec`s per block inside sonora
//!   0.2.0 (upstream PR #40 removes them); under the engine they come from the real-time pool.
//! - **Reset:** sonora has no reset, and rebuilding allocates as much as `new`, which the audio
//!   thread mustn't do (1.6). `reset` runs one dithered-silent render and capture pair through AEC3
//!   and discards the output. That flushes the mic audio AEC3 holds from before the stage stopped
//!   (its framing and band-split delay, [`AEC3_LATENCY`]), and keeps the converged echo path and
//!   delay estimate, so AEC3 doesn't reconverge after a latency test. A reset with nothing processed
//!   since the last one does nothing. This is our proposal; a clean start would mean building a new
//!   stage off the audio thread.

use std::sync::Arc;

use nf_core::controls::ChainControls;
use nf_core::rt::Dither;
use nf_core::stage::{ProcessIo, Stage, StageError, StageInfo, StageReport, StageRole, StageStats};
use nf_core::{BLOCK, Block, SAMPLE_RATE};
use sonora::config::{EchoCanceller, HighPassFilter as HpfConfig, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::{check_format, sanitize};

/// The stream offset, 430 samples (8.96 ms), measured with an impulse and a silent reference by the
/// `stream_offset_is_aec3_latency` test. It is AEC3's capture path (block framing and band
/// handling): the same with the high-pass off, and 0 with AEC3 off. sonora-nnnoiseless.md 1.8's
/// estimate from the code, about 4.5 ms, is too low.
pub const AEC3_LATENCY: u32 = 430;
/// The reference dither's amplitude: about -180 dBFS.
const REFERENCE_DITHER: f32 = 1e-9;

const INFO: StageInfo = StageInfo {
    id: "aec3",
    name: "WebRTC AEC3",
    role: StageRole::EchoCancel,
    needs_reference: true,
    needs_worker: false,
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
    // need no run-time overlap checks (see RnnoiseStage).
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
            dither: Dither::new(0x5EED_AEC3),
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

/// `src` with non-finite samples zeroed.
#[inline]
fn sanitized(src: &Block, dst: &mut Block) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = sanitize(s);
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
    fn info(&self) -> StageInfo {
        INFO
    }

    fn activate(&mut self, sample_rate: u32, block: usize) -> Result<(), StageError> {
        check_format(INFO.name, sample_rate, block)
    }

    fn latency(&self) -> u32 {
        AEC3_LATENCY
    }

    fn process(&mut self, io: ProcessIo<'_>) -> StageReport {
        self.dirty = true;
        match io.reference {
            Some(r) => sanitized(r, &mut self.reference),
            None => self.reference.fill(0.0),
        }
        self.dither.apply(&mut self.reference[..], REFERENCE_DITHER);
        sanitized(io.input, &mut self.mic);
        self.run(io.output);
        StageReport::default()
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

    /// AEC3's fullband ERLE (its linear filter's, so it understates the output's echo reduction) and
    /// its echo-delay estimate (4 ms steps; one step under the echo path's bulk delay in our tests).
    fn stats(&self) -> StageStats {
        let s = self.apm.statistics();
        StageStats {
            echo_delay_ms: s.delay_ms.map_or(f32::NAN, |d| d as f32),
            erle_db: s.echo_return_loss_enhancement.map_or(f32::NAN, |e| e as f32),
        }
    }
}
