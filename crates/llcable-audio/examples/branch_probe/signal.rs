//! Distinct stereo codes representable in 16-bit PCM. Each code embeds its frame index and four check bits.
//! Capture, the router input witness and playback all timestamp the same markers against one monotonic clock.
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use llcable_routing::{Effect, Input, PacketEffect};

pub const RATE: usize = 48_000;
pub const MAX_SECONDS: usize = 120;
// Relatively prime to the usual periods, so the markers sample different positions in the packets.
const STRIDE: usize = 1009;
const MASK: usize = 4095;

pub fn frame(n: usize) -> [f32; 2] {
    let (lo, hi) = (n & MASK, (n >> 12) & MASK);
    let l = (lo << 2) | ((hi ^ (lo >> 4)) & 3);
    let r = (((hi + 67 * lo) & MASK) << 2) | ((lo ^ (hi >> 4)) & 3);
    [(l + 1) as f32 / 32768., -((r + 1) as f32) / 32768.]
}

pub fn decode(stereo: [f32; 2]) -> Option<usize> {
    let l = (stereo[0] * 32768.) as usize;
    let r = (-stereo[1] * 32768.) as usize;
    if !(1..=16384).contains(&l) || !(1..=16384).contains(&r) {
        return None;
    }
    let lo = (l - 1) >> 2;
    let hi = ((r - 1) >> 2).wrapping_sub(67 * lo) & MASK;
    let n = (hi << 12) | lo;
    (stereo == frame(n)).then_some(n)
}

pub fn marker_count(first: usize, last: usize) -> usize {
    (last + 1).div_ceil(STRIDE).saturating_sub(first.div_ceil(STRIDE))
}

#[derive(Clone)]
pub struct Marks {
    start: Instant,
    submitted: Arc<[AtomicU64]>,
    routed: Arc<[AtomicU64]>,
}

impl Marks {
    pub fn new() -> Self {
        let stamps = || (0..=(MAX_SECONDS * RATE / STRIDE)).map(|_| AtomicU64::new(0)).collect();
        Self { start: Instant::now(), submitted: stamps(), routed: stamps() }
    }

    pub fn now(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    pub fn submit(&self, start: usize, end: usize) {
        if start < end {
            let now = self.now();
            for i in start.div_ceil(STRIDE)..end.div_ceil(STRIDE) {
                self.submitted[i].store(now, Relaxed);
            }
        }
    }

    pub fn witness(&self) -> Effect {
        Effect::Custom(Box::new(self.clone()))
    }

    pub fn latency(&self, code: usize, arrival: u64) -> Option<(f64, f64)> {
        if !code.is_multiple_of(STRIDE) {
            return None;
        }
        let i = code / STRIDE;
        let submitted = self.submitted.get(i)?.load(Relaxed);
        let routed = self.routed[i].load(Relaxed);
        (submitted > 0 && routed >= submitted && arrival >= routed)
            .then(|| ((arrival - submitted) as f64 / 1e6, (arrival - routed) as f64 / 1e6))
    }
}

impl PacketEffect for Marks {
    fn ready_on_thread(&self) -> bool {
        true
    }

    fn process(&mut self, samples: &mut [[f32; 2]], _key: Option<Input<'_>>) {
        // Find the first complete code in this packet. Only candidate markers need a second decode.
        if let Some((offset, first)) = samples.iter().enumerate().find_map(|(i, &s)| decode(s).map(|n| (i, n))) {
            let skip = first.next_multiple_of(STRIDE) - first;
            for sample in samples.iter().skip(offset + skip).step_by(STRIDE) {
                if let Some(n) = decode(*sample)
                    && n.is_multiple_of(STRIDE)
                    && let Some(stamp) = self.routed.get(n / STRIDE)
                {
                    stamp.store(self.now(), Relaxed);
                }
            }
        }
    }
}
