use crate::browser::Browser;
use diting::diting_net::CookieJar;
use crate::{
    ClickRequest, ClickResponse, EvalRequest, EvalResponse, FetchRequest, FetchResponse,
    OutputFormat, SearchRequest, SearchResponse,
};
#[cfg(feature = "screenshot")]
use crate::{ScreenshotRequest, ScreenshotResponse};
use anyhow::{Context, Result};
use std::sync::Arc;

/// Error type for /search (separate from anyhow so we can map to HTTP status).
#[derive(Debug)]
pub enum SearchError {
    /// Internal error → 500
    Other(String),
    /// Caller error (unknown engine name, bad time_range) → 400
    BadRequest(String),
}

/// Build a browser instance.
/// `use_proxy` decides whether the upstream `AGINXBROWSER_PROXY` is applied. Domestic
/// sites should pass `false` (direct is faster and SOCKS5 often times out);
/// foreign sites that are blocked/unreachable directly pass `true`.
///
/// Auto-detection: if the target URL matches a known blocked domain, proxy is
/// used regardless of `use_proxy` flag (the site is unreachable without proxy).
pub fn build_browser(use_proxy: bool, url: &str, tls_fingerprint: Option<&str>) -> Result<Browser> {
    build_browser_with_jar(use_proxy, url, tls_fingerprint, true)
}

/// Process-global cookie jar shared by every stateless request handler. A
/// fresh incognito profile per request is a CAPTCHA magnet — anti-bot
/// systems score "first-ever visitor" traffic hardest, so reusing cookies
/// from prior visits (baidu/wappass tokens, cf_clearance-style grants)
/// measurably cuts challenge rates on repeat URLs.
static SHARED_COOKIE_JAR: std::sync::LazyLock<Arc<CookieJar>> = std::sync::LazyLock::new(|| {
    let jar = Arc::new(CookieJar::new());
    if crate::config::ephemeral() {
        return jar;
    }
    let path = cookie_store_path();
    match jar.load_from_file(&path) {
        Ok(n) if n > 0 => {
            tracing::info!("restored {} cookies from {}", n, path.display());
        }
        // A corrupt or unreadable store must not silently reset (obscura#855
        // item 1 shape): the user would come back logged-out with no clue why.
        Ok(_) => {}
        Err(e) => tracing::warn!("cookie store load failed, starting fresh: {}", e),
    }
    jar
});

fn cookie_store_path() -> std::path::PathBuf {
    let dir = std::env::var("AGINXBROWSER_COOKIE_STORE_DIR")
        .ok()
        .or_else(|| crate::config::app_data_dir().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string());
    std::path::PathBuf::from(dir).join("cookie-store.json")
}

/// Persist the shared jar (best-effort; called after stateless requests).
pub fn persist_shared_cookies() {
    if crate::config::ephemeral() {
        return;
    }
    let path = cookie_store_path();
    if let Err(e) = SHARED_COOKIE_JAR.save_to_file(&path) {
        tracing::warn!("cookie store save failed: {}", e);
    }
}

/// [`build_browser`] variant that can opt out of the shared cookie jar
/// (isolation-sensitive flows pass `false`).
pub fn build_browser_with_jar(
    use_proxy: bool,
    url: &str,
    tls_fingerprint: Option<&str>,
    share_cookies: bool,
) -> Result<Browser> {
    // Stealth defaults on; disable via AGINXBROWSER_STEALTH=0 (diagnostic / when
    // the wreq stealth client misbehaves on a given site).
    let stealth = !matches!(
        std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(),
        Some("0")
    );
    let mut builder = Browser::builder().stealth(stealth);
    if share_cookies {
        builder = builder.shared_cookie_jar(SHARED_COOKIE_JAR.clone());
    }
    if let Some(fp) = tls_fingerprint {
        builder = builder.tls_fingerprint(fp);
    }
    if crate::config::should_auto_proxy(url) || use_proxy {
        if let Some(proxy) = crate::config::proxy_from_env() {
            builder = builder.proxy(&proxy);
        }
    }
    Ok(builder.build()?)
}

/// [`build_browser`] variant bound to an account's private jar (account.rs).
/// Same shape as the shared-jar wiring, but the jar belongs to one named
/// login identity: concurrent same-account sessions share it (two tabs, one
/// profile), and nothing an account does ever lands in the anonymous
/// shared jar or another account's. `user_agent` is the identity's persona
/// UA — it reaches the HTTP transport and `navigator.userAgent` alike, and
/// the TLS emulation OS follows it, so the whole device shape stays
/// coherent.
pub fn build_browser_for_account(
    use_proxy: bool,
    url: &str,
    tls_fingerprint: Option<&str>,
    jar: std::sync::Arc<CookieJar>,
    user_agent: Option<&str>,
) -> Result<Browser> {
    let stealth = !matches!(
        std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(),
        Some("0")
    );
    let mut builder = Browser::builder().stealth(stealth).shared_cookie_jar(jar);
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua);
    }
    if let Some(fp) = tls_fingerprint {
        builder = builder.tls_fingerprint(fp);
    }
    if crate::config::should_auto_proxy(url) || use_proxy {
        if let Some(proxy) = crate::config::proxy_from_env() {
            builder = builder.proxy(&proxy);
        }
    }
    Ok(builder.build()?)
}

/// Test-only handle on the process-global jar — the multi-account isolation
/// tests assert account jars never leak into it.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn shared_cookie_jar_for_tests() -> std::sync::Arc<CookieJar> {
    SHARED_COOKIE_JAR.clone()
}

/// Native stack size for threads that host a V8 isolate. V8 counts its JS
/// frames against the hosting thread's native stack — minified SPA bundles
/// (juejin.cn class) recurse past what a default 2 MB thread survives
/// (`RangeError: Maximum call stack size exceeded` before the page renders).
/// Sized in config (`AGINXBROWSER_JS_STACK_MB`, default 32 MB); V8's own
/// ceiling is raised to match — see diting_js::runtime.
pub(crate) fn v8_stack_size() -> usize {
    crate::config::js_stack_mb() * 1024 * 1024
}

