//! The processing chain: input meter → echo cancel → noise stage → output meter.
//!
//! Each stage sits in a slot that is off (bypassed: a copy, no stage CPU, no delay, TODO #15), on,
//! in measurement passthrough (a delay line of the stage's latency stands in for it, so the measured
//! delay equals the live configuration's, TODO #13), or draining (just switched off, still delayed).
//! A toggle takes effect at the next block:
//! - Between paths at the same latency (the stage and its stand-in delay line, which is fed every
//!   block in every mode), a linear crossfade over the block: aligned, so no comb, and no dip for a
//!   stage whose output is a delayed copy of its input where there's signal (RNNoise). AEC3's
//!   high-pass shifts the phase of low tones, so its crossfades dip a 100-300 Hz tone by 1-4.5 dB
//!   for that one block (1 kHz and up, noise and speech: under 0.5 dB).
//! - A stage that didn't run last block is reset, then primed with last block's input before its
//!   output is used, so its first block is processed audio rather than a fade-in from silence
//!   (review DSP-1). One block covers a latency up to [`BLOCK`] (RNNoise 480, AEC3 430). A longer
//!   one (a model pack on its inference worker: 2400) is still in its start-up region after the
//!   prime, so the slot stays on its delay line, which has the same latency, for the stage's
//!   `ceil(latency / BLOCK) - 1` further start-up blocks while the stage runs (the switch-on block
//!   among them), and then crossfades into it as from passthrough (`Mode::Starting`): no dip and
//!   no silence at a switch-on.
//! - Switching on from off raises the latency from 0 to the stage's: a dip splice (the old path out
//!   over 2.5 ms, then the new in over 2.5 ms). Overlapping the two misaligned copies would comb
//!   (review DSP-6).
//! - Switching off crossfades into the delay line and stops running the stage at once. The slot then
//!   drains: the latency drops later, with a dip splice, at the first block where both the delayed
//!   and the undelayed signal are below -50 dBFS, or after 1.5 s; or at the block where measurement
//!   passthrough comes on, so a stage switched off just before a latency test can't drop its
//!   latency at a quiet probe gap in the middle of it. [`Chain::latency`] counts a draining stage.
//!
//! The stages are CLAP-shaped (`stage`, TODO 17), and the chain is their host:
//! - [`Chain::activate`] checks each stage against the host profile ([`check_host_profile`]), refuses one in the wrong
//!   slot, activates it at [`Activation::HOST`], and caches its latency (fixed until the next activate) and its input
//!   count.
//! - **Ports:** the chain gets the mic and the speaker reference (already on the mic's clock: the timeline, the slip
//!   resampler and the dither have run). Each slot's main input is the previous slot's output (the AEC slot's, the mic);
//!   a stage with a second input port gets the reference on it, the v0 host's only sidechain source. That reference is the
//!   same block in every slot, not delayed by the slots upstream: the engine's lead (12-18 ms) is set up for the AEC slot,
//!   whose main input is the mic. A later slot's main input lags the mic by the upstream latency (430 samples with AEC3
//!   on, 0 with it off, changing as it drains), so its sidechain leads by that much more. No v0 stage takes one there;
//!   whichever stage first does decides whether the chain delays it.
//! - **Report:** after the slots run, when the noise filter runs live (on, not in passthrough), [`ChainReport::noise`] is
//!   read from the noise stage's extensions (`speech-probability/0`, `gain-adjustment-metering/0`; NaN where it has
//!   none). Every branch that runs it live processes this block last, after any reset and priming.
//! - Bypass is the host's: `enabled` and `passthrough` are slot state, not stage params.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::controls::ChainControls;
use crate::dsp::delay::DelayLine;
use crate::dsp::fade::{crossfade_into, dip_splice_into};
use crate::dsp::meter::{Meter, MeterReading, mean_square};
use crate::stage::{Activation, AecStats, ProcessIo, Stage, StageError, StageReport, StageRole, check_host_profile};
use crate::{BLOCK, Block};

/// Largest stage latency the passthrough delay lines can stand in for.
pub const MAX_STAGE_LATENCY: usize = 4096;
/// A draining slot drops its latency at a block quieter than this mean square (-50 dBFS)...
const QUIET_MEAN_SQUARE: f32 = 1e-5;
/// ... or after this many blocks (1.5 s).
const DRAIN_TIMEOUT_BLOCKS: u32 = 150;

/// One block's results.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChainReport {
    /// Raw input meter (`None` when meters are off).
    pub input: Option<MeterReading>,
    /// Output meter (`None` when meters are off).
    pub output: Option<MeterReading>,
    /// From the noise stage this block, if it ran.
    pub noise: Option<StageReport>,
    /// Timer ticks spent in the AEC stage and in the noise stage (from the caller's clock; 0 if not run).
    pub aec_ticks: u64,
    pub noise_ticks: u64,
}

/// What a slot did last block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Off,
    On,
    Passthrough,
    /// Switched off and crossfaded into the delay line, which still delays by the stage latency;
    /// the count is blocks since the switch.
    Draining(u32),
    /// Switched on and restarted, the stage running but still in its start-up region: the output is the delay line (the
    /// same latency) for this many more blocks, then a crossfade into the stage.
    Starting(u32),
}

/// What the controls ask a slot for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Off,
    On,
    Passthrough,
}

impl Want {
    fn new(enabled: bool, passthrough: bool) -> Self {
        match (enabled, passthrough) {
            (false, _) => Want::Off,
            (true, false) => Want::On,
            (true, true) => Want::Passthrough,
        }
    }

    /// The mode to start in, on the chain's first block (no transition from anything).
    fn mode(self) -> Mode {
        match self {
            Want::Off => Mode::Off,
            Want::On => Mode::On,
            Want::Passthrough => Mode::Passthrough,
        }
    }
}

