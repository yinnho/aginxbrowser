//! The colocated cookie-jar contract suite (split out of cookies.rs
//! riding the god-file ratchet cap — same shape as diting_layout's
//! text.rs + text/tests.rs).
use super::*;

#[test]
fn test_set_and_get_cookie() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/path").unwrap();
    jar.set_cookie("session=abc123; Path=/; Secure; HttpOnly", &url);

    let header = jar.get_cookie_header(&url);
    assert!(header.contains("session=abc123"));
}

/// #102: the metadata face carries every attribute an operator reasons
/// about — scoping, flags, expiry, host-anchoring — and never a value.
/// `value` must not merely be empty: the serialized face has no such
/// key at all, so no downstream surface can grow one by accident.
#[test]
fn metadata_face_carries_attributes_but_never_values() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/login").unwrap();
    jar.set_cookie(
        "sess=TOPSECRET-VALUE; Domain=example.com; Path=/; Secure; \
         HttpOnly; SameSite=None; Max-Age=3600",
        &url,
    );
    // Host-anchored (no Domain attribute): the host_only=true shape.
    jar.set_cookie("tracker=hunter2", &url);
    // Expired on arrival: the walk must skip it like get_all_cookies.
    jar.set_cookie("dead=zzz; Max-Age=0", &url);

    let meta = jar.get_all_cookie_metadata();
    assert_eq!(meta.len(), 2, "expired-on-arrival must be filtered");

    let sess = meta.iter().find(|m| m.name == "sess").unwrap();
    assert_eq!(sess.domain, "example.com");
    assert_eq!(sess.path, "/");
    assert!(sess.secure);
    assert!(sess.http_only);
    assert_eq!(sess.same_site, "None");
    assert!(sess.expires.is_some(), "Max-Age must land as expiry");
    assert!(!sess.host_only, "Domain= cookies are not host-anchored");

    let tracker = meta.iter().find(|m| m.name == "tracker").unwrap();
    assert!(tracker.host_only, "no Domain attr = host-anchored");

    let face = serde_json::to_string(&meta).unwrap();
    assert!(
        !face.contains("value"),
        "the metadata face must not even carry a value key: {face}"
    );
    assert!(!face.contains("TOPSECRET"), "{face}");
    assert!(!face.contains("hunter2"), "{face}");
}

// obscura #855 perms half: the store file and its directory must not
// inherit the process umask — 0600 file, 0700 dir, set explicitly.

#[cfg(unix)]
#[test]
fn save_to_file_sets_explicit_permissions() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("store");
    let path = dir.join("cookie-store.json");

    // A hostile umask must not leak into the store.
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("session=abc; Path=/", &url);
    jar.save_to_file(&path).unwrap();

    use std::os::unix::fs::PermissionsExt;
    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "store file must be owner-only");
    assert_eq!(dir_mode, 0o700, "store dir must be owner-only");

    // Overwriting an existing (loose) file still lands 0600 — the rename
    // must not preserve the old inode's permissions.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    jar.save_to_file(&path).unwrap();
    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "re-save must re-tighten a loose file");
}

// obscura #855 item 1 error-surfacing half: an unwritable store location
// must return Err, never silently succeed. A file where the parent
// directory should be is a deterministic failure that also works when
// tests run as root (where a chmod-0 directory would still be writable).

#[test]
fn save_to_file_errors_when_parent_is_not_a_directory() {
    let base = tempfile::tempdir().unwrap();
    let blocker = base.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();

    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("session=abc; Path=/", &url);

    assert!(
        jar.save_to_file(&blocker.join("cookie-store.json")).is_err(),
        "unwritable store path must surface an error (obscura#855)"
    );
}

// obscura #915: RFC 6265 §5.3 — a non-HTTP (document.cookie) write must
// not overwrite or evict a cookie whose http_only flag is set.

#[test]
fn document_cookie_cannot_overwrite_httponly_cookie() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/app/page").unwrap();
    jar.set_cookie("session=server; Path=/app; HttpOnly", &url);

    jar.set_cookie_from_js("session=pwned; Path=/app", &url);

    let header = jar.get_cookie_header(&url);
    assert!(
        header.contains("session=server"),
        "HttpOnly cookie survives the document.cookie overwrite: {header}"
    );
    assert!(
        !header.contains("pwned"),
        "the JS payload value must not land: {header}"
    );
    // Still invisible to document.cookie reads.
    let js_view = jar.get_js_visible_cookies(&url);
    assert!(
        !js_view.contains("session="),
        "HttpOnly stays hidden from JS reads: {js_view}"
    );
}

