/*++

    noise-filter (TODO 19, spike checks 4 and 5). Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    Control.cpp

Abstract:

    The control device that takes an app's capture ring (NF_VMIC_IOCTL_ATTACH) and hands out the driver's statistics
    (NF_VMIC_IOCTL_STATS). The protocol and its rules are Shared\nf_vmic.h; the capture side is
    CCaptureStreamEngine::AppRingCapturePass (Common\StreamEngine.cpp).

    Lifetime (Learn, wdf/using-control-device-objects, "Deleting a Control Device Object"): a driver with both a control
    device and PnP devices must delete the control device itself, at PASSIVE_LEVEL, and only after the framework has deleted
    the PnP devices, which it learns from their EvtCleanupCallback; until then the driver can't unload. So it is made with the
    first PnP device (Codec_EvtBusDeviceAdd) and deleted in the last one's context cleanup (Codec_EvtDeviceContextCleanup),
    as the KMDF toaster's sideband filter does (general/toaster/toastDrv/kmdf/filter/sideband/filter.c). The framework
    destroys a deleted control device only once the last handle to it closes, so the driver makes and deletes the symbolic
    link itself, and a PnP device that comes back gets a new control device under the same name while an old handle is still
    open (NfVmic_CreateControlDevice).

    Nothing here is pageable: the cancel routine runs at up to DISPATCH_LEVEL, the IOCTL callback may (parallel queue, no
    execution level set), and every function takes the ring's spin lock.

Environment:

    Kernel mode

--*/

#include "public.h"
#include "AppRing.h"

//
// Only SYSTEM and Administrators can open this control device. For a standard user, nf-vmic's explicitly approved broker
// owns the handle, checks the interactive session and transfers only the live ring pages to its parent. STATS stays privileged.
//
DECLARE_CONST_UNICODE_STRING(g_NfVmicSddl, L"D:P(A;;GA;;;SY)(A;;GA;;;BA)");
DECLARE_CONST_UNICODE_STRING(g_NfVmicDeviceName, NF_VMIC_DEVICE_NAME);
DECLARE_CONST_UNICODE_STRING(g_NfVmicSymlinkName, NF_VMIC_SYMLINK_NAME);

//
// The control device's life, guarded by s_CdoLock (a wait lock: WdfDeviceCreate and the rest need PASSIVE_LEVEL, which a
// spin lock or a fast mutex wouldn't leave). s_DeviceCount is 0 or 1, excluding the independent WASAPI-only cable.
//
static WDFWAITLOCK  s_CdoLock;
static ULONG        s_DeviceCount;
static WDFDEVICE    s_ControlDevice;
static WDFQUEUE     s_ControlQueue;

//
// Each control-device file object's context (zeroed by the framework). CleanedUp: its EvtFileCleanup has run, so no ring may be
// published on it any more (NfVmic_Attach says why). Set and read under g_NfAppRing.Lock.
//
typedef struct _NF_VMIC_FILE_CONTEXT {
    BOOLEAN CleanedUp;
} NF_VMIC_FILE_CONTEXT, *PNF_VMIC_FILE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(NF_VMIC_FILE_CONTEXT, NfVmicGetFileContext);

static EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL   NfVmic_EvtIoDeviceControl;
static EVT_WDF_REQUEST_CANCEL               NfVmic_EvtRequestCancel;
static EVT_WDF_FILE_CLEANUP                 NfVmic_EvtFileCleanup;

_Use_decl_annotations_
NTSTATUS
NfVmic_DriverInit(
    WDFDRIVER Driver
)
{
    UNREFERENCED_PARAMETER(Driver);

    KeInitializeSpinLock(&g_NfAppRing.Lock);

    // Parented to the driver object by default; it goes with it.
    return WdfWaitLockCreate(WDF_NO_OBJECT_ATTRIBUTES, &s_CdoLock);
}

//
// Unpublishes the attachment. Lock held. The ring stays mapped and locked until its request completes, so the last writes to
// the header here are safe; after this no capture pass can find the ring.
//
static
VOID
NfVmic_UnpublishLocked(
    _In_ ULONG Reason
)
{
    PNF_APP_RING ring = &g_NfAppRing;

    if (ring->Header != NULL)
    {
        NfVmicStore32(&ring->Header->State, 0);
        NfVmicStore32(&ring->Header->PacketFrames, 0);
    }

    // No reader writes this attachment's copy again: the skip log's spans still being filled in are settled as they stand.
    NfVmicAppRingSettleClaims(ring, TRUE);

    ring->Request = NULL;
    ring->File = NULL;
    ring->Header = NULL;
    ring->Data = NULL;
    ring->CapacityFrames = 0;
    ring->AttachPid = 0;
    ring->Reader = NULL;
    ring->ReaderState = 0;
    ring->ReaderPacketFrames = 0;

    ring->Stats.Detaches++;
    switch (Reason)
    {
    case NF_APP_DETACH_CANCEL:  ring->Stats.DetachCancel++;     break;
    case NF_APP_DETACH_CLEANUP: ring->Stats.DetachCleanup++;    break;
    default:                    ring->Stats.DetachRemoval++;    break;
    }
}

