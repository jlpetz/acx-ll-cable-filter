//! The Lab's results: the metrics, each clip's scores per signal, the summary (means, and paired differences from the
//! baseline with their 95% CIs, overall, per noise and per SNR), the text scorecard, `clips.csv` and `lab.json`.
//!
//! **Signals:** each clip is scored as `clean` (the segment's speech alone, the same for all its clips: context, not
//! compared), `noisy` (the input) and each model's output, aligned to the input (its latency dropped).
//!
//! **The intervals** are cluster-robust by speaker and noise file (`stats`): a clip's speaker is its segment's speech take.
//!
//! **The pause figures** (noise in pauses, and 200 ms clear) leave out the clips where the speech recording's own sound in the
//! pauses (its room's noise, its reverberation) is within `quality`'s 25 dB of the added noise there (VCTK's at 10 and 20 dB,
//! mostly): there the figure would count some of the speech as noise left, and pull every model toward 0 dB. `clips.csv`
//! keeps them, with that level (`speech_in_pauses_db`), and the scorecard counts them.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::playlist::{Playlist, snr_text};
use super::stats::{self, Interval, Key};
use crate::quality::analysis::PAUSE_SPEECH_WARN_DB;

/// Which way a metric is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Better {
    Higher,
    Lower,
    /// Nearer zero (the speech's level change).
    Zero,
}

/// One metric.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricDef {
    /// The CSV and JSON key.
    pub key: &'static str,
    /// The scorecard's label.
    pub label: &'static str,
    pub better: Better,
    /// Shown per noise type in the scorecard too.
    pub headline: bool,
    /// Needs ONNX Runtime (a MOS model), or is a level measure (any build).
    pub mos: bool,
}

const fn m(key: &'static str, label: &'static str, better: Better, headline: bool, mos: bool) -> MetricDef {
    MetricDef { key, label, better, headline, mos }
}

/// The metrics, in the order of the scores' arrays.
#[rustfmt::skip]
pub const METRICS: [MetricDef; 13] = [
    m("dnsmos_sig", "DNSMOS SIG", Better::Higher, false, true),
    m("dnsmos_bak", "DNSMOS BAK", Better::Higher, false, true),
    m("dnsmos_ovrl", "DNSMOS OVRL", Better::Higher, true, true),
    m("sigmos_col", "SIGMOS COL", Better::Higher, false, true),
    m("sigmos_disc", "SIGMOS DISC", Better::Higher, false, true),
    m("sigmos_loud", "SIGMOS LOUD", Better::Higher, false, true),
    m("sigmos_noise", "SIGMOS NOISE", Better::Higher, false, true),
    m("sigmos_reverb", "SIGMOS REVERB", Better::Higher, false, true),
    m("sigmos_sig", "SIGMOS SIG", Better::Higher, false, true),
    m("sigmos_ovrl", "SIGMOS OVRL", Better::Higher, true, true),
    m("pauses_db", "noise in pauses, dB", Better::Lower, true, false),
    m("steady_pauses_db", "  200 ms clear, dB", Better::Lower, false, false),
    m("speech_db", "speech kept, dB", Better::Zero, true, false),
];
pub const N_METRICS: usize = METRICS.len();
/// Where DNSMOS's and SIGMOS's scores start in a [`Scores`] array, and the level measures.
pub const DNSMOS_AT: usize = 0;
pub const SIGMOS_AT: usize = 3;
pub const LEVELS_AT: usize = 10;
/// The pause figures, left out where the speech's own sound is too near the noise in the pauses ([`ClipScores::pauses_counted`]).
pub const PAUSES: [usize; 2] = [LEVELS_AT, LEVELS_AT + 1];

/// A signal's scores, in [`METRICS`]' order; NaN where not measured.
pub type Scores = [f64; N_METRICS];

/// No scores yet.
pub const NONE: Scores = [f64::NAN; N_METRICS];

