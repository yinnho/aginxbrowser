pub mod baidu;
pub mod baidu_images;
pub mod bing;
pub mod bing_images;
pub mod bing_news;
pub mod duckduckgo;
pub mod github_repos;
pub mod arxiv;
pub mod huggingface;
pub mod hn;
pub mod mdn;
pub mod meilisearch;
pub mod npm;
pub mod openalex;
pub mod pypi;
pub mod rubygems;
pub mod sogou;
pub mod sogou_wechat;
pub mod stackexchange;
pub mod wikipedia;

mod captcha;
mod dual_path;
mod obs;
mod validate;

#[cfg(test)]
mod tests;

pub use dual_path::{get_direct_first, get_direct_first_if};
pub use obs::search_obs_snapshot;
pub use validate::{classify_rejected_body, BodyRejectClass};

use captcha::{
    bing_news_enabled, captcha_policy, captcha_suspend_duration, disabled_engines, CaptchaPolicy,
};
use obs::{obs_inc, SEARCH_OBS};

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

/// Freshness window for news-style queries. Engines without a server-side
/// filter (or a dated response body to filter on) ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTimeRange {
    Day,
    Week,
    Month,
    Year,
}

impl SearchTimeRange {
    /// Parse the API vocabulary ("day"|"week"|"month"|"year").
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }
}

/// Parameters passed from the /search API request, adapted for engine use.
pub struct SearchParams {
    pub language: String,
    pub pageno: usize,
    pub use_proxy: bool,
    pub timeout_secs: u64,
    /// Restrict to these engine names; empty = all eligible engines for the category.
    pub engine_filter: Vec<String>,
    /// Freshness window; None = no restriction.
    pub time_range: Option<SearchTimeRange>,
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

impl SearchEngineRegistry {
    pub fn new() -> Self {
        let mut registry = SearchEngineRegistry {
            engines: Vec::new(),
            state: Arc::new(RwLock::new(HashMap::new())),
        };

        // Register engines. Each engine internally decides whether to use
        // wreq (stealth) or reqwest (plain), and holds its own client.
        // AGINXBROWSER_DISABLE_ENGINES / AGINXBROWSER_BING_NEWS=0 drop names
        // before register — CN research ops use this to silence bing_news.
        let disabled = disabled_engines();
        let mut register = |engine: Arc<dyn SearchEngine>| {
            let name = engine.name().to_string();
            if disabled.contains(&name.to_ascii_lowercase()) {
                tracing::info!("search: engine {name} disabled via AGINXBROWSER_DISABLE_ENGINES");
                return;
            }
            tracing::info!("search: registered engine {name}");
            registry.engines.push(engine);
        };
        register(Arc::new(baidu::BaiduEngine::new()));
        register(Arc::new(baidu_images::BaiduImagesEngine::new()));
        register(Arc::new(bing::BingEngine::new()));
        register(Arc::new(bing_images::BingImagesEngine::new()));
        register(Arc::new(sogou::SogouEngine::new()));
        register(Arc::new(sogou_wechat::SogouWechatEngine::new()));
        register(Arc::new(duckduckgo::DuckDuckGoEngine::new()));
        register(Arc::new(wikipedia::WikipediaEngine::new()));
        register(Arc::new(stackexchange::StackExchangeEngine::new()));
        register(Arc::new(mdn::MdnEngine::new()));
        register(Arc::new(github_repos::GithubEngine::new()));
        register(Arc::new(arxiv::ArxivEngine::new()));
        if bing_news_enabled() {
            register(Arc::new(bing_news::BingNewsEngine::new()));
        } else {
            tracing::info!("search: engine bing_news disabled via AGINXBROWSER_BING_NEWS");
        }
        register(Arc::new(huggingface::HuggingFaceEngine::new()));
        register(Arc::new(hn::HnEngine::new()));
        register(Arc::new(npm::NpmEngine::new()));
        register(Arc::new(pypi::PypiEngine::new()));
        register(Arc::new(rubygems::RubygemsEngine::new()));
        register(Arc::new(openalex::OpenalexEngine::new()));
        if let Some(meili) = meilisearch::MeilisearchEngine::from_env() {
            tracing::info!("search: registered engine meilisearch (private index)");
            register(Arc::new(meili));
        }

        registry
    }

