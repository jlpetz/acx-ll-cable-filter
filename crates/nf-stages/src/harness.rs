//! TODO 2's offline harness: runs a stage, a model or the chain block by block, exactly as the engine calls them, and
//! measures what the checks need. Not real-time code: everything here allocates freely.
//!
//! - [`run_chain`]: the chain over a mic and a reference (`noise-filter process` and `quality` use it).
//! - [`chain_for`]: one stage alone in its role's slot of a [`Chain`], enabled, so it's called as the engine calls it.
//! - [`run_stage`]: direct per-block calls on a stage, with what its extensions published after each block.
//! - Offsets: [`impulse_offset`] (an impulse's peak, to the sample), [`gcc_offset`] (GCC-PHAT on a broadband signal, to a
//!   fraction of one) and [`model_offset`] (a [`NoiseModel`] driven hop by hop).
//! - [`parity`]: an output against a reference output (nf-ort's parity checks; a pack's PCM16 conformance pair is
//!   `nf_ort::conformance::compare_pcm16`).
//! - [`scenes`]: the test scenes, over [`crate::synth`].
//! - [`audio`]: the fetched test-audio set (TODO 14) and the quality measures the ignored tests print (the pause cut, below
//!   and above 8 kHz, and the speech kept), shared by nf-stages' and nf-ort's tests.
//! - The checks any stage, model or pack must pass, callable outside the tests (TODO 5's pack loader will run them on a
//!   pack): [`check_latency`] and [`check_model`] (check 1) and [`check_profile`] (check 4's common part). Each returns
//!   what it measured, or why it failed.
//!
//! The tests call them on every built-in stage: `tests/conformance.rs` (check 1 and 4, plus our stages' own interface
//! promises), the parity tests (check 2), `tests/timing.rs` and `noise-filter bench` (check 3).

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::{Chain, MAX_STAGE_LATENCY};
use nf_core::controls::ChainControls;
use nf_core::latency::analysis::gcc_phat;
use nf_core::noise::NoiseModel;
use nf_core::stage::{Activation, AecStats, ParamFlags, ProcessIo, Stage, StageError, StageReport, StageRole, check_host_profile};
use nf_core::{BLOCK, Block, SAMPLE_RATE};

use crate::synth::{Voice, white_noise};

pub mod audio;

/// Runs `mic` and `reference` (silence past either's end) through `chain` in blocks, and returns as many samples as `mic`:
/// the chain's output from sample `skip` on, flushed with silence so the tail comes out too. `skip` = the chain's latency
/// aligns the output with the input.
pub fn run_chain(chain: &mut Chain, mic: &[f32], reference: &[f32], skip: usize) -> Vec<f32> {
    let blocks = (mic.len() + skip).div_ceil(BLOCK);
    let mut out = Vec::with_capacity(blocks * BLOCK);
    let (mut m, mut r, mut y) = ([0.0f32; BLOCK], [0.0f32; BLOCK], [0.0f32; BLOCK]);
    for b in 0..blocks {
        fill(&mut m, mic, b * BLOCK);
        fill(&mut r, reference, b * BLOCK);
        chain.process(&m, &r, &mut y);
        out.extend_from_slice(&y);
    }
    out.drain(..skip.min(out.len()));
    out.truncate(mic.len());
    out
}

/// `block` = `x[from..from + BLOCK]`, with silence past the end of `x`.
pub fn fill(block: &mut Block, x: &[f32], from: usize) {
    let src = x.get(from..).unwrap_or(&[]);
    let n = src.len().min(BLOCK);
    let (head, tail) = block.split_at_mut(n);
    head.copy_from_slice(&src[..n]);
    tail.fill(0.0);
}

/// A chain holding `stage` alone, in its role's slot, with that slot switched on and activated: the stage is then called
/// exactly as the engine calls it (host-profile check, activation, bypass state, priming). An effect has no slot in v0.
pub fn chain_for(stage: Box<dyn Stage>, controls: Arc<ChainControls>) -> Result<Chain, StageError> {
    let mut chain = match check_host_profile(&*stage)? {
        StageRole::EchoCancel => {
            controls.aec.enabled.store(true, Relaxed);
            Chain::new(controls, Some(stage), None)
        }
        StageRole::Noise => {
            controls.noise.enabled.store(true, Relaxed);
            Chain::new(controls, None, Some(stage))
        }
        StageRole::Effect => return Err(StageError(format!("stage {}: the v0 chain has no effects slot", stage.descriptor().id))),
    };
    chain.activate()?;
    Ok(chain)
}

