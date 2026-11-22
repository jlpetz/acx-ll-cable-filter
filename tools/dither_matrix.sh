#!/usr/bin/env bash
# TODO 10's dither and FTZ/DAZ matrix (crates/nf-engine/tests/dither_matrix) on a quiet box, as one command.
#
# Builds the matrix's test exe twice, release with the onnx feature: sonora's FTZ/DAZ guard off, and on
# (nf-stages/sonora-denormal-guard); copies both to target/dither-matrix/bin/, each with a note of the source it was built
# from (bin/dither-matrix-<off|on>.txt: the commit, git status, the features, the date and the exe's sha256; the commit is
# compiled into the exe too, and in every JSON line). Then runs each once as a discarded warm-up (the first launch of a new
# exe: Defender's scan, the page cache, the DLLs), then ROUNDS rounds in ABBA order (round 1 guard off then on, round 2 on
# then off, and so on), so a drift over the run falls on both builds alike, as doc/dev/build-report.md section 13 ran its
# exes. Each run is alone (--ignored --test-threads 1), while typeperf logs the box's total CPU each second and a PowerShell
# sampler the processes that used CPU. UL-UNAS is required (NF_REQUIRE_MODELS=1): install its pack first
# (python -I tools/fetch_models.py ulunas). Nothing here opens an audio device.
#
# Usage: tools/dither_matrix.sh ROUNDS [--no-build]
#   --no-build  reuse the exes in target/dither-matrix/bin/ (build again after any change to the code or the toolchain);
#               each must match the sha256 its note recorded.
#   The harness's own variables pass through (its module notes: NF_DITHER_MATRIX_SCALE, _CHAINS, _GATES, _DITHERS, _SIGNALS),
#   e.g. a short trial: NF_DITHER_MATRIX_SCALE=0.2 tools/dither_matrix.sh 1
#
# Output, in target/dither-matrix/run-YYYYMMDD-HHMMSS/:
#   r<i>-<off|on>.jsonl  one JSON line per cell; .txt its table; .log the run's stderr (progress, then the table)
#   warm-<off|on>.*      the discarded warm-up runs (the summary doesn't read them)
#   rounds.log           the exes' notes, the tree at run time, and each run's start and end (local time) and exit code
#   cpu.csv              typeperf, \Processor(_Total)\% Processor Time, each second (PDH-CSV, local time)
#   procs.log            every 2 s, each process that used over 0.2 s of CPU since the last sample (the matrix's own too)
#   build-<off|on>.log   cargo's output
#   summary.txt          tools/dither_matrix_summary.py's, written at the end (run it again with other CPU limits)
#
# Time at full length (scale 1): about 5 minutes a round (guard off about 2.5 minutes, on 2.2: 175 cells, 64 s of audio
# each), extrapolated from a 0.2-scale round on the dev box (Xeon 6975P-C VM, 2026-11-21: 37 s a run off and 34 s on, of
# which 26 s and 23 s the cells' blocks, which scale, and 10 s building the chains, which doesn't, most of both UL-UNAS's);
# plus the two warm-up runs (another round's worth), so 10 rounds take about 55 minutes. Plus the two builds: about a minute
# each when only the harness changed, a few minutes after a dependency or toolchain change.

set -euo pipefail

