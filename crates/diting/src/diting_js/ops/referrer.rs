//! Referrer-Policy resolution + Referer header derivation (god-file ratchet split from mod.rs, pure move).

use super::*;

/// Parse a comma-separated policy list (header value or meta content) per
/// Referrer Policy §"determine policy for token": tokens are
/// case-insensitive, invalid ones are skipped, the LAST valid token wins.
/// None = the whole value carries no policy.
pub(crate) fn last_valid_referrer_token(value: &str) -> Option<String> {
    let mut found = None;
    for tok in value.split(',') {
        let t = tok.trim().to_ascii_lowercase();
        if REFERRER_POLICY_TOKENS.contains(&t.as_str()) {
            found = Some(t);
        }
    }
    found
}

/// The document's own referrer policy (Referrer Policy §"Determine request's
/// Referrer Policy" base): a policy delivered via the Referrer-Policy
/// response header wins outright; otherwise the FIRST `<meta name=referrer>`
/// in tree order whose content yields a valid policy; "" = spec default.
pub(crate) fn resolve_referrer_policy_from(gs: &JsState, dom: &DomTree) -> String {
    if !gs.referrer_policy_header.is_empty() {
        return gs.referrer_policy_header.clone();
    }
    for nid in dom.query_selector_all("meta").unwrap_or_default() {
        let Some(n) = dom.get_node(nid) else { continue };
        let is_referrer_meta = n
            .get_attribute("name")
            .map(|v| v.eq_ignore_ascii_case("referrer"))
            .unwrap_or(false);
        if !is_referrer_meta {
            continue;
        }
        let Some(content) = n.get_attribute("content").map(|v| v.to_string()) else {
            continue;
        };
        if let Some(p) = last_valid_referrer_token(&content) {
            return p;
        }
    }
    String::new()
}

/// Op-time wrapper: same resolution, from OpState's shared document state.
pub(super) fn resolve_document_referrer_policy(state: &OpState) -> String {
    let shared = state.borrow::<SharedState>().clone();
    let gs = shared.borrow();
    match gs.dom.as_ref() {
        Some(dom) => resolve_referrer_policy_from(&gs, dom),
        None => gs.referrer_policy_header.clone(),
    }
}

/// Referer header for scripted fetch() requests per the Fetch standard
/// (obscura#875). `referrer_init` is the resolved RequestInit referrer: ""
/// sends none, "about:client" uses the document URL, anything else parses as
/// a URL (non-HTTP(S) or unparseable → no referrer, per spec). `policy` is
/// the validated referrerPolicy token; "" means the default
/// strict-origin-when-cross-origin. Same/cross-origin and the https→http
/// downgrade are judged against each hop's target so redirects re-strip.
/// Origin-only results keep the trailing "/" that our navigation_referrer
/// established (Chrome serializes without it; servers parse the header as a
/// URL either way).
pub(crate) fn fetch_referer(policy: &str, referrer_init: &str, document: &Url, target: &Url) -> String {
    let referrer = if referrer_init.is_empty() {
        return String::new();
    } else if referrer_init == "about:client" {
        document.clone()
    } else {
        match Url::parse(referrer_init) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => u,
            _ => return String::new(),
        }
    };
    let full = |u: &Url| -> String {
        let mut u = u.clone();
        u.set_fragment(None);
        let _ = u.set_username("");
        let _ = u.set_password(None);
        u.to_string()
    };
    let origin = format!("{}/", referrer.origin().ascii_serialization());
    let same_origin = referrer.origin() == target.origin();
    let downgrade = referrer.scheme() == "https" && target.scheme() != "https";
    let policy = if policy.is_empty() {
        "strict-origin-when-cross-origin"
    } else {
        policy
    };
    match policy {
        "no-referrer" => String::new(),
        "unsafe-url" => full(&referrer),
        "origin" => origin,
        "strict-origin" => {
            if downgrade {
                String::new()
            } else {
                origin
            }
        }
        "no-referrer-when-downgrade" => {
            if downgrade {
                String::new()
            } else {
                full(&referrer)
            }
        }
        "origin-when-cross-origin" | "strict-origin-when-cross-origin" => {
            if same_origin {
                full(&referrer)
            } else if downgrade && policy == "strict-origin-when-cross-origin" {
                String::new()
            } else {
                origin
            }
        }
        // JS validates the token; an unreachable value must not leak a URL.
        _ => String::new(),
    }
}
