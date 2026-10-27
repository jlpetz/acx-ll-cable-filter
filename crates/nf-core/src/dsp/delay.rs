//! A fixed-capacity delay line: the dry path for the floor mix, and the measurement passthrough.

use crate::{BLOCK, Block};

/// Delays a block stream by a set number of samples (0..=capacity). The buffer is allocated once.
#[derive(Debug, Clone)]
pub struct DelayLine {
    /// A power-of-two ring of at least `capacity + BLOCK + 1` samples.
    buf: Box<[f32]>,
    mask: usize,
    /// Where the next block is written; always `< buf.len()` (masked).
    write: usize,
    delay: usize,
    capacity: usize,
}

impl DelayLine {
    /// A delay line that can delay by up to `capacity` samples, starting at `delay` (clamped).
    pub fn new(capacity: usize, delay: usize) -> Self {
        let size = (capacity + BLOCK + 1).next_power_of_two();
        Self { buf: vec![0.0; size].into_boxed_slice(), mask: size - 1, write: 0, delay: delay.min(capacity), capacity }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn delay(&self) -> usize {
        self.delay
    }

    /// Changes the delay (clamped to the capacity). The samples already stored stay, so a change
    /// jumps within the stored history rather than to silence.
    pub fn set_delay(&mut self, delay: usize) {
        self.delay = delay.min(self.capacity);
    }

    /// Writes `input` and reads the delayed block into `out`.
    ///
    /// The whole block is written before any of it is read, each as at most two contiguous copies.
    /// That gives the same output as interleaving per sample because the ring holds at least
    /// `capacity + BLOCK + 1` samples, so the block being written never reaches the oldest sample
    /// still to be read.
    #[inline]
    pub fn process(&mut self, input: &Block, out: &mut Block) {
        let w = self.write;
        let r = (w + self.buf.len() - self.delay) & self.mask;
        write_ring(&mut self.buf, w, input);
        read_ring(&self.buf, r, out);
        self.write = (w + BLOCK) & self.mask;
    }

    pub fn reset(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
    }
}

/// Copies `src` into `ring` from index `at`, wrapping to the start once. The `min`s never bind
/// (`at < ring.len()` and `src.len() < ring.len()` by construction); they make every bound
/// provable, so no panic path is compiled in.
#[inline]
fn write_ring(ring: &mut [f32], at: usize, src: &[f32]) {
    let (front, back) = ring.split_at_mut(at.min(ring.len()));
    let n = back.len().min(src.len());
    let (head, tail) = src.split_at(n);
    back[..n].copy_from_slice(head);
    let k = tail.len().min(front.len());
    front[..k].copy_from_slice(&tail[..k]);
}

/// Copies `dst.len()` samples out of `ring` from index `at`, wrapping once (bounds as above).
#[inline]
fn read_ring(ring: &[f32], at: usize, dst: &mut [f32]) {
    let (front, back) = ring.split_at(at.min(ring.len()));
    let n = back.len().min(dst.len());
    let (head, tail) = dst.split_at_mut(n);
    head.copy_from_slice(&back[..n]);
    let k = tail.len().min(front.len());
    tail[..k].copy_from_slice(&front[..k]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original per-sample ring, kept as the reference for the segment-copy version.
    struct PerSample {
        buf: Vec<f32>,
        mask: usize,
        write: usize,
        delay: usize,
    }

    impl PerSample {
        fn new(capacity: usize, delay: usize) -> Self {
            let size = (capacity + BLOCK + 1).next_power_of_two();
            Self { buf: vec![0.0; size], mask: size - 1, write: 0, delay: delay.min(capacity) }
        }
        fn process(&mut self, input: &Block, out: &mut Block) {
            let mut w = self.write;
            for (x, y) in input.iter().zip(out.iter_mut()) {
                self.buf[w] = *x;
                *y = self.buf[(w.wrapping_sub(self.delay)) & self.mask];
                w = (w + 1) & self.mask;
            }
            self.write = w;
        }
    }

    #[test]
    fn delays_by_exact_samples_across_blocks() {
        for d in [0usize, 1, 37, 479, 480, 481, 960, 1024] {
            let mut line = DelayLine::new(1024, d);
            let mut input = [0.0f32; BLOCK];
            let mut out = [0.0f32; BLOCK];
            let mut seen = Vec::new();
            for blk in 0..8 {
                for (i, s) in input.iter_mut().enumerate() {
                    *s = (blk * BLOCK + i) as f32 + 1.0;
                }
                line.process(&input, &mut out);
                seen.extend_from_slice(&out);
            }
            for (n, v) in seen.iter().enumerate() {
                let expect = if n >= d { (n - d) as f32 + 1.0 } else { 0.0 };
                assert_eq!(*v, expect, "delay {d} at {n}");
            }
        }
    }

    #[test]
    fn matches_the_per_sample_ring_through_wraps_and_delay_changes() {
        let mut seed = 1u32;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for capacity in [0usize, 1, 31, 480, 543, 4096] {
            let mut fast = DelayLine::new(capacity, capacity / 2);
            let mut slow = PerSample::new(capacity, capacity / 2);
            let (mut a, mut b) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
            for blk in 0..200 {
                let input: Block = std::array::from_fn(|i| (blk * BLOCK + i) as f32);
                if blk % 7 == 3 {
                    let d = rand() as usize % (capacity + 2); // sometimes past the capacity: clamps
                    fast.set_delay(d);
                    slow.delay = d.min(capacity);
                }
                fast.process(&input, &mut a);
                slow.process(&input, &mut b);
                assert_eq!(a, b, "capacity {capacity}, block {blk}");
            }
        }
    }

    #[test]
    fn delay_is_clamped_and_reset_clears_history() {
        let mut line = DelayLine::new(100, 500);
        assert_eq!(line.delay(), 100);
        assert_eq!(line.capacity(), 100);
        line.process(&[1.0; BLOCK], &mut [0.0; BLOCK]);
        line.reset();
        let mut out = [9.0f32; BLOCK];
        line.process(&[0.0; BLOCK], &mut out);
        assert!(out.iter().all(|&v| v == 0.0));
        assert_eq!(line.delay(), 100, "reset keeps the delay");
    }
}
