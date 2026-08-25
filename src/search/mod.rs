pub mod baidu;
pub mod baidu_images;
pub mod bing;
pub mod bing_images;
pub mod bing_news;
pub mod github_repos;
pub mod google;
pub mod arxiv;
pub mod huggingface;
pub mod meilisearch;
pub mod npm;
pub mod pypi;
pub mod sogou;
pub mod sogou_wechat;
pub mod stackexchange;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;
use url::Url;

use crate::SearchResultItem;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Parameters passed from the /search API request, adapted for engine use.
pub struct SearchParams {
    pub language: String,
    pub pageno: usize,
    pub use_proxy: bool,
    pub timeout_secs: u64,
    /// Restrict to these engine names; empty = all eligible engines for the category.
    pub engine_filter: Vec<String>,
}

/// Image-specific result fields. Populated only for `images`-category results.
/// Kept in a sub-struct so general-result construction sites only add
/// `image: None` instead of four separate fields.
#[derive(Debug, Clone)]
pub struct ImageResult {
    /// Binary direct link to the image — `curl -o` downloadable as jpg/png.
    /// NOT the URL of the page where the image appears.
    pub image_url: String,
    /// Page where the image appears (attribution / source tracing).
    pub source_url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// A single raw result from one engine, before merging.
#[derive(Debug, Clone)]
pub struct RawSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
    /// Position-based score: N - position (0-indexed).
    pub score: f64,
    /// Cookies needed to fetch this URL (e.g. sogou session cookies for
    /// /link redirect URLs). Passed to the diting browser during fetch.
    pub cookies: Vec<String>,
    /// Reserved structured-extraction payload for this result. No engine
    /// populates it today; kept in the API shape for the extraction tier.
    pub js_extract_result: Option<serde_json::Value>,
    /// Present only for `images`-category results. None for general results.
    pub image: Option<ImageResult>,
}

/// Error from a single engine.
#[derive(Debug)]
pub enum SearchEngineError {
    /// Engine hit a CAPTCHA; should be suspended. Carries metadata for reporting.
    Captcha {
        url: String,
        captcha_type: Option<crate::captcha::CaptchaType>,
    },
    /// Network / parse error; transient, do not suspend.
    Transient(String),
    /// Engine is currently suspended (skipped).
    Suspended,
}

// ---------------------------------------------------------------------------
// SearchEngine trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &str;
    fn categories(&self) -> &[&str];
    /// Minimum CAPTCHA suspension for this engine. Override for engines
    /// whose anti-spider cooldown requires a longer minimum suspension.
    fn base_captcha_suspend(&self) -> Duration {
        Duration::from_secs(300)
    }
    /// Execute a search. The engine must handle its own HTTP client selection
    /// (stealth wreq vs plain reqwest) internally.
    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError>;
}

// ---------------------------------------------------------------------------
// SearchEngineRegistry
// ---------------------------------------------------------------------------

/// Per-engine suspension and backoff state.
struct EngineSuspendState {
    /// Resume time if currently suspended.
    resume_at: Option<std::time::Instant>,
    /// Consecutive CAPTCHA count. Reset on success or expiry.
    captcha_count: u32,
}

pub struct SearchEngineRegistry {
    engines: Vec<Arc<dyn SearchEngine>>,
    /// Per-engine suspension + CAPTCHA backoff state.
    state: Arc<RwLock<HashMap<String, EngineSuspendState>>>,
}

/// Progressive backoff: 1st CAPTCHA → 5 min, 2nd → 10 min, 3rd → 30 min, 4th+ → 1 h.
fn captcha_backoff(count: u32) -> Duration {
    match count {
        0 | 1 => Duration::from_secs(300),
        2 => Duration::from_secs(600),
        3 => Duration::from_secs(1800),
        _ => Duration::from_secs(3600),
    }
}

