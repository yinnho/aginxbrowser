//! robots.txt compliance (RFC 9309 subset), checked before every autonomous
//! fetch path — `/fetch`, `/screenshot`, `/download`, and their MCP and
//! firecrawl equivalents. aginxbrowser grabs one page when an agent asks
//! (real-time data, not crawling), and honoring the site's rules by default
//! is the stance that keeps it that way. Operators who disagree own their
//! instance: `AGINXBROWSER_IGNORE_ROBOTS=1`.
//!
//! Interactive sessions (navigate/click/type) are deliberately exempt — they
//! are agent-driven the way a person at a keyboard is, and robots.txt governs
//! autonomous fetching, not browser interaction.
//!
//! Semantics follow RFC 9309 where it draws hard lines and common crawler
//! practice where it leaves room:
//!
//! * 2xx → parse; product-token groups take priority over `*`; longest
//!   matching rule wins, ties go to Allow; `*` and `$` wildcards honored.
//! * 404 / 410 → allow all (the host has no rules).
//! * other 4xx / 5xx / network failure → deny. A server refusing to hand out
//!   its rules is not implicit permission — assuming it was is how Lightpanda
//!   ended up banned from half of public infrastructure (their #3156/#3234).
//! * private / loopback hosts → exempt (the operator's own network).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use url::Url;

/// Honest product token used both for the robots.txt fetch itself and for
/// `User-agent:` group matching. Never the stealth UA — you don't get to
/// read the rules wearing a borrowed name.
const PRODUCT_TOKEN: &str = "aginxbrowser";

/// How long a successfully fetched (or 404'd) policy stays cached.
fn positive_ttl() -> Duration {
    static TTL: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        Duration::from_secs(
            std::env::var("AGINXBROWSER_ROBOTS_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
        )
    })
}

/// Shorter TTL for refusals, so a temporarily dead robots endpoint doesn't
/// lock the host out (or get hammered) for a full hour.
const NEGATIVE_TTL: Duration = Duration::from_secs(300);

/// RFC 9309 practical limits: cap the body, skip overlong lines.
const MAX_ROBOTS_BYTES: usize = 512 * 1024;
const MAX_LINE_BYTES: usize = 1000;

#[derive(Clone)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Clone)]
enum Policy {
    /// No applicable rules — everything allowed.
    AllowAll,
    /// The selected group(s)' rules; longest matching pattern decides.
    Rules(Vec<Rule>),
    /// The server refused or failed to serve robots.txt — complete disallow,
    /// with the reason carried into the error message.
    DenyAll(String),
}

struct Cached {
    policy: Policy,
    fetched: Instant,
}

static CACHE: LazyLock<tokio::sync::Mutex<HashMap<String, Cached>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Err(reason) when robots.txt disallows fetching `url`.
///
/// Cheap after the first call for a host — the policy is cached.
pub async fn assert_allowed(url: &str) -> Result<(), String> {
    if ignore_env() {
        return Ok(());
    }
    let url = Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(());
    }
    if is_private_host(url.host_str().unwrap_or_default()) {
        return Ok(());
    }

    let key = format!(
        "{}://{}",
        url.scheme(),
        match url.port() {
            Some(p) => format!("{}:{}", url.host_str().unwrap_or_default(), p),
            None => url.host_str().unwrap_or_default().to_string(),
        }
    );

    let policy = {
        let mut cache = CACHE.lock().await;
        if let Some(hit) = cache.get(&key) {
            let ttl = match &hit.policy {
                Policy::DenyAll(_) => NEGATIVE_TTL,
                _ => positive_ttl(),
            };
            if hit.fetched.elapsed() < ttl {
                hit.policy.clone()
            } else {
                cache.remove(&key);
                fetch_policy(&key).await
            }
        } else {
            fetch_policy(&key).await
        }
    };

    match policy {
        Policy::AllowAll => Ok(()),
        Policy::DenyAll(reason) => Err(format!(
            "robots.txt on {key} is unreachable ({reason}); aginxbrowser assumes denial rather than permission in that case. \
             Set AGINXBROWSER_IGNORE_ROBOTS=1 on the server to override."
        )),
        Policy::Rules(rules) => {
            let target = url.path();
            let target = match url.query() {
                Some(q) => format!("{target}?{q}"),
                None => target.to_string(),
            };
            match best_match(&rules, &target) {
                Some(rule) if !rule.allow => Err(format!(
                    "robots.txt disallows {target} on {key} (matched `Disallow: {}`); aginxbrowser honors robots.txt by default. \
                     Set AGINXBROWSER_IGNORE_ROBOTS=1 on the server to override.",
                    rule.pattern
                )),
                _ => Ok(()),
            }
        }
    }
}

