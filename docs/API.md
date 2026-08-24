# AginxBrowser API Reference

[English](API.md) | [中文](API.zh-CN.md)

> Complete HTTP API + MCP Server integration guide. Get up and running in 5 minutes.

## Quick Start

```bash
# Build and start
cargo build --release
./target/release/aginxbrowser

# Verify the service
curl http://127.0.0.1:8089/health
# → {"status":"ok","engine":"diting"}

# Fetch a page
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# Create an interactive session
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

---

## HTTP API

Listens on `0.0.0.0:8089` by default; override via the `AGINXBROWSER_BIND` environment variable.

### GET /health

Health check.

```bash
curl http://127.0.0.1:8089/health
```

Response:

```json
{"status":"ok","engine":"diting"}
```

---

### POST /fetch

Fetch a page and return its content. Supports tiered rendering, automatic Cloudflare bypass, TLS fingerprint switching, and JS data extraction.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| format | string | | `"markdown"` | Output format: `markdown` / `html` / `text` |
| selector | string | | `null` | CSS selector; only extract the matching region |
| wait_secs | u64 | | `null` | Extra seconds to wait after page load (let JS rendering finish) |
| use_proxy | bool | | `false` | Route through the `OBSCURA_PROXY` proxy. Set `true` for overseas sites |
| cookies | string[] | | `[]` | Cookies injected before navigation, format `["name=value", ...]` |
| max_chars | usize | | `50000` | Truncate `content` to this many characters. `0` = unlimited |
| auto_bypass_challenge | bool | | `true` | Automatically detect and bypass Cloudflare Turnstile challenges |
| render_tier | string | | `"auto"` | Rendering strategy (see below) |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode), see below |
| js_extract | object | | `null` | JS data extraction (see below) |

**`render_tier` options:**

| Value | Description |
|----|------|
| `auto` | Direct HTTP fetch first; automatically fall back to the browser when content is insufficient (**recommended**, default) |
| `http` | Pure HTTP, no browser. Fastest but cannot capture JS-rendered content |
| `obscura` | Force obscura browser rendering. Slowest but most reliable |

**`tls_fingerprint` options (requires `--features stealth`):**

| Value | Description |
|----|------|
| `null` | Default Chrome145 |
| `"chrome145"` | Chrome 145 |
| `"firefox133"` | Firefox 133 |
| `"firefox147"` | Firefox 147 |
| `"safari17_5"` | Safari 17.5 |
| `"safari18"` | Safari 18 |
| `"safari26"` | Safari 26 |
| `"edge145"` | Edge 145 |

**`js_extract` format:**

```json
{
  "expression": "JSON.stringify(window.__INITIAL_STATE__)",
  "timeout_ms": 5000
}
```

| Field | Type | Default | Description |
|------|------|------|------|
| expression | string | — | JS expression evaluated in the page context |
| timeout_ms | u64 | `5000` | Timeout waiting for a non-null result (milliseconds) |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL (after redirects) |
| title | string? | Page title |
| content | string | Fetched content (markdown/html/text) |
| truncated | bool | Whether `content` was truncated by `max_chars` |
| js_extract_result | any? | JS extraction result (only present when `js_extract` is set) |
| captcha_event | object? | CAPTCHA event (only present when a CAPTCHA is detected) |

**`captcha_event` format:**

| Field | Type | Description |
|------|------|------|
| engine | string | Name of the search engine that triggered the CAPTCHA (empty for `/fetch`) |
| captcha_type | string | `cloudflare_turnstile` / `recaptcha_v2` / `hcaptcha` / `slider` / `unknown` |
| url | string | URL that triggered the CAPTCHA |
| auto_solve_attempted | bool | Whether auto-solve was attempted |
| auto_solve_succeeded | bool | Whether auto-solve succeeded |

**Example — basic fetch:**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

```json
{
  "url": "https://example.com/",
  "title": "Example Domain",
  "content": "# Example Domain\n\nThis domain is for use in illustrative examples...",
  "truncated": false
}
```

**Example — extract structured data from an SPA:**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://spa-site.example.com",
    "js_extract": {
      "expression": "JSON.stringify(window.__INITIAL_STATE__)",
      "timeout_ms": 3000
    }
  }'
```

