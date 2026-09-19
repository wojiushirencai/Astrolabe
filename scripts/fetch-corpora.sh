#!/usr/bin/env bash
# Fetch the five pinned acceptance corpora into corpus/.
#
# Pins are tags or full commit SHAs — never a moving branch tip. Upstream
# changes must not silently retune the five-language gate numbers.
#
# Override the destination with CORPUS_ROOT (default: <workspace>/corpus)
# so CI and local dry-runs can clone without touching an existing tree.
#
# Evidence for each pin (2026-09-11):
#   gin              tag v1.12.0              local corpus/go/version.go
#   gson             tag gson-parent-2.14.0   existing CI clone + GitHub tag
#   ripgrep          tag 15.2.0               local corpus/rust/Cargo.toml
#   serena           commit 701e7c84…         origin/main; local feat HEAD
#                                             48609832 is unpublished
#   openvisio-oss    commit bdb1d2a3…         local HEAD == origin/main
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS_ROOT="${CORPUS_ROOT:-$ROOT/corpus}"

export GIT_TERMINAL_PROMPT=0

# dest_relative|https_url|ref|extra clone flags (space-separated, or empty)
CORPORA=(
  "go|https://github.com/gin-gonic/gin.git|v1.12.0|"
  "java|https://github.com/google/gson.git|gson-parent-2.14.0|"
  "rust|https://github.com/BurntSushi/ripgrep.git|15.2.0|"
  "python|https://github.com/oraios/serena.git|701e7c843f46c6a649203a488cece1bf19f1df90|--filter=blob:none"
  "typescript|https://github.com/syntaxPriest/openvisio-oss.git|bdb1d2a366d74ec92cc6d8968aed384ffad0c508|--filter=blob:none"
)

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ ]]
}

git_dir() {
  git -C "$1" rev-parse --git-dir >/dev/null 2>&1
}

current_commit() {
  git -C "$1" rev-parse HEAD
}

# True when dest is a git checkout of the exact pinned ref.
matches_pin() {
  local dest="$1" ref="$2"
  git_dir "$dest" || return 1
  local head
  head="$(current_commit "$dest")"
  if is_sha "$ref"; then
    [[ "$head" == "$ref" ]]
    return
  fi
  local peeled
  peeled="$(git -C "$dest" rev-parse "$ref^{commit}" 2>/dev/null || true)"
  [[ -n "$peeled" && "$head" == "$peeled" ]]
}

dir_nonempty() {
  local dest="$1"
  [[ -d "$dest" ]] && [[ -n "$(ls -A "$dest" 2>/dev/null || true)" ]]
}

clone_at_ref() {
  local dest="$1" url="$2" ref="$3"
  shift 3
  local extra=("$@")

  mkdir -p "$(dirname "$dest")"
  rm -rf "$dest"

  if is_sha "$ref"; then
    mkdir -p "$dest"
    git -C "$dest" init --quiet
    git -C "$dest" remote add origin "$url"
    # blob:none (if requested) keeps the first fetch small; checkout then
    # pulls only the trees we need. Depth 1 still pins a single commit.
    git -C "$dest" fetch --depth 1 --quiet "${extra[@]}" origin "$ref"
    git -C "$dest" checkout --force --quiet FETCH_HEAD
  else
    git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$ref" \
      --single-branch "${extra[@]}" "$url" "$dest"
  fi

  if ! matches_pin "$dest" "$ref"; then
    echo "error: $dest HEAD=$(current_commit "$dest") does not match pin $ref" >&2
    exit 1
  fi
}

fetch_one() {
  local dest_name="$1" url="$2" ref="$3" extra_flags="$4"
  local dest="$CORPUS_ROOT/$dest_name"
  local extra=()
  # shellcheck disable=SC2206
  [[ -n "$extra_flags" ]] && extra=($extra_flags)

  if dir_nonempty "$dest"; then
    if matches_pin "$dest" "$ref"; then
      echo "skip $dest_name (already $ref)"
      return
    fi
    if ! git_dir "$dest"; then
      echo "skip $dest_name (exists without git metadata; not overwriting)"
      return
    fi
    echo "replace $dest_name (HEAD=$(current_commit "$dest") != $ref)"
  else
    echo "clone $dest_name @ $ref"
  fi

  clone_at_ref "$dest" "$url" "$ref" "${extra[@]}"
  echo "ok    $dest_name  $(current_commit "$dest")  $(git -C "$dest" describe --tags --always)"
}

mkdir -p "$CORPUS_ROOT"

for entry in "${CORPORA[@]}"; do
  IFS='|' read -r dest_name url ref extra_flags <<<"$entry"
  fetch_one "$dest_name" "$url" "$ref" "$extra_flags"
done

echo
echo "corpora ready under $CORPUS_ROOT"
du -sh "$CORPUS_ROOT" "$CORPUS_ROOT"/* 2>/dev/null | sort -h