/// The signals every clip is scored as: `clean`, `noisy`, then the models.
pub fn signal_names(models: &[String]) -> Vec<String> {
    ["clean".to_owned(), "noisy".to_owned()].into_iter().chain(models.iter().cloned()).collect()
}

/// The index of the clean and noisy signals.
pub const CLEAN: usize = 0;
pub const NOISY: usize = 1;

/// One clip's scores, a row per signal, and what decides how far they count.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipScores {
    pub signals: Vec<Scores>,
    /// The clean speech's own level in the pauses against the noise's there, in dB (`analysis::speech_left_in_pauses_db`);
    /// NaN where the level measures didn't run.
    pub speech_in_pauses_db: f64,
    /// The 9.01 s windows DNSMOS scored in each of the clip's signals (all the clip's length), if it ran.
    pub dnsmos_windows: Option<usize>,
}

impl ClipScores {
    /// No scores yet, for `signals` signals.
    pub fn empty(signals: usize) -> Self {
        Self { signals: vec![NONE; signals], speech_in_pauses_db: f64::NAN, dnsmos_windows: None }
    }

    /// Whether the pause figures count: the speech's own sound in the pauses at least [`PAUSE_SPEECH_WARN_DB`] under the noise
    /// there (or not measured, when they weren't either).
    pub fn pauses_counted(&self) -> bool {
        self.speech_in_pauses_db.is_nan() || self.speech_in_pauses_db <= -f64::from(PAUSE_SPEECH_WARN_DB)
    }

    /// A metric's value for a signal as the summary counts it: NaN for a pause figure the clip's speech spoils.
    pub fn counted(&self, signal: usize, metric: usize) -> f64 {
        if PAUSES.contains(&metric) && !self.pauses_counted() { f64::NAN } else { self.signals[signal][metric] }
    }
}

/// Each clip's speaker (its segment's speech take) and noise file: the summary's clusters.
pub fn keys(p: &Playlist) -> Vec<Key> {
    p.clips.iter().map(|c| (p.segments[c.segment].take, c.noise)).collect()
}

/// One metric for one signal over a set of clips: the mean, and the paired difference from the baseline (none for the
/// baseline itself and the clean reference).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub mean: f64,
    pub n: usize,
    pub diff: Option<Interval>,
}

/// The summary over a set of clips: `cells[metric][signal]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub clips: usize,
    pub cells: Vec<Vec<Cell>>,
}

/// The summary over the clips `which` keeps, against the signal `baseline`, the intervals clustered by `keys` (each clip's
/// speaker and noise: [`keys`]).
pub fn summarize(scores: &[ClipScores], keys: &[Key], which: &dyn Fn(usize) -> bool, baseline: usize) -> Table {
    let kept: Vec<usize> = (0..scores.len()).filter(|&i| which(i)).collect();
    let signals = kept.first().map_or(0, |&c| scores[c].signals.len());
    let keys: Vec<Key> = kept.iter().map(|&i| keys[i]).collect();
    let cells = (0..N_METRICS)
        .map(|m| {
            let column = |s: usize| -> Vec<f64> { kept.iter().map(|&c| scores[c].counted(s, m)).collect() };
            let base = column(baseline);
            (0..signals)
                .map(|s| {
                    let x = column(s);
                    let (mean, n) = stats::mean(&x);
                    let diff = (s != baseline && s != CLEAN).then(|| stats::paired(&x, &base, &keys)).flatten();
                    Cell { mean, n, diff }
                })
                .collect()
        })
        .collect();
    Table { clips: kept.len(), cells }
}

/// What a run was: for the scorecard's head and the JSON's `about`.
#[derive(Clone, Debug, PartialEq)]
pub struct About {
    pub models: Vec<String>,
    pub baseline: usize,
    /// The chain's latency per model (samples), dropped from its output.
    pub latencies: Vec<u32>,
    /// Which metric families ran: DNSMOS, SIGMOS, the level measures; and why the MOS ones didn't, if not.
    pub dnsmos: bool,
    pub sigmos: bool,
    pub levels: bool,
    pub mos_note: Option<String>,
    pub seconds: f64,
    pub warnings: Vec<String>,
}

