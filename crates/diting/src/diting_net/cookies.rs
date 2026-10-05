use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;
use url::Url;

const DEFAULT_SAME_SITE: &str = "Lax";

/// Cookies scoped to one domain, keyed by the (name, path) pair — RFC 6265
/// identifies a cookie by the (name, domain, path) triple, so same-name
/// cookies with different paths must coexist.
type DomainCookies = HashMap<(String, String), CookieEntry>;

/// The mutation ring's length. A long merchant session (telemetry rotates
/// msToken every few seconds, analytics stacks js writes, SSO chains mint
/// dozens per navigation) needs thousands; rows are ~200 bytes, so the cap
/// is memory noise next to one retained response body.
const COOKIE_TRACE_CAP: usize = 4000;

/// One observed cookie mutation, metadata-only (issue #203: "which response
/// deleted `unb`/`sn`, which hop minted the fxg SSO" — answerable without a
/// single value leaving the jar). `source` names the write path: "http" is
/// a response's Set-Cookie (redirect hops included — the URL is the hop the
/// header rode on), "js" a document.cookie write, "import" seeded state
/// (CDP/import_curl).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CookieTraceEntry {
    pub ts_ms: u64,
    pub source: &'static str,
    pub request_url: String,
    pub name: String,
    pub domain: String,
    pub path: String,
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: String,
    pub expires: Option<i64>,
    /// An expiry-delete (past Expires/Max-Age): the row that removed a
    /// cookie rather than replaced it.
    pub deleted: bool,
}

/// Request context for SameSite send/receive decisions. The network layer
/// knows the initiator; the jar doesn't. Two facts drive every Chrome rule:
/// is the request cross-site, and is it a top-level navigation with a safe
/// (GET/HEAD) method — cross-site Lax cookies ride exactly and only those.
pub struct SendContext {
    pub cross_site: bool,
    pub top_level_navigation: bool,
    pub method_safe: bool,
}

impl SendContext {
    /// fetch()/XHR/asset request: Lax and Strict never ride cross-site
    /// subrequests, whatever the method.
    pub fn subresource(cross_site: bool) -> Self {
        SendContext {
            cross_site,
            top_level_navigation: false,
            method_safe: false,
        }
    }

    /// Top-level navigation.
    pub fn navigation(cross_site: bool, method_safe: bool) -> Self {
        SendContext {
            cross_site,
            top_level_navigation: true,
            method_safe,
        }
    }

    fn allows(&self, same_site: &str) -> bool {
        if !self.cross_site {
            return true;
        }
        match same_site {
            "None" => true,
            "Lax" => self.top_level_navigation && self.method_safe,
            _ => false, // Strict never crosses sites
        }
    }
}

pub struct CookieJar {
    cookies: RwLock<HashMap<String, DomainCookies>>,
    trace: RwLock<VecDeque<CookieTraceEntry>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CookieEntry {
    name: String,
    value: String,
    path: String,
    domain: String,
    host_only: bool,
    secure: bool,
    http_only: bool,
    expires: Option<u64>,
    same_site: String,
}

impl CookieJar {
    pub fn new() -> Self {
        CookieJar {
            cookies: RwLock::new(HashMap::new()),
            trace: RwLock::new(VecDeque::new()),
        }
    }

    fn record_trace(
        &self,
        source: &'static str,
        request_url: &str,
        entry: &CookieEntry,
        deleted: bool,
    ) {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut trace = self.trace.write().unwrap();
        if trace.len() == COOKIE_TRACE_CAP {
            trace.pop_front();
        }
        trace.push_back(CookieTraceEntry {
            ts_ms,
            source,
            request_url: request_url.to_string(),
            name: entry.name.clone(),
            domain: entry.domain.clone(),
            path: entry.path.clone(),
            host_only: entry.host_only,
            secure: entry.secure,
            http_only: entry.http_only,
            same_site: entry.same_site.clone(),
            expires: entry.expires.map(|e| e as i64),
            deleted,
        });
    }

