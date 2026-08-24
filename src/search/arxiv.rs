use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// arXiv via the export.arxiv.org Atom API (free, no key). "academic"
/// category source. The API caps page size at 100 and paginates by
/// startIndex; we request 10 to match the other engines.
pub struct ArxivEngine {
    client: reqwest::Client,
}

impl ArxivEngine {
    pub fn new() -> Self {
        ArxivEngine {
            client: super::build_plain_client(15),
        }
    }
}

#[async_trait]
impl SearchEngine for ArxivEngine {
    fn name(&self) -> &str {
        "arxiv"
    }

    fn categories(&self) -> &[&str] {
        &["general", "academic"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        let start = (params.pageno.max(1) - 1) * 10;
        // https: the http:// endpoint 301s to it and our search clients don't
        // follow redirects (redirect policy none, for CAPTCHA detection).
        let url = format!(
            "https://export.arxiv.org/api/query?search_query=all:{}&start={}&max_results=10&sortBy=relevance",
            urlencoding::encode(query),
            start,
        );

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/atom+xml")
            .send()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("fetch error: {e}")))?;

        if !resp.status().is_success() {
            return Err(SearchEngineError::Transient(format!(
                "HTTP {} from export.arxiv.org",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SearchEngineError::Transient(format!("read error: {e}")))?;
        parse_arxiv_atom(&body)
    }
}

/// Minimal Atom reader: entries are flat (no nesting beyond one level of
/// link/category), so a small hand-rolled extractor beats pulling in a
/// full XML crate dependency.
fn parse_arxiv_atom(body: &str) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let mut results = Vec::new();
    for entry in body.split("<entry>").skip(1) {
        let entry = match entry.split("</entry>").next() {
            Some(e) => e,
            None => continue,
        };

        let title = unescape_xml(tag_content(entry, "title"));
        // arXiv abstracts embed newlines; flatten for the snippet shape.
        let summary = unescape_xml(tag_content(entry, "summary"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let id_url = tag_content(entry, "id");
        if title.is_empty() || id_url.is_empty() {
            continue;
        }

        // Authors are repeated <name> elements.
        let authors: Vec<String> = entry
            .split("<author>")
            .skip(1)
            .map(|a| unescape_xml(tag_content(a.split("</author>").next().unwrap_or(""), "name")))
            .filter(|n| !n.is_empty())
            .collect();

        // Categories are repeated <category term="..."/> attributes.
        let categories: Vec<&str> = split_categories(entry);
        let snippet = format!(
            "{} | [{}] | {} ({})",
            authors.join(", "),
            categories.join(", "),
            summary,
            tag_content(entry, "published"),
        );

        results.push(RawSearchResult {
            title,
            url: id_url,
            snippet,
            engine: "arxiv".into(),
            score: 0.0,
            cookies: Vec::new(),
            js_extract_result: None,
            image: None,
        });
    }
    // Score after collection so positions are stable regardless of skips.
    let total = results.len().max(1) as f64;
    for (i, r) in results.iter_mut().enumerate() {
        r.score = total - i as f64;
    }
    Ok(results)
}

fn tag_content<'a>(entry: &'a str, tag: &str) -> String {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    match entry.find(&open) {
        Some(start) => {
            let rest = &entry[start + open.len()..];
            match rest.find(&close) {
                Some(end) => rest[..end].trim().to_string(),
                None => String::new(),
            }
        }
        None => String::new(),
    }
}

fn split_categories(entry: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = entry;
    while let Some(pos) = rest.find("<category term=\"") {
        rest = &rest[pos + "<category term=\"".len()..];
        if let Some(end) = rest.find('"') {
            out.push(&rest[..end]);
            rest = &rest[end..];
        } else {
            break;
        }
    }
    out
}

fn unescape_xml(s: String) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::{parse_arxiv_atom, unescape_xml};

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <updated>2024-01-01T00:00:00Z</updated>
    <published>2024-01-01T00:00:00Z</published>
    <title>A Study of &amp; Agents on the Web</title>
    <summary>Abstract with
      newlines   and spaces.</summary>
    <author><name>Ada Lovelace</name></author>
    <author><name>Alan Turing</name></author>
    <category term="cs.AI" />
    <category term="cs.CL" />
  </entry>
  <entry>
    <id></id>
    <title>missing id skipped</title>
  </entry>
</feed>"#;

    #[test]
    fn parses_entries_with_authors_and_categories() {
        let results = parse_arxiv_atom(SAMPLE).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A Study of & Agents on the Web");
        assert_eq!(results[0].url, "http://arxiv.org/abs/2401.00001v1");
        assert!(results[0].snippet.starts_with("Ada Lovelace, Alan Turing | [cs.AI, cs.CL] | "));
        assert!(results[0].snippet.contains("(2024-01-01T00:00:00Z)"));
        // Newlines flattened.
        assert!(!results[0].snippet.contains('\n'));
        assert_eq!(results[0].score, 1.0);
        assert_eq!(results[0].engine, "arxiv");
    }

    #[test]
    fn unescape_handles_entity_order() {
        assert_eq!(unescape_xml("a &amp;lt; b".into()), "a &lt; b");
        assert_eq!(unescape_xml("&amp;".into()), "&");
    }
}
