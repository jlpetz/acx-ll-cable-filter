//! Telemetry the audio thread publishes and the UI reads, through atomics only.
//!
//! The audio thread writes with relaxed stores (one block of skew between fields is harmless), and
//! the UI takes a [`TelemetrySnapshot`] about 20 times a second.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};

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
    /// The run's mic and output streams are in WASAPI exclusive mode (asked for in `IoConfig` and granted; a refused one
    /// runs shared, with a warning), set at the start and cleared at the stop.
    pub mic_exclusive: AtomicBool,
    pub output_exclusive: AtomicBool,
    /// The output's target adapts to the delivery (`IoConfig::adaptive_output`): `output_target_ms` moves.
    pub output_adaptive: AtomicBool,
}

/// A plain copy of [`Telemetry`] for the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TelemetrySnapshot {
    pub state: EngineState,
    pub blocks: u64,
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
    pub tap_overflows: u64,
    pub output_target_ms: f32,
    pub reference_target_ms: f32,
    pub reference_stamped: bool,
    pub mic_period_ms: f32,
    pub output_period_ms: f32,
    pub mic_exclusive: bool,
    pub output_exclusive: bool,
    pub output_adaptive: bool,
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

    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            state: self.state(),
            blocks: self.blocks.load(Relaxed),
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
            tap_overflows: self.tap_overflows.load(Relaxed),
            output_target_ms: self.output_target_ms.load(),
            reference_target_ms: self.reference_target_ms.load(),
            reference_stamped: self.reference_stamped.load(Relaxed),
            mic_period_ms: self.mic_period_ms.load(),
            output_period_ms: self.output_period_ms.load(),
            mic_exclusive: self.mic_exclusive.load(Relaxed),
            output_exclusive: self.output_exclusive.load(Relaxed),
            output_adaptive: self.output_adaptive.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(mic_ms: f32, wait_ms: f32, chain_ms: f32, queue_ms: f32, render_ms: f32) -> LatencyBudget {
        LatencyBudget { mic_ms, wait_ms, chain_ms, queue_ms, render_ms }
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
}
