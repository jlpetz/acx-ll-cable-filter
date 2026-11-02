//! Device health (TODO 21): how each stream delivers, from a short passive run (`noise-filter devices --check`). The
//! engine already measures it: the chain recorder's per-block facts (`ChainBlock`: the mic's wakes and its latency budget,
//! the reference's take, the output's events and clock readings) and its counters. [`measure`] turns them into figures,
//! [`assess`] the figures into a plain verdict per stream (steady, in bursts, late; drifting; glitched) with what to try, and
//! [`format`] prints them. All three are pure, so the verdicts are tested on figures and facts made up for them.
//!
//! The clocks' drift is fitted over the blocks (`nf_engine::clock_drift`: each clock's readings against the mic's), not read
//! from the drift loops: their estimates take a minute or more to settle, and bursty delivery throws them hundreds of ppm off
//! meanwhile, so a 15 s check would print the wrong drift.
//!
//! The thresholds are provisional. The only data so far is the dev VM's VB-CABLE (TODO 8's detail, "Delivery on
//! VB-CABLE"): 0, 480 or 960 frames a 10 ms wake, a mic block 10-21 ms old when read (16 on average), and a loopback that
//! swings 10-30 ms against the mic; steady delivery in the simulations reads a block 10.7 ms old. They are set between
//! those, and real hardware's figures are to set them properly.

use std::fmt::Write as _;

use nf_engine::{ChainBlock, DriftFit, EngineStatus, MicTiming, OutputEvents, ReferenceFacts, StreamInfo, TelemetrySnapshot};

use crate::commands::fixed;

/// A mic whose blocks come two or more to a wake this often (a share of its blocks) delivers in bursts. Steady delivery
/// gives a block a wake; the cables gave pairs often enough that about half their blocks came two to a wake.
pub const BUNCHED_SHARE: f32 = 0.05;
/// A mic whose delivery (a block's first sample's age when read) has its 95th percentile this far (ms) over its median
/// swings: steady delivery holds within about a millisecond; the cables' runs 10-21 ms. Without stamps, its wakes' gaps
/// this far over a block's 10 ms (their 95th percentile) do.
pub const SWING_MS: f32 = 3.0;
/// A mic whose delivery's mean runs this far (ms) past its packet's length is late: steady delivery is about 1 ms past; the
/// cables' about 6.
pub const LATE_MS: f32 = 3.0;
/// The loopback: a share of its blocks served while it hadn't delivered for two periods and 5 ms (stale), or short of
/// samples that hadn't come yet (owed: zeros served, the samples discarded when they come).
pub const REF_STALE_SHARE: f32 = 0.02;
pub const REF_OWED_SHARE: f32 = 0.01;
/// The loopback's lead target this far (ms) over what steady delivery needs: its delivery needed that much more (the cables'
/// rose from 12 to 13.5-23.4 ms with the shared mic). It's the whole rise over the target's start, in the start-up and after
/// it; with the exclusive mic up to a loopback period of it is that mic's own need, wherever it lands (doc/todo/10-aec.md:
/// the lead rose to 18.3 ms on the cables; a live check, 2026-11-02, called a rise of 12 to 18 bursts of 6.0 ms, which the
/// old rule gives only for a rise after the start-up: inferred, as its report wasn't kept).
pub const REF_RAISE_MS: f32 = 3.0;
/// The lead's readings scattering this far (ms RMS) from the loop's low-passed lead: the stamps, or the delivery that times it,
/// are noisy (the cables' stamps: within 1 ms).
pub const REF_SCATTER_MS: f32 = 1.0;
/// An output whose render side (from our buffer to its play-out, by its clock) runs this far (ms) past half its period holds
/// the audio long: Windows' audio engine and the cable held about 20 ms past it in shared mode; exclusive mode, 2.
pub const RENDER_HOLD_MS: f32 = 10.0;
/// Clocks this far apart (ppm, as printed: rounded) are unusual: crystals are within about 100 ppm of each other. The engine
/// corrects up to about 1500.
pub const DRIFT_PPM: f64 = 300.0;
/// A drift fit less precise than this (ppm, its 95% interval) gives no verdict: a mic timed by its delivery in a short check.
pub const DRIFT_PRECISION_PPM: f64 = 50.0;
/// The reference's start-up, from its alignment (or its last restart): 2.5 s in which its target follows the need up, and a
/// centring jump at its end (engine.md, "The reference"). Left out of its figures, with a few blocks to spare.
pub const REF_STARTUP_BLOCKS: usize = 250;
const REF_STARTUP_SPARE: usize = 10;
/// The run's first second (the output's starting fill, a first wake's backlog): left out of the mic's and the output's figures,
/// and of the drift fits.
pub const START_BLOCKS: usize = 100;
/// A block, in ms: the most often a mic delivers blocks.
const BLOCK_MS: f32 = nf_core::BLOCK as f32 * 1000.0 / nf_core::SAMPLE_RATE as f32;

/// The 5th, 50th and 95th percentiles, the largest and the mean of some per-block values, in ms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spread {
    pub p5: f32,
    pub median: f32,
    pub p95: f32,
    pub max: f32,
    pub mean: f32,
}

impl Spread {
    /// `None` without values.
    pub fn of(values: impl Iterator<Item = f32>) -> Option<Self> {
        let mut v: Vec<f32> = values.filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f32::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        let mean = (v.iter().map(|&x| f64::from(x)).sum::<f64>() / v.len() as f64) as f32;
        Some(Self { p5: at(0.05), median: at(0.5), p95: at(0.95), max: v[v.len() - 1], mean })
    }
}

/// How one stream was opened, for the report: its device, period and mode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stream {
    pub device: String,
    pub period_ms: f32,
    /// "exclusive 16-bit", "shared", ...
    pub mode: String,
    pub exclusive: bool,
    /// Exclusive mode was asked for and refused: why.
    pub refused: Option<String>,
}

impl Stream {
    fn of(s: &StreamInfo) -> Self {
        let mode = match s.exclusive {
            Some(format) => format!("exclusive {format}"),
            None if s.exclusive_refused.is_some() => "shared, exclusive refused".to_owned(),
            None => "shared".to_owned(),
        };
        Self {
            device: s.device_name.clone(),
            period_ms: s.period_ms,
            mode,
            exclusive: s.exclusive.is_some(),
            refused: s.exclusive_refused.clone(),
        }
    }

    /// The engine said how it opened the stream (its period, so the verdicts that need it can be given).
    fn known(&self) -> bool {
        self.period_ms > 0.0
    }

    /// A virtual cable (VB-Audio's): it has no hardware clock and moves audio on its own timer, so it delivers in bursts by
    /// design (TODO 8's detail).
    fn is_virtual_cable(&self) -> bool {
        let d = self.device.to_ascii_lowercase();
        d.contains("vb-audio") || d.starts_with("cable")
    }
}

/// The mic's figures over the check.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MicFigures {
    pub stream: Stream,
    pub blocks: usize,
    /// The wakes that delivered blocks.
    pub wakes: usize,
    /// The share of blocks that came two or more to a wake (the wake before had too little): a burst.
    pub bunched: f32,
    /// The gaps between the wakes that delivered blocks, in ms: their 95th percentile and the longest.
    pub wake_gap_p95_ms: f32,
    pub wake_gap_max_ms: f32,
    /// Mic delivery (the latency budget's: a block's first sample's capture to the wake that read it), per block; `None`
    /// without the mic's stamps (or the readout).
    pub delivery: Option<Spread>,
    /// Mic glitches (packets flagged discontinuous or missing by device position), stalls (100 ms with no packet), and samples its
    /// FIFO discarded.
    pub glitches: u64,
    pub stalls: u64,
    pub discarded: u64,
}