usage() {
    echo "usage: $0 ROUNDS [--no-build]" >&2
    exit 2
}
[ $# -ge 1 ] || usage
ROUNDS="$1"
[[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]] || usage
BUILD=1
if [ $# -ge 2 ]; then
    [ "$2" = "--no-build" ] || usage
    BUILD=0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/dither-matrix"
BIN="$OUT/bin"
RUN="$OUT/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$BIN" "$RUN"
cd "$ROOT"

# Builds the test exe with `features`, copies it to bin/dither-matrix-<build>.exe and writes its note beside it. The source
# is compiled in (NF_DITHER_MATRIX_SOURCE; cargo rebuilds the harness when it changes).
build() {
    local build="$1" features="$2" exe commit status source
    echo "building: guard $build (--features $features)"
    commit="$(git rev-parse HEAD)"
    status="$(git status --porcelain)"
    source="$commit"
    [ -z "$status" ] || source="$commit with uncommitted changes"
    NF_DITHER_MATRIX_SOURCE="$source" cargo test -p nf-engine --release --features "$features" --test dither_matrix --no-run \
        --message-format=json-render-diagnostics >"$RUN/build-$build.json" 2>"$RUN/build-$build.log" || {
        tail -20 "$RUN/build-$build.log" >&2
        exit 1
    }
    # The test exe's path from cargo's JSON (its backslashes escaped there), with forward slashes.
    exe="$(sed -n 's/.*"executable":"\([^"]*dither_matrix-[^"]*\)".*/\1/p' "$RUN/build-$build.json" | tail -1 | sed 's/\\\\/\//g')"
    [ -n "$exe" ] || {
        echo "no dither_matrix exe in cargo's output ($RUN/build-$build.json)" >&2
        exit 1
    }
    cp "$(cygpath -u "$exe")" "$BIN/dither-matrix-$build.exe"
    rm -f "$RUN/build-$build.json"
    {
        echo "built $(date '+%Y-%m-%d %H:%M:%S'), guard $build, --features $features"
        echo "commit $commit"
        echo "source $source"
        printf '%s\n' "$status" | sed '/^$/d; s/^/  changed: /'
        echo "exe-sha256 $(sha256sum "$BIN/dither-matrix-$build.exe" | cut -d' ' -f1)"
    } >"$BIN/dither-matrix-$build.txt"
}

if [ "$BUILD" = 1 ]; then
    build off onnx
    build on onnx,nf-stages/sonora-denormal-guard
fi
# Each exe must be the one its note describes (a --no-build run after a hand copy, or a build cut short, stops here).
for b in off on; do
    exe="$BIN/dither-matrix-$b.exe" note="$BIN/dither-matrix-$b.txt"
    [ -x "$exe" ] && [ -r "$note" ] || {
        echo "no $exe or no $note: run without --no-build" >&2
        rmdir "$RUN" 2>/dev/null || true
        exit 1
    }
    recorded="$(sed -n 's/^exe-sha256 //p' "$note")"
    actual="$(sha256sum "$exe" | cut -d' ' -f1)"
    [ "$recorded" = "$actual" ] || {
        echo "$exe isn't the exe $note describes (sha256 $actual, noted $recorded): run without --no-build" >&2
        rmdir "$RUN" 2>/dev/null || true
        exit 1
    }
done
{
    echo "dither matrix: $ROUNDS rounds, ABBA (odd rounds guard off then on, even rounds on then off) after a discarded run of each, from $(date '+%Y-%m-%d %H:%M:%S')"
    for b in off on; do
        echo "exe $b:"
        sed 's/^/  /' "$BIN/dither-matrix-$b.txt"
    done
    echo "at run time:"
    git rev-parse HEAD 2>/dev/null | sed 's/^/  commit /'
    git status --porcelain 2>/dev/null | sed 's/^/  changed: /'
    env | grep '^NF_DITHER_MATRIX_' | sed 's/^/  /' || true
} >"$RUN/rounds.log"

# The loggers: typeperf, and the process sampler (written here, run by Windows PowerShell; -ExecutionPolicy Bypass, as a
# client Windows refuses -File scripts by default). Stopped on exit by their Windows pids, so another session's typeperf is
# left alone.
cat >"$RUN/procs.ps1" <<'PS'
param([string]$Out)
$last = @{}
Get-Process | ForEach-Object { if ($_.CPU) { $last[$_.Id] = $_.CPU } }
while ($true) {
    Start-Sleep -Seconds 2
    $t = (Get-Date).ToString("HH:mm:ss")
    $now = @{}
    foreach ($p in Get-Process) {
        if (-not $p.CPU) { continue }
        $now[$p.Id] = $p.CPU
        $before = if ($last.ContainsKey($p.Id)) { $last[$p.Id] } else { 0 }
        $d = $p.CPU - $before
        if ($d -gt 0.2) { Add-Content -Path $Out -Value ("{0} {1,-24} {2,6} {3,6:N2}s" -f $t, $p.Name, $p.Id, $d) }
    }
    $last = $now
}
PS
typeperf "\\Processor(_Total)\\% Processor Time" -si 1 -o "$(cygpath -w "$RUN/cpu.csv")" -y >"$RUN/typeperf.log" 2>&1 &
TYPEPERF=$!
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$RUN/procs.ps1")" -Out "$(cygpath -w "$RUN/procs.log")" \
    >"$RUN/procs.err" 2>&1 &
PROCS=$!
stop_loggers() {
    for p in $TYPEPERF $PROCS; do
        if [ -r "/proc/$p/winpid" ]; then
            taskkill //F //T //PID "$(cat "/proc/$p/winpid")" >/dev/null 2>&1 || true
        fi
    done
}
trap stop_loggers EXIT
# Let the loggers start (typeperf's first sample is blank) and the box settle; then both must be running, and typeperf have
# logged a number (a run without the CPU log would count no round, which the summary would say only at the end).
cpu_samples() {
    local n
    n="$(grep -c '","[0-9]' "$RUN/cpu.csv" 2>/dev/null || true)"
    echo "${n:-0}"
}
sleep 5
for _ in $(seq 1 15); do
    [ "$(cpu_samples)" -ge 1 ] && break
    sleep 1
done
[ "$(cpu_samples)" -ge 1 ] && [ -r "/proc/$TYPEPERF/winpid" ] || {
    echo "typeperf logged no CPU sample to $RUN/cpu.csv (its output: $RUN/typeperf.log)" >&2
    exit 1
}
[ -r "/proc/$PROCS/winpid" ] || {
    echo "the process sampler isn't running (its output: $RUN/procs.err)" >&2
    exit 1
}

export NF_REQUIRE_MODELS=1
# One run of build $2 as tag $3, logged in rounds.log as "$1 guard $2 start/exit"; a failure stops the matrix.
run() {
    local label="$1" b="$2" tag="$3" code
    echo "$label guard $b start $(date +%H:%M:%S)" | tee -a "$RUN/rounds.log"
    set +e
    NF_DITHER_MATRIX_OUT="$(cygpath -w "$RUN")" NF_DITHER_MATRIX_TAG="$tag" \
        "$BIN/dither-matrix-$b.exe" --ignored --exact dither_matrix --nocapture --test-threads 1 >"$RUN/$tag.log" 2>&1
    code=$?
    set -e
    echo "$label guard $b exit $code end $(date +%H:%M:%S)" | tee -a "$RUN/rounds.log"
    [ "$code" = 0 ] || {
        tail -20 "$RUN/$tag.log" >&2
        exit 1
    }
    # The run says which build it was: a copy mix-up stops here.
    grep -q "\"guard\":\"$b\"" "$RUN/$tag.jsonl" || {
        echo "$tag.jsonl doesn't say guard $b" >&2
        exit 1
    }
}
# The warm-ups: "discard ..." lines, which the summary's round pattern doesn't match.
for b in off on; do
    run discard "$b" "warm-$b"
done
for i in $(seq 1 "$ROUNDS"); do
    if [ $((i % 2)) = 1 ]; then order="off on"; else order="on off"; fi
    for b in $order; do
        run "round $i" "$b" "r$i-$b"
    done
done
sleep 2
stop_loggers
python -I "$ROOT/tools/dither_matrix_summary.py" "$RUN" --out "$RUN/summary.txt" >/dev/null ||
    echo "the summary failed: python -I tools/dither_matrix_summary.py $RUN" >&2
echo "done: $RUN"
