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
   `noise-filter devices --check` (a short passive run).
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

## Open
- Thresholds for "bursty" and "late", from real devices (the cables are the only data so far; #8's detail file).
- Whether calibration results should persist or be re-measured each start (the adaptive loops cover drift in a run).
- Where it sits in the GUI (#15).
