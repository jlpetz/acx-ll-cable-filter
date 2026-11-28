use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use llcable_control::{CableId, Diagnostics};
use windows_reactor::{ComponentContext, ComponentTask};

use super::{Mixer, Msg};
use crate::patch::Inventory;

pub(super) type Samples = Vec<(CableId, Result<Diagnostics, String>)>;

#[derive(Clone, Copy)]
struct Baseline {
    underruns: u64,
    missing: u64,
    last: Diagnostics,
}

impl Baseline {
    fn observe(previous: Option<Self>, sample: Diagnostics) -> Self {
        let Some(mut base) = previous else {
            return Self { underruns: sample.underrun_frames, missing: sample.render_lost_packets.unwrap_or(0), last: sample };
        };
        if sample.underrun_frames < base.last.underrun_frames
            || sample.render_lost_packets.zip(base.last.render_lost_packets).is_some_and(|(now, last)| now < last)
        {
            // This cable's device counters restarted. Include any losses already recorded by the new instance.
            base.underruns = 0;
            base.missing = 0;
        } else if base.last.render_lost_packets.is_none() {
            base.missing = sample.render_lost_packets.unwrap_or(0);
        }
        Self { last: sample, ..base }
    }
}

struct CableReading {
    sample: Result<Diagnostics, String>,
    baseline: Option<Baseline>,
}

#[derive(Default)]
pub(crate) struct CableMonitor {
    enabled: bool,
    readings: BTreeMap<CableId, CableReading>,
    generation: u64,
    pending: Option<ComponentTask>,
    next_read: Option<Instant>,
}

impl CableMonitor {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn sample(&self, id: CableId) -> Option<&Result<Diagnostics, String>> {
        self.readings.get(&id).map(|r| &r.sample)
    }

    pub(crate) fn losses(&self, id: CableId) -> Option<(u64, Option<u64>)> {
        let reading = self.readings.get(&id)?;
        let sample = reading.sample.as_ref().ok()?;
        let base = reading.baseline?;
        Some((sample.underrun_frames - base.underruns, sample.render_lost_packets.map(|n| n - base.missing)))
    }

    pub(crate) fn can_reset(&self) -> bool {
        self.enabled && self.readings.values().any(|r| r.sample.is_ok())
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.cancel();
            self.enabled = enabled;
            if enabled {
                self.readings.clear();
            }
        }
    }

    pub(super) fn reset(&mut self) {
        if self.can_reset() {
            self.cancel();
            // Read the current totals again. The last displayed sample may predate a loss by up to a second.
            self.readings.clear();
        }
    }

    fn cancel(&mut self) {
        if let Some(task) = self.pending.take() {
            task.cancel();
        }
        self.generation = self.generation.wrapping_add(1);
        self.next_read = None;
    }

    pub(super) fn rediscovered(&mut self, inv: &Inventory) {
        self.cancel();
        self.readings.retain(|id, _| inv.cables.iter().any(|c| c.id == *id));
    }

    pub(super) fn schedule(&mut self, visible: bool, inventory: Option<&Inventory>, cx: &ComponentContext<Mixer>) {
        let Some(inv) = inventory.filter(|inv| visible && self.enabled && !inv.cables.is_empty()) else {
            if self.pending.is_some() {
                self.cancel();
            }
            return;
        };
        if self.pending.is_some() {
            return;
        }
        let ids: Vec<_> = inv.cables.iter().map(|c| c.id).collect();
        let delay = self.next_read.map_or(Duration::ZERO, |due| due.saturating_duration_since(Instant::now()));
        let generation = self.generation;
        self.pending = Some(cx.spawn_background_with_rejection(
            move |cancel| {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let samples = ids
                    .into_iter()
                    .take_while(|_| !cancel.is_cancelled())
                    .map(|id| (id, llcable_control::diagnostics(id).map_err(|e| e.to_string())));
                Msg::CableSamples(generation, samples.collect())
            },
            Msg::CableSamples(generation, vec![]),
        ));
    }

    pub(super) fn received(&mut self, generation: u64, samples: Samples) {
        if generation != self.generation || !self.enabled {
            return;
        }
        self.pending = None;
        self.next_read = Some(Instant::now() + Duration::from_secs(1));
        for (id, sample) in samples {
            let previous = self.readings.get(&id).and_then(|r| r.baseline);
            let baseline = match &sample {
                Ok(stats) => Some(Baseline::observe(previous, *stats)),
                Err(_) => previous,
            };
            self.readings.insert(id, CableReading { sample, baseline });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: CableId = CableId(1);
    const B: CableId = CableId(2);

    fn reading(id: CableId, underruns: u64, missing: Option<u64>) -> (CableId, Result<Diagnostics, String>) {
        (id, Ok(Diagnostics { underrun_frames: underruns, render_lost_packets: missing, ..Default::default() }))
    }

    #[test]
    fn reset_reads_a_fresh_baseline_for_each_cable_and_preserves_device_totals() {
        let mut m = CableMonitor::default();
        assert!(!m.enabled());
        m.set_enabled(true);
        let old = m.generation;
        m.received(old, vec![reading(A, 3, Some(1)), reading(B, 1069, Some(2))]);
        assert_eq!(m.losses(B), Some((0, Some(0))));
        m.received(old, vec![reading(A, 4, Some(1)), reading(B, 1197, Some(3))]);
        assert_eq!(m.losses(A), Some((1, Some(0))));
        assert_eq!(m.losses(B), Some((128, Some(1))));
        m.reset();
        m.received(old, vec![reading(B, 1197, Some(3))]);
        assert!(!m.can_reset());
        m.received(m.generation, vec![reading(A, 4, Some(1)), reading(B, 1205, Some(3))]);
        assert_eq!(m.losses(B), Some((0, Some(0))));
        assert_eq!(m.sample(B).unwrap().as_ref().unwrap().underrun_frames, 1205);
    }

    #[test]
    fn stopping_freezes_readings_and_a_new_monitoring_run_starts_at_zero() {
        let mut m = CableMonitor::default();
        m.set_enabled(true);
        let old = m.generation;
        m.received(old, vec![reading(B, 100, None)]);
        m.received(old, vec![reading(B, 102, None)]);
        m.set_enabled(false);
        m.received(old, vec![reading(B, 108, None)]);
        assert_eq!(m.losses(B), Some((2, None)));
        assert!(!m.can_reset());
        m.set_enabled(true);
        assert!(m.sample(B).is_none());
        m.received(m.generation, vec![reading(B, 110, None)]);
        assert_eq!(m.losses(B), Some((0, None)));
    }

    #[test]
    fn retry_keeps_the_baseline_and_a_device_counter_restart_keeps_new_losses() {
        let mut m = CableMonitor::default();
        m.set_enabled(true);
        let g = m.generation;
        m.received(g, vec![reading(B, 100, Some(1))]);
        m.received(g, vec![(B, Err("offline".into()))]);
        assert!(m.losses(B).is_none());
        m.received(g, vec![reading(B, 110, Some(3))]);
        assert_eq!(m.losses(B), Some((10, Some(2))));
        m.received(g, vec![reading(B, 105, Some(1))]);
        assert_eq!(m.losses(B), Some((105, Some(1))));
    }
}
