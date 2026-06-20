use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Baidu search engine. Uses Baidu's JSON API endpoint (tn=json).
pub struct BaiduEngine {
    #[cfg(feature = "stealth")]
    stealth: Option<std::sync::Arc<crate::obscura_net::wreq_client::StealthHttpClient>>,
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
            "https://www.baidu.com/s?wd={}&tn=json&rn=10&pn={}&ie=utf-8",
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
                super::plain_fetch(&self.plain_client, &url).await?
            };
        }
        #[cfg(not(feature = "stealth"))]
        {
            html = super::plain_fetch(&self.plain_client, &url).await?;
        }

        parse_baidu_json(&html)
    }
}

/// Parse Baidu's JSON search results.
fn parse_baidu_json(text: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    // Baidu's JSON sometimes has escape issues; fix common ones.
    let fixed = text.replace("\\/", "/").replace("\\'", "'");

    let json: serde_json::Value =
        serde_json::from_str(&fixed).map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;

    let entries = json
        .get("feed")
        .and_then(|f| f.get("entry"))
        .and_then(|e| e.as_array());

    let Some(entries) = entries else {
        // No results or non-standard response.
        return Ok(Vec::new());
    };

    let total = entries.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        let title = html_unescape(
            entry
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let url = entry
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let snippet = html_unescape(
            entry
                .get("abs")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );

        if title.is_empty() || url.is_empty() {
            continue;
        }

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "baidu".to_string(),
            score: total - i as f64,
        });
    }

    Ok(results)
}

/// Unescape common HTML entities in Baidu's JSON output.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
