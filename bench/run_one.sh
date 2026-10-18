#!/usr/bin/env bash
# Run ONE benchmark cell: claude -p on a copy of the corpus repo, arm A (no MCP) or C (Astrolabe).
# usage: bench/run_one.sh TASK_ID ARM REP
# env:   ASTROLABE_BIN  path to the astrolabe binary (arm C)           [required for C]
#        BENCH_CORPORA  corpora dir (default ./bench-work/corpora)
#        BENCH_RUNS     output dir  (default ./bench-work/runs)
#        MODEL          passed to `claude --model` (default: claude's own default)
#        TMO            wall timeout seconds (default 1500)
#        BARRIER, NARMS paired start: every arm touches $BARRIER/<arm>.ready and waits for NARMS files
#        BENCH_NO_SANDBOX=1  skip bubblewrap (weaker isolation, see bench/README.md)
#        DRY_CMD        run this shell snippet instead of claude (no API call; for testing isolation)
# Credentials: claude inherits your environment (e.g. ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL). Nothing is read from or
# copied out of your real ~/.claude; the run gets a fresh, empty HOME.
set -u
T=$1; ARM=$2; REP=$3
HERE=$(cd "$(dirname "$0")" && pwd)
TASKS_FILE=${TASKS_FILE:-$HERE/tasks/tasks.json}
CORPORA=${BENCH_CORPORA:-$PWD/bench-work/corpora}
RUNS=${BENCH_RUNS:-$PWD/bench-work/runs}
TMO=${TMO:-1500}
D=$RUNS/$T-$ARM-r$REP
REPO_NAME=$(jq -r --arg t "$T" '.[]|select(.id==$t).repo' "$TASKS_FILE")
PROMPT=$(jq -r --arg t "$T" '.[]|select(.id==$t).prompt' "$TASKS_FILE")
[ -d "$CORPORA/$REPO_NAME" ] || { echo "corpus $REPO_NAME missing in $CORPORA (run bench/fetch_corpora.sh)" >&2; exit 2; }
rm -rf "$D"; mkdir -p "$D/home" "$D/stage" "$D/gocache"
cp -a "$CORPORA/$REPO_NAME" "$D/repo"
D=$(cd "$D" && pwd)

BW_EXTRA=()
case "$ARM" in
  A) echo '{"mcpServers":{}}' > "$D/stage/mcp.json" ;;
  C) [ -x "${ASTROLABE_BIN:-}" ] || { echo "ASTROLABE_BIN must point to an executable" >&2; exit 2; }
     jq -n '{mcpServers:{astrolabe:{command:"/tmp/astrolabe",args:[".","--context=claude-code"]}}}' > "$D/stage/mcp.json"
     BW_EXTRA+=(--ro-bind "$ASTROLABE_BIN" /tmp/astrolabe) ;;
  *) echo "bad arm $ARM (A or C)" >&2; exit 2 ;;
esac
[ "$ARM" = C ] && jq -n --arg b "$ASTROLABE_BIN" '{mcpServers:{astrolabe:{command:$b,args:[".","--context=claude-code"]}}}' > "$D/stage/mcp.host.json" \
  || cp "$D/stage/mcp.json" "$D/stage/mcp.host.json"
# the model must not read the answers: WebSearch/WebFetch are disabled in every arm
claude_cmd() {  # $1 = mcp config path as seen by claude
  CLAUDE=(timeout -s INT -k 30 "$TMO" claude -p "$PROMPT" ${MODEL:+--model "$MODEL"} --output-format stream-json --verbose
          --permission-mode bypassPermissions --disallowedTools WebSearch WebFetch --strict-mcp-config --mcp-config "$1")
  [ -n "${DRY_CMD:-}" ] && CLAUDE=(bash -c "$DRY_CMD")
  return 0
}

if [ -n "${BARRIER:-}" ]; then
  mkdir -p "$BARRIER"; touch "$BARRIER/$ARM.ready"
  for _ in $(seq 1 600); do [ "$(ls "$BARRIER" | grep -c ready)" -ge "${NARMS:-2}" ] && break; sleep 0.5; done
fi

START=$(date +%s.%N)
if [ -z "${BENCH_NO_SANDBOX:-}" ] && command -v bwrap >/dev/null; then
  # $HOME, /tmp and the bench/corpora/runs dirs are tmpfs: the model sees only the repo copy, the MCP config, toolchains (ro) and
  # a per-run Go build cache. Task files, scorers and other runs' transcripts are not visible.
  BW=(--ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs "$HOME")
  # hide task files, scorers, corpora and all runs (incl. other arms' transcripts) from the model
  for p in "$HERE" "$CORPORA" "$RUNS" "$(dirname "$HERE")"; do
    p=$(realpath -m "$p"); [ -d "$p" ] || continue
    case "$HOME/" in "$p"/*|"$p"/) continue;; esac     # never blank an ancestor of $HOME
    case "$p" in /|/tmp|/usr|/etc|/home|/bin|/lib*|/opt|/var) continue;; esac
    BW+=(--tmpfs "$p")
  done
  for p in "$HOME/.local" "$HOME/.rustup" "$HOME/go"; do [ -d "$p" ] && BW+=(--ro-bind "$p" "$p"); done
  [ -d "$HOME/.cargo" ] && BW+=(--bind "$HOME/.cargo" "$HOME/.cargo")
  mkdir -p "$HOME/.cache" 2>/dev/null || true
  BW+=(--bind "$D/gocache" "$HOME/.cache/go-build" --bind "$D/repo" /tmp/repo --ro-bind "$D/stage" /tmp/ab)
  claude_cmd /tmp/ab/mcp.json
  bwrap "${BW[@]}" "${BW_EXTRA[@]}" --chdir /tmp/repo --unshare-pid --die-with-parent "${CLAUDE[@]}" \
    < /dev/null > "$D/stream.jsonl" 2> "$D/stderr.log"
  EC=$?
else
  echo "WARNING: running without bubblewrap; the model can see the whole filesystem (incl. $HERE)" >&2
  claude_cmd "$D/stage/mcp.host.json"
  (cd "$D/repo" && HOME="$D/home" "${CLAUDE[@]}" < /dev/null > "$D/stream.jsonl" 2> "$D/stderr.log")
  EC=$?
fi
END=$(date +%s.%N)
echo "$EC" > "$D/exit"; python3 -c "print($END-$START)" > "$D/wall"
# independent post-run check on the repo copy (outside the sandbox), then drop the copy
python3 "$HERE/checks.py" "$T" "$D/repo" > "$D/check.json" 2> "$D/check.err"
git -C "$D/repo" diff > "$D/diff.patch" 2>/dev/null
[ -n "${KEEP_REPO:-}" ] || rm -rf "$D/repo"
rm -rf "$D/home" "$D/gocache"
