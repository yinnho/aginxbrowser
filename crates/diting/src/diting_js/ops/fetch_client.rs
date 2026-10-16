//! Shared reqwest client cache + builder (god-file ratchet split from mod.rs, pure move).

// op_fetch_url backs JS-level `fetch()` and XHR. Pre-#139 it used a
// process-wide `OnceLock<reqwest::Client>` initialised with no proxy, so
// every JS network call bypassed the configured upstream proxy. We now
// build a client per request, threading whatever `proxy_url` the page's
// HttpClient was configured with.
//
// The per-request build cost is negligible (≪1ms) compared with the actual
// network round-trip; the simplification is worth not having to invalidate
// a cache when the proxy is reconfigured between fetches.
//
// Process-wide cache keyed by proxy URL. Previously we built a fresh
// reqwest::Client on every op_fetch_url call (every JS fetch(), XHR,
// dynamic script load). Each build re-initialised TLS roots and a
// fresh connection pool with zero reuse, costing ~5ms per fetch on top
// of any real network work. On an asset-heavy page with 30+ subresources
// that adds ~150ms of pure waste. With the cache, the first fetch on a
// given proxy pays the build cost once and every subsequent fetch reuses
// the same connection pool.
pub(super) static FETCH_CLIENT_CACHE: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<String, reqwest::Client>>,
> = std::sync::OnceLock::new();

/// Shared HTTP client cache for any code in diting-js that needs a
/// reqwest::Client (op_fetch_url for JS-side fetch/XHR, the ES module
/// loader for dynamic imports). Keyed by proxy URL ("" = direct).
/// One client per distinct proxy, reused for every request, so the
/// connection pool actually warms up.
pub fn cached_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    let key = proxy_url.unwrap_or("").to_string();
    let cache = FETCH_CLIENT_CACHE
        .get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Ok(read) = cache.read() {
        if let Some(client) = read.get(&key) {
            return Ok(client.clone());
        }
    }
    let client = build_request_client(proxy_url)?;
    if let Ok(mut write) = cache.write() {
        write.entry(key).or_insert_with(|| client.clone());
    }
    Ok(client)
}

/// Pick a reqwest client for `url`. When the page was opened without a proxy
/// (`proxy_url` is None), subresources also go direct. When the page opened
/// through a proxy (foreign sites), subresources follow it too.
pub(crate) async fn select_request_client(_url: &str, proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    cached_request_client(proxy_url)
}

pub(super) fn build_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    // Redirects are followed manually below so each hop can be re-validated
    // against the same SSRF policy as the initial URL (GHSA-8v6v-g4rh-jmcm).
    // With reqwest's default auto-follow, an attacker-controlled origin can
    // 302 to http://127.0.0.1 and read the internal-service body.
    // Per-request timeout so a scripted fetch()/XHR, or a CORS preflight OPTIONS
    // (issue #251), to a server that accepts the connection but never responds
    // cannot hang forever. Without it op_fetch_url never returns, the fetch
    // promise never settles, and the JS XHR is stuck at readyState 1 with no
    // completion event (which stranded Angular HttpClient). On timeout reqwest's
    // send().await errors, which op_fetch_url propagates and the fetch shim turns
    // into an XHR `error`/`loadend`. 30s matches the other clients in the
    // workspace; AGINXBROWSER_FETCH_TIMEOUT_MS overrides it for tighter cloud limits.
    // The knob is a READ-stall timeout (#49), not a total-duration cap: in
    // reqwest `.read_timeout` gives the header phase 30s from request start
    // (the never-responding server above still times out) and then resets the
    // clock on every body chunk, so a large-but-flowing body (32MB wasm over a
    // slow CDN) completes instead of dying mid-stream with a DecodeError —
    // which used to strand the page's bootstrap promise forever.
    let timeout_ms: u64 = std::env::var("AGINXBROWSER_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let mut builder = crate::diting_net::client::reqwest_builder_no_env_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .read_timeout(std::time::Duration::from_millis(timeout_ms))
        .connect_timeout(std::time::Duration::from_secs(10))
        // Be explicit about pool hygiene: these clients are cached
        // process-wide (FETCH_CLIENT_CACHE), and a half-dead idle connection
        // handed back out of the pool stalls every later request to that
        // origin (the "long-run dispatch degradation", bug #24). A short
        // idle window plus TCP keepalive reaps stale connections instead of
        // trusting them.
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .tcp_keepalive(std::time::Duration::from_secs(30));
    if let Some(proxy) = proxy_url {
        let p = reqwest::Proxy::all(proxy)
            .map_err(|e| format!("Invalid op_fetch_url proxy '{}': {}", proxy, e))?;
        builder = builder.proxy(p);
    }
    builder
        .build()
        .map_err(|e| format!("failed to build reqwest::Client: {}", e))
}
