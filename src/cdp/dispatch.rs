//! CDP command routing + per-connection state (claimed from obscura-cdp
//! dispatch.rs, adapted to the diting engine).
//!
//! Adaptation notes vs upstream:
//! - no screencasts (upstream render feature; diting screenshots go through
//!   `crate::screenshot::render_html_to_png_diting` instead)
//! - Fetch interception pauses only script-initiated fetch()/XHR at the
//!   Request stage (document/subresource loads ride the navigation transport)

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use diting::diting_browser::{BrowserContext, Page};
use diting::diting_net::url_pattern_matches;
use diting::diting_js::ops::{InterceptResolution, InterceptedRequest};
use serde_json::{json, Value};

use crate::cdp::domains;
use crate::cdp::types::{CdpEvent, CdpRequest, CdpResponse};

/// A pause waiting for the client's `Fetch.continueRequest` /
/// `fulfillRequest` / `failRequest`. Only the id the client answered with and
/// the resolver matter — the request shape already went out on the wire in
/// the `Fetch.requestPaused` event.
pub struct PendingPause {
    pub bridge_request_id: String,
    /// Request URL / resource type, echoed on the Network-domain events the
    /// bridge emits when the pause resolves (fulfill → responseReceived,
    /// fail → loadingFailed).
    pub url: String,
    pub resource_type: String,
    pub resolver: Option<tokio::sync::oneshot::Sender<InterceptResolution>>,
}

/// Per-page Fetch-intercept state, created by `Fetch.enable`.
pub struct FetchInterceptState {
    /// `Fetch.enable` urlPatterns; empty matches every request.
    pub patterns: Vec<String>,
    /// Engine → bridge channel for parked requests (armed via
    /// `Page::set_fetch_intercept`).
    pub rx: tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>,
    pub pending: Vec<PendingPause>,
}

/// One `Page.startScreencast` subscription. Lives on the CdpContext (not the
/// Page) so it dies with the WebSocket connection, like `fetch_intercept`.
/// Keyed by page id; events carry the session that armed it.
#[derive(Clone, Debug)]
#[cfg(feature = "screenshot")]
pub struct ScreencastState {
    /// The CDP session that armed the subscription; frames carry it so a
    /// second connection's session filter doesn't swallow them.
    pub session_id: Option<String>,
    pub format: String,
    /// JPEG quality 0-100; ignored for png.
    pub quality: u8,
    pub max_width: u32,
    pub max_height: u32,
    pub every_nth_frame: u32,
    pub frame_seq: u64,
    /// A frame is in flight until the client acks (`Page.screencastFrameAck`);
    /// while set, the pump skips this page (Chrome's backpressure).
    pub outstanding_ack: bool,
    /// Damage signature of the last emitted frame: (realm gen, dom epoch,
    /// layout rev, scroll_x, scroll_y, viewport w, h). A tick with an
    /// unchanged signature emits nothing — static pages cost zero frames.
    /// The layout rev covers attribute-level mutations, which clear the
    /// layout cache without moving the tree-shape epoch; the realm gen
    /// disambiguates across navigations, where epoch and rev restart at 0
    /// and two static pages could otherwise collide (#189).
    pub last_damage: Option<(u64, u64, u64, f32, f32, f32, f32)>,
}

