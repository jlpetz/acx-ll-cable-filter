//! RNNoise through nnnoiseless 0.5.2, as a model inside nf-core's noise stage (`nf_core::noise::NoiseStage`), which
//! sanitises its input, clamps the model's output and runs the floor mix and the gate after it ([`rnnoise_stage`]).
//! Facts below cite doc/dev/api/sonora-nnnoiseless.md, Part 2.
//!
//! - **Scale (2.2):** nnnoiseless works in i16 units. The model scales its (sanitised, ±1) input ×32768 and its output
//!   ÷32768; the noise stage clamps that to ±1 (nnnoiseless doesn't). Fed ±1 directly, every frame quieter than about
//!   -14 dBFS would count as silence and pass unfiltered.
//! - **Non-finite and huge input (2.2):** a NaN reaches an `unwrap_unchecked` inside easyfft (undefined behaviour: an
//!   abort in debug). So does a finite sample of about 1e17 or more, which the ×32768 overflows to inf. The noise stage
//!   zeros NaN and ±inf and clamps the rest to ±`nf_core::dsp::INPUT_LIMIT` (1e4) before the model and in the dry path.
//! - **Latency (2.3):** output block k is the overlap-add of input block k-1: a hop of 480 and a window of 960, so
//!   `latency_samples` 960 and a stream offset of one block, [`RNNOISE_LATENCY`]. The hop is the block, so the noise stage
//!   runs no re-blocker around it, and `NoisePost` delays the dry path by the same 480.
//! - **Allocation (2.4):** `new` and `prepare` allocate; `process_hop` doesn't. easyfft builds its FFT planner and scratch
//!   per thread on that thread's first FFT, and leaks them. `prepare` (the stage's `activate`) therefore runs two frames
//!   through a throwaway state on the *calling* thread, and the engine activates on the audio thread. Activated on another
//!   thread, the first block allocates. The engine also calls [`crate::warm_up_fft_thread`] before it enters its real-time
//!   scope, so the leaked state lives on the system heap rather than the locked pool, and each start gives the pool back in
//!   full (review RT-1, `tests/rt_pool.rs`).
//! - **Reset:** nnnoiseless has none, and a fresh state costs ten allocations (`DenoiseState::new`). So a pristine spare is
//!   kept, built off the audio path. `reset` swaps it in: no allocation, no free. The used state stays parked in the
//!   spare's slot until `prepare` rebuilds the spare. A second `reset` before then replaces the state in place: ten
//!   allocations and frees on the audio thread, the one exception to its no-allocation rule (`NoiseModel::reset`), which
//!   the engine's real-time pool serves (each passthrough end and each noise toggle after the first, until the next
//!   start). The noise stage resets the model only when it processed something since its last reset, so the chain's
//!   resets don't spend the spare. Feeding silence instead isn't a reset: RNNoise's input high-pass (pole radius
//!   0.998) rings on from old audio for tens of ms, and silent frames don't touch the RNN state at all.

use std::sync::Arc;

use nf_core::chain::MAX_STAGE_LATENCY;
use nf_core::controls::ChainControls;
use nf_core::noise::{ModelInfo, NoiseModel, NoiseStage};
use nf_core::stage::{StageError, text};
use nf_core::{BLOCK, Block};
use nnnoiseless::DenoiseState;

/// The stream offset: one block (see the module notes).
pub const RNNOISE_LATENCY: u32 = 480;
/// RNNoise as the noise stage sees it: a 480 hop, a 960 window, no lookahead, and a VAD.
pub static RNNOISE_INFO: ModelInfo =
    ModelInfo { id: text("rnnoise"), name: text("RNNoise"), hop: BLOCK, latency_samples: 960, has_vad: true };
/// i16 full scale, nnnoiseless's unit.
const S16: f32 = 32768.0;
const INV_S16: f32 = 1.0 / S16;

const _: () = assert!(DenoiseState::FRAME_SIZE == BLOCK);
const _: () = assert!(RNNOISE_LATENCY as usize == BLOCK && BLOCK <= MAX_STAGE_LATENCY);
const _: () = assert!(RNNOISE_INFO.model_offset() == RNNOISE_LATENCY as usize);
const _: () = assert!(RNNOISE_INFO.stage_latency() == RNNOISE_LATENCY as usize);

