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
//!   chain's mic behind the chain's reference (GCC-PHAT, within [`SEARCH_MS`] of where the calibration put it), which is what
//!   AEC3 sees, plus the lead over the turn: the delay after the reference's tap. The median turn is reported, with the range.

use nf_core::latency::analysis::{gcc_phat, xcorr_peak};
use nf_core::latency::sweep::exp_sweep;
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_engine::{ChainBlock, ChainRecording};

use super::signals::{Scene, Turn, turn_at, turn_len};

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
/// The measurement looks this far either side of the delay the calibration expects: room for a jump of the reference's read
/// position after it (a change of the lead target), or a cable that slips by a period.
pub const SEARCH_MS: f64 = 40.0;
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
/// even count).
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// Far-only turns measured, of those the recording holds.
    pub turns: usize,
    pub turns_held: usize,
}

/// Measures the echo's delay in `chain`, the take's whole chain recording, where `cal` found the marker and the far voice
/// started at `far_start` (see the module notes).
pub fn measure(chain: &ChainRecording, scene: &Scene, plan: &Plan, cal: &Calibration, far_start: u64) -> Result<EchoDelay, String> {
    let turn = turn_len();
    let n = chain.mic.len().min(chain.reference.len()) as i64;
    // The lag the calibration expects in the chain, and where the scene's input starts in the chain's mic.
    let expected = (cal.mic_at - cal.ref_at + plan.scene_at as f64 - far_start as f64).round() as i64;
    let scene0 = cal.mic_at.round() as i64 + plan.scene_at as i64;
    let search = to_samples(SEARCH_MS).round() as usize;
    let (mut turns, mut held) = (Vec::new(), 0usize);
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
        turns.push((seen + lead, seen, lead));
    }
    if turns.is_empty() {
        return Err(if held == 0 {
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
        turns_held: held,
    })
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
    /// earlier).
    #[derive(Clone, Copy)]
    struct Rig {
        lead_ms: f64,
        rig_ms: f64,
        start: usize,
        jump: Option<(usize, i64)>,
    }

    const RIG: Rig = Rig { lead_ms: 14.0, rig_ms: -40.0, start: 120_000, jump: None };
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
        let mic = place(probe_track, rig.start as f64 + (rig.rig_ms - rig.lead_ms) * 1e-3 * FS);
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
            let d = measure(&c, &scene, &plan, &again, start.frames).unwrap();
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
        let d = measure(&c, &scene, &plan, &cal, start.frames).unwrap();
        // A 5 ms echo path behind a 14 ms lead: AEC3 gets the echo 9 ms before its reference
        assert!((d.measured_ms - 5.0).abs() < SAMPLE_MS && (d.seen_ms + 9.0).abs() < SAMPLE_MS, "{d:?}");
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("AEC3 got the echo 9.0 ms before the reference it's given: the echo came 5.0 ms after"), "{}", w[0]);
        let just = EchoDelay { measured_ms: 15.5, seen_ms: 1.5, ..d };
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
        let d = measure(&c, &scene, &plan, &cal, start.frames).unwrap();
        assert_eq!((d.turns, d.turns_held), (2, 2));
        // Without the far voice
        let c = chain(&far_track(&plan, &[], start.frames), &probe, RIG, RIG.start + probe.len());
        let e = measure(&c, &scene, &plan, &cal, start.frames).unwrap_err();
        assert!(e.contains("didn't stand out") && e.contains("3 far-only turns"), "{e}");
        let short = chain(&track, &probe, RIG, RIG.start + plan.scene_at as usize + 24_000);
        assert_eq!(measure(&short, &scene, &plan, &cal, start.frames).unwrap_err(), "the chain recording doesn't hold a far-only turn");
    }

    /// The measurement on a live take saved before the calibration existed (`quality --test echo --seconds S --save DIR`, the
    /// far voice then a fixed 100 ms ahead, no marker): the scene is found in the chain recording by its own tracks instead
    /// (`input.wav` in `chain_mic.wav`, `far.wav` in `chain_ref.wav`), then measured as a calibrated take is. Ignored (it
    /// reads files): `NF_QUALITY_TAKE=DIR NF_QUALITY_SECONDS=60 cargo test -p nf-app --release a_saved_take -- --ignored
    /// --nocapture`. On the dev VM's six chain-recorded takes (2026-11-01) it gave 14.5, 24.9, 31.3, 42.0, 52.4 and 63.4 ms,
    /// all seven far-only turns of each within 0.1 ms of each other but for the first take's first (3.3 ms: the cable flipped).
    #[test]
    #[ignore]
    fn a_saved_take_measures() {
        let Ok(dir) = std::env::var("NF_QUALITY_TAKE") else { return };
        let dir = std::path::Path::new(&dir);
        let seconds: f32 = std::env::var("NF_QUALITY_SECONDS").map_or(60.0, |s| s.parse().unwrap());
        let read = |name: &str| crate::wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        let (mic, reference, input, far) = (read("chain_mic.wav"), read("chain_ref.wav"), read("input.wav"), read("far.wav"));
        let csv = std::fs::read_to_string(dir.join("chain_blocks.csv")).unwrap();
        let mut rows = csv.lines();
        let header: Vec<&str> = rows.next().unwrap().split(',').collect();
        let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
        let (index, mic_index, read_index, lead, reading, stamped) =
            (col("index"), col("mic_index"), col("ref_read_index"), col("lead_ms"), col("lead_reading_ms"), col("ref_stamped"));
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
                    mic_skipped: 0,
                    reference: (!f[read_index].is_empty()).then(|| ReferenceFacts {
                        read_index: f[read_index].parse().unwrap(),
                        lead_reading_ms: num(reading) as f32,
                        lead_ms: num(lead) as f32,
                        stamped: f[stamped] == "1",
                        ..ReferenceFacts::default()
                    }),
                    aec_delay_ms: f32::NAN,
                    aec_erle_db: f32::NAN,
                    settings: ChainParams::default(),
                    budget: None,
                }
            })
            .collect();
        let chain = ChainRecording { mic, reference, blocks, ..Default::default() };
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
        let d = measure(&chain, &scene, &old, &cal, 0).unwrap();
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
