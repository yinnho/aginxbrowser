use async_trait::async_trait;

use base64::Engine;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Bing search engine. Uses plain reqwest (Bing does not block standard TLS).
pub struct BingEngine {
    client: reqwest::Client,
}

impl BingEngine {
    pub fn new() -> Self {
        BingEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for BingEngine {
    fn name(&self) -> &str {
        "bing"
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
            "https://www.bing.com/search?q={}&count=10&offset={}&setlang={}",
            urlencoding::encode(query),
            offset,
            urlencoding::encode(&params.language),
        );

        let html = super::plain_fetch(&self.client, &url).await?;
        parse_bing_html(&html)
    }
}

/// Parse Bing's HTML search results.
fn parse_bing_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let item_selector = scraper::Selector::parse("ol#b_results li.b_algo")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    let link_selector = scraper::Selector::parse("h2 a")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    let snippet_selector = scraper::Selector::parse("p")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let items: Vec<_> = document.select(&item_selector).collect();
    let total = items.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let link_el = match item.select(&link_selector).next() {
            Some(el) => el,
            None => continue,
        };

        let title: String = link_el.text().collect();
        let raw_url = link_el.value().attr("href").unwrap_or("").to_string();

        if title.is_empty() || raw_url.is_empty() {
            continue;
        }

        // Unwrap Bing redirect URLs.
        let url = unwrap_bing_url(&raw_url);

        // Extract snippet. Remove algoSlug_icon spans first by extracting
        // only the text from <p> children that are not spans.
        let snippet: String = item
            .select(&snippet_selector)
            .next()
            .map(|p| p.text().collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string();

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "bing".to_string(),
            score: total - i as f64,
        });
    }

    Ok(results)
}

/// Decode Bing's redirect URL format.
/// Bing sometimes wraps URLs as: https://www.bing.com/ck/a?u=a1<base64url>&...
/// The `u` parameter, when starting with "a1", contains a base64url-encoded
/// real URL.
fn unwrap_bing_url(raw: &str) -> String {
    if !raw.starts_with("https://www.bing.com/ck/a?") {
        return raw.to_string();
    }

    // Parse the query string to extract the `u` parameter.
    let url = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return raw.to_string(),
    };

    let u_param = url
        .query_pairs()
        .find(|(k, _)| k == "u")
        .map(|(_, v)| v.to_string());

    let Some(u_val) = u_param else {
        return raw.to_string();
    };

    if !u_val.starts_with("a1") {
        return raw.to_string();
    }

    // Base64url decode the part after "a1".
    let b64 = &u_val[2..];
    // Add padding if needed.
    let padded = match b64.len() % 4 {
        2 => format!("{}==", b64),
        3 => format!("{}=", b64),
        _ => b64.to_string(),
    };

    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(padded.trim_end_matches('=')) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(decoded) => {
                // The decoded string may contain a URL directly or may have
                // additional formatting. Try to extract a clean URL.
                if decoded.starts_with("http") {
                    decoded
                } else {
                    raw.to_string()
                }
            }
            Err(_) => raw.to_string(),
        },
        Err(_) => raw.to_string(),
    }
}
