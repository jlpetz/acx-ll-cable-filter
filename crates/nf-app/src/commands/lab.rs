//! `lab`: TODO 14's offline batch (the Lab mode's phase 0). Builds the playlist from the fetched test audio (`lab::playlist`),
//! runs every clip through every chosen noise model offline, the chain as the engine builds it with the model inline
//! (`nf_engine::models::build_chain`, as `process` does; a fresh chain a clip, its latency dropped), and scores the clean
//! speech, the noisy input and each output: DNSMOS P.835 and SIGMOS (nf-ort's metric runners; a build with the `onnx`
//! feature), and `quality`'s level measures (noise in pauses, speech kept), which need the clean reference, with the clean
//! speech's own level in the pauses (`quality`'s check: where it's within 25 dB of the noise the summary leaves the clip's pause
//! figures out). Then the scorecard (`lab::report`): means, paired differences from the baseline with 95% CIs clustered by
//! speaker and noise, overall and per noise type; with `--out DIR`, `scorecard.txt`, `clips.csv` (a row per clip and signal)
//! and `lab.json` (the clips and the summaries).
//!
//! With DNSMOS, clips are 16 s at most ([`DNSMOS_MAX_CLIP_S`]): its script's window arithmetic, which we keep, skips a longer
//! clip's windows from the eighth (the one ending at 16.01 s) on.
//!
//! Clips run on `--jobs` worker threads (each with its own metric sessions and chains); every clip is independent and every
//! step deterministic, so the results don't depend on the job count. No devices, nothing real-time: everything here
//! allocates freely. A build without the `onnx` feature runs the level measures and RNNoise alone, and says so.

use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nf_core::controls::ChainControls;
use nf_engine::ProcessingConfig;
use nf_engine::models::{ModelThread, build_chain};
use nf_stages::harness::run_chain;

use crate::cli::{LabArgs, LabMetric};
use crate::exit::Failure;
use crate::lab::playlist::{self, Playlist};
use crate::lab::report::{self, About, CLEAN, ClipScores, LEVELS_AT, NOISY, NONE, Scores};
use crate::quality::analysis::{MIN_REGION_S, Metrics, noise_metrics, speech_left_in_pauses_db};
use crate::quality::signals::Scene;

/// The longest clip DNSMOS scores whole: in `dnsmos_local.py`'s arithmetic (kept by `nf_ort::metric::Dnsmos::windows`), a clip
/// of 17 s or more loses its windows from the eighth (the one ending at 16.01 s) to the 24th, each a sample short in floating
/// point, so a 30 s clip is scored on its first 15 s. Under 17 s every window counts; 16 is the round number under it.
pub const DNSMOS_MAX_CLIP_S: f32 = 16.0;

