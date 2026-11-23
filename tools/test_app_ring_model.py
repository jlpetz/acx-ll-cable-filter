"""An offline model of the driver's app-ring capture passes (driver/src/Common/StreamEngine.cpp: AppRingCapturePass,
AppRingOtherPass, AppRingPlayCopy, AppRingHistoryWrite; TODO 19), checking the ring's rules for three capture streams
through a reader handoff: no stream gets a ring frame twice, and every gap in a stream's frames is one of its counted skips,
to the frame: the one that takes the slot loses exactly its HandoffSkippedFrames, and one that plays on behind a new reader
with a shorter packet moves on to the new lag at once and loses exactly its SecondarySkippedFrames (the owner, 2026-11-23:
lower latency over a one-time skip), where the driver from a66d8ab to e801320 kept the longer lag with no gap. Frames are
numbered by the ring's position, time in frames on the QPC time line; the app always has frames ready (no underruns), and
each pass's timer is late by up to 817 us, the maximum in the A1 session's live runs (the later session's wasn't read).
Left out: the copy's 32,768-frame length and its 32-gap record, glitch adjustments, a Pause and Run of one stream.

It also shows the cause of the uncounted gaps in the live three-stream runs (2026-11-23): a reader that gave the slot up only
after its timer stopped left the copy short for as long as the stop took, and a stream playing on lost what the next
reader then wrote there. Run from any directory:
    python tools/test_app_ring_model.py
"""
import random
import unittest

LAG = 49  # LOOPBACK_LAG_HNS (1 ms) in frames, plus one
MAX_GAP = 4800  # LOOPBACK_MAX_GAP_HNS (100 ms): a longer jump starts a new run of the copy
LATE_MAX = 39  # 817 us, the timer lateness maximum in the A1 session's live runs


class Stream:
    def __init__(self, name, packet, start):
        self.name, self.packet, self.anchor = name, packet, start
        self.end = None  # m_AppEnd: its place in the copy (None: none yet)
        self.lead, self.other_valid, self.no_slot = 0, False, False
        self.out = []  # every frame it delivered: a ring position, or None (silence)
        self.skipped = 0  # HandoffSkippedFrames counted when it took the slot
        self.counted = []  # its counted skips in order: ("handoff" or "secondary", frames)


class Ring:
    def __init__(self):
        self.reader, self.read_frame, self.attach_passes = None, 0, 0
        self.valid, self.hist_from, self.hist_end, self.lead = False, 0, 0, 0
        self.copy, self.gaps = {}, []  # time-line frame -> ring position; the recorded gaps [from, end)

    def gap_frames(self, lo, hi):
        return sum(max(0, min(hi, e) - max(lo, f)) for f, e in self.gaps)

    def held_frames(self, lo, hi):
        """The copy's frames in [lo, hi) from its valid start to its end, less its recorded gaps (as the driver counts a skip)."""
        lo, hi = max(lo, self.hist_from), min(hi, self.hist_end)
        return hi - lo - self.gap_frames(lo, hi) if self.valid and hi > lo else 0

    def history_write(self, at, frames):
        """AppRingHistoryWrite: frames (ring positions, None for silence) at time-line frame `at`, on from the copy's end."""
        if not self.valid or at > self.hist_end + MAX_GAP:
            self.valid, self.hist_from, self.hist_end, self.gaps = True, at, at, []
        if at > self.hist_end:
            self.gaps.append((self.hist_end, at))
            self.hist_end = at
        for i, f in enumerate(frames):
            if at + i >= self.hist_end:
                self.copy[at + i] = f
        self.hist_end = max(self.hist_end, at + len(frames))

    def history_read(self, t):
        if self.valid and self.hist_from <= t < self.hist_end and not any(f <= t < e for f, e in self.gaps):
            return self.copy.get(t)
        return None


def play_copy(ring, s, start, n):
    """AppRingPlayCopy: n frames of the copy from `start`, silence before the stream's place; the place then reaches the end."""
    out = [None if (s.end is not None and t < s.end) else ring.history_read(t) for t in range(start, start + n)]
    s.end = start + n if s.end is None else max(s.end, start + n)
    return out


