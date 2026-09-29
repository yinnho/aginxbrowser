//! Process-global network access policy: the private-network / scoped
//! CIDR / file:// gates and the URL validator. Split from the client god
//! file (ratchet) — these are free functions over statics, no client state.
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

use super::ssrf::is_forbidden_ip;
use super::NetError;

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
    pub(super) net: IpAddr,
    pub(super) prefix: u8,
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
pub(super) fn scoped_allow_network(ip: IpAddr) -> bool {
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
                        "Access to private/internal IP address {} is not allowed{}",
                        ip,
                        super::PRIVATE_NETWORK_HINT
                    )));
                }
            }
            url::Host::Ipv6(ip) => {
                if is_forbidden_ip(IpAddr::V6(ip)) {
                    return Err(NetError::Network(format!(
                        "Access to private/internal IPv6 address {} is not allowed{}",
                        ip,
                        super::PRIVATE_NETWORK_HINT
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
                        "Access to localhost domain '{}' is not allowed{}",
                        domain,
                        super::PRIVATE_NETWORK_HINT
                    )));
                }
            }
        }
    }

    Ok(())
}

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
