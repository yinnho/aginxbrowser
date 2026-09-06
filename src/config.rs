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

/// Check if a URL points to a known foreign/blocked domain that requires proxy.
/// Uses suffix matching: `sub.github.com` matches `github.com`.
/// Returns `false` if URL parsing fails (safe fallback).
///
/// Governs every transport's proxy routing, not just the /fetch endpoint: the
/// net clients consult this per request when no explicit proxy was configured,
/// so page/session/CDP navigations reach blocked origins the same way /fetch
/// and download do.
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

#[cfg(test)]
mod tests {
    use super::{app_data_dir, ephemeral, should_auto_proxy};

    // The list gates routing for every navigation now — pin the match shapes.
    #[test]
    fn auto_proxy_matches_apex_and_subdomains_only() {
        assert!(should_auto_proxy("https://en.wikipedia.org/wiki/Rust"));
        assert!(should_auto_proxy("https://wikipedia.org/"));
        assert!(should_auto_proxy("https://raw.githubusercontent.com/x/y"));
        assert!(!should_auto_proxy("https://notwikipedia.org/"));
        assert!(!should_auto_proxy("https://example.com/"));
        assert!(!should_auto_proxy("not a url"));
    }

    #[test]
    fn ephemeral_parses_env_truthiness() {
        let _env = super::EPHEMERAL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("AGINXBROWSER_EPHEMERAL");
        assert!(!ephemeral());
        for truthy in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            std::env::set_var("AGINXBROWSER_EPHEMERAL", truthy);
            assert!(ephemeral(), "{truthy:?} should enable ephemeral");
        }
        for falsy in ["0", "false", "no", "off", ""] {
            std::env::set_var("AGINXBROWSER_EPHEMERAL", falsy);
            assert!(!ephemeral(), "{falsy:?} should not enable ephemeral");
        }
        std::env::remove_var("AGINXBROWSER_EPHEMERAL");
        assert!(!ephemeral());
    }

    #[test]
    fn app_data_dir_lands_under_platform_location() {
        if std::env::var_os("HOME").is_none() && std::env::var_os("XDG_DATA_HOME").is_none() {
            return; // no platform anchor; callers fall back to "."
        }
        let dir = app_data_dir().expect("anchor env set");
        assert!(dir.is_absolute(), "relative: {}", dir.display());
        assert!(
            dir.components().any(|c| c.as_os_str() == "aginxbrowser"),
            "path: {}",
            dir.display()
        );
    }
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

#[cfg(test)]
pub(crate) static EPHEMERAL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// When set, no credential-bearing state touches the disk: the shared
/// cookie jar and per-origin localStorage stay memory-only for the life of
/// the process (the shared jar exists to blunt CAPTCHA rates across
/// stateless fetches — see server::SHARED_COOKIE_JAR — but every auth
/// session that flows through it also lands in that file, which is not a
/// trade every deployment wants to make).
pub fn ephemeral() -> bool {
    matches!(
        std::env::var("AGINXBROWSER_EPHEMERAL")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Default on-disk home for credential-bearing state (the shared cookie
/// jar, per-origin localStorage). The current working directory was the
/// original default — which put live login cookies one careless
/// `git add .` away from a commit — so state now lands in the platform
/// application-data location instead. `AGINXBROWSER_COOKIE_STORE_DIR` /
/// `AGINXBROWSER_STORAGE_DIR` still override, and files are written 0600.
pub fn app_data_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join("Library/Application Support/aginxbrowser")
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return Some(std::path::PathBuf::from(xdg).join("aginxbrowser"));
            }
        }
        std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".local/share/aginxbrowser"))
    }
}
