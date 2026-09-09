//! CDP Page domain — claimed from upstream obscura-cdp page.rs, adapted to
//! the diting engine. This is the largest domain: it owns navigation and the
//! post-navigation event sequence (`emit_navigation_events`) that Playwright's
//! `wait_for_load_state` and Puppeteer's `waitForNavigation` both key off.
//!
//! Adaptation notes vs upstream:
//! - single main frame per page (no child-frame bookkeeping — see dispatch.rs)
//! - `captureScreenshot` routes through the diting renderer, gated on the
//!   `screenshot` feature (production builds omit it, matching the HTTP API)

#[cfg(feature = "screenshot")]
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use crate::diting_browser::lifecycle::{LifecycleState, WaitUntil};
use crate::diting_browser::page::NetworkEvent;
use crate::diting_cdp::dispatch::CdpContext;
#[cfg(feature = "screenshot")]
use crate::diting_cdp::dispatch::ScreencastState;
use crate::diting_cdp::types::CdpEvent;
use crate::diting_cdp::util::url_is_file_scheme;

/// Default viewport reported by `getLayoutMetrics` and used for full-page
/// screenshots. The single-realm engine has no compositor viewport, so this is
/// a stable constant rather than a mutable per-page value.
const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 720;

fn now_epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The CDP `Frame` wire shape for the single main frame of a page.
pub(crate) fn frame_json(frame_id: &str, loader_id: &str, url: &str) -> Value {
    json!({
        "id": frame_id,
        "loaderId": loader_id,
        "url": url,
        "domainAndRegistry": "",
        "securityOrigin": "",
        "mimeType": "text/html",
        "secureContextType": "Secure",
        "crossOriginIsolatedContextType": "NotIsolated",
        "gatedAPIFeatures": [],
        "adFrameStatus": { "adFrameType": "none" },
    })
}

/// Push an event, honouring the browser-level (no session) vs session-level
/// distinction: `with_session` would otherwise stamp a bogus `sessionId: ""`
/// onto browser-connection events.
fn emit(ctx: &mut CdpContext, method: &str, params: Value, session_id: &Option<String>) {
    let ev = match session_id {
        Some(sid) => CdpEvent::with_session(method, params, sid.clone()),
        None => CdpEvent::new(method, params),
    };
    ctx.pending_events.push(ev);
}

