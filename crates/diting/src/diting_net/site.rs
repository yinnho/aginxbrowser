//! Site classification for Fetch-Metadata and SameSite enforcement.
//!
//! Chrome computes `Sec-Fetch-Site` and cookie send/receive permissions from
//! the *site* (registrable domain, eTLD+1) of the initiator vs the target —
//! not from full-origin equality. Comparing origins instead of sites labels
//! every sibling-subdomain request (`item.upload.taobao.com` →
//! `everyhelp.taobao.com`) as `cross-site`, a combination real Chrome never
//! produces (same-site Lax login cookies riding a self-declared cross-site
//! subrequest) and one taobao risk control punishes with a defensive logout
//! (#203). The embedded official public_suffix_list.dat is the same data
//! Chrome uses; the matcher below is the PSL's own three-rule algorithm —
//! longest matching suffix rule, `*` wildcards, `!` exceptions.

use std::collections::HashMap;
use std::sync::OnceLock;
use url::Url;

const PSL_DATA: &str = include_str!("public_suffix_list.dat");

struct SuffixList {
    /// Rule key (lowercase, no `!` prefix, wildcards kept as `*` labels) →
    /// whether it is an exception rule.
    rules: HashMap<String, bool>,
}

fn suffix_list() -> &'static SuffixList {
    static LIST: OnceLock<SuffixList> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut rules = HashMap::new();
        for line in PSL_DATA.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if let Some(rule) = line.split_whitespace().next() {
                let (rule, exception) = match rule.strip_prefix('!') {
                    Some(r) => (r, true),
                    None => (rule, false),
                };
                rules.insert(rule.to_lowercase(), exception);
            }
        }
        SuffixList { rules }
    })
}

/// Index into `labels` where the public suffix begins (PSL algorithm). IPs
/// never get here — see [`registrable_domain`].
fn suffix_start(labels: &[&str]) -> usize {
    let list = suffix_list();
    let mut best: Option<usize> = None; // start index of the longest matching rule
    let mut exception: Option<usize> = None;
    for start in 0..labels.len() {
        let mut candidates: Vec<String> = vec![labels[start..].join(".")];
        // Wildcard rules: any single label replaced by `*`.
        for k in start..labels.len() {
            let mut parts: Vec<&str> = labels[start..].to_vec();
            parts[k - start] = "*";
            candidates.push(parts.join("."));
        }
        for cand in candidates {
            match list.rules.get(&cand) {
                Some(&true) => exception = Some(start),
                Some(&false) => {
                    if best.is_none_or(|b| start < b) {
                        best = Some(start);
                    }
                }
                None => {}
            }
        }
    }
    // An exception rule wins: the public suffix is the rule minus its
    // leftmost label. No match at all → the default `*` rule: last label.
    match exception {
        Some(start) => start + 1,
        None => best.unwrap_or(labels.len() - 1),
    }
}

/// Registrable domain (eTLD+1) of a bare host. Hosts with no registrable
/// domain — IPs, `localhost`, hosts that are themselves a public suffix —
/// are their own opaque site: equal only to themselves.
pub fn registrable_domain(host: &str) -> String {
    let h = host.trim_end_matches('.').to_lowercase();
    if h.parse::<std::net::IpAddr>().is_ok() {
        return h;
    }
    let labels: Vec<&str> = h.split('.').collect();
    let start = suffix_start(&labels);
    if start == 0 {
        h
    } else {
        labels[start - 1..].join(".")
    }
}

/// Schemeful same-site (Chrome): both http(s), same scheme, same registrable
/// domain. Port and path do not participate.
pub fn is_same_site(a: &Url, b: &Url) -> bool {
    if a.scheme() != b.scheme() || !matches!(a.scheme(), "http" | "https") {
        return false;
    }
    match (a.host_str(), b.host_str()) {
        (Some(ha), Some(hb)) => registrable_domain(ha) == registrable_domain(hb),
        _ => false,
    }
}

