# aginxbrowser — DeepSeek Harness plugin

Web access for DSH agents: **fetch JS-rendered / protected pages as clean markdown**, **aggregated search across 14 engines**, and **stateful interactive sessions** — all against a single-binary Rust browser engine, no local Chrome, no Node runtime.

Every session returns a **live view URL**: open it in any browser to watch the agent browse frame-by-frame and take over with the mouse. Oversight without screen-sharing.

## Why this instead of a headless-Chromium tool

- **Markdown first, pixels on demand.** Reads default to clean markdown (with prompt-injection stripping); screenshots are opt-in. In our benchmarks the engine settles a page in ~0.5s and ~227MB where headless Chrome needs ~4s and 2.1GB/tab — and markdown output skips the pixel→vision→coordinate round-trip entirely.
- **Interactive elements come as an indexed list** (`[0] <a href=…>Learn more</a>` with rects) — click by index or coordinates, no DOM-dump spelunking.
- **Sessions hold logins.** Persistent sessions survive engine restarts (same id revives logged-in); cookies can be seeded from a DevTools "Copy as cURL" paste.
- **Search with a memory.** 14 engines (Baidu, Sogou, WeChat 公众号 articles, Bing, …) deduped and scored, cached locally for repeat queries, with per-engine health surfaced honestly.
- **The human can always look.** `live_view_url` per session — watch, or click into the same session.

## Install

The plugin talks to an aginxbrowser engine over HTTP. The default is the hosted instance at `https://browser.aginx.net`; to run your own, see [Self-hosting](#self-hosting).

Install via `dshx` (tarball or git URL both work):

```bash
npm pack                     # produces dsh-aginxbrowser-0.1.0.tgz
dshx install aginxbrowser dsh-aginxbrowser-0.1.0.tgz
dshx list                    # should show: [on] aginxbrowser
```

For DSH Desktop profiles, reference the package under its real name in **both** places — the `package.json` dependency key and the `dsh.profile.bundles` entry:

```json
"dependencies": { "dsh-aginxbrowser": "file:./dsh-aginxbrowser-0.1.0.tgz" }
```

DSH Desktop 2.0.5+ enforces dependency-key == actual package name and drops into recovery mode otherwise.

For developing this plugin against a harness checkout, overlay the source directly:

```yaml
# cordis.yml
plugins:
  '@deepseek-ai/dsh':
    $overlay:
      - insert:
          - id: 'dsh-aginxbrowser': '/absolute/path/to/dsh-aginxbrowser/lib/index.js'
```

```bash
pnpm i
pnpm dsh web --patch ./cordis.yml   # Web UI at 127.0.0.1:3080
```

## Config

| key | default | |
|---|---|---|
| `baseUrl` | `https://browser.aginx.net` | Engine base URL. Point at your own instance to run fully local. |
| `apiKey` | *(empty)* | Bearer token, if the instance requires one. |
| `timeoutMs` | `90000` | Per-request timeout against the engine. |
| `screenshotDir` | `$TMPDIR/aginxbrowser-dsh` | Where `agx_session_screenshot` writes PNGs. |

## Tools

Read side:

- **`agx_fetch`** — one page as markdown/html/text. Auto tier tries plain HTTP first, renders only when needed (`render_tier` to force). Cloudflare-style challenges, stealth TLS fingerprints, JS-state extraction (`js_extract`), background XHR body capture (`capture_xhr`).
- **`agx_search`** — 14-engine aggregated search; `fetch_top` also pulls top-N page contents.
- **`agx_doctor`** — engine health: live engines, feature gates. Tell engine-side issues from site-side ones.

Session side (stateful, indexed, live-viewable):

- **`agx_session_create`** — open a session; returns `session_id` + `live_view_url`. Viewport pinning, cookie seeding, `persistent` (survives restarts), `keepalive`, mobile emulation.
- **`agx_session_state`** — the page as `[N] <tag …>` listings with rects; N feeds click/input.
- **`agx_session_navigate`** / **`agx_session_click`** / **`agx_session_click_xy`** / **`agx_session_input`** / **`agx_session_scroll`** / **`agx_session_wait`** / **`agx_session_eval`** — the interaction set; `wait` supports selector or JS predicate; `click_xy` does full mouse-event fidelity for canvas surfaces.
- **`agx_session_screenshot`** — PNG to a local file (default captures the live viewport including unsaved form input).
- **`agx_session_list`** / **`agx_session_close`** — housekeeping.

Typical loop: `agx_fetch` for one-shot reads; for interaction, `agx_session_create` → `agx_session_state` → `agx_session_click`/`agx_session_input` → `agx_session_state` again.

## Live view

`agx_session_create` returns e.g. `https://browser.aginx.net/live.html?session=s_42`. The page polls frames from the session and forwards your mouse clicks into it — the same session the agent is driving. Hand it to whoever is supervising the agent.

## Self-hosting

```bash
curl -fsSL https://browser.aginx.net/install.sh | sh   # single static binary
aginxbrowser --bind 127.0.0.1:8788
```

Also on Docker (`ghcr.io/yinnho/aginxbrowser`), Homebrew (`brew install yinnho/tap/aginxbrowser`), and as an [Umbrel app](https://apps.umbrel.com/app/aginxbrowser). Then set `baseUrl: http://127.0.0.1:8788`.

## Security notes

- Page content is returned as **data, not instructions**; the engine's fetch/search output strips prompt-injection payloads (zero-width chars, instruction-shaped lines, hidden text) with an observable `sanitize_report`.
- Cookies you pass to sessions are secrets — this plugin never logs them.
- SSRF posture is the engine's job, not the client's: the hosted instance enforces its own network policy regardless of what URLs tools request.
- Treat `agx_session_eval` results like any other web content: untrusted data.

## License

Apache-2.0. Engine source and HTTP API docs: [yinnho/aginxbrowser](https://github.com/yinnho/aginxbrowser).