/// The echo reference's figures (the speakers' loopback), after its start-up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReferenceFigures {
    pub stream: Stream,
    pub blocks: usize,
    /// The share of blocks served while the loopback hadn't delivered within two periods (stale), and short of samples (owed).
    pub stale: f32,
    pub owed: f32,
    /// The lead's readings against the loop's low-passed lead, ms RMS.
    pub scatter_ms: f32,
    /// The stamps timed the lead at the end (else the delivery did).
    pub stamped: bool,
    /// The lead and its target at the end; the target it started at, and where its start-up left it; ms.
    pub lead_ms: f32,
    pub target_ms: f32,
    pub start_target_ms: f32,
    pub settled_target_ms: f32,
    /// Its clock against the mic's, fitted over the check (`None`: not measured, without stamps or too short a run).
    pub drift: Option<DriftFit>,
    /// Slips and jumps after the start-up (the jumps other than a raise of the target: a break undone, a step of the count, a
    /// centring), and the gaps (blocks served zeros owed) over the check.
    pub slips: u64,
    pub jumps: u64,
    pub gaps: u64,
}

/// The output's figures over the check.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputFigures {
    pub stream: Stream,
    pub events: OutputEvents,
    /// The latency budget's render side (our buffer to the play-out, by the device's clock) and output queue, per block.
    pub render: Option<Spread>,
    pub queue: Option<Spread>,
    pub target_ms: f32,
    /// Its clock against the mic's, fitted over the check (`None`: not measured).
    pub drift: Option<DriftFit>,
}

/// Everything the check measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Figures {
    pub seconds: f64,
    pub mic: MicFigures,
    pub output: OutputFigures,
    pub reference: Option<ReferenceFigures>,
    /// How the drift fits read the mic's clock (`None`: they couldn't).
    pub mic_timing: Option<MicTiming>,
    /// The filter restarted during the check (a fault or a device change): the figures cover the first run only.
    pub restarted: bool,
}

const MS_PER_SAMPLE: f32 = 1000.0 / nf_core::SAMPLE_RATE as f32;

/// Where the reference's start-up ends in its `facts` (one a block, in order): [`REF_STARTUP_BLOCKS`] and a few more from its
/// alignment, or from the start-up's last restart (a stream's stamps came or went); from the first block without either.
fn reference_settled_from(facts: &[ReferenceFacts]) -> usize {
    let anchor = facts.iter().rposition(|r| r.events & (ReferenceFacts::ALIGNED | ReferenceFacts::RESTARTED) != 0);
    anchor.map_or(REF_STARTUP_BLOCKS, |a| a + REF_STARTUP_BLOCKS + REF_STARTUP_SPARE).min(facts.len())
}

/// The check's figures from its chain recording's `blocks` (the run's, from its first block), the counters (`counters`: the
/// run's own, as its last publication had them), and the status (the streams, as the run opened them).
pub fn measure(blocks: &[ChainBlock], counters: &TelemetrySnapshot, status: &EngineStatus, seconds: f64, restarted: bool) -> Figures {
    let stream = |s: Option<&StreamInfo>| s.map(Stream::of).unwrap_or_default();
    let all = blocks;
    let blocks = blocks.get(START_BLOCKS.min(blocks.len())..).unwrap_or(&[]);
    // The mic's wakes: blocks that share one came in one burst.
    let mut wakes: Vec<(f64, usize)> = Vec::new();
    for b in blocks.iter().filter(|b| b.mic_wake_hns.is_finite()) {
        match wakes.last_mut() {
            Some((t, n)) if *t == b.mic_wake_hns => *n += 1,
            _ => wakes.push((b.mic_wake_hns, 1)),
        }
    }
    let counted: usize = wakes.iter().map(|w| w.1).sum();
    let bunched = wakes.iter().filter(|w| w.1 >= 2).map(|w| w.1).sum::<usize>();
    let gaps = Spread::of(wakes.windows(2).map(|w| ((w[1].0 - w[0].0) * 1e-4) as f32));
    let mic = MicFigures {
        stream: stream(status.mic.as_ref()),
        blocks: blocks.len(),
        wakes: wakes.len(),
        bunched: if counted > 0 { bunched as f32 / counted as f32 } else { 0.0 },
        wake_gap_p95_ms: gaps.map_or(f32::NAN, |g| g.p95),
        wake_gap_max_ms: gaps.map_or(f32::NAN, |g| g.max),
        delivery: Spread::of(blocks.iter().filter_map(|b| b.budget).map(|b| b.mic_ms)),
        glitches: counters.capture_glitches,
        stalls: counters.stalls,
        discarded: blocks.iter().map(|b| b.mic_discarded).sum(),
    };
    let drift = nf_engine::clock_drift(blocks);
    let output = OutputFigures {
        stream: stream(status.output.as_ref()),
        events: OutputEvents::of(counters),
        render: Spread::of(blocks.iter().filter_map(|b| b.budget).map(|b| b.render_ms)),
        queue: Spread::of(blocks.iter().filter_map(|b| b.budget).map(|b| b.queue_ms)),
        target_ms: blocks.last().map_or(counters.output_target_ms, |b| b.output.target as f32 * MS_PER_SAMPLE),
        drift: drift.output,
    };
    let reference = status.reference.as_ref().and_then(|info| {
        let facts: Vec<_> = all.iter().filter_map(|b| b.reference).collect();
        let start_target_ms = facts.first()?.target_ms;
        let last = *facts.last()?;
        let from = reference_settled_from(&facts);
        let settled = facts.get(from..).unwrap_or(&[]);
        let n = settled.len();
        let share = |f: &dyn Fn(&ReferenceFacts) -> bool| {
            if n > 0 { settled.iter().filter(|r| f(r)).count() as f32 / n as f32 } else { 0.0 }
        };
        let deviations: Vec<f64> = settled
            .iter()
            .filter(|r| r.lead_reading_ms.is_finite() && r.lead_ms.is_finite())
            .map(|r| f64::from(r.lead_reading_ms - r.lead_ms))
            .collect();
        let scatter_ms = if deviations.is_empty() {
            f32::NAN
        } else {
            (deviations.iter().map(|d| d * d).sum::<f64>() / deviations.len() as f64).sqrt() as f32
        };
        Some(ReferenceFigures {
            stream: Stream::of(info),
            blocks: n,
            stale: share(&|r| !r.fresh),
            owed: share(&|r| r.owed > 0),
            scatter_ms,
            stamped: last.stamped,
            lead_ms: last.lead_ms,
            target_ms: last.target_ms,
            start_target_ms,
            settled_target_ms: settled.first().map_or(last.target_ms, |r| r.target_ms),
            drift: drift.reference,
            slips: settled.iter().filter(|r| r.consumed != 480 && r.consumed != 0).count() as u64,
            // A raise of the target is a jump too, said with the raise (it's the need's); these are the timeline's others.
            jumps: settled
                .iter()
                .filter(|r| r.jumped != 0 && r.events & (ReferenceFacts::BROKEN | ReferenceFacts::STEPPED | ReferenceFacts::CENTRED) != 0)
                .count() as u64,
            gaps: counters.reference_gaps,
        })
    });
    Figures { seconds, mic, output, reference, mic_timing: drift.mic, restarted }
}

/// Which stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mic,
    Output,
    Reference,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Kind::Mic => "Microphone",
            Kind::Output => "Output",
            Kind::Reference => "Echo reference",
        }
    }
}

/// One stream's verdict, what it rests on, and what to try.
#[derive(Clone, Debug, PartialEq)]
pub struct Health {
    pub kind: Kind,
    pub stream: Stream,
    /// The mic delivered no audio after the run's first second.
    pub silent: bool,
    /// Delivers in bursts (the mic, the loopback), or is slow to play (the output: holds the audio long after taking it).
    pub bursty: bool,
    pub slow: bool,
    /// Late: steady, but past its packet (the mic), or stale at times (the loopback: no delivery for two periods and 5 ms).
    pub late: bool,
    pub drifting: bool,
    pub glitched: bool,
    /// Its delivery couldn't be timed (the mic without trusted stamps): only its wakes were judged.
    pub untimed: bool,
    /// What it costs here, in ms, when that can be said (the bursts' or the lateness' share of the latency).
    pub costs_ms: Option<f32>,
    pub findings: Vec<String>,
    pub tries: Vec<String>,
}

