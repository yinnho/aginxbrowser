use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Client, Method};
use tokio::sync::RwLock;
use url::Url;

use crate::diting_net::cookies::CookieJar;

/// A reqwest builder with reqwest's implicit system/env proxy matcher turned
/// off. Every HTTP client the engine builds goes through here.
///
/// reqwest reads `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` from the
/// environment by default (loopback destinations are exempt, like Chrome's
/// implicit localhost bypass), which silently routes engine traffic through
/// a proxy the operator never configured in the engine — `use_proxy:false`
/// fetches, search's direct-first tier, robots checks, downloads, all of
/// it. When that env proxy dies, every public fetch fails with an error
/// that never mentions a proxy (obscura#491). The engine's proxy decision
/// is explicit instead: `AGINXBROWSER_PROXY` / the context `proxy_url`,
/// attached with `.proxy()` at the call sites — and `.no_proxy()` must come
/// BEFORE that attach, since it also clears any already-pushed proxy.
pub(crate) fn reqwest_builder_no_env_proxy() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy()
}

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
            crate::diting_net::encoding::decode_response(&self.body, self.content_type())
        } else {
            crate::diting_net::encoding::decode_non_html(&self.body, self.content_type())
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

/// CDP `Network.ResourceType`-shaped label for a request. Drives
/// `RequestInfo.resource_type` and the page's NetworkEvent kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Document,
    Script,
    Stylesheet,
    Image,
    Font,
    Xhr,
    Fetch,
    Other,
}

impl ResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Script => "Script",
            Self::Stylesheet => "Stylesheet",
            Self::Image => "Image",
            Self::Font => "Font",
            Self::Xhr => "XHR",
            Self::Fetch => "Fetch",
            Self::Other => "Other",
        }
    }
}

/// A request about to be sent (or just answered), as seen by an
/// on_request / on_response observer. Headers are the fully-built set the
/// transport sent, lowercased like `Response.headers`.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub url: Url,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub resource_type: ResourceType,
}

pub type RequestCallback = Arc<dyn Fn(&RequestInfo) + Send + Sync>;
pub type ResponseCallback = Arc<dyn Fn(&RequestInfo, &Response) + Send + Sync>;

