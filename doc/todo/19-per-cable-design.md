## TODO 19 design: accessible diagnostics on every cable

**Status: the owner answered its questions (2026-11-24, with Astra; below), and asked for three revisions before it is
built: discovery without hard-coded cables, diagnostics reset when a cable is re-created, and no compatibility promise
(pre-release). Being revised; nothing built.**

**The owner's decisions (2026-11-24, with Astra):**
- **Any number of cables, discovered.** Adding a cable (C, D, E...) is a setup action: another instance of the same
  installed driver package, which the existing app finds by asking Windows; no new driver binary or app build, and a plain
  cable needs no filter setup. A discovery-only device interface per cable is enumerated, then the chosen cable's own
  control device opened; arrival notifications can wait. No fixed slot count and no compiled-in paths: letters are labels,
  gaps allowed. Acceptance: add D while A carries audio, B and C absent; D appears and works at once, A keeps playing,
  nothing rebuilt.
- **Diagnostics without UAC, redacted:** signed-in interactive users may read a cable's statistics and logs; feeding stays
  behind the write permission and the broker's approval. Document the trade-off: another signed-in user (Remote Desktop
  included) can tell when a cable is active. The feeder's process id is hidden from read-only handles.
- **Snapshots:** the 10 ms limit applies to refreshing the snapshot; readers arriving together get the same timestamped,
  redacted snapshot (no ERROR_BUSY, so no reader can starve another); the GUI refreshes once a second.
- **No compatibility promise (pre-release; the owner: breaking changes welcome now for a clearer layout):** the statistics
  get the clearest layout now and their own version, changed when the layout changes; a mismatched tool is told to update.
  The audio-protocol version changes only for changes to how audio is exchanged.
- **Identity and generations:** a cable keeps a stable identity across removal and re-adding; each binding gets a fresh
  generation, with its counters and logs reset (so "how many frames has this instance lost" is unambiguous); A's continue.
  The diagnostics view may keep history across generations, marking each restart.
- **Names:** the product's short name is **ACX-LL-Cable** (from the repository's acx-ll-cable-filter): endpoints like
  "Microphone (ACX-LL-Cable A)" and "Speakers (ACX-LL-Cable B)"; in docs and the GUI, "ACX-LL-Cable B" beside "VB-CABLE B".
  `\\.\NoiseFilterVmicB` style names and `--cable b` are aliases; the CLI lists installed cables and takes a stable id too.
- **Writers:** one independent writer per cable, two feeds from the one broker included; stopping or removing B leaves A
  running; the broker resolves the requested cable from the driver's discovered devices; the session approval's scope made
  clear. A user running only plain cables needs neither the broker nor the app.
- **Input guard:** reject the destination cable's own microphones as the app's input; another cable is allowed (a mic ->
  effects -> B); match by cable identity, not endpoint name; reject a known cycle (A -> B -> A) where the app runs several
  routes.
- **Fan-out (the owner, 2026-11-24):** capture the physical microphone once and send it to one or more cables, each
  receiving the dry mic, the output of a shared effects chain, or its own chain; stopping or reconfiguring one destination
  leaves the others playing. Acceptance: Cable A carries the dry mic to OBS while Cable B carries the echo-cancelled,
  denoised mic to Discord.
- **Acceptance runs:** WASAPI into B counts as an interim milestone; the effect is the gain at 0.25: the receiving mic
  measures -12.04 dB while the other cable carries its own signal unchanged (as already shown on A, TODO 17); then direct
  to B, swapped to direct to A, then B removed while A keeps recording. A reduced-rights token for an early access check;
  a real standard account for release (diagnostics without UAC, direct attach refused without approval, the broker's
  approval and cancellation).

Read-only analysis. I changed nothing. The tree is clean on main, now at d0c39e1, a docs-only commit by the lead after e2fc539. Two public sources were read through `gh api` into a scratch folder, which is now deleted: WDF's open-source `FxPkgGeneral` and microsoft/low-latency-audio's ACX driver.

