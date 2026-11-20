# Research record, 2026-10-25/26

This file records what two research workflows found. The viability findings and the six shortlisted
models were each re-checked by an adversarial verifier; the sub-15 ms model survey comes from the
sweeps alone. Versions and open PRs move quickly, so recheck them before coding against them. Where
a number below is our own estimate or count rather than a published figure, it says so.

## The pasted plan, checked

| Claim | Verdict |
|---|---|
| `ulunas_onnx` streaming models exist | True. Added 2026-02-13 in PR #11 by Kailai Shen (community), MIT |
| "~20 ms latency floor" | False for UL-UNAS: 32 ms algorithmic (512/256 at 16 kHz, no lookahead). 20 ms matches DeepFilterNet3_ll ('minimum latency is 20 ms') and RNNoise v0.1.1; which one the plan meant is our guess |
| Convert to INT8 ONNX | No INT8 version exists. ORT's quantizer doesn't cover GRU, and per-node overhead, not MACs, sets the cost |
| Fork Poise | Poise (chabandou/poise-voice-isolator) cleans system **playback**, not the mic; about 190 ms end to end on Windows |
| nih-plug VST3 in Carla behind VB-Matrix ASIO | A plugin on the mic sees the speaker reference only if the host routes loopback into a sidechain (nih-plug supports aux inputs; OBS PR #12752, open, adds a VST3 host with sidechain). That is another hop with its own clock and buffering, while a standalone app opens the loopback directly. nih-plug builds VST3/CLAP only, its VST3 bindings are GPLv3, OBS and Equalizer APO host VST2, and ASIO doesn't reach Discord/Teams |
| DeepFilterNet dominates via VST | Only third-party VSTs exist. Upstream's last release was v0.5.6 (2023-08-31) and its last commit 2024-10-17. werman's RNNoise plugin is the real incumbent |

## Latency arithmetic

- **Algorithmic latency of an STFT model:** window length + lookahead (frames × hop).
- **Capture-to-output with WASAPI periods of `p` samples:** `N + L + p − gcd(p, H)`, where N is the window, L the lookahead and H the hop, all in samples. A plain pass-through costs `p`.
- **Consequence:** a 10 ms hop matches the default 10 ms period exactly. UL-UNAS's 16 ms hop costs an extra ~8 ms (640 samples at 16 kHz = 40 ms in total). FastEnhancer-48k's 512-sample hop costs an extra ~9.3 ms (1472 samples = 30.7 ms). Periods that divide the hop remove the penalty.

## Noise models (released weights only)

| Model | Rate | Window/hop | Algorithmic | At 10 ms periods | CPU, one core | Code / weights licence |
|---|---|---|---|---|---|---|
| RNNoise via `nnnoiseless` 0.5.2 | 48 kHz | 960/480 | 20 ms | 20 ms | ~0.36% p50, 0.77% p99 (7800X3D, Windows, LTO) | BSD-3 / BSD-3 (2017 weights embedded) |
| FastEnhancer-B 48k (ICASSP 2026) | 48 kHz | 1024/512 | 21.3 ms | 30.7 ms | ~5% (our estimate for B; T ~2.5%; not measured on x86) | MIT / no explicit licence; training data includes AI-Hub and CC BY-NC sets |
| UL-UNAS (TASLP 2026) | 16 kHz | 512/256 | 32 ms | 40 ms | RTF ~0.04-0.19, none first-party: 0.066 (ORT, i5-14600KF); ~0.07 (1.1 ms mean, 1.8 ms p99 per hop, CPU unnamed); 0.19 (Python ORT, Xeon 8352V 2.1 GHz); 0.043 on one Arm phone core | MIT / MIT (files in the repo) |
| FastEnhancer-B 16k (DNS) | 16 kHz | 512/256 | 32 ms | 40 ms | 0.29-0.31 ms per hop (tract, i9-9900K) | MIT / no explicit licence; DNS3 + VCTK data |
| GTCRN (ICASSP 2024) | 16 kHz | 512/256 | 32 ms | 40 ms | 0.45-0.49 ms per hop (tract, i9) | MIT / MIT; the author confirmed commercial use (gtcrn #104) |
| DeepFilterNet3_ll | 48 kHz | 960/480 | 20 ms | 20 ms | heavy: ~9.74M params and ~1.1 GMAC/s (our count and estimate from the ONNX; the authors publish neither), 39 MB weights | MIT/Apache code / weights unresolved (#697/#700/#709) |
| DeepFilterNet3 | 48 kHz | 960/480 + 2 frames | 40 ms | 40 ms | RTF 0.19 (i5-8250U, single thread) | MIT/Apache code / weights: the DFN3 paper says they were 'published under an open source license' (licence not named; #697/#700/#709 unanswered) |

Quality notes:
- **RNNoise:** VCTK PESQ 2.29 (2.33 re-measured). It barely raises DNSMOS OVRL on blind DNS sets, lowers SIG (damages speech) and lets some noise through. Use it as the baseline and eco mode.
- **UL-UNAS:**
  - The authors' own DNS3 test gives PESQ 2.30.
  - On the official DNS2020 synthetic sets (CoFi-Lite's same-lab retrain) its edge over GTCRN is marginal: OVRL 3.13 vs 3.09 without reverb, 2.44 vs 2.43 with reverb. No significant gain on URGENT2026 either (issue #12).
  - The shipped checkpoint was retrained just before release and has no scores of its own.
- **FastEnhancer-48k:** no published 48 kHz quality for T/B. Users report occasional speech suppression (#9) and over-aggressiveness (#16). The 16 kHz DNS-trained models are weak on tonal or music noise (#4).
- **GTCRN:** below UL-UNAS. It leaves musical noise on noise-only input (#76) and drops the level.
- **DeepFilterNet3_ll:** about DFN3 quality in a third-party test (PESQ-WB 3.18 vs 3.22) at ~3x DFN3's CPU.
- **Nothing under 15 ms ships weights** (muNet, TVBC and Deep FIR have none; this survey comes from the sweeps, without an adversarial re-check). Getting under 15 ms needs a retrain with an asymmetric window pair: 8 ms is nearly free in published results and 4 ms costs quality. muNet's authors have patent applications pending.

Runtime notes:
- **tract #2629 (open):** tract carries GRU state across calls when a model passes `initial_h` but doesn't output `Y_h` (UL-UNAS, GTCRN), so its output diverges from onnxruntime. PR #2717 added a `reset_every_turn` option that the importer doesn't set yet. Check each model before using tract.
- **ort:** 2.0.0-rc.15 (2026-10-19, ONNX Runtime 1.31), still a release candidate with API changes each RC. The prebuilt binaries need AVX2. Pin the exact version.
- **UL-UNAS streaming ONNX** (re-checked 2026-11-02 against the files at `00f7c700`, parsed, not run; the pack's manifest pins
  their hashes):
  - One frame `[1,257,1,2]` (`mix`, B,F,T,RI) plus flat caches of 5358, 402 and 1056 floats (`conv_cache`, `tfa_cache`,
    `inter_cache`, each fed back from its `*_out`); opset 11 only; ir_version 6; pytorch 1.11.0; no `metadata_props`, no
    external data. **697 nodes** in `ulunas_stream_simple.onnx` (static shapes; 324 initializers, 170,892 floats); the
    other export, `ulunas_stream.onnx`, has dynamic output shapes and 5,482 nodes (2,412 `Constant`).
  - 28 GRUs: 10 unidirectional cTFA (their state is `tfa_cache`), 4 unidirectional inter-RNN (hidden 8; `inter_cache`), and
    14 bidirectional intra-RNN across frequency (hidden 4) that start from a constant zero `initial_h` each call and leave
    `Y_h` unused: tract #2629's pattern; ONNX Runtime runs it right.
  - It expects `torch.stft(n_fft=512, hop=256, win=512, periodic Hann, center=True, reflect, unnormalised)` at 16 kHz in ±1,
    computes `log10(clamp(|X|, 1e-12))` features (bins 0-64 linear, the rest in 64 ERB bands from 2031 Hz), and returns
    `enh = mix × m` with a real mask; causal, no lookahead.
  - Measured through our host (TODO 5's detail): its conformance pair matches upstream's `enhanced_*.wav` to -64 and -66 dB
    RMS (4 and 1 LSB at most); a run takes about 0.95-0.97 ms on the dev VM (p99 1.05-1.96 ms over 10,000 runs) with
    about 3,710 C runtime allocations (`doc/todo/6-inference-memory.md`).
  - ONNX Runtime 1.31 facts found on the way (2026-11-02; recheck on an update): `session.set_denormal_as_zero`
    (`with_flush_to_zero`) sets FTZ/DAZ once a process, on the first session's thread, for good; a model loaded from
    bytes resolves a relative external-data location against the current folder (only `..` and absolute paths are
    refused). We set neither, and refuse external data ourselves (`nf_ort::onnx_check`).
  - Synthesis must divide by the per-sample overlapped `w²` envelope; a scalar normaliser causes periodic distortion.
  - The reference output `enh_onnx.wav` is generated by a script and isn't in the repo; `audio/enhanced/*.wav` (the offline model) is.

## Echo cancellation

- **`sonora`:** a pure-Rust port of WebRTC AEC3, BSD-3, with Windows CI.
  - 0.2.0 (2026-07-29) is the M145 port. Main carries further fixes up to M156 (unreleased), including render-buffer headroom on underruns.
  - **Open PRs:** #38 (denormal stall on silent reference, stacked on #36), #40 (allocation-free processing), #41 (custom AEC3 config).
  - **Workaround until those merge:** FTZ/DAZ on the DSP thread plus ~1e-9 dither on the reference. The stall is mostly an Intel problem.
  - **Cost:** about 45 µs per 10 ms frame on an i9-14900K with denormals handled.
- **AEC3 behaviour:** 10 ms frames and 4 ms internal blocks. The delay-estimation window is about 608 ms by default (our calculation from the code). It detects clock drift but doesn't resample.
- **AEC3 latency:** at 48 kHz (sonora 0.2.0, `Rate48kHz`; 0.3.0 the same, 2026-11-20) the capture output lags the mic by 430 samples (8.96 ms), measured with an impulse and GCC-PHAT (`nf_stages::AEC3_LATENCY`, `crates/nf-stages/tests/aec3.rs`). Reading the code gave about 4.5 ms, which is too low (`doc/dev/api/sonora-nnnoiseless.md` 1.8).
- **Ruled out:**
  - tonarino `webrtc-audio-processing` doesn't build on MSVC (PR #102 open).
  - SpeexDSP MDF has no delay estimator and is linear only.
  - Voice Capture DSP outputs at most 22.05 kHz.
  - The `aec3` crate (RubyBit/aec3-rs 0.4.0; MIT OR BSD-3-Clause on crates.io, with a LICENSE GitHub can't classify) calls its API a work in progress, expected to change.
- **Windows built-in:**
  - Voice Clarity does AEC, NS and dereverb for Communications-mode capture, only when the OEM supplies no comms processing.
  - `IAcousticEchoCancellationControl` (22621+) returns E_NOINTERFACE when unsupported.
  - Both depend on the device, so this is an optional path at most.
- **Neural AEC for later:**
  - LocalVQE: Apache-2.0, 16 kHz, GGML not ONNX. v1.4-AEC has 203K params and takes 0.83 ms per hop on 4 threads.
  - DTLN-aec: MIT, 2021.
  - NKF-AEC has no licence.

## Windows audio I/O

- **Crate:** `wasapi` 0.26.0 (2026-10-11, MIT, windows 0.62). Its API changes every release, so pin it.
  - Missing pieces: RATEADJUST / `IAudioClockAdjustment` and MMCSS. Use the windows crate directly for those (`AudioClient::as_iaudioclient`).
  - cpal is shared-mode only, has no IAudioClient3 and ignores the SILENT flag.
- **IAudioClient3 low-latency periods:**
  - `InitializeSharedAudioStream` accepts only EVENTCALLBACK, so there's no loopback, autoconvert or RATEADJUST on that path.
  - Small periods are documented only for Microsoft's inbox HDAudio driver (128-480 samples). USB mics and VB-Cable will probably run at 10 ms.
  - Small periods on an endpoint switch every stream on it.
- **VB-Cable:**
  - The default pipe is 7168 samples, about 149 ms at 48 kHz. The rule is pipe = 3 × the largest client buffer, so 3 × 480 = 1440 samples (about 30 ms) is the floor for 10 ms clients by that rule (our inference; VB-Audio's own example is 3 × 512 = 1536).
  - VB-Audio publishes only the maximum latency, never a measured delay.
- **Clocks:** mic, speakers and cable run on independent clocks. Correct drift in the app: rubato 5.0.1 (MIT OR Apache-2.0) has an Async resampler that takes ratio changes at runtime, and a Slip type (crossfaded insert or drop, no delay, handles up to ~0.8% drift).
- **Loopback reference:**
  - Shared mode only.
  - No packets arrive during silence: zero-fill on the mic clock, or keep a silent render stream open at the default period.
  - It's tapped before the volume control, so echo gain changes with the volume slider.
  - DRM-protected audio can't be captured.
- **Ducking:** triggered by the device role (the communications device), not the stream category. Call `SetDuckingPreference(TRUE)` on our sessions, and the user sets Sound → Communications to "Do nothing".
- **Mic mode:** capture RAW where `RawProcessingSupported`, so OEM effects don't run ahead of AEC3.
- **Threads:**
  - MMCSS "Pro Audio" via `AvSetMmThreadCharacteristicsW`; the `audio_thread_priority` crate only registers "Audio".
  - SPSC rings between threads (rtrb 0.4.0).
  - `assert_no_alloc` in debug builds.
  - Inference stays off the WASAPI callback.
- **Two windows-core versions** (Reactor and windows 0.62) link side by side. Keep all audio COM in the engine crate, and run audio threads in the MTA.

## GUI (windows-reactor 0.100.0)

- **Controls it has:** ComboBox, CheckBox, ToggleSwitch, Slider, NumberBox, ProgressBar (no height setting), Rectangle, Border, InfoBar, ContentDialog. Build meters from Borders, as tmr-gui does.
- **Feeding it from the audio thread:** `LocalSender` isn't Send. Use tmr-gui's re-armed `spawn_background` waiter, reading values the audio thread publishes in atomics.
- **Lifecycle limits:** `App::run` returns only when every window has closed. There's no close-cancel, no exposed HWND, no start-hidden and no timer.
- **Runtime:** the Windows App Runtime 2.x (2.4.0.0 minimum; 2.5.1 current, installer 120,199,480 B). It can install per user without UAC (`--quiet`); self-contained is about 56 MiB.
- **Windows 11 (22000+) only:** that's Reactor's limit (its bootstrap calls TryCreatePackageDependency); the App SDK itself supports Windows 10 1809.

## Landscape

- **Existing free options:**
  - Discord already bundles Krisp for free, plus its own AEC.
  - Windows Voice Clarity covers Communications-mode apps.
  - NVIDIA Broadcast has dereverb but no AEC.
  - Krisp itself is a 7-day trial, then $8-16 a month.
- **Small 2026 Windows AEC apps:**
  - DeviesVi/rust-aec: MIT, sonora with a patched sonora-aec3, v0.3.3.
  - samudinzul/aec-client: MIT, C++ with ImGui, DTLN-AEC at 16 kHz by default, AEC3 marked experimental, unsigned; Defender deletes its exe.
  - mini-aec and EchoNull have no licence.
- **Rust noise-only apps:** noisegate, openmic, audio-forge. Also RoomMute (Rust/WASAPI/VB-Cable, standard DFN3; AEC not checked).
- **Signing:** Microsoft Artifact Signing takes individuals only in the US or Canada, organisations in more countries. Otherwise an OV certificate (about $200-400 a year, secondary source).

## Follow-up fact-checks (2026-10-26)
These findings live in the detail files that use them:
- **Inference memory** (why the runtimes allocate, paging, large pages, the ORT allocator controls): `doc/todo/6-inference-memory.md`.
- **Model-pack spec, sherpa-onnx metadata conventions, and 16 vs 48 kHz:** `doc/todo/3-model-spec.md`.
- **Weights licences, patents and retraining:** `doc/todo/7-retraining.md`.
- **CLAP and VST3 hosting and isolation:** `doc/todo/17-post-effects.md`.
- **WASAPI timing, probes and estimators:** `doc/todo/13-latency-measurement.md`.
- **Quality metrics (DNSMOS, SIGMOS, AECMOS) and statistics:** `doc/todo/14-quality-lab.md`.
