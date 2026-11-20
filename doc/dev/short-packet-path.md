# The short packet path: design and estimate (2026-11-20)

**Status.** Deferred. The owner chose option B, the live switch, on 2026-11-20. The proposal came from an outside review.
A chain with no block-based stage could process each mic packet as it arrives. The owner initially asked for this during
the spike if it was quick. We estimate 950-1050 changed lines across fifteen files for B, including the audio thread's
output path and the noise toggle. Code references are to b41f0c3.

## What it would save
- **The block's fill wait, `BLOCK - packet`, for every sample.** Every sample in a block has the same latency: the block,
  plus mic delivery, the output queue and the render side. Processing per packet replaces the block with the packet. The
  saving is 6.67 ms with a 160-frame (3.33 ms) mic, which is the exclusive mic (the default) on our driver, on HDAudio and
  on VB-CABLE's capture side. It is 8 ms at 96 frames, 7.5 ms at 120 and 5 ms at 240. A 10 ms mic saves nothing: a shared
  mic at the engine's default period, such as VB-CABLE with `--no-exclusive-mic`. If VB-CABLE's capture refuses 3.33 ms
  and falls back to its 3 ms minimum (144 frames), the unit stays 480 and it saves nothing either. In the budget
  (budget.rs) this is the **wait**: a block's first sample waits two more 160-frame packets, and that becomes the unit's own
  work (µs). Mic delivery and the queue stay as they are. Against the spike's settled run through our pair (readout 20.4 ms,
  doc/todo/19-spike-2026-11-20.md check 7), it is about a third of the readout. That figure is an estimate to measure.
- **The output target's block term is not saved.** The target is one period + one block + the margin. Its block is the
  spare for late wakes and bursts, not a result of block-sized writes. The drift loop reads the fill before each write,
  which is the low point of the sawtooth, so steady delivery needs one period + the margin whatever the write size
  (engine.md, "The output target"). The adaptive target (on by default) already removes the spare by measurement: with
  the exclusive output and steady 3.33 ms packets it comes down from 14 ms to 5 in the simulations. A short path could also
  shrink the fixed target's spare to a packet. That would save another 6.67 ms during a run's first four minutes, with
  `--no-adaptive-output`, and after three strikes. But it would cut the late-wake allowance from 10 ms to 3.33 ms. The
  soak's miss was a block 8 ms or more late (about 10 in the simulations), and the fixed target rides out blocks up to
  13.5 ms late (out_target.rs). We don't recommend it.

## When it applies, and how often
Only with **no block-bound stage**: echo cancelling off and the noise filter off.

| Part | Block-bound? |
|---|---|
| AEC3 (sonora) | Yes: 10 ms frames; latency 430 |
| RNNoise | Yes: 480 hop; latency 480 |
| Model packs | Yes: re-blocked to 480 (`Reblocked`); the inference worker exchanges whole blocks, `depth` × 480 behind |
| Floor mix (`NoisePost`) | Bound to the model: it mixes the model's output with the dry signal delayed by the model's latency |
| Gate | Inside the noise stage, and its detector is the block's RMS. Its hold, ramps and floor tracker count in blocks (`SILENT_RUN_BLOCKS`, τ per block). It has no lookahead, so a gate of its own (TODO 17's "level-based gate") could run per packet with its constants rescaled. None exists in v0. |
| High band | Inside the noise stage (the model's band) |
| Meters | No: a side measurement that delays nothing. `Meter::measure` takes a block and `MeterBallistics` is tuned per 10 ms block, so they can take the assembled blocks |
| The chain's slots | With every slot off and nothing draining, the output is the input bit for bit (`*out = *input`; the AEC slot isn't built when AEC was off at the start) |

So in today's product the short path is **the app's bypass**. AEC off is the default (headphones). The noise filter on is
the default, and it is the reason the app exists. Both off means the app routes the mic to the virtual cable and adds
only meters. A user who calls through that would get lower latency by picking the mic in the call app directly. The usual
reason to switch the filter off is an A/B mid-run, and the short path doesn't reach that without a restart or a live
switch (below). We have no usage data. By design the case is rare. It would become relevant with a stage that can run per
packet, such as a gain or EQ effect activated with variable frame counts (CLAP allows `activate(48000, 1, 480)`; the v0
host profile fixes 480 for every stage).

