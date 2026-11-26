# ACX-LL-Cable

ACX-LL-Cable is a low-latency virtual audio cable for Windows. Each cable has a Speakers playback endpoint and one or two microphone endpoints. The companion app adds echo cancellation, RNNoise, a gate and an optional CLAP effect.

## Try a plain cable

With Cable A installed, select **Speakers (ACX-LL-Cable A)** as the output of a player and **Microphone (ACX-LL-Cable A)** as the input of a recorder. Play a clip and record it. **Call Microphone (ACX-LL-Cable A)** carries the same signal and is intended for call apps. You can leave the filter app closed and keep your Windows default devices as they are.

## Try filtering without a physical mic

The development box has A and B installed. This route uses B as the source and A as the filtered output. A speech clip with some background noise makes RNNoise easy to hear.

1. Set your player's output to **Speakers (ACX-LL-Cable B)**.
2. Open `target\v3\release\noise-filter.exe`. Under **Devices**, select **Microphone (ACX-LL-Cable B)** for **Microphone** and **Direct to driver** for **Output route**. If **Allow direct output…** appears, use it to approve the helper for this app session.
3. Leave **Echo cancellation (WebRTC AEC3)** off for this clip. Enable **Noise reduction**, select RNNoise if a model choice is shown, and press **Start**.
4. Select **Microphone (ACX-LL-Cable A)** in your recorder, play the clip and watch the input and output meters. Record the same passage with RNNoise on and off to hear the difference. **Stop** releases the devices. Minimising the window keeps the filter running.

For a first signal check without a player or recorder, start the route above and expand **Measure latency**. In **Virtual cables** mode, set **Play probe on** to B's Speakers and **Capture app output from** to A's Microphone. **Run test** temporarily puts processing in passthrough while it sends and records its sweeps. Use the speech clip to hear the filter.

The app runs one processing chain with one destination today. Direct output feeds A when A is present, or the only installed cable. To filter into B instead, choose a different input, set **Output route** to **WASAPI playback device**, and select **Speakers (ACX-LL-Cable B)** under **Output (virtual cable)**. Independent cables can carry plain audio at the same time. Splitting one capture into multiple independently processed destinations is planned.

To try the gate on its own, turn off echo cancellation and the noise filter. Choose **Static** under **Noise gate**, then open **Gate settings** to set its threshold or move it within the pipeline. **Processing presets** saves and recalls the stage choices, gate position and external effect parameters. Loading a preset keeps the current microphone, echo reference and output cable.

## Build the app from a checkout

On a configured Windows development machine, run these PowerShell commands from the repository root. They build the settings app, console launcher and sibling effect host.

```powershell
cargo build --release --target-dir target/v3 -p nf-app -p nf-fx
.\target\v3\release\noise-filter.exe
```

For cable setup on a development machine, see the [driver guide](driver/README.md#building-and-installing-an-elevated-windows-powershell). To try the bundled example gain effect or a compatible third-party plugin, see [Running an external effect](doc/todo/17-post-effects.md#running-an-external-effect).