    /// The mutation ring, oldest first. Metadata only — never a value.
    pub fn cookie_trace(&self) -> Vec<CookieTraceEntry> {
        self.trace.read().unwrap().iter().cloned().collect()
    }

    pub fn set_cookie(&self, set_cookie_str: &str, url: &Url) {
        self.set_cookie_inner(set_cookie_str, url, "http", None)
    }

    /// Context-aware twin of [`CookieJar::set_cookie`] for the HTTP response
    /// path: SameSite receive rules apply (a cross-site subresource response
    /// cannot set cookies that don't opt in with `SameSite=None`).
    pub fn set_cookie_in_context(
        &self,
        set_cookie_str: &str,
        url: &Url,
        ctx: &SendContext,
        source: &'static str,
    ) {
        self.set_cookie_inner(set_cookie_str, url, source, Some(ctx))
    }

    /// The seeded-state twin of [`CookieJar::set_cookie`]: same parsing and
    /// storage rules, but the mutation ring records the write as "import" —
    /// session-create injection and account seeding carried no response.
    pub fn set_cookie_seeded(&self, set_cookie_str: &str, url: &Url) {
        self.set_cookie_inner(set_cookie_str, url, "import", None)
    }

    fn set_cookie_inner(
        &self,
        set_cookie_str: &str,
        url: &Url,
        source: &'static str,
        ctx: Option<&SendContext>,
    ) {
        let parts: Vec<&str> = set_cookie_str.splitn(2, ';').collect();
        let name_value = parts[0].trim();
        let (name, value) = match name_value.split_once('=') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => return,
        };

        let request_host = url.host_str().unwrap_or("").to_lowercase();
        let mut domain_attr: Option<String> = None;
        let mut path = default_cookie_path(url.path());
        // Whether the cookie carried an explicit Path attribute — the
        // __Host- prefix rule (#137) reads this, not the derived path.
        let mut path_attr: Option<String> = None;
        let mut secure = false;
        let mut http_only = false;
        let mut expires: Option<u64> = None;
        let mut same_site = "Lax".to_string();

        if parts.len() > 1 {
            for attr in parts[1].split(';') {
                let attr = attr.trim();
                if let Some((key, val)) = attr.split_once('=') {
                    match key.trim().to_lowercase().as_str() {
                        "domain" => {
                            domain_attr = Some(val.trim().trim_start_matches('.').to_lowercase());
                        }
                        "path" => {
                            let p = val.trim().to_string();
                            path = p.clone();
                            path_attr = Some(p);
                        }
                        "expires" => {
                            if let Ok(ts) = parse_http_date(val.trim()) {
                                expires = Some(ts);
                            }
                        }
                        "max-age" => {
                            if let Ok(secs) = val.trim().parse::<i64>() {
                                if secs <= 0 {
                                    expires = Some(0);
                                } else {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    expires = Some(now + secs as u64);
                                }
                            }
                        }
                        "samesite" => {
                            same_site = normalize_same_site(val);
                        }
                        _ => {}
                    }
                } else {
                    match attr.to_lowercase().as_str() {
                        "secure" => secure = true,
                        "httponly" => http_only = true,
                        _ => {}
                    }
                }
            }
        }

        // SameSite gates (Chrome). `SameSite=None` without Secure is refused
        // in every context, and a cross-site subresource response cannot set
        // any cookie that doesn't opt in with `SameSite=None`. Refusal is
        // total: the header must not even act as an expiry-deletion.
        if same_site == "None" && !secure {
            return;
        }
        if let Some(ctx) = ctx {
            if ctx.cross_site && !ctx.top_level_navigation && same_site != "None" {
                return;
            }
        }

