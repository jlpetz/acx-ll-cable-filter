//! The gate that ends the noise stage (TODO #11): static or auto threshold, hold, linear-in-dB ramps.
//!
//! Contract:
//! - Detector: the block's RMS in dBFS of the signal being gated (the model's output, floor mixed).
//! - Open when: Static, `rms_db >= threshold_db`; Auto, `rms_db >= noise_floor_db + auto_margin_db`;
//!   or, if `use_vad` and `speech_probability` is a number, `speech_probability >= 0.5`.
//! - Close only after the condition has been false for `hold_ms`. While open, the level condition
//!   has 3 dB of hysteresis: it still holds down to 3 dB below the open threshold.
//! - Ramps are linear in dB. `attack_ms` is the time from `range_db` up to 0 dB, and `release_ms` the
//!   time from 0 dB down to `range_db`, so `GateConfig`'s "time to close fully" is literal. The dB
//!   value is computed at the block's start and end, and at the sample where a ramp reaches its
//!   target inside the block; `powf` runs only at those points, and the per-sample gain interpolates
//!   linear amplitude between them (index-derived, so it vectorises). No per-sample `exp` or `powf`,
//!   and no denormal gains (the closed gain is at least -120 dB).
//! - Auto noise floor, tracked in every mode so switching to Auto starts warm:
//!   - It follows a power-domain smoothed level: a one-pole on the block mean square, τ = 100 ms.
//!   - It falls towards that level quickly (a one-pole in dB, τ = 200 ms) and rises slowly, 3 dB/s
//!     while closed and 1 dB/s while open, never above the level.
//!   - Blocks below -140 dBFS (digital silence and the engine's dither) are ignored, and so are
//!     non-finite ones (an infinite block would leave the smoothed level NaN for good).
//!   - After `new`, `reset` or 50 silent blocks (0.5 s) in a row it acquires: it rises at
//!     25 dB/s until blocks have stayed within 3 dB of the floor for `hold_ms`, and at least
//!     100 ms (a hold of 0-10 ms let one or two quiet start-up blocks, the chain's priming block or
//!     RNNoise's first output, end it with the floor 20 dB low: seconds of ungated noise). At the default 6 dB
//!     margin that is the open condition (with its hysteresis) failing for the hold, so the Auto
//!     gate's first close. Three departures from "the first block where the open condition is
//!     false", each found on RNNoise output: one block isn't enough
//!     (RNNoise's first output block ended it with the floor 30 dB low, left to rise at 1 dB/s); the
//!     VAD doesn't count (it fires on noise at 0 dB SNR, which dragged the floor to speech level);
//!     and the distance is fixed rather than the margin, or a larger margin would stop the floor
//!     further below the noise and cancel itself.
//!   - It starts at -90 dBFS and is clamped to [-120, -20] dBFS.
//!   - "Open" for the rise rate is the gate's state (hold included) in Auto mode, and the Auto
//!     condition (without hysteresis) in the other modes, but only while the model doesn't report
//!     "not speech" (a probability under 0.5, whatever `use_vad` says). Open on level alone with
//!     the model saying no speech is residual noise above a floor that lags it: RNNoise's residual
//!     stays 5-10 dB up for a second or two after each talk spurt, and at 1 dB/s the gate stayed
//!     open through 2 s gaps (closed fully in 3 of 8 of nf-stages' gaps; 5 of 8 at 3 dB/s).
//! - Mode Off: unity gain. Switched off while closed, it ramps back to unity over 5 ms, then leaves
//!   the signal bit-exact.
//! - Real-time: no allocation; the per-sample loops vectorise (doc/dev/api/hot-loop-tools.md R2).
//!
//! Why the floor follows a smoothed level rather than block minima: a denoiser's residual swings
//! by more than the default 6 dB margin from block to block, so a minimum tracker sat at the
//! quietest blocks and the margin never closed the gate. The smoothed power level sits at the
//! residual's mean, and the margin then measures speech against that.

use crate::config::GateMode;
use crate::controls::GateParams;
use crate::dsp::meter::mean_square;
use crate::{BLOCK, Block, SAMPLE_RATE, db_to_lin, ms_to_samples};

