# noise-filter's virtual audio driver (TODO 19)

Our own root-enumerated virtual mic (option D), started from Microsoft's ACX **AudioCodec** sample
(https://github.com/microsoft/Windows-driver-samples, `audio/Acx/Samples`, commit d49bb7d, 2026-11-05), and cut down from
there (doc/todo/19-driver-base-2026-10-28.md has the plan and the spike's checks).

**Licence.** The files under `src/` derive from the sample, which is MS-PL (`LICENSE-MS-PL.txt`): they stay MS-PL (its
section 3(D)). The scripts are ours, under the repo's licence. `src/Common/PacketSize.{h,cpp}` are ported from
microsoft/low-latency-audio (MIT, `LICENSE-MIT-low-latency-audio.txt`) and keep its Microsoft and Yamaha notices.

**What changed from the sample** (the first commit after the import shows it): KMDF 1.35 (Windows 11 24H2 / Server 2025,
build 26100, and up: the owner, 2026-11-20) in both projects, and the INF's OS floor 10.0...26100 to match. ACX stays 1.1,
the newest WDK 28000 has.

Packet sizes (spike check 2): both circuits publish `DEVPKEY_KsAudio_PacketSize_Constraints2` on their KSCATEGORY_AUDIO
interface right after `AcxDeviceAddCircuit`: a 2 ms floor, 128-frame RAW and DEFAULT packets, no alignment, no maximum.
A failure to publish is reported (DbgPrintEx) and leaves the default 10 ms periods; it doesn't fail the device.

The loopback pair (spike check 7): what's played into the speaker endpoint comes out of the microphone endpoint. A 768 KB
non-paged ring on the device, under a spin lock; both streams on one QPC time line (the stream engine's own schedule), so
the two ends never drift and need no fill control. The render pass copies the next packet once the client has released it
(SetRenderPacket), else the one completing; the capture pass reads one packet a fixed 1 ms lag behind. Both endpoints offer
48 kHz stereo with float32 first and signed 24-in-32 PCM second. On the Server 2025 spike Windows stored float32 on both
endpoints and the filter opened exclusive float output. Float32 alone left both interfaces disabled. The loopback copies
matching formats and emits silence if the two streams negotiate different formats. The [format validation checklist](../doc/todo/19-spike-2026-11-20.md#format-validation)
covers the integer fallback and Windows 11. The sample's tone, WAV reader and
SaveData are no longer called. Two of the sample's stream-engine bugs are fixed: UpdatePosition subtracted the glitch
adjustment (time) from a byte position, and let the position pass the packet in progress (a Pause could then stall the
next Run's timer for about 111 days); and the first capture packet of a run reported a QPC of 0.

## Building and installing (an elevated Windows PowerShell)

1. `scripts\Setup-TestSigning.ps1 [-Ems]`: a test certificate (`CN=Noise Filter Test Driver`), trusted, and test signing
   on (Secure Boot must be off). Reboot once afterwards. `-Ems` would also turn on Emergency Management Services for the
   EC2 serial console; the spike box runs without it (the owner: as few changes as possible). Ported from TMR-MD's
   build.ps1.
2. `scripts\Build-Driver.ps1`: MSBuild, then the package in `out\Release\AudioCodec`: the `.sys` signed, the catalog made
   by inf2cat for `10_GE_X64,Server2025_X64` and signed.
3. `scripts\Install-Driver.ps1`: makes the `ROOT\AudioCodec` device with devcon (or updates the driver on it).
4. `scripts\Get-DriverStatus.ps1`, `scripts\Uninstall-Driver.ps1`.
5. An install can make the new endpoints Windows' defaults: `scripts\Set-DefaultAudio.ps1 -Playback ID -Recording ID` puts
   them back (`noise-filter devices` lists the ids). `scripts\Get-EndpointFormat.ps1` shows the device format Windows
   stored for each endpoint.

Needs Visual Studio 2026 (C++ and the WDK extension) and WDK/SDK 10.0.28000. Two WDK checks are switched off in the build
because their x86 tools fail under VS 2026's 64-bit MSBuild (InfVerif looks for `x86\InfVerif.dll`, ApiValidator's
`aitstatic.exe` exits 193); InfVerif runs from the WDK's x64 tools instead. From Git Bash, start PowerShell with
`env -u PSModulePath powershell -File ...`: Git Bash's PSModulePath lists PowerShell 7's modules first, which leaves
Windows PowerShell without its `Cert:` drive.
