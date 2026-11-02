//! A pack's per-hop and per-block CPU, offline: what `noise-filter bench --model ID` prints and nf-ort's ignored timing test
//! runs (`cargo test -p nf-ort --release --test ort -- --ignored timing --nocapture`), on `nf_stages::bench`'s signals and
//! timer, with RNNoise's rows beside them. Rows:
//! - the ONNX Runtime run alone, per hop (its input frames made from the signal before timing);
//! - the model per hop (`OrtSpectralModel::process_hop`: the bin-mapped STFT, the run, the high band, the inverse);
//! - the noise stage per block, its model inline (all of it on this thread, the default depth behind);
//! - the noise stage per block with its model on a worker: the audio thread's side only, paced at [`PACE`] (4× real time, so
//!   the worker keeps up), the worker's figures in its `InferenceStats`.
//!
//! Each row counts the Rust allocations after the warm-up (an `AllocCounter`) and, when [`crate::heap_probe`] is installed,
//! the C runtime heap calls on this thread. [`ProbeHooks`] counts the worker's too.
//!
//! [`bursts`] feeds the stage on its worker as the engine's wakes do, at real time on average: `k` blocks back to back every
//! `k` × 10 ms, and counts the misses (TODO 6: a wake of `k` blocks costs `max(0, k - depth)` when the worker had caught up).

use std::any::Any;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_core::dsp::stft::{Stft, hann};
use nf_core::noise::{DEFAULT_DEPTH, Executor, InferenceSnapshot, NoiseModel, WorkerCounters, WorkerGuard, WorkerHooks};
use nf_core::stage::{Activation, ProcessIo, Stage};
use nf_core::{BLOCK, Block};
use nf_stages::bench::{self as sbench, AllocCounter, BlockTimes, Signals, time_blocks_counted};

use crate::heap_probe;
use crate::pack::Pack;
use crate::session::{ModelSession, SessionOptions};
use crate::spectral::OrtSpectralModel;

/// The worker row's pace: a block every 2.5 ms.
pub const PACE: Duration = Duration::from_micros(2500);

/// One row and its C runtime heap calls on this thread (with the probe installed).
pub struct Row {
    pub times: BlockTimes,
    pub crt_calls: Option<u64>,
}

impl Row {
    pub fn line(&self) -> String {
        let mut l = self.times.line();
        if let Some(c) = self.crt_calls {
            let n = self.times.blocks().max(1);
            l.push_str(&format!("   C heap calls {c} ({:.1}/unit)", c as f64 / n as f64));
        }
        l
    }
}

/// What [`measure`] found.
pub struct Report {
    pub rows: Vec<Row>,
    /// The worker row's model, as its stats had it at the end.
    pub worker: InferenceSnapshot,
}

fn crt_calls() -> u64 {
    heap_probe::counts().crt_calls()
}

/// The thread's C runtime calls as a counter, when the probe is installed (counting is turned on for the run).
fn crt_counter() -> Option<AllocCounter> {
    heap_probe::installed().then_some(crt_calls as AllocCounter)
}

/// Another worker's hooks, with the heap probe counting on the worker (its counters then carry its C runtime calls).
pub struct ProbeHooks(pub Arc<dyn WorkerHooks>);

impl WorkerHooks for ProbeHooks {
    fn enter(&self) -> WorkerGuard {
        let inner = self.0.enter();
        heap_probe::set_counting(true);
        inner
    }

    fn steady(&self) -> Box<dyn Any> {
        self.0.steady()
    }

    fn counters(&self) -> WorkerCounters {
        let c = self.0.counters();
        WorkerCounters { crt_heap_calls: heap_probe::installed().then(crt_calls), ..c }
    }

    fn give_up(&self) -> bool {
        self.0.give_up()
    }
}

/// Each hop's model input from `x` (bin-mapped: the 48 kHz STFT's bins 0..n_fft/2 over r, and the Nyquist bin's fold, as
/// `spectral.rs` maps them), made before timing.
fn mix_frames(pack: &Pack, x: &[f32]) -> Vec<Vec<f32>> {
    let st = pack.stft();
    let r = pack.rate_factor();
    let mut stft = Stft::new(&hann(st.window_length * r, st.periodic), st.hop_length * r);
    let (k, scale) = (st.n_fft / 2, pack.manifest.signal.input_scale / r as f32);
    let fold = r > 1;
    x.chunks_exact(stft.hop())
        .map(|hop| {
            let spec = stft.push(hop);
            let mut m: Vec<f32> = spec[..=k].iter().flat_map(|c| [c.re * scale, c.im * scale]).collect();
            if fold {
                (m[2 * k], m[2 * k + 1]) = (2.0 * spec[k].re * scale, 0.0);
            }
            m
        })
        .collect()
}

/// The rows (module notes) for `pack` over `sig`'s noisy near end, timed on this thread. `hooks` set up the worker row's
/// worker. Turns this thread's heap-probe counting on while it runs, if the probe is installed.
pub fn measure(pack: &Pack, sig: &Signals, count: Option<AllocCounter>, hooks: Arc<dyn WorkerHooks>) -> Result<Report, String> {
    let crt = crt_counter();
    let was = heap_probe::set_counting(crt.is_some());
    let r = rows(pack, sig, count, crt, hooks);
    heap_probe::set_counting(was);
    r
}

