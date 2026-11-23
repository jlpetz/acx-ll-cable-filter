"""An offline model of the loopback's render lead (driver/src/Common/StreamEngine.cpp: LoopbackRenderPass, LoopbackCapturePass;
TODO 19): one or two render streams and a capture stream, frames numbered per render stream, time in frames on the QPC time
line, each pass's timer late by up to 39 frames (817 us, the A1 session's maximum in TODO 19's spike notes). It checks that
the capture stream never gets a frame twice, and that every render frame it loses within a render stream's run (a gap, or the
run's first frames) is one of its counted skips, to the frame: the driver now takes a fall in the lead at once and skips the
difference once, counted (LeadSkips, LeadSkippedFrames), following the owner's app-ring decision of 2026-11-23 (lower latency
over a one-time skip). The older rules are kept for comparison: 0c03e29 to e801320 kept a longer lead through a fall within
the run that set it and took a lead set in a new run at once; 5de56b9 kept it across runs too (Astra's review of 5de56b9: an
overlapping render stream that claimed the ring as the owner stopped was held a packet behind for its whole run). Left out:
the ring's length, glitch adjustments, a Pause and Run, formats. Run from any directory:
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
        self.run_first = None  # its frame number at its run's start (ValidFrom)

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
            self.anchor_valid, self.run_first = True, self.next_packet * self.packet
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
    """A capture stream anchored at 0. `rule`: "jump", the driver now (a fall taken at once, the skip counted); "run", 0c03e29
    to e801320 (a longer lead kept through a fall within the run that set it); "keep", 5de56b9 (kept across runs too)."""

    def __init__(self, packet, rule):
        self.packet, self.rule = packet, rule
        self.anchor_valid, self.anchor_lead, self.anchor_run, self.base = False, 0, 0, 0
        self.out = []  # every frame delivered: (render stream, frame number), or None (silence)
        self.leads = []  # (packet, the lead it read behind)
        self.skips = []  # the counted skips, "jump" only: (the render stream whose audio it passed over, or None; frames)

    def run_pass(self, ring, k):
        """LoopbackCapturePass for packet k."""
        hi, lo = ring.write_pos, ring.valid_from
        lead = ring.lead
        if self.anchor_valid and lead < self.anchor_lead:
            old = -LAG - self.anchor_lead + k * self.packet
            new = old + (self.anchor_lead - lead)
            skip_lo, skip_hi = max(old, lo), min(new, hi)
            if self.rule == "jump":
                frames = max(0, skip_hi - skip_lo)
                self.skips.append((ring.data[skip_lo][0] if frames else None, frames))
            elif (self.rule == "keep" or ring.lead_run == self.anchor_run) and skip_hi > skip_lo:
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


def simulate(rule, rng, a_early_from, b_start, a_stop, preroll=True, end=16000):
    """Render stream A (480 frames, from frame 1000) owns the ring until `a_stop`; B (480 frames, from `b_start`, its packets
    released early from the start) claims it at its first pass after that; a 480-frame capture stream reads throughout.
    Returns the capture stream and the render streams."""
    ring = Ring()
    a = Render("A", 480, 1000, a_early_from, preroll)
    b = Render("B", 480, b_start, 0, preroll)
    c = Capture(480, rule)
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
    return c, (a, b)


def faults(c):
    """(repeats, gaps) within each render stream's frames as the capture stream delivered them."""
    frames = [f for f in c.out if f is not None]
    repeats = sum(a[0] == b[0] and b[1] <= a[1] for a, b in zip(frames, frames[1:]))
    gaps = sum(a[0] == b[0] and b[1] > a[1] + 1 for a, b in zip(frames, frames[1:]))
    return repeats, gaps


def losses(c, render):
    """The frames of `render`'s run the capture stream lost, in order: its run's first frames before the first it delivered,
    then each gap within its frames. Its tail after the last frame delivered (cut off by a new owner's claim) isn't one."""
    mine = [f[1] for f in c.out if f is not None and f[0] == render.name]
    if not mine:
        return []
    lost = [mine[0] - render.run_first] if mine[0] > render.run_first else []
    return lost + [b - a - 1 for a, b in zip(mine, mine[1:]) if b > a + 1]


def first_of(c, name):
    """The first frame number of render stream `name` the capture stream delivered (its start's skip)."""
    return next(f[1] for f in c.out if f is not None and f[0] == name)


