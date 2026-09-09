use std::sync::Arc;
use crate::diting_net::CookieJar;
use serde::{Deserialize, Serialize};

/// A cookie as exposed to the Rust API. Bin-dead by itself; the round-trip
/// tests read cookies back through [`CookieStore::get_for_url`].
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
}

/// Cookie management for a browser session.
pub struct CookieStore {
    jar: Arc<CookieJar>,
}

impl CookieStore {
    pub(crate) fn new(jar: Arc<CookieJar>) -> Self {
        Self { jar }
    }

    /// Set a cookie via Set-Cookie header string.
    ///
    /// Example: `store.set("session=abc123; Domain=example.com; Path=/; HttpOnly")?;`
    pub fn set(&self, set_cookie_str: &str, url: &str) -> Result<(), crate::error::Error> {
        let parsed = url::Url::parse(url)
            .map_err(|e| crate::error::Error::Internal(e.into()))?;
        self.jar.set_cookie(set_cookie_str, &parsed);
        Ok(())
    }

    /// Get cookies for a specific URL.
    #[allow(dead_code)] // bin-dead; server.rs round-trip test reads cookies back
    pub fn get_for_url(&self, url: &str) -> Result<Vec<Cookie>, crate::error::Error> {
        let parsed = url::Url::parse(url)
            .map_err(|e| crate::error::Error::Internal(e.into()))?;
        let header = self.jar.get_cookie_header(&parsed);
        Ok(header
            .split("; ")
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                Some(Cookie {
                    name: parts.next()?.to_string(),
                    value: parts.next().unwrap_or("").to_string(),
                    domain: parsed.host_str()?.to_string(),
                    path: "/".into(),
                    secure: false,
                    http_only: false,
                })
            })
            .collect())
    }
}
