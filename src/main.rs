use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

mod browser;
mod captcha;
mod config;
mod cookie;
mod doctor_cli;
mod download;
mod error;
mod firecrawl_compat;
mod mcp;
mod page;
mod rate;
mod render;
mod robots;
mod search;
mod server;
mod session;
#[cfg(feature = "screenshot")]
mod screenshot;

// Inlined Diting engine (formerly external crates).
mod diting_dom;
mod diting_net;
mod diting_js;
mod diting_browser;
mod diting_cdp;
// Cascade layer absorbed from upstream obscura-render (read-only slice,
// not yet wired to the product pipeline — see docs/engine/render.md).
mod diting_css;
// Taffy fork-delta classification tests (obscura's vendored taffy vs the
// stock 0.13.0 our blitz pipeline pins) — docs/engine/render.md §11.
#[cfg(feature = "screenshot")]
mod diting_layout;
// Bundled CJK font supply for /screenshot determinism (batch 3c) —
// docs/engine/render.md §18.
#[cfg(feature = "screenshot")]
mod diting_fonts;

use server::{do_click, do_eval, do_fetch, do_search, SearchError};
use render::smart_fetch;

#[derive(Debug, Deserialize, Clone)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default)]
    pub format: OutputFormat,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Route through AGINXBROWSER_PROXY. Default false (direct) — set true for
    /// foreign sites that are blocked or slow without a proxy.
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation (`["name=value", ...]`). For sites
    /// that gate content behind a logged-in session (e.g. WeChat articles).
    #[serde(default)]
    pub cookies: Vec<String>,
    /// Truncate `content` to at most this many characters. 0 = no limit.
    /// Default 50000 — keeps responses from blowing up an LLM context window.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// Automatically detect and bypass Cloudflare Turnstile challenges.
    /// When a "Just a moment..." page is detected, waits up to 25s for
    /// the `cf_clearance` cookie and re-navigates. Default: true.
    #[serde(default = "default_true")]
    pub auto_bypass_challenge: bool,
    /// Rendering strategy. `auto` (default): try fast HTTP-direct first, fall
    /// back to the JS browser only if the page needs rendering. `http`: force
    /// HTTP-only (fastest, no JS). `browser`: always use the full browser.
    #[serde(default)]
    pub render_tier: RenderTier,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133",
    /// "safari17_5", "edge145", etc. None → Chrome145 default.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// Optional JS expression to evaluate after page load. The result is
    /// returned as `js_extract_result` in the response. Example:
    /// `"JSON.stringify(window.__INITIAL_STATE__)"`.
    #[serde(default)]
    pub js_extract: Option<JsExtractConfig>,
}

/// Configuration for JS global extraction after page load.
#[derive(Debug, Deserialize, Serialize, Clone, schemars::JsonSchema)]
pub struct JsExtractConfig {
    /// JS expression to evaluate. Must return a JSON-serializable value.
    pub expression: String,
    /// Maximum time (ms) to wait for the expression to return non-null.
    /// The page is settled and the expression retried until it succeeds or
    /// this timeout expires. Default 5000.
    #[serde(default = "default_js_extract_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_js_extract_timeout_ms() -> u64 {
    5000
}

/// Tiered rendering strategy selector.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RenderTier {
    /// HTTP-direct first, fall back to diting browser. (default)
    #[default]
    Auto,
    /// Pure HTTP, no V8/JS. Fastest; misses JS-rendered content.
    Http,
    /// Always use the diting browser (current behaviour pre-tiering).
    Obscura,
}

fn default_max_chars() -> usize {
    50_000
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Markdown,
    Html,
    Text,
}

