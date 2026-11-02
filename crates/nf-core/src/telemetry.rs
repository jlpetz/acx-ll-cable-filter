//! Telemetry the audio thread publishes and the UI reads, through atomics only.
//!
//! The audio thread writes with relaxed stores (one block of skew between fields is harmless), and
//! the UI takes a [`TelemetrySnapshot`] about 20 times a second.

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64};

use crate::atomic::{AtomicF32, AtomicF64};

/// A reference lead target above this (ms) may be longer than the echo path's own delay: the loopback taps the mix ahead
/// of the speakers, whose output latency can be as little as 10-30 ms, and a lead past the echo's delay puts the echo
/// before its reference, which AEC3 can't cancel (its delay estimate then reads about 0). Shown as a warning.
pub const REFERENCE_TARGET_HIGH_MS: f32 = 30.0;

/// Engine run state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Stopped,
    Starting,
    Running,
    /// Stopped by an error; the engine's status holds the message.
    Error,
}

impl EngineState {
    fn to_u8(self) -> u8 {
        match self {
            EngineState::Stopped => 0,
            EngineState::Starting => 1,
            EngineState::Running => 2,
            EngineState::Error => 3,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => EngineState::Starting,
            2 => EngineState::Running,
            3 => EngineState::Error,
            _ => EngineState::Stopped,
        }
    }
}

/// One meter's atomics.
#[derive(Debug, Default)]
pub struct MeterCell {
    pub peak_dbfs: AtomicF32,
    pub rms_dbfs: AtomicF32,
}

/// A meter reading in dBFS.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterSnapshot {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
}

impl MeterCell {
    pub fn store(&self, peak_dbfs: f32, rms_dbfs: f32) {
        self.peak_dbfs.store(peak_dbfs);
        self.rms_dbfs.store(rms_dbfs);
    }
    pub fn load(&self) -> MeterSnapshot {
        MeterSnapshot { peak_dbfs: self.peak_dbfs.load(), rms_dbfs: self.rms_dbfs.load() }
    }
}

/// The latency readout's budget (engine.md, "The latency budget"; TODO 13): where a block's first sample spends its time
/// between its capture and its play-out, in ms. The readout is `mic + wait + queue + render`, block for block. The chain's
/// offset isn't in it: the readout times the block's first output sample, which carries the input from the chain's offset
/// before the block's first input sample, so that sample's own way through the app is the readout plus `chain_ms`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LatencyBudget {
    /// Mic delivery (measured): the first sample's capture time by the mic's stamps to the wake that read it. At least the
    /// packet's length (a block-sized packet: the block's own 10 ms), plus how late the device and its driver delivered it.
    pub mic_ms: f32,
    /// Wait (measured): that wake (its time, taken before the mic's packets are read) to the block's write (its clock reading):
    /// all the audio thread does in between. The wake's own work first: reading the mic's packets, servicing the reference
    /// (the keep-alive's top-up and, with AEC on, the loopback's drain) and the tap's and the recorder's hand-overs. Then the
    /// blocks before this one in the same wake, whole, and this block up to its write: its reference, its chain, the output's
    /// padding. And, when the block's last samples came at a later wake than its first, the wakes between.
    pub wait_ms: f32,
    /// The chain's stream offset (its stages' latency, as they report it): not in the readout.
    pub chain_ms: f32,
    /// Output queue (measured): what was queued ahead of the block's first sample when it was written, in the device's buffer
    /// and our pending samples: the output fill the drift loop holds, interpolated as it sees it (frames written less the
    /// device position now, calibrated to the padding), or the padding before the calibration.
    pub queue_ms: f32,
    /// Render side (derived, the rest of the readout): from the moment the queue would have drained to the play-out the
    /// output's AudioClock predicts. Half a device period when the device takes whole periods (the queue is calibrated to the
    /// middle of the padding's steps), plus what the shared-mode engine and the device hold after taking a sample, as the
    /// clock reports it.
    pub render_ms: f32,
}

impl LatencyBudget {
    /// The parts the readout holds: mic delivery + wait + output queue + render side.
    #[inline]
    pub fn readout_ms(&self) -> f32 {
        self.mic_ms + self.wait_ms + self.queue_ms + self.render_ms
    }

    /// The readout and the chain's offset: the first sample's whole way, from its capture to its play-out.
    #[inline]
    pub fn total_ms(&self) -> f32 {
        self.readout_ms() + self.chain_ms
    }
}

/// The budget's totals over a run: the blocks that had one, and each part's sum (ms). Two snapshots give the means between
/// them ([`Self::means_since`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BudgetTotals {
    pub blocks: u64,
    pub mic_ms: f64,
    pub wait_ms: f64,
    pub chain_ms: f64,
    pub queue_ms: f64,
    pub render_ms: f64,
}

impl BudgetTotals {
    #[inline]
    pub fn add(&mut self, b: &LatencyBudget) {
        self.blocks += 1;
        self.mic_ms += f64::from(b.mic_ms);
        self.wait_ms += f64::from(b.wait_ms);
        self.chain_ms += f64::from(b.chain_ms);
        self.queue_ms += f64::from(b.queue_ms);
        self.render_ms += f64::from(b.render_ms);
    }

