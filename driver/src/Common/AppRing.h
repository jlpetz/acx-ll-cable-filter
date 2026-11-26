/*++

    noise-filter (TODO 19, spike checks 4 and 5): the driver's side of the app-fed capture ring, and its statistics.
    Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    AppRing.h

Abstract:

    The entry points the stream engine (Common\StreamEngine.cpp), the device (AudioCodec\Driver\Device.cpp) and the control
    device (AudioCodec\Driver\Control.cpp) share: a cable's event logs, the records of its app ring's copy, its timer
    statistics and capture-timestamp trace, and its control device's life. A cable's state is Shared\Cable.h's, in its
    device's context (TODO 19, the per-cable design). The protocol is Shared\nf_vmic.h.

    Include after public.h: it has no include guard, and this file uses its NF_CABLE, LOOPBACK_RING and the WDF types.

Environment:

    Kernel mode

--*/

#pragma once

#include "nf_vmic.h"

//
// The one driver-wide variable (the per-cable design, section 5): stream serials unique across cables within a driver load, so
// "A #17" and "B #17" can't both exist and a merged log stays unambiguous. A label, not a counter: it doesn't reset.
//
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
// The skip log's records of the app ring's copy (Cable.h's NF_APP_SEGMENT and NF_APP_CLAIM; AppRing.cpp). App ring lock held.
// Their entries are in NF_APP_RING.Loopback's skip log, the same cable's, which STATS copies.
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
// A cable's timer statistics (NF_CABLE.StreamStats; AppRing.cpp): interlocked, no lock, no allocation; callable up to
// DISPATCH_LEVEL from any code.
//
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordTimerPass(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONG       Kind,
    _In_    LONGLONG    LatenessHns
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordGlitch(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONG       Kind,
    _In_    ULONGLONG   AdjustHns
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordRun(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONG       Kind
);

// A timer pass's working time, in QPC ticks (NF_VMIC_STREAM_STATS.WorkQpcTicks).
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicRecordPassWork(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONG       Kind,
    _In_    ULONGLONG   QpcTicks
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyStreamStats(
    _In_                        PNF_CABLE               Cable,
    _Out_writes_(NF_VMIC_KINDS) PNF_VMIC_STREAM_STATS   Out
);

//
// A cable's capture-timestamp trace (NF_CABLE.CaptureTrace, NF_VMIC_CAPTURE_TRACE): interlocked, no lock, no allocation;
// callable up to DISPATCH_LEVEL.
//
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureQuery(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONGLONG   QueryQpc,
    _In_    ULONG       Packet,
    _In_    ULONGLONG   PacketQpc
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureNotify(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONGLONG   Packet,
    _In_    ULONGLONG   Qpc
);

// NfVmicTraceCaptureCount's counters.
#define NF_VMIC_TRACE_POSITION          0
#define NF_VMIC_TRACE_CURRENT_PACKET    1

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicTraceCaptureCount(
    _Inout_ PNF_CABLE   Cable,
    _In_    ULONG       Counter
);

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyCaptureTrace(
    _In_    PNF_CABLE               Cable,
    _Out_   PNF_VMIC_CAPTURE_TRACE  Out
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
// A cable's control device (AudioCodec\Driver\Control.cpp; the per-cable design, section 3). noinline: called from PAGE code
// (Codec_EvtBusDeviceAdd, Codec_EvtDeviceContextCleanup's callers) and each takes the ring's spin lock, which inlined into PAGE
// code would be pageable code at DISPATCH_LEVEL (as StreamEngine.h's LoopbackRenderPass says).
//

// Makes the cable's control device, Cable->DeviceName with the link Cable->LinkName (both set by the caller, from the identity).
// A failure costs the feed only: it is reported, kept in Cable->ControlStatus and returned, and the cable plays on as a plain
// cable. Cable->Ring.History must be allocated first: no ring can be published without it.
__declspec(noinline)
__drv_requiresIRQL(PASSIVE_LEVEL)
NTSTATUS
NfVmic_CableAdd(
    _In_    WDFDRIVER   Driver,
    _Inout_ PNF_CABLE   Cable
);

// Ends the cable's feed and deletes its control device, in the design's order: Removing set, the link deleted (if this binding
// made it), the queue purged (the pending ATTACH ends with STATUS_DEVICE_REMOVED), the rundown waited for, the device deleted.
// From the device's context cleanup, before the cable's memory goes; whatever Codec_EvtBusDeviceAdd got to.
__declspec(noinline)
__drv_requiresIRQL(PASSIVE_LEVEL)
VOID
NfVmic_CableCleanup(
    _Inout_ PNF_CABLE   Cable
);