/// A mean as the scorecard shows it: two decimals (no `-0.00`), `--` for none.
fn num(v: f64) -> String {
    if !v.is_finite() {
        "--".into()
    } else if v.abs() < 0.005 {
        "0.00".into()
    } else {
        format!("{v:.2}")
    }
}

/// The scorecard's first column.
const LABEL_WIDTH: usize = 30;

/// A paired difference: `+0.15 [+0.12, +0.18]`, or `no difference [-0.02, +0.03]` when its CI holds 0.
pub fn diff_text(d: &Option<Interval>) -> String {
    match d {
        Some(i) if i.spans_zero() => format!("no difference [{:+.2}, {:+.2}]", i.lo, i.hi),
        Some(i) => format!("{:+.2} [{:+.2}, {:+.2}]", i.mean, i.lo, i.hi),
        None => "--".into(),
    }
}

/// The metrics that ran.
fn shown(about: &About) -> Vec<usize> {
    (0..N_METRICS)
        .filter(|&m| match m {
            m if (DNSMOS_AT..SIGMOS_AT).contains(&m) => about.dnsmos,
            m if m < LEVELS_AT => about.sigmos,
            _ => about.levels,
        })
        .collect()
}

/// How many speakers and noise files the playlist's clips come from.
fn cluster_counts(p: &Playlist) -> (usize, usize) {
    let distinct = |v: Vec<usize>| v.into_iter().collect::<std::collections::BTreeSet<_>>().len();
    (distinct(p.clips.iter().map(|c| p.segments[c.segment].take).collect()), distinct(p.clips.iter().map(|c| c.noise).collect()))
}

/// What the intervals are clustered by, for the scorecard's head (as `stats::mean_ci` decides it).
fn ci_unit(speakers: usize, noises: usize) -> String {
    match (speakers >= 2, noises >= 2) {
        (true, true) => format!(
            "cluster-robust by speaker ({speakers}) and noise file ({noises}): two-way in the overall table (df {}), by speaker \
             within a noise (df {})",
            speakers.min(noises) - 1,
            speakers - 1
        ),
        (true, false) => format!("cluster-robust by speaker ({speakers}; df {})", speakers - 1),
        (false, true) => format!("cluster-robust by noise file ({noises}; df {}; one speaker, so none within a noise)", noises - 1),
        (false, false) => "but one speaker and one noise give none".into(),
    }
}

