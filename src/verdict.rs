//! Session verdict — the decision layer's fact sheet (issue #73).
//!
//! One call answers "where did this session land": `challenge` (risk
//! control engaged — punish page, or a 200-status MTop body that
//! swallowed the wall), `captcha` (explicit CAPTCHA interstitial),
//! `login` (bounced to a login form), `empty`, `landed`, or `unknown`
//! (couldn't classify — check the facts).
//!
//! v1 is deliberately code-only and eval-free: it reads signals the
//! engine already has (current URL, the risk-control rows
//! [`crate::har::challenge_rows`] builds, the main document's status and
//! size, console errors) and runs a rule table generalized out of
//! captcha.rs's hardcoded markers. No screenshots, no page evals, no
//! model — the target is single-digit milliseconds. The honest limit:
//! verdict sees network and URL facts, not the DOM — a rich set_content
//! page with zero subresource traffic reads as `empty`.
//!
//! Verdict observes; it never bypasses. On `challenge` the sheet carries
//! the same human-handoff instruction as session_challenges — a person
//! solves the slider in the live view and the retry rides the cookie.

use serde_json::{json, Value};

/// The six verdicts, most to least specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Risk control engaged (punish page / walled API body). Needs a
    /// human handoff or a backoff — retrying the same identity now
    /// re-hits the wall.
    Challenge,
    /// An explicit CAPTCHA interstitial (reCAPTCHA / Turnstile /
    /// antispider page), not risk-control JSON.
    Captcha,
    /// The page bounced to a login form — auth expired or never held.
    Login,
    /// Nothing meaningful rendered (blank page, no traffic).
    Empty,
    /// A normal content page (2xx document, no wall signals).
    Landed,
    /// Couldn't classify — read `facts` (e.g. a 403/412 document is
    /// neither a wall page nor content; the caller decides what a
    /// non-2xx means for its retry policy).
    Unknown,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Challenge => "challenge",
            Verdict::Captcha => "captcha",
            Verdict::Login => "login",
            Verdict::Empty => "empty",
            Verdict::Landed => "landed",
            Verdict::Unknown => "unknown",
        }
    }
}

/// One URL-shape classification rule: `needle` is substring-matched
/// against the lowercased current URL. Host-shaped needles
/// ("punish.taobao.com") are unambiguous; path-shaped ones
/// ("_____tmd_____/punish", "/signin") stay narrow on purpose — a
/// marker that fires on ordinary article URLs is worse than no rule.
#[derive(Debug)]
pub struct UrlRule {
    pub needle: &'static str,
    pub verdict: Verdict,
}

/// The seed rule table: captcha.rs's hardcoded markers generalized, plus
/// the login fronts of the sites the playbook flows drive. A needle
/// appearing in two rules resolves by list order — keep challenge and
/// captcha needles ahead of login ones.
pub const DEFAULT_RULES: &[UrlRule] = &[
    // Risk control (taobao/tmall TMD: punish landings and x5 tokens).
    UrlRule { needle: "_____tmd_____/punish", verdict: Verdict::Challenge },
    UrlRule { needle: "punish.taobao.com", verdict: Verdict::Challenge },
    UrlRule { needle: "punish.tmall.com", verdict: Verdict::Challenge },
    // CAPTCHA interstitials.
    UrlRule { needle: "sorry.google.com", verdict: Verdict::Captcha },
    UrlRule { needle: "/sorry/", verdict: Verdict::Captcha },
    UrlRule { needle: "/antispider", verdict: Verdict::Captcha },
    UrlRule { needle: "wappass.baidu.com", verdict: Verdict::Captcha },
    UrlRule { needle: "captcha.qq.com", verdict: Verdict::Captcha },
    UrlRule { needle: "challenges.cloudflare.com", verdict: Verdict::Captcha },
    UrlRule { needle: "challenge-platform", verdict: Verdict::Captcha },
    // Login fronts.
    UrlRule { needle: "login.taobao.com", verdict: Verdict::Login },
    UrlRule { needle: "login.tmall.com", verdict: Verdict::Login },
    UrlRule { needle: "passport.zhihu.com/signin", verdict: Verdict::Login },
    UrlRule { needle: "accounts.google.com/servicelogin", verdict: Verdict::Login },
    UrlRule { needle: "x.com/login", verdict: Verdict::Login },
    UrlRule { needle: "twitter.com/login", verdict: Verdict::Login },
    UrlRule { needle: "passport.bilibili.com/login", verdict: Verdict::Login },
    UrlRule { needle: "xiaohongshu.com/login", verdict: Verdict::Login },
    UrlRule { needle: "weibo.com/login", verdict: Verdict::Login },
];