impl Health {
    fn new(kind: Kind, stream: &Stream) -> Self {
        Self {
            kind,
            stream: stream.clone(),
            silent: false,
            bursty: false,
            slow: false,
            late: false,
            drifting: false,
            glitched: false,
            untimed: false,
            costs_ms: None,
            findings: Vec::new(),
            tries: Vec::new(),
        }
    }

    /// "steady", "steady by its wakes (not timed)", or what's wrong: "delivered nothing", "in bursts", "late" (the loopback:
    /// "stale at times"), "slow to play", "drifting", "glitched", joined.
    pub fn verdict(&self) -> String {
        let mut v = Vec::new();
        if self.silent {
            v.push("delivered nothing");
        }
        if self.bursty {
            v.push("in bursts");
        }
        if self.late {
            v.push(if self.kind == Kind::Reference { "stale at times" } else { "late" });
        }
        if self.slow {
            v.push("slow to play");
        }
        if self.drifting {
            v.push("drifting");
        }
        if self.glitched {
            v.push("glitched");
        }
        match v.is_empty() {
            true if self.untimed => "steady by its wakes (not timed)".to_owned(),
            true => "steady".to_owned(),
            false => v.join(", "),
        }
    }

    /// Nothing wrong, and its delivery timed.
    pub fn steady(&self) -> bool {
        !(self.silent || self.bursty || self.slow || self.late || self.drifting || self.glitched || self.untimed)
    }

    fn try_(&mut self, what: &str) {
        if !self.tries.iter().any(|t| t == what) {
            self.tries.push(what.to_owned());
        }
    }
}

const TRY_USB: &str = "plug it into another USB port, straight into the PC rather than through a hub";
const TRY_DRIVER: &str = "install the maker's own driver instead of Windows' generic one";
const TRY_ENHANCEMENTS: &str = "turn its audio enhancements off (Settings > System > Sound > the device > Audio enhancements: Off)";
const TRY_LATENCYMON: &str =
    "check the PC's DPC latency with LatencyMon (resplendence.com): a driver that holds the CPU delays every device";
const TRY_CABLE: &str = "this is a virtual cable: it has no clock of its own and moves audio on its own timer, so it delivers \
                         in bursts by design; a device with its own clock needn't";
const TRY_PRIVACY: &str = "check that Windows lets desktop apps use the microphone (Settings > Privacy & security > Microphone > Let \
                           desktop apps access your microphone)";
const TRY_REPLUG: &str = "check that it isn't muted or disabled (Settings > System > Sound), or unplug it and plug it back in";

fn ms(v: f32) -> String {
    fixed(f64::from(v), 1)
}

fn percent(share: f32) -> String {
    format!("{}%", fixed(f64::from(share * 100.0), 0))
}

fn mic_health(m: &MicFigures) -> Health {
    let mut h = Health::new(Kind::Mic, &m.stream);
    let period = m.stream.period_ms;
    if m.blocks == 0 || m.wakes == 0 {
        h.silent = true;
        h.findings.push("It delivered no audio after the check's first second.".to_owned());
    } else {
        match m.delivery {
            Some(d) => {
                let swing = d.p95 - d.median;
                h.bursty = m.bunched >= BUNCHED_SHARE || swing >= SWING_MS;
                // Late against its packet: only with the packet's length known.
                h.late = !h.bursty && m.stream.known() && d.mean - period >= LATE_MS;
                if h.bursty {
                    // What the bursts add on average, over the block's steadiest delivery.
                    h.costs_ms = Some((d.mean - d.p5).max(0.0));
                    h.findings.push(format!(
                        "Delivers in bursts: {} of its blocks came two or more to a wake (wakes up to {} ms apart; a block is {} \
                         ms), and a block's first sample was {}-{} ms old when read (the 5th to 95th percentile, the oldest {}), \
                         which adds about {} ms to the latency here.",
                        percent(m.bunched),
                        ms(m.wake_gap_max_ms),
                        ms(BLOCK_MS),
                        ms(d.p5),
                        ms(d.p95),
                        ms(d.max),
                        ms(d.mean - d.p5)
                    ));
                } else if h.late {
                    h.costs_ms = Some(d.mean - period);
                    h.findings.push(format!(
                        "Delivers steadily but late: a block's first sample was {} ms old when read on average, {} ms past its {} \
                         ms packet.",
                        ms(d.mean),
                        ms(d.mean - period),
                        ms(period)
                    ));
                } else {
                    h.findings.push(format!(
                        "Delivers steadily: a block's first sample was {}-{} ms old when read (the 5th to 95th percentile), {} of \
                         its blocks two to a wake.",
                        ms(d.p5),
                        ms(d.p95),
                        percent(m.bunched)
                    ));
                }
            }
            None => {
                // No stamps: the wakes alone say whether it bunches, not how late it is.
                h.untimed = true;
                let swing = m.wake_gap_p95_ms - period.max(BLOCK_MS);
                h.bursty = m.bunched >= BUNCHED_SHARE || swing >= SWING_MS;
                let wakes = format!(
                    "{} of its blocks came two or more to a wake, and 95% of its wakes came within {} ms of the one before (the \
                     longest gap {} ms; a block is {} ms)",
                    percent(m.bunched),
                    ms(m.wake_gap_p95_ms),
                    ms(m.wake_gap_max_ms),
                    ms(BLOCK_MS)
                );
                h.findings.push(if h.bursty {
                    format!("Delivers in bursts by its wakes: {wakes}.")
                } else {
                    format!("By its wakes it delivers steadily: {wakes}.")
                });
                h.findings.push(
                    "Its packets carry no timestamps the filter trusts, so how late it delivers can't be timed (nor the latency \
                     readout, nor the reference's lead by its stamps)."
                        .to_owned(),
                );
            }
        }
    }
    if !m.stream.known() {
        h.findings.push("The engine didn't say how it opened the stream (its period), so its lateness wasn't judged.".to_owned());
    }
    h.glitched = m.glitches > 0 || m.stalls > 0;
    if h.glitched {
        h.findings.push(format!(
            "Lost audio: {} mic glitches (packets missing or flagged discontinuous), {} stalls (100 ms with no packet){}.",
            m.glitches,
            m.stalls,
            if m.discarded > 0 { format!(", {} samples dropped from a full buffer", m.discarded) } else { String::new() }
        ));
    }
    if let Some(why) = &m.stream.refused {
        h.findings.push(format!("Exclusive mode was refused ({why}), so it ran shared."));
    }
    let cable = m.stream.is_virtual_cable();
    if h.silent {
        h.try_(TRY_PRIVACY);
        h.try_(TRY_REPLUG);
    }
    if h.bursty || h.late {
        // A virtual cable has no port, driver choice or enhancements to change.
        let tries: &[&str] = if cable { &[TRY_CABLE, TRY_LATENCYMON] } else { &[TRY_USB, TRY_DRIVER, TRY_ENHANCEMENTS, TRY_LATENCYMON] };
        for t in tries {
            h.try_(t);
        }
        if !m.stream.exclusive && m.stream.refused.is_none() {
            h.try_("the exclusive mic (on by default; --exclusive-mic turns it back on) reads the driver directly at its own period");
        }
    }
    if h.untimed && !cable {
        h.try_(TRY_DRIVER);
    }
    if h.glitched {
        if m.stream.exclusive {
            h.try_(&format!(
                "in exclusive mode its buffer is one period ({} ms): a filter thread held up longer loses audio, so try \
                 --no-exclusive-mic and see whether the glitches stop",
                ms(period)
            ));
        }
        h.try_(TRY_LATENCYMON);
    }
    h
}

