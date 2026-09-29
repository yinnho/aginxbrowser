use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// OpenAlex via the public api.openalex.org works API (free, no key,
/// CN-reachable directly). "academic" alongside arXiv: OpenAlex is a
/// cross-publisher metadata index (250M+ works, PubMed/DOI coverage) —
/// broader than arXiv's preprints, so academic queries get journal and
/// conference literature too. Not in the general pool (same precision
/// argument as arXiv #179).
///
/// The anonymous search pool occasionally answers HTTP 429 "search cluster
/// under elevated load, retry in 30s" (measured 2026-09-29) — that maps to
/// Transient, i.e. the engine is skipped for that one query, no suspension;
/// arXiv still serves the category.
pub struct OpenalexEngine {
    client: reqwest::Client,
}

impl OpenalexEngine {
    pub fn new() -> Self {
        OpenalexEngine {
            client: super::build_plain_client(10),
        }
    }
}

/// OpenAlex's `page` is 1-BASED (meta echoes `"page": 1` for the first
/// page) — pageno passes through clamped up, like mdn.
fn search_url(query: &str, pageno: usize) -> String {
    format!(
        "https://api.openalex.org/works?search={}&per-page=10&page={}",
        urlencoding::encode(query),
        pageno.max(1),
    )
}

#[async_trait]
impl SearchEngine for OpenalexEngine {
    fn name(&self) -> &str {
        "openalex"
    }

    fn categories(&self) -> &[&str] {
        &["academic"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let url = search_url(query, params.pageno);

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
                "HTTP {} from api.openalex.org",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_openalex_json(&body)
    }
}

/// Rebuild text from OpenAlex's `abstract_inverted_index` — a word →
/// [positions] map instead of a string. Pure half, unit-tested.
fn rebuild_abstract(inv: &serde_json::Value) -> String {
    let Some(map) = inv.as_object() else {
        return String::new();
    };
    let mut slots: Vec<(usize, &str)> = Vec::new();
    for (word, positions) in map {
        if let Some(list) = positions.as_array() {
            for p in list {
                if let Some(idx) = p.as_u64() {
                    slots.push((idx as usize, word.as_str()));
                }
            }
        }
    }
    slots.sort_by_key(|(idx, _)| *idx);
    slots
        .into_iter()
        .map(|(_, w)| w)
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_openalex_json(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchEngineError::Transient(format!("json parse: {e}")))?;
    let works = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let total = works.len().max(1) as f64;
    let mut results = Vec::new();
    for (i, work) in works.iter().enumerate() {
        let title = work
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() {
            continue;
        }
        // Prefer the DOI link (publisher resolvable); fall back to the
        // OpenAlex work page, which carries the same metadata.
        let Some(url) = work
            .get("doi")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                work.get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
        else {
            continue;
        };

        let mut parts: Vec<String> = Vec::new();
        if let Some(year) = work.get("publication_year").and_then(|v| v.as_i64()) {
            parts.push(year.to_string());
        }
        let authorships = work.get("authorships").and_then(|v| v.as_array());
        let names: Vec<&str> = authorships
            .map(|list| {
                list.iter()
                    .filter_map(|a| {
                        a.get("author")
                            .and_then(|author| author.get("display_name"))
                            .and_then(|n| n.as_str())
                    })
                    .collect()
            })
            .unwrap_or_default();
        match names.len() {
            0 => parts.push("-".into()),
            1 => parts.push(names[0].into()),
            _ => {
                if names.len() > 2 {
                    parts.push(format!("{}, {} et al.", names[0], names[1]));
                } else {
                    parts.push(names.join(", "));
                }
            }
        }
        if let Some(venue) = work
            .get("primary_location")
            .and_then(|loc| loc.get("source"))
            .and_then(|s| s.get("display_name"))
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
        {
            parts.push(venue.to_string());
        }
        let abstract_text = work
            .get("abstract_inverted_index")
            .map(rebuild_abstract)
            .unwrap_or_default();
        parts.push(abstract_text.chars().take(200).collect());
        let snippet = parts.join(" | ");

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "openalex".into(),
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
    use super::{parse_openalex_json, rebuild_abstract, search_url};

    // Mirrors a real works response (verified direct from CN, 2026-09-29):
    // doi + openalex id, abstract as inverted index, venue sometimes null.
    const BODY: &str = r#"{
        "meta": {"count": 86019, "page": 1},
        "results": [
            {
                "id": "https://openalex.org/W3034030180",
                "doi": "https://doi.org/10.1145/3385412.3386036",
                "title": "Understanding memory and thread safety practices and issues in real-world Rust programs",
                "publication_year": 2020,
                "authorships": [
                    {"author": {"display_name": "Yiji Zhang"}},
                    {"author": {"display_name": "Sen Yang"}},
                    {"author": {"display_name": "Feng Lyu"}}
                ],
                "primary_location": {"source": null},
                "abstract_inverted_index": {"Rust": [0], "is": [1], "a": [2], "young": [3], "programming": [4]}
            },
            {
                "id": "https://openalex.org/W0000000001",
                "doi": null,
                "title": "A work with no abstract",
                "publication_year": 2024,
                "authorships": [],
                "primary_location": {"source": {"display_name": "Nature"}}
            },
            {"id": "https://openalex.org/Wx", "doi": null, "title": ""}
        ]}"#;

    #[test]
    fn parses_works_with_doi_url_and_inverted_abstract() {
        let results = parse_openalex_json(BODY).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine, "openalex");
        assert_eq!(results[0].url, "https://doi.org/10.1145/3385412.3386036");
        // >2 authorships fold to "first two et al."
        assert!(results[0].snippet.starts_with("2020 | Yiji Zhang, Sen Yang et al. | "));
        assert!(results[0].snippet.contains("Rust is a young programming"));
        assert_eq!(results[0].score, 3.0);
    }

    #[test]
    fn no_doi_falls_back_to_openalex_page_and_venue_shows() {
        let results = parse_openalex_json(BODY).unwrap();
        assert_eq!(results[1].url, "https://openalex.org/W0000000001");
        assert!(results[1].snippet.contains("2024 | - | Nature |"));
    }

    #[test]
    fn page_is_one_based_and_clamps_zero() {
        assert!(search_url("rust", 1).ends_with("page=1"));
        assert!(search_url("rust", 0).ends_with("page=1"));
        assert!(search_url("rust", 3).ends_with("page=3"));
    }

    #[test]
    fn inverted_index_rebuilds_word_order() {
        let inv: serde_json::Value =
            serde_json::from_str(r#"{"world": [1], "hello": [0], "!": [2]}"#).unwrap();
        assert_eq!(rebuild_abstract(&inv), "hello world !");
        // Non-object input degrades to empty.
        assert_eq!(rebuild_abstract(&serde_json::Value::Null), "");
    }
}
