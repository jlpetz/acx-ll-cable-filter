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
Three hardware IDs pick the microphones (`Cable.ps1 variant <label> both|call|low-latency`, and `Install-Driver.ps1
-Variant` for A; the driver reads `NfMicrophones` from the device's hardware key): `ROOT\AcxLlCable` both,
`ROOT\AcxLlCableCall` the call microphone, `ROOT\AcxLlCableLowLatency` the low-latency one. Switching re-creates that
cable's device, which needs admin.

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

**A cable's endpoints, by role (the app's contract, `nf_vmic::roles`).** Windows' endpoint IDs belong to one installation
of a cable's device. In the first install trial, removing and adding B again gave it three new IDs. Its old records were
still present immediately afterwards. Disabling and enabling B preserved all three IDs. An app saves
a cable's id with the endpoint's role ("B's Call Microphone": the `CableId` and an `EndpointKind`, Speakers, Microphone or
Call Microphone), never the endpoint's id or name, and resolves the current id each time it opens one:
`nf_vmic::roles::resolve(id, role)`, or `Snapshot::read()` then `snapshot.resolve(id, role)` for many. It matches endpoints by
the KS filter they're connected to, its device instance ID (the cable's node) and its reference string (the pin: `Speaker0`,
`Microphone0`, `Microphone1`), never by name, and never returns an endpoint Windows keeps for a removed node or an earlier
installation (not present), even with the same instance ID and pin. It returns the endpoint's id, its state (active,
disabled in the Sound panel, unplugged), its name for display and its node; or none, and why: the cable isn't present, its
microphones lack the role (a one-microphone variant), it is disabled (its endpoints come back with their ids once enabled),
its device isn't working, or Windows hasn't made that endpoint yet. Any user, on a control thread (it enumerates devices and
reads endpoint properties through COM on a thread of its own), never an audio thread. From a shell: `acx-ll-cable endpoint
<label|id> speakers|microphone|call-microphone` (or `scripts\Cable.ps1 endpoint ...`) prints `<id> <state> "<name>"` and
exits 0, or says why on stderr and exits 3 (a caller may redirect the script's stderr: the exit code stays the tool's).

**The boot crash breaker** (ours, for the owner's go; `Device.cpp` says what it can't catch). With Driver Verifier on at
boot, a deterministic bugcheck in a cable's start would come back at every boot. AddDevice counts the device's start
attempts in its hardware key (`NfStartAttempts` under `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device Parameters`,
flushed before it goes on); a completed start (EvtDeviceSelfManagedIoInit) or a binding that ends without a crash clears it.
A clear that fails (the key or the value) is tried again when the binding ends (its context cleanup): the binding forgets
its count only once the value is gone.
A device whose last start never completed is refused: code 31, with 0xE0AC0001 as its problem status, a System event from
the source `AudioCodec` (event ID 1, "ACX-LL-Cable <label> refused: its last start never completed"), and a debug message
saying how to clear it. The setup tool deletes the count when it installs a cable (`Cable.ps1 add <label>` repairs a
refused cable in place). By hand, elevated, to test the refusal: `pnputil /disable-device "<instance>"`, then `reg add
"HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device Parameters" /v NfStartAttempts /t REG_DWORD /d 1 /f`, then `pnputil
/enable-device "<instance>"`, in that order: the disable ends the running binding, whose context cleanup clears a count it
still holds (one whose clear at start failed), so a value set before the disable could be gone by the enable; set after it,
nothing clears it before AddDevice reads it. To clear it: `reg delete "HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device
Parameters" /v NfStartAttempts /f`, then a disable and an enable. It refuses at one start, not two, because at boot the cables
most likely start before the Session Manager makes the SYSTEM hive writable (to confirm on the box), and a crash then loses
the boot's count: the count that stops a loop is the one a crash in start leaves in a session (an install, an update, an
enable), and at two every boot after would count its second attempt in memory only and crash again. So it stops the loop
that follows a crash first seen in a session; it can't catch a crash that only happens at boot, nor one in DriverEntry,
before the count is written.

### Using the cables

Every cable works with the filter app closed. Send audio to a cable's **Speakers** endpoint and select a microphone of the
same cable in the receiving application: what's played into "Speakers (ACX-LL-Cable A)" comes out of both "Microphone
(ACX-LL-Cable A)" and "Call Microphone (ACX-LL-Cable A)" (those its variant makes). Each cable has its own playback and
recording pair, loopback memory, format negotiation, app ring and control device, and any cable can be fed directly: a
direct feed takes over that cable's microphones while attached, and the others carry what is played into them.

