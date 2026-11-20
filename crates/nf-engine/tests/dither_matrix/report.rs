//! The matrix's output: one JSON line per cell (`<tag>.jsonl`) and a table (`<tag>.txt`, printed too), in the output folder.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::measure::{Diff, Output, PhaseResult, Stats};

/// What every line carries about the run.
pub struct RunInfo {
    pub tag: String,
    pub guard: bool,
    pub onnx: bool,
    pub release: bool,
    pub scale: f32,
    pub speech: String,
}

/// One cell's identity.
pub struct Cell<'a> {
    pub chain: &'a str,
    pub gate: &'a str,
    pub dither: &'a str,
    pub signal: &'a str,
    pub latency: u32,
    pub blocks: usize,
    /// The chain's build and the feed's dithering, and the run.
    pub setup_s: f64,
    pub run_s: f64,
}

pub struct Report {
    jsonl: BufWriter<File>,
    table: PathBuf,
    info: RunInfo,
    timing: Vec<String>,
    output: Vec<String>,
    skipped: Vec<String>,
}

fn round(x: f64, places: i32) -> f64 {
    let s = 10f64.powi(places);
    (x * s).round() / s
}

fn stats_json(s: &Option<Stats>) -> Value {
    match s {
        Some(s) => json!({ "n": s.n, "p50": round(s.p50, 2), "p99": round(s.p99, 2), "max": round(s.max, 2), "mean": round(s.mean, 2) }),
        None => Value::Null,
    }
}

fn opt(x: Option<f64>, places: i32) -> Value {
    x.map_or(Value::Null, |v| json!(round(v, places)))
}

fn output_json(o: &Output) -> Value {
    json!({
        "zero_share": round(o.zero_share, 6),
        "subnormal_share": round(o.subnormal_share, 6),
        "first_zero_ms": opt(o.first_zero_ms, 1),
        "all_zero_ms": opt(o.all_zero_ms, 1),
        "peak_dbfs": opt(o.peak_dbfs, 1),
        "late_peak_dbfs": opt(o.late_peak_dbfs, 1),
    })
}

