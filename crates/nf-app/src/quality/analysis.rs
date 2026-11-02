//! The quality test's analysis: aligns the recording with the scene's clean near speech, then compares levels over regions of
//! the scene. [`analyse_with`] is a pure function of the signals, so it's unit-tested on synthetic recordings.
//!
//! - **Alignment**, in two steps, so that a path whose delay drifts, wanders or steps during the take still lines up (one lag
//!   for the whole take lost the scene once the delay moved by about 1.5 ms):
//!   - a coarse delay: the cross-correlation of the clean speech's and the recording's 5 ms frame envelopes, over delays up to
//!     the lead plus [`MAX_PATH_DELAY_S`]. An envelope barely changes when the delay moves by a few ms;
//!   - then [`WINDOW_S`] windows of the clean speech (those with at least 0.5 s of speech in them), each cross-correlated with
//!     the recording within 50 ms of the coarse delay (nf-core's `xcorr_peak`), for its delay and its correlation coefficient.
//!     The windows' median coefficient decides: below [`MIN_CORRELATION`] the recording doesn't hold the scene, and the test
//!     has no result. The windows that reach it, less any that found the delay a pitch period off their neighbours, make a
//!     delay track (linear between their centres, held beyond them), which the recording is read along to give the aligned
//!     recording. Their spread and a straight-line fit (the drift, in ppm) are reported, with a warning when the delay moved by
//!     more than 2 ms. Each step of the track over 2 ms is looked into ([`delay_steps`]): a run of digital silence in the
//!     recording there, about as long as the step, is samples inserted after the filter (on the dev VM's cables, most likely
//!     the output cable's own, or the recorder's filling a jump of the capture's position: TODO 9), and the warning says so.
//! - **Regions**, in the clean signal's time, and only as far as the recording reaches:
//!   - near busy: within 50 ms of near speech activity; far busy: from 50 ms before far activity to 50 ms plus the echo's
//!     150 ms tail after it;
//!   - cores: each activity run with 40 ms trimmed off both ends (runs shorter than 80 ms are skipped);
//!   - noise test, from the first second on (the filter settles in it): noise in pauses (not near busy; input vs output), and
//!     the same at least 200 ms from speech (the steady state, past the filter's release around speech); speech in its cores;
//!   - echo test, each region inside its own kind of turn ([`signals::turn_at`]), from the second turn on: the first, far-only
//!     turn is the echo canceller's to converge in, and is reported on its own. The echo where only the far end talks (far
//!     cores in far-only turns, not near busy; input vs output, the drop being the ERLE), pooled and per turn, with the scene's
//!     noise over the same samples (it caps the drop the analysis can see while it passes). The near speech's cores in
//!     near-only turns (not far busy), in double-talk turns (far busy), and in double-talk turns where the far voice is
//!     mid-syllable too (far cores).
//!   - after a rig slip ([`analyse_with`]'s `slip_from`, from `echo_delay::track_rig`): the far-only turns from the one that
//!     holds the slip on are left out of the pooled echo figure and listed apart, and the double-talk figures stop there too
//!     (the canceller had a misaligned reference from then on): what the echo did there is the rig's.
//! - **Levels** are RMS over each region in dBFS; a change is the output's level minus the reference's. The speech regions
//!   measure the speech's own share of the output, so echo and noise left there don't count as speech: per 20 ms frame, the
//!   output's least-squares projection on the clean speech (gain `<y,c>/<c,c>`). What's left over (echo, noise, and the
//!   filters' own changes to the speech's spectrum) is reported as the rest.
//!
//! [`signals::turn_at`]: super::signals::turn_at

use std::ops::Range;

use nf_core::SAMPLE_RATE;
use nf_core::latency::analysis::xcorr_peak;

use super::QualityTest;
use super::signals::{RIR_S, Scene, Turn, far_only_from, turn_at, turn_len};

/// The weakest median window coefficient that counts as aligned.
///
/// Measured on 30 s synthetic scenes run offline through the same chain, with the windowed alignment (2026-11-01, the
/// ignored `the_alignment_threshold_holds_through_the_chain` test in commands/quality.rs). Aligned recordings, the delay
/// steady: the noise test's input itself (the filter passing everything) 0.91 at the default 10 dB SNR and 0.58 at 0 dB,
/// through RNNoise 0.97 and 0.89; the echo test's input (AEC off) 0.89, through AEC3 0.74, through AEC3 and RNNoise 0.71. The
/// delay drifting 20 or 100 ppm cost each of them 0.03 at most; wandering ±1 ms over 30 s, or stepping by 1 ms, 0.06; wandering
/// ±1 ms over 10 s, 0.16. The lowest of all was 0.49 (the 0 dB input with the 10 s wander), and a 300 s take drifting 50 ppm
/// (15 ms over the take) gave 0.97. Recordings without the scene: another synthetic talker 0.07 (through RNNoise too), the
/// noise alone 0.03, the echo alone 0.04, the scene 4 s off (outside the search) 0.10. 0.3 is about 0.6 of the lowest aligned
/// case and three times the highest misaligned one. `--snr` stops at 0 dB so that a scene played with the filter off stays
/// aligned.
pub const MIN_CORRELATION: f32 = 0.3;
/// The longest path (probe to capture) the alignment looks for, past the lead.
pub const MAX_PATH_DELAY_S: f64 = 2.0;
/// The alignment's windows, and how far from the coarse delay each one looks.
pub const WINDOW_S: f64 = 2.0;
const WINDOW_SEARCH_S: f64 = 0.05;
/// A window needs this much speech activity to be measured.
const MIN_WINDOW_SPEECH_S: f64 = 0.5;
/// The coarse delay's envelope frames (5 ms).
const ENVELOPE_FRAME: usize = 240;
/// A delay that moves by more than this over the take earns a warning, and a step between two windows by more than it is
/// looked into ([`delay_steps`]).
const DELAY_MOVE_WARN_MS: f64 = 2.0;
/// A run of exact zeros in the recording at least this long (1 ms) can be a hole in it.
const MIN_SILENCE: usize = 48;
/// A window this far from the median delay of the windows around it lined up a pitch period off, and is left out.
const MAX_SLIP_MS: f64 = 3.0;
/// The noise test's metrics skip the scene's first second, while the noise filter converges.
const SETTLE_S: f64 = 1.0;
/// The margin kept from speech activity, the steady-state pauses' margin, and what is trimmed off each run for its core.
const MARGIN_S: f64 = 0.05;
const STEADY_MARGIN_S: f64 = 0.2;
const CORE_TRIM_S: f64 = 0.04;
/// A region shorter than this is reported with a warning (and the steady-state pauses aren't reported).
pub const MIN_REGION_S: f64 = 0.25;
/// The speech regions' projection frames (20 ms).
const PROJECTION_FRAME: usize = 960;
/// Near speech left in the pauses within this of the noise there earns a warning (and `noise-filter lab` leaves the clip's pause
/// figures out of its summary).
pub const PAUSE_SPEECH_WARN_DB: f32 = 25.0;
/// An echo drop within this of the scene's ceiling (or past it) earns a warning.
const CEILING_WARN_DB: f32 = 3.0;

/// Where the scene lies in the recording, and how steadily.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Alignment {
    /// Where the clean signal's first sample is in the recording (the median of the windows that lined up): rounded to a
    /// sample, and as found.
    pub delay: i64,
    pub delay_exact: f64,
    /// The windows' median correlation coefficient (what decides), and the lowest.
    pub correlation: f32,
    pub correlation_low: f32,
    /// The lined-up windows' median confidence: how far each one's peak stands above the rest of its correlation, in dB.
    pub confidence_db: f32,
    /// The windows measured, and those that lined up (their coefficient reached [`MIN_CORRELATION`]).
    pub windows: usize,
    pub windows_aligned: usize,
    /// The lined-up windows' lowest and highest delay, in samples, and the straight line through their delays as a drift in
    /// ppm (positive: the delay grows; NaN with fewer than two windows).
    pub delay_low: f64,
    pub delay_high: f64,
    pub drift_ppm: f64,
}

impl Alignment {
    /// The delay past `lead` (where the scene starts in the recording: the lead, and the echo test's calibration), in ms: about
    /// the probe-to-capture latency (the capture starts just before the tracks).
    pub fn path_ms(&self, lead: usize) -> f64 {
        (self.delay_exact - lead as f64) * 1000.0 / f64::from(SAMPLE_RATE)
    }

    /// How far the delay moved over the take, in ms.
    pub fn delay_spread_ms(&self) -> f64 {
        (self.delay_high - self.delay_low) * 1000.0 / f64::from(SAMPLE_RATE)
    }
}