/// The v0 noise stage: RNNoise, then the floor and the gate, reading `controls.noise` every block. Allocates (build it
/// before real-time); `activate` it on the thread that will run it.
pub fn rnnoise_stage(controls: Arc<ChainControls>) -> Result<NoiseStage, StageError> {
    NoiseStage::new(controls, Box::new(RnnoiseModel::new()))
}

/// RNNoise as a [`NoiseModel`].
pub struct RnnoiseModel {
    state: Box<DenoiseState<'static>>,
    /// Pristine while `spare_fresh`; otherwise the state the last `reset` retired.
    spare: Box<DenoiseState<'static>>,
    spare_fresh: bool,
    // Blocks inline rather than boxed, and the loops in kernels taking each block as its own argument: then LLVM knows
    // nothing overlaps and vectorises without run-time overlap checks.
    /// The hop's input in i16 units.
    rnn_in: Block,
    /// The model's output in i16 units: last hop's input, denoised.
    rnn_out: Block,
}

impl RnnoiseModel {
    /// Two model states, the live one and the spare (allocates).
    pub fn new() -> Self {
        Self { state: DenoiseState::new(), spare: DenoiseState::new(), spare_fresh: true, rnn_in: [0.0; BLOCK], rnn_out: [0.0; BLOCK] }
    }
}

impl Default for RnnoiseModel {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseModel for RnnoiseModel {
    fn info(&self) -> &ModelInfo {
        &RNNOISE_INFO
    }

    /// Rebuilds a used spare and warms up easyfft on the calling thread (see the module notes).
    fn prepare(&mut self) -> Result<(), StageError> {
        if !self.spare_fresh {
            self.spare = DenoiseState::new();
            self.spare_fresh = true;
        }
        warm_up_thread();
        Ok(())
    }

    fn reset(&mut self) {
        if self.spare_fresh {
            std::mem::swap(&mut self.state, &mut self.spare);
            self.spare_fresh = false;
        } else {
            self.state = DenoiseState::new();
        }
    }

    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let (Some(input), Some(output)) = (input.first_chunk::<BLOCK>(), output.first_chunk_mut::<BLOCK>()) else {
            return f32::NAN; // never: the stage passes whole hops of 480
        };
        to_model(input, &mut self.rnn_in);
        // The VAD comes from the window [k-1 | k], which is what this call's output covers.
        let vad = self.state.process_frame(&mut self.rnn_out[..], &self.rnn_in[..]);
        from_model(&self.rnn_out, output);
        vad
    }
}

/// The (sanitised) hop in i16 units.
#[inline]
fn to_model(input: &Block, scaled: &mut Block) {
    for (s, &x) in scaled.iter_mut().zip(input) {
        *s = x * S16;
    }
}

/// The model's output back to ±1 (the noise stage clamps it).
#[inline]
fn from_model(y: &Block, out: &mut Block) {
    for (o, &v) in out.iter_mut().zip(y) {
        *o = v * INV_S16;
    }
}

/// Makes easyfft build this thread's FFT state now, through a throwaway model state so the live one stays pristine. One
/// silent frame runs every FFT; the tone frame also runs the RNN and the pitch filter, in case a later version allocates
/// there.
pub(crate) fn warm_up_thread() {
    let mut scratch = DenoiseState::new();
    let mut out = [0.0f32; BLOCK];
    scratch.process_frame(&mut out, &[0.0; BLOCK]);
    let tone: Block = std::array::from_fn(|i| 3000.0 * (i as f32 * 0.07).sin());
    scratch.process_frame(&mut out, &tone);
}

#[cfg(test)]
mod tests {
    use nf_core::config::ProcessingConfig;
    use nf_core::stage::{Activation, ProcessIo, Stage, StageRole, check_host_profile};

    use super::*;