**Example — extract a specific region (CSS selector):**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/trending","format":"text","selector":"article","use_proxy":true}'
```

**Caching**: `/fetch` has an in-process cache (key includes url/format/selector/cookies/use_proxy/max_chars/render_tier/tls_fingerprint); TTL is controlled by `AGINXBROWSER_CACHE_TTL_SECS` (default 600s, `0` disables). Repeat fetches of the same URL hit the cache (~0.01s vs ~1s on first fetch).

**Security**: built-in SSRF protection (blocks non-http(s) schemes and private-network/loopback IPs), DNS rebinding protection, robots.txt compliance, and tracker blocking (stealth mode).

---

### POST /click

Load a page and click the specified element (`element.click()`), returning the page text after the click.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| selector | string | ✅ | — | CSS selector |
| wait_secs | u64 | | `null` | Extra seconds to wait after page load |
| use_proxy | bool | | `false` | Route through a proxy |
| cookies | string[] | | `[]` | Cookies injected before navigation |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL |
| selector | string | The selector used |
| clicked | bool | Whether the click succeeded |
| text_after | string? | Page text after the click |

**Example:**

```bash
curl -sS -X POST http://127.0.0.1:8089/click \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com","selector":"a"}'
```

---

### POST /eval

Execute arbitrary JavaScript on the page and return the result. Supports `async`/`Promise`.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| script | string | ✅ | — | JS expression or async IIFE |
| wait_secs | u64 | | `null` | Extra seconds to wait after page load |
| use_proxy | bool | | `false` | Route through a proxy |
| cookies | string[] | | `[]` | Cookies injected before navigation |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL |
| result | any | JS execution result |

> The `script` parameter of `/eval` supports **async functions**: returned Promises are awaited automatically. Ideal for dynamically rendered React/Vue-style pages — wait for rendering to finish, then extract data.

**Example — async script (wait for dynamic rendering):**

```bash
curl -sS -X POST http://127.0.0.1:8089/eval \
  -H "Content-Type: application/json" \
  -d '{
    "url":"https://github.com/trending",
    "script":"(async()=>{await new Promise(r=>setTimeout(r,4000));return Array.from(document.querySelectorAll(\"article.Box-row\")).slice(0,5).map(a=>a.querySelector(\"h2 a\")?.textContent?.trim())})()",
    "use_proxy":true
  }'
```

---

### POST /search

Native aggregated search with optional automatic content fetching. Agents go from "search" to "read" in one step.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| q | string | ✅ | — | Search query |
| fetch_top | usize | | `0` | Fetch full page content for the top N results. `0` = snippets only |
| categories | string | | `"general"` | Search category, comma-separated: `general` / `images` / `news`. `images` returns direct image links |
| language | string | | `"zh-CN"` | Language |
| max_results | usize | | `10` | Maximum number of results |
| max_chars_per | usize | | `4000` | Per-result content truncation in characters. `0` = unlimited |
| wait_secs | u64 | | `3` | Seconds to wait for JS rendering per page while fetching content |
| use_proxy | bool | | `false` | Whether to route content fetching through a proxy (overseas sites) |

**Built-in search engines:**

| Engine | Categories | HTTP client | Description |
|------|------|------------|------|
| Baidu | general | wreq stealth | Baidu JSON API |
| Bing | general | plain reqwest | Bing HTML parsing |
| Sogou | general | plain reqwest | Sogou web search |
| Sogou WeChat | general, news | plain reqwest | Sogou WeChat search |
| Google | general | wreq stealth + proxy | Google HTML parsing; requires a proxy from mainland China |
| Baidu Images | images | wreq stealth | Baidu Images `acjson` JSON |
| Bing Images | images | plain reqwest | Bing Images `images/async` |

Engines are queried concurrently and results merged with deduplication: identical URLs (after normalization) merge into a single entry, `engines` lists the source engines, and `score` accumulates.

**CAPTCHA progressive backoff**: when an engine triggers a CAPTCHA it pauses automatically, with the pause duration escalating on consecutive hits (5 min → 10 min → 30 min → 1 h) and resetting after a successful search. Set the `CAPTCHA_SOLVER_API_KEY` environment variable to enable automatic CAPTCHA solving.

**Response fields:**

| Field | Type | Description |
|------|------|------|
| query | string | Search query |
| number_of_results | usize | Total number of results |
| results | array | Result list |
| captcha_events | array | List of CAPTCHA events |

**Each entry in `results`:**

| Field | Type | Description |
|------|------|------|
| title | string | Title |
| url | string | Link |
| snippet | string | Search snippet |
| engines | string[] | Source engines |
| score | float | Combined score |
| content | string? | Page content (only present within the `fetch_top` range) |
| content_truncated | bool | Whether the content was truncated |
| fetch_error | string? | Reason content fetching failed |
| image_url | string? | Direct link to the image binary (downloadable straight to jpg/png with `curl -o`). Only with `categories=images` |
| source_url | string? | URL of the page hosting the image (provenance/copyright) |
| width | u32? | Image width (px) |
| height | u32? | Image height (px) |

> With `categories=images`, the `url` field equals `image_url` (the direct image link, convenient for immediate download); `snippet` is empty. Baidu Images prefers `objURL` (original image, highest quality) and falls back to a CDN-proxied direct link when unavailable.

**Example — search + fetch top 3 pages:**

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","fetch_top":3,"max_chars_per":2000}'
```