/// The levels over one region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Levels {
    pub seconds: f64,
    /// The reference (the input, or the clean speech) and the aligned recording, RMS in dBFS; NaN for an empty region.
    pub reference_dbfs: f32,
    pub output_dbfs: f32,
}

impl Levels {
    /// The output's level minus the reference's, in dB.
    pub fn change_db(&self) -> f32 {
        self.output_dbfs - self.reference_dbfs
    }
}

/// The near speech over one region: the clean speech, the output, and the output split into the speech's own share (its
/// projection on the clean speech) and the rest. RMS in dBFS; NaN for an empty region. The region's change is the speech's
/// share minus the clean speech: what the filters did to the speech itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeechLevels {
    pub seconds: f64,
    pub clean_dbfs: f32,
    pub output_dbfs: f32,
    pub speech_dbfs: f32,
    pub rest_dbfs: f32,
}

/// The echo where only the far end talks, and the scene's noise over the same samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoLevels {
    /// The input (the echo and the noise) and the output.
    pub levels: Levels,
    pub noise_dbfs: f32,
}

impl EchoLevels {
    /// The drop from the input to the output: the ERLE (with the noise filter on, the whole chain's echo reduction).
    pub fn drop_db(&self) -> f32 {
        -self.levels.change_db()
    }