//
// The framework calls this when the pending ATTACH is cancelled: CancelIoEx, the issuing thread or process exiting, or the
// queue's purge when the control device goes (NfVmic_DeleteControlDevice, with Removing set). It takes the lock before it
// completes the request, so a detach elsewhere that still finds the request published (and calls WdfRequestUnmarkCancelable
// under the same lock) can't race the completion. A removal completes with STATUS_DEVICE_REMOVED rather than STATUS_CANCELLED,
// so the app can tell it from its own cancel: it must then close its handles, since an open one keeps the driver loaded.
//
_Use_decl_annotations_
static
VOID
NfVmic_EvtRequestCancel(
    WDFREQUEST Request
)
{
    PNF_APP_RING    ring = &g_NfAppRing;
    KIRQL           oldIrql;
    BOOLEAN         unpublished = FALSE;
    BOOLEAN         removing;

    KeAcquireSpinLock(&ring->Lock, &oldIrql);
    removing = ring->Removing;
    if (ring->Request == Request)
    {
        NfVmic_UnpublishLocked(removing ? NF_APP_DETACH_REMOVAL : NF_APP_DETACH_CANCEL);
        unpublished = TRUE;
    }
    // Else a detach elsewhere took it first: WdfRequestUnmarkCancelable returned STATUS_CANCELLED there, so completing it is ours.
    KeReleaseSpinLock(&ring->Lock, oldIrql);

    if (unpublished)
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "AudioCodec: app ring detached (%s)\n",
                   removing ? "the device is going away" : "cancelled");
    }

    // Completion unlocks the pages, which also releases the system mapping (Learn, MmGetSystemAddressForMdlSafe); nothing
    // can reach the ring any more.
    WdfRequestComplete(Request, removing ? STATUS_DEVICE_REMOVED : STATUS_CANCELLED);
}

//
// EvtFileCleanup's work: marks File cleaned up, so no ring is published on it from now on, and ends the attachment if File's
// request holds it (the handle closed with the ATTACH still pending).
//
static
VOID
NfVmic_DetachFile(
    _In_ WDFFILEOBJECT File
)
{
    PNF_APP_RING    ring = &g_NfAppRing;
    KIRQL           oldIrql;
    WDFREQUEST      request = NULL;
    NTSTATUS        status = STATUS_SUCCESS;

    KeAcquireSpinLock(&ring->Lock, &oldIrql);
    // In the same locked section as the detach: an ATTACH on File still on its way either was published before this (and is
    // ended below) or finds the flag and is refused.
    NfVmicGetFileContext(File)->CleanedUp = TRUE;
    if (ring->Request != NULL && ring->File == File)
    {
        request = ring->Request;
        // Under the lock: the cancel routine takes it before it completes the request, so the request is still ours here
        // (Learn, WdfRequestUnmarkCancelable: never call it after EvtRequestCancel has completed the request).
        status = WdfRequestUnmarkCancelable(request);
        NfVmic_UnpublishLocked(NF_APP_DETACH_CLEANUP);
    }
    KeReleaseSpinLock(&ring->Lock, oldIrql);

    if (request != NULL)
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "AudioCodec: app ring detached (handle closed)\n");

        // STATUS_CANCELLED: the cancel routine is on its way and completes the request (finding it unpublished).
        if (status != STATUS_CANCELLED)
        {
            WdfRequestComplete(request, STATUS_CANCELLED);
        }
    }
}

_Use_decl_annotations_
static
VOID
NfVmic_EvtFileCleanup(
    WDFFILEOBJECT FileObject
)
{
    NfVmic_DetachFile(FileObject);
}

static
VOID
NfVmic_CountReject(
    _In_ NTSTATUS Status
)
{
    KIRQL oldIrql;

    KeAcquireSpinLock(&g_NfAppRing.Lock, &oldIrql);
    if (Status == STATUS_DEVICE_BUSY)
    {
        g_NfAppRing.Stats.BusyRejects++;
    }
    else if (Status == STATUS_NOT_SUPPORTED)
    {
        // NfVmic_Attach's one source of it: a capture stream open in another format.
        g_NfAppRing.Stats.FormatRejects++;
    }
    else if (Status != STATUS_CANCELLED && Status != STATUS_DEVICE_REMOVED)
    {
        g_NfAppRing.Stats.InvalidRejects++;
    }
    KeReleaseSpinLock(&g_NfAppRing.Lock, oldIrql);
}

