# v0.2.0

First tagged release. 102 commits since the 0.1.0 state: a CDP bridge that lets existing automation stacks drive the engine directly, the diting render pipeline as the screenshot default, a multi-engine aggregated search, and a long list of DOM/CDP behavior fixes — most of them confirmed against upstream obscura issues and contributed back with data.

## Highlights

### CDP WebSocket bridge — Playwright / Puppeteer / browser-use connect directly
The engine speaks Chrome DevTools Protocol over `/devtools/{browser|page}/{id}`. Point your existing stack at it — no Chromium, no WebDriver, no Docker:

```python
# Playwright (Python)
browser = p.chromium.connect_over_cdp("http://127.0.0.1:8089")
```

Implemented domains: browser, dom, emulation, input, network, page, runtime, storage, target. Full walkthrough in [docs/integrations.md](docs/integrations.md) (Playwright Py/Node, Puppeteer, browser-use, Firecrawl drop-in).

### diting is now the default screenshot engine
`/screenshot` renders through the diting pipeline (Stylo/Taffy lineage CSS layout → PNG). The legacy path stays available as an opt-in. Notable render fixes this cycle: `srcset`/`<picture>` source selection, absolute-position static-position harvesting before reparenting (blitz#764 class), synthetic ICB for fixed/abs viewport anchoring, WebP/JPEG decode via magic-byte dispatch, non-GB2312 symbol glyph coverage (·—✓→●), cyclic-percentage images resolving to natural size, and Baidu SERP hang fix.

### Multi-engine search with aggregation and dedup
`POST /search` fans out across engines and merges results: Baidu, Bing, Sogou (web/news/WeChat), DuckDuckGo, Stack Exchange, GitHub repositories, arXiv, npm, PyPI, HuggingFace (models/datasets/spaces), plus image search (`categories=images`) returning direct binary URLs. Proxy semantics are direct-first with automatic fallback (`AGINXBROWSER_PROXY`), so overseas deployments run zero-config.

### MCP server (13 tools)
`fetch`, `search`, `eval`, `click` + 9 session tools for multi-step flows (login, pagination, form filling), with cookie export/import to carry logged-in state. Registered in the official MCP Registry; hosted instance at `https://browser.aginx.net/mcp`.

## Fixed (confirmed against upstream obscura issues)

We treat upstream open issues as free audits: reproduce against our tree, fix first, then report back with numbers.

- **Label activation** (#721): clicking `<label>` now toggles its labeled control per spec — pre-click activation for checkbox/radio (state flips before the event, reverts on cancel, radio groups exclusive), `for`-association rules (empty string associates nothing, dangling no-op, disabled controls inert), deep descendants via `closest('label')`, and a click-in-progress guard checked *before* pre-activation so self-clicking handlers can't recurse (#726) or double-fire.
- **Runtime.consoleAPICalled + exceptionThrown** (#677): console output and uncaught exceptions now reach CDP clients, with registered executionContextId (unregistered ids get silently dropped by Playwright).
- **Page.Frame completeness** (#703): all frame payloads carry loaderId, secureContextType, crossOriginIsolatedContextType, gatedAPIFeatures — centralized through one serializer so emit sites can't drift.
- **connectOverCDP handshake** (#715): attachedToTarget flushed before createTarget response (Playwright resolves pages synchronously), flat sessionId echo, complete getTargetInfo.
- **sessionStorage persistence** (#678): snapshot/re-seed across navigation and target switches instead of dropping on realm teardown.
- **Binary fetch bodies** (#716): base64 transport end-to-end, byte-exact regression-tested; btoa no longer mangles bytes via TextEncoder.
- **postMessage targetOrigin** (#704), **file:// scheme gate** (#708), **base href resolution** (#658 class).
- **Firecrawl #4375 class**: malformed DDG `uddg=` escapes no longer silently return empty results.

## Changed

- Release binaries are stripped (~92 MB with stealth+screenshot vs 115 MB before).
- Env vars renamed `OBSCURA_*` → `AGINXBROWSER_*`.
- Engine identity reported as "diting" in /health and /doctor.

## Try it

```bash
# Hosted (no install)
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp

# Self-hosted
git clone https://github.com/yinnho/aginxbrowser.git && cd aginxbrowser
cargo build --release --features stealth,screenshot   # ~4 min
./target/release/aginxbrowser                          # :8089
```

Prebuilt binaries for this release: macOS (arm64, x86_64) and Linux (x86_64 musl) — see Assets below. `cargo build` remains the source of truth; binaries ship as-is without the captcha-solver key check.

## Notes

- Self-hosting gives you your own network identity: the shared hosted egress IP is treated accordingly by aggressive anti-bot sites (Google et al). Local runs browse from your own IP/proxy.
- Apache-2.0. The engine is developed in the open alongside upstream obscura/blitz — fixes flow back as issue comments with reproduction data.
