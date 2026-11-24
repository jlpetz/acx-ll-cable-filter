"""An offline model of the driver's app-ring capture passes (driver/src/Common/StreamEngine.cpp: AppRingCapturePass,
AppRingOtherPass, AppRingPlayCopy, AppRingHistoryWrite; TODO 19), checking the ring's rules for three and four capture
streams through a reader handoff: no stream gets a ring frame twice, and every gap in a stream's frames is one of its counted
skips, to the frame: the one that takes the slot loses exactly its HandoffSkippedFrames, and one that plays on behind a new
reader with a shorter packet moves on to the new lag at once and loses exactly its SecondarySkippedFrames (the owner,
2026-11-23: lower latency over a one-time skip), where the driver from a66d8ab to e801320 kept the longer lag with no gap.
Frames are numbered by the ring's position, time in frames on the QPC time line; the app always has frames ready (no
underruns), and each pass's timer is late by up to 817 us, the maximum in the A1 session's live runs (the later session's
wasn't read). Left out: the copy's 32,768-frame length and its 32-gap record, a Pause and Run of one stream.

The skip log (NF_VMIC_STATS' SkipLog, the owner, 2026-11-24: "build it rather than marking things inconclusive"): every frame
of the ring a stream never gets is in one of its own entries, by the ring's frames it concerns: the taker's skip, a skip to a
shorter lag, a jump in its own time line (a glitch adjustment), and a read past the copy's end (the reader late: silence, and
the frames the reader then writes there never reach it; the entry's ring span is filled in by those writes). So each gap ties
to its own entries exactly, with any number of streams: where two play on through one handoff to a shorter packet the
driver-wide counter holds both skips and neither gap matches it alone (Astra's four-stream model, 2026-11-24: 334 + 352,
counted 686), but each matches its own entry. verify's rule (nf-vmic-test: a gap's ring span is a chain of one stream's
entries, end to end, any stream's, since verify doesn't know its own) is ties() here, and its verdict judge(): a gap passes
only where its chains hold the two skips the owner allowed (a taker's, 2026-11-20; to a shorter lag, 2026-11-23); a late read
or a jump is the driver's record of a loss, and fails. Where several streams' chains tie a gap, the gap takes their verdict
where they agree, and is INCONCLUSIVE only where they don't (the owner, 2026-11-24). Each jump is counted where it is logged
(the device's SecondaryJumps and SecondaryJumpFrames, the owner, 2026-11-24), and the log's jumps add up to the counters.

It also shows the cause of the uncounted gaps in the live three-stream runs (2026-11-23): a reader that gave the slot up only
after its timer stopped left the copy short for as long as the stop took, and a stream playing on lost what the next
reader then wrote there; the log now has each such loss as a late read. Run from any directory:
    python tools/test_app_ring_model.py
"""
import random
import unittest

LAG = 49  # LOOPBACK_LAG_HNS (1 ms) in frames, plus one
MAX_GAP = 4800  # LOOPBACK_MAX_GAP_HNS (100 ms): a longer jump starts a new run of the copy
LATE_MAX = 39  # 817 us, the timer lateness maximum in the A1 session's live runs


class Entry:
    """A skip-log entry (NF_VMIC_EVENT): its kind, stream, frames counted, time-line span, and the ring's frames it concerns."""

    def __init__(self, seq, kind, stream, frames, span, ring):
        self.seq, self.kind, self.stream, self.frames, self.span, self.ring = seq, kind, stream, frames, span, ring
        self.pending = False  # NF_VMIC_EVENTF_PENDING: the reader hasn't yet written the whole span

    def __repr__(self):
        return f"{self.kind}({self.stream}, {self.frames}, span {self.span}, ring {self.ring})"


class Stream:
    def __init__(self, name, packet, start):
        self.name, self.packet, self.anchor = name, packet, start
        self.end = None  # m_AppEnd: its place in the copy (None: none yet)
        self.lead, self.other_valid, self.no_slot = 0, False, False
        self.out = []  # every frame it delivered: a ring position, or None (silence)
        self.skipped = 0  # HandoffSkippedFrames counted when it took the slot
        self.counted = []  # its counted skips in order: ("handoff" or "secondary", frames)
        self.late = None  # its open late-read entry (m_EvLateSequence), extended while its reads stay past the copy's end
        self.glitch_at, self.glitch = None, 0  # a glitch adjustment of its time line: from packet glitch_at, `glitch` frames