/// Run a browser operation on a dedicated single-threaded runtime.
///
/// The V8 runtime holds `Rc<RefCell<…>>` state, which is `!Send`, so a
/// `Page` cannot be held across `.await` points on Tokio's multi-threaded
/// runtime. We spin up a current-thread runtime on a blocking thread and drive
/// the whole navigation there — the V8 isolate stays on one thread for its
/// entire lifetime, which is what deno_core expects. That thread gets a deep
/// native stack (`v8_stack_size`): callers reach us from tokio blocking
/// threads whose default 2 MB stack is too shallow for V8-heavy pages.
pub(crate) fn run_on_local_runtime<F, T>(f: F) -> Result<T>
where
    F: for<'a> FnOnce(
            &'a tokio::runtime::Runtime,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + 'a>>
        + Send
        + 'static,
    T: Send + 'static,
{
    let handle = std::thread::Builder::new()
        .stack_size(v8_stack_size())
        .name("v8-page".to_string())
        .spawn(move || run_on_local_runtime_on_thread(f))
        .context("failed to spawn V8 runtime thread")?;
    handle
        .join()
        .unwrap_or_else(|panic| Err(anyhow::anyhow!("V8 runtime thread panicked: {panic:?}")))
}

fn run_on_local_runtime_on_thread<F, T>(f: F) -> Result<T>
where
    F: for<'a> FnOnce(
            &'a tokio::runtime::Runtime,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + 'a>>
        + Send
        + 'static,
    T: Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, f(&runtime));
    // Drop the page/browser inside the LocalSet + runtime context so V8 cleanup
    // happens on the owning thread.
    drop(local);
    drop(runtime);
    result
}

/// Inject request-supplied cookies into the browser's cookie jar before
/// navigation. Entries are `name=value` pairs scoped to the target URL's
/// host, or full Set-Cookie strings anchored at their own `Domain` —
/// needed for sites (e.g. WeChat articles, taobao shops) that gate
/// content behind a logged-in session cookie.
pub(crate) fn inject_cookies(browser: &Browser, cookies: &[String], target_url: &str) {
    if cookies.is_empty() {
        return;
    }
    tracing::debug!(
        "inject_cookies: {} cookies for {}",
        cookies.len(),
        target_url
    );
    let store = browser.cookies();
    for c in cookies {
        let (full, anchor) = normalize_cookie_entry(c, target_url);
        let _ = store.set(&full, &anchor);
    }
}

/// Split a caller-supplied cookie entry into its full Set-Cookie form and
/// the URL to anchor it at. The jar validates `Domain` against the anchor
/// host (RFC 6265 §5.3), so an entry declaring `Domain=.tmall.com` must
/// anchor at tmall.com — anchoring it at the taobao.com page being opened
/// gets it silently dropped, which breaks exactly the cross-subdomain
/// restore that login-state injection exists for. Bare `name=value`
/// entries keep anchoring at the target URL's host.
pub(crate) fn normalize_cookie_entry(entry: &str, target_url: &str) -> (String, String) {
    let lower = entry.to_ascii_lowercase();
    if lower.contains("domain=") || lower.contains("path=") {
        let anchor = cookie_domain_attr(entry)
            .map(|d| format!("https://{}/", d.trim_start_matches('.')))
            .unwrap_or_else(|| target_url.to_string());
        (entry.to_string(), anchor)
    } else {
        let host = url::Url::parse(target_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        (
            format!("{}; Domain={}; Path=/", entry, host),
            target_url.to_string(),
        )
    }
}

fn cookie_domain_attr(set_cookie: &str) -> Option<String> {
    set_cookie.split(';').skip(1).find_map(|attr| {
        let (k, v) = attr.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("domain")
            .then(|| v.trim().to_string())
    })
}

/// Deserialize a `cookies` field whose entries are either bare
/// `"name=value"` strings (the original shape) or CDP-style objects
/// `{"name","value","domain","path","secure","httpOnly","sameSite"}`.
/// Browser-exported login state arrives in the object shape, and a
/// string-only field silently drops every attribute except the value —
/// the domain most of all. Objects become full Set-Cookie strings here
/// so every downstream consumer (which all speak Set-Cookie) is
/// unaffected.
pub(crate) fn cookie_list_from_json<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    raw.into_iter()
        .map(|v| -> Result<String, D::Error> {
            let full = match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Object(m) => {
                    let name = m
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty());
                    let value = m.get("value").and_then(|v| v.as_str());
                    let (Some(name), Some(value)) = (name, value) else {
                        return Err(serde::de::Error::custom(format!(
                            "cookie object needs string \"name\" and \"value\", got keys: {:?}",
                            m.keys().collect::<Vec<_>>()
                        )));
                    };
                    let mut full = format!("{name}={value}");
                    for (key, attr) in [
                        ("domain", "Domain"),
                        ("path", "Path"),
                        ("sameSite", "SameSite"),
                    ] {
                        if let Some(val) = m
                            .get(key)
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            full.push_str(&format!("; {attr}={val}"));
                        }
                    }
                    for (key, attr) in [("secure", "Secure"), ("httpOnly", "HttpOnly")] {
                        if m.get(key).and_then(|v| v.as_bool()) == Some(true) {
                            full.push_str(&format!("; {attr}"));
                        }
                    }
                    full
                }
                other => {
                    return Err(serde::de::Error::custom(format!(
                        "cookie entries must be strings or objects, got: {other}"
                    )))
                }
            };
            Ok(full)
        })
        .collect()
}

