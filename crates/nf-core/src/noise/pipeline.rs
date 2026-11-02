//! [`Pipelined`]: a [`NoiseModel`] run on an inference worker thread, `depth` blocks behind the audio thread (TODO 6). This
//! is CLAUDE.md's one exception to the real-time rule: ONNX Runtime allocates from the C runtime's heap on every run, which
//! the audio thread may not, so the run happens on a worker whose per-run allocations are measured and kept off the capture
//! and render threads. The audio thread's side allocates nothing, takes no lock and makes one system call, a wake-up.
//!
//! **Protocol.** The audio thread exchanges whole 480-sample blocks with the worker through two bounded rings (`rtrb`,
//! capacity [`RING`] each way): a [`Job`] `{ epoch, seq, reset_inner, samples }` out, a [`Done`] `{ epoch, seq, ok, vad,
//! samples }` back, both moved by value. Each call `c` (one block), with `D` the depth:
//! 1. takes result `(epoch, c - D)` from the ring: older ones ahead of it are dropped and counted as `late`, those of an
//!    older epoch (before a reset) dropped silently, and a later block's at the front means it never came;
//! 2. pushes job `c`; a full ring is a `gap`, and the next job that gets in carries `reset_inner`;
//! 3. wakes the worker (`Thread::unpark`: an atomic swap, plus `WakeByAddressSingle` only if it is parked; no allocation,
//!    no blocking);
//! 4. outputs result `c - D` if it came and is usable, else the substitute (below) and counts a `miss`.
//!
//! So each block's result is used exactly `D` calls later in both executors ([`Executor::Inline`] runs the job on the
//! calling thread and keeps its result for `D` calls), and offline output equals live output bit for bit whenever the worker
//! misses no deadline. The cost is `D` blocks: the offset is the inner model's (re-blocked) stage latency + `D` × 480
//! (UL-UNAS at the default depth 2: 1440 + 960 = 2400).
//!
//! **The deadline is the call `D` calls later, not a block's time.** The engine processes every complete block of a wake
//! back to back, so a wake that brings `k` blocks (a 20 ms device period, a cable's burst, a late wake, a stall's backlog)
//! gives the worker microseconds for the jobs pushed earlier in it: if the worker had caught up before the wake, the wake
//! costs `max(0, k - D)` misses. Depth 2 absorbs the two-block wakes VB-CABLE gives a fifth to a third of the time (TODO
//! 8's detail, "Delivery on VB-CABLE"); a stall's backlog misses (counted, never a gap: [`RING`] holds more than the
//! engine's mic FIFO). [`DEFAULT_DEPTH`]; the config's `model_depth` sets it (1 to [`MAX_DEPTH`]).
//!
//! **The substitute** is the clean input delayed by the same offset (a fixed [`DelayLine`]), times `g`: `rms(out) / rms(dry)`
//! over the good blocks, clamped to [0, 1] and smoothed, and 0 after a stage reset. The two paths are aligned, so going back
//! from the substitute to the model takes a one-block crossfade and makes no comb (none from a silent substitute: after a
//! stage reset the model's first output follows exactly, as a fresh model's would).
//!
//! **Start-up rule.** After any reset of the inner model (a stage reset, a gap, an error) the first `D + ⌈inner latency /
//! 480⌉` calls (5 for UL-UNAS at depth 2: exactly its 2400-sample start-up region) output the substitute, counting no miss.
//! With `g = 0` after a stage reset that is silence, as a fresh model's start-up is; the chain keeps a stage switched on on
//! its own aligned delay line until the stage's start-up has passed (`chain.rs`), so a toggle-on never plays it. Both
//! executors follow the rule, so they stay bit-identical, and a toggle-on, which primes and processes in one block, never
//! counts a miss. A new `Pipelined` starts in that state.
//!
//! **The worker** runs [`WorkerHooks::enter`] first (the engine's: MMCSS "Pro Audio" at normal priority), then `make()` (a
//! pack's: loading and hashing it, and building its session), wraps a model whose hop isn't 480 in [`Reblocked`], runs its
//! `prepare()` (a pack's warm-up), then [`WorkerHooks::steady`] (the engine's: nf-mem's real-time scope, so whatever its
//! steady state allocates in Rust comes from the locked pool and is counted, while the pack's bytes and the session's
//! building don't take the pool), and says "ready". The builder waits up to [`READY_TIMEOUT`], looking at
//! [`WorkerHooks::give_up`] every 100 ms. The loop pops jobs, resets the model when a job says so, runs it, pushes the
//! result, and parks for up to 100 ms when the ring is empty, checking its stop flag. No panic paths in the loop (release
//! builds abort); it logs nothing. Dropping the `Pipelined` stops and joins it; the model (an ONNX Runtime session) is
//! dropped on the worker.
//!
//! [`InferenceStats`] (atomics, shared): runs, misses, late results, gaps, errors, the run time (last, max, p50 and p99 over
//! the last 100 runs), the worker's own allocation counters from its hooks since it was ready, and a scheduling class the
//! worker was refused.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::atomic::AtomicF32;
use crate::dsp::delay::DelayLine;
use crate::dsp::meter::mean_square;
use crate::latency::stats::RollingStats;
use crate::noise::{ModelInfo, NoiseModel, Reblocked};
use crate::stage::StageError;
use crate::{BLOCK, Block};

