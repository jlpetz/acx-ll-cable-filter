## TODO 19 design: any number of cables, each with its own feed and diagnostics

**Status: the owner answered its questions (2026-11-24, with Astra; below), and asked for three revisions before it is
built: discovery without hard-coded cables, diagnostics reset when a cable is re-created, and no compatibility promise
(pre-release). Revised to those (ours, 2026-11-24, 3af523c), then again for a review of that revision (ours, 2026-11-24):
STATS redacted for every reader, the identity and label carried in a fixed instance ID that no other cable can take, one
package for every cable, an update that puts the cables back when it can't finish, and the runs re-ordered so each can
run at its slot. For the owner's review; nothing built.**

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

The design below is ours (a workflow's read-only design, 2026-11-24, on main at 9d7fd98, revised on 3af523c), written to
the decisions above; anything not attributed to the owner is a proposal until the owner agrees. Public sources read for
this file: WDF's `FxPkgGeneral`, `FxDevice::DeleteObject` and `FxIoQueue::QueueRequest`
(microsoft/Windows-Driver-Frameworks), microsoft/low-latency-audio's ACX driver, the WDK 10.0.28000 headers on the dev box
(KMDF 1.35, ACX 1.1), and Learn pages.

**Marks.** **[V: source]** verified: a header on the dev box (KMDF headers under `Include\wdf\kmdf\1.35`, the rest under
`Include\10.0.28000.0`), a Learn page, WDF's source, our code, or a live run recorded in `19-spike-2026-11-20.md`. **[C]**
to confirm on the box. **Words.** A *cable* is one device node of the one driver package: a speaker endpoint and one or
two microphones looped back. Its *label* is its letter ("D"). Its *identity* is a GUID that outlives removal. Its *instance
ID* is the device node's, fixed by the setup tool from the label, the identity and an instance number *n* (section 4). A
*binding* is one life of its framework device, from AddDevice to context cleanup; its *generation* tells bindings apart.

### Summary
- **Discovery.** Each cable registers a discovery-only device interface of our own class. Its properties carry the
  identity, label, generation and control path. User mode lists the interfaces with `CM_Get_Device_Interface_ListW` and
  reads the properties; nothing ever opens the interface.
- **One control device per cable, named from its identity**, `\\.\AcxLlCable_<id>`. `\\.\NoiseFilterVmic` goes. Labels,
  ids, paths and the old names are aliases that nf-vmic resolves through discovery.
- **All per-cable state in the device context.** The app ring, its copy, the counters, the skip and underrun logs, the
  timer statistics, the capture trace and the statistics snapshot. No global table; only the stream-serial counter stays
  global.
- **Identity and label in the instance ID.** The setup tool creates each cable's device node as
  `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, so the driver reads both at AddDevice and nothing is written after the node is
  registered. The instance ID is unique for good: re-adding a cable reuses its own, a variant switch takes the next n, and
  no cable can take another's endpoints. A setup-owned store remembers label, id and n across removal. The generation is
  the binding's start time, and each binding starts its counters at zero.
- **Statistics.** A new layout with its own magic and version. One shared snapshot per cable, at most 10 ms old, redacted
  for every reader. A write-access IOCTL gives the elevated tools a fresh, unredacted one.
- **Access.** The SDDL adds read for interactive users. Feeding still needs write access, held only by SYSTEM and
  Administrators, so it goes through the elevated broker.
- **Names.** Every model's description is "ACX-LL-Cable", and each instance's friendly name is "ACX-LL-Cable D", so its
  endpoints read "Microphone (ACX-LL-Cable D)". The three microphone-variant hardware IDs are shared by every cable.
- **Setup.** One Rust implementation (nf-vmic's `setup`, a small `acx-ll-cable` tool) behind `Cable.ps1`,
  `Install-Driver.ps1` and the variant switch. A cable is added on the package the others run. Only `update` changes the
  package; it moves every cable together, and puts them back if it can't finish.
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
  other variants), `Cable-B.ps1` (devcon install of B from the package the primary cable runs, its
  `DEVPKEY_Device_DriverInfPath`; a file lock; the default-role restore), and `variant.rs` (re-creation through SetupDi
  with `DICD_GENERATE_ID`, `DiInstallDevice` from the driver store, a lock in a private namespace bounded by
  Administrators; its steps act on every device of ours, not one cable).
- **User mode:** `protocol::USER_PATH` is fixed; the broker helper holds one `Option<Attachment>` and reads one-byte
  commands, ending on an unknown one; `RingOutput::open()` calls `attach_live(LIVE_CAPACITY)`; `sessions::is_ours` matches
  "AudioCodec Device" by name; nf-vmic-test opens `USER_PATH` and finds its endpoint by the name "Microphone (AudioCodec
  Device)" (`nf-vmic-test.rs:81, 379`).

### 2. Discovery: one interface per cable, never opened
**Driver, in AddDevice after `WdfDeviceCreate`:**
1. `WdfDeviceCreateDeviceInterface(device, &GUID_DEVINTERFACE_ACX_LL_CABLE, L"AcxLlCable")`, a new interface class GUID of
   ours. Called from EvtDriverDeviceAdd, the framework enables the interface when the device starts and disables it when it
   stops [V: Learn, WdfDeviceCreateDeviceInterface, Remarks]; PASSIVE_LEVEL [V: wdfdevice.h]. WDF does the enabling, which
   `IoRegisterDeviceInterface` would leave to us. The interface has to be the FDO's: a control device can't have one
   [V: Learn, WdfDeviceRetrieveDeviceInterfaceString: STATUS_INVALID_DEVICE_REQUEST for a `WdfControlDeviceInitAllocate`
   device; Learn, Using Control Device Objects: no control device in methods "that support device interfaces"]. The
   reference string only keeps the instance apart from ACX's own interfaces; nothing opens it.
2. `WdfStringCreate`, then `WdfDeviceRetrieveDeviceInterfaceString` (PASSIVE [V: wdfdevice.h]; STATUS_INVALID_DEVICE_STATE
   if the system hasn't assigned a link yet [V: Learn]). Then `IoSetDeviceInterfacePropertyData` for each key below, with
   LOCALE_NEUTRAL and Flags 0 (PASSIVE in wdm.h; "IRQL <= APC_LEVEL in the context of a system thread" on Learn [V]). Its
   link may come from `IoRegisterDeviceInterface` [V: Learn, parameter text], so a registered interface that isn't enabled
   yet is a valid target. Set at every AddDevice, before the start enables the interface [C: values set in AddDevice are
   read back once the interface is enabled; fallback: set them in EvtDevicePrepareHardware, which also precedes the
   enable].
   - **The order, so a listed interface never mixes two bindings' values.** Within a boot the properties stay on the
     interface's symbolic link, which every binding of the device node shares; Flags 0 only means they don't survive a
     restart [V: Learn]. So AddDevice first deletes our six keys (Data NULL deletes a property [V: Learn,
     IoSetDeviceInterfacePropertyData]), then writes them, Generation last.
   - **If any write fails,** the interface stays disabled for this binding: `WdfDeviceSetDeviceInterfaceStateEx(FALSE)`
     in AddDevice [V: wdfdevice.h, KMDF 1.35; C: called before the start, it keeps the framework from enabling the
     interface], and the failure goes to the driver's log. nf-vmic's check of Generation against CABLE_INFO (section 6)
     stays the backstop.
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
3. **Endpoints:** MMDevice enumeration, render and capture, in the states ACTIVE, DISABLED and UNPLUGGED. Not NOTPRESENT:
   a removed node's endpoints linger in that state.
   - Each endpoint's KS filter path gives an instance ID and a reference string (`sessions::identify`, extended to
     render's Speaker0). The reference names the endpoint: Speaker0 Speakers, Microphone0 Microphone, Microphone1 Call
     Microphone. The documented device topology route comes first, then the property-store copy, as today.
   - The endpoint belongs to the cable whose device node has that instance ID (compared without case), and only while that
     KS filter interface is present: `CM_Get_Device_Interface_ListW(&KSCATEGORY_AUDIO, <instance ID>, ..., PRESENT)` lists
     it.
   - Instance IDs are never reused by another cable (section 4), so a stale endpoint or a cached filter path can't be
     credited to the wrong one.
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
  reason: a deleted control device lives until its last handle closes), and deleted only if this binding created it (the
  created flag), so a binding whose name collided never deletes another binding's link. With a handle held across a
  disable and enable, the new control device got the name and the old handle failed with ERROR_BAD_COMMAND [V: live,
  19-spike, check 4].
- **A name collision** means another device node carries the same identity (a setup error). It costs that binding's feed
  only: ControlStatus STATUS_OBJECT_NAME_COLLISION, ControlPath empty, the cable plays as a plain cable. `cables` and the
  setup tool report the duplicate.
- **The control device's context** is `NF_CONTROL_CONTEXT { EX_RUNDOWN_REF Rundown; PNF_CABLE Cable; }`, nonpaged
  [V: Learn, Framework Object Context Space]. Every callback reaches the cable through it: the IOCTL callback through its
  queue's device; the cancel routine and EvtFileCleanup through `WdfRequestGetFileObject` and `WdfFileObjectGetDevice`
  (<= DISPATCH [V: wdffileobject.h:119]). Each takes rundown protection first (callable at DISPATCH when the block is
  nonpaged [V: Learn, ExAcquireRundownProtection]).
- **When protection is refused** (only after step 4 below; the order should keep any callback from meeting it, and the
  rule is for the case it doesn't):
  - EvtRequestCancel always completes its request, as it does today, and then touches nothing but the request. A cancel
    routine that returned without completing would hang `WdfIoQueuePurgeSynchronously`.
  - EvtFileCleanup and the IOCTL callback skip their work; an IOCTL completes with STATUS_INVALID_DEVICE_STATE, as the
    purged queue's do.
- **Deletion, in the cable's context cleanup at PASSIVE:**
  1. Set the ring's Removing under its lock.
  2. Delete the link, if this binding created it.
  3. `WdfIoQueuePurgeSynchronously`: the pending ATTACH ends with STATUS_DEVICE_REMOVED, and the call returns once every
     delivered request has completed (today's code, with its WDF-source citation).
  4. `ExWaitForRundownProtectionRelease` (<= APC_LEVEL [V: wdm.h]). After it, a file cleanup on the lingering deleted
     control device is refused protection and touches nothing.
  5. `WdfObjectDelete`, then free the cable's pool buffers.

  The guard is new because the cable now lives in the PnP device's context, which goes with the binding, while a deleted
  control device lives on until its last handle closes. The ring used to be static for exactly that reason (AppRing.h).
  Deleting one cable's control device while other cables' devices live departs from Learn's wording; section 14, risk 3,
  quotes it, says why we think it safe, and gives the fallback.
- **No EvtDeviceFileCreate.** The IOCTLs' access bits decide what a handle may do (section 7), and STATS is redacted for
  every caller (section 6), so nothing needs to know a handle's granted access. It lives in the handle table, not the
  FILE_OBJECT, so an IOCTL callback can't see it; per-handle redaction would need EvtDeviceFileCreate to record the
  create's `AccessState->PreviouslyGrantedAccess` in the file context. Not needed.
- **Queue:** parallel and not power-managed, as today, with `ExecutionLevel = WdfExecutionLevelPassive`. Queues take it
  from KMDF 1.9, and the framework then calls their callbacks only at PASSIVE_LEVEL [V: Learn, WDF_EXECUTION_LEVEL]. STATS
  needs that for its fast mutex (section 6).
- **A keeps no special name; `\\.\NoiseFilterVmic` goes.** One scheme is clearer, and nothing in the driver makes A
  special any more: any cable can be fed. A tool built for the old name fails at open with ERROR_FILE_NOT_FOUND instead of
  misreading a new layout. Pre-release, the owner welcomes the break.
- **The aliases the owner listed** (`\\.\NoiseFilterVmicB` style names, `--cable b`) are served in user mode. nf-vmic's
  selector takes a label, an id, a control path, or an old name (`\\.\NoiseFilterVmic<L>` for label L, and
  `\\.\NoiseFilterVmic` for A), and resolves each through discovery (section 10). No kernel alias link, as we propose it;
  whether the owner meant kernel links is open question 4.

### 4. Identity, labels and generations
- **Where it lives: in the instance ID.** The setup tool creates each cable's device node with a fixed instance ID,
  `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`: `SetupDiCreateDeviceInfoW` with a full device instance ID and without
  `DICD_GENERATE_ID` [V: Learn, SetupDiCreateDeviceInfoW: DeviceName is a full device instance ID unless DICD_GENERATE_ID
  is set].
  - `<label>` is one to eight of A-Z and 0-9 (the setup gives single letters), `<id>` the GUID's 36 characters, and `<n>`
    a decimal instance number from 1, kept in the store.
  - About 70 characters: under MAX_DEVICE_ID_LEN [V: cfgmgr32.h:55], with no `\` [V: Learn, Instance ID].
  - The tool writes upper case. The driver and nf-vmic compare without case [C: the case Windows keeps].
- **Why there, and not in the hardware key** (the previous revision's place). The instance ID exists from
  `SetupDiCreateDeviceInfoW`, before DIF_REGISTERDEVICE. A hardware-key value can only be written once the node is
  registered: `SetupDiCreateDevRegKeyW` requires a registered instance [V: Learn, SetupDiCreateDevRegKeyW, Remarks]. And
  once a node is registered, Plug and Play may install and start it by itself (variant.rs, step 5), so AddDevice could run
  before the value is there and fail the node's first binding. (A review suggested writing the key on the unregistered
  element; Learn's remark rules that out.) In the instance ID, nothing the driver reads at AddDevice is written after
  registration, so there is no window.
- **The cost:** a label can't change without re-creating the node. No command needs that (ours): the store binds a label
  to its identity anyway.
- **Unique for good.** `DICD_GENERATE_ID`'s `ROOT\MEDIA\NNNN` only has to be unused when it's made, so a removed node's
  number can come back for a new cable. That cable would inherit everything Windows keyed to the old KS filter paths: the
  MMDevice endpoint ids, their cached names and properties, the Sound panel's renames, apps' saved device choices (an app
  set to B would silently get D). variant.rs avoids reuse only because it creates the new element before removing the old
  node. With the identity in the ID, a cable's instance ID can never be another cable's.
  - **Re-adding keeps n:** the same instance ID, so the same KS filter paths, so (we expect [C]) the same MMDevice
    endpoints: an app set to B finds B again. Open question 2 asks the owner to agree.
  - **A variant switch takes n + 1:** fresh endpoints, which TODO 22's default period needs (Windows reads it only when
    it makes an endpoint).
  - **If a removed node's key is still there,** `SetupDiCreateDeviceInfoW` fails with ERROR_DEVINST_ALREADY_EXISTS
    [V: Learn]. The tool then opens it (`SetupDiOpenDeviceInfoW`) and installs on it [C: what `DiUninstallDevice` leaves
    of a root node, and installing on a re-opened one]. Failing that, it takes n + 1 and says the endpoints are new.
- **The store:** `HKLM\SOFTWARE\ACX-LL-Cable\Cables\<label>` (`Id`, `N`, `Mics`), owned by the setup tool. Adding a label
  reuses its identity and n; removing keeps them; `remove --forget` drops them.
- **Who reads it:** AddDevice, before `WdfDeviceCreate`. `WdfFdoInitQueryPropertyEx(DEVPKEY_Device_InstanceId)`
  [V: wdffdo.h:295]; the label and the 36 characters are parsed after the last `\`, then `RtlGUIDFromString` takes the
  characters with braces added (PASSIVE [V: wdm.h:15218]). The same string goes to CABLE_INFO.
- **A missing or invalid identity fails AddDevice** with STATUS_DEVICE_CONFIGURATION_ERROR (ours), so every working cable
  has one, and discovery, the control device's name and the setup never meet an unnamed one. A node made outside the tool
  (devcon, Device Manager's Add legacy hardware) has a `ROOT\MEDIA\NNNN` and no identity: it shows CM_PROB_FAILED_ADD, 31
  [V: cfg.h:98, "driver failed add"; C: what Device Manager shows], until `Cable.ps1 add <label> -Adopt <instance>`
  re-creates it in the tool's form (new endpoints, once). Open question 5 offers the owner an alternative.
- **The hardware key** keeps only what the INF writes (`NfMicrophones`, from the `.HW` AddReg, as today).
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
- **The binding:** Id and Label (from the instance ID), Generation, BindQpc, Microphones, `InstanceId[200]`
  (MAX_DEVICE_ID_LEN [V: cfgmgr32.h:55]), and the circuits' component IDs (section 8).
- **The ring:** today's `NF_APP_RING`: the attachment, the reader slot, the copy's bookkeeping, segments, claims,
  OtherFormatCaptureOpens, the counters, Removing. Its Loopback pointer is fixed at AddDevice to the same context's ring;
  the owner bookkeeping goes.
- **The copy:** `History`, 256 KiB from nonpaged pool, allocated in AddDevice and freed in context cleanup, as
  `Loopback.Buffer` is.
- **Timer statistics and the capture trace:** `StreamStats[2]` and `CaptureTrace`, interlocked as today.
- **The jump totals** (SecondaryJumps, SecondaryJumpFrames) move from `LOOPBACK_RING` into the ring's counters. They are the
  copy's, and are already counted under the ring's lock.
- **Control and discovery:** the control device, its queue, ControlStatus, the created flag, and the discovery status.
- **The snapshot:** a FAST_MUTEX, its QPC, and an `NF_CABLE_STATS` (6,896 bytes), stored redacted.
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
cables within a driver load, so "A #17" and "B #17" can't both exist, and the merged log stays unambiguous. It's a label,
not a counter, so it doesn't reset. Read-only constants and `g_RegistryPath` stay too. Gone: `s_CdoLock`, `s_DeviceCount`,
`s_ControlDevice`, `s_ControlQueue`, `g_NfAppRing`, `g_NfAppHistory`, `g_NfStreamStats`, `g_NfCaptureTrace`,
`NfVmic_DriverInit`, `LOOPBACK_RING.AppFeed` and `CODEC_DEVICE_CONTEXT.VmicRegistered`.

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
| STATS 0x801 | 0x00226004 | BUFFERED | read | the shared snapshot, redacted for every caller |
| CABLE_INFO 0x802 | 0x00226008 | BUFFERED | read | the binding's facts and the feed state |
| STATS_FULL 0x803 | 0x0022E00C | BUFFERED | read, write | a fresh snapshot, unredacted |

**The input, and the order of the checks.** The three diagnostic IOCTLs take `NF_STATS_IN {Magic, StatsVersion, InBytes
(16), Flags (0)}`. METHOD_BUFFERED hands the input and the output in one system buffer, which the driver reads from and
then writes into [V: Learn, Buffer Descriptions for I/O Control Codes]. So the order is fixed:
1. Copy NF_STATS_IN to the stack once the input length is at least 8, then check, as ATTACH does: Magic (else
   STATUS_INVALID_PARAMETER); StatsVersion (else STATUS_REVISION_MISMATCH: ERROR_REVISION_MISMATCH, 1306); an input length
   of exactly 16, InBytes 16 and Flags 0 (else STATUS_INVALID_PARAMETER).
2. Only then check the output length (exactly the struct, else STATUS_INVALID_BUFFER_SIZE), retrieve the output buffer,
   zero it and fill it.

Today's `NfVmic_Stats` zeroes its output first (`RtlZeroMemory(out, ...)`). Carried over as is, that would wipe the input
before Magic and Version are read, so every call would fail. Every diagnostic call exercises the overlap, and P4's sweep
covers each refusal.

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

**`NF_CABLE_INFO`** (560 bytes):

| Offset | Size | Fields |
| --- | --- | --- |
| 0 | 16 | Magic, StatsVersion, InfoBytes, Flags (ATTACHED, LIVE) |
| 16 | 16 | CableId |
| 32 | 24 | Generation, BindQpc, QpcFrequency |
| 56 | 8 | Microphones (bit 0 Microphone0, bit 1 Microphone1); ProtocolMin (u16, 1), ProtocolMax (u16, 2) |
| 64 | 32 | Label: 16 UTF-16 units, NUL-padded |
| 96 | 64 | BuildId: 64 ASCII bytes, NUL-padded (section 9) |
| 160 | 400 | InstanceId: 200 UTF-16 units |

CABLE_INFO takes no lock of ours: the facts don't change during a binding, and the feed state is the interlocked info word.
Each request still pays WDF's own raise, since queuing it takes the queue's spin lock [V: WDF source, fxioqueue.cpp,
FxIoQueue::QueueRequest] (section 14, risk 6). nf-vmic checks after each open that its Id, Generation and InstanceId match
the discovered interface, which catches a duplicate or a re-bind between listing and opening.

**The snapshot (the owner's 10 ms rule).** STATS takes the cable's fast mutex (raises to APC_LEVEL [V: wdm.h:27081]):
1. If the snapshot is 10 ms old or more by QPC, or was never taken, refresh it. That is today's STATS copy: the ring's
   lock, the loopback's, the logs, with QpcSnapshot read last. The refresh stores it redacted: Pid 0 and REDACTED set.
2. Copy it into the request's buffer and release the mutex.

Readers arriving together: the first refreshes, and the others wait microseconds on the mutex and get the same bytes,
QpcSnapshot included. No ERROR_BUSY, and no reader can starve another. However many readers there are, a cable's snapshot
refreshes at most 100 times a second. That bounds our own contention with the capture DPC and, under Verifier, our own
spin-lock-raise trims found in 1c8d9d4 (TODO 13); each request's WDF queue lock is still paid (section 14, risk 6).

STATS_FULL always refreshes, into the request's buffer, unredacted, under the same spin locks; the shared snapshot is left
alone. Only write-access handles can send it, which the I/O manager checks (section 7): the elevated tools and the broker.
nf-vmic-test's verify and stamps need statistics current to the packet, which a 10 ms cache would hide. The GUI refreshes
once a second (the owner).

**Redaction.** Every STATS reply has Pid 0 and REDACTED set, whoever asks. An IOCTL callback can't tell a read-only handle
from a read-write one (section 3), and it doesn't need to: the owner's rule, the pid hidden from read-only handles, holds
because the one unredacted path, STATS_FULL, needs write access. It also matches the owner's "readers arriving together get
the same ... redacted snapshot". Nothing else in STATS, CABLE_INFO or the discovery properties names a process, session,
user, image or handle, and STATS carries no audio samples, only positions and counts [V: the layouts above].

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
- **Held handles (the trade-off's other half, ours).** Such a user can also keep a read-only handle open as long as they
  like. A handle keeps a deleted control device, and so the driver image, loaded, which blocks `update` until it closes.
  `update` names the holders, with their process ids and images (section 9); closing them is the admin's.
- **Low integrity.** Low-integrity processes carry IU too, and can read. IoCreateDeviceSecure's SDDL is a DACL alone ("D:P"
  followed by allow ACEs [V: Learn, SDDL for Device Objects]), so no mandatory label can be set there. An object without a
  label counts as medium integrity, whose default policy stops writes from below, not reads [C, for a device object]. What
  they can read is what any signed-in user may.
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
  hardware ID picks the microphones and the instance ID names the cable.
- **The letter: a per-instance FriendlyName**, "ACX-LL-Cable D". Windows names an endpoint "<endpoint description>
  (<adapter>)", and today the adapter part is the device's description [V: our code and live names]. With a FriendlyName
  it should be that, and per-instance names also end Windows' "2- " numbering.
  - **Set before registration:** the tool sets `SPDRP_FRIENDLYNAME` on the device information element beside the hardware
    ID, before DIF_REGISTERDEVICE, with the call variant.rs makes for `SPDRP_HARDWAREID`
    (`SetupDiSetDeviceRegistryPropertyW` [V: variant.rs, step 3]). So it is there before Plug and Play could start the
    node.
  - **Re-applied after every DiInstallDevice**, in `add`, `variant` and `update`, and the endpoint names checked after
    each. An endpoint made while the name was missing would read "(ACX-LL-Cable)" with Windows' numbering, and may keep
    that name after the FriendlyName comes back (TODO 22's stale properties).
  - [C] The FriendlyName survives the install, an update and a variant switch, and names the endpoints when they're made.
  - **Preferred if it works [C]:** the driver sets `DEVPKEY_Device_FriendlyName` from the label in AddDevice with
    `WdfDeviceAssignProperty` (<= APC [V: wdfdevice.h]). That is re-applied at every binding, before the endpoints are
    made. Learn names no forbidden keys, but we haven't seen a driver set this one. The tool's write stays as a second
    layer.
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
  re-creates that cable's device node with the new hardware ID and the next instance number
  (`ROOT\MEDIA\ACXLLCABLE_B_<id>_<n + 1>`, so its endpoints are new and Windows reads the variant's default period). It
  carries the identity, label and FriendlyName (a new binding), installs the package the other cables run (section 9),
  and touches no other cable: S1 makes variant.rs's steps per cable (section 13).
- **Docs and GUI:** "ACX-LL-Cable B" beside "VB-CABLE B" (the owner).
- **Code:** `sessions::is_ours` and `identify` go by the KS filter's instance ID (our form), with the name only as a
  display fallback. variant.rs's `DEVICE_NAME`, `MICROPHONE` and `CALL_MICROPHONE` strings go.

### 9. Setup: one implementation, the scripts, update and uninstall
**One implementation in Rust:** nf-vmic's `setup` module (ours), extracted from variant.rs. It finds our devices, takes the
privileged lock (now for every setup action), picks the package, creates and removes. A small elevated tool,
`acx-ll-cable` (a bin in nf-vmic; no audio, no app), runs it for plain-cable users, who need neither the app nor the
broker. The release installer (TODO 18) and the scripts run the same tool; `noise-filter mic variant` calls the library.

**Which package (ours): the one the cables run.** `add` and `variant` install the package the present cables run (their
`DEVPKEY_Device_DriverInfPath` and its DriverVer), from the compatible-driver list as variant.rs builds it.
- If our device nodes run different packages, they refuse and say to run `update`.
- If a newer package of ours is staged, they say so, and that `update` moves every cable to it, then go on with the
  running one.
- With no cable present, they take the newest, as variant.rs does.
- Only `update` changes the package.

The reason: every cable runs one service and one image. A cable installed from another package would run the loaded image
while its DriverInfPath named the new one; the install would rewrite the service's ImagePath while the old image is
loaded; and DiInstallDevice could then ask for a restart, which variant.rs's judge treats as RestartPending, its
microphones unchecked. Cable-B.ps1 already installs B from the package the primary runs.

**The tool's commands:**
- **`list`** (any user): present cables (discovery, plus every device node with one of our hardware IDs, legacy ones
  included, so unconfigured ones and ones with a problem show too), remembered absent cables (from the store), and
  duplicates.
- **`add <label> [--mics both|call|low-latency] [--adopt <instance>]`**, under the lock:
  - present and healthy: no change, so `add` is idempotent;
  - present but unhealthy: re-created with the same identity and instance ID;
  - absent: the identity and n come from the store, or are new (`UuidCreate`; n 1).
  - **Creating it** (variant.rs's steps 3-5 [V: live, the variant switches of 03c477a], changed): the device information
    element with its instance ID (no `DICD_GENERATE_ID`); the hardware ID and the FriendlyName on the element;
    DIF_REGISTERDEVICE; `DiInstallDevice` with the package above. Nothing is written between registration and install.
  - **Then:** up to DRIVER_WAIT for it to work and MICS_WAIT for its endpoints. The FriendlyName is re-applied. Check that
    CABLE_INFO's Id and Label are the store's, its DriverInfPath and BuildId are the other cables', and its endpoints are
    named "... (ACX-LL-Cable D)". A failed check is an error that names it; what to do then (a restart, or a re-creation
    with n + 1 for fresh endpoints) waits on the confirm items. Then record it in the store.

  `add` never restarts another cable.
- **`remove <label> [--forget]`:** `DiUninstallDevice` the device nodes with that identity. A feed on it ends with
  ERROR_DEVICE_REMOVED (today's path). The store entry stays unless `--forget`. The endpoints Windows keeps for it
  (NOTPRESENT) are neither listed nor credited (section 2).
- **`variant <label> <mics>`** (section 8).
- **`update <package folder>`:** every cable to the package (below).

**`driver/scripts/Cable.ps1`** (dev; replaces Cable-B.ps1): `Cable.ps1 list | add <L> [-Mics m] [-Adopt id] |
remove <L> [-Forget] | variant <L> <m>`. It runs the tool and fails if it's missing; `-Build` builds it first
(`cargo build --release -p nf-vmic --bin acx-ll-cable`), so no run rebuilds anything unasked. It keeps Cable-B.ps1's
restore of the default roles Windows moved to a new endpoint (Set-DefaultAudio.ps1) around `add` and `variant`.

**`Install-Driver.ps1`:**
1. The checks as today.
2. Migration, once: remove the legacy device nodes (`ROOT\AudioCodec*`, `ROOT\NoiseFilterCableB`), which the new INF no
   longer matches.
3. `pnputil /add-driver` (to the store only, no `/install`).
4. If device nodes of ours are present, `acx-ll-cable update <package folder>`; otherwise `acx-ll-cable add A --mics
   <Variant>`. `-Variant` keeps its meaning, for A.
5. The status.

**A driver update moves every cable** (our review of 09e5520: cables sharing the service keep the old image loaded).
Every cable runs one service and one image, which reloads only once no device and no control-device handle holds it. Every
cable's audio stops for the update's seconds: one image can't be swapped under a running cable. `update <package folder>`,
under the lock:
1. **Find every device node of ours:** every present node, in any state and with any problem code, with one of our
   hardware IDs, the legacy ones too. From SetupDi, not discovery: a node with a problem or without an interface can
   still hold the image.
2. **Disable each** with `CM_Disable_DevNode` (polite, CM_DISABLE_UI_NOT_OK), without CM_DISABLE_PERSIST, which "persists
   through restart by setting CONFIGFLAG_DISABLED" [V: cfgmgr32.h:958]. So a crash or a reboot mid-update brings every
   cable back.
3. **Wait up to 10 s for the image to unload** [C: how to read it: the service's state, `sc query`, or the driver object].
   - **On a veto (CR_REMOVE_VETOED) or the timeout:** re-enable every node this run disabled, name the holders, and exit
     non-zero. Nothing is installed.
   - **The holders:** the tool enumerates the system's handles (`NtQuerySystemInformation`'s
     SystemExtendedHandleInformation, which Sysinternals handle and Process Explorer read; undocumented, so ours, for an
     elevated tool only). It keeps File handles whose object name starts `\Device\AcxLlCable_`, querying each name on a
     worker thread with a timeout (a name query can block on a synchronous pipe), and prints each holder's pid and image.
     Any interactive user's read-only handle can be one (section 7).
4. **Install the package on each** (`DiInstallDevice`), re-apply its FriendlyName, and enable every node. [C: on a node
   disabled without persistence, DiInstallDevice may start it, which is fine once the old image is gone.]
5. **Check each:** it runs the package (`DEVPKEY_Device_DriverInfPath`, and `DEVPKEY_Device_DriverVersion` equal to the
   package's DriverVer); its CABLE_INFO BuildId equals the `BuildId.txt` beside the package; its endpoints keep their
   names. This is the review's proof that the loaded image is the new one. A node that fails is named and the run exits
   non-zero; the others are left running.

**BuildId, unique per build.** Build-Driver.ps1 makes it `<12-hex short commit>[+] <DriverVer>`: `+` marks a dirty tree,
and DriverVer is the INF's per-build stamp (the UTC date, then the time of day as H.m.s.ms [V: Build-Driver.ps1]), e.g.
`3af523c0d1e2+ 11/24/2026,3.27.41.512`. That is at most 40 characters, in a 64-byte field (section 6).
- Two builds of the same dirty tree differ, which the commit alone couldn't show, and that is how a driver is iterated
  before a commit.
- It reaches the driver through a generated header that one source file includes, so it costs one compile and a link,
  not a full rebuild. The script writes it to `BuildId.txt` beside the package.
- Today's warning, a stamped DriverVer other than the one asked for (StampInf stamps only when MSBuild's tracking says
  so), becomes an error, since the BuildId names that DriverVer.

[C] Whether `pnputil /add-driver /install` alone restarts every cable and reloads the image; the procedure doesn't rely on
it.

**`Uninstall-Driver.ps1`:** `acx-ll-cable remove --all --forget` (the legacy IDs included), then delete the packages (both
INF names during the migration). `-KeepIdentities` keeps the store. **`Get-DriverStatus.ps1`** shows the tool's `list`.

**Privacy:** cable ids are random GUIDs unique to each machine, and they are in each cable's instance ID, control path and
KS filter paths. Scrub them from logs pasted into docs; write "A's id" (CLAUDE.md, Privacy).

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
    pub instance_id: String,                      // ROOT\MEDIA\ACXLLCABLE_D_<id>_1
    pub control_path: Option<String>,             // None: no control device; control_status says why
    pub control_status: i32,                      // NTSTATUS
    pub diagnostics_version: u32,
    pub endpoints: Vec<CableEndpoint>,            // MMDevice id, name as Windows shows it, kind, active or disabled
    pub feed: Feed,                               // Attached { live: bool } | Free | Unknown
}
pub enum EndpointKind { Speakers, Microphone, CallMicrophone }
pub enum Selector { Label(String), Id(CableId), Path(String) }  // FromStr: the forms below
pub fn list() -> Result<Vec<Cable>, WinError>;
pub fn find<'a>(cables: &'a [Cable], s: &Selector) -> Result<&'a Cable, FindError>;   // NotFound, Ambiguous(ids)
pub fn cable_of_endpoint(cables: &[Cable], endpoint_id: &str) -> Option<CableId>;     // render or capture, by KS filter

// cables.rs, for the broker's elevated helper: no COM, no MMDevice, no string a user can set.
pub struct ControlTarget { pub id: CableId, pub generation: u64, pub instance_id: String }
pub fn resolve_control(id: &CableId) -> Result<ControlTarget, FindError>;  // the interface list and its properties only
pub fn control_path(id: &CableId) -> String;                                // \\.\AcxLlCable_<id>, built from the id

// diagnostics.rs: read-only, any interactive user. Open, read, close each time.
pub fn read(cable: &Cable) -> io::Result<Snapshot>;              // CABLE_INFO + STATS (redacted, at most 10 ms old)
pub struct Snapshot { pub info: CableInfo, pub stats: CableStats }
pub fn merged_log(snaps: &[(&Cable, &Snapshot)]) -> Vec<LabelledEvent>;   // by QPC, labelled "B #17"
pub struct Timeline;                                             // a cable's history; push() marks a restart on a new generation

// device.rs: feeding (write access: elevated, or the broker's helper).
impl Device {
    pub fn open(cable: &Cable) -> io::Result<Device>;            // read+write; checks CABLE_INFO against `cable`
    pub fn open_target(t: &ControlTarget) -> io::Result<Device>; // the same, for the helper
    pub fn info(&self) -> io::Result<CableInfo>;
    pub fn stats_full(&self) -> io::Result<CableStats>;
    // attach and attach_live as today
}

// broker.rs: the contract (the helper's side is Astra's).
pub fn attach_live(cable: &CableId, capacity: u32) -> io::Result<LiveFeed>;
```

**The selector's forms** (`Selector::from_str`): a label, "d" or "D"; a name, "ACX-LL-Cable D"; an id; a control path,
`\\.\AcxLlCable_<id>`; and the old names as the owner's aliases, `\\.\NoiseFilterVmic<L>` for label L and
`\\.\NoiseFilterVmic` for label A. A unit test covers each form.

**Errors, by Win32 code, and nf-vmic's names for them:**
- ERROR_FILE_NOT_FOUND: the cable is gone (`Gone`);
- ERROR_DEVICE_REMOVED: removed while fed (`Removed`);
- ERROR_BAD_COMMAND: on a handle kept across a re-bind or a removal, where the purged queue answers
  STATUS_INVALID_DEVICE_STATE [V: live, 19-spike, check 4: the held handle got 22 after both]. This cable was re-bound or
  removed: close the handle and resolve the cable again (`Rebound`). Long-lived handles (the broker helper, Device users)
  treat it as the end of that feed;
- ERROR_BUSY: that cable already has a writer (`Busy`);
- ERROR_ACCESS_DENIED: needs the broker's approval (`NeedsApproval`);
- ERROR_REVISION_MISMATCH: update the tool (`UpdateTool`).

`protocol.rs` mirrors the new structs with offset asserts and drops `USER_PATH`. Every `Stats` user changes once (no
compatibility promise).

**nf-vmic-test (ours):**
- `--cable <label|id|path|old name>` on feed, verify, kill-test, latency, stamps, idle-test, open, stats and probe. The
  default is the cable labelled A if present, else the only cable, else an error that lists them. kill-test passes it to
  its child. `--path <\\.\AcxLlCable_<id>>` (N0) works before discovery exists.
- verify and open resolve their endpoint by the cable's instance ID (Microphone0, or Microphone1 with `--mic call`), not
  by name. verify also fails a recording whose endpoint belongs to another cable than the fed one, instead of passing
  against the wrong statistics. Its before and after statistics, and stamps', come from STATS_FULL.
- `stats --cable X|all [--watch]`: `all` prints each cable, then the merged, labelled log; `--watch` keeps a Timeline and
  marks restarts. Read-only unless elevated, when it uses STATS_FULL.
- New `cables [--watch]`: every cable's label, name, id, generation (as a UTC time), instance, control path and status,
  endpoints, feed state and versions. It works unelevated. `--watch` re-lists about once a second and prints arrivals,
  departures and new generations; P5 uses it in place of the app until G1.
- New `probe --cable X [--readonly-open] --ioctl stats|stats-full|cable-info|attach`: one IOCTL on a handle opened as
  asked, printing the Win32 code. `probe --sweep` runs P4's negative cases.
- New `isolation` (section 12).
- `noise-filter mic variant|sessions` (nf-app, a thin CLI over nf-vmic) take `--cable` too.

### 11. Astra's parts: requirements and interfaces only
**Broker (B1):**
- **Commands:** START and STOP carry a 16-byte CableId, so a command is exactly 17 bytes, read only once 17 are available.
  The replies are unchanged (a code and a section handle).
- **A malformed command ends nothing.** An unknown command byte gets ERROR_INVALID_DATA, and an id that names no cable
  ERROR_FILE_NOT_FOUND; the loop goes on, and no other cable's feed ends (today's helper exits on an unknown command,
  `helper.rs`). A broken pipe still ends the helper, and so every feed: the app is gone.