        // Validate Domain against the response origin (RFC 6265): an unrelated
        // Domain attribute must not let a response from attacker.example scope
        // a cookie to victim.example (cookie tossing).
        let (domain, host_only) = match resolve_cookie_domain(&request_host, domain_attr.as_deref()) {
            Some(d) => d,
            None => return,
        };

        // A Secure cookie from an untrustworthy origin is refused outright
        // (Chromium M89+): never stored, and never even an expiry-delete —
        // the gate sits before the prefix check and the delete branch.
        // Loopback HTTP IS trustworthy, so local dev servers keep working
        // (obscura#1107).
        if secure && !is_trustworthy_origin(url) {
            return;
        }

        // Reject before storage AND before the expiry-delete branch below —
        // a prefix-violating cookie is ignored entirely, so it must not even
        // serve as a deletion of a previously-valid same-name cookie.
        if !cookie_prefix_ok(&name, secure, is_trustworthy_origin(url), host_only, path_attr.as_deref()) {
            return;
        }

        if let Some(exp) = expires {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if exp <= now {
                // An expired Set-Cookie is a deletion of the matching
                // (name, path) entry, not a no-op.
                let mut cookies = self.cookies.write().unwrap();
                if let Some(domain_cookies) = cookies.get_mut(&domain) {
                    domain_cookies.remove(&(name.clone(), path.clone()));
                }
                drop(cookies);
                self.record_trace(
                    source,
                    url.as_str(),
                    &CookieEntry {
                        name: name.clone(),
                        value: String::new(),
                        path: path.clone(),
                        domain: domain.clone(),
                        host_only,
                        secure,
                        http_only,
                        expires,
                        same_site: same_site.clone(),
                    },
                    true,
                );
                return;
            }
        }

        let entry = CookieEntry {
            name: name.clone(),
            value,
            path: path.clone(),
            domain: domain.clone(),
            host_only,
            secure,
            http_only,
            expires,
            same_site: same_site.clone(),
        };

        {
            let mut cookies = self.cookies.write().unwrap();
            cookies.entry(domain.clone()).or_default().insert((name.clone(), path.clone()), entry.clone());
        }
        self.record_trace(source, url.as_str(), &entry, false);
    }

    /// Context-aware twin of [`CookieJar::get_cookie_header`]: enforces
    /// SameSite send rules (Chrome, Lax-by-default — cookies stored without
    /// an explicit attribute are already normalized to `Lax` at parse time).
    pub fn get_cookie_header_for(&self, url: &Url, ctx: &SendContext) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = is_trustworthy_origin(url);
        let cookies = self.cookies.read().unwrap();

        let mut matching: Vec<String> = Vec::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for (domain, domain_cookies) in cookies.iter() {
            if !domain_matches(host, domain) {
                continue;
            }
            for entry in domain_cookies.values() {
                if entry.host_only && !host.eq_ignore_ascii_case(domain) {
                    continue;
                }
                if let Some(exp) = entry.expires {
                    if exp <= now {
                        continue;
                    }
                }
                if entry.secure && !is_secure {
                    continue;
                }
                if !path_matches(path, &entry.path) {
                    continue;
                }
                if !ctx.allows(&entry.same_site) {
                    continue;
                }
                matching.push(format!("{}={}", entry.name, entry.value));
            }
        }

