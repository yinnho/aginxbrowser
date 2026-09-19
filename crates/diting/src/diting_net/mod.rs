#![allow(dead_code)]
pub mod client;
pub mod cookies;
pub mod encoding;
pub mod blocklist;
#[cfg(feature = "stealth")]
pub mod wreq_client;

pub use client::{
    env_allows_private_network, HttpClient, NetError,
    CallbackRegistry, RequestCallback, RequestInfo, Response, ResponseCallback, ResourceType,
};
pub use cookies::CookieJar;
pub use encoding::{
    decode_devtools_body, decode_non_html, decode_response_with_name, decode_with_label,
    label_name, url_encode_query,
};

/// Fold a response's headers into the JS/CDP-visible map WITHOUT the naive
/// `HashMap::from_iter` collapse, which keeps only the LAST value of a
/// repeated header (obscura #913 — Link/WWW-Authenticate/Cache-Control
/// etc. silently lose every value but one). Combination rules follow the
/// fetch/XHR header-combine convention (", "), with `set-cookie` exempt —
/// RFC 7230 §3.2.2 forbids comma-merging it, and Chrome's DevTools joins
/// it with "\n" instead. CDP consumers see the same string; their "\n for
/// everything" convention is a known, documented divergence (the map type
/// carries one value per name, so both faces share it).
pub fn collect_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers.iter() {
        // HeaderName is normalized to lowercase by the http crate, so the
        // key is already the case-insensitive form every consumer expects.
        grouped
            .entry(name.as_str().to_string())
            .or_default()
            .push(value.to_str().unwrap_or("").to_string());
    }
    grouped
        .into_iter()
        .map(|(name, values)| {
            let sep = if name == "set-cookie" { "\n" } else { ", " };
            (name, values.join(sep))
        })
        .collect()
}
#[cfg(feature = "stealth")]
pub use wreq_client::{
    DEFAULT_TLS_FINGERPRINT, StealthHttpClient, STEALTH_USER_AGENT, emulation_os_for_ua,
    parse_tls_fingerprint, warn_on_ua_tls_mismatch,
};

/// DevTools `Fetch.enable` urlPattern glob: `*` matches zero or more
/// characters anywhere in the pattern (Chrome also supports `?`, which real
/// clients rarely use for routing). Multi-star patterns are matched as
/// prefix / ordered infixes / suffix containment, so Playwright's `**/*`
/// style globs work.
///
/// Shared by the CDP Fetch domain and the engine's hard-block gate
/// (`Network.setBlockedURLs` — matched resources fail outright, Chrome
/// semantics). Lives here, not in the CDP face, because the block decision
/// is a network-layer concern the engine owns regardless of whether any
/// CDP client is attached.
pub fn url_pattern_matches(pattern: &str, url: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    if !url.starts_with(parts[0]) {
        return false;
    }
    let mut rest = &url[parts[0].len()..];
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if i == last {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    true
}

/// Serializes tests that read or mutate `AGINXBROWSER_ALLOW_PRIVATE_NETWORK`,
/// since process env is shared across parallel test threads.
#[cfg(test)]
pub(crate) static PRIVATE_NET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod collect_headers_tests {
    use super::collect_response_headers;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        h
    }

    #[test]
    fn repeated_headers_combine_instead_of_collapsing() {
        // obscura #913: HashMap::from_iter kept only the LAST link value.
        let h = map(&[
            ("link", "</a.css>; rel=preload"),
            ("link", "</b.js>; rel=preload"),
            ("www-authenticate", "Basic realm=x"),
            ("www-authenticate", "Bearer realm=y"),
            ("content-type", "text/html"),
        ]);
        let collected = collect_response_headers(&h);
        assert_eq!(
            collected.get("link").map(String::as_str),
            Some("</a.css>; rel=preload, </b.js>; rel=preload"),
            "fetch/XHR combine rule is \", \""
        );
        assert_eq!(
            collected.get("www-authenticate").map(String::as_str),
            Some("Basic realm=x, Bearer realm=y")
        );
        assert_eq!(collected.get("content-type").map(String::as_str), Some("text/html"));
    }

    #[test]
    fn set_cookie_joins_with_newline_not_comma() {
        // RFC 7230 §3.2.2: set-cookie must never be comma-merged (values
        // contain embedded commas); Chrome's DevTools join is "\n".
        let h = map(&[
            ("set-cookie", "a=1; Path=/"),
            ("set-cookie", "b=2; Expires=Thu, 01 Jan 2030 00:00:00 GMT"),
        ]);
        let collected = collect_response_headers(&h);
        assert_eq!(
            collected.get("set-cookie").map(String::as_str),
            Some("a=1; Path=/\nb=2; Expires=Thu, 01 Jan 2030 00:00:00 GMT"),
            "both values survive, newline-separated"
        );
    }

    #[test]
    fn keys_come_out_lowercase() {
        let h = map(&[("X-Custom-Header", "v")]);
        // HeaderName normalizes on construction; the collector must not
        // regress the lowercase contract consumers' `.get()` calls rely on.
        let collected = collect_response_headers(&h);
        assert_eq!(collected.get("x-custom-header").map(String::as_str), Some("v"));
    }
}

#[cfg(test)]
mod url_pattern_tests {
    use super::url_pattern_matches;

    #[test]
    fn url_patterns_cover_client_shapes() {
        assert!(url_pattern_matches("*", "https://x.test/a.png"));
        assert!(url_pattern_matches("**/*", "https://x.test/a.png"));
        assert!(url_pattern_matches("**/*.png", "https://x.test/a/b.png"));
        assert!(!url_pattern_matches("**/*.png", "https://x.test/a/b.jpg"));
        assert!(url_pattern_matches("https://x.test/*", "https://x.test/api"));
        assert!(!url_pattern_matches("https://x.test/*", "https://y.test/api"));
        assert!(url_pattern_matches(
            "https://x.test/api/v1",
            "https://x.test/api/v1"
        ));
        assert!(!url_pattern_matches(
            "https://x.test/api/v1",
            "https://x.test/api/v2"
        ));
    }
}
