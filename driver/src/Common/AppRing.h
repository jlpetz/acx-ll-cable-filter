/*++

    noise-filter (TODO 19, spike checks 4 and 5): the driver's side of the app-fed capture ring, and its statistics.
    Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    AppRing.h

Abstract:

    The one app ring's state (g_NfAppRing), the timer statistics (g_NfStreamStats), and the entry points the stream engine
    (Common\StreamEngine.cpp), the device (AudioCodec\Driver\Device.cpp, Driver.cpp) and the control device
    (AudioCodec\Driver\Control.cpp) share. The protocol is Shared\nf_vmic.h.

    Include after public.h: it has no include guard, and this file uses its LOOPBACK_RING and the WDF types.

Environment:

    Kernel mode

--*/

#pragma once

#include "nf_vmic.h"

// Why an attachment ended (NF_VMIC_RING_STATS: DetachCancel, DetachCleanup, DetachRemoval).
#define NF_APP_DETACH_CANCEL    0
#define NF_APP_DETACH_CLEANUP   1
#define NF_APP_DETACH_REMOVAL   2

// The reader's copy for the other capture streams (g_NfAppHistory): 2^15 frames of 48 kHz float32 stereo, 683 ms, 256 KiB.
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
// The one app ring. A global (non-paged .bss, zeroed by the loader) rather than either device's context: the capture streams
// (under the PnP device) and the control device's callbacks both reach it, and it outlives both, since the image stays loaded
// while any device object exists. Every field, and g_NfAppHistory, is guarded by Lock, a spin lock: the capture timer's DPC and
// the cancel routine take it at DISPATCH_LEVEL.
//
// Lock order: Lock, then a LOOPBACK_RING's Lock (Public.h); nothing takes them the other way. Two paths nest them:
// NF_VMIC_IOCTL_STATS (Loopback's counters), and a capture stream's creation and destruction (CaptureStreamOpen and
// CaptureStreamClose, StreamEngine.cpp), which check and record the stream's format in both under the two locks at once.
// An event log's lock (NF_EVENT_LOG) comes last: taken inside either or both, and nothing is taken inside it. STATS copies
// the logs inside both, so the counters it copies agree with them.
// The loopback's records of what is open are the CaptureOpen and RenderOpen slots in each device's LOOPBACK_RING, under that
// ring's lock. The app ring keeps its own count of capture streams open in another format (OtherFormatCaptureOpens), across
// every device, since any device's capture stream reads the one app ring: ATTACH checks it under Lock alone.
//
typedef struct _NF_APP_RING {
    KSPIN_LOCK              Lock;                   // KeInitializeSpinLock in NfVmic_DriverInit

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
    // The reader's copy (g_NfAppHistory), reset at ATTACH: the packets the reader delivered, each at its frames on the QPC
    // time line (as LOOPBACK_RING's positions: frame n is n / 48000 s of QPC time, in slot n & (NF_APP_HISTORY_FRAMES - 1)).
    // The other capture streams read it HistoryLeadFrames plus 1 ms and a frame behind their own place on that time line, and
    // never at a frame they have had (StreamEngine.cpp, AppRingPlayCopy); a pausing one keeps the lag it has.
    //
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

    // Capture streams open (EvtCircuitCreateStream to the stream engine's destruction) in a format other than the ring's, on
    // every device: ATTACH is refused while this isn't 0, and no such stream is created while a ring is attached.
    ULONG                   OtherFormatCaptureOpens;

    NF_VMIC_RING_STATS      Stats;                  // cumulative since the driver loaded
    ULONGLONG               SecondaryFramesCopied;  // NF_VMIC_STATS's, cumulative too
    ULONGLONG               SecondaryUnderrunFrames;
    ULONGLONG               ReaderHandoffs;         // NF_VMIC_STATS's: a stream playing the copy took the slot another held
    ULONGLONG               HandoffSkippedFrames;   // the copy's frames such a stream never delivered (its lag at most)
    ULONGLONG               SecondarySkips;         // NF_VMIC_STATS's: a stream playing the copy moved on to a shorter reader packet
    ULONGLONG               SecondarySkippedFrames; // the copy's frames such a stream passed over (the packets' difference at most)

    BOOLEAN                 Removing;               // the control device is being deleted: no new attachment

    // The loopback ring of the PnP device that registered first (STATS reads its counters); cleared in that device's context
    // cleanup, before its context goes.
    PLOOPBACK_RING          Loopback;
    WDFDEVICE               LoopbackOwner;
} NF_APP_RING, *PNF_APP_RING;