    /// Registry from a caller-supplied engine list — the production set is
    /// fixed in `new()`, but the fallback/orchestration layer above (and
    /// its tests) needs to drive `native_search` against a controlled set
    /// of engines (e.g. one that always walls, one that answers).
    /// Test-only: the binary crate exports nothing, so an un-gated
    /// constructor reads as dead code in non-test builds.
    #[cfg(test)]
    pub fn with_engines(engines: Vec<Arc<dyn SearchEngine>>) -> Self {
        SearchEngineRegistry {
            engines,
            state: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Suspend an engine after CAPTCHA, using progressive backoff.
    /// Duration is max(backoff_ladder, engine's base_captcha_suspend).
    /// Returns the engine's consecutive-CAPTCHA count after the increment
    /// (the backoff-ladder step), which callers surface in CaptchaEvent.
    async fn suspend_engine(&self, name: &str, base_suspend: Duration) -> u32 {
        let mut state = self.state.write().await;
        let s = state.entry(name.to_string()).or_insert(EngineSuspendState {
            resume_at: None,
            captcha_count: 0,
        });
        s.captcha_count += 1;
        let duration = captcha_suspend_duration(s.captcha_count, base_suspend);
        let resume_at = std::time::Instant::now() + duration;
        s.resume_at = Some(resume_at);
        obs_inc(&SEARCH_OBS.captcha_suspend_total);
        let policy = match captcha_policy() {
            CaptchaPolicy::Suspend => "suspend",
            CaptchaPolicy::Demote => "demote",
        };
        tracing::warn!(
            "search: engine {} suspended for {:?} (CAPTCHA #{}, policy={})",
            name, duration, s.captcha_count, policy
        );
        s.captcha_count
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

    /// Catalog for discovery/validation: every registered engine's name and
    /// the categories it serves. Owned strings — the trait's borrowed
    /// returns don't outlive the registry lock-free read.
    pub fn engine_catalog(&self) -> Vec<(String, Vec<String>)> {
        self.engines
            .iter()
            .map(|e| {
                (
                    e.name().to_string(),
                    e.categories().iter().map(|c| c.to_string()).collect(),
                )
            })
            .collect()
    }

    /// Live per-engine suspension state for /doctor: who is currently
    /// benched, until when, and how many CAPTCHAs stacked up.
    pub async fn health_snapshot(&self) -> Vec<EngineHealth> {
        self.cleanup_suspensions().await;
        let state = self.state.read().await;
        let now = std::time::Instant::now();
        self.engines
            .iter()
            .map(|e| {
                let name = e.name();
                let (suspended, remaining, count) = match state.get(name) {
                    Some(s) => match s.resume_at {
                        Some(t) if t > now => (true, (t - now).as_secs(), s.captcha_count),
                        _ => (false, 0, s.captcha_count),
                    },
                    None => (false, 0, 0),
                };
                EngineHealth {
                    name: name.to_string(),
                    categories: e.categories().iter().map(|c| c.to_string()).collect(),
                    suspended,
                    suspend_remaining_secs: remaining,
                    captcha_count: count,
                    available: !suspended,
                }
            })
            .collect()
    }
}

/// One engine's health row for /doctor and /engines.
pub struct EngineHealth {
    pub name: String,
    pub categories: Vec<String>,
    pub suspended: bool,
    pub suspend_remaining_secs: u64,
    pub captcha_count: u32,
    /// `!suspended` — the set callers can actually use right now.
    pub available: bool,
}

// ---------------------------------------------------------------------------
// Native search: concurrent dispatch + merge/dedup
// ---------------------------------------------------------------------------

/// Execute native search across all eligible engines, merge and dedup results.
/// Also returns per-engine error reasons (transient failures, CAPTCHA
/// suspensions, task panics) so a zero-result response explains itself
/// instead of silently reporting nothing (v0.3.1 Windows report P1-1: a
/// filtered engine that fails looks identical to "no results found").
pub async fn native_search(
    registry: &SearchEngineRegistry,
    query: &str,
    params: SearchParams,
    categories: &str,
    max_results: usize,
) -> (
    Vec<SearchResultItem>,
    usize,
    Vec<crate::captcha::CaptchaEvent>,
    std::collections::BTreeMap<String, String>,
) {
    registry.cleanup_suspensions().await;

    // Filter engines by category.
    let requested: Vec<&str> = categories.split(',').map(|s| s.trim()).collect();
    let cjk_heavy = is_cjk_heavy(query);
    // Explicit engine_filter always wins; otherwise CJK-heavy general queries
    // drop registry/AI engines that fuzzy-match Chinese into irrelevant
    // English hits (measured: huggingface/arxiv-class noise on TCM queries).
    let general_only = requested.iter().all(|c| *c == "general")
        || (requested.contains(&"general")
            && !requested.iter().any(|c| {
                matches!(*c, "code" | "packages" | "ai" | "academic" | "news" | "images")
            }));
    let eligible: Vec<Arc<dyn SearchEngine>> = registry
        .engines
        .iter()
        .filter(|e| e.categories().iter().any(|c| requested.contains(c)))
        .filter(|e| params.engine_filter.is_empty() || params.engine_filter.iter().any(|n| n.as_str() == e.name()))
        .filter(|e| {
            if !params.engine_filter.is_empty() {
                return true;
            }
            if cjk_heavy && general_only && is_cjk_noise_engine(e.name()) {
                tracing::debug!(
                    "search: skipping engine {} for CJK-heavy general query",
                    e.name()
                );
                return false;
            }
            true
        })
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
            time_range: params.time_range,
        };

        let state = registry.state.clone();

        handles.push((name.clone(), tokio::spawn(async move {
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

            obs_inc(&SEARCH_OBS.engine_dispatch_total);
            if name == "bing_news" {
                obs_inc(&SEARCH_OBS.bing_news_attempt_total);
            }
            let result = engine.search(&query, params).await;
            (name, result)
        })));
    }

    // Collect results.
    let mut all_results: Vec<RawSearchResult> = Vec::new();
    let mut total_count = 0usize;
    let mut captcha_events: Vec<crate::captcha::CaptchaEvent> = Vec::new();
    let mut engine_errors: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for (handle_name, handle) in handles {
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
                let hit_count = registry.suspend_engine(&name, base_suspend).await;
                captcha_events.push(crate::captcha::CaptchaEvent {
                    engine: name.clone(),
                    captcha_type: captcha_type.unwrap_or(crate::captcha::CaptchaType::Unknown),
                    url,
                    detected_at: crate::now_secs(),
                    hit_count,
                    auto_solve_attempted: false,
                    auto_solve_succeeded: false,
                });
                engine_errors.insert(
                    name,
                    "CAPTCHA hit — engine suspended, other engines still serve results".into(),
                );
            }
            Ok((name, Err(SearchEngineError::Transient(msg)))) => {
                tracing::warn!("search: engine {} transient error: {}", name, msg);
                engine_errors.insert(name, format!("transient: {msg}"));
            }
            Ok((name, Err(SearchEngineError::Suspended))) => {
                let remaining = {
                    let state = registry.state.read().await;
                    state
                        .get(&name)
                        .and_then(|s| s.resume_at)
                        .map(|t| t.saturating_duration_since(std::time::Instant::now()).as_secs())
                        .unwrap_or(0)
                };
                tracing::debug!("search: engine {} skipped (suspended)", name);
                engine_errors.insert(
                    name,
                    format!("skipped: CAPTCHA suspension, resumes in {remaining}s"),
                );
            }
            Err(e) => {
                tracing::error!("search: engine task panicked: {}", e);
                engine_errors.insert(handle_name, format!("engine task panicked: {e}"));
            }
        }
    }

