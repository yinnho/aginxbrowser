use crate::{
    ClickRequest, ClickResponse, EvalRequest, EvalResponse, FetchRequest, FetchResponse,
    OutputFormat, SearchRequest, SearchResponse,
};
#[cfg(feature = "screenshot")]
use crate::{ScreenshotRequest, ScreenshotResponse};
use crate::browser::Browser;
use crate::diting_net::CookieJar;
use anyhow::{Context, Result};
use std::sync::Arc;

/// Error type for /search (separate from anyhow so we can map to HTTP status).
#[derive(Debug)]
pub enum SearchError {
    /// Internal error → 500
    Other(String),
}

/// Check if a URL points to a known foreign/blocked domain that requires proxy.
/// Uses suffix matching: `sub.github.com` matches `github.com`.
/// Returns `false` if URL parsing fails (safe fallback).
pub fn should_auto_proxy(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };

    // Known foreign domains that are blocked in China.
    // Suffix match: `raw.githubusercontent.com` matches `githubusercontent.com`.
    const BLOCKED_DOMAINS: &[&str] = &[
        "github.com",
        "githubusercontent.com",
        "github.io",
        "google.com",
        "google.co.jp",
        "googleapis.com",
        "googleusercontent.com",
        "wikipedia.org",
        "stackoverflow.com",
        "medium.com",
        "x.com",
        "twitter.com",
        "youtube.com",
        "reddit.com",
        "openai.com",
        "anthropic.com",
    ];

    for domain in BLOCKED_DOMAINS {
        if host == *domain || host.ends_with(&format!(".{}", domain)) {
            return true;
        }
    }
    false
}

/// Build a browser instance.
/// `use_proxy` decides whether the upstream `OBSCURA_PROXY` is applied. Domestic
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
static SHARED_COOKIE_JAR: std::sync::LazyLock<Arc<CookieJar>> =
    std::sync::LazyLock::new(|| {
        let jar = Arc::new(CookieJar::new());
        let path = cookie_store_path();
        if let Ok(n) = jar.load_from_file(&path) {
            if n > 0 {
                tracing::info!("restored {} cookies from {}", n, path.display());
            }
        }
        jar
    });

fn cookie_store_path() -> std::path::PathBuf {
    let dir = std::env::var("AGINXBROWSER_COOKIE_STORE_DIR")
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(dir).join("cookie-store.json")
}

/// Persist the shared jar (best-effort; called after stateless requests).
pub fn persist_shared_cookies() {
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
    let stealth = !matches!(std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(), Some("0"));
    let mut builder = Browser::builder().stealth(stealth);
    if share_cookies {
        builder = builder.shared_cookie_jar(SHARED_COOKIE_JAR.clone());
    }
    if let Some(fp) = tls_fingerprint {
        builder = builder.tls_fingerprint(fp);
    }
    if should_auto_proxy(url) || use_proxy {
        if let Ok(proxy) = std::env::var("OBSCURA_PROXY") {
            builder = builder.proxy(&proxy);
        }
    }
    Ok(builder.build()?)
}

/// Run a browser operation on a dedicated single-threaded runtime.
///
/// The V8 runtime holds `Rc<RefCell<…>>` state, which is `!Send`, so a
/// `Page` cannot be held across `.await` points on Tokio's multi-threaded
/// runtime. We spin up a current-thread runtime on a blocking thread and drive
/// the whole navigation there — the V8 isolate stays on one thread for its
/// entire lifetime, which is what deno_core expects.
pub(crate) fn run_on_local_runtime<F, T>(f: F) -> Result<T>
where
    F: for<'a> FnOnce(&'a tokio::runtime::Runtime) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + 'a>>
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
/// navigation. Each entry is a Set-Cookie style string (`name=value`). They
/// are scoped to the target URL's host so they attach to the first request —
/// needed for sites (e.g. WeChat articles) that gate content behind a
/// logged-in session cookie.
pub(crate) fn inject_cookies(browser: &Browser, cookies: &[String], target_url: &str) {
    if cookies.is_empty() {
        return;
    }
    tracing::debug!("inject_cookies: {} cookies for {}", cookies.len(), target_url);
    let store = browser.cookies();
    let base = match url::Url::parse(target_url) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("inject_cookies: failed to parse target URL '{}': {}", target_url, e);
            return;
        }
    };
    let domain = format!("Domain={}", base.host_str().unwrap_or(""));
    for c in cookies {
        // Allow callers to pass either a bare "name=value" or a full Set-Cookie.
        let full = if c.to_ascii_lowercase().contains("domain=") || c.to_ascii_lowercase().contains("path=") {
            c.clone()
        } else {
            format!("{}; {}; Path=/", c, domain)
        };
        let _ = store.set(&full, target_url);
    }
}

