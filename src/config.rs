#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::diting_net::CookieJar;

/// The upstream proxy URL, read once per call site from the environment:
/// `AGINXBROWSER_PROXY`.
pub fn proxy_from_env() -> Option<String> {
    std::env::var("AGINXBROWSER_PROXY")
        .ok()
        .filter(|p| !p.is_empty())
}

/// First standard proxy env var that is set, as "NAME=value". Used to warn
/// that the engine ignores these: every engine client pins reqwest/wreq's
/// implicit env/system proxy matcher off (see `reqwest_builder_no_env_proxy`),
/// so `AGINXBROWSER_PROXY` is the only knob that routes engine traffic
/// through a proxy. Without the warning, a shell-level proxy (clash, corp
/// egress) silently looks "configured" while the engine fetches direct.
pub fn standard_proxy_env() -> Option<String> {
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(v) = std::env::var(name) {
            if !v.is_empty() {
                return Some(format!("{name}={v}"));
            }
        }
    }
    None
}

/// Configuration for launching a Browser instance.
pub struct BrowserConfig {
    /// Proxy URL (e.g., "socks5://127.0.0.1:1080")
    pub proxy: Option<String>,
    /// Enable stealth mode (fingerprint spoofing)
    pub stealth: bool,
    /// Custom User-Agent string
    pub user_agent: Option<String>,
    /// Directory for persistent cookie storage
    pub storage_dir: Option<PathBuf>,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133",
    /// "safari17_5", "edge145", etc. None → Chrome145 default.
    pub tls_fingerprint: Option<String>,
    /// Caller-owned cookie jar shared across browser instances (the stateless
    /// HTTP handlers pass the process-global jar here).
    pub shared_cookie_jar: Option<Arc<CookieJar>>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            stealth: false,
            user_agent: None,
            storage_dir: None,
            tls_fingerprint: None,
            shared_cookie_jar: None,
        }
    }
}

/// Deep-stack budget (MB) shared by every thread that hosts a V8 isolate
/// and by V8's own JS stack ceiling (set once before the first isolate;
/// see diting_js::runtime). Minified SPA bundles recurse past defaults —
/// override for constrained hosts via `AGINXBROWSER_JS_STACK_MB` (1..=1024).
pub fn js_stack_mb() -> usize {
    std::env::var("AGINXBROWSER_JS_STACK_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&mb| (1..=1024).contains(&mb))
        .unwrap_or(32)
}
