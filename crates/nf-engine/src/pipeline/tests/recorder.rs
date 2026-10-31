//! The chain recorder on the audio path (recorder.rs): it gets exactly what the chain got, block for block and in order,
//! with the chain's output beside it; its facts account for every move of the echo path; a full ring loses whole blocks,
//! counted; a change of the chain's settings shows; and the live chain reruns offline, sample for sample, from a recording
//! that starts at the run's first block.

use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

use nf_core::config::ProcessingConfig;
use nf_core::controls::ChainControls;
use nf_stages::synth;
use rtrb::{Consumer, RingBuffer};

use super::timeline::{Delivery, Ev, Row, Scene, Stamps, cable_pipe, index_coded, replay_into, scene_rows};
use super::*;
use crate::api::{ChainRecording, ReferenceFacts};
use crate::recorder::{ChainItem, ChainTx, add};

/// Computes an output of its own (the mic less half the reference) and keeps what it got and gave.
#[derive(Default)]
struct Spy {
    mic: Vec<f32>,
    reference: Vec<f32>,
    out: Vec<f32>,
}

impl BlockProcessor for Spy {
    fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
        for ((o, &m), &x) in out.iter_mut().zip(mic).zip(r) {
            *o = m - 0.5 * x;
        }
        self.mic.extend_from_slice(mic);
        self.reference.extend_from_slice(r);
        self.out.extend_from_slice(out);
        ChainReport::default()
    }
    fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
        (Some(StageStats { echo_delay_ms: 40.0, erle_db: 9.5 }), None)
    }
}

fn drain(rx: &mut Consumer<ChainItem>, rec: &mut ChainRecording) {
    while let Ok(item) = rx.pop() {
        add(rec, &item);
    }
}

/// Replays `rows` through the cables' pipeline with the recorder armed from the first block (a ring big enough for all of
/// it), and returns the pipeline and the recording.
fn recorded<P: BlockProcessor>(
    rows: &[Row], proc: &mut P, mic_of: &dyn Fn(u64, usize) -> Vec<f32>, lb_of: &dyn Fn(u64, usize) -> Vec<f32>,
) -> (Pipeline, ChainRecording) {
    let blocks = rows.last().map_or(0, |r| (r.now / 1e5) as usize) + 100;
    let (tx, mut rx) = RingBuffer::<ChainItem>::new(blocks);
    let mut p = cable_pipe();
    p.recorder = Some(ChainTx::new(tx));
    let p = replay_into(p, rows, proc, mic_of, lb_of);
    let mut rec = ChainRecording::default();
    drain(&mut rx, &mut rec);
    (p, rec)
}

/// Every move of the echo path is in the facts: block to block, the mic advances a block plus what its FIFO discarded, and
/// the reference's read position by what the block before consumed plus this block's jumps. The counters agree with them:
/// a gap is a block with owed zeros (or none delivered yet), a slip one that consumed 479 or 481, and the first reading
/// aligns once.
fn check_facts(p: &Pipeline, rec: &ChainRecording, what: &str) {
    let facts: Vec<ReferenceFacts> = rec.blocks.iter().map(|b| b.reference.expect("a reference")).collect();
    for (k, (w, f)) in rec.blocks.windows(2).zip(facts.windows(2)).enumerate() {
        assert_eq!(w[1].mic_index, w[0].mic_index + BLOCK as u64 + w[1].mic_discarded, "{what}: block {}", k + 1);
        assert_eq!(f[1].read_index, f[0].read_index + i64::from(f[0].consumed) + f[1].jumped, "{what}: block {}: {:?}", k + 1, f[1]);
    }
    let count = |pred: &dyn Fn(&ReferenceFacts) -> bool| facts.iter().filter(|f| pred(f)).count() as u64;
    let c = p.counters;
    assert_eq!(c.reference_gaps, count(&|f| f.owed > 0 || f.events & ReferenceFacts::NO_READING != 0), "{what}: {c:?}");
    assert_eq!(c.reference_slips, count(&|f| f.consumed == 479 || f.consumed == 481), "{what}: {c:?}");
    assert_eq!(count(&|f| f.events & ReferenceFacts::ALIGNED != 0), 1, "{what}");
    // Once aligned, every block has its lead reading (none here is off by more than the loop's own jitter and the events').
    let first = facts.iter().position(|f| f.events & ReferenceFacts::ALIGNED != 0).unwrap();
    assert!(facts[first..].iter().all(|f| f.lead_reading_ms.is_finite() && f.target_ms > 0.0), "{what}");
}

