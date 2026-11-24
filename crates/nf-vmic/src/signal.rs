//! The test signal and its checker (nf-vmic-test's `feed` and `verify`).
//!
//! Frame `n` carries the code `k = n mod 2^23`, a different float32 on each channel, both finite, normal and below 0.5 in
//! magnitude (no denormals, no clipping):
//! - left: `k` in the mantissa's 23 bits, the sign from bit 1 of `k`, the exponent from `(k >> 3) mod 7` (2^-2 down to 2^-8);
//! - right: `(k * RIGHT_MULTIPLIER + RIGHT_OFFSET) mod 2^23` in the mantissa (odd multiplier and offset: a bijection whose
//!   parity is the opposite of `k`'s), the sign from bits 0 and 1 of `k` (left's sign xor `k`'s parity), the exponent from
//!   `(k >> 11) mod 7`.
//!
//! The checker reads `k` from the left sample's mantissa and requires both samples' bits to be exactly the code's. Each of these
//! turns every code into a non-code: a swap (in every code, the signs differ exactly when the left mantissa is odd; swapped,
//! they differ exactly when it is even), a mono fold, one channel copied to both (the channels are never equal), a sign flip on
//! either channel or both, and a float to 24-bit PCM to float round trip at a 2^23 scale (a sample survives that only with an
//! even mantissa, and the two mantissas' parities differ). At the 2^23 - 1 scale some converters use, the round trip changes
//! 99.6% of the frames (31,799 of the 2^23 codes come back bit-identical), so a recording through it still fails. A 32-bit
//! integer stage isn't seen at all: every sample's magnitude is at least 2^-8, so its 24 significant bits fit a 2^31 scale
//! exactly (lossless here, lossy for real audio below -48 dBFS). No code is 0.0 or -0.0 on either channel, so silence (both
//! samples' bits zero) is never a code: the checker tells an underrun from the signal everywhere.

/// Codes repeat every 2^23 frames (174.8 s at 48 kHz).
pub const CODE_MODULUS: u64 = 1 << 23;
const CODE_MASK: u32 = (CODE_MODULUS - 1) as u32;
/// The right channel's mantissa scramble: odd, so `k * RIGHT_MULTIPLIER mod 2^23` is a bijection.
pub const RIGHT_MULTIPLIER: u32 = 0x0037_79B1;
/// Odd, so the right mantissa's parity is always the opposite of the left's.
pub const RIGHT_OFFSET: u32 = 0x0015_5555;
/// How many corrupt frames [`CheckReport::first_corrupt`] keeps.
pub const CORRUPT_EXAMPLES: usize = 8;
/// How many gaps [`CheckReport::gap_list`] keeps (a recording with more is far past any the driver's skip log can explain: it
/// keeps 64 events).
pub const GAP_RECORDS: usize = 256;

/// One forward jump in the codes: the code that was due, the code that came (the frames between never arrived), where in the
/// recording it came, and the silence between the two codes (an underrun next to the gap, or a late read's silence in place of
/// the frames it lost).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gap {
    /// The frame index in the recording of the code that came.
    pub index: u64,
    pub due: u32,
    pub got: u32,
    pub silence: u64,
}

/// The code frame `n` carries.
pub fn code(n: u64) -> u32 {
    (n & (CODE_MODULUS - 1)) as u32
}

/// A binary32's bits: `sign` (0 or 1), the unbiased exponent `exponent`, and a 23-bit `mantissa`.
const fn f32_bits(sign: u32, exponent: i32, mantissa: u32) -> u32 {
    (sign << 31) | (((127 + exponent) as u32) << 23) | (mantissa & CODE_MASK)
}

/// The left sample's bits for code `k` (below 2^23).
pub fn left_bits(k: u32) -> u32 {
    let k = k & CODE_MASK;
    f32_bits((k >> 1) & 1, -2 - ((k >> 3) % 7) as i32, k)
}

