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

/// Phase lengths at scale 1, in seconds. A cell is 52 s of audio: 15 s of silence, 6 s of speech and 15 s of silence, then 4 s
/// silent, 6 s of the far end alone and 6 s with the near end.
const SILENCE_S: f32 = 15.0;
const SPEECH_S: f32 = 6.0;
const QUIET_S: f32 = 4.0;
const FAR_S: f32 = 6.0;
const NEAR_S: f32 = 6.0;
/// The near end's speech and noise and the far end's speech, at the timing scene's levels (nf-stages' `bench.rs`): active RMS
/// in dBFS, and the pink noise's RMS.
const NEAR_DB: f32 = -26.0;
const FAR_DB: f32 = -20.0;
const NOISE_DB: f32 = -45.0;
/// The takes from the fetched test audio (`tools/fetch_test_audio.py`): a female near end, a male far end.
const NEAR_TAKE: &str = "speech_tsp_fa01";
const FAR_TAKE: &str = "speech_tsp_ma01";

/// Whose blocks the engine dithers (`pipeline.rs` dithers both today).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DitherMode {
    /// Today's: the mic's block, then the reference's, from one generator.
    MicAndReference,
    /// The proposal: the mic clean. The generator still draws the mic's block's worth and drops it, so the reference gets
    /// today's values, and the output's difference from today's is the mic dither's alone.
    ReferenceOnly,
    /// Neither (the AEC3 stage still dithers its reference itself).
    None,
}

impl DitherMode {
    pub const ALL: [DitherMode; 3] = [DitherMode::MicAndReference, DitherMode::ReferenceOnly, DitherMode::None];

    pub fn name(self) -> &'static str {
        match self {
            DitherMode::MicAndReference => "mic+ref",
            DitherMode::ReferenceOnly => "ref",
            DitherMode::None => "none",
        }
    }

    /// Dithers one block pair from `d`, as the pipeline would under this policy.
    fn apply(self, d: &mut Dither, mic: &mut [f32], reference: &mut [f32]) {
        match self {
            DitherMode::MicAndReference => {
                d.apply(mic, DITHER);
                d.apply(reference, DITHER);
            }
            DitherMode::ReferenceOnly => {
                let mut dropped = [0.0f32; BLOCK];
                d.apply(&mut dropped, DITHER);
                d.apply(reference, DITHER);
            }
            DitherMode::None => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    /// Sustained digital silence: the mic and the reference exact zero from the start.
    Silence,
    /// The scene (near-end speech and noise, the far end and its echo), then the mic and the reference exact zero: the tails.
    SpeechThenSilence,
    /// Both silent, then the far end alone (the mic exact zero, as a muted mic), then the near end too (with the echo).
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
}

impl Phase {
    pub fn blocks(&self) -> usize {
        self.end - self.start
    }
}

/// A signal: the mic and the far end (the reference, for a chain with AEC3), whole blocks, and its phases in order.
pub struct Signal {
    pub kind: SignalKind,
    pub mic: Vec<f32>,
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

/// `x` looped or cut to `len` samples, into `dst` from `at`.
fn put(dst: &mut [f32], at: usize, x: &[f32], len: usize) {
    if x.is_empty() {
        return;
    }
    dst.iter_mut().skip(at).take(len).zip(x.iter().cycle()).for_each(|(d, &s)| *d = s);
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
        let phase = |name, start, len, mic_silent| Phase { name, start, end: start + len, mic_silent };
        let phases = match kind {
            SignalKind::Silence => vec![phase("silence", 0, blocks(SILENCE_S, scale), true)],
            SignalKind::SpeechThenSilence => {
                let s = blocks(SPEECH_S, scale);
                vec![phase("speech", 0, s, false), phase("silence", s, blocks(SILENCE_S, scale), true)]
            }
            SignalKind::ReferenceThenNear => {
                let (q, f) = (blocks(QUIET_S, scale), blocks(FAR_S, scale));
                vec![phase("quiet", 0, q, true), phase("far-only", q, f, true), phase("near", q + f, blocks(NEAR_S, scale), false)]
            }
        };
        let len = phases.last().map_or(0, |p| p.end) * BLOCK;
        let (mut mic, mut far) = (vec![0.0f32; len], vec![0.0f32; len]);
        // The far end plays from its first active phase to the end of the last, and its echo reaches the mic where the mic
        // is live; a silent phase's mic and reference are exact zeros.
        let far_from = match kind {
            SignalKind::ReferenceThenNear => phases[1].start,
            _ => 0,
        };
        let live = |p: &Phase| match kind {
            SignalKind::Silence => false,
            SignalKind::SpeechThenSilence => p.name == "speech",
            SignalKind::ReferenceThenNear => p.name != "quiet",
        };
        let far_end = phases.iter().filter(|p| live(p)).map(|p| p.end).max().unwrap_or(0);
        if far_end > far_from {
            put(&mut far, far_from * BLOCK, &speech.far, (far_end - far_from) * BLOCK);
        }
        let echo = echo_of(&far);
        let noise = pink_noise(len, db_to_lin(NOISE_DB), 4);
        for p in phases.iter().filter(|p| !p.mic_silent) {
            let (a, n) = (p.start * BLOCK, p.blocks() * BLOCK);
            let mut near = vec![0.0f32; n];
            put(&mut near, 0, &speech.near, n);
            let live_mic = add(&add(&near, &noise[a..a + n]), &echo[a..a + n]);
            mic[a..a + n].copy_from_slice(&live_mic);
        }
        Self { kind, mic, far, phases }
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
        let mut d = Dither::new(PIPELINE_SEED);
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
    let mut d = Dither::new(WARM_UP_SEED);
    let (mut mic, mut reference, mut out) = ([0.0f32; BLOCK], [0.0f32; BLOCK], [0.0f32; BLOCK]);
    for _ in 0..WARM_UP_BLOCKS {
        mic.fill(0.0);
        reference.fill(0.0);
        dither.apply(&mut d, &mut mic, &mut reference);
        chain.process(&mic, &reference, &mut out);
    }
}
