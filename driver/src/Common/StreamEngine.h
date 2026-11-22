#pragma once

#include "savedata.h"
#include "tonegenerator.h"
#include "WaveReader.h"
#include "SimPeakMeter.h"
#include "keyworddetector.h"

#define HNSTIME_PER_MILLISECOND 10000

#define MAX_PACKET_COUNT 2

#define DEFAULT_FREQUENCY 220
#define LOOPBACK_FREQUENCY 500

// noise-filter (TODO 19): the loopback pair's timing.
// The capture side reads this far behind the render side's presentation time line: the budget for a render timer that fires
// late while the capture timer is on time (high-resolution timers fire late, never early; up to about 1 ms per Learn as the
// base study read it; spike check 5 measures it on the box). The capture side adds one frame for integer rounding.
#define LOOPBACK_LAG_HNS        (1 * HNSTIME_PER_MILLISECOND)
// A render time line that moves later by up to this much (resume, a late timer's glitch adjustment) keeps its ring run and
// zeros the gap; a longer move starts a new run.
#define LOOPBACK_MAX_GAP_HNS    (100 * HNSTIME_PER_MILLISECOND)

// Stream callbacks shared between Capture and Render

VOID
EvtStreamDestroy(
    _In_ WDFOBJECT Object
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetHwLatency(
    _In_ ACXSTREAM Stream,
    _Out_ ULONG* FifoSize,
    _Out_ ULONG* Delay
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamAllocateRtPackets(
    _In_ ACXSTREAM Stream,
    _In_ ULONG PacketCount,
    _In_ ULONG PacketSize,
    _Out_ PACX_RTPACKET* Packets
);

PAGED_CODE_SEG
VOID
EvtStreamFreeRtPackets(
    _In_ ACXSTREAM Stream,
    _In_ PACX_RTPACKET Packets,
    _In_ ULONG PacketCount
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamPrepareHardware(
    _In_ ACXSTREAM Stream
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamReleaseHardware(
    _In_ ACXSTREAM Stream
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamRun(
    _In_ ACXSTREAM Stream
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamPause(
    _In_ ACXSTREAM Stream
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamAssignDrmContentId(
    _In_ ACXSTREAM      Stream,
    _In_ ULONG          DrmContentId,
    _In_ PACXDRMRIGHTS  DrmRights
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetCurrentPacket(
    _In_ ACXSTREAM          Stream,
    _Out_ PULONG            CurrentPacket
);

PAGED_CODE_SEG
NTSTATUS
EvtStreamGetPresentationPosition(
    _In_ ACXSTREAM          Stream,
    _Out_ PULONGLONG        PositionInBlocks,
    _Out_ PULONGLONG        QPCPosition
);

//
// noise-filter (TODO 19): the loopback pair's creation-time format check (StreamEngine.cpp). A circuit's
// EvtCircuitCreateStream calls LoopbackStreamOpen before it creates the stream; the stream engine takes the record over
// (CStreamEngine::LoopbackAdoptOpen) and its destructor gives it back with LoopbackStreamClose.
//

// The loopback ring of the device a circuit belongs to, or NULL.
__drv_maxIRQL(DISPATCH_LEVEL)
PLOOPBACK_RING
LoopbackRingOfCircuit(
    _In_    ACXCIRCUIT          Circuit
);

// A stream format's loopback fields (Valid left FALSE: it needs the packet size, known only at Run).
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
LoopbackFormatOf(
    _In_    ACXDATAFORMAT       DataFormat,
    _Out_   PLOOPBACK_FORMAT    Format
);

// noinline (both): called from PAGE code (the circuits' EvtCircuitCreateStream, the stream engine's destructor) and they
// take the ring's spin lock. Inlined into a PAGE function, the locked region would be pageable code running at
// DISPATCH_LEVEL (a bugcheck once paged out, and at once under Driver Verifier).
//
// Counts a stream in on its circuit (Render: the render circuit, else the capture circuit), or refuses it with
// STATUS_NOT_SUPPORTED while the other circuit has a stream open in a different format.
__declspec(noinline)
__drv_maxIRQL(DISPATCH_LEVEL)
NTSTATUS
LoopbackStreamOpen(
    _In_    PLOOPBACK_RING          Ring,
    _In_    BOOLEAN                 Render,
    _In_    const LOOPBACK_FORMAT * Format
);

__declspec(noinline)
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
LoopbackStreamClose(
    _In_    PLOOPBACK_RING          Ring,
    _In_    BOOLEAN                 Render,
    _In_    const LOOPBACK_FORMAT * Format
);

//
// noise-filter (TODO 19, spike check 4): the capture circuit's creation-time checks, the loopback's and the app ring's, in one
// step under both locks (AppRing.h has the order). Refuses (STATUS_NOT_SUPPORTED) a format other than 48 kHz float32 stereo
// while an app ring is attached, counted in RefusedCaptureOpens; else LoopbackStreamOpen's check and record (Ring: NULL if
// the device has none), and a stream in another format counted in OtherFormatCaptureOpens, which ATTACH refuses while it
// isn't 0. CaptureStreamClose gives both records back. noinline for the reason above.
//
__declspec(noinline)
__drv_maxIRQL(DISPATCH_LEVEL)
NTSTATUS
CaptureStreamOpen(
    _In_opt_    PLOOPBACK_RING          Ring,
    _In_        const LOOPBACK_FORMAT * Format
);

__declspec(noinline)
__drv_maxIRQL(DISPATCH_LEVEL)
VOID
CaptureStreamClose(
    _In_opt_    PLOOPBACK_RING          Ring,
    _In_        const LOOPBACK_FORMAT * Format
);


class CStreamEngine
{
public:
    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    AllocateRtPackets(
        _In_    ULONG           PacketCount,
        _In_    ULONG           PacketSize,
        _Out_   PACX_RTPACKET * Packets
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    VOID
    FreeRtPackets(
        _Frees_ptr_ PACX_RTPACKET   Packets,
        _In_        ULONG           PacketCount
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    PrepareHardware();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    ReleaseHardware();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Run();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Pause();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetPresentationPosition(
        _Out_   PULONGLONG      PositionInBlocks,
        _Out_   PULONGLONG      QPCPosition
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetLinearBufferPosition(
        _Out_   PULONGLONG  Position
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    SetCurrentWritePosition(
        _In_    ULONG   Position
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    SetLastBufferPosition(
        _In_    ULONG   Position
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetCurrentPacket(
        _Out_ PULONG CurrentPacket
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetHWLatency(
        _Out_   ULONG         * FifoSize, 
        _Out_   ULONG         * Delay
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    AssignDrmContentId(
        _In_ ULONG          DrmContentId,
        _In_ PACXDRMRIGHTS  DrmRights
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    CSimPeakMeter *
    GetPeakMeter();

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    CStreamEngine(
        _In_    ACXSTREAM       Stream, 
        _In_    ACXDATAFORMAT   StreamFormat,
        _In_    BOOL            Offload,
        _In_opt_    CSimPeakMeter   *CircuitPeakmeter
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    ~CStreamEngine();

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    VOID
    SetFrequency(
        _In_    DWORD           ToneFrequency 
        )
    {
        PAGED_CODE();

        m_ToneFrequency = ToneFrequency;
    }

    // noise-filter (TODO 19): takes over the record LoopbackStreamOpen (Render) or CaptureStreamOpen made for this stream; the
    // destructor gives it back.
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    VOID
    LoopbackAdoptOpen(
        _In_opt_ PLOOPBACK_RING         Ring,
        _In_    BOOLEAN                 Render,
        _In_    const LOOPBACK_FORMAT * Format
        );

protected:
    PVOID               m_Packets[MAX_PACKET_COUNT];
    ULONG               m_PacketsCount;
    ULONG               m_PacketSize;
    ULONG               m_FirstPacketOffset;
    WDFTIMER            m_NotificationTimer;
    ACX_STREAM_STATE    m_CurrentState;
    ULONG               m_CurrentPacket;
    ULONGLONG           m_Position;
    ACXSTREAM           m_Stream;
    ACXDATAFORMAT       m_StreamFormat;
    ULONGLONG           m_StartTime;
    ULONGLONG           m_StartPosition;
    ULONGLONG           m_GlitchAdjust;
    LARGE_INTEGER       m_PerformanceCounterFrequency;
    LARGE_INTEGER       m_LastPacketStart;  // a capture stream's latest completed packet's start (PacketStartQpc)
    DWORD               m_ToneFrequency;
    BOOL                m_Offload;
    CSimPeakMeter       m_PeakMeter;
    CSimPeakMeter*      m_pCircuitPeakmeter;

    // noise-filter (TODO 19): the device's loopback ring and this stream's place on its time line.
    PLOOPBACK_RING      m_Loopback;         // in the WDFDEVICE's context; NULL if the device has none
    LOOPBACK_FORMAT     m_LbFormat;         // this stream's format, snapshotted at Run
    ULONG               m_LbPacketFrames;   // m_PacketSize in frames
    ULONG               m_LbRingFrames;     // LOOPBACK_RING_BYTES in frames
    ULONGLONG           m_LbAnchor;         // ring frame of this stream's byte position 0 on its current time line
    BOOLEAN             m_LbAnchorValid;
    BOOLEAN             m_LbOpen;           // this stream holds an open record (LoopbackAdoptOpen)
    PLOOPBACK_RING      m_LbOpenRing;       // the ring it is counted open in, or NULL (a capture stream on a device without one)
    BOOLEAN             m_LbOpenRender;
    LOOPBACK_FORMAT     m_LbOpenFormat;

    // noise-filter (TODO 19, spike check 5): the timer statistics (AppRing.h).
    ULONG               m_StatsKind;        // NF_VMIC_KIND_RENDER or NF_VMIC_KIND_CAPTURE: the NF_VMIC_STREAM_STATS it counts in
    ULONGLONG           m_ArmedHns;         // the time ScheduleNextPass armed the timer for (its schedule's 100 ns units)

    static
    __drv_maxIRQL(DISPATCH_LEVEL)
    _Function_class_(EVT_WDF_TIMER)
    VOID s_EvtStreamPassCallback(
        _In_    WDFTIMER        Timer
        );

    // This is run every time the stream timer fires
    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    StreamPassCallback();

    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    ScheduleNextPass();

    // noise-filter (TODO 19): the position at Qpc, one clock reading the caller took (so GetPresentationPosition can report
    // that same reading as the position's QPC).
    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    UpdatePosition(
        _In_    LARGE_INTEGER   Qpc
        );

    // noise-filter (TODO 19): one QPC reading in 100 ns units (StreamEngine.cpp says why it takes a snapshot).
    __drv_maxIRQL(DISPATCH_LEVEL)
    ULONGLONG
    QpcToHns(
        _In_    LARGE_INTEGER   Qpc
        );

    // noise-filter (TODO 19): QpcToHns's inverse, to the tick below.
    __drv_maxIRQL(DISPATCH_LEVEL)
    ULONGLONG
    HnsToQpc(
        _In_    ULONGLONG       Hns
        );

    // noise-filter (TODO 19): a packet's start on the stream's time line, as a QPC (StreamPassCallback says why).
    __drv_maxIRQL(DISPATCH_LEVEL)
    ULONGLONG
    PacketStartQpc(
        _In_    ULONGLONG       Packet
        );

    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    ULONG
    GetBytesPerSecond();

    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    ProcessPacket() = 0;

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    VOID
    LoopbackSetFormat();

    __drv_maxIRQL(DISPATCH_LEVEL)
    ULONGLONG
    LoopbackAnchor();
};

class CRenderStreamEngine : public CStreamEngine
{
public:
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    CRenderStreamEngine(
        _In_    ACXSTREAM       Stream,
        _In_    ACXDATAFORMAT   StreamFormat,
        _In_    BOOL            Offload,
        _In_    CSimPeakMeter   *CircuitPeakmeter
        );

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    ~CRenderStreamEngine();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    PrepareHardware();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    ReleaseHardware();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    AssignDrmContentId(
        _In_ ULONG          DrmContentId,
        _In_ PACXDRMRIGHTS  DrmRights
        );

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    SetRenderPacket(
        _In_    ULONG           Packet,
        _In_    ULONG           Flags,
        _In_    ULONG           EosPacketLength
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Run();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Pause();

protected:
    CSaveData m_SaveData;

    // noise-filter (TODO 19): the render half of the loopback pair.
    ULONG               m_LbNextPacket;     // the next render packet to copy into the ring
    volatile LONG64     m_LbReleased;       // (1 << 32) | newest packet the client released (SetRenderPacket); 0: none since Stop

    __drv_maxIRQL(DISPATCH_LEVEL)
    BOOLEAN
    LoopbackReleased(
        _In_    ULONG           Packet
        );

    // noinline: called from PAGE code (Run, Pause, ReleaseHardware, the destructor) and takes the ring's spin lock. Inlined
    // into a PAGE function, its locked region would be pageable code running at DISPATCH_LEVEL (a bugcheck once paged out,
    // and at once under Driver Verifier).
    __declspec(noinline)
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    LoopbackRenderPass(
        _In_    BOOLEAN         AtRun
        );

    // noinline: called from PAGE code (Run, Pause, ReleaseHardware, the destructor) and takes the ring's spin lock. Inlined
    // into a PAGE function, its locked region would be pageable code running at DISPATCH_LEVEL (a bugcheck once paged out,
    // and at once under Driver Verifier).
    __declspec(noinline)
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    LoopbackRenderStop(
        _In_    BOOLEAN         ReleaseOwnership
        );

    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    ProcessPacket();

};

class CCaptureStreamEngine : public CStreamEngine
{
public:
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    CCaptureStreamEngine(
        _In_    ACXSTREAM       Stream,
        _In_    ACXDATAFORMAT   StreamFormat
        );

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    ~CCaptureStreamEngine();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    PrepareHardware();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    ReleaseHardware();

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetCapturePacket(
        _Out_   ULONG         * LastCapturePacket,
        _Out_   ULONGLONG     * QPCPacketStart,
        _Out_   BOOLEAN       * MoreData
        );

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Run();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Pause();

protected:
    ToneGenerator   m_ToneGenerator;
    CWaveReader     m_WaveReader;
    DWORD           m_EnableWaveCapture;
    UNICODE_STRING  m_HostCaptureFileName;
    UNICODE_STRING  m_LoopbackCaptureFileName;

    // noise-filter (TODO 19): the capture half of the loopback pair.
    ULONG           m_LbLagFrames;      // LOOPBACK_LAG_HNS in frames, rounded up, plus one
    ULONG           m_LbAnchorLead;     // the render lead (LOOPBACK_RING.RenderLeadFrames) the anchor was made with
    ULONGLONG       m_LbReadBase;       // ring frame read for this stream's byte position 0

    // noise-filter (TODO 19, spike check 4): the app-fed ring (AppRing.h).
    BOOLEAN         m_AppFormatOk;      // 48 kHz float32 stereo, whole frames (snapshotted at Run): the app ring may be copied
    ULONGLONG       m_AppOtherEnd;      // where this stream's latest pass as another stream ended, on the copy's time line
    BOOLEAN         m_AppOtherValid;    // m_AppOtherEnd is from this run: a handoff to this stream counts what it skips

    // noinline: takes the ring's spin lock; kept out of line as its render counterparts are, so no PAGE caller can inline it.
    __declspec(noinline)
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    LoopbackCapturePass(
        _In_                            ULONG   Packet,
        _Out_writes_bytes_(PacketSize)  PBYTE   PacketBuffer,
        _In_                            ULONG   PacketSize
        );

    // noinline: takes the app ring's spin lock; out of line as LoopbackCapturePass is. TRUE: an app ring was attached and this
    // pass was served from it (audio or silence); FALSE: none attached, so the loopback serves the pass.
    __declspec(noinline)
    __drv_maxIRQL(DISPATCH_LEVEL)
    BOOLEAN
    AppRingCapturePass(
        _In_                            ULONG   Packet,
        _Out_writes_bytes_(PacketSize)  PBYTE   PacketBuffer,
        _In_                            ULONG   PacketSize
        );

    // The pass of a stream that isn't the reader: the reader's copy, a fixed lag behind. App ring lock held (AppRingCapturePass).
    __drv_requiresIRQL(DISPATCH_LEVEL)
    VOID
    AppRingOtherPass(
        _In_                            ULONG   Packet,
        _Out_writes_bytes_(PacketSize)  PBYTE   PacketBuffer,
        _In_                            ULONG   PacketSize
        );

    // noinline: called from PAGE code (Pause, ReleaseHardware, the destructor) and takes the app ring's spin lock. Inlined
    // into a PAGE function, its locked region would be pageable code at DISPATCH_LEVEL.
    __declspec(noinline)
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    AppRingReaderStop();

    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    ProcessPacket();

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    ReadRegistrySettings();
};

class CBufferedCaptureStreamEngine : public CCaptureStreamEngine
{
public:
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    CBufferedCaptureStreamEngine(
        _In_ ACXSTREAM          Stream,
        _In_ ACXDATAFORMAT      StreamFormat,
        _In_ CKeywordDetector * KeywordDetector
        );

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    ~CBufferedCaptureStreamEngine();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Run();

    virtual
    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    Pause();

    __drv_maxIRQL(PASSIVE_LEVEL)
    PAGED_CODE_SEG
    NTSTATUS
    GetCapturePacket(
        _Out_   ULONG         * LastCapturePacket,
        _Out_   ULONGLONG     * QPCPacketStart,
        _Out_   BOOLEAN       * MoreData
        );

protected:
    virtual
    __drv_maxIRQL(DISPATCH_LEVEL)
    VOID
    ProcessPacket();

    CKeywordDetector * m_KeywordDetector;
};

// Define circuit/stream pin context.
//
typedef struct _STREAM_TIMER_CONTEXT {
    CStreamEngine * StreamEngine;
} STREAM_TIMER_CONTEXT, *PSTREAM_TIMER_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(STREAM_TIMER_CONTEXT, GetStreamTimerContext)
