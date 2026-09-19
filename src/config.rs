use std::path::PathBuf;
use std::sync::Arc;

use diting::diting_net::CookieJar;

// Runtime knobs (AGINXBROWSER_* env) moved into the engine with the
// workspace split — the engine reads its own knobs (ARCHITECTURE.md §2 R2).
// Re-exported here so product call sites (`crate::config::…`) are unchanged;
// single implementation lives in diting::env_knobs.
pub use diting::env_knobs::{
    app_data_dir, ephemeral, js_stack_mb, proxy_from_env, should_auto_proxy, standard_proxy_env,
};
// Test-only re-export (parity with the pre-split `#[cfg(test)]` visibility):
// product tests hold the lock when flipping AGINXBROWSER_EPHEMERAL.
#[cfg(test)]
pub use diting::env_knobs::EPHEMERAL_ENV_LOCK;

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
