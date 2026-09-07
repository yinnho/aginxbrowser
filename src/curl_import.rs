//! `import-curl`: turn a DevTools "Copy as cURL" command into a logged-in
//! browser session. The user does the hard part of a login (CAPTCHA, SMS,
//! sliders) in their real Chrome, right-clicks any authenticated request in
//! the Network panel, and pastes the copied cURL here — the engine parses
//! out the Cookie header / `-b` jar and injects it into a fresh session
//! anchored at the request's own URL, so the agent starts where the human
//! left off. Zero client-side install: this is the credential-transfer path
//! that works on a stock headless engine (no extension, no debug port).

/// The interesting parts of one copied cURL command.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedCurl {
    /// Request URL (first non-flag argument).
    pub url: String,
    /// Lower-cased header names → raw values, order preserved.
    pub headers: Vec<(String, String)>,
    /// Cookie pairs from every source (`-H cookie:`, `-b`, `--cookie`),
    /// as bare `name=value` entries ready for session injection.
    pub cookies: Vec<String>,
    /// `-b NAME=VALUE` or `--cookie` with a file path instead of pairs.
    pub cookie_file: Option<String>,
    /// Request method if explicitly set with `-X`; GET otherwise.
    pub method: Option<String>,
    /// `-d`/`--data*` request body (any variant; `-G` demotion ignored).
    pub data: Option<String>,
    /// `-u user:pass` basic-auth pair, kept raw (not injected into pages).
    pub user: Option<String>,
}

