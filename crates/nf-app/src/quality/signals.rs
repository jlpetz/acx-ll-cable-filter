//! The quality test's scene: what is played as the mic, the clean near speech the recording is measured against, and, for the
//! echo test, the far voice played on the far-end device.
//!
//! - **Noise test:** near speech (the synthetic low voice, or `--speech`) at -20 dBFS active level, plus pink noise (or
//!   `--noise`) `--snr` below that level (10 dB by default).
//! - **Echo test:** 3 s turns of far only, double talk and near only, over and over. The far end talks first, so the echo
//!   canceller has a far-only turn to converge in. The near speech talks in the double-talk and near-only turns, the far voice
//!   (the synthetic high voice, or `--far-speech`) in the far-only and double-talk turns. The mic gets the near speech, the far
//!   voice through a room impulse response (RT60 0.15 s, 150 ms long) at 6 dB below the near speech's active level, and pink
//!   noise 60 dB below it (or `--snr`): low, so the noise doesn't cap the ERLE the analysis can see. The far voice itself, at
//!   -20 dBFS active level, goes to the far-end device: the engine's echo reference. The echo is mixed into the mic in step with
//!   [`Scene::far`] (its regions are where the far voice is active, in the mic's time); when the far end plays it is the take's
//!   business: the command places it so the echo reaches the mic `--echo-delay` after the far end's loopback taps the far
//!   voice (25 ms by default, [`super::echo_delay`]), as a speaker's output latency and the air would.
//! - **Recordings** (`--speech`, `--noise`, `--far-speech`) replace the synthetic speech, noise or far voice. They must be
//!   48 kHz (mono, or downmixed); they're looped or trimmed to the scene's length, and speech is set to the same active level.
//!   Their speech activity comes from a frame-energy detector ([`detect_activity`]) plus a 200 ms hangover (ITU-T P.56's), so
//!   a real recording's reverberation and trailing low-level sounds count as speech rather than pause; a synthetic voice's
//!   activity is its syllables. In the echo test a recording is gated to its turns with 10 ms fades, and a synthetic voice keeps
//!   only the syllables that lie wholly inside its turns, so neither is cut mid-syllable without the activity knowing.
//! - If the mic signal would peak above 0.99, the scene's mic parts are all scaled down together (the description says so).

use std::ops::Range;
use std::path::Path;

use nf_core::SAMPLE_RATE;
use nf_stages::synth::{Voice, add, convolve, db, mix_at_snr, pink_noise, rms_over, room_impulse_response, speech_like};

use super::QualityTest;
use crate::exit::Failure;
use crate::wav;

/// The active level of the near speech, and of the far voice as played, in dBFS.
pub const SPEECH_DBFS: f32 = -20.0;
/// The echo's active level below the near speech's.
pub const ECHO_BELOW_DB: f32 = 6.0;
/// The noise below the near speech's active level by default: the noise test's, and the echo test's low noise (at 30 dB the
/// noise sat about 25 dB under the echo, which capped the ERLE the analysis could see).
pub const DEFAULT_NOISE_SNR_DB: f32 = 10.0;
pub const DEFAULT_ECHO_SNR_DB: f32 = 60.0;
/// The echo test's turns (far only, double talk, near only) are each this long.
pub const TURN_S: f64 = 3.0;
/// The room impulse response's length: the echo outlasts the far voice by this much.
pub const RIR_S: f64 = 0.15;
const RIR_RT60_S: f32 = 0.15;
/// The fades where a recording is gated to its turns.
const GATE_FADE_S: f64 = 0.01;
/// The mic signal's peak is kept under this.
const MAX_PEAK: f32 = 0.99;
const SEED_NEAR: u64 = 21;
const SEED_FAR: u64 = 22;
const SEED_NOISE: u64 = 23;
const SEED_RIR: u64 = 24;
/// The activity detector: 10 ms frames, active within 15.9 dB of the active level (ITU-T P.56's margin), gaps up to 30 ms
/// bridged, runs under 20 ms dropped.
const FRAME: usize = 480;
const ACTIVE_BELOW_DB: f64 = 15.9;
const BRIDGE_FRAMES: usize = 3;
const MIN_RUN_FRAMES: usize = 2;
/// The hangover added after each run the detector finds in a recording (P.56's).
const HANGOVER_S: f64 = 0.2;

/// The user's recordings, read as 48 kHz mono.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserAudio {
    pub speech: Option<Vec<f32>>,
    pub noise: Option<Vec<f32>>,
    pub far_speech: Option<Vec<f32>>,
}

