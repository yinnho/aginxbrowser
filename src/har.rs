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

/// Compact one-line-per-request view for agents: method/url/status/type/size.
pub fn compact_events(events: &[NetworkEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|e| {
            json!({
                "method": e.method,
                "url": e.url,
                "status": e.status,
                "type": e.resource_type,
                "size": e.body_size,
            })
        })
        .collect()
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
