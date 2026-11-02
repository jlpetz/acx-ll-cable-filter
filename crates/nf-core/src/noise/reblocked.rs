//! [`Reblocked`]: a [`NoiseModel`] of any hop, run in [`BLOCK`]-sample hops through the [`Reblocker`] (TODO 5, 6). The noise
//! stage re-blocks a model itself; this adaptor exists so a layer that wants 480-sample hops ([`super::Pipelined`]'s worker)
//! can take any model, and so the re-blocked model's offset can be checked on its own (`harness::check_model`).
//!
//! Its hop is 480 and its `latency_samples` is the inner model's stage latency plus 480, so its offset (`latency_samples -
//! 480`) is the inner model's `stage_latency()`: `(latency_samples - hop) + (hop - gcd(hop, 480))`. UL-UNAS bin-mapped: hop
//! 768, 1536 → offset 1440. The VAD is the last inner hop's, carried over a block with none (a hop over 480); the reports
//! of the block's inner hops add up ([`HopReport::add`]). Real-time like the re-blocker: fixed buffers, no allocation.

use crate::BLOCK;
use crate::dsp::reblock::Reblocker;
use crate::noise::{HopReport, ModelInfo, NoiseModel};
use crate::stage::StageError;

/// A model run in 480-sample hops (see the module notes).
pub struct Reblocked {
    inner: Box<dyn NoiseModel>,
    info: ModelInfo,
    reblock: Reblocker,
    vad: f32,
    report: HopReport,
}

impl Reblocked {
    /// Wraps `inner` (allocates the re-blocker's buffers).
    pub fn new(inner: Box<dyn NoiseModel>) -> Self {
        let i = inner.info();
        let latency_samples = (i.stage_latency() + BLOCK) as u32;
        let info = ModelInfo { id: i.id.clone(), name: i.name.clone(), hop: BLOCK, latency_samples, has_vad: i.has_vad };
        let reblock = Reblocker::new(i.hop);
        Self { inner, info, reblock, vad: f32::NAN, report: HopReport::default() }
    }

    /// The model inside.
    pub fn inner(&self) -> &dyn NoiseModel {
        &*self.inner
    }
}

impl NoiseModel for Reblocked {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    fn prepare(&mut self) -> Result<(), StageError> {
        self.inner.prepare()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.reblock.reset();
        self.vad = f32::NAN;
        self.report = HopReport::default();
    }

    fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let (Some(input), Some(output)) = (input.first_chunk::<BLOCK>(), output.first_chunk_mut::<BLOCK>()) else {
            return f32::NAN; // never: its hop is 480
        };
        let (inner, vad, report) = (&mut self.inner, &mut self.vad, &mut self.report);
        self.reblock.process(input, output, |i, o| {
            *vad = inner.process_hop(i, o);
            report.add(inner.take_report());
        });
        self.vad
    }

    fn take_report(&mut self) -> HopReport {
        std::mem::take(&mut self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::reblock::reblock_delay;
    use crate::noise::DelayModel;

    fn ramp(len: usize) -> Vec<f32> {
        (0..len).map(|i| ((i * 7919) % 2000) as f32 / 2000.0 - 0.5).collect()
    }

    fn run(m: &mut dyn NoiseModel, x: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (i, o) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0) {
            m.process_hop(i, o);
        }
        y
    }

    /// Its offset is the inner model's stage latency, to the sample, at every hop the re-blocker takes.
    #[test]
    fn the_offset_is_the_inner_models_stage_latency() {
        for (hop, lat) in [(160usize, 320u32), (256, 768), (480, 960), (512, 1024), (768, 1536), (960, 1920), (1024, 2048)] {
            let mut m = Reblocked::new(Box::new(DelayModel::new(hop, lat)));
            let want = (lat as usize - hop) + reblock_delay(hop);
            assert_eq!((m.info().hop, m.info().model_offset(), m.info().stage_latency()), (BLOCK, want, want), "hop {hop}");
            let x = ramp(30 * BLOCK);
            let y = run(&mut m, &x);
            for n in 0..y.len() {
                assert_eq!(y[n], if n >= want { x[n - want] } else { 0.0 }, "hop {hop}, sample {n}");
            }
            m.reset();
            assert_eq!(run(&mut m, &x), y, "hop {hop}: a reset is fresh");
        }
        assert_eq!(Reblocked::new(Box::new(DelayModel::new(768, 1536))).info().latency_samples, 1920);
    }

    /// Reports one inner hop's run per hop, and fails a block whose hop failed.
    struct Counting {
        info: ModelInfo,
        hops: u32,
    }

    impl NoiseModel for Counting {
        fn info(&self) -> &ModelInfo {
            &self.info
        }
        fn prepare(&mut self) -> Result<(), StageError> {
            Ok(())
        }
        fn reset(&mut self) {}
        fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
            output.copy_from_slice(input);
            self.hops += 1;
            self.hops as f32
        }
        fn take_report(&mut self) -> HopReport {
            HopReport { runs: 1, run_us: 2.0, failed: self.hops == 3 }
        }
    }

    #[test]
    fn reports_add_up_over_a_block_and_the_vad_carries_over() {
        let info =
            |hop| ModelInfo { id: crate::stage::text("c"), name: crate::stage::text("c"), hop, latency_samples: hop as u32, has_vad: true };
        // Hop 160: three hops a block.
        let mut m = Reblocked::new(Box::new(Counting { info: info(160), hops: 0 }));
        let mut out = [0.0f32; BLOCK];
        assert_eq!(m.process_hop(&[0.0; BLOCK], &mut out), 3.0);
        assert_eq!(m.take_report(), HopReport { runs: 3, run_us: 6.0, failed: true });
        assert_eq!(m.take_report(), HopReport::default(), "taken");
        // Hop 768: 0 or 1 hops a block; a block with none keeps the VAD and reports nothing.
        let mut m = Reblocked::new(Box::new(Counting { info: info(768), hops: 0 }));
        let vads: Vec<f32> = (0..4).map(|_| m.process_hop(&[0.0; BLOCK], &mut out)).collect();
        assert!(vads[0].is_nan() && vads[1..] == [1.0, 1.0, 2.0], "{vads:?}");
    }
}
