//! The latency readout's budget (engine.md, "The latency budget"; TODO 13): the readout of each block split into where its
//! first sample spent the time, so the parts add up to the readout block for block.
//!
//! - [`WakeLog`] remembers which wake delivered the mic FIFO's samples, so a block's first sample can be timed to the wake
//!   that read it (mic delivery) and from there to the block's write (wait).
//! - [`split`] makes one block's [`LatencyBudget`] from its times and the output queue ahead of it: the render side is what's
//!   left of the readout.
//! - [`BudgetStats`] keeps the last 100 blocks' parts (the readout's own window) and the run's totals, and publishes them once
//!   a second.
//!
//! All of it runs on the audio thread: fixed arrays, no allocation, no panic paths, O(1) per block (the wake log's trimming
//! is amortised: each wake is popped once).

use nf_core::SAMPLE_RATE;
use nf_core::latency::stats::RollingStats;
use nf_core::telemetry::{BudgetTotals, LatencyBudget, Telemetry};

/// Wakes the log remembers: the mic FIFO holds 200 ms, so 256 cover it down to a period of 0.8 ms.
const WAKES: usize = 256;
/// ms per sample at the app's rate.
const MS_PER_SAMPLE: f64 = 1000.0 / SAMPLE_RATE as f64;

/// The wakes that delivered the mic FIFO's samples: each one's first sample (the FIFO's absolute index) and its time (hns).
/// Oldest first; trimmed as the blocks take the samples.
pub(crate) struct WakeLog {
    wakes: Box<[(u64, f64); WAKES]>,
    /// The oldest wake's slot, and the wakes held.
    first: usize,
    len: usize,
}

impl WakeLog {
    pub(crate) fn new() -> Self {
        Self { wakes: Box::new([(0, 0.0); WAKES]), first: 0, len: 0 }
    }

    #[inline]
    fn at(&self, k: usize) -> Option<(u64, f64)> {
        self.wakes.get((self.first + k) % WAKES).copied()
    }

    /// A wake at `hns` delivered the samples from absolute index `from` on (`from` past the last wake's). With the log full
    /// they count as the newest wake's: that wake's time is earlier than theirs, so their delivery reads short and their wait
    /// long by the wakes between (the sum doesn't change).
    #[inline]
    pub(crate) fn delivered(&mut self, from: u64, hns: f64) {
        if self.len == WAKES {
            return;
        }
        if let Some(slot) = self.wakes.get_mut((self.first + self.len) % WAKES) {
            *slot = (from, hns);
            self.len += 1;
        }
    }

    /// The time of the wake that delivered sample `index` (the FIFO's head: it only moves on), forgetting the wakes before
    /// it. `None` if no wake delivered it (it can't happen: every sample in the FIFO came at a wake).
    #[inline]
    pub(crate) fn wake_of(&mut self, index: u64) -> Option<f64> {
        while self.len > 1 && self.at(1).is_some_and(|(from, _)| from <= index) {
            self.first = (self.first + 1) % WAKES;
            self.len -= 1;
        }
        self.at(0).filter(|&(from, _)| self.len > 0 && from <= index).map(|(_, hns)| hns)
    }

