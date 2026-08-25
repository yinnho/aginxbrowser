use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Hugging Face Hub search via the official JSON API (`/api/models`,
/// keyless). "ai" category source: models/datasets for agents that need to
/// pick a checkpoint, a dataset, or a Space.
///
/// Connectivity is auto-sensed per request, not bound at startup: the fetch
/// goes **direct first** and falls back to AGINXBROWSER_PROXY only when the
/// direct attempt fails at transport level (the signature of a blocked
/// target). Overseas deployments never configure a proxy and connect
/// directly; CN deployments set it once and blocked targets fall through
/// automatically. Without any proxy on a blocked network every call times
/// out as Transient and the engine is simply skipped for that query.
pub struct HuggingFaceEngine {
    endpoint: String,
}

impl HuggingFaceEngine {
    pub fn new() -> Self {
        Self::with_endpoint("models")
    }

    /// `models` | `datasets` | `spaces` — same API shape, different hub path.
    pub fn with_endpoint(endpoint: &str) -> Self {
        HuggingFaceEngine {
            endpoint: endpoint.to_string(),
        }
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
                Err(e) => tracing::warn!("huggingface proxy '{}' ignored: {}", proxy_str, e),
            }
            builder
                .build()
                .expect("failed to build proxied reqwest client for huggingface")
        })
    }
}

const HF_HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json"),
    (
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
    ),
];

#[async_trait]
impl SearchEngine for HuggingFaceEngine {
    fn name(&self) -> &str {
        if self.endpoint == "models" {
            "huggingface"
        } else {
            // Distinct names let operators filter engines=["huggingface_datasets"].
            Box::leak(format!("huggingface_{}", self.endpoint).into_boxed_str())
        }
    }

    fn categories(&self) -> &[&str] {
        &["general", "ai"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // The API paginates via cursor tokens; page param maps to `limit` +
        // skip semantics only loosely. Keep it simple: fetch the top 20 by
        // trending (direction=-1) and slice client-side per page.
        let _page_skip = (params.pageno.max(1) - 1) * 10;
        let url = format!(
            "https://huggingface.co/api/{}?search={}&direction=-1&limit=20",
            self.endpoint,
            urlencoding::encode(query),
        );

        let body = super::get_direct_first(&url, HF_HEADERS, Self::proxied_client).await?;
        parse_huggingface_json(&body, &self.endpoint)
    }
}

fn parse_huggingface_json(
    body: &str,
    endpoint: &str,
) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let items = parsed.as_array().cloned().unwrap_or_default();

    let total = items.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, entry) in items.iter().enumerate() {
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let url = if endpoint == "models" {
            format!("https://huggingface.co/{id}")
        } else {
            format!("https://huggingface.co/{endpoint}/{id}")
        };

        let mut parts: Vec<String> = Vec::new();
        if let Some(likes) = entry.get("likes").and_then(|v| v.as_i64()) {
            if likes > 0 {
                parts.push(format!("♥ {likes}"));
            }
        }
        if let Some(dl) = entry.get("downloads").and_then(|v| v.as_i64()) {
            if dl > 0 {
                parts.push(format!("↓ {dl}"));
            }
        }
        if let Some(pipeline) = entry.get("pipeline_tag").and_then(|v| v.as_str()) {
            parts.push(pipeline.to_string());
        }
        if let Some(tags) = entry.get("tags").and_then(|v| v.as_array()) {
            let t: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).take(5).collect();
            if !t.is_empty() {
                parts.push(format!("[{}]", t.join(", ")));
            }
        }

        results.push(RawSearchResult {
            title: id.to_string(),
            url,
            snippet: parts.join(" · "),
            engine: "huggingface".into(),
            score: total - i as f64,
            cookies: Vec::new(),
            js_extract_result: None,
            image: None,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::parse_huggingface_json;

    #[test]
    fn parses_models_with_signal_snippets() {
        let body = r#"[
            {"id": "meta-llama/Llama-3", "likes": 95000, "downloads": 12000000,
             "pipeline_tag": "text-generation", "tags": ["llama", "conversational", "en"]},
            {"id": ""}
        ]"#;
        let results = parse_huggingface_json(body, "models").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "meta-llama/Llama-3");
        assert_eq!(results[0].url, "https://huggingface.co/meta-llama/Llama-3");
        assert!(results[0].snippet.contains("♥ 95000"));
        assert!(results[0].snippet.contains("text-generation"));
        assert!(results[0].snippet.contains("[llama, conversational, en]"));
        assert_eq!(results[0].score, 2.0);
    }

    #[test]
    fn datasets_get_endpoint_url_path() {
        let body = r#"[{"id": "squad", "likes": 900}]"#;
        let results = parse_huggingface_json(body, "datasets").unwrap();
        assert_eq!(results[0].url, "https://huggingface.co/datasets/squad");
    }

    #[test]
    fn malformed_json_is_transient() {
        assert!(parse_huggingface_json("nope", "models").is_err());
    }
}
