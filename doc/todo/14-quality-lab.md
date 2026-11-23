# 14. Quality analyser: the Lab mode (MOS)

Status: designed 2026-10-26 (the owner wants it in the app, not as a companion). Metric licences
and inputs were fact-checked the same day against the Microsoft DNS-, SIG- and AEC-Challenge repos
and the metric papers. Phase 0's batch, `noise-filter lab`, built 2026-11-02 (uncommitted when written), with DNSMOS P.835
and SIGMOS as packs and the licences rechecked at their sources that day ("The batch" under Phases).

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
- **Rechecked 2026-11-02 at the sources** (the GitHub API, the repos' own files):
  - DNS-Challenge at `591184a` (its newest commit, 2024-03-29): the README's Legal Notices license "the Microsoft
    documentation and other content in this repository under the Creative Commons Attribution 4.0" (`LICENSE`) and "any
    code" under MIT (`LICENSE-CODE`); nothing says which the ONNX files are, so the pack fetches and keeps both notices.
    `DNSMOS/DNSMOS/sig_bak_ovr.onnx` is 1,157,965 B, as the catalogue had it; the reference is `DNSMOS/dnsmos_local.py`.
  - SIG-Challenge at `bf45251` (2024-06-09): `LICENSE` is MIT (GitHub reads it so). The model is in Git LFS
    (`ICASSP2024/sigmos/model-sigmos_1697718653_41d092e8-epo-200.onnx`, oid `f939dcc1...`, 26,264,942 B); `sigmos.py`
    still calls V1 "an alpha version", and the Transparency FAQ adds no accuracy figure.
  - Neither repo publishes reference outputs for its metric: the tests pin our runners' own scores instead.

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
    - **Built 2026-11-02** (`echo_delay::track_rig`; app.md's quality row): the measured delay is searched within 250 ms
      (`MAX_RIG_OFFSET_MS`), so a slip shows at its size. Both devices' content is tracked against what each played: a
      second of the probe's track (the scene's input) and of the far end's (the far voice from where it started), every half
      second, found in the chain's mic and reference within 250 ms of the last window's place, or failing that within 2 s
      (`RIG_REACQUIRE_MS`: a bigger slip; a correlation of 0.5 or more; windows where the device played nothing skipped),
      and the place turned into the device's own position (the mic stream's index plus its unfilled skips; the reference
      timeline's index at the block's rate), so the engine's discards, skips and read-position jumps don't count. Each side
      runs on one clock, so its offset holds to the sample while the rig holds; a move of 1 ms or more that the next window
      confirms is a slip (a window off both neighbours is an outlier). The report: a `rig` line (each side's windows and
      spread, each slip), a warning naming the side, the size and when (between the start of the last window at the old
      offset and the end of the first at the new, in that side's own content's time: the probe's input for the mic, the far
      voice for the reference, which the far end plays from `far_start`), and the far-only turns from the one that holds it
      on left out of the echo figure (listed apart: "from N s ... left out") and of the delay's median (their delays listed
      apart), and double talk from then on left out too (AEC3 had a misaligned reference there).
    - **Fixed after review (2026-11-02):** a slip past 250 ms used to be missed with no sign (every window after it failed
      the search and was skipped, so the side "held"): now the 2 s look finds it at its size, and a side whose windows stop
      being found even there, two or more to the end of the take, has lost its track (`Lost`: the `rig` line and the warning
      say from when, and the turns from its last window found on are set apart as for a slip). The reference side's times
      were the probe's, off by the far voice's advance (7 ms on `events-echo-2`, 120 on `d25-b-1`; up to 250): now the far
      voice's own. A step in a side's last window, with no window after it, used to count as a slip: now it's named as
      unconfirmed and sets nothing apart.
    - **Tested** on fake chain recordings: a 70 ms far-end slip at 20 s found on the reference's side, +70.0 ms, between
      windows 1.5 s apart that hold the far voice's 20.065 s, the mic held; its turns set apart, the ones after it measured
      70 ms short; an 11 ms mic slip found as the mic's; 300 ms slips (the reference either way, the mic later) found at their
      size; a 3 s slip as the reference's track lost from before 20 s; an engine jump of the reference (-144 samples, in its
      facts) no slip; an unconfirmed last step set nothing apart.
    - **On the take itself** (`events-echo-2`, rerun from its files by the ignored `a_saved_takes_rig` test): the reference
      slipped +70.3 ms between 23.5 and 27.5 s (the far voice is silent in the 24-27 s near-only turn), the mic held over 112
      windows to 0.0 ms, the reference's 84 windows held to 0.0 ms either side of the slip; the echo came 25.01 ms after its
      tap in the three far-only turns before it (25 asked) and -45.28 ms in the four after (27-54 s), so those are left out.
      Rerun after the review's fixes (the far voice's times, the 2 s look, lost tracks): the same, and the other 15
      calibrated takes (`events-echo-1`, `events-echo-shared-1`, `excl-echo-d25`, `d25-*`, the eight `sweep-b-*`) held on both
      sides, nothing lost or unconfirmed, their delays within 0.25 ms of the asked.
  - **The batch, `noise-filter lab` (built 2026-11-02; app.md's command table has its options).** Named for this file's Lab
    and its CLI sketch (`lab run playlist.toml ...`): phase 0 is that batch, and the later phases belong under the same name.
    - **Metric packs** (ours): `packs/dnsmos` (`sig_bak_ovr.onnx`, both licence files) and `packs/sigmos` (the model, by
      `media.githubusercontent.com`; the fetch tool learned `"lfs": true`, checking the LFS pointer's git blob, rebuilt from
      the download's SHA-256 and size, against the commit's tree: `508d54d...`). Role `metric` (spec amendment "Metric
      packs"): the noise stage refuses them and the metric runners refuse noise packs; the same file checks and external-data
      refusal. Fetched with `python -I tools/fetch_models.py dnsmos sigmos` (27.4 MB).
    - **The runners** (`nf_ort::metric`, offline, allocating freely; one ONNX Runtime thread a session, a session a worker):
      DNSMOS P.835 as `dnsmos_local.py` runs it (the clip repeated to 9.01 s, 9.01 s windows a second apart, Python's float
      arithmetic for their bounds, the non-personalised polynomials, the mean). That arithmetic has a quirk we keep: windows
      7-23 (and some from 119) come out a sample short, and the script skips them, so a 45 s clip scores 19 of its 36
      windows; under 17 s none is lost, so with DNSMOS the Lab refuses `--clip-seconds` over 16, and `clips.csv` gives each
      clip's windows (`dnsmos_windows`). SIGMOS as `sigmos.py` frames it (960-point STFT, hop 480, the square
      root of a periodic Hann, 480 zeros in front and up to 960 behind, |X|^0.3 with the phase kept, three planes), one run a
      clip. The P.808 model (`model_v8.onnx`, a librosa mel spectrogram) isn't ported yet.
    - **The resampler for DNSMOS** (`nf_ort::metric::resample`): the script's own, reproduced. `dnsmos_local.py` calls
      `librosa.resample` with the rates positionally (before librosa 0.10), and DNS-Challenge 591184a's `requirements.txt`
      pins librosa 0.8.1 (with numpy 1.22.4 and onnxruntime 1.13.1: late 2022), whose `resample` runs resampy's
      `kaiser_best` and pads to `ceil(n / 3)`. librosa 0.8.1 takes `resampy >= 0.2.2`, and resampy 0.3.0 (2022-06-29)
      regenerated `kaiser_best` (50 crossings, 8192 points each, roll-off 0.917, from 64, 512 and 0.948), so the pinned set
      installs the new one: ours is resampy 0.3-0.4.3's table (its β, which resampy doesn't keep, solved from the table's
      last entry: 12.984585247040012, rebuilding all 409 601 entries to 7e-16) read as `resample_f` reads it (index step
      `int(8192 / 3)` = 2730, the right wing interpolated at 2730.67 + 2730k: 150 taps behind, 149 ahead), then librosa's
      padding. The first version (an exact Kaiser sinc with 0.2.2's parameters, zero phase) was neither, and moved the
      synthetic test scenes' DNSMOS by up to 0.026; 0.2.2's resampy against 0.4's moves them by up to 0.03 (BAK), its output
      -52 dB off below 4 kHz, -46 dB to 7 kHz, as much as the signal in the transition band above.
    - **Checked against numpy** (`tools/check_metric_features.py`, run by hand on a dump from the ignored
      `dump_metric_features` test): SIGMOS's features are identical, value for value, to a numpy transcription of
      `sigmos.py`'s, and DNSMOS's 16 kHz input to numpy ports of librosa 0.8.1's `resample` and resampy 0.4.3's
      `resample`/`resample_f`, every float32 sample including librosa's padding (with resampy's own `kaiser_best.npz` tables,
      fetched to a scratch folder and read with `allow_pickle=False`, or the rebuilt ones). The upstream scripts themselves are
      never run: untrusted data. The metric tests pin the runners' scores
      on fixed synthetic scenes (to 2e-3) and check their order: clean over 20 dB over 5 dB (DNSMOS BAK, SIGMOS NOISE), and on
      UL-UNAS's real conformance clip 0174, enhanced and clean over noisy.
    - **The playlist** (`lab::playlist`): the 10 speech takes cut into 15 s segments (a last part of 10 s or more kept: 17
      segments, TSP 6 and VCTK 11) x the 7 noises (`keyboard`, `fan`, `aircon`, `dogs`, `cafe`, `cafe-demand`, `music`; a
      long noise read from an offset that moves with the clip) x 0, 10 and 20 dB: 357 clips, each built by `quality`'s
      noise-scene builder (-20 dBFS active speech, P.56-style activity), the same every run. Each clip through a fresh chain
      per model, the model inline, its latency dropped; the clean speech (once a segment), the noisy input and each output
      scored by DNSMOS, SIGMOS and `quality`'s level measures (`analysis::noise_metrics`, factored out of `analyse`).
    - **Statistics** (`lab::stats`, ours): means over the clips; each signal's paired difference from the baseline with a
      95% CI by Student's t, cluster-robust (CR1) by speaker (a speech take: a VCTK take's segments share a speaker and a
      room) and noise file: two-way where a table pools noises (Cameron, Gelbach and Miller 2011: the two one-way variances
      added, their cells' taken off, never under either one-way), by speaker within a noise; df the fewer clusters less 1
      (6 overall, 9 per noise). With equal clusters it's exactly the t over cluster means; the point estimate stays the clips'
      mean. "No difference" where it holds 0. The first version took t over the 357 clips as if independent, which a review
      found too narrow by half or more (noise in pauses [-10.4, -7.5] dB over clips, [-17.1, -6.6] over speakers; speech kept
      and DNSMOS SIG crossed 0 over the 7 noise files). In a simulation of the Lab's layout (speaker, noise and segment
      effects as big as the clips' own scatter) the two-way interval held the true mean 94.7% of the time, the one over
      clips 27%. Why t and not a bootstrap: the percentile bootstrap under-covers at the small end (18% too narrow at 10, and
      7 to 10 clusters is where the Lab is) and adds a seed. The t quantile is the CDF's inverse by bisection (the incomplete
      beta's continued fraction), to the tables' 9 digits.
    - **The speech's own sound in the pauses** (quality's check, `speech_left_in_pauses_db`): VCTK's recordings carry their
      room's noise and reverberation, so noise in pauses there partly measures the recording, which a model may keep as
      speech, and every figure is pulled toward 0 dB (a review found it: the first run didn't check). On the fetched set,
      against the added noise in the pauses: VCTK -35.8 to -21.8 dB at 0 dB SNR, -22.3 to -7.6 at 10, -11.2 to +12.2 at 20;
      TSP -59.5 to -36.7, -51.2 to -14.1, -39.9 to +4.7 (the intermittent noises, dogs and music, quiet in some pauses, give
      the high ends). Where it's within 25 dB (quality's warning), the clip's pause figures stay in `clips.csv` (with that
      level) but out of the summaries, and the scorecard counts them: 179 of the 357 (every VCTK clip at 10 and 20 dB, 12 of
      its 77 at 0 dB, 13 of TSP's 126), so the pause figures speak for TSP and for VCTK at 0 dB.
    - **Outputs:** the scorecard (overall, then the headline metrics per noise type), `clips.csv` (a row a clip and signal),
      `lab.json` (what ran, the takes' licences, every clip, the summaries overall, per noise and per SNR). A build without
      `onnx` runs RNNoise and the level measures and says so. `noise-filter.exe` isn't run here: the run below is the ignored
      `the_fetched_set_s_scorecard` test, the same function on the same set.
    - **The run** (2026-11-02, after the review's fixes: the CIs clustered, the pause figures checked, the resampler
      resampy's; the dev VM, release, onnx, 8 threads, 490 s; files in `Downloads\nf-lab\full-2026-11-02-b`): means over 357
      clips, clean / noisy / RNNoise / UL-UNAS, and UL-UNAS less RNNoise with its 95% CI (two-way by 10 speakers and 7
      noises, df 6):

      | Metric | clean | noisy | RNNoise | UL-UNAS | UL-UNAS - RNNoise |
      |---|---|---|---|---|---|
      | DNSMOS SIG | 3.54 | 3.02 | 3.13 | 3.19 | no difference [-0.01, +0.13] |
      | DNSMOS BAK | 4.04 | 2.40 | 3.45 | 4.00 | +0.55 [+0.28, +0.82] |
      | DNSMOS OVRL | 3.26 | 2.26 | 2.67 | 2.93 | +0.26 [+0.12, +0.41] |
      | SIGMOS COL | 4.20 | 3.36 | 3.17 | 3.70 | +0.53 [+0.40, +0.67] |
      | SIGMOS DISC | 4.33 | 3.95 | 3.35 | 3.97 | +0.62 [+0.50, +0.74] |
      | SIGMOS LOUD | 3.92 | 3.14 | 3.49 | 4.08 | +0.60 [+0.46, +0.73] |
      | SIGMOS NOISE | 4.42 | 1.91 | 3.39 | 4.25 | +0.86 [+0.59, +1.13] |
      | SIGMOS REVERB | 4.73 | 4.12 | 4.19 | 4.40 | +0.20 [+0.09, +0.32] |
      | SIGMOS SIG | 4.03 | 3.04 | 2.87 | 3.56 | +0.68 [+0.57, +0.80] |
      | SIGMOS OVRL | 3.64 | 2.19 | 2.44 | 3.12 | +0.68 [+0.54, +0.82] |
      | noise in pauses, dB (178 clips) | | 0.00 | -19.71 | -35.60 | -15.89 [-26.28, -5.49] |
      | 200 ms clear, dB (136 clips) | | 0.00 | -22.61 | -36.91 | -14.30 [-25.84, -2.75] |
      | speech kept, dB | | 0.04 | -0.69 | -0.59 | no difference [-0.06, +0.26] |

      - UL-UNAS over RNNoise on both OVRLs for every noise type (DNSMOS +0.15 cafe-demand to +0.60 music; SIGMOS +0.53 cafe
        to +0.92 music; by speaker, df 9), and on every metric overall but DNSMOS SIG and the speech kept, which the
        clustered CIs no longer separate. Noise in pauses: more cut on every noise but cafe-demand (no difference, [-9.4,
        +0.0]), about 30 dB more on music and dogs, where RNNoise cuts only 7.4 and 6.4 dB. Speech kept: UL-UNAS keeps more on
        music, aircon, fan and keyboard (+0.06 to +0.38 dB), no difference on the cafes and dogs.
      - RNNoise against the noisy input: DNSMOS BAK and OVRL up, SIG no difference; SIGMOS NOISE, OVRL and LOUD up, but DISC
        down 0.61 [0.23, 0.98], COL down 0.19 and SIG down 0.17: SIGMOS hears RNNoise's artefacts (discontinuity most).
        Worth a listen before trusting it (uncalibrated).
      - Against the first run (earlier that day: the CIs over clips, every clip's pause figures, the exact-sinc resampler):
        the means are the same but for UL-UNAS's DNSMOS SIG and OVRL, 0.01 lower now (3.20 and 2.94 then: the resampler);
        the intervals are about 3 to 7 times wider; DNSMOS SIG (+0.06 [+0.04, +0.08]) and the speech kept (+0.10 [+0.07, +0.14]) lose their verdicts; noise in
        pauses, -8.97 [-10.41, -7.54] dB over every clip then, is -15.89 over those it can be read on, and the cafe
        difference ("no difference" then) shows.
    - **To do:** the P.808 runner (librosa's mel, its padding mode pinned by version); a `--save` of the clips' WAVs for
      listening; model variants in one run (`ulunas:zero` for the high-band A/B); the live take's half (phase 1); more
      speakers (and then perhaps a two-way cluster bootstrap); speech takes with a quieter floor, for the pause figures at 10
      and 20 dB.
- **Phase 1:** live capture that saves the raw mic take and loopback next to the processed output.
  - **Its small first part, the chain recorder (built 2026-11-01, for #10):** `Engine::record_chain` records what the chain
    got and gave live, block by block, from the run's first block when armed before the start (engine.md, "The chain
    recorder"). `quality --save` writes it beside the scene's WAVs: `chain_mic.wav` and `chain_ref.wav` (the mic and the
    reference as the chain got them: the mic dithered on the blocks a stage was to run on, as captured in bypass and in
    measurement passthrough; the reference after the timeline and the dither), `chain_out.wav`, `chain_blocks.csv` (a row a
    block) and `chain_config.json`; `process --warm-up` with that config reruns them into `chain_out.wav` exactly, and
    other settings can process the same blocks. It isn't the raw device take: the mic and the loopback as delivered,
    before the engine's FIFOs, with their stamps, are still to record.
- **Phase 2:** AECMOS with talk-type segmentation, from far-end and near-end activity.
- **Later:** the Lab page in the GUI (#15), with the blind-rating player.

## Output
- A scorecard per model: latency, CPU p50/p99, dropouts, metric means with paired 95% CIs, and the
  blind-test result.
- JSON/CSV, comparable across models, settings and machines.
