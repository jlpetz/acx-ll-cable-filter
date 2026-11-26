# GUI spec (nf-gui, v0)

A windows-reactor 0.100.0 window (patterns: `TMR-APP/crates/tmr-gui`, and
`doc/dev/api/windows-reactor.md`). Single process for v0: closing the window exits. The owner said
minimise for now; the tray comes later (TODO #15, #16).

## Routing and effects design review

The owner wants visible split and mix controls and a single ordered list for built-ins and CLAP effects. Three native
Reactor mockups are available for choosing the layout. The current settings window is documented below.
[Compare the concepts](routing-preview.png). Run `cargo run -p nf-gui --example routing_preview` to try them.

All three layouts edit one in-memory example. **Hardware mic** represents a physical microphone. It feeds **Cable A Output**
dry for OBS and **Cable B Output** through AEC and RNNoise for Discord. Audio played into **Cable C Input** joins the mic
on A at −9 dB. To try a two-cable mix, turn on **Cable D Input → Cable A Output** and turn off **Hardware mic → Cable A Output**.
Both C and D now feed A. B keeps its own mic chain. The meters, devices and plugin catalog are examples.

**Input** and **Output** name the two ends from the cable's perspective. Windows currently lists the playback end as **Speakers**
and its recording ends as **Microphone** (low latency) and **Call Microphone** (communications). Both recording ends carry the
same cable signal. A preview destination represents that signal. The recording app chooses the endpoint. **Source** and
**destination** describe a pipeline connection, so a cable output used downstream keeps its Output name.

- **Signal paths** shows where each source goes. Choose **Edit sends** on the source to connect another destination.
- **Routing matrix** shows all source/destination pairs. Each active cell in a row is a copy, and each active cell in a column contributes to the mix.
- **Mixer strips** puts the contributing sources and a stereo meter on each output.

Select a source to edit its shared stages, a send to process one copy, or an output for effects after the mix. **Add stage**
opens the same catalog at each location. Select a stage to adjust it, move it earlier or later, bypass it or remove it.
Open **Send** to pick the original input, the output of a particular shared stage, or the end of the shared pipeline, and to set its gain.
**Reuse this pipeline** copies stage instances and their settings onto a different route for the duration of the preview.

The proposed first view is Signal paths, with the matrix available for larger setups. The owner is choosing the layout.
Live split and mix depend on the engine work in [TODO 24](../todo/24-routing-graph.md) and multiple live effect instances
in [TODO 17](../todo/17-post-effects.md). The product's existing plain-cable use remains available with the app closed.
Voicemeeter Banana's per-input sends and separate output meters informed the matrix and strips.
[VB-Audio's routing diagram, p. 10](https://vb-audio.com/Voicemeeter/VoicemeeterBanana_UserManual.pdf#page=10).

## Layout (top to bottom, one column, about 520×760 by default, scrollable)
1. **Header:**
   - title "noise-filter";
   - a status line showing the filter state. A timed retry reads "Waiting to retry". A fault that
     needs a device or routing change reads "Waiting for a device or settings change". The notice gives the cause and remedy.
   - a Start/Stop button (one button that toggles). While the engine retries it offers Stop,
     which disarms the retry.
   - Two InfoBars under it:
     - **the notice:** errors and results (a start that failed, a fault that stopped a run, once
       per fault with what happens next, the large-page setup's result, a settings file that
       couldn't be read);
     - **the warnings bar:** the engine's standing warnings while it runs, one per line (a mic on
       the output cable, the output being the default playback device, which also says it runs
       shared until that's fixed, a lost echo reference, MMCSS), then its notes
       (`EngineStatus::notes`: a stream asked for in exclusive mode that runs shared, with why, the
       cost, the fix and where to turn the switch off). Notes alone make it informational ("Running
       with a note"), not a warning: exclusive mode is the default, so a refusal is ordinary. It
       closes by itself when they clear.
2. **Devices** (each a ComboBox with a header):
   - **Microphone:** capture endpoints, plus "Default microphone".
   - **Output (virtual cable):** render endpoints; no default. The placeholder reads "Pick the
     cable your apps use as their mic".
   - **Echo reference (speakers):** render endpoints minus the chosen output, plus "Default
     speakers".
   - A "Refresh devices" button.
3. **Processing:**
   - Noise reduction can be bypassed. The control names the sole available model or offers a selector when there are alternatives.
   - Echo cancellation enables WebRTC AEC3 with the chosen speakers as its reference.
   - Slider "Max suppression": -60..0 dB. The label shows "unlimited" at -60.
   - **Gate:**
     - Off / Static / Auto. The gate works independently with noise reduction bypassed.
     - The Gate settings panel places it before echo cancellation, before noise reduction, after noise reduction or after the external effect.
     - Slider "Threshold (dBFS)", -80..-10 (Static), or "Margin above noise (dB)", 0..20 (Auto);
     - Sliders for Attack (0.5..50 ms), Release (20..1000 ms) and Hold (0..500 ms);
     - Slider "Range (dB)", -80..0;
     - "Hold open on speech" can use an adjacent noise model's speech estimate. It defaults off.
   - Processing presets save the stages, gate placement and external effect parameters. Loading a preset preserves the current devices,
     output route and monitoring settings.
4. **Meters** (ToggleSwitch "Show meters"; updated at 20 Hz when on):
   - **Input:** a horizontal bar of Borders. The RMS fill colours green / yellow (> -12 dBFS) /
     red (> -3); a thin peak marker; the numbers alongside; a "CLIP" badge that latches red. Click
     the badge to call `engine.reset_clip()`. A "near clip" hint.
   - **Output:** the same bar with noise reduction, speech probability when available, and measured gate attenuation when available.
     Silence has no measurable input-to-output ratio. Its reduction readout shows "—".
5. **Latency** (ToggleSwitch "Live readout"):
   - Line 1: "Pipeline: 12.3 ms (p95 14.1)", or "—" when invalid; under it its budget, "Budget (means): mic 16.2 + wait
     0.1 + output queue 22.0 + render 23.4 ms, chain 10.0 ms on top" (engine.md, "The latency budget"); then "Drift
     out/ref: +12/-40 ppm · Fill out/ref: 15.0/12.0 ms (target 15.5/12.0)".
   - Line 2: "CPU per block: p50 45 µs, p99 80 µs (AEC p99 x, noise p99 y)"; then "Dropouts:
     capture n, output n, reference gaps n, mic stalls n · Coarse drops out/ref: n/n · Tap
     overflows: n" (a coarse drop is one correction that dropped whole blocks); then "RT heap
     allocations: n (must stay 0) · Pool allocations: n/s" (`rt_heap_allocations`,
     `rt_pool_allocations_per_s`).
   - Note: "Excludes the cable's own buffer and the receiving app".
6. **Measure latency** (an Expander or section):
   - ComboBox Mode: "Virtual cables" / "Speakers and mic".
   - ComboBox "Play probe on": render endpoints.
   - ComboBox "Capture app output from": capture endpoints.
   - NumberBox Repetitions (1..20, default 5).
   - Button "Run test": disabled unless the engine is running. While running it reads "Cancel",
     with a ProgressBar.
   - The result: app (input tap → cable out) median ± spread; with sweeps the engine's own events
     disturbed, the undisturbed ones' median ± spread, a line when they span an event, and a line
     for each disturbed one, saying why (TODO 13; "Disturbed sweeps: none seen" otherwise); input
     path; end to end; chain; the mic's and the output's events; confidence; warnings; conditions. A "Copy" button is optional (skip if Reactor has no
     clipboard).
   - **Help text, virtual mode:** "Set Microphone to CABLE-A Output. Play the probe on CABLE-A
     Input. Capture CABLE Output (the cable set as Output above). Needs two virtual cables
     (e.g. VB-CABLE plus CABLE-A, or two Voicemeeter inputs)."
   - **Help text, acoustic mode:** "Play the probe on your speakers, with the mic as Microphone.
     Quiet room, normal volume."
7. **Diagnostics** (collapsed by default):
   - memory (`status.memory`), MMCSS, RAW mic in effect;
   - each stream's format and period; chain latency;
   - **Latency** (the owner, 2026-11-02: on by default, the lowest latency; some will turn one
     off): ToggleSwitches, each with a dimmed line on what it does, costs and when to turn it off
     (`settings::IoSwitch`; changing one restarts the streams):
     - "Exclusive output": "Lowest latency (20-30 ms less on VB-CABLE): the filter plays straight
       into the cable's driver. Only the filter plays into the cable, and your apps still record
       its other side as usual. Turn off if the cable misbehaves."
     - "Exclusive microphone": "Lowest latency. While the filter runs, other apps can't open your
       microphone directly; they use the filtered mic. Turn off if you record your raw microphone
       in another app. With echo cancelling on, if echo gets through or your voice crackles, try
       turning this off." (its other two costs, the longer echo floor and the one-period buffer,
       as what to try.)
     - "Adaptive output buffer": "Trims the output buffer to what your PC needs after four minutes
       of running (about 7 ms less on VB-CABLE). A rare hiccup can then cost a short glitch, three
       a run at most. Turn off for a recording that must have none."
     - under them: "A device that refuses exclusive mode runs in shared mode, and a note at the top
       says why and how to fix it." The stream lines above already say each stream's mode.
     - With both exclusive by default, a CLI `run`, `latency` or `quality` on the window's mic or
       cable can't open them while the window's filter runs (and the window's can't while one of
       those runs): the start error says the device is held in exclusive mode, maybe by another
       copy of the filter.
   - **Streams:** ToggleSwitches "Low-latency periods (IAudioClient3)", "RAW mic" (both with "For the microphone in shared
     mode (with the exclusive microphone off, or refused)." under them) and "Keep reference alive";
   - Slider "Output margin (ms)", 0..20;
   - a Button "Enable large pages…", which runs `noise-filter.exe setup-large-pages` elevated
     through `nf_mem::privilege::run_elevated` and then says "Sign out and back in to apply".

## Behaviour
- **One window:** a second launch brings the running window forward instead of opening another
  (Windows may only flash its taskbar button when the launch comes from a terminal).
- **Window size:** the remembered size is fitted to the primary monitor's work area at start-up.
  A side that fills the work area (maximised, snapped, or the start-up height cut to the screen)
  isn't remembered; the other side is.
- **Settings file:** `%APPDATA%\noise-filter\settings.json`, holding `EngineConfig` and the UI's own
  fields (latency-test devices and mode, repetitions, meter and readout toggles, window size).
  - Load at start; on error, back the file up as `.bad` and use the defaults. A file that can't be
    read nor backed up (locked, or on a share or drive that isn't there yet) is left alone, and
    the session doesn't save (a warning says so; restart once it's readable). A missing folder is
    a first run only if AppData itself is there.
  - A `"version"` (`SETTINGS_VERSION`, 1 from the automatic model depth on; a file without one is 0)
    tells a value whose meaning changed: a file from before 1 with `processing.model_depth` 2 (the
    default the window saved, which it never sets) loads as automatic (`settings::upgrade_engine`,
    which the CLI's `--config` uses too).
  - Save on every change, debounced to 500 ms, through a background save. Never block the UI
    thread on disk if Reactor makes that awkward; small synchronous writes are acceptable.
- **Applying:** every control change calls `engine.apply(&config)` (cheap). Device and I/O changes
  restart the streams inside the engine.
- **Start:** Start calls `engine.start(&config)`, which can block up to a few seconds, so run it in
  `spawn_background` and show "Starting…". On error, show the message in an InfoBar.
- **Data feed:** the telemetry and status refresh comes from a re-armed background waiter at 20 Hz
  (meters or readout visible) or 2 Hz (neither visible), as tmr-gui does it. Its messages carry
  `TelemetrySnapshot` and `EngineStatus` (`Clone`).
- **Latency test:** polled with the same waiter while it runs.
- **Device lists:** loaded at start and on "Refresh" (`nf_engine::list_devices`, in the
  background). Show the friendly name, and keep the id in a parallel Vec.
- **Missing device:** a selected device that's missing from the list shows as "(missing) <id>"
  and is kept in the config.
- **Errors:** never panic on engine errors. Show them in the InfoBar.

## Not in v0
Tray, close-to-tray, start with Windows, the Lab page, the model picker (only RNNoise exists),
localisation.