    // Merge and dedup.
    let merged = merge_results(all_results, max_results);
    (merged, total_count, captcha_events, engine_errors)
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

    // Soft domain diversity: at most MAX_PER_HOST from one host before
    // filling remaining slots, so a single noisy engine (arxiv/hf) cannot
    // dominate the page for a Chinese medical query.
    diversify_by_host(merged, max_results)
}


// ---------------------------------------------------------------------------
// Query / merge helpers (CJK selection + host diversity)
// ---------------------------------------------------------------------------

/// True when ≥40% of non-whitespace/punctuation characters are CJK/Kana/Hangul.
pub(crate) fn is_cjk_heavy(query: &str) -> bool {
    let mut cjk = 0usize;
    let mut letters = 0usize;
    for c in query.chars() {
        if c.is_whitespace() || c.is_ascii_punctuation() {
            continue;
        }
        letters += 1;
        if is_cjk_char(c) {
            cjk += 1;
        }
    }
    letters > 0 && (cjk as f64 / letters as f64) >= 0.4
}

fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
            | '\u{3400}'..='\u{4DBF}' // Extension A
            | '\u{F900}'..='\u{FAFF}' // Compatibility Ideographs
            | '\u{3040}'..='\u{30FF}' // Hiragana + Katakana
            | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

