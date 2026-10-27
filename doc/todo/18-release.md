# 18. Packaging, signing, licences and the user setup guide

## Packaging
- **Target:** Windows 11 (22000+), the owner's decision.
- **Windows App Runtime 2.x:** for the settings window only; the engine doesn't need it.
  - Minimum 2.4.0.0; current 2.5.1 (2026-09-16), installer 120,199,480 B.
  - It installs per user without UAC (`--quiet`), so an installer can chain it.
  - Learn lists the Visual C++ Redistributable as a prerequisite for unpackaged apps. Check whether
    a Rust/Reactor exe needs it (TMR's tmr.exe is a test case).
  - Self-contained alternative: about 56 MiB of DLLs.
- **Large-page privilege (#6):** the installer runs elevated once and grants `SeLockMemoryPrivilege` to the installing user (`LsaAddAccountRights`, or secedit as TMR does). It takes effect at the next sign-in. Whether the app then runs without admin is unconfirmed (#6): a standard account should keep the right; an admin account under UAC may lose it in its filtered token and get the 4 KB + VirtualLock fallback. Grant to the interactive user's SID, not the elevated process's account. Without it, the app falls back to locked 4 KB pages and says so.
- **Binary size:** ONNX Runtime (if the ort path ships) is the largest part. The GTCRN LADSPA plugin (Linux) self-reports about 6-12 MB with a minimal static build and ~22 MB with the stock download; the Windows size is unmeasured. Hand-written ports (#6) remove it.

## Signing
- **Microsoft Artifact Signing:** takes individual developers only in the US or Canada; organisations in more countries qualify (paid Azure subscription).
- **Otherwise:** an OV certificate (about $200-400 a year, secondary source).
- **Unsigned builds** get SmartScreen warnings, and Defender has deleted similar unsigned audio tools outright (aec-client).

## Licences
- **NOTICE file:** every model's code and weights licence, plus attribution.
  - nnnoiseless/RNNoise: BSD-3.
  - UL-UNAS: MIT; note the probable China-only patent filing (#7).
  - sonora: BSD-3-Clause. Check whether the repo carries WebRTC's PATENTS file, and reproduce it if so.
  - FastEnhancer and DeepFilterNet: see #5 and #7.
- **Training-data notes:** recorded per model pack (#3).

## User setup guide
1. Install VB-Cable (the user's job; the app never installs drivers).
   - Set its latency to about 1440-1536 samples (the default 7168 can add up to 149 ms at 48 kHz).
   - Set CABLE Input and CABLE Output to 48 kHz in Windows Sound settings, to match the app.
   - Run the control panel as administrator. The manual says Internal SR changes persist only then; the latency setting probably does too (unverified; check once on a desktop).
2. Don't make CABLE Input the default playback device. In Discord, Teams or OBS, select CABLE Output as the mic.
3. Turn off the voice app's own noise suppression and echo cancellation (Discord, Teams) while the app does them. They add delay and process the audio twice, and two echo cancellers fight each other.
4. Sound → Communications tab: "Do nothing", to stop other audio being turned down.
5. Avoid Bluetooth headsets for low latency.