/// Check if the current page is a Cloudflare challenge.
fn is_cloudflare_challenge(page: &mut crate::page::Page) -> bool {
    let title_val = page.evaluate("document.title");
    let title = title_val.as_str().unwrap_or("");
    if title.contains("Just a moment") || title.contains("Attention Required") {
        return true;
    }
    let has_turnstile_val = page.evaluate(
        r#"!!document.querySelector('iframe[src*="challenges.cloudflare.com"]')"#,
    );
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
    tracing::info!("Cloudflare challenge detected at {}, auto-bypassing...", url);

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

/// Read the rendered text content from the live DOM (after JS has run).
/// When `selector` is given, return that element's innerText; otherwise the
/// whole body. This reflects JS-filled content (WeChat/SPA), unlike parsing
/// the initial HTML snapshot.
///
/// Our innerText does NOT exclude script/style text (unlike a real
/// browser), so we blank those elements' textContent on the live DOM first.
/// This mutates the page, but do_fetch discards it right after.
fn rendered_text(page: &mut crate::page::Page, selector: Option<&str>) -> String {
    let js = match selector {
        Some(sel) => {
            let escaped = sel.replace('\\', "\\\\").replace('`', "\\`").replace('$', "\\$");
            format!(
                "(function(){{var el=document.querySelector(`{escaped}`);if(!el)return'';el.querySelectorAll('script,style,noscript').forEach(function(e){{e.textContent=''}});return el.innerText;}})()"
            )
        }
        None => {
            "(function(){var b=document.body;if(!b)return '';b.querySelectorAll('script,style,noscript').forEach(function(e){e.textContent=''});return b.innerText;})()".to_string()
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
            let content = rendered_text(&mut page, None);

            // If we landed on an antispider/CAPTCHA page, treat it as an error
            // rather than returning the CAPTCHA page content as search result body.
            if is_antispider {
                return Err(anyhow::anyhow!("CAPTCHA/antispider page detected at {}", final_url));
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
            let content = match req.format {
                OutputFormat::Html => page.content(),
                OutputFormat::Text | OutputFormat::Markdown => {
                    rendered_text(&mut page, req.selector.as_deref())
                }
            };

            // Truncate to max_chars (0 = unlimited). Keeps huge pages from
            // blowing up a downstream LLM context window.
            let (content, truncated) = if req.max_chars > 0 && content.chars().count() > req.max_chars {
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
                if let Some(ct) = crate::captcha::detect_captcha_type(&final_url, Some(&html_snapshot)) {
                    let mut event = crate::captcha::CaptchaEvent {
                        engine: String::new(),
                        captcha_type: ct.clone(),
                        url: final_url.clone(),
                        auto_solve_attempted: false,
                        auto_solve_succeeded: false,
                    };
                    if let Some(config) = crate::captcha::load_solver_config_from_env() {
                        event.auto_solve_attempted = true;
                        let result = crate::captcha::auto_solve_captcha(
                            &final_url, &html_snapshot, &ct, &config,
                        ).await;
                        event.auto_solve_succeeded = matches!(
                            result,
                            crate::captcha::CaptchaSolveResult::Solved { .. }
                        );
                    }
                    Some(event)
                } else {
                    None
                }
            };

            Ok(FetchResponse {
                url: page.url(),
                title,
                content,
                truncated,
                captcha_event,
                js_extract_result,
            })
        })
    })
}