/// Page-scoped store for the passive on_request/on_response callbacks (upstream
/// issue #408). Each `Page` owns one, so a callback never fires for another
/// page's requests and dies with its page. The HTTP client itself stays
/// callback-free; page-driven fetches pass the page's registry in. Ids keep
/// the `u64` shape upstream established on `Page::on_request`/`on_response`.
pub struct CallbackRegistry {
    on_request: RwLock<Vec<(u64, RequestCallback)>>,
    on_response: RwLock<Vec<(u64, ResponseCallback)>>,
    id_counter: std::sync::atomic::AtomicU64,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        CallbackRegistry {
            on_request: RwLock::new(Vec::new()),
            on_response: RwLock::new(Vec::new()),
            id_counter: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Register a request callback; the returned id detaches it via
    /// `remove_request`. Sync like the pre-registry push path: registration
    /// happens from `Page` setup where no reader holds the lock, so
    /// `try_write` cannot fail there.
    pub fn add_request(&self, cb: RequestCallback) -> u64 {
        let id = self.next_id();
        if let Ok(mut v) = self.on_request.try_write() {
            v.push((id, cb));
        }
        id
    }

    /// Register a response callback; see `add_request`.
    pub fn add_response(&self, cb: ResponseCallback) -> u64 {
        let id = self.next_id();
        if let Ok(mut v) = self.on_response.try_write() {
            v.push((id, cb));
        }
        id
    }

    /// Detach a request callback. Returns true when the id was found and
    /// removed, so a double detach is a visible no-op.
    pub fn remove_request(&self, id: u64) -> bool {
        match self.on_request.try_write() {
            Ok(mut v) => {
                let before = v.len();
                v.retain(|(cid, _)| *cid != id);
                v.len() != before
            }
            Err(_) => false,
        }
    }

    /// Detach a response callback; see `remove_request`.
    pub fn remove_response(&self, id: u64) -> bool {
        match self.on_response.try_write() {
            Ok(mut v) => {
                let before = v.len();
                v.retain(|(cid, _)| *cid != id);
                v.len() != before
            }
            Err(_) => false,
        }
    }

    /// True when at least one request callback is registered. Lets fire sites
    /// skip building a `RequestInfo` when nobody listens.
    pub async fn has_request_callbacks(&self) -> bool {
        !self.on_request.read().await.is_empty()
    }

    /// True when at least one response callback is registered.
    pub async fn has_response_callbacks(&self) -> bool {
        !self.on_response.read().await.is_empty()
    }

    pub async fn fire_request(&self, info: &RequestInfo) {
        for (_, cb) in self.on_request.read().await.iter() {
            cb(info);
        }
    }

    pub async fn fire_response(&self, info: &RequestInfo, resp: &Response) {
        for (_, cb) in self.on_response.read().await.iter() {
            cb(info, resp);
        }
    }
}

impl Default for CallbackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide opt-in via env var. Older flow that issue #4 introduced. The
/// new `--allow-private-network` CLI flag (issue #33) sets a per-client field
/// that is OR'd with this so existing scripts and Docker setups that pin the
/// env var keep working unchanged.
/// True when SSL_CERT_FILE / SSL_CERT_DIR point at a custom CA bundle.
/// Empty strings count as unset — some environments export them empty.
pub(crate) fn custom_cert_store_requested(
    cert_file: Option<&std::ffi::OsStr>,
    cert_dir: Option<&std::ffi::OsStr>,
) -> bool {
    fn present(v: Option<&std::ffi::OsStr>) -> bool {
        v.is_some_and(|s| !s.is_empty())
    }
    present(cert_file) || present(cert_dir)
}

pub fn env_allows_private_network() -> bool {
    matches!(
        std::env::var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// True when `ip` must never be the target of an outbound request from the
/// engine: loopback, RFC1918 private, link-local (incl. the 169.254.169.254
/// cloud-metadata endpoint), broadcast, documentation, the unspecified address
/// (0.0.0.0 / ::, which the OS routes to localhost), IPv6 unique-local
/// (fc00::/7), and any IPv4-mapped/compatible IPv6 form of the above.
/// Centralizes the SSRF deny-set so the literal-host check and the
/// DNS-resolution check (`SsrfGuardResolver`) can never disagree.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
            {
                return true;
            }
            // Unwrap IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d)
            // forms and re-check the embedded v4 so e.g. [::ffff:127.0.0.1] or
            // [::ffff:169.254.169.254] cannot slip past the v6 arm.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return is_forbidden_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

/// reqwest DNS resolver that performs the lookup and then rejects the whole
/// request if ANY resolved address is in the SSRF deny-set. This closes the
/// DNS-rebinding bypass a host-string check alone cannot: a public name that
/// resolves to 127.0.0.1 / 169.254.169.254 / an RFC1918 address is blocked at
/// connect time, using the very addresses reqwest will dial. When private
/// access is permitted (`--allow-private-network` or
/// `AGINXBROWSER_ALLOW_PRIVATE_NETWORK`) the lookup passes through unfiltered.
pub struct SsrfGuardResolver {
    allow_private: bool,
}

impl SsrfGuardResolver {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

impl Resolve for SsrfGuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow_private || env_allows_private_network();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            if !allow {
                if let Some(bad) = addrs.iter().find(|sa| is_forbidden_ip(sa.ip())) {
                    return Err(format!(
                        "SSRF blocked: '{}' resolves to forbidden address {}",
                        host,
                        bad.ip()
                    )
                    .into());
                }
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

pub(crate) fn validate_url(url: &Url, allow_private_network: bool) -> Result<(), NetError> {
    let allow_private_network = allow_private_network || env_allows_private_network();
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" && scheme != "file" {
        return Err(NetError::Network(format!(
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
                if is_forbidden_ip(IpAddr::V4(ip)) {
                    return Err(NetError::Network(format!(
                        "Access to private/internal IP address {} is not allowed",
                        ip
                    )));
                }
            }
            url::Host::Ipv6(ip) => {
                if is_forbidden_ip(IpAddr::V6(ip)) {
                    return Err(NetError::Network(format!(
                        "Access to private/internal IPv6 address {} is not allowed",
                        ip
                    )));
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(NetError::Network(format!(
                        "Access to localhost domain '{}' is not allowed",
                        domain
                    )));
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn fetch_file_url(url: &Url) -> Result<Response, NetError> {
    let path = url
        .to_file_path()
        .map_err(|_| NetError::Network("Invalid file URL".to_string()))?;
    let body = tokio::fs::read(&path)
        .await
        .map_err(|e| NetError::Network(format!("Failed to read file: {}", e)))?;

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

pub struct HttpClient {
    client: tokio::sync::OnceCell<Client>,
    /// Direct-connect client (no proxy). Built once on first use.
    direct_client: tokio::sync::OnceCell<Client>,
    /// Proxy client for known-blocked domains when no explicit proxy was
    /// configured. Built once on first auto-proxy hit from `AGINXBROWSER_PROXY`;
    /// `None` inside means the env var is unset (every later check falls
    /// through to the direct client without rebuilding).
    auto_proxy_client: tokio::sync::OnceCell<Option<Client>>,
    proxy_url: Option<String>,
    pub cookie_jar: Arc<CookieJar>,
    pub user_agent: RwLock<String>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    pub timeout: Duration,
    pub in_flight: Arc<std::sync::atomic::AtomicU32>,
    pub block_trackers: bool,
    /// When true, `validate_url` lets localhost / RFC1918 / link-local addresses
    /// through in addition to the `AGINXBROWSER_ALLOW_PRIVATE_NETWORK` env var.
    /// Set via `--allow-private-network` on the CLI (issue #33).
    pub allow_private_network: bool,
}

impl HttpClient {
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
        HttpClient {
            client: tokio::sync::OnceCell::new(),
            direct_client: tokio::sync::OnceCell::new(),
            auto_proxy_client: tokio::sync::OnceCell::new(),
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
            let mut builder = reqwest_builder_no_env_proxy()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                // Bug #24 (long-run degradation): a pooled connection that went
                // half-dead while idle (NAT drop, proxy reset) used to be handed
                // back out and stall every subsequent request until the process
                // was restarted. Keep the idle window short and send TCP
                // keepalive so the pool reaps stale connections instead.
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_keepalive(Duration::from_secs(30))
                .danger_accept_invalid_certs(false);
                // No manual Accept-Encoding header: reqwest 0.12 with the
                // gzip/brotli/deflate cargo features decodes by the RESPONSE's
                // Content-Encoding header regardless of what we advertised
                // (pinned by render.rs's unconditional-gzip fixture test), so
                // Aliyun Tengine fronts that compress unrequested still
                // decode. Advertising nothing keeps the request fingerprint
                // plain; advertising would also be fine, but never set the
                // header by hand — a manual Accept-Encoding disables reqwest's
                // auto-decode and raw gzip then reaches the HTML parser.

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
            reqwest_builder_no_env_proxy()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                // See get_client: short idle window + keepalive against
                // half-dead pooled connections (bug #24).
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_keepalive(Duration::from_secs(30))
                .danger_accept_invalid_certs(false)
                // SSRF guard: reject hostnames that resolve to a
                // private/loopback IP at connect time (replaces the old
                // TOCTOU pre-resolution check). Only on the direct client —
                // with a proxy, the proxy resolves target DNS and the only
                // local lookup is the proxy host itself, which is often
                // deliberately a loopback address (e.g. socks5://127.0.0.1).
                .dns_resolver(Arc::new(SsrfGuardResolver::new(self.allow_private_network)))
                .build()
                .expect("failed to build direct HTTP client")
        }).await
    }

    /// Pick the client for this request. An explicitly configured proxy
    /// (`context proxy_url`) routes ALL traffic — that is the operator's
    /// opt-in. Without one, requests go direct except known-blocked domains
    /// (the same list /fetch and download honor), which ride
    /// `AGINXBROWSER_PROXY` when set. That per-request fallback is what keeps
    /// page/session/CDP navigations working on the CN boundary: a session
    /// created without use_proxy used to hard-fail on wikipedia while /fetch
    /// succeeded for the same origin.
    async fn get_client_for(&self, url: &Url) -> &Client {
        if self.proxy_url.is_some() {
            return self.get_client().await;
        }
        if let Some(c) = self.get_auto_proxy_client(url).await {
            return c;
        }
        self.get_direct_client().await
    }

    /// Proxied client for known-blocked domains, built once from
    /// `AGINXBROWSER_PROXY`. `None` when the domain isn't listed or no proxy
    /// is configured — callers fall through to the direct client.
    ///
    /// Like `get_client`, no SSRF DNS resolver here: with a proxy the target
    /// resolves at the proxy, and the proxy host itself is often loopback.
    async fn get_auto_proxy_client(&self, url: &Url) -> Option<&Client> {
        if !crate::config::should_auto_proxy(url.as_str()) {
            return None;
        }
        let proxy = match crate::config::proxy_from_env() {
            Some(p) => p,
            None => return None,
        };
        self.auto_proxy_client
            .get_or_init(|| async move {
                let mut builder = reqwest_builder_no_env_proxy()
                    .redirect(Policy::none())
                    .timeout(Duration::from_secs(30))
                    .connect_timeout(Duration::from_secs(10))
                    .pool_idle_timeout(Duration::from_secs(60))
                    .tcp_keepalive(Duration::from_secs(30))
                    .danger_accept_invalid_certs(false);
                if let Ok(p) = reqwest::Proxy::all(&proxy) {
                    builder = builder.proxy(p);
                }
                Some(builder.build().expect("failed to build auto-proxy HTTP client"))
            })
            .await
            .as_ref()
    }

    /// Read-only accessor for the proxy URL the client was configured with
    /// (if any). Exposed so callers outside the net module — notably
    /// `op_fetch_url` in `diting-js` (#139) — can route their own reqwest
    /// requests through the same upstream proxy.
    pub fn proxy_url(&self) -> Option<&str> {
        self.proxy_url.as_deref()
    }

    /// The reqwest client this request should use (context-scoped, tied to
    /// this browser context). Cloning a `reqwest::Client` is cheap — it shares
    /// the underlying connection pool — so callers that need an owned handle
    /// (e.g. `op_fetch_url`, which builds a request and follows redirects
    /// itself) can take one without copying the pool.
    pub async fn request_client(&self, url: &str) -> Client {
        match url::Url::parse(url) {
            Ok(u) => self.get_client_for(&u).await.clone(),
            Err(_) => self.get_direct_client().await.clone(),
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, NetError> {
        self.fetch_with_method(Method::GET, url, None).await
    }

    pub async fn post_form(&self, url: &Url, body: &str) -> Result<Response, NetError> {
        self.fetch_with_method(Method::POST, url, Some(body.as_bytes().to_vec())).await
    }

    /// Passive-observer variants (upstream issue #408): fire the registry's
    /// on_request callbacks with the fully-built request just before each hop
    /// is sent, and on_response with the completed response. `None` behaves
    /// exactly like the untraced entry points.
    pub async fn fetch_with_callbacks(
        &self,
        url: &Url,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(Method::GET, url, None, callbacks, resource_type)
            .await
    }

    /// See `fetch_with_callbacks`.
    pub async fn post_form_with_callbacks(
        &self,
        url: &Url,
        body: &str,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(
            Method::POST,
            url,
            Some(body.as_bytes().to_vec()),
            callbacks,
            resource_type,
        )
        .await
    }

    pub async fn fetch_with_method(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<Vec<u8>>,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(initial_method, url, initial_body, None, ResourceType::Document)
            .await
    }

    async fn fetch_with_method_traced(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<Vec<u8>>,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
    ) -> Result<Response, NetError> {
        validate_url(url, self.allow_private_network)?;

        if url.scheme() == "file" {
            return fetch_file_url(url).await;
        }

        let mut method = initial_method;
        let mut body = initial_body;
        if self.block_trackers {
            if let Some(host) = url.host_str() {
                if crate::diting_net::blocklist::is_blocked(host) {
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

            // Passive on_request observers (upstream #408): capture the
            // fully-built header set (it is moved into the request below)
            // and fire per hop just before the request goes out. Skipped
            // entirely when nobody listens.
            let sent_headers = match callbacks {
                Some(cbs) if cbs.has_request_callbacks().await => Some((
                    cbs,
                    headers
                        .iter()
                        .map(|(k, v)| {
                            (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string())
                        })
                        .collect::<HashMap<String, String>>(),
                )),
                _ => None,
            };

            let mut req_builder = self.get_client_for(&current_url).await.request(method.clone(), current_url.as_str())
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

            if let Some((cbs, sent_headers)) = sent_headers.as_ref() {
                let info = RequestInfo {
                    url: current_url.clone(),
                    method: method.as_str().to_string(),
                    headers: sent_headers.clone(),
                    resource_type,
                };
                cbs.fire_request(&info).await;
            }

            self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let resp = match req_builder.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    // Name the culprit when the configured upstream proxy is
                    // the part that cannot be reached: the target URL alone
                    // reads as "site down" and sends the operator debugging
                    // the wrong layer (obscura#491's debugging cost).
                    return Err(match (&self.proxy_url, e.is_connect()) {
                        (Some(proxy), true) => NetError::Network(format!(
                            "upstream proxy {} unreachable while fetching {}: {} — unset AGINXBROWSER_PROXY to connect directly",
                            proxy, current_url, e
                        )),
                        _ => NetError::Network(format!("{}: {}", current_url, e)),
                    });
                }
            };
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
                        NetError::Network("Invalid redirect Location header".into())
                    })?;
                    let next_url = current_url.join(location_str).map_err(|e| {
                        NetError::Network(format!("Invalid redirect URL: {}", e))
                    })?;
                    validate_url(&next_url, self.allow_private_network)?;
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
                NetError::Network(format!("Failed to read body: {}", e))
            })?.to_vec();

            let response = Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body: body_bytes,
                redirected_from: redirects,
            };

            // Passive on_response observers: fired with the completed final
            // response (post-redirect, body read).
            if let Some(cbs) = callbacks {
                if cbs.has_response_callbacks().await {
                    let info = RequestInfo {
                        url: response.url.clone(),
                        method: method.as_str().to_string(),
                        headers: response.headers.clone(),
                        resource_type,
                    };
                    cbs.fire_response(&info, &response).await;
                }
            }

            return Ok(response);
        }

        Err(NetError::TooManyRedirects(current_url.to_string()))
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

impl Default for HttpClient {
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
pub enum NetError {
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

    // Env-sensitive: AGINXBROWSER_ALLOW_PRIVATE_NETWORK overrides rejection, so this
    // runs under the crate-wide env lock with the variable cleared.
    #[tokio::test]
    async fn validate_url_ssrf_rules() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        for bad in [
            "http://127.0.0.1/",
            "http://127.1.2.3:8080/admin",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://localhost:3000/",
        ] {
            let url = Url::parse(bad).unwrap();
            assert!(validate_url(&url, false).is_err(), "{bad} must be rejected");
        }

        for good in ["http://example.com/", "https://example.com:8443/a?b=c", "file:///tmp/x.html"] {
            let url = Url::parse(good).unwrap();
            assert!(validate_url(&url, false).is_ok(), "{good} must be allowed");
        }
        let url = Url::parse("ftp://example.com/").unwrap();
        assert!(validate_url(&url, false).is_err(), "ftp must be rejected");
    }

    #[test]
    fn is_forbidden_ip_covers_mapped_and_unspecified() {
        use std::str::FromStr;
        for bad in [
            "127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.5.4", "169.254.169.254",
            "0.0.0.0", "255.255.255.255", "192.0.2.1", // documentation
            "::1", "::", "fc00::1", "fe80::1",
            "::ffff:127.0.0.1", "::ffff:10.0.0.1", // IPv4-mapped
        ] {
            assert!(is_forbidden_ip(IpAddr::from_str(bad).unwrap()), "{bad} must be forbidden");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!is_forbidden_ip(IpAddr::from_str(good).unwrap()), "{good} must be allowed");
        }
    }

    #[tokio::test]
    async fn ssrf_guard_resolver_blocks_loopback_resolution() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let guarded = SsrfGuardResolver::new(false);
        assert!(
            guarded.resolve(Name::from_str("localhost").unwrap()).await.is_err(),
            "localhost must be rejected by the guarded resolver"
        );

        let permissive = SsrfGuardResolver::new(true);
        assert!(
            permissive.resolve(Name::from_str("localhost").unwrap()).await.is_ok(),
            "allow_private must pass localhost through"
        );
    }

    use std::str::FromStr;

    /// Serve a canned 200 response on a NON-loopback local address.
    ///
    /// reqwest exempts loopback destinations from env-proxy matching (like
    /// Chrome's implicit localhost bypass), so a 127.0.0.1 fixture is blind
    /// to exactly the failure these tests pin — the first probe round here
    /// reported "env ignored" purely because of that exemption. The LAN
    /// address is discovered via a UDP connect (no packet leaves; it only
    /// makes the routing table pick an interface).
    async fn lan_http_origin(body: &'static str) -> Option<(Url, tokio::task::JoinHandle<()>)> {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        probe.connect("8.8.8.8:80").ok()?;
        let ip = probe.local_addr().ok()?.ip();
        if ip.is_loopback() {
            return None;
        }
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.ok()?;
        let addr = listener.local_addr().ok()?;
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        stream.read(&mut buf),
                    )
                    .await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        Some((Url::parse(&format!("http://{addr}/")).unwrap(), handle))
    }

    fn set_dead_standard_proxy_env() {
        for (k, v) in [
            ("HTTP_PROXY", "http://127.0.0.1:1"),
            ("HTTPS_PROXY", "http://127.0.0.1:1"),
            ("ALL_PROXY", "http://127.0.0.1:1"),
            ("http_proxy", "http://127.0.0.1:1"),
            ("https_proxy", "http://127.0.0.1:1"),
            ("all_proxy", "http://127.0.0.1:1"),
        ] {
            unsafe { std::env::set_var(k, v) };
        }
    }

    fn clear_standard_proxy_env() {
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }

    /// obscura#491 class: standard proxy env vars must not silently route
    /// engine traffic through a proxy the operator never configured in the
    /// engine. If they do, a dead HTTP_PROXY takes down every public fetch
    /// with an error that never mentions a proxy. Runs under the crate env
    /// lock because the env mutation is process-global.
    #[allow(clippy::await_holding_lock)] // env guard must span the fixture fetch — that's the serialization
    #[tokio::test]
    async fn standard_proxy_env_cannot_hijack_engine_clients() {
        let Some((url, origin)) = lan_http_origin("env-proxy-hijack").await else {
            eprintln!("skip: no non-loopback local address to serve on");
            return;
        };
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        set_dead_standard_proxy_env();

        // proxy_url None + private-network allowed (the LAN fixture address is
        // RFC1918): the operator asked for a direct fetch.
        let client = HttpClient::with_full_options(
            std::sync::Arc::new(CookieJar::new()),
            None,
            true,
        );
        let fetched = client.fetch(&url).await;

        clear_standard_proxy_env();
        origin.abort();

        match fetched {
            Ok(resp) => assert_eq!(resp.status, 200, "direct fetch must succeed"),
            Err(e) => panic!("standard proxy env hijacked a proxy-less client: {e:?}"),
        }
    }

    /// #664 lesson applied to the proxy path: when the configured upstream
    /// proxy is unreachable, the error must name the proxy and its knob —
    /// the reader should not have to go verbose-logging to learn a proxy is
    /// involved at all (the original #491 debugging cost).
    #[allow(clippy::await_holding_lock)] // env guard must span the fixture fetch — that's the serialization
    #[tokio::test]
    async fn dead_upstream_proxy_error_names_the_proxy() {
        let Some((url, origin)) = lan_http_origin("proxy-naming").await else {
            eprintln!("skip: no non-loopback local address to serve on");
            return;
        };
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        clear_standard_proxy_env();

        let client = HttpClient::with_options(
            std::sync::Arc::new(CookieJar::new()),
            Some("http://127.0.0.1:1"),
        );
        // The engine's own proxy gate: RFC1918 target needs the opt-in even
        // though the dial actually goes to the dead proxy.
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let fetched = client.fetch(&url).await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        origin.abort();

        let err = fetched.expect_err("dialing a dead proxy must fail");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("upstream proxy") && msg.contains("http://127.0.0.1:1"),
            "error must name the unreachable proxy, got: {msg}"
        );
        assert!(
            msg.contains("AGINXBROWSER_PROXY"),
            "error must name the knob, got: {msg}"
        );
    }
}
