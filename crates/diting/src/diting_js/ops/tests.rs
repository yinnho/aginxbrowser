//! The ops module's tests, split from the module root (ARCHITECTURE.md
//! P2 god-file ratchet).
use super::{
    cors_response_allows, cors_unsafe_request_header_names, is_cors_safelisted_content_type,
    is_cors_safelisted_request_header, parse_cors_header_list, preflight_allows_header,
    preflight_allows_method, validate_fetch_url, FetchCredentials,
};
use super::{image_header_dimensions, pbkdf2_derive, PBKDF2_MAX_ITERATIONS, PBKDF2_MAX_OUTPUT_BYTES};

/// Header-only dimension parsing for the four formats op_image_info
/// serves. Byte layouts pinned against the spec tables: PNG IHDR (BE
/// u32s at fixed offsets), GIF LSD (LE u16s), JPEG SOFn behind EXIF
/// segments, and all three WebP chunk faces. Distinctive non-1×1
/// numbers throughout — a 1 here could pass via some other fallback.
#[test]
fn image_header_dimensions_all_formats() {
    // PNG: sig + chunk len + "IHDR" + w(62) + h(33).
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0, 0, 0, 0x0D]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&62u32.to_be_bytes());
    png.extend_from_slice(&33u32.to_be_bytes());
    assert_eq!(image_header_dimensions(&png), Some((62, 33)));

    // GIF89a: logical screen 100×7 (LE u16s at 6/8).
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&100u16.to_le_bytes());
    gif.extend_from_slice(&7u16.to_le_bytes());
    assert_eq!(image_header_dimensions(&gif), Some((100, 7)));

    // JPEG: SOI, a 16-byte EXIF APP1 segment ahead of SOF0 carrying
    // 400×300, then payload padding (the walker must hop the segment).
    let mut jpg = vec![0xFF, 0xD8];
    jpg.extend_from_slice(&[0xFF, 0xE1]); // APP1
    jpg.extend_from_slice(&16u16.to_be_bytes()); // seg len (includes itself)
    jpg.extend_from_slice(&[0u8; 14]);
    jpg.extend_from_slice(&[0xFF, 0xC0]); // SOF0
    jpg.extend_from_slice(&11u16.to_be_bytes());
    jpg.extend_from_slice(&[8]); // precision
    jpg.extend_from_slice(&300u16.to_be_bytes()); // h first per spec
    jpg.extend_from_slice(&400u16.to_be_bytes()); // w
    jpg.extend_from_slice(&[0x03, 0x01, 0x11, 0x00]); // component count + sampling
    assert_eq!(image_header_dimensions(&jpg), Some((400, 300)));

    // WebP VP8X: RIFF/WEBP + chunk header + flags(4) + w-1, h-1 as 24-bit LE.
    let mut vp8x = b"RIFF".to_vec();
    vp8x.extend_from_slice(&[0, 0, 0, 0]); // riff size
    vp8x.extend_from_slice(b"WEBPVP8X");
    vp8x.extend_from_slice(&10u32.to_le_bytes()); // chunk size
    vp8x.extend_from_slice(&[0, 0, 0, 0]); // reserved+flags
    vp8x.extend_from_slice(&[61, 0, 0]); // w-1 = 61
    vp8x.extend_from_slice(&[44, 0, 0]); // h-1 = 44
    assert_eq!(image_header_dimensions(&vp8x), Some((62, 45)));

    // WebP VP8L: 0x2F signature, then two packed 14-bit minus-one values:
    // w-1 = 61 fills byte 1; h-1 = 44 = 11<<2 starts at bit 14 (byte 2).
    let mut vp8l = b"RIFF".to_vec();
    vp8l.extend_from_slice(&[0, 0, 0, 0]);
    vp8l.extend_from_slice(b"WEBPVP8L");
    vp8l.extend_from_slice(&5u32.to_le_bytes());
    vp8l.extend_from_slice(&[0x2F, 61, 0x00, 0x0B, 0x00]); // w-1=61, h-1=44
    assert_eq!(image_header_dimensions(&vp8l), Some((62, 45)));

    // Garbage, empty, and a PNG signature cut before IHDR all miss.
    assert_eq!(image_header_dimensions(b"not an image"), None);
    assert_eq!(image_header_dimensions(&[]), None);
    assert_eq!(image_header_dimensions(&png[..16]), None);
}

