"""Tests for tools/dither_matrix_summary.py: which rounds count (by the CPU over each run; the warm-ups and ABBA order as
tools/dither_matrix.sh logs them), the medians over them, the per-second means, the bits across builds, the output measures
per build, and the policies against the control. Run from any directory:
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


def output(zero_share, fnv1a):
    o = {"subnormal_share": 0.0, "peak_dbfs": None if zero_share == 1.0 else -180.0, "late_peak_dbfs": None, "fnv1a": fnv1a}
    for x in ("", "_i24", "_i16"):
        z = 1.0 if x else zero_share  # quantised, -180 dBFS is zero
        o[f"zero_share{x}"] = z
        o[f"first_zero_ms{x}"] = 0.0 if z else None
        o[f"all_zero_ms{x}"] = 0.0 if z == 1.0 else None
    return o


def cell(guard, dither, p50, zero_share, fnv1a, vs_today, si_sdr=None):
    silence = {
        "name": "silence", "start_block": 0, "blocks": 10, "mic_silent": True, "ref_silent": True, "timed_blocks": 10,
        "us": {"chain": stats(p50), "aec3": stats(p50 / 2), "noise": None, "model": None, "post": None},
        "chain_mean_us_per_s": [p50 / 2, p50], "aec3_mean_us_per_s": [p50 / 4, p50 / 2],
        "output": output(zero_share, fnv1a), "si_sdr_db": None, "vs_today": vs_today,
    }
    speech = dict(silence, name="speech", mic_silent=False, ref_silent=False, si_sdr_db=si_sdr, output=output(0.0, "ab"))
    return {"guard": guard, "chain": "aec3", "gate": "-", "dither": dither, "signal": "speech-silence", "phases": [speech, silence]}


class Summary(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        d = self.dir.name
        # The warm-ups, then round 1 quiet (off, on), round 2 busy in its "on" run (on, off: ABBA), round 3 quiet (off, on),
        # its "on" across midnight.
        with open(os.path.join(d, "rounds.log"), "w", encoding="utf-8") as f:
            f.write("dither matrix: 3 rounds, ABBA (odd rounds guard off then on, even rounds on then off) after a discarded "
                    "run of each, from 2026-11-20 23:58:30\nexe off:\n  built 2026-11-20 23:50:00, guard off\n")
            f.write("discard guard off start 23:58:40\ndiscard guard off exit 0 end 23:58:50\n")
            f.write("discard guard on start 23:58:50\ndiscard guard on exit 0 end 23:59:00\n")
            for r, b, start, end in [(1, "off", "23:59:00", "23:59:10"), (1, "on", "23:59:10", "23:59:20"),
                                     (2, "on", "23:59:20", "23:59:30"), (2, "off", "23:59:30", "23:59:40"),
                                     (3, "off", "23:59:40", "23:59:50"), (3, "on", "23:59:50", "00:00:05")]:
                f.write(f"round {r} guard {b} start {start}\nround {r} guard {b} exit 0 end {end}\n")
        with open(os.path.join(d, "cpu.csv"), "w", encoding="utf-8") as f:
            f.write('"(PDH-CSV 4.0)","\\\\BOX\\Processor(_Total)\\% Processor Time"\n"11/20/2026 23:58:59.000"," "\n')
            for sec in range(0, 70):
                t = 23 * 3600 + 59 * 60 + sec
                busy = 21 <= sec < 31  # within round 2's "on" run and the first second of its "off"
                day = 20 if t < 86400 else 21
                t %= 86400
                f.write(f'"11/{day}/2026 {t // 3600:02}:{t // 60 % 60:02}:{t % 60:02}.500","{60.0 if busy else 13.0}"\n')
        for r, p50 in [(1, 40.0), (2, 99.0), (3, 50.0)]:
            for b, extra in [("off", 300.0), ("on", 0.0)]:
                with open(os.path.join(d, f"r{r}-{b}.jsonl"), "w", encoding="utf-8") as f:
                    zero = 1.0 if b == "on" else (0.5 if r == 3 else 0.0)
                    f.write(json.dumps(cell(b, "mic+ref", p50 + extra, 0.0, "11", None, 12.5)) + "\n")
                    f.write(json.dumps(cell(b, "mic+ref/seed2", p50, 0.0, "22", {"max_dbfs": -60.0, "rms_dbfs": -80.0, "differ_share": 1.0}, 12.25)) + "\n")
                    f.write(json.dumps(cell(b, "ref", p50 + extra, zero, "33" if b == "off" else "44",
                                            {"max_dbfs": -180.0, "rms_dbfs": -190.0, "differ_share": 1.0}, 12.5)) + "\n")
        self.text = s.summarise(d, 18.0, 25.0, False)

    def tearDown(self):
        self.dir.cleanup()

    def rows(self, start, section):
        """The rows of `section` (the paragraph whose first line starts with it) that start with `start`."""
        para = next(p for p in self.text.split("\n\n") if p.startswith(section))
        return [line for line in para.splitlines() if line.startswith(start)]

    def test_busy_rounds_are_left_out_and_medians_taken_over_the_rest(self):
        self.assertIn("Counted rounds (both runs at most 18.0% mean and 25.0% peak): 1, 3", self.text)
        self.assertIn("Completed rounds (the output's measures): 1, 2, 3", self.text)
        # A run's window runs to the end of its last second: round 2's "off" run catches the busy run's last sample.
        self.assertIn("round   1 guard on   23:59:10-23:59:20  CPU mean  13.0 peak  13.0  quiet", self.text)
        self.assertIn("round   2 guard on   23:59:20-23:59:30  CPU mean  55.7 peak  60.0  busy", self.text)
        self.assertIn("round   2 guard off  23:59:30-23:59:40  CPU mean  17.3 peak  60.0  busy", self.text)
        self.assertNotIn("discard", self.text)  # the warm-ups aren't rounds

    def test_medians_and_per_second_means(self):
        text = s.summarise(self.dir.name, 60.0, 60.0, False)  # every round quiet: 1, 2, 3
        self.assertIn("Counted rounds (both runs at most 60.0% mean and 60.0% peak): 1, 2, 3", text)
        para = next(p for p in text.split("\n\n") if p.startswith("Per-block cost"))
        row = [line for line in para.splitlines() if line.startswith("aec3") and " ref " in line and "silence  " in line]
        self.assertEqual(len(row), 1, text)
        # The chain's p50: off 340, 399, 350 -> 350, on 40, 99, 50 -> 50; its p99 is twice that.
        self.assertIn("  350.0    50.0    700.0   100.0", row[0])
        para = next(p for p in text.split("\n\n") if p.startswith("Per-second mean"))
        row = [line for line in para.splitlines() if line.startswith("aec3") and " ref " in line and "silence  " in line]
        # The chain's first second is half its p50, its last its p50; AEC3's a quarter and a half.
        self.assertIn("  175.0    25.0    350.0    50.0     87.5    12.5    175.0    25.0", row[0])

    def test_bits_across_builds(self):
        rows = self.rows("  ", "Output bits")
        self.assertIn("  5 of 6 cell phases bit-identical across builds; these differ:", rows)
        differ = [r for r in rows if "off 33" in r]
        self.assertEqual(len(differ), 1, rows)
        self.assertIn("on 44", differ[0])
        self.assertFalse(any("varies between a build's runs" in r for r in rows), rows)  # its zeros vary, its bits don't

    def test_output_measures_per_build_and_their_variation(self):
        rows = self.rows("aec3", "Output:")
        ref = [r for r in rows if " ref " in r and "silence  " in r]
        self.assertEqual([r.split()[5] for r in ref], ["off", "on"])  # the builds differ: one row each
        self.assertIn("varies", ref[0])  # guard off: no zeros in rounds 1 and 2, half in round 3
        self.assertIn("  100.000", ref[1])
        same = [r for r in rows if " mic+ref " in r and "silence  " in r]
        self.assertEqual(len(same), 1, rows)
        self.assertIn(" both ", same[0])

    def test_policies_against_the_control(self):
        rows = self.rows("aec3", "Mic live")
        self.assertEqual(len(rows), 1, self.text)
        # mic+ref/seed2, then ref; none and bare didn't run; the SI-SDR of mic+ref, seed2, ref.
        self.assertIn("-60.0/-80.0", rows[0])
        self.assertIn("-180.0/-190.0", rows[0])
        self.assertIn("12.50         12.25         12.50", rows[0])

    def test_all_counts_every_round(self):
        self.assertIn("Counted rounds (all): 1, 2, 3", s.summarise(self.dir.name, 18.0, 25.0, True))


if __name__ == "__main__":
    unittest.main()
