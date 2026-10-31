//! The audio thread's per-block bookkeeping, apart from WASAPI (engine.md "The audio thread").
//!
//! [`Pipeline`] owns the mic FIFO, the reference's timeline ([`Reference`], reference.rs), the
//! output's drift loop and pending samples, the latency readout and the rolling statistics. It
//! talks to streams only through [`CaptureSource`] and [`RenderSink`], to the chain through
//! [`BlockProcessor`] and to the clock through [`Ticks`], so tests drive it with fakes and a
//! simulated device clock.
//!
//! Everything called per wake-up or per block is real-time: no allocation, locks, logging,
//! formatting or panics. Errors are raw HRESULTs.

use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::{Chain, ChainReport};
use nf_core::controls::ChainParams;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::meter::MeterBallistics;
use nf_core::dsp::slip::{Slip, SlipResampler};
use nf_core::latency::stats::RollingStats;
use nf_core::rt::Dither;
use nf_core::stage::StageStats;
use nf_core::telemetry::Telemetry;
use nf_core::{BLOCK, Block, SAMPLE_RATE};
use rtrb::Producer;

use crate::api::ChainBlock;
use crate::fifo::Fifo;
use crate::recorder::ChainTx;
use crate::reference::{MicTime, Reference};
use crate::timing::{HNS_PER_S, StampMap};

/// A raw HRESULT from a failed stream call.
pub(crate) type Hr = i32;

/// Dither amplitude for the mic and reference blocks: about -180 dBFS. It keeps AEC3's and
/// RNNoise's filters out of denormal range on exact digital silence (sonora #34) without FTZ/DAZ.
pub(crate) const DITHER: f32 = 1e-9;
/// Mic FIFO capacity: 200 ms.
const MIC_FIFO: usize = 9_600;
/// Missing mic or loopback frames (by device position) up to this many are replaced by zeros so
/// the stream stays continuous; a bigger jump is only counted (on the mic; the reference's stamps
/// show it as a broken timeline).
pub(crate) const MAX_GAP_FILL: u64 = 4_800;
/// Output samples we can hold while the device buffer is full.
const PENDING: usize = 6 * BLOCK;
/// Headroom on the reference's starting lead, in frames (2 ms). Steady delivery needs up to a
/// loopback period of lead, at the worst phase of the two clocks, plus the packets' timing jitter.
/// The output side gets the same headroom from `output_margin_ms`.
pub(crate) const REF_MARGIN: usize = 96;
/// Blocks per statistics period (1 s).
pub(crate) const STATS_BLOCKS: u64 = 100;
/// 100 ns units per sample at the app's rate.
const HNS_PER_SAMPLE: f64 = HNS_PER_S / SAMPLE_RATE as f64;
/// A readout outside (0, 2000) ms is a bad stamp, not a latency.
const MAX_READOUT_MS: f64 = 2_000.0;

/// One capture packet, converted to mono by the source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Packet {
    pub frames: usize,
    /// Device position of the first frame (`BufferInfo::index`).
    pub device_pos: u64,
    /// QPC time of the first frame in 100 ns, or `None` when the driver flagged it as unreliable.
    pub qpc_hns: Option<u64>,
    pub discontinuity: bool,
}

/// A capture stream as the pipeline sees it.
pub(crate) trait CaptureSource {
    /// Reads the next packet into `out` as mono f32 (silent packets already zeroed). `Ok(None)`
    /// when no packet is ready.
    fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<Packet>, Hr>;
}

/// A render clock reading: the stream position in frames, the QPC time that position belongs to,
/// and the QPC time the reading was taken (`now_hns`), so the position can be carried forward to
/// "now" when the device updates it a period at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClockReading {
    pub pos_frames: f64,
    pub qpc_hns: f64,
    pub now_hns: f64,
}

impl ClockReading {
    /// The device position at `now_hns`: `pos + (now - qpc) * fs`, carried forward by at most
    /// `max_frames` (a stale position isn't extrapolated without bound). `max`/`min` rather than
    /// `clamp`, which asserts its bounds (a panic path on the audio thread); NaN carries nothing.
    #[inline]
    pub(crate) fn position_now(&self, max_frames: f64) -> f64 {
        self.pos_frames + ((self.now_hns - self.qpc_hns) / HNS_PER_SAMPLE).max(0.0).min(max_frames)
    }
}

/// A render stream as the pipeline sees it. Frames are at the app's 48 kHz.
pub(crate) trait RenderSink {
    fn buffer_frames(&self) -> usize;
    /// Frames queued in the device buffer.
    fn padding(&mut self) -> Result<usize, Hr>;
    /// Writes mono frames; the caller guarantees they fit.
    fn write(&mut self, mono: &[f32]) -> Result<(), Hr>;
    fn write_silence(&mut self, frames: usize) -> Result<(), Hr>;
    /// `Ok(None)` while the position isn't running yet (it reads 0 for a few ms after start).
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr>;
}

/// The output's drift loop, as nf-core's `DriftController` provides it (the reference has its own:
/// reference.rs).
pub(crate) trait DriftControl {
    fn update(&mut self, fill_samples: f32) -> Slip;
    fn ppm(&self) -> f32;
    fn reset(&mut self, fill_samples: f32);
}

impl DriftControl for DriftController {
    #[inline]
    fn update(&mut self, fill_samples: f32) -> Slip {
        DriftController::update(self, fill_samples)
    }
    #[inline]
    fn ppm(&self) -> f32 {
        DriftController::ppm(self)
    }
    #[inline]
    fn reset(&mut self, fill_samples: f32) {
        DriftController::reset(self, fill_samples)
    }
}

/// The ±1-sample block resampler, as nf-core's `SlipResampler` provides it.
pub(crate) trait Resampler {
    fn resample(&mut self, input: &[f32], out: &mut [f32]);
}

impl Resampler for SlipResampler {
    #[inline]
    fn resample(&mut self, input: &[f32], out: &mut [f32]) {
        SlipResampler::resample(self, input, out)
    }
}

/// A monotonic tick source (QPC in the engine).
pub(crate) trait Ticks {
    fn ticks(&mut self) -> u64;
    fn to_us(&self, ticks: u64) -> f32;
}

/// The chain, as the pipeline drives it.
pub(crate) trait BlockProcessor {
    fn process<T: Ticks>(&mut self, mic: &Block, reference: &Block, out: &mut Block, ticks: &mut T) -> ChainReport;
    fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>);
    /// The live settings it reads each block, for the chain recorder (the tests' stand-ins have none).
    fn params(&self) -> ChainParams {
        ChainParams::default()
    }
}

impl BlockProcessor for Chain {
    #[inline]
    fn process<T: Ticks>(&mut self, mic: &Block, reference: &Block, out: &mut Block, ticks: &mut T) -> ChainReport {
        self.process_timed(mic, reference, out, || ticks.ticks())
    }
    fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
        Chain::stage_stats(self)
    }
    #[inline]
    fn params(&self) -> ChainParams {
        self.controls().load()
    }
}

/// One tapped mic block for the latency test: the QPC time of its first sample (hns; NaN when
/// unknown) and the raw samples.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TapItem {
    pub qpc_hns: f64,
    pub samples: Block,
}

/// Counters the pipeline keeps (published once a second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// Mic packets flagged discontinuous or missing by device position.
    pub capture_glitches: u64,
    /// Output underruns: zeros written because the padding was under a period and the fill, with
    /// this block and the wake's backlog, fell short of the target; or the device found empty (it
    /// ran dry). Not counted at the first block, which sets the starting fill.
    pub render_underruns: u64,
    /// Output coarse drops: times the fill ran over target + 2 blocks and whole blocks were dropped
    /// back to within half a block (one per event), plus pending overflows.
    pub output_drops: u64,
    /// Blocks whose reference was (partly) zeros for samples that hadn't arrived: none had yet, or
    /// they came late and were discarded when they did (the timeline kept whole; reference.rs).
    pub reference_gaps: u64,
    /// Reference coarse corrections after the start-up: jumps of the reference's timeline, either
    /// way. A broken timeline undone (a stall longer than the buffers, a device reset), a change of
    /// the lead target (a raise when underflows persisted, a lowering after five minutes under), or,
    /// timed by delivery, a step of the count put back or a centring after a break.
    pub reference_drops: u64,
    pub output_slips: u64,
    pub reference_slips: u64,
    /// Tap items lost because the latency test's ring was full.
    pub tap_overflows: u64,
}

/// Fixed sizes and targets for one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PipelineConfig {
    /// The output's device period, in frames.
    pub out_period: usize,
    /// `io.output_margin_ms`, in frames.
    pub out_margin: usize,
    /// The loopback's period in frames, when there's a reference stream.
    pub ref_period: Option<usize>,
    /// Largest capture packet of the mic or the loopback, in frames.
    pub max_packet: usize,
}

impl PipelineConfig {
    /// One render period + one block + the margin (engine.md "Targets").
    pub(crate) fn out_target(&self) -> usize {
        self.out_period + BLOCK + self.out_margin
    }
    /// The reference's starting lead target (engine.md "The reference"): one loopback period +
    /// [`REF_MARGIN`]; 0 without a reference. It may rise in the first seconds, and later when
    /// underflows persist (and come back down after five minutes under).
    pub(crate) fn ref_target(&self) -> usize {
        self.ref_period.map_or(0, |p| p + REF_MARGIN)
    }
}

/// The per-block state of one run.
pub(crate) struct Pipeline<D = DriftController, R = SlipResampler> {
    // Mic.
    mic: Fifo,
    /// Samples ever pushed into the mic FIFO; the head sample's index is `mic_abs - mic.len()`.
    mic_abs: u64,
    mic_next_pos: Option<u64>,
    mic_stamps: StampMap,
    /// The wake that delivered the last mic packet (hns): the mic's timing without stamps.
    mic_last_hns: f64,
    /// Since the last block: mic samples dropped by the FIFO (it overflowed), and device-position gaps not zero-filled
    /// (the chain recorder's `mic_discarded` and `mic_skipped`).
    mic_discarded: u64,
    mic_skipped: i64,
    packet: Box<[f32]>,
    // Reference.
    has_reference: bool,
    reference: Reference,
    ref_rs: R,
    // Output.
    out_drift: D,
    out_rs: R,
    out_target: usize,
    out_period: usize,
    pending: Box<[f32]>,
    pending_len: usize,
    /// Frames ever written to the device, prefill and padding included: the device-stream index of
    /// the next frame written.
    written_total: u64,
    /// The output fill the drift loop saw at the last block (samples), as published.
    last_out_fill: f64,
    /// Calibrates the interpolated output fill to the padding.
    fill_offset: FillOffset,
    /// The first block has set the output's starting fill (wasapi/F).
    out_started: bool,
    /// Dropping whole output blocks back towards the target (after a coarse overfill).
    out_draining: bool,
    // Block buffers.
    mic_blk: Box<Block>,
    ref_blk: Box<Block>,
    ref_in: Box<[f32; BLOCK + 1]>,
    out_blk: Box<Block>,
    slipped: Box<[f32; BLOCK + 1]>,
    dither: Dither,
    /// The latency test's tap, while one runs.
    pub(crate) tap: Option<Producer<TapItem>>,
    /// The chain recorder, while one is armed (recorder.rs).
    pub(crate) recorder: Option<ChainTx>,
    /// Display ballistics for the published meters (DSP-3).
    in_meter: MeterBallistics,
    out_meter: MeterBallistics,
    // Statistics.
    pub(crate) counters: Counters,
    latency_ms: RollingStats<100>,
    block_us: RollingStats<100>,
    aec_us: RollingStats<100>,
    noise_us: RollingStats<100>,
    blocks: u64,
    near_clip_period: bool,
    last_pool_allocs: Option<u64>,
    pub(crate) last_latency_ms: Option<f32>,
}