- **A minimal resolver.** The helper resolves the id with `cables::resolve_control`: the interface list and its
  properties (CableId, Generation, ControlStatus, InstanceId), and nothing else. Not `cables::list()`, which also reads
  MMDevice endpoints through COM and AudioSrv, with strings users can set (endpoint renames). It refuses a cable whose
  ControlStatus isn't STATUS_SUCCESS, builds the path from the id (`control_path`) rather than trusting ControlPath, and
  opens it with `Device::open_target`, which checks CABLE_INFO's Id, Generation and InstanceId. Then it attaches
  LIVE_CAPACITY.
- **One writer per cable:** it holds a map from CableId to Attachment. A second START for the same cable gets ERROR_BUSY;
  another process's feed gets the driver's STATUS_DEVICE_BUSY. STOP detaches only its own cable, and a feed that ends
  (DEVICE_REMOVED, Rebound) frees its entry.
- **The session checks** run as today, at every START and every 200 ms (`helper.rs`). Still no command supplies a path,
  handle, capacity or duplication target: the id only picks among the cables the driver published, and the path is built
  from the id.
- **The approval's scope, stated in the GUI:** one UAC prompt starts the helper for this app process. It may then feed
  any ACX-LL-Cable present, until the app exits, while this session is the only signed-in interactive one. When a second
  user signs in, the helper ends, and with it every broker feed. Nothing persists.

