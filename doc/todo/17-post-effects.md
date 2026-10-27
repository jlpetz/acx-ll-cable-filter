# 17. One stage format (CLAP) for echo cancel, noise filter and effects

Status: the owner chose CLAP for plugins on 2026-10-26, then asked whether the noise filter and AEC
should use the same format. The proposal below says yes, and awaits the owner's OK.

## Every stage CLAP-shaped (proposal, 2026-10-26)
- **Two inputs is not a problem for the AEC.** CLAP's audio-ports extension allows one main input
  and output (at index 0) plus further, non-main ports. A second input port is the usual sidechain.
  - **AEC stage:** main input = mic; second input port = speaker reference (already resampled to the
    mic clock by the engine, #9); main output = echo-cancelled mic.
  - **Latency:** reported with CLAP's latency extension. It may change only during `activate`; if
    active, the plugin calls `request_restart`.
- **Noise stage:** a stage that loads model packs (#3), which are data rather than plugins. RNNoise
  and DFN3 are built in.
  - The floor and gate live inside it as parameters, because they need the model's gain and VAD.
  - It publishes its speech probability through a namespaced extension. A third-party denoiser
    without it gets a level-based gate instead.
- **Chain:** AEC → noise stage → effects (a voice changer, EQ). Each stage gets the same host
  profile.
- **Our stages:**
  - Run in-process through a Rust interface that mirrors CLAP's model (activate with rate and block
    size, process with ports, latency, params, state), so there's no FFI on the hot path.
  - A thin wrapper (clack-plugin, or nih-plug's ISC CLAP export) can also export them as `.clap`
    files. `clap-wrapper` then makes VST3s, so other hosts (Bitwig, REAPER, and OBS once its VST3
    host lands) can use our AEC and noise filter.
  - In those hosts, routing the speaker reference to the AEC's sidechain is the user's job.
- **Users can swap stages:** a third-party CLAP denoiser can replace ours.
- **What doesn't change:** clock-drift handling, reference alignment and the 10 ms block size are
  the engine's job before it calls any stage.
- **What it gives the owner's ask:** one published format for every stage (the CLAP host profile),
  plus the model-pack format for ONNX models inside the noise stage.

## The owner's ask
No universal plugin system fits this case, so the app should provide one: a standard interface,
published so that others can add stages after AEC and noise suppression, e.g. a voice changer.
Publishing is tentative ('should probably publish the spec').

## Effects details (CLAP chosen by the owner, 2026-10-26)
- **Models (our proposal, part of #17, awaiting the owner):** our own declarative model-pack spec (#3).
- **Effects:** don't invent a binary plugin ABI. No plugins would exist for it on day one, and CLAP
  already defines threading, state, parameters, latency and GUI. "Publishing the spec" for effects
  then means a short **host profile**, not a new format:
  - **What the app guarantees:** 48 kHz float; fixed 480-frame blocks (`activate(48000, 480,
    480)`); mono or stereo; no MIDI or transport.
  - **Supported CLAP extensions:** audio-ports, params, state, latency, gui, log, thread-check.
  - **Namespaced feature tags** for our stages. CLAP has no denoise or speech tag; the nearest are
    `restoration` and `de-esser`.
  - **Optionally one namespaced extension:** a per-block speech-probability value from the noise
    model, so a voice changer can gate on it.

## Formats (verified at primary sources)
- **CLAP:**
  - MIT licence; 1.x is API/ABI stable (1.2.10, 2026-07-13).
  - First-class on Windows: a Win32 HWND GUI, and search paths `%COMMONPROGRAMFILES%\CLAP`,
    `%LOCALAPPDATA%\Programs\Common\CLAP` and `CLAP_PATH`.
  - Rust host: `clack-host` 0.2.0 (2026-09-12, MIT OR Apache-2.0, Windows in CI, MSRV 1.85). It
    calls itself "feature-complete" but warns of breaking changes.
  - Plugins in our categories: Graillon 3.x voice changer (free edition; Auburn's blog says CLAP,
    its product page doesn't), FabFilter Pro-Q 4, u-he Presswerk.
  - `clap-wrapper` (MIT) lets authors ship the same plugin as VST3.
- **VST3:**
  - The SDK has been MIT since 3.8.0 (2025-10-20); latest 3.8.1.
  - The Rust `vst3` crate 0.3.0 (MIT/Apache) is raw unsafe COM bindings, so we'd write the host
    layer. `vst3-sys` is still GPLv3, and so is nih-plug's VST3 export.
  - The Windows voice-changer and free-effect catalogue is mostly VST3, so it may matter sooner than
    "later" (unquantified).
- **VST2:** not distributable without a pre-October-2018 Steinberg agreement. No.

## Isolation
- **Real-time rules for plugins are advisory, never enforced** (CLAP: avoid malloc, locks and I/O
  on the audio thread; VST3 similar). An in-process plugin can stall or crash the mic chain.
- **Run third-party plugins in one separate fx-host process,** like Bitwig's "Together" mode (Bitwig
  keeps audio playing after a plugin crash).
  - **Audio:** a shared-memory section (`CreateFileMapping2`, build 20348+) kept resident with
    `VirtualLock`. Private VirtualAlloc2 memory can't be shared, and large pages are pointless for a
    few KB per block.
  - **Signalling:** an event pair per 10 ms block, with the fx-host's audio thread under MMCSS "Pro
    Audio".
  - **Watchdog** (our own design; Windows' 10-failure APO rule counts load and format failures, not
    runtime deadline misses):
    - on a missed deadline or a dead process, pass the clean audio through for that block;
    - restart the fx-host;
    - disable the plugin after N failures.
  - **Open:** jitter of a per-block cross-process round trip, which decides between synchronous
    calls and one extra block of latency.
- **GUIs:** plugin editors need an HWND and the main thread. Parenting across processes links the
  two input queues ("like juggling chainsaws"). So the fx-host owns editors as its own top-level
  windows, and the Reactor window only sends "open the editor for slot N".
  - The draft `clap.webview/3` extension (HTML GUI over messages) could render inside the Reactor
    process through WebView2 later; few plugins support it yet.
- **Alternative sandbox: WCLAP** (CLAP compiled to wasm32, run in-process under Wasmtime). It costs
  about 1.5× CPU and is immature (`wclap-bridge` is properly tested only on macOS).

## Next
1. The owner's OK on making the AEC and noise filter CLAP-shaped stages too.
2. Then the stage interface and host profile v0 in phase 0 (#2 runs stages through it).
3. clack-host in an fx-host process for third-party plugins.
4. VST3 hosting later.
