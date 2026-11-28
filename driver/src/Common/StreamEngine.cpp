/*++

    THIS CODE AND INFORMATION IS PROVIDED "AS IS" WITHOUT WARRANTY OF ANY
    KIND, EITHER EXPRESSED OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
    IMPLIED WARRANTIES OF MERCHANTABILITY AND/OR FITNESS FOR A PARTICULAR
    PURPOSE.

Module Name:

    StreamEngine.cpp

Abstract:

    Virtual Streaming Engine - this module controls streaming logic for
    the device.

Environment:

    Kernel mode

--*/

#include "private.h"
#include "public.h"
#include <devguid.h>
#include <ks.h>
#include <mmsystem.h>
#include <ksmedia.h>
// noise-filter (TODO 19, spike check 4): KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, which ksmedia.h defines only after mmreg.h (and
// private.h includes it first). NOBITMAP as Inc\AudioFormats.h has it: no BITMAPINFOHEADER in kernel mode.
#define NOBITMAP
#include <mmreg.h>
#include "streamengine.h"
#include "AppRing.h"

#ifndef __INTELLISENSE__
#include "streamengine.tmh"
#endif

// Only an explicitly enabled verbose ETW session samples the first four render packets. Keep sample values out of traces.
#define NF_LOOPBACK_TRACE_ENABLED() (WPP_CHECK_INIT WPP_LEVEL_FLAGS_ENABLED(LEVEL_VERBOSE, FLAG_STREAM))

_Use_decl_annotations_
PAGED_CODE_SEG
CStreamEngine::CStreamEngine(
    _In_ ACXSTREAM Stream,
    _In_ ACXDATAFORMAT StreamFormat,
    _In_ BOOL Offload,
    _In_opt_ CSimPeakMeter *CircuitPeakmeter
)
    : m_PacketsCount(0),
    m_PacketSize(0),
    m_FirstPacketOffset(0),
    m_NotificationTimer(NULL),
    m_CurrentState(AcxStreamStateStop),
    m_CurrentPacket(0),
    m_Position(0),
    m_Stream(Stream),
    m_StreamFormat(StreamFormat),
    m_StartTime(0),
    m_StartPosition(0),
    m_GlitchAdjust(0),
    m_ToneFrequency(DEFAULT_FREQUENCY),
    m_Offload(Offload),
    m_pCircuitPeakmeter(CircuitPeakmeter)
{
    PAGED_CODE();

    KeQueryPerformanceCounter(&m_PerformanceCounterFrequency);
    RtlZeroMemory(m_Packets, sizeof(m_Packets));

    //
    // noise-filter (TODO 19): find the device's loopback ring and its cable. The stream's circuit belongs to one of our
    // WDFDEVICEs, a cable (AcxStreamGetCircuit, acxstreams.h:839, up to DISPATCH_LEVEL; then LoopbackRingOfCircuit and
    // CableOfCircuit). The pointers are kept: the device context outlives the stream (inference: the stream descends from the
    // device).
    //
    RtlZeroMemory(&m_LbFormat, sizeof(m_LbFormat));
    m_LbPacketFrames = 0;
    m_LbRingFrames = 0;
    m_LbAnchor = 0;
    m_LbAnchorValid = FALSE;
    m_LbOpen = FALSE;
    m_LbOpenRing = NULL;
    m_LbOpenRender = FALSE;
    RtlZeroMemory(&m_LbOpenFormat, sizeof(m_LbOpenFormat));

    // noise-filter (TODO 19, spike check 5): the timer statistics. CCaptureStreamEngine's constructor makes it a capture stream.
    m_StatsKind = NF_VMIC_KIND_RENDER;
    m_ArmedHns = 0;

    // noise-filter (TODO 19): its serial in the event logs, from 1, in creation order across every device.
    m_Serial = (ULONG)InterlockedIncrement(&g_NfStreamSerial);

    {
        ACXCIRCUIT circuit = AcxStreamGetCircuit(Stream);

        m_Loopback = (circuit != NULL) ? LoopbackRingOfCircuit(circuit) : NULL;
        m_Cable = (circuit != NULL) ? CableOfCircuit(circuit) : NULL;
    }

    // One context holds both (the per-cable design, section 5): the cable's app ring logs into this loopback ring's logs.
    NT_ASSERT(m_Cable == NULL || m_Cable->Ring.Loopback == m_Loopback);
}

_Use_decl_annotations_
PAGED_CODE_SEG
CStreamEngine::~CStreamEngine()
{
    PAGED_CODE();

    // noise-filter (TODO 19): this stream is no longer open on its circuit (the record LoopbackStreamOpen or, for a capture
    // stream, CaptureStreamOpen made: on its cable, whose loopback ring m_LbOpenRing is).
    if (m_LbOpen)
    {
        if (m_LbOpenRender)
        {
            LoopbackStreamClose(m_LbOpenRing, TRUE, &m_LbOpenFormat);
        }
        else
        {
            NT_ASSERT(m_Cable != NULL && m_Cable->Ring.Loopback == m_LbOpenRing);
            CaptureStreamClose(m_Cable, &m_LbOpenFormat);
        }
        m_LbOpen = FALSE;
        m_LbOpenRing = NULL;
    }
}