/// The signals a session can hand the classifier, all collected without
/// evals by the session thread.
pub struct VerdictInput<'a> {
    pub url: &'a str,
    pub session_id: &'a str,
    /// Named login identity running the session, when there is one.
    pub account: Option<&'a str>,
    /// Rows from [`crate::har::challenge_rows`] — risk-control walls
    /// the traffic hit (URL-shaped landings AND 200-status bodies).
    pub challenge_events: usize,
    /// The main document request's HTTP status, when one was seen.
    pub doc_status: Option<u16>,
    /// The main document's response size in bytes, when known.
    pub doc_bytes: Option<usize>,
    /// Total network requests the session issued.
    pub requests: usize,
    /// Error-level console entries (count).
    pub console_errors: usize,
    /// Last few error texts (capped by the caller).
    pub console_last: Vec<String>,
}

/// Classify. Decision order, most urgent signal first:
///
/// 1. risk-control rows > 0 → `challenge` — a swallowed MTop wall wins
///    even when the current URL looks clean, and it out-ranks a login
///    bounce (risk control is the state that must not be retried into).
/// 2. current-URL rule hit → that rule's verdict.
/// 3. `about:blank` with zero traffic → `empty`; with traffic (the
///    set_content case) → `landed`.
/// 4. main document: 2xx/304 → `landed` (a <256-byte 200 → `empty`);
///    any other status → `unknown` with the status in facts.
/// 5. no document event and no rule → `unknown`.
///
/// Returns the verdict plus the machine-readable signals that fired
/// (why this verdict — auditable, and what the ablation pass counts).
pub fn verdict_for(input: &VerdictInput, rules: &[UrlRule]) -> (Verdict, Vec<String>) {
    let mut signals = Vec::new();
    if input.challenge_events > 0 {
        signals.push(format!("risk_control_rows:{}", input.challenge_events));
        return (Verdict::Challenge, signals);
    }
    let lower = input.url.to_ascii_lowercase();
    for rule in rules {
        if lower.contains(rule.needle) {
            signals.push(format!("url_rule:{}", rule.needle));
            return (rule.verdict, signals);
        }
    }
    if lower == "about:blank" {
        if input.requests == 0 {
            signals.push("blank_no_traffic".into());
            return (Verdict::Empty, signals);
        }
        // about:blank with live traffic is the set_content shape — the
        // flow landed content, there just is no document event to vouch.
        signals.push("blank_with_traffic".into());
        return (Verdict::Landed, signals);
    }
    if let Some(status) = input.doc_status {
        if (200..300).contains(&status) || status == 304 {
            if input.doc_bytes.is_some_and(|b| b < 256) {
                signals.push("doc_tiny".into());
                return (Verdict::Empty, signals);
            }
            signals.push(format!("doc_{status}"));
            return (Verdict::Landed, signals);
        }
        // 403/412/429...: not a wall page, not content — the caller
        // owns what a non-2xx means (zhihu burst 403 = backoff, not
        // handoff). facts.doc_status carries the number.
        signals.push(format!("doc_status_{status}"));
        return (Verdict::Unknown, signals);
    }
    signals.push("no_document_event".into());
    (Verdict::Unknown, signals)
}

