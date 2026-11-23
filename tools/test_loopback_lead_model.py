"""An offline model of the loopback's render lead (driver/src/Common/StreamEngine.cpp: LoopbackRenderPass, LoopbackCapturePass;
TODO 19): one or two render streams and a capture stream, frames numbered per render stream, time in frames on the QPC time
line, each pass's timer late by up to 39 frames (817 us, the A1 session's maximum in TODO 19's spike notes). It checks that
the capture stream never gets a frame twice or loses one within a render stream's run, keeps a longer lead through a fall
within one run, and takes a lead set in a new run at once (Astra's review of 5de56b9: an overlapping render stream that
claimed the ring as the owner stopped was held a packet behind for its whole run). Left out: the ring's length, glitch
adjustments, a Pause and Run, formats. Run from any directory:
    python tools/test_loopback_lead_model.py
"""
import random
import unittest

LAG = 49  # LOOPBACK_LAG_HNS (1 ms) in frames, plus one
LATE_MAX = 39  # 817 us


class Ring:
    def __init__(self):
        self.owner, self.valid_from, self.write_pos = None, 0, 0
        self.lead, self.lead_run, self.restarts = 0, 0, 0
        self.data = {}  # time-line frame -> (render stream, its frame number)


class Render:
    """A render stream: packets of `packet` frames from `anchor`; its client releases packets early from pass `early_from` on,
    and at Run (the pre-roll) too if `preroll`."""

    def __init__(self, name, packet, anchor, early_from, preroll):
        self.name, self.packet, self.anchor, self.early_from, self.preroll = name, packet, anchor, early_from, preroll
        self.next_packet, self.anchor_valid = 0, False

    def run_pass(self, ring, k, at_run):
        """LoopbackRenderPass: at Run (k = 0), or the pass completing packet k."""
        if ring.owner is None:  # claim the ring: a new run
            ring.owner, ring.valid_from, ring.write_pos = self, 0, 0
            self.anchor_valid = False
        elif ring.owner is not self:
            return  # RenderNotOwner
        early = (self.preroll and k >= self.early_from) if at_run else k >= self.early_from
        end = (k + 1 if early else k) if at_run else (k + 2 if early else k + 1)
        if not self.anchor_valid:  # a new run at this stream's current packet
            self.next_packet = max(self.next_packet, k)
            ring.valid_from = ring.write_pos = self.anchor + self.next_packet * self.packet
            ring.restarts += 1
            self.anchor_valid = True
        copied = False
        while end > self.next_packet:
            for i in range(self.packet):
                ring.data[ring.write_pos + i] = (self.name, self.next_packet * self.packet + i)
            ring.write_pos += self.packet
            self.next_packet += 1
            copied = True
        if copied:
            ring.lead = 0 if early else self.packet
            ring.lead_run = ring.restarts


class Capture:
    """A capture stream anchored at 0; `by_run` is the driver now (a longer lead kept only through a fall within its run),
    False the driver at 5de56b9."""

    def __init__(self, packet, by_run):
        self.packet, self.by_run = packet, by_run
        self.anchor_valid, self.anchor_lead, self.anchor_run, self.base = False, 0, 0, 0
        self.out = []  # every frame delivered: (render stream, frame number), or None (silence)
        self.leads = []  # (packet, the lead it read behind)

    def run_pass(self, ring, k):
        """LoopbackCapturePass for packet k."""
        hi, lo = ring.write_pos, ring.valid_from
        lead = ring.lead
        if self.anchor_valid and lead < self.anchor_lead and (ring.lead_run == self.anchor_run or not self.by_run):
            kept = -LAG - self.anchor_lead + k * self.packet
            short = kept + (self.anchor_lead - lead)
            if min(short, hi) > max(kept, lo):
                lead = self.anchor_lead
        if not self.anchor_valid or lead != self.anchor_lead:
            self.anchor_lead, self.base, self.anchor_valid = lead, -LAG - lead, True
        self.anchor_run = ring.lead_run
        start = self.base + k * self.packet
        self.out += [ring.data.get(t) if lo <= t < hi else None for t in range(start, start + self.packet)]
        self.leads.append((k, self.anchor_lead))

    def lead_at(self, frame):
        """The lead of its first pass at or after time-line frame `frame`."""
        return next(lead for k, lead in self.leads if k * self.packet >= frame)