#[test]
fn document_cookie_cannot_delete_httponly_cookie_via_expiry() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("session=server; HttpOnly", &url);

    jar.set_cookie_from_js("session=; Path=/; Max-Age=0", &url);
    jar.set_cookie_from_js("session=; Expires=Thu, 01 Jan 1970 00:00:00 GMT", &url);

    let header = jar.get_cookie_header(&url);
    assert!(
        header.contains("session=server"),
        "the expiry-delete form must not evict an HttpOnly cookie either: {header}"
    );
}

#[test]
fn document_cookie_still_overwrites_non_httponly_cookie() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("pref=old", &url);

    jar.set_cookie_from_js("pref=new", &url);

    let header = jar.get_cookie_header(&url);
    assert!(header.contains("pref=new") && !header.contains("old"),
        "non-HttpOnly cookies keep their normal overwrite semantics: {header}");
}

#[test]
fn test_cookie_domain_matching() {
    let jar = CookieJar::new();
    let url = Url::parse("https://www.example.com/").unwrap();
    jar.set_cookie("token=xyz; Domain=example.com", &url);

    let header = jar.get_cookie_header(&url);
    assert!(header.contains("token=xyz"));

    let sub_url = Url::parse("https://api.example.com/").unwrap();
    let header2 = jar.get_cookie_header(&sub_url);
    assert!(header2.contains("token=xyz"));

    let other_url = Url::parse("https://other.com/").unwrap();
    let header3 = jar.get_cookie_header(&other_url);
    assert!(header3.is_empty());
}

#[test]
fn test_cdp_cookie_with_leading_dot_domain_matches_requests() {
    let jar = CookieJar::new();
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "token".to_string(),
        value: "xyz".to_string(),
        domain: ".example.com".to_string(),
        path: "/".to_string(),
        secure: false,
        http_only: false,
        same_site: String::new(),
        expires: None,
    }]);

    let apex_url = Url::parse("https://example.com/").unwrap();
    let apex_header = jar.get_cookie_header(&apex_url);
    assert!(apex_header.contains("token=xyz"));

    let subdomain_url = Url::parse("https://api.example.com/").unwrap();
    let subdomain_header = jar.get_cookie_header(&subdomain_url);
    assert!(subdomain_header.contains("token=xyz"));

    let other_url = Url::parse("https://other.com/").unwrap();
    let other_header = jar.get_cookie_header(&other_url);
    assert!(other_header.is_empty());
}

#[test]
fn test_secure_cookie_not_sent_over_http() {
    let jar = CookieJar::new();
    let https_url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("secure_token=secret; Secure", &https_url);

    let http_url = Url::parse("http://example.com/").unwrap();
    let header = jar.get_cookie_header(&http_url);
    assert!(header.is_empty());
}

#[test]
fn test_max_age_zero_deletes_cookie() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("session=abc", &url);
    assert!(jar.get_cookie_header(&url).contains("session=abc"));

    jar.set_cookie("session=abc; Max-Age=0", &url);
    assert!(jar.get_cookie_header(&url).is_empty());
}

#[test]
fn test_max_age_sets_expiry() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("token=xyz; Max-Age=3600", &url);
    assert!(jar.get_cookie_header(&url).contains("token=xyz"));
}

#[test]
fn test_expired_cookie_not_sent() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("old=gone; Expires=Thu, 01 Jan 2020 00:00:00 GMT", &url);
    assert!(jar.get_cookie_header(&url).is_empty());
}

#[test]
fn test_samesite_parsed() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("strict_cookie=val; SameSite=Strict", &url);
    assert!(jar.get_cookie_header(&url).contains("strict_cookie=val"));
}

#[test]
fn test_clear_cookies() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("a=1", &url);
    assert!(!jar.get_cookie_header(&url).is_empty());

    jar.clear();
    assert!(jar.get_cookie_header(&url).is_empty());
}

#[test]
fn test_set_cookies_from_cdp_preserves_same_site_and_expires() {
    let jar = CookieJar::new();
    let future_expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3600;
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "sid".to_string(),
        value: "abc".to_string(),
        domain: "example.com".to_string(),
        path: "/".to_string(),
        secure: true,
        http_only: true,
        same_site: "Strict".to_string(),
        expires: Some(future_expiry),
    }]);

    let cookies = jar.get_all_cookies();
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].same_site, "Strict");
    assert_eq!(cookies[0].expires, Some(future_expiry));
}