/// Reads the recordings given (48 kHz only); an error names the option.
pub fn read_user_audio(speech: Option<&Path>, noise: Option<&Path>, far_speech: Option<&Path>) -> Result<UserAudio, Failure> {
    let read = |flag: &str, path: Option<&Path>| {
        path.map(|p| wav::read_mono_48k(p).map(|m| m.samples).map_err(|f| Failure::error(format!("{flag} {}", f.message)))).transpose()
    };
    Ok(UserAudio { speech: read("--speech", speech)?, noise: read("--noise", noise)?, far_speech: read("--far-speech", far_speech)? })
}

/// One quality-test scene, 48 kHz mono.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub test: QualityTest,
    /// Played as the mic: the near speech, the noise and (echo test) the echo.
    pub input: Vec<f32>,
    /// The near speech alone, as mixed in: the recording is aligned with it and measured against it.
    pub clean: Vec<f32>,
    /// The noise alone, as mixed in.
    pub noise: Vec<f32>,
    /// Echo test: the echo alone as mixed in, and the far voice in step with it: the echo is `far` through the room, with its
    /// direct sound at the same index. The far-end device plays `far` [`Scene::echo_delay_ms`] ahead of its echo by the
    /// streams' stamps (`super::echo_delay::Plan` places it).
    pub echo: Option<Vec<f32>>,
    pub far: Option<Vec<f32>>,
    /// Echo test: how long after the far end's loopback taps the far voice its echo is to reach the mic (`--echo-delay`).
    pub echo_delay_ms: Option<f32>,
    /// Where the near speech, and the far voice, are active (sample ranges in time order).
    pub near_activity: Vec<Range<usize>>,
    pub far_activity: Vec<Range<usize>>,
    /// What was built, for the report.
    pub description: String,
}

/// Builds the scene: `seconds` long, the noise `snr_db` below the speech (or the test's default), with the user's recordings
/// in place of the synthetic parts they replace; for the echo test, its echo `echo_delay_ms` after the far voice's tap (the
/// description says so; the command plays it so).
pub fn build(test: QualityTest, seconds: f32, snr_db: Option<f32>, echo_delay_ms: f32, user: &UserAudio) -> Result<Scene, String> {
    let len = (f64::from(seconds) * f64::from(SAMPLE_RATE)).round() as usize;
    if len < SAMPLE_RATE as usize {
        return Err("The scene must be at least 1 s long.".into());
    }
    match test {
        QualityTest::Noise => noise_scene(len, snr_db.unwrap_or(DEFAULT_NOISE_SNR_DB), user),
        QualityTest::Echo => echo_scene(len, snr_db.unwrap_or(DEFAULT_ECHO_SNR_DB), echo_delay_ms, user),
    }
}

fn noise_scene(len: usize, snr_db: f32, user: &UserAudio) -> Result<Scene, String> {
    let mut near = near_track(len, user)?;
    near.set_level(SPEECH_DBFS);
    let (input, noise) = mix_at_snr(&near.samples, near.level(), &noise_source(len, user)?, snr_db);
    let mut scene = Scene {
        test: QualityTest::Noise,
        input,
        clean: near.samples,
        noise,
        echo: None,
        far: None,
        echo_delay_ms: None,
        near_activity: near.activity,
        far_activity: Vec::new(),
        description: String::new(),
    };
    let scaled = limit_peak(&mut scene);
    scene.description = format!(
        "Noise test, {} s: {} at {SPEECH_DBFS:.1} dBFS active level ({} s of it active), {} {snr_db:.1} dB below it{scaled}.",
        seconds_text(len),
        speech_name(user.speech.is_some(), "--speech"),
        seconds_text(active_len(&scene.near_activity)),
        noise_name(user),
    );
    Ok(scene)
}

