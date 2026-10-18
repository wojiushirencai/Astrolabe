#!/usr/bin/env bash
# Paired benchmark: for each (task, rep) arm A and arm C start at the same instant (barrier), P pairs in parallel.
# usage: bench/run_pairs.sh --tasks N5,N8 --pairs 10 [--parallel 3]    (needs ASTROLABE_BIN, a working `claude`)
# WARNING: real API spend. Our 40-pair run (4 tasks x 10) cost about $214 (see bench/README.md); set a budget first.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TASKS=""; PAIRS=10; PAR=3
while [ $# -gt 0 ]; do case "$1" in
  --tasks) TASKS=$2; shift 2;; --pairs) PAIRS=$2; shift 2;; --parallel) PAR=$2; shift 2;;
  *) echo "unknown arg $1" >&2; exit 2;; esac; done
[ -n "$TASKS" ] || { echo "--tasks required, e.g. --tasks N5,N8" >&2; exit 2; }
RUNS=${BENCH_RUNS:-$PWD/bench-work/runs}; mkdir -p "$RUNS"
export BENCH_RUNS=$RUNS
BAR=${BENCH_BARRIERS:-$PWD/bench-work/barriers}
one_pair() { t=$1; r=$2; export BARRIER="$BAR/$t-r$r" NARMS=2; rm -rf "$BARRIER"
  "$HERE/run_one.sh" "$t" A "$r" & "$HERE/run_one.sh" "$t" C "$r" & wait; }
export -f one_pair; export HERE BAR
for r in $(seq 1 "$PAIRS"); do for t in ${TASKS//,/ }; do echo "$t $r"; done; done \
  | xargs -P "$PAR" -L 1 bash -c 'one_pair $0 $1'
echo "done; score with: python3 $HERE/report.py $RUNS"
