//! Stateless acquisition routes (ARCHITECTURE.md P2): fetch/click/eval/
//! search/download plus the engine-vocabulary /engines endpoint and the
//! /fetch response cache. Split from the crate root; behavior unchanged.
use std::collections::HashMap;

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::render::smart_fetch;
use crate::robots;
use crate::server::{self, do_click, do_eval, do_search, SearchError};
use crate::store;
use crate::{cache_ttl_secs, download, now_secs, spawn_blocking, AppError};

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
    /// Cookies to inject before navigation. Entries are `"name=value"`
    /// strings or CDP-style objects `{"name","value","domain",...}` —
    /// browser-exported login state arrives in the object shape.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
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
    /// back to the JS browser only if the page needs rendering. `http`: pure
    /// HTTP (fastest, no JS; errors instead of upgrading). `obscura`: always
    /// use the full browser.
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
    /// Strip prompt-injection carriers from `content` before it reaches the
    /// caller (zero-width characters, human-invisible text spans,
    /// instruction-shaped lines). Default true; set false when studying
    /// injection content itself. See the `sanitize_report` response field.
    #[serde(default = "default_true")]
    pub sanitize: bool,
    /// Return the bodies of background XHR/fetch requests the page issued,
    /// as the `xhr` response array. Each entry is a URL substring an entry
    /// must contain; an empty array matches every XHR/fetch. Forces the
    /// browser tier (script-initiated requests only exist post-JS).
    #[serde(default)]
    pub capture_xhr: Option<Vec<String>>,
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
    /// Wire name is "browser". "obscura" is still accepted and not advertised.
    #[serde(alias = "obscura")]
    Browser,
}

fn default_max_chars() -> usize {
    50_000
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default, Clone, PartialEq)]
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
    /// Cookies to inject before navigation. Entries are `"name=value"`
    /// strings or CDP-style objects `{"name","value","domain",...}`.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
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
    /// Cookies to inject before navigation. Entries are `"name=value"`
    /// strings or CDP-style objects `{"name","value","domain",...}`.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
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
    /// URLs that each issued a redirect before the response landed at
    /// `url` — so `redirected_from.first().unwrap_or(&url)` is the requested
    /// URL and `url` the effective one. Always empty on the browser tier.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub redirected_from: Vec<String>,
    /// What the injection stripper removed — present only when sanitization
    /// was on and actually stripped something (observable, never silent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sanitize_report: Option<crate::sanitize::SanitizeReport>,
    /// Background XHR/fetch responses captured for `capture_xhr` requests:
    /// `[{url, method, status, mime, body, body_truncated}]` — the page's
    /// own API face, usually cleaner than the rendered DOM.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub xhr: Vec<serde_json::Value>,
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
#[serde(deny_unknown_fields)]
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
    /// Unknown names are a 400 carrying the valid list — GET /engines discovers them.
    #[serde(default)]
    pub engines: Vec<String>,
    /// Freshness window: "day" | "week" | "month" | "year". Currently honored
    /// by bing_news (items are filtered by their pubDate); engines without a
    /// server-side filter ignore it. Invalid values are a 400.
    #[serde(default)]
    pub time_range: Option<String>,
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

#[derive(Debug, Serialize, Clone)]
pub struct SearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub results: Vec<SearchResultItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captcha_events: Vec<crate::captcha::CaptchaEvent>,
    /// Why an engine contributed nothing: CAPTCHA suspension (with resume
    /// countdown), transient fetch/parse failure, or a panicked task. Absent
    /// when every eligible engine answered — a zero-result response with an
    /// empty map is then genuinely "no hits", not a swallowed failure
    /// (v0.3.1 Windows report P1-1).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engine_errors: std::collections::BTreeMap<String, String>,
    /// Engines that served in place of an explicitly requested set where
    /// EVERY named engine errored (CAPTCHA wall / transient failure): the
    /// same query re-ran over the rest of the category's engines so a
    /// site-specific wall doesn't leave the agent empty-handed, and the
    /// substitution is disclosed here — never silent (v0.3.2 Windows
    /// report #9). Absent when no fallback ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_engines: Option<Vec<String>>,
}

/// Engine health rows shared by /doctor and /engines: name, categories
/// served, live suspension state and stacked CAPTCHA count.
pub(crate) async fn search_engine_rows() -> Vec<serde_json::Value> {
    server::search_engine_health()
        .await
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "categories": e.categories,
                "suspended": e.suspended,
                "suspend_remaining_secs": if e.suspended { Some(e.suspend_remaining_secs) } else { None },
                "captcha_count": e.captcha_count,
            })
        })
        .collect()
}

/// GET /engines: the search-engine vocabulary for /search's `engines` filter
/// plus live health — so an agent discovers valid names AND who is currently
/// benched by a CAPTCHA in one call (v0.3.1 Windows report P1-1: the
/// alternative was guessing names and reading zero results as "no hits").
pub(crate) async fn engines_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "engines": search_engine_rows().await }))
}