/// The right sample's bits for code `k` (below 2^23).
pub fn right_bits(k: u32) -> u32 {
    let k = k & CODE_MASK;
    let mantissa = k.wrapping_mul(RIGHT_MULTIPLIER).wrapping_add(RIGHT_OFFSET) & CODE_MASK;
    f32_bits(((k >> 1) ^ k) & 1, -2 - ((k >> 11) % 7) as i32, mantissa)
}

/// Frame `n` of the test signal.
pub fn frame(n: u64) -> [f32; 2] {
    let k = code(n);
    [f32::from_bits(left_bits(k)), f32::from_bits(right_bits(k))]
}

/// Silence: both samples' bits zero (what the driver's underruns and zero-filled packets hold). Never a code.
pub fn is_silence(frame: [f32; 2]) -> bool {
    frame[0].to_bits() == 0 && frame[1].to_bits() == 0
}

/// The code a frame carries, or `None` if it isn't exactly one of the signal's frames (both channels' bits).
pub fn decode(frame: [f32; 2]) -> Option<u32> {
    let (left, right) = (frame[0].to_bits(), frame[1].to_bits());
    let k = left & CODE_MASK;
    (left_bits(k) == left && right_bits(k) == right).then_some(k)
}

/// What [`SequenceChecker`] saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Every frame fed.
    pub frames: u64,
    /// Frames carrying a code, exactly.
    pub signal_frames: u64,
    pub first_code: Option<u32>,
    /// The first code's frame index in the recording: with `first_code`, the code each frame before it would carry.
    pub first_code_index: Option<u64>,
    pub last_code: Option<u32>,
    /// The lowest and highest decoded code, including intermediate packets. Used to check disjoint cable marker ranges.
    pub code_range: Option<(u32, u32)>,
    /// Signal frames exactly one code after the previous signal frame.
    pub steps_ok: u64,
    /// Forward jumps (frames lost), and the frames they skipped.
    pub gaps: u64,
    pub gap_frames: u64,
    /// The first [`GAP_RECORDS`] gaps, in order.
    pub gap_list: Vec<Gap>,
    /// Backward jumps (frames delivered again), and how far back.
    pub repeats: u64,
    pub repeat_frames: u64,
    /// Silence before the first signal frame (the feed not yet read).
    pub zeros_before_signal: u64,
    /// Silence between the first and the last signal frame, and its runs: the driver's underruns.
    pub zeros_inside: u64,
    pub zero_runs_inside: u64,
    /// Silence after the last signal frame so far: the feed ending, or the recording stopping inside an underrun.
    pub zeros_after_signal: u64,
    /// Frames that are neither silence nor a code: a sample off the code's bits, the channels swapped, folded or copied.
    pub corrupt_frames: u64,
    /// The first few: (frame index in the recording, left bits, right bits).
    pub first_corrupt: Vec<(u64, u32, u32)>,
}

/// Checks a recording of the test signal frame by frame.
#[derive(Clone, Debug, Default)]
pub struct SequenceChecker {
    report: CheckReport,
    last: Option<u32>,
    /// The silence since the last signal frame is in one run (a corrupt frame ends a run).
    in_zero_run: bool,
    /// Runs of silence since the last signal frame: inside the signal once another signal frame comes.
    pending_runs: u64,
}

impl SequenceChecker {
    pub fn new() -> Self {
        let mut checker = Self::default();
        checker.report.first_corrupt.reserve(CORRUPT_EXAMPLES);
        checker
    }

