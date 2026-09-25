//! SSRF deny-set + the DNS-resolver guard — split from the module root
//! (god-file ratchet). One coherent topic: which IPs the engine must never
//! dial, and the reqwest resolver that enforces it at connect time.
use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::{env_allows_private_network, scoped_allow_network};

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
