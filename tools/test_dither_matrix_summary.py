"""Tests for tools/dither_matrix_summary.py: which rounds count (by the CPU over each run), the medians over them, and the
output measures per build. Run from any directory:
    python tools/test_dither_matrix_summary.py
"""
import json
import os
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import dither_matrix_summary as s  # noqa: E402


def stats(p50):
    return {"n": 10, "p50": p50, "p99": p50 * 2, "max": p50 * 3, "mean": p50}


def cell(guard, p50, zero_share, vs_today):
    phase = {
        "name": "silence", "start_block": 0, "blocks": 10, "mic_silent": True, "timed_blocks": 10,
        "us": {"chain": stats(p50), "aec3": stats(p50 / 2), "noise": None, "model": None, "post": None},
        "chain_mean_us_per_s": [p50],
        "output": {"zero_share": zero_share, "subnormal_share": 0.0, "first_zero_ms": 0.0 if zero_share else None,
                   "all_zero_ms": 0.0 if zero_share == 1.0 else None, "peak_dbfs": None if zero_share == 1.0 else -180.0},
        "vs_today": vs_today,
    }
    return {"guard": guard, "chain": "aec3", "gate": "-", "dither": "ref", "signal": "silence", "phases": [phase]}


class Summary(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        d = self.dir.name
        # Round 1 quiet, round 2 busy in its "on" run, round 3 quiet; round 3's "on" crosses midnight.
        with open(os.path.join(d, "rounds.log"), "w", encoding="utf-8") as f:
            f.write("dither matrix: 3 rounds, ABAB (guard off, then on), from 2026-11-20 23:58:50\n")
            for r, b, start, end in [(1, "off", "23:59:00", "23:59:10"), (1, "on", "23:59:10", "23:59:20"),
                                     (2, "off", "23:59:20", "23:59:30"), (2, "on", "23:59:30", "23:59:40"),
                                     (3, "off", "23:59:40", "23:59:50"), (3, "on", "23:59:50", "00:00:05")]:
                f.write(f"round {r} guard {b} start {start}\nround {r} guard {b} exit 0 end {end}\n")
        with open(os.path.join(d, "cpu.csv"), "w", encoding="utf-8") as f:
            f.write('"(PDH-CSV 4.0)","\\\\BOX\\Processor(_Total)\\% Processor Time"\n"11/20/2026 23:58:59.000"," "\n')
            for sec in range(0, 70):
                t = 23 * 3600 + 59 * 60 + sec
                busy = 30 <= sec < 40  # round 2's "on" run
                day = 20 if t < 86400 else 21
                t %= 86400
                f.write(f'"11/{day}/2026 {t // 3600:02}:{t // 60 % 60:02}:{t % 60:02}.500","{60.0 if busy else 13.0}"\n')
        for r, p50 in [(1, 40.0), (2, 99.0), (3, 50.0)]:
            for b, extra in [("off", 300.0), ("on", 0.0)]:
                with open(os.path.join(d, f"r{r}-{b}.jsonl"), "w", encoding="utf-8") as f:
                    zero = 1.0 if b == "on" else (0.5 if r == 3 else 0.0)
                    f.write(json.dumps(cell(b, p50 + extra, zero, {"max_dbfs": -180.0, "differ_share": 1.0})) + "\n")

    def tearDown(self):
        self.dir.cleanup()

    def test_busy_rounds_are_left_out_and_medians_taken_over_the_rest(self):
        text = s.summarise(self.dir.name, 18.0, 25.0, False)
        self.assertIn("Counted rounds (both runs at most 18.0% mean and 25.0% peak): 1, 3", text)
        # A run's window runs to the end of its last second: round 2's "off" run catches the busy run's first sample.
        self.assertIn("round   2 guard off  23:59:20-23:59:30  CPU mean  17.3 peak  60.0  busy", text)
        self.assertIn("round   2 guard on   23:59:30-23:59:40  CPU mean  55.7 peak  60.0  busy", text)
        row = [line for line in text.splitlines() if line.startswith("aec3") and "  345.0" in line]
        self.assertEqual(len(row), 1, text)
        # The chain's p50: off 340 and 350 -> 345, on 40 and 50 -> 45; its p99 is twice that.
        self.assertIn("  345.0    45.0    690.0    90.0", row[0])

    def test_output_measures_per_build_and_their_variation(self):
        text = s.summarise(self.dir.name, 18.0, 25.0, False)
        out = [line for line in text.splitlines() if line.startswith("aec3") and "varies" in line]
        self.assertEqual(len(out), 1, text)  # guard off: no zeros in round 1, half in round 3
        self.assertIn("  0.000 100.000", out[0])

    def test_all_counts_every_round(self):
        self.assertIn("Counted rounds (all): 1, 2, 3", s.summarise(self.dir.name, 18.0, 25.0, True))


if __name__ == "__main__":
    unittest.main()