/// Check if the current page is a Cloudflare challenge.
fn is_cloudflare_challenge(page: &mut crate::page::Page) -> bool {
    let title_val = page.evaluate("document.title");
    let title = title_val.as_str().unwrap_or("");
    if title.contains("Just a moment") || title.contains("Attention Required") {
        return true;
    }
    let has_turnstile_val =
        page.evaluate(r#"!!document.querySelector('iframe[src*="challenges.cloudflare.com"]')"#);
    has_turnstile_val.as_bool().unwrap_or(false)
}

/// After goto(), detect and auto-bypass Cloudflare Turnstile challenges.
/// Waits for `cf_clearance` cookie, then re-navigates if the page hasn't
/// auto-redirected.
pub(crate) async fn maybe_bypass_challenge(page: &mut crate::page::Page) -> Result<()> {
    if !is_cloudflare_challenge(page) {
        return Ok(());
    }
    let url = page.url();
    tracing::info!(
        "Cloudflare challenge detected at {}, auto-bypassing...",
        url
    );

    // Give Turnstile JS time to execute (managed challenge auto-completes).
    page.settle(5000).await;

    // Wait for cf_clearance cookie (the signal that Turnstile passed).
    match page
        .wait_for_cookie("cf_clearance", std::time::Duration::from_secs(25))
        .await
    {
        Ok(()) => {
            tracing::info!("cf_clearance cookie received, challenge passed");
            // If the page didn't auto-redirect, re-navigate.
            if is_cloudflare_challenge(page) {
                tracing::info!("Re-navigating to {} after challenge pass", url);
                page.goto(&url).await?;
                page.settle(3000).await;
            }
        }
        Err(e) => {
            tracing::warn!("cf_clearance timeout: {}", e);
            // Don't fail hard — the page might still have usable content
            // (e.g. invisible challenge that completed without cookie).
        }
    }
    Ok(())
}

/// Check if the current page is a bytedance WAF JS challenge (juejin.cn
/// class): a tiny stub whose `<body onload="readygo()">` brute-forces a
/// SHA256 PoW over 1ms ticks and reloads with a `_wafchallengeid` answer
/// cookie. Signature: a global `readygo` function + almost no visible text
/// (real pages don't define that global; challenge stubs carry no content).
fn is_byte_waf_challenge(page: &mut crate::page::Page) -> bool {
    let val = page.evaluate(
        "(function() {\
            try {\
                if (!document.body) return false;\
                var text = (document.body.innerText || '').replace(/\\s+/g, '');\
                return text.length < 100 && typeof window.readygo === 'function';\
            } catch (e) { return false; }\
        })()",
    );
    val.as_bool().unwrap_or(false)
}

/// After goto(), ride out the bytedance WAF JS challenge. The challenge
/// solves itself in-page — readygo() sets the answer cookie, then calls
/// location.reload() — but the reload is recorded as a pending JS
/// navigation that nobody in the stateless fetch path drains (sessions
/// have a command-loop pump; do_fetch doesn't), and the PoW ticks need
/// event-loop pumping that the caller's default 0s settle never gives.
/// So: pump, drain the reload, settle the real page.
pub(crate) async fn maybe_bypass_byte_waf(page: &mut crate::page::Page) -> Result<()> {
    if !is_byte_waf_challenge(page) {
        return Ok(());
    }
    let url = page.url();
    tracing::info!(
        "byte-WAF challenge detected at {}, waiting for PoW + reload",
        url
    );

    // Pump so readygo's 1ms interval ticks (the PoW is trivially small —
    // observed answers are single/low double digits) and the reload lands
    // in pending_navigation.
    page.settle(2000).await;

    // Drain the JS-initiated reload; the re-navigation runs load events
    // again, this time with the answer cookie attached by the client.
    match page.process_pending_navigation().await {
        Ok(true) => {
            page.settle(1500).await;
        }
        Ok(false) => {
            // No reload recorded — either it fired inside settle's own
            // navigation handling or the solver failed. Re-check below.
        }
        Err(e) => {
            tracing::warn!("byte-WAF reload navigation failed: {}", e);
        }
    }

    if is_byte_waf_challenge(page) {
        tracing::warn!("byte-WAF challenge at {} still present after settle", url);
    }
    Ok(())
}

/// Read the rendered text content from the live DOM (after JS has run).
/// When `selector` is given, return that element's innerText; otherwise the
/// whole body. This reflects JS-filled content (WeChat/SPA), unlike parsing
/// the initial HTML snapshot.
fn rendered_text(page: &mut crate::page::Page, selector: Option<&str>) -> String {
    // innerText now carries rendered-text semantics itself (script/style/
    // display:none excluded, block boundaries as newlines), so no DOM
    // mutation is needed here — the old workaround blanked script text by
    // destructively rewriting textContent before reading.
    let js = match selector {
        Some(sel) => {
            let escaped = sel
                .replace('\\', "\\\\")
                .replace('`', "\\`")
                .replace('$', "\\$");
            format!("(function(){{var el=document.querySelector(`{escaped}`);return el?el.innerText:'';}})()")
        }
        None => {
            // WeChat articles ship the full body server-rendered inside
            // #js_content but keep it visibility:hidden until their module JS
            // reveals it — body.innerText is then just the title/byline shell
            // (v0.3.1 report P1-3: 133 chars where the article is 981). Per
            // spec a not-rendered element's own innerText is its textContent,
            // so the container read still carries the article. Whichever
            // extraction is richer wins: once the reveal does run, body
            // reclaims the article (plus the byline lines) and matches anyway.
            "(function(){var b=document.body?document.body.innerText:'';\
             var c=document.querySelector('#js_content');var a=c?c.innerText:'';\
             return (a.trim().length>b.trim().length)?a:b;})()"
                .to_string()
        }
    };
    let raw = page.evaluate(&js).as_str().unwrap_or("").to_string();
    // Collapse runs of whitespace (heavy SPA pages produce lots of blank
    // lines from empty layout containers) — keeps the output tight.
    collapse_whitespace(&raw)
}

/// Collapse runs of >=3 whitespace chars (spaces/tabs/newlines) into a single
/// blank line, and trim each line. Keeps readable paragraph breaks without the
/// hundreds of empty lines SPA layouts inject.
fn collapse_whitespace(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Same as fetch_url_text but injects search-session cookies before navigation.
/// Needed for sogou WeChat /link redirect URLs which require the sogou session
/// cookie to pass the antispider check.
fn fetch_url_text_with_cookies(
    url: String,
    use_proxy: bool,
    wait_secs: u64,
    max_chars: usize,
    cookies: &[String],
) -> Result<(String, bool)> {
    let cookies = cookies.to_vec(); // Clone so the closure owns the data.
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            let browser = build_browser(use_proxy, &url, None)?;
            if !cookies.is_empty() {
                inject_cookies(&browser, &cookies, &url);
            }
            let mut page = browser.new_page().await?;
            page.goto(&url).await?;

            // Auto-bypass Cloudflare Turnstile challenge if detected.
            maybe_bypass_challenge(&mut page).await?;

            // Ride out bytedance WAF JS challenges (juejin.cn class).
            maybe_bypass_byte_waf(&mut page).await?;

            if wait_secs > 0 {
                page.settle(wait_secs * 1000).await;
            }

            // Check if we landed on an antispider/CAPTCHA page.
            let final_url = page.url();
            tracing::info!("fetch_url_text: {} -> final_url={}", url, final_url);
            let is_antispider = final_url.contains("/antispider")
                || final_url.contains("wappass.baidu.com")
                || final_url.contains("sorry.google.com")
                || final_url.contains("challenge-platform");
            let mut content = rendered_text(&mut page, None);
            // Search bodies always get the stripper — there is no per-item
            // knob to turn it off, and a result list is exactly where a
            // poisoned page meets a model with no human in the loop.
            let hidden = crate::sanitize::parse_hidden_spans(
                &page.evaluate(crate::sanitize::HIDDEN_SPAN_PROBE),
            );
            content = crate::sanitize::sanitize_text(&content, &hidden).0;

            // If we landed on an antispider/CAPTCHA page, treat it as an error
            // rather than returning the CAPTCHA page content as search result body.
            if is_antispider {
                return Err(anyhow::anyhow!(
                    "CAPTCHA/antispider page detected at {}",
                    final_url
                ));
            }

            let (content, truncated) = if max_chars > 0 && content.chars().count() > max_chars {
                (content.chars().take(max_chars).collect::<String>(), true)
            } else {
                (content, false)
            };
            Ok((content, truncated))
        })
    })
}