pub fn lab(args: &LabArgs) -> Result<(), Failure> {
    let start = Instant::now();
    let dir = args
        .audio
        .clone()
        .or_else(playlist::default_dir)
        .ok_or_else(|| Failure::error("Neither NF_TEST_AUDIO nor LOCALAPPDATA is set: give --audio DIR."))?;
    let (speech, noises) = playlist::read_set(&dir).map_err(Failure::error)?;
    let p = playlist::plan(speech, noises, args.playlist.clone()).map_err(Failure::usage)?;
    say!("Lab: {} (from {}).", p.description(), dir.display());

    let mut cfg = match &args.config {
        Some(path) => crate::config::load(path)?.processing,
        None => ProcessingConfig::default(),
    };
    args.processing.apply(&mut cfg);
    cfg.aec_enabled = false;
    cfg.noise_enabled = true;
    let models = match &args.models {
        Some(m) => m.clone(),
        None => default_models(),
    };
    // Each model's chain once, to check it before the batch and for its latency
    let mut latencies = Vec::with_capacity(models.len());
    for m in &models {
        let chain = model_chain(&cfg, m).map_err(|e| Failure::error(format!("Couldn't build the chain for {m}: {e}")))?;
        latencies.push(chain.latency());
    }
    let names = report::signal_names(&models);
    let baseline = match &args.baseline {
        Some(b) => names
            .iter()
            .position(|n| n == b)
            .filter(|&i| i != CLEAN)
            .ok_or_else(|| Failure::usage(format!("--baseline {b} isn't one of the models ({}) or noisy", models.join(", "))))?,
        None => 2,
    };
    let (dnsmos, sigmos, mos_note) = metric_choice(args.metrics.as_deref())?;
    let levels = args.metrics.as_ref().is_none_or(|m| m.contains(&LabMetric::Levels));
    if dnsmos && p.options.clip_s > DNSMOS_MAX_CLIP_S {
        return Err(Failure::usage(format!(
            "--clip-seconds {}: DNSMOS scores clips of {DNSMOS_MAX_CLIP_S:.0} s at most whole (its script skips a longer clip's \
             windows from the one ending at 16.01 s on): give --clip-seconds {DNSMOS_MAX_CLIP_S:.0} or less, or leave DNSMOS out (--metrics sigmos,levels)",
            p.options.clip_s
        )));
    }
    // Open the metrics once here, so a missing pack fails before the batch
    mos::Scorer::new(dnsmos, sigmos).map_err(Failure::error)?;

    let jobs = args.jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()).min(8)).max(1);
    let total = p.segments.len() + p.clips.len();
    let metrics: Vec<&str> = [dnsmos.then_some("DNSMOS P.835"), sigmos.then_some("SIGMOS"), levels.then_some("the level measures")]
        .into_iter()
        .flatten()
        .collect();
    if metrics.is_empty() {
        return Err(Failure::usage("--metrics leaves nothing this build can run: add levels"));
    }
    let threads = if jobs == 1 { "1 thread".to_owned() } else { format!("{jobs} threads") };
    say!("Scoring each clip as {} with {}, on {threads}...", names.join(", "), metrics.join(", "));
    if let Some(n) = &mos_note {
        say!("note: {n}");
    }
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let clean = Mutex::new(vec![NONE; p.segments.len()]);
    let clips = Mutex::new(vec![ClipScores::empty(0); p.clips.len()]);
    let warnings = Mutex::new(Vec::<String>::new());
    let failed = Mutex::new(None::<String>);
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                let mut scorer = match mos::Scorer::new(dnsmos, sigmos) {
                    Ok(sc) => sc,
                    Err(e) => {
                        failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                        return;
                    }
                };
                loop {
                    let j = next.fetch_add(1, Relaxed);
                    if j >= total || failed.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                        return;
                    }
                    let result = if j < p.segments.len() {
                        score_clean(&p, j, &mut scorer).map(|(sc, w)| {
                            clean.lock().unwrap_or_else(|e| e.into_inner())[j] = sc;
                            w
                        })
                    } else {
                        let c = j - p.segments.len();
                        score_clip(&p, c, &models, &cfg, &mut scorer, levels).map(|(sc, w)| {
                            clips.lock().unwrap_or_else(|e| e.into_inner())[c] = sc;
                            w
                        })
                    };
                    match result {
                        Ok(w) => warnings.lock().unwrap_or_else(|e| e.into_inner()).extend(w),
                        Err(e) => {
                            failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                            return;
                        }
                    }
                    let d = done.fetch_add(1, Relaxed) + 1;
                    if d * 10 / total != (d - 1) * 10 / total {
                        say!("  {d} of {total} ({}%), {:.0} s", d * 100 / total, start.elapsed().as_secs_f64());
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(Failure::error(e));
    }
    let clean = clean.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut scores = clips.into_inner().unwrap_or_else(|e| e.into_inner());
    for (c, sc) in p.clips.iter().zip(scores.iter_mut()) {
        sc.signals[CLEAN] = clean[c.segment];
    }
    let mut warnings = warnings.into_inner().unwrap_or_else(|e| e.into_inner());
    warnings.sort();
    warnings.dedup();
    let about = About { models, baseline, latencies, dnsmos, sigmos, levels, mos_note, seconds: start.elapsed().as_secs_f64(), warnings };
    let card = report::scorecard(&p, &scores, &about);
    say!("\n{card}");
    if let Some(out) = &args.out {
        write_outputs(out, &p, &scores, &about, &card)?;
        say!("Wrote scorecard.txt, clips.csv and lab.json to {}.", out.display());
    }
    Ok(())
}