    /// Each part's mean over the blocks since `before` (an earlier snapshot's totals); `None` when no block had a budget
    /// since. Totals lower than `before`'s are a new run's, counted from its start.
    pub fn means_since(&self, before: &BudgetTotals) -> Option<LatencyBudget> {
        let base = if self.blocks < before.blocks { BudgetTotals::default() } else { *before };
        let n = (self.blocks - base.blocks) as f64;
        let mean = |now: f64, then: f64| ((now - then) / n) as f32;
        (n > 0.0).then(|| LatencyBudget {
            mic_ms: mean(self.mic_ms, base.mic_ms),
            wait_ms: mean(self.wait_ms, base.wait_ms),
            chain_ms: mean(self.chain_ms, base.chain_ms),
            queue_ms: mean(self.queue_ms, base.queue_ms),
            render_ms: mean(self.render_ms, base.render_ms),
        })
    }
}

/// The output side's events (TODO 9): what the engine's drift loop and coarse corrections did to the stream it played, and the
/// silence an exclusive output's device played where our ring ran short, so that a move of the output path's delay can be told
/// from the devices' own (a cable that inserts silence after taking our samples moves the delay too, and none of these count
/// it). Counted from a run's first block; [`Self::between`] gives a span's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputEvents {
    /// Underruns (`render_underruns`): the device found dry, a deficit, or an exclusive output's ring short of a period at an
    /// event; once a block at most.
    pub underruns: u64,
    /// Silence an exclusive output's device played in our stream where its ring ran short at an event, or for the periods an
    /// event answered late (`output_silence_frames`). Each frame plays every later sample a frame later, until the drift loop
    /// slips it back out. 0 for a shared output, whose device engine plays its own silence outside our stream.
    pub silence_frames: u64,
    /// Deficits: blocks at which zeros were written to top the device up to the target (each counts as an underrun too), and
    /// the zeros written. Each zero lengthens the output path by a sample until the drift loop slips it back out.
    pub deficits: u64,
    pub zero_frames: u64,
    /// Coarse drops (`output_coarse_drops`): whole blocks dropped to pull an overfull buffer back to its target (one per event),
    /// and pending samples that overflowed; and the samples dropped. Each shortens the path by a sample.
    pub coarse_drops: u64,
    pub dropped_frames: u64,
    /// The drift loop's slips (one sample a block at most), and their net: samples inserted less samples dropped.
    pub slips: u64,
    pub slip_net: i64,
    /// Times the fill's calibration to the device's padding started over from the next readings (the device ran dry, or its
    /// clock jumped): the drift loop holds the fill it then measures.
    pub reanchors: u64,
}

impl OutputEvents {
    /// The run's so far, as a snapshot has them.
    pub fn of(t: &TelemetrySnapshot) -> Self {
        Self {
            underruns: t.render_underruns,
            silence_frames: t.output_silence_frames,
            deficits: t.output_deficits,
            zero_frames: t.output_zero_frames,
            coarse_drops: t.output_coarse_drops,
            dropped_frames: t.output_dropped_frames,
            slips: t.output_slips,
            slip_net: t.output_slip_net,
            reanchors: t.output_reanchors,
        }
    }

    /// The events between two snapshots. A restart between them starts the counts again: then those since it.
    pub fn between(before: &TelemetrySnapshot, after: &TelemetrySnapshot) -> Self {
        let (b, a) = (Self::of(before), Self::of(after));
        if after.blocks < before.blocks {
            return a;
        }
        Self {
            underruns: a.underruns.saturating_sub(b.underruns),
            silence_frames: a.silence_frames.saturating_sub(b.silence_frames),
            deficits: a.deficits.saturating_sub(b.deficits),
            zero_frames: a.zero_frames.saturating_sub(b.zero_frames),
            coarse_drops: a.coarse_drops.saturating_sub(b.coarse_drops),
            dropped_frames: a.dropped_frames.saturating_sub(b.dropped_frames),
            slips: a.slips.saturating_sub(b.slips),
            slip_net: a.slip_net.wrapping_sub(b.slip_net),
            reanchors: a.reanchors.saturating_sub(b.reanchors),
        }
    }

    /// Samples the engine's own corrections and an exclusive device's silence added to the output path (zeros, silence and
    /// inserted slips, less what it dropped): the path moved by this much, until the drift loop slipped it back.
    pub fn net_frames(&self) -> i64 {
        (self.zero_frames as i64)
            .wrapping_add(self.silence_frames as i64)
            .wrapping_sub(self.dropped_frames as i64)
            .wrapping_add(self.slip_net)
    }

