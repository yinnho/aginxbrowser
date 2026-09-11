#[cfg(feature = "stealth")]
use std::collections::HashMap;
#[cfg(feature = "stealth")]
use std::error::Error;
#[cfg(feature = "stealth")]
use std::sync::Arc;
#[cfg(feature = "stealth")]
use std::time::Duration;

#[cfg(feature = "stealth")]
use tokio::sync::RwLock;
#[cfg(feature = "stealth")]
use url::Url;

#[cfg(feature = "stealth")]
use crate::diting_net::cookies::CookieJar;
#[cfg(feature = "stealth")]
use crate::diting_net::client::{Response, NetError};

#[cfg(feature = "stealth")]
pub const STEALTH_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

/// The TLS fingerprint stealth builds present by default. `with_proxy_and_os`
/// constructs the matching emulation and `/health` reports this string, so
/// the wire behavior and the reported name cannot drift apart.
#[cfg(feature = "stealth")]
pub const DEFAULT_TLS_FINGERPRINT: &str = "chrome145";

/// Map a user-friendly TLS fingerprint name to a wreq `Emulation` variant.
/// Accepted values (case-insensitive): "chrome145"/"chrome", "chrome131",
/// "firefox133"/"firefox", "firefox147", "safari17_5"/"safari", "safari18",
/// "edge145"/"edge". Returns None for unknown names (caller falls back to
/// Chrome145). Only meaningful when the `stealth` feature is enabled.
#[cfg(feature = "stealth")]
pub fn parse_tls_fingerprint(s: &str) -> Option<wreq_util::Emulation> {
    match s.to_ascii_lowercase().as_str() {
        "chrome145" | "chrome" => Some(wreq_util::Emulation::Chrome145),
        "chrome131" => Some(wreq_util::Emulation::Chrome131),
        "firefox133" | "firefox" => Some(wreq_util::Emulation::Firefox133),
        "firefox147" => Some(wreq_util::Emulation::Firefox147),
        "safari17_5" | "safari" => Some(wreq_util::Emulation::Safari17_5),
        "safari18" => Some(wreq_util::Emulation::Safari18),
        "edge145" | "edge" => Some(wreq_util::Emulation::Edge145),
        _ => None,
    }
}

/// Derive the TLS emulation's OS from an advertised User-Agent. The
/// fingerprint's platform (JA3 is OS-specific) must match the UA the
/// transport sends — "shape coherence".
#[cfg(feature = "stealth")]
pub fn emulation_os_for_ua(ua: &str) -> wreq_util::EmulationOS {
    if ua.contains("Windows") {
        wreq_util::EmulationOS::Windows
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        wreq_util::EmulationOS::MacOS
    } else if ua.contains("Android") {
        wreq_util::EmulationOS::Android
    } else if ua.contains("iPhone") || ua.contains("iPad") {
        wreq_util::EmulationOS::IOS
    } else {
        wreq_util::EmulationOS::Linux
    }
}

/// Browser family + major version from a User-Agent ("Chrome/152.0.0.0" ->
/// ("chrome", 152)). Edge is matched before Chrome because an Edge UA embeds
/// both tokens; Safari's own version hides in `Version/`, not `Safari/`.
#[cfg(feature = "stealth")]
pub fn ua_browser_version(ua: &str) -> Option<(&'static str, u32)> {
    let (family, marker) = if ua.contains("Edg/") {
        ("edge", "Edg/")
    } else if ua.contains("Firefox/") {
        ("firefox", "Firefox/")
    } else if ua.contains("Chrome/") {
        ("chrome", "Chrome/")
    } else if ua.contains("Safari/") {
        ("safari", "Version/")
    } else {
        return None;
    };
    let version = ua.split(marker).nth(1)?;
    let major = version.split('.').next()?.parse().ok()?;
    Some((family, major))
}

/// (family, major version) for a TLS fingerprint name ("safari17_5" ->
/// ("safari", 17)).
#[cfg(feature = "stealth")]
fn fingerprint_family_version(name: &str) -> Option<(&'static str, u32)> {
    let lower = name.to_ascii_lowercase();
    let digits = lower.find(|c: char| c.is_ascii_digit())?;
    let family = match &lower[..digits] {
        "chrome" => "chrome",
        "firefox" => "firefox",
        "safari" => "safari",
        "edge" => "edge",
        _ => return None,
    };
    let major = lower[digits..]
        .split(['_', '.'])
        .next()?
        .parse()
        .ok()?;
    Some((family, major))
}