/// Click an element by CSS selector using JS `element.click()`.
pub fn do_click(req: ClickRequest) -> Result<ClickResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
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
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let result = page.evaluate_async(&req.script).await;

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
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
            };

            // JS-rendered DOM — the same source /fetch uses for OutputFormat::Html.
            let html = page.content();
            // Pre-fetch while the page (its cookie'd HTTP client) is still alive.
            let resources = crate::screenshot::prefetch_render_resources(&page, &final_url, &html).await;
            drop(page);
            drop(browser);

            // Render off-thread-ish: Blitz layout/paint is sync and CPU-bound.
            // We're already on a blocking runtime thread, so just call it directly.
            // engine=diting swaps in our own css+layout+paint stack (no
            // Stylo/vello/parley — the render-claim line).
            let rendered = if req.engine.as_deref() == Some("diting") {
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
            } else {
                crate::screenshot::render_html_to_png(
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
                    Some(sel) => crate::screenshot::element_rects_diting(
                        &html,
                        sel,
                        req.selector_all,
                        req.width as f32,
                        req.height as f32,
                        // External <link> sheet bodies the prefetch pass already
                        // fetched — feed them to diting so its cascade sees what
                        // Blitz saw. Inline <style> blocks come from the HTML.
                        Some(&resources
                            .iter()
                            .filter(|(k, v)| k.ends_with(".css") && !v.is_empty())
                            .map(|(_, v)| String::from_utf8_lossy(v.as_ref()).into_owned())
                            .collect::<Vec<_>>()
                            .join("\n")),
                    ).ok(),
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

/// /search: native search across Baidu/Bing/Sogou/Google, optionally grab body for top N results.
pub async fn do_search(req: SearchRequest) -> Result<SearchResponse, SearchError> {
    // Step 1: native search via built-in engines.
    static REGISTRY: std::sync::LazyLock<crate::search::SearchEngineRegistry> =
        std::sync::LazyLock::new(crate::search::SearchEngineRegistry::new);
    let params = crate::search::SearchParams {
        language: req.language.clone(),
        pageno: 1,
        use_proxy: req.use_proxy,
        timeout_secs: 15,
        engine_filter: req.engines,
    };

    let (mut items, number_of_results, captcha_events) =
        crate::search::native_search(&REGISTRY, &req.q, params, &req.categories, req.max_results).await;

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
            let url = items[i].url.clone();
            let cookies = items[i].cookies.clone();
            let use_proxy = req.use_proxy;
            let wait = req.wait_secs;
            let max_chars = req.max_chars_per;
            if !cookies.is_empty() {
                tracing::debug!("do_search: item {} url={} has {} cookies", i, url, cookies.len());
            }
            handles.push(tokio::task::spawn_blocking(move || {
                (i, fetch_url_text_with_cookies(url, use_proxy, wait, max_chars, &cookies))
            }));
        }
        for h in handles {
            let (i, res) = h.await.map_err(|e| {
                SearchError::Other(format!("fetch task panicked: {e}"))
            })?;
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

    Ok(SearchResponse {
        query: req.q,
        number_of_results,
        results: items,
        captcha_events,
    })
}

#[cfg(test)]
mod shared_jar_tests {
    use super::*;
    use crate::browser::Browser;

    /// The stateless handlers' CAPTCHA mitigation: two browsers built from
    /// the same shared jar observe each other's cookies, so a repeat visit
    /// to a site presents the first visit's grants (wappass tokens etc.)
    /// instead of looking like a brand-new visitor.
    #[tokio::test]
    async fn shared_cookie_jar_spans_browser_instances() {
        let jar = Arc::new(CookieJar::new());
        let mk = || {
            Browser::builder()
                .stealth(false)
                .shared_cookie_jar(jar.clone())
                .build()
                .unwrap()
        };
        let b1 = mk();
        let url = url::Url::parse("https://www.example.com/").unwrap();
        b1.cookies().set("sid=abc123", "https://www.example.com/").unwrap();

        let b2 = mk();
        let got = b2.cookies().get_for_url("https://www.example.com/").unwrap();
        let names: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"sid"), "second browser sees the cookie: {names:?}");
    }
}
