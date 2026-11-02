//! The echo test's echo delay (`quality --test echo --echo-delay MS`, TODO 14): where the far voice is played so that its echo
//! reaches the mic the asked delay after the far end's loopback taps the far voice, by both streams' QPC stamps, as a speaker's
//! output latency and the air would put it; and where the echo landed. Pure functions of the plan and a chain recording, so
//! they're unit-tested on fake ones; the command (`commands/quality.rs`) plays the take and runs the calibration beside it.
//!
//! - **The delay asked** is the echo's delay after its reference's tap: a real speaker's echo follows the loopback tap by the
//!   output's latency and the air, often only 10-30 ms, hence the 25 ms default. The engine serves the reference with a lead (the
//!   mic block's capture time less the tap time of the reference sample served with it; 12-14 ms on the cables), so AEC3 sees
//!   the echo the delay less the lead after its reference, and an echo path shorter than the lead can't be cancelled.
//! - **Why calibrate in each take.** The rig adds an offset of its own: an echo pre-mixed into the probe's cable reaches the mic
//!   `rig` after the far end's loopback tap of the same audio, both tracks playing it at once. On the dev VM's cables it's
//!   negative (VB-CABLE's loopback stamps run late) and it doesn't hold from take to take: in six 60 s takes with cable B as the
//!   far end (2026-11-01, `quality --save`'s chain recordings, all with the far voice 100 ms ahead) it went from -86 to -37 ms,
//!   about 10 ms later each take, while within each take it held to 0.1 ms (except in one take's first 6 s, where the mic's
//!   copy flipped between two places 11 ms apart). The base cable as the far end gave about -10 ms. So it's measured in the
//!   take's own streams, just before the scene: a calibration in a take of its own would miss by about 10 ms.
//! - **The take's programme** ([`Plan`], frames after `play_record`'s lead of silence): a [`marker`] (a 0.2 s sweep) on both
//!   tracks at 0, the far end's copy as the reference and the probe's as its echo with no delay; the far voice on the far end
//!   from where the calibration puts it (an `nf_engine::LatePart`), no earlier than [`Plan::earliest`]; the scene's input on
//!   the probe from [`Plan::scene_at`].
//! - **The calibration** ([`find_marker`], on the chain recording as it grows): the marker's arrival in the chain's mic and in
//!   its reference (a matched filter), their lag in the chain plus the reference's lead there, is the rig's offset. The far
//!   voice then starts the asked delay less that offset ahead of its echo ([`Plan::far_start`]; behind it when that's
//!   negative). It looks only where the marker can be ([`search_from`]: from half a second before the take's lead ends), so
//!   each look correlates about a second of the recording, not the run so far.
//! - **The measurement** ([`measure`], on the take's whole chain recording): in each far-only turn, the lag of the echo in the
//!   chain's mic behind the chain's reference (GCC-PHAT, within [`SEARCH_MS`] of where the calibration put it: as far as the
//!   rig's offset may be), which is what AEC3 sees, plus the lead over the turn: the delay after the reference's tap. The
//!   median turn is reported, with the range; turns after a rig slip are reported apart.
//! - **Rig slips** ([`track_rig`], TODO 14's note of 2026-11-02): a take whose far end's playback slipped 70 ms against its
//!   echo mid-take, with nothing in the streams' positions or stamps to show it, read as a filter that stopped cancelling.
//!   So each device's content is tracked against what was played: windows of the probe's track (the scene's input) in the
//!   chain's mic, and of the far end's (the far voice where it started) in the chain's reference, each found within
//!   [`MAX_RIG_OFFSET_MS`] of where the last one was (or, failing that, within [`RIG_REACQUIRE_MS`]: a bigger slip), and its
//!   place turned into the device's own position (the mic's stream index and skips, the reference's timeline index), so the
//!   engine's own moves (a discard, a skip, a read-position jump) don't count. The offset of that position from the played one
//!   holds while the rig holds; a move of [`SLIP_MS`] or more between windows, which the next window confirms, is a slip: the
//!   report says on which side, when and by how much, and leaves the far-only turns from its turn on out of the echo figures
//!   (and double talk from then on), as the rig's, not the filter's. A side whose windows stop being found at all, to the end
//!   of the take ([`RIG_LOST_WINDOWS`] or more), has lost its track, and the turns from its last window found on are left out
//!   the same way. A step in a side's last window, with none after it to confirm it, is reported as unconfirmed and sets
//!   nothing apart. Each side's times are its own content's place in the scene: the probe's input sample, or the far voice's
//!   (the far end plays its sample `i` at `far_start + i`, so the far voice's times line up with the turns).

use std::fmt::Write as _;

use nf_core::latency::analysis::{gcc_phat, xcorr_peak};
use nf_core::latency::sweep::exp_sweep;
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_engine::{ChainBlock, ChainRecording};

use super::signals::{Scene, Turn, far_only_from, turn_at, turn_len};

/// `--echo-delay`'s default (typical speakers) and its range, in ms.
pub const DEFAULT_ECHO_DELAY_MS: f32 = 25.0;
pub const MAX_ECHO_DELAY_MS: f32 = 300.0;
/// The shortest echo delay the echo canceller serves with the exclusive mic (the default since 2026-11-02), in ms: the mic's
/// blocks come fresher, so the reference's lead rose to 18.3 ms on the cables (12-14 shared), and an echo needs about that
/// plus a little to come after its reference (TODO 10's detail, the echo floor). The default delay stays over it.
pub const EXCLUSIVE_MIC_ECHO_FLOOR_MS: f32 = 20.0;
// The default delay is one the canceller serves with the default (exclusive) mic, with room for the lead's take-to-take spread.
const _: () = assert!(DEFAULT_ECHO_DELAY_MS >= EXCLUSIVE_MIC_ECHO_FLOOR_MS + 4.0);
/// The most the rig may put a zero-delay echo before (or after) its reference's tap for the take to make up for it.
pub const MAX_RIG_OFFSET_MS: f64 = 250.0;
/// The measurement looks this far either side of the delay the calibration expects: as far as the rig's offset may be (a 40 ms
/// search reported a 70 ms slip as 40), so a jump of the reference's read position after it, or a slip of the rig, shows at its
/// size.
pub const SEARCH_MS: f64 = MAX_RIG_OFFSET_MS;
/// The rig tracker's windows: a second of what a device played, every half second; a window counts where what was played is
/// within [`RIG_ACTIVE_DB`] of its loudest window and its copy correlates at [`RIG_MIN_PEAK`] or more.
const RIG_WINDOW_S: f64 = 1.0;
const RIG_HOP_S: f64 = 0.5;
const RIG_ACTIVE_DB: f64 = 30.0;
const RIG_MIN_PEAK: f32 = 0.5;
/// A window not found within [`MAX_RIG_OFFSET_MS`] of where the last one was is looked for again this far either side, so a
/// slip bigger than the plan makes up for still shows at its size.
pub const RIG_REACQUIRE_MS: f64 = 2_000.0;
/// This many active windows in a row not found even there, to the end of the take, lose a side's track.
pub const RIG_LOST_WINDOWS: usize = 2;
/// A device's content moving by this much or more between windows (or from the marker), confirmed by the window after, is a
/// slip. Each side runs on one clock (a cable's two ends, a device and its loopback), so between windows it holds to the
/// sample; VB-CABLE's resyncs move it by about a period (10-11 ms), and the dev VM's far end slipped by 70 ms.
pub const SLIP_MS: f64 = 1.0;
/// AEC3 needs the echo at least about this long after the reference it's given: under it, the echo can come before its
/// reference as the delay wanders by the stamps' jitter (the mic's are in 0.5 ms steps within ±1 ms on the cables).
pub const MIN_CAUSAL_MS: f64 = 2.0;
/// The marker: an exponential sweep, 300 Hz to 6 kHz in 0.2 s, 10 ms fades, peaking at -20 dBFS.
const MARKER_S: f32 = 0.2;
const MARKER_F1: f32 = 300.0;
const MARKER_F2: f32 = 6_000.0;
const MARKER_FADE_S: f32 = 0.01;
const MARKER_PEAK: f32 = 0.1;
/// From the marker's end, the time the calibration has to find it and place the far voice: the marker's way through the cables
/// (about 60 ms each on the dev VM) and the engine's block, the chain recorder's 20 ms drain, the calibration's 20 ms poll and
/// its correlation (a few ms in a release build, about 50 in a dev build: [`search_from`] keeps it to about a second of the
/// recording), and `play_record`'s 100 ms write-ahead, with room to spare.
const PLACE_S: f64 = 0.6;
/// The matched filter's least confidence for the marker's arrival (silence and dither give about 14 dB).
const MARKER_MIN_CONFIDENCE_DB: f32 = 20.0;
/// The marker counts as arrived once this much more has been recorded after it.
const MARKER_GUARD_S: f64 = 0.05;
/// [`search_from`]'s margin before the earliest the marker can be in the chain recording.
const SEARCH_MARGIN_MS: f64 = 250.0;
/// The marker is looked for in this much of the chain recording from [`search_from`]: a slow start of `play_record`'s streams
/// fits in it several times over, and it keeps the matched filter's FFT to 2^18 at most (2^16 for the look that finds it).
const MARKER_SEARCH_S: f64 = 5.0;
/// A chain recording this long with no reference in any block has none.
const NO_REFERENCE_BLOCKS: usize = 100;
/// The lead at the marker: the readings within this many blocks of it (0.25 s either side).
const LEAD_BLOCKS: usize = 25;
/// A far-only turn is measured when at least this much of it is in the recording, its reference carries the far voice, and its
/// correlation peak has this confidence.
const MIN_TURN_S: f64 = 1.0;
const MIN_REFERENCE_RMS: f64 = 1e-4;
const TURN_MIN_CONFIDENCE_DB: f32 = 12.0;
/// A measured delay this far off the asked, or turns this far apart, earn a warning.
const DELAY_WARN_MS: f64 = 2.0;

fn fs() -> f64 {
    f64::from(SAMPLE_RATE)
}

fn to_ms(samples: f64) -> f64 {
    samples * 1000.0 / fs()
}

fn to_samples(ms: f64) -> f64 {
    ms * 1e-3 * fs()
}

/// The calibration marker, as both tracks play it.
pub fn marker() -> Vec<f32> {
    exp_sweep(SAMPLE_RATE, MARKER_F1, MARKER_F2, MARKER_S, MARKER_FADE_S, MARKER_PEAK).signal
}

