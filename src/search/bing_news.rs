use async_trait::async_trait;

use super::{SearchParams, RawSearchResult, SearchEngine, SearchEngineError};

/// Bing News via the infinite-scroll AJAX fragment — the route SearXNG's
/// bing_news engine has served since it abandoned `format=RSS`. The RSS
/// output is retired: `/news/search?q=…&format=RSS` 302s to the portal
/// from every exit we can observe (#178), and the whole news vertical is
/// geo-walled from CN IPs (even `cn.bing.com/news` 302s to `/`), so the
/// proxy retry below is the load-bearing path on CN deployments.
pub struct BingNewsEngine;

impl BingNewsEngine {
    pub fn new() -> Self {
        BingNewsEngine
    }
}

const BN_HEADERS: &[(&str, &str)] = &[(
    "User-Agent",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
)];

#[async_trait]
impl SearchEngine for BingNewsEngine {
    fn name(&self) -> &str {
        "bing_news"
    }

    fn categories(&self) -> &[&str] {
        &["general", "news"]
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<RawSearchResult>, SearchEngineError> {
        // SearXNG's request shape: 10 items per page, `first` is the 1-based
        // item offset and SFX the 0-based page echo.
        let page = params.pageno.saturating_sub(1);
        let url = format!(
            "https://www.bing.com/news/infinitescrollajax?q={}&InfiniteScroll=1&first={}&SFX={}&form=PTFTNR&setlang={}",
            urlencoding::encode(query),
            page * 10 + 1,
            page,
            urlencoding::encode(&params.language),
        );
        // time_range rides the server-side `qft` freshness window (same
        // mapping as SearXNG): day is "last hour" (interval 4) because Bing's
        // last-day and last-week lists barely differ; there is no year
        // window, month covers it.
        let url = match params.time_range {
            Some(super::SearchTimeRange::Day) => format!("{url}&qft=interval%3D%224%22"),
            Some(super::SearchTimeRange::Week) => format!("{url}&qft=interval%3D%227%22"),
            Some(super::SearchTimeRange::Month | super::SearchTimeRange::Year) => {
                format!("{url}&qft=interval%3D%229%22")
            }
            None => url,
        };

        // The fragment is bare HTML — a page of `div.newsitem` cards, no
        // page chrome. The CN geo-302 lands on the portal home instead, an
        // HTTP-level success transport errors can't flag; body_ok makes the
        // helper retry through the proxy whenever the direct body isn't a
        // news fragment (the blocked-target signature for this engine).
        fn is_fragment(body: &str) -> bool {
            body.contains("newsitem")
        }
        let body =
            super::get_direct_first_if(&url, BN_HEADERS, super::proxied_plain_client, is_fragment).await?;
        parse_bing_news_fragment(&body)
    }
}

/// Parse the infinitescrollajax fragment: `div.newsitem` cards carrying
/// `a.title` (link), `div.snippet` (text), and a source bar whose
/// aria-labelled spans name the publisher and age — folded into the snippet
/// the way the RSS engine folded pubDate.
fn parse_bing_news_fragment(
    body: &str,
) -> Result<Vec<RawSearchResult>, SearchEngineError> {
    let document = scraper::Html::parse_document(body);

    let item_selector = scraper::Selector::parse("div.newsitem")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    let link_selector = scraper::Selector::parse("a.title")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    let snippet_selector = scraper::Selector::parse("div.snippet")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
    // Bing's source bar class varies (source / t_source); substring-match the
    // class attribute the way SearXNG's contains(@class, "source") does.
    let source_selector = scraper::Selector::parse("div[class*='source']")
        .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;

    let items: Vec<_> = document.select(&item_selector).collect();
    let total = items.len().max(1) as f64;
    let mut results = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let link_el = match item.select(&link_selector).next() {
            Some(el) => el,
            None => continue,
        };
        let title: String = link_el.text().collect::<String>().trim().to_string();
        let url = link_el.value().attr("href").unwrap_or("").to_string();
        if title.is_empty() || url.is_empty() {
            continue;
        }

        let snippet: String = item
            .select(&snippet_selector)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        // Source metadata: the first aria-labelled span in the source bar
        // (publisher / age) plus the link's data-author, SearXNG's pair.
        let mut metadata: Vec<String> = Vec::new();
        if let Some(source) = item.select(&source_selector).next() {
            let label_sel = scraper::Selector::parse("span[aria-label]")
                .map_err(|e| SearchEngineError::Transient(format!("selector parse: {e}")))?;
            if let Some(span) = source.select(&label_sel).next() {
                let label = span
                    .value()
                    .attr("aria-label")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !label.is_empty() {
                    metadata.push(label);
                }
            }
        }
        if let Some(author) = link_el.value().attr("data-author") {
            let author = author.trim();
            if !author.is_empty() {
                metadata.push(author.to_string());
            }
        }

        let snippet = if metadata.is_empty() {
            snippet
        } else {
            format!("{} ({})", snippet, metadata.join(" | "))
        };

        results.push(RawSearchResult {
            title,
            url,
            snippet,
            engine: "bing_news".into(),
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
    use super::parse_bing_news_fragment;

    // Mirrors the real infinitescrollajax fragment (verified through a live
    // proxy exit, 2026-09-29): `div.news-card.newsitem.cardcommon` carrying
    // a source bar (publisher + aria-labelled age), then the `a.title`
    // anchor wrapping an <h2>, then `div.snippet`.
    const FRAGMENT: &str = r#"<div class="news-card newsitem cardcommon"
    url="https://www.forbes.com/sites/monicamercuri/2025/05/02/rust-roadmap"
    data-title="Rust 2.0 roadmap published" data-author="Forbes"
    title="Rust 2.0 roadmap published">
  <div class="t_row">
    <div class="source set_top" style="height:16px">
      <div class="caption_img nositelink"><img title="Forbes" width="44" height="16" src="/th?id=OJ.vi0&amp;pid=news" class="pubimg"/></div>
      <div style="margin-top: 2px"><span><span class="news-separator"></span></span><span tabindex="0" aria-label="9/3/2026">25d</span></div>
    </div>
    <a target="_blank" class="title" data-artpy="0" data-author="Polygon on MSN" href="https://www.msn.com/en-us/news/other/rust-roadmap/ar-AA2bxPBh" h="ID=news,5076.1"><h2 class=" ns_hd_h2">Rust 2.0 roadmap published</h2></a>
    <div class="snippet" title="The core team outlined plans for 2026.">The core team outlined plans for 2026.</div>
  </div>
</div>
<div class="news-card newsitem cardcommon">
  <a target="_blank" class="title" data-author="The Register" href="https://example.com/second"><h2>Second headline</h2></a>
  <div class="snippet">No source bar on this one.</div>
</div>
<div class="newsitem"><div class="snippet">no title link — skipped</div></div>"#;

    #[test]
    fn parses_fragment_items_with_source_metadata() {
        let results = parse_bing_news_fragment(FRAGMENT).unwrap();
        assert_eq!(results.len(), 2);
        // Title comes from the <h2> inside the a.title anchor; URL from its
        // href (attributes in any order, matching the real markup).
        assert_eq!(results[0].title, "Rust 2.0 roadmap published");
        assert_eq!(
            results[0].url,
            "https://www.msn.com/en-us/news/other/rust-roadmap/ar-AA2bxPBh"
        );
        // Metadata pair, SearXNG's picks: the source bar's first
        // aria-labelled span (the date) + the anchor's data-author (the
        // syndication host — not the newsitem div's own data-author).
        assert_eq!(
            results[0].snippet,
            "The core team outlined plans for 2026. (9/3/2026 | Polygon on MSN)"
        );
        // Score descends with rank.
        assert!(results[0].score > results[1].score);
        assert_eq!(results[0].engine, "bing_news");
    }

    #[test]
    fn item_without_source_bar_keeps_bare_snippet() {
        let results = parse_bing_news_fragment(FRAGMENT).unwrap();
        // No source bar, but the anchor carries data-author — the other half
        // of SearXNG's metadata pair.
        assert_eq!(results[1].snippet, "No source bar on this one. (The Register)");
    }

    #[test]
    fn portal_page_is_empty_not_error() {
        // A portal/interstitial that happens to pass body_ok upstream still
        // parses to zero items here — the engine reports "no results", not a
        // parse crash, so /doctor shows it as healthy-but-empty.
        let results = parse_bing_news_fragment("<html><body>portal</body></html>").unwrap();
        assert!(results.is_empty());
    }
}