/// (#103) ECDSA flat key payload: four u16-BE length-prefixed sections
/// [pkcs8, sec1, scalar, spki]; absent sections are a bare 0x0000. The JS
/// shim's ecdsaUnflat mirrors this walk, so the byte layout is the wire
/// contract between the two halves.
#[test]
fn ecdsa_flat_layout() {
    let flat = super::ecdsa_flat([b"PK", b"POINT", b"", b"SPKI-DER"]);
    let expect: Vec<u8> = [
        &[0u8, 2][..], b"PK",
        &[0, 5], b"POINT",
        &[0, 0],
        &[0, 8], b"SPKI-DER",
    ]
    .concat();
    assert_eq!(flat, expect);
}

/// (#103) The ECDSA prehash digests pin FIPS 180-4 vectors ("abc"); an
/// unknown hash name must be an error, not a silent fallback digest.
#[test]
fn ecdsa_digest_vectors_and_reject() {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let d = super::ecdsa_digest("SHA-256", b"abc").unwrap();
    assert_eq!(
        hex(&d),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let d = super::ecdsa_digest("SHA-384", b"abc").unwrap();
    assert_eq!(
        hex(&d),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    );
    assert!(super::ecdsa_digest("MD5", b"abc").is_err());
    assert!(super::ecdsa_digest("SHA-256", &[]).is_ok());
}

/// The #395 paint-only predicate: only a name diff inside
/// {transform, opacity} may keep the solve cache. Value changes,
/// whitelist additions/removals, prefixed properties and missing
/// before-states must all fall back to the full invalidation.
/// Chrome's computed backdrop-filter face: "blur(<length>)" or "none".
/// The props-table membership is asserted here too — a serialization
/// arm without a table entry is invisible to getComputedStyle.
#[cfg(feature = "screenshot")]
#[test]
fn backdrop_filter_computed_face() {
    assert!(super::COMPUTED_STYLE_PROPS.contains(&"backdrop-filter"));
    let mut s = crate::diting_css::ComputedStyle::default();
    assert_eq!(
        super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
        Some("none")
    );
    s.backdrop_blur = Some(12.0);
    assert_eq!(
        super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
        Some("blur(12px)")
    );
    s.backdrop_blur = Some(2.5);
    assert_eq!(
        super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
        Some("blur(2.5px)")
    );
}

#[cfg(feature = "screenshot")]
#[test]
fn style_write_paint_only_diff_matrix() {
    use super::{style_property_names, style_write_is_paint_only};
    // Values differ freely while the names stay — the seek case.
    assert!(style_write_is_paint_only(
        Some("transform: translate3d(-200px, 0px, 0px); opacity: 0"),
        Some("transform: translate3d(-32.4821px, 0px, 0px); opacity: 0.8376"),
    ));
    // Adding a whitelisted property to an existing whitelisted set.
    assert!(style_write_is_paint_only(
        Some("transform: translate3d(-200px, 0px, 0px)"),
        Some("transform: translate3d(-12px, 0px, 0px); opacity: 0.5"),
    ));
    // Clearing the animation off a style that only ever held it.
    assert!(style_write_is_paint_only(
        Some("opacity: 0.42; transform: scale(2)"),
        Some(""),
    ));
    // A geometry property appears — full invalidation.
    assert!(!style_write_is_paint_only(
        Some("transform: translate3d(-200px, 0px, 0px); opacity: 0"),
        Some("transform: translate3d(-200px, 0px, 0px); opacity: 0; width: 300px"),
    ));
    // Prefixed transform is a different name — never whitelisted on a
    // guess.
    assert!(!style_write_is_paint_only(
        Some("transform: scale(2)"),
        Some("transform: scale(2); -webkit-transform: scale(2)"),
    ));
    // A geometry property disappears.
    assert!(!style_write_is_paint_only(
        Some("width: 300px; opacity: 0.5"),
        Some("opacity: 0.5"),
    ));
    // No before-state to diff — conservative full drop (first write on
    // a bare element).
    assert!(!style_write_is_paint_only(None, Some("opacity: 0")));
    assert!(!style_write_is_paint_only(Some("opacity: 0"), None));
    // Name extraction: casing, whitespace and empty fragments.
    assert_eq!(
        style_property_names("TRANSFORM: scale(2) ; ; opacity:0"),
        ["transform", "opacity"].into_iter().map(str::to_string).collect::<std::collections::HashSet<_>>()
    );
}

// Ephemeral deployments must not persist login tokens: storage_file is
// the single choke point every localStorage flush goes through.
#[test]
fn storage_file_is_none_under_ephemeral() {
    let _env = crate::env_knobs::EPHEMERAL_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    std::env::remove_var("AGINXBROWSER_EPHEMERAL");
    assert!(super::storage_file("https://example.com").is_some());

    std::env::set_var("AGINXBROWSER_EPHEMERAL", "1");
    assert_eq!(super::storage_file("https://example.com"), None);
    assert_eq!(
        super::storage_file("https://login.taobao.com"),
        None,
        "the gate must sit above every origin"
    );
    std::env::remove_var("AGINXBROWSER_EPHEMERAL");
}

// obscura "enforce CORS preflight permissions" (04f0475) same-hole port:
// the safelist decides whether a cross-origin request even needs a
// preflight, so "any content-type" passing would have made every JSON
// API call a simple request.
#[test]
fn cors_safelist_content_type_is_value_sensitive() {
    for ok in [
        "text/plain",
        "text/plain; charset=utf-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=----x",
        "TEXT/PLAIN",
    ] {
        assert!(is_cors_safelisted_content_type(ok), "must be safelisted: {ok}");
    }
    for blocked in [
        "application/json",
        "application/json; charset=utf-8",
        "text/html",
        "text/plain()",
        "nonsense",
        "text/plain; utf-8\u{7f}",
    ] {
        assert!(
            !is_cors_safelisted_content_type(blocked),
            "must NOT be safelisted: {blocked}"
        );
    }
}

#[test]
fn cors_unsafe_header_names_sorted_and_lowercased() {
    let headers: std::collections::HashMap<String, String> = [
        ("Content-Type", "application/json".to_string()), // unsafe value
        ("X-Custom", "anything".to_string()),              // unsafe name
        ("Accept", "text/html".to_string()),               // safelisted
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    assert_eq!(
        cors_unsafe_request_header_names(&headers),
        vec!["content-type".to_string(), "x-custom".to_string()]
    );

    let simple: std::collections::HashMap<String, String> = [
        ("accept".to_string(), "text/html".to_string()),
        ("content-type".to_string(), "text/plain".to_string()),
    ]
    .into_iter()
    .collect();
    assert!(cors_unsafe_request_header_names(&simple).is_empty());
}

// The preflight gate itself: "*" is consent for anonymous requests only,
// and never for the Authorization header (Fetch standard).
#[test]
fn preflight_permissions_respect_credentials_and_authorization() {
    let put: reqwest::Method = reqwest::Method::PUT;
    let get = reqwest::Method::GET;

    assert!(preflight_allows_method(&put, &["PUT", "POST"], true));
    assert!(preflight_allows_method(&get, &[], true)); // safelisted method
    assert!(!preflight_allows_method(&put, &["POST"], false));
    assert!(preflight_allows_method(&put, &["*"], false));
    assert!(!preflight_allows_method(&put, &["*"], true));

    assert!(preflight_allows_header("content-type", &["Content-Type"], false));
    assert!(preflight_allows_header("x-custom", &["*"], false));
    assert!(!preflight_allows_header("x-custom", &["*"], true));
    assert!(!preflight_allows_header("authorization", &["*"], false));
}

#[test]
fn parse_cors_header_list_rejects_non_tokens() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "access-control-allow-methods",
        "GET, PUT".parse().unwrap(),
    );
    assert_eq!(
        parse_cors_header_list(&headers, "access-control-allow-methods"),
        Some(vec!["GET", "PUT"])
    );

    // Absent header parses to empty (a server that said nothing allowed
    // nothing) — but a malformed value is a parse failure.
    assert_eq!(
        parse_cors_header_list(&headers, "access-control-allow-headers"),
        Some(Vec::new())
    );
    headers.insert(
        "access-control-allow-headers",
        "X-Custom, bad name".parse().unwrap(),
    );
    assert_eq!(
        parse_cors_header_list(&headers, "access-control-allow-headers"),
        None
    );
}