/// The echo take's programme, in frames after `play_record`'s lead (see the module notes).
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The asked delay, ms.
    pub delay_ms: f64,
    /// On both tracks at 0.
    pub marker: Vec<f32>,
    /// The far voice starts no earlier than this (the marker's end and [`PLACE_S`]) and no later than `latest`; the probe's
    /// scene starts at `scene_at`, which leaves room for a rig [`MAX_RIG_OFFSET_MS`] either way.
    pub earliest: u64,
    pub scene_at: u64,
    pub latest: u64,
}

/// Where the far voice starts, and whether the rig's offset was past what the plan makes up for (then it starts as near as
/// it can).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FarStart {
    pub frames: u64,
    pub clamped: bool,
}

impl Plan {
    pub fn new(delay_ms: f64) -> Self {
        let marker = marker();
        let earliest = marker.len() as u64 + to_samples(PLACE_S * 1000.0).round() as u64;
        let scene_at = earliest + to_samples(MAX_RIG_OFFSET_MS + delay_ms).round() as u64;
        let mut plan = Self { delay_ms, marker, earliest, scene_at, latest: u64::MAX };
        plan.latest = plan.unclamped(MAX_RIG_OFFSET_MS).max(0) as u64;
        plan
    }

    /// The far voice's start for a rig offset, before the clamp: `delay_ms - rig_ms` ahead of the scene.
    fn unclamped(&self, rig_ms: f64) -> i64 {
        (self.scene_at as f64 - to_samples(self.delay_ms - rig_ms)).round() as i64
    }

    /// Where the far voice starts for a rig that puts a zero-delay echo `rig_ms` after its reference's tap (negative: before
    /// it), so that its echo reaches the mic `delay_ms` after the tap: `delay_ms - rig_ms` ahead of the scene's start on the
    /// probe (behind it when that's negative), within `earliest..=latest`.
    pub fn far_start(&self, rig_ms: f64) -> FarStart {
        let at = self.unclamped(rig_ms);
        let frames = at.clamp(self.earliest as i64, self.latest as i64) as u64;
        FarStart { frames, clamped: frames as i64 != at }
    }

    /// The far voice's start without a calibration: as for a rig that adds nothing, `delay_ms` ahead of the scene.
    pub fn default_start(&self) -> u64 {
        self.far_start(0.0).frames
    }

    /// How far ahead of its echo the far voice played from `start`, in ms (negative: behind it).
    pub fn advance_ms(&self, start: u64) -> f64 {
        to_ms(self.scene_at as f64 - start as f64)
    }

    /// The probe's track: the marker, silence, and the scene's input from `scene_at`.
    pub fn probe_track(&self, input: &[f32]) -> Vec<f32> {
        let mut x = self.marker.clone();
        x.resize(self.scene_at as usize, 0.0);
        x.extend_from_slice(input);
        x
    }

    /// The far voice as the far end played it from `start`, against the scene's input (`far.wav`): sample `i` is what it played
    /// as the probe played the input's sample `i`, zeros where it played nothing.
    pub fn far_as_played(&self, far: &[f32], start: u64) -> Vec<f32> {
        let shift = self.scene_at as i64 - start as i64;
        (0..far.len() as i64).map(|i| usize::try_from(i + shift).ok().and_then(|j| far.get(j)).copied().unwrap_or(0.0)).collect()
    }
}

/// Where the marker came in a chain recording, and what that says about the rig.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// Where the marker starts in the chain recording's mic and reference, in samples from its first block.
    pub mic_at: f64,
    pub ref_at: f64,
    /// The matched filter's confidence at each, in dB.
    pub mic_confidence_db: f32,
    pub ref_confidence_db: f32,
    /// The reference's lead at the marker in ms (the loop's low-passed lead within 0.25 s, each block's taken to the marker
    /// block's echo path), and whether the stamps timed it (else the delivery did).
    pub lead_ms: f64,
    pub stamped: bool,
    /// How long after its reference's tap a zero-delay echo reaches the mic, in ms: the marker's lag in the chain plus the lead.
    pub rig_ms: f64,
}

/// What a look at the chain recording found.
#[derive(Clone, Debug, PartialEq)]
pub enum Marker {
    Found(Calibration),
    /// Not all of it yet, in the mic or the reference (each says whether it's there).
    NotYet {
        mic: bool,
        reference: bool,
    },
    /// It can't be found: why.
    Failed(String),
}

/// Where a take's marker can be in its chain recording, for [`find_marker`]: `take_at` is the recording's length in samples as
/// the take started (`ChainRecorder::blocks` times a block, just before `play_record`), `lead` is `play_record`'s lead in frames.
/// The marker plays after the lead, and the recording's samples come as the mic captures them, so the mic's copy can't be in it
/// before `take_at + lead`. The reference's copy can come up to the rig's largest offset ([`MAX_RIG_OFFSET_MS`]) before the
/// mic's, and [`SEARCH_MARGIN_MS`] is to spare: the search starts half a second before the lead ends. So the calibration's looks
/// before the lead's last quarter second see too little to correlate, and the one that finds the marker correlates about a
/// second.
pub fn search_from(take_at: usize, lead: usize) -> usize {
    (take_at + lead).saturating_sub(to_samples(MAX_RIG_OFFSET_MS + SEARCH_MARGIN_MS).round() as usize)
}

/// Looks for the marker in `chain`, in the [`MARKER_SEARCH_S`] from sample `from` on ([`search_from`]): found once all of it has
/// arrived in both the mic and the reference. Its places are in samples from the recording's first block.
pub fn find_marker(chain: &ChainRecording, marker: &[f32], from: usize) -> Marker {
    if chain.blocks.len() >= NO_REFERENCE_BLOCKS && chain.blocks.iter().all(|b| b.reference.is_none()) {
        return Marker::Failed("the filter has no echo reference, so nothing times the far end".into());
    }
    let n = chain.mic.len().min(chain.reference.len());
    let window = from.min(n)..n.min(from.saturating_add(to_samples(MARKER_SEARCH_S * 1000.0) as usize));
    let look = |x: &[f32]| arrival(&x[window.clone()], marker).map(|(at, confidence)| (at + window.start as f64, confidence));
    let (mic, reference) = (look(&chain.mic), look(&chain.reference));
    let (Some((mic_at, mic_confidence_db)), Some((ref_at, ref_confidence_db))) = (mic, reference) else {
        return Marker::NotYet { mic: mic.is_some(), reference: reference.is_some() };
    };
    let b = (ref_at / BLOCK as f64) as usize;
    let Some(here) = chain.blocks.get(b).and_then(path) else {
        return Marker::Failed("the echo reference had no timeline at the marker".into());
    };
    let Some(lead_ms) = lead_near(&chain.blocks[b.saturating_sub(LEAD_BLOCKS)..(b + LEAD_BLOCKS + 1).min(chain.blocks.len())], here) else {
        return Marker::Failed("the echo reference had no lead readings at the marker".into());
    };
    let stamped = chain.blocks[b].reference.is_some_and(|r| r.stamped);
    let lag = run_position(&chain.blocks, mic_at) - run_position(&chain.blocks, ref_at);
    Marker::Found(Calibration { mic_at, ref_at, mic_confidence_db, ref_confidence_db, lead_ms, stamped, rig_ms: to_ms(lag) + lead_ms })
}

/// Where `marker` starts in `x`, and the matched filter's confidence, once all of it and [`MARKER_GUARD_S`] after it are there.
fn arrival(x: &[f32], marker: &[f32]) -> Option<(f64, f32)> {
    let guard = to_samples(MARKER_GUARD_S * 1000.0);
    if (x.len() as f64) < marker.len() as f64 + guard {
        return None;
    }
    let p = xcorr_peak(marker, x, x.len())?;
    let whole = p.delay >= 0.0 && p.delay + marker.len() as f64 + guard <= x.len() as f64;
    (whole && p.confidence_db >= MARKER_MIN_CONFIDENCE_DB).then_some((p.delay, p.confidence_db))
}

/// A position in the recording as a sample of the run: the recording skips blocks its ring lost.
fn run_position(blocks: &[ChainBlock], at: f64) -> f64 {
    let k = (at / BLOCK as f64).floor();
    blocks.get(k as usize).map_or(at, |b| b.index as f64 * BLOCK as f64 + (at - k * BLOCK as f64))
}

/// The echo path the engine held at a block, in samples: the mic's index less the reference's.
fn path(b: &ChainBlock) -> Option<f64> {
    b.reference.map(|r| (i128::from(b.mic_index) - i128::from(r.read_index)) as f64)
}

/// The reference's lead over `blocks` in ms: the median of the loop's low-passed lead (its readings are in the mic stamps' 0.5
/// ms steps, and skewed; the low-pass follows the read position's jumps at once), each taken to the echo path `to`. A block
/// whose path is a sample longer served a reference sample tapped a sample earlier, so its lead is 1/48 ms more: so a jump of
/// the read position among them doesn't move it.
fn lead_near(blocks: &[ChainBlock], to: f64) -> Option<f64> {
    let leads: Vec<f64> = blocks
        .iter()
        .filter_map(|b| {
            let (r, p) = (b.reference?, path(b)?);
            r.lead_ms.is_finite().then(|| f64::from(r.lead_ms) - to_ms(p - to))
        })
        .collect();
    median(&leads)
}

/// Where the echo landed over the take's far-only turns: the median turn (by its delay after the tap; the lower middle one of an
/// even count). With a rig slip, over the turns before it; those after it are listed apart.
#[derive(Clone, Debug, PartialEq)]
pub struct EchoDelay {
    pub asked_ms: f64,
    /// The echo's delay after its reference's tap (`seen_ms + lead_ms`), the lag AEC3 sees (the echo behind the reference it's
    /// given, in the chain), and the reference's lead, in ms.
    pub measured_ms: f64,
    pub seen_ms: f64,
    pub lead_ms: f64,
    /// The lowest and highest delay after the tap among the turns, in ms.
    pub low_ms: f64,
    pub high_ms: f64,
    /// Far-only turns measured (before a rig slip), of those the recording holds.
    pub turns: usize,
    pub turns_held: usize,
    /// The far-only turns from the rig's slip on: each one's start in the scene (s) and its delay after the tap (ms).
    pub slipped: Vec<(f64, f64)>,
}