fn echo_scene(len: usize, snr_db: f32, echo_delay_ms: f32, user: &UserAudio) -> Result<Scene, String> {
    let (near_turns, far_turns) = turns(len);
    let mut near = near_track(len, user)?.within(&near_turns, "--speech")?;
    near.set_level(SPEECH_DBFS);
    let far_voice = match &user.far_speech {
        Some(x) => Track::recorded(x, len, "--far-speech")?,
        None => Track::synthetic(len, Voice::HIGH, SEED_FAR),
    };
    let mut far = far_voice.within(&far_turns, "--far-speech")?;
    far.set_level(SPEECH_DBFS);
    let far_scaled = scale_to_peak(&mut far.samples);

    let rir = room_impulse_response((RIR_S * f64::from(SAMPLE_RATE)) as usize, RIR_RT60_S, SEED_RIR);
    let mut echo = convolve(&far.samples, &rir);
    echo.truncate(len);
    let near_level = near.level();
    let echo_level = rms_over(&echo, &far.activity);
    if echo_level > 0.0 {
        let g = near_level * 10f32.powf(-ECHO_BELOW_DB / 20.0) / echo_level;
        echo.iter_mut().for_each(|v| *v *= g);
    }
    let (near_and_noise, noise) = mix_at_snr(&near.samples, near_level, &noise_source(len, user)?, snr_db);
    let mut scene = Scene {
        test: QualityTest::Echo,
        input: add(&near_and_noise, &echo),
        clean: near.samples,
        noise,
        echo: Some(echo),
        far: Some(far.samples),
        echo_delay_ms: Some(echo_delay_ms),
        near_activity: near.activity,
        far_activity: far.activity,
        description: String::new(),
    };
    let scaled = limit_peak(&mut scene);
    let far_scaled = if far_scaled < 0.0 { format!(", scaled {far_scaled:.1} dB to keep it from clipping") } else { String::new() };
    scene.description = format!(
        "Echo test, {} s in {TURN_S:.0} s turns of far only, double talk and near only: near {} at {SPEECH_DBFS:.1} dBFS active level, \
         the far voice's echo {ECHO_BELOW_DB:.1} dB below it (RT60 {RIR_RT60_S:.2} s), {} {snr_db:.1} dB below it{scaled}; \
         the far voice ({}) at {SPEECH_DBFS:.1} dBFS active level on the far-end device{far_scaled}, played so that its echo \
         reaches the mic {} ms after the far end's loopback taps it (a speaker's output latency and the air; calibrated in the take).",
        seconds_text(len),
        speech_name(user.speech.is_some(), "--speech"),
        noise_name(user),
        speech_name(user.far_speech.is_some(), "--far-speech"),
        delay_text(echo_delay_ms),
    );
    Ok(scene)
}

/// A delay in ms as the description gives it: "25", "12.5".
fn delay_text(ms: f32) -> String {
    if (ms - ms.round()).abs() < 1e-3 { format!("{ms:.0}") } else { format!("{ms:.1}") }
}

fn near_track(len: usize, user: &UserAudio) -> Result<Track, String> {
    match &user.speech {
        Some(x) => Track::recorded(x, len, "--speech"),
        None => Ok(Track::synthetic(len, Voice::LOW, SEED_NEAR)),
    }
}

/// The noise to mix in, at any level (it's scaled to the SNR).
fn noise_source(len: usize, user: &UserAudio) -> Result<Vec<f32>, String> {
    match &user.noise {
        Some(x) => {
            let x = looped(x, len, "--noise")?;
            if x.iter().all(|&v| v == 0.0) { Err("The --noise recording is silent.".into()) } else { Ok(x) }
        }
        None => Ok(pink_noise(len, 1.0, SEED_NOISE)),
    }
}

/// Scales the mic parts of `scene` together so the input peaks under [`MAX_PEAK`]; says so when it did.
fn limit_peak(scene: &mut Scene) -> String {
    let peak = scene.input.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak <= MAX_PEAK {
        return String::new();
    }
    let g = MAX_PEAK / peak;
    for x in [Some(&mut scene.input), Some(&mut scene.clean), Some(&mut scene.noise), scene.echo.as_mut()].into_iter().flatten() {
        x.iter_mut().for_each(|v| *v *= g);
    }
    format!(" (all scaled {:.1} dB to keep the mic from clipping)", db(g))
}

/// Scales `x` to peak under [`MAX_PEAK`]; returns the gain in dB (0 when it fits).
fn scale_to_peak(x: &mut [f32]) -> f32 {
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak <= MAX_PEAK {
        return 0.0;
    }
    let g = MAX_PEAK / peak;
    x.iter_mut().for_each(|v| *v *= g);
    db(g)
}

fn speech_name(recorded: bool, flag: &str) -> String {
    if recorded { format!("the {flag} recording") } else { "synthetic speech".into() }
}

fn noise_name(user: &UserAudio) -> &'static str {
    if user.noise.is_some() { "the --noise recording" } else { "pink noise" }
}

fn seconds_text(samples: usize) -> String {
    let s = samples as f64 / f64::from(SAMPLE_RATE);
    if (s - s.round()).abs() < 1e-9 { format!("{s:.0}") } else { format!("{s:.1}") }
}

fn active_len(activity: &[Range<usize>]) -> usize {
    activity.iter().map(|r| r.len()).sum()
}

/// `x` repeated or cut to `len`.
fn looped(x: &[f32], len: usize, flag: &str) -> Result<Vec<f32>, String> {
    if x.is_empty() {
        return Err(format!("The {flag} recording is empty."));
    }
    Ok(x.iter().copied().cycle().take(len).collect())
}

