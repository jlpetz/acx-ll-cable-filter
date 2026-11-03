# 10. Echo cancellation (WebRTC AEC3 via `sonora`)

Facts and sources: `doc/research-2026-10.md` (Echo cancellation).

## Choice
- **`sonora`:** a pure-Rust port of WebRTC's audio processing (BSD-3), with Windows CI. It runs AEC3 at 48 kHz in 10 ms frames; about 45 µs per frame on an i9-14900K once denormals are handled.
- **Ruled out:**
  - tonarino `webrtc-audio-processing`: doesn't build on MSVC (PR #102 open).
  - SpeexDSP: no delay estimator, linear only.
  - Voice Capture DSP: at most 22.05 kHz.
  - the `aec3` crate (RubyBit/aec3-rs 0.4.0; MIT OR BSD-3-Clause, with a LICENSE GitHub can't classify): its README calls it a work in progress, with an API expected to change.

## Getting a real-time-safe build
- **0.2.0 vs main:** the release (2026-07-29) is the M145 port. Main has further upstream fixes up to M156, including render-buffer headroom on underruns, which matters for loopback gaps, and sonora #37 (fmaf ignoring FTZ under `+crt-static`).
- **Open PRs:**
  - sonora #38: denormal stall on a silent reference, stacked on sonora #36.
  - sonora #40: allocation-free processing.
  - sonora #41: custom AEC3 config. One downstream user needed it for double-talk tuning; it also exposes `num_filters` for a wider delay window.
- **`mul_add` (sonora #37) no longer matters to us (2026-10-28):** the build moved to x86-64-v3, where each `mul_add` is one FMA instruction, so the exes call no `fmaf`. The vendored 0.2.0 with the AEC path's `mul_add`s as plain arithmetic (2026-10-26, AEC3 91 → 55 µs a block at v2) went with the move; crates.io's 0.2.0 takes about 50 µs at v3 (`doc/dev/build-report.md` §11).
- **Plan:** vendor a fork of main with sonora #38 and #40 (and #41) merged. Until then, FTZ/DAZ on the DSP thread plus ~1e-9 dither on the reference, which zero-filled gaps make mandatory.
- **One thread:** sonora's API takes `&mut self` for both render and capture, so one DSP thread owns the instance and feeds the reference frame before the matching mic frame.
- **Full band:** set `MaxProcessingRate::Rate48kHz`. sonora's default (`Rate32kHz`, as in upstream WebRTC) caps the whole capture path at 16 kHz whatever noise model runs.
  - Rate48kHz routes audio through WebRTC's three-band filter bank (0-8/8-16/16-24 kHz). It's not perfect-reconstruction (~9.5 dB round-trip SNR), so A/B it against 32 kHz in #14.
  - AEC3 runs its linear filter on the bottom band only. Above 8 kHz it applies one scalar gain plus comfort noise, so leftover far-end speech above 8 kHz is handled coarsely, whatever the noise model's rate.

## As a stage (proposal, #17, awaiting the owner)
- A CLAP-shaped stage (#17): main input = mic, second (non-main) input port = speaker reference,
  main output = echo-cancelled mic.
- The engine resamples the reference to the mic clock (#9) and feeds it in the same 10 ms block as
  the mic.
- Latency is reported through the latency extension: 430 samples (8.96 ms) at 48 kHz, measured with an impulse and GCC-PHAT (`nf_stages::AEC3_LATENCY`, `crates/nf-stages/tests/aec3.rs`). The earlier ~4.5 ms was a code-reading estimate and too low (`doc/dev/api/sonora-nnnoiseless.md` 1.8).

## Behaviour and UX
- **Order:** AEC → noise model → floor/gate. Nothing nonlinear or time-varying runs before AEC.
- **Residual echo:** the noise model keeps leftover far-end echo, because it's speech. So AEC3's residual-echo suppressor, which sees the reference, stays enabled and does that job. A joint mic+reference neural model is the higher-quality later option (see Later).
- **One frame clock:** all stages share the 10 ms frame, with no re-buffering between them. That's the useful part of the "single pass" idea; separate FFTs per stage cost microseconds.
- **Opt-in "using speakers" mode,** off for headphones. With closed headphones there's little echo, so AEC there would mostly risk double-talk artifacts (our inference, unverified).
- **Warnings and guards:**
  - Block the virtual cable, and our own monitor output, from being picked as the reference.
  - Warn when the noise filter is on without AEC while on speakers: Discord's own AEC then sees an echo path through a time-varying denoiser.
- **Stats:** show AEC3's delay estimate and ERLE.
- **Delay window:** the default delay-estimation window is about 608 ms (our calculation from the code). Bluetooth or USB speakers with large output latency may need a bigger window (custom config, sonora #41).
- **Reconvergence:** volume changes (loopback is pre-volume), render-side enhancement APOs and stalls all trigger it; one report took about a minute after a heavy stall.

## First live test (2026-11-01)
`quality --test echo` over the dev VM's VB-CABLE cables (#14's detail file has the runs): AEC3 removed only 1-5 dB of echo a
far-only turn (7-23 dB with the base cable as the far end), where the same scene offline removes 21-53 dB, with a 96 ms
lead and 30 ppm of drift too. The engine counted 5-8 reference gaps a second whichever cable carried the reference: its
reference FIFO (target one loopback period + one block + 96 samples, 22 ms) found less than a block at a mic wake. Each gap
zero-fills to the target and resets the drift loop, and the coarse correction later discards the excess. Both move the
reference against the mic, so AEC3's linear filter never holds (its delay estimate wandered from 96 to 116 ms in one take).

Next:
1. Log the loopback packets (count, size, QPC) against the mic wakes on the cables, to see whether the loopback comes in
   bursts (VB-CABLE's statistics show 1024-frame buffers) or stops.
2. Proposed fix: a late loopback packet should be waited for, not zero-filled. The reference's latency costs nothing at the
   output, only its lead over the echo, so a deeper target (raised on an underflow, as a jitter buffer would) keeps the
   timeline whole. It has to stay inside the echo path's own delay (loopback taps at mix time, ahead of the speakers).
3. Then rerun the echo test, and on real speakers in phase 2.

## Fix (2026-11-01): the reference timeline
The probe (`live_probe.rs`, step 1) traced the cables: the mic delivers 0, 480 or 960 frames a 10 ms wake, its data 19-21
ms old, its QPC stamps in 0.5 ms steps within ±1 ms; the loopback steadily or in bursts of up to 1440, its stamps within
0.05 ms; loopback frames against mic frames swing by 20-30 ms; the stamps put the clocks 1-2 ppm apart. Four 30 s traces
are in `%LOCALAPPDATA%\noise-filter\traces` (not in git; the probe writes there, built with `--features live-probe`).
`reference.rs` replaces the reference FIFO (engine.md, "The reference", has the details):
- a virtual read position: missing samples are served as zeros and owed (discarded when they come), so the timeline
  stays whole;
- the loop holds the *lead* (the mic block's capture time less the loopback tap time of the sample served with it) from
  both streams' stamps, which bursts don't reach: slips at a least-squares drift (20 s window, trusted once its standard
  error is within 10 ppm), corrections only outside ±0.5 ms;
- a lead target of one period + 2 ms, raised in the first 2.5 s to what three wakes' fresh takes needed + 3 ms (the
  cables: 13.5-23.4 ms); after that raised only when underflows persist (6% of the takes for good, or 0.6 s in a row),
  lowered after five minutes 3 ms or more under, capped at 50 ms;
- one jump undoes a broken timeline (a step of the lead past 4 ms in the median of 9 readings and in 7 of them);
  loopback samples that never came (lost, or silence without the keep-alive) are zero-filled once two stamps agree (or
  the position does), so they need none;
- a stream without stamps, or whose stamps scatter past 3.5 ms (stamped at delivery, or noise), is timed by delivery:
  no slips, a jump of a packet per step of the count (once a beat of the clocks), breaks undone and centred.

### Review round (same day)
The first version had holes, found by a review with scratch scenes:
- **Silence without the keep-alive raised the target.** Over 2.2 s of it every take counted as short, and the target
  jumped to near the cap for good. Now a take counts only while the loopback is fresh (delivered within two periods and
  5 ms), so 3 s or 10 s of silence leaves the target and the path where they were (no jump).
- **A stall in the start-up ratcheted the target to the cap** (a 100 ms stall at 1 s: 47 ms, for the session; non-causal
  on any echo path under it). Now a need counts once three wakes reached it, and a run of wakes counts only once the
  loopback delivers again (a stall's first wakes are fresh but need more each wake). And the target comes back down.
- **Without stamps, a loopback slower than the mic lost the reference for good** (all-zero blocks from about 100 s at
  -300 ppm, 13 min at 30 ppm). Now a 3 s mean of the delivery-timed lead puts each step of the count back in a jump of a
  packet: five over 170 s at ±300 ppm, one block of zeros per jump back, and nothing else lost.
- **Noisy stamps made jumps** (±5 ms jitter: 9-10 in 37 s; stamps put on at delivery: 178). Breaks now go by the median
  and 7 of 9 readings, with thresholds scaled by the stamps' jitter, and stamps that scatter past 3.5 ms are given up
  for delivery timing: ±3 ms jitter stays stamped with no jump, ±5 ms and delivery stamps switch with none (one count
  step at 300 ppm in 40 s).
- **One loopback stamp 5 ms off put 240 zeros in** and two moves of the path. Now it needs the next stamp (or the
  position) to agree: a stamp off on its own moves nothing.
- **The offline AEC3 check measured AEC3's suppressor**, not its filter. It now has the quality test's turns (far only,
  double talk, near only; pink noise), the echo path at 25, 40, 60 and 100 ms, a start-up stall and 3 s of silence, and
  asserts AEC3's own ERLE.
- Found on the way: a drifting bursty loopback's phase against the mic's wakes sets its need, and the start-up sees one
  phase. At the worst one, 13-20% of its blocks came up short for 30 s of every 100 (100 ppm), which cost AEC3 3-4 dB
  of its ERLE throughout; a fifth of the takes for good was too high a bar to raise for it. At 6% it raises once, at the
  first such phase, and every block is whole after.
- The quality report, `run`'s status line and the GUI show the lead, the target and whether stamps timed it; the report
  counts slips and jumps over the take and warns when the target passes 30 ms.

Replayed through the pipeline (`traces_hold_the_echo_path`; moves, short blocks and the lag after 3 s; before: the
committed pipeline, the implementer's figures):

| trace | gaps | drops | slips | path moves | short blocks | lag after 3 s | target |
|---|---|---|---|---|---|---|---|
| CABLE_Input-1, before | 237 | 235 | 326 | 314 | 171 all zero | 74..1536 | |
| after | 3 | 0 | 2 | 0 | 0 | 1090..1092 | 23.3 ms |
| CABLE-B_Input-2, before | 206 | 205 | 414 | 290 | 152 all zero | 28..1056 | |
| after | 3 | 0 | 2 | 0 | 0 | 1101..1102 | 23.4 ms |
| CABLE_Input-3, before | 283 | 282 | 88 | 333 | 197 all zero | 88..1056 | |
| after | 0 | 0 | 1 | 0 | 0 | 620..621 | 13.7 ms |
| CABLE-B_Input-4, before | 10 | 6 | 2021 | 15 | 9 all zero | 75..1056 | |
| after | 0 | 0 | 1 | 0 | 0 | 617..618 | 13.5 ms |

(The 3 gaps are start-up takes, before the target rose to what the bursty mic's late packets need.)

Offline AEC3 on those deliveries (`aec3_converges_on_the_traces`: the quality test's turns, the far voice through a room of
RT60 0.15 s at 25/40/60/100 ms behind the loopback by the traces' own stamps, against the same audio delivered steadily):
- Near voice silent: AEC3's own ERLE, live/steady, 13.4-16.1 / 15.4-16.6 dB across the traces and delays (within 0-2.9
  dB, the 23 ms leads lowest); the echo removed where the far voice sounds 61-62 dB for both (the noise floor caps it).
  Before the fix the check's first version (a continuous far end, no near voice) had AEC3's ERLE at 0.9-5.9 dB against
  about 9.6 dB with steady delivery.
- Near voice in its turns: in double talk the near voice's own share is 6.7-7.4 dB down, live and steady alike (AEC3's
  suppressor); AEC3's ERLE moves by up to 10 dB from run to run, steady included (its own divergences in double talk).
- Synthetic: a bursty loopback (lead 27 ms) in a 25 ms echo path isn't causal (AEC3's delay estimate reads 0, its ERLE
  8.5 dB against 14.6 steady); in 40 ms and up it's within 1 dB. A 100 ms start-up stall and 3 s of far-end silence
  change nothing; a bursty loopback drifting 300 ppm, raised once, ends 0.7-2.7 dB under steady.
- The check still doesn't reproduce the live 1-5 dB: the reference path explains AEC3's own low ERLE, maybe not all of
  the live result (the quality test's measure, the live echo path's 96-116 ms).

### Live again (same day)
- `quality --test echo --no-ns --seconds 60` with the fix: reference gaps 1 (was 450), 0 slips, 0 jumps, the lead 14.0 ms
  (target 14.1), but the echo still dropped only 2.0 dB and AEC3's own ERLE was 0.2 dB.
- A live recording of the mic and the loopback side by side, with their QPC stamps (a temporary probe, kept outside the
  repo), showed why: **the cable rig was non-causal.** The echo, pre-mixed into cable A's stream, reached the mic about 40
  ms before cable B's loopback stamp for the same far audio (cable B at its default Latency, 7168), and about 10 ms before
  the base cable's (3072). VB-CABLE's loopback stamps run late by part of its Latency. On real speakers the echo comes out
  of the device the loopback taps, so it always follows the tap; only this rig puts it ahead. Offline AEC3 on that
  stamp-aligned live pair removed 2-8 dB; with the mic 100 ms later, 23-56 dB.
- So the quality test's echo scene then played the far voice 100 ms ahead of its echo (`ECHO_DELAY_S`, a speaker's output
  latency; `far.wav` is saved as played). Live: the echo drops 14.4 dB (far-only turns 12-18 dB; the first, converging
  turn 21 dB), the lead 14.5 ms, 3 slips, 0 jumps, 3 start-up gaps. (Since replaced by `--echo-delay`, 25 ms after the
  reference's tap by default, with the rig's offset calibrated in each take and the delay measured after it: #14's detail
  file. Re-read, the six chain-recorded takes below ran at echo delays of 14-63 ms: AEC3 got the echo 0.7-51 ms after its
  reference.)
- Still short of offline: the same saved scene through `process --ref far.wav` drops 46-57 dB a turn once converged (22
  dB in the first), and the live mic and loopback recorded together (aligned by stamps at a 14 ms lead) got 49-56 dB. So
  the signals can be cancelled, and something in the engine's live path still differs. AEC3's own figures live: delay
  0.0 ms, ERLE 5.9 dB.

Next:
1. Record what the chain is fed live (the mic and reference blocks, with the chain's output) next to `quality --save`'s
   WAVs, and run AEC3 offline on exactly those: it tells the reference's content and timing from the live processing.
   It's #14's phase 1 (save the raw take and the loopback) in a small form.
   **Built and run (2026-11-01):** the engine's chain recorder (`Engine::record_chain`, engine.md "The chain
   recorder"). `quality --save DIR` arms it before the start and writes `chain_mic.wav`, `chain_ref.wav`, `chain_out.wav`,
   `chain_blocks.csv` (a row a block: capture QPC, the mic's discards and skips, the reference's read index and so the echo
   path in samples, its lead reading, slips, owed zeros, jumps and why, AEC3's delay and ERLE) and `chain_config.json`. In DIR, `noise-filter process
   chain_mic.wav chain_rerun.wav --ref chain_ref.wav --config chain_config.json --warm-up` reruns the live chain; it should
   match `chain_out.wav` sample for sample (it does offline, through bursty delivery: `pipeline/tests/recorder.rs`). If it
   does, the chain behaved live as offline and the difference is in what it was fed: compare `chain_ref.wav` against
   `far.wav` and `chain_mic.wav` against `input.wav` (what the cables delivered against what was played; the path's moves
   in the CSV). If it doesn't, something in the live process differs from `process`.
   **Result:** six live 60 s takes with the recorder (`echo-5` to `echo-10` in the dev VM's
   `Downloads
f-quality`, outside the repo), `--no-ns`: the far-only turns after the first dropped the echo by a median
   of 57-60 dB each take (lowest turn 21-55 dB; the first, converging turn 19-40 dB), AEC3's own ERLE 11-16 dB, no
   reference gaps, 0-4 slips, no jumps. The offline rerun matched `chain_out.wav` exactly (largest difference 0). So the
   engine now cancels live as well as offline (46-57 dB on the same scene). The earlier 14 dB take (`echo-4`, without the
   recorder; 3 reference gaps and 3 slips in its start-up, AEC3's delay estimate 0) didn't recur in the six: unexplained,
   possibly a bad start-up. If it does recur, its chain recording will show why.
   **Since then** the echo test calibrates a realistic echo delay in each take (`quality --echo-delay MS`, default 25 ms
   after the reference's tap, measured afterwards from the chain recording). The old fixed 100 ms gave 14-63 ms in
   practice: cable B's offset moved about 10 ms a take. The sweep below maps the shortest echo that cancels.
2. On real speakers in phase 2, check that the lead stays under the echo path's delay (12 ms expected with steady
   delivery; a target over 30 ms is flagged). A bursty device needing more lead than its echo path has can only be owed
   blocks or be non-causal: AEC3's delay estimate near 0 with a low ERLE would say so, and could cap the target (not done).

## Echo delay sweep (2026-11-01)
`quality --test echo --no-ns --seconds 30 --echo-delay D`, cable B as the far end (the base cable at 25 ms: 56 dB, offset
-17 ms). Each row one take; the drop is the median far-only turn (the noise caps the figure near 55 dB). The lead was
12.0-14.8 ms. The rig's own offset (where an undelayed echo lands against its reference's tap) was -72 to -96 ms on cable
B, found by the calibration each take.

| Echo delay (asked / measured) | AEC3 sees it after its reference | Drop, median (lowest) | AEC3's ERLE |
|---|---|---|---|
| 0 / 0.0 ms | -14.1 ms | 11.0 (7.9) dB | 6.6 dB |
| 5 / 4.9 ms | -9.8 ms | 16.7 (12.4) dB | 6.7 dB |
| 10 / 10.0 ms | -4.0 ms | 38.6 (33.5) dB | 8.5 dB |
| 13 / 13.0 ms | -0.8 ms | 44.9 (39.5) dB | 10.7 dB |
| 15 / 15.0 ms | +1.1 ms | 52.9 (52.7) dB | 17.9 dB |
| 20 / 20.0 ms | +5.3 ms | 52.5 (51.5) dB | 16.7 dB |
| 25 / 25.0 ms (three takes) | +10.9 to +13.0 ms | 51.1-52.8 (45.3-51.1) dB | 16.9-17.8 dB |
| 40 / 40.0 ms | +25.9 ms | 52.2 (52.0) dB | 16.3 dB |
| 100 / 99.9 ms | +85.3 ms | 52.0 (45.4) dB | 16.0 dB |

So the echo cancels fully once it reaches the mic at least the lead (12-14 ms) after its reference's tap, and falls off
below: the lead is the floor on the echo path the engine can serve. Shared-mode speakers are usually past it (an engine
period plus the device's buffer), but low-latency ones may not be. Lowering it means a smaller loopback period (the lead
starts at one loopback period + 2 ms, the newest reference a mic block can be sure of), or letting the mic wait for the
reference only when the echo path is that short, which costs that wait in latency.

With the exclusive mic (the default since 2026-11-02, #13; `--no-exclusive-mic` or the window's "Exclusive microphone" switch turns it off: mic blocks reach the app 0.8 ms after capture instead of about 16) the lead rose to 18.3 ms: each mic block is processed before the loopback, still on its 10 ms shared period, has delivered as recent a reference. The floor rises by as much, to about 20 ms (`quality::echo_delay::EXCLUSIVE_MIC_ECHO_FLOOR_MS`), and applies by default now: speakers whose echo comes back sooner than that after the mix echo through more (the window's switch line says to try it off when echo gets through with echo cancelling on; whether the exclusive mic should yield to shared when AEC is on is open, ours to propose). At 25 ms (the echo test's default, over the floor) it cancelled a median 54.5 dB a far-only turn.

## The echo-path guard (built 2026-11-03, ours)
The floor above is the lead: an echo that reaches the mic sooner after its tap comes before its reference, and AEC3 can't
place it. The two ways down were a smaller loopback period or a mic wait only when the path is that short. The first is
dropped (#13's detail: the speakers' own output path shortens about as much as the loopback's packets, so the margin
doesn't reliably grow, and every app on the speakers would run at the small period). The second is built, as the
echo-path guard (`echo_guard.rs`; engine.md, "The echo-path guard"; `io.echo_guard`, on by default, `--no-echo-guard`):
- It holds the mic back (each block processed later, read by the reference loop as captured later, so the reference served
  is that much newer) only for an echo seen that AEC3 can't place: the mic's energy envelope following the far end's
  (correlation 0.4 or more over 5 s; headphones show none), AEC3's ERLE under 10 dB and its delay estimate 0 or 4 ms.
- On a trial first: the hold that brings the lead to 2 ms, kept only if AEC3's ERLE then rises 4 dB to 10 or more; trimmed
  once to what AEC3's delay estimate says the echo needs, plus 4 ms; let go when the echo goes or the hold stops helping.
- Why a trial: AEC3 reads its delay in 4 ms steps less its headroom, so a short echo it does place reads 0 as well; with
  no echo at all (headphones) the ERLE is low too. Only the ERLE after a trial tells.
- Offline (`the_echo_path_guard_holds_the_mic_for_an_early_echo`, steady 10 ms delivery, a 12 ms lead, 100 s of the echo
  scene through AEC3), AEC3's ERLE over the last 20 s:

| Echo after the tap | Guard off | Guard on (hold) |
|---|---|---|
| 5 ms, near voice silent | 6.9 dB | 19.3 dB (10 ms) |
| 40 ms, near voice silent | 19.4 dB | 19.4 dB (none) |
| 5 ms, near voice in its turns | 6.8 dB | 15.9 dB (10 ms) |
| 40 ms, near voice in its turns | 15.5 dB | 15.5 dB (none) |

  AEC3's delay estimate read 24 ms for the 40 ms echo's 28 ms past the lead: it errs short, which the trim's margin
  allows for.
- Live: not yet. The rig takes are scripted (`quality --test echo --echo-delay 5`, the same with `--no-echo-guard`, then
  10 and 25 ms, 150 s each); on 2026-11-03 the owner's RDP session was disconnected, so the session had no audio endpoints.

## Later
- **Optional Windows path:** open the mic in Communications mode, check `IAudioEffectsManager` for AEC/NS, and set the reference with `IAcousticEchoCancellationControl` (22621+; E_NOINTERFACE when unsupported).
  - Where the OEM supplies no Communications-mode processing, this path gets Voice Clarity (Windows 11 client editions).
  - Whether Voice Clarity accepts a chosen reference through `IAcousticEchoCancellationControl` is unverified. Without that, its reference is the default render device.
- **Neural AEC (v2 experiment):** LocalVQE (Apache-2.0, 16 kHz, GGML C++). v1.4-AEC has 203K params; the current v1.3 is a joint AEC+NS model with 4.8M params.
