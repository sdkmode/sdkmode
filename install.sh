#!/bin/sh
# sdkmode installer.
#
#   curl -fsSL https://sh.sdkmo.de | sh
#
# Env overrides:
#   SDKMODE_VERSION      install a specific tag (default: latest release)
#   SDKMODE_INSTALL_DIR  install location (default: $HOME/.local/bin)
set -eu

REPO="sdkmode/sdkmode"
BIN="sdkmode"
INSTALL_DIR="${SDKMODE_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1" >&2; }

command -v curl >/dev/null 2>&1 || err "curl is required"

# Map this machine to one of the targets we publish binaries for.
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "no prebuilt binary for Linux $arch" ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      *) err "no prebuilt binary for macOS $arch (only Apple Silicon is published)" ;;
    esac
    ;;
  *) err "unsupported OS: $os" ;;
esac

# Resolve the version: $SDKMODE_VERSION, else the latest release's tag.
tag="${SDKMODE_VERSION:-}"
if [ -z "$tag" ]; then
  tag=$(
    curl -fsL "https://api.github.com/repos/$REPO/releases/latest" \
      | grep '"tag_name"' | head -n1 \
      | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
  ) || true
fi
[ -n "$tag" ] || err "could not determine the latest release of $REPO"

asset="$BIN-$tag-$target"
url="https://github.com/$REPO/releases/download/$tag/$asset"

info "Installing $BIN $tag ($target)..."
tmp=$(mktemp "${TMPDIR:-/tmp}/sdkmode.XXXXXX") || err "could not create a temp file"
sum=$(mktemp "${TMPDIR:-/tmp}/sdkmode.XXXXXX") || err "could not create a temp file"
trap 'rm -f "$tmp" "$sum"' EXIT
curl -fSL --progress-bar "$url" -o "$tmp" || err "download failed: $url"

# Best-effort SHA-256 integrity check. The release is expected to publish a
# "<asset>.sha256" alongside each binary (this depends on the release workflow
# producing those files; older releases predate them). Behaviour:
#   - checksum file present + matches  -> proceed
#   - checksum file present + mismatch -> hard fail (fail-closed)
#   - checksum file absent             -> warn and proceed (backward compat)
#   - no sha256 tool available         -> warn and proceed
if curl -fsSL "$url.sha256" -o "$sum" 2>/dev/null; then
  # The expected digest is the first whitespace-delimited field (handles both
  # bare digests and "<digest>  <filename>" formats).
  expected=$(cut -d' ' -f1 "$sum" | head -n1)
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp" | cut -d' ' -f1)
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
  else
    actual=""
    info "warning: no sha256sum/shasum found; skipping checksum verification"
  fi
  if [ -n "$actual" ]; then
    [ "$expected" = "$actual" ] \
      || err "checksum mismatch for $asset (expected $expected, got $actual)"
    info "Checksum verified."
  fi
else
  info "warning: no published checksum for this release; skipping verification"
fi

chmod +x "$tmp"

mkdir -p "$INSTALL_DIR" || err "could not create $INSTALL_DIR"
mv "$tmp" "$INSTALL_DIR/$BIN" \
  || err "could not write to $INSTALL_DIR (set SDKMODE_INSTALL_DIR, or re-run with sudo)"
rm -f "$sum"
trap - EXIT

info "Installed $BIN to $INSTALL_DIR/$BIN"

# Nudge if the install dir isn't on PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    info ""
    info "$INSTALL_DIR is not on your PATH. Add it, e.g.:"
    info "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
