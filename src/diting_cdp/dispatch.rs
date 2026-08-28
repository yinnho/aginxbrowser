//! CDP command routing + per-connection state (claimed from obscura-cdp
//! dispatch.rs, adapted to the diting engine).
//!
//! Adaptation notes vs upstream:
//! - no screencasts (upstream render feature; diting screenshots go through
//!   `crate::screenshot::render_html_to_png_diting` instead)
//! - no Fetch-intercept state yet (Fetch domain not claimed in wave 1)
//! - single realm per page, so child-frame bookkeeping is a stub

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::diting_browser::{BrowserContext, Page};
use serde_json::json;

use crate::diting_cdp::domains;
use crate::diting_cdp::types::{CdpEvent, CdpRequest, CdpResponse};

pub struct CdpContext {
    pub pages: Vec<Page>,
    pub sessions: HashMap<String, String>, // session_id -> page_id
    /// Current document loader per page. Navigation events and later
    /// script-initiated Network events must share this id; inventing a loader
    /// for each fetch breaks DevTools request grouping.
    pub current_loader_ids: HashMap<String, String>,
    /// Child frame ids already reported to the client, per page (single-realm
    /// engine: currently always empty, kept for the drain structure).
    pub announced_frames: HashMap<String, Vec<String>>,
    pub pending_events: Vec<CdpEvent>,
    pub default_context: Arc<BrowserContext>,
    pub browser_contexts: HashMap<String, Arc<BrowserContext>>,
    page_counter: u32,
    browser_context_counter: u32,
    target_session_counter: u64,
    pub preload_scripts: Vec<(String, String)>, // (identifier, source)
    pub preload_counter: u32,
    // Which sessions asked for each `Runtime.addBinding` name. A binding is a
    // session-scoped subscription in CDP, and a client discards any event whose
    // sessionId is not one it holds, so the call has to go back to the session
    // that registered the name rather than to whichever session of the page
    // happens to come first out of a HashMap.
    pub binding_sessions: HashMap<String, Vec<String>>, // binding name -> session ids
    // World names registered via Page.createIsolatedWorld. After every
    // navigation execution contexts are cleared and must be re-emitted,
    // otherwise Playwright/Puppeteer hang waiting for their utility world to
    // come back.
    pub isolated_worlds: Vec<String>,
    // Set of executionContextIds emitted via Runtime.executionContextCreated.
    // Pre-populated with the default-frame contexts (`1`, `2`); extended by
    // Page.createIsolatedWorld. Runtime.evaluate / callFunctionOn consult this
    // set to reject requests targeting an unknown context — matching real
    // Chrome's "Cannot find context with specified id" CDP error.
    pub valid_context_ids: HashSet<i64>,
    // Monotonic counter for isolated-world execution context ids — incrementing,
    // never reused, mirroring what real Chrome would emit.
    pub next_isolated_context_id: i64,
}

impl CdpContext {
    /// Build a CDP context around an already-constructed default browser
    /// context. The server passes an isolated context per WebSocket connection;
    /// tests may construct their own.
    pub fn new_with_shared_context(default_context: Arc<BrowserContext>) -> Self {
        let mut valid_context_ids = HashSet::new();
        valid_context_ids.insert(1);
        valid_context_ids.insert(2);
        CdpContext {
            pages: Vec::new(),
            sessions: HashMap::new(),
            current_loader_ids: HashMap::new(),
            announced_frames: HashMap::new(),
            pending_events: Vec::new(),
            default_context,
            browser_contexts: HashMap::new(),
            page_counter: 0,
            browser_context_counter: 0,
            target_session_counter: 0,
            preload_scripts: Vec::new(),
            binding_sessions: HashMap::new(),
            preload_counter: 0,
            isolated_worlds: Vec::new(),
            valid_context_ids,
            next_isolated_context_id: 100,
        }
    }

    /// A fresh context with an isolated BrowserContext (own cookie jar +
    /// HTTP client), as the server hands every WebSocket connection.
    pub fn new_with_options(proxy: Option<String>, stealth: bool) -> Self {
        Self::new_with_full_options(proxy, stealth, None)
    }

