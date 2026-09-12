//! HAR 1.2 export and media-link sniffing over a Page's recorded network
//! events.
//!
//! The consumer shaping this module is the playback-link workflow: an agent
//! drives a session, the site's player boots and issues its runtime requests
//! (manifest/API/segment fetches), and those requests land in
//! `Page::network_events`. Media links are extracted from that request log —
//! URLs that only appear in page HTML are frequently decoys, so the network
//! log is the source of truth for "what can actually be played".

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::diting_browser::page::{NetworkEvent, StoredResponseBody};

/// ISO 8601 UTC timestamp (HAR `startedDateTime` shape) from unix seconds.
/// Civil-from-days is Howard Hinnant's algorithm; avoids a chrono dependency
/// for one format call per entry.
pub fn iso8601(unix_secs: f64) -> String {
    if !unix_secs.is_finite() {
        return "1970-01-01T00:00:00.000Z".to_string();
    }
    let secs = unix_secs.floor() as i64;
    let millis = (((unix_secs - secs as f64) * 1000.0).round() as i64).clamp(0, 999);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

/// Days-since-epoch -> (year, month, day). Hinnant 2013.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `(path suffix, kind)` table, checked against the lowercased URL with any
/// query/fragment stripped. Order is irrelevant — suffixes are distinct.
const MEDIA_SUFFIXES: &[(&str, &str)] = &[
    (".m3u8", "hls"),
    (".mpd", "dash"),
    (".flv", "flv"),
    (".mp4", "mp4"),
    (".m4v", "mp4"),
    (".ts", "ts"),
    (".m4s", "segment"),
    (".webm", "webm"),
    (".mp3", "mp3"),
    (".m4a", "m4a"),
    (".aac", "aac"),
];

/// `(content-type substring, kind)` fallback for extension-less player URLs.
const MEDIA_MIMES: &[(&str, &str)] = &[
    ("mpegurl", "hls"),
    ("dash+xml", "dash"),
    ("video/mp2t", "ts"),
    ("video/", "video"),
    ("audio/", "audio"),
];

/// Classify a request as a media/streaming resource: URL path suffix first
/// (players request manifests with expiry query strings), then the response
/// Content-Type. Returns the short kind tag, or None for non-media traffic.
pub fn media_kind(url: &str, mime: Option<&str>) -> Option<&'static str> {
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    let bare = bare.to_ascii_lowercase();
    if let Some((_, kind)) = MEDIA_SUFFIXES.iter().find(|(s, _)| bare.ends_with(s)) {
        return Some(kind);
    }
    let mime = mime.unwrap_or("").to_ascii_lowercase();
    MEDIA_MIMES
        .iter()
        .find(|(m, _)| mime.contains(m))
        .map(|(_, kind)| *kind)
}

/// Anti-bot challenge endpoints the engine can recognize by URL shape.
/// These answer 200 with a challenge page, so status alone reads as success
/// — exactly the shape that left an mtop promise pending with no signal
/// (the tmall report: the API redirect into `_____tmd_____/punish` looked
/// like a normal response row). Returns the short challenge tag.
pub fn challenge_kind(url: &str) -> Option<&'static str> {
    // TMD (taobao/tmall anti-bot): the punish path lands the challenge page.
    // Path-shaped — query/fragment stripped first, same convention as
    // media_kind — so a mere query param naming the marker never trips it.
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    let lower = bare.to_ascii_lowercase();
    if lower.contains("_____tmd_____/punish") {
        return Some("punish");
    }
    // The dedicated punish hosts (where the x5sec slider lives) are host-shaped
    // and unambiguous — a redirect that lands there IS the wall, no path
    // convention needed.
    if lower.contains("punish.taobao.com") || lower.contains("punish.tmall.com") {
        return Some("punish");
    }
    None
}

/// Risk control that answers 200 with an ordinary-looking JSON body. The
/// taobao x5 wall fronts MTop requests this way: the URL stays the plain
/// API endpoint, only the `ret` array (`FAIL_SYS_USER_VALIDATE`,
/// `RGV587_ERROR::SM`) or an `x5secdata` field says the response is a
/// challenge. URL-shape detection can never see this — the body is the
/// only signal.
pub fn body_challenge(body: &str) -> Option<&'static str> {
    if body.contains("FAIL_SYS_USER_VALIDATE")
        || body.contains("RGV587")
        || body.contains("x5secdata")
        || body.contains("_____tmd_____/punish")
    {
        Some("punish")
    } else {
        None
    }
}

