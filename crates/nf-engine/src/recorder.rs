//! The chain recorder (engine.md, "The chain recorder"; TODO 10, 14): what the chain was fed live and what it made of it.
//!
//! The audio thread pushes every block it processes (the mic block and the reference block as the chain got them, the
//! chain's output, and the block's facts: [`ChainBlock`]) into a pre-allocated rtrb ring while a recorder is armed; a full
//! ring loses the block, and the audio thread counts it ([`ChainTx`]). The ring goes to the audio thread and comes back
//! through the run's hand-over rings (handover.rs), as the latency test's tap does, so it's never freed there. A thread of
//! the recorder's own moves the blocks into memory every 20 ms, up to the length asked for.
//!
//! Armed while no run is live, the producer waits here (`pending`) and the next start puts it in its run's command ring
//! before handing the run over, so the run's first wake takes it before its first block; a start that fails hands it back
//! (`run_failed`) for the next. Armed during a live run, it goes straight to that run's command ring. Stopping drops the
//! ring's consumer; the audio thread sees the producer abandoned at its next wake and hands it back, and the control side
//! frees it.
//!
//! The recording so far sits behind a mutex the drain thread holds while it appends (a few blocks every 20 ms), so the control
//! side can copy from it while it runs (`Handle::copy_from`). Neither is the audio thread, which only pushes into the ring.

use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nf_core::{BLOCK, Block};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::api::{ChainBlock, ChainParams, ChainRecording, EngineError, GateMode, GateParams, MAX_CHAIN_RECORDING_S};
use crate::control::{Inner, lock};
use crate::handover::{self, End, Handed, Port};

/// The ring's capacity: 4 s of blocks (about 2.4 MB). The drain thread empties it every 20 ms.
const RING_BLOCKS: usize = 400;
const DRAIN_EVERY: Duration = Duration::from_millis(20);
/// How long a stop waits for the audio thread to hand the ring back (it looks once a wake, every 10 ms or so).
const RETURN_WAIT: Duration = Duration::from_millis(500);
/// Memory reserved up front, in blocks (30 s); more as it's needed.
const RESERVE_BLOCKS: usize = 3_000;

/// One recorded block, as the audio thread pushes it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChainItem {
    pub(crate) block: ChainBlock,
    /// Blocks the ring had no room for before this one (since the recorder was armed).
    pub(crate) lost: u64,
    pub(crate) mic: Block,
    pub(crate) reference: Block,
    pub(crate) output: Block,
}

impl ChainItem {
    const BLANK: Self = Self {
        block: ChainBlock {
            index: 0,
            capture_qpc_hns: 0.0,
            mic_stamped: false,
            mic_index: 0,
            mic_wake_hns: 0.0,
            mic_queued: 0,
            mic_discarded: 0,
            mic_skipped: 0,
            reference: None,
            aec_delay_ms: 0.0,
            aec_erle_db: 0.0,
            settings: ChainParams {
                aec_enabled: false,
                noise_enabled: false,
                passthrough: false,
                floor_db: 0.0,
                gate: GateParams {
                    mode: GateMode::Off,
                    threshold_db: 0.0,
                    auto_margin_db: 0.0,
                    attack_ms: 0.0,
                    release_ms: 0.0,
                    hold_ms: 0.0,
                    range_db: 0.0,
                    use_vad: false,
                },
            },
            budget: None,
        },
        lost: 0,
        mic: [0.0; BLOCK],
        reference: [0.0; BLOCK],
        output: [0.0; BLOCK],
    };
}

/// What the audio thread holds while it records: the ring's producer, and the count of blocks the ring had no room for,
/// which the recorder reads as it ends (blocks lost after the last one it got show only there).
pub(crate) struct ChainTx {
    ring: Producer<ChainItem>,
    lost: Arc<AtomicU64>,
}

impl ChainTx {
    pub(crate) fn new(ring: Producer<ChainItem>) -> Self {
        Self { ring, lost: Arc::new(AtomicU64::new(0)) }
    }

    /// The count of blocks lost, shared with the recorder's thread.
    pub(crate) fn lost(&self) -> Arc<AtomicU64> {
        self.lost.clone()
    }

