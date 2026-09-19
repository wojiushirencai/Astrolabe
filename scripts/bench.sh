#!/usr/bin/env bash
# Index-cost regression gate for the five acceptance corpora.
#
# Usage:
#   bash scripts/bench.sh              # table; compare to baselines if present (no fail)
#   bash scripts/bench.sh --ci        # fail on REGRESS / STALE_BASELINE / nothing ran
#   bash scripts/bench.sh --save      # write crates/astrolabe-core/examples/baselines/*.json
#   bash scripts/bench.sh --quick     # warmup 0, 1 run (sanity, not a gate)
#
# Missing corpora are skipped with a reason; they do not crash the script.
# A run that skipped everything still exits 1 so CI cannot silently pass.
set -uo pipefail

# Cursor/sandbox shells sometimes shrink PATH; keep coreutils resolvable.
PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:${PATH:-}"
export PATH

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="report"   # report | ci | save
QUICK=0
WARMUP=2
RUNS=5
RSS_RATIO="${ASTROLABE_BENCH_RSS_RATIO:-1.20}"
TIME_RATIO="${ASTROLABE_BENCH_TIME_RATIO:-2.50}"
RSS_FLOOR="${ASTROLABE_BENCH_RSS_FLOOR_BYTES:-4194304}"
TIME_FLOOR="${ASTROLABE_BENCH_TIME_FLOOR_MS:-150}"

usage() {
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ci) MODE="ci"; shift ;;
    --save) MODE="save"; shift ;;
    --quick) QUICK=1; shift ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    --runs) RUNS="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "$QUICK" -eq 1 ]]; then
  WARMUP=0
  RUNS=1
fi

BASELINE_DIR="$ROOT/crates/astrolabe-core/examples/baselines"
OUT_DIR="$ROOT/target/bench"
mkdir -p "$OUT_DIR"

# name|relative-or-absolute-from-ROOT|marketing-note
corpora=(
  "go|corpus/go|gin"
  "java|corpus/java|gson"
  "rust|corpus/rust|ripgrep"
  "serena|../serena|serena"
  "openvisio-oss|../openvisio-oss|openvisio-oss"
)

echo "==> cargo build --release -p astrolabe-core --example index_bench"
if ! cargo build --release -p astrolabe-core --example index_bench; then
  echo "index_bench failed to compile (other tasks may be mid-edit). Aborting." >&2
  exit 1
fi

BIN="$ROOT/target/release/examples/index_bench"
if [[ ! -x "$BIN" ]]; then
  echo "missing binary $BIN" >&2
  exit 1
fi

echo
printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
  "语料" "说明" "扫描" "符号" "失败" "中位耗时" "峰值RSS" "判定"
printf '%s\n' '-----------------+----------------+----------+------------+------------+----------+------------+--------'

ran=0
failed=0
skipped=0
skip_reasons=()