    fn stage() -> NoiseStage {
        let cfg = ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() };
        let mut s = rnnoise_stage(Arc::new(ChainControls::new(&cfg))).expect("stage");
        s.activate(&Activation::HOST).expect("activate");
        s
    }

    fn run(s: &mut NoiseStage, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        let mut y = [0.0f32; BLOCK];
        for x in input.as_chunks::<BLOCK>().0 {
            s.process(ProcessIo::new(&[x], &mut y));
            out.extend_from_slice(&y);
        }
        out
    }

    fn run_model(m: &mut RnnoiseModel, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; input.len()];
        for (x, y) in input.as_chunks::<BLOCK>().0.iter().zip(out.as_chunks_mut::<BLOCK>().0) {
            m.process_hop(x, y);
        }
        out
    }

    fn noise(len: usize, seed: u32, amp: f32) -> Vec<f32> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                amp * (x as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    #[test]
    fn info_and_latency() {
        let s = stage();
        assert_eq!(*s.model_info(), RNNOISE_INFO);
        assert_eq!(s.latency(), 480);
        assert_eq!(check_host_profile(&s), Ok(StageRole::Noise));
        assert!(s.speech_probability().is_some() && s.gain_adjustment().is_some() && s.aec_telemetry().is_none());
    }

    #[test]
    fn other_formats_are_refused() {
        let mut s = stage();
        assert!(s.activate(&Activation { sample_rate: 44_100, ..Activation::HOST }).is_err());
        assert!(s.activate(&Activation { min_frames: 512, max_frames: 512, ..Activation::HOST }).is_err());
        assert!(s.activate(&Activation::HOST).is_ok());
    }

    #[test]
    fn a_quiet_impulse_comes_out_one_block_later() {
        // Below RNNoise's silence threshold the gains stay at 1, so the framing shows alone: the impulse returns 480
        // samples later (through the model's DC-blocking high-pass).
        let mut s = stage();
        let mut x = vec![0.0f32; 4 * BLOCK];
        x[BLOCK + 37] = 1e-6;
        let y = run(&mut s, &x);
        let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i);
        assert_eq!(peak, Some(BLOCK + 37 + 480));
        let gain = y[BLOCK + 37 + 480] / 1e-6;
        assert!((gain - 1.0).abs() < 0.01, "gain {gain}");
    }

    #[test]
    fn non_finite_input_is_treated_as_silence() {
        let mut s = stage();
        let mut x = noise(6 * BLOCK, 7, 0.1);
        for v in x.iter_mut().skip(BLOCK).step_by(97) {
            *v = f32::NAN;
        }
        x[3 * BLOCK] = f32::INFINITY;
        x[3 * BLOCK + 1] = f32::NEG_INFINITY;
        let y = run(&mut s, &x);
        assert!(y.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        // A block of NaN is a block of zeros.
        let mut a = stage();
        let mut b = stage();
        let ya = run(&mut a, &[noise(BLOCK, 3, 0.1), vec![f32::NAN; BLOCK], vec![0.0; BLOCK]].concat());
        let yb = run(&mut b, &[noise(BLOCK, 3, 0.1), vec![0.0; BLOCK], vec![0.0; BLOCK]].concat());
        assert_eq!(ya, yb);
    }

    #[test]
    fn output_is_clamped_to_full_scale() {
        // A square wave at 4x full scale: the model's output exceeds ±32768, the stage's doesn't.
        let mut s = stage();
        let x: Vec<f32> = (0..8 * BLOCK).map(|i| if (i / 60) % 2 == 0 { 4.0 } else { -4.0 }).collect();
        let y = run(&mut s, &x);
        assert!(y.iter().all(|v| v.abs() <= 1.0));
        assert!(y.iter().any(|v| v.abs() == 1.0), "the test signal should have clipped");
        let unclamped = run_model(&mut RnnoiseModel::new(), &x);
        assert!(unclamped.iter().any(|v| v.abs() > 1.0), "the model itself doesn't clamp");
    }

    #[test]
    fn huge_finite_samples_reach_the_model_bounded() {
        // A finite sample of about 1e17 or more overflowed the ×32768 to inf, and the NaN it became aborted inside easyfft
        // (undefined behaviour in release). The stage clamps its input to ±INPUT_LIMIT first, and the model stays finite
        // at that limit, held for blocks on end.
        let limit = nf_core::dsp::INPUT_LIMIT;
        let mut x = noise(16 * BLOCK, 21, 0.1);
        for (k, i) in (BLOCK..x.len()).step_by(53).enumerate() {
            x[i] = [1e17, f32::MAX, -f32::MAX, -3e30, f32::MIN_POSITIVE * 1e30][k % 5];
        }
        for (i, v) in x[8 * BLOCK..12 * BLOCK].iter_mut().enumerate() {
            *v = if (i / 60) % 2 == 0 { f32::MAX } else { -1e20 };
        }
        let y = run(&mut stage(), &x);
        assert!(y.iter().all(|v| v.is_finite() && v.abs() <= 1.0), "the stage's output is finite and clamped");
        let at_limit: Vec<f32> = (0..20 * BLOCK).map(|i| if (i / 60) % 2 == 0 { limit } else { -limit }).collect();
        let raw = run_model(&mut RnnoiseModel::new(), &at_limit);
        assert!(raw.iter().all(|v| v.is_finite()), "the model itself at the limit");
        let loud_noise = noise(20 * BLOCK, 5, limit);
        assert!(run_model(&mut RnnoiseModel::new(), &loud_noise).iter().all(|v| v.is_finite()));
    }

    #[test]
    fn reset_restores_a_pristine_state_each_time() {
        let input = noise(10 * BLOCK, 11, 0.2);
        let fresh = run(&mut stage(), &input);
        let mut s = stage();
        for round in 0..3 {
            run(&mut s, &noise(7 * BLOCK, 100 + round, 0.3));
            s.reset();
            assert_eq!(run(&mut s, &input), fresh, "reset {round}");
        }
        // activate rebuilds the spare, and the next reset is pristine again.
        s.activate(&Activation::HOST).expect("activate");
        run(&mut s, &noise(3 * BLOCK, 5, 0.3));
        s.reset();
        assert_eq!(run(&mut s, &input), fresh);
    }

    #[test]
    fn the_model_spends_its_spare_then_rebuilds() {
        let mut m = RnnoiseModel::new();
        assert!(m.spare_fresh);
        m.reset();
        assert!(!m.spare_fresh, "the first reset swaps the spare in");
        m.reset();
        assert!(!m.spare_fresh, "the second rebuilds in place");
        m.prepare().unwrap();
        assert!(m.spare_fresh, "prepare rebuilds the spare");
        // A pristine model either way.
        let x = noise(6 * BLOCK, 4, 0.2);
        let fresh = run_model(&mut RnnoiseModel::new(), &x);
        run_model(&mut m, &noise(3 * BLOCK, 9, 0.3));
        m.reset();
        assert_eq!(run_model(&mut m, &x), fresh);
    }

    #[test]
    fn reset_brings_back_no_stale_audio() {
        let mut s = stage();
        run(&mut s, &noise(8 * BLOCK, 9, 0.5));
        s.reset();
        assert!(run(&mut s, &[0.0; 3 * BLOCK]).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn the_speech_probability_is_published() {
        let mut s = stage();
        let read = |s: &NoiseStage| {
            let sp = s.speech_probability().map(|e| e.speech_probability());
            (sp.unwrap_or(f32::NAN), s.gain_adjustment().map_or(f32::NAN, |e| e.gain_adjustment_db()))
        };
        assert!(read(&s).0.is_nan(), "nothing processed yet");
        let mut y = [0.0f32; BLOCK];
        s.process(ProcessIo::new(&[&[0.0; BLOCK]], &mut y));
        assert_eq!(read(&s).0, 0.0, "a silent frame reports 0");
        let x = noise(BLOCK, 1, 0.3);
        s.process(ProcessIo::new(&[x.as_slice().try_into().expect("block")], &mut y));
        let (sp, gr) = read(&s);
        assert!((0.0..=1.0).contains(&sp));
        assert!(gr <= 0.0);
    }
}
