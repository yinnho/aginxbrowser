//! Process-global search observability counters
//! (aginxos REQ: search-captcha-challenge, 2026-10-13).

use std::sync::atomic::{AtomicU64, Ordering};

use super::captcha::{captcha_ladder, captcha_policy, CaptchaPolicy};

/// Process-global counters for CAPTCHA-suspend share and bing_news dual-path
/// fail rate (REQ observability). Exposed on `/engines` and `/doctor`.
#[derive(Default)]
pub(crate) struct SearchObs {
    /// Engines actually dispatched (not skipped for suspension up-front).
    pub(crate) engine_dispatch_total: AtomicU64,
    pub(crate) captcha_suspend_total: AtomicU64,
    pub(crate) bing_news_attempt_total: AtomicU64,
    pub(crate) bing_news_dual_path_fail_total: AtomicU64,
}

pub(crate) static SEARCH_OBS: SearchObs = SearchObs {
    engine_dispatch_total: AtomicU64::new(0),
    captcha_suspend_total: AtomicU64::new(0),
    bing_news_attempt_total: AtomicU64::new(0),
    bing_news_dual_path_fail_total: AtomicU64::new(0),
};

pub(crate) fn obs_inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Snapshot for `/engines` / `/doctor`. Rates are 0 when the denominator is 0.
pub fn search_obs_snapshot() -> serde_json::Value {
    let dispatch = SEARCH_OBS.engine_dispatch_total.load(Ordering::Relaxed);
    let captcha = SEARCH_OBS.captcha_suspend_total.load(Ordering::Relaxed);
    let bn_attempts = SEARCH_OBS.bing_news_attempt_total.load(Ordering::Relaxed);
    let bn_dual = SEARCH_OBS.bing_news_dual_path_fail_total.load(Ordering::Relaxed);
    let captcha_share = if dispatch == 0 {
        0.0
    } else {
        captcha as f64 / dispatch as f64
    };
    let bn_dual_rate = if bn_attempts == 0 {
        0.0
    } else {
        bn_dual as f64 / bn_attempts as f64
    };
    serde_json::json!({
        "engine_dispatch_total": dispatch,
        "captcha_suspend_total": captcha,
        "captcha_suspend_share": captcha_share,
        "bing_news_attempt_total": bn_attempts,
        "bing_news_dual_path_fail_total": bn_dual,
        "bing_news_dual_path_fail_rate": bn_dual_rate,
        "captcha_policy": match captcha_policy() {
            CaptchaPolicy::Suspend => "suspend",
            CaptchaPolicy::Demote => "demote",
        },
        "captcha_backoff_secs": captcha_ladder(),
    })
}
