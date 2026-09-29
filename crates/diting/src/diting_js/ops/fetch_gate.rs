//! The page-reachable URL gate for fetch()/XHR/dynamic-import — scheme
//! allow-list plus the private-network checks. Split from the ops god file
//! (ratchet); pure function over `url::Url`, no OpState.
use crate::diting_net::client::policy::PRIVATE_NETWORK_HINT;

/// Also applied by the ES module loader (obscura #849): dynamic import() is
/// as page-reachable as fetch(), so it answers to the same scheme and
/// private-network policy. The cached client these paths share never
/// auto-follows redirects, so validating the resolved specifier covers
/// every hop a fetch can actually take.
pub fn validate_fetch_url(url: &url::Url) -> Result<(), String> {
    let scheme = url.scheme();
    // file:// is rejected up front for page-reachable fetch/XHR, matching the
    // deny-by-default navigation posture (upstream obscura #708: the old gate
    // allowed the scheme through and short-circuited the SSRF checks; the
    // transports couldn't actually fetch it, but the inconsistency leaked an
    // "allowed" signal to probing scripts).
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "Forbidden URL scheme '{}' - only http and https are allowed",
            scheme
        ));
    }

    if crate::diting_net::env_allows_private_network() {
        return Ok(());
    }

    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(ip) => {
                // Shared deny-set with navigation (is_forbidden_base), not a
                // local re-listing: the hand-rolled loopback/RFC1918 checks
                // here missed the IANA special-purpose ranges (198.18.0.0/15
                // benchmarking, 100.64/10 CGNAT metadata, 0.0.0.0/8) and the
                // embedded-IPv4 forms (mapped, 6to4, NAT64), so a page could
                // fetch addresses the navigation gate blocks (obscura #852
                // family). Scoped allow-network subtraction is built into
                // is_forbidden_ip.
                if crate::diting_net::client::is_forbidden_ip(std::net::IpAddr::V4(ip)) {
                    return Err(format!(
                        "Access to private/internal IP address {} is not allowed{}",
                        ip,
                        PRIVATE_NETWORK_HINT
                    ));
                }
            }
            url::Host::Ipv6(ip) => {
                if crate::diting_net::client::is_forbidden_ip(std::net::IpAddr::V6(ip)) {
                    return Err(format!(
                        "Access to private/internal IPv6 address {} is not allowed{}",
                        ip,
                        PRIVATE_NETWORK_HINT
                    ));
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(format!(
                        "Access to localhost domain '{}' is not allowed{}",
                        domain,
                        PRIVATE_NETWORK_HINT
                    ));
                }
            }
        }
    }

    Ok(())
}