/// Evaluate a JS expression on a page, retrying until it returns non-null
/// or the timeout expires. Used for extracting `window.__INITIAL_STATE__`
/// and similar JS globals from SPA pages.
fn extract_js_global(
    page: &mut crate::page::Page,
    expression: &str,
    timeout_ms: u64,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut interval = 200u64;
    loop {
        let js = format!(
            "(function() {{ try {{ var r = {}; return r == null ? null : (typeof r === 'object' ? JSON.stringify(r) : r); }} catch(e) {{ return null; }} }})()",
            expression
        );
        let val = page.evaluate(&js);
        if !val.is_null() {
            // If the value is a string containing JSON, parse it.
            if let Some(s) = val.as_str() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                    return parsed;
                }
                return serde_json::Value::String(s.to_string());
            }
            return val;
        }
        if std::time::Instant::now() >= deadline {
            return serde_json::Value::Null;
        }
        // Synchronous sleep — we're inside run_on_local_runtime.
        std::thread::sleep(std::time::Duration::from_millis(interval));
        interval = (interval * 2).min(2000);
        // Also pump the JS event loop.
        let _ = page.evaluate("1+1");
    }
}

/// Fetch a page and return content in the requested format.
pub fn do_fetch(req: FetchRequest) -> Result<FetchResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            page.goto(&req.url).await?;

            // Auto-bypass Cloudflare Turnstile challenge if detected.
            if req.auto_bypass_challenge {
                maybe_bypass_challenge(&mut page).await?;
            }

            // Ride out bytedance WAF JS challenges (juejin.cn class) —
            // unconditional, like the Cloudflare path in search fetches.
            maybe_bypass_byte_waf(&mut page).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            // Title: prefer a visible article-title element (WeChat's
            // #activity-name), then document.title, then og:title meta.
            let title = page
                .evaluate(
                    "((document.querySelector('#activity-name,h1,.article-title')||{}).textContent||'').trim()\
                     || document.title\
                     || (document.querySelector('meta[property=\"og:title\"]')||{}).content\
                     || ''",
                )
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            // Source the content from the RENDERED DOM, not the initial HTML
            // snapshot. On heavy SPA pages (WeChat: 6.6MB shell) the article
            // body is filled in by JS and sits deep in document.documentElement
            // .outerHTML — converting the whole shell to markdown then
            // truncating to max_chars would cut the body off entirely.
            // body.innerText (after settle/wait) is the already-rendered text.
            // Content extraction, with the injection stripper on the
            // text/markdown path (raw HTML is never sanitized — the caller
            // asked for the bytes as they are).
            let (content, sanitize_report) = match req.format {
                OutputFormat::Html => (page.content(), None),
                OutputFormat::Text | OutputFormat::Markdown => {
                    let raw = rendered_text(&mut page, req.selector.as_deref());
                    if !req.sanitize {
                        (raw, None)
                    } else {
                        // Probe while the page is still alive: text that a
                        // human can't see (opacity 0 / sub-4px font) but
                        // innerText happily carries.
                        let hidden = crate::sanitize::parse_hidden_spans(
                            &page.evaluate(crate::sanitize::HIDDEN_SPAN_PROBE),
                        );
                        let (clean, report) = crate::sanitize::sanitize_text(&raw, &hidden);
                        let report = if report.is_clean() {
                            None
                        } else {
                            Some(report)
                        };
                        (clean, report)
                    }
                }
            };

            // Truncate to max_chars (0 = unlimited). Keeps huge pages from
            // blowing up a downstream LLM context window.
            let (content, truncated) =
                if req.max_chars > 0 && content.chars().count() > req.max_chars {
                    let cut: String = content.chars().take(req.max_chars).collect();
                    (cut, true)
                } else {
                    (content, false)
                };

            // JS extraction: evaluate the user-specified expression after page
            // has settled and content is extracted.
            let js_extract_result = req
                .js_extract
                .as_ref()
                .map(|cfg| extract_js_global(&mut page, &cfg.expression, cfg.timeout_ms));

            // CAPTCHA detection and optional auto-solve.
            let captcha_event = {
                let final_url = page.url();
                let html_snapshot = page.content();
                crate::captcha::detect_and_maybe_solve(&final_url, &html_snapshot).await
            };

            // Background API capture (capture_xhr): the page's own XHR/fetch
            // responses — usually the clean structured face, an order of
            // magnitude cheaper to read than the rendered DOM. Per-body cap
            // rides `max_chars` (the LLM-context knob), bounded so 20 bodies
            // can't each claim the full default.
            let xhr = if let Some(filters) = req.capture_xhr.as_ref() {
                page.inner.sync_js_network_events();
                let body_cap = req.max_chars.min(8_000);
                crate::har::xhr_bodies(&page.inner.network_events, filters, body_cap, &|rid| {
                    page.inner.get_response_body(rid)
                })
            } else {
                Vec::new()
            };

            Ok(FetchResponse {
                url: page.url(),
                title,
                content,
                truncated,
                captcha_event,
                js_extract_result,
                tier: Some("browser"),
                redirected_from: Vec::new(),
                sanitize_report,
                xhr,
            })
        })
    })
}