    pub fn new_with_full_options(
        proxy: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
    ) -> Self {
        let ctx = BrowserContext::with_storage_and_network(
            "default".to_string(),
            proxy,
            stealth,
            user_agent,
            None,
            false,
            None,
        );
        Self::new_with_shared_context(Arc::new(ctx))
    }

    /// Claim the next isolated-world execution context id and register it as
    /// valid for `Runtime.evaluate`/`callFunctionOn`.
    pub fn next_isolated_context(&mut self) -> i64 {
        let id = self.next_isolated_context_id;
        self.next_isolated_context_id += 1;
        self.valid_context_ids.insert(id);
        id
    }

    pub fn create_page_in_context(&mut self, context_id: Option<&str>) -> Result<String, String> {
        let context = match context_id {
            Some(id) => self
                .browser_context(id)
                .cloned()
                .ok_or_else(|| format!("Browser context not found: {}", id))?,
            None => self.default_context.clone(),
        };
        self.page_counter += 1;
        let page_id = format!("page-{}", self.page_counter);
        let mut page = Page::new(page_id.clone(), context);
        page.navigate_blank();
        self.pages.push(page);
        self.current_loader_ids
            .insert(page_id.clone(), format!("loader-blank-{page_id}"));
        Ok(page_id)
    }

    pub fn browser_context(&self, id: &str) -> Option<&Arc<BrowserContext>> {
        if id == self.default_context.id {
            Some(&self.default_context)
        } else {
            self.browser_contexts.get(id)
        }
    }

    pub fn create_browser_context(&mut self) -> String {
        self.browser_context_counter += 1;
        let id = format!("context-{}", self.browser_context_counter);
        let context = Arc::new(self.default_context.isolated_copy(id.clone(), false));
        self.browser_contexts.insert(id.clone(), context);
        id
    }

    /// Allocate a distinct CDP session for every explicit target attachment.
    /// A target may have more than one client session at a time (for example,
    /// Playwright's managed page session plus `newCDPSession(page)`). Reusing
    /// the page's auto-attach session id makes the client's session registry
    /// overwrite the original route.
    pub(crate) fn next_target_session(&mut self, target_id: &str) -> String {
        self.target_session_counter = self.target_session_counter.saturating_add(1);
        format!("{target_id}-session-{}", self.target_session_counter)
    }

    pub fn dispose_browser_context(&mut self, id: &str) -> Result<Vec<String>, String> {
        if id == self.default_context.id {
            return Err("The default browser context cannot be disposed".to_string());
        }
        if self.browser_contexts.remove(id).is_none() {
            return Err(format!("Browser context not found: {}", id));
        }

        let page_ids: Vec<String> = self
            .pages
            .iter()
            .filter(|page| page.context.id == id)
            .map(|page| page.id.clone())
            .collect();
        for page_id in &page_ids {
            self.remove_page(page_id);
        }
        Ok(page_ids)
    }

