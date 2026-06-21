use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Google search engine. Requires proxy for access from China.
///
/// Uses plain reqwest (NOT wreq stealth) because Google returns server-rendered
/// HTML only for GSA (Google Search App) User-Agent requests, and wreq's Chrome
/// emulation auto-injects sec-ch-ua/sec-fetch headers that conflict with the
/// GSA UA (Google detects the mismatch → JS-only page). Plain reqwest with
/// GSA UA + shuffled TLS ciphers (courtesy of the proxy's stunnel) works.
pub struct GoogleEngine {
    client: reqwest::Client,
}

impl GoogleEngine {
    pub fn new() -> Self {
        // Google needs proxy; build reqwest client with SOCKS5h proxy.
        let proxy_url = std::env::var("OBSCURA_PROXY").ok();
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none());

        if let Some(proxy) = proxy_url {
            // Must use socks5h (remote DNS) — China's DNS for google.com is poisoned.
            let proxy_str = if proxy.starts_with("socks5://") && !proxy.starts_with("socks5h://") {
                format!("socks5h{}", &proxy[7..])
            } else {
                proxy
            };
            match reqwest::Proxy::all(&proxy_str) {
                Ok(p) => builder = builder.proxy(p),
                Err(e) => tracing::warn!("google proxy '{}' ignored: {}", proxy_str, e),
            }
        }

        GoogleEngine {
            client: builder.build().expect("failed to build reqwest client for google"),
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
        let lr = format!("lang_{}", lang);
        let cr = format!("country{}", lang.to_uppercase());
        let url = format!(
            "https://www.google.com/search?q={}&num=10&start={}&hl={}-US&lr={}&cr={}&ie=utf8&oe=utf8&filter=0",
            urlencoding::encode(query),
            start,
            lang,
            lr,
            cr,
        );

        // Use GSA (Google Search App) User-Agent — Google returns server-rendered
        // HTML for GSA requests. The "NSTNWV" suffix is critical (SearXNG trick).
        let resp = self.client.get(&url)
            .header("User-Agent", "Mozilla/5.0 (Linux; Android 6.0; Nexus 5 Build/MRA58N) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/39.0.9869.1911 Mobile Safari/537.36 NSTNWV")
            .header("Accept", "*/*")
            .header("Accept-Encoding", "gzip, deflate")
            .header("Accept-Language", "en,en-US;q=0.7,en;q=0.3")
            .header("Cache-Control", "no-cache")
            .header("DNT", "1")
            .header("Cookie", "CONSENT=YES+")
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        let status = resp.status();
        if status.is_redirection() {
            if let Some(location) = resp.headers().get("location") {
                let loc = location.to_str().unwrap_or("");
                if loc.contains("sorry.google.com") || loc.contains("/sorry/") {
                    return Err(SearchEngineError::Captcha { suspend_secs: 1800 });
                }
            }
            return Err(SearchEngineError::Transient(format!("redirect: {}", resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?"))));
        }

        let html = resp.text().await
            .map_err(|e| SearchEngineError::Transient(format!("read body: {e}")))?;

        // Check for CAPTCHA in body
        if html.contains("/sorry/") || html.contains("unusual traffic") {
            return Err(SearchEngineError::Captcha { suspend_secs: 1800 });
        }

        parse_google_html(&html)
    }
}

/// Parse Google's HTML search results using SearXNG's approach:
/// Find `<a data-ved>` anchors, extract title from inner `<div style>`,
/// URL from href, snippet from specific class.
fn parse_google_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let mut results = Vec::new();

    // SearXNG's approach: select <a> tags with data-ved and no class.
    // These are the result links.
    let result_selector = scraper::Selector::parse("a[data-ved]")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    for anchor in document.select(&result_selector) {
        // Skip anchors with class (navigation, etc.).
        if anchor.value().attr("class").is_some() {
            continue;
        }

        // Extract title from inner div[@style] (SearXNG's approach).
        let title = extract_google_title_from_anchor(&anchor);
        if title.is_none() {
            continue;
        }
        let title = title.unwrap();
        if title.is_empty() {
            continue;
        }

        // Extract URL from href.
        let raw_url = anchor.value().attr("href").unwrap_or("").to_string();
        let url = unwrap_google_url(&raw_url);
        if url.is_empty() || url.starts_with("https://www.google.com/") || url.starts_with("/search?") {
            continue;
        }

        // Extract snippet: look in parent/grandparent div for text content.
        let snippet = extract_google_snippet_from_anchor(&anchor);

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "google".to_string(),
            score: 0.0, // assigned below
        });
    }