#[derive(Debug, Deserialize)]
pub struct ClickRequest {
    pub url: String,
    pub selector: String,
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation.
    #[serde(default)]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EvalRequest {
    pub url: String,
    pub script: String,
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation.
    #[serde(default)]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FetchResponse {
    pub url: String,
    pub title: Option<String>,
    pub content: String,
    /// True when `content` was truncated to `max_chars`.
    #[serde(default)]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captcha_event: Option<crate::captcha::CaptchaEvent>,
    /// Result of evaluating `js_extract.expression` after page load.
    /// Only present when `js_extract` was set in the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub js_extract_result: Option<serde_json::Value>,
    /// Which tier served the request: `"http"` (Tier 1, plain HTTP+convert)
    /// or `"browser"` (Tier 2, V8 render). Absent on surfaces that predate
    /// tiering. Lets callers see WHY a fetch was fast/slow — and the
    /// benchmark measure tier hit-rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct ClickResponse {
    pub url: String,
    pub selector: String,
    pub clicked: bool,
    pub text_after: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EvalResponse {
    pub url: String,
    pub result: serde_json::Value,
}

/// /screenshot request: render a page's JS-rendered DOM to a PNG.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct ScreenshotRequest {
    pub url: String,
    /// Viewport width in CSS pixels. Default 1280.
    #[serde(default = "default_screenshot_width")]
    pub width: u32,
    /// Viewport height in CSS pixels. Default 800 (ignored when `full_page`).
    #[serde(default = "default_screenshot_height")]
    pub height: u32,
    /// Device pixel ratio. Default 1.0. Higher = sharper but larger PNG.
    #[serde(default = "default_screenshot_scale")]
    pub scale: f32,
    /// Capture the full scrolled page height (tracks computed content height,
    /// capped at 16000px) instead of just the viewport. Default true.
    #[serde(default = "default_screenshot_full_page")]
    pub full_page: bool,
    /// Extra seconds to wait for JS rendering after load before capturing.
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// CSS selector for element-level capture. Default (None): whole page.
    /// With `selector_all=false` the image is cropped to the first match and
    /// its rect is returned; with `selector_all=true` the image renders
    /// normally and rects for every match are returned.
    #[serde(default)]
    pub selector: Option<String>,
    /// With `selector`: report rects for ALL matches instead of cropping to
    /// the first. Default false.
    #[serde(default)]
    pub selector_all: bool,
    /// With `selector`: also run the diting layout engine over the page HTML
    /// and return its rects in `selector_rects_diting` (an independent
    /// cross-check of the Blitz pipeline). Default false.
    #[serde(default)]
    pub diting_rects: bool,
    /// Render engine: "diting" (default — our own css+layout+paint stack,
    /// no Stylo/vello/parley in the path) or "blitz" (the Blitz reference
    /// pipeline via the pinned rev, for comparison renders).
    #[serde(default)]
    pub engine: Option<String>,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation.
    #[serde(default)]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

#[cfg(feature = "screenshot")]
fn default_screenshot_width() -> u32 { 1280 }
#[cfg(feature = "screenshot")]
fn default_screenshot_height() -> u32 { 800 }
#[cfg(feature = "screenshot")]
fn default_screenshot_scale() -> f32 { 1.0 }
#[cfg(feature = "screenshot")]
fn default_screenshot_full_page() -> bool { true }

/// /screenshot response: PNG encoded as base64 (so it rides in the existing
/// JSON API; clients `base64 -d` or `<img src="data:image/png;base64,...">`).
#[cfg(feature = "screenshot")]
#[derive(Debug, Serialize)]
pub struct ScreenshotResponse {
    pub url: String,
    pub title: Option<String>,
    /// Actual rendered pixel dimensions of the PNG (differs from the request
    /// when `full_page` tracks content height or a `selector` crop is used).
    pub width: u32,
    pub height: u32,
    /// Base64-encoded PNG bytes.
    pub image_base64: String,
    /// Always "png" for now.
    pub format: String,
    /// CSS-pixel rects (page-relative) for the `selector` match(es). Present
    /// only when a selector was given. Single match = the cropped region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_rects: Option<Vec<crate::screenshot::ElementRect>>,
    /// The same rects computed by the diting engine (diting_dom/css/layout)
    /// as an independent pass over the page HTML — the Blitz/Stylo pipeline's
    /// cross-check. Present only when a selector was given and the request
    /// opted in with `diting_rects`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_rects_diting: Option<Vec<crate::screenshot::ElementRect>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SearchRequest {
    pub q: String,
    #[serde(default)]
    pub fetch_top: usize,
    #[serde(default = "default_categories")]
    pub categories: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    #[serde(default = "default_max_chars_per")]
    pub max_chars_per: usize,
    #[serde(default = "default_wait_secs_search")]
    pub wait_secs: u64,
    #[serde(default)]
    pub use_proxy: bool,
    /// Restrict search to these engine names (e.g. ["baidu"]). Empty = all eligible engines.
    #[serde(default)]
    pub engines: Vec<String>,
}

fn default_categories() -> String {
    "general".into()
}
fn default_language() -> String {
    "zh-CN".into()
}
fn default_max_results() -> usize {
    10
}
fn default_max_chars_per() -> usize {
    4000
}
fn default_wait_secs_search() -> u64 {
    3
}

#[derive(Debug, Serialize, Clone)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engines: Vec<String>,
    pub score: f64,
    /// 正文（仅 index < fetch_top 才有值，否则 None）
    pub content: Option<String>,
    pub content_truncated: bool,
    pub fetch_error: Option<String>,
    /// Cookies needed to fetch this URL (e.g. sogou session for /link redirect).
    /// Not serialized in API response — only used internally during fetch.
    #[serde(skip)]
    pub cookies: Vec<String>,
    /// Result of evaluating `js_extract` expression for this result's page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub js_extract_result: Option<serde_json::Value>,
    /// 图片直链（二进制，curl -o 可直接下成 jpg/png）。仅 `images` 分类结果有值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// 图片所在网页 URL（溯源/版权）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub results: Vec<SearchResultItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captcha_events: Vec<crate::captcha::CaptchaEvent>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub enum AppError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    TooManyRequests(String),
    BadGateway(String),
    GatewayTimeout(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg),
            AppError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
            AppError::GatewayTimeout(msg) => (StatusCode::GATEWAY_TIMEOUT, msg),
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(ErrorResponse { error: message })).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        let e = err.into();
        let msg = e.to_string();
        if msg.starts_with("rate limit:") {
            // Stance gate (crate::rate) must surface as 429, not the generic
            // catch-all — the status IS part of the message.
            AppError::TooManyRequests(msg)
        } else if msg.contains("timeout") || msg.contains("timed out") {
            AppError::GatewayTimeout(msg)
        } else if msg.contains("resolve") || msg.contains("connect") || msg.contains("dns") {
            AppError::BadGateway(msg)
        } else if msg.contains("selector") || msg.contains("parse") {
            AppError::BadRequest(msg)
        } else {
            AppError::Internal(msg)
        }
    }
}