        matching.join("; ")
    }

    pub fn get_cookie_header(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = is_trustworthy_origin(url);
        let cookies = self.cookies.read().unwrap();

        let mut matching: Vec<String> = Vec::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for (domain, domain_cookies) in cookies.iter() {
            if !domain_matches(host, domain) {
                continue;
            }
            for entry in domain_cookies.values() {
                if entry.host_only && !host.eq_ignore_ascii_case(domain) {
                    continue;
                }
                if let Some(exp) = entry.expires {
                    if exp <= now {
                        continue;
                    }
                }
                if entry.secure && !is_secure {
                    continue;
                }
                if !path_matches(path, &entry.path) {
                    continue;
                }
                matching.push(format!("{}={}", entry.name, entry.value));
            }
        }

        matching.join("; ")
    }

    pub fn get_all_cookies(&self) -> Vec<CookieInfo> {
        let cookies = self.cookies.read().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut result = Vec::new();
        for domain_cookies in cookies.values() {
            for entry in domain_cookies.values() {
                if entry.expires.is_some_and(|expires| expires <= now) {
                    continue;
                }
                result.push(CookieInfo {
                    name: entry.name.clone(),
                    value: entry.value.clone(),
                    domain: entry.domain.clone(),
                    path: entry.path.clone(),
                    secure: entry.secure,
                    http_only: entry.http_only,
                    same_site: entry.same_site.clone(),
                    expires: entry.expires.map(|e| e as i64),
                });
            }
        }
        result
    }

    /// Metadata-only twin of [`CookieJar::get_all_cookies`] (#102): the
    /// same live, unexpired walk, minus every value — the "what auth state
    /// exists" answer for callers who must not hold credentials.
    pub fn get_all_cookie_metadata(&self) -> Vec<CookieMetadata> {
        let cookies = self.cookies.read().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut result = Vec::new();
        for domain_cookies in cookies.values() {
            for entry in domain_cookies.values() {
                if entry.expires.is_some_and(|expires| expires <= now) {
                    continue;
                }
                result.push(CookieMetadata {
                    name: entry.name.clone(),
                    domain: entry.domain.clone(),
                    path: entry.path.clone(),
                    secure: entry.secure,
                    http_only: entry.http_only,
                    same_site: entry.same_site.clone(),
                    expires: entry.expires.map(|e| e as i64),
                    host_only: entry.host_only,
                });
            }
        }
        result
    }

    pub fn set_cookies_from_cdp(&self, cookies: Vec<CookieInfo>) {
        // Seeded state (import/curl, account revival): no response URL rode
        // these in, but the ring still owes the reader the login-state
        // baseline — when each cookie appeared.
        let mut seeded: Vec<CookieEntry> = Vec::with_capacity(cookies.len());
        {
            let mut jar = self.cookies.write().unwrap();
            for cookie in cookies {
                let same_site = if cookie.same_site.is_empty() {
                    DEFAULT_SAME_SITE.to_string()
                } else {
                    cookie.same_site
                };
                let expires = cookie.expires.and_then(|e| if e > 0 { Some(e as u64) } else { None });
                if let Some(domain_cookies) = jar.get_mut(&cookie.domain) {
                    domain_cookies.retain(|_key, entry| {
                        entry.name != cookie.name || entry.path != cookie.path
                    });
                }
                let entry = CookieEntry {
                    name: cookie.name.clone(),
                    value: cookie.value,
                    path: cookie.path.clone(),
                    domain: cookie.domain.clone(),
                    host_only: false,
                    secure: cookie.secure,
                    http_only: cookie.http_only,
                    expires,
                    same_site,
                };
                jar.entry(cookie.domain.clone()).or_default().insert((cookie.name.clone(), cookie.path.clone()), entry.clone());
                seeded.push(entry);
            }
        }
        for entry in seeded {
            self.record_trace("import", "", &entry, false);
        }
    }

    pub fn get_js_visible_cookies(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = is_trustworthy_origin(url);
        let cookies = self.cookies.read().unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut matching: Vec<String> = Vec::new();

        for (domain, domain_cookies) in cookies.iter() {
            if !domain_matches(host, domain) {
                continue;
            }
            for entry in domain_cookies.values() {
                if entry.http_only {
                    continue;
                }
                if entry.host_only && !host.eq_ignore_ascii_case(domain) {
                    continue;
                }
                if let Some(exp) = entry.expires {
                    if exp <= now {
                        continue;
                    }
                }
                if entry.secure && !is_secure {
                    continue;
                }
                if !path_matches(path, &entry.path) {
                    continue;
                }
                matching.push(format!("{}={}", entry.name, entry.value));
            }
        }

        matching.join("; ")
    }

    pub fn set_cookie_from_js(&self, cookie_str: &str, url: &Url) {
        let parts: Vec<&str> = cookie_str.splitn(2, ';').collect();
        let name_value = parts[0].trim();
        let (name, value) = match name_value.split_once('=') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => return,
        };

        let request_host = url.host_str().unwrap_or("").to_lowercase();
        let mut domain_attr: Option<String> = None;
        let mut path = default_cookie_path(url.path());
        // #137: same explicit-Path tracking as the Set-Cookie path — the
        // __Host- prefix rule reads the attribute, not the derived path.
        let mut path_attr: Option<String> = None;
        let mut secure = false;
        let mut expires: Option<u64> = None;
        let mut same_site = "Lax".to_string();

        if parts.len() > 1 {
            for attr in parts[1].split(';') {
                let attr = attr.trim();
                if let Some((key, val)) = attr.split_once('=') {
                    match key.trim().to_lowercase().as_str() {
                        "domain" => {
                            domain_attr = Some(val.trim().trim_start_matches('.').to_lowercase());
                        }
                        "path" => {
                            let p = val.trim().to_string();
                            path = p.clone();
                            path_attr = Some(p);
                        }
                        "expires" => {
                            if let Ok(ts) = parse_http_date(val.trim()) {
                                expires = Some(ts);
                            }
                        }
                        "max-age" => {
                            if let Ok(secs) = val.trim().parse::<i64>() {
                                if secs <= 0 {
                                    expires = Some(0);
                                } else {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    expires = Some(now + secs as u64);
                                }
                            }
                        }
                        "samesite" => {
                            same_site = normalize_same_site(val);
                        }
                        _ => {}
                    }
                } else if attr.to_lowercase() == "secure" {
                    secure = true;
                }
            }
        }

        let (domain, host_only) = match resolve_cookie_domain(&request_host, domain_attr.as_deref()) {
            Some(d) => d,
            None => return,
        };

        // Same write-side trust gate as the HTTP path: document.cookie
        // cannot store a Secure cookie from a plain-HTTP public page
        // (loopback excepted, obscura#1107).
        if secure && !is_trustworthy_origin(url) {
            return;
        }

        // Same prefix rules on the document.cookie write path (#67) —
        // otherwise page JS could just mint a __Host- cookie itself.
        if !cookie_prefix_ok(&name, secure, is_trustworthy_origin(url), host_only, path_attr.as_deref()) {
            return;
        }

        // RFC 6265 §5.3 storage model, non-HTTP API write (obscura #915): a
        // document.cookie write targeting an existing cookie with the same
        // (domain, name, path) whose http_only flag is set must be silently
        // ignored — both the plain overwrite and the expiry-delete form
        // below, which would otherwise let page JS evict a server-set
        // HttpOnly session cookie. Like every document.cookie failure, the
        // write is a silent no-op, never an exception.
        {
            let cookies = self.cookies.read().unwrap();
            if cookies
                .get(&domain)
                .and_then(|dc| dc.get(&(name.clone(), path.clone())))
                .is_some_and(|e| e.http_only)
            {
                return;
            }
        }

        if let Some(exp) = expires {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if exp <= now {
                let mut cookies = self.cookies.write().unwrap();
                if let Some(domain_cookies) = cookies.get_mut(&domain) {
                    domain_cookies.remove(&(name.clone(), path.clone()));
                }
                drop(cookies);
                self.record_trace(
                    "js",
                    url.as_str(),
                    &CookieEntry {
                        name: name.clone(),
                        value: String::new(),
                        path: path.clone(),
                        domain: domain.clone(),
                        host_only,
                        secure,
                        http_only: false,
                        expires,
                        same_site: same_site.clone(),
                    },
                    true,
                );
                return;
            }
        }

        let entry = CookieEntry {
            name: name.clone(),
            value,
            path: path.clone(),
            domain: domain.clone(),
            host_only,
            secure,
            http_only: false,
            expires,
            same_site: same_site.clone(),
        };

        {
            let mut cookies = self.cookies.write().unwrap();
            cookies.entry(domain.clone()).or_default().insert((name.clone(), path.clone()), entry.clone());
        }
        self.record_trace("js", url.as_str(), &entry, false);
    }

    pub fn delete_cookies_filtered(&self, name: &str, domain: &str, path: Option<&str>) {
        let mut cookies = self.cookies.write().unwrap();
        let matches_path = |entry_path: &str| match path {
            Some(p) => entry_path == p,
            None => true,
        };
        if domain.is_empty() {
            for domain_cookies in cookies.values_mut() {
                domain_cookies.retain(|_k, e| !(e.name == name && matches_path(&e.path)));
            }
        } else {
            let domains_to_try = [
                domain.to_string(),
                format!(".{}", domain.trim_start_matches('.')),
                domain.trim_start_matches('.').to_string(),
            ];
            for d in &domains_to_try {
                if let Some(domain_cookies) = cookies.get_mut(d.as_str()) {
                    domain_cookies.retain(|_k, e| !(e.name == name && matches_path(&e.path)));
                }
            }
        }
    }

    pub fn clear(&self) {
        self.cookies.write().unwrap().clear();
    }

    /// Serialize all non-expired cookies to a JSON file.
    /// Writes atomically via tempfile then rename.
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;

        let cookies = self.cookies.read().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut all: Vec<CookieInfo> = Vec::new();
        for domain_cookies in cookies.values() {
            for entry in domain_cookies.values() {
                if let Some(exp) = entry.expires {
                    if exp <= now {
                        continue;
                    }
                }
                all.push(CookieInfo {
                    name: entry.name.clone(),
                    value: entry.value.clone(),
                    domain: entry.domain.clone(),
                    path: entry.path.clone(),
                    secure: entry.secure,
                    http_only: entry.http_only,
                    same_site: entry.same_site.clone(),
                    expires: entry.expires.map(|e| e as i64),
                });
            }
        }

        let json = serde_json::to_string_pretty(&all).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            // Session tokens live here; the store must not inherit the
            // process umask. Tighten explicitly instead of relying on
            // tempfile's incidental 0600 (obscura#855's perms half).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut tmp = tempfile::NamedTempFile::new_in(
            path.parent().unwrap_or(std::path::Path::new(".")),
        )?;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Load cookies from a JSON file into the jar.
    /// Merges with existing cookies (does not clear).
    /// Returns the number of cookies loaded.
    pub fn load_from_file(&self, path: &std::path::Path) -> Result<usize, std::io::Error> {
        if !path.exists() {
            return Ok(0);
        }
        let data = std::fs::read_to_string(path)?;
        let cookies: Vec<CookieInfo> =
            serde_json::from_str(&data).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e)
            })?;
        let count = cookies.len();
        self.set_cookies_from_cdp(cookies);
        Ok(count)
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CookieInfo {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
    #[serde(default, rename = "sameSite")]
    pub same_site: String,
    #[serde(default)]
    pub expires: Option<i64>,
}

