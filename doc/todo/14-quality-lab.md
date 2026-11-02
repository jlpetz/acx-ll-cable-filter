# 14. Quality analyser: the Lab mode (MOS)

Status: designed 2026-10-26 (the owner wants it in the app, not as a companion). Metric licences
and inputs were fact-checked the same day against the Microsoft DNS-, SIG- and AEC-Challenge repos
and the metric papers.

## Why in the app
- **Scores come from the exact code that ships:** the same model packs, framing, AEC and gate. A
  companion app would duplicate or diverge.
- **One report:** latency (#13) and quality share it.
- **Kept off the live path:** scoring runs on a worker after recording, never on audio threads.
- **A CLI equivalent** (e.g. `lab run playlist.toml --models all --out report.json`) for batch
  runs. The offline batch also runs as `cargo test`; `cargo bench` needs the owner's OK (the
  project rules in `CLAUDE.md` list only check, clippy, build and test).
- **Packaging:** the metric models' licences allow bundling, but they come as a download on first use of the Lab (SIGMOS alone is 26 MB), so the base install stays light.
- **Dependency:** the metrics run through ort. If the model path doesn't already ship ONNX Runtime,
  the Lab adds it (download it with the metric models).

## Record once, process many
Playing the playlist live through model A, then B, then C gives each a different take: room noise,
mic distance and fan speed change. So:
1. **Offline batch** (the main quality comparison): the same WAV files go through every model,
   faster than real time, repeatable.
2. **Live capture:**
   - Play the playlist through speakers (or a cable). Save the raw mic take and the loopback next
     to the processed output.
   - Then run the raw take offline through every other model.
   - Also score the live cable-B capture itself: offline re-processing hides real-time failures
     (underruns, dropped frames), and SIGMOS's DISC dimension catches discontinuities.

## Metric models
| Metric | What it gives | Licence / size | Input | Use |
|---|---|---|---|---|
| DNSMOS P.835 (+ P.808, personalised) | SIG / BAK / OVRL predicted MOS | MIT code + CC BY 4.0 content (ship both notices); real files, ~1.4 MB | 16 kHz; 9.01 s windows stepped by 1 s; `sig_bak_ovr.onnx` takes raw waveform, then fixed polynomial calibration; `model_v8` takes a librosa mel spectrogram | Lab download |
| SIGMOS (P.804) | 7 dimensions incl. COL, DISC, NOISE, REVERB, OVRL; full band | MIT; 26 MB (Git LFS) | 48 kHz; host STFT 960/480, sqrt-Hann, 0.3 compression | Lab download; **no published accuracy**, so treat as uncalibrated until checked against our own blind ratings |
| AECMOS | echo score + "other degradation" | MIT; 48 kHz model 2.7 MB | loopback + mic + output; the 48 kHz model needs a talk-type label per segment (near-end, far-end or double talk); a 16 kHz model needs none | Lab download, for mode B (real echo path) only; skip the AEC convergence time at the start of each take |
| ScoreQ (no-reference "telephone") | independent cross-check | MIT code, CC BY 4.0 weights, 378 MB | 16 kHz waveform | optional extra download |
| Left out | NISQA (CC BY-NC-SA weights), SQUIM subjective (CC BY-NC), UTMOS (trained for TTS naturalness) | | | not bundled |

Notes:
- **Run them through ort.** DNSMOS uses only basic ops (its STFT is built from Conv layers).
  SIGMOS and AECMOS run in Python onnxruntime; load-test them under ort.
- **Port the feature extraction to Rust** (librosa mel; SIGMOS STFT) and check it against golden
  outputs made once with the reference Python scripts.
  - Pin and record the librosa version. 0.9 changed the default STFT padding, and 0.10 changed the
    default resampler.
  - Our resampler choice moves scores slightly.
- **DNSMOS works at 16 kHz**, so it's deaf above 8 kHz. Label it so in the report. SIGMOS and
  AECMOS-48k see the full band.
- **Goodhart:** DNSMOS was trained on output from ~40 DNS Challenge suppressors. DNS-trained models
  may score well on it without sounding better.