/// Every anti-bot challenge the session's traffic hit, as structured rows —
/// the one-call answer to "did we get punished". URL-shaped walls come via
/// [`challenge_kind`]; body-shaped ones (200-status MTop JSON) via
/// [`body_challenge`] on retained XHR/fetch bodies. Rows carry `via` so an
/// agent can tell a wall it navigated into from one an API call swallowed.
pub fn challenge_rows(
    events: &[NetworkEvent],
    body_of: &dyn Fn(&str) -> Option<StoredResponseBody>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for e in events {
        if let Some(kind) = challenge_kind(&e.url) {
            out.push(json!({
                "url": e.url,
                "method": e.method,
                "status": e.status,
                "kind": kind,
                "via": "url",
            }));
            continue;
        }
        // Body-shaped: only script-initiated responses that actually landed
        // can carry the MTop risk-control JSON.
        if e.resource_type != "XHR" && e.resource_type != "Fetch" {
            continue;
        }
        if e.status == 0 || e.error.is_some() {
            continue;
        }
        let Some(body) = body_of(&e.request_id) else {
            continue;
        };
        if body.base64_encoded {
            continue;
        }
        if let Some(kind) = body_challenge(&body.body) {
            out.push(json!({
                "url": e.url,
                "method": e.method,
                "status": e.status,
                "kind": kind,
                "via": "body",
            }));
        }
    }
    out
}

/// Compact one-line-per-request view for agents: method/url/status/type/size.
/// `status: 0` rows carry the reason they never produced a servable response
/// (SSRF block, CORS refusal, transport failure) in `error`.
pub fn compact_events(events: &[NetworkEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|e| {
            let mut row = json!({
                "method": e.method,
                "url": e.url,
                "status": e.status,
                "type": e.resource_type,
                "size": e.body_size,
            });
            if let Some(error) = &e.error {
                row["error"] = json!(error);
            }
            if let Some(kind) = challenge_kind(&e.url) {
                row["challenge"] = json!(kind);
            }
            row
        })
        .collect()
}

/// XHR/fetch rows with their retained response bodies — the page's own API
/// face (Scrapling's `capture_xhr` insight: the background API a page calls
/// is the clean structured read, an order of magnitude cheaper than chewing
/// the rendered DOM). `url_substrings` filters (`[]` = every XHR/fetch);
/// `body_of` is the page's retained-body lookup. Text-stored bodies only —
/// the DevTools retention policy keeps replacement-free text as strings,
/// and base64 rows are binary assets an agent can't read as JSON anyway.
/// Capped: bodies are big, and 20 is more APIs than any one page has.
pub fn xhr_bodies(
    events: &[NetworkEvent],
    url_substrings: &[String],
    body_max_chars: usize,
    body_of: &dyn Fn(&str) -> Option<StoredResponseBody>,
) -> Vec<Value> {
    const MAX_ENTRIES: usize = 20;
    let mut out = Vec::new();
    for e in events {
        if out.len() >= MAX_ENTRIES {
            tracing::warn!("xhr_bodies: more than {MAX_ENTRIES} matching requests, dropping the tail");
            break;
        }
        if e.resource_type != "XHR" && e.resource_type != "Fetch" {
            continue;
        }
        if e.status == 0 || e.error.is_some() {
            continue;
        }
        if !url_substrings.is_empty()
            && !url_substrings.iter().any(|s| e.url.contains(s.as_str()))
        {
            continue;
        }
        let Some(body) = body_of(&e.request_id) else {
            continue;
        };
        if body.base64_encoded {
            continue;
        }
        let mime = e
            .response_headers
            .get("content-type")
            .cloned()
            .unwrap_or_default();
        let (body_text, body_truncated) =
            if body_max_chars > 0 && body.body.chars().count() > body_max_chars {
                (
                    body.body.chars().take(body_max_chars).collect::<String>(),
                    true,
                )
            } else {
                (body.body.clone(), false)
            };
        let mut row = json!({
            "url": e.url,
            "method": e.method,
            "status": e.status,
            "mime": mime,
            "body": body_text,
            "body_truncated": body_truncated,
        });
        // Risk-control JSON answers 200 like any other API row — the tag is
        // the only thing separating it from a successful response.
        if let Some(kind) = body_challenge(&body.body) {
            row["challenge"] = json!(kind);
        }
        out.push(row);
    }
    out
}