    /// Anything but slips: an underrun, silence, zeros, a drop or a re-anchor.
    pub fn any_correction(&self) -> bool {
        self.underruns > 0 || self.silence_frames > 0 || self.deficits > 0 || self.coarse_drops > 0 || self.reanchors > 0
    }
    /// In one phrase: "no underruns, zeros, drops or re-anchors; 37 slips (net -5 samples)", or the ones there were: "1 underrun
    /// (2.0 ms of silence played), 1 deficit (12.0 ms of zeros), 1 coarse drop (10.0 ms dropped), 1 re-anchor; 37 slips (net -5
    /// samples)". The silence an exclusive device played and the zeros lengthen the output path and the drops shorten it, until
    /// the drift loop slips it back; a move of the path these don't show is the devices' own.
    pub fn text(&self) -> String {
        let ms = |frames: u64| format!("{:.1}", frames as f64 * 1000.0 / f64::from(crate::SAMPLE_RATE));
        let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        match (self.underruns, self.silence_frames) {
            (0, 0) => {}
            (n, 0) => parts.push(plural(n, "underrun", "underruns")),
            (n, f) => parts.push(format!("{} ({} ms of silence played)", plural(n, "underrun", "underruns"), ms(f))),
        }
        if self.deficits > 0 {
            parts.push(format!("{} ({} ms of zeros)", plural(self.deficits, "deficit", "deficits"), ms(self.zero_frames)));
        }
        if self.coarse_drops > 0 || self.dropped_frames > 0 {
            parts.push(format!("{} ({} ms dropped)", plural(self.coarse_drops, "coarse drop", "coarse drops"), ms(self.dropped_frames)));
        }
        if self.reanchors > 0 {
            parts.push(plural(self.reanchors, "re-anchor", "re-anchors"));
        }
        let corrections = if parts.is_empty() { "no underruns, zeros, drops or re-anchors".to_owned() } else { parts.join(", ") };
        let slips = match self.slips {
            0 => "no slips".to_owned(),
            n => format!("{} (net {:+} samples)", plural(n, "slip", "slips"), self.slip_net),
        };
        format!("{corrections}; {slips}")
    }
}

/// A [`LatencyBudget`]'s atomics.
#[derive(Debug, Default)]
pub struct BudgetCell {
    pub mic_ms: AtomicF32,
    pub wait_ms: AtomicF32,
    pub chain_ms: AtomicF32,
    pub queue_ms: AtomicF32,
    pub render_ms: AtomicF32,
}

impl BudgetCell {
    pub fn store(&self, b: &LatencyBudget) {
        self.mic_ms.store(b.mic_ms);
        self.wait_ms.store(b.wait_ms);
        self.chain_ms.store(b.chain_ms);
        self.queue_ms.store(b.queue_ms);
        self.render_ms.store(b.render_ms);
    }
    pub fn load(&self) -> LatencyBudget {
        LatencyBudget {
            mic_ms: self.mic_ms.load(),
            wait_ms: self.wait_ms.load(),
            chain_ms: self.chain_ms.load(),
            queue_ms: self.queue_ms.load(),
            render_ms: self.render_ms.load(),
        }
    }
}

/// [`BudgetTotals`]' atomics.
#[derive(Debug, Default)]
pub struct BudgetTotalsCell {
    pub blocks: AtomicU64,
    pub mic_ms: AtomicF64,
    pub wait_ms: AtomicF64,
    pub chain_ms: AtomicF64,
    pub queue_ms: AtomicF64,
    pub render_ms: AtomicF64,
}

impl BudgetTotalsCell {
    pub fn store(&self, t: &BudgetTotals) {
        self.blocks.store(t.blocks, Relaxed);
        self.mic_ms.store(t.mic_ms);
        self.wait_ms.store(t.wait_ms);
        self.chain_ms.store(t.chain_ms);
        self.queue_ms.store(t.queue_ms);
        self.render_ms.store(t.render_ms);
    }
    pub fn load(&self) -> BudgetTotals {
        BudgetTotals {
            blocks: self.blocks.load(Relaxed),
            mic_ms: self.mic_ms.load(),
            wait_ms: self.wait_ms.load(),
            chain_ms: self.chain_ms.load(),
            queue_ms: self.queue_ms.load(),
            render_ms: self.render_ms.load(),
        }
    }
}