/// The auto noise floor's start and clamp, in dBFS.
const FLOOR_START_DB: f32 = -90.0;
const FLOOR_MIN_DB: f32 = -120.0;
const FLOOR_MAX_DB: f32 = -20.0;
/// Blocks quieter than this (dBFS) are digital silence or dither: the floor ignores them.
const FLOOR_SILENCE_DB: f32 = -140.0;
/// This many silent blocks in a row (0.5 s) make the floor re-acquire when the signal returns.
const SILENT_RUN_BLOCKS: u32 = 50;
const BLOCKS_PER_S: f32 = SAMPLE_RATE as f32 / BLOCK as f32;
/// The smoothed level's one-pole coefficient: 1 - exp(-10 ms / 100 ms).
const LEVEL_ALPHA: f32 = 0.095_162_58;
/// The floor's fall towards the level, as a one-pole in dB: 1 - exp(-10 ms / 200 ms).
const FALL_ALPHA: f32 = 0.048_770_576;
/// The floor's rise per block: 3 dB/s closed, 1 dB/s open (speech mustn't drag it up), 25 dB/s
/// while acquiring.
const RISE_CLOSED_DB: f32 = 3.0 / BLOCKS_PER_S;
const RISE_OPEN_DB: f32 = 1.0 / BLOCKS_PER_S;
const RISE_ACQUIRE_DB: f32 = 25.0 / BLOCKS_PER_S;
/// While open, the level condition holds down to this far below the open threshold.
const HYSTERESIS_DB: f32 = 3.0;
/// An acquisition ends once blocks have stayed within this of the floor for the hold time...
const ACQUIRED_WITHIN_DB: f32 = 3.0;
/// ...and for at least this many samples (100 ms), whatever the hold.
const ACQUIRE_MIN: f32 = SAMPLE_RATE as f32 * 0.1;
/// The deepest closed gain, in dB: keeps gains far from the denormal range.
const RANGE_MIN_DB: f32 = -120.0;
/// Off ramps the gain back to unity over this many samples (5 ms).
const OFF_RAMP: usize = 240;
const _: () = assert!(OFF_RAMP <= BLOCK);

/// What the gate did this block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateReport {
    /// Gain at the end of the block, in dB (<= 0).
    pub gain_db: f32,
    /// Whether the gate is open (the condition held, or the hold is running). Always true in Off.
    pub open: bool,
    /// Auto mode's tracked noise floor, in dBFS.
    pub noise_floor_db: f32,
}