/// Media requests only, with the classification tag and response MIME. This
/// is the playback-link sniffer surface: every entry is a request the page
/// actually issued, not a URL scraped out of markup.
pub fn media_entries(events: &[NetworkEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| {
            let mime = e
                .response_headers
                .get("content-type")
                .map(|s| s.as_str());
            media_kind(&e.url, mime).map(|kind| {
                json!({
                    "url": e.url,
                    "kind": kind,
                    "status": e.status,
                    "mime": mime.unwrap_or(""),
                    "type": e.resource_type,
                    "via": "network",
                })
            })
        })
        .collect()
}

/// Playback URLs hidden inside retained XHR/fetch response bodies. Players
/// receive signed media links from JSON APIs (bilibili `playurl` durl/dash
/// being the canonical case) and the engine has no media pipeline, so those
/// links never appear as network entries — without this pass they would be
/// invisible to the filter=media sniffer even though the page holds them.
pub fn media_from_bodies(
    events: &[NetworkEvent],
    body_of: &dyn Fn(&str) -> Option<StoredResponseBody>,
) -> Vec<Value> {
    const MAX_SCAN: usize = 4 * 1024 * 1024;
    let mut seen: std::collections::HashSet<String> =
        events.iter().map(|e| e.url.clone()).collect();
    let mut out = Vec::new();
    for e in events {
        if e.resource_type != "XHR" && e.resource_type != "Fetch" {
            continue;
        }
        let Some(body) = body_of(&e.request_id) else {
            continue;
        };
        // JSON media manifests are plain text; base64 bodies are binary
        // assets where URL extraction would only produce noise.
        if body.base64_encoded || body.body.len() > MAX_SCAN {
            continue;
        }
        for url in extract_media_urls(&body.body) {
            if seen.insert(url.clone()) {
                if let Some(kind) = media_kind(&url, None) {
                    out.push(json!({
                        "url": url,
                        "kind": kind,
                        "status": e.status,
                        "mime": "",
                        "type": e.resource_type,
                        "via": "body",
                    }));
                }
            }
        }
    }
    out
}

/// Pull `http(s)://` URLs whose path carries a media suffix out of a text
/// body. JSON escapes must be flattened first (`&` → `&`, `\/` → `/`)
/// or signed query strings get truncated mid-escape.
fn extract_media_urls(text: &str) -> Vec<String> {
    const DELIMS: &[char] = &['"', '\'', ' ', '\\', '<', '>', '(', ')', '[', ']', '{', '}', '\n', '\r', '\t'];
    let flat = text.replace("\\u0026", "&").replace("\\/", "/");
    let mut out = Vec::new();
    let mut rest = flat.as_str();
    while let Some(pos) = rest.find("http") {
        rest = &rest[pos..];
        let taken = if rest.starts_with("https://") || rest.starts_with("http://") {
            let end = rest.find(DELIMS).unwrap_or(rest.len());
            let mut url = rest[..end].to_string();
            while url.ends_with(['.', ',', ';', ':', '!', '?', '"', '\'']) {
                url.pop();
            }
            if url.len() > 12 {
                Some(url)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(url) = taken {
            out.push(url);
        }
        rest = &rest[4.min(rest.len())..];
    }
    out
}

/// Canonical status text for the phrases HAR consumers expect. Unknown codes
/// emit the empty string (legal per spec).
fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        412 => "Precondition Failed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn header_pairs(headers: &HashMap<String, String>) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect()
}

/// Full HAR 1.2 log (`application/json` body for `GET /session/:id/har`).
/// `body_of` resolves retained response bodies by request id; requests whose
/// body was not retained (over the entry/byte limits) simply omit `text`.
/// Phases we do not measure are `-1` per the HAR "not available" convention.
pub fn har_log(page_title: &str, events: &[NetworkEvent], body_of: &dyn Fn(&str) -> Option<StoredResponseBody>) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let started = events
        .first()
        .map(|e| iso8601(e.timestamp))
        .unwrap_or_else(|| iso8601(now));

    let entries: Vec<Value> = events
        .iter()
        .map(|e| {
            let mime = e
                .response_headers
                .get("content-type")
                .map(|s| s.as_str())
                .unwrap_or("");
            let mut content = Map::new();
            content.insert("size".into(), json!(e.body_size));
            content.insert("mimeType".into(), json!(mime));
            if let Some(body) = body_of(&e.request_id) {
                content.insert("text".into(), json!(body.body));
                if body.base64_encoded {
                    content.insert("encoding".into(), json!("base64"));
                }
            }
            json!({
                "startedDateTime": iso8601(e.timestamp),
                "time": -1.0,
                "_resourceType": e.resource_type,
                "request": {
                    "method": e.method,
                    "url": e.url,
                    "httpVersion": "unknown",
                    "headers": header_pairs(&e.headers),
                    "queryString": query_pairs(&e.url),
                    "cookies": [],
                    "headersSize": -1,
                    "bodySize": 0,
                },
                "response": {
                    "status": e.status,
                    "statusText": status_text(e.status),
                    "httpVersion": "unknown",
                    "headers": header_pairs(&e.response_headers),
                    "cookies": [],
                    "content": Value::Object(content),
                    "redirectURL": e.response_headers.get("location").cloned().unwrap_or_default(),
                    "headersSize": -1,
                    "bodySize": e.body_size,
                },
                "cache": {},
                "timings": { "send": -1.0, "wait": -1.0, "receive": -1.0 },
            })
        })
        .collect();

    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "aginxbrowser", "version": env!("CARGO_PKG_VERSION") },
            "pages": [{
                "startedDateTime": started,
                "id": "page_1",
                "title": page_title,
                "pageTimings": { "onContentLoad": -1.0, "onLoad": -1.0 },
            }],
            "entries": entries,
        }
    })
}

