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
//! captcha / risk_markers plus a disk overlay for platform login fronts
//! (`config/verdict_rules.json`). No screenshots, no page evals, no
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
#[derive(Debug, Clone)]
pub struct UrlRule {
    pub needle: String,
    pub verdict: Verdict,
}

impl UrlRule {
    pub fn new(needle: impl Into<String>, verdict: Verdict) -> Self {
        Self {
            needle: needle.into(),
            verdict,
        }
    }
}

fn parse_verdict_name(s: &str) -> Option<Verdict> {
    match s {
        "challenge" => Some(Verdict::Challenge),
        "captcha" => Some(Verdict::Captcha),
        "login" => Some(Verdict::Login),
        "empty" => Some(Verdict::Empty),
        "landed" => Some(Verdict::Landed),
        "unknown" => Some(Verdict::Unknown),
        _ => None,
    }
}

/// Generic walls that stay in Rust: Ali TMD (via [`crate::risk_markers`])
/// plus Cloudflare / Google / Baidu / QQ captcha interstitials.
/// Platform login fronts live in `config/verdict_rules.json`.
pub fn core_rules() -> Vec<UrlRule> {
    vec![
        UrlRule::new(crate::risk_markers::ALI_TMD_PUNISH_PATH, Verdict::Challenge),
        UrlRule::new(crate::risk_markers::ALI_PUNISH_HOSTS[0], Verdict::Challenge),
        UrlRule::new(crate::risk_markers::ALI_PUNISH_HOSTS[1], Verdict::Challenge),
        UrlRule::new("sorry.google.com", Verdict::Captcha),
        UrlRule::new("/sorry/", Verdict::Captcha),
        UrlRule::new("/antispider", Verdict::Captcha),
        UrlRule::new("wappass.baidu.com", Verdict::Captcha),
        UrlRule::new("captcha.qq.com", Verdict::Captcha),
        UrlRule::new("challenges.cloudflare.com", Verdict::Captcha),
        UrlRule::new("challenge-platform", Verdict::Captcha),
    ]
}

/// Embedded fallback — same bytes as repo `config/verdict_rules.json` so
/// tests and bare binaries keep login classification without a cwd file.
const EMBEDDED_VERDICT_RULES_JSON: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/config/verdict_rules.json"));

/// Parse an overlay document `{ "rules": [ { "needle", "verdict" }, ... ] }`.
/// Unknown verdict names and empty needles are skipped (warn via tracing).
pub fn parse_overlay_json(raw: &str) -> Vec<UrlRule> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        tracing::warn!("verdict_rules: overlay JSON did not parse");
        return Vec::new();
    };
    let Some(arr) = v.get("rules").and_then(|r| r.as_array()) else {
        tracing::warn!("verdict_rules: overlay missing rules[]");
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(needle) = item.get("needle").and_then(|n| n.as_str()) else {
            continue;
        };
        if needle.is_empty() {
            continue;
        }
        let Some(name) = item.get("verdict").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(verdict) = parse_verdict_name(name) else {
            tracing::warn!("verdict_rules: unknown verdict {name:?} for needle {needle:?}");
            continue;
        };
        out.push(UrlRule::new(needle, verdict));
    }
    out
}

fn overlay_candidates() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("AGINXBROWSER_VERDICT_RULES") {
        paths.push(std::path::PathBuf::from(p));
    }
    paths.push(std::path::PathBuf::from("config/verdict_rules.json"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("config/verdict_rules.json"));
        }
    }
    paths
}

fn load_overlay_rules() -> Vec<UrlRule> {
    for path in overlay_candidates() {
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let rules = parse_overlay_json(&raw);
                tracing::debug!(
                    "verdict_rules: loaded {} overlay rule(s) from {}",
                    rules.len(),
                    path.display()
                );
                return rules;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    "verdict_rules: failed to read {}: {e} — falling through",
                    path.display()
                );
            }
        }
    }
    parse_overlay_json(EMBEDDED_VERDICT_RULES_JSON)
}