    pub fn feed(&mut self, frame: [f32; 2]) {
        let r = &mut self.report;
        let index = r.frames;
        r.frames += 1;
        if is_silence(frame) {
            if self.last.is_none() {
                r.zeros_before_signal += 1;
            } else {
                r.zeros_after_signal += 1;
                if !self.in_zero_run {
                    self.pending_runs += 1;
                }
                self.in_zero_run = true;
            }
            return;
        }
        self.in_zero_run = false;
        let due = self.last.map(|k| (k + 1) & CODE_MASK);
        let Some(k) = decode(frame) else {
            r.corrupt_frames += 1;
            if r.first_corrupt.len() < CORRUPT_EXAMPLES {
                r.first_corrupt.push((index, frame[0].to_bits(), frame[1].to_bits()));
            }
            // The corrupt frame still takes its place in the sequence: the next one is due after it.
            if due.is_some() {
                self.last = due;
            }
            return;
        };
        // The silence since the last signal frame lies inside the signal.
        let silence = r.zeros_after_signal;
        r.zeros_inside += r.zeros_after_signal;
        r.zero_runs_inside += self.pending_runs;
        r.zeros_after_signal = 0;
        self.pending_runs = 0;
        r.signal_frames += 1;
        r.code_range = Some(r.code_range.map_or((k, k), |(min, max)| (min.min(k), max.max(k))));
        match due {
            None => (r.first_code, r.first_code_index) = (Some(k), Some(index)),
            Some(d) if k == d => r.steps_ok += 1,
            Some(d) => {
                let ahead = k.wrapping_sub(d) & CODE_MASK;
                if u64::from(ahead) < CODE_MODULUS / 2 {
                    r.gaps += 1;
                    r.gap_frames += u64::from(ahead);
                    if r.gap_list.len() < GAP_RECORDS {
                        r.gap_list.push(Gap { index, due: d, got: k, silence });
                    }
                } else {
                    r.repeats += 1;
                    r.repeat_frames += u64::from(d.wrapping_sub(k) & CODE_MASK);
                }
            }
        }
        self.last = Some(k);
        r.last_code = Some(k);
    }

    pub fn report(&self) -> &CheckReport {
        &self.report
    }

    /// [`CheckReport::is_clean`] so far.
    pub fn is_clean(&self) -> bool {
        self.report.is_clean()
    }
}

