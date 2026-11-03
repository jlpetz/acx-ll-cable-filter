//! The flight recorder (engine.md, "The flight recorder"; TODO 13, 21): the last seconds of every block's facts (and, if
//! asked, its audio) kept in memory on the control side, and written out when something goes wrong, so a soak or a user's
//! run shows what ran dry.
//!
//! - **The audio thread** does what it does for the chain recorder (recorder.rs): one `ChainItem` a block, the block's facts
//!   and its mic, reference and output, copied into a pre-allocated rtrb ring (`ChainTx`; a full ring loses the block,
//!   counted). It only flags: the facts it already writes say what happened (an underrun, zeros, a stall, a glitch, the
//!   wake's gap, the block's time). Each run gets a ring of its own, made and touched on the control side as the run starts
//!   (`run_channel`) and handed over through the run's own hand-over rings (handover.rs), so the recorder follows the
//!   engine across its restarts; the ring goes back to the control side to be freed, never on the audio thread.
//! - **The recorder's thread** (normal priority) drains the rings every 20 ms into [`Core`]: a bounded ring of the last
//!   `facts_s` + `post_roll_s` seconds of facts (and `audio_s` + `post_roll_s` of audio), pre-allocated, never grown. It
//!   detects the triggers from the facts ([`Watch`]) and from the control side's notes (a fault), waits out the post-roll
//!   (by blocks, or by time when no blocks come), cuts the window, and writes it: `flight_blocks.csv`, `summary.txt` (what
//!   triggered, when, and a reading of the facts around it in words: [`read`]), and the WAVs when audio is kept.
//! - **A dump for each kind:** a trigger goes into a pending dump that holds its kind, or whose trigger came half a second or
//!   less before it (the most serious of those leads it: the output, then the mic, the thread, a fault, the reference, the
//!   model); else it opens a dump of its own, which may overlap another kind's, each with its whole pre- and post-roll. A
//!   dump is cut as its post-roll's last block comes in, whatever the drain's batches.
//! - **Rate-limited:** at most one dump a `min_interval_s` (60 s) and `max_dumps_per_kind` (3) an arming for each kind, so
//!   a misbehaving loopback can't use up the dumps an output underrun needs, then or later; at most `max_dumps` (20) in all;
//!   the rest are counted, by kind. On disk: the newest `keep_dumps` (20) dump folders are kept in the folder.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nf_core::{BLOCK, Block, SAMPLE_RATE};
use rtrb::Consumer;

use crate::api::{ChainBlock, EngineError, FlightConfig, FlightDump, FlightStats, ReferenceFacts};
use crate::control::{Inner, lock};
use crate::handover::{self, End, Port};
use crate::recorder::{self, ChainItem, ChainTx};
use crate::timing::Qpc;

const DRAIN_EVERY: Duration = Duration::from_millis(20);
/// How long a stop waits for the audio thread to hand the live run's ring back (it looks once a wake).
const RETURN_WAIT: Duration = Duration::from_millis(500);
const BLOCKS_PER_S: f64 = 100.0;
const HNS_PER_S: f64 = 1e7;
const HNS_PER_MS: f64 = 1e4;
/// A gap between the thread's wakes this long (ms), with no wait timed out, is a late wake: the thread held up, or the
/// devices' events late. Its waits time out at 100 ms (a stall, the mic's); an exclusive output's events wake it every few ms.
const LATE_WAKE_MS: f32 = 40.0;
/// A block whose own processing took this long (µs: half a block) is slow.
const SLOW_BLOCK_US: f32 = 5_000.0;
/// The thread's triggers (a late wake, a slow block) count from this block of a run on: its first second is the streams
/// starting (as `devices --check` leaves it out).
const THREAD_GRACE: u64 = 100;
/// The reference's start-up, in blocks from its alignment: its first 250 readings (reference.rs), and 10 more, as `devices
/// --check` counts it. Its jumps and short takes there are by design.
const REFERENCE_SETTLE: u64 = 260;
/// A burst of short takes: this many of the last [`SHORT_WINDOW`] blocks' reference takes owed samples (5% of a second). The
/// next burst counts once a whole window has had none.
const SHORT_BURST: u32 = 5;
const SHORT_WINDOW: u32 = 100;
/// Triggers folded into a pending dump that it lists one by one, at most (the rest are counted).
const MAX_ALSO: usize = 24;
/// A trigger this soon after a pending dump's (blocks, and seconds by the clock) goes into it, whatever its kind: the same
/// incident (a stall's first block has its underrun, its skip and its glitch; a held thread's, its underrun and late wake).
const NEAR_BLOCKS: u64 = 50;
const NEAR_S: f64 = 0.5;
/// How long past its post-roll a dump waits by the clock when its blocks don't come (the audio stopped), for the drain's own
/// delay.
const DUE_SLACK_S: f64 = 0.5;
/// Control-side notes kept, at most.
const MAX_NOTES: usize = 64;

/// What makes a dump. The order is the headline's, when one block has several.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    OutputUnderrun,
    OutputDeficit,
    OutputDrop,
    MicStall,
    MicOverflow,
    MicSkip,
    MicGlitch,
    LateWake,
    SlowBlock,
    ReferenceLost,
    ReferenceJump,
    ReferenceShort,
    ModelMiss,
    /// A control-side note: the audio stopped by itself.
    Fault,
}

/// The triggers a block's facts can show, in the headline's order.
#[rustfmt::skip]
const BLOCK_TRIGGERS: [Trigger; 13] = [
    Trigger::OutputUnderrun, Trigger::OutputDeficit, Trigger::OutputDrop, Trigger::MicStall, Trigger::MicOverflow, Trigger::MicSkip,
    Trigger::MicGlitch, Trigger::LateWake, Trigger::SlowBlock, Trigger::ReferenceLost, Trigger::ReferenceJump,
    Trigger::ReferenceShort, Trigger::ModelMiss,
];

/// What the rate limit counts by: one dump a `min_interval_s`, and `max_dumps_per_kind` an arming, for each. In order of how
/// serious it is (a dump's headline is its most serious trigger's): what the user hears first, then what costs the echo
/// canceller or the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Output,
    Mic,
    Thread,
    Fault,
    Reference,
    Model,
}

const KINDS: usize = 6;

impl Kind {
    const ALL: [Kind; KINDS] = [Kind::Output, Kind::Mic, Kind::Thread, Kind::Fault, Kind::Reference, Kind::Model];

    fn words(self) -> &'static str {
        match self {
            Kind::Output => "the output",
            Kind::Mic => "the mic",
            Kind::Thread => "the audio thread",
            Kind::Fault => "faults",
            Kind::Reference => "the echo reference",
            Kind::Model => "the model",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl Trigger {
    fn bit(self) -> u16 {
        1 << self as u16
    }

    pub(crate) fn kind(self) -> Kind {
        match self {
            Trigger::OutputUnderrun | Trigger::OutputDeficit | Trigger::OutputDrop => Kind::Output,
            Trigger::MicStall | Trigger::MicOverflow | Trigger::MicSkip | Trigger::MicGlitch => Kind::Mic,
            Trigger::LateWake | Trigger::SlowBlock => Kind::Thread,
            Trigger::ReferenceLost | Trigger::ReferenceJump | Trigger::ReferenceShort => Kind::Reference,
            Trigger::ModelMiss => Kind::Model,
            Trigger::Fault => Kind::Fault,
        }
    }

    /// Its name in the CSV's `trigger` column and the dump's folder.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Trigger::OutputUnderrun => "output-underrun",
            Trigger::OutputDeficit => "output-deficit",
            Trigger::OutputDrop => "output-drop",
            Trigger::MicStall => "mic-stall",
            Trigger::MicOverflow => "mic-overflow",
            Trigger::MicSkip => "mic-skip",
            Trigger::MicGlitch => "mic-glitch",
            Trigger::LateWake => "late-wake",
            Trigger::SlowBlock => "slow-block",
            Trigger::ReferenceLost => "reference-lost",
            Trigger::ReferenceJump => "reference-jump",
            Trigger::ReferenceShort => "reference-short",
            Trigger::ModelMiss => "model-miss",
            Trigger::Fault => "fault",
        }
    }

    pub(crate) fn words(self) -> &'static str {
        match self {
            Trigger::OutputUnderrun => "an output underrun",
            Trigger::OutputDeficit => "an output deficit (zeros written to top the device up)",
            Trigger::OutputDrop => "an output coarse drop (samples dropped to catch up)",
            Trigger::MicStall => "a mic stall (a wait timed out without the mic's event)",
            Trigger::MicOverflow => "the mic's FIFO overflowing (the filter didn't run for 200 ms)",
            Trigger::MicSkip => "a mic position skip (a gap of more than 100 ms, not filled)",
            Trigger::MicGlitch => "a mic glitch (samples missing by the device's position, or flagged discontinuous)",
            Trigger::LateWake => "a late wake of the audio thread",
            Trigger::SlowBlock => "a block that took the audio thread too long",
            Trigger::ReferenceLost => "the echo reference lost",
            Trigger::ReferenceJump => "a jump of the echo reference's timeline",
            Trigger::ReferenceShort => "a burst of short echo reference takes",
            Trigger::ModelMiss => "a model miss (the model pack's result came too late)",
            Trigger::Fault => "a fault that stopped the audio",
        }
    }

    /// The names of the triggers in `bits`, `|`-joined (the CSV's).
    fn names(bits: u16) -> String {
        BLOCK_TRIGGERS.iter().filter(|t| bits & t.bit() != 0).map(|t| t.name()).collect::<Vec<_>>().join("|")
    }

    /// Every trigger, a block's and the control side's.
    fn all() -> impl Iterator<Item = Trigger> {
        BLOCK_TRIGGERS.into_iter().chain([Trigger::Fault])
    }
}

/// What the control side tells the recorder (`note`): a fault is a trigger; the rest go in the summary.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Note {
    /// The audio stopped by itself (a fault, or the audio thread died), worded.
    Fault(String),
    /// A start or a restart failed, worded.
    StartFailed(String),
    /// The streams restart, and why.
    Restarting(&'static str),
    /// The run carries on without its echo reference, worded.
    ReferenceLost(String),
    /// The exclusive period guard stepped a stream up (control.rs), worded.
    PeriodRaised(String),
    /// Stopped by the user (`Engine::stop`).
    Stopped,
}

impl Note {
    fn text(&self) -> String {
        match self {
            Note::Fault(m) => format!("the audio stopped: {m}"),
            Note::StartFailed(m) => format!("a start failed: {m}"),
            Note::Restarting(why) => format!("the streams restart ({why})"),
            Note::ReferenceLost(m) => format!("the echo reference was lost: {m}"),
            Note::PeriodRaised(m) => format!("the exclusive period guard: {m}"),
            Note::Stopped => "the filter was stopped".to_owned(),
        }
    }
}

/// A note, with when it came (QPC, 100 ns units).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Noted {
    pub(crate) at_hns: f64,
    pub(crate) note: Note,
}

/// One block as the recorder keeps it: its sequence number since the recorder was armed (from 1), the run it came from (from
/// 1, a new one at each start), the blocks its ring had lost before it in that run, its facts, and the triggers it showed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entry {
    pub(crate) seq: u64,
    pub(crate) run: u32,
    pub(crate) lost: u64,
    pub(crate) block: ChainBlock,
    pub(crate) hits: u16,
}

/// One block's audio, kept beside its facts when asked.
#[derive(Clone, Copy)]
struct Sound {
    seq: u64,
    mic: Block,
    reference: Block,
    output: Block,
}