/// The scorecard: what ran, the overall table, then the headline metrics per noise type, then notes and warnings.
pub fn scorecard(p: &Playlist, scores: &[ClipScores], about: &About) -> String {
    let names = signal_names(&about.models);
    let base = &names[about.baseline];
    let keys = keys(p);
    let (speakers, noises) = cluster_counts(p);
    let mut s = String::new();
    let lat: Vec<String> = about.models.iter().zip(&about.latencies).map(|(m, l)| format!("{m} {:.1} ms", f64::from(*l) / 48.0)).collect();
    let _ = writeln!(s, "noise-filter lab: {}; {:.0} s.", p.description(), about.seconds);
    let _ = writeln!(s, "Models (latency dropped from their output): {}; baseline {base}.", lat.join(", "));
    let _ = writeln!(
        s,
        "Means over the clips; then each signal's paired difference from {base}: its mean and 95% CI by Student's t, {}; \
         \"no difference\" where it holds 0.",
        ci_unit(speakers, noises)
    );
    let others: Vec<usize> = (0..names.len()).filter(|&i| i != about.baseline && i != CLEAN).collect();
    let all = summarize(scores, &keys, &|_| true, about.baseline);
    let table = |s: &mut String, rows: &[(String, Table)], m: usize| {
        for (label, t) in rows {
            let _ = write!(s, "{label:<LABEL_WIDTH$}");
            for c in &t.cells[m] {
                let _ = write!(s, " {:>8}", num(c.mean));
            }
            for &o in &others {
                let _ = write!(s, "   {:<30}", diff_text(&t.cells[m][o].diff));
            }
            // The clips counted, where not all of the table's (the pause figures), or few
            let counted = t.cells[m].iter().map(|c| c.n).max().unwrap_or(0);
            let few = match t.clips {
                n if counted < n => format!("  ({counted} of {n} clips)"),
                1 => "  (1 clip)".to_owned(),
                n if n < 10 => format!("  ({n} clips)"),
                _ => String::new(),
            };
            let _ = writeln!(s, "{few}");
            trim_line(s);
        }
    };
    let header = |s: &mut String, first: &str| {
        let _ = write!(s, "{first:<LABEL_WIDTH$}");
        for n in &names {
            let _ = write!(s, " {n:>8}");
        }
        for &o in &others {
            let _ = write!(s, "   {:<30}", format!("{} - {base}", names[o]));
        }
        let _ = writeln!(s);
        trim_line(s);
    };
    let _ = writeln!(s);
    header(&mut s, &format!("all {} clips", all.clips));
    for m in shown(about) {
        table(&mut s, &[(METRICS[m].label.to_owned(), all.clone())], m);
    }
    for m in shown(about).into_iter().filter(|&m| METRICS[m].headline) {
        let _ = writeln!(s);
        header(&mut s, &format!("{} by noise", METRICS[m].label));
        let rows: Vec<(String, Table)> = p
            .labels
            .iter()
            .enumerate()
            .map(|(n, l)| (format!("  {l}"), summarize(scores, &keys, &|i| p.clips[i].noise == n, about.baseline)))
            .collect();
        table(&mut s, &rows, m);
    }
    let _ = writeln!(s);
    for n in notes(about).into_iter().chain(pause_note(p, scores)) {
        let _ = writeln!(s, "note: {n}");
    }
    for w in &about.warnings {
        let _ = writeln!(s, "warning: {w}");
    }
    s
}

/// Drops the spaces before the last line's newline (the columns' padding).
fn trim_line(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
        let keep = s.trim_end_matches(' ').len();
        s.truncate(keep);
        s.push('\n');
    }
}

/// The note on the clips the pause figures leave out ([`ClipScores::pauses_counted`]), with how many at each SNR.
fn pause_note(p: &Playlist, scores: &[ClipScores]) -> Option<String> {
    let out: Vec<usize> = (0..scores.len()).filter(|&i| !scores[i].pauses_counted()).collect();
    if out.is_empty() {
        return None;
    }
    let by_snr: Vec<String> = p
        .options
        .snrs_db
        .iter()
        .map(|&d| (d, out.iter().filter(|&&i| p.clips[i].snr_db == d).count()))
        .filter(|&(_, n)| n > 0)
        .map(|(d, n)| format!("{n} at {} dB", snr_text(d)))
        .collect();
    let nearest = out.iter().map(|&i| scores[i].speech_in_pauses_db).fold(f64::NEG_INFINITY, f64::max);
    let nearest = if nearest < 0.0 { format!("{:.1} dB under it", -nearest) } else { format!("{nearest:.1} dB over it") };
    Some(format!(
        "Noise in pauses (and 200 ms clear) leaves out {} of the {} clips ({}): there the speech recording's own sound in the \
         pauses (its room's noise, its reverberation) is within {PAUSE_SPEECH_WARN_DB:.0} dB of the added noise (at the nearest, \
         {nearest}), so the figure would count it as noise left. clips.csv keeps their figures and that level \
         (speech_in_pauses_db).",
        out.len(),
        scores.len(),
        by_snr.join(", "),
    ))
}

