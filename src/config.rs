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
