//! Shared anti-bot / risk-control markers (Ali TMD / x5 / MTop, etc.).
//!
//! `har`, `captcha`, and `verdict` used to each hardcode the same needles and
//! drifted. Observation stays in core (detect + surface, never bypass);
//! site *business* (signing, publish) stays in `workflow/`. Login-front URL
//! rules remain in `verdict::DEFAULT_RULES` — this module is the Ali-shaped
//! wall cluster plus helpers the three faces share.

/// Path-shaped TMD punish marker (also appears inside MTop JSON bodies).
pub const ALI_TMD_PUNISH_PATH: &str = "_____tmd_____/punish";

/// Hosts that *are* the x5sec slider landing — host-shaped, unambiguous.
pub const ALI_PUNISH_HOSTS: &[&str] = &["punish.taobao.com", "punish.tmall.com"];

/// Query/token fragment that means risk control has engaged (captcha face).
/// Broader than path/host — a bare `x5sec=` on an otherwise clean URL.
pub const ALI_X5SEC_TOKEN: &str = "x5sec";

/// MTop / JSON body markers: HTTP 200 with an ordinary-looking payload
/// whose `ret` / fields say the request was walled.
pub const ALI_BODY_MARKERS: &[&str] = &[
    "FAIL_SYS_USER_VALIDATE",
    "RGV587",
    "x5secdata",
    ALI_TMD_PUNISH_PATH,
];

/// 1688 (and kin) in-place punish page title — body-only signal when the
/// product URL answers 200 without a punish host.
pub const ALI_CAPTCHA_TITLE_ZH: &str = "\u{9a8c}\u{8bc1}\u{7801}\u{62e6}\u{622a}"; // 验证码拦截

/// Short tag echoed on har/session challenge rows.
pub const ALI_CHALLENGE_TAG: &str = "punish";

/// URL-shape challenge for the network log face: strip query/fragment
/// before matching the path needle so a mere `?_____tmd_____/punish=`
/// query name never trips it; hosts match on the full lowercased URL.
pub fn ali_url_challenge_kind(url: &str) -> Option<&'static str> {
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    let path_l = bare.to_ascii_lowercase();
    if path_l.contains(ALI_TMD_PUNISH_PATH) {
        return Some(ALI_CHALLENGE_TAG);
    }
    let full_l = url.to_ascii_lowercase();
    for host in ALI_PUNISH_HOSTS {
        if full_l.contains(host) {
            return Some(ALI_CHALLENGE_TAG);
        }
    }
    None
}

/// Captcha/verdict-style URL hit: substring on the full URL (query counts),
/// plus the broader `x5sec` token used by the captcha detector.
pub fn ali_url_looks_walled(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.contains(ALI_TMD_PUNISH_PATH) {
        return true;
    }
    for host in ALI_PUNISH_HOSTS {
        if lower.contains(host) {
            return true;
        }
    }
    // captcha.rs historically tripped on any `x5sec` substring (token on
    // punish landings and post-solve cookies in the URL).
    if lower.contains(ALI_X5SEC_TOKEN) {
        return true;
    }
    false
}

/// Body/HTML looks like an Ali risk-control payload or in-place punish page.
pub fn ali_body_looks_walled(body: &str) -> bool {
    for m in ALI_BODY_MARKERS {
        if body.contains(m) {
            return true;
        }
    }
    if body.contains(ALI_CAPTCHA_TITLE_ZH) {
        return true;
    }
    // Script-driven redirects embed punish hosts in HTML without the
    // page URL changing yet.
    let lower = body.to_ascii_lowercase();
    for host in ALI_PUNISH_HOSTS {
        if lower.contains(host) {
            return true;
        }
    }
    if lower.contains(ALI_TMD_PUNISH_PATH) {
        return true;
    }
    false
}

/// Body-only challenge tag for har (MTop JSON) — same markers, no HTML title
/// (har retained bodies are API JSON; title is a captcha/HTML concern).
pub fn ali_body_challenge_kind(body: &str) -> Option<&'static str> {
    for m in ALI_BODY_MARKERS {
        if body.contains(m) {
            return Some(ALI_CHALLENGE_TAG);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn har_style_strips_query_for_path_needle() {
        // Path form trips; query-only name does not (har convention).
        assert_eq!(
            ali_url_challenge_kind(
                "https://h5api.m.taobao.com/h5/x/1.0/_____tmd_____/punish"
            ),
            Some("punish")
        );
        assert!(ali_url_challenge_kind(
            "https://h5api.m.taobao.com/h5/x/1.0/?_____tmd_____/punish=x"
        )
        .is_none());
        assert_eq!(
            ali_url_challenge_kind("https://punish.taobao.com/auth?x5sec=abc"),
            Some("punish")
        );
    }

    #[test]
    fn captcha_style_query_and_x5sec_count() {
        assert!(ali_url_looks_walled(
            "https://h5api.m.taobao.com/h5/x/1.0/?_____tmd_____/punish=x"
        ));
        assert!(ali_url_looks_walled("https://punish.tmall.com/x"));
        assert!(ali_url_looks_walled(
            "https://punish.taobao.com/auth?x5sec=abcd"
        ));
        assert!(!ali_url_looks_walled(
            "https://h5api.m.taobao.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/"
        ));
    }

    #[test]
    fn body_markers_shared() {
        assert!(ali_body_looks_walled(
            r#"{"ret":["FAIL_SYS_USER_VALIDATE"],"data":{"x5secdata":"z"}}"#
        ));
        assert_eq!(
            ali_body_challenge_kind(r#"{"x5secdata":"zzz"}"#),
            Some("punish")
        );
        assert!(ali_body_looks_walled(&format!(
            "<title>{ALI_CAPTCHA_TITLE_ZH}</title>"
        )));
        assert!(ali_body_challenge_kind(r#"{"ok":true}"#).is_none());
    }
}