/// What the scorecard's figures are, and aren't.
fn notes(about: &About) -> Vec<String> {
    let mut n = Vec::new();
    if let Some(why) = &about.mos_note {
        n.push(why.clone());
    }
    if about.dnsmos {
        n.push(
            "DNSMOS P.835 (DNS-Challenge 591184a) scores 16 kHz audio (each clip resampled from 48 kHz as its script's librosa \
             0.8.1 and resampy 0.4 do it): it's deaf above 8 kHz. It was trained on the output of about 40 DNS Challenge suppressors, so a \
             DNS-trained model may score well on it without sounding better."
                .into(),
        );
    }
    if about.sigmos {
        n.push(
            "SIGMOS v1 (SIG-Challenge bf45251) sees the full band, but upstream calls it an alpha with no published accuracy: \
                treat its scores as uncalibrated until checked against our own blind ratings."
                .into(),
        );
    }
    if about.levels {
        n.push(
            "Noise in pauses is the output's level change from the input there (lower: more noise cut); speech kept is the change \
             of the output's projection on the clean speech in its syllables' cores (0 dB: all of it). Both skip each clip's first \
             second, as quality's noise test does."
                .into(),
        );
    }
    n.push(
        "Only the means are reliable: DNSMOS agrees with listeners per system (0.94-0.98), far less per clip (0.71-0.83). The \
         CIs allow for clips sharing a speaker or a noise file, so they speak for other speakers and noises like these; with \
         this few of each they're wide and only approximate (a little narrow, if anything). A listening test has the final say."
            .into(),
    );
    n
}

/// `clips.csv`: a row per clip and signal, every metric (empty where not measured).
pub fn csv(p: &Playlist, scores: &[ClipScores], models: &[String]) -> String {
    let names = signal_names(models);
    let mut s = String::from("clip,segment,noise,snr_db,seconds,speech_in_pauses_db,dnsmos_windows,signal");
    for m in &METRICS {
        let _ = write!(s, ",{}", m.key);
    }
    s.push('\n');
    for (c, sc) in p.clips.iter().zip(scores) {
        let seg = &p.segments[c.segment];
        for (name, row) in names.iter().zip(&sc.signals) {
            // -inf: the speech is digitally silent in the pauses; empty: not measured
            let floor = match sc.speech_in_pauses_db {
                v if v.is_finite() => format!("{v:.2}"),
                f64::NEG_INFINITY => "-inf".into(),
                _ => String::new(),
            };
            let windows = sc.dnsmos_windows.map_or(String::new(), |w| w.to_string());
            let _ = write!(
                s,
                "{},{},{},{},{:.3},{floor},{windows},{name}",
                c.id,
                seg.name,
                p.labels[c.noise],
                snr_text(c.snr_db),
                seg.range.len() as f64 / 48_000.0
            );
            for v in row {
                if v.is_finite() {
                    let _ = write!(s, ",{v:.5}");
                } else {
                    s.push(',');
                }
            }
            s.push('\n');
        }
    }
    s
}

fn interval_json(i: &Option<Interval>) -> Value {
    match i {
        Some(i) => json!({"mean": i.mean, "lo": i.lo, "hi": i.hi, "n": i.n, "clusters": i.clusters, "no_difference": i.spans_zero()}),
        None => Value::Null,
    }
}

fn finite(v: f64) -> Value {
    if v.is_finite() { json!(v) } else { Value::Null }
}

fn table_json(t: &Table, names: &[String], metrics: &[usize]) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("clips".into(), json!(t.clips));
    for &m in metrics {
        let mut per = serde_json::Map::new();
        for (s, name) in names.iter().enumerate() {
            let c = &t.cells[m][s];
            per.insert(name.clone(), json!({"mean": finite(c.mean), "n": c.n, "diff": interval_json(&c.diff)}));
        }
        out.insert(METRICS[m].key.into(), Value::Object(per));
    }
    Value::Object(out)
}