    /// The most the drop can show while the scene's noise passes untouched: the input over the noise.
    pub fn ceiling_db(&self) -> f32 {
        self.levels.reference_dbfs - self.noise_dbfs
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Metrics {
    /// The noise in pauses (input vs output), the same in the pauses' steady state, and the speech in its cores.
    Noise { pauses: Levels, steady_pauses: Levels, speech: SpeechLevels },
    /// The echo where only the far end talks (input vs output), pooled over the far-only turns after the first, each of those
    /// turns, and the first, converging turn from the first second on. The near speech where only it talks, in double talk,
    /// and where both talkers are mid-syllable.
    Echo {
        far_only: EchoLevels,
        far_only_turns: Vec<Levels>,
        /// The far-only turns from a rig slip's turn on (left out of `far_only`), and the scene sample that turn starts at.
        slipped_turns: Vec<Levels>,
        slipped_from: Option<usize>,
        converging: Levels,
        near_only: SpeechLevels,
        double_talk: SpeechLevels,
        both_talking: SpeechLevels,
    },
}

impl Metrics {
    /// Each main region, which takes a length warning: its name and its length in seconds. (The steady-state pauses and both
    /// talkers mid-syllable are a closer look at a main region, shown with their lengths.)
    fn region_seconds(&self) -> Vec<(&'static str, f64)> {
        match self {
            Metrics::Noise { pauses, speech, .. } => vec![("noise in pauses", pauses.seconds), ("speech in its cores", speech.seconds)],
            Metrics::Echo { far_only, near_only, double_talk, .. } => vec![
                ("echo where only the far end talks (after the first turn)", far_only.levels.seconds),
                ("near speech alone", near_only.seconds),
                ("near speech in double talk", double_talk.seconds),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Analysis {
    pub alignment: Alignment,
    /// The recording read along the delay track, as long as the clean signal (zeros where the recording doesn't reach).
    pub aligned: Vec<f32>,
    pub metrics: Metrics,
    /// Where the delay stepped during the take, and the silence in the recording that explains a step, if any.
    pub delay_steps: Vec<DelayStep>,
    pub warnings: Vec<String>,
}

/// A step of the path's delay between two windows of the alignment (TODO 9).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayStep {
    /// About where in the scene (s): at the silence that explains it, else midway between the two windows' centres.
    pub at_s: f64,
    /// How far the delay moved (ms; positive: the recording came later).
    pub ms: f64,
    /// A run of digital silence (exact zeros) in the recording between the two windows, about as long as the step: where it
    /// starts in the recording (s) and how long it is (ms). Silence inserted into the stream after the filter delays everything
    /// after it by its length.
    pub silence: Option<(f64, f64)>,
}

/// The steps of the delay `track` (each window's centre in the clean signal, and its delay, in samples) over
/// [`DELAY_MOVE_WARN_MS`], each with the run of exact zeros in `recording` between the two windows that is closest to its
/// length, when that's within 2 ms or a quarter of the step (a step the delay grew by; a drop has no silence to show).
pub fn delay_steps(track: &[(f64, f64)], recording: &[f32]) -> Vec<DelayStep> {
    let fs = f64::from(SAMPLE_RATE);
    let min_step = DELAY_MOVE_WARN_MS * 1e-3 * fs;
    let mut steps = Vec::new();
    for w in track.windows(2) {
        let ((c0, d0), (c1, d1)) = (w[0], w[1]);
        let step = d1 - d0;
        if step.abs() <= min_step {
            continue;
        }
        let (from, to) = ((c0 + d0).max(0.0) as usize, ((c1 + d1).max(0.0) as usize).min(recording.len()));
        let silence = (step > 0.0)
            .then(|| {
                zero_runs(recording.get(from..to).unwrap_or(&[]))
                    .map(|(at, n)| (from + at, n))
                    .min_by(|a, b| (a.1 as f64 - step).abs().total_cmp(&(b.1 as f64 - step).abs()))
            })
            .flatten()
            .filter(|&(_, n)| (n as f64 - step).abs() <= (2e-3 * fs).max(0.25 * step));
        steps.push(DelayStep {
            at_s: silence.map_or((c0 + c1) / 2.0, |(at, _)| at as f64 - d0) / fs,
            ms: step * 1e3 / fs,
            silence: silence.map(|(at, n)| (at as f64 / fs, n as f64 * 1e3 / fs)),
        });
    }
    steps
}

/// The runs of exact zeros in `x` at least [`MIN_SILENCE`] long: (start, length).
fn zero_runs(x: &[f32]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < x.len() {
            if x[i] != 0.0 {
                i += 1;
                continue;
            }
            let start = i;
            while i < x.len() && x[i] == 0.0 {
                i += 1;
            }
            if i - start >= MIN_SILENCE {
                return Some((start, i - start));
            }
        }
        None
    })
}

/// A sentence a step: where, how far, and what explains it.
fn step_text(s: &DelayStep) -> String {
    let at = format!("It stepped by {:+.1} ms about {:.1} s into the scene", s.ms, s.at_s);
    match s.silence {
        Some((t, ms)) => format!(
            "{at}, where the recording holds {ms:.1} ms of digital silence ({t:.2} s into it): samples inserted after the filter, \
             by the output cable or the recording (the filter's own output events are listed below)."
        ),
        None => format!("{at}; no silence in the recording explains it (compare the filter's output events below)."),
    }
}

/// Why the recording couldn't be aligned, with what the windows found if there were any.
#[derive(Clone, Debug, PartialEq)]
pub struct AlignmentFailure {
    pub alignment: Option<Alignment>,
    pub reason: String,
}

/// Aligns `recording` (which starts `lead` samples before the scene was played) with the scene's clean speech, and measures it
/// ([`analyse_with`] without a rig slip: the tests' shorthand).
#[cfg(test)]
pub fn analyse(scene: &Scene, recording: &[f32], lead: usize) -> Result<Analysis, AlignmentFailure> {
    analyse_with(scene, recording, lead, None)
}

/// Aligns `recording` (which starts `lead` samples before the scene was played) with the scene's clean speech, and measures it;
/// in the echo test, with the far-only turns from the one holding scene sample `slip_from` on left out of the pooled echo
/// figure (a rig slip there: `echo_delay::RigTrack::excluded_from`) and listed apart, and the double-talk figures stopped there.
pub fn analyse_with(scene: &Scene, recording: &[f32], lead: usize, slip_from: Option<usize>) -> Result<Analysis, AlignmentFailure> {
    let (alignment, track) = align(scene, recording, lead)?;
    let delay_steps = delay_steps(&track, recording);
    let len = scene.clean.len();
    let (aligned, covered) = follow(recording, &track, len);
    let fs = f64::from(SAMPLE_RATE);
    let samples = |s: f64| (s * fs).round() as usize;

    let mut warnings = Vec::new();
    if alignment.delay < lead as i64 {
        warnings.push(format!(
            "The scene arrived {:.1} ms before it was played, which no real path does: check that --capture records the filter's output.",
            -alignment.path_ms(lead)
        ));
    }
    if covered < len {
        warnings.push(format!(
            "The recording ends {:.2} s before the scene does; the metrics leave that part out.",
            (len - covered) as f64 / fs
        ));
    }
    if alignment.delay_spread_ms() > DELAY_MOVE_WARN_MS {
        let mut w = format!(
            "The path's delay moved by {:.1} ms during the take (drift {:+.1} ppm): the recording was aligned window by window, but \
             the levels near each move are less certain. Check the filter's drift and dropouts below.",
            alignment.delay_spread_ms(),
            alignment.drift_ppm
        );
        for step in &delay_steps {
            w.push(' ');
            w.push_str(&step_text(step));
        }
        warnings.push(w);
    }
    if alignment.windows_aligned < alignment.windows {
        warnings.push(format!(
            "{} of the {} windows didn't line up (their correlation was under {MIN_CORRELATION:.2}, or they found the delay a \
             pitch period off the windows around them); the alignment there is carried over from the windows around them.",
            alignment.windows - alignment.windows_aligned,
            alignment.windows
        ));
    }

    let margin = samples(MARGIN_S);
    let near_busy = mark(len, &scene.near_activity, margin, margin);
    let near_core = mark(len, &cores(&scene.near_activity, samples(CORE_TRIM_S)), 0, 0);
    let metrics = match scene.test {
        QualityTest::Noise => {
            // A recording's reverberation or trailing speech in the pauses would be kept by the filter, and read as noise left.
            let left = speech_left_in_pauses_db(scene, covered);
            if left > -PAUSE_SPEECH_WARN_DB {
                warnings.push(format!(
                    "The near speech's own sound in the pauses (its reverberation, or its recording's background) is only {:.1} dB \
                     under the noise there, so the noise-in-pauses figure includes some of it.",
                    -left
                ));
            }
            noise_metrics(scene, &aligned, covered)
        }
        QualityTest::Echo => {
            let turn = turn_len();
            let scope = samples(SETTLE_S).max(turn).min(covered)..covered;
            let far_busy = mark(len, &scene.far_activity, margin, margin + samples(RIR_S));
            let far_core = mark(len, &cores(&scene.far_activity, samples(CORE_TRIM_S)), 0, 0);
            let far_alone = |i: usize| turn_at(i) == Turn::FarOnly && far_core[i] && !near_busy[i];
            let double_talk = |i: usize| turn_at(i) == Turn::DoubleTalk && near_core[i];
            // The far-only turn holding a rig slip, and every one after it, are the rig's; so is double talk from there on (the
            // near-only turns have no far voice to misalign)
            let slipped_from = slip_from.map(far_only_from);
            let pooled = scope.start..scope.end.min(slipped_from.unwrap_or(usize::MAX)).max(scope.start);
            let far_only = EchoLevels {
                levels: levels(&scene.input, &aligned, pooled.clone(), far_alone),
                noise_dbfs: rms_dbfs(&scene.noise, pooled.clone(), far_alone),
            };
            let (drop, ceiling) = (far_only.drop_db(), far_only.ceiling_db());
            if drop.is_finite() && ceiling.is_finite() && drop > ceiling - CEILING_WARN_DB {
                warnings.push(format!(
                    "The echo's drop ({drop:.1} dB) is within {CEILING_WARN_DB:.0} dB of the most this scene shows while its noise \
                     passes ({ceiling:.1} dB, the input over the noise there), or past it: near it, the noise caps the figure (a \
                     higher --snr lowers the noise); past it, the noise filter is cutting the noise too."
                ));
            }
            let turn_levels = |slipped: bool| -> Vec<Levels> {
                (3 * turn..covered)
                    .step_by(3 * turn)
                    .filter(|&t0| slipped_from.is_some_and(|s| t0 >= s) == slipped)
                    .map(|t0| levels(&scene.input, &aligned, t0..(t0 + turn).min(covered), far_alone))
                    .filter(|l| l.seconds > 0.0)
                    .collect()
            };
            Metrics::Echo {
                far_only,
                far_only_turns: turn_levels(false),
                slipped_turns: turn_levels(true),
                slipped_from,
                converging: levels(&scene.input, &aligned, samples(SETTLE_S).min(turn)..turn.min(covered), far_alone),
                near_only: speech_levels(&scene.clean, &aligned, scope.clone(), |i| {
                    turn_at(i) == Turn::NearOnly && near_core[i] && !far_busy[i]
                }),
                double_talk: speech_levels(&scene.clean, &aligned, pooled.clone(), |i| double_talk(i) && far_busy[i]),
                both_talking: speech_levels(&scene.clean, &aligned, pooled, |i| double_talk(i) && far_core[i]),
            }
        }
    };
    let slipped = matches!(metrics, Metrics::Echo { slipped_from: Some(_), .. });
    for (name, seconds) in metrics.region_seconds() {
        // After a rig slip the echo and double-talk figures are short by design: the slip's warning says why
        if seconds < MIN_REGION_S && !(slipped && (name.starts_with("echo") || name.ends_with("double talk"))) {
            warnings.push(format!("There was too little {name} ({seconds:.2} s) to measure it well; make the scene longer (--seconds)."));
        }
    }
    Ok(Analysis { alignment, aligned, metrics, delay_steps, warnings })
}

/// The noise test's regions (module notes) over `aligned`, the output in the clean signal's time, as far as `covered`: the
/// noise in pauses (input vs output), the same 200 ms clear of speech, and the speech's share in its cores, from the first
/// second on. [`analyse_with`] measures a live recording with it once aligned; `noise-filter lab` an offline run, aligned exactly.
pub fn noise_metrics(scene: &Scene, aligned: &[f32], covered: usize) -> Metrics {
    let (len, fs) = (scene.clean.len(), f64::from(SAMPLE_RATE));
    let samples = |s: f64| (s * fs).round() as usize;
    let near_busy = mark(len, &scene.near_activity, samples(MARGIN_S), samples(MARGIN_S));
    let near_core = mark(len, &cores(&scene.near_activity, samples(CORE_TRIM_S)), 0, 0);
    let steady = mark(len, &scene.near_activity, samples(STEADY_MARGIN_S), samples(STEADY_MARGIN_S));
    let scope = samples(SETTLE_S).min(covered)..covered;
    Metrics::Noise {
        pauses: levels(&scene.input, aligned, scope.clone(), |i| !near_busy[i]),
        steady_pauses: levels(&scene.input, aligned, scope.clone(), |i| !steady[i]),
        speech: speech_levels(&scene.clean, aligned, scope, |i| near_core[i]),
    }
}

/// The near speech's own level in the noise test's pauses against the noise's there, in dB (its reverberation, or a
/// recording's background): near 0, the noise-in-pauses figure counts some of the speech as noise left.
pub fn speech_left_in_pauses_db(scene: &Scene, covered: usize) -> f32 {
    let fs = f64::from(SAMPLE_RATE);
    let margin = (MARGIN_S * fs).round() as usize;
    let near_busy = mark(scene.clean.len(), &scene.near_activity, margin, margin);
    let scope = ((SETTLE_S * fs).round() as usize).min(covered)..covered;
    let pause = |i: usize| !near_busy[i];
    rms_dbfs(&scene.clean, scope.clone(), pause) - rms_dbfs(&scene.noise, scope, pause)
}

/// One alignment window: its centre in the clean signal, and the delay and the coefficient found there (NaN and 0 when the
/// recording was silent there).
struct Window {
    centre: f64,
    delay: f64,
    correlation: f32,
    confidence_db: f32,
}

/// The alignment (see the module notes) and its delay track: (centre in the clean signal, delay) of each window that lined up.
fn align(scene: &Scene, recording: &[f32], lead: usize) -> Result<(Alignment, Vec<(f64, f64)>), AlignmentFailure> {
    let fail = |alignment: Option<Alignment>, reason: String| Err(AlignmentFailure { alignment, reason });
    if recording.iter().all(|&v| v == 0.0) {
        return fail(None, "The recording is silent: nothing came back through the filter.".into());
    }
    let max_lag = lead + (MAX_PATH_DELAY_S * f64::from(SAMPLE_RATE)) as usize;
    let Some(coarse) = xcorr_peak(&envelope(&scene.clean), &envelope(recording), max_lag.div_ceil(ENVELOPE_FRAME)) else {
        return fail(None, "The recording's level never changes, so it can't hold the scene.".into());
    };
    let coarse = (coarse.delay * ENVELOPE_FRAME as f64).round() as i64;
    let windows = measure_windows(&scene.clean, &scene.near_activity, recording, coarse);
    if windows.is_empty() {
        return fail(None, "The recording is too short to hold the scene.".into());
    }
    let correlations: Vec<f64> = windows.iter().map(|w| f64::from(w.correlation)).collect();
    let good = without_pitch_slips(windows.iter().filter(|w| w.correlation >= MIN_CORRELATION).collect());
    let track: Vec<(f64, f64)> = good.iter().map(|w| (w.centre, w.delay)).collect();
    let delays: Vec<f64> = track.iter().map(|t| t.1).collect();
    let confidences: Vec<f64> = good.iter().map(|w| f64::from(w.confidence_db)).collect();
    let delay_exact = median(&delays).unwrap_or(coarse as f64);
    let a = Alignment {
        delay: delay_exact.round() as i64,
        delay_exact,
        correlation: median(&correlations).unwrap_or(0.0) as f32,
        correlation_low: correlations.iter().copied().fold(f64::INFINITY, f64::min) as f32,
        confidence_db: median(&confidences).map_or(f32::NAN, |c| c as f32),
        windows: windows.len(),
        windows_aligned: good.len(),
        delay_low: delays.iter().copied().fold(f64::INFINITY, f64::min),
        delay_high: delays.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        drift_ppm: drift_ppm(&track),
    };
    if a.correlation < MIN_CORRELATION || track.is_empty() {
        let reason = format!(
            "The recording doesn't hold the scene: its correlation with the speech is {:.2} (the median of {} windows of {WINDOW_S:.0} s), \
             and aligning needs {MIN_CORRELATION:.2}.",
            a.correlation, a.windows
        );
        return fail(Some(a), reason);
    }
    if a.delay < 0 {
        return fail(Some(a), "The recording leads the scene, so it can't be the filter's output.".into());
    }
    Ok((a, track))
}

/// The windows (in time order) less those that lined up a pitch period off: when a window's main peak is smeared (the delay
/// moving within it), the peak one pitch period away (5-10 ms) can win. Such a window lies more than [`MAX_SLIP_MS`] from the
/// median delay of the five windows around it (two either side, or the five at that end of the take); after a real step the
/// windows that follow agree with it, so they stay. If none would be left (two windows that disagree), the best one stays.
fn without_pitch_slips(good: Vec<&Window>) -> Vec<&Window> {
    let max = MAX_SLIP_MS * 1e-3 * f64::from(SAMPLE_RATE);
    let delays: Vec<f64> = good.iter().map(|w| w.delay).collect();
    let around = |k: usize| {
        let from = k.saturating_sub(2).min(good.len().saturating_sub(5));
        &delays[from..(from + 5).min(good.len())]
    };
    let kept: Vec<&Window> =
        (0..good.len()).filter(|&k| median(around(k)).is_some_and(|m| (delays[k] - m).abs() <= max)).map(|k| good[k]).collect();
    if kept.is_empty() { good.into_iter().max_by(|a, b| a.correlation.total_cmp(&b.correlation)).into_iter().collect() } else { kept }
}

/// Each 5 ms frame's RMS, less their mean: a speech envelope that cross-correlates whatever the noise floor.
fn envelope(x: &[f32]) -> Vec<f32> {
    let rms: Vec<f64> = x.chunks(ENVELOPE_FRAME).map(|c| (energy(c) / c.len() as f64).sqrt()).collect();
    let mean = rms.iter().sum::<f64>() / rms.len().max(1) as f64;
    rms.iter().map(|&v| (v - mean) as f32).collect()
}

/// The alignment windows (see the module notes): whole windows (or a last one at least half as long) with enough speech, that
/// lie inside the recording at the coarse delay.
fn measure_windows(clean: &[f32], activity: &[Range<usize>], recording: &[f32], coarse: i64) -> Vec<Window> {
    let fs = f64::from(SAMPLE_RATE);
    let (w, r) = ((WINDOW_S * fs) as usize, (WINDOW_SEARCH_S * fs) as usize);
    let mut out = Vec::new();
    for s in (0..clean.len()).step_by(w) {
        let e = (s + w).min(clean.len());
        let speech: usize = activity.iter().map(|a| a.end.min(e).saturating_sub(a.start.max(s))).sum();
        let (at, end) = (s as i64 + coarse, e as i64 + coarse);
        if e - s < w / 2 || (speech as f64) < MIN_WINDOW_SPEECH_S * fs || at < 0 || end > recording.len() as i64 {
            continue;
        }
        let bs = (at - r as i64).max(0) as usize;
        let (a, b) = (&clean[s..e], &recording[bs..(end as usize + r).min(recording.len())]);
        let centre = (s + e) as f64 / 2.0;
        let Some(p) = xcorr_peak(a, b, 2 * r) else {
            out.push(Window { centre, delay: f64::NAN, correlation: 0.0, confidence_db: f32::NAN });
            continue;
        };
        // xcorr_peak scales by all of b's energy; the coefficient takes only the part of b the window overlaps at the peak.
        let tau = p.delay.round() as i64;
        let (lo, hi) = (tau.clamp(0, b.len() as i64) as usize, (tau + a.len() as i64).clamp(0, b.len() as i64) as usize);
        let overlap = energy(&b[lo..hi.max(lo)]);
        let correlation = if overlap > 0.0 { (f64::from(p.peak) * (energy(b) / overlap).sqrt()).min(1.0) as f32 } else { 0.0 };
        out.push(Window { centre, delay: bs as f64 - s as f64 + p.delay, correlation, confidence_db: p.confidence_db });
    }
    out
}

/// The slope of the straight line through the track's delays, in ppm; NaN with fewer than two points.
fn drift_ppm(track: &[(f64, f64)]) -> f64 {
    let n = track.len() as f64;
    let (mx, my) = (track.iter().map(|t| t.0).sum::<f64>() / n, track.iter().map(|t| t.1).sum::<f64>() / n);
    let sxx: f64 = track.iter().map(|t| (t.0 - mx) * (t.0 - mx)).sum();
    let sxy: f64 = track.iter().map(|t| (t.0 - mx) * (t.1 - my)).sum();
    if track.len() >= 2 && sxx > 0.0 { sxy / sxx * 1e6 } else { f64::NAN }
}

/// The recording read along the delay track (linear between the windows' centres, held beyond them, rounded to a sample), as
/// long as the clean signal; and how much of the clean signal's time the recording reaches.
fn follow(recording: &[f32], track: &[(f64, f64)], len: usize) -> (Vec<f32>, usize) {
    let mut out = Vec::with_capacity(len);
    let (mut covered, mut k) = (len, 0);
    for n in 0..len {
        let t = n as f64;
        while k + 1 < track.len() && track[k + 1].0 <= t {
            k += 1;
        }
        let (x0, d0) = track[k];
        let d = match track.get(k + 1) {
            Some(&(x1, d1)) if t > x0 => d0 + (d1 - d0) * (t - x0) / (x1 - x0),
            _ => d0,
        };
        let i = n as i64 + d.round() as i64;
        if i >= recording.len() as i64 && covered == len {
            covered = n;
        }
        out.push(usize::try_from(i).ok().and_then(|i| recording.get(i)).copied().unwrap_or(0.0));
    }
    (out, covered)
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

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum()
}

/// A per-sample mask: `ranges` widened by `before` and `after`.
fn mark(len: usize, ranges: &[Range<usize>], before: usize, after: usize) -> Vec<bool> {
    let mut m = vec![false; len];
    for r in ranges {
        let (a, b) = (r.start.saturating_sub(before), (r.end + after).min(len));
        if a < b {
            m[a..b].fill(true);
        }
    }
    m
}

/// Each run with `trim` off both ends; runs no longer than twice that are skipped.
fn cores(ranges: &[Range<usize>], trim: usize) -> Vec<Range<usize>> {
    ranges.iter().filter(|r| r.len() > 2 * trim).map(|r| r.start + trim..r.end - trim).collect()
}

/// RMS in dBFS of a sum of squares over `n` samples; NaN when there are none.
fn dbfs(sum: f64, n: usize) -> f32 {
    if n == 0 { f32::NAN } else { (10.0 * (sum / n as f64).log10()) as f32 }
}

/// The RMS levels of `reference` and `output` over the samples of `range` where `mask` holds.
fn levels(reference: &[f32], output: &[f32], range: Range<usize>, mask: impl Fn(usize) -> bool) -> Levels {
    let (mut r, mut o, mut n) = (0.0f64, 0.0f64, 0usize);
    for i in range.start..range.end.min(reference.len()).min(output.len()) {
        if mask(i) {
            let (x, y) = (f64::from(reference[i]), f64::from(output[i]));
            r += x * x;
            o += y * y;
            n += 1;
        }
    }
    Levels { seconds: n as f64 / f64::from(SAMPLE_RATE), reference_dbfs: dbfs(r, n), output_dbfs: dbfs(o, n) }
}

/// The RMS level of `x` over the samples of `range` where `mask` holds.
fn rms_dbfs(x: &[f32], range: Range<usize>, mask: impl Fn(usize) -> bool) -> f32 {
    let r = range.start..range.end.min(x.len());
    let (sum, n) = r.filter(|&i| mask(i)).fold((0.0f64, 0usize), |(s, n), i| (s + f64::from(x[i]) * f64::from(x[i]), n + 1));
    dbfs(sum, n)
}

/// The clean speech and the output over the samples of `range` where `mask` holds, the output split per 20 ms frame into its
/// projection on the clean speech and the rest.
fn speech_levels(clean: &[f32], output: &[f32], range: Range<usize>, mask: impl Fn(usize) -> bool) -> SpeechLevels {
    let end = range.end.min(clean.len()).min(output.len());
    let (mut cc, mut yy, mut kept, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize);
    let mut start = range.start;
    while start < end {
        let stop = (start + PROJECTION_FRAME).min(end);
        let (mut fcc, mut fyc, mut fyy) = (0.0f64, 0.0f64, 0.0f64);
        for i in (start..stop).filter(|&i| mask(i)) {
            let (c, y) = (f64::from(clean[i]), f64::from(output[i]));
            fcc += c * c;
            fyc += y * c;
            fyy += y * y;
            n += 1;
        }
        // The frame's gain is <y,c>/<c,c>, so the speech's share of its energy is <y,c>^2/<c,c> (at most <y,y>).
        if fcc > 0.0 {
            kept += (fyc * fyc / fcc).min(fyy);
        }
        cc += fcc;
        yy += fyy;
        start = stop;
    }
    SpeechLevels {
        seconds: n as f64 / f64::from(SAMPLE_RATE),
        clean_dbfs: dbfs(cc, n),
        output_dbfs: dbfs(yy, n),
        speech_dbfs: dbfs(kept, n),
        rest_dbfs: dbfs((yy - kept).max(0.0), n),
    }
}

#[cfg(test)]
mod tests {
    use nf_stages::synth::{Voice, pink_noise, speech_like};

    use super::*;
    use crate::quality::echo_delay::{DEFAULT_ECHO_DELAY_MS, MAX_ECHO_DELAY_MS};
    use crate::quality::signals::{DEFAULT_NOISE_SNR_DB, SPEECH_DBFS, UserAudio, build};

    const FS: usize = 48_000;
    const LEAD: usize = FS;

    /// The recording a perfect path would give: `lead` + `path` samples of silence, `processed`, then 2 s of silence.
    fn recording(processed: &[f32], path: usize) -> Vec<f32> {
        let mut r = vec![0.0; LEAD + path];
        r.extend_from_slice(processed);
        r.extend(std::iter::repeat_n(0.0, 2 * FS));
        r
    }

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    /// A speech region's change: its speech share over the clean speech.
    fn gain(s: &SpeechLevels) -> f32 {
        s.speech_dbfs - s.clean_dbfs
    }

    /// A "filter" that cuts the noise by 20 dB in the pauses and by 30 dB near speech, keeps the speech, and scales it all by
    /// -6 dB. The delay comes out to the sample and each level change to within 0.01 dB.
    #[test]
    fn the_noise_test_finds_a_known_delay_gain_and_noise_cut() {
        let scene = build(QualityTest::Noise, 12.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let len = scene.clean.len();
        let g = 0.5f32;
        let g_db = 20.0 * g.log10();
        // 20 ms around speech: inside the analysis's 50 ms margin, so every pause it measures has the 20 dB cut.
        let near = mark(len, &scene.near_activity, 960, 960);
        let processed: Vec<f32> = (0..len).map(|i| g * (scene.clean[i] + scene.noise[i] * if near[i] { 0.0316 } else { 0.1 })).collect();
        let path = 6_173;
        let a = analyse(&scene, &recording(&processed, path), LEAD).unwrap();
        let al = a.alignment;
        assert_eq!(al.delay, (LEAD + path) as i64);
        assert!((al.delay_exact - (LEAD + path) as f64).abs() < 0.05, "{al:?}");
        assert!((al.path_ms(LEAD) - path as f64 / 48.0).abs() < 0.01);
        assert!(al.correlation > 0.95 && al.correlation_low > 0.95, "{al:?}");
        // Every 2 s window has speech enough; they all line up, at the same delay
        assert_eq!((al.windows, al.windows_aligned), (6, 6), "{al:?}");
        assert!(al.delay_spread_ms() < 0.01 && al.drift_ppm.abs() < 1.0, "{al:?}");
        assert_eq!(a.aligned, processed);
        assert!(a.warnings.is_empty(), "{:?}", a.warnings);
        let Metrics::Noise { pauses, steady_pauses, speech } = a.metrics else { panic!("{:?}", a.metrics) };
        assert!(close(pauses.reference_dbfs, SPEECH_DBFS - DEFAULT_NOISE_SNR_DB, 0.5), "{pauses:?}");
        assert!(close(pauses.change_db(), g_db - 20.0, 0.01), "{pauses:?}");
        assert!(close(steady_pauses.change_db(), g_db - 20.0, 0.01), "{steady_pauses:?}");
        assert!(steady_pauses.seconds > 0.5 && steady_pauses.seconds < pauses.seconds, "{steady_pauses:?} {pauses:?}");
        assert!(close(gain(&speech), g_db, 0.01), "{speech:?}");
        // The speech's share is nearly all the output; the rest is the noise left near it, 30 dB down and more
        assert!(close(speech.output_dbfs, speech.speech_dbfs, 0.01) && speech.rest_dbfs < speech.speech_dbfs - 30.0, "{speech:?}");
        assert!(pauses.seconds > 1.5 && speech.seconds > 2.0, "{pauses:?} {speech:?}");
        // The cores are louder than the active level (the edges are left out); the pauses stay clear of the speech.
        assert!(speech.clean_dbfs > SPEECH_DBFS, "{speech:?}");
    }

    /// A 21 s echo scene: far-only turns at 0, 9 and 18 s, double talk at 3 and 12 s, near only at 6 and 15 s.
    fn echo_scene() -> Scene {
        build(QualityTest::Echo, 21.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap()
    }

    /// An "echo canceller" that scales the echo and the noise at sample `i` by `cut(i, near)` (`near`: within 20 ms of the near
    /// speech, so inside the analysis's 50 ms margin), keeps the near speech, and scales it all by `g`.
    fn cancel(scene: &Scene, g: f32, cut: impl Fn(usize, bool) -> f32) -> Vec<f32> {
        let echo = scene.echo.as_ref().unwrap();
        let near = mark(scene.clean.len(), &scene.near_activity, 960, 960);
        (0..scene.clean.len()).map(|i| g * (scene.clean[i] + (echo[i] + scene.noise[i]) * cut(i, near[i]))).collect()
    }

    /// 20 dB off the echo where the near end is silent (40 dB near its speech): ERLE 20 dB plus the gain in every far-only
    /// turn, and the near speech's change is the gain alone, alone, in double talk and with both talkers mid-syllable.
    #[test]
    fn the_echo_test_finds_the_erle_and_keeps_the_near_speech() {
        let scene = echo_scene();
        let g = 0.8f32;
        let g_db = 20.0 * g.log10();
        let path = 9_000;
        let a = analyse(&scene, &recording(&cancel(&scene, g, |_, near| if near { 0.01 } else { 0.1 }), path), LEAD).unwrap();
        assert_eq!(a.alignment.delay, (LEAD + path) as i64);
        assert!(a.alignment.correlation > 0.95, "{:?}", a.alignment);
        assert!(a.warnings.is_empty(), "{:?}", a.warnings);
        let Metrics::Echo { far_only, far_only_turns, converging, near_only, double_talk, both_talking, .. } = a.metrics else {
            panic!("{:?}", a.metrics)
        };
        // The input where only the far end talks is the echo and a little noise: 6 dB under the speech's active level, and a
        // little louder in the far voice's cores than over all of its activity.
        let echo_db = SPEECH_DBFS - 6.0;
        let input = far_only.levels.reference_dbfs;
        assert!(input > echo_db - 0.5 && input < echo_db + 3.0, "{far_only:?}");
        assert!(close(far_only.drop_db(), 20.0 - g_db, 0.01), "ERLE: {far_only:?}");
        // The scene's noise is 60 dB under the speech: far below the echo, so it caps nothing here
        assert!(far_only.ceiling_db() > 50.0, "{far_only:?}");
        // The two far-only turns after the first, each on its own, and the first from 1 s on
        assert_eq!(far_only_turns.len(), 2);
        for t in far_only_turns.iter().chain([&converging]) {
            assert!(close(-t.change_db(), 20.0 - g_db, 0.01) && t.seconds > 0.3, "{t:?}");
        }
        assert!(converging.seconds < 2.0, "{converging:?}");
        for (l, at_least) in [(near_only, 0.4), (double_talk, 0.4), (both_talking, 0.05)] {
            assert!(close(gain(&l), g_db, 0.01) && l.seconds > at_least, "{l:?}");
        }
        assert!(both_talking.seconds < double_talk.seconds);

        // Without the canceller (the input passed as it is) the output is the same as the input where only the far end talks,
        // and in double talk the near speech's own share is unchanged: the echo there is the rest, about 6 dB under it.
        let a = analyse(&scene, &recording(&scene.input, path), LEAD).unwrap();
        let Metrics::Echo { far_only, double_talk, .. } = a.metrics else { panic!() };
        assert!(close(far_only.drop_db(), 0.0, 1e-4), "{far_only:?}");
        assert!(close(gain(&double_talk), 0.0, 0.3), "{double_talk:?}");
        assert!(double_talk.output_dbfs > double_talk.clean_dbfs, "the echo adds to the output: {double_talk:?}");
        assert!(double_talk.rest_dbfs > double_talk.clean_dbfs - 12.0, "{double_talk:?}");
    }

    /// The echo's regions are where the echo is in the mic: the far voice's activity in the scene's time, widened by the
    /// margins and the room's tail. The echo delay moves only when the far end plays the far voice, not the echo, so the
    /// regions hold the echo whatever the delay: the scene is the same at 0 and 300 ms, all but a trace of the echo's energy is
    /// inside the far end's busy region, and the same recording measures the same.
    #[test]
    fn the_echo_regions_hold_the_echo_whatever_the_delay() {
        let (soon, late) = (
            build(QualityTest::Echo, 21.0, None, 0.0, &UserAudio::default()).unwrap(),
            build(QualityTest::Echo, 21.0, None, MAX_ECHO_DELAY_MS, &UserAudio::default()).unwrap(),
        );
        assert_eq!((&soon.input, &soon.clean, &soon.far), (&late.input, &late.clean, &late.far));
        assert_eq!((&soon.far_activity, &soon.near_activity), (&late.far_activity, &late.near_activity));
        let echo = soon.echo.as_deref().unwrap();
        let fs = f64::from(SAMPLE_RATE);
        let busy = mark(echo.len(), &soon.far_activity, (MARGIN_S * fs) as usize, ((MARGIN_S + RIR_S) * fs) as usize);
        let (inside, outside) = echo.iter().zip(&busy).fold((0.0f64, 0.0f64), |(i, o), (&v, &b)| {
            let e = f64::from(v) * f64::from(v);
            if b { (i + e, o) } else { (i, o + e) }
        });
        assert!(10.0 * (outside / inside).log10() < -40.0, "the echo outside the far end's busy region: {outside} of {inside}");
        let processed = cancel(&soon, 1.0, |_, _| 0.1);
        let (a, b) =
            (analyse(&soon, &recording(&processed, 4_000), LEAD).unwrap(), analyse(&late, &recording(&processed, 4_000), LEAD).unwrap());
        assert_eq!(a.metrics, b.metrics);
    }

    /// A canceller that is still converging in the first turn, and does worse in double talk, leaves the far-only figure
    /// where it was: it counts only far-only turns after the first. The first turn is reported on its own.
    #[test]
    fn the_far_only_figure_leaves_out_the_first_turn_and_double_talk() {
        let scene = echo_scene();
        let path = 3_000;
        let steady = analyse(&scene, &recording(&cancel(&scene, 1.0, |_, _| 0.01), path), LEAD).unwrap();
        let worse = |i: usize, _| match (i / turn_len(), turn_at(i)) {
            (0, _) => 0.3,
            (_, Turn::DoubleTalk) => 0.5,
            _ => 0.01,
        };
        let rough = analyse(&scene, &recording(&cancel(&scene, 1.0, worse), path), LEAD).unwrap();
        let (
            Metrics::Echo { far_only: a, double_talk: clean_dt, .. },
            Metrics::Echo { far_only: b, converging, far_only_turns, double_talk, .. },
        ) = (&steady.metrics, &rough.metrics)
        else {
            panic!()
        };
        assert!(close(a.drop_db(), 40.0, 0.01) && close(b.drop_db(), a.drop_db(), 1e-4), "{a:?} {b:?}");
        assert!(far_only_turns.iter().all(|t| close(-t.change_db(), 40.0, 0.01)), "{far_only_turns:?}");
        assert!(close(-converging.change_db(), -20.0 * 0.3f32.log10(), 0.01), "{converging:?}");
        // The echo left in double talk is the rest there, not the near speech
        assert!(close(gain(double_talk), 0.0, 0.3), "{double_talk:?}");
        assert!(double_talk.rest_dbfs > clean_dt.rest_dbfs + 10.0, "{double_talk:?} {clean_dt:?}");
    }

    /// A rig slip 10 s in (inside the second far-only turn, 9-12 s): a canceller that cut the echo by 40 dB before it and by
    /// only 3 dB from then on. The pooled echo figure is the turns before the slip's (40 dB), and the turns from its turn on
    /// (9 and 18 s) are listed apart with what they did; double talk counts only its turn before the slip (6-9 s, not 15-18 s);
    /// without the slip, the pooled figure falls and double talk holds the echo left after it.
    #[test]
    fn the_far_only_turns_after_a_rig_slip_are_left_out_of_the_echo_figure() {
        let scene = build(QualityTest::Echo, 21.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let slip = 10 * FS;
        let processed = cancel(&scene, 1.0, |i, _| if i < slip { 0.01 } else { 0.7 });
        let rec = recording(&processed, 2_000);
        let a = analyse_with(&scene, &rec, LEAD, Some(slip)).unwrap();
        let Metrics::Echo { far_only, far_only_turns, slipped_turns, slipped_from, .. } = &a.metrics else { panic!() };
        assert_eq!(*slipped_from, Some(9 * FS), "from the start of the turn holding the slip");
        assert!(far_only_turns.is_empty(), "no far-only turn after the first comes before the slip: {far_only_turns:?}");
        assert_eq!(slipped_turns.len(), 2);
        // The turn holding the slip (9-12 s) mixes both; the one after it (18-21 s) is the 3 dB alone
        assert!(
            -slipped_turns[0].change_db() < 39.0 && close(-slipped_turns[1].change_db(), -20.0 * 0.7f32.log10(), 0.01),
            "{slipped_turns:?}"
        );
        assert!(far_only.levels.seconds == 0.0 && far_only.drop_db().is_nan(), "{far_only:?}");
        assert!(!a.warnings.iter().any(|w| w.contains("too little echo")), "the slip explains it: {:?}", a.warnings);
        let unflagged = analyse(&scene, &rec, LEAD).unwrap();
        let (Metrics::Echo { double_talk, both_talking, .. }, Metrics::Echo { double_talk: all_dt, both_talking: all_both, .. }) =
            (&a.metrics, &unflagged.metrics)
        else {
            panic!()
        };
        assert!(double_talk.seconds > 0.25 && double_talk.seconds < 0.75 * all_dt.seconds, "{double_talk:?} {all_dt:?}");
        assert!(both_talking.seconds < all_both.seconds, "{both_talking:?} {all_both:?}");
        assert!(double_talk.rest_dbfs < all_dt.rest_dbfs - 10.0, "only the 40 dB part's echo: {double_talk:?} {all_dt:?}");
        // A slip in the third far-only turn (18-21 s): the second is pooled, at its 40 dB
        let late =
            analyse_with(&scene, &recording(&cancel(&scene, 1.0, |i, _| if i < 19 * FS { 0.01 } else { 0.7 }), 2_000), LEAD, Some(19 * FS))
                .unwrap();
        let Metrics::Echo { far_only, far_only_turns, slipped_turns, .. } = &late.metrics else { panic!() };
        assert!(close(far_only.drop_db(), 40.0, 0.01) && far_only_turns.len() == 1 && slipped_turns.len() == 1, "{far_only:?}");
        // Unflagged, the same recording pools the slipped turn in and the figure falls
        let plain = analyse(&scene, &recording(&cancel(&scene, 1.0, |i, _| if i < 19 * FS { 0.01 } else { 0.7 }), 2_000), LEAD).unwrap();
        let Metrics::Echo { far_only, slipped_turns, .. } = &plain.metrics else { panic!() };
        assert!(far_only.drop_db() < 10.0 && slipped_turns.is_empty(), "{far_only:?}");
    }

    /// A canceller that removes the echo exactly and passes the noise, in a scene whose noise is only 30 dB under the
    /// speech: the drop is the scene's ceiling, and that's said.
    #[test]
    fn an_erle_at_the_scenes_noise_ceiling_is_flagged() {
        let scene = build(QualityTest::Echo, 12.0, Some(30.0), DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let processed: Vec<f32> = scene.clean.iter().zip(&scene.noise).map(|(c, n)| c + n).collect();
        let a = analyse(&scene, &recording(&processed, 2_000), LEAD).unwrap();
        let Metrics::Echo { far_only, .. } = a.metrics else { panic!() };
        assert!(close(far_only.drop_db(), far_only.ceiling_db(), 1e-3), "{far_only:?}");
        assert!(far_only.ceiling_db() > 15.0 && far_only.ceiling_db() < 30.0, "{far_only:?}");
        assert!(a.warnings.iter().any(|w| w.contains("of the most this scene shows")), "{:?}", a.warnings);
    }

    /// `x` with one sample repeated every `every` samples: a path whose delay grows by 1e6 / `every` ppm in sample slips.
    fn stretched(x: &[f32], every: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(x.len() + x.len() / every + 1);
        for (i, &v) in x.iter().enumerate() {
            out.push(v);
            if i % every == every - 1 {
                out.push(v);
            }
        }
        out
    }

    /// A delay that drifts (50 ppm in slips) or steps (3 ms, mid-take) is followed window by window: the scene stays aligned,
    /// the drift is measured, a move over 2 ms is warned about, and the levels barely move.
    #[test]
    fn a_drifting_or_stepping_delay_is_followed() {
        let scene = build(QualityTest::Noise, 24.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        let processed: Vec<f32> = scene.clean.iter().zip(&scene.noise).map(|(c, n)| 0.5 * (c + 0.1 * n)).collect();
        let still = analyse(&scene, &recording(&processed, 2_500), LEAD).unwrap();
        let Metrics::Noise { pauses: p0, speech: s0, .. } = still.metrics else { panic!() };

        let drifting = analyse(&scene, &recording(&stretched(&processed, 20_000), 2_500), LEAD).unwrap();
        let al = drifting.alignment;
        assert!(al.correlation > 0.9 && al.windows_aligned == al.windows, "{al:?}");
        assert!((al.drift_ppm - 50.0).abs() < 2.0, "{al:?}");
        // 50 ppm over the 22 s between the first and last windows' centres: about 1.1 ms, under the warning
        assert!((al.delay_spread_ms() - 1.0).abs() < 0.25, "{al:?}");
        assert!(drifting.warnings.is_empty(), "{:?}", drifting.warnings);
        let Metrics::Noise { pauses, speech, .. } = drifting.metrics else { panic!() };
        assert!(close(pauses.change_db(), p0.change_db(), 0.05) && close(gain(&speech), gain(&s0), 0.1), "{speech:?} {s0:?}");

        // 144 samples (3 ms) of silence inserted 12 s in
        let mut stepped = processed[..12 * FS].to_vec();
        stepped.extend(std::iter::repeat_n(0.0, 144));
        stepped.extend_from_slice(&processed[12 * FS..]);
        let a = analyse(&scene, &recording(&stepped, 2_500), LEAD).unwrap();
        assert!((a.alignment.delay_spread_ms() - 3.0).abs() < 0.01, "{:?}", a.alignment);
        assert!(a.warnings.iter().any(|w| w.contains("moved by 3.0 ms")), "{:?}", a.warnings);
        // Exact away from the step: up to the centre of the window before it, and from the centre of the window after it
        assert_eq!(a.aligned[..11 * FS], processed[..11 * FS]);
        assert_eq!(a.aligned[13 * FS..], processed[13 * FS..]);
        // The step is found, at the silence that made it.
        assert_eq!(a.delay_steps.len(), 1, "{:?}", a.delay_steps);
        let step = a.delay_steps[0];
        assert!((step.ms - 3.0).abs() < 0.01 && (step.at_s - 12.0).abs() < 0.01, "{step:?}");
        let (at, ms) = step.silence.unwrap();
        assert!((at - (LEAD + 2_500 + 12 * FS) as f64 / 48_000.0).abs() < 1e-6 && (ms - 3.0).abs() < 1e-9, "{step:?}");

        // The output cable's (TODO 9): 581 samples (12.1 ms) of silence inside a window, 12.3 s in, as the dev VM's quality
        // takes had them. The warning says where, and that the silence explains it.
        let mut holed = processed[..12 * FS + 14_400].to_vec();
        holed.extend(std::iter::repeat_n(0.0, 581));
        holed.extend_from_slice(&processed[12 * FS + 14_400..]);
        let a = analyse(&scene, &recording(&holed, 2_500), LEAD).unwrap();
        assert!((a.alignment.delay_spread_ms() - 12.1).abs() < 0.05, "{:?}", a.alignment);
        let step = *a.delay_steps.first().unwrap();
        assert!((step.ms - 12.1).abs() < 0.05 && (step.at_s - 12.3).abs() < 0.01, "{:?}", a.delay_steps);
        assert!((step.silence.unwrap().1 - 12.1).abs() < 0.05, "{step:?}");
        let w = a.warnings.iter().find(|w| w.contains("moved by 12.1 ms")).unwrap();
        assert!(
            w.contains("It stepped by +12.1 ms about 12.3 s into the scene, where the recording holds 12.1 ms of digital silence"),
            "{w}"
        );
        assert!(w.contains("samples inserted after the filter, by the output cable or the recording"), "{w}");
    }

    /// A saved take's delay steps (TODO 9), rerun from its files: `NF_QUALITY_TAKE=DIR` (a `quality --save` folder of the
    /// synthetic scene, whose clean speech `build` makes again) and `NF_QUALITY_TEST=echo|noise` (echo by default); the scene's
    /// length is input.wav's. Ignored (it reads files): `NF_QUALITY_TAKE=DIR cargo test -p nf-app --release a_saved_takes_delay
    /// -- --ignored --nocapture`. On the dev VM's takes of 2026-11-01 it finds one step in each of echo-7, echo-8, echo-9 and
    /// d25-base (+10.6, +9.9, +11.1 and +12.9 ms), each at a run of zeros in the recording as long within 0.8 ms (10.6, 9.2,
    /// 11.1 and 12.1 ms), and none in echo-6 and echo-10 (their delay held within 2.1 ms).
    #[test]
    #[ignore]
    fn a_saved_takes_delay_steps() {
        let Ok(dir) = std::env::var("NF_QUALITY_TAKE") else { return };
        let dir = std::path::Path::new(&dir);
        let read = |name: &str| crate::wav::read_mono_48k(&dir.join(name)).unwrap().samples;
        let (input, recording) = (read("input.wav"), read("recording.wav"));
        let test = if std::env::var("NF_QUALITY_TEST").is_ok_and(|t| t == "noise") { QualityTest::Noise } else { QualityTest::Echo };
        let seconds = input.len() as f32 / FS as f32;
        let scene = build(test, seconds, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        assert_eq!(scene.clean, read("clean.wav"), "the take's scene: its test (NF_QUALITY_TEST) and length");
        let a = analyse(&scene, &recording, LEAD).unwrap();
        let al = &a.alignment;
        println!(
            "{}: {} of {} windows, delay {:.2} to {:.2} ms past the lead",
            dir.display(),
            al.windows_aligned,
            al.windows,
            (al.delay_low - LEAD as f64) / 48.0,
            (al.delay_high - LEAD as f64) / 48.0
        );
        for s in &a.delay_steps {
            println!("  {}", step_text(s));
        }
    }

    /// The steps of a delay track over 2 ms, each with the silence between its windows that's about as long; a drop, or a
    /// step no silence of its length explains, has none.
    #[test]
    fn delay_steps_find_the_silence_that_explains_them() {
        let mut rec = vec![0.25f32; 400_000];
        // 576 zeros (12 ms) between the second and third windows, and a 60-sample run (1.25 ms) there too, which doesn't fit.
        rec[190_000..190_576].fill(0.0);
        rec[200_000..200_060].fill(0.0);
        let track = [(48_000.0, 9_000.0), (144_000.0, 9_010.0), (240_000.0, 9_586.0), (336_000.0, 9_100.0)];
        let steps = delay_steps(&track, &rec);
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert!((steps[0].ms - 12.0).abs() < 1e-9 && steps[0].silence == Some((190_000.0 / 48_000.0, 12.0)), "{:?}", steps[0]);
        assert!((steps[0].at_s - (190_000.0 - 9_010.0) / 48_000.0).abs() < 1e-9);
        // The drop back has no silence to show, and is placed between its windows.
        assert!(steps[1].ms < -10.0 && steps[1].silence.is_none() && (steps[1].at_s - 6.0).abs() < 1e-9, "{:?}", steps[1]);
        // Without the 12 ms run, the 1.25 ms one doesn't explain a 12 ms step.
        rec[190_000..190_576].fill(0.25);
        assert_eq!(delay_steps(&track, &rec)[0].silence, None);
        assert!(
            step_text(&delay_steps(&track, &rec)[0])
                .ends_with("no silence in the recording explains it (compare the filter's output events below).")
        );
        // Steps under 2 ms aren't steps.
        assert!(delay_steps(&[(0.0, 100.0), (96_000.0, 190.0)], &rec).is_empty());
    }

    #[test]
    fn a_recording_without_the_scene_doesnt_align() {
        let scene = build(QualityTest::Noise, 6.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        // Silence
        let e = analyse(&scene, &vec![0.0; 9 * FS], LEAD).unwrap_err();
        assert_eq!(e.alignment, None);
        assert!(e.reason.contains("silent"), "{}", e.reason);
        // Another talker, and noise alone
        for other in [speech_like(6 * FS, Voice::HIGH, 77).samples, pink_noise(6 * FS, 0.03, 78)] {
            let e = analyse(&scene, &recording(&other, 5_000), LEAD).unwrap_err();
            let weak = e.alignment.expect("windows, but weak ones");
            assert!(weak.correlation < 0.2 && weak.windows >= 1, "{weak:?}");
            assert!(e.reason.contains("doesn't hold the scene"), "{}", e.reason);
        }
        // The scene itself, but further away than the window: its own speech at the wrong delay
        let far = recording(&scene.input, (MAX_PATH_DELAY_S * FS as f64) as usize + 2 * FS);
        let e = analyse(&scene, &far, LEAD).unwrap_err();
        assert!(e.alignment.unwrap().correlation < MIN_CORRELATION, "{:?}", e.alignment);
    }

    #[test]
    fn a_recording_that_leads_the_scene_fails_and_a_short_one_is_measured_as_far_as_it_goes() {
        let scene = build(QualityTest::Noise, 8.0, None, DEFAULT_ECHO_DELAY_MS, &UserAudio::default()).unwrap();
        // The scene starts 4000 samples before the recording does
        let e = analyse(&scene, &scene.input[4_000..], LEAD).unwrap_err();
        assert_eq!(e.alignment.map(|a| a.delay), Some(-4_000));
        assert!(e.reason.contains("leads"), "{}", e.reason);
        // Before the lead is over: aligned, with a warning
        let early = analyse(&scene, &recording(&scene.input, 0)[480..], LEAD).unwrap();
        assert_eq!(early.alignment.delay, (LEAD - 480) as i64);
        assert!(early.warnings.iter().any(|w| w.contains("before it was played")), "{:?}", early.warnings);
        // A recording that stops 2 s into the scene's end
        let mut short = recording(&scene.input, 100);
        short.truncate(LEAD + 100 + 6 * FS);
        let a = analyse(&scene, &short, LEAD).unwrap();
        assert!(a.warnings.iter().any(|w| w.contains("ends 2.00 s before")), "{:?}", a.warnings);
        assert!(a.aligned[6 * FS..].iter().all(|&v| v == 0.0));
        let Metrics::Noise { pauses, .. } = a.metrics else { panic!() };
        assert!(close(pauses.change_db(), 0.0, 1e-4) && pauses.seconds < 5.0, "{pauses:?}");
        // Nothing of the scene fits at all
        let e = analyse(&scene, &recording(&scene.input, 100)[..LEAD + FS], LEAD).unwrap_err();
        assert!(e.reason.contains("too short"), "{}", e.reason);
    }

    #[test]
    fn masks_cores_and_levels() {
        assert_eq!(mark(10, &[2..4, 8..9], 1, 2), [false, true, true, true, true, true, false, true, true, true]);
        assert_eq!(cores(&[0..10, 20..24, 30..35], 2), [2..8, 32..33]);
        let l = levels(&[1.0, 0.5, 0.5], &[0.1, 0.05, 1.0], 0..3, |i| i < 2);
        assert!((l.seconds - 2.0 / 48_000.0).abs() < 1e-12);
        assert!(close(l.change_db(), -20.0, 1e-4), "{l:?}");
        assert_eq!(levels(&[1.0, 0.5, 0.5], &[0.1, 0.05, 1.0], 1..9, |_| true).seconds, 2.0 / 48_000.0, "the range, cut to the signal");
        let none = levels(&[1.0], &[1.0], 0..1, |_| false);
        assert!(none.reference_dbfs.is_nan() && none.change_db().is_nan() && none.seconds == 0.0);
        let silent = levels(&[1.0], &[0.0], 0..1, |_| true);
        assert_eq!(silent.output_dbfs, f32::NEG_INFINITY);
        assert!(close(rms_dbfs(&[0.1, 1.0, 0.1], 0..3, |i| i != 1), -20.0, 1e-4));
        assert_eq!(median(&[3.0, f64::NAN, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0]), Some(2.5));
        assert_eq!(median(&[f64::NAN]), None);
    }

    /// The speech's share is the output's projection on the clean speech: a gain and unrelated noise come apart.
    #[test]
    fn speech_levels_split_the_output_into_the_speech_and_the_rest() {
        let c = speech_like(2 * FS, Voice::LOW, 5).samples;
        let n = pink_noise(2 * FS, 0.01, 6);
        let y: Vec<f32> = c.iter().zip(&n).map(|(c, n)| 0.5 * c + n).collect();
        let s = speech_levels(&c, &y, 0..2 * FS, |i| c[i] != 0.0);
        assert!(close(gain(&s), 20.0 * 0.5f32.log10(), 0.05), "{s:?}");
        let noise_there = rms_dbfs(&n, 0..2 * FS, |i| c[i] != 0.0);
        assert!(close(s.rest_dbfs, noise_there, 0.2), "{s:?}, the noise at {noise_there}");
        // Inverted, the share is the same; with nothing of the speech in it, there is none
        let inverted: Vec<f32> = c.iter().map(|v| -v).collect();
        assert!(close(gain(&speech_levels(&c, &inverted, 0..2 * FS, |_| true)), 0.0, 1e-4));
        let s = speech_levels(&c, &vec![0.0; 2 * FS], 0..2 * FS, |_| true);
        assert!(s.speech_dbfs == f32::NEG_INFINITY && s.rest_dbfs == f32::NEG_INFINITY, "{s:?}");
        assert!(gain(&speech_levels(&c, &y, 0..2 * FS, |_| false)).is_nan());
    }

    /// A window a pitch period off its neighbours is left out; a step the windows after it agree with isn't.
    #[test]
    fn windows_a_pitch_period_off_are_left_out() {
        let windows = |delays: &[(f64, f32)]| -> Vec<Window> {
            let at =
                |(k, &(delay, correlation)): (usize, &(f64, f32))| Window { centre: k as f64, delay, correlation, confidence_db: 20.0 };
            delays.iter().enumerate().map(at).collect()
        };
        let kept = |w: &[Window]| -> Vec<f64> { without_pitch_slips(w.iter().collect()).iter().map(|w| w.delay).collect() };
        // 400 samples (8.3 ms) off, alone or two together
        assert_eq!(kept(&windows(&[(100.0, 0.9), (100.5, 0.9), (500.0, 0.6), (101.0, 0.9), (100.0, 0.9)])), [100.0, 100.5, 101.0, 100.0]);
        assert_eq!(kept(&windows(&[(100.0, 0.9), (100.0, 0.9), (500.0, 0.6), (500.0, 0.6), (100.0, 0.9), (100.0, 0.9)])), [100.0; 4]);
        // A 10 ms step mid-take, and a slow drift: all kept
        let step = windows(&[(100.0, 0.9), (100.0, 0.9), (100.0, 0.9), (580.0, 0.9), (580.0, 0.9), (580.0, 0.9)]);
        assert_eq!(kept(&step).len(), 6);
        let drift = windows(&[(100.0, 0.9), (150.0, 0.9), (200.0, 0.9), (250.0, 0.9), (300.0, 0.9)]);
        assert_eq!(kept(&drift).len(), 5);
        // Two that disagree: the better one
        assert_eq!(kept(&windows(&[(100.0, 0.5), (500.0, 0.8)])), [500.0]);
        assert!(kept(&[]).is_empty());
    }

    #[test]
    fn the_track_is_followed_and_its_drift_measured() {
        let rec: Vec<f32> = (0..40).map(|i| i as f32).collect();
        // Held before the first centre and after the last; between them, linear and rounded
        let (out, covered) = follow(&rec, &[(2.0, 5.0), (6.0, 7.0)], 10);
        assert_eq!(out, [5.0, 6.0, 7.0, 9.0, 10.0, 12.0, 13.0, 14.0, 15.0, 16.0]);
        assert_eq!(covered, 10);
        let (out, covered) = follow(&rec, &[(0.0, 35.0)], 10);
        assert_eq!((out[4], out[5], covered), (39.0, 0.0, 5));
        assert!((drift_ppm(&[(0.0, 10.0), (1e6, 11.0), (2e6, 12.0)]) - 1.0).abs() < 1e-9);
        assert!(drift_ppm(&[(5.0, 10.0)]).is_nan());
    }
}
