use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Hacker News via the Algolia hn.algolia.com API (free, no key, CN-reachable
/// directly). `general` pool: HN results are ranked story links with points
/// and comment counts attached — exactly the evidence class an agent wants
/// for tech queries. SearXNG files hackernews under its "it" vertical; we
/// don't have one, so general is the home.
pub struct HnEngine {
    client: reqwest::Client,
}

impl HnEngine {
    pub fn new() -> Self {
        HnEngine {
            client: super::build_plain_client(10),
        }
    }
}

/// Algolia's `page` is 0-BASED (page=1 is the SECOND page) — pageno maps
/// minus one, clamped at 0. Mirror of bing_news's offset math; the opposite
/// of mdn/openalex's 1-based pass-through.
fn search_url(query: &str, pageno: usize, cutoff_epoch: Option<i64>) -> String {
    let mut url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=10&page={}",
        urlencoding::encode(query),
        pageno.saturating_sub(1),
    );
    if let Some(ts) = cutoff_epoch {
        // > URL-encoded: numericFilters is a list, the operator must survive.
        url.push_str(&format!("&numericFilters=created_at_i%3E{ts}"));
    }
    url
}

/// `time_range` → created_at cutoff, pure half for tests. Windows are
/// 1d/7d/30d/365d — Algolia filters on creation epoch, not recency buckets.
fn cutoff_for(tr: super::SearchTimeRange, now_secs: i64) -> i64 {
    let window = match tr {
        super::SearchTimeRange::Day => 86_400,
        super::SearchTimeRange::Week => 604_800,
        super::SearchTimeRange::Month => 2_592_000,
        super::SearchTimeRange::Year => 31_536_000,
    };
    now_secs - window
}

#[async_trait]
impl SearchEngine for HnEngine {
    fn name(&self) -> &str {
        "hn"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = params.time_range.map(|tr| cutoff_for(tr, now));
        let url = search_url(query, params.pageno, cutoff);

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
            )
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        if !resp.status().is_success() {
            return Err(SearchEngineError::Transient(format!(
                "HTTP {} from hn.algolia.com",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_hn_json(&body)
    }
}

fn parse_hn_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let hits = parsed
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let total = hits.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let title = hit
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() {
            continue;
        }
        // Ask HN / self-posts have no external url — point at the HN thread.
        let url = hit
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let id = hit.get("objectID").and_then(|v| v.as_str()).unwrap_or("");
                format!("https://news.ycombinator.com/item?id={id}")
            });
        let points = hit.get("points").and_then(|v| v.as_i64()).unwrap_or(0);
        let comments = hit
            .get("num_comments")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let date = hit
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .get(..10)
            .unwrap_or("")
            .to_string();
        let author = hit
            .get("author")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string();
        let snippet = format!("{points} points · {comments} comments · {date} · by {author}");

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "hn".into(),
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
    use super::{cutoff_for, parse_hn_json, search_url};
    use crate::search::SearchTimeRange;

    // Mirrors a real Algolia response (verified direct from CN, 2026-09-29):
    // url present for link stories, null-ish for Ask HN; points/num_comments
    // alongside created_at ISO timestamps.
    const BODY: &str = r#"{
        "hits": [
            {"objectID": "18382470", "title": "Why Discord is switching from Go to Rust",
             "url": "https://blog.discord.com/why-discord-is-switching-from-go-to-rust-a190bbca2b1f",
             "points": 1582, "num_comments": 642, "author": "howdyalice",
             "created_at": "2020-02-04T17:30:40Z"},
            {"objectID": "42100001", "title": "Ask HN: What are you building?",
             "url": null, "points": 300, "num_comments": 1200, "author": "dan",
             "created_at": "2026-09-28T06:00:00Z"},
            {"objectID": "x", "title": "", "points": 1}
        ],
        "nbHits": 12345, "page": 0}"#;

    #[test]
    fn parses_stories_with_thread_fallback() {
        let results = parse_hn_json(BODY).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine, "hn");
        assert!(results[0].url.starts_with("https://blog.discord.com/"));
        assert!(results[0].snippet.contains("1582 points"));
        assert!(results[0].snippet.contains("2020-02-04"));
        // null url → HN thread URL; skipped entry keeps scores over raw count.
        assert_eq!(
            results[1].url,
            "https://news.ycombinator.com/item?id=42100001"
        );
        assert_eq!(results[0].score, 3.0);
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn page_is_zero_based_and_clamps() {
        assert!(search_url("rust", 1, None).contains("page=0"));
        assert!(search_url("rust", 0, None).contains("page=0"));
        assert!(search_url("rust", 2, None).contains("page=1"));
    }

    #[test]
    fn cutoff_encodes_operator_and_maps_windows() {
        let url = search_url("rust", 1, Some(1_700_000_000));
        assert!(url.contains("numericFilters=created_at_i%3E1700000000"));
        assert_eq!(cutoff_for(SearchTimeRange::Day, 1_000_000), 1_000_000 - 86_400);
        assert_eq!(cutoff_for(SearchTimeRange::Year, 1_000_000), 1_000_000 - 31_536_000);
    }
}