#[test]
fn test_set_cookies_from_cdp_session_when_expires_none() {
    let jar = CookieJar::new();
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "n".to_string(),
        value: "v".to_string(),
        domain: "example.com".to_string(),
        path: "/".to_string(),
        secure: false,
        http_only: false,
        same_site: String::new(),
        expires: None,
    }]);
    let cookies = jar.get_all_cookies();
    assert_eq!(cookies[0].expires, None);
    assert_eq!(cookies[0].same_site, DEFAULT_SAME_SITE);
}

#[test]
fn test_delete_cookies_filtered_path_mismatch_preserves_cookie() {
    let jar = CookieJar::new();
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "sid".to_string(),
        value: "v".to_string(),
        domain: "example.com".to_string(),
        path: "/admin".to_string(),
        secure: false,
        http_only: false,
        same_site: String::new(),
        expires: None,
    }]);
    jar.delete_cookies_filtered("sid", "example.com", Some("/other"));
    assert_eq!(jar.get_all_cookies().len(), 1);

    jar.delete_cookies_filtered("sid", "example.com", Some("/admin"));
    assert!(jar.get_all_cookies().is_empty());
}

#[test]
fn test_delete_cookies_filtered_no_path_deletes_regardless() {
    let jar = CookieJar::new();
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "sid".to_string(),
        value: "v".to_string(),
        domain: "example.com".to_string(),
        path: "/admin".to_string(),
        secure: false,
        http_only: false,
        same_site: String::new(),
        expires: None,
    }]);
    jar.delete_cookies_filtered("sid", "example.com", None);
    assert!(jar.get_all_cookies().is_empty());
}

#[test]
fn test_set_cookies_from_cdp_expired_does_not_persist() {
    let jar = CookieJar::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "old".to_string(),
        value: "v".to_string(),
        domain: "example.com".to_string(),
        path: "/".to_string(),
        secure: false,
        http_only: false,
        same_site: String::new(),
        expires: Some(now - 1),
    }]);
    let url = Url::parse("https://example.com/").unwrap();
    assert!(jar.get_cookie_header(&url).is_empty());
}

#[test]
fn test_same_name_cookies_with_different_paths_coexist() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("n=a; Path=/", &url);
    jar.set_cookie("n=b; Path=/app", &url);

    let app_url = Url::parse("https://example.com/app/page").unwrap();
    let header = jar.get_cookie_header(&app_url);
    assert!(header.contains("n=a"), "root cookie missing in '{header}'");
    assert!(header.contains("n=b"), "app cookie missing in '{header}'");
}

#[test]
fn test_same_name_same_path_cookie_is_replaced() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("n=a; Path=/app", &url);
    jar.set_cookie("n=b; Path=/app", &url);

    let app_url = Url::parse("https://example.com/app/page").unwrap();
    let header = jar.get_cookie_header(&app_url);
    assert!(header.contains("n=b"));
    assert!(!header.contains("n=a"));
}

#[test]
fn test_max_age_zero_deletes_only_matching_path() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("n=root; Path=/", &url);
    jar.set_cookie("n=app; Path=/app", &url);

    jar.set_cookie("n=gone; Path=/app; Max-Age=0", &url);

    let app_url = Url::parse("https://example.com/app/page").unwrap();
    let header = jar.get_cookie_header(&app_url);
    assert!(header.contains("n=root"), "root-path cookie must survive");
    assert!(!header.contains("n=app"), "app-path cookie must be deleted");
}

#[test]
fn test_expired_update_deletes_existing_cookie() {
    // An expired Set-Cookie is a deletion, not a no-op: previously the old
    // cookie survived because exp<now just returned early.
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/app/login").unwrap();
    jar.set_cookie("s=old", &url);
    let app_url = Url::parse("https://example.com/app/dashboard").unwrap();
    assert!(jar.get_cookie_header(&app_url).contains("s=old"));

    jar.set_cookie("s=new; Expires=Thu, 01 Jan 2020 00:00:00 GMT", &url);
    assert!(jar.get_cookie_header(&app_url).is_empty());
}

