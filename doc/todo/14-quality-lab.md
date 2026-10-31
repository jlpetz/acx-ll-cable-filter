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
      now plays the far voice 100 ms ahead of its echo, as a speaker's output latency would, and saves `far.wav` as played.
      Then: echo down 14.4 dB (12-18 dB a far-only turn) against 46-57 dB offline on the same scene (#10's detail file).
- **Phase 1:** live capture that saves the raw mic take and loopback next to the processed output.
- **Phase 2:** AECMOS with talk-type segmentation, from far-end and near-end activity.
- **Later:** the Lab page in the GUI (#15), with the blind-rating player.

## Output
- A scorecard per model: latency, CPU p50/p99, dropouts, metric means with paired 95% CIs, and the
  blind-test result.
- JSON/CSV, comparable across models, settings and machines.