/// A stage's output over a run, with what it published after each block.
#[derive(Clone, Debug, Default)]
pub struct Run {
    pub out: Vec<f32>,
    /// Its `speech-probability/0` and `gain-adjustment-metering/0` after each block (NaN where it has none).
    pub reports: Vec<StageReport>,
    /// Its `aec-telemetry/0` after each block (NaN where it has none).
    pub aec: Vec<AecStats>,
}

/// Runs `stage` (activated) over `input` in whole blocks, `aux` on its second input port when given (block-aligned with
/// `input`, silence past its end), calling it directly as the chain calls a stage that is on.
pub fn run_stage(stage: &mut dyn Stage, input: &[f32], aux: Option<&[f32]>) -> Run {
    assert_eq!(input.len() % BLOCK, 0, "whole blocks only");
    let blocks = input.len() / BLOCK;
    let mut run = Run { out: Vec::with_capacity(input.len()), reports: Vec::with_capacity(blocks), aec: Vec::with_capacity(blocks) };
    let (mut y, mut r) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
    for (b, x) in input.as_chunks::<BLOCK>().0.iter().enumerate() {
        match aux {
            Some(a) => {
                fill(&mut r, a, b * BLOCK);
                stage.process(ProcessIo::new(&[x, &r], &mut y));
            }
            None => stage.process(ProcessIo::new(&[x], &mut y)),
        }
        run.reports.push(StageReport::read(stage));
        run.aec.push(stage.aec_telemetry().map(|t| t.aec_stats()).unwrap_or_default());
        run.out.extend_from_slice(&y);
    }
    run
}

/// Runs `model` over `x` hop by hop (zero-padded to whole hops); the output is as long as `x`.
pub fn run_model(model: &mut dyn NoiseModel, x: &[f32]) -> Vec<f32> {
    let hop = model.info().hop.max(1);
    let mut padded = x.to_vec();
    padded.resize(x.len().div_ceil(hop) * hop, 0.0);
    let mut out = vec![0.0f32; padded.len()];
    for (i, o) in padded.chunks_exact(hop).zip(out.chunks_exact_mut(hop)) {
        model.process_hop(i, o);
    }
    out.truncate(x.len());
    out
}

/// The index of the largest |x| (the first, on a tie; 0 for an empty slice).
pub fn peak_index(x: &[f32]) -> usize {
    x.iter().enumerate().fold((0, -1.0f32), |(bi, bv), (i, v)| if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }).0
}

/// The stream offset an impulse of `amp` at sample `at` shows through a fresh stage from `make` (activated): its output's
/// peak minus `at`. With `aux`, a silent second input. `None` if the peak comes before the impulse.
pub fn impulse_offset(make: impl FnOnce() -> Box<dyn Stage>, at: usize, amp: f32, aux: bool) -> Option<usize> {
    let len = (at + MAX_STAGE_LATENCY + 2 * BLOCK).div_ceil(BLOCK) * BLOCK;
    let mut x = vec![0.0f32; len];
    x[at] = amp;
    let silent = vec![0.0f32; len];
    let mut stage = make();
    let y = run_stage(&mut *stage, &x, aux.then_some(&silent[..])).out;
    peak_index(&y).checked_sub(at)
}

/// The GCC-PHAT search range for a stage that declares `declared` samples: twice that plus a block, so a framing error of a
/// whole block either way still shows.
pub fn max_lag_for(declared: u32) -> usize {
    2 * declared as usize + BLOCK
}

/// The delay of `y` against `x` by GCC-PHAT (β 0.75), to a fraction of a sample, and its confidence in dB.
pub fn gcc_offset(x: &[f32], y: &[f32], max_lag: usize) -> Option<(f64, f32)> {
    gcc_phat(x, y, max_lag, 0.75)
}

/// A model's stream offset in its own framing: `x` (an impulse, or anything with one clear peak) driven through it hop by
/// hop, the output's peak minus the input's. Its declared one is `latency_samples - hop`.
pub fn model_offset(model: &mut dyn NoiseModel, x: &[f32]) -> Option<usize> {
    let y = run_model(model, x);
    peak_index(&y).checked_sub(peak_index(x))
}

