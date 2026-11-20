"""Summarises a tools/dither_matrix.sh run (TODO 10's dither and FTZ/DAZ matrix): each run's total CPU from typeperf, then
per cell and phase each build's per-block costs (the median over the counted rounds of each run's p50, p99 and max, in µs)
side by side, guard off and on, and the output's measures per build (they repeat exactly from run to run of a build; a cell
whose measures vary between a build's runs is marked "varies").

Usage: python -I tools/dither_matrix_summary.py RUN_DIR [--max-mean PCT] [--max-peak PCT] [--all] [--out FILE]

A round counts when both its runs stayed at or under --max-mean (default 18) mean and --max-peak (default 25) peak total CPU,
as doc/dev/build-report.md section 13 counted them (the matrix's one thread is 12.5% of the dev box's 8 logical CPUs); --all
counts every round. A median of an even count is the mean of the middle two. Reads only RUN_DIR; writes --out if given.
"""

import argparse
import csv
import json
import os
import re
import sys
from datetime import datetime, timedelta

STAGES = ("chain", "aec3", "model", "post")
OUTPUT = ("zero_share", "first_zero_ms", "all_zero_ms", "peak_dbfs")


def median(xs):
    xs = sorted(x for x in xs if x is not None)
    if not xs:
        return None
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def read_rounds(path):
    """{(round, build): (start, end, exit)} from rounds.log, its times on the header's date (a day on at each wrap)."""
    runs, day, last = {}, None, None
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for line in lines:
        m = re.search(r"from (\d{4}-\d\d-\d\d) (\d\d:\d\d:\d\d)", line)
        if m:
            day = datetime.strptime(m[1], "%Y-%m-%d")
            last = datetime.strptime(f"{m[1]} {m[2]}", "%Y-%m-%d %H:%M:%S")
            continue
        m = re.match(r"round (\d+) guard (\w+) (start|exit (\d+) end) (\d\d):(\d\d):(\d\d)", line)
        if not m or day is None:
            continue
        t = day + timedelta(hours=int(m[5]), minutes=int(m[6]), seconds=int(m[7]))
        while t < last:
            day += timedelta(days=1)
            t += timedelta(days=1)
        last = t
        key = (int(m[1]), m[2])
        r = runs.setdefault(key, {"start": None, "end": None, "exit": None})
        if m[3] == "start":
            r["start"] = t
        else:
            r["end"], r["exit"] = t, int(m[4])
    return runs


def read_cpu(path):
    """[(time, percent)] from typeperf's CSV (its blank first sample dropped)."""
    out = []
    if not os.path.exists(path):
        return out
    with open(path, encoding="utf-8", errors="replace") as f:
        for row in list(csv.reader(f))[1:]:
            if len(row) < 2 or not row[1].strip():
                continue
            try:
                out.append((datetime.strptime(row[0], "%m/%d/%Y %H:%M:%S.%f"), float(row[1])))
            except ValueError:
                continue
    return out


def cpu_over(cpu, start, end):
    """Mean and peak of the samples from `start` to the end of `end`'s second (the log's times are whole seconds)."""
    s = [v for t, v in cpu if start <= t <= end + timedelta(seconds=1)]
    return (sum(s) / len(s), max(s)) if s else (None, None)


def read_cells(path):
    """{(chain, gate, dither, signal, phase): phase dict} from one run's JSON lines."""
    cells = {}
    with open(path, encoding="utf-8") as f:
        lines = [line for line in f.read().splitlines() if line.strip()]
    for line in lines:
        c = json.loads(line)
        for p in c["phases"]:
            cells[(c["chain"], c["gate"], c["dither"], c["signal"], p["name"])] = p
    return cells


def fmt(x, w=7, places=1):
    return f"{x:{w}.{places}f}" if isinstance(x, (int, float)) else f"{'-':>{w}}"