extern NF_APP_RING              g_NfAppRing;
extern UCHAR                    g_NfAppHistory[NF_APP_HISTORY_BYTES];   // non-paged .bss; under g_NfAppRing.Lock
extern NF_VMIC_STREAM_STATS     g_NfStreamStats[NF_VMIC_KINDS];     // interlocked access only
extern NF_VMIC_CAPTURE_TRACE    g_NfCaptureTrace;                   // interlocked access only
extern volatile LONG            g_NfStreamSerial;                   // the last stream serial given (NF_VMIC_EVENT.Stream); interlocked

//
// The event logs (Public.h's NF_EVENT_LOG; nf_vmic.h, "The event logs"; AppRing.cpp): each takes its own spin lock, the last in
// every lock order, so they are callable up to DISPATCH_LEVEL from any code, other spin locks held or not. No allocation. A
// NULL log (no device) logs nothing: the logging returns 0, the updates FALSE.
//

// Logs Event (its Sequence and Qpc set here) and returns its Sequence.
__drv_maxIRQL(DISPATCH_LEVEL)
ULONGLONG
NfVmicLogEvent(
    _Inout_opt_ PNF_EVENT_LOG       Log,
    _In_        const NF_VMIC_EVENT* Event
);

// A run's entry, extended: Frames more (saturating), its span to SpanEnd, Flags added. FALSE: the entry is gone (overwritten).
__drv_maxIRQL(DISPATCH_LEVEL)
BOOLEAN
NfVmicLogExtend(
    _Inout_opt_ PNF_EVENT_LOG       Log,
    _In_        ULONGLONG           Sequence,
    _In_        ULONG               Frames,
    _In_        ULONGLONG           SpanEnd,
    _In_        USHORT              Flags
);

// The entry's ring span takes in [RingFrom, RingTo): set with NF_VMIC_EVENTF_RING if it had none, else extended to RingTo.
__drv_maxIRQL(DISPATCH_LEVEL)
BOOLEAN
NfVmicLogRingSpan(
    _Inout_opt_ PNF_EVENT_LOG       Log,
    _In_        ULONGLONG           Sequence,
    _In_        ULONGLONG           RingFrom,
    _In_        ULONGLONG           RingTo
);

__drv_maxIRQL(DISPATCH_LEVEL)
BOOLEAN
NfVmicLogFlags(
    _Inout_opt_ PNF_EVENT_LOG       Log,
    _In_        ULONGLONG           Sequence,
    _In_        USHORT              Set,
    _In_        USHORT              Clear
);

// The log's slots, whole, and its count, at one instant (STATS). Entries: Out's length, the log's.
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyEventLog(
    _Inout_                     PNF_EVENT_LOG   Log,
    _Out_writes_(Entries)       PNF_VMIC_EVENT  Out,
    _In_                        ULONG           Entries,
    _Out_                       PUINT64         Logged
);

//
// The skip log's records of the app ring's copy (AppRing.h's NF_APP_SEGMENT and NF_APP_CLAIM; AppRing.cpp). App ring lock held.
// Their entries are in NF_APP_RING.Loopback's skip log, the app feed device's, which STATS copies.
//

// The copy's audio written at time-line [From, To), from the ring's frame RingFrame: mapped, and joined to the claims it fills.
__drv_requiresIRQL(DISPATCH_LEVEL)
VOID
NfVmicAppRingAudioWritten(
    _Inout_ PNF_APP_RING    Ring,
    _In_    ULONGLONG       From,
    _In_    ULONGLONG       To,
    _In_    ULONGLONG       RingFrame
);

// The ring's frames the copy holds as audio in time-line [Lo, Hi): FALSE if none. *Whole: FALSE where part of the span may be
// audio whose record was overwritten (the span is then UNTRACKED).
__drv_requiresIRQL(DISPATCH_LEVEL)
BOOLEAN
NfVmicAppRingSpan(
    _In_    PNF_APP_RING    Ring,
    _In_    ULONGLONG       Lo,
    _In_    ULONGLONG       Hi,
    _Out_   PULONGLONG      RingFrom,
    _Out_   PULONGLONG      RingTo,
    _Out_   PBOOLEAN        Whole
);

// Claims [Lo, Hi) for the skip-log entry Sequence (PENDING), or marks it UNTRACKED if no claim is free.
__drv_requiresIRQL(DISPATCH_LEVEL)
VOID
NfVmicAppRingClaim(
    _Inout_ PNF_APP_RING    Ring,
    _In_    ULONGLONG       Lo,
    _In_    ULONGLONG       Hi,
    _In_    ULONGLONG       Sequence
);