//
// The header's copy, checked. Everything the driver keeps comes from the copy: the app can change its memory meanwhile.
//
static
NTSTATUS
NfVmic_CheckHeader(
    _In_ const NF_VMIC_RING_HEADER* Header,
    _In_ ULONG                      BufferBytes,
    _In_ ULONG                      Version
)
{
    UINT32 capacity = Header->CapacityFrames;

    // Magic, then version, then everything else, so that another version's header gets STATUS_REVISION_MISMATCH whatever it
    // changed (the input's version, checked first, already tells a client of another version apart).
    if (Header->Magic != NF_VMIC_MAGIC)
    {
        return STATUS_INVALID_PARAMETER;
    }
    if (Header->Version != Version)
    {
        return STATUS_REVISION_MISMATCH;
    }
    if (Header->HeaderBytes != NF_VMIC_HEADER_BYTES)
    {
        return STATUS_INVALID_PARAMETER;
    }
    if (capacity < NF_VMIC_MIN_CAPACITY_FRAMES || capacity > NF_VMIC_MAX_CAPACITY_FRAMES || (capacity & (capacity - 1)) != 0)
    {
        return STATUS_INVALID_PARAMETER;
    }
    if (Header->SampleRate != NF_VMIC_SAMPLE_RATE ||
        Header->Channels != NF_VMIC_CHANNELS ||
        Header->SampleFormat != NF_VMIC_FORMAT_FLOAT32 ||
        Header->FrameBytes != NF_VMIC_FRAME_BYTES ||
        Header->Flags != 0)
    {
        return STATUS_INVALID_PARAMETER;
    }
    // 64-bit arithmetic, and the capacity is at most 2^18 here, so nothing can overflow.
    if (Header->TotalBytes != NF_VMIC_RING_BYTES(capacity) || NF_VMIC_RING_BYTES(capacity) != (UINT64)BufferBytes)
    {
        return STATUS_INVALID_BUFFER_SIZE;
    }

    return STATUS_SUCCESS;
}

