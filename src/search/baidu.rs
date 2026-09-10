use async_trait::async_trait;

use super::{strip_tags, SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// The browser headers a plain (non-stealth) client must carry: without a
/// User-Agent the engine is a bot on sight, and baidu walls headerless
/// requests even before the TLS fingerprint question starts.
const HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
    ),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    ),
    ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
];

/// Baidu search engine. Parses the HTML SERP (`/s`).
///
/// The legacy `tn=json` API is wall-dead as of 2026-09: every vantage
/// point we can test from (plain curl, the hosted stealth build, a
/// Windows user box) receives the wappass captcha page for it, while the
/// HTML SERP still serves plain requests with a browser UA. The wall
/// arrives as a 200 whose body is just a captcha link — without the body
/// check below, that parsed to zero results and read as "no hits" while
/// `/engines` showed the engine healthy (v0.3.2 Windows report P1-1).
pub struct BaiduEngine {
    #[cfg(feature = "stealth")]
    stealth: Option<std::sync::Arc<crate::diting_net::wreq_client::StealthHttpClient>>,
    plain_client: reqwest::Client,
}

impl BaiduEngine {
    pub fn new() -> Self {
        #[cfg(feature = "stealth")]
        let stealth = {
            let s = super::build_stealth_client(false); // Baidu direct (domestic)
            Some(s)
        };

        BaiduEngine {
            #[cfg(feature = "stealth")]
            stealth,
            plain_client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for BaiduEngine {
    fn name(&self) -> &str {
        "baidu"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let offset = (params.pageno.saturating_sub(1)) * 10;
        let url = format!(
            "https://www.baidu.com/s?wd={}&rn=10&pn={}&ie=utf-8",
            urlencoding::encode(query),
            offset,
        );

        let html;
        #[cfg(feature = "stealth")]
        {
            html = if let Some(ref stealth) = self.stealth {
                match super::stealth_fetch(stealth.as_ref(), &url).await {
                    Ok((text, _)) => text,
                    Err(e) => return Err(e),
                }
            } else {
                super::plain_fetch_with(&self.plain_client, &url, HEADERS).await?
            };
        }
        #[cfg(not(feature = "stealth"))]
        {
            html = super::plain_fetch_with(&self.plain_client, &url, HEADERS).await?;
        }

        // The 200-body wall variant: the page is nothing but a captcha
        // link (wappass / static/captcha / 百度安全验证). The 302 variant is
        // caught during the fetch itself.
        if looks_walled(&html) {
            return Err(SearchEngineError::Captcha {
                url: url.to_string(),
                captcha_type: crate::captcha::detect_captcha_type(&url, Some(&html)),
            });
        }

        parse_baidu_html(&html)
    }
}

/// Wall fingerprints for Baidu's risk-control pages, matched against a
/// served body. A real SERP never carries these.
fn looks_walled(html: &str) -> bool {
    html.contains("wappass.baidu.com")
        || html.contains("百度安全验证")
        || html.contains("/static/captcha/")
}

/// A result container node: the outer card carries `result` /
/// `c-container` classes; card internals stack their own `c-container`
/// divs.
fn is_result_container_node(node: &scraper::Node) -> bool {
    matches!(node, scraper::node::Node::Element(el)
        if el.classes().any(|c| c == "result" || c == "c-container"))
}

/// Parse Baidu's HTML SERP. Results are `h3 a` headings whose NEAREST
/// enclosing result container is top-level — nearest-container matching
/// keeps nested card internals (which carry their own headings) out,
/// however the card orders them. The real destination URL sits in the
/// container's `mu` attribute; the `h3 a` href is Baidu's `/link?url=`
/// redirect wrapper.
fn parse_baidu_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let link_sel = scraper::Selector::parse("h3 a")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let mut results: Vec<RawSearchResult> = Vec::new();
    for link in document.select(&link_sel) {
        let title: String = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let Some(container_node) =
            link.ancestors().find(|a| is_result_container_node(a.value()))
        else {
            continue;
        };
        // Card internals are c-container divs nested inside the outer card —
        // only the outermost container is a result.
        if container_node.ancestors().any(|a| is_result_container_node(a.value())) {
            continue;
        }
        let Some(container) = scraper::ElementRef::wrap(container_node) else {
            continue;
        };
        let url = container
            .value()
            .attr("mu")
            .filter(|u| u.starts_with("http"))
            .or_else(|| link.value().attr("href").filter(|h| !h.is_empty()))
            .map(str::to_string);
        let Some(url) = url else {
            continue;
        };
        // One card can carry a second heading (related links); the mu URL
        // is the card's identity, so keep the first.
        if results.iter().any(|r| r.url == url) {
            continue;
        }

        results.push(RawSearchResult {
            title,
            url,
            snippet: baidu_snippet(&container),
            engine: "baidu".to_string(),
            score: 0.0, // Assigned by position below.
            cookies: vec![],
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

/// Snippet for one result card. The current "cosc" cards server-render
/// the summary as `span.summary-text_<hash>` — one span per line — which
/// is the cheapest source (no JSON involved). The classic layout serves
/// `div.c-abstract`. The s-data comment blob is a salvage path only:
/// Baidu's embedded JSON carries invalid `\-` escapes (their own bug —
/// CSS custom properties like `--bottom-gap` land in a style string as
/// `-\-bottom-gap`), so the escapes are repaired before serde sees it.
fn baidu_snippet(item: &scraper::ElementRef) -> String {
    if let Ok(sel) = scraper::Selector::parse("span[class*='summary-text']") {
        let parts: Vec<String> = item
            .select(&sel)
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }

    if let Ok(sel) = scraper::Selector::parse("div.c-abstract") {
        if let Some(el) = item.select(&sel).next() {
            let t: String = el.text().collect();
            if !t.trim().is_empty() {
                return t.trim().to_string();
            }
        }
    }

    let html = item.html();
    let Some(start) = html.find("s-data:") else {
        return String::new();
    };
    let rest = &html[start + "s-data:".len()..];
    let Some(end) = rest.find("-->") else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&repair_json_escapes(&rest[..end]))
    else {
        return String::new();
    };

    let mut parts: Vec<String> = Vec::new();
    if let Some(lines) = v.pointer("/summaryData/generalLines").and_then(|l| l.as_array()) {
        for line in lines {
            if let Some(datas) = line.get("data").and_then(|d| d.as_array()) {
                for d in datas {
                    if let Some(t) = d.get("text").and_then(|t| t.as_str()) {
                        let t = strip_tags(t);
                        if !t.is_empty() {
                            parts.push(t);
                        }
                    }
                }
            }
        }
    }
    parts.join(" ")
}

/// Repair invalid escape sequences in Baidu's embedded s-data JSON:
/// escape a backslash that isn't part of a legal JSON escape (`\"`
/// `\\` `\/` `\b` `\f` `\n` `\r` `\t` `\uXXXX`). Observed live: CSS
/// custom properties inside style strings arrive as `-\-bottom-gap`,
/// which strict parsers reject whole-blob.
fn repair_json_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let next = bytes.get(i + 1).copied();
            let legal = matches!(next,
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') | Some(b'u'));
            out.push('\\');
            if !legal {
                out.push('\\'); // double it: a literal backslash was intended
            }
            if let Some(c) = next {
                out.push(c as char);
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal SERP mixing the live shapes (captured 2026-09-10): a cosc
    /// card whose summary is server-rendered as hash-suffixed
    /// `summary-text` spans next to a s-data comment with Baidu's invalid
    /// `\-` escapes, a nested c-container that must be skipped, a classic
    /// c-abstract card, and a container without an h3.
    const SERP: &str = r#"<html><body><div id="content">
        <div class="result c-container new-pmd" mu="https://example.com/real">
            <div class="c-container inner"><h3><a href="http://www.baidu.com/link?url=xyz">nested must not appear</a></h3></div>
            <h3><a href="http://www.baidu.com/link?url=abc">First title</a></h3>
            <span class="summary-text_15QGa c-color-text">first summary line</span>
            <span class="summary-text_15QGa c-color-text">second line</span>
            <div data-sanssr-cmpt="card/www-summary"><!--s-data:{"summaryData":{"generalLines":[{"prefixTime":"2天前","data":[{"text":"<em>First</em> snippet line"}]}]},"style":"-\-bottom-gap: .03rem;"}--></div>
        </div>
        <div class="result c-container" mu="https://example.com/classic">
            <h3><a href="http://www.baidu.com/link?url=def">Classic card</a></h3>
            <div class="c-abstract">classic abstract text</div>
        </div>
        <div class="c-container" mu="https://example.com/no-h3"><span>no heading here</span></div>
    </div></body></html>"#;

    #[test]
    fn parses_mu_urls_and_sdata_snippets() {
        let rs = parse_baidu_html(SERP).unwrap();
        assert_eq!(rs.len(), 2, "outer cards only; nested + no-h3 skipped: {rs:?}");
        assert_eq!(rs[0].title, "First title");
        assert_eq!(rs[0].url, "https://example.com/real");
        // The rendered summary-text spans win over the s-data blob.
        assert_eq!(rs[0].snippet, "first summary line second line");
        assert_eq!(rs[1].url, "https://example.com/classic");
        assert_eq!(rs[1].snippet, "classic abstract text");
        assert!(rs.iter().all(|r| r.engine == "baidu"));
        assert!(rs[0].score > rs[1].score, "position-ranked");
    }

    /// The s-data salvage path: no rendered spans, no c-abstract, and the
    /// blob carries Baidu's invalid `\-` escape — repaired, the JSON
    /// parses and yields the summary line.
    #[test]
    fn sdata_salvage_survives_invalid_escapes() {
        let doc = r#"<html><body><div class="result c-container" mu="https://example.com/x">
            <h3><a href="http://www.baidu.com/link?url=1">Salvage card</a></h3>
            <div data-sanssr-cmpt="card/www-summary"><!--s-data:{"summaryData":{"generalLines":[{"prefixTime":"2天前","data":[{"text":"<em>salvaged</em> text"}]}]},"style":"-\-bottom-gap: .03rem;"}--></div>
        </div></body></html>"#;
        let rs = parse_baidu_html(doc).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].snippet, "salvaged text");
    }

    #[test]
    fn repair_doubles_only_illegal_backslashes() {
        assert_eq!(repair_json_escapes(r#"-\-b"#), r#"-\\-b"#);
        assert_eq!(repair_json_escapes(r#"{"a":"b\n"}"#), r#"{"a":"b\n"}"#);
        assert_eq!(repair_json_escapes(r#"中 \q"#), r#"中 \\q"#);
    }

    #[test]
    fn wall_bodies_are_flagged() {
        // Shape captured live 2026-09-10: a 200 whose body is a captcha link.
        let wall = r#"<a href="https://wappass.baidu.com/static/captcha/tuxing_v2.html?&amp;logid=1">continue</a>"#;
        assert!(looks_walled(wall));
        assert!(looks_walled("百度安全验证"));
        assert!(!looks_walled(SERP));
    }

    /// Live-shape check against a real captured SERP: set
    /// `AGINXBROWSER_BAIDU_SERP_FIXTURE=/path/to/s.html` (saved from a
    /// browser session — curl gets TLS-fingerprint-walled) and the parser
    /// runs against the real 1MB page instead of the hand-built fixture.
    /// Read-only env gate: unset (CI) the test is a no-op. The wall being
    /// intermittent, this is the honest way to re-verify markup drift
    /// without hammering baidu from a benched IP.
    #[test]
    fn parses_a_live_captured_serp_when_provided() {
        let Ok(path) = std::env::var("AGINXBROWSER_BAIDU_SERP_FIXTURE") else {
            return;
        };
        let html = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        let rs = parse_baidu_html(&html).unwrap();
        assert!(!rs.is_empty(), "real SERP must yield results");
        let with_snippet = rs.iter().filter(|r| !r.snippet.is_empty()).count();
        assert!(
            with_snippet >= rs.len() / 2,
            "at least half the cards carry a snippet (summary-text spans), got {with_snippet}/{}",
            rs.len()
        );
        assert!(
            rs.iter().any(|r| r.url.starts_with("https://") && !r.url.contains("baidu.com/link")),
            "mu attribute must surface real destination URLs, got {:?}",
            rs.iter().map(|r| r.url.as_str()).take(5).collect::<Vec<_>>()
        );
    }
}