/// The shared telemetry block. One per engine; `Arc`-shared with the UI.
#[derive(Debug, Default)]
pub struct Telemetry {
    pub state: AtomicU8,
    /// Blocks processed since start.
    pub blocks: AtomicU64,
    /// The once-a-second publications since start (stored last, with `Release`): a reader that waits for it to move gets
    /// counters that include everything up to then ([`Telemetry::publishes`]).
    pub publishes: AtomicU64,
    pub input: MeterCell,
    pub output: MeterCell,
    /// Latched when the raw input clips; cleared by [`Telemetry::reset_clip`].
    pub clip_latched: AtomicBool,
    /// The raw input came within 0.1 dB of full scale in the last second.
    pub near_clip: AtomicBool,
    /// What the noise stage took off this block, measured on its output (floor and gate included), in dB (<= 0).
    pub noise_reduction_db: AtomicF32,
    pub speech_probability: AtomicF32,
    pub latency_valid: AtomicBool,
    pub latency_ms_median: AtomicF32,
    pub latency_ms_p95: AtomicF32,
    /// The readout's budget over the same last 100 blocks (about a second; valid with `latency_valid`): each part's mean (the
    /// means add up to the readout's mean, not its median), and each part's own 95th percentile (those don't add up).
    pub budget_mean: BudgetCell,
    pub budget_p95: BudgetCell,
    /// The budget over the run so far, for the means between two snapshots (published each second).
    pub budget_totals: BudgetTotalsCell,
    pub output_drift_ppm: AtomicF32,
    /// The reference loop's drift estimate: positive when the loopback's clock runs fast against the mic's.
    pub reference_drift_ppm: AtomicF32,
    pub output_fill_ms: AtomicF32,
    /// The reference's lead (low-passed): how long before the mic block's capture the reference served with it was
    /// tapped. Its loop holds it at `reference_target_ms` (nf-engine `reference.rs`).
    pub reference_fill_ms: AtomicF32,
    pub output_slips: AtomicU64,
    pub reference_slips: AtomicU64,
    /// Capture packets flagged as discontinuous, or missing by device position.
    pub capture_glitches: AtomicU64,
    /// Times the output cable's buffer ran dry.
    pub render_underruns: AtomicU64,
    /// Blocks whose reference was (partly) zeros for samples that hadn't arrived (discarded when they did, so the echo
    /// path doesn't move).
    pub reference_gaps: AtomicU64,
    /// Whole-chain processing time per block over the last second, in µs.
    pub block_us_p50: AtomicF32,
    pub block_us_p99: AtomicF32,
    pub block_us_max: AtomicF32,
    pub aec_us_p99: AtomicF32,
    pub noise_us_p99: AtomicF32,
    pub aec_delay_ms: AtomicF32,
    pub aec_erle_db: AtomicF32,
    /// Allocations on the audio thread after warm-up that fell back to the system heap (must stay 0).
    pub rt_heap_allocations: AtomicU64,
    /// Allocations on the audio thread in the last second served from the locked pool (O(1) TLSF;
    /// informational: sonora 0.2.0 makes about 43 per block until its PR #40).
    pub rt_pool_allocations_per_s: AtomicU64,
    /// Mic event waits that timed out (100 ms with no packet).
    pub stalls: AtomicU64,
    /// Coarse corrections of the output: times whole blocks were dropped to pull an overfull buffer
    /// back to its target (one per event, however many blocks it dropped). Of the reference: jumps of
    /// its timeline after the start-up, either way (a broken timeline undone, a change of its lead
    /// target, or without stamps a step of the count put back).
    pub output_coarse_drops: AtomicU64,
    pub reference_coarse_drops: AtomicU64,
    /// The output's other events ([`OutputEvents`]): deficits and their zeros, the samples coarse drops took, the slips' net
    /// (inserted less dropped), and the fill calibration's re-anchors.
    pub output_deficits: AtomicU64,
    pub output_zero_frames: AtomicU64,
    pub output_dropped_frames: AtomicU64,
    pub output_slip_net: AtomicI64,
    pub output_reanchors: AtomicU64,
    /// Silence an exclusive output's device played in our stream where its ring ran short ([`OutputEvents::silence_frames`]).
    pub output_silence_frames: AtomicU64,
    /// Latency-test tap blocks lost because the tap ring was full.
    pub tap_overflows: AtomicU64,
    /// The targets the drift loops hold, in ms (0 while stopped or without a reference): the output's
    /// fill, and the reference's lead (both published each second: they can move).
    pub output_target_ms: AtomicF32,
    pub reference_target_ms: AtomicF32,
    /// The reference's lead is timed by both streams' QPC stamps; false: by when their packets were delivered (a stream
    /// without stamps, or whose stamps scatter too widely), which can't slip at the drift (nf-engine `reference.rs`).
    pub reference_stamped: AtomicBool,
    pub mic_period_ms: AtomicF32,
    pub output_period_ms: AtomicF32,
    /// The run's mic and output streams are in WASAPI exclusive mode (asked for in `IoConfig`, the default, and granted; a
    /// refused one runs shared, with a note), set at the start and cleared at the stop.
    pub mic_exclusive: AtomicBool,
    pub output_exclusive: AtomicBool,
    /// The output's target adapts to the delivery (`IoConfig::adaptive_output`): `output_target_ms` moves.
    pub output_adaptive: AtomicBool,
    /// The adaptive target holds for a trial, the window that proves the delivery before it first comes down: its lowering is
    /// still to come (nf-engine `out_target.rs`; `--settle` waits past it). Published each second.
    pub output_target_on_trial: AtomicBool,
    /// The noise model runs on an inference worker (a model pack, TODO 6): the `inference_*` counters below are its
    /// (`nf_core::noise::InferenceStats`), published once a second.
    pub inference_active: AtomicBool,
    pub inference_runs: AtomicU64,
    /// Blocks whose result came too late: the substitute (the delayed input, gain-matched) played instead.
    pub inference_misses: AtomicU64,
    pub inference_late: AtomicU64,
    pub inference_gaps: AtomicU64,
    pub inference_errors: AtomicU64,
    /// One inference's time on the worker, in µs, over its last 100 runs, and the most since the start.
    pub inference_run_us_p50: AtomicF32,
    pub inference_run_us_p99: AtomicF32,
    pub inference_run_us_max: AtomicF32,
    /// The worker's Rust allocations since it was ready (from the locked pool), and those that fell back to the heap.
    pub inference_worker_allocs: AtomicU64,
    pub inference_worker_fallbacks: AtomicU64,
    /// The audio thread's wakes that processed 1, 2, 3, and 4 or more blocks (TODO 6, 13): what a model pack's inference
    /// worker, `depth` blocks behind, has to keep up with. A wake of `k` blocks costs it `max(0, k - depth)` misses
    /// ([`WakeBlocks::misses_at`]).
    pub wake_blocks: [AtomicU64; 4],
}

