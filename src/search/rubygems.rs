use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// RubyGems via the official `/api/v1/search.json` (free, no key). "packages"
/// alongside npm/PyPI — same "find the package for X" agent need, Ruby
/// ecosystem coverage.
///
/// rubygems.org is CN-blocked at transport level (measured 2026-09-29:
/// direct times out, the AGINXBROWSER_PROXY leg answers), so the fetch rides
/// [`super::get_direct_first_if`]: overseas operators connect directly, CN
/// operators' blocked target falls through to their proxy automatically,
/// and the per-host direct-probe memory (aginxos REQ #414) stops re-paying
/// the 4s probe after two failures.
pub struct RubygemsEngine;

impl RubygemsEngine {
    pub fn new() -> Self {
        RubygemsEngine
    }
}

const RG_HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json"),
    (
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
    ),
];

#[async_trait]
impl SearchEngine for RubygemsEngine {
    fn name(&self) -> &str {
        "rubygems"
    }

    fn categories(&self) -> &[&str] {
        &["packages"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // The API takes no offset — it returns the ranked list, and paging
        // is a client-side slice (same shape as huggingface).
        let url = format!(
            "https://rubygems.org/api/v1/search.json?query={}",
            urlencoding::encode(query),
        );
        let _ = params.pageno; // slicing below

        // An error page (HTML) or a geo-substituted body is not a gem array.
        fn is_gem_array(body: &str) -> bool {
            body.trim_start().starts_with('[')
        }
        let body =
            super::get_direct_first_if(&url, RG_HEADERS, super::proxied_plain_client, is_gem_array)
                .await?;
        let skip = params.pageno.saturating_sub(1) * 10;
        parse_rubygems_json(&body, skip, 10)
    }
}

fn parse_rubygems_json(
    body: &str,
    skip: usize,
    take: usize,
) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let gems = parsed
        .as_array()
        .cloned()
        .unwrap_or_default();

    let page: Vec<&serde_json::Value> = gems.iter().skip(skip).take(take).collect();
    let total = page.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, gem) in page.iter().enumerate() {
        let name = gem.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let url = gem
            .get("project_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://rubygems.org/gems/{name}"));
        let version = gem.get("version").and_then(|v| v.as_str()).unwrap_or("-");
        let downloads = gem
            .get("downloads")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let info = gem.get("info").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = format!("v{version} · ↓{downloads} · {info}");

        results.push(RawSearchResult {
            title: name.to_string(),
            url,
            snippet,
            engine: "rubygems".into(),
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
    use super::parse_rubygems_json;

    // Mirrors a real /api/v1/search.json response (verified via proxy leg,
    // 2026-09-29): ranked gems with version/downloads/info/project_uri.
    const BODY: &str = r#"[
        {"name": "rust", "version": "0.16", "downloads": 114481,
         "info": "Ruby advanced statistical library based on RinRuby",
         "project_uri": "https://rubygems.org/gems/rust",
         "homepage_uri": "https://github.com/intersim/ruby-rust"},
        {"name": "rust-ie", "version": "1.0.0", "downloads": 12,
         "info": "", "project_uri": ""},
        {"name": ""}
    ]"#;

    #[test]
    fn parses_gems_with_project_uri_and_signal_snippet() {
        let results = parse_rubygems_json(BODY, 0, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine, "rubygems");
        assert_eq!(results[0].url, "https://rubygems.org/gems/rust");
        assert!(results[0].snippet.contains("v0.16"));
        assert!(results[0].snippet.contains("↓114481"));
        assert!(results[0].snippet.contains("statistical"));
        // Score = N - position over the RAW page count (3 incl. the skipped).
        assert_eq!(results[0].score, 3.0);
    }

    #[test]
    fn missing_project_uri_falls_back_to_gem_page() {
        let results = parse_rubygems_json(BODY, 0, 10).unwrap();
        assert_eq!(results[1].url, "https://rubygems.org/gems/rust-ie");
    }

    #[test]
    fn client_side_paging_slices_by_skip() {
        let one = parse_rubygems_json(BODY, 0, 1).unwrap();
        let two = parse_rubygems_json(BODY, 1, 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].title, "rust");
        assert_eq!(two[0].title, "rust-ie");
        // Beyond the list: empty page, not an error.
        assert!(parse_rubygems_json(BODY, 10, 10).unwrap().is_empty());
    }

    #[test]
    fn non_array_body_is_transient() {
        assert!(parse_rubygems_json("<html>blocked</html>", 0, 10).is_err());
    }
}