/// Engines whose fuzzy / namespace matching turns CJK medical / news queries
/// into irrelevant English registry hits when the request is general-only.
fn is_cjk_noise_engine(name: &str) -> bool {
    matches!(
        name,
        "huggingface"
            | "npm"
            | "pypi"
            | "rubygems"
            | "hn"
            | "github"
            | "stackexchange"
    )
}

const MAX_PER_HOST: usize = 2;

fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()))
        .unwrap_or_default()
}

fn diversify_by_host(merged: Vec<SearchResultItem>, max_results: usize) -> Vec<SearchResultItem> {
    // Prefer breadth: take up to MAX_PER_HOST per host first, then fill any
    // remaining slots from the deferred surplus so a single-host SERP still
    // reaches max_results. The early slots therefore cannot be all-arxiv
    // when another host is available.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(max_results.min(merged.len()));
    let mut deferred = Vec::new();
    for item in merged {
        let h = host_of(&item.url);
        let n = counts.entry(h).or_insert(0);
        if *n < MAX_PER_HOST {
            *n += 1;
            out.push(item);
        } else {
            deferred.push(item);
        }
    }
    for item in deferred {
        if out.len() >= max_results {
            break;
        }
        out.push(item);
    }
    out.truncate(max_results);
    out
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
/// Strip inline markup (`<em>` highlights and friends) from a text run
/// lifted out of SERP HTML or embedded JSON. Shared by the engines that
/// read highlight-tagged snippets (baidu, and friends).
pub(crate) fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

pub fn build_plain_client(timeout_secs: u64) -> reqwest::Client {
    diting::diting_net::client::reqwest_builder_no_env_proxy()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client for search")
}

/// Resolve the proxy URL used by search engines' retry / proxy-first legs.
///
/// Order:
/// 1. `AGINXBROWSER_PROXY` (canonical knob; HTTPS_PROXY is ignored by design).
/// 2. Local aginx-proxy stunnel at `127.0.0.1:8800` when that socket is
///    listening — the CN device default (research / grok use the same
///    `socks5h://127.0.0.1:8800`). Covers the historical redfin miss where
///    the unit set no `AGINXBROWSER_PROXY` and duckduckgo/huggingface timed
///    out with no fallback.
pub(crate) fn search_proxy_url() -> Option<String> {
    if let Some(proxy) = crate::config::proxy_from_env() {
        return Some(normalize_socks5h(&proxy));
    }
    if local_socks_proxy_up("127.0.0.1:8800") {
        return Some("socks5h://127.0.0.1:8800".into());
    }
    None
}

/// Cached TCP probe for the local socks listen socket. 30s TTL so a /search
/// that fans out to a dozen engines does not pay a connect() each.
fn local_socks_proxy_up(addr: &str) -> bool {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    struct Cache {
        addr: String,
        up: bool,
        checked_at: Instant,
    }
    static CACHE: std::sync::OnceLock<Mutex<Option<Cache>>> = std::sync::OnceLock::new();
    let lock = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = guard.as_ref() {
        if c.addr == addr && c.checked_at.elapsed() < Duration::from_secs(30) {
            return c.up;
        }
    }
    let up = std::net::TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| {
            std::net::SocketAddr::from(([127, 0, 0, 1], 8800))
        }),
        Duration::from_millis(50),
    )
    .is_ok();
    *guard = Some(Cache {
        addr: addr.to_string(),
        up,
        checked_at: Instant::now(),
    });
    up
}