def simulate(by_run, rng, a_early_from, b_start, a_stop, preroll=True, end=16000):
    """Render stream A (480 frames, from frame 1000) owns the ring until `a_stop`; B (480 frames, from `b_start`, its packets
    released early from the start) claims it at its first pass after that; a 480-frame capture stream reads throughout."""
    ring = Ring()
    a = Render("A", 480, 1000, a_early_from, preroll)
    b = Render("B", 480, b_start, 0, preroll)
    c = Capture(480, by_run)
    events = []  # (time, order at the same time, stream, packet, at Run)
    for s in (a, b):
        events.append((s.anchor, 0, s, 0, True))
        for k in range((end - s.anchor) // s.packet):
            at = s.anchor + (k + 1) * s.packet + rng.randint(0, LATE_MAX)
            if s is a and at >= a_stop:
                break  # its timer stopped
            events.append((at, 2, s, k, False))
    events.append((a_stop, 1, None, 0, False))  # A's ReleaseHardware gives the ring up
    events += [((k + 1) * c.packet + rng.randint(0, LATE_MAX), 2, c, k, False) for k in range(end // c.packet)]
    for _at, _order, s, k, at_run in sorted(events, key=lambda e: (e[0], e[1])):
        if s is None:
            if ring.owner is a:
                ring.owner = None
        elif s is c:
            c.run_pass(ring, k)
        else:
            s.run_pass(ring, k, at_run)
    return c


def faults(c):
    """(repeats, gaps) within each render stream's frames as the capture stream delivered them."""
    frames = [f for f in c.out if f is not None]
    repeats = sum(a[0] == b[0] and b[1] <= a[1] for a, b in zip(frames, frames[1:]))
    gaps = sum(a[0] == b[0] and b[1] > a[1] + 1 for a, b in zip(frames, frames[1:]))
    return repeats, gaps


def first_of(c, name):
    """The first frame number of render stream `name` the capture stream delivered (its start's skip)."""
    return next(f[1] for f in c.out if f is not None and f[0] == name)


class LoopbackLeadModel(unittest.TestCase):
    RUNS = 150

    def runs(self, by_run, seed, **kw):
        rng = random.Random(seed)
        return [simulate(by_run, rng, **kw) for _ in range(self.RUNS)]

    def overlapping(self, by_run):
        # A copies as packets complete (a packet's lead) and stops; B, running alongside with its packets released early (no
        # lead), claims the ring at its next pass.
        rng = random.Random(3)
        return [
            simulate(by_run, rng, a_early_from=10**9, b_start=rng.randint(1500, 4000), a_stop=rng.randint(6000, 9000))
            for _ in range(self.RUNS)
        ]

    def assert_clean(self, captures):
        for c in captures:
            self.assertEqual(faults(c), (0, 0), "no frame twice, none lost within a render stream's run")

    def test_an_overlapping_owner_gets_its_own_lead(self):
        now = self.overlapping(by_run=True)
        self.assert_clean(now)
        self.assertEqual(sum(c.leads[-1][1] != 0 for c in now), 0, "B's run read behind its own lead, 0")
        # 5de56b9: the keep's test saw B's audio in the span it would skip, and held B's whole run a packet behind.
        before = self.overlapping(by_run=False)
        self.assert_clean(before)
        self.assertGreater(sum(c.leads[-1][1] == 480 for c in before), self.RUNS // 2)

    def test_a_fall_within_a_run_keeps_the_lead(self):
        # A's first three packets copied as they complete, then released early: the lead falls within A's run, and the
        # capture stream keeps the packet rather than skip the start of A's audio, both before and now.
        for by_run in (True, False):
            captures = self.runs(by_run, 5, a_early_from=3, b_start=10**6, a_stop=14000)
            self.assert_clean(captures)
            self.assertTrue(all(c.lead_at(10000) == 480 for c in captures))
            self.assertTrue(all(first_of(c, "A") == 0 for c in captures), "A's start never skipped")

    def test_a_new_owner_after_a_gap(self):
        rng = random.Random(9)
        for preroll in (True, False):
            for by_run in (True, False):
                captures = []
                for _ in range(self.RUNS):
                    a_stop = rng.randint(4000, 6000)
                    captures.append(simulate(by_run, rng, a_early_from=10**9, b_start=a_stop + rng.randint(2000, 4000),
                                             a_stop=a_stop, preroll=preroll))
                self.assert_clean(captures)
                kept = sum(c.leads[-1][1] != 0 for c in captures)
                skips = [first_of(c, "B") for c in captures]
                if by_run or preroll:
                    # B's lead taken at once. With its pre-roll copied at Run, the capture stream's read hasn't reached B's
                    # start yet, so nothing is skipped; with its first copy a pass later, B's start is skipped, by at most
                    # the difference in leads (a packet).
                    self.assertEqual(kept, 0)
                    self.assertTrue(all(s < 480 for s in skips), max(skips))
                    if preroll:
                        self.assertTrue(all(s == 0 for s in skips))
                else:
                    # 5de56b9 without a pre-roll: B's start kept, and its whole run a packet behind, in most runs.
                    self.assertGreater(kept, self.RUNS // 2)
                    self.assertTrue(all(s == 0 for s in skips))


if __name__ == "__main__":
    unittest.main()