/// Everything about a cookie except its value (#102). Names, scoping and
/// expiry are metadata an operator reasons about ("which login landed, is
/// the session cookie Secure, when does it die"); the value is a
/// credential and never enters a read-back surface. Same discipline as the
/// account face (AccountSummary), one layer down.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CookieMetadata {
    pub name: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
    #[serde(default, rename = "sameSite")]
    pub same_site: String,
    /// Unix seconds; `None` = session cookie (dies with the session).
    #[serde(default)]
    pub expires: Option<i64>,
    /// Host-anchored (set without a `Domain=` attribute): sent to the
    /// exact host only, never sibling subdomains.
    #[serde(rename = "hostOnly")]
    pub host_only: bool,
}

fn parse_http_date(s: &str) -> Result<u64, ()> {
    let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

    let s = s.replace('-', " ");
    let parts: Vec<&str> = s.split_whitespace().collect();

    if parts.len() < 5 { return Err(()); }

    let day: u64 = parts[1].parse().map_err(|_| ())?;
    let month = months.iter().position(|m| parts[2].to_lowercase().starts_with(m))
        .ok_or(())? as u64 + 1;
    let year: u64 = parts[3].parse().map_err(|_| ())?;

    let time_parts: Vec<&str> = parts[4].split(':').collect();
    let hour: u64 = time_parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minute: u64 = time_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let second: u64 = time_parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

    // Range-check before any arithmetic (#69): Expires is untrusted header
    // input. An unbounded year turned the accumulation loop below into a
    // single-header CPU burn (~10^11 iterations for year 99999999999),
    // day=0 underflowed `day - 1` into a far-future "permanent" cookie,
    // and an out-of-range hour could overflow the final seconds product.
    // Out-of-range rejects like any unparsable date — session-cookie
    // semantics. Second allows 60 for the HTTP-date leap second.
    if !(1..=31).contains(&day)
        || !(1601..=9999).contains(&year)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(());
    }

    let mut days_total: u64 = 0;
    for y in 1970..year {
        days_total += if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) { 366 } else { 365 };
    }
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    for m in 1..month {
        days_total += days_in_month[m as usize] + if m == 2 && is_leap { 1 } else { 0 };
    }
    days_total += day - 1;

    Ok(days_total * 86400 + hour * 3600 + minute * 60 + second)
}

