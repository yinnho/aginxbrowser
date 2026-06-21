use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Sogou WeChat search engine. Searches WeChat public account articles.
///
/// Uses plain reqwest for BOTH search and /link redirect resolution.
/// weixin.sogou.com search doesn't check TLS fingerprint (curl works fine).
/// wreq stealth (even with Linux UA) sometimes triggers /link's antispider
/// due to its auto-injected sec-ch-ua/sec-fetch headers. And with macOS UA
/// (needed for WeChat article fetching), the search page itself triggers
/// antispider. Plain reqwest avoids both issues.
pub struct SogouWechatEngine {
    plain_client: reqwest::Client,
}

impl SogouWechatEngine {
    pub fn new() -> Self {
        SogouWechatEngine {
            plain_client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for SogouWechatEngine {
    fn name(&self) -> &str {
        "sogou_wechat"
    }

    fn categories(&self) -> &[&str] {
        &["general", "news"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // Use plain reqwest for the entire search + redirect resolution flow.
        // weixin.sogou.com doesn't check TLS fingerprint (curl works fine),
        // and plain reqwest avoids the Chrome Client Hints headers that
        // wreq stealth auto-injects (which trigger /link's antispider).
        let results = reqwest_search_and_resolve(&self.plain_client, query, params.pageno).await;

        if results.is_empty() {
            // Could be CAPTCHA or network error — return empty rather than
            // erroring so other engines can still contribute results.
            return Ok(Vec::new());
        }

        Ok(results)
    }
}

/// Use plain reqwest
async fn reqwest_search_and_resolve(
    plain_client: &reqwest::Client,
    query: &str,
    pageno: usize,
) -> Vec<RawSearchResult> {
    let search_url = format!(
        "https://weixin.sogou.com/weixin?type=2&query={}&page={}&ie=utf8",
        urlencoding::encode(query),
        pageno,
    );

    // Step 1: Search with plain reqwest, collect cookies.
    let resp = match plain_client.get(&search_url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("sogou_wechat: reqwest search failed: {}", e);
            return Vec::new();
        }
    };

    // Collect cookies from Set-Cookie headers.
    let mut cookies: Vec<String> = Vec::new();
    for val in resp.headers().get_all("set-cookie") {
        if let Ok(s) = val.to_str() {
            if let Some(pair) = s.split(';').next() {
                let trimmed = pair.trim();
                if !trimmed.is_empty() {
                    cookies.push(trimmed.to_string());
                }
            }
        }
    }
    let cookie_header = cookies.join("; ");
    tracing::info!("sogou_wechat: reqwest search cookies: '{}' len={}", cookie_header, cookie_header.len());

    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("sogou_wechat: reqwest search read body failed: {}", e);
            return Vec::new();
        }
    };

    // Check for CAPTCHA.
    if html.contains("antispider") || html.contains("用户频率限制") {
        tracing::warn!("sogou_wechat: reqwest search hit CAPTCHA");
        return Vec::new();
    }

    // Step 2: Parse search results.
    let mut results = match parse_sogou_wechat_html(&html) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("sogou_wechat: reqwest parse failed: {:?}", e);
            return Vec::new();
        }
    };

    if results.is_empty() {
        return results;
    }

    // Step 3: Resolve /link redirect URLs.
    for result in results.iter_mut() {
        if !result.url.contains("weixin.sogou.com/link") {
            continue;
        }

        let mut req = plain_client.get(&result.url)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Referer", &search_url);

        if !cookie_header.is_empty() {
            req = req.header("Cookie", &cookie_header);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_redirection() {
                    if let Some(location) = resp.headers().get("location") {
                        let loc = location.to_str().unwrap_or("");
                        if loc.contains("/antispider") {
                            tracing::debug!("sogou_wechat: /link hit antispider");
                        } else if loc.contains("mp.weixin.qq.com") {
                            tracing::info!("sogou_wechat: resolved {} -> {}", result.url, loc);
                            result.url = loc.to_string();
                        } else if loc.starts_with("http") {
                            // Follow one more hop.
                            if let Ok(next) = plain_client.get(loc)
                                .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
                                .send().await
                            {
                                if next.status().is_redirection() {
                                    if let Some(loc2) = next.headers().get("location") {
                                        let loc2_str = loc2.to_str().unwrap_or("");
                                        if loc2_str.contains("mp.weixin.qq.com") {
                                            tracing::info!("sogou_wechat: resolved {} -> {}", result.url, loc2_str);
                                            result.url = loc2_str.to_string();
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if status.is_success() {
                    // 200 with HTML — might contain meta/JS redirect.
                    if let Ok(body) = resp.text().await {
                        if let Some(real_url) = extract_weixin_url_from_html(&body) {
                            tracing::info!("sogou_wechat: resolved (200) {} -> {}", result.url, real_url);
                            result.url = real_url;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!("sogou_wechat: /link resolve failed: {}", e);
            }
        }
    }

    results
}

/// Extract mp.weixin.qq.com
fn extract_weixin_url_from_html(html: &str) -> Option<String> {
    // Strategy 1: Collect JS string concatenation fragments.
    // Look for patterns like: url += '...';  or url = '...';
    // The variable is named "url" and is built up with += assignments.
    let mut fragments: Vec<String> = Vec::new();
    for line in html.lines() {
        let trimmed = line.trim();
        // Match: url += '...' or url += "..."
        // Also match: url = '...' or url = "..."
        if trimmed.starts_with("url +=") || trimmed.starts_with("url=") {
            let rest = if trimmed.starts_with("url +=") {
                trimmed[6..].trim()
            } else {
                trimmed[4..].trim()
            };
            // Extract the string between quotes.
            if let Some(content) = extract_quoted_string(rest) {
                fragments.push(content);
            }
        }
    }
    if !fragments.is_empty() {
        let full_url: String = fragments.join("");
        if full_url.contains("mp.weixin.qq.com") || full_url.contains("weixin.qq") {
            tracing::debug!("sogou_wechat: reconstructed URL from {} fragments: {}", fragments.len(), full_url);
            return Some(full_url);
        }
    }

    // Strategy 2: Look for meta refresh.
    if let Some(start) = html.find("url=") {
        let rest = &html[start + 4..];
        let end = rest.find(&['"', '\'', ';', ' ']).unwrap_or(rest.len());
        let url = &rest[..end];
        if url.contains("mp.weixin.qq.com") {
            return Some(url.to_string());
        }
    }

    // Strategy 3: Look for location.href or window.location in JS.
    for pattern in &["location.href=\"", "location.href='", "location=\"", "location='", "window.location=\"", "window.location='"] {
        if let Some(start) = html.find(pattern) {
            let rest = &html[start + pattern.len()..];
            let end = rest.find(&['"', '\'']).unwrap_or(rest.len());
            let url = &rest[..end];
            if url.contains("mp.weixin.qq.com") {
                return Some(url.to_string());
            }
        }
    }

    // Strategy 4: Look for any mp.weixin.qq.com or weixin.qq in the HTML
    // (handles cases where the URL is partially split but still visible).
    if let Some(pos) = html.find("weixin.qq") {
        // Walk backwards to find "http".
        let before = &html[..pos];
        let proto_start = before.rfind("http").unwrap_or(0);
        let url_start = &html[proto_start..];
        // Find the end of the URL.
        let end = url_start.find(&['"', '\'', '<', ' ', '\\', '\n']).unwrap_or(url_start.len());
        let url = &url_start[..end];
        if url.starts_with("http") && url.contains("weixin.qq") {
            return Some(url.to_string());
        }
    }

    None
}

/// Extract the content of
fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim_start();
    if s.starts_with('\'') {
        let end = s[1..].find('\'')?;
        Some(s[1..1 + end].to_string())
    } else if s.starts_with('"') {
        let end = s[1..].find('"')?;
        Some(s[1..1 + end].to_string())
    } else {
        None
    }
}

/// Parse Sogou WeChat HTML search results.
fn parse_sogou_wechat_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    // Results are <li> elements with id starting with "sogou_vr_".
    let item_selector = scraper::Selector::parse("li[id^=\"sogou_vr_\"]")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let items: Vec<_> = document.select(&item_selector).collect();
    let total = items.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let title = extract_title(item);
        let url = extract_url(item);
        let snippet = extract_snippet(item);

        if title.is_empty() || url.is_empty() {
            continue;
        }

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "sogou_wechat".to_string(),
            score: total - i as f64,
            cookies: vec![], // Filled in by search() from wreq session.
        });
    }

    Ok(results)
}

fn extract_title(item: &scraper::ElementRef) -> String {
    let selector = match scraper::Selector::parse("h3 a") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    item.select(&selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn extract_url(item: &scraper::ElementRef) -> String {
    let selector = match scraper::Selector::parse("h3 a") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let href = item
        .select(&selector)
        .next()
        .and_then(|el| el.value().attr("href"))
        .unwrap_or("")
        .to_string();

    if href.starts_with("/link?url=") {
        format!("https://weixin.sogou.com{}", href)
    } else {
        href
    }
}

fn extract_snippet(item: &scraper::ElementRef) -> String {
    // Try p.txt-info first.
    let selectors = ["p.txt-info", "p[class^=\"txt-info\"]", "div.txt-box p"];
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
    String::new()
}
