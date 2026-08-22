//! Firecrawl-compatible `/v1/scrape` endpoint.
//!
//! Lets existing Firecrawl clients switch to aginxbrowser by changing only the
//! base URL. Plain scrapes (no actions) use the fast layered renderer. When the
//! request carries actions or a screenshot, we run a real single-page browser
//! session: navigate once, execute the actions in order on that page, then
//! extract content (and optionally a screenshot) from its final state.

use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::render::smart_fetch;
use crate::{AppError, FetchRequest, OutputFormat};

/// Firecrawl `/v1/scrape` request.
#[allow(dead_code)] // only_main_content/timeout accepted for API compat, not yet wired
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
    /// Pre-extraction actions, executed sequentially on a single page.
    #[serde(default)]
    pub actions: Vec<ScrapeAction>,
    /// Optional CSS selector (Firecrawl's `excludeTags`/main-content handling
    /// is simplified to a direct selector pass-through).
    #[serde(default)]
    pub selector: Option<String>,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133", etc.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// Capture the full scrolled page height (can be large) instead of just the
    /// viewport, when a screenshot is requested via `formats: ["screenshot"]`
    /// with no `screenshot` action. Default true (matches Firecrawl). Ignored
    /// when a `screenshot` action is present — its `fullPage` field wins.
    #[serde(default = "default_screenshot_full_page")]
    pub screenshot_full_page: bool,
}

fn default_screenshot_full_page() -> bool {
    true
}

fn default_formats() -> Vec<String> {
    vec!["markdown".into()]
}

/// A pre-extraction action, executed in order on the same page. `click`,
/// `writeText`, `pressKey`, `scroll` and `wait` all run against the live page;
/// `screenshot` captures the page's final state (needs the `screenshot` feature).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ScrapeAction {
    #[serde(rename = "click")]
    Click { selector: String },
    #[serde(rename = "wait")]
    Wait { milliseconds: u32 },
    #[serde(rename = "screenshot")]
    Screenshot {
        /// Capture the full scrolled page height (can be large) instead of just
        /// the viewport. Default false — viewport-only keeps buffers bounded.
        #[serde(default, rename = "fullPage")]
        full_page: bool,
    },
    #[serde(rename = "scroll")]
    Scroll,
    #[serde(rename = "writeText")]
    WriteText { text: String, selector: Option<String> },
    #[serde(rename = "pressKey")]
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
    /// Base64 data-URI PNG of the rendered page, present when a `screenshot`
    /// action or `formats: ["screenshot"]` is requested and the binary is built
    /// with the `screenshot` feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
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
    let wants_screenshot = req.actions.iter().any(|a| matches!(a, ScrapeAction::Screenshot { .. }))
        || req.formats.iter().any(|f| f == "screenshot");

    // Screenshots can only be produced when built with the `screenshot` feature.
    // Without it, don't route to a session (or imply success) just for one.
    #[cfg(feature = "screenshot")]
    let screenshot_available = true;
    #[cfg(not(feature = "screenshot"))]
    let screenshot_available = false;

    // Any action (including `wait`) or a screenshot needs the single-page
    // session. A `wait` action must drive the V8 event loop — the fast path's
    // Tier-1 HTTP-direct can return an unrendered SPA shell and ignores
    // wait_secs entirely, so a Wait-only scrape of a JS-rendered page would
    // miss the JS-injected content. Plain scrapes (no actions) stay fast.
    let needs_session = !req.actions.is_empty()
        || (wants_screenshot && screenshot_available);

    if needs_session {
        return Ok(scrape_with_session(&req, wants_html, wants_markdown, wants_screenshot).await);
    }
    Ok(scrape_with_fetch(&req, wants_html, wants_markdown).await)
}