//
// NF_VMIC_IOCTL_ATTACH: check, map, snapshot, publish under the lock, and leave the request pending.
//
static
VOID
NfVmic_Attach(
    _In_ WDFREQUEST Request,
    _In_ size_t     OutputBufferLength,
    _In_ size_t     InputBufferLength
)
{
    PNF_APP_RING            ring = &g_NfAppRing;
    NTSTATUS                status;
    PNF_VMIC_ATTACH_IN      input = NULL;
    PMDL                    mdl = NULL;
    PUCHAR                  base;
    PNF_VMIC_RING_HEADER    header;
    NF_VMIC_RING_HEADER     copy;
    ULONG                   pid;
    WDFFILEOBJECT           file;
    KIRQL                   oldIrql;

    //
    // User mode only. The lifetime rules assume it (the I/O manager cancels a request when the thread or process that issued
    // it exits), and so does the probe that locked the pages in the caller's mode. low-latency-audio refuses other
    // requestors the same way (Device.cpp at dc76402, L2190).
    //
    if (WdfRequestGetRequestorMode(Request) != UserMode)
    {
        status = STATUS_INVALID_DEVICE_REQUEST;
        goto reject;
    }

    //
    // The input's magic and version before any size (nf_vmic.h): every version starts its input with those two fields, so a
    // client of another version gets STATUS_REVISION_MISMATCH whatever else it changed. Only the 8 bytes are read until the
    // length is known to be this version's.
    //
    if (InputBufferLength < RTL_SIZEOF_THROUGH_FIELD(NF_VMIC_ATTACH_IN, Version))
    {
        status = STATUS_INVALID_PARAMETER;
        goto reject;
    }
    status = WdfRequestRetrieveInputBuffer(Request, RTL_SIZEOF_THROUGH_FIELD(NF_VMIC_ATTACH_IN, Version), (PVOID*)&input, NULL);
    if (!NT_SUCCESS(status))
    {
        goto reject;
    }
    if (input->Magic != NF_VMIC_MAGIC)
    {
        status = STATUS_INVALID_PARAMETER;
        goto reject;
    }
    if (input->Version != NF_VMIC_VERSION && input->Version != NF_VMIC_LIVE_VERSION)
    {
        status = STATUS_REVISION_MISMATCH;
        goto reject;
    }
    // This version's input struct, exactly: the length first, then the fields past the first 8 bytes.
    if (InputBufferLength != sizeof(NF_VMIC_ATTACH_IN) || input->InBytes != sizeof(NF_VMIC_ATTACH_IN) ||
        input->Flags != (input->Version == NF_VMIC_LIVE_VERSION ? NF_VMIC_ATTACH_LIVE : 0u))
    {
        status = STATUS_INVALID_PARAMETER;
        goto reject;
    }

    //
    // The ring's length within the protocol's limits before anything maps or reads it. Before the header's version, which
    // can't be read without it; the input's version, checked above, is what tells another version apart.
    //
    if (OutputBufferLength < NF_VMIC_MIN_RING_BYTES || OutputBufferLength > NF_VMIC_MAX_RING_BYTES)
    {
        status = STATUS_INVALID_BUFFER_SIZE;
        goto reject;
    }

    //
    // METHOD_OUT_DIRECT: the output buffer is the IRP's MDL, its pages probed for write access and locked by the I/O manager
    // for the request's life (Learn, kernel/buffer-descriptions-for-i-o-control-codes). One MDL, exactly the buffer, from the
    // start of a page (the app VirtualAllocs it): the system mapping keeps the MDL's byte offset, so the header's fields are
    // naturally aligned there too.
    //
    status = WdfRequestRetrieveOutputWdmMdl(Request, &mdl);
    if (!NT_SUCCESS(status))
    {
        goto reject;
    }
    if (mdl->Next != NULL || MmGetMdlByteCount(mdl) != (ULONG)OutputBufferLength)
    {
        status = STATUS_INVALID_PARAMETER;
        goto reject;
    }
    if (MmGetMdlByteOffset(mdl) != 0)
    {
        status = STATUS_DATATYPE_MISALIGNMENT;
        goto reject;
    }

    //
    // Mapped into system space, no-execute (as low-latency-audio's AsioBufferObject.cpp at dc76402, L143). The mapping is
    // released when the request completes and the I/O manager unlocks the pages (Learn, MmGetSystemAddressForMdlSafe,
    // remarks): nothing here unmaps it, and nothing may touch it after the request completes.
    //
    base = (PUCHAR)MmGetSystemAddressForMdlSafe(mdl, NormalPagePriority | MdlMappingNoExecute);
    if (base == NULL)
    {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto reject;
    }
    header = (PNF_VMIC_RING_HEADER)base;

    // One copy of the header, checked; nothing from line 0 is read from the ring again.
    RtlCopyMemory(&copy, base, sizeof(copy));
    status = NfVmic_CheckHeader(&copy, (ULONG)OutputBufferLength, input->Version);
    if (!NT_SUCCESS(status))
    {
        goto reject;
    }

    pid = WdfRequestGetRequestorProcessId(Request);
    file = WdfRequestGetFileObject(Request);

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    if (ring->Removing)
    {
        status = STATUS_DEVICE_REMOVED;
        goto rejectLocked;
    }
    if (ring->Request != NULL)
    {
        status = STATUS_DEVICE_BUSY;
        goto rejectLocked;
    }

    //
    // A capture stream open in a format other than the ring's would hold the reader slot and get only zeros, so the conflict
    // shows here instead (nf_vmic.h). CaptureStreamOpen (StreamEngine.cpp) counts such a stream under this lock and refuses
    // one while a ring is published, which happens under this lock too: the check and the publish below are one step.
    //
    if (ring->OtherFormatCaptureOpens != 0)
    {
        status = STATUS_NOT_SUPPORTED;
        goto rejectLocked;
    }

    //
    // The handle's cleanup may have run already. IRP_MJ_CLEANUP comes when the last handle to the file object closes, even
    // while this request (which holds a reference to the file object, not a handle) is still on its way here, and KMDF then
    // calls NfVmic_EvtFileCleanup, which found nothing published; the framework's flush of the file's requests after it covers
    // only those still in its queues, never one the driver owns (WDF source: FxPkgGeneral::OnCleanup, FxIoQueue::
    // FlushByFileObject). Published now, the ring would stay attached with no handle left to cancel it through and no cleanup
    // to come: until the issuing thread exited (a handle on a completion port issues requests that belong to no thread, so
    // perhaps not even then) or the device went away, holding the app's pages locked and every other ATTACH at
    // STATUS_DEVICE_BUSY. NfVmic_DetachFile sets CleanedUp under this lock, so either it finds the request published and ends
    // it, or this finds the flag.
    //
    if (file == NULL || NfVmicGetFileContext(file)->CleanedUp)
    {
        status = STATUS_CANCELLED;
        goto rejectLocked;
    }

    // Cancelable from here; WdfRequestMarkCancelableEx never calls the cancel routine itself, and the routine waits on the lock.
    status = WdfRequestMarkCancelableEx(Request, NfVmic_EvtRequestCancel);
    if (!NT_SUCCESS(status))
    {
        // STATUS_CANCELLED: cancelled already. Not published; ours to complete.
        goto rejectLocked;
    }

    ring->Request = Request;
    ring->File = file;
    ring->Header = header;
    ring->Data = base + NF_VMIC_HEADER_BYTES;
    ring->CapacityFrames = copy.CapacityFrames;
    ring->AttachPid = pid;
    ring->LiveFeed = (input->Flags & NF_VMIC_ATTACH_LIVE) != 0;
    ring->LiveStreams = 0;
    ring->ClockSequence = 0;
    ring->ReadFrame = 0;
    ring->LastWriteSeen = 0;
    ring->Reader = NULL;
    ring->ReaderState = 0;
    ring->ReaderPacketFrames = 0;
    ring->AttachResyncs = 0;
    ring->AttachMismatchPackets = 0;
    ring->AttachPasses = 0;
    ring->AttachFramesCopied = 0;
    ring->AttachUnderrunFrames = 0;
    ring->HistoryValid = FALSE;
    ring->HistoryLeadFrames = 0;
    ring->HistoryFrom = 0;
    ring->HistoryEnd = 0;
    ring->HistoryGapNext = 0;
    RtlZeroMemory(ring->HistoryGaps, sizeof(ring->HistoryGaps));
    // The skip log's records of the copy start again too (the last detach settled its claims).
    NfVmicAppRingSettleClaims(ring, TRUE);
    NfVmicAppRingMapReset(ring);
    ring->Stats.Attaches++;

    // The driver's line, zeroed, then ATTACHED: the app's sign, with ERROR_IO_PENDING, that the ring is live.
    RtlZeroMemory(base + NF_VMIC_DRIVER_LINE_OFFSET, NF_VMIC_LINE_BYTES);
    NfVmicStore32(&header->State, NF_VMIC_STATE_ATTACHED);

    KeReleaseSpinLock(&ring->Lock, oldIrql);

    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "AudioCodec: app ring attached (process %lu, %u frames)\n",
               pid, copy.CapacityFrames);

    // Pending until a detach completes it.
    return;