/// Build a reqwest client through [`search_proxy_url`] — the proxy-retry /
/// proxy-first leg of [`get_direct_first_if`]. Returns None when no proxy is
/// configured and the local 8800 stunnel is down. Shared by every
/// direct-first engine (bing_news, duckduckgo, wikipedia, huggingface,
/// github, arxiv, …).
pub(crate) fn proxied_plain_client() -> Option<reqwest::Client> {
    proxied_plain_client_timeout(12)
}

/// Like [`proxied_plain_client`] with an explicit timeout — challenge/geo
/// retries use a ≤2s budget so dual-path Object-moved spam does not eat the
/// full 12s (aginxos REQ search-captcha-challenge).
pub(crate) fn proxied_plain_client_timeout(timeout_secs: u64) -> Option<reqwest::Client> {
    search_proxy_url().map(|proxy_str| {
        let mut builder = diting::diting_net::client::reqwest_builder_no_env_proxy()
            .timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none());
        match reqwest::Proxy::all(&proxy_str) {
            Ok(p) => builder = builder.proxy(p),
            Err(e) => tracing::warn!("search: proxy scheme rejected: {}", e),
        }
        builder
            .build()
            .expect("failed to build proxied reqwest client for search")
    })
}

/// socks5:// resolves the TARGET hostname locally — on a walled box that
/// fails before the proxy is ever reached — so rewrite to socks5h:// and let
/// the proxy resolve. The scheme prefix is 9 bytes ("socks5://"); the
/// original `format!("socks5h{}", &proxy[7..])` off-by-one produced
/// `socks5h//host:port` (no colon), whose empty hostname surfaces only as
/// "nodename nor servname provided" DNS errors deep in the connect chain —
/// every engine's proxy-retry leg silently broken for socks5:// operators
/// (socks5h:// operators, like the hosted instance, never saw it).
fn normalize_socks5h(proxy: &str) -> String {
    match proxy.strip_prefix("socks5://") {
        Some(rest) => format!("socks5h://{rest}"),
        None => proxy.to_string(),
    }
}

/// Build a stealth wreq client. Returns None if the "stealth" feature is not
/// enabled. The client is configured with proxy if AGINXBROWSER_PROXY is set and
/// `use_proxy` is true.
#[cfg(feature = "stealth")]
pub fn build_stealth_client(
    use_proxy: bool,
) -> Arc<diting::diting_net::wreq_client::StealthHttpClient> {
    let proxy_url = if use_proxy {
        crate::config::proxy_from_env()
    } else {
        None
    };
    let cookie_jar = Arc::new(diting::diting_net::cookies::CookieJar::new());
    let client =
        diting::diting_net::wreq_client::StealthHttpClient::with_proxy(cookie_jar, proxy_url.as_deref());
    Arc::new(client)
}

#[cfg(not(feature = "stealth"))]
#[allow(dead_code)] // public helper stub for non-stealth builds
pub fn build_stealth_client(_use_proxy: bool) -> Option<()> {
    None
}


/// Fetch a URL via the stealth client, returning the decoded text and the
/// final URL. Handles CAPTCHA detection via 302 redirects.
#[cfg(feature = "stealth")]
pub async fn stealth_fetch(
    client: &diting::diting_net::wreq_client::StealthHttpClient,
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
    plain_fetch_with(client, url, &[]).await
}

/// `plain_fetch` with per-request headers. Engines served to a plain
/// (non-stealth) client MUST send a browser UA — a headerless reqwest is
/// fingerprinted as a bot on sight (v0.3.2 Windows report: baidu walled
/// the engine while /engines showed it healthy).
pub async fn plain_fetch_with(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<String, SearchEngineError> {
    let mut current_url = url.to_string();
    let max_redirects = 5;

    for _ in 0..max_redirects {
        let mut req = client.get(&current_url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let resp = req
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
        let text = diting::diting_net::encoding::decode_non_html(&bytes, None);
        return Ok(text);
    }

    Err(SearchEngineError::Transient("too many redirects".into()))
}