pub struct CdpContext {
    pub pages: Vec<Page>,
    pub sessions: HashMap<String, String>, // session_id -> page_id
    /// Current document loader per page. Navigation events and later
    /// script-initiated Network events must share this id; inventing a loader
    /// for each fetch breaks DevTools request grouping.
    pub current_loader_ids: HashMap<String, String>,
    /// Child (iframe) frames of each page, in document order. Single-realm
    /// engine: a child frame has no real JS context of its own — evals
    /// targeting it run in the main realm with the frame's document swapped
    /// into the global — but the CDP frame surface (frameAttached /
    /// frameNavigated / per-frame executionContextCreated / getFrameTree)
    /// must exist for clients that model iframes (Playwright frameLocator).
    pub child_frames: HashMap<String, Vec<ChildFrame>>,
    /// executionContextId -> (page_id, frame_id) for contexts minted for
    /// child frames (default + isolated worlds). Main-frame contexts are not
    /// in this map; lookup falls back to the session's page.
    pub frame_contexts: HashMap<i64, (String, String)>,
    /// objectId ("injected-script-id") -> iframe arena node id, for
    /// callFunctionOn scope recovery: Playwright's evaluateWithArguments sends
    /// only the utility-script objectId, so the target frame is inferred
    /// from which frame minted that object.
    pub frame_object_ids: HashMap<String, u32>,
    /// Minting counter for child frame ids ("frame-N").
    pub next_child_frame_seq: u64,
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
    // Sessions that called `Network.enable`. Chrome delivers a target's
    // Network.* events to every session that enabled the domain, not just
    // the one that drove the navigation — the Playwright `newCDPSession`
    // observer shape (#13).
    pub network_enabled_sessions: HashSet<String>,
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
    /// Fetch-intercept state per page (`Fetch.enable` arms it). Lives on the
    /// context, not the Page, so it dies with the WebSocket connection.
    pub fetch_intercept: HashMap<String, FetchInterceptState>,
    /// Active screencast subscriptions per page (`Page.startScreencast`).
    /// Same connection-scoped lifetime as `fetch_intercept`.
    #[cfg(feature = "screenshot")]
    pub screencast: HashMap<String, ScreencastState>,
    /// Bridge-unique pause ids (`fi-{N}`) — independent of the engine's
    /// per-realm `intercept-{N}` ids, which restart at 1 after navigation.
    pub fetch_pause_counter: u64,
    /// The page a page-scoped WebSocket connection (`/devtools/page/<id>`)
    /// auto-provisions. Naive CDP clients attach to a page endpoint and send
    /// sessionless commands (`Runtime.evaluate` with no `sessionId`), which
    /// Chrome routes to that one target; without this fallback those commands
    /// would find no page (obscura#680).
    pub default_page: Option<String>,
    /// CDP navigations currently running on a spawned task (the WebSocket
    /// loop's LocalSet), keyed by page id. While a load is in flight the
    /// page is owned by the load task — not `pages` — so commands resolving
    /// to it are parked in `deferred` until the load lands.
    pub pending_loads: HashMap<String, PendingNav>,
    /// Commands that resolved to a loading page, replayed in order once the
    /// load lands (the WebSocket loop's receiver arm decides via
    /// `should_park`).
    pub deferred: Vec<CdpRequest>,
    /// Spawn channel for CDP-driven navigations. Set only by the WebSocket
    /// loop (a LocalSet — `Page` is `!Send`); every other face leaves it
    /// `None` and keeps the navigate-inline, respond-after-load contract.
    pub load_tx: Option<futures::channel::mpsc::UnboundedSender<LoadDone>>,
}

/// A CDP navigation handed to a spawned task. The task owns the `Page` for
/// the duration (removed from `pages`), so the loop keeps serving commands
/// for every other page — and the socket itself — while the load's network
/// awaits yield.
pub struct LoadDone {
    pub page_id: String,
    pub page: Page,
    pub result: Result<(), String>,
}

/// Commit-phase bookkeeping a spawned navigation leaves behind: which
/// session drove it (event routing), the loader the outgoing document's
/// carried events belong to (re-emitted by the tail), and the URL announced
/// at commit — the tail re-announces the frame if a redirect chain landed
/// elsewhere.
pub struct PendingNav {
    pub session_id: Option<String>,
    pub old_loader: String,
    pub announced_url: String,
}