/// `?a=b&c=d` from a URL, in order — the HAR `queryString` array.
fn query_pairs(url: &str) -> Vec<Value> {
    let Some(query) = url::Url::parse(url).ok().and_then(|u| u.query().map(String::from)) else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => json!({ "name": name, "value": value }),
            None => json!({ "name": pair, "value": "" }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(url: &str, resource_type: &str, status: u16, ts: f64) -> NetworkEvent {
        NetworkEvent {
            request_id: format!("p.{}", ts as u32),
            url: url.to_string(),
            method: "GET".to_string(),
            resource_type: resource_type.to_string(),
            status,
            headers: HashMap::new(),
            response_headers: std::sync::Arc::new(HashMap::new()),
            body_size: 0,
            timestamp: ts,
            error: None,
        }
    }

    #[test]
    fn iso8601_formats_epoch_and_2026() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_788_307_200.0), "2026-09-02T00:00:00.000Z");
        assert_eq!(iso8601(1_788_307_200.5), "2026-09-02T00:00:00.500Z");
        assert_eq!(iso8601(-1.0), "1969-12-31T23:59:59.000Z");
    }

    #[test]
    fn media_kind_reads_suffix_then_mime() {
        assert_eq!(
            media_kind(
                "https://cdn.example/live/index.m3u8?expires=99",
                Some("application/octet-stream")
            ),
            Some("hls")
        );
        assert_eq!(media_kind("https://cdn.example/v/f.mp4", None), Some("mp4"));
        // Extension-less player URL classified by Content-Type.
        assert_eq!(
            media_kind("https://cdn.example/manifest", Some("application/vnd.apple.mpegurl")),
            Some("hls")
        );
        assert_eq!(media_kind("https://cdn.example/api/resolve", Some("application/json")), None);
        assert_eq!(media_kind("https://cdn.example/app.js", None), None);
        // A .ts page route is only media when the path really ends in .ts.
        assert_eq!(media_kind("https://cdn.example/ts", None), None);
    }

    #[test]
    fn compact_events_carries_failure_reason_on_status_zero_rows() {
        let mut failed = event("https://h5api.example.com/rest", "Fetch", 0, 1.0);
        failed.error = Some("CORS error: Origin 'https://shop.example' not in Access-Control-Allow-Origin ''".into());
        let ok = event("https://shop.example/", "Document", 200, 2.0);
        let rows = compact_events(&[failed, ok]);
        assert!(rows[0].get("error").is_some(), "status-0 row must carry its reason");
        assert!(
            rows[1].get("error").is_none(),
            "successful rows stay lean — no null error field"
        );
    }

    #[test]
    fn compact_events_marks_challenge_rows() {
        // The tmall report shape: the mtop API redirect lands on a punish
        // page that answers 200 — status alone reads as success, so the
        // row needs the explicit challenge tag.
        let mut punished = event(
            "https://h5api.m.tmall.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/_____tmd_____/punish",
            "Fetch",
            200,
            1.0,
        );
        punished.error = None;
        let api = event(
            "https://h5api.m.tmall.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/",
            "Fetch",
            200,
            2.0,
        );
        let rows = compact_events(&[punished, api]);
        assert_eq!(rows[0].get("challenge"), Some(&json!("punish")));
        assert!(
            rows[1].get("challenge").is_none(),
            "ordinary rows carry no challenge field"
        );
        // Path-shaped, not substring-anywhere: a query param mentioning the
        // marker must not trip it.
        assert_eq!(
            challenge_kind("https://shop.example/search?q=_____tmd_____/punish"),
            None
        );
        // The dedicated punish hosts are the x5 slider's home — a redirect
        // landing there is the wall regardless of path.
        assert_eq!(
            challenge_kind("https://punish.taobao.com/auth?x5sec=abc"),
            Some("punish")
        );
        assert_eq!(
            challenge_kind("https://PUNISH.TMALL.com/captcha/index.html"),
            Some("punish")
        );
    }

    #[test]
    fn body_challenge_reads_mtop_risk_control_json() {
        // The taobao 0.4.1 report shape: the MTop endpoint answers 200, the
        // URL stays the plain API path, only the body says "validate".
        let mtop = r#"{"api":"mtop.taobao.shop.simple.item.fetch","ret":["FAIL_SYS_USER_VALIDATE","RGV587_ERROR::SM::Validate failed"],"data":{"url":"https://h5api.m.taobao.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/?_____tmd_____/punish%3Faction%3Dcaptcha"}}"#;
        assert_eq!(body_challenge(mtop), Some("punish"));
        assert_eq!(
            body_challenge(r#"{"data":{"x5secdata":"zzz"}}"#),
            Some("punish")
        );
        // Ordinary success JSON never trips.
        assert_eq!(
            body_challenge(r#"{"api":"mtop.taobao.item.get","data":{}}"#),
            None
        );
        assert_eq!(body_challenge(""), None);
    }

    #[test]
    fn challenge_rows_see_url_and_body_walls() {
        // URL-shaped: the redirect that landed on the punish path.
        let landed = event(
            "https://h5api.m.taobao.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/_____tmd_____/punish",
            "Fetch",
            200,
            1.0,
        );
        // Body-shaped: plain API URL, 200, risk-control JSON retained.
        let swallowed = event(
            "https://h5api.m.taobao.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/",
            "Fetch",
            200,
            2.0,
        );
        let clean = event(
            "https://h5api.m.taobao.com/h5/mtop.taobao.recommend.feed/1.0/",
            "XHR",
            200,
            3.0,
        );
        let mut failed = event("https://h5api.m.taobao.com/h5/x/", "Fetch", 0, 4.0);
        failed.error = Some("net::ERR_FAILED".into());
        let bodies = HashMap::from([
            (
                swallowed.request_id.clone(),
                StoredResponseBody {
                    body: r#"{"ret":["FAIL_SYS_USER_VALIDATE"]}"#.into(),
                    base64_encoded: false,
                },
            ),
            (
                clean.request_id.clone(),
                StoredResponseBody {
                    body: r#"{"data":{"items":[]}}"#.into(),
                    base64_encoded: false,
                },
            ),
        ]);

        let rows = challenge_rows(
            &[landed, swallowed.clone(), clean.clone(), failed],
            &|rid| bodies.get(rid).cloned(),
        );
        assert_eq!(rows.len(), 2, "punished URL + swallowed body: {rows:?}");
        assert_eq!(rows[0]["via"], "url");
        assert_eq!(rows[0]["kind"], "punish");
        assert_eq!(rows[1]["via"], "body");
        assert_eq!(
            rows[1]["url"],
            "https://h5api.m.taobao.com/h5/mtop.taobao.shop.simple.item.fetch/1.0/"
        );

        // The same body-level tag rides the xhr_bodies rows.
        let xhr = xhr_bodies(&[swallowed, clean], &[], 0, &|rid| bodies.get(rid).cloned());
        assert_eq!(xhr[0].get("challenge"), Some(&json!("punish")));
        assert!(
            xhr[1].get("challenge").is_none(),
            "clean API row stays lean"
        );
    }

    #[test]
    fn xhr_bodies_keeps_script_initiated_text_responses_only() {
        let mut api = event("https://api.example/items?all=1", "Fetch", 200, 1.0);
        api.response_headers = std::sync::Arc::new(HashMap::from([(
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )]));
        let doc = event("https://shop.example/", "Document", 200, 2.0);
        let mut failed = event("https://api.example/broken", "XHR", 0, 3.0);
        failed.error = Some("net::ERR_FAILED".into());
        let bodies = HashMap::from([(
            api.request_id.clone(),
            StoredResponseBody {
                body: "{\"items\":[1,2,3]}".into(),
                base64_encoded: false,
            },
        )]);

        // Empty filter list = every script-initiated response; document rows,
        // failed requests and unretained bodies are dropped.
        let out = xhr_bodies(&[doc, api.clone(), failed], &[], 0, &|rid| bodies.get(rid).cloned());
        assert_eq!(out.len(), 1, "only the retained XHR body: {out:?}");
        assert_eq!(out[0]["url"], "https://api.example/items?all=1");
        assert_eq!(out[0]["status"], 200);
        assert_eq!(out[0]["mime"], "application/json; charset=utf-8");
        assert_eq!(out[0]["body"], "{\"items\":[1,2,3]}");
        assert_eq!(out[0]["body_truncated"], false);

        // Substring filter narrows; char cap truncates and flags. ("miss"
        // carries a longer body but is filtered out by URL.)
        let miss = event("https://api.example/other", "XHR", 200, 4.0);
        let long = StoredResponseBody {
            body: "abcdefghij".into(),
            base64_encoded: false,
        };
        let bodies2 = HashMap::from([
            (api.request_id.clone(), bodies.get(&api.request_id).unwrap().clone()),
            (miss.request_id.clone(), long),
        ]);
        let out = xhr_bodies(&[api, miss], &["/items".to_string()], 4, &|rid| bodies2.get(rid).cloned());
        assert_eq!(out.len(), 1, "filter keeps only /items: {out:?}");
        assert_eq!(out[0]["body"], "{\"it");
        assert_eq!(out[0]["body_truncated"], true);

        // Binary (base64-retained) bodies never join the agent-facing array.
        let mut bin = event("https://cdn.example/pic.png", "XHR", 200, 5.0);
        bin.request_id = "bin".into();
        let out = xhr_bodies(&[bin], &[], 0, &|rid| {
            (rid == "bin").then(|| StoredResponseBody {
                body: "iVBORw0KGgo=".into(),
                base64_encoded: true,
            })
        });
        assert!(out.is_empty(), "base64 bodies are skipped: {out:?}");
    }

    #[test]
    fn media_from_bodies_extracts_playback_links_from_playurl_json() {
        // bilibili-style playurl payload: signed durl URLs, & escapes,
        // a page event plus an unrelated XHR that must contribute nothing.
        let events = vec![
            event("https://e.example/video/BV1GJ411x7h7/", "Document", 200, 1.0),
            event("https://api.example/x/player/wbi/playurl?avid=1", "Fetch", 200, 2.0),
            event("https://api.example/x/web-interface/nav", "Fetch", 200, 3.0),
        ];
        let bodies: HashMap<String, StoredResponseBody> = HashMap::from([
            (
                events[1].request_id.clone(),
                StoredResponseBody {
                    body: "{\"code\":0,\"data\":{\"durl\":[{\"url\":\"https://cdn.example/upgcxcode/99/91/137649199/137649199_da2-1-16.mp4?e=abc\\u0026oi=9\",\"backup_url\":[\"https://upos.example/backup/137649199.mp4?e=z\\u0026oi=1\"]}],\"dash\":{\"video\":[{\"baseUrl\":\"https://v.example/dash/137649199.m4s\"}]}}}".into(),
                    base64_encoded: false,
                },
            ),
            (
                events[2].request_id.clone(),
                StoredResponseBody {
                    body: "{\"code\":0,\"data\":{\"isLogin\":false}}".into(),
                    base64_encoded: false,
                },
            ),
        ]);
        let body_of = |rid: &str| bodies.get(rid).cloned();
        let media = media_from_bodies(&events, &body_of);
        let urls: Vec<&str> = media
            .iter()
            .map(|m| m["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://cdn.example/upgcxcode/99/91/137649199/137649199_da2-1-16.mp4?e=abc&oi=9",
                "https://upos.example/backup/137649199.mp4?e=z&oi=1",
                "https://v.example/dash/137649199.m4s",
            ]
        );
        assert_eq!(media[0]["via"], "body");
        assert_eq!(media[0]["kind"], "mp4");
        // No bodies retained -> nothing extracted, no panic.
        let empty = media_from_bodies(&events, &|_| None);
        assert!(empty.is_empty());
    }

    #[test]
    fn media_entries_carry_kind_status_and_mime() {
        let mut events = vec![
            event("https://e.example/page", "Document", 200, 1.0),
            event("https://e.example/video/master.m3u8?tk=1", "Fetch", 200, 2.0),
        ];
        let headers = HashMap::from([(
            "content-type".to_string(),
            "application/vnd.apple.mpegurl".to_string(),
        )]);
        events[1].response_headers = std::sync::Arc::new(headers);
        let media = media_entries(&events);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["kind"], "hls");
        assert_eq!(media[0]["status"], 200);
        assert_eq!(media[0]["via"], "network");
        assert_eq!(media[0]["url"], "https://e.example/video/master.m3u8?tk=1");
    }

    #[test]
    fn har_log_is_valid_1_2_with_bodies_and_query_strings() {
        let mut doc = event("https://e.example/p?a=1&b", "Document", 200, 100.0);
        doc.body_size = 5;
        doc.response_headers = std::sync::Arc::new(HashMap::from([(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )]));
        let mut seg = event("https://e.example/seg-0.ts", "Fetch", 200, 101.0);
        seg.response_headers = std::sync::Arc::new(HashMap::from([(
            "content-type".to_string(),
            "video/mp2t".to_string(),
        )]));
        let bodies: HashMap<String, StoredResponseBody> = HashMap::from([
            (
                doc.request_id.clone(),
                StoredResponseBody { body: "hello".into(), base64_encoded: false },
            ),
            (
                seg.request_id.clone(),
                StoredResponseBody { body: "AAEC".into(), base64_encoded: true },
            ),
        ]);
        let log = har_log(
            "Example",
            &[doc, seg],
            &|rid: &str| bodies.get(rid).cloned(),
        );
        let log = log["log"].as_object().unwrap();
        assert_eq!(log["version"], "1.2");
        assert_eq!(log["pages"][0]["title"], "Example");
        let entries = log["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let doc_entry = &entries[0];
        assert_eq!(doc_entry["startedDateTime"], "1970-01-01T00:01:40.000Z");
        assert_eq!(doc_entry["_resourceType"], "Document");
        assert_eq!(doc_entry["request"]["queryString"][0]["name"], "a");
        assert_eq!(doc_entry["request"]["queryString"][1]["name"], "b");
        assert_eq!(doc_entry["response"]["content"]["text"], "hello");
        assert!(doc_entry["response"]["content"].get("encoding").is_none());
        let seg_entry = &entries[1];
        assert_eq!(seg_entry["response"]["content"]["encoding"], "base64");
        assert_eq!(seg_entry["response"]["statusText"], "OK");
        assert_eq!(seg_entry["timings"]["wait"], -1.0);
        // Not-retained bodies omit text but keep size.
        let bare = har_log("t", &[event("https://e.example/x", "Script", 404, 5.0)], &|_| None);
        let e = &bare["log"]["entries"][0];
        assert!(e["response"]["content"].get("text").is_none());
        assert_eq!(e["response"]["statusText"], "Not Found");
    }

    #[test]
    fn compact_events_are_token_shaped() {
        let events = vec![event("https://e.example/a.js", "Script", 200, 1.0)];
        let rows = compact_events(&events);
        assert_eq!(rows[0]["url"], "https://e.example/a.js");
        assert_eq!(rows[0]["type"], "Script");
        assert!(rows[0].get("mime").is_none());
    }
}