/// Where [`check_latency`] puts its impulses: past a stage's start-up, at block phases 17, 301, 0 and 479.
pub const IMPULSE_AT: [usize; 4] = [5 * BLOCK + 17, 20 * BLOCK + 301, 50 * BLOCK, 61 * BLOCK + 479];
/// Check 1's GCC-PHAT bound: within this many samples of the declared offset ...
pub const GCC_TOLERANCE: f64 = 0.1;
/// ... at this confidence or more.
pub const GCC_MIN_CONFIDENCE_DB: f32 = 25.0;

/// What check 1 measured on a stage or a model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatencyReport {
    /// The declared offset: a stage's `latency()`, or a model's `latency_samples - hop`.
    pub declared: u32,
    /// The impulse amplitude, when the impulse check ran (it then matched at every phase).
    pub impulse_amp: Option<f32>,
    /// GCC-PHAT's offset on speech in pink noise, to a fraction of a sample, and its confidence.
    pub gcc_offset: f64,
    pub gcc_confidence_db: f32,
}

/// Check 1's broadband scene: 8 s of a low voice over pink noise at 30 dB SNR.
fn gcc_scene() -> Vec<f32> {
    scenes::noisy_speech(8 * SAMPLE_RATE as usize, Voice::LOW, 30.0, 1).mix
}

/// GCC-PHAT of `y` against `x` must find `declared` within [`GCC_TOLERANCE`] at [`GCC_MIN_CONFIDENCE_DB`] or more.
fn gcc_matches(what: &str, x: &[f32], y: &[f32], declared: u32, max_lag: usize) -> Result<(f64, f32), String> {
    let (d, conf) = gcc_offset(x, y, max_lag).ok_or_else(|| format!("{what}: GCC-PHAT found no correlation"))?;
    if (d - f64::from(declared)).abs() < GCC_TOLERANCE && conf > GCC_MIN_CONFIDENCE_DB {
        Ok((d, conf))
    } else {
        Err(format!("{what}: GCC-PHAT on speech gives {d:.3} samples at {conf:.1} dB, declared {declared}"))
    }
}

/// TODO 2's check 1 on a stage, to the sample: the stream offset it shows equals its `latency()`.
/// - With `impulse_amp`: an impulse at each of [`IMPULSE_AT`] comes out exactly `latency()` later. Pass `None` for a stage
///   that suppresses an isolated click, as a neural model may; RNNoise passes one at 1e-6, under its silence threshold.
/// - Always: GCC-PHAT on speech in pink noise finds `latency()` within [`GCC_TOLERANCE`], at [`GCC_MIN_CONFIDENCE_DB`] or
///   more (the measurement that carries over to any model).
///
/// `make` gives a fresh stage each call, which the check activates at [`Activation::HOST`]. A second input gets silence.
pub fn check_latency(make: &dyn Fn() -> Box<dyn Stage>, impulse_amp: Option<f32>) -> Result<LatencyReport, String> {
    let fresh = || -> Result<Box<dyn Stage>, String> {
        let mut s = make();
        s.activate(&Activation::HOST).map_err(|e| e.0)?;
        Ok(s)
    };
    let mut stage = fresh()?;
    let name = stage.descriptor().id.to_string();
    let declared = stage.latency();
    let aux = stage.audio_ports().input_count() > 1;
    if let Some(amp) = impulse_amp {
        for at in IMPULSE_AT {
            let s = fresh()?;
            let measured = impulse_offset(|| s, at, amp, aux);
            if measured != Some(declared as usize) {
                let phase = at % BLOCK;
                return Err(format!(
                    "{name}: an impulse at {at} (block phase {phase}) shows an offset of {measured:?}, declared {declared}"
                ));
            }
        }
    }
    let x = gcc_scene();
    let silent = vec![0.0f32; x.len()];
    let y = run_stage(&mut *stage, &x, aux.then_some(&silent[..])).out;
    let (gcc_offset, gcc_confidence_db) = gcc_matches(&name, &x, &y, declared, max_lag_for(declared))?;
    Ok(LatencyReport { declared, impulse_amp, gcc_offset, gcc_confidence_db })
}