/// One live child frame: the iframe element's arena node id is the identity
/// (it survives attribute mutations; removal detaches the frame). `contexts`
/// lists every executionContextId minted for this frame — default world
/// first, then one per registered isolated world.
pub struct ChildFrame {
    pub frame_id: String,
    pub nid: u32,
    pub url: String,
    pub name: String,
    pub loader_id: String,
    pub contexts: Vec<i64>,
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
            child_frames: HashMap::new(),
            frame_contexts: HashMap::new(),
            frame_object_ids: HashMap::new(),
            next_child_frame_seq: 0,
            pending_events: Vec::new(),
            default_context,
            browser_contexts: HashMap::new(),
            page_counter: 0,
            browser_context_counter: 0,
            target_session_counter: 0,
            preload_scripts: Vec::new(),
            binding_sessions: HashMap::new(),
            network_enabled_sessions: HashSet::new(),
            preload_counter: 0,
            isolated_worlds: Vec::new(),
            valid_context_ids,
            next_isolated_context_id: 100,
            fetch_intercept: HashMap::new(),
            fetch_pause_counter: 0,
            #[cfg(feature = "screenshot")]
            screencast: HashMap::new(),
            default_page: None,
            pending_loads: HashMap::new(),
            deferred: Vec::new(),
            load_tx: None,
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
        // Unpark anything the client never answered; the dropped oneshot side
        // falls through to the real request on the engine side anyway, but an
        // explicit Continue keeps semantics obvious.
        if let Some(mut state) = self.fetch_intercept.remove(id) {
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
        }
        self.pages.retain(|p| p.id != id);
        self.current_loader_ids.remove(id);
        self.child_frames.remove(id);
        self.frame_contexts.retain(|_, (pid, _)| pid != id);
        self.sessions.retain(|_, v| v != id);
        // Sessions of the closed page are gone from `sessions`; their
        // Network-enable state must not outlive them.
        self.network_enabled_sessions
            .retain(|sid| self.sessions.contains_key(sid));
    }

    /// Sessions besides `navigating` that should receive a page's Network.*
    /// navigation events: attached to `page_id` and armed via
    /// `Network.enable`. Chrome delivers a target's network events to every
    /// enabled session, not just the one that drove the navigation — the
    /// Playwright `newCDPSession(page)` observer watching a goto another
    /// session issued (#13). Sorted so delivery order is deterministic.
    pub fn other_network_sessions(
        &self,
        navigating: &Option<String>,
        page_id: &str,
    ) -> Vec<String> {
        let mut others: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, pid)| pid.as_str() == page_id)
            .map(|(sid, _)| sid.clone())
            .filter(|sid| self.network_enabled_sessions.contains(sid))
            .filter(|sid| navigating.as_deref() != Some(sid.as_str()))
            .collect();
        others.sort_unstable();
        others
    }

    /// Session lookup with the page-connection fallback: a command with no
    /// (or an unknown) `sessionId` on a page-scoped WebSocket routes to the
    /// auto-provisioned page, the way Chrome scopes every command on a
    /// page-level endpoint to that target. Browser-level connections leave
    /// `default_page` unset and behave exactly as before.
    fn resolve_page_id(&self, session_id: &Option<String>) -> Option<&String> {
        session_id
            .as_ref()
            .and_then(|sid| self.sessions.get(sid))
            .or(self.default_page.as_ref())
    }

    pub fn get_session_page(&self, session_id: &Option<String>) -> Option<&Page> {
        let page_id = self.resolve_page_id(session_id)?;
        self.get_page(page_id)
    }

    /// The page id a session routes to. Domain handlers that must touch both
    /// the page and other context state use this to avoid overlapping borrows.
    pub fn session_page_id(&self, session_id: &Option<String>) -> Option<&String> {
        self.resolve_page_id(session_id)
    }

    pub fn next_child_frame_id(&mut self) -> String {
        self.next_child_frame_seq += 1;
        format!("frame-{}", self.next_child_frame_seq)
    }

    /// Drop a page's child frames, returning the removed entries so the
    /// caller can emit frameDetached / clean context ids. Also clears the
    /// context and object-id side tables.
    pub fn clear_child_frames(&mut self, page_id: &str) -> Vec<ChildFrame> {
        let removed = self.child_frames.remove(page_id).unwrap_or_default();
        for frame in &removed {
            for cid in &frame.contexts {
                self.frame_contexts.remove(cid);
                self.valid_context_ids.remove(cid);
            }
        }
        if !removed.is_empty() {
            let nids: HashSet<u32> = removed.iter().map(|f| f.nid).collect();
            self.frame_object_ids.retain(|_, nid| !nids.contains(nid));
        }
        removed
    }

    /// Find a page's child frame by its iframe element's arena node id.
    pub fn child_frame_by_nid(&self, page_id: &str, nid: u32) -> Option<&ChildFrame> {
        self.child_frames
            .get(page_id)?
            .iter()
            .find(|f| f.nid == nid)
    }

    pub fn get_session_page_mut(&mut self, session_id: &Option<String>) -> Option<&mut Page> {
        let page_id = self
            .resolve_page_id(session_id)
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

    /// Remove and own a page temporarily — the spawned-navigation handoff.
    /// Unlike `remove_page`, no subscription cleanup runs: the page comes
    /// back through `LoadDone`.
    pub fn take_page(&mut self, id: &str) -> Option<Page> {
        let idx = self.pages.iter().position(|p| p.id == id)?;
        Some(self.pages.remove(idx))
    }

    /// True when this command resolves to a page whose CDP navigation is
    /// in flight on a spawned task — the loop parks it in `deferred`
    /// instead of dispatching into a missing page. Chrome answers such
    /// commands against the provisional document; with a single realm,
    /// queueing is the honest approximation (identical to the pre-spawn
    /// behavior, where every command queued behind the load).
    pub fn should_park(&self, req: &CdpRequest) -> bool {
        // enable/disable never touch pages; clients arm subscriptions
        // (Runtime.enable, Fetch.enable) around navigations and must not
        // stall behind the load.
        if req.method.ends_with(".enable") || req.method.ends_with(".disable") {
            return false;
        }
        // Fetch resolutions answer parked requests — including a navigation
        // document parked mid-load (page.route over Page.navigate). They only
        // touch the intercept state, never the checked-out page, so parking
        // them would deadlock the load until the resolution timeout.
        if matches!(
            req.method.as_str(),
            "Fetch.continueRequest" | "Fetch.fulfillRequest" | "Fetch.failRequest"
        ) {
            return false;
        }
        self.session_page_id(&req.session_id)
            .map(|pid| self.pending_loads.contains_key(pid))
            .unwrap_or(false)
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
        "Accessibility" => {
            domains::accessibility::handle(method, &req.params, ctx, &req.session_id).await
        }
        "Browser" => domains::browser::handle(method, &req.params, ctx, &req.session_id).await,
        "Page" => domains::page::handle(method, &req.params, ctx, &req.session_id).await,
        "DOM" => domains::dom::handle(method, &req.params, ctx, &req.session_id).await,
        "Runtime" => domains::runtime::handle(method, &req.params, ctx, &req.session_id).await,
        "Network" => domains::network::handle(method, &req.params, ctx, &req.session_id).await,
        "Input" => domains::input::handle(method, &req.params, ctx, &req.session_id).await,
        "Emulation" => domains::emulation::handle(method, &req.params, ctx, &req.session_id).await,
        "Storage" => domains::storage::handle(method, &req.params, ctx, &req.session_id).await,
        "Fetch" => domains::fetch::handle(method, &req.params, ctx, &req.session_id).await,
        // Accepted but no-op. Puppeteer's FrameManager.initialize calls
        // Audits.enable on connect — refusing it breaks puppeteer.connect()
        // before any user code runs.
        "Log" | "Performance" | "Security" | "CSS" | "ServiceWorker" | "Inspector" | "Debugger"
        | "Profiler" | "HeapProfiler" | "Overlay" | "Audits" | "Tracing" | "DeviceAccess"
        | "SystemInfo" | "Media" | "WebAuthn" => Ok(json!({})),
        _ => Err(format!("Unknown domain: {}", domain)),
    };

    drain_and_settle(ctx).await;

    match result {
        Ok(value) => CdpResponse::success(req.id, value, req.session_id.clone()),
        Err(msg) => {
            // A domain handler pins a Chrome-accurate protocol code by
            // prefixing its message with `#-32000 ` (Chrome's DOM errors
            // carry -32000, not the JSON-RPC method-not-found -32601 this
            // arm defaults to). The prefix is stripped before the wire.
            let (code, msg) = match msg.strip_prefix("#-32000 ") {
                Some(m) => (-32000, m.to_string()),
                None => (-32601, msg),
            };
            tracing::warn!("CDP error for {}: {}", req.method, msg);
            CdpResponse::error(req.id, code, msg, req.session_id.clone())
        }
    }
}

