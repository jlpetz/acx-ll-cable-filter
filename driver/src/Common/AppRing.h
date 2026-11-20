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

//
// The one app ring. A global (non-paged .bss, zeroed by the loader) rather than either device's context: the capture streams
// (under the PnP device) and the control device's callbacks both reach it, and it outlives both, since the image stays loaded
// while any device object exists. Every field is guarded by Lock, a spin lock: the capture timer's DPC and the cancel routine
// take it at DISPATCH_LEVEL. Lock order: Lock, then Loopback->Lock (NF_VMIC_IOCTL_STATS only); nothing takes them the other way.
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

    NF_VMIC_RING_STATS      Stats;                  // cumulative since the driver loaded

    BOOLEAN                 Removing;               // the control device is being deleted: no new attachment

    // The loopback ring of the PnP device that registered first (STATS reads its counters); cleared in that device's context
    // cleanup, before its context goes.
    PLOOPBACK_RING          Loopback;
    WDFDEVICE               LoopbackOwner;
} NF_APP_RING, *PNF_APP_RING;

extern NF_APP_RING              g_NfAppRing;
extern NF_VMIC_STREAM_STATS     g_NfStreamStats[NF_VMIC_KINDS];     // interlocked access only

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

__drv_maxIRQL(DISPATCH_LEVEL)
VOID
NfVmicCopyStreamStats(
    _Out_writes_(NF_VMIC_KINDS) PNF_VMIC_STREAM_STATS Out
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