/// Emit the full post-navigation CDP event sequence navigation-waiters key off:
/// frameStartedLoading, frameNavigated, per-request Network events,
/// domContentEventFired, loadEventFired, lifecycle events, and fresh execution
/// contexts (default + any isolated worlds).
///
/// Shared by `Page.navigate` and the post-eval drain in the Runtime domain
/// (`emit_post_eval_nav`), so a `location.href = ...` in an evaluated script
/// produces the same sequence a direct navigation does.
pub(crate) fn emit_navigation_events(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    frame_id: &str,
    loader_id: &str,
    page_url: &str,
    page_id: &str,
    network_events: &[NetworkEvent],
    _wait_until: WaitUntil,
    reached_idle: bool,
) {
    let ts = now_epoch_seconds();

    emit(
        ctx,
        "Page.frameStartedLoading",
        json!({ "frameId": frame_id }),
        session_id,
    );
    emit(
        ctx,
        "Page.frameNavigated",
        json!({
            "frame": frame_json(frame_id, loader_id, page_url),
            "type": "Navigation",
        }),
        session_id,
    );

    for ev in network_events {
        let request_id = ev.request_id.clone();
        emit(
            ctx,
            "Network.requestWillBeSent",
            json!({
                "requestId": request_id,
                "loaderId": loader_id,
                "documentURL": page_url,
                "request": {
                    "url": ev.url,
                    "method": ev.method,
                    "headers": ev.headers,
                    "initialPriority": "High",
                    "referrerPolicy": "no-referrer-when-downgrade",
                },
                "timestamp": ev.timestamp,
                "wallTime": ts,
                "initiator": { "type": "other" },
                "type": ev.resource_type,
                "frameId": frame_id,
                "hasUserGesture": false,
            }),
            session_id,
        );
        emit(
            ctx,
            "Network.responseReceived",
            json!({
                "requestId": request_id,
                "loaderId": loader_id,
                "timestamp": ev.timestamp,
                "type": ev.resource_type,
                "response": {
                    "url": ev.url,
                    "status": ev.status,
                    "statusText": "",
                    "headers": ev.response_headers.as_ref(),
                    "mimeType": "text/html",
                    "connectionReused": false,
                    "connectionId": 0,
                    "encodedDataLength": ev.body_size,
                    "securityState": "secure",
                    "protocol": "http/1.1",
                    "fromDiskCache": false,
                    "fromServiceWorker": false,
                },
                "frameId": frame_id,
            }),
            session_id,
        );
        emit(
            ctx,
            "Network.loadingFinished",
            json!({
                "requestId": request_id,
                "timestamp": ev.timestamp,
                "encodedDataLength": ev.body_size,
            }),
            session_id,
        );
    }

    emit(
        ctx,
        "Page.domContentEventFired",
        json!({ "timestamp": ts }),
        session_id,
    );
    emit(ctx, "Page.loadEventFired", json!({ "timestamp": ts }), session_id);
    emit(
        ctx,
        "Page.lifecycleEvent",
        json!({ "frameId": frame_id, "loaderId": loader_id, "name": "init", "timestamp": ts }),
        session_id,
    );
    emit(
        ctx,
        "Page.lifecycleEvent",
        json!({ "frameId": frame_id, "loaderId": loader_id, "name": "DOMContentLoaded", "timestamp": ts }),
        session_id,
    );
    emit(
        ctx,
        "Page.lifecycleEvent",
        json!({ "frameId": frame_id, "loaderId": loader_id, "name": "load", "timestamp": ts }),
        session_id,
    );
    if reached_idle {
        emit(
            ctx,
            "Page.lifecycleEvent",
            json!({ "frameId": frame_id, "loaderId": loader_id, "name": "networkIdle", "timestamp": ts }),
            session_id,
        );
    }

    emit(ctx, "Runtime.executionContextsCleared", json!({}), session_id);
    emit(
        ctx,
        "Runtime.executionContextCreated",
        json!({
            "context": {
                "id": 1,
                "origin": page_url,
                "name": "",
                "uniqueId": format!("ctx-{page_id}"),
                "auxData": {
                    "isDefault": true,
                    "type": "default",
                    "frameId": frame_id,
                }
            }
        }),
        session_id,
    );

    // Re-emit isolated-world contexts after every navigation (Playwright's
    // utility world lives in one); a fresh id each time mirrors real Chrome.
    for world in ctx.isolated_worlds.clone() {
        let context_id = ctx.next_isolated_context();
        emit(
            ctx,
            "Runtime.executionContextCreated",
            json!({
                "context": {
                    "id": context_id,
                    "origin": page_url,
                    "name": world,
                    "uniqueId": format!("ctx-{page_id}-{world}"),
                    "auxData": {
                        "isDefault": false,
                        "type": "isolated",
                        "frameId": frame_id,
                    }
                }
            }),
            session_id,
        );
    }
}

/// Drain a page's recorded navigation state (frame id, final URL, network
/// events, idle flag), mint a fresh loaderId, and emit the full
/// post-navigation event sequence. The shared tail of every "a navigation
/// just happened" site: `Page.navigate`/`reload`, the post-eval drain for
/// JS-initiated navigations, and `Target.createTarget`'s inline `url`
/// navigation. Without the last one, a page created with a URL never
/// announces itself to Page-domain waiters — frameNavigated /
/// domContentEventFired / loadEventFired never fire and chromiumoxide-style
/// clients hang waiting for the initial load (obscura#833 shape).
///
/// Returns `(frame_id, loader_id)` for callers that echo them in a command
/// response.
pub(crate) fn emit_navigation_for_page(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    page_id: &str,
) -> (String, String) {
    let (frame_id, url_str, network_events, reached_idle) = {
        let Some(page) = ctx.get_page_mut(page_id) else {
            return (String::new(), String::new());
        };
        (
            page.frame_id.clone(),
            page.url_string(),
            page.network_events.drain(..).collect::<Vec<_>>(),
            page.lifecycle == LifecycleState::NetworkIdle,
        )
    };
    let loader_id = format!("loader-{}", uuid::Uuid::new_v4());
    ctx.current_loader_ids
        .insert(page_id.to_string(), loader_id.clone());
    emit_navigation_events(
        ctx,
        session_id,
        &frame_id,
        &loader_id,
        &url_str,
        page_id,
        &network_events,
        WaitUntil::Load,
        reached_idle,
    );
    (frame_id, loader_id)
}