/// A stage plus its bypass state.
struct Slot {
    stage: Box<dyn Stage>,
    /// The role this slot takes.
    role: StageRole,
    /// Input blocks `process` gets: 1, or 2 with the reference on the second (ports are constant; cached again at
    /// activate).
    inputs: usize,
    /// `stage.latency()`, cached at activate.
    latency: u32,
    /// Stand-in for the stage in passthrough and while draining: a delay of the stage's latency.
    delay: DelayLine,
    mode: Mode,
    /// Last block's input and reference, to prime the stage after a reset.
    prev_in: Box<Block>,
    prev_ref: Box<Block>,
}

impl Slot {
    fn new(stage: Box<dyn Stage>, role: StageRole) -> Self {
        Self {
            inputs: stage.audio_ports().input_count().clamp(1, 2),
            stage,
            role,
            latency: 0,
            delay: DelayLine::new(MAX_STAGE_LATENCY, 0),
            mode: Mode::Off,
            prev_in: Box::new([0.0; BLOCK]),
            prev_ref: Box::new([0.0; BLOCK]),
        }
    }

    /// Checks the stage against the host profile and this slot, activates it and caches what the chain reads of it.
    fn activate(&mut self) -> Result<(), StageError> {
        let role = check_host_profile(&*self.stage)?;
        let id = self.stage.descriptor().id.clone();
        if role != self.role {
            return Err(StageError(format!("stage {id} is a {role:?} stage, not one for the {:?} slot", self.role)));
        }
        self.stage.activate(&Activation::HOST)?;
        let lat = self.stage.latency();
        if lat as usize > MAX_STAGE_LATENCY {
            return Err(StageError(format!("stage {id} latency {lat} exceeds the passthrough limit {MAX_STAGE_LATENCY}")));
        }
        self.latency = lat;
        self.inputs = self.stage.audio_ports().input_count().clamp(1, 2);
        self.delay.set_delay(lat as usize);
        Ok(())
    }

    /// The latency this slot adds now, given whether its stage is enabled.
    fn latency(&self, enabled: bool) -> u32 {
        if enabled || matches!(self.mode, Mode::Draining(_)) { self.latency } else { 0 }
    }

    /// The blocks, from the one it is switched on in, whose output a restarted stage gives from its start-up region: that
    /// region is `latency` samples from its reset, and the prime (last block's input) takes the first block of it.
    fn start_blocks(&self) -> u32 {
        (self.latency as usize).div_ceil(BLOCK).saturating_sub(1) as u32
    }
}

/// The chain. Built and activated before real-time; `process` is real-time.
pub struct Chain {
    controls: Arc<ChainControls>,
    aec: Option<Slot>,
    noise: Option<Slot>,
    input_meter: Meter,
    output_meter: Meter,
    a: Box<Block>,
    b: Box<Block>,
    c: Box<Block>,
    /// The first block has run (it starts each slot in its wanted mode, with no transition).
    started: bool,
    /// Passthrough was on last block (its rising edge ends drains).
    was_passthrough: bool,
}

impl Chain {
    /// `aec` goes in the echo-cancel slot and `noise` in the noise slot; [`Chain::activate`] checks they belong there.
    pub fn new(controls: Arc<ChainControls>, aec: Option<Box<dyn Stage>>, noise: Option<Box<dyn Stage>>) -> Self {
        Self {
            controls,
            aec: aec.map(|s| Slot::new(s, StageRole::EchoCancel)),
            noise: noise.map(|s| Slot::new(s, StageRole::Noise)),
            input_meter: Meter::new(),
            output_meter: Meter::new(),
            a: Box::new([0.0; BLOCK]),
            b: Box::new([0.0; BLOCK]),
            c: Box::new([0.0; BLOCK]),
            started: false,
            was_passthrough: false,
        }
    }

    /// Checks each stage against the host profile and its slot, activates it at the app's format ([`Activation::HOST`]) on
    /// the calling thread, and caches its latency and input count; sizes the passthrough delays. May allocate; may be
    /// called again (the stages keep their state).
    pub fn activate(&mut self) -> Result<(), StageError> {
        for slot in [self.aec.as_mut(), self.noise.as_mut()].into_iter().flatten() {
            slot.activate()?;
        }
        Ok(())
    }

    /// The chain's current stream offset in samples: the enabled stages, plus any stage still
    /// draining after being switched off (each stage's latency as cached at activate).
    pub fn latency(&self) -> u32 {
        let c = &self.controls;
        self.aec.as_ref().map_or(0, |s| s.latency(c.aec.enabled.load(Relaxed)))
            + self.noise.as_ref().map_or(0, |s| s.latency(c.noise.enabled.load(Relaxed)))
    }

    /// The live controls the chain reads each block.
    pub fn controls(&self) -> &ChainControls {
        &self.controls
    }

    /// The echo canceller's estimates (its `aec-telemetry/0`), if there is one and it publishes them. Real-time.
    pub fn aec_stats(&self) -> Option<AecStats> {
        self.aec.as_ref().and_then(|s| s.stage.aec_telemetry()).map(|t| t.aec_stats())
    }

    /// Processes one block without timing.
    #[inline]
    pub fn process(&mut self, mic: &Block, reference: &Block, out: &mut Block) -> ChainReport {
        self.process_timed(mic, reference, out, || 0)
    }

