//! The Lab's playlist (TODO 14 phase 0): clips built from the fetched test-audio set (`tools/fetch_test_audio.py`; the
//! catalogue's starter set), the same every run.
//!
//! - **Speech:** each `speech_*.wav` take (TSP's six speakers, VCTK's four) cut into segments of `--clip-seconds` (15 s by
//!   default) from its start; a last part of 10 s or more is a segment too, a shorter one is dropped. A TSP take (14.5-17 s)
//!   gives one segment, a VCTK take (38-48 s) two or three: distinct speech, so more distinct clips (the unit of analysis).
//! - **Noise:** each `noise_*.wav` and `music_*.wav`, by a label from its name (`keyboard`, `fan`, `aircon`, `dogs`, `cafe`
//!   for Freesound's cafe, `cafe-demand` for DEMAND's, `music`). A noise longer than the clip is cut from an offset that
//!   moves with the segment and the SNR (in steps of 0.1 s), so clips don't all share its first seconds; a shorter one is
//!   looped from its start.
//! - **Clips:** every segment with every noise at every `--snr` (0, 10 and 20 dB by default), built by `quality`'s scene
//!   builder (`quality::signals::build`, the noise test): the speech at -20 dBFS active level (P.56-style activity with a
//!   200 ms hangover), the noise that far below it, both scaled down together if the mix would clip. So a clip's levels are
//!   those of a `quality --speech --noise` take.
//! - **The clean reference** of a segment: the same speech at the same level, without the noise ([`super::super::quality::
//!   signals::near_speech`]): scored once per segment.
//!
//! [`plan`] is a pure function of the takes' names and lengths; [`scene`] builds one clip's signals.

use std::ops::Range;
use std::path::{Path, PathBuf};

use nf_core::SAMPLE_RATE;

use crate::quality::QualityTest;
use crate::quality::signals::{self, Scene, UserAudio};

/// The defaults: SNRs, the segment length, and the shortest last segment kept.
pub const DEFAULT_SNRS_DB: [f32; 3] = [0.0, 10.0, 20.0];
pub const DEFAULT_CLIP_S: f32 = 15.0;
/// Shorter than this, DNSMOS would tile the clip (its window is 9.01 s), and the catalogue asks for takes of 10 s or more.
pub const MIN_CLIP_S: f32 = 10.0;
pub const MAX_CLIP_S: f32 = 60.0;
/// A noise's offset steps.
const OFFSET_STEP_S: f64 = 0.1;

/// One take of the set: its file's stem (`speech_tsp_fa01`), its samples (48 kHz mono) and what the set's manifest says of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub name: String,
    pub samples: Vec<f32>,
    pub licence: Option<String>,
    pub attribution: Option<String>,
}

#[cfg(test)]
impl Take {
    /// A take without the manifest's facts (tests).
    pub fn new(name: &str, samples: Vec<f32>) -> Self {
        Self { name: name.to_owned(), samples, licence: None, attribution: None }
    }
}

/// A part of a speech take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Index into [`Playlist::speech`].
    pub take: usize,
    /// `tsp_fa01`, or `vctk_p225#2` for a take's second segment.
    pub name: String,
    pub range: Range<usize>,
}

/// One clip: a segment in a noise at an SNR.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// `tsp_fa01+keyboard@10`.
    pub id: String,
    pub segment: usize,
    /// Index into [`Playlist::noises`], and the noise's label.
    pub noise: usize,
    pub snr_db: f32,
    /// Where the noise is read from (samples).
    pub noise_offset: usize,
}

/// What to build.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub snrs_db: Vec<f32>,
    pub clip_s: f32,
    /// Only these noise labels, or these speech segments' takes (`tsp_fa01`, `vctk_p225`); all when empty.
    pub noises: Vec<String>,
    pub speech: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { snrs_db: DEFAULT_SNRS_DB.to_vec(), clip_s: DEFAULT_CLIP_S, noises: Vec::new(), speech: Vec::new() }
    }
}

/// The takes, the segments and the clips, in order (segment by segment, each noise, each SNR).
#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub speech: Vec<Take>,
    pub noises: Vec<Take>,
    /// Each noise's label, in [`Playlist::noises`]' order.
    pub labels: Vec<String>,
    pub segments: Vec<Segment>,
    pub clips: Vec<Clip>,
    pub options: Options,
}