/// Each ring's capacity, in blocks: more than the engine's mic FIFO holds (20 blocks) plus the depth, so the longest backlog
/// a wake can process back to back never finds the job ring full (a gap resets the model's state). About 2 KB a slot.
pub const RING: usize = 32;
/// How many blocks behind the audio thread the model runs, by default (module notes: two-block wakes are common).
pub const DEFAULT_DEPTH: u32 = 2;
/// The deepest the pipeline goes (UL-UNAS: 1440 + 4 × 480 = 3360 samples, under the chain's 4096).
pub const MAX_DEPTH: u32 = 4;
/// How long building waits for the worker's "ready" (its model built, prepared and warmed up): well under the engine's
/// 5 s start timeout, so a slow build fails the start with the model named rather than as a timeout.
pub const READY_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the builder looks at [`WorkerHooks::give_up`] while it waits.
const READY_POLL: Duration = Duration::from_millis(100);
/// How long the worker parks with nothing to do before it checks its stop flag again.
const PARK: Duration = Duration::from_millis(100);
/// How long [`Executor::Thread`] with `wait_for_results` waits for one result before counting a miss (offline only).
const WAIT_LIMIT: Duration = Duration::from_secs(10);
/// The substitute's gain follows the good blocks' `rms(out) / rms(dry)` with this weight per block (about 50 ms).
const GAIN_SMOOTHING: f32 = 0.2;
/// Below this dry RMS (-120 dBFS) a good block doesn't move the substitute's gain.
const MIN_DRY_RMS: f32 = 1e-6;

/// Where the model runs.
pub enum Executor {
    /// On the calling thread, `depth` blocks behind as on a worker: offline tools and tests.
    Inline,
    /// On a worker thread of its own.
    Thread {
        hooks: Arc<dyn WorkerHooks>,
        /// Each call waits for its due result instead of substituting: offline use and tests only (it blocks).
        wait_for_results: bool,
    },
}

/// The worker thread's set-up, and its counters (the engine's: MMCSS, then the real-time scope; nf-mem's counters).
pub trait WorkerHooks: Send + Sync {
    /// Runs first on the worker, before the model is built; the guard lives as long as the thread.
    fn enter(&self) -> WorkerGuard;

    /// Runs on the worker once its model is built, prepared and warmed up, just before it says "ready"; the guard lives
    /// until its loop ends, before the model is dropped.
    fn steady(&self) -> Box<dyn Any> {
        Box::new(())
    }

    /// The worker thread's own counters, read on the worker after each job. Real-time.
    fn counters(&self) -> WorkerCounters {
        WorkerCounters::default()
    }

    /// Read on the building thread every 100 ms while it waits for "ready": `true` gives the wait up (the engine's: the run
    /// was stopped). The worker then stops on its own once its build is done.
    fn give_up(&self) -> bool {
        false
    }
}

/// What [`WorkerHooks::enter`] returns: the guard, and a real-time scheduling class the worker was refused.
pub struct WorkerGuard {
    pub guard: Box<dyn Any>,
    /// The code of the refusal (the engine's: MMCSS's Win32 error; the worker then runs at a raised normal priority below
    /// the audio thread's), for the start's warnings ([`InferenceSnapshot::worker_sched_refused`]).
    pub refused: Option<u32>,
}

impl WorkerGuard {
    /// No guard and no refusal.
    pub fn none() -> Self {
        Self { guard: Box::new(()), refused: None }
    }
}

/// A worker thread's counters ([`WorkerHooks::counters`]): cumulative, since the thread started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerCounters {
    /// Rust allocations served by the real-time pool.
    pub pool_allocs: u64,
    /// Rust allocations that fell back to the system heap.
    pub fallbacks: u64,
    /// C runtime heap calls (allocations, frees, reallocations), when a heap probe is installed (`nf_ort::heap_probe`).
    pub crt_heap_calls: Option<u64>,
}

/// No set-up and no counters: a plain thread (tests).
pub struct NoHooks;

impl WorkerHooks for NoHooks {
    fn enter(&self) -> WorkerGuard {
        WorkerGuard::none()
    }
}

/// What the worker and the audio thread count (see the module notes). Shared as an `Arc`; every field is an atomic.
#[derive(Debug, Default)]
pub struct InferenceStats {
    /// Jobs (blocks) the model ran and handed back (counted after the result is in the ring).
    pub jobs: AtomicU64,
    /// Inferences (model hops) run.
    pub runs: AtomicU64,
    /// Blocks whose result wasn't there in time (outside the start-up rule's blocks): the substitute played.
    pub misses: AtomicU64,
    /// Results that came after their block was played.
    pub late: AtomicU64,
    /// Jobs the full ring had no room for.
    pub gaps: AtomicU64,
    /// Results the model marked failed (an error, or a non-finite output).
    pub errors: AtomicU64,
    /// One inference's time, in µs: the last, the most, and the median and 99th percentile of the last 100.
    pub run_us_last: AtomicF32,
    pub run_us_max: AtomicF32,
    pub run_us_p50: AtomicF32,
    pub run_us_p99: AtomicF32,
    /// The worker's counters since it was ready ([`WorkerCounters`]); `crt_heap_calls` is valid with `crt_probe`.
    pub worker_pool_allocs: AtomicU64,
    pub worker_fallbacks: AtomicU64,
    pub worker_crt_heap_calls: AtomicU64,
    pub crt_probe: AtomicBool,
    /// [`WorkerGuard::refused`]'s code, 0 for none.
    pub worker_sched_refused: AtomicU32,
}