fn normalize_same_site(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "strict" => "Strict",
        "none" => "None",
        _ => "Lax",
    }
    .to_string()
}

// RFC 6265bis §4.1.3 cookie name prefixes (#67): __Secure- demands the
// Secure attribute; __Host- demands Secure, host-only scope (no effective
// Domain attribute), and an *explicit* Path=/ attribute (#137, obscura
// #1099) — a default-path of "/" derived from a root request URL does not
// qualify, and an explicit Path=/ on a deep URL must be honored (the old
// check read the request path and got both directions wrong). Prefix
// matching is case-insensitive. A violating cookie is dropped like any
// other invalid Set-Cookie — on both the HTTP and document.cookie paths.
//
// The Secure demands additionally require a trustworthy origin: Chromium
// rejects Secure-flag cookies over plain HTTP even with the attribute
// present, except on loopback (obscura#1107), where http://localhost is
// treated as potentially trustworthy and the prefixes DO unlock.
fn cookie_prefix_ok(
    name: &str,
    secure: bool,
    trustworthy: bool,
    host_only: bool,
    path_attr: Option<&str>,
) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("__secure-") {
        return secure && trustworthy;
    }
    if lower.starts_with("__host-") {
        return secure && trustworthy && host_only && path_attr == Some("/");
    }
    true
}

