use async_trait::async_trait;

use super::{ImageResult, RawSearchResult, SearchEngine, SearchEngineError, SearchParams};

/// Bing Image search. Uses Bing's `images/async` endpoint, which returns HTML
/// fragments where each result anchor (`a.iusc`) carries an embedded JSON `m`
/// attribute with the direct image link (`murl`) and source page (`purl`).
pub struct BingImagesEngine {
    client: reqwest::Client,
}

impl BingImagesEngine {
    pub fn new() -> Self {
        BingImagesEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for BingImagesEngine {
    fn name(&self) -> &str {
        "bing_images"
    }

    fn categories(&self) -> &[&str] {
        &["images"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let count = 35usize;
        let first = (params.pageno.saturating_sub(1)) * count + 1;
        let enc = urlencoding::encode(query);
        let lang = urlencoding::encode(&params.language);
        let url = format!(
            "https://cn.bing.com/images/async?q={enc}&first={first}&count={count}&mmre=1&setlang={lang}"
        );

        let html = super::plain_fetch(&self.client, &url).await?;
        parse_bing_images_html(&html)
    }
}

fn parse_bing_images_html(html: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(html);

    let anchor_selector = scraper::Selector::parse("a.iusc")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let anchors: Vec<_> = document.select(&anchor_selector).collect();
    let total = anchors.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, anchor) in anchors.iter().enumerate() {
        let Some(m) = anchor.value().attr("m") else { continue };

        // The `m` attribute is a JSON string. scraper already unescapes HTML
        // entities, but fall back to manual &quot; replacement in case the
        // attribute was double-encoded in the raw HTML.
        let meta: serde_json::Value = match serde_json::from_str(m) {
            Ok(v) => v,
            Err(_) => match serde_json::from_str(&m.replace("&quot;", "\"")) {
                Ok(v) => v,
                Err(_) => continue,
            },
        };

        let Some(image_url) = meta
            .get("murl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        if !(image_url.starts_with("http://") || image_url.starts_with("https://")) {
            continue;
        }

        let source_url = meta
            .get("purl")
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("http"))
            .map(|s| s.to_string());

        let title = meta
            .get("t")
            .and_then(|v| v.as_str())
            .map(super::html_unescape)
            .unwrap_or_default();

        let width = meta
            .get("mw")
            .and_then(|v| v.as_u64())
            .filter(|&w| w > 0)
            .map(|w| w as u32);
        let height = meta
            .get("mh")
            .and_then(|v| v.as_u64())
            .filter(|&h| h > 0)
            .map(|h| h as u32);

        results.push(RawSearchResult {
            title,
            url: image_url.clone(),
            snippet: String::new(),
            engine: "bing_images".to_string(),
            score: total - i as f64,
            cookies: vec![],
            js_extract_result: None,
            image: Some(ImageResult {
                image_url,
                source_url,
                width,
                height,
            }),
        });
    }

    Ok(results)
}
