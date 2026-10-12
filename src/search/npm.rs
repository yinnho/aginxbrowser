use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// npm package search via api.npms.io (keyless JSON; SearXNG's npm engine
/// uses the same endpoint). "packages" category source. Snippet packs
/// version/description/author — the signals an agent needs to pick a
/// package without opening npmjs.com.
/// Connectivity: api.npms.io is overseas; fetch rides
/// [`super::get_direct_first`] (proxy-first when available).
pub struct NpmEngine;

impl NpmEngine {
    pub fn new() -> Self {
        NpmEngine
    }
}

#[async_trait]
impl SearchEngine for NpmEngine {
    fn name(&self) -> &str {
        "npm"
    }

    fn categories(&self) -> &[&str] {
        &["general", "packages"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // npms.io paginates with from/size (25 default; keep 10 to match).
        let from = (params.pageno.max(1) - 1) * 10;
        let url = format!(
            "https://api.npms.io/v2/search?q={}&from={}&size=10",
            urlencoding::encode(query),
            from,
        );

        const NPM_HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];
        let body = super::get_direct_first(&url, NPM_HEADERS, super::proxied_plain_client).await?;
        parse_npm_json(&body)
    }
}

fn parse_npm_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let rows = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let total = rows.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let pkg = match row.get("package") {
            Some(p) => p,
            None => continue,
        };
        let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let link = pkg
            .get("links")
            .and_then(|l| l.get("npm"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name.is_empty() || link.is_empty() {
            continue;
        }
        let version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("-");
        let description = pkg.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let author = pkg
            .get("author")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");

        let snippet = format!("v{} by {} | {}", version, author, description);

        results.push(RawSearchResult {
            title: name.to_string(),
            url: link.to_string(),
            snippet,
            engine: "npm".into(),
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
    use super::parse_npm_json;

    #[test]
    fn parses_package_rows() {
        let body = r#"{"total": 2, "results": [
            {"package": {
                "name": "axios", "version": "1.7.0",
                "description": "Promise based HTTP client",
                "author": {"name": "matt"},
                "links": {"npm": "https://www.npmjs.com/package/axios"}
             }},
            {"package": {"name": "", "links": {}}}
        ]}"#;
        let results = parse_npm_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "axios");
        assert_eq!(results[0].url, "https://www.npmjs.com/package/axios");
        assert!(results[0].snippet.starts_with("v1.7.0 by matt | "));
        assert_eq!(results[0].score, 2.0);
        assert_eq!(results[0].engine, "npm");
    }

    #[test]
    fn malformed_json_is_transient() {
        assert!(parse_npm_json("bad").is_err());
    }
}
