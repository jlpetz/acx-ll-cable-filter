"""An offline model of the driver's app-ring capture passes (driver/src/Common/StreamEngine.cpp: AppRingCapturePass,
AppRingOtherPass, AppRingPlayCopy, AppRingHistoryWrite; TODO 19), checking the ring's rules for three capture streams
through a reader handoff: no stream gets a ring frame twice, a stream that plays on loses none, and the one that takes the
slot loses exactly its counted skip (HandoffSkippedFrames). Frames are numbered by the ring's position, time in frames on
the QPC time line; the app always has frames ready (no underruns), and each pass's timer is late by up to 817 us, the
maximum in the A1 session's live runs (the later session's wasn't read). Left out: the copy's 32,768-frame length and its
32-gap record, glitch adjustments, a Pause and Run of one stream.

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


class Ring:
    def __init__(self):
        self.reader, self.read_frame, self.attach_passes = None, 0, 0
        self.valid, self.hist_from, self.hist_end, self.lead = False, 0, 0, 0
        self.copy, self.gaps = {}, []  # time-line frame -> ring position; the recorded gaps [from, end)

    def gap_frames(self, lo, hi):
        return sum(max(0, min(hi, e) - max(lo, f)) for f, e in self.gaps)

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


def capture_pass(ring, s, k):
    """AppRingCapturePass for stream s's packet k (the ring always attached, every stream in its format)."""
    frame = s.anchor + k * s.packet
    if ring.reader is None and not s.no_slot:
        ring.reader = s
        if ring.attach_passes and s.other_valid and ring.valid:  # a handoff: its counted skip
            start = max(s.end, ring.hist_from)
            end = min(frame, ring.hist_end)
            if end > start:
                s.skipped += end - start - ring.gap_frames(start, end)
        s.other_valid = False
    elif ring.reader is not s:  # AppRingOtherPass
        if not s.other_valid or ring.lead > s.lead:
            s.lead = ring.lead
        s.out += play_copy(ring, s, frame - LAG - s.lead, s.packet)
        s.other_valid = True
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


def run(packets, starts, stop_at, stall, release_first, rng):
    """The first of `packets` reads; the others start at `starts`; the reader's Pause comes at `stop_at`. With release_first
    (the driver now) it gives the slot up then and its timer stops 20 frames later (a pass in between plays the copy);
    otherwise its timer stops then and the slot is given up `stall` frames later, when WdfTimerStop's wait ends (the driver
    before). The others run to the end."""
    ring = Ring()
    streams = [Stream(f"s{i}", p, t) for i, (p, t) in enumerate(zip(packets, starts))]
    end_time = stop_at + 3000
    events = []
    for s in streams:
        for k in range((end_time - s.anchor) // s.packet):
            at = s.anchor + (k + 1) * s.packet + rng.randint(0, LATE_MAX)
            if s is streams[0] and at > stop_at + (20 if release_first else 0):
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
            capture_pass(ring, s, k)
    return streams


def faults(s):
    """(repeats, gaps, frames lost) in a stream's output, by ring position."""
    codes = [f for f in s.out if f is not None]
    steps = [b - a for a, b in zip(codes, codes[1:])]
    return sum(d <= 0 for d in steps), sum(d > 1 for d in steps), sum(d - 1 for d in steps if d > 1)


def handoffs(release_first, stall, runs=200, seed=7):
    """Runs A (a 160-frame reader, then 480 and 240) and B (a 480-frame reader, then 160 and 240): per stream, its faults."""
    rng = random.Random(seed)
    results = []
    for i in range(runs):
        packets = [160, 480, 240] if i % 2 == 0 else [480, 160, 240]
        starts = [0] + [rng.randint(500, 1500) for _ in packets[1:]]
        stop_at = rng.randint(4000, 8000)
        streams = run(packets, starts, stop_at, stall, release_first, rng)
        results += [(s, faults(s), s is streams[0]) for s in streams]
    return results


class AppRingModel(unittest.TestCase):
    def check_rules(self, results):
        takers = 0
        for s, (repeats, gaps, lost), first in results:
            self.assertEqual(repeats, 0, f"{s.name} ({s.packet}) had frames twice")
            if s.skipped:
                takers += 1
                self.assertEqual((gaps, lost), (1, s.skipped), f"the taker {s.name} ({s.packet}): its gap is its counted skip")
            else:
                self.assertEqual(gaps, 0, f"{s.name} ({s.packet}) played on and lost {lost} frames")
        return takers

    def test_the_rules_hold_through_a_handoff(self):
        # The driver now: the slot goes before the timer stops, however long the stop takes.
        takers = self.check_rules(handoffs(release_first=True, stall=400))
        self.assertGreater(takers, 150, "most runs hand off with a counted skip")

    def test_a_slot_held_while_the_timer_stops_loses_frames(self):
        # The driver before 2026-11-23: with the slot held 400 frames (8.3 ms) after the reader's timer stopped (the live
        # runs: 273 to 460 frames after its last pass at least, from a stream that played on reading 64 to 251 frames past
        # the copy's end), a stream that plays on loses frames nothing counts, and still gets none twice.
        results = handoffs(release_first=False, stall=400)
        lost = [(s.packet, f[2]) for s, f, _ in results if not s.skipped and f[1]]
        self.assertTrue(lost, "the stall loses frames")
        self.assertTrue(all(f[0] == 0 for _, f, _ in results), "but repeats none")
        # With no stall to speak of (the slot given up within the lateness), the old order keeps the rules too.
        self.check_rules(handoffs(release_first=False, stall=0))


if __name__ == "__main__":
    unittest.main()
