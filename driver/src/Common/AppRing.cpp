/*++

    noise-filter (TODO 19, spike checks 4 and 5). Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0.

Module Name:

    AppRing.cpp

Abstract:

    A cable's event logs, the skip log's records of its app ring's copy, and its stream timers' statistics: how often each
    stream kind's timer ran, how late against the time it was armed for (a histogram and the maximum), and its glitch
    adjustments. Everything here is non-paged: the timer DPCs call it. The state is each cable's (Shared\Cable.h, in its
    device's context); only the stream serial is driver-wide.

Environment:

    Kernel mode

--*/

#include "public.h"
#include "AppRing.h"

volatile LONG           g_NfStreamSerial;

static
PNF_VMIC_STREAM_STATS
NfVmicStreamStats(
    _In_ PNF_CABLE  Cable,
    _In_ ULONG      Kind
)
{
    return &Cable->StreamStats[(Kind < NF_VMIC_KINDS) ? Kind : NF_VMIC_KIND_CAPTURE];
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
    PNF_CABLE   Cable,
    ULONG       Kind,
    LONGLONG    LatenessHns
)
{
    PNF_VMIC_STREAM_STATS   stats = NfVmicStreamStats(Cable, Kind);
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
    PNF_CABLE   Cable,
    ULONG       Kind,
    ULONGLONG   AdjustHns
)
{
    PNF_VMIC_STREAM_STATS stats = NfVmicStreamStats(Cable, Kind);

    InterlockedIncrement64((volatile LONG64*)&stats->GlitchPasses);
    InterlockedAdd64((volatile LONG64*)&stats->GlitchHns, (LONG64)AdjustHns);
}

_Use_decl_annotations_
VOID
NfVmicRecordRun(
    PNF_CABLE   Cable,
    ULONG       Kind
)
{
    InterlockedIncrement64((volatile LONG64*)&NfVmicStreamStats(Cable, Kind)->Runs);
}

_Use_decl_annotations_
VOID
NfVmicRecordPassWork(
    PNF_CABLE   Cable,
    ULONG       Kind,
    ULONGLONG   QpcTicks
)
{
    InterlockedAdd64((volatile LONG64*)&NfVmicStreamStats(Cable, Kind)->WorkQpcTicks, (LONG64)QpcTicks);
}