/// One trigger, where and when it came.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hit {
    pub(crate) trigger: Trigger,
    /// The block's sequence number; for a note, the newest block's when it came.
    pub(crate) seq: u64,
    pub(crate) run: u32,
    /// The run's block index; `None` for a note.
    pub(crate) index: Option<u64>,
    /// When (QPC, 100 ns units): the block's wake, or the note's time.
    pub(crate) at_hns: f64,
    /// A fault's words.
    pub(crate) words: Option<String>,
}

impl Hit {
    fn words(&self) -> String {
        match &self.words {
            Some(m) => format!("{} ({m})", self.trigger.words()),
            None => self.trigger.words().to_owned(),
        }
    }
}

/// The recorder's sizes, in blocks and QPC units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Limits {
    pub(crate) pre: u64,
    pub(crate) post: u64,
    pub(crate) audio: u64,
    pub(crate) post_hns: f64,
    pub(crate) min_interval_hns: f64,
    pub(crate) max_per_kind: u32,
    pub(crate) max_dumps: u32,
}

impl Limits {
    pub(crate) fn of(c: &FlightConfig) -> Self {
        let blocks = |s: f64| (s * BLOCKS_PER_S).round().max(0.0) as u64;
        Self {
            pre: blocks(c.facts_s),
            post: blocks(c.post_roll_s).max(1),
            audio: blocks(c.audio_s),
            post_hns: c.post_roll_s * HNS_PER_S,
            min_interval_hns: c.min_interval_s * HNS_PER_S,
            max_per_kind: c.max_dumps_per_kind,
            max_dumps: c.max_dumps,
        }
    }
}

/// A trigger waiting out its post-roll (its headline: the most serious of those that came together), and the ones that came
/// into it meanwhile.
struct Pending {
    first: Hit,
    also: Vec<Hit>,
    more: u64,
    /// The kinds of its triggers (bits of [`Kind::bit`]): a later trigger of one of them goes into it.
    kinds: u8,
    /// The kinds it was allowed a dump for, and when (QPC): its first trigger's, and one that took the headline over. Its cut
    /// counts the dump against each (`min_interval_s`, `max_dumps_per_kind`).
    claims: [Option<f64>; KINDS],
    due_seq: u64,
    due_hns: f64,
}

impl Pending {
    /// Whether `h` came half a second or less after its headline: the same incident, whatever its kind.
    fn near(&self, h: &Hit) -> bool {
        h.seq <= self.first.seq + NEAR_BLOCKS && h.at_hns - self.first.at_hns <= NEAR_S * HNS_PER_S
    }

    fn push_also(&mut self, h: Hit) {
        if self.also.len() < MAX_ALSO {
            self.also.push(h);
        } else {
            self.more += 1;
        }
    }
}

/// The audio a dump keeps: 48 kHz mono, a block of each per entry from `from_seq` on.
pub(crate) struct Audio {
    pub(crate) from_seq: Option<u64>,
    pub(crate) mic: Vec<f32>,
    pub(crate) reference: Vec<f32>,
    pub(crate) output: Vec<f32>,
}

/// What a dump writes: the trigger and those that came in its post-roll, the blocks around it, the audio if kept, the
/// control side's notes, and the triggers the rate limit held back since the last dump, by kind.
pub(crate) struct Window {
    pub(crate) first: Hit,
    pub(crate) also: Vec<Hit>,
    pub(crate) more: u64,
    pub(crate) entries: Vec<Entry>,
    pub(crate) audio: Option<Audio>,
    pub(crate) notes: Vec<Noted>,
    pub(crate) suppressed: [u64; KINDS],
    /// Blocks the post-roll was short of: the recorder stopped, or no blocks came (the audio stopped) for longer than it.
    pub(crate) post_short: u64,
    pub(crate) armed_hns: f64,
    pub(crate) limits: Limits,
}

/// Per run, what the triggers need from the blocks before: the last block, the reference's alignment, and its recent short
/// takes (a bit a block).
#[derive(Default)]
struct Watch {
    run: u32,
    last: Option<ChainBlock>,
    aligned_at: Option<u64>,
    shorts: u128,
    burst: bool,
}

impl Watch {
    /// The triggers block `b` of `run` shows (bits of [`Trigger::bit`]). The output's are never at a run's first block,
    /// which sets the starting fill (its facts count none there); the thread's count after the run's first second. The
    /// reference's jumps and short takes count only after its start-up; it's lost when a block has none after one that had it.
    fn check(&mut self, run: u32, b: &ChainBlock) -> u16 {
        if run != self.run {
            *self = Watch { run, ..Watch::default() };
        }
        let mut hits = 0u16;
        let mut hit = |t: Trigger| hits |= t.bit();
        let (o, t) = (&b.output, &b.thread);
        if o.underrun {
            hit(Trigger::OutputUnderrun);
        }
        if o.zeros > 0 {
            hit(Trigger::OutputDeficit);
        }
        if o.dropped > 0 || o.skipped {
            hit(Trigger::OutputDrop);
        }
        if t.stalls > 0 {
            hit(Trigger::MicStall);
        }
        if b.mic_discarded > 0 {
            hit(Trigger::MicOverflow);
        }
        if b.mic_skipped != 0 {
            hit(Trigger::MicSkip);
        }
        if t.mic_glitches > 0 {
            hit(Trigger::MicGlitch);
        }
        if b.index >= THREAD_GRACE && t.wake_gap_ms >= LATE_WAKE_MS && t.stalls == 0 {
            hit(Trigger::LateWake);
        }
        if b.index >= THREAD_GRACE && t.process_us >= SLOW_BLOCK_US {
            hit(Trigger::SlowBlock);
        }
        match &b.reference {
            None => {
                if self.last.is_some_and(|l| l.reference.is_some()) {
                    hit(Trigger::ReferenceLost);
                }
            }
            Some(r) => {
                if r.events & ReferenceFacts::ALIGNED != 0 {
                    self.aligned_at = Some(b.index);
                }
                // Armed after the alignment: the run's start stands in for it.
                let settled = b.index >= self.aligned_at.unwrap_or(0) + REFERENCE_SETTLE;
                let jumps = ReferenceFacts::BROKEN | ReferenceFacts::STEPPED | ReferenceFacts::RETARGETED | ReferenceFacts::CENTRED;
                if settled && (r.events & jumps != 0 || r.jumped != 0) {
                    hit(Trigger::ReferenceJump);
                }
                self.shorts = (self.shorts << 1) | u128::from(settled && r.owed > 0);
                let recent = (self.shorts & ((1u128 << SHORT_WINDOW) - 1)).count_ones();
                if recent >= SHORT_BURST && !self.burst {
                    self.burst = true;
                    hit(Trigger::ReferenceShort);
                } else if recent == 0 {
                    self.burst = false;
                }
            }
        }
        if t.model_misses > 0 {
            hit(Trigger::ModelMiss);
        }
        self.last = Some(*b);
        hits
    }
}

/// The recorder's logic, apart from its thread and files (pure: tests feed it): the bounded rings of facts and audio, the
/// triggers, the post-rolls and the rate limit.
pub(crate) struct Core {
    limits: Limits,
    armed_hns: f64,
    facts: VecDeque<Entry>,
    facts_cap: usize,
    audio: VecDeque<Sound>,
    audio_cap: usize,
    /// The newest block's sequence number (0 before the first).
    seq: u64,
    watch: Watch,
    /// The dumps waiting out their post-rolls: one at most holding each kind, so at most [`KINDS`].
    pending: Vec<Pending>,
    /// Windows cut as their post-roll's last block came in, for [`Core::poll`] (at most one a pending dump).
    ready: VecDeque<Window>,
    /// Per kind: when its last dump's trigger came, and its dumps so far.
    last_dump_hns: [Option<f64>; KINDS],
    kind_dumps: [u32; KINDS],
    dumps: u32,
    /// Per kind, the triggers held back since the last dump (for its summary) and in all.
    suppressed_since: [u64; KINDS],
    pub(crate) suppressed_by: [u64; KINDS],
    pub(crate) suppressed: u64,
    notes: VecDeque<Noted>,
    pub(crate) blocks: u64,
}

impl Core {
    /// Allocates the rings: the facts for the pre-roll and the post-roll (and the trigger's own block), and the audio's the
    /// same way when it's kept. They never grow: a dump is cut as its post-roll's last block comes in, when the ring holds
    /// its window exactly, its headline's pre-roll included.
    pub(crate) fn new(limits: Limits, armed_hns: f64) -> Self {
        let facts_cap = (limits.pre + limits.post + 1) as usize;
        let audio_cap = if limits.audio > 0 { (limits.audio + limits.post + 1) as usize } else { 0 };
        Self {
            limits,
            armed_hns,
            facts: VecDeque::with_capacity(facts_cap),
            facts_cap,
            audio: VecDeque::with_capacity(audio_cap),
            audio_cap,
            seq: 0,
            watch: Watch::default(),
            pending: Vec::with_capacity(KINDS),
            ready: VecDeque::with_capacity(KINDS),
            last_dump_hns: [None; KINDS],
            kind_dumps: [0; KINDS],
            dumps: 0,
            suppressed_since: [0; KINDS],
            suppressed_by: [0; KINDS],
            suppressed: 0,
            notes: VecDeque::with_capacity(MAX_NOTES),
            blocks: 0,
        }
    }

    /// The rings' allocated sizes (they must not grow).
    #[cfg(test)]
    fn capacities(&self) -> (usize, usize, usize, usize, usize) {
        (self.facts.capacity(), self.audio.capacity(), self.notes.capacity(), self.pending.capacity(), self.ready.capacity())
    }

    /// One block from run `run`'s ring; `now_hns` (QPC) times its triggers when its facts have no wake time. A pending dump
    /// whose post-roll this block ends is cut now, before the next block pushes its window's first out of the ring.
    pub(crate) fn feed(&mut self, run: u32, item: &ChainItem, now_hns: f64) {
        self.seq += 1;
        self.blocks += 1;
        let b = &item.block;
        let hits = self.watch.check(run, b);
        if self.facts.len() >= self.facts_cap {
            self.facts.pop_front();
        }
        self.facts.push_back(Entry { seq: self.seq, run, lost: item.lost, block: *b, hits });
        if self.audio_cap > 0 {
            if self.audio.len() >= self.audio_cap {
                self.audio.pop_front();
            }
            self.audio.push_back(Sound { seq: self.seq, mic: item.mic, reference: item.reference, output: item.output });
        }
        if hits != 0 {
            let at_hns = if b.thread.wake_hns > 0.0 { b.thread.wake_hns } else { now_hns };
            for t in BLOCK_TRIGGERS.iter().filter(|t| hits & t.bit() != 0) {
                self.hit(Hit { trigger: *t, seq: self.seq, run, index: Some(b.index), at_hns, words: None });
            }
        }
        while let Some(i) = self.pending.iter().position(|p| self.seq >= p.due_seq) {
            let w = self.cut(i);
            self.ready.push_back(w);
        }
    }

    /// A control-side note: kept for the summary, and a fault is a trigger (at the newest block).
    pub(crate) fn note(&mut self, n: Noted) {
        if let Note::Fault(m) = &n.note {
            let h =
                Hit { trigger: Trigger::Fault, seq: self.seq, run: self.watch.run, index: None, at_hns: n.at_hns, words: Some(m.clone()) };
            self.hit(h);
        }
        if self.notes.len() >= MAX_NOTES {
            self.notes.pop_front();
        }
        self.notes.push_back(n);
    }

