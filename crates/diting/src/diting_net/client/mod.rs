use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
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
pub fn reqwest_builder_no_env_proxy() -> reqwest::ClientBuilder {
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
    Fetch,
}

impl ResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Script => "Script",
            Self::Stylesheet => "Stylesheet",
            Self::Image => "Image",
            Self::Fetch => "Fetch",
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
#[cfg(feature = "stealth")]
pub(crate) fn custom_cert_store_requested(
    cert_file: Option<&std::ffi::OsStr>,
    cert_dir: Option<&std::ffi::OsStr>,
) -> bool {
    fn present(v: Option<&std::ffi::OsStr>) -> bool {
        v.is_some_and(|s| !s.is_empty())
    }
    present(cert_file) || present(cert_dir)
}

/// Truthy set shared by the engine's opt-in env knobs: "1"/"true"/"yes"/"on"
/// (case-insensitive, trimmed) all count.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// The CLI-flag half of the private-network opt-in, OR'd with the env var.
static PRIVATE_NETWORK_FLAG: AtomicBool = AtomicBool::new(false);

/// `--allow-private-network` (issue #33): five call-site comments promised
/// this flag while only the env var was wired. Startup sets this instead of
/// mutating the environment mid-process.
pub fn set_allow_private_network(enabled: bool) {
    PRIVATE_NETWORK_FLAG.store(enabled, Ordering::Relaxed);
}

pub fn env_allows_private_network() -> bool {
    PRIVATE_NETWORK_FLAG.load(Ordering::Relaxed) || env_flag("AGINXBROWSER_ALLOW_PRIVATE_NETWORK")
}

/// One parsed entry of the scoped allow-network list: network address and
/// prefix length in the entry's own family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopedCidr {
    net: IpAddr,
    prefix: u8,
}

/// Parse a comma/space-separated CIDR list — `10.20.0.0/16,192.168.1.0/24` or
/// bare addresses (`127.0.0.1` → /32, `::1` → /128). Tokens that don't parse
/// are skipped, never widened: an unparsable entry means the engine keeps
/// blocking that range (fail closed), it does not fall back to
/// allow-everything.
pub fn parse_scoped_cidrs(spec: &str) -> Vec<ScopedCidr> {
    let mut out = Vec::new();
    for token in spec.split([',', ' ', '\t']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (addr_part, default_prefix) = match token.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (token, None),
        };
        let ip: IpAddr = match addr_part.parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };
        let max = match ip {
            IpAddr::V4(_) => 32u8,
            IpAddr::V6(_) => 128u8,
        };
        let prefix = match default_prefix {
            Some(p) => match p.parse::<u8>() {
                Ok(p) if p <= max => p,
                _ => continue,
            },
            None => max,
        };
        out.push(ScopedCidr { net: ip, prefix });
    }
    out
}

fn ip_in_scoped_cidr(ip: IpAddr, c: ScopedCidr) -> bool {
    // Mask-compare in the entry's own family; a v6 entry never matches a v4
    // target (embedded-v4 targets are reached through the recursive leaf
    // check inside `is_forbidden_ip`, which re-enters this function with the
    // extracted v4 address).
    let (ip_bits, net_bits, max) = match (ip, c.net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => (
            u32::from(ip) as u128,
            u32::from(net) as u128,
            32u8,
        ),
        (IpAddr::V6(ip), IpAddr::V6(net)) => (u128::from(ip), u128::from(net), 128u8),
        _ => return false,
    };
    if c.prefix == 0 {
        return true;
    }
    let shift = max - c.prefix;
    (ip_bits >> shift) == (net_bits >> shift)
}

/// The CLI-flag half of the scoped allow-network opt-in
/// (`--allow-network <cidr,cidr,...>`). Parsed once at startup because the
/// value is a list, not a boolean.
static ALLOW_NETWORK_FLAG: std::sync::RwLock<Vec<ScopedCidr>> = std::sync::RwLock::new(Vec::new());

/// Set the scoped allow-network list from the CLI flag. Pass an empty spec
/// to clear.
pub fn set_allow_network(spec: Option<&str>) {
    let parsed = spec.map(parse_scoped_cidrs).unwrap_or_default();
    if let Ok(mut slot) = ALLOW_NETWORK_FLAG.write() {
        *slot = parsed;
    }
}