fn output_health(o: &OutputFigures) -> Health {
    let mut h = Health::new(Kind::Output, &o.stream);
    let e = &o.events;
    h.glitched = e.underruns > 0 || e.coarse_drops > 0;
    let what = e.text();
    h.findings.push(if h.glitched { format!("Ran short or overfilled: {what}.") } else { format!("Took every block in time: {what}.") });
    if let Some(r) = o.render {
        let queue = o
            .queue
            .map_or(String::new(), |q| format!("; the queue before it {} ms on average (target {} ms)", ms(q.mean), ms(o.target_ms)));
        h.findings.push(format!(
            "It plays the audio {} ms after taking it from the filter's buffer on average (the render side, by its clock){queue}.",
            ms(r.mean)
        ));
        // How long it holds the audio past half its period: only with the period known.
        if o.stream.known() {
            let hold = r.mean - o.stream.period_ms / 2.0;
            h.slow = hold >= RENDER_HOLD_MS;
            if h.slow {
                h.costs_ms = Some(hold);
            }
        }
    }
    if !o.stream.known() {
        h.findings.push("The engine didn't say how it opened the stream (its period), so its render side wasn't judged.".to_owned());
    }
    if let Some(why) = &o.stream.refused {
        h.findings.push(format!("Exclusive mode was refused ({why}), so it ran shared."));
    }
    if h.slow && !o.stream.exclusive {
        h.try_(
            "exclusive output (on by default; --exclusive-output turns it back on) plays into the device's driver directly, \
             skipping Windows' audio engine (20-30 ms less on VB-CABLE)",
        );
        if !o.stream.is_virtual_cable() {
            h.try_(TRY_DRIVER);
        }
    }
    if h.glitched {
        h.try_(TRY_LATENCYMON);
        h.try_(
            "with the adaptive output target (on by default) a rare hiccup can run it dry once the target has come down: \
             --no-adaptive-output keeps the spare block",
        );
        if o.stream.exclusive {
            h.try_("--no-exclusive-output, to see whether it glitches only in exclusive mode");
        }
    }
    h
}

/// The loopback; `mic_exclusive`: the mic ran in exclusive mode, whose blocks need up to a loopback period more lead.
fn reference_health(r: &ReferenceFigures, mic_exclusive: bool) -> Health {
    let mut h = Health::new(Kind::Reference, &r.stream);
    let settled = r.blocks > 0;
    h.late = settled && r.stale >= REF_STALE_SHARE;
    // The target's rise over its start: in the start-up (it follows the need up) and after it. With the exclusive mic up to a
    // loopback period of it is that mic's own need, wherever it lands: where the mic's blocks fall against the loopback's
    // packets sets it, and a need that first shows near the start-up's end, or after it, raises the target only once the
    // takes have come up short for 0.6 s (reference.rs), past the start-up. So the allowance is the whole rise's, not the
    // start-up's alone (a live check called a rise of 12 to 18 ms bursts of 6.0 ms, which the start-up-only allowance gives
    // only for a rise after it: inferred, doc/todo/21-device-health.md).
    let startup_raise = r.settled_target_ms - r.start_target_ms;
    let raised = r.target_ms - r.settled_target_ms;
    let rise = startup_raise.max(0.0) + raised.max(0.0);
    let own_need = if mic_exclusive { if r.stream.known() { r.stream.period_ms } else { 10.0 } } else { 0.0 };
    let excess = rise - own_need;
    let raised_for_bursts = excess >= REF_RAISE_MS;
    h.bursty = (settled && r.owed >= REF_OWED_SHARE) || raised_for_bursts;
    if settled {
        h.findings.push(format!(
            "After its start-up the loopback was delivering at {} of the blocks (its last packet no more than two periods and 5 \
             ms old), and {} of them were short of samples (zeros served for ones still to come); at the end its lead was {} ms, \
             against a target of {} ms.",
            percent(1.0 - r.stale),
            percent(r.owed),
            ms(r.lead_ms),
            ms(r.target_ms)
        ));
    } else {
        h.findings.push(format!(
            "Too few blocks after its start-up to judge how it delivers (a longer check would say); its lead {} ms, against a \
             target of {} ms.",
            ms(r.lead_ms),
            ms(r.target_ms)
        ));
    }
    if startup_raise >= 0.5 {
        h.findings.push(format!(
            "In its start-up its lead target rose from {} to {} ms, to what its delivery needed.",
            ms(r.start_target_ms),
            ms(r.settled_target_ms)
        ));
    }
    if raised >= 0.5 {
        h.findings.push(format!(
            "After its start-up its target rose {} ms more, to {} ms: its takes kept coming up short (each raise is a jump of the \
             echo path, which the canceller takes 5-10 s to follow).",
            ms(raised),
            ms(r.target_ms)
        ));
    }
    if mic_exclusive && rise >= 0.5 {
        // Of the rise alone: the verdict may be bursts or staleness for its own reasons (the shares above, the stamps below).
        h.findings.push(format!(
            "With the exclusive mic up to a loopback period ({} ms) of that rise is expected, in the start-up or after it: each \
             mic block is processed before the loopback, on its own period, has delivered as recent a reference{}.",
            ms(own_need),
            if raised_for_bursts { "" } else { "; so the rise itself is no sign of bursts" }
        ));
    }
    if raised_for_bursts {
        h.costs_ms = Some(excess);
        h.findings.push(format!(
            "Its delivery needed about {} ms more lead than steady delivery does: the echo has to reach the mic that much later \
             after the speakers' mix for the canceller to catch it.",
            ms(excess)
        ));
    }
    if r.stamped {
        if r.scatter_ms.is_finite() {
            h.findings.push(format!(
                "Its timestamps time the lead; the readings scatter {} ms RMS about the loop's lead{}.",
                ms(r.scatter_ms),
                if r.scatter_ms >= REF_SCATTER_MS { " (noisy)" } else { "" }
            ));
        }
    } else {
        h.bursty = true;
        h.findings.push(
            "Its timestamps (or the mic's) aren't trusted, so the lead is timed by when the packets came, which swings with \
             bursts and follows the drift only in steps."
                .to_owned(),
        );
    }
    if r.jumps > 0 {
        h.findings.push(format!(
            "After its start-up its timeline jumped {} {} (each jump moves the echo path, and the canceller takes 5-10 s to \
             follow).",
            r.jumps,
            if r.jumps == 1 { "time" } else { "times" }
        ));
    }
    if r.slips > 0 {
        // The drift loop's one-sample slips are how it should follow the clocks: said only so they aren't taken for jumps.
        h.findings.push(format!(
            "It followed the clocks' drift in {} one-sample slips after its start-up, as it should: they hold the echo path \
             where it is.",
            r.slips
        ));
    }
    h.glitched = r.jumps > 0;
    if h.bursty || h.late || h.glitched {
        let tries: &[&str] =
            if r.stream.is_virtual_cable() { &[TRY_CABLE, TRY_LATENCYMON] } else { &[TRY_DRIVER, TRY_ENHANCEMENTS, TRY_LATENCYMON] };
        for t in tries {
            h.try_(t);
        }
    }
    h
}

