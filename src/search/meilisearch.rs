use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Meilisearch adapter (SearXNG-parity): query the operator's own
/// self-hosted Meilisearch instance as a search source — private corpora,
/// crawled archives, internal KBs — alongside the web engines.
///
/// Configured entirely through environment variables; when unset the engine
/// simply isn't registered, so stock deployments see no behavior change:
/// - `AGINXBROWSER_MEILI_URL`    e.g. `http://127.0.0.1:7700`
/// - `AGINXBROWSER_MEILI_INDEX`  index uid to query
/// - `AGINXBROWSER_MEILI_KEY`    optional Bearer key
///
/// Hits are returned as key-value snippets (SearXNG's KeyValue result shape):
/// agents get every indexed field without us hardcoding a schema.
pub struct MeilisearchEngine {
    client: reqwest::Client,
    base_url: String,
    index: String,
    auth_key: Option<String>,
}

impl MeilisearchEngine {
    /// Returns None when AGINXBROWSER_MEILI_URL / _INDEX are not configured.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("AGINXBROWSER_MEILI_URL").ok()?;
        let index = std::env::var("AGINXBROWSER_MEILI_INDEX").ok()?;
        if base_url.is_empty() || index.is_empty() {
            return None;
        }
        Some(MeilisearchEngine {
            client: super::build_plain_client(10),
            base_url: base_url.trim_end_matches('/').to_string(),
            index,
            auth_key: std::env::var("AGINXBROWSER_MEILI_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
        })
    }
}

#[async_trait]
impl SearchEngine for MeilisearchEngine {
    fn name(&self) -> &str {
        "meilisearch"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let offset = (params.pageno.max(1) - 1) * 10;
        let url = format!("{}/indexes/{}/search", self.base_url, self.index);

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json");
        if let Some(key) = &self.auth_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        let resp = req
            .body(
                serde_json::json!({
                    "q": query,
                    "offset": offset,
                    "limit": 10,
                })
                .to_string(),
            )
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        if !resp.status().is_success() {
            return Err(SearchEngineError::Transient(format!(
                "HTTP {} from meilisearch",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_meili_json(&body)
    }
}

fn parse_meili_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let hits = parsed
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Document URL field: prefer conventional keys, else skip results with
    // nothing linkable (they'd be dead weight in an agent's context).
    const URL_KEYS: &[&str] = &["url", "link", "href"];

    let total = hits.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let obj = match hit.as_object() {
            Some(o) => o,
            None => continue,
        };
        let url = URL_KEYS
            .iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let title = obj
            .get("title")
            .or_else(|| obj.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(url)
            .to_string();

        // Snippet = all fields flattened key:value — SearXNG KeyValue shape.
        let mut pairs: Vec<String> = Vec::with_capacity(obj.len());
        for (k, v) in obj {
            if k == "url" || k == "title" || k == "name" {
                continue;
            }
            let vs = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if vs.is_empty() {
                continue;
            }
            pairs.push(format!("{}: {}", k, vs));
        }

        results.push(RawSearchResult {
            title,
            url: url.to_string(),
            snippet: pairs.join(" | "),
            engine: "meilisearch".into(),
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
    use super::parse_meili_json;

    #[test]
    fn flattens_hits_to_keyvalue_snippets() {
        let body = r#"{
            "hits": [
                {"url": "https://internal/doc/1", "title": "Runbook", "owner": "sre", "updated": "2026-08-01"},
                {"title": "no url here"}
            ],
            "query": "runbook"
        }"#;
        let results = parse_meili_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://internal/doc/1");
        assert_eq!(results[0].title, "Runbook");
        assert!(results[0].snippet.contains("owner: sre"));
        assert!(results[0].snippet.contains("updated: 2026-08-01"));
        assert!(!results[0].snippet.contains("url:"));
        assert_eq!(results[0].engine, "meilisearch");
    }

    #[test]
    fn empty_hits_yields_empty_vec() {
        let results = parse_meili_json(r#"{"hits": []}"#).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn malformed_json_is_transient_error() {
        assert!(parse_meili_json("nope").is_err());
    }
}
