use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Client, Method};
use tokio::sync::RwLock;
use url::Url;

use crate::obscura_net::cookies::CookieJar;

#[derive(Debug, Clone)]
pub struct Response {
    pub url: Url,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub redirected_from: Vec<Url>,
}

impl Response {
    /// Decode the body as text, honoring the response charset.
    ///
    /// Uses the HTTP `Content-Type` header's `charset=` parameter, then for
    /// HTML responses falls back to sniffing `<meta charset>` in the first
    /// 1KB, then UTF-8. Mirrors browser behaviour per the HTML5 spec.
    pub fn text(&self) -> String {
        if self.is_html() {
            crate::obscura_net::encoding::decode_response(&self.body, self.content_type())
        } else {
            crate::obscura_net::encoding::decode_non_html(&self.body, self.content_type())
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn is_html(&self) -> bool {
        self.content_type()
            .map(|ct| ct.contains("text/html"))
            .unwrap_or(false)
    }
}

/// Process-wide opt-in via env var. Older flow that issue #4 introduced. The
/// new `--allow-private-network` CLI flag (issue #33) sets a per-client field
/// that is OR'd with this so existing scripts and Docker setups that pin the
/// env var keep working unchanged.
pub fn env_allows_private_network() -> bool {
    matches!(
        std::env::var("OBSCURA_ALLOW_PRIVATE_NETWORK")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn validate_url(url: &Url, allow_private_network: bool) -> Result<(), ObscuraNetError> {
    let allow_private_network = allow_private_network || env_allows_private_network();
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" && scheme != "file" {
        return Err(ObscuraNetError::Network(format!(
            "Forbidden URL scheme '{}' - only http, https, and file are allowed",
            scheme
        )));
    }

    if scheme == "file" || allow_private_network {
        return Ok(());
    }

    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(ip) => {
                if ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_documentation()
                {
                    return Err(ObscuraNetError::Network(format!(
                        "Access to private/internal IP address {} is not allowed",
                        ip
                    )));
                }
            }
            url::Host::Ipv6(ip) => {
                if ip.is_loopback() || ip.is_unicast_link_local() {
                    return Err(ObscuraNetError::Network(format!(
                        "Access to private/internal IPv6 address {} is not allowed",
                        ip
                    )));
                }
                // Check for IPv4-mapped IPv6 addresses (::ffff:x.x.x.x)
                if let Some(ipv4) = ip.to_ipv4() {
                    if ipv4.is_loopback() || ipv4.is_private() || ipv4.is_link_local()
                        || ipv4.is_broadcast() || ipv4.is_documentation()
                    {
                        return Err(ObscuraNetError::Network(format!(
                            "Access to private/internal IP address {} (mapped from IPv6) is not allowed",
                            ipv4
                        )));
                    }
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(ObscuraNetError::Network(format!(
                        "Access to localhost domain '{}' is not allowed",
                        domain
                    )));
                }
            }
        }
    }

    Ok(())
}

/// DNS rebinding protection: resolve the domain and verify that none of the
/// resulting IPs are private/internal. This catches attacks where a domain
/// initially resolves to a public IP (passing `validate_url`) but then
/// rebinds to an internal address.
///
/// Skip when a proxy is configured — the proxy handles DNS resolution itself.
async fn validate_url_rebinding(url: &Url, has_proxy: bool) -> Result<(), ObscuraNetError> {
    if has_proxy || env_allows_private_network() {
        return Ok(());
    }
    let Some(url::Host::Domain(domain)) = url.host() else {
        return Ok(()); // Direct IPs already checked by validate_url.
    };
    let lookup = format!("{}:{}", domain, url.port().unwrap_or(80));
    match tokio::net::lookup_host(&lookup).await {
        Ok(addrs) => {
            for addr in addrs {
                let ip = addr.ip();
                let is_internal = match ip {
                    std::net::IpAddr::V4(v4) => {
                        v4.is_loopback() || v4.is_private() || v4.is_link_local()
                    }
                    std::net::IpAddr::V6(v6) => {
                        v6.is_loopback() || v6.is_unicast_link_local()
                    }
                };
                if is_internal {
                    return Err(ObscuraNetError::Network(format!(
                        "DNS rebinding blocked: {} resolved to private/internal IP {}",
                        domain, ip
                    )));
                }
            }
        }
        Err(e) => {
            // DNS resolution failure — don't block, let the actual request
            // fail naturally with a more specific error.
            tracing::debug!("DNS pre-check for {} failed: {}", domain, e);
        }
    }
    Ok(())
}

async fn fetch_file_url(url: &Url) -> Result<Response, ObscuraNetError> {
    let path = url
        .to_file_path()
        .map_err(|_| ObscuraNetError::Network("Invalid file URL".to_string()))?;
    let body = tokio::fs::read(&path)
        .await
        .map_err(|e| ObscuraNetError::Network(format!("Failed to read file: {}", e)))?;

    let mut headers = HashMap::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ct = match ext.to_lowercase().as_str() {
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" | "mjs" => "application/javascript",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "ico" => "image/x-icon",
            _ => "application/octet-stream",
        };
        headers.insert("content-type".to_string(), ct.to_string());
    }