    /// Whether kind `k` may have a dump for a trigger at `at_hns`: its last was `min_interval_s` ago or more, and it has had
    /// fewer than `max_dumps_per_kind`.
    fn kind_may(&self, k: Kind, at_hns: f64) -> bool {
        let recent = self.last_dump_hns[k as usize].is_some_and(|t| at_hns - t < self.limits.min_interval_hns);
        !recent && self.kind_dumps[k as usize] < self.limits.max_per_kind
    }

    /// A trigger: into the pending dump that holds its kind, or whose headline came half a second or less before it (where,
    /// if it's more serious and its kind may have a dump, it takes the headline over, the window re-centred on it); else a
    /// dump of its own, unless its kind's limits or the dumps in all say no (then counted).
    fn hit(&mut self, h: Hit) {
        let k = h.trigger.kind();
        let into = self.pending.iter().position(|p| p.kinds & k.bit() != 0).or_else(|| self.pending.iter().position(|p| p.near(&h)));
        if let Some(i) = into {
            let lead = k < self.pending[i].first.trigger.kind() && self.pending[i].near(&h) && self.kind_may(k, h.at_hns);
            let (post, post_hns) = (self.limits.post, self.limits.post_hns);
            let p = &mut self.pending[i];
            p.kinds |= k.bit();
            if lead {
                p.claims[k as usize] = Some(h.at_hns);
                (p.due_seq, p.due_hns) = (h.seq + post, h.at_hns + post_hns);
                let was = std::mem::replace(&mut p.first, h);
                if p.also.len() >= MAX_ALSO {
                    p.also.pop();
                    p.more += 1;
                }
                p.also.insert(0, was);
            } else {
                p.push_also(h);
            }
            return;
        }
        if !self.kind_may(k, h.at_hns) || self.dumps + self.pending.len() as u32 >= self.limits.max_dumps {
            self.suppressed_since[k as usize] += 1;
            self.suppressed_by[k as usize] += 1;
            self.suppressed += 1;
            return;
        }
        let mut claims = [None; KINDS];
        claims[k as usize] = Some(h.at_hns);
        let (due_seq, due_hns) = (h.seq + self.limits.post, h.at_hns + self.limits.post_hns);
        self.pending.push(Pending { first: h, also: Vec::new(), more: 0, kinds: k.bit(), claims, due_seq, due_hns });
    }

    /// The next window that's due: one cut as its post-roll's last block came in, else a pending dump whose blocks haven't
    /// come (the audio stopped) once its post-roll's time has passed, half a second's slack for the drain's own delay
    /// included. `now_hns` is QPC. Call it until it's `None`.
    pub(crate) fn poll(&mut self, now_hns: f64) -> Option<Window> {
        if let Some(w) = self.ready.pop_front() {
            return Some(w);
        }
        let i = self.pending.iter().position(|p| now_hns >= p.due_hns + DUE_SLACK_S * HNS_PER_S)?;
        Some(self.cut(i))
    }

    /// The windows still to come, now, their post-rolls cut short (the recorder stops). Call it until it's `None`.
    pub(crate) fn flush(&mut self) -> Option<Window> {
        if let Some(w) = self.ready.pop_front() {
            return Some(w);
        }
        (!self.pending.is_empty()).then(|| self.cut(0))
    }

    /// Pending dump `i`'s window: its headline's pre-roll to its post-roll's end, from the rings as they are.
    fn cut(&mut self, i: usize) -> Window {
        let p = self.pending.remove(i);
        let (from, to) = (p.first.seq.saturating_sub(self.limits.pre), p.due_seq);
        let entries: Vec<Entry> = self.facts.iter().filter(|e| e.seq >= from && e.seq <= to).copied().collect();
        let audio = (self.audio_cap > 0).then(|| {
            let from = p.first.seq.saturating_sub(self.limits.audio);
            let kept: Vec<&Sound> = self.audio.iter().filter(|a| a.seq >= from && a.seq <= to).collect();
            let join = |f: fn(&Sound) -> &Block| kept.iter().flat_map(|a| f(a).iter().copied()).collect::<Vec<f32>>();
            Audio {
                from_seq: kept.first().map(|a| a.seq),
                mic: join(|a| &a.mic),
                reference: join(|a| &a.reference),
                output: join(|a| &a.output),
            }
        });
        let since = p.first.at_hns - (self.limits.pre as f64 / BLOCKS_PER_S + 1.0) * HNS_PER_S;
        let notes = self.notes.iter().filter(|n| n.at_hns >= since).cloned().collect();
        for (k, at) in p.claims.iter().enumerate() {
            if let Some(at) = at {
                self.last_dump_hns[k] = Some(*at);
                self.kind_dumps[k] += 1;
            }
        }
        self.dumps += 1;
        Window {
            post_short: p.due_seq.saturating_sub(self.seq),
            first: p.first,
            also: p.also,
            more: p.more,
            entries,
            audio,
            notes,
            suppressed: std::mem::take(&mut self.suppressed_since),
            armed_hns: self.armed_hns,
            limits: self.limits,
        }
    }
}

/// The summary's reading of a window: one sentence for what it looks like, and the facts around the trigger in words.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Reading {
    pub(crate) verdict: String,
    pub(crate) lines: Vec<String>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied()
}

/// The gaps between the mic's delivering wakes over consecutive `entries` of one run (ms), each with the block after it.
fn mic_gaps(entries: &[Entry]) -> Vec<(f64, &Entry)> {
    entries
        .windows(2)
        .filter(|w| w[0].run == w[1].run)
        .filter_map(|w| {
            let d = (w[1].block.mic_wake_hns - w[0].block.mic_wake_hns) / HNS_PER_MS;
            (d.is_finite() && d > 0.0).then_some((d, &w[1]))
        })
        .collect()
}

/// "1 wait timed out", "2 waits timed out".
fn waits(n: u32) -> String {
    format!("{n} {} timed out", if n == 1 { "wait" } else { "waits" })
}

fn ms(samples: f64) -> f64 {
    samples * 1000.0 / f64::from(SAMPLE_RATE)
}

