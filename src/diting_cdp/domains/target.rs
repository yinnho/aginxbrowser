use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;
use crate::diting_cdp::types::CdpEvent;
use crate::diting_cdp::util::url_is_file_scheme;

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    parent_session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "setDiscoverTargets" => {
            ctx.pending_events.push(CdpEvent::new(
                "Target.targetCreated",
                json!({
                    "targetInfo": {
                        "targetId": "browser",
                        "type": "browser",
                        "title": "",
                        "url": "",
                        "attached": true,
                        "canAccessOpener": false,
                        "browserContextId": "",
                    }
                }),
            ));
            for page in &ctx.pages {
                ctx.pending_events.push(CdpEvent::new(
                    "Target.targetCreated",
                    json!({
                        "targetInfo": {
                            "targetId": page.id,
                            "type": "page",
                            "title": page.title,
                            "url": page.url_string(),
                            "attached": false,
                            "canAccessOpener": false,
                            "browserContextId": page.context.id,
                        }
                    }),
                ));
            }
            Ok(json!({}))
        }
        "getTargets" => {
            let targets: Vec<Value> = ctx
                .pages
                .iter()
                .map(|page| {
                    json!({
                        "targetId": page.id,
                        "type": "page",
                        "title": page.title,
                        "url": page.url_string(),
                        "attached": true,
                        "canAccessOpener": false,
                        "browserContextId": page.context.id,
                    })
                })
                .collect();
            Ok(json!({ "targetInfos": targets }))
        }
        "createTarget" => {
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            let context_id = params.get("browserContextId").and_then(|v| v.as_str());

            // Same gate as Page.navigate. Without this, a CDP client can call
            // Target.createTarget {url:"file:///etc/passwd"} and then
            // Runtime.evaluate the body off the created target, bypassing the
            // page-domain check entirely.
            let allow_file_access = context_id
                .and_then(|id| ctx.browser_context(id))
                .unwrap_or(&ctx.default_context)
                .allow_file_access;
            if url_is_file_scheme(url) && !allow_file_access {
                return Err(
                    "Target.createTarget to file:// is disabled. Restart with `--allow-file-access` to enable."
                        .to_string(),
                );
            }

            let page_id = ctx.create_page_in_context(context_id)?;
            let session_id = format!("{}-session", page_id);

            let mut navigated = false;
            if let Some(page) = ctx.get_page_mut(&page_id) {
                if url == "about:blank" || url.is_empty() {
                    page.navigate_blank();
                } else {
                    let _ = page.navigate(url).await;
                    navigated = true;
                }
            }

            ctx.sessions.insert(session_id.clone(), page_id.clone());

            if let Some((title, page_url, browser_context_id)) =
                ctx.get_page(&page_id).map(|page| {
                    (
                        page.title.clone(),
                        page.url_string(),
                        page.context.id.clone(),
                    )
                })
            {
                ctx.pending_events.push(CdpEvent::new(
                    "Target.targetCreated",
                    json!({
                        "targetInfo": {
                            "targetId": page_id,
                            "type": "page",
                            "title": title,
                            "url": page_url,
                            "attached": false,
                            "canAccessOpener": false,
                            "browserContextId": browser_context_id,
                        }
                    }),
                ));

                ctx.pending_events.push(CdpEvent::new(
                    "Target.attachedToTarget",
                    json!({
                        "sessionId": session_id,
                        "targetInfo": {
                            "targetId": page_id,
                            "type": "page",
                            "title": title,
                            "url": page_url,
                            "attached": true,
                            "canAccessOpener": false,
                            "browserContextId": browser_context_id,
                        },
                        "waitingForDebugger": false,
                    }),
                ));
            }

            // A page created with a URL navigated before the client ever saw
            // it: emit the load-event sequence AFTER attachedToTarget (so the
            // client can resolve the sessionId the events carry) instead of
            // never — Page-domain waiters otherwise hang on the initial load
            // (obscura#833 shape).
            if navigated {
                super::page::emit_navigation_for_page(
                    ctx,
                    &Some(session_id),
                    &page_id,
                );
            }

            Ok(json!({ "targetId": page_id }))
        }
        "attachToBrowserTarget" => {
            // Playwright calls this on connect to obtain a session for the
            // implicit "browser" target. Returning Unknown method aborts the
            // connect handshake before any user code runs.
            let session_id = "browser-session".to_string();
            ctx.sessions
                .insert(session_id.clone(), "browser".to_string());

            ctx.pending_events.push(CdpEvent::new(
                "Target.attachedToTarget",
                json!({
                    "sessionId": session_id,
                    "targetInfo": {
                        "targetId": "browser",
                        "type": "browser",
                        "title": "",
                        "url": "",
                        "attached": true,
                        "canAccessOpener": false,
                        "browserContextId": "",
                    },
                    "waitingForDebugger": false,
                }),
            ));

            Ok(json!({ "sessionId": session_id }))
        }
        "attachToTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(|v| v.as_str())
                .ok_or("targetId required")?;
            if ctx.get_page(target_id).is_none() {
                return Err("Target not found".to_string());
            }
            let session_id = ctx.next_target_session(target_id);
            ctx.sessions
                .insert(session_id.clone(), target_id.to_string());

            if let Some(page) = ctx.get_page(target_id) {
                let params = json!({
                    "sessionId": session_id,
                    "targetInfo": {
                        "targetId": target_id,
                        "type": "page",
                        "title": page.title,
                        "url": page.url_string(),
                        "attached": true,
                        "canAccessOpener": false,
                        "browserContextId": page.context.id,
                    },
                    "waitingForDebugger": false,
                });
                let event = match parent_session_id {
                    Some(parent) => CdpEvent::with_session(
                        "Target.attachedToTarget",
                        params,
                        parent.clone(),
                    ),
                    None => CdpEvent::new("Target.attachedToTarget", params),
                };
                ctx.pending_events.push(event);
            }

            Ok(json!({ "sessionId": session_id }))
        }
        "closeTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(|v| v.as_str())
                .ok_or("targetId required")?;
            let session_id = format!("{}-session", target_id);

            ctx.pending_events.push(CdpEvent::new(
                "Target.detachedFromTarget",
                json!({
                    "sessionId": session_id,
                    "targetId": target_id,
                }),
            ));
            ctx.pending_events.push(CdpEvent::new(
                "Target.targetDestroyed",
                json!({ "targetId": target_id }),
            ));

            ctx.remove_page(target_id);
            Ok(json!({ "success": true }))
        }
        "setAutoAttach" => Ok(json!({})),
        // No multi-target lifecycle to manage: one page per connection thread.
        // Ack these so Chrome-shaped clients that call them do not warn.
        "detachFromTarget" => {
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                ctx.sessions.remove(session_id);
            }
            Ok(json!({}))
        }
        "activateTarget" => Ok(json!({})),
        "getBrowserContexts" => {
            let mut ids: Vec<&String> = ctx.browser_contexts.keys().collect();
            ids.sort();
            Ok(json!({ "browserContextIds": ids }))
        }
        "createBrowserContext" => {
            let id = ctx.create_browser_context();
            Ok(json!({ "browserContextId": id }))
        }
        "disposeBrowserContext" => {
            let context_id = params
                .get("browserContextId")
                .and_then(|v| v.as_str())
                .ok_or("browserContextId required")?;
            let sessions: Vec<(String, String)> = ctx
                .sessions
                .iter()
                .filter_map(|(session_id, page_id)| {
                    ctx.get_page(page_id)
                        .filter(|page| page.context.id == context_id)
                        .map(|_| (session_id.clone(), page_id.clone()))
                })
                .collect();
            let page_ids = ctx.dispose_browser_context(context_id)?;
            for (session_id, page_id) in sessions {
                ctx.pending_events.push(CdpEvent::new(
                    "Target.detachedFromTarget",
                    json!({ "sessionId": session_id, "targetId": page_id }),
                ));
            }
            for page_id in page_ids {
                ctx.pending_events.push(CdpEvent::new(
                    "Target.targetDestroyed",
                    json!({ "targetId": page_id }),
                ));
            }
            Ok(json!({}))
        }
        "getTargetInfo" => {
            let target_id = params.get("targetId").and_then(|v| v.as_str());
            match target_id {
                Some(id) => {
                    let page = ctx.get_page(id).ok_or("Target not found")?;
                    Ok(json!({
                        "targetInfo": {
                            "targetId": id,
                            "type": "page",
                            "title": page.title,
                            "url": page.url_string(),
                            "attached": true,
                            "canAccessOpener": false,
                            "browserContextId": page.context.id,
                        }
                    }))
                }
                None => {
                    // canAccessOpener is required on every TargetInfo per the
                    // CDP spec; strict clients (chromiumoxide) panic if it's
                    // missing.
                    Ok(json!({
                        "targetInfo": {
                            "targetId": "browser",
                            "type": "browser",
                            "title": "",
                            "url": "",
                            "attached": true,
                            "canAccessOpener": false,
                        }
                    }))
                }
            }
        }
        _ => Err(format!("Unknown Target method: {}", method)),
    }
}