class Ring:
    def __init__(self):
        self.reader, self.read_frame, self.attach_passes = None, 0, 0
        self.valid, self.hist_from, self.hist_end, self.lead = False, 0, 0, 0
        self.copy, self.gaps = {}, []  # time-line frame -> ring position; the recorded gaps [from, end)
        self.log, self.claims = [], []  # the skip log; the spans the reader hasn't written yet: [lo, hi, entry]
        self.jumps, self.jump_frames = 0, 0  # SecondaryJumps, SecondaryJumpFrames: counted where each jump is logged

    def gap_frames(self, lo, hi):
        return sum(max(0, min(hi, e) - max(lo, f)) for f, e in self.gaps)

    def held_frames(self, lo, hi):
        """The copy's frames in [lo, hi) from its valid start to its end, less its recorded gaps (as the driver counts a skip)."""
        lo, hi = max(lo, self.hist_from), min(hi, self.hist_end)
        return hi - lo - self.gap_frames(lo, hi) if self.valid and hi > lo else 0

    def ring_span(self, lo, hi):
        """The ring's frames the copy holds as audio in [lo, hi) (the driver's segment map): (first, last + 1), or None."""
        held = [f for f in (self.history_read(t) for t in range(lo, hi)) if f is not None]
        return (held[0], held[-1] + 1) if held else None

    def log_skip(self, kind, s, frames, lo, hi, claim=True):
        """Logs a skip of [lo, hi) on the time line; what the copy doesn't hold yet there is claimed (AppRingClaim), so the
        reader's writes fill the entry's ring span in."""
        e = Entry(len(self.log) + 1, kind, s.name, frames, (lo, hi), self.ring_span(lo, hi))
        self.log.append(e)
        claim_lo = max(lo, self.hist_end) if self.valid else lo
        if claim and hi > claim_lo:
            self.claims.append([claim_lo, hi, e])
            e.pending = True
        return e

    def history_write(self, at, frames):
        """AppRingHistoryWrite: frames (ring positions, None for silence) at time-line frame `at`, on from the copy's end. The
        audio it writes into a claimed span joins that entry's ring span; a claim the copy's end has passed is settled."""
        if not self.valid or at > self.hist_end + MAX_GAP:
            self.valid, self.hist_from, self.hist_end, self.gaps = True, at, at, []
        if at > self.hist_end:
            self.gaps.append((self.hist_end, at))
            self.hist_end = at
        for i, f in enumerate(frames):
            t = at + i
            if t >= self.hist_end:
                self.copy[t] = f
                for lo, hi, e in self.claims:
                    if f is not None and lo <= t < hi:
                        e.ring = (f, f + 1) if e.ring is None else (e.ring[0], f + 1)
        self.hist_end = max(self.hist_end, at + len(frames))
        for c in [c for c in self.claims if c[1] <= self.hist_end]:
            c[2].pending = False
            self.claims.remove(c)

    def history_read(self, t):
        if self.valid and self.hist_from <= t < self.hist_end and not any(f <= t < e for f, e in self.gaps):
            return self.copy.get(t)
        return None


def play_copy(ring, s, start, n):
    """AppRingPlayCopy: n frames of the copy from `start`, silence before the stream's place; the place then reaches the end.
    What it asks for past the copy's end (or with no copy yet) is a late read: logged, extended while it stays contiguous."""
    out, ahead = [], None
    for t in range(start, start + n):
        if s.end is not None and t < s.end:
            out.append(None)
            continue
        out.append(ring.history_read(t))
        if ahead is None and (not ring.valid or t >= ring.hist_end):
            ahead = t
    if ahead is None:
        s.late = None
    else:
        end = start + n
        claim = next((c for c in ring.claims if c[2] is s.late), None) if s.late else None
        if claim and claim[1] == ahead:
            claim[1], s.late.frames, s.late.span = end, s.late.frames + end - ahead, (s.late.span[0], end)
        else:
            s.late = ring.log_skip("late", s, end - ahead, ahead, end)
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
    if s.other_valid:
        at_old = start + lead - s.lead  # this pass's read at its latest pass's lag
        if min(at_old, start) > s.end:  # its own time line jumped: what lies between never reaches it
            frames = ring.held_frames(s.end, min(at_old, start))
            # Counted where it is logged, the frames as the entry has them (the owner, 2026-11-24: two totals per device).
            ring.jumps, ring.jump_frames = ring.jumps + 1, ring.jump_frames + frames
            ring.log_skip("jump", s, frames, s.end, min(at_old, start))
        if shorter:
            lo = max(at_old, s.end)
            if start > lo:
                frames = ring.held_frames(lo, start)
                s.counted.append(("secondary", frames))
                ring.log_skip("secondary", s, frames, lo, start)
    s.lead = lead
    s.out += play_copy(ring, s, start, s.packet)
    s.other_valid = True