/// The models a lab compares by default: RNNoise, and with the `onnx` feature every noise model pack installed.
fn default_models() -> Vec<String> {
    let mut m = vec![nf_core::config::DEFAULT_NOISE_MODEL.to_owned()];
    m.extend(mos::installed_noise_packs());
    m
}

/// Which MOS metrics run, and the note when they can't: the asked ones (`--metrics`), or by default all this build has.
fn metric_choice(asked: Option<&[LabMetric]>) -> Result<(bool, bool, Option<String>), Failure> {
    let wants = |m: LabMetric| asked.is_none_or(|a| a.contains(&m));
    let (dnsmos, sigmos) = (wants(LabMetric::Dnsmos), wants(LabMetric::Sigmos));
    if mos::AVAILABLE {
        return Ok((dnsmos, sigmos, None));
    }
    match asked {
        None => Ok((
            false,
            false,
            Some("DNSMOS and SIGMOS didn't run: they need a build with --features onnx (ONNX Runtime); this one has the level measures only.".into()),
        )),
        Some(a) if a.iter().any(|m| *m != LabMetric::Levels) => Err(Failure::usage(
            "--metrics dnsmos and sigmos need a build with --features onnx (ONNX Runtime): this one has the level measures only",
        )),
        Some(_) => Ok((false, false, None)),
    }
}

/// A fresh chain for model `m` (the noise stage alone, the model inline, as `process` builds it).
fn model_chain(cfg: &ProcessingConfig, m: &str) -> Result<nf_core::chain::Chain, String> {
    let cfg = ProcessingConfig { noise_model: m.to_owned(), ..cfg.clone() };
    build_chain(Arc::new(ChainControls::new(&cfg)), &cfg, false, ModelThread::Inline).map(|b| b.chain).map_err(|e| e.0)
}

/// The level measures of `out` (aligned with the scene's input) into `s`.
fn level_measures(scene: &Scene, out: &[f32], s: &mut Scores) {
    if let Metrics::Noise { pauses, steady_pauses, speech } = noise_metrics(scene, out, out.len().min(scene.clean.len())) {
        s[LEVELS_AT] = f64::from(pauses.change_db());
        s[LEVELS_AT + 1] = if steady_pauses.seconds >= MIN_REGION_S { f64::from(steady_pauses.change_db()) } else { f64::NAN };
        s[LEVELS_AT + 2] = f64::from(speech.speech_dbfs - speech.clean_dbfs);
    }
}

/// A segment's clean reference, scored by the MOS metrics.
fn score_clean(p: &Playlist, s: usize, scorer: &mut mos::Scorer) -> Result<(Scores, Vec<String>), String> {
    let x = p.clean(s).map_err(|e| format!("{}: {e}", p.segments[s].name))?;
    let mut sc = NONE;
    let (_, errors) = scorer.score(&x, &mut sc);
    Ok((sc, errors.into_iter().map(|e| format!("{} (clean): {e}", p.segments[s].name)).collect()))
}

