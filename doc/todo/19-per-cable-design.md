## TODO 19 design: any number of cables, each with its own feed and diagnostics

**Status: the owner answered its questions (2026-11-24, with Astra; below), and asked for three revisions before it is
built: discovery without hard-coded cables, diagnostics reset when a cable is re-created, and no compatibility promise
(pre-release). Revised below (ours, 2026-11-24, for the owner's review); nothing built.**

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

The design below is ours (a workflow's read-only design, 2026-11-24, on main at 9d7fd98), written to the decisions above;
anything not attributed to the owner is a proposal until the owner agrees. Two public sources were read earlier for this
file (WDF's `FxPkgGeneral`, microsoft/low-latency-audio's ACX driver); this revision rechecked the WDK 10.0.28000 headers on
the dev box (KMDF 1.35, ACX 1.1) and Learn pages.

**Marks.** **[V: source]** verified: a header on the dev box (KMDF headers under `Include\wdf\kmdf\1.35`, the rest under
`Include\10.0.28000.0`), a Learn page, our code, or a live run recorded in `19-spike-2026-11-20.md`. **[C]** to confirm on
the box. **Words.** A *cable* is one device node of the one driver package: a speaker endpoint and one or two microphones
looped back. Its *label* is its letter ("D"). Its *identity* is a GUID that outlives removal. A *binding* is one life of its
framework device, from AddDevice to context cleanup; its *generation* tells bindings apart.

### Summary
- **Discovery.** Each cable registers a discovery-only device interface of our own class. Its properties carry the
  identity, label, generation and control path. User mode lists the interfaces with `CM_Get_Device_Interface_ListW` and
  reads the properties; nothing ever opens the interface.
- **One control device per cable, named from its identity**, `\\.\AcxLlCable_<id>`. `\\.\NoiseFilterVmic` goes. Labels,
  ids and paths are aliases that nf-vmic resolves through discovery.
- **All per-cable state in the device context.** The app ring, its copy, the counters, the skip and underrun logs, the
  timer statistics, the capture trace and the statistics snapshot. No global table; only the stream-serial counter stays
  global.
- **Identity in the device's hardware key**, written by the setup tool before the first start. A setup-owned store
  remembers it across removal. The generation is the binding's start time, and each binding starts its counters at zero.
- **Statistics.** A new layout with its own magic and version. One shared snapshot per cable, at most 10 ms old, redacted
  for read-only handles. A write-access IOCTL gives the elevated tools a fresh, unredacted one.
- **Access.** The SDDL adds read for interactive users. Feeding still needs write access, held only by SYSTEM and
  Administrators, so it goes through the elevated broker.
- **Names.** Every model's description is "ACX-LL-Cable", and each instance's friendly name is "ACX-LL-Cable D", so its
  endpoints read "Microphone (ACX-LL-Cable D)". The three microphone-variant hardware IDs are shared by every cable.
- **Setup.** One Rust implementation (nf-vmic's `setup`, a small `acx-ll-cable` tool) behind `Cable.ps1`,
  `Install-Driver.ps1` and the variant switch. A driver update moves every cable together.
- **The audio protocol is unchanged**: ATTACH v1 and v2, the ring header and its rules.

### 1. What is there today (verified in code, 9d7fd98)
- **One control device.** `Control.cpp` makes `\Device\NoiseFilterVmic` (SDDL SY and BA). `s_DeviceCount` is 0 or 1; a
  second primary fails AddDevice with STATUS_DEVICE_BUSY. `Device.cpp` recognises `ROOT\NoiseFilterCableB` by hardware ID,
  and B never registers with the feed.
- **Driver-wide globals** in `AppRing.cpp`: `g_NfAppRing`, `g_NfAppHistory` (256 KiB .bss), `g_NfStreamStats`,
  `g_NfCaptureTrace`, `g_NfStreamSerial`. The stream engine gates every record on `m_Loopback->AppFeed`.
- **Already per device:** the `LOOPBACK_RING` in `CODEC_DEVICE_CONTEXT`, with its counters and both event logs. B's exist
  but no IOCTL reads them.
- **INF:** models `ROOT\AudioCodec` (both microphones), `...Call`, `...LowLatency`, and `ROOT\NoiseFilterCableB` (its own
  sections: Speaker0 and Microphone0). Descriptions "AudioCodec Device" and "Noise Filter Cable B". B's circuits have their
  own component GUIDs (`DriverSettings.h`, 09e5520; the reason isn't recorded).
- **Endpoint names follow the device's description**, not the interfaces' FriendlyName ("AudioCodec Speaker"):
  "Microphone (AudioCodec Device)", "Speakers (Noise Filter Cable B)" (sessions.rs, Cable-B.ps1, nf-gui). The engine's loop
  guard reads that adapter part as the endpoint's interface friendly name (`audio.rs:401`). The pin-name callback
  (`CaptureCircuit.cpp`) names "Call Microphone".
- **Setup:** `Install-Driver.ps1` (devcon install of a new device; `pnputil /add-driver /install` to update; removes the
  other variants), `Cable-B.ps1` (devcon install of B from A's installed INF, a file lock, the default-role restore), and
  `variant.rs` (re-creation through SetupDi with `DICD_GENERATE_ID`, `DiInstallDevice` from the driver store, a lock in a
  private namespace bounded by Administrators).
- **User mode:** `protocol::USER_PATH` is fixed; the broker helper holds one `Option<Attachment>`; `RingOutput::open()`
  calls `attach_live(LIVE_CAPACITY)`; `sessions::is_ours` matches "AudioCodec Device" by name.

### 2. Discovery: one interface per cable, never opened
**Driver, in AddDevice after `WdfDeviceCreate`:**
1. `WdfDeviceCreateDeviceInterface(device, &GUID_DEVINTERFACE_ACX_LL_CABLE, L"AcxLlCable")`, a new interface class GUID of
   ours. Called from EvtDriverDeviceAdd, the framework enables the interface when the device starts and disables it when it
   stops [V: Learn, WdfDeviceCreateDeviceInterface, Remarks]; PASSIVE_LEVEL [V: wdfdevice.h]. WDF does the enabling, which
   `IoRegisterDeviceInterface` would leave to us. The interface has to be the FDO's: a control device can't have one
   [V: Learn, WdfDeviceRetrieveDeviceInterfaceString: STATUS_INVALID_DEVICE_REQUEST for a `WdfControlDeviceInitAllocate`
   device]. The reference string only keeps the instance apart from ACX's own interfaces; nothing opens it.
2. `WdfStringCreate`, then `WdfDeviceRetrieveDeviceInterfaceString` (PASSIVE [V: wdfdevice.h]; STATUS_INVALID_DEVICE_STATE
   if the system hasn't assigned a link yet [V: Learn]). Then `IoSetDeviceInterfacePropertyData` for each key below, with
   LOCALE_NEUTRAL and Flags 0 (PASSIVE in wdm.h; "IRQL <= APC_LEVEL in the context of a system thread" on Learn [V]). Its
   link may come from `IoRegisterDeviceInterface` [V: Learn, parameter text], so a registered interface that isn't enabled
   yet is a valid target. Set at every AddDevice, before the start enables the interface, so an interface that's listed
   always carries its own binding's values [C: values set in AddDevice are read back once the interface is enabled;
   fallback: set them in EvtDevicePrepareHardware, which also precedes the enable].
3. A failure is logged and costs discovery only: the cable still plays as a plain cable. Today a control-device failure
   likewise costs only the feed.

**Properties** (`DEVPKEY_AcxLlCable_*`, one property-set GUID of ours; types [V: devpropdef.h]):

| pid | Name | Type | Value |
| --- | --- | --- | --- |
| 2 | CableId | DEVPROP_TYPE_GUID | the identity |
| 3 | Label | DEVPROP_TYPE_STRING | "D" |
| 4 | Generation | DEVPROP_TYPE_UINT64 | the binding's start (section 4) |
| 5 | ControlPath | DEVPROP_TYPE_STRING | `\\.\AcxLlCable_<id>`, empty if there is no control device |
| 6 | ControlStatus | DEVPROP_TYPE_NTSTATUS | STATUS_SUCCESS, or why there's no control device |
| 7 | DiagnosticsVersion | DEVPROP_TYPE_UINT32 | NF_STATS_VERSION (section 6) |

**User mode** (nf-vmic `cables::list`, any user, no elevation):
1. `CM_Get_Device_Interface_List_SizeW`, then `CM_Get_Device_Interface_ListW(&GUID_DEVINTERFACE_ACX_LL_CABLE, NULL, ...,
   CM_GET_DEVICE_INTERFACE_LIST_PRESENT)` [V: cfgmgr32.h:2852, 2784, 985]. Retry while it returns CR_BUFFER_SMALL: a cable
   can arrive between the two calls.
2. For each interface, `CM_Get_Device_Interface_PropertyW` [V: cfgmgr32.h:2910] for the six keys, and
   `DEVPKEY_Device_InstanceId` [V: devpkey.h:86] for the device node that exposes it [C: an interface answers that key;
   fallback: parse the instance ID from the path, as `sessions::parse_filter` does for KS filters]. A standard user can
   read them [C].
3. **Endpoints:** MMDevice enumeration, render and capture, every state. Each endpoint's KS filter path gives an instance
   ID and a reference string (`sessions::identify`, extended to render's Speaker0), which name the cable and the endpoint
   (Speaker0 Speakers, Microphone0 Microphone, Microphone1 Call Microphone). This is the documented device topology route
   first, then the property-store copy, as today.
4. **The feed state:** open ControlPath read-only, send CABLE_INFO, close (section 6). A caller that can't open it (a
   service without the interactive SID) gets "unknown".

**No I/O goes through the interface.** Nothing of ours ever opens it. An open would reach ACX's create dispatch for the
FDO, whose handling of an unknown reference string isn't documented (WDF hands a create to the class extension first;
`FxPkgGeneral::OnCreate`, read earlier for this file), and a handle on the FDO takes part in query-remove. [C: what an open
of the path does. Nothing depends on it.]

**Arrival notifications wait (the owner).** Later, `CM_Register_Notification` with a device-interface filter for this class
[V: cfgmgr32.h:4657]. Until then nf-vmic re-lists when asked; the app re-lists when its MMDevice notifications show
endpoints come or go, and when a cable picker opens.

### 3. Each cable's control device
- **Name from the identity:** `\Device\AcxLlCable_<id>`, link `\DosDevices\AcxLlCable_<id>`, opened from user mode as
  `\\.\AcxLlCable_<id>`. `<id>` is the GUID's 36 lower-case characters, formatted at PASSIVE in AddDevice with ntstrsafe,
  which `CaptureCircuit.cpp`'s name callback already uses.
- **Lifetime, per instance:** made in the cable's AddDevice, deleted in its context cleanup. This is today's lifetime with
  the device count and `s_CdoLock` gone, since no state is shared. The link is still made and deleted by hand (today's
  reason: a deleted control device lives until its last handle closes). With a handle held across a disable and enable,
  the new control device got the name and the old handle failed with ERROR_BAD_COMMAND [V: live, 19-spike, check 4].
- **A name collision** means another device node carries the same identity (a setup error). It costs that binding's feed
  only: ControlStatus STATUS_OBJECT_NAME_COLLISION, ControlPath empty, the cable plays as a plain cable. `cables` and the
  setup tool report the duplicate.
- **The control device's context** is `NF_CONTROL_CONTEXT { EX_RUNDOWN_REF Rundown; PNF_CABLE Cable; }`, nonpaged
  [V: Learn, Framework Object Context Space]. Every callback reaches the cable through it: the IOCTL callback through its
  queue's device; the cancel routine and EvtFileCleanup through `WdfRequestGetFileObject` and `WdfFileObjectGetDevice`
  (<= DISPATCH [V: wdffileobject.h:119]). Each takes rundown protection first (callable at DISPATCH when the block is
  nonpaged [V: Learn, ExAcquireRundownProtection]).
- **Deletion, in the cable's context cleanup at PASSIVE:**
  1. Set the ring's Removing under its lock.
  2. Delete the link.
  3. `WdfIoQueuePurgeSynchronously`: the pending ATTACH ends with STATUS_DEVICE_REMOVED, and the call returns once every
     delivered request has completed (today's code, with its WDF-source citation).
  4. `ExWaitForRundownProtectionRelease` (<= APC_LEVEL [V: wdm.h]). After it, a file cleanup on the lingering deleted
     control device is refused protection and touches nothing.
  5. `WdfObjectDelete`, then free the cable's pool buffers.

  The guard is new because the cable now lives in the PnP device's context, which goes with the binding, while a deleted
  control device lives on until its last handle closes. The ring used to be static for exactly that reason (AppRing.h).
- **No EvtDeviceFileCreate.** The IOCTLs' access bits decide what a handle may do (section 7). The earlier draft's
  reliance on `PreviouslyGrantedAccess` at create is gone.
- **Queue:** parallel and not power-managed, as today, with `ExecutionLevel = WdfExecutionLevelPassive`. Queues take it
  from KMDF 1.9, and the framework then calls their callbacks only at PASSIVE_LEVEL [V: Learn, WDF_EXECUTION_LEVEL]. STATS
  needs that for its fast mutex (section 6).
- **A keeps no special name; `\\.\NoiseFilterVmic` goes.** One scheme is clearer, and nothing in the driver makes A
  special any more: any cable can be fed. A tool built for the old name fails at open with ERROR_FILE_NOT_FOUND instead of
  misreading a new layout. Pre-release, the owner welcomes the break. The aliases the owner listed (`\\.\NoiseFilterVmicB`
  style names, `--cable b`) are served in user mode: nf-vmic's selector takes a label, an id or a control path and
  resolves each through discovery. No kernel alias link: it would be a second global name per cable, made and deleted by
  hand, stale once a label changes, and wanted by no client that discovery doesn't already serve.

### 4. Identity, labels and generations
- **Where it lives:** two values in the device node's hardware key (PLUGPLAY_REGKEY_DEVICE, the key `NfMicrophones` is read
  from today [V: Device.cpp, live with the variants]). `NfCableId` (REG_SZ, "{GUID}") and `NfCableLabel` (REG_SZ, one to
  eight of A-Z and 0-9; the setup gives single letters).
- **Who writes it:** the setup tool, after registering the device node and before installing its driver.
  `SetupDiCreateDevRegKeyW(DIREG_DEV)` slots in between DIF_REGISTERDEVICE and DiInstallDevice in variant.rs's sequence
  [C: the key can be made and written before the install, and the INF's `.HW` AddReg and later driver updates leave the
  values alone]. The identity outlives removal in a store the tool owns, `HKLM\SOFTWARE\ACX-LL-Cable\Cables\<label>` (`Id`,
  `Mics`). Adding a label reuses its identity; removing keeps it; `remove --forget` drops it. Removal deletes the device
  key with the device, which is why the store exists.
- **Who reads it:** AddDevice, before `WdfDeviceCreate`. `WdfFdoInitOpenRegistryKey(PLUGPLAY_REGKEY_DEVICE, KEY_READ)`
  [V: wdffdo.h:162], `WdfRegistryQueryUnicodeString` (PASSIVE [V: wdfregistry.h:419]), `RtlGUIDFromString` (PASSIVE
  [V: wdm.h:15218]).
- **A missing or invalid value fails AddDevice** with STATUS_DEVICE_CONFIGURATION_ERROR (ours). Then every working cable
  has an identity, and discovery, the control device's name and the setup never meet an unnamed one. A device made by hand
  (`devcon install`) shows a problem code [C: which] until `Cable.ps1 add <label> -Adopt <instance>` writes the values and
  restarts it.
- **The instance ID** is read with `WdfFdoInitQueryPropertyEx(DEVPKEY_Device_InstanceId)` [V: wdffdo.h:295] for
  CABLE_INFO. It isn't the identity: devcon and variant.rs's `DICD_GENERATE_ID` give each new device node a new
  `ROOT\MEDIA\NNNN` [V: variant.rs, step 3].
- **Generation** is `KeQuerySystemTimePrecise` at AddDevice [V: wdm.h:25572]: UTC in 100 ns units, unique per binding, and
  it says when the binding began. Readers compare it for equality only, since the clock can be set back. `BindQpc` goes
  beside it. Nothing is persisted and nothing is global.
- **A binding is one framework device**, AddDevice to context cleanup. A disable and enable, a driver update, a variant
  switch and a removal and re-add each make a new one. Its counters and logs start at zero because they live in the
  context, which the framework zeroes [V: Learn, Framework Object Context Space]. Event Sequence and Attach count from 1.
- **"A's continue"**, as we read it: nothing of A's binding is touched while another cable is removed or re-added
  (per-cable state, no global, no shared lock). A's counters run on through B's removal, and run P3 checks it. A's own
  restart is a new binding of A, like any cable's, and the GUI's history joins the generations, marking each restart. If
  the owner meant that A's counters survive A's own restarts, that is open question 1.

### 5. Per-cable state in the device context
`CODEC_DEVICE_CONTEXT` gains `NF_CABLE Cable` beside `LOOPBACK_RING Loopback`, both nonpaged [V: Learn]. It holds:
- **The binding:** Id, Label, Generation, BindQpc, Microphones, `InstanceId[200]` (MAX_DEVICE_ID_LEN [V: cfgmgr32.h:55]),
  and the circuits' component IDs (section 8).
- **The ring:** today's `NF_APP_RING`: the attachment, the reader slot, the copy's bookkeeping, segments, claims,
  OtherFormatCaptureOpens, the counters, Removing. Its Loopback pointer is fixed at AddDevice to the same context's ring;
  the owner bookkeeping goes.
- **The copy:** `History`, 256 KiB from nonpaged pool, allocated in AddDevice and freed in context cleanup, as
  `Loopback.Buffer` is.
- **Timer statistics and the capture trace:** `StreamStats[2]` and `CaptureTrace`, interlocked as today.
- **The jump totals** (SecondaryJumps, SecondaryJumpFrames) move from `LOOPBACK_RING` into the ring's counters. They are the
  copy's, and are already counted under the ring's lock.
- **Control and discovery:** the control device, its queue, ControlStatus, a created flag, and the discovery status.
- **The snapshot:** a FAST_MUTEX, its QPC, and an `NF_CABLE_STATS` (6,896 bytes).
- **An info word:** ATTACHED and LIVE, interlocked, for CABLE_INFO.

Each cable costs about 1.05 MB of nonpaged memory: the loopback ring's 768,000 bytes, the copy's 262,144, and about 20 KB
of context.

**Reaching the state:**
- **Streams:** `CableOfCircuit(circuit)`, as `LoopbackRingOfCircuit` does today (`AcxCircuitGetWdfDevice`, <= DISPATCH, as
  StreamEngine.cpp cites). `m_Cable` is set in the constructor. `&g_NfAppRing` becomes `&m_Cable->Ring` and
  `g_NfAppHistory` becomes `m_Cable->History`. `NfVmicRecord*` and `NfVmicTrace*` take the cable. The AppFeed gates go, so
  every cable records into and reads its own ring. `CaptureStreamOpen` and `CaptureStreamClose` take the cable. Assert that
  a stream's `m_Cable` and `m_Loopback` share one context.
- **The control device:** through `NF_CONTROL_CONTEXT` (section 3).
- Nothing global points at any cable.

**What stays global, and why.** `g_NfStreamSerial`, one interlocked LONG. It gives streams labels that are unique across
cables within a driver load, so "A #17" and "B #17" can't both exist, and the merged log and the isolation check stay
unambiguous. It's a label, not a counter, so it doesn't reset. Read-only constants and `g_RegistryPath` stay too. Gone:
`s_CdoLock`, `s_DeviceCount`, `s_ControlDevice`, `s_ControlQueue`, `g_NfAppRing`, `g_NfAppHistory`, `g_NfStreamStats`,
`g_NfCaptureTrace`, `NfVmic_DriverInit`, `LOOPBACK_RING.AppFeed` and `CODEC_DEVICE_CONTEXT.VmicRegistered`.

**Lock order, per cable:** the ring's lock, then the loopback's, then a log's, as today. The snapshot's fast mutex comes
above them all: it is held at APC_LEVEL while the refresh takes the spin locks, and never taken under one. Rundown
protection isn't a lock: nothing waits under it. No path holds two cables' locks.

### 6. Statistics: versions, layout, snapshot, redaction
**Versions.**
- **The audio protocol is unchanged:** NF_VMIC_VERSION 1 and NF_VMIC_LIVE_VERSION 2, for ATTACH, its input, the ring
  header and the ring's rules. Per-cable devices change nothing in how audio is exchanged; finding a cable isn't
  exchanging audio. The rule texts in `nf_vmic.h` change wording only: a cable's microphones share that cable's ring, not
  the driver's one ring.
- **Diagnostics get their own:** NF_STATS_MAGIC ("NFST") and NF_STATS_VERSION 1. It covers STATS, STATS_FULL, CABLE_INFO
  and the discovery properties, and changes with any layout change. A mismatched tool is told to update, with both
  versions named; the driver's version comes from the discovery property, read without any I/O.

**IOCTLs** (FILE_DEVICE_UNKNOWN; the values follow CTL_CODE):

| Function | Value | Method | Access | Purpose |
| --- | --- | --- | --- | --- |
| ATTACH 0x800 | 0x0022E002 | OUT_DIRECT | read, write | unchanged |
| STATS 0x801 | 0x00226004 | BUFFERED | read | the shared snapshot, redacted for this handle |
| CABLE_INFO 0x802 | 0x00226008 | BUFFERED | read | the binding's facts and the feed state |
| STATS_FULL 0x803 | 0x0022E00C | BUFFERED | read, write | a fresh snapshot, unredacted |

The three diagnostic IOCTLs take `NF_STATS_IN {Magic, StatsVersion, InBytes (16), Flags (0)}`. Magic and version are
checked before anything else, as ATTACH does. Another version gets STATUS_REVISION_MISMATCH (ERROR_REVISION_MISMATCH,
1306). The output must be exactly the struct, or STATUS_INVALID_BUFFER_SIZE.

**`NF_CABLE_STATS`** (6,896 bytes; every counter is the binding's, from zero; times in 100 ns units):

| Offset | Size | Group | Fields |
| --- | --- | --- | --- |
| 0 | 64 | Header | Magic, StatsVersion, StatsBytes, Flags; QpcFrequency; QpcSnapshot (read last); CableId; Generation; BindQpc |
| 64 | 32 | Attachment | Pid (0: none, or REDACTED), CapacityFrames, ReadFrame, WriteFrame (untrusted), ReaderPacketFrames, Version (1, 2; 0 none) |
| 96 | 128 | Ring | today's NF_VMIC_RING_STATS, in its order |
| 224 | 64 | Copy | SecondaryFramesCopied, SecondaryUnderrunFrames, ReaderHandoffs, HandoffSkippedFrames, SecondarySkips, SecondarySkippedFrames, SecondaryJumps, SecondaryJumpFrames |
| 288 | 112 | Loopback | today's twelve, then LeadSkips and LeadSkippedFrames |
| 400 | 256 | Stream[2] | NF_VMIC_STREAM_STATS: render, capture |
| 656 | 64 | CaptureTrace | today's |
| 720 | 32 | Logs | SkipEventsLogged, UnderrunEventsLogged, SkipLogEntries (64), UnderrunLogEntries (32), EventBytes (64), 4 spare |
| 752 | 4,096 | SkipLog | NF_VMIC_EVENT[64]: event n in slot (n - 1) % 64 |
| 4,848 | 2,048 | UnderrunLog | NF_VMIC_EVENT[32] |

Flags: ATTACHED 0x1, READING 0x2, FORMAT_MISMATCH 0x4, LIVE 0x8 (the attachment is a live one), REDACTED 0x10. Against
today's layout:
- its own magic and version, with the identity and generation in the header;
- the copy's counters together, where today they're scattered and the jumps sit in the loopback's group;
- the loopback's lead skips with the loopback's counters;
- the logs' descriptor before the logs, and the Reserved holes gone;
- no LOOPBACK flag, since a cable's statistics always have their device;
- counters since the binding, not since the driver loaded.

**`NF_CABLE_INFO`** (528 bytes):

| Offset | Size | Fields |
| --- | --- | --- |
| 0 | 16 | Magic, StatsVersion, InfoBytes, Flags (ATTACHED, LIVE) |
| 16 | 16 | CableId |
| 32 | 24 | Generation, BindQpc, QpcFrequency |
| 56 | 8 | Microphones (bit 0 Microphone0, bit 1 Microphone1); ProtocolMin (u16, 1), ProtocolMax (u16, 2) |
| 64 | 32 | Label: 16 UTF-16 units, NUL-padded |
| 96 | 32 | BuildId: 32 ASCII bytes, NUL-padded (section 9) |
| 128 | 400 | InstanceId: 200 UTF-16 units |

CABLE_INFO takes no spin lock: the facts don't change during a binding, and the feed state is the interlocked info word.
A reader looping on it can't contend with the capture DPC. nf-vmic checks after each open that its Id, Generation and
InstanceId match the discovered interface, which catches a duplicate or a re-bind between listing and opening.

**The snapshot (the owner's 10 ms rule).** STATS takes the cable's fast mutex (raises to APC_LEVEL [V: wdm.h:27081]):
1. If the snapshot is 10 ms old or more by QPC, or was never taken, refresh it. That is today's STATS copy: the ring's
   lock, the loopback's, the logs, with QpcSnapshot read last.
2. Copy it into the request's buffer and release the mutex.
3. Redact the copy for a read-only handle.

Readers arriving together: the first refreshes, and the others wait microseconds on the mutex and get the same
QpcSnapshot. No ERROR_BUSY, and no reader can starve another. However many read-only readers there are, a cable's snapshot
refreshes at most 100 times a second. That bounds the contention with the capture DPC and, under Verifier, the
spin-lock-raise trims found in 1c8d9d4 (TODO 13).

STATS_FULL always refreshes. Only write-access handles can send it: the elevated tools and the broker. nf-vmic-test's
verify and stamps need statistics current to the packet, which a 10 ms cache would hide. The GUI refreshes once a second
(the owner).

**Redaction.** On a read-only handle, STATS returns AttachedPid 0 and sets REDACTED. Nothing else in STATS, CABLE_INFO or
the discovery properties names a process, session, user, image or handle, and STATS carries no audio samples, only
positions and counts [V: the layouts above].

### 7. The access split
- **SDDL** `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)`, with FILE_DEVICE_SECURE_OPEN as today. IU is "users who initially
  logged onto the machine interactively, such as local logons and Remote Desktop logons" [V: wdmsec.h:326].
- **The mechanism:** the I/O manager checks an IOCTL's access bits against the handle before the driver sees the request,
  and FILE_READ_ACCESS is FILE_READ_DATA [V: wdm.h]. A GENERIC_READ handle passes STATS and CABLE_INFO; ATTACH and
  STATS_FULL fail on it at DeviceIoControl with ERROR_ACCESS_DENIED. A non-admin's read-write open still fails at
  CreateFile with ERROR_ACCESS_DENIED, which the broker fallback in `broker::attach_live` relies on [V: code: the SDDL
  grants write to SY and BA only, and broker.rs falls back on that error; a real standard account is run R's].
- **Feeding:** write access means SYSTEM and Administrators: the elevated broker helper or an elevated app. The broker's
  approval (a UAC prompt) is unchanged.
- **The trade-off (the owner's).** Another signed-in user, Remote Desktop included, can open any cable read-only and see:
  that it exists; whether a feed is attached; when recorders run (passes, the reader's packet); and its skips and
  underruns, with times. They can't hear the audio, see the feeder's process id, feed, or detach. The README and the GUI's
  help say so.
- **Sandboxed (AppContainer) processes** match no entry and can't read [C, if it matters].
- [C] A non-admin read-only open passes STATS and gets ERROR_ACCESS_DENIED on STATS_FULL and ATTACH, and the
  `runas /trustlevel:0x20000` token is non-admin and carries IU (run P4).

### 8. Names, hardware IDs and variants
- **INF:** the description "ACX-LL-Cable" for every model, the manufacturer "ACX-LL-Cable" (ours; TODO 18 settles the
  provider and package names), and interface FriendlyNames "ACX-LL-Cable Speakers", "... Microphone", "... Call
  Microphone".
- **Hardware IDs, shared by every cable:**
  - `ROOT\AcxLlCable`: Speaker0, Microphone0 and Microphone1;
  - `ROOT\AcxLlCableCall`: Speaker0 and Microphone1;
  - `ROOT\AcxLlCableLowLatency`: Speaker0 and Microphone0.

  `NfMicrophones` comes from the `.HW` AddReg as today, and B's model and sections go. Any cable may take any variant: the
  hardware ID picks the microphones and the hardware key picks the cable.
- **The letter: a per-instance FriendlyName**, "ACX-LL-Cable D", set by the setup tool with
  `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` before the install. Windows names an endpoint "<endpoint
  description> (<adapter>)", and today the adapter part is the device's description [V: our code and live names]. With a
  FriendlyName it should be that, and per-instance names also end Windows' "2- " numbering [C: the FriendlyName survives
  the install and names the endpoints when they're made]. Fallback [C]: the driver sets `DEVPKEY_Device_FriendlyName` from
  the label in AddDevice with `WdfDeviceAssignProperty` (<= APC [V: wdfdevice.h]). Learn names no forbidden keys, but we
  haven't seen a driver set this one.
- **The pin-name callback is unchanged.** It names "Microphone" and "Call Microphone", the render pin's category gives
  "Speakers", and the letter is in the adapter part. So: "Speakers (ACX-LL-Cable A)", "Microphone (ACX-LL-Cable A)", "Call
  Microphone (ACX-LL-Cable A)", "Speakers (ACX-LL-Cable B)" and so on.
- **Component IDs per cable:** the identity XOR each circuit's constant (today's `CODEC_*_COMPONENT_GUID`s), kept in the
  context; the `CABLE_B_*` GUIDs go. B got its own in 09e5520 and the sample says the ID "uniquely identifies the circuit
  instance" [V: DriverSettings.h]; with any number of cables, only a derived ID scales [C: nothing objects to derived IDs].
- **A's two microphones:** `Install-Driver.ps1` makes A with `both` (today's default; the owner's option A, 2026-11-23).
  `add` also defaults to `both` for every label (ours: one rule for all cables, and the call microphone's 5 ms default
  period suits Discord on any cable; a user may disable one in the Sound panel, as decided for A). `--mics` picks another.
  Open question 3.
- **The variant switch, per cable:** `noise-filter mic variant --cable B call`, or `Cable.ps1 variant B call`. It
  re-creates that cable's device node with the new hardware ID, carrying its identity, label and FriendlyName (a new
  binding), and touches no other cable. See section 9 on which image it loads.
- **Docs and GUI:** "ACX-LL-Cable B" beside "VB-CABLE B" (the owner).
- **Code:** `sessions::is_ours` and `identify` go by identity (instance ID to cable), with the name only as a display
  fallback. variant.rs's `DEVICE_NAME`, `MICROPHONE` and `CALL_MICROPHONE` strings go.

### 9. Setup: one implementation, the scripts, update and uninstall
**One implementation in Rust:** nf-vmic's `setup` module (ours), extracted from variant.rs. It finds our devices, takes the
privileged lock (now for every setup action), picks the driver from the store, creates and removes. A small elevated tool,
`acx-ll-cable` (a bin in nf-vmic; no audio, no app), runs it for plain-cable users, who need neither the app nor the
broker. The release installer (TODO 18) and the scripts run the same tool; `noise-filter mic variant` calls the library.

**The tool's commands:**
- **`list`** (any user): present cables (discovery, plus every device node with one of our hardware IDs, so unconfigured
  ones and ones with a problem show too), remembered absent cables (from the store), and duplicates.
- **`add <label> [--mics both|call|low-latency] [--adopt <instance>]`**, under the lock:
  - present and healthy: no change, so `add` is idempotent;
  - present but unhealthy: re-created with the same identity;
  - absent: the identity comes from the store or is new (`UuidCreate`). Then create the node (`SetupDiCreateDeviceInfoW`
    with `DICD_GENERATE_ID`, the hardware ID, the FriendlyName, DIF_REGISTERDEVICE, the DIREG_DEV values) and install the
    store's newest package with `DiInstallDevice`. These are variant.rs's steps 3-5 plus the key [V: live, the variant
    switches of 03c477a]. Wait for it to work and for its endpoints (MICS_WAIT), then record it in the store.

  `add` never restarts another cable.
- **`remove <label> [--forget]`:** `DiUninstallDevice` the device nodes with that identity. A feed on it ends with
  ERROR_DEVICE_REMOVED (today's path). The store entry stays unless `--forget`.
- **`variant <label> <mics>`.**
- **`update <package>`:** every cable to the package (below).

**`driver/scripts/Cable.ps1`** (dev; replaces Cable-B.ps1): `Cable.ps1 list | add <L> [-Mics m] [-Adopt id] |
remove <L> [-Forget] | variant <L> <m>`. It runs the tool (`cargo build --release -p nf-vmic --bin acx-ll-cable` if it's
missing) and keeps Cable-B.ps1's restore of the default roles Windows moved to a new endpoint (Set-DefaultAudio.ps1)
around `add` and `variant`.

**`Install-Driver.ps1`:**
1. The checks as today.
2. Migration, once: remove the legacy device nodes (`ROOT\AudioCodec*`, `ROOT\NoiseFilterCableB`), which the new INF no
   longer matches.
3. `pnputil /add-driver` (to the store only, no `/install`).
4. If cables are present, `acx-ll-cable update`; otherwise `acx-ll-cable add A --mics <Variant>`. `-Variant` keeps its
   meaning, for A.
5. The status.

**A driver update moves every cable** (our review of 09e5520: cables sharing the service keep the old image loaded).
Every cable runs one service and one image, which reloads only once no device and no control-device handle holds it.
`update` therefore:
1. lists the cables and disables them all (`CM_Disable_DevNode`);
2. waits up to 10 s for the service to stop [C: how to read that, `sc query` or the driver object]. If it doesn't, it names
   the likely holders (an open diagnostics or feed handle: the GUI, the broker, nf-vmic-test) and stops before changing
   anything;
3. installs the package on each (`DiInstallDevice`, still disabled [C]) and enables them all;
4. checks that each runs the package (`DEVPKEY_Device_DriverInfPath` and `...DriverVersion`), and that each cable's
   CABLE_INFO BuildId equals the package's. Build-Driver.ps1 injects `NF_BUILD_ID` (the commit, with a mark for a dirty
   tree) and writes it beside the package. This is the review's proof that the loaded image is the new one.

[C] Whether `pnputil /add-driver /install` alone restarts every cable and reloads the image; the procedure doesn't rely on
it. A variant switch while other cables run re-creates its cable on the image already loaded. That's harmless when the
package hasn't changed; after a new build, run `update` first.

**`Uninstall-Driver.ps1`:** `acx-ll-cable remove --all --forget` (the legacy IDs included), then delete the packages (both
INF names during the migration). `-KeepIdentities` keeps the store. **`Get-DriverStatus.ps1`** shows the tool's `list`.

**Privacy:** cable ids are random GUIDs unique to each machine. Scrub them from logs pasted into docs; write "A's id"
(CLAUDE.md, Privacy).

### 10. The user-mode contract (crates/nf-vmic, ours) and nf-vmic-test
The engine, the broker and the GUI (Astra's) build against this:

```rust
// cables.rs: discovery. Any user, no elevation, no UAC.
pub struct CableId(pub GUID);                     // Display/FromStr: 36 lower-case characters
pub struct Cable {
    pub id: CableId,
    pub label: String,                            // "D"
    pub name: String,                             // "ACX-LL-Cable D": the device's friendly name
    pub generation: u64,                          // the binding's start (100 ns UTC)
    pub instance_id: String,                      // ROOT\MEDIA\0004
    pub control_path: Option<String>,             // None: no control device; control_status says why
    pub control_status: i32,                      // NTSTATUS
    pub diagnostics_version: u32,
    pub endpoints: Vec<CableEndpoint>,            // MMDevice id, name as Windows shows it, kind, active or disabled
    pub feed: Feed,                               // Attached { live: bool } | Free | Unknown
}
pub enum EndpointKind { Speakers, Microphone, CallMicrophone }
pub enum Selector { Label(String), Id(CableId), Path(String) }  // FromStr: "d", "ACX-LL-Cable D", an id, \\.\AcxLlCable_...
pub fn list() -> Result<Vec<Cable>, WinError>;
pub fn find<'a>(cables: &'a [Cable], s: &Selector) -> Result<&'a Cable, FindError>;   // NotFound, Ambiguous(ids)
pub fn cable_of_endpoint(cables: &[Cable], endpoint_id: &str) -> Option<CableId>;     // render or capture, by KS filter

// diagnostics.rs: read-only, any interactive user. Open, read, close each time.
pub fn read(cable: &Cable) -> io::Result<Snapshot>;              // CABLE_INFO + STATS (redacted, at most 10 ms old)
pub struct Snapshot { pub info: CableInfo, pub stats: CableStats }
pub fn merged_log(snaps: &[(&Cable, &Snapshot)]) -> Vec<LabelledEvent>;   // by QPC, labelled "B #17"
pub struct Timeline;                                             // a cable's history; push() marks a restart on a new generation

// device.rs: feeding (write access: elevated, or the broker's helper).
impl Device {
    pub fn open(cable: &Cable) -> io::Result<Device>;            // read+write; checks CABLE_INFO against `cable`
    pub fn info(&self) -> io::Result<CableInfo>;
    pub fn stats_full(&self) -> io::Result<CableStats>;
    // attach and attach_live as today
}

// broker.rs: the contract (the helper's side is Astra's).
pub fn attach_live(cable: &CableId, capacity: u32) -> io::Result<LiveFeed>;
```

**Errors, by Win32 code:**
- ERROR_FILE_NOT_FOUND: the cable is gone;
- ERROR_DEVICE_REMOVED: removed while fed;
- ERROR_BUSY: that cable already has a writer;
- ERROR_ACCESS_DENIED: needs the broker's approval;
- ERROR_REVISION_MISMATCH: update the tool.

`protocol.rs` mirrors the new structs with offset asserts and drops `USER_PATH`. Every `Stats` user changes once (no
compatibility promise).

**nf-vmic-test (ours):**
- `--cable <label|id|path>` on feed, verify, kill-test, latency, stamps, idle-test, open and stats. The default is the cable
  labelled A if present, else the only cable, else an error that lists them. kill-test passes it to its child.
- verify and open resolve their endpoint by the cable's identity (Microphone0, or Microphone1 with `--mic call`), not by
  name. verify also fails a recording whose endpoint belongs to another cable than the fed one, instead of passing
  against the wrong statistics. Its before and after statistics, and stamps', come from STATS_FULL.
- `stats --cable X|all [--watch]`: `all` prints each cable, then the merged, labelled log; `--watch` keeps a Timeline and
  marks restarts. Read-only unless elevated, when it uses STATS_FULL.
- New `cables`: every cable's label, name, id, generation (as a UTC time), instance, control path and status, endpoints,
  feed state and versions. It works unelevated.
- New `isolation` (section 12).
- `noise-filter mic variant|sessions` (nf-app, a thin CLI over nf-vmic) take `--cable` too.

### 11. Astra's parts: requirements and interfaces only
**Broker (B1):**
- START and STOP carry a 16-byte CableId; the replies are unchanged (a code and a section handle).
- The elevated helper resolves the id itself (`cables::list`), opens that cable with `Device::open(&cable)` (which checks
  CABLE_INFO), and attaches LIVE_CAPACITY.
- It holds a map from CableId to Attachment: one writer per cable. A second START for the same cable gets ERROR_BUSY;
  another process's feed gets the driver's STATUS_DEVICE_BUSY. STOP detaches only its own cable, and a feed that ends
  (DEVICE_REMOVED) frees its entry.
- The session checks run as today, at every START. Still no command supplies a path, handle, capacity or duplication
  target: the id only picks among the cables the driver published, and the path comes from discovery, not the caller.
- **The approval's scope, stated in the GUI:** one UAC prompt starts the helper for this app process. It may then feed
  any ACX-LL-Cable present, until the app exits, while this session is the only signed-in interactive one. Nothing
  persists.

**Engine (E1), fan-out:**
- The physical microphone is captured once and goes to N destinations. Each destination's source is the dry mic, the
  shared chain's output, or its own chain; its output is `Direct(CableId)` or `Wasapi(endpoint id)`.
- Each destination has its own output or feeding thread (MMCSS, the real-time rules), fed from the capture through
  lock-free queues. Stopping, reconfiguring, losing a device or underrunning one destination leaves the capture and the
  others running; each retries on its own.
- A direct destination's `LiveFeed` comes from the broker, or for an elevated app from a direct Attachment, which stays on
  the thread that made it (device.rs: `!Send`, since Windows cancels a thread's I/O when it exits). The AEC reference is
  captured once for every chain that uses it.
- Settings name destinations by CableId; labels are for display only. No compatibility promise.

**The input guard:**
- Reject an input whose cable (`cable_of_endpoint`) is one of the destinations' cables: any of its microphones. Another
  cable's microphone is allowed.
- Across all the app's routes, each input cable has an edge to each of its destination cables. Reject a configuration
  that makes a cycle (A -> B with B -> A), and name it.
- Everything goes by identity: renamed endpoints still match, and a WASAPI destination on a cable's Speakers is that
  cable.

**GUI (G1):**
- The cable list from `cables::list()`: name, endpoints, feed state, and the generation as a start time. A destination
  picker per route; the plain-cable hints go by identity.
- A combined diagnostics view: each cable's counters, the merged log labelled "B #17", restarts marked (Timeline). It
  refreshes once a second on a control thread, opening, reading and closing each time; never on an audio thread.
- The approval's scope text.

### 12. Acceptance runs (script plans)
**Common:**
- Verifier on, the box idle, one audio test at a time (CLAUDE.md); 3 rounds a phase unless stated.
- Scripts under `nf-scratch/iso/`, run by the lead or the owner.
- **`nf-vmic-test isolation --fed F --plain P --seconds S [--external] [--expect-gain G]`** reads both cables' STATS_FULL
  before and after. In tool mode it feeds F (`--start 4000000` on B, `100000` on A) and verifies F's recording. It plays the
  signal into P's Speakers (exclusive float render) and decodes P's Microphone. Reader periods differ, 160 frames on F and
  480 on P, so each cable's timer counts can match only its own streams.
- **F's checks:**
  - the same attachment throughout; Attaches +0, or +1 in tool mode;
  - Ring.Passes about seconds x 48000 / ReaderPacketFrames;
  - FramesCopied + UnderrunFrames about Passes x the packet;
  - Stream[CAPTURE].TimerPasses within 2% + 5 of its own recorder's;
  - Stream[RENDER] +0 on the direct route;
  - every log entry is F's attachment, with ring frames in F's fed range.
- **P's checks:**
  - not attached in either snapshot;
  - the ring's deltas (Attaches, Passes, FramesCopied, NotReaderPackets, UnderrunFrames) all 0;
  - Loopback.RenderRestarts +1 and CaptureAnchors at least +1;
  - its streams' counters within tolerance of its own recorders;
  - the recording decodes only P's range: 0 corrupt, 0 repeats.
- **Across them:** no stream serial in both cables' logs.

**Phases:**
- **S0, migration** (after D1 and S1; the owner's go, since it removes today's two devices): Build-Driver.ps1,
  Install-Driver.ps1 (A, both), `Cable.ps1 add B`.
  - Check: `Cable.ps1 list` and `nf-vmic-test cables`, both elevated and unelevated, show A and B with their ids,
    generations, ControlStatus 0 and endpoints "Speakers / Microphone / Call Microphone (ACX-LL-Cable A)" and "... (ACX-LL-Cable B)".
  - Then D1's live regression on A (three and four recorders, kill-test 10 of 10, ten minutes, disable and enable with a
    handle held), and spike check 4's removal cases on B with A attached. Say first: about an hour, box idle.
- **P0a / P0b:** tool-fed B with A plain, then swapped.
- **P1w (the interim milestone, before B1 and E1):**
  - The app's input is a microphone that isn't B's: VB-CABLE's, as in TODO 17's run, or a third cable C, which the input
    guard allows. The chain is off except nf-fx's gain at 0.25, routed by WASAPI into "Speakers (ACX-LL-Cable B)". A is
    plain, carrying the test signal.
  - Check B's microphone as TODO 17 checked A: -12.04 dB against the source, and delay-aligned samples exactly the source
    x 0.25 (a power of two, exact in float32).
  - Check A decodes bit-exact (0 corrupt, 0 repeats) and its ring deltas are 0. `isolation --external` checks the
    counters.
- **P1, direct to B** (after B1 and E1): as P1w, with B's ring deltas showing the app's one attachment and A's ring
  untouched.
- **P2, swapped:** direct to A with the gain, B plain carrying the test signal bit-exact.
- **P3, B removed while A records:** `feed --cable B` (or the app direct to B) beside `verify --cable A --seconds 60`;
  `Cable.ps1 remove B` mid-run.
  - Check: B's feed ends with ERROR_DEVICE_REMOVED; A's verify passes; A's generation is unchanged and its DetachRemoval
    +0.
  - Then `Cable.ps1 add B`. Check: B is back with the same id, a new generation, Attaches 0 and empty logs
    (SkipEventsLogged 0); A's counters continued.
- **P4, a reduced-rights token:** `runas /trustlevel:0x20000`.
  - Check: `whoami /groups` shows IU and no enabled Administrators; `nf-vmic-test cables` and `stats --cable all` succeed
    with REDACTED set and Pid 0; `feed --cable A` fails at open with ERROR_ACCESS_DENIED; STATS_FULL on a read-only handle
    gets ERROR_ACCESS_DENIED.
- **P5, add D with B and C absent while A plays (the owner's acceptance):**
  - `Cable.ps1 remove B`, and C if present. Hash the binaries (nf-vmic-test, the app, the driver image). Start
    `feed --cable A` and `verify --cable A --seconds 60`, and the app with its cable picker open, then `Cable.ps1 add D`.
  - Check within 20 s: D in `cables` with label D, a control path and "Speakers (ACX-LL-Cable D)" and "Microphone
    (ACX-LL-Cable D)"; `feed --cable D` with `verify --cable D` passes at once.
  - Check A: its verify passes and its generation is unchanged.
  - Check the rest: the running app lists D without a restart, the hashes are unchanged, and the default roles are
    unchanged.
- **P6, fan-out** (after E1 and G1): one capture (the stand-in microphone of P1w).
  - Destinations: A dry (direct), B gain 0.25 (direct).
  - Check, recording both at once: A is the source bit-exact, B the source x 0.25.
  - Then stop B's destination, reconfigure it (gain 0.5) and start it again. Check: A's verify passes across each change,
    with its attachment unchanged (Attaches +0).
  - The owner's run on real hardware: OBS on A dry, Discord on B with AEC and the noise stage on.
- **R, release checks (the owner, a real standard account):**
  - diagnostics without UAC: the GUI's view and `nf-vmic-test stats`;
  - direct attach refused without approval: the GUI asks;
  - the broker's approval: one prompt, then both cables can be fed;
  - cancellation: declining leaves WASAPI working, and Stop or closing the app detaches;
  - a second signed-in user sees the feed attached but no process id.

### 13. Work breakdown and order
**Ours (driver, protocol, nf-vmic, nf-vmic-test, scripts):**
- **D1, per-cable driver** (2-3 days): NF_CABLE in the context; per-cable control devices with the rundown guard; the
  identity read in AddDevice; the INF's hardware IDs and names; component IDs; the AppFeed gates gone. STATS keeps today's
  layout for now, read from the cable. Build plus InfVerif.
- **S1, setup** (about 2 days): nf-vmic's `setup` and the `acx-ll-cable` tool; Cable.ps1, Install-, Uninstall- and
  Get-DriverStatus.ps1; variant.rs carrying the identity, per cable. It lands with D1, whose driver refuses a node without
  an identity.
- **D2, diagnostics** (1.5-2 days): the discovery interface and properties, CABLE_INFO, NF_CABLE_STATS with its version,
  the snapshot, STATS_FULL, redaction, the IU SDDL, NF_BUILD_ID, and the `nf_vmic.h` and README texts.
- **P1, nf-vmic** (about 2 days): the protocol mirror, `cables.rs`, `diagnostics.rs`, `Device::open(cable)`,
  `broker::attach_live`'s signature, sessions by identity, `mic --cable`.
- **T1, nf-vmic-test** (about 1.5 days): `--cable`, `cables`, `stats --cable all`, `isolation`, the iso scripts.

**Astra's (broker, engine, GUI):** B1, the broker per cable (about 1 day, then a security review); E1, fan-out and the
input guard (about 3 days); G1, the cable list, destinations and combined diagnostics (about 2 days).

**Order:**
1. S1 with D1.
2. S0 (the owner's go) and D1's live regression.
3. D2 with P1.
4. T1.
5. P0, P1w, P3, P4 and P5.
6. B1 with E1.
7. P1, P2 and P6.
8. G1.
9. R.

When it is built, code comments cite "the owner, 2026-11-24" for the decisions above; the rest is marked ours until the
owner agrees.

### 14. Verifier and IRQL risks
1. **The most-tested code moves.** ATTACH, cancel and cleanup races, and the control device's life, go from globals into
   each context. Keep it mechanical, and re-run the full live set (S0).
2. **The cable's memory now ends with its binding** while a deleted control device lives on. The rundown guard covers
   cleanup after the purge. Everything those paths call is DISPATCH-safe.
3. **One control device deleted while other cables' devices live.** Learn's guidance is about the last PnP device and
   unloading; nothing we've read forbids it [C: re-read "Using Control Device Objects"; run the removal cases on B with A
   attached under Verifier].
4. **New PASSIVE work in AddDevice:** the interface, its properties (in a system thread [V: Learn]), the registry reads,
   the pool allocations (about 1.05 MB a cable) and the control device. A failure in any of the optional ones (interface,
   properties, control device) costs only that feature.
5. **The fast mutex** is taken only in queue callbacks at PASSIVE (the queue's ExecutionLevel), never under a spin lock.
   The refresh raises to DISPATCH under it.
6. **Verifier's trims** (Force IRQL checking) come with each spin lock raised from PASSIVE (1c8d9d4). The snapshot cuts
   read-only STATS to at most 100 raises a second per cable, and CABLE_INFO raises none.
7. **No cross-cable lock nesting.** Verifier's deadlock detection sees more lock instances, not new orders.
8. **ACX and a client interface of another class on its FDO** [C: the device starts, ACX's KS interfaces are unchanged,
   and Audiosrv makes the endpoints].
9. **Paged code:** keep the `noinline` rule on the entry points PAGE code calls that take spin locks (AppRing.h).

### To confirm on the box (collected)
- Discovery properties set in AddDevice are read back once the interface is enabled, by a standard user, and an
  interface answers `DEVPKEY_Device_InstanceId`.
- What an open of the discovery path does (nothing depends on it).
- The hardware key can be written before the install, and installs and updates keep its values.
- The FriendlyName survives the install and names the endpoints; or the driver-set fallback works.
- Derived component IDs are accepted.
- A device with no identity shows which problem code.
- The update procedure: the service stopping, installing on disabled devices, and what `pnputil /install` alone does.
- The access split under IU (P4), and one control device deleted while others live, under Verifier (S0).

### Open questions
1. **"A's continue."** We read it as: A's counters run on while other cables are removed and re-added, and A's own
   restart is a new binding like any cable's, with the GUI joining the history. If you meant A's counters survive A's own
   disable, update or variant switch, that needs state kept outside A's device (a global or a registry carry-over), which
   the per-instance design otherwise avoids. Which did you mean?
2. **Endpoint IDs across re-adding.** The cable's identity survives removal, but Windows may give a re-created device node
   new endpoint IDs (a new `ROOT\MEDIA\NNNN`), so OBS or Discord may need the device picked again. A fixed instance ID per
   identity might keep them, but is untested and may also keep stale endpoint properties (TODO 22). Worth a spike now, or
   later?
3. **Microphones on new cables.** `add` defaults to both microphones for every cable (ours), so each cable shows a
   Microphone and a Call Microphone in every app. Or one microphone by default, as B has now?