impl<D: DriftControl, R: Resampler> Pipeline<D, R> {
    /// Allocates everything the run needs. Not real-time. The output's drift loop is `out_drift`;
    /// the reference's is its own ([`Reference`]).
    pub(crate) fn new(cfg: PipelineConfig, out_drift: D, out_rs: R, ref_rs: R) -> Self {
        Self {
            mic: Fifo::new(MIC_FIFO),
            mic_abs: 0,
            mic_next_pos: None,
            mic_stamps: StampMap::new(SAMPLE_RATE),
            mic_last_hns: 0.0,
            mic_discarded: 0,
            mic_skipped: 0,
            packet: vec![0.0; cfg.max_packet.max(BLOCK)].into_boxed_slice(),
            has_reference: cfg.ref_period.is_some(),
            reference: Reference::new(cfg.ref_target(), cfg.ref_period.unwrap_or(BLOCK)),
            ref_rs,
            out_drift,
            out_rs,
            out_target: cfg.out_target(),
            out_period: cfg.out_period,
            pending: vec![0.0; PENDING].into_boxed_slice(),
            pending_len: 0,
            written_total: 0,
            last_out_fill: 0.0,
            fill_offset: FillOffset::new(cfg.out_period),
            out_started: false,
            out_draining: false,
            mic_blk: Box::new([0.0; BLOCK]),
            ref_blk: Box::new([0.0; BLOCK]),
            ref_in: Box::new([0.0; BLOCK + 1]),
            out_blk: Box::new([0.0; BLOCK]),
            slipped: Box::new([0.0; BLOCK + 1]),
            dither: Dither::new(0x2545_F491),
            tap: None,
            recorder: None,
            in_meter: MeterBallistics::new(),
            out_meter: MeterBallistics::new(),
            counters: Counters::default(),
            latency_ms: RollingStats::new(),
            block_us: RollingStats::new(),
            aec_us: RollingStats::new(),
            noise_us: RollingStats::new(),
            blocks: 0,
            near_clip_period: false,
            last_pool_allocs: None,
            last_latency_ms: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn out_target(&self) -> usize {
        self.out_target
    }

    /// The reference's lead target now, in samples.
    #[cfg(test)]
    pub(crate) fn ref_target(&self) -> usize {
        self.reference.target() as usize
    }

    /// Seeds the per-second pool-allocation count with the count after warm-up.
    pub(crate) fn set_alloc_baseline(&mut self, scope_allocs: u64) {
        self.last_pool_allocs = Some(scope_allocs);
    }

    /// Writes the output's starting fill (zeros up to the target) before the stream starts.
    pub(crate) fn prefill<S: RenderSink>(&mut self, sink: &mut S) -> Result<(), Hr> {
        let n = self.out_target.min(sink.buffer_frames());
        sink.write_silence(n)?;
        self.written_total += n as u64;
        self.last_out_fill = n as f64;
        Ok(())
    }

    /// Whether the mic FIFO holds a complete block, so this wake will process one (the loopback and
    /// keep-alive are serviced only then; performance/F3).
    #[inline]
    pub(crate) fn block_ready(&self) -> bool {
        self.mic.len() >= BLOCK
    }

    /// Stops using the reference after its stream failed (wasapi/B): the chain gets dithered zeros
    /// from now on, with no gaps counted, and the mic-to-output path carries on.
    pub(crate) fn drop_reference(&mut self) {
        self.has_reference = false;
        self.reference.clear();
    }

    /// Drains every ready mic packet into the mic FIFO. Detects gaps by device position (the
    /// driver's discontinuity flag is rarely set), zero-filling short ones, and keeps the QPC time
    /// of the FIFO's samples from the packet stamps. Returns the frames read.
    pub(crate) fn drain_mic<C: CaptureSource>(&mut self, src: &mut C, now_hns: f64) -> Result<usize, Hr> {
        let mut total = 0;
        while let Some(p) = src.read_packet(&mut self.packet)? {
            let n = p.frames.min(self.packet.len());
            let mut glitch = p.discontinuity;
            if let Some(expect) = self.mic_next_pos
                && p.device_pos != expect
            {
                glitch = true;
                let gap = p.device_pos.wrapping_sub(expect);
                if p.device_pos > expect && gap <= MAX_GAP_FILL {
                    self.push_mic_zeros(gap as usize);
                } else {
                    // Negative when the position went back.
                    self.mic_skipped = self.mic_skipped.wrapping_add(gap as i64);
                }
            }
            self.mic_next_pos = Some(p.device_pos + n as u64);
            if glitch {
                self.counters.capture_glitches += 1;
            }
            if let Some(ts) = p.qpc_hns {
                self.mic_stamps.observe(self.mic_abs, ts, now_hns);
            }
            self.mic_last_hns = now_hns;
            let over = n.saturating_sub(self.mic.free());
            if over > 0 {
                // Overflow (we didn't run for 200 ms): keep the newest audio.
                self.mic_discarded += self.mic.discard(over) as u64;
                self.counters.capture_glitches += 1;
            }
            if let Some(src) = self.packet.get(..n) {
                // All of it, unless the packet is longer than the FIFO.
                self.mic_discarded += (n - self.mic.push(src)) as u64;
            }
            self.mic_abs += n as u64;
            total += n;
        }
        Ok(total)
    }

    fn push_mic_zeros(&mut self, n: usize) {
        let over = n.saturating_sub(self.mic.free());
        self.mic_discarded += self.mic.discard(over) as u64;
        self.mic_discarded += (n - self.mic.push_zeros(n)) as u64;
        self.mic_abs += n as u64;
    }

    /// Drains every ready loopback packet onto the reference's timeline (its stamps, position gaps
    /// and owed samples are the timeline's business: reference.rs).
    pub(crate) fn drain_reference<C: CaptureSource>(&mut self, src: &mut C, now_hns: f64) -> Result<usize, Hr> {
        self.reference.wake(now_hns);
        let mut total = 0;
        while let Some(p) = src.read_packet(&mut self.packet)? {
            let n = p.frames.min(self.packet.len());
            if let Some(s) = self.packet.get(..n) {
                self.reference.push(&p, s, now_hns);
            }
            total += n;
        }
        Ok(total)
    }

    /// Mic samples buffered.
    #[cfg(test)]
    pub(crate) fn mic_len(&self) -> usize {
        self.mic.len()
    }

    /// Takes one block off the mic FIFO, as `process_block` does.
    #[cfg(test)]
    pub(crate) fn pop_mic_for_test(&mut self, blk: &mut Block) {
        self.mic.pop_into(&mut blk[..]);
    }

    /// Reference samples buffered.
    #[cfg(test)]
    pub(crate) fn ref_len(&self) -> usize {
        self.reference.fifo_len()
    }

    /// Processes one block if the mic FIFO holds one: reference slip, the chain, output slip and
    /// write, coarse corrections, the latency readout, telemetry and timing. `Ok(false)` when no
    /// complete block is buffered.
    pub(crate) fn process_block<P: BlockProcessor, S: RenderSink, T: Ticks>(
        &mut self, proc: &mut P, sink: &mut S, ticks: &mut T, tel: &Telemetry, readout: bool,
    ) -> Result<bool, Hr> {
        if self.mic.len() < BLOCK {
            return Ok(false);
        }
        let t0 = ticks.ticks();
        let mic_moved = (std::mem::take(&mut self.mic_discarded), std::mem::take(&mut self.mic_skipped));
        let head = self.mic_abs - self.mic.len() as u64;
        let t_capture = self.mic_stamps.time_of(head);
        self.mic.pop_into(&mut self.mic_blk[..]);
        if let Some(tap) = self.tap.as_mut() {
            let item = TapItem { qpc_hns: t_capture.unwrap_or(f64::NAN), samples: *self.mic_blk };
            if tap.push(item).is_err() {
                self.counters.tap_overflows += 1;
            }
        }
        self.dither.apply(&mut self.mic_blk[..], DITHER);
        self.take_reference(head, t_capture);

        let report = proc.process(&self.mic_blk, &self.ref_blk, &mut self.out_blk, ticks);
        self.record(proc, head, t_capture, mic_moved);

        self.push_output(sink, if readout { t_capture } else { None })?;
        if !readout {
            self.latency_ms.clear();
        }

        let t1 = ticks.ticks();
        self.block_us.push(ticks.to_us(t1.wrapping_sub(t0)));
        if report.aec_ticks > 0 {
            self.aec_us.push(ticks.to_us(report.aec_ticks));
        }
        if report.noise_ticks > 0 {
            self.noise_us.push(ticks.to_us(report.noise_ticks));
        }
        self.publish_block(tel, &report);
        self.blocks += 1;
        Ok(true)
    }

    /// Whether a statistics period just ended (call [`Self::publish_second`] then).
    #[inline]
    pub(crate) fn second_due(&self) -> bool {
        self.blocks > 0 && self.blocks.is_multiple_of(STATS_BLOCKS)
    }

    /// Fills `ref_blk` for the mic block whose first sample is `head` (captured at `t_capture` by
    /// its stamp): the reference's timeline serves 480 samples, plus or minus its loop's slip, and
    /// they're resampled to one block (reference.rs). Dithered, as is the silence served without a
    /// reference. The block is timed by its stamp while the mic's stamps are trusted, and by the
    /// wake that delivered the mic's newest sample either way, for the timing without stamps.
    fn take_reference(&mut self, head: u64, t_capture: Option<f64>) {
        if !self.has_reference {
            self.ref_blk.fill(0.0);
            self.dither.apply(&mut self.ref_blk[..], DITHER);
            return;
        }
        let mic = MicTime {
            stamp: t_capture.filter(|_| self.mic_stamps.trusted()),
            delivered: self.mic_last_hns - self.mic_abs.saturating_sub(head) as f64 * HNS_PER_SAMPLE,
            scatter: self.mic_stamps.scatter_hns(),
        };
        let n = self.reference.take(mic, &mut self.ref_in[..], &mut self.counters);
        if let Some(input) = self.ref_in.get(..n) {
            self.ref_rs.resample(input, &mut self.ref_blk[..]);
        }
        self.dither.apply(&mut self.ref_blk[..], DITHER);
    }

    /// While the chain recorder is armed, pushes this block to it: the mic and reference blocks as the chain got them, its
    /// output, and the block's facts (its index, capture time, the mic's moves since the last block, the reference's take,
    /// AEC3's estimates, the chain's settings). A full ring loses the block, counted.
    fn record<P: BlockProcessor>(&mut self, proc: &P, head: u64, t_capture: Option<f64>, (discarded, skipped): (u64, i64)) {
        let Some(rec) = self.recorder.as_mut() else { return };
        let (aec, _) = proc.stage_stats();
        let block = ChainBlock {
            index: self.blocks,
            capture_qpc_hns: t_capture.unwrap_or(f64::NAN),
            mic_stamped: self.mic_stamps.trusted(),
            mic_index: head,
            mic_wake_hns: self.mic_last_hns,
            mic_queued: u32::try_from(self.mic.len()).unwrap_or(u32::MAX),
            mic_discarded: discarded,
            mic_skipped: skipped,
            reference: self.has_reference.then(|| self.reference.facts()),
            aec_delay_ms: aec.map_or(f32::NAN, |a| a.echo_delay_ms),
            aec_erle_db: aec.map_or(f32::NAN, |a| a.erle_db),
            settings: proc.params(),
        };
        rec.push(block, &self.mic_blk, &self.ref_blk, &self.out_blk);
    }

    /// Slips `out_blk` to the output clock, queues it, writes what fits, and applies the coarse
    /// corrections. `t_capture` (when the readout is on) is the block's first-sample capture time.
    ///
    /// The drift loop holds [`Self::output_fill`] (interpolated below a packet) at the target. The
    /// coarse corrections also count this wake's backlog, the whole mic blocks still queued, which
    /// are written before the device needs more (clocks-latency/F1, F2):
    /// - the first block sets the starting fill: zeros up to the target, not counted (wasapi/F);
    /// - padding under a period, and the fill with this block and the backlog still short of the
    ///   target: a real deficit, counted. Zeros top the device up to the target once this wake's
    ///   blocks are written, the least that keeps a period of headroom; the loop restores the rest.
    ///   The fill (phase-independent) decides, not the padding, so an unlucky phase against the
    ///   device's period isn't taken for a deficit;
    /// - padding under a period otherwise: a late wake that this block and the backlog refill. No
    ///   zeros; counted only if the device was empty (it ran dry);
    /// - more than two blocks over the target (the loop's own range), backlog included: whole
    ///   blocks are dropped, pending samples first and then this block, until it's within half a
    ///   block of the target (later blocks of the same burst drop themselves).
    ///
    /// After a correction the loop restarts from the fill the next wake will read: the corrected
    /// fill with this wake's queued blocks written.
    fn push_output<S: RenderSink>(&mut self, sink: &mut S, t_capture: Option<f64>) -> Result<(), Hr> {
        let mut padding = sink.padding()?;
        // One clock reading per block, for the fill and the readout. An error costs only this
        // block's estimate and readout; the next write reports a dead stream.
        let clock = sink.clock().ok().flatten();
        let backlog = self.mic.len() / BLOCK * BLOCK;
        let low = padding < self.out_period;
        if padding == 0 && self.out_started {
            // The device ran dry: its position may have moved on through the silence it played.
            self.fill_offset.reanchor();
        }
        let fill = self.output_fill(clock, padding + self.pending_len, !low);
        // Published as the fill the drift loop sees (before this block), comparable to the target.
        self.last_out_fill = fill;
        let queued = fill + backlog as f64;
        let target = self.out_target as f64;
        // At the start the target is set exactly; after it, this block counts as well.
        let (deficit, this_block) = if self.out_started {
            (low && queued + (BLOCK as f64) < target, BLOCK)
        } else {
            (padding + self.pending_len + backlog < self.out_target, 0)
        };
        let mut slip = Slip::None;
        let mut drop_block = false;
        if deficit {
            if self.out_started {
                self.counters.render_underruns += 1;
            }
            padding = self.flush_pending(sink, padding)?;
            let mut added = 0;
            if self.pending_len == 0 {
                let room = sink.buffer_frames().saturating_sub(padding);
                let n = self.out_target.saturating_sub(padding + backlog + this_block).min(room);
                if n > 0 {
                    sink.write_silence(n)?;
                    self.written_total += n as u64;
                    padding += n;
                    added = n;
                }
            }
            // Zeros raise the estimate and the padding alike; moving pending to the device neither.
            self.out_drift.reset((queued + added as f64) as f32);
            self.out_draining = false;
        } else if low {
            if padding == 0 && self.out_started {
                self.counters.render_underruns += 1;
            }
            slip = self.out_drift.update(fill as f32);
        } else if queued > target + (2 * BLOCK) as f64 || (self.out_draining && queued > target + (BLOCK / 2) as f64) {
            if !self.out_draining {
                self.counters.output_drops += 1;
            }
            let excess = queued - target;
            let rounded = ((excess + (BLOCK / 2) as f64) / BLOCK as f64) as usize * BLOCK;
            let from_pending = rounded.min(self.pending_len);
            self.consume_pending(from_pending);
            drop_block = excess - from_pending as f64 > (BLOCK / 2) as f64;
            let removed = from_pending + if drop_block { BLOCK } else { 0 };
            self.out_draining = excess - removed as f64 > (BLOCK / 2) as f64;
            self.out_drift.reset((queued - removed as f64) as f32);
        } else {
            self.out_draining = false;
            slip = self.out_drift.update(fill as f32);
        }
        self.out_started = true;

        if !drop_block {
            if slip != Slip::None {
                self.counters.output_slips += 1;
            }
            // Where this block's first sample lands in the device stream.
            let index = self.written_total + self.pending_len as u64;
            let m = slip.produced_len().min(BLOCK + 1);
            self.out_rs.resample(&self.out_blk[..], &mut self.slipped[..m]);
            self.append_pending(m);
            if let (Some(t_cap), Some(c)) = (t_capture, clock) {
                self.readout(c, index, t_cap);
            }
        }
        self.flush_pending(sink, padding)?;
        Ok(())
    }

    /// The output fill for the drift loop, before this block, in samples (KNOWN ITEM, output fill
    /// interpolation): frames written (and pending) minus the device position now, carried forward
    /// from the clock reading, so it moves smoothly instead of in packet steps. Calibrated to the
    /// padding by [`FillOffset`]. `device_fill` (padding + pending) without a clock reading or
    /// before the first calibration.
    ///
    /// Only readings that sample the device's sawtooth calibrate: the padding at least a period
    /// (`not_low`), and a clock reading at most a period old. A clock that steps once a period
    /// and hasn't stepped for longer has stalled (as a cable does), and the time to its next take
    /// is then unknown. The position is carried forward by at most a period for the same reason.
    fn output_fill(&mut self, clock: Option<ClockReading>, device_fill: usize, not_low: bool) -> f64 {
        let Some(c) = clock else { return device_fill as f64 };
        let period = self.out_period as f64;
        let est = (self.written_total + self.pending_len as u64) as f64 - c.position_now(period);
        if !est.is_finite() {
            return device_fill as f64;
        }
        let fresh = c.now_hns - c.qpc_hns <= period * HNS_PER_SAMPLE;
        if not_low && fresh {
            self.fill_offset.observe(est - device_fill as f64);
        }
        match self.fill_offset.offset() {
            Some(o) => est - o,
            None => device_fill as f64,
        }
    }

    /// Pushes one latency sample: the block's predicted play-out time minus its capture time.
    fn readout(&mut self, clock: ClockReading, index: u64, t_capture: f64) {
        let t_play = play_time_hns(clock, index, SAMPLE_RATE as f64);
        if let Some(ms) = readout_ms(t_play, t_capture) {
            self.latency_ms.push(ms);
            self.last_latency_ms = Some(ms);
        }
    }

    fn append_pending(&mut self, m: usize) {
        let m = m.min(self.slipped.len()).min(self.pending.len());
        let over = (self.pending_len + m).saturating_sub(self.pending.len());
        if over > 0 {
            self.consume_pending(over);
            self.counters.output_drops += 1;
        }
        let start = self.pending_len;
        if let (Some(dst), Some(src)) = (self.pending.get_mut(start..start + m), self.slipped.get(..m)) {
            dst.copy_from_slice(src);
            self.pending_len += m;
        }
    }

    /// Drops the `n` oldest pending samples.
    fn consume_pending(&mut self, n: usize) {
        let n = n.min(self.pending_len);
        // Through `get_mut` so the bound is visible to LLVM: no slice-index panic path.
        if let Some(live) = self.pending.get_mut(..self.pending_len) {
            live.copy_within(n.., 0);
        }
        self.pending_len -= n;
    }

    /// Writes as much pending audio as the device has room for; returns the new padding.
    fn flush_pending<S: RenderSink>(&mut self, sink: &mut S, padding: usize) -> Result<usize, Hr> {
        let room = sink.buffer_frames().saturating_sub(padding);
        let n = room.min(self.pending_len);
        if n == 0 {
            return Ok(padding);
        }
        if let Some(src) = self.pending.get(..n) {
            sink.write(src)?;
        }
        self.consume_pending(n);
        self.written_total += n as u64;
        Ok(padding + n)
    }

    /// Per-block telemetry: meters (dBFS, through display ballistics so a reader sampling them at
    /// any rate sees every block's peak; DSP-3), the clip latch, near-clip, speech probability and
    /// the noise stage's reduction (NaN while it isn't reporting: off, or in passthrough; DSP-4).
    fn publish_block(&mut self, tel: &Telemetry, report: &ChainReport) {
        match report.input {
            Some(m) => {
                let (peak, rms) = self.in_meter.update(&m);
                tel.input.store(peak, rms);
                if m.clipped {
                    tel.clip_latched.store(true, Relaxed);
                }
                if m.near_clip {
                    self.near_clip_period = true;
                    tel.near_clip.store(true, Relaxed);
                }
            }
            // Meters off: start afresh when they come back.
            None => self.in_meter.reset(),
        }
        match report.output {
            Some(m) => {
                let (peak, rms) = self.out_meter.update(&m);
                tel.output.store(peak, rms);
            }
            None => self.out_meter.reset(),
        }
        match report.noise {
            Some(n) => {
                tel.speech_probability.store(n.speech_probability);
                tel.noise_reduction_db.store(n.gain_reduction_db);
            }
            None => {
                tel.speech_probability.store(f32::NAN);
                tel.noise_reduction_db.store(f32::NAN);
            }
        }
        tel.blocks.store(self.blocks + 1, Relaxed);
    }

    /// Once a second: timings, the latency readout, drift, fills, counters, AEC stats and the
    /// real-time allocation counts. `scope_allocs` and `scope_fallbacks` are this thread's
    /// `nf_mem` counters; `fallback_baseline` is the fallback count after warm-up.
    pub(crate) fn publish_second<P: BlockProcessor>(
        &mut self, tel: &Telemetry, proc: &P, scope_allocs: u64, scope_fallbacks: u64, fallback_baseline: u64,
    ) {
        tel.block_us_p50.store(self.block_us.median());
        tel.block_us_p99.store(self.block_us.percentile(0.99));
        tel.block_us_max.store(self.block_us.max());
        tel.aec_us_p99.store(or_zero(self.aec_us.percentile(0.99)));
        tel.noise_us_p99.store(or_zero(self.noise_us.percentile(0.99)));
        self.aec_us.clear();
        self.noise_us.clear();

        let valid = !self.latency_ms.is_empty();
        tel.latency_valid.store(valid, Relaxed);
        if valid {
            tel.latency_ms_median.store(self.latency_ms.median());
            tel.latency_ms_p95.store(self.latency_ms.percentile(0.95));
        }

        tel.output_drift_ppm.store(self.out_drift.ppm());
        tel.output_fill_ms.store((self.last_out_fill * (1000.0 / SAMPLE_RATE as f64)) as f32);
        if self.has_reference {
            // The reference's "fill" is its lead over the mic block, the quantity its loop holds,
            // and its target can move (reference.rs). Whether the stamps time it, too.
            tel.reference_drift_ppm.store(self.reference.drift_ppm());
            tel.reference_fill_ms.store(self.reference.lead_ms());
            tel.reference_target_ms.store(self.reference.target_ms());
            tel.reference_stamped.store(self.reference.stamped(), Relaxed);
        } else {
            // None, or dropped mid-run.
            tel.reference_drift_ppm.store(0.0);
            tel.reference_fill_ms.store(0.0);
            tel.reference_target_ms.store(0.0);
            tel.reference_stamped.store(false, Relaxed);
        }
        let c = &self.counters;
        tel.output_slips.store(c.output_slips, Relaxed);
        tel.reference_slips.store(c.reference_slips, Relaxed);
        tel.capture_glitches.store(c.capture_glitches, Relaxed);
        tel.render_underruns.store(c.render_underruns, Relaxed);
        tel.reference_gaps.store(c.reference_gaps, Relaxed);
        tel.output_coarse_drops.store(c.output_drops, Relaxed);
        tel.reference_coarse_drops.store(c.reference_drops, Relaxed);
        tel.tap_overflows.store(c.tap_overflows, Relaxed);

        let (aec, _) = proc.stage_stats();
        if let Some(a) = aec {
            tel.aec_delay_ms.store(a.echo_delay_ms);
            tel.aec_erle_db.store(a.erle_db);
        }

        tel.rt_heap_allocations.store(scope_fallbacks.saturating_sub(fallback_baseline), Relaxed);
        let per_s = self.last_pool_allocs.map_or(0, |prev| scope_allocs.saturating_sub(prev));
        tel.rt_pool_allocations_per_s.store(per_s, Relaxed);
        self.last_pool_allocs = Some(scope_allocs);

        tel.near_clip.store(self.near_clip_period, Relaxed);
        self.near_clip_period = false;
    }
}

/// The predicted play-out time (hns) of device-stream frame `index`, from one clock reading:
/// `t_play = clock.qpc + (index - clock.pos) / fs`.
#[inline]
pub(crate) fn play_time_hns(clock: ClockReading, index: u64, fs: f64) -> f64 {
    clock.qpc_hns + (index as f64 - clock.pos_frames) * (HNS_PER_S / fs)
}

/// `t_play - t_capture` in ms, or `None` when it can't be a real latency (a bad stamp, or the
/// clock not yet running).
#[inline]
pub(crate) fn readout_ms(t_play_hns: f64, t_capture_hns: f64) -> Option<f32> {
    let ms = (t_play_hns - t_capture_hns) * 1e-4;
    (ms > 0.0 && ms < MAX_READOUT_MS).then_some(ms as f32)
}

/// Calibrates the interpolated output fill against the padding the targets are set in.
///
/// "Frames written minus the device position now" moves smoothly, but it also counts what the
/// device has taken from our buffer and not yet played, so it sits above the padding by an offset.
/// An engine that takes a period at a time makes that offset a sawtooth one period wide, at
/// whatever phase the two clocks happen to be in. This keeps a window one period wide that slides
/// to take in every reading: once the sawtooth has swept it (one beat of the two clocks) the window
/// is the sawtooth's range and its centre stays put. The drift loop then holds the latency still,
/// and the padding swings evenly about the target, as it did on average before.
///
/// It is seeded on its first reading, so the switch from the padding is continuous. Readings taken
/// next to a dry or stalled device aren't sawtooth samples and are left out (the caller), and the
/// window is never seeded afresh: when the clocks barely drift the readings sweep the sawtooth only
/// slowly, and a fresh, centred seed would shift the held latency by up to half a period for as
/// long as the phase takes to sweep (clocks-latency, output fill interpolation).
///
/// Two events do move the sawtooth itself: the device running dry (its position may move on
/// through the silence it plays, or not) and a clock jump (a reading more than two periods off).
/// Then the window is shifted by how far the readings moved: the median of the last
/// [`OFFSET_RING`] readings before the event against the median of as many after it. The phase
/// against the device changes slowly, so the medians compare like with like; meanwhile the fill
/// falls back to the padding.
#[derive(Clone, Copy, Debug)]
struct FillOffset {
    lo: f64,
    width: f64,
    primed: bool,
    /// The last readings, oldest overwritten.
    ring: [f64; OFFSET_RING],
    ring_len: usize,
    ring_next: usize,
    /// While re-anchoring: the median of the readings before the event.
    before: Option<f64>,
}

/// Readings compared across a re-anchor: 32 blocks, a whole cycle of a 1024-frame device period
/// against 10 ms blocks.
const OFFSET_RING: usize = 32;

impl FillOffset {
    fn new(period: usize) -> Self {
        Self { lo: 0.0, width: period.max(1) as f64, primed: false, ring: [0.0; OFFSET_RING], ring_len: 0, ring_next: 0, before: None }
    }

    /// The sawtooth may have moved (the device ran dry): re-anchor from the next readings.
    fn reanchor(&mut self) {
        if !self.primed || self.before.is_some() {
            return;
        }
        match self.median() {
            Some(m) => self.before = Some(m),
            None => self.primed = false,
        }
        self.ring_len = 0;
        self.ring_next = 0;
    }

    /// The offset to take off the estimate; `None` before the first reading and while
    /// re-anchoring.
    #[inline]
    fn offset(&self) -> Option<f64> {
        (self.primed && self.before.is_none()).then_some(self.lo + 0.5 * self.width)
    }

    /// Takes one valid reading of `estimate - padding`.
    #[inline]
    fn observe(&mut self, x: f64) {
        let half = 0.5 * self.width;
        if !self.primed {
            self.lo = x - half;
            self.primed = true;
        } else if self.before.is_none() && (x - (self.lo + half)).abs() > 2.0 * self.width {
            // A reading no sawtooth explains: the clock jumped.
            self.reanchor();
        }
        self.push(x);
        if let Some(before) = self.before {
            if self.ring_len == OFFSET_RING
                && let Some(after) = self.median()
            {
                self.lo += after - before;
                self.before = None;
            }
            return;
        }
        if x > self.lo + self.width {
            self.lo = x - self.width;
        } else if x < self.lo {
            self.lo = x;
        }
    }

    #[inline]
    fn push(&mut self, x: f64) {
        if let Some(slot) = self.ring.get_mut(self.ring_next) {
            *slot = x;
        }
        self.ring_next = (self.ring_next + 1) % OFFSET_RING;
        self.ring_len = (self.ring_len + 1).min(OFFSET_RING);
    }

    /// The median of the readings held (an in-place sort of a copy: no allocation).
    fn median(&self) -> Option<f64> {
        let mut a = self.ring;
        let s = a.get_mut(..self.ring_len.min(OFFSET_RING))?;
        s.sort_unstable_by(f64::total_cmp);
        s.get(s.len() / 2).copied()
    }
}

#[inline]
fn or_zero(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use nf_core::config::ProcessingConfig;
    use nf_core::controls::ChainControls;
    use nf_core::dsp::meter::MeterReading;
    use nf_core::stage::StageReport;

    use super::*;

    /// A PI drift loop with a slip accumulator, shaped like nf-core's contract: enough to exercise
    /// the bookkeeping without depending on that controller's tuning.
    struct FakeDrift {
        target: f32,
        filt: Option<f32>,
        integral: f32,
        acc: f32,
        ppm: f32,
        resets: usize,
    }

    impl FakeDrift {
        fn new(target: usize) -> Self {
            Self { target: target as f32, filt: None, integral: 0.0, acc: 0.0, ppm: 0.0, resets: 0 }
        }
    }

    impl DriftControl for FakeDrift {
        fn update(&mut self, fill: f32) -> Slip {
            let f = self.filt.map_or(fill, |f| f + 0.02 * (fill - f));
            self.filt = Some(f);
            let err = f - self.target;
            // Integral time about 4x the proportional loop's (about 520 blocks): well damped.
            self.integral = (self.integral + err * 0.002).clamp(-2000.0, 2000.0);
            self.ppm = (err * 4.0 + self.integral).clamp(-2000.0, 2000.0);
            self.acc += self.ppm * 1e-6 * BLOCK as f32;
            if self.acc >= 1.0 {
                self.acc -= 1.0;
                Slip::Drop
            } else if self.acc <= -1.0 {
                self.acc += 1.0;
                Slip::Insert
            } else {
                Slip::None
            }
        }
        fn ppm(&self) -> f32 {
            self.ppm
        }
        fn reset(&mut self, fill: f32) {
            self.filt = Some(fill);
            self.acc = 0.0;
            self.resets += 1;
        }
    }

    /// Linear interpolation from n to m samples (end points exact).
    struct FakeResampler;

    impl Resampler for FakeResampler {
        fn resample(&mut self, input: &[f32], out: &mut [f32]) {
            let (n, m) = (input.len(), out.len());
            if n == m {
                out.copy_from_slice(input);
                return;
            }
            for (j, o) in out.iter_mut().enumerate() {
                let p = j as f64 * (n - 1) as f64 / (m - 1) as f64;
                let i = p.floor() as usize;
                let f = (p - i as f64) as f32;
                let a = input[i];
                let b = input[(i + 1).min(n - 1)];
                *o = a + (b - a) * f;
            }
        }
    }

    struct FakeTicks(u64);

    impl Ticks for FakeTicks {
        fn ticks(&mut self) -> u64 {
            self.0 += 10;
            self.0
        }
        fn to_us(&self, ticks: u64) -> f32 {
            ticks as f32 * 0.1
        }
    }

    /// Copies the mic to the output.
    struct CopyChain;

    impl BlockProcessor for CopyChain {
        fn process<T: Ticks>(&mut self, mic: &Block, _r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
            *out = *mic;
            ChainReport::default()
        }
        fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
            (None, None)
        }
    }

    /// Records the reference block the chain saw.
    struct RefSpy(Vec<f32>);

    impl BlockProcessor for RefSpy {
        fn process<T: Ticks>(&mut self, mic: &Block, r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
            self.0.extend_from_slice(r);
            *out = *mic;
            ChainReport::default()
        }
        fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
            (Some(StageStats { echo_delay_ms: 52.0, erle_db: 12.0 }), None)
        }
    }