/// A plain copy of [`InferenceStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InferenceSnapshot {
    pub jobs: u64,
    pub runs: u64,
    pub misses: u64,
    pub late: u64,
    pub gaps: u64,
    pub errors: u64,
    pub run_us_last: f32,
    pub run_us_max: f32,
    pub run_us_p50: f32,
    pub run_us_p99: f32,
    pub worker_pool_allocs: u64,
    pub worker_fallbacks: u64,
    pub worker_crt_heap_calls: Option<u64>,
    pub worker_sched_refused: Option<u32>,
}

impl InferenceStats {
    pub fn snapshot(&self) -> InferenceSnapshot {
        InferenceSnapshot {
            jobs: self.jobs.load(Acquire),
            runs: self.runs.load(Relaxed),
            misses: self.misses.load(Relaxed),
            late: self.late.load(Relaxed),
            gaps: self.gaps.load(Relaxed),
            errors: self.errors.load(Relaxed),
            run_us_last: self.run_us_last.load(),
            run_us_max: self.run_us_max.load(),
            run_us_p50: self.run_us_p50.load(),
            run_us_p99: self.run_us_p99.load(),
            worker_pool_allocs: self.worker_pool_allocs.load(Relaxed),
            worker_fallbacks: self.worker_fallbacks.load(Relaxed),
            worker_crt_heap_calls: self.crt_probe.load(Relaxed).then(|| self.worker_crt_heap_calls.load(Relaxed)),
            worker_sched_refused: Some(self.worker_sched_refused.load(Relaxed)).filter(|&c| c != 0),
        }
    }
}

/// One block for the worker.
struct Job {
    epoch: u32,
    seq: u64,
    reset_inner: bool,
    samples: Block,
}

/// Its result.
struct Done {
    epoch: u32,
    seq: u64,
    ok: bool,
    vad: f32,
    samples: Block,
}

/// Runs the (480-hop) model on jobs: the same code on the worker and inline, so both executors agree bit for bit.
struct Runner {
    model: Box<dyn NoiseModel>,
    stats: Arc<InferenceStats>,
    run_us: RollingStats<100>,
}

impl Runner {
    /// `model`, re-blocked to 480 if it needs it.
    fn new(model: Box<dyn NoiseModel>, stats: Arc<InferenceStats>) -> Self {
        let model: Box<dyn NoiseModel> = if model.info().hop == BLOCK { model } else { Box::new(Reblocked::new(model)) };
        Self { model, stats, run_us: RollingStats::new() }
    }

    /// The job's result. The caller counts it in `jobs` once it is handed back.
    #[inline]
    fn run(&mut self, job: &Job) -> Done {
        if job.reset_inner {
            self.model.reset();
        }
        let mut samples = [0.0f32; BLOCK];
        let vad = self.model.process_hop(&job.samples, &mut samples);
        let report = self.model.take_report();
        let ok = !report.failed && samples.iter().all(|v| v.is_finite());
        let s = &self.stats;
        if report.runs > 0 {
            s.runs.fetch_add(u64::from(report.runs), Relaxed);
            let us = report.run_us / report.runs as f32;
            self.run_us.push(us);
            s.run_us_last.store(us);
            s.run_us_max.store(s.run_us_max.load().max(us));
            s.run_us_p50.store(self.run_us.median());
            s.run_us_p99.store(self.run_us.percentile(0.99));
        }
        if !ok {
            s.errors.fetch_add(1, Relaxed);
        }
        Done { epoch: job.epoch, seq: job.seq, ok, vad, samples }
    }
}

/// The inline executor's state, boxed once at construction (its results are large).
struct InlineRun {
    runner: Runner,
    /// The last `depth` jobs' results, oldest first (capacity reserved at construction: pushing never allocates).
    pending: VecDeque<Done>,
}

enum Mode {
    Inline(Box<InlineRun>),
    Thread { jobs: Producer<Job>, done: Consumer<Done>, worker: Thread, stop: Arc<AtomicBool>, handle: Option<JoinHandle<()>>, wait: bool },
}

/// A model run `depth` blocks behind, on a worker or inline (see the module notes).
pub struct Pipelined {
    info: ModelInfo,
    mode: Mode,
    stats: Arc<InferenceStats>,
    /// How many calls later a job's result is used.
    depth: u32,
    /// Calls the start-up rule substitutes for after a reset of the inner model.
    startup_calls: u32,
    epoch: u32,
    /// This call's sequence number.
    seq: u64,
    /// Bit `i`: the job of call `seq - 1 - i` got in, in this epoch.
    pushed: u64,
    /// The next job resets the inner model.
    reset_pending: bool,
    /// Calls left in the start-up rule's substitution.
    holdoff: u32,
    /// The clean input, delayed by the offset.
    dry: DelayLine,
    dry_block: Block,
    /// The substitute's gain.
    g: f32,
    /// The last call played the substitute.
    substituted: bool,
}