// Range is safelisted only as a valid byte range (Fetch standard): the
// suffix form and inverted ranges must not ride the safelist.
#[test]
fn cors_safelisted_range_requires_valid_byte_range() {
    assert!(is_cors_safelisted_request_header("range", "bytes=0-1023"));
    assert!(is_cors_safelisted_request_header("range", "bytes=0-"));
    assert!(!is_cors_safelisted_request_header("range", "bytes=1023-0"));
    assert!(!is_cors_safelisted_request_header("range", "bytes=-500"));
    assert!(!is_cors_safelisted_request_header("range", "items=0-10"));
}

// Upstream obscura #708: file:// must be rejected up front for
// page-reachable fetch/XHR (deny-by-default, matching navigation),
// not allowed through the scheme gate to short-circuit SSRF checks.
#[test]
fn fetch_scheme_gate_rejects_file_up_front() {
    let file_url = url::Url::parse("file:///etc/passwd").unwrap();
    let err = validate_fetch_url(&file_url).unwrap_err();
    assert!(err.contains("Forbidden URL scheme 'file'"), "got: {err}");

    let ftp = url::Url::parse("ftp://example.com/x").unwrap();
    assert!(validate_fetch_url(&ftp).is_err());

    let https = url::Url::parse("https://example.com/x").unwrap();
    assert!(validate_fetch_url(&https).is_ok());
}