**Example — image search (returns direct links, downloadable straight from curl):**

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"蔚来ES8 酒红内饰 后排视角","categories":"images","max_results":10}'
```

```json
{
  "query": "蔚来ES8 酒红内饰 后排视角",
  "number_of_results": 20,
  "results": [
    {
      "title": "蔚来ES8 酒红内饰后排实拍",
      "url": "https://n.sinaimg.cn/.../img.jpg",
      "engines": ["baidu_images"],
      "score": 20.0,
      "image_url": "https://n.sinaimg.cn/.../img.jpg",
      "source_url": "https://auto.sina.com.cn/...",
      "width": 1920,
      "height": 1080
    }
  ]
}

# Download the image
curl -sL -o cabin_ref.jpg "<image_url>"
```

---

### POST /screenshot

Render the page's post-JS DOM into a PNG screenshot (returned as base64). **Requires building with `--features screenshot`** (not included by default; see the build section).

Does not use `/fetch`'s tiered rendering — it always drives the obscura browser through full JS execution, then feeds the result to the built-in Blitz rendering stack (Stylo + Taffy + vello_cpu, pure CPU, no Chromium).

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| width | u32 | | `1280` | Viewport width (CSS px) |
| height | u32 | | `800` | Viewport height (CSS px; serves as a lower bound when `full_page` is set) |
| scale | f32 | | `1.0` | Device pixel ratio; higher is sharper but yields larger PNGs |
| full_page | bool | | `true` | Capture the entire scrollable page (tracks content height, capped at 16000px) |
| wait_secs | u64 | | `null` | Extra seconds to wait after load (for JS rendering) |
| selector | string | | `null` | CSS selector; capture the **specified element region** instead of the full page (see below) |
| selector_all | bool | | `false` | Used with `selector`: skip cropping and return coordinates of **all matches** |
| use_proxy | bool | | `false` | Route through the `OBSCURA_PROXY` proxy |
| cookies | string[] | | `[]` | Cookies injected before navigation |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL (after redirects) |
| title | string? | Page title |
| width | u32 | Actual PNG pixel width rendered (differs from the requested value when `full_page` tracks content height or `selector` crops) |
| height | u32 | Actual PNG pixel height rendered |
| image_base64 | string | base64-encoded PNG. Decode with `base64 -d`, or use directly as `data:image/png;base64,...` |
| format | string | Always `"png"` |
| selector_rects | object[]? | Present only when the request includes `selector`. One `{x, y, width, height}` per element, in **CSS px with the page's top-left corner as origin** (not viewport coordinates) |

**Selector mode (element-level screenshot + coordinates):**

- `selector` + `selector_all=false` (default): the image is cropped to the border box of the first matching element; `selector_rects` contains exactly one entry (the cropped region).
- `selector` + `selector_all=true`: the image renders as a normal full page; `selector_rects` returns coordinates for **every match** — the agent can consume just the coordinates without the image.
- Coordinates come from Blitz's post-layout `final_layout` (Taffy border boxes), accumulated along the layout tree into absolute page coordinates.

> ⚠️ **Inline element limitation**: inline elements containing only text (e.g. `<a>文字</a>`) have no standalone Taffy box — crop mode errors out advising you to pick a block-level ancestor, and `selector_all` mode returns `0x0`. Inline elements containing block-level or replaced content (`<a><img>` etc.) fall back to the union of their descendants' boxes. Selectors targeting **block-level containers** (div/section/li, etc.) yield reliable coordinates.

**Example — screenshot a Baidu search:**

```bash
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","full_page":true,"wait_secs":2}' \
  | jq -r .image_base64 | base64 -d > baidu.png