## The design (the smallest correct one we see)
**The unit, chosen at the run's start** (audio.rs `run`): the mic's period in frames when all three of these hold:
- AEC is off. That means no AEC stage, no loopback, no echo-path guard and no hold.
- The noise filter is off.
- The period is under 480 and divides 480. `exclusive_period` picks the smallest such period from the device's minimum
  (never under 2 ms) up to its default: 96, 120, 160 or 240. A device with no divisor from its minimum up to its default
  runs at its minimum (125 frames when both are 2.6 ms, 144 when both are 3 ms), and so does a refused period's fallback
  (VB-CABLE's capture at 3 ms). Such a device keeps its period, and the unit stays 480.

Otherwise the unit is 480. **At 480 every path below has to reduce to today's code bit for bit.** The existing tests are
that check.

**Per unit (the output path):**
- Pop `unit` frames, keep a raw copy for the tap, dither them, copy them into `out_blk[k*unit..]`, and run `push_output`
  on that slice. The dither keeps today's sequence: 8 lanes, every unit a multiple of 8, the reference's draws at the
  block's end as now.
- `push_output` (pipeline.rs L867-1023) uses `BLOCK` with two meanings, and they split:
  - Where it means **this write**, it becomes `unit`: the backlog (whole units still queued, L896), the short and deficit
    tests (L909, L929), the first write's `this_block`, the drop's rounding and its half-block margins (L971-979), and the
    slip length (L1006).
  - Where it means **the loop's range**, it stays `BLOCK`: the drop threshold `target + 2 blocks` and the drift's
    `MAX_ERROR`.
- **Drift loop, one update per write** (`DriftController::with_step(target, unit)`). The filter coefficient, `KI·dt` and
  the slip accumulator's increment scale with the step. The closed loop (`s² + 0.048 Kp s + 0.048 Ki`) doesn't depend on
  the step, so the tuning carries over. There is at most one slip per write: 1500 ppm × 160 is 0.24. At 480 the constants
  are today's floats.
- **Slips:** `Slip::produced_len` becomes `produced(n)`. `SlipResampler`'s contract (`BLOCK ± 1`, debug-asserted)
  widens to lengths within one of each other and at most `BLOCK + 1`. The cubic path already takes any `n >= 2`.
- **Adaptive target:** its mean's coefficient and `WARM_UP` (300 blocks, 3 s) move to per-write. The window, the trial and
  the walk are per second and don't change. The fixed target keeps its block (above).
- **`FillOffset`:** per-write readings. A re-anchor compares the median of the last `OFFSET_RING` readings before it with
  the median of as many after it. The ring is sized to a whole phase cycle of a 1024-frame device period against the
  writes, not to a time span: lcm(1024, unit) / unit readings. That is 32 at 96, 160 and 480 (today's 32), but 64 at 240
  and 128 at 120. Make the ring 128 long, with the length used set at the run's start (32 at 480, so the block path is
  unchanged), or limit the unit to 96 and 160. We'd take the first.
- **Readout and budget:** at the block's first unit only. The readout's window stays 100 blocks (1 s).

**Per block (everything else), at the block's last unit, on the assembled block:**
- The chain runs on the assembled mic block and a dithered-zero reference, into a scratch buffer. Its output is thrown
  away: while every slot is off it equals what already went out. Its report gives the meters, and its state (`prev_in`,
  the delay lines, the drains) stays as it is today.
- The tap, the chain recorder and the flight recorder get the mic, the reference and the output as sent.
- `ChainBlock`'s facts come from the block's first unit: head, capture time and budget.
- **The output's facts** (`OutputFacts`, one a block, read by the CSV, the flight recorder and clock_fit.rs) keep today's
  contract if a block's facts cover the writes since the block before's first one:
  - `index`, `padding`, `pending`, `fill`, `target` and the clock reading come from the block's first unit.
  - The events of the block before's later units and of this block's first unit are this block's: `zeros`, `dropped` and
    `silence` summed, `underrun`, `late` and `reanchored` OR-ed. `silence`, `underrun` and `late` are documented that way
    already ("since the block before"); `zeros` and `dropped` ("at this block") get the same words. A unit's zeros and
    drops go into the stream before its index, so summing a block's own later units into it would break the index rule.
    Carried forward, the index still moves by the last block's length plus this block's `zeros` less its `dropped`.
    clock_fit.rs then breaks the output's line, and adds the silence, at the step where it happened.
  - `slip` and `skipped` describe the block's own samples. At most one slip falls in any block's units (the 1500 ppm limit
    is 0.72 slips per 480 samples, whatever the step), so `slip` keeps its -1..=1 range. A unit can be dropped alone, so
    `skipped` becomes the block's frames dropped (u32, not a bool). A block's length is then 480 + `slip` - `skipped`:
    479-481 with nothing dropped.
  - Underruns and deficits are counted once a block, as `render_underruns` is today, so the facts still add up to the
    counters. A dry spell over a block's three writes is one underrun.
- Thread facts (`process_us` summed), `block_us`, `publish_block`, `blocks += 1`, and `second_due` on block boundaries.
- The audio loop counts **blocks** a wake, not units, in `count_wake_blocks`. That count is what the automatic model depth
  remembers about a mic.

**Switching a stage on in a short run** is the hard part. There are two options:
- **A. Restart.** Turning the noise filter on in a short run restarts the streams, as turning AEC on without its stage
  does today (control.rs `restart_needed`):
  - The control thread needs the run's unit. `Opened` would carry it and `Desired` keep it beside `aec_built`, either as a
    new parameter or as a struct replacing `aec_built`. That touches 28 call sites, most of them tests.
  - The cost: the app's main control becomes a stream restart in such a run. The control thread waits 200 ms after the
    latest change (`COALESCE`, control.rs), with the old run still playing, then stops and reopens the streams (exclusive
    mic and output, the chain, the prefill, the warm-up). That stop-and-reopen is the dropout the call app hears; its
    length isn't measured yet. Today the same toggle is a click-free dip splice.
  - Turning the filter off again doesn't restart: the run stays on blocks until its next start.
- **B. A live switch, short to blocks, at a block boundary.** Back to units only at the next start.
  - The pipeline reads the controls before each unit. When a stage is wanted, it sends that unit faded out (3.33 ms),
    queues `BLOCK - unit` zeros (the next write is a block later, not a unit), then sends the chain's blocks with the
    first one faded in. The chain's own Off-to-On dip splice sits at its block's start, which has already gone out raw.
  - The cost: one gap of about 6.7 ms plus the fades at each switch-on, like the echo-path guard's hold.
  - It needs splice tests: no step across the fades, a continuous sample count, and no underrun at the adaptive target's
    lowest.

**Status and docs:** the status line, the GUI's budget line and the log say which unit ran ("short packet path,
3.33 ms"). engine.md's loop and targets sections change too. The host profile doesn't change: no stage runs on the short
path, so every stage still sees 480-frame blocks.

## Estimate
| File | Change | Lines (code + tests) |
|---|---|---|
| nf-core `dsp/slip.rs` | `produced(n)`, the wider contract; tests at 160 | 15 + 30 |
| nf-core `dsp/drift.rs` | `with_step`; the existing simulation at a 160 step (±50/300/1000 ppm: within ±1 ms, slip rate within 5%) | 15 + 40 |
| nf-engine `out_target.rs` | per-write coefficient and warm-up | 10 + 10 |
| nf-engine `pipeline.rs` | the unit, the per-unit and per-block split, `push_output` on a slice, the output's facts carried forward and counted once a block, the ring's length, `second_due` | 190 |
| nf-engine `pipeline/tests/short.rs` (new) | see below | 300 |
| nf-engine `audio.rs` | choose the unit, the drift's step, blocks a wake | 25 |
| nf-engine `control.rs`, `api.rs`, telemetry | option A: the unit in the control thread, the restart rule, the status | 25 + 28 call sites + 30 |
| the facts' readers: nf-engine `api.rs`, `csv.rs`, `flight.rs`, `recorder.rs`, `pipeline/tests/budget.rs`; nf-app `commands/quality.rs` | `skipped` a frame count: the contract's docs, the CSV's column and its docs, the flight recorder's drop trigger, the facts test's sums, the tests' struct literals. clock_fit.rs reads `silence`, `underrun` and `reanchored` as now, and doesn't change | 50 |
| docs | engine.md, TODO 13's detail | 40 |

The new tests (`pipeline/tests/short.rs`), all on the existing fakes:
- the bypass's output equals the block path's bit for bit;
- the budget's wait is near zero, and the readout is lower by `BLOCK - unit` at the same target;
- the tap and recorder items equal the block path's;
- statistics come once every 100 blocks;
- 10 simulated minutes at ±300 ppm into a jittered exclusive-like sink, with no underruns and the adaptive target settling;
- a late wake;
- `the_output_facts_follow_the_stream_block_for_block`'s cases at a 160-frame unit, with a unit dropped alone among them;
- a re-anchor at a 120-frame unit against a 1024-frame device, which needs the ring's full 128.

**Totals:**
- **Option A:** about 750-850 changed lines across sixteen files, 2-2.5 days.
- **Option B:** instead of A's control-side work (the unit in the control thread and its 28 call sites; control.rs drops
  out), about 150 lines in the pipeline and 100 of splice tests. About 950-1050 lines across fifteen files, 3-3.5 days.

