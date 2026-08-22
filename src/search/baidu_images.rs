use async_trait::async_trait;

use super::{ImageResult, RawSearchResult, SearchEngine, SearchEngineError, SearchParams};

/// Baidu Image search. Uses Baidu's `acjson` JSON image-search endpoint and
/// returns binary direct image links (curl -o downloadable), not page URLs.
pub struct BaiduImagesEngine {
    #[cfg(feature = "stealth")]
    stealth: Option<std::sync::Arc<crate::diting_net::wreq_client::StealthHttpClient>>,
    plain_client: reqwest::Client,
}

impl BaiduImagesEngine {
    pub fn new() -> Self {
        #[cfg(feature = "stealth")]
        let stealth = {
            let s = super::build_stealth_client(false); // image.baidu.com is domestic.
            Some(s)
        };

        BaiduImagesEngine {
            #[cfg(feature = "stealth")]
            stealth,
            plain_client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for BaiduImagesEngine {
    fn name(&self) -> &str {
        "baidu_images"
    }

    fn categories(&self) -> &[&str] {
        &["images"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let rn = 10usize;
        let pn = (params.pageno.saturating_sub(1)) * rn;
        let enc = urlencoding::encode(query);
        let url = format!(
            "https://image.baidu.com/search/acjson?tn=resultjson_com&ipn=rj&fp=result&queryWord={enc}&cl=2&lm=-1&ie=utf-8&oe=utf-8&word={enc}&pn={pn}&rn={rn}&st=-1&face=0&istype=2&nc=1"
        );

        let text;
        #[cfg(feature = "stealth")]
        {
            text = if let Some(ref stealth) = self.stealth {
                match super::stealth_fetch(stealth.as_ref(), &url).await {
                    Ok((t, _)) => t,
                    Err(e) => return Err(e),
                }
            } else {
                super::plain_fetch(&self.plain_client, &url).await?
            };
        }
        #[cfg(not(feature = "stealth"))]
        {
            text = super::plain_fetch(&self.plain_client, &url).await?;
        }

        parse_baidu_images_json(&text)
    }
}

/// Pick the first valid http(s) direct image URL from Baidu's candidates.
/// Baidu returns several URL fields of decreasing quality:
///   objURL    — original full-res source image (best quality; sometimes
///               dead or hotlink-blocked)
///   middleURL — Baidu CDN medium-res proxy (reliable)
///   thumbURL  — Baidu CDN thumbnail proxy (most reliable, low res)
///   hoverURL  — hover preview
/// We prefer the original but fall back to the CDN proxies so callers always
/// get a working direct link when one exists.
fn pick_image_url(item: &serde_json::Value) -> Option<String> {
    for key in ["objURL", "middleURL", "thumbURL", "hoverURL"] {
        if let Some(u) = item.get(key).and_then(|v| v.as_str()) {
            if (u.starts_with("http://") || u.starts_with("https://"))
                && !u.contains("baidu.com/link")
            {
                return Some(u.to_string());
            }
        }
    }
    None
}

fn parse_baidu_images_json(text: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let fixed = text.replace("\\/", "/");
    let json: serde_json::Value = serde_json::from_str(&fixed)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;

    let data = json.get("data").and_then(|d| d.as_array());
    let Some(data) = data else {
        return Ok(Vec::new());
    };

    let total = data.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, item) in data.iter().enumerate() {
        let Some(image_url) = pick_image_url(item) else {
            continue;
        };

        let source_url = item
            .get("fromURL")
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("http"))
            .map(|s| s.to_string());

        let title = item
            .get("fromPageTitle")
            .or_else(|| item.get("fromPageTitleEnc"))
            .and_then(|v| v.as_str())
            .map(super::html_unescape)
            .unwrap_or_default();

        let width = item
            .get("width")
            .and_then(|v| v.as_u64())
            .filter(|&w| w > 0)
            .map(|w| w as u32);
        let height = item
            .get("height")
            .and_then(|v| v.as_u64())
            .filter(|&h| h > 0)
            .map(|h| h as u32);

        results.push(RawSearchResult {
            title,
            url: image_url.clone(),
            snippet: String::new(),
            engine: "baidu_images".to_string(),
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
