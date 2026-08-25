//! Helpers shared across CDP domain handlers (claimed from obscura-cdp).
//!
//! The file-scheme detector is reused by every CDP entrypoint that can
//! trigger a navigation, so no domain can silently skip the
//! `allow_file_access` gate another one enforces.

/// Returns true when `raw` parses as a `file:`-scheme URL, or syntactically
/// starts with `file:` after a possible leading-whitespace strip. Matching is
/// case-insensitive on the scheme so neither `FILE://` nor `File://` slips
/// past callers that gate on `file://`.
pub(crate) fn url_is_file_scheme(raw: &str) -> bool {
    url::Url::parse(raw)
        .map(|u| u.scheme().eq_ignore_ascii_case("file"))
        .unwrap_or_else(|_| raw.trim_start().to_ascii_lowercase().starts_with("file:"))
}

/// Truncate `s` to at most `max` bytes, never splitting a UTF-8 character.
///
/// `&s[..max]` panics when `max` lands inside a multi-byte character, and the
/// strings we truncate for log previews are attacker-controlled (raw WebSocket
/// frames, intercepted URLs). A single frame whose byte `max` straddles a
/// multi-byte char would otherwise panic the CDP processor task.
pub(crate) fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_plain_file_url() {
        assert!(url_is_file_scheme("file:///etc/passwd"));
    }

    #[test]
    fn matches_case_insensitively() {
        assert!(url_is_file_scheme("FILE:///etc/passwd"));
        assert!(url_is_file_scheme("File://x"));
        assert!(url_is_file_scheme("fIlE://y"));
    }

    #[test]
    fn matches_with_leading_whitespace_fallback() {
        // url::Url::parse rejects leading whitespace, but the syntactic
        // fallback still catches `   file:...`.
        assert!(url_is_file_scheme("   file:///etc/passwd"));
    }

    #[test]
    fn rejects_http_https_about_data() {
        assert!(!url_is_file_scheme("http://example.com"));
        assert!(!url_is_file_scheme("https://example.com"));
        assert!(!url_is_file_scheme("about:blank"));
        assert!(!url_is_file_scheme("data:text/plain,hi"));
        assert!(!url_is_file_scheme(""));
    }

    #[test]
    fn rejects_lookalikes_that_are_not_file_scheme() {
        // The URL parser rejects these (no `://`), so the syntactic fallback
        // kicks in. `file` appearing anywhere except as the leading scheme
        // must not match.
        assert!(!url_is_file_scheme("notfile:///x"));
        assert!(!url_is_file_scheme("http://file/"));
    }

    #[test]
    fn truncate_never_splits_a_multibyte_char() {
        let s = format!("{}€tail", "a".repeat(199));
        assert!(!s.is_char_boundary(200), "setup: byte 200 splits the € char");

        let naive = std::panic::catch_unwind(|| {
            let _ = &s[..s.len().min(200)];
        });
        assert!(naive.is_err(), "raw byte slice at a non-char-boundary must panic");

        let safe = truncate_on_char_boundary(&s, 200);
        assert!(s.starts_with(safe));
        assert_eq!(safe.len(), 199, "should stop right before the € char");
    }

    #[test]
    fn truncate_returns_whole_string_when_short() {
        assert_eq!(truncate_on_char_boundary("hi", 200), "hi");
        assert_eq!(truncate_on_char_boundary("", 10), "");
        assert_eq!(truncate_on_char_boundary("abc", 3), "abc");
    }
}