/// The recorder gets the blocks exactly as the chain got them, in order and none missing, with the chain's output for
/// each, and the block's facts. Over the cables' bursty delivery with a loopback stall, a mic loss that breaks the
/// timeline, a mic stall past its FIFO, and without stamps at 300 ppm, the facts account for every move of the echo path.
#[test]
fn the_recorder_gets_exactly_what_the_chain_got() {
    use Delivery::Bursty as B;
    let mut seen = 0u16;
    #[rustfmt::skip]
    let scenes = [
        ("bursty, a loopback stall", Scene::new(B, B, 30.0).at(Ev::LoopbackStall(100.0), 8.0)),
        ("bursty, 200 ms of mic lost", Scene::new(B, B, 30.0).at(Ev::MicLost(200.0), 8.0)),
        ("bursty, the mic stalled 400 ms", Scene::new(B, B, 30.0).at(Ev::MicStall(400.0), 8.0)),
        ("no stamps, 300 ppm", Scene::new(B, B, 300.0).with_stamps(Stamps::Neither)),
    ];
    for (what, sc) in scenes {
        let rows = scene_rows(&sc, 20.0);
        let mut spy = Spy::default();
        let (p, rec) = recorded(&rows, &mut spy, &index_coded, &index_coded);
        assert!(rec.blocks.len() > 1900 && rec.reproducible(), "{what}: {} blocks", rec.blocks.len());
        assert!(rec.settings_changed_at().is_none() && rec.lost_blocks == 0, "{what}");
        assert!(rec.blocks.iter().enumerate().all(|(k, b)| b.index == k as u64), "{what}: in order from block 0");
        assert!(rec.mic == spy.mic && rec.reference == spy.reference && rec.output == spy.out, "{what}: what the chain got and gave");
        assert_eq!(rec.blocks.len() as u64, p.blocks);
        let b = rec.blocks[100];
        let stamped = sc.stamps == Stamps::Both;
        assert_eq!(b.capture_qpc_hns.is_finite() && b.mic_wake_hns > b.capture_qpc_hns && b.mic_stamped, stamped, "{what}: {b:?}");
        assert_eq!((b.aec_delay_ms, b.aec_erle_db), (40.0, 9.5));
        check_facts(&p, &rec, what);
        // The mic's own moves: the stall overflows its 200 ms FIFO (the oldest audio goes, before the first block after
        // it), and the loss skips 200 ms of device position that isn't zero-filled. Neither happens otherwise.
        let discarded: u64 = rec.blocks.iter().map(|b| b.mic_discarded).sum();
        let skipped: i64 = rec.blocks.iter().map(|b| b.mic_skipped).sum();
        let stalled = matches!(sc.event, Ev::MicStall(_));
        assert_eq!(rec.blocks.iter().filter(|b| b.mic_discarded > 0).count(), usize::from(stalled), "{what}");
        assert!(if stalled { discarded >= 9_000 } else { discarded == 0 }, "{what}: {discarded}");
        assert_eq!(skipped, if matches!(sc.event, Ev::MicLost(_)) { 9_600 } else { 0 }, "{what}");
        for f in rec.blocks.iter().filter_map(|b| b.reference) {
            seen |= f.events;
        }
    }
    let names: Vec<&str> = ReferenceFacts { events: seen, ..ReferenceFacts::default() }.event_names().collect();
    for e in ["aligned", "broken", "retargeted", "coasted"] {
        assert!(names.contains(&e), "{e} not among {names:?}");
    }
}

/// A ring the recorder's thread doesn't keep up with loses whole blocks: the audio path carries on, the indices skip the
/// lost ones, and the audio thread counts them all, those after the last block recorded too (the recorder's thread reads
/// that count as it stops). Armed later, the recording starts at that block and isn't reproducible.
#[test]
fn a_full_ring_loses_whole_blocks_and_the_audio_path_carries_on() {
    let mut p = pipe(true);
    let mut sink = FakeSink::new(4800);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let mut spy = Spy::default();
    let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
    let (tx, mut rx) = RingBuffer::<ChainItem>::new(4);
    let tx = ChainTx::new(tx);
    let lost = tx.lost();
    let mut tx = Some(tx);
    let mut rec = ChainRecording::default();
    for b in 0..30usize {
        if b == 5 {
            p.recorder = tx.take();
        }
        mic.push((b * 480) as u64, None, ramp(b * 480, 480));
        lb.push_frames(480, 0.25);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.drain_reference(&mut lb, 0.0).unwrap();
        assert!(p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap());
        sink.advance(480.0, 100_000.0);
        // The thread drains at blocks 14 and 29 only: blocks 9-14 and 19-29 don't fit.
        if b == 14 || b == 29 {
            drain(&mut rx, &mut rec);
        }
    }
    let indices: Vec<u64> = rec.blocks.iter().map(|b| b.index).collect();
    assert_eq!(indices, [5, 6, 7, 8, 15, 16, 17, 18]);
    assert_eq!(rec.lost_blocks, 6, "9 to 14, by the blocks' own counts");
    assert_eq!(lost.load(Acquire), 6 + 11, "and 19 to 29, after the last block taken");
    assert!(!rec.from_first_block() && !rec.reproducible());
    // What was recorded is intact, block-aligned with the chain's own record.
    for (k, b) in rec.blocks.iter().enumerate() {
        let at = b.index as usize * BLOCK;
        assert_eq!(rec.mic[k * BLOCK..(k + 1) * BLOCK], spy.mic[at..at + BLOCK]);
        assert_eq!(rec.output[k * BLOCK..(k + 1) * BLOCK], spy.out[at..at + BLOCK]);
    }
    assert_eq!(spy.mic.len(), 30 * BLOCK, "every block processed");
}

