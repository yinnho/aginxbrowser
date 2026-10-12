//! Unit tests for the search facade (normalize/merge/CAPTCHA/dual-path).

use std::time::Duration;

use super::captcha::{captcha_backoff, captcha_suspend_duration};
use super::dual_path::{
    direct_skip_remaining, note_direct_outcome, DirectFailState, DIRECT_FAILS_CAP,
    DIRECT_FAIL_THRESHOLD, DIRECT_SKIP_SECS,
};
use super::validate::looks_like_bing_news_url;
use super::*;

// ---- normalize_url ----

#[test]
fn normalize_strips_www_and_lowercases() {
    assert_eq!(normalize_url("https://WWW.Example.com/path"), "https://example.com/path");
}

#[test]
fn normalize_preserves_port() {
    assert_eq!(normalize_url("https://example.com:8080/p"), "https://example.com:8080/p");
}

#[test]
fn normalize_strips_trailing_slash() {
    // Path with trailing slash is stripped.
    assert_eq!(normalize_url("https://example.com/path/"), "https://example.com/path");
    // Bare root: the `url` crate normalizes example.com back to "/" so the
    // trailing slash is preserved (cannot represent a truly empty path).
    assert_eq!(normalize_url("https://example.com/"), "https://example.com/");
}

#[test]
fn normalize_strips_tracking_params() {
    let n = normalize_url("https://example.com/a?utm_source=x&q=hello&utm_medium=y");
    assert!(n.contains("q=hello"));
    assert!(!n.contains("utm_source"));
    assert!(!n.contains("utm_medium"));
}

#[test]
fn normalize_dedup_equivalent_urls() {
    let a = normalize_url("https://www.example.com/article/?utm_source=feed");
    let b = normalize_url("https://EXAMPLE.COM/article");
    assert_eq!(a, b);
}

#[test]
fn normalize_invalid_url_returns_lowered_input() {
    assert_eq!(normalize_url("NOT A URL"), "not a url");
}

// ---- merge_results ----

fn raw(title: &str, url: &str, engine: &str, score: f64) -> RawSearchResult {
    RawSearchResult {
        title: title.into(),
        url: url.into(),
        snippet: "s".into(),
        engine: engine.into(),
        score,
        cookies: vec![],
        js_extract_result: None,
        image: None,
    }
}

#[test]
fn merge_dedups_by_normalized_url() {
    let results = vec![
        raw("Foo", "https://example.com/a", "bing", 5.0),
        raw("Foo Bar", "https://www.example.com/a/", "baidu", 3.0),
    ];
    let merged = merge_results(results, 10);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].engines, vec!["bing", "baidu"]);
    assert_eq!(merged[0].score, 8.0);
    // Picks the longest title.
    assert_eq!(merged[0].title, "Foo Bar");
}

#[test]
fn merge_sorts_by_score_desc() {
    let results = vec![
        raw("Low", "https://example.com/low", "bing", 1.0),
        raw("High", "https://example.com/high", "baidu", 9.0),
    ];
    let merged = merge_results(results, 10);
    assert_eq!(merged[0].title, "High");
    assert_eq!(merged[1].title, "Low");
}

// ---- health_snapshot (suspension state /doctor reports) ----

#[tokio::test]
async fn health_snapshot_lists_every_registered_engine_healthy_by_default() {
    let registry = SearchEngineRegistry::new();
    let rows = registry.health_snapshot().await;
    assert!(rows.len() >= 14, "expected all engines, got {}", rows.len());
    assert!(rows.iter().all(|r| !r.suspended && r.captcha_count == 0));
    assert!(rows.iter().any(|r| r.name == "bing_images"));
}

#[test]
fn merge_truncates_to_max_results() {
    let results: Vec<RawSearchResult> = (0..5)
        .map(|i| raw(&format!("t{i}"), &format!("https://e.com/{i}"), "bing", 5.0 - i as f64))
        .collect();
    let merged = merge_results(results, 3);
    assert_eq!(merged.len(), 3);
}

#[test]
fn merge_empty_input() {
    assert!(merge_results(vec![], 10).is_empty());
}

#[test]
fn merge_carries_cookies() {
    let mut r = raw("T", "https://example.com/a", "sogou", 5.0);
    r.cookies = vec!["session=abc".into()];
    let merged = merge_results(vec![r], 10);
    assert_eq!(merged[0].cookies, vec!["session=abc"]);
}