/// Reads the facts around the trigger: a heuristic, in this order. A fault is said as it was worded. A gap between the
/// thread's wakes of three times its usual (and 15 ms more), no wait timing out, is the thread held up when the mic's samples
/// were waiting as it woke (a backlog, or a glitch: an exclusive mic's one-period buffer overran), else the devices' events
/// late; with a shared output, though, the thread wakes on the mic's events alone, and then a mic's driver that held its
/// samples back and delivered them at once looks the same, so the reading says it can't tell which. Waits timing out, or a gap between the mic's deliveries of three times its usual (and 20 ms more), is the mic
/// stopping. A block that took half a block's time is the processing falling behind. Otherwise, with the thread and the mic
/// on time, it's the device the trigger names: the output running dry or overfilling, the loopback delivering late or
/// breaking, the mic losing samples, the model's worker late.
pub(crate) fn read(w: &Window) -> Reading {
    let Some(at) = w.entries.iter().rposition(|e| e.seq <= w.first.seq) else {
        let verdict = match &w.first.words {
            Some(m) => format!("The audio stopped: {m} No block reached the recorder around it."),
            None => "No block reached the recorder around it.".to_owned(),
        };
        return Reading { verdict, lines: Vec::new() };
    };
    let t = &w.entries[at];
    // The trigger's run, up to it.
    let from = w.entries[..=at].iter().rposition(|e| e.run != t.run).map_or(0, |i| i + 1);
    let run = &w.entries[from..=at];
    let second = &run[run.len().saturating_sub(100)..];
    let near = &run[run.len().saturating_sub(21)..];
    let calm = if run.len() >= 150 { &run[..run.len() - 100] } else { run };
    let gap_usual = median(calm.iter().filter(|e| e.block.thread.wakes > 0).map(|e| f64::from(e.block.thread.wake_gap_ms)).collect());
    let mic_usual = median(mic_gaps(calm).iter().map(|g| g.0).collect());
    let proc_usual = median(calm.iter().map(|e| f64::from(e.block.thread.process_us)).collect());
    let gap_max = near.iter().max_by(|a, b| a.block.thread.wake_gap_ms.total_cmp(&b.block.thread.wake_gap_ms));
    let stalls: u32 = near.iter().map(|e| e.block.thread.stalls).sum();
    let mic_max = mic_gaps(near).into_iter().max_by(|a, b| a.0.total_cmp(&b.0));
    let slowest = near.iter().max_by(|a, b| a.block.thread.process_us.total_cmp(&b.block.thread.process_us));
    let glitches: u32 = w.entries.iter().map(|e| e.block.thread.mic_glitches).sum();
    // With a shared output the thread waits on the mic's events alone: then a long gap with the mic's samples waiting is a
    // thread held up or a driver that held them back, and nothing tells which. An exclusive output's events wake it every
    // few ms whatever the mic does.
    let mic_alone = calm.iter().all(|e| e.block.thread.out_wakes == 0);
    let out_wakes: u32 = near.iter().map(|e| e.block.thread.out_wakes).sum();

    let mut lines = Vec::new();
    let o = &t.block.output;
    let mut out = format!(
        "The output at block {}: the device's padding {} samples ({:.1} ms) and {} pending when it was written; the fill {:.1} ms \
         against the target {:.1} ms",
        t.block.index,
        o.padding,
        ms(f64::from(o.padding)),
        o.pending,
        ms(f64::from(o.fill)),
        ms(f64::from(o.target))
    );
    for (n, what) in
        [(o.zeros, "zeros written"), (o.dropped, "pending samples dropped"), (o.silence, "samples of silence the device played")]
    {
        if n > 0 {
            let _ = write!(out, "; {n} {what}");
        }
    }
    if o.underrun {
        out.push_str("; an underrun");
    }
    out.push('.');
    lines.push(out);
    let usual = |v: Option<f64>, unit: &str| v.map_or("--".to_owned(), |x| format!("{x:.1} {unit}"));
    let mut thread = format!("The audio thread: its usual gap between wakes {}", usual(gap_usual, "ms"));
    if let Some(g) = gap_max {
        let _ = write!(
            thread,
            ", the longest in the 200 ms up to the trigger {:.1} ms (before block {})",
            g.block.thread.wake_gap_ms, g.block.index
        );
    }
    let _ = write!(thread, "; {} there", waits(stalls));
    if mic_alone {
        thread.push_str("; it waits on the mic's events alone (the output is shared)");
    } else {
        let _ = write!(thread, "; the exclusive output's events woke it {out_wakes} times there");
    }
    if let Some(s) = slowest {
        let _ = write!(
            thread,
            "; the slowest block took {:.0} us (block {}), the usual {}",
            s.block.thread.process_us,
            s.block.index,
            usual(proc_usual, "us")
        );
    }
    thread.push('.');
    lines.push(thread);
    let mut mic = format!("The mic: its usual gap between deliveries {}", usual(mic_usual, "ms"));
    if let Some((d, e)) = mic_max {
        let _ = write!(mic, ", the longest in the 200 ms up to the trigger {d:.1} ms (before block {})", e.block.index);
    }
    let _ = write!(mic, "; {} samples queued behind block {}", t.block.mic_queued, t.block.index);
    if let Some(b) = &t.block.budget {
        let _ = write!(mic, ", its first sample {:.1} ms old when the thread read it", b.mic_ms);
    }
    let _ = write!(mic, "; {glitches} {} in the window.", if glitches == 1 { "glitch" } else { "glitches" });
    lines.push(mic);
    if let Some(r) = &t.block.reference {
        let short = second.iter().filter(|e| e.block.reference.is_some_and(|r| r.owed > 0)).count();
        let jumps: Vec<String> = w
            .entries
            .iter()
            .filter(|e| e.hits & Trigger::ReferenceJump.bit() != 0)
            .filter_map(|e| e.block.reference.map(|r| (e.block.index, r)))
            .map(|(i, r)| format!("{} samples at block {i} ({})", r.jumped, r.event_names().collect::<Vec<_>>().join(", ")))
            .collect();
        let mut s = format!(
            "The echo reference: its lead {:.1} ms against its target {:.1} ms ({}); the loopback {} at block {}; {short} of the {} \
             takes up to it short",
            r.lead_ms,
            r.target_ms,
            if r.stamped { "by the stamps" } else { "by delivery" },
            if r.fresh { "delivering" } else { "stale" },
            t.block.index,
            second.len()
        );
        if !jumps.is_empty() {
            let _ = write!(s, "; its timeline jumped by {}", jumps.join("; "));
        }
        s.push('.');
        lines.push(s);
    }
    let lost = w.entries.last().map_or(0, |e| e.lost).saturating_sub(w.entries.first().map_or(0, |e| e.lost));
    if lost > 0 && w.entries.first().map(|e| e.run) == w.entries.last().map(|e| e.run) {
        lines.push(format!("{lost} blocks of the window were lost (the recorder's ring was full): the CSV skips them."));
    }

    let blocks_at_wake = |e: &Entry| near.iter().filter(|x| x.block.thread.wake_hns == e.block.thread.wake_hns).count();
    let gap_limit = gap_usual.map_or(f64::INFINITY, |u| (3.0 * u).max(u + 15.0));
    let held = gap_max.filter(|g| f64::from(g.block.thread.wake_gap_ms) >= gap_limit && stalls == 0);
    let mic_limit = mic_usual.map_or(f64::INFINITY, |u| (3.0 * u).max(u + 20.0));
    let mic_quiet = stalls > 0 || mic_max.is_some_and(|(d, _)| d >= mic_limit);
    let index = t.block.index;
    let verdict = if let (Trigger::Fault, Some(m)) = (w.first.trigger, &w.first.words) {
        format!("The audio stopped by itself: {m}")
    } else if let Some(g) = held {
        let e = &g.block;
        let waiting = e.mic_queued as usize >= BLOCK || blocks_at_wake(g) > 1 || e.thread.mic_glitches > 0;
        if waiting && mic_alone {
            format!(
                "The audio thread was held up, or the mic's driver held its samples back: no wake for {:.0} ms before block {} (it \
                 usually wakes every {:.1} ms), and the mic's samples were waiting when it woke{}. With a shared output the thread \
                 wakes on the mic's events alone, so the facts can't tell a thread kept from running (a driver's DPCs, another \
                 real-time thread, power management) from a driver that delivered late, all at once; with an exclusive output its \
                 events would.",
                e.thread.wake_gap_ms,
                e.index,
                gap_usual.unwrap_or(0.0),
                if e.thread.mic_glitches > 0 { " (and some were lost: a mic glitch)" } else { "" }
            )
        } else if waiting {
            format!(
                "The audio thread was held up: no wake for {:.0} ms before block {} (it usually wakes every {:.1} ms), and the mic's \
                 samples were waiting when it woke{}. The thread was late, not the device: something kept it from running (a \
                 driver's DPCs, another real-time thread, power management).",
                e.thread.wake_gap_ms,
                e.index,
                gap_usual.unwrap_or(0.0),
                if e.thread.mic_glitches > 0 { " (and some were lost: a mic glitch)" } else { "" }
            )
        } else {
            format!(
                "No wake for {:.0} ms before block {} (the thread usually wakes every {:.1} ms), with nothing waiting from the mic when \
                 it woke: the devices' events came late (their driver held them), or the thread and the devices were held up \
                 together.",
                e.thread.wake_gap_ms,
                e.index,
                gap_usual.unwrap_or(0.0)
            )
        }
    } else if mic_quiet {
        let gap = mic_max.map_or(0.0, |m| m.0);
        let timed = if stalls > 0 { format!(" ({} without its event)", waits(stalls)) } else { String::new() };
        format!(
            "The mic stopped delivering: no packet for {gap:.0} ms before block {index}{timed}, while the audio thread woke on time. \
             Its device or driver held its audio back (a USB link, a driver's buffering, an exclusive-mode conflict)."
        )
    } else if let Some(s) = slowest.filter(|s| s.block.thread.process_us >= SLOW_BLOCK_US) {
        format!(
            "Block {} took the audio thread {:.1} ms (a block lasts 10 ms; usually {}): the processing itself fell behind.",
            s.block.index,
            s.block.thread.process_us / 1000.0,
            usual(proc_usual, "us")
        )
    } else {
        let on_time = "with the audio thread and the mic on time";
        match w.first.trigger.kind() {
            Kind::Output if w.first.trigger == Trigger::OutputDrop => format!(
                "The output overfilled at block {index} {on_time}: the device took its samples more slowly than they came (it \
                 stalled, or its clock ran slow)."
            ),
            Kind::Output if o.late => format!(
                "The audio thread answered the exclusive output's event late at block {index}: more than a device period after \
                 it came, so the device played a stale period of silence (the queue held {} samples: it wasn't dry). A bigger \
                 device period leaves the thread more room; the filter raises it after repeated late answers.",
                o.padding
            ),
            Kind::Output => format!(
                "The output ran dry at block {index} {on_time}: the device took its samples faster than they came, or its driver \
                 dropped or skipped some (padding {} samples when the block was written).",
                o.padding
            ),
            Kind::Reference if w.first.trigger == Trigger::ReferenceLost => {
                format!("The echo reference stopped at block {index}: its loopback failed, and the filter carries on without it.")
            }
            Kind::Reference if w.first.trigger == Trigger::ReferenceJump => format!(
                "The echo reference's timeline jumped at block {index} {on_time}: the loopback broke (a stall longer than its \
                 buffers, a device reset, a stamp that jumped), or its lead target moved after its underflows persisted."
            ),
            Kind::Reference => format!(
                "The echo reference's loopback delivered late at block {index} {on_time}: its takes ran short of samples (zeros \
                 owed), which costs the echo canceller."
            ),
            Kind::Mic => format!("The mic lost samples at block {index} {on_time}: its device or driver dropped them."),
            Kind::Model => format!(
                "The model pack's worker had no result in time at block {index} {on_time}: its inference ran late (the substitute \
                 played)."
            ),
            Kind::Thread => format!(
                "The audio thread woke late, or took long, around block {index}: the longest gap between its wakes there {:.1} ms \
                 (usually {}), the slowest block {:.0} us. Not long enough to say the thread was held up rather than its devices' \
                 events late.",
                gap_max.map_or(0.0, |g| g.block.thread.wake_gap_ms),
                usual(gap_usual, "ms"),
                slowest.map_or(0.0, |s| s.block.thread.process_us)
            ),
            Kind::Fault => format!("After block {index}: the facts show nothing more than the fault."),
        }
    };
    Reading { verdict, lines }
}

/// `flight_blocks.csv`'s columns: the recorder's own (the run since it was armed, from 1; the seconds since it was armed by
/// the block's wake; the triggers the block showed, `|`-joined; the blocks its ring had lost before it in its run), then
/// `chain_blocks.csv`'s ([`crate::api::CHAIN_BLOCK_COLUMNS`]).
pub(crate) fn columns() -> String {
    format!("run,t_s,trigger,lost,{}", crate::csv::COLUMNS)
}

/// The window's blocks as CSV, [`columns`] then a row a block.
pub(crate) fn blocks_csv(w: &Window) -> String {
    let mut s = String::with_capacity((w.entries.len() + 1) * 260);
    s.push_str(&columns());
    s.push('\n');
    let mut last: Option<&Entry> = None;
    for e in &w.entries {
        let wake = e.block.thread.wake_hns;
        let t = if wake > 0.0 { crate::csv::num((wake - w.armed_hns) / HNS_PER_S, 3) } else { String::new() };
        let _ = write!(s, "{},{t},{},{},", e.run, Trigger::names(e.hits), e.lost);
        crate::csv::row(&mut s, &e.block, last.filter(|l| l.run == e.run).map(|l| &l.block));
        s.push('\n');
        last = Some(e);
    }
    s
}

/// A wall-clock time, local: as the summary says it, and as a dump's folder name starts (`20261102-140312`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Wall {
    pub(crate) text: String,
    pub(crate) stamp: String,
}

/// The local time at QPC `at_hns`, `now_hns` being QPC now: the system clock now, less how long ago that was.
fn wall_at(at_hns: f64, now_hns: f64) -> Wall {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // SAFETY: no arguments; it only returns the time.
    let ft = unsafe { GetSystemTimeAsFileTime() };
    let now = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let ago = (now_hns - at_hns).max(0.0) as u64;
    let then = now.saturating_sub(ago);
    let ft = FILETIME { dwLowDateTime: then as u32, dwHighDateTime: (then >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: valid pointers to live locals; a failure leaves them zeroed, and the text says so.
    let ok = unsafe { FileTimeToSystemTime(&ft, &mut utc).is_ok() && SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_ok() };
    if !ok {
        return Wall { text: "(the time is unknown)".to_owned(), stamp: "00000000-000000".to_owned() };
    }
    let t = local;
    Wall {
        text: format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        ),
        stamp: format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond),
    }
}

/// The triggers the window's blocks showed that the dump doesn't list itself (before its headline, after it in another
/// dump, or past its list's length), by name: how many blocks, and the one nearest the headline.
fn other_triggers(w: &Window) -> Vec<String> {
    let listed =
        |seq: u64, t: Trigger| (w.first.seq == seq && w.first.trigger == t) || w.also.iter().any(|h| h.seq == seq && h.trigger == t);
    BLOCK_TRIGGERS
        .iter()
        .filter_map(|t| {
            let with: Vec<&Entry> = w.entries.iter().filter(|e| e.hits & t.bit() != 0 && !listed(e.seq, *t)).collect();
            let nearest = with.iter().min_by_key(|e| e.seq.abs_diff(w.first.seq))?;
            let d = (nearest.seq as f64 - w.first.seq as f64) / BLOCKS_PER_S;
            let when = if d < 0.0 { format!("{:.2} s before", -d) } else { format!("{d:.2} s after") };
            Some(format!(
                "{}: {} {}, the nearest {when} the trigger (block {} of run {})",
                t.name(),
                with.len(),
                if with.len() == 1 { "block" } else { "blocks" },
                nearest.block.index,
                nearest.run
            ))
        })
        .collect()
}