impl SearchEngineRegistry {
    pub fn new() -> Self {
        let mut registry = SearchEngineRegistry {
            engines: Vec::new(),
            state: Arc::new(RwLock::new(HashMap::new())),
        };

        // Register engines. Each engine internally decides whether to use
        // wreq (stealth) or reqwest (plain), and holds its own client.
        registry.register(baidu::BaiduEngine::new());
        registry.register(baidu_images::BaiduImagesEngine::new());
        registry.register(bing::BingEngine::new());
        registry.register(bing_images::BingImagesEngine::new());
        registry.register(sogou::SogouEngine::new());
        registry.register(sogou_wechat::SogouWechatEngine::new());
        registry.register(google::GoogleEngine::new());
        registry.register(stackexchange::StackExchangeEngine::new());
        registry.register(github_repos::GithubEngine::new());
        registry.register(arxiv::ArxivEngine::new());
        registry.register(bing_news::BingNewsEngine::new());
        registry.register(huggingface::HuggingFaceEngine::new());
        registry.register(npm::NpmEngine::new());
        registry.register(pypi::PypiEngine::new());
        if let Some(meili) = meilisearch::MeilisearchEngine::from_env() {
            tracing::info!("search: registered engine meilisearch (private index)");
            registry.register(meili);
        }

        registry
    }

    fn register(&mut self, engine: impl SearchEngine + 'static) {
        tracing::info!("search: registered engine {}", engine.name());
        self.engines.push(Arc::new(engine));
    }

    /// Check if an engine is currently suspended.
    #[allow(dead_code)]
    async fn is_suspended(&self, name: &str) -> bool {
        let state = self.state.read().await;
        if let Some(s) = state.get(name) {
            if let Some(resume_at) = s.resume_at {
                if std::time::Instant::now() < resume_at {
                    return true;
                }
            }
        }
        false
    }

    /// Suspend an engine after CAPTCHA, using progressive backoff.
    /// Duration is max(backoff_ladder, engine's base_captcha_suspend).
    async fn suspend_engine(&self, name: &str, base_suspend: Duration) {
        let mut state = self.state.write().await;
        let s = state.entry(name.to_string()).or_insert(EngineSuspendState {
            resume_at: None,
            captcha_count: 0,
        });
        s.captcha_count += 1;
        let backoff = captcha_backoff(s.captcha_count);
        let duration = backoff.max(base_suspend);
        let resume_at = std::time::Instant::now() + duration;
        s.resume_at = Some(resume_at);
        tracing::warn!(
            "search: engine {} suspended for {:?} (CAPTCHA #{})",
            name, duration, s.captcha_count
        );
    }

    /// Reset CAPTCHA count for an engine after a successful search.
    async fn reset_captcha_count(&self, name: &str) {
        let mut state = self.state.write().await;
        if let Some(s) = state.get_mut(name) {
            s.captcha_count = 0;
        }
    }

    /// Clean up expired suspensions and reset their backoff counts so
    /// the engine starts fresh after the cooldown period.
    async fn cleanup_suspensions(&self) {
        // Fast path: check under read lock if there's anything to clean up.
        {
            let state = self.state.read().await;
            let now = std::time::Instant::now();
            let needs_cleanup = state.values().any(|s| {
                s.resume_at.is_some_and(|t| t <= now)
            });
            if !needs_cleanup {
                return;
            }
        }
        let mut state = self.state.write().await;
        let now = std::time::Instant::now();
        state.retain(|_, s| {
            if let Some(resume_at) = s.resume_at {
                if resume_at <= now {
                    s.resume_at = None;
                    s.captcha_count = 0;
                }
            }
            s.resume_at.is_some() || s.captcha_count > 0
        });
    }
}

// ---------------------------------------------------------------------------
// Native search: concurrent dispatch + merge/dedup
// ---------------------------------------------------------------------------