#[test]
fn test_default_cookie_path_is_request_directory() {
    assert_eq!(default_cookie_path("/app/login"), "/app");
    assert_eq!(default_cookie_path("/app/"), "/app");
    assert_eq!(default_cookie_path("/"), "/");
    assert_eq!(default_cookie_path("/x"), "/");
    assert_eq!(default_cookie_path(""), "/");

    // Cookie set on /app/login must match /app/dashboard.
    let jar = CookieJar::new();
    let login = Url::parse("https://example.com/app/login").unwrap();
    jar.set_cookie("sess=1", &login);
    let dashboard = Url::parse("https://example.com/app/dashboard").unwrap();
    assert!(jar.get_cookie_header(&dashboard).contains("sess=1"));
}

#[test]
fn test_path_match_requires_slash_boundary() {
    assert!(path_matches("/admin", "/admin"));
    assert!(path_matches("/admin/users", "/admin"));
    assert!(path_matches("/admin/", "/admin/"));
    assert!(!path_matches("/administrator", "/admin"));
    assert!(!path_matches("/api", "/app"));

    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/admin").unwrap();
    jar.set_cookie("a=1; Path=/admin", &url);
    let sibling = Url::parse("https://example.com/administrator").unwrap();
    assert!(jar.get_cookie_header(&sibling).is_empty(), "Path=/admin must not leak to /administrator");
}

#[test]
fn test_unrelated_domain_attr_falls_back_to_host_only() {
    // A response from sub.example.com claiming Domain=other.com must not
    // scope a cookie to other.com (cookie tossing).
    let jar = CookieJar::new();
    let url = Url::parse("https://sub.example.com/").unwrap();
    jar.set_cookie("evil=1; Domain=other.com", &url);

    assert!(jar.get_cookie_header(&Url::parse("https://other.com/").unwrap()).is_empty());
    // Falls back to host-only on the origin.
    assert!(jar.get_cookie_header(&url).contains("evil=1"));
}

#[test]
fn test_host_only_cookie_not_sent_to_subdomain() {
    let jar = CookieJar::new();
    let apex = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("h=1", &apex); // no Domain attr → host-only

    assert!(jar.get_cookie_header(&apex).contains("h=1"));
    let sub = Url::parse("https://www.example.com/").unwrap();
    assert!(jar.get_cookie_header(&sub).is_empty(), "host-only cookie must not leak to subdomains");
}

#[test]
fn test_samesite_normalized() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("a=1; SameSite=strict", &url);
    let cookies = jar.get_all_cookies();
    assert_eq!(cookies[0].same_site, "Strict");
}

#[test]
fn test_save_load_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cookies.json");

    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("session=abc123; Domain=example.com; Path=/", &url);
    jar.set_cookie("token=xyz; Secure; HttpOnly", &url);

    jar.save_to_file(&path).unwrap();
    assert!(path.exists());

    let jar2 = CookieJar::new();
    let count = jar2.load_from_file(&path).unwrap();
    assert_eq!(count, 2);

    let header = jar2.get_cookie_header(&url);
    assert!(header.contains("session=abc123"));
    assert!(header.contains("token=xyz"));
}