_Use_decl_annotations_
PAGED_CODE_SEG
VOID
CStreamEngine::LoopbackAdoptOpen(
    PLOOPBACK_RING          Ring,
    BOOLEAN                 Render,
    const LOOPBACK_FORMAT * Format
)
{
    PAGED_CODE();

    m_LbOpen = TRUE;
    m_LbOpenRing = Ring;
    m_LbOpenRender = Render;
    m_LbOpenFormat = *Format;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::AllocateRtPackets(
    _In_    ULONG           PacketCount,
    _In_    ULONG           PacketSize,
    _Out_   PACX_RTPACKET* Packets
)
{
    NTSTATUS status = STATUS_SUCCESS;
    PACX_RTPACKET packets = NULL;
    PVOID packetBuffer = NULL;
    ULONG i;
    ULONG packetAllocSizeInPages = 0;
    ULONG packetAllocSizeInBytes = 0;
    ULONG firstPacketOffset = 0;
    size_t packetsSize = 0;

    PAGED_CODE();

    if (PacketCount > MAX_PACKET_COUNT)
    {
        ASSERT(FALSE);
        status = STATUS_INVALID_PARAMETER;
        goto exit;
    }

    status = RtlSizeTMult(PacketCount, sizeof(ACX_RTPACKET), &packetsSize);
    if (!NT_SUCCESS(status))
    {
        ASSERT(FALSE);
        goto exit;
    }

    packets = (PACX_RTPACKET)ExAllocatePool2(POOL_FLAG_NON_PAGED, packetsSize, DeviceDriverTag);
    if (!packets)
    {
        status = STATUS_NO_MEMORY;
        ASSERT(FALSE);
        goto exit;
    }

    //
    // We need to allocate page-aligned buffers, to ensure no kernel memory leaks
    // to user space. Round up the packet size to page aligned, then calculate
    // the first packet's buffer offset so packet 0 ends on a page boundary and
    // packet 1 begins on a page boundary.
    //
    status = RtlULongAdd(PacketSize, PAGE_SIZE - 1, &packetAllocSizeInPages);
    if (!NT_SUCCESS(status))
    {
        ASSERT(FALSE);
        goto exit;
    }
    packetAllocSizeInPages = packetAllocSizeInPages / PAGE_SIZE;
    packetAllocSizeInBytes = PAGE_SIZE * packetAllocSizeInPages;
    firstPacketOffset = packetAllocSizeInBytes - PacketSize;

    for (i = 0; i < PacketCount; ++i)
    {
        PMDL pMdl = NULL;

        ACX_RTPACKET_INIT(&packets[i]);

        packetBuffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, packetAllocSizeInBytes, DeviceDriverTag);
        if (packetBuffer == NULL)
        {
            status = STATUS_NO_MEMORY;
            goto exit;
        }

        pMdl = IoAllocateMdl(packetBuffer, packetAllocSizeInBytes, FALSE, TRUE, NULL);
        if (pMdl == NULL)
        {
            status = STATUS_NO_MEMORY;
            goto exit;
        }

        MmBuildMdlForNonPagedPool(pMdl);

        WDF_MEMORY_DESCRIPTOR_INIT_MDL(&((packets)[i].RtPacketBuffer), pMdl, packetAllocSizeInBytes);

        packets[i].RtPacketSize = PacketSize;
        if (i == 0)
        {
            packets[i].RtPacketOffset = firstPacketOffset;
        }
        else
        {
            packets[i].RtPacketOffset = 0;
        }
        m_Packets[i] = packetBuffer;

        packetBuffer = NULL;
    }

    *Packets = packets;
    packets = NULL;
    m_PacketsCount = PacketCount;
    m_PacketSize = PacketSize;
    m_FirstPacketOffset = firstPacketOffset;

exit:
    if (packetBuffer)
    {
        ExFreePoolWithTag(packetBuffer, DeviceDriverTag);
    }
    if (packets)
    {
        FreeRtPackets(packets, PacketCount);
    }
    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
VOID
CStreamEngine::FreeRtPackets(
    _Frees_ptr_ PACX_RTPACKET   Packets,
    _In_        ULONG           PacketCount
)
{
    ULONG i;
    PVOID buffer;

    PAGED_CODE();

    for (i = 0; i < PacketCount; ++i)
    {
        if (Packets[i].RtPacketBuffer.u.MdlType.Mdl)
        {
            buffer = MmGetMdlVirtualAddress(Packets[i].RtPacketBuffer.u.MdlType.Mdl);
            IoFreeMdl(Packets[i].RtPacketBuffer.u.MdlType.Mdl);
            ExFreePool(buffer);
        }
    }

    ExFreePool(Packets);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::PrepareHardware()
{
    NTSTATUS                status = STATUS_UNSUCCESSFUL;
    WDF_TIMER_CONFIG        timerConfig;
    WDF_OBJECT_ATTRIBUTES   timerAttributes;
    PSTREAM_TIMER_CONTEXT   timerCtx;

    PAGED_CODE();

    //
    // If already in this state, do nothing.
    //
    if (m_CurrentState == AcxStreamStatePause)
    {
        // Nothing to do.
        status = STATUS_SUCCESS;
        goto exit;
    }

    if (m_CurrentState != AcxStreamStateStop)
    {
        // Error out.
        status = STATUS_INVALID_STATE_TRANSITION;
        goto exit;
    }

    //
    // Stop to Pause.
    //
    WDF_TIMER_CONFIG_INIT(&timerConfig, CStreamEngine::s_EvtStreamPassCallback);
    timerConfig.AutomaticSerialization = TRUE;
    timerConfig.UseHighResolutionTimer = WdfTrue;
    timerConfig.Period = 0;

    WDF_OBJECT_ATTRIBUTES_INIT(&timerAttributes);
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&timerAttributes, STREAM_TIMER_CONTEXT);
    timerAttributes.ParentObject = m_Stream;

    status = WdfTimerCreate(&timerConfig, &timerAttributes, &m_NotificationTimer);
    if (!NT_SUCCESS(status))
    {
        goto exit;
    }

    timerCtx = GetStreamTimerContext(m_NotificationTimer);
    timerCtx->StreamEngine = this;

    m_CurrentState = AcxStreamStatePause;
    status = STATUS_SUCCESS;

exit:
    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::ReleaseHardware()
{
    PAGED_CODE();

    //
    // If already in this state, do nothing.
    //
    if (m_CurrentState == AcxStreamStateStop)
    {
        // Nothing to do.
        goto exit;
    }

    //
    // Just assert we are in the correct state. 
    // On the way down we always want to succeed.
    //
    ASSERT(m_CurrentState == AcxStreamStatePause);

    //
    // Pause to Stop.
    //
    if (m_NotificationTimer)
    {
        WdfTimerStop(m_NotificationTimer, TRUE);
        WdfObjectDelete(m_NotificationTimer);
        m_NotificationTimer = NULL;
    }

    KeFlushQueuedDpcs();

    m_Position = 0;
    m_GlitchAdjust = 0;
    m_CurrentPacket = 0;

    m_CurrentState = AcxStreamStateStop;

exit:
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::Pause()
{
    NTSTATUS status = STATUS_UNSUCCESSFUL;

    PAGED_CODE();

    if (m_CurrentState == AcxStreamStatePause)
    {
        // Nothing to do.
        status = STATUS_SUCCESS;
        goto exit;
    }

    if (m_CurrentState != AcxStreamStateRun)
    {
        // Error out.
        status = STATUS_INVALID_STATE_TRANSITION;
        goto exit;
    }

    m_PeakMeter.StopStream();
    if (m_pCircuitPeakmeter)
    {
        m_pCircuitPeakmeter->StopStream();
    }

    //
    // Run to Pause.
    //
    WdfTimerStop(m_NotificationTimer, TRUE);

    // Save the position we paused at.
    UpdatePosition(KeQueryPerformanceCounter(NULL));

    m_CurrentState = AcxStreamStatePause;
    status = STATUS_SUCCESS;

exit:
    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::Run()
{
    NTSTATUS status = STATUS_UNSUCCESSFUL;

    PAGED_CODE();

    if (m_CurrentState == AcxStreamStateRun)
    {
        // Nothing to do.
        status = STATUS_SUCCESS;
        goto exit;
    }

    if (m_CurrentState != AcxStreamStatePause)
    {
        status = STATUS_INVALID_STATE_TRANSITION;
        goto exit;
    }

    m_PeakMeter.StartStream();
    if (m_pCircuitPeakmeter)
    {
        m_pCircuitPeakmeter->StartStream();
    }

    // noise-filter (TODO 19, spike check 5): counted per stream kind (NF_VMIC_IOCTL_STATS), in the stream's cable.
    if (m_Cable != NULL)
    {
        NfVmicRecordRun(m_Cable, m_StatsKind);
    }

    BeginRunClock();
    m_CurrentState = AcxStreamStateRun;
    ScheduleNextPass();
    status = STATUS_SUCCESS;

exit:
    return status;
}

_Use_decl_annotations_
VOID
CStreamEngine::BeginRunClock()
{
    // A fresh QPC anchor at the saved byte position makes the first deadline relative to this Run, including after Pause.
    m_StartTime = QpcToHns(KeQueryPerformanceCounter(NULL));
    m_StartPosition = m_Position;
    m_GlitchAdjust = 0;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::GetPresentationPosition(
    _Out_   PULONGLONG      PositionInBlocks,
    _Out_   PULONGLONG      QPCPosition
)
{
    ULONG blockAlign;
    LARGE_INTEGER qpc;

    PAGED_CODE();

    blockAlign = AcxDataFormatGetBlockAlign(m_StreamFormat);

    // Update the position based on the current time. noise-filter (TODO 19): the position and the QPC reported with it come
    // from one clock reading; the sample read the clock in UpdatePosition and again for the QPC, so the pair was off by the
    // time between the two reads.
    qpc = KeQueryPerformanceCounter(NULL);
    UpdatePosition(qpc);

    *PositionInBlocks = m_Position / blockAlign;
    *QPCPosition = (ULONGLONG)qpc.QuadPart;

    if (m_StatsKind == NF_VMIC_KIND_CAPTURE && m_Cable != NULL)
    {
        NfVmicTraceCaptureCount(m_Cable, NF_VMIC_TRACE_POSITION);
    }

    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::GetLinearBufferPosition(
    _Out_   PULONGLONG      Position
)
{
    UNREFERENCED_PARAMETER(Position);
    PAGED_CODE();
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::SetCurrentWritePosition(
    _In_   ULONG      Position
)
{
    UNREFERENCED_PARAMETER(Position);
    PAGED_CODE();
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::SetLastBufferPosition(
    _In_   ULONG      Position
)
{
    UNREFERENCED_PARAMETER(Position);
    PAGED_CODE();
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::AssignDrmContentId(
    ULONG          DrmContentId,
    PACXDRMRIGHTS  DrmRights
)
{
    PAGED_CODE();

    UNREFERENCED_PARAMETER(DrmContentId);
    UNREFERENCED_PARAMETER(DrmRights);
    
    //
    // At this point the driver should enforce the new DrmRights.
    //
    // HDMI render: if DigitalOutputDisable or CopyProtect is true, enable HDCP.
    // 
    // From MSDN:
    //
    // This sample doesn't forward protected content, but if your driver uses 
    // lower layer drivers or a different stack to properly work, please see the 
    // following info from MSDN:
    //
    // "Before allowing protected content to flow through a data path, the system
    // verifies that the data path is secure. To do so, the system authenticates
    // each module in the data path beginning at the upstream end of the data path
    // and moving downstream. As each module is authenticated, that module gives
    // the system information about the next module in the data path so that it
    // can also be authenticated. To be successfully authenticated, a module's 
    // binary file must be signed as DRM-compliant.
    //
    // Two adjacent modules in the data path can communicate with each other in 
    // one of several ways. If the upstream module calls the downstream module 
    // through IoCallDriver, the downstream module is part of a WDM driver. In 
    // this case, the upstream module calls the AcxDrmForwardContentToDeviceObject
    // function to provide the system with the device object representing the 
    // downstream module. (If the two modules communicate through the downstream
    // module's content handlers, the upstream module calls AcxDrmAddContentHandlers
    // instead.)
    //
    // For more information, see MSDN's DRM Functions and Interfaces.
    //

    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::GetHWLatency(
    _Out_   ULONG* FifoSize,
    _Out_   ULONG* Delay
)
{
    PAGED_CODE();

    *FifoSize = 128;
    *Delay = 0;

    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
CSimPeakMeter *
CStreamEngine::GetPeakMeter()
{
    PAGED_CODE();

    return &m_PeakMeter;
}

_Use_decl_annotations_
VOID
CStreamEngine::s_EvtStreamPassCallback(
    _In_    WDFTIMER        Timer
)
{
    CStreamEngine* This;
    PSTREAM_TIMER_CONTEXT timerCtx;

    // Get our stream engine pointer from the timer context
    timerCtx = GetStreamTimerContext(Timer);
    This = timerCtx->StreamEngine;

    //
    // noise-filter (TODO 19, spike check 5): how late the timer fired: this pass's time (the schedule's units, one QPC
    // reading through QpcToHns) minus the time ScheduleNextPass armed it for. Timer-fired passes only: the ones
    // ScheduleNextPass runs at once count as glitch adjustments instead.
    //
    // The same reading starts the pass's working time (NF_VMIC_STREAM_STATS.WorkQpcTicks), taken again once StreamPassCallback
    // returns: the CPU cost of a pass, for the /Os and /Ot comparison and the soak.
    //
    // Both counted in the stream's cable (NF_CABLE.StreamStats).
    if (This->m_Cable == NULL)
    {
        This->StreamPassCallback();
        return;
    }
    PNF_CABLE       cable = This->m_Cable;
    ULONG           kind = This->m_StatsKind;
    LARGE_INTEGER   passStart = KeQueryPerformanceCounter(NULL);
    {
        ULONGLONG now = This->QpcToHns(passStart);

        NfVmicRecordTimerPass(cable, kind, (LONGLONG)(now - This->m_ArmedHns));
    }

    // Call the StreamPassCallback for the engine
    This->StreamPassCallback();

    NfVmicRecordPassWork(cable, kind, (ULONGLONG)(KeQueryPerformanceCounter(NULL).QuadPart - passStart.QuadPart));
}

// This is run every time the stream timer fires
_Use_decl_annotations_
VOID
CStreamEngine::StreamPassCallback()
{
    ULONGLONG completedPacket;
    ULONGLONG qpcNotify;

    // Process the packet (e.g. save render to file/generate capture data)
    ProcessPacket();

    // We've completed a packet! Increment our currently active packet
    completedPacket = (ULONGLONG)InterlockedIncrement64((LONG64*)&m_CurrentPacket) - 1;
    // Save the time at which we moved to the next packet
    qpcNotify = (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart;

    //
    // noise-filter (TODO 19, the capture-timestamp trace, 2026-11-23): ACX gives a capture packet the QPC passed here as its
    // first frame's time: GetBuffer's device QPC position, shared and exclusive, every mode and period tried, equal to the
    // tick (NF_VMIC_CAPTURE_TRACE, nf-vmic-test stamps). It never called EvtAcxStreamGetCapturePacket, whose QPCPacketStart
    // the sample meant for that. So a capture stream passes its packet's start on the time line ScheduleNextPass keeps
    // (smooth, its glitch adjustments included), not now: with now, every client saw its frames a packet late, after the
    // time it received them. A render stream passes now, as the sample did.
    //
    if (m_StatsKind == NF_VMIC_KIND_CAPTURE)
    {
        qpcNotify = PacketStartQpc(completedPacket);
        InterlockedExchange64(&m_LastPacketStart.QuadPart, (LONG64)qpcNotify);
        if (m_Cable != NULL)
        {
            NfVmicTraceCaptureNotify(m_Cable, completedPacket, qpcNotify);
        }
    }

    // Tell ACX we've completed the packet.
    (void)AcxRtStreamNotifyPacketComplete(m_Stream, completedPacket, qpcNotify);

    // Schedule when our new current packet will finish
    ScheduleNextPass();
}

//
// noise-filter (TODO 19, Astra's review 2026-11-23): Value x Mul / Div without the 64-bit product. The schedule's bytes
// times 10^7 (and the position's 100 ns units times its bytes a second) overflowed after 55.6 days of one stream at 48 kHz
// stereo float32 (384,000 bytes a second): 2^64 / 10^7 / 384,000 s. Split into whole Divs and the rest, as LoopbackHnsToFrames
// does: exact (the floor of the full product's quotient), and the rest times Mul stays far below 2^64 for these units.
//
static
FORCEINLINE
ULONGLONG
NfScaleU64(
    _In_ ULONGLONG  Value,
    _In_ ULONGLONG  Mul,
    _In_ ULONGLONG  Div
)
{
    return (Value / Div) * Mul + ((Value % Div) * Mul) / Div;
}

_Use_decl_annotations_
VOID
CStreamEngine::ScheduleNextPass()
{
    LONGLONG delay = 0;
    ULONG bytesPerSecond;
    ULONGLONG nextPacket = 0;
    ULONGLONG nextPacketStartPosition = 0;
    ULONGLONG nextPacketPositionFromLastPause = 0;
    ULONGLONG nextPacketTimeFromLastPauseHns = 0;
    ULONGLONG nextPacketTime = 0;
    ULONGLONG currentTime;
    LARGE_INTEGER qpc;
    BOOLEAN inTimerQueue = FALSE;

    // Get the number of bytes per second from our stored stream format
    bytesPerSecond = GetBytesPerSecond();

    // Calculate the absolute position of the beginning of the next packet from the beginning of the stream
    nextPacket = m_CurrentPacket + 1;
    nextPacketStartPosition = nextPacket * m_PacketSize;

    // Adjust next packet position to account for the last time we resumed from Pause
    nextPacketPositionFromLastPause = nextPacketStartPosition - m_StartPosition;

    // Convert from bytes to HNS (to prevent truncation, multiply first then divide)
    nextPacketTimeFromLastPauseHns = NfScaleU64(nextPacketPositionFromLastPause, HNS_PER_SEC, bytesPerSecond);

    // Next packet time is Time @ resume from Pause, offset for lost time due to glitch, with next packet time added
    nextPacketTime = m_StartTime + m_GlitchAdjust + nextPacketTimeFromLastPauseHns;

    // noise-filter (TODO 19): one clock reading, converted (QpcToHns).
    qpc = KeQueryPerformanceCounter(NULL);
    currentTime = QpcToHns(qpc);

    // Determine how long we want to wait, in HNS. Negative since it's a relative wait
    delay = -(LONGLONG)(nextPacketTime - currentTime);

    // If the delay isn't negative, this means we lost some time (e.g. broken into kernel debugger). Update
    // our glitch adjust to account for that lost time, and attempt to schedule again
    if (delay >= 0)
    {
        // Glitch!!!
        // Update the glitch adjustment and set the new delay.
        m_GlitchAdjust += delay;

        // noise-filter (TODO 19, spike check 5): counted, with the time it moved the stream's time line.
        if (m_Cable != NULL)
        {
            NfVmicRecordGlitch(m_Cable, m_StatsKind, (ULONGLONG)delay);
        }

        StreamPassCallback();

        return;
    }

    // noise-filter (TODO 19, spike check 5): the time this pass is due, for its lateness (s_EvtStreamPassCallback).
    m_ArmedHns = nextPacketTime;

    // Start the timer for our next pass! Note the timer isn't periodic.
    inTimerQueue = WdfTimerStart(m_NotificationTimer, delay);

    // We shouldn't be scheduling our next pass if the timer was previously still pending
    ASSERT(inTimerQueue == FALSE);
}

_Use_decl_annotations_
VOID
CStreamEngine::UpdatePosition(
    LARGE_INTEGER   Qpc
)
{
    ULONGLONG currentTime;
    ULONG bytesPerSecond;

    if (m_CurrentState != AcxStreamStateRun)
    {
        return;
    }
    bytesPerSecond = GetBytesPerSecond();
    currentTime = QpcToHns(Qpc);

    //
    // noise-filter (TODO 19): two fixes to the sample's position.
    // - The glitch adjustment is time (100 ns units, ScheduleNextPass adds it to the schedule), so it comes off the elapsed
    //   time; the sample subtracted it from the byte position, leaving the position about 0.98 x G bytes behind the packets
    //   (0.5 s after 10 ms of lateness at 192,000 B/s), which a Pause and Run then turned into a half-second stall.
    // - The position stays within the packet in progress: no packet past it has completed. A Pause that cancels a pass still
    //   queued past its due time would otherwise store a position beyond it, and the next Run's ScheduleNextPass would
    //   underflow (nextPacketStartPosition - m_StartPosition) and arm the timer about 111 days out.
    //
    ULONGLONG elapsed = currentTime - m_StartTime;
    elapsed = (elapsed > m_GlitchAdjust) ? elapsed - m_GlitchAdjust : 0;
    ULONGLONG position = m_StartPosition + NfScaleU64(elapsed, bytesPerSecond, HNS_PER_SEC);
    ULONGLONG packetEnd = (m_CurrentPacket + 1) * m_PacketSize;
    m_Position = MIN(position, packetEnd);
}

_Use_decl_annotations_
ULONG
CStreamEngine::GetBytesPerSecond()
{
    ULONG bytesPerSecond;

    bytesPerSecond = AcxDataFormatGetAverageBytesPerSec(m_StreamFormat);

    return bytesPerSecond;
}

//
// noise-filter (TODO 19): a QPC reading in 100 ns units. KSCONVERT_PERFORMANCE_TIME (ks.h:2361) names its PerformanceTime
// argument three times (HighPart twice, LowPart once), and the sample handed it KeQueryPerformanceCounter(NULL) in
// ScheduleNextPass and UpdatePosition: three clock reads per conversion (three calls in the release StreamEngine.obj). Across
// a low-word wrap (every 2^32 ticks: about 7 minutes at 10 MHz) the high word of one read could meet the low word of another,
// a time about 429 s off; ScheduleNextPass then armed its timer that far out (directly, or after a glitch adjustment that
// size), and the stream stalled. Every conversion takes one reading, passed here by value.
//
_Use_decl_annotations_
ULONGLONG
CStreamEngine::QpcToHns(
    LARGE_INTEGER   Qpc
)
{
    return KSCONVERT_PERFORMANCE_TIME(m_PerformanceCounterFrequency.QuadPart, Qpc);
}

_Use_decl_annotations_
ULONGLONG
CStreamEngine::HnsToQpc(
    ULONGLONG   Hns
)
{
    ULONGLONG frequency = (ULONGLONG)m_PerformanceCounterFrequency.QuadPart;

    // Whole seconds and the rest apart, as KSCONVERT_PERFORMANCE_TIME does, so neither product overflows.
    return (Hns / HNS_PER_SEC) * frequency + (Hns % HNS_PER_SEC) * frequency / HNS_PER_SEC;
}

_Use_decl_annotations_
ULONGLONG
CStreamEngine::PacketStartQpc(
    ULONGLONG   Packet
)
{
    ULONG     bytesPerSecond = GetBytesPerSecond();
    ULONGLONG end = (Packet + 1) * m_PacketSize;
    ULONGLONG toEnd = (end > m_StartPosition) ? end - m_StartPosition : 0;
    ULONGLONG deadline;
    ULONGLONG packetHns;

    //
    // The packet's deadline on ScheduleNextPass's time line (the latest Run's time and position, the glitch adjustments, the
    // nominal rate: the same sum, so the very time the pass was armed for), less one packet. So the stamps step by exactly a
    // packet, and across a Pause by a packet plus the pause. The first packet after a Run inside one completes early, after
    // only the rest of it, and its stamp falls before the Run by what the packet held before the Pause. (Astra's review,
    // 2026-11-23: stamped from the Run itself, the first two packets after a resume stepped 1.3-6.6 ms for 10 ms of audio.)
    //
    deadline = m_StartTime + m_GlitchAdjust + NfScaleU64(toEnd, HNS_PER_SEC, bytesPerSecond);
    packetHns = NfScaleU64(m_PacketSize, HNS_PER_SEC, bytesPerSecond);
    return HnsToQpc((deadline > packetHns) ? deadline - packetHns : 0);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::GetCurrentPacket(
    _Out_   PULONG          CurrentPacket
)
{
    ULONGLONG currentPacket;
    PAGED_CODE();

    currentPacket = (ULONGLONG)InterlockedCompareExchange64((LONG64*)&m_CurrentPacket, -1, -1);

    // ACX's packet numbers are 32-bit: the count's low 32 bits (m_CurrentPacket).
    *CurrentPacket = (ULONG)currentPacket;

    if (m_StatsKind == NF_VMIC_KIND_CAPTURE && m_Cable != NULL)
    {
        NfVmicTraceCaptureCount(m_Cable, NF_VMIC_TRACE_CURRENT_PACKET);
    }

    return STATUS_SUCCESS;
}

///////////////////////////////////////////////////////////////////////////////
//
// noise-filter (TODO 19, spike check 7): the render-to-capture loopback pair.
//
// The render pass copies each packet into the device's ring at the frame its own time line presents it; the capture pass
// copies one packet from the frame its own time line captures, a lag behind (1 ms and a frame, plus the render side's lead:
// LoopbackCapturePass). Both time lines are ScheduleNextPass's (absolute QPC, nominal rate), so the two ends stay locked with
// no drift and no fill-level control. All integer; nothing allocates; all shared state is under the ring's spin lock (held
// for one packet copy).
//

static
ULONGLONG
LoopbackHnsToFrames(
    _In_ ULONGLONG  Hns,
    _In_ ULONG      SampleRate
)
{
    // Split so the product cannot overflow (100 ns QPC time since boot times 48000 would after about 14 months).
    return (Hns / HNS_PER_SEC) * SampleRate + ((Hns % HNS_PER_SEC) * SampleRate) / HNS_PER_SEC;
}

// The same sample layout (Valid not compared).
static
BOOLEAN
LoopbackFormatSame(
    _In_ const LOOPBACK_FORMAT* A,
    _In_ const LOOPBACK_FORMAT* B
)
{
    return (A->SampleRate == B->SampleRate &&
            A->Channels == B->Channels &&
            A->BitsPerSample == B->BitsPerSample &&
            A->ValidBitsPerSample == B->ValidBitsPerSample &&
            A->BlockAlign == B->BlockAlign &&
            RtlEqualMemory(&A->SubFormat, &B->SubFormat, sizeof(GUID))) ? TRUE : FALSE;
}

static
BOOLEAN
LoopbackFormatEqual(
    _In_ const LOOPBACK_FORMAT* A,
    _In_ const LOOPBACK_FORMAT* B
)
{
    return (A->Valid && B->Valid && LoopbackFormatSame(A, B)) ? TRUE : FALSE;
}

// noise-filter (TODO 19, spike check 4): the app ring's format (nf_vmic.h), 48 kHz float32 stereo in 8-byte frames. Valid not
// looked at: the creation-time check has no packet size yet, and Run adds Valid itself (m_AppFormatOk).
static
BOOLEAN
AppRingFormatIs(
    _In_ const LOOPBACK_FORMAT* Format
)
{
    return (Format->SampleRate == NF_VMIC_SAMPLE_RATE &&
            Format->Channels == NF_VMIC_CHANNELS &&
            Format->BitsPerSample == 32 &&
            Format->ValidBitsPerSample == 32 &&
            Format->BlockAlign == NF_VMIC_FRAME_BYTES &&
            IsEqualGUID(Format->SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)) ? TRUE : FALSE;
}

// The context of the device a circuit belongs to, or NULL: its loopback ring and its cable.
static
PCODEC_DEVICE_CONTEXT
CodecContextOfCircuit(
    _In_ ACXCIRCUIT Circuit
)
{
    // AcxCircuitGetWdfDevice: acxcircuit.h:1290, up to DISPATCH_LEVEL.
    WDFDEVICE device = AcxCircuitGetWdfDevice(Circuit);

    return (device != NULL) ? GetCodecDeviceContext(device) : NULL;
}

_Use_decl_annotations_
PLOOPBACK_RING
LoopbackRingOfCircuit(
    ACXCIRCUIT  Circuit
)
{
    PCODEC_DEVICE_CONTEXT devCtx = CodecContextOfCircuit(Circuit);

    return (devCtx != NULL) ? &devCtx->Loopback : NULL;
}

_Use_decl_annotations_
PNF_CABLE
CableOfCircuit(
    ACXCIRCUIT  Circuit
)
{
    PCODEC_DEVICE_CONTEXT devCtx = CodecContextOfCircuit(Circuit);

    return (devCtx != NULL) ? &devCtx->Cable : NULL;
}

_Use_decl_annotations_
VOID
LoopbackFormatOf(
    ACXDATAFORMAT       DataFormat,
    PLOOPBACK_FORMAT    Format
)
{
    RtlZeroMemory(Format, sizeof(*Format));

    // The AcxDataFormatGet* calls: acxdataformat.h, up to DISPATCH_LEVEL.
    Format->SampleRate = AcxDataFormatGetSampleRate(DataFormat);
    Format->Channels = AcxDataFormatGetChannelsCount(DataFormat);
    Format->BitsPerSample = AcxDataFormatGetBitsPerSample(DataFormat);
    Format->ValidBitsPerSample = AcxDataFormatGetValidBitsPerSample(DataFormat);
    Format->BlockAlign = AcxDataFormatGetBlockAlign(DataFormat);
    Format->SubFormat = AcxDataFormatGetSubFormat(DataFormat);
}

//
// The creation-time check. The loopback copies raw bytes, so a render and a capture stream in different formats would give the
// capture client silence (LoopbackCapturePass's zero-fill, counted in MismatchPackets) while both opened successfully. Instead
// the second of the two is refused here, at creation, so its client can retry with the format the other end has. The check
// and the record are one locked step, so two creations on the two circuits cannot both pass with different formats.
//
_Use_decl_annotations_
NTSTATUS
LoopbackStreamOpen(
    PLOOPBACK_RING          Ring,
    BOOLEAN                 Render,
    const LOOPBACK_FORMAT * Format
)
{
    PLOOPBACK_OPEN  own = Render ? Ring->RenderOpen : Ring->CaptureOpen;
    PLOOPBACK_OPEN  other = Render ? Ring->CaptureOpen : Ring->RenderOpen;
    PLOOPBACK_OPEN  slot = NULL;
    NTSTATUS        status = STATUS_SUCCESS;
    KIRQL           oldIrql;
    ULONG           i;

    KeAcquireSpinLock(&Ring->Lock, &oldIrql);

    for (i = 0; i < LOOPBACK_OPEN_FORMATS; i++)
    {
        if (other[i].Count != 0 && !LoopbackFormatSame(&other[i].Format, Format))
        {
            status = STATUS_NOT_SUPPORTED;
            goto exit;
        }
    }

    // This format's slot on this circuit, else a free one.
    for (i = 0; i < LOOPBACK_OPEN_FORMATS; i++)
    {
        if (own[i].Count != 0 && LoopbackFormatSame(&own[i].Format, Format))
        {
            slot = &own[i];
            break;
        }
        if (own[i].Count == 0 && slot == NULL)
        {
            slot = &own[i];
        }
    }

    if (slot == NULL)
    {
        // More formats open on this circuit at once than it lists (not expected: ACX takes only listed formats).
        status = STATUS_NOT_SUPPORTED;
        goto exit;
    }

    if (slot->Count == 0)
    {
        slot->Format = *Format;
    }
    slot->Count++;

exit:
    if (!NT_SUCCESS(status))
    {
        Ring->RefusedOpens++;
    }

    KeReleaseSpinLock(&Ring->Lock, oldIrql);

    return status;
}

_Use_decl_annotations_
VOID
LoopbackStreamClose(
    PLOOPBACK_RING          Ring,
    BOOLEAN                 Render,
    const LOOPBACK_FORMAT * Format
)
{
    PLOOPBACK_OPEN  own = Render ? Ring->RenderOpen : Ring->CaptureOpen;
    BOOLEAN         found = FALSE;
    KIRQL           oldIrql;
    ULONG           i;

    KeAcquireSpinLock(&Ring->Lock, &oldIrql);

    for (i = 0; i < LOOPBACK_OPEN_FORMATS; i++)
    {
        if (own[i].Count != 0 && LoopbackFormatSame(&own[i].Format, Format))
        {
            own[i].Count--;
            found = TRUE;
            break;
        }
    }

    KeReleaseSpinLock(&Ring->Lock, oldIrql);

    // Every close matches an open (LoopbackAdoptOpen, or the creating callback's own clean-up).
    ASSERT(found);
    DBG_UNREFERENCED_LOCAL_VARIABLE(found);
}

//
// noise-filter (TODO 19, spike check 4): the app ring is a separate attachment from the loopback, so a capture stream in
// 24-in-32 could otherwise take the reader slot of an attached ring and get only zeros (AppRingCapturePass's mismatch
// branch). Each refusal makes the conflict visible at open time instead, whichever came first: here a capture stream while a
// ring is attached, in NfVmic_Attach (Control.cpp) an ATTACH while such a stream is open. Both decide under the cable's app
// ring's lock, and this records the stream under it, so a non-float open and an ATTACH can't both get through. The
// loopback's check and record (LoopbackStreamOpen) run inside, under its own lock: the app ring's, then the loopback's, the one
// order (Cable.h). Shared-mode streams run at the device format (float32), so in practice this refuses exclusive-mode clients.
// Every cable has its own app ring (the per-cable design), so the checks are per cable: a stream on one cable never meets
// another's ring.
//
_Use_decl_annotations_
NTSTATUS
CaptureStreamOpen(
    PNF_CABLE               Cable,
    const LOOPBACK_FORMAT * Format
)
{
    PNF_APP_RING    app;
    PLOOPBACK_RING  ring;
    BOOLEAN         ringFormat = AppRingFormatIs(Format);
    NTSTATUS        status = STATUS_SUCCESS;
    KIRQL           oldIrql;

    if (Cable == NULL || Cable->Ring.Loopback == NULL)
    {
        return STATUS_INVALID_DEVICE_STATE;
    }
    app = &Cable->Ring;
    ring = app->Loopback;

    KeAcquireSpinLock(&app->Lock, &oldIrql);

    if (!ringFormat && app->Header != NULL)
    {
        app->Stats.RefusedCaptureOpens++;
        status = STATUS_NOT_SUPPORTED;
    }
    else
    {
        status = LoopbackStreamOpen(ring, FALSE, Format);
        if (NT_SUCCESS(status) && !ringFormat)
        {
            app->OtherFormatCaptureOpens++;
        }
    }

    KeReleaseSpinLock(&app->Lock, oldIrql);

    return status;
}

_Use_decl_annotations_
VOID
CaptureStreamClose(
    PNF_CABLE               Cable,
    const LOOPBACK_FORMAT * Format
)
{
    PNF_APP_RING    app;
    KIRQL           oldIrql;

    // Every close matches an open, which needed the cable (CaptureStreamOpen).
    NT_ASSERT(Cable != NULL && Cable->Ring.Loopback != NULL);
    if (Cable == NULL || Cable->Ring.Loopback == NULL)
    {
        return;
    }
    app = &Cable->Ring;

    KeAcquireSpinLock(&app->Lock, &oldIrql);

    LoopbackStreamClose(app->Loopback, FALSE, Format);
    if (!AppRingFormatIs(Format))
    {
        ASSERT(app->OtherFormatCaptureOpens != 0);
        if (app->OtherFormatCaptureOpens != 0)
        {
            app->OtherFormatCaptureOpens--;
        }
    }

    KeReleaseSpinLock(&app->Lock, oldIrql);
}

// Writes Frames frames from Source (NULL: zeros) at ring frame Frame, splitting at the wrap. The lock of the ring that Buffer
// is held (a LOOPBACK_RING's, or the app ring's for its History).
static
VOID
LoopbackRingWrite(
    _In_        PUCHAR          Buffer,
    _In_        ULONG           RingFrames,
    _In_        ULONG           BlockAlign,
    _In_        ULONGLONG       Frame,
    _In_opt_    const BYTE*     Source,
    _In_        ULONGLONG       Frames
)
{
    while (Frames != 0)
    {
        ULONG   index = (ULONG)(Frame % RingFrames);
        ULONG   chunk = (ULONG)MIN(Frames, (ULONGLONG)(RingFrames - index));
        SIZE_T  bytes = (SIZE_T)chunk * BlockAlign;
        PBYTE   dest = Buffer + (SIZE_T)index * BlockAlign;

        if (Source != NULL)
        {
            RtlCopyMemory(dest, Source, bytes);
            Source += bytes;
        }
        else
        {
            RtlZeroMemory(dest, bytes);
        }

        Frame += chunk;
        Frames -= chunk;
    }
}

// Reads Frames frames at ring frame Frame into Dest, splitting at the wrap. The lock of the ring that Buffer is held.
static
VOID
LoopbackRingRead(
    _In_                                    const UCHAR*    Buffer,
    _In_                                    ULONG           RingFrames,
    _In_                                    ULONG           BlockAlign,
    _In_                                    ULONGLONG       Frame,
    _Out_writes_bytes_(Frames * BlockAlign) PBYTE           Dest,
    _In_                                    ULONGLONG       Frames
)
{
    while (Frames != 0)
    {
        ULONG   index = (ULONG)(Frame % RingFrames);
        ULONG   chunk = (ULONG)MIN(Frames, (ULONGLONG)(RingFrames - index));
        SIZE_T  bytes = (SIZE_T)chunk * BlockAlign;

        RtlCopyMemory(Dest, Buffer + (SIZE_T)index * BlockAlign, bytes);

        Dest += bytes;
        Frame += chunk;
        Frames -= chunk;
    }
}

_Use_decl_annotations_
PAGED_CODE_SEG
VOID
CStreamEngine::LoopbackSetFormat()
{
    LOOPBACK_FORMAT format;
    BOOLEAN         valid;

    PAGED_CODE();

    LoopbackFormatOf(m_StreamFormat, &format);

    //
    // The ring's positions are frames on the QPC time line, so this stream's byte clock (GetBytesPerSecond, which
    // ScheduleNextPass paces the timer by) must be exactly rate * block align, and the ring and the packets must hold whole
    // frames. Anything else stays out of the loopback (the capture side zero-fills and counts a mismatch).
    //
    valid = (m_Loopback != NULL &&
             format.SampleRate != 0 &&
             format.BlockAlign != 0 &&
             GetBytesPerSecond() == format.SampleRate * format.BlockAlign &&
             LOOPBACK_RING_BYTES % format.BlockAlign == 0 &&
             m_PacketSize != 0 &&
             m_PacketSize % format.BlockAlign == 0) ? TRUE : FALSE;

    format.Valid = valid;
    m_LbFormat = format;
    m_LbPacketFrames = valid ? m_PacketSize / format.BlockAlign : 0;
    m_LbRingFrames = valid ? LOOPBACK_RING_BYTES / format.BlockAlign : 0;
}

_Use_decl_annotations_
ULONGLONG
CStreamEngine::LoopbackAnchor()
{
    //
    // ScheduleNextPass puts this stream's byte position b at time m_StartTime + m_GlitchAdjust + (b - m_StartPosition) / bytes
    // per second (100 ns units). Ring frame n is time n / SampleRate, so b lands on ring frame
    // HnsToFrames(m_StartTime + m_GlitchAdjust) + (b - m_StartPosition) / BlockAlign; this returns that for b = 0 (to within
    // a frame of rounding). It changes only when the time line does: Run, or a glitch adjustment.
    //
    return LoopbackHnsToFrames(m_StartTime + m_GlitchAdjust, m_LbFormat.SampleRate) - m_StartPosition / m_LbFormat.BlockAlign;
}

_Use_decl_annotations_
PAGED_CODE_SEG
CRenderStreamEngine::CRenderStreamEngine(
    _In_    ACXSTREAM       Stream,
    _In_    ACXDATAFORMAT   StreamFormat,
    _In_    BOOL            Offload,
    _In_    CSimPeakMeter * CircuitPeakmeter

)
    : CStreamEngine(Stream, StreamFormat, Offload, CircuitPeakmeter),
    m_LbNextPacket(0),
    m_LbReleased(0),
    m_LbPauseEndPlusOne(0),
    m_LbPausedPacket(0),
    m_LbPausedReleases(0),
    m_LbRunBegan(FALSE)
{
    PAGED_CODE();
    KeInitializeEvent(&m_FirstRunDone, SynchronizationEvent, FALSE);
    KeInitializeDpc(&m_FirstRunDpc, FirstRunDpc, this);
}

_Use_decl_annotations_
PAGED_CODE_SEG
CRenderStreamEngine::~CRenderStreamEngine()
{
    PAGED_CODE();

    // noise-filter (TODO 19): give the ring up if ReleaseHardware did not.
    LoopbackRenderStop(TRUE);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::PrepareHardware()
{
    PAGED_CODE();

    //
    // noise-filter (TODO 19): the render data goes to the device's loopback ring (LoopbackRenderPass), so the sample's
    // SaveData sink is no longer set up (m_SaveData.SetDataFormat, Initialize, SetMaxWriteSize).
    //
    const BOOLEAN newLifetime = m_CurrentState == AcxStreamStateStop;
    NTSTATUS status = CStreamEngine::PrepareHardware();
    if (NT_SUCCESS(status) && newLifetime)
    {
        m_FirstRunPending = TRUE;
    }
    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::ReleaseHardware()
{
    NTSTATUS status;

    PAGED_CODE();

    //
    // noise-filter (TODO 19): no SaveData sink to drain (m_SaveData.WaitAllWorkItems, Cleanup). The base stops and deletes
    // the timer, flushes queued DPCs and resets the packet count to 0, so no pass runs while the loopback state resets.
    //
    status = CStreamEngine::ReleaseHardware();

    LoopbackRenderStop(TRUE);
    InterlockedExchange64(&m_LbReleased, 0);
    InterlockedExchange(&m_LbRunBegan, FALSE);
    m_LbRetiredPlusOne = 0;
    for (ULONG i = 0; i < MAX_PACKET_COUNT; ++i)
    {
        InterlockedExchange64(&m_LbReissue[i], 0);
        m_LbAppliedReissue[i] = 0;
        m_LbPublishedPlusOne[i] = 0;
    }
    m_LbNextPacket = 0;
    m_LbAnchorValid = FALSE;

    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::AssignDrmContentId(
    ULONG          DrmContentId,
    PACXDRMRIGHTS  DrmRights
    )
{
    PAGED_CODE();

    UNREFERENCED_PARAMETER(DrmContentId);

    //
    // At this point the driver should enforce the new DrmRights.
    // The sample driver handles DrmRights per stream basis, and 
    // stops writing the stream to disk, if CopyProtect = TRUE.
    //
    // HDMI render: if DigitalOutputDisable or CopyProtect is true, enable HDCP.
    // Loopback: if CopyProtect is true, disable loopback stream.
    //
    
    //
    // Sample writes each stream seperately to disk. If the rights for this
    // stream indicates that the stream is CopyProtected, stop writing to disk.
    //
    m_SaveData.Disable(DrmRights->CopyProtect);

    //
    // From MSDN:
    //
    // This sample doesn't forward protected content, but if your driver uses 
    // lower layer drivers or a different stack to properly work, please see the 
    // following info from MSDN:
    //
    // "Before allowing protected content to flow through a data path, the system
    // verifies that the data path is secure. To do so, the system authenticates
    // each module in the data path beginning at the upstream end of the data path
    // and moving downstream. As each module is authenticated, that module gives
    // the system information about the next module in the data path so that it
    // can also be authenticated. To be successfully authenticated, a module's 
    // binary file must be signed as DRM-compliant.
    //
    // Two adjacent modules in the data path can communicate with each other in 
    // one of several ways. If the upstream module calls the downstream module 
    // through IoCallDriver, the downstream module is part of a WDM driver. In 
    // this case, the upstream module calls the AcxDrmForwardContentToDeviceObject
    // function to provide the system with the device object representing the 
    // downstream module. (If the two modules communicate through the downstream
    // module's content handlers, the upstream module calls AcxDrmAddContentHandlers
    // instead.)
    //
    // For more information, see MSDN's DRM Functions and Interfaces.
    //

    return STATUS_SUCCESS;
}

// The two DMA slots can contain only the newest release and its immediate predecessor. Pack both generation tags in one
// atomic word: (newest + 1) * 2 + previousWasReleased, zero means no notices. EOS encodes the final length with the tags.
// With two notifications this preserves a skipped packet exactly, even when the client submits the following packet before
// the skipped one's deadline. A separate field would let the DPC observe the new generation with the old slot's valid bit.
static_assert(MAX_PACKET_COUNT == 2);
// Returns ordinary (positive) release tags and the exclusive end frame plus one (zero for an open stream). For a sealed
// final packet its length is 0..PacketFrames, so a stride of PacketFrames+1 also represents an empty final packet.
static LONG64
LoopbackDecodeEos(LONG64 State, ULONG PacketFrames, ULONGLONG* EndPlusOne)
{
    *EndPlusOne = 0;
    if (State >= 0)
    {
        return State;
    }
    ULONGLONG bits = ~(ULONGLONG)State;
    ULONGLONG packed = bits >> 1;
    ULONGLONG packet = packed / ((ULONGLONG)PacketFrames + 1);
    ULONGLONG valid = packed - packet * ((ULONGLONG)PacketFrames + 1);
    *EndPlusOne = packet * PacketFrames + valid + 1;
    return (LONG64)(((packet + 1) << 1) | (bits & 1));
}

static BOOLEAN
LoopbackHasPacket(LONG64 State, ULONGLONG Packet)
{
    ULONGLONG bits = (ULONGLONG)State;
    ULONGLONG newestPlusOne = bits >> 1;
    return newestPlusOne == Packet + 1 || ((bits & 1) != 0 && newestPlusOne == Packet + 2);
}

static NTSTATUS
LoopbackReleasePacket(volatile LONG64* Released, ULONGLONG Packet, BOOLEAN SinglePreroll, BOOLEAN* Repeated,
                      ULONG PacketFrames = 0, ULONG ValidFrames = 0)
{
    *Repeated = FALSE;
    BOOLEAN eos = PacketFrames != 0;
    LONG64 observed = InterlockedCompareExchange64(Released, 0, 0);
    for (;;)
    {
        if (observed < 0)
        {
            return STATUS_INVALID_DEVICE_STATE;
        }
        ULONGLONG newestPlusOne = (ULONGLONG)observed >> 1;
        // WASAPI can announce P+1 while paused with just P's buffer filled; it announces P+1 again after Start. The explicit
        // KS pre-roll of P then P+1, including a partial/empty final P+1, unambiguously identifies both. Once P was submitted,
        // a second P+1 is literal. EOS always names its literal packet.
        ULONGLONG next = SinglePreroll && !LoopbackHasPacket(observed, Packet - 1) ? Packet : Packet + 1;
        if (eos && newestPlusOne > next)
        {
            return STATUS_DATA_LATE_ERROR;
        }
        ULONGLONG bits;
        if (next > newestPlusOne)
        {
            // Carry forward only a genuinely submitted predecessor. A first notice for packet 1 also covers the implicit
            // initial packet 0 of a WASAPI shared stream that began in cyclic DMA mode. This applies only to the first pair.
            BOOLEAN previous = (newestPlusOne != 0 && next - newestPlusOne == 1) ||
                               (newestPlusOne == 0 && next == 2 && !eos);
            bits = (next << 1) | (previous ? 1u : 0u);
        }
        else if (next == newestPlusOne)
        {
            bits = (ULONGLONG)observed;
        }
        else if (newestPlusOne - next == 1)
        {
            // A delayed normal notification can still authenticate its own slot, never a later generation in the same half.
            bits = (ULONGLONG)observed | 1;
        }
        else
        {
            return STATUS_SUCCESS;
        }
        LONG64 updated = (LONG64)bits;
        if (eos)
        {
            ULONGLONG stride = (ULONGLONG)PacketFrames + 1;
            if (Packet > (((ULONGLONG)MAXLONGLONG >> 1) - ValidFrames) / stride)
            {
                return STATUS_INTEGER_OVERFLOW;
            }
            updated = (LONG64)~(((Packet * stride + ValidFrames) << 1) | (bits & 1));
        }
        if (updated == observed)
        {
            *Repeated = next == Packet + 1 && LoopbackHasPacket(observed, Packet);
            return STATUS_SUCCESS;
        }
        LONG64 previous = InterlockedCompareExchange64(Released, updated, observed);
        if (previous == observed)
        {
            *Repeated = next == Packet + 1 && LoopbackHasPacket(observed, Packet);
            return STATUS_SUCCESS;
        }
        observed = previous;
    }
}

// A slot needs just one pending revision. The generation tag prevents a delayed callback from altering a newer packet
// occupying the same physical half. Sixteen bits distinguish repeat publications until the ring next observes this slot.
static VOID
LoopbackReissue(volatile LONG64* Slot, ULONGLONG Packet)
{
    ULONGLONG generation = (Packet + 1) << 16;
    LONG64 observed = ReadNoFence64(Slot);
    for (;;)
    {
        ULONGLONG prior = (ULONGLONG)observed & ~0xffffull;
        if (prior > generation)
        {
            return;
        }
        LONG64 updated = (LONG64)(generation | (prior == generation ? ((ULONG)observed + 1) & 0xffffu : 1u));
        LONG64 previous = InterlockedCompareExchange64(Slot, updated, observed);
        if (previous == observed)
        {
            return;
        }
        observed = previous;
    }
}

// Trace metadata for one optional startup diagnostic. Report frame indices only (-1: entirely zero).
static VOID
LoopbackTraceEdges(const BYTE* Bytes, ULONG Frames, ULONG Align, LONG* First, LONG* Last)
{
    *First = *Last = -1;
    if (Bytes == NULL)
    {
        return;
    }
    for (ULONG i = 0; i < Frames; ++i)
    {
        for (ULONG j = 0; j < Align; ++j)
        {
            if (Bytes[(SIZE_T)i * Align + j] != 0)
            {
                if (*First < 0)
                {
                    *First = (LONG)i;
                }
                *Last = (LONG)i;
                break;
            }
        }
    }
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::SetRenderPacket(
    _In_    ULONG           Packet,
    _In_    ULONG           Flags,
    _In_    ULONG           EosPacketLength
)
{
    NTSTATUS status = STATUS_SUCCESS;
    ULONGLONG currentPacket;
    LONG ahead;

    PAGED_CODE();

    if ((m_CurrentState != AcxStreamStatePause && m_CurrentState != AcxStreamStateRun) ||
        ReadNoFence64(&m_LbReleased) < 0)
    {
        return STATUS_INVALID_DEVICE_STATE;
    }
    if ((Flags & ~KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM) != 0)
    {
        return STATUS_INVALID_PARAMETER;
    }

    currentPacket = (ULONGLONG)InterlockedCompareExchange64((LONG64*)&m_CurrentPacket, -1, -1);

    //
    // noise-filter (TODO 19, Astra's recheck 2026-11-23): Packet is ACX's 32-bit number, the count's low 32 bits, so the two
    // compare by their difference. The sample compared them directly: at 2^32 packets the client's packet 0 was "late".
    //
    ahead = (LONG)(Packet - (ULONG)currentPacket);

    // In Pause the current packet can be pre-rolled before the first Run or a resume. In Run it is already transferring.
    if (ahead < 0 || (ahead == 0 && m_CurrentState != AcxStreamStatePause))
    {
        //ASSERT(FALSE);
        status = STATUS_DATA_LATE_ERROR;
    }
    else if (ahead > 1)
    {
        //ASSERT(FALSE);
        status = STATUS_DATA_OVERRUN;
    }

    if ((currentPacket < 4 || (Flags & KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM) != 0) && NF_LOOPBACK_TRACE_ENABLED())
    {
        DoTraceMessage(FLAG_STREAM, "render release stream=%lu state=%lu current=%I64u offered=%lu flags=0x%lx eosBytes=%lu window=0x%lx",
                       m_Serial, (ULONG)m_CurrentState, currentPacket, Packet, Flags, EosPacketLength, (ULONG)status);
    }

    //
    // noise-filter (TODO 19): remember the newest packet the client has finished writing, for the loopback's look-ahead
    // (LoopbackRenderPass). A late one counts too if its data is still in the buffer.
    //
    // Kept on the 64-bit count, plus one (0: none); a late packet from before the stream's first (no such packet) is ignored.
    if (status != STATUS_DATA_OVERRUN && (ahead >= 0 || (ULONGLONG)(-(LONGLONG)ahead) <= currentPacket))
    {
        ULONGLONG   packet = currentPacket + (ULONGLONG)(LONGLONG)ahead;
        BOOLEAN     repeated;

        if ((Flags & KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM) != 0)
        {
            // A rejected packet cannot terminate the stream or invalidate previously released data.
            if (!NT_SUCCESS(status))
            {
                return status;
            }
            ULONG align = AcxDataFormatGetBlockAlign(m_StreamFormat);
            if (align == 0 || m_PacketSize == 0 || m_PacketSize % align != 0 ||
                EosPacketLength > m_PacketSize || EosPacketLength % align != 0)
            {
                return STATUS_INVALID_PARAMETER;
            }
            status = LoopbackReleasePacket(&m_LbReleased, packet, FALSE, &repeated, m_PacketSize / align, EosPacketLength / align);
            if (NT_SUCCESS(status) && repeated && m_PacketsCount == MAX_PACKET_COUNT)
            {
                LoopbackReissue(&m_LbReissue[packet % MAX_PACKET_COUNT], packet);
            }
            if (NF_LOOPBACK_TRACE_ENABLED())
            {
                DoTraceMessage(FLAG_STREAM, "render EOS stream=%lu packet=%I64u bytes=%lu result=0x%lx",
                               m_Serial, packet, EosPacketLength, (ULONG)status);
            }
            return status;
        }

        // A first P+1 before Run can be WASAPI's announcement of its single pre-filled P. Inside Run, even before the
        // stream has acquired its ring lock and changed state to Run, the shared engine can already be submitting P+1.
        BOOLEAN singlePreroll = ahead == 1 && m_PacketsCount == MAX_PACKET_COUNT && m_CurrentState == AcxStreamStatePause &&
                                ReadAcquire(&m_LbRunBegan) == FALSE;
        if (currentPacket == 1 && packet == 2 && m_PacketsCount == MAX_PACKET_COUNT && m_LbFormat.Valid && NF_LOOPBACK_TRACE_ENABLED())
        {
            LONG first, last;
            LoopbackTraceEdges(LoopbackPacketBuffer(0), m_LbPacketFrames, m_LbFormat.BlockAlign, &first, &last);
            DoTraceMessage(FLAG_STREAM, "render packet2 offer stream=%lu qpc=%I64d frames=%lu firstNonzero=%ld lastNonzero=%ld",
                           m_Serial, KeQueryPerformanceCounter(NULL).QuadPart, m_LbPacketFrames, first, last);
        }
        NTSTATUS release = LoopbackReleasePacket(&m_LbReleased, packet, singlePreroll, &repeated);
        if (!NT_SUCCESS(release))
        {
            return release;
        }
        if (repeated && ahead >= 0 && m_PacketsCount == MAX_PACKET_COUNT)
        {
            // Still report STATUS_DATA_LATE_ERROR for the transferring packet. Its already delivered frames cannot be
            // recalled; a reader that has not reached it can still hear the replacement before ACX returns the slot.
            LoopbackReissue(&m_LbReissue[packet % MAX_PACKET_COUNT], packet);
        }
    }

    return status;
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::ProcessPacket()
{
    //
    // noise-filter (TODO 19): the render half of the loopback pair, in place of the sample's m_SaveData.WriteData.
    //
    LoopbackRenderPass(FALSE);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::Run()
{
    PAGED_CODE();

    if (m_CurrentState == AcxStreamStatePause)
    {
        InterlockedExchange(&m_LbRunBegan, TRUE);
        // The timer is stopped in Pause, so no pass reads these while they change. The packets exist by now (inference: ACX
        // normally allocates them before PrepareHardware, Learn acx-streaming, and a client cannot start without them).
        LoopbackSetFormat();
        if (NF_LOOPBACK_TRACE_ENABLED())
        {
            DoTraceMessage(FLAG_STREAM, "render Run stream=%lu packetFrames=%lu packetCount=%lu", m_Serial, m_LbPacketFrames, m_PacketsCount);
        }
    }

    NTSTATUS status = CStreamEngine::Run();
    if (!NT_SUCCESS(status))
    {
        InterlockedExchange(&m_LbRunBegan, FALSE);
    }
    return status;
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::BeginRunClock()
{
    // ACX invokes Run at PASSIVE_LEVEL. A passive acquisition of the loopback spin lock can take several ms under
    // Force IRQL checking. If the first completion slips relative to the shared engine's clock, it can submit different
    // audio twice for one future WaveRT packet. Keep the initial clock and pre-roll together under the existing lock.
    const BOOLEAN first = m_FirstRunPending;
    m_FirstRunPending = FALSE;
    if (first && m_PacketsCount == MAX_PACKET_COUNT && m_LbFormat.Valid && KeGetCurrentIrql() == PASSIVE_LEVEL)
    {
        // Only one initial Run per prepared hardware lifetime; no loopback locks held while waiting. The SynchronizationEvent
        // is consumed by this wait, including when the DPC finished before the wait began. Run arms its timer only afterwards.
        // FALSE means this same per-stream DPC is already queued. Its completion is still the one to wait for.
        const BOOLEAN queued = KeInsertQueueDpc(&m_FirstRunDpc, NULL, NULL);
        NT_ASSERT(queued);
        UNREFERENCED_PARAMETER(queued);
        (void)KeWaitForSingleObject(&m_FirstRunDone, Executive, KernelMode, FALSE, NULL);
        return;
    }
    LoopbackRenderPass(TRUE);
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::FirstRunDpc(KDPC* Dpc, PVOID Context, PVOID Arg1, PVOID Arg2)
{
    UNREFERENCED_PARAMETER(Dpc);
    UNREFERENCED_PARAMETER(Arg1);
    UNREFERENCED_PARAMETER(Arg2);
    CRenderStreamEngine* engine = static_cast<CRenderStreamEngine*>(Context);
    engine->LoopbackRenderPass(TRUE);
    // Last stream access. ReleaseHardware subsequently flushes queued DPCs before the stream object can be deleted.
    KeSetEvent(&engine->m_FirstRunDone, IO_NO_INCREMENT, FALSE);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::Pause()
{
    NTSTATUS status;

    PAGED_CODE();

    if (m_CurrentState == AcxStreamStateRun && m_LbFormat.Valid)
    {
        // ACX can deliver a packet just before Pause without another render tick. Freeze its complete publication (including
        // any EOS length) before announcing the cutoff; subsequent releases cannot authenticate bytes for the stopped run.
        // The capture reader observes the cutoff with acquire ordering. The next Run clears it under the same reader lock.
        m_LbPausedPacket = (ULONGLONG)ReadNoFence64((LONG64*)&m_CurrentPacket);
        m_LbPausedReleases = ReadAcquire64(&m_LbReleased);
        for (ULONG i = 0; i < MAX_PACKET_COUNT; ++i)
        {
            m_LbPausedReissue[i] = ReadAcquire64(&m_LbReissue[i]);
        }
        LARGE_INTEGER pauseQpc = KeQueryPerformanceCounter(NULL);
        ULONGLONG end = LoopbackHnsToFrames(QpcToHns(pauseQpc), m_LbFormat.SampleRate);
        InterlockedExchange64(&m_LbPauseEndPlusOne, (LONG64)(end + 1));
        if (NF_LOOPBACK_TRACE_ENABLED())
        {
            DoTraceMessage(FLAG_STREAM, "render Pause stream=%lu qpc=%I64d end=%I64u", m_Serial, pauseQpc.QuadPart, end);
        }
    }
    status = CStreamEngine::Pause();

    if (NT_SUCCESS(status))
    {
        // The timer has stopped (WdfTimerStop waits for its callback): the capture side reads what is left, then idles.
        LoopbackRenderStop(FALSE);
        InterlockedExchange(&m_LbRunBegan, FALSE);
    }

    return status;
}

ULONGLONG
CRenderStreamEngine::AbsoluteEosEndPlusOne()
{
    // Called with the ring lock held. A PASSIVE_LEVEL EOS publisher never has to acquire it: its CAS made the final slot
    // and the precise valid length visible together. Until Run establishes the anchor, there is no absolute frame address.
    ULONGLONG eos;
    LONG64 releases = ReadAcquire64(&m_LbPauseEndPlusOne) != 0 ? m_LbPausedReleases : ReadNoFence64(&m_LbReleased);
    (void)LoopbackDecodeEos(releases, m_LbPacketFrames, &eos);
    return eos != 0 && m_LbAnchorValid ? m_LbAnchor + eos : 0;
}

// Repair a release revised while ACX still owns that generation. This runs under the ring lock before capture reads,
// before render returns a completed slot, and while Pause drains. Already-delivered frames stay with their reader;
// changing the existing ring bytes neither changes a reader's position nor publishes any new time.
VOID
CRenderStreamEngine::RefreshReissuedPackets()
{
    for (ULONG slot = 0; slot < MAX_PACKET_COUNT; ++slot)
    {
        LONG64 revision = ReadAcquire64(&m_LbReissue[slot]);
        if (revision == m_LbAppliedReissue[slot])
        {
            continue;
        }
        ULONGLONG generation = (ULONGLONG)revision >> 16;
        if (generation == 0 || m_LbPublishedPlusOne[slot] != generation || !m_LbAnchorValid || m_LbAnchor != LoopbackAnchor())
        {
            continue;
        }
        ULONGLONG packet = generation - 1;
        ULONGLONG current = (ULONGLONG)ReadNoFence64((LONG64*)&m_CurrentPacket);
        ULONGLONG eos;
        LONG64 released = LoopbackDecodeEos(ReadAcquire64(&m_LbReleased), m_LbPacketFrames, &eos);
        if (packet < m_LbRetiredPlusOne || packet < current || packet - current > 1 || !LoopbackHasPacket(released, packet))
        {
            continue;
        }
        ULONGLONG pause = (ULONGLONG)ReadAcquire64(&m_LbPauseEndPlusOne);
        ULONGLONG from = m_LbAnchor + packet * m_LbPacketFrames;
        if (pause != 0)
        {
            LONG64 frozen = LoopbackDecodeEos(m_LbPausedReleases, m_LbPacketFrames, &eos);
            if (revision != m_LbPausedReissue[slot] || packet < m_LbPausedPacket || packet - m_LbPausedPacket > 1 ||
                !LoopbackHasPacket(frozen, packet) || from >= pause - 1)
            {
                continue;
            }
        }
        if (from < m_Loopback->ValidFrom || from >= m_Loopback->WritePos || m_Loopback->WritePos - from > m_LbRingFrames)
        {
            continue;
        }
        ULONG frames = (ULONG)MIN((ULONGLONG)m_LbPacketFrames, m_Loopback->WritePos - from);
        if (eos != 0)
        {
            ULONGLONG end = m_LbAnchor + eos - 1;
            frames = from < end ? (ULONG)MIN((ULONGLONG)frames, end - from) : 0;
        }
        LoopbackRingWrite(m_Loopback->Buffer, m_LbRingFrames, m_LbFormat.BlockAlign, from, LoopbackPacketBuffer(slot), frames);
        m_LbAppliedReissue[slot] = revision;
    }
}

// Publish only the contiguous released slots reached by this reader's demand (or the Pause cutoff). Both the current slot
// and the next remain owned until their completion notifications. A delayed render tick can leave the read deadline just
// inside the next packet. The shared lock serialises this copy with the normal timer and all other readers.
VOID
CRenderStreamEngine::CopyReleasedPackets(ULONGLONG DemandEnd)
{
    for (ULONG slots = 0; slots < MAX_PACKET_COUNT && m_Loopback->WritePos < DemandEnd; ++slots)
    {
        if (!CopyReleasedPacket())
        {
            break;
        }
    }
}

BOOLEAN
CRenderStreamEngine::CopyReleasedPacket()
{
    ULONGLONG current = (ULONGLONG)ReadNoFence64((LONG64*)&m_CurrentPacket);
    ULONGLONG packet = m_LbNextPacket;
    if (m_PacketsCount != MAX_PACKET_COUNT || packet < m_LbRetiredPlusOne || packet < current || packet - current > 1 || !m_LbAnchorValid)
    {
        return FALSE;
    }
    ULONGLONG pause = (ULONGLONG)ReadAcquire64(&m_LbPauseEndPlusOne);
    if (pause != 0 && (packet < m_LbPausedPacket || packet - m_LbPausedPacket > 1))
    {
        return FALSE;
    }
    ULONGLONG eos;
    LONG64 released = LoopbackDecodeEos(InterlockedCompareExchange64(&m_LbReleased, 0, 0), m_LbPacketFrames, &eos);
    if (released == 0 || !LoopbackHasPacket(released, packet) || m_LbAnchor != LoopbackAnchor())
    {
        return FALSE;
    }
    if (pause != 0)
    {
        // Only a generation already released at Pause whose playback had begun belongs to this run. Drain its full valid
        // packet just as a look-ahead copy would; a future or genuinely absent packet remains silent. The live tag above also
        // confirms the same slot has not since been reclaimed. A pre-roll EOS for the next run cannot truncate the old one.
        LONG64 frozen = LoopbackDecodeEos(m_LbPausedReleases, m_LbPacketFrames, &eos);
        if (!LoopbackHasPacket(frozen, packet) || m_Loopback->WritePos >= pause - 1)
        {
            return FALSE;
        }
    }
    ULONGLONG from = packet * m_LbPacketFrames;
    if (m_Loopback->WritePos != m_LbAnchor + from || (eos != 0 && eos - 1 <= from))
    {
        return FALSE;
    }
    ULONG frames = eos != 0 ? (ULONG)MIN((ULONGLONG)m_LbPacketFrames, eos - 1 - from) : m_LbPacketFrames;
    ULONG slot = (ULONG)(packet % MAX_PACKET_COUNT);
    LONG64 revision = ReadAcquire64(&m_LbReissue[slot]);
    if (pause != 0 && revision != m_LbPausedReissue[slot])
    {
        return FALSE;
    }
    LoopbackRingWrite(m_Loopback->Buffer, m_LbRingFrames, m_LbFormat.BlockAlign, m_Loopback->WritePos,
                      LoopbackPacketBuffer(slot), frames);
    m_LbAppliedReissue[slot] = revision;
    m_LbPublishedPlusOne[slot] = packet + 1;
    m_Loopback->WritePos += frames;
    m_LbNextPacket++;
    if (packet == current)
    {
        m_Loopback->RenderLatePackets++;
    }
    return TRUE;
}

// These payload classifications are for an explicitly enabled short startup trace only. They reveal no sample values.
static ULONG
LoopbackTraceNonzero(_In_reads_bytes_opt_(Bytes) const BYTE* Buffer, SIZE_T Bytes)
{
    if (Buffer != NULL)
    {
        for (SIZE_T i = 0; i < Bytes; ++i)
        {
            if (Buffer[i] != 0)
            {
                return 1;
            }
        }
    }
    return 0;
}

static ULONG
LoopbackTraceZeroMask(_In_reads_bytes_opt_(Bytes) const BYTE* Buffer, SIZE_T Bytes)
{
    if (Bytes < sizeof(ULONG))
    {
        return 4;
    }
    if (Buffer == NULL)
    {
        return 3;
    }
    ULONG first, last;
    RtlCopyMemory(&first, Buffer, sizeof(first));
    RtlCopyMemory(&last, Buffer + Bytes - sizeof(last), sizeof(last));
    return (first == 0 ? 1u : 0u) | (last == 0 ? 2u : 0u);
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::LoopbackRenderPass(
    BOOLEAN AtRun
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;
    ULONGLONG       current, completingPlusOne = 0;
    ULONGLONG       endPacket;
    ULONGLONG       anchor;
    ULONGLONG       eos;
    ULONGLONG       pauseCutoff;
    LONG64          releaseState;
    LONG64          releases;
    BOOLEAN         lookAhead;
    BOOLEAN         copied = FALSE;
    struct {
        ULONG Packet, Slot, ZeroMask, Nonzero;
        LONG FirstNonzero, LastNonzero;
        ULONGLONG RingFrame;
        LONG64 Released;
    } trace[MAX_PACKET_COUNT];
    ULONG traceCount = 0;
    struct {
        ULONG Frames, ZeroMask, Nonzero, PublishedNonzero;
        ULONGLONG Anchor;
    } slot1 = {};
    BOOLEAN traceCompletion = FALSE;

    if (ring == NULL || !m_LbFormat.Valid)
    {
        if (AtRun)
        {
            CStreamEngine::BeginRunClock();
        }
        return;
    }

    KeAcquireSpinLock(&ring->Lock, &oldIrql);
    if (AtRun)
    {
        // Acquire before sampling QPC. A long passive lock acquisition (notably with Driver Verifier) must not consume
        // the first render period while Start is still on the stack. The first DPC cannot pass this pre-roll or copy the
        // client's second DMA slot before Start returns just because this lock was delayed.
        CStreamEngine::BeginRunClock();
        InterlockedExchange64(&m_LbPauseEndPlusOne, 0);
    }

    if (ring->Buffer == NULL)
    {
        goto exit;
    }

    if (ring->Owner == NULL)
    {
        // Claim the ring: a new run, in this stream's format.
        ring->Owner = this;
        ring->RenderFormat = m_LbFormat;
        ring->RenderActive = FALSE;
        ring->ValidFrom = 0;
        ring->WritePos = 0;
        ring->EosEndPlusOne = 0;
        ring->PausedEndPlusOne = 0;
        m_LbAnchorValid = FALSE;
    }
    else if (ring->Owner != this)
    {
        ring->RenderNotOwner++;
        goto exit;
    }

    if (AtRun)
    {
        ring->PausedEndPlusOne = 0;
        RtlZeroMemory(m_LbPublishedPlusOne, sizeof(m_LbPublishedPlusOne));
    }
    else
    {
        // After this pass ACX is free to overwrite the completing DMA slot.
        completingPlusOne = (ULONGLONG)ReadNoFence64((LONG64*)&m_CurrentPacket) + 1;
        RefreshReissuedPackets();
    }

    //
    // Which packets are final. In the two-packet event-driven mode the client writes packet k+1 while the driver streams
    // packet k and says so with SetRenderPacket(k+1) (Learn, EVT_ACX_STREAM_SET_RENDER_PACKET); it writes k+2 into k's half
    // only after AcxRtStreamNotifyPacketComplete(k), which StreamPassCallback calls after this pass. So at the pass completing
    // k, packet k is final and k+1 is final once released: copying it now puts it in the ring a period before it is
    // presented. Without SetRenderPacket in this run (or in one-packet mode) each packet is copied as it completes.
    //
    current = (ULONGLONG)InterlockedCompareExchange64((LONG64*)&m_CurrentPacket, -1, -1);
    pauseCutoff = (ULONGLONG)ReadAcquire64(&m_LbPauseEndPlusOne);
    releaseState = InterlockedCompareExchange64(&m_LbReleased, 0, 0);
    if (!AtRun && pauseCutoff != 0 && (releaseState != 0 || m_LbPausedReleases != 0))
    {
        // An in-flight DPC must not promote a post-Pause release into this run, nor consume a future slot. An already
        // completed or reclaimed current generation cannot be used a second time. Commit only the frozen current packet
        // (if due and still valid); the base timer independently finishes its ACX notification and retires.
        CopyReleasedPackets(pauseCutoff - 1);
        goto exit;
    }
    releases = LoopbackDecodeEos(releaseState, m_LbPacketFrames, &eos);
    if (!AtRun && current == 1 && m_PacketsCount == MAX_PACKET_COUNT && m_LbAnchorValid && NF_LOOPBACK_TRACE_ENABLED())
    {
        // Packet 1 is still owned by the driver until the timer notifies ACX after this pass. A previous pass (or an
        // underfilled capture read) may already have published it. Compare the exact ring bytes with the slot as it is now.
        traceCompletion = TRUE;
        slot1.Anchor = m_LbAnchor;
        slot1.Frames = eos == 0 ? m_LbPacketFrames : (eos - 1 > m_LbPacketFrames ?
            (ULONG)MIN((ULONGLONG)m_LbPacketFrames, eos - 1 - m_LbPacketFrames) : 0);
        SIZE_T bytes = (SIZE_T)slot1.Frames * m_LbFormat.BlockAlign;
        PBYTE source = LoopbackPacketBuffer(1);
        slot1.ZeroMask = LoopbackTraceZeroMask(source, bytes);
        slot1.Nonzero = LoopbackTraceNonzero(source, bytes);
        slot1.PublishedNonzero = 2; // unavailable at the time of this snapshot
        ULONGLONG first = m_LbAnchor + m_LbPacketFrames;
        if (m_LbNextPacket > 1 && first >= ring->ValidFrom && first <= ring->WritePos &&
            ring->WritePos - first <= m_LbRingFrames && slot1.Frames <= ring->WritePos - first)
        {
            ULONG index = (ULONG)(first % m_LbRingFrames);
            SIZE_T head = (SIZE_T)MIN(slot1.Frames, m_LbRingFrames - index) * m_LbFormat.BlockAlign;
            slot1.PublishedNonzero = LoopbackTraceNonzero(ring->Buffer + (SIZE_T)index * m_LbFormat.BlockAlign, head) ||
                                     LoopbackTraceNonzero(ring->Buffer, bytes - head);
        }
    }
    lookAhead = (m_PacketsCount == MAX_PACKET_COUNT && releases != 0) ? TRUE : FALSE;
    if (AtRun)
    {
        // Before the first pass only the current packet is in play: the client's pre-roll.
        endPacket = (lookAhead && LoopbackHasPacket(releases, current)) ? current + 1 : current;
    }
    else
    {
        // ProcessPacket runs before the packet count moves on, so current is the completing packet.
        endPacket = (lookAhead && LoopbackHasPacket(releases, current + 1)) ? current + 2 : current + 1;
    }

    if (eos != 0)
    {
        // The stream timer and ACX notifications keep their normal cadence. The cable stops reading the circular DMA slots
        // once every valid frame is copied, and retains the last run's anchor while its capture clients drain the tail.
        ULONGLONG frames = eos - 1;
        ULONGLONG packets = frames / m_LbPacketFrames + (frames % m_LbPacketFrames != 0);
        if (m_LbAnchorValid && m_LbNextPacket >= packets)
        {
            goto exit;
        }
        current = MIN(current, packets);
        endPacket = MIN(endPacket, packets);
    }

    //
    // Anchor this stream on the ring's time line, again whenever its own time line moves (Run after Pause, or a late timer's
    // glitch adjustment in ScheduleNextPass). A short move later keeps the run and zeros the gap; anything else starts a new
    // run at the oldest packet still in the buffer.
    //
    anchor = LoopbackAnchor();
    if (!m_LbAnchorValid || anchor != m_LbAnchor)
    {
        ULONGLONG   next;
        ULONGLONG   maxGap = LoopbackHnsToFrames(LOOPBACK_MAX_GAP_HNS, m_LbFormat.SampleRate);
        BOOLEAN     sameRun = m_LbAnchorValid;

        if (!sameRun && current > m_LbNextPacket)
        {
            m_LbNextPacket = current;
        }

        next = anchor + m_LbNextPacket * m_LbPacketFrames;

        if (sameRun && next >= ring->WritePos && next - ring->WritePos <= maxGap)
        {
            LoopbackRingWrite(ring->Buffer, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, NULL, next - ring->WritePos);
            ring->RenderGapFills++;
        }
        else
        {
            ring->ValidFrom = next;
            ring->RenderRestarts++;
        }

        ring->WritePos = next;
        m_LbAnchor = anchor;
        m_LbAnchorValid = TRUE;
    }

    //
    // Packets that left the buffer uncopied (not expected: every packet gets a pass, even in ScheduleNextPass's glitch
    // path): zeros, to keep the time line.
    //
    if (current > m_LbNextPacket)
    {
        ULONGLONG   lost = current - m_LbNextPacket;
        ULONGLONG   lostFrames = lost * m_LbPacketFrames;
        if (eos != 0)
        {
            lostFrames = MIN(lostFrames, (eos - 1) - m_LbNextPacket * m_LbPacketFrames);
        }

        LoopbackRingWrite(ring->Buffer, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, NULL,
                          MIN(lostFrames, (ULONGLONG)m_LbRingFrames));
        ULONGLONG pause = PauseEndPlusOne();
        if (pause != 0)
        {
            ULONGLONG due = pause - 1 > ring->WritePos ? MIN(lostFrames, pause - 1 - ring->WritePos) : 0;
            lost = due / m_LbPacketFrames + (due % m_LbPacketFrames != 0);
        }
        ring->WritePos += lostFrames;
        ring->RenderLostPackets += (ULONG)MIN(lost, (ULONGLONG)MAXULONG);
        m_LbNextPacket = current;
    }

    //
    // Copy the final packets not yet copied, in order, each at its place on the time line.
    //
    while (endPacket > m_LbNextPacket)
    {
        ULONG   packetIndex = (ULONG)(m_LbNextPacket % m_PacketsCount);
        PBYTE   packetBuffer = LoopbackPacketBuffer(packetIndex);
        ULONG   frames = m_LbPacketFrames;
        LONG64  revision = ReadAcquire64(&m_LbReissue[packetIndex]);

        if (lookAhead && !AtRun && m_LbNextPacket == current)
        {
            // Released too late to go a period early: the capture side may run short on it.
            ring->RenderLatePackets++;
        }

        if (eos != 0)
        {
            frames = (ULONG)MIN((ULONGLONG)frames, (eos - 1) - m_LbNextPacket * m_LbPacketFrames);
        }
        // At its deadline a notified stream's absent generation is lost. The physical slot may still contain an earlier
        // packet or an unsubmitted future payload. Publish silence and count the missing source packet exactly once.
        if (lookAhead && !LoopbackHasPacket(releases, m_LbNextPacket))
        {
            packetBuffer = NULL;
            ULONGLONG pause = PauseEndPlusOne();
            if (pause == 0 || ring->WritePos < pause - 1)
            {
                ring->RenderLostPackets++;
            }
        }
        LoopbackRingWrite(ring->Buffer, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, packetBuffer, frames);
        m_LbAppliedReissue[packetIndex] = revision;
        m_LbPublishedPlusOne[packetIndex] = lookAhead && packetBuffer != NULL ? m_LbNextPacket + 1 : 0;
        if (m_LbNextPacket < 4 && traceCount < RTL_NUMBER_OF(trace) && NF_LOOPBACK_TRACE_ENABLED())
        {
            // Bit 0: the first sample word is zero; bit 1: the last sample word is zero; bit 2: too short to sample.
            // Read only words that belong to this packet (a partial EOS is possible). Publish after releasing the ring lock.
            SIZE_T bytes = (SIZE_T)frames * m_LbFormat.BlockAlign;
            trace[traceCount].Packet = (ULONG)m_LbNextPacket;
            trace[traceCount].Slot = packetIndex;
            trace[traceCount].ZeroMask = LoopbackTraceZeroMask(packetBuffer, bytes);
            trace[traceCount].Nonzero = m_LbNextPacket == 1 ? LoopbackTraceNonzero(packetBuffer, bytes) : 2;
            trace[traceCount].FirstNonzero = trace[traceCount].LastNonzero = -2; // only the second post-start slot is probed
            if (m_LbNextPacket == 2)
            {
                LoopbackTraceEdges(packetBuffer, frames, m_LbFormat.BlockAlign, &trace[traceCount].FirstNonzero, &trace[traceCount].LastNonzero);
            }
            trace[traceCount].RingFrame = ring->WritePos;
            trace[traceCount].Released = releases >> 1;
            traceCount++;
        }
        ring->WritePos += frames;
        m_LbNextPacket++;
        copied = TRUE;
    }

    //
    // The lead (how early this stream's packets reach the ring), from a pass that copied. The pass at Run before the client has
    // released a packet copies nothing: its packet as the lead moved every capture stream running a packet back, and then on
    // again, skipping the start of this stream's audio. The first pass that copies says how this stream's packets are copied;
    // a capture stream takes a change at once (LoopbackCapturePass: a fall skips the difference once, counted).
    //
    if (copied)
    {
        ring->RenderLeadFrames = lookAhead ? 0 : m_LbPacketFrames;
        ring->RenderActive = TRUE;
    }

exit:
    if (completingPlusOne != 0)
    {
        // Close the slot to capture-side DMA reads before unlocking, including the interval between ProcessPacket
        // returning and the base timer incrementing m_CurrentPacket and notifying ACX. The ring copy remains readable.
        m_LbRetiredPlusOne = completingPlusOne;
    }
    KeReleaseSpinLock(&ring->Lock, oldIrql);
    if (traceCompletion)
    {
        DoTraceMessage(FLAG_STREAM, "render slot1 complete stream=%lu anchor=%I64u frames=%lu zeroMask=%lu anyNonzero=%lu publishedNonzero=%lu",
                       m_Serial, slot1.Anchor, slot1.Frames, slot1.ZeroMask, slot1.Nonzero, slot1.PublishedNonzero);
    }
    for (ULONG i = 0; i < traceCount; ++i)
    {
        DoTraceMessage(FLAG_STREAM, "render copy stream=%lu packet=%lu slot=%lu atRun=%lu releasedPlusOne=%I64d ringFrame=%I64u zeroMask=%lu anyNonzero=%lu firstNonzero=%ld lastNonzero=%ld",
                       m_Serial, trace[i].Packet, trace[i].Slot, (ULONG)AtRun, trace[i].Released, trace[i].RingFrame, trace[i].ZeroMask, trace[i].Nonzero, trace[i].FirstNonzero, trace[i].LastNonzero);
    }
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::LoopbackRenderStop(
    BOOLEAN ReleaseOwnership
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;
    LARGE_INTEGER   inactiveQpc = {};
    ULONGLONG       writePos = 0, anchor = 0, nextPacket = 0, nextBeforeDrain = 0, currentAtPause = 0;
    LONG64          released = 0;
    LONG64          frozenReleases = 0;

    if (ring == NULL)
    {
        return;
    }

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    if (ring->Owner == this)
    {
        if (!ReleaseOwnership && ring->RenderActive && PauseEndPlusOne() != 0)
        {
            if (NF_LOOPBACK_TRACE_ENABLED())
            {
                nextBeforeDrain = m_LbNextPacket;
                currentAtPause = m_LbPausedPacket;
                frozenReleases = m_LbPausedReleases;
            }
            // No capture DPC is required to wake during Stop. Make the same frozen, still-owned slot available for a later
            // reader before clearing RenderActive. The timer has stopped; the remaining frames then drain from the ring.
            RefreshReissuedPackets();
            CopyReleasedPackets(PauseEndPlusOne() - 1);
        }
        if (ring->RenderActive && NF_LOOPBACK_TRACE_ENABLED())
        {
            inactiveQpc = KeQueryPerformanceCounter(NULL);
            writePos = ring->WritePos;
            anchor = m_LbAnchor;
            nextPacket = m_LbNextPacket;
            ULONGLONG ignoredEnd;
            released = LoopbackDecodeEos(ReadNoFence64(&m_LbReleased), m_LbPacketFrames, &ignoredEnd) >> 1;
        }
        if (ring->RenderActive)
        {
            ring->PausedEndPlusOne = PauseEndPlusOne();
        }
        ring->RenderActive = FALSE;

        if (ReleaseOwnership)
        {
            // RenderFormat and the valid range stay, so the capture side can still read the tail; the next claim resets them.
            ring->EosEndPlusOne = AbsoluteEosEndPlusOne();
            ring->Owner = NULL;
        }
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
    if (inactiveQpc.QuadPart != 0)
    {
        DoTraceMessage(FLAG_STREAM, "render inactive stream=%lu qpc=%I64d writePos=%I64u anchor=%I64u nextPacket=%I64u releasedPlusOne=%I64d",
                       m_Serial, inactiveQpc.QuadPart, writePos, anchor, nextPacket, released);
        if (!ReleaseOwnership)
        {
            DoTraceMessage(FLAG_STREAM, "render pause snapshot stream=%lu currentAtPause=%I64u nextBeforeDrain=%I64u frozenReleaseState=%I64d",
                           m_Serial, currentAtPause, nextBeforeDrain, frozenReleases);
        }
    }
}


_Use_decl_annotations_
PAGED_CODE_SEG
CCaptureStreamEngine::CCaptureStreamEngine(
    _In_    ACXSTREAM       Stream,
    _In_    ACXDATAFORMAT   StreamFormat
)
    : CStreamEngine(Stream, StreamFormat, FALSE, NULL),
    m_EnableWaveCapture(0),
    m_LbLagFrames(0),
    m_LbAnchorLead(0),
    m_AppFormatOk(FALSE),
    m_AppEnd(0),
    m_AppEndAttach(0),
    m_AppOtherLead(0),
    m_AppOtherValid(FALSE),
    m_AppLiveAttach(0),
    m_AppNoSlot(FALSE),
    m_EvUnderrun(0),
    m_EvSilence(0),
    m_EvLate(0),
    m_EvLateEnd(0),
    m_EvAttach(0),
    m_EvLbUnderrun(0)
{
    PAGED_CODE();

    // noise-filter (TODO 19, spike check 5): this stream's timer counts as a capture stream's (NF_VMIC_IOCTL_STATS).
    m_StatsKind = NF_VMIC_KIND_CAPTURE;

    m_LastPacketStart.QuadPart = 0;

    RtlInitUnicodeString(&m_HostCaptureFileName, NULL);
    RtlInitUnicodeString(&m_LoopbackCaptureFileName, NULL);
}

_Use_decl_annotations_
PAGED_CODE_SEG
CCaptureStreamEngine::~CCaptureStreamEngine()
{
    PAGED_CODE();

    // noise-filter (TODO 19, spike check 4): give the app ring's reader slot up if Pause and ReleaseHardware did not.
    AppRingReaderStop();

    RtlFreeUnicodeString(&m_HostCaptureFileName);
    RtlFreeUnicodeString(&m_LoopbackCaptureFileName);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::PrepareHardware()
{
    PAGED_CODE();

    //
    // noise-filter (TODO 19): the capture data comes from the device's loopback ring (LoopbackCapturePass), so the sample's
    // tone generator and WAV reader are no longer set up (ReadRegistrySettings, m_WaveReader.Init, m_ToneGenerator.Init).
    // m_EnableWaveCapture stays 0, so ReleaseHardware waits for no WAV work items.
    //
    return CStreamEngine::PrepareHardware();
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::ReleaseHardware()
{
    NTSTATUS status;

    PAGED_CODE();

    if (m_EnableWaveCapture)
    {
        m_WaveReader.WaitAllWorkItems();
    }

    status = CStreamEngine::ReleaseHardware();

    // noise-filter (TODO 19, spike check 4): the timer is gone (the base deleted it and flushed queued DPCs): give the app
    // ring's reader slot up.
    AppRingReaderStop();

    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::GetCapturePacket(
    _Out_   ULONG* LastCapturePacket,
    _Out_   ULONGLONG* QPCPacketStart,
    _Out_   BOOLEAN* MoreData
)
{
    NTSTATUS status = STATUS_SUCCESS;
    ULONGLONG currentPacket;
    LONGLONG qpcPacketStart;

    PAGED_CODE();

    currentPacket = (ULONGLONG)InterlockedCompareExchange64((LONG64*)&m_CurrentPacket, -1, -1);
    qpcPacketStart = InterlockedCompareExchange64(&m_LastPacketStart.QuadPart, -1, -1);

    // ACX's packet numbers are 32-bit: the count's low 32 bits (m_CurrentPacket).
    *LastCapturePacket = (ULONG)(currentPacket - 1);
    *QPCPacketStart = (ULONGLONG)qpcPacketStart;
    *MoreData = FALSE;

    // noise-filter (TODO 19): the capture-timestamp trace (NF_VMIC_CAPTURE_TRACE), the stream's cable's.
    if (m_Cable != NULL)
    {
        NfVmicTraceCaptureQuery(m_Cable, (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart, (ULONG)(currentPacket - 1),
                                (ULONGLONG)qpcPacketStart);
    }

    return status;
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::ProcessPacket()
{
    ULONGLONG currentPacket;
    ULONG packetIndex;
    PBYTE packetBuffer;

    currentPacket = (ULONGLONG)InterlockedCompareExchange64((LONG64*)&m_CurrentPacket, -1, -1);

    packetIndex = (ULONG)(currentPacket % m_PacketsCount);
    packetBuffer = (PBYTE)m_Packets[packetIndex];

    // Packet 0 starts at an offset if the size isn't a multiple of page_size
    if (packetIndex == 0)
    {
        packetBuffer += m_FirstPacketOffset;
    }

    //
    // noise-filter (TODO 19): an attached app ring (spike check 4: its cable's) serves the pass first; without one, the capture
    // half of the loopback pair (spike check 7). Both in place of the sample's tone generator and WAV reader
    // (m_ToneGenerator.GenerateSine, m_WaveReader.ReadWaveData).
    //
    if (m_Cable == NULL || !AppRingCapturePass(currentPacket, packetBuffer, m_PacketSize))
    {
        LoopbackCapturePass(currentPacket, packetBuffer, m_PacketSize);
    }
    else
    {
        // The app ring served the pass: its loopback underrun run, if any, has ended (only this stream's passes touch it).
        m_EvLbUnderrun = 0;
    }
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::Run()
{
    PAGED_CODE();

    if (m_CurrentState == AcxStreamStatePause)
    {
        // The timer is stopped in Pause, so no pass reads these while they change.
        LoopbackSetFormat();

        // The lag behind the render time line: LOOPBACK_LAG_HNS rounded up to whole frames, plus one for rounding.
        m_LbLagFrames = m_LbFormat.Valid ?
            (ULONG)(((ULONGLONG)LOOPBACK_LAG_HNS * m_LbFormat.SampleRate + HNS_PER_SEC - 1) / HNS_PER_SEC) + 1 : 0;

        // Anchor again at the first pass: Run gives this stream a new time line (m_StartTime).
        m_LbAnchorValid = FALSE;

        //
        // noise-filter (TODO 19, spike check 4): the app ring holds 48 kHz float32 stereo frames and is copied raw, so only into
        // packets of exactly that format (nf_vmic.h). m_LbFormat.Valid adds whole frames in a packet and the byte clock.
        //
        m_AppFormatOk = (m_LbFormat.Valid && AppRingFormatIs(&m_LbFormat)) ? TRUE : FALSE;
        m_AppOtherValid = FALSE;

        // It may hold the reader slot again (AppRingReaderStop barred it). No pass reads this while the timer is stopped.
        m_AppNoSlot = FALSE;

        // A new time line: its runs in the event logs end (the next pass with silence starts one).
        m_EvUnderrun = 0;
        m_EvSilence = 0;
        m_EvLate = 0;
        m_EvLateEnd = 0;
        m_EvAttach = 0;
        m_EvLbUnderrun = 0;
    }

    return CStreamEngine::Run();
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::LoopbackCapturePass(
    ULONGLONG Packet,
    PBYTE   PacketBuffer,
    ULONG   PacketSize
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;
    ULONG           blockAlign = m_LbFormat.BlockAlign;
    ULONG           lead;
    ULONGLONG       anchor;
    ULONGLONG       readStart;
    ULONGLONG       readEnd;
    ULONGLONG       dueEnd;
    ULONGLONG       eosEnd;
    ULONGLONG       validLo;
    ULONGLONG       validHi;
    ULONGLONG       copyLo;
    ULONGLONG       copyHi;

    if (ring == NULL)
    {
        m_EvLbUnderrun = 0;
        RtlZeroMemory(PacketBuffer, PacketSize);
        return;
    }

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    if (ring->Buffer == NULL || !ring->RenderFormat.Valid)
    {
        // No render stream has run since the device started (or the device is going away).
        ring->IdlePackets++;
        goto zero;
    }

    if (!LoopbackFormatEqual(&m_LbFormat, &ring->RenderFormat))
    {
        // Raw bytes only between identical formats; no conversion in the kernel. Creation refuses a format the other circuit
        // has open (LoopbackStreamOpen), so this zero-fill is the fallback for what that leaves: the tail of a render run whose
        // stream has closed (RenderFormat is kept) read in another format before a new render stream claims the ring, or a
        // stream that failed LoopbackSetFormat's checks.
        ring->MismatchPackets++;
        goto zero;
    }

    //
    // Anchor the read position the lag (plus the render side's lead) behind this stream's own place on the ring's time line,
    // again whenever that moves (Run, a late timer's glitch adjustment) or the render lead changes. Between anchors it
    // advances exactly one packet a pass. An underrun or an overrun does not move it: the time line is right, the audio was
    // just not (or no longer) there.
    //
    // The render lead changes only as a render stream starts copying (its packet, then 0 once its client releases packets
    // early: LoopbackRenderPass) and when another render stream claims the ring, which starts a new run there. So a longer
    // lead moves the read back only onto a new run: silence before its start, and after it the new owner's audio, which this
    // stream hasn't had (the ring held the last owner's there, or nothing), so no frame of the ring comes out twice. A shorter
    // one is taken at once too: the read moves on by the difference and the stream skips it, once, counted (LeadSkips, and
    // the ring's audio between its reads at the two leads in LeadSkippedFrames). Within a run that is the owner's audio
    // where its packets begin to go a period early; at a new run (another owner, or the owner's restart), silence before the
    // run's start or its first frames before the shorter lead's read. A new run's lead is the owner's rule (2026-11-23); a
    // fall within a run follows the owner's app-ring decision of 2026-11-23 (lower latency over a one-time skip): from
    // 5de56b9 to e801320 a stream kept the longer lead there while the ring held audio it would skip, a packet more latency
    // until that audio ended or it next ran. What the ring doesn't hold yet between the two reads isn't counted: there the
    // shorter lead's read underruns too (a client that released a packet too late to go early, RenderLatePackets).
    //

    anchor = LoopbackAnchor();
    lead = ring->RenderLeadFrames;
    readStart = anchor - m_LbLagFrames - lead + Packet * m_LbPacketFrames;
    readEnd = readStart + m_LbPacketFrames;
    if (ring->RenderActive && ring->Owner != NULL)
    {
        CRenderStreamEngine* owner = static_cast<CRenderStreamEngine*>(ring->Owner);
        owner->RefreshReissuedPackets();
        if (readEnd > ring->WritePos)
        {
            owner->CopyReleasedPackets(readEnd);
        }
    }

    // The audio the ring still holds of the current render run.
    validHi = ring->WritePos;
    validLo = ring->ValidFrom;
    if (validHi - validLo > m_LbRingFrames)
    {
        validLo = validHi - m_LbRingFrames;
    }
    eosEnd = ring->Owner != NULL ? static_cast<CRenderStreamEngine*>(ring->Owner)->AbsoluteEosEndPlusOne() : ring->EosEndPlusOne;
    if (eosEnd != 0)
    {
        validHi = MIN(validHi, eosEnd - 1);
    }

    if (m_LbAnchorValid && lead < m_LbAnchorLead)
    {
        // This pass's read at the old lead, and at the new one: what the ring holds between them never reaches this stream.
        ULONGLONG   oldStart = anchor - m_LbLagFrames - m_LbAnchorLead + (ULONGLONG)Packet * m_LbPacketFrames;
        ULONGLONG   newStart = oldStart + (m_LbAnchorLead - lead);
        ULONGLONG   skipLo = MAX(oldStart, validLo);
        ULONGLONG   skipHi = MIN(newStart, validHi);
        NF_VMIC_EVENT event;

        ring->LeadSkips++;
        if (skipHi > skipLo)
        {
            ring->LeadSkippedFrames += skipHi - skipLo;
        }

        // The skip log (nf_vmic.h): the span between the two reads, and the render audio the ring held there.
        RtlZeroMemory(&event, sizeof(event));
        event.Kind = NF_VMIC_EVENT_LOOPBACK_LEAD_SKIP;
        event.Stream = m_Serial;
        event.PacketFrames = m_LbPacketFrames;
        event.Frames = (skipHi > skipLo) ? (UINT32)MIN(skipHi - skipLo, (ULONGLONG)MAXULONG) : 0;
        event.SpanFrom = oldStart;
        event.SpanFrames = (UINT32)MIN(newStart - oldStart, (ULONGLONG)MAXULONG);
        if (skipHi > skipLo)
        {
            event.Flags = NF_VMIC_EVENTF_RING;
            event.RingFrom = skipLo;
            event.RingTo = skipHi;
        }
        (void)NfVmicLogEvent(&ring->SkipLog, &event);
    }
    if (!m_LbAnchorValid || anchor != m_LbAnchor || lead != m_LbAnchorLead)
    {
        m_LbAnchor = anchor;
        m_LbAnchorLead = lead;
        m_LbAnchorValid = TRUE;
        ring->CaptureAnchors++;
    }

    copyLo = MAX(readStart, validLo);
    copyHi = MIN(readEnd, validHi);
    if (copyLo >= copyHi)
    {
        copyLo = readEnd;
        copyHi = readEnd;
    }

    if (copyLo > readStart)
    {
        RtlZeroMemory(PacketBuffer, (SIZE_T)(copyLo - readStart) * blockAlign);
    }
    if (copyHi > copyLo)
    {
        LoopbackRingRead(ring->Buffer, m_LbRingFrames, blockAlign, copyLo,
                         PacketBuffer + (SIZE_T)(copyLo - readStart) * blockAlign, copyHi - copyLo);
    }
    if (readEnd > copyHi)
    {
        RtlZeroMemory(PacketBuffer + (SIZE_T)(copyHi - readStart) * blockAlign, (SIZE_T)(readEnd - copyHi) * blockAlign);
    }

    // An explicit EOS is an exclusive end. Charge every unavailable frame due before it, including in a partial last packet.
    // Bytes past a late EOS may already be in the ring; the copy above also clips them. Without EOS the whole read is due.
    dueEnd = eosEnd != 0 ? MIN(readEnd, eosEnd - 1) : readEnd;
    if (dueEnd > MAX(readStart, validHi))
    {
        // A capture callback can run while Pause waits for the render timer, or drain an earlier packet after it stops.
        // Bound demand on the two streams' common timeline. Already-published look-ahead audio still drains in full.
        ULONGLONG pauseEnd = ring->RenderActive && ring->Owner != NULL ?
            static_cast<CRenderStreamEngine*>(ring->Owner)->PauseEndPlusOne() : ring->PausedEndPlusOne;
        if (pauseEnd != 0)
        {
            dueEnd = MIN(dueEnd, pauseEnd - 1);
        }
    }
    if (dueEnd > MAX(readStart, validHi) && (ring->RenderActive || ring->PausedEndPlusOne != 0))
    {
        ULONGLONG under = dueEnd - MAX(readStart, validHi);

        ring->UnderrunPackets++;
        ring->UnderrunFrames += under;

        // The underrun log: one entry a run of underrunning passes, extended while it lasts.
        if (m_EvLbUnderrun == 0 ||
            !NfVmicLogExtend(&ring->UnderrunLog, m_EvLbUnderrun, (ULONG)MIN(under, (ULONGLONG)MAXULONG), dueEnd, 0))
        {
            NF_VMIC_EVENT event;

            RtlZeroMemory(&event, sizeof(event));
            event.Kind = NF_VMIC_EVENT_LOOPBACK_UNDERRUN;
            event.Stream = m_Serial;
            event.PacketFrames = m_LbPacketFrames;
            event.Frames = (UINT32)MIN(under, (ULONGLONG)MAXULONG);
            event.SpanFrom = dueEnd - under;
            event.SpanFrames = event.Frames;
            m_EvLbUnderrun = NfVmicLogEvent(&ring->UnderrunLog, &event);
        }
    }
    else
    {
        if (readEnd > validHi)
        {
            ring->IdlePackets++;
        }
        m_EvLbUnderrun = 0;
    }
    if (readStart < validLo && validLo > ring->ValidFrom)
    {
        ring->OverrunPackets++;
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
    return;

zero:
    // Idle or in another format: no render audio this pass, so its underrun run (render audio due but not there) has ended.
    m_EvLbUnderrun = 0;
    KeReleaseSpinLock(&ring->Lock, oldIrql);
    RtlZeroMemory(PacketBuffer, PacketSize);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::Pause()
{
    PAGED_CODE();

    //
    // noise-filter (TODO 19, spike check 4): give the app ring's reader slot up first, while the timer still runs; this stream
    // holds it again only after its next Run. Given up after the base Pause, the slot stayed held while the timer was stopped
    // (WdfTimerStop waits for a running callback and the queued DPCs) and nothing wrote the reader's copy: another stream read
    // past the copy's end, got silence there, and never got those frames of the ring once the next reader wrote them (the live
    // three-stream runs, 2026-11-23: 55 to 111 frames lost, the slot held 2.4 to 6.3 ms after the reader's next pass was due,
    // its timer lateness under 1 ms). A pass of this stream before its timer stops plays the copy as another stream's. The next
    // stream to run, another one or this one after its Run, reads on from the ring's read_frame.
    //
    AppRingReaderStop();

    return CStreamEngine::Pause();
}

///////////////////////////////////////////////////////////////////////////////
//
// noise-filter (TODO 19, spike check 4): the app-fed capture ring, one per cable (m_Cable->Ring). Shared\nf_vmic.h has the rules,
// Shared\Cable.h the state.
//
// An attached app ring serves every capture pass ahead of the loopback. The read is sample-continuous: read_frame advances by
// the frames copied, a whole packet when the ring holds it, so every frame the app writes comes out once and in order, and an
// underrun inserts silence without dropping anything. write_frame is the one value read back from the app's memory (every
// pass) and is never trusted: a fill below 0 or above the capacity re-syncs, counted. Raw copies only into 48 kHz float32
// stereo packets. All under the ring's lock, held for one packet's copy (two for the reader); nothing allocates.
//
// One stream reads the app's ring (the reader); the audio engine runs one device stream per processing mode, so others can
// be running too. The reader also writes each packet it delivers into the cable's copy (NF_APP_RING.History) at its frames on the
// stream engine's QPC time line, and every other stream reads that copy at its own place on the time line, a lag behind: the
// reader writes a packet at the pass that ends it, so the copy holds the time line up to that pass, a packet and a late timer
// behind now at worst. The lag is the reader's packet (NF_APP_RING.HistoryLeadFrames) plus LOOPBACK_LAG_HNS and a frame
// (m_LbLagFrames), as the loopback's capture side reads a render stream that copies its packets as they complete. Every time
// line is the same QPC clock at the same nominal rate, so nothing drifts and no fill control is needed. The app's protocol
// doesn't change: one ReadFrame, paced by the reader.
//
// No stream has the ring's audio twice. The copy holds the ring's frames in the order the readers delivered them, none left
// out at a handoff: a new reader takes what the copy already holds for its packet's time from the copy, and reads the ring on
// from the copy's end. And no stream has the same frame of the copy twice: each keeps its place in it (m_AppEnd), and gets silence
// rather than anything before it (AppRingPlayCopy). So a stream playing the copy skips the ring's audio only where its own time
// line jumps (Run, a glitch adjustment), where the copy didn't hold its frames in time (silence, counted as underruns), once as
// it takes the reader slot (counted, HandoffSkippedFrames), and once as it plays on behind a new reader with a shorter packet
// (counted, SecondarySkippedFrames: AppRingOtherPass). (Astra's review, 2026-11-23: a stream that stayed another one through a
// handoff to a reader with a longer packet moved back by the difference and played it again, 320 frames from 160 to 480; and a
// new reader whose packet started before the last one's end left that much of the ring out of the copy.)
//

//
// Part of the reader's packet into the copy: Frames frames from Source (NULL: silence) at time-line frame Frame. Ring lock
// held, ring published, called by the reader in the ring's format, with the packet's parts in order. Its own consecutive
// packets append. A time line that moved later by a little (a reader handoff, a late timer's glitch adjustment) leaves a gap,
// zeroed and recorded (HistoryGaps); by more than LOOPBACK_MAX_GAP_HNS, a new run. The reader writes from the copy's end on
// (AppRingCapturePass takes what the copy already holds for its packet from the copy); should a part start before it, only what
// the copy doesn't hold yet is written.
//
// The skip log's records (AppRing.h): the audio written is mapped to the ring's frames (Source's first is RingFrame; unused for
// silence), and joins the ring spans of the claims it fills; the claims the copy's end then passes are settled.
//
static
VOID
AppRingHistoryWrite(
    _In_                                                PNF_APP_RING    Ring,
    _In_                                                ULONGLONG       Frame,
    _In_reads_bytes_opt_(Frames * NF_VMIC_FRAME_BYTES)  const BYTE*     Source,
    _In_                                                ULONGLONG       RingFrame,
    _In_                                                ULONG           Frames
)
{
    ULONGLONG   maxGap = LoopbackHnsToFrames(LOOPBACK_MAX_GAP_HNS, NF_VMIC_SAMPLE_RATE);
    ULONGLONG   end = Frame + Frames;
    ULONGLONG   from;

    if (!Ring->HistoryValid || (Frame > Ring->HistoryEnd && Frame - Ring->HistoryEnd > maxGap))
    {
        Ring->HistoryFrom = Frame;
        Ring->HistoryEnd = Frame;
        Ring->HistoryValid = TRUE;
        Ring->HistoryGapNext = 0;
        RtlZeroMemory(Ring->HistoryGaps, sizeof(Ring->HistoryGaps));
        NfVmicAppRingMapReset(Ring);
    }
    if (Frame > Ring->HistoryEnd)
    {
        // Recorded (AppRing.h), so the other streams count this silence as an underrun: no reader delivered it.
        PNF_APP_HISTORY_GAP gap = &Ring->HistoryGaps[Ring->HistoryGapNext % NF_APP_HISTORY_GAPS];

        LoopbackRingWrite(Ring->History, NF_APP_HISTORY_FRAMES, NF_VMIC_FRAME_BYTES, Ring->HistoryEnd, NULL, Frame - Ring->HistoryEnd);
        gap->From = Ring->HistoryEnd;
        gap->End = Frame;
        Ring->HistoryGapNext++;
        Ring->HistoryEnd = Frame;
    }

    if (end > Ring->HistoryEnd)
    {
        // Frame <= HistoryEnd here. Of a packet longer than the copy, only its last NF_APP_HISTORY_FRAMES can be read back.
        from = MAX(Frame, Ring->HistoryEnd);
        if (end - from > NF_APP_HISTORY_FRAMES)
        {
            from = end - NF_APP_HISTORY_FRAMES;
        }
        LoopbackRingWrite(Ring->History, NF_APP_HISTORY_FRAMES, NF_VMIC_FRAME_BYTES, from,
                          (Source != NULL) ? Source + (SIZE_T)(from - Frame) * NF_VMIC_FRAME_BYTES : NULL, end - from);
        if (Source != NULL)
        {
            NfVmicAppRingAudioWritten(Ring, from, end, RingFrame + (from - Frame));
        }
        Ring->HistoryEnd = end;
    }

    NfVmicAppRingSettleClaims(Ring, FALSE);
}

// The start of what the copy holds: its current run's start, or NF_APP_HISTORY_FRAMES before its end. HistoryValid, ring lock held.
static
ULONGLONG
AppRingHistoryValidFrom(
    _In_ PNF_APP_RING   Ring
)
{
    return (Ring->HistoryEnd - Ring->HistoryFrom > NF_APP_HISTORY_FRAMES) ? Ring->HistoryEnd - NF_APP_HISTORY_FRAMES : Ring->HistoryFrom;
}

//
// The frames of [Lo, Hi) in the copy's recorded gaps (silence no reader delivered). The gaps are disjoint (HistoryEnd only grows
// within a run, and a new run clears them), so their overlaps add up to no more than Hi - Lo; an empty span meets none. Ring
// lock held.
//
static
ULONGLONG
AppRingGapFrames(
    _In_ PNF_APP_RING   Ring,
    _In_ ULONGLONG      Lo,
    _In_ ULONGLONG      Hi
)
{
    ULONGLONG   frames = 0;
    ULONG       i;

    for (i = 0; i < NF_APP_HISTORY_GAPS; i++)
    {
        ULONGLONG   lo = MAX(Lo, Ring->HistoryGaps[i].From);
        ULONGLONG   hi = MIN(Hi, Ring->HistoryGaps[i].End);

        if (hi > lo)
        {
            frames += hi - lo;
        }
    }

    return frames;
}

//
// The copy's frames in [Lo, Hi) as a skip counts them (HandoffSkippedFrames, SecondarySkippedFrames): from its valid start at the
// earliest (nothing before a new run or past the copy's length was audio a stream would have had) to its end at the latest,
// less its recorded gaps (silence no reader delivered); the last reader's underrun silence there counts, as the copy holds it.
// Ring lock held.
//
static
ULONGLONG
AppRingSkipFrames(
    _In_ PNF_APP_RING   Ring,
    _In_ ULONGLONG      Lo,
    _In_ ULONGLONG      Hi
)
{
    if (!Ring->HistoryValid)
    {
        return 0;
    }
    Lo = MAX(Lo, AppRingHistoryValidFrom(Ring));
    Hi = MIN(Hi, Ring->HistoryEnd);
    return (Hi > Lo) ? Hi - Lo - AppRingGapFrames(Ring, Lo, Hi) : 0;
}

//
// Frames frames of the copy from time-line frame From into Dest: what the copy holds of them, silence for the rest (not yet
// written, or overwritten). Returns how many are the ring's audio: those the copy holds, less its recorded gaps. Ring lock held.
//
static
ULONGLONG
AppRingHistoryRead(
    _In_                                                PNF_APP_RING    Ring,
    _In_                                                ULONGLONG       From,
    _In_                                                ULONG           Frames,
    _Out_writes_bytes_(Frames * NF_VMIC_FRAME_BYTES)    PBYTE           Dest
)
{
    ULONGLONG   end = From + Frames;
    ULONGLONG   validLo = 0;
    ULONGLONG   validHi = 0;
    ULONGLONG   copyLo;
    ULONGLONG   copyHi;

    if (Ring->HistoryValid)
    {
        validLo = AppRingHistoryValidFrom(Ring);
        validHi = Ring->HistoryEnd;
    }

    copyLo = MAX(From, validLo);
    copyHi = MIN(end, validHi);
    if (copyLo >= copyHi)
    {
        copyLo = end;
        copyHi = end;
    }

    if (copyLo > From)
    {
        RtlZeroMemory(Dest, (SIZE_T)(copyLo - From) * NF_VMIC_FRAME_BYTES);
    }
    if (copyHi > copyLo)
    {
        LoopbackRingRead(Ring->History, NF_APP_HISTORY_FRAMES, NF_VMIC_FRAME_BYTES, copyLo,
                         Dest + (SIZE_T)(copyLo - From) * NF_VMIC_FRAME_BYTES, copyHi - copyLo);
    }
    if (end > copyHi)
    {
        RtlZeroMemory(Dest + (SIZE_T)(copyHi - From) * NF_VMIC_FRAME_BYTES, (SIZE_T)(end - copyHi) * NF_VMIC_FRAME_BYTES);
    }

    return (copyHi - copyLo) - AppRingGapFrames(Ring, copyLo, copyHi);
}

//
// This attachment's counters and the reader's state into the header's driver line, and the state and packet kept for STATS
// (ReaderState, ReaderPacketFrames), so the two never disagree. Ring lock held, ring published, called by the reader.
//
static
VOID
AppRingStoreCounters(
    _In_ PNF_APP_RING   Ring,
    _In_ ULONG          State,
    _In_ ULONG          PacketFrames,
    _In_ ULONGLONG      Qpc
)
{
    PNF_VMIC_RING_HEADER header = Ring->Header;

    Ring->ReaderState = State;
    Ring->ReaderPacketFrames = PacketFrames;

    NfVmicStore64(&header->FramesCopied, Ring->AttachFramesCopied);
    NfVmicStore64(&header->UnderrunFrames, Ring->AttachUnderrunFrames);
    NfVmicStore32(&header->Resyncs, Ring->AttachResyncs);
    NfVmicStore32(&header->MismatchPackets, Ring->AttachMismatchPackets);
    NfVmicStore32(&header->State, State);
    NfVmicStore32(&header->PacketFrames, PacketFrames);
    NfVmicStore64(&header->Passes, Ring->AttachPasses);
    NfVmicStore64(&header->LastPassQpc, Qpc);
}

//
// The app ring's event logs (nf_vmic.h, "The event logs"): its cable's (NF_APP_RING.Loopback, in the context every stream that
// reads the app ring belongs to), which STATS copies. NULL with no such device (then nothing is logged). Ring lock held.
//
static
PNF_EVENT_LOG
AppRingSkipLog(
    _In_ PNF_APP_RING Ring
)
{
    return (Ring->Loopback != NULL) ? &Ring->Loopback->SkipLog : NULL;
}

static
PNF_EVENT_LOG
AppRingUnderrunLog(
    _In_ PNF_APP_RING Ring
)
{
    return (Ring->Loopback != NULL) ? &Ring->Loopback->UnderrunLog : NULL;
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::AppRingEventInit(
    PNF_VMIC_EVENT  Event,
    USHORT          Kind,
    ULONGLONG       Frames,
    ULONGLONG       SpanFrom,
    ULONGLONG       SpanTo
)
{
    PNF_APP_RING ring = &m_Cable->Ring;

    RtlZeroMemory(Event, sizeof(*Event));
    Event->Kind = Kind;
    Event->Flags = (USHORT)(((ring->Reader == this) ? NF_VMIC_EVENTF_READER : 0) | (m_AppNoSlot ? NF_VMIC_EVENTF_PAUSING : 0));
    Event->Stream = m_Serial;
    Event->PacketFrames = m_LbPacketFrames;
    Event->Frames = (UINT32)MIN(Frames, (ULONGLONG)MAXULONG);
    Event->Attach = (UINT32)ring->Stats.Attaches;
    Event->SpanFrom = SpanFrom;
    Event->SpanFrames = (SpanTo > SpanFrom) ? (UINT32)MIN(SpanTo - SpanFrom, (ULONGLONG)MAXULONG) : 0;
}

_Use_decl_annotations_
ULONGLONG
CCaptureStreamEngine::AppRingLogSkip(
    USHORT      Kind,
    ULONGLONG   Frames,
    ULONGLONG   Lo,
    ULONGLONG   Hi,
    BOOLEAN     Claim
)
{
    PNF_APP_RING    ring = &m_Cable->Ring;
    NF_VMIC_EVENT   event;
    ULONGLONG       sequence;

    AppRingEventInit(&event, Kind, Frames, Lo, Hi);

    // The ring's frames the copy holds as audio in the span: what the stream never gets of it.
    if (ring->HistoryValid)
    {
        ULONGLONG   lo = MAX(Lo, AppRingHistoryValidFrom(ring));
        ULONGLONG   hi = MIN(Hi, ring->HistoryEnd);
        BOOLEAN     whole = TRUE;

        if (hi > lo && NfVmicAppRingSpan(ring, lo, hi, &event.RingFrom, &event.RingTo, &whole))
        {
            event.Flags |= NF_VMIC_EVENTF_RING;
        }
        if (!whole)
        {
            event.Flags |= NF_VMIC_EVENTF_UNTRACKED;
        }
    }
    sequence = NfVmicLogEvent(AppRingSkipLog(ring), &event);

    // What the copy doesn't hold yet (past its end, or no copy yet): the reader's writes there fill the ring span in.
    if (Claim)
    {
        ULONGLONG claimLo = ring->HistoryValid ? MAX(Lo, ring->HistoryEnd) : Lo;

        NfVmicAppRingClaim(ring, claimLo, Hi, sequence);
    }

    return sequence;
}

_Use_decl_annotations_
BOOLEAN
CCaptureStreamEngine::AppRingCapturePass(
    ULONGLONG Packet,
    PBYTE   PacketBuffer,
    ULONG   PacketSize
)
{
    PNF_APP_RING    ring = &m_Cable->Ring;
    KIRQL           oldIrql;
    ULONG           packetFrames;
    ULONG           capacity;
    ULONG           held;
    ULONG           want;
    ULONG           copyFrames;
    ULONG           index;
    ULONG           first;
    PBYTE           dest;
    ULONGLONG       writeFrame;
    ULONGLONG       readFrame;
    ULONGLONG       fill;
    ULONGLONG       frame;
    ULONGLONG       at;
    ULONGLONG       qpc;

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    // History: never NULL while a ring is published (Cable.h); checked anyway, so no pass can write a copy that isn't there.
    if (ring->Header == NULL || ring->History == NULL)
    {
        // No app ring attached: the loopback serves this pass. A place in an earlier attachment's copy means nothing now, and
        // its open runs in the event logs have ended (the next attachment's silence starts its own).
        m_AppOtherValid = FALSE;
        m_EvUnderrun = 0;
        m_EvSilence = 0;
        m_EvLate = 0;
        KeReleaseSpinLock(&ring->Lock, oldIrql);
        return FALSE;
    }

    ring->Stats.Passes++;

    // Its open runs in the event logs are of one attachment: an ATTACH since its latest pass (a detach and an attach between
    // two of its passes) ends them, so no entry spans two attachments or keeps an earlier one's Attach.
    if (m_EvAttach != ring->Stats.Attaches)
    {
        m_EvUnderrun = 0;
        m_EvSilence = 0;
        m_EvLate = 0;
        m_EvAttach = ring->Stats.Attaches;
    }

    if (ring->LiveFeed && m_AppFormatOk && !m_AppNoSlot && m_AppLiveAttach != ring->Stats.Attaches)
    {
        m_AppLiveAttach = ring->Stats.Attaches;
        if (ring->LiveStreams++ == 0)
        {
            // No recorder could consume the old queue. A new recording must never replay an earlier mic session.
            // New streams joining or taking over from a running secondary leave both the queue and the history alone.
            ULONGLONG discardTo = NfVmicLoadAcquire64(&ring->Header->WriteFrame);

            if (discardTo != ring->ReadFrame)
            {
                // The skip log: the ring's frames discarded (untrusted: WriteFrame is the app's).
                NF_VMIC_EVENT event;

                AppRingEventInit(&event, NF_VMIC_EVENT_LIVE_DISCARD, (discardTo > ring->ReadFrame) ? discardTo - ring->ReadFrame : 0,
                                 0, 0);
                event.Flags |= NF_VMIC_EVENTF_RING | ((discardTo < ring->ReadFrame) ? NF_VMIC_EVENTF_BACKWARD : 0);
                event.RingFrom = ring->ReadFrame;
                event.RingTo = discardTo;
                (void)NfVmicLogEvent(AppRingSkipLog(ring), &event);
            }
            ring->ReadFrame = discardTo;
            ring->HistoryValid = FALSE;
        }
    }

    if (ring->Reader == NULL && !m_AppNoSlot)
    {
        //
        // The first stream to run after ATTACH (or after the last reader began to pause) holds the reader slot, whatever its
        // format. A stream that gave the slot up (m_AppNoSlot: it is pausing) plays the copy until its timer stops.
        //
        ring->Reader = this;
        if (ring->AttachPasses != 0 && m_AppOtherValid && m_AppEndAttach == ring->Stats.Attaches)
        {
            //
            // A handoff: this stream played the copy as another stream during this run and now takes the slot another stream
            // held (the owner, 2026-11-20: a one-time skip, a gapless handoff later). Its packet below plays the copy for the
            // time the copy already holds and the ring from there on, so what lies between its place in the copy (m_AppEnd) and
            // this packet's start never reaches it: its lag at most. Counted from the copy's valid start at the earliest (as
            // AppRingHistoryRead reads it: nothing before a new run or past the copy's length was audio this stream would have
            // had) to the copy's end at the latest, less the copy's recorded gaps there (silence no reader delivered). The last
            // reader's underrun silence in that span counts too: the copy holds it as delivered. m_AppOtherValid says this run's
            // passes found the ring's format (AppRingOtherPass), so m_LbPacketFrames is this stream's packet.
            //
            // Each handoff is in the skip log too, with the ring's frames the copy held as audio there. What the copy doesn't
            // hold up to this packet becomes a gap in it at this pass (no audio there, nothing to claim).
            //
            ULONGLONG   start = m_AppEnd;
            ULONGLONG   end = LoopbackAnchor() + (ULONGLONG)Packet * m_LbPacketFrames;
            ULONGLONG   skipped = AppRingSkipFrames(ring, start, end);

            ring->ReaderHandoffs++;
            ring->HandoffSkippedFrames += skipped;
            (void)AppRingLogSkip(NF_VMIC_EVENT_HANDOFF_SKIP, skipped, start, end, FALSE);
        }
        m_AppOtherValid = FALSE;
    }
    else if (ring->Reader != this)
    {
        // One reader at a time: another capture stream plays the reader's copy, and the ring's position stays the reader's.
        ring->Stats.NotReaderPackets++;
        AppRingOtherPass(Packet, PacketBuffer, PacketSize);
        KeReleaseSpinLock(&ring->Lock, oldIrql);
        return TRUE;
    }

    // Every pass of the slot holder, FORMAT_MISMATCH ones too (the header's Passes and LastPassQpc); FramesCopied measures reading.
    ring->AttachPasses++;
    qpc = (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart;

    //
    // m_AppFormatOk includes the loopback's check that the packet holds whole frames (Run, LoopbackSetFormat), so
    // m_LbPacketFrames * NF_VMIC_FRAME_BYTES == PacketSize; it is checked again here because the copy below is bounded by
    // m_LbPacketFrames and must stay inside PacketBuffer whatever changed since Run.
    //
    if (!m_AppFormatOk || m_LbPacketFrames == 0 || (ULONGLONG)m_LbPacketFrames * NF_VMIC_FRAME_BYTES != PacketSize)
    {
        //
        // Raw copies only into 48 kHz float32 stereo packets: silence, counted, and read_frame stays where it is. The slot stays
        // held, shown as FORMAT_MISMATCH (not READING), with this stream's packet in its own format's frames (0 when
        // LoopbackSetFormat found the format unusable).
        //
        ring->Stats.MismatchPackets++;
        ring->AttachMismatchPackets++;
        // Not an underrun (MismatchPackets counts it): its underrun run as the reader, if any, has ended.
        m_EvUnderrun = 0;
        AppRingStoreCounters(ring, NF_VMIC_STATE_ATTACHED | NF_VMIC_STATE_FORMAT_MISMATCH, m_LbPacketFrames, qpc);
        KeReleaseSpinLock(&ring->Lock, oldIrql);
        RtlZeroMemory(PacketBuffer, PacketSize);
        return TRUE;
    }

    packetFrames = m_LbPacketFrames;
    capacity = ring->CapacityFrames;
    frame = LoopbackAnchor() + (ULONGLONG)Packet * packetFrames;

    //
    // What the copy already holds for this packet's time comes from the copy, and the ring is read on from the copy's end. Only a
    // new reader's packet can start before that end (one reader's packets follow each other, and its time line moves only
    // later): at a handoff, or when a stream that ran before the last reader stopped takes the slot. Read from the ring there,
    // the copy (written only past its end) would leave that many of the ring's frames out for the streams that play it.
    //
    held = 0;
    if (ring->HistoryValid && frame < ring->HistoryEnd)
    {
        held = (ULONG)MIN(ring->HistoryEnd - frame, (ULONGLONG)packetFrames);
        AppRingPlayCopy(frame, held, PacketBuffer);
    }
    else
    {
        // Nothing of the copy played at this pass: its runs playing it (silence, reads past its end), if any, have ended.
        m_EvSilence = 0;
        m_EvLate = 0;
    }
    want = packetFrames - held;
    dest = PacketBuffer + (SIZE_T)held * NF_VMIC_FRAME_BYTES;
    at = frame + held;

    // The one value read back from the app's memory, untrusted. Acquire: the frames before it are written.
    writeFrame = NfVmicLoadAcquire64(&ring->Header->WriteFrame);
    readFrame = ring->ReadFrame;

    // Unsigned: a write_frame behind read_frame shows as a fill far above the capacity, so one test catches both.
    fill = writeFrame - readFrame;
    if (fill > capacity)
    {
        // The skip log: the ring's frames the re-sync passed over (none if WriteFrame went back), the packet's place.
        NF_VMIC_EVENT event;

        AppRingEventInit(&event, NF_VMIC_EVENT_RESYNC, (writeFrame > readFrame) ? writeFrame - readFrame : 0, at, at);
        event.Flags |= NF_VMIC_EVENTF_RING | ((writeFrame < readFrame) ? NF_VMIC_EVENTF_BACKWARD : 0);
        event.RingFrom = readFrame;
        event.RingTo = writeFrame;
        (void)NfVmicLogEvent(AppRingSkipLog(ring), &event);

        readFrame = writeFrame;
        fill = 0;
        ring->Stats.Resyncs++;
        ring->AttachResyncs++;
    }

    //
    // Each part of the rest of the packet goes into the reader's copy too, at its frames on this stream's time line, from the same
    // source (the app's frames, or silence): not read back from PacketBuffer, which this stream's client can see and write.
    //
    copyFrames = (ULONG)MIN(fill, (ULONGLONG)want);
    if (copyFrames != 0)
    {
        // At most two runs, to the ring's end and then from its start; the capacity is a power of two (checked at ATTACH).
        index = (ULONG)(readFrame & (capacity - 1));
        first = MIN(copyFrames, capacity - index);
        RtlCopyMemory(dest, ring->Data + (SIZE_T)index * NF_VMIC_FRAME_BYTES, (SIZE_T)first * NF_VMIC_FRAME_BYTES);
        AppRingHistoryWrite(ring, at, ring->Data + (SIZE_T)index * NF_VMIC_FRAME_BYTES, readFrame, first);
        if (copyFrames > first)
        {
            RtlCopyMemory(dest + (SIZE_T)first * NF_VMIC_FRAME_BYTES,
                          ring->Data,
                          (SIZE_T)(copyFrames - first) * NF_VMIC_FRAME_BYTES);
            AppRingHistoryWrite(ring, at + first, ring->Data, readFrame + first, copyFrames - first);
        }
    }
    if (copyFrames < want)
    {
        // An underrun: what the app hasn't written yet is silence. read_frame stops at write_frame: nothing is skipped.
        ULONG under = want - copyFrames;

        RtlZeroMemory(dest + (SIZE_T)copyFrames * NF_VMIC_FRAME_BYTES, (SIZE_T)under * NF_VMIC_FRAME_BYTES);
        AppRingHistoryWrite(ring, at + copyFrames, NULL, 0, under);
        ring->Stats.UnderrunPackets++;
        ring->Stats.UnderrunFrames += under;
        ring->AttachUnderrunFrames += under;

        // The underrun log: one entry a run of underrunning passes, extended while it lasts; its ring span from where the ring
        // ran dry at the run's start to where its latest silence began.
        if (m_EvUnderrun != 0 && NfVmicLogExtend(AppRingUnderrunLog(ring), m_EvUnderrun, under, at + want, 0))
        {
            (void)NfVmicLogRingSpan(AppRingUnderrunLog(ring), m_EvUnderrun, 0, readFrame + copyFrames);
        }
        else
        {
            NF_VMIC_EVENT event;

            AppRingEventInit(&event, NF_VMIC_EVENT_READER_UNDERRUN, under, at + copyFrames, at + want);
            event.Flags |= NF_VMIC_EVENTF_RING;
            event.RingFrom = readFrame + copyFrames;
            event.RingTo = readFrame + copyFrames;
            m_EvUnderrun = NfVmicLogEvent(AppRingUnderrunLog(ring), &event);
        }
    }
    else
    {
        m_EvUnderrun = 0;
    }
    ring->HistoryLeadFrames = packetFrames;

    // Its place in the copy, should it play the copy again in this attachment (after a Pause and Run, another holding the slot).
    AppRingPlaceReached(frame + packetFrames);

    readFrame += copyFrames;
    ring->ReadFrame = readFrame;
    ring->LastWriteSeen = writeFrame;
    ring->Stats.FramesCopied += copyFrames;
    ring->AttachFramesCopied += copyFrames;

    //
    // The counters, then read_frame with release ordering: once the app sees it, the copy is done and those slots are free.
    // READING whether or not the ring held a whole packet: an underrun is still a pass served from the ring.
    //
    // Live producers pair the absolute read position with the QPC of this pass. Both barriers are on the driver-owned
    // sequence (never use a read-modify-write on app-controlled memory). Ordinary feeds retain the old layout semantics.
    if (ring->LiveFeed)
    {
        NfVmicStore64(&ring->Header->ClockSequence, ++ring->ClockSequence);
        KeMemoryBarrier();
    }
    AppRingStoreCounters(ring, NF_VMIC_STATE_ATTACHED | NF_VMIC_STATE_READING, packetFrames, qpc);
    NfVmicStoreRelease64(&ring->Header->ReadFrame, readFrame);
    if (ring->LiveFeed)
    {
        NfVmicStoreRelease64(&ring->Header->ClockSequence, ++ring->ClockSequence);
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
    return TRUE;
}

//
// A stream that isn't the reader reads the reader's copy, as LoopbackCapturePass reads the loopback ring: from its own place
// on the time line, the lag behind (m_LbLagFrames and the reader's packet, HistoryLeadFrames). That place moves only with its
// time line (Run, a glitch adjustment) or the reader's packet (a handoff), so between those it advances exactly a packet a pass
// and plays the readers' audio continuously.
//
// At a handoff to a reader with a longer packet its place moves back by the difference, and it gets that much silence
// (AppRingPlayCopy), not the audio it has had again: counted as an underrun. To one with a shorter packet it moves on by the
// difference at once and skips it, once: what the copy holds between its place and its new read never reaches it, counted
// (SecondarySkips, SecondarySkippedFrames) apart from the taker's skip (HandoffSkippedFrames), so a recording's gap can be tied
// to the stream that had it (the owner, 2026-11-23: a recorder playing on jumps forward to the new reader's lag, lower latency
// over a one-time skip; from a66d8ab to e801320 it kept the longer lag until it next ran). Counted as a handoff's skip is: from
// the copy's valid start at the earliest to its end at the latest, less its recorded gaps (silence no reader delivered), the
// last reader's underrun silence there included. The copy holds the whole span unless the new reader's timer is later than
// this one's by more than the 1 ms, which leaves this stream's own read short too (silence, counted as underruns). A stream
// that is pausing (m_AppNoSlot: it gave the slot up, and its timer stops after a pass or so) keeps its lag instead: its client
// is stopping, and a skip of its would count in the window of the streams that play on. What the copy doesn't hold (not yet
// written, or already overwritten) is silence, and so is what it holds for a gap in the reader's time line: both counted as
// underruns.
//
_Use_decl_annotations_
VOID
CCaptureStreamEngine::AppRingOtherPass(
    ULONGLONG Packet,
    PBYTE   PacketBuffer,
    ULONG   PacketSize
)
{
    PNF_APP_RING    ring = &m_Cable->Ring;
    ULONG           lead = ring->HistoryLeadFrames;
    ULONGLONG       from;

    if (!m_AppFormatOk || m_LbPacketFrames == 0 || (ULONGLONG)m_LbPacketFrames * NF_VMIC_FRAME_BYTES != PacketSize)
    {
        // Raw copies only into 48 kHz float32 stereo packets (the fallback: CaptureStreamOpen refuses other formats while a
        // ring is attached).
        ring->Stats.MismatchPackets++;
        m_AppOtherValid = FALSE;
        RtlZeroMemory(PacketBuffer, PacketSize);
        return;
    }

    if (m_AppOtherValid && m_AppNoSlot && lead < m_AppOtherLead)
    {
        lead = m_AppOtherLead;
    }

    // m_LbLagFrames: LOOPBACK_LAG_HNS and a frame (Run; m_AppFormatOk implies m_LbFormat.Valid, which sets it).
    from = LoopbackAnchor() - m_LbLagFrames - lead + (ULONGLONG)Packet * m_LbPacketFrames;

    // Not the reader at this pass: its underrun run as the reader, if any, has ended.
    m_EvUnderrun = 0;

    //
    // m_AppOtherValid: its latest pass was this run's and played the copy; the attachment check, that its place is in this copy
    // (an ATTACH between two passes starts the copy and its lead again). Its place (m_AppEnd) is then where that pass's read
    // ended, and this pass's read at the same lag (atOld) starts there unless its own time line moved.
    //
    if (m_AppOtherValid && m_AppEndAttach == ring->Stats.Attaches)
    {
        ULONGLONG   atOld = from + lead - m_AppOtherLead;
        ULONGLONG   jumpEnd = MIN(atOld, from);

        //
        // A jump in its own time line (a late timer's glitch adjustment): its read at the old lag starts past its place, and
        // what the copy holds between never reaches it. Logged, so its gap has an entry too, and counted in the cable whose
        // skip log takes the entry (the app ring's SecondaryJumps and SecondaryJumpFrames, the owner, 2026-11-24), the
        // frames as the entry has them (the copy's, at most its length: the entry's 32 bits hold them), so the log's jumps
        // over a window add up to the counters' rise. Those are the frames the copy holds there now, as a skip's are counted:
        // what lies past its end (claimed below) joins only the entry's ring span as the reader writes it.
        //
        if (jumpEnd > m_AppEnd)
        {
            ULONGLONG jumped = AppRingSkipFrames(ring, m_AppEnd, jumpEnd);

            ring->SecondaryJumps++;
            ring->SecondaryJumpFrames += jumped;
            (void)AppRingLogSkip(NF_VMIC_EVENT_SECONDARY_JUMP, jumped, m_AppEnd, jumpEnd, TRUE);
        }

        //
        // A shorter reader packet than its latest pass played behind: this pass's read at the old lag would have started the
        // difference earlier. What lies between its place (or that read, if later: the jump above is not this skip) and this
        // read is skipped, counted and logged.
        //
        if (lead < m_AppOtherLead)
        {
            ULONGLONG start = MAX(atOld, m_AppEnd);

            if (from > start)
            {
                ULONGLONG skipped = AppRingSkipFrames(ring, start, from);

                ring->SecondarySkips++;
                ring->SecondarySkippedFrames += skipped;
                (void)AppRingLogSkip(NF_VMIC_EVENT_SECONDARY_SKIP, skipped, start, from, TRUE);
            }
        }
    }
    m_AppOtherLead = lead;

    AppRingPlayCopy(from, m_LbPacketFrames, PacketBuffer);

    // Its place (m_AppEnd) is now from this run, for the skips a handoff or a shorter lag count (AppRingCapturePass, above).
    m_AppOtherValid = TRUE;
}

//
// Frames frames of the reader's copy for this stream, from time-line frame From, into Dest. A stream never has the same frame
// of the copy twice: what lies before its place (m_AppEnd: where its reads of the copy, or its packets as the reader, reached
// in this attachment) is silence, as is what the copy doesn't hold (AppRingHistoryRead). Counted: the ring's audio in
// SecondaryFramesCopied, the silence in SecondaryUnderrunFrames. Its place then reaches From + Frames.
//
// Logged (nf_vmic.h, "The event logs"): what it asks for past the copy's end (or with no copy yet) is a late read in the skip
// log, the audio the reader then writes there never reaching it; one entry for its contiguous reads past the end, extended,
// whose ring span those writes fill in. Its other silence (frames it had had, the copy's gaps, before its valid start) is an
// underrun run in the underrun log.
//
_Use_decl_annotations_
VOID
CCaptureStreamEngine::AppRingPlayCopy(
    ULONGLONG   From,
    ULONG       Frames,
    PBYTE       Dest
)
{
    PNF_APP_RING    ring = &m_Cable->Ring;
    ULONGLONG       end = From + Frames;
    ULONGLONG       from = From;
    ULONGLONG       audio;
    ULONGLONG       ahead;
    ULONGLONG       late;
    ULONGLONG       silence;

    if (m_AppEndAttach == ring->Stats.Attaches && m_AppEnd > From)
    {
        from = MIN(m_AppEnd, end);
        RtlZeroMemory(Dest, (SIZE_T)(from - From) * NF_VMIC_FRAME_BYTES);
    }

    audio = AppRingHistoryRead(ring, from, (ULONG)(end - from), Dest + (SIZE_T)(from - From) * NF_VMIC_FRAME_BYTES);
    ring->SecondaryFramesCopied += audio;
    ring->SecondaryUnderrunFrames += Frames - audio;

    // Past the copy's end: [ahead, end), none of it audio (AppRingHistoryRead reads the copy only to its end).
    ahead = ring->HistoryValid ? MAX(from, ring->HistoryEnd) : from;
    late = (end > ahead) ? end - ahead : 0;
    silence = Frames - audio - late;

    if (late == 0)
    {
        m_EvLate = 0;
    }
    else if (m_EvLate != 0 && m_EvLateEnd == ahead && NfVmicAppRingClaimExtend(ring, m_EvLate, ahead, end))
    {
        (void)NfVmicLogExtend(AppRingSkipLog(ring), m_EvLate, (ULONG)late, end, 0);
        m_EvLateEnd = end;
    }
    else
    {
        m_EvLate = AppRingLogSkip(NF_VMIC_EVENT_LATE_READ, late, ahead, end, TRUE);
        m_EvLateEnd = end;
    }

    if (silence == 0)
    {
        m_EvSilence = 0;
    }
    else
    {
        USHORT flags = 0;

        if (from > From)
        {
            flags |= NF_VMIC_EVENTF_BEHIND;
        }
        if (ring->HistoryValid && AppRingHistoryValidFrom(ring) > from && end > from)
        {
            flags |= NF_VMIC_EVENTF_OLD;
        }
        if (silence > (from - From) + ((flags & NF_VMIC_EVENTF_OLD) ? MIN(AppRingHistoryValidFrom(ring), end) - from : 0))
        {
            flags |= NF_VMIC_EVENTF_GAP;
        }
        if (m_EvSilence == 0 || !NfVmicLogExtend(AppRingUnderrunLog(ring), m_EvSilence, (ULONG)silence, end, flags))
        {
            NF_VMIC_EVENT event;

            AppRingEventInit(&event, NF_VMIC_EVENT_SECONDARY_UNDERRUN, silence, From, end);
            event.Flags |= flags;
            m_EvSilence = NfVmicLogEvent(AppRingUnderrunLog(ring), &event);
        }
    }

    AppRingPlaceReached(end);
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::AppRingPlaceReached(
    ULONGLONG   End
)
{
    PNF_APP_RING ring = &m_Cable->Ring;

    // Stats.Attaches counts the attachments, so it names this one: a place in an earlier one's copy means nothing now.
    if (m_AppEndAttach != ring->Stats.Attaches || End > m_AppEnd)
    {
        m_AppEnd = End;
        m_AppEndAttach = ring->Stats.Attaches;
    }
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::AppRingReaderStop()
{
    if (m_Cable == NULL)
    {
        return;
    }
    PNF_APP_RING    ring = &m_Cable->Ring;
    KIRQL           oldIrql;

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    // Under the lock: a pass of this stream on another processor reads it there (AppRingCapturePass).
    m_AppNoSlot = TRUE;
    if (ring->Header != NULL && ring->LiveFeed && m_AppLiveAttach == ring->Stats.Attaches)
    {
        NT_ASSERT(ring->LiveStreams > 0);
        --ring->LiveStreams;
    }
    m_AppLiveAttach = 0;

    if (ring->Reader == this)
    {
        ring->Reader = NULL;
        ring->ReaderState = 0;
        ring->ReaderPacketFrames = 0;
    }
    if (ring->Reader == NULL && ring->Header != NULL)
    {
        // A running secondary can take the slot on its next pass. The live producer keeps writing across that handoff.
        ULONG state = NF_VMIC_STATE_ATTACHED;
        if (ring->LiveFeed && ring->LiveStreams != 0)
        {
            state |= NF_VMIC_STATE_LIVE_HANDOFF;
        }
        NfVmicStore32(&ring->Header->State, state);
        NfVmicStore32(&ring->Header->PacketFrames, 0);
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CCaptureStreamEngine::ReadRegistrySettings()
{
    NTSTATUS                    status;
    PDRIVER_OBJECT              DriverObject;
    HANDLE                      DriverKey;

    RTL_QUERY_REGISTRY_TABLE    paramTable[] = {
        // QueryRoutine     Flags                                           Name                        EntryContext                  DefaultType                                                     DefaultData                   DefaultLength
        { NULL,   RTL_QUERY_REGISTRY_DIRECT | RTL_QUERY_REGISTRY_TYPECHECK, L"EnableWaveCapture",       &m_EnableWaveCapture,         (REG_DWORD << RTL_QUERY_REGISTRY_TYPECHECK_SHIFT) | REG_DWORD,  &m_EnableWaveCapture,         sizeof(DWORD) },
        { NULL,   RTL_QUERY_REGISTRY_DIRECT | RTL_QUERY_REGISTRY_TYPECHECK, L"HostCaptureFileName",     &m_HostCaptureFileName,       (REG_SZ << RTL_QUERY_REGISTRY_TYPECHECK_SHIFT) | REG_SZ,        &m_HostCaptureFileName,       sizeof(UNICODE_STRING) },
        { NULL,   RTL_QUERY_REGISTRY_DIRECT | RTL_QUERY_REGISTRY_TYPECHECK, L"LoopbackCaptureFileName", &m_LoopbackCaptureFileName,   (REG_SZ << RTL_QUERY_REGISTRY_TYPECHECK_SHIFT) | REG_SZ,        &m_LoopbackCaptureFileName,   sizeof(UNICODE_STRING) },
        { NULL,   0,                                                        NULL,                       NULL,                         0,                                                              NULL,                         0 }
    };

    PAGED_CODE();

    DriverObject = WdfDriverWdmGetDriverObject(WdfGetDriver());
    DriverKey = NULL;
    status = IoOpenDriverRegistryKey(DriverObject, 
                                 DriverRegKeyParameters,
                                 KEY_READ,
                                 0,
                                 &DriverKey);

    if (!NT_SUCCESS(status))
    {
        ASSERT(FALSE);
        goto exit;
    }

    status = RtlQueryRegistryValues(RTL_REGISTRY_HANDLE,
                                  (PCWSTR) DriverKey,
                                  &paramTable[0],
                                  NULL,
                                  NULL);

    if (DriverKey)
    {
        ZwClose(DriverKey);
    }

exit:
    if (!NT_SUCCESS(status))
    {
        m_EnableWaveCapture = FALSE;
    }

    return status;
}
_Use_decl_annotations_
PAGED_CODE_SEG
CBufferedCaptureStreamEngine::CBufferedCaptureStreamEngine(
    _In_ ACXSTREAM          Stream,
    _In_ ACXDATAFORMAT      StreamFormat,
    _In_ CKeywordDetector* KeywordDetector

)
    : CCaptureStreamEngine(Stream, StreamFormat),
    m_KeywordDetector(KeywordDetector)
{
    PAGED_CODE();
}

_Use_decl_annotations_
PAGED_CODE_SEG
CBufferedCaptureStreamEngine::~CBufferedCaptureStreamEngine()
{
    PAGED_CODE();
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CBufferedCaptureStreamEngine::Pause()
{
    PAGED_CODE();

    m_KeywordDetector->Stop();
    return CCaptureStreamEngine::Pause();
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CBufferedCaptureStreamEngine::Run()
{
    PAGED_CODE();

    m_KeywordDetector->Run();
    return CCaptureStreamEngine::Run();
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CBufferedCaptureStreamEngine::GetCapturePacket(
    _Out_   ULONG* LastCapturePacket,
    _Out_   ULONGLONG* QPCPacketStart,
    _Out_   BOOLEAN* MoreData
)
{
    PAGED_CODE();

    // retrieve the packet from the fifo queue
    return m_KeywordDetector->GetReadPacket(m_PacketsCount, m_PacketSize, m_Packets, LastCapturePacket, QPCPacketStart, MoreData);
}

_Use_decl_annotations_
VOID
CBufferedCaptureStreamEngine::ProcessPacket()
{
    LARGE_INTEGER qpc;
    LARGE_INTEGER qpcFrequency;

    qpc = KeQueryPerformanceCounter(&qpcFrequency);

    // Add the next packet to the fifo queue
    m_KeywordDetector->DpcRoutine(qpc.QuadPart, qpcFrequency.QuadPart);
}

//Streamengine callbacks

VOID
EvtStreamDestroy(
    _In_ WDFOBJECT Object
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    ctx = GetStreamEngineContext((ACXSTREAM)Object);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;
    ctx->StreamEngine = NULL;
    delete streamEngine;
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetHwLatency(
    _In_ ACXSTREAM Stream,
    _Out_ ULONG* FifoSize,
    _Out_ ULONG* Delay
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->GetHWLatency(FifoSize, Delay);
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamAllocateRtPackets(
    _In_ ACXSTREAM        Stream,
    _In_ ULONG            PacketCount,
    _In_ ULONG            PacketSize,
    _Out_ PACX_RTPACKET* Packets
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->AllocateRtPackets(PacketCount, PacketSize, Packets);
}

PAGED_CODE_SEG
VOID
EvtStreamFreeRtPackets(
    _In_ ACXSTREAM     Stream,
    _In_ PACX_RTPACKET Packets,
    _In_ ULONG         PacketCount
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->FreeRtPackets(Packets, PacketCount);
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamPrepareHardware(
    _In_ ACXSTREAM Stream
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->PrepareHardware();
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamReleaseHardware(
    _In_ ACXSTREAM Stream
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->ReleaseHardware();
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamRun(
    _In_ ACXSTREAM Stream
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->Run();
}


PAGED_CODE_SEG
NTSTATUS
EvtStreamPause(
    _In_ ACXSTREAM Stream
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->Pause();
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamAssignDrmContentId(
    _In_ ACXSTREAM      Stream,
    _In_ ULONG          DrmContentId,
    _In_ PACXDRMRIGHTS  DrmRights
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine * streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = (CStreamEngine*)ctx->StreamEngine;

    return streamEngine->AssignDrmContentId(DrmContentId, DrmRights);
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetCurrentPacket(
    _In_ ACXSTREAM          Stream,
    _Out_ PULONG            CurrentPacket
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = static_cast<CStreamEngine*>(ctx->StreamEngine);

    return streamEngine->GetCurrentPacket(CurrentPacket);
}

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetPresentationPosition(
    _In_ ACXSTREAM          Stream,
    _Out_ PULONGLONG        PositionInBlocks,
    _Out_ PULONGLONG        QPCPosition
)
{
    PSTREAMENGINE_CONTEXT ctx;
    CStreamEngine* streamEngine = NULL;

    PAGED_CODE();

    ctx = GetStreamEngineContext(Stream);

    streamEngine = static_cast<CStreamEngine*>(ctx->StreamEngine);

    return streamEngine->GetPresentationPosition(PositionInBlocks, QPCPosition);
}

