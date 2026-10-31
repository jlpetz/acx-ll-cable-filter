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
- The app's live readout (56-66 ms) is above its design (about 32-42 ms: a block of input, the chain's 10 ms, the 22 ms
  output target). The cable-only runs below show the readout is right: the excess is the engine's.

**Cable only** (2026-11-01): `latency --cable-only --reps 5`, each cable's Input to its own Output, both streams at a 10 ms
period (as the engine's mic was). Each figure is one run's median; the sweeps within a run spread by 1-2 ms.

| Cable (its Latency) | Probe to capture |
|---|---|
| A (3072) | 60.8 / 52.3 ms |
| Base (3072) | 62.3 / 60.2 ms |
| B (7168, the default) | 61.7 ms |

- Each cable costs 52-62 ms by itself: all of the virtual loop's input path (52-62 ms). So the engine's own part of app +
  output cable (118-127 ms) is about 60-65 ms, which is what its readout says. VB-CABLE's stamps don't inflate the readout;
  the engine holds about 20-30 ms more than its design.
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