/// Click an element by CSS selector using JS `element.click()`.
pub fn do_click(req: ClickRequest) -> Result<ClickResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let clicked = if let Some(el) = page.query_selector(&req.selector) {
                el.click().context("element.click() failed")?;
                true
            } else {
                false
            };

            page.settle(500).await;
            // A submit click routes through the bootstrap form glue and
            // lands as a pending JS navigation; draining it fires the POST
            // and moves the page to the action URL (obscura #618 class —
            // the CDP layer drains after clicks, this is the stateless
            // surface's equivalent).
            if page.process_pending_navigation().await.unwrap_or(false) {
                // The landed document needs its own event-loop slice before
                // url/innerText are read.
                page.settle(800).await;
            }
            let text_after = page
                .evaluate("document.body.innerText")
                .as_str()
                .map(|s| s.to_string());

            Ok(ClickResponse {
                url: page.url(),
                selector: req.selector,
                clicked,
                text_after,
            })
        })
    })
}

/// Evaluate arbitrary JavaScript on the page.
pub fn do_eval(req: EvalRequest) -> Result<EvalResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let result = page.evaluate_async(&req.script).await;
            // A location.href-style navigation in the script lands as a
            // pending JS navigation — drain so the reported url follows it
            // (mirrors the CDP Runtime.evaluate path, which drains post-eval).
            if page.process_pending_navigation().await.unwrap_or(false) {
                page.settle(500).await;
            }

            Ok(EvalResponse {
                url: page.url(),
                result,
            })
        })
    })
}

/// /screenshot: render the JS-rendered DOM of a page to a PNG via inlined Blitz.
///
/// Unlike /fetch (which can short-circuit to raw HTTP for static pages), this
/// always drives the diting browser so SPA/JS-rendered content is captured.
/// The page's `document.documentElement.outerHTML` is then fed to Blitz for
/// layout + paint — no Chromium. Sub-resources (images, head stylesheets) are
/// pre-fetched through the page's own HTTP client (same cookies/UA/proxy, plus
/// stealth TLS when enabled) and served to Blitz synchronously by
/// PrefetchedNetProvider; misses answer empty so nothing blocks paint.
#[cfg(feature = "screenshot")]
pub fn do_screenshot(req: ScreenshotRequest) -> Result<ScreenshotResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport before navigation (same as /video and /pdf):
            // JS-time layout (media queries) — and a bare SVG document's
            // root intrinsic — must agree with the requested render size.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            // JS-rendered DOM — the same source /fetch uses for OutputFormat::Html.
            let html = page.content();
            // Pre-fetch while the page (its cookie'd HTTP client) is still alive.
            let resources = crate::screenshot::prefetch_render_resources(
                &page,
                &final_url,
                &html,
                req.width as f32,
            )
            .await;
            drop(page);
            drop(browser);

            // Render off-thread-ish: layout/paint is sync and CPU-bound.
            // We're already on a blocking runtime thread, so just call it directly.
            // Default engine is diting — our own css+layout+paint stack (no
            // Stylo/vello/parley in the path). engine=blitz opts back into
            // the Blitz reference pipeline for comparison renders — only
            // available when compiled with the `blitz-reference` feature.
            let rendered = if req.engine.as_deref() == Some("blitz") {
                #[cfg(feature = "blitz-reference")]
                {
                    crate::screenshot_reference::render_html_to_png(
                        &html,
                        &final_url,
                        req.width,
                        req.height,
                        req.scale,
                        req.full_page,
                        req.selector.as_deref(),
                        req.selector_all,
                        Some(&resources),
                    )?
                }
                #[cfg(not(feature = "blitz-reference"))]
                {
                    anyhow::bail!(
                        "engine=\"blitz\" requires the blitz-reference feature (this build renders with diting only)"
                    );
                }
            } else {
                crate::screenshot::render_html_to_png_diting(
                    &html,
                    &final_url,
                    req.width,
                    req.height,
                    req.scale,
                    req.full_page,
                    req.selector.as_deref(),
                    req.selector_all,
                    Some(&resources),
                )?
            };

            // Computed before the response struct moves `final_url`.
            let css_urls = crate::screenshot::stylesheet_hrefs(&html, &final_url);
            Ok(ScreenshotResponse {
                url: final_url,
                title,
                width: rendered.pixel_width,
                height: rendered.pixel_height,
                image_base64: base64_png(&rendered.png),
                format: "png".to_string(),
                selector_rects: if req.selector.is_some() {
                    Some(rendered.rects)
                } else {
                    None
                },
                selector_rects_diting: match req.selector.as_deref().filter(|_| req.diting_rects) {
                    Some(sel) => {
                        // css_urls computed above the response struct (before
                        // `final_url` moved) — link hrefs resolved absolute,
                        // not a `.css` suffix guess (MediaWiki's load.php
                        // sheets have no extension).
                        crate::screenshot::element_rects_diting(
                            &html,
                            sel,
                            req.selector_all,
                            req.width as f32,
                            req.height as f32,
                            // External <link> sheet bodies the prefetch pass already
                            // fetched — feed them to diting so its cascade sees what
                            // Blitz saw. Inline <style> blocks come from the HTML.
                            Some(
                                &resources
                                    .iter()
                                    .filter(|(k, v)| {
                                        !v.is_empty()
                                            && (css_urls.contains(k.as_str())
                                                || k.ends_with(".css"))
                                    })
                                    .map(|(_, v)| String::from_utf8_lossy(v.as_ref()).into_owned())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                        )
                        .ok()
                    }
                    None => None,
                },
            })
        })
    })
}

#[cfg(feature = "screenshot")]
fn base64_png(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(bytes)
}

