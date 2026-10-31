<!-- TODO 14 detail: the test-audio catalogue of 2026-11-01 (a research workflow: four searches, each checked by an adversarial
licence verifier, then this synthesis; no claim was refuted). Section 5 fetches the starter set. -->

# noise-filter test-audio catalogue (checked 2026-11-01)

**The owner's decisions (2026-11-01):**
- **No audio in the repo, ever:** it's bloat. The repo keeps this catalogue (links) and a fetch script; the audio lives in a local
  cache only. Read "commit" in the starter set below as "fetch to the local cache".
- **A small local starter set** to get going; the rest of the catalogue stays as links until a test needs it.
- **Whatever the licences allow, used locally:** we never re-host or re-share. The project is free and non-commercial, so
  non-commercial (NC) sets are fine for local testing too. Open question for later (TODO #7): whether NC terms follow model
  weights trained on NC data, if we ever publish such weights.
- **Freesound:** the owner downloads the five CC0 originals (448209, 242042, 273620, 337101, 370973) by hand into their
  Downloads folder; the fetch script picks them up from there by their `<id>__` file names.

**The starter set is fetched into a local cache by `tools/fetch_test_audio.py`: see section 5.** "(inference)" marks my own reasoning, not something a source says. **NC** = non-commercial, **SA** = share-alike, **ND** = no derivatives. "Repo" = may we commit a small subset to our MIT OR Apache-2.0 repo.

Licence texts: [CC0](https://creativecommons.org/publicdomain/zero/1.0/legalcode), [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/legalcode), [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/legalcode), [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/), [CC BY-SA 3.0 AU](https://creativecommons.org/licenses/by-sa/3.0/au/), [CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/legalcode).

## 1. Catalogue

### Clean speech
| Name | Licence | Repo | Comm. | Rate / format | Size | For |
|---|---|---|---|---|---|---|
| [TSP](https://www.mmsp.ece.mcgill.ca/Documents/Data/TSP-Speech-Database/Licence.html) | Simplified BSD, (c) 2018 Peter Kabal | yes, keep the notice | yes | 48 kHz 16-bit mono WAV, anechoic; the docs say "very little energy above 7 kHz" | 48k.zip 253,816,919 B; 1,444 files of 1.3-4.8 s; 23 adults + 2 children | bundled takes |
| [VCTK 0.92](https://datashare.ed.ac.uk/handle/10283/3443) | CC BY 4.0 | yes, credit and list changes | yes | 48 kHz 16-bit FLAC, 2 mics, hemi-anechoic | 11.75 GB zip; single FLACs by HTTP Range | full-band takes, far-end voice |
| [OpenSLR 83](https://www.openslr.org/83/) | **CC BY-SA 4.0** | yes, edits stay **SA** | yes | 48 kHz 16-bit mono WAV | 11 zips of 103 MB-1.7 GB; 120 speakers | accents |
| [PTDB-TUG](https://www2.spsc.tugraz.at/databases/PTDB-TUG/) | [ODbL 1.0](http://opendatacommons.org/licenses/odbl/1.0/) + [DbCL 1.0](https://opendatacommons.org/licenses/dbcl/1-0/) | a small extract, probably (inference) | yes | 48 kHz 16-bit mono, ~8 s | 3.9 GB, or file by file | longer sentences |
| [AISHELL-3](https://www.openslr.org/93/) | Apache-2.0 (stated on the OpenSLR page only) | yes | yes | 44.1 kHz 16-bit mono | 19 GB tgz | Mandarin |
| [LibriVox](https://librivox.org/pages/public-domain/) | public domain in the US; archive.org PDM 1.0 | yes, if the text is PD worldwide (inference) | yes | 44.1 kHz 128 kbps MP3, ~16.5 kHz bandwidth (measured on sample chapters) | ~16 MB per chapter | takes of 30 s+ |
| [EARS](https://github.com/facebookresearch/ears_dataset), [DAPS](https://zenodo.org/records/4660670), [Expresso](https://speechbot.github.io/expresso/) | **CC BY-NC 4.0** | no | **no** | 48 kHz float / 44.1 kHz / 48 kHz 24-bit | 69.17 / 16.06 / 38.4 GB | owner's private tests only |
| [Common Voice](https://mozilladatacollective.com/datasets/cmu5jplf300nwmh07iqvk9leo) | CC0-1.0, plus download terms: "forbidden to re-host or re-share" | **no** | yes | MP3, recorded in browsers | 88.43 GB (English) | poor-mic input only |

Below 44.1 kHz: [LibriSpeech](https://www.openslr.org/12/) (CC BY 4.0, 16 kHz) and [LibriTTS](https://www.openslr.org/60/) (CC BY 4.0, 24 kHz). [LibriTTS-R](https://www.openslr.org/141/) was restored by a model (Miipher), so it isn't a clean reference. [RAVDESS](https://zenodo.org/records/1188976) is **CC BY-NC-SA 4.0**.

### Noise
| Covers | Name | Licence | Repo | Comm. | Rate / format | Size |
|---|---|---|---|---|---|---|
| all five | [Freesound](https://freesound.org/help/faq/) | per clip: CC0, CC BY 4.0 or **BY-NC 4.0**; older clips use the 3.0 versions | CC0 yes; BY with credit | CC0/BY | whatever was uploaded; originals need a login or OAuth2 ([API](https://freesound.org/docs/api/resources_apiv2.html)) | per clip |
| all five | [FSD50K](https://zenodo.org/records/4060432) | compilation CC BY 4.0; clips: 19,873 CC0, 23,506 BY 3.0, **6,041 BY-NC 3.0**, 1,777 Sampling+ | CC0/BY clips | the record asks commercial users to contact the authors | 44.1 kHz 16-bit mono, 0.3-30 s | 24.7 GB (eval 6.26 GB) |
| typing, fan | [DNS5 noise](https://github.com/microsoft/DNS-Challenge), Freesound part | "original terms"; the README says CC0 only | only after checking each clip | probably (inference) | 48 kHz 16-bit mono, 10 s; Freesound ID in each file name | 4.46 GB |
| none | DNS5 noise, AudioSet part | YouTube audio; [AudioSet](https://research.google.com/audioset/download.html) licenses only labels and features | no | unclear | upsampled to 48 kHz | 35.76 GB |
| cafe, office | [DEMAND](https://zenodo.org/records/1227121) | **CC BY-SA 3.0** according to its own PDF (the Zenodo metadata says CC BY 4.0) | yes, **SA** | yes | 48 kHz 16-bit, 16 ch, 300 s | 7.4 GB; ch01 of all 18 scenes is ~350 MB by Range |
| cafe, home | QUT-NOISE ([data.researchdatafinder.qut.edu.au](https://data.researchdatafinder.qut.edu.au); I don't have the exact page URL) | **CC BY-SA 3.0 AU** (LICENSE.txt in the zip) | yes, **SA** | yes | 48 kHz 16-bit stereo, ~42 min | CAFE zip 1.71 GB |
| music | [Open Goldberg Variations](https://archive.org/details/OpenGoldbergVariations) | CC0 1.0 | yes | yes | 96 kHz 24-bit stereo FLAC, solo piano | 1.39 GB, 31 tracks |
| dog, music | [SONYC-UST v2.3](https://zenodo.org/records/3966543) | CC BY 4.0 | yes | yes | 48 kHz 16-bit mono, 10 s, street sensors | 13.3 GB |
| music, babble | [MUSAN](https://www.openslr.org/17/) | CC BY 4.0, plus a LICENSE file per folder | file by file | yes | **16 kHz** | 11 GB |
| dog, typing | [ESC-50](https://github.com/karolpiczak/ESC-50) | **CC BY-NC 3.0**; the ESC-10 subset is CC BY 3.0, but 2 of its Rooster clips are Sampling+ | ESC-10, clip by clip | ESC-10 | 44.1 kHz, 5 s | ~600 MB |
| cafe, bar | [WHAM!](http://wham.whisper.ai/) | **CC BY-NC 4.0** | no | **no** | 16 or 48 kHz, binaural | 17 / 68.1 GB |
| street | [TAU 2019](https://zenodo.org/records/2589280) | its own **non-commercial** licence, which also bans "distributing the results" | no | **no** | 48 kHz 24-bit binaural | 35.6 GB |

Excluded:
- [BBC RemArc](https://sound-effects.bbcrewind.co.uk/licensing): no sharing, no business use.
- [Sonniss GDC](https://sonniss.com/gdc-bundle-license/): no AI use; media production only.
- [Pixabay](https://pixabay.com/service/license-summary/): no standalone redistribution.
- [MTG-Jamendo](https://github.com/MTG/mtg-jamendo-dataset): non-commercial research only.
- [CHiME-3/4](https://catalog.ldc.upenn.edu/LDC2017S24): LDC licence.
- [MS-SNSD](https://github.com/microsoft/MS-SNSD): 16 kHz, and its clips can't be traced to their sources.

[FMA](https://github.com/mdeff/fma) is under each artist's own CC licence, so it can only be used track by track. **Gap:** I have no verified permissive music with vocals yet.

### Echo / AEC
All of these come from [microsoft/AEC-Challenge](https://github.com/microsoft/AEC-Challenge). The repo is MIT, but **no licence covers Microsoft's crowdsourced recordings**. We can use them in tests but shouldn't commit them; commercial use is unclear.

| Set | Format | Size | For |
|---|---|---|---|
| ICASSP 2022 test set (in the repo, LFS) | 48 kHz 16-bit mono mic + loopback, 14.5-39.6 s | 4.22 GB; 800 clips: 300 double talk, 300 far end, 200 near end; echo-path movement in both double-talk and far-end clips | real double talk, echo-path change |
| ICASSP 2023 blind set | 48 kHz, mic + loopback + enrolment, 12-78 s | 4,644,587,804 B; 300 DT, 300 FE, 200 NE | AECMOS on real devices |
| fullband_synthetic.zip | 48 kHz 16-bit, 10 s; far end, echo, near end, mic and target files; speech from LibriVox, some noise from AudioSet | 28,360,469,586 B; 10,000 examples, 2,543 with no noise; members can be fetched by Range | ERLE and speech damage with ground truth |
| fullband / fullband_mobile | 48 kHz, real PCs / Android | 20.7 / 36.7 GB | bulk tests |
| `real` sweeps | **16 kHz** | ~3,055 sweep loopback/mic pairs | real device echo paths (deconvolve them) |

The downloads moved on 2026-10-05 to `crowdsourcedatapub.blob.core.windows.net/crowdsource-data/aec_challenge_data_downloads/`. Files stored in the repo can be fetched one at a time from `media.githubusercontent.com/media/microsoft/AEC-Challenge/main/datasets/...`. [MYRiAD](https://zenodo.org/records/7389996) and [trajectoRIR](https://zenodo.org/records/15564430) (moving mics, 48 kHz 24-bit) are **CC BY-NC-SA 4.0**.

### Room impulse responses
| Name | Licence | Repo | Comm. | Rate | Size | Notes |
|---|---|---|---|---|---|---|
| [pyroomacoustics](https://github.com/LCAV/pyroomacoustics), [RIR-Generator](https://github.com/ehabets/RIR-Generator) | MIT (these are tools; avoid gpuRIR, which is AGPL) | our own output, yes | yes | any | tiny | RIRs and echo paths we can bundle |
| [Motus](https://zenodo.org/records/4923187) | CC BY 4.0 | yes | yes | 48 kHz 24-bit, 32 ch | best_of.zip 461.5 MB | one room, 6 furniture layouts × 4 positions |
| [ACE](https://zenodo.org/records/6257551) | CC BY 4.0 on Zenodo; a secondary list says **BY-ND 4.0** | not until that is settled | yes | 48 kHz 16-bit, plus fan and babble noise | single-channel set 417 MB | device-mic RIRs |
| [dEchorate](https://zenodo.org/records/6576203) | CC BY 4.0 | yes | yes | 48 kHz, but measured with a 100 Hz-14 kHz sweep | RIRs 4.19 GB | controlled reverb |
| DNS5 `impulse_responses_000.tar.bz2` ([README](https://github.com/microsoft/DNS-Challenge)) | Apache 2.0, per the README | simulated part only | simulated part only | 48 kHz 16-bit, 1 s, low level | 264,965,552 B | the real part is RWCP, **research use only** ([SLR13](https://www.openslr.org/13/)) |
| [BUT ReverbDB](https://speech.fit.vut.cz/software/but-speech-fit-reverb-database) | CC BY 4.0 (web page) / Apache 2.0 (read_me) | yes | yes | **16 kHz** | 9.3 GB | real rooms |
| Aachen AIR, [MIT IR Survey](https://mcdermottlab.mit.edu/Reverb/IR_Survey.html), OpenAIR | no licence found | no | no | 48 kHz / 32 kHz / varies | IR Survey 11.66 MB | our own tests at most |

### Ready-made evaluation sets
| Name | Licence | Repo | Format | Size | Published RNNoise score |
|---|---|---|---|---|---|
| [VoiceBank-DEMAND test](https://datashare.ed.ac.uk/handle/10283/2791) | CC BY 4.0; the speech is from VCTK 0.80 (ODC-By 1.0); the noisy files may be **SA** because of DEMAND | clean: yes; noisy: treat as **SA** | 48 kHz 16-bit mono, 824 clips of 1.2-9.8 s | 154,332,064 + 170,578,854 B | PESQ 2.33, STOI 0.922; noisy input 1.97 / 0.921 ([DFN2](https://arxiv.org/abs/2205.05474)) |
| [DNS4 blind](https://dnschallengepublic.blob.core.windows.net/dns4archive/blind_testset_bothtracks.zip) | none stated; file names contain MTurk worker IDs | no | 48 kHz 16-bit mono, 10 s; 859 real clips (typing, fan, dog, door and others) | 1.66 GB; the clips alone 824.7 MB | DNSMOS SIG 3.88, BAK 3.69, OVRL 3.38; noisy input 4.14 / 2.94 / 3.29 (DFN2) |
| DNS5 blind | none stated | no | 48 kHz, 16- or 24-bit, 10-365 s | 3.89 GB | none |
| EARS-WHAM, [URGENT 2024](https://huggingface.co/datasets/urgent-challenge/urgent2024_official) | **CC BY-NC 4.0** / **CC BY-NC-SA 4.0** | no | 48 kHz / mixed rates | not checked | none |
| [NOIZEUS](https://ecs.utdallas.edu/loizou/speech/noizeus/) | none | no | 8 kHz, telephone-filtered | ~35 MB (inference) | skip |

The `weights.rnn` file in `nnnoiseless` 0.5.2 is byte-identical to the weights in [xiph/rnnoise](https://github.com/xiph/rnnoise) v0.1.1, the original 2017 model. So these published figures apply to the model we run.

### Quality-metric models
| Name | Licence | Input | Size |
|---|---|---|---|
| [DNSMOS](https://github.com/microsoft/DNS-Challenge/tree/master/DNSMOS) P.835 / P.808 | repo is MIT for code and CC BY 4.0 for content; which one covers the ONNX files is unclear | 16 kHz, 9.01 s windows | 1,157,965 / 224,860 B |
| [SIGMOS](https://github.com/microsoft/SIG-Challenge) | MIT; labelled "alpha" | 48 kHz | 26,264,942 B (LFS) |
| [AECMOS](https://github.com/microsoft/AEC-Challenge/tree/main/AECMOS/AECMOS_local) `Run_1668423760` | repo MIT | 48 kHz loopback + mic + enhanced output + talk type; the script only scores the first 20 s | 2,682,822 B |
| [pystoi](https://github.com/mpariente/pystoi) | MIT | not checked | not checked |
| [PESQ](https://github.com/ludlows/PESQ) (P.862 C code) | the IPR notice bans altering or translating it and commercial use; other uses need a licence from OPTICOM or Psytechnics, and benchmarking a filter is not on the permitted list | 16 kHz | never port or ship it |

## 2. Recommended starter set

Everything ends up as 48 kHz 16-bit mono.
- **Commit** means small and CC0, BSD or CC BY. It goes in the repo as FLAC, about 15 MB in total (estimate).
- **Fetch** means the script downloads it.
- Freesound originals need a logged-in account, so you would download those once and commit trimmed CC0 copies.
- The Freesound details below come from each clip's page, `https://freesound.org/s/<id>/`, read on 2026-11-01.

| Need | Pick | Source / size | Where | Attribution |
|---|---|---|---|---|
| Speech, 3 F + 3 M | TSP `48k/FA/FA01_01-06`, `FB/FB07_01-06`, `FC/FC13_01-06`, `MA/MA01_01-06`, `MB/MB07_01-06`, `MC/MC13_01-06`; each speaker's 6 sentences joined into one take of about 15 s | [48k.zip](https://www.mmsp.ece.mcgill.ca/Documents/Data/TSP-Speech-Database/48k.zip), one Range request per member, ~7 MB compressed | commit | "Speech from the TSP Speech Database, Copyright (c) 2018 Peter Kabal. Simplified BSD licence, see LICENSES/TSP.txt. Sentences joined and level-normalised." |
| Full-band speech | VCTK: about 6 utterances from each of 4 speakers, the same mic throughout; avoid p280 and p315, which had MKH 800 problems | [VCTK zip](https://datashare.ed.ac.uk/server/api/core/bitstreams/535f4286-e54c-4038-838c-a02285e32cb2/content) by Range, ~5-10 MB (inference) | commit | "Contains audio from the CSTR VCTK Corpus 0.92 (Yamagishi, Veaux, MacDonald; University of Edinburgh), https://doi.org/10.7488/ds/2645, CC BY 4.0. Changes: utterances joined, converted to 48 kHz mono, level-normalised." |
| Keyboard | 448209 "Keyboard Typing" by florianreichelt: CC0, 48 kHz 32-bit stereo WAV, 18.7 s, 6.9 MB | Freesound | commit | none required (CC0); give a courtesy credit in the manifest |
| Fan | 242042 "Computer Fan Loop" by Ezcah: CC0, 48 kHz 16-bit mono FLAC, 60.6 s, 3.1 MB. Also 273620 "Old air conditioning unit" by myrinvp: CC0, 48 kHz 32-bit stereo WAV, 16.2 s | Freesound | commit 20-30 s | same as keyboard |
| Dog | 337101 "Group_of_Dogs_Barking.WAV" by ivolipa: CC0, 48 kHz 24-bit stereo WAV, 21.8 s, 6.0 MB | Freesound | commit | same as keyboard |
| Cafe | 370973 "coffee shop ambience" by waweee: CC0, 48 kHz 24-bit mono WAV, 4:56, 40.7 MB | Freesound | commit a 30 s excerpt | same as keyboard |
| Cafe, standard reference | DEMAND `PCAFETER_48k.zip`, member `ch01.wav`, 300 s | [Zenodo](https://zenodo.org/records/1227121/files/PCAFETER_48k.zip): zip is 314,117,272 B, md5 b7b8b10f41b747cd0fd3849db13cebd2; the member is ~17-23 MB by Range | fetch | "DEMAND, J. Thiemann, N. Ito, E. Vincent, doi:10.5281/zenodo.1227121, CC BY-SA 3.0. Excerpt, converted." Anything mixed with it stays **SA**. |
| Music | Open Goldberg file "Kimiko Ishizaka - J.S. Bach- -Open- Goldberg Variations, BWV 988 (Piano) - 05 Variatio 4 a 1 Clav..flac": 68.57 s, 19,963,388 B, sha1 336565dfc01932980923ba966ca4b6093f8825f3 | [archive.org metadata](https://archive.org/metadata/OpenGoldbergVariations) | commit a 20 s excerpt | courtesy: "Kimiko Ishizaka, Open Goldberg Variations, CC0" |
| RIRs | 4 simulated at 48 kHz with pyroomacoustics: small office, living room, car, and a laptop speaker-to-mic path. Plus the real Motus `best_of.zip` | [Motus](https://zenodo.org/records/4923187), 461.5 MB | simulated: commit; Motus: fetch | "Motus, doi:10.5281/zenodo.4923187, CC BY 4.0; one capsule extracted and resampled" |
| AEC double talk | (a) Our own: the TSP FA take as near end and a VCTK male as far end, passed through a simulated echo path, which gives ground truth. (b) One double-talk mic + loopback pair from the ICASSP 2022 test set. (c) 2-3 noise-free `fullband_synthetic` examples picked from `meta.csv`, each 5 files of 960,044 B | (b) GitHub LFS media URL; (c) the Azure zip by Range | (a) commit; (b) and (c) fetch | (a) the TSP and VCTK lines; (b) and (c) cite the AEC Challenge, which grants no licence |
| Benchmarks | VoiceBank-DEMAND test set (325 MB) to check STOI against ~0.922 (PESQ only if you accept its notice); DNS4 blind `testclips/` (825 MB) for DNSMOS | links in section 1 | fetch | Valentini-Botinhao (2017), doi:10.7488/ds/2117, CC BY 4.0 |

## 3. Fetch plan: `tools/fetch_test_audio.py`
The plan as proposed. Section 5 describes what was built and where it differs.
- **Sources file** (`tools/test_audio_sources.json`, committed). For each item it holds:
  - the URL and the kind (`file`, `zip-member` or `tar-prefix`), plus the member path;
  - the expected size and SHA-256;
  - the licence name and URL, and the attribution text;
  - a recipe: channel, start, length, joins and gain;
  - a tier: `permissive`, `unlicensed` or `nc`.
- **Storage:** a gitignored cache, either `test-audio/` in `.gitignore` or `%LOCALAPPDATA%\noise-filter\test-audio`. Downloads go in `raw/` and converted files in `wav48/`.
- **Zip members by Range:**
  - Read the EOCD/ZIP64 record and the central directory, fetch only the member needed, inflate it, check its CRC-32, then its SHA-256.
  - Range works on DataShare, Zenodo (rate limit 133), Azure blobs, GitHub LFS media URLs, archive.org and the McGill server.
  - A tar.bz2 can only be read from the start, which is enough for the DNS simulated RIRs at the head of that archive.
- **Checksums:** check the publisher's hash where there is one (Zenodo md5, archive.org sha1, the MD5s in the AEC README), and pin our own SHA-256 in the sources file. The DNS SHA-1 index now returns 404.
- **Conversion:** to 48 kHz mono 16-bit PCM WAV with one fixed resampler. Record the channel choice, gain and trims, and keep the originals. Takes are rebuilt from the recipe, so the output is the same on every run.
- **Manifest:** `manifest.json` lists every output file with its source, licence, attribution, changes, hashes, rate, duration and tier. The script also generates `ATTRIBUTION.md` from it.
- **Tiers:**
  - By default the script fetches permissive sources only.
  - `--unlicensed` adds the AEC and DNS sets.
  - `--non-commercial` adds EARS, DAPS and WHAM! with a warning; their outputs never mix with the other tiers.
- **Safety:** treat downloads as untrusted data and never run upstream scripts (the DNS5 blind script runs `wget` despite its "DRY RUN" comment). Back off on HTTP 429.

## 4. Licence pitfalls
- **Licences are per clip.** The headline licences of Freesound, FSD50K, ESC-50 and the DNS noise don't hold for every clip. Of the 78 Freesound IDs in the DNS 2020 test set, freesound.org now shows 72 CC0, 3 CC BY, 1 BY-NC and 2 deleted. Record each clip's licence at fetch time.
- **NC never ships:** EARS, DAPS, Expresso, WHAM!, ESC-50, TAU, MYRiAD, trajectoRIR, URGENT 2024 and RAVDESS. NC terms arguably restrict even internal use by a commercial distributor (inference).
- **Share-alike:** DEMAND, QUT-NOISE, OpenSLR 83 and probably the Valentini noisy files.
  - Mixing speech with SA noise creates an adaptation, which has to stay BY-SA. A BY-SA 3.0 adaptation may be released under 4.0.
  - SA material can never be mixed with NC material.
  - Separate files with their own LICENSE count as aggregation and leave our MIT/Apache code unaffected (inference).
- **CC BY** requires credit and a note of the changes; resampling, joining and mixing all count. **BSD** (TSP) requires shipping the notice and the disclaimer.
- **No licence is not permission.** The AEC and DNS real recordings, the MIT IR Survey, AIR, OpenAIR and NOIZEUS are fetch-only. DNS and AEC file names also contain MTurk worker IDs and staff names.
- **AudioSet audio** is YouTube content, even though the DNS and AEC READMEs list it as CC BY 4.0.
- **Contract terms can sit on top of CC0:** Common Voice forbids re-sharing.
- **LibriVox** is public domain only in the US; outside it, the authors and translators should have died at least 70 years ago. Fetch the `_128kb.mp3` original, not archive.org's VBR transcode.
- **ACE:** CC BY versus BY-ND is unresolved, so don't ship modified copies.
- **Installer:** anything bundled needs its BSD text, CC BY credits and SA licence text in the app's NOTICE.

## 5. Fetching the starter set
`tools/fetch_test_audio.py` fetches section 2's starter set into a local cache. `tools/test_audio_sources.json` holds each source's URL, licence and attribution, every file's pinned size and SHA-256, and each output's recipe. Nothing it fetches goes in the repo; `.gitignore` refuses audio files anyway.
1. **Dependencies** (once): `python -m pip install numpy soundfile soxr`. Don't use `--user`: `python -I` leaves out the user site-packages.
2. **Freesound** (once, by hand, since it needs a login): download the originals of [448209](https://freesound.org/s/448209/), [242042](https://freesound.org/s/242042/), [273620](https://freesound.org/s/273620/), [337101](https://freesound.org/s/337101/) and [370973](https://freesound.org/s/370973/) into your Downloads folder under Freesound's names (`<id>__<user>__<title>.<ext>`), or anywhere else plus `--freesound-dir`. Use each page's Download button (the original), not the preview player, which fails the pin. The script copies them into the cache and checks them against their pins. A missing clip is named with its link and skipped.
3. **Run** `python -I tools/fetch_test_audio.py` (`--dir` for another cache). The first run took 195 s on the dev box: 71 requests, 74 MB. A second run only checks hashes and fetches nothing. Failed requests (HTTP 429 and 5xx, dropped connections) are tried up to 6 times, 5-80 s apart: archive.org has answered 500 three times running. A take whose input is missing is left out of `manifest.json`, and its old file is removed from `wav48/`.

The cache is `%LOCALAPPDATA%\noise-filter\test-audio`:
- `raw/`: the files as published, 132 MB. Zip members come by HTTP Range and are CRC-checked: 6.5 MB of TSP's 254 MB zip, 20 MB of VCTK's 11.75 GB, 19.6 MB of DEMAND's 314 MB.
- `wav48/`: 19 takes of 48 kHz mono 16-bit PCM, 104 MB and 19 minutes:
  - speech: `speech_tsp_*` (6 speakers, 14.5-17 s) and `speech_vctk_p225/p226/p237/p248` (mic1, utterances 003-008, 38-48 s), each joined with 0.4 s gaps and 10 ms fades;
  - noise: `noise_cafe_demand_pcafeter_ch01` (300 s) and `noise_keyboard/fan/aircon/dogs/cafe_<id>`;
  - music: `music_goldberg_var04` (69 s, from 96 kHz stereo);
  - echo: the ICASSP 2022 double-talk pair `aec_icassp2022_dt_00113069_mic`/`_lpb` (39 s; tier `unlicensed`, local only).
- `manifest.json`: per take, its source URLs, licence, attribution, changes, the raw and converted SHA-256s, rate and duration. `ATTRIBUTION.md` is generated from it, with the TSP notice.

Takes are whole recordings, not section 2's excerpts, since the cache has no size limit. Freesound 337101 and the AEC mic file are clipped in the originals.

**Where this differs from section 3's plan** (our choices, not the owner's): the AEC pair (tier `unlicensed`) is fetched by default, since there are no tier flags (`--unlicensed`, `--non-commercial`) yet. The kinds are `zip`, `file` and `freesound`; there's no tar reader yet. A recipe joins, fades and trims, but has no gain or channel choice (channels are averaged) and does no level normalisation.

To add a source, write its entry and outputs without `size`/`sha256` and run once with `--pin`: that fetches it, checks what it can (CRC-32, a publisher's SHA-1) and records the pins.