**Engine (E1), fan-out:**
- The physical microphone is captured once and goes to N destinations. Each destination's source is the dry mic, the
  shared chain's output, or its own chain; its output is `Direct(CableId)` or `Wasapi(endpoint id)`.
- Each destination has its own output or feeding thread (MMCSS, the real-time rules), fed from the capture through
  lock-free queues. Stopping, reconfiguring, losing a device or underrunning one destination leaves the capture and the
  others running; each retries on its own, re-resolving its cable after `Rebound` or `Removed`.
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
- The approval's scope text, and the trade-off's (section 7).

### 12. Acceptance runs (script plans)
**Common:**
- Verifier on, the box idle, one audio test at a time (CLAUDE.md); 3 rounds a phase unless stated.
- **The box idle includes no IOCTL pollers** for timing runs: no `stats --watch`, `cables --watch` or GUI diagnostics
  beyond the run's own. Each request pays WDF's queue-lock raise and, under Verifier's Force IRQL checking, a trim
  (section 14, risk 6). A run that needs a poller (P5, P5b) polls at about 1 Hz.
- Scripts under `nf-scratch/iso/`, run by the lead or the owner.
- **`nf-vmic-test isolation --fed F --plain P --seconds S [--external] [--expect-gain G]`** reads both cables' STATS_FULL
  before and after. In tool mode it feeds F (`--start 4000000` on B, `100000` on A) and verifies F's recording. It plays the
  signal into P's Speakers (exclusive float render) and decodes P's Microphone. Reader periods differ, 160 frames on F and
  480 on P, so each cable's timer counts and log entries can match only its own streams.
