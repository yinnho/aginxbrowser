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
# → {"status":"ok","engine":"diting","version":"0.3.1","commit":"a1b2c3d",...}

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

Health check. Also the build-identity call: `version` + `commit` answer "which source is this binary" (compare against the release tag to verify doc/tag/binary/source are the same commit), and `ua`/`tls` say what the instance presents to sites — the UA browser traffic carries (`AGINXBROWSER_UA` override, else the pinned persona; imported sessions keep the copied request's own UA by design) and the default TLS fingerprint (`"off"` in non-stealth builds). `commit` is `"unknown"` for git-less builds.

```bash
curl http://127.0.0.1:8089/health
```

Response:

```json
{
  "status": "ok",
  "engine": "diting",
  "version": "0.3.1",
  "commit": "a1b2c3d",
  "ua": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
  "tls": "chrome145",
  "capabilities": { "screenshot": true, "stealth": true, "captcha_solver": false }
}
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
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation: `"name=value"` strings (may carry `; Domain=…; Path=/; Secure` attributes) or CDP-style objects `{"name","value","domain","path","secure","httpOnly","sameSite"}`. Entries declaring `Domain=` anchor at that domain, so sibling-domain login state (`.taobao.com` / `.tmall.com` style) survives injection instead of being dropped by RFC 6265 domain checks |
| max_chars | usize | | `50000` | Truncate `content` to this many characters. `0` = unlimited |
| auto_bypass_challenge | bool | | `true` | Automatically detect and bypass Cloudflare Turnstile challenges |
| render_tier | string | | `"auto"` | Rendering strategy (see below) |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode), see below |
| js_extract | object | | `null` | JS data extraction (see below) |
| sanitize | bool | | `true` | Strip prompt-injection carriers from the text/markdown output (see below) |
| capture_xhr | string[] | | `null` | Return the page's script-initiated XHR/fetch response bodies as a first-class `xhr` field. Entries are URL substrings; `[]` = every XHR/fetch. Forces the browser tier |

**`render_tier` options:**

| Value | Description |
|----|------|
| `auto` | Direct HTTP fetch first; automatically fall back to the browser when content is insufficient (**recommended**, default) |
| `http` | Pure HTTP, no browser. Fastest but cannot capture JS-rendered content |
| `obscura` / `browser` | Force the full JS browser. Slowest but most reliable |

> **JS-heavy SPA sites** (cls.cn, juejin-class feeds, WeChat articles) render their content client-side: with `auto` the HTTP pass returns a small skeleton and the sufficiency gate usually catches it, but a site that returns a *plausible-looking* stub defeats the heuristic. When you know the target is an SPA, pass `"render_tier":"browser"` explicitly — you skip a wasted HTTP round-trip and get the rendered page directly. The `tier` field in the response tells you which path served a given fetch.
>
> **WeChat article text** (`mp.weixin.qq.com`, no explicit `selector`): the article body is server-rendered inside `#js_content` but hidden until WeChat's own JS reveals it, so plain `body` text is just the title/byline shell. `format:"text"`/`"markdown"` reads the `#js_content` container and returns the fuller of the two extractions — the full article comes back even when the reveal script never finishes.

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
| tier | string? | Which path served the page: `"http"` (plain HTTP + conversion, ~100ms) or `"browser"` (V8 render) — present under `render_tier: "auto"` too, so callers can see why a fetch was fast or slow |
| redirected_from | string[]? | The redirect trail: `redirected_from[0]` is the URL you asked for, `url` is where the content actually came from (absent when no redirect happened) |
| js_extract_result | any? | JS extraction result (only present when `js_extract` is set) |
| sanitize_report | object? | What the injection stripper removed (only present when `sanitize` fired — see below) |
| xhr | object[] | Script-initiated response bodies (only present when `capture_xhr` is set — see below) |
| captcha_event | object? | CAPTCHA event (only present when a CAPTCHA is detected; covers Cloudflare/Google/Baidu challenge pages plus Taobao/Tmall risk-control signals — `punish` redirects, `x5sec`, and MTop `FAIL_SYS_USER_VALIDATE`/`RGV587` replies even when they arrive as HTTP 200) |

**`sanitize` — injection stripping (default on):**

Page text is untrusted input, and a reading tool owes its caller content that doesn't carry instructions aimed at the reader. On `text`/`markdown` output (raw `html` is never touched) three carriers are stripped:

- **Zero-width/steganographic characters** (`​` family) — never legitimate page prose.
- **Hidden-span text** — the live DOM is probed for elements a human can't see (`opacity:0`, sub-4px font) whose text `innerText` happily carries. Whole-hidden containers (SSR-pending reveals, WeChat `#js_content`) are guarded: when hidden text is more than half the extraction, nothing is removed.
- **Instruction-shaped lines** — lines matching curated injection phrasings (EN/CN: "ignore previous instructions" class, chat markup like `<|im_start|>`) are dropped whole, because the payload continues past the matched phrase.

Removal is observable, never silent: when anything fires, `sanitize_report` says what — `{"zero_width_removed": 1, "hidden_spans_removed": 1, "patterns_hit": {"ignore_previous_instructions": 1}}`. It's a heuristic, not a firewall; to study the payload itself, pass `"sanitize": false`. The `selector` parameter is the CSS-narrowing half of the story: restrict the extraction to the content region and the chrome's injected noise never enters the text at all.

**`capture_xhr` — the page's own API face:**

Usually the cleanest read of a JS-heavy page isn't the rendered DOM but the JSON APIs the page itself calls. `capture_xhr` returns those response bodies alongside the text:

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://spa.example.com/list","capture_xhr":["/api/"],"wait_secs":2}'
```

Each row is `{"url", "method", "status", "mime", "body", "body_truncated"}`. Bodies are capped at `min(max_chars, 8000)` chars per entry (at most 20 entries) so a page's API traffic can't flood the context; binary (base64) bodies are skipped. `wait_secs` matters here: the page's fetches need a moment to land in the network log.


**`captcha_event` format:**

| Field | Type | Description |
|------|------|------|
| engine | string | Name of the search engine that triggered the CAPTCHA (empty for `/fetch`) |
| captcha_type | string | `cloudflare_turnstile` / `recaptcha_v2` / `hcaptcha` / `slider` / `unknown` |
| url | string | URL that triggered the CAPTCHA |
| detected_at | u64 | Wall-clock time of detection (unix seconds) |
| hit_count | u32 | Consecutive CAPTCHA hits for this engine — the backoff-ladder step driving the suspension duration (`1` → 5 min, `2` → 10 min, `3` → 30 min, `4+` → 1 h). Always `1` on the `/fetch` path |
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
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (`"name=value"` strings or CDP-style objects, same semantics as `/fetch`) |
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
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (`"name=value"` strings or CDP-style objects, same semantics as `/fetch`) |
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
| engines | string[] | | `[]` | Restrict to these engine names (e.g. `["baidu"]`). Empty = all engines serving `categories`. Unknown names → 400 with the valid list |
| time_range | string | | — | Freshness window: `day` / `week` / `month` / `year`. Honored by engines with dated results (bing_news filters by pubDate); others ignore it. Invalid values → 400 |

> Unknown request fields are rejected with a 400 — a typo'd parameter name (`count`, `limit`, `num`) fails loudly instead of being silently ignored.

**Built-in search engines** (`GET /engines` returns this list with live health):

| Engine | Categories | Description |
|------|------|------|
| baidu | general | Baidu HTML SERP (browser UA; the old `tn=json` API is wall-dead) |
| bing | general | Bing HTML parsing |
| sogou | general | Sogou web search |
| sogou_wechat | general, news | Sogou WeChat article search |
| duckduckgo | general | DuckDuckGo HTML (stealth fingerprint) |
| bing_news | general, news | Bing News RSS; honors `time_range` by pubDate |
| baidu_images | images | Baidu Images `acjson` JSON |
| bing_images | images | Bing Images `images/async` |
| arxiv | general, academic | arXiv API |
| huggingface | general, ai | Hugging Face models search |
| github | general, code | GitHub repository search |
| stackexchange | general, code | StackExchange API |
| npm | general, packages | npm registry |
| pypi | general, packages | PyPI |

`meilisearch` additionally registers when `AGINXBROWSER_MEILI_URL` + `AGINXBROWSER_MEILI_INDEX` are set (private index). Google search is not included — it requires a proxy and steady CAPTCHA clearance from mainland China; use `duckduckgo`/`bing` instead.

Engines are queried concurrently and results merged with deduplication: identical URLs (after normalization) merge into a single entry, `engines` lists the source engines, and `score` accumulates. `GET /engines` returns every engine's name, categories, and live suspension state — call it to discover valid names for the `engines` filter.

**CAPTCHA progressive backoff**: when an engine triggers a CAPTCHA it pauses automatically, with the pause duration escalating on consecutive hits (5 min → 10 min → 30 min → 1 h) and resetting after a successful search. Set the `CAPTCHA_SOLVER_API_KEY` environment variable to enable automatic CAPTCHA solving.

**Response fields:**

| Field | Type | Description |
|------|------|------|
| query | string | Search query |
| number_of_results | usize | Number of results returned (equals `results.length`) |
| results | array | Result list |
| captcha_events | array | List of CAPTCHA events (see `captcha_event` format above — each carries `detected_at` + `hit_count`) |
| engine_errors | object | Per-engine reason an engine contributed nothing: CAPTCHA suspension (with resume countdown), transient fetch/parse failure, or a task panic. Absent when every eligible engine answered |

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

**Example — restrict to WeChat articles, today's news only:**

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"A股 午评","engines":["sogou_wechat","bing_news"],"categories":"news","time_range":"day","fetch_top":2}'
```

If `engines` names an engine the server doesn't know, the response is a 400 carrying the valid list:

```json
{"error":"unknown engine \"wechat\"; valid engines: baidu, baidu_images, bing, bing_images, sogou, sogou_wechat, ... (also check GET /engines)"}
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

### GET /engines

Discover the search-engine vocabulary plus live health. Returns every registered engine with the categories it serves and its current suspension state — call this before using /search's `engines` filter, or to see who is currently benched by a CAPTCHA.

```bash
curl -sS http://127.0.0.1:8089/engines
```

```json
{
  "engines": [
    { "name": "baidu", "categories": ["general"], "suspended": false, "captcha_count": 0 },
    { "name": "sogou_wechat", "categories": ["general", "news"], "suspended": true,
      "suspend_remaining_secs": 184, "captcha_count": 1 }
  ]
}
```

`suspended: true` means the engine hit a CAPTCHA and is in progressive backoff — `/search` skips it (the response's `engine_errors` says so, with the resume countdown) until the suspension expires.

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
| cookies | string[] \| object[] | | `[]` | Cookies to send (`["name=value", ...]` or CDP-style objects) for gated downloads |

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

Does not use `/fetch`'s tiered rendering — it always drives the obscura browser through full JS execution, then renders the result with the built-in diting engine (our own CSS cascade + Taffy box layout + CPU paint, no Chromium). Pass `"engine": "blitz"` to opt into the Blitz reference pipeline for comparison renders — that requires building with `--features blitz-reference` (blitz is not compiled in by default).

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
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (`"name=value"` strings or CDP-style objects, same semantics as `/fetch`) |
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
- Coordinates come from diting's post-layout Taffy border boxes, accumulated along the layout tree into absolute page coordinates.

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

> Screenshots are the agent's "visual input" — CSS rendering on complex sites is approximate (not Chromium pixel-perfect). Sub-resources such as images are not fetched separately (`<img>` may be missing from screenshots); text and layout are reliable.

---

### POST /video

Render a page's animation timelines to an MP4 video (returned as base64). **Requires building with `--features screenshot` and ffmpeg on the server's PATH.**

The page's scripts must expose their timelines in `window.__timelines` — objects with `duration()` and `pause(t)` (a paused GSAP timeline registered there works as-is):

```js
const tl = gsap.timeline({ paused: true });
tl.from("#box", { opacity: 0, x: -200, duration: 2, ease: "power2.out" });
window.__timelines = { main: tl };
```

Each frame seeks every registered timeline to `t = i/fps` and paints the viewport — the frame values carry no wall clock, so output is deterministic across runs. The full path runs in-process: seek → viewport band paint → raw RGBA piped into ffmpeg → H.264/yuv420p MP4.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL (must populate `window.__timelines`) |
| fps | f64 | | `24` | Frames per second |
| width | u32 | | `1280` | Viewport width (CSS px; floored to even — yuv420p) |
| height | u32 | | `720` | Viewport height (CSS px) |
| hold_tail_secs | f64 | | `0.5` | Freeze the final timeline state for this many extra seconds |
| max_duration_secs | f64 | | `120` | Safety cap on timeline + hold tail (longer timelines error instead of encoding) |
| wait_timelines_ms | u64 | | `10000` | How long to wait for `window.__timelines` to appear |
| use_proxy | bool | | `false` | Route through the `AGINXBROWSER_PROXY` proxy |
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (same semantics as `/fetch`) |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |
| narration | object[] | | `[]` | Voiceover clips: `{url, start_secs, volume}` — each fetched, delayed to its start time, all mixed into one AAC track. Any TTS output works (mp3/wav/ogg/m4a, probed by content). Fetch failure is an error, never a silent video |
| audio | object | | `null` | Background music: `{url, volume, fade_out_secs, loop_audio}` — looped to cover the video, volume-scaled, faded out at the tail |
| subtitles_srt | string | | `null` | Inline SRT text muxed as a soft (toggleable) mov_text track — no libass needed for muxing. Cap 64 KiB |
| subtitles_language | string | | `null` | ISO language tag for the subtitle track ("eng", "zh") |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL |
| title | string? | Page title |
| frames | u32 | Frames written to the encoder |
| timeline_secs | f64 | Longest registered timeline, seconds |
| duration_secs | f64 | Total video length = timeline + hold tail |
| width / height | u32 | Encoded pixel size |
| video_base64 | string | base64-encoded MP4 (H.264, yuv420p). Decode with `base64 -d`, or use directly as `data:video/mp4;base64,...` |
| has_audio | bool | Whether an audio track (BGM and/or narration) was muxed in |
| has_subtitles | bool | Whether a soft subtitle track was muxed in |
| format | string | Always `"mp4"` |

Audio details: audio URLs are fetched through the page's own HTTP client (http/https only, ≤16 MiB, 8 s timeout, same SSRF gate as subresources). One audio source rides a plain `-af volume/adelay/afade` chain; two or more (e.g. BGM + narration clips) are mixed with `amix=normalize=0` so narration isn't halved for having quiet music under it. With ≥2 audio inputs the subtitle stream is mapped explicitly. Burned-in captions need no engine support — register a second `__timelines` entry that writes caption text/opacity in `pause(t)`, and it seeks along with everything else.

**Example:**

```bash
curl -sS -X POST http://127.0.0.1:8089/video \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/anim.html","fps":20,"width":800,"height":450}' \
  | jq -r .video_base64 | base64 -d > anim.mp4
```

Errors carry the failure reason verbatim: no `__timelines` before `wait_timelines_ms`, zero timeline duration, the duration cap, ffmpeg missing from PATH, or ffmpeg exiting non-zero (with its stderr tail).

---

### POST /pdf

Cut a rendered page into a set of pages and package as PDF (default), per-page PNGs, or an image-based PPTX/DOCX. **Requires building with `--features screenshot`.**

Two slicing modes, picked by whether `selector` is set:

- **Print** (no `selector`): fixed-height pages over the whole document (default 794×1123, A4 @96dpi), breaking at top-level block boundaries where possible — the break lands on the deepest block bottom that fits, with a half-page floor so pages never collapse to slivers. A remainder shorter than 64px merges into the previous page instead of a near-blank tail.
- **Slides** (`selector` set): one page per CSS-selector match, sized to that element. Build the deck as plain HTML with one `.slide` div per slide; each match becomes a page at its own height.

Each page is painted as a viewport band off the live tree's layout — same primitive the timeline pump rides, no Chromium. The PDF is image-based: per-page JPEG (`jpeg_quality`) embedded via DCTDecode, one page object per page with its own MediaBox in points (px→pt at 96dpi), so variable-height pages need no normalization. PPTX and DOCX are the same pages re-packaged: PPTX as one slide per page (deck-sized to the largest page, images anchored top-left), DOCX as one page-sized section per page with zero margins — Word sizes every section independently, so each page keeps its exact height. Both containers are written by hand (stored-ZIP, fixed timestamps — byte-deterministic), zero new dependencies.

`format="pptx-native"` is the editable tier: instead of rasterizing pages, each `selector` match is walked element-by-element off the live tree (gBCR + computed style from the same layout cache the band paints ride) and mapped to native DrawingML — text becomes real text runs (`<a:t>` with font family/size/weight/color/alignment), background boxes become shapes (solid fill, `border-radius` → roundRect, CSS gradients → `gradFill` with the angle converted), `<img>` becomes a `p:pic` with the fetched bytes as a media part. Slides mode only: `selector` is required, one slide per match, and the deck is sized to the largest slide. What a text box can't express (per-glyph inline styling, z-index reordering, transforms, borders) degrades by omission — the element still lands as an editable shape. Note: the engine doesn't expand the CSS `background` shorthand into `background-image` yet, so gradient slides must use the `background-image` longhand.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | ✅ | — | Target URL |
| format | string | | `"pdf"` | `"pdf"` (base64 PDF), `"png"` (one base64 PNG per page), `"pptx"` (one slide per page), `"pptx-native"` (editable: element-level DrawingML; requires `selector`), or `"docx"` (one page-sized section per page) |
| width | u32 | | `794` | Page width (CSS px) |
| height | u32 | | `1123` | Page height (CSS px) — print pagination only; slides size each page to its element |
| selector | string | | `null` | CSS selector; set → slides mode, unset → print mode |
| max_pages | usize | | `50` | Safety cap on emitted pages (more pages errors instead of rendering) |
| jpeg_quality | u8 | | `90` | JPEG quality 1-100 for page embedding in PDF/PPTX/DOCX (PNG format ignores it) |
| use_proxy | bool | | `false` | Route through the `AGINXBROWSER_PROXY` proxy |
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (same semantics as `/fetch`) |
| tls_fingerprint | string | | `null` | TLS fingerprint (stealth mode) |

**Response fields:**

| Field | Type | Description |
|------|------|------|
| url | string | Final URL |
| title | string? | Page title |
| pages | usize | Page count |
| width / height | u32 | Requested page size (slides pages vary in height — each PNG self-describes) |
| pdf_base64 | string? | base64 PDF (set when `format="pdf"`). Decode with `base64 -d`, or use directly as `data:application/pdf;base64,...` |
| pages_base64 | string[] | One base64 PNG per page (non-empty when `format="png"`) |
| pptx_base64 | string? | base64 PPTX, one slide per page (set when `format="pptx"` or `"pptx-native"`) |
| docx_base64 | string? | base64 DOCX, one page-sized section per page (set when `format="docx"`) |
| format | string | `"pdf"` / `"png"` / `"pptx"` / `"pptx-native"` / `"docx"` |

**Examples:**

```bash
# Print mode: paginate an article into A4 pages
curl -sS -X POST http://127.0.0.1:8089/pdf \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/long-article.html"}' \
  | jq -r .pdf_base64 | base64 -d > article.pdf

# Slides mode: one .slide per page, as PNGs
curl -sS -X POST http://127.0.0.1:8089/pdf \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/deck.html","selector":".slide","format":"png"}' \
  | jq -r '.pages_base64[0]' | base64 -d > slide-0.png

# Slides mode → PowerPoint deck (HTML .slide divs become real slides)
curl -sS -X POST http://127.0.0.1:8089/pdf \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/deck.html","selector":".slide","format":"pptx"}' \
  | jq -r .pptx_base64 | base64 -d > deck.pptx

# Slides mode → editable PowerPoint (text runs, shapes, gradients, images)
curl -sS -X POST http://127.0.0.1:8089/pdf \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/deck.html","selector":".slide","format":"pptx-native"}' \
  | jq -r .pptx_base64 | base64 -d > deck.pptx
```

Errors carry the reason verbatim: a selector that matches nothing, more pages than `max_pages`, or a document with no content height.

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
| cookies | string[] \| object[] | | `[]` | Cookies injected before navigation (`"name=value",...` or CDP-style objects) so the session starts already logged in |
| persistent | bool | | `false` | Persist login state to the server-side store: if the session idles out or the server restarts, the same `session_id` revives logged-in on the next call (`session/{id}/close` drops the snapshot; idle expiry keeps it) |

**Response:**

```json
{"session_id": "s_1", "url": "https://example.com/"}
```

### POST /import/curl

Turn a DevTools **"Copy as cURL"** command into a logged-in browser session — the credential-transfer path that works on a stock headless engine (no extension, no debug port). The human does the hard part of a login (CAPTCHA, SMS, slider) in their own Chrome, opens DevTools → Network, right-clicks any authenticated request → *Copy* → *Copy as cURL*, and pastes the command here. The engine parses out the Cookie header / `-b` jar, injects it into a fresh session anchored at the copied request's URL — the agent continues from where the human left off, no password or second login. bash, PowerShell and cmd copy flavors all parse.

**Request fields:**

| Field | Type | Required | Default | Description |
|------|------|------|------|------|
| curl | string | ✅ | — | The copied cURL command |
| use_proxy | bool | | `false` | Route the session's traffic through the `AGINXBROWSER_PROXY` proxy |

**Response:**

```json
{
  "session_id": "s_1",
  "url": "https://example.com/member/home",
  "host": "example.com",
  "cookie_count": 12,
  "method": "GET",
  "has_body": false,
  "user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/145.0.0.0",
  "authorization_prefix": "Bearer eyJhbGciOi…",
  "expires_in_secs": 480,
  "note": "login state injected; navigate with session tools"
}
```

The `session_id` is an ordinary session — drive it with the session API or the MCP session tools. `authorization_prefix` previews only the first 16 chars (the full header is a credential; the response never echoes whole cookies). `method`/`has_body` report what kind of request was copied. Cookies anchor on the copied request's host; other subdomains of the site may re-authenticate — that is the site's device binding, not a lost credential.

**Tips**: an XHR on the logged-in page usually carries the most complete cookie set (document requests sometimes miss the `httpOnly` API-session pair). `-b FILE` cookie jars are rejected — the server cannot read your disk. Treat a pasted command like a password: it carries the login state verbatim.

### POST /session/{id}/clone

Derive a new session carrying the source's full login state — cookies, `localStorage`/`sessionStorage`, viewport pin, dialog policy, proxy and keepalive flags — with the source left untouched. Use it to snapshot a logged-in state before risky actions, or to run the same login in parallel sessions. The manual `cookies` → `session/create` round-trip this replaces is where a hand-edited cookie string clobbers a working login.

**Response:**

```json
{"session_id": "s_2", "cloned_from": "s_1", "url": "https://example.com/dashboard", "viewport": {"width": 390, "height": 844, "mobile": true}, "expires_in_secs": 431}
```

`viewport` is `null` when the source never pinned one.

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

### POST /session/{id}/click_xy

Click at page coordinates via the real mouse chain: `pointerdown`/`mousedown` → `pointerup`/`mouseup` → `click`, each hit-tested through `elementFromPoint` at the given viewport position — the shape canvas/map pages and custom widgets listen for. `click_count: 2` also synthesizes `dblclick`.

| Field | Type | Default | Description |
|------|------|------|------|
| x | number | ✅ | Viewport X in CSS pixels |
| y | number | ✅ | Viewport Y in CSS pixels |
| button | string | `"left"` | `left` / `right` / `middle` |
| click_count | u32 | `1` | Chrome click count: 2 adds `dblclick`, 3+ sets `detail` |

**Response:** `{"url": "...", "x": 120, "y": 120}`

### POST /session/{id}/drag

Press at `from`, glide through `steps` interpolated `mousemove` events (`delay_ms` apart), release at `to` — drags a marker/canvas selection the way a real pointer would, so move-driven widgets (map markers, drag handles, sliders) track every intermediate position.

| Field | Type | Default | Description |
|------|------|------|------|
| from | object | ✅ | `{"x":…,"y":…}` press point |
| to | object | ✅ | `{"x":…,"y":…}` release point |
| steps | u32 | `10` | Interpolated move events (clamped 1..200) |
| delay_ms | u64 | `30` | Pause between moves (clamped 0..1000) |

**Response:** `{"url": "...", "from": {"x":120,"y":120}, "to": {"x":300,"y":220}, "steps": 10}`

### POST /session/{id}/input

Type text into an input field by index. After writing the value, `input` + `change` events are dispatched (bubbling), so framework-bound forms (Vue/React models, validation listeners) see the text.

**Request fields:**

| Field | Type | Required | Description |
|------|------|------|------|
| index | usize | ✅ | Element index |
| text | string | ✅ | Text to enter |
| events | string | `"standard"` | `standard` → `input` + `change` after the value lands; `full` → per-character `keydown`/`keypress`/`input`/`keyup` cycles for strict keyboard listeners |

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

Close the session and release its resources. For a `persistent` session this also drops the on-disk login snapshot — idle expiry keeps it, an explicit close does not.

**Response:**

```json
{"ok": true}
```

### GET /session/{id}/cookies

Export the session's cookies as full Set-Cookie strings (`["name=value; Domain=example.com; Path=/", ...]`, flags included). Use it to persist login state — store it, then pass `cookies` to a future `session_create` to start the session already logged in, no re-login needed. The full form (not bare `name=value` pairs) is what makes cross-subdomain logins survive the round-trip: a `.taobao.com` cookie re-anchors at its own domain on the way back in, where a bare pair would be scoped to whatever page you happen to open.

**Response:**

```json
{"url": "https://example.com/dashboard", "cookies": ["sessionid=abc123; Domain=example.com; Path=/", "csrftoken=xyz; Domain=example.com; Path=/; HttpOnly"]}
```

**Login-state reuse loop:**

```bash
# 1. Log in normally in one session (session_create -> input -> click)
# 2. Export the cookies
curl -sS http://127.0.0.1:8089/session/$SID/cookies | jq -r .cookies[]

# 3. Next time, create the session with cookies directly — no login required
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/dashboard","cookies":["sessionid=abc123; Domain=example.com; Path=/"]}'
```

> 🔒 The hosted instance never persists any cookie to disk — cookies live only in session memory and are wiped when the session is reclaimed after 8 minutes idle. Callers hold their own login state (use a throwaway account, not your main one).

### GET /session/{id}/export

Export the session's recorded action log. Every page-changing command since create — `navigate` / `click` / `input` / `scroll` / `eval` — was recorded in order, in memory only (reads like `state` and `cookies` are not recorded; the log dies with the session).

**Query parameters:**

| Field | Type | Default | Description |
|------|------|------|------|
| format | string | `bash` | `bash` → a runnable curl script replaying every recorded action against a fresh session; `jsonl` → the raw action log, one JSON object per line; `json` → a flow.json document — the same recording as editable ops (`{op, args}`) with cookies/storage stripped, for replay via [`POST /flow/run`](#post-flowrun) |

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

**Response (`format=json`)** — the recording as a flow document:

```json
{
  "create": { "url": "https://example.com/login" },
  "steps": [
    { "op": "input", "args": { "index": 1, "text": "user@example.com" } },
    { "op": "click", "args": { "index": 3 } }
  ]
}
```

This is the authoring entry point for flows: drive a session by hand, export as json, then curate (prune probe evals, promote URLs to `{{vars}}`, add `wait` steps and `expect` assertions, mark extraction evals with `save`) and replay via `POST /flow/run`. Cookies and storage are stripped — a flow is a shareable asset. `ok:false` probe actions are dropped. `wait` steps are not part of the recording (the recorder logs actions, not your waiting), so waits are hand-added during curation.

> ⚠️ Treat an exported script like credentials — it embeds any cookies the session was created with.
>
> **Index caveat**: `click`/`input` replay the *index* from the original run's `/state` output. If the page's element order changed, an index may land elsewhere. The script is a readable, editable starting point, not a guaranteed selector — fix the index (or swap in a selector of your own) and re-run.

### POST /flow/run

Run a flow — a recorded/edited JSON session script — to completion server-side, zero model tokens in the loop. The engine stays LLM-free: a flow is pure data (`{{var}}` substitution, `wait` predicates, `expect` checks — no LLM calls anywhere).

**Request body:**

| Field | Type | Default | Description |
|------|------|------|------|
| flow | object | — | Inline flow document (see below) |
| name | string | — | Or run a server-side workflow asset: `workflow/<name>/flow.json` (override the directory with `AGINXBROWSER_WORKFLOW_DIR`). An unknown name errors back with the list of installed workflows — that error is the discovery call |
| vars | object | `{}` | Values for `{{placeholders}}`; wins over the flow's own `vars` defaults |
| session_id | string | — | Reuse a live session (e.g. from `POST /import/curl`) instead of creating a fresh one — that's how login state and flows compose |

One of `flow` / `name` is required.

**Flow document:**

```json
{
  "vars": { "handle": "aginxbrowser" },
  "create": { "url": "https://x.com/{{handle}}", "use_proxy": true },
  "steps": [
    { "op": "wait", "args": { "predicate": "document.querySelectorAll('article').length >= 1", "timeout_ms": 20000 } },
    { "op": "eval", "args": { "script": "JSON.stringify({...})" },
      "expect": { "selector": "article" }, "save": "profile" }
  ]
}
```

- `vars` — default values for `{{placeholders}}` in `create` and step args; request `vars` override them. Substituting an undeclared var fails fast with a receipt (no session created).
- `create` — passed to `POST /session/create` semantics (url / cookies / use_proxy / viewport / …); skipped entirely when `session_id` is given.
- `steps[]` — `op` is one of the session verbs: `navigate`, `set_content`, `click`, `click_xy`, `drag`, `input`, `scroll`, `viewport`, `wait`, `eval`. `expect` (optional) gates the step: `url_contains`, `selector`, `text_contains`, `eval_truthy` — all must hold. `save` (optional) stores the step's result under that key in the receipt.
- On failure the flow aborts at the failing step and the receipt carries `status:"failed"`, `failed_step`, `reason`, the page `url`, a viewport `screenshot` (base64), everything `saved` so far — and the session **stays alive** for manual takeover (`session_id` in the receipt).

**Response (ok):**

```json
{
  "status": "ok",
  "session_id": "s_7",
  "steps_done": 2,
  "saved": { "profile": "{\"url\":\"https://x.com/aginxbrowser\",...}" }
}
```

**Response (failed):**

```json
{
  "status": "failed",
  "session_id": "s_8",
  "failed_step": 0,
  "reason": "wait timeout: predicate never became truthy within 15000ms",
  "url": "https://www.zhihu.com/question/603518666",
  "screenshot": "<base64 png>",
  "steps_done": 0,
  "saved": {}
}
```

The repo ships three sample flows under `workflow/` (`xcom-profile`, `juejin-post`, `zhihu-answer`) — one runs green logged-out, one green, one fails on a 403 wall on purpose, each with a `flow.md` explaining itself. A workflow `name` is one path segment of lowercase/digits/dashes; anything else is rejected before it touches the filesystem. Drop a directory in to deploy — no rebuild.

### GET /session/{id}/network

The session's network request log for the current page — every document, subresource and script-initiated `fetch()`/XHR the page actually issued, one compact row each. This is the sniffer surface.

**Query parameters:**

| Field | Type | Default | Description |
|------|------|------|------|
| filter | string | — | `media` → only playback/stream requests (HLS `.m3u8`, DASH `.mpd`, `.mp4`, `.flv`, `.ts`, `.webm`, audio), classified by URL suffix or response Content-Type |
| include_bodies | bool | `false` | Add an `xhr` array: the page's script-initiated (XHR/fetch) responses with their retained text bodies — the page's own API face. Binary (base64) bodies are skipped |
| url_contains | string | — | With `include_bodies`: only body rows whose URL contains this substring |
| body_max_chars | usize | `4000` | Per-body character cap for `include_bodies` (`0` = unlimited) |

**Response (default):**

```json
{
  "url": "https://example.com/watch",
  "total": 14,
  "requests": [
    {"method":"GET","url":"https://example.com/watch","status":200,"type":"Document","size":51234},
    {"method":"GET","url":"https://cdn.example/api/resolve","status":200,"type":"Fetch","size":210},
    {"method":"GET","url":"https://h5api.example.com/rest","status":0,"type":"Fetch","size":0,
     "error":"CORS error: Origin 'https://example.com' not in Access-Control-Allow-Origin ''"}
  ]
}
```

`status: 0` rows are requests that never produced a servable response — SSRF-blocked, URL-blocklisted, CORS-refused, or dead at the transport layer. The `error` field says which, so a page whose API calls all die at a gate no longer reads as "never issued a request".

**Response (`?filter=media`)** — the playback-link sniffer. Links embedded in page HTML are often decoys; these are the requests the player actually made, so they are what plays:

```json
{
  "url": "https://example.com/watch",
  "media": [
    {"url":"https://cdn.example/v/master.m3u8?token=1","kind":"hls","status":200,"mime":"application/vnd.apple.mpegurl","type":"Fetch"}
  ]
}
```

**Response (`?include_bodies=true`)** — a sibling `xhr` array joins the default response, one row per script-initiated response with its retained body: `{"url":"https://api.example/items","method":"GET","status":200,"mime":"application/json","body":"{\"items\":[…]}","body_truncated":false}`. The same face `/fetch`'s `capture_xhr` returns statelessly, read live off the session.

### GET /session/{id}/har

The same traffic as a full **HAR 1.2** document (`application/json`) — opens in Chrome DevTools or any HAR viewer. Retained response bodies are inlined as `content.text` (large/opaque bodies base64, `content.encoding: "base64"`); bodies beyond the retention limits are absent. Timing phases we do not measure are `-1`.

```bash
curl -sS http://127.0.0.1:8089/session/$SID/har -o page.har
```

### GET /session/{id}/storage

Snapshot the session's `localStorage` + `sessionStorage` for the current origin — the half of login state that cookies can't carry (many sites keep the session token in web storage). Call it before the session idles out, then feed the snapshot back via `session/create`'s `storage` field to restore the logged-in state in a new session.

**Response:**

```json
{"url": "https://example.com", "local_storage": {"token": "eyJ..."}, "session_storage": {}}
```

### GET /session/{id}/console

The session's recent page console output (`log` / `info` / `warn` / `error`, plus `dialog` entries for auto-answered `alert`/`confirm`/`prompt`) as a ring buffer of the last 500 entries, newest last — captures output from page scripts, clicks, evals and navigations alike. The fastest way to see *why* a page misbehaves: click the button, call this, read the error.

**Query parameters** (all optional, combinable):

| Field | Type | Default | Description |
|------|------|------|------|
| level | string | — | Only entries at this exact level (e.g. `error`, `dialog`) |
| since_ts | u64 | — | Only entries at or after this Unix epoch millisecond |
| url_contains | string | — | Only entries whose page URL contains this substring |
| limit | usize | — | Keep only the most recent N matches |

**Response:** `{"url": "...", "total": 3, "matched": 1, "messages": [{"ts_ms": 1712, "level": "error", "text": "TypeError: x is not a function", "url": "https://example.com/"}]}` — `total` is the whole ring, `matched` what the filters let through.

### POST /session/{id}/dialog

Inspect or steer how the session answers `window.alert` / `confirm` / `prompt`. Dialogs never block the page — they are answered immediately under the session policy (default: dismiss) and logged to the console ring with `level: "dialog"`.

| Field | Type | Required | Description |
|------|------|------|------|
| action | string | ✅ | `list` → current policy + every dialog entry seen; `accept` → subsequent dialogs answer OK/true; `dismiss` → answer cancel/false (the default) |
| prompt_text | string | | What `window.prompt` returns when accepted; persists for the session |

**Response:** `{"policy": "accept", "prompt_text": null, "dialogs": [{"ts_ms": 1712, "level": "dialog", "text": "{\"dialog\":\"alert\",\"message\":\"修改成功\"}"}]}`

### POST /session/{id}/viewport

Set the session's viewport (device emulation). Scripts see `innerWidth`/`innerHeight` move, media queries like `(max-width: 600px)` re-evaluate, and `mobile: true` flips `pointer: coarse` / `hover: none` in `matchMedia` and reports `navigator.maxTouchPoints = 5`. Omitted fields keep their current value; the override survives navigation.

```bash
curl -sS -X POST http://127.0.0.1:8089/session/$SID/viewport \
  -H "Content-Type: application/json" -d '{"width": 390, "height": 844, "mobile": true}'
```

**Response:** `{"viewport": {"width": 390, "height": 844, "mobile": true}}`

### POST /session/{id}/screenshot

Screenshot the session's **current DOM state** (mutations from clicks/evals included) as a base64 PNG. Mirrors `POST /screenshot`'s response shape (`image_base64`), so existing consumers work against either. The body is optional — omit it for a viewport-sized capture.

| Field | Type | Default | Description |
|------|------|------|------|
| width | u32 | current | Render width in CSS pixels |
| height | u32 | current | Render height in CSS pixels |
| full_page | bool | `false` | Capture the full scrollable page instead of the viewport |
| selector | string | — | Capture only that element's box |
| selector_all | bool | `false` | With `selector`: capture every match instead of the first |

**Response:** `{"url": "...", "width": 1280, "height": 800, "image_base64": "iVBOR...", "format": "png"}`

### POST /session/{id}/wait

Wait until a CSS selector matches or a JS predicate turns truthy, with a timeout. The page's event loop keeps running while waiting (fetches, timers, promise chains progress), so this replaces blind sleeps for async content: navigate, wait for `.price-card`, then click/read. Exactly one of `selector` / `predicate`.

| Field | Type | Default | Description |
|------|------|------|------|
| selector | string | — | CSS selector to wait for (e.g. `.price-card`) |
| predicate | string | — | JS expression polled until truthy (e.g. `document.querySelectorAll('.card').length >= 3`) |
| timeout_ms | u64 | `10000` | Give up after this long (max 120000) |

**Response:** `{"matched": true, "elapsed_ms": 743, "detail": {"tag": "div", "text": "..."}}` — on expiry, an error naming the selector/predicate.

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

Query it through the `cache` MCP tool: `query` (full-text over page contents/titles/URLs and past search queries, ranked by BM25 × recency fusion), `get` (full cached content of one URL), `url`/`since_hours` filters, `stats`, `clear` (refuses to run without a filter or `all=true`). Text hits come back with `[§ heading]` section prefixes so you know *where on the page* they landed. Every page row stores a `content_hash` plus the previous sample's hash — `cache get` reports `changed_since_prev`, the cheapest drift detector for origins serving frozen bodies (a rate-limited 200 that never changes reads as `false` across consecutive samples).

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

### Provided Tools (32)

#### Core Tools

| Tool | Description |
|------|------|
| `fetch` | Fetch a web page (tiered rendering, stealth, js_extract supported); injection stripping on by default (`sanitize: false` opts out) and `capture_xhr` returns the page's own API responses alongside the text |
| `eval` | Execute JavaScript on the page (async/Promise supported) |
| `click` | Click a page element (CSS selector) |
| `search` | Multi-engine aggregated search (Baidu/Bing/Sogou/Sogou WeChat/Google) |
| `download` | Stream a file to disk with SHA-256 and resume support |
| `cache` | Query the local cache of fetched pages and past searches (full-text incl. CJK, full-content `get`, stats, filtered clear) |
| `render_markdown` | Render markdown into a deterministic, self-contained HTML document; fenced `archify` blocks (typed diagram JSON — sequence / workflow / architecture / dataflow / lifecycle) become inline-SVG diagrams; `theme`/`preset`/`quality` (showcase audit) and optional `session_id` viewport grading |
| `render_video` | Render a page's animation timelines (`window.__timelines`, GSAP-style `duration()`+`pause(t)`) to a base64 MP4 — deterministic seek per frame (`t=i/fps`), in-process paint, ffmpeg encode; needs ffmpeg on PATH and the `screenshot` feature |
| `render_pdf` | Cut a rendered page into pages and package as base64 PDF, per-page PNGs, PPTX (one slide per page), editable PPTX (`format="pptx-native"`: element-level DrawingML — text runs/shapes/gradients/images; requires `selector`) or DOCX (one page-sized section per page) — print mode paginates at top-level block boundaries (default A4 @96dpi), slides mode makes one page per CSS-selector match sized to the element; needs the `screenshot` feature |

#### Session Tools

| Tool | Description |
|------|------|
| `session_create` | Create an interactive browser session; with `persistent: true` the login state survives idle eviction and server restarts — the same `session_id` revives logged-in |
| `import_curl` | Paste a DevTools "Copy as cURL" command → a live session already carrying that site's cookies, anchored at the copied request's URL — the human logs in (CAPTCHA/SMS once) in their own Chrome, the agent continues from there; bash/PowerShell/cmd flavors all parse |
| `session_clone` | Derive a new session carrying the full login state (cookies + storage + viewport + dialog policy); the source stays untouched — snapshot before risky actions, or run one login in parallel |
| `session_list` | List live sessions with idle age and time left before auto-eviction (discover one to reuse) |
| `session_navigate` | Navigate to a new URL within a session |
| `session_state` | Get the indexed page state |
| `session_cookies` | Export the session's current cookies as full Set-Cookie strings (`name=value; Domain=…; Path=/`, for login-state reuse — cross-subdomain state survives the round-trip) |
| `session_storage` | Snapshot the session's `localStorage`/`sessionStorage` for the current origin — the half of login state cookies can't carry; restore it in a new session via `session_create`'s `storage` field |
| `session_console` | Read the session's recent page console output (`log/info/warn/error/dialog` ring buffer of 500, filters: `level`/`since_ts`/`url_contains`/`limit`) — the fastest way to see why a page misbehaves |
| `session_click` | Click an element by index |
| `session_click_xy` | Click at viewport coordinates via the real mouse chain (`pointerdown`→`click`, hit-tested) — for canvas/map/custom widgets; `click_count: 2` adds `dblclick` |
| `session_drag` | Press at `from`, glide through interpolated `mousemove` events, release at `to` — drags map markers/canvas selections the way a real pointer would |
| `session_input` | Type text by index (`input`+`change` dispatched; `events:"full"` for per-character keyboard cycles) |
| `session_scroll` | Scroll the page |
| `session_eval` | Execute JavaScript in the session |
| `session_dialog` | Inspect/steer dialog policy (`alert`/`confirm`/`prompt` never block: auto-answered, logged, `list`/`accept`/`dismiss`) |
| `session_viewport` | Set the session's viewport (device emulation): media queries re-evaluate, `mobile: true` flips `pointer: coarse` / `hover: none`; override survives navigation |
| `session_screenshot` | Screenshot the session's current DOM state (mutations included) as a base64 PNG; optional `width`/`height`/`full_page`/`selector` |
| `session_wait` | Wait until a CSS selector matches or a JS predicate turns truthy, with a timeout — the page's event loop keeps running while waiting, so this replaces blind sleeps for async content |
| `session_network` | Read the session's network request log; `filter: "media"` extracts playback/stream URLs (m3u8, mp4, ...) actually requested by the page — the reliable way to get a real video link. `include_bodies: true` adds an `xhr` array with the page's script-initiated response bodies (its own API face), narrowed by `url_contains` |
| `session_export` | Export the session's recorded actions: a runnable curl replay script (default), the raw action log (`format=jsonl`), or a flow.json document (`format=json` — cookies stripped, editable ops) that `flow_run` replays server-side |
| `flow_run` | Run a flow to completion — zero model tokens: an inline flow document or a server-side `workflow/<name>/flow.json` asset, `{{var}}` substitution, `wait`/`expect` gates, `save` outputs; fails with a receipt (failing step, reason, URL, screenshot) and the session stays alive; `session_id` composes flows with imported login state |
| `session_close` | Close the session (for a persistent one this drops the on-disk login snapshot — idle expiry keeps it, an explicit close does not) |

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
| sanitize | bool | | `true` | Strip prompt-injection carriers (zero-width chars, hidden-span text, instruction-shaped lines) from text/markdown output; response carries a `sanitize_report` when anything fired |
| capture_xhr | string[] | | `null` | Return the page's script-initiated XHR/fetch response bodies as a first-class `xhr` array. Entries are URL substrings; `[]` = every XHR/fetch |

#### `render_markdown` Parameters

| Parameter | Type | Required | Default | Description |
|------|------|------|------|------|
| markdown | string | ✅ | — | Markdown source. Fenced code blocks tagged `archify` carry typed zero-coordinate diagram JSON (`sequence` / `workflow` / `architecture` / `dataflow` / `lifecycle`) rendered to inline SVG by the layout engine; Mermaid sources must be translated to archify JSON by the caller |
| theme | string | | `"light"` | Color scheme: `light` / `dark` — baked into the artifact at generation time |
| preset | string | | `"classic"` | Visual preset: `classic` / `signal-flow` / `blueprint` / `editorial` |
| quality | string | | `"standard"` | `showcase` runs the delivery-gate composition audit (route crossings, label clearance, rhythm) — it grades the artifact without changing a byte |
| session_id | string | | `null` | Load the finished artifact into a live interactive session; the reply grades how it fits the viewport (`fits` / `tall` / `wide` / `oversized`) |

The output is deterministic — same input, same bytes — and the receipt carries the artifact's `sha256` so that's verifiable. Guided-view tabs and the `window.agxViewer` runtime (`focus` / ego views / `route` / `reach`) ship inside the artifact. Diagram vocabulary adapted from archify (MIT, itself based on Cocoon-AI's architecture-diagram-generator).

#### `session_create` Parameters

| Parameter | Type | Required | Default | Description |
|------|------|------|------|------|
| url | string | | `null` | Initial URL |
| use_proxy | bool | | `false` | Route through a proxy |
| cookies | string[] \| object[] | | `[]` | Inject cookies (`"name=value",...` or CDP-style objects) so the session starts already logged in. Pair with `session_cookies` to reuse login state |
| storage | object | | `null` | Web storage to inject after the initial navigation lands: `{"local_storage": {"k":"v"}, "session_storage": {"k":"v"}}`. Round-trips with `session_storage` |
| ttl_secs | u64 | | `480` | Idle time-to-live in seconds before the session is evicted (clamped 60..3600). Raise it for long workflows |
| keepalive | bool | | `false` | Exempt the session from the idle reaper: it lives until `session_close` or server exit — a workflow interrupted by long non-browser steps keeps its login state |
| persistent | bool | | `false` | Persist the login state (cookies, `localStorage`/`sessionStorage`, viewport, dialog policy) to the server-side store after every action. If the session idles out — or the server restarts — the same `session_id` revives logged-in on the next call. `session_close` drops the snapshot; idle expiry keeps it (Playwright storageState semantics, but keyed by the session id you already hold) |
| width / height | u32 | | `null` | Initial viewport, pinned for the session's life (survives navigation) |
| mobile | bool | | `false` | Mobile emulation for the initial viewport (`pointer: coarse`, `hover: none`, `maxTouchPoints = 5`) |

#### Session Operation Parameters

All session operations require the `session_id` parameter. `click`/`input` also need `index` (from `session_state`); `input` additionally needs `text`; `eval` needs `script`; `navigate` needs `url`; `clone` needs nothing but the source id. The acting/rendering tools take optional extras: `click_xy` needs `x`/`y` (optional `button`, `click_count`); `drag` needs `from`/`to` (optional `steps`, `delay_ms`); `viewport` accepts `width`/`height`/`mobile` (all optional — omit to keep current); `screenshot` accepts `width`/`height`/`full_page`/`selector`/`selector_all`; `wait` takes exactly one of `selector` / `predicate` plus `timeout_ms` (default 10000, max 120000); `export` accepts `format` (`bash` default / `jsonl` / `json` for a flow document); `flow_run` takes exactly one of `flow` / `name`, plus optional `vars` and `session_id`; `network` accepts `filter: "media"` or `include_bodies: true` (plus `url_contains`/`body_max_chars`); `dialog` accepts `action` (`list` default / `accept` / `dismiss`) plus optional `prompt_text`; `console` accepts `level`/`since_ts`/`url_contains`/`limit`; `storage`/`cookies` take only `session_id`.

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
| `AGINXBROWSER_UA` | macOS Chrome145 persona | Spoofed User-Agent for browser traffic (a pinned persona from the fingerprint pool — see `/health`'s `ua` for what this instance presents; search-engine transports keep their own defaults). Startup logs a `fingerprint mismatch` warning when the UA's browser family/major version disagrees with the TLS fingerprint (default chrome145) — an intentionally coherent pair avoids a WAF tell |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language header |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` cache TTL (seconds); `0` disables |
| `AGINXBROWSER_MCP_ALLOWED_HOSTS` | unset | Extra `Host` values accepted by `/mcp` (comma-separated) — the DNS-rebinding guard defaults to loopback; add your LAN IP / Docker hostname when other machines call the instance |
| `AGINXBROWSER_DOWNLOAD_DIR` | `.` | Directory where `/download` saves files |
| `AGINXBROWSER_WORKFLOW_DIR` | `./workflow` | Where `flow_run(name=…)` looks for `<name>/flow.json` assets (resolved from the server's working directory); drop a directory in to deploy, no rebuild |
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