/// The dump's `summary.txt`: what triggered, when, the window, the reading, the facts around it, the control side's notes,
/// what the rate limit held back, and the files.
pub(crate) fn summary(w: &Window, wall: &Wall, reading: &Reading, files: &[&str]) -> String {
    let secs = |hns: f64| (hns - w.armed_hns) / HNS_PER_S;
    let h = &w.first;
    let mut s = String::from("noise-filter flight recorder\n\n");
    let _ = writeln!(s, "What:   {} ({})", h.words(), h.trigger.name());
    let block = match h.index {
        Some(i) => format!("block {i} of run {} (about {:.2} s into the run by its blocks)", h.run, i as f64 / BLOCKS_PER_S),
        None => format!("after block {} of run {}", w.entries.iter().rfind(|e| e.seq <= h.seq).map_or(0, |e| e.block.index), h.run),
    };
    let _ = writeln!(s, "When:   {} (local), {:.2} s after the recorder was armed; {block}", wall.text, secs(h.at_hns));
    match (w.entries.first(), w.entries.last()) {
        (Some(a), Some(b)) => {
            let before = h.seq.saturating_sub(a.seq) as f64 / BLOCKS_PER_S;
            let after = b.seq.saturating_sub(h.seq) as f64 / BLOCKS_PER_S;
            let _ = write!(
                s,
                "Window: {} blocks, block {} of run {} to block {} of run {}: {before:.2} s before the trigger and {after:.2} s after",
                w.entries.len(),
                a.block.index,
                a.run,
                b.block.index,
                b.run
            );
            if w.post_short > 0 {
                let _ = write!(s, " (the post-roll {} blocks short: the recorder stopped, or no blocks came)", w.post_short);
            }
            s.push('\n');
        }
        _ => s.push_str("Window: no blocks\n"),
    }
    match &w.audio {
        Some(a) => {
            let from = a
                .from_seq
                .and_then(|q| w.entries.iter().find(|e| e.seq == q))
                .map_or(String::new(), |e| format!(" from block {} of run {}", e.block.index, e.run));
            let _ = writeln!(s, "Audio:  {:.2} s{from} (mic.wav, ref.wav, out.wav)", a.mic.len() as f64 / f64::from(SAMPLE_RATE));
        }
        None => s.push_str("Audio:  not kept (run --flight-audio keeps the last 5 s)\n"),
    }
    if !w.also.is_empty() {
        let n = w.also.len() as u64 + w.more;
        let _ = writeln!(s, "\nAlso in this dump, at its block, within half a second or in its post-roll ({n}):");
        for a in &w.also {
            let at = a.index.map_or(String::new(), |i| format!(", block {i} of run {}", a.run));
            let _ = writeln!(s, "  - {:.2} s: {}{at}", secs(a.at_hns), a.words());
        }
        if w.more > 0 {
            let _ = writeln!(s, "  - and {} more", w.more);
        }
    }
    let others = other_triggers(w);
    if !others.is_empty() {
        s.push_str("\nOther triggers in the window (not in this dump's list; another dump may hold them):\n");
        for l in &others {
            let _ = writeln!(s, "  - {l}");
        }
    }
    let held: Vec<String> =
        Kind::ALL.iter().zip(w.suppressed).filter(|(_, n)| *n > 0).map(|(k, n)| format!("{n} of {}", k.words())).collect();
    if !held.is_empty() {
        let _ = writeln!(
            s,
            "\nNot dumped since the last dump (at most one every {:.0} s and {} an arming for each kind, {} in all): {}.",
            w.limits.min_interval_hns / HNS_PER_S,
            w.limits.max_per_kind,
            w.limits.max_dumps,
            held.join(", ")
        );
    }
    let _ = writeln!(s, "\nReading (a heuristic over the facts; flight_blocks.csv has every block):\n  {}", reading.verdict);
    if !reading.lines.is_empty() {
        s.push_str("\nAround it:\n");
        for l in &reading.lines {
            let _ = writeln!(s, "  - {l}");
        }
    }
    if !w.notes.is_empty() {
        s.push_str("\nThe engine's control side:\n");
        for n in &w.notes {
            let _ = writeln!(s, "  - {:.2} s: {}", secs(n.at_hns), n.note.text());
        }
    }
    let _ = writeln!(
        s,
        "\nFiles: {}. flight_blocks.csv has a row a block: the run since the recorder was armed, the seconds since then, the \
         triggers the block showed and the blocks lost before it, then chain_blocks.csv's columns (engine.md, \"The chain \
         recorder\" and \"The flight recorder\").",
        files.join(", ")
    );
    s
}

/// Writes 48 kHz mono float.
fn write_wav(path: &Path, x: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec { channels: 1, sample_rate: SAMPLE_RATE, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let fail = |e: hound::Error| format!("couldn't write {}: {e}", path.display());
    let mut w = hound::WavWriter::create(path, spec).map_err(fail)?;
    for &v in x {
        w.write_sample(v).map_err(fail)?;
    }
    w.finalize().map_err(fail)
}

/// The prefix of a dump's folder name.
const PREFIX: &str = "flight-";

/// Writes the window into a folder of its own in `dir` and keeps the newest `keep` dump folders there.
pub(crate) fn write_dump(dir: &Path, w: &Window, wall: &Wall, keep: usize) -> Result<FlightDump, String> {
    let fail = |what: &Path, e: std::io::Error| format!("couldn't write {}: {e}", what.display());
    std::fs::create_dir_all(dir).map_err(|e| fail(dir, e))?;
    let base = format!("{PREFIX}{}-{}", wall.stamp, w.first.trigger.name());
    let path = (1..1000)
        .map(|n| if n == 1 { dir.join(&base) } else { dir.join(format!("{base}-{n}")) })
        .find(|p| !p.exists())
        .ok_or_else(|| format!("no free name for a dump in {}", dir.display()))?;
    std::fs::create_dir(&path).map_err(|e| fail(&path, e))?;
    let mut files = vec!["summary.txt", "flight_blocks.csv"];
    let csv = path.join("flight_blocks.csv");
    std::fs::write(&csv, blocks_csv(w)).map_err(|e| fail(&csv, e))?;
    if let Some(a) = &w.audio {
        for (name, x) in [("mic.wav", &a.mic), ("ref.wav", &a.reference), ("out.wav", &a.output)] {
            write_wav(&path.join(name), x)?;
            files.push(name);
        }
    }
    let reading = read(w);
    let text = path.join("summary.txt");
    std::fs::write(&text, summary(w, wall, &reading, &files)).map_err(|e| fail(&text, e))?;
    prune(dir, keep);
    let also = Trigger::all().filter(|t| *t != w.first.trigger && w.also.iter().any(|h| h.trigger == *t)).map(Trigger::name).collect();
    Ok(FlightDump {
        path,
        trigger: w.first.words(),
        also,
        at_s: (w.first.at_hns - w.armed_hns) / HNS_PER_S,
        local_time: wall.text.clone(),
        verdict: reading.verdict,
    })
}

/// Whether `name` is as [`write_dump`] names a dump's folder: `flight-YYYYMMDD-HHMMSS-<trigger>`, and `-N` (2-999) after it
/// when the name was taken.
fn is_dump_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = name.strip_prefix(PREFIX) else { return false };
    let mut parts = rest.splitn(3, '-');
    let (Some(date), Some(time), Some(rest)) = (parts.next(), parts.next(), parts.next()) else { return false };
    if !(date.len() == 8 && digits(date) && time.len() == 6 && digits(time)) {
        return false;
    }
    Trigger::all().any(|t| match rest.strip_prefix(t.name()) {
        Some("") => true,
        Some(n) => n.strip_prefix('-').is_some_and(|n| n.len() <= 3 && digits(n) && !n.starts_with('0') && n != "1"),
        None => false,
    })
}

/// Deletes the oldest dump folders in `dir` past the newest `keep`: only folders named as [`write_dump`] names them (their
/// names sort by time) with a `summary.txt`.
fn prune(dir: &Path, keep: usize) {
    let Ok(list) = std::fs::read_dir(dir) else { return };
    let mut dumps: Vec<PathBuf> = list
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("summary.txt").is_file())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_dump_name))
        .collect();
    dumps.sort();
    let over = dumps.len().saturating_sub(keep);
    for p in dumps.into_iter().take(over) {
        let _ = std::fs::remove_dir_all(p);
    }
}

/// One run's ring, as the recorder's thread drains it.
struct RunRing {
    run: u32,
    rx: Consumer<ChainItem>,
    lost: Arc<AtomicU64>,
}

/// What the handle, the recorder's thread and the control side share.
#[derive(Default)]
pub(crate) struct Link {
    stop: AtomicBool,
    /// Rings for runs started since the thread last looked.
    rings: Mutex<Vec<RunRing>>,
    notes: Mutex<Vec<Noted>>,
    stats: Mutex<FlightStats>,
}

/// The control side's flight recorder state (`Inner::flight`).
#[derive(Default)]
pub(crate) struct Slot {
    /// The armed recorder: each start makes its run a ring.
    link: Option<Arc<Link>>,
    /// The live run's hand-over port.
    port: Option<Port<ChainTx>>,
    /// Runs given a ring since the recorder was armed.
    runs: u32,
}

/// Makes run `run` a ring (its pages touched), puts its producer in the run's command ring and hands its consumer to the
/// recorder's thread. False when the command ring is full (a producer the run hasn't taken yet).
fn hand_ring(link: &Link, port: &mut Port<ChainTx>, run: u32) -> bool {
    let (tx, rx) = recorder::touched_ring();
    let tx = ChainTx::new(tx);
    let lost = tx.lost();
    if port.cmd.push(tx).is_err() {
        return false;
    }
    lock(&link.rings).push(RunRing { run, rx, lost });
    true
}

/// A start's hand-over rings, made before the run is handed to the audio thread: while a flight recorder is armed, the run's
/// own ring is in the command ring already, so its first wake takes it before its first block.
pub(crate) fn run_channel(inner: &Inner) -> (Port<ChainTx>, End<ChainTx>) {
    let (mut port, end) = handover::channel();
    let mut guard = lock(&inner.flight);
    let slot = &mut *guard;
    if let Some(link) = slot.link.as_ref() {
        slot.runs += 1;
        hand_ring(link, &mut port, slot.runs);
    }
    (port, end)
}

/// The run started: the control side keeps its port.
pub(crate) fn run_started(inner: &Inner, port: Port<ChainTx>) {
    lock(&inner.flight).port = Some(port);
}

/// The start failed: its ring, handed back untaken as the run ended (`run_job`), is freed here with the port; the recorder's
/// thread sees it abandoned, empty, and drops it.
pub(crate) fn run_failed(port: Port<ChainTx>) {
    drop(port);
}

/// The run stopped: its port goes, with the ring it handed back. The ring itself ended with the run.
pub(crate) fn run_stopped(inner: &Inner) {
    lock(&inner.flight).port = None;
}

/// A control-side note for the armed recorder (none armed: nothing). Never on the audio thread.
pub(crate) fn note(inner: &Inner, note: Note) {
    let link = lock(&inner.flight).link.clone();
    if let Some(link) = link {
        let mut notes = lock(&link.notes);
        if notes.len() < MAX_NOTES {
            notes.push(Noted { at_hns: Qpc::new().now_hns() as f64, note });
        }
    }
}

/// The flight recorder's handle (`FlightRecorder`): its thread, and the engine it records.
pub(crate) struct Handle {
    inner: Weak<Inner>,
    link: Arc<Link>,
    thread: Option<JoinHandle<()>>,
}

fn check(c: &FlightConfig) -> Result<(), EngineError> {
    let within = |v: f64, lo: f64, hi: f64| v.is_finite() && (lo..=hi).contains(&v);
    let ok = within(c.facts_s, 1.0, 60.0)
        && within(c.audio_s, 0.0, 30.0)
        && within(c.post_roll_s, 0.1, 10.0)
        && within(c.min_interval_s, 0.0, 3600.0)
        && c.max_dumps_per_kind >= 1
        && c.max_dumps >= 1
        && c.keep_dumps >= 1;
    if ok {
        Ok(())
    } else {
        Err(EngineError(
            "The flight recorder takes 1-60 s of facts, 0-30 s of audio, a 0.1-10 s post-roll and 0-3600 s between a kind's \
             dumps, and at least one dump for each kind, one in all and one kept."
                .into(),
        ))
    }
}