Either option also needs the owner's live checks, since the dev box says nothing about real devices: the exclusive mic at
3.33 ms into the exclusive output, on our driver and on VB-CABLE. Check underruns, slips, the readout, and CPU (three times
the padding and clock calls on a shared output).

## Risks
- The output loop runs at three times the rate. A shared output makes `GetCurrentPadding` and `IAudioClock::GetPosition`
  300 times a second instead of 100 (performance/F3's concern), and underruns are found per write.
- The drift loop's discrete step changes. The continuous loop is the same, but the noise per reading falls with smaller
  writes. The 23 s / ±1 ms tuning has to be re-simulated at 160.
- A block's output facts now summarise three writes, and their events are those since the block before's first write
  (above). `skipped` becomes a frame count, which changes the CSV's column. Getting the carry wrong breaks the index rule
  or puts an unbroken step into clock_fit.rs's output line, which skews the fitted drift. The facts test at a 160-frame
  unit is the check.
- The noise toggle becomes a restart (A) or a 7 ms gap (B) where today it's a dip splice.
- There are two paths through the hottest loop. The block path must stay bit for bit the same.
- Shrinking the fixed target's spare is a separate decision (above). This design keeps it.

## Decision (2026-11-20)

The owner chose B. The noise toggle keeps the streams open and switches from short packets to blocks once per run.
The estimated saving today is about 7 ms in bypass with a 160-frame mic period. Implementation is deferred. We recommend
revisiting it with a per-packet effect such as an EQ or standalone gate (TODO 17), or a low-latency direct mode for the
virtual mic (TODO 19).