/// The scoped half of the SSRF escape hatch — obscura#856. An address in
/// this list is allowed THROUGH the deny-set without flipping
/// `--allow-private-network`'s allow-everything switch: point the engine at
/// an internal app on 10.20.x.y while the cloud-metadata endpoints
/// (169.254.169.254, 100.100.100.200) and every other forbidden range stay
/// closed. Reads both the CLI list and `AGINXBROWSER_ALLOW_NETWORK` (same
/// syntax) so Docker setups that only speak env keep working.
fn scoped_allow_network(ip: IpAddr) -> bool {
    if let Ok(list) = ALLOW_NETWORK_FLAG.read() {
        if list.iter().any(|c| ip_in_scoped_cidr(ip, *c)) {
            return true;
        }
    }
    match std::env::var("AGINXBROWSER_ALLOW_NETWORK") {
        Ok(spec) => parse_scoped_cidrs(&spec)
            .iter()
            .any(|c| ip_in_scoped_cidr(ip, *c)),
        Err(_) => false,
    }
}

/// The CLI-flag half of the file:// opt-in (requirements-aginxos P2).
static FILE_ACCESS_FLAG: AtomicBool = AtomicBool::new(false);

/// True when the engine may read `file://` URLs — navigation documents,
/// subresources, /fetch. Off by default: the server binds 0.0.0.0 out of
/// the box, so an unguarded local-file read would hand the operator's
/// filesystem to any client that can reach the port. Flipped by
/// `--allow-file-access` or `AGINXBROWSER_ALLOW_FILE_ACCESS`. The gate
/// itself lives in [`fetch_file_url`], the single choke point both
/// transports (the reqwest funnel and the stealth client, redirect hops
/// included) already delegate to; the CDP-layer gates read this too.
pub fn allow_file_access() -> bool {
    FILE_ACCESS_FLAG.load(Ordering::Relaxed) || env_flag("AGINXBROWSER_ALLOW_FILE_ACCESS")
}

/// Tests flip the gate through this instead of touching the environment.
pub fn set_allow_file_access(enabled: bool) {
    FILE_ACCESS_FLAG.store(enabled, Ordering::Relaxed);
}

/// Shared test fixture for the process-global file-access switch. The
/// switch is one AtomicBool for the whole process, so ANY test that flips
/// it — here or in another module reading it through `allow_file_access()`
/// (screenshot.rs's subresource collector, ...) — must hold this lock while
/// asserting, or the two run concurrently and race (1f7486c pattern). The
/// drop-guard restores both halves (flag + env) even on panic so the
/// off-by-default contract can't leak across tests.
#[cfg(test)]
pub(crate) mod file_access_test {
    use std::sync::atomic::Ordering;
    static FILE_ACCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct FileAccessGuard(std::sync::MutexGuard<'static, ()>, bool);
    impl Drop for FileAccessGuard {
        fn drop(&mut self) {
            super::FILE_ACCESS_FLAG.store(self.1, Ordering::Relaxed);
            std::env::remove_var("AGINXBROWSER_ALLOW_FILE_ACCESS");
        }
    }
    pub(crate) fn file_access_guard(enabled: bool) -> impl Drop {
        let guard = FILE_ACCESS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = super::FILE_ACCESS_FLAG.load(Ordering::Relaxed);
        super::FILE_ACCESS_FLAG.store(enabled, Ordering::Relaxed);
        std::env::remove_var("AGINXBROWSER_ALLOW_FILE_ACCESS");
        FileAccessGuard(guard, prev)
    }
}

/// True when `ip` must never be the target of an outbound request from the
/// engine, UNLESS a scoped allow-network entry (`--allow-network` /
/// `AGINXBROWSER_ALLOW_NETWORK`, obscura#856) explicitly covers it. The pure
/// deny-set lives in [`is_forbidden_base`]; this wrapper subtracts the
/// scoped allowlist so the literal-host check, the DNS-resolution check
/// (`SsrfGuardResolver`) and every embedded-IPv4 recursion site can never
/// disagree about what is allowed.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    is_forbidden_base(ip) && !scoped_allow_network(ip)
}