/// The post-V8 drain shared by `dispatch` and the connection loop's idle
/// pump: both just ran JS, so flush everything that could only have queued
/// while V8 was running — binding/console calls, `document.write`
/// navigation tails, and answered intercept requests. The intercept settle
/// runs the continuations of every fetch the drain just answered itself;
/// without it an auto-continued request sits parked until the event loop is
/// next polled.
pub(crate) async fn drain_and_settle(ctx: &mut CdpContext) {
    drain_binding_calls(ctx);
    drain_console_calls(ctx);
    drain_write_navs(ctx);
    let settle_pages = drain_intercept_calls(ctx);
    for page_id in settle_pages {
        if let Some(page) = ctx.get_page_mut(&page_id) {
            page.settle(50).await;
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
// `console.log`/`warn`/`error`/`debug`, which the bootstrap shim routes
// through `op_console_msg`) and turn each into a `Runtime.consoleAPICalled`
// CDP event.
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
        for (level, msg, _log_url) in calls {
            let cdp_type = match level.as_str() {
                "warn" => "warning",
                "error" => "error",
                "debug" => "debug",
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

// Drain pages that ran `document.write()` and re-emit the post-navigation
// event sequence for each: that parse produced a fresh document, so
// DOMContentLoaded/load fire again (Playwright's setContent blocks on the
// fresh `load` after its tag console message resets the frame's lifecycle).
// Runs AFTER `drain_console_calls` by contract — the tag message must be
// delivered first, or the load events land before the lifecycle reset and
// Playwright clears them, then waits forever (the 30s setContent timeout).
fn drain_write_navs(ctx: &mut CdpContext) {
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

    let mut writers: Vec<(String, Vec<String>)> = Vec::new();
    for page in &mut ctx.pages {
        if page.take_pending_write_nav() {
            let sessions = page_to_sessions
                .get(page.id.as_str())
                .map(|ss| ss.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            writers.push((page.id.clone(), sessions));
        }
    }
    for (page_id, sessions) in writers {
        if sessions.is_empty() {
            domains::page::emit_navigation_for_page(ctx, &None, &[], &page_id);
        } else {
            // Per-session enumeration: each session already gets the full
            // sequence here, so no extra Network fan-out targets (double
            // delivery otherwise).
            for sid in sessions {
                domains::page::emit_navigation_for_page(ctx, &Some(sid), &[], &page_id);
            }
        }
    }
}

// Drain every armed page's intercept channel and surface parked requests as
// `Fetch.requestPaused` events, following the same after-every-dispatch shape
// as the binding/console drains. Pauses only land in the channel while V8
// runs inside a CDP handler, so between-command draining cannot lose one;
// a pause created by a dispatch that itself parks on the parked fetch
// (Runtime.evaluate of an awaited fetch) is undeliverable until that dispatch
// ends — the engine bounds that wait instead (INTERCEPT_RESOLUTION_TIMEOUT_MS).
//
// Returns the page ids whose parked fetches were answered with an automatic
// Continue (no session attached, or the URL matched no pattern). Those
// resolutions unblock continuations that only run when the JS event loop is
// next polled — a sync Runtime.evaluate never polls it — so the dispatcher
// settles each returned page right after this drain.
pub(crate) fn drain_intercept_calls(ctx: &mut CdpContext) -> Vec<String> {
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

    let mut events: Vec<CdpEvent> = Vec::new();
    let mut needs_settle: Vec<String> = Vec::new();
    for (page_id, state) in ctx.fetch_intercept.iter_mut() {
        let mut parked = Vec::new();
        while let Ok(intercepted) = state.rx.try_recv() {
            parked.push(intercepted);
        }
        if parked.is_empty() {
            continue;
        }
        let Some(page_sessions) = page_to_sessions.get(page_id.as_str()) else {
            // No session attached: answer Continue so the fetch proceeds.
            for req in parked {
                let _ = req.resolver.send(InterceptResolution::Continue {
                    url: None,
                    method: None,
                    headers: None,
                    body: None,
                });
            }
            needs_settle.push(page_id.clone());
            continue;
        };
        for req in parked {
            let matched = state.patterns.is_empty()
                || state
                    .patterns
                    .iter()
                    .any(|p| url_pattern_matches(p, &req.url));
            if !matched {
                let _ = req.resolver.send(InterceptResolution::Continue {
                    url: None,
                    method: None,
                    headers: None,
                    body: None,
                });
                if !needs_settle.contains(page_id) {
                    needs_settle.push(page_id.clone());
                }
                continue;
            }
            ctx.fetch_pause_counter += 1;
            let bridge_request_id = format!("fi-{}", ctx.fetch_pause_counter);
            // Hide the engine's out-of-band markers from the client view.
            let headers: serde_json::Map<String, Value> = req
                .headers
                .iter()
                .filter(|(k, _)| !k.starts_with("__diting_"))
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let request_obj = json!({
                "url": req.url.clone(),
                "method": req.method.clone(),
                "headers": headers,
                "initialPriority": "High",
                "referrer": "",
            });
            // Chrome pairs every pause with a Network.requestWillBeSent under
            // one request id, and clients route on that pairing: Playwright
            // auto-continues pauses without `networkId` and never surfaces a
            // request that has no willBeSent to pair with. The bridge id
            // stands in for both halves.
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let will_be_sent = json!({
                "requestId": bridge_request_id,
                "request": request_obj.clone(),
                "timestamp": ts,
                "wallTime": ts,
                "initiator": { "type": "other" },
                "type": req.resource_type.clone(),
                "frameId": page_id.clone(),
                "hasUserGesture": false,
            });
            let paused = json!({
                "requestId": bridge_request_id,
                "networkId": bridge_request_id,
                "request": request_obj,
                "resourceType": req.resource_type.clone(),
                "frameId": page_id.clone(),
            });
            for session_id in page_sessions {
                events.push(CdpEvent {
                    method: "Network.requestWillBeSent".into(),
                    params: will_be_sent.clone(),
                    session_id: Some(session_id.to_string()),
                });
                events.push(CdpEvent {
                    method: "Fetch.requestPaused".into(),
                    params: paused.clone(),
                    session_id: Some(session_id.to_string()),
                });
            }
            state.pending.push(PendingPause {
                bridge_request_id,
                url: req.url.clone(),
                resource_type: req.resource_type.clone(),
                resolver: Some(req.resolver),
            });
        }
    }
    ctx.pending_events.extend(events);
    needs_settle
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
mod tests;
