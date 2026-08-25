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

    pub fn create_page(&mut self) -> String {
        self.create_page_in_context(None)
            .expect("default browser context must exist")
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

/// Whether a CDP method can be dispatched WITHOUT routing through
/// `get_session_page_mut` (which triggers suspend_js/resume_js). Used by the
/// server layer to skip the per-command JS park for pure-bookkeeping methods.
pub(crate) fn is_v8_free_method(method: &str) -> bool {
    matches!(
        method,
        "Target.getTargets"
            | "Target.setDiscoverTargets"
            | "Target.attachToTarget"
            | "Target.attachToBrowserTarget"
            | "Target.setAutoAttach"
            | "Target.getBrowserContexts"
            | "Target.createBrowserContext"
            | "Target.disposeBrowserContext"
            | "Target.getTargetInfo"
            | "Target.detachFromTarget"
            | "Target.activateTarget"
            | "Browser.getVersion"
            | "Browser.close"
            | "Browser.getWindowForTarget"
            | "Browser.setDownloadBehavior"
            | "Browser.getWindowBounds"
            | "Browser.setWindowBounds"
            | "Page.enable"
            | "Page.disable"
            | "Page.getFrameTree"
            | "Page.setDownloadBehavior"
            | "Page.setLifecycleEventsEnabled"
            | "Page.addScriptToEvaluateOnNewDocument"
            | "Page.removeScriptToEvaluateOnNewDocument"
            | "Page.setInterceptFileChooserDialog"
            | "Page.getNavigationHistory"
            | "Page.resetNavigationHistory"
            | "Page.captureSnapshot"
            | "Page.createIsolatedWorld"
            | "Runtime.enable"
            | "Runtime.disable"
            | "Runtime.runIfWaitingForDebugger"
            | "Runtime.getExceptionDetails"
            | "Runtime.discardConsoleEntries"
            | "Network.enable"
            | "Network.disable"
            | "Network.setCacheDisabled"
            | "Network.setRequestInterception"
            | "Network.getBlockedUrls"
            | "Network.getCookies"
            | "Network.getAllCookies"
            | "Storage.getCookies"
            | "Storage.setCookies"
            | "Storage.clearCookies"
            | "Storage.deleteCookies"
            | "Emulation.setTouchEmulationEnabled"
            | "Emulation.setFocusEmulationEnabled"
    )
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
