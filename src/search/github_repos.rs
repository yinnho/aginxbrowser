use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// GitHub repository search via api.github.com (no key required; unauthenticated
/// quota is 10 requests/minute per IP — rate-limit responses surface as
/// Transient so the caller's retry/backoff handles them without CAPTCHA-style
/// suspension). "code" category source: stars/description/language give an
/// agent the quality signals to pick a repo without fetching it.
pub struct GithubEngine {
    client: reqwest::Client,
}

impl GithubEngine {
    pub fn new() -> Self {
        GithubEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for GithubEngine {
    fn name(&self) -> &str {
        "github"
    }

    fn categories(&self) -> &[&str] {
        &["general", "code"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // GitHub paginates from 1, 30 items default; keep 10 to match the
        // other engines' result density and score scale.
        let page = params.pageno.max(1);
        let url = format!(
            "https://api.github.com/search/repositories?q={}&page={}&per_page=10",
            urlencoding::encode(query),
            page,
        );

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            // The API requires a User-Agent; the plain client's UA is generic
            // but present — set an identifying one per GitHub guidelines.
            .header("User-Agent", "aginxbrowser-search")
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        if resp.status() == reqwest::StatusCode::FORBIDDEN || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SearchEngineError::Transient(
                "github api rate limited (unauthenticated quota 10/min)".into(),
            ));
        }
        if !resp.status().is_success() {
            return Err(SearchEngineError::Transient(format!(
                "HTTP {} from api.github.com",
                resp.status()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_github_json(&body)
    }
}

fn parse_github_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let items = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let total = items.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let full_name = item
            .get("full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let html_url = item
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if full_name.is_empty() || html_url.is_empty() {
            continue;
        }
        let title = full_name.clone();
        let description = item
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let language = item.get("language").and_then(|v| v.as_str()).unwrap_or("-");
        let stars = item.get("stargazers_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let updated = item.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");

        let snippet = format!(
            "{} | ⭐ {} | lang: {} | updated: {}",
            description, stars, language, updated
        );

        results.push(RawSearchResult {
            title,
            url: html_url,
            snippet,
            engine: "github".into(),
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
    use super::parse_github_json;

    #[test]
    fn parses_repo_signals_into_snippet() {
        let body = r#"{
            "total_count": 2,
            "items": [
                {
                    "full_name": "tokio-rs/tokio",
                    "html_url": "https://github.com/tokio-rs/tokio",
                    "description": "A runtime for writing reliable asynchronous applications",
                    "stargazers_count": 28000,
                    "language": "Rust",
                    "updated_at": "2026-08-20T00:00:00Z"
                },
                {"full_name": "", "html_url": ""}
            ]
        }"#;
        let results = parse_github_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "tokio-rs/tokio");
        assert!(results[0].snippet.contains("⭐ 28000"));
        assert!(results[0].snippet.contains("lang: Rust"));
        assert_eq!(results[0].engine, "github");
    }

    #[test]
    fn malformed_json_is_transient_error() {
        assert!(parse_github_json("not-json").is_err());
    }
}