/// Gate state carried across blocks.
#[derive(Clone, Debug)]
pub struct Gate {
    /// Gain at the end of the last block, in dB, and the same as a linear factor.
    gain_db: f32,
    gain: f32,
    /// Open (condition met, or holding).
    open: bool,
    /// Samples of hold left.
    hold_left: f32,
    /// The tracked noise floor, in dBFS.
    floor_db: f32,
    /// The smoothed block mean square the floor follows; `None` until a non-silent block.
    level: Option<f32>,
    /// Rising fast until blocks have stayed within 3 dB of the floor for the hold time.
    acquiring: bool,
    /// Samples of non-silent blocks in a row within 3 dB of the floor (ends an acquisition).
    unmet: f32,
    /// Silent blocks in a row.
    silent_run: u32,
    /// The last `range_db` seen (clamped) and its linear gain (one `powf` per change, not per block).
    range_db: f32,
    range_lin: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            gain_db: 0.0,
            gain: 1.0,
            open: true,
            hold_left: 0.0,
            floor_db: FLOOR_START_DB,
            level: None,
            acquiring: true,
            unmet: 0.0,
            silent_run: 0,
            range_db: 0.0,
            range_lin: 1.0,
        }
    }

    /// Gates `buf` in place. `speech_probability` is `NaN` when the model gives none.
    pub fn process(&mut self, buf: &mut Block, params: &GateParams, speech_probability: f32) -> GateReport {
        let ms = mean_square(buf);
        let rms_db = power_db(ms);
        // NaN compares false everywhere below: a NaN level or probability never opens the gate.
        let vad = params.use_vad && speech_probability >= 0.5;
        let auto_thr = self.floor_db + params.auto_margin_db;
        let hyst = if self.open { HYSTERESIS_DB } else { 0.0 };

        // The Auto level condition, with the hysteresis in Auto mode only (elsewhere the gate's state
        // isn't the Auto gate's).
        let auto_level = rms_db >= auto_thr - if params.mode == GateMode::Auto { hyst } else { 0.0 };
        // Still well above the floor: what keeps an acquisition going. A fixed distance, so the floor
        // it settles on doesn't depend on the margin, and the level only: a VAD that fires on noise
        // (RNNoise's does at 0 dB SNR) would otherwise drag the floor up to the speech level.
        let above_floor = rms_db >= self.floor_db + ACQUIRED_WITHIN_DB;
        let open = match params.mode {
            GateMode::Off => {
                self.hold_left = 0.0;
                self.open = true;
                true
            }
            GateMode::Static => self.hold(rms_db >= params.threshold_db - hyst || vad, params.hold_ms),
            GateMode::Auto => self.hold(auto_level || vad, params.hold_ms),
        };
        // Rises slowly while speech holds the gate open; open on level alone with the model saying
        // "not speech" (a probability under 0.5) is noise above a floor that lags it.
        let not_speech = speech_probability < 0.5;
        let busy = if params.mode == GateMode::Auto { open && !not_speech } else { (auto_level || vad) && !not_speech };
        self.track_floor(ms, rms_db, busy, above_floor, ms_to_samples(params.hold_ms));

        if params.mode == GateMode::Off {
            self.ramp_to_unity(buf);
        } else {
            let range_db = self.range(params.range_db);
            let target_db = if open { 0.0 } else { range_db };
            self.ramp(buf, target_db, params);
        }
        GateReport { gain_db: self.gain_db, open, noise_floor_db: self.floor_db }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Applies the hold to this block's condition and returns the open state.
    #[inline]
    fn hold(&mut self, met: bool, hold_ms: f32) -> bool {
        if met {
            let h = ms_to_samples(hold_ms);
            self.hold_left = if h > 0.0 { h } else { 0.0 };
            self.open = true;
        } else if self.hold_left > 0.0 {
            self.hold_left -= BLOCK as f32;
        } else {
            self.open = false;
        }
        self.open
    }

    /// Follows the smoothed level: falls fast, rises slowly (fast while acquiring), ignores silence.
    /// An acquisition ends once `met` (the block is well above the floor) has been false for `hold`
    /// samples of non-silent blocks, and at least [`ACQUIRE_MIN`].
    #[inline]
    fn track_floor(&mut self, ms: f32, rms_db: f32, busy: bool, met: bool, hold: f32) {
        if !ms.is_finite() {
            return;
        }
        if rms_db < FLOOR_SILENCE_DB {
            self.silent_run = self.silent_run.saturating_add(1);
            if self.silent_run >= SILENT_RUN_BLOCKS {
                // The level is stale after this long: start it afresh and re-acquire.
                self.level = None;
                self.acquiring = true;
                self.unmet = 0.0;
            }
            return;
        }
        self.silent_run = 0;
        if met {
            self.unmet = 0.0;
        } else {
            self.unmet += BLOCK as f32;
            // `max` takes the minimum for a NaN hold too.
            if self.unmet > hold.max(ACQUIRE_MIN) {
                self.acquiring = false;
            }
        }
        let level = match self.level {
            Some(l) => l + LEVEL_ALPHA * (ms - l),
            None => ms,
        };
        self.level = Some(level);
        let level_db = power_db(level);
        let f = self.floor_db;
        let f = if level_db < f {
            f + FALL_ALPHA * (level_db - f)
        } else {
            let rise = if self.acquiring {
                RISE_ACQUIRE_DB
            } else if busy {
                RISE_OPEN_DB
            } else {
                RISE_CLOSED_DB
            };
            (f + rise).min(level_db)
        };
        self.floor_db = f.clamp(FLOOR_MIN_DB, FLOOR_MAX_DB);
    }

    /// The closed gain in dB for `range_db`, clamped to [-120, 0] (above 0 or NaN: no attenuation),
    /// caching its linear value.
    #[inline]
    fn range(&mut self, range_db: f32) -> f32 {
        let r = if range_db < 0.0 { range_db.max(RANGE_MIN_DB) } else { 0.0 };
        if r.to_bits() != self.range_db.to_bits() {
            self.range_db = r;
            self.range_lin = db_to_lin(r);
        }
        r
    }

    /// The linear gain for a target in dB: the cached unity or range value, without `powf`.
    #[inline]
    fn target_lin(&self, target_db: f32) -> f32 {
        if target_db == 0.0 { 1.0 } else { self.range_lin }
    }

    /// Moves the gain towards `target_db` in dB at the attack (up) or release (down) rate.
    #[inline]
    fn ramp(&mut self, buf: &mut Block, target_db: f32, params: &GateParams) {
        let d0 = self.gain_db;
        if d0 == target_db {
            let g = self.gain;
            if g != 1.0 {
                for s in buf.iter_mut() {
                    *s *= g;
                }
            }
            return;
        }
        let up = target_db > d0;
        let samples = ms_to_samples(if up { params.attack_ms } else { params.release_ms });
        // The full swing, so attack is range -> 0 and release 0 -> range. A range moved while
        // closed is reached at the rate of the larger of the two spans, so the ramp never stalls.
        let span = -(self.range_db.min(d0));
        let dist = (target_db - d0).abs();
        let (end_db, g1, n) = if samples.is_nan() || samples < 1.0 || span <= 0.0 {
            (target_db, self.target_lin(target_db), 0.0)
        } else {
            let need = dist * samples / span; // samples to reach the target
            // The slack absorbs rounding in the dB steps, so a ramp lands on its target exactly.
            if need <= BLOCK as f32 + 0.01 {
                (target_db, self.target_lin(target_db), need.min(BLOCK as f32))
            } else {
                let step = span / samples * BLOCK as f32;
                let end = if up { d0 + step } else { d0 - step };
                (end, db_to_lin(end), BLOCK as f32)
            }
        };
        let g0 = self.gain;
        if n < 1.0 {
            // Instant: the whole block at the target.
            if g1 != 1.0 {
                for s in buf.iter_mut() {
                    *s *= g1;
                }
            }
        } else {
            linear_gain(buf, g0, g1, 1.0 / n);
        }
        self.gain_db = end_db;
        self.gain = g1;
    }

    /// Off: back to unity over [`OFF_RAMP`] samples, then untouched (bit-exact once at unity).
    #[inline]
    fn ramp_to_unity(&mut self, buf: &mut Block) {
        let g0 = self.gain;
        self.gain = 1.0;
        self.gain_db = 0.0;
        if g0 == 1.0 {
            return;
        }
        let d = 1.0 - g0;
        let step = 1.0 / OFF_RAMP as f32;
        for (i, s) in buf[..OFF_RAMP].iter_mut().enumerate() {
            *s *= g0 + d * ((i as i32 + 1) as f32 * step); // i32: packed conversion (see mix.rs)
        }
    }
}

