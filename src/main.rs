use axum::{
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

mod browser;
mod config;
mod cookie;
mod error;
mod firecrawl_compat;
mod mcp;
mod page;
mod render;
mod search;
mod server;

// Inlined Obscura engine (formerly external crates).
mod obscura_dom;
mod obscura_net;
mod obscura_js;
mod obscura_browser;

use server::{do_click, do_eval, do_search, SearchError};
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
    /// Route through OBSCURA_PROXY. Default false (direct) — set true for
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
    /// HTTP-only (fastest, no JS). `obscura`: always use the full browser.
    #[serde(default)]
    pub render_tier: RenderTier,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133",
    /// "safari17_5", "edge145", etc. None → Chrome145 default.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

/// Tiered rendering strategy selector.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RenderTier {
    /// HTTP-direct first, fall back to obscura browser. (default)
    #[default]
    Auto,
    /// Pure HTTP, no V8/JS. Fastest; misses JS-rendered content.
    Http,
    /// Always use the obscura browser (current behaviour pre-tiering).
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
    /// Route through OBSCURA_PROXY. Default false (direct).
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
    /// Route through OBSCURA_PROXY. Default false (direct).
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
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub results: Vec<SearchResultItem>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub enum AppError {
    BadRequest(String),
    NotFound(String),
    BadGateway(String),
    GatewayTimeout(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
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
        if msg.contains("timeout") || msg.contains("timed out") {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Check if running in MCP mode
    let args: Vec<String> = std::env::args().collect();
    if args.contains(&"--mcp".to_string()) {
        tracing::info!("Starting in MCP mode");
        mcp::run_mcp_stdio().await.map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;
        return Ok(());
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/fetch", post(fetch_handler))
        .route("/click", post(click_handler))
        .route("/eval", post(eval_handler))
        .route("/search", post(search_handler))
        .route("/v1/scrape", post(firecrawl_compat::scrape_handler));

    let bind_addr = std::env::var("AGINXBROWSER_BIND").unwrap_or_else(|_| "0.0.0.0:8089".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("aginxbrowser listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "engine": "obscura" }))
}

async fn fetch_handler(Json(req): Json<FetchRequest>) -> Result<impl IntoResponse, AppError> {
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
    let resp = spawn_blocking(move || do_click(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

async fn eval_handler(Json(req): Json<EvalRequest>) -> Result<impl IntoResponse, AppError> {
    let resp = spawn_blocking(move || do_eval(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

async fn search_handler(Json(req): Json<SearchRequest>) -> Result<impl IntoResponse, AppError> {
    let resp = do_search(req).await.map_err(|e| match e {
        SearchError::Other(msg) => AppError::Internal(msg),
    })?;
    Ok((StatusCode::OK, Json(resp)))
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
        }
    }

    fn resp(url: &str) -> FetchResponse {
        FetchResponse {
            url: url.into(),
            title: Some("t".into()),
            content: "c".into(),
            truncated: false,
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