/// Check 1 on a model alone, driven hop by hop: the offset it shows in its own framing equals `latency_samples - hop`.
/// The same two measurements as [`check_latency`]: an impulse at four phases of a hop (with `impulse_amp`), and GCC-PHAT
/// on speech. `make` gives a fresh model each call, which the check prepares.
pub fn check_model(make: &dyn Fn() -> Box<dyn NoiseModel>, impulse_amp: Option<f32>) -> Result<LatencyReport, String> {
    let fresh = || -> Result<Box<dyn NoiseModel>, String> {
        let mut m = make();
        m.prepare().map_err(|e| e.0)?;
        Ok(m)
    };
    let mut model = fresh()?;
    let info = model.info().clone();
    let (name, hop) = (info.id.to_string(), info.hop.max(1));
    let declared = u32::try_from(info.model_offset()).map_err(|_| format!("{name}: an offset over u32"))?;
    if let Some(amp) = impulse_amp {
        for phase in [17 % hop, hop / 2, 0, hop - 1] {
            let at = 4 * hop + phase;
            let mut x = vec![0.0f32; at + declared as usize + 4 * hop];
            x[at] = amp;
            let measured = model_offset(&mut *fresh()?, &x);
            if measured != Some(declared as usize) {
                return Err(format!("{name}: an impulse at hop phase {phase} shows an offset of {measured:?}, declared {declared}"));
            }
        }
    }
    let x = gcc_scene();
    let y = run_model(&mut *model, &x);
    let (gcc_offset, gcc_confidence_db) = gcc_matches(&name, &x, &y, declared, 2 * declared as usize + hop)?;
    Ok(LatencyReport { declared, impulse_amp, gcc_offset, gcc_confidence_db })
}

/// The part of TODO 2's check 4 every stage must pass, whatever it is (our stages' own promises, such as refusing other
/// formats or their param defaults, are tests of their own):
/// - [`check_host_profile`]: its role, ports and params;
/// - it activates at [`Activation::HOST`], and again without changing its latency, which is at most
///   [`MAX_STAGE_LATENCY`];
/// - each param that isn't read-only reads back a value in its range, clamps a value past either bound to that bound and
///   refuses NaN (each is put back afterwards);
/// - its state round-trips: saved, loaded and saved again, the bytes are the same;
/// - a reset with nothing processed changes nothing: two fresh stages, one of them reset twice first, give the same output
///   on 2 s of noisy speech (and noise on a second input).
///
/// `make` gives a fresh stage each call. Returns its role.
pub fn check_profile(make: &dyn Fn() -> Box<dyn Stage>) -> Result<StageRole, String> {
    let mut stage = make();
    let name = stage.descriptor().id.to_string();
    let fail = |why: String| Err(format!("{name}: {why}"));
    let role = check_host_profile(&*stage).map_err(|e| e.0)?;
    let activate = |s: &mut dyn Stage| s.activate(&Activation::HOST).map_err(|e| format!("{name}: activate: {e}"));
    activate(&mut *stage)?;
    let latency = stage.latency();
    if latency as usize > MAX_STAGE_LATENCY {
        return fail(format!("latency {latency} exceeds {MAX_STAGE_LATENCY}"));
    }
    activate(&mut *stage)?;
    if stage.latency() != latency {
        return fail(format!("activating again moved its latency from {latency} to {}", stage.latency()));
    }
    for p in stage.params().to_vec() {
        let Some(was) = stage.param_value(p.id) else { return fail(format!("param {} ({}) has no value", p.id, p.name)) };
        if !(p.min..=p.max).contains(&was) {
            return fail(format!("param {} ({}) holds {was}, outside {}..{}", p.id, p.name, p.min, p.max));
        }
        if p.flags.contains(ParamFlags::READONLY) {
            continue;
        }
        let span = p.max - p.min;
        for (asked, want) in [(p.min - span, p.min), (p.max + span, p.max)] {
            stage.set_param(p.id, asked).map_err(|e| format!("{name}: {e}"))?;
            let got = stage.param_value(p.id);
            if got != Some(want) {
                return fail(format!("param {} ({}) set to {asked} reads {got:?}, not {want}", p.id, p.name));
            }
        }
        if stage.set_param(p.id, f64::NAN).is_ok() {
            return fail(format!("param {} ({}) took NaN", p.id, p.name));
        }
        stage.set_param(p.id, was).map_err(|e| format!("{name}: {e}"))?;
    }
    let (mut saved, mut again) = (Vec::new(), Vec::new());
    stage.save_state(&mut saved);
    stage.load_state(&saved).map_err(|e| format!("{name}: loading its own state: {e}"))?;
    stage.save_state(&mut again);
    if saved != again {
        return fail("its state doesn't round-trip".into());
    }
    let x = scenes::noisy_speech(2 * SAMPLE_RATE as usize, Voice::LOW, 10.0, 5).mix;
    let far = white_noise(x.len(), 0.05, 6);
    let aux = (stage.audio_ports().input_count() > 1).then_some(&far[..]);
    let mut plain = make();
    activate(&mut *plain)?;
    let mut reset = make();
    activate(&mut *reset)?;
    reset.reset();
    reset.reset();
    if run_stage(&mut *plain, &x, aux).out != run_stage(&mut *reset, &x, aux).out {
        return fail("a reset with nothing processed changed its output".into());
    }
    Ok(role)
}

