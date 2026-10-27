# 19. Our own virtual mic

Status:
- Option A (our UDE driver) chosen by the owner on 2026-10-26.
- The owner added on 2026-10-26: if B (usbip-win2) tests as good and as low-latency as VB-CABLE,
  maybe use B instead.
- VB-CABLE (option C) stays the v0 path and the A/B baseline.
- The research workflow reported on 2026-10-26 (below). It argues for a fourth option, D (our own
  virtual audio driver), over A and B.
- **The owner chose D on 2026-10-28** as the main goal, built from an existing open-source driver
  and simplified to what we need, with VB-CABLE the comparison for latency and CPU. The owner asked
  whether Windows' UAC2 driver was open source: in-box `usbaudio2.sys` is closed (Thesycon's);
  microsoft/low-latency-audio, the new ACX UAC2 driver plus ASIO, is open (MIT) and still public,
  but unreleased and written around a real USB device.

## The owner's criteria (2026-10-26)
1. Latency and quality at least as good as VB-CABLE.
2. The virtual mic is controllable from inside the app.
3. Open source that others can reuse in their own apps and use cases.

## Options
| Option | Kernel code | Path | Verdict after the research |
|---|---|---|---|
| A. Our UDE driver emulating a UAC2 mic | ours (KMDF + UdeCx) | app → shared memory → emulated USB device → in-box `usbaudio2.sys` | Weaker than thought: UDE lacks QueryBusTime, so USB audio over UDE only works with a filter on every USB 3 root hub (what usbip-win2 does). Still has 1 ms USB framing and the class driver's URB queueing. |
| B. usbip-win2 + a Rust USB/IP UAC2 server in our app | usbip-win2's (BSD-2) | app → localhost TCP → usbip-win2 → class driver | Lab candidate only (below). |
| C. VB-CABLE | VB-Audio's | app → render → cable → capture | v0. Closed source; app control limited to picking the device. |
| **D. Our own virtual audio driver** (proposed) | ours (C++, from Microsoft's MS-PL samples) | app → shared memory → our root-enumerated capture endpoint | No USB at all. Fixed 48 kHz, one clock, small buffers, app-controlled, open source. Signing as A. |

## Research results (2026-10-26; sources in the workflow report)
**B: usbip-win2 and tarekwasfy01/Virtual-Cables**
- Virtual-Cables is about 1.5k lines of Go (BSD-2, "experimental", source uploaded once on 2026-07-28). It serves UAC1 devices over USB/IP to usbip-win2, so Windows' `usbaudio.sys` drives them: 48 kHz/16-bit stereo, a 250 ms ring with no drift correction. Read from the code, not measured: delay can settle at the full 250 ms. Its author's maintained product is a paid Store app.
- usbip-win2 installs `usbip2_ude.sys` plus `usbip2_filter.sys`, an upper filter on every `USB\ROOT_HUB30`, physical hubs included. Installing it restarts all USB 3 hubs (every USB device drops briefly).
- **Stability:** 0.9.7.8 bugchecked (0x139 in `usbaudio!USBType1FreeRequest`, usbip-win2 #181) in exactly this loopback-audio setup (fixed in 0.9.8.0). Open on the latest 0.9.8.1: recording a USB audio capture device over USB/IP gives 0-second files, worse with uptime (usbip-win2 #200; the maintainer will fix it later).
- **Signing:** releases alternate between attestation-signed and WHLK-certified; 0.9.8.1 links a Microsoft certification report.
- **Latency:** not expected to beat VB-CABLE, because of the class driver's URB queueing (about 10 ms per URB per Virtual-Cables' comments), 1 ms framing and user-mode pacing. Expect better formats (float32, 24-bit via UAC2), not lower latency.
- The Rust `usbip` crate (jiegec, MIT) seems to have no isochronous scheduling.

**microsoft/low-latency-audio:** an ACX UAC2 driver for *real* USB interfaces, matched by VID/PID; no public release. It creates no virtual endpoints and doesn't support UAC1 (many gaming headsets and webcam mics). The app needs nothing for it: our IAudioClient3 path picks up smaller periods if a user opts a UAC2 mic in. In-box `usbaudio2.sys` (Thesycon, since 10 1703) is unchanged in 25H2 and 26H2.

**D: building our own virtual audio driver**
- **Starting points (MS-PL):** the ACX AudioCodec sample is root-enumerated and fully simulated (timer-driven stream engine); SimpleAudioSample and SYSVAD are WaveRT virtual devices.
- **Small buffers:** a driver can declare them with `DEVPKEY_KsAudio_PacketSize_Constraints2`. If one app opens a small-buffer stream on an endpoint, every app on that endpoint switches to it, so our app could give Discord a small period too (inference).
- **ACX buffers:** single-packet mode needs a whole page per buffer, 1024 samples at 16-bit stereo; use the two-packet mode for small buffers.
- **Rust:** windows-drivers-rs (KMDF 1.33, "not yet recommended for production use") has UdeCx headers but no ACX or PortCls bindings. ACX's C header could be bindgen'd later. Driver in C++ for v1, everything else in Rust.
- **Estimate (the researcher's, unsourced):** 1-2 weeks to a prototype, 6-10 weeks to a shippable v1, plus 2-6 weeks of signing calendar time.

**Signing (applies to A and D)**
- **Attestation:**
  - needs a Hardware Developer Program account and an EV certificate (about US$349-359 a year; SSL.com sells a sole-proprietor EV it says works for Partner Center);
  - works on Windows 10/11 desktop only, and Microsoft now calls it "for testing purposes only".
- **Server:** Windows Server 2016+ loads only HLK-certified (or old cross-signed) drivers. The Server 2025 dev VM therefore needs test-signed builds of our own driver.
- **OSSign:** free for open-source projects with an OSI licence, active maintenance, a real user base and a public build pipeline. The publisher shown would be OSSign's sponsor, not us.
- **Test signing is out for users:** BattlEye refuses it outright; Vanguard and FACEIT require Secure Boot, which test signing needs off.

**For testing on this dev VM now**
- Whether VB-CABLE loads on Server 2025 is unknown (VB-Audio doesn't say how it's signed). Try it first, then Voicemeeter Banana.
- RDP mic redirection gives a real capture endpoint for functional tests (client: "Record from this computer", `audiocapturemode:i:1`; the VM allows it). Its timing goes through RDP, so it's no use for latency or drift numbers.

## Plan (proposed, awaiting the owner)
1. Done: D is the goal (owner, 2026-10-28). Next, a research pass on the base: what the ACX AudioCodec sample, SimpleAudioSample and SYSVAD give, and what to take from low-latency-audio (ACX low-latency streaming, the ASIO driver's user-kernel interface, the format handling).
2. **Dev box preparation, with the owner:** EBS snapshot, then `bcdedit /set testsigning on`, then a reboot at an agreed time (it ends the Claude session and any running workflows).
3. **D spike:** build the ACX AudioCodec sample (or SimpleAudioSample) as one capture endpoint, "noise-filter Microphone", fed from a shared-memory ring the app writes. Then measure it against VB-CABLE with #13.
4. **The engine's output stage becomes a trait** (WASAPI render for VB-CABLE, or the driver's ring), chosen in the settings and A/B'd with #13 and #14.
5. **Signing:** apply to OSSign once the driver repo has a public build pipeline; an EV certificate plus Partner Center is the fallback.
6. **B (optional):** if usbip-win2 #200 is fixed, a measured comparison on a Windows 11 box.

## Risks
- **A kernel bug bugchecks the machine:** snapshots, Driver Verifier, and never test on the owner's main PC first.
- **OSSign may not accept a new project ("real user base"):** the EV-plus-attestation fallback costs money and an organisation-style registration.
- **Attestation's "for testing purposes only" wording:** watch for Microsoft tightening it; WHQL is the long-term answer.