/// A clock's drift finding, and whether it drifts: by the fit's point estimate as printed (rounded), when the fit is precise
/// enough to say.
fn drift_finding(fit: Option<DriftFit>, timing: Option<MicTiming>, kind: Kind) -> (String, bool) {
    let by = if timing == Some(MicTiming::Delivery) { ", the mic timed by its delivery" } else { "" };
    match fit {
        Some(f) if f.uncertainty_ppm <= DRIFT_PRECISION_PPM => {
            let (ppm, within) = (f.ppm.round(), f.uncertainty_ppm.round().max(1.0));
            let how = if ppm == 0.0 {
                format!("Its clock keeps time with the mic's (within {within:.0} ppm")
            } else {
                format!("Its clock runs {:.0} ppm {} against the mic's (±{within:.0}", ppm.abs(), if ppm > 0.0 { "fast" } else { "slow" })
            };
            (format!("{how}, fitted over {:.0} s{by}; the filter corrects it).", f.seconds), ppm.abs() >= DRIFT_PPM)
        }
        Some(f) => (
            format!(
                "Its drift against the mic's couldn't be measured closer than ±{:.0} ppm in {:.0} s{by}, so it isn't judged.",
                f.uncertainty_ppm, f.seconds
            ),
            false,
        ),
        None => {
            let why = match (kind, timing) {
                (_, None) => "the mic's clock couldn't be read (too short a run, or no packets)",
                (Kind::Reference, Some(MicTiming::Delivery)) => "without the mic's timestamps the loopback's can't be compared",
                (Kind::Reference, _) => "its timestamps aren't trusted, or too few readings",
                _ => "too few clock readings",
            };
            (format!("Its drift against the mic's wasn't measured: {why}."), false)
        }
    }
}

/// The verdicts: the mic, the output, and the echo reference when there was one; and the drift on each clock.
pub fn assess(f: &Figures) -> Vec<Health> {
    let mut all = vec![mic_health(&f.mic), output_health(&f.output)];
    if let Some(r) = &f.reference {
        all.push(reference_health(r, f.mic.stream.exclusive));
    }
    // The drift fits measure each clock against the mic's.
    let drifts = [(Kind::Output, f.output.drift), (Kind::Reference, f.reference.as_ref().and_then(|r| r.drift))];
    for (kind, fit) in drifts {
        let Some(h) = all.iter_mut().find(|h| h.kind == kind) else { continue };
        let (finding, drifting) = drift_finding(fit, f.mic_timing, kind);
        h.findings.push(finding);
        if drifting {
            h.drifting = true;
            h.try_(
                "a clock that far off is unusual: a USB device on a busy hub, a Bluetooth device, or a virtual one; the filter \
                 corrects up to about 1500 ppm",
            );
        }
    }
    all
}

/// The report.
pub fn format(f: &Figures, all: &[Health]) -> String {
    let mut s = format!(
        "Device check over {} s (the thresholds are provisional: they come from virtual cables, and real hardware's figures are \
         to set them)",
        fixed(f.seconds, 0)
    );
    if f.restarted {
        s.push_str(
            "\nwarning: the filter restarted during the check (a fault or a device change): the figures cover its first run, \
             its counters up to that run's last whole second.",
        );
    }
    for h in all {
        let what = if h.stream.known() {
            format!("{}, {}, period {} ms", h.stream.device, h.stream.mode, ms(h.stream.period_ms))
        } else if h.stream.device.is_empty() {
            "(the engine didn't say which device)".to_owned()
        } else {
            h.stream.device.clone()
        };
        let _ = write!(s, "\n\n{}: {what}: {}", h.kind.title(), h.verdict());
        for line in &h.findings {
            let _ = write!(s, "\n  {line}");
        }
        if !h.tries.is_empty() {
            let _ = write!(s, "\n  What to try:");
            for t in &h.tries {
                let _ = write!(s, "\n  - {t}");
            }
        }
    }
    let unsteady: Vec<String> = all
        .iter()
        .filter(|h| !h.steady())
        .map(|h| {
            let cost = h.costs_ms.map_or(String::new(), |c| format!(", about {} ms", ms(c)));
            format!("the {} is {}{cost}", h.kind.title().to_lowercase(), h.verdict())
        })
        .collect();
    let overall = if unsteady.is_empty() { "every stream is steady".to_owned() } else { unsteady.join("; ") };
    let _ = write!(s, "\n\nOverall: {overall}.");
    s
}

#[cfg(test)]
mod tests {
    use nf_engine::{ChainParams, LatencyBudget, OutputFacts};

    use super::*;

    fn stream(device: &str, period_ms: f32, exclusive: bool) -> Stream {
        Stream {
            device: device.into(),
            period_ms,
            mode: if exclusive { "exclusive 16-bit".into() } else { "shared".into() },
            exclusive,
            refused: None,
        }
    }

    fn spread(p5: f32, median: f32, p95: f32, max: f32, mean: f32) -> Option<Spread> {
        Some(Spread { p5, median, p95, max, mean })
    }

    fn fit(ppm: f64, uncertainty_ppm: f64) -> Option<DriftFit> {
        Some(DriftFit { ppm, uncertainty_ppm, seconds: 14.0 })
    }

    /// Steady figures: a block a wake, 10.4-11.0 ms old, an output that took everything in time, a loopback that kept up.
    fn steady() -> Figures {
        Figures {
            seconds: 15.0,
            mic: MicFigures {
                stream: stream("Microphone (USB)", 10.0, false),
                blocks: 1500,
                wakes: 1500,
                bunched: 0.0,
                wake_gap_p95_ms: 10.1,
                wake_gap_max_ms: 10.4,
                delivery: spread(10.4, 10.7, 11.0, 11.3, 10.7),
                ..Default::default()
            },
            output: OutputFigures {
                stream: stream("CABLE Input", 2.0, true),
                events: OutputEvents { slips: 12, slip_net: -3, ..Default::default() },
                render: spread(2.5, 3.0, 3.4, 3.6, 3.0),
                queue: spread(12.0, 14.0, 15.5, 16.0, 14.0),
                target_ms: 14.0,
                drift: fit(12.2, 0.4),
            },
            reference: Some(ReferenceFigures {
                stream: stream("Speakers (Realtek)", 10.0, false),
                blocks: 1250,
                stale: 0.0,
                owed: 0.0,
                scatter_ms: 0.1,
                stamped: true,
                lead_ms: 12.0,
                target_ms: 12.0,
                start_target_ms: 12.0,
                settled_target_ms: 12.0,
                drift: fit(-4.0, 0.6),
                ..Default::default()
            }),
            mic_timing: Some(MicTiming::Stamps),
            restarted: false,
        }
    }

    #[test]
    fn steady_streams_read_as_steady() {
        let f = steady();
        let all = assess(&f);
        assert_eq!(all.iter().map(Health::verdict).collect::<Vec<_>>(), ["steady", "steady", "steady"]);
        assert!(all.iter().all(|h| h.tries.is_empty()), "{all:?}");
        let text = format(&f, &all);
        assert!(text.starts_with("Device check over 15 s (the thresholds are provisional"), "{text}");
        assert!(
            text.contains(
                "\n\nMicrophone: Microphone (USB), shared, period 10.0 ms: steady\n  Delivers steadily: a block's first sample was 10.4-11.0 ms"
            ),
            "{text}"
        );
        assert!(
            text.contains("Output: CABLE Input, exclusive 16-bit, period 2.0 ms: steady\n  Took every block in time: no underruns"),
            "{text}"
        );
        assert!(text.contains("Its clock runs 12 ppm fast against the mic's (±1, fitted over 14 s; the filter corrects it)."), "{text}");
        assert!(text.contains("Its clock runs 4 ppm slow against the mic's"), "{text}");
        assert!(text.contains("After its start-up the loopback was delivering at 100% of the blocks (its last packet no more"), "{text}");
        assert!(text.ends_with("Overall: every stream is steady."), "{text}");
    }

