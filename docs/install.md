[English](install.md) | [中文](install.zh-CN.md)

# Install AginxBrowser

> This document is written **for AI Agents to read themselves**. A user pastes the sentence below to their Agent; after reading this document, the Agent can complete the setup on its own:
>
> ```
> Help me install AginxBrowser: https://raw.githubusercontent.com/yinnho/aginxbrowser/main/docs/install.md
> ```

AginxBrowser is a browser engine built for AI agents. One Rust binary with V8 built in — no Chromium dependency. It can read web pages, search the whole web, take screenshots, and interact (click / type / scroll).

**This project runs a public hosted instance**, so in most cases you don't need to deploy anything yourself — one command connects you.

---

## 0. Prerequisites

- An MCP client with HTTP transport support: Claude Code / Claude Desktop / Cursor / Windsurf, etc.
- Reachable access to `https://browser.aginx.net` (public internet).

No Node, no Chromium, no Docker, and no API key needed.

---

## 1. One-line setup (hosted instance, recommended)

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

For other clients, the equivalent config (write it into the corresponding settings file):

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "type": "http",
      "url": "https://browser.aginx.net/mcp"
    }
  }
}
```

---

## 2. Verify

```bash
# Confirm aginxbrowser appears in the list
claude mcp list

# Get the hosted instance's capability list (no network fetch triggered, instant reply)
curl -sS https://browser.aginx.net/doctor | jq .

# Want to confirm the fetch pipeline actually works? Run a live probe (fetches example.com once)
curl -sS 'https://browser.aginx.net/doctor?probe=true' | jq .
```

`/doctor` returns `capabilities` (whether screenshot / stealth / captcha_solver are available), `search_engines`, and `endpoints`. With `?probe=true` it additionally performs one real fetch and reports `ok` / `latency_ms`.

---

## 3. First calls

Once MCP is connected, just ask the Agent in natural language, or call the tools explicitly:

- "Read this web page for me: https://example.com" → `fetch`
- "Search for macbook prices" → `search`
- "Take a screenshot so I can see what this page looks like" → `screenshot` (requires the hosted instance to have the screenshot feature enabled; `/doctor` will tell you)
- "Download this zip / this pdf and save it" → `download` (streams to disk, SHA-256 verified, resumable)
- "Log into this website and paginate to page two" → `session_create` + `session_state` + `session_click`/`session_input`

The HTTP API works when called directly too (without going through MCP):

```bash
curl -sS -X POST https://browser.aginx.net/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

---

## 4. (Optional) Install SKILL.md so the Agent triggers it proactively

After MCP is connected the tools are available, but the Agent doesn't necessarily know **when** to reach for them. Drop the repo-root `SKILL.md` into the skills directory and the Agent will proactively invoke it on tasks like "read a web page / search / screenshot / interact":

```bash
mkdir -p ~/.claude/skills/aginxbrowser
curl -sS https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md \
  -o ~/.claude/skills/aginxbrowser/SKILL.md
```

---

## 5. (Optional) Self-hosting

If the hosted instance covers your needs, skip this step.

### Option A: Homebrew (macOS / Linuxbrew)

```bash
brew install yinnho/aginxbrowser/aginxbrowser
aginxbrowser doctor   # features + fonts + egress self-check
```

### Option B: Docker

```bash
docker run -d -p 8089:8089 ghcr.io/yinnho/aginxbrowser:latest
curl -sS http://127.0.0.1:8089/health
```

### Option C: One-line installer

```bash
# Download, review, then run — never blind-pipe a network script
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh -o install.sh
less install.sh
bash install.sh
```

Detects your platform, downloads the prebuilt v0.2.0+ binary, verifies the SHA-256, installs to `~/.local/bin` (override with `PREFIX=...`, pin with `VERSION=v0.2.0`), and finishes with a self-check:

```bash
aginxbrowser doctor   # features + bundled fonts + env posture + one egress probe
```

### Option C′: Manual prebuilt download

Prebuilt binaries for v0.2.0 (macOS Apple Silicon / macOS Intel / Linux x86_64):

```bash
VER=v0.2.0
OS=$(uname -s); ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-gnu ;;
  *) echo "unsupported: $OS-$ARCH"; exit 1 ;;
esac
curl -fsSL -o aginxbrowser.tar.gz \
  "https://github.com/yinnho/aginxbrowser/releases/download/${VER}/aginxbrowser-${VER}-${T}.tar.gz"
tar xzf aginxbrowser.tar.gz && cd aginxbrowser-${VER}-${T}
./aginxbrowser   # serves the HTTP API on 0.0.0.0:8089 by default
```

Verify the download with the matching `.sha256` file in the same release.

### Option D: Build from source

```bash
git clone https://github.com/yinnho/aginxbrowser.git
cd aginxbrowser
cargo build --release --features stealth,screenshot   # ~4 minutes
./target/release/aginxbrowser                          # listens on 0.0.0.0:8089 by default
```

Environment variables:

| Variable | Default | Description |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | Listen address (for public deployments, bind 127.0.0.1 behind an nginx reverse proxy instead) |
| `AGINXBROWSER_PROXY` | none | Proxy address (used when `use_proxy:true`, for fetching sites from other regions) |
| `CAPTCHA_SOLVER_API_KEY` | none | 2captcha key; when set, CAPTCHAs are solved automatically |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` cache TTL (seconds); `0` disables caching |

---

## Capability list (14 MCP tools)

| Tool | Purpose |
|------|------|
| `fetch` | Read a web page → markdown/html/text (tiered rendering, stealth, js_extract) |
| `search` | Multi-engine aggregated search (Baidu/Bing/Sogou/Sogou WeChat/Google), image search supported |
| `eval` | Execute JS on the page (async/Promise supported) |
| `click` | Load a page and click a CSS selector |
| `download` | Stream a file to disk over HTTP(S) (SHA-256 verified, resumable) — binaries, archives, datasets |
| `session_create` | Create a persistent interactive session (multi-step login / form filling / pagination); supports `cookies` injection to carry a logged-in state |
| `session_navigate` / `session_state` / `session_click` / `session_input` / `session_scroll` / `session_eval` / `session_cookies` / `session_close` | Session operations (`session_cookies` exports logged-in state for reuse) |

Full field reference: [API.md](https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md).

---

## Troubleshooting

- **Tools won't respond**: Start with `curl https://browser.aginx.net/doctor?probe=true` and check `probe.ok` and `probe.error`.
- **Screenshot unavailable**: `/doctor` reports `capabilities.screenshot` as false — the hosted instance doesn't have the screenshot feature enabled; use `fetch` or `/v1/scrape` instead.
- **Sites in other regions unreadable**: Pass `use_proxy: true` to `fetch` / `search`.
- **Blocked by Cloudflare**: Bypassed automatically by default; if still blocked, try a different `tls_fingerprint` (firefox133 / safari18, etc.).
- **Content behind a login wall**: Pass `cookies: ["name=value", ...]` to `fetch` to inject session cookies.

---

© 2026 OpenCarrier · Apache-2.0 open source · hosted at browser.aginx.net