### Recommendation in short
- **One control device per cable instance.** A keeps `\\.\NoiseFilterVmic` unchanged. B gets `\\.\NoiseFilterVmicB`.
- **Same protocol on both.** Each speaks today's ATTACH (v1/v2) and STATS. A new read-access IOCTL, `NF_VMIC_IOCTL_CABLE_INFO`, returns the cable's identity.
- **Per-cable state.** Today's global app ring, its copy, the timer statistics and the capture trace become a fixed array of per-cable slots in non-paged static storage.
- **Read access for ordinary users.** The SDDL gains read for interactive users, `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)`. ATTACH's existing write-access bit then keeps the feed on the approved path. The I/O manager enforces this before the driver sees the request.
- **Not recommended for now:** a custom device interface on the ACX FDO. Why is below.

### 1. What is there today (verified in code)
- **One control device.** `driver/src/AudioCodec/Driver/Control.cpp` creates `\Device\NoiseFilterVmic` with SDDL SY+BA. `s_DeviceCount` is 0 or 1, and a second primary fails AddDevice with STATUS_DEVICE_BUSY. Cable B never calls `NfVmic_DeviceAdd`: `Device.cpp` detects `ROOT\NoiseFilterCableB` by hardware ID, and `VmicRegistered` stays FALSE.
- **Driver-wide globals.** `g_NfAppRing`, `g_NfAppHistory` (256 KiB in .bss), `g_NfStreamStats`, `g_NfCaptureTrace` and `g_NfStreamSerial` are all globals in `driver/src/Common/AppRing.cpp`.
- **A's counters are already A-only.** Astra's 09e5520 gates every timer, glitch, run and capture-trace record on `m_Loopback->AppFeed` (`StreamEngine.cpp`, timer callback, Run, notify, GetCapturePacket, ProcessPacket, CaptureStreamOpen/Close). B's streams record nothing.
- **B's records exist but can't be read.** B's loopback counters and both its event logs live in its own `LOOPBACK_RING` (the loopback lead skips and underruns go to `ring->SkipLog` and `ring->UnderrunLog` of the stream's own device). No IOCTL can read them.
- **The app side has one path.** `crates/nf-vmic/src/device.rs` opens `USER_PATH` read and write. The broker helper (`broker/helper.rs`) opens the same path and holds one `Option<Attachment>`. `ring_output.rs` calls `broker::attach_live(LIVE_CAPACITY)`.
- **No app code reads STATS.** Only nf-vmic-test does (it checks `stats_bytes == size_of::<Stats>()` exactly).

### 2. Discovery: how requests reach an ACX FDO, and the options weighed

**Facts from the WDK 10.0.28000 on this box**
- **ACX 1.1 offers no device-level I/O hooks.** `acx/km/1.1/acxdevice.h` exposes only `AcxDeviceInitInitialize` and `AcxDeviceInitialize`, plus circuit add/remove: no file-object, queue or request hook at device level. Per-circuit hooks do exist: `AcxCircuitInitAssignAcxRequestPreprocessCallback`, `...Properties` and `...Methods`.
- **The class-extension API isn't shipped.** `wdfcx.h` (the WDF class-extension interface ACX is built on) is not in the WDK.
- **IRQLs.** `EVT_WDF_DEVICE_FILE_CREATE` is `_IRQL_requires_max_(PASSIVE_LEVEL)`. `WdfRequestGetFileObject` and `WdfFileObjectGetDevice` run at up to DISPATCH_LEVEL.
- **KMDF 1.35 APIs present:**
  - `WdfDeviceQueryPropertyEx`, `WdfDeviceAssignProperty` and `WDF_REQUEST_PARAMETERS.Parameters.Create.SecurityContext` are present.
  - No `WdfDeviceAssignInterfaceProperty` in kernel KMDF. WDM's `IoSetDeviceInterfacePropertyData` is there.
- **`wdmsec.h`:**
  - It supports `IU` ("local and Remote Desktop logons").
  - It ships the world-read pattern `SDDL_DEVOBJ_SYS_ALL_ADM_RWX_WORLD_R` = `D:P(A;;GA;;;SY)(A;;GRGWGX;;;BA)(A;;GR;;;WD)`.
- **`wdm.h`:** FILE_READ_ACCESS/FILE_WRITE_ACCESS equal FILE_READ_DATA/FILE_WRITE_DATA. A handle opened GENERIC_READ therefore passes STATS (FILE_READ_ACCESS) and fails ATTACH (FILE_READ_ACCESS|FILE_WRITE_ACCESS) at DeviceIoControl with ERROR_ACCESS_DENIED.