    /// On the audio thread, once a block: pushes it, with the count of blocks lost before it, or counts it lost when the
    /// ring is full. Lock-free and allocation-free.
    #[inline]
    pub(crate) fn push(&mut self, block: ChainBlock, mic: &Block, reference: &Block, output: &Block) {
        // Only this thread writes the count.
        let lost = self.lost.load(Relaxed);
        if self.ring.push(ChainItem { block, lost, mic: *mic, reference: *reference, output: *output }).is_err() {
            self.lost.store(lost + 1, Release);
        }
    }
}

impl Handed for ChainTx {
    fn is_abandoned(&self) -> bool {
        self.ring.is_abandoned()
    }
}

/// The control side's recorder state (`Inner::recorder`).
#[derive(Default)]
pub(crate) struct Slot {
    /// Armed while no run was live: the next start hands it to its run.
    pending: Option<ChainTx>,
    /// The live run's hand-over port.
    port: Option<Port<ChainTx>>,
}

/// A start's hand-over rings, made before the run is handed to the audio thread: a recorder armed before the start is put
/// in the command ring now, so the run's first wake takes it before the run's first block.
pub(crate) fn run_channel(inner: &Inner) -> (Port<ChainTx>, End<ChainTx>) {
    let (mut port, end) = handover::channel();
    let mut slot = lock(&inner.recorder);
    if let Some(p) = slot.pending.take()
        && let Err(PushError::Full(p)) = port.cmd.push(p)
    {
        // A fresh ring has room; kept for the next start if not.
        slot.pending = Some(p);
    }
    (port, end)
}

/// The run started: the control side keeps its port. A recorder armed while it started goes to it now (the run's first
/// blocks may come before it).
pub(crate) fn run_started(inner: &Inner, mut port: Port<ChainTx>) {
    let mut slot = lock(&inner.recorder);
    if let Some(p) = slot.pending.take()
        && let Err(PushError::Full(p)) = port.cmd.push(p)
    {
        slot.pending = Some(p);
    }
    slot.port = Some(port);
}

/// The start failed: a recorder it was given, which its audio thread hands back untaken as the run ends (`run_job`), waits
/// for the next start, unless the recorder was stopped meanwhile (then it's freed here). A thread that hung, or died, didn't
/// hand it back: the recorder records nothing more.
pub(crate) fn run_failed(inner: &Inner, mut port: Port<ChainTx>) {
    if let Ok(p) = port.ret.pop()
        && !p.is_abandoned()
    {
        let mut slot = lock(&inner.recorder);
        // One recorder at a time: a live one can't have another pending.
        if slot.pending.is_none() {
            slot.pending = Some(p);
        }
    }
}

/// The run stopped: its port goes (with any producer it handed back, freed here). A recorder that went to it has ended.
pub(crate) fn run_stopped(inner: &Inner) {
    lock(&inner.recorder).port = None;
}

/// What the recorder's handle and its thread share.
#[derive(Default)]
struct DrainState {
    stop: AtomicBool,
    blocks: AtomicU64,
    lost: AtomicU64,
    /// The recording so far: the thread appends to it, the handle copies from it and takes it at the end.
    rec: Mutex<ChainRecording>,
}

/// The recorder's handle (`ChainRecorder`): its thread, and the engine it records.
pub(crate) struct Handle {
    inner: Weak<Inner>,
    state: Arc<DrainState>,
    thread: Option<JoinHandle<()>>,
}

/// Arms a recorder for up to `max_seconds`: allocates the ring (touching its pages, so the audio thread never faults on
/// them), starts the thread that drains it, and hands the producer to the live run, or keeps it for the next start.
pub(crate) fn arm(inner: &Arc<Inner>, max_seconds: f64) -> Result<Handle, EngineError> {
    if !(max_seconds.is_finite() && max_seconds > 0.0 && max_seconds <= MAX_CHAIN_RECORDING_S) {
        return Err(EngineError(format!("The chain recorder records for up to {} minutes.", MAX_CHAIN_RECORDING_S / 60.0)));
    }
    if inner.recording.swap(true, AcqRel) {
        return Err(EngineError("The chain is being recorded already.".into()));
    }
    let (mut ring, mut rx) = RingBuffer::<ChainItem>::new(RING_BLOCKS);
    while ring.push(ChainItem::BLANK).is_ok() {}
    while rx.pop().is_ok() {}
    let tx = ChainTx::new(ring);
    let lost = tx.lost();
    let state = Arc::new(DrainState::default());
    let max_blocks = ((max_seconds * 100.0).round() as usize).max(1);
    let s = state.clone();
    let spawned = std::thread::Builder::new().name("nf-chain-rec".into()).spawn(move || drain(rx, &lost, &s, max_blocks));
    let thread = match spawned {
        Ok(t) => t,
        Err(_) => {
            inner.recording.store(false, Release);
            return Err(EngineError("The chain recorder couldn't start a thread.".into()));
        }
    };
    // From here the handle's drop stops the thread, takes the producer back and releases the claim.
    let mut handle = Handle { inner: Arc::downgrade(inner), state, thread: Some(thread) };
    let mut slot = lock(&inner.recorder);
    match slot.port.as_mut() {
        Some(port) => {
            // Producers an earlier recording handed back: freed here.
            while port.ret.pop().is_ok() {}
            if port.cmd.push(tx).is_err() {
                drop(slot);
                // Never handed over: nothing to take back.
                handle.finish(false);
                return Err(EngineError("The audio thread didn't take the chain recorder; try again.".into()));
            }
        }
        // One recorder at a time: there's no other pending.
        None => slot.pending = Some(tx),
    }
    Ok(handle)
}

