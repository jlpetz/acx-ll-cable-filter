//! Optional per-stage observations. A reader can poll from another thread without touching the graph.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicI8, AtomicU32, AtomicU64};

/// Peaks and gain are linear amplitudes. Levels are measured at the actual model or packet stage, without compensating for
/// its latency. Missing values mean that the stage has no such measurement or it has not completed a block yet.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StageReading {
    pub input_peak: Option<f32>,
    pub output_peak: Option<f32>,
    pub gain: Option<f32>,
    pub gate_open: Option<bool>,
    pub speech_probability: Option<f32>,
    pub worker_misses: Option<u64>,
    pub echo_delay_ms: Option<f32>,
    pub erle_db: Option<f32>,
}

#[derive(Debug)]
struct MeterData {
    input: AtomicU32,
    output: AtomicU32,
    gain: AtomicU32,
    open: AtomicI8,
    speech: AtomicU32,
    misses: AtomicU64,
    delay: AtomicU32,
    erle: AtomicU32,
}

impl Default for MeterData {
    fn default() -> Self {
        let unset = || AtomicU32::new(f32::NAN.to_bits());
        Self {
            input: unset(),
            output: unset(),
            gain: unset(),
            open: AtomicI8::new(-1),
            speech: unset(),
            misses: AtomicU64::new(u64::MAX),
            delay: unset(),
            erle: unset(),
        }
    }
}

/// Independent, relaxed observations for a display. The UI may see adjacent packets' values during an update.
#[derive(Default, Clone, Debug)]
pub struct StageMeter(Arc<MeterData>);

impl StageMeter {
    pub fn read(&self) -> StageReading {
        let read = |a: &AtomicU32| {
            let x = f32::from_bits(a.load(Relaxed));
            x.is_finite().then_some(x)
        };
        let m = &*self.0;
        let misses = m.misses.load(Relaxed);
        StageReading {
            input_peak: read(&m.input),
            output_peak: read(&m.output),
            gain: read(&m.gain),
            gate_open: match m.open.load(Relaxed) {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            speech_probability: read(&m.speech),
            worker_misses: (misses != u64::MAX).then_some(misses),
            echo_delay_ms: read(&m.delay),
            erle_db: read(&m.erle),
        }
    }

    /// Publish only a stage's supported fields once per block. Passing a non-finite estimate marks it unavailable.
    pub fn publish(&self, r: StageReading) {
        let m = &*self.0;
        for (a, v) in [
            (&m.input, r.input_peak),
            (&m.output, r.output_peak),
            (&m.gain, r.gain),
            (&m.speech, r.speech_probability),
            (&m.delay, r.echo_delay_ms),
            (&m.erle, r.erle_db),
        ] {
            if let Some(x) = v {
                a.store(x.to_bits(), Relaxed);
            }
        }
        if let Some(open) = r.gate_open {
            m.open.store(i8::from(open), Relaxed);
        }
        if let Some(misses) = r.worker_misses {
            m.misses.store(misses, Relaxed);
        }
    }
}

pub(super) fn peak(stereo: &[[f32; 2]]) -> f32 {
    stereo.iter().fold(0.0f32, |p, f| p.max(f[0].abs()).max(f[1].abs()))
}
