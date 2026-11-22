//! The matrix's signals, in phases, and the engine's feed of them: its dither on the mic and the reference blocks as the
//! policy says, and its reference handling.

use nf_core::chain::Chain;
use nf_core::rt::Dither;
use nf_core::{BLOCK, Block, SAMPLE_RATE, db_to_lin};
use nf_stages::harness::audio::{active_rms, audio_dir, read};
use nf_stages::synth::{Voice, add, convolve, delayed, pink_noise, room_impulse_response, scaled, speech_like};

/// The engine's dither amplitude, about -180 dBFS (nf-engine's `pipeline.rs`, `DITHER`), copied: the constant is the crate's
/// own. nf-stages' `bench.rs` copies it too.
pub const DITHER: f32 = 1e-9;
/// The pipeline's one generator, the mic's block first and then the reference's (`pipeline.rs`: `Dither::new(0x2545_F491)`).
pub const PIPELINE_SEED: u32 = 0x2545_F491;
/// The engine's start-up warm-up (`audio.rs`, `warm_up`): this many silent blocks, dithered by their own generator.
pub const WARM_UP_SEED: u32 = 0x1234_5678;
pub const WARM_UP_BLOCKS: usize = 3;
/// The reseeded control's generators: today's seeds XORed with this (the golden ratio's word).
pub const RESEED: u32 = 0x9E37_79B9;

/// Phase lengths at scale 1, in seconds. A cell is 64 s of audio: 15 s of silence; 6 s of speech and 15 s of silence; then
/// 6 s of the scene, 10 s of the near end alone, 6 s of the far end alone and 6 s of both.
const SILENCE_S: f32 = 15.0;
const SPEECH_S: f32 = 6.0;
const NEAR_ONLY_S: f32 = 10.0;
const FAR_S: f32 = 6.0;
const BOTH_S: f32 = 6.0;
/// The near end's speech and noise and the far end's speech, at the timing scene's levels (nf-stages' `bench.rs`): active RMS
/// in dBFS, and the pink noise's RMS.
const NEAR_DB: f32 = -26.0;
const FAR_DB: f32 = -20.0;
const NOISE_DB: f32 = -45.0;
/// The takes from the fetched test audio (`tools/fetch_test_audio.py`): a female near end, a male far end.
const NEAR_TAKE: &str = "speech_tsp_fa01";
const FAR_TAKE: &str = "speech_tsp_ma01";

/// Whose blocks are dithered: the engine's mic and reference blocks (`pipeline.rs` dithers both today), and the AEC3 stage's
/// own reference dither (`nf_stages::Aec3Stage`, 1e-9 on every block), which every policy but "bare" keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DitherMode {
    /// Today's: the mic's block, then the reference's, from one generator.
    MicAndReference,
    /// The control: today's dither from other generator seeds (the pipeline's and the warm-up's, XORed with [`RESEED`]). Its
    /// difference from today's is what any -180 dBFS perturbation does to the output (a recurrent or adaptive stage's
    /// sensitivity), to set the other policies' differences against.
    Reseeded,
    /// The proposal: the mic clean. The generator still draws the mic's block's worth and drops it, so the reference gets
    /// today's values, and the output's difference from today's is the mic dither's alone.
    ReferenceOnly,
    /// The engine dithers neither; the AEC3 stage still dithers its reference.
    None,
    /// Nothing dithered anywhere: the engine's dither off and the AEC3 stage's too (its amplitude 0). Chains with AEC3 only
    /// (without it, "bare" is "none").
    Bare,
}

impl DitherMode {
    /// In the order a cell's policies run: today's first, for the others' difference from it.
    #[rustfmt::skip]
    pub const ALL: [DitherMode; 5] =
        [DitherMode::MicAndReference, DitherMode::Reseeded, DitherMode::ReferenceOnly, DitherMode::None, DitherMode::Bare];