/// One clip: the noisy input and each model's output, scored (the level measures if `levels`).
fn score_clip(
    p: &Playlist, c: usize, models: &[String], cfg: &ProcessingConfig, scorer: &mut mos::Scorer, levels: bool,
) -> Result<(ClipScores, Vec<String>), String> {
    let clip = &p.clips[c];
    let scene = playlist::scene(p, clip)?;
    let mut scores = ClipScores::empty(2 + models.len());
    let rows = &mut scores.signals;
    let mut warnings = Vec::new();
    let (windows, errors) = scorer.score(&scene.input, &mut rows[NOISY]);
    scores.dnsmos_windows = windows;
    warnings.extend(errors.into_iter().map(|e| format!("{} (noisy): {e}", clip.id)));
    if levels {
        level_measures(&scene, &scene.input, &mut rows[NOISY]);
        scores.speech_in_pauses_db = f64::from(speech_left_in_pauses_db(&scene, scene.clean.len()));
    }
    for (k, m) in models.iter().enumerate() {
        let mut chain = model_chain(cfg, m)?;
        let lat = chain.latency() as usize;
        let out = run_chain(&mut chain, &scene.input, &[], lat);
        if !out.iter().all(|v| v.is_finite()) {
            return Err(format!("{}: {m}'s output isn't finite", clip.id));
        }
        let (_, errors) = scorer.score(&out, &mut scores.signals[2 + k]);
        warnings.extend(errors.into_iter().map(|e| format!("{} ({m}): {e}", clip.id)));
        if levels {
            level_measures(&scene, &out, &mut scores.signals[2 + k]);
        }
    }
    Ok((scores, warnings))
}

fn write_outputs(dir: &Path, p: &Playlist, scores: &[ClipScores], about: &About, card: &str) -> Result<(), Failure> {
    std::fs::create_dir_all(dir).map_err(|e| Failure::error(format!("Couldn't create {}: {e}", dir.display())))?;
    let json = serde_json::to_string_pretty(&report::json(p, scores, about)).map_err(|e| Failure::error(format!("lab.json: {e}")))?;
    for (name, text) in [("scorecard.txt", card.to_owned()), ("clips.csv", report::csv(p, scores, &about.models)), ("lab.json", json)] {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|e| Failure::error(format!("Couldn't write {}: {e}", path.display())))?;
    }
    Ok(())
}

/// The MOS metrics, through nf-ort (the `onnx` feature).
#[cfg(feature = "onnx")]
mod mos {
    use nf_ort::metric::{Dnsmos, Sigmos};

    use crate::lab::report::{DNSMOS_AT, SIGMOS_AT, Scores};

    pub const AVAILABLE: bool = true;

    /// A worker's metric sessions.
    pub struct Scorer {
        dnsmos: Option<Dnsmos>,
        sigmos: Option<Sigmos>,
    }

    impl Scorer {
        pub fn new(dnsmos: bool, sigmos: bool) -> Result<Self, String> {
            let hint = "python -I tools/fetch_models.py dnsmos sigmos, or --metrics levels";
            let open = |what: &str, e: String| format!("{what}: {e} ({hint})");
            Ok(Self {
                dnsmos: dnsmos.then(Dnsmos::new).transpose().map_err(|e| open("DNSMOS", e))?,
                sigmos: sigmos.then(Sigmos::new).transpose().map_err(|e| open("SIGMOS", e))?,
            })
        }

        /// The clip's DNSMOS and SIGMOS scores into `s` (48 kHz), each metric's on its own: the windows DNSMOS scored, and what
        /// failed (the other metric's scores still come).
        pub fn score(&mut self, x: &[f32], s: &mut Scores) -> (Option<usize>, Vec<String>) {
            let (mut windows, mut errors) = (None, Vec::new());
            if let Some(d) = &mut self.dnsmos {
                match d.score_48k(x) {
                    Ok(r) => {
                        s[DNSMOS_AT..DNSMOS_AT + 3].copy_from_slice(&[r.sig, r.bak, r.ovrl]);
                        windows = Some(r.windows);
                    }
                    Err(e) => errors.push(e),
                }
            }
            if let Some(m) = &mut self.sigmos {
                match m.score(x) {
                    Ok(r) => s[SIGMOS_AT..SIGMOS_AT + 7].copy_from_slice(&r.values()),
                    Err(e) => errors.push(e),
                }
            }
            (windows, errors)
        }
    }