/// Fast path: plain scrape with no interactive actions. Uses the layered
/// renderer (HTTP direct when the page is static, obscura when JS is needed).
async fn scrape_with_fetch(
    req: &ScrapeRequest,
    wants_html: bool,
    wants_markdown: bool,
) -> (StatusCode, Json<ScrapeResponse>) {
    let format = if wants_html {
        OutputFormat::Html
    } else {
        OutputFormat::Markdown
    };
    // Fold any `wait` actions into the settle budget. Take the larger of
    // wait_for and the summed wait actions (they express the same intent —
    // don't silently drop one when the other is set). Round up to the next
    // second so a sub-second wait (e.g. 500ms) still yields ≥1s of settle,
    // not zero.
    let extra_wait_ms: u64 = req
        .actions
        .iter()
        .map(|a| match a {
            ScrapeAction::Wait { milliseconds } => *milliseconds as u64,
            _ => 0,
        })
        .sum();
    let wait_ms = req.wait_for.unwrap_or(0).max(extra_wait_ms);
    let wait_secs = if wait_ms > 0 {
        Some((wait_ms + 999) / 1000)
    } else {
        None
    };
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
        tls_fingerprint: req.tls_fingerprint.clone(),
        js_extract: None,
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

            let description = html.as_deref().and_then(extract_description);
            let data = ScrapeData {
                markdown,
                html,
                links: None,
                screenshot: None,
                metadata: ScrapeMetadata {
                    title: resp.title.clone(),
                    source_url: resp.url.clone(),
                    description,
                    status_code: 200,
                    error: None,
                },
            };
            (StatusCode::OK, Json(ScrapeResponse { success: true, data }))
        }
        Err(e) => {
            // Firecrawl returns success:false on failure rather than an HTTP error.
            let data = ScrapeData {
                markdown: None,
                html: None,
                links: None,
                screenshot: None,
                metadata: ScrapeMetadata {
                    title: None,
                    source_url: req.url.clone(),
                    description: None,
                    status_code: 500,
                    error: Some(format!("{}", e)),
                },
            };
            (StatusCode::OK, Json(ScrapeResponse { success: false, data }))
        }
    }
}

/// Result of a single-page session scrape.
struct ScrapeSessionOutcome {
    url: String,
    title: Option<String>,
    /// JS-rendered DOM (selector-scoped when `selector` was given).
    html: String,
    /// Raw PNG bytes of the rendered page, if requested (screenshot feature).
    screenshot_png: Option<Vec<u8>>,
}