/// Warn when the effective UA and the TLS emulation disagree on browser
/// family or major version. An AGINXBROWSER_UA override rides the stealth
/// transport's default Chrome145 handshake, and "Chrome/152" in the UA over
/// a chrome/145 JA3+HTTP/2 setting is the kind of tell WAFs diff (taobao
/// compat report ⑤). Returns true when a mismatch was logged; `None`
/// fingerprint means the default Chrome145 handshake.
#[cfg(feature = "stealth")]
pub fn warn_on_ua_tls_mismatch(ua: &str, tls_fingerprint: Option<&str>) -> bool {
    let Some((ua_family, ua_major)) = ua_browser_version(ua) else {
        return false;
    };
    let (tls_family, tls_major) = match tls_fingerprint {
        Some(name) => match fingerprint_family_version(name) {
            Some(v) => v,
            None => return false,
        },
        None => ("chrome", 145),
    };
    if ua_family != tls_family || ua_major != tls_major {
        tracing::warn!(
            "fingerprint mismatch: UA advertises {}/{} but the TLS handshake emulates {}/{} — align AGINXBROWSER_UA with the tls_fingerprint (or drop the override)",
            ua_family,
            ua_major,
            tls_family,
            tls_major
        );
        true
    } else {
        false
    }
}

/// GETs are idempotent, so a connection reset mid-request is safe to retry
/// once. Some anti-bot frontends RST the first TLS connection from a fresh IP
/// and only serve the retry.
#[cfg(feature = "stealth")]
async fn send_get_with_connection_reset_retry(
    request: wreq::RequestBuilder,
    url: &Url,
) -> Result<wreq::Response, wreq::Error> {
    let retry = request.try_clone();
    match request.send().await {
        Err(error) if error.is_connection_reset() => {
            let Some(retry) = retry else {
                return Err(error);
            };
            tracing::debug!(%url, "retrying GET after connection reset");
            retry.send().await
        }
        result => result,
    }
}

#[cfg(feature = "stealth")]
pub struct StealthHttpClient {
    /// Proxy-configured client. None when no proxy is set.
    proxied_client: Option<wreq::Client>,
    /// Proxy client for known-blocked domains, built when no explicit proxy
    /// was configured but `AGINXBROWSER_PROXY` is set. Same per-domain
    /// fallback the reqwest transport applies, so stealth document requests
    /// reach blocked origins too.
    auto_proxied_client: Option<wreq::Client>,
    /// Direct-connect client (no proxy). Always present.
    direct_client: wreq::Client,
    /// The configured upstream proxy, kept for error reporting only (the
    /// clients already embed it) — an unreachable proxy must be named in the
    /// error, not folded into "error sending request" (obscura#491).
    proxy_url: Option<String>,
    pub cookie_jar: Arc<CookieJar>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    /// Override the emulation's built-in User-Agent. wreq's Chrome emulation
    /// hardcodes a Linux UA, which clashes with anti-bot heuristics expecting
    /// the UA to match the TLS fingerprint's advertised platform.
    pub user_agent: RwLock<String>,
    pub accept_language: RwLock<String>,
    /// When true, `validate_url` lets localhost / RFC1918 / link-local hosts
    /// through on the stealth path too. Mirrors the HttpClient field of the
    /// same name: a context built with the private-network opt-in used to
    /// open its stealth document requests while HttpClient allowed them —
    /// the same half-threaded-flag shape as obscura#793. The env var
    /// (`AGINXBROWSER_ALLOW_PRIVATE_NETWORK`) is OR'd inside `validate_url`
    /// itself, so this field only carries the per-context flag.
    pub allow_private_network: bool,
}

#[cfg(feature = "stealth")]
impl StealthHttpClient {
    pub fn new(cookie_jar: Arc<CookieJar>) -> Self {
        Self::with_proxy(cookie_jar, None)
    }

    /// Build a stealth wreq client. When `proxy_url` is Some, the SOCKS5 proxy
    /// is wired via `Proxy::http` (see note below); otherwise the client is
    /// direct-only.
    /// Build a stealth wreq client with an optional explicit OS override and a
    /// chosen TLS `emulation` (browser fingerprint). When `os_override` is Some,
    /// it takes precedence over the UA-derived OS, allowing engines like Google
    /// to use Android TLS fingerprints for GSA User-Agent requests.
    fn build_stealth_client_with_os(
        proxy_url: Option<&str>,
        os_override: Option<wreq_util::EmulationOS>,
        emulation: wreq_util::Emulation,
    ) -> wreq::Client {
        // Honor SSL_CERT_FILE / SSL_CERT_DIR (opt-in only): when set, load
        // those CA roots instead of the bundled defaults, so hosts behind a
        // private/national CA verify on the stealth path too. Unset keeps the
        // previous behavior byte-for-byte.
        let cert_store = if crate::diting_net::client::custom_cert_store_requested(
            std::env::var_os("SSL_CERT_FILE").as_deref(),
            std::env::var_os("SSL_CERT_DIR").as_deref(),
        ) {
            match wreq::tls::CertStore::builder().set_default_paths().build() {
                Ok(store) => store,
                Err(e) => {
                    tracing::warn!(
                        "SSL_CERT_FILE/SSL_CERT_DIR set but cert store failed to build ({}); using default roots",
                        e
                    );
                    wreq::tls::CertStore::default()
                }
            }
        } else {
            wreq::tls::CertStore::default()
        };

        let os = if let Some(os) = os_override {
            os
        } else {
            // The emulation OS must match the advertised User-Agent, otherwise the
            // TLS/JA3 fingerprint (OS-specific) clashes with the HTTP UA — a strong
            // anti-bot signal ("shape coherence"). Pages pass the OS derived from
            // the context's resolved UA explicitly; this env fallback applies only
            // to callers that construct the client standalone (search engines).
            emulation_os_for_ua(&std::env::var("AGINXBROWSER_UA").unwrap_or_default())
        };

        let emulation_opts = wreq_util::EmulationOption::builder()
            .emulation(emulation)
            .emulation_os(os)
            .build();

        // .no_proxy() disables wreq's implicit env/system proxy matcher
        // (HTTP_PROXY/HTTPS_PROXY/ALL_PROXY) — the engine's proxy decision is
        // the explicit `proxy_url` below, nothing else. Must precede the
        // .proxy() attach, which no_proxy() would otherwise clear.
        let mut builder = wreq::Client::builder()
            .no_proxy()
            .emulation(emulation_opts)
            .cert_store(cert_store)
            .timeout(Duration::from_secs(30))
            .redirect(wreq::redirect::Policy::none());

        if let Some(proxy) = proxy_url {
            // Proxy::all intercepts both http and https requests. Proxy::http
            // only catches plain http, so https sites (the common case) would
            // bypass the proxy entirely and connect directly — which is why
            // foreign sites behind a SOCKS5 proxy appeared unreachable. wreq's
            // SOCKS support (behind the `socks` feature) handles socks5://
            // URLs through either entry point.
            match wreq::Proxy::all(proxy) {
                Ok(p) => builder = builder.proxy(p),
                Err(e) => tracing::warn!("stealth proxy '{}' ignored: {}", proxy, e),
            }
        }

        builder.build().expect("failed to build wreq stealth client")
    }