    pub fn get_page(&self, id: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    pub fn get_page_mut(&mut self, id: &str) -> Option<&mut Page> {
        self.pages.iter_mut().find(|p| p.id == id)
    }

    pub fn remove_page(&mut self, id: &str) {
        self.pages.retain(|p| p.id != id);
        self.current_loader_ids.remove(id);
        self.announced_frames.remove(id);
        self.sessions.retain(|_, v| v != id);
    }

    pub fn get_session_page(&self, session_id: &Option<String>) -> Option<&Page> {
        let page_id = session_id.as_ref().and_then(|sid| self.sessions.get(sid))?;
        self.get_page(page_id)
    }

    pub fn get_session_page_mut(&mut self, session_id: &Option<String>) -> Option<&mut Page> {
        let page_id = session_id
            .as_ref()
            .and_then(|sid| self.sessions.get(sid))
            .cloned()?;

        // Single V8 isolate per process thread: only one page can run JS at a
        // time on this connection's thread. Park any other live isolate while
        // the target runs, and resume it afterwards.
        let target_has_js = self.pages.iter().any(|p| p.id == page_id && p.has_js());

        if !target_has_js {
            for page in &mut self.pages {
                if page.id != page_id && page.has_js() {
                    page.suspend_js();
                    break;
                }
            }
            if let Some(target) = self.pages.iter_mut().find(|p| p.id == page_id) {
                target.resume_js();
            }
        }

        self.get_page_mut(&page_id)
    }
}

pub async fn dispatch(req: &CdpRequest, ctx: &mut CdpContext) -> CdpResponse {
    // headless_chrome (and older Puppeteer) wrap every CDP call inside
    // Target.sendMessageToTarget. Unwrap and recurse.
    if req.method == "Target.sendMessageToTarget" {
        return dispatch_send_message_to_target(req, ctx).await;
    }

    let (domain, method) = match req.method.split_once('.') {
        Some((d, m)) => (d, m),
        None => {
            return CdpResponse::error(
                req.id,
                -32601,
                format!("Invalid method format: {}", req.method),
                req.session_id.clone(),
            );
        }
    };

    let result = match domain {
        "Target" => domains::target::handle(method, &req.params, ctx, &req.session_id).await,
        "Browser" => domains::browser::handle(method, &req.params).await,
        "Page" => domains::page::handle(method, &req.params, ctx, &req.session_id).await,
        "DOM" => domains::dom::handle(method, &req.params, ctx, &req.session_id).await,
        "Runtime" => domains::runtime::handle(method, &req.params, ctx, &req.session_id).await,
        "Network" => domains::network::handle(method, &req.params, ctx, &req.session_id).await,
        "Input" => domains::input::handle(method, &req.params, ctx, &req.session_id).await,
        "Emulation" => domains::emulation::handle(method, &req.params, ctx, &req.session_id).await,
        "Storage" => domains::storage::handle(method, &req.params, ctx, &req.session_id).await,
        // Accepted but no-op. Puppeteer's FrameManager.initialize calls
        // Audits.enable on connect — refusing it breaks puppeteer.connect()
        // before any user code runs.
        "Log" | "Performance" | "Security" | "CSS" | "ServiceWorker" | "Inspector" | "Debugger"
        | "Profiler" | "HeapProfiler" | "Overlay" | "Audits" | "Tracing" | "DeviceAccess"
        | "SystemInfo" | "Media" | "WebAuthn" | "Fetch" => Ok(json!({})),
        _ => Err(format!("Unknown domain: {}", domain)),
    };

    drain_binding_calls(ctx);
    drain_console_calls(ctx);

    match result {
        Ok(value) => CdpResponse::success(req.id, value, req.session_id.clone()),
        Err(msg) => {
            tracing::warn!("CDP error for {}: {}", req.method, msg);
            CdpResponse::error(req.id, -32601, msg, req.session_id.clone())
        }
    }
}

// Drain every page's binding-call queue (filled when page JS invokes a
// `Runtime.addBinding` shim) and turn each entry into a Runtime.bindingCalled
// CDP event the writer task forwards to the connected client. Called after
// every dispatch — binding calls only land in the queue while V8 is running
// inside a CDP handler, so there is no window in which they could pile up
// without a draining opportunity.
pub(crate) fn drain_binding_calls(ctx: &mut CdpContext) {
    // page_id -> every session on that page. A page commonly has more than
    // one: Target.createTarget opens a session and the Target.attachToTarget
    // that follows opens another, so a client that reaches a page the ordinary
    // way holds two and uses the second.
    let mut page_to_sessions: HashMap<&str, Vec<&str>> = HashMap::new();
    for (session_id, page_id) in &ctx.sessions {
        page_to_sessions
            .entry(page_id.as_str())
            .or_default()
            .push(session_id.as_str());
    }
    // Fix an order the events can be asserted in.
    for sessions in page_to_sessions.values_mut() {
        sessions.sort_unstable();
    }

    let mut events: Vec<CdpEvent> = Vec::new();
    for page in &mut ctx.pages {
        let calls = page.take_pending_binding_calls();
        if calls.is_empty() {
            continue;
        }
        let Some(page_sessions) = page_to_sessions.get(page.id.as_str()) else {
            // No session attached — drop the calls; there is no client to
            // deliver them to.
            continue;
        };
        for (name, payload) in calls {
            // The sessions that asked for this binding, narrowed to the page
            // the call came from. Falling back to every session of the page
            // keeps a binding installed without a session deliverable rather
            // than silently dropped.
            let registered = ctx.binding_sessions.get(&name);
            let targets: Vec<&str> = page_sessions
                .iter()
                .copied()
                .filter(|session| {
                    registered.is_none_or(|owners| owners.iter().any(|owner| owner == session))
                })
                .collect();
            let targets = if targets.is_empty() {
                page_sessions.clone()
            } else {
                targets
            };
            for session_id in targets {
                events.push(CdpEvent {
                    method: "Runtime.bindingCalled".into(),
                    // Use executionContextId=1: the default main-frame context
                    // emitted by Runtime.enable and post-navigation. Playwright's
                    // _onBindingCalled only fires for a registered context id, so
                    // a bogus id silently drops the callback.
                    params: json!({
                        "name": name,
                        "payload": payload,
                        "executionContextId": 1,
                    }),
                    session_id: Some(session_id.to_string()),
                });
            }
        }
    }
    ctx.pending_events.extend(events);
}

// Drain every page's console-call queue (filled when page JS calls
// `console.log`/`warn`/`error`, which the bootstrap shim routes through
// `op_console_msg`) and turn each into a `Runtime.consoleAPICalled` CDP event.
// Called after every dispatch, right after `drain_binding_calls`, so console
// output produced during a CDP handler becomes visible to the client on the
// same turn.
pub(crate) fn drain_console_calls(ctx: &mut CdpContext) {
    let mut page_to_sessions: HashMap<&str, Vec<&str>> = HashMap::new();
    for (session_id, page_id) in &ctx.sessions {
        page_to_sessions
            .entry(page_id.as_str())
            .or_default()
            .push(session_id.as_str());
    }
    for sessions in page_to_sessions.values_mut() {
        sessions.sort_unstable();
    }

    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);

