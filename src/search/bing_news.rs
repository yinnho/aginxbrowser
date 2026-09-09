use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Bing News via the RSS output format (`format=RSS`) — the same route
/// SearXNG's bing_news engine effectively serves when HTML scraping breaks.
///
/// Connectivity is auto-sensed per request, not bound at startup: direct
/// first; on a geo-redirect (www.bing.com → cn.bing.com, which kills the
/// news vertical) or a transport failure, retry once through
/// AGINXBROWSER_PROXY when configured. Overseas deployments connect
/// directly with no proxy at all; CN deployments set it once and blocked
/// targets fall through automatically.
pub struct BingNewsEngine;

impl BingNewsEngine {
    pub fn new() -> Self {
        BingNewsEngine
    }

    fn proxied_client() -> Option<reqwest::Client> {
        crate::config::proxy_from_env().map(|proxy| {
            let proxy_str = if proxy.starts_with("socks5://") && !proxy.starts_with("socks5h://") {
                format!("socks5h{}", &proxy[7..])
            } else {
                proxy
            };
            let mut builder = crate::diting_net::client::reqwest_builder_no_env_proxy()
                .timeout(std::time::Duration::from_secs(12))
                .redirect(reqwest::redirect::Policy::none());
            match reqwest::Proxy::all(&proxy_str) {
                Ok(p) => builder = builder.proxy(p),
                Err(e) => tracing::warn!("bing_news proxy '{}' ignored: {}", proxy_str, e),
            }
            builder
                .build()
                .expect("failed to build proxied reqwest client for bing_news")
        })
    }
}

const BN_HEADERS: &[(&str, &str)] = &[(
    "User-Agent",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
)];

#[async_trait]
impl SearchEngine for BingNewsEngine {
    fn name(&self) -> &str {
        "bing_news"
    }

    fn categories(&self) -> &[&str] {
        &["general", "news"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // RSS pages are ~10 items; first= is 1-based item offset.
        let first = (params.pageno.max(1) - 1) * 10 + 1;
        let url = format!(
            "https://www.bing.com/news/search?q={}&format=RSS&first={}",
            urlencoding::encode(query),
            first,
        );

        // Direct first. The CN geo-302 chain lands on an HTML portal page —
        // an HTTP-level success transport errors can't flag. body_ok makes
        // the helper retry through the proxy whenever the direct body isn't
        // RSS (the blocked-target signature for this engine).
        fn is_rss(body: &str) -> bool {
            body.contains("<rss") || body.contains("<item>")
        }
        let body =
            super::get_direct_first_if(&url, BN_HEADERS, Self::proxied_client, is_rss).await?;
        // time_range: the RSS route has no server-side freshness param, but
        // every item carries a pubDate — filter client-side (v0.3.1 Windows
        // report P2-6: intraday agents need "today only").
        let cutoff = params
            .time_range
            .map(|tr| tr.cutoff_epoch(std::time::SystemTime::now()));
        parse_bing_news_rss(&body, cutoff)
    }
}

/// Minimal RSS reader: <item><title/link/description/pubDate>. Flat enough
/// that string scanning beats an XML crate dependency. Items dated before
/// `cutoff` (when set) are dropped; undated items are kept — we don't hide
/// what we can't date.
fn parse_bing_news_rss(
    body: &str,
    cutoff: Option<u64>,
) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    if !body.contains("<rss") && !body.contains("<item>") {
        return Err(SearchEngineError::Transient(
            "bing news response is not RSS".into(),
        ));
    }

    let mut results = Vec::new();
    for item_xml in body.split("<item>").skip(1) {
        let item_xml = match item_xml.split("</item>").next() {
            Some(x) => x,
            None => continue,
        };
        let title = unescape(tag_text(item_xml, "title"));
        let link = tag_text(item_xml, "link");
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let pub_date = tag_text(item_xml, "pubDate");
        if let Some(cutoff) = cutoff {
            if let Some(epoch) = rfc822_epoch(&pub_date) {
                if epoch < cutoff {
                    continue;
                }
            }
        }
        let description = unescape(tag_text(item_xml, "description"));
        // Strip residual HTML tags from the description snippet.
        let description = strip_tags(&description);
        let pub_date = tag_text(item_xml, "pubDate");

        let snippet = if pub_date.is_empty() {
            description
        } else {
            format!("{} ({})", description, pub_date)
        };

        results.push(RawSearchResult {
            title,
            url: link,
            snippet,
            engine: "bing_news".into(),
            score: 0.0,
            cookies: Vec::new(),
            js_extract_result: None,
            image: None,
        });
    }
    let total = results.len().max(1) as f64;
    for (i, r) in results.iter_mut().enumerate() {
        r.score = total - i as f64;
    }
    Ok(results)
}