/// `lab.json`: what ran, every clip's scores, and the summary overall, per noise and per SNR.
pub fn json(p: &Playlist, scores: &[ClipScores], about: &About) -> Value {
    let names = signal_names(&about.models);
    let metrics = shown(about);
    let keys = keys(p);
    let (speakers, noises) = cluster_counts(p);
    let clips: Vec<Value> = p
        .clips
        .iter()
        .zip(scores)
        .map(|(c, sc)| {
            let mut per = serde_json::Map::new();
            for (name, row) in names.iter().zip(&sc.signals) {
                let vals: serde_json::Map<String, Value> = metrics.iter().map(|&m| (METRICS[m].key.to_owned(), finite(row[m]))).collect();
                per.insert(name.clone(), Value::Object(vals));
            }
            let seg = &p.segments[c.segment];
            json!({
                "id": c.id, "segment": seg.name, "speech": p.speech[seg.take].name, "start_s": seg.range.start as f64 / 48_000.0,
                "seconds": seg.range.len() as f64 / 48_000.0, "noise": p.labels[c.noise], "noise_file": p.noises[c.noise].name,
                "noise_offset_s": c.noise_offset as f64 / 48_000.0, "snr_db": c.snr_db,
                "speech_in_pauses_db": finite(sc.speech_in_pauses_db), "pauses_counted": sc.pauses_counted(),
                "dnsmos_windows": sc.dnsmos_windows, "scores": per,
            })
        })
        .collect();
    let by = |keep: &dyn Fn(usize) -> bool| table_json(&summarize(scores, &keys, keep, about.baseline), &names, &metrics);
    let by_noise: serde_json::Map<String, Value> =
        p.labels.iter().enumerate().map(|(n, l)| (l.clone(), by(&|i| p.clips[i].noise == n))).collect();
    let by_snr: serde_json::Map<String, Value> =
        p.options.snrs_db.iter().map(|&d| (snr_text(d), by(&|i| p.clips[i].snr_db == d))).collect();
    let takes: Vec<Value> =
        p.speech.iter().chain(&p.noises).map(|t| json!({"take": t.name, "licence": t.licence, "attribution": t.attribution})).collect();
    json!({
        "about": {
            "tool": format!("noise-filter lab {}", env!("CARGO_PKG_VERSION")),
            "playlist": p.description(),
            "signals": names,
            "models": about.models,
            "latency_samples": about.latencies,
            "baseline": names[about.baseline],
            "metrics": metrics.iter().map(|&m| json!({"key": METRICS[m].key, "label": METRICS[m].label.trim(), "better": format!("{:?}", METRICS[m].better).to_lowercase()})).collect::<Vec<_>>(),
            "statistics": format!(
                "means over clips; paired differences from the baseline with 95% CIs by Student's t, cluster-robust (CR1) by speaker \
                 ({speakers}) and noise file ({noises}), two-way (Cameron-Gelbach-Miller) where both vary, df the fewer clusters \
                 less 1; an interval's n is its clips, its clusters the fewer factor's count"
            ),
            "pauses": format!(
                "pauses_db and steady_pauses_db leave out of the summaries the clips whose speech_in_pauses_db is over -{PAUSE_SPEECH_WARN_DB:.0} dB"
            ),
            "dnsmos": about.dnsmos.then_some("DNSMOS P.835 sig_bak_ovr.onnx, DNS-Challenge 591184a; 16 kHz as librosa 0.8.1 (its requirements.txt) resamples, through resampy 0.3-0.4.3's kaiser_best, padded to ceil(n / 3)"),
            "sigmos": about.sigmos.then_some("SIGMOS v1 (alpha), SIG-Challenge bf45251; 48 kHz"),
            "notes": notes(about).into_iter().chain(pause_note(p, scores)).collect::<Vec<_>>(),
            "warnings": about.warnings,
            "seconds": about.seconds,
            "takes": takes,
            "local_only": "the clips are built from the local test-audio cache and never shared; DEMAND's (CC BY-SA 3.0) mixes would stay BY-SA",
        },
        "summary": {"all": by(&|_| true), "by_noise": by_noise, "by_snr": by_snr},
        "clips": clips,
    })
}

#[cfg(test)]
mod tests {
    use nf_stages::synth::pink_noise;

    use super::super::playlist::{Options, Take, plan};
    use super::*;

