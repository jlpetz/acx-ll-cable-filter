# 13. Latency measurement

Status: designed 2026-10-26 and fact-checked the same day (WASAPI timing, probes and estimators,
existing tools). Sources are Microsoft Learn, the Chromium and wasapi 0.26.0 sources, 3GPP TS
26.132, ITU-T P.501 and the OboeTester source, unless a line says otherwise.

## Layers
0. **Offline conformance (#2):** the measured stream offset equals `latency_samples − hop`. The hop
   of buffering only exists live (layers 1-3). Also CPU per frame. Phase 0.
1. **Always-on readout** from WASAPI timestamps (below). Phase 1, printed to the console until the
   GUI exists.
2. **Virtual loop (no physical audio):** probe → cable A → app (cable A's output as its mic) →
   cable B. Capture cable A's and cable B's outputs together; the difference = app + cable B.
   - Needs two cables (VB-CABLE + CABLE A, or Voicemeeter's two virtual inputs). With one cable:
     calibrate the cable alone and add layer 1.
   - Use the QPC timestamps to split the total into cable A transit, app pipeline and cable B
     transit. Phase 1.
3. **Real mic and speakers:** the probe plays on the speakers; capture the physical mic directly and
   the cable output together. The difference = what the app adds compared with Discord reading the
   raw mic (speaker, air and mic delays cancel). Also gives the absolute acoustic round trip.
   Phase 2. Oblique's RTL Utility also supports acoustic coupling, so it is a precedent for this
   mode.

## Layer 1: the always-on readout
- **Timing data:**
  - Capture: `GetBuffer`'s device position and QPC time (100 ns units) for each packet. The wasapi
    crate's `BufferInfo{flags, index, timestamp}` exposes them; frame k of a packet was captured at
    `timestamp + k/fs`.
  - Render (the cable): `AudioClock::get_position` → (pos, qpc). Play-out time of frame j =
    `qpc + (j − pos·fs/freq)/fs`. wasapi can't tell S_FALSE from S_OK here, and the position reads
    0 for a few ms after Start.
  - QPC is one timebase for the whole machine, so capture and play-out stamps subtract directly.
- **Sanity checks (Chromium's, in production):**
  - drop stamps flagged `TIMESTAMP_ERROR`, or more than 500 ms from now, or going backwards;
  - count index gaps and `DATA_DISCONTINUITY` as glitches;
  - subtract the samples waiting in the app's own rings from each capture time.
- **Readout:** carry each sample's capture time through the pipeline. Latency = play-out − capture,
  shown as the median and p95 over 1 s, next to the model's paper latency (window − hop +
  lookahead, plus one hop of buffering).
- **Not covered:** the VB-Cable pipe, the consuming app's buffering, and ADC/DAC time the driver
  doesn't stamp. Show the last loop-measured value for those.
- **Not trustworthy until validated:** stamp accuracy on VB-Cable and USB devices is undocumented.
  Check the QPC-predicted cable transit against a layer-2 loop measurement once per device type.
- **Don't display `GetStreamLatency`:** it's a fixed "maximum latency" hint for sizing writes, not
  a measurement.
- **Drift:** independent clocks typically differ by 30-50 ppm, so watch the slope (latency creeping
  at tens of µs per second means the drift controller (#9) isn't holding).

## Echo-path timing
- **The reference's stamps aren't play-out times.** Without a hardware loopback pin, WASAPI copies
  the engine's mix into the loopback buffer, so a reference packet's QPC is roughly mix time. The
  speaker's own buffering, the DAC and the air path come on top. Measure them (layer 3, or AEC3's
  `delay_ms`) rather than predict them.
- **AEC3 mostly ignores `set_stream_delay_ms`.** Upstream uses it only to seed alignment after a
  reset, unless `use_external_delay_estimator` is on (off by default). Rely on AEC3's own
  estimator. Check that sonora keeps this behaviour.
- **sonora 0.2.0 statistics** for tuning, no probe needed: ERL, ERLE, `delay_ms`,
  `delay_median_ms`, `delay_standard_deviation_ms`, `residual_echo_likelihood`, divergent-filter
  fraction.

## Probes and estimators, by path
- **Virtual loop, processing bypassed:**
  - an exponential sine sweep (Farina; robust to clock mismatch, harmonics separate out); or
  - an OboeTester-style Manchester-coded random pulse (Apache-2.0; normalised cross-correlation,
    coarse then fine; confidence = correlation − noise/signal, minimum 0.2 for that analyser).

  Use a matched filter and parabolic sub-sample peak interpolation.
- **Acoustic loop:**
  - a long exponential sweep, repeated a few times;
  - take the first direct-path peak, not the strongest;
  - in a noisy room, average more, use an SNR-weighted estimator, or MLS (better than sweeps when
    background noise isn't white, per Stan et al. 2002).
- **Through the noise model:** sweeps, clicks and PN bursts get removed, so use clean speech.
  Optionally add a CSS-like voiced lead-in (P.501's is 48.62 ms of voiced sound, then 200 ms of
  PN, then a 101.38 ms pause).
  - Generate it ourselves: ITU's text is "all rights reserved", and the signal database terms
    weren't checked.
  - Correlate the app's own mic input against the cable output, not against the generated probe.
    In mode B the room and speakers then cancel out.
  - GCC-PHAT (β ≈ 0.7-0.8), limited to the speech band, over 1-2 s windows; take the median.
  - Gate on peak-to-sidelobe ratio, and watch for false peaks one pitch period away.
  - Fallback: 3GPP TS 26.132 takes the peak of the Hilbert envelope of the cross-correlation,
    which tolerates the phase changes that speech processing introduces.
- **Through the AEC:**
  - **Latency:** turn AEC off, or take the reference from process loopback that excludes our own
    process (`new_application_loopback_client(pid, false)`), so our probe isn't in it. Notes on that
    path: the wasapi doc comment for `include_tree` is inverted relative to its code; process
    loopback mixes every other process on every endpoint; it needs build 20348+ and has no
    AudioClock.
  - **Tuning:** far-end speech, then near-end speech over it (double talk), reading the sonora
    statistics above.

## Measurement hygiene
- **Mic mode:** open the mic in RAW mode where supported, so OEM or Windows effects don't suppress
  the probe or add delay. Record whether RAW was in effect.
- **Engine period:** record it with every result (`get_current_shared_mode_engine_period`). Another
  app requesting small buffers changes it for every stream on that endpoint.
- **VB-Cable:** pin its Internal SR to 48 kHz (otherwise it resamples and Max Latency scales), and
  log its Latency setting.
- **Repetition:** repeat over minutes and while a game runs, and graph latency over time.
  DeepFilterNet's LADSPA plugin creeps from about 55 ms to over 100 ms under load.
- **Scope:** we measure up to the cable output. Discord's own capture and jitter buffering is out of
  reach.

## Measurements
**Dev VM, VB-CABLE, virtual loop** (2026-10-31 to 11-01): `latency --probe "CABLE-A Input" --capture "CABLE Output" --mic
"CABLE-A Output" --output "CABLE Input" --mode virtual --reps 5`, release build at x86-64-v3, the chain in passthrough
(RNNoise's 10 ms offset). Each row is one run.

| Cable Latency (base and A) | Probe to app input | App + output cable | End to end | App's live readout | Cable errors |
|---|---|---|---|---|---|
| 7168 (default) | 60.1 ms | 125.4 ms | 185.4 ms | 63.1 ms | not watched |
| 3 x 512 or 768 | 52-59 ms | 118-124 ms | 175-179 ms | 56-66 ms | rising; b256 and b512 red |
| 3 x 1024 = 3072 | 56.0 / 61.8 ms | 123.2 / 127.3 ms | 179.0 / 189.6 ms | 66.3 / 63.7 ms | none |

- VB-CABLE settles at about 55-60 ms a hop whatever its Latency setting, and wanders by 5-10 ms from run to run, which is as
  large as any difference between settings. Cable B as the output, at the default, measured 125.4 ms app + cable with a
  17 ms spread between sweeps.
- After a Latency change both cables passed only silence ("peak silent" in the report) until `pnputil /restart-device` on
  their devices.
- The app's live readout (56-66 ms) is above its design: 32 ms, a block of input and the 22 ms output target, plus a render
  side the design left out. (First written here as 32-42 ms, with the chain's 10 ms in it; the readout doesn't hold the
  chain, "Budget" below, so it's 24-34 ms over, and the app's own path is the readout plus the chain.) The cable-only runs
  below show the readout is right: the excess is the engine's.

**Cable only** (2026-11-01): `latency --cable-only --reps 5`, each cable's Input to its own Output, both streams at a 10 ms
period (as the engine's mic was). Each figure is one run's median; the sweeps within a run spread by 1-2 ms.

| Cable (its Latency) | Probe to capture |
|---|---|
| A (3072) | 60.8 / 52.3 ms |
| Base (3072) | 62.3 / 60.2 ms |
| B (7168, the default) | 61.7 ms |

- Each cable costs 52-62 ms by itself: all of the virtual loop's input path (52-62 ms). So the engine's own part of app +
  output cable (118-127 ms) is about 60-65 ms, near what its readout says (though that part is the readout plus the chain's
  10 ms, so it's 5-10 ms short of them: "Budget" below). VB-CABLE's stamps don't inflate the readout; the readout holds
  about 24-34 ms more than its 32 ms design.
- Not yet explained. Candidates: the shared-mode audio engine's period on each side (a capture packet waits up to a period
  before the app is woken; the mix runs a period ahead of the device), which the design sum leaves out; and the output
  target (a period + a block + the margin, 22 ms), whose block may be more than it needs.

## Cable-only mode (built and run 2026-11-01)
`latency --cable-only --probe "CABLE-A Input" --capture "CABLE-A Output" [--reps N]` plays the same sweep train with no engine
(`nf_engine::cable_latency_test`, on the calling thread, COM MTA and MMCSS while streaming) and times each sweep from the
probe's play-out (its AudioClock, as the engine test does) to its arrival in the capture (the packets' QPC stamps). The report
is the engine test's, labelled "cable only": median, spread, each repetition, the lowest confidence, and both streams' formats
and periods. Both tests share the play-and-record loop and the analysis. `run`'s options, `--mode` and `--through-model` are
refused with it. Its results are under Measurements.

Reading it against the engine test (the report's "compare with" line says the same):
- **Which figure is which.** The engine report's app figure (output capture minus the tap) already holds the output cable, so
  its end to end is input path + app. The fair comparisons are cable-only on cable A against the input path, and cable-only on
  the output cable against the output cable's share of app + output cable (what's left of it is the engine's own part).
  Adding cable(A) + app + cable(output) would count the output cable twice.
- **Same timing, different capture clients.** Both tests time the probe by its AudioClock and the arrival by the capture
  packets' QPC stamps (the engine's tap is stamped from the mic packets' QPC too). But cable-only records with the test's own
  shared-mode event capture at the default period, while the engine's mic opens with IAudioClient3's low-latency period and RAW
  when it can. If VB-CABLE's stamps depend on the client's period, cable(A) can differ from the input path for that alone:
  compare the periods on the two reports' conditions lines. A low-latency capture option for cable-only would test it.

## Budget (built and run 2026-11-01)
The readout now comes split into where each block's first sample spends its time (engine.md, "The latency budget", has the
definitions): **mic delivery + wait + output queue + render side = the readout**, block for block (`pipeline/tests/budget.rs`),
and the chain's offset beside it. Telemetry has the means and p95s over the readout's last 100 blocks and the run's totals;
`chain_blocks.csv` has the parts per block; the quality and latency reports print the means over the take or the test; `run`'s
status line (`[mic+wait+queue+render]`) and the GUI show the means.
- **Mic delivery** (measured): the first sample's capture stamp to the wake that read it. A block-sized packet read as it
  fills gives 10 ms; the cables' 19-21 ms packets put the mean at about 16.
- **Wait** (measured): that wake to the block's write: the wake's own work (reading the mic, servicing the reference, with
  AEC on the loopback's drain, and the hand-overs), the blocks before it in the same wake, and its own chain (0.05-0.15 ms);
  more only for a block whose samples came over two wakes. Expected near 0 on the cables (their packets are whole blocks).
- **Output queue** (measured): the fill the drift loop holds. Its mean is the target, 22 ms, whatever the delivery. A late
  block finds less queued (the device played on), so delivery + queue holds still block for block, and so does the readout.
- **Render side** (derived, the rest): from the queue's drain to the AudioClock's play-out. Half the output's period (5 ms:
  the queue is calibrated to the middle of the padding's steps), plus whatever the shared-mode engine and VB-CABLE hold
  after taking a sample, by their clock.
- **Chain** (reported): 8.96, 10 or 18.96 ms. **The readout doesn't hold it:** it times the block's first output sample,
  which carries the input from the chain's offset before. So the readout's design was a block + the output target (32 ms),
  not 42, and the design leaves the render side out. The sweep's "app + output cable" holds the readout, the chain and the
  output cable, so the first run's 125.4 ms leaves 52 ms for the output cable after 63.1 + 10. Cable-only put that cable at
  60-62 ms, 8-10 ms more: within the runs' wander, but worth a second look.

**Reading for the cables:** with mic ≈ 16, wait ≈ 0 and queue ≈ 22, the render side is what's left of 56-66 ms: 18-28 ms,
which is 13-23 ms over its half period. So most of the excess over the 32 ms design is on the render side (the engine and
cable after the take), and about 6 ms is the bursty mic. A live take will say. The output opens at the cable's default
10 ms shared-mode period (no IAudioClient3), so a smaller render period is a candidate if the render side is the engine's.

**Steady delivery** would take the mic's ~6 ms of bursts off delivery and the readout. The queue's mean is the target
whatever the delivery, and the render side doesn't depend on it, so neither shrinks unless the target changes.

**The spare block.** The code doesn't say what the target's block is for (engine.md left "whether one period + the margin
is enough" to be measured), but its arithmetic shows it. The loop holds the mean of the fills read before each write, so
the lowest fill is the target less how far a block came after the mean. Steady delivery leaves the block unused; bursts
use it. With the target a block lower (12 ms) in the event simulation (`the_spare_block_is_what_bursts_need`, and the
ignored `a_block_lower_target_sweep`, 1 ms wake jitter):
- steady delivery ran clean at every phase (10.5 ms left at the lowest);
- the cables' bursts in turn (blocks 20.7 and 10.7 ms old) left 5 ms, with one underrun at one phase;
- one wake in five missed left nothing, with 2-28 underruns a minute at the worst phases.

With today's target all of them ran clean (10-16 ms left).

**An adaptive output target**, like the reference's, would need:
- A need measured from the bursts: how far a block's mic delivery runs above its mean, or how far the queue dips under the
  target. Take a high percentile over long enough to see a drifting cable's phases.
- A target of the device's own need (half a period at its worst phase in the simulation; the design reasons a whole one)
  + the margin + that need. Start at today's, lower it only after the need has stayed under for minutes, and raise it at
  once when the queue runs short (zeros refill to the target, as now).
- Moves without a click: the drift loop's slips (`DriftController::set_target`; at 1500 ppm, about 7 s per 10 ms).
- The deficit and drop thresholds following it, and its target published each second (today only at the start).

The cables' need is about 5 ms (delivery 21 ms at most against a mean of 16), so it would take 5-10 ms off there (a whole
or half period for the device), and 10-15 ms with steady delivery. Which device need holds is for a live run to show.

**Found on the way: a dry spell moves the readout for good.** In the simulation, after the output runs dry, the device's
clock has counted the silence it played, so the readout reads low by that silence for the rest of the run. A 40 ms mic
stall took it from 37.7 to 16.1 ms. A 60 ms one put the play-out before the capture, and there was no readout at all for
5.6 s in one run and for the 20 s left in another. The real latency was back where it was, and the budget shows the drop
all on the render side (5 to -16 ms). `FillOffset` re-anchors the fill after a dry device; the readout has nothing like
it. Whether WASAPI's clock counts an underrun's silence is unverified. A live render side that goes negative after
`render_underruns` grows would show it.

**Measured live** (2026-11-01, the cables, release build):

| Run | Mic delivery | Wait | Output queue | Render side | Readout | Chain on top | App path |
|---|---|---|---|---|---|---|---|
| `latency --mode virtual`, passthrough | 16.4 | 0.0 | 22.2 | 25.7 | 64.2 | 10.0 | 74.2 ms |
| `quality --test echo --no-ns` (AEC3) | 17.0 | 0.1 | 21.9 | 25.6 | 64.6 | 9.0 | 73.6 ms |

The render side is the largest part: about 20 ms beyond its half-period convention, between leaving our buffer and the
output's play-out by its clock (the shared-mode engine's period and VB-CABLE's own render side). Candidates, largest
first:
1. **The render side:** exclusive mode on the output cable (only the app plays into it; built as `--exclusive-output`,
   below), or a smaller engine period (VB-CABLE offers none below 10 ms in shared mode; our own driver, #19, can).
2. **The output queue's spare block:** an adaptive output target (above; built as `--adaptive-output`, below), 3-4 ms
   on the cables' bursts, 8 with steady delivery.
3. **Mic delivery's 6 ms beyond a block:** VB-CABLE's bursts (#8's detail file); a device matter.
The chain's 9-19 ms belongs to the algorithms (#5, #7).

**The options, A/B live** (2026-11-01, the same day, `latency --mode virtual --reps 5`, passthrough; the options were
opt-in then: `--exclusive-output`, `--exclusive-mic`, `--adaptive-output`):

| Options | Mic + wait | Queue | Render side | Readout | App path (+ chain 10) | App + output cable | Spread |
|---|---|---|---|---|---|---|---|
| none (shared) | 17.1 | 21.9 | 25.9 | 64.9 | 74.9 | 126.3 | 3.2 |
| exclusive output (2 ms, 16-bit) | 18.1 | 13.8 | 2.8 | 34.6 | 44.6 | 73.9 | 6.1 |
| exclusive mic (3.33 ms, 16-bit) | 0.8 + 6.7 | 22.6 | 16.1 | 46.1 | 56.1 | 107.9 | 1.3 |
| both exclusive | 0.8 + 6.7 | 14.1 | 3.2 | 24.8 | 34.8 | 63.7 | 1.3 |
| both + adaptive (target 14 to 6.8 ms in 169 s) | 0.8 + 6.7 | 6.8 | 3.8 | 18.1 | 28.1 | 52.0 | 1.2 |

- Exclusive output removes the shared engine's render side (26 to 3 ms) and the output cable gets faster too: app + output
  cable fell 52 ms against the readout's 30.
- The exclusive mic delivers every 3.33 ms with no bursts (mic delivery 0.8 ms; the wait is the block filling), and the
  spread between sweeps fell to about 1 ms.
- Glitches: none. `quality --test noise` with both exclusive (30 s) and with all three (60 s, after the target came
  down): 0 mic glitches, 0 underruns, 0 stalls; the noise filter's figures unchanged (-25 to -27 dB in pauses, speech
  -0.3 to -0.4 dB). `quality --test echo` at 25 ms with both exclusive: a median 54.5 dB a far-only turn, no dropouts.
- The trade-offs: exclusive mode locks the endpoint (fine for the output cable, which only the app plays into; with the
  mic, other apps can't open it directly), 16-bit only on VB-CABLE, and a fresher mic needs a longer reference lead (18.3
  ms with the exclusive mic against 12-14 shared), which raises the shortest echo AEC3 can cancel by about as much (#10).
  The adaptive target trades the spare block for at most three extra underruns a run on rare 10-15 ms hiccups.

**Now the defaults** (the owner, 2026-11-02): "By default apply the lowest processing default (exclusives, adaptive queues)
etc. We should however keep in mind that some users may want to toggle it off for recording streams etc. Though as long as
the final output is not exclusive that shouldn't be a problem most of the time." The final output, the cable's recording
side that Discord or OBS read, stays shared; only our render into the cable and our mic capture are exclusive. Built the
same day:
- `IoConfig`'s three default to on; a settings file or engine config without them gets them on, one that saved them off
  keeps them off (e074ff7's window wrote all three as `false`, so a `settings.json` it saved keeps them off until the
  window's switches turn them on). `--no-exclusive-output`, `--no-exclusive-mic`, `--no-adaptive-output` turn one off; the
  plain flags turn one back on over a config; `--help` says what each costs and when to turn it off (recording the raw mic
  in another app; a device that misbehaves in exclusive mode; a recording that must have no glitch).
- The window: three switches in Diagnostics > Latency, each with a line on what it does and costs (gui.md, section 7).
- A refusal is ordinary now: a note (`EngineStatus::notes`), informational in the window, `note:` in `run`, with the stream,
  why, the cost (20-30 ms output, 10-20 mic, from the table above), the fix, and where to turn it off (the window's switch,
  the CLI's flag; engine.md, "Exclusive mode"). A format check that takes nothing leaves the initialise to say why, so "in
  use" and "not allowed" aren't reported as "no format".
- Ours, not the owner's (a review's finding): never the output exclusive on Windows' default playback device (the speakers
  picked by mistake, or CABLE Input left the default): held exclusively, no other app could play sound. It runs shared
  there, its warning says so, and a change of default to or from the output restarts the streams.
- Two copies on the same devices: the window and a CLI `run`/`latency`/`quality` on the same mic or cable can't both run
  now (the second's shared open is refused, `AUDCLNT_E_DEVICE_IN_USE`; its start error names another copy of the filter as
  the likely holder). Stop the window's filter before a lab run on its devices, and before `latency --cable-only` on a
  cable the window holds.
- `latency` and `quality` measure what the product does by default, at once: the adaptive target is then at its fixed
  start (14 ms with the exclusive output), which they say before measuring; `--settle` waits for it (up to 308 s), as they
  did with `--adaptive-output` before. Their conditions give the target at the test's end, neutrally ("at the fixed
  target", or "down from" it), as a strike or a timed-out `--settle` reads the same as a run's first two minutes. The
  18.1 ms row above is the settled one; an unsettled run should read about the "both exclusive" row's 24.8.
- The echo test's default delay (25 ms) is over the exclusive mic's floor (about 20 ms, #10).
- `latency --cable-only` times both ends in shared mode, while the product now renders into the output cable exclusively
  (and the A/B shows exclusive mode changes the cable's own transit: app + output cable fell 52 ms against the readout's 30).
  So cable-only compares the cables' shared paths; how a cable does as the product's output is the full `--mode virtual`
  run's app + output cable with that cable as the output.

**The latency options** (built 2026-11-01, each opt-in then so a live A/B could compare them; engine.md, "Exclusive mode"
and "The output target"). What each should take off the cables' readout of 64 ms (mic 16.4 + wait 0 + queue 22.2 + render
25.7), by the event simulations and the design; the live runs will say:

| Option | Mic + wait | Output queue | Render side | Readout | What it cuts |
|---|---|---|---|---|---|
| none (today) | 16.4 | 22.2 | 25.7 | 64 | |
| `--exclusive-output` (2 ms, 16-bit) | 16.4 | 14 | 3 + the cable's own | 33-43 | the target's period (10 → 2 ms: 8 ms), and the render side's engine pass: 23 ms if the 20 ms over the half period is all the engine's, 13 if half is VB-CABLE's |
| `--exclusive-mic` (3.33 ms) | 10.5-16 | 22.2 | 25.7 | 58-64 | the mic's bursts (6 ms), if they're VB-CABLE's schedule beating against the engine's pass; nothing if the cable bursts by itself |
| `--adaptive-output` (shared) | 16.4 | 18-19 | 25.7 | 60-61 | the spare block, as far as the bursts allow (they need 5-6 ms over a 12 ms floor), after its 2-minute window and a walk down of up to a minute |
| all three, steady delivery | 10.5 | 5-6 | 3 + the cable's own | 19-29 | up to 45 ms |

- The budget shows each cut where it lands: the queue's mean is the target (14 ms exclusive, the adaptive one where it
  settled); the exclusive render side is the device's two buffers (1.5 periods: 3 ms at 2 ms) plus what VB-CABLE holds past
  its clock; with the exclusive mic, mic delivery reads about 3.6 ms and the wait about 6.7 (a block's first sample comes
  with its 3.33 ms packet and waits for the other two).
- The adaptive target takes four minutes to move (a window, and a second, its trial, to confirm it) and then walks down
  (a tenth of the way a second, at most 0.5 ms: a step drove the queue 1.4 ms under the new target for 10 s, under the
  floor in exclusive mode), so `latency --settle` and `quality --settle` wait for it to settle (up to 308 s) before they
  measure; in a shorter run it is the fixed one.
  Its floor is a whole period + the margin (12 ms shared, 4 ms exclusive), the design's reasoning rather than the
  simulation's half period, plus 1 ms of headroom over the largest dip in two minutes; it never goes over the fixed
  target.
- **What the adaptive target costs:** below the fixed target, a rare hiccup of 10-15 ms that the spare block covered (a
  missed wake) runs the device dry: a glitch. In the event simulations, with the event after the target has come down,
  a wake 15 ms late did so in half the phases and drifts tried, and one 10 ms late at the worst phase. An underrun under
  the fixed target is a strike: back to the fixed target at once, held there by the need it showed for 10 minutes (20
  after a second strike), and fixed for the run after a third, so a run has at most three underruns more than with the
  fixed target. Without that memory hiccups rarer than the two-minute window came back each time (a wake 15 ms late every
  150 s: 7 underruns in 1000 s, none with the fixed target); with it, three in 50 minutes and none after. The live runs
  should watch the output underruns over a long `run` (and the target in its status line jumping back to 22 or 14 ms).
- The chain's 9-10 ms comes on top of all of these; the cable's own transit (52-62 ms, cable-only) isn't in the readout,
  and the latency test's "app + output cable" says whether exclusive mode changes it.

## Soak runs (built 2026-11-02)
`run --seconds N` (up to a day) stops after N seconds and, like any `run`, ends with a summary of the whole run (app.md):
the dropouts (and the reference's after its first 5 s), the output's events, the readout (the seconds' medians, and their
p95s with the worst second), the budget's means and the median second's p95s, the drift, the output target, clipped seconds
and real-time heap allocations. After the loop it waits for one more once-a-second publication (up to 1.5 s,
`Telemetry::publishes`) and counts it, so an event in the final second isn't lost. Exit 6 when it wasn't clean (mic glitches
or stalls, output underruns or coarse drops, a fault, or a real-time heap allocation), 3 when Ctrl+C cut it short, else 0: a
30-60 minute soak reads as one result. Only with `--seconds`: a run without it exits 0 whatever its summary says (ours: an
interactive run isn't a test, and the adaptive target allows up to three underruns a run). The latency p95 is the seconds'
p95s (their median and the worst), not one over every block, which would need a per-block histogram in the engine.

## Soak (2026-11-02)
`run --mic "CABLE-A Output" --output "CABLE Input" --aec --reference "CABLE-B Input" --seconds 1800` on the dev VM's cables,
the defaults (exclusive mic and output, the adaptive target), a silent mic (nothing played into cable A), the engine-track
build (bf5534e). The log is `Downloads\nf-scratch\soak-1.txt` (outside the repo). Exit 6: it glitched once.
- **Latency:** readout 20.4 ms median (the seconds' medians 18.7-28.0; the worst, 28.3 ms, at 137 s, just after the
  underrun below), p95 20.7; budget 0.8 + 6.8 + 8.9 + 5.5 = 22.0 ms, the chain's 19.0 on top: 40.9 ms. CPU about 100 us
  a block; 0 real-time heap allocations; 0 mic glitches or stalls.
- **One output underrun, at about 122 s:** the adaptive target's first walk down (14 to 9.9 ms in 9 s, heading for about
  6.8) went under what the cable needed. The strike put it back to 14 ms; it walked down again from 233 s, held at 10.9
  from 270 s, and came down to 6.8 by 751 s, with no underrun in the 17 minutes after. As designed (at most three strikes),
  but a slower first walk, or one that stops at the measured need plus more than 1 ms, would avoid it. Built: the trial
  below (first as two stages). What ran the cable dry isn't known (the run kept no chain recording): the queue was at 10.6 ms against
  the 9.9 target, so a block about 8 ms or more late, or a late answer to the exclusive output's event; the render side
  read 2 ms (a period) higher for the rest of the run after it, which a stale period not counted in the device stream's
  index would do. A soak with the chain recorder armed would say.
- **The reference** (cable B's loopback, carrying only the keep-alive's silence): 171 short takes after the first 5 s, in
  three bursts (about 66 by 19 min, 113 by 23.5 min, 175 by 26 min) and 2 jumps (one before 19 min, one at 26 min). Each
  jump makes AEC3 reconverge for 5-10 s. Cable B's loopback hiccupping, as in #10's traces; real speakers to compare.
- **Unexplained:** the output drift reading climbed from about 0 to +98 ppm over the last 30 s of the run, with the latency
  unchanged. Check in the next soak whether it's real (VB-CABLE changing rate) or the reading.

## The first lowering after a trial (built 2026-11-02, ours)
`out_target.rs` ("Two windows before the first lowering"; engine.md, "The output target"). Until proven (the run's start,
and after a strike), a window that would lower the target starts a trial instead: the target holds where it is (the
fixed target) for a second window, and comes down only if that one passes without an underrun and allows it too, on its
own need; a trial whose window doesn't allow it has failed, and the next window that allows one starts another. Proven,
each lowering goes at once. The end point is the same, a window later: in the soak's case 14 ms to 240 s, 6.8 by about
290 s instead of 165. `latency --settle` and `quality --settle` wait past the trial (it's published,
`output_target_on_trial`), up to 308 s instead of 188.
- **First built as two stages** (the same day, replaced after review): the first lowering stopped halfway and held there
  for the second window. The verifier's sweep and ours showed it no safer than the soak needed: the soak ran dry with its
  queue at 10.6 ms, on a block 8 ms or more late (about 10 in the simulations, a missed wake), and held halfway
  (10.1-10.4 ms), or at a period + a block (12 ms, tried next), the target ran short with a block 10 ms late (a block more
  than a block late, with the fill under a period, is a deficit: zeros), where the fixed target rode out 13.5. A first
  lowering under 2 ms also went at once and counted as proven, so the next, however big, skipped the halfway stop.
- **Simulated** (`pipeline/tests/exclusive.rs`, `the_first_lowering_waits_for_a_second_window`; the soak's conditions,
  both exclusive, 160-frame packets up to 1.8 ms late): the trial and the old single stage end at 6.8 ms within the jitter
  their windows saw, nothing underrunning. A block 11 ms late at 129 s (the single stage 9 s into its walk, at 9.7 ms, as
  the soak's was at 9.9) runs the single stage dry, a strike back to 14 ms for ten minutes; the trial, at 14 ms, rides it
  out and fails on it, another trial passes once it has left the window, and the target ends within 0.3 ms of the same
  point. A block 13 ms late at 180 s, mid-trial, is ridden out too. On a 5 ms shared device (`budget.rs`,
  `an_underrun_raises_the_adaptive_target_at_once`) a wake 13 ms late at 200 s runs the single stage's lowered target dry
  and not the trial; it ends at the single stage's end point. A wake 15 ms late every 150 s (`events.rs`,
  `rare_hiccups_cost_the_adaptive_target_at_most_three_underruns`) fails every trial: no underrun in 50 minutes, where the
  single stage struck three times.
- **What the trial covers** (swept by hand, the same conditions, a block late by X on top of its jitter): at 129, 150, 180
  and 230 s up to 13.5 ms (14 ran it dry at 150 and 230); the single stage 9 s into its walk up to 10.5 (11 ran it dry).
  A hiccup rarer than the two windows, coming after them, is the strikes' business as before; one larger than the fixed
  target covers runs it dry either way.
- **The traces** in `%LOCALAPPDATA%\noise-filter\traces` (the cables' shared mic and loopback, 20-30 s each), the mic's
  delivery looped to 330 s on a shared 10 ms device (`timeline.rs`, ignored `traces_and_the_adaptive_target`, which
  prints): needs of 3.4-6.9 ms; the single stage settles at 16.7-20.0 ms by 180 s, the trial at 16.4-19.9 by 300 s, and
  neither underruns. They hold no hiccup the first window misses, so they show the end point kept, not the underrun
  avoided; the exclusive mic's delivery (the soak's case) is the simulation's above.
- **For the live check:** a 30-minute soak on the cables, the defaults. Expect the target at 14 ms to 240 s, then 6.8 by
  about 290 s, and no underrun in the first four minutes. With the chain recorder armed (`quality --save`, or a recorder
  in a longer run) an underrun would show what ran dry. The cost to check against: 7 ms more latency for minutes 2 to 4
  than the single stage had.

## Soak 2 (2026-11-02, the engine fixes, b53a965)
The same command and conditions as the first soak; the log is `Downloads\nf-scratch\soak-2.txt`. Exit 6.
- **The new first walk works:** the target held at 14 ms in its trial until about 344 s, walked to 6.9 by about 390 s,
  and nothing underran during or after it.
- **The exclusive mic was perfectly steady:** 180,100 wakes of one block each, none of two or more.
- **One output underrun anyway, at about 110 s, at the fixed 14 ms target** (before any lowering): a hiccup bigger than
  the fixed target covers. The first soak's underrun was at about 122 s, 9 s into its first walk, so it may have been the
  same kind of event rather than the walk. Two soaks, both near the two-minute mark: suspicious, not yet explained.
  (Times are from the count of status lines, which skips a second now and then: 1734 lines for 1801 s.)
- **The reference had 1,109 short takes** (the first soak 171), mostly from about 92 s to 595 s and again at about
  1234-1251 s, and 2 jumps: cable B's loopback, carrying only the keep-alive's silence, misbehaving for minutes at a time.
- **Latency:** readout 20.1 ms median, p95 20.6; app path 40.3 ms with AEC3 and RNNoise. 0 real-time heap allocations.
- **The output drift reading** again read high at the end (+69.6 ppm; the first soak +97.9), with the latency steady.
- **Next:** a flight recorder (the last seconds of per-block facts kept in memory and written out when an underrun or a
  reference jump happens), so a soak or a user's run shows what ran dry.

## Precedents
- **RTL Utility 1.0.13:** free; a train of narrow impulses plus correlation. Its guide warns WASAPI
  shared-mode results are often invalid from its CLI.
- **OboeTester:** the coded-pulse design above.
- **jack_delay:** tone phases, ~1/1000-sample accuracy, linear paths only.
- **WebRTC's ADM loopback test:** one impulse per second, ms resolution.

## Output
- **Into #14's scorecard:** latency (median, p95, spread), CPU p50/p99, dropouts, plus the
  conditions (engine period, RAW, cable settings).
- **Saved per device pair,** so tuning (periods, drift target, floor, gate) has a baseline.