/// A block's mean square in dB (10 log10), -200 for zero; NaN stays NaN.
#[inline]
fn power_db(ms: f32) -> f32 {
    if ms <= 1e-20 { -200.0 } else { 10.0 * ms.log10() }
}

/// Multiplies `buf` by a gain going linearly from `g0` (the last block's end) to `g1` over the first
/// `1 / inv_n` samples, then `g1`. Written as `g1 - d * u` so the settled part is exactly `g1`.
#[inline]
fn linear_gain(buf: &mut Block, g0: f32, g1: f32, inv_n: f32) {
    let d = g1 - g0;
    for (i, s) in buf.iter_mut().enumerate() {
        // `i as i32`: converts as a packed `cvtdq2ps` (usize -> f32 is scalar at x86-64-v2).
        let u = 1.0 - (i as i32 + 1) as f32 * inv_n;
        let u = if u > 0.0 { u } else { 0.0 };
        *s *= g1 - d * u;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GateConfig;
    use crate::lin_to_db;

    /// The shipped defaults (attack 2, release 150, hold 100, range -40, margin 6), VAD off.
    fn params(mode: GateMode) -> GateParams {
        let c = GateConfig::default();
        GateParams {
            mode,
            threshold_db: -40.0,
            auto_margin_db: c.auto_margin_db,
            attack_ms: c.attack_ms,
            release_ms: c.release_ms,
            hold_ms: c.hold_ms,
            range_db: c.range_db,
            use_vad: false,
        }
    }

    /// Deterministic white noise at `rms` (uniform, so peak = rms * sqrt(3)).
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
        fn block(&mut self, rms: f32) -> Block {
            let k = rms * 3f32.sqrt();
            std::array::from_fn(|_| k * self.next())
        }
    }

    /// Runs `levels` (one DC level per block) through the gate; returns the per-sample gains and the
    /// reports. DC makes the gain readable as out / in.
    fn run(gate: &mut Gate, p: &GateParams, levels: &[f32]) -> (Vec<f32>, Vec<GateReport>) {
        let mut gains = Vec::new();
        let mut reports = Vec::new();
        for &l in levels {
            let mut b = [l; BLOCK];
            reports.push(gate.process(&mut b, p, f32::NAN));
            gains.extend(b.iter().map(|v| v / l));
        }
        (gains, reports)
    }

    fn ms(n: f32) -> usize {
        ms_to_samples(n) as usize
    }

    /// DC levels that are powers of two, so out / in recovers the gain exactly: -60.2 and -18.1 dBFS.
    const QUIET: f32 = 1.0 / 1024.0;
    const LOUD: f32 = 1.0 / 8.0;

    #[test]
    fn coefficients_match_their_time_constants() {
        assert!((LEVEL_ALPHA - (1.0 - (-10.0f32 / 100.0).exp())).abs() < 1e-7);
        assert!((FALL_ALPHA - (1.0 - (-10.0f32 / 200.0).exp())).abs() < 1e-7);
    }

    #[test]
    fn off_is_bit_exact_unity() {
        let mut g = Gate::new();
        let mut n = Noise(7);
        let p = params(GateMode::Off);
        for _ in 0..50 {
            let x = n.block(0.01);
            let mut y = x;
            let r = g.process(&mut y, &p, 0.1);
            assert_eq!(y.map(f32::to_bits), x.map(f32::to_bits));
            assert_eq!(r.gain_db, 0.0);
            assert!(r.open);
        }
    }

    #[test]
    fn static_release_reaches_the_range_within_hold_plus_release() {
        // The default release (150 ms) and hold (100 ms): loud, then quiet from block 20.
        let p = params(GateMode::Static);
        let levels: Vec<f32> = (0..60).map(|b| if b < 20 { LOUD } else { QUIET }).collect();
        let mut gate = Gate::new();
        let (g, r) = run(&mut gate, &p, &levels);
        let drop = 20 * BLOCK;
        let (hold, release) = (ms(p.hold_ms), ms(p.release_ms));
        // Open and at unity through the hold.
        assert!(g[..drop + hold].iter().all(|&v| v == 1.0), "unity until the hold ends");
        assert!(r[20..30].iter().all(|r| r.open) && !r[30].open);
        // Then linear in dB: halfway through the release it is at half the range.
        let mid = lin_to_db(g[drop + hold + release / 2]);
        assert!((mid - -20.0).abs() < 0.3, "half the release: {mid} dB");
        // At the range by hold + release, and not before.
        let at = drop + hold + release;
        assert!((lin_to_db(g[at - 1]) - -40.0).abs() < 1e-3, "at the range: {} dB", lin_to_db(g[at - 1]));
        assert!(lin_to_db(g[at - 240]) > -39.0, "not there early");
        assert!(g[at..].iter().all(|&v| v == db_to_lin(-40.0)), "settled exactly");
        // Monotonic, and the dB at each block edge falls by the same step.
        assert!(g.windows(2).all(|w| w[1] <= w[0]));
        let per_block = -40.0 / (release as f32 / BLOCK as f32);
        for b in 31..=44 {
            assert!((r[b].gain_db - r[b - 1].gain_db - per_block).abs() < 1e-3, "block {b}: {} dB", r[b].gain_db);
        }
    }

    #[test]
    fn attack_takes_attack_ms_from_the_range_then_is_exactly_unity() {
        let p = params(GateMode::Static);
        let mut gate = Gate::new();
        let (_, r) = run(&mut gate, &p, &[QUIET; 40]);
        assert_eq!(r[39].gain_db, -40.0, "closed");
        let (g, r) = run(&mut gate, &p, &[LOUD, LOUD]);
        assert!(r[0].open);
        let n = ms(p.attack_ms); // 96 samples
        let range = db_to_lin(-40.0);
        // A linear amplitude ramp from the closed gain, continuous with the last block.
        assert!((g[0] - (range + (1.0 - range) / n as f32)).abs() < 1e-6, "{}", g[0]);
        assert!(g[n - 2] < 1.0 && g[n - 2] > 0.95);
        assert!(g[n..].iter().all(|&v| v == 1.0), "bit-exact unity once open");
        assert!(g[..n].windows(2).all(|w| w[1] > w[0]));
        assert_eq!(r[0].gain_db, 0.0);
    }

    #[test]
    fn hysteresis_keeps_the_gate_open_down_to_3_db_below_the_threshold() {
        let mut p = params(GateMode::Static);
        p.hold_ms = 0.0;
        let db = |d: f32| db_to_lin(d);
        let mut gate = Gate::new();
        // -41 dB doesn't open a closed gate; -39 does; -42 keeps it open; -43.5 closes it.
        let (_, r) = run(&mut gate, &p, &[db(-60.0), db(-41.0), db(-39.0), db(-42.0), db(-42.9), db(-43.5), db(-41.0)]);
        assert_eq!(r.iter().map(|r| r.open).collect::<Vec<_>>(), [false, false, true, true, true, false, false]);
    }

    #[test]
    fn zero_hold_closes_on_the_next_block() {
        let mut p = params(GateMode::Static);
        p.hold_ms = 0.0;
        let mut gate = Gate::new();
        let (_, r) = run(&mut gate, &p, &[LOUD, LOUD, QUIET, QUIET]);
        assert_eq!(r.iter().map(|r| r.open).collect::<Vec<_>>(), [true, true, false, false]);
    }

    #[test]
    fn vad_opens_and_nan_probability_does_not() {
        let mut p = params(GateMode::Static);
        p.use_vad = true;
        p.hold_ms = 0.0;
        let mut gate = Gate::new();
        let mut b = [QUIET; BLOCK];
        assert!(gate.process(&mut b, &p, 0.9).open);
        assert!(!gate.process(&mut b, &p, f32::NAN).open);
        assert!(!gate.process(&mut b, &p, 0.3).open);
        p.use_vad = false;
        assert!(!gate.process(&mut b, &p, 0.9).open, "VAD ignored when not enabled");
    }

    #[test]
    fn ramps_are_continuous_across_blocks_and_parameter_changes() {
        // Alternate loud and quiet every 3 blocks with no hold, so ramps meet block edges mid-way
        // and change direction; then change the times and the range mid-ramp.
        let mut p = params(GateMode::Static);
        p.hold_ms = 0.0;
        p.attack_ms = 15.0;
        p.release_ms = 20.0;
        let levels: Vec<f32> = (0..60).map(|b| if (b / 3) % 2 == 0 { LOUD } else { QUIET }).collect();
        let mut gate = Gate::new();
        let (mut g, _) = run(&mut gate, &p, &levels);
        for (attack, release, range) in [(5.0, 20.0, -40.0), (1.0, 50.0, -60.0), (50.0, 100.0, -20.0)] {
            p.attack_ms = attack;
            p.release_ms = release;
            p.range_db = range;
            let (more, _) = run(&mut gate, &p, &[QUIET, QUIET, LOUD, LOUD, QUIET]);
            g.extend(more);
        }
        let step = |i: usize| (g[i] - g[i - 1]).abs();
        // A block edge never steps by more than the ramps on either side of it: the next block
        // carries on from where the last one ended.
        for k in 1..g.len() / BLOCK {
            let e = k * BLOCK;
            let inner = (e - BLOCK + 1..e).chain(e + 1..e + BLOCK).map(step).fold(0.0f32, f32::max);
            assert!(step(e) <= inner * 1.01 + 1e-7, "block {k}: edge step {} against {inner}", step(e));
        }
        // Linear in dB at R dB per sample, interpolated in amplitude, never moves by more than
        // (ln 10 / 20) R per sample; the steepest ramp here is 60 dB in 1 ms.
        let limit = std::f32::consts::LN_10 / 20.0 * (60.0 / ms(1.0) as f32) * 1.01;
        assert!((1..g.len()).all(|i| step(i) <= limit));
        assert!(g.iter().all(|&v| v.is_normal() && v <= 1.0));
    }

    #[test]
    fn a_range_moved_while_closed_is_followed_without_stalling() {
        let mut p = params(GateMode::Static);
        let mut gate = Gate::new();
        run(&mut gate, &p, &[QUIET; 40]);
        p.range_db = 0.0; // no attenuation now: back up to 0 dB at the attack rate
        let (g, r) = run(&mut gate, &p, &[QUIET; 2]);
        assert_eq!(r[1].gain_db, 0.0);
        assert!(g[ms(p.attack_ms)..].iter().all(|&v| v == 1.0));
        p.range_db = -20.0;
        let (_, r) = run(&mut gate, &p, &[QUIET; 20]);
        assert_eq!(r[19].gain_db, -20.0);
        p.range_db = f32::NEG_INFINITY; // clamped to -120 dB: no zero or denormal gain
        let (g, r) = run(&mut gate, &p, &[QUIET; 40]);
        assert_eq!(r[39].gain_db, -120.0);
        assert!(g.iter().all(|&v| v.is_normal()));
    }

    #[test]
    fn auto_closes_within_two_seconds_on_steady_noise_even_after_silence() {
        // DSP-5: a fresh gate, and one after the engine's dithered warm-up (blocks near -190 dBFS).
        for warm_up in [0usize, 3, 200] {
            let p = params(GateMode::Auto);
            let mut gate = Gate::new();
            let mut n = Noise(99);
            for _ in 0..warm_up {
                let mut b = n.block(3e-10);
                gate.process(&mut b, &p, f32::NAN);
            }
            let mut closed = None;
            let mut last = GateReport::default();
            for blk in 0..500 {
                let mut b = n.block(db_to_lin(-60.0));
                last = gate.process(&mut b, &p, f32::NAN);
                if closed.is_none() && !last.open {
                    closed = Some(blk);
                }
            }
            let t = closed.expect("closes") as f32 / BLOCKS_PER_S;
            assert!(t <= 2.0, "warm-up {warm_up}: closed after {t} s");
            assert!(!last.open && last.gain_db == -40.0);
            assert!((last.noise_floor_db - -60.0).abs() < 0.5, "floor {}", last.noise_floor_db);
        }
    }

    /// verify-dsp NEW-0: with the GUI's shortest holds (0-10 ms), one to three quiet start-up
    /// blocks (the chain's priming block, RNNoise's first output, near -100 dBFS) don't end the
    /// acquisition with the floor far below the noise: it still closes on -70 dBFS noise as fast as
    /// with the default hold.
    #[test]
    fn a_short_hold_does_not_cut_the_acquisition_short() {
        for hold_ms in [0.0, 10.0, f32::NAN] {
            for quiet in [1usize, 3] {
                let p = GateParams { hold_ms, ..params(GateMode::Auto) };
                let mut gate = Gate::new();
                let mut n = Noise(7);
                for _ in 0..quiet {
                    let mut b = n.block(db_to_lin(-100.0));
                    gate.process(&mut b, &p, 0.1);
                }
                let mut closed = None;
                let mut last = GateReport::default();
                for blk in 0..300 {
                    let mut b = n.block(db_to_lin(-70.0));
                    last = gate.process(&mut b, &p, 0.1);
                    if closed.is_none() && !last.open {
                        closed = Some(blk);
                    }
                }
                let t = closed.expect("closes") as f32 / BLOCKS_PER_S;
                assert!(t <= 2.0, "hold {hold_ms}, {quiet} quiet: closed after {t} s");
                assert!(!last.open && (last.noise_floor_db - -70.0).abs() < 1.0, "{last:?}");
            }
        }
    }

    #[test]
    fn auto_tracks_a_noise_floor_stepping_from_minus_70_to_minus_50_and_back() {
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        let mut n = Noise(12345);
        let mut last = GateReport::default();
        for _ in 0..500 {
            let mut b = n.block(db_to_lin(-70.0));
            last = gate.process(&mut b, &p, f32::NAN);
        }
        assert!((last.noise_floor_db - -70.0).abs() < 0.5, "floor {}", last.noise_floor_db);
        assert!(!last.open, "noise alone keeps the gate closed");

        // Step up 20 dB: the gate opens (it could be speech), the floor climbs at 1 dB/s while open
        // and 3 dB/s once the gate has closed, and the gate closes when it catches up.
        let mut t_closed = None;
        for blk in 0..2500 {
            let mut b = n.block(db_to_lin(-50.0));
            last = gate.process(&mut b, &p, f32::NAN);
            if blk == 100 {
                assert!(last.noise_floor_db < -67.0, "rises slowly: {}", last.noise_floor_db);
            }
            if !last.open && t_closed.is_none() {
                t_closed = Some(blk);
            }
        }
        assert!((last.noise_floor_db - -50.0).abs() < 0.5, "floor {}", last.noise_floor_db);
        assert!(!last.open);
        // 17 dB at 1 dB/s (to within the 3 dB hysteresis), then the hold.
        let t = t_closed.expect("closes once the floor catches up") as f32 / BLOCKS_PER_S;
        assert!((16.0..=18.5).contains(&t), "closed after {t} s");

        // Step back down: the floor follows within about a second (τ 100 ms level, τ 200 ms fall).
        for _ in 0..100 {
            let mut b = n.block(db_to_lin(-70.0));
            last = gate.process(&mut b, &p, f32::NAN);
        }
        assert!(last.noise_floor_db < -68.0, "{}", last.noise_floor_db);
        // And speech-level blocks above the floor open the gate.
        let mut b = n.block(db_to_lin(-30.0));
        assert!(gate.process(&mut b, &p, f32::NAN).open);
    }

    #[test]
    fn auto_closes_on_a_residual_that_swings_by_more_than_the_margin_from_block_to_block() {
        // A denoiser's residual: each block's level jumps around by ±5 dB, so neighbouring blocks
        // differ by up to 10 dB, more than the 6 dB margin. A floor on block minima sat 5 dB under
        // the middle and kept the gate open for good; the smoothed power level doesn't.
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        let mut n = Noise(4242);
        let (mut open_blocks, mut closed_at) = (0, None);
        for blk in 0..1000 {
            let swing = 5.0 * n.next(); // uniform in ±5 dB
            let mut b = n.block(db_to_lin(-70.0 + swing));
            let r = gate.process(&mut b, &p, f32::NAN);
            if closed_at.is_none() && !r.open {
                closed_at = Some(blk);
            }
            if blk >= 300 && r.open {
                open_blocks += 1;
            }
        }
        assert!(closed_at.is_some_and(|b| b < 200), "first closed at block {closed_at:?}");
        assert!(open_blocks <= 7, "open {open_blocks} of 700 blocks");
    }

    #[test]
    fn open_on_noise_the_floor_rises_at_the_closed_rate_unless_the_model_reports_speech() {
        // Noise 10 dB above a settled floor holds the gate open on level alone. With a speech
        // probability under 0.5 the floor climbs at 3 dB/s; reported as speech (or with no model
        // probability) it climbs at 1 dB/s.
        let p = params(GateMode::Auto);
        for (prob, rate) in [(0.1f32, 3.0f32), (0.9, 1.0), (f32::NAN, 1.0)] {
            let mut gate = Gate::new();
            let mut n = Noise(31);
            for _ in 0..500 {
                let mut b = n.block(db_to_lin(-70.0));
                gate.process(&mut b, &p, prob);
            }
            let mut r = GateReport::default();
            for _ in 0..50 {
                let mut b = n.block(db_to_lin(-70.0));
                r = gate.process(&mut b, &p, prob);
            }
            let start = r.noise_floor_db;
            for _ in 0..100 {
                let mut b = n.block(db_to_lin(-60.0));
                r = gate.process(&mut b, &p, prob);
                assert!(r.open, "probability {prob}: open on the louder noise");
            }
            let rose = r.noise_floor_db - start;
            assert!((rose - rate).abs() < 0.15, "probability {prob}: rose {rose} dB in 1 s");
        }
    }

    #[test]
    fn speech_opens_at_once_and_pauses_close() {
        // Bursts 25 dB above steady noise: open in the first block of each, closed in the pauses.
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        let mut n = Noise(5);
        for _ in 0..300 {
            let mut b = n.block(db_to_lin(-65.0));
            gate.process(&mut b, &p, f32::NAN);
        }
        for burst in 0..10 {
            for blk in 0..150 {
                let level = if blk < 50 { -40.0 } else { -65.0 };
                let mut b = n.block(db_to_lin(level));
                let r = gate.process(&mut b, &p, f32::NAN);
                if blk < 50 {
                    assert!(r.open && (blk == 0 || r.gain_db == 0.0), "burst {burst} block {blk}");
                } else if blk >= 50 + 10 + 15 {
                    assert!(!r.open && r.gain_db == -40.0, "burst {burst} pause block {blk}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn the_floor_ignores_silence_and_reacquires_after_a_long_run() {
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        let mut n = Noise(77);
        for _ in 0..300 {
            let mut b = n.block(db_to_lin(-60.0));
            gate.process(&mut b, &p, f32::NAN);
        }
        // Short and long digital silence leave the floor where it was.
        for len in [3, SILENT_RUN_BLOCKS as usize + 10] {
            let mut r = GateReport::default();
            for _ in 0..len {
                let mut b = [0.0f32; BLOCK];
                r = gate.process(&mut b, &p, f32::NAN);
            }
            assert!((r.noise_floor_db - -60.0).abs() < 0.5, "after {len} silent blocks: {}", r.noise_floor_db);
        }
        // After the long run it re-acquires: unmuted into noise 20 dB louder, it catches up and
        // closes in about a second rather than 7 s at the closed rate.
        let mut closed = None;
        for blk in 0..300 {
            let mut b = n.block(db_to_lin(-40.0));
            if closed.is_none() && !gate.process(&mut b, &p, f32::NAN).open {
                closed = Some(blk);
            }
        }
        let t = closed.expect("closes") as f32 / BLOCKS_PER_S;
        assert!(t < 1.5, "re-acquired in {t} s");
    }

    #[test]
    fn nan_and_infinite_blocks_leave_the_floor_alone() {
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        let mut n = Noise(8);
        for _ in 0..300 {
            gate.process(&mut n.block(db_to_lin(-60.0)), &p, f32::NAN);
        }
        for bad in [f32::NAN, f32::INFINITY] {
            let mut b = n.block(db_to_lin(-60.0));
            b[17] = bad;
            gate.process(&mut b, &p, f32::NAN);
        }
        let mut r = GateReport::default();
        for _ in 0..200 {
            r = gate.process(&mut n.block(db_to_lin(-60.0)), &p, f32::NAN);
        }
        assert!((r.noise_floor_db - -60.0).abs() < 0.5, "floor {}", r.noise_floor_db);
        assert!(!r.open && r.gain_db == -40.0);
    }

    #[test]
    fn floor_is_clamped() {
        let p = params(GateMode::Auto);
        let mut gate = Gate::new();
        // Fresh blocks each time: the gate scales them in place.
        let mut r = GateReport::default();
        for _ in 0..200 {
            r = gate.process(&mut [2e-7f32; BLOCK], &p, f32::NAN); // -134 dBFS: above the silence cut
        }
        assert_eq!(r.noise_floor_db, -120.0);
        for _ in 0..11_000 {
            r = gate.process(&mut [0.5f32; BLOCK], &p, f32::NAN);
        }
        assert_eq!(r.noise_floor_db, -20.0);
    }

    #[test]
    fn switching_off_while_closed_ramps_back_to_unity() {
        let mut gate = Gate::new();
        let (_, r) = run(&mut gate, &params(GateMode::Static), &[QUIET; 40]);
        assert_eq!(r[39].gain_db, -40.0);
        let p = params(GateMode::Off);
        let mut b = [QUIET; BLOCK];
        let r = gate.process(&mut b, &p, f32::NAN);
        assert_eq!(r.gain_db, 0.0);
        let range = db_to_lin(-40.0);
        assert!((b[0] / QUIET - range).abs() < 0.01, "starts from the closed gain");
        assert!((b[OFF_RAMP - 1] / QUIET - 1.0).abs() < 1e-5, "unity after 5 ms");
        assert!(b[OFF_RAMP..].iter().all(|&v| v == QUIET), "untouched after the ramp");
        for w in b.windows(2) {
            assert!(w[1] >= w[0]);
        }
        let mut c = [QUIET; BLOCK];
        gate.process(&mut c, &p, f32::NAN);
        assert!(c.iter().all(|&v| v == QUIET));
    }

    #[test]
    fn zero_attack_and_release_are_instant() {
        let mut p = params(GateMode::Static);
        p.attack_ms = 0.0;
        p.release_ms = 0.0;
        p.hold_ms = 0.0;
        let mut gate = Gate::new();
        let (g, _) = run(&mut gate, &p, &[QUIET]);
        assert!(g.iter().all(|&v| v == db_to_lin(-40.0)), "instant close");
        let (g, _) = run(&mut gate, &p, &[LOUD]);
        assert!(g.iter().all(|&v| v == 1.0), "instant open");
        p.attack_ms = f32::NAN; // treated as instant, not a NaN gain
        run(&mut gate, &p, &[QUIET]);
        let (g, _) = run(&mut gate, &p, &[LOUD]);
        assert!(g.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn reset_reopens_and_restarts_the_floor() {
        let mut gate = Gate::new();
        run(&mut gate, &params(GateMode::Static), &[QUIET; 100]);
        gate.reset();
        let mut b = [0.0f32; BLOCK];
        let r = gate.process(&mut b, &params(GateMode::Off), f32::NAN);
        assert_eq!(r.gain_db, 0.0);
        assert_eq!(r.noise_floor_db, FLOOR_START_DB, "restarted, and the silent block is ignored");
        assert!(gate.acquiring);
    }
}