/// True when the origin Chromium would consider potentially trustworthy for
/// cookie purposes: HTTPS anywhere, or plain HTTP on a loopback host —
/// "localhost" (and its subdomains), 127.0.0.0/8, [::1], and IPv4-mapped
/// loopback (obscura#1107: Secure cookies are both set and sent over
/// http://localhost in Chrome; any other plain-HTTP host is refused).
/// IPv6 hosts arrive bracketed from `Url::host_str` serialization.
fn is_trustworthy_origin(url: &Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    if url.scheme() != "http" {
        return false;
    }
    let host = url.host_str().unwrap_or("");
    let h = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if h.eq_ignore_ascii_case("localhost") || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || matches!(v6.to_ipv4_mapped(), Some(v4) if v4.is_loopback())
        }
        Err(_) => false,
    }
}

// RFC 6265 §5.1.3: the domain-match algorithm's suffix case applies only
// when "the string is a host name (i.e., not an IP address)". IPv6 hosts
// arrive bracketed from Url::host_str serialization, so strip the brackets
// before asking std.
fn is_ip_literal(host: &str) -> bool {
    let h = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    h.parse::<std::net::IpAddr>().is_ok()
}

// Resolve the effective cookie domain per RFC 6265 5.3: a Domain attribute
// that is not a parent domain of the request host is ignored and the cookie
// falls back to host-only on the origin. Returns (domain, host_only).
fn resolve_cookie_domain(origin_host: &str, domain_attr: Option<&str>) -> Option<(String, bool)> {
    let origin = origin_host.trim().trim_start_matches('.').to_lowercase();
    if origin.is_empty() {
        return None;
    }
    let dom = match domain_attr {
        None => return Some((origin, true)),
        Some(raw) => raw.trim().trim_start_matches('.').to_lowercase(),
    };
    if dom.is_empty() {
        return Some((origin, true));
    }
    // An IP-literal origin never widens (#68): suffix matching on the
    // string "192.168.1.100" would let Domain=168.1.100 scope a cookie
    // that domain_matches then sends to unrelated IPs like 10.168.1.100.
    // Browsers ignore a Domain attribute on IP hosts entirely — the
    // cookie stays host-only.
    if is_ip_literal(&origin) {
        return Some((origin, true));
    }
    // RFC 6265 §5.3: an explicit Domain attribute that domain-matches the
    // origin — including Domain=origin itself — makes a domain cookie, not a
    // host-only one. Browsers agree: `Domain=example.com` set from example.com
    // is sent to subdomains. Anchoring an apex-declared cookie at the apex
    // used to yield host-only, which broke cross-subdomain restore.
    if dom.contains('.') && (origin == dom || origin.ends_with(&format!(".{dom}"))) {
        Some((dom, false))
    } else {
        Some((origin, true))
    }
}

