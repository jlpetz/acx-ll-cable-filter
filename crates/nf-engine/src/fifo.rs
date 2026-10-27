//! A fixed-capacity sample FIFO for the audio thread (the mic and reference FIFOs).
//!
//! Allocated once; push, pop and discard never allocate and copy in at most two contiguous
//! segments, so no per-sample index (and no bounds check) sits in the loop.

/// A ring of `f32` samples.
#[derive(Debug)]
pub(crate) struct Fifo {
    buf: Box<[f32]>,
    /// Index of the oldest sample.
    head: usize,
    len: usize,
}

impl Fifo {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { buf: vec![0.0; capacity.max(1)].into_boxed_slice(), head: 0, len: 0 }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub(crate) fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// The write position and the length of the free run that starts there.
    #[inline]
    fn tail(&self) -> usize {
        let t = self.head + self.len;
        if t >= self.buf.len() { t - self.buf.len() } else { t }
    }

    /// Appends as much of `src` as fits; returns how many samples were taken.
    pub(crate) fn push(&mut self, src: &[f32]) -> usize {
        let n = src.len().min(self.free());
        let tail = self.tail();
        let first = n.min(self.buf.len() - tail);
        let (a, b) = src[..n].split_at(first);
        if let Some(dst) = self.buf.get_mut(tail..tail + first) {
            dst.copy_from_slice(a);
        }
        if let Some(dst) = self.buf.get_mut(..b.len()) {
            dst.copy_from_slice(b);
        }
        self.len += n;
        n
    }

    /// Appends `n` zeros (as many as fit); returns how many were added.
    pub(crate) fn push_zeros(&mut self, n: usize) -> usize {
        let n = n.min(self.free());
        let tail = self.tail();
        let first = n.min(self.buf.len() - tail);
        if let Some(dst) = self.buf.get_mut(tail..tail + first) {
            dst.fill(0.0);
        }
        if let Some(dst) = self.buf.get_mut(..n - first) {
            dst.fill(0.0);
        }
        self.len += n;
        n
    }

    /// Pops exactly `dst.len()` samples, oldest first. Pops nothing and returns false when fewer
    /// are stored.
    pub(crate) fn pop_into(&mut self, dst: &mut [f32]) -> bool {
        let n = dst.len();
        if n > self.len {
            return false;
        }
        let first = n.min(self.buf.len() - self.head);
        let (a, b) = dst.split_at_mut(first);
        if let Some(src) = self.buf.get(self.head..self.head + first) {
            a.copy_from_slice(src);
        }
        if let Some(src) = self.buf.get(..b.len()) {
            b.copy_from_slice(src);
        }
        self.advance(n);
        true
    }

    /// Drops up to `n` of the oldest samples; returns how many were dropped.
    pub(crate) fn discard(&mut self, n: usize) -> usize {
        let n = n.min(self.len);
        self.advance(n);
        n
    }

    /// Empties the FIFO (the reference's, when its stream is dropped mid-run).
    pub(crate) fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        let h = self.head + n;
        self.head = if h >= self.buf.len() { h - self.buf.len() } else { h };
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: usize, n: usize) -> Vec<f32> {
        (start..start + n).map(|v| v as f32).collect()
    }

    #[test]
    fn push_pop_in_order_across_the_wrap() {
        let mut f = Fifo::new(10);
        assert_eq!(f.capacity(), 10);
        assert_eq!(f.push(&ramp(0, 7)), 7);
        let mut out = [0.0f32; 5];
        assert!(f.pop_into(&mut out));
        assert_eq!(out.to_vec(), ramp(0, 5));
        // 2 left at indices 5-6; this push wraps.
        assert_eq!(f.push(&ramp(7, 8)), 8);
        assert_eq!(f.len(), 10);
        assert_eq!(f.free(), 0);
        let mut all = [0.0f32; 10];
        assert!(f.pop_into(&mut all));
        assert_eq!(all.to_vec(), ramp(5, 10));
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn push_takes_only_what_fits() {
        let mut f = Fifo::new(4);
        assert_eq!(f.push(&ramp(0, 6)), 4);
        assert_eq!(f.push(&[9.0]), 0);
        let mut out = [0.0f32; 4];
        assert!(f.pop_into(&mut out));
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn short_pop_takes_nothing() {
        let mut f = Fifo::new(8);
        f.push(&ramp(1, 3));
        let mut out = [7.0f32; 4];
        assert!(!f.pop_into(&mut out));
        assert_eq!(out, [7.0; 4], "untouched");
        assert_eq!(f.len(), 3);
        let mut empty: [f32; 0] = [];
        assert!(f.pop_into(&mut empty));
    }

    #[test]
    fn zeros_and_discard_wrap_too() {
        let mut f = Fifo::new(6);
        f.push(&ramp(1, 5));
        assert_eq!(f.discard(4), 4);
        assert_eq!(f.len(), 1);
        // head at 4; zeros wrap past the end.
        assert_eq!(f.push_zeros(4), 4);
        assert_eq!(f.push_zeros(9), 1, "only the free space");
        let mut out = [9.0f32; 6];
        assert!(f.pop_into(&mut out));
        assert_eq!(out, [5.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(f.discard(3), 0);
        f.push(&[1.0, 2.0]);
        f.clear();
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn long_random_sequence_matches_a_vecdeque() {
        use std::collections::VecDeque;
        let mut f = Fifo::new(37);
        let mut model = VecDeque::new();
        let mut rng = 12345u32;
        let mut next = 0usize;
        for _ in 0..5000 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let n = (rng % 20) as usize;
            match rng % 4 {
                0 | 1 => {
                    let src = ramp(next, n);
                    let took = f.push(&src);
                    model.extend(src[..took].iter().copied());
                    next += n;
                }
                2 => {
                    let mut out = vec![0.0; n];
                    if f.pop_into(&mut out) {
                        let expect: Vec<f32> = model.drain(..n).collect();
                        assert_eq!(out, expect);
                    } else {
                        assert!(model.len() < n);
                    }
                }
                _ => {
                    let d = f.discard(n);
                    model.drain(..d);
                }
            }
            assert_eq!(f.len(), model.len());
        }
    }
}
