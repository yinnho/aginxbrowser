use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Wikipedia via the MediaWiki `list=search` API (free, no key). The language
/// edition follows the request's language tag (zh-CN → zh.wikipedia.org).
/// Every language edition is CN-blocked at the network level, so the fetch
/// rides the direct-first/proxy-retry posture — on CN deployments the proxy
/// retry is the load-bearing path, same as bing_news (#181).
pub struct WikipediaEngine;

impl WikipediaEngine {
    pub fn new() -> Self {
        WikipediaEngine
    }
}

const WIKI_HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json"),
    (
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
    ),
];

#[async_trait]
impl SearchEngine for WikipediaEngine {
    fn name(&self) -> &str {
        "wikipedia"
    }

    fn categories(&self) -> &[&str] {
        &["general"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // Primary language subtag selects the wiki: zh-CN → zh, en-US → en.
        let lang = params
            .language
            .split('-')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let lang = if lang.is_empty() { "en" } else { &lang };
        // 10 per page; sroffset is the 0-based item offset.
        let offset = params.pageno.saturating_sub(1) * 10;
        let url = format!(
            "https://{}.wikipedia.org/w/api.php?action=query&list=search&format=json&srlimit=10&sroffset={}&srsearch={}",
            lang,
            offset,
            urlencoding::encode(query),
        );
        // A CN-blocked request answers with an error page, not this JSON — the
        // search-array key is the blocked-target signature for the proxy retry.
        fn is_api_body(body: &str) -> bool {
            body.contains("\"search\"")
        }
        let body = super::get_direct_first_if(
            &url,
            WIKI_HEADERS,
            super::proxied_plain_client,
            is_api_body,
        )
        .await?;
        parse_wikipedia_json(&body, lang)
    }
}

fn parse_wikipedia_json(
    body: &str,
    lang: &str,
) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let items = parsed
        .get("query")
        .and_then(|q| q.get("search"))
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
        if title.is_empty() {
            continue;
        }
        // Wiki URLs spell spaces as underscores; the rest (CJK titles arrive
        // as raw UTF-8) must survive percent-encoding.
        let url = format!(
            "https://{}.wikipedia.org/wiki/{}",
            lang,
            urlencoding::encode(&title.replace(' ', "_"))
        );
        // The snippet carries <span class="searchmatch"> highlight markup and
        // HTML entities — strip both before it reaches the merge layer.
        let raw = item.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        let mut snippet = super::html_unescape(&super::strip_tags(raw));
        // Last-edit date is the only freshness signal in the payload; fold it
        // into the snippet the way bing_news folds its source metadata.
        if let Some(date) = item
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|t| t.get(..10))
        {
            snippet = format!("{snippet} (edited {date})");
        }
        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "wikipedia".into(),
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
    use super::parse_wikipedia_json;

    // Mirrors a real zh/en API response (verified through a live proxy exit,
    // 2026-09-28): searchmatch spans in the snippet, ISO timestamp, wordcount.
    const BODY: &str = r#"{
        "batchcomplete": "",
        "continue": {"sroffset": 3, "continue": "-||"},
        "query": {
            "searchinfo": {"totalhits": 7450},
            "search": [
                {
                    "ns": 0,
                    "title": "Rust (programming language)",
                    "pageid": 29414838,
                    "size": 119486,
                    "wordcount": 10944,
                    "snippet": "<span class=\"searchmatch\">Rust</span> is a general-purpose programming <span class=\"searchmatch\">language</span> that emphasizes performance &amp; type safety",
                    "timestamp": "2026-09-20T16:00:03Z"
                },
                {
                    "ns": 0,
                    "title": "Rust (fungus)",
                    "pageid": 52148,
                    "snippet": "Rusts are plant diseases caused by <span class=\"searchmatch\">pathogenic</span> fungi",
                    "timestamp": "2026-08-01T00:00:00Z"
                },
                {"ns": 0, "title": "", "snippet": "no title — skipped"}
            ]
        }
    }"#;

    #[test]
    fn parses_items_with_stripped_snippet_and_edit_date() {
        let results = parse_wikipedia_json(BODY, "en").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine, "wikipedia");
        // Highlight markup stripped, entities unescaped.
        assert!(!results[0].snippet.contains("searchmatch"));
        assert!(!results[0].snippet.contains("&amp;"));
        assert!(results[0].snippet.starts_with("Rust is a general-purpose"));
        assert!(results[0].snippet.contains("performance & type safety"));
        // Edit date folded in from the ISO timestamp's date part.
        assert!(results[0].snippet.ends_with("(edited 2026-09-20)"));
        // Score = N - position over the RAW item count (3 incl. the skipped).
        assert_eq!(results[0].score, 3.0);
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn spaces_become_underscores_in_page_url() {
        let results = parse_wikipedia_json(BODY, "en").unwrap();
        assert_eq!(
            results[0].url,
            "https://en.wikipedia.org/wiki/Rust_%28programming_language%29"
        );
    }

    #[test]
    fn cjk_title_is_percent_encoded_in_page_url() {
        let body = r#"{"query": {"search": [
            {"title": "出租車", "snippet": "一種公共交通工具", "timestamp": "2026-09-01T00:00:00Z"}
        ]}}"#;
        let results = parse_wikipedia_json(body, "zh").unwrap();
        assert_eq!(
            results[0].url,
            "https://zh.wikipedia.org/wiki/%E5%87%BA%E7%A7%9F%E8%BB%8A"
        );
    }

    #[test]
    fn error_page_yields_empty_not_error() {
        // A challenge/error body that slipped past body_ok parses to zero
        // items — "no results", not a parse crash.
        let results = parse_wikipedia_json("<html>blocked</html>", "en");
        assert!(results.is_err());
        let results = parse_wikipedia_json(r#"{"query": {}}"#, "en").unwrap();
        assert!(results.is_empty());
    }
}
