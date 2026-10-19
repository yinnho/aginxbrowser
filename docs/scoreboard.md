# Performance / compatibility scoreboard (evidence)

[English](scoreboard.md) | [中文](scoreboard.zh-CN.md)

> Snapshot: **2026-10-19 Asia/Shanghai** · AginxBrowser **0.6.0** · `/health` commit **`0c302d9`**  
> Binary: local `target/debug` (`cargo build --features stealth` + `AGINXBROWSER_BUILD_COMMIT`) — **not** Homebrew cellar, **not** `--release`  
> Raw receipts: [`bench/scoreboard-20261019/`](../bench/scoreboard-20261019/) · [`INDEX.json`](../bench/scoreboard-20261019/INDEX.json)

This page is a **tip micro-bench** with committed receipts. It complements the historical 20-page harness in [`bench/README.md`](../bench/README.md) (2026-08-28 / 2026-09-29) and the wire-level [Stealth/TLS comparison](stealth-tls-comparison.md). Absolute milliseconds on a debug build are **not** marketing release numbers.

---

## Methodology

### What we measure

| Metric | How | Why |
|--------|-----|-----|
| **Cold start → first `/health`** | Spawn server, poll until 200 | Process bring-up cost for agents |
| **Warm `/health`** | 3 samples after ready | Control-plane overhead |
| **`POST /fetch` markdown** | `render_tier:"auto"`, `format:"markdown"`, cache TTL 0 | Agent-usable text wall time |
| **Peak RSS** | `ps` sample during shortset | Memory footprint of one server |
| **Session snapshot** | `create` → `navigate` → `GET /snapshot` | Interactive agent refs latency |
| **curl / Chrome headless** | Same machine baselines | Dumb HTTP vs real Chromium |

### Engines

