#!/usr/bin/env bash
# Record host load once a minute and detect machine freezes (gaps > 20 s). Run alongside run_pairs.sh; stop with Ctrl-C.
# usage: bench/monitor_load.sh [outdir]   -> outdir/load.txt, outdir/freeze.txt
OUT=${1:-bench-work}; mkdir -p "$OUT"
( while true; do echo "$(date '+%F %T') $(cut -d' ' -f1-4 /proc/loadavg) $(head -1 /proc/stat | awk '{print "steal="$9" user="$2" sys="$4" idle="$5}')" >> "$OUT/load.txt"; sleep 60; done ) &
LP=$!; trap 'kill $LP' EXIT
last=$(date +%s)
while true; do sleep 5; now=$(date +%s); gap=$((now-last-5)); [ "$gap" -gt 20 ] && echo "$(date '+%F %T %Z') freeze ${gap}s" >> "$OUT/freeze.txt"; last=$now; done