pub(crate) async fn fetch_handler(Json(req): Json<FetchRequest>) -> Result<impl IntoResponse, AppError> {
    // robots.txt gate before the cache — a policy flip applies to cached
    // content too, and the robots policy itself is host-cached so this is
    // cheap on the hot path.
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
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
    store::record_fetch(store::REST_OWNER, &resp);
    Ok((StatusCode::OK, Json(resp)))
}

/// Cache key: the request fields that change the response.
fn fetch_cache_key(req: &FetchRequest) -> String {
    format!(
        "{}|{:?}|{:?}|{}|{:?}|{}|{}|{}|{:?}|{:?}|{}|{:?}",
        req.url,
        req.format,
        req.selector,
        req.use_proxy,
        req.cookies,
        req.max_chars,
        req.wait_secs.unwrap_or(0),
        req.auto_bypass_challenge,
        req.render_tier,
        req.tls_fingerprint,
        req.sanitize,
        req.capture_xhr,
    )
}

type FetchCache = std::sync::Mutex<HashMap<String, (u64, FetchResponse)>>;

static FETCH_CACHE: std::sync::LazyLock<FetchCache> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Max entries before triggering eviction.
const CACHE_CAPACITY: usize = 256;

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

pub(crate) async fn click_handler(Json(req): Json<ClickRequest>) -> Result<impl IntoResponse, AppError> {
    // /click fetches the URL autonomously before acting on it — same robots
    // gate as /fetch (see robots.rs for the contract).
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || do_click(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

pub(crate) async fn eval_handler(Json(req): Json<EvalRequest>) -> Result<impl IntoResponse, AppError> {
    // /eval fetches the URL autonomously to run the script on it — same
    // robots gate as /fetch (see robots.rs for the contract).
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || do_eval(req)).await?;
    Ok((StatusCode::OK, Json(resp?)))
}

pub(crate) async fn search_handler(Json(req): Json<SearchRequest>) -> Result<impl IntoResponse, AppError> {
    let categories = req.categories.clone();
    let resp = do_search(req).await.map_err(|e| match e {
        SearchError::Other(msg) => AppError::Internal(msg),
        SearchError::BadRequest(msg) => AppError::BadRequest(msg),
    })?;
    store::record_search(store::REST_OWNER, &resp.query, &categories, &resp);
    Ok((StatusCode::OK, Json(resp)))
}

pub(crate) async fn download_handler(
    Json(req): Json<download::DownloadRequest>,
) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    let resp = download::do_download(req).await?;
    server::persist_shared_cookies();
    Ok((StatusCode::OK, Json(resp)))
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
            sanitize: true,
            capture_xhr: None,
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
            redirected_from: Vec::new(),
            sanitize_report: None,
            xhr: Vec::new(),
        }
    }

    #[test]
    fn fetch_request_accepts_string_and_object_cookie_entries() {
        let r: FetchRequest = serde_json::from_str(
            r#"{"url":"https://shop.miceal.taobao.com/","cookies":[
                "bare=1",
                {"name":"cookie1","value":"t","domain":".taobao.com",
                 "path":"/","secure":true,"httpOnly":true,"sameSite":"None"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(r.cookies[0], "bare=1");
        assert_eq!(
            r.cookies[1],
            "cookie1=t; Domain=.taobao.com; Path=/; SameSite=None; Secure; HttpOnly"
        );
    }

    #[test]
    fn fetch_request_rejects_cookie_objects_without_value() {
        let err = serde_json::from_str::<FetchRequest>(
            r#"{"url":"https://e.com/","cookies":[{"name":"x"}]}"#,
        );
        assert!(err.is_err());
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

    // v0.3.1 Windows report P2-4: guessed parameter names (count/limit/num)
    // were silently ignored by serde, so the response read as "max_results
    // is ignored" when the caller's cap never landed. Unknown fields now
    // 400 at the door instead of being swallowed.
    #[test]
    fn search_request_rejects_guessed_param_names() {
        for bad in [
            r#"{"q":"x","count":5}"#,
            r#"{"q":"x","limit":5}"#,
            r#"{"q":"x","num":5}"#,
        ] {
            assert!(
                serde_json::from_str::<SearchRequest>(bad).is_err(),
                "guessed param must be rejected loudly: {bad}"
            );
        }
    }

    #[test]
    fn search_request_accepts_engines_and_time_range() {
        let r: SearchRequest =
            serde_json::from_str(r#"{"q":"x","engines":["baidu"],"time_range":"day"}"#).unwrap();
        assert_eq!(r.engines, vec!["baidu"]);
        assert_eq!(r.time_range.as_deref(), Some("day"));
    }

    // "browser" is the name callers should send. "obscura" stays accepted.
    #[test]
    fn render_tier_browser_is_canonical_and_obscura_still_parses() {
        let browser: FetchRequest =
            serde_json::from_str(r#"{"url":"https://e.com","render_tier":"browser"}"#).unwrap();
        let legacy: FetchRequest =
            serde_json::from_str(r#"{"url":"https://e.com","render_tier":"obscura"}"#).unwrap();
        assert_eq!(browser.render_tier, RenderTier::Browser);
        assert_eq!(legacy.render_tier, RenderTier::Browser);
        assert_eq!(
            serde_json::to_value(RenderTier::Browser).unwrap(),
            serde_json::json!("browser")
        );
    }
}
