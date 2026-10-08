//! The transport half of the scripted fetch()/XHR path, split out of
//! ops/mod.rs (god-file ratchet): the walk's send-clonable input/output
//! types and the manual SSRF-revalidated redirect walk itself. Everything
//! here is OpState-free by design — the sync-XHR driver runs this walk on
//! a worker thread where Rc/RefCell state cannot follow.

use std::collections::HashMap;
use std::sync::Arc;

use crate::diting_net::cookies::CookieJar;
use crate::diting_net::HttpClient;

use super::cors::{
    cors_response_allows, cors_unsafe_request_header_names, is_cors_safelisted_method,
    parse_cors_header_list, preflight_allows_header, preflight_allows_method, request_origin,
};
use super::fetch_gate::validate_fetch_url;
use super::fetch_body_byte_limit;
use super::{fetch_referer, select_request_client, FetchCredentials, FETCH_REDIRECT_LIMIT};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use url::Url;

/// op_fetch_url's terminal response. The manual redirect walk yields a live
/// reqwest response; the legacy-TLS fallback (transport failure on the raw
/// client) yields an already-buffered engine response whose redirects were
/// resolved — and per-hop revalidated — inside the HttpClient.
enum OpFetchOutcome {
    Live(reqwest::Response),
    Buffered(crate::diting_net::Response),
}

/// Network-event payload the walk hands back to its driver, which records it
/// into OpState (`fetch-{N}` id space, body store, js_network_events) once
/// the walk returns and OpState is reachable again.
pub(crate) struct FetchNetworkEvent {
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) status: u16,
    pub(crate) response_headers: std::collections::HashMap<String, String>,
    pub(crate) request_headers: std::collections::HashMap<String, String>,
    pub(crate) body_size: usize,
    pub(crate) stored_text: Option<String>,
    pub(crate) resp_body_base64: String,
    // #203 redirect observability — threaded to the recorded event.
    pub(crate) final_url: String,
    pub(crate) redirects: Vec<crate::diting_net::RedirectHop>,
}

/// A walk failure to replay into the network-event log. `url` is the
/// REQUESTED address (#203: rows used to carry only the last hop, so a
/// submit.htm bounced to error.taobao.com was unfindable by its original
/// URL); `final_url` is where the walk ended up — the response that
/// triggered the failure, or the redirect target that was refused.
pub(crate) struct FetchFailure {
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) reason: String,
    pub(crate) final_url: String,
    pub(crate) redirects: Vec<crate::diting_net::RedirectHop>,
}

/// What [`fetch_url_walk`] produces: the exact JSON envelope the op returns,
/// plus the network event to record (None on the preflight-reject paths,
/// which carry their failure through `deps.failures` instead).
pub(crate) struct FetchWalkOutcome {
    pub(crate) json: String,
    pub(crate) network: Option<FetchNetworkEvent>,
}

/// Send-clonable inputs both fetch drivers (the deferred async op and the
/// sync-XHR op) hand to [`fetch_url_walk`]. Nothing here borrows OpState —
/// the sync driver runs the walk on a worker thread, where the Rc/RefCell
/// state cannot follow. Failure network events recorded mid-walk are
/// collected here and replayed by the driver.
pub(crate) struct FetchWalkDeps {
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) custom_headers: std::collections::HashMap<String, String>,
    pub(crate) body_bytes: Vec<u8>,
    pub(crate) page_origin: String,
    pub(crate) mode: String,
    pub(crate) credentials: FetchCredentials,
    pub(crate) cookie_jar: Option<Arc<CookieJar>>,
    pub(crate) in_flight: Option<Arc<std::sync::atomic::AtomicU32>>,
    pub(crate) in_flight_list: Option<std::sync::Arc<std::sync::Mutex<Vec<crate::diting_net::InFlightScripted>>>>,
    pub(crate) http_client: Option<Arc<HttpClient>>,
    pub(crate) proxy_url: Option<String>,
    pub(crate) document_url: String,
    pub(crate) referrer_policy: String,
    pub(crate) referrer_init: String,
    pub(crate) callbacks: Option<std::sync::Arc<crate::diting_net::CallbackRegistry>>,
    pub(crate) failures: Vec<FetchFailure>,
}

/// Drop-remove a pushed in-flight entry — the walk returns from a dozen
/// paths (redirect hops, fallback escapes, errors); Drop is the only
/// discipline that keeps the registry honest across all of them (#116).
struct InFlightEntryGuard {
    list: std::sync::Arc<
        std::sync::Mutex<Vec<crate::diting_net::InFlightScripted>>,
    >,
    pub(crate) url: String,
    pub(crate) method: String,
}

impl Drop for InFlightEntryGuard {
    fn drop(&mut self) {
        if let Ok(mut l) = self.list.lock() {
            if let Some(pos) = l
                .iter()
                .position(|f| f.url == self.url && f.method == self.method)
            {
                l.remove(pos);
            }
        }
    }
}

/// Case-insensitive header-name membership, shared by the hop-header merge
/// (custom headers win over browser defaults).
fn header_present(headers: &HashMap<String, String>, name: &str) -> bool {
    headers.keys().any(|k| k.eq_ignore_ascii_case(name))
}