    // If the SearXNG approach didn't work, try fallback: div.g containers.
    if results.is_empty() {
        if let Ok(sel) = scraper::Selector::parse("div.g") {
            for item in document.select(&sel) {
                if let Some(r) = parse_google_result_fallback(&item) {
                    results.push(r);
                }
            }
        }
    }

    let total = results.len().max(1) as f64;
    for (i, r) in results.iter_mut().enumerate() {
        r.score = total - i as f64;
    }

    tracing::info!("google: parsed {} results from HTML (len={})", results.len(), html.len());

    tracing::info!("google: parsed {} results from HTML (len={})", results.len(), html.len());

    Ok(results)
}

/// Extract title from a Google result anchor using SearXNG's approach:
/// look for a div[@style] child (the title container).
fn extract_google_title_from_anchor(anchor: &scraper::ElementRef) -> Option<String> {
    if let Ok(sel) = scraper::Selector::parse("div[style]") {
        if let Some(div) = anchor.select(&sel).next() {
            let text: String = div.text().collect();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    // Fallback: h3 inside the anchor.
    if let Ok(sel) = scraper::Selector::parse("h3") {
        if let Some(h3) = anchor.select(&sel).next() {
            let text: String = h3.text().collect();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Extract snippet from the area around a Google result anchor.
fn extract_google_snippet_from_anchor(anchor: &scraper::ElementRef) -> String {
    // SearXNG looks in ../../div[contains(@class, "ilUpNd")]
    // Try parent -> parent -> snippet divs.
    // Since scraper doesn't support parent traversal easily, try looking
    // for known snippet class names in the anchor's ancestors.
    // For simplicity, scan sibling and nearby elements.
    let selectors = [
        "div.VwiC3b",
        "span.aCOSRe",
        "div.IsZvec",
        "div[data-sncf]",
    ];

    for sel_str in &selectors {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            // Try within anchor first.
            if let Some(el) = anchor.select(&sel).next() {
                let text: String = el.text().collect();
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
        }
    }

    String::new()
}

/// Unwrap Google redirect URLs.
fn unwrap_google_url(raw: &str) -> String {
    if raw.starts_with("/url?q=") {
        let encoded = &raw[7..]; // Skip "/url?q="
        let end = encoded.find("&sa=").unwrap_or_else(|| encoded.find('&').unwrap_or(encoded.len()));
        let raw_url = &encoded[..end];
        if let Ok(decoded) = urlencoding::decode(raw_url) {
            return decoded.to_string();
        }
        return raw_url.to_string();
    }
    if raw.starts_with("http") {
        return raw.to_string();
    }
    String::new()
}

/// Fallback parser: try div.g containers when the SearXNG approach fails.
fn parse_google_result_fallback(item: &scraper::ElementRef) -> Option<RawSearchResult> {
    let title = {
        if let Ok(sel) = scraper::Selector::parse("h3") {
            if let Some(h3) = item.select(&sel).next() {
                let text: String = h3.text().collect();
                text.trim().to_string()
            } else {
                return None;
            }
        } else {
            return None;
        }
    };

    let url = {
        if let Ok(sel) = scraper::Selector::parse("a[href]") {
            let mut found = None;
            for a in item.select(&sel) {
                let href = a.value().attr("href").unwrap_or("");
                let u = unwrap_google_url(href);
                if !u.is_empty() && !u.starts_with("https://www.google.com/") && !u.starts_with("/search?") {
                    found = Some(u);
                    break;
                }
            }
            found?
        } else {
            return None;
        }
    };

    if title.is_empty() {
        return None;
    }

    let snippet = {
        let selectors = ["div.VwiC3b", "span.aCOSRe", "div.IsZvec", "div[data-sncf]"];
        let mut s = String::new();
        for sel_str in &selectors {
            if let Ok(sel) = scraper::Selector::parse(sel_str) {
                if let Some(el) = item.select(&sel).next() {
                    let text: String = el.text().collect();
                    if !text.trim().is_empty() {
                        s = text.trim().to_string();
                        break;
                    }
                }
            }
        }
        s
    };

    Some(RawSearchResult {
        title,
        url,
        snippet,
        engine: "google".to_string(),
        score: 0.0,
    })
}
