//! RNNoise through nnnoiseless 0.5.2, then nf-core's `NoisePost` (floor mix + gate).
//! Facts below cite doc/dev/api/sonora-nnnoiseless.md, Part 2.
//!
//! - **Scale (2.2):** nnnoiseless works in i16 units. The input is ×32768 on the way in, and the
//!   output ÷32768 and clamped to ±1 on the way out. Fed ±1 directly, every frame quieter than about
//!   -14 dBFS would count as silence and pass unfiltered.
//! - **Non-finite input (2.2):** a NaN reaches an `unwrap_unchecked` inside easyfft (undefined
//!   behaviour), so NaN and ±inf become 0 before the model and in the dry path.
//! - **Latency (2.3):** output block k is the overlap-add of input block k-1, so the stream offset
//!   is one block, 480 samples. `NoisePost` delays the dry path by the same amount.
//! - **Allocation (2.4):** `new` and `activate` allocate; `process` doesn't. easyfft builds its FFT
//!   planner and scratch per thread on that thread's first FFT, and leaks them. `activate` therefore
//!   runs two frames through a throwaway state on the *calling* thread, and the engine activates on
//!   the audio thread. Activated on another thread, the first `process` allocates. The engine also
//!   calls [`crate::warm_up_fft_thread`] before it enters its real-time scope, so the leaked state
//!   lives on the system heap rather than the locked pool, and each start gives the pool back in
//!   full (review RT-1, `tests/rt_pool.rs`).
//! - **Reset:** nnnoiseless has none, and a fresh state costs ten allocations (`DenoiseState::new`).
//!   So a pristine spare is kept, built off the audio path. `reset` swaps it in: no allocation, no
//!   free. The used state stays parked in the spare's slot until `activate` rebuilds the spare. A
//!   second `reset` before then replaces the state in place (ten allocations and frees). A reset
//!   with nothing processed since the last one is a no-op, so the chain's reset on its first block
//!   doesn't spend the spare. Feeding silence instead isn't a reset: RNNoise's input high-pass
//!   (pole radius 0.998) rings on from old audio for tens of ms, and silent frames don't touch the
//!   RNN state at all.

use std::sync::Arc;

use nf_core::chain::MAX_STAGE_LATENCY;
use nf_core::controls::ChainControls;
use nf_core::dsp::noise_post::NoisePost;
use nf_core::stage::{ProcessIo, Stage, StageError, StageInfo, StageReport, StageRole};
use nf_core::{BLOCK, Block};
use nnnoiseless::DenoiseState;

use crate::{check_format, sanitize};

/// The stream offset: one block (see the module notes).
pub const RNNOISE_LATENCY: u32 = 480;
/// i16 full scale, nnnoiseless's unit.
const S16: f32 = 32768.0;
const INV_S16: f32 = 1.0 / S16;

const _: () = assert!(DenoiseState::FRAME_SIZE == BLOCK);
const _: () = assert!(RNNOISE_LATENCY as usize == BLOCK && BLOCK <= MAX_STAGE_LATENCY);

const INFO: StageInfo =
    StageInfo { id: "rnnoise", name: "RNNoise", role: StageRole::Noise, needs_reference: false, needs_worker: false };

/// RNNoise noise stage.
pub struct RnnoiseStage {
    controls: Arc<ChainControls>,
    state: Box<DenoiseState<'static>>,
    /// Pristine while `spare_fresh`; otherwise the state the last `reset` retired.
    spare: Box<DenoiseState<'static>>,
    spare_fresh: bool,
    /// Something was processed since `new` or the last `reset`.
    dirty: bool,
    post: NoisePost,
    // Blocks inline rather than boxed, and the loops in kernels taking each block as its own
    // argument: then LLVM knows nothing overlaps and vectorises without run-time overlap checks.
    /// This block's input with non-finite samples zeroed, in ±1: the dry path.
    clean: Block,
    /// The same in i16 units, for the model.
    rnn_in: Block,
    /// The model's output in i16 units: last block's input, denoised.
    rnn_out: Block,
}

impl RnnoiseStage {
    /// Reads `controls.noise` (floor, gate) every block.
    pub fn new(controls: Arc<ChainControls>) -> Self {
        Self {
            controls,
            state: DenoiseState::new(),
            spare: DenoiseState::new(),
            spare_fresh: true,
            dirty: false,
            post: NoisePost::new(RNNOISE_LATENCY),
            clean: [0.0; BLOCK],
            rnn_in: [0.0; BLOCK],
            rnn_out: [0.0; BLOCK],
        }
    }
}

impl Stage for RnnoiseStage {
    fn info(&self) -> StageInfo {
        INFO
    }

    /// Rebuilds a used spare and warms up easyfft on the calling thread (see the module notes).
    fn activate(&mut self, sample_rate: u32, block: usize) -> Result<(), StageError> {
        check_format(INFO.name, sample_rate, block)?;
        if !self.spare_fresh {
            self.spare = DenoiseState::new();
            self.spare_fresh = true;
        }
        warm_up_thread();
        Ok(())
    }