    /// Processes one block. `now` reads a monotonic tick counter (QPC in the engine) so each stage's
    /// cost can be reported; it's called four times at most.
    pub fn process_timed<F: FnMut() -> u64>(&mut self, mic: &Block, reference: &Block, out: &mut Block, mut now: F) -> ChainReport {
        let meters = self.controls.meters_enabled.load(Relaxed);
        let passthrough = self.controls.passthrough.load(Relaxed);
        let want_aec = self.controls.aec.enabled.load(Relaxed);
        let want_noise = self.controls.noise.enabled.load(Relaxed);
        let test_starts = passthrough && !self.was_passthrough;
        self.was_passthrough = passthrough;
        if !self.started {
            self.started = true;
            if let Some(s) = self.aec.as_mut() {
                s.mode = Want::new(want_aec, passthrough).mode();
            }
            if let Some(s) = self.noise.as_mut() {
                s.mode = Want::new(want_noise, passthrough).mode();
            }
        }
        let mut report = ChainReport::default();
        if meters {
            report.input = Some(self.input_meter.measure(mic));
        }

        // AEC: mic (+ reference) -> a
        let t0 = now();
        match self.aec.as_mut() {
            Some(slot) => run_slot(slot, Want::new(want_aec, passthrough), test_starts, mic, reference, &mut self.a, &mut self.c),
            None => *self.a = *mic,
        }
        let t1 = now();
        if want_aec && self.aec.is_some() {
            report.aec_ticks = t1.wrapping_sub(t0);
        }

        // Noise: a (+ the reference, if it takes one) -> b
        match self.noise.as_mut() {
            Some(slot) => {
                run_slot(slot, Want::new(want_noise, passthrough), test_starts, &self.a, reference, &mut self.b, &mut self.c);
                if want_noise && !passthrough {
                    report.noise = Some(StageReport::read(&*slot.stage));
                }
            }
            None => *self.b = *self.a,
        }
        let t2 = now();
        if want_noise && self.noise.is_some() {
            report.noise_ticks = t2.wrapping_sub(t1);
        }

        *out = *self.b;
        if meters {
            report.output = Some(self.output_meter.measure(out));
        }
        report
    }
}

/// Resets the stage and runs it once on last block's input (into `discard`), so its next output is
/// processed audio from the start.
fn restart(slot: &mut Slot, discard: &mut Block) {
    slot.stage.reset();
    let ins = [&*slot.prev_in, &*slot.prev_ref];
    slot.stage.process(ProcessIo::new(&ins[..slot.inputs], discard));
}

#[inline]
fn quiet(x: &Block) -> bool {
    mean_square(x) < QUIET_MEAN_SQUARE
}