/// How an output matches a reference output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parity {
    /// Samples compared.
    pub compared: usize,
    pub max_abs: f32,
    /// The error's RMS against the reference's, in dB (`-inf` when they match exactly).
    pub rms_err_db: f32,
    /// The first sample whose bits differ.
    pub first_mismatch: Option<usize>,
}

/// `out[n + shift]` against `reference[n]`, over as much as both have (`shift`: an output that lags its reference by that
/// many samples).
pub fn parity(out: &[f32], reference: &[f32], shift: usize) -> Parity {
    let out = out.get(shift..).unwrap_or(&[]);
    let n = out.len().min(reference.len());
    let (mut max_abs, mut err2, mut ref2, mut first_mismatch) = (0.0f32, 0.0f64, 0.0f64, None);
    for (i, (&a, &b)) in out[..n].iter().zip(&reference[..n]).enumerate() {
        let d = a - b;
        max_abs = max_abs.max(d.abs());
        err2 += f64::from(d) * f64::from(d);
        ref2 += f64::from(b) * f64::from(b);
        if first_mismatch.is_none() && a.to_bits() != b.to_bits() {
            first_mismatch = Some(i);
        }
    }
    let rms_err_db = if err2 == 0.0 {
        f32::NEG_INFINITY
    } else if ref2 == 0.0 {
        f32::INFINITY
    } else {
        (10.0 * (err2 / ref2).log10()) as f32
    };
    Parity { compared: n, max_abs, rms_err_db, first_mismatch }
}

/// The harness's scenes: wrappers over [`crate::synth`], deterministic from their seeds.
pub mod scenes {
    use nf_core::rt::Dither;
    use nf_core::{BLOCK, db_to_lin};

    use crate::synth::{
        Speech, Voice, add, convolve, delayed, mix_at_snr, pink_noise, room_impulse_response, scaled, speech_like, white_noise,
    };

    /// The echo path's bulk delay in [`echo_room`]: 40 ms.
    pub const ECHO_DELAY: usize = 1920;
    /// The block phases [`impulses`] puts its impulses at.
    pub const IMPULSE_PHASES: [usize; 4] = [0, 17, 301, 479];

    /// Speech in noise.
    pub struct NoisySpeech {
        pub mix: Vec<f32>,
        pub speech: Speech,
        /// The noise as mixed.
        pub noise: Vec<f32>,
    }

    /// `voice` over pink noise at `snr_db` below its active level.
    pub fn noisy_speech(len: usize, voice: Voice, snr_db: f32, seed: u64) -> NoisySpeech {
        let speech = speech_like(len, voice, seed);
        let (mix, noise) = mix_at_snr(&speech.samples, speech.active_rms(), &pink_noise(len, 1.0, seed + 1000), snr_db);
        NoisySpeech { mix, speech, noise }
    }

    /// A far talker, the echo it makes at the mic, and the mic's own noise floor.
    pub struct EchoRoom {
        pub far: Speech,
        pub echo: Vec<f32>,
        pub mic_noise: Vec<f32>,
    }

    impl EchoRoom {
        /// What the mic hears: the echo over its noise floor.
        pub fn mic(&self) -> Vec<f32> {
            add(&self.echo, &self.mic_noise)
        }
    }

