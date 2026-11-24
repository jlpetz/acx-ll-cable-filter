/*++

    noise-filter (TODO 19, the per-cable design, doc/todo/19-per-cable-design.md, sections 3 to 5). Original to noise-filter,
    not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    Cable.h

Abstract:

    One cable's state: everything that was driver-wide before (the app ring, the reader's copy, their counters, the timer
    statistics, the capture-timestamp trace) and what names the cable (its identity, label and instance ID), kept in each
    device's context (CODEC_DEVICE_CONTEXT.Cable, Public.h) beside its loopback ring. Nothing global points at a cable; only
    the stream serial stays driver-wide (AppRing.h, g_NfStreamSerial).

    Included by Public.h after LOOPBACK_RING and NF_EVENT_LOG, and before CODEC_DEVICE_CONTEXT. Types only.

Environment:

    Kernel mode

--*/

#pragma once

// Why an attachment ended (NF_VMIC_RING_STATS: DetachCancel, DetachCleanup, DetachRemoval).
#define NF_APP_DETACH_CANCEL    0
#define NF_APP_DETACH_CLEANUP   1
#define NF_APP_DETACH_REMOVAL   2

// The reader's copy for the other capture streams (NF_APP_RING.History): 2^15 frames of 48 kHz float32 stereo, 683 ms, 256 KiB.
#define NF_APP_HISTORY_FRAMES   32768u
#define NF_APP_HISTORY_BYTES    (NF_APP_HISTORY_FRAMES * NF_VMIC_FRAME_BYTES)

//
// The latest gaps in the reader's copy (NF_APP_RING.HistoryGaps): silence the copy holds where the reader's time line jumped
// later (a glitch adjustment, a handoff), which the other streams count as underruns, not as the ring's audio. A whole reader
// packet follows each gap, so at most (its packet + its timer's lateness + the lag) / the reader's packet + 1 gaps end after
// the start of another stream's read: 11 for a 10 ms stream (lateness under 10 ms) beside a 2 ms reader (the default build's
// shortest packet, PacketSize.cpp) whose time line jumps at every pass, 22 beside a 1 ms one (the 96-frame trial build's floor).
// Should more than 32 do, the older ones' silence counts as copied.
//
#define NF_APP_HISTORY_GAPS     32u

typedef struct _NF_APP_HISTORY_GAP {
    ULONGLONG               From;                   // [From, End), frames on the time line; [0, 0) when unused
    ULONGLONG               End;
} NF_APP_HISTORY_GAP, *PNF_APP_HISTORY_GAP;

//
// The copy's map from its time line to the ring's frames (NF_APP_RING.Segments), for the skip log's ring spans (nf_vmic.h, "The
// event logs"): each record a run of audio the reader wrote, time-line frames [From, To) holding the ring's frames from Ring
// on. A run grows while the reader's audio follows on in both (its packets one after another, no underrun between); an
// underrun, a gap in its time line, a handoff or a resync starts another. The latest 64 are kept: with an underrun every other
// pass of a 96-frame reader (two records a pass) they reach 32 ms back, and a skip's span lies within a lag and a packet of
// the copy's end. A span reaching before what an overwritten record covered (SegmentsLostTo) is marked UNTRACKED.
//
#define NF_APP_SEGMENTS         64u

typedef struct _NF_APP_SEGMENT {
    ULONGLONG               From;                   // [From, To), frames on the time line; [0, 0) when unused
    ULONGLONG               To;
    ULONGLONG               Ring;                   // the ring's frame at From
} NF_APP_SEGMENT, *PNF_APP_SEGMENT;

//
// A span of the time line the copy didn't hold yet when a stream passed over it (a late read, or a skip or jump past the copy's
// end), claimed for its skip-log entry (NF_APP_RING.Claims): the audio the reader writes there joins the entry's ring span,
// and once the copy's end passes Hi the claim is settled (the entry's PENDING cleared). 16 at once (a few a stream); a span
// with none free is marked UNTRACKED.
//
#define NF_APP_CLAIMS           16u