/// `speech_tsp_fa01` → `tsp_fa01`.
pub fn speech_name(stem: &str) -> &str {
    stem.strip_prefix("speech_").unwrap_or(stem)
}

/// A noise's label from its file's stem: `music` for `music_*`; else, after `noise_`, its first two parts less a trailing
/// numeric id (Freesound's), joined by `-`: `noise_keyboard_448209` → `keyboard`, `noise_cafe_demand_pcafeter_ch01` →
/// `cafe-demand`.
pub fn noise_label(stem: &str) -> String {
    if stem.starts_with("music_") {
        return "music".into();
    }
    let mut parts: Vec<&str> = stem.strip_prefix("noise_").unwrap_or(stem).split('_').collect();
    if parts.len() > 1 && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit())) {
        parts.pop();
    }
    parts.truncate(2);
    parts.join("-")
}

/// The segments of a take of `len` samples: `clip` long from its start, and a last one if at least [`MIN_CLIP_S`].
pub fn segments(len: usize, clip: usize) -> Vec<Range<usize>> {
    let min = (f64::from(MIN_CLIP_S) * f64::from(SAMPLE_RATE)) as usize;
    let mut out = Vec::new();
    let mut at = 0;
    while at < len {
        let end = (at + clip).min(len);
        if end - at >= min.min(clip) {
            out.push(at..end);
        }
        at = end;
    }
    out
}

/// The playlist over `speech` and `noises` (see the module notes). Errors say what's missing.
pub fn plan(speech: Vec<Take>, noises: Vec<Take>, options: Options) -> Result<Playlist, String> {
    let fs = f64::from(SAMPLE_RATE);
    let clip = (f64::from(options.clip_s) * fs).round() as usize;
    let keep_speech = |t: &Take| options.speech.is_empty() || options.speech.iter().any(|s| s == speech_name(&t.name));
    let speech: Vec<Take> = speech.into_iter().filter(keep_speech).collect();
    let labels_all: Vec<String> = noises.iter().map(|n| noise_label(&n.name)).collect();
    for want in &options.noises {
        if !labels_all.contains(want) {
            return Err(format!("no noise labelled '{want}' in the set (it has {})", labels_all.join(", ")));
        }
    }
    for want in &options.speech {
        if !speech.iter().any(|t| speech_name(&t.name) == want) {
            return Err(format!("no speech take '{want}' in the set"));
        }
    }
    let (noises, labels): (Vec<Take>, Vec<String>) =
        noises.into_iter().zip(labels_all).filter(|(_, l)| options.noises.is_empty() || options.noises.contains(l)).unzip();
    let mut segs = Vec::new();
    for (k, t) in speech.iter().enumerate() {
        let parts = segments(t.samples.len(), clip);
        let many = parts.len() > 1;
        for (j, r) in parts.into_iter().enumerate() {
            let name = if many { format!("{}#{}", speech_name(&t.name), j + 1) } else { speech_name(&t.name).to_owned() };
            segs.push(Segment { take: k, name, range: r });
        }
    }
    if segs.is_empty() {
        return Err(format!("no speech take of {MIN_CLIP_S:.0} s or more in the set"));
    }
    if noises.is_empty() {
        return Err("no noise in the set".into());
    }
    if options.snrs_db.is_empty() {
        return Err("no SNR to mix at".into());
    }
    let step = (OFFSET_STEP_S * fs) as usize;
    let mut clips = Vec::new();
    for (s, seg) in segs.iter().enumerate() {
        for (n, noise) in noises.iter().enumerate() {
            for (k, &snr) in options.snrs_db.iter().enumerate() {
                let room = noise.samples.len().saturating_sub(seg.range.len());
                // A different stretch of a long noise for each segment and SNR: steps of 0.1 s, coprime strides
                let noise_offset = if room == 0 { 0 } else { ((s * 7_919 + k * 1_009 + n * 104_729) * step) % room };
                clips.push(Clip {
                    id: format!("{}+{}@{}", seg.name, labels[n], snr_text(snr)),
                    segment: s,
                    noise: n,
                    snr_db: snr,
                    noise_offset,
                });
            }
        }
    }
    Ok(Playlist { speech, noises, labels, segments: segs, clips, options })
}