```

**Example — crop the first search result + get coordinates of all results:**

```bash
# Crop just the first .result under #content_left
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","selector":"#content_left .result"}' \
  | jq -r .image_base64 | base64 -d > first-result.png

# Skip the image — just want page coordinates for all 9 results
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","selector":"#content_left .result","selector_all":true,"full_page":false,"width":100,"height":100}' \
  | jq -c '.selector_rects'
# [{"x":150,"y":2843,"width":608,"height":153}, {"x":150,"y":3016,"width":608,"height":69}, ...]
```

```json
{
  "url": "https://www.baidu.com/s?wd=蔚来ES8",
  "title": "蔚来ES8_百度搜索",
  "width": 1280,
  "height": 800,
  "image_base64": "iVBORw0KGgo...",
  "format": "png"
}
```

> Screenshots are the agent's "visual input" — but inline Blitz is beta; CSS rendering on complex sites is approximate (not Chromium pixel-perfect). Sub-resources such as images are not fetched separately (`<img>` may be missing from screenshots); text and layout are reliable.

---

### POST /v1/scrape (Firecrawl-compatible)

[Firecrawl](https://github.com/mendableai/firecrawl)-compatible endpoint. Existing Firecrawl clients can migrate by simply changing the base URL.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| formats | string[] | | `["markdown"]` | Output formats: `["markdown"]` / `["html"]` / `["markdown","html"]` |
| onlyMainContent | bool | | `false` | Main content only (parameter accepted, not yet implemented) |
| waitFor | u64 | | `null` | Milliseconds to wait for JS rendering |
| timeout | u32 | | `null` | Timeout in milliseconds (parameter accepted) |
| actions | object[] | | `[]` | Actions to perform before scraping (see below) |
| selector | string | | `null` | CSS selector |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |

**`actions` format:**

```json
[
  {"type": "click", "selector": "button.accept"},
  {"type": "wait", "milliseconds": 1000}
]
```

| type | Fields | Description |
|------|------|------|
| `click` | `selector` | Click an element (anchor links navigate to the target page) |
| `wait` | `milliseconds` | Wait the given number of milliseconds |
| `screenshot` | — | Screenshot the rendered page, returned as a base64 data-URI (requires the `screenshot` feature) |
| `scroll` | — | Scroll the page |
| `writeText` | `text`, `selector` | Type text into the matching element |
| `pressKey` | `key` | Press a key (Enter submits the enclosing GET form) |

When any `actions` are present, `/v1/scrape` follows a **single-page session flow**: navigate once → execute actions in order → extract from the final state of that page. All actions operate on the same page. When the request includes a `screenshot` action (or `formats` contains `"screenshot"`), the response's `data.screenshot` carries a `data:image/png;base64,...` screenshot; the field is omitted when the `screenshot` feature is not enabled.

**Response (Firecrawl format; HTTP 200 for both success and failure):**

```json
{
  "success": true,
  "data": {
    "markdown": "...",
    "html": "...",
    "metadata": {
      "title": "Example Domain",
      "sourceURL": "https://example.com/",
      "description": "...",
      "statusCode": 200
    }
  }
}
```

---

## Session API (Interactive Browser Sessions)

Persistent browser sessions with indexed interaction. Each session gets its own V8 runtime + page context, and is reclaimed automatically after 8 minutes of inactivity.

Lets AI agents browse the web the way a human does: open a page → inspect state → click/type → collect results.

### POST /session/create

Create an interactive browser session.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | | `null` | Initial URL (optional) |
| use_proxy | bool | | `false` | Route through a proxy |
| cookies | string[] | | `[]` | Cookies injected before navigation (`["name=value",...]`) so the session starts already logged in |

**Response:**

```json
{"session_id": "s_1", "url": "https://example.com/"}
```

### POST /session/{id}/navigate

Navigate to a new URL.

**Request fields:**

| Field | Type | Required | Description |
|------|------|------|------|
| url | string | ✅ | Target URL |

**Response:**

```json
{"url": "https://example.com/page2", "title": "Page 2"}
```

### POST /session/{id}/state

Get the current page state as an indexed list of interactive elements.

**Response format (compact text):**

```
url=https://example.com/login
title=Login
viewport=1280x800