## Statistics
- **Only averages are reliable.** Per-clip agreement with listeners is low: DNSMOS P.835 PCC
  0.71-0.83 per clip vs 0.94-0.98 per system, and two human P.808 runs agree only ~0.5 per clip.
- **Unit of analysis:** aggregate and bootstrap over **distinct clips** or non-overlapping
  segments, not over DNSMOS's windows, which overlap by ~89%. The paper's system-level figures rest
  on about 50 clips per model; aim for that many paired clips.
- **Reporting:** the mean and the paired difference per metric with a 95% confidence interval.
  When the interval spans zero, show "no difference".

## Listening test (the final say)
- **To choose a model:** blind A/B preference, or P.808 CCR (−3 to +3), with randomised order. ABX
  only answers "can I hear a difference?" (e.g. is the cheaper model audibly worse?).
- **Must-haves:**
  - loudness matching to ITU-R BS.1770 before every trial (the `ebur128` crate, MIT), or the
    louder version wins;
  - looped playback with sample-accurate switching and short fades;
  - a hidden reference and randomised trial order;
  - a binomial or sign test on the results (e.g. 12 of 16 correct gives p < 0.05).
- **Write it fresh:** BeaqleJS is GPLv3, and webMUSHRA is JavaScript under a conditional custom
  licence. microsoft/P.808 (MIT) is the design model (CCR, gold and trapping clips).

## Playlist
- **Speech with:** keyboard, fan, music, café, a barking dog; speech over speech (AEC double talk);
  the owner's own recordings. Takes of 10 s or more (DNSMOS tiles shorter clips).
- **Bundled clips** depend on their licences; otherwise they're downloaded.
- **Don't bundle ITU test signals** ("all rights reserved").