/// The pure SSRF deny-set — see [`is_forbidden_ip`] for the public entry.
/// Loopback, RFC1918 private, link-local (incl. the 169.254.169.254
/// cloud-metadata endpoint), broadcast, documentation, the unspecified address
/// (0.0.0.0 / ::, which the OS routes to localhost), IPv6 unique-local
/// (fc00::/7), CGNAT (100.64.0.0/10 — where Alibaba's 100.100.100.200
/// metadata endpoint lives), benchmarking (198.18.0.0/15), multicast and the
/// reserved/future ranges, and ANY IPv6 form with an embedded IPv4
/// (IPv4-mapped, IPv4-compatible, 6to4 2002::/16, NAT64 64:ff9b::/96)
/// whose embedded IPv4 lands in the deny-set — an allow-listed embedded v4
/// stays reachable through those wrappers. Teredo (2001:0000::/32) is the
/// exception: all three attacker-writable slots (server, client, XOR-
/// obfuscated client) must individually clear the deny-set + allowlist, so
/// a CIDR that opens the client obfuscates to a public address and the
/// whole form stays blocked (deliberate — fail closed).
fn is_forbidden_base(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 0.0.0.0/8: "this network" — some stacks treat it as
                // loopback-adjacent; block the whole /8, not just 0.0.0.0.
                || o[0] == 0
                // 100.64.0.0/10 CGNAT (cloud metadata endpoints live here,
                // e.g. Alibaba 100.100.100.200).
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                // 192.0.0.0/24 (Oracle 192.0.0.192 et al), 192.31.196.0/24
                // and 192.52.193.0/24 (AS112), 192.88.99.0/24 (6to4 relay),
                // 192.175.48.0/24 (Portmap).
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 192 && o[1] == 31 && o[2] == 196)
                || (o[0] == 192 && o[1] == 52 && o[2] == 193)
                || (o[0] == 192 && o[1] == 88 && o[2] == 99)
                || (o[0] == 192 && o[1] == 175 && o[2] == 48)
                // 198.18.0.0/15 benchmarking.
                || (o[0] == 198 && o[1] & 0xfe == 18)
                || v4.is_multicast() // 224.0.0.0/4
                || (o[0] & 0xf0) == 0xf0 // 240.0.0.0/4 reserved/future
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast() // ff00::/8
            {
                return true;
            }
            let seg = v6.segments();
            // 2001:db8::/32 documentation, ::/96-adjacent discard (100::/64).
            if seg[0] == 0x2001 && seg[1] == 0x0db8 {
                return true;
            }
            if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
                return true;
            }
            // Teredo 2001:0000::/32 (RFC 4380): the server IPv4 sits in bits
            // 32-63 and the client IPv4 in bits 96-127, XOR-obfuscated with
            // 0xffff. A crafted literal puts attacker bytes in every slot, so
            // check server, raw client, and de-obfuscated client alike.
            if seg[0] == 0x2001 && seg[1] == 0 {
                for (hi, lo) in [
                    (seg[2], seg[3]),
                    (seg[6], seg[7]),
                    (seg[6] ^ 0xffff, seg[7] ^ 0xffff),
                ] {
                    if is_forbidden_ip(IpAddr::V4(std::net::Ipv4Addr::new(
                        (hi >> 8) as u8,
                        hi as u8,
                        (lo >> 8) as u8,
                        lo as u8,
                    ))) {
                        return true;
                    }
                }
            }
            if let Some(v4) = embedded_ipv4(v6) {
                return is_forbidden_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

/// Extract the IPv4 address carried inside an IPv6 address, for the
/// single-embedded-address formats: IPv4-mapped (::ffff:a.b.c.d),
/// IPv4-compatible (::a.b.c.d), 6to4 (2002:a.b.c.d::, the public v4 sits in
/// bits 16-47), and NAT64 (64:ff9b::a.b.c.d, well-known prefix). Without the
/// last two, a literal host like [2002:a9fe:a9fe::] (6to4-wrapped
/// 169.254.169.254) or [64:ff9b::7f00:1] (NAT64-wrapped 127.0.0.1) bypasses
/// the v6 arm and dials a forbidden v4 target. Teredo carries two
/// attacker-writable slots (server + XOR-obfuscated client) and is checked
/// directly in `is_forbidden_ip`.
fn embedded_ipv4(v6: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let seg = v6.segments();
    // 6to4: 2002:<high16>:<low16>::/48 — embedded v4 is bits 16..48.
    if seg[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            seg[1] as u8,
            (seg[2] >> 8) as u8,
            seg[2] as u8,
        ));
    }
    // NAT64 prefixes 64:ff9b::/96 (well-known, RFC 6052) and 64:ff9b:1::/48
    // (local-use, RFC 8215) — in both the embedded v4 is the low 32 bits, so
    // one prefix pair check covers both forms.
    if seg[0] == 0x0064 && seg[1] == 0xff9b {
        return Some(std::net::Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ));
    }
    // IPv4-compatible ::a.b.c.d (all zero except the low 32 bits).
    v6.to_ipv4()
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

