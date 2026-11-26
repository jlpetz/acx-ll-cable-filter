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
//! - **Reset:** nnnoiseless has none, and a fresh state costs ten allocations (`DenoiseState::new`). So pristine spares are
//!   kept, built off the audio thread, and `reset` swaps one in: no allocation, no free, no lock, no system call. A thread of
//!   the model's own, the spare maker (started at the first `prepare`, the stage's `activate`), keeps [`SPARES`] of them on a
//!   lock-free ring and frees the states that `reset` retired, on a second: it looks every [`MAKER_POLL`] (100 ms), and
//!   `prepare` fills the ring before it starts it, so the first resets after an `activate` find their spares there. Only a
//!   third real reset within 100 ms of two others could find none (no control toggles that fast): the one known exception
//!   to the reset rule (`nf_core::noise::NoiseModel::reset`), it rebuilds the state in place, ten allocations that the
//!   engine's real-time pool serves, as every reset after the first did before the maker (TODO 4), and counts it
//!   ([`RnnoiseModel::rebuilds`]); the used state is retired as at any reset, so nothing is freed on the audio thread. A
//!   model that was never prepared (no maker) has its first [`SPARES`] resets free and rebuilds after them. The maker polls
//!   rather than being woken: waking it (`Thread::unpark`) would be a system call in `reset`, which the stage rule keeps
//!   free of them, and a thread that looks at two rings ten times a second costs nothing beside the audio thread's hundreds
//!   of wakes. The states built with the model and at `prepare` come from the real-time pool (the engine builds the chain
//!   in its real-time scope), and the maker frees them when they're retired: nf-mem's pool takes frees from any thread
//!   (by address, under its spin lock, which the audio thread takes only when it allocates: at a start, and in the
//!   exception above). The noise stage resets the model only
//!   when it processed something since its last reset, so the chain's resets don't spend a spare. Dropping the model stops
//!   and joins the maker, so every state is freed by the time the drop returns (the engine's real-time pool comes back in
//!   full at each start: review RT-1). Feeding silence instead isn't a reset: RNNoise's input high-pass (pole radius 0.998)
//!   rings on from old audio for tens of ms, and silent frames don't touch the RNN state at all.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::thread::JoinHandle;
use std::time::Duration;

use nf_core::chain::MAX_STAGE_LATENCY;
use nf_core::controls::ChainControls;
use nf_core::noise::{ModelInfo, NoiseModel, NoiseStage};
use nf_core::stage::{StageError, text};
use nf_core::{BLOCK, Block};
use nnnoiseless::DenoiseState;
use rtrb::{Consumer, Producer, PushError, RingBuffer};

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

/// Pristine spare states kept for `reset` (the module notes): two, so two real resets within the maker's poll find one each.
pub const SPARES: usize = 2;
/// How often the spare maker looks for spent spares to replace (it needs no wake-up from the audio thread).
pub const MAKER_POLL: Duration = Duration::from_millis(100);
/// The retired states' ring: room for every state the audio thread can retire between two of the maker's passes, and more.
const RETIRED: usize = 2 * SPARES + 2;

type State = Box<DenoiseState<'static>>;

/// The maker's side of the spares: pristine states go out on `fresh`, retired ones come back on `retired` to be freed.
struct Shelf {
    fresh: Producer<State>,
    retired: Consumer<State>,
}

impl Shelf {
    /// Frees what was retired and fills `fresh` with pristine states (not real-time: it allocates and frees). It frees before
    /// each state it builds, so resets that take each new spare as it comes can't fill the retired ring meanwhile.
    fn top_up(&mut self, stop: Option<&AtomicBool>) {
        loop {
            while let Ok(used) = self.retired.pop() {
                drop(used);
            }
            if self.fresh.slots() == 0 || stop.is_some_and(|s| s.load(Acquire)) || self.fresh.push(DenoiseState::new()).is_err() {
                return;
            }
        }
    }
}

/// The spare maker's thread and its stop flag. Dropping it stops and joins the thread, which hands its shelf back, so what
/// the shelf holds is freed by whoever drops the model, as the states it built at the start were allocated there.
struct Maker {
    thread: Option<JoinHandle<Shelf>>,
    stop: Arc<AtomicBool>,
}

impl Maker {
    /// Starts the maker on `shelf`; `None` if no thread could be had (the shelf is gone with it, and the model rebuilds in
    /// place once its spares are spent).
    fn start(mut shelf: Shelf) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("nf-rnnoise-spares".into())
            .spawn(move || {
                while !flag.load(Acquire) {
                    shelf.top_up(Some(&flag));
                    std::thread::park_timeout(MAKER_POLL);
                }
                shelf
            })
            .ok()?;
        Some(Self { thread: Some(thread), stop })
    }
}

impl Drop for Maker {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            drop(t.join());
        }
    }
}

/// RNNoise as a [`NoiseModel`].
pub struct RnnoiseModel {
    state: State,
    /// The audio thread's side of the spares: pristine ones come in on `fresh`, retired ones go out on `retired` (the module
    /// notes).
    fresh: Consumer<State>,
    retired: Producer<State>,
    /// A retired state the ring had no room for, retired again at the next reset.
    parked: Option<State>,
    /// The maker's side, until the first `prepare` starts the maker with it.
    shelf: Option<Shelf>,
    maker: Option<Maker>,
    /// Resets that found no spare and rebuilt the state in place (the module notes).
    rebuilds: u64,
    // Blocks inline rather than boxed, and the loops in kernels taking each block as its own argument: then LLVM knows
    // nothing overlaps and vectorises without run-time overlap checks.
    /// The hop's input in i16 units.
    rnn_in: Block,
    /// The model's output in i16 units: last hop's input, denoised.
    rnn_out: Block,
}

