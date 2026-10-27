# 15-16. Settings window, tray and engine process

Facts: `doc/research-2026-10.md` (GUI). Decided by the owner on 2026-10-26:
- Windows 11 only, and the Windows App Runtime is acceptable.
- For now the window just minimises. Later the window closes and a tray icon reopens or starts it
  (the engine's Win32 tray, #16, phase 3).

## Processes
- **Engine process (always on):** plain Win32 plus the windows crate. It owns the audio streams,
  DSP, settings and the tray icon (Shell_NotifyIcon): open settings, pause, quit. It never loads
  WinUI.
- **Settings process (on demand):** the same exe with `--settings`, running a Reactor window.
  - Why a separate process: Reactor's `App::run` returns only when every window has closed, and
    it's unknown whether WinUI's Application.Start can run twice in one process.
  - Closing the window ends this process; the engine keeps running.
- **IPC:** a named pipe. The engine sends state (devices, meter values, latency readout, AEC
  stats); the window sends settings changes.

## Window contents
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

## Settings and autostart
- **Settings:** persisted by the engine (e.g. a TOML file in `%APPDATA%`).
- **Autostart:** start with Windows via an HKCU `...\CurrentVersion\Run` entry (command line of 260
  characters or fewer).
