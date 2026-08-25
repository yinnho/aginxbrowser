use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// DuckDuckGo via its server-rendered HTML endpoint (`html.duckduckgo.com/html/`)
/// — the scrape-friendly surface that keeps working while google.com serves
/// JS shells to every user-agent (the GSA trick our google.rs relied on died
/// in Aug 2026). "general" category source.
///
/// Connectivity: html.duckduckgo.com is unreachable from CN networks — direct
/// first, then falls back through AGINXBROWSER_PROXY when configured (see
/// `get_direct_first_if`).
pub struct DuckDuckGoEngine;

impl DuckDuckGoEngine {
    pub fn new() -> Self {
        DuckDuckGoEngine
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
                Err(e) => tracing::warn!("duckduckgo proxy '{}' ignored: {}", proxy_str, e),
            }
            builder
                .build()
                .expect("failed to build proxied reqwest client for duckduckgo")
        })
    }
}

const DDG_HEADERS: &[(&str, &str)] = &[(
    "User-Agent",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
)];

#[async_trait]
impl SearchEngine for DuckDuckGoEngine {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let page = params.pageno.max(1);
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}&page={}",
            urlencoding::encode(query),
            page,
        );

        // body_ok = page contains result anchors; a challenge/portal page
        // (anomaly detection) fails this and drives the proxy retry.
        fn has_results(body: &str) -> bool {
            body.contains("result__a")
        }
        let body = super::get_direct_first_if(&url, DDG_HEADERS, Self::proxied_client, has_results)
            .await?;
        if body.contains("anomaly") && !body.contains("result__a") {
            return Err(SearchEngineError::Captcha {
                url: url.clone(),
                captcha_type: Some(crate::captcha::CaptchaType::Unknown),
            });
        }
        parse_ddg_html(&body)
    }
}

fn parse_ddg_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let link_sel = scraper::Selector::parse("a.result__a")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    // Snippets live in a sibling <a class="result__snippet"> within the same
    // result wrapper; walk up from the link and query inside it.
    let snippet_sel = scraper::Selector::parse("a.result__snippet")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let mut results: Vec<RawSearchResult> = Vec::new();
    for anchor in document.select(&link_sel) {
        let title: String = anchor.text().collect();
        let raw_href = anchor.value().attr("href").unwrap_or("");

        // Links come as //duckduckgo.com/l/?uddg=<urlencoded real url>&rut=...
        let url = unwrap_ddg_redirect(raw_href);
        if title.trim().is_empty() || url.is_empty() {
            continue;
        }

        // Snippet: nearest following result__snippet whose uddg target
        // matches ours (DDG repeats the redirect href on the snippet link).
        let want = extract_uddg(raw_href);
        let mut snippet = String::new();
        for sib in document.select(&snippet_sel) {
            if extract_uddg(sib.value().attr("href").unwrap_or("")) == want && !want.is_empty() {
                snippet = sib.text().collect::<Vec<_>>().join(" ");
                break;
            }
        }

        results.push(RawSearchResult {
            title: title.trim().to_string(),
            url,
            snippet,
            engine: "duckduckgo".into(),
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

/// Extract the real target from a DDG redirect href:
/// `//duckduckgo.com/l/?uddg=<percent-encoded>&rut=...` → decoded URL.
/// Returns "" for non-redirect hrefs (ads etc.).
fn extract_uddg(href: &str) -> String {
    let marker = "uddg=";
    let start = match href.find(marker) {
        Some(i) => i + marker.len(),
        None => return String::new(),
    };
    let rest = &href[start..];
    let end = rest.find('&').unwrap_or(rest.len());
    urlencoding::decode(&rest[..end]).unwrap_or_default().into_owned()
}

fn unwrap_ddg_redirect(href: &str) -> String {
    if !href.contains("uddg=") {
        // Not a redirect wrapper — use as-is (normalize protocol-relative).
        if let Some(rest) = href.strip_prefix("//") {
            return format!("https://{}", rest);
        }
        return href.to_string();
    }
    // Redirect wrapper with an empty target is a junk result.
    extract_uddg(href)
}

#[cfg(test)]
mod tests {
    use super::{extract_uddg, parse_ddg_html, unwrap_ddg_redirect};

    const SAMPLE: &str = r#"<html><body>
<div class="result">
  <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=aaa">Rust Programming Language</a>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=aaa">A language empowering everyone to build reliable software.</a>
</div>
<div class="result">
  <a class="result__a" href="https://direct.example.com/page">Direct link no redirect</a>
</div>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=&amp;rut=bbb">empty target skipped</a>
</div>
</body></html>"#;

    #[test]
    fn parses_results_unwrapping_redirects() {
        let results = parse_ddg_html(SAMPLE).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert!(results[0].snippet.contains("empowering everyone"));
        assert_eq!(results[0].score, 2.0);
        // Direct (non-redirect) href passes through.
        assert_eq!(results[1].url, "https://direct.example.com/page");
        assert_eq!(results[1].engine, "duckduckgo");
    }

    #[test]
    fn extract_uddg_decodes_percent_encoding() {
        assert_eq!(extract_uddg("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.b%2Fc&rut=x"), "https://a.b/c");
        assert_eq!(extract_uddg("https://plain.example.com/x"), "");
    }

    #[test]
    fn unwrap_passthrough_protocol_relative() {
        assert_eq!(unwrap_ddg_redirect("//example.com/x"), "https://example.com/x");
    }

    /// Firecrawl #4375 parity: a malformed percent escape in one result URL
    /// must not poison the page. `urlencoding::decode` is lossy (never
    /// throws), so the bad URL survives as-is and siblings still parse.
    #[test]
    fn malformed_percent_escape_does_not_poison_the_page() {
        let body = r#"<html><body>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fok.example%2F&amp;rut=a">Good</a>
</div>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fbad.example%2F%ZZ&rut=b">Bad escape</a>
</div>
</body></html>"#;
        let results = parse_ddg_html(body).unwrap();
        assert_eq!(results.len(), 2);
        // The malformed escape decodes lossily instead of aborting the page.
        assert_eq!(results[0].url, "https://ok.example/");
        assert!(results[1].url.contains("bad.example"));
    }
}