/// `10`, `-5`, `7.5`.
pub fn snr_text(db: f32) -> String {
    if (db - db.round()).abs() < 1e-6 { format!("{db:.0}") } else { format!("{db:.1}") }
}

impl Playlist {
    /// A segment's samples.
    pub fn segment(&self, s: usize) -> &[f32] {
        let seg = &self.segments[s];
        &self.speech[seg.take].samples[seg.range.clone()]
    }

    /// A segment's clean reference: its speech at the scene's level, no noise ([`signals::near_speech`]).
    pub fn clean(&self, s: usize) -> Result<Vec<f32>, String> {
        let x = self.segment(s);
        signals::near_speech(x, x.len()).map(|(c, _)| c)
    }

    /// One line: how many clips, from what.
    pub fn description(&self) -> String {
        let takes = |prefix: &str| self.segments.iter().filter(|s| self.speech[s.take].name.starts_with(prefix)).count();
        let (tsp, vctk) = (takes("speech_tsp"), takes("speech_vctk"));
        let other = self.segments.len() - tsp - vctk;
        let mut from = Vec::new();
        for (n, what) in [(tsp, "TSP"), (vctk, "VCTK"), (other, "other")] {
            if n > 0 {
                from.push(format!("{what} {n}"));
            }
        }
        let secs: Vec<f64> = self.segments.iter().map(|s| s.range.len() as f64 / f64::from(SAMPLE_RATE)).collect();
        let (lo, hi) = secs.iter().fold((f64::INFINITY, 0.0f64), |(l, h), &v| (l.min(v), h.max(v)));
        let snrs: Vec<String> = self.options.snrs_db.iter().map(|&d| snr_text(d)).collect();
        let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        let length = if (hi - lo).abs() < 0.05 { format!("{lo:.1} s") } else { format!("{lo:.1}-{hi:.1} s") };
        format!(
            "{}: {} ({}) x {} ({}) x {} {} dB, {length} each",
            count(self.clips.len(), "clip"),
            count(self.segments.len(), "speech segment"),
            from.join(", "),
            count(self.noises.len(), "noise"),
            self.labels.join(", "),
            if snrs.len() == 1 { "SNR" } else { "SNRs" },
            snrs.join(", "),
        )
    }
}

/// A clip's signals (`quality`'s noise scene: see the module notes).
pub fn scene(p: &Playlist, clip: &Clip) -> Result<Scene, String> {
    let speech = p.segment(clip.segment).to_vec();
    let len = speech.len();
    let noise = &p.noises[clip.noise].samples;
    if noise.is_empty() {
        return Err(format!("{}: the noise is empty", p.noises[clip.noise].name));
    }
    let excerpt: Vec<f32> = noise.iter().copied().cycle().skip(clip.noise_offset % noise.len()).take(len).collect();
    let user = UserAudio { speech: Some(speech), noise: Some(excerpt), far_speech: None };
    signals::build(QualityTest::Noise, len as f32 / SAMPLE_RATE as f32, Some(clip.snr_db), 0.0, &user)
        .map_err(|e| format!("{}: {e}", clip.id))
}

/// The set's folder: `NF_TEST_AUDIO`, else `%LOCALAPPDATA%\noise-filter\test-audio\wav48`.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("NF_TEST_AUDIO")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join("noise-filter").join("test-audio").join("wav48")))
}

/// The set's speech and noise takes from `dir` (sorted by name), with each one's licence and attribution from the set's
/// `manifest.json` (beside `dir`, or in it) when there is one.
pub fn read_set(dir: &Path) -> Result<(Vec<Take>, Vec<Take>), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!("No test audio in {} ({e}): fetch it with python -I tools/fetch_test_audio.py, or give --audio DIR.", dir.display())
    })?;
    let mut names: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| (s.to_owned(), p.clone())))
        .filter(|(s, _)| s.starts_with("speech_") || s.starts_with("noise_") || s.starts_with("music_"))
        .collect();
    names.sort();
    let facts = manifest_facts(dir);
    let (mut speech, mut noises) = (Vec::new(), Vec::new());
    for (name, path) in names {
        let wav = crate::wav::read_mono_48k(&path).map_err(|f| f.message)?;
        let (licence, attribution) = facts.iter().find(|f| f.0 == name).map(|f| (f.1.clone(), f.2.clone())).unwrap_or_default();
        let take = Take { name: name.clone(), samples: wav.samples, licence, attribution };
        if name.starts_with("speech_") { speech.push(take) } else { noises.push(take) }
    }
    if speech.is_empty() || noises.is_empty() {
        return Err(format!(
            "{} has no speech_*.wav or no noise_*.wav takes: fetch them with python -I tools/fetch_test_audio.py.",
            dir.display()
        ));
    }
    Ok((speech, noises))
}