    /// The installed packs that load as noise models (their ids, sorted).
    pub fn installed_noise_packs() -> Vec<String> {
        let Some(dir) = nf_ort::models_dir() else { return Vec::new() };
        let mut ids: Vec<String> =
            std::fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
        ids.sort();
        ids.retain(|id| nf_ort::Pack::installed(id).is_ok());
        ids
    }
}

/// Without ONNX Runtime: no MOS metrics, RNNoise alone.
#[cfg(not(feature = "onnx"))]
mod mos {
    use crate::lab::report::Scores;

    pub const AVAILABLE: bool = false;

    pub struct Scorer;

    impl Scorer {
        pub fn new(_dnsmos: bool, _sigmos: bool) -> Result<Self, String> {
            Ok(Self)
        }

        pub fn score(&mut self, _x: &[f32], _s: &mut Scores) -> (Option<usize>, Vec<String>) {
            (None, Vec::new())
        }
    }

    pub fn installed_noise_packs() -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use nf_stages::synth::{Voice, pink_noise, speech_like};

    use super::*;
    use crate::lab::playlist::Options;

    /// A small synthetic set in a temp folder, written as the fetch tool writes the cache (48 kHz 16-bit mono WAVs).
    fn set(dir: &Path) {
        let write = |name: &str, x: &[f32]| {
            let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(dir.join(format!("{name}.wav")), spec).unwrap();
            for &v in x {
                w.write_sample((v * 32767.0).round() as i16).unwrap();
            }
            w.finalize().unwrap();
        };
        write("speech_tsp_fa01", &speech_like(11 * 48_000, Voice::HIGH.at(-26.0), 1).samples);
        write("noise_fan_242042", &pink_noise(14 * 48_000, 0.1, 2));
        write("noise_keyboard_448209", &pink_noise(5 * 48_000, 0.05, 3));
    }

    fn args(dir: &Path, out: &Path) -> LabArgs {
        LabArgs {
            audio: Some(dir.to_path_buf()),
            out: Some(out.to_path_buf()),
            models: Some(vec!["rnnoise".into()]),
            baseline: Some("noisy".into()),
            metrics: Some(vec![LabMetric::Levels]),
            playlist: Options { snrs_db: vec![10.0], ..Options::default() },
            config: None,
            processing: Default::default(),
            jobs: Some(2),
        }
    }