    /// The cables' mic (TODO 8): half its blocks two to a wake, 10-21 ms old: in bursts, costing about 6 ms, with the cable's
    /// own explanation first among what to try; a shared one is told about the exclusive mic too.
    #[test]
    fn the_cables_bursts_read_as_bursts() {
        let mut f = steady();
        f.mic.stream = stream("CABLE-A Output (VB-Audio Cable A)", 10.0, false);
        f.mic.bunched = 0.5;
        f.mic.wake_gap_max_ms = 30.0;
        f.mic.delivery = spread(10.4, 15.0, 20.9, 21.2, 16.4);
        let all = assess(&f);
        let mic = &all[0];
        assert!(mic.bursty && !mic.late && !mic.glitched, "{mic:?}");
        assert_eq!(mic.verdict(), "in bursts");
        assert!((mic.costs_ms.unwrap() - 6.0).abs() < 1e-4);
        assert!(mic.findings[0].starts_with("Delivers in bursts: 50% of its blocks came two or more to a wake (wakes up to 30.0 ms"));
        assert!(mic.tries[0].starts_with("this is a virtual cable") && mic.tries.iter().any(|t| t.contains("LatencyMon")), "{mic:?}");
        assert!(!mic.tries[0].contains("TODO"), "no internal numbers in what users read");
        assert!(mic.tries.iter().any(|t| t.contains("exclusive mic")), "{mic:?}");
        assert!(!mic.tries.iter().any(|t| t.contains("USB port")), "a cable has no port: {mic:?}");
        assert!(format(&f, &all).ends_with("Overall: the microphone is in bursts, about 6.0 ms."));
        // A swing alone is a burst too; a steady but late one is late.
        f.mic.bunched = 0.0;
        assert!(assess(&f)[0].bursty);
        f.mic.delivery = spread(15.8, 16.0, 16.4, 16.6, 16.0);
        f.mic.stream = stream("Microphone (USB)", 10.0, false);
        let mic = &assess(&f)[0];
        assert!(!mic.bursty && mic.late && (mic.costs_ms.unwrap() - 6.0).abs() < 1e-4, "{mic:?}");
        assert!(mic.tries[0].contains("USB port") && mic.tries.iter().any(|t| t.contains("audio enhancements")), "{mic:?}");
        assert!(mic.findings[0].starts_with("Delivers steadily but late: a block's first sample was 16.0 ms old"), "{mic:?}");
        // An exclusive mic isn't late for delivering inside its packet.
        f.mic.stream = stream("Mic", 3.33, true);
        f.mic.delivery = spread(0.6, 0.8, 1.0, 1.2, 0.8);
        assert!(assess(&f)[0].steady());
    }

    /// A mic without trusted stamps is judged by its wakes: bunched blocks or long gaps between wakes are bursts, and
    /// otherwise it's steady by its wakes but not timed, which the overall line says rather than "every stream is steady". A
    /// mic that delivered nothing says so first, and what to check.
    #[test]
    fn an_untimed_or_silent_mic_isnt_called_steady() {
        let mut f = steady();
        f.mic.delivery = None;
        f.mic.bunched = 0.5;
        f.mic.wake_gap_max_ms = 30.0;
        f.mic.wake_gap_p95_ms = 20.0;
        let all = assess(&f);
        let mic = &all[0];
        assert!(mic.bursty && mic.untimed && mic.costs_ms.is_none(), "{mic:?}");
        assert_eq!(mic.verdict(), "in bursts");
        assert!(mic.findings[0].starts_with("Delivers in bursts by its wakes: 50% of its blocks came two or more to a wake"), "{mic:?}");
        assert!(mic.findings[1].starts_with("Its packets carry no timestamps the filter trusts"), "{mic:?}");
        assert!(format(&f, &all).ends_with("Overall: the microphone is in bursts."));
        // Long gaps alone: bursts too.
        f.mic.bunched = 0.0;
        assert!(assess(&f)[0].bursty);
        // A wake a block, steadily: steady by its wakes, but not timed.
        f.mic.wake_gap_p95_ms = 10.4;
        let all = assess(&f);
        let mic = &all[0];
        assert!(!mic.bursty && !mic.steady() && mic.untimed, "{mic:?}");
        assert_eq!(mic.verdict(), "steady by its wakes (not timed)");
        assert!(mic.findings[0].starts_with("By its wakes it delivers steadily: 0% of its blocks"), "{mic:?}");
        assert!(mic.tries.iter().any(|t| t.contains("maker's own driver")), "{mic:?}");
        assert!(format(&f, &all).ends_with("Overall: the microphone is steady by its wakes (not timed)."));
        // Nothing delivered after the first second, three stalls: it says so, and not that it lacks stamps.
        f.mic = MicFigures { stream: stream("Microphone (USB)", 10.0, false), stalls: 3, ..Default::default() };
        let mic = &assess(&f)[0];
        assert_eq!(mic.verdict(), "delivered nothing, glitched");
        assert_eq!(mic.findings[0], "It delivered no audio after the check's first second.");
        assert!(!mic.findings.iter().any(|l| l.contains("timestamps")), "{mic:?}");
        assert!(mic.tries[0].contains("Privacy & security > Microphone"), "{mic:?}");
    }

    /// Glitches: the exclusive mic is told to try shared mode; an output that ran dry, about LatencyMon and the adaptive
    /// target; refusals say why; a far-off clock drifts.
    #[test]
    fn glitches_refusals_and_drift_say_what_to_try() {
        let mut f = steady();
        f.mic.stream = stream("Mic", 3.33, true);
        f.mic.glitches = 2;
        f.output.events.underruns = 1;
        f.output.events.deficits = 1;
        f.output.events.zero_frames = 480;
        f.output.drift = fit(-450.0, 1.0);
        let all = assess(&f);
        assert!(all[0].glitched && all[0].tries.iter().any(|t| t.contains("--no-exclusive-mic")), "{:?}", all[0]);
        let out = &all[1];
        assert!(out.glitched && out.drifting, "{out:?}");
        assert_eq!(out.verdict(), "drifting, glitched");
        assert!(out.findings[0].starts_with("Ran short or overfilled: 1 underrun, 1 deficit (10.0 ms of zeros)"), "{out:?}");
        assert!(out.findings.iter().any(|l| l.starts_with("Its clock runs 450 ppm slow against the mic's")), "{out:?}");
        assert!(
            out.tries.iter().any(|t| t.contains("--no-adaptive-output")) && out.tries.iter().any(|t| t.contains("--no-exclusive-output"))
        );
        // A slow shared output, its exclusive mode refused.
        let mut f = steady();
        f.output.stream = Stream { refused: Some("another app is using it".into()), ..stream("Speakers", 10.0, false) };
        f.output.render = spread(24.0, 25.7, 26.0, 26.1, 25.7);
        let out = &assess(&f)[1];
        assert!(out.slow && (out.costs_ms.unwrap() - 20.7).abs() < 1e-4, "{out:?}");
        assert!(out.findings.iter().any(|l| l == "Exclusive mode was refused (another app is using it), so it ran shared."));
        assert!(out.tries.iter().any(|t| t.starts_with("exclusive output")), "{out:?}");
    }

    /// The drift verdict is the printed figure's: 299.6 prints as 300 and drifts, 299.4 as 299 and doesn't; a fit too
    /// imprecise to say (a mic timed by its delivery) gives no verdict, and none at all says why.
    #[test]
    fn the_drift_is_judged_as_printed() {
        let mut f = steady();
        for (ppm, drifting, text) in [(299.6, true, "runs 300 ppm fast"), (299.4, false, "runs 299 ppm fast"), (0.3, false, "keeps time")] {
            f.output.drift = fit(ppm, 2.0);
            let out = &assess(&f)[1];
            assert_eq!(out.drifting, drifting, "{ppm}: {out:?}");
            assert!(out.findings.iter().any(|l| l.contains(text)), "{ppm}: {out:?}");
        }
        f.output.drift = fit(400.0, 80.0);
        f.mic_timing = Some(MicTiming::Delivery);
        let out = &assess(&f)[1];
        assert!(!out.drifting, "{out:?}");
        assert!(
            out.findings.iter().any(|l| l
                == "Its drift against the mic's couldn't be measured closer than ±80 ppm in 14 s, the mic timed by its delivery, so \
                    it isn't judged."),
            "{out:?}"
        );
        f.output.drift = None;
        f.mic_timing = None;
        let out = &assess(&f)[1];
        assert!(out.findings.iter().any(|l| l.starts_with("Its drift against the mic's wasn't measured: the mic's clock")), "{out:?}");
    }

