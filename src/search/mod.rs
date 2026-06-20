pub mod baidu;
pub mod bing;
pub mod google;
pub mod sogou;
pub mod sogou_wechat;

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
}

/// Error from a single engine.
#[derive(Debug)]
pub enum SearchEngineError {
    /// Engine hit a CAPTCHA; should be suspended.
    Captcha { suspend_secs: u64 },
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

pub struct SearchEngineRegistry {
    engines: Vec<Arc<dyn SearchEngine>>,
    /// Engines suspended due to CAPTCHA. Key = engine name, Value = resume time.
    suspended: Arc<RwLock<HashMap<String, std::time::Instant>>>,
}

impl SearchEngineRegistry {
    pub fn new() -> Self {
        let mut registry = SearchEngineRegistry {
            engines: Vec::new(),
            suspended: Arc::new(RwLock::new(HashMap::new())),
        };

        // Register engines. Each engine internally decides whether to use
        // wreq (stealth) or reqwest (plain), and holds its own client.
        registry.register(baidu::BaiduEngine::new());
        registry.register(bing::BingEngine::new());
        registry.register(sogou::SogouEngine::new());
        registry.register(sogou_wechat::SogouWechatEngine::new());
        registry.register(google::GoogleEngine::new());

        registry
    }

    fn register(&mut self, engine: impl SearchEngine + 'static) {
        tracing::info!("search: registered engine {}", engine.name());
        self.engines.push(Arc::new(engine));
    }

    /// Check if an engine is currently suspended.
    async fn is_suspended(&self, name: &str) -> bool {
        let suspended = self.suspended.read().await;
        if let Some(resume_at) = suspended.get(name) {
            if std::time::Instant::now() < *resume_at {
                return true;
            }
        }
        false
    }

    /// Mark an engine as suspended for the given duration.
    async fn suspend_engine(&self, name: &str, duration: Duration) {
        let resume_at = std::time::Instant::now() + duration;
        self.suspended.write().await.insert(name.to_string(), resume_at);
        tracing::warn!("search: engine {} suspended for {:?}", name, duration);
    }

    /// Clean up expired suspensions.
    async fn cleanup_suspensions(&self) {
        let mut suspended = self.suspended.write().await;
        let now = std::time::Instant::now();
        suspended.retain(|_, resume_at| *resume_at > now);
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
) -> (Vec<SearchResultItem>, usize) {
    registry.cleanup_suspensions().await;

    // Filter engines by category.
    let requested: Vec<&str> = categories.split(',').map(|s| s.trim()).collect();
    let eligible: Vec<Arc<dyn SearchEngine>> = registry
        .engines
        .iter()
        .filter(|e| e.categories().iter().any(|c| requested.contains(c)))
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
        };

        let suspended = registry.suspended.clone();

        handles.push(tokio::spawn(async move {
            // Check suspension inside the task.
            {
                let s = suspended.read().await;
                if let Some(resume_at) = s.get(&name) {
                    if std::time::Instant::now() < *resume_at {
                        return (name, Err(SearchEngineError::Suspended));
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

    for handle in handles {
        match handle.await {
            Ok((name, Ok(results))) => {
                total_count += results.len();
                all_results.extend(results);
            }
            Ok((name, Err(SearchEngineError::Captcha { suspend_secs }))) => {
                registry
                    .suspend_engine(&name, Duration::from_secs(suspend_secs))
                    .await;
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
    (merged, total_count)
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
        if let Ok(new) = Url::parse(&format!(
            "{}://{}{}{}{}",
            url.scheme(),
            normalized,
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
        if let Ok(new) = Url::parse(&format!(
            "{}://{}{}{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
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

            for r in &group {
                if !engines.contains(&r.engine) {
                    engines.push(r.engine.clone());
                }
                total_score += r.score;
                // Prefer the result with the longest title (usually most descriptive).
                if r.title.len() > best_title.len() {
                    best_title = r.title.clone();
                    best_url = r.url.clone();
                }
                if r.snippet.len() > best_snippet.len() {
                    best_snippet = r.snippet.clone();
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

/// Build a plain reqwest client suitable for search (no auto-redirect, 15s timeout).
pub fn build_plain_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client for search")
}

/// Build a stealth wreq client. Returns None if the "stealth" feature is not
/// enabled. The client is configured with proxy if OBSCURA_PROXY is set and
/// `use_proxy` is true.
#[cfg(feature = "stealth")]
pub fn build_stealth_client(
    use_proxy: bool,
) -> Arc<crate::obscura_net::wreq_client::StealthHttpClient> {
    let proxy_url = if use_proxy {
        std::env::var("OBSCURA_PROXY").ok()
    } else {
        None
    };
    let cookie_jar = Arc::new(crate::obscura_net::cookies::CookieJar::new());
    let client =
        crate::obscura_net::wreq_client::StealthHttpClient::with_proxy(cookie_jar, proxy_url.as_deref());
    Arc::new(client)
}

#[cfg(not(feature = "stealth"))]
pub fn build_stealth_client(_use_proxy: bool) -> Option<()> {
    None
}

/// Fetch a URL via the stealth client, returning the decoded text and the
/// final URL. Handles CAPTCHA detection via 302 redirects.
#[cfg(feature = "stealth")]
pub async fn stealth_fetch(
    client: &crate::obscura_net::wreq_client::StealthHttpClient,
    url: &str,
) -> Result<(String, String), SearchEngineError> {
    let parsed = Url::parse(url).map_err(|e| SearchEngineError::Transient(format!("bad url: {e}")))?;
    let resp = client
        .fetch(&parsed)
        .await
        .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

    // CAPTCHA detection: check if any intermediate redirect went to an
    // anti-spider URL. The stealth client follows redirects internally but
    // records them in redirected_from.
    for redirected in &resp.redirected_from {
        if redirected.as_str().contains("/antispider")
            || redirected.as_str().contains("wappass.baidu.com")
            || redirected.as_str().contains("sorry.google.com")
        {
            return Err(SearchEngineError::Captcha { suspend_secs: 1800 });
        }
    }

    let text = resp.text();
    let final_url = resp.url.to_string();
    Ok((text, final_url))
}

/// Fetch a URL via plain reqwest, returning the decoded text. Handles
/// charset decoding for Chinese engines (GBK/GB2312).
pub async fn plain_fetch(client: &reqwest::Client, url: &str) -> Result<String, SearchEngineError> {
    let resp = client
        .get(url)
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
                return Err(SearchEngineError::Captcha { suspend_secs: 1800 });
            }
        }
        // For non-CAPTCHA redirects, follow manually by re-requesting.
        // For simplicity, we'll just return an error for now.
        return Err(SearchEngineError::Transient(format!("redirect to: {}", resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?"))));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SearchEngineError::Transient(format!("read body error: {e}")))?;

    // Decode with charset detection (handles GBK/GB2312 from Baidu/Sogou).
    let text = crate::obscura_net::encoding::decode_non_html(&bytes.to_vec(), None);
    Ok(text)
}
