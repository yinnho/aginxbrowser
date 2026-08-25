use std::sync::Arc;

use serde_json::{json, Value};

use crate::diting_cdp::cookie_params::{parse_cdp_cookie, parse_delete_cookies_params};
use crate::diting_cdp::dispatch::CdpContext;
use crate::diting_net::CookieJar;

const SESSION_COOKIE_EXPIRES: i64 = -1;
const DEFAULT_SECURE_PORT: u16 = 443;
const DEFAULT_INSECURE_PORT: u16 = 80;
const SOURCE_SCHEME_SECURE: &str = "Secure";
const SOURCE_SCHEME_NONSECURE: &str = "NonSecure";
const DEFAULT_SAME_SITE: &str = "Lax";

// Resolve the cookie jar for a Network request: prefer the session's page jar,
// fall back to the default browser context. Puppeteer and Playwright both call
// Network.setCookie/getCookies/deleteCookies BEFORE attaching to a target —
// requiring a session would break those flows (Storage.* mirrors this).
fn cookie_jar_for<'a>(ctx: &'a CdpContext, session_id: &Option<String>) -> &'a Arc<CookieJar> {
    ctx.get_session_page(session_id)
        .map(|p| &p.context.cookie_jar)
        .unwrap_or(&ctx.default_context.cookie_jar)
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" => Ok(json!({})),
        "disable" => {
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.clear_response_bodies();
            } else {
                for page in &mut ctx.pages {
                    page.clear_response_bodies();
                }
            }
            Ok(json!({}))
        }
        "setExtraHTTPHeaders" => {
            let headers = params.get("headers").and_then(|v| v.as_object());
            if let Some(page) = ctx.get_session_page(session_id) {
                if let Some(headers) = headers {
                    let header_map: std::collections::HashMap<String, String> = headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect();
                    page.http_client.set_extra_headers(header_map).await;
                }
            }
            Ok(json!({}))
        }
        "setUserAgentOverride" => {
            let ua = params.get("userAgent").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(page) = ctx.get_session_page(session_id) {
                page.http_client.set_user_agent(ua).await;
            }
            Ok(json!({}))
        }
        "getCookies" | "getAllCookies" => {
            let cookies = cookie_jar_for(ctx, session_id).get_all_cookies();
            let cdp_cookies: Vec<Value> = cookies.iter().map(cookie_info_to_cdp_json).collect();
            Ok(json!({ "cookies": cdp_cookies }))
        }
        "setCookie" => {
            let cookie =
                parse_cdp_cookie(params).ok_or("setCookie: missing required name/domain (or url)")?;
            cookie_jar_for(ctx, session_id).set_cookies_from_cdp(vec![cookie]);
            Ok(json!({ "success": true }))
        }
        "setCookies" => {
            if let Some(cookies) = params.get("cookies").and_then(|v| v.as_array()) {
                let parsed: Vec<_> = cookies.iter().filter_map(parse_cdp_cookie).collect();
                cookie_jar_for(ctx, session_id).set_cookies_from_cdp(parsed);
            }
            Ok(json!({}))
        }
        "deleteCookies" => {
            if let Some(filter) = parse_delete_cookies_params(params) {
                cookie_jar_for(ctx, session_id).delete_cookies_filtered(
                    &filter.name,
                    &filter.domain,
                    filter.path.as_deref(),
                );
            }
            Ok(json!({}))
        }
        "clearBrowserCookies" => {
            cookie_jar_for(ctx, session_id).clear();
            Ok(json!({}))
        }
        "setCacheDisabled" => Ok(json!({})),
        "setRequestInterception" => Ok(json!({})),
        "setBlockedURLs" => {
            let patterns = params
                .get("urls")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(ToString::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.set_blocked_urls(patterns);
            } else {
                for page in &mut ctx.pages {
                    page.set_blocked_urls(patterns.clone());
                }
            }
            Ok(json!({}))
        }
        "getResponseBody" => {
            let request_id = params
                .get("requestId")
                .and_then(|v| v.as_str())
                .ok_or("Network.getResponseBody requires requestId")?;

            let body = if let Some(page) = ctx.get_session_page(session_id) {
                page.get_response_body(request_id)
            } else {
                ctx.pages
                    .iter()
                    .find_map(|page| page.get_response_body(request_id))
            };

            match body {
                Some(body) => Ok(json!({
                    "body": body.body,
                    "base64Encoded": body.base64_encoded,
                })),
                None => Err(format!("No response body found for requestId {}", request_id)),
            }
        }
        _ => Err(format!("Unknown Network method: {}", method)),
    }
}

pub(crate) fn cookie_info_to_cdp_json(c: &crate::diting_net::cookies::CookieInfo) -> Value {
    let expires = c.expires.unwrap_or(SESSION_COOKIE_EXPIRES);
    let session = c.expires.is_none();
    let same_site = if c.same_site.is_empty() {
        DEFAULT_SAME_SITE
    } else {
        c.same_site.as_str()
    };
    json!({
        "name": c.name,
        "value": c.value,
        "domain": c.domain,
        "path": c.path,
        "expires": expires,
        "size": c.name.len() + c.value.len(),
        "httpOnly": c.http_only,
        "secure": c.secure,
        "session": session,
        "sameSite": same_site,
        "sameParty": false,
        "sourceScheme": if c.secure { SOURCE_SCHEME_SECURE } else { SOURCE_SCHEME_NONSECURE },
        "sourcePort": if c.secure { DEFAULT_SECURE_PORT } else { DEFAULT_INSECURE_PORT },
        "priority": "Medium",
    })
}