/// Measures the echo's delay in `chain`, the take's whole chain recording, where `cal` found the marker and the far voice
/// started at `far_start` (see the module notes). `slip_from`: the scene sample from which the rig had slipped
/// ([`RigTrack::excluded_from`]): the turns from the one holding it on are listed apart, not in the figure.
pub fn measure(
    chain: &ChainRecording, scene: &Scene, plan: &Plan, cal: &Calibration, far_start: u64, slip_from: Option<usize>,
) -> Result<EchoDelay, String> {
    let turn = turn_len();
    let n = chain.mic.len().min(chain.reference.len()) as i64;
    // The lag the calibration expects in the chain, and where the scene's input starts in the chain's mic.
    let expected = (cal.mic_at - cal.ref_at + plan.scene_at as f64 - far_start as f64).round() as i64;
    let scene0 = cal.mic_at.round() as i64 + plan.scene_at as i64;
    let search = to_samples(SEARCH_MS).round() as usize;
    let (mut turns, mut held, mut slipped) = (Vec::new(), 0usize, Vec::new());
    for t0 in (0..scene.input.len()).step_by(3 * turn) {
        debug_assert_eq!(turn_at(t0), Turn::FarOnly);
        let t1 = (t0 + turn).min(scene.input.len());
        let (m0, m1) = (scene0 + t0 as i64, scene0 + t1 as i64);
        if ((t1 - t0) as f64) < MIN_TURN_S * fs() || m0.min(m0 - expected) < 0 || m1.max(m1 - expected) > n {
            continue;
        }
        held += 1;
        let (m0, m1, r0) = (m0 as usize, m1 as usize, (m0 - expected) as usize);
        let (mic, reference) = (&chain.mic[m0..m1], &chain.reference[r0..r0 + (m1 - m0)]);
        if rms(reference) < MIN_REFERENCE_RMS {
            continue;
        }
        let Some((lag, confidence)) = gcc_phat(reference, mic, search, 1.0) else { continue };
        if confidence < TURN_MIN_CONFIDENCE_DB {
            continue;
        }
        // The lead over the turn, at the echo path it held most of the time (the correlation's peak is that part's).
        let blocks = &chain.blocks[(m0 / BLOCK).min(chain.blocks.len())..m1.div_ceil(BLOCK).min(chain.blocks.len())];
        let paths: Vec<f64> = blocks.iter().filter_map(path).collect();
        let Some(lead) = median(&paths).and_then(|p| lead_near(blocks, p)) else { continue };
        let seen = to_ms(expected as f64 + lag);
        if slip_from.is_some_and(|s| t1 > s) {
            slipped.push((t0 as f64 / fs(), seen + lead));
        } else {
            turns.push((seen + lead, seen, lead));
        }
    }
    if turns.is_empty() {
        return Err(if !slipped.is_empty() {
            let d: Vec<String> = slipped.iter().map(|(_, ms)| format!("{ms:.1}")).collect();
            format!("every far-only turn came after the rig slipped or lost its track (their echo came {} ms after its tap)", d.join(", "))
        } else if held == 0 {
            "the chain recording doesn't hold a far-only turn".into()
        } else {
            format!("the echo didn't stand out against its reference in any of the {held} far-only turns")
        });
    }
    // The median turn, a turn's own figures (the lower middle one of an even count), so its delay is its lag plus its lead.
    turns.sort_by(|a, b| a.0.total_cmp(&b.0));
    let k = turns.len();
    let (measured_ms, seen_ms, lead_ms) = turns[(k - 1) / 2];
    Ok(EchoDelay {
        asked_ms: plan.delay_ms,
        measured_ms,
        seen_ms,
        lead_ms,
        low_ms: turns[0].0,
        high_ms: turns[k - 1].0,
        turns: k,
        turns_held: held - slipped.len(),
        slipped,
    })
}

/// Which device a slip moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The probe's content in the filter's mic (the near end's cable).
    Mic,
    /// The far end's content in the filter's echo reference (its loopback).
    Reference,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Mic => "the mic (the probe's track in the filter's mic)",
            Side::Reference => "the far end's loopback (the far voice in the filter's reference)",
        }
    }
}

/// One window of a device's played track, found in the chain recording.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigPoint {
    /// The window's centre in the scene, in samples (negative before the scene starts): the probe's input sample on the mic's
    /// side, the far voice's on the reference's.
    pub at: i64,
    /// The device's position of the window's copy less the window's place in the track, in samples, against the marker's.
    pub offset: f64,
    /// The copy's correlation coefficient.
    pub peak: f32,
}

/// A move of a device's content against what it played.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slip {
    pub side: Side,
    /// Where in the scene it happened, in samples (negative before the scene): after the start of the last window that still
    /// had the old offset (the marker, for a slip before the first window), and before the end of the first that has the new
    /// (a window shows the offset of most of its content). A second and a half apart for neighbouring windows; far apart where
    /// the device played nothing between them (the far voice is silent in near-only turns).
    pub after: i64,
    pub at: i64,
    /// How far the content moved, in samples (positive: it came later).
    pub samples: f64,
    /// The window after it agreed with its new offset. A side's last window has none to: a step there is reported as
    /// unconfirmed and sets no turns apart.
    pub confirmed: bool,
}

/// A side's track lost: from some window on, to the end of the take, none was found even within [`RIG_REACQUIRE_MS`] of where
/// the content had been (it moved further, or stopped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lost {
    pub side: Side,
    /// The start of the last window found (the marker's place, if none was), in scene samples.
    pub after: i64,
    /// The active windows not found from then on.
    pub windows: usize,
}

impl Lost {
    pub fn after_s(&self) -> f64 {
        self.after as f64 / fs()
    }
}

impl Slip {
    pub fn after_s(&self) -> f64 {
        self.after as f64 / fs()
    }

    pub fn at_s(&self) -> f64 {
        self.at as f64 / fs()
    }

    /// "at about 20.2 s" (neighbouring windows bound it), or "between 24.0 and 27.0 s".
    pub fn when(&self) -> String {
        if (self.at - self.after) as f64 <= (RIG_WINDOW_S + RIG_HOP_S) * fs() {
            format!("at about {:.1} s", (self.after + self.at) as f64 / 2.0 / fs())
        } else {
            format!("between {:.1} and {:.1} s", self.after_s(), self.at_s())
        }
    }

    pub fn ms(&self) -> f64 {
        to_ms(self.samples)
    }
}

/// Both devices' content against what was played over the take ([`track_rig`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigTrack {
    pub mic: Vec<RigPoint>,
    pub reference: Vec<RigPoint>,
    /// In time order, confirmed or not.
    pub slips: Vec<Slip>,
    /// The sides whose track was lost.
    pub lost: Vec<Lost>,
}

impl RigTrack {
    /// The scene sample from which the echo figures are the rig's: the earliest a confirmed slip can have come, or a lost track
    /// was last found (0 if before the scene).
    pub fn excluded_from(&self) -> Option<usize> {
        let slips = self.slips.iter().filter(|s| s.confirmed).map(|s| s.after);
        slips.chain(self.lost.iter().map(|l| l.after)).min().map(|a| a.max(0) as usize)
    }

    /// How far a side's content wandered over its windows between its slips (the widest stretch's range, ms), and how many
    /// windows it had.
    pub fn spread_ms(&self, side: Side) -> (f64, usize) {
        let pts = if side == Side::Mic { &self.mic } else { &self.reference };
        if pts.is_empty() {
            return (f64::NAN, 0);
        }
        // The windows kept differ by a slip's step only where it slipped: a new stretch starts there
        let (step, mut widest) = (to_samples(SLIP_MS), 0.0f64);
        let (mut lo, mut hi, mut last) = (f64::INFINITY, f64::NEG_INFINITY, pts[0].offset);
        for p in pts {
            if (p.offset - last).abs() >= step {
                widest = widest.max(hi - lo);
                (lo, hi) = (f64::INFINITY, f64::NEG_INFINITY);
            }
            (lo, hi, last) = (lo.min(p.offset), hi.max(p.offset), p.offset);
        }
        (to_ms(widest.max(hi - lo)), pts.len())
    }

    /// Whether `side` slipped (a confirmed slip).
    pub fn slipped(&self, side: Side) -> bool {
        self.slips.iter().any(|s| s.side == side && s.confirmed)
    }

    /// Whether `side` held: no confirmed slip, and its track not lost.
    pub fn held(&self, side: Side) -> bool {
        !self.slipped(side) && self.lost.iter().all(|l| l.side != side)
    }
}

/// A chain recording's sample `n` as a position of the device behind it: the mic stream's index (and the frames its position
/// skipped unfilled, which it doesn't count), or the reference timeline's index (at the block's rate: 479 or 481 samples at a
/// slip). `None` where the block has no reference, or took nothing.
struct Positions<'a> {
    blocks: &'a [ChainBlock],
    /// The mic's skips up to each block.
    skipped: Vec<i64>,
}

impl<'a> Positions<'a> {
    fn new(blocks: &'a [ChainBlock]) -> Self {
        let skipped = blocks.iter().scan(0i64, |acc, b| {
            *acc += b.mic_skipped;
            Some(*acc)
        });
        Self { blocks, skipped: skipped.collect() }
    }

    fn at(&self, side: Side, n: f64) -> Option<f64> {
        if n < 0.0 {
            return None;
        }
        let k = (n / BLOCK as f64).floor() as usize;
        let (b, o) = (self.blocks.get(k)?, n - (k * BLOCK) as f64);
        match side {
            Side::Mic => Some(b.mic_index as f64 + self.skipped[k] as f64 + o),
            Side::Reference => {
                b.reference.filter(|r| r.consumed > 0).map(|r| r.read_index as f64 + o * f64::from(r.consumed) / BLOCK as f64)
            }
        }
    }
}

/// Tracks both devices' content in `chain` against what they played (see the module notes): the probe's track
/// ([`Plan::probe_track`] of the scene's input) in the chain's mic, the far end's (the marker, then the scene's far voice from
/// `far_start`) in its reference, from where `cal` found the marker. Each side's times are its own content's in the scene: the
/// input's from `scene_at` on the probe's track, the far voice's from `far_start` on the far end's.
pub fn track_rig(chain: &ChainRecording, scene: &Scene, plan: &Plan, cal: &Calibration, far_start: u64) -> RigTrack {
    let pos = Positions::new(&chain.blocks);
    let probe = plan.probe_track(&scene.input);
    let mut far = plan.marker.clone();
    far.resize(far_start as usize, 0.0);
    far.extend_from_slice(scene.far.as_deref().unwrap_or(&[]));
    let scene_at = plan.scene_at as usize;
    let mic = track_side(&pos, Side::Mic, &chain.mic, &probe, cal.mic_at, scene_at, scene_at);
    let reference = track_side(&pos, Side::Reference, &chain.reference, &far, cal.ref_at, far_start as usize, far_start as usize);
    let mut slips: Vec<Slip> = mic.slips.into_iter().chain(reference.slips).collect();
    slips.sort_by_key(|s| s.at);
    RigTrack { mic: mic.points, reference: reference.points, slips, lost: mic.lost.into_iter().chain(reference.lost).collect() }
}