fn diff_json(d: &Option<Diff>) -> Value {
    match d {
        Some(d) => json!({
            "max_dbfs": opt(d.max_dbfs, 1),
            "rms_dbfs": opt(d.rms_dbfs, 1),
            "differ_share": round(d.differ_share, 6),
            "max_dbfs_per_s": d.max_dbfs_per_s.iter().map(|&v| opt(v, 1)).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    }
}

/// `p50 / p99 / max` in µs, or a dash.
fn triple(s: &Option<Stats>) -> String {
    match s {
        Some(s) => format!("{:7.1} {:7.1} {:8.1}", s.p50, s.p99, s.max),
        None => format!("{:>7} {:>7} {:>8}", "-", "-", "-"),
    }
}

fn ms(x: Option<f64>) -> String {
    x.map_or("never".into(), |v| format!("{v:.0}"))
}

fn dbfs(x: Option<f64>) -> String {
    x.map_or("zero".into(), |v| format!("{v:.1}"))
}

/// The row's cell and phase.
fn key(c: &Cell, p: &PhaseResult) -> String {
    format!("{:<13} {:<6} {:<7} {:<14} {:<8}", c.chain, c.gate, c.dither, c.signal, p.phase.name)
}

impl Report {
    /// Creates (or truncates) `<dir>/<tag>.jsonl`; the table is written by [`Report::finish`].
    pub fn create(dir: &Path, info: RunInfo) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let jsonl = BufWriter::new(File::create(dir.join(format!("{}.jsonl", info.tag)))?);
        Ok(Self { jsonl, table: dir.join(format!("{}.txt", info.tag)), info, timing: Vec::new(), output: Vec::new(), skipped: Vec::new() })
    }

    /// A cell's line (flushed at once, so a run cut short keeps what it did) and its table rows.
    pub fn cell(&mut self, c: &Cell, phases: &[PhaseResult]) -> std::io::Result<()> {
        let i = &self.info;
        let ph: Vec<Value> = phases
            .iter()
            .map(|p| {
                let t = &p.timing;
                json!({
                    "name": p.phase.name,
                    "start_block": p.phase.start,
                    "blocks": p.phase.blocks(),
                    "mic_silent": p.phase.mic_silent,
                    "timed_blocks": t.timed,
                    "us": {
                        "chain": stats_json(&t.chain),
                        "aec3": stats_json(&t.aec3),
                        "noise": stats_json(&t.noise),
                        "model": stats_json(&t.model),
                        "post": stats_json(&t.post),
                    },
                    "chain_mean_us_per_s": t.chain_mean_per_s.iter().map(|&v| round(v, 1)).collect::<Vec<_>>(),
                    "output": output_json(&p.output),
                    "vs_today": diff_json(&p.vs_today),
                })
            })
            .collect();
        let line = json!({
            "tag": i.tag,
            "guard": if i.guard { "on" } else { "off" },
            "onnx": i.onnx,
            "release": i.release,
            "scale": round(f64::from(i.scale), 6),
            "speech": i.speech,
            "chain": c.chain,
            "gate": c.gate,
            "dither": c.dither,
            "signal": c.signal,
            "latency_samples": c.latency,
            "blocks": c.blocks,
            "setup_s": round(c.setup_s, 3),
            "run_s": round(c.run_s, 3),
            "phases": ph,
        });
        writeln!(self.jsonl, "{line}")?;
        self.jsonl.flush()?;
        for p in phases {
            let t = &p.timing;
            self.timing.push(format!(
                "{}  {:>5}  {}  {}  {}  {}  {}",
                key(c, p),
                t.timed,
                triple(&t.chain),
                triple(&t.aec3),
                triple(&t.model),
                triple(&t.post),
                triple(&t.noise)
            ));
            let o = &p.output;
            let vs = p
                .vs_today
                .as_ref()
                .map_or("-".to_owned(), |d| format!("{:>6} {:>6} {:7.3}", dbfs(d.max_dbfs), dbfs(d.rms_dbfs), 100.0 * d.differ_share));
            self.output.push(format!(
                "{}  {:>6}  {:7.3}  {:>6}  {:>6}  {:>6}  {:>6}  {:9.4}  {}",
                key(c, p),
                if p.phase.mic_silent { "yes" } else { "no" },
                100.0 * o.zero_share,
                ms(o.first_zero_ms),
                ms(o.all_zero_ms),
                dbfs(o.peak_dbfs),
                dbfs(o.late_peak_dbfs),
                100.0 * o.subnormal_share,
                vs
            ));
        }
        Ok(())
    }

    /// A chain this build couldn't run, and why.
    pub fn skipped(&mut self, chain: &str, why: &str) {
        self.skipped.push(format!("{chain}: {why}"));
    }

    /// Writes the table and returns it.
    pub fn finish(mut self) -> std::io::Result<String> {
        self.jsonl.flush()?;
        let i = &self.info;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "TODO 10's dither matrix: {}; sonora's FTZ/DAZ guard {}; onnx {}; {}; scale {}; speech: {}",
            i.tag,
            if i.guard { "on" } else { "off" },
            if i.onnx { "yes" } else { "no" },
            if i.release { "release build" } else { "debug build (its timings mean little)" },
            i.scale,
            i.speech
        );
        for k in &self.skipped {
            let _ = writeln!(s, "skipped: {k}");
        }
        let head = format!("{:<13} {:<6} {:<7} {:<14} {:<8}", "chain", "gate", "dither", "signal", "phase");
        let _ = writeln!(s, "\nTiming, µs a block: p50 p99 max, over each phase's blocks after the signal's first second");
        let _ = writeln!(s, "{head}  {:>5}  {:<24}  {:<24}  {:<24}  {:<24}  noise stage", "timed", "chain", "aec3", "model", "post");
        for r in &self.timing {
            let _ = writeln!(s, "{}", r.trim_end());
        }
        let _ = writeln!(
            s,
            "\nOutput: exact zeros (% of samples), ms from the phase's start to the first all-zero block and to all-zero output \
             to its end, the peak over the phase and over its second half (dBFS), subnormals (%), and against today's dither \
             (mic+ref) in the same cell: the largest difference and its RMS (dBFS), and the samples that differ (%)"
        );
        let _ = writeln!(
            s,
            "{head}  {:>6}  {:>7}  {:>6}  {:>6}  {:>6}  {:>6}  {:>9}  vs today: max, RMS, differ%",
            "silent", "zeros%", "first0", "all0", "peak", "late", "subnorm%"
        );
        for r in &self.output {
            let _ = writeln!(s, "{}", r.trim_end());
        }
        std::fs::write(&self.table, &s)?;
        Ok(s)
    }
}