**Facts from public source, read only**
- **WDF's create chain.** In `FxPkgGeneral::OnCreate`, a class extension's `EvtCxDeviceFileCreate` runs first and may claim the create. If none claims it, the client's create queue or `EvtDeviceFileCreate` gets it.
  - So a custom reference string on an ACX FDO reaches us only if ACX declines it.
  - ACX's behaviour for an unknown reference string, and for a non-KS IOCTL on a file object that isn't a circuit's, is closed source and undocumented here.
- **Microsoft's own ACX driver uses no custom interface or queue.** That is low-latency-audio's `src/uac2-driver`:
  - It sets a file-object config with only a cleanup callback after `AcxDeviceInitInitialize`.
  - It registers a private KS property set (`KSPROPSETID_LowLatencyAudio`) on the render circuit.
  - Its ASIO client finds the circuit's own KS interface with SetupDi and the reference string, opens it GENERIC_READ and sends IOCTL_KS_PROPERTY.
  - The handler locks the ASIO buffer itself: `IoAllocateMdl` and `MmProbeAndLockPages` in `AsioBufferObject.cpp:114-134`, with a KernelMode probe at :124.
- **KMDF's sideband patterns.** The toaster sideband filter uses a control device; Control.cpp follows it. kbfiltr uses a raw PDO child with its own interface and SDDL. That kbfiltr detail is from memory, not re-read.

**Options**
1. **Per-instance named control devices (recommended).**
   - For:
     - ACX's FDO dispatch is untouched.
     - The lifetime is the live-tested one, per instance: made in the cable's AddDevice, deleted in its context cleanup, link deleted by hand, queue purged.
     - Our own SDDL. The I/O manager's access bits split read from feed.
     - An open handle doesn't veto PnP removal.
     - The isolation is structural: a handle on B's device can't reach A's slot.
   - Against:
     - The names are global, so a new cable is a new table row in the driver (acceptable).
     - No PnP arrival notification. The app re-probes when the MMDevice notifications show endpoints appear, which it already handles.
2. **A custom interface on each ACX FDO** (`WdfDeviceCreateDeviceInterface` with a reference string). Not now:
   - (a) ACX's claim or decline of the create, and its routing of our IOCTLs, are unknown and need a spike.
   - (b) An open handle on the FDO takes part in query-remove. The GUI, the broker and nf-vmic-test would all need `CM_Register_Notification` handle notices, or disable, update and the `mic variant` re-creation get vetoed or need a reboot.
   - (c) The FDO's security descriptor is the one Audiosrv's KS-filter opens use. ATTACH's restriction would move from SDDL to a check in code.
   - (d) The FDO's S0-idle policy against a request pending for hours. Our queue would need `PowerManaged = WdfFalse`, and ACX's own create path is unknown.