    pub fn with_proxy(cookie_jar: Arc<CookieJar>, proxy_url: Option<&str>) -> Self {
        Self::with_proxy_and_os(cookie_jar, proxy_url, None)
    }

    /// Build a StealthHttpClient with an explicit OS override for TLS emulation.
    /// This allows Google engine to use Android TLS fingerprints matching its GSA
    /// User-Agent, so Google returns server-rendered HTML instead of JS-only pages.
    pub fn with_proxy_and_os(
        cookie_jar: Arc<CookieJar>,
        proxy_url: Option<&str>,
        os_override: Option<wreq_util::EmulationOS>,
    ) -> Self {
        Self::with_proxy_and_emulation(
            cookie_jar,
            proxy_url,
            os_override,
            parse_tls_fingerprint(DEFAULT_TLS_FINGERPRINT).unwrap_or(wreq_util::Emulation::Chrome145),
        )
    }

    /// Build a StealthHttpClient with an explicit TLS `emulation` (browser
    /// fingerprint) and optional OS override. Use this to switch between
    /// Chrome/Firefox/Safari/Edge fingerprints per request.
    pub fn with_proxy_and_emulation(
        cookie_jar: Arc<CookieJar>,
        proxy_url: Option<&str>,
        os_override: Option<wreq_util::EmulationOS>,
        emulation: wreq_util::Emulation,
    ) -> Self {
        let proxied_client = proxy_url.map(|_| Self::build_stealth_client_with_os(proxy_url, os_override, emulation));
        let direct_client = Self::build_stealth_client_with_os(None, os_override, emulation);
        let auto_proxied_client = if proxy_url.is_none() {
            crate::config::proxy_from_env().map(|p| {
                Self::build_stealth_client_with_os(Some(&p), os_override, emulation)
            })
        } else {
            None
        };

        StealthHttpClient {
            proxied_client,
            auto_proxied_client,
            direct_client,
            proxy_url: proxy_url.map(|s| s.to_string()),
            cookie_jar,
            extra_headers: RwLock::new(HashMap::new()),
            user_agent: RwLock::new(
                std::env::var("AGINXBROWSER_UA").unwrap_or_else(|_| {
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36".to_string()
                }),
            ),
            accept_language: RwLock::new(
                std::env::var("AGINXBROWSER_ACCEPT_LANGUAGE")
                    .unwrap_or_else(|_| "zh-CN,zh;q=0.9,en;q=0.8".to_string()),
            ),
            allow_private_network: false,
        }
    }