    fn latency(&self) -> u32 {
        RNNOISE_LATENCY
    }

    fn process(&mut self, io: ProcessIo<'_>) -> StageReport {
        self.dirty = true;
        to_model(io.input, &mut self.clean, &mut self.rnn_in);
        // The VAD comes from the window [k-1 | k], which is what this call's output covers.
        let vad = self.state.process_frame(&mut self.rnn_out[..], &self.rnn_in[..]);
        from_model(&self.rnn_out, io.output);
        self.post.process(&self.clean, io.output, &self.controls.noise, vad)
    }

    fn reset(&mut self) {
        if !self.dirty {
            return; // already pristine: keep the spare
        }
        self.dirty = false;
        if self.spare_fresh {
            std::mem::swap(&mut self.state, &mut self.spare);
            self.spare_fresh = false;
        } else {
            self.state = DenoiseState::new();
        }
        self.post.reset();
    }
}

/// The stage input, sanitised: in ±1 into `clean` (the dry path), in i16 units into `scaled`.
#[inline]
fn to_model(input: &Block, clean: &mut Block, scaled: &mut Block) {
    for ((c, s), &x) in clean.iter_mut().zip(scaled.iter_mut()).zip(input) {
        let v = sanitize(x);
        *c = v;
        *s = v * S16;
    }
}

/// The model's output back to ±1, clamped (nnnoiseless doesn't clamp).
#[inline]
fn from_model(y: &Block, out: &mut Block) {
    for (o, &v) in out.iter_mut().zip(y) {
        *o = (v * INV_S16).clamp(-1.0, 1.0);
    }
}

/// Makes easyfft build this thread's FFT state now, through a throwaway model state so the live
/// one stays pristine. One silent frame runs every FFT; the tone frame also runs the RNN and the
/// pitch filter, in case a later version allocates there.
pub(crate) fn warm_up_thread() {
    let mut scratch = DenoiseState::new();
    let mut out = [0.0f32; BLOCK];
    scratch.process_frame(&mut out, &[0.0; BLOCK]);
    let tone: Block = std::array::from_fn(|i| 3000.0 * (i as f32 * 0.07).sin());
    scratch.process_frame(&mut out, &tone);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nf_core::SAMPLE_RATE;
    use nf_core::config::ProcessingConfig;

    fn stage() -> RnnoiseStage {
        let cfg = ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() };
        let mut s = RnnoiseStage::new(Arc::new(ChainControls::new(&cfg)));
        s.activate(SAMPLE_RATE, BLOCK).expect("activate");
        s
    }

    fn run(s: &mut RnnoiseStage, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        let mut y = [0.0f32; BLOCK];
        for x in input.as_chunks::<BLOCK>().0 {
            s.process(ProcessIo { input: x, reference: None, output: &mut y });
            out.extend_from_slice(&y);
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
        assert_eq!(s.info(), INFO);
        assert_eq!(s.latency(), 480);
    }

    #[test]
    fn other_formats_are_refused() {
        let mut s = stage();
        assert!(s.activate(44_100, BLOCK).is_err());
        assert!(s.activate(SAMPLE_RATE, 512).is_err());
        assert!(s.activate(SAMPLE_RATE, BLOCK).is_ok());
    }

    #[test]
    fn a_quiet_impulse_comes_out_one_block_later() {
        // Below RNNoise's silence threshold the gains stay at 1, so the framing shows alone: the
        // impulse returns 480 samples later (through the model's DC-blocking high-pass).
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
        s.activate(SAMPLE_RATE, BLOCK).expect("activate");
        assert!(s.spare_fresh);
        run(&mut s, &noise(3 * BLOCK, 5, 0.3));
        s.reset();
        assert!(!s.spare_fresh);
        assert_eq!(run(&mut s, &input), fresh);
    }

    #[test]
    fn resetting_an_unused_stage_keeps_the_spare() {
        let mut s = stage();
        s.reset();
        s.reset();
        assert!(s.spare_fresh, "nothing was processed: nothing to clear");
        run(&mut s, &noise(BLOCK, 1, 0.1));
        s.reset();
        assert!(!s.spare_fresh);
        s.reset();
        assert!(!s.spare_fresh && !s.dirty);
    }

    #[test]
    fn reset_brings_back_no_stale_audio() {
        let mut s = stage();
        run(&mut s, &noise(8 * BLOCK, 9, 0.5));
        s.reset();
        assert!(run(&mut s, &[0.0; 3 * BLOCK]).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn the_speech_probability_is_reported() {
        let mut s = stage();
        let mut y = [0.0f32; BLOCK];
        let r = s.process(ProcessIo { input: &[0.0; BLOCK], reference: None, output: &mut y });
        assert_eq!(r.speech_probability, 0.0, "a silent frame reports 0");
        let x = noise(BLOCK, 1, 0.3);
        let r = s.process(ProcessIo { input: x.as_slice().try_into().expect("block"), reference: None, output: &mut y });
        assert!((0.0..=1.0).contains(&r.speech_probability));
        assert!(r.gain_reduction_db <= 0.0);
    }
}
