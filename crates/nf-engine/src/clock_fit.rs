//! The clocks' drift over a chain recording ([`crate::api::clock_drift`]; `noise-filter devices --check`, TODO 21): each
//! clock as a straight line of time against its sample index, fitted by least squares, and the lines' slopes compared.
//!
//! The drift loops' own estimates aren't used: they are the loops' integrals, which settle in a minute or more (the output's
//! PI loop has a natural frequency of about 0.17 rad/s), and bursty delivery gives them a start-up transient of hundreds of
//! ppm. A line fitted to the clocks' own readings needs no settling: a few seconds of stamps give a slope to a few ppm.
//!
//! - **The mic**: its block's first sample's index against its capture stamp (`capture_qpc_hns`), or, without trusted stamps,
//!   the samples delivered by a wake against that wake's time (noisier: the wake's lateness scatters it).
//! - **The output**: the device's clock readings (`OutputFacts::clock_pos` against `clock_qpc_hns`), an exclusive device's
//!   silence added back so the line is the device's own.
//! - **The reference**: its timeline's index of the first sample served (`read_index`) against that sample's tap time, the
//!   mic block's capture stamp less the block's stamped lead reading.
//!
//! A step that breaks a line (a mic position skip, an output underrun or re-anchor, a reference jump or restart) starts a new
//! stretch with its own intercept; the slope is shared, so a glitch moves no slope.
//!
//! Not real-time: it runs on a finished recording.

use crate::api::{ChainBlock, ClockDrift, DriftFit, MicTiming, ReferenceFacts};

/// The least span (s) of a line worth a slope, and the least readings: with stamps 3 s give a slope to a few tens of ppm (the
/// uncertainty says how close), so the shortest check (5 s, its first second left out) gets one.
const MIN_SPAN_S: f64 = 3.0;
const MIN_POINTS: usize = 50;
const HNS_PER_S: f64 = 1e7;
const BLOCK: f64 = nf_core::BLOCK as f64;

/// A clock's readings, (sample index, QPC time in hns), in order, in stretches that each have their own intercept.
#[derive(Default)]
struct Line {
    points: Vec<(f64, f64)>,
    /// Where each stretch starts in `points`.
    starts: Vec<usize>,
    /// The next reading starts a stretch.
    broken: bool,
}

/// A line's slope (hns per sample) and its standard error, and the time it spans (hns).
#[derive(Clone, Copy, Debug)]
struct Slope {
    hns_per_sample: f64,
    se: f64,
    span_hns: f64,
}

impl Line {
    /// The next reading starts a new stretch.
    fn brk(&mut self) {
        self.broken = true;
    }

    /// A reading; one that isn't a number, or repeats the last (a clock that didn't move between blocks), adds nothing.
    fn push(&mut self, index: f64, t_hns: f64) {
        if !(index.is_finite() && t_hns.is_finite()) {
            return;
        }
        if self.broken || self.starts.is_empty() {
            self.starts.push(self.points.len());
            self.broken = false;
        } else if self.points.last() == Some(&(index, t_hns)) {
            return;
        }
        self.points.push((index, t_hns));
    }

    fn stretches(&self) -> impl Iterator<Item = &[(f64, f64)]> {
        self.starts.iter().enumerate().map(|(k, &a)| {
            let b = self.starts.get(k + 1).copied().unwrap_or(self.points.len());
            &self.points[a..b]
        })
    }

    /// Time against index by least squares, one intercept a stretch (stretches of one reading say nothing). `None` under
    /// [`MIN_POINTS`] readings or [`MIN_SPAN_S`] in all.
    fn fit(&self) -> Option<Slope> {
        let (mut sxx, mut sxt, mut n, mut groups, mut span) = (0.0, 0.0, 0usize, 0usize, 0.0);
        let mut means = Vec::with_capacity(self.starts.len());
        for s in self.stretches() {
            if s.len() < 2 {
                means.push(None);
                continue;
            }
            let k = s.len() as f64;
            let (mx, mt) = (s.iter().map(|p| p.0).sum::<f64>() / k, s.iter().map(|p| p.1).sum::<f64>() / k);
            for &(x, t) in s {
                sxx += (x - mx) * (x - mx);
                sxt += (x - mx) * (t - mt);
            }
            n += s.len();
            groups += 1;
            span += s[s.len() - 1].1 - s[0].1;
            means.push(Some((mx, mt)));
        }
        if n < MIN_POINTS || span < MIN_SPAN_S * HNS_PER_S || sxx <= 0.0 {
            return None;
        }
        let b = sxt / sxx;
        let mut rss = 0.0;
        for (s, m) in self.stretches().zip(&means) {
            if let Some((mx, mt)) = m {
                rss += s.iter().map(|&(x, t)| (t - mt - b * (x - mx)).powi(2)).sum::<f64>();
            }
        }
        let dof = n.saturating_sub(groups + 1).max(1) as f64;
        let se = (rss / dof / sxx).sqrt();
        (b.is_finite() && b > 0.0).then_some(Slope { hns_per_sample: b, se, span_hns: span })
    }
}