fn build_effective_rules() -> Vec<UrlRule> {
    // Core first: challenge/captcha cannot be shadowed by a login overlay
    // needle (verdict_for returns on first hit).
    let mut rules = core_rules();
    rules.extend(load_overlay_rules());
    rules
}

/// Effective rule table: core walls + disk/embedded platform login overlay.
/// Cached for the process lifetime (edit the file → restart to pick up).
pub fn rules() -> &'static [UrlRule] {
    static RULES: std::sync::OnceLock<Vec<UrlRule>> = std::sync::OnceLock::new();
    RULES.get_or_init(build_effective_rules).as_slice()
}

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
        if lower.contains(&rule.needle) {
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

/// The full fact sheet (the REST/flow step payload):
/// `{verdict, url, facts, signals, elapsed_ms}` plus `account` and the
/// human-handoff instruction when the verdict is `challenge`.
pub fn fact_sheet(input: &VerdictInput, elapsed_ms: u64) -> Value {
    let (verdict, signals) = verdict_for(input, rules());
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
            let (got, _) = verdict_for(&input(url), rules());
            assert_eq!(got, want, "url {url}");
        }
    }

    /// Swallowed risk-control bodies beat every other signal, including
    /// a current URL that sits on a login form.
    #[test]
    fn risk_control_rows_outrank_url_rules() {
        let mut i = input("https://login.taobao.com/member/login.jhtml");
        i.challenge_events = 2;
        let (v, signals) = verdict_for(&i, rules());
        assert_eq!(v, Verdict::Challenge);
        assert_eq!(signals, ["risk_control_rows:2"]);
    }

    #[test]
    fn blank_doc_and_status_semantics() {
        // about:blank, nothing ever happened → empty.
        let mut i = input("about:blank");
        i.doc_status = None;
        i.requests = 0;
        assert_eq!(verdict_for(&i, rules()).0, Verdict::Empty);
        // about:blank with traffic = set_content landing.
        i.requests = 3;
        assert_eq!(verdict_for(&i, rules()).0, Verdict::Landed);
        // 2xx document → landed; a near-empty 200 → empty.
        let mut i = input("https://www.zhihu.com/question/1");
        assert_eq!(verdict_for(&i, rules()).0, Verdict::Landed);
        i.doc_bytes = Some(40);
        assert_eq!(verdict_for(&i, rules()).0, Verdict::Empty);
        // Non-2xx is neither wall nor content — unknown with the status
        // in facts (the zhihu burst-403 backoff case).
        i.doc_status = Some(403);
        i.doc_bytes = Some(4_000);
        let (v, signals) = verdict_for(&i, rules());
        assert_eq!(v, Verdict::Unknown);
        assert!(signals.contains(&"doc_status_403".to_string()), "{signals:?}");
        // No document event, no rule → unknown.
        let mut i = input("https://example.com/x");
        i.doc_status = None;
        i.requests = 0;
        let (v, _) = verdict_for(&i, rules());
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

    #[test]
    fn overlay_json_loads_login_needles() {
        let overlay = parse_overlay_json(EMBEDDED_VERDICT_RULES_JSON);
        assert!(
            overlay.iter().any(|r| r.needle == "xiaohongshu.com/login" && r.verdict == Verdict::Login),
            "embedded overlay must carry xhs login: {overlay:?}"
        );
        // effective table = core + overlay
        let all = rules();
        assert!(all.iter().any(|r| r.needle.contains("punish.taobao.com")));
        assert!(all.iter().any(|r| r.needle == "login.taobao.com"));
    }

    /// The table is pluggable: a caller (or a future site pack) can
    /// classify what the defaults don't know without touching this
    /// module.
    #[test]
    fn custom_rules_extend_classification() {
        let rules = [UrlRule::new("old.reddit.com/login", Verdict::Login)];
        let (v, signals) = verdict_for(&input("https://old.reddit.com/login?dest=x"), &rules);
        assert_eq!(v, Verdict::Login);
        assert_eq!(signals, ["url_rule:old.reddit.com/login"]);
    }
}