    pub fn name(self) -> &'static str {
        match self {
            DitherMode::MicAndReference => "mic+ref",
            DitherMode::Reseeded => "mic+ref/seed2",
            DitherMode::ReferenceOnly => "ref",
            DitherMode::None => "none",
            DitherMode::Bare => "bare",
        }
    }

    /// Whether the policy has a cell in a chain (with AEC3 or not) on `signal`: "bare" needs AEC3 (without it, it is "none"),
    /// and the reseeded control a signal with speech (sustained silence has none to compare).
    pub fn runs(self, has_aec: bool, signal: SignalKind) -> bool {
        match self {
            DitherMode::Bare => has_aec,
            DitherMode::Reseeded => signal != SignalKind::Silence,
            _ => true,
        }
    }

    /// The AEC3 stage's reference dither amplitude: the app's, or 0 under "bare".
    pub fn stage_dither(self) -> f32 {
        if self == DitherMode::Bare { 0.0 } else { nf_stages::AEC3_REFERENCE_DITHER }
    }

    /// The pipeline's generator's seed and the warm-up's.
    pub fn seeds(self) -> (u32, u32) {
        match self {
            DitherMode::Reseeded => (PIPELINE_SEED ^ RESEED, WARM_UP_SEED ^ RESEED),
            _ => (PIPELINE_SEED, WARM_UP_SEED),
        }
    }

    /// Dithers one block pair from `d`, as the pipeline would under this policy.
    fn apply(self, d: &mut Dither, mic: &mut [f32], reference: &mut [f32]) {
        match self {
            DitherMode::MicAndReference | DitherMode::Reseeded => {
                d.apply(mic, DITHER);
                d.apply(reference, DITHER);
            }
            DitherMode::ReferenceOnly => {
                let mut dropped = [0.0f32; BLOCK];
                d.apply(&mut dropped, DITHER);
                d.apply(reference, DITHER);
            }
            DitherMode::None | DitherMode::Bare => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    /// Sustained digital silence: the mic and the reference exact zero from the start.
    Silence,
    /// The scene (near-end speech and noise, the far end and its echo), then the mic and the reference exact zero: the tails.
    SpeechThenSilence,
    /// The scene; then the far end exact zero with the near end live (near-end talk alone, a call's commonest state, and the
    /// reference silent after activity, where sonora #34's stall was); then the far end alone with the mic exact zero (a
    /// muted mic); then both.
    ReferenceThenNear,
}

impl SignalKind {
    pub const ALL: [SignalKind; 3] = [SignalKind::Silence, SignalKind::SpeechThenSilence, SignalKind::ReferenceThenNear];

    pub fn name(self) -> &'static str {
        match self {
            SignalKind::Silence => "silence",
            SignalKind::SpeechThenSilence => "speech-silence",
            SignalKind::ReferenceThenNear => "ref-near",
        }
    }
}

/// A stretch of a signal, in blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: &'static str,
    pub start: usize,
    pub end: usize,
    /// The mic is exact zero throughout (a silent stretch: its output's exact zeros are measured).
    pub mic_silent: bool,
    /// The far end is exact zero throughout, before any dither (the reference of a chain with AEC3; without AEC3 the
    /// reference is silence throughout).
    pub ref_silent: bool,
}

impl Phase {
    pub fn blocks(&self) -> usize {
        self.end - self.start
    }
}

/// A signal: the mic, the near end's speech in it alone (for the output's SI-SDR), and the far end (the reference, for a
/// chain with AEC3), whole blocks, and its phases in order.
pub struct Signal {
    pub kind: SignalKind,
    pub mic: Vec<f32>,
    pub near: Vec<f32>,
    pub far: Vec<f32>,
    pub phases: Vec<Phase>,
}

/// The near and far speech the signals are made of, at their levels, and where it came from.
pub struct Speech {
    pub near: Vec<f32>,
    pub far: Vec<f32>,
    pub source: String,
}

impl Speech {
    /// Two takes from the fetched test audio (`NF_TEST_AUDIO`, or the local cache), or else nf-stages' synthetic voices (the
    /// timing scene's), each at its level.
    pub fn load() -> Self {
        let takes = audio_dir().map(|d| (d.join(format!("{NEAR_TAKE}.wav")), d.join(format!("{FAR_TAKE}.wav"))));
        match takes {
            Some((n, f)) if n.is_file() && f.is_file() => Self {
                near: at_level(read(&n), NEAR_DB),
                far: at_level(read(&f), FAR_DB),
                source: format!("{NEAR_TAKE} near, {FAR_TAKE} far"),
            },
            _ => {
                eprintln!("no {NEAR_TAKE} and {FAR_TAKE} in the test audio: the synthetic voices instead");
                let len = 20 * SAMPLE_RATE as usize;
                Self {
                    near: speech_like(len, Voice::HIGH.at(NEAR_DB), 3).samples,
                    far: speech_like(len, Voice::LOW.at(FAR_DB), 1).samples,
                    source: "synthetic (nf_stages::synth::speech_like)".into(),
                }
            }
        }
    }
}

/// `x` scaled to an active RMS of `db` dBFS.
fn at_level(x: Vec<f32>, db: f32) -> Vec<f32> {
    let r = active_rms(&x);
    if r > 0.0 { scaled(&x, db_to_lin(db) / r) } else { x }
}

/// `x` looped from its sample `from` (modulo its length) into all of `dst`.
fn put(dst: &mut [f32], x: &[f32], from: usize) {
    if x.is_empty() {
        return;
    }
    dst.iter_mut().zip(x.iter().cycle().skip(from % x.len())).for_each(|(d, &s)| *d = s);
}

/// Blocks in `seconds` × `scale` (at least one).
fn blocks(seconds: f32, scale: f32) -> usize {
    ((seconds * scale * SAMPLE_RATE as f32 / BLOCK as f32).round() as usize).max(1)
}

