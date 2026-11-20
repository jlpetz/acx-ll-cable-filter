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
#include "streamengine.h"

#ifndef __INTELLISENSE__
#include "streamengine.tmh"
#endif

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
    // noise-filter (TODO 19): find the device's loopback ring. The stream's circuit belongs to our one WDFDEVICE
    // (AcxStreamGetCircuit, acxstreams.h:839; AcxCircuitGetWdfDevice, acxcircuit.h:1290; both up to DISPATCH_LEVEL).
    // The pointer is kept: the device context outlives the stream (inference: the stream descends from the device).
    //
    m_Loopback = NULL;
    RtlZeroMemory(&m_LbFormat, sizeof(m_LbFormat));
    m_LbPacketFrames = 0;
    m_LbRingFrames = 0;
    m_LbAnchor = 0;
    m_LbAnchorValid = FALSE;

    {
        ACXCIRCUIT              circuit = AcxStreamGetCircuit(Stream);
        WDFDEVICE               device = (circuit != NULL) ? AcxCircuitGetWdfDevice(circuit) : NULL;
        PCODEC_DEVICE_CONTEXT   devCtx = (device != NULL) ? GetCodecDeviceContext(device) : NULL;

        if (devCtx != NULL)
        {
            m_Loopback = &devCtx->Loopback;
        }
    }
}

_Use_decl_annotations_
PAGED_CODE_SEG
CStreamEngine::~CStreamEngine()
{
    PAGED_CODE();
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
    LARGE_INTEGER qpcStart;

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

    //
    // Pause to Run.
    //
    // Save the time and position - if we ran and paused previously, the StartTime and StartPosition will allow
    // us to continue scheduling packet completions correctly, while still reporting absolute position from the
    // start of the stream.
    //
    qpcStart = KeQueryPerformanceCounter(NULL);
    m_StartTime = QpcToHns(qpcStart);
    m_StartPosition = m_Position;

    // noise-filter (TODO 19): the run's first packet starts now. StreamPassCallback moves this to m_LastPacketStart, which
    // GetCapturePacket reports as the packet's QPC; left as it was (0 from the constructor, or the last pass before a
    // Pause), the first packet of every run had the wrong capture time.
    InterlockedExchange64(&m_CurrentPacketStart.QuadPart, qpcStart.QuadPart);

    // Reset time we've lost to glitches
    m_GlitchAdjust = 0;

    ScheduleNextPass();

    m_CurrentState = AcxStreamStateRun;
    status = STATUS_SUCCESS;

exit:
    return status;
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

    // Call the StreamPassCallback for the engine
    This->StreamPassCallback();
}

