use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// PyPI package search, keyless. The pypi.org `/search` HTML page is behind
/// a JS bot challenge (Fastly) and the legacy RPC search API was disabled in
/// 2021, so ranked fuzzy search isn't available without a key. What IS
/// keyless and stable: the per-package JSON API (`/pypi/{name}/json`).
/// This engine therefore normalizes the query into candidate package names
/// ("http client" → `httpclient`, `http-client`, `http_client`) and probes
/// each — exact/kebab/snake resolution covers the dominant agent need
/// ("find the package for X") even though it's not ranked search.
pub struct PypiEngine {
    client: reqwest::Client,
}

impl PypiEngine {
    pub fn new() -> Self {
        PypiEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for PypiEngine {
    fn name(&self) -> &str {
        "pypi"
    }

    fn categories(&self) -> &[&str] {
        &["general", "packages"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // Normalize the query into candidate package names: "http client"
        // → "httpclient", "http-client", "http_client". Probe each via the
        // keyless per-package JSON API; hits become results.
        let normalized = query.to_lowercase();
        let candidates: Vec<String> = [
            normalized.replace(' ', ""),
            normalized.replace(' ', "-"),
            normalized.replace(' ', "_"),
        ]
        .into_iter()
        .collect();

        let _ = params.pageno; // per-package API has no paging
        let mut results = Vec::new();
        for cand in candidates.iter().take(3) {
            let url = format!("https://pypi.org/pypi/{}/json", cand);
            let resp = match self.client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => return Err(SearchEngineError::Transient(format!("fetch error: {e}"))),
            };
            if !resp.status().is_success() {
                continue;
            }
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    return Err(SearchEngineError::Transient(format!("read error: {e}")))
                }
            };
            if let Ok(mut parsed) = parse_pypi_json(&body) {
                results.append(&mut parsed);
            }
        }
        Ok(results)
    }
}

fn parse_pypi_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let info = match parsed.get("info") {
        Some(i) => i,
        None => return Ok(Vec::new()),
    };

    let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let version = info.get("version").and_then(|v| v.as_str()).unwrap_or("-");
    let summary = info.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let author = info
        .get("author")
        .or_else(|| info.get("maintainer"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("-");
    let home = info
        .get("home_page")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://pypi.org/project/{name}/"));

    Ok(vec![RawSearchResult {
        title: name.to_string(),
        url: home,
        snippet: format!("v{} by {} | {}", version, author, summary),
        engine: "pypi".into(),
        score: 10.0,
        cookies: Vec::new(),
        js_extract_result: None,
        image: None,
    }])
}

#[cfg(test)]
mod tests {
    use super::parse_pypi_json;

    #[test]
    fn parses_package_info() {
        let body = r#"{"info": {
            "name": "requests", "version": "2.32.0",
            "summary": "Python HTTP for Humans.",
            "author": "Kenneth Reitz",
            "home_page": ""
        }}"#;
        let results = parse_pypi_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "requests");
        assert_eq!(results[0].url, "https://pypi.org/project/requests/");
        assert!(results[0].snippet.contains("v2.32.0"));
        assert_eq!(results[0].engine, "pypi");
    }

    #[test]
    fn missing_info_yields_empty() {
        assert!(parse_pypi_json(r#"{"message": "Not Found"}"#).unwrap().is_empty());
    }
}
