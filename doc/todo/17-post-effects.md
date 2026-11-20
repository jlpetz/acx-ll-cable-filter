# 17. One stage format (CLAP) for echo cancel, noise filter and effects

Status: the owner chose CLAP for plugins on 2026-10-26, and on 2026-11-02 OK'd the proposal below: the
AEC and the noise filter are CLAP-shaped stages too ("the best and most modern alt to VST3"). The stage
interface v0 is built (2026-11-02, "Built" below); the `.clap` export and the fx-host are next.

## Every stage CLAP-shaped (proposed 2026-10-26; the owner's OK, 2026-11-02)
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

## Optional automatic gain control

The owner put automatic gain control alongside EQ and voice changers in scope (2026-11-20). Our proposal is a
separately switchable app effect using Sonora AGC2. Someone using headphones should be able to select it with AEC
off, with any selected denoiser or on its own. TODO 11 covers the existing gate and the proposed gate-only mode.

Start with AGC2's adaptive digital gain and peak limiter. Set `adaptive_digital` explicitly and leave
`input_volume_controller` off so the level adjustment applies to the cable feed. Start after the optional denoiser
and gate. Compare gain on and off on quiet speech followed by loud speech, a sudden peak and a pause with room tone.
Check the speech level, peak clipping and any audible rise in background noise. Include AGC2 in TODO 10's digital
silence check. Measure the added sample delay, p99 block time and audio-thread allocations with the offline harness (TODO 2).
[Sonora 0.3.0 gain controls](https://github.com/dignifiedquire/sonora/blob/sonora-v0.3.0/crates/sonora/src/config.rs#L216-L250)

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

## Built (2026-11-02): the stage interface v0
The decisions in this section are ours, within the owner's 2026-11-02 decision, unless a line says otherwise.
- **The trait** (`crates/nf-core/src/stage/`): `nf_core::stage::Stage`, CLAP's model in Rust, in-process, no
  FFI and no clack dependency.
  - descriptor (id, name, vendor, url, version, description, feature tags; the role is parsed from a tag). Its text,
    the ports' and the params' are `Cow<'static, str>` (`nf_core::stage::Text`): literals for our stages, owned
    strings for what is only known at run time (a pack's manifest, a third-party plugin's descriptor in the fx-host);
  - audio ports (main mono in and out at index 0; the AEC's speaker reference on a non-main input);
  - `activate(&Activation)` (always `Activation::HOST`, 48000/480/480);
  - `process(ProcessIo { inputs, output })`, `reset`, `latency`;
  - params (list, value, set), state (a `b"NFST"` blob of the params). `set_param` takes `&mut self`: whoever holds
    the stage calls it, on the processing thread between blocks while it is active, which is where CLAP delivers
    its in-events. No other thread reaches a stage;
  - extensions as typed getters returning `Option<&dyn Ext>`: `clap.gain-adjustment-metering/0`, and ours
    `noise-filter.speech-probability/0` and `noise-filter.aec-telemetry/0`.
- **The host profile** is `doc/spec/host-profile-v0.md`. `check_host_profile` enforces its role, port and param
  rules; `Chain::activate` and the harness run it, and the chain refuses a stage in the wrong slot.
- **Our stages on it:**
  - AEC3 (`noise-filter.echo-cancel`): ports "Mic" + "Speaker reference", latency 430, no params, telemetry.
  - The noise stage (`noise-filter.denoise`, `nf_core::noise::NoiseStage`): any model through an internal
    `NoiseModel` trait (#3), RNNoise now; the floor and gate inside it as params 1-9 over `ChainControls`' atomics
    (the owner's decision: inside the noise stage); gain adjustment always; speech probability when the model
    has a VAD.
- **The chain is their host:**
  - bypass, passthrough and priming stay host-level slot state (no `IS_BYPASS` param);
  - the latency is cached at activate;
  - the reference is the only sidechain source, the same block in every slot (not delayed by upstream slots, so
    the engine's lead holds for the AEC slot only; a later slot's sidechain leads by the upstream latency on top);
  - `ChainReport::noise` and `Chain::aec_stats` are read from the extensions.
- **The engine builds its chain with `nf_stages::build_chain`**, the constructor the offline tools use.
- **Unchanged:** the engine's behaviour, latency, budget and readout. The output is bit-identical to the tree
  before (`crates/nf-stages/tests/refactor_parity.rs`, 20 s of toggled scenes with and without AEC3: outputs,
  reports, meters and latencies).
- **Deviations from CLAP that the `.clap` export must adapt** (also in the host profile):
  - the host chooses the rate and any frame count in `[min, max]`, but our stages take only 48 kHz in 480-frame
    blocks: the wrapper buffers host blocks into 480 (reuse `nf_core::dsp::reblock`), adds that delay to its reported
    latency, and resamples or refuses other rates;
  - `activate` runs on the processing thread (easyfft's per-thread warm-up): a foreign host's first block would
    allocate;
  - AEC3's `reset` keeps the converged echo path and delay estimate, where CLAP's clears all processing state;
  - the gain adjustment is measured over the block (output against latency-aligned input), not the last sample, and
    is NaN before the first block where the draft says zero (map it to 0 dB); the draft id
    `clap.gain-adjustment-metering/0` may change;
  - `start/stop_processing`, `deactivate`, process status, `steady_time` and events aren't modelled: no-ops or
    constants in the export.
- **Provisional:** the namespace `noise-filter` (ids `noise-filter.*`, tags `noise-filter:*`) is one macro,
  `nf_core::namespaced!`. Nothing persists the ids yet.
- **Two paths write the noise params:** `ChainControls::apply` (the GUI and the config, from any thread, unclamped:
  tests use a -200 dB floor) and `set_param` (clamped, by the stage's holder). Harmless while nothing in the app calls
  `set_param`; whether the GUI writes through params is TODO 15's call.
- **Params of a stage's own** (an effect, a pack's `controls[]`) have no path from the GUI yet. Our proposal: a
  lock-free single-producer queue of (slot, id, value) that `Chain::process` drains into each slot's `set_param`
  before running it, built with TODO 15 or the first effect.

## Next
1. Freeze the namespace (reverse-DNS) and the ids before anything is published. Recheck the CLAP facts the code
   cites (the param flag bits, `IS_ENUM` requiring `IS_STEPPED`, the standard feature tags, the gain-adjustment
   draft) against the headers then.
2. The `.clap` export of our stages (clack-plugin, or nih-plug's ISC export), adapting the deviations above.
3. clack-host in an fx-host process for third-party plugins, with the profile's gui, log and thread-check.
4. VST3 hosting later.