/// `clock`'s drift against the mic's line: positive when it runs fast (fewer hns a sample).
fn against(mic: Slope, clock: Slope) -> DriftFit {
    let ratio = mic.hns_per_sample / clock.hns_per_sample;
    let rel = ((mic.se / mic.hns_per_sample).powi(2) + (clock.se / clock.hns_per_sample).powi(2)).sqrt();
    DriftFit { ppm: (ratio - 1.0) * 1e6, uncertainty_ppm: 2.0 * rel * 1e6, seconds: clock.span_hns.min(mic.span_hns) / HNS_PER_S }
}

/// A reference fact that breaks its line: a jump of the timeline, or its timing changed.
fn breaks_reference(r: &ReferenceFacts) -> bool {
    r.jumped != 0 || r.events & (ReferenceFacts::RESTARTED | ReferenceFacts::UNSTAMPED | ReferenceFacts::BROKEN) != 0
}

pub(crate) fn clock_drift(blocks: &[ChainBlock]) -> ClockDrift {
    let (mut stamped, mut delivered, mut output, mut reference) = (Line::default(), Line::default(), Line::default(), Line::default());
    // An exclusive device's silence since the first block: its clock reading leaves it out.
    let mut silence = 0.0;
    for b in blocks {
        if b.mic_skipped != 0 {
            stamped.brk();
            delivered.brk();
        }
        if b.mic_stamped {
            stamped.push(b.mic_index as f64, b.capture_qpc_hns);
        } else {
            stamped.brk();
        }
        // The samples delivered by the wake that brought the block's newest: the block's own and those still queued.
        delivered.push(b.mic_index as f64 + BLOCK + f64::from(b.mic_queued), b.mic_wake_hns);

        let o = &b.output;
        silence += f64::from(o.silence);
        if o.underrun || o.reanchored {
            output.brk();
        }
        output.push(o.clock_pos + silence, o.clock_qpc_hns);

        match &b.reference {
            Some(r) if r.stamped && b.mic_stamped => {
                if breaks_reference(r) {
                    reference.brk();
                }
                reference.push(r.read_index as f64, b.capture_qpc_hns - f64::from(r.lead_reading_ms) * 1e4);
            }
            _ => reference.brk(),
        }
    }
    let (mic, timing) = match stamped.fit() {
        Some(s) => (Some(s), Some(MicTiming::Stamps)),
        None => match delivered.fit() {
            Some(s) => (Some(s), Some(MicTiming::Delivery)),
            None => (None, None),
        },
    };
    // The reference's tap times come from the mic's stamps: without them, no reference line.
    let reference = if timing == Some(MicTiming::Stamps) { reference.fit() } else { None };
    ClockDrift {
        mic: timing,
        output: mic.zip(output.fit()).map(|(m, o)| against(m, o)),
        reference: mic.zip(reference).map(|(m, r)| against(m, r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ChainParams, OutputFacts};

    const H: f64 = 1e7 / 48_000.0;
    const T0: f64 = 1e10;

    /// A deterministic scatter in -1..1 for block `k`.
    fn scatter(k: u64, salt: u64) -> f64 {
        let mut x = (k + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt;
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// Block `k` of a run whose mic is on time by its stamps (which scatter up to 0.2 ms), an output device `out_ppm` fast
    /// whose clock the block read at its wake (its readings' times scatter 0.1 ms), and a loopback `ref_ppm` fast whose
    /// timeline is read 12 ms before the mic block (its stamps scatter 0.2 ms).
    fn block(k: u64, out_ppm: f64, ref_ppm: f64) -> ChainBlock {
        let capture = T0 + k as f64 * BLOCK * H + 2_000.0 * scatter(k, 1);
        let wake = capture + (BLOCK + 50.0) * H;
        // The device's clock: a reading at each 10 ms tick, at its rate.
        let out_h = H / (1.0 + out_ppm * 1e-6);
        let pos = ((wake - T0) / out_h / 480.0).floor() * 480.0;
        // The loopback's: the sample tapped 12 ms before the capture, by its rate, and its lead.
        let ref_h = H / (1.0 + ref_ppm * 1e-6);
        let read_index = ((capture - 120_000.0 - T0) / ref_h).round();
        let tap = T0 + read_index * ref_h + 2_000.0 * scatter(k, 2);
        ChainBlock {
            index: k,
            capture_qpc_hns: capture,
            mic_stamped: true,
            mic_index: k * 480,
            mic_wake_hns: wake,
            mic_queued: 0,
            mic_discarded: 0,
            mic_skipped: 0,
            reference: Some(ReferenceFacts {
                read_index: read_index as i64,
                lead_reading_ms: ((capture - tap) / 1e4) as f32,
                stamped: true,
                consumed: 480,
                ..Default::default()
            }),
            aec_delay_ms: f32::NAN,
            aec_erle_db: f32::NAN,
            settings: ChainParams::default(),
            budget: None,
            output: OutputFacts { clock_pos: pos, clock_qpc_hns: T0 + pos * out_h + 1_000.0 * scatter(k, 3), ..Default::default() },
        }
    }

    fn near(fit: Option<DriftFit>, ppm: f64, tol: f64) -> bool {
        fit.is_some_and(|f| (f.ppm - ppm).abs() <= tol)
    }

    /// Clocks on lines, their readings scattered: the output's and the reference's drift come out to within a few ppm, the
    /// error inside the uncertainty.
    #[test]
    fn lines_give_the_drift() {
        for (out_ppm, ref_ppm) in [(0.0, 0.0), (100.0, -80.0), (-300.0, 250.0)] {
            let blocks: Vec<_> = (0..1500).map(|k| block(k, out_ppm, ref_ppm)).collect();
            let d = clock_drift(&blocks);
            assert_eq!(d.mic, Some(MicTiming::Stamps));
            let (o, r) = (d.output.unwrap(), d.reference.unwrap());
            for (fit, ppm) in [(o, out_ppm), (r, ref_ppm)] {
                assert!((fit.ppm - ppm).abs() <= fit.uncertainty_ppm && fit.uncertainty_ppm < 5.0, "{out_ppm}, {ref_ppm}: {d:?}");
            }
            assert!((o.seconds - 15.0).abs() < 0.1, "{d:?}");
        }
    }

    /// Steps that break a line (a mic position skip, an output underrun with an exclusive device's silence, a re-anchor, a
    /// reference jump) don't move the slope; nor does a stretch without stamps.
    #[test]
    fn steps_dont_move_the_slope() {
        let mut blocks: Vec<_> = (0..1500).map(|k| block(k, 100.0, -50.0)).collect();
        for b in &mut blocks[500..] {
            // The mic's position skipped 4800 frames that weren't counted: its index runs 4800 behind its stamps.
            b.mic_index -= 4800;
            // The exclusive device played 96 frames of silence our clock leaves out.
            b.output.clock_pos -= 96.0;
            let r = b.reference.as_mut().unwrap();
            // The reference's timeline jumped back 480: the same samples are tapped as before, read later.
            r.read_index -= 480;
            r.lead_reading_ms += 10.0;
        }
        blocks[500].mic_skipped = 4800;
        blocks[500].output.silence = 96;
        blocks[500].output.underrun = true;
        blocks[500].reference.as_mut().unwrap().jumped = -480;
        for b in &mut blocks[1000..] {
            b.output.clock_pos += 1000.0;
        }
        blocks[1000].output.reanchored = true;
        for b in &mut blocks[700..760] {
            b.mic_stamped = false;
            b.capture_qpc_hns = f64::NAN;
        }
        let d = clock_drift(&blocks);
        assert!(near(d.output, 100.0, 3.0) && near(d.reference, -50.0, 3.0), "{d:?}");
    }

    /// Without the mic's stamps its delivery times it, wakes 0.5-10.5 ms late in turn: the output's drift still comes out,
    /// less precisely, and the reference's (timed by the stamps) not at all. Too short a span gives none.
    #[test]
    fn without_stamps_the_delivery_times_the_mic() {
        let mut blocks: Vec<_> = (0..1500).map(|k| block(k, 100.0, 0.0)).collect();
        for (k, b) in blocks.iter_mut().enumerate() {
            b.mic_stamped = false;
            b.capture_qpc_hns = f64::NAN;
            b.mic_wake_hns += if k % 2 == 0 { 0.0 } else { 1e5 };
        }
        let d = clock_drift(&blocks);
        assert_eq!((d.mic, d.reference), (Some(MicTiming::Delivery), None));
        let o = d.output.unwrap();
        assert!((o.ppm - 100.0).abs() < 10.0 && o.uncertainty_ppm < 100.0, "{o:?}");
        let short = clock_drift(&blocks[..250]);
        assert_eq!((short.mic, short.output), (None, None), "2.5 s is too short");
    }
}