rejectLocked:
    KeReleaseSpinLock(&ring->Lock, oldIrql);

reject:
    NfVmic_CountReject(status);
    WdfRequestComplete(Request, status);
}

//
// NF_VMIC_IOCTL_STATS: a snapshot of the counters. The output buffer is the system buffer (METHOD_BUFFERED), zeroed first so
// nothing of the kernel's leaks into it.
//
static
VOID
NfVmic_Stats(
    _In_ WDFREQUEST Request
)
{
    PNF_APP_RING    ring = &g_NfAppRing;
    PNF_VMIC_STATS  out = NULL;
    NTSTATUS        status;
    LARGE_INTEGER   frequency;
    LARGE_INTEGER   now;
    KIRQL           oldIrql;

    status = WdfRequestRetrieveOutputBuffer(Request, sizeof(NF_VMIC_STATS), (PVOID*)&out, NULL);
    if (!NT_SUCCESS(status))
    {
        WdfRequestComplete(Request, status);
        return;
    }

    RtlZeroMemory(out, sizeof(*out));
    (void)KeQueryPerformanceCounter(&frequency);
    out->Magic = NF_VMIC_MAGIC;
    out->Version = NF_VMIC_VERSION;
    out->StatsBytes = sizeof(NF_VMIC_STATS);
    out->QpcFrequency = (UINT64)frequency.QuadPart;
    out->SkipLogEntries = NF_VMIC_SKIP_LOG_ENTRIES;
    out->UnderrunLogEntries = NF_VMIC_UNDERRUN_LOG_ENTRIES;
    out->EventBytes = sizeof(NF_VMIC_EVENT);

    // The timer statistics and the capture-timestamp trace are interlocked: no lock.
    NfVmicCopyStreamStats(out->Stream);
    NfVmicCopyCaptureTrace(&out->CaptureTrace);

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    out->Ring = ring->Stats;
    out->SecondaryFramesCopied = ring->SecondaryFramesCopied;
    out->SecondaryUnderrunFrames = ring->SecondaryUnderrunFrames;
    out->ReaderHandoffs = ring->ReaderHandoffs;
    out->HandoffSkippedFrames = ring->HandoffSkippedFrames;
    out->SecondarySkips = ring->SecondarySkips;
    out->SecondarySkippedFrames = ring->SecondarySkippedFrames;
    if (ring->Header != NULL)
    {
        out->Flags |= NF_VMIC_STATS_ATTACHED;
        out->AttachedPid = ring->AttachPid;
        out->AttachedCapacityFrames = ring->CapacityFrames;
        out->AttachedReadFrame = ring->ReadFrame;
        out->AttachedWriteFrame = ring->LastWriteSeen;
        if (ring->Reader != NULL)
        {
            // What the stream holding the reader slot last wrote to the header (AppRingStoreCounters), so the two agree: READING
            // only if its latest pass read the ring, FORMAT_MISMATCH if it holds the slot in another format.
            if ((ring->ReaderState & NF_VMIC_STATE_READING) != 0)
            {
                out->Flags |= NF_VMIC_STATS_READING;
            }
            if ((ring->ReaderState & NF_VMIC_STATE_FORMAT_MISMATCH) != 0)
            {
                out->Flags |= NF_VMIC_STATS_FORMAT_MISMATCH;
            }
            out->ReaderPacketFrames = ring->ReaderPacketFrames;
        }
    }

    if (ring->Loopback != NULL)
    {
        PLOOPBACK_RING loopback = ring->Loopback;

        // Lock order: the app ring's, then the loopback's (AppRing.h).
        KeAcquireSpinLockAtDpcLevel(&loopback->Lock);
        out->Loopback.IdlePackets = loopback->IdlePackets;
        out->Loopback.MismatchPackets = loopback->MismatchPackets;
        out->Loopback.UnderrunPackets = loopback->UnderrunPackets;
        out->Loopback.UnderrunFrames = loopback->UnderrunFrames;
        out->Loopback.OverrunPackets = loopback->OverrunPackets;
        out->Loopback.CaptureAnchors = loopback->CaptureAnchors;
        out->Loopback.RenderRestarts = loopback->RenderRestarts;
        out->Loopback.RenderGapFills = loopback->RenderGapFills;
        out->Loopback.RenderLatePackets = loopback->RenderLatePackets;
        out->Loopback.RenderLostPackets = loopback->RenderLostPackets;
        out->Loopback.RenderNotOwner = loopback->RenderNotOwner;
        out->Loopback.RefusedOpens = loopback->RefusedOpens;
        out->LoopbackLeadSkips = loopback->LeadSkips;
        out->LoopbackLeadSkippedFrames = loopback->LeadSkippedFrames;
        //
        // The device's event logs, inside both locks: every writer of them holds one or the other (the app ring's events the
        // app ring's, the loopback's its own), so the logs are copied at the instant the counters are, and agree with them.
        // Their own locks come last (AppRing.h's order).
        //
        NfVmicCopyEventLog(&loopback->SkipLog, out->SkipLog, NF_VMIC_SKIP_LOG_ENTRIES, &out->SkipEventsLogged);
        NfVmicCopyEventLog(&loopback->UnderrunLog, out->UnderrunLog, NF_VMIC_UNDERRUN_LOG_ENTRIES, &out->UnderrunEventsLogged);
        KeReleaseSpinLockFromDpcLevel(&loopback->Lock);
        out->Flags |= NF_VMIC_STATS_LOOPBACK;
    }

    // The snapshot's time, read last: every counter and event-log entry it holds is from before it (an entry's Qpc is read as it
    // is logged), so nf-vmic-test's ages of the entries are never negative.
    now = KeQueryPerformanceCounter(NULL);
    out->QpcNow = (UINT64)now.QuadPart;

    KeReleaseSpinLock(&ring->Lock, oldIrql);

    WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, sizeof(NF_VMIC_STATS));
}

