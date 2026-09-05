//! Fetch domain: request interception (Playwright `page.route()` /
//! Puppeteer `setRequestInterception()`).
//!
//! `Fetch.enable` arms the engine's intercept kernel: every script-initiated
//! fetch()/XHR — including dynamically inserted classic `<script src>`, which
//! loads through `op_fetch_url` — is parked and surfaced to the bridge as an
//! `InterceptedRequest`. The shared drain (`dispatch::drain_intercept_calls`)
//! turns those into `Fetch.requestPaused` events after each command; the
//! client answers with `continueRequest` / `fulfillRequest` / `failRequest`,
//! which resolve the parked op.
//!
//! Boundaries: Request stage only (no Response-stage pauses). Parser-time
//! static subresources (`<script src>`, `<link rel=stylesheet>` discovered
//! during navigation) are also not parked — they load inside the
//! `Page.navigate` dispatch, and this bridge processes commands strictly
//! sequentially, so a pause created there could not be answered before the
//! dispatch that created it returns; parking one would either deadlock the
//! navigation or ride out the resolution timeout into an unobserved
//! pass-through. Hard-blocking those is `Network.setBlockedURLs` territory
//! (no client round trip, no timing dependency).

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use crate::diting_cdp::dispatch::{CdpContext, FetchInterceptState};
use crate::diting_js::ops::InterceptResolution;

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" => enable(params, ctx, session_id).await,
        "disable" => disable(ctx, session_id).await,
        "continueRequest" => resolve(params, ctx, session_id, resolve_continue).await,
        "fulfillRequest" => resolve(params, ctx, session_id, resolve_fulfill).await,
        "failRequest" => resolve(params, ctx, session_id, resolve_fail).await,
        _ => Err(format!("Unknown Fetch method: {}", method)),
    }
}