/// `Sec-Fetch-Site` value for a request from `initiator` to `target`, per the
/// Fetch spec's four-value vocabulary. `None` initiator = address-bar /
/// tool-initiated navigation → `"none"`.
pub fn sec_fetch_site(initiator: Option<&Url>, target: &Url) -> &'static str {
    let Some(init) = initiator else { return "none" };
    if init.origin() == target.origin() {
        "same-origin"
    } else if is_same_site(init, target) {
        "same-site"
    } else {
        "cross-site"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn sibling_subdomains_are_same_site_not_cross() {
        // The #203 shape: item.upload.taobao.com → everyhelp.taobao.com.
        assert_eq!(
            sec_fetch_site(
                Some(&u("https://item.upload.taobao.com/sell/v2/publish.htm")),
                &u("https://everyhelp.taobao.com/version/getWidgetVersion")
            ),
            "same-site"
        );
    }

    #[test]
    fn same_origin_and_true_cross_site() {
        assert_eq!(
            sec_fetch_site(
                Some(&u("https://a.taobao.com/x")),
                &u("https://a.taobao.com/y")
            ),
            "same-origin"
        );
        assert_eq!(
            sec_fetch_site(
                Some(&u("https://item.upload.taobao.com/")),
                &u("https://g.alicdn.com/main.css")
            ),
            "cross-site"
        );
    }

    #[test]
    fn port_difference_is_same_site() {
        // Cross-origin (port), same-site: the loopback test stand-in for
        // sibling subdomains.
        assert_eq!(
            sec_fetch_site(Some(&u("http://localhost:8080/")), &u("http://localhost:8081/x")),
            "same-site"
        );
    }

    #[test]
    fn second_level_suffixes_and_the_suffix_themselves() {
        // com.cn is a public suffix: a.baidu.com.cn vs b.baidu.com.cn are
        // same-site; baidu.com.cn vs baidu.com are cross-site.
        assert!(is_same_site(
            &u("https://a.baidu.com.cn/"),
            &u("https://b.baidu.com.cn/")
        ));
        assert!(!is_same_site(
            &u("https://baidu.com.cn/"),
            &u("https://www.baidu.com/")
        ));
        // github.io is a public suffix: user repos are distinct sites.
        assert!(!is_same_site(
            &u("https://alice.github.io/"),
            &u("https://bob.github.io/")
        ));
    }

    #[test]
    fn exception_and_wildcard_rules() {
        // PSL doc examples: *.ck is a wildcard, www.ck the exception — so
        // www.ck's suffix is ck (registrable www.ck), while a.b.ck's suffix
        // is b.ck (registrable a.b.ck itself, the wildcard rule's shape).
        assert_eq!(registrable_domain("www.ck"), "www.ck");
        assert_eq!(registrable_domain("a.b.ck"), "a.b.ck");
        assert_eq!(registrable_domain("x.y.www.ck"), "www.ck");
        assert_eq!(registrable_domain("test.example.com"), "example.com");
    }

    #[test]
    fn schemeful_and_opaque_hosts() {
        // http vs https are different sites (schemeful same-site).
        assert!(!is_same_site(
            &u("http://a.taobao.com/"),
            &u("https://b.taobao.com/")
        ));
        // IP and localhost have no registrable domain: opaque, equal only to
        // themselves.
        assert_eq!(
            sec_fetch_site(Some(&u("http://localhost:8080/")), &u("http://127.0.0.1:8080/x")),
            "cross-site"
        );
        assert!(is_same_site(
            &u("http://127.0.0.1:8080/"),
            &u("http://127.0.0.1:9999/x")
        ));
        assert_eq!(registrable_domain("localhost"), "localhost");
    }

    #[test]
    fn no_initiator_is_none() {
        assert_eq!(sec_fetch_site(None, &u("https://taobao.com/")), "none");
    }
}