/// Drive a full navigation of the session page, then emit the navigation event
/// sequence. Both `Page.navigate` and `Page.reload` route through here so the
/// `allow_file_access` gate and preload-script sync cannot diverge.
async fn navigate_page(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    url: &str,
) -> Result<Value, String> {
    // Sync context-level preload scripts (Runtime.addBinding shims +
    // Page.addScriptToEvaluateOnNewDocument sources) into the page so they run
    // before the next document's own scripts. Clone first: the mutable page
    // borrow below conflicts with an immutable ctx borrow.
    let preload_sources: Vec<String> = ctx
        .preload_scripts
        .iter()
        .map(|(_, source)| source.clone())
        .collect();

    let (frame_id, page_id) = {
        let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
        if url_is_file_scheme(url) && !crate::diting_net::client::allow_file_access() {
            return Err(
                "file:// navigation is disabled. Restart with `--allow-file-access` to enable."
                    .to_string(),
            );
        }
        page.set_preload_scripts(preload_sources);
        page.navigate(url).await.map_err(|e| e.to_string())?;
        (page.frame_id.clone(), page.id.clone())
    };

    let (_frame_id, loader_id) = emit_navigation_for_page(ctx, session_id, &page_id);
    Ok(json!({ "frameId": frame_id, "loaderId": loader_id }))
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" => {
            // Report the current main frame if a page already exists, so a
            // client attaching to an existing target can register it without
            // waiting for the next navigation (getFrameTree remains
            // authoritative; this just lets frame-tracked events flow).
            let frame = ctx.get_session_page(session_id).map(|page| {
                let loader_id = ctx
                    .current_loader_ids
                    .get(&page.id)
                    .cloned()
                    .unwrap_or_else(|| format!("loader-{}", page.id));
                (page.frame_id.clone(), loader_id, page.url_string())
            });
            if let Some((frame_id, loader_id, url)) = frame {
                emit(
                    ctx,
                    "Page.frameNavigated",
                    json!({ "frame": frame_json(&frame_id, &loader_id, &url), "type": "Navigation" }),
                    session_id,
                );
            }
            Ok(json!({}))
        }
        "disable" => Ok(json!({})),
        "navigate" => {
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("url required")?;
            navigate_page(ctx, session_id, url).await
        }
        "reload" => {
            let url = ctx
                .get_session_page(session_id)
                .map(|p| p.url_string())
                .unwrap_or_else(|| "about:blank".to_string());
            navigate_page(ctx, session_id, &url).await
        }
        "getFrameTree" => {
            let page = ctx.get_session_page(session_id).ok_or("No page")?;
            let loader_id = ctx
                .current_loader_ids
                .get(&page.id)
                .cloned()
                .unwrap_or_else(|| format!("loader-{}", page.id));
            let frame_id = page.frame_id.clone();
            let url = page.url_string();
            Ok(json!({
                "frameTree": {
                    "frame": frame_json(&frame_id, &loader_id, &url),
                    "childFrames": [],
                }
            }))
        }
        "getNavigationHistory" => {
            let page = ctx.get_session_page(session_id).ok_or("No page")?;
            let entries: Vec<Value> = page
                .history
                .iter()
                .enumerate()
                .map(|(i, url)| {
                    json!({ "id": i, "url": url, "title": page.title, "userTypedURL": url })
                })
                .collect();
            Ok(json!({ "currentIndex": page.history_index, "entries": entries }))
        }
        "resetNavigationHistory" => Ok(json!({})),
        "navigateToHistoryEntry" => {
            let entry_id = params.get("entryId").and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            let url = {
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                let Some(url) = page.history.get(entry_id).cloned() else {
                    return Err(format!("History entry {entry_id} not found"));
                };
                page.set_history_index(entry_id);
                url
            };
            navigate_page(ctx, session_id, &url).await
        }
        "addScriptToEvaluateOnNewDocument" => {
            let source = params
                .get("source")
                .and_then(|v| v.as_str())
                .ok_or("source required")?;
            ctx.preload_counter += 1;
            let identifier = format!("__diting_preload_{}", ctx.preload_counter);
            ctx.preload_scripts
                .push((identifier.clone(), source.to_string()));
            Ok(json!({ "identifier": identifier }))
        }
        "removeScriptToEvaluateOnNewDocument" => {
            let identifier = params
                .get("identifier")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            ctx.preload_scripts.retain(|(k, _)| k != identifier);
            Ok(json!({}))
        }
        "createIsolatedWorld" => {
            let world_name = params
                .get("worldName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let (frame_id, page_url, page_id) = {
                let page = ctx.get_session_page(session_id).ok_or("No page")?;
                let frame_id = params
                    .get("frameId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| page.frame_id.clone());
                (frame_id, page.url_string(), page.id.clone())
            };
            let context_id = ctx.next_isolated_context();
            ctx.isolated_worlds.push(world_name.to_string());
            emit(
                ctx,
                "Runtime.executionContextCreated",
                json!({
                    "context": {
                        "id": context_id,
                        "origin": page_url,
                        "name": world_name,
                        "uniqueId": format!("ctx-{page_id}-{world_name}"),
                        "auxData": {
                            "isDefault": false,
                            "type": "isolated",
                            "frameId": frame_id,
                        }
                    }
                }),
                session_id,
            );
            Ok(json!({ "executionContextId": context_id }))
        }
        "getLayoutMetrics" => {
            // Real values (AginxOS P1): the page's effective viewport — the
            // pinned emulation override, else the persona viewport, else the
            // 1280x720 default — and the document's scrollable content size
            // from the same layout the band paint uses (a 1x1 band paints
            // nothing but walks the content extent; the layout is the cached
            // run every rect consumer shares).
            #[cfg(feature = "screenshot")]
            {
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                let (w, h) = page.effective_viewport();
                let content = page
                    .viewport_band_frame(0.0, 0.0, (1.0, 1.0))
                    .map(|(f, _)| f.content_size)
                    .unwrap_or((w, h));
                let (cw, ch) = (content.0.max(1.0) as i64, content.1.max(1.0) as i64);
                let viewport = json!({
                    "pageX": 0, "pageY": 0,
                    "clientWidth": w as i64, "clientHeight": h as i64,
                });
                let visual = json!({
                    "offsetX": 0, "offsetY": 0,
                    "pageX": 0, "pageY": 0,
                    "clientWidth": w as i64, "clientHeight": h as i64,
                    "scale": 1, "zoom": 1,
                });
                let content = json!({ "x": 0, "y": 0, "width": cw, "height": ch });
                Ok(json!({
                    "layoutViewport": viewport,
                    "visualViewport": visual,
                    "contentSize": content,
                    "cssLayoutViewport": viewport,
                    "cssVisualViewport": visual,
                    "cssContentSize": content,
                }))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                let (w, h) = (DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT);
                let viewport = json!({
                    "pageX": 0, "pageY": 0,
                    "clientWidth": w, "clientHeight": h,
                });
                let visual = json!({
                    "offsetX": 0, "offsetY": 0,
                    "pageX": 0, "pageY": 0,
                    "clientWidth": w, "clientHeight": h,
                    "scale": 1, "zoom": 1,
                });
                let content = json!({ "x": 0, "y": 0, "width": w, "height": h });
                Ok(json!({
                    "layoutViewport": viewport,
                    "visualViewport": visual,
                    "contentSize": content,
                    "cssLayoutViewport": viewport,
                    "cssVisualViewport": visual,
                    "cssContentSize": content,
                }))
            }
        }
        "captureScreenshot" => {
            #[cfg(feature = "screenshot")]
            {
                let params = params.clone();
                let beyond = params
                    .get("captureBeyondViewport")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let format = params
                    .get("format")
                    .and_then(Value::as_str)
                    .map(|f| f.to_ascii_lowercase())
                    .unwrap_or_else(|| "png".to_string());
                if let Some(clip) = params.get("clip") {
                    let scale = clip.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                    if (scale - 1.0).abs() > f64::EPSILON {
                        return Err(
                            "Page.captureScreenshot: clip.scale != 1 is not supported".to_string()
                        );
                    }
                }
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                if beyond && params.get("clip").is_none() {
                    // Legacy full-page path, byte-identical behavior for
                    // every client that doesn't opt into viewport capture
                    // (Puppeteer/Playwright defaults, the HTTP /screenshot
                    // surface).
                    let (html, url) = {
                        let v = page.evaluate("document.documentElement.outerHTML");
                        let html = match v.as_str() {
                            Some(s) if !s.is_empty() => s.to_string(),
                            _ => "<!DOCTYPE html><html><head></head><body></body></html>".to_string(),
                        };
                        (html, page.url_string())
                    };
                    let rendered = crate::screenshot::render_html_to_png_diting(
                        &html,
                        &url,
                        DEFAULT_VIEWPORT_WIDTH,
                        DEFAULT_VIEWPORT_HEIGHT,
                        1.0,
                        true,
                        None,
                        false,
                        None,
                    )
                    .map_err(|e| format!("screenshot failed: {e}"))?;
                    // The legacy path renders to an encoded PNG, so a jpeg
                    // request decodes before re-encoding — handing the PNG
                    // bytes to `RgbaImage::from_raw` as raw pixels is the
                    // "frame buffer size mismatch" crash (any no-clip jpeg
                    // capture died 3/3 on the AginxOS real-device report).
                    let data = if format == "jpeg" || format == "jpg" {
                        encode_legacy_jpeg(&rendered.png, quality_param(&params))?
                    } else {
                        rendered.png
                    };
                    return Ok(json!({ "data": BASE64.encode(&data) }));
                }
                // Band paths. `clip` coordinate semantics follow Chrome: with
                // `captureBeyondViewport:false` the clip is viewport-relative
                // (Playwright pins it to the current view and scrolls the
                // page — treating it as page-absolute paints the top band
                // forever, stale pixels at any scroll); with `true` it is
                // page-absolute (Puppeteer's fullPage/region shape). No clip
                // at all: the scrolled viewport band itself. band_frame
                // clamps the origin into the document exactly like Chrome
                // clamps scroll.
                let (sx, sy, vw, vh) = match params.get("clip") {
                    Some(clip) => {
                        let f = |k: &str| {
                            clip.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32
                        };
                        let (cx, cy, cw, ch) =
                            (f("x"), f("y"), f("width").max(1.0), f("height").max(1.0));
                        if beyond {
                            (cx, cy, cw, ch)
                        } else {
                            let (ox, oy) = page.scroll_offset();
                            (ox + cx, oy + cy, cw, ch)
                        }
                    }
                    None => {
                        let (w, h) = page.effective_viewport();
                        let (ox, oy) = page.scroll_offset();
                        (ox, oy, w, h)
                    }
                };
                let (frame, missing) = page
                    .viewport_band_frame(sx, sy, (vw, vh))
                    .ok_or_else(|| "band paint failed: no live document".to_string())?;
                if !missing.is_empty() {
                    // Fill the image table (page client, SSRF gate, 2 MiB /
                    // 3 s caps — the render-path pre-fetch semantics), then
                    // re-blit with real rasters.
                    let url = page.url_string();
                    fetch_band_images(page, &url, missing).await;
                    let (frame, _) = page
                        .viewport_band_frame(sx, sy, (vw, vh))
                        .ok_or_else(|| "band paint failed: no live document".to_string())?;
                    let data = encode_frame(&format, quality_param(&params), frame.width, frame.height, frame.rgba)?;
                    return Ok(json!({ "data": BASE64.encode(&data) }));
                }
                let data = encode_frame(&format, quality_param(&params), frame.width, frame.height, frame.rgba)?;
                Ok(json!({ "data": BASE64.encode(&data) }))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                Err("captureScreenshot requires the `screenshot` feature".to_string())
            }
        }
        // Accepted but no-op: no dialogs, no downloads in the single-realm
        // engine. Ack so Chrome-shaped clients don't error out.
        "setLifecycleEventsEnabled" => Ok(json!({})),
        "setDownloadBehavior" => Ok(json!({})),
        "setInterceptFileChooserDialog" => Ok(json!({})),
        "handleJavaScriptDialog" => Ok(json!({})),
        "close" => Ok(json!({})),
        "bringToFront" => Ok(json!({})),
        "setWebLifecycleState" => Ok(json!({})),
        "getAppManifest" => Ok(json!({ "errors": [] })),
        "getInstallabilityErrors" => Ok(json!({ "installabilityErrors": [] })),
        // Viewport frame streaming (AginxOS P0): the pump lives on the
        // connection loop's tick; start/stop manage the per-page state.
        "startScreencast" => {
            #[cfg(feature = "screenshot")]
            {
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                let page_id = page.id.clone();
                let p = params.clone();
                let format = p
                    .get("format")
                    .and_then(Value::as_str)
                    .map(|f| f.to_ascii_lowercase())
                    .unwrap_or_else(|| "png".to_string());
                let max_width = p.get("maxWidth").and_then(Value::as_u64).unwrap_or(0) as u32;
                let max_height = p.get("maxHeight").and_then(Value::as_u64).unwrap_or(0) as u32;
                let every_nth_frame = p
                    .get("everyNthFrame")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                ctx.screencast.insert(
                    page_id,
                    ScreencastState {
                        session_id: session_id.clone(),
                        format,
                        quality: quality_param(&p),
                        max_width,
                        max_height,
                        every_nth_frame,
                        frame_seq: 0,
                        outstanding_ack: false,
                        last_damage: None,
                    },
                );
                // Chrome emits the first frame immediately, not on the next
                // client message — the pump only ticks between messages.
                pump_screencast_frames(ctx).await;
                Ok(json!({}))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                Ok(json!({}))
            }
        }
        "stopScreencast" => {
            #[cfg(feature = "screenshot")]
            {
                let page_id = ctx.get_session_page(session_id).map(|p| p.id.clone());
                if let Some(pid) = page_id {
                    ctx.screencast.remove(&pid);
                }
                Ok(json!({}))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                Ok(json!({}))
            }
        }
        "screencastFrameAck" => {
            #[cfg(feature = "screenshot")]
            {
                // The ack carries the frame's numeric sessionId; at most one
                // frame is ever in flight per page, so clearing unblocks the
                // pump unambiguously.
                let page_id = ctx.get_session_page(session_id).map(|p| p.id.clone());
                if let Some(pid) = page_id {
                    if let Some(st) = ctx.screencast.get_mut(&pid) {
                        st.outstanding_ack = false;
                    }
                }
                Ok(json!({}))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                Ok(json!({}))
            }
        }
        _ => Err(format!("Unknown Page method: {}", method)),
    }
}