def summarise(run_dir, max_mean, max_peak, count_all):
    lines = []
    say = lines.append
    runs = read_rounds(os.path.join(run_dir, "rounds.log"))
    cpu = read_cpu(os.path.join(run_dir, "cpu.csv"))
    rounds = sorted({r for r, _ in runs})
    say(f"TODO 10's dither matrix: {run_dir}")
    say("")
    say("Runs: total CPU, mean and peak (%), from typeperf")
    counted = []
    for r in rounds:
        ok = True
        for b in ("off", "on"):
            info = runs.get((r, b))
            if not info or info["end"] is None or info["exit"] != 0 or not os.path.exists(os.path.join(run_dir, f"r{r}-{b}.jsonl")):
                ok = False
                say(f"  round {r:3} guard {b:3}  missing or failed")
                continue
            mean, peak = cpu_over(cpu, info["start"], info["end"])
            quiet = mean is not None and mean <= max_mean and peak <= max_peak
            ok = ok and (quiet or count_all)
            span = f"{info['start']:%H:%M:%S}-{info['end']:%H:%M:%S}"
            say(f"  round {r:3} guard {b:3}  {span}  CPU mean {fmt(mean, 5)} peak {fmt(peak, 5)}  {'quiet' if quiet else 'busy'}")
        if ok:
            counted.append(r)
    say(f"Counted rounds ({'all' if count_all else f'both runs at most {max_mean}% mean and {max_peak}% peak'}): "
        f"{', '.join(map(str, counted)) or 'none'}")
    data = {b: [read_cells(os.path.join(run_dir, f"r{r}-{b}.jsonl")) for r in counted] for b in ("off", "on")}
    keys = []
    for b in ("off", "on"):
        for cells in data[b]:
            keys.extend(k for k in cells if k not in keys)
    head = f"{'chain':<13} {'gate':<6} {'dither':<7} {'signal':<14} {'phase':<8}"
    say("")
    say("Per-block cost, us: the median over the counted rounds of each run's figure, guard off | on")
    cols = [f"{s} {q}" for s in STAGES for q in ("p50", "p99", "max")]
    say(head + "".join(f"  {c:>15}" for c in cols))
    for k in keys:
        row = f"{k[0]:<13} {k[1]:<6} {k[2]:<7} {k[3]:<14} {k[4]:<8}"
        for s in STAGES:
            for q in ("p50", "p99", "max"):
                v = [median([(c.get(k, {}).get("us", {}).get(s) or {}).get(q) for c in data[b]]) for b in ("off", "on")]
                row += f"  {fmt(v[0])} {fmt(v[1])}"
        say(row)
    say("")
    say("Output per build, off | on: exact zeros (%), ms to the first all-zero block and to all-zero output, the peak over the "
        "phase and its second half (dBFS), and against today's dither the largest difference and its RMS (dBFS) and the "
        "samples that differ (%)")
    names = ("zeros%", "first0", "all0", "peak", "late peak", "vs today max", "vs today RMS", "differ%")
    say(head + "".join(f"  {n:>15}" for n in names))

    def measure(b, k):
        seen = []
        for cells in data[b]:
            p = cells.get(k)
            if p is None:
                continue
            o, d = p["output"], p.get("vs_today")
            vs = (d["max_dbfs"], d.get("rms_dbfs"), d["differ_share"]) if d else ("-", "-", "-")
            seen.append((o["zero_share"], o["first_zero_ms"], o["all_zero_ms"], o["peak_dbfs"], o.get("late_peak_dbfs")) + vs)
        if not seen:
            return None, False
        return seen[0], any(s != seen[0] for s in seen)

    def show(v, i, scale=1.0, never="never"):
        if v is None:
            return f"{'-':>7}"
        x = v[i]
        if x is None:
            return f"{never:>7}"
        if isinstance(x, str):
            return f"{x:>7}"
        return f"{x * scale:7.{3 if scale != 1.0 else 1}f}"

    for k in keys:
        row = f"{k[0]:<13} {k[1]:<6} {k[2]:<7} {k[3]:<14} {k[4]:<8}"
        vs = [measure(b, k) for b in ("off", "on")]
        shown = [(100.0, "-"), (1.0, "never"), (1.0, "never"), (1.0, "zero"), (1.0, "zero"), (1.0, "same"), (1.0, "same"), (100.0, "-")]
        for i, (scale, never) in enumerate(shown):
            row += "  " + " ".join(show(v, i, scale, never) for v, _ in vs)
        if any(var for _, var in vs):
            row += "  varies"
        say(row)
    return "\n".join(lines) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("run_dir")
    ap.add_argument("--max-mean", type=float, default=18.0)
    ap.add_argument("--max-peak", type=float, default=25.0)
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--out")
    a = ap.parse_args(argv)
    text = summarise(a.run_dir, a.max_mean, a.max_peak, a.all)
    sys.stdout.write(text)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            f.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