    /// The loopback: stale takes are late, owed ones and a raised target bursts, untrusted stamps too; jumps after the
    /// start-up count as glitches.
    #[test]
    fn the_loopback_reads_as_late_bursty_or_jumping() {
        let mut f = steady();
        let r = f.reference.as_mut().unwrap();
        r.stale = 0.05;
        r.owed = 0.02;
        r.settled_target_ms = 14.0;
        r.target_ms = 23.4;
        r.jumps = 1;
        let h = &assess(&f)[2];
        assert!(h.late && h.bursty && h.glitched, "{h:?}");
        assert_eq!(h.verdict(), "in bursts, stale at times, glitched");
        // The shared mic needs no raise: 2 ms in the start-up and 9.4 after it.
        assert!((h.costs_ms.unwrap() - 11.4).abs() < 1e-4, "{h:?}");
        assert!(
            h.findings[0]
                .starts_with("After its start-up the loopback was delivering at 95% of the blocks (its last packet no more than two"),
            "{h:?}"
        );
        assert!(h.findings.iter().any(|l| l.starts_with("After its start-up its target rose 9.4 ms more, to 23.4 ms")), "{h:?}");
        assert!(h.findings[0].contains("and 2% of them were short of samples"), "{h:?}");
        let mut f = steady();
        f.reference.as_mut().unwrap().stamped = false;
        assert!(assess(&f)[2].bursty);
        // Nothing after the start-up: too few blocks, not "delivered at 100%".
        let mut f = steady();
        f.reference.as_mut().unwrap().blocks = 0;
        let h = &assess(&f)[2];
        assert!(h.steady() && h.findings[0].starts_with("Too few blocks after its start-up"), "{h:?}");
        f.reference = None;
        assert_eq!(assess(&f).len(), 2, "no reference without echo cancelling");
    }

    /// With the exclusive mic the loopback's target rises up to a period by design (12 to 18 ms on the cables): that is
    /// steady, and said, whether the rise comes in the start-up or after it (the live check of 2026-11-02 called a 12 to 18
    /// rise bursts "needing 6.0 ms more lead", which the old rule gives only for all of it after the start-up), or some of
    /// each; with a shared mic the same rise is the delivery's need, a burst. A rise past the exclusive mic's period is a
    /// burst too, by what's past it. The rise's own clause speaks of the rise alone: a stream bursty or stale for another
    /// reason isn't called steady by it.
    #[test]
    fn the_exclusive_mics_own_raise_is_steady_wherever_it_lands() {
        let exclusive = |settled: f32, end: f32| {
            let mut f = steady();
            f.mic.stream = stream("CABLE-A Output (VB-Audio Virtual Cable A)", 3.33, true);
            f.mic.delivery = spread(0.6, 0.8, 1.0, 1.2, 0.8);
            let r = f.reference.as_mut().unwrap();
            r.stream = stream("CABLE-B Input (VB-Audio Virtual Cable B)", 10.0, false);
            (r.settled_target_ms, r.target_ms, r.lead_ms) = (settled, end, end);
            f
        };
        // The live check's figures: 12 at the start and at the start-up's end, 18 at the end.
        for (settled, end) in [(12.0, 18.0), (18.0, 18.0), (14.0, 18.0)] {
            let f = exclusive(settled, end);
            let all = assess(&f);
            let h = &all[2];
            assert_eq!(h.verdict(), "steady", "{settled} then {end}: {h:?}");
            assert!(h.costs_ms.is_none() && h.tries.is_empty(), "{settled} then {end}: {h:?}");
            assert!(!h.findings.iter().any(|l| l.contains("more lead than steady delivery")), "{settled} then {end}: {h:?}");
            assert!(
                h.findings.iter().any(|l| l.starts_with(
                    "With the exclusive mic up to a loopback period (10.0 ms) of that rise is \
                                                         expected, in the start-up or after it"
                ) && l.ends_with("; so the rise itself is no sign of bursts.")),
                "{settled} then {end}: {h:?}"
            );
            assert!(format(&f, &all).ends_with("Overall: every stream is steady."), "{settled} then {end}");
        }
        // Bursty or stale for another reason (2% owed, 5% stale), the rise's clause speaks of the rise alone and says
        // nothing of the stream as steady.
        for (owed, stale, verdict) in [(0.02, 0.0, "in bursts"), (0.0, 0.05, "stale at times")] {
            let mut f = exclusive(12.0, 18.0);
            let r = f.reference.as_mut().unwrap();
            (r.owed, r.stale) = (owed, stale);
            let h = &assess(&f)[2];
            assert_eq!(h.verdict(), verdict, "{h:?}");
            assert!(h.costs_ms.is_none() && !h.findings.iter().any(|l| l.contains("steady")), "{h:?}");
            assert!(h.findings.iter().any(|l| l.ends_with("; so the rise itself is no sign of bursts.")), "{h:?}");
        }
        let h = &assess(&exclusive(12.0, 18.0))[2];
        assert!(
            h.findings.iter().any(|l| l
                == "After its start-up its target rose 6.0 ms more, to 18.0 ms: its takes kept coming up \
                                            short (each raise is a jump of the echo path, which the canceller takes 5-10 s to \
                                            follow)."),
            "{h:?}"
        );
        assert!(!h.findings.iter().any(|l| l.starts_with("In its start-up")), "no rise in the start-up: {h:?}");
        let h = &assess(&exclusive(18.0, 18.0))[2];
        assert!(h.findings.iter().any(|l| l == "In its start-up its lead target rose from 12.0 to 18.0 ms, to what its delivery needed."));
        // The shared mic: the same rise is a burst, 6 ms, in the start-up or after it.
        for (settled, end) in [(12.0, 18.0), (18.0, 18.0)] {
            let mut f = exclusive(settled, end);
            f.mic = steady().mic;
            let h = &assess(&f)[2];
            assert!(h.bursty && (h.costs_ms.unwrap() - 6.0).abs() < 1e-4, "{h:?}");
            assert!(!h.findings.iter().any(|l| l.starts_with("With the exclusive mic")), "{h:?}");
        }
        // The exclusive mic, risen 15 ms (some in the start-up, the rest after): 5 ms past its own need.
        let h = &assess(&exclusive(20.0, 27.0))[2];
        assert!(h.bursty && (h.costs_ms.unwrap() - 5.0).abs() < 1e-4, "{h:?}");
        assert!(h.findings.iter().any(|l| l.starts_with("Its delivery needed about 5.0 ms more lead")), "{h:?}");
        assert!(h.findings.iter().any(|l| l.starts_with("With the exclusive mic") && l.ends_with("a reference.")), "{h:?}");
    }

