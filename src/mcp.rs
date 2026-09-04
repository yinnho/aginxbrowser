use std::sync::Arc;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    tool, tool_handler, tool_router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService, StreamableHttpServerConfig,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::render::smart_fetch;
use crate::server::{do_click, do_eval, do_search};
use crate::session::{self, SessionCommand};
use crate::{ClickRequest, EvalRequest, FetchRequest, OutputFormat, SearchRequest};

// ============================================================================
// Tool parameter structs (JsonSchema → auto-generated MCP input schemas)
// ============================================================================

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FetchParams {
    /// The URL to fetch
    pub url: String,
    /// Output format: "markdown", "html", or "text" (default: markdown)
    #[serde(default = "default_format")]
    pub format: String,
    /// CSS selector to extract specific content
    #[serde(default)]
    pub selector: Option<String>,
    /// Seconds to wait for JS rendering
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Route through proxy (for blocked foreign sites)
    #[serde(default)]
    pub use_proxy: bool,
    /// Maximum characters to return (default: 50000)
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// Auto-detect and bypass Cloudflare Turnstile challenges (default: true)
    #[serde(default = "default_true")]
    pub auto_bypass_challenge: bool,
    /// Rendering strategy: "auto" (default), "http", or "obscura"
    #[serde(default)]
    pub render_tier: crate::RenderTier,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133", etc.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// JS expression to extract from the page after rendering
    #[serde(default)]
    pub js_extract: Option<JsExtractParams>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct JsExtractParams {
    /// JavaScript expression to evaluate (e.g. "window.__INITIAL_STATE__")
    pub expression: String,
    /// Timeout in milliseconds (default: 5000)
    #[serde(default = "default_js_timeout")]
    pub timeout_ms: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EvalParams {
    /// The URL to load
    pub url: String,
    /// JavaScript code to execute (supports async/Promise)
    pub script: String,
    /// Seconds to wait before executing
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClickParams {
    /// The URL to load
    pub url: String,
    /// CSS selector of element to click
    pub selector: String,
    /// Seconds to wait after click
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Search query
    pub q: String,
    /// Fetch content for top N results
    #[serde(default)]
    pub fetch_top: usize,
    /// Search categories (default: general)
    #[serde(default = "default_categories")]
    pub categories: String,
    /// Maximum number of results (default: 10)
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// Max characters per result content
    #[serde(default = "default_max_chars_per")]
    pub max_chars_per: usize,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadParams {
    /// URL of the file to download (http/https)
    pub url: String,
    /// Explicit output filename. When omitted: Content-Disposition → URL tail → "download"
    #[serde(default)]
    pub filename: Option<String>,
    /// Resume an interrupted download when a local partial file exists
    #[serde(default)]
    pub resume: bool,
    /// Route through proxy (default: false; auto-enabled for known blocked domains)
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to send with the request (["name=value", ...]) for gated downloads
    #[serde(default)]
    pub cookies: Vec<String>,
}

// ---------------------------------------------------------------------------
// Session tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateParams {
    /// Initial URL to navigate to (optional)
    #[serde(default)]
    pub url: Option<String>,
    /// Route through proxy (default: false)
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation (["name=value", ...]). Lets the
    /// session start already logged-in. Round-trips with session_cookies.
    #[serde(default)]
    pub cookies: Vec<String>,
    /// Web Storage to inject after the initial navigation lands:
    /// {"local_storage": {"k":"v"}, "session_storage": {"k":"v"}}. For login
    /// states that live in localStorage rather than the cookie jar.
    /// Round-trips with session_storage.
    pub storage: Option<Value>,
    /// Idle time-to-live in seconds before the session is evicted
    /// (default: 480, clamped 60..3600). Raise it for long workflows.
    pub ttl_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCookiesParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionNavigateParams {
    /// Session ID
    pub session_id: String,
    /// URL to navigate to
    pub url: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionStateParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionClickParams {
    /// Session ID
    pub session_id: String,
    /// Element index (from /state output)
    pub index: usize,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionInputParams {
    /// Session ID
    pub session_id: String,
    /// Element index (from /state output)
    pub index: usize,
    /// Text to type into the input field
    pub text: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionScrollParams {
    /// Session ID
    pub session_id: String,
    /// Scroll direction: "up" or "down" (default: down)
    #[serde(default = "default_scroll_dir")]
    pub direction: String,
    /// Scroll amount in viewport-heights (default: 3)
    #[serde(default = "default_scroll_amount")]
    pub amount: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionEvalParams {
    /// Session ID
    pub session_id: String,
    /// JavaScript code to execute
    pub script: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionViewportParams {
    /// Session ID
    pub session_id: String,
    /// Viewport width in CSS pixels; omit to keep the current width
    pub width: Option<u32>,
    /// Viewport height in CSS pixels; omit to keep the current height
    pub height: Option<u32>,
    /// Mobile emulation: matchMedia answers pointer:coarse / hover:none and
    /// navigator.maxTouchPoints reports 5 (default: false)
    #[serde(default)]
    pub mobile: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionScreenshotParams {
    /// Session ID
    pub session_id: String,
    /// Render width in CSS pixels; defaults to the session's current viewport
    pub width: Option<u32>,
    /// Render height in CSS pixels; defaults to the session's current viewport
    pub height: Option<u32>,
    /// Capture the full scrollable page instead of the viewport (default: false)
    #[serde(default)]
    pub full_page: bool,
    /// CSS selector: capture only that element's box
    pub selector: Option<String>,
    /// With selector, capture every match (default: first match only)
    #[serde(default)]
    pub selector_all: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionWaitParams {
    /// Session ID
    pub session_id: String,
    /// CSS selector to wait for (e.g. ".price-card")
    pub selector: Option<String>,
    /// JS expression polled until truthy (e.g. "document.querySelectorAll('.card').length >= 3")
    pub predicate: Option<String>,
    /// Give up after this many milliseconds (default: 10000, max: 120000)
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wait_timeout_ms() -> u64 {
    10_000
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionExportParams {
    /// Session ID
    pub session_id: String,
    /// Output format: "bash" (default) renders a runnable curl script that
    /// replays every recorded action against a fresh session; "jsonl" returns
    /// the raw action log, one JSON object per line
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionNetworkParams {
    /// Session ID
    pub session_id: String,
    /// "media" extracts playback/stream links (m3u8/HLS, mp4, dash, ...) from the requests the page actually issued - the reliable way to get a real video link, since URLs embedded in page HTML are often decoys. Media elements and player iframes the engine never fetches (video/audio/source src, iframe src) are merged in as candidates: entries carry via="network" (confirmed requests) or via="dom" (candidates, with their tag; iframes surface as kind "iframe" - player pages to navigate or sniff inside, not playable URLs). Omit to list every request as compact rows.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CacheParams {
    /// Full-text search over cached page contents, titles, URLs and past search queries. Omit to list the latest rows.
    #[serde(default)]
    pub query: Option<String>,
    /// Only rows whose URL contains this substring
    #[serde(default)]
    pub url: Option<String>,
    /// Return the FULL cached content of this exact URL instead of listing hits
    #[serde(default)]
    pub get: Option<String>,
    /// Which rows to search: "auto" (default, pages + searches), "pages", or "searches"
    #[serde(default)]
    pub kind: Option<String>,
    /// Only rows stored within the last N hours
    #[serde(default)]
    pub since_hours: Option<u64>,
    /// Maximum rows returned (default: 10, max 100)
    #[serde(default = "default_max_results")]
    pub limit: usize,
    /// Return row counts and database size instead of rows
    #[serde(default)]
    pub stats: bool,
    /// Delete matching rows instead of returning them (requires url, since_hours, or all)
    #[serde(default)]
    pub clear: bool,
    /// With clear: delete everything cached for this caller
    #[serde(default)]
    pub all: bool,
}

fn default_scroll_dir() -> String {
    "down".to_string()
}
fn default_scroll_amount() -> u32 {
    3
}

fn default_format() -> String {
    "markdown".to_string()
}
fn default_max_chars() -> usize {
    50000
}
fn default_categories() -> String {
    "general".to_string()
}
fn default_max_results() -> usize {
    10
}
fn default_max_chars_per() -> usize {
    4000
}
fn default_true() -> bool {
    true
}
fn default_js_timeout() -> u64 {
    5000
}

// ============================================================================
// MCP Server — wraps aginxbrowser HTTP API as MCP tools
// ============================================================================

/// MCP server wrapping aginxbrowser's core operations as MCP tools.
///
/// The sync operations (fetch/eval/click) each call `run_on_local_runtime`
/// internally, which creates its own current-thread Tokio runtime for V8.
/// Since MCP tool handlers run on Tokio's multi-threaded runtime, we must
/// dispatch these calls via `spawn_blocking` to avoid the "cannot start a
/// runtime from within a runtime" panic.
#[derive(Debug, Clone)]
pub struct AginxBrowserMcp {
    /// Local-store owner for this MCP instance: "global" under the default
    /// scope, or a unique id per client session when
    /// AGINXBROWSER_STORE_SCOPE=session (shared deployments).
    pub owner: String,
}

#[tool_router]
impl AginxBrowserMcp {
    #[tool(
        description = "Fetch a webpage and return clean markdown/html/text. Use whenever the agent needs to READ any web page - blogs, docs, articles, JS-rendered SPAs, Cloudflare-protected sites. Static pages are served over plain HTTP (~100ms tier:\"http\"); pages that need JS get the full browser (tier:\"browser\"). render_tier selects auto (default) / http (pure HTTP, refuses the upgrade) / obscura (always browser).",
        annotations(title = "Fetch Webpage", read_only_hint = true)
    )]
    async fn fetch(&self, Parameters(params): Parameters<FetchParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = FetchRequest {
            url: params.url,
            format: match params.format.as_str() {
                "html" => OutputFormat::Html,
                "text" => OutputFormat::Text,
                _ => OutputFormat::Markdown,
            },
            selector: params.selector,
            wait_secs: params.wait_secs,
            use_proxy: params.use_proxy,
            cookies: vec![],
            max_chars: params.max_chars,
            auto_bypass_challenge: params.auto_bypass_challenge,
            render_tier: params.render_tier,
            tls_fingerprint: params.tls_fingerprint,
            js_extract: params.js_extract.map(|j| crate::JsExtractConfig {
                expression: j.expression,
                timeout_ms: j.timeout_ms,
            }),
        };

        match smart_fetch(req).await {
            Ok(resp) => {
                crate::store::record_fetch(&self.owner, &resp);
                let mut out = json!({
                    "url": resp.url,
                    "title": resp.title,
                    "content": resp.content,
                    "truncated": resp.truncated,
                    "tier": resp.tier,
                });
                if !resp.redirected_from.is_empty() {
                    out["redirected_from"] = json!(resp.redirected_from);
                }
                out.to_string()
            }
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        }
    }

    #[tool(
        description = "Execute JavaScript on a webpage and return the result. Supports async/Promise.",
        annotations(title = "Evaluate JavaScript")
    )]
    async fn eval(&self, Parameters(params): Parameters<EvalParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = EvalRequest {
            url: params.url,
            script: params.script,
            wait_secs: params.wait_secs,
            use_proxy: false,
            cookies: vec![],
            tls_fingerprint: None,
        };

        match tokio::task::spawn_blocking(move || do_eval(req)).await {
            Ok(Ok(resp)) => json!({
                "url": resp.url,
                "result": resp.result
            })
            .to_string(),
            Ok(Err(e)) => json!({ "error": format!("{}", e) }).to_string(),
            Err(e) => json!({ "error": format!("task panicked: {}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Click an element on a webpage using CSS selector.",
        annotations(title = "Click Element")
    )]
    async fn click(&self, Parameters(params): Parameters<ClickParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = ClickRequest {
            url: params.url,
            selector: params.selector,
            wait_secs: params.wait_secs,
            use_proxy: false,
            cookies: vec![],
            tls_fingerprint: None,
        };

        match tokio::task::spawn_blocking(move || do_click(req)).await {
            Ok(Ok(resp)) => json!({
                "url": resp.url,
                "clicked": resp.clicked,
                "text_after": resp.text_after
            })
            .to_string(),
            Ok(Err(e)) => json!({ "error": format!("{}", e) }).to_string(),
            Err(e) => json!({ "error": format!("task panicked: {}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Search the web across Baidu/Bing/Sogou/WeChat/Google (aggregated + deduped) and optionally fetch the top results' full content. Use when the agent needs to FIND information online - replaces a search API. Supports image search returning direct image URLs.",
        annotations(title = "Web Search", read_only_hint = true)
    )]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        let categories = params.categories.clone();
        let req = SearchRequest {
            q: params.q,
            fetch_top: params.fetch_top,
            categories: params.categories,
            language: "zh-CN".to_string(),
            max_results: params.max_results,
            max_chars_per: params.max_chars_per,
            wait_secs: 3,
            use_proxy: false,
            engines: Vec::new(),
        };

        // do_search is already async and uses spawn_blocking internally for
        // the fetch_top body-grabbing, so it's safe to call directly.
        match do_search(req).await {
            Ok(resp) => {
                crate::store::record_search(&self.owner, &resp.query, &categories, &resp);
                json!({
                    "query": resp.query,
                    "number_of_results": resp.number_of_results,
                    "results": resp.results
                })
                .to_string()
            }
            Err(e) => json!({ "error": format!("{:?}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Download a file over HTTP(S) with streaming to disk (no memory buffering), SHA-256 integrity hash, and optional resume of interrupted transfers. Filename resolution: explicit param → Content-Disposition → URL tail. Use for binaries, archives, datasets, documents - anything where the agent wants the FILE saved, not its text content read.",
        annotations(title = "Download File")
    )]
    async fn download(&self, Parameters(params): Parameters<DownloadParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        match crate::download::do_download(crate::download::DownloadRequest {
            url: params.url,
            filename: params.filename,
            resume: params.resume,
            use_proxy: params.use_proxy,
            cookies: params.cookies,
        })
        .await
        {
            Ok(resp) => json!({
                "url": resp.url,
                "path": resp.path,
                "filename": resp.filename,
                "size_bytes": resp.size_bytes,
                "content_type": resp.content_type,
                "sha256": resp.sha256,
                "resumed": resp.resumed,
            })
            .to_string(),
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        }
    }

    #[tool(
        description = "Query the LOCAL CACHE of every page this server has fetched and every search it has run. Check here BEFORE re-fetching or re-searching — a hit is instant and free while a fresh fetch costs 5-60s. Use query for full-text search (works for Chinese substrings and English words), get to pull a page's full cached content, stats for counts, clear to delete rows.",
        annotations(title = "Local Cache", read_only_hint = false)
    )]
    async fn cache(&self, Parameters(params): Parameters<CacheParams>) -> String {
        if params.clear {
            return match crate::store::clear(&self.owner, params.url.as_deref(), params.since_hours, params.all)
            {
                Ok((pages, searches)) => json!({
                    "cleared_pages": pages,
                    "cleared_searches": searches
                })
                .to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        if let Some(url) = &params.get {
            return match crate::store::get_page(&self.owner, url) {
                Ok(Some(p)) => json!(p).to_string(),
                Ok(None) => json!({ "error": "not in cache", "url": url }).to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        if params.stats {
            return match crate::store::stats(&self.owner) {
                Ok(s) => json!(s).to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        let q = crate::store::CacheQuery {
            query: params.query,
            url: params.url,
            kind: params.kind.unwrap_or_else(|| "auto".into()),
            since_hours: params.since_hours,
            limit: params.limit,
        };
        match crate::store::query(&self.owner, &q) {
            Ok(r) => json!(r).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    // ------------------------------------------------------------------
    // Session tools
    // ------------------------------------------------------------------

    #[tool(
        description = "Create a persistent interactive browser session for multi-step interaction - clicking, typing, scrolling, reading state across page transitions. Use when the agent must INTERACT with a page (login flows, forms, pagination, click-through) rather than read it once. Returns session_id; persists 8 min idle.",
        annotations(title = "Create Browser Session")
    )]
    async fn session_create(&self, Parameters(params): Parameters<SessionCreateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        let url = params.url.clone();
        let storage = params.storage.clone();
        let id = mgr.create(params.url.as_deref(), params.use_proxy, params.cookies, storage, params.ttl_secs);
        json!({ "session_id": id, "url": url }).to_string()
    }

    #[tool(
        description = "Navigate a browser session to a new URL.",
        annotations(title = "Session Navigate")
    )]
    async fn session_navigate(&self, Parameters(params): Parameters<SessionNavigateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Navigate {
            url: params.url.clone(),
            reply,
        }).await {
            Ok(resp) => json!({ "url": resp.url, "title": resp.title }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Get the current page state as an indexed list of interactive elements. Returns compact text with [N] indexes for use with click/input tools.",
        annotations(title = "Session State", read_only_hint = true)
    )]
    async fn session_state(&self, Parameters(params): Parameters<SessionStateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::State { reply }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Export the session's current cookies as [\"name=value\", ...] for the page's URL. Use to persist a logged-in session and replay it later via session_create with cookies. Round-trips with session_create's cookies field.",
        annotations(title = "Session Cookies", read_only_hint = true)
    )]
    async fn session_cookies(&self, Parameters(params): Parameters<SessionCookiesParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Cookies { reply }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Snapshot the session's localStorage/sessionStorage for the current origin: \
{url, local_storage, session_storage}. Feed it back via session_create's `storage` field to restore \
a logged-in state in a new session — the half of login state that cookies can't carry (many sites \
keep the session token in localStorage). Call before the session idles out.",
        annotations(title = "Session Storage", read_only_hint = true)
    )]
    async fn session_storage(&self, Parameters(params): Parameters<SessionCookiesParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Storage { reply }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Click an interactive element by its index (from session_state output).",
        annotations(title = "Session Click")
    )]
    async fn session_click(&self, Parameters(params): Parameters<SessionClickParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Click {
            index: params.index,
            reply,
        }).await {
            Ok(resp) => json!({ "url": resp.url, "clicked": resp.clicked, "text_after": resp.text_after }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Type text into an input/textarea element by its index (from session_state output).",
        annotations(title = "Session Input")
    )]
    async fn session_input(&self, Parameters(params): Parameters<SessionInputParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Input {
            index: params.index,
            text: params.text.clone(),
            reply,
        }).await {
            Ok(filled) => json!({ "filled": filled }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Scroll the page up or down by a number of viewport-heights.",
        annotations(title = "Session Scroll")
    )]
    async fn session_scroll(&self, Parameters(params): Parameters<SessionScrollParams>) -> String {
        let direction = match params.direction.as_str() {
            "up" => session::ScrollDirection::Up,
            _ => session::ScrollDirection::Down,
        };
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Scroll {
            direction,
            amount: params.amount,
            reply,
        }).await {
            Ok(scrolled) => json!({ "scrolled": scrolled }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Execute arbitrary JavaScript in the browser session and return the result.",
        annotations(title = "Session Eval")
    )]
    async fn session_eval(&self, Parameters(params): Parameters<SessionEvalParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Eval {
            script: params.script.clone(),
            reply,
        }).await {
            Ok(result) => json!({ "result": result }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Set the session's viewport (device emulation): scripts see innerWidth/innerHeight \
move, media queries like (max-width: 600px) re-evaluate, element rects re-anchor, and mobile=true \
flips pointer/hover matchMedia answers to coarse/none. Omitted width/height keeps the current value.",
        annotations(title = "Session Viewport")
    )]
    async fn session_viewport(&self, Parameters(params): Parameters<SessionViewportParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Viewport {
            width: params.width,
            height: params.height,
            mobile: params.mobile,
            reply,
        }).await {
            Ok(viewport) => json!({ "viewport": viewport }).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Screenshot the session's CURRENT DOM state (mutations from clicks/evals included) \
as a base64 PNG via the built-in renderer. Width/height default to the session's viewport, so \
session_viewport + session_screenshot shows the responsive layout. Returns \
{url, width, height, image_base64, format}.",
        annotations(title = "Session Screenshot")
    )]
    async fn session_screenshot(&self, Parameters(params): Parameters<SessionScreenshotParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Screenshot {
            width: params.width,
            height: params.height,
            full_page: params.full_page,
            selector: params.selector.clone(),
            selector_all: params.selector_all,
            reply,
        }).await {
            Ok(s) => s,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Wait until a CSS selector matches or a JS predicate turns truthy, with a timeout. \
The page's event loop keeps running while waiting (fetches, timers, promise chains progress), so this \
replaces blind sleeps for async content: navigate, session_wait for '.price-card', then click/read. \
Returns {matched, elapsed_ms, detail:{tag,text} or the predicate value}; errors with `timeout ...` \
naming the selector/predicate on expiry. Exactly one of selector/predicate.",
        annotations(title = "Session Wait", read_only_hint = true)
    )]
    async fn session_wait(&self, Parameters(params): Parameters<SessionWaitParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Wait {
            selector: params.selector.clone(),
            predicate: params.predicate.clone(),
            timeout_ms: params.timeout_ms,
            reply,
        }).await {
            Ok(s) => s,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Read the session's network request log. filter=\"media\" extracts playback/stream URLs (m3u8/HLS, mp4, dash, flv...) actually requested by the page's player at runtime - the reliable way to get a real video link, since links embedded in page HTML are often decoys. Media elements and player iframes the engine never fetches (video/audio/source/iframe src) are merged in as candidates: via=\"network\" entries are confirmed requests, via=\"dom\" entries are candidates carrying their tag (iframes = kind \"iframe\", navigate into them to sniff). Default returns every request as compact rows (method/url/status/type/size). Navigate to the video page first, let it load, then call this.",
        annotations(title = "Session Network Sniffer", read_only_hint = true)
    )]
    async fn session_network(&self, Parameters(params): Parameters<SessionNetworkParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Network {
            media_only: params.filter.as_deref() == Some("media"),
            reply,
        }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "List live browser sessions with idle age and the time left before auto-eviction. Use to discover a session to reuse instead of creating a new one; sessions expire after 8 min idle.",
        annotations(title = "List Browser Sessions", read_only_hint = true)
    )]
    async fn session_list(&self) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        let sessions = mgr.list();
        json!({ "count": sessions.len(), "sessions": sessions }).to_string()
    }

    #[tool(
        description = "Export a browser session's recorded action log. Format \"bash\" (default) returns a runnable curl script that replays every recorded action (navigate/click/input/scroll/eval) against a fresh session on this server — hand it to a shell or cron, zero model tokens. Format \"jsonl\" returns the raw action log, one JSON object per line.",
        annotations(title = "Export Session Replay Script", read_only_hint = true)
    )]
    async fn session_export(&self, Parameters(params): Parameters<SessionExportParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        let jsonl = match mgr.send(&params.session_id, |reply| SessionCommand::Export { reply }).await {
            Ok(j) => j,
            Err(e) => return json!({ "error": e }).to_string(),
        };
        match params.format.as_deref() {
            Some("jsonl") => json!({ "format": "jsonl", "actions": jsonl }).to_string(),
            _ => {
                let script = session::replay_bash(&jsonl, "http://127.0.0.1:8089");
                json!({ "format": "bash", "script": script }).to_string()
            }
        }
    }

    #[tool(
        description = "Close a browser session and free its resources.",
        annotations(title = "Session Close")
    )]
    async fn session_close(&self, Parameters(params): Parameters<SessionCloseParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.close(&params.session_id);
        json!({ "ok": true }).to_string()
    }
}

#[tool_handler]
impl ServerHandler for AginxBrowserMcp {}

// ============================================================================
// Server startup
// ============================================================================

/// Start MCP server on stdio transport.
pub async fn run_mcp_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!("Starting aginxbrowser MCP server on stdio");
    AginxBrowserMcp {
        owner: crate::store::session_owner(),
    }
    .serve(rmcp::transport::io::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

/// Build an MCP server for the streamable HTTP transport, mounted at `/mcp`.
///
/// rmcp's streamable HTTP server validates the inbound `Host` header against
/// `allowed_hosts` (defaults to loopback only) to prevent DNS rebinding, so a
/// public deployment must list its own hostname.
pub fn mcp_http_service() -> StreamableHttpService<AginxBrowserMcp, LocalSessionManager> {
    let mut hosts = vec![
        "browser.aginx.net".to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ];
    // rmcp's Host-header guard (DNS-rebinding protection) defaults to
    // loopback only, so an instance reached over a LAN IP or a docker
    // hostname would have /mcp rejected. Operators extend the allowlist
    // with a comma-separated list instead of rebuilding.
    if let Ok(extra) = std::env::var("AGINXBROWSER_MCP_ALLOWED_HOSTS") {
        hosts.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(String::from),
        );
    }
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(hosts);
    StreamableHttpService::new(
        || Ok(AginxBrowserMcp {
            owner: crate::store::session_owner(),
        }),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
