//! robots.txt compliance (RFC 9309 subset), available on every autonomous
//! fetch path — `/fetch`, `/click`, `/eval`, `/screenshot`, `/download`, the
//! `/search` fetch_top body-grab, and the MCP and firecrawl equivalents.
//! aginxbrowser is a real-time acquisition layer, not a crawler: an agent
//! arrives with a question, reads a few pages, leaves with the answer, and
//! robots.txt is crawler etiquette — not a gate for that. The check is
//! therefore OFF by default; operators who want it opt in with
//! `AGINXBROWSER_HONOR_ROBOTS=1`.
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
//! * other 4xx → allow all — the server declines to serve robots.txt to us,
//!   and RFC 9309 / Google semantics read that as "no rules apply" (a
//!   login-walled robots.txt doesn't restrict anonymous fetching).
//! * 5xx / network failure (after one retry) → deny. Treating a failing
//!   robots.txt as allow-all is how Lightpanda ended up banned from half of
//!   public infrastructure (their #3156/#3234) — the gate denies while the
//!   server is in trouble, with a short negative TTL so recovery is quick.
//!   One carve-out: when the failure is a TLS handshake the default stack
//!   cannot speak at all — a CBC-only TLS 1.2 server (obscura#769) — a
//!   final attempt rides the BoringSSL transport before denying, still
//!   under the honest product UA.
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

#[derive(Clone, Debug)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Clone, Debug)]
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

/// Err(reason) when robots.txt disallows fetching `url`. Only runs at all
/// when the operator opted in (see [`honor_env`]).
///
/// Cheap after the first call for a host — the policy is cached.
pub async fn assert_allowed(url: &str) -> Result<(), String> {
    if !honor_env() {
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
        // Read-side of the cache is lock-scoped; the network fetch happens
        // OUTSIDE the lock (a 5s robots.txt fetch must not serialize every
        // concurrent caller), and the miss path writes back so later calls
        // are actually cached — the original version never inserted, making
        // every request refetch (and hammering hosts into throttling us).
        let cached = {
            let mut cache = CACHE.lock().await;
            match cache.get(&key) {
                Some(hit) => {
                    let ttl = match &hit.policy {
                        Policy::DenyAll(_) => NEGATIVE_TTL,
                        _ => positive_ttl(),
                    };
                    if hit.fetched.elapsed() < ttl {
                        Some(hit.policy.clone())
                    } else {
                        cache.remove(&key);
                        None
                    }
                }
                None => None,
            }
        };
        match cached {
            Some(p) => p,
            None => {
                let fetched = fetch_policy(&key).await;
                let mut cache = CACHE.lock().await;
                cache.insert(key.clone(), Cached { policy: fetched.clone(), fetched: Instant::now() });
                fetched
            }
        }
    };

    match policy {
        Policy::AllowAll => Ok(()),
        Policy::DenyAll(reason) => Err(format!(
            "robots.txt on {key} is unreachable ({reason}); this instance checks robots.txt (AGINXBROWSER_HONOR_ROBOTS=1) and assumes denial rather than permission while it fails. \
             Remove the env to skip the check."
        )),
        Policy::Rules(rules) => {
            let target = url.path();
            let target = match url.query() {
                Some(q) => format!("{target}?{q}"),
                None => target.to_string(),
            };
            match best_match(&rules, &target) {
                Some(rule) if !rule.allow => Err(format!(
                    "robots.txt disallows {target} on {key} (matched `Disallow: {}`). This instance checks robots.txt (AGINXBROWSER_HONOR_ROBOTS=1); remove it to skip the check.",
                    rule.pattern
                )),
                _ => Ok(()),
            }
        }
    }
}