for spec in "${corpora[@]}"; do
  IFS='|' read -r name rel note <<<"$spec"
  # Paths that start with ../ are outside the workspace.
  if [[ "$rel" == ../* ]]; then
    path="$(cd "$ROOT/$rel" 2>/dev/null && pwd)" || path=""
    check="$ROOT/$rel"
  else
    path="$ROOT/$rel"
    check="$path"
  fi

  if [[ ! -d "$check" ]]; then
    printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
      "$name" "$note" "-" "-" "-" "-" "-" "SKIP"
    skip_reasons+=("$name: missing $rel")
    skipped=$((skipped + 1))
    continue
  fi

  out_json="$OUT_DIR/${name}.json"
  # Build argv as a non-empty array. Bash 3.2 + `set -u` treats empty
  # `${arr[@]}` as unbound.
  cmd=(
    "$BIN"
    --name "$name"
    --warmup "$WARMUP"
    --runs "$RUNS"
    --format kv
    --rss-ratio "$RSS_RATIO"
    --time-ratio "$TIME_RATIO"
    --rss-floor-bytes "$RSS_FLOOR"
    --time-floor-ms "$TIME_FLOOR"
    --save "$out_json"
  )
  if [[ -f "$BASELINE_DIR/${name}.json" ]]; then
    cmd+=(--baseline "$BASELINE_DIR/${name}.json")
  elif [[ "$MODE" == "ci" ]]; then
    printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
      "$name" "$note" "-" "-" "-" "-" "-" "NO_BASELINE"
    echo "  missing baseline $BASELINE_DIR/${name}.json (run: bash scripts/bench.sh --save)" >&2
    failed=1
    continue
  fi
  if [[ "$MODE" == "ci" ]]; then
    cmd+=(--ci)
  fi
  cmd+=("$check")

  # Capture stdout; leave worker progress on stderr.
  if ! output="$("${cmd[@]}" 2>"$OUT_DIR/${name}.err")"; then
    status=$?
    printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
      "$name" "$note" "-" "-" "-" "-" "-" "FAIL"
    echo "  index_bench exit $status; see $OUT_DIR/${name}.err" >&2
    tail -n 20 "$OUT_DIR/${name}.err" >&2 || true
    failed=1
    continue
  fi
  # Progress lives on stderr; copy a short tail into the log for humans.
  if [[ -s "$OUT_DIR/${name}.err" ]]; then
    cat "$OUT_DIR/${name}.err" >&2
  fi

  line="$(printf '%s\n' "$output" | awk -F'|' '/^ASTROLABE_BENCH\|/ { line=$0 } END { print line }')"
  if [[ -z "$line" ]]; then
    printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
      "$name" "$note" "-" "-" "-" "-" "-" "FAIL"
    echo "  no ASTROLABE_BENCH line" >&2
    failed=1
    continue
  fi

  IFS='|' read -r _ bname scanned symbols fails unresolved elapsed_us elapsed_min elapsed_max peak_rss peak_min peak_max verdict <<<"$line"
  elapsed_s="$(awk -v us="$elapsed_us" 'BEGIN { printf "%.2fs", us/1e6 }')"
  peak_mb="$(awk -v b="$peak_rss" 'BEGIN { printf "%.1fMB", b/1024/1024 }')"

  printf '%-16s | %-14s | %8s | %10s | %10s | %8s | %10s | %s\n' \
    "$bname" "$note" "$scanned" "$symbols" "$fails" "$elapsed_s" "$peak_mb" "$verdict"

  ran=$((ran + 1))
  case "$verdict" in
    REGRESS|STALE_BASELINE) failed=1 ;;
  esac

  if [[ "$MODE" == "save" ]]; then
    mkdir -p "$BASELINE_DIR"
    if [[ -f "$out_json" ]]; then
      cp "$out_json" "$BASELINE_DIR/${name}.json"
      echo "  wrote $BASELINE_DIR/${name}.json" >&2
    fi
  fi
done

echo
if [[ "$skipped" -gt 0 ]]; then
  echo "跳过 $skipped 个语料："
  for r in "${skip_reasons[@]}"; do
    echo "  - $r"
  done
  echo
fi

echo "机器可读结果: $OUT_DIR/*.json"
echo "阈值: RSS ≤ max(baseline×${RSS_RATIO}, baseline+${RSS_FLOOR}B); 耗时 ≤ max(baseline×${TIME_RATIO}, baseline+${TIME_FLOOR}ms)"
echo "预热 ${WARMUP} + 实测 ${RUNS} 次，取中位数。release 构建。"

if [[ "$ran" -eq 0 ]]; then
  echo "没有跑成任何一个语料。" >&2
  exit 1
fi

if [[ "$MODE" == "ci" && "$failed" -ne 0 ]]; then
  exit 1
fi

if [[ "$failed" -ne 0 && "$MODE" != "report" ]]; then
  exit 1
fi

exit 0
