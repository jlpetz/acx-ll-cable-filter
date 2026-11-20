#!/usr/bin/env bash
# TODO 10's dither and FTZ/DAZ matrix (crates/nf-engine/tests/dither_matrix) on a quiet box, as one command.
#
# Builds the matrix's test exe twice, release with the onnx feature: sonora's FTZ/DAZ guard off, and on
# (nf-stages/sonora-denormal-guard); copies both to target/dither-matrix/bin/. Then runs them alternately, guard off then on,
# ROUNDS rounds (ABAB), each run alone (--ignored --test-threads 1), while typeperf logs the box's total CPU each second and a
# PowerShell sampler the processes that used CPU, as doc/dev/build-report.md section 13 did. UL-UNAS is required
# (NF_REQUIRE_MODELS=1): install its pack first (python -I tools/fetch_models.py ulunas). Nothing here opens an audio device.
#
# Usage: tools/dither_matrix.sh ROUNDS [--no-build]
#   --no-build  reuse the exes in target/dither-matrix/bin/ (build again after any change to the code or the toolchain).
#   The harness's own variables pass through (its module notes: NF_DITHER_MATRIX_SCALE, _CHAINS, _GATES, _DITHERS, _SIGNALS),
#   e.g. a short trial: NF_DITHER_MATRIX_SCALE=0.2 tools/dither_matrix.sh 1
#
# Output, in target/dither-matrix/run-YYYYMMDD-HHMMSS/:
#   r<i>-<off|on>.jsonl  one JSON line per cell; .txt its table; .log the run's stderr (progress, then the table)
#   rounds.log           each run's start and end (local time) and exit code, and the exes' hashes
#   cpu.csv              typeperf, \Processor(_Total)\% Processor Time, each second (PDH-CSV, local time)
#   procs.log            every 2 s, each process that used over 0.2 s of CPU since the last sample (the matrix's own too)
#   build-<off|on>.log   cargo's output
#   summary.txt          tools/dither_matrix_summary.py's, written at the end (run it again with other CPU limits)
#
# Time at full length (scale 1): about 2.5 minutes a round, 75 s guard off and 72 s on, extrapolated from a 0.2-scale round
# on the dev box (Xeon 6975P-C VM, 2026-11-20: 21 s a run, 13.5 s of it the cells' blocks and 7.4 s building the 126 chains,
# most of both UL-UNAS's); so 10 rounds take about 25 minutes. Plus the two builds: about 50 s each when only the harness
# changed, a few minutes after a dependency or toolchain change.

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

# Builds the test exe with `features` and copies it to bin/dither-matrix-<build>.exe.
build() {
    local build="$1" features="$2" exe
    echo "building: guard $build (--features $features)"
    cargo test -p nf-engine --release --features "$features" --test dither_matrix --no-run --message-format=json-render-diagnostics \
        >"$RUN/build-$build.json" 2>"$RUN/build-$build.log" || {
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
}

if [ "$BUILD" = 1 ]; then
    build off onnx
    build on onnx,nf-stages/sonora-denormal-guard
fi
for b in off on; do
    [ -x "$BIN/dither-matrix-$b.exe" ] || {
        echo "no $BIN/dither-matrix-$b.exe: run without --no-build" >&2
        exit 1
    }
done
{
    echo "dither matrix: $ROUNDS rounds, ABAB (guard off, then on), from $(date '+%Y-%m-%d %H:%M:%S')"
    git rev-parse HEAD 2>/dev/null | sed 's/^/commit /'
    git status --porcelain 2>/dev/null | sed 's/^/  changed: /'
    env | grep '^NF_DITHER_MATRIX_' | sed 's/^/  /' || true
    for b in off on; do
        echo "exe $b $(sha256sum "$BIN/dither-matrix-$b.exe" | cut -d' ' -f1)"
    done
} >"$RUN/rounds.log"

# The loggers: typeperf, and the process sampler (written here, run by Windows PowerShell). Stopped on exit by their Windows
# pids, so another session's typeperf is left alone.
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
typeperf "\\Processor(_Total)\\% Processor Time" -si 1 -o "$(cygpath -w "$RUN/cpu.csv")" -y >/dev/null 2>&1 &
TYPEPERF=$!
powershell.exe -NoProfile -File "$(cygpath -w "$RUN/procs.ps1")" -Out "$(cygpath -w "$RUN/procs.log")" >/dev/null 2>&1 &
PROCS=$!
stop_loggers() {
    for p in $TYPEPERF $PROCS; do
        if [ -r "/proc/$p/winpid" ]; then
            taskkill //F //T //PID "$(cat "/proc/$p/winpid")" >/dev/null 2>&1 || true
        fi
    done
}
trap stop_loggers EXIT
# Let the loggers start (typeperf's first sample is blank) and the box settle.
sleep 5

export NF_REQUIRE_MODELS=1
for i in $(seq 1 "$ROUNDS"); do
    for b in off on; do
        tag="r$i-$b"
        echo "round $i guard $b start $(date +%H:%M:%S)" | tee -a "$RUN/rounds.log"
        set +e
        NF_DITHER_MATRIX_OUT="$(cygpath -w "$RUN")" NF_DITHER_MATRIX_TAG="$tag" \
            "$BIN/dither-matrix-$b.exe" --ignored --exact dither_matrix --nocapture --test-threads 1 >"$RUN/$tag.log" 2>&1
        code=$?
        set -e
        echo "round $i guard $b exit $code end $(date +%H:%M:%S)" | tee -a "$RUN/rounds.log"
        [ "$code" = 0 ] || {
            tail -20 "$RUN/$tag.log" >&2
            exit 1
        }
        # The run says which build it was: a copy mix-up stops here.
        grep -q "\"guard\":\"$b\"" "$RUN/$tag.jsonl" || {
            echo "$tag.jsonl doesn't say guard $b" >&2
            exit 1
        }
    done
done
sleep 2
stop_loggers
python -I "$ROOT/tools/dither_matrix_summary.py" "$RUN" --out "$RUN/summary.txt" >/dev/null ||
    echo "the summary failed: python -I tools/dither_matrix_summary.py $RUN" >&2
echo "done: $RUN"
