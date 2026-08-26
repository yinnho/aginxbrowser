#!/usr/bin/env bash
#
# AginxBrowser binary installer (self-host).
#
# Downloads the prebuilt release for your platform, verifies the SHA-256,
# and drops the binary into PREFIX (default ~/.local/bin). The hosted
# instance at browser.aginx.net needs none of this — this script is for
# running your own engine.
#
# Usage (download, review, then run — never blind-pipe from the network):
#   curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh -o install.sh
#   less install.sh
#   bash install.sh
#
# Overrides:
#   VERSION=v0.2.0          pin a release (default: latest)
#   PREFIX=/usr/local/bin   install target (default: ~/.local/bin)
#
set -euo pipefail

REPO="yinnho/aginxbrowser"
VERSION="${VERSION:-}"

# ── resolve version (default: latest release) ───────────────────────────────
if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  if [ -z "$VERSION" ]; then
    echo "[fail] could not resolve the latest release (GitHub API unreachable?)" >&2
    echo "       pin one instead: VERSION=v0.2.0 bash install.sh" >&2
    exit 1
  fi
fi
VER_NUM="${VERSION#v}"

# ── platform → target triple ────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
  Darwin-arm64)  T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64)  T=x86_64-unknown-linux-gnu ;;
  *)
    echo "[fail] no prebuilt binary for $OS-$ARCH." >&2
    echo "       build from source instead:" >&2
    echo "         git clone https://github.com/$REPO.git && cd aginxbrowser" >&2
    echo "         cargo build --release --features stealth,screenshot" >&2
    exit 1
    ;;
esac

BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
TARBALL="aginxbrowser-$VERSION-$T.tar.gz"
PREFIX="${PREFIX:-$HOME/.local/bin}"
BIN="$PREFIX/aginxbrowser"

echo "==> AginxBrowser installer"
echo "    release: $VERSION ($T)"
echo "    target:  $BIN"
echo ""

# ── download + verify ───────────────────────────────────────────────────────
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> downloading $TARBALL ..."
curl -fsSL -o "$TMP/$TARBALL" "$BASE_URL/$TARBALL"

echo "==> verifying sha256 ..."
curl -fsSL -o "$TMP/$TARBALL.sha256" "$BASE_URL/$TARBALL.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$TMP" && shasum -a 256 -c "$TARBALL.sha256")
elif command -v sha256sum >/dev/null 2>&1; then
  (cd "$TMP" && sha256sum -c "$TARBALL.sha256")
else
  echo "[warn] no sha256 tool found — skipping integrity check" >&2
fi

# ── install ─────────────────────────────────────────────────────────────────
echo "==> installing to $BIN ..."
tar xzf "$TMP/$TARBALL" -C "$TMP"
mkdir -p "$PREFIX"
mv "$TMP/aginxbrowser-$VERSION-$T/aginxbrowser" "$BIN"
chmod +x "$BIN"

# ── self-check ──────────────────────────────────────────────────────────────
echo ""
echo "==> running doctor ..."
if "$BIN" doctor; then
  echo ""
  echo "==> done."
else
  echo ""
  echo "==> installed, but doctor reported a problem above (often just egress"
  echo "    being firewalled — set AGINXBROWSER_PROXY if you need a proxy)."
fi
echo ""
echo "    start a server:  $BIN            # HTTP API on 0.0.0.0:8089"
echo "    register in claude code:"
echo "      claude mcp add aginxbrowser --transport http http://127.0.0.1:8089/mcp"
echo "    docs: https://github.com/$REPO/blob/main/docs/API.md"
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo ""
     echo "    [note] $PREFIX is not on your PATH — add it to use 'aginxbrowser' directly:" \
          "echo 'export PATH=\"$PREFIX:\$PATH\"' >> ~/.zshrc (or ~/.bashrc)" ;;
esac