/// (stem, licence, attribution) of each output the set's manifest lists.
fn manifest_facts(dir: &Path) -> Vec<(String, Option<String>, Option<String>)> {
    let text = [dir.join("manifest.json"), dir.parent().map(|p| p.join("manifest.json")).unwrap_or_default()]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    let Some(doc) = text.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { return Vec::new() };
    let field = |o: &serde_json::Value, k: &str| o.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter()
                .filter_map(|o| {
                    let file = field(o, "file")?;
                    let stem = Path::new(&file).file_stem()?.to_str()?.to_owned();
                    Some((stem, field(o, "licence"), field(o, "attribution")))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use nf_stages::synth::{Voice, db, pink_noise, rms, rms_over, speech_like};

    use super::*;

    const FS: usize = 48_000;

    fn set() -> (Vec<Take>, Vec<Take>) {
        let speech = vec![
            Take::new("speech_tsp_fa01", speech_like(16 * FS, Voice::HIGH.at(-30.0), 1).samples),
            Take::new("speech_vctk_p225", speech_like(41 * FS, Voice::LOW.at(-26.0), 2).samples),
            Take::new("speech_tsp_ma01", speech_like(9 * FS, Voice::LOW, 3).samples),
        ];
        let noises = vec![
            Take::new("noise_keyboard_448209", pink_noise(19 * FS, 0.05, 4)),
            Take::new("noise_cafe_demand_pcafeter_ch01", pink_noise(60 * FS, 0.2, 5)),
            Take::new("music_goldberg_var04", pink_noise(12 * FS, 0.1, 6)),
        ];
        (speech, noises)
    }

    #[test]
    fn labels_and_segments() {
        assert_eq!(noise_label("noise_keyboard_448209"), "keyboard");
        assert_eq!(noise_label("noise_cafe_demand_pcafeter_ch01"), "cafe-demand");
        assert_eq!(noise_label("noise_cafe_370973"), "cafe");
        assert_eq!(noise_label("music_goldberg_var04"), "music");
        assert_eq!(speech_name("speech_vctk_p225"), "vctk_p225");
        let s = |secs: f64| (secs * FS as f64) as usize;
        // 41 s in 15 s segments: 15, 15 and the last 11 (kept); 47.5 s: 15, 15, 15 and 2.5 dropped; 16 s: 15 and 1 dropped
        assert_eq!(segments(s(41.0), s(15.0)), [0..s(15.0), s(15.0)..s(30.0), s(30.0)..s(41.0)]);
        assert_eq!(segments(s(47.5), s(15.0)).len(), 3);
        let one = |len: f64| -> (usize, Range<usize>) {
            let segs = segments(s(len), s(15.0));
            (segs.len(), segs[0].clone())
        };
        assert_eq!(one(16.0), (1, 0..s(15.0)));
        assert_eq!(one(14.5), (1, 0..s(14.5)), "a take a little under the clip length is one segment");
        assert!(segments(s(9.0), s(15.0)).is_empty());
    }

    /// The playlist: every segment with every noise at every SNR, in order, with unique ids and offsets that fit; the same
    /// every time; filters and errors.
    #[test]
    fn the_playlist_is_every_segment_noise_and_snr() {
        let (speech, noises) = set();
        let p = plan(speech.clone(), noises.clone(), Options::default()).unwrap();
        // fa01 (16 s): 1 segment; p225 (41 s): 3; ma01 (9 s): none
        let names: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["tsp_fa01", "vctk_p225#1", "vctk_p225#2", "vctk_p225#3"]);
        assert_eq!(p.labels, ["keyboard", "cafe-demand", "music"]);
        assert_eq!(p.clips.len(), 4 * 3 * 3);
        assert_eq!(p.clips[0].id, "tsp_fa01+keyboard@0");
        assert_eq!(p.clips[1].id, "tsp_fa01+keyboard@10");
        assert_eq!(p.clips.last().unwrap().id, "vctk_p225#3+music@20");
        let mut ids: Vec<&str> = p.clips.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), p.clips.len(), "unique ids");
        for c in &p.clips {
            let (seg, noise) = (&p.segments[c.segment], &p.noises[c.noise]);
            assert!(c.noise_offset + seg.range.len() <= noise.samples.len() || c.noise_offset == 0, "{c:?}");
            assert_eq!(c.noise_offset % 4_800, 0, "steps of 0.1 s");
        }
        // The cafe (60 s) is read from different places; music (12 s) is looped from its start under a longer segment
        let offsets: Vec<usize> = p.clips.iter().filter(|c| c.noise == 1).map(|c| c.noise_offset).collect();
        let mut distinct = offsets.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() > offsets.len() / 2, "{offsets:?}");
        let music = p.noises[2].samples.len();
        let longer = |c: &&Clip| c.noise == 2 && p.segments[c.segment].range.len() >= music;
        assert!(p.clips.iter().filter(longer).count() == 9 && p.clips.iter().filter(longer).all(|c| c.noise_offset == 0));
        assert_eq!(plan(speech.clone(), noises.clone(), Options::default()).unwrap(), p, "deterministic");
        assert_eq!(
            p.description(),
            "36 clips: 4 speech segments (TSP 1, VCTK 3) x 3 noises (keyboard, cafe-demand, music) x SNRs 0, 10, 20 dB, 11.0-15.0 s each"
        );
        // Filters
        let o = Options { noises: vec!["music".into()], speech: vec!["vctk_p225".into()], snrs_db: vec![5.0], ..Options::default() };
        let f = plan(speech.clone(), noises.clone(), o).unwrap();
        assert_eq!(f.clips.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), [
            "vctk_p225#1+music@5", "vctk_p225#2+music@5", "vctk_p225#3+music@5"
        ]);
        let bad = Options { noises: vec!["traffic".into()], ..Options::default() };
        assert!(plan(speech.clone(), noises.clone(), bad).unwrap_err().contains("no noise labelled 'traffic'"));
        let short = vec![Take::new("speech_tsp_ma01", vec![0.1; 9 * FS])];
        assert!(plan(short, noises, Options::default()).unwrap_err().contains("10 s or more"));
    }

    /// A clip is quality's noise scene: the speech at -20 dBFS active level, the noise the SNR below it, the input their sum;
    /// the noise is the take's from the clip's offset; the clean reference is the scene's clean speech.
    #[test]
    fn a_clip_is_quality_s_noise_scene_at_its_snr() {
        let (speech, noises) = set();
        let p = plan(speech, noises, Options::default()).unwrap();
        for c in [&p.clips[1], &p.clips[3 * 3 + 3 + 2]] {
            let s = scene(&p, c).unwrap();
            let len = p.segments[c.segment].range.len();
            assert_eq!((s.input.len(), s.clean.len(), s.noise.len()), (len, len, len));
            let level = db(rms_over(&s.clean, &s.near_activity));
            assert!((level - signals::SPEECH_DBFS).abs() < 0.01, "{}: {level}", c.id);
            assert!((db(rms(&s.noise)) - (signals::SPEECH_DBFS - c.snr_db)).abs() < 0.01, "{}", c.id);
            assert!(s.input.iter().zip(s.clean.iter().zip(&s.noise)).all(|(i, (a, b))| (i - (a + b)).abs() < 1e-6));
            // The noise is the take's own from the offset, scaled
            let raw = &p.noises[c.noise].samples;
            let g = s.noise[100] / raw[c.noise_offset + 100];
            assert!((s.noise[5000] - g * raw[c.noise_offset + 5000]).abs() < 1e-6, "{}", c.id);
            assert_eq!(p.clean(c.segment).unwrap(), s.clean, "{}: the clean reference", c.id);
        }
    }
}