**Making, removing and moving cables (S1).** Only the setup tool, `acx-ll-cable` (a bin in `crates\nf-vmic`), makes a
cable's device node; the scripts run it, and fail if it isn't built (`-Build` builds it: `cargo build --release -p nf-vmic
--bin acx-ll-cable`). In an elevated PowerShell window (`list` takes any user):
- `scripts\Cable.ps1 list`: the present cables with their devices, endpoints and control devices, the remembered absent ones,
  the old driver's devices and setup errors. A disabled cable's line ends `: disabled`, its device's line says how it comes
  back, and its endpoints are listed as not present; a failed cable's device line says its problem code instead ("a problem:
  Code 31 ..."). A cable whose disable waits for a restart (below) ends `: disabled at the next restart`, its device working;
  one disabled without CONFIGFLAG_DISABLED ends `: disabled, not in every hardware profile` (a setup run that stopped, which
  a restart undoes, or another tool's for this profile only, devcon's disable: Windows' documented calls that aren't
  deprecated don't tell the two apart).
- `Cable.ps1 add <label> [-Mics both|call|low-latency] [-Adopt <instance ID>] [-EndpointMap <file>]`: makes a cable on the
  package the others run, with both microphones unless `-Mics` (or what it had). Present and working, or disabled: no change
  (`enable` turns a disabled cable on). Present but not working: repaired in place. Removed earlier: back with its identity
  and, with the microphones it had, its instance ID, though Windows gives it new endpoint ids (the first install's trial);
  with other microphones, the next instance number (as a variant switch). `-Adopt` re-creates a device made outside the tool
  (`list` shows them as "Not a cable") as the cable; nothing adopts one otherwise.
- `Cable.ps1 remove <label> [-Forget]`: removes that cable's device only; its identity stays in the setup's store
  (`HKLM\SOFTWARE\ACX-LL-Cable\Cables`) unless `-Forget`.
- `Cable.ps1 disable <label>` and `Cable.ps1 enable <label>` (`acx-ll-cable disable|enable <label>`): keeps the cable
  installed but off, across restarts too, until `enable`; its endpoints keep their ids (not present while it is off), so
  apps find them again. Only that cable's device, through SetupAPI's DIF_PROPERTYCHANGE: the disable in every hardware
  profile (CONFIGFLAG_DISABLED, which the tool checks it set), the enable in every profile and then the current one, as
  Microsoft's devcon sample enables, so a devcon disable (the current profile only) is undone too
  (`crates\nf-vmic\src\setup\devices.rs` says why rather than CfgMgr32's `CM_DISABLE_PERSIST`). `enable` clears the crash
  breaker's count first, checks CONFIGFLAG_DISABLED is gone, waits for the device and its endpoints, and fails its check
  (exit 2) for an endpoint that came back with a new id. A device Windows can't stop or start now is changed at the next
  restart (exit 7): a disable that waits so leaves the cable working until then, `list` saying so, and `enable` cancels it.
  `update` leaves a disabled cable disabled, on the new package; `add` leaves it as it is (other microphones asked for are
  said, not made).
- `Cable.ps1 variant <label> both|call|low-latency`: re-creates that cable's device with the next instance number (new
  endpoints, which Windows makes with the variant's default period); no other cable is touched. A disabled cable, or one
  whose disable waits for a restart, is refused (exit 2): the re-made device would be enabled, so `enable` it first, switch,
  then `disable` it again.

  `add` and `variant` put back the default roles Windows moved to a new endpoint.

`scripts\Install-Driver.ps1` adds the package to the driver store, then:
- the old driver's devices present (`ROOT\AudioCodec*` as A, `ROOT\NoiseFilterCableB` as B): the migration, once
  (`acx-ll-cable migrate`). Nothing changes unless the package makes A and B; then A and B are recorded in the store, every
  device of ours is disabled until the old image unloads, the old devices are removed, and A (its variant, or `-Variant`)
  and B (`-CableBMics`, default both) are made on the new package. A run that stops part way is finished by running it again:
  a cable it made and didn't record yet (or made in a run that needs a restart, which stays pending until then) is checked (its
  identity on one device, working on the package, its endpoints) and recorded, and with the old devices gone and every cable
  on the package, nothing is disabled or installed again. A cable made stays pending until Windows has made every endpoint of
  its microphones (after a restart too), and `acx-ll-cable list` says so (a `note:` under it); a restart Windows needs, the
  removal of a cable that failed to install among them, stops the run (exit 7) before the next cable. It runs the migration
  while a record in the store has `Pending` or `Replaces` (below).
- cables present: `acx-ll-cable update <package folder>`, every cable moved to the package together. Every cable stops for a
  few seconds: one image serves them all. If a cable refuses the disable, or the old image is still loaded 10 s after every
  cable was disabled (an app has a cable's control device open), every cable is enabled again, nothing is installed, and it
  says to close the apps using the cables. `-Variant` then switches A.
- none: `acx-ll-cable add A --mics <Variant>`.

`scripts\Uninstall-Driver.ps1` removes every device of ours (`acx-ll-cable remove --all --forget`, the old driver's too) and
deletes the packages; `-KeepIdentities` keeps the store, so cables added again keep their identities. A cable's id is a
random GUID unique to the machine: scrub it from logs pasted into docs.

**Exit codes**, the tool's, the three scripts' and `noise-filter mic variant`'s: 0 done (or nothing to do); 7 Windows wants
a restart to finish, with or without other errors beside it (a caller stops there: restart, then run it again, which finishes
what is left and says what is still wrong; a removal Windows finishes at the restart, the migration's of an old device among
them; pnputil's own 3010 is 7 too); 2 the tool's error (its steps say what changed), or a package Uninstall-Driver.ps1
couldn't delete; 3 `endpoint` only, no endpoint for that role now; 1 a usage error or a script's own check (not elevated,
test signing, no package, no tool).

**The endpoint map.** `Install-Driver.ps1 -EndpointMap <file>` (the migration, or the first `add A`) and `Cable.ps1 add
<label> -EndpointMap <file>` pass `--endpoint-map <file>` to the tool, which writes, as JSON, which new endpoint takes each
old one's place, by cable and role (Speakers, Microphone, Call Microphone), with the ids and names. The old driver's
endpoints map to A's and B's; a re-added cable's to its own. Each old endpoint's `match` is `same` (Windows kept the
endpoint), `moved` (one new endpoint of that role: remap the id), `gone`, `ambiguous` or, for a new endpoint without an old
one, `added`; only `same` and `moved` are remapped. B's one microphone maps to its low-latency Microphone, and its Call
Microphone is `added`. The tool publishes the file whole at the end, once it has read the endpoints before the run: written
to a new temporary file of its own beside it (`<file>.<pid>-<n>.tmp`, created exclusively, so no file there already, of any
name, is opened, truncated or replaced), flushed, then renamed over it, so a reader finds the file as it was or the new map,
never part of one; a run that publishes none (stopped earlier, killed) leaves the file as it was, a killed one perhaps its
temporary file too (format and fields:
`crates\nf-vmic\src\setup\endpoint_map.rs`). A reader closes the file before the run ends: Windows doesn't rename over a file
another process has open, so the tool waits about a second for it, then refuses the run at the start (nothing changed) or
doesn't publish (the file as it was). A migration's cable keeps the old devices it replaces in its record (`Replaces`) once
it is made. Each run maps the cables it makes or checks, and every present cable recorded as made whose record has them,
however the run ends, so the map of the run that finishes the migration (no record left pending) holds all of it, whatever
runs came before (killed, stopped by a restart); that run then drops them, a record at a time, each keeping them as
`Replaced`, so a drop cut short loses no cable from the next run's map. A map published while a record is pending may lack
a cable not made or recorded yet (`result` is then `error` or `restart`). Whichever run publishes the whole map, its rows
are the same (cable, role, old and new ids, `match`); an old endpoint's `state` may differ (a retry after the old devices'
removal finds them `notpresent`). A map asked for and not published keeps them, and the next `Install-Driver.ps1` runs the
migration again to publish it.

The settings window saves an ACX-LL-Cable device choice by cable ID and role. To recover choices that still refer to
old Windows endpoint IDs after a migration, close the window and run `noise-filter-cli remap-devices <map.json>` as
the user who owns the settings. Use the map from the completed `migrate` or `add` command. `--config <file>` selects
a particular settings file. The command reports any ambiguous choices and leaves them available for manual selection.

**Feeding a cable (N0, interim until discovery: D2 and N1).** nf-vmic-test takes a cable's control device with `--path
\\.\AcxLlCable_<id>` (the id as `Cable.ps1 list` shows it); without it, and the app's **Direct driver ring** and the broker
always, take the cable labelled A, else the only cable. Each mode records or plays that cable's own endpoints, found by the
instance ID in their KS filter's path. `\\.\NoiseFilterVmic` is gone: a tool that opens it gets ERROR_FILE_NOT_FOUND. The
control device admits SYSTEM and Administrators only, so the audio tools run elevated. The dev box migrated both cables
in the first install trial on 2026-11-26.

To check isolation on the dev box, run `nf-vmic-test open --render --exclusive --format float --signal --seconds 15`
on each Speakers endpoint (`--device "Speakers (ACX-LL-Cable A)"` and `--device "Speakers (ACX-LL-Cable B)"`), with
`--start 100000` on A and `--start 4000000` on B. While both renderers play, run
`nf-vmic-test open --exclusive --format float --seconds 3 --device "Microphone (ACX-LL-Cable A)"` and the equivalent on
`Microphone (ACX-LL-Cable B)` concurrently. Each capture's decoded marker range should remain inside its own renderer's
range, with zero corrupt frames. Repeat with `nf-vmic-test feed --seconds 15 --start 100000 --target 1440` (with N0's
`--path` to A's control device) in place of A's renderer to verify an attached A ring coexists with B, and the same on B
with A attached.

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
3. `scripts\Install-Driver.ps1 [-BuildTool]`: the migration from the old driver's devices, the update of every cable, or
   cable A, through the setup tool ("Using the cables", above). More cables: `scripts\Cable.ps1 add <label>`.
4. `scripts\Get-DriverStatus.ps1` (the tool's `list`), `scripts\Uninstall-Driver.ps1 [-KeepIdentities]`.
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
