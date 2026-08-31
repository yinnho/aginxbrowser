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
| use_proxy | bool | | `false` | Route through the `AGINXBROWSER_PROXY` proxy. Set `true` for overseas sites |
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
  "truncated": false,
  "tier": "http"
}
```

The `tier` field reports which strategy served the request: `"http"` (plain HTTP + conversion) or `"browser"` (V8 render) — including under `render_tier: "auto"`, so callers can see why a fetch was fast or slow.

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

**Security**: built-in SSRF protection (blocks non-http(s) schemes and private-network/loopback IPs), DNS rebinding protection, and tracker blocking (stealth mode). An RFC 9309 robots.txt checker ships built in but is off by default (`AGINXBROWSER_HONOR_ROBOTS=1` to opt in).

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

### POST /download

Stream a file from a URL to disk. Unlike `/fetch` (which returns page content for reading), `/download` saves the raw bytes — use it for binaries, archives, datasets, documents. The body streams chunk-by-chunk to disk (never buffered in memory), with SHA-256 computed incrementally so integrity is verifiable in one call.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | File URL (`http`/`https` only) |
| filename | string | | auto | Output filename. Auto resolution: `Content-Disposition` header → URL path tail → `"download"` |
| resume | bool | | `false` | Continue an interrupted download when a local partial file exists. Server support is probed via `Range: bytes=N-`: `206` appends, `200` restarts |
| use_proxy | bool | | `false` | Route through proxy (auto-enabled for known blocked domains like github.com) |
| cookies | string[] | | `[]` | Cookies to send (`["name=value", ...]`) for gated downloads |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL after redirects |
| path | string | Absolute path of the completed file on disk |
| filename | string | Resolved filename |
| size_bytes | u64 | Bytes written by this call (append counts only appended portion) |
| content_type | string? | Response Content-Type |
| sha256 | string | SHA-256 over the complete file content |
| resumed | bool | Whether an existing partial file was continued via Range/206 |

**Behavior notes:**

- Files land in `AGINXBROWSER_DOWNLOAD_DIR` (default: current working directory). In-flight data is written to `<filename>.part`, then renamed on success.
- Same SSRF policy as `/fetch`: loopback / private / link-local targets are rejected unless `AGINXBROWSER_ALLOW_PRIVATE_NETWORK=1`.
- Redirects are followed (up to 20 hops), each hop re-validated against SSRF.
- A 30s stall timeout aborts if no bytes arrive (dead connection instead of hang). Hard cap: 4 GB per call.
- Filenames are sanitized (path traversal stripped, length capped).

**Example — download and verify:**

```bash
curl -sS -X POST http://127.0.0.1:8089/download \
  -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/obsidianmd/obsidian-releases/releases/download/v1.5.3/Obsidian-1.5.3-macOS.dmg","resume":true}'
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
| use_proxy | bool | | `false` | Route through the `AGINXBROWSER_PROXY` proxy |
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

### GET /session/list

List live sessions — the discovery twin of `/session/create` (reuse an idle session instead of spawning a fresh V8 thread per step). Entries carry idle age and the eviction budget; most recently active first.

**Response:**

```json
{"count": 1, "sessions": [{"session_id": "s_1", "idle_secs": 19, "expires_in_secs": 460}]}
```

Sessions are process-global and shared across callers (HTTP and MCP alike) — that's what makes "one instance per machine, every agent shares it" work.

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
{"url": "https://example.com/dashboard", "clicked": true, "text_after": "Dashboard …"}
```

`text_after` is the landed page's text after the click (capped at 2000 chars) — before/after evidence in one response, same contract as `/click`.

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

### GET /session/{id}/export

Export the session's recorded action log. Every page-changing command since create — `navigate` / `click` / `input` / `scroll` / `eval` — was recorded in order, in memory only (reads like `state` and `cookies` are not recorded; the log dies with the session).

**Query parameters:**

| Field | Type | Default | Description |
|------|------|------|------|
| format | string | `bash` | `bash` → a runnable curl script replaying every recorded action against a fresh session; `jsonl` → the raw action log, one JSON object per line |

**Response (`format=jsonl`):**

```
{"action":"create","url":"https://example.com/login","use_proxy":false,"cookies":[]}
{"action":"input","index":1,"text":"user@example.com","ok":true}
{"action":"click","index":3,"ok":true}
```

**Response (`format=bash`, the default)** — a self-contained replay script:

```bash
#!/usr/bin/env bash
# aginxbrowser session replay — recorded actions re-run as plain curl.
# No LLM in the loop: replay costs zero model tokens.
set -eu
BASE="${AGINXBROWSER_URL:-http://127.0.0.1:8089}"
POST() { curl -sS -X POST "$BASE/$1" -H 'Content-Type: application/json' -d "$2"; }