[0] <a href="/home" rect=[24,16,52x19]>Home</a>
[1] <input type=email placeholder=Email rect=[24,60,232x22] />
[2] <input type=password placeholder=Password rect=[24,100,232x22] />
[3] <button id=submit rect=[24,140,88x28]>Sign In</button>
[4] <a href="/forgot" rect=[120,144,110x19]>Forgot password?</a>
```

Index numbers `[N]` feed `click` / `input` operations. `rect=[x,y,w,h]` is the element's position relative to the current viewport (y changes with scrolling) — use it to judge whether an element is in view and needs a `scroll` before `click`.

### POST /session/{id}/click

Click an interactive element by index.

**Request fields:**

| Field | Type | Required | Description |
|------|------|------|------|
| index | usize | ✅ | Element index (from `/state`) |

**Response:**

```json
{"url": "https://example.com/dashboard", "clicked": true}
```

### POST /session/{id}/input

Type text into an input field by index.

**Request fields:**

| Field | Type | Required | Description |
|------|------|------|------|
| index | usize | ✅ | Element index |
| text | string | ✅ | Text to enter |

**Response:**

```json
{"filled": true}
```

### POST /session/{id}/scroll

Scroll the page.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| direction | string | | `"down"` | `up` or `down` |
| amount | u32 | | `3` | Number of viewport heights to scroll |

**Response:**

```json
{"scrolled": true}
```

### POST /session/{id}/eval

Execute JavaScript within the session.

**Request fields:**

| Field | Type | Required | Description |
|------|------|------|------|
| script | string | ✅ | JS code (async supported) |

**Response:**

```json
{"result": "..."}
```

### POST /session/{id}/close

Close the session and release its resources.

**Response:**

```json
{"ok": true}
```

### GET /session/{id}/cookies

Export the session's current-page cookies (as a `["name=value",...]` array). Use it to persist login state — store it, then pass `cookies` to a future `session_create` to start the session already logged in, no re-login needed.

**Response:**

```json
{"url": "https://example.com/dashboard", "cookies": ["sessionid=abc123", "csrftoken=xyz"]}
```

**Login-state reuse loop:**

```bash
# 1. Log in normally in one session (session_create -> input -> click)
# 2. Export the cookies
curl -sS http://127.0.0.1:8089/session/$SID/cookies | jq -r .cookies[]