typedef struct _NF_APP_CLAIM {
    ULONGLONG               Lo;                     // [Lo, Hi), frames on the time line
    ULONGLONG               Hi;
    ULONGLONG               Sequence;               // the entry's in the skip log; 0: a free claim
} NF_APP_CLAIM, *PNF_APP_CLAIM;

//
// A cable's app ring (one per cable: NF_CABLE.Ring, in the device's context, non-paged). The capture streams (under the PnP
// device) and the control device's callbacks (Control.cpp) both reach it; the control device can outlive the context (a
// deleted control device lives until its last handle closes), which its rundown guard covers (Control.cpp,
// NF_CONTROL_CONTEXT). Every field, and the copy History points at, is guarded by Lock, a spin lock: the capture timer's DPC
// and the cancel routine take it at DISPATCH_LEVEL.
//
// Lock order, per cable: Lock, then the same context's LOOPBACK_RING Lock (Public.h); nothing takes them the other way. Two
// paths nest them: NF_VMIC_IOCTL_STATS (Loopback's counters), and a capture stream's creation and destruction
// (CaptureStreamOpen and CaptureStreamClose, StreamEngine.cpp), which check and record the stream's format in both under the
// two locks at once. An event log's lock (NF_EVENT_LOG) comes last: taken inside either or both, and nothing is taken inside
// it. STATS copies the logs inside both, so the counters it copies agree with them. No path holds two cables' locks.
// The loopback's records of what is open are the CaptureOpen and RenderOpen slots in its LOOPBACK_RING, under that ring's
// lock. The app ring keeps its own count of its cable's capture streams open in another format (OtherFormatCaptureOpens):
// ATTACH checks it under Lock alone.
//
typedef struct _NF_APP_RING {
    KSPIN_LOCK              Lock;                   // KeInitializeSpinLock in Codec_EvtBusDeviceAdd

    // The attachment while one is published; all NULL or 0 otherwise.
    WDFREQUEST              Request;                // the pending ATTACH: driver-owned and cancelable
    WDFFILEOBJECT           File;                   // its file object (EvtFileCleanup ends only its own)
    PNF_VMIC_RING_HEADER    Header;                 // the app's ring in system space (no-execute): its header,
    PUCHAR                  Data;                   // and its frames, Header + NF_VMIC_HEADER_BYTES
    ULONG                   CapacityFrames;         // from the header's copy at ATTACH; never read from the ring again
    ULONG                   AttachPid;
    BOOLEAN                 LiveFeed;               // opt-in NF_VMIC_ATTACH_LIVE
    ULONG                   LiveStreams;            // streams that passed since the end of the last idle interval
    ULONGLONG               ClockSequence;          // producer-owned, never read from the app mapping
    ULONGLONG               ReadFrame;              // the driver's position: stored to Header->ReadFrame, never read back
    ULONGLONG               LastWriteSeen;          // the WriteFrame the latest pass read (STATS; untrusted)
    PVOID                   Reader;                 // the CCaptureStreamEngine holding the reader slot, or NULL
    ULONG                   ReaderState;            // the State bits it last wrote to the header (0: no reader)
    ULONG                   ReaderPacketFrames;     // the PacketFrames it last wrote; STATS reports both, so the two agree
    ULONG                   AttachResyncs;          // this attachment's counters: stored to the header, never read back
    ULONG                   AttachMismatchPackets;
    ULONGLONG               AttachPasses;
    ULONGLONG               AttachFramesCopied;
    ULONGLONG               AttachUnderrunFrames;

    //
    // The reader's copy, reset at ATTACH: the packets the reader delivered, each at its frames on the QPC time line (as
    // LOOPBACK_RING's positions: frame n is n / 48000 s of QPC time, in slot n & (NF_APP_HISTORY_FRAMES - 1) of History). The
    // other capture streams read it HistoryLeadFrames plus 1 ms and a frame behind their own place on that time line, and
    // never at a frame they have had (StreamEngine.cpp, AppRingPlayCopy); a pausing one keeps the lag it has.
    //
    PUCHAR                  History;                // NF_APP_HISTORY_BYTES, non-paged: Codec_EvtBusDeviceAdd to the context
                                                    // cleanup; never NULL while a ring is published (the control device is
                                                    // made only once it is allocated)
    BOOLEAN                 HistoryValid;           // a reader in the ring's format has written since ATTACH
    ULONG                   HistoryLeadFrames;      // the packet of the reader that wrote last, in frames (0: none yet)
    ULONGLONG               HistoryFrom;            // [HistoryFrom, HistoryEnd) is the current run (only its last
    ULONGLONG               HistoryEnd;             // NF_APP_HISTORY_FRAMES held)
    ULONG                   HistoryGapNext;         // the entry of HistoryGaps the next gap fills (modulo NF_APP_HISTORY_GAPS)
    NF_APP_HISTORY_GAP      HistoryGaps[NF_APP_HISTORY_GAPS];   // the current run's latest gaps, cleared with each new run

    // The skip log's records of the copy (above): its map to the ring's frames, cleared with each new run and at ATTACH, and
    // the spans being filled in, settled at detach.
    ULONGLONG               SegmentNext;            // the record of Segments the next run of audio fills (modulo NF_APP_SEGMENTS)
    ULONGLONG               SegmentsLostTo;         // the latest To of a record overwritten in this run (0: none)
    NF_APP_SEGMENT          Segments[NF_APP_SEGMENTS];
    NF_APP_CLAIM            Claims[NF_APP_CLAIMS];

    // This cable's capture streams open (EvtCircuitCreateStream to the stream engine's destruction) in a format other than the
    // ring's: ATTACH is refused while this isn't 0, and no such stream is created while a ring is attached.
    ULONG                   OtherFormatCaptureOpens;

    // Since the binding (AddDevice: the context is zeroed), not since the driver loaded.
    NF_VMIC_RING_STATS      Stats;
    ULONGLONG               SecondaryFramesCopied;  // NF_VMIC_STATS's
    ULONGLONG               SecondaryUnderrunFrames;
    ULONGLONG               ReaderHandoffs;         // NF_VMIC_STATS's: a stream playing the copy took the slot another held
    ULONGLONG               HandoffSkippedFrames;   // the copy's frames such a stream never delivered (its lag at most)
    ULONGLONG               SecondarySkips;         // NF_VMIC_STATS's: a stream playing the copy moved on to a shorter reader packet
    ULONGLONG               SecondarySkippedFrames; // the copy's frames such a stream passed over (the packets' difference at most)
    //
    // The copy's jumps in a capture stream's own time line (NF_VMIC_EVENT_SECONDARY_JUMP), and the copy's frames they passed
    // over (each entry's Frames: what the copy held there as it was logged; a part past its end then joins only the entry's
    // ring span, nf_vmic.h), counted where each is logged (StreamEngine.cpp, AppRingOtherPass), under Lock as their entries
    // are. They were the loopback ring's until the per-cable design moved them here: they are the copy's (the owner,
    // 2026-11-24: two totals per device, so the log's cross-check covers jumps too).
    //
    ULONGLONG               SecondaryJumps;
    ULONGLONG               SecondaryJumpFrames;

    BOOLEAN                 Removing;               // the cable's binding is ending: no new attachment, ever (set once)

    // The same context's loopback ring, whose event logs take this ring's events and whose counters STATS reads. Fixed in
    // Codec_EvtBusDeviceAdd, before anything else can reach the ring, and never cleared: both live in one context.
    PLOOPBACK_RING          Loopback;
} NF_APP_RING, *PNF_APP_RING;