class LoopbackLeadModel(unittest.TestCase):
    RUNS = 150

    def runs(self, rule, seed, **kw):
        rng = random.Random(seed)
        return [simulate(rule, rng, **kw) for _ in range(self.RUNS)]

    def overlapping(self, rule):
        # A copies as packets complete (a packet's lead) and stops; B, running alongside with its packets released early (no
        # lead), claims the ring at its next pass.
        rng = random.Random(3)
        return [
            simulate(rule, rng, a_early_from=10**9, b_start=rng.randint(1500, 4000), a_stop=rng.randint(6000, 9000))
            for _ in range(self.RUNS)
        ]

    def assert_clean(self, runs):
        for c, _ in runs:
            self.assertEqual(faults(c), (0, 0), "no frame twice, none lost within a render stream's run")

    def assert_counted(self, runs):
        """The driver now: no frame twice, and each render stream's losses are its counted skips with frames, in order."""
        for c, renders in runs:
            self.assertEqual(faults(c)[0], 0, "no frame twice")
            for r in renders:
                self.assertEqual(losses(c, r), [f for name, f in c.skips if name == r.name], f"render stream {r.name}")

    def test_an_overlapping_owner_gets_its_own_lead(self):
        for rule in ("jump", "run"):
            runs = self.overlapping(rule)
            self.assert_clean(runs)
            self.assertEqual(sum(c.leads[-1][1] != 0 for c, _ in runs), 0, "B's run read behind its own lead, 0")
            if rule == "jump":
                self.assert_counted(runs)
                lost = [f for c, _ in runs for name, f in c.skips if name == "B"]
                self.assertTrue(lost and max(lost) < 480, "B's first frames, counted, less than a packet")
        # 5de56b9: the keep's test saw B's audio in the span it would skip, and held B's whole run a packet behind.
        before = self.overlapping("keep")
        self.assert_clean(before)
        self.assertGreater(sum(c.leads[-1][1] == 480 for c, _ in before), self.RUNS // 2)

    def test_a_fall_within_a_run_skips_once(self):
        # A's first three packets copied as they complete, then released early: the lead falls within A's run. The driver
        # now takes the shorter lead at once and skips the difference once, a packet of A's audio, counted.
        runs = self.runs("jump", 5, a_early_from=3, b_start=10**6, a_stop=14000)
        self.assert_counted(runs)
        for c, (a, _) in runs:
            self.assertEqual(c.lead_at(10000), 0)
            self.assertEqual(losses(c, a), [480], "one gap of the difference in leads")
            self.assertEqual(first_of(c, "A"), 0, "A's start never skipped")
        # 0c03e29 to e801320, and 5de56b9: the capture stream kept the packet rather than skip, a packet more latency.
        for rule in ("run", "keep"):
            runs = self.runs(rule, 5, a_early_from=3, b_start=10**6, a_stop=14000)
            self.assert_clean(runs)
            self.assertTrue(all(c.lead_at(10000) == 480 for c, _ in runs))
            self.assertTrue(all(first_of(c, "A") == 0 for c, _ in runs), "A's start never skipped")

    def test_a_new_owner_after_a_gap(self):
        rng = random.Random(9)
        for preroll in (True, False):
            for rule in ("jump", "run", "keep"):
                runs = []
                for _ in range(self.RUNS):
                    a_stop = rng.randint(4000, 6000)
                    runs.append(simulate(rule, rng, a_early_from=10**9, b_start=a_stop + rng.randint(2000, 4000), a_stop=a_stop,
                                         preroll=preroll))
                self.assert_clean(runs)
                kept = sum(c.leads[-1][1] != 0 for c, _ in runs)
                skips = [first_of(c, "B") for c, _ in runs]
                if rule == "jump":
                    self.assert_counted(runs)
                if rule != "keep" or preroll:
                    # B's lead taken at once. With its pre-roll copied at Run, the capture stream's read hasn't reached B's
                    # start yet, so nothing is skipped; with its first copy a pass later, B's start is skipped, by at most
                    # the difference in leads (a packet): counted now.
                    self.assertEqual(kept, 0)
                    self.assertTrue(all(s < 480 for s in skips), max(skips))
                    if preroll:
                        self.assertTrue(all(s == 0 for s in skips))
                    else:
                        self.assertGreater(sum(s > 0 for s in skips), self.RUNS // 2)
                else:
                    # 5de56b9 without a pre-roll: B's start kept, and its whole run a packet behind, in most runs.
                    self.assertGreater(kept, self.RUNS // 2)
                    self.assertTrue(all(s == 0 for s in skips))


if __name__ == "__main__":
    unittest.main()