// ---------------------------------------------------------------------------
// Session API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SessionCreateRequest {
    pub url: Option<String>,
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation (`["name=value", ...]`). Lets a
    /// session start already logged-in. Round-trips with
    /// GET /session/:id/cookies.
    #[serde(default)]
    pub cookies: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionCreateResponse {
    pub session_id: String,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionClickRequest {
    pub index: usize,
}

#[derive(Debug, Deserialize)]
pub struct SessionInputRequest {
    pub index: usize,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionScrollRequest {
    #[serde(default = "default_scroll_direction")]
    pub direction: session::ScrollDirection,
    #[serde(default = "default_scroll_amount")]
    pub amount: u32,
}

fn default_scroll_direction() -> session::ScrollDirection {
    session::ScrollDirection::Down
}

fn default_scroll_amount() -> u32 {
    3
}

#[derive(Debug, Deserialize)]
pub struct SessionEvalRequest {
    pub script: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionNavigateRequest {
    pub url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "aginxbrowser=info,diting_browser::page=warn,diting_net::wreq_client=warn,diting::console=error",
                    )
                }),
        )
        .init();

    // CLI subcommands exit before the server boots — doctor especially must
    // not pay the V8 warmup below (self-hosters run it to debug a box that
    // may not even reach the network).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("doctor") {
        std::process::exit(doctor_cli::run().await);
    }

    // Warm up V8 on the main thread before any session/blocking thread creates
    // an isolate: the first isolate's JSDispatchTable init is not safe to race
    // from several threads (upstream obscura #430; construction itself is
    // serialized inside the runtime).
    std::mem::drop(diting_js::runtime::JsRuntime::new());

    // Check if running in MCP mode
    if args.contains(&"--mcp".to_string()) {
        tracing::info!("Starting in MCP mode");
        mcp::run_mcp_stdio().await.map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;
        return Ok(());
    }

    let app = Router::new()
        .route("/", get(status_handler))
        .route("/status", get(status_handler))
        .route("/health", get(health_handler))
        .route("/doctor", get(doctor_handler))
        .route("/fetch", post(fetch_handler))
        .route("/click", post(click_handler))
        .route("/eval", post(eval_handler))
        .route("/search", post(search_handler))
        .route("/download", post(download_handler))
        .route("/v1/scrape", post(firecrawl_compat::scrape_handler))
        .route("/session/create", post(session_create_handler))
        .route("/session/list", get(session_list_handler))
        .route("/session/:id/navigate", post(session_navigate_handler))
        .route("/session/:id/state", post(session_state_handler))
        .route("/session/:id/cookies", get(session_cookies_handler))
        .route("/session/:id/export", get(session_export_handler))
        .route("/session/:id/click", post(session_click_handler))
        .route("/session/:id/input", post(session_input_handler))
        .route("/session/:id/scroll", post(session_scroll_handler))
        .route("/session/:id/eval", post(session_eval_handler))
        .route("/session/:id/close", post(session_close_handler))
        .route("/mcp", get(mcp_handler).post(mcp_handler))
        // CDP bridge — Playwright connectOverCDP / Puppeteer connect surface.
        .route("/json/version", get(diting_cdp::http::json_version))
        .route("/json/version/", get(diting_cdp::http::json_version))
        .route("/json/list", get(diting_cdp::http::json_list))
        .route("/json/list/", get(diting_cdp::http::json_list))
        .route("/devtools/:kind/:id", get(diting_cdp::http::devtools_ws));

    #[cfg(feature = "screenshot")]
    let app = app.route("/screenshot", post(screenshot_handler));

    let bind_addr = std::env::var("AGINXBROWSER_BIND").unwrap_or_else(|_| "0.0.0.0:8089".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("aginxbrowser listening on {}", listener.local_addr()?);

    // Standard proxy env vars do NOT configure this engine (reqwest/wreq's
    // implicit env matcher is pinned off on every engine client); make that
    // visible once at startup instead of letting a shell proxy look
    // "configured" while fetches go direct (obscura#491).
    if config::proxy_from_env().is_none() {
        if let Some(env) = config::standard_proxy_env() {
            tracing::warn!(
                "{env} is set but ignored — set AGINXBROWSER_PROXY to route engine traffic through a proxy"
            );
        }
    }

    // Periodically persist the process-global shared cookie jar (stateless
    // handlers mutate it in place; a crash shouldn't cost returning-client
    // cookies that keep anti-bot CAPTCHA rates down).
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            server::persist_shared_cookies();
        }
    });

    axum::serve(listener, app.with_state(mcp::mcp_http_service())).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    // Cheap liveness check for uptime monitors / load balancers. Includes
    // compiled-in capabilities so an agent can learn what's available from a
    // single cheap call (no network probe - see /doctor for that).
    Json(serde_json::json!({
        "status": "ok",
        "engine": "diting",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "screenshot": cfg!(feature = "screenshot"),
            "stealth": cfg!(feature = "stealth"),
            "captcha_solver": std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        }
    }))
}