3. **A raw PDO child per cable with its own interface and SDDL** (kbfiltr's pattern). It is PnP-native discovery, but it has 2(b)'s veto. ACX already makes child devices (`AcxDeviceAddCircuitDevice`), so child-list ownership on the FDO is a risk. This is the fallback if the owner wants interface-class enumeration and notifications.
4. **A private KS property set on the circuit** (low-latency-audio's route). It is fine for read-only data. For ATTACH it is wrong:
   - IOCTL_KS_PROPERTY is FILE_ANY_ACCESS, so there is no access-bit split.
   - It is METHOD_NEITHER, so the driver must probe and lock the user ring itself, where today the I/O manager does it in the caller's mode.
   - It would hold a property request pending for the attachment's life.
5. **One control device with a cable selector** in ATTACH_IN and STATS input. Rejected:
   - The ATTACH input changes, which needs a version bump under the owner's rule.
   - Per-cable removal can't use the queue purge, so new cancellation code is needed at the most delicate spot.
   - One SDDL covers every cable.

A later add-on to option 1, if wanted: a discovery-only interface on each FDO, never opened, whose `IoSetDeviceInterfacePropertyData` property names the control path. It gives PnP enumeration without I/O through ACX.

### 3. A cable's identity
- **Stable ID:** the cable's role, from the hardware ID AddDevice already reads. `ROOT\AudioCodec*` gives 0 (the primary pair, AudioCodec Device); `ROOT\NoiseFilterCableB` gives 1 (Noise Filter Cable B). The role survives variant switches and B's remove and add.
- **Instance:** `DEVPKEY_Device_InstanceId`. It is read with `WdfDeviceQueryPropertyEx` at PASSIVE in AddDevice into a stack buffer, then copied into the slot under its lock. A `Generation` counter is bumped at each bind, so a re-created device can be told apart.
- **Endpoints:** derived, not stored. They are Speaker0 always, Microphone0 if `Microphones` bit 0, Microphone1 if bit 1 (B: Speaker0 and Microphone0). The app maps them through the KS filter path's instance ID and reference string, which `sessions::identify` and `parse_filter` already parse, extended to render endpoints.
- **Friendly name:** from PnP (DeviceDesc, FriendlyName) by instance ID, app-side. The driver carries no strings.
- **Build stamp (optional):** a field in CABLE_INFO, which also answers the 09e5520 review's request to prove the loaded image.

### 4. Per-cable app ring, STATS and logs (driver; ours)
- **Slots.** `NF_CABLE g_NfCables[NF_VMIC_CABLES]` (2) in non-paged static storage. Each slot holds:
  - today's `NF_APP_RING` (its Lock, Removing, Loopback/LoopbackOwner, Stats, segments, claims);
  - its History (`g_NfAppHistory[2][256 KiB]`; or allocate B's on first bind and free it in DriverUnload);
  - `StreamStats[2]` and `CaptureTrace`;
  - its binding: Device, Generation, Microphones, BoundQpc, `InstanceId[200]`;
  - its own ControlDevice and ControlQueue.

  Static storage keeps today's reasoning: it outlives the PnP device and any lingering deleted control device. `g_NfStreamSerial` stays driver-wide, so a stream serial is unique across cables and (cable, serial) labels are unambiguous.
- **Devices.**
  - Every device calls `NfVmic_DeviceAdd(Driver, device, &devCtx->Loopback, cable, microphones)`, B included.
  - `devCtx->Cable` replaces `Loopback.AppFeed`.
  - A duplicate of the same cable still fails AddDevice (STATUS_DEVICE_BUSY: today's rule, now per cable).
  - A control-device failure (for example a name collision) still costs only that cable's feed.
- **Stream engine.**
  - `m_Cable` is set in the constructor, as `m_Loopback` is.
  - `&g_NfAppRing` becomes `&m_Cable->Ring`, and `g_NfAppHistory` becomes `m_Cable->History`.
  - The `NfVmicRecord*` and `NfVmicTrace*` calls take the cable.
  - The AppFeed gates go, so B's streams record into B.
  - `CaptureStreamOpen/Close` get the cable from the caller (`CaptureCircuit.cpp:774`).
- **Control devices.**
  - Per-cable name table in `nf_vmic.h`: `\Device\NoiseFilterVmicB`, `\DosDevices\NoiseFilterVmicB`, `\\.\NoiseFilterVmicB`.
  - A new `EvtDeviceFileCreate` (PASSIVE) fills the file context: CleanedUp, `Writable` (FILE_WRITE_DATA in `SecurityContext->AccessState->PreviouslyGrantedAccess`) and `Cable`.
  - The cancel routine, EvtFileCleanup and the IOCTL callback reach the slot through `WdfRequestGetFileObject` and the file context. All are DISPATCH-safe, and the slot is static, so a lingering deleted control device's cleanup is safe.
- **STATS** becomes the slot's view:
  - its Ring, its device's Loopback and logs, its StreamStats and CaptureTrace;
  - appended: CableId and Generation (StatsBytes grows; no bump);
  - a new flag `NF_VMIC_STATS_REDACTED`: AttachedPid is 0 for a read-only handle.
- **New `NF_VMIC_IOCTL_CABLE_INFO`** = `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_READ_ACCESS)` = 0x00226008.
  - Fields: Magic, Version, InfoBytes, Flags (WRITABLE: this handle may attach; ATTACHED), CableId, Generation, Microphones, BoundQpc, InstanceId, optional build stamp. About 512 bytes.
  - An older driver answers ERROR_INVALID_FUNCTION, meaning primary only.
- **Coordination.** The workflow building the owner's decision (4) (two 64-bit jump totals per cable) also appends to STATS. Whichever lands second takes the next offsets. Those totals live naturally in the slot.
- **Proposal (ours): STATS for a shorter buffer fills a prefix.** It would accept any buffer of at least 64 bytes, write min(len, sizeof), and keep StatsBytes as the driver's size. The GUI's diagnostics then don't break at every append; tools still check `>=`.

### 5. The access split
- **Feed stays privileged.** ATTACH keeps needing write access, so only SYSTEM and Administrators can attach, which means the elevated broker or an elevated app. A standard user's CreateFile for read and write still fails with ERROR_ACCESS_DENIED, so `broker::attach_live`'s fallback (`broker.rs:149-153`) is unchanged.
- **Read-only access opens STATS and CABLE_INFO to interactive users**, without UAC and apart from feed permission (the owner, 2026-11-24).
- **Proposals (ours):**
  - Withhold AttachedPid from read-only handles.
  - Rate-limit read-only STATS per cable, for example one per 10 ms with ERROR_BUSY beyond; writable handles are exempt, so `stats --time` still works. A tight loop from an unprivileged process otherwise contends the cable's ring spin lock against its capture DPC.
  - Diagnostics readers open, read and close each poll. A long-lived handle on a control device keeps the image loaded after removal, which is today's "close nf-vmic-test before reinstalling" caveat, now reachable by any user's GUI.
- **Residual exposure:** another signed-in user can see when this user's microphone streams run. This is the owner's call (open questions).

### 6. App side
- **nf-vmic (ours: protocol and library):**
  - `protocol.rs`: a `Cable` enum with fixed paths, the CABLE_INFO mirror and the STATS appendix, each with offset asserts.
  - `device.rs`:
    - `Device::open_cable(cable)` opens read and write; `Device::open()` stays A.
    - `Device::open_diagnostics(cable)` opens GENERIC_READ.
    - `cable_info()` reads the identity.
  - New `cables.rs`:
    - Probe each path read-only and map endpoints through `sessions`.
    - `combined()` merges the cables' event logs by QPC, each labelled with its cable and stream serial (for example "B #17"). Each cable's snapshot keeps its own QpcNow: there is deliberately no cross-cable lock.
  - `sessions::is_ours` and `identify` should know B by instance ID; today they match "AudioCodec Device" only.
- **Broker (Astra's):**
  - START and STOP carry a cable byte indexed into a compiled-in table, or become START_B and STOP_B. That keeps the helper's property that no command supplies a path, handle, capacity or duplication target.
  - The helper holds `[Option<Attachment>; 2]`, with session checks for both.
  - `attach_live(cable, capacity)`. Each LiveFeed stops only its own cable.
- **Engine (Astra's):**
  - `IoConfig.direct_cable` with a serde default of primary, so old settings keep feeding A.
  - `RingOutput::open(cable)`; per-cable names and messages in `open_error`.
  - The loop guard in `audio.rs:397-411` goes by the cable's identity, not the name.
  - CLI `--cable`.
- **GUI (Astra's):**
  - The Output route lists Direct to each cable present, plus WASAPI. The plain-cable hints follow it.
  - One approval covers both cables.
  - An optional Diagnostics card: per-cable counters and the combined log labelled by cable and stream. It polls at most once a second on a control thread, never an audio thread; STATS reads stall up to 89 ms under Verifier (19-spike, 2026-11-24).

### 7. nf-vmic-test (ours)
- **`--cable a|b`** (default a) on feed, verify, kill-test, latency, stamps, idle-test and open:
  - The default `--device` follows the cable.
  - verify adds a guard check: the recorded endpoint's filter-path instance must equal CABLE_INFO's InstanceId, so a recording on another cable fails instead of passing against the wrong STATS.
  - open reads STATS from the cable that owns `--device`'s endpoint (none for VB-CABLE).
  - kill-test passes `--cable` to its child.
- **`stats --cable a|b|all`:** all prints each cable and then the merged, labelled log.
- **New `cables`:** lists each cable's path, whether it is present, read or write access, identity, endpoints and attachment state.
- **New `isolation`:** the acceptance checker (section 9).
- The help text and the README's "Independent Cable B" isolation paragraph follow.

### 8. Compatibility and the version rule
- **No version bump.** NF_VMIC_VERSION stays 1 and LIVE 2. B's control device speaks the identical ATTACH, header and ring rules. CABLE_INFO is a new diagnostic IOCTL. STATS grows by appending. Per the owner's 2026-11-23 rule, none of this needs a bump.
- **Existing apps.** A's name, protocol and ATTACH semantics are unchanged, and the shipped app and broker keep working. A's counters excluded B before and still do.
- **Behaviour changes:**
  - A standard user can now open A read-only. Nothing relied on that failing; the broker fallback relies on the read-write open failing, which it still does.
  - Old nf-vmic-test builds refuse the larger STATS ("rebuild one"), as today.
- **New app on an older driver.** B's path gives ERROR_FILE_NOT_FOUND ("this driver has no direct feed for B") and CABLE_INFO gives ERROR_INVALID_FUNCTION.

### 9. Acceptance test as a script plan
The script is `nf-scratch/iso/iso.sh`, run by the lead or the owner with the box idle, under Verifier, 3 rounds a phase, about 15 minutes. `nf-vmic-test isolation --fed F --plain P --seconds 15 [--external]`:
- reads both cables' STATS before and after;
- in tool mode, feeds F (`--start 4000000` on B, `100000` on A) and verifies its recording;
- plays the signal into P's Speakers with exclusive float render and records P's Microphone with a float decode;
- uses periods that make the two cables' expected timer-pass sums at least 3x apart: F's recorder at `--period-frames 160`, about 300 passes a second; P's recorder at 480, about 100 a second.

**Checks on the fed cable F:**
- Same attachment throughout; Attaches +0, or +1 in tool mode.
- Ring.Passes ≈ seconds × 48000 / ReaderPacketFrames.
- FramesCopied + UnderrunFrames ≈ Passes × packet.
- Stream[CAPTURE].TimerPasses within ±2% + 5 of its own recorder's expected count, and outside the sum that would include P's recorder.
- Stream[RENDER] delta 0 on the direct route.
- Every log entry has F's Attach, and its ring frames are in F's fed range.

**Checks on the plain cable P:**
- Not attached in either snapshot.
- Ring.Attaches, Passes, FramesCopied, NotReaderPackets and UnderrunFrames deltas all 0.
- Loopback.RenderRestarts +1 and CaptureAnchors at least +1.
- Stream[RENDER] and Stream[CAPTURE] within tolerance of P's own streams.
- The recording decodes only P's range: 0 corrupt, 0 repeats.

**Cross check:** no stream serial appears in both cables' logs.

**Phases:**
- **P0a / P0b:** tool-fed B with A plain, then swapped. Needs only the driver and the tool work.
- **P1w:** the app with an effect (nf-fx's gain example) routed by WASAPI into Speakers (Noise Filter Cable B), `--external`, A plain. B's checks flip: Ring deltas 0, Loopback and Stream[RENDER] rise by the app's render passes. Possible before the broker change.
- **P1:** the app, effect on, Direct to B. The app's input is a non-cable mic (VB-CABLE on the dev box).
- **P2:** the app, effect on, Direct to A, with B plain (the swap).
- **P3:** `Cable-B.ps1 remove` during `feed --cable b` plus `verify --cable a`. B's feed ends with ERROR_DEVICE_REMOVED; A passes; A's DetachRemoval delta is 0. Then add B back: Generation +1 and B's counters continue.
- **P4:** an unprivileged read through `runas /trustlevel:0x20000`, if it proves to be a non-admin IU token. `stats --cable all` succeeds with REDACTED set, and `feed --cable a` gets ERROR_ACCESS_DENIED. The owner's standard-user run is the release check.

### 10. Verifier and IRQL risks
- **No new nesting, but per cable.** The lock order stays: the cable's ring lock, then its device's loopback lock, then the log locks. No path holds two cables' locks (assert that a stream's `m_Cable` is its device's slot). Verifier's deadlock detection sees more lock instances, not new nesting.
- **New callbacks.** `EvtDeviceFileCreate` runs at PASSIVE and must complete every request. Everything the cancel and cleanup paths call is DISPATCH-safe. Nothing allocates in a DPC; the slots are static.
- **B's control device delete while A's lives.** B's purge and delete happen at PASSIVE in B's cleanup, which is new live territory: run spike check 4's cases on B (disable and enable with a handle held, kill-test) with A attached.
- **Small things.** The const name tables live in .rdata. A 256 KiB static history for B (or allocate it on first bind).
- **Paged-code inlining.** The `noinline` rule on the Control entry points stays, so the spin-lock code isn't inlined into PAGE code.

### 11. Work breakdown
**Ours (driver, protocol, nf-vmic-test):**
- D1, the slots refactor:
  - No behaviour change for A; B registers with its own control device.
  - Build plus InfVerif.
  - Live: A's regression set (three and four recorders, kill-test 10 of 10, ten minutes, disable/enable with a handle held), then B's feed, verify and removal.
  - About 2-3 days.
- D2: CABLE_INFO, the STATS appendix, the file context with access and redaction, the SDDL with IU read, the rate limit; README and `nf_vmic.h` text. About 1 day.
- P1: `protocol.rs` mirror, device.rs opens, `cables.rs`, the sessions fix. About 1 day.
- T1: nf-vmic-test `--cable`, `cables`, `stats --cable all`, `isolation`; `iso.sh`. About 1.5 days.

**Astra's (broker, engine routing, GUI):**
- B1: broker START/STOP per cable, per-cable attachments. About 0.5 day; security review.
- E1: `direct_cable` config, `RingOutput::open(cable)`, loop guard, CLI `--cable`. About 0.5-1 day.
- G1: per-cable route choices, the Diagnostics card with the combined view. About 1-1.5 days.

**Order:** D1 → D2 + P1 → T1, which allows P0, P1w, P3 and P4. B1 + E1 allow P1 and P2. G1 last.

### 12. Risks
1. **The most-tested and most delicate code moves.** D1 shifts attach, cancel and cleanup races and the control-device lifetime from globals into slots. Make it a mechanical change with slot 0 identical, and re-run the full live set.
2. **Widening read access.** Cross-session timing is exposed; the pid is withheld. Polling can contend the ring lock; the rate limit answers that. User handles can keep the image loaded; open, read and close answers that.
3. **Redaction depends on unconfirmed data.** It assumes `PreviouslyGrantedAccess` is filled at create. If not, the fallback is an admin-only STATS code.
4. **Merge collisions.** With the decision-(4) workflow on STATS offsets, and with the open STATS-stall analysis.
5. **More holders of the old image.** The 09e5520 review found that while B is present, an A variant switch doesn't unload the image. B's control-device handles add to that. The fix belongs with `variant.rs`.
6. **The broker is security-critical.** Keep the fixed-table property.
7. **Naming.** "Cable A/B" collides with VB-CABLE's A and B in the docs; use "primary pair (AudioCodec Device)" and "Noise Filter Cable B".
8. **If PnP-native discovery is wanted later,** ACX's create and IOCTL routing and query-remove behaviour need a spike first.

**Needs confirming on the box:**
- `PreviouslyGrantedAccess` at EvtDeviceFileCreate for a read-only open.
- That an IU read open lets a non-admin run STATS while ATTACH fails at DeviceIoControl.
- That the `runas /trustlevel` token is non-admin and carries the IU SID.
- B's control-device delete while A's lives, under Verifier.
- Only for options 2 and 3: ACX's handling of an unknown reference string, its IOCTL routing for non-circuit file objects, and query-remove with an open FDO handle.

When this is implemented, code comments cite "the owner, 2026-11-24" for: per-cable records, the optional combined view labelled by cable and stream, diagnostics apart from feed permission, and the acceptance test. The SDDL, redaction, rate limit, names, prefix STATS and cumulative per-slot counters are marked ours until the owner agrees.

## Recommendations
- Use per-instance named control devices: keep \\.\NoiseFilterVmic for the primary pair exactly as it is and add \\.\NoiseFilterVmicB for Noise Filter Cable B, both speaking today's protocol unchanged. Hold off on a custom interface on the ACX FDO: ACX's create and IOCTL routing for an unknown reference string is undocumented, a handle on the FDO joins query-remove, and the FDO's security descriptor is the one Audiosrv's KS-filter opens use.
- Replace the driver-wide g_NfAppRing, g_NfAppHistory, g_NfStreamStats and g_NfCaptureTrace with a static per-cable slot array chosen by hardware ID. Keep g_NfStreamSerial driver-wide so (cable, stream) labels are unique. Drop the AppFeed gates so B's streams record into B's own slot.
- Add NF_VMIC_IOCTL_CABLE_INFO (0x00226008, METHOD_BUFFERED, FILE_READ_ACCESS) for the cable's identity: role, generation, microphones, instance ID and an optional build stamp. Append CableId and Generation plus a REDACTED flag to STATS (StatsBytes grows; no version bump). Coordinate the offsets with the decision-(4) workflow, which also appends to STATS.
- Split the access with SDDL D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU) and the existing IOCTL access bits: STATS and CABLE_INFO open to interactive users, while ATTACH still needs write access, so only the broker or an elevated app can feed. Capture each handle's write access in a new EvtDeviceFileCreate, withhold AttachedPid from read-only handles, rate-limit read-only STATS per cable, and have diagnostics readers open, read and close each poll.
- Ours: driver slots refactor (D1), CABLE_INFO, access split and README (D2), protocol.rs/device.rs/cables.rs (P1), nf-vmic-test --cable, cables, stats --cable all and an isolation checker plus iso.sh (T1). Astra's: broker per-cable START/STOP through a compiled-in table (B1), the engine's direct_cable setting, RingOutput::open(cable) and a loop guard by identity (E1), and the GUI's per-cable route plus an optional combined Diagnostics card polled at most once a second on a control thread (G1).
- Run the acceptance test in phases under Verifier, 3 rounds each. P0a/P0b (tool-fed B with A plain, then swapped) and P1w (the app with an effect into B over WASAPI) are possible before the broker change. P1/P2 (direct to B, then direct to A) come after the broker and engine work. P3 removes B mid-feed while A verifies; P4 checks unprivileged reads. Use distinct reader periods (160 and 480 frames) so each cable's timer-pass counts can only match its own streams, and check that no stream serial appears in both cables' logs.
- Before closing D1, re-run A's full live regression set (three and four recorders, kill-test 10 of 10, ten minutes, disable/enable with a handle held), and run spike check 4's removal cases on B while A is attached.

## Open questions
- Discovery: fixed per-cable control-device names plus a CABLE_INFO IOCTL (ours, proposed), or PnP-native discovery (a raw PDO per cable, or a discovery-only interface on each FDO) for arrival notifications, at the cost of query-remove vetoes and more PnP code?
- Who may read diagnostics without UAC: interactive users (IU, proposed), authenticated users, or administrators only? With IU, another signed-in user can see when this user's microphone streams run, even with the pid withheld.
- Should AttachedPid be withheld from read-only handles (proposed), and should read-only STATS be rate-limited per cable (proposed: one per 10 ms, ERROR_BUSY beyond), or neither?
- May STATS fill a prefix for a shorter buffer, so older readers such as the GUI's diagnostics keep working as the struct grows (proposed)? It would count as a diagnostic change under your 2026-11-23 rule, so no NF_VMIC_VERSION bump.
- When B is removed and added again, should its counters and logs stay cumulative since the driver loaded, with a generation number (proposed), or reset?
- Names: is \\.\NoiseFilterVmicB acceptable, and '--cable a|b' in nf-vmic-test, given that 'Cable A/B' also means VB-CABLE's A and B in the docs?
- May the broker hold one direct feed per cable at once (proposed), even though the app runs one filter chain per engine for now?
- For a direct feed, should the app refuse only the fed cable's own microphones as its input (proposed), or every one of our microphones as today?
- Acceptance: can the WASAPI-into-B run (P1w) count as the first 'B running effects' run before the direct-B route lands? Is nf-fx's gain example enough as 'effects'? Is runas /trustlevel:0x20000 acceptable for the unprivileged-read check, or only your standard-user run?