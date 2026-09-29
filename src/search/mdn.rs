use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// MDN Web Docs via the site's own v1 search API (free, no key, CN-reachable
/// directly). Code docs ONLY — not the general pool: the API fuzzy-matches
/// CJK queries character-by-character (搜「蔚来」 returns 「注册归因来原」
/// and friends), the same noise class that pulled arxiv out of general
/// (#179/#181). SearXNG ships mdn in "it" — we're stricter because our
/// general pool is default-on.
pub struct MdnEngine {
    client: reqwest::Client,
}

impl MdnEngine {
    pub fn new() -> Self {
        MdnEngine {
            client: super::build_plain_client(10),
        }
    }
}

#[async_trait]
impl SearchEngine for MdnEngine {
    fn name(&self) -> &str {
        "mdn"
    }

    fn categories(&self) -> &[&str] {
        &["code"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let locale = mdn_locale(&params.language);
        let url = search_url(query, &locale, params.pageno);

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
                "HTTP {} from developer.mozilla.org",
                resp.status()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_mdn_json(&body)
    }
}

/// MDN's `page` is 1-BASED and rejects 0 with HTTP 400 — pageno maps through
/// as-is (clamped up), not minus one. This is a measured trap, not a guess.
fn search_url(query: &str, locale: &str, pageno: usize) -> String {
    format!(
        "https://developer.mozilla.org/api/v1/search?q={}&locale={}&page={}",
        urlencoding::encode(query),
        locale,
        pageno.max(1),
    )
}

/// MDN's locale set is a short fixed list (en-US, zh-CN, zh-TW, ja, ko, fr,
/// de, es, it, pt-BR, ru). Map the request language onto it: zh needs the
/// region disambiguated, en needs the default region; other primaries pass
/// through and MDN falls back to en-US server-side for unknown tags.
fn mdn_locale(language: &str) -> String {
    let lower = language.trim().to_lowercase();
    if lower.starts_with("zh-tw") || lower.starts_with("zh-hant") {
        return "zh-TW".into();
    }
    match lower.split('-').next().unwrap_or("") {
        "zh" => "zh-CN".into(),
        "en" | "" => "en-US".into(),
        primary => primary.into(),
    }
}

fn parse_mdn_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let documents = parsed
        .get("documents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let total = documents.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, doc) in documents.iter().enumerate() {
        let title = doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // mdn_url is SITE-RELATIVE (/en-US/docs/…) — prefix the origin.
        let path = doc
            .get("mdn_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() || path.is_empty() {
            continue;
        }
        let url = format!("https://developer.mozilla.org{path}");
        let snippet = doc
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "mdn".into(),
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
    use super::{mdn_locale, parse_mdn_json, search_url};

    // Mirrors a real v1 response (verified direct from CN, 2026-09-28):
    // site-relative mdn_url, summary text, metadata/suggestions alongside.
    const BODY: &str = r#"{
        "documents": [
            {
                "title": "Flexbox",
                "mdn_url": "/en-US/docs/Learn_web_development/Core/CSS_layout/Flexbox",
                "summary": "Flexbox is a one-dimensional layout method for arranging items in rows or columns."
            },
            {
                "title": "CSS",
                "mdn_url": "/en-US/docs/Web/CSS",
                "summary": "Cascading Style Sheets"
            },
            {"title": "", "mdn_url": "", "summary": "no title — skipped"}
        ],
        "metadata": {"took_ms": 12, "total": 320},
        "suggestions": ["flexbox tutorial"]
    }"#;

    #[test]
    fn parses_documents_with_prefixed_url() {
        let results = parse_mdn_json(BODY).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine, "mdn");
        assert_eq!(
            results[0].url,
            "https://developer.mozilla.org/en-US/docs/Learn_web_development/Core/CSS_layout/Flexbox"
        );
        assert!(results[0].snippet.starts_with("Flexbox is a one-dimensional"));
        // Score = N - position over the RAW item count (3 incl. the skipped).
        assert_eq!(results[0].score, 3.0);
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn empty_documents_yields_empty_vec() {
        let results = parse_mdn_json(r#"{"documents": [], "metadata": {"total": 0}}"#).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn page_is_one_based_and_clamps_zero() {
        // page=0 → HTTP 400 from the API (measured); pageno maps through
        // as-is, clamped up, never minus one.
        assert_eq!(
            search_url("flexbox", "en-US", 1),
            "https://developer.mozilla.org/api/v1/search?q=flexbox&locale=en-US&page=1"
        );
        assert!(search_url("flexbox", "en-US", 0).ends_with("page=1"));
        assert!(search_url("flexbox", "en-US", 2).ends_with("page=2"));
    }

    #[test]
    fn locale_maps_regions_for_supported_primaries() {
        assert_eq!(mdn_locale("zh-CN"), "zh-CN");
        assert_eq!(mdn_locale("zh"), "zh-CN");
        assert_eq!(mdn_locale("zh-TW"), "zh-TW");
        assert_eq!(mdn_locale("zh-Hant-HK"), "zh-TW");
        assert_eq!(mdn_locale("en"), "en-US");
        assert_eq!(mdn_locale("en-US"), "en-US");
        assert_eq!(mdn_locale(""), "en-US");
        assert_eq!(mdn_locale("ja"), "ja");
        assert_eq!(mdn_locale("pt-BR"), "pt");
    }
}
