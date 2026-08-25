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
            let mut builder = reqwest::Client::builder()
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
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
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
        parse_bing_news_rss(&body)
    }
}

/// Minimal RSS reader: <item><title/link/description/pubDate>. Flat enough
/// that string scanning beats an XML crate dependency.
fn parse_bing_news_rss(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
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
    use super::{parse_bing_news_rss, strip_tags};

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
        let results = parse_bing_news_rss(SAMPLE).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust 2.0 roadmap published");
        assert_eq!(results[0].url, "https://example.com/rust-roadmap");
        assert_eq!(results[0].snippet, "The core team outlined plans for 2026. (Tue, 25 Aug 2026 08:00:00 GMT)");
        assert_eq!(results[0].score, 1.0);
        assert_eq!(results[0].engine, "bing_news");
    }

    #[test]
    fn non_rss_body_is_transient_error() {
        assert!(parse_bing_news_rss("<html>portal</html>").is_err());
    }

    #[test]
    fn strip_tags_removes_markup() {
        assert_eq!(strip_tags("<b>bold</b> plain"), "bold plain");
    }
}