/// The chain's settings are recorded with each block, as the chain read them: a change during the recording (an
/// `Engine::apply`, a latency test's passthrough) shows at its block, and the recording is no longer reproducible with one
/// config.
#[test]
fn a_settings_change_shows_and_ends_the_reproducible_part() {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let mut chain = Chain::new(controls.clone(), None, None);
    chain.activate().unwrap();
    let mut p = pipe(true);
    let mut sink = FakeSink::new(4800);
    p.prefill(&mut sink).unwrap();
    let tel = Telemetry::new();
    let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
    let (tx, mut rx) = RingBuffer::<ChainItem>::new(64);
    p.recorder = Some(ChainTx::new(tx));
    for b in 0..30usize {
        match b {
            10 => controls.passthrough.store(true, Relaxed),
            20 => controls.noise.floor_db.store(-12.0),
            _ => {}
        }
        mic.push((b * 480) as u64, None, ramp(b * 480, 480));
        lb.push_frames(480, 0.25);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.drain_reference(&mut lb, 0.0).unwrap();
        assert!(p.process_block(&mut chain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap());
        sink.advance(480.0, 100_000.0);
    }
    let mut rec = ChainRecording::default();
    drain(&mut rx, &mut rec);
    assert_eq!(rec.blocks.len(), 30);
    let first = rec.blocks[0].settings;
    assert!(first.same(&ChainControls::new(&ProcessingConfig::default()).load()), "{first:?}");
    assert!(rec.blocks[10].settings.passthrough && rec.blocks[20].settings.floor_db == -12.0);
    assert_eq!(rec.settings_changed_at(), Some(10));
    assert!(rec.from_first_block() && rec.lost_blocks == 0 && !rec.reproducible());
    let before = ChainRecording { blocks: rec.blocks[..10].to_vec(), ..rec.clone() };
    assert!(before.reproducible(), "the blocks before the change are");
}

/// The offline rerun (TODO 10, "Next" 1): the real chain (AEC3 and RNNoise) behind the pipeline, over the cables' bursty
/// delivery with a loopback stall, its echo 60 ms behind the loopback. A chain built and warmed up as the engine's
/// (`warm_up_chain`), fed the recording's mic and reference block by block, reproduces its output sample for sample; one
/// that skips the warm-up doesn't. `noise-filter process --warm-up` does the same with the saved WAVs.
#[test]
fn the_live_chain_reruns_offline_sample_for_sample() {
    let cfg = ProcessingConfig { aec_enabled: true, ..ProcessingConfig::default() };
    let chain = |warm: bool| {
        let mut c = nf_stages::build_chain(Arc::new(ChainControls::new(&cfg)), true).unwrap();
        if warm {
            crate::api::warm_up_chain(&mut c);
        }
        c
    };
    let rows = scene_rows(&Scene::new(Delivery::Bursty, Delivery::Bursty, 30.0).at(Ev::LoopbackStall(100.0), 6.0), 12.0);
    let len = 14 * 48_000;
    let far = synth::speech_like(len, synth::Voice::HIGH, 22).samples;
    let near = synth::speech_like(len, synth::Voice::LOW, 21).samples;
    let noise = synth::pink_noise(len, 1e-4, 23);
    let at = |x: &[f32], i: i64| usize::try_from(i).ok().and_then(|i| x.get(i)).copied().unwrap_or(0.0);
    let mic_of = |code: u64, n: usize| -> Vec<f32> {
        (code as i64..code as i64 + n as i64).map(|i| 0.5 * at(&far, i - 2_880) + 0.25 * at(&near, i) + at(&noise, i)).collect()
    };
    let lb_of = |code: u64, n: usize| -> Vec<f32> { (code as i64..code as i64 + n as i64).map(|i| at(&far, i)).collect() };

    let mut live = chain(true);
    let (p, rec) = recorded(&rows, &mut live, &mic_of, &lb_of);
    assert!(rec.reproducible() && rec.blocks.len() > 1100, "{} blocks", rec.blocks.len());
    assert!(p.counters.reference_gaps > 0, "the stall owed some blocks: {:?}", p.counters);
    assert!(rec.blocks.last().is_some_and(|b| b.aec_erle_db.is_finite()), "AEC3 reported");

    let rerun = |c: &mut Chain| -> Vec<f32> {
        let mut out = Vec::with_capacity(rec.output.len());
        let mut y = [0.0f32; BLOCK];
        for (m, r) in rec.mic.as_chunks::<BLOCK>().0.iter().zip(rec.reference.as_chunks::<BLOCK>().0) {
            c.process(m, r, &mut y);
            out.extend_from_slice(&y);
        }
        out
    };
    let offline = rerun(&mut chain(true));
    let first_diff = offline.iter().zip(&rec.output).position(|(a, b)| a.to_bits() != b.to_bits());
    assert_eq!(first_diff, None, "the rerun matches the live output sample for sample");
    assert_eq!(offline.len(), rec.output.len());
    let cold = rerun(&mut chain(false));
    assert_ne!(cold, rec.output, "without the warm-up the chain starts from another state");
}
