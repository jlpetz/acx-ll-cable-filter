# ACX-LL-Cable

ACX-LL-Cable is a low-latency virtual audio cable for Windows. Each cable has one playback input and one or two recording outputs. The optional mixer captures a source once, splits or mixes stereo audio, and applies effects on the routes you choose.

## Try a plain cable

With Cable A installed, select **LLCable-A In** as the output of a player and **LLCable-A Out** as the input of a recorder. Play a clip and record it. **LLCable-A Out-5ms** carries the same signal with a 5 ms default engine period for apps that use Windows' default timing. The cable also works with the filter app closed. Some pickers append **(ACX-LL-Cable A)**. Older installations may show **Speakers**, **Microphone** and **Call Microphone** respectively.

In Windows Sound, a blue arrow points **into** the playback socket and an amber arrow points **out of** the recording plug. See the [Playback tab](doc/design/llcable-windows-playback.png) and [Recording tab](doc/design/llcable-windows-recording.png). After upgrading an older installation, `driver\scripts\Update-CableAppearance.ps1 -Label A -WhatIf` previews a refresh of its factory names and icons. Omit `-WhatIf` in an elevated shell to apply it.

## Try the live mixer

Open `target\v3\release\llcable-mixer.exe`. **Cables** lists installed cables. Turn on **Monitor cables** to read driver statistics once a second while viewing the panel. Monitoring starts off. **Reset cable counters** starts a new count of underrun frames and missing playback packets from the next reading. A red `!` marks a loss since that reset. The **Device total** column keeps the history since the cable device started. Live route counters start over each time you start audio.

For playback and recording in other applications, choose **LLCable-A In** and **LLCable-A Out** and leave the mixer stopped.

Open **Mixer** and select the destination you want to hear. Connect a source, then select **Start audio**. To try this with two virtual cables, play a clip to **LLCable-A In**, connect **LLCable-A Out** to destination **LLCable-B Out**, and record **LLCable-B Out**. Use **Output effects** to add RNNoise to B. Use a physical microphone or a third virtual source to feed both A and B, leaving A dry. Each send can take the original source, its output after a particular stage, or the result of its whole rack. **Wiring** shows the selected point. Playback devices can also serve as destinations.

Source, send and destination racks accept ordered stages. AEC uses a selected reference, ducking lowers one signal in response to another, and a gate can close a branch independently of the denoiser. The stereo EQ has separate low, mid and high gains and works at the incoming packet size. RNNoise, Sonora suppression and the isolated CLAP voice host operate in mono on their own branches. An untouched stereo send preserves left and right. Edits take effect with **Apply changes**, which briefly restarts routing. **Stop audio** releases the devices.

## Try the single-chain settings app

The development box has A and B installed. This route uses B as the source and A as the filtered output. A speech clip with some background noise makes RNNoise easy to hear.

1. Set your player's output to **LLCable-B In**.
2. Open `target\v3\release\noise-filter.exe`. Under **Devices**, select **LLCable-B Out** for **Microphone** and **Direct to driver** for **Output route**. If **Allow direct output…** appears, use it to approve the helper for this app session.
3. Leave **Echo cancellation (WebRTC AEC3)** off for this clip. Enable **Noise reduction**, select RNNoise if a model choice is shown, and press **Start**.
4. Select **LLCable-A Out** in your recorder, play the clip and watch the input and output meters. Record the same passage with RNNoise on and off to hear the difference. **Stop** releases the devices. Minimising the window keeps the filter running.

For a first signal check without a player or recorder, start the route above and expand **Measure latency**. In **Virtual cables** mode, set **Play probe on** to **LLCable-B In** and **Capture app output from** to **LLCable-A Out**. **Run test** temporarily puts processing in passthrough while it sends and records its sweeps. Use the speech clip to hear the filter.

This window runs one voice chain. **Direct cable** selects any installed cable. Automatic selects A, or the only installed cable. The CLI accepts the same choice with `--direct-cable B`. Choosing a specific cable keeps its saved identity through a Windows endpoint change. Other cables can carry plain audio while this chain runs.

To try the gate on its own, turn off echo cancellation and the noise filter. Choose **Static** under **Noise gate**, then open **Gate settings** to set its threshold or move it within the pipeline. **Processing presets** saves and recalls the stage choices, gate position and external effect parameters. Loading a preset keeps the current microphone, echo reference and output cable.

## Try the design prototype with simulated audio

```powershell
cargo run -p nf-gui --example mixer_v2
```

The example mic feeds **LLCable-A Out** dry for OBS and **LLCable-B Out** after RNNoise for Discord. B opens selected. Click **+ LLCable-C In** and open **Wiring** to see C join the filtered mic. Click a wire and choose **Disconnect** to remove just that send. Each effect appears in processing order. **All sends** in the mixer shows every connection together.

Click the selected output again for a meters-only view. Right-click an output or use **…** for its diagnostics and effects. Compact spacing and blue low-level bands are the initial defaults. **Appearance** saves your choices. Routes and effects last for the window session. Switch **Demo signal** off to see digital silence.

**Diagnostics** shows the denoiser and gate stages reached by the selected mix, their settings, and simulated levels. **Open live settings** takes you to measured reduction, gate attenuation, speech probability and latency sweeps on the actual route.

## Build the app from a checkout

On a configured Windows development machine, build the mixer, single-chain app and companion executables together. `llcable-status.exe` reads driver telemetry from a terminal. Keep `llcable-broker.exe` and `nf-fx-host.exe` beside the apps for session-approved direct feeds and isolated plug-ins.

```powershell
cargo build --release --target-dir target/v3
.\target\v3\release\llcable-mixer.exe
```

For cable setup on a development machine, see the [driver guide](driver/README.md#building-and-installing-an-elevated-windows-powershell). To try the bundled example gain effect or a compatible third-party plugin, see [Running an external effect](doc/todo/17-post-effects.md#running-an-external-effect).

## Embed in another app

| Component | Use it for |
| --- | --- |
| `driver/` and `acx-ll-cable` | Install and manage independent cable instances. Normal WASAPI clients can use each cable directly. |
| `llcable-sdk` | Discover cables and roles, read aggregate diagnostics, and attach a direct stereo writer to a selected cable. |
| `llcable-routing` | Build mono/stereo packet graphs, split dry and processed signals, mix, pan and duck in a host of your choice. |
| `llcable-audio` and `llcable-wasapi` | Run the graph against native Windows devices and per-cable rings. |
| `llcable-effects` | Add native voice effects. Enable its `clap` feature for the isolated external voice-effect host. |
| `llcable-control` | Build a local control panel. Enable `voice` to embed the single-chain engine and its latency tools. |

Start with the [standalone client example](examples/independent-client/src/main.rs) or the [live routing example](crates/llcable-audio/examples/route.rs). A dry client can depend on the SDK and routing alone. The first-party Rust crates are MIT or Apache-2.0. The optional native denoisers reuse `nnnoiseless` and Sonora with their respective upstream notices. The repository's [vendor notes](vendor/README.md) describe the Sonora patch.

If you import `llcable-effects` or the legacy voice engine into another Cargo workspace, copy the root manifest's `[patch.crates-io]` entry for Sonora and adjust its path to this checkout. Cargo applies patches from the consuming workspace root. The SDK, audio host and routing graph work on their own without this patch.
