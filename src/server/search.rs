//! `/search` orchestration façade: registry, cache, validation, fetch_top glue.
//!
//! Engine implementations live in `crate::search`; this module is the HTTP-facing
//! acquire path that fans out and optionally hydrates result bodies.

use super::fetch::fetch_url_text_with_cookies;
use crate::{SearchRequest, SearchResponse};

/// Error type for /search (separate from anyhow so we can map to HTTP status).
#[derive(Debug)]
pub enum SearchError {
    /// Internal error → 500
    Other(String),
    /// Caller error (unknown engine name, bad time_range) → 400
    BadRequest(String),
}


/// Shared search engine registry. LazyLock so engine clients (reqwest/wreq)
/// are built once on first use.
pub(crate) static SEARCH_REGISTRY: std::sync::LazyLock<crate::search::SearchEngineRegistry> =
    std::sync::LazyLock::new(crate::search::SearchEngineRegistry::new);

/// Live per-engine suspension state for /doctor.
pub(crate) async fn search_engine_health() -> Vec<crate::search::EngineHealth> {
    SEARCH_REGISTRY.health_snapshot().await
}

/// Validate a SearchRequest's engine filter and time_range before dispatch.
///
/// v0.3.1 Windows report P1-1: `engines: ["baidu"]` (or a typo like
/// "wechat") could silently yield zero results in 0.0s — an unknown name
/// filtered the registry to nothing, or the named engine was benched by a
/// CAPTCHA, and the response looked identical to "no results found". Two
/// of those three failures are caller errors and get a 400 with the valid
/// vocabulary; the suspension case is surfaced per-engine in the response
/// (`engine_errors`) instead. Catalog comes from the caller so tests drive
/// it against mock registries.
fn validate_search_request(
    catalog: &[(String, Vec<String>)],
    req: &crate::SearchRequest,
) -> Result<(), String> {
    if let Some(tr) = req.time_range.as_deref() {
        if crate::search::SearchTimeRange::parse(tr).is_none() {
            return Err(format!(
                "invalid time_range {tr:?}: expected one of day, week, month, year"
            ));
        }
    }
    if req.engines.is_empty() {
        return Ok(());
    }
    let requested: Vec<String> = req
        .categories
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    for name in &req.engines {
        match catalog.iter().find(|(n, _)| n == name) {
            None => {
                let valid: Vec<&str> = catalog.iter().map(|(n, _)| n.as_str()).collect();
                return Err(format!(
                    "unknown engine {name:?}; valid engines: {} (also check GET /engines)",
                    valid.join(", ")
                ));
            }
            Some((_, cats)) => {
                if !cats.iter().any(|c| requested.contains(c)) {
                    return Err(format!(
                        "engine {name:?} does not serve category {:?} (it serves: {})",
                        req.categories,
                        cats.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Rescue candidates for the fallback round (v0.3.2 Windows report #9):
/// the engines serving the requested categories that the caller did NOT
/// name. Non-empty only when an explicit `engines` filter errored in its
/// entirety — every named engine is in `engine_errors` — so a real
/// zero-hits answer from any surviving engine stands as-is and the
/// fallback never rewrites a deliberate engine choice that worked.
fn fallback_candidates(
    requested: &[String],
    categories: &str,
    engine_errors: &std::collections::BTreeMap<String, String>,
    catalog: &[(String, Vec<String>)],
) -> Vec<String> {
    if requested.is_empty() || !requested.iter().all(|e| engine_errors.contains_key(e)) {
        return Vec::new();
    }
    let cats: Vec<&str> = categories.split(',').map(|s| s.trim()).collect();
    catalog
        .iter()
        .filter(|(n, cs)| {
            !requested.iter().any(|r| r == n) && cs.iter().any(|c| cats.contains(&c.as_str()))
        })
        .map(|(n, _)| n.clone())
        .collect()
}

/// /search: native search across Baidu/Bing/Sogou/Google, optionally grab body for top N results.
pub async fn do_search(req: SearchRequest) -> Result<SearchResponse, SearchError> {
    do_search_with_registry(&SEARCH_REGISTRY, req).await
}

/// [`do_search`] over a caller-supplied registry (tests drive mock engine
/// sets through the full orchestration, fallback round included).
pub(crate) async fn do_search_with_registry(
    registry: &crate::search::SearchEngineRegistry,
    req: SearchRequest,
) -> Result<SearchResponse, SearchError> {
    validate_search_request(&registry.engine_catalog(), &req).map_err(SearchError::BadRequest)?;

    // Step 0: short-lived in-process cache (v0.3.1 Windows report P2-5: the
    // same query re-fired back-to-back paid the full ~12s every time). Same
    // TTL knob and eviction shape as the /fetch cache in main.rs.
    let cache_key = search_cache_key(&req);
    if let Some(cached) = search_cache_get(&cache_key) {
        return Ok(cached);
    }

    // Step 1: native search via built-in engines.
    let params = crate::search::SearchParams {
        language: req.language.clone(),
        pageno: 1,
        use_proxy: req.use_proxy,
        timeout_secs: 15,
        engine_filter: req.engines.clone(),
        time_range: req
            .time_range
            .as_deref()
            .and_then(crate::search::SearchTimeRange::parse),
    };

    let (mut items, _raw_total, mut captcha_events, mut engine_errors) =
        crate::search::native_search(registry, &req.q, params, &req.categories, req.max_results)
            .await;

    // Step 1.5: fallback round. An explicit engines filter that failed in
    // its entirety (every named engine errored, zero results) is usually a
    // site-specific wall — the same query over the rest of the category's
    // engines still answers. The substitution is disclosed via
    // `fallback_engines`, never silent (v0.3.2 Windows report #9).
    let mut fallback_engines = None;
    if items.is_empty() {
        let rescue = fallback_candidates(
            &req.engines,
            &req.categories,
            &engine_errors,
            &registry.engine_catalog(),
        );
        if !rescue.is_empty() {
            tracing::info!(
                "search: engines {:?} all failed, retrying over {:?}",
                req.engines,
                rescue
            );
            let fb_params = crate::search::SearchParams {
                language: req.language.clone(),
                pageno: 1,
                use_proxy: req.use_proxy,
                timeout_secs: 15,
                engine_filter: rescue.clone(),
                time_range: req
                    .time_range
                    .as_deref()
                    .and_then(crate::search::SearchTimeRange::parse),
            };
            let (fb_items, _t, fb_events, fb_errors) = crate::search::native_search(
                registry,
                &req.q,
                fb_params,
                &req.categories,
                req.max_results,
            )
            .await;
            engine_errors.extend(fb_errors);
            captcha_events.extend(fb_events);
            if !fb_items.is_empty() {
                items = fb_items;
                fallback_engines = Some(rescue);
            }
        }
    }

    // Step 2: optionally grab body for the top fetch_top results (concurrent).
    // Each fetch runs in its own blocking thread + current-thread runtime
    // (V8 is !Send), so spawn_blocking gives natural isolation + concurrency.
    // Cookies from the search session (e.g. sogou WeChat) are passed through
    // so the diting browser can authenticate redirect URLs.
    let n = req.fetch_top.min(items.len());
    if n > 0 {
        let mut handles = Vec::with_capacity(n);
        for (i, item) in items.iter_mut().enumerate().take(n) {
            // Image results: `url` is a binary image link, not a page — fetching
            // it as HTML is meaningless. Leave content as None for images.
            if item.image_url.is_some() {
                continue;
            }
            // robots.txt gates the body-grab too: /search must not become a
            // side door around the /fetch policy check. A denied item keeps
            // its result entry; only the content fetch is skipped, with the
            // reason in fetch_error so the agent can see why. Same for the
            // per-domain page budget — the reason text carries the stance.
            if let Err(reason) = crate::rate::check_domain(&item.url) {
                item.fetch_error = Some(reason);
                continue;
            }
            if let Err(reason) = crate::robots::assert_allowed(&item.url).await {
                item.fetch_error = Some(reason);
                continue;
            }
            let url = item.url.clone();
            let cookies = item.cookies.clone();
            let use_proxy = req.use_proxy;
            let wait = req.wait_secs;
            let max_chars = req.max_chars_per;
            if !cookies.is_empty() {
                tracing::debug!(
                    "do_search: item {} url={} has {} cookies",
                    i,
                    url,
                    cookies.len()
                );
            }
            handles.push(tokio::task::spawn_blocking(move || {
                (i, fetch_url_text_with_cookies(url, use_proxy, wait, max_chars, &cookies))
            }));
        }
        for h in handles {
            let (i, res) = h
                .await
                .map_err(|e| SearchError::Other(format!("fetch task panicked: {e}")))?;
            match res {
                Ok((content, truncated, final_url)) => {
                    // Redirect-hop re-check (#175): a result whose fetch
                    // redirected onto a robots-disallowed page keeps its
                    // entry but loses the body — same posture as the
                    // pre-fetch item gate above.
                    if let Err(reason) = crate::robots::assert_allowed(&final_url).await {
                        items[i].fetch_error = Some(reason);
                        continue;
                    }
                    items[i].content = Some(content);
                    items[i].content_truncated = truncated;
                }
                Err(e) => {
                    items[i].fetch_error = Some(format!("{e}"));
                }
            }
        }
    }

    let resp = SearchResponse {
        query: req.q,
        // Post-merge, post-truncate count: equals results.len(). The old
        // pre-merge raw total drifted (19 vs 20 between identical queries)
        // and read as "max_results is ignored" to callers comparing it with
        // their cap (v0.3.1 Windows report P2-4).
        number_of_results: items.len(),
        results: items,
        captcha_events,
        engine_errors,
        fallback_engines,
    };
    // Cache successes and clean zeros only. A walled answer (0 results +
    // engine errors) is transient by nature — caching it for the full TTL
    // would keep serving the wall after it lifts (v0.3.2 Windows report:
    // the follow-up search hours later still paid for the morning's wall).
    if !resp.results.is_empty() || resp.engine_errors.is_empty() {
        search_cache_put(&cache_key, &resp);
    }
    Ok(resp)
}

/// Cache key: the request fields that change the response. Mirrors the
/// /fetch cache's shape.
fn search_cache_key(req: &SearchRequest) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
        req.q,
        req.categories,
        req.language,
        req.max_results,
        req.fetch_top,
        req.max_chars_per,
        req.wait_secs,
        req.use_proxy,
        req.time_range.as_deref().unwrap_or(""),
        req.engines,
    )
}

type SearchCache = std::sync::Mutex<std::collections::HashMap<String, (u64, SearchResponse)>>;

static SEARCH_CACHE: std::sync::LazyLock<SearchCache> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Max cached searches before eviction. Search responses (with fetch_top
/// bodies) are large, so this stays well under the /fetch cache's 256.
const SEARCH_CACHE_CAPACITY: usize = 64;

fn search_cache_get(key: &str) -> Option<SearchResponse> {
    let ttl = crate::cache_ttl_secs();
    if ttl == 0 {
        return None;
    }
    let now = crate::now_secs();
    let Ok(mut cache) = SEARCH_CACHE.lock() else {
        return None;
    };
    let (ts, resp) = cache.get(key)?;
    if now.saturating_sub(*ts) < ttl {
        Some(resp.clone())
    } else {
        cache.remove(key);
        None
    }
}

fn search_cache_put(key: &str, resp: &SearchResponse) {
    let ttl = crate::cache_ttl_secs();
    if ttl == 0 {
        return;
    }
    if let Ok(mut cache) = SEARCH_CACHE.lock() {
        if cache.len() >= SEARCH_CACHE_CAPACITY {
            let now = crate::now_secs();
            // First pass: drop expired entries.
            cache.retain(|_, (ts, _)| now.saturating_sub(*ts) < ttl);
            // Still over: evict the oldest entry.
            while cache.len() >= SEARCH_CACHE_CAPACITY {
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
        cache.insert(key.to_string(), (crate::now_secs(), resp.clone()));
    }
}

