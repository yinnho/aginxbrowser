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
        if url_is_file_scheme(url) && !page.context.allow_file_access {
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
            let w = DEFAULT_VIEWPORT_WIDTH;
            let h = DEFAULT_VIEWPORT_HEIGHT;
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
        "captureScreenshot" => {
            #[cfg(feature = "screenshot")]
            {
                let (html, url) = {
                    let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
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
                Ok(json!({ "data": BASE64.encode(&rendered.png) }))
            }
            #[cfg(not(feature = "screenshot"))]
            {
                Err("captureScreenshot requires the `screenshot` feature".to_string())
            }
        }
        // Accepted but no-op: no dialogs, no downloads, no screencasts in the
        // single-realm engine. Ack so Chrome-shaped clients don't error out.
        "setLifecycleEventsEnabled" => Ok(json!({})),
        "setDownloadBehavior" => Ok(json!({})),
        "setInterceptFileChooserDialog" => Ok(json!({})),
        "handleJavaScriptDialog" => Ok(json!({})),
        "close" => Ok(json!({})),
        "bringToFront" => Ok(json!({})),
        "startScreencast" => Ok(json!({})),
        "stopScreencast" => Ok(json!({})),
        "setWebLifecycleState" => Ok(json!({})),
        "getAppManifest" => Ok(json!({ "errors": [] })),
        "getInstallabilityErrors" => Ok(json!({ "installabilityErrors": [] })),
        _ => Err(format!("Unknown Page method: {}", method)),
    }
}
