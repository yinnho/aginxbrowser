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
use serde_json::json;

use crate::server::{do_click, do_eval, do_fetch, do_search};
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
pub struct SessionCloseParams {
    /// Session ID
    pub session_id: String,
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
pub struct AginxBrowserMcp;

#[tool_router]
impl AginxBrowserMcp {
    #[tool(
        description = "Fetch a webpage and return clean markdown/html/text. Use whenever the agent needs to READ any web page - blogs, docs, articles, JS-rendered SPAs, Cloudflare-protected sites. Supports JS rendering, stealth TLS fingerprints, and structured-data extraction via js_extract.",
        annotations(title = "Fetch Webpage", read_only_hint = true)
    )]
    async fn fetch(&self, Parameters(params): Parameters<FetchParams>) -> String {
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

        match tokio::task::spawn_blocking(move || do_fetch(req)).await {
            Ok(Ok(resp)) => json!({
                "url": resp.url,
                "title": resp.title,
                "content": resp.content,
                "truncated": resp.truncated
            })
            .to_string(),
            Ok(Err(e)) => json!({ "error": format!("{}", e) }).to_string(),
            Err(e) => json!({ "error": format!("task panicked: {}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Execute JavaScript on a webpage and return the result. Supports async/Promise.",
        annotations(title = "Evaluate JavaScript")
    )]
    async fn eval(&self, Parameters(params): Parameters<EvalParams>) -> String {
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
        let req = SearchRequest {
            q: params.q,
            fetch_top: params.fetch_top,
            categories: params.categories,
            language: "zh-CN".to_string(),
            max_results: params.max_results,
            max_chars_per: params.max_chars_per,
            wait_secs: 3,
            use_proxy: false,
        };

        // do_search is already async and uses spawn_blocking internally for
        // the fetch_top body-grabbing, so it's safe to call directly.
        match do_search(req).await {
            Ok(resp) => json!({
                "query": resp.query,
                "number_of_results": resp.number_of_results,
                "results": resp.results
            })
            .to_string(),
            Err(e) => json!({ "error": format!("{:?}", e) }).to_string(),
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
        let id = mgr.create(params.url.as_deref(), params.use_proxy, params.cookies);
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
        description = "Click an interactive element by its index (from session_state output).",
        annotations(title = "Session Click")
    )]
    async fn session_click(&self, Parameters(params): Parameters<SessionClickParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Click {
            index: params.index,
            reply,
        }).await {
            Ok(resp) => json!({ "url": resp.url, "clicked": resp.clicked }).to_string(),
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
    AginxBrowserMcp
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
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(vec![
        "browser.aginx.net".to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ]);
    StreamableHttpService::new(
        || Ok(AginxBrowserMcp),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