/// The operator-level opt-in. Deliberately not a per-request field: the
/// robots stance belongs to whoever runs the instance, not to each caller.
fn honor_env() -> bool {
    std::env::var("AGINXBROWSER_HONOR_ROBOTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fetch and classify the host's robots.txt. Uses the honest product UA and
/// the instance's global proxy setting on the plain rustls client, with a
/// short timeout; robots.txt is small. When rustls cannot even complete the
/// handshake — a CBC-only TLS 1.2 server (obscura#769) — one last attempt
/// rides the BoringSSL transport ([`fetch_policy_via_legacy_tls`]), still
/// under the honest product UA: what the stealth client lends is its cipher
/// shelf, never its name.
async fn fetch_policy(origin: &str) -> Policy {
    let mut builder = crate::diting_net::client::reqwest_builder_no_env_proxy()
        .timeout(Duration::from_secs(5))
        .user_agent(format!(
            "{PRODUCT_TOKEN}/{} (+https://browser.aginx.net)",
            env!("CARGO_PKG_VERSION")
        ));
    if let Some(proxy) = crate::config::proxy_from_env() {
        // Follow the same proxy decision the page fetch makes for this
        // origin. Unconditional proxying broke China-hosted sites on
        // foreign-exit deployments: robots.txt timed out through the proxy,
        // DenyAll got cached, and /fetch 403'd a page the engine fetches
        // direct just fine (gongkaoleida class). Should the caller force
        // use_proxy for this origin, the page fetch rides the proxy while
        // robots is judged from our direct IP — fine, robots content is
        // path-based, not client-IP-based.
        if crate::config::should_auto_proxy(origin) {
            if let Ok(p) = reqwest::Proxy::all(&proxy) {
                builder = builder.proxy(p);
            }
        }
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return Policy::DenyAll(format!("client build: {e}")),
    };

    // One inline retry on network-level failure before denying: a single
    // 5s-timeout blip must not block a host for the whole negative TTL —
    // this is a real-time fetcher, and the robots gate should deny on the
    // site's refusal, not on our own packet loss. HTTP-level statuses are
    // not retried: a 403/5xx answer IS the site speaking.
    let mut resp = match client.get(format!("{origin}/robots.txt")).send().await {
        Ok(r) => r,
        Err(first) => {
            tokio::time::sleep(Duration::from_millis(750)).await;
            match client.get(format!("{origin}/robots.txt")).send().await {
                Ok(r) => r,
                Err(second) => {
                    // A rustls double-failure includes the obscura#769 case:
                    // CBC-only TLS 1.2 server, TCP fine, every handshake
                    // refused because rustls ships no CBC suites. One last
                    // attempt rides the BoringSSL transport — same honest
                    // UA, same proxy decision — before denying.
                    #[cfg(feature = "stealth")]
                    match fetch_policy_via_legacy_tls(origin).await {
                        Ok(policy) => return policy,
                        Err(legacy) => {
                            return Policy::DenyAll(format!(
                                "network: {first}; retry: {second}; legacy-tls: {legacy}"
                            ))
                        }
                    }
                    #[cfg(not(feature = "stealth"))]
                    return Policy::DenyAll(format!("network: {first}; retry: {second}"));
                }
            }
        }
    };
    let status = resp.status().as_u16();
    if !(200..=299).contains(&status) {
        return policy_from_robots_response(status, &[]);
    }
    // Stream with a hard stop at MAX_ROBOTS_BYTES (upstream #581
    // class): `.bytes().await` would buffer a hostile multi-GB
    // "robots.txt" in full before the truncate below ever ran. A
    // real robots.txt is bytes-to-a-few-KiB; Google parses at most
    // 500 KiB, so the cap loses nothing legitimate.
    let mut body: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ROBOTS_BYTES.saturating_sub(body.len());
                if room == 0 {
                    break;
                }
                let take = chunk.len().min(room);
                body.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(e) => return Policy::DenyAll(format!("body read: {e}")),
        }
    }
    policy_from_robots_response(status, &body)
}

/// Classify a fetched robots.txt (HTTP status + body) into a policy. Shared
/// by both transports so the legacy-TLS fallback speaks identical semantics.
fn policy_from_robots_response(status: u16, body: &[u8]) -> Policy {
    match status {
        404 | 410 => Policy::AllowAll,
        200..=299 => match std::str::from_utf8(body) {
            Ok(text) => parse_rules(text),
            // Bizarre, but not a refusal — parse nothing, allow everything.
            Err(_) => Policy::AllowAll,
        },
        // Other 4xx (403, 401, …): the server declines to serve robots.txt
        // to us. RFC 9309 / Google semantics treat that as "no rules apply"
        // — a walled-off robots.txt does not restrict anonymous fetching.
        400..=499 => Policy::AllowAll,
        // Server trouble (5xx): assume denial — this is the Lightpanda
        // #3156/#3234 lesson: treating a 5xxing robots.txt as allow-all is
        // what hammered a site into crisis. Short negative TTL above.
        code => Policy::DenyAll(format!("http {code}")),
    }
}