    Ok(Response {
        url: url.clone(),
        status: 200,
        headers,
        body,
        redirected_from: Vec::new(),
    })
}

pub struct ObscuraHttpClient {
    client: tokio::sync::OnceCell<Client>,
    /// Direct-connect client (no proxy). Built once on first use.
    direct_client: tokio::sync::OnceCell<Client>,
    proxy_url: Option<String>,
    pub cookie_jar: Arc<CookieJar>,
    pub user_agent: RwLock<String>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    pub timeout: Duration,
    pub in_flight: Arc<std::sync::atomic::AtomicU32>,
    pub block_trackers: bool,
    /// When true, `validate_url` lets localhost / RFC1918 / link-local addresses
    /// through in addition to the `OBSCURA_ALLOW_PRIVATE_NETWORK` env var.
    /// Set via `--allow-private-network` on the CLI (issue #33).
    pub allow_private_network: bool,
}

impl ObscuraHttpClient {
    pub fn new() -> Self {
        Self::with_cookie_jar(Arc::new(CookieJar::new()))
    }

    pub fn with_cookie_jar(cookie_jar: Arc<CookieJar>) -> Self {
        Self::with_options(cookie_jar, None)
    }

    pub fn with_options(cookie_jar: Arc<CookieJar>, proxy_url: Option<&str>) -> Self {
        Self::with_full_options(cookie_jar, proxy_url, false)
    }

