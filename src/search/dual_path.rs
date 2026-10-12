//! Direct-first proxy fallback with per-host failure memory
//! (aginxos REQ #414 + search-captcha-challenge).

use url::Url;

use super::obs::{obs_inc, SEARCH_OBS};
use super::validate::{
    challenge_skip_remaining, is_challenge_class, log_rejected_body,
    looks_like_bing_news_url, note_dual_path_challenge, CHALLENGE_PROXY_TIMEOUT_SECS,
};
use super::{
    build_plain_client, classify_rejected_body, proxied_plain_client_timeout, BodyRejectClass,
    SearchEngineError,
};

/// Flatten a reqwest error's source chain (hyper/tokio causes) into one
/// string — the top-level Display alone hides whether a failure was
/// connect/timeout/TLS, which is the one bit remote diagnosis needs.
fn err_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        out.push_str(" <- ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// Speculative direct-leg budget (aginxos REQ #414). The direct attempt is a
/// probe — on a walled network it exists only to be failed — so it gets 4s,
/// not the full fetch budget; the proxy retry keeps its own (longer) client
/// timeout. Bounded the per-engine cost of a blocked host at 4s + proxy
/// instead of 12s + proxy (device log 2026-09-28: single /search calls at
/// 2m08s–2m18s, mostly waiting out direct timeouts that never succeed).
pub(crate) const DIRECT_PROBE_TIMEOUT_SECS: u64 = 4;

/// Consecutive direct failures before a host is sentenced to proxy-only.
pub(crate) const DIRECT_FAIL_THRESHOLD: u8 = 2;
/// How long the proxy-only sentence lasts; any direct success lifts it.
pub(crate) const DIRECT_SKIP_SECS: u64 = 600;
/// Hosts tracked before eviction (expired first, then soonest-to-expire).
pub(crate) const DIRECT_FAILS_CAP: usize = 64;

/// Per-host memory of direct-probe outcomes (aginxos REQ #414). The wall is
/// a property of the box, not of the request, so the memory is process
/// global: after `DIRECT_FAIL_THRESHOLD` consecutive unusable direct legs
/// the host skips straight to the proxy instead of re-paying the probe
/// timeout on every query.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DirectFailState {
    fails: u8,
    skip_until: u64,
}

static DIRECT_FAILS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, DirectFailState>>,
> = std::sync::OnceLock::new();

fn direct_fails()
-> &'static std::sync::Mutex<std::collections::HashMap<String, DirectFailState>> {
    DIRECT_FAILS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Seconds left on a host's proxy-only sentence (0 = probe allowed). Pure
/// half — the fetch path feeds it the shared map, tests feed their own.
pub(crate) fn direct_skip_remaining(
    map: &std::collections::HashMap<String, DirectFailState>,
    host: &str,
    now: u64,
) -> u64 {
    match map.get(host) {
        Some(s) if s.skip_until > now => s.skip_until - now,
        _ => 0,
    }
}

/// Record a direct-probe outcome. Ok clears the host; a fail bumps the
/// streak, sentencing it once the threshold is reached, and the map is
/// capped (expired entries first, then the soonest-to-expire).
pub(crate) fn note_direct_outcome(
    map: &mut std::collections::HashMap<String, DirectFailState>,
    host: &str,
    ok: bool,
    now: u64,
) {
    if ok {
        map.remove(host);
        return;
    }
    let st = map
        .entry(host.to_string())
        .or_insert(DirectFailState { fails: 0, skip_until: 0 });
    st.fails = st.fails.saturating_add(1);
    if st.fails >= DIRECT_FAIL_THRESHOLD {
        st.skip_until = now.saturating_add(DIRECT_SKIP_SECS);
    }
    if map.len() > DIRECT_FAILS_CAP {
        map.retain(|_, s| s.skip_until > now);
        while map.len() > DIRECT_FAILS_CAP {
            let Some(k) = map
                .iter()
                .min_by_key(|(_, s)| (s.skip_until, s.fails))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&k);
        }
    }
}

/// [`note_direct_outcome`] over the process-global map.
fn note_direct_outcome_global(host: &str, ok: bool) {
    let mut map = direct_fails()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    note_direct_outcome(&mut map, host, ok, crate::now_secs());
}

/// Fetch `url` with GET, **direct first**; retry through the proxy when the
/// direct attempt fails at transport level (connect/timeout — the signature
/// of a blocked target) OR when the returned body fails `body_ok` (the
/// signature of a geo-substituted page, e.g. Bing News redirecting CN users
/// to a portal). HTTP-level outcomes are otherwise not retried.
///
/// This makes blocked-source engines work for both deployment classes:
/// overseas operators never configure a proxy and everything connects
/// directly; CN operators set AGINXBROWSER_PROXY once and blocked targets
/// automatically fall through to it. No per-request use_proxy flag needed.
///
/// On a walled network the direct leg is a *probe* — it exists to be failed
/// — so it carries a short budget ([`DIRECT_PROBE_TIMEOUT_SECS`]) and a
/// per-host failure memory: after [`DIRECT_FAIL_THRESHOLD`] consecutive
/// unusable direct outcomes the host goes straight to the proxy for
/// [`DIRECT_SKIP_SECS`] instead of re-paying the probe on every query
/// (aginxos REQ #414: device /search calls ran 2m08s+ mostly waiting out
/// direct timeouts that never succeed).
pub async fn get_direct_first<F>(
    url: &str,
    headers: &[(&str, &str)],
    build_proxied_client: F,
) -> Result<String, SearchEngineError>
where
    F: FnOnce() -> Option<reqwest::Client>,
{
    get_direct_first_if(url, headers, build_proxied_client, |_| true).await
}

