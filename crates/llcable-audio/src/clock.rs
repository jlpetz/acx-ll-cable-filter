//! Independent clocks need some elasticity. Sources on the master clock bypass this queue entirely.
use std::collections::VecDeque;

pub(super) type Frame = [f32; 2];

/// Average a tenth of a second of fill before judging drift: two independent packet periods make instantaneous fill
/// swing even when their clocks agree. Only sustained excess or shortage changes the rate. The unadjusted path preserves sample bits.
#[derive(Default)]
pub(super) struct Drift {
    direction: i32,
    elapsed: usize,
    mean: Option<f64>,
}

impl Drift {
    pub fn correction(&mut self, level: usize, low: usize, high: usize, frames: usize) -> i32 {
        let mean = self.mean.get_or_insert(level as f64);
        *mean += (level as f64 - *mean) * (frames as f64 / (4800 + frames) as f64);
        let direction = if *mean < low as f64 {
            -1
        } else if *mean > high as f64 {
            1
        } else {
            0
        };
        if direction == 0 || self.direction != direction {
            self.elapsed = 0;
            self.direction = direction;
        }
        if direction != 0 {
            self.elapsed += frames;
            if self.elapsed >= 960 {
                self.elapsed = 0;
                return direction;
            }
        }
        0
    }
}

pub(super) fn interpolate(a: Frame, b: Frame, t: f32) -> Frame {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

pub(super) struct Follower {
    queue: VecDeque<Frame>,
    limit: usize,
    period: usize,
    primed: bool,
    drift: Drift,
    pub lost: u64,
}

impl Follower {
    pub fn new(max_frames: usize, period: usize) -> Self {
        let limit = 4 * (max_frames + period);
        Self { queue: VecDeque::with_capacity(limit), limit, period, primed: false, drift: Drift::default(), lost: 0 }
    }

    pub fn push(&mut self, packet: &[Frame]) {
        for &frame in packet {
            if self.queue.len() == self.limit {
                self.queue.pop_front();
                self.lost += 1;
            }
            self.queue.push_back(frame);
        }
    }

    pub fn render(&mut self, out: &mut [Frame]) {
        let n = out.len();
        let block = n.max(self.period);
        self.primed |= self.queue.len() >= n + block;
        if !self.primed {
            out.fill([0.; 2]);
            return;
        }
        let available = self.queue.len();
        if available < n {
            for frame in &mut out[..available] {
                *frame = self.queue.pop_front().unwrap();
            }
            out[available..].fill([0.; 2]);
            self.lost += (n - available) as u64;
            self.primed = false;
            return;
        }
        let adjust = self.drift.correction(available, n + 3 * block / 4, n + 2 * block, n);
        let consume = n.saturating_add_signed(adjust as isize).clamp(1, available);
        if consume == n || n == 1 {
            for frame in out {
                *frame = self.queue.pop_front().unwrap();
            }
        } else {
            for (i, frame) in out.iter_mut().enumerate() {
                let pos = i * (consume - 1);
                let lo = pos / (n - 1);
                let t = (pos % (n - 1)) as f32 / (n - 1) as f32;
                *frame = interpolate(self.queue[lo], self.queue[(lo + 1).min(consume - 1)], t);
            }
            self.queue.drain(..consume);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_secondary_is_bit_exact_after_priming() {
        let mut input = Follower::new(96, 96);
        let mut output = [[0.; 2]; 96];
        for i in 0..100 {
            input.push(&[[i as f32, -(i as f32)]; 96]);
            input.render(&mut output);
            if i > 0 {
                assert_eq!(output, [[(i - 1) as f32, -((i - 1) as f32)]; 96]);
            }
        }
        assert_eq!(input.lost, 0);
    }

    #[test]
    fn stalled_secondary_does_not_hold_up_master() {
        let mut input = Follower::new(8, 8);
        input.push(&[[1.; 2]; 16]);
        let mut out = [[0.; 2]; 8];
        for _ in 0..2 {
            input.render(&mut out);
            assert_eq!(out, [[1.; 2]; 8]);
        }
        input.render(&mut out);
        assert_eq!(out, [[0.; 2]; 8]);
        assert_eq!(input.lost, 8);
    }

    #[test]
    fn independent_clock_500_ppm_fast_and_slow_stay_in_bounds_for_ten_minutes() {
        for drift in [-1_isize, 1] {
            let mut follower = Follower::new(960, 480);
            let one_packet = [[0.25, -0.5]; 480];
            let mut secondary_due = 0;
            let mut out = [[0.; 2]; 96];
            for i in 0..300_000 {
                secondary_due += 96;
                if i % 1000 < 48 {
                    secondary_due += drift;
                } // 48 frames / 96,000 frames = 500 ppm
                if secondary_due >= 480 {
                    secondary_due -= 480;
                    follower.push(&one_packet);
                }
                follower.render(&mut out);
            }
            assert_eq!(follower.lost, 0, "{drift}: missed or overflowed");
            assert!(follower.queue.len() < 2000);
        }
    }
}