/// One side's track: the windows kept, the slips, and whether it was lost.
struct SideTrack {
    points: Vec<RigPoint>,
    slips: Vec<Slip>,
    lost: Option<Lost>,
}

/// One side of [`track_rig`]: `played` (track frames, the marker at 0) found window by window in `recorded` from track frame
/// `from` on, the marker having arrived at `marker_at` there; window places are reported against track frame `origin` (where
/// the side's scene content starts).
fn track_side(pos: &Positions<'_>, side: Side, recorded: &[f32], played: &[f32], marker_at: f64, from: usize, origin: usize) -> SideTrack {
    let ms = |v: f64| to_samples(v).round() as usize;
    let (w, hop, search, wide) = (ms(RIG_WINDOW_S * 1000.0), ms(RIG_HOP_S * 1000.0), ms(MAX_RIG_OFFSET_MS), ms(RIG_REACQUIRE_MS));
    let (marker, half) = (-(origin as i64), (w / 2) as i64);
    let Some(base) = pos.at(side, marker_at) else { return SideTrack { points: Vec::new(), slips: Vec::new(), lost: None } };
    let starts: Vec<usize> = (from..played.len().saturating_sub(w)).step_by(hop).collect();
    let loudest = starts.iter().map(|&t| rms(&played[t..t + w])).fold(0.0f64, f64::max);
    let quiet = loudest * 10f64.powf(-RIG_ACTIVE_DB / 20.0);
    let mut points = Vec::new();
    // Where the last window's copy was in the recording, and its track frame; the active windows not found since
    let (mut last, mut misses) = ((marker_at, 0usize), 0usize);
    for t in starts {
        let x = &played[t..t + w];
        if rms(x) < quiet.max(1e-6) {
            continue;
        }
        let expect = last.0 + (t - last.1) as f64;
        if expect + w as f64 > recorded.len() as f64 {
            // The recording ends before this window's copy would
            break;
        }
        // The window's copy within `reach` of where it would be at the last window's offset
        let find = |reach: usize| -> Option<(f64, f32)> {
            let lo = (expect - reach as f64).round().max(0.0) as usize;
            let hi = ((expect + (w + reach) as f64).round() as usize).min(recorded.len());
            let p = (hi > lo + w).then(|| xcorr_peak(x, &recorded[lo..hi], hi - lo - w)).flatten()?;
            (p.delay >= 0.0 && p.peak >= RIG_MIN_PEAK).then_some((lo as f64 + p.delay, p.peak))
        };
        let Some((n, peak)) = find(search).or_else(|| find(wide)) else {
            misses += 1;
            continue;
        };
        let Some(at) = pos.at(side, n) else { continue };
        points.push(RigPoint { at: (t + w / 2) as i64 - origin as i64, offset: at - t as f64 - base, peak });
        (last, misses) = ((n, t), 0);
    }
    let lost = (misses >= RIG_LOST_WINDOWS).then(|| Lost { side, after: points.last().map_or(marker, |p| p.at - half), windows: misses });
    let (points, slips) = slips_of(side, &points, marker, half);
    SideTrack { points, slips, lost }
}

/// The slips in a side's windows (in time order, the marker's offset 0, the marker at scene sample `marker`, the windows
/// `half` either side of their centres): a move of [`SLIP_MS`] or more from the level held so far, which the next window
/// confirms; a window that disagrees with both of its neighbours is an outlier, left out; a step in the last window, with
/// none after it, is an unconfirmed slip (not kept). Returns the windows kept and the slips.
fn slips_of(side: Side, points: &[RigPoint], marker: i64, half: i64) -> (Vec<RigPoint>, Vec<Slip>) {
    let step = to_samples(SLIP_MS);
    let (mut level, mut since, mut slips, mut kept) = (0.0f64, marker, Vec::new(), Vec::new());
    for (k, p) in points.iter().enumerate() {
        if (p.offset - level).abs() < step {
            (level, since) = (p.offset, p.at - half);
            kept.push(*p);
            continue;
        }
        let slip = |confirmed| Slip { side, after: since, at: p.at + half, samples: p.offset - level, confirmed };
        match points.get(k + 1) {
            Some(q) if (q.offset - p.offset).abs() < step => {
                slips.push(slip(true));
                (level, since) = (p.offset, p.at - half);
                kept.push(*p);
            }
            Some(_) => {}
            None => slips.push(slip(false)),
        }
    }
    (kept, slips)
}

/// The report's line on the rig: each side's windows and how far its content wandered, each slip and each lost track.
pub fn rig_line(r: &RigTrack) -> String {
    let side = |s: Side, what: &str| {
        let (spread, n) = r.spread_ms(s);
        let between = if r.slipped(s) { " between slips" } else { "" };
        if n == 0 { format!("{what} not tracked") } else { format!("{what} {n} windows, within {} ms{between}", fixed1(spread)) }
    };
    let mut line = format!("{}, {}", side(Side::Mic, "the mic's"), side(Side::Reference, "the reference's"));
    if r.slips.is_empty() && r.lost.is_empty() {
        line.push_str(": held");
    }
    for s in &r.slips {
        if s.confirmed {
            let _ = write!(line, "; {} slipped {:+.1} ms {}", short(s.side), s.ms(), s.when());
        } else {
            let _ = write!(line, "; {}'s last window {:+.1} ms off {}, unconfirmed", short(s.side), s.ms(), s.when());
        }
    }
    for l in &r.lost {
        let _ = write!(line, "; {}'s track lost from {:.1} s ({} windows not found)", short(l.side), l.after_s(), l.windows);
    }
    line
}

fn short(side: Side) -> &'static str {
    match side {
        Side::Mic => "the mic",
        Side::Reference => "the reference",
    }
}

fn fixed1(v: f64) -> String {
    if v.is_finite() { format!("{v:.1}") } else { "--".into() }
}

/// The warnings a rig earns: a slip or a lost track (where, by how much, and that the echo figures from its turn on are the
/// rig's), and an unconfirmed step at a side's end (reported, nothing set apart).
pub fn slip_warnings(r: &RigTrack) -> Vec<String> {
    let mut w = Vec::new();
    if let Some(from) = r.excluded_from() {
        let mut each: Vec<String> = r
            .slips
            .iter()
            .filter(|s| s.confirmed)
            .map(|s| format!("{} by {:+.1} ms {} into the scene", s.side.name(), s.ms(), s.when()))
            .collect();
        each.extend(r.lost.iter().map(|l| {
            format!(
                "{} lost from {:.1} s into the scene ({} windows not found within {RIG_REACQUIRE_MS:.0} ms of where its content \
                 was: it moved further, or stopped)",
                l.side.name(),
                l.after_s(),
                l.windows
            )
        }));
        let held = [Side::Mic, Side::Reference].into_iter().filter(|&side| r.held(side)).map(short).collect::<Vec<_>>();
        let held = if held.is_empty() { String::new() } else { format!(" ({} held)", held.join(" and ")) };
        w.push(format!(
            "The rig slipped: {}{held}, against what it played, with nothing in the filter's positions to show it. Real speakers \
             can't do that (the echo and its reference are the same playback), so the far-only turns from {:.0} s on are left \
             out of the echo figures, and double talk from then on: the rig's failure, not the filter's.",
            each.join("; "),
            far_only_from(from) as f64 / fs()
        ));
    }
    for s in r.slips.iter().filter(|s| !s.confirmed) {
        w.push(format!(
            "The last window of {} was {:+.1} ms off the rest {} into the scene, with no window after it to confirm a slip: \
             the echo figures keep the turns there.",
            s.side.name(),
            s.ms(),
            s.when()
        ));
    }
    w
}

