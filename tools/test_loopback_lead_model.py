"""An offline model of the loopback's render lead (driver/src/Common/StreamEngine.cpp: LoopbackRenderPass, LoopbackCapturePass;
TODO 19): one or two render streams and a capture stream, frames numbered per render stream, time in frames on the QPC time
line, each pass's timer late by up to 39 frames (817 us, the A1 session's maximum in TODO 19's spike notes). It checks that
the capture stream never gets a frame twice, and that every render frame it loses within a render stream's run (a gap, or the
run's first frames) is one of its counted skips, to the frame: the driver now takes a fall in the lead at once and skips the
difference once, counted (LeadSkips, LeadSkippedFrames), following the owner's app-ring decision of 2026-11-23 (lower latency
over a one-time skip). The older rules are kept for comparison: 0c03e29 to e801320 kept a longer lead through a fall within
the run that set it and took a lead set in a new run at once; 5de56b9 kept it across runs too (Astra's review of 5de56b9: an
overlapping render stream that claimed the ring as the owner stopped was held a packet behind for its whole run).
Separate cases exercise startup, Pause, resume, two-slot publication and exact end of stream. The model omits the finite
ring capacity, timer glitch adjustments and sample format negotiation. Run from any directory with:
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
        self.eos = None  # exclusive absolute frame from the current owner's explicit EOS
        self.pause_end = None  # frame at Pause entry; already-published audio can still drain beyond it
        self.active = False
        self.data = {}  # time-line frame -> (render stream, its frame number)


class Render:
    """A render stream: packets of `packet` frames from `anchor`; its client releases packets early from pass `early_from` on,
    and at Run (the pre-roll) too if `preroll`."""

    def __init__(self, name, packet, anchor, early_from, preroll):
        self.name, self.packet, self.anchor, self.early_from, self.preroll = name, packet, anchor, early_from, preroll
        self.next_packet, self.anchor_valid = 0, False
        self.run_first = None  # its frame number at its run's start (ValidFrom)
        self.eos = None  # exclusive stream-relative frame
        self.released = -1
        self.submitted = set()  # exact explicit submissions; a notification for P+1 cannot authenticate a hole at P
        self.current = 0
        self.pause_slots = None
        self.late_copies = 0

    def release(self, ring, current, packet, *, state="run", eos_frames=None):
        if state not in ("pause", "run"):
            return "stopped"
        if self.eos is not None:
            return "ended"
        if packet < current + (state == "run"):
            if state == "run" and packet == current and eos_frames is None:
                self.submitted.add(packet)  # ordinary current packet is late, but authentic until its DMA half is reused
            return "late"
        if packet > current + 1:
            return "overrun"
        if eos_frames is not None:
            if packet < self.released:
                return "late"
            if not 0 <= eos_frames <= self.packet:
                return "invalid"
            self.set_eos(ring, packet, eos_frames)
        self.submitted.add(packet)
        self.released = max(self.released, packet)
        return "ok"

    def begin_pause(self, ring, current, cutoff):
        if ring.owner is self:
            ring.pause_end = cutoff
            self.current = current
            self.pause_slots = self.submitted.intersection((current, current + 1))

    def release_published(self, ring, current, packet, payload, *, state="run"):
        """A second ordinary notification revises the bytes at their published frame addresses; readers keep their cursors."""
        status = self.release(ring, current, packet, state=state)
        start = self.anchor + packet * self.packet
        if (state == "run" and status in ("ok", "late") and current <= packet <= current + 1
                and ring.owner is self and self.anchor_valid and ring.active and ring.pause_end is None
                and ring.eos is None and ring.valid_from <= start and start + len(payload) <= ring.write_pos):
            ring.data.update((start + i, sample) for i, sample in enumerate(payload))
        return status

    def copy_released(self, ring, demand):
        if self.pause_slots is not None:
            demand = min(demand, ring.pause_end)
        while ring.owner is self and self.anchor_valid and ring.write_pos < demand:
            packet = self.next_packet
            if packet not in (self.current, self.current + 1) or packet not in self.submitted:
                break
            if self.pause_slots is not None and packet not in self.pause_slots:
                break
            self.late_copies += int(packet == self.current)
            self.copy_next(ring)

    def copy_next(self, ring):
        start = self.next_packet * self.packet
        frames = self.packet if self.eos is None else max(0, min(self.packet, self.eos - start))
        for i in range(frames):
            ring.data[ring.write_pos + i] = (self.name, start + i)
        ring.write_pos += frames
        self.next_packet += 1

    def set_eos(self, ring, packet, frames):
        self.eos = packet * self.packet + frames
        if ring.owner is self and self.anchor_valid:
            ring.eos = self.anchor + self.eos

    def run_pass(self, ring, k, at_run):
        """LoopbackRenderPass: at Run (k = 0), or the pass completing packet k."""
        if ring.owner is None:  # claim the ring: a new run
            ring.owner, ring.valid_from, ring.write_pos = self, 0, 0
            ring.eos, ring.pause_end, ring.active = None, None, False
            self.anchor_valid = False
        elif ring.owner is not self:
            return  # RenderNotOwner
        self.current = k
        if at_run:
            ring.pause_end = None
            self.pause_slots = None
        early = (self.preroll and k >= self.early_from) if at_run else k >= self.early_from
        end = (k + 1 if early else k) if at_run else (k + 2 if early else k + 1)
        if self.eos is not None:
            count = (self.eos + self.packet - 1) // self.packet
            if self.anchor_valid and self.next_packet >= count:
                return
            k, end = min(k, count), min(end, count)
        if not self.anchor_valid:  # a new run at this stream's current packet
            self.next_packet = max(self.next_packet, k)
            ring.valid_from = ring.write_pos = self.anchor + self.next_packet * self.packet
            ring.restarts += 1
            self.anchor_valid, self.run_first = True, self.next_packet * self.packet
            if self.eos is not None:
                ring.eos = self.anchor + self.eos
        copied = False
        while end > self.next_packet:
            self.copy_next(ring)
            copied = True
        if copied:
            ring.active = True
            ring.lead = 0 if early else self.packet
            ring.lead_run = ring.restarts


class Capture:
    """A capture stream anchored at 0. `rule`: "jump", the driver now (a fall taken at once, the skip counted); "run", 0c03e29
    to e801320 (a longer lead kept through a fall within the run that set it); "keep", 5de56b9 (kept across runs too)."""

    def __init__(self, packet, rule):
        self.packet, self.rule = packet, rule
        self.anchor_valid, self.anchor_lead, self.anchor_run, self.base = False, 0, 0, 0
        self.out = []  # every frame delivered: (render stream, frame number), or None (silence)
        self.underrun = 0
        self.leads = []  # (packet, the lead it read behind)
        self.skips = []  # the counted skips, "jump" only: (the render stream whose audio it passed over, or None; frames)

    def run_pass(self, ring, k):
        """LoopbackCapturePass for packet k."""
        demand = (k + 1) * self.packet - LAG - ring.lead
        if demand > ring.write_pos and ring.owner is not None and (ring.active or ring.pause_end is not None):
            ring.owner.copy_released(ring, demand)
        hi, lo = ring.write_pos, ring.valid_from
        if ring.eos is not None:
            hi = min(hi, ring.eos)
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
        due_end = start + self.packet
        if ring.eos is not None:
            due_end = min(due_end, ring.eos)
        if ring.pause_end is not None:
            due_end = min(due_end, ring.pause_end)
        if ring.active or ring.pause_end is not None:
            self.underrun += max(0, due_end - max(start, hi))
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
                ring.owner, ring.active = None, False
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

    def test_rereleased_slot_repairs_each_readers_unconsumed_fade_in_place(self):
        # WMP's observed shared startup: samples 0..64 of the slot are silent. It then submits the 415-frame fade.
        n = 480
        payload = [None] * 65 + [("fade", i) for i in range(65, n)]
        for current in (0, 1):  # a queued next packet, or a late submission of the current packet
            ring, a = Ring(), Render("A", n, 0, 0, True)
            a.run_pass(ring, 0, False)
            ring.data.update((n+i, None) for i in range(n))
            self.assertEqual((ring.valid_from, ring.write_pos, a.next_packet), (0, 2*n, 2))
            readers = [(Capture(n + LAG + used, "jump"), used) for used in (32, 95)]
            for c, used in readers:
                c.run_pass(ring, 0)
                self.assertEqual(c.out[-used:], [None] * used)
            self.assertEqual(a.release_published(ring, current, 1, payload), "late" if current == 1 else "ok")
            self.assertEqual((ring.valid_from, ring.write_pos, a.next_packet, ring.lead), (0, 2*n, 2, 0))
            # The next packet is real program material. It follows the revised slot with no added or repeated frame.
            a.run_pass(ring, 1, False)
            for c, used in readers:
                c.run_pass(ring, 1)
                self.assertEqual(c.out[LAG:LAG+n], [("A", i) for i in range(n)])
                self.assertEqual(c.out[LAG+n:LAG+2*n], [None] * used + payload[used:])
                self.assertEqual(c.out[LAG+2*n:], [("A", i) for i in range(2*n, len(c.out)-LAG)])
                self.assertEqual((c.underrun, c.skips), (0, []))

    def test_rerelease_cannot_rewrite_another_generation_paused_run_or_eos(self):
        for guard in ("retired", "not published", "new owner", "paused", "next pre-roll", "stopped", "EOS"):
            with self.subTest(guard=guard):
                ring, a = Ring(), Render("A", 128, 0, 0, True)
                a.run_pass(ring, 0, False)
                current, offered, state = 1, 1, "run"
                if guard == "retired":
                    current = 3  # the same physical slot can now be owned by packet 3
                elif guard == "not published":
                    offered = 2
                elif guard == "new owner":
                    ring.owner = Render("B", 128, 100, 0, True)
                elif guard in ("paused", "next pre-roll"):
                    a.begin_pause(ring, current, 160)
                    if guard == "next pre-roll":
                        state = "pause"
                elif guard == "stopped":
                    state = "stop"
                else:
                    a.set_eos(ring, 1, 16)
                original = ring.data.copy()
                a.release_published(ring, current, offered, [("new", i) for i in range(128)], state=state)
                self.assertEqual(ring.data, original)
                self.assertEqual((ring.write_pos, a.next_packet), (256, 2))

    def test_startup_release_uses_run_entry_even_while_the_stream_state_is_pause(self):
        def submit(valid, current, offered, run_began):
            if offered == current + 1 and not run_began and current not in valid:
                valid.add(current)  # one pre-filled buffer, reported by WASAPI as P+1 in Pause
            else:
                if not valid and offered == 1:
                    valid.add(0)  # the shared stream's implicit initial current buffer
                valid.add(offered)

        valid = set()
        submit(valid, 0, 1, False)
        self.assertEqual(valid, {0})
        submit(valid, 0, 1, True)
        self.assertEqual(valid, {0, 1})
        valid.clear()
        submit(valid, 0, 1, True)  # first callback races the PASSIVE lock in Run, before it arms packet 0
        self.assertEqual(valid, {0, 1})
        valid.clear()
        submit(valid, 8, 9, False)  # after Pause a single pre-fill is again for the current packet
        self.assertEqual(valid, {8})
        submit(valid, 8, 9, False)  # explicitly pre-rolling a second slot still works
        self.assertEqual(valid, {8, 9})

    def test_eos_drains_exactly_and_does_not_replay_dma_slots(self):
        for packet, frames in ((0, 0), (0, 1), (0, 128), (1, 0), (1, 37), (1, 128)):
            with self.subTest(packet=packet, frames=frames):
                ring, c = Ring(), Capture(128, "jump")
                a = Render("A", 128, 0, 0, True)
                if packet == 1:
                    a.run_pass(ring, 0, True)
                a.set_eos(ring, packet, frames)
                a.run_pass(ring, 0, True)
                for k in range(5):
                    a.run_pass(ring, k, False)
                    c.run_pass(ring, k)
                length = packet * 128 + frames
                self.assertEqual(c.out, [None] * LAG + [("A", i) for i in range(length)] + [None] * (640 - LAG - length))
                self.assertEqual(ring.write_pos, length)
                self.assertEqual(c.underrun, 0)

    def test_late_eos_clips_data_already_in_ring_and_clears_for_next_owner(self):
        ring, c = Ring(), Capture(128, "jump")
        a = Render("A", 128, 0, 0, True)
        a.run_pass(ring, 0, False)  # both slots already copied before the EOS notification
        a.set_eos(ring, 1, 5)
        for k in range(3):
            c.run_pass(ring, k)
        self.assertEqual(c.out[LAG:], [("A", i) for i in range(133)] + [None] * (384 - LAG - 133))
        self.assertEqual(c.underrun, 0)
        ring.owner, ring.active = None, False
        b = Render("B", 128, 384, 0, True)
        b.run_pass(ring, 0, True)
        c.run_pass(ring, 3)
        self.assertEqual(c.out[-(128 - LAG):], [("B", i) for i in range(128 - LAG)])

    def test_unavailable_frames_before_eos_are_still_underruns(self):
        for eos_frames, missing in ((None, 128 - LAG), (30, 30), (128, 128 - LAG)):
            with self.subTest(eos_frames=eos_frames):
                ring, c = Ring(), Capture(128, "jump")
                a = Render("A", 128, 0, 0, True)
                a.run_pass(ring, 0, True)
                if eos_frames is not None:
                    a.set_eos(ring, 1, eos_frames)
                # The second render packet misses the capture deadline. Only the part due by this pass counts.
                c.run_pass(ring, 1)
                self.assertEqual(c.underrun, missing)

    def test_pause_preserves_a_real_shortfall_on_both_sides_of_timer_shutdown(self):
        for still_stopping in (True, False):
            ring, c = Ring(), Capture(128, "jump")
            a = Render("A", 128, 0, 0, True)
            a.run_pass(ring, 0, True)
            ring.pause_end = 158  # 128 frames arrived; 30 more were due before Pause.
            ring.active = still_stopping
            for k in range(5):
                c.run_pass(ring, k)
            self.assertEqual(c.underrun, 30)

    def test_partial_pause_drains_a_previously_submitted_current_packet_for_every_reader(self):
        # Observed on the dev box: Pause came 25..90 frames into packet 1, after its ordinary late submission. Its full
        # packet must be as available as a normal pre-filled packet, whichever callback acquires the ring lock first.
        for due in (25, 90):
            for stopped_first in (False, True):
                with self.subTest(due=due, stopped_first=stopped_first):
                    ring, a = Ring(), Render("A", 480, 0, 0, True)
                    a.run_pass(ring, 0, True)
                    self.assertEqual(a.release(ring, 1, 1), "late")
                    a.begin_pause(ring, 1, 480 + due)
                    if stopped_first:
                        a.copy_released(ring, ring.pause_end)
                        ring.active = False
                    for packet in (128, 240):
                        c = Capture(packet, "jump")
                        for k in range(10):
                            c.run_pass(ring, k)
                        self.assertEqual(c.out, [None] * LAG + [("A", i) for i in range(960)] + [None] * (10 * packet - LAG - 960))
                        self.assertEqual(c.underrun, 0)
                    a.copy_released(ring, ring.pause_end)
                    self.assertEqual((ring.write_pos, a.next_packet), (960, 2))
                    # The resumed stream is partway through packet 1. Both readers have already received its committed
                    # tail; only packet 2 may be published on the new timeline.
                    a.anchor, a.anchor_valid = 2400, False
                    a.run_pass(ring, 1, True)
                    a.run_pass(ring, 1, False)
                    self.assertEqual((ring.valid_from, ring.write_pos), (3360, 3840))
                    self.assertEqual([ring.data[t] for t in range(3360, 3840)], [("A", i) for i in range(960, 1440)])

    def test_partial_pause_hole_and_subsequent_preroll_belong_to_different_runs(self):
        for submit_after in (False, True):
            with self.subTest(submit_after=submit_after):
                ring, a = Ring(), Render("A", 480, 0, 0, True)
                a.run_pass(ring, 0, True)
                self.assertEqual(a.release(ring, 1, 2), "ok")  # packet 2 was submitted, packet 1 was not
                a.begin_pause(ring, 1, 530)
                if submit_after:
                    self.assertEqual(a.release(ring, 1, 1, state="pause"), "ok")
                a.copy_released(ring, ring.pause_end)
                ring.active = False
                c = Capture(128, "jump")
                for k in range(12):
                    c.run_pass(ring, k)
                self.assertEqual(c.out, [None] * LAG + [("A", i) for i in range(480)] + [None] * (1536 - LAG - 480))
                self.assertEqual((c.underrun, ring.write_pos, a.next_packet), (50, 480, 1))
                if submit_after:
                    # Resume on a later anchor; a new pre-roll can only appear on the resumed timeline.
                    a.anchor, a.anchor_valid = 1536, False
                    a.run_pass(ring, 1, True)
                    self.assertIsNone(ring.pause_end)
                    self.assertEqual(ring.valid_from, 1536 + 480)
                    self.assertEqual(ring.data[ring.valid_from], ("A", 480))

    def test_reader_can_reach_the_released_next_slot_before_the_current_render_tick(self):
        for cutoff in (None, 971, 960):
            for submission in ("before", "missing", "after"):
                if cutoff is None and submission == "after":
                    continue
                with self.subTest(cutoff=cutoff, submission=submission):
                    ring, a = Ring(), Render("A", 480, 0, 0, True)
                    a.run_pass(ring, 0, True)
                    a.run_pass(ring, 0, False)  # packets 0 and 1 are in the ring, timer for packet 1 has yet to run
                    a.current = 1
                    if submission == "before":
                        self.assertEqual(a.release(ring, 1, 2), "ok")
                    if cutoff is not None:
                        a.begin_pause(ring, 1, cutoff)
                        if submission == "after":
                            self.assertEqual(a.release(ring, 1, 2), "ok")
                    # With the normal 49-frame lag, two 510-frame reads reach 11 frames into packet 2 (the WMP trace: 7–14).
                    expected = submission == "before" and cutoff != 960
                    readers = [Capture(510, "jump"), Capture(510, "jump")]
                    for c in readers:
                        c.run_pass(ring, 0)
                        self.assertEqual(ring.write_pos, 960)
                    for c in readers:
                        c.run_pass(ring, 1)
                        self.assertEqual(c.out[-11:], [("A", i) for i in range(960, 971)] if expected else [None] * 11)
                        self.assertEqual(c.underrun, 0 if expected or cutoff == 960 else 11)
                        self.assertEqual((ring.lead, c.skips, a.late_copies), (0, [], 0))
                        self.assertEqual((a.next_packet, ring.write_pos), (3, 1440) if expected else (2, 960))
                    self.assertEqual(readers[0].out, readers[1].out)

    def test_pause_drains_published_tail_and_resume_requires_audio_again(self):
        ring, c = Ring(), Capture(128, "jump")
        a = Render("A", 128, 0, 0, True)
        a.run_pass(ring, 0, True)
        ring.pause_end = 100
        for k in range(3):
            c.run_pass(ring, k)
        self.assertEqual(sum(frame is not None for frame in c.out), 128)
        self.assertEqual(c.underrun, 0)
        a.run_pass(ring, 0, True)
        c.run_pass(ring, 3)
        self.assertEqual(c.underrun, 128)

    def test_rejected_eos_does_not_seal_the_stream(self):
        ring, a = Ring(), Render("A", 128, 0, 0, True)
        self.assertEqual(a.release(ring, 7, 8), "ok")
        self.assertEqual(a.release(ring, 8, 4, eos_frames=1), "late")
        self.assertEqual(a.release(ring, 8, 8, eos_frames=1), "late")
        self.assertEqual(a.release(ring, 7, 7, state="pause", eos_frames=1), "late")  # newer accepted release
        self.assertEqual(a.release(ring, 8, 10, eos_frames=1), "overrun")
        self.assertEqual(a.release(ring, 8, 9, eos_frames=129), "invalid")
        self.assertEqual(a.release(ring, 8, 9), "ok")
        self.assertIsNone(a.eos)
        self.assertEqual(a.release(ring, 10, 10, state="pause", eos_frames=37), "ok")
        self.assertEqual(a.release(ring, 10, 11), "ended")

    def test_stopped_packet_cannot_seal_the_next_preroll(self):
        ring, a = Ring(), Render("A", 128, 0, 0, True)
        self.assertEqual(a.release(ring, 0, 0, state="stop", eos_frames=0), "stopped")
        self.assertEqual(a.release(ring, 0, 1, state="stop"), "stopped")
        self.assertEqual(a.release(ring, 0, 0, state="pause"), "ok")
        self.assertEqual(a.release(ring, 0, 1), "ok")

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


    def test_render_start_publishes_preroll_before_its_first_deadline(self):
        # One 128-frame exclusive buffer of pre-roll, followed by a refill when Start returns. If the clock starts before
        # acquiring the ring lock, the first DPC can consume both halves while only the first one contains client data.
        packet = 128
        def playout(acquire_delay, anchor_before_lock):
            anchor = 0 if anchor_before_lock else acquire_delay
            deadline = anchor + packet
            first_timer_wins = deadline <= acquire_delay
            # The first client refill after a missed notification is assigned to packet 2. Its samples still begin at 128.
            slots = [list(range(packet)), [None] * packet]
            if first_timer_wins:
                return slots[0] + slots[1]
            ring, render = Ring(), Render("A", packet, anchor, 0, True)
            render.run_pass(ring, 0, True)
            self.assertEqual(ring.write_pos, anchor + packet)
            slots[1] = list(range(packet, 2 * packet))
            return slots[0] + slots[1]
        self.assertEqual(playout(2 * packet, True), list(range(packet)) + [None] * packet)
        for delay in (0, packet - 1, packet, 2 * packet, 10 * packet):
            self.assertEqual(playout(delay, False), list(range(2 * packet)))


class TwoSlotReleases(unittest.TestCase):
    """Which of the two physical DMA slots the client has actually submitted, independent of the samples they contain."""

    @staticmethod
    def receive(word, offered, current, paused=False, eos=False):
        if word < 0:
            return word, False
        newest = (word >> 1) - 1
        has_current = current == newest or (word & 1 and current == newest - 1)
        packet = current if paused and not eos and offered == current + 1 and not has_current else offered
        if eos and packet < newest:
            return word, False
        if packet > newest:
            predecessor = newest >= 0 and packet == newest + 1 or newest == -1 and packet == 1 and not eos
            word = ((packet + 1) << 1) | int(predecessor)
        elif packet == newest - 1:
            word |= 1
        return -word if eos else word, True

    @staticmethod
    def submitted(word, packet):
        newest = (abs(word) >> 1) - 1
        return word != 0 and (packet == newest or bool(abs(word) & 1) and packet == newest - 1)

    def test_omitted_packet_is_never_read_from_the_reused_slots(self):
        for omitted in (None, 1, 3):
            with self.subTest(omitted=omitted):
                word = 0
                slots = ["unsubmitted", "unsubmitted"]
                output, missed = [], []
                for current in range(6):
                    offered = range(2) if current == 0 else [current + 1] if current < 5 else []
                    for packet in offered:
                        if packet != omitted:
                            slots[packet % 2] = packet
                            word, ok = self.receive(word, packet, current, paused=current == 0, eos=packet == 5)
                            self.assertTrue(ok)
                    if self.submitted(word, current):
                        output.append(slots[current % 2])
                    else:
                        output.append(None)
                        missed.append(current)
                self.assertEqual(output, [None if i == omitted else i for i in range(6)])
                self.assertEqual(missed, [] if omitted is None else [omitted])

    def test_windows_preroll_is_confirmed_again_at_run_and_direct_eos_stays_literal(self):
        for current in (0, 19, 2**32 - 1):
            prior = current << 1 if current else 0  # last packet of the previous run
            word, _ = self.receive(prior, current + 1, current, paused=True)
            self.assertTrue(self.submitted(word, current))
            self.assertFalse(self.submitted(word, current + 1))
            word, _ = self.receive(word, current + 1, current)
            self.assertTrue(all(self.submitted(word, p) for p in (current, current + 1)))
            # Two explicitly identified pre-roll buffers, including a final zero-length one.
            first, _ = self.receive(prior, current, current, paused=True)
            both, _ = self.receive(first, current + 1, current, paused=True, eos=True)
            self.assertTrue(all(self.submitted(both, p) for p in (current, current + 1)))
            only_final, _ = self.receive(prior, current + 1, current, paused=True, eos=True)
            self.assertFalse(self.submitted(only_final, current))
            self.assertTrue(self.submitted(only_final, current + 1))

    def test_duplicate_late_and_eos_releases_never_overwrite_a_newer_generation(self):
        word, _ = self.receive(0, 0, 0, paused=True)
        word, _ = self.receive(word, 2, 1)
        self.assertFalse(self.submitted(word, 1))
        self.assertEqual(self.receive(word, 1, 2, eos=True), (word, False))
        self.assertEqual(self.receive(word, 2, 2), (word, True))
        both, _ = self.receive(word, 1, 2)
        self.assertTrue(all(self.submitted(both, p) for p in (1, 2)))
        self.assertEqual(self.receive(both, 0, 2), (both, True))
        sealed, _ = self.receive(both, 2, 2, eos=True)
        self.assertEqual(self.receive(sealed, 3, 2), (sealed, False))

    def test_final_length_and_both_generation_tags_are_visible_in_the_same_atomic_snapshot(self):
        # Positive states have ordinary generation tags. Each negative state contains the complete final publication.
        for packet_frames in (96, 128, 480):
            for final in (0, 1, 5, 2**32 - 1, 2**32):
                for valid in (0, 1, packet_frames - 1, packet_frames):
                    for previous in (0, 1):
                        sealed = ~(((final * (packet_frames + 1) + valid) << 1) | previous)
                        self.assertLess(sealed, 0)
                        bits = ~sealed
                        packet, length = divmod(bits >> 1, packet_frames + 1)
                        tags = ((packet + 1) << 1) | (bits & 1)
                        self.assertTrue(self.submitted(tags, final))
                        if final:
                            self.assertEqual(self.submitted(tags, final - 1), bool(previous))
                        self.assertEqual(packet * packet_frames + length, final * packet_frames + valid)


if __name__ == "__main__":
    unittest.main()
