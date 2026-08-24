use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Stack Overflow via the Stack Exchange API v2.3 (free, no API key).
/// SearXNG-parity "code" category source: programming Q&A with score and
/// answered-state in the snippet, exactly what an agent needs to judge
/// result quality before fetching.
pub struct StackExchangeEngine {
    client: reqwest::Client,
}

impl StackExchangeEngine {
    pub fn new() -> Self {
        StackExchangeEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for StackExchangeEngine {
    fn name(&self) -> &str {
        "stackexchange"
    }

    fn categories(&self) -> &[&str] {
        &["general", "code"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let page = params.pageno.max(1);
        let url = format!(
            "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={}&site=stackoverflow&page={}&pagesize=10&filter=!nNPvSNVZJS",
            urlencoding::encode(query),
            page,
        );

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            // The API 403s the default reqwest UA; a normal client UA passes.
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        if !resp.status().is_success() {
            return Err(SearchEngineError::Transient(format!(
                "HTTP {} from api.stackexchange.com",
                resp.status()
            )));
        }

        // The API answers gzip-compressed JSON; reqwest decodes transparently.
        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_stack_exchange_json(&body)
    }
}

fn parse_stack_exchange_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
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
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let link = item
            .get("link")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() || link.is_empty() {
            continue;
        }

        // Snippet packs the judgment signals: tags, author, answered state,
        // score. The API doesn't return the answer body on search (would need
        // a second round-trip per question), so this is the token-cheap shape.
        let tags: Vec<&str> = item
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        let owner = item
            .get("owner")
            .and_then(|o| o.get("display_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let is_answered = item
            .get("is_answered")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let score = item
            .get("score")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let answer_count = item
            .get("answer_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let snippet = format!(
            "[{}] by {} | {} | score: {} | answers: {}",
            tags.join(", "),
            owner,
            if is_answered { "answered" } else { "unanswered" },
            score,
            answer_count,
        );

        results.push(RawSearchResult {
            title,
            url: link,
            snippet,
            engine: "stackexchange".into(),
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
    use super::parse_stack_exchange_json;

    #[test]
    fn parses_items_with_judgment_signals() {
        let body = r#"{
            "items": [
                {
                    "title": "How to await a tokio task?",
                    "link": "https://stackoverflow.com/q/123",
                    "tags": ["rust", "tokio"],
                    "owner": {"display_name": "alice"},
                    "is_answered": true,
                    "score": 42,
                    "answer_count": 3
                },
                {
                    "title": "",
                    "link": ""
                }
            ],
            "has_more": true
        }"#;
        let results = parse_stack_exchange_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].engine, "stackexchange");
        assert!(results[0].snippet.contains("[rust, tokio]"));
        assert!(results[0].snippet.contains("answered"));
        assert!(results[0].snippet.contains("score: 42"));
        // Score = N - position over the RAW item count (2 items incl. the
        // skipped one), matching the other engines' scoring base.
        assert_eq!(results[0].score, 2.0);
    }

    #[test]
    fn empty_items_yields_empty_vec() {
        let results = parse_stack_exchange_json(r#"{"items": []}"#).unwrap();
        assert!(results.is_empty());
    }
}