/// The far end's echo at the mic: a room's (RT60 0.15 s), 40 ms late, -6 dB (the timing scene's, nf-stages' `bench.rs`).
fn echo_of(far: &[f32]) -> Vec<f32> {
    let rir = room_impulse_response(9600, 0.15, 2);
    let e = convolve(far, &rir);
    scaled(&delayed(e.get(..far.len()).unwrap_or(&e), 1920), 0.5)
}

impl Signal {
    /// The `kind` of signal from `speech`, its phases `scale` times their full length.
    pub fn new(kind: SignalKind, speech: &Speech, scale: f32) -> Self {
        // (name, seconds, the mic silent, the far end silent), in order.
        let plan: &[(&'static str, f32, bool, bool)] = match kind {
            SignalKind::Silence => &[("silence", SILENCE_S, true, true)],
            SignalKind::SpeechThenSilence => &[("speech", SPEECH_S, false, false), ("silence", SILENCE_S, true, true)],
            SignalKind::ReferenceThenNear => &[
                ("scene", SPEECH_S, false, false),
                ("near-only", NEAR_ONLY_S, false, true),
                ("far-only", FAR_S, true, false),
                ("both", BOTH_S, false, false),
            ],
        };
        let mut phases = Vec::with_capacity(plan.len());
        let mut start = 0;
        for &(name, seconds, mic_silent, ref_silent) in plan {
            let end = start + blocks(seconds, scale);
            phases.push(Phase { name, start, end, mic_silent, ref_silent });
            start = end;
        }
        let len = start * BLOCK;
        let (mut mic, mut near, mut far) = (vec![0.0f32; len], vec![0.0f32; len], vec![0.0f32; len]);
        // Each talker carries on from where its last phase stopped. The far end plays where it isn't silent, and its echo
        // (the room's tail too) reaches the mic where the mic is live, with the near end and the pink noise; a silent phase's
        // mic or far end is exact zeros.
        let (mut at_near, mut at_far) = (0, 0);
        for p in phases.iter().filter(|p| !p.ref_silent) {
            let s = p.start * BLOCK..p.end * BLOCK;
            put(&mut far[s.clone()], &speech.far, at_far);
            at_far += s.len();
        }
        let echo = echo_of(&far);
        let noise = pink_noise(len, db_to_lin(NOISE_DB), 4);
        for p in phases.iter().filter(|p| !p.mic_silent) {
            let s = p.start * BLOCK..p.end * BLOCK;
            put(&mut near[s.clone()], &speech.near, at_near);
            at_near += s.len();
            let live = add(&add(&near[s.clone()], &noise[s.clone()]), &echo[s.clone()]);
            mic[s].copy_from_slice(&live);
        }
        Self { kind, mic, near, far, phases }
    }

    pub fn blocks(&self) -> usize {
        self.mic.len() / BLOCK
    }
}

/// What the chain is fed, block by block: the mic, and the reference (the far end with AEC3 on, which opens the loopback;
/// zeros with it off, as the engine serves without a reference stream), each dithered as the policy says.
pub struct Feed {
    pub mic: Vec<f32>,
    pub reference: Vec<f32>,
}

impl Feed {
    pub fn new(signal: &Signal, dither: DitherMode, has_reference: bool) -> Self {
        let mut mic = signal.mic.clone();
        let mut reference = if has_reference { signal.far.clone() } else { vec![0.0; signal.far.len()] };
        let mut d = Dither::new(dither.seeds().0);
        for (m, r) in mic.as_chunks_mut::<BLOCK>().0.iter_mut().zip(reference.as_chunks_mut::<BLOCK>().0) {
            dither.apply(&mut d, m, r);
        }
        Self { mic, reference }
    }

    pub fn blocks(&self) -> usize {
        self.mic.len().min(self.reference.len()) / BLOCK
    }

    #[inline]
    pub fn block(&self, b: usize) -> (&Block, &Block) {
        (&self.mic.as_chunks::<BLOCK>().0[b], &self.reference.as_chunks::<BLOCK>().0[b])
    }
}

/// The engine's start-up warm-up (`audio.rs`, `warm_up`): three silent blocks through the chain, dithered by their own
/// generator as the policy says (a clean mic would be clean here too).
pub fn warm_up(chain: &mut Chain, dither: DitherMode) {
    let mut d = Dither::new(dither.seeds().1);
    let (mut mic, mut reference, mut out) = ([0.0f32; BLOCK], [0.0f32; BLOCK], [0.0f32; BLOCK]);
    for _ in 0..WARM_UP_BLOCKS {
        mic.fill(0.0);
        reference.fill(0.0);
        dither.apply(&mut d, &mut mic, &mut reference);
        chain.process(&mic, &reference, &mut out);
    }
}
