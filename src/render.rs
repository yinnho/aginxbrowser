//! Tiered rendering strategy: try cheap HTTP-direct first, fall back to the
//! diting browser only when the page needs JS rendering.
//!
//! - Tier 1 (`http_fetch`): pure `HttpClient`, no V8. ~100ms. Works for
//!   static HTML. Returns `None` when the content looks insufficient (SPA shell,
//!   antispider redirect, non-200) so the caller upgrades to Tier 2.
//! - Tier 2 (`do_fetch` in `server.rs`): full diting browser with V8/JS.
//!   ~1-2s. Handles SPAs, Cloudflare, JS-rendered content.

use std::sync::Arc;

use crate::{FetchResponse, OutputFormat, RenderTier};

use crate::diting_net::{CookieJar, HttpClient};

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

    // Measure text after dropping script/style/noscript/head bodies, the same
    // drop the markdown conversion applies. Measuring the raw HTML let a
    // JS-only SPA shell (17KB inline bundle, zero text nodes) pass the bar on
    // its script text, and Tier 1 then served the empty conversion result
    // (gongkaoleida class) instead of deferring to Tier 2.
    let text_only: String = strip_html_tags(&strip_non_content(html));

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
        // 按 UTF-8 字符边界 push，而非单字节（否则中文/日文等非 ASCII 被拆成 Latin-1 乱码）
        let ch = html[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
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
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bytedance WAF JS challenge stub (juejin.cn class)? Detected by signature:
/// the stub must reference the `_wafchallengeid` answer cookie and its
/// `readygo` driver. Size-based deferral misses it because the inline PoW
/// script's text inflates the visible-text heuristic past the bar.
fn is_byte_waf_challenge_html(html: &str) -> bool {
    html.contains("_wafchallengeid") && html.contains("readygo")
}

/// Tier 1: fetch via plain HTTP and return if the content is sufficient.
///
/// Returns `None` when the page needs JS rendering (Tier 2). On hard network
/// errors returns `Some(Err)` so the caller can surface it instead of silently
/// falling through to the slower path.
///
/// `proxy_url`: the `AGINXBROWSER_PROXY` value, applied when `use_proxy` is set or
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

    // Proxy decision mirrors config.rs::should_auto_proxy + use_proxy.
    let use_proxy = use_proxy || crate::config::should_auto_proxy(url);
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

    let client = HttpClient::with_full_options(jar, proxy, false);
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

    // Bytedance WAF JS challenge stub → defer (Tier 2 rides out the PoW).
    if is_byte_waf_challenge_html(&html) {
        tracing::debug!("http_fetch: byte-WAF challenge stub for {}, deferring to Tier 2", url);
        return Ok(None);
    }

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

    // Safety net under the sufficiency bar: if the conversion produced
    // nothing at all, this page had no content outside script/style (or the
    // selector didn't match the static DOM). Tier 2 may still render real
    // content — serving an empty result at tier "http" is the worst outcome.
    if content.trim().is_empty() {
        tracing::debug!("http_fetch: converted content empty for {}, deferring to Tier 2", url);
        return Ok(None);
    }

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
        captcha_event: None,
        js_extract_result: None,
        tier: Some("http"),
        redirected_from: resp
            .redirected_from
            .iter()
            .map(|u| u.to_string())
            .collect(),
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
    // Per-domain page budget — see rate.rs for the stance. Runs before the
    // tiers (and before the caller's response cache, whose hits never count:
    // a repeat the target never sees costs it nothing).
    crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
    // Tier 1: HTTP direct (only when not forced to the browser).
    if tier1_eligible(&req) {
        let proxy_url = crate::config::proxy_from_env();
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

    // Tier 2: diting browser (existing do_fetch logic, runs on a dedicated
    // current-thread runtime via spawn_blocking because V8 is !Send — calling
    // run_on_local_runtime directly from an async context panics).
    tracing::info!("smart_fetch: Tier 2 (browser) for {}", req.url);
    match tokio::task::spawn_blocking(move || crate::server::do_fetch(req)).await {
        Ok(res) => res.map_err(Into::into),
        Err(e) => Err(anyhow::anyhow!("Tier 2 fetch task panicked: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_content_sufficient ----

    #[test]
    fn content_sufficient_static_html() {
        let html = r#"<html><head><title>Hello World Page Title</title></head>
            <body><h1>Welcome to the site</h1>
            <p>This is a real page with more than enough visible text content to clearly pass the threshold for sufficiency checking.</p></body></html>"#;
        assert!(is_content_sufficient(html));
    }

    #[test]
    fn content_insufficient_spa_shell() {
        // SPA mount point with near-empty body text.
        let html = r#"<html><body><div id="app"></div></body></html>"#;
        assert!(!is_content_sufficient(html));
    }

    #[test]
    fn content_sufficient_spa_with_text() {
        // SPA mount that already has substantial SSR text (> 200 chars) —
        // passes the stricter SPA-shell threshold.
        let html = r#"<html><body><div id="app">This rendered content contains substantially more than two hundred characters of real visible text so it passes the stricter SPA-shell threshold that the content sufficiency heuristic applies whenever a known framework mount point is detected in the page markup.</div></body></html>"#;
        assert!(is_content_sufficient(html));
    }

    #[test]
    fn content_insufficient_noscript_js_hint() {
        let html = r#"<html><head><title>x</title></head>
            <body><noscript>Please enable JavaScript to run this app.</noscript></body></html>"#;
        assert!(!is_content_sufficient(html));
    }

    #[test]
    fn content_insufficient_near_empty() {
        // Very little visible text — looks like a challenge stub.
        let html = "<html><body>ok</body></html>";
        assert!(!is_content_sufficient(html));
    }

    // ---- is_antispider_url ----

    #[test]
    fn antispider_url_detection() {
        assert!(is_antispider_url("https://wappass.baidu.com/captcha"));
        assert!(is_antispider_url("https://example.com/antispider/check"));
        assert!(is_antispider_url("https://sorry.google.com/sorry"));
        assert!(is_antispider_url("https://x.com/cdn-cgi/challenge-platform/h/g/"));
    }

    #[test]
    fn normal_url_not_antispider() {
        assert!(!is_antispider_url("https://example.com/"));
        assert!(!is_antispider_url("https://mp.weixin.qq.com/s/article"));
    }

    // ---- is_byte_waf_challenge_html ----

    #[test]
    fn byte_waf_stub_detected_by_signature() {
        // Realistic challenge stub: inline PoW script text far exceeds the
        // 64-char visible-text bar, so size heuristics alone would keep it.
        let html = r#"<html><head><meta charset="utf-8"><title>challenge</title></head>
            <body onload="readygo()"><script src="https://lf3-short.ibytedapm.com/slardar.js"></script>
            <script>var cs="eyJ2Ijp7ImEiOiJhYmMiLCJiIjoxNzAwLCJjIjoiZGVmIn0sInMiOiJzaWcifQ==";
            function readygo(){var c=JSON.parse(atob(cs));
            document.cookie="_wafchallengeid="+btoa(JSON.stringify(c))+"; Max-Age=1";location.reload();}</script>
            Please wait...</body></html>"#;
        assert!(is_byte_waf_challenge_html(html));
        // The signature gate is what actually catches this stub (it fires
        // first in http_fetch). The sufficiency bar now rejects it too —
        // script bodies don't count as visible text, leaving "Please
        // wait..." under the 64-char bar — so an unsigned variant of the
        // same stub can't slip through Tier 1 either.
        assert!(!is_content_sufficient(html));
    }

    #[test]
    fn normal_article_not_byte_waf() {
        let html = r#"<html><body><article><h1>readygo launched a new product</h1>
            <p>An article mentioning readygo, the observability tool, in running text
            without any WAF cookie reference. This is enough text to be sufficient.</p></article></body></html>"#;
        assert!(!is_byte_waf_challenge_html(html));
    }

    // ---- extract_title ----

    #[test]
    fn extract_title_present() {
        assert_eq!(extract_title("<html><head><title>My Page</title></head></html>").as_deref(), Some("My Page"));
    }

    #[test]
    fn extract_title_missing() {
        assert_eq!(extract_title("<html><body>no title</body></html>"), None);
    }

    #[test]
    fn extract_title_case_insensitive_tag() {
        assert_eq!(extract_title("<TITLE>Mixed Case</TITLE>").as_deref(), Some("Mixed Case"));
    }

    // ---- strip_non_content ----

    #[test]
    fn strip_non_content_removes_style_script() {
        let html = r#"<style>body{color:red}</style><script>alert(1)</script><p>keep me</p>"#;
        let out = strip_non_content(html);
        assert!(!out.contains("color:red"));
        assert!(!out.contains("alert(1)"));
        assert!(out.contains("keep me"));
    }

    #[test]
    fn strip_non_content_keeps_body() {
        let html = "<head><meta charset='utf-8'></head><body><p>text</p></body>";
        let out = strip_non_content(html);
        assert!(out.contains("text"));
        assert!(!out.contains("charset"));
    }

    // ---- strip_html_tags ----

    #[test]
    fn strip_html_tags_collapses_whitespace() {
        let html = "<p>hello</p>\n  <b>world</b>";
        let out = strip_html_tags(html);
        assert_eq!(out, "hello world");
    }

    #[test]
    fn strip_html_tags_nested() {
        let out = strip_html_tags("<div><span>a</span><span>b</span></div>");
        assert_eq!(out, "ab");
    }

    // ---- UTF-8 multibyte preservation ----

    #[test]
    fn strip_non_content_preserves_multibyte_utf8() {
        // The old byte-wise copy split each UTF-8 code unit into Latin-1
        // mojibake ("这" -> "è ¿ \x99"). Chinese/Japanese text must survive.
        let html = "<p>这是一段中文。</p><script>alert(1)</script>";
        let out = strip_non_content(html);
        assert!(out.contains("这是一段中文。"));
        assert!(!out.contains("alert(1)"));
    }

    #[test]
    fn strip_html_tags_preserves_multibyte_utf8() {
        let out = strip_html_tags("<div>日本語のテキスト</div>");
        assert_eq!(out, "日本語のテキスト");
    }

    // ---- sufficiency bar vs script/style bodies (gongkaoleida class) ----

    #[test]
    fn content_sufficient_rejects_script_only_shell() {
        // The gongkaoleida shape: a big inline bundle, zero text nodes, no
        // <title>. The script text alone used to pass the 64-char bar, Tier 1
        // accepted the page, and the markdown conversion came back empty.
        let bundle = "var a=1;function f(){return document.getElementById('x')};".repeat(60);
        let html = format!(
            r#"<html><head><script>{bundle}</script></head><body><div id="page"></div><script>{bundle}</script></body></html>"#
        );
        assert!(!is_content_sufficient(&html));
    }

    #[test]
    fn content_sufficient_counts_text_outside_scripts() {
        // A bundle plus real body text must still pass — the fix defers
        // script-only shells, not every scripting-heavy page.
        let bundle = "var longVariableName=function(anotherArgument){return anotherArgument*2};".repeat(20);
        let html = format!(
            r#"<html><head><script>{bundle}</script></head><body><h1>Welcome to the site</h1>
            <p>This is a real page with more than enough visible text content to clearly pass the threshold for sufficiency checking.</p></body></html>"#
        );
        assert!(is_content_sufficient(&html));
    }

    // ---- http_fetch deferral over a loopback fixture ----

    /// Serve one fixed raw HTTP response to every request on an ephemeral
    /// port; returns the port. Raw bytes so fixtures control the exact wire
    /// (gzip members, header casing).
    async fn raw_response_fixture(response: Vec<u8>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let r = response.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(&r).await;
                });
            }
        });
        port
    }

    /// Hand-roll a one-member gzip stream holding a single stored
    /// (uncompressed) deflate block, so a fixture can serve a gzip response
    /// without pulling a compression crate into dev-deps.
    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= 65535);
        fn crc32_ieee(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for i in 0..256u32 {
                let mut c = i;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
                table[i as usize] = c;
            }
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
            }
            crc ^ 0xFFFF_FFFF
        }
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0x03];
        let len = data.len() as u16;
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32_ieee(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    // A script-only shell must defer to Tier 2, not come back as an empty
    // tier-"http" result: the sufficiency bar ignores script text and the
    // converted-content emptiness check catches whatever else slips through.
    #[tokio::test(flavor = "current_thread")]
    async fn http_fetch_defers_script_only_shell_to_tier_2() {
        let _guard = crate::server::test_util::net_env_guard();
        let bundle = "var a=1;function f(){return document.getElementById('x')};".repeat(60);
        let body = format!(
            r#"<html><head><script>{bundle}</script></head><body><div id="page"></div></body></html>"#
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let port = raw_response_fixture(response.into_bytes()).await;
        let got = http_fetch(
            &format!("http://127.0.0.1:{port}/"),
            false,
            None,
            OutputFormat::Markdown,
            None,
            &[],
            50000,
        )
        .await
        .unwrap();
        assert!(got.is_none(), "script-only shell must defer to Tier 2, got {:?}", got.map(|r| r.content.chars().count()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_fetch_accepts_real_static_content() {
        let _guard = crate::server::test_util::net_env_guard();
        let body = "<html><head><title>static ok</title></head><body><p>Plenty of real text on this page, comfortably past the sufficiency bar for the tier-one HTTP gate.</p></body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let port = raw_response_fixture(response.into_bytes()).await;
        let got = http_fetch(
            &format!("http://127.0.0.1:{port}/"),
            false,
            None,
            OutputFormat::Markdown,
            None,
            &[],
            50000,
        )
        .await
        .unwrap()
        .expect("static content must be served at tier http");
        assert_eq!(got.tier, Some("http"));
        assert_eq!(got.title.as_deref(), Some("static ok"));
        assert!(!got.content.trim().is_empty());
    }

    // requested/effective pairing: the tier-"http" response must carry the
    // redirect trail — `url` alone can't tell a 200 at the requested address
    // from a 200 somewhere the request was bounced to.
    #[tokio::test(flavor = "current_thread")]
    async fn http_fetch_exposes_the_redirect_trail() {
        let _guard = crate::server::test_util::net_env_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]);
                    let out = if head.starts_with("GET /start") {
                        "HTTP/1.1 302 Found\r\nlocation: /final\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    } else {
                        let body = "<html><head><title>after hop</title></head><body><p>Plenty of real text on this page, comfortably past the sufficiency bar for the tier-one HTTP gate.</p></body></html>";
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    };
                    let _ = stream.write_all(out.as_bytes()).await;
                });
            }
        });
        let got = http_fetch(
            &format!("http://127.0.0.1:{port}/start"),
            false,
            None,
            OutputFormat::Markdown,
            None,
            &[],
            50000,
        )
        .await
        .unwrap()
        .expect("redirected fetch must land at tier http");
        assert_eq!(got.url, format!("http://127.0.0.1:{port}/final"));
        assert_eq!(
            got.redirected_from,
            vec![format!("http://127.0.0.1:{port}/start")]
        );
    }

    // Aliyun Tengine fronts (gongkaoleida class) reply `Content-Encoding:
    // gzip` even though we never advertise Accept-Encoding. The engine must
    // decode by the response header — a title pulled out of the gunzipped
    // page is the proof (a raw-bytes pass would leave title None).
    #[tokio::test(flavor = "current_thread")]
    async fn http_fetch_decodes_unconditional_gzip_by_response_header() {
        let _guard = crate::server::test_util::net_env_guard();
        let html = "<html><head><title>gz ok</title></head><body><p>Enough real text on this page to clear the sufficiency gate without any help at all.</p></body></html>";
        let gz = gzip_stored(html.as_bytes());
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        response.extend_from_slice(&gz);
        let port = raw_response_fixture(response).await;
        let got = http_fetch(
            &format!("http://127.0.0.1:{port}/"),
            false,
            None,
            OutputFormat::Markdown,
            None,
            &[],
            50000,
        )
        .await
        .unwrap()
        .expect("gunzipped page must be accepted");
        assert_eq!(
            got.title.as_deref(),
            Some("gz ok"),
            "body must be gunzipped before parsing"
        );
    }
}
