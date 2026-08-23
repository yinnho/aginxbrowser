use std::path::PathBuf;
use std::sync::Arc;

use crate::diting_net::{CookieJar, HttpClient, RobotsCache};

pub struct BrowserContext {
    /// Context label (upstream names contexts for CDP Target.setAutoAttach
    /// events; ours is per-request "api" and nothing reads it back yet).
    #[allow(dead_code)]
    pub id: String,
    pub cookie_jar: Arc<CookieJar>,
    pub http_client: Arc<HttpClient>,
    pub user_agent: String,
    pub proxy_url: Option<String>,
    pub robots_cache: Arc<RobotsCache>,
    pub obey_robots: bool,
    /// Persona flags: stealth gates tracker-blocking at HttpClient
    /// construction and the lazy wreq client in page.rs. tls_fingerprint
    /// selects the wreq Emulation for that client (stealth builds only),
    /// so plain builds see both as unread.
    #[cfg_attr(not(feature = "stealth"), allow(dead_code))]
    pub stealth: bool,
    /// When true, CDP-driven navigation to file:// URLs is permitted.
    /// Default is false: a remote CDP client cannot point the browser
    /// at /etc/shadow even if Obscura is running as a privileged user.
    /// Flip on via `obscura serve --allow-file-access` for legitimate
    /// local-HTML testing workflows. The CLI's own `obscura fetch
    /// file://...` path is unaffected because it does not go through
    /// the CDP server.
    #[allow(dead_code)] // read by the CDP file:// guard, which is not absorbed
    pub allow_file_access: bool,
    pub storage_dir: Option<PathBuf>,
    /// When true, the http client allows fetching localhost / RFC1918 /
    /// link-local addresses. Set via `--allow-private-network` (issue #33).
    /// Independent of `allow_file_access` because they cover different threat
    /// models: file:// is a local file-system read, while private-network is
    /// the broader SSRF gate from issue #4.
    #[allow(dead_code)] // gate is enforced at HttpClient construction; field kept for introspection
    pub allow_private_network: bool,
    /// TLS fingerprint name override (stealth mode only): "chrome145",
    /// "firefox133", etc. None → Chrome145. Kept as a String (not the
    /// `Emulation` enum) so the field exists regardless of the `stealth`
    /// feature; parsed into an `Emulation` lazily in the stealth client.
    #[cfg_attr(not(feature = "stealth"), allow(dead_code))]
    pub tls_fingerprint: Option<String>,
}

impl BrowserContext {
    /// Variant that also accepts the `allow_private_network` opt-in and a TLS
    /// fingerprint override. All pre-existing constructors default
    /// `allow_private_network` to `false` and `tls_fingerprint` to None; callers
    /// that want the CLI's `--allow-private-network` (issue #33) behaviour or a
    /// custom TLS fingerprint go through here.
    pub fn with_storage_and_network(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
        tls_fingerprint: Option<String>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, storage_dir, allow_private_network, tls_fingerprint)
    }

    fn _new_inner(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
        tls_fingerprint: Option<String>,
    ) -> Self {
        let cookie_jar = Arc::new(CookieJar::new());

        // Restore cookies from disk if storage_dir is configured
        if let Some(ref dir) = storage_dir {
            let cookie_path = dir.join("cookies.json");
            if cookie_path.exists() {
                match cookie_jar.load_from_file(&cookie_path) {
                    Ok(n) if n > 0 => {
                        tracing::info!("Loaded {} cookies from {}", n, cookie_path.display());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("Failed to load cookies from {}: {}", cookie_path.display(), e);
                    }
                }
            }
        }

        let mut client = HttpClient::with_full_options(
            cookie_jar.clone(),
            proxy_url.as_deref(),
            allow_private_network,
        );
        if stealth {
            client.block_trackers = true;
        }
        // Resolution chain: explicit per-context UA → AGINXBROWSER_UA → the
        // fingerprint pool's stable default (macOS Chrome 145; pin or rotate
        // via OBSCURA_PROFILE / OBSCURA_ROTATE_PROFILE — see profiles.rs).
        let resolved_ua = user_agent.unwrap_or_else(|| {
            std::env::var("AGINXBROWSER_UA").unwrap_or_else(|_| {
                crate::diting_browser::profiles::select_profile()
                    .user_agent
                    .to_string()
            })
        });
        // Sync the http client's UA at construction so navigation requests pick it
        // up before any async setup runs. The lock has no other holders here, so
        // try_write always succeeds; we fall back silently if it ever fails.
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = resolved_ua.clone();
        }
        let http_client = Arc::new(client);
        BrowserContext {
            id,
            cookie_jar,
            http_client,
            user_agent: resolved_ua,
            proxy_url,
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: false,
            stealth,
            allow_file_access: false,
            storage_dir,
            allow_private_network,
            tls_fingerprint,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // constructor-matrix coverage lives in this file's tests
    pub fn with_options(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
    ) -> Self {
        Self::with_full_options(id, proxy_url, stealth, None)
    }

    #[cfg_attr(not(test), allow(dead_code))] // constructor-matrix coverage lives in this file's tests
    pub fn with_full_options(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, None, false, None)
    }

    /// Persist cookies to disk if storage_dir is configured. Currently
    /// unwired: the server builds a fresh Browser per request and never
    /// passes a storage_dir, so no shutdown moment owns the jar. Parked
    /// until session persistence lands (the load half in `_new_inner`
    /// already works).
    #[allow(dead_code)]
    pub fn save_cookies(&self) {
        if let Some(ref dir) = self.storage_dir {
            let _ = std::fs::create_dir_all(dir);
            let cookie_path = dir.join("cookies.json");
            if let Err(e) = self.cookie_jar.save_to_file(&cookie_path) {
                tracing::warn!("Failed to save cookies to {}: {}", cookie_path.display(), e);
            } else {
                tracing::info!("Saved cookies to {}", cookie_path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_propagates_user_agent_to_http_client() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            Some("Custom-UA/1.0".to_string()),
        );
        assert_eq!(ctx.user_agent, "Custom-UA/1.0");
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert_eq!(client_ua, "Custom-UA/1.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_falls_back_to_chrome_default() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            None,
        );
        assert!(ctx.user_agent.contains("Chrome"));
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert!(client_ua.contains("Chrome"));
        assert_eq!(ctx.user_agent, client_ua);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_options_keeps_default_user_agent() {
        let ctx = BrowserContext::with_options("test".to_string(), None, false);
        assert!(ctx.user_agent.contains("Chrome"));
    }
}
