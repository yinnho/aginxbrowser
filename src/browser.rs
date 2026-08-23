use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::diting_browser::BrowserContext;
use crate::diting_net::CookieJar;

use crate::config::BrowserConfig;
use crate::cookie::CookieStore;
use crate::error::Error;
use crate::page::Page;

static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);

pub struct Browser {
    context: Arc<BrowserContext>,
    cookie_jar: Arc<CookieJar>,
}

impl Browser {
    pub fn build(config: BrowserConfig) -> Result<Self, Error> {
        let context = BrowserContext::with_storage_and_network(
            "api".to_string(),
            config.proxy,
            config.stealth,
            config.user_agent,
            config.storage_dir.clone(),
            false,
            config.tls_fingerprint.clone(),
        );

        let context = Arc::new(context);
        let cookie_jar = context.cookie_jar.clone();

        Ok(Browser { context, cookie_jar })
    }

    pub fn builder() -> BrowserBuilder {
        BrowserBuilder::default()
    }

    pub async fn new_page(&self) -> Result<Page, Error> {
        let id = NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed);
        let page = crate::diting_browser::Page::new(
            format!("page-{}", id),
            self.context.clone(),
        );
        Ok(Page {
            inner: page,
            context: self.context.clone(),
        })
    }

    /// Access the cookie store for this browser session.
    pub fn cookies(&self) -> CookieStore {
        CookieStore::new(self.cookie_jar.clone())
    }
}

#[derive(Default)]
pub struct BrowserBuilder {
    config: BrowserConfig,
}

impl BrowserBuilder {
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.config.proxy = Some(proxy.into());
        self
    }
    pub fn stealth(mut self, stealth: bool) -> Self {
        self.config.stealth = stealth;
        self
    }
    // Per-instance UA override (env AGINXBROWSER_UA covers the process-wide
    // case) and the storage_dir cookie-persistence knob. No caller sets
    // either today; parked with the persistence feature (see
    // BrowserContext::save_cookies).
    #[allow(dead_code)]
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.config.user_agent = Some(ua.into());
        self
    }
    #[allow(dead_code)]
    pub fn storage_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.config.storage_dir = Some(dir.into());
        self
    }
    /// Override the TLS fingerprint (stealth mode only). Accepts names like
    /// "chrome145", "firefox133", "safari17_5", "edge145".
    pub fn tls_fingerprint(mut self, fp: impl Into<String>) -> Self {
        self.config.tls_fingerprint = Some(fp.into());
        self
    }
    pub fn build(self) -> Result<Browser, Error> {
        Browser::build(self.config)
    }
}