/// Arms the flight recorder: makes its folder, starts its thread, and gives the live run a ring (or leaves it for the next
/// start's).
pub(crate) fn arm(inner: &Arc<Inner>, config: FlightConfig) -> Result<Handle, EngineError> {
    check(&config)?;
    std::fs::create_dir_all(&config.dir)
        .map_err(|e| EngineError(format!("The flight recorder can't use {}: {e}", config.dir.display())))?;
    if inner.flying.swap(true, AcqRel) {
        return Err(EngineError("The flight recorder is armed already.".into()));
    }
    let link = Arc::new(Link::default());
    let armed_hns = Qpc::new().now_hns() as f64;
    let (l, c) = (link.clone(), config);
    let spawned = std::thread::Builder::new().name("nf-flight".into()).spawn(move || drain(&l, &c, armed_hns));
    let thread = match spawned {
        Ok(t) => t,
        Err(_) => {
            inner.flying.store(false, Release);
            return Err(EngineError("The flight recorder couldn't start a thread.".into()));
        }
    };
    let mut handle = Handle { inner: Arc::downgrade(inner), link: link.clone(), thread: Some(thread) };
    let mut guard = lock(&inner.flight);
    let slot = &mut *guard;
    slot.link = Some(link.clone());
    slot.runs = 0;
    if let Some(port) = slot.port.as_mut() {
        // Rings an earlier recorder's run handed back: freed here.
        while port.ret.pop().is_ok() {}
        slot.runs = 1;
        if !hand_ring(&link, port, 1) {
            drop(guard);
            handle.finish();
            return Err(EngineError("The audio thread didn't take the flight recorder; try again.".into()));
        }
    }
    Ok(handle)
}

impl Handle {
    pub(crate) fn stats(&self) -> FlightStats {
        lock(&self.link.stats).clone()
    }

    pub(crate) fn stop(mut self) -> FlightStats {
        self.finish()
    }

