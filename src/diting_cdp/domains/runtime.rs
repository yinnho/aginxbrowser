use crate::diting_browser::lifecycle::LifecycleState;
use crate::diting_js::runtime::{EvalOutcome, ExceptionInfo, RemoteObjectInfo};
use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;
use crate::diting_cdp::types::CdpEvent;

/// Whether a binding name is a plain JS identifier and therefore safe to
/// interpolate into the generated shim / teardown scripts. Chromium bindings
/// are identifiers; anything else could break out of the surrounding string
/// literal and inject arbitrary JS into the page. Both addBinding and
/// removeBinding share this guard.
fn is_valid_binding_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !name.chars().next().unwrap_or('0').is_ascii_digit()
}

/// Drain pending JS-initiated navigation (form.submit, location.assign, etc),
/// then emit the same CDP nav-event sequence Page.navigate emits so
/// Puppeteer's waitForNavigation / Playwright's wait_for_url resolves.
async fn emit_post_eval_nav(
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<(), String> {
    let did_navigate = {
        let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
        page.process_pending_navigation().await.map_err(|e| e.to_string())?
    };
    if !did_navigate {
        return Ok(());
    }
    let (frame_id, page_url, page_id, network_events, reached_idle) = {
        let p = ctx.get_session_page_mut(session_id).ok_or("No page")?;
        (
            p.frame_id.clone(),
            p.url_string(),
            p.id.clone(),
            p.network_events.drain(..).collect::<Vec<_>>(),
            p.lifecycle == LifecycleState::NetworkIdle,
        )
    };
    let loader_id = format!("loader-{}", uuid::Uuid::new_v4());
    super::page::emit_navigation_events(
        ctx,
        session_id,
        &frame_id,
        &loader_id,
        &page_url,
        &page_id,
        &network_events,
        crate::diting_browser::lifecycle::WaitUntil::Load,
        reached_idle,
    );
    Ok(())
}

fn exception_details_json(exc: &ExceptionInfo) -> Value {
    let mut exception = json!({
        "type": "object",
        "subtype": "error",
        "className": exc.class_name,
        "description": exc.description,
    });
    if let Some(oid) = &exc.object_id {
        exception["objectId"] = json!(oid);
    }
    json!({
        "exceptionId": 1,
        "text": exc.text,
        "lineNumber": 0,
        "columnNumber": 0,
        "exception": exception,
        "executionContextId": 1,
    })
}

/// Shape a Runtime.evaluate / callFunctionOn response from an `EvalOutcome`.
/// A thrown expression returns `result` as the error object AND carries
/// `exceptionDetails`, and additionally emits `Runtime.exceptionThrown` so
/// `page.on('pageerror')` listeners fire — matching real Chrome.
fn evaluate_response(
    outcome: EvalOutcome,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Value {
    let result = remote_object_from_info(&outcome.info);
    match outcome.exception {
        None => json!({ "result": result }),
        Some(exc) => {
            let details = exception_details_json(&exc);
            let ts_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            ctx.pending_events.push(CdpEvent {
                method: "Runtime.exceptionThrown".into(),
                params: json!({ "timestamp": ts_ms, "exceptionDetails": details.clone() }),
                session_id: session_id.clone(),
            });
            json!({ "result": result, "exceptionDetails": details })
        }
    }
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" => {
            // Puppeteer's FrameManager.initialize calls Runtime.enable on the
            // browser-level connection BEFORE any page target exists. Real
            // Chrome replies with `{}` and emits executionContextCreated when
            // a context appears. If there's no session, succeed silently.
            if let Some(page) = ctx.get_session_page(session_id) {
                let event = crate::diting_cdp::types::CdpEvent {
                    method: "Runtime.executionContextCreated".to_string(),
                    params: json!({
                        "context": {
                            "id": 1,
                            "origin": page.url_string(),
                            "name": "",
                            "uniqueId": format!("ctx-{}", page.id),
                            "auxData": {
                                "isDefault": true,
                                "type": "default",
                                "frameId": page.frame_id,
                            }
                        }
                    }),
                    session_id: session_id.clone(),
                };
                ctx.pending_events.push(event);
            }
            Ok(json!({}))
        }
        "disable" => Ok(json!({})),
        "evaluate" => {
            let expression = params
                .get("expression")
                .and_then(|v| v.as_str())
                .ok_or("expression required")?;
            let return_by_value = params
                .get("returnByValue")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            validate_context_id(params, "contextId", ctx)?;
            let await_promise = params
                .get("awaitPromise")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // CDP `timeout` field (ms); default Chrome's protocolTimeout so a
            // long evaluation doesn't starve the session forever. The engine's
            // evaluate path is additionally bounded by the V8 watchdog.
            let timeout_ms = params
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);

            let outcome = {
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.evaluate_for_cdp_outcome(expression, return_by_value, await_promise),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        return Err(format!("Runtime.evaluate exceeded {timeout_ms}ms timeout"));
                    }
                }
            };
            emit_post_eval_nav(ctx, session_id).await?;

            Ok(evaluate_response(outcome, ctx, session_id))
        }
        "callFunctionOn" => {
            let function_declaration = params
                .get("functionDeclaration")
                .and_then(|v| v.as_str())
                .unwrap_or("() => undefined");
            let return_by_value = params
                .get("returnByValue")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let await_promise = params
                .get("awaitPromise")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let object_id = params.get("objectId").and_then(|v| v.as_str());
            let arguments = params
                .get("arguments")
                .and_then(|v| v.as_array())
                .map(|a| a.to_vec())
                .unwrap_or_default();

            validate_context_id(params, "executionContextId", ctx)?;

            let timeout_ms = params
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);

            let outcome = {
                let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.call_function_on_for_cdp_outcome(
                        function_declaration,
                        object_id,
                        &arguments,
                        return_by_value,
                        await_promise,
                    ),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        return Err(format!(
                            "Runtime.callFunctionOn exceeded {timeout_ms}ms timeout"
                        ));
                    }
                }
            };
            emit_post_eval_nav(ctx, session_id).await?;

            Ok(evaluate_response(outcome, ctx, session_id))
        }
        "getProperties" => {
            // Puppeteer's $$() flow: evaluate querySelectorAll → handle for the
            // NodeList → getProperties on that handle → indexed items, each
            // annotated subtype:'node' so JSHandle.asElement() wraps it as an
            // ElementHandle. Walk the object in JS, allocating a stable child
            // oid per (parent_oid + index).
            let object_id = params.get("objectId").and_then(|v| v.as_str());
            if let Some(oid) = object_id {
                // serde_json::to_string emits a double-quoted literal that
                // escapes backslashes, quotes AND control characters — the
                // old manual single-quote splice broke on newlines in an id
                // (upstream obscura #709).
                let escaped_oid =
                    serde_json::to_string(oid).unwrap_or_else(|_| "\"\"".to_string());
                let code = format!(
                    "(function() {{\
                        var obj = globalThis.__diting_objects[{oid_str}];\
                        if (!obj || typeof obj !== 'object') return [];\
                        var keys = Object.keys(obj);\
                        return keys.map(function(k) {{\
                            var v = obj[k];\
                            var t = typeof v;\
                            var item = {{ name: k, type: t }};\
                            if (v === null) {{ item.value = null; return item; }}\
                            if (t !== 'object' && t !== 'function') {{ item.value = v; return item; }}\
                            var childOid = {obj_key} + '::' + k;\
                            globalThis.__diting_objects[childOid] = v;\
                            item.childOid = childOid;\
                            if (typeof v.nodeType === 'number') {{\
                                item.subtype = 'node';\
                                item.className = v.constructor && v.constructor.name ? v.constructor.name : (v.tagName ? 'HTML' + v.tagName.charAt(0) + v.tagName.slice(1).toLowerCase() + 'Element' : 'Node');\
                                item.description = v.tagName ? v.tagName.toLowerCase() : (v.nodeName || 'node');\
                            }} else if (Array.isArray(v)) {{\
                                item.subtype = 'array';\
                                item.className = 'Array';\
                                item.description = 'Array(' + v.length + ')';\
                            }} else {{\
                                item.className = (v.constructor && v.constructor.name) || 'Object';\
                                item.description = item.className;\
                            }}\
                            return item;\
                        }});\
                    }})()",
                    oid_str = escaped_oid,
                    obj_key = escaped_oid,
                );
                let result = {
                    let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
                    page.evaluate(&code)
                };
                if let Value::Array(props) = result {
                    let descriptors: Vec<Value> = props
                        .iter()
                        .map(|p| {
                            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let prop_type =
                                p.get("type").and_then(|v| v.as_str()).unwrap_or("undefined");
                            let mut remote = json!({ "type": prop_type });
                            if let Some(child_oid) = p.get("childOid").and_then(|v| v.as_str()) {
                                remote["type"] = json!("object");
                                if let Some(sub) = p.get("subtype").and_then(|v| v.as_str()) {
                                    remote["subtype"] = json!(sub);
                                }
                                if let Some(cls) = p.get("className").and_then(|v| v.as_str()) {
                                    remote["className"] = json!(cls);
                                }
                                if let Some(desc) = p.get("description").and_then(|v| v.as_str()) {
                                    remote["description"] = json!(desc);
                                }
                                remote["objectId"] = json!(child_oid);
                            } else if let Some(val) = p.get("value") {
                                match val {
                                    Value::Null => {
                                        remote["type"] = json!("object");
                                        remote["subtype"] = json!("null");
                                        remote["value"] = json!(null);
                                    }
                                    Value::String(s) => {
                                        remote["type"] = json!("string");
                                        remote["value"] = json!(s);
                                    }
                                    Value::Number(n) => {
                                        remote["type"] = json!("number");
                                        remote["value"] = json!(n);
                                    }
                                    Value::Bool(b) => {
                                        remote["type"] = json!("boolean");
                                        remote["value"] = json!(b);
                                    }
                                    _ => {
                                        remote["value"] = val.clone();
                                    }
                                }
                            }
                            json!({
                                "name": name,
                                "value": remote,
                                "configurable": true,
                                "enumerable": true,
                                "writable": true,
                                "isOwn": true,
                            })
                        })
                        .collect();
                    Ok(json!({ "result": descriptors, "internalProperties": [] }))
                } else {
                    Ok(json!({ "result": [], "internalProperties": [] }))
                }
            } else {
                Ok(json!({ "result": [], "internalProperties": [] }))
            }
        }
        "releaseObject" => {
            if let Some(oid) = params.get("objectId").and_then(|v| v.as_str()) {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.release_object(oid);
                }
            }
            Ok(json!({}))
        }
        "releaseObjectGroup" => {
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.release_object_group();
            }
            Ok(json!({}))
        }
        "addBinding" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if is_valid_binding_name(name) {
                // The shim forwards every call back to Rust through
                // op_binding_called; the dispatcher drains the queue and emits
                // Runtime.bindingCalled events. Chromium's V8Inspector rejects
                // calls without exactly one argument and ToString-coerces that
                // argument into the payload — match both behaviors.
                let shim = format!(
                    "globalThis['{name}'] = function (arg) {{\
                        if (arguments.length !== 1) return;\
                        try {{\
                            const payload = typeof arg === 'string' ? arg : String(arg);\
                            Deno.core.ops.op_binding_called('{name}', payload);\
                        }} catch (e) {{ /* swallow: binding must not throw into page */ }}\
                    }};",
                    name = name,
                );
                // Re-install on every navigation: globalThis is wiped on each
                // new document, and puppeteer registers bindings once-per-page
                // rather than once-per-document.
                let key = format!("__diting_binding__{}", name);
                ctx.preload_scripts.retain(|(k, _)| k != &key);
                ctx.preload_scripts.push((key, shim.clone()));
                // Remember who subscribed, so the call goes back to this
                // session rather than to whichever session of the page a
                // HashMap happens to yield first.
                if let Some(session_id) = session_id {
                    let owners = ctx.binding_sessions.entry(name.to_string()).or_default();
                    if !owners.contains(session_id) {
                        owners.push(session_id.clone());
                    }
                }
                // Install on the current page so the binding is usable
                // immediately, without waiting for the next navigation.
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(&shim);
                }
            }
            Ok(json!({}))
        }
        "removeBinding" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if is_valid_binding_name(name) {
                let key = format!("__diting_binding__{}", name);
                ctx.preload_scripts.retain(|(k, _)| k != &key);
                if let Some(session_id) = session_id {
                    if let Some(owners) = ctx.binding_sessions.get_mut(name) {
                        owners.retain(|owner| owner != session_id);
                        if owners.is_empty() {
                            ctx.binding_sessions.remove(name);
                        }
                    }
                }
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(&format!("delete globalThis['{}'];", name));
                }
            }
            Ok(json!({}))
        }
        "runIfWaitingForDebugger" => Ok(json!({})),
        "getExceptionDetails" => Ok(json!({ "exceptionDetails": null })),
        "discardConsoleEntries" => Ok(json!({})),
        _ => Err(format!("Unknown Runtime method: {}", method)),
    }
}

/// Reject `Runtime.{evaluate,callFunctionOn}` calls that target an execution
/// context we have not advertised. Returns `Ok(())` when the parameter is
/// absent (defaulting to the page's default context) or when the id matches
/// one of `ctx.valid_context_ids`.
fn validate_context_id(params: &Value, field: &str, ctx: &CdpContext) -> Result<(), String> {
    let Some(id) = params.get(field).and_then(|v| v.as_i64()) else {
        return Ok(());
    };
    if !ctx.valid_context_ids.contains(&id) {
        return Err(format!("Cannot find context with specified id: {}", id));
    }
    Ok(())
}

pub(crate) fn remote_object_from_info(info: &RemoteObjectInfo) -> Value {
    let mut obj = json!({ "type": info.js_type });

    if let Some(ref subtype) = info.subtype {
        obj["subtype"] = json!(subtype);
    }

    if !info.class_name.is_empty() {
        obj["className"] = json!(info.class_name);
    }

    if !info.description.is_empty() {
        obj["description"] = json!(info.description);
    }

    if let Some(ref oid) = info.object_id {
        obj["objectId"] = json!(oid);
    }

    if let Some(ref value) = info.value {
        obj["value"] = value.clone();
    }

    obj
}