/// Run all actions sequentially on a single page, then extract markdown/html
/// (and optionally a screenshot) from that page's final state.
async fn scrape_with_session(
    req: &ScrapeRequest,
    wants_html: bool,
    wants_markdown: bool,
    wants_screenshot: bool,
) -> (StatusCode, Json<ScrapeResponse>) {
    let url = req.url.clone();
    let actions = req.actions.clone();
    let selector = req.selector.clone();
    let tls_fingerprint = req.tls_fingerprint.clone();
    let wait_for_ms = req.wait_for.unwrap_or(0);
    let screenshot_full_page = req.screenshot_full_page;

    // run_on_local_runtime creates its own current-thread runtime (V8 is !Send),
    // so it must run on a blocking thread — same pattern as do_click/do_eval/
    // do_screenshot. Otherwise "cannot start a runtime from within a runtime".
    let outcome = tokio::task::spawn_blocking(move || {
        crate::server::run_on_local_runtime(move |_rt| {
            Box::pin(async move {
            let browser = crate::server::build_browser(false, &url, tls_fingerprint.as_deref())?;
            let mut page = browser.new_page().await?;
            page.goto(&url).await?;

            // Auto-bypass Cloudflare Turnstile challenges, matching do_fetch —
            // otherwise the "Just a moment..." stub would be scraped as content.
            crate::server::maybe_bypass_challenge(&mut page).await?;

            // Let JS settle after navigation before running actions.
            page.settle(wait_for_ms.max(2000)).await;

            for action in &actions {
                match action {
                    ScrapeAction::Click { selector } => {
                        // Click the element first so SPA click/hashchange listeners
                        // fire (a #fragment anchor's routing is driven by the click
                        // event, not by navigating to the href). Then, for http(s)
                        // anchors, also goto the href so the chain continues on the
                        // target when the site relies on native navigation. JS
                        // navigations the click handler starts are drained next.
                        let nav = page.evaluate(&format!(
                            "(() => {{ const el = document.querySelector({sel}); if (!el) return ''; el.click(); if (el.tagName === 'A' && el.href) {{ const attr = el.getAttribute('href') || ''; if (el.href.startsWith('javascript:') || attr.startsWith('#')) return ''; return el.href; }} return ''; }})()",
                            sel = js_str(selector),
                        ));
                        let mut navigated = false;
                        if let Some(url) = nav.as_str().filter(|s| !s.starts_with("javascript:") && !s.is_empty()) {
                            if let Ok(_u) = url::Url::parse(url) {
                                // A dead/timeout target must not abort the scrape —
                                // log and continue with the current page state.
                                if let Err(e) = page.goto(url).await {
                                    tracing::warn!("firecrawl click goto failed: {}", e);
                                } else {
                                    navigated = true;
                                }
                            }
                        }
                        // Drain any JS-initiated navigation from the click handler
                        // (location.href / form.submit) that evaluate couldn't
                        // complete.
                        if let Err(e) = page.process_pending_navigation().await {
                            tracing::warn!("firecrawl click nav drain failed: {}", e);
                        } else {
                            navigated = true;
                        }
                        if navigated {
                            // Let the target page's async render land before the
                            // next action runs (SPA routes render after load).
                            page.settle(1200).await;
                        }
                    }
                    ScrapeAction::Wait { milliseconds } => {
                        tokio::time::sleep(std::time::Duration::from_millis(*milliseconds as u64)).await;
                    }
                    ScrapeAction::WriteText { text, selector } => {
                        if let Some(sel) = selector {
                            page.evaluate(&format!(
                                "(() => {{ const el = document.querySelector({sel}); if (!el) return false; el.focus(); const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value')?.set || Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value')?.set; if (setter) setter.call(el, {text}); else el.value = {text}; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return true; }})()",
                                sel = js_str(sel),
                                text = js_str(text),
                            ));
                        }
                    }
                    ScrapeAction::PressKey { key } => {
                        let (code, key_code) = key_code_for(key);
                        // Synthetic KeyboardEvents don't trigger native form
                        // implicit submission, and JS-initiated navigation inside
                        // evaluate is unreliable. So: dispatch the key events,
                        // and when Enter lands in a GET form, have the script
                        // RETURN the form's action URL (controls serialized) and
                        // navigate with page.goto() — the browser's real nav path.
                        // Serialization matches real form submission: skip disabled
                        // controls, checkbox/radio only when checked, <select
                        // multiple> appends every selected option's value.
                        let submit = if key.eq_ignore_ascii_case("enter") { "true" } else { "false" };
                        let nav = page.evaluate(&format!(
                            "(() => {{ const el = document.activeElement; if (!el) return ''; const opts = {{ key: {key}, code: {code}, keyCode: {key_code}, which: {key_code}, bubbles: true }}; el.dispatchEvent(new KeyboardEvent('keydown', opts)); el.dispatchEvent(new KeyboardEvent('keypress', opts)); el.dispatchEvent(new KeyboardEvent('keyup', opts)); if ({submit}) {{ const form = el.form; if (form && !(form.method && form.method.toLowerCase() === 'post')) {{ try {{ const u = new URL(form.action || window.location.href, window.location.href); for (const c of form.elements) {{ if (!c.name || c.disabled) continue; const t = c.type; if (t === 'submit' || t === 'button' || t === 'image' || t === 'file' || t === 'reset') continue; if (t === 'checkbox' || t === 'radio') {{ if (c.checked) u.searchParams.append(c.name, c.value || 'on'); continue; }} if (t === 'select-multiple') {{ for (const o of c.selectedOptions) u.searchParams.append(c.name, o.value); continue; }} u.searchParams.append(c.name, c.value !== undefined ? c.value : ''); }} return u.toString(); }} catch (e) {{ return ''; }} }} }} return ''; }})()",
                            key = js_str(key),
                            code = js_str(&code),
                            key_code = key_code,
                            submit = submit,
                        ));
                        let mut navigated = false;
                        if let Some(url) = nav.as_str().filter(|s| !s.starts_with("javascript:") && !s.is_empty()) {
                            if let Ok(_u) = url::Url::parse(url) {
                                // A dead/timeout target must not abort the scrape —
                                // log and continue with the current page state.
                                if let Err(e) = page.goto(url).await {
                                    tracing::warn!("firecrawl pressKey goto failed: {}", e);
                                } else {
                                    navigated = true;
                                }
                            }
                        }
                        // Drain a navigation the site's own key handler started
                        // (e.g. Enter on a POST form whose submit handler runs).
                        if let Err(e) = page.process_pending_navigation().await {
                            tracing::warn!("firecrawl pressKey nav drain failed: {}", e);
                        } else {
                            navigated = true;
                        }
                        if navigated {
                            page.settle(1200).await;
                        }
                    }
                    ScrapeAction::Scroll => {
                        page.scroll_by(0, 800);
                    }
                    ScrapeAction::Screenshot { .. } => {
                        // Captured from the page's final state below.
                    }
                }
            }

            // Let async effects of the actions land before extracting.
            page.settle(1500).await;

            let final_url = page.url();
            let title = page
                .evaluate(
                    "((document.querySelector('#activity-name,h1,.article-title')||{}).textContent||'').trim()\
                     || document.title\
                     || (document.querySelector('meta[property=\"og:title\"]')||{}).content\
                     || ''",
                )
                .as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let full_html = page.content();

            let html = match &selector {
                Some(sel) => page
                    .evaluate(&format!(
                        "(() => {{ const el = document.querySelector({sel}); return el ? el.outerHTML : document.documentElement.outerHTML; }})()",
                        sel = js_str(sel),
                    ))
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| full_html.clone()),
                None => full_html.clone(),
            };

            // Free the V8 page/browser before the CPU-bound Blitz layout/paint
            // (do_screenshot drops them first for the same reason) so peak memory
            // stays bounded on large pages. full_page: an explicit Screenshot
            // action's fullPage wins; otherwise the request-level screenshot_full_page
            // (default true, matches Firecrawl) applies for formats-only requests.
            #[cfg(feature = "screenshot")]
            let full_page = actions
                .iter()
                .find_map(|a| match a {
                    ScrapeAction::Screenshot { full_page } => Some(*full_page),
                    _ => None,
                })
                .unwrap_or(screenshot_full_page);
            #[cfg(not(feature = "screenshot"))]
            let _ = screenshot_full_page;
            // Pre-fetch images/stylesheets while the page's cookie'd HTTP
            // client is still alive (dropped right below, before paint).
            #[cfg(feature = "screenshot")]
            let resources = crate::screenshot::prefetch_render_resources(&page, &final_url, &full_html).await;
            drop(page);
            drop(browser);

            #[cfg(feature = "screenshot")]
            let screenshot_png = if wants_screenshot {
                // A render failure must not abort the scrape — the markdown/html
                // are already extracted; just omit the screenshot (like the old
                // capture_screenshot, which warned and returned None).
                match crate::screenshot::render_html_to_png(
                    &full_html,
                    &final_url,
                    1280,
                    800,
                    1.0,
                    full_page,
                    None,
                    false,
                    Some(&resources),
                ) {
                    Ok(rendered) => Some(rendered.png),
                    Err(e) => {
                        tracing::warn!("firecrawl screenshot render failed: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            #[cfg(not(feature = "screenshot"))]
            let screenshot_png: Option<Vec<u8>> = None;
            #[cfg(not(feature = "screenshot"))]
            let _ = wants_screenshot;

            Ok(ScrapeSessionOutcome {
                url: final_url,
                title,
                html,
                screenshot_png,
            })
            })
        })
    })
    .await;

    match outcome {
        Ok(Ok(outcome)) => {
            // Only strip+convert when markdown was requested — strip_non_content
            // allocates a lowercased copy + same-capacity output, wasted on a
            // screenshot-only or html-only request.
            let markdown = if wants_markdown {
                let stripped = crate::render::strip_non_content(&outcome.html);
                Some(html2md::parse_html(&stripped))
            } else {
                None
            };
            let html = if wants_html { Some(outcome.html) } else { None };
            let description = html.as_deref().and_then(extract_description);
            let screenshot = outcome.screenshot_png.map(|png| {
                use base64::{engine::general_purpose::STANDARD, Engine as _};
                format!("data:image/png;base64,{}", STANDARD.encode(&png))
            });

            let data = ScrapeData {
                markdown,
                html,
                links: None,
                screenshot,
                metadata: ScrapeMetadata {
                    title: outcome.title,
                    source_url: outcome.url,
                    description,
                    status_code: 200,
                    error: None,
                },
            };
            (StatusCode::OK, Json(ScrapeResponse { success: true, data }))
        }
        Ok(Err(e)) => {
            let data = ScrapeData {
                markdown: None,
                html: None,
                links: None,
                screenshot: None,
                metadata: ScrapeMetadata {
                    title: None,
                    source_url: req.url.clone(),
                    description: None,
                    status_code: 500,
                    error: Some(format!("{}", e)),
                },
            };
            (StatusCode::OK, Json(ScrapeResponse { success: false, data }))
        }
        Err(e) => {
            let data = ScrapeData {
                markdown: None,
                html: None,
                links: None,
                screenshot: None,
                metadata: ScrapeMetadata {
                    title: None,
                    source_url: req.url.clone(),
                    description: None,
                    status_code: 500,
                    error: Some(format!("{}", e)),
                },
            };
            (StatusCode::OK, Json(ScrapeResponse { success: false, data }))
        }
    }
}
/// Encode a Rust string as a JSON string literal (also a valid JS string literal).
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Map a key name to its DOM `code` and numeric `keyCode`.
fn key_code_for(key: &str) -> (String, u32) {
    match key.to_ascii_lowercase().as_str() {
        "enter" => ("Enter".into(), 13),
        "escape" | "esc" => ("Escape".into(), 27),
        "backspace" => ("Backspace".into(), 8),
        "tab" => ("Tab".into(), 9),
        "arrowup" => ("ArrowUp".into(), 38),
        "arrowdown" => ("ArrowDown".into(), 40),
        "arrowleft" => ("ArrowLeft".into(), 37),
        "arrowright" => ("ArrowRight".into(), 39),
        "space" => ("Space".into(), 32),
        "delete" => ("Delete".into(), 46),
        other => (other.to_string(), 0),
    }
}

/// Extract `<meta name="description" content="...">` from raw HTML.
fn extract_description(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    // Look for name="description" then walk back/forward to the content attr.
    let needle = r#"name="description""#;
    let idx = lower.find(needle).or_else(|| lower.find(r#"name='description'"#))?;
    // Search within ±200 chars for content="...". Snap the window to char
    // boundaries — a CJK description means raw byte offsets can land mid-char
    // and slicing would panic (observed on baidu.com).
    let snap_floor = |i: usize| {
        let mut i = i;
        while i > 0 && !lower.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let snap_ceil = |i: usize| {
        let mut i = i.min(lower.len());
        while i < lower.len() && !lower.is_char_boundary(i) {
            i += 1;
        }
        i
    };
    let start = snap_floor(idx.saturating_sub(200));
    let end = snap_ceil(idx + 200);
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