/// Execute native search across all eligible engines, merge and dedup results.
pub async fn native_search(
    registry: &SearchEngineRegistry,
    query: &str,
    params: SearchParams,
    categories: &str,
    max_results: usize,
) -> (Vec<SearchResultItem>, usize, Vec<crate::captcha::CaptchaEvent>) {
    registry.cleanup_suspensions().await;

    // Filter engines by category.
    let requested: Vec<&str> = categories.split(',').map(|s| s.trim()).collect();
    let eligible: Vec<Arc<dyn SearchEngine>> = registry
        .engines
        .iter()
        .filter(|e| e.categories().iter().any(|c| requested.contains(c)))
        .filter(|e| params.engine_filter.is_empty() || params.engine_filter.iter().any(|n| n.as_str() == e.name()))
        .cloned() // Arc clone — cheap
        .collect();

    // Dispatch to all eligible engines concurrently.
    let mut handles = Vec::with_capacity(eligible.len());
    for engine in eligible {
        let name = engine.name().to_string();
        let query = query.to_string();
        let params = SearchParams {
            language: params.language.clone(),
            pageno: params.pageno,
            use_proxy: params.use_proxy,
            timeout_secs: params.timeout_secs,
            engine_filter: Vec::new(),
        };

        let state = registry.state.clone();

        handles.push(tokio::spawn(async move {
            // Check suspension inside the task.
            {
                let s = state.read().await;
                if let Some(es) = s.get(&name) {
                    if let Some(resume_at) = es.resume_at {
                        if std::time::Instant::now() < resume_at {
                            return (name, Err(SearchEngineError::Suspended));
                        }
                    }
                }
            }

            let result = engine.search(&query, params).await;
            (name, result)
        }));
    }

    // Collect results.
    let mut all_results: Vec<RawSearchResult> = Vec::new();
    let mut total_count = 0usize;
    let mut captcha_events: Vec<crate::captcha::CaptchaEvent> = Vec::new();

    for handle in handles {
        match handle.await {
            Ok((name, Ok(results))) => {
                total_count += results.len();
                all_results.extend(results);
                registry.reset_captcha_count(&name).await;
            }
            Ok((name, Err(SearchEngineError::Captcha { url, captcha_type }))) => {
                let base_suspend = registry
                    .engines
                    .iter()
                    .find(|e| e.name() == name)
                    .map(|e| e.base_captcha_suspend())
                    .unwrap_or(Duration::from_secs(300));
                registry.suspend_engine(&name, base_suspend).await;
                captcha_events.push(crate::captcha::CaptchaEvent {
                    engine: name,
                    captcha_type: captcha_type.unwrap_or(crate::captcha::CaptchaType::Unknown),
                    url,
                    auto_solve_attempted: false,
                    auto_solve_succeeded: false,
                });
            }
            Ok((name, Err(SearchEngineError::Transient(msg)))) => {
                tracing::warn!("search: engine {} transient error: {}", name, msg);
            }
            Ok((name, Err(SearchEngineError::Suspended))) => {
                tracing::debug!("search: engine {} skipped (suspended)", name);
            }
            Err(e) => {
                tracing::error!("search: engine task panicked: {}", e);
            }
        }
    }

    // Merge and dedup.
    let merged = merge_results(all_results, max_results);
    (merged, total_count, captcha_events)
}

// ---------------------------------------------------------------------------
// Merge and dedup
// ---------------------------------------------------------------------------

/// Normalize a URL for dedup: lowercase scheme+host, strip trailing /, strip
/// common tracking params, strip www. prefix.
fn normalize_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_lowercase();
    };

    // Lowercase scheme and host.
    if let Some(host) = url.host_str() {
        let normalized = host.strip_prefix("www.").unwrap_or(host).to_lowercase();
        // This is a no-op if the host is already the same; we just ensure
        // consistent casing. Url::set_host is unavailable, so we rebuild.
        // Preserve port number if present.
        let port_suffix = url.port().map(|p| format!(":{}", p)).unwrap_or_default();
        if let Ok(new) = Url::parse(&format!(
            "{}://{}{}{}{}{}",
            url.scheme(),
            normalized,
            port_suffix,
            url.path().strip_suffix('/').unwrap_or(url.path()),
            if url.query().is_some() { "?" } else { "" },
            url.query().unwrap_or(""),
        )) {
            url = new;
        }
    }

    // Strip tracking parameters.
    let tracking_params = ["utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content"];
    if url.query().is_some() {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !tracking_params.contains(&k.as_ref()))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if pairs.is_empty() {
            url.set_query(None);
        } else {
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let qs: String = pairs
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("&");
            url.set_query(Some(&qs));
        }
    }

    // Strip trailing / from path.
    let path = url.path();
    let stripped = path.strip_suffix('/').unwrap_or(path);
    if stripped != path {
        // Cannot mutate path in-place easily; rebuild.
        // Preserve port number.
        let port_suffix = url.port().map(|p| format!(":{}", p)).unwrap_or_default();
        if let Ok(new) = Url::parse(&format!(
            "{}://{}{}{}{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            port_suffix,
            stripped,
            if url.query().is_some() { "?" } else { "" },
            url.query().unwrap_or(""),
        )) {
            url = new;
        }
    }

    url.to_string().to_lowercase()
}

