use crate::{
    ClickRequest, ClickResponse, EvalRequest, EvalResponse, FetchRequest, FetchResponse,
    OutputFormat, SearchRequest, SearchResponse,
};
use crate::browser::Browser;
use anyhow::{Context, Result};

/// Error type for /search (separate from anyhow so we can map to 503 vs 500).
#[derive(Debug)]
pub enum SearchError {
    /// SearXNG backend unreachable / errored → 503
    BackendUnavailable(String),
    /// Other internal error → 500
    Other(String),
}

/// Check if a URL points to a known foreign/blocked domain that requires proxy.
/// Uses suffix matching: `sub.github.com` matches `github.com`.
/// Returns `false` if URL parsing fails (safe fallback).
fn should_auto_proxy(url: &str) -> bool {
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
fn build_browser(use_proxy: bool, url: &str) -> Result<Browser> {
    // Stealth defaults on; disable via AGINXBROWSER_STEALTH=0 (diagnostic / when
    // the wreq stealth client misbehaves on a given site).
    let stealth = !matches!(std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(), Some("0"));
    let mut builder = Browser::builder().stealth(stealth);
    if should_auto_proxy(url) || use_proxy {
        if let Ok(proxy) = std::env::var("OBSCURA_PROXY") {
            builder = builder.proxy(&proxy);
        }
    }
    Ok(builder.build()?)
}

/// Run an Obscura operation on a dedicated single-threaded runtime.
///
/// Obscura's V8 runtime holds `Rc<RefCell<…>>` state, which is `!Send`, so a
/// `Page` cannot be held across `.await` points on Tokio's multi-threaded
/// runtime. We spin up a current-thread runtime on a blocking thread and drive
/// the whole navigation there — the V8 isolate stays on one thread for its
/// entire lifetime, which is what deno_core expects.
fn run_on_local_runtime<F, T>(f: F) -> Result<T>
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
fn inject_cookies(browser: &Browser, cookies: &[String], target_url: &str) {
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
async fn maybe_bypass_challenge(page: &mut crate::page::Page) -> Result<()> {
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
/// Obscura's innerText does NOT exclude script/style text (unlike a real
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

/// Core fetch: navigate to `url`, wait for JS, return rendered text (markdown-like).
/// Used by both /fetch and /search's body-grabbing. Runs on a dedicated
/// current-thread runtime (V8 is !Send). `max_chars=0` means unlimited.
fn fetch_url_text(
    url: String,
    use_proxy: bool,
    wait_secs: u64,
    max_chars: usize,
) -> Result<(String, bool)> {
    fetch_url_text_with_cookies(url, use_proxy, wait_secs, max_chars, &[])
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
            let browser = build_browser(use_proxy, &url)?;
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

/// Fetch a page and return content in the requested format.
pub fn do_fetch(req: FetchRequest) -> Result<FetchResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            let browser = build_browser(req.use_proxy, &req.url)?;
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

            Ok(FetchResponse {
                url: page.url(),
                title,
                content,
                truncated,
            })
        })
    })
}

/// Click an element by CSS selector using JS `element.click()`.
pub fn do_click(req: ClickRequest) -> Result<ClickResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            let browser = build_browser(req.use_proxy, &req.url)?;
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
            let browser = build_browser(req.use_proxy, &req.url)?;
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

/// /search: native search across Baidu/Bing/Sogou/Google, optionally grab body for top N results.
pub async fn do_search(req: SearchRequest) -> Result<SearchResponse, SearchError> {
    // Step 1: native search via built-in engines.
    let registry = crate::search::SearchEngineRegistry::new();
    let params = crate::search::SearchParams {
        language: req.language.clone(),
        pageno: 1,
        use_proxy: req.use_proxy,
        timeout_secs: 15,
    };

    let (mut items, number_of_results) =
        crate::search::native_search(&registry, &req.q, params, &req.categories, req.max_results).await;

    // Step 2: optionally grab body for the top fetch_top results (concurrent).
    // Each fetch runs in its own blocking thread + current-thread runtime
    // (V8 is !Send), so spawn_blocking gives natural isolation + concurrency.
    // Cookies from the search session (e.g. sogou WeChat) are passed through
    // so the obscura browser can authenticate redirect URLs.
    let n = req.fetch_top.min(items.len());
    if n > 0 {
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
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
        search_backend: "native".into(),
    })
}