_Use_decl_annotations_
VOID
NfVmicCopyStreamStats(
    PNF_CABLE               Cable,
    PNF_VMIC_STREAM_STATS   Out
)
{
    ULONG kind;
    ULONG i;

    // Each counter is read whole (an aligned 64-bit read); the set is not one snapshot, which counters don't need.
    for (kind = 0; kind < NF_VMIC_KINDS; kind++)
    {
        const volatile LONG64*  source = (const volatile LONG64*)&Cable->StreamStats[kind];
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
    PNF_CABLE   Cable,
    ULONGLONG   QueryQpc,
    ULONG       Packet,
    ULONGLONG   PacketQpc
)
{
    PNF_VMIC_CAPTURE_TRACE trace = &Cable->CaptureTrace;

    InterlockedIncrement64((volatile LONG64*)&trace->PacketQueries);
    InterlockedExchange64((volatile LONG64*)&trace->LastQueryQpc, (LONG64)QueryQpc);
    InterlockedExchange64((volatile LONG64*)&trace->LastQueryPacket, (LONG64)Packet);
    InterlockedExchange64((volatile LONG64*)&trace->LastQueryPacketQpc, (LONG64)PacketQpc);
}

_Use_decl_annotations_
VOID
NfVmicTraceCaptureNotify(
    PNF_CABLE   Cable,
    ULONGLONG   Packet,
    ULONGLONG   Qpc
)
{
    InterlockedExchange64((volatile LONG64*)&Cable->CaptureTrace.LastNotifyPacket, (LONG64)Packet);
    InterlockedExchange64((volatile LONG64*)&Cable->CaptureTrace.LastNotifyQpc, (LONG64)Qpc);
}

_Use_decl_annotations_
VOID
NfVmicTraceCaptureCount(
    PNF_CABLE   Cable,
    ULONG       Counter
)
{
    InterlockedIncrement64((volatile LONG64*)((Counter == NF_VMIC_TRACE_POSITION) ? &Cable->CaptureTrace.PositionQueries
                                                                                  : &Cable->CaptureTrace.CurrentPacketQueries));
}

_Use_decl_annotations_
VOID
NfVmicCopyCaptureTrace(
    PNF_CABLE               Cable,
    PNF_VMIC_CAPTURE_TRACE  Out
)
{
    const volatile LONG64*  source = (const volatile LONG64*)&Cable->CaptureTrace;
    UINT64*                 dest = (UINT64*)Out;
    ULONG                   i;

    for (i = 0; i < sizeof(NF_VMIC_CAPTURE_TRACE) / sizeof(UINT64); i++)
    {
        dest[i] = (UINT64)ReadNoFence64(&source[i]);
    }
}

// The smaller and the larger of two 64-bit values (Private.h's MIN and MAX aren't included here).
static
FORCEINLINE
ULONGLONG
NfMin64(
    _In_ ULONGLONG A,
    _In_ ULONGLONG B
)
{
    return (A < B) ? A : B;
}

static
FORCEINLINE
ULONGLONG
NfMax64(
    _In_ ULONGLONG A,
    _In_ ULONGLONG B
)
{
    return (A > B) ? A : B;
}

//
// The event logs (AppRing.h). Each call holds the log's lock for one entry (STATS: for the whole log's copy); event n is in slot
// (n - 1) % Entries. The lock is the last in every lock order, so a caller may hold the app ring's and a loopback's.
//

// The slot holding event Sequence, or NULL if a later event has taken it (or there is no such event). Log lock held.
static
PNF_VMIC_EVENT
NfVmicLogSlot(
    _In_ PNF_EVENT_LOG  Log,
    _In_ ULONGLONG      Sequence
)
{
    PNF_VMIC_EVENT slot;

    if (Sequence == 0 || Sequence > Log->Logged)
    {
        return NULL;
    }
    slot = &Log->Slots[(Sequence - 1) % Log->Entries];
    return (slot->Sequence == Sequence) ? slot : NULL;
}

// Each entry point below returns at once for a NULL log, or one not yet set up (no slots).

_Use_decl_annotations_
ULONGLONG
NfVmicLogEvent(
    PNF_EVENT_LOG           Log,
    const NF_VMIC_EVENT*    Event
)
{
    KIRQL           oldIrql;
    ULONGLONG       sequence;
    PNF_VMIC_EVENT  slot;

    if (Log == NULL || Log->Slots == NULL || Log->Entries == 0)
    {
        return 0;
    }

    KeAcquireSpinLock(&Log->Lock, &oldIrql);
    sequence = ++Log->Logged;
    slot = &Log->Slots[(sequence - 1) % Log->Entries];
    *slot = *Event;
    slot->Sequence = sequence;
    // Inside the lock, so the times run in the log's order.
    slot->Qpc = (UINT64)KeQueryPerformanceCounter(NULL).QuadPart;
    KeReleaseSpinLock(&Log->Lock, oldIrql);

    return sequence;
}

_Use_decl_annotations_
BOOLEAN
NfVmicLogExtend(
    PNF_EVENT_LOG   Log,
    ULONGLONG       Sequence,
    ULONG           Frames,
    ULONGLONG       SpanEnd,
    USHORT          Flags
)
{
    KIRQL           oldIrql;
    PNF_VMIC_EVENT  slot;
    BOOLEAN         found;

    if (Log == NULL || Log->Slots == NULL || Log->Entries == 0)
    {
        return FALSE;
    }

    KeAcquireSpinLock(&Log->Lock, &oldIrql);
    slot = NfVmicLogSlot(Log, Sequence);
    found = (slot != NULL) ? TRUE : FALSE;
    if (found)
    {
        slot->Frames = (UINT32)NfMin64((ULONGLONG)slot->Frames + Frames, MAXULONG);
        if (SpanEnd > slot->SpanFrom)
        {
            slot->SpanFrames = (UINT32)NfMin64(SpanEnd - slot->SpanFrom, MAXULONG);
        }
        slot->Flags |= Flags;
    }
    KeReleaseSpinLock(&Log->Lock, oldIrql);

    return found;
}

_Use_decl_annotations_
BOOLEAN
NfVmicLogRingSpan(
    PNF_EVENT_LOG   Log,
    ULONGLONG       Sequence,
    ULONGLONG       RingFrom,
    ULONGLONG       RingTo
)
{
    KIRQL           oldIrql;
    PNF_VMIC_EVENT  slot;
    BOOLEAN         found;

    if (Log == NULL || Log->Slots == NULL || Log->Entries == 0)
    {
        return FALSE;
    }

    KeAcquireSpinLock(&Log->Lock, &oldIrql);
    slot = NfVmicLogSlot(Log, Sequence);
    found = (slot != NULL) ? TRUE : FALSE;
    if (found)
    {
        if ((slot->Flags & NF_VMIC_EVENTF_RING) == 0)
        {
            slot->RingFrom = RingFrom;
            slot->RingTo = RingTo;
            slot->Flags |= NF_VMIC_EVENTF_RING;
        }
        else if (RingTo > slot->RingTo)
        {
            slot->RingTo = RingTo;
        }
    }
    KeReleaseSpinLock(&Log->Lock, oldIrql);

    return found;
}

_Use_decl_annotations_
BOOLEAN
NfVmicLogFlags(
    PNF_EVENT_LOG   Log,
    ULONGLONG       Sequence,
    USHORT          Set,
    USHORT          Clear
)
{
    KIRQL           oldIrql;
    PNF_VMIC_EVENT  slot;
    BOOLEAN         found;

    if (Log == NULL || Log->Slots == NULL || Log->Entries == 0)
    {
        return FALSE;
    }

    KeAcquireSpinLock(&Log->Lock, &oldIrql);
    slot = NfVmicLogSlot(Log, Sequence);
    found = (slot != NULL) ? TRUE : FALSE;
    if (found)
    {
        slot->Flags = (UINT16)((slot->Flags | Set) & ~Clear);
    }
    KeReleaseSpinLock(&Log->Lock, oldIrql);

    return found;
}

_Use_decl_annotations_
VOID
NfVmicCopyEventLog(
    PNF_EVENT_LOG   Log,
    PNF_VMIC_EVENT  Out,
    ULONG           Entries,
    PUINT64         Logged
)
{
    KIRQL oldIrql;

    if (Log == NULL || Log->Slots == NULL || Log->Entries != Entries)
    {
        RtlZeroMemory(Out, (SIZE_T)Entries * sizeof(NF_VMIC_EVENT));
        *Logged = 0;
        return;
    }

    // Whole entries, and the count with them: no writer is inside the lock.
    KeAcquireSpinLock(&Log->Lock, &oldIrql);
    RtlCopyMemory(Out, Log->Slots, (SIZE_T)Entries * sizeof(NF_VMIC_EVENT));
    *Logged = (UINT64)Log->Logged;
    KeReleaseSpinLock(&Log->Lock, oldIrql);
}

//
// The skip log's records of the app ring's copy (AppRing.h). App ring lock held throughout; the entries are in the same cable's
// skip log (NF_APP_RING.Loopback), as every app ring event is.
//
static
PNF_EVENT_LOG
NfVmicAppRingSkipLog(
    _In_ PNF_APP_RING Ring
)
{
    return (Ring->Loopback != NULL) ? &Ring->Loopback->SkipLog : NULL;
}

_Use_decl_annotations_
VOID
NfVmicAppRingAudioWritten(
    PNF_APP_RING    Ring,
    ULONGLONG       From,
    ULONGLONG       To,
    ULONGLONG       RingFrame
)
{
    PNF_EVENT_LOG   log = NfVmicAppRingSkipLog(Ring);
    BOOLEAN         grown = FALSE;
    ULONG           i;

    if (To <= From)
    {
        return;
    }

    // The map: the latest record grows where this follows on from it in both the time line and the ring.
    if (Ring->SegmentNext != 0)
    {
        PNF_APP_SEGMENT last = &Ring->Segments[(Ring->SegmentNext - 1) % NF_APP_SEGMENTS];

        if (last->To == From && last->Ring + (last->To - last->From) == RingFrame)
        {
            last->To = To;
            grown = TRUE;
        }
    }
    if (!grown)
    {
        PNF_APP_SEGMENT next = &Ring->Segments[Ring->SegmentNext % NF_APP_SEGMENTS];

        if (Ring->SegmentNext >= NF_APP_SEGMENTS && next->To > Ring->SegmentsLostTo)
        {
            Ring->SegmentsLostTo = next->To;
        }
        next->From = From;
        next->To = To;
        next->Ring = RingFrame;
        Ring->SegmentNext++;
    }

    // The claims it fills: their entries' ring spans take in the ring's frames written there.
    for (i = 0; i < NF_APP_CLAIMS; i++)
    {
        PNF_APP_CLAIM   claim = &Ring->Claims[i];
        ULONGLONG       lo = NfMax64(From, claim->Lo);
        ULONGLONG       hi = NfMin64(To, claim->Hi);

        if (claim->Sequence != 0 && hi > lo)
        {
            (void)NfVmicLogRingSpan(log, claim->Sequence, RingFrame + (lo - From), RingFrame + (hi - From));
        }
    }
}

_Use_decl_annotations_
BOOLEAN
NfVmicAppRingSpan(
    PNF_APP_RING    Ring,
    ULONGLONG       Lo,
    ULONGLONG       Hi,
    PULONGLONG      RingFrom,
    PULONGLONG      RingTo,
    PBOOLEAN        Whole
)
{
    ULONG       count = (ULONG)NfMin64(Ring->SegmentNext, NF_APP_SEGMENTS);
    BOOLEAN     any = FALSE;
    ULONG       i;

    *RingFrom = 0;
    *RingTo = 0;
    *Whole = (Lo >= Ring->SegmentsLostTo) ? TRUE : FALSE;

    // The ring's frames follow the time line's within the copy (the reader delivers them in order), so the span runs from the
    // lowest the records map there to the highest.
    for (i = 0; i < count; i++)
    {
        PNF_APP_SEGMENT segment = &Ring->Segments[i];
        ULONGLONG       lo = NfMax64(Lo, segment->From);
        ULONGLONG       hi = NfMin64(Hi, segment->To);

        if (hi > lo)
        {
            ULONGLONG ringLo = segment->Ring + (lo - segment->From);
            ULONGLONG ringHi = segment->Ring + (hi - segment->From);

            if (!any || ringLo < *RingFrom)
            {
                *RingFrom = ringLo;
            }
            if (!any || ringHi > *RingTo)
            {
                *RingTo = ringHi;
            }
            any = TRUE;
        }
    }

    return any;
}

_Use_decl_annotations_
VOID
NfVmicAppRingClaim(
    PNF_APP_RING    Ring,
    ULONGLONG       Lo,
    ULONGLONG       Hi,
    ULONGLONG       Sequence
)
{
    PNF_EVENT_LOG   log = NfVmicAppRingSkipLog(Ring);
    ULONG           i;

    if (Sequence == 0 || Hi <= Lo)
    {
        return;
    }

    for (i = 0; i < NF_APP_CLAIMS; i++)
    {
        PNF_APP_CLAIM claim = &Ring->Claims[i];

        if (claim->Sequence == 0)
        {
            claim->Lo = Lo;
            claim->Hi = Hi;
            claim->Sequence = Sequence;
            (void)NfVmicLogFlags(log, Sequence, NF_VMIC_EVENTF_PENDING, 0);
            return;
        }
    }

    (void)NfVmicLogFlags(log, Sequence, NF_VMIC_EVENTF_UNTRACKED, 0);
}

_Use_decl_annotations_
BOOLEAN
NfVmicAppRingClaimExtend(
    PNF_APP_RING    Ring,
    ULONGLONG       Sequence,
    ULONGLONG       Hi,
    ULONGLONG       NewHi
)
{
    ULONG i;

    for (i = 0; i < NF_APP_CLAIMS; i++)
    {
        PNF_APP_CLAIM claim = &Ring->Claims[i];

        if (Sequence != 0 && claim->Sequence == Sequence && claim->Hi == Hi)
        {
            claim->Hi = NewHi;
            return TRUE;
        }
    }

    return FALSE;
}

_Use_decl_annotations_
VOID
NfVmicAppRingSettleClaims(
    PNF_APP_RING    Ring,
    BOOLEAN         All
)
{
    PNF_EVENT_LOG   log = NfVmicAppRingSkipLog(Ring);
    ULONG           i;

    for (i = 0; i < NF_APP_CLAIMS; i++)
    {
        PNF_APP_CLAIM claim = &Ring->Claims[i];

        if (claim->Sequence != 0 && (All || (Ring->HistoryValid && claim->Hi <= Ring->HistoryEnd)))
        {
            (void)NfVmicLogFlags(log, claim->Sequence, 0, NF_VMIC_EVENTF_PENDING);
            claim->Sequence = 0;
        }
    }
}

_Use_decl_annotations_
VOID
NfVmicAppRingMapReset(
    PNF_APP_RING Ring
)
{
    Ring->SegmentNext = 0;
    Ring->SegmentsLostTo = 0;
    RtlZeroMemory(Ring->Segments, sizeof(Ring->Segments));
}
