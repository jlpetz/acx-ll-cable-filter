//! Re-blocks a model's hop to the host's [`BLOCK`]-frame blocks (TODO 3), so a model with its own hop (FastEnhancer-48k's
//! 512, UL-UNAS bin-mapped's 768, TODO 5) runs inside the noise stage unchanged.
//!
//! Each block is appended to an input buffer; every whole hop in it goes through the model into an output buffer; one block
//! is read from the output buffer's front. The output buffer starts primed with [`reblock_delay`]`(hop)` = `hop - gcd(hop,
//! 480)` zeros, the least that keeps it from running dry: after block k it has produced `hop * floor(480k / hop)` samples
//! and been read `480k`, a shortfall of `480k mod hop`, whose largest value is `hop - gcd(hop, 480)`. So the re-blocker
//! adds exactly that delay, and none for a hop that divides 480 or is a multiple of it.
//!
//! Fixed buffers, sized in `new`: the input holds `hop + 480` (under one hop is left after each block), the output
//! `delay + hop + 480`. A block runs at most ⌈480/hop⌉ hops, and its copies are bounded by the buffer sizes: no
//! allocation, O(1) per block.

use crate::{BLOCK, Block};

/// The greatest common divisor (`gcd(a, 0) = a`).
pub const fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The delay the re-blocker adds for `hop`: `hop - gcd(hop, BLOCK)` samples (0 for `hop == 0`).
pub const fn reblock_delay(hop: usize) -> usize {
    hop.saturating_sub(gcd(hop, BLOCK))
}

/// Runs a hop-framed model in [`BLOCK`]-frame blocks (see the module notes).
#[derive(Clone, Debug)]
pub struct Reblocker {
    hop: usize,
    delay: usize,
    /// Input not yet run through the model: `input[..in_len]`, always under one hop between blocks.
    input: Box<[f32]>,
    in_len: usize,
    /// Model output not yet read: `output[..out_len]`, at most `delay` between blocks.
    output: Box<[f32]>,
    out_len: usize,
}

impl Reblocker {
    /// A re-blocker for `hop` (at least 1) samples. Allocates its buffers.
    pub fn new(hop: usize) -> Self {
        let hop = hop.max(1);
        let delay = reblock_delay(hop);
        Self {
            hop,
            delay,
            input: vec![0.0; hop + BLOCK].into_boxed_slice(),
            in_len: 0,
            output: vec![0.0; delay + hop + BLOCK].into_boxed_slice(),
            out_len: delay,
        }
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    /// The delay it adds, in samples.
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// Feeds one block and reads one: `model(hop_in, hop_out)` runs once per whole hop the input now holds (0 to ⌈480/hop⌉
    /// times), each `hop_in` and `hop_out` exactly `hop` samples. Real-time.
    #[inline]
    pub fn process(&mut self, input: &Block, out: &mut Block, mut model: impl FnMut(&[f32], &mut [f32])) {
        let hop = self.hop;
        // In: append. `in_len < hop` here, so this fits.
        self.input[self.in_len..self.in_len + BLOCK].copy_from_slice(input);
        self.in_len += BLOCK;
        // Every whole hop through the model. `out_len <= delay` here, and at most `hop - 1 + 480` samples come out.
        let mut pos = 0;
        while pos + hop <= self.in_len {
            let o = self.out_len;
            model(&self.input[pos..pos + hop], &mut self.output[o..o + hop]);
            self.out_len += hop;
            pos += hop;
        }
        self.input.copy_within(pos..self.in_len, 0);
        self.in_len -= pos;
        // Out: one block from the front. The priming keeps `out_len >= BLOCK` (module notes); the zero fill never runs.
        let n = self.out_len.min(BLOCK);
        out[..n].copy_from_slice(&self.output[..n]);
        out[n..].fill(0.0);
        self.output.copy_within(n..self.out_len, 0);
        self.out_len -= n;
    }

    /// Back to the state after `new`: nothing buffered, the output primed with `delay` zeros. Real-time.
    pub fn reset(&mut self) {
        self.input.fill(0.0);
        self.in_len = 0;
        self.output.fill(0.0);
        self.out_len = self.delay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOPS: [usize; 10] = [1, 160, 240, 256, 480, 512, 768, 960, 1024, 1500];

    #[test]
    fn the_delay_is_the_hop_less_its_gcd_with_the_block() {
        #[rustfmt::skip]
        let expect = [(160, 0), (240, 0), (256, 224), (480, 0), (512, 480), (768, 672), (960, 480), (1024, 992), (441, 438), (1, 0)];
        for (hop, delay) in expect {
            assert_eq!(reblock_delay(hop), delay, "hop {hop}");
            assert_eq!(Reblocker::new(hop).delay(), delay);
        }
        assert_eq!((gcd(480, 0), gcd(0, 480), gcd(512, 480)), (480, 480, 32));
    }

    /// An identity model through the re-blocker: the output is the input `delay` samples later, sample for sample, so the
    /// output buffer never ran dry (its zero fill would show), and no block runs more than ⌈480/hop⌉ hops.
    #[test]
    fn an_identity_model_comes_out_delayed_exactly() {
        for hop in HOPS {
            let mut r = Reblocker::new(hop);
            let d = r.delay();
            let blocks = 3 * hop.max(BLOCK) / BLOCK * 8 + 20;
            let x: Vec<f32> = (0..blocks * BLOCK).map(|i| (i + 1) as f32).collect();
            let mut y = vec![0.0f32; x.len()];
            let mut most = 0;
            for (xi, yo) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0) {
                let mut hops = 0;
                r.process(xi, yo, |i, o| {
                    assert_eq!((i.len(), o.len()), (hop, hop));
                    o.copy_from_slice(i);
                    hops += 1;
                });
                most = most.max(hops);
            }
            for n in 0..y.len() {
                let expect = if n >= d { x[n - d] } else { 0.0 };
                assert_eq!(y[n], expect, "hop {hop}, sample {n}");
            }
            assert!(most <= BLOCK.div_ceil(hop), "hop {hop}: {most} hops in one block");
        }
    }

    #[test]
    fn reset_brings_back_the_primed_state() {
        for hop in HOPS {
            let x: Vec<f32> = (0..7 * BLOCK).map(|i| (i % 97) as f32 - 40.0).collect();
            let run = |r: &mut Reblocker| -> Vec<f32> {
                let mut y = vec![0.0f32; x.len()];
                for (xi, yo) in x.as_chunks::<BLOCK>().0.iter().zip(y.as_chunks_mut::<BLOCK>().0) {
                    r.process(xi, yo, |i, o| o.copy_from_slice(i));
                }
                y
            };
            let mut r = Reblocker::new(hop);
            let first = run(&mut r);
            r.process(&[5.0; BLOCK], &mut [0.0; BLOCK], |i, o| o.copy_from_slice(i));
            r.reset();
            assert_eq!(run(&mut r), first, "hop {hop}");
        }
    }
}
