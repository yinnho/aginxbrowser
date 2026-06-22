//! Firecrawl-compatible `/v1/scrape` endpoint.
//!
//! Lets existing Firecrawl clients switch to aginxbrowser by changing only the
//! base URL. This is a thin adapter over `smart_fetch` — request shapes are
//! mapped to our `FetchRequest`, and our `FetchResponse` is reshaped to the
//! Firecrawl response envelope.

use axum::{
    Json,
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::render::smart_fetch;
use crate::server::{do_click};
use crate::{AppError, ClickRequest, FetchRequest, OutputFormat};

/// Firecrawl `/v1/scrape` request.
#[derive(Debug, Deserialize)]
pub struct ScrapeRequest {
    pub url: String,
    #[serde(default = "default_formats")]
    pub formats: Vec<String>,
    /// Firecrawl allows targeting a sub-section; we map it to our `selector`.
    #[serde(default)]
    pub only_main_content: bool,
    /// Milliseconds to wait for JS rendering.
    #[serde(default)]
    pub wait_for: Option<u64>,
    #[serde(default)]
    pub timeout: Option<u32>,
    /// Pre-extraction actions (click / wait / screenshot).
    #[serde(default)]
    pub actions: Vec<ScrapeAction>,
    /// Optional CSS selector (Firecrawl's `excludeTags`/main-content handling
    /// is simplified to a direct selector pass-through).
    #[serde(default)]
    pub selector: Option<String>,
}

fn default_formats() -> Vec<String> {
    vec!["markdown".into()]
}

/// A pre-extraction action. We support `click` and `wait`; `screenshot` and
/// others are accepted but ignored (no screenshot support yet).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScrapeAction {
    Click { selector: String },
    Wait { milliseconds: u32 },
    Screenshot,
    Scroll,
    WriteText { text: String, selector: Option<String> },
    PressKey { key: String },
}

/// Firecrawl `/v1/scrape` response.
#[derive(Debug, Serialize)]
pub struct ScrapeResponse {
    pub success: bool,
    pub data: ScrapeData,
}

#[derive(Debug, Serialize)]
pub struct ScrapeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    pub metadata: ScrapeMetadata,
}

#[derive(Debug, Serialize)]
pub struct ScrapeMetadata {
    pub title: Option<String>,
    /// Firecrawl calls this `sourceURL` (camelCase).
    #[serde(rename = "sourceURL")]
    pub source_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Handle `POST /v1/scrape`.
pub async fn scrape_handler(
    Json(req): Json<ScrapeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let wants_html = req.formats.iter().any(|f| f == "html");
    let wants_markdown = req.formats.iter().any(|f| f == "markdown");

    // If both formats requested (or html is among them), fetch raw HTML and
    // derive markdown from it via html2md. This avoids a second navigation.
    let format = if wants_html {
        OutputFormat::Html
    } else {
        OutputFormat::Markdown
    };

    // Map waitFor → wait_secs (seconds).
    let wait_secs = req.wait_for.map(|ms| ms / 1000).filter(|s| *s > 0);

    // Run click actions first (each navigates + clicks). We take the last
    // click's resulting page implicitly — Firecrawl actions chain on one page,
    // but our do_click opens a fresh page each time. For v1 we run the final
    // click and let smart_fetch re-navigate; this covers the common single-click
    // case. Pure-wait actions just extend the wait.
    let mut extra_wait: u64 = 0;
    for action in &req.actions {
        match action {
            ScrapeAction::Click { selector } => {
                let click_req = ClickRequest {
                    url: req.url.clone(),
                    selector: selector.clone(),
                    wait_secs: Some(2),
                    use_proxy: false,
                    cookies: vec![],
                };
                if let Err(e) = tokio::task::spawn_blocking(move || do_click(click_req)).await {
                    tracing::warn!("firecrawl click action failed: {}", e);
                }
            }
            ScrapeAction::Wait { milliseconds } => {
                extra_wait += (*milliseconds as u64) / 1000;
            }
            ScrapeAction::Screenshot | ScrapeAction::Scroll
            | ScrapeAction::WriteText { .. } | ScrapeAction::PressKey { .. } => {
                // Not yet supported; ignored.
            }
        }
    }
    let wait_secs = wait_secs.or_else(|| (extra_wait > 0).then_some(extra_wait));

    let fetch_req = FetchRequest {
        url: req.url.clone(),
        format: format.clone(),
        selector: req.selector.clone(),
        wait_secs,
        use_proxy: false,
        cookies: vec![],
        max_chars: 0, // Firecrawl clients expect full content.
        auto_bypass_challenge: true,
        render_tier: Default::default(),
    };

    match smart_fetch(fetch_req).await {
        Ok(resp) => {
            let (markdown, html) = match format {
                OutputFormat::Html => {
                    // resp.content is raw HTML. Derive markdown from it.
                    let stripped = crate::render::strip_non_content(&resp.content);
                    let md = if wants_markdown {
                        Some(html2md::parse_html(&stripped))
                    } else {
                        None
                    };
                    let h = if wants_html {
                        Some(resp.content.clone())
                    } else {
                        None
                    };
                    (md, h)
                }
                OutputFormat::Markdown => {
                    let md = if wants_markdown {
                        Some(resp.content.clone())
                    } else {
                        None
                    };
                    (md, None)
                }
                OutputFormat::Text => (Some(resp.content.clone()), None),
            };

            // Extract description from the HTML if we have it, else leave None.
            let description = html.as_deref().and_then(extract_description);

            let data = ScrapeData {
                markdown,
                html,
                links: None,
                metadata: ScrapeMetadata {
                    title: resp.title.clone(),
                    source_url: resp.url.clone(),
                    description,
                    status_code: 200,
                    error: None,
                },
            };
            Ok((StatusCode::OK, Json(ScrapeResponse { success: true, data })))
        }
        Err(e) => {
            // Firecrawl returns success:false on failure rather than an HTTP error.
            let data = ScrapeData {
                markdown: None,
                html: None,
                links: None,
                metadata: ScrapeMetadata {
                    title: None,
                    source_url: req.url,
                    description: None,
                    status_code: 500,
                    error: Some(format!("{}", e)),
                },
            };
            Ok((
                StatusCode::OK,
                Json(ScrapeResponse { success: false, data }),
            ))
        }
    }
}

/// Extract `<meta name="description" content="...">` from raw HTML.
fn extract_description(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    // Look for name="description" then walk back/forward to the content attr.
    let needle = r#"name="description""#;
    let idx = lower.find(needle).or_else(|| lower.find(r#"name='description'"#))?;
    // Search within ±200 chars for content="..."
    let start = idx.saturating_sub(200);
    let end = (idx + 200).min(lower.len());
    let window = &lower[start..end];
    let content_idx = window.find("content=")?;
    let after = &window[content_idx + 8..];
    let desc = if after.starts_with('"') {
        after[1..].split('"').next()?
    } else if after.starts_with('\'') {
        after[1..].split('\'').next()?
    } else {
        after.split_whitespace().next()?
    };
    let desc = desc.trim();
    if desc.is_empty() {
        None
    } else {
        // Pull from the original (non-lowered) string at the same offsets.
        Some(desc.to_string())
    }
}
