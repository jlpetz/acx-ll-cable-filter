# noise-filter TODO

Active work for the noise-filter app (a Rust, Windows 11 mic noise filter and echo canceller). Each
entry holds status, decisions and the next step. The reasoning lives in `doc/todo/NN-slug.md`, and
verified research numbers in `doc/research-2026-10.md`. `python todo_check.py` checks entry sizes and
gives the next free number.

Phases: 0 offline (WAV in, WAV out), 1 live noise filter, 2 echo cancellation, 3 GUI and tray.

---

## Active Tasks

---

### 1. [project] Repo, workspace and crate layout
**Priority**: High (phase 0) | **Status**: own git repo at `C:\Users\Administrator\Downloads\noise-filter`, copied out of the TMR workspace 2026-10-26; v0 built (crates `nf-core`, `nf-mem`, `nf-stages`, `nf-engine`, `nf-gui`, `nf-app`); first commit 2026-10-28 (f9b0d35, the owner's OK).
- Decided (owner, 2026-10-26): Windows 11 only; its own repo, outside TMR; public release is the intent; its own TODO list; CLAP hosting for post-effect plugins, with a published host profile, and the AEC and noise filter in the same format (#17; the owner's OK, 2026-11-02); large pages by default (#6); GTCRN dropped (#5); the FastEnhancer licence question waits until we get there (#5).
- Owner's wish (tentative, 'should probably publish the spec'): a standard stage interface, published so others can add stages such as a voice changer (#3, #17).
- Decided (owner, 2026-10-28): the x86-64-v3 CPU baseline, like TMR; both exes check the CPU first and refuse others (exit 5). TMR's commit rules, rustfmt settings and fmt-then-clippy routine (`CLAUDE.md`).
- Pin exact versions: `wasapi =0.26.0`, `windows-reactor =0.100.0`, `sonora =0.2.0` (from crates.io again: the vendored `mul_add` patch went with the v3 baseline, #10), later `ort =2.0.0-rc.15`.
- The old pre-move copy at `C:\Users\Administrator\Downloads\TMR\noise-filter` was deleted 2026-11-02 (the owner's OK; it held only a TODO.md, `doc/` and `todo_check.py` from 2026-10-26).
**Next**: start future sessions in this folder; keep the crate layout as the stage interface (#17) and model stages (#5) land.
**Detail**: this entry; project rules in `CLAUDE.md`

---

### 2. [dsp] Offline harness: WAV in, WAV out, through the whole chain
**Priority**: High (phase 0) | **Status**: partly: `noise-filter process` runs a WAV (and `--ref`) through the chain offline, `--warm-up` reruns a live chain recording exactly, and `bench` times the stages; the conformance checks and the stage interface (#17) aren't built.
- Runs the same chain the live app runs (AEC → noise stage with its floor/gate), through the same stage interface (#17), on WAV files, faster than real time, with no audio devices. It's the base for #3's conformance test, #13 layer 0 and #14's batch scoring.
- Checks: the measured stream offset equals each model's declared `latency_samples − hop` to the sample (the hop of buffering only shows live, #13); output matches each model's reference; CPU per frame (p50/p99/max).
- Runs under `cargo test`, which the project rules allow on the dev box. `cargo bench` isn't on that list: ask the owner, or time frames in an ignored test.
**Next**: build it with RNNoise (#4) first, then the echo canceller on recorded mic+reference pairs.
**Detail**: `doc/todo/2-offline-harness.md`

---

### 3. [models] Model packs: the published format for ONNX noise models
**Priority**: High (phase 0) | **Status**: design, fact-checked 2026-10-26; spec v0 not written.
- Effects are CLAP plugins (owner). Making the AEC and noise filter CLAP-shaped stages too is our proposal, awaiting the owner (#17). A model pack is data the noise stage loads: manifest + ONNX + licences + checksums, no code; `spectral` or `waveform`; explicit cache pairs, layout, output kind, latency; separate code/weights/data licences and patent notes; sherpa-onnx key names; the manifest wins over embedded metadata; reject custom ops and outside paths.
- Models needing custom DSP (RNNoise, DFN3) are built into the noise stage in Rust.
- Always output full band at 48 kHz; 16 kHz models via a 1536/768 STFT (bins 0-256, ×1/3) plus a pluggable high-band policy, A/B'd in #14.
**Next**: write spec v0 and the noise stage's internal model trait; the conformance test in #2.
**Detail**: `doc/todo/3-model-spec.md`

---

### 4. [models] RNNoise baseline (`nnnoiseless`)
**Priority**: High (phase 0) | **Status**: chosen as the baseline (owner, 2026-10-26).
- Pure Rust, BSD-3 code and weights, 48 kHz, 10 ms frames (matches AEC3 and WASAPI), 20 ms algorithmic latency, ~0.4% of a core. Weakest quality, so it's the bring-up model and later the eco mode. Its VAD output can drive the gate (#11).
- Gotchas: `default-features = false`; warm up the state before streaming (first-call and reset allocations); dev-profile `opt-level` for dependencies. It can't load RNNoise 0.2 models.
**Next**: first model through #2, then the first live model in phase 1.
**Detail**: `doc/research-2026-10.md` (Noise models)

---

### 5. [models] Candidate models: FastEnhancer, UL-UNAS, DeepFilterNet3
**Priority**: Medium | **Status**: shortlisted, not integrated; weights licences fact-checked 2026-10-26 (#7). GTCRN dropped (owner, 2026-10-26).
- FastEnhancer-B 48k: 21.3 ms, full band, no resampler, the best paper fit. Its published weights can't ship without the author's grant (no weights licence; AI-Hub and non-commercial training data); our own retrain at a 10 ms hop is the alternative (#7). Asking the author waits until we get there (owner).
- UL-UNAS: 32 ms, 16 kHz, MIT weights in the repo (probably fine to ship); a China-only patent filing probably covers it (CN119673187, unconfirmed), to be listed in the NOTICE (#18). Run it through `ort`; tract mis-carries its GRU state (tract #2629).
- DeepFilterNet3: standard weights plausibly open; _ll is a bigger model (9.7M params) with weaker evidence. Opt-in only.
- Open (owner): does dropping GTCRN over its patent filings also cover UL-UNAS, which probably shares the family? Until answered, UL-UNAS stays the next integration.
- RNNoise (#4) is the fallback. Choose by #13 and #14 on the owner's own mic, not by paper scores.
**Next**: after #2/#3, integrate UL-UNAS; FastEnhancer when its weights or our retrain allow.
**Detail**: `doc/research-2026-10.md` (Noise models), `doc/todo/7-retraining.md`

---

### 6. [models] Inference memory: large pages by default, and hand-written ports
**Priority**: Medium | **Status**: decided (owner, 2026-10-26): large pages by default; requiring `SeLockMemoryPrivilege` is fine.
- The privilege is granted once, elevated (the installer, or a `--setup-large-pages` step as in TMR), and takes effect at the next sign-in. Running without admin afterwards is unconfirmed: XMRig says yes for Windows 10+, but an admin account's UAC-filtered token may drop the right. Test both account types.
- One large-page block, allocated at start-up, holds weights, state, scratch and audio buffers (TMR's direct `VirtualAlloc2` path, never placeholder-replace). Without the privilege, or without contiguous memory: 4 KB pages pinned with `VirtualLock` (which needs no privilege), plus a warning (our proposal; refusing to start is the alternative). Size the block at start-up; a model switch that doesn't fit restarts the engine.
- Also: a hard working-set minimum (keeps code and heaps resident), pre-faulted buffers, MMCSS threads (#8). ORT path: preallocated outputs, a kept arena, optionally a pinned env allocator. Hand ports: zero allocations per frame.
- Answered 2026-10-25: no INT8 or GPU inference; FP32 on the CPU, one thread.
**Next**: measure ORT's per-`Run` allocations for UL-UNAS (WPR/ETW); then the port order.
**Detail**: `doc/todo/6-inference-memory.md`

---

### 7. [models] Retraining route: clean-licence data, a 10 ms hop, lower latency
**Priority**: Low | **Status**: fact-checked 2026-10-26.
- Re-implementing every candidate in Rust is unblocked (MIT, or MIT/Apache for DFN). Only shipping some published weights is in question: UL-UNAS probably fine; DFN3 plausible; FastEnhancer-48k not without a grant. UL-UNAS's probable China-only patent goes in the notices. GTCRN, the best-licensed, was dropped by the owner (the least attractive of the set).
- FastEnhancer-48k retrains easily: `hop 480`, `n_fft 960` gives 20 ms and matches WASAPI; 1 GPU; clean data from DNS5 fullband, VCTK, Freesound CC0, FSD50K CC0/BY. ShareAlike sources mean the result may be CC BY-SA. Independent retrains have under-performed, so budget for iteration.
**Next**: read wall-clock time from FastEnhancer's TensorBoard logs and estimate the cost; decide after #14 compares the published models.
**Detail**: `doc/todo/7-retraining.md`

---

### 8. [engine] Audio I/O: mic capture, cable render, loopback reference
**Priority**: High (phase 1) | **Status**: built in v0 (nf-engine), reviewed 2026-10-26, run live on VB-CABLE 2026-11-01/02 (bursty delivery found and handled; exclusive streams added, #13).
- `wasapi` crate. Exclusive mode by default for the mic (3.33 ms) and the output (2 ms; never on Windows' default playback device), falling back to shared (IAudioClient3 small periods where the driver allows, else 10 ms) with a note (owner, 2026-11-02).
- Mic in RAW mode where supported. The reference is a loopback of the chosen speakers, zero-filled through silence. `SetDuckingPreference(TRUE)` on our sessions.
- **Requirement:** every audio thread (capture, DSP, render, inference worker) registers with MMCSS "Pro Audio" (`AvSetMmThreadCharacteristicsW`). MMCSS gives it priority 23-26 (High category; `SystemResponsiveness` keeps 20% for others) and tags it Deadline QoS. Without the tag, a minimised or occluded window puts the process at Low QoS. MMCSS priority also weighs foreground state, so check dropouts with the window minimised in phase 1.
- Threads are joined by SPSC rings; no allocation or locks on audio threads; device-change rebuilds on a control thread.
- The output target (one period + one block + margin) now starts at 14 ms with the exclusive output and walks down with the adaptive target (6.8 ms on the cables, #13).
**Next**: dropouts with the window minimised and a long soak; real hardware's delivery (#21).
**Detail**: `doc/todo/8-audio-io.md`

---

### 9. [engine] Clock drift and buffer control
**Priority**: High (phase 1-2) | **Status**: built in v0; simulated against stepped device clocks (nf-engine `pipeline/tests/events.rs`).
- Mic, speakers and cable run on three clocks. A PI controller (nf-core `DriftController`) on each FIFO's fill drives a ±1-sample-per-block slip (`SlipResampler`); a small target fill, not a big safety margin. The output fill is interpolated from the device clock and calibrated to the padding, so the latency holds still as clocks beat; coarse corrections handle stalls and late wakes.
- Small IAudioClient3 periods rule out RATEADJUST, so drift is corrected in the app.
**Next**: with #8; measure real drift (ppm over 30+ minutes) on the owner's device pairs. Look into the output path's delay moving 11-13 ms within a 60 s `quality` take (3 of 6 on the cables, 2026-11-01; #14's detail file): the report doesn't yet show the output's coarse drops.
**Detail**: `doc/todo/8-audio-io.md`

---

### 10. [aec] Echo cancellation (WebRTC AEC3 via `sonora`)
**Priority**: High (phase 2) | **Status**: integrated in v0 (`Aec3Stage`). Live on the cables 2026-11-01: the reference timeline rebuilt (`reference.rs`: 450 gaps a minute to none, the echo path still), and the quality scene made causal, and a chain recorder added. With a calibrated, realistic echo delay (`quality --echo-delay`, default 25 ms) the echo drops 45-62 dB a far-only turn, as offline; an echo arriving sooner than the reference's lead (12-14 ms) after its tap cancels less (13 ms 45 dB, 10 ms 39, 5 ms 17, 0 ms 11). One earlier take, 14 dB, stays unexplained.
- `sonora` 0.2.0 from crates.io again (2026-10-28): the owner's move to x86-64-v3 makes its `mul_add`s FMA instructions, so the vendored patch went (AEC3 91 µs a block at v2, 55 patched, 50 at v3); no FTZ/DAZ, ~1e-9 dither on the mic and reference instead. sonora #38/#40 (allocations) and sonora #41 (AEC3 config) are still unreleased. 10 ms frames; `MaxProcessingRate::Rate48kHz` (the default caps the chain at 16 kHz).
- Residual echo: the noise model keeps leftover far-end echo (it's speech), so AEC3's suppressor stays on. One 10 ms frame clock for all stages.
- Proposed (#17, awaiting the owner): a CLAP-shaped stage with the mic on the main input, the speaker reference on a second input port, one output.
- Order: AEC → noise model → gate. Opt-in "using speakers" mode. Stop the cable being picked as the reference. Show AEC3 delay and ERLE. Warn when NS is on without AEC on speakers.
- Later: an optional Windows Voice Clarity path; a neural AEC (LocalVQE, 16 kHz, GGML) as a v2 experiment.
**Next**: lower the lead's floor for short echo paths (a smaller loopback period, or a mic wait only when the path is that short); recorded pairs in #2; real speakers in phase 2. A low take's chain recording (`quality --save`) shows why.
**Detail**: `doc/todo/10-aec.md`

---

### 11. [dsp] Noise floor and gate
**Priority**: Medium (phase 1) | **Status**: built in v0 (`NoisePost`: floor mix + gate); gate redesigned 2026-10-26.
- Floor = maximum attenuation: mix delay-matched dry signal into the model's output, in the same frames, with no added latency. It also hides model artifacts.
- Gate: static or auto threshold, hold, linear-in-dB attack/release, no lookahead. Auto follows a smoothed level of the model's output.
- Our call (2026-10-26, not the owner's): "hold open on speech (VAD)" defaults to off. RNNoise's VAD fires on 87-100% of pure-noise blocks, which held the Auto gate open.
- Open: at the default 6 dB margin, RNNoise's residual bursts sometimes open the gate (9-12 dB measured better, speech effect at those margins not measured); some gaps take 0.7-2 s to close after a talk spurt.
**Next**: tune the margin and rise rates on real recordings in the Lab (#14).
**Detail**: `doc/todo/11-floor-gate-meter.md`

---

### 12. [dsp] Input level meter
**Priority**: Medium (phase 1) | **Status**: designed (owner's request).
- Raw-input peak and RMS in dBFS with peak hold, before any processing; a clip light that latches at about −0.1 dBFS or on flat-topped runs.
- Toggle on/off, about 20 Hz; auto-pause when the window closes (minimise detection is still open, #15). Later: output level and gain reduction.
**Next**: phase 1: a console readout (peak/RMS dBFS, clip latch) from the atomics, so the owner can check levels during live runs; the GUI meter in phase 3.
**Detail**: `doc/todo/11-floor-gate-meter.md`

---

### 13. [lab] Latency measurement
**Priority**: High (phases 0-2) | **Status**: layers 1 (readout) and 2 (sweep test, virtual or acoustic) built in v0, layer 2 run on the dev VM's VB-CABLE loop and a cable-only mode built and run (2026-11-01): each cable is 52-62 ms by itself. A per-block latency budget built and run the same day: readout 64 ms = mic delivery 16 + output queue 22 + render side 26 (the design was 32: a block + the queue), the chain's 9-19 ms on top.
- Layer 0 offline conformance (#2). Layer 1 always-on readout: capture QPC stamps (`BufferInfo`) vs cable play-out (`AudioClock`), with Chromium's sanity checks and ring occupancy subtracted (console in phase 1). Layer 2 virtual loop (two cables). Layer 3 real mic and speakers, captured together.
- Probes: sweep or a coded pulse for bypassed and acoustic paths; clean speech with GCC-PHAT (Hilbert-envelope fallback) through the model; AEC off or an exclude-self process-loopback reference.
- Echo-path delay is measured, not predicted: loopback stamps are mix time, not play-out, and AEC3 mostly ignores `set_stream_delay_ms`. Log engine period, RAW mode and cable settings with every result.
- Latency options (2026-11-01), the defaults since 2026-11-02 (owner: the lowest latency by default, with switches to turn each off, e.g. for recording the raw mic): exclusive output (2 ms), exclusive mic (3.33 ms), adaptive output target. Readout 64.9 ms shared, 34.6 exclusive output, 24.8 both, 18.1 both + adaptive (app path 28 ms with the chain); no glitches in 30-60 s takes.
**Next**: longer soak runs for glitches; IAudioClient3 small periods on the output and the speakers' keep-alive (shorter loopback packets, a shorter reference lead, #10), to verify on real hardware; layer 3 in phase 2.
**Detail**: `doc/todo/13-latency-measurement.md`

---

### 14. [lab] Quality analyser: the Lab mode (MOS)
**Priority**: Medium (phase 0 batch, later GUI) | **Status**: designed (owner: in the app, not a companion); metric licences fact-checked 2026-10-26. Phase 0's live half, `noise-filter quality` (level metrics over the virtual cables), built and first run 2026-11-01: the noise test works (-27 dB noise, -0.6 dB speech); the echo test found #10's reference gaps.
- Record once, process many: the same input (WAV playlist, or a saved raw take plus loopback) through every model. Also score the live output, to catch real-time glitches.
- Metrics (Lab download): DNSMOS P.835/P.808 (MIT + CC BY 4.0, 16 kHz only), SIGMOS (MIT, full band, no published accuracy), AECMOS (MIT, mode B). Optional: ScoreQ. Left out: NISQA, SQUIM subjective (non-commercial), UTMOS.
- Test audio (2026-11-01): `python -I tools/fetch_test_audio.py` fetches a licence-checked starter set (19 takes: speech, noise, music, an AEC pair) into `%LOCALAPPDATA%\noise-filter\test-audio`; no audio in git. Catalogue: `doc/todo/14-test-audio-catalogue-2026-11-01.md`.
- Statistics over distinct clips (~50 paired), paired differences with 95% CIs. Final say: blind A/B or CCR, loudness-matched (BS.1770); ABX only for "is there a difference?".
**Next**: phase 0: the batch CLI with #2 (the echo test's live half is done: #10), playlist, DNSMOS/SIGMOS feature parity. Phase 1: live capture saving raw take and loopback (the chain recorder, `quality --save`, is its first part). Phase 2: AECMOS. Later: the Lab page (#15).
**Detail**: `doc/todo/14-quality-lab.md`

---

### 15. [gui] Settings window (Reactor)
**Priority**: Medium (phase 3) | **Status**: designed.
- Device pickers (mic, output cable, echo reference), AEC and noise-filter checkboxes, floor/gate controls, model picker, meter, latency readout, the Lab page.
- A separate process that talks to the engine over a named pipe. Owner: minimise for now; once #16's tray exists, the window closes freely and the tray reopens or starts it.
- Noise filter off means bypassed: the model isn't run and its delay is removed (a brief jump on toggle is fine).
**Next**: after phase 2.
**Detail**: `doc/todo/15-gui-tray.md`

---

### 16. [engine] Tray, engine process, settings and autostart
**Priority**: Medium (phase 3) | **Status**: designed.
- The always-on engine owns the tray icon (Shell_NotifyIcon): open settings, pause, quit. Settings persist; start with Windows via HKCU Run.
**Next**: with #15.
**Detail**: `doc/todo/15-gui-tray.md`

---

### 17. [plugins] Stage format: CLAP for effects, the AEC and the noise filter (owner)
**Priority**: Medium (shapes phase 0) | **Status**: CLAP chosen for plugins (owner, 2026-10-26); the AEC and the noise filter are CLAP-shaped stages too (owner, 2026-11-02: "the best and most modern alt to VST3"). Stage interface being built (phase 0).
- Every stage is CLAP-shaped. The AEC takes the mic on its main input and the speaker reference on a second input port (CLAP allows one main input plus non-main ones, the sidechain convention). The noise filter is a stage that loads model packs (#3), with the floor and gate inside it and a namespaced extension publishing its speech probability. Effects (a voice changer) come after.
- Our own stages run in-process through the same Rust interface (no FFI). They can also be exported as `.clap` files (and VST3 via clap-wrapper) for other hosts. Third-party plugins run in an fx-host process (shared memory, a watchdog that passes clean audio through on a miss, editors as its own windows).
- The published spec = a CLAP host profile (48 kHz, 480-frame blocks, port layout per role, supported and our own extensions) + the model-pack format (#3). VST3 hosting later; no VST2.
**Next**: the stage interface in phase 0, with #2 running stages through it.
**Detail**: `doc/todo/17-post-effects.md`

---

### 18. [release] Packaging, signing, licences and the user setup guide
**Priority**: Low | **Status**: notes.
- Windows 11, an x86-64-v3 CPU (owner, 2026-10-28; the exes refuse others, exit code 5), plus the Windows App Runtime (per-user, `--quiet`). Before a release: `tools/check_cpu_gate_asm.py` on both exes' asm, and after every toolchain update. Signing: Artifact Signing takes only individuals in the US or Canada; otherwise an organisation or an OV certificate. Unsigned builds get SmartScreen/Defender trouble.
- Large-page setup: the installer runs elevated once and grants `SeLockMemoryPrivilege` to the user (as TMR's `--setup-large-pages` does); it takes effect at the next sign-in (#6).
- A NOTICE file for every model's code and weights licence, and known patents (#7).
- User guide: VB-Cable latency ~1440-1536 samples, cable format 48 kHz; Sound → Communications "Do nothing"; turn off Discord/Teams NS and AEC; avoid Bluetooth headsets.
- Open: ship the console launcher `noise-filter-cli.exe` as `noise-filter.com` too? A plain `noise-filter ...` then works in terminals, but Win+R and Start search pick the .com and flash a console.
**Next**: when a release nears.
**Detail**: `doc/todo/18-release.md`

---

### 19. [driver] Our own virtual mic
**Priority**: High (after v0 live testing) | **Status**: option D chosen by the owner 2026-10-28 as the main goal: our own root-enumerated virtual audio driver, based on an existing open-source one and cut down to what we need. VB-CABLE (C) stays the v0 path and the latency/CPU benchmark; usbip-win2 (B) is a lab comparison at most.
- Owner's criteria: latency and quality at least VB-CABLE's; controllable from inside the app; open source others can reuse.
- Why not A/B (research 2026-10-26): USB audio over UDE needs a filter on every USB 3 root hub, as usbip-win2 has; usbip-win2 has an open capture bug (usbip-win2 #200) and a past bugcheck in this path (usbip-win2 #181); USB adds 1 ms framing plus class-driver queueing.
- Candidate bases: Microsoft's ACX AudioCodec sample (MS-PL, root-enumerated, simulated stream engine), SimpleAudioSample/SYSVAD (WaveRT); microsoft/low-latency-audio (MIT, open source, unreleased) for its ACX low-latency streaming, its ASIO driver's user-kernel interface and its control panel. Its USB half doesn't apply. In-box usbaudio2.sys is closed (Thesycon).
- Signing: attestation (EV certificate + Partner Center, or OSSign) for Windows 10/11 client; Server loads only HLK-signed, so the dev VM needs test signing. Test signing is out for users (BattlEye, Vanguard, FACEIT).
- Base study (2026-10-28): start from the ACX AudioCodec sample, cut down (about 3.5-4.5k lines when done, 19-34 dev-days, an estimate), borrowing low-latency-audio's packet-size code, stream engine and shared-buffer handling; SimpleAudioSample is the fallback. Drafts of a request to Microsoft are in the detail file, for the owner to post.
- Dev box toolchain (owner, 2026-10-28): Visual Studio 2026 with the C++ workload, the Spectre libraries and the WDK extension; WDK 10.0.28000.2526 and SDK 10.0.28000.2705 (ACX 1.0/1.1, KMDF up to 1.35, inf2cat, stampinf, infverif, signtool). VB-CABLE 3.3.1.7 loads on Server 2025.
**Next**: when the owner un-pauses the spike: an EBS snapshot, `bcdedit /set testsigning on` and a reboot; then the spike's seven checks.
**Detail**: `doc/todo/19-virtual-mic-ude.md`, `doc/todo/19-driver-base-2026-10-28.md`

---

### 21. [engine] Device health and calibration
**Priority**: Medium (phase 1-2) | **Status**: asked by the owner (2026-11-01); proposal ours, not started.
- Owner: tell the user when a device or driver delivers in bursts or late, so they can fix it; and a calibration that sizes the buffers to the devices, not too big or small.
- Proposed: a health readout per stream (steady or bursty, how late, drift, glitches, with a plain verdict and what to try), and a calibration (passive: measure delivery and set the starting targets; active with speakers: time the echo path and check it against the reference's lead, #10).
- Most of the measures exist: the latency budget (#13), the reference's facts (#10), the counters, the live probe.
**Next**: define the verdicts and thresholds, then `noise-filter devices --check` (real-hardware data needed for the thresholds).
**Detail**: `doc/todo/21-device-health.md`