#[test]
fn test_load_nonexistent_file_returns_zero() {
    let jar = CookieJar::new();
    let count = jar
        .load_from_file(std::path::Path::new("/nonexistent/cookies.json"))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_domain_matches_subdomain_without_leading_dot() {
    let jar = CookieJar::new();
    jar.set_cookies_from_cdp(vec![CookieInfo {
        name: "session".to_string(),
        value: "abc".to_string(),
        domain: "xiaohongshu.com".to_string(),
        path: "/".to_string(),
        secure: false,
        http_only: true,
        same_site: String::new(),
        expires: None,
    }]);
    let url = Url::parse("https://www.xiaohongshu.com/explore").unwrap();
    let header = jar.get_cookie_header(&url);
    assert!(header.contains("session=abc"), "Cookie header was: '{}'", header);
}

#[test]
fn test_cookie_from_file_load_then_send_in_request() {
    // Simulate what happens: load cookies from file → navigate → cookie should be in request
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cookies.json");
    
    // Write cookies like we exported from Chrome
    let cookies = serde_json::json!([
        {"name": "a1", "value": "testval", "domain": "xiaohongshu.com", "path": "/", "secure": false, "httpOnly": false},
        {"name": "web_session", "value": "sess123", "domain": "xiaohongshu.com", "path": "/", "secure": false, "httpOnly": true},
    ]);
    std::fs::write(&path, serde_json::to_string(&cookies).unwrap()).unwrap();
    
    let jar = CookieJar::new();
    let count = jar.load_from_file(&path).unwrap();
    assert_eq!(count, 2, "Should load 2 cookies");
    
    let url = Url::parse("https://www.xiaohongshu.com/explore").unwrap();
    let header = jar.get_cookie_header(&url);
    assert!(header.contains("a1=testval"), "Missing a1 in: '{}'", header);
    assert!(header.contains("web_session=sess123"), "Missing web_session in: '{}'", header);
}

// #69: Expires is untrusted header input — range-check before arithmetic.

#[test]
fn test_parse_http_date_rejects_out_of_range_fields() {
    // An unbounded year turned the accumulation loop into a single-header
    // CPU burn (~10^11 iterations for year 99999999999).
    assert!(parse_http_date("Sat, 01 Jan 99999999999 00:00:00 GMT").is_err());
    // day=0 underflowed `day - 1` into a far-future "permanent" cookie.
    assert!(parse_http_date("Sat, 00 Jan 2030 00:00:00 GMT").is_err());
    assert!(parse_http_date("Sat, 32 Jan 2030 00:00:00 GMT").is_err());
    assert!(parse_http_date("Sat, 01 Jan 2030 99:00:00 GMT").is_err());
    assert!(parse_http_date("Sat, 01 Jan 2030 00:99:00 GMT").is_err());
    assert!(parse_http_date("Sat, 01 Jan 2030 00:00:99 GMT").is_err());
    // Boundaries still parse: the HTTP-date leap second and max year.
    assert!(parse_http_date("Sat, 01 Jan 9999 23:59:60 GMT").is_ok());
    assert!(parse_http_date("Sat, 01 Jan 1601 00:00:00 GMT").is_ok());
}

// #68: IP-literal origins never domain-widen (RFC 6265 §5.1.3).

#[test]
fn test_ip_literal_origin_never_widens_domain() {
    let (domain, host_only) =
        resolve_cookie_domain("192.168.1.100", Some("168.1.100")).unwrap();
    assert_eq!(domain, "192.168.1.100");
    assert!(host_only, "a Domain attribute on an IP host is ignored entirely");

    // Suffix strings must not match across unrelated IPs.
    assert!(!domain_matches("10.168.1.100", "168.1.100"));
    assert!(domain_matches("192.168.1.100", "192.168.1.100"));
    // Regular host suffix matching is unchanged.
    assert!(domain_matches("sub.example.com", "example.com"));
}

#[test]
fn test_cookie_set_on_ip_host_stays_on_that_ip() {
    let jar = CookieJar::new();
    let url = Url::parse("http://192.168.1.100/").unwrap();
    jar.set_cookie("s=1; Domain=168.1.100", &url);
    assert!(jar.get_cookie_header(&url).contains("s=1"));

    let other = Url::parse("http://10.168.1.100/").unwrap();
    assert!(
        jar.get_cookie_header(&other).is_empty(),
        "a host whose string merely ends in the suffix must not receive the cookie"
    );
}

// #67: RFC 6265bis §4.1.3 cookie name prefixes, on both write paths.

#[test]
fn test_secure_prefix_requires_secure_attribute() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("__Secure-sid=1", &url);
    jar.set_cookie("__secure-sid=1", &url); // prefix match is case-insensitive
    assert!(
        jar.get_all_cookies().is_empty(),
        "__Secure- without the Secure attribute must be rejected"
    );
    jar.set_cookie("__Secure-sid=1; Secure", &url);
    assert!(jar.get_cookie_header(&url).contains("__Secure-sid=1"));
}

#[test]
fn test_host_prefix_requires_secure_hostonly_root_path() {
    let jar = CookieJar::new();
    let https = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("__Host-sid=1", &https); // missing Secure
    jar.set_cookie("__Host-sid=1; Secure; Domain=example.com", &https); // not host-only
    jar.set_cookie("__Host-sid=1; Secure; Path=/app", &https); // not root path
    assert!(
        jar.get_all_cookies().is_empty(),
        "all three __Host- violations must be rejected"
    );
    jar.set_cookie("__Host-sid=1; Secure; Path=/", &https);
    assert!(jar.get_cookie_header(&https).contains("__Host-sid=1"));
}

