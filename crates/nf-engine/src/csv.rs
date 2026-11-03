//! The chain recorder's blocks as CSV, a row a block (`nf_engine::chain_blocks_csv`: `quality --save`'s `chain_blocks.csv`, and
//! the flight recorder's `flight_blocks.csv`, which puts its own columns first: flight.rs).
//!
//! The columns, a row a block (`ChainBlock`): the block's index and the mic's timing and moves (samples its FIFO discarded,
//! device position it skipped unfilled); the reference's take (`ReferenceFacts`; empty without a reference), with `path`, the
//! mic's index less the reference's, the echo path as the engine holds it (it moves by `mic_discarded` on the mic's side, and
//! on the reference's by a slip, `consumed` other than 480, and a jump, `jumped`); AEC3's own estimates; 1 where the chain's
//! settings changed from the block before; the latency readout's budget (`LatencyBudget`; empty without one): mic delivery,
//! wait, the chain's offset, the output queue and the render side in ms, and the readout, which is mic delivery + wait +
//! queue + render side (the chain's offset isn't in it); what the output side did (`OutputFacts`, TODO 9): the device-stream
//! index the block's first sample went to (block to block it moves by the last block's 480 and its slip, or nothing when that
//! one was skipped, plus this block's zeros less its dropped samples), the device's padding and our pending samples when it
//! was written, the fill the drift loop read and its target (samples), the slip (-1, 0, +1), the zeros written (a deficit),
//! the pending samples dropped, 1 where the block itself was dropped (skipped), an underrun, a re-anchor of the fill's
//! calibration, the silence an exclusive device played since the block before (samples), and the device's clock as the block
//! read it (its position in our stream's frames, and the QPC time it belongs to: `clock_drift` fits the drift from those); and
//! what the audio thread did (`ThreadFacts`): the wake that processed the block, the longest gap between wakes since the block
//! before (ms; 0 for a later block of the same wake), the wakes since the block before's and those of them the exclusive
//! output's event made (0 with a shared output), the stalls (wakes with nothing from the mic 100 ms after its last), the mic's
//! glitches, the block's processing time (µs) and a model pack's misses. Times are QPC in 100 ns units; a number
//! that's unknown is empty.

use std::fmt::Write as _;

use crate::api::ChainBlock;

#[rustfmt::skip]
pub(crate) const COLUMNS: &str = concat!(
    "index,capture_qpc_hns,mic_stamped,mic_index,mic_wake_hns,mic_queued,mic_discarded,mic_skipped,",
    "ref_read_index,path,lead_reading_ms,lead_ms,target_ms,drift_ppm,ref_stamped,fresh,delivered,buffered,",
    "consumed,owed,missing,jumped,events,aec_delay_ms,aec_erle_db,settings_changed,",
    "mic_delivery_ms,wait_ms,chain_ms,queue_ms,render_ms,readout_ms,",
    "out_index,out_padding,out_pending,out_fill,out_target,out_slip,out_zeros,out_dropped,out_skipped,",
    "out_underrun,out_reanchored,out_silence,out_clock_pos,out_clock_qpc_hns,",
    "wake_hns,wake_gap_ms,wakes,out_wakes,stalls,mic_glitches,process_us,model_misses",
);
const BUDGET_COLUMNS: usize = 6;
const REFERENCE_COLUMNS: usize = 15;

/// `v` with `places` decimals; empty when it isn't a number.
pub(crate) fn num(v: f64, places: usize) -> String {
    if v.is_finite() { format!("{v:.places$}") } else { String::new() }
}

/// [`COLUMNS`], then a row a block.
pub(crate) fn chain_blocks(blocks: &[ChainBlock]) -> String {
    let mut s = String::with_capacity((blocks.len() + 1) * 200);
    s.push_str(COLUMNS);
    s.push('\n');
    let mut last = None;
    for b in blocks {
        row(&mut s, b, last);
        s.push('\n');
        last = Some(b);
    }
    s
}