// This is run every time the stream timer fires
_Use_decl_annotations_
VOID
CStreamEngine::StreamPassCallback()
{
    ULONGLONG completedPacket;
    ULONGLONG qpcCompleted;

    // Process the packet (e.g. save render to file/generate capture data)
    ProcessPacket();

    // We've completed a packet! Increment our currently active packet
    completedPacket = (ULONG)InterlockedIncrement((LONG*)&m_CurrentPacket) - 1;
    // Save the time at which we moved to the next packet
    qpcCompleted = (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart;

    InterlockedExchange64(&m_LastPacketStart.QuadPart, m_CurrentPacketStart.QuadPart);
    InterlockedExchange64(&m_CurrentPacketStart.QuadPart, qpcCompleted);

    // Tell ACX we've completed the packet.
    (void)AcxRtStreamNotifyPacketComplete(m_Stream, completedPacket, qpcCompleted);

    // Schedule when our new current packet will finish
    ScheduleNextPass();
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
    nextPacketTimeFromLastPauseHns = nextPacketPositionFromLastPause * HNS_PER_SEC / bytesPerSecond;

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

        StreamPassCallback();

        return;
    }

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
    ULONGLONG position = m_StartPosition + elapsed * bytesPerSecond / HNS_PER_SEC;
    ULONGLONG packetEnd = ((ULONGLONG)m_CurrentPacket + 1) * m_PacketSize;
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
PAGED_CODE_SEG
NTSTATUS
CStreamEngine::GetCurrentPacket(
    _Out_   PULONG          CurrentPacket
)
{
    ULONG currentPacket;
    PAGED_CODE();

    currentPacket = (ULONG)InterlockedCompareExchange((LONG*)&m_CurrentPacket, -1, -1);

    *CurrentPacket = currentPacket;

    return STATUS_SUCCESS;
}

///////////////////////////////////////////////////////////////////////////////
//
// noise-filter (TODO 19, spike check 7): the render-to-capture loopback pair.
//
// The render pass copies each packet into the device's ring at the frame its own time line presents it; the capture pass
// copies one packet from the frame its own time line captures, minus a fixed lag. Both time lines are ScheduleNextPass's
// (absolute QPC, nominal rate), so the two ends stay locked with no drift and no fill-level control. All integer; nothing
// allocates; all shared state is under the ring's spin lock (held for one packet copy).
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

static
BOOLEAN
LoopbackFormatEqual(
    _In_ const LOOPBACK_FORMAT* A,
    _In_ const LOOPBACK_FORMAT* B
)
{
    return (A->Valid &&
            B->Valid &&
            A->SampleRate == B->SampleRate &&
            A->Channels == B->Channels &&
            A->BitsPerSample == B->BitsPerSample &&
            A->ValidBitsPerSample == B->ValidBitsPerSample &&
            A->BlockAlign == B->BlockAlign &&
            RtlEqualMemory(&A->SubFormat, &B->SubFormat, sizeof(GUID))) ? TRUE : FALSE;
}

// Writes Frames frames from Source (NULL: zeros) at ring frame Frame, splitting at the wrap. Ring lock held.
static
VOID
LoopbackRingWrite(
    _In_        PLOOPBACK_RING  Ring,
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
        PBYTE   dest = Ring->Buffer + (SIZE_T)index * BlockAlign;

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

// Reads Frames frames at ring frame Frame into Dest, splitting at the wrap. Ring lock held.
static
VOID
LoopbackRingRead(
    _In_                                    PLOOPBACK_RING  Ring,
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

        RtlCopyMemory(Dest, Ring->Buffer + (SIZE_T)index * BlockAlign, bytes);

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

    RtlZeroMemory(&format, sizeof(format));

    format.SampleRate = AcxDataFormatGetSampleRate(m_StreamFormat);
    format.Channels = AcxDataFormatGetChannelsCount(m_StreamFormat);
    format.BitsPerSample = AcxDataFormatGetBitsPerSample(m_StreamFormat);
    format.ValidBitsPerSample = AcxDataFormatGetValidBitsPerSample(m_StreamFormat);
    format.BlockAlign = AcxDataFormatGetBlockAlign(m_StreamFormat);
    format.SubFormat = AcxDataFormatGetSubFormat(m_StreamFormat);

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
    m_LbReleased(0)
{
    PAGED_CODE();
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
    return CStreamEngine::PrepareHardware();
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
    ULONG currentPacket;

    UNREFERENCED_PARAMETER(Flags);
    UNREFERENCED_PARAMETER(EosPacketLength);

    PAGED_CODE();

    currentPacket = (ULONG)InterlockedCompareExchange((LONG*)&m_CurrentPacket, -1, -1);

    if (Packet <= currentPacket)
    {
        //ASSERT(FALSE);
        status = STATUS_DATA_LATE_ERROR;
    }
    else if (Packet > currentPacket + 1)
    {
        //ASSERT(FALSE);
        status = STATUS_DATA_OVERRUN;
    }

    //
    // noise-filter (TODO 19): remember the newest packet the client has finished writing, for the loopback's look-ahead
    // (LoopbackRenderPass). A late one (Packet <= currentPacket, e.g. a pre-roll of packet 0 before Run) counts too: its data
    // is in the buffer. One writer: the client's property calls on this stream (inference: ACX serialises them).
    //
    if (status != STATUS_DATA_OVERRUN)
    {
        LONG64 released = InterlockedCompareExchange64(&m_LbReleased, 0, 0);

        if (released == 0 || (LONG)(Packet - (ULONG)released) > 0)
        {
            InterlockedExchange64(&m_LbReleased, ((LONG64)1 << 32) | Packet);
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
    NTSTATUS status;

    PAGED_CODE();

    if (m_CurrentState == AcxStreamStatePause)
    {
        // The timer is stopped in Pause, so no pass reads these while they change. The packets exist by now (inference: ACX
        // normally allocates them before PrepareHardware, Learn acx-streaming, and a client cannot start without them).
        LoopbackSetFormat();
    }

    status = CStreamEngine::Run();

    if (NT_SUCCESS(status))
    {
        // Copy the packet the client pre-rolled before Start, if it released it, so the first packet is in the ring before
        // its presentation time too. Serialised with the first timer pass by the ring's lock.
        LoopbackRenderPass(TRUE);
    }

    return status;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
CRenderStreamEngine::Pause()
{
    NTSTATUS status;

    PAGED_CODE();

    status = CStreamEngine::Pause();

    if (NT_SUCCESS(status))
    {
        // The timer has stopped (WdfTimerStop waits for its callback): the capture side reads what is left, then idles.
        LoopbackRenderStop(FALSE);
    }

    return status;
}

_Use_decl_annotations_
BOOLEAN
CRenderStreamEngine::LoopbackReleased(
    ULONG Packet
)
{
    LONG64 released = InterlockedCompareExchange64(&m_LbReleased, 0, 0);

    return (released != 0 && (LONG)((ULONG)released - Packet) >= 0) ? TRUE : FALSE;
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::LoopbackRenderPass(
    BOOLEAN AtRun
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;
    ULONG           current;
    ULONG           endPacket;
    ULONGLONG       anchor;
    BOOLEAN         lookAhead;
    BOOLEAN         copied = FALSE;

    if (ring == NULL || !m_LbFormat.Valid)
    {
        return;
    }

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

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
        m_LbAnchorValid = FALSE;
    }
    else if (ring->Owner != this)
    {
        ring->RenderNotOwner++;
        goto exit;
    }

    //
    // Which packets are final. In the two-packet event-driven mode the client writes packet k+1 while the driver streams
    // packet k and says so with SetRenderPacket(k+1) (Learn, EVT_ACX_STREAM_SET_RENDER_PACKET); it writes k+2 into k's half
    // only after AcxRtStreamNotifyPacketComplete(k), which StreamPassCallback calls after this pass. So at the pass completing
    // k, packet k is final and k+1 is final once released: copying it now puts it in the ring a period before it is
    // presented. Without SetRenderPacket in this run (or in one-packet mode) each packet is copied as it completes.
    //
    lookAhead = (m_PacketsCount == MAX_PACKET_COUNT && InterlockedCompareExchange64(&m_LbReleased, 0, 0) != 0) ? TRUE : FALSE;
    current = (ULONG)InterlockedCompareExchange((LONG*)&m_CurrentPacket, -1, -1);
    if (AtRun)
    {
        // Before the first pass only the current packet is in play: the client's pre-roll.
        endPacket = (lookAhead && LoopbackReleased(current)) ? current + 1 : current;
    }
    else
    {
        // ProcessPacket runs before the packet count moves on, so current is the completing packet.
        endPacket = (lookAhead && LoopbackReleased(current + 1)) ? current + 2 : current + 1;
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

        if (!sameRun && (LONG)(current - m_LbNextPacket) > 0)
        {
            m_LbNextPacket = current;
        }

        next = anchor + (ULONGLONG)m_LbNextPacket * m_LbPacketFrames;

        if (sameRun && next >= ring->WritePos && next - ring->WritePos <= maxGap)
        {
            LoopbackRingWrite(ring, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, NULL, next - ring->WritePos);
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
    if ((LONG)(current - m_LbNextPacket) > 0)
    {
        ULONG       lost = current - m_LbNextPacket;
        ULONGLONG   lostFrames = (ULONGLONG)lost * m_LbPacketFrames;

        LoopbackRingWrite(ring, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, NULL, MIN(lostFrames, (ULONGLONG)m_LbRingFrames));
        ring->WritePos += lostFrames;
        ring->RenderLostPackets += lost;
        m_LbNextPacket = current;
    }

    //
    // Copy the final packets not yet copied, in order, each at its place on the time line.
    //
    while ((LONG)(endPacket - m_LbNextPacket) > 0)
    {
        ULONG   packetIndex = m_LbNextPacket % m_PacketsCount;
        PBYTE   packetBuffer = (PBYTE)m_Packets[packetIndex];

        // Packet 0 starts at an offset if the size isn't a multiple of page_size
        if (packetIndex == 0)
        {
            packetBuffer += m_FirstPacketOffset;
        }

        if (lookAhead && !AtRun && m_LbNextPacket == current)
        {
            // Released too late to go a period early: the capture side may run short on it.
            ring->RenderLatePackets++;
        }

        LoopbackRingWrite(ring, m_LbRingFrames, m_LbFormat.BlockAlign, ring->WritePos, packetBuffer, m_LbPacketFrames);
        ring->WritePos += m_LbPacketFrames;
        m_LbNextPacket++;
        copied = TRUE;
    }

    ring->RenderLeadFrames = lookAhead ? 0 : m_LbPacketFrames;
    if (copied)
    {
        ring->RenderActive = TRUE;
    }

exit:
    KeReleaseSpinLock(&ring->Lock, oldIrql);
}

_Use_decl_annotations_
VOID
CRenderStreamEngine::LoopbackRenderStop(
    BOOLEAN ReleaseOwnership
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;

    if (ring == NULL)
    {
        return;
    }

    KeAcquireSpinLock(&ring->Lock, &oldIrql);

    if (ring->Owner == this)
    {
        ring->RenderActive = FALSE;

        if (ReleaseOwnership)
        {
            // RenderFormat and the valid range stay, so the capture side can still read the tail; the next claim resets them.
            ring->Owner = NULL;
        }
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
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
    m_LbReadBase(0)
{
    PAGED_CODE();

    m_CurrentPacketStart.QuadPart = 0;
    m_LastPacketStart.QuadPart = 0;

    RtlInitUnicodeString(&m_HostCaptureFileName, NULL);
    RtlInitUnicodeString(&m_LoopbackCaptureFileName, NULL);
}

_Use_decl_annotations_
PAGED_CODE_SEG
CCaptureStreamEngine::~CCaptureStreamEngine()
{
    PAGED_CODE();

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
    PAGED_CODE();

    if (m_EnableWaveCapture)
    {
        m_WaveReader.WaitAllWorkItems();
    }

    return CStreamEngine::ReleaseHardware();
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
    ULONG currentPacket;
    LONGLONG qpcPacketStart;

    PAGED_CODE();

    currentPacket = (ULONG)InterlockedCompareExchange((LONG*)&m_CurrentPacket, -1, -1);
    qpcPacketStart = InterlockedCompareExchange64(&m_LastPacketStart.QuadPart, -1, -1);

    *LastCapturePacket = currentPacket - 1;
    *QPCPacketStart = (ULONGLONG)qpcPacketStart;
    *MoreData = FALSE;

    return status;
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::ProcessPacket()
{
    ULONG currentPacket;
    ULONG packetIndex;
    PBYTE packetBuffer;

    currentPacket = (ULONG)InterlockedCompareExchange((LONG*)&m_CurrentPacket, -1, -1);

    packetIndex = currentPacket % m_PacketsCount;
    packetBuffer = (PBYTE)m_Packets[packetIndex];

    // Packet 0 starts at an offset if the size isn't a multiple of page_size
    if (packetIndex == 0)
    {
        packetBuffer += m_FirstPacketOffset;
    }

    //
    // noise-filter (TODO 19): the capture half of the loopback pair, in place of the sample's tone generator and WAV reader
    // (m_ToneGenerator.GenerateSine, m_WaveReader.ReadWaveData).
    //
    LoopbackCapturePass(currentPacket, packetBuffer, m_PacketSize);
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
    }

    return CStreamEngine::Run();
}

_Use_decl_annotations_
VOID
CCaptureStreamEngine::LoopbackCapturePass(
    ULONG   Packet,
    PBYTE   PacketBuffer,
    ULONG   PacketSize
)
{
    PLOOPBACK_RING  ring = m_Loopback;
    KIRQL           oldIrql;
    ULONG           blockAlign = m_LbFormat.BlockAlign;
    ULONGLONG       anchor;
    ULONGLONG       readStart;
    ULONGLONG       readEnd;
    ULONGLONG       validLo;
    ULONGLONG       validHi;
    ULONGLONG       copyLo;
    ULONGLONG       copyHi;

    if (ring == NULL)
    {
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
        // Raw bytes only between identical formats; no conversion in the kernel.
        ring->MismatchPackets++;
        goto zero;
    }

    //
    // Anchor the read position the lag (plus the render side's lead) behind this stream's own place on the ring's time line,
    // again whenever that moves (Run, a late timer's glitch adjustment) or the render lead changes. Between anchors it
    // advances exactly one packet a pass. An underrun or an overrun does not move it: the time line is right, the audio was
    // just not (or no longer) there.
    //
    anchor = LoopbackAnchor();
    if (!m_LbAnchorValid || anchor != m_LbAnchor || ring->RenderLeadFrames != m_LbAnchorLead)
    {
        m_LbAnchor = anchor;
        m_LbAnchorLead = ring->RenderLeadFrames;
        m_LbReadBase = anchor - m_LbLagFrames - m_LbAnchorLead;
        m_LbAnchorValid = TRUE;
        ring->CaptureAnchors++;
    }

    readStart = m_LbReadBase + (ULONGLONG)Packet * m_LbPacketFrames;
    readEnd = readStart + m_LbPacketFrames;

    // The audio the ring still holds of the current render run.
    validHi = ring->WritePos;
    validLo = ring->ValidFrom;
    if (validHi - validLo > m_LbRingFrames)
    {
        validLo = validHi - m_LbRingFrames;
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
        LoopbackRingRead(ring, m_LbRingFrames, blockAlign, copyLo,
                         PacketBuffer + (SIZE_T)(copyLo - readStart) * blockAlign, copyHi - copyLo);
    }
    if (readEnd > copyHi)
    {
        RtlZeroMemory(PacketBuffer + (SIZE_T)(copyHi - readStart) * blockAlign, (SIZE_T)(readEnd - copyHi) * blockAlign);
    }

    if (readEnd > validHi)
    {
        if (ring->RenderActive)
        {
            ring->UnderrunPackets++;
            ring->UnderrunFrames += readEnd - MAX(readStart, validHi);
        }
        else
        {
            ring->IdlePackets++;
        }
    }
    if (readStart < validLo && validLo > ring->ValidFrom)
    {
        ring->OverrunPackets++;
    }

    KeReleaseSpinLock(&ring->Lock, oldIrql);
    return;

zero:
    KeReleaseSpinLock(&ring->Lock, oldIrql);
    RtlZeroMemory(PacketBuffer, PacketSize);
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

