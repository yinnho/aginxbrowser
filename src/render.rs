//! Tiered rendering strategy: try cheap HTTP-direct first, fall back to the
//! obscura browser only when the page needs JS rendering.
//!
//! - Tier 1 (`http_fetch`): pure `ObscuraHttpClient`, no V8. ~100ms. Works for
//!   static HTML. Returns `None` when the content looks insufficient (SPA shell,
//!   antispider redirect, non-200) so the caller upgrades to Tier 2.
//! - Tier 2 (`do_fetch` in `server.rs`): full obscura browser with V8/JS.
//!   ~1-2s. Handles SPAs, Cloudflare, JS-rendered content.

use std::sync::Arc;

use crate::{FetchResponse, OutputFormat, RenderTier};

use crate::obscura_net::{CookieJar, ObscuraHttpClient};

/// Does the URL point at a known antispider/CAPTCHA redirect target?
/// Shared by the render tier and the search module.
pub fn is_antispider_url(url: &str) -> bool {
    url.contains("/antispider")
        || url.contains("wappass.baidu.com")
        || url.contains("sorry.google.com")
        || url.contains("challenge-platform")
}

/// Heuristic: is this HTML "sufficient" to return without JS rendering?
///
/// Returns `false` (→ upgrade to Tier 2) when:
/// - It carries a `<noscript>` "enable JS" hint, OR
/// - It's a known SPA shell (`<div id="app">` / `<div id="root">`) with almost
///   no visible text, OR
/// - The extracted text is suspiciously tiny (< 200 chars — covers near-empty
///   challenge stubs and redirect placeholders).
///
/// Otherwise `true` — the static HTML already carries the content.
fn is_content_sufficient(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();

    // <noscript> with a JS-required hint → likely needs rendering.
    if lower.contains("<noscript") && lower.contains("enable javascript") {
        return false;
    }

    // Crude text extraction: drop everything between < and >, collapse ws.
    let text_only: String = strip_html_tags(&lower).split_whitespace().collect::<Vec<_>>().join(" ");

    // SPA framework shells: a mount point with near-empty body text. We only
    // flag it when the visible text is very short, which is the SPA signature.
    let has_spa_mount = lower.contains(r#"id="app""#)
        || lower.contains(r#"id='app'"#)
        || lower.contains(r#"id="root""#)
        || lower.contains(r#"id='root'"#);
    if has_spa_mount && text_only.len() < 200 {
        return false;
    }

    // Too little visible text to be a real page (challenge stubs, redirects).
    // Many legit small pages exist, so the bar is low here — we only defer
    // when there's essentially nothing readable.
    if text_only.len() < 64 {
        return false;
    }

    true
}

/// Extract `<title>` from raw HTML (no V8 needed).
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after_open = lower[start..].find('>')? + start + 1;
    let end = lower[after_open..].find("</title>")? + after_open;
    let title = html[after_open..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Remove `<style>`, `<script>`, `<noscript>`, and `<head>` blocks from HTML so
/// they don't leak into markdown/text output (html2md doesn't strip them).
pub fn strip_non_content(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let b = lower.as_bytes();
    // Tags whose entire contents (incl. inner text) we drop.
    const DROP: &[&[u8]] = &[
        b"<style", b"<script", b"<noscript", b"<head",
    ];
    while i < bytes.len() {
        if b[i] == b'<' {
            // Find the tag name end to match DROP prefixes.
            let matched = DROP.iter().any(|tag| {
                lower[i..].as_bytes().starts_with(tag)
            });
            if matched {
                // Skip to the matching close tag.
                let close = find_close_tag(&lower, i);
                if let Some(pos) = close {
                    i = pos;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Find the position just after the close tag matching the tag starting at `start`.
fn find_close_tag(lower: &str, start: usize) -> Option<usize> {
    // Determine the tag name (letters after '<').
    let bytes = lower.as_bytes();
    let mut name_end = start + 1;
    while name_end < bytes.len() && bytes[name_end].is_ascii_alphabetic() {
        name_end += 1;
    }
    let tag_name = &lower[start + 1..name_end];
    let close = format!("</{}>", tag_name);
    lower[name_end..].find(&close).map(|p| name_end + p + close.len())
}

/// Strip HTML tags → plain text (very rough; for `OutputFormat::Text` on Tier 1).
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for &b in html.as_bytes() {
        match b {
            b'<' => in_tag = true,
            b'>' => in_tag = false,
            _ if !in_tag => out.push(b as char),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tier 1: fetch via plain HTTP and return if the content is sufficient.
///
/// Returns `None` when the page needs JS rendering (Tier 2). On hard network
/// errors returns `Some(Err)` so the caller can surface it instead of silently
/// falling through to the slower path.
///
/// `proxy_url`: the `OBSCURA_PROXY` value, applied when `use_proxy` is set or
/// the domain is known-blocked (mirrors `build_browser` in server.rs).
pub async fn http_fetch(
    url: &str,
    use_proxy: bool,
    proxy_url: Option<&str>,
    format: OutputFormat,
    selector: Option<&str>,
    cookies: &[String],
    max_chars: usize,
) -> Result<Option<FetchResponse>, String> {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return Err(format!("invalid url: {e}")),
    };

    // Proxy decision mirrors server.rs::should_auto_proxy + use_proxy.
    let use_proxy = use_proxy || crate::server::should_auto_proxy(url);
    let proxy = if use_proxy { proxy_url } else { None };

    let jar = Arc::new(CookieJar::new());
    // Inject request cookies into the jar (same logic as server.rs::inject_cookies,
    // but on the standalone jar rather than a Browser).
    if !cookies.is_empty() {
        let base = url::Url::parse(url).ok();
        let domain = base
            .as_ref()
            .and_then(|u| u.host_str())
            .unwrap_or("");
        for c in cookies {
            let full = if c.to_ascii_lowercase().contains("domain=")
                || c.to_ascii_lowercase().contains("path=")
            {
                c.clone()
            } else {
                format!("{}; Domain={}; Path=/", c, domain)
            };
            let _ = jar.set_cookie(&full, &parsed);
        }
    }

    let client = ObscuraHttpClient::with_full_options(jar, proxy, false);
    let resp = client.fetch(&parsed).await.map_err(|e| e.to_string())?;

    // Non-200 → let Tier 2 try (it handles redirects/challenges differently).
    if resp.status != 200 {
        tracing::debug!("http_fetch: status {} for {}, deferring to Tier 2", resp.status, url);
        return Ok(None);
    }

    // Non-HTML → Tier 1 can't render; defer.
    if !resp.is_html() {
        tracing::debug!("http_fetch: non-HTML content-type for {}, deferring to Tier 2", url);
        return Ok(None);
    }

    // Antispider redirect → defer (Tier 2 has the challenge-bypass logic).
    if is_antispider_url(resp.url.as_str()) || resp.redirected_from.iter().any(|u| is_antispider_url(u.as_str())) {
        tracing::debug!("http_fetch: antispider redirect for {}, deferring to Tier 2", url);
        return Ok(None);
    }

    let html = resp.text();

    // Insufficient (SPA shell / too short) → defer to JS rendering.
    if !is_content_sufficient(&html) {
        tracing::debug!("http_fetch: content insufficient (len={}) for {}, deferring to Tier 2", html.len(), url);
        return Ok(None);
    }

    let title = extract_title(&html);
    // Drop <style>/<script>/<head> so they don't leak into markdown/text.
    let body_html = strip_non_content(&html);
    let content = match format {
        OutputFormat::Html => {
            if let Some(sel) = selector {
                extract_selector_html(&body_html, sel)
            } else {
                html.clone()
            }
        }
        OutputFormat::Text => strip_html_tags(&body_html),
        OutputFormat::Markdown => {
            if let Some(sel) = selector {
                let h = extract_selector_html(&body_html, sel);
                if h.is_empty() {
                    String::new()
                } else {
                    html2md::parse_html(&h)
                }
            } else {
                html2md::parse_html(&body_html)
            }
        }
    };

    let (content, truncated) = if max_chars > 0 && content.chars().count() > max_chars {
        (content.chars().take(max_chars).collect::<String>(), true)
    } else {
        (content, false)
    };

    Ok(Some(FetchResponse {
        url: resp.url.to_string(),
        title,
        content,
        truncated,
    }))
}

/// Extract the outer HTML of the first element matching `selector` using
/// `scraper`. Returns the full document if parsing/selecting fails.
fn extract_selector_html(html: &str, selector: &str) -> String {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse(selector) else {
        return html.to_string();
    };
    let Some(elem) = doc.select(&sel).next() else {
        return String::new();
    };
    elem.html()
}

/// Decide whether Tier 1 (HTTP) should be attempted at all for this request.
fn tier1_eligible(req: &crate::FetchRequest) -> bool {
    match req.render_tier {
        RenderTier::Obscura => false,
        RenderTier::Http | RenderTier::Auto => true,
    }
}

/// Dispatch a fetch through the tiered strategy.
///
/// Tier 1 (HTTP direct) runs on the ambient Tokio runtime — it's pure async
/// HTTP with no V8, so it needs no `run_on_local_runtime`. Only if Tier 1
/// declines (returns `None`) do we fall back to Tier 2, which spins up the
/// current-thread runtime for V8.
pub async fn smart_fetch(req: crate::FetchRequest) -> Result<FetchResponse, anyhow::Error> {
    // Tier 1: HTTP direct (only when not forced to obscura).
    if tier1_eligible(&req) {
        let proxy_url = std::env::var("OBSCURA_PROXY").ok();
        match http_fetch(
            &req.url,
            req.use_proxy,
            proxy_url.as_deref(),
            req.format.clone(),
            req.selector.as_deref(),
            &req.cookies,
            req.max_chars,
        )
        .await
        {
            Ok(Some(resp)) => {
                tracing::info!("smart_fetch: Tier 1 (HTTP) succeeded for {}", req.url);
                return Ok(resp);
            }
            Ok(None) => {
                tracing::info!("smart_fetch: Tier 1 deferred {} to Tier 2", req.url);
            }
            Err(e) => {
                // Tier 1 network error — fall through to Tier 2, which may
                // succeed with different fetch settings (stealth, etc.).
                tracing::warn!("smart_fetch: Tier 1 error for {}: {}, trying Tier 2", req.url, e);
            }
        }
    }

    // Tier 2: obscura browser (existing do_fetch logic, runs on a dedicated
    // current-thread runtime via spawn_blocking because V8 is !Send — calling
    // run_on_local_runtime directly from an async context panics).
    tracing::info!("smart_fetch: Tier 2 (obscura) for {}", req.url);
    match tokio::task::spawn_blocking(move || crate::server::do_fetch(req)).await {
        Ok(res) => res.map_err(Into::into),
        Err(e) => Err(anyhow::anyhow!("Tier 2 fetch task panicked: {}", e)),
    }
}