## Phases
- **Phase 0:** the batch CLI with #2: playlist, DNSMOS/SIGMOS feature parity, scorecard.
  - **Its live half, `noise-filter quality` (built and first run 2026-11-01):** plays a scene through the running filter
    over the virtual cables and measures the recording, with levels, not MOS. The noise test is speech in pink noise (10 dB
    SNR). The echo test (12 s at least) is 3 s turns of far only, double talk and near only, with the far voice on `--far`, the
    echo reference, and noise 60 dB under the speech, so the noise doesn't cap the ERLE (at 30 dB it sat 25 dB under the echo).
    Synthetic signals by default, or 48 kHz WAVs, whose speech activity gets a 200 ms hangover (P.56's) so their reverberation
    counts as speech, not pause.
  - **Alignment is windowed**, because one lag for the whole take lost the scene once the path's delay moved by about 1.5 ms
    (drift between the cables, the engine's slip-based drift loop). A coarse delay comes from the cross-correlation of 5 ms
    frame envelopes; then 2 s windows of the clean speech are each correlated within 50 ms of it, for their own delay and
    coefficient. The median coefficient decides (under 0.3 is no result), and the windows' delays make the track the recording
    is read along; a window that locks onto a pitch period (5-10 ms off its neighbours) is dropped. The report gives the
    windows, the delay's range and the drift in ppm, and warns past 2 ms of movement. Measured offline through the chain
    (2026-11-01): 0.49-0.97 aligned, with 100 ppm drift, ±1 ms wander and 1 ms steps included, and 0.97 for 300 s drifting
    50 ppm; 0.10 at most for recordings without the scene (another talker 0.07, the scene 4 s off 0.10). The old single-lag
    figures were 0.035 at most for recordings without the scene but 0.17 for the scene 10-50 ms off its delay (its pitch
    periods), a margin of about 1.8 times; the windows' ±50 ms search finds the true delay instead of such an offset.
  - **Regions:** noise in pauses (and 200 ms clear of speech, past the filter's release) and the speech's own share in its
    syllable cores: the output's projection on the clean speech per 20 ms, so noise or echo left there isn't counted as speech
    (the rest is reported beside it). The echo test measures each region inside its own kind of turn, from the second turn on:
    the echo's drop where only the far end talks (the ERLE; with the noise filter on, the chain's echo reduction), pooled and
    per far-only turn, with the first, converging turn on its own and the ceiling the scene's noise sets; and the near
    speech's share alone, in double talk, and with both talkers mid-syllable. Plus the engine's drift and AEC3's own delay and
    ERLE estimates (the reference-to-mic lag behind a given ERLE), and dropouts (the engine's counters and the recording's
    gaps). `--save` writes input, clean, far, recording and aligned WAVs, which the metric models can score later. The
    analysis is a pure function, tested on synthetic recordings.
  - **First live runs** (2026-11-01, the dev VM's cables: probe CABLE-A Input, mic CABLE-A Output, output CABLE Input,
    capture CABLE Output; WAVs kept outside the repo in `C:\Users\Administrator\Downloads\nf-quality\`):
    - Noise test, defaults: noise in pauses -27.2 dB (-28.9 dB 200 ms clear of speech), speech -0.6 dB, no dropouts. 15 of
      15 windows aligned (lowest 0.44), path delay 191 ms, which matches #13's end to end.
    - Echo test, `--far "CABLE-B Input"`: the echo drops only 2.2 dB (median far-only turn) with the noise filter on, 2.1 dB
      with `--no-ns` over 60 s, and AEC3's own ERLE estimate is 0.2-0.7 dB. Through `process`, the saved scene and its far
      voice give 21-53 dB a turn, and 27-54 dB with the mic 96 ms behind the reference and 30 ppm of drift. So AEC3 copes
      and the live reference path doesn't: 57 reference gaps in 30 s, 450 in 60 s (#10). With the base cable as the far end
      (and cable B as the output) it was 7-23 dB a turn, with 177 gaps in 30 s.
    - After #10's reference fix the gaps were gone but the echo still dropped 2 dB: the rig was non-causal (VB-CABLE's
      loopback stamps run late, so the echo pre-mixed into cable A came 10-40 ms ahead of its reference). The echo scene
      then played the far voice a fixed 100 ms ahead of its echo (`ECHO_DELAY_S`, since replaced by the calibrated delay
      below) and saved `far.wav` as played. Then: echo down 14.4 dB (12-18 dB a far-only turn) against 46-57 dB offline on
      the same scene (#10's detail file).
    - With the chain recorder (`--save` now also writes what the chain got and gave, rerunnable exactly offline), six
      takes: far-only turns a median of 57-60 dB each, as offline. In three of the six the analysis saw the output path's
      delay move by 11-13 ms during the take (2 ms in the others): the output side (#9), not the echo. Looked into
      2026-11-02: each was one step at a run of 9-12 ms of digital silence in the recording, inserted between the filter's
      output and the test capture: the cable's, unless the next take's capture gaps show our recorder filled a position jump
      (`play_record` zero-fills one; the old takes kept no gap counts) (#8's detail file, "The output path's 9-13 ms steps"). The report now prints the output's events
      over the take, and its delay warning names each step and the silence that explains it.
  - **The echo delay, calibrated in each take (built 2026-11-01, not yet run live).** The fixed 100 ms was too generous: a
    real speaker's echo follows the loopback tap by the output's latency and the air, often only 10-30 ms, and the engine's
    lead (12-14 ms on the cables) has to fit under it. `--echo-delay MS` (0-300, default 25) is now the echo's delay after
    the far voice's loopback tap, by both streams' stamps (`quality::echo_delay`):
    - **The rig's offset moves between takes, so it's measured in each.** The six chain-recorded takes above, re-read
      (the far-only turns' echo against `chain_ref.wav` by GCC-PHAT, plus the lead): the echo came 14.5, 24.9, 31.3, 42.0,
      52.4 and 63.4 ms after its reference's tap (AEC3's lag 0.7-51.4 ms; its own delay estimates 0, 8, 12, 24, 36 and
      48 ms agree), so cable B's own offset went from -86 to -37 ms, about 10 ms later each take, while each take held it
      to 0.1 ms. In one take's first 6 s the mic's copy of the probe flipped between two places 11 ms apart (presumably
      VB-CABLE resyncing its buffer), then held. So those 57-60 dB were at 14-63 ms, and a calibration in a take of its own
      would miss by about 10 ms. (`echo_delay`'s ignored `a_saved_take_measures` reruns the measurement on such a take.)
    - **The take:** a 0.2 s sweep on both devices right after `play_record`'s lead (the far end's copy as the reference, the
      probe's as a zero-delay echo); a thread beside the take reads the chain recording every 20 ms
      (`ChainRecorder::copy_from`), finds the sweep in the mic and the reference (matched filter), and takes their lag in
      the chain plus the reference's lead there as the rig's offset. It looks only from half a second before the lead ends
      (the sweep can't come sooner, less the rig's 250 ms room and a 250 ms margin), so a look correlates about a second:
      looking from the run's start took about 220 ms a look in a dev build, most of the time the far voice's start allows.
      The far voice (an `nf_engine::LatePart`) then starts
      the delay less the offset ahead of its echo, either sign; the scene's input starts 0.85 s plus the delay after the
      sweep, which leaves room for an offset of ±250 ms. Too late or with AEC off, the far voice plays at its default (as
      for a rig that adds nothing), and the report says so.
    - **Measured after the take** in the same chain recording, each far-only turn: the echo against its reference within
      40 ms of where the calibration put it (AEC3's lag Z) plus the lead W gives the delay Y. The report: "echo delay:
      asked X ms, measured Y ms after the reference's tap (turns, range); AEC3 sees it Z ms after the reference it's given
      (the lead W ms)", and the calibration line. Warnings: Z under 2 ms or negative (the echo before its reference can't
      be cancelled), Y over 2 ms off the asked, or moving over 2 ms between turns.
    - Offline: on fake chain recordings the calibration finds offsets of both signs to 0.0001 ms, through a jump of the
      reference beside the sweep; the measured delay lands within 0.03 samples of the truth at 0, 25, 40 and 300 ms, through
      a jump mid-take. The echo regions are where the echo is in the mic (the far voice's activity in the scene's time),
      so they don't move with the delay.
  - **A rig slip (2026-11-02, `events-echo-2` in the dev VM's `Downloads\nf-quality`):** a 60 s echo take cancelled
    well, then fell to 2-4 dB a far-only turn. The engine held still (its echo path 624 samples, the lead 17.78 ms, no
    jumps or skips); the far voice in cable B's loopback came 70 ms later from about 24 s on, against `far.wav` as played,
    while the mic (cable A) held to the sample: the test's far-end playback slipped, with nothing in the loopback's
    positions or stamps to show it. Real speakers can't do that (the echo and its reference are the same playback). The
    report said 40 ms because the measured delay is searched within 40 ms of the calibration's. To do: widen that search,
    and track the far and mic content against what was played (as `Downloads\nf-scratch\which_slipped.py` did) so a take
    that slipped is flagged as the rig's, from the second it happened. Likely what the unexplained 14 dB take of
    2026-11-01 (#10) was.
- **Phase 1:** live capture that saves the raw mic take and loopback next to the processed output.
  - **Its small first part, the chain recorder (built 2026-11-01, for #10):** `Engine::record_chain` records what the chain
    got and gave live, block by block, from the run's first block when armed before the start (engine.md, "The chain
    recorder"). `quality --save` writes it beside the scene's WAVs: `chain_mic.wav` and `chain_ref.wav` (the mic and the
    reference as the chain got them, after the timeline and the dither), `chain_out.wav`, `chain_blocks.csv` (a row a
    block) and `chain_config.json`; `process --warm-up` with that config reruns them into `chain_out.wav` exactly, and
    other settings can process the same blocks. It isn't the raw device take: the mic and the loopback as delivered,
    before the engine's FIFOs, with their stamps, are still to record.
- **Phase 2:** AECMOS with talk-type segmentation, from far-end and near-end activity.
- **Later:** the Lab page in the GUI (#15), with the blind-rating player.

## Output
- A scorecard per model: latency, CPU p50/p99, dropouts, metric means with paired 95% CIs, and the
  blind-test result.
- JSON/CSV, comparable across models, settings and machines.