/// Process start, for the status page's uptime readout.
static STARTED: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn fmt_uptime(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (days, rest) = (secs / 86400, secs % 86400);
    let (hours, mins) = (rest / 3600, (rest % 3600) / 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) => format!("{hours}h {mins}m"),
        _ => format!("{days}d {hours}h {mins}m"),
    }
}

/// Human-facing status page at `/`. Umbrel (and any self-hoster poking the
/// port) needs a page the browser can open after install; agents keep using
/// /health and /doctor. Everything is server-rendered — no client JS, no
/// external assets, works on an offline LAN box.
async fn status_handler() -> axum::response::Html<String> {
    let (sessions, uptime) = {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        (mgr.session_count(), STARTED.elapsed())
    };
    let version = env!("CARGO_PKG_VERSION");
    let caps = [
        ("screenshot", cfg!(feature = "screenshot")),
        ("stealth", cfg!(feature = "stealth")),
        (
            "captcha-solver",
            std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        ),
    ]
    .map(|(name, on)| {
        if on {
            format!(r#"<span class="cap on">{name}</span>"#)
        } else {
            format!(r#"<span class="cap">{name}</span>"#)
        }
    })
    .join("");

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aginxbrowser</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font: 16px/1.6 ui-sans-serif, system-ui, sans-serif; margin: 0;
         display: flex; justify-content: center; min-height: 100vh;
         background: Canvas; color: CanvasText; }}
  main {{ max-width: 40rem; padding: 3rem 1.5rem 4rem; }}
  h1 {{ font-size: 1.4rem; margin: 0 0 .25rem; }}
  p.tag {{ margin: 0 0 2rem; opacity: .7; }}
  .ok {{ color: #1a7f37; }}
  dl {{ display: grid; grid-template-columns: max-content 1fr; gap: .4rem 1.5rem;
        margin: 0 0 2rem; }}
  dt {{ opacity: .7; }}
  dd {{ margin: 0; font-variant-numeric: tabular-nums; }}
  .cap {{ display: inline-block; border: 1px solid; border-radius: 1em;
          padding: .05rem .7rem; margin: 0 .35rem .35rem 0; opacity: .55; }}
  .cap.on {{ opacity: 1; border-color: #1a7f37; color: #1a7f37; }}
  table {{ border-collapse: collapse; width: 100%; font: .9rem/1.5 ui-monospace, monospace; }}
  td {{ padding: .3rem .6rem .3rem 0; vertical-align: top; }}
  td:first-child {{ opacity: .7; white-space: nowrap; }}
  footer {{ margin-top: 2.5rem; font-size: .85rem; opacity: .7; }}
  a {{ color: inherit; }}
</style>
</head>
<body>
<main>
  <h1>aginxbrowser <small style="font-weight:400">v{version}</small></h1>
  <p class="tag">server-side browser for AI agents &mdash; one Rust binary, no Chromium</p>

  <dl>
    <dt>status</dt><dd><span class="ok">&#9679; running</span></dd>
    <dt>uptime</dt><dd>{}</dd>
    <dt>active sessions</dt><dd>{sessions}</dd>
    <dt>engine</dt><dd>diting</dd>
  </dl>

  <p style="margin-bottom:.5rem">capabilities</p>
  <p style="margin:0 0 2rem">{caps}</p>

  <table>
    <tr><td>GET&nbsp;&nbsp;/health</td><td>liveness + capabilities (JSON)</td></tr>
    <tr><td>GET&nbsp;&nbsp;/doctor</td><td>deep self-report, <code>?probe=true</code> for a live fetch</td></tr>
    <tr><td>POST&nbsp;/fetch</td><td>fetch a URL, render JS, return markdown/HTML</td></tr>
    <tr><td>POST&nbsp;/search</td><td>multi-engine meta-search</td></tr>
    <tr><td>POST&nbsp;/screenshot</td><td>render a page to PNG (CPU)</td></tr>
    <tr><td>POST&nbsp;/download</td><td>streaming file download</td></tr>
    <tr><td>GET&nbsp;&nbsp;/mcp</td><td>MCP endpoint (streamable HTTP)</td></tr>
  </table>

  <footer>
    <a href="https://github.com/yinnho/aginxbrowser">github.com/yinnho/aginxbrowser</a>
    &middot; <a href="https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md">API reference</a>
    &middot; Apache-2.0
  </footer>
</main>
</body>
</html>"#,
        fmt_uptime(uptime)
    );

    axum::response::Html(html)
}

/// Query params for /doctor.
#[derive(Deserialize)]
struct DoctorParams {
    /// `?probe=true` runs a live micro-fetch (proves the fetch pipeline +
    /// network egress actually work, not just that the binary is up). Off by
    /// default - a probe spins up a browser and hits the network, so it's
    /// opt-in (borrowing agent-reach's lesson: "shutil.which() alone is NOT
    /// proof of health - really execute a lightweight command").
    #[serde(default)]
    probe: Option<bool>,
}

/// Deep capability self-report + optional live probe. Agents should call this
/// (not /health) when they want to know which features are usable before
/// relying on them.
async fn doctor_handler(Query(params): Query<DoctorParams>) -> impl IntoResponse {
    let capabilities = serde_json::json!({
        "screenshot": cfg!(feature = "screenshot"),
        "stealth": cfg!(feature = "stealth"),
        "captcha_solver": std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        // The product stance, visible where agents and operators look first.
        "robots_honored": std::env::var("AGINXBROWSER_IGNORE_ROBOTS")
            .map(|v| !(v == "1" || v.eq_ignore_ascii_case("true")))
            .unwrap_or(true),
    });

    let probe = if params.probe.unwrap_or(false) {
        let probe_url = std::env::var("AGINXBROWSER_DOCTOR_URL")
            .unwrap_or_else(|_| "https://example.com".to_string());
        let req = FetchRequest {
            url: probe_url.clone(),
            format: OutputFormat::Markdown,
            selector: None,
            wait_secs: None,
            use_proxy: false,
            cookies: vec![],
            max_chars: 500,
            auto_bypass_challenge: true,
            render_tier: RenderTier::Auto,
            tls_fingerprint: None,
            js_extract: None,
        };
        // do_fetch drives the real fetch pipeline (build_browser -> goto ->
        // extract) on a local runtime; spawn_blocking because it is !Send and
        // cannot run inside the tokio runtime - same pattern as the MCP tools.
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || do_fetch(req)).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(Ok(resp)) => serde_json::json!({
                "url": probe_url,
                "ok": true,
                "latency_ms": latency_ms,
                "title": resp.title,
                "content_chars": resp.content.chars().count(),
            }),
            Ok(Err(e)) => serde_json::json!({
                "url": probe_url,
                "ok": false,
                "latency_ms": latency_ms,
                "error": format!("{:?}", e),
            }),
            Err(e) => serde_json::json!({
                "url": probe_url,
                "ok": false,
                "latency_ms": latency_ms,
                "error": format!("task panicked: {}", e),
            }),
        }
    } else {
        serde_json::Value::Null
    };

    Json(serde_json::json!({
        "status": "ok",
        "engine": "diting",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": capabilities,
        "search_engines": [
            "baidu", "bing", "sogou", "sogou_wechat", "duckduckgo",
            "stackexchange", "github", "arxiv",
            "bing_news", "huggingface", "npm", "pypi",
            "baidu_images", "bing_images"
        ],
        "endpoints": [
            "/health", "/doctor", "/fetch", "/click", "/eval", "/search",
            "/download", "/v1/scrape", "/session/create", "/session/list", "/mcp"
        ],
        "probe": probe,
    }))
}

async fn mcp_handler(
    State(service): State<StreamableHttpService<mcp::AginxBrowserMcp, LocalSessionManager>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = service.handle(req).await.into_parts();
    // rmcp's body error is Infallible (never produced); coerce to an Error type.
    use http_body_util::BodyExt;
    let body = axum::body::Body::new(body.map_err(|never| -> std::io::Error { match never {} }));
    Response::from_parts(parts, body)
}

async fn fetch_handler(Json(req): Json<FetchRequest>) -> Result<impl IntoResponse, AppError> {
    // robots.txt gate before the cache — a policy flip applies to cached
    // content too, and the robots policy itself is host-cached so this is
    // cheap on the hot path.
    robots::assert_allowed(&req.url).await.map_err(AppError::Forbidden)?;
    // Short-lived in-process cache. Each /fetch spins up a fresh V8 browser
    // (expensive), so repeated grabs of the same URL in one session benefit a
    // lot. Keyed by everything that affects the result (url/format/selector/
    // cookies/use_proxy/max_chars). TTL via AGINXBROWSER_CACHE_TTL_SECS
    // (default 600s; 0 disables).
    let cache_key = fetch_cache_key(&req);
    if let Some(cached) = fetch_cache_get(&cache_key) {
        return Ok((StatusCode::OK, Json(cached)));
    }

    let resp = smart_fetch(req).await?;
    fetch_cache_put(&cache_key, &resp);
    Ok((StatusCode::OK, Json(resp)))
}

/// Cache key: the request fields that change the response.
fn fetch_cache_key(req: &FetchRequest) -> String {
    format!(
        "{}|{:?}|{:?}|{}|{:?}|{}|{}|{}|{:?}|{:?}",
        req.url, req.format, req.selector, req.use_proxy, req.cookies, req.max_chars,
        req.wait_secs.unwrap_or(0), req.auto_bypass_challenge, req.render_tier,
        req.tls_fingerprint,
    )
}

type FetchCache = std::sync::Mutex<HashMap<String, (u64, FetchResponse)>>;

static FETCH_CACHE: std::sync::LazyLock<FetchCache> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(HashMap::new())
});