    /// Pick the client. An explicitly configured proxy applies to the whole
    /// page (foreign sites), so all requests go through it. Without one,
    /// requests go direct except known-blocked domains, which ride
    /// `AGINXBROWSER_PROXY` when set — the same fallback /fetch applies, so a
    /// stealth session page doesn't hard-fail on an origin /fetch reaches.
    async fn select_client(&self, url: &Url) -> &wreq::Client {
        if let Some(p) = &self.proxied_client {
            return p;
        }
        if let Some(p) = &self.auto_proxied_client {
            if crate::config::should_auto_proxy(url.as_str()) {
                return p;
            }
        }
        &self.direct_client
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, NetError> {
        self.fetch_inner(url, None, wreq::Method::GET, None, None, None, true).await
    }

    /// Subresource GET (band-paint image fetch, 反馈⑫ shape): like [`fetch`]
    /// but carrying the initiating document as Referer, recomputed per redirect
    /// hop by the same strict-origin-when-cross-origin trim the plain client's
    /// subresource path applies. Referer-checking image CDNs reject a bare
    /// request even with a perfect TLS fingerprint.
    pub async fn fetch_subresource(
        &self,
        url: &Url,
        referrer: Option<&str>,
    ) -> Result<Response, NetError> {
        self.fetch_inner(url, referrer, wreq::Method::GET, None, None, None, true)
            .await
    }

    /// Non-GET transport for the legacy-TLS fallback (`retry_via_legacy_tls`):
    /// the escape hatch used to be GET-only, which stranded scripted POSTs on
    /// CBC-only servers — now a connect-stage POST failure rides here with its
    /// body and Content-Type. The method name is parsed rather than passed as
    /// a type so the plain client's reqwest Method never has to unify with
    /// wreq's. `request_headers` mirrors what the scripted attempt was
    /// sending (Origin, Referer, Fetch-Metadata, client hints) — Referer-
    /// checking WAFs 403 the bare shape even after the handshake succeeds —
    /// and `include_cookies` carries the fetch credentials policy.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_with_body(
        &self,
        url: &Url,
        referrer: Option<&str>,
        method: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        request_headers: Option<&HashMap<String, String>>,
        include_cookies: bool,
    ) -> Result<Response, NetError> {
        let method = wreq::Method::from_bytes(method.as_bytes())
            .map_err(|e| NetError::Network(format!("invalid method {method:?}: {e}")))?;
        self.fetch_inner(url, referrer, method, body, content_type, request_headers, include_cookies)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_inner(
        &self,
        url: &Url,
        referrer: Option<&str>,
        method: wreq::Method,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        request_headers: Option<&HashMap<String, String>>,
        include_cookies: bool,
    ) -> Result<Response, NetError> {
        // The stealth path must enforce the same SSRF rules as the reqwest
        // path — without this, StealthHttpClient could reach loopback/private
        // addresses that HttpClient rejects. The per-context opt-in rides the
        // `allow_private_network` field (obscura#793 shape); the env var is
        // OR'd inside `validate_url`.
        crate::diting_net::client::validate_url(url, self.allow_private_network)?;
        if url.scheme() == "file" {
            return crate::diting_net::client::fetch_file_url(url).await;
        }

        let mut current_url = url.clone();

        if let Some(host) = current_url.host_str() {
            if crate::diting_net::blocklist::is_blocked(host) {
                tracing::debug!("Blocked tracker: {}", current_url);
                return Ok(Response {
                    status: 0,
                    url: current_url,
                    headers: HashMap::new(),
                    body: Vec::new(),
                    redirected_from: Vec::new(),
                });
            }
        }

        let mut redirects = Vec::new();
        let mut method = method;
        let mut body = body;
        let mut content_type = content_type;

        for _ in 0..20 {
            let mut req = self
                .select_client(&current_url)
                .await
                .request(method.clone(), current_url.as_str());

            // Override the emulation's hardcoded Linux UA + en-US locale so the
            // advertised identity is internally consistent (UA platform must
            // match sec-ch-ua-platform; Chinese sites expect zh-CN).
            let ua = self.user_agent.read().await.clone();
            let lang = self.accept_language.read().await.clone();
            let (_, platform) = crate::diting_net::client::derive_client_hints(&ua);
            // Request-local scripted headers (the fetch()/XHR escape hatch)
            // mirror what the primary reqwest attempt was sending and win
            // over both the transport defaults and extra_headers, so the
            // retry is the same request on another stack.
            let request_local = |name: &str| {
                request_headers
                    .iter()
                    .flat_map(|h| h.keys())
                    .any(|k| k.eq_ignore_ascii_case(name))
            };
            let extra = self.extra_headers.read().await;
            if !request_local("user-agent") {
                req = req.header("User-Agent", &ua);
            }
            // Only set Accept-Language automatically if not overridden in extra_headers.
            if !request_local("accept-language") && !extra.contains_key("Accept-Language") {
                req = req.header("Accept-Language", &lang);
            }
            // Only set Sec-Ch-Ua-Platform automatically if not overridden.
            // Some engines (e.g. Google with GSA UA) explicitly set this to ""
            // in extra_headers to suppress it.
            if !request_local("sec-ch-ua-platform")
                && !extra.contains_key("Sec-Ch-Ua-Platform")
            {
                req = req.header("Sec-Ch-Ua-Platform", &platform);
            }

            // Document-initiated subresource requests carry the initiator's
            // Referer — recomputed per hop (a redirect can change the
            // same/cross-origin answer); extra_headers overrides it.
            if let Some(src) = referrer {
                if !request_local("referer") && !extra.contains_key("Referer") {
                    if let Ok(source) = url::Url::parse(src) {
                        let ref_value = crate::diting_net::client::HttpClient::navigation_referrer(
                            &source,
                            &current_url,
                        );
                        if !ref_value.is_empty() {
                            req = req.header("Referer", &ref_value);
                        }
                    }
                }
            }

            let cookie_header = self.cookie_jar.get_cookie_header(&current_url);
            if include_cookies && !cookie_header.is_empty() {
                req = req.header("Cookie", &cookie_header);
            }

            for (k, v) in self.extra_headers.read().await.iter() {
                if !request_local(k) {
                    req = req.header(k.as_str(), v.as_str());
                }
            }
            // Request-local headers ride the first hop only: they were
            // computed for the hop that failed (Origin, Referer,
            // sec-fetch-site keyed to that origin), and redirect hops are
            // new requests back on the transport's own defaults.
            if let Some(headers) = request_headers.filter(|_| redirects.is_empty()) {
                for (k, v) in headers {
                    req = req.header(k.as_str(), v.as_str());
                }
            }

            // The legacy-TLS fallback rides here with a method and payload;
            // the plain GET paths pass None for both. Content-Type only on
            // methods that can carry one — a GET advertising a content-type
            // is a WAF tell, and the body drops naturally when a 3xx
            // downgrade sets `body = None` below.
            if method != wreq::Method::GET && method != wreq::Method::HEAD {
                if let Some(ct) = content_type {
                    if !request_local("content-type") {
                        req = req.header("Content-Type", ct);
                    }
                }
            }
            if let Some(b) = body {
                req = req.body(b.to_vec());
            }

            // GET/HEAD are idempotent, so the connection-reset retry applies;
            // any other method sends exactly once (a reset after the request
            // went out may already have applied server-side).
            let resp = if matches!(method, wreq::Method::GET | wreq::Method::HEAD) {
                send_get_with_connection_reset_retry(req, &current_url).await
            } else {
                req.send().await
            };
            let resp = match resp {
                Ok(resp) => resp,
                Err(e) => {
                    // Mirror the reqwest path: name an unreachable configured
                    // proxy instead of folding it into "error sending request"
                    // (obscura#491).
                    return Err(match (&self.proxy_url, e.is_connect()) {
                        (Some(proxy), true) => NetError::Network(format!(
                            "upstream proxy {} unreachable while fetching {}: {} — unset AGINXBROWSER_PROXY to connect directly",
                            proxy, current_url, e
                        )),
                        _ => NetError::Network(format!(
                            "{}: {} (source: {:?})",
                            current_url,
                            e,
                            e.source()
                        )),
                    });
                }
            };

            let status = resp.status();
            tracing::info!("stealth fetch {} -> status {}", current_url, status);

            for val in resp.headers().get_all("set-cookie") {
                if let Ok(s) = val.to_str() {
                    self.cookie_jar.set_cookie(s, &current_url);
                }
            }

            let response_headers = crate::diting_net::collect_response_headers(resp.headers());

            if status.is_redirection() {
                if let Some(location) = resp.headers().get("location") {
                    let location_str = location.to_str().map_err(|_| {
                        NetError::Network("Invalid redirect Location".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        NetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    // A redirect must not be able to bounce the stealth client
                    // to a forbidden target (e.g. 302 -> http://127.0.0.1/).
                    crate::diting_net::client::validate_url(
                        &next_url,
                        self.allow_private_network,
                    )?;
                    if next_url.scheme() == "file" {
                        return crate::diting_net::client::fetch_file_url(&next_url).await;
                    }
                    redirects.push(current_url.clone());
                    tracing::info!("stealth redirect {} -> {}", current_url, next_url);
                    current_url = next_url;
                    // Mirror the plain client (and Chrome): 301/302/303
                    // rewrite the method to GET and drop body + content-type;
                    // 307/308 preserve both.
                    if status == wreq::StatusCode::MOVED_PERMANENTLY
                        || status == wreq::StatusCode::FOUND
                        || status == wreq::StatusCode::SEE_OTHER
                    {
                        method = wreq::Method::GET;
                        body = None;
                        content_type = None;
                    }
                    continue;
                }
            }

            let body = resp.bytes().await.map_err(|e| {
                NetError::Network(format!("Failed to read body: {}", e))
            })?.to_vec();

            return Ok(Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body,
                redirected_from: redirects,
            });
        }

        Err(NetError::TooManyRedirects(url.to_string()))
    }

    pub async fn set_extra_headers(&self, headers: HashMap<String, String>) {
        *self.extra_headers.write().await = headers;
    }

    pub async fn set_user_agent(&self, ua: &str) {
        *self.user_agent.write().await = ua.to_string();
    }

    pub async fn set_accept_language(&self, lang: &str) {
        *self.accept_language.write().await = lang.to_string();
    }
}

#[cfg(all(test, feature = "stealth"))]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use url::Url;

    use super::StealthHttpClient;
    use super::{fingerprint_family_version, ua_browser_version, warn_on_ua_tls_mismatch};
    use crate::diting_net::cookies::CookieJar;

    const CHROME152_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
    const FIREFOX133_UA: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0";
    const EDGE126_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";
    const SAFARI17_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4.1 Safari/605.1.15";

    #[test]
    fn ua_browser_version_parses_family_and_major() {
        assert_eq!(
            ua_browser_version(crate::diting_net::STEALTH_USER_AGENT),
            Some(("chrome", 145))
        );
        assert_eq!(ua_browser_version(CHROME152_UA), Some(("chrome", 152)));
        assert_eq!(ua_browser_version(EDGE126_UA), Some(("edge", 126)));
        assert_eq!(ua_browser_version(FIREFOX133_UA), Some(("firefox", 133)));
        assert_eq!(ua_browser_version(SAFARI17_UA), Some(("safari", 17)));
        assert_eq!(ua_browser_version("curl/8.0"), None);
    }

    #[test]
    fn fingerprint_names_parse_to_versions() {
        assert_eq!(fingerprint_family_version("chrome145"), Some(("chrome", 145)));
        assert_eq!(
            fingerprint_family_version("Chrome131"),
            Some(("chrome", 131))
        );
        assert_eq!(
            fingerprint_family_version("safari17_5"),
            Some(("safari", 17))
        );
        assert_eq!(
            fingerprint_family_version("firefox147"),
            Some(("firefox", 147))
        );
        assert_eq!(fingerprint_family_version("edge145"), Some(("edge", 145)));
        assert_eq!(fingerprint_family_version("chrome"), None);
        assert_eq!(fingerprint_family_version("dolphin9"), None);
    }

    #[test]
    fn ua_tls_mismatch_warns_on_version_drift() {
        // Report's exact case: AGINXBROWSER_UA=Chrome/152 over the default
        // Chrome145 handshake.
        assert!(warn_on_ua_tls_mismatch(CHROME152_UA, None));
        // Family drift warns too.
        assert!(warn_on_ua_tls_mismatch(FIREFOX133_UA, None));
        assert!(warn_on_ua_tls_mismatch(CHROME152_UA, Some("firefox147")));
        // Coherent pairs stay silent.
        assert!(!warn_on_ua_tls_mismatch(
            crate::diting_net::STEALTH_USER_AGENT,
            None
        ));
        assert!(!warn_on_ua_tls_mismatch(FIREFOX133_UA, Some("firefox133")));
        // Unrecognizable UA or fingerprint: no opinion, no warning.
        assert!(!warn_on_ua_tls_mismatch("curl/8.0", None));
        assert!(!warn_on_ua_tls_mismatch(CHROME152_UA, Some("chrome")));
    }

    const PLAIN_BODY: &str = "<!DOCTYPE html><html><body><p id=\"mark\">gzip ok</p></body></html>";

    // gzip (level 9) of PLAIN_BODY, hardcoded so the fixture needs no
    // compression dependency. A wrong byte fails the assert below.
    const GZIP_BODY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xb3, 0x51,
        0x74, 0xf1, 0x77, 0x0e, 0x89, 0x0c, 0x70, 0x55, 0xc8, 0x28, 0xc9, 0xcd,
        0xb1, 0xb3, 0x81, 0x90, 0x49, 0xf9, 0x29, 0x95, 0x76, 0x36, 0x05, 0x0a,
        0x99, 0x29, 0xb6, 0x4a, 0xb9, 0x89, 0x45, 0xd9, 0x4a, 0x76, 0xe9, 0x55,
        0x99, 0x05, 0x0a, 0xf9, 0xd9, 0x36, 0xfa, 0x05, 0x76, 0x36, 0xfa, 0x10,
        0x69, 0x7d, 0xb0, 0x5a, 0x00, 0x80, 0x3d, 0x1c, 0x5f, 0x41, 0x00, 0x00,
        0x00,
    ];

    /// Serve one `Content-Encoding: gzip` response on an ephemeral port.
    async fn gzip_fixture() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        GZIP_BODY.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(GZIP_BODY).await;
                });
            }
        });

        port
    }

    // The emulation profile advertises gzip, so origins compress. Without the
    // decoder the raw gzip bytes reach the HTML parser as document text.
    // The fixture is on loopback, so this runs under the env lock with
    // AGINXBROWSER_ALLOW_PRIVATE_NETWORK set, then restores it.
    #[tokio::test]
    async fn stealth_client_decodes_gzip_response() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let port = gzip_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let result = client.fetch(&url).await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let resp = result.unwrap();
        assert_eq!(resp.text(), PLAIN_BODY, "gzip body must be decompressed");
    }

    // The stealth path must enforce the same SSRF rules as the reqwest path.
    #[tokio::test]
    async fn stealth_fetch_rejects_loopback() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        assert!(client.fetch(&url).await.is_err(), "loopback must be rejected");
    }

    // obscura#793 same shape: the per-context allow-private flag must ride
    // the stealth client, not only the reqwest one — with the env var unset,
    // the flag alone decides.
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_fetch_honors_context_private_flag() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let (port, _heads) = head_recording_fixture().await;
        let mut client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network = true;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let resp = client
            .fetch(&url)
            .await
            .expect("context flag must open the stealth path to loopback");
        assert_eq!(resp.status, 200);
    }

    // fetch_subresource carries the document as Referer under the same
    // strict-origin-when-cross-origin trim as the plain client's subresource
    // path — same-origin sends the full URL, cross-origin (a port difference
    // is an origin difference) only the origin — while a plain fetch stays
    // bare. The pump image fetch rides this: Referer-checking image CDNs
    // reject a bare request even with a perfect TLS fingerprint (反馈⑫).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_subresource_carries_trimmed_referrer() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, heads) = head_recording_fixture().await;
        let mut client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network = true;
        let same = Url::parse(&format!("http://127.0.0.1:{port}/same.png")).unwrap();
        let cross = Url::parse(&format!("http://127.0.0.1:{port}/cross.png")).unwrap();
        let bare = Url::parse(&format!("http://127.0.0.1:{port}/bare.png")).unwrap();
        let results = vec![
            client
                .fetch_subresource(&same, Some(&format!("http://127.0.0.1:{port}/doc.html#frag")))
                .await,
            client
                .fetch_subresource(&cross, Some("http://other.example:9/doc.html"))
                .await,
            client.fetch(&bare).await,
        ];
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        for r in results {
            assert_eq!(r.expect("fixture fetch").status, 200);
        }
        let heads = heads.lock().unwrap();
        let head_of = |path: &str| {
            heads
                .iter()
                .find(|h| h.contains(&format!("GET {path} ")))
                .unwrap_or_else(|| panic!("no request for {path}: {heads:?}"))
                .clone()
        };
        let same_head = head_of("/same.png");
        assert!(
            same_head
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("referer: http://127.0.0.1:{port}/doc.html"))),
            "same-origin subresource must carry the full document URL (fragment stripped): {same_head}"
        );
        let cross_head = head_of("/cross.png");
        assert!(
            cross_head
                .lines()
                .any(|l| l.eq_ignore_ascii_case("referer: http://other.example:9/")),
            "cross-origin subresource must trim to the origin: {cross_head}"
        );
        let bare_head = head_of("/bare.png");
        assert!(
            !bare_head.to_ascii_lowercase().contains("referer:"),
            "plain fetch must not invent a Referer: {bare_head}"
        );
    }

    /// Serve 200s on an ephemeral port, recording each request's raw head
    /// (request line + all header lines) into the shared vec.
    async fn head_recording_fixture() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let heads2 = heads.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let h = heads2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let Ok(n) = stream.read(&mut buf).await else { return };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let head = req.split("\r\n\r\n").next().unwrap_or("").to_string();
                    h.lock().unwrap().push(head);
                    let body = "<!DOCTYPE html><html><body><p>ok</p></body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, heads)
    }

    /// Read one full HTTP request (head until the blank line, then the
    /// declared Content-Length of body) off the stream, as "head\nbody".
    async fn read_request(
        stream: &mut tokio::net::TcpStream,
    ) -> Option<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        for _ in 0..16 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let clen = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + clen {
                    return Some(format!(
                        "{head}\n{}",
                        String::from_utf8_lossy(&buf[pos + 4..])
                    ));
                }
            }
        }
        Some(String::from_utf8_lossy(&buf).to_string())
    }

    /// Recording server for method-semantics tests: every request (head +
    /// body) lands in the shared vec; `/r301` and `/r307` answer a redirect
    /// to `/land`, everything else a plain 200.
    async fn method_recording_fixture() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log2 = log.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let l = log2.clone();
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut stream).await else { return };
                    l.lock().unwrap().push(req.clone());
                    let path = req.split(' ').nth(1).unwrap_or("").to_string();
                    let resp = match path.as_str() {
                        "/r301" => "HTTP/1.1 301 Moved Permanently\r\nlocation: /land\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string(),
                        "/r307" => "HTTP/1.1 307 Temporary Redirect\r\nlocation: /land\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string(),
                        _ => "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string(),
                    };
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, log)
    }

    /// The legacy-TLS fallback's POST transport: method, body and declared
    /// Content-Type must all reach the wire (`fetch_with_body` used to be
    /// GET-only — the taobao seller-backend shape this exists for).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_post_rides_method_body_and_content_type() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let mut client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network = true;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/echo")).unwrap();
        let result = client
            .fetch_with_body(
                &url,
                None,
                "POST",
                Some(b"title=%E6%B5%8B%E8%AF%95".as_slice()),
                Some("application/x-www-form-urlencoded"),
                None,
                true,
            )
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.expect("fixture POST").status, 200);
        let log = log.lock().unwrap();
        let req = log
            .iter()
            .find(|r| r.starts_with("POST /echo "))
            .unwrap_or_else(|| panic!("no POST /echo recorded: {log:?}"));
        assert!(
            req.lines()
                .any(|l| l.eq_ignore_ascii_case("content-type: application/x-www-form-urlencoded")),
            "declared content-type must ride the POST: {req}"
        );
        assert!(
            req.ends_with("title=%E6%B5%8B%E8%AF%95"),
            "body must ride the POST verbatim: {req}"
        );
    }

    /// Redirect method semantics on the stealth transport must mirror the
    /// plain client (and Chrome): 301 rewrites the POST to a bodyless GET,
    /// 307 preserves method + body.
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_post_redirect_downgrades_301_preserves_307() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let mut client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network = true;

        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();
        let r1 = client
            .fetch_with_body(&mk("r301"), None, "POST", Some(b"a=1".as_slice()), Some("application/x-www-form-urlencoded"), None, true)
            .await;
        let r2 = client
            .fetch_with_body(&mk("r307"), None, "POST", Some(b"a=1".as_slice()), Some("application/x-www-form-urlencoded"), None, true)
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(r1.expect("301 walk").status, 200);
        assert_eq!(r2.expect("307 walk").status, 200);

        let log = log.lock().unwrap();
        let landed = log
            .iter()
            .filter(|r| r.contains(" /land "))
            .collect::<Vec<_>>();
        assert_eq!(landed.len(), 2, "both walks must land: {log:?}");
        let downgraded = landed
            .iter()
            .find(|r| r.starts_with("GET /land "))
            .unwrap_or_else(|| panic!("301 must land as GET: {log:?}"));
        assert!(
            !downgraded.to_ascii_lowercase().contains("content-type:"),
            "the 301 GET must not carry the POST's content-type: {downgraded}"
        );
        assert!(
            !downgraded.ends_with("a=1"),
            "the 301 GET must drop the body: {downgraded}"
        );
        let preserved = landed
            .iter()
            .find(|r| r.starts_with("POST /land "))
            .unwrap_or_else(|| panic!("307 must land as POST: {log:?}"));
        assert!(
            preserved.ends_with("a=1"),
            "the 307 POST must keep its body: {preserved}"
        );
    }

    /// The scripted escape hatch must send the same request, not a bare one:
    /// the rebuilt per-hop headers (Origin, Referer, Fetch-Metadata) reach
    /// the wire and win over the transport's defaults, while the credentials
    /// policy gates the cookie jar — a `credentials: 'omit'` POST rides the
    /// legacy transport without leaking the session cookie (the taobao
    /// seller-backend receipts proved the headered variant end to end).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn scripted_request_headers_ride_and_credentials_gate_cookies() {
        use std::collections::HashMap;
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let jar = Arc::new(CookieJar::new());
        let mut client = StealthHttpClient::new(jar.clone());
        client.allow_private_network = true;
        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();
        jar.set_cookie("sid=legacy", &mk("post"));

        let headers = HashMap::from([
            ("Origin".to_string(), "https://shop.example.com".to_string()),
            (
                "Referer".to_string(),
                "https://shop.example.com/sell/post.htm".to_string(),
            ),
            ("sec-fetch-site".to_string(), "same-origin".to_string()),
        ]);
        let include = client
            .fetch_with_body(
                &mk("post"),
                None,
                "POST",
                Some(b"a=1".as_slice()),
                Some("application/x-www-form-urlencoded"),
                Some(&headers),
                true,
            )
            .await;
        let omit = client
            .fetch_with_body(
                &mk("omit"),
                None,
                "POST",
                Some(b"a=1".as_slice()),
                None,
                Some(&headers),
                false,
            )
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(include.expect("headered POST").status, 200);
        assert_eq!(omit.expect("omitted-credentials POST").status, 200);

        let log = log.lock().unwrap();
        let wire = |path: &str| {
            log.iter()
                .find(|r| r.starts_with(&format!("POST /{path} ")))
                .unwrap_or_else(|| panic!("no POST /{path} recorded: {log:?}"))
                .to_ascii_lowercase()
        };
        let included = wire("post");
        for want in [
            "origin: https://shop.example.com",
            "referer: https://shop.example.com/sell/post.htm",
            "sec-fetch-site: same-origin",
            "cookie: sid=legacy",
        ] {
            assert!(
                included.contains(want),
                "the retry must mirror the scripted request, missing {want:?}: {included}"
            );
        }
        let omitted = wire("omit");
        assert!(
            !omitted.contains("cookie:"),
            "credentials:'omit' must not leak the jar cookie: {omitted}"
        );
    }

    // set_extra_headers must reach the wire per-request, and an extras
    // override must win over the client's default Accept-Language (the
    // suppression contract the per-hop merge loop implements).
    #[allow(clippy::await_holding_lock)] // env-lock guard held for the fixture fetch, as above
    #[tokio::test]
    async fn set_extra_headers_reach_the_wire() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, heads) = head_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client
            .set_extra_headers(
                [("x-diting-test".to_string(), "abc".to_string()), ("Accept-Language".to_string(), "ja".to_string())]
                    .into_iter()
                    .collect(),
            )
            .await;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let result = client.fetch(&url).await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let resp = result.expect("fixture fetch");
        assert_eq!(resp.status, 200);
        let heads = heads.lock().unwrap();
        let head = heads.first().expect("fixture saw the request").to_lowercase();
        assert!(
            head.contains("x-diting-test: abc"),
            "custom extra header must land on the wire, got:\n{head}"
        );
        assert!(
            head.contains("accept-language: ja"),
            "extras Accept-Language must override the default, got:\n{head}"
        );
    }
}
