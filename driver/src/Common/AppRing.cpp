/*++

    noise-filter (TODO 19, spike checks 4 and 5). Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    AppRing.cpp

Abstract:

    The app ring's state, and the stream timers' statistics: how often each stream kind's timer ran, how late against the
    time it was armed for (a histogram and the maximum), and its glitch adjustments. Everything here is non-paged: the timer
    DPCs call it.

Environment:

    Kernel mode

--*/

#include "public.h"
#include "AppRing.h"

NF_APP_RING             g_NfAppRing;
DECLSPEC_CACHEALIGN UCHAR g_NfAppHistory[NF_APP_HISTORY_BYTES];
NF_VMIC_STREAM_STATS    g_NfStreamStats[NF_VMIC_KINDS];
NF_VMIC_CAPTURE_TRACE   g_NfCaptureTrace;

static
PNF_VMIC_STREAM_STATS
NfVmicStreamStats(
    _In_ ULONG Kind
)
{
    return &g_NfStreamStats[(Kind < NF_VMIC_KINDS) ? Kind : NF_VMIC_KIND_CAPTURE];
}

static
ULONG
NfVmicLatenessBucket(
    _In_ ULONGLONG Hns
)
{
    static const ULONGLONG bounds[NF_VMIC_LATENESS_BUCKETS - 1] = NF_VMIC_LATENESS_BOUNDS_HNS;
    ULONG i;

    for (i = 0; i < NF_VMIC_LATENESS_BUCKETS - 1; i++)
    {
        if (Hns < bounds[i])
        {
            return i;
        }
    }

    return NF_VMIC_LATENESS_BUCKETS - 1;
}

_Use_decl_annotations_
VOID
NfVmicRecordTimerPass(
    ULONG       Kind,
    LONGLONG    LatenessHns
)
{
    PNF_VMIC_STREAM_STATS   stats = NfVmicStreamStats(Kind);
    ULONGLONG               late;
    LONG64                  seen;

    InterlockedIncrement64((volatile LONG64*)&stats->TimerPasses);

    if (LatenessHns < 0)
    {
        // Before its time. High-resolution timers fire late, never early, so this can only be rounding: counted, and taken as 0.
        InterlockedIncrement64((volatile LONG64*)&stats->EarlyPasses);
        LatenessHns = 0;
    }

    late = (ULONGLONG)LatenessHns;
    InterlockedIncrement64((volatile LONG64*)&stats->LatenessBuckets[NfVmicLatenessBucket(late)]);
    InterlockedAdd64((volatile LONG64*)&stats->LatenessSumHns, (LONG64)late);

    seen = ReadNoFence64((const volatile LONG64*)&stats->LatenessMaxHns);
    while ((LONG64)late > seen)
    {
        LONG64 previous = InterlockedCompareExchange64((volatile LONG64*)&stats->LatenessMaxHns, (LONG64)late, seen);

        if (previous == seen)
        {
            break;
        }
        seen = previous;
    }
}

_Use_decl_annotations_
VOID
NfVmicRecordGlitch(
    ULONG       Kind,
    ULONGLONG   AdjustHns
)
{
    PNF_VMIC_STREAM_STATS stats = NfVmicStreamStats(Kind);

    InterlockedIncrement64((volatile LONG64*)&stats->GlitchPasses);
    InterlockedAdd64((volatile LONG64*)&stats->GlitchHns, (LONG64)AdjustHns);
}

_Use_decl_annotations_
VOID
NfVmicRecordRun(
    ULONG Kind
)
{
    InterlockedIncrement64((volatile LONG64*)&NfVmicStreamStats(Kind)->Runs);
}

_Use_decl_annotations_
VOID
NfVmicRecordPassWork(
    ULONG       Kind,
    ULONGLONG   QpcTicks
)
{
    InterlockedAdd64((volatile LONG64*)&NfVmicStreamStats(Kind)->WorkQpcTicks, (LONG64)QpcTicks);
}

_Use_decl_annotations_
VOID
NfVmicCopyStreamStats(
    PNF_VMIC_STREAM_STATS Out
)
{
    ULONG kind;
    ULONG i;

    // Each counter is read whole (an aligned 64-bit read); the set is not one snapshot, which counters don't need.
    for (kind = 0; kind < NF_VMIC_KINDS; kind++)
    {
        const volatile LONG64*  source = (const volatile LONG64*)&g_NfStreamStats[kind];
        UINT64*                 dest = (UINT64*)&Out[kind];

        for (i = 0; i < sizeof(NF_VMIC_STREAM_STATS) / sizeof(UINT64); i++)
        {
            dest[i] = (UINT64)ReadNoFence64(&source[i]);
        }
    }
}

_Use_decl_annotations_
VOID
NfVmicTraceCaptureQuery(
    ULONGLONG   QueryQpc,
    ULONG       Packet,
    ULONGLONG   PacketQpc
)
{
    InterlockedIncrement64((volatile LONG64*)&g_NfCaptureTrace.PacketQueries);
    InterlockedExchange64((volatile LONG64*)&g_NfCaptureTrace.LastQueryQpc, (LONG64)QueryQpc);
    InterlockedExchange64((volatile LONG64*)&g_NfCaptureTrace.LastQueryPacket, (LONG64)Packet);
    InterlockedExchange64((volatile LONG64*)&g_NfCaptureTrace.LastQueryPacketQpc, (LONG64)PacketQpc);
}

_Use_decl_annotations_
VOID
NfVmicTraceCaptureNotify(
    ULONGLONG   Packet,
    ULONGLONG   Qpc
)
{
    InterlockedExchange64((volatile LONG64*)&g_NfCaptureTrace.LastNotifyPacket, (LONG64)Packet);
    InterlockedExchange64((volatile LONG64*)&g_NfCaptureTrace.LastNotifyQpc, (LONG64)Qpc);
}

_Use_decl_annotations_
VOID
NfVmicTraceCaptureCount(
    ULONG Counter
)
{
    InterlockedIncrement64((volatile LONG64*)((Counter == NF_VMIC_TRACE_POSITION) ? &g_NfCaptureTrace.PositionQueries
                                                                                  : &g_NfCaptureTrace.CurrentPacketQueries));
}

_Use_decl_annotations_
VOID
NfVmicCopyCaptureTrace(
    PNF_VMIC_CAPTURE_TRACE Out
)
{
    const volatile LONG64*  source = (const volatile LONG64*)&g_NfCaptureTrace;
    UINT64*                 dest = (UINT64*)Out;
    ULONG                   i;

    for (i = 0; i < sizeof(NF_VMIC_CAPTURE_TRACE) / sizeof(UINT64); i++)
    {
        dest[i] = (UINT64)ReadNoFence64(&source[i]);
    }
}