impl Handle {
    pub(crate) fn blocks(&self) -> u64 {
        self.state.blocks.load(Relaxed)
    }

    pub(crate) fn lost_blocks(&self) -> u64 {
        self.state.lost.load(Relaxed)
    }

    /// The blocks recorded so far from position `from` on, copied (`ChainRecorder::copy_from`).
    pub(crate) fn copy_from(&self, from: usize) -> ChainRecording {
        copy_from(&lock(&self.state.rec), from)
    }

    pub(crate) fn stop(mut self) -> ChainRecording {
        self.finish(true)
    }

    /// Stops the thread (it drains the ring once more and drops its consumer), takes the producer back if it was `handed`
    /// over, and releases the claim. Once.
    fn finish(&mut self, handed: bool) -> ChainRecording {
        let Some(thread) = self.thread.take() else { return ChainRecording::default() };
        self.state.stop.store(true, Release);
        // A thread that panicked leaves what it had appended.
        let _ = thread.join();
        let rec = std::mem::take(&mut *lock(&self.state.rec));
        if let Some(inner) = self.inner.upgrade() {
            if handed && !rec.run_ended {
                reclaim(&inner);
            }
            inner.recording.store(false, Release);
        }
        rec
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.finish(true);
    }
}

/// The producer back from wherever it is: still pending (armed before a start that didn't come), or with the live run,
/// which hands it back at its next wake now that its consumer has gone. Not waited for past [`RETURN_WAIT`]: then it's
/// freed with the run's port, at the next arm or when the run stops.
fn reclaim(inner: &Inner) {
    if lock(&inner.recorder).pending.take().is_some() {
        return;
    }
    let deadline = Instant::now() + RETURN_WAIT;
    loop {
        {
            let mut slot = lock(&inner.recorder);
            match slot.port.as_mut() {
                Some(port) => {
                    if port.ret.pop().is_ok() {
                        return;
                    }
                }
                // No live run: it went with the run.
                None => return,
            }
        }
        if Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The recorder's thread: moves the ring's blocks into the recording (`state.rec`) every [`DRAIN_EVERY`] until it's stopped,
/// the limit is reached, or the producer has gone (the run ended). `lost` is the audio thread's count of blocks the ring had no
/// room for.
fn drain(mut rx: Consumer<ChainItem>, lost: &AtomicU64, state: &DrainState, max_blocks: usize) {
    {
        let mut guard = lock(&state.rec);
        let rec = &mut *guard;
        let reserve = max_blocks.min(RESERVE_BLOCKS);
        for v in [&mut rec.mic, &mut rec.reference, &mut rec.output] {
            v.reserve(reserve * BLOCK);
        }
        rec.blocks.reserve(reserve);
    }
    loop {
        // Read before draining: once the producer has gone, or the recorder is stopped, this drain takes every block pushed
        // before, and the count every block lost before.
        let (stop, ended, lost_now) = (state.stop.load(Acquire), rx.is_abandoned(), lost.load(Acquire));
        {
            let mut rec = lock(&state.rec);
            while let Ok(item) = rx.pop() {
                if rec.blocks.len() >= max_blocks {
                    rec.truncated = true;
                    break;
                }
                add(&mut rec, &item);
            }
            // The blocks' own counts cover the losses before the last block; those since show only in the audio thread's
            // count. Cut at the limit, those are past the recording's end.
            let lost = if rec.truncated { rec.lost_blocks } else { rec.lost_blocks.max(lost_now) };
            state.blocks.store(rec.blocks.len() as u64, Relaxed);
            state.lost.store(lost, Relaxed);
            if rec.truncated {
                return;
            }
            if stop || ended {
                rec.lost_blocks = lost;
                rec.run_ended = !stop;
                return;
            }
        }
        std::thread::sleep(DRAIN_EVERY);
    }
}

/// `rec`'s blocks from position `from` on (none past its end), with its counts and flags.
fn copy_from(rec: &ChainRecording, from: usize) -> ChainRecording {
    let from = from.min(rec.blocks.len());
    let at = from * BLOCK;
    let tail = |v: &Vec<f32>| v.get(at..).unwrap_or_default().to_vec();
    ChainRecording {
        mic: tail(&rec.mic),
        reference: tail(&rec.reference),
        output: tail(&rec.output),
        blocks: rec.blocks[from..].to_vec(),
        lost_blocks: rec.lost_blocks,
        truncated: rec.truncated,
        run_ended: rec.run_ended,
    }
}

/// Appends one block, and the count of blocks lost before it.
pub(crate) fn add(rec: &mut ChainRecording, item: &ChainItem) {
    rec.lost_blocks = rec.lost_blocks.max(item.lost);
    rec.mic.extend_from_slice(&item.mic);
    rec.reference.extend_from_slice(&item.reference);
    rec.output.extend_from_slice(&item.output);
    rec.blocks.push(item.block);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(index: u64) -> ChainBlock {
        ChainBlock { index, ..ChainItem::BLANK.block }
    }

    /// Block `index`'s samples: its index (mic), plus a quarter (reference) and a half (output).
    fn push(tx: &mut ChainTx, index: u64) {
        let v = index as f32;
        tx.push(block(index), &[v; BLOCK], &[v + 0.25; BLOCK], &[v + 0.5; BLOCK]);
    }

    /// The fake audio thread's run: its stop, the blocks it has processed, and whether it holds a recorder.
    #[derive(Default)]
    struct Run {
        stop: AtomicBool,
        blocks: AtomicU64,
        holding: AtomicBool,
    }

    impl Run {
        /// Waits until the run has processed `n` blocks.
        fn wait_for_blocks(&self, n: u64) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.blocks.load(Acquire) < n {
                assert!(Instant::now() < deadline, "the run processed only {} blocks", self.blocks.load(Acquire));
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Waits until the run has handed its recorder back.
        fn wait_until_handed_back(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.holding.load(Acquire) {
                assert!(Instant::now() < deadline, "the run kept the recorder");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Stands in for the audio thread: a wake every millisecond takes the recorder through its hand-over (as `audio_loop`
    /// does before its blocks) and processes one block, pushing it while it holds the producer. Ends at `run.stop`, and
    /// drops the producer with the run, as a run that ends does.
    fn fake_run(mut end: End<ChainTx>, run: Arc<Run>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut slot, mut returning) = (None, None);
            let mut index = 0u64;
            while !run.stop.load(Acquire) {
                handover::service(&mut slot, &mut end, false, &mut returning);
                run.holding.store(slot.is_some() || returning.is_some(), Release);
                if let Some(tx) = slot.as_mut() {
                    push(tx, index);
                }
                index += 1;
                run.blocks.store(index, Release);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    }

    fn wait_for_recorded(h: &Handle, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.blocks() < n {
            assert!(Instant::now() < deadline, "only {} blocks", h.blocks());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_thread(h: &Handle, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn check_contents(rec: &ChainRecording) {
        assert_eq!(rec.mic.len(), rec.blocks.len() * BLOCK);
        for (k, b) in rec.blocks.iter().enumerate() {
            let v = b.index as f32;
            let at = k * BLOCK;
            assert!(rec.mic[at..at + BLOCK].iter().all(|&x| x == v), "block {k}");
            assert!(rec.reference[at..at + BLOCK].iter().all(|&x| x == v + 0.25));
            assert!(rec.output[at..at + BLOCK].iter().all(|&x| x == v + 0.5));
        }
        let consecutive = rec.blocks.windows(2).all(|w| w[1].index == w[0].index + 1);
        assert!(consecutive && rec.lost_blocks == 0, "in order, none lost");
    }

    /// Armed before the start, the run's first wake takes the recorder before its first block: the recording starts at
    /// block 0, and on stop the ring comes back to the control side.
    #[test]
    fn armed_before_the_start_it_records_from_the_first_block() {
        let inner = Inner::new();
        let h = arm(&inner, 60.0).unwrap();
        assert!(arm(&inner, 60.0).is_err(), "one at a time");
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        wait_for_recorded(&h, 30);
        let rec = h.stop();
        assert!(rec.from_first_block() && rec.reproducible() && !rec.run_ended && !rec.truncated, "{:?}", rec.blocks.first());
        check_contents(&rec);
        // The run handed the ring back rather than dropping it: the stop took it (or, had the run been slower than the
        // stop's wait, the next arm does), so a new recorder can be armed and the return ring is empty after it.
        run.wait_until_handed_back();
        let again = arm(&inner, 1.0).unwrap();
        assert!(lock(&inner.recorder).port.as_mut().is_some_and(|p| p.ret.pop().is_err()));
        drop(again);
        run.stop.store(true, Release);
        thread.join().unwrap();
        assert!(run.blocks.load(Acquire) >= 30);
    }

    /// While it records, a copy from any position gives the blocks recorded so far from there, as the recording holds them,
    /// and leaves the recording whole.
    #[test]
    fn a_copy_while_it_records_gives_the_blocks_so_far() {
        let inner = Inner::new();
        let h = arm(&inner, 60.0).unwrap();
        assert!(h.copy_from(0).blocks.is_empty(), "nothing yet");
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        wait_for_recorded(&h, 20);
        let all = h.copy_from(0);
        assert!(all.blocks.len() >= 20 && all.from_first_block() && !all.run_ended, "{}", all.blocks.len());
        check_contents(&all);
        // From a position: the rest, its samples with it; past the end, nothing.
        let rest = h.copy_from(12);
        assert_eq!(rest.blocks[0].index, 12);
        assert_eq!(rest.mic.len(), rest.blocks.len() * BLOCK);
        assert!(rest.mic[..BLOCK].iter().all(|&x| x == 12.0) && rest.reference[0] == 12.25 && rest.output[0] == 12.5);
        assert!(h.copy_from(1_000_000).blocks.is_empty());
        // Copies, built up as a reader would, agree with what the stop gives.
        let mut built = h.copy_from(0);
        wait_for_recorded(&h, built.blocks.len() as u64 + 10);
        let more = h.copy_from(built.blocks.len());
        assert!(!more.blocks.is_empty() && more.blocks[0].index == built.blocks.len() as u64);
        built.mic.extend_from_slice(&more.mic);
        built.blocks.extend_from_slice(&more.blocks);
        let rec = h.stop();
        check_contents(&rec);
        assert!(rec.blocks.len() >= built.blocks.len());
        assert_eq!(rec.blocks[..built.blocks.len()], built.blocks[..]);
        assert_eq!(rec.mic[..built.mic.len()], built.mic[..]);
        run.stop.store(true, Release);
        thread.join().unwrap();
    }

    /// Armed while a run is live, the run takes it at its next wake: the recording starts later than block 0, and isn't
    /// reproducible offline.
    #[test]
    fn armed_while_running_it_records_from_the_next_wake() {
        let inner = Inner::new();
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        run.wait_for_blocks(5);
        let h = arm(&inner, 60.0).unwrap();
        wait_for_recorded(&h, 20);
        let rec = h.stop();
        assert!(rec.blocks[0].index >= 5, "{}", rec.blocks[0].index);
        assert!(!rec.from_first_block() && !rec.reproducible() && !rec.run_ended);
        assert!(rec.blocks.len() >= 20);
        check_contents(&rec);
        run.stop.store(true, Release);
        thread.join().unwrap();
    }

    /// A start that fails before its loop hands the recorder back untaken, and the next start's run records from its first
    /// block; one stopped meanwhile is freed instead.
    #[test]
    fn a_failed_start_passes_the_recorder_to_the_next() {
        let inner = Inner::new();
        let h = arm(&inner, 60.0).unwrap();
        // The start fails: its run gives the producer back as it ends (`run_job`) and drops its end.
        let (port, mut end) = run_channel(&inner);
        assert!(lock(&inner.recorder).pending.is_none(), "handed to the start");
        handover::give_back(&mut end);
        drop(end);
        run_failed(&inner, port);
        assert!(lock(&inner.recorder).pending.is_some(), "kept for the next start");
        // The retry starts.
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        wait_for_recorded(&h, 10);
        let rec = h.stop();
        assert!(rec.reproducible() && !rec.run_ended);
        check_contents(&rec);
        run.stop.store(true, Release);
        thread.join().unwrap();
        run_stopped(&inner);

        // Stopped while its start was under way, it comes back abandoned and goes.
        let h = arm(&inner, 60.0).unwrap();
        let (port, mut end) = run_channel(&inner);
        let rec = h.stop();
        assert!(rec.blocks.is_empty());
        handover::give_back(&mut end);
        drop(end);
        run_failed(&inner, port);
        assert!(lock(&inner.recorder).pending.is_none());
        assert!(arm(&inner, 1.0).is_ok(), "the claim was released");
    }

    /// The run ending (a stop or a restart) ends the recording with it; a recorder whose start never came records nothing;
    /// and the length limit stops it.
    #[test]
    fn the_recording_ends_with_the_run_or_at_its_limit() {
        let inner = Inner::new();
        let h = arm(&inner, 60.0).unwrap();
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        wait_for_recorded(&h, 10);
        run.stop.store(true, Release);
        thread.join().unwrap();
        run_stopped(&inner);
        wait_for_thread(&h, "the thread saw the run end");
        let rec = h.stop();
        assert!(rec.run_ended && rec.from_first_block());
        check_contents(&rec);

        // Armed, then stopped with no start: nothing, and the producer is taken back from `pending`.
        let h = arm(&inner, 60.0).unwrap();
        assert!(lock(&inner.recorder).pending.is_some());
        let rec = h.stop();
        assert!(rec.blocks.is_empty() && !rec.reproducible());
        assert!(lock(&inner.recorder).pending.is_none());

        // 0.1 s: ten blocks.
        let h = arm(&inner, 0.1).unwrap();
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone());
        run_started(&inner, port);
        wait_for_thread(&h, "stopped at the limit");
        let rec = h.stop();
        assert!(rec.truncated && rec.blocks.len() == 10, "{}", rec.blocks.len());
        run.stop.store(true, Release);
        thread.join().unwrap();

        for bad in [0.0, -1.0, f64::NAN, MAX_CHAIN_RECORDING_S + 1.0] {
            assert!(arm(&inner, bad).is_err(), "{bad}");
        }
        assert!(arm(&inner, MAX_CHAIN_RECORDING_S).is_ok(), "the claim was released each time");
    }

    /// A full ring loses whole blocks, counted by the audio thread: those before a block the recording got by that block,
    /// and those after the last (the drain thread stalled to the end) by the count it reads as it stops.
    #[test]
    fn a_full_ring_loses_blocks_and_counts_them() {
        let (ring, rx) = RingBuffer::<ChainItem>::new(3);
        let mut tx = ChainTx::new(ring);
        let lost = tx.lost();
        let state = DrainState::default();
        let mut rx = Some(rx);
        let mut rec = ChainRecording::default();
        for i in 0..10 {
            push(&mut tx, i);
            if i == 5 {
                while let Ok(it) = rx.as_mut().unwrap().pop() {
                    add(&mut rec, &it);
                }
            }
        }
        // Blocks 3-5 were lost before block 6, and 9 after the last: the stop's drain takes 6-8 and counts all four.
        state.stop.store(true, Release);
        drain(rx.take().unwrap(), &lost, &state, 100);
        let rest = std::mem::take(&mut *lock(&state.rec));
        assert_eq!(rest.lost_blocks, 4);
        let first: Vec<u64> = rec.blocks.iter().map(|b| b.index).collect();
        assert_eq!((first.as_slice(), rec.lost_blocks), ([0, 1, 2].as_slice(), 0));
        let indices: Vec<u64> = rest.blocks.iter().map(|b| b.index).collect();
        assert_eq!(indices, [6, 7, 8]);
        assert_eq!(rest.mic[0], 6.0);
        // Cut at its limit, a recording counts only the losses before its last block.
        let (ring, rx) = RingBuffer::<ChainItem>::new(3);
        let mut tx = ChainTx::new(ring);
        for i in 0..5 {
            push(&mut tx, i);
        }
        let state = DrainState::default();
        drain(rx, &tx.lost(), &state, 2);
        let cut = std::mem::take(&mut *lock(&state.rec));
        assert!(cut.truncated && cut.blocks.len() == 2 && cut.lost_blocks == 0, "{cut:?}");
        assert_eq!(tx.lost().load(Acquire), 2, "3 and 4, after the limit");
    }
}