/// Runs one slot for a block, handling on/off and passthrough transitions (see the module notes).
/// `scratch` holds the old path's output during a transition. `test_starts`: passthrough comes on
/// this block (a latency test), so a draining slot drops its latency now rather than at a quiet
/// block mid-test. A stage with two input ports gets `reference` on the second, as the chain got it (module notes).
fn run_slot(slot: &mut Slot, want: Want, test_starts: bool, input: &Block, reference: &Block, out: &mut Block, scratch: &mut Block) {
    let ins = [input, reference];
    let ins = &ins[..slot.inputs];
    slot.mode = match (slot.mode, want) {
        // Steady states. The delay is fed in every one, so it is always primed.
        (Mode::On, Want::On) => {
            slot.stage.process(ProcessIo::new(ins, out));
            slot.delay.process(input, scratch);
            Mode::On
        }
        (Mode::Passthrough | Mode::Draining(_), Want::Passthrough) => {
            // A draining slot is already on the delay path: nothing to fade.
            slot.delay.process(input, out);
            Mode::Passthrough
        }
        (Mode::Off, Want::Off) => {
            slot.delay.process(input, scratch);
            *out = *input;
            Mode::Off
        }
        (Mode::Draining(n), Want::Off) => {
            slot.delay.process(input, scratch);
            *out = *input;
            if test_starts || n + 1 >= DRAIN_TIMEOUT_BLOCKS || (quiet(input) && quiet(scratch)) {
                dip_splice_into(scratch, out); // delayed out, undelayed in: the latency goes
                Mode::Off
            } else {
                *out = *scratch;
                Mode::Draining(n + 1)
            }
        }
        // Same latency on both sides: crossfade.
        (Mode::On, Want::Passthrough) => {
            slot.stage.process(ProcessIo::new(ins, scratch));
            slot.delay.process(input, out);
            crossfade_into(scratch, out);
            Mode::Passthrough
        }
        (Mode::On, Want::Off) => {
            slot.stage.process(ProcessIo::new(ins, scratch));
            slot.delay.process(input, out);
            crossfade_into(scratch, out);
            Mode::Draining(0)
        }
        (Mode::Passthrough, Want::Off) => {
            slot.delay.process(input, out);
            Mode::Draining(0)
        }
        (Mode::Passthrough | Mode::Draining(_), Want::On) => {
            slot.delay.process(input, scratch);
            restart(slot, out);
            slot.stage.process(ProcessIo::new(ins, out));
            match slot.start_blocks() {
                0 => {
                    crossfade_into(scratch, out);
                    Mode::On
                }
                // Still starting up: the delay path, already playing, carries on.
                n => {
                    *out = *scratch;
                    Mode::Starting(n - 1)
                }
            }
        }
        // From no latency to the stage's: dip splice (into the delay path while the stage starts up).
        (Mode::Off, Want::On) => {
            slot.delay.process(input, scratch);
            restart(slot, out);
            slot.stage.process(ProcessIo::new(ins, out));
            let n = slot.start_blocks();
            if n > 0 {
                *out = *scratch;
            }
            dip_splice_into(input, out);
            if n == 0 { Mode::On } else { Mode::Starting(n - 1) }
        }
        // The stage's start-up, on the delay path (the same latency), then a crossfade into the stage.
        (Mode::Starting(n), Want::On) => {
            slot.delay.process(input, scratch);
            slot.stage.process(ProcessIo::new(ins, out));
            if n == 0 {
                crossfade_into(scratch, out);
                Mode::On
            } else {
                *out = *scratch;
                Mode::Starting(n - 1)
            }
        }
        // Already on the delay path: nothing to fade.
        (Mode::Starting(_), Want::Passthrough) => {
            slot.delay.process(input, out);
            Mode::Passthrough
        }
        (Mode::Starting(_), Want::Off) => {
            slot.delay.process(input, out);
            Mode::Draining(0)
        }
        (Mode::Off, Want::Passthrough) => {
            slot.delay.process(input, out);
            dip_splice_into(input, out);
            Mode::Passthrough
        }
    };
    *slot.prev_in = *input;
    if slot.inputs == 2 {
        *slot.prev_ref = *reference;
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::config::ProcessingConfig;
    use crate::dsp::fade::SPLICE_FADE;
    use crate::stage::{
        AecTelemetry, AudioPorts, GainAdjustmentMetering, MONO_IN_OUT, MONO_WITH_REFERENCE, SpeechProbability, StageDescriptor, Text,
        VENDOR, features, text,
    };

    static NOISE: StageDescriptor = StageDescriptor {
        id: text("test.scaled"),
        name: text("scaled"),
        vendor: VENDOR,
        url: text(""),
        version: text("0"),
        description: text(""),
        features: std::borrow::Cow::Borrowed(&[features::DENOISE]),
    };

    /// [`NOISE`] with these tags instead.
    fn descriptor(features: &[Text]) -> StageDescriptor {
        StageDescriptor { features: features.to_vec().into(), ..NOISE.clone() }
    }

    /// A test stage: scales by `gain` and delays by `lat` samples; counts resets and calls.
    struct Scaled {
        line: DelayLine,
        lat: u32,
        gain: f32,
        resets: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    impl Stage for Scaled {
        fn descriptor(&self) -> &StageDescriptor {
            &NOISE
        }
        fn audio_ports(&self) -> &AudioPorts {
            &MONO_IN_OUT
        }
        fn activate(&mut self, a: &Activation) -> Result<(), StageError> {
            a.require_host("scaled")
        }
        fn latency(&self) -> u32 {
            self.lat
        }
        fn process(&mut self, io: ProcessIo<'_>) {
            self.calls.fetch_add(1, Relaxed);
            self.line.process(io.main(), io.output);
            for s in io.output.iter_mut() {
                *s *= self.gain;
            }
        }
        fn reset(&mut self) {
            self.line.reset();
            self.resets.fetch_add(1, Relaxed);
        }
    }

    /// Each block's first sample on the main input and on the second, if any.
    type Seen = Arc<std::sync::Mutex<Vec<(f32, Option<f32>)>>>;

    /// Records what it was given on each port, publishes every extension (or none), and can change its latency after
    /// activate (which the chain mustn't see until the next one).
    struct Wired {
        descriptor: StageDescriptor,
        ports: AudioPorts,
        extensions: bool,
        lat: Arc<AtomicUsize>,
        seen: Seen,
        blocks: f32,
    }

    impl Wired {
        fn new(features: &[Text], ports: AudioPorts, extensions: bool) -> Self {
            Self { descriptor: descriptor(features), ports, extensions, lat: Arc::default(), seen: Arc::default(), blocks: 0.0 }
        }
    }

    impl Stage for Wired {
        fn descriptor(&self) -> &StageDescriptor {
            &self.descriptor
        }
        fn audio_ports(&self) -> &AudioPorts {
            &self.ports
        }
        fn activate(&mut self, _: &Activation) -> Result<(), StageError> {
            Ok(())
        }
        fn latency(&self) -> u32 {
            self.lat.load(Relaxed) as u32
        }
        fn process(&mut self, io: ProcessIo<'_>) {
            self.blocks += 1.0;
            self.seen.lock().unwrap().push((io.main()[0], io.aux(1).map(|r| r[0])));
            *io.output = *io.main();
        }
        fn reset(&mut self) {}
        fn gain_adjustment(&self) -> Option<&dyn GainAdjustmentMetering> {
            if self.extensions { Some(self) } else { None }
        }
        fn speech_probability(&self) -> Option<&dyn SpeechProbability> {
            if self.extensions { Some(self) } else { None }
        }
        fn aec_telemetry(&self) -> Option<&dyn AecTelemetry> {
            if self.extensions { Some(self) } else { None }
        }
    }

    impl GainAdjustmentMetering for Wired {
        fn gain_adjustment_db(&self) -> f32 {
            -self.blocks
        }
    }

    impl SpeechProbability for Wired {
        fn speech_probability(&self) -> f32 {
            self.blocks / 100.0
        }
    }

    impl AecTelemetry for Wired {
        fn aec_stats(&self) -> AecStats {
            AecStats { echo_delay_ms: self.blocks, erle_db: 2.0 * self.blocks }
        }
    }

    /// Blocks whose first sample is `v` (the mic) or `-v` (the reference), so each port's block can be told apart.
    fn feed_marked(chain: &mut Chain, from: usize, to: usize) {
        let mut out = [0.0f32; BLOCK];
        for b in from..to {
            chain.process(&[b as f32; BLOCK], &[-(b as f32); BLOCK], &mut out);
        }
    }

    #[test]
    fn a_stage_in_the_wrong_slot_is_refused() {
        let c = Arc::new(ChainControls::new(&ProcessingConfig::default()));
        let wired = |f: &[Text], p| -> Box<dyn Stage> { Box::new(Wired::new(f, p, false)) };
        let refuse = |aec: Option<Box<dyn Stage>>, noise: Option<Box<dyn Stage>>, why: &str| {
            let e = Chain::new(c.clone(), aec, noise).activate().unwrap_err();
            assert!(e.0.contains(why), "{e} (wanted {why:?})");
        };
        refuse(None, Some(wired(&[features::ECHO_CANCEL], MONO_WITH_REFERENCE.clone())), "EchoCancel stage, not one for the Noise slot");
        refuse(Some(wired(&[features::DENOISE], MONO_IN_OUT.clone())), None, "Noise stage, not one for the EchoCancel slot");
        refuse(None, Some(wired(&[], MONO_IN_OUT.clone())), "Effect stage");
        refuse(Some(wired(&[features::ECHO_CANCEL], MONO_IN_OUT.clone())), None, "non-main input for the reference");
        let mut ok = Chain::new(
            c.clone(),
            Some(wired(&[features::ECHO_CANCEL], MONO_WITH_REFERENCE.clone())),
            Some(wired(&[features::DENOISE], MONO_IN_OUT.clone())),
        );
        assert!(ok.activate().is_ok());
    }

    #[test]
    fn each_port_gets_its_block_and_priming_replays_both() {
        // The AEC slot gets the mic and the reference; the noise slot the AEC's output, and the reference only if it has a
        // second port. Switched back on, each is primed with the previous block on every port it has.
        for noise_ports in [&MONO_IN_OUT, &MONO_WITH_REFERENCE] {
            let c = Arc::new(ChainControls::new(&ProcessingConfig { aec_enabled: true, ..ProcessingConfig::default() }));
            let aec = Wired::new(&[features::ECHO_CANCEL], MONO_WITH_REFERENCE.clone(), false);
            let noise = Wired::new(&[features::DENOISE], noise_ports.clone(), false);
            let (aec_seen, noise_seen) = (aec.seen.clone(), noise.seen.clone());
            let mut chain = Chain::new(c.clone(), Some(Box::new(aec)), Some(Box::new(noise)));
            chain.activate().unwrap();
            feed_marked(&mut chain, 1, 3);
            c.aec.enabled.store(false, Relaxed);
            c.noise.enabled.store(false, Relaxed);
            feed_marked(&mut chain, 3, 6);
            c.aec.enabled.store(true, Relaxed);
            c.noise.enabled.store(true, Relaxed);
            feed_marked(&mut chain, 6, 7);
            let reference = |b: f32| (noise_ports.inputs.len() == 2).then_some(-b);
            // Blocks 1, 2 live; 3 the switch-off block (still run, into the delay path); 6 primed with 5, then live.
            assert_eq!(*aec_seen.lock().unwrap(), [
                (1.0, Some(-1.0)),
                (2.0, Some(-2.0)),
                (3.0, Some(-3.0)),
                (5.0, Some(-5.0)),
                (6.0, Some(-6.0))
            ]);
            let n: Vec<_> = [1.0, 2.0, 3.0, 5.0, 6.0].map(|b| (b, reference(b))).into();
            assert_eq!(*noise_seen.lock().unwrap(), n, "{} noise ports", noise_ports.inputs.len());
        }
    }

    #[test]
    fn the_report_and_the_aec_stats_come_from_the_extensions() {
        for extensions in [true, false] {
            let c = Arc::new(ChainControls::new(&ProcessingConfig { aec_enabled: true, ..ProcessingConfig::default() }));
            let aec = Wired::new(&[features::ECHO_CANCEL], MONO_WITH_REFERENCE.clone(), extensions);
            let noise = Wired::new(&[features::DENOISE], MONO_IN_OUT.clone(), extensions);
            let mut chain = Chain::new(c.clone(), Some(Box::new(aec)), Some(Box::new(noise)));
            chain.activate().unwrap();
            let mut out = [0.0f32; BLOCK];
            let r = chain.process(&[0.1; BLOCK], &[0.0; BLOCK], &mut out);
            let n = r.noise.expect("the noise stage ran live");
            if extensions {
                assert_eq!((n.speech_probability, n.gain_reduction_db), (0.01, -1.0), "this block's values");
                assert_eq!(chain.aec_stats(), Some(AecStats { echo_delay_ms: 1.0, erle_db: 2.0 }));
            } else {
                assert!(n.speech_probability.is_nan() && n.gain_reduction_db.is_nan());
                assert_eq!(chain.aec_stats(), None);
            }
            c.passthrough.store(true, Relaxed);
            assert_eq!(chain.process(&[0.1; BLOCK], &[0.0; BLOCK], &mut out).noise, None, "not in passthrough");
        }
    }

    #[test]
    fn the_latency_is_cached_at_activate() {
        let c = Arc::new(ChainControls::new(&ProcessingConfig::default()));
        let noise = Wired::new(&[features::DENOISE], MONO_IN_OUT.clone(), false);
        let lat = noise.lat.clone();
        lat.store(300, Relaxed);
        let mut chain = Chain::new(c, None, Some(Box::new(noise)));
        chain.activate().unwrap();
        lat.store(200, Relaxed);
        assert_eq!(chain.latency(), 300, "fixed until the next activate");
        chain.activate().unwrap();
        assert_eq!(chain.latency(), 200);
        lat.store(MAX_STAGE_LATENCY + 1, Relaxed);
        assert!(chain.activate().unwrap_err().0.contains("exceeds the passthrough limit"));
    }

    struct Rig {
        chain: Chain,
        controls: Arc<ChainControls>,
        resets: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    fn rig(lat: u32, gain: f32, cfg: &ProcessingConfig) -> Rig {
        let controls = Arc::new(ChainControls::new(cfg));
        let resets = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let stage =
            Scaled { line: DelayLine::new(MAX_STAGE_LATENCY, lat as usize), lat, gain, resets: resets.clone(), calls: calls.clone() };
        let mut chain = Chain::new(controls.clone(), None, Some(Box::new(stage)));
        chain.activate().unwrap();
        Rig { chain, controls, resets, calls }
    }

    /// The half-gain stage most tests use (its output is told apart from the bypass by level).
    fn chain_counting(lat: u32, cfg: &ProcessingConfig) -> (Chain, Arc<ChainControls>, Arc<AtomicUsize>) {
        let r = rig(lat, 0.5, cfg);
        (r.chain, r.controls, r.resets)
    }

    fn chain_with(lat: u32, cfg: &ProcessingConfig) -> (Chain, Arc<ChainControls>) {
        let (chain, controls, _) = chain_counting(lat, cfg);
        (chain, controls)
    }

    /// Runs `blocks` blocks of the constant `level` and returns the output.
    fn feed(chain: &mut Chain, level: f32, blocks: usize) -> Vec<f32> {
        let mut out = [0.0f32; BLOCK];
        let mut all = Vec::new();
        for _ in 0..blocks {
            chain.process(&[level; BLOCK], &[0.0; BLOCK], &mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    fn max_abs(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn passthrough_keeps_the_stage_latency_and_unity_gain() {
        let cfg = ProcessingConfig::default();
        let (mut chain, controls) = chain_with(480, &cfg);
        controls.passthrough.store(true, Relaxed);
        let reference = [0.0f32; BLOCK];
        let mut out = [0.0f32; BLOCK];
        let mut collected = Vec::new();
        for blk in 0..6 {
            let mut x = [0.0f32; BLOCK];
            if blk == 2 {
                x[10] = 1.0;
            }
            chain.process(&x, &reference, &mut out);
            collected.extend_from_slice(&out);
        }
        // The impulse at 2*480+10 must come out 480 later, at gain 1.
        let idx = collected.iter().position(|v| v.abs() > 0.5).expect("impulse passes");
        assert_eq!(idx, 2 * BLOCK + 10 + 480);
        assert!((collected[idx] - 1.0).abs() < 1e-6);
        assert_eq!(chain.latency(), 480);
    }

    /// verify-dsp NEW-2: a stage switched off just before a latency test (passthrough on) drops its
    /// latency at the test's first block, not at the first quiet probe gap mid-test, so every
    /// repetition measures the same delay.
    #[test]
    fn a_drain_ends_when_a_latency_test_starts() {
        let cfg = ProcessingConfig::default();
        let (mut chain, controls) = chain_with(480, &cfg);
        feed(&mut chain, 0.5, 3);
        controls.noise.enabled.store(false, Relaxed);
        feed(&mut chain, 0.5, 2);
        assert_eq!(chain.latency(), 480, "draining on loud input");
        controls.passthrough.store(true, Relaxed);
        feed(&mut chain, 0.5, 1);
        assert_eq!(chain.latency(), 0, "dropped at the test's first block");
        feed(&mut chain, 0.0, 5);
        assert_eq!(chain.latency(), 0);
    }

    #[test]
    fn off_bypasses_with_no_delay_and_on_resets() {
        let cfg = ProcessingConfig { noise_enabled: false, ..ProcessingConfig::default() };
        let (mut chain, controls) = chain_with(480, &cfg);
        let reference = [0.0f32; BLOCK];
        let mut x = [0.0f32; BLOCK];
        x[5] = 1.0;
        let mut out = [0.0f32; BLOCK];
        chain.process(&x, &reference, &mut out);
        assert_eq!(out, x, "bypass is a copy");
        assert_eq!(chain.latency(), 0);
        controls.noise.enabled.store(true, Relaxed);
        chain.process(&[0.0; BLOCK], &reference, &mut out); // transition block
        chain.process(&x, &reference, &mut out);
        assert_eq!(chain.latency(), 480);
    }

    #[test]
    fn turning_on_resets_the_stage() {
        let cfg = ProcessingConfig { noise_enabled: false, ..ProcessingConfig::default() };
        let (mut chain, controls, resets) = chain_counting(480, &cfg);
        feed(&mut chain, 1.0, 2);
        assert_eq!(resets.load(Relaxed), 0);
        controls.noise.enabled.store(true, Relaxed);
        feed(&mut chain, 1.0, 2);
        assert_eq!(resets.load(Relaxed), 1);
    }

    #[test]
    fn leaving_passthrough_brings_back_no_stale_audio() {
        let (mut chain, controls, resets) = chain_counting(480, &ProcessingConfig::default());
        // The stage runs on ones, so its delay line is full of them; then passthrough runs on zeros.
        feed(&mut chain, 1.0, 3);
        controls.passthrough.store(true, Relaxed);
        feed(&mut chain, 0.0, 4);
        let before = resets.load(Relaxed);
        // Leaving passthrough: the stage must not replay the ones it held from before the test.
        controls.passthrough.store(false, Relaxed);
        let out = feed(&mut chain, 0.0, 3);
        assert!(out.iter().all(|&v| v == 0.0), "stale audio after passthrough: max {}", max_abs(&out));
        assert_eq!(resets.load(Relaxed), before + 1);
    }

    #[test]
    fn switched_on_during_passthrough_the_stage_still_resets_when_it_resumes() {
        let (mut chain, controls, _) = chain_counting(480, &ProcessingConfig::default());
        feed(&mut chain, 1.0, 3);
        controls.noise.enabled.store(false, Relaxed);
        feed(&mut chain, 1.0, 1); // the switch-off block still runs the stage: it holds ones
        feed(&mut chain, 0.0, 2);
        controls.passthrough.store(true, Relaxed);
        controls.noise.enabled.store(true, Relaxed); // on, but standing in as a delay
        feed(&mut chain, 0.0, 3);
        controls.passthrough.store(false, Relaxed);
        let out = feed(&mut chain, 0.0, 3);
        assert!(out.iter().all(|&v| v == 0.0), "audio from before the stage was switched off came back");
    }

    #[test]
    fn nothing_older_than_the_latency_comes_back_after_switching_on() {
        // The stage and its delay run on ones, then the stage is switched off and drains on zeros.
        // Switched on (on zeros) and straight into passthrough, neither may replay those ones: the
        // priming block is the one just before the switch, which is zeros.
        let (mut chain, controls, _) = chain_counting(480, &ProcessingConfig::default());
        feed(&mut chain, 1.0, 3);
        controls.noise.enabled.store(false, Relaxed);
        feed(&mut chain, 0.0, 3);
        assert_eq!(chain.latency(), 0, "drained on the quiet blocks");
        controls.noise.enabled.store(true, Relaxed);
        let on = feed(&mut chain, 0.0, 1);
        assert!(on.iter().all(|&v| v == 0.0), "stale stage: max {}", max_abs(&on));
        controls.passthrough.store(true, Relaxed);
        let out = feed(&mut chain, 0.0, 3);
        assert!(out.iter().all(|&v| v == 0.0), "stale delay: max {}", max_abs(&out));
    }

    #[test]
    fn a_stage_switched_on_is_primed_with_the_previous_block() {
        // Review DSP-1: a reset stage's first output was a fade from silence (RNNoise's 480 samples
        // of latency are a whole block). Primed, its first block is already the delayed signal. A
        // longer latency (a model pack on its worker: 1920 at depth 1, 2400 at 2) plays the delay
        // line through the stage's start-up instead, then crossfades: the same delayed signal.
        for lat in [480u32, 430, 1000, 1920, 2400] {
            let cfg = ProcessingConfig { noise_enabled: false, ..ProcessingConfig::default() };
            let mut r = rig(lat, 1.0, &cfg);
            let x = sine(16 * BLOCK);
            let mut y = vec![0.0f32; x.len()];
            for (b, (xi, yo)) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0).enumerate() {
                r.controls.noise.enabled.store(b >= 4, Relaxed);
                r.chain.process(xi, &[0.0; BLOCK], yo);
            }
            let l = lat as usize;
            // After the splice's fade-in, the switch block is exactly the input `lat` samples earlier.
            for n in (4 * BLOCK + 2 * SPLICE_FADE).max(l)..y.len() {
                assert_eq!(y[n], x[n - l], "latency {lat}, sample {n}");
            }
        }
    }

    #[test]
    fn switching_off_stops_the_stage_and_drains_at_the_first_quiet_block() {
        let mut r = rig(480, 1.0, &ProcessingConfig::default());
        let x = sine(10 * BLOCK);
        let mut y = [0.0f32; BLOCK];
        for xi in x.as_chunks::<BLOCK>().0 {
            r.chain.process(xi, &[0.0; BLOCK], &mut y);
        }
        r.controls.noise.enabled.store(false, Relaxed);
        let calls = r.calls.load(Relaxed);
        // Loud: still delayed (latency counted), the stage no longer runs after the switch block.
        let mut out = Vec::new();
        for xi in x.as_chunks::<BLOCK>().0 {
            r.chain.process(xi, &[0.0; BLOCK], &mut y);
            out.extend_from_slice(&y);
            assert_eq!(r.chain.latency(), 480, "draining");
        }
        assert_eq!(r.calls.load(Relaxed), calls + 1, "only the switch block ran the stage");
        for n in BLOCK..out.len() {
            assert_eq!(out[n], x[(n + x.len() - 480) % x.len()], "delayed through the drain at {n}");
        }
        // Quiet (-60 dBFS): the first block whose delayed side is quiet too drops the latency.
        let q = [0.001f32; BLOCK];
        r.chain.process(&q, &[0.0; BLOCK], &mut y); // the delayed side still holds the tone
        assert_eq!(r.chain.latency(), 480);
        r.chain.process(&q, &[0.0; BLOCK], &mut y);
        assert_eq!(r.chain.latency(), 0, "dropped at the quiet block");
        r.chain.process(&q, &[0.0; BLOCK], &mut y);
        assert_eq!(y, q, "then bypassed exactly");
    }

    // ---- Transition quality: no click, no comb (review DSP-1, DSP-6) -------------------------

    /// A 173 Hz sine at 0.3: at 10 ms of offset it is 263° out of phase, so overlapping a copy
    /// delayed by a block nearly cancels mid-fade.
    fn sine(n: usize) -> Vec<f32> {
        (0..n).map(|i| (0.3 * (std::f64::consts::TAU * 173.0 * i as f64 / 48_000.0).sin()) as f32).collect()
    }

    /// Uniform white noise in ±0.3 (xorshift).
    fn noise(n: usize) -> Vec<f32> {
        let mut s = 0x9E37_79B9u32;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                0.3 * ((s as f32 / u32::MAX as f32) * 2.0 - 1.0)
            })
            .collect()
    }

    fn max_step(x: &[f32], r: Range<usize>) -> f32 {
        (r.start.max(1)..r.end.min(x.len())).map(|i| (x[i] - x[i - 1]).abs()).fold(0.0, f32::max)
    }

    /// The RMS of 10 ms windows starting in `r`, every 1 ms: the lowest, in dB against `reference`.
    fn worst_window_db(x: &[f32], r: Range<usize>, reference: f32) -> f32 {
        let rms = |s: usize| (x[s..s + BLOCK].iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / BLOCK as f64).sqrt();
        let worst = r.step_by(48).filter(|&s| s + BLOCK <= x.len()).map(rms).fold(f64::INFINITY, f64::min);
        (20.0 * (worst / f64::from(reference)).log10()) as f32
    }

    /// What one transition is allowed: a dip splice has a 5 ms V in it; an aligned crossfade between
    /// identical paths is nothing at all.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Kind {
        Splice,
        Crossfade,
    }

    #[test]
    fn transitions_have_no_click_and_no_comb_dip() {
        // (block, noise enabled, passthrough, what the block does)
        let schedule: &[(usize, bool, bool, Kind)] = &[
            (20, true, false, Kind::Splice),      // stage on: latency 0 -> L
            (40, true, true, Kind::Crossfade),    // into passthrough
            (60, true, false, Kind::Crossfade),   // out of passthrough (restart + prime)
            (80, false, false, Kind::Crossfade),  // stage off: into the delay path
            (230, false, false, Kind::Splice),    // the drain times out (the signal never goes quiet)
            (250, true, false, Kind::Splice),     // on again
            (270, false, false, Kind::Crossfade), // off ...
            (275, true, false, Kind::Crossfade),  // ... and on again mid-drain
            (290, true, true, Kind::Crossfade),   // passthrough ...
            (295, false, true, Kind::Crossfade),  // ... stage off while in it: drains, no audible change
            (300, true, true, Kind::Crossfade),   // ... back on: still the same delay path
            (305, true, false, Kind::Crossfade),  // out of passthrough
        ];
        let blocks = 320;
        for lat in [480u32, 430, 1920, 2400] {
            // A switch-on crossfades from the delay line once the stage's start-up has passed: up to this many blocks on.
            let start = (lat as usize).div_ceil(BLOCK);
            for (name, x) in [("sine", sine(blocks * BLOCK)), ("noise", noise(blocks * BLOCK))] {
                let cfg = ProcessingConfig { noise_enabled: false, ..ProcessingConfig::default() };
                let mut r = rig(lat, 1.0, &cfg);
                let mut y = vec![0.0f32; x.len()];
                let mut latency = Vec::new();
                for (b, (xi, yo)) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0).enumerate() {
                    if let Some(&(_, on, pt, _)) = schedule.iter().find(|e| e.0 == b) {
                        r.controls.noise.enabled.store(on, Relaxed);
                        r.controls.passthrough.store(pt, Relaxed);
                    }
                    r.chain.process(xi, &[0.0; BLOCK], yo);
                    latency.push(r.chain.latency() as usize);
                }
                let peak = max_abs(&x);
                let natural = max_step(&x, 0..x.len());
                let floor_rms = (0..x.len() - BLOCK)
                    .step_by(48)
                    .map(|s| (x[s..s + BLOCK].iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / BLOCK as f64).sqrt());
                let quietest = floor_rms.fold(f64::INFINITY, f64::min) as f32;
                for &(b, _, _, kind) in schedule {
                    let around = (b - 1) * BLOCK..(b + start + 1) * BLOCK;
                    let step = max_step(&y, around.clone());
                    let dip = worst_window_db(&y, around.start - BLOCK..around.end, quietest);
                    let what = format!("{name}, latency {lat}, block {b} ({kind:?})");
                    eprintln!("{what}: max step {:.2}x natural, worst 10 ms window {dip:+.2} dB", step / natural);
                    match kind {
                        Kind::Crossfade => {
                            assert!(step <= natural * 1.001 + 1e-6, "{what}: step {step} vs natural {natural}");
                            assert!(dip >= -0.05, "{what}: dip {dip} dB");
                        }
                        Kind::Splice => {
                            // The fades add at most peak / SPLICE_FADE a sample to the signal's own
                            // slope. The 5 ms V costs up to 2/3 of the energy under it: about -1.8 dB
                            // in a 10 ms window on noise, -2 to -3.5 dB on a low tone by its phase.
                            // The comb the splices replace reached -7.8 dB, and the unprimed switch-on
                            // about -5.5 dB.
                            let limit = (natural + peak / SPLICE_FADE as f32) * 1.02;
                            assert!(step <= limit, "{what}: step {step} vs {limit}");
                            let floor = if name == "sine" { -3.5 } else { -2.5 };
                            assert!(dip >= floor, "{what}: dip {dip} dB");
                        }
                    }
                }
                // Between transitions the output is the input at the latency then in force, exactly.
                for b in 1..blocks {
                    if schedule.iter().any(|e| e.0 == b || e.0 + 1 == b) {
                        continue;
                    }
                    // A long stage's start-up blocks and its crossfade are on the delay path or between identical
                    // paths: exact too, so nothing else is skipped.
                    let l = latency[b - 1];
                    for n in b * BLOCK..(b + 1) * BLOCK {
                        assert_eq!(y[n], x[n - l], "{name}, latency {lat}: sample {n} at offset {l}");
                    }
                }
                assert_eq!(latency[229], lat as usize, "{name}: still draining before the timeout");
                assert_eq!(latency[230], 0, "{name}: drained at the timeout");
            }
        }
    }

    /// A model pack's layout (a 768-hop model re-blocked, two blocks behind on the inline executor: a stage latency of 2400)
    /// switched off and on again, from draining and from off: the blocks after the switch-on hold the signal (the verifier's
    /// case was three silent blocks after the crossfade, then the model's output starting hard), and it is the stage's.
    #[test]
    fn a_long_model_stage_switches_on_without_silence() {
        use crate::noise::{DEFAULT_DEPTH, DelayModel, Executor, NoiseModel, NoiseStage, Pipelined};
        for (off_blocks, from) in [(3usize, "draining"), (200, "off")] {
            let cfg = ProcessingConfig { floor_db: -100.0, ..ProcessingConfig::default() };
            let controls = Arc::new(ChainControls::new(&cfg));
            let make = Box::new(|| Ok(Box::new(DelayModel::new(768, 1536)) as Box<dyn NoiseModel>));
            let stage =
                NoiseStage::new(controls.clone(), Box::new(Pipelined::spawn(make, Executor::Inline, DEFAULT_DEPTH).unwrap())).unwrap();
            let mut chain = Chain::new(controls.clone(), None, Some(Box::new(stage)));
            chain.activate().unwrap();
            assert_eq!(chain.latency(), 2400);
            let x = sine((40 + off_blocks + 40) * BLOCK);
            let mut y = vec![0.0f32; x.len()];
            for (b, (xi, yo)) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0).enumerate() {
                controls.noise.enabled.store(!(40..40 + off_blocks).contains(&b), Relaxed);
                chain.process(xi, &[0.0; BLOCK], yo);
            }
            let on = 40 + off_blocks;
            let rms = |v: &[f32]| (v.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / v.len() as f64).sqrt();
            let delayed = |b: usize| &x[b * BLOCK - 2400..(b + 1) * BLOCK - 2400];
            // The switch-on block: a crossfade from draining (none), a dip splice from off (its 5 ms V).
            let ratio = rms(&y[on * BLOCK..(on + 1) * BLOCK]) / rms(delayed(on));
            let floor = if from == "off" { 0.6 } else { 0.999 };
            assert!(ratio >= floor, "{from}: the switch-on block at {ratio} of the delayed input's RMS");
            // Then the delay line through the stage's start-up, the crossfade into the stage and the stage's own output (the
            // input 2400 samples late): the same signal, exactly.
            for n in (on + 1) * BLOCK..y.len() {
                assert_eq!(y[n], x[n - 2400], "{from}: sample {n}");
            }
        }
    }
}