// RFC 6265 5.1.4 default-path: the directory of the request URI — everything
// up to but not including the right-most '/'. Using the full request path
// scopes a session cookie to the exact URL that set it, so a cookie set on
// /app/login would not match /app/dashboard.
pub fn default_cookie_path(request_path: &str) -> String {
    if !request_path.starts_with('/') {
        return "/".to_string();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => request_path[..idx].to_string(),
    }
}

// RFC 6265 5.1.4 path-match: bare starts_with over-matches sibling paths that
// share a string prefix (Path=/admin leaking to /administrator), so a prefix
// match must end on a '/' boundary.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

fn domain_matches(host: &str, domain: &str) -> bool {
    // Avoid allocations on the hot path. Cookie lookup runs per fetch
    // (every subresource on a page) and walks every domain in the jar.
    // Previously this allocated 2 lowercase Strings + a "." prefix
    // per (host, domain) pair.
    let domain = domain.trim_start_matches('.');
    // IP literals never suffix-match (#68): a cookie stored for
    // 192.0.2.1 must not leak to a host whose string merely ends in
    // that suffix (10.192.0.2.1). RFC 6265 §5.1.3 domain-match for an
    // IP host is exact string identity.
    if is_ip_literal(host) || is_ip_literal(domain) {
        return host.eq_ignore_ascii_case(domain);
    }
    if host.len() < domain.len() {
        return false;
    }
    // Exact match (case-insensitive)
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    // Suffix match with a '.' boundary: host = "sub.example.com",
    // domain = "example.com". The byte before the suffix in host
    // must be '.'.
    let prefix_len = host.len() - domain.len();
    if prefix_len < 1 { return false; }
    if !host.is_char_boundary(prefix_len) { return false; }
    if host.as_bytes()[prefix_len - 1] != b'.' { return false; }
    host[prefix_len..].eq_ignore_ascii_case(domain)
}

// The colocated contract suite — split out riding the god-file ratchet
// cap, same shape as diting_layout's text.rs + text/tests.rs (the layering
// audit exempts tests.rs).
#[cfg(test)]
mod tests;