// Sequence's claim, if it ends at Hi, ends at NewHi instead. FALSE: no such claim (settled, or never made).
__drv_requiresIRQL(DISPATCH_LEVEL)
BOOLEAN
NfVmicAppRingClaimExtend(
    _Inout_ PNF_APP_RING    Ring,
    _In_    ULONGLONG       Sequence,
    _In_    ULONGLONG       Hi,
    _In_    ULONGLONG       NewHi
);

// Settles the claims the copy's end has passed (All: every claim, as the attachment ends), clearing their entries' PENDING.
__drv_requiresIRQL(DISPATCH_LEVEL)
VOID
NfVmicAppRingSettleClaims(
    _Inout_ PNF_APP_RING    Ring,
    _In_    BOOLEAN         All
);

// Clears the copy's map (a new run of the copy, or ATTACH).
__drv_requiresIRQL(DISPATCH_LEVEL)
VOID
NfVmicAppRingMapReset(
    _Inout_ PNF_APP_RING    Ring
);

//
// Timer statistics (AppRing.cpp): interlocked, no lock, no allocation; callable up to DISPATCH_LEVEL from any code.
//
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordTimerPass(
    _In_ ULONG      Kind,
    _In_ LONGLONG   LatenessHns
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordGlitch(
    _In_ ULONG      Kind,
    _In_ ULONGLONG  AdjustHns
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordRun(
    _In_ ULONG      Kind
);

// A timer pass's working time, in QPC ticks (NF_VMIC_STREAM_STATS.WorkQpcTicks).
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordPassWork(
    _In_ ULONG      Kind,
    _In_ ULONGLONG  QpcTicks
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyStreamStats(
    _Out_writes_(NF_VMIC_KINDS) PNF_VMIC_STREAM_STATS Out
);

//
// The capture-timestamp trace (NF_VMIC_CAPTURE_TRACE): interlocked, no lock, no allocation; callable up to DISPATCH_LEVEL.
//
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureQuery(
    _In_ ULONGLONG  QueryQpc,
    _In_ ULONG      Packet,
    _In_ ULONGLONG  PacketQpc
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureNotify(
    _In_ ULONGLONG  Packet,
    _In_ ULONGLONG  Qpc
);

// NfVmicTraceCaptureCount's counters.
#define NF_VMIC_TRACE_POSITION          0
#define NF_VMIC_TRACE_CURRENT_PACKET    1

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureCount(
    _In_ ULONG      Counter
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyCaptureTrace(
    _Out_ PNF_VMIC_CAPTURE_TRACE Out
);

//
// The header's fields are UINT32 and UINT64 (nf_vmic.h); the WDK's volatile accessors take ULONG and ULONG64 pointers.
//
FORCEINLINE
VOID
NfVmicStore32(
    volatile UINT32*    Destination,
    UINT32              Value
)
{
    WriteULongNoFence((volatile ULONG*)Destination, (ULONG)Value);
}

FORCEINLINE
VOID
NfVmicStore64(
    volatile UINT64*    Destination,
    UINT64              Value
)
{
    WriteULong64NoFence(Destination, Value);
}

FORCEINLINE
VOID
NfVmicStoreRelease64(
    volatile UINT64*    Destination,
    UINT64              Value
)
{
    WriteULong64Release(Destination, Value);
}

FORCEINLINE
UINT64
NfVmicLoadAcquire64(
    const volatile UINT64*  Source
)
{
    return ReadULong64Acquire(Source);
}

//
// The control device (AudioCodec\Driver\Control.cpp). noinline: called from INIT and PAGE code (DriverEntry,
// Codec_EvtBusDeviceAdd) and each takes the ring's spin lock, which inlined into PAGE code would be pageable code at
// DISPATCH_LEVEL (as StreamEngine.h's LoopbackRenderPass says).
//
__declspec(noinline)
__drv_requiresIRQL(PASSIVE_LEVEL)
NTSTATUS
NfVmic_DriverInit(
    _In_ WDFDRIVER      Driver
);

__declspec(noinline)
__drv_requiresIRQL(PASSIVE_LEVEL)
NTSTATUS
NfVmic_DeviceAdd(
    _In_ WDFDRIVER      Driver,
    _In_ WDFDEVICE      Device,
    _In_ PLOOPBACK_RING Loopback
);

__declspec(noinline)
__drv_requiresIRQL(PASSIVE_LEVEL)
VOID
NfVmic_DeviceCleanup(
    _In_ WDFDEVICE      Device
);
