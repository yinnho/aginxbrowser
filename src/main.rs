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
mod page;
mod search;
mod server;

// Inlined Obscura engine (formerly external crates).
mod obscura_dom;
mod obscura_net;
mod obscura_js;
mod obscura_browser;

use server::{do_click, do_eval, do_fetch, do_search, SearchError};

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
}

fn default_max_chars() -> usize {
    50_000
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
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub results: Vec<SearchResultItem>,
    pub search_backend: String,
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

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/fetch", post(fetch_handler))
        .route("/click", post(click_handler))
        .route("/eval", post(eval_handler))
        .route("/search", post(search_handler));

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

    let resp = spawn_blocking(move || do_fetch(req)).await??;
    fetch_cache_put(&cache_key, &resp);
    Ok((StatusCode::OK, Json(resp)))
}

/// Cache key: the request fields that change the response. `wait_secs` is
/// intentionally excluded (it only waits for rendering, same final content).
fn fetch_cache_key(req: &FetchRequest) -> String {
    format!(
        "{}|{:?}|{:?}|{}|{:?}|{}",
        req.url, req.format, req.selector, req.use_proxy, req.cookies, req.max_chars,
    )
}

type FetchCache = std::sync::Mutex<HashMap<String, (u64, FetchResponse)>>;

static FETCH_CACHE: std::sync::LazyLock<FetchCache> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(HashMap::new())
});

fn cache_ttl_secs() -> u64 {
    std::env::var("AGINXBROWSER_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
}

fn fetch_cache_get(key: &str) -> Option<FetchResponse> {
    let ttl = cache_ttl_secs();
    if ttl == 0 {
        return None;
    }
    let now = now_secs();
    let cache = FETCH_CACHE.lock().ok()?;
    let (ts, resp) = cache.get(key)?;
    if now.saturating_sub(*ts) < ttl {
        Some(resp.clone())
    } else {
        None
    }
}

fn fetch_cache_put(key: &str, resp: &FetchResponse) {
    if cache_ttl_secs() == 0 {
        return;
    }
    if let Ok(mut cache) = FETCH_CACHE.lock() {
        // Bound the cache to avoid unbounded growth across distinct URLs.
        if cache.len() > 256 {
            cache.clear();
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
        SearchError::BackendUnavailable(msg) => AppError::ServiceUnavailable(msg),
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