/// Split a copied cURL command into shell-ish tokens. Handles the three
/// DevTools flavors of line continuation (bash `\`, PowerShell `` ` ``,
/// cmd `^`) and bash's `$'...'` ANSI-C quoting, which Chrome emits for
/// request bodies containing newlines. Unquoted `\` escapes the next char;
/// inside `"..."` only `"`/`\`/`$` are escapes and a doubled `""` is a
/// literal quote (cmd flavor pastes those).
fn tokenize(cmd: &str) -> Vec<String> {
    // Line continuations first, before any quoting logic sees them: a
    // trailing `\` (bash), backtick (PowerShell) or `^` (cmd) joins lines.
    let joined = cmd
        .replace("\\\r\n", " ")
        .replace("\\\n", " ")
        .replace("`\r\n", " ")
        .replace("`\n", " ")
        .replace("^\r\n", " ")
        .replace("^\n", " ");

    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut started = false; // current token has content (quotes count)
    let mut chars = joined.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for cc in chars.by_ref() {
                    if cc == '\'' {
                        break;
                    }
                    cur.push(cc);
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => {
                            // cmd.exe flavor doubles quotes inside a quoted
                            // arg; a third quote continues the same token.
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                cur.push('"');
                            } else {
                                break;
                            }
                        }
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\' | '$')) => cur.push(e),
                            Some(other) => {
                                cur.push('\\');
                                cur.push(other);
                            }
                            None => cur.push('\\'),
                        },
                        Some(cc) => cur.push(cc),
                        None => break,
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                // ANSI-C quoting: $'\n' → newline, $'\'' → quote, ...
                chars.next();
                started = true;
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('t') => cur.push('\t'),
                            Some('r') => cur.push('\r'),
                            Some('0') => cur.push('\0'),
                            Some(e @ ('\\' | '\'' | '"')) => cur.push(e),
                            Some(other) => {
                                cur.push('\\');
                                cur.push(other);
                            }
                            None => cur.push('\\'),
                        },
                        Some('\'') => break,
                        Some(cc) => cur.push(cc),
                        None => break,
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(e) = chars.next() {
                    cur.push(e);
                }
            }
            other => {
                started = true;
                cur.push(other);
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

/// Parse a copied cURL command. Unknown flags are ignored (the goal is the
/// credential + target, not replaying curl); flag values that are not
/// recognized are skipped so they can't masquerade as the URL.
pub fn parse_curl(cmd: &str) -> Result<ParsedCurl, String> {
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return Err("empty command".into());
    }
    let mut p = ParsedCurl::default();
    let mut iter = tokens.into_iter().peekable();
    // Skip the binary name (curl, curl.exe, or a path ending in curl).
    if let Some(first) = iter.peek() {
        let bare = first.rsplit('/').next().unwrap_or(first);
        if bare.eq_ignore_ascii_case("curl") || bare.eq_ignore_ascii_case("curl.exe") || bare == "wget" {
            iter.next();
        }
    }
    let mut url_seen = false;
    // Flags whose value comes as the NEXT token; anything else before the
    // URL that starts with '-' is treated as consuming a value only when
    // listed here.
    while let Some(tok) = iter.next() {
        // Long flags use `--name` or `--name=value`; short flags are a
        // single char with the value attached directly (`-H'cookie: x'`,
        // `-XPOST`, `-b=v`) or as the next token (`-H 'cookie: x'`).
        let (flag, attached): (String, Option<String>) = if let Some(rest) = tok.strip_prefix("--") {
            match rest.split_once('=') {
                Some((f, v)) => (f.to_string(), Some(v.to_string())),
                None => (rest.to_string(), None),
            }
        } else if let Some(rest) = tok.strip_prefix('-') {
            let mut chars = rest.chars();
            match chars.next() {
                Some(f) => {
                    let mut val: String = chars.collect();
                    if val.starts_with('=') {
                        val.remove(0);
                    }
                    (f.to_string(), (!val.is_empty()).then_some(val))
                }
                // A lone `-` (stdin marker) or `--` terminator: skip.
                None => continue,
            }
        } else {
            // Bare argument: the first one is the URL.
            if !url_seen {
                p.url = tok.clone();
                url_seen = true;
            }
            continue;
        };
        let flag = flag.as_str();
        let value = |it: &mut std::iter::Peekable<std::vec::IntoIter<String>>| -> Option<String> {
            attached.clone().or_else(|| it.next())
        };
        match flag {
            "H" | "header" => {
                if let Some(v) = value(&mut iter) {
                    if let Some((name, val)) = v.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        let val = val.trim().to_string();
                        if name == "cookie" {
                            p.cookies.extend(split_cookie_pairs(&val));
                        } else {
                            p.headers.push((name, val));
                        }
                    }
                }
            }
            "b" | "cookie" => {
                if let Some(v) = value(&mut iter) {
                    // A cookie string is `name=value` pairs; anything
                    // without a single `=` (cookies.txt, /path/to/jar) is a
                    // file we can't read from a pasted command.
                    if v.contains('=') {
                        p.cookies.extend(split_cookie_pairs(&v));
                    } else {
                        p.cookie_file = Some(v);
                    }
                }
            }
            "A" | "user-agent" => {
                if let Some(v) = value(&mut iter) {
                    p.headers.push(("user-agent".into(), v));
                }
            }
            "e" | "referer" => {
                if let Some(v) = value(&mut iter) {
                    p.headers.push(("referer".into(), v));
                }
            }
            "u" | "user" => p.user = value(&mut iter),
            "X" | "request" => p.method = value(&mut iter),
            "d" | "data" | "data-raw" | "data-binary" | "data-ascii" | "data-urlencode" => {
                p.data = value(&mut iter);
            }
            // Valueless flags and flags we deliberately ignore.
            _ => {
                let value_taking = [
                    "o", "output", "x", "proxy", "U", "proxy-user", "m", "max-time",
                    "connect-timeout", "c", "cookie-jar", "C", "continue-at",
                    "D", "dump-header", "F", "form", "form-string", "T", "upload-file",
                    "w", "write-out", "y", "speed-time", "Y", "speed-limit",
                    "z", "time-cond", "retry", "interface", "local-port", "resolve",
                ];
                if value_taking.contains(&flag) {
                    let _ = value(&mut iter);
                }
            }
        }
    }
    if !url_seen || p.url.is_empty() {
        return Err("no URL found in the command".into());
    }
    if !(p.url.starts_with("http://") || p.url.starts_with("https://")) {
        return Err(format!("URL must be http(s): {}", p.url));
    }
    if p.cookie_file.is_some() && p.cookies.is_empty() {
        return Err("cookie jar came from a file (-b FILE) which cannot be imported — copy a request instead".into());
    }
    Ok(p)
}

/// A Cookie header value (`a=1; b=2; ...`) into bare `name=value` entries.
/// Pairs without `=` (rare cookie attributes like `Secure` leaking in) are
/// dropped rather than poisoning the jar.
fn split_cookie_pairs(header: &str) -> Vec<String> {
    header
        .split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() || !pair.contains('=') {
                return None;
            }
            // Attributes that can ride along in a -b string have no value
            // we want; pairs where the name would be one are skipped.
            let name = pair.split('=').next().unwrap_or("").trim();
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "secure" | "httponly" | "samesite" | "path" | "domain" | "max-age" | "expires"
            ) {
                return None;
            }
            Some(pair.to_string())
        })
        .collect()
}