def capture_pass(ring, s, k, jump=True, pausing_keeps=True):
    """AppRingCapturePass for stream s's packet k (the ring always attached, every stream in its format)."""
    frame = s.anchor + k * s.packet + (s.glitch if s.glitch_at is not None and k >= s.glitch_at else 0)
    if ring.reader is None and not s.no_slot:
        ring.reader = s
        if ring.attach_passes and s.other_valid:  # a handoff: its counted skip
            skipped = ring.held_frames(s.end, frame)
            s.skipped += skipped
            s.counted.append(("handoff", skipped))
            # What the copy doesn't hold up to its packet becomes a gap in the copy at this pass: nothing to claim.
            ring.log_skip("handoff", s, skipped, s.end, frame, claim=False)
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


def run(packets, starts, stop_at, stall, release_first, rng, jump=True, pause=20, pausing_keeps=True, late_max=LATE_MAX,
        glitch=None):
    """The first of `packets` reads; the others start at `starts`; the reader's Pause comes at `stop_at`. With release_first
    (the driver now) it gives the slot up then and its timer stops `pause` frames later (a pass in between plays the copy);
    otherwise its timer stops then and the slot is given up `stall` frames later, when WdfTimerStop's wait ends (the driver
    before). The others run to the end. `glitch`: (stream index, packet, frames): that stream's time line moves later by the
    frames from that packet on (a late timer's glitch adjustment)."""
    ring = Ring()
    streams = [Stream(f"s{i}", p, t) for i, (p, t) in enumerate(zip(packets, starts))]
    if glitch:
        streams[glitch[0]].glitch_at, streams[glitch[0]].glitch = glitch[1], glitch[2]
    end_time = stop_at + 3000
    events = []
    for s in streams:
        for k in range((end_time - s.anchor) // s.packet):
            shift = s.glitch if s.glitch_at is not None and k >= s.glitch_at else 0
            at = s.anchor + (k + 1) * s.packet + shift + rng.randint(0, late_max)
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
    for s in streams:
        s.ring = ring
    return streams


def faults(s):
    """(repeats, the frames each gap lost in order) in a stream's output, by ring position."""
    codes = [f for f in s.out if f is not None]
    steps = [b - a for a, b in zip(codes, codes[1:])]
    return sum(d <= 0 for d in steps), [d - 1 for d in steps if d > 1]


def gap_spans(s):
    """Each gap in a stream's output as the ring's frames it lost, [lo, hi): what verify reads from the codes either side."""
    codes = [f for f in s.out if f is not None]
    return [(a + 1, b) for a, b in zip(codes, codes[1:]) if b > a + 1]


def ties(lo, hi, entries):
    """verify's rule: every chain of one stream's entries, end to end, starting with one whose span starts at lo, whose ring
    span is [lo, hi)."""
    spans = [e for e in entries if e.ring and e.ring[1] > e.ring[0]]
    chains = []
    for first in (e for e in spans if e.ring[0] == lo):
        chain, at = [first], first.ring[1]
        while at < hi:
            nxt = next((e for e in spans if e.stream == first.stream and e.ring[0] == at and e not in chain), None)
            if nxt is None:
                break
            chain.append(nxt)
            at = nxt.ring[1]
        if at == hi:
            chains.append(chain)
    return chains


def tie(lo, hi, entries):
    """The first chain that ties [lo, hi) (ties()), or None."""
    chains = ties(lo, hi, entries)
    return chains[0] if chains else None


ALLOWED = ("handoff", "secondary")  # the skips the owner allowed: a taker's (2026-11-20), to a shorter lag (2026-11-23)


def candidates(lo, hi, entries):
    """Every chain that ties [lo, hi) (ties()), each with its verdict: "ok" where it holds only allowed skips, else "fail". An
    entry still pending is no gap's record (nf-vmic-test's LogWindow::settling: a stream's own entries for a gap are settled
    before it reads the code after it, which check_log asserts), so verify leaves it out, and so does this."""
    settled = [e for e in entries if not e.pending]
    return [(chain, "ok" if all(e.kind in ALLOWED for e in chain) else "fail") for chain in ties(lo, hi, settled)]


def judge(lo, hi, entries):
    """verify's verdict on a gap (nf-vmic-test's tie_gaps and judge_ties; the owner, 2026-11-24: INCONCLUSIVE only when the
    ambiguity changes the verdict): the candidates' verdict where they agree, however many streams' they are ("ok" where
    every chain that ties it holds only allowed skips, "fail" where every one holds a loss, a late read or a jump), "fail"
    where none ties it, and "inconclusive" only where chains tie it and are judged differently (the log can't say whose the
    gap was, and that decides it). An entry still pending is no candidate (candidates()). The model's driver never runs out
    of records of the copy or of claims, so it has no untracked entry, and verify's rule for them (an untracked entry that
    may hold a gap's frames is weighed against its candidates, nf-vmic-test's Rival) isn't modelled here."""
    verdicts = {verdict for _chain, verdict in candidates(lo, hi, entries)}
    if not verdicts:
        return "fail"
    return verdicts.pop() if len(verdicts) == 1 else "inconclusive"


def handoffs(release_first, stall, runs=200, seed=7, cases="AB", **kw):
    """Runs A (a 160-frame reader, then 480 and 240), B (a 480-frame reader, then 160 and 240) and C (B with a second 480-frame
    stream: a handoff to the 160 or the 240 has two streams playing on to a shorter packet): per stream, its faults, whether it
    was the first reader, and the run's case."""
    rng = random.Random(seed)
    results = []
    layouts = {"A": [160, 480, 240], "B": [480, 160, 240], "C": [480, 160, 240, 480]}
    for i in range(runs):
        case = cases[i % len(cases)]
        packets = layouts[case]
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

    def check_log(self, results):
        """Every gap is a chain of the stream's own entries, exactly, and verify's rule (which doesn't know the stream) ties it
        to them, among any others for exactly those frames; every entry with ring frames lies in one of its stream's gaps, or
        past the last frame it delivered; an entry is left pending only where its span runs past the copy's end when the run
        ends (a stream's last read, with no reader pass after it); and the entries' frames are the counts, the jumps' the
        device's jump counters (verify's cross-check). Returns the entries by kind, and verify's verdict on each gap (judge())
        by verdict."""
        kinds, verdicts = {}, {}
        seen = set()
        for s, _faults, _first, _case, streams in results:
            ring = s.ring
            if id(ring) not in seen:
                seen.add(id(ring))
                self.assertTrue(all(e.span[1] > ring.hist_end for e in ring.log if e.pending), "the reader wrote past each claim")
                jumps = [e for e in ring.log if e.kind == "jump"]
                self.assertEqual((len(jumps), sum(e.frames for e in jumps)), (ring.jumps, ring.jump_frames), "the jump counters")
                for e in ring.log:
                    kinds.setdefault(e.kind, []).append(e)
            own = [e for e in ring.log if e.stream == s.name]
            gaps = gap_spans(s)
            for lo, hi in gaps:
                chain = tie(lo, hi, own)
                self.assertIsNotNone(chain, f"{s.name} ({s.packet}): gap [{lo}, {hi}) has its own entries: {own}")
                # verify's rule rests on this (LogWindow::settling): the stream got the code after the gap, so the copy's end
                # passed every claim of its entries for it, and each was settled; a settled one is never pending again.
                self.assertFalse(any(e.pending for e in chain), f"{s.name}: [{lo}, {hi}) {chain} settled")
                self.assertIn(chain, ties(lo, hi, ring.log), f"{s.name}: verify's rule ties [{lo}, {hi}) to its own")
                verdict = judge(lo, hi, ring.log)
                own_verdict = "ok" if all(e.kind in ALLOWED for e in chain) else "fail"
                self.assertIn(verdict, (own_verdict, "inconclusive"), f"{s.name}: [{lo}, {hi}) {chain}")
                verdicts.setdefault(verdict, []).append((s, lo, hi, chain))
            last = max((f for f in s.out if f is not None), default=-1)
            for e in own:
                if e.ring and e.ring[1] > e.ring[0]:
                    inside = any(lo <= e.ring[0] and e.ring[1] <= hi for lo, hi in gaps) or e.ring[0] > last
                    self.assertTrue(inside, f"{s.name}: {e} is a gap of its own ({gaps}, last {last})")
            for kind in ("handoff", "secondary"):
                self.assertEqual([e.frames for e in own if e.kind == kind], [f for k, f in s.counted if k == kind])
        return kinds, verdicts

    def test_the_rules_hold_through_a_handoff(self):
        # The driver now: the slot goes before the timer stops, however long the stop takes.
        results = handoffs(release_first=True, stall=400)
        takers, skippers = self.check_rules(results)
        _kinds, verdicts = self.check_log(results)
        self.assertEqual(sorted(verdicts), ["ok"], "verify passes every gap")
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

    def test_four_streams_each_gap_is_its_own_entry(self):
        # Case C: a 480-frame reader stops with three streams running. When the 160 or the 240 takes the slot, the other two
        # play on to a shorter packet and both skip: the driver-wide count rises by both, so neither stream's gap matches it
        # alone (verify before the log: INCONCLUSIVE), but each matches its own entry, exactly (verify now: PASS).
        results = handoffs(release_first=True, stall=400, runs=300, cases="C")
        self.check_rules(results)
        kinds, verdicts = self.check_log(results)
        self.assertEqual(sorted(kinds), ["handoff", "secondary"], "no late read or jump in these runs")
        self.assertEqual(sorted(verdicts), ["ok"], "verify passes every stream's gaps")
        # verify doesn't know its stream: now and then two streams' skips are of the same frames, and both are named (2 of
        # the 828 gaps with this seed). Those are two candidates with the same verdict, each an allowed skip, so the gap
        # passes: identical skips, not candidates judged differently (verify's INCONCLUSIVE), of which these runs have none.
        shared = [v for v in verdicts["ok"] if len(ties(v[1], v[2], v[0].ring.log)) > 1]
        self.assertLess(len(shared), len(verdicts["ok"]) // 20, "a gap tied to more than one stream's entry is rare")
        self.assertTrue(shared, "but there are some")
        self.assertTrue(all({v for _c, v in candidates(lo, hi, s.ring.log)} == {"ok"} for s, lo, hi, _chain in shared))
        two = 0
        for streams in {id(r[4]): r[4] for r in results}.values():
            skips = [e for e in streams[0].ring.log if e.kind == "secondary" and e.ring]
            if len(skips) < 2:
                continue
            two += 1
            total = sum(e.frames for e in skips)
            for e in skips:
                s = next(t for t in streams if t.name == e.stream)
                [(lo, hi)] = [g for g in gap_spans(s) if g[0] == e.ring[0]]
                self.assertEqual(hi - lo, e.frames, "the copy held the whole span: the gap is its frames")
                self.assertNotEqual(hi - lo, total, "the driver-wide count is both skips, neither gap alone")
        self.assertGreater(two, 150, "most C runs have two streams playing on to a shorter packet")

    def test_late_reads_are_logged_and_tied(self):
        # A timer up to 3 ms late (past the 1 ms lag): streams playing the copy read past its end, get silence, and lose what
        # the reader then writes there. Each such loss is a late read in the log, its ring span filled in by those writes,
        # so every gap still ties to its own entries exactly.
        results = handoffs(release_first=True, stall=0, runs=120, cases="ABC", late_max=150)
        kinds, verdicts = self.check_log(results)
        late = [e for e in kinds.get("late", []) if e.ring]
        self.assertGreater(len(late), 50, "late reads that lost frames")
        self.assertTrue(all(e.ring[1] - e.ring[0] <= e.frames for e in late), "no more frames lost than the silence it got")
        # Tied, and failed: a late read is a loss, not a skip the ring's rules allow.
        failed = verdicts.get("fail", [])
        self.assertGreater(len(failed), 50)
        self.assertTrue(all(any(e.kind == "late" for e in chain) for _s, _lo, _hi, chain in failed))
        # Gaps more than one stream's entries tie are rare here too (2 of 423 with this seed), and none has candidates judged
        # differently: no gap is INCONCLUSIVE (the owner, 2026-11-24: only where the ambiguity changes the verdict).
        everything = sum(verdicts.values(), [])
        shared = [v for v in everything if len({c[0].stream for c in ties(v[1], v[2], v[0].ring.log)}) > 1]
        self.assertLess(len(shared), len(everything) // 20)
        self.assertNotIn("inconclusive", verdicts)

    def test_a_jump_in_a_streams_own_time_line_is_logged(self):
        # A glitch adjustment in a stream playing on (its timer 4 ms late once): its time line moves later and it skips that
        # much of the copy, logged as a jump; its gap ties to it.
        rng = random.Random(3)
        jumps = 0
        for i in range(40):
            packets = [160, 480, 240] if i % 2 else [480, 160, 240, 480]
            starts = [0] + [rng.randint(500, 1500) for _ in packets[1:]]
            stop_at = rng.randint(4000, 8000)
            streams = run(packets, starts, stop_at, 0, True, rng, glitch=(2, rng.randint(2, 6), 192))
            results = [(s, faults(s), s is streams[0], "J", streams) for s in streams]
            kinds, verdicts = self.check_log(results)
            jumps += len([e for e in kinds.get("jump", []) if e.ring])
            jump_gaps = [v for v in sum(verdicts.values(), []) if any(e.kind == "jump" for e in v[3])]
            self.assertTrue(all(judge(lo, hi, s.ring.log) != "ok" for s, lo, hi, _chain in jump_gaps), "a jump's gap fails")
        self.assertGreater(jumps, 30)

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
        # stream's read past the copy's end does: nothing reaches its stopping client, and its passes count no skip; the log
        # has them as late reads.)
        for pausing_keeps in (True, False):
            results = handoffs(release_first=True, stall=0, pause=1000, pausing_keeps=pausing_keeps)
            self.check_rules([r for r in results if not r[2]])
            _kinds, verdicts = self.check_log(results)
            # The streams playing on lose nothing but their allowed skips; only the stopping reader's late reads fail.
            stopping = {id(r[0]) for r in results if r[2]}
            self.assertTrue(all(id(s) in stopping for s, *_ in verdicts.get("fail", [])))
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
        # the copy's end), a stream that plays on loses frames nothing counts, and still gets none twice. The log has each
        # loss as a late read, its ring span what the taker then wrote there.
        results = handoffs(release_first=False, stall=400, jump=False)
        lost = [(s.packet, sum(f[1])) for s, f, _, _, _ in results if not s.skipped and f[1]]
        self.assertTrue(lost, "the stall loses frames")
        self.assertTrue(all(f[0] == 0 for _, f, _, _, _ in results), "but repeats none")
        kinds, verdicts = self.check_log(results)
        self.assertTrue([e for e in kinds["late"] if e.ring], "logged as late reads")
        # verify fails those gaps, as it did before the log (then with no entry, now with the late read's): the bug it found.
        self.assertTrue(verdicts.get("fail"), "verify fails the losses")
        self.assertTrue(all(any(e.kind == "late" for e in chain) for _s, _lo, _hi, chain in verdicts["fail"]))
        # With no stall to speak of (the slot given up within the lateness), the old order keeps the rules too.
        self.check_rules(handoffs(release_first=False, stall=0))

    def test_tie_follows_one_streams_chain(self):
        a = Entry(1, "secondary", "s1", 10, (0, 10), (100, 110))
        b = Entry(2, "late", "s1", 5, (10, 15), (110, 115))
        c = Entry(3, "late", "s2", 5, (10, 15), (110, 120))
        self.assertEqual(tie(100, 115, [a, b, c]), [a, b])
        self.assertEqual(tie(100, 110, [a, b, c]), [a])
        self.assertIsNone(tie(100, 120, [a, b, c]), "another stream's entry doesn't extend s1's chain")
        self.assertIsNone(tie(101, 110, [a, b, c]))
        # verify's verdict: the skip alone passes, with its late read it fails, no tie fails.
        self.assertEqual([judge(100, 110, [a, b, c]), judge(100, 115, [a, b, c]), judge(101, 110, [a, b, c])], ["ok", "fail", "fail"])
        # Two streams' entries for the same frames, judged differently: verify can't say whose the gap was, and that decides it.
        d = Entry(4, "late", "s2", 10, (0, 10), (100, 110))
        self.assertEqual(len(ties(100, 110, [a, d])), 2)
        self.assertEqual(judge(100, 110, [a, d]), "inconclusive")
        # Judged alike, the candidates give their verdict (the owner, 2026-11-24): two allowed skips pass, two losses fail.
        e = Entry(5, "secondary", "s2", 10, (0, 10), (100, 110))
        f = Entry(6, "jump", "s3", 10, (0, 10), (100, 110))
        self.assertEqual([judge(100, 110, [a, e]), judge(100, 110, [d, f])], ["ok", "fail"])
        self.assertEqual([v for _c, v in candidates(100, 110, [a, e, d])], ["ok", "ok", "fail"])


if __name__ == "__main__":
    unittest.main()
