[English](install.md) | [中文](install.zh-CN.md)

# Install AginxBrowser

> This document is written **for AI Agents to read themselves**. A user pastes the sentence below to their Agent; after reading this document, the Agent can complete the setup on its own:
>
> ```
> Help me install AginxBrowser: https://raw.githubusercontent.com/yinnho/aginxbrowser/main/docs/install.md
> ```

AginxBrowser is a browser engine built for AI agents. One Rust binary with V8 built in — no Chromium dependency. It can read web pages, search the whole web, take screenshots, and interact (click / type / scroll). **The HTTP API is the whole interface**: install locally, start the server, and every capability is one `curl` POST away.

---

## 0. Prerequisites

- A machine to run it on (macOS / Linux / Windows).
- `curl` (for verification and first calls).

No Node, no Chromium, no Docker, and no API key needed.

---

## 1. Install and start

### Option A: Homebrew (macOS / Linuxbrew)

```bash
# Homebrew 7 gates third-party taps behind an explicit trust step
# (older brew has no such command and needs none)
brew trust yinnho/aginxbrowser
brew install yinnho/aginxbrowser/aginxbrowser
aginxbrowser          # starts the HTTP server on 0.0.0.0:8089
```

### Option B: One-line installer

```bash
# Download, review, then run — never blind-pipe a network script
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh -o install.sh
less install.sh
bash install.sh
```

Detects your platform, downloads the prebuilt binary, verifies the SHA-256, installs to `/usr/local/bin` or `~/.local/bin` (override with `AGINXBROWSER_BIN_DIR=...`, pin with `AGINXBROWSER_VERSION=v0.5.x`), and finishes with a self-check.

### Option C: Manual prebuilt download

Prebuilt binaries per release (macOS Apple Silicon / Linux x86_64 / Windows x86_64; Windows assets ship from v0.3.1 — the Intel-macOS asset was dropped in v0.2.10, its baked V8 snapshot carried the wrong architecture):

```bash
VER=v0.5.26
OS=$(uname -s); ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64)  T=aarch64-apple-darwin ;;
  Linux-x86_64)  T=x86_64-unknown-linux-gnu ;;
  MINGW*-x86_64) T=x86_64-pc-windows-msvc ;;   # git-bash; PowerShell users: pick the asset by hand
  *) echo "unsupported: $OS-$ARCH"; exit 1 ;;
esac
curl -fsSL -o aginxbrowser.tar.gz \
  "https://github.com/yinnho/aginxbrowser/releases/download/${VER}/aginxbrowser-${VER}-${T}.tar.gz"
tar xzf aginxbrowser.tar.gz && cd aginxbrowser-${VER}-${T}
./aginxbrowser   # serves the HTTP API on 0.0.0.0:8089 by default (.exe on Windows)
```

Verify the download with the matching `.sha256` file in the same release. Release binaries carry the full feature set (stealth TLS + screenshots); `doctor` reports the compiled-in feature set.

The archive also carries the WeChat OA article composer — `workflow/wechat-oa-post/` (`md_to_args.py` + `templates/`), caller-side tooling that turns a markdown post into `args_json` for the baked-in `wechat-oa-post` flow.

### Option D: Build from source

```bash
git clone https://github.com/yinnho/aginxbrowser.git
cd aginxbrowser
cargo build --release --features stealth,screenshot   # ~4 minutes
./target/release/aginxbrowser                          # listens on 0.0.0.0:8089 by default
```

---

## 2. Verify

```bash
# Server up?
curl -sS http://127.0.0.1:8089/health

# Capability list (no network fetch triggered, instant reply)
curl -sS http://127.0.0.1:8089/doctor | jq .

# Want to confirm the fetch pipeline actually works? Run a live probe (fetches example.com once)
curl -sS 'http://127.0.0.1:8089/doctor?probe=true' | jq .
```

`/doctor` returns `capabilities` (whether screenshot / stealth / captcha_solver are available), `search_engines`, and `endpoints`. With `?probe=true` it additionally performs one real fetch and reports `ok` / `latency_ms`.

---

## 3. First calls

```bash
BASE=http://127.0.0.1:8089

# Read a page -> markdown
curl -sS -X POST $BASE/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# Search (Baidu/Bing/Sogou/Sogou WeChat/Google, aggregated)
curl -sS -X POST $BASE/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook price","max_results":5}'

# Multi-step interaction: create a session, act by index, close
# 1. POST $BASE/session/create {"url":"https://site.com/login"} -> session_id
# 2. GET  $BASE/session/$ID/state                              -> [N] indexes
# 3. POST $BASE/session/$ID/input  {"index":1,"text":"user"}
# 4. POST $BASE/session/$ID/click  {"index":3}                 -> submit
```

Or just ask the Agent in natural language once the skill (below) is installed: "read this web page", "search for macbook prices", "log into this website and paginate to page two".

---

## 4. (Optional) Install SKILL.md so the Agent triggers it proactively

The API is available to any process, but the Agent doesn't necessarily know **when** to reach for it. Drop `SKILL.md` into the skills directory and the Agent will proactively invoke it on tasks like "read a web page / search / screenshot / interact":

```bash
bash skill.sh   # from the repo root; downloads SKILL.md and verifies the local instance
# or by hand:
mkdir -p ~/.claude/skills/aginxbrowser
curl -sS https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md \
  -o ~/.claude/skills/aginxbrowser/SKILL.md
```

---

## Environment variables

| Variable | Default | Description |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | Listen address (bind `127.0.0.1:8089` to keep it loopback-only) |
| `AGINXBROWSER_TOKEN` | none | When set, every route requires this as a bearer token |
| `AGINXBROWSER_PROXY` | none | Proxy address (applied when `use_proxy:true`, and applied automatically for browser/session navigations to known-blocked domains — wikipedia.org, github.com, …). Standard `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` are ignored by the engine — `AGINXBROWSER_PROXY` is the only proxy knob |
| `CAPTCHA_SOLVER_API_KEY` | none | 2captcha key; when set, CAPTCHAs are solved automatically |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` cache TTL (seconds); `0` disables caching |
| `AGINXBROWSER_DOMAIN_RATE_PER_MIN` | `20` | Per-domain pages/minute cap (429 beyond; `0` disables) — aginxbrowser is for real-time lookups, not crawling |
| `AGINXBROWSER_SESSION_PAGE_LIMIT` | `200` | Pages per interactive session (`0` disables) |
| `AGINXBROWSER_STORE_SCOPE` | `global` | Local persistent cache scope; `session` keeps everything in memory only |

---

## Troubleshooting

- **Connection refused**: the server isn't running — start `aginxbrowser` and check `curl http://127.0.0.1:8089/health`.
- **Tools won't respond**: start with `curl 'http://127.0.0.1:8089/doctor?probe=true'` and check `probe.ok` and `probe.error`.
- **Screenshot unavailable**: `/doctor` reports `capabilities.screenshot` as false — build from source with `--features stealth,screenshot`; use `fetch` or `/v1/scrape` meanwhile.
- **Sites in other regions unreadable**: Pass `use_proxy: true` to `fetch` / `search` (needs `AGINXBROWSER_PROXY` set).
- **Blocked by Cloudflare**: Bypassed automatically by default; if still blocked, try a different `tls_fingerprint` (firefox133 / safari18, etc.).
- **Content behind a login wall**: Pass `cookies: ["name=value", ...]` to `fetch` to inject session cookies.

Full route reference: [API.md](https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md).

---

© 2026 OpenCarrier · Apache-2.0 open source
