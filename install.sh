#!/usr/bin/env sh
# Promptly installer (macOS / Linux).
#
# Downloads the latest prebuilt `promptly` + `promptlyd` binaries from GitHub
# Releases and installs them to a local bin directory. No Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/AidanHT/promptly-daemon/main/install.sh | sh
#
# Environment overrides:
#   PROMPTLY_VERSION       tag to install         (default: latest release)
#   PROMPTLY_INSTALL_DIR   where to put binaries   (default: $HOME/.local/bin)
set -eu

REPO="AidanHT/promptly-daemon"
INSTALL_DIR="${PROMPTLY_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '\033[0;32m%s\033[0m\n' "$*"; }
warn() { printf '\033[0;33m%s\033[0m\n' "$*" >&2; }
err()  { printf '\033[0;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v uname >/dev/null 2>&1 || err "missing required tool: uname"
command -v tar   >/dev/null 2>&1 || err "missing required tool: tar"

if command -v curl >/dev/null 2>&1; then
  fetch()    { curl -fsSL "$1"; }
  download() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch()    { wget -qO- "$1"; }
  download() { wget -qO "$2" "$1"; }
else
  err "need either curl or wget on PATH"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Linux-x86_64)                 target="x86_64-unknown-linux-gnu" ;;
  Darwin-arm64 | Darwin-aarch64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64)                target="x86_64-apple-darwin" ;;
  *)
    err "no prebuilt binary for $os-$arch.
Install from source instead (needs Rust — https://rustup.rs):
  cargo install --git https://github.com/$REPO promptly promptlyd"
    ;;
esac

tag="${PROMPTLY_VERSION:-}"
if [ -z "$tag" ]; then
  say "Resolving the latest release..."
  tag="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 \
        | sed -E 's/.*"tag_name"[ ]*:[ ]*"([^"]+)".*/\1/')"
  [ -n "$tag" ] || err "could not resolve the latest release (set PROMPTLY_VERSION=vX.Y.Z)"
fi

asset="promptly-$tag-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ..."
download "$url" "$tmp/$asset" || err "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp" || err "could not unpack $asset"

src="$tmp/promptly-$tag-$target"
mkdir -p "$INSTALL_DIR"
# Unlink before copying: overwriting a running binary in place fails with
# ETXTBSY on Linux (a running process keeps its unlinked inode, so this is safe).
rm -f "$INSTALL_DIR/promptly" "$INSTALL_DIR/promptlyd"
cp "$src/promptly" "$INSTALL_DIR/promptly"
cp "$src/promptlyd" "$INSTALL_DIR/promptlyd"
chmod 0755 "$INSTALL_DIR/promptly" "$INSTALL_DIR/promptlyd"

say "Installed promptly + promptlyd $tag to $INSTALL_DIR"

if command -v pgrep >/dev/null 2>&1 && pgrep -x promptlyd >/dev/null 2>&1; then
  warn "note: a promptlyd daemon is still running an older version — restart it with 'promptly down' then 'promptly up' (or just 'promptly start')."
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    warn "note: $INSTALL_DIR is not on your PATH. Add it, for example:
  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile && . ~/.profile"
    ;;
esac

say "Done. Run 'promptly doctor' to verify your setup."
