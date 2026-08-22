use std::collections::HashMap;
use std::sync::RwLock;
use url::Url;

const DEFAULT_SAME_SITE: &str = "Lax";

pub struct CookieJar {
    // Keyed by (name, path): RFC 6265 identifies a cookie by the
    // (name, domain, path) triple, so same-name cookies with different paths
    // must coexist instead of overwriting each other.
    cookies: RwLock<HashMap<String, HashMap<(String, String), CookieEntry>>>,
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
        }
    }

    pub fn set_cookie(&self, set_cookie_str: &str, url: &Url) {
        let parts: Vec<&str> = set_cookie_str.splitn(2, ';').collect();
        let name_value = parts[0].trim();
        let (name, value) = match name_value.split_once('=') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => return,
        };

        let request_host = url.host_str().unwrap_or("").to_lowercase();
        let mut domain_attr: Option<String> = None;
        let mut path = default_cookie_path(url.path());
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
                            path = val.trim().to_string();
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

        // Validate Domain against the response origin (RFC 6265): an unrelated
        // Domain attribute must not let a response from attacker.example scope
        // a cookie to victim.example (cookie tossing).
        let (domain, host_only) = match resolve_cookie_domain(&request_host, domain_attr.as_deref()) {
            Some(d) => d,
            None => return,
        };

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
            same_site,
        };

        let mut cookies = self.cookies.write().unwrap();
        cookies.entry(domain).or_default().insert((name, path), entry);
    }

    pub fn get_cookie_header(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = url.scheme() == "https";
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

    pub fn set_cookies_from_cdp(&self, cookies: Vec<CookieInfo>) {
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
            jar.entry(cookie.domain.clone()).or_default().insert((cookie.name, cookie.path), entry);
        }
    }

    pub fn get_js_visible_cookies(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = url.scheme() == "https";
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
                            path = val.trim().to_string();
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
                        _ => {}
                    }
                }
            }
        }

        let (domain, host_only) = match resolve_cookie_domain(&request_host, domain_attr.as_deref()) {
            Some(d) => d,
            None => return,
        };

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
            same_site,
        };

        let mut cookies = self.cookies.write().unwrap();
        cookies.entry(domain).or_default().insert((name, path), entry);
    }

    pub fn delete_cookie(&self, name: &str, domain: &str) {
        let mut cookies = self.cookies.write().unwrap();
        if domain.is_empty() {
            for domain_cookies in cookies.values_mut() {
                domain_cookies.retain(|_k, e| e.name != name);
            }
        } else {
            let domains_to_try = [
                domain.to_string(),
                format!(".{}", domain.trim_start_matches('.')),
                domain.trim_start_matches('.').to_string(),
            ];
            for d in &domains_to_try {
                if let Some(domain_cookies) = cookies.get_mut(d.as_str()) {
                    domain_cookies.retain(|_k, e| e.name != name);
                }
            }
        }
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
        }
        let mut tmp = tempfile::NamedTempFile::new_in(
            path.parent().unwrap_or(std::path::Path::new(".")),
        )?;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
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

    let mut days_total: u64 = 0;
    for y in 1970..year {
        days_total += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
    }
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
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
    if dom.is_empty() || dom == origin {
        return Some((origin, true));
    }
    if dom.contains('.') && origin.ends_with(&format!(".{dom}")) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get_cookie() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/path").unwrap();
        jar.set_cookie("session=abc123; Path=/; Secure; HttpOnly", &url);

        let header = jar.get_cookie_header(&url);
        assert!(header.contains("session=abc123"));
    }

    #[test]
    fn test_cookie_domain_matching() {
        let jar = CookieJar::new();
        let url = Url::parse("https://www.example.com/").unwrap();
        jar.set_cookie("token=xyz; Domain=example.com", &url);

        let header = jar.get_cookie_header(&url);
        assert!(header.contains("token=xyz"));

        let sub_url = Url::parse("https://api.example.com/").unwrap();
        let header2 = jar.get_cookie_header(&sub_url);
        assert!(header2.contains("token=xyz"));

        let other_url = Url::parse("https://other.com/").unwrap();
        let header3 = jar.get_cookie_header(&other_url);
        assert!(header3.is_empty());
    }

    #[test]
    fn test_cdp_cookie_with_leading_dot_domain_matches_requests() {
        let jar = CookieJar::new();
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "token".to_string(),
            value: "xyz".to_string(),
            domain: ".example.com".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires: None,
        }]);

        let apex_url = Url::parse("https://example.com/").unwrap();
        let apex_header = jar.get_cookie_header(&apex_url);
        assert!(apex_header.contains("token=xyz"));

        let subdomain_url = Url::parse("https://api.example.com/").unwrap();
        let subdomain_header = jar.get_cookie_header(&subdomain_url);
        assert!(subdomain_header.contains("token=xyz"));

        let other_url = Url::parse("https://other.com/").unwrap();
        let other_header = jar.get_cookie_header(&other_url);
        assert!(other_header.is_empty());
    }

    #[test]
    fn test_secure_cookie_not_sent_over_http() {
        let jar = CookieJar::new();
        let https_url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("secure_token=secret; Secure", &https_url);

        let http_url = Url::parse("http://example.com/").unwrap();
        let header = jar.get_cookie_header(&http_url);
        assert!(header.is_empty());
    }

    #[test]
    fn test_max_age_zero_deletes_cookie() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("session=abc", &url);
        assert!(jar.get_cookie_header(&url).contains("session=abc"));

        jar.set_cookie("session=abc; Max-Age=0", &url);
        assert!(jar.get_cookie_header(&url).is_empty());
    }

    #[test]
    fn test_max_age_sets_expiry() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("token=xyz; Max-Age=3600", &url);
        assert!(jar.get_cookie_header(&url).contains("token=xyz"));
    }

    #[test]
    fn test_expired_cookie_not_sent() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("old=gone; Expires=Thu, 01 Jan 2020 00:00:00 GMT", &url);
        assert!(jar.get_cookie_header(&url).is_empty());
    }

    #[test]
    fn test_samesite_parsed() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("strict_cookie=val; SameSite=Strict", &url);
        assert!(jar.get_cookie_header(&url).contains("strict_cookie=val"));
    }

    #[test]
    fn test_clear_cookies() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("a=1", &url);
        assert!(!jar.get_cookie_header(&url).is_empty());

        jar.clear();
        assert!(jar.get_cookie_header(&url).is_empty());
    }

    #[test]
    fn test_set_cookies_from_cdp_preserves_same_site_and_expires() {
        let jar = CookieJar::new();
        let future_expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 3600;
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "sid".to_string(),
            value: "abc".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: "Strict".to_string(),
            expires: Some(future_expiry),
        }]);

        let cookies = jar.get_all_cookies();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].same_site, "Strict");
        assert_eq!(cookies[0].expires, Some(future_expiry));
    }

    #[test]
    fn test_set_cookies_from_cdp_session_when_expires_none() {
        let jar = CookieJar::new();
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "n".to_string(),
            value: "v".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires: None,
        }]);
        let cookies = jar.get_all_cookies();
        assert_eq!(cookies[0].expires, None);
        assert_eq!(cookies[0].same_site, DEFAULT_SAME_SITE);
    }

    #[test]
    fn test_delete_cookies_filtered_path_mismatch_preserves_cookie() {
        let jar = CookieJar::new();
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "sid".to_string(),
            value: "v".to_string(),
            domain: "example.com".to_string(),
            path: "/admin".to_string(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires: None,
        }]);
        jar.delete_cookies_filtered("sid", "example.com", Some("/other"));
        assert_eq!(jar.get_all_cookies().len(), 1);

        jar.delete_cookies_filtered("sid", "example.com", Some("/admin"));
        assert!(jar.get_all_cookies().is_empty());
    }

    #[test]
    fn test_delete_cookies_filtered_no_path_deletes_regardless() {
        let jar = CookieJar::new();
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "sid".to_string(),
            value: "v".to_string(),
            domain: "example.com".to_string(),
            path: "/admin".to_string(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires: None,
        }]);
        jar.delete_cookies_filtered("sid", "example.com", None);
        assert!(jar.get_all_cookies().is_empty());
    }

    #[test]
    fn test_set_cookies_from_cdp_expired_does_not_persist() {
        let jar = CookieJar::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "old".to_string(),
            value: "v".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires: Some(now - 1),
        }]);
        let url = Url::parse("https://example.com/").unwrap();
        assert!(jar.get_cookie_header(&url).is_empty());
    }

    #[test]
    fn test_same_name_cookies_with_different_paths_coexist() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("n=a; Path=/", &url);
        jar.set_cookie("n=b; Path=/app", &url);

        let app_url = Url::parse("https://example.com/app/page").unwrap();
        let header = jar.get_cookie_header(&app_url);
        assert!(header.contains("n=a"), "root cookie missing in '{header}'");
        assert!(header.contains("n=b"), "app cookie missing in '{header}'");
    }

    #[test]
    fn test_same_name_same_path_cookie_is_replaced() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("n=a; Path=/app", &url);
        jar.set_cookie("n=b; Path=/app", &url);

        let app_url = Url::parse("https://example.com/app/page").unwrap();
        let header = jar.get_cookie_header(&app_url);
        assert!(header.contains("n=b"));
        assert!(!header.contains("n=a"));
    }

    #[test]
    fn test_max_age_zero_deletes_only_matching_path() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("n=root; Path=/", &url);
        jar.set_cookie("n=app; Path=/app", &url);

        jar.set_cookie("n=gone; Path=/app; Max-Age=0", &url);

        let app_url = Url::parse("https://example.com/app/page").unwrap();
        let header = jar.get_cookie_header(&app_url);
        assert!(header.contains("n=root"), "root-path cookie must survive");
        assert!(!header.contains("n=app"), "app-path cookie must be deleted");
    }

    #[test]
    fn test_expired_update_deletes_existing_cookie() {
        // An expired Set-Cookie is a deletion, not a no-op: previously the old
        // cookie survived because exp<now just returned early.
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/app/login").unwrap();
        jar.set_cookie("s=old", &url);
        let app_url = Url::parse("https://example.com/app/dashboard").unwrap();
        assert!(jar.get_cookie_header(&app_url).contains("s=old"));

        jar.set_cookie("s=new; Expires=Thu, 01 Jan 2020 00:00:00 GMT", &url);
        assert!(jar.get_cookie_header(&app_url).is_empty());
    }

    #[test]
    fn test_default_cookie_path_is_request_directory() {
        assert_eq!(default_cookie_path("/app/login"), "/app");
        assert_eq!(default_cookie_path("/app/"), "/app");
        assert_eq!(default_cookie_path("/"), "/");
        assert_eq!(default_cookie_path("/x"), "/");
        assert_eq!(default_cookie_path(""), "/");

        // Cookie set on /app/login must match /app/dashboard.
        let jar = CookieJar::new();
        let login = Url::parse("https://example.com/app/login").unwrap();
        jar.set_cookie("sess=1", &login);
        let dashboard = Url::parse("https://example.com/app/dashboard").unwrap();
        assert!(jar.get_cookie_header(&dashboard).contains("sess=1"));
    }

    #[test]
    fn test_path_match_requires_slash_boundary() {
        assert!(path_matches("/admin", "/admin"));
        assert!(path_matches("/admin/users", "/admin"));
        assert!(path_matches("/admin/", "/admin/"));
        assert!(!path_matches("/administrator", "/admin"));
        assert!(!path_matches("/api", "/app"));

        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/admin").unwrap();
        jar.set_cookie("a=1; Path=/admin", &url);
        let sibling = Url::parse("https://example.com/administrator").unwrap();
        assert!(jar.get_cookie_header(&sibling).is_empty(), "Path=/admin must not leak to /administrator");
    }

    #[test]
    fn test_unrelated_domain_attr_falls_back_to_host_only() {
        // A response from sub.example.com claiming Domain=other.com must not
        // scope a cookie to other.com (cookie tossing).
        let jar = CookieJar::new();
        let url = Url::parse("https://sub.example.com/").unwrap();
        jar.set_cookie("evil=1; Domain=other.com", &url);

        assert!(jar.get_cookie_header(&Url::parse("https://other.com/").unwrap()).is_empty());
        // Falls back to host-only on the origin.
        assert!(jar.get_cookie_header(&url).contains("evil=1"));
    }

    #[test]
    fn test_host_only_cookie_not_sent_to_subdomain() {
        let jar = CookieJar::new();
        let apex = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("h=1", &apex); // no Domain attr → host-only

        assert!(jar.get_cookie_header(&apex).contains("h=1"));
        let sub = Url::parse("https://www.example.com/").unwrap();
        assert!(jar.get_cookie_header(&sub).is_empty(), "host-only cookie must not leak to subdomains");
    }

    #[test]
    fn test_samesite_normalized() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("a=1; SameSite=strict", &url);
        let cookies = jar.get_all_cookies();
        assert_eq!(cookies[0].same_site, "Strict");
    }

    #[test]
    fn test_save_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.json");

        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.set_cookie("session=abc123; Domain=example.com; Path=/", &url);
        jar.set_cookie("token=xyz; Secure; HttpOnly", &url);

        jar.save_to_file(&path).unwrap();
        assert!(path.exists());

        let jar2 = CookieJar::new();
        let count = jar2.load_from_file(&path).unwrap();
        assert_eq!(count, 2);

        let header = jar2.get_cookie_header(&url);
        assert!(header.contains("session=abc123"));
        assert!(header.contains("token=xyz"));
    }

    #[test]
    fn test_load_nonexistent_file_returns_zero() {
        let jar = CookieJar::new();
        let count = jar
            .load_from_file(std::path::Path::new("/nonexistent/cookies.json"))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_domain_matches_subdomain_without_leading_dot() {
        let jar = CookieJar::new();
        jar.set_cookies_from_cdp(vec![CookieInfo {
            name: "session".to_string(),
            value: "abc".to_string(),
            domain: "xiaohongshu.com".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: true,
            same_site: String::new(),
            expires: None,
        }]);
        let url = Url::parse("https://www.xiaohongshu.com/explore").unwrap();
        let header = jar.get_cookie_header(&url);
        assert!(header.contains("session=abc"), "Cookie header was: '{}'", header);
    }

    #[test]
    fn test_cookie_from_file_load_then_send_in_request() {
        // Simulate what happens: load cookies from file → navigate → cookie should be in request
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.json");
        
        // Write cookies like we exported from Chrome
        let cookies = serde_json::json!([
            {"name": "a1", "value": "testval", "domain": "xiaohongshu.com", "path": "/", "secure": false, "httpOnly": false},
            {"name": "web_session", "value": "sess123", "domain": "xiaohongshu.com", "path": "/", "secure": false, "httpOnly": true},
        ]);
        std::fs::write(&path, serde_json::to_string(&cookies).unwrap()).unwrap();
        
        let jar = CookieJar::new();
        let count = jar.load_from_file(&path).unwrap();
        assert_eq!(count, 2, "Should load 2 cookies");
        
        let url = Url::parse("https://www.xiaohongshu.com/explore").unwrap();
        let header = jar.get_cookie_header(&url);
        assert!(header.contains("a1=testval"), "Missing a1 in: '{}'", header);
        assert!(header.contains("web_session=sess123"), "Missing web_session in: '{}'", header);
    }
}
