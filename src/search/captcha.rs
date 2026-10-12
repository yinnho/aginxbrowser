//! CAPTCHA suspend policy + engine disable knobs
//! (aginxos REQ: search-captcha-challenge, 2026-10-13).

use std::time::Duration;

/// How long to bench an engine after a CAPTCHA.
///
/// - `suspend` (default): progressive backoff ladder, overridable via
///   `AGINXBROWSER_CAPTCHA_BACKOFF_SECS` (comma list; single value = flat).
/// - `demote`: skip the engine for the rest of the process (≈1y bench) so a
///   research loop does not wait out fixed 300s/3600s walls; success still
///   clears the count when the engine is tried again after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptchaPolicy {
    Suspend,
    Demote,
}

pub(crate) fn captcha_policy() -> CaptchaPolicy {
    match std::env::var("AGINXBROWSER_CAPTCHA_POLICY")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "demote" | "skip" | "session" => CaptchaPolicy::Demote,
        _ => CaptchaPolicy::Suspend,
    }
}

/// Default ladder: 5m → 10m → 30m → 1h (matches historical behaviour).
fn default_captcha_ladder() -> [u64; 4] {
    [300, 600, 1800, 3600]
}

pub(crate) fn captcha_ladder() -> [u64; 4] {
    let Ok(raw) = std::env::var("AGINXBROWSER_CAPTCHA_BACKOFF_SECS") else {
        return default_captcha_ladder();
    };
    let parts: Vec<u64> = raw
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .filter(|&n| n > 0)
        .collect();
    match parts.as_slice() {
        [] => default_captcha_ladder(),
        [one] => [*one, *one, *one, *one],
        [a, b] => [*a, *b, *b, *b],
        [a, b, c] => [*a, *b, *c, *c],
        [a, b, c, d, ..] => [*a, *b, *c, *d],
    }
}

pub(crate) fn captcha_backoff(count: u32) -> Duration {
    let ladder = captcha_ladder();
    let secs = match count {
        0 | 1 => ladder[0],
        2 => ladder[1],
        3 => ladder[2],
        _ => ladder[3],
    };
    Duration::from_secs(secs)
}

pub(crate) fn captcha_suspend_duration(count: u32, base_suspend: Duration) -> Duration {
    match captcha_policy() {
        // Session demote: long enough that research loops treat the engine as
        // gone until restart; ignores per-engine base floors (sogou_wechat's
        // 3600s) so operators who asked for demote are not still stuck.
        CaptchaPolicy::Demote => Duration::from_secs(86_400 * 365),
        CaptchaPolicy::Suspend => captcha_backoff(count).max(base_suspend),
    }
}

/// Engines listed in `AGINXBROWSER_DISABLE_ENGINES` (comma-separated names)
/// are not registered. Typical CN research deploy: `bing_news` (infinitescroll
/// challenge on both paths) until a parseable Bing web leg lands.
pub(crate) fn disabled_engines() -> std::collections::HashSet<String> {
    std::env::var("AGINXBROWSER_DISABLE_ENGINES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `AGINXBROWSER_BING_NEWS=0|false|off` also disables the bing_news leg
/// (alias for putting it in DISABLE_ENGINES). Default stays on; CN ops that
/// hit Object-moved spam set this or DISABLE_ENGINES.
pub(crate) fn bing_news_enabled() -> bool {
    match std::env::var("AGINXBROWSER_BING_NEWS")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "0" | "false" | "off" | "no" | "disabled" => false,
        _ => true,
    }
}
