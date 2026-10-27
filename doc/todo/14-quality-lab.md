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
- **Phase 1:** live capture that saves the raw mic take and loopback next to the processed output.
- **Phase 2:** AECMOS with talk-type segmentation, from far-end and near-end activity.
- **Later:** the Lab page in the GUI (#15), with the blind-rating player.

## Output
- A scorecard per model: latency, CPU p50/p99, dropouts, metric means with paired 95% CIs, and the
  blind-test result.
- JSON/CSV, comparable across models, settings and machines.