/// Merge raw results from all engines: dedup by normalized URL, combine engine
/// lists and scores, sort by total score descending.
fn merge_results(results: Vec<RawSearchResult>, max_results: usize) -> Vec<SearchResultItem> {
    // Group by normalized URL.
    let mut grouped: HashMap<String, Vec<RawSearchResult>> = HashMap::new();
    for r in results {
        let key = normalize_url(&r.url);
        grouped.entry(key).or_default().push(r);
    }

    // Merge each group.
    let mut merged: Vec<SearchResultItem> = grouped
        .into_values()
        .map(|group| {
            let mut engines = Vec::new();
            let mut total_score = 0.0;
            let mut best_title = String::new();
            let mut best_snippet = String::new();
            let mut best_url = String::new();
            let mut cookies = Vec::new();
            let mut js_extract_result = None;
            let mut image: Option<ImageResult> = None;

            for r in &group {
                if !engines.contains(&r.engine) {
                    engines.push(r.engine.clone());
                }
                total_score += r.score;
                if r.title.len() > best_title.len() {
                    best_title = r.title.clone();
                    best_url = r.url.clone();
                }
                if r.snippet.len() > best_snippet.len() {
                    best_snippet = r.snippet.clone();
                }
                if cookies.is_empty() && !r.cookies.is_empty() {
                    cookies = r.cookies.clone();
                }
                if js_extract_result.is_none() && r.js_extract_result.is_some() {
                    js_extract_result = r.js_extract_result.clone();
                }
                if image.is_none() && r.image.is_some() {
                    image = r.image.clone();
                }
            }

            SearchResultItem {
                title: best_title,
                url: best_url,
                snippet: best_snippet,
                engines,
                score: total_score,
                content: None,
                content_truncated: false,
                fetch_error: None,
                cookies,
                js_extract_result,
                image_url: image.as_ref().map(|i| i.image_url.clone()),
                source_url: image.as_ref().and_then(|i| i.source_url.clone()),
                width: image.as_ref().and_then(|i| i.width),
                height: image.as_ref().and_then(|i| i.height),
            }
        })
        .collect();

    // Sort by score descending.
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Truncate to max_results.
    merged.truncate(max_results);
    merged
}

// ---------------------------------------------------------------------------
// Helpers for engine implementations
// ---------------------------------------------------------------------------

/// Unescape common HTML entities. Shared by image-search engines; Baidu image
/// titles and Bing `m.t` fields arrive HTML-entity-encoded.
pub fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Build a plain reqwest client suitable for search (no auto-redirect, 15s timeout).
pub fn build_plain_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client for search")
}

/// Build a stealth wreq client. Returns None if the "stealth" feature is not
/// enabled. The client is configured with proxy if AGINXBROWSER_PROXY is set and
/// `use_proxy` is true.
#[cfg(feature = "stealth")]
pub fn build_stealth_client(
    use_proxy: bool,
) -> Arc<crate::diting_net::wreq_client::StealthHttpClient> {
    let proxy_url = if use_proxy {
        crate::config::proxy_from_env()
    } else {
        None
    };
    let cookie_jar = Arc::new(crate::diting_net::cookies::CookieJar::new());
    let client =
        crate::diting_net::wreq_client::StealthHttpClient::with_proxy(cookie_jar, proxy_url.as_deref());
    Arc::new(client)
}

#[cfg(not(feature = "stealth"))]
#[allow(dead_code)] // public helper stub for non-stealth builds
pub fn build_stealth_client(_use_proxy: bool) -> Option<()> {
    None
}