/// The operator-level opt-out. Deliberately not a per-request field: the
/// robots stance belongs to whoever runs the instance, not to each caller.
fn ignore_env() -> bool {
    std::env::var("AGINXBROWSER_IGNORE_ROBOTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fetch and classify the host's robots.txt. Uses the honest product UA and
/// the instance's global proxy setting (never the stealth client — and never
/// a borrowed fingerprint) with a short timeout; robots.txt is small.
async fn fetch_policy(origin: &str) -> Policy {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent(format!(
            "{PRODUCT_TOKEN}/{} (+https://browser.aginx.net)",
            env!("CARGO_PKG_VERSION")
        ));
    if let Some(proxy) = crate::config::proxy_from_env() {
        if let Ok(p) = reqwest::Proxy::all(&proxy) {
            builder = builder.proxy(p);
        }
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return Policy::DenyAll(format!("client build: {e}")),
    };

    let resp = match client.get(format!("{origin}/robots.txt")).send().await {
        Ok(r) => r,
        Err(e) => return Policy::DenyAll(format!("network: {e}")),
    };
    match resp.status().as_u16() {
        404 | 410 => Policy::AllowAll,
        200..=299 => {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return Policy::DenyAll(format!("body read: {e}")),
            };
            let body = &body[..body.len().min(MAX_ROBOTS_BYTES)];
            match std::str::from_utf8(body) {
                Ok(text) => parse_rules(text),
                // Bizarre, but not a refusal — parse nothing, allow everything.
                Err(_) => Policy::AllowAll,
            }
        }
        // Explicit refusal (403 etc.) or server trouble (5xx): assume denial.
        code => Policy::DenyAll(format!("http {code}")),
    }
}

/// Parse a robots.txt body into the policy for our product token.
///
/// Groups are runs of `User-agent:` lines followed by rule lines; a
/// `User-agent:` line after rules starts a new group. If any group names our
/// token only those groups apply; otherwise the `*` groups do (RFC 9309
/// §2.2.1 — never both). Junk lines and non-robots content (an HTML error
/// page served with 200) simply yield no rules → allow all, which is both
/// what the RFC's parsers do and what keeps a misbehaving CDN from bricking
/// the whole product.
fn parse_rules(body: &str) -> Policy {
    #[derive(Default)]
    struct Group {
        tokens: Vec<String>,
        rules: Vec<Rule>,
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut current: Option<Group> = None;

    for raw in body.lines() {
        let line = if raw.len() > MAX_LINE_BYTES { "" } else { raw };
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match key.as_str() {
            "user-agent" => {
                // A UA line after rules closes the current group.
                if current.as_ref().is_some_and(|g| !g.rules.is_empty()) {
                    groups.push(current.take().unwrap());
                }
                let g = current.get_or_insert_with(Group::default);
                // "AginxBrowser/1.0" names the product + version; the product
                // token is what group matching compares.
                g.tokens.extend(
                    value
                        .split_whitespace()
                        .map(|t| t.split('/').next().unwrap_or(t).to_ascii_lowercase()),
                );
            }
            "allow" => {
                if let Some(g) = current.as_mut() {
                    g.rules.push(Rule { allow: true, pattern: value });
                }
            }
            "disallow" => {
                if let Some(g) = current.as_mut() {
                    // Bare `Disallow:` (no value) means "allow everything".
                    g.rules.push(Rule { allow: value.is_empty(), pattern: value });
                }
            }
            _ => {}
        }
    }
    if let Some(g) = current {
        groups.push(g);
    }

    let named: Vec<&Group> = groups
        .iter()
        .filter(|g| g.tokens.iter().any(|t| t == PRODUCT_TOKEN))
        .collect();
    let selected: Vec<&Group> = if named.is_empty() {
        groups.iter().filter(|g| g.tokens.iter().any(|t| t == "*")).collect()
    } else {
        named
    };

    let rules: Vec<Rule> = selected.into_iter().flat_map(|g| g.rules.clone()).collect();
    if rules.is_empty() {
        Policy::AllowAll
    } else {
        Policy::Rules(rules)
    }
}

/// Longest matching pattern wins; ties go to Allow (RFC 9309 §2.2.2).
fn best_match<'r>(rules: &'r [Rule], target: &str) -> Option<&'r Rule> {
    rules
        .iter()
        .filter(|r| !r.pattern.is_empty() && rule_matches(&r.pattern, target))
        .max_by_key(|r| (r.pattern.len(), r.allow))
}

