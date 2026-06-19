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
use crate::obscura_net::cookies::CookieJar;
#[cfg(feature = "stealth")]
use crate::obscura_net::client::{Response, ObscuraNetError};

#[cfg(feature = "stealth")]
pub const STEALTH_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

#[cfg(feature = "stealth")]
pub struct StealthHttpClient {
    /// Proxy-configured client. None when no proxy is set.
    proxied_client: Option<wreq::Client>,
    /// Direct-connect client (no proxy). Always present.
    direct_client: wreq::Client,
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
    fn build_stealth_client(proxy_url: Option<&str>) -> wreq::Client {
        // Issue #184: `set_default_paths()` reads OpenSSL's compile-time CA
        // paths, which only resolve on Linux. `CertStore::default()` uses wreq's
        // bundled Mozilla roots (`webpki-root-certs`), same on every platform.
        let cert_store = wreq::tls::CertStore::default();

        // The emulation OS must match the advertised User-Agent, otherwise the
        // TLS/JA3 fingerprint (OS-specific) clashes with the HTTP UA — a strong
        // anti-bot signal ("shape coherence"). Derive from AGINXBROWER_UA.
        let ua = std::env::var("AGINXBROWER_UA").unwrap_or_default();
        let os = if ua.contains("Windows") {
            wreq_util::EmulationOS::Windows
        } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
            wreq_util::EmulationOS::MacOS
        } else if ua.contains("Android") {
            wreq_util::EmulationOS::Android
        } else if ua.contains("iPhone") || ua.contains("iPad") {
            wreq_util::EmulationOS::IOS
        } else {
            wreq_util::EmulationOS::Linux
        };

        let emulation_opts = wreq_util::EmulationOption::builder()
            .emulation(wreq_util::Emulation::Chrome145)
            .emulation_os(os)
            .build();

        let mut builder = wreq::Client::builder()
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
        let proxied_client = proxy_url.map(|_| Self::build_stealth_client(proxy_url));
        // Direct client is always built (no proxy arg).
        let direct_client = Self::build_stealth_client(None);

        StealthHttpClient {
            proxied_client,
            direct_client,
            cookie_jar,
            extra_headers: RwLock::new(HashMap::new()),
            user_agent: RwLock::new(
                std::env::var("AGINXBROWER_UA").unwrap_or_else(|_| {
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36".to_string()
                }),
            ),
            accept_language: RwLock::new(
                std::env::var("AGINXBROWER_ACCEPT_LANGUAGE")
                    .unwrap_or_else(|_| "zh-CN,zh;q=0.9,en;q=0.8".to_string()),
            ),
            in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    /// Pick the client. When no proxy was configured there is only the direct
    /// client; otherwise the proxy applies to the whole page (foreign sites),
    /// so all requests go through it.
    async fn select_client(&self, _url: &Url) -> &wreq::Client {
        match &self.proxied_client {
            Some(p) => p,
            None => &self.direct_client,
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, ObscuraNetError> {
        let mut current_url = url.clone();

        if let Some(host) = current_url.host_str() {
            if crate::obscura_net::blocklist::is_blocked(host) {
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
            let (_, platform) = crate::obscura_net::client::derive_client_hints(&ua);
            req = req.header("User-Agent", &ua);
            req = req.header("Accept-Language", &lang);
            req = req.header("Sec-Ch-Ua-Platform", &platform);

            let cookie_header = self.cookie_jar.get_cookie_header(&current_url);
            if !cookie_header.is_empty() {
                req = req.header("Cookie", &cookie_header);
            }

            for (k, v) in self.extra_headers.read().await.iter() {
                req = req.header(k.as_str(), v.as_str());
            }

            self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = req.send().await.map_err(|e| {
                self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                ObscuraNetError::Network(format!("{}: {} (source: {:?})", current_url, e, e.source()))
            })?;
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
                        ObscuraNetError::Network("Invalid redirect Location".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        ObscuraNetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    redirects.push(current_url.clone());
                    tracing::info!("stealth redirect {} -> {}", current_url, next_url);
                    current_url = next_url;
                    continue;
                }
            }

            let body = resp.bytes().await.map_err(|e| {
                ObscuraNetError::Network(format!("Failed to read body: {}", e))
            })?.to_vec();

            return Ok(Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body,
                redirected_from: redirects,
            });
        }

        Err(ObscuraNetError::TooManyRedirects(url.to_string()))
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
