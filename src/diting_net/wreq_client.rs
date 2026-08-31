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
    pub in_flight: Arc<std::sync::atomic::AtomicU32>,
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
        Self::with_proxy_and_emulation(cookie_jar, proxy_url, os_override, wreq_util::Emulation::Chrome145)
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
            in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
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
        // The stealth path must enforce the same SSRF rules as the reqwest
        // path — without this, StealthHttpClient could reach loopback/private
        // addresses that HttpClient rejects.
        crate::diting_net::client::validate_url(url, false)?;
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

        for _ in 0..20 {
            let mut req = self.select_client(&current_url).await.get(current_url.as_str());

            // Override the emulation's hardcoded Linux UA + en-US locale so the
            // advertised identity is internally consistent (UA platform must
            // match sec-ch-ua-platform; Chinese sites expect zh-CN).
            let ua = self.user_agent.read().await.clone();
            let lang = self.accept_language.read().await.clone();
            let (_, platform) = crate::diting_net::client::derive_client_hints(&ua);
            let extra = self.extra_headers.read().await;
            req = req.header("User-Agent", &ua);
            // Only set Accept-Language automatically if not overridden in extra_headers.
            if !extra.contains_key("Accept-Language") {
                req = req.header("Accept-Language", &lang);
            }
            // Only set Sec-Ch-Ua-Platform automatically if not overridden.
            // Some engines (e.g. Google with GSA UA) explicitly set this to ""
            // in extra_headers to suppress it.
            if !extra.contains_key("Sec-Ch-Ua-Platform") {
                req = req.header("Sec-Ch-Ua-Platform", &platform);
            }

            let cookie_header = self.cookie_jar.get_cookie_header(&current_url);
            if !cookie_header.is_empty() {
                req = req.header("Cookie", &cookie_header);
            }

            for (k, v) in self.extra_headers.read().await.iter() {
                req = req.header(k.as_str(), v.as_str());
            }

            self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = match send_get_with_connection_reset_retry(req, &current_url).await {
                Ok(resp) => resp,
                Err(e) => {
                    self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
            self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

            let status = resp.status();
            tracing::info!("stealth fetch {} -> status {}", current_url, status);

            for val in resp.headers().get_all("set-cookie") {
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
                if let Some(location) = resp.headers().get("location") {
                    let location_str = location.to_str().map_err(|_| {
                        NetError::Network("Invalid redirect Location".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        NetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    // A redirect must not be able to bounce the stealth client
                    // to a forbidden target (e.g. 302 -> http://127.0.0.1/).
                    crate::diting_net::client::validate_url(&next_url, false)?;
                    if next_url.scheme() == "file" {
                        return crate::diting_net::client::fetch_file_url(&next_url).await;
                    }
                    redirects.push(current_url.clone());
                    tracing::info!("stealth redirect {} -> {}", current_url, next_url);
                    current_url = next_url;
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

    pub fn active_requests(&self) -> u32 {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_network_idle(&self) -> bool {
        self.active_requests() == 0
    }
}

#[cfg(all(test, feature = "stealth"))]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use url::Url;

    use super::StealthHttpClient;
    use crate::diting_net::cookies::CookieJar;

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
