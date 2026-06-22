use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::server::{do_click, do_eval, do_fetch, do_search};
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
        description = "Fetch a webpage and return its content. Supports JS rendering, stealth mode, and multiple output formats (markdown/html/text).",
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
        description = "Search the web using multiple engines (Baidu, Bing, Sogou, WeChat, Google) and optionally fetch page content for top results.",
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
