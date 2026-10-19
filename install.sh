#!/bin/sh
# Astrolabe AI installer: downloads the GitHub Release archive for this platform, verifies its sha256 against the
# release's SHA256SUMS, and installs the `astrolabe` binary. Nothing else is touched (no PATH or config edits).
#
#   curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
#   ASTROLABE_VERSION=0.14.0 ASTROLABE_INSTALL_DIR=$HOME/bin sh install.sh
#
# Env: ASTROLABE_VERSION (default: latest release), ASTROLABE_INSTALL_DIR (default: $HOME/.local/bin),
#      ASTROLABE_LIBC=musl|gnu (Linux x64 only; default: musl if ldd reports musl, else gnu)
# Supported: macOS arm64/x64, Linux x64 (gnu/musl), Linux arm64 (gnu). Windows: see docs/INSTALL_GUIDE.md.
set -eu

REPO="wojiushirencai/Astrolabe"
DIR="${ASTROLABE_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
need curl; need tar; need uname; need mktemp

os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64)                target=aarch64-apple-darwin ;;
  Darwin/x86_64)               target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64)
    libc=${ASTROLABE_LIBC:-}
    if [ -z "$libc" ]; then
      if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then libc=musl; else libc=gnu; fi
    fi
    target=x86_64-unknown-linux-$libc ;;
  Linux/aarch64|Linux/arm64)   target=aarch64-unknown-linux-gnu ;;
  *) die "unsupported platform $os/$arch (see https://github.com/$REPO/releases for manual downloads)" ;;
esac

version="${ASTROLABE_VERSION:-}"
if [ -z "$version" ]; then
  version=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$version" ] || die "could not determine latest version; set ASTROLABE_VERSION"
fi
version=${version#v}

archive="astrolabe-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/v$version"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $archive ..."
curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "download failed: $base/$archive"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

expected=$(awk -v f="$archive" '$2==f || $2=="*"f {print $1}' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "no checksum for $archive in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
else die "need sha256sum or shasum to verify the download"; fi
[ "$expected" = "$actual" ] || die "sha256 mismatch for $archive (expected $expected, got $actual); nothing installed"
say "sha256 OK ($actual)"

tar -xzf "$tmp/$archive" -C "$tmp" astrolabe || die "archive does not contain an 'astrolabe' binary"
mkdir -p "$DIR"
install -m 0755 "$tmp/astrolabe" "$DIR/astrolabe" 2>/dev/null || { cp "$tmp/astrolabe" "$DIR/astrolabe" && chmod 0755 "$DIR/astrolabe"; }
[ "$os" = Darwin ] && xattr -d com.apple.quarantine "$DIR/astrolabe" 2>/dev/null || true

say "Installed: $DIR/astrolabe ($("$DIR/astrolabe" --version 2>&1 | head -n 1))"
case ":$PATH:" in *":$DIR:"*) ;; *) say "Note: $DIR is not on your PATH. Add it, or use the absolute path in your MCP config." ;; esac
say "Next: claude mcp add --scope user astrolabe -- $DIR/astrolabe . --context=claude-code"