/// Same as [`get_direct_first`] with a body-validity predicate: when the
/// direct response's body fails `body_ok`, the proxy retry runs even though
/// the HTTP layer succeeded.
///
/// Challenge / geo-substitute / portal rejects are classified in the error
/// string (not a single "body failed validation"). For those classes — and
/// for bing_news infinitescroll URLs — the proxy retry uses a ≤2s budget; a
/// dual-path challenge sentences the host so the next call fails immediately
/// instead of re-paying both legs (aginxos REQ search-captcha-challenge).
pub async fn get_direct_first_if<F>(
    url: &str,
    headers: &[(&str, &str)],
    build_proxied_client: F,
    body_ok: impl Fn(&str) -> bool,
) -> Result<String, SearchEngineError>
where
    F: FnOnce() -> Option<reqwest::Client>,
{
    let host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));

    // Recent dual-path challenge: fail fast (no probe, no 12s proxy).
    if let Some(h) = host.as_deref() {
        let rem = challenge_skip_remaining(h, crate::now_secs());
        if rem > 0 {
            return Err(SearchEngineError::Transient(format!(
                "unavailable: recent dual-path challenge/portal on {h}; retry in {rem}s"
            )));
        }
    }

    // Built up front: the skip decision below needs to know whether a proxy
    // exists at all — skipping the probe without a fallback would just fail.
    let proxied = build_proxied_client();
    // Known-overseas hosts (should_auto_proxy) go proxy-first when a proxy
    // is available — the direct leg on a CN box is a guaranteed timeout, and
    // the operator asked for proxy-for-overseas rather than fail-fast/skip.
    // Recent-failure memory still skips the probe for other hosts.
    let skip_direct = proxied.is_some()
        && host.as_deref().is_some_and(|h| {
            let auto = crate::config::should_auto_proxy(&format!("https://{h}/"));
            let sentenced = direct_skip_remaining(
                &direct_fails().lock().unwrap_or_else(|e| e.into_inner()),
                h,
                crate::now_secs(),
            ) > 0;
            auto || sentenced
        });

    let plain = build_plain_client(DIRECT_PROBE_TIMEOUT_SECS);
    let last_err;
    let mut direct_reject_class: Option<BodyRejectClass> = None;

    if skip_direct {
        tracing::debug!(
            "search: skipping direct probe for {} (auto-proxy host or recent failures; straight to proxy)",
            host.as_deref().unwrap_or("?")
        );
        last_err = "direct skipped: overseas auto-proxy or recent-failure list".into();
    } else {
        // Attempt 1: direct.
        let mut req = plain.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) => {
                    if body_ok(&body) {
                        if let Some(h) = &host {
                            note_direct_outcome_global(h, true);
                        }
                        return Ok(body);
                    }
                    let class = classify_rejected_body(&body);
                    direct_reject_class = Some(class);
                    log_rejected_body(url, "direct", &body, class);
                    last_err = format!(
                        "direct: body failed validation ({})",
                        class.as_str()
                    );
                }
                Err(_) => {
                    last_err = "direct: read body failed".into();
                }
            },
            Err(e) => {
                last_err = format!("direct: {}", err_chain(&e));
            }
        }
        if let Some(h) = &host {
            note_direct_outcome_global(h, false);
        }
    }

    // Attempt 2: proxied retry, only when a proxy is configured.
    if let Some(proxied) = proxied {
        // Challenge/geo/portal (or bing_news infinitescroll) → ≤2s proxy budget.
        let challenge_fast = direct_reject_class.is_some_and(is_challenge_class)
            || looks_like_bing_news_url(url);
        let client = if challenge_fast {
            tracing::info!(
                "search: direct fetch of {} unusable ({}); retrying via proxy (≤{}s challenge budget)",
                url,
                last_err,
                CHALLENGE_PROXY_TIMEOUT_SECS
            );
            proxied_plain_client_timeout(CHALLENGE_PROXY_TIMEOUT_SECS).unwrap_or(proxied)
        } else {
            tracing::info!(
                "search: direct fetch of {} unusable ({}); retrying via proxy",
                url,
                last_err
            );
            proxied
        };
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        return match req.send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) if body_ok(&body) => Ok(body),
                Ok(body) => {
                    let class = classify_rejected_body(&body);
                    log_rejected_body(url, "proxy", &body, class);
                    if is_challenge_class(class) {
                        if let Some(h) = &host {
                            note_dual_path_challenge(h);
                        }
                    }
                    if looks_like_bing_news_url(url) {
                        obs_inc(&SEARCH_OBS.bing_news_dual_path_fail_total);
                    }
                    Err(SearchEngineError::Transient(format!(
                        "proxy: body failed validation too ({}; challenge/portal on both paths)",
                        class.as_str()
                    )))
                }
                Err(e) => Err(SearchEngineError::Transient(format!(
                    "proxy: read body failed: {e}"
                ))),
            },
            Err(e) => {
                // Timed-out challenge budget still counts as dual-path fail for
                // bing_news so the next call can skip via challenge memory.
                if challenge_fast {
                    if let Some(h) = &host {
                        note_dual_path_challenge(h);
                    }
                    if looks_like_bing_news_url(url) {
                        obs_inc(&SEARCH_OBS.bing_news_dual_path_fail_total);
                    }
                }
                Err(SearchEngineError::Transient(format!(
                    "direct ({last_err}) + proxy: {}",
                    err_chain(&e)
                )))
            }
        };
    }

    // No proxy configured — hand back the direct attempt's failure so
    // engines can distinguish "wrong content" from "no connection" (a
    // body that passed validation already returned above).
    Err(SearchEngineError::Transient(last_err))
}
