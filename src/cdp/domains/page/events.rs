//! Post-navigation CDP event emission (god-file ratchet split from page.rs):
//! the Network requestWillBeSent/responseReceived/loadingFinished triple, the
//! commit-phase navigation prefix, and the frame + execution-context
//! announcements navigation-waiters key off.

use serde_json::json;

use diting::diting_browser::page::NetworkEvent;
use crate::cdp::dispatch::CdpContext;

use super::{emit, frame_json, now_epoch_seconds};

/// Emit the full post-navigation CDP event sequence navigation-waiters key off:
/// frameStartedLoading, frameNavigated, per-request Network events,
/// domContentEventFired, loadEventFired, lifecycle events, and fresh execution
/// contexts (default + any isolated worlds).
///
/// Shared by `Page.navigate` and the post-eval drain in the Runtime domain
/// (`emit_post_eval_nav`), so a `location.href = ...` in an evaluated script
/// produces the same sequence a direct navigation does.
/// Emit the requestWillBeSent / responseReceived / loadingFinished triple
/// for one recorded network event. Shared by the post-navigation batch and
/// by the outgoing document's carried events, which must ride under the
/// loader they actually belonged to.
///
/// `extra` carries the page's other Network-enabled sessions (Chrome
/// delivers a target's events to every session that enabled the domain,
/// #13); the navigating session always receives the trio regardless, so
/// clients that never call Network.enable keep the old behavior.
pub(super) fn emit_network_event(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    extra: &[String],
    frame_id: &str,
    loader_id: &str,
    document_url: &str,
    ev: &NetworkEvent,
) {
    let ts = now_epoch_seconds();
    let request_id = ev.request_id.clone();
    let will_be_sent = json!({
        "requestId": request_id,
        "loaderId": loader_id,
        "documentURL": document_url,
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
    });
    let response_received = json!({
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
    });
    let loading_finished = json!({
        "requestId": request_id,
        "timestamp": ev.timestamp,
        "encodedDataLength": ev.body_size,
    });
    let mut targets: Vec<Option<String>> = vec![session_id.clone()];
    targets.extend(extra.iter().map(|sid| Some(sid.clone())));
    for target in targets {
        emit(
            ctx,
            "Network.requestWillBeSent",
            will_be_sent.clone(),
            &target,
        );
        emit(
            ctx,
            "Network.responseReceived",
            response_received.clone(),
            &target,
        );
        emit(
            ctx,
            "Network.loadingFinished",
            loading_finished.clone(),
            &target,
        );
    }
}

/// Commit-phase announcement for a CDP navigation: the outgoing document's
/// carried network events stream under the loader they belonged to, the
/// fresh loaderId is minted, and the frame + execution contexts are
/// announced for the target URL. Chrome resolves `Page.navigate` here —
/// before the document has loaded — which is what lets a spawned
/// navigation hand the socket back to the connection loop while its
/// fetches run.
///
/// `target_url` is the URL being navigated to (the frame announces it at
/// commit; a redirect chain that lands elsewhere is re-announced by the
/// tail). Returns `(frame_id, loader_id, old_loader)`.
pub(crate) fn emit_navigation_prefix(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    extra_network: &[String],
    page_id: &str,
    target_url: &str,
) -> (String, String, String) {
    let (frame_id, carried, carried_url) = {
        let Some(page) = ctx.get_page_mut(page_id) else {
            return (String::new(), String::new(), String::new());
        };
        (
            page.frame_id.clone(),
            std::mem::take(&mut page.carried_network_events),
            page.carried_network_url.clone(),
        )
    };
    // The outgoing document's network events — including its script-initiated
    // fetch/XHR, which only ever sat in the JS runtime's queue — were carried
    // across this navigation by the page layer. Emit them first, under the
    // loader they belonged to (still the current one here), so a client sees
    // them before the new document's frameNavigated (obscura #920 shape).
    let old_loader = ctx
        .current_loader_ids
        .get(page_id)
        .cloned()
        .unwrap_or_default();
    for ev in &carried {
        emit_network_event(
            ctx,
            session_id,
            extra_network,
            &frame_id,
            &old_loader,
            &carried_url,
            ev,
        );
    }
    let loader_id = format!("loader-{}", uuid::Uuid::new_v4());
    ctx.current_loader_ids
        .insert(page_id.to_string(), loader_id.clone());
    // Child frames belong to the outgoing document: detach all of them
    // before the new document's frame/context announcements.
    for frame in ctx.clear_child_frames(page_id) {
        emit(
            ctx,
            "Page.frameDetached",
            json!({ "frameId": frame.frame_id, "reason": "remove" }),
            session_id,
        );
    }
    emit_context_events(ctx, session_id, &frame_id, &loader_id, target_url, page_id);
    (frame_id, loader_id, old_loader)
}

/// Frame + execution-context announcement for a navigation. Chrome tears
/// down and re-creates the execution context at commit, so these stream
/// with the prefix — before the new document's resources — rather than
/// after its lifecycle events.
fn emit_context_events(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
    frame_id: &str,
    loader_id: &str,
    page_url: &str,
    page_id: &str,
) {
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