// ---- direct-probe failure learning (aginxos REQ #414) ----

fn fresh_fail_map() -> std::collections::HashMap<String, DirectFailState> {
    std::collections::HashMap::new()
}

#[test]
fn two_consecutive_failures_sentence_the_host() {
    let mut map = fresh_fail_map();
    note_direct_outcome(&mut map, "wall.example", false, 1_000);
    assert_eq!(direct_skip_remaining(&map, "wall.example", 1_001), 0, "first fail is still just a streak");
    note_direct_outcome(&mut map, "wall.example", false, 1_100);
    let left = direct_skip_remaining(&map, "wall.example", 1_101);
    assert!(
        left > DIRECT_SKIP_SECS - 10 && left <= DIRECT_SKIP_SECS,
        "sentenced for ~{DIRECT_SKIP_SECS}s, got {left}"
    );
    // Other hosts are unaffected — the wall is per-host.
    assert_eq!(direct_skip_remaining(&map, "open.example", 1_101), 0);
}

#[test]
fn direct_success_lifts_the_sentence() {
    let mut map = fresh_fail_map();
    note_direct_outcome(&mut map, "wall.example", false, 1_000);
    note_direct_outcome(&mut map, "wall.example", false, 1_000);
    assert!(direct_skip_remaining(&map, "wall.example", 1_000) > 0);
    note_direct_outcome(&mut map, "wall.example", true, 1_050);
    assert_eq!(direct_skip_remaining(&map, "wall.example", 1_051), 0);
}

#[test]
fn sentence_expires() {
    let mut map = fresh_fail_map();
    note_direct_outcome(&mut map, "wall.example", false, 1_000);
    note_direct_outcome(&mut map, "wall.example", false, 1_000);
    let until = 1_000 + DIRECT_SKIP_SECS;
    assert!(direct_skip_remaining(&map, "wall.example", until - 1) > 0);
    assert_eq!(direct_skip_remaining(&map, "wall.example", until), 0, "expired");
}

#[test]
fn geo_substituted_bodies_count_as_failures() {
    // Body-validation failures feed the same streak as transport errors
    // (get_direct_first_if calls note_direct_outcome on both unusable
    // outcomes); the pure halves can't see the reason, only that the
    // fetch path told them "unusable" — pin that wiring expectation
    // against the threshold constant.
    let mut map = fresh_fail_map();
    for _ in 0..DIRECT_FAIL_THRESHOLD {
        note_direct_outcome(&mut map, "cn-redirect.example", false, 5_000);
    }
    assert!(direct_skip_remaining(&map, "cn-redirect.example", 5_001) > 0);
}

#[test]
fn fail_map_is_capped() {
    let mut map = fresh_fail_map();
    for i in 0..(DIRECT_FAILS_CAP + 8) as u64 {
        let host = format!("h{i}.example");
        note_direct_outcome(&mut map, &host, false, 10_000);
        note_direct_outcome(&mut map, &host, false, 10_000); // sentenced
    }
    assert!(
        map.len() <= DIRECT_FAILS_CAP,
        "cap {} exceeded: {}",
        DIRECT_FAILS_CAP,
        map.len()
    );
}

// ---- socks5 → socks5h normalization (proxy-retry leg) ----

#[test]
fn socks5_rewrites_to_socks5h_with_colon_intact() {
    // The old off-by-one produced "socks5h//h:1080" — empty hostname,
    // silent DNS failure in the connect chain.
    assert_eq!(
        normalize_socks5h("socks5://h.example:1080"),
        "socks5h://h.example:1080"
    );
}

#[test]
fn non_socks5_schemes_pass_through() {
    assert_eq!(normalize_socks5h("socks5h://h:1080"), "socks5h://h:1080");
    assert_eq!(normalize_socks5h("http://h:8080"), "http://h:8080");
    assert_eq!(normalize_socks5h("https://h:8443"), "https://h:8443");
}

// ---- CJK query heuristic ----