_Use_decl_annotations_
static
VOID
NfVmic_EvtIoDeviceControl(
    WDFQUEUE    Queue,
    WDFREQUEST  Request,
    size_t      OutputBufferLength,
    size_t      InputBufferLength,
    ULONG       IoControlCode
)
{
    UNREFERENCED_PARAMETER(Queue);

    switch (IoControlCode)
    {
    case NF_VMIC_IOCTL_ATTACH:
        NfVmic_Attach(Request, OutputBufferLength, InputBufferLength);
        break;

    case NF_VMIC_IOCTL_STATS:
        NfVmic_Stats(Request);
        break;

    default:
        WdfRequestComplete(Request, STATUS_INVALID_DEVICE_REQUEST);
        break;
    }
}

//
// Creates the control device. s_CdoLock held, PASSIVE_LEVEL. The steps are Learn's, "Creating a Control Device Object".
//
static
NTSTATUS
NfVmic_CreateControlDevice(
    _In_ WDFDRIVER Driver
)
{
    NTSTATUS                status;
    PWDFDEVICE_INIT         init;
    WDFDEVICE               device = NULL;
    WDFQUEUE                queue = NULL;
    WDF_FILEOBJECT_CONFIG   fileConfig;
    WDF_OBJECT_ATTRIBUTES   fileAttributes;
    WDF_IO_QUEUE_CONFIG     queueConfig;
    KIRQL                   oldIrql;
    BOOLEAN                 symlink = FALSE;

    init = WdfControlDeviceInitAllocate(Driver, &g_NfVmicSddl);
    if (init == NULL)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    // The security descriptor covers opens of the device's namespace too (\\.\NoiseFilterVmic\anything).
    WdfDeviceInitSetCharacteristics(init, FILE_DEVICE_SECURE_OPEN, FALSE);
    // More than one handle: a STATS reader alongside the app that attached.
    WdfDeviceInitSetExclusive(init, FALSE);

    status = WdfDeviceInitAssignName(init, &g_NfVmicDeviceName);
    if (!NT_SUCCESS(status))
    {
        goto error;
    }

    // EvtFileCleanup: a handle closed with its ATTACH still pending ends the attachment. Each file object gets an
    // NF_VMIC_FILE_CONTEXT, zeroed, whose CleanedUp keeps an ATTACH still on its way from publishing after the cleanup.
    WDF_FILEOBJECT_CONFIG_INIT(&fileConfig, WDF_NO_EVENT_CALLBACK, WDF_NO_EVENT_CALLBACK, NfVmic_EvtFileCleanup);
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttributes, NF_VMIC_FILE_CONTEXT);
    WdfDeviceInitSetFileObjectConfig(init, &fileConfig, &fileAttributes);

    // On success WdfDeviceCreate takes the init (and sets it to NULL).
    status = WdfDeviceCreate(&init, WDF_NO_OBJECT_ATTRIBUTES, &device);
    if (!NT_SUCCESS(status))
    {
        goto error;
    }

    //
    // The symbolic link by hand, not WdfDeviceCreateSymbolicLink. The framework deletes a link it made only when it destroys
    // the device, and it destroys a deleted control device only once the last handle to it closes (WDF source:
    // FxDevice::DeleteObject defers the destruction, and FxDevice::Destroy is the one caller of DeleteSymbolicLink). A handle
    // still open on the old device, a STATS reader's say, would then hold the name across a disable and enable or a PnP
    // restart: this link would fail with STATUS_OBJECT_NAME_COLLISION, and nothing would make the control device again until
    // the PnP device was next added. So NfVmic_DeleteControlDevice deletes the link itself, with the device. The device's own
    // name goes at once too: WdfObjectDelete calls IoDeleteDevice straight away (FxDevice::DeleteObject), which takes a named
    // device object out of the namespace (an inference, for the live test to confirm). While s_ControlDevice is set, the link
    // exists.
    //
    status = IoCreateSymbolicLink((PUNICODE_STRING)&g_NfVmicSymlinkName, (PUNICODE_STRING)&g_NfVmicDeviceName);
    if (!NT_SUCCESS(status))
    {
        goto error;
    }
    symlink = TRUE;

    //
    // Parallel dispatch: the ATTACH stays pending for as long as the ring is attached, and a sequential queue would hold every
    // later request (STATS included) behind it. Not power-managed: a control device's queues can't be (Learn, the same page).
    //
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoDeviceControl = NfVmic_EvtIoDeviceControl;
    queueConfig.PowerManaged = WdfFalse;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &queue);
    if (!NT_SUCCESS(status))
    {
        goto error;
    }

    //
    // Removing (set when the last control device went) cleared before WdfControlFinishInitializing opens this one to I/O:
    // cleared after it, an ATTACH that came in between would be refused with STATUS_DEVICE_REMOVED by a live device. Nothing of
    // the old device can take advantage: its queue was purged under s_CdoLock, which the caller holds, and stays purged, so a
    // handle still open on it reaches no callback but its cleanup.
    //
    KeAcquireSpinLock(&g_NfAppRing.Lock, &oldIrql);
    g_NfAppRing.Removing = FALSE;
    KeReleaseSpinLock(&g_NfAppRing.Lock, oldIrql);

    // I/O is refused until this.
    WdfControlFinishInitializing(device);

    s_ControlDevice = device;
    s_ControlQueue = queue;
    return STATUS_SUCCESS;

