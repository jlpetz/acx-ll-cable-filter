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
  about the loop's lead, slips (said as the drift loop's, routine) and jumps (a raise's own jump is said with the raise,
  not counted as one). In the start-up the target rises to the delivery's need by design; with the exclusive mic (the
  default) that need includes up to a loopback period of its own (each mic block is processed before the loopback, on its
  own period, has delivered as recent a reference: 12 to 18.3 ms on the cables, #10's detail file), so only the part of
  the whole rise (in the start-up and after it) past that reads as bursts; with the shared mic, a rise of 3 ms or more.
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
  1% owed, its target risen 3 ms over its start in all (with the exclusive mic, 3 ms past that mic's own need of a loopback
  period), or without trusted stamps; slow to play: a render side 10 ms past half its period (the cables' shared output: about 20; exclusive: 2);
  drifting: 300 ppm, as printed (rounded), from a fit within ±50 ppm. Real hardware's figures are to set them.

## Fixes after the first live checks (2026-11-02, ours)
- **The exclusive mic's own raise, called bursts.** A live check on the cables (the exclusive mic, CABLE-B's loopback)
  reported the reference "in bursts", "needed 6.0 ms more lead than steady delivery does": its target had gone from 12 to
  18 ms, the exclusive mic's own need, but all of it just after the check's start-up (block 260 from the alignment), and
  the allowance applied to the start-up's raise alone. Why it can land there: the need is set by where the mic's blocks
  fall against the loopback's packets, and in the start-up the target follows a need only once three wakes have reached
  it; after the start-up it rises only once short takes persist (0.6 s of them, `reference.rs`), so a need that first shows
  near the start-up's end, or after it, raises the target past block 260 (the cables' chain recordings with the exclusive
  mic have it at blocks 5-35). Now the allowance is the whole rise's: the rise over the start target (in the start-up and
  after) less a loopback period, 3 ms or more of that is bursts.
  - **Inferred, not observed:** the report wasn't kept (the log holds no report text). Under the old rule only 12, 12, 18
    with the exclusive mic gives exactly 6.0, but the old rule would then also have counted the raise's own jump: the
    reference "in bursts, glitched", "its timeline jumped 1 times". If the report said only "in bursts", the other
    reading fits: the check didn't see the mic as exclusive (a shared or refused mic, own need 0, a 12 to 18 rise in the
    start-up also gives 6.0), which this change doesn't touch and which is then a different fault. To confirm live: rerun
    `devices --check` with the defaults and read the Mic line ("exclusive 16-bit") and whether the reference says "In its
    start-up ... rose" or "After its start-up ... rose". Tested with the live figures (`health.rs`,
  `the_exclusive_mics_own_raise_is_steady_wherever_it_lands`: 12, 12, 18 is steady, as are 12, 18, 18 and 12, 14, 18;
  shared, the same is bursts of 6 ms; 12, 20, 27 exclusive is bursts of 5) and from blocks
  (`a_raise_just_after_the_start_up_is_the_exclusive_mic_s_own`: a retarget at block 300, the raise's jump, three slips).
  With the shared mic the rule moves a little: a rise of 3 ms or more in all, where it was 3 in the start-up or 3 after.
- **Wording.** The loopback's freshness read as lateness ("had delivered ... at 100% of the blocks"): now "the loopback
  was delivering at 100% of the blocks (its last packet no more than two periods and 5 ms old), and 0% of them were short
  of samples", and its verdict for staleness is "stale at times" (was "late (stale)"). Slips, routine drift corrections,
  read like faults next to the jumps: now said apart, as the drift's ("as it should: they hold the echo path where it
  is"), and a jump finding only when there were jumps. A raise after the start-up says it's a jump of the echo path. The
  output's "Ran short or over" is "Ran short or overfilled". The exclusive mic's allowance first ended "so this is
  steady", which a stream bursty or stale for another reason (1% owed, 2% stale, untrusted stamps) contradicted next to
  its verdict; it now says of the rise alone "so the rise itself is no sign of bursts" (tested both ways).

## The flight recorder in the check (built 2026-11-03, ours)
The owner's ask, telling users when a device or driver misbehaves, at the moment it happens rather than as a run's averages:
the engine's flight recorder (engine.md, "The flight recorder") keeps the last 10 s of every block's facts and, on an output
underrun, a mic stall or glitch, a late wake of the audio thread, the reference's jumps or bursts of short takes, a model miss
or a fault, writes them with a reading in words (a held-up thread, a mic that stopped delivering, an output that ran dry).
- **`devices --check` arms it always** (into `%LOCALAPPDATA%\noise-filter\flight`, or `--flight DIR`): the report ends with
  "Flight recorder:" and names each dump (its folder, what triggered it, its reading), or says nothing went wrong. One that
  can't be armed (its folder can't be made) is a warning, and the check runs without it, its report saying so.
- **The stall count changed** (a fix found with it): a stall now needs a wake with nothing from the mic, neither its event
  nor a packet. With the exclusive output a thread held up past 100 ms counted one although the mic's packets were waiting,
  so the check's "glitches and stalls" could blame a mic for the system's hold-up. And a mic packet flagged discontinuous
  is no glitch when it's the run's first (many drivers flag the first after a start; unverified here: the cables don't),
  which would have made every check on such a mic report a glitch and write a dump. The first real-mic checks should
  confirm both.
- **Telling the thread from the device** takes the exclusive output: its events wake the thread every few ms whatever the
  mic does. With a shared output (the default playback device, or `--no-exclusive-output`) the thread wakes on the mic's
  events alone, and a held thread and a mic's driver that held its packets back look the same; the reading says it can't
  tell rather than blame the device (`ThreadFacts::out_wakes`).
- **For the window later** (TODO 15): `Engine::arm_flight_recorder` with `FlightRecorder::stats` (each dump's folder,
  trigger, time and reading) is the API to show a user "your microphone stopped delivering for 160 ms at 14:03:12" and
  where the facts are. Retention is the engine's (the newest 20 dumps in the folder), so an always-on recorder stays under
  about 10 MB. Whether the window arms it always is still to decide.

## Open
- Thresholds for "bursty" and "late", from real devices (the cables are the only data so far; #8's detail file). The first
  `devices --check` runs on real mics and speakers give them.
- Whether calibration results should persist or be re-measured each start (the adaptive loops cover drift in a run).
- Where it sits in the GUI (#15).
