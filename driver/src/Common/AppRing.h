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
// The one app ring. A global (non-paged .bss, zeroed by the loader) rather than either device's context: the capture streams
// (under the PnP device) and the control device's callbacks both reach it, and it outlives both, since the image stays loaded
// while any device object exists. Every field, and g_NfAppHistory, is guarded by Lock, a spin lock: the capture timer's DPC and
// the cancel routine take it at DISPATCH_LEVEL.
//
// Lock order: Lock, then a LOOPBACK_RING's Lock (Public.h); nothing takes them the other way. Two paths nest them:
// NF_VMIC_IOCTL_STATS (Loopback's counters), and a capture stream's creation and destruction (CaptureStreamOpen and
// CaptureStreamClose, StreamEngine.cpp), which check and record the stream's format in both under the two locks at once.
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
VOID
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