fn rows(
    pack: &Pack, sig: &Signals, count: Option<AllocCounter>, crt: Option<AllocCounter>, hooks: Arc<dyn WorkerHooks>,
) -> Result<Report, String> {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let x = &sig.noisy;
    let mut rows = Vec::new();
    // The run alone.
    let frames = mix_frames(pack, x);
    let mut session = ModelSession::new(pack, SessionOptions::default())?;
    let (t, c) = time_blocks_counted("ONNX Runtime run (per hop)", frames.len(), count, crt, None, |h| {
        session.mix_mut().copy_from_slice(&frames[h]);
        let _ = black_box(session.run());
    });
    rows.push(Row { times: t, crt_calls: c });
    drop(session);
    // The model per hop.
    let mut model = OrtSpectralModel::new(pack, controls.clone(), SessionOptions::default())?;
    model.prepare().map_err(|e| e.0)?;
    let hop = model.info().hop;
    let mut out = vec![0.0f32; hop];
    let hops: Vec<&[f32]> = x.chunks_exact(hop).collect();
    let (t, c) = time_blocks_counted("model per hop (STFT 1536/768, run, inverse)", hops.len(), count, crt, None, |h| {
        black_box(model.process_hop(hops[h], &mut out));
    });
    rows.push(Row { times: t, crt_calls: c });
    drop(model);
    // The stage, inline and on a worker.
    let blocks = sig.blocks();
    let block = |b: usize| -> &Block { x.as_chunks::<BLOCK>().0.get(b).unwrap_or(&[0.0; BLOCK]) };
    let mut worker = InferenceSnapshot::default();
    for (name, exec, pace) in [
        ("noise stage per block, model inline", Executor::Inline, None),
        ("noise stage per block, audio side (worker)", Executor::Thread { hooks: hooks.clone(), wait_for_results: false }, Some(PACE)),
    ] {
        let (mut stage, stats) =
            crate::pack_stage(controls.clone(), pack.clone(), exec, SessionOptions::default(), DEFAULT_DEPTH).map_err(|e| e.0)?;
        stage.activate(&Activation::HOST).map_err(|e| e.0)?;
        let mut y = [0.0f32; BLOCK];
        let (t, c) = time_blocks_counted(name, blocks, count, crt, pace, |b| {
            stage.process(ProcessIo::new(&[block(b)], &mut y));
            black_box(&y);
        });
        rows.push(Row { times: t, crt_calls: c });
        worker = stats.snapshot();
    }
    rows.push(Row { times: sbench::rnnoise(sig, count).map_err(|e| e.0)?, crt_calls: None });
    rows.push(Row { times: sbench::rnnoise_model(sig, count).map_err(|e| e.0)?, crt_calls: None });
    Ok(Report { rows, worker })
}

/// What [`bursts`] counted for one depth and burst length.
#[derive(Clone, Copy, Debug)]
pub struct BurstRow {
    pub depth: u32,
    /// Blocks a wake brought, back to back.
    pub burst: usize,
    pub blocks: usize,
    pub stats: InferenceSnapshot,
}

impl BurstRow {
    pub fn line(&self) -> String {
        let s = &self.stats;
        format!(
            "depth {}, wakes of {} block(s): {} misses in {} blocks ({:.1}%), {} late, {} gaps; run p50 {:.0} µs, p99 {:.0}",
            self.depth,
            self.burst,
            s.misses,
            self.blocks,
            100.0 * s.misses as f64 / self.blocks.max(1) as f64,
            s.late,
            s.gaps,
            s.run_us_p50,
            s.run_us_p99
        )
    }
}

/// The noise stage around `pack` on a worker (`hooks`), `depth` blocks behind, fed `blocks` blocks of `x` as wakes of
/// `burst` blocks back to back every `burst` × 10 ms (real time on average): the engine's bursty delivery (module notes).
/// Takes `blocks` × 10 ms.
pub fn bursts(pack: &Pack, x: &[f32], hooks: Arc<dyn WorkerHooks>, depth: u32, burst: usize, blocks: usize) -> Result<BurstRow, String> {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let exec = Executor::Thread { hooks, wait_for_results: false };
    let (mut stage, stats) = crate::pack_stage(controls, pack.clone(), exec, SessionOptions::default(), depth).map_err(|e| e.0)?;
    stage.activate(&Activation::HOST).map_err(|e| e.0)?;
    let all = x.as_chunks::<BLOCK>().0;
    let mut y = [0.0f32; BLOCK];
    let burst = burst.max(1);
    let start = Instant::now();
    for b in 0..blocks {
        if b % burst == 0 {
            let due = start + Duration::from_millis(10 * b as u64);
            while Instant::now() < due {
                std::hint::spin_loop();
            }
        }
        let block = all.get(b % all.len().max(1)).unwrap_or(&[0.0; BLOCK]);
        stage.process(ProcessIo::new(&[block], &mut y));
        black_box(&y);
    }
    Ok(BurstRow { depth, burst, blocks, stats: stats.snapshot() })
}