    /// A queue of packets.
    #[derive(Default)]
    struct FakeCapture {
        packets: VecDeque<(Packet, Vec<f32>)>,
    }

    impl FakeCapture {
        fn push(&mut self, pos: u64, qpc: Option<u64>, samples: Vec<f32>) {
            let p = Packet { frames: samples.len(), device_pos: pos, qpc_hns: qpc, discontinuity: false };
            self.packets.push_back((p, samples));
        }
        /// `n` unstamped frames of `v`, in packets of at most a block.
        fn push_frames(&mut self, n: usize, v: f32) {
            for k in (0..n).step_by(480) {
                self.push(0, None, vec![v; (n - k).min(480)]);
            }
        }
    }

    impl CaptureSource for FakeCapture {
        fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<Packet>, Hr> {
            let Some((p, s)) = self.packets.pop_front() else { return Ok(None) };
            out[..s.len()].copy_from_slice(&s);
            Ok(Some(p))
        }
    }

    /// A render device consuming at its own clock. `padding` is fractional so a drifting
    /// consumption rate can be simulated.
    struct FakeSink {
        buffer: usize,
        padding: f64,
        played: f64,
        now_hns: f64,
        written: Vec<f32>,
        silence: usize,
        dry_frames: f64,
        fail_write: Option<Hr>,
    }

