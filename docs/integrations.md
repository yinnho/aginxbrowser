# Using AginxBrowser from Playwright, Puppeteer, browser-use, Firecrawl, and MCP clients

AginxBrowser speaks three protocols, so existing automation tools drive it by pointing at one URL — no Chromium, no WebDriver server, no Docker. This doc shows each integration.

- **CDP** (Chrome DevTools Protocol): Playwright `connectOverCDP`, Puppeteer `connect`, browser-use
- **HTTP**: the native API plus a Firecrawl-compatible `/v1/scrape`
- **MCP**: Claude Code / Cursor / Claude Desktop

## Start the service

```bash
cargo build --release --features stealth,screenshot
./target/release/aginxbrowser
# → Listening on 0.0.0.0:8089
```

Everything below assumes `http://127.0.0.1:8089`; use `https://browser.aginx.net` for the hosted instance (same surface).

## CDP bridge

The engine advertises itself as a Chrome instance on the standard discovery endpoints:

| Endpoint | Returns |
|---|---|
| `GET /json/version` | `Browser: "Chrome/122.0.6261.69"`, `Protocol-Version: "1.3"`, `webSocketDebuggerUrl` |
| `GET /json/list` | `[]` — targets are per-connection, created over the browser socket |
| `WS /devtools/{browser\|page}/{id}` | the debugger WebSocket |

Implemented CDP domains: `browser`, `dom`, `emulation`, `fetch`, `input`, `network`, `page`, `runtime`, `storage`, `target`. Playwright and Puppeteer create targets themselves via `Target.createTarget`, so the empty `/json/list` is normal.

### Request interception (`page.route()` / `setRequestInterception`)

The `Fetch` domain is wired to the engine's intercept kernel: script-initiated `fetch()`/XHR pause at the Request stage and surface as `Fetch.requestPaused`, so Playwright routing and Puppeteer interception work:

```python
page.route("**/api/*", lambda route: route.fulfill(status=200, body="mocked"))
page.route("**/tracker.js", lambda route: route.abort())
page.route("**/slow.json", lambda route: route.continue_(url=url.replace("/slow", "")))
```

Boundaries: document/subresource loads take the navigation transport and are not intercepted (route page-initiated `fetch()` calls instead); Response-stage interception and `takeResponseBodyAsStream` are not implemented. A pause that outlives the command which created it (e.g. `evaluate` awaiting its own intercepted fetch) falls through to the real request after a bounded resolution timeout rather than hanging the connection.

### Screenshots & screencast (`Page.captureScreenshot` / `startScreencast`)

`captureScreenshot` honors the CDP params, and the `clip` coordinate world flips with `captureBeyondViewport` exactly as in Chrome:

| Shape | Path | `clip` means |
|---|---|---|
| no params, or `captureBeyondViewport: true` | full-page render | page-absolute |
| `captureBeyondViewport: false` | **viewport band** — painted from the live layout cache, cost independent of page height | viewport-relative |

The band path is the one to drive scrolling captures with: re-capturing at successive `window.scrollTo()` positions costs the same per frame whether the page is 900 px or 10,000 px tall (measured flat across a 10728 px page). `format: "jpeg"` (with `quality` 0–100, default 100) and `"png"` (default) work on both paths; `clip.scale ≠ 1` is rejected with a protocol error.

`Page.startScreencast` is a real pump: frames flow on a 33 ms cadence gated by a damage signature (DOM epoch, scroll, viewport), with ack-based backpressure (`screencastFrameAck`) — one frame in flight at a time, zero frames while the page is static. An initial snapshot frame is pushed on start, like Chrome. Page time advances even for a silent client: the pump drives the page's JS event loop itself, so a page that damages itself via `setInterval`/animations keeps producing frames without any heartbeat messages from you.

`Page.getLayoutMetrics` reports the real viewport and content size; the documentElement `scrollTop`/`scrollLeft` setters and `window.scrollTo` both land in the same scroll state the band path reads.

### Playwright (Python)

```python
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.connect_over_cdp("http://127.0.0.1:8089")
    page = browser.new_page()
    page.goto("https://example.com")
    print(page.title())
```

### Playwright (Node)

```js
const { chromium } = require('playwright');
const browser = await chromium.connectOverCDP('http://127.0.0.1:8089');
const page = await browser.newPage();
await page.goto('https://example.com');
```

### Puppeteer

```js
const puppeteer = require('puppeteer');
const browser = await puppeteer.connect({ browserURL: 'http://127.0.0.1:8089' });
const page = await browser.newPage();
await page.goto('https://example.com');
console.log(await page.title());
```

### browser-use

browser-use drives Playwright underneath; point its `BrowserProfile` at the CDP endpoint (matches browser-use's own `examples/browser/using_cdp.py`):

```python
from browser_use import Agent, Tools
from browser_use.browser import BrowserProfile, BrowserSession
from browser_use.llm import ChatOpenAI

session = BrowserSession(
    browser_profile=BrowserProfile(cdp_url="http://127.0.0.1:8089", is_local=True)
)

agent = Agent(
    task='Visit https://duckduckgo.com and search for "browser-use founders"',
    llm=ChatOpenAI(model="gpt-4.1-mini"),
    tools=Tools(),
    browser_session=session,
)

await agent.run()
await session.kill()
```

## Firecrawl-compatible `/v1/scrape`

Existing Firecrawl clients migrate by changing only the base URL.

```bash
curl -X POST http://127.0.0.1:8089/v1/scrape \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com","formats":["markdown"]}'
```

Request shape (subset of Firecrawl's):

```jsonc
{
  "url": "https://example.com",
  "formats": ["markdown"],          // "markdown" | "html" | "screenshot"
  "wait_for": 3000,                  // ms to let JS settle
  "selector": "article",             // CSS selector scope
  "tls_fingerprint": "chrome145",    // stealth-mode TLS fingerprint
  "actions": [                       // run in order on one page
    { "type": "click", "selector": "#login" },
    { "type": "writeText", "text": "hello", "selector": "#q" },
    { "type": "pressKey", "key": "Enter" },
    { "type": "wait", "milliseconds": 1000 },
    { "type": "scroll" },
    { "type": "screenshot", "fullPage": false }
  ]
}
```

Response: `{ success, data: { markdown, html, links, screenshot, metadata: { title, sourceURL, description, status_code, error } } }` — `screenshot` is a base64 data-URI PNG.

## MCP

Register the MCP server (27 tools: `fetch`, `eval`, `search`, `download`, `cache`, screenshot, plus the session tools — `session_create`, `session_clone`, `session_navigate`, `session_state`, `session_cookies`, `session_storage`, `session_console`, `session_dialog`, `session_click`, `session_click_xy`, `session_drag`, `session_input`, `session_scroll`, `session_eval`, `session_viewport`, `session_screenshot`, `session_wait`, `session_network`, `session_list`, `session_export`, `session_close`):

```bash
claude mcp add aginxbrowser --transport http http://127.0.0.1:8089/mcp
```

Hosted: `https://browser.aginx.net/mcp`. Claude Code, Cursor, and Claude Desktop connect the same way.

## Notes

- **Screenshots are opt-in**: `/screenshot` (and the `screenshot` format/action) require `cargo build --release --features screenshot`.
- **Native HTTP API** (`/fetch`, `/click`, `/eval`, `/search`, `/session/*`) — full reference in [API.md](API.md).
- **CDP targets are per-connection** — each `/devtools` WebSocket is an isolated browser with its own cookie jar; there is no persistent target registry.