- **F's checks:**
  - the same attachment throughout; Attaches +0, or +1 in tool mode;
  - Ring.Passes about seconds x 48000 / ReaderPacketFrames;
  - FramesCopied + UnderrunFrames about Passes x the packet;
  - Stream[CAPTURE].TimerPasses within 2% + 5 of its own recorder's;
  - Stream[RENDER] +0 on the direct route;
  - every new log entry is F's own: its PacketFrames F's recorder's (160), its Attach F's attachment's number, and its
    ring frames in F's fed range.
- **P's checks:**
  - not attached in either snapshot;
  - the ring's deltas (Attaches, Passes, FramesCopied, NotReaderPackets, UnderrunFrames) all 0;
  - Loopback.RenderRestarts +1 and CaptureAnchors at least +1;
  - its streams' counters within tolerance of its own recorders;
  - every new log entry is P's own: PacketFrames 480 and Attach 0;
  - the recording decodes only P's range: 0 corrupt, 0 repeats.
- **Across them:** each entry is checked against its own cable, above. A check that no stream serial is in both logs would
  prove nothing: serials are global, so they differ whatever happens. If the per-entry checks prove too weak,
  NF_CABLE_STATS could carry the serials of the streams last opened on its cable (a layout change, with its version).
- **Ids across cables:** P3 and P5 record every cable's instance ID and MMDevice endpoint ids before and after, and check
  that no id moves from one cable to another.