def other_pass(ring, s, frame, jump, pausing_keeps):
    """AppRingOtherPass. `jump`: the driver now (a shorter reader packet is taken at once, the difference skipped once and
    counted); False: a66d8ab to e801320 (the longest packet kept). `pausing_keeps`: a pausing stream keeps its lag (now)."""
    lead = ring.lead
    shorter = s.other_valid and lead < s.lead
    if shorter and (not jump or (s.no_slot and pausing_keeps)):
        lead, shorter = s.lead, False
    start = frame - LAG - lead
    if shorter:
        lo = max(start - (s.lead - lead), s.end)
        if start > lo:
            s.counted.append(("secondary", ring.held_frames(lo, start)))
    s.lead = lead
    s.out += play_copy(ring, s, start, s.packet)
    s.other_valid = True


def capture_pass(ring, s, k, jump=True, pausing_keeps=True):
    """AppRingCapturePass for stream s's packet k (the ring always attached, every stream in its format)."""
    frame = s.anchor + k * s.packet
    if ring.reader is None and not s.no_slot:
        ring.reader = s
        if ring.attach_passes and s.other_valid and ring.valid:  # a handoff: its counted skip
            skipped = ring.held_frames(s.end, frame)
            s.skipped += skipped
            s.counted.append(("handoff", skipped))
        s.other_valid = False
    elif ring.reader is not s:
        other_pass(ring, s, frame, jump, pausing_keeps)
        return
    ring.attach_passes += 1
    held = min(ring.hist_end - frame, s.packet) if ring.valid and frame < ring.hist_end else 0
    out = play_copy(ring, s, frame, held) if held else []
    fresh = list(range(ring.read_frame, ring.read_frame + s.packet - held))
    ring.history_write(frame + held, fresh)
    ring.read_frame += len(fresh)
    ring.lead = s.packet
    s.end = frame + s.packet if s.end is None else max(s.end, frame + s.packet)
    s.out += out + fresh