/// [`WakeBlocks::delivery`]'s thresholds: the wakes it needs (10 s of a block a wake), and the blocks per miss at depth 1
/// that still count as a block at a time.
pub const DELIVERY_MIN_WAKES: u64 = 1_000;
pub const DELIVERY_STEADY_BLOCKS: u64 = 500;

/// How many wakes processed 1, 2, 3, and 4 or more blocks ([`Telemetry::wake_blocks`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeBlocks(pub [u64; 4]);

impl WakeBlocks {
    /// The misses a model pack's worker `depth` blocks behind would have had at these wakes if it caught up between them
    /// (a wake of 4 or more counted as 4: a lower bound).
    pub fn misses_at(&self, depth: u32) -> u64 {
        self.0.iter().enumerate().map(|(i, &n)| n * (i as u64 + 1).saturating_sub(u64::from(depth))).sum()
    }

    /// Wakes that processed more than one block.
    pub fn bursts(&self) -> u64 {
        self.0[1..].iter().sum()
    }

    /// The wakes counted, and the blocks they processed (a wake of 4 or more counted as 4).
    pub fn wakes(&self) -> u64 {
        self.0.iter().sum()
    }

    pub fn blocks(&self) -> u64 {
        self.0.iter().enumerate().map(|(i, &n)| n * (i as u64 + 1)).sum()
    }

    /// How the mic delivered, for the automatic model depth (TODO 5; `config::ProcessingConfig::model_depth_for`): unknown
    /// before [`DELIVERY_MIN_WAKES`] wakes; a block at a time when a worker one block behind would have missed at most one
    /// block in [`DELIVERY_STEADY_BLOCKS`] (each miss plays a block of the unfiltered mic: one every 5 s at most); else in
    /// bursts. The cables' shared mic, a fifth to a third of its wakes two blocks, is bursts by far; the exclusive mic, and
    /// steady delivery, have none but a stall's.
    pub fn delivery(&self) -> crate::config::MicDelivery {
        use crate::config::MicDelivery;
        if self.wakes() < DELIVERY_MIN_WAKES {
            MicDelivery::Unknown
        } else if self.misses_at(1).saturating_mul(DELIVERY_STEADY_BLOCKS) <= self.blocks() {
            MicDelivery::OneAtATime
        } else {
            MicDelivery::Bursts
        }
    }

    /// From `earlier` to these (each count's growth).
    pub fn since(&self, earlier: &WakeBlocks) -> WakeBlocks {
        WakeBlocks(std::array::from_fn(|i| self.0[i].saturating_sub(earlier.0[i])))
    }

    pub fn add(&mut self, other: &WakeBlocks) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a += b;
        }
    }
}

/// A plain copy of [`Telemetry`] for the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TelemetrySnapshot {
    pub state: EngineState,
    pub blocks: u64,
    /// The once-a-second publications since start: a snapshot whose count moved past an earlier one's has a later second's
    /// counters ([`Telemetry::publishes`]).
    pub publishes: u64,
    pub input: MeterSnapshot,
    pub output: MeterSnapshot,
    pub clip_latched: bool,
    pub near_clip: bool,
    pub noise_reduction_db: f32,
    pub speech_probability: f32,
    pub latency_valid: bool,
    pub latency_ms_median: f32,
    pub latency_ms_p95: f32,
    pub budget_mean: LatencyBudget,
    pub budget_p95: LatencyBudget,
    pub budget_totals: BudgetTotals,
    pub output_drift_ppm: f32,
    pub reference_drift_ppm: f32,
    pub output_fill_ms: f32,
    pub reference_fill_ms: f32,
    pub output_slips: u64,
    pub reference_slips: u64,
    pub capture_glitches: u64,
    pub render_underruns: u64,
    pub reference_gaps: u64,
    pub block_us_p50: f32,
    pub block_us_p99: f32,
    pub block_us_max: f32,
    pub aec_us_p99: f32,
    pub noise_us_p99: f32,
    pub aec_delay_ms: f32,
    pub aec_erle_db: f32,
    pub rt_heap_allocations: u64,
    pub rt_pool_allocations_per_s: u64,
    pub stalls: u64,
    pub output_coarse_drops: u64,
    pub reference_coarse_drops: u64,
    pub output_deficits: u64,
    pub output_zero_frames: u64,
    pub output_dropped_frames: u64,
    pub output_slip_net: i64,
    pub output_reanchors: u64,
    pub output_silence_frames: u64,
    pub tap_overflows: u64,
    pub output_target_ms: f32,
    pub reference_target_ms: f32,
    pub reference_stamped: bool,
    pub mic_period_ms: f32,
    pub output_period_ms: f32,
    pub mic_exclusive: bool,
    pub output_exclusive: bool,
    pub output_adaptive: bool,
    pub output_target_on_trial: bool,
    pub inference_active: bool,
    pub inference_runs: u64,
    pub inference_misses: u64,
    pub inference_late: u64,
    pub inference_gaps: u64,
    pub inference_errors: u64,
    pub inference_run_us_p50: f32,
    pub inference_run_us_p99: f32,
    pub inference_run_us_max: f32,
    pub inference_worker_allocs: u64,
    pub inference_worker_fallbacks: u64,
    pub wake_blocks: WakeBlocks,
}