    /// A low voice at the far end; its echo through a synthetic room (RT60 `rt60_s`), 40 ms late at ×0.5; and a -70 dBFS
    /// mic floor.
    pub fn echo_room(len: usize, rt60_s: f32, seed: u64) -> EchoRoom {
        let far = speech_like(len, Voice::LOW, seed);
        let rir = room_impulse_response(9600, rt60_s, seed + 50);
        let echo = scaled(&delayed(&convolve(&far.samples, &rir)[..len], ECHO_DELAY), 0.5);
        EchoRoom { far, echo, mic_noise: white_noise(len, db_to_lin(-70.0), seed + 9) }
    }

    /// Double talk: an echo room, and a near talker who joins at `near_from`.
    pub struct DoubleTalk {
        pub room: EchoRoom,
        pub near: Speech,
        /// `near`'s samples, silent before `near_from`.
        pub near_samples: Vec<f32>,
        /// The echo, the mic floor and the near talker.
        pub mic: Vec<f32>,
    }

    /// [`echo_room`] (RT60 0.15 s) plus a high voice at -26 dBFS from sample `near_from` on.
    pub fn double_talk(len: usize, near_from: usize, seed: u64) -> DoubleTalk {
        let room = echo_room(len, 0.15, seed);
        let near = speech_like(len, Voice::HIGH.at(-26.0), seed + 100);
        let mut near_samples = near.samples.clone();
        near_samples[..near_from.min(len)].fill(0.0);
        let mic = add(&room.mic(), &near_samples);
        DoubleTalk { room, near, near_samples, mic }
    }

    /// `count` impulses of `amp`, one every `spacing` blocks from block 1, cycling through the block phases
    /// [`IMPULSE_PHASES`]: the signal, and where each impulse is.
    pub fn impulses(count: usize, amp: f32, spacing: usize) -> (Vec<f32>, Vec<usize>) {
        let spacing = spacing.max(1);
        let at: Vec<usize> = (0..count).map(|k| (1 + k * spacing) * BLOCK + IMPULSE_PHASES[k % IMPULSE_PHASES.len()]).collect();
        let mut x = vec![0.0f32; (2 + count * spacing) * BLOCK];
        for &i in &at {
            x[i] = amp;
        }
        (x, at)
    }

    /// Silence dithered as the engine dithers its mic and reference (±1e-9, about -180 dBFS).
    pub fn dithered_silence(len: usize, seed: u32) -> Vec<f32> {
        let mut x = vec![0.0f32; len];
        Dither::new(seed).apply(&mut x, 1e-9);
        x
    }
}

#[cfg(test)]
mod tests {
    use nf_core::config::ProcessingConfig;
    use nf_core::noise::{DelayModel, ModelInfo, NoiseStage};
    use nf_core::stage::{AudioPorts, StageDescriptor};

    use super::*;

    /// Reports its inner stage's latency plus `lie`.
    struct Lying {
        inner: Box<dyn Stage>,
        lie: i32,
    }

    impl Stage for Lying {
        fn descriptor(&self) -> &StageDescriptor {
            self.inner.descriptor()
        }
        fn audio_ports(&self) -> &AudioPorts {
            self.inner.audio_ports()
        }
        fn activate(&mut self, a: &Activation) -> Result<(), StageError> {
            self.inner.activate(a)
        }
        fn latency(&self) -> u32 {
            self.inner.latency().saturating_add_signed(self.lie)
        }
        fn process(&mut self, io: ProcessIo<'_>) {
            self.inner.process(io);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
    }

    /// A noise stage around a pure delay at hop 512 (re-blocked: 992 samples), floor off, misreporting by `lie`.
    fn delay_stage(lie: i32) -> impl Fn() -> Box<dyn Stage> {
        move || {
            let c = Arc::new(ChainControls::new(&ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() }));
            let inner = Box::new(NoiseStage::new(c, Box::new(DelayModel::new(512, 1024))).expect("stage"));
            Box::new(Lying { inner, lie })
        }
    }

    /// A pure delay at hop 256 whose `latency_samples` is off by `lie`.
    struct LyingModel {
        inner: DelayModel,
        info: ModelInfo,
    }

    impl NoiseModel for LyingModel {
        fn info(&self) -> &ModelInfo {
            &self.info
        }
        fn prepare(&mut self) -> Result<(), StageError> {
            self.inner.prepare()
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
            self.inner.process_hop(input, output)
        }
    }