    /// The live check's case from the blocks: the exclusive mic, the loopback aligned at block 2 and its target raised from 12
    /// to 18 ms at block 300, after the check's start-up (block 262), as the engine raises it once the takes have come up
    /// short for 0.6 s. Measured, the start-up leaves it at 12 and the rise is after it; assessed, it's steady. One-sample
    /// slips are said as the drift's, not as trouble.
    #[test]
    fn a_raise_just_after_the_start_up_is_the_exclusive_mic_s_own() {
        let mut blocks: Vec<ChainBlock> = (0..1500u64).map(|i| block(i, 10.0 * i as f64, 0.8)).collect();
        for (i, b) in blocks.iter_mut().enumerate() {
            let target_ms = if i < 300 { 12.0 } else { 18.0 };
            b.reference = Some(ReferenceFacts {
                fresh: true,
                consumed: if i % 400 == 399 { 479 } else { 480 },
                // The raise moves the read position 6 ms back, a jump of the timeline, as the engine's does.
                read_index: i as i64 * 480 - 576 - if i >= 300 { 288 } else { 0 },
                jumped: if i == 300 { -288 } else { 0 },
                lead_reading_ms: target_ms,
                lead_ms: target_ms,
                target_ms,
                stamped: true,
                events: match i {
                    2 => ReferenceFacts::ALIGNED,
                    300 => ReferenceFacts::RETARGETED,
                    _ => 0,
                },
                ..Default::default()
            });
        }
        let info = |name: &str, period_ms: f32, exclusive: Option<&'static str>| StreamInfo {
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms,
            low_latency: false,
            autoconvert: false,
            raw: false,
            exclusive,
            exclusive_refused: None,
        };
        let status = EngineStatus {
            mic: Some(info("CABLE-A Output (VB-Audio Virtual Cable A)", 3.33, Some("16-bit"))),
            output: Some(info("CABLE Input (VB-Audio Virtual Cable)", 2.0, Some("16-bit"))),
            reference: Some(info("CABLE-B Input (VB-Audio Virtual Cable B)", 10.0, None)),
            ..Default::default()
        };
        let f = measure(&blocks, &TelemetrySnapshot::default(), &status, 15.0, false);
        let r = f.reference.as_ref().unwrap();
        assert_eq!((r.start_target_ms, r.settled_target_ms, r.target_ms), (12.0, 12.0, 18.0), "{r:?}");
        assert_eq!((r.blocks, r.slips, r.jumps), (1500 - 262, 3, 0), "{r:?}");
        let all = assess(&f);
        let h = &all[2];
        assert_eq!(h.verdict(), "steady", "{h:?}");
        assert!(
            h.findings.iter().any(|l| l
                == "It followed the clocks' drift in 3 one-sample slips after its start-up, as it should: \
                                            they hold the echo path where it is."),
            "{h:?}"
        );
        assert!(!h.findings.iter().any(|l| l.contains("jumped")), "{h:?}");
    }

    /// A stream the engine didn't describe (its status gone at the end) gets no period-based verdict, nor USB advice for it.
    #[test]
    fn an_unknown_stream_isnt_judged_by_its_period() {
        let mut f = steady();
        f.mic.stream = Stream::default();
        f.mic.delivery = spread(10.4, 10.7, 11.0, 11.3, 10.7);
        f.output.stream = Stream::default();
        f.output.render = spread(24.0, 25.7, 26.0, 26.1, 25.7);
        let all = assess(&f);
        assert!(!all[0].late && all[0].tries.is_empty(), "{:?}", all[0]);
        assert!(!all[1].slow, "{:?}", all[1]);
        let text = format(&f, &all);
        assert!(text.contains("\n\nMicrophone: (the engine didn't say which device): steady"), "{text}");
        assert!(text.contains("so its lateness wasn't judged"), "{text}");
    }

    fn block(index: u64, wake_ms: f64, mic_ms: f32) -> ChainBlock {
        // An output device 50 ppm fast, read at each block.
        let pos = index as f64 * 480.0 - 960.0;
        ChainBlock {
            index,
            capture_qpc_hns: index as f64 * 1e5,
            mic_stamped: true,
            mic_index: index * 480,
            mic_wake_hns: wake_ms * 1e4,
            mic_queued: 0,
            mic_discarded: 0,
            mic_skipped: 0,
            reference: None,
            aec_delay_ms: f32::NAN,
            aec_erle_db: f32::NAN,
            settings: ChainParams::default(),
            budget: Some(LatencyBudget { mic_ms, wait_ms: 0.1, chain_ms: 10.0, queue_ms: 14.0, render_ms: 3.0 }),
            output: OutputFacts {
                target: 672,
                clock_pos: pos,
                clock_qpc_hns: 3e5 + pos * 1e7 / 48_000.0 / (1.0 + 50e-6),
                ..Default::default()
            },
        }
    }

    /// From the blocks: bursts are blocks that share a wake, the wake gaps and the delivery come from the facts (after the
    /// run's first second), the drift from a fit of the clocks, the counters from the telemetry, and the reference's figures
    /// leave its start-up out (from its alignment).
    #[test]
    fn the_figures_come_from_the_blocks_and_counters() {
        // Pairs every other wake: wakes at 0, 20, 30, 50, 60, ... with two blocks at each 20 ms gap.
        let mut blocks = Vec::new();
        let mut t = 0.0;
        for k in 0..600u64 {
            if k % 3 == 0 {
                t += 20.0;
                blocks.push(block(blocks.len() as u64, t, 20.5));
                blocks.push(block(blocks.len() as u64, t, 10.5));
            } else {
                t += 10.0;
                blocks.push(block(blocks.len() as u64, t, 10.5));
            }
        }
        for (i, b) in blocks.iter_mut().enumerate() {
            b.reference = Some(ReferenceFacts {
                fresh: i % 50 != 0,
                owed: if i >= 300 && i % 100 == 0 { 12 } else { 0 },
                consumed: 480,
                read_index: i as i64 * 480 - 576,
                lead_reading_ms: if i % 2 == 0 { 12.5 } else { 11.5 },
                lead_ms: 12.0,
                target_ms: if i < 10 { 12.0 } else { 15.0 },
                stamped: true,
                drift_ppm: 30.0,
                jumped: if i == 5 { -300 } else { 0 },
                events: if i == 3 { ReferenceFacts::ALIGNED } else { 0 },
                ..Default::default()
            });
        }
        let info = |name: &str, period_ms: f32| StreamInfo {
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            period_ms,
            low_latency: false,
            autoconvert: false,
            raw: false,
            exclusive: None,
            exclusive_refused: None,
        };
        let status = EngineStatus {
            mic: Some(info("Mic", 10.0)),
            output: Some(StreamInfo { exclusive: Some("16-bit"), ..info("CABLE Input", 2.0) }),
            reference: Some(info("Speakers", 10.0)),
            ..Default::default()
        };
        let counters = TelemetrySnapshot {
            capture_glitches: 1,
            stalls: 2,
            output_slips: 4,
            reference_gaps: 3,
            output_drift_ppm: -800.0,
            ..Default::default()
        };
        let f = measure(&blocks, &counters, &status, 15.0, false);
        // 800 blocks in 200 cycles of a pair and two singles; from the 100th (25 whole cycles): 175 cycles, 525 wakes.
        assert_eq!((blocks.len(), f.mic.blocks, f.mic.wakes), (800, 700, 525));
        assert!((f.mic.bunched - 350.0 / 700.0).abs() < 1e-4, "{}", f.mic.bunched);
        assert_eq!((f.mic.wake_gap_p95_ms, f.mic.wake_gap_max_ms), (20.0, 20.0));
        let d = f.mic.delivery.unwrap();
        assert_eq!((d.p5, d.median, d.max), (10.5, 10.5, 20.5));
        assert_eq!((f.mic.glitches, f.mic.stalls, f.output.events.slips), (1, 2, 4));
        assert!(f.output.stream.exclusive && (f.output.target_ms - 14.0).abs() < 1e-4);
        // The drift is the clocks', not the loop's estimate.
        assert_eq!(f.mic_timing, Some(MicTiming::Stamps));
        let drift = f.output.drift.unwrap();
        assert!((drift.ppm - 50.0).abs() < 0.5 && drift.uncertainty_ppm < 1.0, "{drift:?}");
        let r = f.reference.unwrap();
        // From the alignment at block 3: 263 blocks of start-up.
        assert_eq!(r.blocks, blocks.len() - 263);
        assert_eq!(
            (r.start_target_ms, r.settled_target_ms, r.target_ms, r.jumps, r.gaps),
            (12.0, 15.0, 15.0, 0, 3),
            "the start-up's jump left out"
        );
        assert!((r.stale - 10.0 / 537.0).abs() < 1e-4 && (r.owed - 5.0 / 537.0).abs() < 1e-4, "{r:?}");
        assert!((r.scatter_ms - 0.5).abs() < 1e-4, "{r:?}");
        let ref_drift = r.drift.unwrap();
        assert!(ref_drift.ppm.abs() < ref_drift.uncertainty_ppm, "a loopback on the mic's clock, its readings scattered: {ref_drift:?}");
        // No reference stream: none.
        assert!(measure(&blocks, &counters, &EngineStatus { reference: None, ..status }, 15.0, false).reference.is_none());
    }
}