    /// A playlist of 2 segments (2 speakers) x 2 noises x 2 SNRs and made-up scores: model B beats model A by 0.5 on DNSMOS
    /// OVRL on every clip, and ties it (±0.01 by speaker) on noise in pauses; the first clip's speech is loud in its pauses.
    fn fixture() -> (Playlist, Vec<ClipScores>, About) {
        let speech = vec![Take::new("speech_tsp_fa01", vec![0.1; 15 * 48_000]), Take::new("speech_tsp_mb07", vec![0.1; 12 * 48_000])];
        let noises = vec![
            Take::new("noise_fan_242042", pink_noise(20 * 48_000, 0.1, 1)),
            Take::new("noise_dogs_337101", pink_noise(20 * 48_000, 0.1, 2)),
        ];
        let p = plan(speech, noises, Options { snrs_db: vec![5.0, 15.0], ..Options::default() }).unwrap();
        let scores = (0..p.clips.len())
            .map(|i| {
                let base = 2.0 + 0.1 * i as f64;
                let mut rows = vec![NONE; 4];
                rows[CLEAN][2] = 4.0;
                rows[NOISY][2] = base - 1.0;
                rows[2][2] = base;
                rows[3][2] = base + 0.5;
                rows[NOISY][LEVELS_AT] = 0.0;
                rows[2][LEVELS_AT] = -20.0 - i as f64;
                rows[3][LEVELS_AT] = -20.0 - i as f64 + if p.clips[i].segment == 0 { 0.01 } else { -0.01 };
                ClipScores { signals: rows, speech_in_pauses_db: if i == 0 { -12.5 } else { -40.0 }, dnsmos_windows: Some(6) }
            })
            .collect();
        let about = About {
            models: vec!["a".into(), "b".into()],
            baseline: 2,
            latencies: vec![480, 2400],
            dnsmos: true,
            sigmos: false,
            levels: true,
            mos_note: None,
            seconds: 12.0,
            warnings: vec![],
        };
        (p, scores, about)
    }

    #[test]
    fn the_summary_pairs_each_signal_with_the_baseline() {
        let (p, scores, about) = fixture();
        let keys = keys(&p);
        assert_eq!(keys[..4], [(0, 0), (0, 0), (0, 1), (0, 1)], "speaker (take) and noise");
        let t = summarize(&scores, &keys, &|_| true, about.baseline);
        assert_eq!(t.clips, 8);
        let ovrl = &t.cells[2];
        assert_eq!((ovrl[CLEAN].mean, ovrl[CLEAN].diff), (4.0, None), "the clean reference isn't compared");
        assert_eq!(ovrl[2].diff, None, "the baseline isn't compared with itself");
        let b = ovrl[3].diff.unwrap();
        assert!((b.mean - 0.5).abs() < 1e-12 && (b.hi - b.lo) < 1e-9, "the same +0.5 on every clip: {b:?}");
        let noisy = ovrl[NOISY].diff.unwrap();
        assert!((noisy.mean + 1.0).abs() < 1e-12 && !noisy.spans_zero());
        let pauses = t.cells[LEVELS_AT][3].diff.unwrap();
        assert!(pauses.spans_zero(), "a tie: {pauses:?}");
        // The first clip's pause figures don't count (its speech 12.5 dB under the noise there); its OVRL does
        assert_eq!((t.cells[LEVELS_AT][2].n, pauses.n, b.n), (7, 7, 8));
        assert!((t.cells[LEVELS_AT][2].mean - (-20.0 - 28.0 / 7.0)).abs() < 1e-12, "clips 1 to 7: {:?}", t.cells[LEVELS_AT][2]);
        // Two speakers and two noises: two-way, two clusters
        assert_eq!((b.clusters, pauses.clusters), (2, 2));
        assert!(t.cells[SIGMOS_AT][2].mean.is_nan() && t.cells[SIGMOS_AT][3].diff.is_none(), "not measured");
        // Per noise: half the clips each
        let fan = summarize(&scores, &keys, &|i| p.clips[i].noise == 0, about.baseline);
        assert_eq!(fan.clips, 4);
        assert_eq!(diff_text(&Some(b)), "+0.50 [+0.50, +0.50]");
        assert!(diff_text(&Some(pauses)).starts_with("no difference ["));
        assert_eq!(diff_text(&None), "--");
    }