# 3. Next time, create the session with cookies directly — no login required
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/dashboard","cookies":["sessionid=abc123","csrftoken=xyz"]}'
```

> 🔒 The hosted instance never persists any cookie to disk — cookies live only in session memory and are wiped when the session is reclaimed after 8 minutes idle. Callers hold their own login state (use a throwaway account, not your main one).

### Session Usage Example

```bash
# 1. Create a session
SID=$(curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/login"}' | jq -r .session_id)

# 2. Inspect the page state
curl -sS -X POST http://127.0.0.1:8089/session/$SID/state

# 3. Enter the username
curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -H "Content-Type: application/json" \
  -d '{"index":1,"text":"user@example.com"}'

# 4. Enter the password
curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -H "Content-Type: application/json" \
  -d '{"index":2,"text":"mypassword"}'

# 5. Click Sign In
curl -sS -X POST http://127.0.0.1:8089/session/$SID/click \
  -H "Content-Type: application/json" \
  -d '{"index":3}'

# 6. Inspect the post-login state
curl -sS -X POST http://127.0.0.1:8089/session/$SID/state

# 7. Close the session
curl -sS -X POST http://127.0.0.1:8089/session/$SID/close
```

---

## Automatic CAPTCHA Solving

When a search engine or target site throws up a CAPTCHA, AginxBrowser will:

1. **Detect** the CAPTCHA type (Cloudflare Turnstile, reCAPTCHA v2, hCaptcha, slider)
2. **Report** it via the `captcha_event` field so the caller knows
3. **Auto-solve** it (if the `CAPTCHA_SOLVER_API_KEY` environment variable is set)

**Configuration:**

```bash
# Set your 2captcha API key
export CAPTCHA_SOLVER_API_KEY=your_api_key_here

# Optional: switch the CAPTCHA solving service (default 2captcha)
export CAPTCHA_SOLVER_SERVICE=2captcha
```

Once configured, `/fetch` and `/search` automatically submit CAPTCHAs to 2captcha and inject the token — no manual intervention needed.

---

## MCP Server

AginxBrowser wraps its core operations as an MCP (Model Context Protocol) server that AI agents can call directly — no hand-written HTTP client required. Two access modes are supported:

- **stdio**: `--mcp` mode, local/self-hosted, communicating over stdin/stdout
- **streamable HTTP**: the HTTP server ships a built-in `/mcp` endpoint, directly reachable from the public internet (works out of the box on the hosted instance)

### Getting Started

**Option 1: Hosted instance (zero deployment, recommended)**

This project runs a publicly hosted instance; Claude Code connects with one line:

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

The HTTP server ships a `/mcp` endpoint speaking the MCP Streamable HTTP protocol (SSE), supporting both `GET` (SSE event stream) and `POST` (request/response). Any MCP client with HTTP transport support (Claude Code / Claude Desktop / Cursor) can connect.

**Option 2: Self-hosted stdio**

```bash
./target/release/aginxbrowser --mcp
```

`--mcp` mode speaks the stdio protocol, does not start an HTTP server, and communicates with MCP clients over stdin/stdout.

### Provided Tools (13)

#### Core Tools

| Tool | Description |
|------|------|
| `fetch` | Fetch a web page (tiered rendering, stealth, js_extract supported) |
| `eval` | Execute JavaScript on the page (async/Promise supported) |
| `click` | Click a page element (CSS selector) |
| `search` | Multi-engine aggregated search (Baidu/Bing/Sogou/Sogou WeChat/Google) |

#### Session Tools

| Tool | Description |
|------|------|
| `session_create` | Create an interactive browser session |
| `session_navigate` | Navigate to a new URL within a session |
| `session_state` | Get the indexed page state |
| `session_cookies` | Export the session's current cookies (`["name=value",...]`, for login-state reuse) |
| `session_click` | Click an element by index |
| `session_input` | Type text by index |
| `session_scroll` | Scroll the page |
| `session_eval` | Execute JavaScript in the session |
| `session_close` | Close the session |

#### `fetch` Tool Parameters

| Parameter | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| format | string | | `"markdown"` | Output format: `markdown` / `html` / `text` |
| selector | string | | `null` | CSS selector |
| wait_secs | u64 | | `null` | Seconds to wait after page load |
| use_proxy | bool | | `false` | Route through a proxy |
| max_chars | usize | | `50000` | Character truncation limit |
| auto_bypass_challenge | bool | | `true` | Automatically bypass Cloudflare Turnstile |
| render_tier | string | | `"auto"` | Rendering strategy: `auto` / `http` / `obscura` |
| tls_fingerprint | string | | `null` | TLS fingerprint |
| js_extract | object | | `null` | JS data extraction: `{expression, timeout_ms}` |

#### `session_create` Parameters

| Parameter | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | | `null` | Initial URL |
| use_proxy | bool | | `false` | Route through a proxy |
| cookies | string[] | | `[]` | Inject cookies (`["name=value",...]`) so the session starts already logged in. Pair with `session_cookies` to reuse login state |

#### Session Operation Parameters

All session operations require the `session_id` parameter. `click`/`input` also need `index` (from `session_state`); `input` additionally needs `text`; `eval` needs `script`.

### Client Configuration

#### Claude Code

**Hosted instance (one command)**:

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

Or configure the HTTP transport in a settings file:

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

**Self-hosted (stdio)**: edit the project-level or global settings file:

**Project-level** `.claude/settings.json`:

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "/path/to/aginxbrowser",
      "args": ["--mcp"]
    }
  }
}
```

**Global** `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "/path/to/aginxbrowser",
      "args": ["--mcp"]
    }
  }
}
```

#### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "/path/to/aginxbrowser",
      "args": ["--mcp"]
    }
  }
}
```

#### Cursor

Edit `.cursor/mcp.json` in the project root:

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "/path/to/aginxbrowser",
      "args": ["--mcp"]
    }
  }
}
```