    /// The whole batch on a synthetic set, levels only: RNNoise cuts the noise in pauses against the noisy input, and every
    /// clip is in the CSV and the JSON.
    #[test]
    fn a_small_lab_runs_end_to_end() {
        let dir = crate::test_dir("lab-set");
        let out = dir.join("out");
        set(&dir);
        lab(&args(&dir, &out)).unwrap();
        let csv = std::fs::read_to_string(out.join("clips.csv")).unwrap();
        assert_eq!(csv.lines().count(), 1 + 2 * 3, "2 clips x clean, noisy and rnnoise");
        let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("lab.json")).unwrap()).unwrap();
        let cut = j["summary"]["all"]["pauses_db"]["rnnoise"]["diff"]["mean"].as_f64().unwrap();
        assert!(cut < -10.0, "RNNoise cuts the noise in pauses: {cut}");
        assert_eq!(j["summary"]["all"]["pauses_db"]["noisy"]["mean"].as_f64(), Some(0.0), "the input against itself");
        assert_eq!(j["about"]["baseline"], "noisy");
        let card = std::fs::read_to_string(out.join("scorecard.txt")).unwrap();
        assert!(card.contains("2 clips: 1 speech segment (TSP 1) x 2 noises") && card.contains("rnnoise - noisy"), "{card}");
        assert!(!card.contains("DNSMOS SIG") && card.contains("noise in pauses, dB"), "levels only: {card}");
        assert!(card.contains("cluster-robust by noise file (2; df 1; one speaker, so none within a noise)"), "{card}");
        // The speech's own level in the pauses, measured on every clip (the synthetic voice has none: far under the noise)
        let row = csv.lines().nth(2).unwrap();
        let floor: f64 = row.split(',').nth(5).unwrap().parse().unwrap();
        assert!(floor < -40.0 && j["clips"][0]["pauses_counted"] == true, "{row}");
        // Deterministic, whatever the job count
        let again = dir.join("again");
        lab(&LabArgs { jobs: Some(1), out: Some(again.clone()), ..args(&dir, &out) }).unwrap();
        assert_eq!(std::fs::read_to_string(again.join("clips.csv")).unwrap(), csv);
    }

    /// The whole default lab on the fetched test-audio set (every model installed, every metric this build has), the
    /// scorecard printed and the files written to `NF_LAB_OUT` (default: a temp folder). Ignored (it reads the set and runs
    /// for minutes; the onnx build for the MOS metrics): `NF_LAB_OUT=DIR cargo test -p nf-app --release --features
    /// nf-app/onnx the_fetched_set -- --ignored --nocapture` (`NF_LAB_SNR=10` for one SNR, `NF_LAB_JOBS=N`).
    #[test]
    #[ignore]
    fn the_fetched_set_s_scorecard() {
        let Some(dir) = playlist::default_dir().filter(|d| d.is_dir()) else { return eprintln!("skipped: no test audio") };
        let out = std::env::var_os("NF_LAB_OUT").map_or_else(|| std::env::temp_dir().join("nf-lab"), std::path::PathBuf::from);
        let snrs = std::env::var("NF_LAB_SNR").ok().map(|v| v.split(',').map(|x| x.trim().parse().unwrap()).collect());
        let a = LabArgs {
            audio: Some(dir),
            out: Some(out),
            models: None,
            baseline: None,
            metrics: None,
            playlist: Options { snrs_db: snrs.unwrap_or_else(|| playlist::DEFAULT_SNRS_DB.to_vec()), ..Options::default() },
            config: None,
            processing: Default::default(),
            jobs: std::env::var("NF_LAB_JOBS").ok().map(|j| j.parse().unwrap()),
        };
        lab(&a).unwrap();
    }

    #[test]
    fn a_missing_set_or_a_bad_baseline_fails_with_a_message() {
        let dir = crate::test_dir("lab-errors");
        let e = lab(&args(&dir.join("none"), &dir)).unwrap_err();
        assert!(e.message.contains("fetch_test_audio.py"), "{}", e.message);
        set(&dir);
        let e = lab(&LabArgs { baseline: Some("ulunas".into()), ..args(&dir, &dir.join("o")) }).unwrap_err();
        assert!(e.message.contains("--baseline ulunas isn't one of the models"), "{}", e.message);
        let e = lab(&LabArgs { models: Some(vec!["no-such-pack".into()]), ..args(&dir, &dir.join("o")) }).unwrap_err();
        assert!(e.message.contains("no-such-pack"), "{}", e.message);
        // Clips over 16 s with DNSMOS: refused where DNSMOS runs; fine without it
        let long = |metrics: Vec<LabMetric>| LabArgs {
            metrics: Some(metrics),
            playlist: Options { clip_s: 20.0, snrs_db: vec![10.0], ..Options::default() },
            out: None,
            ..args(&dir, &dir.join("o"))
        };
        if mos::AVAILABLE {
            let e = lab(&long(vec![LabMetric::Dnsmos])).unwrap_err();
            assert!(e.message.starts_with("--clip-seconds 20: DNSMOS scores clips of 16 s at most whole"), "{}", e.message);
        }
        lab(&long(vec![LabMetric::Levels])).unwrap();
        // Without ONNX Runtime: the MOS metrics refused when asked for, left out with a note by default
        if !mos::AVAILABLE {
            let e = metric_choice(Some(&[LabMetric::Dnsmos])).unwrap_err();
            assert!(e.message.contains("--features onnx"), "{}", e.message);
            assert!(metric_choice(None).unwrap().2.is_some_and(|n| n.contains("--features onnx")));
        } else {
            assert_eq!(metric_choice(None).unwrap(), (true, true, None));
        }
        assert_eq!(metric_choice(Some(&[LabMetric::Levels])).unwrap(), (false, false, None));
    }
}
