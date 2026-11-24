# noise-filter's virtual audio driver (TODO 19)

Our own root-enumerated virtual mic (option D), started from Microsoft's ACX **AudioCodec** sample
(https://github.com/microsoft/Windows-driver-samples, `audio/Acx/Samples`, commit d49bb7d, 2026-11-05), and cut down from
there (doc/todo/19-driver-base-2026-10-28.md has the plan and the spike's checks).

**Licence.** The files under `src/` derive from the sample, which is MS-PL (`LICENSE-MS-PL.txt`): they stay MS-PL (its
section 3(D)). The scripts are ours, under the repo's licence. `src/Common/PacketSize.{h,cpp}` are ported from
microsoft/low-latency-audio (MIT, `LICENSE-MIT-low-latency-audio.txt`) and keep its Microsoft and Yamaha notices.
`src/Shared/nf_vmic.h`, `src/Shared/Cable.h`, `src/Common/AppRing.{h,cpp}` and `src/AudioCodec/Driver/Control.cpp` are ours,
not the sample's: MIT OR Apache-2.0, so the Rust mirror of the protocol stays under the repo's licence (the repo's licence, the owner, 2026-11-23: `LICENSE-MIT` and `LICENSE-APACHE` at the root).

**What changed from the sample** (the first commit after the import shows it): KMDF 1.35 (Windows 11 24H2 / Server 2025,
build 26100, and up: the owner, 2026-11-20) in both projects, and the INF's OS floor 10.0...26100 to match. ACX stays 1.1,
the newest WDK 28000 has.

Packet sizes (spike check 2): both circuits publish `DEVPKEY_KsAudio_PacketSize_Constraints2` on their KSCATEGORY_AUDIO
interface right after `AcxDeviceAddCircuit`: a 2 ms floor, 128-frame RAW and DEFAULT packets, no alignment, no maximum.
A failure to publish is reported (DbgPrintEx) and leaves the default 10 ms periods; it doesn't fail the device.

The loopback pair (spike check 7): what's played into the speaker endpoint comes out of the microphone endpoint. A 768 KB
non-paged ring on the device, under a spin lock; both streams on one QPC time line (the stream engine's own schedule), so
the two ends never drift and need no fill control. The render pass copies the next packet once the client has released it
(SetRenderPacket), else the one completing; the capture pass reads one packet 1 ms and a frame behind, plus the render side's
lead (a packet where packets are copied as they complete, else none). The lead changes only as a render stream starts copying
or another claims the ring, which starts a new run there, so no frame comes out twice. A capture stream takes a change at
once: a fall moves its read on and skips the difference once, counted in STATS (`LoopbackLeadSkips`,
`LoopbackLeadSkippedFrames`). For a lead set in a new run (another owner, or the owner's restart) that is the owner's rule;
within a run it follows the owner's app-ring decision (both 2026-11-23: lower latency over a one-time skip; from 5de56b9 to
e801320 a fall within a run kept the longer lead while the ring held audio it would skip).
Both endpoints offer 48 kHz stereo with float32 first and signed 24-in-32 PCM second. On the Server 2025 spike Windows
stored float32 on both endpoints and the filter opened exclusive float output. Float32 alone left both interfaces disabled.
The loopback copies raw bytes between matching formats only, so a stream is refused at creation (STATUS_NOT_SUPPORTED,
counted in `RefusedOpens`) while the other endpoint has a stream open in a different format, and its client can retry with
that format. Each circuit's open formats are recorded in the ring under its lock, so the check and the record are one step.
The capture side's zero-fill (`MismatchPackets`) stays as the fallback, e.g. for a closed render run's tail read in
another format. The [format validation checklist](../doc/todo/19-spike-2026-11-20.md#format-validation) covers the
integer fallback and Windows 11. The sample's tone, WAV reader and SaveData are no longer called.

Three of the sample's stream-engine bugs are fixed. UpdatePosition subtracted the glitch adjustment (time) from a byte
position, and let the position pass the packet in progress (a Pause could then stall the next Run's timer for about 111
days). Capture packets were stamped a packet late: ACX hands clients the QPC passed to `AcxRtStreamNotifyPacketComplete` as
the packet's first frame's time (it never calls `EvtAcxStreamGetCapturePacket`, whose answer the sample meant for that),
and the sample passed the pass's own time, the packet's end. A capture stream now passes its packet's start on the stream's
time line (STATS' `CaptureTrace` and `nf-vmic-test stamps` show it). And `KSCONVERT_PERFORMANCE_TIME` (ks.h) names its argument
three times, so ScheduleNextPass and UpdatePosition read the clock three times per conversion: across a low-word wrap
(every 2^32 ticks, about 7 minutes at 10 MHz) the words of two reads could make a time about 429 s off and stall the
stream. Each conversion now takes one reading (`QpcToHns`), and GetPresentationPosition reports the position and its QPC
from the same reading (the sample read it three times in UpdatePosition and once more for the QPC).

Two microphones (TODO 19, the owner's option A): "Microphone" keeps IAudioClient3's small periods (128-480 frames, the
packet-size constraints) for apps that ask for them, and "Call Microphone" (a second capture circuit, `Microphone1`) has a
5 ms default engine period from the INF (`PKEY_AudioEngine_OEMPeriod`) for apps that ask for none, as call apps do. Windows
reads that only when it makes the endpoint (TODO 22). Both play the app's ring and the loopback.
Three hardware IDs pick the microphones (`Install-Driver.ps1 -Variant both|call|low-latency`; the driver reads
`NfMicrophones` from the device's hardware key): `ROOT\AcxLlCable` both, `ROOT\AcxLlCableCall` the call microphone,
`ROOT\AcxLlCableLowLatency` the low-latency one. Switching re-creates the device, which needs admin.

**Cables (TODO 19, the per-cable design, `doc/todo/19-per-cable-design.md`).** Every cable (A, B, C...) is a device node of
the one package, with one of the three hardware IDs (shared by every cable) and an instance ID the setup tool fixes,
`ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`: the label one to eight of A-Z and 0-9, the id the cable's identity (a GUID's 36
characters), n an instance number from 1. The driver reads the label and identity there at AddDevice and refuses a node
without them (STATUS_DEVICE_CONFIGURATION_ERROR, code 31: devcon and Add legacy hardware make `ROOT\MEDIA\NNNN`). Each cable
has its own app ring and copy, counters, event logs, timer statistics, capture trace and control device,
`\\.\AcxLlCable_<id>` (the identity in lower case), all in its device's context: removing or re-adding one cable touches no
other's, and its counters start at zero with each binding (AddDevice). Only the stream serial is driver-wide. The device's
friendly name is "ACX-LL-Cable <label>", which Windows puts in its endpoints' names ("Microphone (ACX-LL-Cable D)"); each
circuit's component ID is the identity XOR the sample's constant.

### Independent Cable B

Either cable works with the filter app closed. Send audio to its **Speakers** endpoint and select the **Microphone** on
the same adapter in the receiving application. For the primary pair the adapter is **AudioCodec Device**. Both of its
microphones receive the same signal. The optional **Noise Filter Cable B** adapter has a separate playback and recording
pair, loopback memory and format negotiation.

On the development checkout, install the current primary driver first. In an elevated PowerShell window run
`driver\scripts\Cable-B.ps1 add` to create B. The script uses the primary device's installed INF, retains its microphone
variant and restores any pre-existing default roles that Windows moved to B. `Cable-B.ps1 list` shows B and its endpoints.
`Cable-B.ps1 remove` removes only B. Running `add` again on a healthy pair keeps its endpoint IDs.

To process the signal on A, choose **Direct driver ring** or its WASAPI Speakers endpoint in the filter app. For B, choose
**WASAPI** and **Speakers (Noise Filter Cable B)**. A direct feed takes over A's microphones while attached. B continues
carrying whatever is played into B. The app currently runs one filter chain per engine instance.

To check isolation on the dev box, run `nf-vmic-test open --render --exclusive --format float --signal --seconds 15`
on each Speakers endpoint (`--device "Speakers (AudioCodec Device)"` and `--device "Speakers (Noise Filter Cable B)"`),
with `--start 100000` on A and `--start 4000000` on B. While both renderers play, run
`nf-vmic-test open --exclusive --format float --seconds 3 --device "Microphone (AudioCodec Device)"` and the equivalent on
`Microphone (Noise Filter Cable B)` concurrently. Each capture's decoded marker range should remain inside its own
renderer’s range, with zero corrupt frames. Repeat with `nf-vmic-test feed --seconds 15 --start 100000 --target 1440`
in place of A's renderer to verify an attached A ring coexists with B.

**Latency: two figures, measured at different points** (the Server 2025 VM, 2026-11-20 to 23;
doc/todo/19-spike-2026-11-20.md):

| Measured | Microphone | Call Microphone |
|---|---|---|
| The app's ring to the recording app's receipt, p50 / p99: `nf-vmic-test latency`, from the QPC just after the feeder stores `WriteFrame` to the QPC when the client's GetBuffer returns (every frame decoded back to its number); shared, one packet of fill | 9.8 / 10.4 ms at the default 10 ms period; 3.1 / 3.7 ms for a client that asks for 3.33 ms (160 frames, IAudioClient3) | 4.6 / 5.6 ms (the 5 ms default period) |
| Speakers to mic, sample transit by the device timestamps at both ends: `noise-filter latency --cable-only`, from the probe's play-out by its AudioClock to the arrival by the capture packets' stamps; shared, 10 ms render | 1.91 ms (spread 0.18) | 1.62 ms |

The first answers when the recording app actually has the audio: the ring's fill plus the client's wake, about a packet at a
one-packet fill, so it follows the client's period. The second is the loopback's own transit (the capture pass reads 1 ms
behind the render) and compares with another cable's `--cable-only` figure (VB-CABLE's: 50-62 ms on the same VM); a client
gets each packet about a packet plus its wake after its first frame's device time (10.4 ms at a 10 ms period).

The app feed (spike check 4): each cable's control device, `\\.\AcxLlCable_<id>`, takes an app's ring of 48 kHz float32
stereo frames (page-aligned, 1,024 to 262,144 frames) in one pending METHOD_OUT_DIRECT IOCTL; while one is attached, the microphone endpoint
plays it instead of the loopback, frame for frame. The request ends with ERROR_OPERATION_ABORTED (a cancel, the issuing thread
or process exiting, or the handle closed) or ERROR_DEVICE_REMOVED (the device is going away: the app must then close every
handle, since an open handle on the control device keeps the driver loaded). `src/Shared/nf_vmic.h` has the protocol, every
ATTACH outcome and the ring's rules; `crates/nf-vmic` mirrors it, and its `nf-vmic-test` feeds, verifies, prints the
statistics and kills feeders. The statistics IOCTL also carries each stream kind's timer lateness histogram (spike check 5).
Only SYSTEM and Administrators may open it for the spike: letting unelevated users in first needs attachments tied to a
session (`Control.cpp` says why). Close nf-vmic-test before reinstalling, for the same reason as above.

Every capture stream running in 48 kHz float32 stereo hears the ring. The first to run reads it and sets the pace. The
others play a driver-owned copy of what the reader delivered (256 KiB, non-paged, the cable's) on the same QPC
time line, the reader's packet plus 1 ms behind (`SecondaryFramesCopied`; `SecondaryUnderrunFrames` counts the silence
they get where no reader delivered, a jump in the reader's time line included). No stream gets a frame twice. At a handoff
(the reader stops and another stream takes the reader slot) the taker skips its lag once (`ReaderHandoffs`,
`HandoffSkippedFrames`); the others play on, getting counted silence where the new reader's packet is longer, and where it
is shorter moving on to its lag at once and skipping the difference once, counted apart (`SecondarySkips`,
`SecondarySkippedFrames`: the owner, 2026-11-23, lower latency over a one-time skip; from a66d8ab to e801320 they kept the
longer lag until they next ran). A reader gives the reader slot up as its Pause starts, while its timer still runs: given
up after the timer stopped, it left the copy short for as long as the stop took, and the streams playing on lost 55 to 111
frames at live handoffs (2026-11-23). More than one can run: a cable's two microphones share its ring, and a RAW and a
DEFAULT client make two device streams on one (spike check 4's live runs).

Each cable also keeps two event logs, which its STATS copies (`nf_vmic.h`, "The event logs"; the
owner, 2026-11-24: telemetry to find bugs by, and to prove the design). The skip log (64 entries) holds every event in which
a capture stream never got some of the audio it was due, each with its stream (a serial given at creation), its time, the
span on the QPC time line and the ring's frames it concerns: a taker's handoff skip, a skip to a shorter lag, a jump in a
stream's own time line, a late read (a stream that read past the copy's end gets silence, and the frames the reader then
writes there never reach it: the entry's ring frames are filled in by those writes), a resync, a live discard, and the
loopback's lead skip. The underrun log (32) holds the silence streams got with nothing lost, one entry a run (of one
attachment). So `nf-vmic-test verify` ties each gap in a recording to an entry for exactly the ring frames its codes name,
with any number of recorders (where driver-wide counters can't say whose a skip was); the entry may be any stream's, since
verify doesn't know its own, and where several streams' entries name the same frames it lists every one, and the gap takes
their verdict where they agree (INCONCLUSIVE only where they don't, the owner, 2026-11-24, or where an untracked entry, its
ring frames maybe short, may hold them and would judge it otherwise; an entry still being filled in just after the recording
is no gap's record, since a stream's own entries for a gap settle before it reads the code after it). A gap passes only as
one of the two skips the owner allowed (a taker's, 2026-11-20; to a shorter lag, 2026-11-23): one tied to a late read, a jump
or a resync is the driver's record of a loss, and fails; a late read or jump no gap of the recording names is listed,
diagnostic only (the owner, 2026-11-24). verify also checks the log against the counters, the jumps against the device's own
two (`SecondaryJumps`, `SecondaryJumpFrames`, appended to STATS; the frames the copy held as each jump was logged, as a
skip's are counted, so a part past the copy's end then is only in the entry's ring frames).

The ring is raw float32, so a conflict with another format shows at open time, both ways: while a ring is attached, a
capture stream in another format is refused at creation (STATUS_NOT_SUPPORTED, `RefusedCaptureOpens`), and an ATTACH
while one is open fails with ERROR_NOT_SUPPORTED (`FormatRejects`); the app then feeds through its WASAPI output instead
(24-in-32 into the speaker endpoint, the loopback path). Each check and its publish or record are one step under the app
ring's lock, with the loopback's lock nested inside it at stream creation. Shared-mode streams run at the device format
(float32), so the refusals meet exclusive-mode clients only; the mismatch zero-fill stays as the fallback.

**DriverVer's version is the build's UTC time of day** (since e3026e5; before, StampInf's local time of day, which ranked a
build after local midnight older than the evening's builds). On 2026-11-23 only, packages built before that change carry
the larger, local-time versions and outrank later builds of the same date until UTC midnight: clear the driver store
(`Uninstall-Driver.ps1`) before installing a newer build that day.

## Building and installing (an elevated Windows PowerShell)

1. `scripts\Setup-TestSigning.ps1 [-Ems]`: a test certificate (`CN=Noise Filter Test Driver`), trusted, and test signing
   on (Secure Boot must be off). Reboot once afterwards. `-Ems` would also turn on Emergency Management Services for the
   EC2 serial console; the spike box runs without it (the owner: as few changes as possible). Ported from TMR-MD's
   build.ps1.
2. `scripts\Build-Driver.ps1`: MSBuild, then the package in `out\Release\AudioCodec`: the `.sys` signed, the catalog made
   by inf2cat for `10_GE_X64,Server2025_X64` and signed. `-PacketFrames 96` builds the 96-frame trial (a 1 ms floor). The
   size goes to MSBuild as the properties `NfPacketFrames` and `NfMinPacketPeriodHns`, which `SamplesCommon.vcxproj` makes
   defines; MSBuild records each file's compiler command line, so a build of the other size recompiles what it must, with
   a Visual Studio or bare `msbuild` build (no properties: 128 frames) in between too. On top of that, a guard kept from
   when the size reached cl.exe through the CL variable, which MSBuild doesn't record (until 2026-11-23):
   `out\Release\packet-frames.txt` reads `96-complete` or `128-complete` once a build has finished, and a build of the
   other size rebuilds everything, as does one after a build that failed or was stopped (the marker then reads
   `<size>-incomplete`) or with no marker or an empty one. The old package is removed before MSBuild runs: a failed build
   leaves none in `out\` to install by mistake. A build that stops after the package is made, at InfVerif (if it can't
   run; a report is not fatal) or at the signing, leaves the package of the size asked for, unsigned, and the marker
   `<size>-complete`: the next build of that size builds incrementally (nothing to recompile), then packages and signs.
   The INF's DriverVer is the build's start in UTC (`NfInfDate`, `NfInfVersion`): the date, as inf2cat refuses one after
   UTC today, which the WDK's default, the local date, is here after local midnight (inf2cat exits -2); and the time of
   day as the version, `H.m.s.ms` (03:27:41.512 UTC is `3.27.41.512`), so by date, then version, a later build ranks newer
   (the WDK's default version, the local time of day, put a build after local midnight but before UTC midnight below the
   evening's builds before it). A package with an older DriverVer doesn't replace a newer one: `Uninstall-Driver.ps1`
   first. Packages built before 2026-11-23's change carry the local time of day as the version, so one from earlier the
   same UTC day can outrank a newer build.
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

Release x64 favours speed: both projects set `FavorSizeOrSpeed` to `Speed`, so cl.exe gets `/Ox /Ot` (the WDK's default
was `/Ox /Os`). The .sys grew from 54,272 to 57,856 bytes (2026-11-20).

Under Driver Verifier (the dev box keeps `/standard` on), any of the driver's spin locks taken from PASSIVE_LEVEL can stall
its caller for milliseconds: Force IRQL checking first trims all pageable system memory on such a raise, up to about 100 ms
of it a second (a kernel trace, 2026-11-24, doc/todo/19-spike-2026-11-20.md). That's STATS, ATTACH, detach, stream open
and close, Run and Pause; the per-packet path runs in the timer DPC and isn't affected. So a tool reading STATS keeps it off
its capture thread (nf-vmic-test verify does), and handoff or stream-start timings taken under Verifier run long.