/// The full fact sheet (the MCP/REST/flow step payload):
/// `{verdict, url, facts, signals, elapsed_ms}` plus `account` and the
/// human-handoff instruction when the verdict is `challenge`.
pub fn fact_sheet(input: &VerdictInput, elapsed_ms: u64) -> Value {
    let (verdict, signals) = verdict_for(input, DEFAULT_RULES);
    let mut facts = json!({
        "challenge_events": input.challenge_events,
        "requests": input.requests,
        "console_errors": input.console_errors,
    });
    if let Some(s) = input.doc_status {
        facts["doc_status"] = json!(s);
    }
    if let Some(b) = input.doc_bytes {
        facts["doc_bytes"] = json!(b);
    }
    if !input.console_last.is_empty() {
        facts["console_last"] = json!(input.console_last);
    }
    let mut sheet = json!({
        "verdict": verdict.as_str(),
        "url": input.url,
        "facts": facts,
        "signals": signals,
        "elapsed_ms": elapsed_ms,
    });
    if let Some(name) = input.account {
        sheet["account"] = json!(name);
    }
    if verdict == Verdict::Challenge {
        // Same contract as session_challenges: detect and surface, never
        // auto-bypass. A human solves it in the live view and the retry
        // rides the cookie that solving sets.
        sheet["handoff"] = json!(format!(
            "risk control engaged — hand this session to a human: open \
             /live?session={} on this engine's HTTP port, solve the \
             challenge there, then retry in this session (or branch on a \
             backoff if the wall is a swallowed API body)",
            input.session_id
        ));
    }
    sheet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(url: &'a str) -> VerdictInput<'a> {
        VerdictInput {
            url,
            session_id: "s-test",
            account: None,
            challenge_events: 0,
            doc_status: Some(200),
            doc_bytes: Some(40_000),
            requests: 12,
            console_errors: 0,
            console_last: vec![],
        }
    }

    /// The rule table fires on real wall/login URLs and stays quiet on
    /// ordinary content URLs of the same sites.
    #[test]
    fn url_rules_classify_walls_and_logins() {
        let cases = [
            ("https://punish.taobao.com/auth?x5sec=abc", Verdict::Challenge),
            (
                "https://h5api.m.taobao.com/h5/x/1.0/?_____tmd_____/punish=x",
                Verdict::Challenge,
            ),
            ("https://www.google.com/sorry/index?q=x", Verdict::Captcha),
            ("https://t.captcha.qq.com/cap_union_prehandle", Verdict::Captcha),
            ("https://login.taobao.com/member/login.jhtml", Verdict::Login),
            // creator.xiaohongshu.com bounces dead sessions here — the
            // xhs-post dogfood's verdict gate rides this needle.
            ("https://creator.xiaohongshu.com/login?source=official", Verdict::Login),
            // Quiet-table negatives: no rule fires, the healthy 200 doc
            // speaks → landed (Unknown is reserved for non-2xx / no-doc,
            // NOT "table doesn't know this URL").
            ("https://www.zhihu.com/signin?next=%2F", Verdict::Landed),
            ("https://www.taobao.com/item/8123", Verdict::Landed),
            ("https://passport.zhihu.com/signin", Verdict::Login),
            ("https://x.com/login", Verdict::Login),
        ];
        for (url, want) in cases {
            let (got, _) = verdict_for(&input(url), DEFAULT_RULES);
            assert_eq!(got, want, "url {url}");
        }
    }

    /// Swallowed risk-control bodies beat every other signal, including
    /// a current URL that sits on a login form.
    #[test]
    fn risk_control_rows_outrank_url_rules() {
        let mut i = input("https://login.taobao.com/member/login.jhtml");
        i.challenge_events = 2;
        let (v, signals) = verdict_for(&i, DEFAULT_RULES);
        assert_eq!(v, Verdict::Challenge);
        assert_eq!(signals, ["risk_control_rows:2"]);
    }

    #[test]
    fn blank_doc_and_status_semantics() {
        // about:blank, nothing ever happened → empty.
        let mut i = input("about:blank");
        i.doc_status = None;
        i.requests = 0;
        assert_eq!(verdict_for(&i, DEFAULT_RULES).0, Verdict::Empty);
        // about:blank with traffic = set_content landing.
        i.requests = 3;
        assert_eq!(verdict_for(&i, DEFAULT_RULES).0, Verdict::Landed);
        // 2xx document → landed; a near-empty 200 → empty.
        let mut i = input("https://www.zhihu.com/question/1");
        assert_eq!(verdict_for(&i, DEFAULT_RULES).0, Verdict::Landed);
        i.doc_bytes = Some(40);
        assert_eq!(verdict_for(&i, DEFAULT_RULES).0, Verdict::Empty);
        // Non-2xx is neither wall nor content — unknown with the status
        // in facts (the zhihu burst-403 backoff case).
        i.doc_status = Some(403);
        i.doc_bytes = Some(4_000);
        let (v, signals) = verdict_for(&i, DEFAULT_RULES);
        assert_eq!(v, Verdict::Unknown);
        assert!(signals.contains(&"doc_status_403".to_string()), "{signals:?}");
        // No document event, no rule → unknown.
        let mut i = input("https://example.com/x");
        i.doc_status = None;
        i.requests = 0;
        let (v, _) = verdict_for(&i, DEFAULT_RULES);
        assert_eq!(v, Verdict::Unknown);
    }

    /// The fact sheet carries the verdict, the facts it rested on, and
    /// the handoff only for challenge verdicts.
    #[test]
    fn fact_sheet_shape_and_handoff() {
        let clean = fact_sheet(&input("https://www.zhihu.com/question/1"), 3);
        assert_eq!(clean["verdict"], "landed");
        assert_eq!(clean["facts"]["doc_status"], 200);
        assert_eq!(clean["elapsed_ms"], 3);
        assert!(clean.get("handoff").is_none());

        let mut walled = input("https://shop.m.taobao.com/shop");
        walled.challenge_events = 1;
        walled.account = Some("scraper-1");
        let sheet = fact_sheet(&walled, 5);
        assert_eq!(sheet["verdict"], "challenge");
        assert_eq!(sheet["account"], "scraper-1");
        let handoff = sheet["handoff"].as_str().unwrap();
        assert!(handoff.contains("/live?session=s-test"), "{handoff}");
    }

    /// The table is pluggable: a caller (or a future site pack) can
    /// classify what the defaults don't know without touching this
    /// module.
    #[test]
    fn custom_rules_extend_classification() {
        let rules = [UrlRule { needle: "old.reddit.com/login", verdict: Verdict::Login }];
        let (v, signals) = verdict_for(&input("https://old.reddit.com/login?dest=x"), &rules);
        assert_eq!(v, Verdict::Login);
        assert_eq!(signals, ["url_rule:old.reddit.com/login"]);
    }
}
