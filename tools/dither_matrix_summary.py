"""Summarises a tools/dither_matrix.sh run (TODO 10's dither and FTZ/DAZ matrix): each run's total CPU from typeperf; per cell
and phase each build's per-block costs (the median over the counted rounds of each run's p50, p99 and max, in us) and the
chain's and AEC3's mean cost in the phase's first and last second, guard off and on side by side; whether the output's bits
are the same in both builds; the output's measures (they repeat exactly from run to run of a build: a cell whose measures vary
between a build's runs is marked "varies"; one row for both builds when they agree); and, where the mic is live, each policy's
difference from today's dither set against the reseeded control's, with the output's SI-SDR.

Usage: python -I tools/dither_matrix_summary.py RUN_DIR [--max-mean PCT] [--max-peak PCT] [--all] [--out FILE]

A round counts for the costs when both its runs stayed at or under --max-mean (default 18) mean and --max-peak (default 25)
peak total CPU, as doc/dev/build-report.md section 13 counted them (the matrix's one thread is 12.5% of the dev box's 8
logical CPUs); --all counts every round. The output's measures don't depend on the load: they come from every completed round.
A median of an even count is the mean of the middle two. Reads only RUN_DIR; writes --out if given.
"""

import argparse
import csv
import json
import os
import re
import sys
from datetime import datetime, timedelta

STAGES = ("chain", "aec3", "model", "post")
BUILDS = ("off", "on")
# The reseeded control and the policies set against it, in the order the harness runs them.
CONTROL = "mic+ref/seed2"
AGAINST = (CONTROL, "ref", "none", "bare")
POLICIES = ("mic+ref",) + AGAINST


def median(xs):
    xs = sorted(x for x in xs if x is not None)
    if not xs:
        return None
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def read_rounds(path):
    """{(round, build): (start, end, exit)} from rounds.log, its times on the header's date (a day on at each wrap). The
    warm-up runs ("discard guard ...") don't match."""
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


def key_text(k):
    return f"{k[0]:<13} {k[1]:<6} {k[2]:<13} {k[3]:<14} {k[4]:<9}"


HEAD = f"{'chain':<13} {'gate':<6} {'dither':<13} {'signal':<14} {'phase':<9}"
# The output's measures: (column, JSON key under "output" or at the phase, scale, shown when None).
OUTPUT = (
    ("zeros%", "zero_share", 100.0, "-"),
    ("first0", "first_zero_ms", 1.0, "never"),
    ("all0", "all_zero_ms", 1.0, "never"),
    ("i24 0%", "zero_share_i24", 100.0, "-"),
    ("i24 1st0", "first_zero_ms_i24", 1.0, "never"),
    ("i24 all0", "all_zero_ms_i24", 1.0, "never"),
    ("i16 0%", "zero_share_i16", 100.0, "-"),
    ("i16 1st0", "first_zero_ms_i16", 1.0, "never"),
    ("i16 all0", "all_zero_ms_i16", 1.0, "never"),
    ("peak", "peak_dbfs", 1.0, "zero"),
    ("late", "late_peak_dbfs", 1.0, "zero"),
)


def measures(p):
    """A phase's output measures, in OUTPUT's order, then the SI-SDR, the difference from today's (max, RMS, differ share)
    and the bits' hash."""
    o, d = p["output"], p.get("vs_today")
    vs = (d["max_dbfs"], d.get("rms_dbfs"), d["differ_share"]) if d else ("-", "-", "-")
    return tuple(o.get(k) for _, k, _, _ in OUTPUT) + (p.get("si_sdr_db"),) + vs + (o.get("fnv1a"),)


def show(x, scale=1.0, never="-", w=8, places=None):
    """`x` times `scale` in `w` columns (3 places for a share, else 1, or `places`), `never` for None."""
    if x is None:
        return f"{never:>{w}}"
    if isinstance(x, str):
        return f"{x:>{w}}"
    return f"{x * scale:{w}.{places if places is not None else (3 if scale != 1.0 else 1)}f}"


