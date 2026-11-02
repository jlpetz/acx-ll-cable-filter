# 21. Device health and calibration

## The owner's ask (2026-11-01)
- Tell the user when a device or its driver delivers in bursts or late, "so if the hardware/driver is playing up they can
  fix it": a measure in the app, or a calibration option.
- A calibration that sizes the buffers to the devices, "not too big or small", rather than fixed margins. (On the echo
  test's fixed 100 ms: the owner is fine with it if it's needed; the test now calibrates a realistic delay instead, #10.)

## What the engine already measures (2026-11-01)
- **Per block** (the latency budget, #13; `chain_blocks.csv`): mic delivery (capture stamp to the wake that read it), the
  output queue, the render side, and the readout they add up to.
- **The reference** (`reference.rs`, #10): its lead and target, whether a take was short (owed samples), whether the
  loopback was fresh, its stamps' scatter, slips and jumps, and the drift.
- **Counters:** mic glitches, output underruns, reference gaps, stalls, coarse drops, drift (ppm) per clock.
- **The live probe** (`live_probe.rs`, a test behind the `live-probe` feature): frames per wake, the data's age when read,
  and the loopback's swing against the mic: the measures that showed VB-CABLE's bursts (#8's detail file).

## Proposal (ours, awaiting the owner)
1. **A health readout per stream** (mic, speakers' loopback, output): steady or bursty (how far a packet's arrival runs
   past its mean, in ms, as a high percentile), how late (the data's age beyond its packet), drift, glitches. Each with a
   plain verdict and what to try: e.g. "Your microphone delivers in 20 ms bursts, which adds about 10 ms here. Try another
   USB port, the device's own driver, or turning off audio enhancements." In the GUI's readout and `run`'s status, and as
   `noise-filter devices --check` (a short passive run; built, below; the GUI's and `run`'s are still to do).
2. **Calibration** (a GUI button and a command):
   - passive, 10-20 s: measure each stream's delivery as above and set the starting targets from it (the reference's lead
     and the output queue: the need plus a margin), so the adaptive loops start right instead of from fixed margins;
   - active, with speakers (optional, the user agrees to a short sound): play a chirp on the speakers and time its echo in
     the mic against the loopback, which gives the echo path's real delay. Check it against the lead the loopback needs:
     when the echo comes sooner (#10's sweep: under about 13 ms on the cables), say that echo cancelling will be weak and
     why, and offer what helps (a smaller loopback period, or a short mic wait only in that case);
   - save the result per device pair, and show it again when the devices change.
3. **The lab's version already exists:** `quality --test echo` calibrates the rig's echo offset with a marker on both
   devices in each take (#10, #14). The product's active calibration is the same measurement on real speakers.

## Built (2026-11-02): `noise-filter devices --check` (ours; the owner's ask, the design ours)
The engine runs on the chosen devices as `run` would (echo cancelling on unless `--no-aec`, so the loopback is checked; the
latency readout on) for `--seconds` (15 by default, 5-120) with the chain recorder armed, then a report per stream: a verdict,
the figures behind it, and what to try (app.md, "Commands"). The logic is pure and tested (`nf-app/src/health.rs`:
`measure` makes the figures from the blocks and counters, `assess` the verdicts, `format` the text). Nothing is saved yet.
The figures are one run's: the streams as it opened them, its chain recording's blocks, and its counters as its last
once-a-second publication had them (the check waits for one more after its time, so the last moments count); after a
restart the first run's, its counters up to its last whole second, with a warning.
- **Mic** (from the run's second second on, as the output): blocks that came two or more to a wake (a burst: the wake
  before had too little), the gaps between delivering wakes, a block's first sample's age when read (the budget's mic
  delivery: 5th-95th percentile, the oldest; what the bursts add is the mean less the 5th percentile), glitches and stalls,
  a refused exclusive mode and why. Without trusted stamps (no delivery figure) the wakes alone judge it: bunched blocks, or
  wake gaps 3 ms over a block at their 95th percentile, are bursts, and otherwise it's `steady by its wakes (not timed)`,
  never plain steady, with the maker's driver to try. One that delivered nothing after the first second says so, with
  Windows' microphone privacy setting and the device's mute among what to try.
- **Output:** its events (underruns and the silence an exclusive device played, deficits, coarse drops, slips,
  re-anchors), the render side's mean (how long the device and its driver hold the audio after taking it), the queue
  against its target.
- **Echo reference** (after its start-up: 250 blocks and 10 more from its alignment, or the start-up's last restart): the
  share of blocks the loopback was stale (no delivery for two periods and 5 ms) or short (zeros owed), its lead and target,
  the start-up's raise of the target and any raise after it, whether the stamps timed it and how far the readings scatter
  about the loop's lead, slips and jumps. In the start-up the target rises to the delivery's need by design; with the
  exclusive mic (the default) that need includes up to a loopback period of its own (each mic block is processed before
  the loopback, on its own period, has delivered as recent a reference: 12 to 18.3 ms on the cables, #10's detail file), so
  only a raise past that, or any raise after the start-up, reads as bursts.
- **Each clock's drift** against the mic's, fitted over the blocks after the first second (`nf_engine::clock_drift`,
  `clock_fit.rs`): each clock is a line of time against its sample index, the mic's from its index and capture stamps (or
  its delivery, without them), the output's from the device's clock readings (`OutputFacts::clock_pos`, an exclusive
  device's silence added back), the loopback's from its timeline's index and the tap time its stamped lead reading gives;
  least squares with an intercept a stretch between the steps that break a line (a mic position skip, an output underrun or
  re-anchor, a reference jump or restart). Not the drift loops' estimates: the output loop's integral (a PI loop, ωn about
  0.17 rad/s) settles in 30-60 s, and the cables' bursts throw it hundreds of ppm off before that (in the simulation at 15 s:
  -196 at a true 0 ppm, -289 at +100, on the loop's sign; the verifier's runs, 2026-11-02, had it flag a true +100 ppm as
  drifting and give -100 the wrong sign). The fit's 95% interval is printed; one looser than ±50 ppm gives no verdict. It
  needs 3 s of each clock, so the shortest check (5 s) gets one. `pipeline/tests/budget.rs`'s
  `a_short_check_measures_the_drift` (15 s, steady and bursty with jittered wakes, -300 to +300 ppm) has it within 5 ppm with
  stamps and 30 by delivery; the simulation's clocks are exact, so real stamps' scatter is what will widen it.
- **What to try:** another USB port (not a hub), the maker's driver, audio enhancements off, LatencyMon for DPC latency,
  exclusive mode (or, for glitches with it, turning it off), `--no-adaptive-output` for an output that ran dry; a virtual
  cable is told it delivers in bursts by design (no port or driver to change).
- **Thresholds (provisional, ours):** from the cables, the only data (bursty: about half their blocks two to a wake, a block
  10-21 ms old, 16 on average; steady in the simulations: a block a wake, 10.7 ms old). In bursts: 5% of blocks two or more
  to a wake, or a 3 ms swing (p95 less median); late: a mean 3 ms past the packet; the loopback late at 2% stale, in bursts at
  1% owed, its target raised 3 ms after its start-up (or in it, 3 ms past the exclusive mic's own need), or without trusted
  stamps; slow to play: a render side 10 ms past half its period (the cables' shared output: about 20; exclusive: 2);
  drifting: 300 ppm, as printed (rounded), from a fit within ±50 ppm. Real hardware's figures are to set them.

## Open
- Thresholds for "bursty" and "late", from real devices (the cables are the only data so far; #8's detail file). The first
  `devices --check` runs on real mics and speakers give them.
- Whether calibration results should persist or be re-measured each start (the adaptive loops cover drift in a run).
- Where it sits in the GUI (#15).