impl RnnoiseModel {
    /// The live state and [`SPARES`] pristine ones (allocates). The spare maker starts at the first `prepare`.
    pub fn new() -> Self {
        let (fresh_tx, fresh) = RingBuffer::new(SPARES);
        let (retired, retired_rx) = RingBuffer::new(RETIRED);
        let mut shelf = Shelf { fresh: fresh_tx, retired: retired_rx };
        shelf.top_up(None);
        Self {
            state: DenoiseState::new(),
            fresh,
            retired,
            parked: None,
            shelf: Some(shelf),
            maker: None,
            rebuilds: 0,
            rnn_in: [0.0; BLOCK],
            rnn_out: [0.0; BLOCK],
        }
    }

    /// Pristine spares ready for the next resets.
    pub fn spares(&self) -> usize {
        self.fresh.slots()
    }

    /// The spare maker runs (from the first `prepare`).
    pub fn has_maker(&self) -> bool {
        self.maker.is_some()
    }

    /// Resets that found no spare and rebuilt the state in place, ten allocations each: with the maker, only a third real
    /// reset within its poll of two others (the module notes).
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Hands a retired state over to be freed off this thread: the parked one first, then this one; one the ring has no room
    /// for waits in `parked`. Real-time, but for one case the ring's size rules out while the maker runs: the ring and the
    /// parking place both full (only a model never prepared, on a thread that isn't real-time) frees it here.
    fn retire(&mut self, used: State) {
        if let Some(p) = self.parked.take()
            && let Err(PushError::Full(p)) = self.retired.push(p)
        {
            self.parked = Some(p);
        }
        if let Err(PushError::Full(u)) = self.retired.push(used) {
            if self.parked.is_none() {
                self.parked = Some(u);
            } else {
                drop(u);
            }
        }
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

    /// Starts the spare maker the first time, its spares filled first, and warms up easyfft on the calling thread (see the
    /// module notes).
    fn prepare(&mut self) -> Result<(), StageError> {
        if let Some(mut shelf) = self.shelf.take() {
            shelf.top_up(None);
            self.maker = Maker::start(shelf);
        }
        warm_up_thread();
        Ok(())
    }

    /// Swaps in a pristine spare and retires the used state to the maker; with none left (the module notes), builds the
    /// fresh state here (ten allocations, counted) and retires the used one all the same.
    fn reset(&mut self) {
        let fresh = self.fresh.pop().unwrap_or_else(|_| {
            self.rebuilds = self.rebuilds.saturating_add(1);
            DenoiseState::new()
        });
        let used = std::mem::replace(&mut self.state, fresh);
        self.retire(used);
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

    /// Waits (up to 5 s) until the maker has put back every spare.
    fn until_spares_are_back(m: &RnnoiseModel) {
        let start = std::time::Instant::now();
        while m.spares() < SPARES {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the maker didn't replace a spare: {}", m.spares());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The spares: two built with the model; the first `prepare` starts their maker, which replaces each one a reset takes
    /// (within its 100 ms poll), so every reset swaps in a pristine state. Without the maker (a model never prepared) the
    /// first two resets take the spares and the rest rebuild in place: pristine either way.
    #[test]
    fn the_maker_replaces_each_spare_a_reset_takes() {
        let x = noise(6 * BLOCK, 4, 0.2);
        let fresh = run_model(&mut RnnoiseModel::new(), &x);
        let mut m = RnnoiseModel::new();
        assert_eq!((m.spares(), m.has_maker()), (SPARES, false));
        m.prepare().unwrap();
        assert!(m.has_maker(), "prepare starts the maker");
        for round in 0..6 {
            run_model(&mut m, &noise(3 * BLOCK, 9 + round, 0.3));
            m.reset();
            assert_eq!(m.spares(), SPARES - 1, "round {round}: the reset took a spare");
            assert_eq!(run_model(&mut m, &x), fresh, "round {round}: pristine");
            until_spares_are_back(&m);
        }
        // Two resets in a row take both; the maker replaces both.
        m.reset();
        m.reset();
        assert_eq!(m.spares(), 0);
        until_spares_are_back(&m);
        assert_eq!(m.rebuilds(), 0, "every reset found a spare");
        // A second prepare keeps the maker it has.
        m.prepare().unwrap();
        assert!(m.has_maker() && m.spares() == SPARES);
        drop(m);
        // Without the maker the spares run out, and each reset after them rebuilds in place, counted.
        let mut never = RnnoiseModel::new();
        for round in 0..4 {
            run_model(&mut never, &noise(3 * BLOCK, 20 + round, 0.3));
            never.reset();
            assert_eq!(never.spares(), SPARES.saturating_sub(round as usize + 1), "round {round}");
            assert_eq!(never.rebuilds(), (round as u64 + 1).saturating_sub(SPARES as u64), "round {round}");
            assert_eq!(run_model(&mut never, &x), fresh, "round {round}: without the maker, pristine all the same");
        }
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
        let x: &Block = x.as_slice().try_into().expect("block");
        s.process(ProcessIo::new(&[x], &mut y));
        let (sp, gr) = read(&s);
        assert!((0.0..=1.0).contains(&sp));
        assert!(gr.is_nan(), "the aligned input is still the preceding silent block");
        s.process(ProcessIo::new(&[&[0.0; BLOCK]], &mut y));
        let (sp, gr) = read(&s);
        assert!((0.0..=1.0).contains(&sp));
        assert!((-120.0..=0.0).contains(&gr), "the noisy block now has a measurable reduction: {gr}");
    }
}