/// `quality` param: JPEG quality 0-100 (Chrome default 100).
#[cfg(feature = "screenshot")]
fn quality_param(params: &Value) -> u8 {
    params
        .get("quality")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(0, 100) as u8
}

/// Re-encode an already-encoded PNG as jpeg for the legacy full-page path,
/// whose renderer hands back `RenderedScreenshot::png` (compressed bytes, not
/// a raw frame buffer — that distinction is the whole point of this helper).
#[cfg(feature = "screenshot")]
fn encode_legacy_jpeg(png: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png).map_err(|e| format!("jpeg decode: {e}"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| format!("jpeg encode: {e}"))?;
    Ok(out.into_inner())
}

/// Encode a band frame as png (default) or jpeg (`quality`), mirroring the
/// render path's encoder settings.
#[cfg(feature = "screenshot")]
fn encode_frame(
    format: &str,
    quality: u8,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<Vec<u8>, String> {
    if format == "jpeg" || format == "jpg" {
        let img = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| "frame buffer size mismatch".to_string())?;
        let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        rgb.write_with_encoder(encoder)
            .map_err(|e| format!("jpeg encode: {e}"))?;
        Ok(out.into_inner())
    } else {
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(
                std::io::Cursor::new(&mut png_bytes),
                width.max(1),
                height.max(1),
            );
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("png encode header: {e}"))?;
            writer
                .write_image_data(&rgba)
                .map_err(|e| format!("png encode: {e}"))?;
        }
        Ok(png_bytes)
    }
}