    fn delay_model(lie: i32) -> impl Fn() -> Box<dyn NoiseModel> {
        move || {
            let inner = DelayModel::new(256, 768);
            let info = ModelInfo { latency_samples: 768u32.saturating_add_signed(lie), ..inner.info().clone() };
            Box::new(LyingModel { inner, info })
        }
    }

    #[test]
    fn the_latency_checks_pass_the_truth_and_catch_a_lie() {
        let honest = check_latency(&delay_stage(0), Some(0.5)).expect("an honest stage passes");
        assert_eq!((honest.declared, honest.impulse_amp), (992, Some(0.5)));
        assert!((honest.gcc_offset - 992.0).abs() < 1e-3 && honest.gcc_confidence_db > GCC_MIN_CONFIDENCE_DB, "{honest:?}");
        for lie in [-1, 1, 480] {
            let e = check_latency(&delay_stage(lie), Some(0.5)).unwrap_err();
            assert!(e.contains("an impulse at"), "{e}");
            let e = check_latency(&delay_stage(lie), None).unwrap_err();
            assert!(e.contains("GCC-PHAT"), "lie {lie}: GCC-PHAT alone catches it: {e}");
        }
        let model = check_model(&delay_model(0), Some(0.25)).expect("an honest model passes");
        assert_eq!(model.declared, 512);
        for lie in [-1, 1, 256] {
            assert!(check_model(&delay_model(lie), Some(0.25)).unwrap_err().contains("hop phase"), "lie {lie}");
            assert!(check_model(&delay_model(lie), None).unwrap_err().contains("GCC-PHAT"), "lie {lie}");
        }
    }

    #[test]
    fn the_profile_check_passes_a_conforming_stage() {
        assert_eq!(check_profile(&delay_stage(0)), Ok(StageRole::Noise));
        let noise = || -> Box<dyn Stage> {
            let c = Arc::new(ChainControls::new(&ProcessingConfig::default()));
            Box::new(NoiseStage::new(c, Box::new(DelayModel::new(480, 960).with_vad())).expect("stage"))
        };
        assert_eq!(check_profile(&noise), Ok(StageRole::Noise), "with its params and state");
    }

    #[test]
    fn blocks_are_filled_with_silence_past_the_end() {
        let mut b = [1.0f32; BLOCK];
        fill(&mut b, &[0.5; 10], 4);
        assert!(b[..6].iter().all(|&v| v == 0.5) && b[6..].iter().all(|&v| v == 0.0));
        fill(&mut b, &[0.5; 10], 50);
        assert!(b.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn parity_finds_the_first_difference_and_the_error() {
        let a = [0.0f32, 1.0, 2.0, 3.0, 4.0];
        let exact = parity(&a, &a[1..], 1);
        assert_eq!((exact.compared, exact.max_abs, exact.rms_err_db, exact.first_mismatch), (4, 0.0, f32::NEG_INFINITY, None));
        let off = parity(&[1.0, 2.0, 3.5], &[1.0, 2.0, 3.0, 9.0], 0);
        assert_eq!((off.compared, off.max_abs, off.first_mismatch), (3, 0.5, Some(2)));
        assert!((off.rms_err_db - (10.0 * (0.25f32 / 14.0).log10())).abs() < 1e-4, "{}", off.rms_err_db);
        assert_eq!(parity(&a, &a, 9).compared, 0);
    }

    #[test]
    fn a_model_s_offset_is_measured_hop_by_hop() {
        for (hop, lat) in [(480, 960), (256, 768), (768, 1536)] {
            let mut x = vec![0.0f32; 10 * hop];
            x[3 * hop + 5] = 0.25;
            assert_eq!(model_offset(&mut DelayModel::new(hop, lat), &x), Some(lat as usize - hop), "hop {hop}");
        }
    }

    #[test]
    fn the_impulse_scene_covers_every_block_phase() {
        let (x, at) = scenes::impulses(8, 0.5, 3);
        assert_eq!(at.iter().map(|i| i % BLOCK).collect::<Vec<_>>(), [0, 17, 301, 479, 0, 17, 301, 479]);
        assert!(at.iter().all(|&i| x[i] == 0.5) && x.iter().filter(|v| **v != 0.0).count() == 8);
        assert_eq!(x.len() % BLOCK, 0);
        let s = scenes::dithered_silence(BLOCK, 5);
        assert!(s.iter().all(|v| v.abs() <= 1e-9) && s.iter().any(|v| *v != 0.0));
    }
}
