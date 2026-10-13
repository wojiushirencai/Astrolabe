#!/usr/bin/env bash
# Fetch the pinned acceptance corpora into corpus/.
#
# Pins are tags or full commit SHAs — never a moving branch tip. Upstream
# changes must not silently retune the acceptance gate numbers.
#
# Override the destination with CORPUS_ROOT (default: <workspace>/corpus)
# so CI and local dry-runs can clone without touching an existing tree.
#
# Evidence for each pin (2026-09-11, dart added 2026-10-02):
#   gin              tag v1.12.0              local corpus/go/version.go
#   gson             tag gson-parent-2.14.0   existing CI clone + GitHub tag
#   ripgrep          tag 15.2.0               local corpus/rust/Cargo.toml
#   serena           commit 701e7c84…         origin/main; local feat HEAD
#                                             48609832 is unpublished
#   openvisio-oss    commit bdb1d2a3…         local HEAD == origin/main
#   lang8/*          commits = HEADs of the PR #51 (2026-10-12) recall-bench clones
#   flutter-packages tag webview_flutter_wkwebview-v3.27.0 local corpus/dart (GitHub tag)
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
  "dart|https://github.com/flutter/packages.git|webview_flutter_wkwebview-v3.27.0|"
  # lang8 declaration-recall gate (scripts/recall-gate), pins = PR #51 bench HEADs
  "lang8/swift|https://github.com/apple/swift-argument-parser.git|efd239f0055b3d6b6cc122a786c46a0ea8d0235b|"
  "lang8/objc|https://github.com/AFNetworking/AFNetworking.git|d9f589cc2c1fe9d55eb5eea00558010afea7a41e|"
  "lang8/kotlin|https://github.com/square/okhttp.git|42e5888d45623662c138ed27e80ea28bbe0aa5cc|"
  "lang8/ruby|https://github.com/sinatra/sinatra.git|cb22afd7902b566b6eaba6c4ea89739494a65d12|"
  "lang8/shell|https://github.com/bats-core/bats-core.git|0c92eb4c418d59fcb1a1a8c1d7a015f8149689f2|"
  "lang8/lua|https://github.com/neovim/nvim-lspconfig.git|6d27c288ff951e1e94cb5f91c800adeddf0dc0c2|"
  "lang8/vue|https://github.com/vbenjs/vue-vben-admin.git|4634655c621338abd97386350e5dc3d9119dab2c|"
  # larger lang8 corpora, pins = recall-bench clones (2026-10-12)
  "lang8/swift-alamofire|https://github.com/Alamofire/Alamofire.git|bda9ed57d72988a3a2ada33d824583541f86eac6|--filter=blob:none"
  "lang8/objc-sdwebimage|https://github.com/SDWebImage/SDWebImage.git|c3ad5e1a9bf55c9b76d4c362430b5fcded96c502|--filter=blob:none"
  "lang8/objcpp-mmkv|https://github.com/Tencent/MMKV.git|ad7657ef9d120dbcdd7432d75aa6c59391149b22|--filter=blob:none"
  "lang8/kotlin-coroutines|https://github.com/Kotlin/kotlinx.coroutines.git|113ee1f3c0b0ca0b7a809e75dd8d2340290121c6|--filter=blob:none"
  "lang8/ruby-rubocop|https://github.com/rubocop/rubocop.git|103a818b5c37f0d043fcce4eb6f1b858ec46aa51|--filter=blob:none"
  "lang8/shell-nvm|https://github.com/nvm-sh/nvm.git|b1b3a0038c0666c7436cb5d383608b4b72c8300b|--filter=blob:none"
  "lang8/lua-luals|https://github.com/LuaLS/lua-language-server.git|66d6405138fd472334d87785c1f90388eabfa9d7|--filter=blob:none"
  "lang8/vue-element-plus|https://github.com/element-plus/element-plus.git|0a56b818fb879092b6bd8e0a2916b161fa760796|--filter=blob:none"
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

TARGET="${1:-}"

mkdir -p "$CORPUS_ROOT"

for entry in "${CORPORA[@]}"; do
  IFS='|' read -r dest_name url ref extra_flags <<<"$entry"
  if [[ -n "$TARGET" && "$TARGET" != "all" ]]; then
    if [[ "$dest_name" != "$TARGET" ]]; then
      if [[ "$dest_name" == "dart" && "$TARGET" == "flutter-packages" ]]; then
        :
      elif [[ "$TARGET" == "lang8" && "$dest_name" == lang8/* ]]; then
        :
      else
        continue
      fi
    fi
  fi
  fetch_one "$dest_name" "$url" "$ref" "$extra_flags"
done

echo
echo "corpora ready under $CORPUS_ROOT"
du -sh "$CORPUS_ROOT" "$CORPUS_ROOT"/* 2>/dev/null | sort -h