impl Telemetry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_state(&self, s: EngineState) {
        self.state.store(s.to_u8(), Relaxed);
    }

    pub fn state(&self) -> EngineState {
        EngineState::from_u8(self.state.load(Relaxed))
    }

    /// Clears the clip latch (the UI's "reset" click).
    pub fn reset_clip(&self) {
        self.clip_latched.store(false, Relaxed);
    }

    /// Whether the raw input clipped since the latch was last cleared, clearing it: a reader that shows the latch (`run`'s
    /// status line, once a second) loses no clip between its readings.
    pub fn take_clip(&self) -> bool {
        self.clip_latched.swap(false, Relaxed)
    }

    /// The once-a-second publications so far, read with `Acquire`: once it has moved past an earlier reading, a snapshot taken
    /// after this has every counter as of that publication.
    pub fn publishes(&self) -> u64 {
        self.publishes.load(Acquire)
    }

    /// Copies an inference worker's stats in (once a second, before [`Telemetry::published`]), or marks none.
    pub fn publish_inference(&self, stats: Option<&crate::noise::InferenceStats>) {
        self.inference_active.store(stats.is_some(), Relaxed);
        let Some(s) = stats else { return };
        let s = s.snapshot();
        self.inference_runs.store(s.runs, Relaxed);
        self.inference_misses.store(s.misses, Relaxed);
        self.inference_late.store(s.late, Relaxed);
        self.inference_gaps.store(s.gaps, Relaxed);
        self.inference_errors.store(s.errors, Relaxed);
        self.inference_run_us_p50.store(s.run_us_p50);
        self.inference_run_us_p99.store(s.run_us_p99);
        self.inference_run_us_max.store(s.run_us_max);
        self.inference_worker_allocs.store(s.worker_pool_allocs, Relaxed);
        self.inference_worker_fallbacks.store(s.worker_fallbacks, Relaxed);
    }

    /// Counts a wake that processed `blocks` blocks (none: nothing). The audio thread, once a wake. Real-time.
    #[inline]
    pub fn count_wake_blocks(&self, blocks: usize) {
        if blocks > 0 {
            self.wake_blocks[blocks.min(4) - 1].fetch_add(1, Relaxed);
        }
    }

    /// Marks a publication done: after every counter of it is stored (the audio thread, once a second).
    pub fn published(&self) {
        self.publishes.fetch_add(1, Release);
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        // First, with `Acquire`: the counters read after it are at least that publication's.
        let publishes = self.publishes();
        TelemetrySnapshot {
            state: self.state(),
            blocks: self.blocks.load(Relaxed),
            publishes,
            input: self.input.load(),
            output: self.output.load(),
            clip_latched: self.clip_latched.load(Relaxed),
            near_clip: self.near_clip.load(Relaxed),
            noise_reduction_db: self.noise_reduction_db.load(),
            speech_probability: self.speech_probability.load(),
            latency_valid: self.latency_valid.load(Relaxed),
            latency_ms_median: self.latency_ms_median.load(),
            latency_ms_p95: self.latency_ms_p95.load(),
            budget_mean: self.budget_mean.load(),
            budget_p95: self.budget_p95.load(),
            budget_totals: self.budget_totals.load(),
            output_drift_ppm: self.output_drift_ppm.load(),
            reference_drift_ppm: self.reference_drift_ppm.load(),
            output_fill_ms: self.output_fill_ms.load(),
            reference_fill_ms: self.reference_fill_ms.load(),
            output_slips: self.output_slips.load(Relaxed),
            reference_slips: self.reference_slips.load(Relaxed),
            capture_glitches: self.capture_glitches.load(Relaxed),
            render_underruns: self.render_underruns.load(Relaxed),
            reference_gaps: self.reference_gaps.load(Relaxed),
            block_us_p50: self.block_us_p50.load(),
            block_us_p99: self.block_us_p99.load(),
            block_us_max: self.block_us_max.load(),
            aec_us_p99: self.aec_us_p99.load(),
            noise_us_p99: self.noise_us_p99.load(),
            aec_delay_ms: self.aec_delay_ms.load(),
            aec_erle_db: self.aec_erle_db.load(),
            rt_heap_allocations: self.rt_heap_allocations.load(Relaxed),
            rt_pool_allocations_per_s: self.rt_pool_allocations_per_s.load(Relaxed),
            stalls: self.stalls.load(Relaxed),
            output_coarse_drops: self.output_coarse_drops.load(Relaxed),
            reference_coarse_drops: self.reference_coarse_drops.load(Relaxed),
            output_deficits: self.output_deficits.load(Relaxed),
            output_zero_frames: self.output_zero_frames.load(Relaxed),
            output_dropped_frames: self.output_dropped_frames.load(Relaxed),
            output_slip_net: self.output_slip_net.load(Relaxed),
            output_reanchors: self.output_reanchors.load(Relaxed),
            output_silence_frames: self.output_silence_frames.load(Relaxed),
            tap_overflows: self.tap_overflows.load(Relaxed),
            output_target_ms: self.output_target_ms.load(),
            reference_target_ms: self.reference_target_ms.load(),
            reference_stamped: self.reference_stamped.load(Relaxed),
            mic_period_ms: self.mic_period_ms.load(),
            output_period_ms: self.output_period_ms.load(),
            mic_exclusive: self.mic_exclusive.load(Relaxed),
            output_exclusive: self.output_exclusive.load(Relaxed),
            output_adaptive: self.output_adaptive.load(Relaxed),
            output_target_on_trial: self.output_target_on_trial.load(Relaxed),
            inference_active: self.inference_active.load(Relaxed),
            inference_runs: self.inference_runs.load(Relaxed),
            inference_misses: self.inference_misses.load(Relaxed),
            inference_late: self.inference_late.load(Relaxed),
            inference_gaps: self.inference_gaps.load(Relaxed),
            inference_errors: self.inference_errors.load(Relaxed),
            inference_run_us_p50: self.inference_run_us_p50.load(),
            inference_run_us_p99: self.inference_run_us_p99.load(),
            inference_run_us_max: self.inference_run_us_max.load(),
            inference_worker_allocs: self.inference_worker_allocs.load(Relaxed),
            inference_worker_fallbacks: self.inference_worker_fallbacks.load(Relaxed),
            wake_blocks: WakeBlocks(std::array::from_fn(|i| self.wake_blocks[i].load(Relaxed))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(mic_ms: f32, wait_ms: f32, chain_ms: f32, queue_ms: f32, render_ms: f32) -> LatencyBudget {
        LatencyBudget { mic_ms, wait_ms, chain_ms, queue_ms, render_ms }
    }

    /// The wakes' block counts, and the misses a worker at each depth would have had at them (TODO 6).
    #[test]
    fn wake_blocks_count_the_bursts_and_what_a_depth_would_miss() {
        let tel = Telemetry::new();
        for k in [1usize, 1, 2, 0, 3, 6, 2] {
            tel.count_wake_blocks(k);
        }
        let w = tel.snapshot().wake_blocks;
        assert_eq!(w, WakeBlocks([2, 2, 1, 1]));
        assert_eq!((w.bursts(), w.misses_at(1), w.misses_at(2), w.misses_at(3), w.misses_at(4)), (4, 2 + 2 + 3, 1 + 2, 1, 0));
        let mut sum = w.since(&WakeBlocks([1, 0, 0, 0]));
        assert_eq!(sum, WakeBlocks([1, 2, 1, 1]));
        sum.add(&WakeBlocks([1, 1, 1, 1]));
        assert_eq!(sum, WakeBlocks([2, 3, 2, 2]));
        assert_eq!((w.wakes(), w.blocks()), (6, 2 + 4 + 3 + 4));
    }

    /// The automatic depth's reading of the wakes (TODO 5): too few is unknown; a block a wake with a rare stall's pair is a
    /// block at a time; the cables' shared bursts (a pair every third wake) and one pair in a hundred are bursts.
    #[test]
    fn the_wakes_say_how_the_mic_delivers() {
        use crate::config::MicDelivery;
        assert_eq!(WakeBlocks([999, 0, 0, 0]).delivery(), MicDelivery::Unknown);
        assert_eq!(WakeBlocks([1_000, 0, 0, 0]).delivery(), MicDelivery::OneAtATime);
        assert_eq!(WakeBlocks([30_000, 30, 2, 1]).delivery(), MicDelivery::OneAtATime, "a pair every 10 s or so");
        assert_eq!(WakeBlocks([20_000, 10_000, 0, 0]).delivery(), MicDelivery::Bursts, "the cables' shared mic");
        assert_eq!(WakeBlocks([9_900, 100, 0, 0]).delivery(), MicDelivery::Bursts, "a pair every 100 wakes");
        assert_eq!(WakeBlocks([0, 1_000, 0, 0]).delivery(), MicDelivery::Bursts, "a 20 ms period: two blocks every wake");
    }

    /// The readout is the four parts on the sample's way; the chain's offset comes on top of it.
    #[test]
    fn the_readout_is_the_parts_less_the_chain() {
        let b = budget(16.0, 0.25, 10.0, 22.0, 23.5);
        assert_eq!(b.readout_ms(), 61.75);
        assert_eq!(b.total_ms(), 71.75);
    }

    /// Two snapshots' totals give each part's mean between them; a new run's (lower) totals count from its start, and no
    /// block between them gives none.
    #[test]
    fn totals_give_the_means_between_two_snapshots() {
        let mut t = BudgetTotals::default();
        t.add(&budget(10.0, 0.0, 10.0, 22.0, 20.0));
        let before = t;
        assert_eq!(t.means_since(&before), None);
        t.add(&budget(20.0, 1.0, 10.0, 12.0, 20.0));
        t.add(&budget(10.0, 0.0, 10.0, 22.0, 21.0));
        assert_eq!(t.means_since(&before), Some(budget(15.0, 0.5, 10.0, 17.0, 20.5)));
        assert_eq!(t.means_since(&BudgetTotals::default()), Some(budget(40.0 / 3.0, 1.0 / 3.0, 10.0, 56.0 / 3.0, 61.0 / 3.0)));
        let mut restarted = BudgetTotals::default();
        restarted.add(&budget(12.0, 0.0, 0.0, 22.0, 20.0));
        assert_eq!(restarted.means_since(&t), Some(budget(12.0, 0.0, 0.0, 22.0, 20.0)), "from the new run's start");

        let tel = Telemetry::new();
        tel.budget_totals.store(&t);
        tel.budget_mean.store(&budget(1.0, 2.0, 3.0, 4.0, 5.0));
        let s = tel.snapshot();
        assert_eq!((s.budget_totals, s.budget_mean), (t, budget(1.0, 2.0, 3.0, 4.0, 5.0)));
    }

    /// The output's events between two snapshots, the samples they moved the path by, and a restart between the snapshots.
    #[test]
    fn output_events_between_two_snapshots() {
        let tel = Telemetry::new();
        tel.blocks.store(100, Relaxed);
        tel.output_slips.store(10, Relaxed);
        tel.output_slip_net.store(-4, Relaxed);
        let before = tel.snapshot();
        assert!(!OutputEvents::of(&before).any_correction());
        tel.blocks.store(200, Relaxed);
        tel.render_underruns.store(1, Relaxed);
        tel.output_deficits.store(1, Relaxed);
        tel.output_zero_frames.store(576, Relaxed);
        tel.output_coarse_drops.store(1, Relaxed);
        tel.output_dropped_frames.store(480, Relaxed);
        tel.output_slips.store(13, Relaxed);
        tel.output_slip_net.store(-7, Relaxed);
        tel.output_reanchors.store(1, Relaxed);
        tel.output_silence_frames.store(96, Relaxed);
        let after = tel.snapshot();
        let e = OutputEvents::between(&before, &after);
        let expected = OutputEvents {
            underruns: 1,
            silence_frames: 96,
            deficits: 1,
            zero_frames: 576,
            coarse_drops: 1,
            dropped_frames: 480,
            slips: 3,
            slip_net: -3,
            reanchors: 1,
        };
        assert_eq!(e, expected);
        assert_eq!(e.net_frames(), 576 + 96 - 480 - 3);
        assert!(e.any_correction());
        // A restart between them: the counts since it.
        let restarted = TelemetrySnapshot { blocks: 50, output_slips: 2, ..TelemetrySnapshot::default() };
        assert_eq!(OutputEvents::between(&after, &restarted).slips, 2);
    }

    #[test]
    fn output_events_read_as_what_happened() {
        let none = OutputEvents { slips: 37, slip_net: -5, ..Default::default() };
        assert_eq!(none.text(), "no underruns, zeros, drops or re-anchors; 37 slips (net -5 samples)");
        assert_eq!(OutputEvents::default().text(), "no underruns, zeros, drops or re-anchors; no slips");
        let some = OutputEvents {
            underruns: 1,
            silence_frames: 96,
            deficits: 1,
            zero_frames: 576,
            coarse_drops: 2,
            dropped_frames: 960,
            slips: 1,
            slip_net: 1,
            reanchors: 1,
        };
        assert_eq!(
            some.text(),
            "1 underrun (2.0 ms of silence played), 1 deficit (12.0 ms of zeros), 2 coarse drops (20.0 ms dropped), 1 re-anchor; \
             1 slip (net +1 samples)"
        );
        let two = OutputEvents { underruns: 2, ..Default::default() };
        assert_eq!(two.text(), "2 underruns; no slips");
    }

    /// Taking the clip latch clears it, so the next reading shows only a new clip.
    #[test]
    fn taking_the_clip_latch_clears_it() {
        let tel = Telemetry::new();
        assert!(!tel.take_clip());
        tel.clip_latched.store(true, Relaxed);
        assert!(tel.snapshot().clip_latched);
        assert!(tel.take_clip());
        assert!(!tel.take_clip() && !tel.snapshot().clip_latched);
    }
}