/// Glob match where `*` spans anything and `$` anchors the end; compared by
/// DP over chars so pathological patterns stay linear-ish (no regex dep).
fn rule_matches(pattern: &str, target: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = target.chars().collect();
    // Trailing `$` only anchors when it is the final char.
    let anchored = p.last() == Some(&'$');
    let p = if anchored { &p[..p.len() - 1] } else { &p[..] };

    // dp[j]: the pattern consumed so far matches target[..j].
    let mut dp = vec![false; t.len() + 1];
    dp[0] = true;
    for &c in p.iter() {
        let mut next = vec![false; t.len() + 1];
        if c == '*' {
            // `*` consumes zero or more chars: once a column is reachable,
            // every column to its right is too.
            let mut acc = false;
            for j in 0..=t.len() {
                acc = acc || dp[j];
                next[j] = acc;
            }
        } else {
            for j in 1..=t.len() {
                next[j] = dp[j - 1] && t[j - 1] == c;
            }
        }
        dp = next;
    }
    if anchored {
        dp[t.len()]
    } else {
        dp.iter().any(|&x| x)
    }
}

/// Loopback, RFC1918, link-local, and .local/.internal names are the
/// operator's own turf — no robots ceremony for them.
fn is_private_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".local") || h.ends_with(".internal") {
        return true;
    }
    let ip = match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127 || o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168) || (o[0] == 169 && o[1] == 254)
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(allow: bool, pattern: &str) -> Rule {
        Rule { allow, pattern: pattern.to_string() }
    }

    #[test]
    fn parse_disallow_all() {
        let p = parse_rules("User-agent: *\nDisallow: /");
        match p {
            Policy::Rules(rs) => assert_eq!(rs.len(), 1),
            _ => panic!("expected rules"),
        }
    }

    #[test]
    fn product_group_beats_star_group() {
        let p = parse_rules(
            "User-agent: *\nDisallow: /\n\nUser-agent: AginxBrowser\nDisallow: /private",
        );
        match p {
            Policy::Rules(rs) => {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].pattern, "/private");
            }
            _ => panic!("expected rules"),
        }
    }

    #[test]
    fn star_group_ignored_when_product_group_exists() {
        // RFC 9309 §2.2.1: never combine; the most specific group wins.
        let p = parse_rules(
            "User-agent: aginxbrowser\nAllow: /\n\nUser-agent: *\nDisallow: /",
        );
        match p {
            Policy::Rules(rs) => {
                assert!(rs.iter().all(|r| r.allow));
            }
            _ => panic!("expected rules"),
        }
    }

    #[test]
    fn empty_disallow_means_allow_everything() {
        let p = parse_rules("User-agent: *\nDisallow:");
        match p {
            Policy::Rules(rs) => assert!(rs[0].allow && rs[0].pattern.is_empty()),
            _ => panic!("expected rules"),
        }
    }

    #[test]
    fn garbage_yields_allow_all() {
        assert!(matches!(parse_rules("<html><body>challenge page</body>"), Policy::AllowAll));
        assert!(matches!(parse_rules(""), Policy::AllowAll));
        assert!(matches!(parse_rules("# just a comment"), Policy::AllowAll));
    }

    #[test]
    fn longest_match_wins_ties_to_allow() {
        let rs = vec![rule(false, "/"), rule(true, "/p")];
        assert_eq!(best_match(&rs, "/p").unwrap().allow, true);
        assert_eq!(best_match(&rs, "/x").unwrap().allow, false);
        // Same length: Allow wins.
        let rs = vec![rule(false, "/a"), rule(true, "/b")];
        assert!(best_match(&rs, "/a").unwrap().allow == false);
    }

    #[test]
    fn wildcard_and_anchor() {
        assert!(rule_matches("/*.pdf", "/docs/file.pdf"));
        assert!(!rule_matches("/*.pdf", "/docs/file.txt"));
        assert!(rule_matches("/private$", "/private"));
        assert!(!rule_matches("/private$", "/privateX"));
        assert!(rule_matches("/a*/c", "/a/b/c"));
        assert!(rule_matches("/", "/anything/here"));
    }

    #[test]
    fn private_hosts() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.1.2.3"));
        assert!(is_private_host("192.168.1.1"));
        assert!(is_private_host("172.16.0.5"));
        assert!(is_private_host("[::1]".trim_matches(|c| c == '[' || c == ']')));
        assert!(is_private_host("printer.local"));
        assert!(!is_private_host("example.com"));
        assert!(!is_private_host("8.8.8.8"));
    }

    #[test]
    fn ua_tokens_split_and_lowercased() {
        // "User-agent: FooBot AginxBrowser/1.0" matches on the token split.
        let p = parse_rules("User-agent: FooBot aginxbrowser/1.0\nDisallow: /no");
        match p {
            Policy::Rules(rs) => assert_eq!(rs[0].pattern, "/no"),
            _ => panic!("expected rules"),
        }
    }
}