    /// The wakes held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// One block's budget. Times are QPC hns: `capture` the block's first sample's capture time (the mic's stamps), `wake` the
/// wake that read it, `write` the block's write (its clock reading), `play` the play-out the clock predicts for its first
/// output sample. `queue` is the output queued ahead of that sample when it was written, in samples; `chain` the chain's
/// offset in samples. The render side is what's left of the readout (`play - capture`), so the four parts add up to it.
#[inline]
pub(crate) fn split(capture: f64, wake: f64, write: f64, play: f64, queue: f64, chain: u32) -> LatencyBudget {
    const MS_PER_HNS: f64 = 1e-4;
    let queue_ms = queue * MS_PER_SAMPLE;
    LatencyBudget {
        mic_ms: ((wake - capture) * MS_PER_HNS) as f32,
        wait_ms: ((write - wake) * MS_PER_HNS) as f32,
        chain_ms: (f64::from(chain) * MS_PER_SAMPLE) as f32,
        queue_ms: queue_ms as f32,
        render_ms: ((play - write) * MS_PER_HNS - queue_ms) as f32,
    }
}

/// The budget's statistics: each part over the readout's window (its last 100 blocks), and the run's totals.
pub(crate) struct BudgetStats {
    mic: RollingStats<100>,
    wait: RollingStats<100>,
    chain: RollingStats<100>,
    queue: RollingStats<100>,
    render: RollingStats<100>,
    totals: BudgetTotals,
}

impl BudgetStats {
    pub(crate) fn new() -> Self {
        Self {
            mic: RollingStats::new(),
            wait: RollingStats::new(),
            chain: RollingStats::new(),
            queue: RollingStats::new(),
            render: RollingStats::new(),
            totals: BudgetTotals::default(),
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, b: &LatencyBudget) {
        self.mic.push(b.mic_ms);
        self.wait.push(b.wait_ms);
        self.chain.push(b.chain_ms);
        self.queue.push(b.queue_ms);
        self.render.push(b.render_ms);
        self.totals.add(b);
    }

    /// The readout went off: the window starts afresh when it comes back (the totals carry on).
    pub(crate) fn clear(&mut self) {
        for s in [&mut self.mic, &mut self.wait, &mut self.chain, &mut self.queue, &mut self.render] {
            s.clear();
        }
    }

    /// Once a second: the window's means and 95th percentiles (left as they were while it's empty, as the readout's are),
    /// and the totals.
    pub(crate) fn publish(&self, tel: &Telemetry) {
        if !self.mic.is_empty() {
            let of = |f: &dyn Fn(&RollingStats<100>) -> f32| LatencyBudget {
                mic_ms: f(&self.mic),
                wait_ms: f(&self.wait),
                chain_ms: f(&self.chain),
                queue_ms: f(&self.queue),
                render_ms: f(&self.render),
            };
            tel.budget_mean.store(&of(&|s| s.mean()));
            tel.budget_p95.store(&of(&|s| s.percentile(0.95)));
        }
        tel.budget_totals.store(&self.totals);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block's first sample is timed to the wake that delivered it: a later block's head, read at a later wake, gets that
    /// one, and the wakes before it are forgotten. Samples held over from an earlier wake keep that wake's time.
    #[test]
    fn the_log_finds_the_wake_that_delivered_a_sample() {
        let mut log = WakeLog::new();
        assert_eq!(log.wake_of(0), None, "nothing delivered");
        log.delivered(0, 100.0);
        log.delivered(240, 150.0);
        log.delivered(720, 200.0);
        assert_eq!(log.wake_of(0), Some(100.0));
        assert_eq!(log.wake_of(239), Some(100.0), "the same wake's");
        assert_eq!(log.wake_of(480), Some(150.0), "read at the second wake, completed at the third");
        assert_eq!(log.len(), 2, "the first wake is forgotten");
        assert_eq!(log.wake_of(960), Some(200.0));
        assert_eq!(log.len(), 1, "the newest stays: its samples aren't all taken");
        // The FIFO discarded its oldest: the head jumps over wakes.
        log.delivered(1200, 250.0);
        log.delivered(1440, 300.0);
        assert_eq!(log.wake_of(1500), Some(300.0));
    }

    /// A full log counts a new wake's samples as the newest wake's, and lookups carry on.
    #[test]
    fn a_full_log_folds_new_wakes_into_the_newest() {
        let mut log = WakeLog::new();
        for k in 0..WAKES as u64 + 10 {
            log.delivered(k * 48, k as f64);
        }
        assert_eq!(log.len(), WAKES);
        let newest = (WAKES - 1) as f64;
        assert_eq!(log.wake_of(0), Some(0.0));
        assert_eq!(log.wake_of((WAKES as u64 + 5) * 48), Some(newest), "folded into the newest held");
        assert_eq!(log.len(), 1);
        log.delivered(100_000, 1e6);
        assert_eq!(log.wake_of(100_000), Some(1e6), "room again");
    }

    /// The parts are the gaps between the block's times, the queue in ms, and the rest of the readout: they add up to it.
    #[test]
    fn the_parts_add_up_to_the_readout() {
        // Captured at 0, read 16 ms later, written 0.2 ms after that, played 61.7 ms after the capture, 22 ms queued ahead.
        let b = split(1e10, 1e10 + 160_000.0, 1e10 + 162_000.0, 1e10 + 617_000.0, 1056.0, 480);
        assert!((b.mic_ms - 16.0).abs() < 1e-4 && (b.wait_ms - 0.2).abs() < 1e-4, "{b:?}");
        assert!((b.queue_ms - 22.0).abs() < 1e-4 && (b.chain_ms - 10.0).abs() < 1e-4, "{b:?}");
        assert!((b.render_ms - 23.5).abs() < 1e-4, "{b:?}");
        assert!((b.readout_ms() - 61.7).abs() < 1e-4 && (b.total_ms() - 71.7).abs() < 1e-4, "{b:?}");
    }

    /// Each second: the window's means and 95th percentiles, and the totals; an empty window leaves the last values.
    #[test]
    fn the_stats_publish_means_percentiles_and_totals() {
        let tel = Telemetry::new();
        let mut s = BudgetStats::new();
        for k in 0..100 {
            let mic = if k % 2 == 0 { 10.0 } else { 20.0 };
            s.push(&LatencyBudget { mic_ms: mic, wait_ms: 0.1, chain_ms: 10.0, queue_ms: 37.0 - mic, render_ms: 5.0 });
        }
        s.publish(&tel);
        let (mean, p95, totals) = (tel.budget_mean.load(), tel.budget_p95.load(), tel.budget_totals.load());
        assert!((mean.mic_ms - 15.0).abs() < 1e-4 && (mean.queue_ms - 22.0).abs() < 1e-4, "{mean:?}");
        assert!((mean.readout_ms() - 42.1).abs() < 1e-3 && (mean.chain_ms - 10.0).abs() < 1e-4, "{mean:?}");
        assert_eq!((p95.mic_ms, p95.queue_ms), (20.0, 27.0));
        assert_eq!(totals.blocks, 100);
        assert!((totals.mic_ms - 1500.0).abs() < 1e-6, "{totals:?}");
        s.clear();
        s.publish(&tel);
        assert_eq!(tel.budget_mean.load(), mean, "kept while the window is empty");
        assert_eq!(tel.budget_totals.load().blocks, 100, "the totals carry on");
    }
}