/// /video: render the page's registered timelines (window.__timelines —
/// GSAP-style objects with `duration()` + `pause(t)`) to an MP4.
///
/// Unlike /screenshot, which extracts outerHTML and renders offline, this
/// needs the LIVE page: the timelines live in the page's own V8 isolate, so
/// the seek loop (pause(t) → viewport band paint → ffmpeg stdin) runs while
/// the browser is still up. Requires ffmpeg on PATH. See src/video.rs for
/// the protocol.
#[cfg(feature = "screenshot")]
pub fn do_video(req: crate::VideoRequest) -> Result<crate::VideoResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport so JS-time layout (media queries) and the band
            // paint below agree on the requested size.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            page.goto(&req.url).await?;

            let opts = crate::video::TimelineVideoOptions {
                fps: req.fps,
                viewport: (req.width as f32, req.height as f32),
                hold_tail_secs: req.hold_tail_secs,
                max_duration_secs: req.max_duration_secs,
                wait_timelines: std::time::Duration::from_millis(req.wait_timelines_ms),
                audio: req.audio.map(|a| crate::video::AudioTrack {
                    url: a.url,
                    volume: a.volume,
                    fade_out_secs: a.fade_out_secs,
                    loop_audio: a.loop_audio,
                }),
                narration: req
                    .narration
                    .into_iter()
                    .map(|c| crate::video::NarrationClip {
                        url: c.url,
                        start_secs: c.start_secs,
                        volume: c.volume,
                    })
                    .collect(),
                subtitles_srt: req.subtitles_srt,
                subtitles_language: req.subtitles_language,
                burn_subtitles: req.burn_subtitles,
            };
            let video = crate::video::render_timeline_video(&mut page.inner, &opts).await?;
            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            Ok(crate::VideoResponse {
                url: final_url,
                title,
                frames: video.frames,
                timeline_secs: video.timeline_secs,
                duration_secs: video.duration_secs,
                width: video.width,
                height: video.height,
                video_base64: base64_png(&video.mp4),
                has_audio: video.has_audio,
                has_subtitles: video.has_subtitles,
                burned_subtitles: video.burned_subtitles,
                format: "mp4".to_string(),
            })
        })
    })
}

/// /pdf: cut the page into a set of pages and package as PDF (default),
/// per-page PNGs, image-based PPTX/DOCX, or native (editable) PPTX — the
/// logical page-slicing layer (print pagination at block boundaries, or one
/// page per `selector` match in slides mode). Same live-page shape as
/// /video: the geometry comes from the live tree's layout, so the whole run
/// happens while the browser is up. See src/pages.rs, src/ooxml.rs, and
/// src/pptx_native.rs.
#[cfg(feature = "screenshot")]
pub fn do_pdf(req: crate::PdfRequest) -> Result<crate::PdfResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport so JS-time layout and the band paints agree
            // on the requested page width.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            // Print-media export semantics (what Chrome's save-as-PDF runs
            // under): decks that park their page-per-slide layout in an
            // `@media print` block un-stack here — the flex-row +
            // translateX carousel renders one slide per page instead of the
            // overflow-clipped screen frame. No-op for pages without a
            // print block. Realm replay carries it past the navigation.
            page.inner
                .set_emulated_media(None, Some(Some("print".into())));
            page.goto(&req.url).await?;

            // Native PPTX branches BEFORE the page-set render: its walker
            // collects element rects itself — painting page bands first
            // would be wasted work. Slides semantics only (one slide per
            // selector match), so a missing selector is an explicit error.
            if req.format.eq_ignore_ascii_case("pptx-native") {
                let selector = req.selector.clone().ok_or_else(|| {
                    anyhow::anyhow!("format=pptx-native requires `selector` (one slide per match)")
                })?;
                let (bytes, slides) =
                    crate::pptx_native::pptx_native_deck(&mut page.inner, &selector, req.max_pages)
                        .await?;
                return Ok(crate::PdfResponse {
                    url: page.url(),
                    title: {
                        let v = page.evaluate("document.title");
                        v.as_str()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    },
                    pages: slides,
                    width: req.width,
                    height: req.height,
                    pdf_base64: None,
                    pages_base64: Vec::new(),
                    pptx_base64: Some(base64_png(&bytes)),
                    docx_base64: None,
                    format: "pptx-native".to_string(),
                });
            }

            let mode = match req.selector.as_deref() {
                Some(sel) => crate::pages::PageMode::Slides(sel.to_string()),
                None => crate::pages::PageMode::Print,
            };
            // Only the PDF path consumes the vector text layer — collecting
            // it strips vectorizable text from the band raster, so the
            // raster-only formats must not turn it on.
            let format = if req.format.eq_ignore_ascii_case("png") {
                "png"
            } else if req.format.eq_ignore_ascii_case("pptx") {
                "pptx"
            } else if req.format.eq_ignore_ascii_case("docx") {
                "docx"
            } else {
                "pdf"
            };
            let opts = crate::pages::PagePumpOptions {
                mode,
                page_size: (req.width as f32, req.height as f32),
                max_pages: req.max_pages,
                collect_text: format == "pdf",
            };
            let set = crate::pages::render_page_set(&mut page.inner, &opts).await?;
            tracing::debug!(
                "page set: {} pages, content {}x{}, break origins {:?}",
                set.pages.len(),
                set.content_size.0 as u32,
                set.content_size.1 as u32,
                set.pages
                    .iter()
                    .map(|p| p.origin_y as u32)
                    .collect::<Vec<_>>()
            );
            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            // PNG wants per-page PNGs; the other three formats all embed
            // per-page JPEGs, so they share one encode pass.
            let mut pngs: Vec<String> = Vec::new();
            let mut jpegs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            if format == "png" {
                for p in &set.pages {
                    let png = crate::pages::png_of(p.width, p.height, &p.rgba)
                        .map_err(anyhow::Error::msg)?;
                    pngs.push(base64_png(&png));
                }
            } else {
                for p in &set.pages {
                    let jpeg = crate::pages::jpeg_of(p.width, p.height, &p.rgba, req.jpeg_quality)
                        .map_err(anyhow::Error::msg)?;
                    jpegs.push((p.width, p.height, jpeg));
                }
            }
            let refs: Vec<(u32, u32, &[u8])> = jpegs
                .iter()
                .map(|(w, h, j)| (*w, *h, j.as_slice()))
                .collect();
            let pdf_refs: Vec<(u32, u32, &[u8], &[diting::diting_layout::paint::PdfOp])> = jpegs
                .iter()
                .zip(set.text_ops.iter())
                .map(|((w, h, j), ops)| (*w, *h, j.as_slice(), ops.as_slice()))
                .collect();
            let (pdf_base64, pptx_base64, docx_base64) = match format {
                "pptx" => (
                    None,
                    Some(base64_png(&crate::ooxml::pptx_of_pages(&refs))),
                    None,
                ),
                "docx" => (
                    None,
                    None,
                    Some(base64_png(&crate::ooxml::docx_of_pages(&refs))),
                ),
                "pdf" => (
                    Some(base64_png(&crate::pages::pdf_of_pages(&pdf_refs))),
                    None,
                    None,
                ),
                _ => (None, None, None),
            };

            Ok(crate::PdfResponse {
                url: final_url,
                title,
                pages: set.pages.len(),
                width: req.width,
                height: req.height,
                pdf_base64,
                pages_base64: pngs,
                pptx_base64,
                docx_base64,
                format: format.to_string(),
            })
        })
    })
}