/// Max entries before triggering eviction.
const CACHE_CAPACITY: usize = 256;

/// Lazy-initialized TTL read from env (parsed once, then cached).
fn cache_ttl_secs() -> u64 {
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("AGINXBROWSER_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600)
    })
}

fn fetch_cache_get(key: &str) -> Option<FetchResponse> {
    let ttl = cache_ttl_secs();
    if ttl == 0 {
        return None;
    }
    let now = now_secs();
    let Ok(mut cache) = FETCH_CACHE.lock() else {
        return None;
    };
    let Some((ts, resp)) = cache.get(key) else {
        return None;
    };
    if now.saturating_sub(*ts) < ttl {
        Some(resp.clone())
    } else {
        // Lazily remove expired entry on miss (avoids stale buildup).
        cache.remove(key);
        None
    }
}

fn fetch_cache_put(key: &str, resp: &FetchResponse) {
    let ttl = cache_ttl_secs();
    if ttl == 0 {
        return;
    }
    if let Ok(mut cache) = FETCH_CACHE.lock() {
        // Evict when over capacity.
        if cache.len() >= CACHE_CAPACITY {
            let now = now_secs();
            // First pass: drop expired entries.
            cache.retain(|_, (ts, _)| now.saturating_sub(*ts) < ttl);
            // Second pass: if still over capacity, evict oldest entries one-by-one
            // until we're under the limit. This preserves recent/hot entries better
            // than the old "keep newest half" approach.
            while cache.len() >= CACHE_CAPACITY {
                if let Some(oldest) = cache
                    .iter()
                    .filter(|(_, (ts, _))| now.saturating_sub(*ts) >= ttl)
                    .map(|(k, _)| k.clone())
                    .next()
                {
                    cache.remove(&oldest);
                } else {
                    // All entries are within TTL; evict the single oldest.
                    let oldest = cache
                        .iter()
                        .min_by_key(|(_, (ts, _))| *ts)
                        .map(|(k, _)| k.clone());
                    if let Some(k) = oldest {
                        cache.remove(&k);
                    } else {
                        break;
                    }
                }
            }
        }
        cache.insert(key.to_string(), (now_secs(), resp.clone()));
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn click_handler(Json(req): Json<ClickRequest>) -> Result<impl IntoResponse, AppError> {
    // /click fetches the URL autonomously before acting on it — same robots
    // gate as /fetch (see robots.rs for the contract).
    robots::assert_allowed(&req.url).await.map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || do_click(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

async fn eval_handler(Json(req): Json<EvalRequest>) -> Result<impl IntoResponse, AppError> {
    // /eval fetches the URL autonomously to run the script on it — same
    // robots gate as /fetch (see robots.rs for the contract).
    robots::assert_allowed(&req.url).await.map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || do_eval(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

#[cfg(feature = "screenshot")]
async fn screenshot_handler(Json(req): Json<ScreenshotRequest>) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url).await.map_err(AppError::Forbidden)?;
    // V8 (deno_core) holds !Send state, so drive the whole capture on a
    // current-thread runtime on a blocking thread — same pattern as do_eval.
    let resp = spawn_blocking(move || server::do_screenshot(req)).await??;
    Ok((StatusCode::OK, Json(resp)))
}

async fn search_handler(Json(req): Json<SearchRequest>) -> Result<impl IntoResponse, AppError> {
    let resp = do_search(req).await.map_err(|e| match e {
        SearchError::Other(msg) => AppError::Internal(msg),
    })?;
    Ok((StatusCode::OK, Json(resp)))
}

async fn download_handler(Json(req): Json<download::DownloadRequest>) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url).await.map_err(AppError::Forbidden)?;
    let resp = download::do_download(req).await?;
    server::persist_shared_cookies();
    Ok((StatusCode::OK, Json(resp)))
}

// ---------------------------------------------------------------------------
// Session handlers
// ---------------------------------------------------------------------------

async fn session_create_handler(Json(req): Json<SessionCreateRequest>) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    mgr.evict_expired();
    let id = mgr.create(req.url.as_deref(), req.use_proxy, req.cookies);
    Ok((StatusCode::OK, Json(SessionCreateResponse {
        session_id: id,
        url: req.url,
    })))
}

/// Live sessions with idle age and time left before auto-eviction — the
/// discovery twin of /session/create (reuse instead of spawning a fresh V8
/// thread per step).
async fn session_list_handler() -> impl IntoResponse {
    let mut mgr = session::SESSIONS.lock().await;
    mgr.evict_expired();
    let sessions = mgr.list();
    axum::Json(serde_json::json!({ "count": sessions.len(), "sessions": sessions }))
}

async fn session_navigate_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionNavigateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr.send(&id, |reply| session::SessionCommand::Navigate {
        url: req.url.clone(),
        reply,
    }).await.map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

/// Session-command errors: the stance gate (rate.rs) must surface as 429,
/// not the generic 500 the session handlers default to.
fn session_err(e: String) -> AppError {
    if e.starts_with("rate limit:") {
        AppError::TooManyRequests(e)
    } else {
        AppError::Internal(e)
    }
}

async fn session_state_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let compact_text = mgr.send(&id, |reply| session::SessionCommand::State { reply }).await
        .map_err(|e| AppError::Internal(e))?;
    // Return as plain text for token efficiency.
    Ok((StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], compact_text))
}