SID=$(POST session/create '{"url":"https://example.com/login","cookies":[],"use_proxy":false}' | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p')
[ -n "$SID" ] || { echo "session create failed" >&2; exit 1; }
POST "session/$SID/input" '{"index":1,"text":"user@example.com"}' > /dev/null
POST "session/$SID/click" '{"index":3}' > /dev/null

echo '--- final state ---'
POST "session/$SID/state" '{}'
echo
```

Run it anywhere with `curl` (override the target with `AGINXBROWSER_URL`), from cron, in CI, on another machine. What an agent figured out interactively becomes a deterministic, auditable script — re-running it costs **zero model tokens**.

> ⚠️ Treat an exported script like credentials — it embeds any cookies the session was created with.
>
> **Index caveat**: `click`/`input` replay the *index* from the original run's `/state` output. If the page's element order changed, an index may land elsewhere. The script is a readable, editable starting point, not a guaranteed selector — fix the index (or swap in a selector of your own) and re-run.

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

## robots.txt Checking (opt-in)

AginxBrowser fetches on demand — one page when an agent asks, not bulk crawling — and by default does **not** consult `robots.txt` on any path: real-time acquisition is not crawling, and robots.txt is crawler etiquette. The full RFC 9309 checker ships built in and, when the operator opts in with `AGINXBROWSER_HONOR_ROBOTS=1`, applies to every autonomous path — `/fetch`, `/click`, `/eval`, `/screenshot`, `/download`, the `/search` fetch_top body-grab (denied results keep their entry; `fetch_error` carries the reason), their MCP tool equivalents, and the Firecrawl-compatible `/v1/scrape`. A disallowed URL then returns **HTTP 403** with the matched rule in the error, so the agent can see exactly why:

```json
{"error": "robots.txt disallows /yinnho/aginxbrowser/pulse on https://github.com (matched `Disallow: /*/*/pulse`). This instance checks robots.txt (AGINXBROWSER_HONOR_ROBOTS=1); remove it to skip the check."}
```

Semantics (RFC 9309 subset):

| robots.txt outcome | Result |
|------|------|
| Rule disallows the path (longest match wins, ties → Allow; `*`/`$` wildcards honored) | 403 with the matched rule |
| 404 / 410 | allowed (no rules exist) |
| body parses to no applicable rules | allowed |
| other 4xx fetching robots.txt | allowed — the server declines to serve rules to us; RFC 9309 / Google semantics read that as "no rules apply" |
| 5xx / network failure fetching robots.txt (one retry) | 403 while the server is in trouble, short negative cache (300s) so recovery is quick — treating a failing robots.txt as allow-all is how Lightpanda #3156 got sites hammered |
| private / loopback host (`127.0.0.1`, RFC1918, `.local`, …) | exempt (operator's own network) |

Scope notes:

- **Interactive sessions are exempt by design.** `/session/{id}/navigate`, `click`, `input`, `scroll` drive a browser the way a person at a keyboard does; robots.txt governs autonomous fetching, not browser interaction.
- The robots.txt fetch itself uses the honest product User-Agent (`aginxbrowser/<version> (+https://browser.aginx.net)`) — the name robots.txt group matching keys on is never a borrowed one. If the site's TLS stack is older than the default client can speak (a CBC-only TLS 1.2 server, obscura#769), the fetch takes one final ride on the stealth transport's BoringSSL stack — a different cipher shelf, the same honest name.
- Policies are cached per host (default 1h; refusals 5min so a dead endpoint doesn't lock the host out for an hour).

**Operator opt-in** (the stance belongs to whoever runs the instance, not to each caller):

```bash
export AGINXBROWSER_HONOR_ROBOTS=1     # opt in to robots.txt checks
export AGINXBROWSER_ROBOTS_TTL_SECS=3600  # per-host policy cache TTL
```

`aginxbrowser doctor` and `GET /doctor` both report the active stance.

## Rate & Page Budgets (default on)

AginxBrowser is a real-time retrieval tool, not a crawler — budgets enforce that on **how much** any caller can fetch:

- **Per-domain rate**: 20 pages/minute per registrable domain (`AGINXBROWSER_DOMAIN_RATE_PER_MIN`). Subdomains share one budget, so rotating `www.` / `api.` / random subdomains doesn't escape. A private/loopback host is exempt (the operator's own network), and a domain's window resets when the minute rolls over.
- **Per-session page budget**: 200 pages per interactive session (`AGINXBROWSER_SESSION_PAGE_LIMIT`). Every navigation counts, plus clicks that change the page; reads on the current page (state/scroll/eval/typing) are free. An over-budget session refuses further navigations but stays interactive until closed.

Counted surfaces: `/fetch`, `/click`, `/eval`, `/screenshot`, `/download`, the `/search` fetch_top body-grab (an over-budget item keeps its entry; `fetch_error` carries the reason), `/v1/scrape` (both plain and actions paths), their MCP tool equivalents, and session navigations/clicks. Subresources a page pulls are never counted. The CDP bridge is exempt — it is a raw automation surface by design, like Chrome's remote debugging port.

An over-budget request returns **HTTP 429** with the stance in the message (MCP tools return the same text in their `error` field):

```json
{"error": "rate limit: example.com is capped at 20 pages/min — aginxbrowser does real-time lookups for agents, not site crawling. Slow down, or self-host and tune AGINXBROWSER_DOMAIN_RATE_PER_MIN."}
```

Attempts count even when the fetch itself fails — a rate limit that only counted successes would be one you escape by hammering 404s.

**Operator tuning** (hosted runs are tighter; your instance is yours):

```bash
export AGINXBROWSER_DOMAIN_RATE_PER_MIN=20    # pages per domain per minute, 0 disables
export AGINXBROWSER_SESSION_PAGE_LIMIT=200    # pages per session, 0 disables
```

---

## Local Store (durable cache, default on)

Every successful `fetch` and `search` — HTTP API and MCP tools alike — is persisted to a local SQLite database so an agent can query what it already read instead of paying for it again (a cache hit is instant; a fresh fetch costs 5-60s). Default location: `~/.aginxbrowser/cache.db` (WAL mode, `0600`).

- **Pages**: one row per fetched URL — title, extracted content, serving tier, fetch time — deduplicated by normalized URL and FTS5-indexed (Chinese substrings work: CJK text is indexed per character)
- **Searches**: whole result sets per `(query, categories)` pair
- **TTL**: pages 30 days, search results 7 days; expired rows are purged lazily on writes

Query it through the `cache` MCP tool: `query` (full-text over page contents/titles/URLs and past search queries), `get` (full cached content of one URL), `url`/`since_hours` filters, `stats`, `clear` (refuses to run without a filter or `all=true`).

| Env | Default | Meaning |
|-----|---------|---------|
| `AGINXBROWSER_STORE` | on | Set `0` to disable persistence entirely |
| `AGINXBROWSER_STORE_PATH` | `~/.aginxbrowser/cache.db` | Database file location |
| `AGINXBROWSER_STORE_TTL_HOURS` | `720` | Page rows time-to-live |
| `AGINXBROWSER_STORE_SEARCH_TTL_HOURS` | `168` | Search-result rows time-to-live |
| `AGINXBROWSER_STORE_SCOPE` | `global` | `global` = one shared pool, right for single-user instances; `session` = each MCP client session only sees its own rows — set this on public multi-client deployments |

This is the durable layer; the short-lived in-process `/fetch` cache (`AGINXBROWSER_CACHE_TTL_SECS`) is unchanged and sits in front of it.

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

### Session Semantics (`Mcp-Session-Id`)

The streamable HTTP transport follows the protocol's dual session semantics — this is the *MCP-layer* session (the JSON-RPC conversation), separate from browser sessions:

- **Header absent** on `initialize`: the server creates a new isolated MCP session and returns the ID in the `Mcp-Session-Id` response header. A client that never sends the header back gets a fresh session per connection — sessions don't leak into each other.
- **Header present**: the request continues the identified session. An unknown or expired ID returns `404` — clients re-initialize.
- **HTTP `DELETE`** with the header terminates that MCP session.

Browser sessions (`session_create` & co.) are shared across MCP sessions by design: two MCP clients on the same server can list (`session_list`) and reuse the same browser session IDs, which is what makes "one instance per machine, every agent shares it" work. For a self-hosted instance reached over a LAN IP or a Docker hostname (not `localhost`/`127.0.0.1`), add the hostname to `AGINXBROWSER_MCP_ALLOWED_HOSTS` — the transport validates the `Host` header as DNS-rebinding protection and rejects unlisted hosts with `403`.

### Provided Tools (17)

#### Core Tools

| Tool | Description |
|------|------|
| `fetch` | Fetch a web page (tiered rendering, stealth, js_extract supported) |
| `eval` | Execute JavaScript on the page (async/Promise supported) |
| `click` | Click a page element (CSS selector) |
| `search` | Multi-engine aggregated search (Baidu/Bing/Sogou/Sogou WeChat/Google) |
| `download` | Stream a file to disk with SHA-256 and resume support |
| `cache` | Query the local cache of fetched pages and past searches (full-text incl. CJK, full-content `get`, stats, filtered clear) |

#### Session Tools

| Tool | Description |
|------|------|
| `session_create` | Create an interactive browser session |
| `session_list` | List live sessions with idle age and time left before auto-eviction (discover one to reuse) |
| `session_navigate` | Navigate to a new URL within a session |
| `session_state` | Get the indexed page state |
| `session_cookies` | Export the session's current cookies (`["name=value",...]`, for login-state reuse) |
| `session_click` | Click an element by index |
| `session_input` | Type text by index |
| `session_scroll` | Scroll the page |
| `session_eval` | Execute JavaScript in the session |
| `session_export` | Export the session's recorded actions as a runnable curl replay script (`format=jsonl` for the raw log) |
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
| `AGINXBROWSER_MCP_ALLOWED_HOSTS` | unset | Extra `Host` values accepted by `/mcp` (comma-separated) — the DNS-rebinding guard defaults to loopback; add your LAN IP / Docker hostname when other machines call the instance |
| `AGINXBROWSER_DOWNLOAD_DIR` | `.` | Directory where `/download` saves files |
| `AGINXBROWSER_PROXY` | None | Proxy address (used when `use_proxy:true`, and applied automatically for browser/session/CDP navigations to known-blocked domains) |
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