//
// A cable's names (TODO 19, the per-cable design, section 4). The setup tool creates each cable's device node with the
// instance ID ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>: label one to eight of A-Z and 0-9, id the identity's 36 characters (a GUID
// without braces), n a decimal instance number from 1. The driver reads it in AddDevice (DEVPKEY_Device_InstanceId) and
// parses what follows the last backslash, without case.
//
#define NF_CABLE_INSTANCE_PREFIX        L"ACXLLCABLE_"
#define NF_CABLE_LABEL_MAX_CHARS        8u
#define NF_CABLE_ID_CHARS               36u
#define NF_CABLE_INSTANCE_ID_CHARS      200u        // MAX_DEVICE_ID_LEN (cfgmgr32.h), its NUL included
#define NF_CABLE_NAME_CHARS             64u         // \DosDevices\AcxLlCable_<id> is 59 characters

//
// The boot crash breaker (Device.cpp; ours, for the owner's go, removable): a device's start attempts that haven't completed, a
// REG_DWORD in its hardware key (Device Parameters; absent is 0), which the setup tool deletes when it installs a cable; and
// the status AddDevice refuses a device with once two starts in a row never completed (DEVPKEY_Device_ProblemStatus beside
// code 31: a customer-defined NTSTATUS, facility 0xAC).
//
#define NF_CABLE_START_ATTEMPTS_VALUE   L"NfStartAttempts"
#define NF_CABLE_START_ATTEMPTS_LIMIT   2u
#define NF_STATUS_START_BREAKER         ((NTSTATUS)0xE0AC0001L)