/// Widen a copied Cookie header's bare `name=value` pairs to the URL's
/// registrable domain. Header pairs carry no Domain attribute, and the copy
/// is usually made on a different subdomain than the ones the site's data
/// APIs live on (taobao shops: the page is shopN.taobao.com but its data
/// comes from h5api.m.taobao.com — host-only anchoring sent that POST zero
/// cookies and MTop answered FAIL_SYS_ILLEGAL_ACCESS with an empty body).
/// The real browser held almost all of these as registrable-domain cookies;
/// anchoring there is the best reconstruction of the attribute the header
/// dropped.
fn widen_cookie_scope(cookies: Vec<String>, url: &str) -> Vec<String> {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    else {
        return cookies;
    };
    let domain = crate::rate::registrable_domain(&host);
    cookies
        .into_iter()
        .map(|c| format!("{}; Domain={}; Path=/", c, domain))
        .collect()
}

/// Parse a copied cURL command and turn its login state into a live session
/// navigating to the copied URL. Shared by the HTTP `/import/curl` endpoint
/// and the MCP `import_curl` tool so the two can't drift.
pub async fn create_session_from_curl(cmd: &str, use_proxy: bool) -> Result<serde_json::Value, String> {
    let parsed = parse_curl(cmd)?;
    if parsed.cookies.is_empty() {
        return Err("no Cookie header or -b jar found — copy the command from a request made while logged in (an XHR on the logged-in page usually carries the most complete cookie set)".into());
    }
    let host = url::Url::parse(&parsed.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let ua = parsed
        .headers
        .iter()
        .find(|(n, _)| n == "user-agent")
        .map(|(_, v)| v.clone());
    let authorization = parsed
        .headers
        .iter()
        .find(|(n, _)| n == "authorization")
        .map(|(_, v)| format!("{}…", &v[..v.len().min(16)]));
    let method = parsed.method.clone().unwrap_or_else(|| "GET".into());
    let has_body = parsed.data.is_some();
    let cookie_count = parsed.cookies.len();

    let mut mgr = crate::session::SESSIONS.lock().await;
    mgr.evict_expired();
    let id = mgr.create(
        Some(&parsed.url),
        use_proxy,
        widen_cookie_scope(parsed.cookies, &parsed.url),
        None,
        None,
        None,
        false,
        false,
    );
    let expires_in_secs = mgr.expires_in_secs(&id);
    Ok(serde_json::json!({
        "session_id": id,
        "url": parsed.url,
        "host": host,
        "cookie_count": cookie_count,
        "method": method,
        "has_body": has_body,
        "user_agent": ua,
        "authorization_prefix": authorization,
        "expires_in_secs": expires_in_secs,
        // Cookie pairs are scoped to the registrable domain (taobao.com),
        // since the real browser held them domain-wide and the site's data
        // APIs usually live on a sibling subdomain of the copied URL.
        "note": "login state injected; navigate with session tools",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH: &str = r#"curl 'https://www.example.com/member/home?tab=orders' \
  -H 'accept: text/html,application/xhtml+xml' \
  -H 'cookie: sid=S1; token=T%3D2; _ga=GA1.2.33' \
  -H 'user-agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/145.0.0.0' \
  --compressed"#;

    #[test]
    fn bash_multiline_with_headers() {
        let p = parse_curl(BASH).unwrap();
        assert_eq!(p.url, "https://www.example.com/member/home?tab=orders");
        assert_eq!(p.cookies.len(), 3);
        assert!(p.cookies.contains(&"sid=S1".to_string()));
        assert!(p.cookies.contains(&"token=T%3D2".to_string()));
        assert_eq!(
            p.headers.iter().find(|(n, _)| n == "user-agent").map(|(_, v)| v.as_str()),
            Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/145.0.0.0")
        );
        assert!(p.headers.iter().any(|(n, _)| n == "accept"));
    }

    #[test]
    fn data_raw_ansi_c_quoting() {
        let cmd = r#"curl 'https://api.example.com/login' --data-raw $'a=1\nb=2' -H 'cookie: x=y'"#;
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.data.as_deref(), Some("a=1\nb=2"));
        assert_eq!(p.cookies, vec!["x=y".to_string()]);
    }

    #[test]
    fn attached_short_flag_and_b_jar() {
        let cmd = r#"curl -H'cookie: a=1; b=2' -b 'c=3' -X POST -d 'k=v' 'https://x.io/a'"#;
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.url, "https://x.io/a");
        assert_eq!(p.cookies.len(), 3);
        assert_eq!(p.method.as_deref(), Some("POST"));
        assert_eq!(p.data.as_deref(), Some("k=v"));
    }

    #[test]
    fn powershell_flavor() {
        let cmd = "curl.exe 'https://ps.example.com/hello' `\n  -H 'cookie: s=42' `\n  --compressed";
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.url, "https://ps.example.com/hello");
        assert_eq!(p.cookies, vec!["s=42".to_string()]);
    }

    #[test]
    fn cmd_flavor_doubled_quotes() {
        let cmd = "curl \"https://c.example.com/q?a=\\\"b\\\"\" -H \"cookie: k=v\"";
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.url, "https://c.example.com/q?a=\"b\"");
        assert_eq!(p.cookies, vec!["k=v".to_string()]);
    }

    #[test]
    fn unknown_value_flags_do_not_eat_the_url() {
        // -o and --max-time take values; -k and --compressed don't. If the
        // value-taking list were wrong the URL would land in a flag slot.
        let cmd = r#"curl -k --compressed -o /dev/null --max-time 30 'https://u.example.com/' -H 'cookie: q=1'"#;
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.url, "https://u.example.com/");
        assert_eq!(p.cookies, vec!["q=1".to_string()]);
    }

    #[test]
    fn attribute_like_pairs_are_dropped() {
        let p = parse_curl(r#"curl 'https://a.io/' -b 'sid=9; Secure; Path=/'"#).unwrap();
        assert_eq!(p.cookies, vec!["sid=9".to_string()]);
    }

    #[test]
    fn cookie_file_is_rejected_not_guessed() {
        let err = parse_curl(r#"curl 'https://a.io/' -b cookies.txt"#).unwrap_err();
        assert!(err.contains("file"), "{err}");
    }

    #[test]
    fn errors() {
        assert!(parse_curl("").is_err());
        assert!(parse_curl("curl -k --compressed").is_err()); // no URL
        assert!(parse_curl("curl 'ftp://a.io/x'").is_err()); // not http(s)
    }

    #[test]
    fn bare_pairs_widen_to_the_registrable_domain() {
        // The copy was made on one subdomain; the site's data APIs usually
        // live on a sibling (taobao shops: page on shopN.taobao.com, MTop
        // POST on h5api.m.taobao.com — host-only anchoring sent that POST
        // zero cookies and MTop answered FAIL_SYS_ILLEGAL_ACCESS).
        let p = parse_curl(BASH).unwrap();
        let widened = widen_cookie_scope(p.cookies.clone(), &p.url);
        assert!(widened.iter().all(|c| c.ends_with("; Domain=example.com; Path=/")), "{widened:?}");
        // Interop: the session layer anchors a Domain= entry at its own
        // domain, which is what makes the cookie reach sibling subdomains.
        let (full, anchor) = crate::server::normalize_cookie_entry(&widened[0], &p.url);
        assert!(full.contains("Domain=example.com"), "{full}");
        assert_eq!(anchor, "https://example.com/");

        let shop = parse_curl(
            r#"curl 'https://shop437121404.taobao.com/category.htm' -H 'cookie: unb=3468643797; cookie2=1'"#,
        )
        .unwrap();
        let widened = widen_cookie_scope(shop.cookies, &shop.url);
        assert!(widened.iter().all(|c| c.ends_with("; Domain=taobao.com; Path=/")), "{widened:?}");
    }

    #[test]
    fn widening_honors_multi_part_suffixes() {
        let p = parse_curl(r#"curl 'https://www.example.co.uk/account' -H 'cookie: sid=S1'"#).unwrap();
        let widened = widen_cookie_scope(p.cookies, &p.url);
        assert_eq!(widened, vec!["sid=S1; Domain=example.co.uk; Path=/".to_string()]);
    }

    #[test]
    fn two_label_hosts_anchor_directly() {
        let p = parse_curl(r#"curl 'https://example.com/' -H 'cookie: sid=S1'"#).unwrap();
        let widened = widen_cookie_scope(p.cookies, &p.url);
        assert_eq!(widened, vec!["sid=S1; Domain=example.com; Path=/".to_string()]);
    }
}
