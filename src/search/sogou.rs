use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Sogou general search engine.
///
/// Uses plain reqwest (NOT wreq stealth) because wreq's Chrome emulation
/// auto-injects sec-ch-ua/sec-fetch headers that trigger sogou's antispider
/// (302 → /antispider). Plain reqwest with a standard browser UA works fine —
/// even curl with no headers gets 200 from sogou.
pub struct SogouEngine {
    client: reqwest::Client,
}

impl SogouEngine {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest client for sogou");

        SogouEngine { client }
    }
}

#[async_trait]
impl SearchEngine for SogouEngine {
    fn name(&self) -> &str {
        "sogou"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let url = format!(
            "https://www.sogou.com/web?query={}&page={}&ie=utf8",
            urlencoding::encode(query),
            params.pageno,
        );

        let resp = self.client.get(&url)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        let status = resp.status();
        if status.is_redirection() {
            if let Some(location) = resp.headers().get("location") {
                let loc = location.to_str().unwrap_or("");
                if loc.contains("/antispider") {
                    return Err(SearchEngineError::Captcha {
                        url: url.to_string(),
                        captcha_type: Some(crate::captcha::CaptchaType::SliderCaptcha),
                    });
                }
            }
            return Err(SearchEngineError::Transient(format!("redirect: {}", resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?"))));
        }

        let bytes = resp.bytes().await
            .map_err(|e| SearchEngineError::Transient(format!("read body: {e}")))?;

        // Decode with charset detection (Sogou may return GBK).
        let html = crate::diting_net::encoding::decode_non_html(&bytes.to_vec(), None);

        // Check for CAPTCHA indicators in the HTML body.
        if html.contains("/antispider") || html.contains("用户频率限制") {
            tracing::warn!("sogou: CAPTCHA detected in HTML body (len={})", html.len());
            return Err(SearchEngineError::Captcha {
                url: url.to_string(),
                captcha_type: Some(crate::captcha::CaptchaType::SliderCaptcha),
            });
        }

        parse_sogou_html(&html)
    }
}

/// Parse Sogou's HTML search results.
fn parse_sogou_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let rb_selector = scraper::Selector::parse("div.rb")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    let vrwrap_selector = scraper::Selector::parse("div.vrwrap")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let mut results = Vec::new();
    let mut position = 0usize;

    // Type 1: Standard results (div.rb)
    for item in document.select(&rb_selector) {
        if let Some(r) = parse_sogou_standard_item(&item, &mut position) {
            results.push(r);
        }
    }

    // Type 2: Rich results (div.vrwrap)
    for item in document.select(&vrwrap_selector) {
        if let Some(r) = parse_sogou_vrwrap_item(&item, &mut position) {
            results.push(r);
        }
    }

    let total = results.len().max(1) as f64;
    for (i, r) in results.iter_mut().enumerate() {
        r.score = total - i as f64;
    }

    Ok(results)
}

fn parse_sogou_standard_item(
    item: &scraper::ElementRef,
    position: &mut usize,
) -> Option<RawSearchResult> {
    let h3_selector = scraper::Selector::parse("h3.pt a").ok()?;
    let ft_selector = scraper::Selector::parse("div.ft").ok()?;

    let link_el = item.select(&h3_selector).next()?;
    let title: String = link_el.text().collect();
    let raw_url = link_el.value().attr("href").unwrap_or("").to_string();

    if title.is_empty() || raw_url.is_empty() {
        return None;
    }

    let url = resolve_sogou_url(&raw_url, &item.html());
    let snippet = item
        .select(&ft_selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();

    *position += 1;
    Some(RawSearchResult {
        title,
        url,
        snippet,
        engine: "sogou".to_string(),
        score: 0.0, // Will be assigned by position later.
        cookies: vec![],
        js_extract_result: None,
        image: None,
    })
}

fn parse_sogou_vrwrap_item(
    item: &scraper::ElementRef,
    position: &mut usize,
) -> Option<RawSearchResult> {
    // Try vr-title class first, then fallback to generic h3 a.
    let h3_sel = scraper::Selector::parse("h3.vr-title a")
        .ok()
        .or_else(|| scraper::Selector::parse("h3 a").ok())?;

    let link_el = item.select(&h3_sel).next()?;
    let title: String = link_el.text().collect();
    let raw_url = link_el.value().attr("href").unwrap_or("").to_string();

    if title.is_empty() || raw_url.is_empty() {
        return None;
    }

    let url = resolve_sogou_url(&raw_url, &item.html());
    let snippet = extract_sogou_snippet(item);

    *position += 1;
    Some(RawSearchResult {
        title,
        url,
        snippet,
        engine: "sogou".to_string(),
        score: 0.0,
        cookies: vec![],
        js_extract_result: None,
        image: None,
    })
}

fn extract_sogou_snippet(item: &scraper::ElementRef) -> String {
    // Try attribute-centent first, then fz-mid space-txt.
    let selectors = [
        "div.attribute-centent",
        "div.fz-mid.space-txt",
        "div.str-text-info",
        "p",
    ];

    for sel_str in &selectors {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            if let Some(el) = item.select(&sel).next() {
                let text: String = el.text().collect();
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
        }
    }

    String::new()
}

/// Resolve Sogou redirect URLs. Sogou wraps external URLs as
/// /link?url=... and may also embed the real URL in a data-url attribute.
fn resolve_sogou_url(raw: &str, item_html: &str) -> String {
    if !raw.starts_with("/link?url=") {
        return raw.to_string();
    }

    // Try to extract the real URL from data-url attribute.
    if let Some(start) = item_html.find("data-url=\"") {
        let rest = &item_html[start + 10..];
        if let Some(end) = rest.find('"') {
            let url = &rest[..end];
            if url.starts_with("http") {
                return url.to_string();
            }
        }
    }

    // Fallback: prefix with sogou base.
    format!("https://www.sogou.com{}", raw)
}
