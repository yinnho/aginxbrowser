use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Sogou WeChat search engine. Searches WeChat public account articles.
/// MUST use wreq stealth client — weixin.sogou.com blocks plain reqwest and
/// httpx TLS fingerprints.
pub struct SogouWechatEngine {
    #[cfg(feature = "stealth")]
    stealth: Option<std::sync::Arc<crate::obscura_net::wreq_client::StealthHttpClient>>,
    plain_client: reqwest::Client,
}

impl SogouWechatEngine {
    pub fn new() -> Self {
        #[cfg(feature = "stealth")]
        let stealth = {
            let s = super::build_stealth_client(false); // Sogou is domestic, direct
            Some(s)
        };

        SogouWechatEngine {
            #[cfg(feature = "stealth")]
            stealth,
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
        let url = format!(
            "https://weixin.sogou.com/weixin?type=2&query={}&page={}&ie=utf8",
            urlencoding::encode(query),
            params.pageno,
        );

        // Try stealth client first, fall back to plain reqwest.
        let html;
        #[cfg(feature = "stealth")]
        {
            html = if let Some(ref stealth) = self.stealth {
                match super::stealth_fetch(stealth.as_ref(), &url).await {
                    Ok((text, _)) => text,
                    Err(e) => return Err(e),
                }
            } else {
                super::plain_fetch(&self.plain_client, &url).await?
            };
        }
        #[cfg(not(feature = "stealth"))]
        {
            html = super::plain_fetch(&self.plain_client, &url).await?;
        }

        // Check for CAPTCHA indicators in the HTML body.
        if html.contains("antispider") || html.contains("用户频率限制") {
            return Err(SearchEngineError::Captcha {
                suspend_secs: 3600, // 60 minutes — Sogou WeChat CAPTCHA is hard to pass
            });
        }

        parse_sogou_wechat_html(&html)
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