error:
    if (init != NULL)
    {
        WdfDeviceInitFree(init);
    }
    if (symlink)
    {
        (void)IoDeleteSymbolicLink((PUNICODE_STRING)&g_NfVmicSymlinkName);
    }
    if (device != NULL)
    {
        WdfObjectDelete(device);
    }
    return status;
}

//
// Deletes the control device. s_CdoLock held, PASSIVE_LEVEL, from the last PnP device's context cleanup.
//
static
VOID
NfVmic_DeleteControlDevice()
{
    KIRQL oldIrql;

    KeAcquireSpinLock(&g_NfAppRing.Lock, &oldIrql);
    g_NfAppRing.Removing = TRUE;
    KeReleaseSpinLock(&g_NfAppRing.Lock, oldIrql);

    // The link NfVmic_CreateControlDevice made by hand (it says why): no new handle finds the device from here, and the name is
    // free for the next control device even while a handle to this one stays open.
    (void)IoDeleteSymbolicLink((PUNICODE_STRING)&g_NfVmicSymlinkName);

    //
    // Cancels the pending ATTACH, if any: the purge calls the cancel routine of every request the driver owns and marked
    // cancelable (WDF source, FxIoQueue::QueuePurge), and NfVmic_EvtRequestCancel unpublishes the ring, counts a removal and
    // completes the request with STATUS_DEVICE_REMOVED. Returns once every request the queue delivered, STATS included, has
    // completed; the queue stays purged, so later ones on a handle still open fail with STATUS_INVALID_DEVICE_STATE (Learn,
    // WdfIoQueuePurgeSynchronously; FxIoQueue::QueueRequest). An ATTACH between the flag and the purge is refused by the flag,
    // so none can be published behind the purge.
    //
    WdfIoQueuePurgeSynchronously(s_ControlQueue);

    // IoDeleteDevice at once, the rest once the last handle closes (NfVmic_CreateControlDevice).
    WdfObjectDelete(s_ControlDevice);
    s_ControlDevice = NULL;
    s_ControlQueue = NULL;

    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "AudioCodec: app-feed control device deleted\n");
}

