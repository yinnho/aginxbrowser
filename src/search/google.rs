use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Google search engine. Requires proxy for access from China.
/// Uses wreq stealth client with proxy.
pub struct GoogleEngine {
    #[cfg(feature = "stealth")]
    stealth: Option<std::sync::Arc<crate::obscura_net::wreq_client::StealthHttpClient>>,
    plain_client: reqwest::Client,
}

impl GoogleEngine {
    pub fn new() -> Self {
        // Google needs proxy; build stealth client with proxy support.
        #[cfg(feature = "stealth")]
        let stealth = {
            // Only create stealth client if proxy is configured, since Google
            // is unreachable from China without proxy.
            let proxy_url = std::env::var("OBSCURA_PROXY").ok();
            if proxy_url.is_some() {
                let s = super::build_stealth_client(true);
                Some(s)
            } else {
                None
            }
        };

        GoogleEngine {
            #[cfg(feature = "stealth")]
            stealth,
            plain_client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for GoogleEngine {
    fn name(&self) -> &str {
        "google"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let start = (params.pageno.saturating_sub(1)) * 10;
        let lang = params.language.split('-').next().unwrap_or("en");
        let url = format!(
            "https://www.google.com/search?q={}&num=10&start={}&hl={}",
            urlencoding::encode(query),
            start,
            lang,
        );

        // Google requires stealth + proxy. If no stealth client is available,
        // skip (return empty rather than error, so other engines still work).
        #[cfg(feature = "stealth")]
        {
            let html = if let Some(ref stealth) = self.stealth {
                match super::stealth_fetch(stealth.as_ref(), &url).await {
                    Ok((text, _final_url)) => {
                        // Check for Google CAPTCHA/sorry page.
                        if text.contains("/sorry/") || text.contains("unusual traffic") {
                            return Err(SearchEngineError::Captcha { suspend_secs: 1800 });
                        }
                        text
                    }
                    Err(e) => return Err(e),
                }
            } else {
                // No stealth client available (no proxy configured).
                // Google is unreachable without proxy from China.
                tracing::debug!("search: google skipped (no stealth/proxy)");
                return Ok(Vec::new());
            };
            return parse_google_html(&html);
        }

        #[cfg(not(feature = "stealth"))]
        {
            return Ok(Vec::new());
        }
    }
}

/// Parse Google's HTML search results.
/// Google's HTML structure changes frequently; this parser tries multiple
/// selector strategies with fallbacks.
fn parse_google_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let mut results = Vec::new();
    let mut position = 0usize;

    // Strategy 1: Modern Google uses div.g containers with data-sokoban.
    if let Ok(sel) = scraper::Selector::parse("div.g") {
        for item in document.select(&sel) {
            if let Some(r) = parse_google_result(&item, &mut position) {
                results.push(r);
            }
        }
    }

    // If no results found, try alternative selectors.
    if results.is_empty() {
        if let Ok(sel) = scraper::Selector::parse("div[data-hveid]") {
            for item in document.select(&sel) {
                if let Some(r) = parse_google_result(&item, &mut position) {
                    results.push(r);
                }
            }
        }
    }

    let total = results.len().max(1) as f64;
    for (i, r) in results.iter_mut().enumerate() {
        r.score = total - i as f64;
    }

    Ok(results)
}

fn parse_google_result(
    item: &scraper::ElementRef,
    position: &mut usize,
) -> Option<RawSearchResult> {
    // Find the main link: h3 inside an anchor, or a with data-ved.
    let title = extract_google_title(item)?;
    let url = extract_google_url(item)?;

    if title.is_empty() || url.is_empty() {
        return None;
    }

    // Skip Google internal links.
    if url.starts_with("https://www.google.com/") || url.starts_with("/search?") {
        return None;
    }

    let snippet = extract_google_snippet(item);

    *position += 1;
    Some(RawSearchResult {
        title,
        url,
        snippet,
        engine: "google".to_string(),
        score: 0.0,
    })
}

fn extract_google_title(item: &scraper::ElementRef) -> Option<String> {
    // Try h3 first (most reliable).
    if let Ok(sel) = scraper::Selector::parse("h3") {
        if let Some(h3) = item.select(&sel).next() {
            let text: String = h3.text().collect();
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

fn extract_google_url(item: &scraper::ElementRef) -> Option<String> {
    // Find anchor with href that looks like a result link.
    if let Ok(sel) = scraper::Selector::parse("a[href]") {
        for a in item.select(&sel) {
            let href = a.value().attr("href").unwrap_or("");
            if href.starts_with("/url?q=") {
                // Unwrap Google redirect: /url?q=ACTUAL_URL&sa=...
                let encoded = &href[7..]; // Skip "/url?q="
                if let Some(end) = encoded.find("&sa=") {
                    let raw = &encoded[..end];
                    if let Ok(decoded) = urlencoding::decode(raw) {
                        return Some(decoded.to_string());
                    }
                }
                // Fallback: take everything before first &.
                if let Some(end) = encoded.find('&') {
                    let raw = &encoded[..end];
                    if let Ok(decoded) = urlencoding::decode(raw) {
                        return Some(decoded.to_string());
                    }
                }
            } else if href.starts_with("http") && !href.contains("google.com/search") {
                return Some(href.to_string());
            }
        }
    }
    None
}

fn extract_google_snippet(item: &scraper::ElementRef) -> String {
    // Google's snippet container class changes frequently. Try multiple.
    let selectors = [
        "div.VwiC3b",     // Common modern class
        "span.aCOSRe",    // Alternative
        "div[data-sncf]", // Data-attribute based
        "div.IsZvec",     // Another common one
    ];

    for sel_str in &selectors {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            if let Some(el) = item.select(&sel).next() {
                let text: String = el.text().collect();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    // Fallback: any <span> with substantial text.
    if let Ok(sel) = scraper::Selector::parse("span") {
        for el in item.select(&sel) {
            let text: String = el.text().collect();
            if text.trim().len() > 50 {
                return text.trim().to_string();
            }
        }
    }

    String::new()
}