/// Convert a buffered `diting_net::Response` (stealth-hop or legacy-fallback
/// shape) into the CORS preflight decision tuple: headers as a HeaderMap for
/// the method/header list parsers below, the two CORS answer headers, the
/// status. The conversion the legacy-fallback path used to do inline; the
/// stealth-first preflight needs the identical one (#116).
fn buffered_preflight_parts(
    fr: &crate::diting_net::Response,
) -> (
    reqwest::header::HeaderMap,
    String,
    String,
    u16,
) {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &fr.headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::try_from(name.as_str()),
            reqwest::header::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(n, v);
        }
    }
    (
        headers,
        fr.header("access-control-allow-origin")
            .unwrap_or("")
            .to_string(),
        fr.header("access-control-allow-credentials")
            .unwrap_or("")
            .to_string(),
        fr.status,
    )
}

/// The transport half of op_fetch_url, shared verbatim by the deferred async
/// op and the sync-XHR op (obscura#908): client selection, CORS preflight,
/// the manual SSRF-revalidated redirect walk, scripted per-hop headers, the
/// terminal CORS check, the body cap, and response-callback dispatch. Touches
/// no OpState — everything stateful rides inside `deps` as Send clones;
/// failure events collect into `deps.failures` and the success network event
/// returns in the outcome, both for the driver to record once the walk
/// returns and OpState is reachable again.
pub(crate) async fn fetch_url_walk(
    deps: &mut FetchWalkDeps,
) -> Result<FetchWalkOutcome, deno_error::JsErrorBox> {
    let url = deps.url.clone();
    let method = deps.method.clone();
    let mode = deps.mode.clone();
    let page_origin = deps.page_origin.clone();
    let document_url = deps.document_url.clone();
    // The fetch's initiator for site classification: the document, else the
    // origin string, else an opaque sentinel (bare tool fetch — no page).
    let fetch_initiator: url::Url = url::Url::parse(&document_url)
        .ok()
        .or_else(|| url::Url::parse(&page_origin).ok())
        .unwrap_or_else(|| url::Url::parse("about:blank").unwrap());
    let referrer_policy = deps.referrer_policy.clone();
    let referrer_init = deps.referrer_init.clone();
    let custom_headers = std::mem::take(&mut deps.custom_headers);
    let body_bytes = std::mem::take(&mut deps.body_bytes);
    let credentials = deps.credentials;
    let cookie_jar = deps.cookie_jar.clone();
    let in_flight = deps.in_flight.clone();
    let in_flight_list = deps.in_flight_list.clone();
    let http_client = deps.http_client.clone();
    let callbacks = deps.callbacks.clone();
    let proxy_url = deps.proxy_url.clone();

    // Pages use their context-scoped client so sequential runtimes never
    // share an async connection pool (upstream ab6fa0e, #453). The
    // process-wide cache remains the fallback for runtimes with no owning
    // HttpClient (e.g. a bare module-loader runtime).
    let client = match &http_client {
        Some(client) => client.request_client(&url).await,
        None => select_request_client(&url, proxy_url.as_deref())
            .await
            .map_err(deno_error::JsErrorBox::generic)?,
    };

    let req_method: reqwest::Method = method.parse().unwrap_or(reqwest::Method::GET);

    let is_cross_origin = request_origin(&url)
        .map(|initial| !page_origin.is_empty() && initial != page_origin)
        .unwrap_or(false);

    let unsafe_header_names = if is_cross_origin && mode == "cors" {
        cors_unsafe_request_header_names(&custom_headers)
    } else {
        Vec::new()
    };
    let needs_preflight = is_cross_origin
        && mode == "cors"
        && (!is_cors_safelisted_method(&req_method) || !unsafe_header_names.is_empty());

    if needs_preflight {
        let mut preflight_request = client
            .request(reqwest::Method::OPTIONS, &url)
            .header("Origin", &page_origin)
            .header("Access-Control-Request-Method", method.as_str());
        if !unsafe_header_names.is_empty() {
            preflight_request = preflight_request.header(
                "Access-Control-Request-Headers",
                unsafe_header_names.join(","),
            );
        }
        // The preflight's three headers (never cookies — a CORS preflight
        // is never credentialed), as one map so both transports send the
        // same request.
        let mut preflight_headers: HashMap<String, String> = HashMap::new();
        preflight_headers.insert("Origin".to_string(), page_origin.clone());
        preflight_headers.insert(
            "Access-Control-Request-Method".to_string(),
            method.clone(),
        );
        if !unsafe_header_names.is_empty() {
            preflight_headers.insert(
                "Access-Control-Request-Headers".to_string(),
                unsafe_header_names.join(","),
            );
        }
        // #116: the preflight rides the same transport the main request
        // will — stealth-first on a stealth page, before plain rustls.
        // `Some(Err)` records that the stealth stack already fired so the
        // plain attempt's own legacy fallback stays gated (at most two
        // transports per request); `None` (non-stealth build/context,
        // SOCKS) leaves the plain path exactly as it was.
        let stealth_preflight = match http_client.as_ref() {
            Some(hc) => match url::Url::parse(&url) {
                Ok(u) => {
                    hc.scripted_stealth_hop(
                        &reqwest::Method::OPTIONS,
                        &u,
                        None,
                        None,
                        Some(&preflight_headers),
                        false,
                    )
                    .await
                }
                Err(_) => None,
            },
            None => None,
        };
        let preflight_stealth_err = match &stealth_preflight {
            Some(Err(se)) => Some(se.to_string()),
            _ => None,
        };
        let (pf_headers, allowed_origin, allow_credentials, preflight_status) =
            match &stealth_preflight {
                Some(Ok(fr)) => buffered_preflight_parts(fr),
                _ => match preflight_request.send().await {
                    Ok(p) => (
                        p.headers().clone(),
                        p.headers()
                            .get("access-control-allow-origin")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                        p.headers()
                            .get("access-control-allow-credentials")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                        p.status().as_u16(),
                    ),
                    Err(e) => {
                        // The plain preflight keeps the legacy-TLS escape
                        // hatch (a CBC-only endpoint kills the OPTIONS in
                        // the ClientHello before the main request — which
                        // does retry on connect-stage failure — ever ran)
                        // unless the stealth stack already fired above.
                        let fallback = match http_client.as_ref() {
                            Some(hc) if preflight_stealth_err.is_none() => {
                                match url::Url::parse(&url) {
                                    Ok(u) => Some(
                                        hc.scripted_fetch_fallback(
                                            &reqwest::Method::OPTIONS,
                                            &u,
                                            &e.to_string(),
                                            None,
                                            None,
                                            e.is_connect(),
                                            Some(&preflight_headers),
                                            false,
                                        )
                                        .await,
                                    ),
                                    Err(_) => None,
                                }
                            }
                            _ => None,
                        };
                        match fallback {
                            Some(Ok(fr)) => buffered_preflight_parts(&fr),
                            _ => {
                                let error = match preflight_stealth_err {
                                    Some(se) => format!(
                                        "CORS preflight failed: {e} (stealth transport also failed: {se})"
                                    ),
                                    None => format!("CORS preflight failed: {e}"),
                                };
                                deps.failures.push(FetchFailure {
                                        url: url.clone(),
                                        method: method.clone(),
                                        reason: error.clone(),
                                        final_url: url.clone(),
                                        redirects: Vec::new(),
                                    });
                                return Err(deno_error::JsErrorBox::generic(error));
                            }
                        }
                    }
                },
            };

        // Every preflight rejection below is an early exit the JS side sees as
        // a rejected promise — each must leave a status-0 network event with
        // the reason, or the request vanishes from /network (the same ghost
        // the taobao punished-mtop report chased on the response-side gate).
        let mut reject_preflight = |message: String| -> deno_error::JsErrorBox {
            deps.failures.push(FetchFailure {
                url: url.clone(),
                method: method.clone(),
                reason: message.clone(),
                final_url: url.clone(),
                redirects: Vec::new(),
            });
            deno_error::JsErrorBox::generic(message)
        };

        // Fetch validates the preflight's HTTP status before its CORS headers;
        // the only observable difference is which error a 403-without-ACAO
        // preflight reports.
        if !(200..300).contains(&preflight_status) {
            return Err(reject_preflight(format!(
                "CORS preflight returned HTTP {}",
                preflight_status
            )));
        }
        if !cors_response_allows(credentials, &page_origin, &allowed_origin, &allow_credentials) {
            return Err(reject_preflight(format!(
                "CORS preflight: Origin '{}' not allowed by Access-Control-Allow-Origin '{}'",
                page_origin, allowed_origin
            )));
        }

        // The preflight must actually authorize the method and the unsafe
        // headers — a server that allows the origin but never listed the
        // method/headers does not consent to this request (obscura "enforce
        // CORS preflight permissions" fix, 04f0475).
        let allowed_methods = parse_cors_header_list(
            &pf_headers,
            "access-control-allow-methods",
        )
        .ok_or_else(|| {
            reject_preflight(
                "CORS preflight returned an invalid Access-Control-Allow-Methods value".to_string(),
            )
        })?;
        let allowed_headers = parse_cors_header_list(
            &pf_headers,
            "access-control-allow-headers",
        )
        .ok_or_else(|| {
            reject_preflight(
                "CORS preflight returned an invalid Access-Control-Allow-Headers value".to_string(),
            )
        })?;
        let credentialed = credentials == FetchCredentials::Include;
        if !preflight_allows_method(&req_method, &allowed_methods, credentialed) {
            return Err(reject_preflight(format!(
                "CORS preflight did not allow method '{}'",
                req_method
            )));
        }
        if let Some(name) = unsafe_header_names
            .iter()
            .find(|name| !preflight_allows_header(name, &allowed_headers, credentialed))
        {
            return Err(reject_preflight(format!(
                "CORS preflight did not allow request header '{}'",
                name
            )));
        }
    }

    // Follow redirects manually so the SSRF policy applies to every hop.
    // reqwest's auto-follow would bypass validate_fetch_url on the redirect
    // target and let an attacker-allowed origin 302 to http://127.0.0.1
    // (GHSA-8v6v-g4rh-jmcm).
    let mut current_url = url.clone();
    let mut current_method = req_method;
    let mut current_body = body_bytes;
    let mut redirects_followed: usize = 0;
    // Every hop target, in order — bounded by FETCH_REDIRECT_LIMIT because
    // the walk stops there. Surfaced on the success JSON (and the
    // redirect-shaped failures) so JS consumers can see where a fetch that
    // "went somewhere else" actually went; the walk stops at the limit so
    // the vec needs no separate cap.
    let mut redirect_chain: Vec<String> = Vec::new();
    // Same trail with each hop's 3xx status — the network-event face
    // (#203); `redirect_chain` above stays the url-only JSON envelope.
    let mut redirect_hops: Vec<crate::diting_net::RedirectHop> = Vec::new();
    // Fetch's redirect stripping (obscura#967 same hole): once the chain
    // crosses an origin, credentials scripted into custom headers stop
    // riding (browsers never forward Authorization to the redirect target),
    // and a 301/302/303 that downgrades the method to GET drops the request
    // body headers with it. Both sticky for the rest of the chain.
    let mut strip_credentials = false;
    let mut strip_body_headers = false;
    // Fetch's response tainting: once a cors-mode request touches a
    // cross-origin URL, every response in the chain gets the CORS check —
    // including the intermediate redirect responses (obscura#973 same hole:
    // only the terminal response used to be checked).
    let mut cors_tainted = request_origin(&url)
        .map(|o| o != page_origin)
        .unwrap_or(false);
    // #116: sticky across hops — once the stealth stack has fired for this
    // request (Ok or Err), plain's own legacy fallback stays off: a request
    // never burns more than two transports.
    let mut stealth_fired = false;
    // #97: the header set of the hop that produced the final response —
    // rebuilt every iteration alongside `hop_headers` (assigned before any
    // hop can break or continue), so after the loop it holds the LAST hop's
    // outbound set (redirects re-shape headers: credentials stripped on
    // cross-origin jumps, body headers dropped on method downgrades).
    // Surfaced on every network face.
    let mut final_hop_headers: HashMap<String, String>;

    // Passive on_request observers (upstream #408): fire with the request as
    // the script shaped it, once, before the first hop goes out.
    if let Some(cbs) = callbacks.as_ref() {
        if cbs.has_request_callbacks().await {
            let sent_headers: HashMap<String, String> = custom_headers
                .iter()
                .map(|(k, v)| (k.to_lowercase(), v.clone()))
                .collect();
            let info = crate::diting_net::RequestInfo {
                url: url::Url::parse(&current_url).unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                method: current_method.to_string(),
                headers: sent_headers,
                resource_type: crate::diting_net::ResourceType::Fetch,
            };
            cbs.fire_request(&info).await;
        }
    }

    let mut response = loop {
        let mut req = client.request(current_method.clone(), &current_url);

        // Cross-origin and credentials are per-hop: a redirect can change
        // either answer (upstream b744b9b). Browsers send Origin on every
        // non-GET/HEAD request too — same-origin POSTs carry it (SolidStart
        // server functions 403 without it), so gate on method, not just domain.
        let current_is_cross_origin = request_origin(&current_url)
            .map(|o| o != page_origin)
            .unwrap_or(false);
        let method_needs_origin = current_method != reqwest::Method::GET
            && current_method != reqwest::Method::HEAD;
        if method_needs_origin || current_is_cross_origin {
            req = req.header("Origin", &page_origin);
        }

        let credentials_allowed = credentials.allows(&page_origin, &current_url);
        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    // SameSite send gate: fetch()/XHR are subresources, so
                    // Lax/Strict cookies never ride a cross-site hop.
                    let send_ctx = crate::diting_net::cookies::SendContext::subresource(
                        !crate::diting_net::site::is_same_site(&fetch_initiator, &parsed_url),
                    );
                    let cookie_header = jar.get_cookie_header_for(&parsed_url, &send_ctx);
                    if !cookie_header.is_empty() {
                        req = req.header("Cookie", &cookie_header);
                    }
                }
            }
        }

        // Send browser-default headers on fetch()/XHR requests. The navigation
        // path sets these, but this op did not: scripted requests went out bare
        // (no UA, no Accept, no Fetch-Metadata / client-hint headers) and WAFs
        // that key on `sec-fetch-site: same-origin` (mcpservers.org /submit
        // 403s without it) rejected them. Honor explicit overrides.
        const DEFAULT_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
        let ua_override = custom_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.clone());
        // Scripted hops used to hardcode this macOS-Chrome145 UA while the
        // document hop carried the context persona's (import_curl sessions
        // and per-account personas ride their own) — one more identity
        // mismatch for WAFs to key on. Prefer the context client's UA;
        // DEFAULT_UA stays the floor for bare runtimes with no owning
        // client (identical string under the default persona, so pinned
        // header expectations don't move).
        let client_ua = match http_client.as_ref() {
            Some(hc) => hc.user_agent.read().await.clone(),
            None => DEFAULT_UA.to_string(),
        };
        let effective_ua = ua_override.clone().unwrap_or(client_ua);
        let (sec_ch_ua, sec_ch_ua_platform) =
            crate::diting_net::client::derive_client_hints(&effective_ua);
        if ua_override.is_none() {
            req = req.header("User-Agent", &effective_ua);
        }
        for (name, value) in [
            ("sec-ch-ua", sec_ch_ua.clone()),
            ("sec-ch-ua-mobile", "?0".to_string()),
            ("sec-ch-ua-platform", sec_ch_ua_platform.clone()),
            ("accept", "*/*".to_string()),
        ] {
            if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case(name)) {
                req = req.header(name, value);
            }
        }
        // Fetch Metadata: derived per-hop because a redirect can change
        // same/cross-site. fetch()/XHR are always dest "empty". The value
        // follows the Fetch spec's four-value vocabulary against the
        // initiator's SITE (registrable domain), not full-origin equality —
        // a sibling-subdomain request (item.upload.taobao.com →
        // everyhelp.taobao.com) is `same-site`, a label Chrome can produce
        // while carrying that site's Lax login cookies; `cross-site` there
        // is an impossible-in-Chrome combination taobao risk control reads
        // as cookie theft (#203).
        let sec_fetch_site = crate::diting_net::site::sec_fetch_site(
            Some(&fetch_initiator),
            &url::Url::parse(&current_url).unwrap_or_else(|_| fetch_initiator.clone()),
        );
        let sec_fetch_mode = if mode.is_empty() { "cors" } else { mode.as_str() };
        for (name, value) in [
            ("sec-fetch-site", sec_fetch_site.to_string()),
            ("sec-fetch-mode", sec_fetch_mode.to_string()),
            ("sec-fetch-dest", "empty".to_string()),
        ] {
            if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case(name)) {
                req = req.header(name, value);
            }
        }

        // The Referer honors RequestInit's referrerPolicy/referrer when the
        // fetch carried them (obscura#875); the default
        // strict-origin-when-cross-origin trims per hop like client.rs.
        // Domain-whitelist APIs (e.g. AMap keys bound to a domain) reject
        // bare requests. Explicit Referer in fetch init still wins.
        if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case("referer"))
            && (!document_url.is_empty() || referrer_init != "about:client")
        {
            if let Ok(target) = Url::parse(&current_url) {
                let doc = Url::parse(&document_url).unwrap_or_else(|_| target.clone());
                let ref_val = fetch_referer(&referrer_policy, &referrer_init, &doc, &target);
                if !ref_val.is_empty() {
                    req = req.header(reqwest::header::REFERER, ref_val);
                }
            }
        }

        // Per-hop effective scripted headers: the first hop sends everything
        // as the script shaped it; after a credential-triggering or
        // method-downgrading redirect the stripped subset rides instead.
        let effective_headers: HashMap<String, String> = custom_headers
            .iter()
            .filter(|(k, _)| {
                let lower = k.to_ascii_lowercase();
                if strip_credentials
                    && matches!(
                        lower.as_str(),
                        "authorization" | "proxy-authorization" | "cookie"
                    )
                {
                    return false;
                }
                if strip_body_headers
                    && matches!(
                        lower.as_str(),
                        "content-type"
                            | "content-length"
                            | "content-encoding"
                            | "content-language"
                            | "content-location"
                    )
                {
                    return false;
                }
                true
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in &effective_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        // #116: the hop's full outbound header set as one map. The
        // stealth-first attempt below and the legacy fallback in the Err
        // arm both send THIS — the same request on another stack, not a
        // near-copy rebuilt per site. Custom (filtered) headers ride as
        // the script shaped them; each browser default joins only when
        // the script didn't set its own (the same gates the plain hop's
        // .header() calls above apply).
        let mut hop_headers: HashMap<String, String> = effective_headers.clone();
        if (method_needs_origin || current_is_cross_origin)
            && !header_present(&hop_headers, "origin")
        {
            hop_headers.insert("Origin".into(), page_origin.clone());
        }
        if ua_override.is_none() && !header_present(&hop_headers, "user-agent") {
            hop_headers.insert("User-Agent".into(), effective_ua.clone());
        }
        for (name, value) in [
            ("sec-ch-ua", sec_ch_ua.clone()),
            ("sec-ch-ua-mobile", "?0".to_string()),
            ("sec-ch-ua-platform", sec_ch_ua_platform.clone()),
            ("accept", "*/*".to_string()),
            ("sec-fetch-site", sec_fetch_site.to_string()),
            ("sec-fetch-mode", sec_fetch_mode.to_string()),
            ("sec-fetch-dest", "empty".to_string()),
        ] {
            if !header_present(&hop_headers, name) {
                hop_headers.insert(name.to_string(), value);
            }
        }
        if !header_present(&hop_headers, "referer")
            && (!document_url.is_empty() || referrer_init != "about:client")
        {
            if let (Ok(target), Ok(doc)) =
                (Url::parse(&current_url), Url::parse(&document_url))
            {
                let ref_val = fetch_referer(&referrer_policy, &referrer_init, &doc, &target);
                if !ref_val.is_empty() {
                    hop_headers.insert("Referer".into(), ref_val);
                }
            }
        }
        let hop_ctype = hop_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        final_hop_headers = hop_headers.clone();

        if !current_body.is_empty() {
            req = req.body(current_body.clone());
        }

        if let Some(ref counter) = in_flight {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // #116: register the hop so a hung scripted request is observable
        // while it hangs (fetch/XHR promises that never settle read on the
        // surface as a silent EVAL_TIMEOUT). Dropped — i.e. removed — on
        // every exit path of this hop.
        let _in_flight_guard = in_flight_list.as_ref().map(|list| {
            let entry = crate::diting_net::InFlightScripted {
                url: current_url.clone(),
                method: current_method.as_str().to_string(),
                dispatched_at_ms: crate::diting_net::client::epoch_ms(),
            };
            if let Ok(mut l) = list.lock() {
                l.push(entry);
            }
            InFlightEntryGuard {
                list: list.clone(),
                url: current_url.clone(),
                method: current_method.as_str().to_string(),
            }
        });

        // #116 stealth-first hop: on a stealth page the scripted request
        // rides the wreq emulation stack BEFORE plain rustls — the same
        // TLS fingerprint the navigation above it presented, so the cookie
        // jar never appears behind two JA3s (the bilibili-412-shaped tell
        // that held scripted requests until xhr.timeout fired while the
        // document request sailed through). Some(Ok) settles the whole
        // request here: the stealth walk re-validates SSRF per hop,
        // downgrades 303-class redirects and stores cookies itself.
        // Some(Err) means the stealth stack fired and failed — fall
        // through to plain with plain's legacy fallback gated off below.
        // None keeps the plain path exactly as it was (non-stealth
        // build/context, SOCKS proxy).
        let stealth_hop = match http_client.as_ref() {
            Some(hc) => match url::Url::parse(&current_url) {
                Ok(u) => {
                    hc.scripted_stealth_hop(
                        &current_method,
                        &u,
                        (!current_body.is_empty()).then_some(current_body.as_slice()),
                        hop_ctype.as_deref(),
                        Some(&hop_headers),
                        credentials_allowed,
                    )
                    .await
                }
                Err(_) => None,
            },
            None => None,
        };
        match stealth_hop {
            Some(Ok(buffered)) => {
                if let Some(ref counter) = in_flight {
                    counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                // #231: the stealth walk resolved its redirects inside the
                // client — adopt the trail (target+status hops and the
                // url-only chain) so the network face reports the wander
                // on the stealth build too, not just the final URL.
                redirect_hops.extend(buffered.redirect_hops.iter().cloned());
                redirect_chain
                    .extend(buffered.redirect_hops.iter().map(|h| h.url.clone()));
                break OpFetchOutcome::Buffered(buffered);
            }
            Some(Err(_)) => {
                stealth_fired = true;
            }
            None => {}
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if let Some(ref counter) = in_flight {
                    counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                // Scripted fetch()/XHR walked this far on a raw reqwest
                // client with no retry of its own — the one subresource path
                // the Tier2 fallback didn't cover (the g.alicdn.com shape:
                // plain rustls dies on the handshake, the stealth stack's
                // BoringSSL connects). A GET/HEAD transport failure gets the
                // same one-shot legacy-TLS escape hatch the script and
                // stylesheet loaders use; any other method rides only when
                // the failure is connect-stage (DNS/TCP/TLS — the request
                // provably never left the machine), so a POST form submit
                // cannot double-submit (taobao seller-backend shape:
                // CBC-only endpoints killed every publish POST at the
                // handshake). The retry sends the hop's merged header set
                // (customs + Origin, UA, Fetch-Metadata, client hints —
                // `hop_headers`, not a rebuilt near-copy) and the
                // credentials policy. On a stealth page the stealth-first
                // attempt above already rode this same transport, so the
                // fallback stays gated (#116: at most two transports).
                let fallback = match http_client.as_ref() {
                    Some(hc)
                        if !stealth_fired
                            && (current_method == reqwest::Method::GET
                                || current_method == reqwest::Method::HEAD
                                || e.is_connect()) =>
                    {
                        match url::Url::parse(&current_url) {
                            Ok(u) => Some(
                                hc.scripted_fetch_fallback(
                                    &current_method,
                                    &u,
                                    &e.to_string(),
                                    (!current_body.is_empty()).then_some(current_body.as_slice()),
                                    hop_ctype.as_deref(),
                                    e.is_connect(),
                                    Some(&hop_headers),
                                    credentials_allowed,
                                )
                                .await,
                            ),
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };
                match fallback {
                    Some(Ok(buffered)) => {
                        // #231: same adoption for the legacy-TLS fallback —
                        // its redirects were walked client-side as well.
                        redirect_hops.extend(buffered.redirect_hops.iter().cloned());
                        redirect_chain
                            .extend(buffered.redirect_hops.iter().map(|h| h.url.clone()));
                        break OpFetchOutcome::Buffered(buffered);
                    }
                    Some(Err(fallback_err)) => {
                        deps.failures.push(FetchFailure {
                            url: url.clone(),
                            method: current_method.as_str().to_string(),
                            reason: fallback_err.to_string(),
                            final_url: current_url.clone(),
                            redirects: redirect_hops.clone(),
                        });
                        return Err(deno_error::JsErrorBox::generic(fallback_err.to_string()))
                    }
                    None => {
                        deps.failures.push(FetchFailure {
                            url: url.clone(),
                            method: current_method.as_str().to_string(),
                            reason: e.to_string(),
                            final_url: current_url.clone(),
                            redirects: redirect_hops.clone(),
                        });
                        return Err(deno_error::JsErrorBox::generic(e.to_string()))
                    }
                }
            }
        };

        if let Some(ref counter) = in_flight {
            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }

        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    let send_ctx = crate::diting_net::cookies::SendContext::subresource(
                        !crate::diting_net::site::is_same_site(&fetch_initiator, &parsed_url),
                    );
                    for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
                        if let Ok(s) = val.to_str() {
                            jar.set_cookie_in_context(s, &parsed_url, &send_ctx, "http");
                        }
                    }
                }
            }
        }

        if !resp.status().is_redirection() {
            break OpFetchOutcome::Live(resp);
        }

        let location_header = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location_header else {
            // 3xx without a Location header is not actually a redirect.
            break OpFetchOutcome::Live(resp);
        };

        let base = match url::Url::parse(&current_url) {
            Ok(b) => b,
            Err(_) => break OpFetchOutcome::Live(resp),
        };
        let next_url = match base.join(&location) {
            Ok(u) => u,
            Err(_) => break OpFetchOutcome::Live(resp),
        };

        // Re-validate every redirect target against the SSRF policy.
        if let Err(reason) = validate_fetch_url(&next_url) {
            let error = format!("Redirect to forbidden URL blocked: {}", reason);
            deps.failures.push(FetchFailure {
                url: url.clone(),
                method: current_method.as_str().to_string(),
                reason: error.clone(),
                final_url: next_url.to_string(),
                redirects: redirect_hops.clone(),
            });
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": next_url.to_string(),
                    "headers": {},
                    "blocked": true,
                    "error": error,
                    "redirect_chain": redirect_chain,
                })
                .to_string(),
                network: None,
            });
        }

        redirect_chain.push(next_url.to_string());
        redirect_hops.push(crate::diting_net::RedirectHop {
            url: next_url.to_string(),
            status: resp.status().as_u16(),
        });
        redirects_followed += 1;
        if redirects_followed > FETCH_REDIRECT_LIMIT {
            let error = format!("Too many redirects (>{})", FETCH_REDIRECT_LIMIT);
            deps.failures.push(FetchFailure {
                url: url.clone(),
                method: current_method.as_str().to_string(),
                reason: error.clone(),
                final_url: next_url.to_string(),
                redirects: redirect_hops.clone(),
            });
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": next_url.to_string(),
                    "headers": {},
                    "blocked": true,
                    "error": error,
                    "redirect_chain": redirect_chain,
                })
                .to_string(),
                network: None,
            });
        }

        // The CORS check applies to every tainted response, redirect
        // responses included: a cross-origin server must authorize the
        // redirect itself before the follow happens.
        if mode == "cors" && cors_tainted {
            let allowed = resp
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let allow_credentials = resp
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
                let error = format!(
                    "CORS error: redirect to '{}' blocked: Origin '{}' not in Access-Control-Allow-Origin '{}'",
                    next_url, page_origin, allowed
                );
                deps.failures.push(FetchFailure {
                    url: url.clone(),
                    method: current_method.as_str().to_string(),
                    reason: error.clone(),
                    final_url: current_url.clone(),
                    redirects: redirect_hops.clone(),
                });
                return Ok(FetchWalkOutcome {
                    json: serde_json::json!({
                        "status": 0,
                        "body": "",
                        "url": url,
                        "headers": {},
                        "corsBlocked": true,
                        "corsError": error,
                        "redirect_chain": redirect_chain,
                    })
                    .to_string(),
                    network: None,
                });
            }
        }
        cors_tainted = cors_tainted
            || request_origin(next_url.as_str())
                .map(|o| o != page_origin)
                .unwrap_or(false);

        // Browser semantics: 301/302/303 downgrade to GET with no body (303
        // unconditionally; 301/302 only when the method actually changes —
        // a GET→GET redirect keeps its header list). 307/308 preserve
        // method and body.
        let status_code = resp.status().as_u16();
        let method_downgrades = status_code == 303
            || ((status_code == 301 || status_code == 302)
                && current_method != reqwest::Method::GET
                && current_method != reqwest::Method::HEAD);
        if method_downgrades {
            current_method = reqwest::Method::GET;
            current_body.clear();
            strip_body_headers = true;
        }
        if request_origin(next_url.as_str()) != request_origin(&current_url) {
            strip_credentials = true;
        }

        current_url = next_url.to_string();
    };

    // The legacy fallback resolved (and revalidated) its redirects inside
    // the HttpClient; adopt its final URL so the CORS check and the passive
    // observers below see where the content actually came from.
    let (status, resp_headers, buffered_body): (
        u16,
        std::collections::HashMap<String, String>,
        Option<Vec<u8>>,
    ) = match &response {
        OpFetchOutcome::Live(r) => {
            let status = r.status().as_u16();
            let headers = crate::diting_net::collect_response_headers(r.headers());
            (status, headers, None)
        }
        OpFetchOutcome::Buffered(r) => {
            current_url = r.url.to_string();
            (r.status, r.headers.clone(), Some(r.body.clone()))
        }
    };

    // #224: a real wire response is never status 0 — that is the client
    // layer's marker for "no servable response" (tracker block, transport
    // refused, synthesized empty). Chrome rejects those fetches with
    // TypeError; passing the row through as a live response hands the page
    // an opaque status-0 Response or a 0-byte success it cannot tell apart
    // from the real thing, so the page's catch/toast/retry paths never run
    // (the doudian broken-pipe retries completed as exactly this shape).
    if status == 0 {
        let error =
            "request blocked or the transport produced no response (status 0)".to_string();
        deps.failures.push(FetchFailure {
            url: url.clone(),
            method: current_method.as_str().to_string(),
            reason: error.clone(),
            final_url: current_url.clone(),
            redirects: redirect_hops.clone(),
        });
        return Ok(FetchWalkOutcome {
            json: serde_json::json!({
                "status": 0,
                "body": "",
                "url": url,
                "headers": {},
                "blocked": true,
                "error": error,
                "redirect_chain": redirect_chain,
            })
            .to_string(),
            network: None,
        });
    }

    let final_is_cross_origin = request_origin(&current_url)
        .map(|o| o != page_origin)
        .unwrap_or(false);
    if final_is_cross_origin && mode == "cors" {
        let allowed = resp_headers
            .get("access-control-allow-origin")
            .map(|s| s.as_str())
            .unwrap_or("");
        let allow_credentials = resp_headers
            .get("access-control-allow-credentials")
            .map(|s| s.as_str())
            .unwrap_or("");

        if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
            // #163 diagnosability: when the response arrived from a different
            // URL than the one the script asked for, say so — a same-origin
            // request that the server bounced to a cross-origin host reads
            // as "engine misjudged a same-origin fetch" otherwise. Chrome
            // names the redirect too; the no-redirect message stays
            // byte-identical (pinned by fetch_honors_request_credentials_
            // across_origins).
            let redirect_note = if current_url != url.as_str() {
                format!(" (after redirect to '{}')", current_url)
            } else {
                String::new()
            };
            let error = if credentials == FetchCredentials::Include {
                format!("CORS error: credentialed request requires Access-Control-Allow-Origin '{}' and Access-Control-Allow-Credentials 'true'{}", page_origin, redirect_note)
            } else {
                format!("CORS error: Origin '{}' not in Access-Control-Allow-Origin '{}'{}", page_origin, allowed, redirect_note)
            };
            deps.failures.push(FetchFailure {
                    url: url.clone(),
                    method: current_method.as_str().to_string(),
                    reason: error.clone(),
                    final_url: current_url.clone(),
                    redirects: redirect_hops.clone(),
                });
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": url,
                    "headers": {},
                    "corsBlocked": true,
                    "corsError": error,
                })
                .to_string(),
                network: None,
            });
        }
    }

    // Cap the buffered body (upstream #581): the retained-for-CDP limits
    // above only gate the cache, never the allocation, and everything
    // downstream (utf8-lossy, base64) copies the full buffer again.
    // Content-Length is checked before reading; a lying or absent header
    // still runs into the per-chunk check while streaming.
    let body_limit = fetch_body_byte_limit();
    if let Some(len) = resp_headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        if len > body_limit {
            return Err(deno_error::JsErrorBox::generic(format!(
                "fetch response body too large: content-length {} exceeds limit {} bytes",
                len, body_limit
            )));
        }
    }
    let resp_bytes: Vec<u8> = match buffered_body {
        // The fallback pre-buffered its body; the per-chunk cap the
        // streaming path enforces applies to it as one shot.
        Some(bytes) => {
            if bytes.len() > body_limit {
                return Err(deno_error::JsErrorBox::generic(format!(
                    "fetch response body exceeded limit of {} bytes",
                    body_limit
                )));
            }
            bytes
        }
        None => {
            // buffered_body is Some exactly for the Buffered outcome, so
            // streaming here implies Live.
            let live = match &mut response {
                OpFetchOutcome::Live(r) => r,
                OpFetchOutcome::Buffered(_) => {
                    unreachable!("buffered bodies never take the streaming path")
                }
            };
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = live
                .chunk()
                .await
                .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?
            {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > body_limit {
                    return Err(deno_error::JsErrorBox::generic(format!(
                        "fetch response body exceeded limit of {} bytes",
                        body_limit
                    )));
                }
            }
            bytes
        }
    };
    // Chromium DevTools body policy (Chrome 152 verified, obscura #791): a
    // declared non-UTF-8 charset (GBK) decodes to text with base64Encoded=false;
    // opaque or undecodable bodies travel base64 byte-exact.
    let stored_text = crate::diting_net::decode_devtools_body(
        &resp_bytes,
        resp_headers.get("content-type").map(|s| s.as_str()),
    );
    let resp_body_base64 = BASE64.encode(&resp_bytes);

    // Hand the success network event to the driver (recorded once the walk
    // returns and OpState is reachable again), then fire the passive
    // on_response observers.
    let network = FetchNetworkEvent {
        url: url.clone(),
        method: method.clone(),
        status,
        response_headers: resp_headers.clone(),
        request_headers: final_hop_headers.clone(),
        body_size: resp_bytes.len(),
        stored_text: stored_text.clone(),
        resp_body_base64: resp_body_base64.clone(),
        final_url: current_url.clone(),
        redirects: redirect_hops.clone(),
    };

    if let Some(cbs) = callbacks.as_ref() {
        if cbs.has_response_callbacks().await {
            let info = crate::diting_net::RequestInfo {
                url: url::Url::parse(&current_url)
                    .unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                method: method.clone(),
                headers: resp_headers.clone(),
                resource_type: crate::diting_net::ResourceType::Fetch,
            };
            let net_resp = crate::diting_net::Response {
                url: url::Url::parse(&current_url)
                    .unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                status,
                headers: resp_headers.clone(),
                body: resp_bytes.to_vec(),
                redirected_from: Vec::new(),
                redirect_hops: Vec::new(),
                request_headers: final_hop_headers.clone(),
            };
            cbs.fire_response(&info, &net_resp).await;
        }
    }

    Ok(FetchWalkOutcome {
        json: serde_json::json!({
            "status": status,
            "body": stored_text.unwrap_or_default(),
            "bodyBase64": resp_body_base64,
            "url": url,
            "final_url": current_url,
            "redirected": redirects_followed > 0,
            "redirect_chain": redirect_chain,
            "headers": resp_headers,
        })
        .to_string(),
        network: Some(network),
    })
}
