//! Browser / cookie-jar factory for the acquire façade.
//!
//! Process-global shared jar, account-bound jars, cookie normalize/deserialize,
//! and the single-thread V8 runtime host used by every stateless handler.

use crate::browser::Browser;
use anyhow::{Context, Result};
use diting::diting_net::CookieJar;
use std::sync::Arc;

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

pub(crate) fn cookie_store_path() -> std::path::PathBuf {
    // STORAGE_DIR sits in the chain so an isolated instance (#203's
    // "private database" setup) doesn't import the shared cookie jar —
    // same isolation contract the local store honors for cache.db.
    let dir = std::env::var("AGINXBROWSER_COOKIE_STORE_DIR")
        .ok()
        .or_else(|| std::env::var("AGINXBROWSER_STORAGE_DIR").ok())
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