def summarise(run_dir, max_mean, max_peak, count_all):
    lines = []
    say = lines.append
    runs = read_rounds(os.path.join(run_dir, "rounds.log"))
    cpu = read_cpu(os.path.join(run_dir, "cpu.csv"))
    rounds = sorted({r for r, _ in runs})
    say(f"TODO 10's dither matrix: {run_dir}")
    say("")
    say("Runs: total CPU, mean and peak (%), from typeperf")
    counted, done = [], []
    for r in rounds:
        ok = complete = True
        for b in BUILDS:
            info = runs.get((r, b))
            if not info or info["end"] is None or info["exit"] != 0 or not os.path.exists(os.path.join(run_dir, f"r{r}-{b}.jsonl")):
                ok = complete = False
                say(f"  round {r:3} guard {b:3}  missing or failed")
                continue
            mean, peak = cpu_over(cpu, info["start"], info["end"])
            quiet = mean is not None and mean <= max_mean and peak <= max_peak
            ok = ok and (quiet or count_all)
            span = f"{info['start']:%H:%M:%S}-{info['end']:%H:%M:%S}"
            say(f"  round {r:3} guard {b:3}  {span}  CPU mean {fmt(mean, 5)} peak {fmt(peak, 5)}  {'quiet' if quiet else 'busy'}")
        if complete:
            done.append(r)
        if ok:
            counted.append(r)
    say(f"Counted rounds ({'all' if count_all else f'both runs at most {max_mean}% mean and {max_peak}% peak'}): "
        f"{', '.join(map(str, counted)) or 'none'}")
    say(f"Completed rounds (the output's measures): {', '.join(map(str, done)) or 'none'}")
    cost = {b: [read_cells(os.path.join(run_dir, f"r{r}-{b}.jsonl")) for r in counted] for b in BUILDS}
    out = {b: [read_cells(os.path.join(run_dir, f"r{r}-{b}.jsonl")) for r in done] for b in BUILDS}
    keys = []
    for b in BUILDS:
        for cells in out[b]:
            keys.extend(k for k in cells if k not in keys)

    say("")
    say("Per-block cost, us: the median over the counted rounds of each run's figure, guard off | on")
    cols = [f"{s} {q}" for s in STAGES for q in ("p50", "p99", "max")]
    say(HEAD + "".join(f"  {c:>15}" for c in cols))
    for k in keys:
        row = key_text(k)
        for s in STAGES:
            for q in ("p50", "p99", "max"):
                v = [median([(c.get(k, {}).get("us", {}).get(s) or {}).get(q) for c in cost[b]]) for b in BUILDS]
                row += f"  {fmt(v[0])} {fmt(v[1])}"
        say(row)

    say("")
    say("Per-second mean cost, us: the chain's and AEC3's in the phase's first and last second (a stall still there in the last "
        "second persists), the median over the counted rounds, guard off | on")
    per_s = [("chain", "chain_mean_us_per_s", 0), ("chain", "chain_mean_us_per_s", -1), ("aec3", "aec3_mean_us_per_s", 0),
             ("aec3", "aec3_mean_us_per_s", -1)]
    say(HEAD + "".join(f"  {f'{n} ' + ('first s' if i == 0 else 'last s'):>15}" for n, _, i in per_s))
    for k in keys:
        row = key_text(k)
        for _, field, i in per_s:
            v = [median([(c.get(k, {}).get(field) or [None])[i] for c in cost[b]]) for b in BUILDS]
            row += f"  {fmt(v[0])} {fmt(v[1])}"
        say(row)

    # The bits: each build's hashes over its completed runs.
    say("")
    say("Output bits (an FNV-1a hash of each phase's f32 output), guard off against on, over the completed rounds")
    differ, unsteady, compared = [], [], 0
    for k in keys:
        h = {b: {c[k]["output"].get("fnv1a") for c in out[b] if k in c} for b in BUILDS}
        if any(len(h[b]) > 1 for b in BUILDS):
            unsteady.append(k)
        if h["off"] and h["on"]:
            compared += 1
            if h["off"] != h["on"]:
                differ.append((k, h))
    if not differ and not unsteady:
        say(f"  bit-identical across builds: all {compared} cell phases run by both")
    else:
        say(f"  {compared - len(differ)} of {compared} cell phases bit-identical across builds; these differ:")
        for k, h in differ:
            say(f"  {key_text(k)}  off {','.join(sorted(h['off']))}  on {','.join(sorted(h['on']))}")
        for k in unsteady:
            say(f"  {key_text(k)}  varies between a build's runs")

    say("")
    say("Output: exact zeros (%), ms to the first all-zero block and to all-zero output, as f32 and as a 24-bit and a 16-bit "
        "cable gets it; the peak over the phase and its second half (dBFS); the SI-SDR against the near end's speech (dB); "
        "and against today's dither the largest difference and its RMS (dBFS) and the samples that differ (%). One row for "
        "both builds when they agree, else one each")
    names = [n for n, _, _, _ in OUTPUT] + ["SI-SDR", "vs max", "vs RMS", "differ%"]
    say(HEAD + f"  {'build':<5}" + "".join(f" {n:>8}" for n in names))
    shown = [(scale, never, None) for _, _, scale, never in OUTPUT] + [(1.0, "-", 2), (1.0, "same", None), (1.0, "same", None), (100.0, "-", None)]

    def first(b, k):
        seen = [measures(c[k]) for c in out[b] if k in c]
        return (seen[0], any(s != seen[0] for s in seen)) if seen else (None, False)

    for k in keys:
        per = {b: first(b, k) for b in BUILDS}
        same = per["off"][0] is not None and per["off"][0][:-1] == (per["on"][0] or ())[:-1]
        for label, builds in ([("both", ("off", "on"))] if same else [(b, (b,)) for b in BUILDS]):
            v, _ = per[builds[0]]
            if v is None:
                continue
            row = key_text(k) + f"  {label:<5}" + "".join(" " + show(v[i], scale, never, 8, pl) for i, (scale, never, pl) in enumerate(shown))
            if any(per[b][1] for b in builds):
                row += "  varies"
            say(row)

    # Where the mic is live: each policy's difference from today's against the control's, and the SI-SDR per policy.
    say("")
    say(f"Mic live: each policy's largest difference from today's / its RMS (dBFS; 'same' identical), the reseeded control "
        f"({CONTROL}) first: a policy no further from today's than the control is within the chain's sensitivity to any "
        f"-180 dBFS perturbation; and the SI-SDR per policy (dB; between one chain's policies only: AEC3's high-pass phase "
        f"lowers its chains' figure). Guard off's figures ('on differs' where guard on's do)")
    say(f"{'chain':<13} {'gate':<6} {'signal':<14} {'phase':<9}" + "".join(f"  {p:>15}" for p in AGAINST)
        + "".join(f" {p:>13}" for p in POLICIES))

    def live(b, k):
        p = next((c[k] for c in out[b] if k in c), None)
        if p is None or p.get("mic_silent", True):
            return None
        d = p.get("vs_today")
        return ((d["max_dbfs"], d.get("rms_dbfs")) if d else None, p.get("si_sdr_db"))

    groups = []
    for k in keys:
        g = (k[0], k[1], k[3], k[4])
        if g not in groups:
            groups.append(g)
    for g in groups:
        figs = {b: {pol: live(b, (g[0], g[1], pol, g[2], g[3])) for pol in POLICIES} for b in BUILDS}
        if not any(figs["off"].values()):
            continue
        row = f"{g[0]:<13} {g[1]:<6} {g[2]:<14} {g[3]:<9}"
        for pol in AGAINST:
            f = figs["off"][pol]
            vs = f[0] if f else None
            text = "-" if vs is None else ("same" if vs[0] is None else f"{vs[0]:.1f}/{vs[1]:.1f}" if vs[1] is not None else f"{vs[0]:.1f}/-")
            row += f"  {text:>15}"
        for pol in POLICIES:
            f = figs["off"][pol]
            row += f" {show(f[1] if f else None, w=13, places=2)}"
        if figs["off"] != figs["on"] and any(figs["on"].values()):
            row += "  on differs"
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