async fn session_cookies_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr.send(&id, |reply| session::SessionCommand::Cookies { reply }).await
        .map_err(|e| AppError::Internal(e))?;
    // `text` is a JSON string {"url":...,"cookies":[...]} from the session
    // thread; parse it back so we emit a real JSON response.
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("cookies parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

#[derive(Deserialize)]
struct SessionExportQuery {
    /// `bash` (default) emits a runnable curl replay script;
    /// `jsonl` emits the raw action log.
    #[serde(default)]
    format: Option<String>,
}

/// Export the session's recorded actions: as a bash+curl replay script
/// (default — replay with zero model tokens) or as raw JSONL.
async fn session_export_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<SessionExportQuery>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let jsonl = mgr.send(&id, |reply| session::SessionCommand::Export { reply }).await
        .map_err(AppError::Internal)?;
    if q.format.as_deref() == Some("jsonl") {
        Ok((StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")], jsonl))
    } else {
        let script = session::replay_bash(&jsonl, "http://127.0.0.1:8089");
        Ok((StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script))
    }
}

async fn session_click_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionClickRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr.send(&id, |reply| session::SessionCommand::Click {
        index: req.index,
        reply,
    }).await.map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

async fn session_input_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionInputRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let filled = mgr.send(&id, |reply| session::SessionCommand::Input {
        index: req.index,
        text: req.text,
        reply,
    }).await.map_err(|e| AppError::Internal(e))?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "filled": filled }))))
}

