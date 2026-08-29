//! Per-domain page-rate limiting and per-session page budgets — the product
//! shape that keeps aginxbrowser a real-time retrieval tool for agents
//! instead of a crawler.
//!
//! The stance (see README "A browser, not a crawler"): an agent arrives with
//! a question, reads a handful of pages, leaves with the answer. There is no
//! site-walking API, robots.txt is honored by default, and these two budgets
//! bound what any caller can do per minute / per session. They are deliberately
//! generous for real work (an agent grinding through a doc site or a console
//! fits easily) and deliberately fatal for the crawl pattern — same-domain
//! page after page, minute after minute.
//!
//! Where the gate counts: page loads only, at the outermost entry per path
//! (`smart_fetch`, `do_fetch` via MCP, click/eval/screenshot/download,
//! search body-grabs, firecrawl scrapes, session navigations). Subresources
//! a page pulls are the page's business and never counted. CDP is exempt:
//! it is a raw automation surface by design, like Chrome's remote port.
//!
//! Operators own their instance: `AGINXBROWSER_DOMAIN_RATE_PER_MIN` and
//! `AGINXBROWSER_SESSION_PAGE_LIMIT` (0 disables either). Hosted runs set
//! tighter values; self-hosted defaults are the loose ones.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);

fn per_minute() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("AGINXBROWSER_DOMAIN_RATE_PER_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20)
    })
}

pub fn session_page_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("AGINXBROWSER_SESSION_PAGE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
    })
}

struct Window {
    count: u32,
    started: Instant,
}

static DOMAINS: LazyLock<Mutex<HashMap<String, Window>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Err with the stance message when `url`'s site is over the per-minute page
/// budget. Records the attempt BEFORE the fetch runs: a rate limit that only
/// counts successes is a rate limit you escape by hammering 404s. Invalid
/// URLs and private/loopback hosts pass (they fail later at fetch, and the
/// operator's own network is no one's to throttle).
pub fn check_domain(url: &str) -> Result<(), String> {
    let limit = per_minute();
    if limit == 0 {
        return Ok(());
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(());
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Ok(());
    }
    let Some(host) = parsed.host_str() else {
        return Ok(());
    };
    if crate::robots::is_private_host(host) {
        return Ok(());
    }
    let site = site_bucket(host);

    let mut map = DOMAINS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let window = map.entry(site.clone()).or_insert(Window { count: 0, started: now });
    if now.duration_since(window.started) >= WINDOW {
        window.count = 0;
        window.started = now;
    }
    if window.count >= limit {
        return Err(format!(
            "rate limit: {site} is capped at {limit} pages/min — aginxbrowser does real-time lookups for agents, not site crawling. \
             Slow down, or self-host and tune AGINXBROWSER_DOMAIN_RATE_PER_MIN."
        ));
    }
    window.count += 1;

    // Bounded map: once large, drop windows that already expired.
    if map.len() > 4096 {
        map.retain(|_, w| now.duration_since(w.started) < WINDOW);
    }
    Ok(())
}

/// Err when a session has spent its page budget. Called before session
/// navigations and navigation-causing clicks; reads (state/scroll/eval on
/// the current page) stay free — the budget bounds how many *pages* a
/// session walks, not how long an agent works one page.
pub fn check_page_budget(loaded: u32) -> Result<(), String> {
    let limit = session_page_limit();
    if limit == 0 || loaded < limit {
        return Ok(());
    }
    Err(format!(
        "rate limit: session page budget exhausted ({limit} pages) — aginxbrowser sessions are for interactive agent work, not bulk page walks. \
         Open a new session, or self-host and tune AGINXBROWSER_SESSION_PAGE_LIMIT."
    ))
}

/// The rate-limit bucket for a host: the registrable domain, so a crawler
/// rotating `www.` / `api.` / random subdomains drains one shared budget.
/// Without a full public-suffix list, two-part suffixes come from a short
/// built-in set (the common ones where last-two-labels would lump unrelated
/// registrants, e.g. everything under .co.uk); every other host buckets by
/// its last two labels.
fn site_bucket(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let last_two = labels[labels.len() - 2..].join(".");
    if TWO_PART_SUFFIXES.contains(&last_two.as_str()) && labels.len() >= 3 {
        labels[labels.len() - 3..].join(".")
    } else {
        last_two
    }
}

/// Common multi-part public suffixes where last-two-labels bucketing would
/// merge unrelated sites into one budget.
const TWO_PART_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk",
    "co.jp", "or.jp", "ne.jp",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn", "ac.cn",
    "com.au", "net.au", "org.au",
    "com.br", "com.mx", "com.ar",
    "co.in", "co.nz", "co.za", "co.kr",
    "com.sg", "com.hk", "com.tw", "com.tr", "com.ru",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn n_ok(url: &str, n: u32) -> u32 {
        // Count how many consecutive check_domain calls pass for `url`.
        let mut ok = 0;
        for _ in 0..n {
            if check_domain(url).is_ok() {
                ok += 1;
            } else {
                break;
            }
        }
        ok
    }

    #[test]
    fn subdomains_share_one_budget() {
        // (test env default is 20/min) — www/api/random subdomains of one
        // registrable domain drain the same window.
        assert_eq!(n_ok("https://www.example.com/p1", 15), 15);
        assert_eq!(n_ok("https://api.example.com/p2", 5), 5);
        assert!(check_domain("https://other.example.com/p3").is_err());
    }

    #[test]
    fn two_part_suffix_buckets_split_registrants() {
        // Two registrants under .co.uk must not share a budget.
        assert!(check_domain("https://shop-a.co.uk/x").is_ok());
        assert!(check_domain("https://shop-b.co.uk/x").is_ok());
    }

    #[test]
    fn distinct_sites_have_distinct_budgets() {
        assert!(check_domain("https://aaa-examplesite.org/x").is_ok());
        assert!(check_domain("https://bbb-examplesite.org/x").is_ok());
    }

    #[test]
    fn rejection_carries_the_stance() {
        n_ok("https://rate-carpets.com/a", 20);
        let err = check_domain("https://rate-carpets.com/b").unwrap_err();
        assert!(err.starts_with("rate limit:"));
        assert!(err.contains("rate-carpets.com"));
        assert!(err.contains("not site crawling"));
        assert!(err.contains("AGINXBROWSER_DOMAIN_RATE_PER_MIN"));
    }

    #[test]
    fn private_and_non_http_pass_free() {
        assert!(check_domain("http://127.0.0.1:8089/x").is_ok());
        assert!(check_domain("http://localhost:1/x").is_ok());
        assert!(check_domain("file:///etc/hosts").is_ok());
        assert!(check_domain("not a url").is_ok());
    }

    #[test]
    fn page_budget_message() {
        let err = check_page_budget(200).unwrap_err();
        assert!(err.starts_with("rate limit:"));
        assert!(err.contains("AGINXBROWSER_SESSION_PAGE_LIMIT"));
        assert!(check_page_budget(199).is_ok());
        assert!(check_page_budget(0).is_ok());
    }

    #[test]
    fn trailing_dot_and_case_normalized() {
        assert_eq!(site_bucket("WWW.Example.COM."), "example.com");
        assert_eq!(site_bucket("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(site_bucket("example.com"), "example.com");
        assert_eq!(site_bucket("test.co.uk"), "test.co.uk");
    }
}