/// Shared search engine registry. LazyLock so engine clients (reqwest/wreq)
/// are built once on first use.
pub(crate) static SEARCH_REGISTRY: std::sync::LazyLock<crate::search::SearchEngineRegistry> =
    std::sync::LazyLock::new(crate::search::SearchEngineRegistry::new);

/// Live per-engine suspension state for /doctor.
pub(crate) async fn search_engine_health() -> Vec<crate::search::EngineHealth> {
    SEARCH_REGISTRY.health_snapshot().await
}

/// Validate a SearchRequest's engine filter and time_range before dispatch.
///
/// v0.3.1 Windows report P1-1: `engines: ["baidu"]` (or a typo like
/// "wechat") could silently yield zero results in 0.0s — an unknown name
/// filtered the registry to nothing, or the named engine was benched by a
/// CAPTCHA, and the response looked identical to "no results found". Two
/// of those three failures are caller errors and get a 400 with the valid
/// vocabulary; the suspension case is surfaced per-engine in the response
/// (`engine_errors`) instead. Catalog comes from the caller so tests drive
/// it against mock registries.
fn validate_search_request(
    catalog: &[(String, Vec<String>)],
    req: &crate::SearchRequest,
) -> Result<(), String> {
    if let Some(tr) = req.time_range.as_deref() {
        if crate::search::SearchTimeRange::parse(tr).is_none() {
            return Err(format!(
                "invalid time_range {tr:?}: expected one of day, week, month, year"
            ));
        }
    }
    if req.engines.is_empty() {
        return Ok(());
    }
    let requested: Vec<String> = req
        .categories
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    for name in &req.engines {
        match catalog.iter().find(|(n, _)| n == name) {
            None => {
                let valid: Vec<&str> = catalog.iter().map(|(n, _)| n.as_str()).collect();
                return Err(format!(
                    "unknown engine {name:?}; valid engines: {} (also check GET /engines)",
                    valid.join(", ")
                ));
            }
            Some((_, cats)) => {
                if !cats.iter().any(|c| requested.contains(c)) {
                    return Err(format!(
                        "engine {name:?} does not serve category {:?} (it serves: {})",
                        req.categories,
                        cats.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Rescue candidates for the fallback round (v0.3.2 Windows report #9):
/// the engines serving the requested categories that the caller did NOT
/// name. Non-empty only when an explicit `engines` filter errored in its
/// entirety — every named engine is in `engine_errors` — so a real
/// zero-hits answer from any surviving engine stands as-is and the
/// fallback never rewrites a deliberate engine choice that worked.
fn fallback_candidates(
    requested: &[String],
    categories: &str,
    engine_errors: &std::collections::BTreeMap<String, String>,
    catalog: &[(String, Vec<String>)],
) -> Vec<String> {
    if requested.is_empty() || !requested.iter().all(|e| engine_errors.contains_key(e)) {
        return Vec::new();
    }
    let cats: Vec<&str> = categories.split(',').map(|s| s.trim()).collect();
    catalog
        .iter()
        .filter(|(n, cs)| {
            !requested.iter().any(|r| r == n) && cs.iter().any(|c| cats.contains(&c.as_str()))
        })
        .map(|(n, _)| n.clone())
        .collect()
}

/// /search: native search across Baidu/Bing/Sogou/Google, optionally grab body for top N results.
pub async fn do_search(req: SearchRequest) -> Result<SearchResponse, SearchError> {
    do_search_with_registry(&SEARCH_REGISTRY, req).await
}

/// [`do_search`] over a caller-supplied registry (tests drive mock engine
/// sets through the full orchestration, fallback round included).
async fn do_search_with_registry(
    registry: &crate::search::SearchEngineRegistry,
    req: SearchRequest,
) -> Result<SearchResponse, SearchError> {
    validate_search_request(&registry.engine_catalog(), &req).map_err(SearchError::BadRequest)?;

    // Step 0: short-lived in-process cache (v0.3.1 Windows report P2-5: the
    // same query re-fired back-to-back paid the full ~12s every time). Same
    // TTL knob and eviction shape as the /fetch cache in main.rs.
    let cache_key = search_cache_key(&req);
    if let Some(cached) = search_cache_get(&cache_key) {
        return Ok(cached);
    }

    // Step 1: native search via built-in engines.
    let params = crate::search::SearchParams {
        language: req.language.clone(),
        pageno: 1,
        use_proxy: req.use_proxy,
        timeout_secs: 15,
        engine_filter: req.engines.clone(),
        time_range: req
            .time_range
            .as_deref()
            .and_then(crate::search::SearchTimeRange::parse),
    };

    let (mut items, _raw_total, mut captcha_events, mut engine_errors) =
        crate::search::native_search(registry, &req.q, params, &req.categories, req.max_results)
            .await;

    // Step 1.5: fallback round. An explicit engines filter that failed in
    // its entirety (every named engine errored, zero results) is usually a
    // site-specific wall — the same query over the rest of the category's
    // engines still answers. The substitution is disclosed via
    // `fallback_engines`, never silent (v0.3.2 Windows report #9).
    let mut fallback_engines = None;
    if items.is_empty() {
        let rescue = fallback_candidates(
            &req.engines,
            &req.categories,
            &engine_errors,
            &registry.engine_catalog(),
        );
        if !rescue.is_empty() {
            tracing::info!(
                "search: engines {:?} all failed, retrying over {:?}",
                req.engines,
                rescue
            );
            let fb_params = crate::search::SearchParams {
                language: req.language.clone(),
                pageno: 1,
                use_proxy: req.use_proxy,
                timeout_secs: 15,
                engine_filter: rescue.clone(),
                time_range: req
                    .time_range
                    .as_deref()
                    .and_then(crate::search::SearchTimeRange::parse),
            };
            let (fb_items, _t, fb_events, fb_errors) = crate::search::native_search(
                registry,
                &req.q,
                fb_params,
                &req.categories,
                req.max_results,
            )
            .await;
            engine_errors.extend(fb_errors);
            captcha_events.extend(fb_events);
            if !fb_items.is_empty() {
                items = fb_items;
                fallback_engines = Some(rescue);
            }
        }
    }

    // Step 2: optionally grab body for the top fetch_top results (concurrent).
    // Each fetch runs in its own blocking thread + current-thread runtime
    // (V8 is !Send), so spawn_blocking gives natural isolation + concurrency.
    // Cookies from the search session (e.g. sogou WeChat) are passed through
    // so the diting browser can authenticate redirect URLs.
    let n = req.fetch_top.min(items.len());
    if n > 0 {
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            // Image results: `url` is a binary image link, not a page — fetching
            // it as HTML is meaningless. Leave content as None for images.
            if items[i].image_url.is_some() {
                continue;
            }
            // robots.txt gates the body-grab too: /search must not become a
            // side door around the /fetch policy check. A denied item keeps
            // its result entry; only the content fetch is skipped, with the
            // reason in fetch_error so the agent can see why. Same for the
            // per-domain page budget — the reason text carries the stance.
            if let Err(reason) = crate::rate::check_domain(&items[i].url) {
                items[i].fetch_error = Some(reason);
                continue;
            }
            if let Err(reason) = crate::robots::assert_allowed(&items[i].url).await {
                items[i].fetch_error = Some(reason);
                continue;
            }
            let url = items[i].url.clone();
            let cookies = items[i].cookies.clone();
            let use_proxy = req.use_proxy;
            let wait = req.wait_secs;
            let max_chars = req.max_chars_per;
            if !cookies.is_empty() {
                tracing::debug!(
                    "do_search: item {} url={} has {} cookies",
                    i,
                    url,
                    cookies.len()
                );
            }
            handles.push(tokio::task::spawn_blocking(move || {
                (
                    i,
                    fetch_url_text_with_cookies(url, use_proxy, wait, max_chars, &cookies),
                )
            }));
        }
        for h in handles {
            let (i, res) = h
                .await
                .map_err(|e| SearchError::Other(format!("fetch task panicked: {e}")))?;
            match res {
                Ok((content, truncated)) => {
                    items[i].content = Some(content);
                    items[i].content_truncated = truncated;
                }
                Err(e) => {
                    items[i].fetch_error = Some(format!("{e}"));
                }
            }
        }
    }

    let resp = SearchResponse {
        query: req.q,
        // Post-merge, post-truncate count: equals results.len(). The old
        // pre-merge raw total drifted (19 vs 20 between identical queries)
        // and read as "max_results is ignored" to callers comparing it with
        // their cap (v0.3.1 Windows report P2-4).
        number_of_results: items.len(),
        results: items,
        captcha_events,
        engine_errors,
        fallback_engines,
    };
    // Cache successes and clean zeros only. A walled answer (0 results +
    // engine errors) is transient by nature — caching it for the full TTL
    // would keep serving the wall after it lifts (v0.3.2 Windows report:
    // the follow-up search hours later still paid for the morning's wall).
    if !resp.results.is_empty() || resp.engine_errors.is_empty() {
        search_cache_put(&cache_key, &resp);
    }
    Ok(resp)
}

/// Cache key: the request fields that change the response. Mirrors the
/// /fetch cache's shape.
fn search_cache_key(req: &SearchRequest) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
        req.q,
        req.categories,
        req.language,
        req.max_results,
        req.fetch_top,
        req.max_chars_per,
        req.wait_secs,
        req.use_proxy,
        req.time_range.as_deref().unwrap_or(""),
        req.engines,
    )
}

type SearchCache = std::sync::Mutex<std::collections::HashMap<String, (u64, SearchResponse)>>;

static SEARCH_CACHE: std::sync::LazyLock<SearchCache> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Max cached searches before eviction. Search responses (with fetch_top
/// bodies) are large, so this stays well under the /fetch cache's 256.
const SEARCH_CACHE_CAPACITY: usize = 64;

fn search_cache_get(key: &str) -> Option<SearchResponse> {
    let ttl = crate::cache_ttl_secs();
    if ttl == 0 {
        return None;
    }
    let now = crate::now_secs();
    let Ok(mut cache) = SEARCH_CACHE.lock() else {
        return None;
    };
    let (ts, resp) = cache.get(key)?;
    if now.saturating_sub(*ts) < ttl {
        Some(resp.clone())
    } else {
        cache.remove(key);
        None
    }
}

fn search_cache_put(key: &str, resp: &SearchResponse) {
    let ttl = crate::cache_ttl_secs();
    if ttl == 0 {
        return;
    }
    if let Ok(mut cache) = SEARCH_CACHE.lock() {
        if cache.len() >= SEARCH_CACHE_CAPACITY {
            let now = crate::now_secs();
            // First pass: drop expired entries.
            cache.retain(|_, (ts, _)| now.saturating_sub(*ts) < ttl);
            // Still over: evict the oldest entry.
            while cache.len() >= SEARCH_CACHE_CAPACITY {
                let oldest = cache
                    .iter()
                    .min_by_key(|(_, (ts, _))| *ts)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    cache.remove(&k);
                } else {
                    break;
                }
            }
        }
        cache.insert(key.to_string(), (crate::now_secs(), resp.clone()));
    }
}

/// Shared test plumbing for handler-level tests that need a real local
/// origin: an env guard that opens the private-network door under the same
/// process-wide lock diting_net uses, and a request-recording HTTP server.
#[cfg(test)]
pub(crate) mod test_util;

#[cfg(test)]
mod tests;