| Engine | Status | Notes |
|--------|--------|-------|
| **aginxbrowser 0.6.0** | **run** | tip `0c302d9`, stealth build, port `18091` |
| **curl** | **run** | `/usr/bin/curl` / Homebrew curl on macOS |
| **Chrome 155 headless** | **run** (example.com only) | `--headless=new --dump-dom`, cold profile |
| **Lightpanda** | **not run** | Binary not installed — [releases](https://github.com/lightpanda-io/browser/releases) |
| **Obscura** | **not run** | Standalone not installed; framed via [agent-browser](https://github.com/vercel-labs/agent-browser) CDP engines |

### Limits (read this)

- **Debug profile.** Numbers are from `cargo build --features stealth` (unoptimized). A `--release` binary will be faster; we did not invent release timings.
- **Network included.** Wall times include Asia/Shanghai egress RTT. Compare columns on this machine, not against someone else's laptop.
- **No invented peer timings.** Lightpanda / Obscura cells stay **not run**.
- **No merchant / authenticated production APIs** in this bench.
- **Shortset ≠ full harness.** Five URLs from [`bench/pages.txt`](../bench/pages.txt); full 20×2×4 history stays in `bench/README.md`.

### How to reproduce

```bash
AGINXBROWSER_BUILD_COMMIT=$(git rev-parse --short HEAD) cargo build --features stealth
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18091 \
  AGINXBROWSER_RESTORE_SESSIONS=0 ./target/debug/aginxbrowser &
curl -sS http://127.0.0.1:18091/health
# fetch
curl -sS -X POST http://127.0.0.1:18091/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","format":"markdown","max_chars":0,"render_tier":"auto"}'
# snapshot
SID=$(curl -sS -X POST http://127.0.0.1:18091/session/create \
  -H 'Content-Type: application/json' -d '{"viewport":{"width":1280,"height":720}}' | jq -r .session_id)
curl -sS -X POST http://127.0.0.1:18091/session/$SID/navigate \
  -H 'Content-Type: application/json' -d '{"url":"https://example.com"}'
curl -sS http://127.0.0.1:18091/session/$SID/snapshot | jq .
```

---

## Matrix A — Process & control plane

| Metric | aginxbrowser | Receipt |
|--------|--------------|---------|
| Cold start → first `/health` | **2025 ms** | [`cold-start.summary.json`](../bench/scoreboard-20261019/cold-start.summary.json) |
| Warm `/health` median (n=3) | **0.47 ms** | [`health-warm.summary.json`](../bench/scoreboard-20261019/health-warm.summary.json) |
| `/health` commit / stealth / TLS | `0c302d9` / stealth **true** / `chrome145` | same |
| Peak RSS during shortset fetch | **58.3 MB** | [`aginx-fetch-shortset.summary.json`](../bench/scoreboard-20261019/aginx-fetch-shortset.summary.json) |

---

## Matrix B — Fetch to markdown (shortset)

Same five public URLs, sequential, cache off. aginx uses `POST /fetch` → markdown; curl downloads raw HTML bytes (not markdown).

| URL | aginx ms | aginx tier | chars | curl ms | curl ok |
|-----|----------|------------|-------|---------|---------|
| `https://example.com` | **587** | http | 156 | 726 | yes |
| `https://www.rust-lang.org/` | 2179 | http | 4916 | 4094 | yes |
| `https://www.python.org/` | 2725 | http | 7762 | 247 | yes |
| `https://nodejs.org/en` | 8273 | http | 4929 | 30017 | **no** (timeout) |
| `https://doc.rust-lang.org/book/title-page.html` | 801 | http | 1573 | 779 | yes |

| Aggregate | Value |
|-----------|-------|
| aginx median (5/5 ok) | **2179 ms** |
| curl median (4/5 ok only) | 753 ms |
| example.com: aginx vs Chrome `--dump-dom` | **587 ms** markdown vs **1617 ms** DOM ([`chrome-example.com.summary.json`](../bench/scoreboard-20261019/chrome-example.com.summary.json)) |

**Reading the numbers**

1. On `example.com`, tip aginx `/fetch` markdown beat both curl HTML and cold Chrome `--dump-dom` on this machine.
2. curl can be faster on some static origins (python.org here) because it stops at bytes — no HTML→markdown conversion.
3. curl **timed out** on nodejs.org in this run; aginx still returned markdown (`tier=http`). Reliability is part of the scoreboard, not just p50.
4. Do **not** compare aginx median (5 ok) to curl median (4 ok) as a pure speed claim — the success sets differ.

---

## Matrix C — Session snapshot (example.com)

Separated navigate vs snapshot (first probe that used `create(url)` folded navigation into the first snapshot; see caveat in `INDEX.json`).

| Step | ms | Notes |
|------|----|-------|
| `POST /session/create` (no url) | 8.6 | empty session |
| `POST …/navigate` → example.com | 2635 | browser session path (not `/fetch` http tier) |
| `GET …/snapshot` (json) | **6.2** | 1 element: link `Learn more` `[ref=e0]` |
| `GET …/snapshot?format=text` | 2.1 | a11y-ish text |
| Warm `GET …/snapshot` | 1.6 | second json call |

Receipt: [`aginx-snapshot-example.com.summary.json`](../bench/scoreboard-20261019/aginx-snapshot-example.com.summary.json).

Navigate ≫ snapshot: once the document is loaded, building actionable refs is milliseconds. `/fetch` http-tier markdown on the same URL stays the fast path for read-only agents.

---

## Matrix D — Peers (not run)

| Engine | Status | Pointer |
|--------|--------|---------|
| Lightpanda | **not run** | [GitHub releases](https://github.com/lightpanda-io/browser/releases) · [lightpanda.io](https://lightpanda.io) |
| Obscura | **not run** | Used via [agent-browser](https://github.com/vercel-labs/agent-browser) CDP (`--engine obscura`); no standalone binary on this Mac |

We publish **our** timings + methodology so peers can be filled in later on the same machine without rewriting the page.

Related: [agent-browser engine guide](agent-browser-engine.md) · [Stealth/TLS comparison](stealth-tls-comparison.md) · historical [bench/README.md](../bench/README.md).

---

## Compatibility framing (not a synthetic “% score”)

| Capability | Aginx tip | Evidence |
|------------|-----------|----------|
| Local HTTP API (no MCP) | yes | [`docs/API.md`](API.md) |
| Interactive snapshot + `ref=eN` | yes | Matrix C + API `/session/{id}/snapshot` |
| Stealth TLS presets | yes | [stealth-tls-comparison](stealth-tls-comparison.md) |
| CDP for agent-browser upstream | design only | [`cdp-serve-design.md`](cdp-serve-design.md) |
| Drop-in Chromium replacement | no | intentional — HTTP-first |

---

## Changelog

| Date | Commit | Note |
|------|--------|------|
| 2026-10-19 | `0c302d9` binary | First public tip scoreboard receipts |
