# 15-16. Settings window, tray and engine process

Facts: `doc/research-2026-10.md` (GUI). Decided by the owner on 2026-10-26:
- Windows 11 only, and the Windows App Runtime is acceptable.
- For now the window just minimises. Later the window closes and a tray icon reopens or starts it
  (the engine's Win32 tray, #16, phase 3).

## Current implementation and acceptance

The owner asked to compare visible routing and ordered stage editors on 2026-11-26. The interactive previews run in the
same Reactor library and share the same example graph. Selection of the product layout is open.
[Designs and launch instructions](../design/gui.md#routing-and-effects-design-review).

`nf-gui` hosts the engine in the settings process. The window controls direct output to the driver or output to a WASAPI
playback endpoint. A stopped filter allows the primary cable to carry ordinary Windows playback. ACX-LL-Cable B has its
own Speakers and microphone endpoints for another route. The output, microphone and echo-reference choices, processing
controls, effect settings and UI preferences persist in `%APPDATA%\noise-filter\settings.json`.

The CLAP card accepts a file selected through the Windows picker. Starting the engine loads the effect in a separate worker
and exposes its writable numeric parameters. The card supports bypass and removal. Worker failure and late blocks are
reported with the dry-audio fallback. An ordinary user can request direct output with **Allow direct output**. That grants a
ring writer for the current app session through a dedicated helper. WASAPI output needs no driver grant.

On the installed v2 dev driver, the broker test authenticated a separate process, attached two fresh rings in succession,
rejected a second simultaneous START, and verified each STOP cleared the attached state. Closing the parent pipe ended the
helper. These processes were already elevated. Approval from a standard account, including a different administrator's
credentials, still needs its own run.

On 2026-11-24, Windows UI Automation exercised the release build on the dev box's private desktop with isolated settings
and stopped audio. Start, the AEC/noise/gate controls, meters and the latency readout were present. The window restored a
stored direct route and CLAP selection, removed the effect, minimised and restored, closed and reopened with the direct route
intact and the effect removed.

A second UI run started the Clack gain example (gain `0.25`) on a virtual mic, direct A and a shared A recorder. It
displayed the active effect, zero dropouts and zero real-time heap allocations. Across 2.6 seconds minimised the same worker
advanced by 15.5 million CPU cycles. The window restored, switched bypass on and off, stopped and closed. Physical Windows 11
acceptance remains open.

## Planned tray and process split (TODO 16)
- **Engine process (always on):** plain Win32 plus the windows crate. It owns the audio streams,
  DSP, settings and the tray icon (Shell_NotifyIcon): open settings, pause, quit. It never loads
  WinUI.
- **Settings process (on demand):** the same exe with `--settings`, running a Reactor window.
  - Why a separate process: Reactor's `App::run` returns only when every window has closed, and
    it's unknown whether WinUI's Application.Start can run twice in one process.
  - Closing the window ends this process; the engine keeps running.
- **IPC:** a named pipe. The engine sends state (devices, meter values, latency readout, AEC
  stats); the window sends settings changes.

## Window design
- **Device pickers (ComboBox):** mic, output cable, echo reference (speakers). The reference picker
  excludes the cable.
- **Checkboxes or ToggleSwitches:** echo cancel ("using speakers"), noise filter. Off means
  bypassed: with the noise filter off, the model isn't run and its delay is removed, so latency
  drops to the I/O minimum. A brief delay jump on toggle is acceptable.
- **Floor and gate:** mode (static or auto), threshold, attack/release (Slider/NumberBox).
- **Model picker:** from the installed model packs, showing each pack's latency and band.
- **Readouts:** meter (#12) and latency readout (#13 layer 1), each with a toggle.
- **The Lab page (#14).**

## Reactor notes (0.100.0)
- **Feeding the window:** `LocalSender` isn't Send, so use tmr-gui's re-armed `spawn_background`
  waiter, with values arriving over the pipe.
- **Meter redraws:** every message re-renders and diffs the whole view, so throttle the meter to
  about 20 Hz.
  - The engine stops sending meter values when the settings process disconnects from the pipe
    (window closed).
  - Pausing while minimised has no Reactor 0.100.0 event. Check whether `on_window_size` reports a
    minimise, or find the HWND (EnumThreadWindows) and watch IsIconic; otherwise rely on the meter
    toggle.
- **Lifecycle:** no close-cancel, no HWND, no start-hidden. Revisit when Reactor's next release
  lands.
- **Pinning:** pin `=0.100.0`; master has been rewritten since.

## Future background settings and autostart
- **Settings:** move persistence to the engine when it runs in its own process.
- **Autostart:** start with Windows via an HKCU `...\CurrentVersion\Run` entry (command line of 260
  characters or fewer).
