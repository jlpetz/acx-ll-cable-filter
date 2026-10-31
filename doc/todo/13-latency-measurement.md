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
  output target). Some of it is probably VB-CABLE's own timestamps and play position; a cable-only measurement (a sweep
  through one cable, no app) is next, to separate the two.

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