    #[test]
    fn the_scorecard_csv_and_json_hold_every_clip_and_the_summary() {
        let (p, scores, about) = fixture();
        let card = scorecard(&p, &scores, &about);
        assert!(card.contains("8 clips: 2 speech segments (TSP 2) x 2 noises (fan, dogs) x SNRs 5, 15 dB"), "{card}");
        assert!(card.contains("baseline a.") && card.contains("b - a") && card.contains("noisy - a"), "{card}");
        let ovrl = card.lines().find(|l| l.starts_with("DNSMOS OVRL")).unwrap();
        assert!(ovrl.contains("+0.50 [+0.50, +0.50]") && ovrl.contains("-1.00 [-1.00, -1.00]"), "{ovrl}");
        let pauses = card.lines().find(|l| l.starts_with("noise in pauses, dB")).unwrap();
        assert!(pauses.contains("no difference") && pauses.ends_with("(7 of 8 clips)"), "{pauses}");
        assert!(card.contains("cluster-robust by speaker (2) and noise file (2): two-way in the overall table (df 1)"), "{card}");
        assert!(!card.contains("SIGMOS"), "SIGMOS didn't run: {card}");
        assert!(card.contains("DNSMOS OVRL by noise") && card.contains("  fan") && card.contains("(4 clips)"), "{card}");
        assert!(card.contains("deaf above 8 kHz") && card.contains("allow for clips sharing a speaker or a noise file"), "{card}");
        assert!(
            card.contains("note: Noise in pauses (and 200 ms clear) leaves out 1 of the 8 clips (1 at 5 dB): there the speech")
                && card.contains("(at the nearest, 12.5 dB under it)"),
            "{card}"
        );
        let csv = csv(&p, &scores, &about.models);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 1 + 8 * 4, "a row per clip and signal");
        assert!(lines[0].starts_with("clip,segment,noise,snr_db,seconds,speech_in_pauses_db,dnsmos_windows,signal,dnsmos_sig,"));
        assert!(lines[1].starts_with("tsp_fa01+fan@5,tsp_fa01,fan,5,15.000,-12.50,6,clean,,,4.00000,"), "{}", lines[1]);
        assert!(lines[3].contains(",-12.50,6,a,") && lines[3].contains(",-20.00000,"), "the CSV keeps the figure: {}", lines[3]);
        let j = json(&p, &scores, &about);
        assert_eq!(j["clips"].as_array().unwrap().len(), 8);
        assert_eq!(j["clips"][0]["scores"]["b"]["dnsmos_ovrl"], json!(2.5));
        assert!(j["clips"][0]["scores"]["b"].get("sigmos_ovrl").is_none(), "only the metrics that ran");
        assert_eq!(j["summary"]["all"]["dnsmos_ovrl"]["b"]["diff"]["mean"].as_f64().map(|v| (v * 1e9).round() / 1e9), Some(0.5));
        assert_eq!(j["summary"]["by_noise"]["dogs"]["clips"], json!(4));
        assert_eq!(j["summary"]["by_snr"]["15"]["clips"], json!(4));
        assert_eq!(j["about"]["baseline"], json!("a"));
        assert_eq!(j["summary"]["all"]["pauses_db"]["b"]["diff"]["no_difference"], json!(true));
        assert_eq!(j["summary"]["all"]["pauses_db"]["b"]["diff"]["n"], json!(7));
        assert_eq!(j["summary"]["all"]["dnsmos_ovrl"]["b"]["diff"]["clusters"], json!(2));
        assert_eq!((&j["clips"][0]["pauses_counted"], &j["clips"][1]["pauses_counted"]), (&json!(false), &json!(true)));
        assert_eq!((&j["clips"][0]["speech_in_pauses_db"], &j["clips"][0]["dnsmos_windows"]), (&json!(-12.5), &json!(6)));
    }
}
