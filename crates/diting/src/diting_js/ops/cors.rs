//! CORS pure helpers (god-file ratchet split from mod.rs): origin
//! derivation, the safelisted-request-header rules, and the preflight
//! authorization predicates the fetch walk consults. Byte/token predicates
//! stay private to this module.

use super::FetchCredentials;

pub(crate) fn request_origin(request_url: &str) -> Option<String> {
    url::Url::parse(request_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// A CORS response (or preflight) must match the credentials mode:
/// credentialed requests require the exact origin plus
/// Access-Control-Allow-Credentials: true.
pub(crate) fn cors_response_allows(
    credentials: FetchCredentials,
    page_origin: &str,
    allowed_origin: &str,
    allow_credentials: &str,
) -> bool {
    if credentials == FetchCredentials::Include {
        allowed_origin == page_origin && allow_credentials == "true"
    } else {
        allowed_origin == "*" || allowed_origin == page_origin
    }
}

pub(crate) fn is_cors_safelisted_method(method: &reqwest::Method) -> bool {
    matches!(method.as_str(), "GET" | "HEAD" | "POST")
}

fn is_cors_unsafe_request_header_byte(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t')
        || matches!(
            byte,
            b'"' | b'(' | b')' | b':' | b'<' | b'>' | b'?' | b'@' | b'[' | b'\\'
                | b']' | b'{' | b'}' | 0x7f
        )
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

pub(crate) fn is_cors_safelisted_content_type(value: &str) -> bool {
    if value.bytes().any(is_cors_unsafe_request_header_byte) {
        return false;
    }

    // A MIME type must have a valid type/subtype before its parameters. This
    // is deliberately narrower than merely splitting at ';': malformed values
    // must not turn an application/json request into a simple request.
    let essence = value
        .split_once(';')
        .map_or(value, |(essence, _)| essence)
        .trim_matches([' ', '\t']);
    let Some((type_, subtype)) = essence.split_once('/') else {
        return false;
    };
    if type_.is_empty()
        || subtype.is_empty()
        || !type_.bytes().all(is_http_token_byte)
        || !subtype.bytes().all(is_http_token_byte)
    {
        return false;
    }

    essence.eq_ignore_ascii_case("application/x-www-form-urlencoded")
        || essence.eq_ignore_ascii_case("multipart/form-data")
        || essence.eq_ignore_ascii_case("text/plain")
}

fn decimal_is_at_most(left: &str, right: &str) -> bool {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len() < right.len() || (left.len() == right.len() && left <= right)
}

fn is_cors_safelisted_range(value: &str) -> bool {
    let Some(range) = value.strip_prefix("bytes=") else {
        return false;
    };
    let Some((start, end)) = range.split_once('-') else {
        return false;
    };
    if start.is_empty()
        || !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    end.is_empty() || decimal_is_at_most(start, end)
}

pub(crate) fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    if name.eq_ignore_ascii_case("accept") {
        return !value.bytes().any(is_cors_unsafe_request_header_byte);
    }
    if name.eq_ignore_ascii_case("accept-language")
        || name.eq_ignore_ascii_case("content-language")
    {
        return value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
        });
    }
    if name.eq_ignore_ascii_case("content-type") {
        return is_cors_safelisted_content_type(value);
    }
    if name.eq_ignore_ascii_case("range") {
        return is_cors_safelisted_range(value);
    }
    false
}

/// Return the sorted, lowercase header names that must be authorized by a
/// CORS preflight. The aggregate safelist cap is observable only on unusual
/// requests and does not add work to same-origin requests.
pub(crate) fn cors_unsafe_request_header_names(headers: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut safelist_value_size = 0usize;

    for (name, value) in headers {
        if is_cors_safelisted_request_header(name, value) {
            safelist_value_size = safelist_value_size.saturating_add(value.len());
        } else {
            unsafe_names.push(name.to_ascii_lowercase());
        }
    }
    if safelist_value_size > 1024 {
        unsafe_names.extend(
            headers
                .iter()
                .filter(|(name, value)| is_cors_safelisted_request_header(name, value))
                .map(|(name, _)| name.to_ascii_lowercase()),
        );
    }
    unsafe_names.sort_unstable();
    unsafe_names.dedup();
    unsafe_names
}

pub(crate) fn parse_cors_header_list<'a>(
    headers: &'a reqwest::header::HeaderMap,
    name: &'static str,
) -> Option<Vec<&'a str>> {
    let mut items = Vec::new();
    for value in headers.get_all(name).iter() {
        let value = value.to_str().ok()?;
        for item in value.split(',') {
            let item = item.trim_matches([' ', '\t']);
            if item.is_empty() || !item.bytes().all(is_http_token_byte) {
                return None;
            }
            items.push(item);
        }
    }
    Some(items)
}

pub(crate) fn preflight_allows_method(method: &reqwest::Method, allowed: &[&str], credentialed: bool) -> bool {
    is_cors_safelisted_method(method)
        || allowed.iter().any(|allowed| {
            *allowed == method.as_str() || (*allowed == "*" && !credentialed)
        })
}

pub(crate) fn preflight_allows_header(name: &str, allowed: &[&str], credentialed: bool) -> bool {
    allowed
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
        || (!name.eq_ignore_ascii_case("authorization")
            && !credentialed
            && allowed.contains(&"*"))
}