    /// Stops it, once: no ring for a later start; the thread drains once more, dumps the triggers still in their post-rolls
    /// and ends (dropping the rings' consumers); the live run hands its ring back; the claim is released.
    fn finish(&mut self) -> FlightStats {
        let Some(thread) = self.thread.take() else { return FlightStats::default() };
        let inner = self.inner.upgrade();
        if let Some(inner) = &inner {
            lock(&inner.flight).link = None;
        }
        self.link.stop.store(true, Release);
        // A thread that panicked leaves what it had done.
        let _ = thread.join();
        // A ring a start made meanwhile, which the thread never took.
        lock(&self.link.rings).clear();
        if let Some(inner) = &inner {
            reclaim(inner);
            inner.flying.store(false, Release);
        }
        std::mem::take(&mut *lock(&self.link.stats))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The live run's ring back from the audio thread, which hands it back at its next wake now that its consumer has gone. Not
/// waited for past [`RETURN_WAIT`]: then it's freed with the run's port, at the next arm or when the run stops.
fn reclaim(inner: &Inner) {
    let deadline = Instant::now() + RETURN_WAIT;
    loop {
        {
            let mut slot = lock(&inner.flight);
            match slot.port.as_mut() {
                Some(port) => {
                    if port.ret.pop().is_ok() {
                        return;
                    }
                }
                None => return,
            }
        }
        if Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The recorder's thread: every [`DRAIN_EVERY`], takes the new runs' rings, drains each into the core (in the order the runs
/// started), drops a ring whose run has ended once it's empty, takes the control side's notes, and writes the dumps that are
/// due. At the stop it drains once more and writes the pending dumps with what it has.
fn drain(link: &Link, cfg: &FlightConfig, armed_hns: f64) {
    let qpc = Qpc::new();
    let mut core = Core::new(Limits::of(cfg), armed_hns);
    let mut rings: Vec<RunRing> = Vec::new();
    let mut ended_lost = 0u64;
    loop {
        let stop = link.stop.load(Acquire);
        rings.append(&mut lock(&link.rings));
        let now = qpc.now_hns() as f64;
        rings.retain_mut(|r| {
            // Read first: once the producer has gone, this drain takes every block pushed before.
            let ended = r.rx.is_abandoned();
            while let Ok(item) = r.rx.pop() {
                core.feed(r.run, &item, now);
            }
            if ended {
                ended_lost += r.lost.load(Acquire);
            }
            !ended
        });
        let notes = std::mem::take(&mut *lock(&link.notes));
        for n in notes {
            core.note(n);
        }
        {
            let mut st = lock(&link.stats);
            st.blocks = core.blocks;
            st.lost_blocks = ended_lost + rings.iter().map(|r| r.lost.load(Relaxed)).sum::<u64>();
            if st.suppressed != core.suppressed {
                st.suppressed = core.suppressed;
                st.suppressed_by = Kind::ALL.iter().zip(core.suppressed_by).filter(|(_, n)| *n > 0).map(|(k, n)| (k.words(), n)).collect();
            }
        }
        while let Some(w) = if stop { core.flush() } else { core.poll(now) } {
            let wall = wall_at(w.first.at_hns, qpc.now_hns() as f64);
            let written = write_dump(&cfg.dir, &w, &wall, cfg.keep_dumps);
            let mut st = lock(&link.stats);
            match written {
                Ok(d) => st.dumps.push(d),
                Err(e) => st.errors.push(e),
            }
        }
        if stop {
            return;
        }
        std::thread::sleep(DRAIN_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FlightConfig, OutputFacts, ThreadFacts};

    const T0: f64 = 1e11;

    /// Block `index` of a steady run: woken every 10 ms by the mic, a block a wake, 100 us of work, the output at its target.
    fn steady(index: u64) -> ChainItem {
        let wake = T0 + index as f64 * 1e5;
        let mut it = ChainItem::BLANK;
        it.block.index = index;
        it.block.mic_index = index * 480;
        it.block.mic_wake_hns = wake;
        it.block.capture_qpc_hns = wake - 1e5;
        it.block.output = OutputFacts { padding: 480, fill: 672.0, target: 672, index: index * 480, ..OutputFacts::default() };
        it.block.thread = ThreadFacts { wake_hns: wake, wake_gap_ms: 10.0, wakes: 1, process_us: 100.0, ..ThreadFacts::default() };
        it.mic[0] = index as f32;
        it
    }

    fn limits(pre_s: f64, post_s: f64, audio_s: f64) -> Limits {
        Limits::of(&FlightConfig { facts_s: pre_s, post_roll_s: post_s, audio_s, ..FlightConfig::new("unused") })
    }

    fn underrun(index: u64) -> ChainItem {
        let mut it = steady(index);
        it.block.output.underrun = true;
        it.block.output.zeros = 192;
        it.block.output.padding = 0;
        it
    }

    /// Feeds blocks `from..to` of run 1, each as `make` gives it, and returns the windows that came due.
    fn feed(core: &mut Core, from: u64, to: u64, make: &dyn Fn(u64) -> ChainItem) -> Vec<Window> {
        feed_in(core, from, to, 1, make)
    }

    /// The same, polling only after each `batch` blocks, as the recorder's thread does after each drain of a ring (the clock
    /// then the batch's last block's).
    fn feed_in(core: &mut Core, from: u64, to: u64, batch: u64, make: &dyn Fn(u64) -> ChainItem) -> Vec<Window> {
        let mut out = Vec::new();
        let mut i = from;
        while i < to {
            let end = (i + batch).min(to);
            let mut now = 0.0;
            for k in i..end {
                let it = make(k);
                now = it.block.thread.wake_hns;
                core.feed(1, &it, now);
            }
            while let Some(w) = core.poll(now) {
                out.push(w);
            }
            i = end;
        }
        out
    }

    fn firsts(windows: &[Window]) -> Vec<(Trigger, Option<u64>)> {
        windows.iter().map(|w| (w.first.trigger, w.first.index)).collect()
    }

    fn span(w: &Window) -> (u64, u64, usize) {
        (w.entries[0].block.index, w.entries.last().unwrap().block.index, w.entries.len())
    }

    /// One underrun makes one dump, 3 s after it, holding the 10 s before it, the trigger's block and the 3 s after; the
    /// rest of the run, recorded on, makes none. The audio, when kept, covers its own (shorter) pre-roll and the post-roll.
    #[test]
    fn a_trigger_dumps_its_window_with_the_pre_and_post_roll() {
        let mut core = Core::new(limits(10.0, 3.0, 5.0), T0);
        let make = |i| if i == 2_000 { underrun(i) } else { steady(i) };
        let windows = feed(&mut core, 0, 6_000, &make);
        assert_eq!(windows.len(), 1);
        let w = &windows[0];
        assert_eq!((w.first.trigger, w.first.index), (Trigger::OutputUnderrun, Some(2_000)));
        assert_eq!(w.also.iter().map(|h| h.trigger).collect::<Vec<_>>(), [Trigger::OutputDeficit], "the same block's other trigger");
        let idx: Vec<u64> = w.entries.iter().map(|e| e.block.index).collect();
        assert_eq!((idx[0], *idx.last().unwrap(), idx.len()), (1_000, 2_300, 1_301), "10 s before, 3 s after");
        assert!(idx.windows(2).all(|p| p[1] == p[0] + 1));
        assert_eq!(w.entries.iter().filter(|e| e.hits != 0).count(), 1);
        assert_eq!(w.post_short, 0);
        let a = w.audio.as_ref().unwrap();
        assert_eq!(a.mic.len(), 801 * BLOCK, "5 s before, the trigger's block, 3 s after");
        assert_eq!((a.mic[0], a.mic[800 * BLOCK]), (1_500.0, 2_300.0));
        assert_eq!(core.suppressed, 0);
        assert!((w.first.at_hns - (T0 + 2_000.0 * 1e5)).abs() < 1.0);
    }

    fn stall(i: u64) -> ChainItem {
        let mut it = steady(i);
        it.block.thread.stalls = 1;
        it
    }

    /// At most one dump a minute for each kind: a second underrun 20 s after the first is counted, not dumped, and the next
    /// dump says so; one 70 s after is dumped. Another kind (a mic stall) 5 s after the first dump gets its own. At most
    /// `max_dumps_per_kind` for each kind, whatever the interval, with another kind's dump after; and no more than
    /// `max_dumps` in all.
    #[test]
    fn dumps_are_rate_limited_by_kind_and_in_all() {
        let mut core = Core::new(limits(2.0, 1.0, 0.0), T0);
        let make = move |i| match i {
            1_000 | 3_000 | 8_000 => underrun(i),
            1_500 => stall(i),
            _ => steady(i),
        };
        let windows = feed(&mut core, 0, 10_000, &make);
        assert_eq!(firsts(&windows), [
            (Trigger::OutputUnderrun, Some(1_000)),
            (Trigger::MicStall, Some(1_500)),
            (Trigger::OutputUnderrun, Some(8_000))
        ]);
        assert_eq!(core.suppressed, 2, "block 3,000's underrun and deficit");
        assert_eq!(windows[2].suppressed[Kind::Output as usize], 2, "said in the next dump");
        assert_eq!(windows[1].suppressed, [0; KINDS]);

        // Ten underruns 5 s apart: the kind's three, and the stall after them its own.
        let mut core = Core::new(Limits { min_interval_hns: 0.0, ..limits(1.0, 0.5, 0.0) }, T0);
        let windows = feed(&mut core, 0, 6_000, &|i| match i {
            _ if i % 500 == 499 && i < 5_000 => underrun(i),
            5_500 => stall(i),
            _ => steady(i),
        });
        assert_eq!(firsts(&windows), [
            (Trigger::OutputUnderrun, Some(499)),
            (Trigger::OutputUnderrun, Some(999)),
            (Trigger::OutputUnderrun, Some(1_499)),
            (Trigger::MicStall, Some(5_500))
        ]);
        assert_eq!((core.suppressed, core.suppressed_by[Kind::Output as usize]), (2 * 7, 2 * 7), "the other seven, with their deficits");

        let mut core = Core::new(Limits { max_dumps: 2, min_interval_hns: 0.0, ..limits(1.0, 0.5, 0.0) }, T0);
        let windows = feed(&mut core, 0, 5_000, &|i| if i % 500 == 499 { underrun(i) } else { steady(i) });
        assert_eq!(windows.len(), 2, "max_dumps");
        assert_eq!(core.suppressed, 2 * 8, "the other eight underruns, each with its deficit");
    }

    /// Soak 2's shape: the reference's bursts of short takes every 5 s from 92 s to 595 s and again at 1234-1251 s, output
    /// underruns at 110 s and 900 s. The reference has its three dumps a minute apart and no more; both underruns have theirs,
    /// the one at 900 s too; the reference's other bursts are counted, by kind.
    #[test]
    fn a_kind_that_keeps_misbehaving_cant_crowd_out_another_later() {
        let burst = |i: u64| {
            let cluster = |from: u64, to: u64| (from..=to).contains(&i) && (i - from) % 500 < 60 && i % 10 == 0;
            cluster(9_200, 59_500) || cluster(123_400, 125_100)
        };
        let make = |i: u64| {
            let mut it = with_reference(i, |i, r| {
                if burst(i) {
                    r.owed = 120;
                }
            });
            if i == 11_000 || i == 90_000 {
                it.block.output = underrun(i).block.output;
            }
            it
        };
        let mut core = Core::new(Limits::of(&FlightConfig::new("unused")), T0);
        let windows = feed_in(&mut core, 0, 130_000, 3, &make);
        assert_eq!(firsts(&windows), [
            (Trigger::ReferenceShort, Some(9_240)),
            (Trigger::OutputUnderrun, Some(11_000)),
            (Trigger::ReferenceShort, Some(15_240)),
            (Trigger::ReferenceShort, Some(21_240)),
            (Trigger::OutputUnderrun, Some(90_000)),
        ]);
        let bursts = 101 + 4;
        assert_eq!(core.suppressed_by, [0, 0, 0, 0, bursts - 3, 0], "only the reference's held back");
    }

    /// A trigger of another kind late in a pending dump's post-roll (an underrun 2.86 s after a burst of short takes) gets a
    /// dump of its own, with its whole pre- and post-roll: two dumps, overlapping. One within half a second of it is the same
    /// incident: one dump, the more serious trigger leading it and the window centred on that.
    #[test]
    fn another_kinds_trigger_in_a_post_roll_gets_its_own_dump() {
        let make = |under: u64| {
            move |i: u64| {
                let mut it = with_reference(i, |i, r| {
                    if (1_964..=2_004).contains(&i) && i % 10 == 4 {
                        r.owed = 60;
                    }
                });
                if i == under {
                    it.block.output = underrun(i).block.output;
                }
                it
            }
        };
        let mut core = Core::new(limits(10.0, 3.0, 0.0), T0);
        let windows = feed(&mut core, 0, 4_000, &make(2_290));
        assert_eq!(firsts(&windows), [(Trigger::ReferenceShort, Some(2_004)), (Trigger::OutputUnderrun, Some(2_290))]);
        assert_eq!(span(&windows[0]), (1_004, 2_304, 1_301));
        assert_eq!(span(&windows[1]), (1_290, 2_590, 1_301), "the underrun's own window, its whole post-roll");
        assert!(windows[0].also.is_empty() && windows[1].also.len() == 1, "the underrun is the second's");
        // Each summary names the other's trigger in its window.
        let wall = Wall { text: String::new(), stamp: String::new() };
        let text = |w: &Window| summary(w, &wall, &read(w), &[]);
        let (first, second) = (text(&windows[0]), text(&windows[1]));
        assert!(
            first.contains("Other triggers in the window") && first.contains("output-underrun: 1 block, the nearest 2.86 s after"),
            "{first}"
        );
        assert!(second.contains("reference-short: 1 block, the nearest 2.86 s before the trigger (block 2004 of run 1)"), "{second}");
        assert!(!second.contains("output-deficit:"), "its own block's deficit is in its list: {second}");
        assert_eq!(core.suppressed, 0);

        let mut core = Core::new(limits(10.0, 3.0, 0.0), T0);
        let windows = feed(&mut core, 0, 4_000, &make(2_030));
        assert_eq!(firsts(&windows), [(Trigger::OutputUnderrun, Some(2_030))], "one dump, the underrun leading it");
        let w = &windows[0];
        assert_eq!(span(w), (1_030, 2_330, 1_301));
        assert_eq!(w.also.iter().map(|h| h.trigger).collect::<Vec<_>>(), [Trigger::ReferenceShort, Trigger::OutputDeficit]);
        // Its kinds both had their dump: neither has another within the minute.
        assert_eq!(
            (core.last_dump_hns[Kind::Output as usize].is_some(), core.last_dump_hns[Kind::Reference as usize].is_some()),
            (true, true)
        );
    }

    /// The recorder's thread polls after a batch of blocks (two or three each 20 ms; hundreds if it was starved): a dump is
    /// cut as its post-roll's last block comes in, so its window is whole whatever the batches, and a trigger after it in the
    /// same batch opens a dump of its own (or, within the minute, is counted).
    #[test]
    fn a_dump_is_cut_at_its_post_rolls_end_whatever_the_batches() {
        let make = |i| if i == 2_001 || i == 2_310 { underrun(i) } else { steady(i) };
        for batch in [1, 3, 100, 400] {
            let mut core = Core::new(Limits { min_interval_hns: 0.0, ..limits(10.0, 3.0, 0.0) }, T0);
            let windows = feed_in(&mut core, 0, 3_000, batch, &make);
            assert_eq!(firsts(&windows), [(Trigger::OutputUnderrun, Some(2_001)), (Trigger::OutputUnderrun, Some(2_310))], "{batch}");
            assert_eq!((span(&windows[0]), span(&windows[1])), ((1_001, 2_301, 1_301), (1_310, 2_610, 1_301)), "{batch}");
            assert!(windows.iter().all(|w| w.post_short == 0 && w.also.len() == 1), "{batch}");
            let mut core = Core::new(limits(10.0, 3.0, 0.0), T0);
            let windows = feed_in(&mut core, 0, 3_000, batch, &make);
            assert_eq!((windows.len(), span(&windows[0]), core.suppressed), (1, (1_001, 2_301, 1_301), 2), "{batch}: counted");
        }
    }

    /// The recorder's memory is its rings', allocated when it's armed: after a long run with dumps of two kinds pending at
    /// once, audio kept, and many notes, they're as allocated and full.
    #[test]
    fn the_recorders_memory_is_bounded() {
        let mut core = Core::new(Limits { max_per_kind: 10, ..limits(10.0, 3.0, 5.0) }, T0);
        let caps = core.capacities();
        let windows = feed(&mut core, 0, 30_000, &|i| match i % 7_000 {
            6_999 => underrun(i),
            99 if i > 7_000 => stall(i),
            _ => steady(i),
        });
        for k in 0..200 {
            core.note(Noted { at_hns: T0 + f64::from(k), note: Note::Restarting("a test") });
        }
        assert_eq!(windows.len(), 8);
        assert_eq!(core.capacities(), caps, "never grown");
        assert_eq!((core.facts.len(), core.audio.len(), core.notes.len()), (1_301, 801, MAX_NOTES));
        assert!(caps.0 < 2 * 1_301 && caps.1 < 2 * 801 && caps.3 < 2 * KINDS && caps.4 < 2 * KINDS, "{caps:?}");
        // About 0.3 KB an entry (0.4 MB for 13 s), 5.8 KB a block of audio (4.6 MB for 8 s).
        assert!(std::mem::size_of::<Entry>() <= 512 && std::mem::size_of::<Sound>() <= 6 * 1024);
    }

    fn with_reference(i: u64, f: impl Fn(u64, &mut ReferenceFacts)) -> ChainItem {
        let mut it = steady(i);
        let mut r = ReferenceFacts { consumed: 480, lead_ms: 12.0, target_ms: 12.0, fresh: true, stamped: true, ..Default::default() };
        if i == 3 {
            r.events = ReferenceFacts::ALIGNED;
            r.jumped = -200;
        }
        f(i, &mut r);
        it.block.reference = Some(r);
        it
    }

    /// The reference's start-up (its alignment, its centring, a raise of its target) is by design: no trigger. After it, a
    /// broken timeline is a jump; five short takes in a second a burst, once until a clean second; and a block without a
    /// reference after one with it is the reference lost.
    #[test]
    fn the_reference_triggers_after_its_start_up() {
        let make = |i| {
            if i >= 17_000 {
                return steady(i);
            }
            with_reference(i, |i, r| match i {
                100 => r.events = ReferenceFacts::RETARGETED,
                253 => (r.events, r.jumped) = (ReferenceFacts::CENTRED, 12),
                1_000 => (r.events, r.jumped) = (ReferenceFacts::BROKEN | ReferenceFacts::COASTED, -480),
                // Five short takes from 8,000 to 8,040, and more after: one burst.
                8_000..=8_100 if i % 10 == 0 => r.owed = 120,
                // A clean second, then another burst.
                10_000..=10_040 if i % 10 == 0 => r.owed = 60,
                _ => {}
            })
        };
        let mut core = Core::new(limits(1.0, 0.5, 0.0), T0);
        let windows = feed(&mut core, 0, 18_000, &make);
        let firsts: Vec<(Trigger, Option<u64>)> = windows.iter().map(|w| (w.first.trigger, w.first.index)).collect();
        let expect =
            [(Trigger::ReferenceJump, Some(1_000)), (Trigger::ReferenceShort, Some(8_040)), (Trigger::ReferenceLost, Some(17_000))];
        assert_eq!(firsts, expect, "the second burst came within the minute");
        assert_eq!(core.suppressed, 1);
        let mut core = Core::new(Limits { min_interval_hns: 0.0, max_per_kind: 10, ..limits(1.0, 0.5, 0.0) }, T0);
        let windows = feed(&mut core, 0, 18_000, &make);
        let firsts: Vec<Trigger> = windows.iter().map(|w| w.first.trigger).collect();
        assert_eq!(firsts, [Trigger::ReferenceJump, Trigger::ReferenceShort, Trigger::ReferenceShort, Trigger::ReferenceLost]);
        assert_eq!(windows[2].first.index, Some(10_040));
    }

    /// A fault comes as a note, after the run's last block: the dump waits out the post-roll by time when no blocks come
    /// (the retry hasn't started), its window ends with the run's last blocks, and its reading says what stopped.
    #[test]
    fn a_fault_dumps_by_time_when_no_blocks_come() {
        let mut core = Core::new(limits(2.0, 3.0, 0.0), T0);
        assert!(feed(&mut core, 0, 1_000, &steady).is_empty());
        let at = T0 + 1_000.0 * 1e5 + 2e6;
        core.note(Noted { at_hns: at, note: Note::Fault("The microphone stopped sending audio.".into()) });
        assert!(core.poll(at + 3.4e7).is_none(), "the post-roll and its slack");
        let w = core.poll(at + 3.6e7).unwrap();
        assert_eq!((w.first.trigger, w.first.index, w.entries.len()), (Trigger::Fault, None, 201));
        assert_eq!((w.entries.last().unwrap().block.index, w.post_short), (999, 300));
        assert_eq!(w.notes.len(), 1);
        let r = read(&w);
        assert!(r.verdict.contains("stopped by itself") && r.verdict.contains("stopped sending"), "{}", r.verdict);
        // A stop with a trigger pending flushes it.
        core.feed(2, &underrun(5), at + 4e7);
        let w = core.flush().unwrap();
        assert_eq!((w.first.trigger, w.first.run, w.post_short), (Trigger::OutputUnderrun, 2, 300));
        assert!(core.flush().is_none());
    }

    /// The reading tells a held-up thread (a long gap between wakes, the mic's samples waiting when it woke) from a mic that
    /// stopped delivering (waits timing out) from an output that ran dry with both on time. With a shared output (the thread
    /// woken by the mic alone) a held thread looks like a mic's driver that held its samples back, and it says so.
    #[test]
    fn the_reading_tells_a_held_thread_from_a_quiet_mic_from_a_dry_output() {
        let window = |make: &dyn Fn(u64) -> ChainItem| {
            let mut core = Core::new(limits(10.0, 1.0, 0.0), T0);
            feed(&mut core, 0, 1_500, make).pop().unwrap()
        };
        // Held: no wake for 60 ms, then six blocks at one wake (the mic's samples waited); with the exclusive output's events
        // waking the thread every 2 ms otherwise, or the mic's alone.
        let held = |exclusive: bool| {
            move |i: u64| {
                let mut it = steady(i);
                if exclusive {
                    (it.block.thread.wake_gap_ms, it.block.thread.wakes, it.block.thread.out_wakes) = (2.0, 5, 4);
                }
                if (1_200..=1_205).contains(&i) {
                    let wake = T0 + 1_205.0 * 1e5;
                    it.block.thread.wake_hns = wake;
                    it.block.mic_wake_hns = wake;
                    let woke = if i == 1_200 { (1, u32::from(exclusive), 60.0) } else { (0, 0, 0.0) };
                    (it.block.thread.wakes, it.block.thread.out_wakes, it.block.thread.wake_gap_ms) = woke;
                    it.block.mic_queued = ((1_205 - i) * 480) as u32;
                }
                if i == 1_205 {
                    it.block.output.underrun = true;
                }
                it
            }
        };
        let r = read(&window(&held(true)));
        assert!(r.verdict.starts_with("The audio thread was held up: no wake for 60 ms before block 1200"), "{}", r.verdict);
        assert!(r.verdict.contains("not the device") && r.lines[1].contains("the exclusive output's events woke it"), "{r:?}");
        let r = read(&window(&held(false)));
        let either = "The audio thread was held up, or the mic's driver held its samples back: no wake for 60 ms before block 1200";
        assert!(r.verdict.starts_with(either) && r.verdict.contains("can't tell"), "{}", r.verdict);
        assert!(r.lines[1].contains("it waits on the mic's events alone (the output is shared)"), "{:?}", r.lines);
        // Quiet: the mic's packets stop for 150 ms before block 1,200 (a wait timed out), the thread waking on time.
        let quiet = |i: u64| {
            let mut it = steady(i);
            if i >= 1_200 {
                for t in [&mut it.block.thread.wake_hns, &mut it.block.mic_wake_hns, &mut it.block.capture_qpc_hns] {
                    *t += 1.5e6;
                }
            }
            if i == 1_200 {
                (it.block.thread.stalls, it.block.thread.wake_gap_ms) = (1, 100.0);
                it.block.output.underrun = true;
            }
            it
        };
        let r = read(&window(&quiet));
        assert!(r.verdict.starts_with("The mic stopped delivering: no packet for 160 ms before block 1200"), "{}", r.verdict);
        let dry = |i: u64| if i == 1_200 { underrun(i) } else { steady(i) };
        let r = read(&window(&dry));
        assert!(r.verdict.starts_with("The output ran dry at block 1200 with the audio thread and the mic on time"), "{}", r.verdict);
        assert!(r.lines[0].contains("padding 0 samples") && r.lines[0].contains("192 zeros written"), "{:?}", r.lines);
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("nf-engine-tests").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A dump's folder holds the CSV (a row a block, the columns the header names), the summary (what, when, the reading) and
    /// the WAVs when audio is kept; past `keep`, the oldest dump folders go, and nothing else in the folder does.
    #[test]
    fn a_dump_writes_its_files_and_keeps_the_newest() {
        let dir = test_dir("flight-dump");
        std::fs::create_dir(dir.join("flight-not-a-dump")).unwrap();
        // A folder of the user's that only looks like one: a summary.txt, and the prefix and a time.
        for mine in ["flight-notes", "flight-20261102-140300-notes", "flight-20261102-140300-output-underrun-old"] {
            std::fs::create_dir(dir.join(mine)).unwrap();
            std::fs::write(dir.join(mine).join("summary.txt"), "mine").unwrap();
        }
        let mut core = Core::new(limits(1.0, 0.5, 0.5), T0);
        let w = feed(&mut core, 0, 400, &|i| if i == 300 { underrun(i) } else { steady(i) }).pop().unwrap();
        let wall = |s: &str| Wall { text: format!("2026-11-02 14:03:{s}.000"), stamp: format!("20261102-1403{s}") };
        let d = write_dump(&dir, &w, &wall("12"), 2).unwrap();
        assert_eq!(d.path, dir.join("flight-20261102-140312-output-underrun"));
        assert!((d.at_s - 3.0).abs() < 1e-6 && d.trigger == "an output underrun" && d.also == ["output-deficit"], "{d:?}");
        let csv = std::fs::read_to_string(d.path.join("flight_blocks.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        let n = columns().split(',').count();
        assert_eq!((lines.len(), lines[0]), (152, columns().as_str()));
        assert!(lines.iter().all(|l| l.split(',').count() == n), "{}", lines[1]);
        assert!(lines[101].starts_with("1,3.000,output-underrun|output-deficit,0,300,"), "{}", lines[101]);
        let text = std::fs::read_to_string(d.path.join("summary.txt")).unwrap();
        for part in
            ["What:   an output underrun", "2026-11-02 14:03:12.000 (local), 3.00 s after", "block 300 of run 1", "The output ran dry"]
        {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(text.contains(&d.verdict));
        for wav in ["mic.wav", "ref.wav", "out.wav"] {
            assert_eq!(hound::WavReader::open(d.path.join(wav)).unwrap().len(), 101 * 480, "{wav}");
        }
        // Two more: the oldest goes; a name taken gets a number.
        write_dump(&dir, &w, &wall("13"), 2).unwrap();
        let again = write_dump(&dir, &w, &wall("13"), 2).unwrap();
        assert_eq!(again.path, dir.join("flight-20261102-140313-output-underrun-2"));
        assert!(!d.path.exists() && dir.join("flight-20261102-140313-output-underrun").exists());
        assert!(dir.join("flight-not-a-dump").exists(), "not a dump: left alone");
        for mine in ["flight-notes", "flight-20261102-140300-notes", "flight-20261102-140300-output-underrun-old"] {
            assert!(dir.join(mine).join("summary.txt").exists(), "{mine}: not named as a dump, left alone");
        }
        for (name, dump) in [
            ("flight-20261102-140312-output-underrun", true),
            ("flight-20261102-140312-fault-12", true),
            ("flight-00000000-000000-mic-stall", true),
            ("flight-20261102-140312-output-underrun-1", false),
            ("flight-20261102-140312-output-underrun-02", false),
            ("flight-20261102-140312-output-underrun-1000", false),
            ("flight-2026110-1403120-output-underrun", false),
            ("flight-20261102-140312-mic", false),
            ("flight-20261102-140312", false),
        ] {
            assert_eq!(is_dump_name(name), dump, "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The fake audio thread's run: its stop, the blocks it has processed, and whether it holds a ring.
    #[derive(Default)]
    struct Run {
        stop: AtomicBool,
        blocks: AtomicU64,
        holding: AtomicBool,
    }

    /// Stands in for the audio thread, as recorder.rs's test does: a wake every millisecond takes the ring through its
    /// hand-over and processes one block (stamped with the real clock, so the post-roll's time runs), pushing it while it
    /// holds the ring; block `underrun_at` of the run underruns. It drops the ring with the run, as a run that ends does.
    fn fake_run(mut end: End<ChainTx>, run: Arc<Run>, underrun_at: u64) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let qpc = Qpc::new();
            let (mut slot, mut returning) = (None, None);
            let mut index = 0u64;
            while !run.stop.load(Acquire) {
                handover::service(&mut slot, &mut end, false, &mut returning);
                run.holding.store(slot.is_some() || returning.is_some(), Release);
                if let Some(tx) = slot.as_mut() {
                    let mut it = if index == underrun_at { underrun(index) } else { steady(index) };
                    it.block.thread.wake_hns = qpc.now_hns() as f64;
                    tx.push(it.block, &it.mic, &it.reference, &it.output);
                }
                index += 1;
                run.blocks.store(index, Release);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The recorder end to end, with its threads and files: armed before a start, it records the first run; across a restart
    /// it follows the next run, whose underrun makes a dump of that run's blocks once the post-roll is over; a fault noted by
    /// the control side makes another; and at the stop the live run's ring comes back and the claim is released.
    #[test]
    fn it_follows_the_runs_and_dumps_what_went_wrong() {
        let dir = test_dir("flight-runs");
        let inner = Inner::new();
        let cfg = FlightConfig { facts_s: 1.0, post_roll_s: 0.2, min_interval_s: 0.0, ..FlightConfig::new(&dir) };
        let h = arm(&inner, cfg.clone()).unwrap();
        assert!(arm(&inner, cfg.clone()).is_err(), "one at a time");
        let bad = FlightConfig { facts_s: 0.0, ..cfg.clone() };
        assert!(check(&bad).is_err());
        // Run 1: no trigger.
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone(), u64::MAX);
        run_started(&inner, port);
        wait_until("run 1 recorded", || h.stats().blocks >= 50);
        run.stop.store(true, Release);
        thread.join().unwrap();
        run_stopped(&inner);
        // Run 2: an underrun at its block 100.
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone(), 100);
        run_started(&inner, port);
        wait_until("the underrun's dump", || !h.stats().dumps.is_empty());
        let d = h.stats().dumps[0].clone();
        assert_eq!(d.trigger, "an output underrun");
        let csv = std::fs::read_to_string(d.path.join("flight_blocks.csv")).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert!(rows.iter().all(|r| r.starts_with("2,")), "run 2's blocks: {}", rows[0]);
        assert!(rows.iter().any(|r| r.contains(",output-underrun|output-deficit,")));
        // A fault, as the control side notes it.
        note(&inner, Note::Fault("The microphone was unplugged.".into()));
        wait_until("the fault's dump", || h.stats().dumps.len() == 2);
        let stats = h.stop();
        assert_eq!(stats.dumps[1].trigger, "a fault that stopped the audio (The microphone was unplugged.)");
        assert!(stats.errors.is_empty() && stats.blocks >= 150, "{stats:?}");
        run.wait_handed_back();
        assert!(!inner.flying.load(Acquire) && lock(&inner.flight).link.is_none());
        // Stopped: nothing more is noted, and the next start gets no ring.
        note(&inner, Note::Stopped);
        let (mut port, _end) = run_channel(&inner);
        assert!(port.cmd.slots() == 1 && port.ret.pop().is_err(), "no ring");
        run.stop.store(true, Release);
        thread.join().unwrap();
        run_stopped(&inner);
        // Armed while a run is live, the run takes it at its next wake.
        let (port, end) = run_channel(&inner);
        let run = Arc::new(Run::default());
        let thread = fake_run(end, run.clone(), u64::MAX);
        run_started(&inner, port);
        let h = arm(&inner, cfg).unwrap();
        wait_until("armed while running", || h.stats().blocks >= 20);
        drop(h);
        run.wait_handed_back();
        run.stop.store(true, Release);
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    impl Run {
        fn wait_handed_back(&self) {
            wait_until("the run handed its ring back", || !self.holding.load(Acquire));
        }
    }
}