// #137 (obscura #1099): the __Host- Path rule reads the *attribute*,
// not the request path — a root URL with no Path attribute derives "/"
// but must not qualify; an explicit Path=/ on a deep URL must be honored.
#[test]
fn test_host_prefix_path_rule_reads_the_attribute_not_the_url() {
    let jar = CookieJar::new();
    let root = Url::parse("https://example.com/").unwrap();
    jar.set_cookie("__Host-sid=1; Secure", &root);
    assert!(
        jar.get_all_cookies().is_empty(),
        "a derived '/' from the request URL is not an explicit Path=/ attribute"
    );
    let deep = Url::parse("https://example.com/a/b").unwrap();
    jar.set_cookie("__Host-sid=1; Secure; Path=/", &deep);
    assert!(
        jar.get_cookie_header(&root).contains("__Host-sid=1"),
        "an explicit Path=/ on a deep-URL response is a valid __Host- cookie"
    );
    let jar2 = CookieJar::new();
    jar2.set_cookie("__Host-sid=1; Secure", &deep); // derives path "/a"
    assert!(jar2.get_all_cookies().is_empty());
}

#[test]
fn test_cookie_prefixes_enforced_on_document_cookie_writes() {
    let jar = CookieJar::new();
    let url = Url::parse("https://example.com/").unwrap();
    jar.set_cookie_from_js("__Host-sid=1; Path=/", &url);
    assert!(
        jar.get_cookie_header(&url).is_empty(),
        "page JS must not be able to mint a __Host- cookie itself"
    );
    jar.set_cookie_from_js("__Secure-sid=1; Secure", &url);
    assert!(
        jar.get_cookie_header(&url).contains("__Secure-sid=1"),
        "a valid __Secure- cookie still stores from document.cookie"
    );
}

// obscura#1107: loopback HTTP is a trustworthy origin in Chromium —
// Secure cookies are set, stored and sent over http://localhost (and
// 127.0.0.1 / [::1]), while any other plain-HTTP host stays refused.

#[test]
fn test_secure_cookie_flows_on_loopback_http() {
    let jar = CookieJar::new();
    let local = Url::parse("http://localhost:8080/app").unwrap();
    jar.set_cookie("sess=abc; Secure; Path=/", &local);
    assert!(
        jar.get_cookie_header(&local).contains("sess=abc"),
        "Chrome sends Secure cookies to http://localhost"
    );
    assert!(
        jar.get_js_visible_cookies(&local).contains("sess=abc"),
        "document.cookie on localhost sees it too"
    );

    // The other loopback spellings, including IPv6.
    for raw in ["http://127.0.0.1:3000/", "http://[::1]:3000/"] {
        let url = Url::parse(raw).unwrap();
        jar.set_cookie("k=v; Secure", &url);
        assert!(
            jar.get_cookie_header(&url).contains("k=v"),
            "loopback host {raw} must accept Secure cookies"
        );
    }

    // A Secure cookie stored from https is also SENT to the loopback
    // http form of the same host — Chrome's trust, not the scheme.
    let https_local = Url::parse("https://localhost/").unwrap();
    jar.set_cookie("cross=1; Secure", &https_local);
    let http_local = Url::parse("http://localhost/").unwrap();
    assert!(
        jar.get_cookie_header(&http_local).contains("cross=1"),
        "the trust follows the host, so http://localhost receives it"
    );
}

#[test]
fn test_secure_cookie_prefixes_unlock_on_loopback_http() {
    let jar = CookieJar::new();
    let local = Url::parse("http://dev.localhost/").unwrap();
    jar.set_cookie("__Host-sid=1; Secure; Path=/", &local);
    assert!(
        jar.get_cookie_header(&local).contains("__Host-sid=1"),
        "*.localhost is trustworthy — the prefix unlocks over plain HTTP"
    );
    jar.set_cookie_from_js("__Secure-sid=2; Secure", &local);
    assert!(
        jar.get_js_visible_cookies(&local).contains("__Secure-sid=2"),
        "document.cookie can mint __Secure- on loopback http, as in Chrome"
    );

    // Control: a non-loopback http origin keeps refusing everything.
    let jar2 = CookieJar::new();
    let remote = Url::parse("http://example.com/").unwrap();
    jar2.set_cookie("__Host-sid=1; Secure; Path=/", &remote);
    jar2.set_cookie("plain=1; Secure", &remote);
    assert!(
        jar2.get_all_cookies().is_empty(),
        "plain HTTP on a public host must still refuse Secure cookies entirely"
    );
}