/// Last-resort transport for the robots.txt fetch (obscura#769): the
/// primary client is rustls, which carries no TLS 1.2 CBC cipher suites at
/// all — against a CBC-only server both attempts die in the handshake while
/// the site is perfectly healthy for every browser. The stealth transport's
/// BoringSSL stack still speaks CBC, so retry once through it. Identity is
/// unchanged: the borrowed part of the stealth client is its cipher shelf,
/// never its name — the advertised User-Agent (what `User-agent:` group
/// matching keys on) stays the honest product token.
#[cfg(feature = "stealth")]
async fn fetch_policy_via_legacy_tls(origin: &str) -> Result<Policy, String> {
    let client = crate::diting_net::StealthHttpClient::with_proxy(
        std::sync::Arc::new(crate::diting_net::CookieJar::new()),
        None,
    );
    client
        .set_user_agent(&format!(
            "{PRODUCT_TOKEN}/{} (+https://browser.aginx.net)",
            env!("CARGO_PKG_VERSION")
        ))
        .await;
    let url = Url::parse(&format!("{origin}/robots.txt")).map_err(|e| format!("url: {e}"))?;
    let resp = match tokio::time::timeout(Duration::from_secs(5), client.fetch(&url)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("timeout".into()),
    };
    let mut body = resp.body;
    body.truncate(MAX_ROBOTS_BYTES);
    Ok(policy_from_robots_response(resp.status, &body))
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
pub(crate) fn is_private_host(host: &str) -> bool {
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
    fn response_status_classification() {
        // 2xx parses the body; 404/410 and other 4xx allow; 5xx denies.
        assert!(matches!(
            policy_from_robots_response(200, b"User-agent: *\nDisallow: /private"),
            Policy::Rules(_)
        ));
        assert!(matches!(policy_from_robots_response(204, b""), Policy::AllowAll));
        assert!(matches!(policy_from_robots_response(404, b""), Policy::AllowAll));
        assert!(matches!(policy_from_robots_response(410, b""), Policy::AllowAll));
        assert!(matches!(policy_from_robots_response(403, b""), Policy::AllowAll));
        assert!(matches!(policy_from_robots_response(503, b""), Policy::DenyAll(_)));
        // A hostile non-UTF-8 body is not a refusal — allow all.
        assert!(matches!(policy_from_robots_response(200, &[0xff, 0xfe]), Policy::AllowAll));
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

    // The gate is OFF by default — aginxbrowser is a real-time acquisition
    // layer, not a crawler, and robots.txt is crawler etiquette. The env
    // flip is the whole switch; no compat alias for the old
    // AGINXBROWSER_IGNORE_ROBOTS name.
    #[test]
    fn robots_gate_defaults_off_and_honor_env_opts_in() {
        static HONOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env = HONOR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        unsafe { std::env::remove_var("AGINXBROWSER_HONOR_ROBOTS") };
        assert!(!honor_env());
        unsafe { std::env::set_var("AGINXBROWSER_HONOR_ROBOTS", "1") };
        assert!(honor_env());
        unsafe { std::env::set_var("AGINXBROWSER_HONOR_ROBOTS", "true") };
        assert!(honor_env());
        unsafe { std::env::set_var("AGINXBROWSER_HONOR_ROBOTS", "0") };
        assert!(!honor_env());
        unsafe { std::env::remove_var("AGINXBROWSER_HONOR_ROBOTS") };
    }

    // End to end on the gate position: by default assert_allowed returns Ok
    // before ANY work — the unparsable URL proves it (parsing would error).
    // With HONOR_ROBOTS=1 the gate actually runs and the same input errors.
    // Both branches stay network-free, so the test makes no requests.
    #[tokio::test]
    async fn default_allows_without_consulting_robots_honor_runs_the_gate() {
        static HONOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env = HONOR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        unsafe { std::env::remove_var("AGINXBROWSER_HONOR_ROBOTS") };
        assert!(assert_allowed(":::not a url").await.is_ok());

        unsafe { std::env::set_var("AGINXBROWSER_HONOR_ROBOTS", "1") };
        assert!(assert_allowed(":::not a url").await.is_err());
        unsafe { std::env::remove_var("AGINXBROWSER_HONOR_ROBOTS") };
    }

    // With AGINXBROWSER_PROXY set, the robots fetch used to attach the proxy
    // unconditionally — including for origins the page-fetch layer fetches
    // DIRECT (the should_auto_proxy allowlist). On a foreign-exit deployment
    // that made China-hosted sites unfetchable: robots timed out through the
    // proxy, DenyAll was cached, and /fetch 403'd. The fetch must skip the
    // proxy for direct origins even when one is configured.
    #[allow(clippy::await_holding_lock)] // the env guard must span the fetch
    #[tokio::test]
    async fn robots_fetch_skips_proxy_for_direct_origins() {
        static PROXY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env = PROXY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Loopback fixture serving a real robots.txt.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let body = "User-agent: *\nDisallow: /private";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });

        // A configured-but-dead proxy: with the old unconditional attach this
        // request could only fail (nothing listens there), and the policy
        // would come back DenyAll(network).
        unsafe { std::env::set_var("AGINXBROWSER_PROXY", "socks5h://127.0.0.1:1") };
        let policy = fetch_policy(&format!("http://127.0.0.1:{port}")).await;
        unsafe { std::env::remove_var("AGINXBROWSER_PROXY") };

        assert!(
            matches!(policy, Policy::Rules(_)),
            "robots fetch must skip the configured proxy for direct origins, got {policy:?}"
        );
    }
}
