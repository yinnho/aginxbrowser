//! Body-reject classification + dual-path challenge host memory
//! (aginxos REQ: search-captcha-challenge, 2026-10-13).

/// Why a direct/proxy body failed `body_ok` — surfaced in logs and
/// `engine_errors` so callers don't see one generic "body failed validation".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRejectClass {
    GeoSubstitute,
    Captcha,
    Portal,
    Other,
}

impl BodyRejectClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GeoSubstitute => "geo-substitute",
            Self::Captcha => "captcha",
            Self::Portal => "portal",
            Self::Other => "other",
        }
    }
}

/// Classify a rejected search body. Order matters: Object-moved geo pages
/// also look like portals; CAPTCHA markers win when present.
pub fn classify_rejected_body(body: &str) -> BodyRejectClass {
    let lower = body.to_ascii_lowercase();
    if lower.contains("captcha")
        || lower.contains("recaptcha")
        || lower.contains("hcaptcha")
        || lower.contains("cf-challenge")
        || lower.contains("challenge-platform")
        || lower.contains("wappass")
        || lower.contains("antispider")
        || lower.contains("验证码")
    {
        return BodyRejectClass::Captcha;
    }
    if lower.contains("object moved")
        || (lower.contains("document.location") && lower.contains("cn.bing.com"))
        || lower.contains("the document has moved")
    {
        return BodyRejectClass::GeoSubstitute;
    }
    if lower.contains("access denied")
        || lower.contains("attention required")
        || lower.contains("just a moment")
        || lower.contains("enable javascript and cookies")
        || lower.contains("checking your browser")
        || lower.contains("security check")
        || (lower.contains("<title>") && lower.contains("bing") && !lower.contains("newsitem"))
    {
        return BodyRejectClass::Portal;
    }
    BodyRejectClass::Other
}

/// Hosts that recently failed *both* direct and proxy with a challenge-class
/// body. Next fetch for that host fails fast (no dual-path spam).
pub(crate) const CHALLENGE_SKIP_SECS: u64 = 600;
/// Proxy budget when retrying a challenge/geo reject (REQ: ≤2s).
pub(crate) const CHALLENGE_PROXY_TIMEOUT_SECS: u64 = 2;

static CHALLENGE_SKIPS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::OnceLock::new();

fn challenge_skips()
-> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    CHALLENGE_SKIPS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn challenge_skip_remaining(host: &str, now: u64) -> u64 {
    let map = challenge_skips().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(host) {
        Some(&until) if until > now => until - now,
        _ => 0,
    }
}

pub(crate) fn note_dual_path_challenge(host: &str) {
    let now = crate::now_secs();
    let mut map = challenge_skips().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(host.to_string(), now.saturating_add(CHALLENGE_SKIP_SECS));
    // Cap map size the same way direct_fails does.
    if map.len() > 64 {
        map.retain(|_, until| *until > now);
    }
}

pub(crate) fn is_challenge_class(class: BodyRejectClass) -> bool {
    matches!(
        class,
        BodyRejectClass::GeoSubstitute | BodyRejectClass::Captcha | BodyRejectClass::Portal
    )
}

pub(crate) fn looks_like_bing_news_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("infinitescrollajax") || lower.contains("/news/")
}

/// Log the first chars of a rejected body with a classified reason so
/// operators can tell geo-substitute / captcha / portal apart (REQ).
pub(crate) fn log_rejected_body(url: &str, path: &str, body: &str, class: BodyRejectClass) {
    let head: String = body.chars().take(200).collect();
    tracing::warn!(
        "search: {path} body failed validation ({}) for {url} (len={}): {head:?}",
        class.as_str(),
        body.len()
    );
}