pub fn validate_url(url: &Url, allow_private_network: bool) -> Result<(), NetError> {
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
    if !allow_file_access() {
        return Err(NetError::Network(
            "file:// access is disabled. Restart with --allow-file-access or set \
             AGINXBROWSER_ALLOW_FILE_ACCESS=1 to enable."
                .to_string(),
        ));
    }
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
    /// Same default and env knob as the stealth transport (`diting_net::wreq_client`),
    /// so both transports advertise one Accept-Language (obscura #777 class).
    pub accept_language: RwLock<String>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    pub in_flight: Arc<std::sync::atomic::AtomicU32>,
    pub block_trackers: bool,
    /// When true, `validate_url` lets localhost / RFC1918 / link-local addresses
    /// through in addition to the `AGINXBROWSER_ALLOW_PRIVATE_NETWORK` env var.
    /// Set via `--allow-private-network` on the CLI (issue #33).
    pub allow_private_network: bool,
    /// Lazy legacy-TLS escape hatch (obscura#769 navigation layer): rustls
    /// carries no TLS 1.2 CBC cipher suites, so CBC-only servers die in the
    /// ClientHello while every browser connects. Built once on first
    /// fallback need, sharing this client's cookie jar and proxy posture;
    /// `None` means unavailable (non-stealth build, or a SOCKS proxy that
    /// wreq cannot speak).
    #[cfg(feature = "stealth")]
    legacy_tls: tokio::sync::OnceCell<Option<Arc<crate::diting_net::StealthHttpClient>>>,
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
            accept_language: RwLock::new(
                std::env::var("AGINXBROWSER_ACCEPT_LANGUAGE")
                    .unwrap_or_else(|_| "zh-CN,zh;q=0.9,en;q=0.8".to_string()),
            ),
            extra_headers: RwLock::new(HashMap::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            block_trackers: false,
            allow_private_network,
            #[cfg(feature = "stealth")]
            legacy_tls: tokio::sync::OnceCell::new(),
        }
    }

    async fn get_client(&self) -> &Client {
        self.client.get_or_init(|| async {
            let mut builder = reqwest_builder_no_env_proxy()
                .redirect(Policy::none())
                // Browser timeout semantics (#49), not scraper semantics:
                // `.timeout()` caps the WHOLE request — connect through body
                // read — so a large-but-live body (the xhs publish page's
                // 32MB ffmpeg-core.wasm at real CDN speed, ~40s) is killed
                // mid-stream at 30s with "error decoding response body" while
                // every browser streams it to completion. `read_timeout` is
                // the browser shape: headers get 30s from request start, then
                // every body chunk resets the stall clock — no total cap.
                .read_timeout(Duration::from_secs(30))
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
                // read_timeout (per-chunk stall, #49), not a total-duration
                // cap — see get_client.
                .read_timeout(Duration::from_secs(30))
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
        if !crate::env_knobs::should_auto_proxy(url.as_str()) {
            return None;
        }
        let proxy = match crate::env_knobs::proxy_from_env() {
            Some(p) => p,
            None => return None,
        };
        self.auto_proxy_client
            .get_or_init(|| async move {
                let mut builder = reqwest_builder_no_env_proxy()
                    .redirect(Policy::none())
                    // read_timeout (per-chunk stall, #49), not a total-duration
                    // cap — see get_client.
                    .read_timeout(Duration::from_secs(30))
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

    /// Compute the default `strict-origin-when-cross-origin` referrer for a
    /// document-initiated request (upstream edb1785). Same-origin sends the
    /// full source URL minus fragment/credentials; cross-origin sends only
    /// the origin; downgrades (https -> http) and non-HTTP(S) schemes send
    /// nothing. Referrer-Policy overrides are not yet plumbed through.
    pub(crate) fn navigation_referrer(source: &Url, target: &Url) -> String {
        if !matches!(source.scheme(), "http" | "https")
            || !matches!(target.scheme(), "http" | "https")
            || (source.scheme() == "https" && target.scheme() == "http")
        {
            return String::new();
        }

        if source.origin() == target.origin() {
            let mut sanitized = source.clone();
            sanitized.set_fragment(None);
            let _ = sanitized.set_username("");
            let _ = sanitized.set_password(None);
            return sanitized.to_string();
        }

        let mut origin = source.origin().ascii_serialization();
        origin.push('/');
        origin
    }

    /// Page-subresource fetch (render-time img/media prefetch): a plain GET
    /// that carries the initiating document's Referer. `referrer` is the raw
    /// document URL; the policy trim happens per hop in the traced path.
    pub async fn fetch_subresource(
        &self,
        url: &Url,
        referrer: Option<&str>,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(Method::GET, url, None, None, ResourceType::Image, referrer)
            .await
    }

    /// Passive-observer variants (upstream issue #408): fire the registry's
    /// on_request callbacks with the fully-built request just before each hop
    /// is sent, and on_response with the completed response. `None` behaves
    /// exactly like the untraced entry points. `referrer` is the initiating
    /// document URL for page-driven loads; top-level tool fetches pass None.
    pub async fn fetch_with_callbacks(
        &self,
        url: &Url,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
        referrer: Option<&str>,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(Method::GET, url, None, callbacks, resource_type, referrer)
            .await
    }

    /// See `fetch_with_callbacks`.
    pub async fn post_form_with_callbacks(
        &self,
        url: &Url,
        body: &str,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
        referrer: Option<&str>,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(
            Method::POST,
            url,
            Some(body.as_bytes().to_vec()),
            callbacks,
            resource_type,
            referrer,
        )
        .await
    }

    pub async fn fetch_with_method(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<Vec<u8>>,
    ) -> Result<Response, NetError> {
        self.fetch_with_method_traced(initial_method, url, initial_body, None, ResourceType::Document, None)
            .await
    }

    async fn fetch_with_method_traced(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<Vec<u8>>,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
        referrer: Option<&str>,
    ) -> Result<Response, NetError> {
        // The primary attempt borrows the body; ownership stays here so a
        // retry can re-send it. `connect_failed` is the send-stage
        // classification (DNS/TCP/TLS — the request never left the machine),
        // the only regime where a non-idempotent retry cannot double-submit.
        let mut connect_failed = false;
        match self
            .fetch_with_method_traced_inner(
                initial_method.clone(),
                url,
                initial_body.as_deref(),
                &mut connect_failed,
                callbacks,
                resource_type,
                referrer,
            )
            .await
        {
            Ok(resp) => Ok(resp),
            Err(e) => {
                // Mirror what the primary attempt would have sent: the plain
                // client hardcodes form-urlencoded on a POST with a body.
                let fallback_body = initial_body.as_deref();
                let fallback_ctype = if fallback_body.is_some() && initial_method == Method::POST {
                    Some("application/x-www-form-urlencoded")
                } else {
                    None
                };
                self.retry_via_legacy_tls(
                    initial_method,
                    url,
                    e,
                    fallback_body,
                    fallback_ctype,
                    connect_failed,
                    None,
                    true,
                )
                .await
            }
        }
    }

    /// One legacy-TLS retry for transport failures (obscura#769 navigation
    /// layer): rustls carries no TLS 1.2 CBC cipher suites, so a CBC-only
    /// server dies in the ClientHello while every browser connects; the
    /// stealth transport's BoringSSL stack still speaks CBC. Guards, in
    /// order:
    /// - GET/HEAD always retry (idempotent). Any other method retries only
    ///   when `connect_stage` says the failure was connect-phase (DNS/TCP/
    ///   TLS) — the request provably never left the machine, so a second
    ///   attempt cannot double-submit. A failure after the bytes went out
    ///   (body read, response-wait reset) keeps the original error. This is
    ///   what lets a scripted form POST ride the escape hatch without
    ///   re-submitting anything (taobao seller-backend shape);
    /// - the URL must pass `validate_url` again. Gate rejections travel as
    ///   `NetError::Network` just like transport failures, so the error
    ///   type cannot fence them — the re-check can, and the legacy
    ///   transport re-validates every hop it walks on top of that;
    /// - redirect loops and an unavailable transport (non-stealth build,
    ///   SOCKS proxy) pass the original error through unchanged.
    ///
    /// The legacy client shares the cookie jar and re-syncs identity at
    /// attempt time. `request_headers` (the scripted fetch()/XHR path)
    /// additionally mirrors the hop's browser-default headers — Origin,
    /// Referer, Fetch-Metadata, client hints — so the retry is the same
    /// request on another stack, not a bare one; the navigation path passes
    /// None and rides transport defaults. `include_cookies` carries the
    /// fetch credentials policy (navigation requests are always
    /// credentialed).
    #[cfg(feature = "stealth")]
    #[allow(clippy::too_many_arguments)]
    async fn retry_via_legacy_tls(
        &self,
        method: Method,
        url: &Url,
        err: NetError,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        connect_stage: bool,
        request_headers: Option<&HashMap<String, String>>,
        include_cookies: bool,
    ) -> Result<Response, NetError> {
        if matches!(err, NetError::TooManyRedirects(_)) {
            return Err(err);
        }
        if !matches!(method, Method::GET | Method::HEAD) && !connect_stage {
            return Err(err);
        }
        if validate_url(url, self.allow_private_network).is_err() {
            return Err(err);
        }
        let Some(legacy) = self.legacy_transport().await else {
            return Err(err);
        };
        legacy
            .set_user_agent(&self.user_agent.read().await.clone())
            .await;
        legacy
            .set_accept_language(&self.accept_language.read().await.clone())
            .await;
        legacy.set_extra_headers(self.extra_headers.read().await.clone()).await;
        tracing::warn!("rustls transport failed for {url}; retrying once via legacy TLS transport");
        match legacy
            .fetch_with_body(
                url,
                None,
                method.as_str(),
                body,
                content_type,
                request_headers,
                include_cookies,
            )
            .await
        {
            Ok(resp) => Ok(resp),
            Err(legacy_err) => Err(NetError::Network(format!(
                "{err}; legacy TLS transport also failed: {legacy_err}"
            ))),
        }
    }

    #[cfg(not(feature = "stealth"))]
    #[allow(clippy::too_many_arguments)]
    async fn retry_via_legacy_tls(
        &self,
        _method: Method,
        _url: &Url,
        err: NetError,
        _body: Option<&[u8]>,
        _content_type: Option<&str>,
        _connect_stage: bool,
        _request_headers: Option<&HashMap<String, String>>,
        _include_cookies: bool,
    ) -> Result<Response, NetError> {
        Err(err)
    }

    /// Legacy-TLS fallback for scripted fetch()/XHR (`op_fetch_url`): the op
    /// walks redirects on a raw reqwest client it obtained via
    /// `request_client()` and has no retry of its own, so a transport
    /// failure there hands the error string here and gets the same
    /// one-attempt BoringSSL escape hatch the subresource loaders use —
    /// same guards (GET/HEAD always; other methods only when
    /// `connect_stage` says the request never left the machine,
    /// `validate_url` re-check, per-hop re-validation inside the stealth
    /// redirect walk), same shared cookie jar and identity sync.
    /// `request_headers` is the hop's rebuilt scripted header set (Origin,
    /// Referer, Fetch-Metadata, client hints) — Referer-checking WAFs
    /// 403 the bare shape even after the handshake succeeds (the taobao
    /// seller-backend receipts proved the headered variant end to end).
    /// `include_cookies` carries the fetch credentials policy.
    /// `Err` carries the original transport error, or the combined message
    /// when the legacy attempt fired and failed too (the `legacy TLS
    /// transport` marker is how tests prove the fallback actually ran).
    #[allow(clippy::too_many_arguments)]
    pub async fn scripted_fetch_fallback(
        &self,
        method: &Method,
        url: &Url,
        transport_err: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        connect_stage: bool,
        request_headers: Option<&HashMap<String, String>>,
        include_cookies: bool,
    ) -> Result<Response, NetError> {
        self.retry_via_legacy_tls(
            method.clone(),
            url,
            NetError::Network(transport_err.to_string()),
            body,
            content_type,
            connect_stage,
            request_headers,
            include_cookies,
        )
        .await
    }

    /// Build the legacy transport on first fallback need, mirroring this
    /// client's cookie jar, proxy and private-network posture. wreq does
    /// not speak SOCKS5 (the #160 shape), so a SOCKS proxy leaves the
    /// client rustls-only rather than silently rewriting the scheme.
    #[cfg(feature = "stealth")]
    async fn legacy_transport(
        &self,
    ) -> Option<&Arc<crate::diting_net::StealthHttpClient>> {
        self.legacy_tls
            .get_or_init(|| async {
                if self
                    .proxy_url
                    .as_deref()
                    .is_some_and(|p| p.starts_with("socks"))
                {
                    return None;
                }
                let mut client = crate::diting_net::StealthHttpClient::with_proxy(
                    self.cookie_jar.clone(),
                    self.proxy_url.as_deref(),
                );
                client.allow_private_network = self.allow_private_network;
                Some(Arc::new(client))
            })
            .await
            .as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_with_method_traced_inner(
        &self,
        initial_method: Method,
        url: &Url,
        initial_body: Option<&[u8]>,
        connect_failed: &mut bool,
        callbacks: Option<&CallbackRegistry>,
        resource_type: ResourceType,
        referrer: Option<&str>,
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
                HeaderValue::from_str(&self.accept_language.read().await.clone())
                    .unwrap_or_else(|_| HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8")),
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
            // Document-initiated requests carry the initiator's Referer —
            // whitelist-checked service APIs (map vendors, CDN anti-leech)
            // reject bare requests — trimmed per hop by the navigation
            // policy, since a redirect can change the same/cross-origin
            // answer. Non-GET/HEAD also carries Origin (same-origin POSTs
            // included, matching the fetch() op's b744b9b semantics).
            // extra_headers below can override both.
            if let Some(src) = referrer {
                if let Ok(source) = Url::parse(src) {
                    let ref_value = Self::navigation_referrer(&source, &current_url);
                    if !ref_value.is_empty() {
                        if let Ok(v) = HeaderValue::from_str(&ref_value) {
                            headers.insert(reqwest::header::REFERER, v);
                        }
                    }
                    if method != Method::GET
                        && method != Method::HEAD
                        && !headers.contains_key(reqwest::header::ORIGIN)
                    {
                        let origin_value = source.origin().ascii_serialization();
                        if let Ok(v) = HeaderValue::from_str(&origin_value) {
                            headers.insert(reqwest::header::ORIGIN, v);
                        }
                    }
                }
            }

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

            if let Some(b) = body {
                if method == Method::POST {
                    req_builder = req_builder.header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    );
                }
                req_builder = req_builder.body(b.to_vec());
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
                    // Connect-phase (DNS/TCP/TLS) is the only failure regime
                    // where a non-idempotent retry is safe — the request
                    // never left the machine. Classify before the error is
                    // stringified; the legacy-TLS fallback reads the flag.
                    *connect_failed = e.is_connect();
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

            let response_headers = crate::diting_net::collect_response_headers(resp.headers());

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

    pub async fn set_accept_language(&self, lang: &str) {
        *self.accept_language.write().await = lang.to_string();
    }

    pub async fn set_extra_headers(&self, headers: HashMap<String, String>) {
        *self.extra_headers.write().await = headers;
    }

    pub fn active_requests(&self) -> u32 {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
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
}

#[cfg(test)]
mod tests;
