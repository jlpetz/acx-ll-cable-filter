# 7. Retraining, weights licensing and patents

Status: fact-checked 2026-10-26. This is a factual summary, not legal advice.

## The owner's question
"Some training data is restricted" (FastEnhancer 48k) and "unresolved" (DeepFilterNet3_ll): does
that mean we can't re-implement these in optimised Rust?

**No. Re-implementation is unblocked for every candidate.**
- The code licences allow porting: MIT for FastEnhancer, UL-UNAS and GTCRN; MIT or Apache-2.0 for
  DeepFilterNet.
- Training-data terms don't stop anyone writing the architecture.
- What's restricted or unclear is only shipping a specific published weights file. Retraining the
  same architecture on clean data gets around it.

## Per model
| Model | Re-implement | Ship the published weights | Notes |
|---|---|---|---|
| RNNoise / `nnnoiseless` | yes (BSD-3) | yes (BSD-3) | The embedded 2017 weights were trained partly on NTT's commercial telephonometry database; they're still BSD-3 from the copyright holder. The "public data only" claim belongs to RNNoise v0.2 models, which nnnoiseless can't load |
| GTCRN (dropped by the owner 2026-10-26) | yes; China-only patent (below) | yes: the GTCRN author confirmed in gtcrn #104 (2026-10-09) that MIT covers the ONNX weights, including commercial redistribution | strongest grant of any candidate; MIT has no patent grant, and the patents belong to NJU/Horizon, not the individual |
| UL-UNAS | yes; China-only patent probable | probably: MIT, weights committed in the repo, same owner as GTCRN but no explicit statement | DiDiSpeech's terms unverified (portal 404) |
| DeepFilterNet3 (standard) | yes | plausible: the paper says the weights were "published under an open source license"; Intel republished DFN2/DFN3 as MIT; the author's MIT/Apache crate embeds them | #697/#700/#709 unanswered; maintainer silent since 2024 |
| DeepFilterNet3_ll | yes | weaker than DFN3: committed 2023-05-23, after that paper sentence (2023-05-14); not in Intel's copy | not DFN3 with zero lookahead: a bigger model (emb/df hidden 512, 3 DF layers vs 256/256/2; ~39 MB vs ~8.6 MB) |
| FastEnhancer 48k | yes; no patents found (search incomplete) | **not without the author's grant** | no weights licence; 93% of the speech is AI-Hub (export and overseas use need a separate agreement; NIA credit required on derived works); 69% of the noise is non-commercial (WHAM!, MSRBench CC BY-NC; TUT forbids commercially distributing "results") |
| FastEnhancer 16k | yes | cleaner data (DNS3 + VCTK), still no explicit weights licence | |

## Provenance caveats that apply widely
- **YouTube-derived noise:** AudioSet's CC BY 4.0 covers Google's CSV files and features, not the
  YouTube audio, and VoxCeleb2 copyright stays with the video owners. That taints every DNS-trained
  model (DFN3, UL-UNAS, GTCRN, FastEnhancer-16k DNS).
- **Non-commercial is not an absolute bar for a free, ad-free app:** CC NC allows non-commercial
  sharing. But weights trained on NC data can't be relabelled MIT for downstream forks.
- **ShareAlike sources** (OpenSLR Google corpora, BibleTTS, DEMAND, Spheres; PTDB is ODbL) suggest
  releasing retrained weights as CC BY-SA 4.0, not MIT. Xiph ships BSD weights trained on BY-SA
  OpenSLR data, though. Hence the separate weights-licence field in the spec (#3).

## Patents
- **CN118230747A** (Nanjing University + Horizon; inventors include Rong, Lu, Zhu, Hu; priority
  2024-02-22; pending; China-only family) describes GTCRN. The GTCRN owner's 2024 comment confirms
  a related patent application exists.
- **CN119673188A** (granted 2026-04-28, China only) matches AdaptCRN, not UL-UNAS.
- **CN119673187** probably covers UL-UNAS (its page was unreachable).
- **Unverified by us:** the patent databases were unreachable during the verification pass.
- **FastEnhancer and DeepFilterNet:** no patents found, but the search was rate-limited and
  incomplete. Deep filtering itself comes from Fraunhofer (Mack & Habets 2020) and is unchecked, so
  "zero patent exposure" can't be claimed for any model.
- **Action:** list the known patents in the third-party notices (#18).

## Retraining route
- **FastEnhancer at 48 kHz is the easiest:**
  - full training code and 48 kHz configs (t/b/s/m/l) with on-the-fly mixing;
  - 500 epochs of 16,384 two-second clips; 1 GPU for T/B/S;
  - wall-clock time is in the TensorBoard logs inside the `ckpt-v1.0.0-48khz` zips.
  - `hop_size` is a free config value: `hop 480` with `n_fft 960` gives 20 ms algorithmic latency
    and matches WASAPI's 10 ms period exactly. The author says a hop change needs retraining, and
    that a smaller hop did slightly better.
  - Keep the dynamic low-pass augmentation (cutoffs ~3.8-22 kHz), so band-limited inputs still
    work.
- **Clean-licence 48 kHz data:**
  - Speech: DNS5 fullband clean (~760 h read, multilingual), VCTK 0.92 (CC BY 4.0), HiFi-TTS (CC BY
    4.0).
  - Noise: Freesound CC0, FSD50K CC0/CC-BY clips, DEMAND (BY-SA), simulated wind; RIRs SLR26/28.
  - DNS noise was upsampled from AudioSet and has little real energy above ~20 kHz (FastEnhancer
    kept 3.2 h of it).
  - Avoid WHAM!, TUT, MSRBench, EARS, DAPS and the NC tracks in FMA.
  - Replace the babble that WHAM!/DEMAND supplied with synthetic babble mixed from clean-licence
    speech.
- **Quality risk:** independent retrains of both FastEnhancer (FastEnhancer issue #9, at 48 kHz) and DFN (DeepFilterNet issue #700)
  under-performed the published weights. Budget for iteration, and validate against the published
  hop-512 weights on VCTK-DEMAND 48 kHz before switching.
- **DFN3-sized with zero lookahead** would be a cheaper low-latency model than _ll, but has no
  published weights. It needs a retrain with `conv_lookahead = df_lookahead = 0` at DFN3's
  dimensions.
- **Asymmetric windows** (8 ms latency nearly free in published results) are a later research
  project. Check muNet's patent applications.

## Shipping order (proposal)
1. RNNoise (v1).
2. UL-UNAS 16 kHz weights (GTCRN dropped by the owner).
3. Standard DFN3 under the Apache/MIT reading, crediting Schröter et al.
4. FastEnhancer 48k: only after the author grants a weights licence (asking waits until we get
   there, owner 2026-10-26), or as our own retrain.