async fn enable(
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    let page_id = ctx
        .session_page_id(session_id)
        .ok_or("No page for session")?
        .clone();

    // Empty/absent patterns match every request (Puppeteer's
    // setRequestInterception default). `requestStage` is accepted but ignored —
    // this bridge pauses at the Request stage only.
    let patterns: Vec<String> = params
        .get("patterns")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.get("urlPattern")
                        .and_then(|u| u.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let page = ctx.get_page_mut(&page_id).ok_or("No page for session")?;
    page.set_fetch_intercept(Some(tx));
    ctx.fetch_intercept.insert(
        page_id,
        FetchInterceptState {
            patterns,
            rx,
            pending: Vec::new(),
        },
    );
    Ok(json!({}))
}

async fn disable(ctx: &mut CdpContext, session_id: &Option<String>) -> Result<Value, String> {
    let page_id = ctx.session_page_id(session_id).ok_or("No page")?.clone();
    if let Some(mut state) = ctx.fetch_intercept.remove(&page_id) {
        // Answer everything still parked so pending fetches resume promptly
        // (Continue semantics) instead of riding out the resolution timeout.
        for pause in state.pending.drain(..) {
            if let Some(resolver) = pause.resolver {
                let _ = resolver.send(InterceptResolution::Continue {
                    url: None,
                    method: None,
                    headers: None,
                    body: None,
                });
            }
        }
        if let Some(page) = ctx.get_page_mut(&page_id) {
            page.set_fetch_intercept(None);
            // The Continue answers above only unblock the parked futures; the
            // event loop must run for the fetches to actually proceed.
            page.settle(50).await;
        }
    }
    Ok(json!({}))
}

async fn resolve(
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    build: fn(&Value) -> Result<InterceptResolution, String>,
) -> Result<Value, String> {
    let request_id = params
        .get("requestId")
        .and_then(|v| v.as_str())
        .ok_or("requestId is required")?
        .to_string();
    let resolution = build(params)?;

    let page_id = ctx
        .session_page_id(session_id)
        .ok_or("No page for session")?
        .clone();
    let state = ctx
        .fetch_intercept.get_mut(&page_id)
        .ok_or("Fetch domain is not enabled for this session")?;
    let pause = state
        .pending
        .iter()
        .position(|p| p.bridge_request_id == request_id)
        .ok_or(format!("Unknown requestId: {}", request_id))?;
    let pause = state.pending.remove(pause);
    // Resolver gone (timeout expiry already fell through to the real
    // request): the resolution is a no-op, but not an error.
    if let Some(resolver) = pause.resolver {
        resolver
            .send(resolution)
            .map_err(|_| "Intercepted request is already gone".to_string())?;
    }
    // Run the parked fetch's continuation now — the JS promise resolves only
    // when the event loop next polls, and without this the client would wait
    // for its next command to see effects.
    if let Some(page) = ctx.get_page_mut(&page_id) {
        page.settle(50).await;
    }
    Ok(json!({}))
}

fn resolve_continue(params: &Value) -> Result<InterceptResolution, String> {
    // Engine re-validates a URL rewrite against the same SSRF gate as the
    // original request.
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let method = params
        .get("method")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let headers = header_map(params.get("headers"));
    // postData is base64 on the CDP wire; it rides byte-native from here on.
    let body = match params.get("postData").and_then(|v| v.as_str()) {
        Some(data) => Some(BASE64.decode(data).map_err(|e| {
            format!("continueRequest postData is not valid base64: {}", e)
        })?),
        None => None,
    };
    Ok(InterceptResolution::Continue {
        url,
        method,
        headers,
        body,
    })
}

fn resolve_fulfill(params: &Value) -> Result<InterceptResolution, String> {
    let status = params
        .get("responseCode")
        .and_then(|v| v.as_u64())
        .ok_or("fulfillRequest requires responseCode")? as u16;
    let headers = header_map(params.get("responseHeaders")).unwrap_or_default();
    let body = match params.get("body").and_then(|v| v.as_str()) {
        // Byte-native: decode straight to bytes so binary fulfillments
        // (images, WASM, ...) round-trip exactly; the JS side re-derives
        // text through the response's charset.
        Some(data) => BASE64
            .decode(data)
            .map_err(|e| format!("fulfillRequest body is not valid base64: {}", e))?,
        None => Vec::new(),
    };
    Ok(InterceptResolution::Fulfill {
        status,
        headers,
        body,
    })
}

fn resolve_fail(params: &Value) -> Result<InterceptResolution, String> {
    let reason = params
        .get("errorReason")
        .and_then(|v| v.as_str())
        .ok_or("failRequest requires errorReason")?
        .to_string();
    Ok(InterceptResolution::Fail { reason })
}

/// CDP wire headers are `{name, value}` entries (continueRequest /
/// fulfillRequest); a plain object is accepted too since Puppeteer's types
/// admit both shapes over the years.
fn header_map(value: Option<&Value>) -> Option<std::collections::HashMap<String, String>> {
    let value = value?;
    let mut map = std::collections::HashMap::new();
    match value {
        Value::Array(entries) => {
            for entry in entries {
                if let (Some(name), Some(val)) = (
                    entry.get("name").and_then(|v| v.as_str()),
                    entry.get("value").and_then(|v| v.as_str()),
                ) {
                    map.insert(name.to_string(), val.to_string());
                }
            }
        }
        Value::Object(entries) => {
            for (name, val) in entries {
                if let Some(val) = val.as_str() {
                    map.insert(name.clone(), val.to_string());
                }
            }
        }
        _ => return None,
    }
    Some(map)
}

/// DevTools `Fetch.enable` urlPattern glob: `*` matches zero or more
/// characters anywhere in the pattern (Chrome also supports `?`, which real
/// clients rarely use for routing). Multi-star patterns are matched as
/// prefix / ordered infixes / suffix containment, so Playwright's `**/*`
/// style globs work.
pub(crate) fn url_pattern_matches(pattern: &str, url: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    if !url.starts_with(parts[0]) {
        return false;
    }
    let mut rest = &url[parts[0].len()..];
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if i == last {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::url_pattern_matches;

    #[test]
    fn url_patterns_cover_client_shapes() {
        assert!(url_pattern_matches("*", "https://x.test/a.png"));
        assert!(url_pattern_matches("**/*", "https://x.test/a.png"));
        assert!(url_pattern_matches("**/*.png", "https://x.test/a/b.png"));
        assert!(!url_pattern_matches("**/*.png", "https://x.test/a/b.jpg"));
        assert!(url_pattern_matches("https://x.test/*", "https://x.test/api"));
        assert!(!url_pattern_matches("https://x.test/*", "https://y.test/api"));
        assert!(url_pattern_matches(
            "https://x.test/api/v1",
            "https://x.test/api/v1"
        ));
        assert!(!url_pattern_matches(
            "https://x.test/api/v1",
            "https://x.test/api/v2"
        ));
    }
}