// The microphones a device makes (Device.cpp, NfReadMicrophones; the INF's NfMicrophones): Microphone0 and Microphone1.
#define NF_MIC_LOW_LATENCY              0x1
#define NF_MIC_CALL                     0x2

//
// One cable, in its device's context (non-paged, zeroed by the framework: every counter and log starts at zero with the
// binding). A binding is one framework device, from AddDevice to the context cleanup.
//
typedef struct _NF_CABLE {
    // The binding: set in Codec_EvtBusDeviceAdd, before any stream or control device exists, and never changed.
    GUID                    Id;                                     // the identity, from the instance ID
    WCHAR                   IdText[NF_CABLE_ID_CHARS + 1];          // the identity in lower case: the control device's name
    WCHAR                   Label[NF_CABLE_LABEL_MAX_CHARS + 1];    // "D", upper case
    ULONG                   InstanceNumber;                         // n
    WCHAR                   InstanceId[NF_CABLE_INSTANCE_ID_CHARS]; // as Plug and Play gave it, NUL-terminated
    ULONGLONG               Generation;                             // KeQuerySystemTimePrecise at AddDevice (100 ns UTC)
    ULONGLONG               BindQpc;                                // KeQueryPerformanceCounter at AddDevice
    ULONG                   Microphones;                            // NF_MIC_* (the hardware ID's NfMicrophones)
    GUID                    RenderComponentId;                      // the identity XOR each circuit's constant
    GUID                    CaptureComponentId;                     // (DriverSettings.h): unique per cable
    GUID                    Capture2ComponentId;

    // The app feed (Control.cpp, StreamEngine.cpp).
    NF_APP_RING             Ring;
    NF_VMIC_STREAM_STATS    StreamStats[NF_VMIC_KINDS];             // the cable's streams' timers: interlocked access only
    NF_VMIC_CAPTURE_TRACE   CaptureTrace;                           // its capture streams' timestamps: interlocked only

    // The cable's control device (Control.cpp): \Device\AcxLlCable_<id>, link \DosDevices\AcxLlCable_<id>. PASSIVE_LEVEL
    // only (AddDevice and the context cleanup).
    WDFDEVICE               ControlDevice;                          // NULL: none (ControlStatus says why)
    WDFQUEUE                ControlQueue;
    NTSTATUS                ControlStatus;                          // STATUS_SUCCESS, or why there's no control device
    BOOLEAN                 LinkCreated;                            // this binding made LinkName: only it deletes it
    WCHAR                   DeviceName[NF_CABLE_NAME_CHARS];
    WCHAR                   LinkName[NF_CABLE_NAME_CHARS];
} NF_CABLE, *PNF_CABLE;