/// Build an Android Chrome145 stealth client. Google requires an Android TLS
/// fingerprint to match the GSA (Google Search App) User-Agent; a desktop
/// Chrome145 TLS fingerprint + mobile UA is detected as inconsistent and
/// Google returns a JS-only page instead of server-rendered HTML.
#[cfg(feature = "stealth")]
#[allow(dead_code)] // public helper for engines needing an Android TLS fingerprint
pub fn build_android_stealth_client(
    use_proxy: bool,
) -> Arc<crate::diting_net::wreq_client::StealthHttpClient> {
    let proxy_url = if use_proxy {
        crate::config::proxy_from_env()
    } else {
        None
    };
    let cookie_jar = Arc::new(crate::diting_net::cookies::CookieJar::new());
    let client = crate::diting_net::wreq_client::StealthHttpClient::with_proxy_and_os(
        cookie_jar,
        proxy_url.as_deref(),
        Some(wreq_util::EmulationOS::Android),
    );
    Arc::new(client)
}

#[cfg(not(feature = "stealth"))]
#[allow(dead_code)] // public helper stub for non-stealth builds
pub fn build_android_stealth_client(_use_proxy: bool) -> Option<()> {
    None
}

/// Fetch a URL via the stealth client, returning the decoded text and the
/// final URL. Handles CAPTCHA detection via 302 redirects.
#[cfg(feature = "stealth")]
pub async fn stealth_fetch(
    client: &crate::diting_net::wreq_client::StealthHttpClient,
    url: &str,
) -> Result<(String, String), SearchEngineError> {
    let parsed = Url::parse(url).map_err(|e| SearchEngineError::Transient(format!("bad url: {e}")))?;
    let resp = client
        .fetch(&parsed)
        .await
        .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

    // CAPTCHA detection: check if any intermediate redirect went to an
    // anti-spider URL, or if the final URL landed on one. The stealth client
    // follows redirects internally; redirected_from holds the *source* URLs
    // and resp.url is the final destination.
    let final_url = resp.url.as_str();
    tracing::debug!(
        "stealth_fetch: {} -> final={} redirects={:?} status={}",
        url, final_url,
        resp.redirected_from.iter().map(|u| u.as_str()).collect::<Vec<_>>(),
        resp.status,
    );
    if final_url.contains("/antispider")
        || final_url.contains("wappass.baidu.com")
        || final_url.contains("sorry.google.com")
    {
        return Err(SearchEngineError::Captcha {
            url: final_url.to_string(),
            captcha_type: crate::captcha::detect_captcha_type(final_url, None),
        });
    }
    for redirected in &resp.redirected_from {
        let next = redirected.as_str();
        if next.contains("/antispider")
            || next.contains("wappass.baidu.com")
            || next.contains("sorry.google.com")
        {
            return Err(SearchEngineError::Captcha {
                url: next.to_string(),
                captcha_type: crate::captcha::detect_captcha_type(next, None),
            });
        }
    }

    let text = resp.text();
    let final_url = resp.url.to_string();
    Ok((text, final_url))
}