/// One block's row (without its line end); `last` is the block before, for `settings_changed`.
pub(crate) fn row(s: &mut String, b: &ChainBlock, last: Option<&ChainBlock>) {
    let _ = write!(
        s,
        "{},{},{},{},{},{},{},{}",
        b.index,
        num(b.capture_qpc_hns, 1),
        u8::from(b.mic_stamped),
        b.mic_index,
        num(b.mic_wake_hns, 1),
        b.mic_queued,
        b.mic_discarded,
        b.mic_skipped
    );
    match &b.reference {
        Some(r) => {
            let events: Vec<&str> = r.event_names().collect();
            let _ = write!(
                s,
                ",{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                r.read_index,
                i128::from(b.mic_index) - i128::from(r.read_index),
                num(f64::from(r.lead_reading_ms), 3),
                num(f64::from(r.lead_ms), 3),
                num(f64::from(r.target_ms), 3),
                num(f64::from(r.drift_ppm), 2),
                u8::from(r.stamped),
                u8::from(r.fresh),
                u8::from(r.delivered),
                r.buffered,
                r.consumed,
                r.owed,
                r.missing,
                r.jumped,
                events.join("|")
            );
        }
        None => s.push_str(&",".repeat(REFERENCE_COLUMNS)),
    }
    let changed = last.is_some_and(|l| !b.settings.same(&l.settings));
    let _ = write!(s, ",{},{},{}", num(f64::from(b.aec_delay_ms), 1), num(f64::from(b.aec_erle_db), 2), u8::from(changed));
    match &b.budget {
        Some(t) => {
            for v in [t.mic_ms, t.wait_ms, t.chain_ms, t.queue_ms, t.render_ms, t.readout_ms()] {
                let _ = write!(s, ",{}", num(f64::from(v), 3));
            }
        }
        None => s.push_str(&",".repeat(BUDGET_COLUMNS)),
    }
    let o = &b.output;
    let _ = write!(
        s,
        ",{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        o.index,
        o.padding,
        o.pending,
        num(f64::from(o.fill), 1),
        o.target,
        o.slip,
        o.zeros,
        o.dropped,
        u8::from(o.skipped),
        u8::from(o.underrun),
        u8::from(o.reanchored),
        o.silence,
        num(o.clock_pos, 1),
        num(o.clock_qpc_hns, 1)
    );
    let t = &b.thread;
    let wake = if t.wake_hns > 0.0 { num(t.wake_hns, 1) } else { String::new() };
    let _ = write!(
        s,
        ",{wake},{},{},{},{},{},{},{}",
        num(f64::from(t.wake_gap_ms), 3),
        t.wakes,
        t.out_wakes,
        t.stalls,
        t.mic_glitches,
        num(f64::from(t.process_us), 1),
        t.model_misses
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{OutputFacts, ReferenceFacts, ThreadFacts};

    /// Every row has the header's columns, with a reference or without, with a budget or without.
    #[test]
    fn every_row_has_the_headers_columns() {
        let blank = crate::recorder::ChainItem::BLANK.block;
        let with_ref = ChainBlock { reference: Some(ReferenceFacts::default()), ..blank };
        let thread = ThreadFacts {
            wake_hns: 1e10,
            wake_gap_ms: 2.5,
            wakes: 4,
            out_wakes: 3,
            stalls: 1,
            mic_glitches: 2,
            process_us: 90.0,
            model_misses: 3,
        };
        let full = ChainBlock { output: OutputFacts::default(), thread, ..with_ref };
        let csv = chain_blocks(&[blank, with_ref, full]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!((lines.len(), lines[0]), (4, COLUMNS));
        let n = COLUMNS.split(',').count();
        assert!(lines.iter().all(|l| l.split(',').count() == n), "{csv}");
        assert!(lines[3].ends_with(",10000000000.0,2.500,4,3,1,2,90.0,3"), "{}", lines[3]);
        assert!(lines[1].ends_with(",,0.000,0,0,0,0,0.0,0"), "an unknown wake is empty: {}", lines[1]);
    }
}
