# TODO 23: pre-release driver validation

The owner agreed on 2026-11-23 that the driver gets a dedicated validation phase before release: Driver Verifier rounds with
the flags beyond `/standard`, and Microsoft's HLK tests. The scope and order below are ours, to refine with the owner.

## Driver Verifier beyond /standard
`/standard` (flags 0x001209bb, the KMDF verifier included) stays on during development (the owner, 2026-11-23). These go in
dedicated rounds, one set at a time, because they inject failures and delays on purpose:

| Flag | What it does | Why it matters for this driver | Priority |
|---|---|---|---|
| 0x00800000 Kernel synchronization delay fuzzing | Random delays around locks and thread switches | The app ring, its copy and the loopback ring are shared between the timer DPC and passive-level requests under spin locks: races there are the likeliest bugs to slip through | High |
| 0x00040000 Systematic low resources simulation (or 0x00000004 randomized) | Fails the driver's allocations, a call site at a time | Every error path: stream creation, ATTACH, the device's start | High |
| 0x02000000 Code integrity checks | Checks the image's memory protections | Planned for the owner's next reboot | High |
| 0x00080000 DDI compliance checking (additional) | More DDI rules | Cheap; can raise false positives | Medium |
| 0x00000200 Force pending I/O and 0x00004000 Invariant MDL checking for the driver | Stress asynchronous requests and mapped buffers | The control device holds a pending ATTACH and maps the app's ring through an MDL | Medium |
| 0x00008000 Power framework delay fuzzing | Delays power transitions | Sleep and resume | Medium |

Not for us: NDIS/WiFi, VM switch, port/miniport checking; IRP logging is a diagnostic.

## HLK and other tools
- The HLK's Device Fundamentals tests, runnable without the full HLK controller: PnP (disable, enable, remove, surprise
  removal), sleep, hibernate and reboot cycles, concurrent I/O stress, and the penetration tests, which fuzz the control
  device's IOCTLs. The last matters most here: the driver takes memory and requests from user mode.
- The HLK audio tests that apply to a virtual endpoint.
- KMDF's own verifier (WDF Verifier: handle tracking, fault injection).
- CodeQL with Microsoft's Windows driver queries (what driver certification runs); ApiValidator on for release builds
  (`Build-Driver.ps1` turns it off today); InfVerif (already run by every build).
- Physical Windows 11 machines (the owner's).
- The per-cable driver's control devices (TODO 19, Astra's review, 2026-11-25): removing one cable deletes its KMDF
  control device while other cables' devices live, which departs from Learn's documented deletion order ("Using Control
  Device Objects"). A pass of the removal runs under Verifier shows this build's behaviour, not that it is supported:
  before release, confirm the lifetime arrangement (WDF's source, or Microsoft) or move to the design's fallback, one
  shared control device routing requests by cable id (`19-per-cable-design.md`).

## Dev-box rules that carry over
- Commit and `sync` before every install; never install over a build with a removal bug while Verifier is on.
- Never stop a process with Git Bash's `kill` or `pkill`, nor `taskkill /T` (2026-11-23: a bash kill ended services.exe's
  tree and bugchecked the box, 0xEF): stop test processes by image name.