    pub fn with_full_options(
        cookie_jar: Arc<CookieJar>,
        proxy_url: Option<&str>,
        allow_private_network: bool,
    ) -> Self {
        ObscuraHttpClient {
            client: tokio::sync::OnceCell::new(),
            direct_client: tokio::sync::OnceCell::new(),
            proxy_url: proxy_url.map(|s| s.to_string()),
            cookie_jar,
            user_agent: RwLock::new(
                std::env::var("AGINXBROWSER_UA").unwrap_or_else(|_| {
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36".to_string()
                }),
            ),
            extra_headers: RwLock::new(HashMap::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            timeout: Duration::from_secs(30),
            block_trackers: false,
            allow_private_network,
        }
    }

    async fn get_client(&self) -> &Client {
        self.client.get_or_init(|| async {
            let mut builder = Client::builder()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(false);
                // No .gzip()/.brotli(): without these reqwest does not
                // advertise Accept-Encoding, so servers reply with plain text
                // and we read raw bytes reliably. reqwest's auto-decode fails
                // when a server ignores our Accept-Encoding and returns a
                // mismatched encoding (Baidu sends br after gzip is advertised).

            if let Some(ref proxy) = self.proxy_url {
                if let Ok(p) = reqwest::Proxy::all(proxy.as_str()) {
                    builder = builder.proxy(p);
                }
            }

            builder.build().expect("failed to build HTTP client")
        }).await
    }

    /// Build (once) a direct-connect client with no upstream proxy.
    async fn get_direct_client(&self) -> &Client {
        self.direct_client.get_or_init(|| async {
            Client::builder()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(false)
                .build()
                .expect("failed to build direct HTTP client")
        }).await
    }

    /// Pick the client for this request. When a proxy is configured, route
    /// through it (foreign-site mode); otherwise connect directly.
    async fn get_client_for(&self) -> &Client {
        if self.proxy_url.is_some() {
            self.get_client().await
        } else {
            self.get_direct_client().await
        }
    }

    /// Read-only accessor for the proxy URL the client was configured with
    /// (if any). Exposed so callers outside the `obscura-net` crate — notably
    /// `op_fetch_url` in `obscura-js` (#139) — can route their own reqwest
    /// requests through the same upstream proxy.
    pub fn proxy_url(&self) -> Option<&str> {
        self.proxy_url.as_deref()
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, ObscuraNetError> {
        self.fetch_with_method(Method::GET, url, None).await
    }

    pub async fn post_form(&self, url: &Url, body: &str) -> Result<Response, ObscuraNetError> {
        self.fetch_with_method(Method::POST, url, Some(body.as_bytes().to_vec())).await
    }

    pub async fn fetch_with_method(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<Vec<u8>>,
    ) -> Result<Response, ObscuraNetError> {
        validate_url(url, self.allow_private_network)?;
        validate_url_rebinding(url, self.proxy_url.is_some()).await?;

        if url.scheme() == "file" {
            return fetch_file_url(url).await;
        }

        let mut method = initial_method;
        let mut body = initial_body;
        if self.block_trackers {
            if let Some(host) = url.host_str() {
                if crate::obscura_net::blocklist::is_blocked(host) {
                    tracing::debug!("Blocked tracker: {}", url);
                    return Ok(Response {
                        status: 0,
                        url: url.clone(),
                        headers: HashMap::new(),
                        body: Vec::new(),
                        redirected_from: Vec::new(),
                    });
                }
            }
        }

        let mut current_url = url.clone();
        let mut redirects = Vec::new();
        let max_redirects = 20;

        for _redirect_count in 0..max_redirects {
            let ua = self.user_agent.read().await.clone();
            let (sec_ch_ua, platform) = derive_client_hints(&ua);
            let mut headers = HeaderMap::new();
            headers.insert(USER_AGENT, HeaderValue::from_str(&ua).unwrap_or_else(|_| {
                HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
            }));
            headers.insert(
                reqwest::header::ACCEPT,
                HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"),
            );
            headers.insert(
                reqwest::header::ACCEPT_LANGUAGE,
                HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
            );
            headers.insert(
                HeaderName::from_static("sec-ch-ua"),
                HeaderValue::from_str(&sec_ch_ua).unwrap(),
            );
            headers.insert(
                HeaderName::from_static("sec-ch-ua-mobile"),
                HeaderValue::from_static("?0"),
            );
            headers.insert(
                HeaderName::from_static("sec-ch-ua-platform"),
                HeaderValue::from_str(&platform).unwrap(),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-dest"),
                HeaderValue::from_static("document"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-mode"),
                HeaderValue::from_static("navigate"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-site"),
                HeaderValue::from_static("none"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-user"),
                HeaderValue::from_static("?1"),
            );
            headers.insert(
                HeaderName::from_static("upgrade-insecure-requests"),
                HeaderValue::from_static("1"),
            );

            let cookie_header = self.cookie_jar.get_cookie_header(&current_url);
            tracing::debug!(
                "Cookie header for {}: {} cookies ({} bytes)",
                current_url.host_str().unwrap_or("?"),
                cookie_header.split("; ").filter(|s| !s.is_empty()).count(),
                cookie_header.len(),
            );
            if !cookie_header.is_empty() {
                match HeaderValue::from_str(&cookie_header) {
                    Ok(val) => {
                        headers.insert(reqwest::header::COOKIE, val);
                    }
                    Err(_) => {
                        let filtered: String = cookie_header
                            .split("; ")
                            .filter(|pair| HeaderValue::from_str(pair).is_ok())
                            .collect::<Vec<_>>()
                            .join("; ");
                        if !filtered.is_empty() {
                            if let Ok(val) = HeaderValue::from_str(&filtered) {
                                headers.insert(reqwest::header::COOKIE, val);
                            }
                        }
                        tracing::debug!(
                            "Cookie header invalid chars, filtered {} -> {} bytes",
                            cookie_header.len(), filtered.len(),
                        );
                    }
                }
            }

            for (k, v) in self.extra_headers.read().await.iter() {
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(v),
                ) {
                    headers.insert(name, val);
                }
            }

            let mut req_builder = self.get_client_for().await.request(method.clone(), current_url.as_str())
                .headers(headers);

            if let Some(ref b) = body {
                if method == Method::POST {
                    req_builder = req_builder.header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    );
                }
                req_builder = req_builder.body(b.clone());
            }

            self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = req_builder.send().await.map_err(|e| {
                self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                ObscuraNetError::Network(format!("{}: {}", current_url, e))
            })?;
            self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

            let status = resp.status();

            for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
                if let Ok(s) = val.to_str() {
                    self.cookie_jar.set_cookie(s, &current_url);
                }
            }

            let response_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
                .collect();

            if status.is_redirection() {
                if let Some(location) = resp.headers().get(reqwest::header::LOCATION) {
                    let location_str = location.to_str().map_err(|_| {
                        ObscuraNetError::Network("Invalid redirect Location header".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        ObscuraNetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    validate_url(&next_url, self.allow_private_network)?;
                    validate_url_rebinding(&next_url, self.proxy_url.is_some()).await?;
                    redirects.push(current_url.clone());
                    current_url = next_url;
                    if status == reqwest::StatusCode::MOVED_PERMANENTLY
                        || status == reqwest::StatusCode::FOUND
                        || status == reqwest::StatusCode::SEE_OTHER
                    {
                        method = Method::GET;
                        body = None;
                    }
                    continue;
                }
            }

            let body_bytes = resp.bytes().await.map_err(|e| {
                tracing::warn!("body read failed for {}: {} (status={}, ctype={:?})", current_url, e, status, response_headers.get("content-type"));
                ObscuraNetError::Network(format!("Failed to read body: {}", e))
            })?.to_vec();

            let response = Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body: body_bytes,
                redirected_from: redirects,
            };

            return Ok(response);
        }

        Err(ObscuraNetError::TooManyRedirects(current_url.to_string()))
    }

    pub async fn set_user_agent(&self, ua: &str) {
        *self.user_agent.write().await = ua.to_string();
    }

    pub async fn set_extra_headers(&self, headers: HashMap<String, String>) {
        *self.extra_headers.write().await = headers;
    }

    pub fn active_requests(&self) -> u32 {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_network_idle(&self) -> bool {
        self.active_requests() == 0
    }
}

impl Default for ObscuraHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive `sec-ch-ua` and `sec-ch-ua-platform` from the User-Agent string so
/// the client hints stay consistent with the advertised UA. Anti-bot systems
/// (WeChat, etc.) flag mismatches like a macOS UA paired with a "Linux"
/// sec-ch-ua-platform or a version drift between UA and sec-ch-ua.
///
/// Returns `(sec_ch_ua_header, sec_ch_ua_platform_header)`.
pub fn derive_client_hints(ua: &str) -> (String, String) {
    // Major version: first \d+ after "Chrome/".
    let version = ua
        .split("Chrome/")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(145);

    let platform = if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        "\"macOS\""
    } else if ua.contains("Windows") {
        "\"Windows\""
    } else if ua.contains("iPhone") || ua.contains("Android") {
        "\"Android\""
    } else {
        "\"Linux\""
    };

    let sec_ch_ua = format!(
        "\"Chromium\";v=\"{}\", \"Not;A=Brand\";v=\"24\", \"Google Chrome\";v=\"{}\"",
        version, version
    );
    (sec_ch_ua, platform.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum ObscuraNetError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Too many redirects: {0}")]
    TooManyRedirects(String),

    #[error("Request blocked: {0}")]
    Blocked(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_client_hints_chrome_version() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
        let (ch_ua, platform) = derive_client_hints(ua);
        assert!(ch_ua.contains(r#""Chromium";v="145""#));
        assert!(ch_ua.contains(r#""Google Chrome";v="145""#));
        assert_eq!(platform, r#""macOS""#);
    }

    #[test]
    fn derive_client_hints_windows() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        let (ch_ua, platform) = derive_client_hints(ua);
        assert!(ch_ua.contains(r#""Chromium";v="131""#));
        assert_eq!(platform, r#""Windows""#);
    }

    #[test]
    fn derive_client_hints_linux() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
        let (_, platform) = derive_client_hints(ua);
        assert_eq!(platform, r#""Linux""#);
    }

    #[test]
    fn derive_client_hints_android() {
        let ua = "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Mobile Safari/537.36";
        let (_, platform) = derive_client_hints(ua);
        assert_eq!(platform, r#""Android""#);
    }

    #[test]
    fn derive_client_hints_defaults_when_no_version() {
        // No Chrome/ token → falls back to 145.
        let ua = "Mozilla/5.0 (Macintosh) Gecko Firefox/120.0";
        let (ch_ua, _) = derive_client_hints(ua);
        assert!(ch_ua.contains(r#""Chromium";v="145""#));
    }
}