    let mut events: Vec<CdpEvent> = Vec::new();
    for page in &mut ctx.pages {
        let calls = page.take_pending_console_calls();
        if calls.is_empty() {
            continue;
        }
        let Some(page_sessions) = page_to_sessions.get(page.id.as_str()) else {
            continue;
        };
        for (level, msg) in calls {
            let cdp_type = match level.as_str() {
                "warn" => "warning",
                "error" => "error",
                _ => "log",
            };
            for session_id in page_sessions {
                events.push(CdpEvent {
                    method: "Runtime.consoleAPICalled".into(),
                    params: json!({
                        "type": cdp_type,
                        // The bootstrap shim stringifies each argument before
                        // handing it to op_console_msg, so a console.log(a, b)
                        // arrives here as one joined string. Emit it as a single
                        // string RemoteObject — enough for a client to show the
                        // line, matching the engine's pre-existing arg handling.
                        "args": [{ "type": "string", "value": msg }],
                        "executionContextId": 1,
                        "timestamp": ts_ms,
                        "stackTrace": { "callFrames": [] },
                    }),
                    session_id: Some(session_id.to_string()),
                });
            }
        }
    }
    ctx.pending_events.extend(events);
}

async fn dispatch_send_message_to_target(req: &CdpRequest, ctx: &mut CdpContext) -> CdpResponse {
    let session_id = req
        .params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let message = match req.params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return CdpResponse::error(
                req.id,
                -32602,
                "sendMessageToTarget requires a message string".into(),
                req.session_id.clone(),
            );
        }
    };

    let inner: CdpRequest = match serde_json::from_str(message) {
        Ok(r) => r,
        Err(e) => {
            return CdpResponse::error(
                req.id,
                -32700,
                format!("sendMessageToTarget message is not a valid CDP request: {e}"),
                req.session_id.clone(),
            );
        }
    };

    // Override the inner session with the one supplied by the wrapper so the
    // inner dispatch routes against the right page. Boxing sidesteps the
    // async-fn recursion limit.
    let inner_with_session = CdpRequest {
        id: inner.id,
        method: inner.method.clone(),
        params: inner.params,
        session_id: session_id.clone().or(inner.session_id),
    };
    let inner_response = Box::pin(dispatch(&inner_with_session, ctx)).await;

    // Re-emit the inner response as the legacy event headless_chrome (and older
    // Puppeteer) listen for instead of correlating responses by id.
    let inner_serialized =
        serde_json::to_string(&inner_response).unwrap_or_else(|_| "{}".into());
    ctx.pending_events.push(CdpEvent {
        method: "Target.receivedMessageFromTarget".to_string(),
        params: json!({
            "sessionId": session_id.clone().unwrap_or_default(),
            "message": inner_serialized,
            "targetId": session_id.clone().unwrap_or_default(),
        }),
        session_id: req.session_id.clone(),
    });

    CdpResponse::success(req.id, json!({}), req.session_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diting_cdp::types::CdpRequest;

    fn create_page(ctx: &mut CdpContext) -> String {
        ctx.create_page_in_context(None)
            .expect("default browser context must exist")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_evaluate_reports_exception_thrown_and_details() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let page_id = create_page(&mut ctx);
        let session_id = "sess-1".to_string();
        ctx.sessions.insert(session_id.clone(), page_id);

        // A real document initializes the V8 isolate (create_page blanks it),
        // so the evaluate below actually runs in a live runtime.
        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({ "url": "data:text/html,<html><body>hi</body></html>" }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&nav, &mut ctx).await.error.is_none());

        let req = CdpRequest {
            id: 2,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "(() => { throw new Error('kaboom') })()",
                "returnByValue": true,
                "awaitPromise": true,
            }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&req, &mut ctx).await;
        assert!(resp.error.is_none(), "unexpected CDP error: {:?}", resp.error);
        let result = resp.result.expect("result");
        let details = result
            .get("exceptionDetails")
            .expect("response carries exceptionDetails");
        assert_eq!(details["text"], "Uncaught (in promise)");
        assert_eq!(details["exception"]["description"], "Error: kaboom");
        assert_eq!(details["exception"]["subtype"], "error");

        let thrown = ctx
            .pending_events
            .iter()
            .find(|e| e.method == "Runtime.exceptionThrown")
            .expect("Runtime.exceptionThrown event queued");
        assert_eq!(thrown.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            thrown.params["exceptionDetails"]["exception"]["description"],
            "Error: kaboom"
        );
    }

    // The click-navigation branch of Input.dispatchMouseEvent emits
    // Page.frameNavigated through frame_json(), so it must carry the same
    // required Frame fields the Page.navigate path does — a previous version
    // inlined the frame here and dropped loaderId + secureContextType +
    // crossOriginIsolatedContextType + gatedAPIFeatures (obscura#703 follow-up).
    #[tokio::test(flavor = "current_thread")]
    async fn input_click_navigation_emits_full_frame_json() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let page_id = create_page(&mut ctx);
        let session_id = "sess-click".to_string();
        ctx.sessions.insert(session_id.clone(), page_id);

        // A link whose href is a second data URL, so clicking it navigates.
        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({
                "url": "data:text/html,<a id=\"go\" href=\"data:text/html,%3Ch1%3Etwo%3C/h1%3E\">go</a>"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&nav, &mut ctx).await.error.is_none());

        // elementFromPoint has no layout in a data: page, so pre-point the
        // click target at the link (the same fallback mousePressed uses).
        let set_target = CdpRequest {
            id: 2,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "globalThis.__diting_click_target = document.getElementById('go')"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&set_target, &mut ctx).await.error.is_none());

        // Drop the initial-load navigation events so only the click's remain.
        // No Runtime.evaluate may run between the two mouse events: its
        // post-eval drain would consume the pending navigation and route the
        // frameNavigated through the Page.navigate path instead of this one.
        ctx.pending_events.clear();

        for (i, ty) in ["mousePressed", "mouseReleased"].iter().enumerate() {
            let req = CdpRequest {
                id: 3 + i as u64,
                method: "Input.dispatchMouseEvent".to_string(),
                // Out-of-viewport coords make elementFromPoint return null (its
                // stub returns <body> for in-viewport hits), so the click-target
                // resolution falls through to __diting_click_target — the same
                // fallback the engine relies on for layout-less pages.
                params: json!({
                    "type": ty,
                    "x": -1,
                    "y": -1,
                    "button": "left",
                    "clickCount": 1,
                }),
                session_id: Some(session_id.clone()),
            };
            assert!(
                dispatch(&req, &mut ctx).await.error.is_none(),
                "dispatchMouseEvent {ty} failed"
            );
        }

        let ev = ctx
            .pending_events
            .iter()
            .find(|e| e.method == "Page.frameNavigated")
            .unwrap_or_else(|| {
                let methods: Vec<_> = ctx.pending_events.iter().map(|e| e.method.clone()).collect();
                panic!("click navigation emits Page.frameNavigated; got {methods:?}")
            });
        let frame = &ev.params["frame"];
        assert!(
            frame.get("loaderId").is_some(),
            "frame carries loaderId: {frame}"
        );
        assert_eq!(frame["secureContextType"], "Secure", "frame: {frame}");
        assert_eq!(
            frame["crossOriginIsolatedContextType"],
            "NotIsolated",
            "frame: {frame}"
        );
        assert!(frame["gatedAPIFeatures"].is_array(), "frame: {frame}");
        assert!(
            frame["url"]
                .as_str()
                .map(|u| u.contains("two"))
                .unwrap_or(false),
            "click navigated to the second URL: {frame}"
        );
    }

    // A CDP mouse click on a <label for=...> must activate its labeled
    // control (checkbox flips, input+change fire) — the same activation the
    // HTMLElement.click() path implements. Without forwarding, Puppeteer's
    // label.click() on Webflow-style checkbox markup silently no-ops.
    #[tokio::test(flavor = "current_thread")]
    async fn input_mouse_click_on_label_activates_control() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let page_id = create_page(&mut ctx);
        let session_id = "sess-label".to_string();
        ctx.sessions.insert(session_id.clone(), page_id);

        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({
                "url": "data:text/html,<label for=c>toggle</label><input type=checkbox id=c>"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&nav, &mut ctx).await.error.is_none());

        // Point the click at the label (elementFromPoint has no layout in a
        // data: page; mousePressed falls back to __diting_click_target).
        let set_target = CdpRequest {
            id: 2,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "globalThis.__diting_click_target = document.querySelector('label')"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&set_target, &mut ctx).await.error.is_none());
        ctx.pending_events.clear();

        for (i, ty) in ["mousePressed", "mouseReleased"].iter().enumerate() {
            let req = CdpRequest {
                id: 3 + i as u64,
                method: "Input.dispatchMouseEvent".to_string(),
                params: json!({
                    "type": ty,
                    "x": -1,
                    "y": -1,
                    "button": "left",
                    "clickCount": 1,
                }),
                session_id: Some(session_id.clone()),
            };
            assert!(
                dispatch(&req, &mut ctx).await.error.is_none(),
                "dispatchMouseEvent {ty} failed"
            );
        }

        // No Runtime.evaluate after the click either — read state via a fresh
        // evaluate only once the click has fully landed.
        let check = CdpRequest {
            id: 5,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "(function(){var c=document.getElementById('c');return 'checked='+c.checked;})()",
                "returnByValue": true,
            }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&check, &mut ctx).await;
        assert!(resp.error.is_none(), "evaluate failed: {:?}", resp.error);
        let result = resp.result.expect("result");
        let value = result["result"]["value"].as_str().expect("string value");
        assert_eq!(value, "checked=true", "label click activated the checkbox");
    }

    // Input.insertText (obscura#577): text that doesn't come from a key
    // press — IME commits, emoji pickers, paste-style agent drivers — has
    // no dispatchKeyEvent representation, so without this arm such clients
    // cannot type at all. It must insert at the caret, replace a selection,
    // and fire an input event announcing the new value (React/Vue
    // controlled inputs only register changes through that combination).
    // The helper embeds text as a serde_json string literal, so CJK and
    // control characters ride through too — the escaping trap upstream's
    // first arm hit in #688.
    #[tokio::test(flavor = "current_thread")]
    async fn input_insert_text_types_into_focused_field() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let page_id = create_page(&mut ctx);
        let session_id = "sess-insert-text".to_string();
        ctx.sessions.insert(session_id.clone(), page_id);

        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({
                "url": "data:text/html,<input id=t>"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&nav, &mut ctx).await.error.is_none());

        // Focus the field, park the caret at position 1 of an existing
        // value, and record every input event's resulting value.
        let park = CdpRequest {
            id: 2,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "(function(){var el=document.getElementById('t');el.value='ab';el.focus();el.setSelectionRange(1,1);window.seen=[];el.addEventListener('input',function(){seen.push(el.value)});})()"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&park, &mut ctx).await.error.is_none());

        let req = CdpRequest {
            id: 3,
            method: "Input.insertText".to_string(),
            params: json!({ "text": "XY" }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&req, &mut ctx).await;
        assert!(
            resp.error.is_none(),
            "Input.insertText failed: {:?}",
            resp.error
        );

        let check = CdpRequest {
            id: 4,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "document.getElementById('t').value + '|' + window.seen.join(',')",
                "returnByValue": true,
            }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&check, &mut ctx).await;
        assert!(resp.error.is_none(), "evaluate failed: {:?}", resp.error);
        let result = resp.result.expect("result");
        let value = result["result"]["value"].as_str().expect("string value");
        assert_eq!(
            value, "aXYb|aXYb",
            "inserted at the caret AND announced via an input event"
        );

        // CJK plus a control character: both must arrive verbatim.
        let cjk = CdpRequest {
            id: 5,
            method: "Input.insertText".to_string(),
            params: json!({ "text": "中\u{1}文" }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&cjk, &mut ctx).await.error.is_none());
        let check = CdpRequest {
            id: 6,
            method: "Runtime.evaluate".to_string(),
            params: json!({
                "expression": "document.getElementById('t').value",
                "returnByValue": true,
            }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&check, &mut ctx).await;
        assert!(resp.error.is_none());
        let result = resp.result.expect("result");
        let value = result["result"]["value"].as_str().expect("string value");
        assert_eq!(
            value, "aXY中\u{1}文b",
            "CJK and control chars insert verbatim"
        );
    }

    // DOM.getBoxModel (obscura#576): Chrome's wire emits integral doubles
    // without the ".0", and strict clients deserialize quads as i64. The
    // whole pipeline — evaluate through the quad helpers — must land on
    // integer-typed JSON numbers for integral coordinates.
    #[tokio::test(flavor = "current_thread")]
    async fn dom_get_box_model_emits_integral_coords_as_integers() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let page_id = create_page(&mut ctx);
        let session_id = "sess-box-model".to_string();
        ctx.sessions.insert(session_id.clone(), page_id);

        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({
                "url": "data:text/html,<div id=d style='position:absolute;left:0;top:0;width:100px;height:20px'>x</div>"
            }),
            session_id: Some(session_id.clone()),
        };
        assert!(dispatch(&nav, &mut ctx).await.error.is_none());

        let doc = CdpRequest {
            id: 2,
            method: "DOM.getDocument".to_string(),
            params: json!({}),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&doc, &mut ctx).await;
        assert!(resp.error.is_none(), "getDocument failed: {:?}", resp.error);
        let root_id = resp.result.expect("result")["root"]["nodeId"]
            .as_u64()
            .expect("root nodeId");

        let query = CdpRequest {
            id: 3,
            method: "DOM.querySelector".to_string(),
            params: json!({ "nodeId": root_id, "selector": "#d" }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&query, &mut ctx).await;
        assert!(
            resp.error.is_none(),
            "querySelector failed: {:?}",
            resp.error
        );
        let node_id = resp.result.expect("result")["nodeId"]
            .as_u64()
            .expect("matched nodeId");
        assert!(node_id > 0, "selector must match the div");

        let box_req = CdpRequest {
            id: 4,
            method: "DOM.getBoxModel".to_string(),
            params: json!({ "nodeId": node_id }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch(&box_req, &mut ctx).await;
        assert!(resp.error.is_none(), "getBoxModel failed: {:?}", resp.error);
        let model = &resp.result.expect("result")["model"];
        let content = model["content"].as_array().expect("content quad");
        assert_eq!(content.len(), 8);
        // A zero-layout test page yields all-zero (or fallback) integral
        // coords — the point is the JSON *type*: i64, never 0.0.
        assert!(
            content.iter().all(|v| v.is_i64()),
            "integral coords must be integer-typed on the wire, got {content:?}"
        );
        let wire = serde_json::to_string(content).expect("serialize");
        assert!(
            !wire.contains(".0") && !wire.contains("."),
            "integral coords must serialize without a decimal point, got {wire}"
        );
        assert!(
            model["width"].is_i64() && model["height"].is_i64(),
            "integral width/height must be integer-typed, got {}x{}",
            model["width"],
            model["height"]
        );
    }
}