#### Remote Server (via SSH)

If AginxBrowser is deployed on a remote server, connect through an SSH tunnel:

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "ssh",
      "args": ["your-server", "/data/www/aginxbrowser/target/release/aginxbrowser", "--mcp"]
    }
  }
}
```

> **Note**: SSH access requires passwordless login to the remote server (set up a public key with `ssh-copy-id`) and a pre-built AginxBrowser binary on that server.

---

## Environment Variables

| Variable | Default | Description |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | HTTP server listen address |
| `AGINXBROWSER_STEALTH` | Enabled | `0` disables stealth (for diagnostics) |
| `AGINXBROWSER_UA` | Linux Chrome145 | Spoofed User-Agent |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language header |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` cache TTL (seconds); `0` disables |
| `OBSCURA_PROXY` | None | Proxy address (used when `use_proxy:true`) |
| `CAPTCHA_SOLVER_API_KEY` | None | 2captcha API key; enables automatic CAPTCHA solving when set |
| `CAPTCHA_SOLVER_SERVICE` | `2captcha` | CAPTCHA solving service |

---

## Error Codes

| HTTP Status | Scenario |
|------------|------|
| 400 | Invalid CSS selector syntax, URL parse failure |
| 404 | Element not found |
| 502 | Target site unreachable (DNS/connection failure) |
| 504 | Request timed out |
| 500 | Other internal errors |

---

## Site Scraping Examples

### WeChat Official Account Articles (public, no login required)

Directly fetchable in stealth mode — **no cookies needed**:

```bash
# Extract title and body with /eval
curl -sS -X POST http://127.0.0.1:8089/eval -H 'Content-Type: application/json' -d '{
  "url": "https://mp.weixin.qq.com/s/xxxxx",
  "script": "({title:document.querySelector(\"#activity-name\")?.textContent?.trim(), body:document.querySelector(\"#js_content\")?.innerText})"
}'

# Search WeChat articles with /search and auto-fetch content
curl -sS -X POST http://127.0.0.1:8089/search -H 'Content-Type: application/json' \
  -d '{"q":"AI人工智能","categories":"news","fetch_top":3,"max_chars_per":2000}'
```

### Interactive Login (Session API)

```bash
# Create session → inspect page → input → click → inspect result
SID=$(curl -sS -X POST http://127.0.0.1:8089/session/create \
  -d '{"url":"https://example.com/login"}' | jq -r .session_id)

curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -d '{"index":1,"text":"user@example.com"}'

curl -sS -X POST http://127.0.0.1:8089/session/$SID/click \
  -d '{"index":3}'

curl -sS -X POST http://127.0.0.1:8089/session/$SID/state
```

### Cloudflare-Protected Sites

`auto_bypass_challenge` is on by default: "Just a moment..." pages are detected automatically and the tool waits for the `cf_clearance` cookie:

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://cloudflare-protected-site.com"
}'
```

### Extract Structured Data from SPAs

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://spa-site.example.com",
  "js_extract": {
    "expression": "JSON.stringify(window.__INITIAL_STATE__)",
    "timeout_ms": 3000
  }
}'
```

### TLS Fingerprint Switching

Some sites check TLS fingerprints; if Chrome gets blocked, try Firefox/Safari instead:

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://strict-site.com",
  "tls_fingerprint": "firefox133",
  "use_proxy": true
}'
```
