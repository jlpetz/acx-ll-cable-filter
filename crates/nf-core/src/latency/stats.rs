//! Fixed-capacity rolling statistics for the live readouts (latency, per-block CPU time). Runs on the
//! audio thread, so it doesn't allocate: percentiles copy into a stack array and select.

/// The last `N` values.
#[derive(Clone, Debug)]
pub struct RollingStats<const N: usize> {
    buf: [f32; N],
    len: usize,
    pos: usize,
}

impl<const N: usize> Default for RollingStats<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RollingStats<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "RollingStats needs N > 0") };
        Self { buf: [0.0; N], len: 0, pos: 0 }
    }

    /// Adds a value, evicting the oldest once full. No division or bounds check (`pos < N` always).
    #[inline]
    pub fn push(&mut self, v: f32) {
        if let Some(slot) = self.buf.get_mut(self.pos) {
            *slot = v;
        }
        self.pos = if self.pos + 1 >= N { 0 } else { self.pos + 1 };
        if self.len < N {
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.pos = 0;
    }

    /// The `p`-quantile (0..=1) of the stored values, by nearest rank; `NaN` when empty.
    pub fn percentile(&self, p: f32) -> f32 {
        if self.len == 0 {
            return f32::NAN;
        }
        // `len` is at most N; the `min` lets LLVM see it, so the slices carry no panic path (the
        // engine calls this on the audio thread once a second).
        let n = self.len.min(N);
        let mut tmp = [0.0f32; N];
        let vals = &mut tmp[..n];
        vals.copy_from_slice(&self.buf[..n]);
        let rank = (((p.clamp(0.0, 1.0) * (n - 1) as f32).round()) as usize).min(n - 1);
        let (_, v, _) = vals.select_nth_unstable_by(rank, |a, b| a.total_cmp(b));
        *v
    }

    pub fn median(&self) -> f32 {
        self.percentile(0.5)
    }

    pub fn max(&self) -> f32 {
        if self.len == 0 {
            return f32::NAN;
        }
        self.buf[..self.len.min(N)].iter().copied().fold(f32::MIN, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_a_known_set() {
        let mut s = RollingStats::<100>::new();
        for i in 1..=100 {
            s.push(i as f32);
        }
        assert_eq!(s.median(), 51.0);
        assert_eq!(s.percentile(0.99), 99.0);
        assert_eq!(s.max(), 100.0);
        s.push(1000.0); // evicts 1
        assert_eq!(s.max(), 1000.0);
        assert_eq!(s.len(), 100);
        assert_eq!(s.percentile(0.0), 2.0);
    }

    #[test]
    fn partial_fill_wraps_and_clear() {
        let mut s = RollingStats::<4>::new();
        assert!(s.is_empty() && s.median().is_nan() && s.max().is_nan());
        for v in [5.0, 1.0, 3.0] {
            s.push(v);
        }
        assert_eq!((s.len(), s.median(), s.max()), (3, 3.0, 5.0));
        for v in [7.0, 8.0, 9.0] {
            s.push(v); // wraps: holds 7, 8, 9, 3
        }
        assert_eq!((s.len(), s.percentile(0.0), s.max()), (4, 3.0, 9.0));
        assert_eq!(s.percentile(f32::NAN), 3.0, "NaN quantile reads as the minimum, no panic");
        s.clear();
        assert!(s.is_empty());
        s.push(2.0);
        assert_eq!(s.median(), 2.0);
    }
}