/// A voice and where it's active.
struct Track {
    samples: Vec<f32>,
    activity: Vec<Range<usize>>,
    /// Synthetic: silent outside its syllables, which are its activity.
    synthetic: bool,
}

impl Track {
    fn synthetic(len: usize, voice: Voice, seed: u64) -> Self {
        let s = speech_like(len, voice.at(SPEECH_DBFS), seed);
        Self { samples: s.samples, activity: s.syllables, synthetic: true }
    }

    fn recorded(x: &[f32], len: usize, flag: &str) -> Result<Self, String> {
        let samples = looped(x, len, flag)?;
        let activity = speech_activity(&samples);
        if activity.is_empty() {
            return Err(format!("The {flag} recording is silent."));
        }
        Ok(Self { samples, activity, synthetic: false })
    }

    /// RMS over the activity.
    fn level(&self) -> f32 {
        rms_over(&self.samples, &self.activity)
    }

    fn set_level(&mut self, dbfs: f32) {
        let level = self.level();
        if level > 0.0 {
            let g = 10f32.powf(dbfs / 20.0) / level;
            self.samples.iter_mut().for_each(|v| *v *= g);
        }
    }

    /// The voice inside `windows` only (see the module notes).
    fn within(mut self, windows: &[Range<usize>], flag: &str) -> Result<Self, String> {
        let inside = |r: &Range<usize>| windows.iter().any(|w| w.start <= r.start && r.end <= w.end);
        if self.synthetic {
            for r in self.activity.iter().filter(|r| !inside(r)) {
                self.samples[r.clone()].fill(0.0);
            }
            self.activity.retain(|r| inside(r));
        } else {
            gate(&mut self.samples, windows, (GATE_FADE_S * f64::from(SAMPLE_RATE)) as usize);
            // Silent outside its windows, so the hangover alone reaches past them: it's cut there.
            self.activity = within_windows(&speech_activity(&self.samples), windows);
        }
        if self.activity.is_empty() {
            let what = if self.synthetic { "The synthetic voice".to_owned() } else { format!("The {flag} recording") };
            return Err(format!("{what} has no speech in its turns; make the scene longer (--seconds)."));
        }
        Ok(self)
    }
}

/// The echo test's three kinds of turn, in the order they come round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    FarOnly,
    DoubleTalk,
    NearOnly,
}

/// The echo test's turn length in samples.
pub fn turn_len() -> usize {
    (TURN_S * f64::from(SAMPLE_RATE)) as usize
}

/// The kind of turn sample `i` lies in: turn k is far only, double talk or near only as k % 3 is 0, 1 or 2.
pub fn turn_at(i: usize) -> Turn {
    match (i / turn_len()) % 3 {
        0 => Turn::FarOnly,
        1 => Turn::DoubleTalk,
        _ => Turn::NearOnly,
    }
}

/// The echo test's turns as (near windows, far windows), adjacent ones merged (see [`turn_at`]).
pub fn turns(len: usize) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (mut near, mut far) = (Vec::new(), Vec::new());
    for start in (0..len).step_by(turn_len()) {
        let r = start..(start + turn_len()).min(len);
        if turn_at(start) != Turn::FarOnly {
            push_merged(&mut near, r.clone());
        }
        if turn_at(start) != Turn::NearOnly {
            push_merged(&mut far, r);
        }
    }
    (near, far)
}

fn push_merged(v: &mut Vec<Range<usize>>, r: Range<usize>) {
    match v.last_mut() {
        Some(last) if last.end == r.start => last.end = r.end,
        _ => v.push(r),
    }
}

/// Zeroes `x` outside `windows`, with raised-cosine fades of `fade` samples inside each window's ends.
fn gate(x: &mut [f32], windows: &[Range<usize>], fade: usize) {
    let mut gain = vec![0.0f32; x.len()];
    for w in windows {
        let w = w.start.min(x.len())..w.end.min(x.len());
        for i in w.clone() {
            let edge = (i - w.start).min(w.end - 1 - i);
            gain[i] =
                if edge >= fade { 1.0 } else { (0.5 - 0.5 * (std::f64::consts::PI * (edge as f64 + 0.5) / fade as f64).cos()) as f32 };
        }
    }
    x.iter_mut().zip(&gain).for_each(|(v, g)| *v *= g);
}