/// Downscale a frame into `max_width`/`max_height` (0 = unconstrained),
/// keeping aspect via `imageops::thumbnail` (area-average, like Chrome's
/// screencast downscale).
#[cfg(feature = "screenshot")]
fn maybe_downscale(
    max_width: u32,
    max_height: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> (u32, u32, Vec<u8>) {
    let mut scale = 1.0f32;
    if max_width > 0 {
        scale = scale.min(max_width as f32 / width.max(1) as f32);
    }
    if max_height > 0 {
        scale = scale.min(max_height as f32 / height.max(1) as f32);
    }
    if scale >= 1.0 {
        return (width, height, rgba);
    }
    let Some(img) = image::RgbaImage::from_raw(width, height, rgba) else {
        return (width, height, Vec::new());
    };
    let nw = ((width as f32 * scale).floor() as u32).max(1);
    let nh = ((height as f32 * scale).floor() as u32).max(1);
    let thumb = image::imageops::thumbnail(&img, nw, nh);
    (nw, nh, thumb.into_raw())
}

/// Fetch the img bodies band paint is missing, through the page's own client
/// (stealth when enabled, else the plain client with the document as
/// Referer), and store them for the next band pass. Same per-URL policy as
/// the render-path pre-fetch: SSRF gate, ≤2 MiB per body, 3 s per request,
/// 200-only. Failures just leave the placeholder — a frame beats a stall.
#[cfg(feature = "screenshot")]
async fn fetch_band_images(page: &crate::diting_browser::Page, base_url: &str, urls: Vec<String>) {
    const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
    const PER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    #[cfg(feature = "stealth")]
    let stealth = page.stealth_client.clone();
    let client = page.http_client.clone();
    let doc_referrer = base_url.to_string();
    let futs = urls.into_iter().map(|u| {
        let client = client.clone();
        let doc_referrer = doc_referrer.clone();
        #[cfg(feature = "stealth")]
        let stealth = stealth.clone();
        async move {
            let Ok(parsed) = url::Url::parse(&u) else { return None };
            if crate::diting_js::ops::validate_fetch_url(&parsed).is_err() {
                return None;
            }
            let resp = tokio::time::timeout(PER_REQUEST_TIMEOUT, async {
                #[cfg(feature = "stealth")]
                if let Some(ref s) = stealth {
                    return s.fetch(&parsed).await.ok();
                }
                #[allow(unreachable_code)]
                client
                    .fetch_subresource(&parsed, Some(doc_referrer.as_str()))
                    .await
                    .ok()
            })
            .await
            .ok()
            .flatten()?;
            if resp.status != 200 || resp.body.is_empty() || resp.body.len() > MAX_BODY_BYTES {
                return None;
            }
            Some((u, resp.body))
        }
    });
    let got: Vec<(String, Vec<u8>)> = futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect();
    for (u, body) in got {
        page.store_band_image(u, body);
    }
}

/// Produce one screencast frame per armed, unacked, damaged page. Called on
/// the connection loop's 33 ms tick and once immediately from
/// `startScreencast` (Chrome emits the first frame right away). Damage =
/// (dom epoch, scroll, viewport): a static page costs zero frames, a scroll
/// or mutation re-blits — frame cost is independent of page height.
#[cfg(feature = "screenshot")]
/// Per-tick JS settle budget for armed screencast pages: long enough for a
/// due timer to fire inside the poll, short enough that several armed pages
/// still fit inside the 33 ms pump cadence.
const SCREENCEAST_SETTLE_MS: u64 = 5;

#[cfg(feature = "screenshot")]
pub(crate) async fn pump_screencast_frames(ctx: &mut CdpContext) {
    if ctx.screencast.is_empty() {
        return;
    }
    let entries: Vec<(String, ScreencastState)> = ctx
        .screencast
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (page_id, mut st) in entries {
        // Drive the page's JS event loop a notch so page-driven damage
        // (timers, animations) progresses without a client message: dispatch
        // is otherwise the loop's only poller, and a silent connection
        // freezes a self-updating page — frames flowed only after a
        // heartbeat evaluate on the AginxOS real-device report. Runs before
        // the ack gate on purpose: a slow-acking client must not freeze the
        // page's own time. Idle pages return from settle immediately.
        if let Some(page) = ctx.get_page_mut(&page_id) {
            page.settle(SCREENCEAST_SETTLE_MS).await;
        }
        if st.outstanding_ack {
            continue;
        }
        // Damage signature first (immutable page access — cheap skip before
        // any layout/paint work). A page id with no page behind it (closed
        // while the cast was armed) drops its state here, or the pump would
        // keep waking every tick for a dead id.
        let (epoch, rev, ox, oy) = {
            let Some(page) = ctx.get_page(&page_id) else {
                ctx.screencast.remove(&page_id);
                continue;
            };
            (
                page.dom_epoch(),
                page.layout_rev(),
                page.scroll_offset().0,
                page.scroll_offset().1,
            )
        };
        let (vw, vh) = {
            let Some(page) = ctx.get_page(&page_id) else { continue };
            page.effective_viewport()
        };
        let sig = (epoch, rev, ox, oy, vw, vh);
        if st.last_damage == Some(sig) {
            continue;
        }
        // everyNthFrame: count *changed* frames; the first changed frame
        // always emits, then every Nth after that.
        st.frame_seq += 1;
        let nth_hit = (st.frame_seq - 1) % st.every_nth_frame.max(1) as u64 == 0;
        st.last_damage = Some(sig);
        if !nth_hit {
            ctx.screencast.insert(page_id, st);
            continue;
        }
        let produced = {
            let Some(page) = ctx.get_page(&page_id) else { continue };
            match page.viewport_band_frame(ox, oy, (vw, vh)) {
                None => None,
                Some((frame, missing)) => {
                    if !missing.is_empty() {
                        let url = page.url_string();
                        fetch_band_images(page, &url, missing).await;
                        page.viewport_band_frame(ox, oy, (vw, vh)).map(|(f, _)| f)
                    } else {
                        Some(frame)
                    }
                }
            }
        };
        let Some(frame) = produced else {
            // No live document yet (pre-navigation): persist the signature
            // so the pump doesn't retry a doomed produce every tick — the
            // next epoch/scroll change re-arms it.
            ctx.screencast.insert(page_id, st);
            continue;
        };
        let (w, h, rgba) = maybe_downscale(
            st.max_width,
            st.max_height,
            frame.width,
            frame.height,
            frame.rgba,
        );
        let data = match encode_frame(&st.format, st.quality, w, h, rgba) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!("screencast frame encode failed: {e}");
                continue;
            }
        };
        st.outstanding_ack = true;
        let params = json!({
            "data": BASE64.encode(&data),
            "metadata": {
                "offsetTop": 0,
                "pageScaleFactor": 1,
                "deviceWidth": vw as i64,
                "deviceHeight": vh as i64,
                "scrollOffsetX": frame.dx as i64,
                "scrollOffsetY": frame.dy as i64,
                "timestamp": now_epoch_seconds(),
            },
            "sessionId": st.frame_seq as i64,
        });
        emit(ctx, "Page.screencastFrame", params, &st.session_id);
        ctx.screencast.insert(page_id, st);
    }
}