/// RFC 822 date ("Tue, 25 Aug 2026 08:00:00 GMT" | "+0800") → epoch seconds.
/// Days-from-civil algorithm (Howard Hinnant's), enough for a freshness
/// cutoff without pulling a date crate in.
fn rfc822_epoch(s: &str) -> Option<u64> {
    let s = s.trim();
    // "Tue, 25 Aug 2026 08:00:00 GMT" — drop the weekday prefix if present.
    let body = match s.find(',') {
        Some(i) => s[i + 1..].trim(),
        None => s,
    };
    let parts: Vec<&str> = body.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let month = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[2].parse().ok()?;
    let hms: Vec<u32> = parts[3].split(':').filter_map(|p| p.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    let zone = parts.get(4).copied().unwrap_or("GMT");
    let offset_secs: i64 = if zone == "GMT" || zone == "UTC" || zone == "Z" {
        0
    } else if let Some(stripped) = zone.strip_prefix(['+', '-']) {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let hh: i64 = stripped.get(..2).and_then(|p| p.parse().ok()).unwrap_or(0);
        let mm: i64 = stripped.get(2..).and_then(|p| p.parse().ok()).unwrap_or(0);
        sign * (hh * 3600 + mm * 60)
    } else {
        0 // Unrecognized zone names (EST…): treat as UTC, the cutoff is coarse anyway.
    };

    // Days from civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hms[0] as i64 * 3600 + hms[1] as i64 * 60 + hms[2] as i64;
    Some((secs - offset_secs).max(0) as u64)
}

fn tag_text(xml: &str, tag: &str) -> String {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    match xml.find(&open) {
        Some(start) => {
            let rest = &xml[start + open.len()..];
            match rest.find(&close) {
                Some(end) => rest[..end].trim().to_string(),
                None => String::new(),
            }
        }
        None => {
            // Self-closing / attribute-carrying form: <tag ...>content</tag>
            let open_attr = format!("<{} ", tag);
            match xml.find(&open_attr) {
                Some(start) => {
                    let rest = &xml[start..];
                    match rest.find('>').and_then(|gt| rest[gt + 1..].find(&close).map(|e| gt + 1 + e)) {
                        Some(end) => {
                            let inner_start = rest.find('>').unwrap() + 1;
                            rest[inner_start..end].trim().to_string()
                        }
                        None => String::new(),
                    }
                }
                None => String::new(),
            }
        }
    }
}

fn unescape(s: String) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_bing_news_rss, rfc822_epoch, strip_tags};

    const SAMPLE: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel>
<item>
  <title>Rust 2.0 roadmap published</title>
  <link>https://example.com/rust-roadmap</link>
  <description>The core team &lt;b&gt;outlined&lt;/b&gt; plans for 2026.</description>
  <pubDate>Tue, 25 Aug 2026 08:00:00 GMT</pubDate>
</item>
<item>
  <title>no-link item</title>
  <description>skipped</description>
</item>
</channel></rss>"#;

    #[test]
    fn parses_rss_items_with_clean_snippets() {
        let results = parse_bing_news_rss(SAMPLE, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust 2.0 roadmap published");
        assert_eq!(results[0].url, "https://example.com/rust-roadmap");
        assert_eq!(results[0].snippet, "The core team outlined plans for 2026. (Tue, 25 Aug 2026 08:00:00 GMT)");
        assert_eq!(results[0].score, 1.0);
        assert_eq!(results[0].engine, "bing_news");
    }

    #[test]
    fn non_rss_body_is_transient_error() {
        assert!(parse_bing_news_rss("<html>portal</html>", None).is_err());
    }

    #[test]
    fn rfc822_epoch_matches_known_instants() {
        // 1787644800 = 2026-08-25T08:00:00Z (verified with `date -u -r`).
        assert_eq!(rfc822_epoch("Tue, 25 Aug 2026 08:00:00 GMT"), Some(1_787_644_800));
        assert_eq!(rfc822_epoch("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // Zone offset shifts the instant the other way.
        assert_eq!(
            rfc822_epoch("Tue, 25 Aug 2026 16:00:00 +0800"),
            Some(1_787_644_800)
        );
        // Junk is None, not a wrong date.
        assert_eq!(rfc822_epoch("not a date"), None);
    }

    #[test]
    fn time_range_cutoff_drops_old_keeps_undated() {
        // 1787644800 = 2026-08-25T08:00:00Z, the SAMPLE item's date.
        let results = parse_bing_news_rss(SAMPLE, Some(1_787_644_800)).unwrap();
        assert_eq!(results.len(), 1, "item dated exactly at the cutoff is kept");

        let results = parse_bing_news_rss(SAMPLE, Some(1_787_644_801)).unwrap();
        assert!(results.is_empty(), "item one second past the cutoff is dropped");

        // The no-link item carries no date — an undated item survives any cutoff
        // once it has a link (we don't hide what we can't date).
        let undated = r#"<?xml version="1.0"?><rss version="2.0"><channel>
<item><title>undated</title><link>https://example.com/u</link><description>d</description></item>
</channel></rss>"#;
        let results = parse_bing_news_rss(undated, Some(9_999_999_999)).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn strip_tags_removes_markup() {
        assert_eq!(strip_tags("<b>bold</b> plain"), "bold plain");
    }
}