/// Fetch a URL via plain reqwest, returning the decoded text. Handles
/// charset decoding for Chinese engines (GBK/GB2312). Follows non-CAPTCHA
/// redirects up to `max_redirects` hops.
pub async fn plain_fetch(client: &reqwest::Client, url: &str) -> Result<String, SearchEngineError> {
    let mut current_url = url.to_string();
    let max_redirects = 5;

    for _ in 0..max_redirects {
        let resp = client
            .get(&current_url)
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        // CAPTCHA detection: check for 302 to known anti-spider locations.
        if resp.status().is_redirection() {
            if let Some(location) = resp.headers().get("location") {
                let loc = location.to_str().unwrap_or("");
                if loc.contains("/antispider")
                    || loc.contains("wappass.baidu.com")
                    || loc.contains("sorry.google.com")
                {
                    return Err(SearchEngineError::Captcha {
                        url: current_url.clone(),
                        captcha_type: crate::captcha::detect_captcha_type(loc, None),
                    });
                }
                // Normal redirect — follow it.
                current_url = if loc.starts_with("http") {
                    loc.to_string()
                } else {
                    // Relative redirect.
                    let base = url::Url::parse(&current_url)
                        .map_err(|e| SearchEngineError::Transient(format!("bad url: {e}")))?;
                    base.join(loc)
                        .map(|u| u.to_string())
                        .unwrap_or_else(|_| loc.to_string())
                };
                continue;
            }
            return Err(SearchEngineError::Transient("redirect without location".into()));
        }

        // Non-redirect response — read body.
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read body error: {e}")))?;

        // Decode with charset detection (handles GBK/GB2312 from Baidu/Sogou).
        let text = crate::diting_net::encoding::decode_non_html(&bytes, None);
        return Ok(text);
    }

    Err(SearchEngineError::Transient("too many redirects".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- normalize_url ----

    #[test]
    fn normalize_strips_www_and_lowercases() {
        assert_eq!(normalize_url("https://WWW.Example.com/path"), "https://example.com/path");
    }

    #[test]
    fn normalize_preserves_port() {
        assert_eq!(normalize_url("https://example.com:8080/p"), "https://example.com:8080/p");
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        // Path with trailing slash is stripped.
        assert_eq!(normalize_url("https://example.com/path/"), "https://example.com/path");
        // Bare root: the `url` crate normalizes example.com back to "/" so the
        // trailing slash is preserved (cannot represent a truly empty path).
        assert_eq!(normalize_url("https://example.com/"), "https://example.com/");
    }

    #[test]
    fn normalize_strips_tracking_params() {
        let n = normalize_url("https://example.com/a?utm_source=x&q=hello&utm_medium=y");
        assert!(n.contains("q=hello"));
        assert!(!n.contains("utm_source"));
        assert!(!n.contains("utm_medium"));
    }

    #[test]
    fn normalize_dedup_equivalent_urls() {
        let a = normalize_url("https://www.example.com/article/?utm_source=feed");
        let b = normalize_url("https://EXAMPLE.COM/article");
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_invalid_url_returns_lowered_input() {
        assert_eq!(normalize_url("NOT A URL"), "not a url");
    }

    // ---- merge_results ----

    fn raw(title: &str, url: &str, engine: &str, score: f64) -> RawSearchResult {
        RawSearchResult {
            title: title.into(),
            url: url.into(),
            snippet: "s".into(),
            engine: engine.into(),
            score,
            cookies: vec![],
            js_extract_result: None,
            image: None,
        }
    }

    #[test]
    fn merge_dedups_by_normalized_url() {
        let results = vec![
            raw("Foo", "https://example.com/a", "bing", 5.0),
            raw("Foo Bar", "https://www.example.com/a/", "baidu", 3.0),
        ];
        let merged = merge_results(results, 10);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].engines, vec!["bing", "baidu"]);
        assert_eq!(merged[0].score, 8.0);
        // Picks the longest title.
        assert_eq!(merged[0].title, "Foo Bar");
    }

    #[test]
    fn merge_sorts_by_score_desc() {
        let results = vec![
            raw("Low", "https://example.com/low", "bing", 1.0),
            raw("High", "https://example.com/high", "baidu", 9.0),
        ];
        let merged = merge_results(results, 10);
        assert_eq!(merged[0].title, "High");
        assert_eq!(merged[1].title, "Low");
    }

    #[test]
    fn merge_truncates_to_max_results() {
        let results: Vec<RawSearchResult> = (0..5)
            .map(|i| raw(&format!("t{i}"), &format!("https://e.com/{i}"), "bing", 5.0 - i as f64))
            .collect();
        let merged = merge_results(results, 3);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn merge_empty_input() {
        assert!(merge_results(vec![], 10).is_empty());
    }

    #[test]
    fn merge_carries_cookies() {
        let mut r = raw("T", "https://example.com/a", "sogou", 5.0);
        r.cookies = vec!["session=abc".into()];
        let merged = merge_results(vec![r], 10);
        assert_eq!(merged[0].cookies, vec!["session=abc"]);
    }
}