/// A recording's speech activity: [`detect_activity`] with [`HANGOVER_S`] added after each run, the runs merged where they meet.
fn speech_activity(x: &[f32]) -> Vec<Range<usize>> {
    let hang = (HANGOVER_S * f64::from(SAMPLE_RATE)) as usize;
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in detect_activity(x) {
        let r = r.start..(r.end + hang).min(x.len());
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The parts of `runs` inside `windows` (both in time order).
fn within_windows(runs: &[Range<usize>], windows: &[Range<usize>]) -> Vec<Range<usize>> {
    runs.iter().flat_map(|r| windows.iter().map(move |w| r.start.max(w.start)..r.end.min(w.end))).filter(|r| !r.is_empty()).collect()
}

/// Speech activity in `x`: the 10 ms frames whose energy lies within 15.9 dB of the active level, the active level being the
/// mean energy of those frames (found by iteration from the mean of every frame that isn't silent), with gaps up to 30 ms
/// bridged and runs under 20 ms dropped. Sample ranges in time order.
pub fn detect_activity(x: &[f32]) -> Vec<Range<usize>> {
    let energy: Vec<f64> = x.chunks(FRAME).map(|c| c.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / c.len() as f64).collect();
    let loudest = energy.iter().copied().fold(0.0f64, f64::max);
    if loudest <= 0.0 {
        return Vec::new();
    }
    // Digital silence and near-silence (60 dB under the loudest frame) don't count towards the first estimate.
    let mean_over = |thr: f64| {
        let (sum, n) = energy.iter().filter(|&&e| e >= thr).fold((0.0, 0usize), |(s, n), &e| (s + e, n + 1));
        sum / n.max(1) as f64
    };
    let margin = 10f64.powf(-ACTIVE_BELOW_DB / 10.0);
    let mut level = mean_over(loudest * 1e-6);
    let mut threshold = level * margin;
    for _ in 0..50 {
        let next = mean_over(threshold);
        if next == level {
            break;
        }
        level = next;
        threshold = level * margin;
    }
    // Runs of active frames, bridging short gaps.
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (k, &e) in energy.iter().enumerate() {
        if e < threshold {
            continue;
        }
        match runs.last_mut() {
            Some(last) if k - last.end <= BRIDGE_FRAMES => last.end = k + 1,
            _ => runs.push(k..k + 1),
        }
    }
    runs.into_iter().filter(|r| r.len() >= MIN_RUN_FRAMES).map(|r| r.start * FRAME..(r.end * FRAME).min(x.len())).collect()
}

#[cfg(test)]
mod tests {
    use hound::{SampleFormat, WavSpec, WavWriter};
    use nf_stages::synth::rms;

    use super::*;

    const FS: usize = 48_000;

    fn level_db(x: &[f32], ranges: &[Range<usize>]) -> f32 {
        db(rms_over(x, ranges))
    }

    /// The samples of `ranges` that `activity` covers, as a fraction.
    fn covered(ranges: &[Range<usize>], activity: &[Range<usize>]) -> f64 {
        let total: usize = ranges.iter().map(|r| r.len()).sum();
        let hit: usize = ranges.iter().flat_map(|r| r.clone()).filter(|&i| activity.iter().any(|a| a.contains(&i))).count();
        hit as f64 / total as f64
    }

    /// The echo delay the tests build with (the command's default).
    const DELAY: f32 = 25.0;

    #[test]
    fn the_noise_scene_has_the_levels_and_snr_asked_for() {
        let s = build(QualityTest::Noise, 10.0, None, DELAY, &UserAudio::default()).unwrap();
        assert_eq!(s.input.len(), 10 * FS);
        assert_eq!((s.clean.len(), s.noise.len()), (10 * FS, 10 * FS));
        assert!(s.far.is_none() && s.echo.is_none() && s.far_activity.is_empty() && s.echo_delay_ms.is_none());
        assert!((level_db(&s.clean, &s.near_activity) - SPEECH_DBFS).abs() < 0.01);
        assert!((db(rms(&s.noise)) - (SPEECH_DBFS - DEFAULT_NOISE_SNR_DB)).abs() < 0.01);
        // The input is exactly the two parts
        assert!(s.input.iter().zip(s.clean.iter().zip(&s.noise)).all(|(i, (c, n))| (i - (c + n)).abs() < 1e-6));
        assert!(s.near_activity.len() >= 20, "{} syllables", s.near_activity.len());
        assert!(s.description.starts_with("Noise test, 10 s: synthetic speech at -20.0 dBFS"), "{}", s.description);
        assert!(s.description.contains("pink noise 10.0 dB below it."), "{}", s.description);
        // Another SNR, and the same scene every time
        let t = build(QualityTest::Noise, 10.0, Some(25.0), DELAY, &UserAudio::default()).unwrap();
        assert!((db(rms(&t.noise)) - (SPEECH_DBFS - 25.0)).abs() < 0.01);
        assert_eq!(t.clean, s.clean);
        assert!(build(QualityTest::Noise, 0.5, None, DELAY, &UserAudio::default()).is_err());
    }

    #[test]
    fn the_echo_scene_takes_turns_and_sets_the_echo_6_db_down() {
        let s = build(QualityTest::Echo, 18.0, None, DELAY, &UserAudio::default()).unwrap();
        let (far, echo) = (s.far.as_ref().unwrap(), s.echo.as_ref().unwrap());
        assert_eq!((far.len(), echo.len(), s.input.len()), (18 * FS, 18 * FS, 18 * FS));
        // Turns of 3 s: far only, double talk, near only, twice. Near talks in 3-9 s and 12-18 s, far in 0-6 s and 9-15 s.
        let (near_turns, far_turns) = turns(18 * FS);
        assert_eq!(near_turns, [3 * FS..9 * FS, 12 * FS..18 * FS]);
        assert_eq!(far_turns, [0..6 * FS, 9 * FS..15 * FS]);
        let inside = |r: &Range<usize>, w: &[Range<usize>]| w.iter().any(|w| w.start <= r.start && r.end <= w.end);
        assert!(s.near_activity.iter().all(|r| inside(r, &near_turns)), "{:?}", s.near_activity);
        assert!(s.far_activity.iter().all(|r| inside(r, &far_turns)), "{:?}", s.far_activity);
        // Outside its turns each voice is silent
        assert!(s.clean[..3 * FS].iter().all(|&v| v == 0.0) && s.clean[9 * FS..12 * FS].iter().all(|&v| v == 0.0));
        assert!(far[6 * FS..9 * FS].iter().all(|&v| v == 0.0) && far[15 * FS..].iter().all(|&v| v == 0.0));
        // Levels: both voices at -20 dBFS active, the echo 6 dB under the near speech, the noise 60 dB under it
        assert!((level_db(&s.clean, &s.near_activity) - SPEECH_DBFS).abs() < 0.01);
        assert!((level_db(far, &s.far_activity) - SPEECH_DBFS).abs() < 0.01);
        assert!((level_db(echo, &s.far_activity) - (SPEECH_DBFS - ECHO_BELOW_DB)).abs() < 0.01);
        assert!((db(rms(&s.noise)) - (SPEECH_DBFS - DEFAULT_ECHO_SNR_DB)).abs() < 0.01);
        // The echo is the far voice through the room: it correlates with the far voice, starts no earlier, and dies out within
        // the response's length after the far voice stops.
        assert!(echo[..FS].iter().any(|&v| v != 0.0));
        let tail_end = 6 * FS + (RIR_S * FS as f64) as usize;
        assert!(echo[tail_end..9 * FS].iter().all(|&v| v.abs() < 1e-6));
        // The echo's direct sound is the far voice itself, at the same index: the scene is in the mic's time, and the far end
        // is placed by the take (`echo_delay`), whatever the delay
        let (direct, _) = nf_core::latency::analysis::gcc_phat(far, echo, 2_400, 1.0).unwrap();
        assert!(direct.abs() < 0.1, "the echo's direct sound {direct} samples after the far voice");
        assert!(
            s.description.ends_with(
                "on the far-end device, played so that its echo reaches the mic 25 ms after the far end's loopback taps it (a \
                 speaker's output latency and the air; calibrated in the take)."
            ),
            "{}",
            s.description
        );
        assert_eq!(s.echo_delay_ms, Some(DELAY));
        let other = build(QualityTest::Echo, 18.0, None, 12.5, &UserAudio::default()).unwrap();
        assert!(other.description.contains("reaches the mic 12.5 ms after"), "{}", other.description);
        assert_eq!(
            (&other.input, &other.far, &other.far_activity, &other.near_activity),
            (&s.input, &s.far, &s.far_activity, &s.near_activity)
        );
        let input_parts = s.input.iter().zip(&s.clean).zip(s.noise.iter().zip(echo.iter()));
        assert!(input_parts.into_iter().all(|((i, c), (n, e))| (i - (c + n + e)).abs() < 1e-6));
        assert!(s.description.starts_with("Echo test, 18 s in 3 s turns"), "{}", s.description);
    }

    #[test]
    fn recordings_replace_the_synthetic_parts_at_the_same_levels() {
        // A "recording": a synthetic voice at -35 dBFS with a little hiss, 4 s, looped to 10 s
        let voice = speech_like(4 * FS, Voice::HIGH.at(-35.0), 9);
        let hiss = pink_noise(4 * FS, 10f32.powf(-80.0 / 20.0), 10);
        let speech = add(&voice.samples, &hiss);
        let hum: Vec<f32> = (0..FS).map(|i| (i as f32 * 0.01).sin()).collect();
        let user = UserAudio { speech: Some(speech.clone()), noise: Some(hum), far_speech: None };
        let s = build(QualityTest::Noise, 10.0, Some(15.0), DELAY, &user).unwrap();
        assert_eq!(s.clean.len(), 10 * FS);
        assert!((level_db(&s.clean, &s.near_activity) - SPEECH_DBFS).abs() < 0.01);
        assert!((db(rms(&s.noise)) - (SPEECH_DBFS - 15.0)).abs() < 0.01);
        // Looped: the 4 s repeat
        assert_eq!(s.clean[FS], s.clean[5 * FS]);
        assert!(s.description.contains("the --speech recording") && s.description.contains("the --noise recording"), "{}", s.description);
        // The detector found the voice's syllables, and nothing in its pauses
        let looped_syllables: Vec<Range<usize>> = (0..3)
            .flat_map(|k| voice.syllables.iter().map(move |r| r.start + 4 * k * FS..(r.end + 4 * k * FS).min(10 * FS)))
            .filter(|r| r.start < 10 * FS)
            .collect();
        let cores: Vec<Range<usize>> = looped_syllables.iter().filter(|r| r.len() > 4800).map(|r| r.start + 2400..r.end - 2400).collect();
        assert!(covered(&cores, &s.near_activity) > 0.95, "{}", covered(&cores, &s.near_activity));
        // Up to 50 ms before a syllable, and up to the hangover and 50 ms after it
        let hang = (HANGOVER_S * FS as f64) as usize;
        let near_speech = |i: usize| looped_syllables.iter().any(|r| i + 2400 >= r.start && i < r.end + hang + 2400);
        assert!(s.near_activity.iter().flat_map(|r| r.clone()).all(near_speech), "activity in a pause");

        // In the echo test the recording is gated to its turns
        let e = build(QualityTest::Echo, 9.0, None, DELAY, &UserAudio { far_speech: Some(speech), ..UserAudio::default() }).unwrap();
        assert!(e.far_activity.iter().all(|r| r.end <= 6 * FS + FRAME), "{:?}", e.far_activity);
        assert!(e.far.as_ref().unwrap()[6 * FS..].iter().all(|&v| v == 0.0));
        assert!(e.description.contains("the far voice (the --far-speech recording)"), "{}", e.description);

        // Empty or silent recordings are refused, by name
        let err = build(QualityTest::Noise, 5.0, None, DELAY, &UserAudio { speech: Some(Vec::new()), ..UserAudio::default() }).unwrap_err();
        assert_eq!(err, "The --speech recording is empty.");
        let err =
            build(QualityTest::Noise, 5.0, None, DELAY, &UserAudio { speech: Some(vec![0.0; FS]), ..UserAudio::default() }).unwrap_err();
        assert_eq!(err, "The --speech recording is silent.");
        let err =
            build(QualityTest::Noise, 5.0, None, DELAY, &UserAudio { noise: Some(vec![0.0; FS]), ..UserAudio::default() }).unwrap_err();
        assert_eq!(err, "The --noise recording is silent.");
    }

    #[test]
    fn a_loud_recording_is_scaled_down_rather_than_clipped() {
        // A tone with sparse clicks, so its peaks sit far above its active level (about 22 dB), with noise at 0 dB SNR
        let mut x = vec![0.0f32; 2 * FS];
        for (i, v) in x.iter_mut().enumerate().skip(FS / 2).take(FS) {
            *v = if (i / 40) % 240 == 0 { 0.9 } else { 0.05 * ((i as f32) * 0.3).sin() };
        }
        let s = build(QualityTest::Noise, 4.0, Some(0.0), DELAY, &UserAudio { speech: Some(x), ..UserAudio::default() }).unwrap();
        let peak = s.input.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= MAX_PEAK + 1e-6, "{peak}");
        assert!(s.description.contains("to keep the mic from clipping"), "{}", s.description);
        assert!(s.input.iter().zip(s.clean.iter().zip(&s.noise)).all(|(i, (c, n))| (i - (c + n)).abs() < 1e-6), "scaled together");
    }

    #[test]
    fn the_detector_follows_the_level_not_the_absolute_scale() {
        let s = speech_like(6 * FS, Voice::LOW, 4);
        for gain_db in [0.0f32, -30.0] {
            let g = 10f32.powf(gain_db / 20.0);
            let x: Vec<f32> = s.samples.iter().map(|v| v * g).collect();
            let a = detect_activity(&x);
            let cores: Vec<Range<usize>> = s.cores(2400);
            assert!(covered(&cores, &a) > 0.95, "{gain_db} dB: {}", covered(&cores, &a));
            assert!(a.iter().all(|r| r.start % FRAME == 0), "frame-aligned");
            // Frames overhang a syllable by up to one frame, and a bridged gap reaches up to 2.5 frames from the syllables
            // either side.
            assert!(a.iter().flat_map(|r| r.clone()).all(|i| s.near_syllable(i, 3 * FRAME)), "{gain_db} dB: activity in a pause");
        }
        assert!(detect_activity(&[0.0; 4800]).is_empty());
        assert!(detect_activity(&[]).is_empty());
    }

    /// The energy of `x` outside `activity` and 50 ms either side of it, against its level over the activity, in dB.
    fn left_in_pauses(x: &[f32], activity: &[Range<usize>]) -> f32 {
        let mut pauses = Vec::new();
        let mut from = 0;
        for r in activity {
            if r.start > from + 4800 {
                pauses.push(from + 2400..r.start - 2400);
            }
            from = r.end;
        }
        level_db(x, &pauses) - level_db(x, activity)
    }

    /// A reverberant recording's tails are speech the filter keeps: the hangover counts them as activity, so the pauses the
    /// analysis measures hold far less of them.
    #[test]
    fn a_recordings_reverberation_counts_as_speech() {
        let dry = speech_like(12 * FS, Voice::LOW, 12);
        let mut wet = convolve(&dry.samples, &room_impulse_response(FS / 2, 0.4, 13));
        wet.truncate(12 * FS);
        let (bare, held) = (detect_activity(&wet), speech_activity(&wet));
        let (bare_left, held_left) = (left_in_pauses(&wet, &bare), left_in_pauses(&wet, &held));
        assert!(held_left < bare_left - 10.0 && held_left < -40.0, "left in the pauses: {bare_left} dB bare, {held_left} dB held");
        // Each run is the detector's, 200 ms longer, merged where they meet
        let hang = (HANGOVER_S * FS as f64) as usize;
        assert!(held.iter().all(|h| bare.iter().any(|b| b.start == h.start)), "{held:?}");
        assert!(held.iter().all(|h| bare.iter().any(|b| b.end + hang == h.end || b.end + hang > wet.len())), "{held:?}");
        assert!(held.windows(2).all(|w| w[0].end < w[1].start) && held.len() < bare.len());
        // Clipped to the turns' windows
        assert_eq!(within_windows(&[0..10, 15..30], &[5..20, 25..40]), [5..10, 15..20, 25..30]);
    }

    #[test]
    fn gating_fades_at_the_window_edges() {
        let mut x = vec![1.0f32; 100];
        gate(&mut x, std::slice::from_ref(&(10..50)), 4);
        assert!(x[..10].iter().all(|&v| v == 0.0) && x[50..].iter().all(|&v| v == 0.0));
        assert!(x[14..46].iter().all(|&v| v == 1.0));
        assert!(x[10] > 0.0 && x[10] < x[11] && x[11] < x[12] && x[12] < x[13] && x[13] < 1.0);
        assert!((x[10] - x[49]).abs() < 1e-6, "symmetric");
        // Far only, then a second of double talk: the far end's turns merge
        let (near, far) = turns(4 * FS);
        assert_eq!((near.len(), far.len()), (1, 1));
        assert_eq!((near[0].clone(), far[0].clone()), (3 * FS..4 * FS, 0..4 * FS));
    }

    #[test]
    fn recordings_must_be_48_khz_and_errors_name_the_option() {
        let dir = crate::test_dir("quality-wav");
        let spec = |rate| WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: SampleFormat::Int };
        let write = |name: &str, rate: u32| {
            let path = dir.join(name);
            let mut w = WavWriter::create(&path, spec(rate)).unwrap();
            for i in 0..2000 {
                w.write_sample(((i % 50) * 200) as i16).unwrap();
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
            path
        };
        let (ok, cd) = (write("ok.wav", 48_000), write("cd.wav", 44_100));
        let user = read_user_audio(Some(&ok), None, Some(&ok)).unwrap();
        assert_eq!(user.speech.as_ref().map(Vec::len), Some(2000), "stereo downmixed to mono");
        assert_eq!(user.speech, user.far_speech);
        assert_eq!(user.noise, None);
        let e = read_user_audio(None, Some(&cd), None).unwrap_err();
        assert!(
            e.message.starts_with("--noise ") && e.message.contains("44100 Hz") && e.message.contains("48000 Hz only"),
            "{}",
            e.message
        );
        let e = read_user_audio(None, None, Some(&dir.join("missing.wav"))).unwrap_err();
        assert!(e.message.starts_with("--far-speech ") && e.message.contains("missing.wav"), "{}", e.message);
        let _ = std::fs::remove_dir_all(dir);
    }
}