/// The warnings a measured delay earns: an echo that reached AEC3 too soon after its reference (or before it), one that
/// landed off the asked delay, and one that moved during the take.
pub fn warnings(d: &EchoDelay) -> Vec<String> {
    let mut w = Vec::new();
    if d.seen_ms < MIN_CAUSAL_MS {
        let when = if d.seen_ms < 0.0 {
            format!("{:.1} ms before the reference it's given", -d.seen_ms)
        } else {
            format!("only {:.1} ms after the reference it's given", d.seen_ms)
        };
        w.push(format!(
            "AEC3 got the echo {when}: the echo came {:.1} ms after its reference's tap, and the filter served the reference with \
             a {:.1} ms lead. An echo that comes before its reference can't be cancelled (AEC3's delay estimate then reads about \
             0), and under about {MIN_CAUSAL_MS:.0} ms it can as the delay wanders: a speaker whose echo comes back sooner than the \
             lead would go uncancelled the same way.",
            d.measured_ms, d.lead_ms
        ));
    }
    if (d.measured_ms - d.asked_ms).abs() > DELAY_WARN_MS {
        w.push(format!(
            "The echo came {:.1} ms after its reference's tap, {:+.1} ms off the {:.1} ms asked: the rig's offset moved after the \
             calibration (the cables slip by about a period now and then), or the far voice couldn't start where the calibration \
             put it (see the calibration line).",
            d.measured_ms,
            d.measured_ms - d.asked_ms,
            d.asked_ms
        ));
    }
    if d.high_ms - d.low_ms > DELAY_WARN_MS {
        w.push(format!(
            "The echo's delay moved by {:.1} ms during the take ({:.1} to {:.1} ms a far-only turn): the rig slipped, or the \
             reference's read position jumped and its lead didn't follow.",
            d.high_ms - d.low_ms,
            d.low_ms,
            d.high_ms
        ));
    }
    w
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn median(v: &[f64]) -> Option<f64> {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if s.is_empty() {
        return None;
    }
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    Some(if s.len() % 2 == 1 { s[m] } else { (s[m - 1] + s[m]) / 2.0 })
}

#[cfg(test)]
mod tests {
    use nf_engine::{ChainParams, ReferenceFacts};
    use nf_stages::synth::pink_noise;

    use super::*;
    use crate::quality::QualityTest;
    use crate::quality::signals::{UserAudio, build};

    const FS: f64 = 48_000.0;
    /// One sample, in ms.
    const SAMPLE_MS: f64 = 1000.0 / FS;

    /// The exclusive mic's floor is the one that applies by default (the default delay is over it: a const assertion).
    #[test]
    fn the_exclusive_mics_echo_floor_is_the_default_one() {
        assert!(nf_engine::IoConfig::default().mic_exclusive);
    }

    /// `x` delayed by `d` samples (a fraction through a 33-tap Blackman-windowed sinc), the same length.
    fn delayed(x: &[f32], d: f64) -> Vec<f32> {
        const K: i64 = 16;
        let (di, frac) = (d.floor() as i64, d - d.floor());
        if frac == 0.0 {
            return (0..x.len() as i64).map(|t| usize::try_from(t - di).ok().and_then(|s| x.get(s)).copied().unwrap_or(0.0)).collect();
        }
        let kernel: Vec<f64> = (-K..=K)
            .map(|m| {
                let u = m as f64 - frac;
                let sinc = if u == 0.0 { 1.0 } else { (std::f64::consts::PI * u).sin() / (std::f64::consts::PI * u) };
                let w = 0.42
                    + 0.5 * (std::f64::consts::PI * u / (K + 1) as f64).cos()
                    + 0.08 * (std::f64::consts::TAU * u / (K + 1) as f64).cos();
                sinc * w
            })
            .collect();
        (0..x.len() as i64)
            .map(|t| {
                let mut acc = 0.0;
                for (k, &h) in kernel.iter().enumerate() {
                    let src = t - di - (k as i64 - K);
                    if let Some(&v) = usize::try_from(src).ok().and_then(|s| x.get(s)) {
                        acc += h * f64::from(v);
                    }
                }
                acc as f32
            })
            .collect()
    }

    /// The rig a fake chain recording comes from: the engine's lead (ms) and the rig's offset (ms, either sign), the take's
    /// programme starting `start` samples into the chain's reference; and optionally a jump of the reference's read position
    /// at a block (by samples: the path that much longer, the lead readings that much more, the reference's content that much
    /// earlier), and a slip of one side's content from a chain sample on (by samples, later; the engine's facts unmoved: a
    /// cable's own).
    #[derive(Clone, Copy)]
    struct Rig {
        lead_ms: f64,
        rig_ms: f64,
        start: usize,
        jump: Option<(usize, i64)>,
        slip: Option<(usize, f64, Side)>,
    }

    const RIG: Rig = Rig { lead_ms: 14.0, rig_ms: -40.0, start: 120_000, jump: None, slip: None };
    /// The fake take's lead (`play_record`'s 1 s), and where its marker is looked for: its programme starts at `RIG.start` in
    /// the reference, so the take started a lead before (`the_marker_is_looked_for_only_where_it_can_be` checks it).
    const LEAD: usize = 48_000;
    const FROM: usize = RIG.start - 24_000;

    /// A fake chain recording of a take: the far end's track through the loopback and the reference's timeline into the chain's
    /// reference, the probe's through the cable into the chain's mic, `rig_ms - lead_ms` behind it (so a zero-delay echo comes
    /// `rig_ms` after its tap by the stamps), each with a little noise; a block's facts with the lead and the echo path.
    fn chain(far_track: &[f32], probe_track: &[f32], rig: Rig, len: usize) -> ChainRecording {
        let place = |track: &[f32], at: f64| {
            let mut x = vec![0.0f32; len];
            let n = track.len().min(len);
            x[..n].copy_from_slice(&track[..n]);
            delayed(&x, at)
        };
        let jump_at = rig.jump.map_or(len, |(b, _)| (b * BLOCK).min(len));
        let jump = rig.jump.map_or(0, |j| j.1);
        let mut reference = place(far_track, rig.start as f64);
        if jump != 0 {
            // From the jump on, the reference served is `jump` samples earlier in the loopback's timeline.
            let later = place(far_track, rig.start as f64 + jump as f64);
            reference[jump_at..].copy_from_slice(&later[jump_at..]);
        }
        let mic_at = rig.start as f64 + (rig.rig_ms - rig.lead_ms) * 1e-3 * FS;
        let mut mic = place(probe_track, mic_at);
        match rig.slip {
            Some((at, by, Side::Reference)) => {
                let later = place(far_track, rig.start as f64 + if at >= jump_at { jump as f64 } else { 0.0 } + by);
                reference[at..].copy_from_slice(&later[at..]);
            }
            Some((at, by, Side::Mic)) => {
                let later = place(probe_track, mic_at + by);
                mic[at..].copy_from_slice(&later[at..]);
            }
            None => {}
        }
        let noise = |seed| pink_noise(len, 3e-5, seed);
        let (mic, reference) = (nf_stages::synth::add(&mic, &noise(5)), nf_stages::synth::add(&reference, &noise(6)));
        let blocks = (0..len / BLOCK)
            .map(|k| {
                let jumped = if k * BLOCK >= jump_at { jump } else { 0 };
                let path = 600 + jumped;
                let lead = rig.lead_ms + jumped as f64 * SAMPLE_MS;
                let reading = lead + if k % 2 == 0 { 0.5 } else { -0.5 };
                ChainBlock {
                    index: k as u64,
                    capture_qpc_hns: k as f64 * 1e5,
                    mic_stamped: true,
                    mic_index: (k * BLOCK) as u64,
                    mic_wake_hns: k as f64 * 1e5 + 1.5e5,
                    mic_queued: 0,
                    mic_discarded: 0,
                    mic_skipped: 0,
                    reference: Some(ReferenceFacts {
                        read_index: (k * BLOCK) as i64 - path,
                        lead_reading_ms: reading as f32,
                        lead_ms: lead as f32,
                        target_ms: lead as f32,
                        stamped: true,
                        fresh: true,
                        delivered: true,
                        consumed: 480,
                        ..ReferenceFacts::default()
                    }),
                    aec_delay_ms: f32::NAN,
                    aec_erle_db: f32::NAN,
                    settings: ChainParams::default(),
                    budget: None,
                    output: Default::default(),
                }
            })
            .collect();
        let len = len / BLOCK * BLOCK;
        ChainRecording {
            output: vec![0.0; len],
            mic: mic[..len].to_vec(),
            reference: reference[..len].to_vec(),
            blocks,
            ..Default::default()
        }
    }

    /// The far end's track: the marker, then the far voice from `start`.
    fn far_track(plan: &Plan, far: &[f32], start: u64) -> Vec<f32> {
        let mut x = plan.marker.clone();
        x.resize(start as usize, 0.0);
        x.extend_from_slice(far);
        x
    }

    #[test]
    fn the_plan_places_the_far_voice_for_the_rig_either_way() {
        let plan = Plan::new(25.0);
        let ms = |v: f64| (v * 48.0).round() as u64;
        assert_eq!(plan.marker.len(), 9_600);
        assert_eq!(plan.earliest, 9_600 + ms(600.0));
        assert_eq!(plan.scene_at, plan.earliest + ms(275.0));
        assert_eq!(plan.latest, plan.scene_at - ms(25.0) + ms(250.0));
        // A rig that adds nothing: the far voice 25 ms ahead of its echo
        assert_eq!(plan.default_start(), plan.scene_at - ms(25.0));
        assert_eq!(plan.far_start(0.0), FarStart { frames: plan.default_start(), clamped: false });
        // The cables' echo 40 ms before its reference's tap: 65 ms ahead
        let early = plan.far_start(-40.0);
        assert_eq!((early.frames, early.clamped), (plan.scene_at - ms(65.0), false));
        assert!((plan.advance_ms(early.frames) - 65.0).abs() < 1e-9);
        // A rig that's late by 40 ms: the far voice 15 ms behind its echo
        let late = plan.far_start(40.0);
        assert_eq!(late.frames, plan.scene_at + ms(15.0));
        assert!((plan.advance_ms(late.frames) + 15.0).abs() < 1e-9);
        // Past ±250 ms: as near as it can
        assert_eq!(plan.far_start(-250.0), FarStart { frames: plan.earliest, clamped: false });
        assert_eq!(plan.far_start(-251.0), FarStart { frames: plan.earliest, clamped: true });
        assert_eq!(plan.far_start(300.0), FarStart { frames: plan.latest, clamped: true });
        // The asked delay's whole range fits
        for d in [0.0, 300.0] {
            let p = Plan::new(d);
            assert_eq!(p.far_start(-MAX_RIG_OFFSET_MS).frames, p.earliest, "{d}");
            assert_eq!(p.default_start(), p.scene_at - ms(d), "{d}");
        }
        // The probe's track: the marker at 0, the input from scene_at
        let input = [0.25f32; 10];
        let probe = plan.probe_track(&input);
        assert_eq!(probe.len(), plan.scene_at as usize + 10);
        assert_eq!(probe[..9_600], plan.marker[..]);
        assert!(probe[9_600..plan.scene_at as usize].iter().all(|&v| v == 0.0));
        assert_eq!(probe[plan.scene_at as usize..], input);
        // far.wav: the far voice moved by the advance, against the input
        let far: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(plan.far_as_played(&far, plan.scene_at - 3), [3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 0.0, 0.0, 0.0]);
        assert_eq!(plan.far_as_played(&far, plan.scene_at + 2), [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
    }

    /// The calibration finds the rig's offset on fake chain recordings, before its reference's tap and after it, at a
    /// fraction of a sample, through a jump of the reference's read position beside the marker; it waits for all of the
    /// marker in both, and says when the filter has no reference.
    #[test]
    fn the_calibration_finds_a_known_offset_either_way() {
        let plan = Plan::new(25.0);
        let far = vec![0.0f32; 48_000];
        let track = far_track(&plan, &far, plan.default_start());
        let probe = plan.probe_track(&[0.0; 48_000]);
        // The marker starts at block 250; a jump 15 blocks before it, among the lead's blocks, by 96 samples (2 ms)
        for (rig_ms, jump) in [(-40.0, None), (-10.3, None), (22.71, None), (-38.2, Some((235, 96)))] {
            let rig = Rig { rig_ms, jump, ..RIG };
            let c = chain(&track, &probe, rig, 200_000);
            let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else {
                panic!("{rig_ms}: {:?}", find_marker(&c, &plan.marker, FROM))
            };
            assert!((cal.rig_ms - rig_ms).abs() < SAMPLE_MS / 2.0, "{rig_ms}: found {cal:?}");
            let jumped = jump.map_or(0, |j| j.1);
            assert!((cal.ref_at - (RIG.start as i64 + jumped) as f64).abs() < 0.05, "{cal:?}");
            assert!((cal.lead_ms - (14.0 + jumped as f64 * SAMPLE_MS)).abs() < 1e-5, "the lead at the marker's path: {cal:?}");
            assert!(cal.stamped && cal.mic_confidence_db > 30.0 && cal.ref_confidence_db > 30.0, "{cal:?}");
        }
        // Only part of the marker in: not yet; the reference's copy in, the mic's not: says which
        let rig = Rig { rig_ms: 30.0, ..RIG };
        let c = chain(&track, &probe, rig, RIG.start + 9_600 + 2_400);
        assert_eq!(find_marker(&c, &plan.marker, FROM), Marker::NotYet { mic: false, reference: true });
        let c = chain(&track, &probe, rig, RIG.start + 2_000);
        assert_eq!(find_marker(&c, &plan.marker, FROM), Marker::NotYet { mic: false, reference: false });
        // The probe silent: the mic never gets it
        let c = chain(&track, &vec![0.0; probe.len()], rig, 200_000);
        assert_eq!(find_marker(&c, &plan.marker, FROM), Marker::NotYet { mic: false, reference: true });
        // No reference in the filter
        let mut c = chain(&track, &probe, RIG, 200_000);
        c.blocks.iter_mut().for_each(|b| b.reference = None);
        assert!(matches!(find_marker(&c, &plan.marker, FROM), Marker::Failed(m) if m.contains("no echo reference")));
    }

    /// The calibration looks only where the marker can be: from half a second before the take's lead ends (the rig's largest
    /// offset and a margin), for 5 s. A copy of the marker before that is never seen, nor one past the 5 s; the places found
    /// are the recording's.
    #[test]
    fn the_marker_is_looked_for_only_where_it_can_be() {
        assert_eq!(search_from(RIG.start - LEAD, LEAD), FROM);
        assert_eq!(search_from(1_000, LEAD), 25_000);
        assert_eq!(search_from(1_000, 20_000), 0, "a lead under half a second: from the recording's start");
        let plan = Plan::new(25.0);
        let track = far_track(&plan, &[], plan.default_start());
        let probe = plan.probe_track(&[]);
        let mut c = chain(&track, &probe, RIG, 200_000);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        // A copy in both before the search's start, at another lag: not seen
        for (x, at) in [(&mut c.mic, 20_000), (&mut c.reference, 50_000)] {
            x[at..at + plan.marker.len()].iter_mut().zip(&plan.marker).for_each(|(v, m)| *v += m);
        }
        assert_eq!(find_marker(&c, &plan.marker, FROM), Marker::Found(cal));
        // Searched from the recording's start, a marker 8.75 s in is past the search's 5 s; from its take's start, it's found
        let late = Rig { start: RIG.start + 300_000, ..RIG };
        let c = chain(&track, &probe, late, late.start + 50_000);
        assert_eq!(find_marker(&c, &plan.marker, 0), Marker::NotYet { mic: false, reference: false });
        let Marker::Found(found) = find_marker(&c, &plan.marker, search_from(late.start - LEAD, LEAD)) else { panic!() };
        assert!((found.ref_at - late.start as f64).abs() < 0.05 && (found.rig_ms - cal.rig_ms).abs() < 1e-6, "{found:?}");
    }

    /// A whole synthetic take: calibrated on the marker, the far voice placed for the asked delay, the echo measured in the
    /// far-only turns: within a sample of where it landed, which is within half a sample of the asked delay; AEC3's lag is the
    /// delay less the lead. Through a jump of the reference mid-take the delay holds, and the lag and the lead move by it.
    #[test]
    fn the_measured_delay_is_found_within_a_sample() {
        let scene = build(QualityTest::Echo, 12.0, None, 25.0, &UserAudio::default()).unwrap();
        // The jump: in the first double-talk turn, 144 samples (3 ms) shorter
        let cases = [(25.0, -40.0, None), (25.0, 18.37, None), (0.0, -9.6, None), (300.0, -85.5, None), (40.0, -37.0, Some((760, -144)))];
        for (delay, rig_ms, jump) in cases {
            let plan = Plan::new(delay);
            let probe = plan.probe_track(&scene.input);
            let len = RIG.start + probe.len() + 48_000;
            let rig = Rig { rig_ms, jump, ..RIG };
            // The calibration, on the chain as far as the marker
            let first = chain(&far_track(&plan, &[], plan.earliest), &plan.probe_track(&[]), rig, RIG.start + plan.earliest as usize);
            let Marker::Found(cal) = find_marker(&first, &plan.marker, FROM) else { panic!("{delay} {rig_ms}") };
            let start = plan.far_start(cal.rig_ms);
            assert!(!start.clamped);
            // The take: where the echo landed is the rig's offset plus the far voice's advance
            let c = chain(&far_track(&plan, scene.far.as_deref().unwrap(), start.frames), &probe, rig, len);
            let truth = rig_ms + plan.advance_ms(start.frames);
            assert!((truth - delay).abs() <= SAMPLE_MS / 2.0 + 1e-9, "{delay}: {truth}");
            let Marker::Found(again) = find_marker(&c, &plan.marker, FROM) else { panic!() };
            assert!((again.rig_ms - cal.rig_ms).abs() < 1e-6 && (again.mic_at - cal.mic_at).abs() < 1e-3, "the same marker in the take");
            let d = measure(&c, &scene, &plan, &again, start.frames, None).unwrap();
            assert!((d.measured_ms - truth).abs() < SAMPLE_MS, "{delay} {rig_ms}: measured {d:?}, truth {truth}");
            // Every turn: through the jump too, where the lag and the lead move by 3 ms the other ways
            assert!((d.low_ms - truth).abs() < SAMPLE_MS && (d.high_ms - truth).abs() < SAMPLE_MS, "{d:?}");
            assert_eq!((d.turns, d.turns_held, d.asked_ms), (2, 2, delay), "far only at 0 and 9 s");
            let leads = if jump.is_some() { [14.0, 11.0] } else { [14.0, 14.0] };
            assert!(leads.iter().any(|l| (d.lead_ms - l).abs() < 1e-5), "{d:?}");
            assert!((d.seen_ms + d.lead_ms - d.measured_ms).abs() < 1e-9 && (d.seen_ms - (truth - d.lead_ms)).abs() < SAMPLE_MS, "{d:?}");
            let causal = d.seen_ms >= MIN_CAUSAL_MS;
            assert_eq!(warnings(&d).iter().any(|w| w.starts_with("AEC3 got the echo")), !causal, "{d:?}");
        }
    }

    /// An echo that reaches AEC3 before its reference, or less than 2 ms after it, is warned about; one that lands off the
    /// asked delay or moves, too.
    #[test]
    fn a_non_causal_echo_is_warned_about() {
        let scene = build(QualityTest::Echo, 12.0, None, 5.0, &UserAudio::default()).unwrap();
        let plan = Plan::new(5.0);
        let rig = Rig { rig_ms: -20.0, ..RIG };
        let start = plan.far_start(-20.0);
        let probe = plan.probe_track(&scene.input);
        let c = chain(&far_track(&plan, scene.far.as_deref().unwrap(), start.frames), &probe, rig, RIG.start + probe.len() + 48_000);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        let d = measure(&c, &scene, &plan, &cal, start.frames, None).unwrap();
        // A 5 ms echo path behind a 14 ms lead: AEC3 gets the echo 9 ms before its reference
        assert!((d.measured_ms - 5.0).abs() < SAMPLE_MS && (d.seen_ms + 9.0).abs() < SAMPLE_MS, "{d:?}");
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("AEC3 got the echo 9.0 ms before the reference it's given: the echo came 5.0 ms after"), "{}", w[0]);
        let just = EchoDelay { measured_ms: 15.5, seen_ms: 1.5, ..d.clone() };
        assert!(
            warnings(&EchoDelay { asked_ms: 15.5, low_ms: 15.5, high_ms: 15.5, ..just })[0].contains("only 1.5 ms after the reference")
        );
        let fine = EchoDelay { asked_ms: 25.0, measured_ms: 25.2, seen_ms: 11.2, lead_ms: 14.0, low_ms: 25.0, high_ms: 25.3, ..d };
        assert!(warnings(&fine).is_empty());
        let off = warnings(&EchoDelay { measured_ms: 14.0, seen_ms: 3.0, low_ms: 14.0, high_ms: 25.0, ..fine });
        assert_eq!(off.len(), 2, "{off:?}");
        assert!(off[0].contains("-11.0 ms off the 25.0 ms asked") && off[1].contains("moved by 11.0 ms"), "{off:?}");
    }

    /// The turns follow the scene wherever the far voice played: a recording that doesn't reach a turn leaves it out, and one
    /// whose reference is silent there has nothing to measure.
    #[test]
    fn turns_the_recording_misses_are_left_out() {
        let scene = build(QualityTest::Echo, 21.0, None, 25.0, &UserAudio::default()).unwrap();
        let plan = Plan::new(25.0);
        let start = plan.far_start(RIG.rig_ms);
        let probe = plan.probe_track(&scene.input);
        let track = far_track(&plan, scene.far.as_deref().unwrap(), start.frames);
        // Cut after the second far-only turn (9-12 s)
        let c = chain(&track, &probe, RIG, RIG.start + plan.scene_at as usize + 13 * 48_000);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        let d = measure(&c, &scene, &plan, &cal, start.frames, None).unwrap();
        assert_eq!((d.turns, d.turns_held), (2, 2));
        // Without the far voice
        let c = chain(&far_track(&plan, &[], start.frames), &probe, RIG, RIG.start + probe.len());
        let e = measure(&c, &scene, &plan, &cal, start.frames, None).unwrap_err();
        assert!(e.contains("didn't stand out") && e.contains("3 far-only turns"), "{e}");
        let short = chain(&track, &probe, RIG, RIG.start + plan.scene_at as usize + 24_000);
        assert_eq!(
            measure(&short, &scene, &plan, &cal, start.frames, None).unwrap_err(),
            "the chain recording doesn't hold a far-only turn"
        );
    }

    /// A take whose far end's playback slipped 70 ms later 20 s into the scene's probe track (the dev VM's `events-echo-2`,
    /// 2026-11-02), the engine's facts unmoved: the tracker finds it on the reference's side, to a window, by its size, at the
    /// far voice's own time (the far voice plays 65 ms ahead, so at its 20.065 s), and the mic held; the measured delay
    /// (searched as far as the rig may be off) gives the turns after it their true delay, 70 ms short, listed apart from the
    /// figure; the warning names the side, the time and the turns left out.
    #[test]
    fn a_70_ms_slip_of_the_far_end_is_found_and_its_turns_set_apart() {
        let scene = build(QualityTest::Echo, 30.0, None, 25.0, &UserAudio::default()).unwrap();
        let plan = Plan::new(25.0);
        let start = plan.far_start(RIG.rig_ms);
        let probe = plan.probe_track(&scene.input);
        let track = far_track(&plan, scene.far.as_deref().unwrap(), start.frames);
        let len = RIG.start + probe.len() + 48_000;
        let slip_at = RIG.start + plan.scene_at as usize + 20 * 48_000;
        let rig = Rig { slip: Some((slip_at, 3_360.0, Side::Reference)), ..RIG };
        let c = chain(&track, &probe, rig, len);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        let r = track_rig(&c, &scene, &plan, &cal, start.frames);
        assert_eq!(r.slips.len(), 1, "{:?}", r.slips);
        let s = r.slips[0];
        assert_eq!(s.side, Side::Reference);
        assert!((s.ms() - 70.0).abs() < 0.05, "{s:?}");
        // Between the last old window's start and the first new one's end, a second and a half apart, in the far voice's time
        let truth = 20.0 + plan.advance_ms(start.frames) / 1000.0;
        assert!((truth - 20.065).abs() < 1e-9, "{truth}");
        assert!(s.after_s() <= truth && truth <= s.at_s() && s.at - s.after == 72_000 && s.confirmed, "{s:?}");
        assert!(s.when().starts_with("at about "), "{}", s.when());
        let (mic_spread, mic_windows) = r.spread_ms(Side::Mic);
        let (ref_spread, ref_windows) = r.spread_ms(Side::Reference);
        assert!(mic_spread < 0.02 && ref_spread < 0.02 && mic_windows > 50 && ref_windows > 30, "{mic_spread} {ref_spread}");
        assert!(!r.slipped(Side::Mic) && r.slipped(Side::Reference));
        // The far-only turns at 0 and 9 s are before it; those at 18 (which holds it) and 27 s after
        let from = r.excluded_from().unwrap();
        let d = measure(&c, &scene, &plan, &cal, start.frames, Some(from)).unwrap();
        assert_eq!((d.turns, d.turns_held), (2, 2), "{d:?}");
        assert!((d.measured_ms - 25.0).abs() < 0.05 && d.high_ms - d.low_ms < 0.05, "{d:?}");
        assert_eq!(d.slipped.iter().map(|t| t.0).collect::<Vec<_>>(), [18.0, 27.0]);
        // The turn holding the slip (18-21 s, two thirds before it) gives the delay it mostly had; the one after, 70 ms sooner
        let (holding, after) = (d.slipped[0].1, d.slipped[1].1);
        assert!((holding - 25.0).abs() < 0.05 && (after + 45.0).abs() < 0.05, "{:?}", d.slipped);
        // Unflagged, the measurement sees the slip at its size (it searched 40 ms before, and said 40)
        let all = measure(&c, &scene, &plan, &cal, start.frames, None).unwrap();
        assert!((all.high_ms - all.low_ms - 70.0).abs() < 0.05, "{all:?}");
        assert!(warnings(&all).iter().any(|w| w.contains("moved by 70.0 ms")), "{:?}", warnings(&all));
        let w = slip_warnings(&r);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("the far end's loopback (the far voice in the filter's reference) by +70.0 ms at"), "{}", w[0]);
        assert!(w[0].contains("(the mic held)") && w[0].contains("the far-only turns from 18 s on are left out"), "{}", w[0]);
        assert!(w[0].contains("of the echo figures, and double talk from then on"), "{}", w[0]);
        assert!(rig_line(&r).ends_with(&format!("; the reference slipped +70.0 ms {}", s.when())), "{}", rig_line(&r));
    }

    /// The mic's side slipping by a VB-CABLE period (11 ms, the take of 2026-11-01 whose mic flipped) is found as the mic's;
    /// the engine's own moves (a jump of the reference's read position, which the facts show) aren't slips; nor is a window
    /// that disagrees with both its neighbours.
    #[test]
    fn the_mic_s_slip_is_its_own_and_the_engine_s_moves_are_none() {
        let scene = build(QualityTest::Echo, 21.0, None, 25.0, &UserAudio::default()).unwrap();
        let plan = Plan::new(25.0);
        let start = plan.far_start(RIG.rig_ms);
        let probe = plan.probe_track(&scene.input);
        let track = far_track(&plan, scene.far.as_deref().unwrap(), start.frames);
        let len = RIG.start + probe.len() + 48_000;
        let mic0 = RIG.start as f64 + (RIG.rig_ms - RIG.lead_ms) * 48.0 + plan.scene_at as f64;
        let rig = Rig { slip: Some(((mic0 + 7.0 * 48_000.0) as usize, 528.0, Side::Mic)), ..RIG };
        let c = chain(&track, &probe, rig, len);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        let r = track_rig(&c, &scene, &plan, &cal, start.frames);
        assert_eq!(r.slips.iter().map(|s| s.side).collect::<Vec<_>>(), [Side::Mic], "{:?}", r.slips);
        let m = r.slips[0];
        assert!((m.ms() - 11.0).abs() < 0.05 && m.after_s() <= 7.0 && 7.0 <= m.at_s() && m.at_s() - m.after_s() <= 1.5 + 1e-9, "{m:?}");
        // A jump of the reference's read position mid-scene (the engine's, in its facts): no slip
        let jumped = Rig { jump: Some((((RIG.start + plan.scene_at as usize) / BLOCK) + 1_000, -144)), ..RIG };
        let c = chain(&track, &probe, jumped, len);
        let r = track_rig(&c, &scene, &plan, &cal, start.frames);
        assert!(r.slips.is_empty(), "{:?}", r.slips);
        assert!(r.spread_ms(Side::Reference).0 < 0.02, "{:?}", r.spread_ms(Side::Reference));
        assert_eq!(r.excluded_from(), None);
        assert!(rig_line(&r).ends_with(": held"), "{}", rig_line(&r));
        assert!(slip_warnings(&r).is_empty());
    }

    /// The slip rule on its own: a step the next window confirms is a slip, a lone outlier isn't, and a slow drift isn't.
    #[test]
    fn a_slip_is_a_confirmed_step() {
        let points = |offsets: &[f64]| -> Vec<RigPoint> {
            offsets.iter().enumerate().map(|(k, &o)| RigPoint { at: k as i64 * 24_000, offset: o, peak: 1.0 }).collect()
        };
        let slips = |offsets: &[f64]| slips_of(Side::Mic, &points(offsets), -48_000, 24_000);
        assert!(slips(&[0.0, 0.0, 500.0, 0.0, 0.0]).1.is_empty(), "an outlier");
        let (kept, s) = slips(&[0.0, 0.0, 528.0, 528.0, 528.0]);
        // After the last old window's start (centred at 0.5 s), before the first new one's end (centred at 1 s)
        assert_eq!((s.len(), s[0].after, s[0].at, s[0].samples, kept.len()), (1, 0, 72_000, 528.0, 5));
        assert_eq!(s[0].when(), "at about 0.8 s");
        // A slip before the first window: from the marker
        let (_, s) = slips(&[528.0, 528.0]);
        assert_eq!((s[0].after, s[0].at), (-48_000, 24_000));
        // Far apart (a device that played nothing between): a span
        let far_apart = [RigPoint { at: 24_000, offset: 0.0, peak: 1.0 }, RigPoint { at: 120_000, offset: 3_360.0, peak: 1.0 }, RigPoint {
            at: 144_000,
            offset: 3_360.0,
            peak: 1.0,
        }];
        assert_eq!(slips_of(Side::Reference, &far_apart, 0, 24_000).1[0].when(), "between 0.0 and 3.0 s");
        let drift: Vec<f64> = (0..100).map(|k| k as f64 * 1.5).collect();
        assert!(slips(&drift).1.is_empty(), "30 ppm-ish of drift, 1.5 samples a window, is no slip");
        // A step in the last window: reported, unconfirmed, not kept, nothing set apart
        let (kept, s) = slips(&[0.0, 0.0, 3_360.0]);
        assert_eq!((s.len(), s[0].confirmed, kept.len()), (1, false, 2), "{s:?}");
        let track = RigTrack { mic: kept, slips: s, ..RigTrack::default() };
        assert_eq!(track.excluded_from(), None);
        assert!(track.held(Side::Mic) && !track.slipped(Side::Mic));
        assert!(rig_line(&track).ends_with("; the mic's last window +70.0 ms off at about 0.8 s, unconfirmed"), "{}", rig_line(&track));
        let w = slip_warnings(&track);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].starts_with("The last window of the mic (the probe's track in the filter's mic) was +70.0 ms off the rest"),
            "{}",
            w[0]
        );
        assert!(w[0].ends_with("the echo figures keep the turns there."), "{}", w[0]);
    }

    /// A slip past the 250 ms the plan makes up for (300 ms either way, on either side) is found again by the wider look, at
    /// its size; one past that (3 s) loses the track, and the turns from the last window found on are set apart as for a slip.
    #[test]
    fn a_slip_past_the_search_is_found_or_the_track_lost() {
        let scene = build(QualityTest::Echo, 30.0, None, 25.0, &UserAudio::default()).unwrap();
        let plan = Plan::new(25.0);
        let start = plan.far_start(RIG.rig_ms);
        let probe = plan.probe_track(&scene.input);
        let track = far_track(&plan, scene.far.as_deref().unwrap(), start.frames);
        let len = RIG.start + probe.len() + 48_000;
        let mic0 = RIG.start as f64 + (RIG.rig_ms - RIG.lead_ms) * 48.0 + plan.scene_at as f64;
        let at20 = |side: Side| {
            if side == Side::Mic { (mic0 + 20.0 * 48_000.0) as usize } else { RIG.start + plan.scene_at as usize + 20 * 48_000 }
        };
        for (side, by) in [(Side::Reference, 14_400.0), (Side::Reference, -14_400.0), (Side::Mic, 14_400.0)] {
            let c = chain(&track, &probe, Rig { slip: Some((at20(side), by, side)), ..RIG }, len);
            let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
            let r = track_rig(&c, &scene, &plan, &cal, start.frames);
            assert!(r.lost.is_empty() && r.slips.len() == 1, "{side:?} {by}: {:?} {:?}", r.slips, r.lost);
            let s = r.slips[0];
            assert!(s.side == side && s.confirmed && (s.samples - by).abs() < 2.0, "{side:?} {by}: {s:?}");
            assert!(s.after_s() <= 20.1 && 19.9 <= s.at_s(), "{s:?}");
            assert_eq!(r.excluded_from().map(far_only_from), Some(18 * 48_000), "{side:?} {by}");
            // The windows after it are kept at the new offset (but the two or three about it, which straddle it)
            let (_, windows) = r.spread_ms(side);
            let unslipped = track_rig(&chain(&track, &probe, RIG, len), &scene, &plan, &cal, start.frames).spread_ms(side).1;
            assert!(windows + 4 >= unslipped, "{side:?} {by}: {windows} of {unslipped}");
        }
        // 3 s later: past the wider look too; lost from the last window found, before 20 s
        let c = chain(&track, &probe, Rig { slip: Some((at20(Side::Reference), 144_000.0, Side::Reference)), ..RIG }, len);
        let Marker::Found(cal) = find_marker(&c, &plan.marker, FROM) else { panic!() };
        let r = track_rig(&c, &scene, &plan, &cal, start.frames);
        assert!(r.slips.is_empty() && r.lost.len() == 1, "{:?} {:?}", r.slips, r.lost);
        let l = r.lost[0];
        assert!(l.side == Side::Reference && l.windows >= RIG_LOST_WINDOWS && l.after_s() < 20.1 && l.after_s() > 18.0, "{l:?}");
        assert!(r.held(Side::Mic) && !r.held(Side::Reference));
        assert_eq!(r.excluded_from(), Some(l.after as usize));
        assert!(rig_line(&r).contains(&format!(
            "; the reference's track lost from {:.1} s ({} windows not found)",
            l.after_s(),
            l.windows
        )));
        let w = slip_warnings(&r);
        assert!(w.len() == 1 && w[0].contains("lost from") && w[0].contains("(the mic held)") && w[0].contains("from 18 s on"), "{w:?}");
    }

    /// A saved take's chain recording, rebuilt from `quality --save`'s files: `chain_mic.wav`, `chain_ref.wav` and the facts
    /// the measurements read from `chain_blocks.csv`.
    fn saved_chain(dir: &std::path::Path) -> ChainRecording {
        let read = |name: &str| crate::wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        let (mic, reference) = (read("chain_mic.wav"), read("chain_ref.wav"));
        let csv = std::fs::read_to_string(dir.join("chain_blocks.csv")).unwrap();
        let mut rows = csv.lines();
        let header: Vec<&str> = rows.next().unwrap().split(',').collect();
        let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
        let (index, mic_index, mic_skipped, read_index, lead, reading, stamped, consumed) = (
            col("index"),
            col("mic_index"),
            col("mic_skipped"),
            col("ref_read_index"),
            col("lead_ms"),
            col("lead_reading_ms"),
            col("ref_stamped"),
            col("consumed"),
        );
        let blocks: Vec<ChainBlock> = rows
            .map(|line| {
                let f: Vec<&str> = line.split(',').collect();
                let num = |i: usize| f[i].parse::<f64>().unwrap_or(f64::NAN);
                ChainBlock {
                    index: f[index].parse().unwrap(),
                    capture_qpc_hns: f64::NAN,
                    mic_stamped: true,
                    mic_index: f[mic_index].parse().unwrap(),
                    mic_wake_hns: f64::NAN,
                    mic_queued: 0,
                    mic_discarded: 0,
                    mic_skipped: f[mic_skipped].parse().unwrap_or(0),
                    reference: (!f[read_index].is_empty()).then(|| ReferenceFacts {
                        read_index: f[read_index].parse().unwrap(),
                        lead_reading_ms: num(reading) as f32,
                        lead_ms: num(lead) as f32,
                        stamped: f[stamped] == "1",
                        consumed: f[consumed].parse().unwrap_or(480),
                        ..ReferenceFacts::default()
                    }),
                    aec_delay_ms: f32::NAN,
                    aec_erle_db: f32::NAN,
                    settings: ChainParams::default(),
                    budget: None,
                    output: Default::default(),
                }
            })
            .collect();
        ChainRecording { mic, reference, blocks, ..Default::default() }
    }

    /// The rig tracker on a saved calibrated take (`quality --test echo --save DIR` from 2026-11-01's calibration on: the marker
    /// before the scene, the far voice as played in `far.wav`, the synthetic scene): the marker found in the chain recording,
    /// the far voice's start from `far.wav`, then both devices tracked, and the echo delay measured with and without the turns
    /// after a slip. Ignored (it reads files): `NF_QUALITY_TAKE=DIR NF_QUALITY_SECONDS=60 NF_QUALITY_DELAY=25 cargo test -p
    /// nf-app --release a_saved_takes_rig -- --ignored --nocapture`. On the dev VM's `events-echo-2` (2026-11-02) it finds the
    /// far end's slip (TODO 14's detail file, "A rig slip").
    #[test]
    #[ignore]
    fn a_saved_takes_rig() {
        let Ok(dir) = std::env::var("NF_QUALITY_TAKE") else { return };
        let dir = std::path::Path::new(&dir);
        let seconds: f32 = std::env::var("NF_QUALITY_SECONDS").map_or(60.0, |s| s.parse().unwrap());
        let delay: f64 = std::env::var("NF_QUALITY_DELAY").map_or(25.0, |s| s.parse().unwrap());
        let read = |name: &str| crate::wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        let chain = saved_chain(dir);
        let (input, played) = (read("input.wav"), read("far.wav"));
        let scene = build(QualityTest::Echo, seconds, None, delay as f32, &UserAudio::default()).unwrap();
        assert_eq!(scene.input.len(), input.len(), "--seconds as the take's (NF_QUALITY_SECONDS)");
        let plan = Plan::new(delay);
        let cal = (0..chain.mic.len())
            .step_by(4 * 48_000)
            .find_map(|from| match find_marker(&chain, &plan.marker, from) {
                Marker::Found(c) => Some(c),
                _ => None,
            })
            .expect("the marker in the chain recording");
        // far.wav is the far voice as played against the input: its lag behind the scene's far voice is the advance
        let far = scene.far.as_deref().unwrap();
        let p = xcorr_peak(&far[..480_000], &played[..960_000.min(played.len())], 96_000).unwrap();
        let start = (plan.scene_at as f64 + p.delay).round() as u64;
        let rig = track_rig(&chain, &scene, &plan, &cal, start);
        println!(
            "{}: the marker at {:.1} / {:.1} (rig {:.2} ms); the far voice from {start} (far.wav at {:.2}, peak {:.2})",
            dir.display(),
            cal.mic_at,
            cal.ref_at,
            cal.rig_ms,
            p.delay,
            p.peak
        );
        println!("  rig: {}", rig_line(&rig));
        for w in slip_warnings(&rig) {
            println!("  warning: {w}");
        }
        let show = |what: &str, d: Result<EchoDelay, String>| match d {
            Ok(d) => println!(
                "  {what}: {:.2} ms after the tap ({} turns, {:.2} to {:.2}); after the slip {:?}",
                d.measured_ms, d.turns, d.low_ms, d.high_ms, d.slipped
            ),
            Err(e) => println!("  {what}: {e}"),
        };
        show("every turn", measure(&chain, &scene, &plan, &cal, start, None));
        show("before the slip", measure(&chain, &scene, &plan, &cal, start, rig.excluded_from()));
    }

    /// The measurement on a live take saved before the calibration existed (`quality --test echo --seconds S --save DIR`, the
    /// far voice then a fixed 100 ms ahead, no marker): the scene is found in the chain recording by its own tracks instead
    /// (`input.wav` in `chain_mic.wav`, `far.wav` in `chain_ref.wav`), then measured as a calibrated take is. Ignored (it
    /// reads files): `NF_QUALITY_TAKE=DIR NF_QUALITY_SECONDS=60 cargo test -p nf-app --release a_saved_take_measures --
    /// --ignored --nocapture`. On the dev VM's six chain-recorded takes (2026-11-01) it gave 14.5, 24.9, 31.3, 42.0, 52.4 and
    /// 63.4 ms, all seven far-only turns of each within 0.1 ms of each other but for the first take's first (3.3 ms: the cable
    /// flipped).
    #[test]
    #[ignore]
    fn a_saved_take_measures() {
        let Ok(dir) = std::env::var("NF_QUALITY_TAKE") else { return };
        let dir = std::path::Path::new(&dir);
        let seconds: f32 = std::env::var("NF_QUALITY_SECONDS").map_or(60.0, |s| s.parse().unwrap());
        let read = |name: &str| crate::wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        let (input, far) = (read("input.wav"), read("far.wav"));
        let chain = saved_chain(dir);
        // Where each track's first 10 s start in the chain's first 20 s.
        let start_in = |x: &[f32], y: &[f32]| xcorr_peak(&y[..480_000], &x[..960_000.min(x.len())], 960_000).unwrap().delay;
        let (probe_at, far_at) = (start_in(&chain.mic, &input), start_in(&chain.reference, &far));
        // The old layout as a plan: the scene at 100 ms, the far voice (as played) at 0 and so 100 ms ahead.
        let old = Plan { delay_ms: f64::NAN, marker: Vec::new(), earliest: 0, scene_at: 4_800, latest: 4_800 };
        let cal = Calibration {
            mic_at: probe_at - 4_800.0,
            ref_at: far_at - 4_800.0,
            mic_confidence_db: f32::NAN,
            ref_confidence_db: f32::NAN,
            lead_ms: f64::NAN,
            stamped: true,
            rig_ms: f64::NAN,
        };
        let scene = build(QualityTest::Echo, seconds, None, 25.0, &UserAudio::default()).unwrap();
        assert_eq!(scene.input.len(), input.len(), "--seconds as the take's (NF_QUALITY_SECONDS)");
        let d = measure(&chain, &scene, &old, &cal, 0, None).unwrap();
        println!(
            "{}: the echo {:.2} ms after its reference's tap ({} of {} far-only turns, {:.2} to {:.2}); AEC3's lag {:.2} ms, \
             the lead {:.2} ms",
            dir.display(),
            d.measured_ms,
            d.turns,
            d.turns_held,
            d.low_ms,
            d.high_ms,
            d.seen_ms,
            d.lead_ms
        );
    }
}