impl CheckReport {
    /// Bit-exact and in order: some signal, and no gap, repeat or corrupt frame. Silence is reported, not judged here: the caller
    /// holds it against the driver's underrun count (nf-vmic-test's `verify`).
    pub fn is_clean(&self) -> bool {
        self.signal_frames > 0 && self.gaps == 0 && self.repeats == 0 && self.corrupt_frames == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(frames: impl IntoIterator<Item = [f32; 2]>) -> SequenceChecker {
        let mut c = SequenceChecker::new();
        frames.into_iter().for_each(|f| c.feed(f));
        c
    }

    /// A dense sample of the codes: both ends of the range and every 61st code between.
    fn sample() -> impl Iterator<Item = u32> {
        let m = CODE_MODULUS as u32;
        (0..4096).chain((4096..m - 4096).step_by(61)).chain(m - 4096..m)
    }

    /// Every code, both channels: the frame decodes to its code, each sample is finite, normal, non-zero and below 0.5 in
    /// magnitude (at least 2^-8), and the channels are never equal or opposite.
    #[test]
    fn every_code_round_trips() {
        for k in 0..CODE_MODULUS as u32 {
            let f = frame(u64::from(k));
            assert_eq!(decode(f), Some(k), "code {k}");
            for x in f {
                assert!(x.is_normal() && (1.0 / 256.0..0.5).contains(&x.abs()), "code {k}: {x}");
            }
            assert!(f[0] != f[1] && f[0] != -f[1], "code {k}: {f:?}");
        }
        // Positions wrap onto codes: frame n and frame n + 2^23 are the same.
        for n in [0, 1, CODE_MODULUS - 1, CODE_MODULUS, CODE_MODULUS + 7, u64::MAX] {
            assert_eq!(decode(frame(n)), Some(code(n)), "frame {n}");
            assert_eq!(frame(n).map(f32::to_bits), frame(n % CODE_MODULUS).map(f32::to_bits));
        }
    }

    #[test]
    fn silence_is_never_a_code() {
        assert!(is_silence([0.0, 0.0]));
        assert_eq!(decode([0.0, 0.0]), None);
        assert_eq!(decode([-0.0, -0.0]), None);
        assert!(!is_silence([-0.0, 0.0]), "-0.0 isn't the driver's silence");
        assert_eq!(decode([f32::NAN, f32::NAN]), None);
        // And no code has a zero sample on either side (every_code_round_trips checks the magnitudes of all of them).
        assert!(sample().all(|k| left_bits(k) & 0x7FFF_FFFF != 0 && right_bits(k) & 0x7FFF_FFFF != 0));
    }

    /// A float to integer to float round trip at `scale` (rounded, and clamped to the integer's range).
    fn int_round_trip(x: f32, scale: f64) -> f32 {
        ((f64::from(x) * scale).round().clamp(-scale, scale - 1.0) / scale) as f32
    }

    /// How many of the sample's frames `fault` turns into non-codes (neither a code nor silence), and the sample's size.
    fn caught(fault: impl Fn([f32; 2]) -> [f32; 2]) -> (u64, u64) {
        sample().fold((0, 0), |(caught, tried), k| {
            let bad = fault(frame(u64::from(k)));
            (caught + u64::from(decode(bad).is_none() && !is_silence(bad)), tried + 1)
        })
    }

    /// The faults a path between the ring and the recording could have. Each turns every code into a non-code, except an int24
    /// round trip at a 2^23 - 1 scale (99.6% of all 2^23 codes; at least 99% of the sample needed) and a 32-bit integer stage,
    /// which the signal can't see (the module's comment).
    #[test]
    fn faults_are_detected() {
        let fold = |f: [f32; 2]| [(f[0] + f[1]) / 2.0; 2];
        let int24_truncated = |x: f32| ((f64::from(x) * 8_388_608.0).trunc() / 8_388_608.0) as f32;
        type Fault<'a> = &'a dyn Fn([f32; 2]) -> [f32; 2];
        let faults: [(&str, Fault); 10] = [
            ("swap", &|f| [f[1], f[0]]),
            ("mono fold", &fold),
            ("left on both", &|f| [f[0], f[0]]),
            ("right on both", &|f| [f[1], f[1]]),
            ("sign flip", &|f| [-f[0], -f[1]]),
            ("left's sign flipped", &|f| [-f[0], f[1]]),
            ("right's sign flipped", &|f| [f[0], -f[1]]),
            ("int24 round trip, 2^23 scale", &|f| f.map(|x| int_round_trip(x, 8_388_608.0))),
            ("int24 truncated, 2^23 scale", &|f| f.map(int24_truncated)),
            ("int16 round trip", &|f| f.map(|x| int_round_trip(x, 32_768.0))),
        ];
        for (name, fault) in faults {
            let (caught, tried) = caught(fault);
            assert_eq!(caught, tried, "{name}: {caught} of {tried}");
        }
        let (caught_int24, tried) = caught(|f| f.map(|x| int_round_trip(x, 8_388_607.0)));
        assert!(caught_int24 * 100 >= tried * 99, "int24 round trip, 2^23 - 1 scale: {caught_int24} of {tried}");
        // A 32-bit integer stage is lossless for every code: if this fails, the exponent rule changed, and so must the comment.
        assert_eq!(caught(|f| f.map(|x| int_round_trip(x, 2_147_483_648.0))), (0, tried));
    }

    #[test]
    fn a_clean_run_through_the_wrap() {
        let start = CODE_MODULUS - 1000;
        let c = check((start..start + 2000).map(frame));
        assert!(c.is_clean());
        let r = c.report();
        assert_eq!((r.signal_frames, r.steps_ok, r.zeros_inside), (2000, 1999, 0));
        assert_eq!((r.first_code, r.last_code), (Some((CODE_MODULUS - 1000) as u32), Some(999)));
    }

    #[test]
    fn leading_and_trailing_silence() {
        let silence = |n| std::iter::repeat_n([0.0; 2], n);
        let c = check(silence(50).chain((0..100).map(frame)).chain(silence(20)));
        assert!(c.is_clean());
        let r = c.report();
        assert_eq!((r.zeros_before_signal, r.zeros_inside, r.zero_runs_inside, r.zeros_after_signal), (50, 0, 0, 20));
    }

    #[test]
    fn inserted_silence_is_counted_inside() {
        let silence = |n| std::iter::repeat_n([0.0; 2], n);
        let c = check((0..100).map(frame).chain(silence(30)).chain((100..200).map(frame)).chain(silence(5)).chain((200..210).map(frame)));
        assert!(c.is_clean());
        let r = c.report();
        assert_eq!((r.zeros_inside, r.zero_runs_inside, r.zeros_after_signal, r.gaps), (35, 2, 0, 0));
    }

    /// An underrun where code 0 is due: silence isn't a code, so the code 0 that follows the silence is the next step, not a
    /// repeat (the old signal's code 0 was silence's bits).
    #[test]
    fn an_underrun_at_the_wrap_is_silence() {
        let end = CODE_MODULUS;
        let c = check((end - 10..end).map(frame).chain(std::iter::repeat_n([0.0; 2], 3)).chain((end..end + 10).map(frame)));
        assert!(c.is_clean(), "{:?}", c.report());
        assert_eq!((c.report().zeros_inside, c.report().repeats, c.report().steps_ok), (3, 0, 19));
    }

    #[test]
    fn gaps_and_repeats() {
        let c = check((10..20).chain(25..30).chain(27..40).map(frame));
        assert!(!c.is_clean());
        let r = c.report();
        assert_eq!((r.gaps, r.gap_frames), (1, 5));
        assert_eq!((r.repeats, r.repeat_frames), (1, 3));
        // The gap's codes either side and its place: frames 20 to 24 never came, code 25 at index 10.
        assert_eq!(r.gap_list, [Gap { index: 10, due: 20, got: 25, silence: 0 }]);
    }

    /// A gap with silence before the code that came (a late read: silence in place of the frames it lost), one at the codes'
    /// wrap, and the list's cap.
    #[test]
    fn each_gap_is_listed_with_its_silence() {
        let silence = |n| std::iter::repeat_n([0.0; 2], n);
        let end = CODE_MODULUS;
        let c = check((0..10).map(frame).chain(silence(4)).chain((14..20).map(frame)).chain((40..43).map(frame)));
        let r = c.report();
        assert_eq!(r.gap_list, [Gap { index: 14, due: 10, got: 14, silence: 4 }, Gap { index: 20, due: 20, got: 40, silence: 0 }]);
        let wrap = check((end - 3..end).chain(2..4).map(frame));
        assert_eq!(wrap.report().gap_list, [Gap { index: 3, due: 0, got: 2, silence: 0 }]);
        let many = check((0..2 * GAP_RECORDS as u64 + 2).map(|n| frame(2 * n)));
        assert_eq!((many.report().gaps, many.report().gap_list.len()), (2 * GAP_RECORDS as u64 + 1, GAP_RECORDS));
    }

    #[test]
    fn corrupt_frames() {
        let mut frames: Vec<[f32; 2]> = (0..10).map(frame).collect();
        frames[3] = [frame(3)[1], frame(3)[0]];
        frames[6][1] = 0.123_456_7;
        let c = check(frames);
        assert!(!c.is_clean());
        let r = c.report();
        assert_eq!((r.corrupt_frames, r.first_corrupt[0].0, r.first_corrupt[1].0), (2, 3, 6));
        // Each corrupt frame kept its place: no gap after it.
        assert_eq!(r.gaps, 0);
    }

    /// A stream's first frames scaled (a render engine's fade-in): silence, then non-codes, then the codes; the first
    /// code's index says which code each frame before it would carry.
    #[test]
    fn first_code_index_places_the_frames_before_it() {
        let mut frames: Vec<[f32; 2]> = vec![[0.0, 0.0]; 3];
        frames.extend((0..8).map(|n| frame(n).map(|s| s * n as f32 / 8.0)));
        frames.extend((8..12).map(frame));
        let c = check(frames);
        let r = c.report();
        assert_eq!((r.zeros_before_signal, r.corrupt_frames, r.first_code, r.first_code_index), (4, 7, Some(8), Some(11)));
        assert_eq!(r.first_corrupt[0].0, 4);
    }
}