// The fetch gate must share the navigation deny-set, not a local
// re-listing: 198.18.0.0/15 (benchmarking), 100.64/10 (CGNAT metadata),
// 0.0.0.0, IPv4-mapped loopback and 6to4-wrapped link-local were all
// fetchable from page JS while the navigation gate blocked them
// (obscura #852 family).
#[test]
fn fetch_gate_shares_navigation_deny_set() {
    let _lock = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    let prev = std::env::var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK").ok();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

    for bad in [
        "http://198.18.0.1/",         // benchmarking range
        "http://100.100.100.200/",    // CGNAT cloud metadata
        "http://0.0.0.0/",            // unspecified, routes to localhost
        "http://[::ffff:127.0.0.1]/", // IPv4-mapped loopback
        "http://[2002:a9fe:a9fe::]/", // 6to4-wrapped link-local
    ] {
        let u = url::Url::parse(bad).unwrap();
        let err = validate_fetch_url(&u).unwrap_err();
        assert!(err.contains("not allowed"), "{bad}: got {err}");
    }

    assert!(validate_fetch_url(&url::Url::parse("http://example.com/x").unwrap()).is_ok());

    if let Some(v) = prev {
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", v);
    }
}

// Upstream b744b9b.
#[test]
fn fetch_credentials_gate_cookies_per_request_origin() {
    let page_origin = "https://www.example.com";
    let same = "https://www.example.com/api";
    let explicit_default_port = "https://www.example.com:443/api";
    let cross = "https://api.example.com/data";

    assert!(!FetchCredentials::Omit.allows(page_origin, same));
    assert!(!FetchCredentials::Omit.allows(page_origin, cross));

    assert!(FetchCredentials::SameOrigin.allows(page_origin, same));
    assert!(FetchCredentials::SameOrigin.allows(page_origin, explicit_default_port));
    assert!(!FetchCredentials::SameOrigin.allows(page_origin, cross));

    assert!(FetchCredentials::Include.allows(page_origin, same));
    assert!(FetchCredentials::Include.allows(page_origin, cross));
}

// Upstream b744b9b.
#[test]
fn credentialed_cors_requires_exact_origin_and_allow_credentials() {
    let page_origin = "https://www.example.com";

    assert!(cors_response_allows(FetchCredentials::SameOrigin, page_origin, "*", ""));
    assert!(cors_response_allows(FetchCredentials::SameOrigin, page_origin, page_origin, ""));
    assert!(!cors_response_allows(FetchCredentials::SameOrigin, page_origin, "https://other.example", ""));

    assert!(cors_response_allows(FetchCredentials::Include, page_origin, page_origin, "true"));
    assert!(!cors_response_allows(FetchCredentials::Include, page_origin, "*", ""));
    assert!(!cors_response_allows(FetchCredentials::Include, page_origin, page_origin, ""));
    assert!(!cors_response_allows(FetchCredentials::Include, page_origin, "https://other.example", "true"));
}

// Upstream cfda91b / #580 — PBKDF2 parameters arrive straight from page JS.
// Without caps, a huge iteration count pins the single-threaded runtime and
// a huge output length forces an unbounded allocation.
#[test]
fn pbkdf2_rejects_excessive_iterations() {
    let err = pbkdf2_derive("SHA-256", b"pw", b"salt", PBKDF2_MAX_ITERATIONS + 1, 32)
        .expect_err("iteration count above the cap must be rejected");
    assert!(
        err.to_string().contains("iteration"),
        "error should name the iteration cap: {err}"
    );
}

#[test]
fn pbkdf2_rejects_excessive_output_length() {
    let err = pbkdf2_derive("SHA-256", b"pw", b"salt", 1_000, PBKDF2_MAX_OUTPUT_BYTES + 1)
        .expect_err("output length above the cap must be rejected");
    assert!(
        err.to_string().contains("length"),
        "error should name the length cap: {err}"
    );
}

#[test]
fn pbkdf2_derives_within_limits() {
    let dk = pbkdf2_derive("SHA-256", b"password", b"salt", 1_000, 32)
        .expect("ordinary parameters must derive successfully");
    assert_eq!(dk.len(), 32, "derived key must have the requested length");
}