    impl FakeSink {
        fn new(buffer: usize) -> Self {
            Self { buffer, padding: 0.0, played: 0.0, now_hns: 1e10, written: Vec::new(), silence: 0, dry_frames: 0.0, fail_write: None }
        }
        /// The device plays `frames` frames over `dt_hns`.
        fn advance(&mut self, frames: f64, dt_hns: f64) {
            let take = frames.min(self.padding);
            self.dry_frames += frames - take;
            self.padding -= take;
            self.played += frames;
            self.now_hns += dt_hns;
        }
    }

    impl RenderSink for FakeSink {
        fn buffer_frames(&self) -> usize {
            self.buffer
        }
        fn padding(&mut self) -> Result<usize, Hr> {
            Ok(self.padding.ceil() as usize)
        }
        fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
            if let Some(hr) = self.fail_write {
                return Err(hr);
            }
            assert!(self.padding.ceil() as usize + mono.len() <= self.buffer, "write must fit");
            self.written.extend_from_slice(mono);
            self.padding += mono.len() as f64;
            Ok(())
        }
        fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
            assert!(self.padding.ceil() as usize + frames <= self.buffer, "silence must fit");
            self.written.extend(std::iter::repeat_n(0.0, frames));
            self.silence += frames;
            self.padding += frames as f64;
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            Ok(Some(ClockReading { pos_frames: self.played, qpc_hns: self.now_hns, now_hns: self.now_hns }))
        }
    }

    fn cfg(reference: bool) -> PipelineConfig {
        PipelineConfig { out_period: 480, out_margin: 96, ref_period: reference.then_some(480), max_packet: 960 }
    }

    fn pipe(reference: bool) -> Pipeline<FakeDrift, FakeResampler> {
        let c = cfg(reference);
        Pipeline::new(c, FakeDrift::new(c.out_target()), FakeResampler, FakeResampler)
    }

    fn ramp(start: usize, n: usize) -> Vec<f32> {
        (start..start + n).map(|v| (v % 1000) as f32 * 1e-3).collect()
    }

    /// The reference's starting target is now a lead, one loopback period + the margin: what steady
    /// delivery needs at the worst phase of the clocks (reference.rs). It was a FIFO fill of a
    /// period + a block + the margin, which counted the mic block's own 10 ms as well.
    #[test]
    fn targets_follow_the_spec() {
        let c = cfg(true);
        assert_eq!(c.out_target(), 480 + 480 + 96);
        assert_eq!(c.ref_target(), 480 + REF_MARGIN);
        assert_eq!(cfg(false).ref_target(), 0);
    }

    /// Index-coded samples, plus one so that a zero is a filled-in sample.
    fn idx1(start: usize, n: usize) -> Vec<f32> {
        (start + 1..start + 1 + n).map(|v| v as f32).collect()
    }

    /// Loopback samples `from..to`, index-coded, in stamped packets of at most 480: sample 0 was
    /// tapped at `t0`.
    fn push_tapped(lb: &mut FakeCapture, from: usize, to: usize, t0: f64) {
        for k in (from..to).step_by(480) {
            let n = (to - k).min(480);
            lb.push(k as u64, Some((t0 + k as f64 * 1e7 / 48_000.0) as u64), idx1(k, n));
        }
    }

    /// The first block lines the reference up by the stamps, either way: the sample served with
    /// the mic block was tapped the target (576 samples, 12 ms) before the block's first sample
    /// was captured, with zeros for any before the loopback's first. Later blocks carry on from
    /// there, and the lead is published against its target.
    #[test]
    fn the_first_block_aligns_the_reference_by_the_stamps() {
        // How long the loopback ran before the mic's first sample, in samples, and the first
        // sample served: 576 before the mic's, or zeros.
        for (ran, first) in [(1920usize, Some(1344.0)), (336, None), (0, None)] {
            let mut p = pipe(true);
            let mut sink = FakeSink::new(4800);
            p.prefill(&mut sink).unwrap();
            let tel = Telemetry::new();
            let mut spy = RefSpy(Vec::new());
            let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
            let t = 1e10;
            let h = 1e7 / 48_000.0;
            for b in 0..3usize {
                let now = t + (b + 1) as f64 * 100_000.0 + 5_000.0;
                mic.push((b * 480) as u64, Some((t + (b * 480) as f64 * h) as u64), vec![0.0; 480]);
                // Everything the loopback has tapped by the mic block's end, in stamped packets.
                let (from, to) = (if b == 0 { 0 } else { ran + b * 480 }, ran + (b + 1) * 480);
                push_tapped(&mut lb, from, to, t - ran as f64 * h);
                p.drain_mic(&mut mic, now).unwrap();
                p.drain_reference(&mut lb, now).unwrap();
                p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            }
            let served = |b: usize| {
                spy.0[b * 480..(b + 1) * 480].iter().position(|&v| v >= 0.5).map(|k| (spy.0[b * 480 + k] - 1.0) as f64 - k as f64)
            };
            let expect = ran as f64 - 576.0;
            match first {
                Some(f) => assert_eq!(served(0), Some(f), "ran {ran}"),
                None if ran + 480 > 576 => assert_eq!(served(0), Some(expect), "ran {ran}: zeros, then sample 0"),
                None => assert_eq!(served(0), None, "ran {ran}: all before the loopback's first sample"),
            }
            assert_eq!(served(2), Some(expect + 960.0), "ran {ran}: the timeline carries on");
            assert_eq!((p.counters.reference_gaps, p.counters.reference_drops, p.counters.reference_slips), (0, 0, 0), "ran {ran}");
            p.publish_second(&tel, &spy, 0, 0, 0);
            assert!((tel.reference_target_ms.load() - 12.0).abs() < 1e-3 && (tel.reference_fill_ms.load() - 12.0).abs() < 0.1);
        }
    }

    #[test]
    fn readout_arithmetic() {
        // Clock at frame 1000 read at t = 1 s; frame 1480 plays 10 ms later.
        let c = ClockReading { pos_frames: 1000.0, qpc_hns: 1e7, now_hns: 1e7 };
        assert!((play_time_hns(c, 1480, 48_000.0) - (1e7 + 100_000.0)).abs() < 1e-6);
        // A frame already played maps into the past.
        assert!((play_time_hns(c, 520, 48_000.0) - (1e7 - 100_000.0)).abs() < 1e-6);
        assert_eq!(readout_ms(1e7 + 250_000.0, 1e7), Some(25.0));
        assert_eq!(readout_ms(1e7, 1e7 + 1.0), None, "negative");
        assert_eq!(readout_ms(1e7 + 3e7, 1e7), None, "3 s is a bad stamp");
    }

    #[test]
    fn mic_gaps_are_counted_and_zero_filled() {
        let mut p = pipe(false);
        let mut mic = FakeCapture::default();
        mic.push(0, Some(1e9 as u64), vec![1.0; 480]);
        mic.push(480, Some((1e9 + 1e5) as u64), vec![1.0; 100]);
        // 50 frames missing.
        mic.push(630, None, vec![1.0; 100]);
        let n = p.drain_mic(&mut mic, 1e9).unwrap();
        assert_eq!(n, 680);
        assert_eq!(p.counters.capture_glitches, 1);
        assert_eq!(p.mic_len(), 730, "the gap became 50 zeros");
        // The discontinuity flag counts too.
        mic.packets.push_back((Packet { frames: 10, device_pos: 730, qpc_hns: None, discontinuity: true }, vec![0.0; 10]));
        p.drain_mic(&mut mic, 1e9).unwrap();
        assert_eq!(p.counters.capture_glitches, 2);
        // A jump of more than 100 ms is only counted.
        mic.push(1_000_000, None, vec![0.0; 10]);
        p.drain_mic(&mut mic, 1e9).unwrap();
        assert_eq!(p.counters.capture_glitches, 3);
        assert_eq!(p.mic_len(), 750);
    }

    #[test]
    fn capture_time_comes_from_the_latest_stamp() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        // Packets of 240 frames, stamped; the second packet's stamp is 1 ms late (jitter).
        let t0 = 5e9;
        mic.push(0, Some(t0 as u64), ramp(0, 240));
        mic.push(240, Some((t0 + 50_000.0 + 10_000.0) as u64), ramp(240, 240));
        p.drain_mic(&mut mic, t0).unwrap();
        // The block's first sample is mapped back from the latest anchor: (t0 + 60_000) - 240 samples.
        let head_time = p.mic_stamps.time_of(0).unwrap();
        assert!((head_time - (t0 + 10_000.0)).abs() < 1e-6);
        assert!(p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, true).unwrap());
        assert!(!p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, true).unwrap());
    }

    #[test]
    fn output_is_continuous_and_readout_matches_the_queue() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        assert_eq!(sink.silence, p.out_target());
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        let mut ticks = FakeTicks(0);
        let fs = 48_000.0;
        for b in 0..50usize {
            // The packet's first sample was captured 10 ms before now.
            let now = sink.now_hns;
            mic.push((b * 480) as u64, Some((now - 100_000.0) as u64), ramp(b * 480, 480));
            p.drain_mic(&mut mic, now).unwrap();
            let queued = sink.padding.ceil();
            assert!(p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, true).unwrap());
            // Readout = 10 ms in the FIFO + what was queued ahead of the block.
            let expect = 10.0 + queued / fs * 1000.0;
            let got = p.last_latency_ms.unwrap() as f64;
            assert!((got - expect).abs() < 1e-3, "block {b}: {got} vs {expect}");
            sink.advance(480.0, 100_000.0);
        }
        assert_eq!(sink.dry_frames, 0.0, "never ran dry");
        assert_eq!(p.counters.render_underruns, 0);
        // Everything after the prefill is the mic ramp, in order (no slips at 0 ppm).
        let body = &sink.written[p.out_target()..];
        let expect = ramp(0, body.len());
        for (i, (a, b)) in body.iter().zip(&expect).enumerate() {
            assert!((a - b).abs() < 2e-9, "sample {i}: {a} vs {b}");
        }
        assert_eq!(tel.blocks.load(Relaxed), 50);
    }

    /// wasapi/F: the output starts before the mic, so the device may have played some or all of the
    /// prefill by the first block. That block tops the fill up to the target and isn't an underrun.
    #[test]
    fn the_first_block_sets_the_starting_fill_without_an_underrun() {
        for left in [None, Some(0usize), Some(600)] {
            let mut p = pipe(false);
            let mut sink = FakeSink::new(4800);
            if let Some(left) = left {
                p.prefill(&mut sink).unwrap();
                let played = (p.out_target() - left) as f64;
                sink.advance(played, played * 1e7 / 48_000.0);
            }
            let tel = Telemetry::new();
            let mut mic = FakeCapture::default();
            mic.push(0, None, vec![0.5; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            assert_eq!(p.counters.render_underruns, 0, "{left:?}");
            assert_eq!(p.out_drift.resets, 1, "{left:?}");
            // Exactly the target, then the block.
            assert_eq!(sink.padding, (p.out_target() + 480) as f64, "{left:?}");
            assert!(sink.written[sink.written.len() - 480..].iter().all(|&v| (v - 0.5).abs() < 1e-6));
        }
    }

    /// Runs one ordinary block so the start-up is behind; the device then holds target + a block.
    fn started(p: &mut Pipeline<FakeDrift, FakeResampler>, sink: &mut FakeSink, mic: &mut FakeCapture, tel: &Telemetry) {
        p.prefill(sink).unwrap();
        mic.push(0, None, vec![0.5; 480]);
        p.drain_mic(mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, sink, &mut FakeTicks(0), tel, false).unwrap();
        assert_eq!(sink.padding, (p.out_target() + 480) as f64);
    }

    /// clocks-latency/F1: a wake 20 ms late finds the padding under a period, but the device never
    /// runs dry and the three queued blocks bring it back: no underrun, no zeros, and no restart of
    /// the loop.
    #[test]
    fn a_late_wake_with_a_backlog_is_no_underrun() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        started(&mut p, &mut sink, &mut mic, &tel);
        let zeros = sink.silence;
        // 30 ms since the last wake: three periods played, three blocks queued.
        sink.advance(1440.0, 300_000.0);
        assert_eq!(sink.padding, 96.0);
        for b in 1..4usize {
            mic.push((b * 480) as u64, None, vec![0.5; 480]);
        }
        p.drain_mic(&mut mic, 0.0).unwrap();
        let resets = p.out_drift.resets;
        let mut fills = Vec::new();
        while p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {
            fills.push(p.last_out_fill);
        }
        assert_eq!(fills, vec![96.0, 576.0, 1056.0], "written as they come, unpadded");
        assert_eq!(p.counters.render_underruns, 0);
        assert_eq!(sink.dry_frames, 0.0);
        assert_eq!(sink.silence, zeros, "no zeros on top of the backlog");
        assert_eq!(p.out_drift.resets, resets, "the loop runs on");
        assert_eq!(sink.padding, (p.out_target() + 480) as f64, "back to the usual fill after a write");
    }

    /// clocks-latency/F1: a mic that delivers two blocks per wake (20 ms packets), with the wakes
    /// 3 ms early and late in turn, never has zeros put in front of its audio: the loop holds the
    /// device fill across the wake, not just before it.
    #[test]
    fn two_blocks_per_wake_with_jitter_are_no_underruns() {
        let c = cfg(false);
        let mut p = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        let mut start_zeros = None;
        for w in 0..1500usize {
            // 20 ms between wakes, alternately 3 ms short and long.
            let dt_ms = if w % 2 == 0 { 17.0 } else { 23.0 };
            if w > 0 {
                sink.advance(dt_ms * 48.0, dt_ms * 1e4);
            }
            mic.push((w * 960) as u64, None, vec![0.25; 960]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            while p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {}
            if w == 0 {
                start_zeros = Some(sink.silence);
            }
        }
        assert_eq!(p.counters.render_underruns, 0, "{:?}", p.counters);
        assert_eq!(sink.dry_frames, 0.0);
        assert_eq!(Some(sink.silence), start_zeros, "no zeros after the start");
        assert_eq!(p.counters.output_drops, 0);
    }

    /// clocks-latency/F1, the finding's own case: a wake 30 ms late finds the padding at 96, under a
    /// period, with this block and one more queued. Written, they leave the device at the target:
    /// that's no deficit, so no zeros go in front of the audio and nothing is counted.
    #[test]
    fn a_late_wake_that_its_blocks_cover_gets_no_zeros() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        started(&mut p, &mut sink, &mut mic, &tel);
        let zeros = sink.silence;
        sink.advance(1440.0, 300_000.0);
        assert_eq!(sink.padding, 96.0);
        for b in 1..3usize {
            mic.push((b * 480) as u64, None, vec![0.5; 480]);
        }
        p.drain_mic(&mut mic, 0.0).unwrap();
        while p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {}
        assert_eq!(sink.silence, zeros);
        assert_eq!(p.counters.render_underruns, 0);
        assert_eq!(sink.dry_frames, 0.0);
        assert_eq!(sink.padding, p.out_target() as f64);
    }

    /// clocks-latency/F1: after a stall that let the device run dry, zeros top it up only to the
    /// target once the queued blocks are written (the least that keeps a period of headroom), so
    /// the gap is as short as it can be and the loop restores the last block.
    #[test]
    fn a_real_deficit_pads_only_what_the_backlog_wont_cover() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        started(&mut p, &mut sink, &mut mic, &tel);
        let zeros = sink.silence;
        // 40 ms since the last wake: the device ran dry; two blocks are queued.
        sink.advance(1920.0, 400_000.0);
        assert!(sink.dry_frames > 0.0 && sink.padding == 0.0);
        for b in 1..3usize {
            mic.push((b * 480) as u64, None, vec![0.5; 480]);
        }
        p.drain_mic(&mut mic, 0.0).unwrap();
        while p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {}
        assert_eq!(p.counters.render_underruns, 1);
        assert_eq!(sink.silence - zeros, p.out_target() - 960, "the target less the two queued blocks");
        assert_eq!(sink.padding, p.out_target() as f64);
    }

    /// clocks-latency/F2: the cable stopped taking audio for 30 ms, leaving the fill three blocks
    /// over. Past two blocks (the loop's range) whole blocks are dropped, one event, until the fill
    /// is within half a block of the target; it doesn't sit in a dead band for the loop to slip.
    #[test]
    fn overfill_past_two_blocks_drops_back_to_within_half_a_block() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        started(&mut p, &mut sink, &mut mic, &tel);
        let mut fills = Vec::new();
        for b in 1..12usize {
            // Wakes 1-3: the device takes nothing.
            if b > 3 {
                sink.advance(480.0, 100_000.0);
            }
            mic.push((b * 480) as u64, None, vec![0.5; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            fills.push(p.last_out_fill as usize);
        }
        let target = p.out_target();
        assert_eq!(&fills[..4], &[target + 480, target + 960, target + 1440, target + 960], "{fills:?}");
        assert_eq!(p.counters.output_drops, 1, "one event: {fills:?}");
        assert_eq!(p.counters.render_underruns, 0);
        let last = *fills.last().unwrap();
        assert!(last.abs_diff(target) <= 240, "{fills:?}");
        assert_eq!(sink.dry_frames, 0.0);
    }

    /// clocks-latency/F2 with the backlog: a 60 ms mic stall ran the device dry, then six blocks
    /// arrive together. The first is written at once (the device needs it), the excess is dropped
    /// from the blocks after it, and the wake ends within half a block of the usual fill.
    #[test]
    fn a_burst_after_a_mic_stall_drops_its_excess() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        started(&mut p, &mut sink, &mut mic, &tel);
        sink.advance(2880.0, 600_000.0);
        assert!(sink.dry_frames > 0.0);
        for b in 1..7usize {
            mic.push((b * 480) as u64, None, ramp(b * 480, 480));
        }
        p.drain_mic(&mut mic, 0.0).unwrap();
        let before = sink.written.len();
        while p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {}
        assert_eq!(p.counters.render_underruns, 1, "it did run dry");
        assert_eq!(p.counters.output_drops, 1);
        let usual = (p.out_target() + 480) as f64;
        assert!((sink.padding - usual).abs() <= 240.0, "{} vs {usual}", sink.padding);
        // The first queued block went out first, unpadded.
        assert_eq!(&sink.written[before..before + 480], &ramp(480, 480)[..]);
    }

    #[test]
    fn overfill_drops_whole_blocks() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(9600);
        let tel = Telemetry::new();
        // The device holds target + 5 blocks.
        sink.padding = (p.out_target() + 5 * 480) as f64;
        let mut mic = FakeCapture::default();
        mic.push(0, None, vec![0.25; 480]);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        assert_eq!(p.counters.output_drops, 1);
        assert!(sink.written.is_empty(), "this block was dropped");
        assert_eq!(p.out_drift.resets, 1);
        // Back in range: blocks flow again.
        sink.padding = p.out_target() as f64;
        mic.push(480, None, vec![0.25; 480]);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        assert_eq!(sink.written.len(), 480);
    }

    #[test]
    fn a_full_device_buffer_keeps_samples_pending() {
        let mut p = pipe(false);
        // A device buffer with room for only part of a block beyond the target.
        let mut sink = FakeSink::new(1200);
        let tel = Telemetry::new();
        sink.padding = 1056.0;
        let mut mic = FakeCapture::default();
        mic.push(0, None, ramp(0, 480));
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        assert_eq!(sink.written.len(), 1200 - 1056, "wrote what fit");
        assert_eq!(p.pending_len, 480 - 144);
        // The device plays a period; the rest goes out first, then the next block.
        sink.advance(480.0, 100_000.0);
        mic.push(480, None, ramp(480, 480));
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        let expect = ramp(0, sink.written.len());
        for (a, b) in sink.written.iter().zip(&expect) {
            assert!((a - b).abs() < 2e-9);
        }
    }

    #[test]
    fn write_errors_surface_as_hresults() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        sink.padding = 1056.0;
        sink.fail_write = Some(0x8889_0004_u32 as i32);
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        mic.push(0, None, vec![0.0; 480]);
        p.drain_mic(&mut mic, 0.0).unwrap();
        let r = p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false);
        assert_eq!(r, Err(0x8889_0004_u32 as i32));
    }

    /// No loopback packets at all (silence without the keep-alive): each block's reference is
    /// dithered zeros and a gap, and nothing is buffered or owed: the timeline starts with the
    /// first packet. (It used to pad the FIFO up to its target.)
    #[test]
    fn reference_gaps_are_zero_filled_and_dithered() {
        let mut p = pipe(true);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut spy = RefSpy(Vec::new());
        let mut mic = FakeCapture::default();
        for b in 0..2usize {
            mic.push((b * 480) as u64, None, vec![0.0; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        }
        assert_eq!(p.counters.reference_gaps, 2);
        assert_eq!(p.ref_len(), 0);
        assert!(spy.0.iter().all(|v| v.abs() <= 1.0001e-9) && spy.0.iter().any(|&v| v != 0.0), "dithered silence");
    }

    /// Runs `blocks` stamped 10 ms wakes with the loopback 20 ms ahead of the mic. `lb_at(b)`
    /// says whether the loopback delivers at wake `b` (everything it has tapped by the mic
    /// block's end when it does); `mic_lost` is a span of mic packets that never arrive. Returns
    /// the pipeline and the reference index each block started at (`None` for all zeros).
    fn stamped_run(
        blocks: usize, lb_at: impl Fn(usize) -> bool, mic_lost: std::ops::Range<usize>,
    ) -> (Pipeline<FakeDrift, FakeResampler>, Vec<Option<f64>>) {
        let mut p = pipe(true);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut spy = RefSpy(Vec::new());
        let (mut mic, mut lb) = (FakeCapture::default(), FakeCapture::default());
        let (t, h) = (1e10, 1e7 / 48_000.0);
        let mut sent = 0usize;
        for b in 0..blocks {
            let now = t + (b + 1) as f64 * 100_000.0 + 5_000.0;
            if !mic_lost.contains(&b) {
                mic.push((b * 480) as u64, Some((t + (b * 480) as f64 * h) as u64), vec![0.0; 480]);
            }
            if lb_at(b) {
                let to = 960 + (b + 1) * 480;
                push_tapped(&mut lb, sent, to, t - 960.0 * h);
                sent = to;
            }
            p.drain_mic(&mut mic, now).unwrap();
            if p.block_ready() {
                p.drain_reference(&mut lb, now).unwrap();
            }
            while p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap() {}
            sink.advance(480.0, 100_000.0);
        }
        let starts = spy.0.chunks(480).map(|blk| blk.iter().position(|&v| v >= 0.5).map(|k| (blk[k] - 1.0) as f64 - k as f64)).collect();
        (p, starts)
    }

    /// A loopback that stalls for 40 ms after the start-up and then delivers it all at once moves
    /// nothing: the blocks it was late for get zeros (three gaps), its late samples are discarded,
    /// and the next block carries on where the timeline is. In order throughout, no jumps or slips.
    #[test]
    fn a_late_loopback_is_owed_not_shifted() {
        let (p, starts) = stamped_run(400, |b| !(350..354).contains(&b), 0..0);
        for (b, s) in starts.iter().enumerate() {
            match b {
                // Partly served, then all owed.
                351..354 => assert!(s.is_none_or(|s| s == 384.0 + 480.0 * b as f64), "block {b}: {s:?}"),
                _ => assert_eq!(*s, Some(384.0 + 480.0 * b as f64), "block {b}"),
            }
        }
        let c = p.counters;
        assert_eq!((c.reference_gaps, c.reference_drops, c.reference_slips), (3, 0, 0), "{c:?}");
        assert_eq!(p.ref_target(), cfg(true).ref_target(), "one stall doesn't raise the target");
    }

    /// clocks-latency/F2, now on the stamps: 200 ms of mic packets lost after the start-up (too
    /// many to zero-fill) put the mic's stamps 200 ms ahead of its samples, a broken timeline.
    /// One jump (counted) discards the reference that played meanwhile, and the echo path is back
    /// where it was. (Coarse corrections used to read the FIFO's fill, past two blocks over.)
    #[test]
    fn a_broken_timeline_is_one_jump() {
        let (p, starts) = stamped_run(450, |_| true, 350..370);
        let lag = |b: usize, mic_block: usize| starts[b].map(|r| (mic_block * 480) as f64 - r);
        // Before: 576 - 960 = -384 samples between the streams' indices.
        assert_eq!(lag(349, 349), Some(-384.0));
        // After the loss the mic's block 370 is the pipeline's block 350; five blocks later it jumps.
        assert_eq!(starts.len(), 430);
        assert_eq!(lag(429, 449), Some(-384.0));
        assert_eq!(p.counters.reference_drops, 1);
        let moves = (300..429).filter(|&b| starts[b + 1].zip(starts[b]).is_none_or(|(n, o)| n - o != 480.0)).count();
        assert_eq!(moves, 1, "one jump");
    }

    /// wasapi/B: once the reference is dropped, the chain gets dithered zeros with no gaps counted,
    /// and the reference telemetry reads 0.
    #[test]
    fn a_dropped_reference_serves_dithered_silence() {
        let mut p = pipe(true);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut spy = RefSpy(Vec::new());
        let mut mic = FakeCapture::default();
        let mut lb = FakeCapture::default();
        lb.push_frames(1440, 0.5);
        p.drain_reference(&mut lb, 0.0).unwrap();
        mic.push(0, None, vec![0.0; 480]);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        assert!(spy.0.iter().all(|&v| (v - 0.5).abs() < 1e-6));
        p.publish_second(&tel, &spy, 0, 0, 0);
        assert!(tel.reference_fill_ms.load() > 0.0);
        p.drop_reference();
        spy.0.clear();
        for b in 1..20usize {
            mic.push((b * 480) as u64, None, vec![0.0; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            sink.advance(480.0, 100_000.0);
        }
        assert!(spy.0.iter().all(|v| v.abs() <= 1.0001e-9) && spy.0.iter().any(|&v| v != 0.0), "dithered silence");
        assert_eq!(p.counters.reference_gaps, 0);
        assert_eq!(p.ref_len(), 0);
        p.publish_second(&tel, &spy, 0, 0, 0);
        assert_eq!((tel.reference_fill_ms.load(), tel.reference_drift_ppm.load()), (0.0, 0.0));
        assert_eq!(p.counters.render_underruns, 0, "the output carried on");
    }

    /// Simulates a mic at 48 kHz against a cable that consumes `ppm` faster than the mic produces
    /// (and a loopback `ref_ppm` faster), with 10 ms packets, for `seconds`.
    fn simulate(ppm: f64, ref_ppm: f64, seconds: usize) -> (Pipeline<FakeDrift, FakeResampler>, FakeSink, Telemetry) {
        simulate_pipe(pipe(true), ppm, ref_ppm, seconds)
    }

    fn simulate_pipe<D: DriftControl, R: Resampler>(
        mut p: Pipeline<D, R>, ppm: f64, ref_ppm: f64, seconds: usize,
    ) -> (Pipeline<D, R>, FakeSink, Telemetry) {
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        let mut lb = FakeCapture::default();
        let mut ticks = FakeTicks(0);
        let mut chain = Chain::new(Arc::new(ChainControls::new(&ProcessingConfig::default())), None, None);
        chain.activate().unwrap();
        // The loopback delivers whole stamped 480-frame packets on its own clock, starting a
        // third of a period out of phase with the mic.
        let packet_hns = 480.0 * 1e7 / (48_000.0 * (1.0 + ref_ppm * 1e-6));
        let mut ref_end = sink.now_hns - 100_000.0 + packet_hns / 3.0;
        let mut ref_pos = 0u64;
        for b in 0..(seconds * 100) {
            let now = sink.now_hns;
            mic.push((b * 480) as u64, Some((now - 100_000.0) as u64), ramp(b * 480, 480));
            while ref_end <= now {
                lb.push(ref_pos, Some((ref_end - packet_hns) as u64), vec![0.0; 480]);
                ref_pos += 480;
                ref_end += packet_hns;
            }
            p.drain_mic(&mut mic, now).unwrap();
            p.drain_reference(&mut lb, now).unwrap();
            while p.process_block(&mut chain, &mut sink, &mut ticks, &tel, true).unwrap() {
                if p.second_due() {
                    p.publish_second(&tel, &chain, 0, 0, 0);
                }
            }
            sink.advance(480.0 * (1.0 + ppm * 1e-6), 100_000.0);
        }
        (p, sink, tel)
    }

    #[test]
    fn drift_is_tracked_by_slips_with_a_bounded_fill() {
        for ppm in [-300.0, 300.0, 1000.0] {
            let (p, sink, tel) = simulate(ppm, -ppm / 2.0, 120);
            assert_eq!(p.counters.render_underruns, 0, "{ppm} ppm");
            assert_eq!(sink.dry_frames, 0.0);
            // The second minute: the slip rate equals the drift (within 10%).
            let expect = (ppm.abs() * 1e-6 * 48_000.0 * 120.0) as u64;
            let slips = p.counters.output_slips;
            assert!(slips as f64 > expect as f64 * 0.8 && (slips as f64) < expect as f64 * 1.1, "{ppm}: {slips} slips vs {expect}");
            // The fill stays near the target.
            let fill_ms = tel.output_fill_ms.load() as f64;
            let target_ms = p.out_target() as f64 / 48.0;
            assert!((fill_ms - target_ms).abs() < 8.0, "{ppm}: fill {fill_ms} ms vs target {target_ms}");
            assert!(tel.latency_valid.load(Relaxed));
            assert!(p.counters.reference_gaps <= 1, "{ppm}: at most the start-up pad: {:?}", p.counters);
            assert!(p.counters.reference_slips > 0);
            // A faster cable drains the output FIFO: negative ppm (it fills slower than we drain).
            let got = tel.output_drift_ppm.load() as f64;
            assert!((got + ppm).abs() < ppm.abs() * 0.3, "{ppm}: reported {got}");
        }
    }

    /// The same scenario with nf-core's real drift controller and slip resampler. Ignored until
    /// those are settled (they were being written in parallel); run with `--ignored`.
    #[test]
    #[ignore]
    fn real_drift_loop_and_resampler() {
        for ppm in [-1000.0, -300.0, 50.0, 300.0, 1000.0] {
            let c = cfg(true);
            let p = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
            let (p, sink, tel) = simulate_pipe(p, ppm, -ppm / 2.0, 120);
            let k = p.counters;
            assert_eq!(k.render_underruns, 0, "{ppm}: {k:?}");
            assert_eq!(sink.dry_frames, 0.0, "{ppm}");
            assert!(k.reference_gaps <= 1, "{ppm}: {k:?}");
            let expect = ppm.abs() * 1e-6 * 48_000.0 * 120.0;
            assert!((k.output_slips as f64 - expect).abs() < expect * 0.1 + 40.0, "{ppm}: {} slips vs {expect}", k.output_slips);
            let fill_ms = tel.output_fill_ms.load() as f64;
            let target_ms = c.out_target() as f64 / 48.0;
            assert!((fill_ms - target_ms).abs() < 2.0, "{ppm}: fill {fill_ms} vs {target_ms}");
            let reported = tel.output_drift_ppm.load() as f64;
            assert!((reported + ppm).abs() < ppm.abs() * 0.1 + 20.0, "{ppm}: reported {reported}");
        }
    }

    #[test]
    fn readout_holds_steady_under_drift() {
        let (_, _, tel) = simulate(300.0, 0.0, 60);
        let med = tel.latency_ms_median.load();
        let p95 = tel.latency_ms_p95.load();
        // 10 ms of packet + about one target of queue (22 ms), give or take a period.
        assert!(med > 25.0 && med < 45.0, "median {med}");
        assert!(p95 >= med && p95 < med + 12.0, "p95 {p95}");
    }

    #[test]
    fn tap_carries_the_raw_mic_with_its_capture_time() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let (tx, mut rx) = rtrb::RingBuffer::<TapItem>::new(2);
        p.tap = Some(tx);
        let mut mic = FakeCapture::default();
        for b in 0..3usize {
            mic.push((b * 480) as u64, Some(1_000_000_000 + b as u64 * 100_000), ramp(b * 480, 480));
            p.drain_mic(&mut mic, 1e9).unwrap();
            p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        }
        assert_eq!(p.counters.tap_overflows, 1, "capacity 2, three blocks");
        let a = rx.pop().unwrap();
        assert_eq!(a.qpc_hns, 1e9);
        assert_eq!(a.samples.to_vec(), ramp(0, 480), "undithered");
        let b = rx.pop().unwrap();
        assert_eq!(b.qpc_hns, 1e9 + 100_000.0);
        // Unknown stamps travel as NaN.
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let (tx, mut rx) = rtrb::RingBuffer::<TapItem>::new(1);
        p.tap = Some(tx);
        mic.push(0, None, vec![0.0; 480]);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut CopyChain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        assert!(rx.pop().unwrap().qpc_hns.is_nan());
    }

    #[test]
    fn per_second_publish() {
        let mut p = pipe(true);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut spy = RefSpy(Vec::new());
        let mut mic = FakeCapture::default();
        for b in 0..100usize {
            mic.push((b * 480) as u64, None, vec![0.0; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut spy, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            sink.advance(480.0, 100_000.0);
        }
        assert!(p.second_due());
        p.publish_second(&tel, &spy, 1_000, 7, 5);
        let s = tel.snapshot();
        assert_eq!(s.rt_heap_allocations, 2, "growth over the baseline");
        assert_eq!(s.rt_pool_allocations_per_s, 0, "no previous second yet");
        assert!(!s.latency_valid, "readout off");
        assert_eq!(s.aec_delay_ms, 52.0);
        assert_eq!(s.aec_erle_db, 12.0);
        // FakeTicks: 10 ticks of 0.1 us between the block's two readings.
        assert!((s.block_us_p50 - 1.0).abs() < 1e-6 && (s.block_us_max - 1.0).abs() < 1e-6);
        assert_eq!(s.aec_us_p99, 0.0, "no AEC ticks: 0, not NaN");
        assert!(s.output_fill_ms > 0.0);
        assert!(s.reference_gaps >= 1);
        p.publish_second(&tel, &spy, 1_043, 7, 5);
        assert_eq!(tel.rt_pool_allocations_per_s.load(Relaxed), 43);
        // Seeded from the warm-up count, the first second reports its own growth.
        let mut p = pipe(false);
        p.set_alloc_baseline(500);
        p.publish_second(&tel, &spy, 543, 0, 0);
        assert_eq!(tel.rt_pool_allocations_per_s.load(Relaxed), 43);
    }

    /// A mic or loopback that makes one stamped packet per wake-up without allocating.
    struct GenCapture {
        pos: u64,
        ready: bool,
        stamp_hns: f64,
    }

    impl CaptureSource for GenCapture {
        fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<Packet>, Hr> {
            if !self.ready {
                return Ok(None);
            }
            self.ready = false;
            for (i, o) in out[..480].iter_mut().enumerate() {
                *o = ((self.pos as usize + i) % 97) as f32 * 0.01;
            }
            let p = Packet { frames: 480, device_pos: self.pos, qpc_hns: Some(self.stamp_hns as u64), discontinuity: false };
            self.pos += 480;
            Ok(Some(p))
        }
    }

    /// A render device that plays a period per wake and keeps nothing.
    struct NullSink {
        padding: usize,
        played: f64,
        now_hns: f64,
    }

    impl RenderSink for NullSink {
        fn buffer_frames(&self) -> usize {
            4800
        }
        fn padding(&mut self) -> Result<usize, Hr> {
            Ok(self.padding)
        }
        fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
            self.padding += mono.len();
            Ok(())
        }
        fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
            self.padding += frames;
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            Ok(Some(ClockReading { pos_frames: self.played, qpc_hns: self.now_hns, now_hns: self.now_hns }))
        }
    }

    /// Runs `blocks` wake-ups (mic, loopback, every block, the per-second publish, the tap and the
    /// chain recorder, its ring full for a while) and returns the allocations and frees they made.
    fn wake_ups<D: DriftControl, R: Resampler>(p: &mut Pipeline<D, R>, blocks: usize, warm: usize) -> (u64, u64) {
        let tel = Telemetry::new();
        let mut chain = Chain::new(Arc::new(ChainControls::new(&ProcessingConfig::default())), None, None);
        chain.activate().unwrap();
        let mut sink = NullSink { padding: 0, played: 0.0, now_hns: 1e10 };
        p.prefill(&mut sink).unwrap();
        let mut mic = GenCapture { pos: 0, ready: false, stamp_hns: 0.0 };
        let mut lb = GenCapture { pos: 0, ready: false, stamp_hns: 0.0 };
        let (tx, mut rx) = rtrb::RingBuffer::<TapItem>::new(16);
        p.tap = Some(tx);
        let (rec_tx, mut rec_rx) = rtrb::RingBuffer::<crate::recorder::ChainItem>::new(16);
        let rec_tx = ChainTx::new(rec_tx);
        let lost = rec_tx.lost();
        p.recorder = Some(rec_tx);
        let (mut recorded, mut wake) = (0u64, 0usize);
        let mut ticks = FakeTicks(0);
        let mut one = |p: &mut Pipeline<D, R>| {
            let now = sink.now_hns;
            mic.ready = true;
            mic.stamp_hns = now - 100_000.0;
            lb.ready = true;
            lb.stamp_hns = now - 100_000.0;
            p.drain_mic(&mut mic, now).unwrap();
            p.drain_reference(&mut lb, now).unwrap();
            while p.process_block(&mut chain, &mut sink, &mut ticks, &tel, true).unwrap() {
                if p.second_due() {
                    p.publish_second(&tel, &chain, 0, 0, 0);
                }
            }
            // The tap's and the recorder's consumers keep up (the latency test's thread and the
            // recorder's, here inline), but for 40 wakes when the recorder's doesn't: its ring fills
            // and blocks are lost, counted.
            while rx.pop().is_ok() {}
            wake += 1;
            while !(warm + 300..warm + 340).contains(&wake) && rec_rx.pop().is_ok() {
                recorded += 1;
            }
            sink.padding = sink.padding.saturating_sub(480);
            sink.played += 480.0;
            sink.now_hns += 100_000.0;
        };
        for _ in 0..warm {
            one(p);
        }
        let ((), allocs, frees) = crate::test_alloc::count(|| {
            for _ in 0..blocks {
                one(p);
            }
        });
        drop(one);
        let lost = lost.load(std::sync::atomic::Ordering::Acquire);
        assert!(lost > 0 || blocks < 340, "the ring was full for a while");
        assert_eq!(recorded + lost, (warm + blocks) as u64, "every block recorded or counted lost");
        (allocs, frees)
    }

    #[test]
    fn the_audio_path_never_allocates() {
        let mut p = pipe(true);
        let (allocs, frees) = wake_ups(&mut p, 1_000, 10);
        assert_eq!((allocs, frees), (0, 0), "allocations and frees in 1000 wake-ups");
        assert!(p.counters.reference_gaps <= 1 && p.counters.render_underruns == 0, "{:?}", p.counters);
    }

    /// The same with nf-core's real drift loop and resampler; run with `--ignored`.
    #[test]
    #[ignore]
    fn the_audio_path_never_allocates_with_the_real_drift_loop() {
        let c = cfg(true);
        let mut p = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
        assert_eq!(wake_ups(&mut p, 1_000, 10), (0, 0));
    }

    #[test]
    fn meters_and_clip_latch_reach_telemetry() {
        let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
        let mut chain = Chain::new(controls, None, None);
        chain.activate().unwrap();
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        let mut x = vec![0.1f32; 480];
        x[7] = 1.0;
        mic.push(0, None, x);
        p.drain_mic(&mut mic, 0.0).unwrap();
        p.process_block(&mut chain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
        let s = tel.snapshot();
        assert!(s.clip_latched && s.near_clip);
        assert!(s.input.peak_dbfs.abs() < 0.01);
        assert!((s.output.peak_dbfs - s.input.peak_dbfs).abs() < 0.01);
        // The near-clip flag covers the last second only.
        p.publish_second(&tel, &chain, 0, 0, 0);
        assert!(tel.near_clip.load(Relaxed));
        p.publish_second(&tel, &chain, 0, 0, 0);
        assert!(!tel.near_clip.load(Relaxed));
        assert!(tel.clip_latched.load(Relaxed), "the latch stays until reset");
    }

    /// Reports one meter reading per block (and optionally a noise-stage report).
    struct MeterChain {
        peak: f32,
        mean_square: f32,
        noise: Option<StageReport>,
    }

    impl BlockProcessor for MeterChain {
        fn process<T: Ticks>(&mut self, mic: &Block, _r: &Block, out: &mut Block, _t: &mut T) -> ChainReport {
            *out = *mic;
            let m = MeterReading { peak: self.peak, mean_square: self.mean_square, clipped: false, near_clip: false };
            ChainReport { input: Some(m), output: Some(m), noise: self.noise, ..ChainReport::default() }
        }
        fn stage_stats(&self) -> (Option<StageStats>, Option<StageStats>) {
            (None, None)
        }
    }

    /// DSP-3 / gui/F4: a 10 ms plosive at -1 dBFS between quiet blocks stays on the published peak
    /// for the hold (1 s), whenever a reader samples it; the RMS is an average, not the last block.
    #[test]
    fn published_meters_hold_peaks_and_average_the_rms() {
        let mut p = pipe(false);
        let mut sink = FakeSink::new(4800);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = FakeCapture::default();
        let loud = 10f32.powf(-1.0 / 20.0);
        let mut chain = MeterChain { peak: 0.01, mean_square: 1e-4, noise: None };
        let mut peaks = Vec::new();
        let mut rms = Vec::new();
        for b in 0..150usize {
            chain.peak = if b == 20 { loud } else { 0.01 };
            chain.mean_square = if b == 20 { 0.25 } else { 1e-4 };
            mic.push((b * 480) as u64, None, vec![0.0; 480]);
            p.drain_mic(&mut mic, 0.0).unwrap();
            p.process_block(&mut chain, &mut sink, &mut FakeTicks(0), &tel, false).unwrap();
            sink.advance(480.0, 100_000.0);
            let s = tel.snapshot();
            peaks.push(s.input.peak_dbfs);
            rms.push(s.output.rms_dbfs);
        }
        // Held for a second after the plosive, at every block a reader could sample...
        for (b, &pk) in peaks.iter().enumerate().take(121).skip(20) {
            assert!((pk + 1.0).abs() < 1e-3, "block {b}: {pk}");
        }
        // ...then falling at 20 dB/s.
        assert!(peaks[131] < -1.1 && peaks[131] > -4.0, "{}", peaks[131]);
        assert!(peaks[149] < peaks[131]);
        // The quiet RMS is -40 dBFS; the plosive lifts the average without jumping to -6.
        assert!((rms[10] + 40.0).abs() < 0.01, "{}", rms[10]);
        assert!(rms[20] > -40.0 && rms[20] < -15.0, "{}", rms[20]);
        assert!(rms[21] > -40.0 && rms[21] < rms[20], "decays, not the last block: {}", rms[21]);
        // Meters off: the ballistics restart, so turning them back on shows the present level and
        // not the held plosive peak (the publish does the reset; nothing here does it by hand).
        let r = MeterReading { peak: 0.01, mean_square: 1e-4, clipped: false, near_clip: false };
        p.publish_block(&tel, &ChainReport { input: None, ..ChainReport::default() });
        p.publish_block(&tel, &ChainReport { input: Some(r), ..ChainReport::default() });
        assert!((tel.input.load().peak_dbfs + 40.0).abs() < 1e-3);
    }

    /// DSP-4: speech and reduction read NaN ("—") while the noise stage isn't reporting (turned off,
    /// or in passthrough), instead of freezing at the last values.
    #[test]
    fn noise_readouts_clear_when_the_stage_stops_reporting() {
        let mut p = pipe(false);
        let tel = Telemetry::new();
        let report = |noise| ChainReport { noise, ..ChainReport::default() };
        p.publish_block(&tel, &report(Some(StageReport { speech_probability: 0.93, gain_reduction_db: -3.0 })));
        assert_eq!((tel.speech_probability.load(), tel.noise_reduction_db.load()), (0.93, -3.0));
        p.publish_block(&tel, &report(None));
        assert!(tel.speech_probability.load().is_nan() && tel.noise_reduction_db.load().is_nan());
    }

    /// A render device that takes a whole period from the buffer at each of its own period ticks and
    /// plays it out over that period, with a clock whose position advances in period steps (stamped
    /// at the tick), like an engine that updates its position once a pass. It runs `ppm` faster than
    /// the mic. `clock: false` reports no position (the fill falls back to the padding).
    struct StepSink {
        padding: usize,
        ticks: u64,
        tick_hns: f64,
        next_tick: f64,
        last_tick: f64,
        now_hns: f64,
        written: u64,
        dry: u64,
        clock: bool,
    }

    impl StepSink {
        fn new(ppm: f64, phase: f64, now: f64, clock: bool) -> Self {
            let tick_hns = 480.0 * 1e7 / (48_000.0 * (1.0 + ppm * 1e-6));
            let next_tick = now + phase * tick_hns;
            Self { padding: 0, ticks: 0, tick_hns, next_tick, last_tick: now, now_hns: now, written: 0, dry: 0, clock }
        }
        fn advance_to(&mut self, t: f64) {
            while self.next_tick <= t {
                let take = self.padding.min(480);
                self.dry += (480 - take) as u64;
                self.padding -= take;
                self.ticks += 1;
                self.last_tick = self.next_tick;
                self.next_tick += self.tick_hns;
            }
            self.now_hns = t;
        }
        /// Frames played by now, exactly.
        fn played_now(&self) -> f64 {
            match self.ticks {
                0 => 0.0,
                k => ((k - 1) * 480) as f64 + (self.now_hns - self.last_tick) / self.tick_hns * 480.0,
            }
        }
    }

    impl RenderSink for StepSink {
        fn buffer_frames(&self) -> usize {
            4800
        }
        fn padding(&mut self) -> Result<usize, Hr> {
            Ok(self.padding)
        }
        fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
            self.write_silence(mono.len())
        }
        fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
            assert!(self.padding + frames <= 4800, "must fit");
            self.padding += frames;
            self.written += frames as u64;
            Ok(())
        }
        fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
            if !self.clock || self.ticks == 0 {
                return Ok(None);
            }
            Ok(Some(ClockReading { pos_frames: ((self.ticks - 1) * 480) as f64, qpc_hns: self.last_tick, now_hns: self.now_hns }))
        }
    }

    /// The output side against a [`StepSink`], with nf-core's real drift loop and resampler and the
    /// mic waking every 10 ms. Returns each block's true latency in ms (frames queued ahead of it
    /// by the device's real position) from `settle` seconds on, with the pipeline and the sink.
    fn simulate_stepped(ppm: f64, clock: bool, seconds: usize, settle: usize) -> (Vec<f64>, Pipeline, StepSink) {
        let c = cfg(false);
        let mut p = Pipeline::new(c, DriftController::new(c.out_target() as f32), SlipResampler::new(), SlipResampler::new());
        let t0 = 1e10;
        let mut sink = StepSink::new(ppm, 0.3, t0, clock);
        p.prefill(&mut sink).unwrap();
        let tel = Telemetry::new();
        let mut mic = GenCapture { pos: 0, ready: false, stamp_hns: 0.0 };
        let mut ticks = FakeTicks(0);
        let mut lat = Vec::new();
        for b in 0..seconds * 100 {
            let now = t0 + (b + 1) as f64 * 100_000.0;
            sink.advance_to(now);
            mic.ready = true;
            mic.stamp_hns = now - 100_000.0;
            p.drain_mic(&mut mic, now).unwrap();
            if b >= settle * 100 {
                lat.push(((sink.written + p.pending_len as u64) as f64 - sink.played_now()) / 48.0);
            }
            while p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, clock).unwrap() {}
        }
        (lat, p, sink)
    }

    fn spread(v: &[f64]) -> f64 {
        let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        hi - lo
    }

    /// KNOWN ITEM, output fill interpolation: against a device whose padding and clock move a period
    /// at a time, the padding alone beats against the mic's wakes, and the loop lets the latency
    /// wander by about a packet over each beat. Frames written minus the interpolated position is
    /// smooth, so the latency holds still to well under 1 ms, with the padding still centred on the
    /// target and no underruns.
    #[test]
    fn the_interpolated_fill_holds_the_latency_still_against_a_stepped_clock() {
        // (ppm, the least wander the padding alone shows: the beat at 1000 ppm, 10 s, is too fast
        // for the loop to follow far).
        stepped_clock_wander(&[(300.0, 5.0), (-300.0, 5.0), (1000.0, 1.5)], 150, 70);
    }

    /// The same at 50 ppm, whose beat is 200 s; run with `--ignored`.
    #[test]
    #[ignore]
    fn the_interpolated_fill_holds_the_latency_still_at_slow_drift() {
        stepped_clock_wander(&[(50.0, 8.0), (-50.0, 8.0)], 700, 250);
    }

    fn stepped_clock_wander(cases: &[(f64, f64)], seconds: usize, settle: usize) {
        for &(ppm, min_off) in cases {
            let (on, p_on, s_on) = simulate_stepped(ppm, true, seconds, settle);
            let (off, p_off, s_off) = simulate_stepped(ppm, false, seconds, settle);
            let (w_on, w_off) = (spread(&on), spread(&off));
            eprintln!("{ppm} ppm: latency wander {w_on:.3} ms interpolated, {w_off:.3} ms on the padding alone");
            assert!(w_on < 0.1, "{ppm} ppm: interpolated wander {w_on:.3} ms");
            assert!(w_off > min_off, "{ppm} ppm: padding-only wander {w_off:.3} ms");
            for (p, s) in [(&p_on, &s_on), (&p_off, &s_off)] {
                assert_eq!((p.counters.render_underruns, s.dry), (0, 0), "{ppm} ppm: {:?}", p.counters);
            }
            // The slips still match the drift (150 s at 300 ppm: 2160).
            let expect = ppm.abs() * 1e-6 * 48_000.0 * seconds as f64;
            let slips = p_on.counters.output_slips as f64;
            assert!((slips - expect).abs() < expect * 0.1 + 40.0, "{ppm}: {slips} slips vs {expect}");
        }
    }

    mod events;
    mod recorder;
    mod timeline;
}