async fn session_scroll_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionScrollRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let scrolled = mgr.send(&id, |reply| session::SessionCommand::Scroll {
        direction: req.direction,
        amount: req.amount,
        reply,
    }).await.map_err(|e| AppError::Internal(e))?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "scrolled": scrolled }))))
}

async fn session_eval_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionEvalRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let result = mgr.send(&id, |reply| session::SessionCommand::Eval {
        script: req.script,
        reply,
    }).await.map_err(|e| AppError::Internal(e))?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "result": result }))))
}

async fn session_close_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    // Wait for the session thread's ack so `ok` is truthful - a runaway eval
    // can pin the thread inside V8 for up to its watchdog budget.
    let closed = mgr.close_and_wait(&id).await;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "ok": closed })),
    ))
}

fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str) -> FetchRequest {
        FetchRequest {
            url: url.into(),
            format: OutputFormat::Markdown,
            selector: None,
            wait_secs: None,
            use_proxy: false,
            cookies: vec![],
            max_chars: 50000,
            auto_bypass_challenge: true,
            render_tier: RenderTier::Auto,
            tls_fingerprint: None,
            js_extract: None,
        }
    }

    fn resp(url: &str) -> FetchResponse {
        FetchResponse {
            url: url.into(),
            title: Some("t".into()),
            content: "c".into(),
            truncated: false,
            captcha_event: None,
            js_extract_result: None,
            tier: None,
        }
    }

    #[test]
    fn cache_key_distinguishes_fields() {
        let a = req("https://e.com");
        let mut b = req("https://e.com");
        // Same → same key.
        assert_eq!(fetch_cache_key(&a), fetch_cache_key(&b));

        // Different url → different key.
        b.url = "https://other.com".into();
        assert_ne!(fetch_cache_key(&a), fetch_cache_key(&b));

        // Different max_chars → different key.
        b = req("https://e.com");
        b.max_chars = 100;
        assert_ne!(fetch_cache_key(&a), fetch_cache_key(&b));

        // Different render_tier → different key.
        b = req("https://e.com");
        b.render_tier = RenderTier::Http;
        assert_ne!(fetch_cache_key(&a), fetch_cache_key(&b));

        // Different use_proxy → different key.
        b = req("https://e.com");
        b.use_proxy = true;
        assert_ne!(fetch_cache_key(&a), fetch_cache_key(&b));

        // Different tls_fingerprint → different key.
        b = req("https://e.com");
        b.tls_fingerprint = Some("firefox133".into());
        assert_ne!(fetch_cache_key(&a), fetch_cache_key(&b));
    }

    #[test]
    fn cache_put_then_get_hits() {
        let key = format!("test_put_get:{}", now_secs());
        fetch_cache_put(&key, &resp("https://e.com"));
        let got = fetch_cache_get(&key);
        assert!(got.is_some());
        assert_eq!(got.unwrap().url, "https://e.com");
    }

    #[test]
    fn cache_get_miss_for_unknown_key() {
        let key = format!("test_miss:{}:{}", now_secs(), std::process::id());
        assert!(fetch_cache_get(&key).is_none());
    }

    #[test]
    fn cache_evicts_oldest_when_over_capacity() {
        // Clear the shared global cache so other tests' entries don't interfere.
        if let Ok(mut cache) = FETCH_CACHE.lock() {
            cache.clear();
        }
        // Insert well over CACHE_CAPACITY entries.
        let base = now_secs();
        for i in 0..CACHE_CAPACITY + 10 {
            let key = format!("test_evict:{i}:{base}");
            fetch_cache_put(&key, &resp(&format!("https://e.com/{i}")));
        }
        // The cache should stay at or below CACHE_CAPACITY (not grow unbounded).
        if let Ok(cache) = FETCH_CACHE.lock() {
            assert!(
                cache.len() <= CACHE_CAPACITY,
                "cache grew to {} entries (capacity {})",
                cache.len(),
                CACHE_CAPACITY,
            );
        }
    }

    #[test]
    fn cache_expired_entry_removed_on_get() {
        // Insert with a timestamp far in the past to simulate expiry.
        let key = format!("test_expired:{}", now_secs());
        if let Ok(mut cache) = FETCH_CACHE.lock() {
            cache.insert(key.clone(), (0, resp("https://expired.com")));
        }
        // get should return None and remove the stale entry.
        assert!(fetch_cache_get(&key).is_none());
        // Confirm it was actually removed from the map.
        if let Ok(cache) = FETCH_CACHE.lock() {
            assert!(!cache.contains_key(&key));
        }
    }
}