**Phases:**
- **S0a, migration and D1's regression** (after D1, S1 and N0; the owner's go, since it removes today's two devices;
  elevated, since D1 keeps today's SDDL): Build-Driver.ps1, Install-Driver.ps1 (A, both), `Cable.ps1 add B`.
  - Check: `acx-ll-cable list` (from SetupDi, since discovery comes with D2) shows A and B working, with instance IDs of
    the new form, and their endpoints, as `noise-filter devices` lists them, are "Speakers / Microphone / Call Microphone
    (ACX-LL-Cable A)" and "... (ACX-LL-Cable B)".
  - Then D1's live regression on A through `--path \\.\AcxLlCable_<A's id>` (three and four recorders, kill-test 10 of 10,
    ten minutes, disable and enable with a handle held), and spike check 4's removal cases on B with A attached. This is
    the gate for section 14's risk 3. Say first: about an hour, box idle.
- **S0b, discovery** (after D2, N1 and T1): `nf-vmic-test cables`, elevated and unelevated, shows A and B with their ids,
  generations, ControlStatus 0 and endpoints; `stats --cable all`, unelevated, shows REDACTED and Pid 0.
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
  - Then `Cable.ps1 add B`. Check: B is back with the same id and instance ID, its endpoints' MMDevice ids those it had
    before the removal [C], a new generation, Attaches 0 and empty logs (SkipEventsLogged 0); A's counters continued; no
    id moved between cables.
- **P4, a reduced-rights token:**
  - Each command runs as `runas /trustlevel:0x20000 "cmd /c <command> > %TEMP%\p4-<n>.txt 2>&1"`, one at a time, then
    the files are read: runas opens a console of its own, so the output has to be redirected inside it.
  - Check: `whoami /groups` shows IU and no enabled Administrators; `nf-vmic-test cables` and `stats --cable all` succeed
    with REDACTED set and Pid 0; `feed --cable A` fails at open with ERROR_ACCESS_DENIED; `probe --cable A
    --readonly-open --ioctl stats-full` and `--ioctl attach` get ERROR_ACCESS_DENIED.
  - **The negative sweep, as IU** (`probe --cable A --sweep`, on STATS and on CABLE_INFO): a wrong Magic and input lengths
    0, 15, 17 and 4096 (ERROR_INVALID_PARAMETER); a wrong StatsVersion (ERROR_REVISION_MISMATCH); output lengths 0 and
    the struct's size - 1 (STATUS_INVALID_BUFFER_SIZE's code [C: ERROR_INVALID_USER_BUFFER]); and the unknown function
    0x804 with read access (ERROR_INVALID_FUNCTION). Then a valid STATS succeeds, A's verify running beside it passes, and
    Verifier stays quiet.
  - P4 runs in one session. If it is ever run with two, a broker feed ends at the second sign-in (as in R).
- **P5, add D with B and C absent while A plays (the owner's acceptance):**
  - Before: build the tool, nf-vmic-test and the app (`Cable.ps1 -Build` for the tool), and hash nf-vmic-test,
    acx-ll-cable, the app and the driver image. `Cable.ps1 remove B`, and C if present. Record the ids (above).
  - Start `feed --cable A`, `verify --cable A --seconds 60`, and `nf-vmic-test cables --watch`, which stands in for the
    app's picker until G1. Then `Cable.ps1 add D`, without `-Build`.
  - Check within 20 s: D in `cables --watch` and `cables`, with label D, a control path, and "Speakers (ACX-LL-Cable D)"
    and "Microphone (ACX-LL-Cable D)"; `feed --cable D` with `verify --cable D` passes at once; D's DriverInfPath and
    CABLE_INFO BuildId equal A's.
  - Check A: its verify passes and its generation is unchanged.
  - Check the rest: the hashes and the default roles are unchanged, and no endpoint id of A's or of the removed B's is
    D's.
- **P5b, the app's picker** (after G1): P5 again, with the app's cable picker open in place of `cables --watch`. Check: the
  running app lists D without a restart.
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
  - a second signed-in user sees a feed attached but no process id. The feed is an elevated direct one, since a broker
    feed ends at the second sign-in (next check): the first user runs `nf-vmic-test feed --cable A` elevated, and the
    second user's `nf-vmic-test stats --cable A` shows ATTACHED, Pid 0 and REDACTED;
  - a broker feed ends when a second user signs in: with the app fed through the broker, the second sign-in ends the
    helper within about 200 ms, the app's feed reports finished, and its next start gets ERROR_ACCESS_DENIED.

### 13. Work breakdown and order
**Ours (driver, protocol, nf-vmic, nf-vmic-test, scripts).** (The nf-vmic item is N1 now; it was P1, which is also a
run's name.)
- **D1, per-cable driver** (2-3 days): NF_CABLE in the context; per-cable control devices with the rundown guard and the
  refusal rules; the identity and label read from the instance ID in AddDevice; the INF's hardware IDs and names;
  component IDs; the AppFeed gates gone. STATS keeps today's layout for now, read from the cable, and the SDDL stays SY
  and BA. Build plus InfVerif.
- **S1, setup** (about 2 days): nf-vmic's `setup` and the `acx-ll-cable` tool; the fixed instance IDs and the store; the
  package rule; `update` with its recovery and holder listing; Cable.ps1 with `-Build`, Install-, Uninstall- and
  Get-DriverStatus.ps1. It lands with D1, whose driver refuses a node without an identity.
  - **variant.rs per cable:** step 1 finds the switched cable's nodes only (by identity, from the instance ID); step 4
    removes only them; step 6's "no other endpoint of ours still active" looks only at the switched cable's old instance;
    the new node takes n + 1 and the package the others run.
  - **Offline tests with two cables present:** the plan, the per-cable filters, the package rule and the instance-ID
    parse, pure.
- **N0, the interim slice** (about half a day, with D1): `protocol`'s fixed `USER_PATH` replaced by `control_path(id)`;
  `Device::open_path`; nf-vmic-test's `--path`; endpoints resolved by the KS filter's instance ID (`sessions::identify`
  already parses it), and `sessions::is_ours` by our instance-ID form with the new name as fallback. Enough for S0a,
  elevated.
- **D2, diagnostics** (1.5-2 days): the discovery interface and properties with their delete-then-write order,
  CABLE_INFO, NF_CABLE_STATS with its version, the snapshot, STATS_FULL, the input checks' order, redaction, the IU SDDL,
  NF_BUILD_ID, and the `nf_vmic.h` and README texts.
- **N1, nf-vmic** (about 2 days): the protocol mirror, `cables.rs` with `resolve_control`, `diagnostics.rs`,
  `Device::open(cable)` and `open_target`, the error names, the selector's forms with their test, `broker::attach_live`'s
  signature, sessions by identity, `mic --cable`.
- **T1, nf-vmic-test** (about 1.5 days): `--cable`, `cables [--watch]`, `stats --cable all`, `probe`, `isolation`, the
  iso scripts.

**Astra's (broker, engine, GUI):** B1, the broker per cable (about 1 day, then a security review); E1, fan-out and the
input guard (about 3 days); G1, the cable list, destinations and combined diagnostics (about 2 days).

**Order:**
1. S1 and N0 with D1.
2. S0a (the owner's go).
3. D2 with N1.
4. T1.
5. S0b, P0, P1w, P3, P4 and P5.
6. B1 with E1.
7. P1 and P2.
8. G1.
9. P5b and P6.
10. R.

When it is built, code comments cite "the owner, 2026-11-24" for the decisions above; the rest is marked ours until the
owner agrees.

### 14. Verifier and IRQL risks
1. **The most-tested code moves.** ATTACH, cancel and cleanup races, and the control device's life, go from globals into
   each context. Keep it mechanical, and re-run the full live set (S0a).
2. **The cable's memory now ends with its binding** while a deleted control device lives on. The rundown guard covers
   cleanup after the purge, with the refusal rules of section 3. Everything those paths call is DISPATCH-safe.
3. **One control device deleted while other cables' devices live: a departure from Learn, ours.**
   - Learn, Using Control Device Objects, Deleting a Control Device Object [V]: "However, the driver must not delete the
     control device objects until after the framework has deleted the other device objects. To determine when the
     framework has deleted the other device objects, your driver should provide EvtCleanupCallback functions for those
     objects." Taken literally, deleting B's control device while A's PnP device lives breaks that rule.
   - Why we think it safe: WDF deletes per object. For a control device, `FxDevice::DeleteObject` deletes the WDM device
     object at once (`MxDeleteDevice`, so no new create reaches it) and defers the framework object's destruction until
     `FxPkgGeneral::CanDestroyControlDevice` sees that device's own open-handle count reach zero; nothing in it is
     driver-wide [V: WDF source, fxdevice.cpp and fxpkggeneral.cpp]. The rule reads as being about unload order. That is
     our reading, not documentation.
   - The gate: S0a's removal cases on B with A attached, under Verifier.
   - **The fallback,** if Verifier or KMDF objects: one control device for the whole driver, made with the first cable and
     deleted in the last cable's context cleanup (today's pattern, which keeps Learn's rule), with a per-cable namespace,
     `\\.\AcxLlCable\<id>`; FILE_DEVICE_SECURE_OPEN applies the device's security to every name under it.
     EvtDeviceFileCreate looks the id up and binds the file to a refcounted per-cable binding object held in its file
     context, and the IOCTLs reach the cable through it; ControlPath carries the namespace path. It brings back a driver-wide list of cables under
     a lock, and the create callback. [C, then: an IU read open of a name under it. Learn says read-only SDDLs grant no
     traversal, so they "might not be an appropriate descriptor for a device with a namespace" [V: Learn, SDDL for Device
     Objects].]
4. **New PASSIVE work in AddDevice:** the instance ID's parse, the interface, its properties (in a system thread
   [V: Learn]), the pool allocations (about 1.05 MB a cable) and the control device. A failure in any of the optional ones (interface,
   properties, control device) costs only that feature.
5. **The fast mutex** is taken only in queue callbacks at PASSIVE (the queue's ExecutionLevel), never under a spin lock.
   The refresh raises to DISPATCH under it.
6. **Verifier's trims** (Force IRQL checking) come with each spin lock raised from PASSIVE (1c8d9d4).
   - Ours: the snapshot cuts STATS to at most 100 refreshes a second per cable, and CABLE_INFO takes no lock of ours.
   - WDF's: every IOCTL through a WDF queue raises at least once, since queuing takes the queue's spin lock [V: WDF source,
     fxioqueue.cpp, FxIoQueue::QueueRequest]. So any interactive user looping on STATS or CABLE_INFO, which IU read now
     allows, causes trims at its IOCTL rate, which would skew TODO 13's timing runs.
   - Hence polling tools stay at about 1 Hz during Verifier runs (`stats --watch`, `cables --watch`, the GUI), and the
     box-idle checklist includes no IOCTL pollers (section 12).
7. **No cross-cable lock nesting.** Verifier's deadlock detection sees more lock instances, not new orders.
8. **ACX and a client interface of another class on its FDO** [C: the device starts, ACX's KS interfaces are unchanged,
   and Audiosrv makes the endpoints].
9. **Paged code:** keep the `noinline` rule on the entry points PAGE code calls that take spin locks (AppRing.h).

### To confirm on the box (collected)
- Discovery properties set in AddDevice are read back once the interface is enabled, by a standard user, and an
  interface answers `DEVPKEY_Device_InstanceId`. `WdfDeviceSetDeviceInterfaceStateEx(FALSE)` in AddDevice keeps the
  interface off at the start.
- What an open of the discovery path does (nothing depends on it).
- An explicit instance ID of our form is accepted by `SetupDiCreateDeviceInfoW`, DIF_REGISTERDEVICE and `DiInstallDevice`,
  and the case Windows keeps. What a removal leaves of it: re-adding with the same n works, and brings back the same
  MMDevice endpoints.
- The FriendlyName set before registration survives the install, an update and a variant switch, and names the endpoints;
  or the driver-set one works (preferred).
- Derived component IDs are accepted.
- A node without an identity shows CM_PROB_FAILED_ADD (31).
- The update procedure: how to read the image unloading, DiInstallDevice on nodes disabled without persistence, the
  holder listing, and what `pnputil /install` alone does.
- The access split under IU (P4), the codes P4's sweep expects, low-integrity reads, and one control device deleted while
  others live, under Verifier (S0a).

### Open questions
**The owner's decisions (2026-11-25, adopting Astra's answers):** Also: the migrated Cable B keeps both microphones; its existing microphone maps to B's low-latency one (an app's saved choice keeps working) and the Call Microphone appears as a new endpoint; a cable stays pending until Windows shows its expected endpoints, after a restart too; a run stops if it can't put back the user's default devices (keeping the new ones is the user's explicit choice). 1, A's counters reset
when A itself restarts, and B removed, restarted or updated alone leaves A's running. 2, a re-add keeps the logical
identity and aims to keep the Windows endpoint IDs on an ordinary same-variant re-add, checked in S0a by default (the
active endpoint IDs compared, a saved selection opening the intended microphone). 3, both microphones by default on every
cable, with a one-mic option. 4, aliases (A, B, D...) in the tools and GUI only, over the stable identity: no kernel names.
5, devices made outside the tool adopted only explicitly (shown, then adopted at the user's choice). Also: treat the first
migration as a device change, recording the old-to-new endpoint mapping and remapping our own saved settings where the
match is unambiguous; today's app opens the old control path, which the new driver removes, so a direct-output app test
needs the branch's app and broker. For release (TODO 23): deleting one cable's control device while A's lives departs
from Learn's documented order; confirm the lifetime arrangement or use the fallback of one shared control device that
routes by cable id.

1. **"A's continue."** We read it as: A's counters run on while other cables are removed and re-added, and A's own
   restart is a new binding like any cable's, with the GUI joining the history. If you meant A's counters survive A's own
   disable, update or variant switch, that needs state kept outside A's device (a global or a registry carry-over), which
   the per-instance design otherwise avoids. Which did you mean?
2. **Endpoints across re-adding (ours, for you to agree).** Re-adding a cable now reuses its instance ID (section 4), so
   we expect Windows to bring back its endpoints [C]. OBS or Discord set to B find B again, with the Sound panel's names
   and settings, and its cached properties (TODO 22) are those it had, from the same INF. A variant switch makes new ones,
   which TODO 22 needs. Keep the endpoints on re-adding, or make re-added cables fresh (n + 1 on every add)?
3. **Microphones on new cables.** `add` defaults to both microphones for every cable (ours), so each cable shows a
   Microphone and a Call Microphone in every app. Or one microphone by default, as B has now?
4. **Kernel alias links.** Your aliases (`\\.\NoiseFilterVmicB` style names, `--cable b`) are served in user mode: the
   selector maps `\\.\NoiseFilterVmic<L>` to label L and `\\.\NoiseFilterVmic` to A. A tool that doesn't use nf-vmic
   can't open those names. Kernel links would serve it, at a cost: a second global name per cable, made and deleted by
   hand beside the control device's; a label held by two nodes (a setup error) leaves one without its alias; and the old
   `\\.\NoiseFilterVmic` would open A in the new layout, which an old tool would misread rather than fail on. (Labels no
   longer change during a binding, so a stale alias isn't one of the costs.) We'd keep them in user mode. Did you mean
   kernel links?
5. **Nodes made outside the tool.** A node from devcon or Device Manager's Add legacy hardware has no identity. Our driver
   fails its AddDevice (CM_PROB_FAILED_ADD) until `Cable.ps1 add <L> -Adopt <instance>` re-creates it, with new
   endpoints. The alternative: the driver makes an identity (`ExUuidCreate`, PASSIVE [V: ntddk.h:10605]), writes it to
   its own hardware key (Device Parameters, which a driver may write), and runs at once as an unlabelled "ACX-LL-Cable"
   until adopted. That keeps an identity in a second place, on a `ROOT\MEDIA\NNNN` that a later node can reuse. Your "a
   plain cable needs no filter setup" reads to us as being about the app and the broker; the tool still makes cables. We'd
   keep failing. Which?