def run(packets, starts, stop_at, stall, release_first, rng, jump=True, pause=20, pausing_keeps=True):
    """The first of `packets` reads; the others start at `starts`; the reader's Pause comes at `stop_at`. With release_first
    (the driver now) it gives the slot up then and its timer stops `pause` frames later (a pass in between plays the copy);
    otherwise its timer stops then and the slot is given up `stall` frames later, when WdfTimerStop's wait ends (the driver
    before). The others run to the end."""
    ring = Ring()
    streams = [Stream(f"s{i}", p, t) for i, (p, t) in enumerate(zip(packets, starts))]
    end_time = stop_at + 3000
    events = []
    for s in streams:
        for k in range((end_time - s.anchor) // s.packet):
            at = s.anchor + (k + 1) * s.packet + rng.randint(0, LATE_MAX)
            if s is streams[0] and at > stop_at + (pause if release_first else 0):
                break  # the timer stopped
            events.append((at, 1, s.name, k, s))
    release = stop_at if release_first else stop_at + stall
    events.append((release, 0, "", 0, None))  # sorts before a pass at the same time
    for _at, kind, _name, k, s in sorted(events, key=lambda e: e[:4]):
        if kind == 0:  # AppRingReaderStop
            streams[0].no_slot = True
            if ring.reader is streams[0]:
                ring.reader = None
        else:
            capture_pass(ring, s, k, jump, pausing_keeps)
    return streams


def faults(s):
    """(repeats, the frames each gap lost in order) in a stream's output, by ring position."""
    codes = [f for f in s.out if f is not None]
    steps = [b - a for a, b in zip(codes, codes[1:])]
    return sum(d <= 0 for d in steps), [d - 1 for d in steps if d > 1]


def handoffs(release_first, stall, runs=200, seed=7, **kw):
    """Runs A (a 160-frame reader, then 480 and 240) and B (a 480-frame reader, then 160 and 240): per stream, its faults,
    whether it was the first reader, and the run's case."""
    rng = random.Random(seed)
    results = []
    for i in range(runs):
        case = "A" if i % 2 == 0 else "B"
        packets = [160, 480, 240] if case == "A" else [480, 160, 240]
        starts = [0] + [rng.randint(500, 1500) for _ in packets[1:]]
        stop_at = rng.randint(4000, 8000)
        streams = run(packets, starts, stop_at, stall, release_first, rng, **kw)
        results += [(s, faults(s), s is streams[0], case, streams) for s in streams]
    return results


class AppRingModel(unittest.TestCase):
    def check_rules(self, results):
        """No repeats; every gap one of the stream's counted skips with frames, in order, to the frame. Returns the takers
        and the streams that skipped on a shorter lag."""
        takers = skippers = 0
        for s, (repeats, gaps), _first, _case, _streams in results:
            self.assertEqual(repeats, 0, f"{s.name} ({s.packet}) had frames twice")
            self.assertEqual(gaps, [f for _, f in s.counted if f], f"{s.name} ({s.packet}): each gap is a counted skip")
            takers += any(kind == "handoff" for kind, _ in s.counted)
            skippers += any(kind == "secondary" and f for kind, f in s.counted)
        return takers, skippers

    def test_the_rules_hold_through_a_handoff(self):
        # The driver now: the slot goes before the timer stops, however long the stop takes.
        results = handoffs(release_first=True, stall=400)
        takers, skippers = self.check_rules(results)
        self.assertGreater(takers, 150, "most runs hand off with a counted skip")
        full = 0
        for s, (_, gaps), first, case, streams in results:
            taker = next((t for t in streams if any(kind == "handoff" for kind, _ in t.counted)), None)
            if first or taker is None or s is taker:
                continue
            if case == "A":
                # Behind a longer packet it moves back (counted silence): no skip, no gap.
                self.assertEqual((s.counted, gaps), ([], []), f"A: {s.name} ({s.packet})")
            else:
                # Behind a shorter one it moves on at once: one skip, counted, of the packets' difference at most (less any
                # gap the handoff left in the copy there), and its lag is the new reader's packet from then on.
                [(kind, frames)] = s.counted
                self.assertEqual(kind, "secondary", f"B: {s.name} ({s.packet})")
                self.assertTrue(0 <= frames <= 480 - taker.packet, frames)
                self.assertEqual(s.lead, taker.packet)
                full += frames == 480 - taker.packet
        self.assertGreater(skippers, 75, "most B runs' stream that plays on skips once, counted")
        self.assertGreater(full, 50, "most of them the whole difference")

    def test_the_longer_lag_kept_before(self):
        # a66d8ab to e801320: behind a shorter packet the stream that plays on kept the longer lag, no gap and nothing counted.
        results = handoffs(release_first=True, stall=400, jump=False)
        _, skippers = self.check_rules(results)
        self.assertEqual(skippers, 0)
        kept = [s for s, _, first, case, _ in results if case == "B" and not first and not s.skipped]
        self.assertTrue(kept and all(s.lead == 480 for s in kept), "a packet more latency to the end")

    def test_a_pausing_stream_keeps_its_lag(self):
        # A stopping reader whose timer runs 1,000 frames past its Pause (two 480-frame passes playing the copy) keeps its own
        # lag: its client is stopping, and a skip counted in the window of the streams that play on would break verify's
        # match of their gaps to the count. Taking the shorter lag there would count skips for it. (Its own passes that late
        # can read past the copy's end while the slot changes hands, and lose what the taker then writes there, as any
        # stream's read past the copy's end does: nothing reaches its stopping client, and its passes count no skip.)
        for pausing_keeps in (True, False):
            results = handoffs(release_first=True, stall=0, pause=1000, pausing_keeps=pausing_keeps)
            self.check_rules([r for r in results if not r[2]])
            first = [(s, f, case) for s, f, is_first, case, _ in results if is_first]
            self.assertTrue(all(f[0] == 0 for _, f, _ in first), "the stopping reader repeats nothing")
            counted = [s for s, _, case in first if s.counted]
            if pausing_keeps:
                self.assertEqual(counted, [], "the stopping reader counts nothing")
            else:
                self.assertTrue(counted, "it would count skips")

    def test_a_slot_held_while_the_timer_stops_loses_frames(self):
        # The driver before 2026-11-23: with the slot held 400 frames (8.3 ms) after the reader's timer stopped (the live
        # runs: 273 to 460 frames after its last pass at least, from a stream that played on reading 64 to 251 frames past
        # the copy's end), a stream that plays on loses frames nothing counts, and still gets none twice.
        results = handoffs(release_first=False, stall=400, jump=False)
        lost = [(s.packet, sum(f[1])) for s, f, _, _, _ in results if not s.skipped and f[1]]
        self.assertTrue(lost, "the stall loses frames")
        self.assertTrue(all(f[0] == 0 for _, f, _, _, _ in results), "but repeats none")
        # With no stall to speak of (the slot given up within the lateness), the old order keeps the rules too.
        self.check_rules(handoffs(release_first=False, stall=0))


if __name__ == "__main__":
    unittest.main()