_Use_decl_annotations_
NTSTATUS
NfVmic_DeviceAdd(
    WDFDRIVER       Driver,
    WDFDEVICE       Device,
    PLOOPBACK_RING  Loopback
)
{
    NTSTATUS    status;
    KIRQL       oldIrql;

    (void)WdfWaitLockAcquire(s_CdoLock, NULL);

    // The primary cable owns the one named app feed. A duplicate manually-created primary must never receive its audio.
    if (s_DeviceCount != 0)
    {
        WdfWaitLockRelease(s_CdoLock);
        return STATUS_DEVICE_BUSY;
    }
    s_DeviceCount++;

    KeAcquireSpinLock(&g_NfAppRing.Lock, &oldIrql);
    if (g_NfAppRing.Loopback == NULL)
    {
        g_NfAppRing.Loopback = Loopback;
        g_NfAppRing.LoopbackOwner = Device;
    }
    KeReleaseSpinLock(&g_NfAppRing.Lock, oldIrql);

    if (s_ControlDevice == NULL)
    {
        //
        // A failure costs the app feed only: reported, and the device goes on (as the toaster's sideband filter does). Nothing
        // retries before the next PnP device is added. A name collision means something still holds the device's name or its
        // link: NfVmic_DeleteControlDevice frees both, so it would be another driver's, or a deleted control device's name if
        // IoDeleteDevice didn't free it (NfVmic_CreateControlDevice).
        //
        status = NfVmic_CreateControlDevice(Driver);
        if (status == STATUS_OBJECT_NAME_COLLISION)
        {
            DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL,
                       "AudioCodec: no app-feed control device (0x%08X): \\Device\\NoiseFilterVmic or its link is still taken\n",
                       status);
        }
        else if (!NT_SUCCESS(status))
        {
            DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: no app-feed control device (0x%08X)\n", status);
        }
    }

    WdfWaitLockRelease(s_CdoLock);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
VOID
NfVmic_DeviceCleanup(
    WDFDEVICE Device
)
{
    KIRQL oldIrql;

    (void)WdfWaitLockAcquire(s_CdoLock, NULL);

    // STATS stops reading this device's loopback counters before its context goes.
    KeAcquireSpinLock(&g_NfAppRing.Lock, &oldIrql);
    if (g_NfAppRing.LoopbackOwner == Device)
    {
        g_NfAppRing.Loopback = NULL;
        g_NfAppRing.LoopbackOwner = NULL;
    }
    KeReleaseSpinLock(&g_NfAppRing.Lock, oldIrql);

    if (s_DeviceCount != 0)
    {
        s_DeviceCount--;
    }
    if (s_DeviceCount == 0 && s_ControlDevice != NULL)
    {
        NfVmic_DeleteControlDevice();
    }

    WdfWaitLockRelease(s_CdoLock);
}
