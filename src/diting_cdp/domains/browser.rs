use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;

const MAX_CONTENTS_DIMENSION: i64 = 10_000_000;

fn contents_dimension(params: &Value, name: &str) -> Result<u32, String> {
    let value = params
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Browser.setContentsSize requires integer {name}"))?;
    if !(1..=MAX_CONTENTS_DIMENSION).contains(&value) {
        return Err(format!(
            "Browser.setContentsSize {name} must be between 1 and {MAX_CONTENTS_DIMENSION}"
        ));
    }
    Ok(value as u32)
}

/// The only "window" a headless target has is its viewport; report the live
/// override (what resize round-trips read back) instead of a fixed 1280x720.
fn window_bounds(ctx: &mut CdpContext, session_id: &Option<String>) -> Value {
    let (w, h) = ctx
        .get_session_page(session_id)
        .and_then(|p| p.viewport_override())
        .map(|(w, h, _)| (w as i64, h as i64))
        .unwrap_or((1280, 720));
    json!({ "left": 0, "top": 0, "width": w, "height": h, "windowState": "normal" })
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "getVersion" => Ok(json!({
            "protocolVersion": "1.3",
            "product": "Chrome/145.0.0.0",
            "revision": "@0000000000000000000000000000000000000000",
            "userAgent": crate::diting_browser::profiles::select_profile().user_agent,
            "jsVersion": "14.5.0.0",
        })),
        "close" => Ok(json!({})),
        "getWindowForTarget" => Ok(json!({
            "windowId": 1,
            "bounds": window_bounds(ctx, session_id),
        })),
        "setDownloadBehavior" => Ok(json!({})),
        "getWindowBounds" => Ok(json!({ "bounds": window_bounds(ctx, session_id) })),
        // Chrome >=129 resize flow: resize the window *contents* — the
        // viewport — rather than the OS window bounds. chrome-devtools-mcp's
        // resize_page pairs getWindowForTarget with this call; the pinned
        // mobile/dpr emulation survives (Page::resize_contents).
        "setContentsSize" => {
            let width = contents_dimension(params, "width")?;
            let height = contents_dimension(params, "height")?;
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.resize_contents(width as f32, height as f32);
            }
            Ok(json!({}))
        }
        // No-op acks for window-management methods Playwright sends during
        // page setup. We don't model real OS windows, but answering with {}
        // lets the client's setup sequence complete instead of tearing down
        // the page on an unknown-method error.
        "setWindowBounds" => Ok(json!({})),
        // Permission grants (camera, geolocation, notifications, ...) —
        // Playwright's `context.grantPermissions()` and Puppeteer's
        // `browserContext.overridePermissions()` call these during setup.
        // We don't gate any capability behind CDP permissions, so the ack is
        // the whole implementation.
        "grantPermissions" | "resetPermissions" => Ok(json!({})),
        _ => Err(format!("Unknown Browser method: {}", method)),
    }
}
