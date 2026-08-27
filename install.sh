#!/bin/sh
# aginxbrowser installer — the one-liner entry point:
#   curl -fsSL https://browser.aginx.net/install.sh | sh
# (raw mirror until the domain alias is up:
#   curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh | sh)
#
# Downloads the release binary for the running OS/arch, verifies its sha256
# against the release-published checksum, installs to a bin dir, and finishes
# with `aginxbrowser doctor` so a broken environment says so immediately.
#
# Env overrides (namespaced and legacy spellings both accepted):
#   AGINXBROWSER_VERSION / VERSION          pin a release (e.g. v0.2.3); default: latest
#   AGINXBROWSER_BIN_DIR / PREFIX           install target; default: /usr/local/bin
#                                           if writable, else ~/.local/bin
#   AGINXBROWSER_GH_PROXY                   prefix mirror for github.com downloads,
#                                           for networks where GitHub is slow/blocked
#                                           (e.g. https://ghfast.top/ — third-party)
set -eu

repo="yinnho/aginxbrowser"
say() { printf '%s\n' "$*" >&2; }

# ---- platform → release target -------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Darwin) vendor="apple-darwin" ;;
    Linux)  vendor="unknown-linux-gnu" ;;
    *) say "unsupported OS: $os (macOS and Linux only)"; exit 1 ;;
esac
case "$arch" in
    x86_64|amd64)  cpu="x86_64" ;;
    aarch64|arm64) cpu="aarch64" ;;
    *) say "unsupported arch: $arch"; exit 1 ;;
esac
target="${cpu}-${vendor}"
if [ "$target" = "aarch64-unknown-linux-gnu" ]; then
    say "no prebuilt linux/arm64 binary yet — use the multi-arch Docker image instead:"
    say "  docker run -d -p 8089:8089 ghcr.io/yinnho/aginxbrowser:latest"
    say "or build from source: cargo build --release --features stealth,screenshot"
    exit 1
fi

# ---- version ---------------------------------------------------------------
version="${AGINXBROWSER_VERSION:-${VERSION:-}}"
if [ -z "$version" ]; then
    version=$(curl -fsSL --connect-timeout 10 "https://api.github.com/repos/$repo/releases/latest" \
        | sed -n 's/.*"tag_name": *"\(v[^"]*\)".*/\1/p' | head -1)
    [ -n "$version" ] || { say "could not resolve latest release (api.github.com unreachable?) — pin one: AGINXBROWSER_VERSION=v0.2.3 sh install.sh"; exit 1; }
fi
asset="aginxbrowser-${version}-${target}.tar.gz"
say ">> aginxbrowser $version for $target"

# ---- download + verify -----------------------------------------------------
gh="https://github.com/$repo/releases/download/${version}"
base="${AGINXBROWSER_GH_PROXY:-}$gh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch() { curl -fsSL --retry 2 --connect-timeout 15 -o "$2" "$1"; }

if ! fetch "$base/$asset" "$tmp/$asset"; then
    say ""
    say "download failed from $gh"
    say "if GitHub is slow/blocked on your network, retry through a mirror:"
    say "  AGINXBROWSER_GH_PROXY=https://ghfast.top/ sh -c 'curl -fsSL <install-url> | sh'"
    say "(mirrors are third-party; homebrew is an alternative: brew install yinnho/aginxbrowser/aginxbrowser)"
    exit 1
fi
fetch "$base/$asset.sha256" "$tmp/$asset.sha256"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && grep "  $asset\$" "$asset.sha256" | sha256sum -c -)
elif command -v shasum >/dev/null 2>&1; then
    want=$(awk '{print $1}' "$tmp/$asset.sha256")
    got=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
    [ "$want" = "$got" ] || { say "sha256 mismatch: expected $want, got $got"; exit 1; }
else
    say ">> [warn] no sha256 tool found — skipping integrity check"
fi
say ">> sha256 ok"

# ---- install ---------------------------------------------------------------
bindir="${AGINXBROWSER_BIN_DIR:-${PREFIX:-}}"
if [ -z "$bindir" ]; then
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        bindir=/usr/local/bin
    else
        bindir="$HOME/.local/bin"
    fi
fi
mkdir -p "$bindir"
# tarballs are one directory deep: aginxbrowser-vX.Y.Z-target/aginxbrowser
tar -xzf "$tmp/$asset" -C "$tmp"
src=$(find "$tmp" -mindepth 2 -maxdepth 2 -name aginxbrowser -type f | head -1)
[ -n "$src" ] || { say "binary not found inside $asset"; exit 1; }
install -m 0755 "$src" "$bindir/aginxbrowser" 2>/dev/null || { cp "$src" "$bindir/aginxbrowser" && chmod 0755 "$bindir/aginxbrowser"; }
say ">> installed: $bindir/aginxbrowser"

# ---- doctor ----------------------------------------------------------------
if "$bindir/aginxbrowser" doctor; then
    :
else
    say ">> installed, but doctor reported a problem above (often just egress"
    say "   being firewalled — set AGINXBROWSER_PROXY if you need a proxy)."
fi

say ""
say "    start a server:   $bindir/aginxbrowser            # HTTP API on 0.0.0.0:8089"
say "    register in claude code:"
say "      claude mcp add aginxbrowser --transport http http://127.0.0.1:8089/mcp"
say "    docs: https://github.com/$repo/blob/main/docs/API.md"
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) say ""
       say "    [note] $bindir is not on your PATH — add it to use 'aginxbrowser' directly:"
       say "      echo 'export PATH=\"$bindir:\$PATH\"' >> ~/.zshrc  (or ~/.bashrc)" ;;
esac