/// The maker of a model, run where the model will run.
pub type MakeModel = Box<dyn FnOnce() -> Result<Box<dyn NoiseModel>, StageError> + Send>;

impl Pipelined {
    /// Builds the model with `make` (on the worker for [`Executor::Thread`], whose hooks run first; here for
    /// [`Executor::Inline`]) and the layer around it, `depth` blocks behind (clamped to 1..=[`MAX_DEPTH`]). For a worker,
    /// waits up to [`READY_TIMEOUT`] for its model to be built, prepared and warmed up, giving up early when the hooks say
    /// so; a failure there is this call's error. Allocates.
    pub fn spawn(make: MakeModel, exec: Executor, depth: u32) -> Result<Self, StageError> {
        let depth = depth.clamp(1, MAX_DEPTH);
        let stats = Arc::new(InferenceStats::default());
        match exec {
            Executor::Inline => {
                let runner = Runner::new(make()?, stats.clone());
                let inner = runner.model.info().clone();
                let pending = VecDeque::with_capacity(depth as usize + 1);
                Ok(Self::with(inner, Mode::Inline(Box::new(InlineRun { runner, pending })), stats, depth))
            }
            Executor::Thread { hooks, wait_for_results } => {
                let (jobs, job_rx) = RingBuffer::<Job>::new(RING);
                let (done_tx, done) = RingBuffer::<Done>::new(RING);
                let stop = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<ModelInfo, StageError>>(1);
                let (s, st, h) = (stats.clone(), stop.clone(), hooks.clone());
                let handle = std::thread::Builder::new()
                    .name("nf-inference".into())
                    .spawn(move || worker_main(make, h, s, st, job_rx, done_tx, ready_tx))
                    .map_err(|e| StageError(format!("couldn't start the inference worker: {e}")))?;
                let worker = handle.thread().clone();
                let started = Instant::now();
                let abandon = |why: String| -> Result<Self, StageError> {
                    // Still building, or gone: it stops at its next look at the flag, on its own.
                    stop.store(true, Release);
                    worker.unpark();
                    Err(StageError(format!("the inference worker {why}")))
                };
                let inner = loop {
                    match ready_rx.recv_timeout(READY_POLL) {
                        Ok(Ok(info)) => break info,
                        Ok(Err(e)) => {
                            let _ = handle.join();
                            return Err(e);
                        }
                        Err(RecvTimeoutError::Disconnected) => return abandon("ended before it was ready".into()),
                        Err(RecvTimeoutError::Timeout) if hooks.give_up() => {
                            return abandon("was still building its model when the start was stopped".into());
                        }
                        Err(RecvTimeoutError::Timeout) if started.elapsed() >= READY_TIMEOUT => {
                            return abandon(format!("wasn't ready within {} s", READY_TIMEOUT.as_secs()));
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                };
                let mode = Mode::Thread { jobs, done, worker, stop, handle: Some(handle), wait: wait_for_results };
                Ok(Self::with(inner, mode, stats, depth))
            }
        }
    }

    fn with(inner: ModelInfo, mode: Mode, stats: Arc<InferenceStats>, depth: u32) -> Self {
        let inner_latency = inner.stage_latency();
        let offset = inner_latency + depth as usize * BLOCK;
        let info = ModelInfo { latency_samples: (offset + BLOCK) as u32, hop: BLOCK, ..inner };
        let startup_calls = depth + inner_latency.div_ceil(BLOCK) as u32;
        Self {
            info,
            mode,
            stats,
            depth,
            startup_calls,
            epoch: 0,
            seq: 0,
            pushed: 0,
            reset_pending: false,
            holdoff: startup_calls,
            dry: DelayLine::new(offset, offset),
            dry_block: [0.0; BLOCK],
            g: 0.0,
            substituted: true,
        }
    }

    /// What the worker and this layer count.
    pub fn stats(&self) -> Arc<InferenceStats> {
        self.stats.clone()
    }

    /// The start-up rule's length, in calls.
    pub fn startup_calls(&self) -> u32 {
        self.startup_calls
    }

    /// How many blocks behind the model runs.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Step 1: this call's due result (call `seq - depth`'s), if it came and is of this epoch.
    #[inline]
    fn take_result(&mut self) -> Option<Done> {
        let want = self.seq.checked_sub(u64::from(self.depth))?;
        let expected = (self.pushed >> (self.depth - 1)) & 1 == 1;
        let epoch = self.epoch;
        let stats = &self.stats;
        match &mut self.mode {
            Mode::Inline(i) => {
                while let Some(d) = i.pending.front() {
                    let (e, s) = (d.epoch, d.seq);
                    if e == epoch && s >= want {
                        return if s == want { i.pending.pop_front() } else { None };
                    }
                    i.pending.pop_front();
                }
                None
            }
            Mode::Thread { done, wait, .. } => {
                let mut since: Option<Instant> = None;
                loop {
                    while let Ok(d) = done.peek() {
                        let (e, s) = (d.epoch, d.seq);
                        if e != epoch {
                            let _ = done.pop(); // from before a reset
                        } else if s < want {
                            let _ = done.pop();
                            stats.late.fetch_add(1, Relaxed);
                        } else if s == want {
                            return done.pop().ok();
                        } else {
                            return None; // a later block's: this one's never came (its job didn't get in)
                        }
                    }
                    if !(*wait && expected) {
                        return None;
                    }
                    // Offline only: wait for it.
                    let t = *since.get_or_insert_with(Instant::now);
                    if t.elapsed() > WAIT_LIMIT {
                        return None;
                    }
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Step 2 and 3: this call's job, to the worker (or run now, inline).
    #[inline]
    fn submit(&mut self, input: &Block) {
        let reset_inner = std::mem::take(&mut self.reset_pending);
        if reset_inner {
            self.holdoff = self.startup_calls;
        }
        let job = Job { epoch: self.epoch, seq: self.seq, reset_inner, samples: *input };
        let got_in = match &mut self.mode {
            Mode::Inline(i) => {
                let d = i.runner.run(&job);
                i.pending.push_back(d);
                self.stats.jobs.fetch_add(1, Release);
                true
            }
            Mode::Thread { jobs, worker, .. } => {
                let pushed = jobs.push(job).is_ok();
                if !pushed {
                    self.stats.gaps.fetch_add(1, Relaxed);
                    self.reset_pending = true;
                }
                worker.unpark();
                pushed
            }
        };
        self.pushed = (self.pushed << 1) | u64::from(got_in);
    }

    /// Step 4's substitute: the delayed clean input × `g`.
    #[inline]
    fn substitute(&self, out: &mut [f32]) {
        for (o, &d) in out.iter_mut().zip(&self.dry_block) {
            *o = d * self.g;
        }
    }
}

impl NoiseModel for Pipelined {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    /// Inline: prepares the model (a pack's warm-up) on the calling thread. On a worker, done there before "ready".
    fn prepare(&mut self) -> Result<(), StageError> {
        match &mut self.mode {
            Mode::Inline(i) => i.runner.model.prepare(),
            Mode::Thread { .. } => Ok(()),
        }
    }

    /// Real-time: a new epoch (results of the old one are dropped), the next job resets the model, the dry path is cleared
    /// and the substitute's gain goes to 0.
    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.reset_pending = true;
        self.pushed = 0;
        self.dry.reset();
        self.g = 0.0;
        self.substituted = true;
        if let Mode::Inline(i) = &mut self.mode {
            i.pending.clear();
        }
    }

    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let (Some(input), Some(output)) = (input.first_chunk::<BLOCK>(), output.first_chunk_mut::<BLOCK>()) else {
            return f32::NAN; // never: its hop is 480
        };
        // 1. The due result.
        let result = self.take_result();
        if result.as_ref().is_some_and(|d| !d.ok) {
            self.reset_pending = true; // the model failed: start it afresh with this job
        }
        // 2, 3. This block's job.
        self.submit(input);
        // 4. The result, or the substitute.
        self.dry.process(input, &mut self.dry_block);
        self.seq = self.seq.wrapping_add(1);
        if self.holdoff > 0 {
            self.holdoff -= 1;
            self.substitute(output);
            self.substituted = true;
            return f32::NAN;
        }
        match result.filter(|d| d.ok) {
            Some(d) => {
                if self.substituted && self.g > 0.0 {
                    // Back from the substitute: a one-block crossfade (the paths are aligned). From silence (g = 0: a stage
                    // reset's start-up) the model's output follows as a fresh model's would.
                    let step = 1.0 / BLOCK as f32;
                    for (i, (o, (&y, &dry))) in output.iter_mut().zip(d.samples.iter().zip(&self.dry_block)).enumerate() {
                        let t = (i + 1) as f32 * step;
                        *o = dry * self.g * (1.0 - t) + y * t;
                    }
                } else {
                    output.copy_from_slice(&d.samples);
                }
                self.substituted = false;
                let dry = mean_square(&self.dry_block).sqrt();
                if dry > MIN_DRY_RMS {
                    let ratio = (mean_square(&d.samples).sqrt() / dry).clamp(0.0, 1.0);
                    if ratio.is_finite() {
                        self.g += GAIN_SMOOTHING * (ratio - self.g);
                    }
                }
                d.vad
            }
            None => {
                self.stats.misses.fetch_add(1, Relaxed);
                self.substitute(output);
                self.substituted = true;
                f32::NAN
            }
        }
    }
}

impl Drop for Pipelined {
    fn drop(&mut self) {
        if let Mode::Thread { stop, worker, handle, .. } = &mut self.mode {
            stop.store(true, Release);
            worker.unpark();
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// The worker thread (see the module notes).
fn worker_main(
    make: MakeModel, hooks: Arc<dyn WorkerHooks>, stats: Arc<InferenceStats>, stop: Arc<AtomicBool>, mut jobs: Consumer<Job>,
    mut done: Producer<Done>, ready: std::sync::mpsc::SyncSender<Result<ModelInfo, StageError>>,
) {
    let entered = hooks.enter();
    stats.worker_sched_refused.store(entered.refused.unwrap_or(0), Relaxed);
    let built = make().and_then(|m| {
        let mut runner = Runner::new(m, stats.clone());
        runner.model.prepare()?;
        Ok(runner)
    });
    let mut runner = match built {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // The info goes back before the steady scope, and the baseline is taken after it has gone, so the counters read 0 for
    // as long as the worker's own steady state allocates nothing.
    let info = runner.model.info().clone();
    let steady = hooks.steady();
    if ready.send(Ok(info)).is_err() {
        return; // the builder gave up
    }
    drop(ready);
    let base = hooks.counters();
    stats.crt_probe.store(base.crt_heap_calls.is_some(), Relaxed);
    while !stop.load(Acquire) {
        while let Ok(job) = jobs.pop() {
            let d = runner.run(&job);
            let _ = done.push(d); // full: the audio thread stopped reading, and drops it as late anyway
            stats.jobs.fetch_add(1, Release);
            let c = hooks.counters();
            stats.worker_pool_allocs.store(c.pool_allocs.saturating_sub(base.pool_allocs), Relaxed);
            stats.worker_fallbacks.store(c.fallbacks.saturating_sub(base.fallbacks), Relaxed);
            if let (Some(now), Some(then)) = (c.crt_heap_calls, base.crt_heap_calls) {
                stats.worker_crt_heap_calls.store(now.saturating_sub(then), Relaxed);
            }
        }
        std::thread::park_timeout(PARK);
    }
    // The steady scope ends first, then the model (its session) drops here, on this thread, before the hooks' guard.
    drop(steady);
    drop(runner);
    drop(entered);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::noise::{DelayModel, HopReport};
    use crate::stage::text;

    fn ramp(len: usize) -> Vec<f32> {
        (0..len).map(|i| ((i * 7919) % 2000) as f32 / 2000.0 * 0.8 - 0.4).collect()
    }

    fn run(m: &mut dyn NoiseModel, x: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (i, o) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0) {
            m.process_hop(i, o);
        }
        y
    }

    fn delay(hop: usize, lat: u32) -> MakeModel {
        Box::new(move || Ok(Box::new(DelayModel::new(hop, lat)) as Box<dyn NoiseModel>))
    }

    fn thread(wait: bool) -> Executor {
        Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: wait }
    }

    /// The offset is the inner model's stage latency + depth × 480; past the start-up rule's calls the output is the input
    /// that late, to the sample, with no miss; before it, silence (g = 0).
    #[test]
    fn the_latency_formula_and_the_start_up_rule() {
        for depth in 1..=MAX_DEPTH {
            for (hop, lat) in [(480usize, 960u32), (768, 1536), (512, 1024), (256, 768)] {
                let inner = (lat as usize - hop) + crate::dsp::reblock::reblock_delay(hop);
                let mut p = Pipelined::spawn(delay(hop, lat), Executor::Inline, depth).unwrap();
                let offset = inner + depth as usize * BLOCK;
                assert_eq!((p.info().hop, p.info().model_offset(), p.depth()), (BLOCK, offset, depth), "hop {hop}");
                assert_eq!(p.startup_calls() as usize, depth as usize + inner.div_ceil(BLOCK));
                let x = ramp(40 * BLOCK);
                let y = run(&mut p, &x);
                let startup = p.startup_calls() as usize * BLOCK;
                assert!(startup >= offset, "the rule covers the start-up region");
                for n in 0..y.len() {
                    let want = if n >= startup { x[n - offset] } else { 0.0 };
                    assert_eq!(y[n], want, "depth {depth}, hop {hop}: sample {n} (start-up silence for {startup})");
                }
                assert_eq!(p.stats().snapshot().misses, 0);
            }
        }
        let p = Pipelined::spawn(delay(768, 1536), Executor::Inline, DEFAULT_DEPTH).unwrap();
        assert_eq!((p.info().latency_samples, p.startup_calls()), (2880, 5), "UL-UNAS's framing: an offset of 2400");
        assert_eq!(Pipelined::spawn(delay(480, 960), Executor::Inline, 0).unwrap().depth(), 1, "clamped");
        assert_eq!(Pipelined::spawn(delay(480, 960), Executor::Inline, 9).unwrap().depth(), MAX_DEPTH);
    }

    /// Inline and a worker that's waited for give the same output, bit for bit, through resets, at each depth.
    #[test]
    fn inline_equals_a_waited_for_thread_bit_for_bit() {
        let x = ramp(60 * BLOCK);
        for depth in [1, 2, 3] {
            let go = |exec| {
                let mut p = Pipelined::spawn(delay(768, 1536), exec, depth).unwrap();
                p.prepare().unwrap();
                let mut y = run(&mut p, &x[..30 * BLOCK]);
                p.reset();
                y.extend(run(&mut p, &x[30 * BLOCK..]));
                drained(&p, 60); // the last `depth` jobs, which no call waited for
                (y, p.stats().snapshot())
            };
            let (a, sa) = go(Executor::Inline);
            let (b, sb) = go(thread(true));
            let first = a.iter().zip(&b).position(|(p, q)| p != q);
            assert_eq!(first, None, "depth {depth}: first difference at {first:?} {sa:?} {sb:?}");
            assert_eq!((sa.misses, sb.misses, sa.jobs, sb.jobs, sb.late, sb.gaps), (0, 0, 60, 60, 0, 0), "depth {depth}");
        }
    }

    /// A delay model that waits at a gate the test holds: the worker's lateness is the test's to set, not the scheduler's.
    struct Held {
        inner: DelayModel,
        gate: Arc<Mutex<()>>,
    }

    impl NoiseModel for Held {
        fn info(&self) -> &ModelInfo {
            self.inner.info()
        }
        fn prepare(&mut self) -> Result<(), StageError> {
            Ok(())
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
            drop(self.gate.lock());
            self.inner.process_hop(input, output)
        }
        fn take_report(&mut self) -> HopReport {
            HopReport { runs: 1, run_us: 1.0, failed: false }
        }
    }

    fn held(gate: &Arc<Mutex<()>>) -> MakeModel {
        let g = gate.clone();
        Box::new(move || Ok(Box::new(Held { inner: DelayModel::new(480, 960), gate: g }) as Box<dyn NoiseModel>))
    }

    /// Waits until the worker has handed back `n` jobs (their results are in the ring).
    fn drained(p: &Pipelined, n: u64) {
        let t = Instant::now();
        while p.stats.jobs.load(Acquire) < n {
            assert!(t.elapsed() < Duration::from_secs(10), "the worker never caught up");
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// The engine's wakes, played deterministically: bursts of `k` calls back to back, the worker held during each burst and
    /// let catch up between them. Each burst costs exactly `max(0, k - depth)` misses past the start-up rule, every block
    /// whose result wasn't there has it dropped as late when it comes, no ring fills (a burst of 6, a stall's backlog, makes
    /// no gap), and every other block is the model's output: exactly the delayed input, or on the crossfade back from a
    /// substitute between it and silence (the substitute's gain is under 1).
    #[test]
    fn a_burst_costs_the_blocks_past_the_depth_and_nothing_else() {
        let x = ramp(240 * BLOCK);
        let blocks = x.as_chunks::<BLOCK>().0;
        for depth in [1u32, 2, 3] {
            for k in [1usize, 2, 3, 6] {
                let gate = Arc::new(Mutex::new(()));
                let mut p = Pipelined::spawn(held(&gate), thread(false), depth).unwrap();
                let offset = p.info().model_offset();
                let startup = p.startup_calls() as usize;
                let mut outs = vec![[0.0f32; BLOCK]; blocks.len()];
                let (mut c, mut misses, mut late) = (0usize, 0u64, 0u64);
                let mut missed = vec![false; blocks.len()];
                // Bursts, then one more single call (position 0: never a miss) that takes the last burst's late results.
                while c + k < blocks.len() {
                    let lock = gate.lock().unwrap();
                    for j in 0..k {
                        p.process_hop(&blocks[c], &mut outs[c]);
                        // This call's due result is of a job pushed earlier in this burst: the held worker hasn't run it.
                        let unready = c >= depth as usize && j >= depth as usize;
                        late += u64::from(unready);
                        if unready && c >= startup {
                            misses += 1;
                            missed[c] = true;
                        }
                        c += 1;
                    }
                    drop(lock);
                    drained(&p, c as u64);
                }
                p.process_hop(&blocks[c], &mut outs[c]);
                c += 1;
                let s = p.stats().snapshot();
                let what = format!("depth {depth}, bursts of {k}: {s:?}");
                assert_eq!((s.misses, s.late, s.gaps), (misses, late, 0), "{what}");
                assert!(outs.iter().flatten().all(|v| v.is_finite()));
                for b in startup..c {
                    let (o, want) = (&outs[b], &x[b * BLOCK - offset..(b + 1) * BLOCK - offset]);
                    if missed[b] {
                        continue;
                    }
                    if missed[b - 1] {
                        let between = |a: f32, w: f32| a * w >= 0.0 && a.abs() <= w.abs() * (1.0 + 1e-6);
                        assert!(o.iter().zip(want).all(|(&a, &w)| between(a, w)), "{what}: block {b}");
                    } else {
                        assert_eq!(&o[..], want, "{what}: block {b}");
                    }
                }
                if depth as usize >= k {
                    assert_eq!(s.misses, 0, "{what}: a depth of at least the burst misses nothing");
                }
            }
        }
    }

    /// A reset drops the old epoch's results, and a full ring is a gap that resyncs with a reset of the model.
    #[test]
    fn old_epochs_are_dropped_and_a_full_ring_resyncs() {
        let gate = Arc::new(Mutex::new(()));
        struct Gated {
            inner: DelayModel,
            gate: Arc<Mutex<()>>,
            resets: Arc<AtomicU64>,
        }
        impl NoiseModel for Gated {
            fn info(&self) -> &ModelInfo {
                self.inner.info()
            }
            fn prepare(&mut self) -> Result<(), StageError> {
                Ok(())
            }
            fn reset(&mut self) {
                self.resets.fetch_add(1, Relaxed);
                self.inner.reset();
            }
            fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
                drop(self.gate.lock());
                self.inner.process_hop(input, output)
            }
        }
        let resets = Arc::new(AtomicU64::new(0));
        let (g, r) = (gate.clone(), resets.clone());
        let make = Box::new(move || Ok(Box::new(Gated { inner: DelayModel::new(480, 960), gate: g, resets: r }) as Box<dyn NoiseModel>));
        let mut p = Pipelined::spawn(make, thread(false), DEFAULT_DEPTH).unwrap();
        let x = ramp(BLOCK);
        let mut out = [0.0f32; BLOCK];
        // Hold the worker: the jobs pile up, the ring fills, and the next ones are gaps.
        let held = gate.lock().unwrap();
        for _ in 0..(RING + 3) {
            p.process_hop(&x, &mut out);
        }
        let s = p.stats().snapshot();
        assert!(s.gaps >= 2, "{s:?}");
        // A reset now: whatever the worker finishes belongs to the old epoch.
        p.reset();
        drop(held);
        std::thread::sleep(Duration::from_millis(50));
        let mut p_out = Vec::new();
        for _ in 0..20 {
            p.process_hop(&x, &mut out);
            p_out.push(out);
            std::thread::sleep(Duration::from_millis(5));
        }
        let s2 = p.stats().snapshot();
        assert_eq!(s2.late, s.late, "old-epoch results are dropped, not counted late: {s2:?}");
        assert!(resets.load(Relaxed) >= 1, "the next job reset the model");
        assert_eq!(s2.misses, s.misses, "after the reset the start-up rule covers the wait, then results arrive: {s2:?}");
        let startup = p.startup_calls() as usize;
        assert_eq!(startup, 3);
        assert!(p_out[..startup].iter().all(|b| b.iter().all(|&v| v == 0.0)), "g = 0 after a stage reset: silence");
        assert!(p_out[startup..].iter().all(|b| b.iter().zip(&x).all(|(a, b)| a == b)), "then the model, exactly (the same block)");
    }

    /// A failed result restarts the model under the start-up rule, in both executors alike.
    #[test]
    fn a_failure_is_an_error_and_a_restart() {
        struct Failing {
            inner: DelayModel,
            hops: u32,
        }
        impl NoiseModel for Failing {
            fn info(&self) -> &ModelInfo {
                self.inner.info()
            }
            fn prepare(&mut self) -> Result<(), StageError> {
                Ok(())
            }
            fn reset(&mut self) {
                self.inner.reset();
            }
            fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
                self.hops += 1;
                self.inner.process_hop(input, output)
            }
            fn take_report(&mut self) -> HopReport {
                HopReport { runs: 1, run_us: 3.0, failed: self.hops == 10 }
            }
        }
        let make = || Box::new(|| Ok(Box::new(Failing { inner: DelayModel::new(480, 960), hops: 0 }) as Box<dyn NoiseModel>)) as _;
        let x = ramp(30 * BLOCK);
        for depth in [1, 2] {
            let mut a = Pipelined::spawn(make(), Executor::Inline, depth).unwrap();
            let mut b = Pipelined::spawn(make(), thread(true), depth).unwrap();
            assert_eq!(run(&mut a, &x), run(&mut b, &x), "depth {depth}");
            let s = a.stats().snapshot();
            assert_eq!((s.errors, s.misses, s.runs), (1, 0, 30));
            assert_eq!((s.run_us_last, s.run_us_p50, s.run_us_max), (3.0, 3.0, 3.0));
        }
    }

    #[test]
    fn a_model_that_fails_to_build_fails_the_spawn() {
        let make = Box::new(|| Err(StageError("no such pack".into())));
        let e = Pipelined::spawn(make, thread(false), DEFAULT_DEPTH).err().unwrap();
        assert_eq!(e.0, "no such pack");
        let info = ModelInfo { id: text("x"), name: text("x"), hop: 480, latency_samples: 960, has_vad: false };
        assert_eq!(info.model_offset(), 480);
    }

    /// The builder stops waiting for a slow build when its hooks give up (the engine's: the run was stopped), within a poll,
    /// and the worker ends by itself once its build is done. A refused scheduling class is reported.
    #[test]
    fn a_stopped_start_gives_up_the_wait_and_a_refusal_is_reported() {
        struct Stopping(Instant);
        impl WorkerHooks for Stopping {
            fn enter(&self) -> WorkerGuard {
                WorkerGuard { guard: Box::new(()), refused: Some(1552) }
            }
            fn give_up(&self) -> bool {
                self.0.elapsed() > Duration::from_millis(150)
            }
        }
        let slow = Box::new(|| {
            std::thread::sleep(Duration::from_millis(700));
            Ok(Box::new(DelayModel::new(480, 960)) as Box<dyn NoiseModel>)
        });
        let t = Instant::now();
        let e = Pipelined::spawn(slow, Executor::Thread { hooks: Arc::new(Stopping(t)), wait_for_results: false }, 2).err().unwrap();
        assert!(e.0.contains("when the start was stopped"), "{}", e.0);
        assert!(t.elapsed() < Duration::from_millis(600), "gave up within a poll: {:?}", t.elapsed());
        let p =
            Pipelined::spawn(delay(480, 960), Executor::Thread { hooks: Arc::new(Stopping(Instant::now())), wait_for_results: false }, 2)
                .unwrap();
        assert_eq!(p.stats().snapshot().worker_sched_refused, Some(1552));
        assert_eq!(Pipelined::spawn(delay(480, 960), thread(false), 2).unwrap().stats().snapshot().worker_sched_refused, None);
    }
}