#[test]
fn cjk_heavy_detects_chinese_medical_query() {
    assert!(is_cjk_heavy("杜仲、续断、桑寄生治疗腰痛"));
    assert!(is_cjk_heavy("中医药 骨关节炎 RCT"));
    assert!(!is_cjk_heavy("rust async tokio tutorial"));
    assert!(!is_cjk_heavy(""));
    // Mixed but English-dominant stays non-CJK.
    assert!(!is_cjk_heavy("how to use 杜仲 in rust"));
}

#[test]
fn overseas_hosts_auto_proxy_for_search() {
    // Mirrors env_knobs::should_auto_proxy — search relies on this list
    // for proxy-first overseas engines.
    assert!(crate::config::should_auto_proxy("https://html.duckduckgo.com/html/"));
    assert!(crate::config::should_auto_proxy("https://huggingface.co/api/models"));
    assert!(crate::config::should_auto_proxy("https://api.github.com/search/repositories"));
    assert!(crate::config::should_auto_proxy("https://export.arxiv.org/api/query"));
    assert!(crate::config::should_auto_proxy("https://pypi.org/pypi/requests/json"));
    assert!(!crate::config::should_auto_proxy("https://www.baidu.com/s"));
    assert!(!crate::config::should_auto_proxy("https://www.sogou.com/web"));
}

#[test]
fn cjk_noise_engines_named() {
    assert!(is_cjk_noise_engine("huggingface"));
    assert!(is_cjk_noise_engine("github"));
    assert!(!is_cjk_noise_engine("baidu"));
    assert!(!is_cjk_noise_engine("sogou"));
    assert!(!is_cjk_noise_engine("duckduckgo"));
}

// ---- body reject classification / captcha policy (REQ captcha-challenge) ----

#[test]
fn classify_object_moved_as_geo_substitute() {
    let body = r#"<html><head><title>Object moved</title></head>
<body><h2>Object moved to <a href="https://cn.bing.com/">here</a>.</h2></body></html>"#;
    assert_eq!(classify_rejected_body(body), BodyRejectClass::GeoSubstitute);
}

#[test]
fn classify_captcha_markers() {
    assert_eq!(
        classify_rejected_body("<div class='captcha-box'>验证码</div>"),
        BodyRejectClass::Captcha
    );
    assert_eq!(
        classify_rejected_body("cf-challenge-running"),
        BodyRejectClass::Captcha
    );
}

#[test]
fn classify_portal_attention_required() {
    assert_eq!(
        classify_rejected_body("<title>Attention Required! | Cloudflare</title>"),
        BodyRejectClass::Portal
    );
}

#[test]
fn captcha_ladder_defaults() {
    // No env override in unit tests → historical ladder.
    assert_eq!(captcha_backoff(1), Duration::from_secs(300));
    assert_eq!(captcha_backoff(2), Duration::from_secs(600));
    assert_eq!(captcha_backoff(3), Duration::from_secs(1800));
    assert_eq!(captcha_backoff(4), Duration::from_secs(3600));
}

#[test]
fn captcha_demote_ignores_base_floor() {
    // Demote path is env-gated; without the env we only assert Suspend
    // still respects the engine base floor (sogou_wechat 3600).
    let d = captcha_suspend_duration(1, Duration::from_secs(3600));
    assert_eq!(d, Duration::from_secs(3600));
}

#[test]
fn looks_like_bing_news_url_detects_infinitescroll() {
    assert!(looks_like_bing_news_url(
        "https://cn.bing.com/news/infinitescrollajax?q=x"
    ));
    assert!(!looks_like_bing_news_url("https://www.baidu.com/s?wd=x"));
}

#[test]
fn diversify_limits_per_host() {
    let results: Vec<RawSearchResult> = (0..6)
        .map(|i| {
            raw(
                &format!("t{i}"),
                &format!("https://arxiv.org/abs/{i}"),
                "arxiv",
                10.0 - i as f64,
            )
        })
        .collect();
    // Plus one other host that should still appear early, before
    // deferred arxiv padding fills the rest of the page.
    let mut results = results;
    results.push(raw("other", "https://example.com/a", "bing", 1.0));
    let merged = merge_results(results, 5);
    let example_pos = merged
        .iter()
        .position(|r| r.url.contains("example.com"))
        .expect("other host squeezed out");
    assert!(
        example_pos <= MAX_PER_HOST,
        "other host should appear among the early diverse slots, got pos {example_pos} in {:?}",
        merged.iter().map(|r| &r.url).collect::<Vec<_>>()
    );
}
