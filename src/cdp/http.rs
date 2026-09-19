//! axum-mounted HTTP/WS surface for the CDP bridge (claimed from upstream
//! obscura-cdp's TCP accept loop, replaced with axum so it rides the same
//! bind + logging + TLS-termination path as the HTTP API).
//!
//! Model: thread-per-connection. Each `/devtools/*` WebSocket upgrade spawns a
//! dedicated OS thread running a current-thread Tokio runtime + `LocalSet`, and
//! builds an isolated `CdpContext` (own cookie jar + HTTP client) there. The
//! `Page`s inside are `!Send` (deno_core `Rc<RefCell<…>>` state), so the whole
//! dispatch loop — including every `.await` — stays pinned to that one thread.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Host, Path};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;

use crate::cdp::dispatch::{self, CdpContext, LoadDone};
use crate::cdp::domains::page::{emit_navigation_for_page, emit_navigation_tail};
use crate::cdp::types::{CdpRequest, CdpResponse};

/// Advertised browser identity. Playwright parses the Chrome major from the
/// `Browser` field of `/json/version`; a malformed or missing value aborts
/// `connectOverCDP` before any user code runs.
const BROWSER_STRING: &str = "Chrome/122.0.6261.69";
const PROTOCOL_VERSION: &str = "1.3";

/// `/json/version` — the discovery endpoint Playwright (`connectOverCDP`) and
/// Puppeteer (`connect`) hit first to learn the WebSocket debugger URL. Each
/// call mints a fresh browser id; there is no persistent browser registry.
pub async fn json_version(Host(host): Host) -> impl IntoResponse {
    let browser_id = uuid::Uuid::new_v4();
    axum::Json(json!({
        "Browser": BROWSER_STRING,
        "Protocol-Version": PROTOCOL_VERSION,
        "User-Agent": BROWSER_STRING,
        "V8-Version": "12.2.285.20",
        "WebKit-Version": "537.36",
        "webSocketDebuggerUrl": format!("ws://{host}/devtools/browser/{browser_id}"),
    }))
}

/// `/json/list` (and `/json`) — Chrome's fresh launch lists exactly one
/// about:blank page, and naive CDP clients depend on that shape: read the
/// list, connect to the page's `webSocketDebuggerUrl`, drive it with
/// sessionless commands. The listed page is a per-connection template — it
/// materializes when a client attaches to its WebSocket and belongs to that
/// connection alone; browser-level connections still start empty and create
/// targets explicitly (see `Target.getTargets`).
pub async fn json_list(Host(host): Host) -> impl IntoResponse {
    let page_id = uuid::Uuid::new_v4();
    axum::Json(json!([{
        "description": "",
        "id": format!("{page_id}"),
        "title": "about:blank",
        "type": "page",
        "url": "about:blank",
        "webSocketDebuggerUrl": format!("ws://{host}/devtools/page/{page_id}"),
    }]))
}

/// WebSocket upgrade handler for `/devtools/{kind}/{id}`. A `browser`
/// connection starts empty and creates targets on demand through
/// `Target.createTarget`. A `page` connection auto-provisions the one page
/// the endpoint names and routes sessionless commands to it — Chrome scopes
/// every command on a page-level endpoint to that target (obscura#680).
pub async fn devtools_ws(
    ws: WebSocketUpgrade,
    Path((kind, _id)): Path<(String, String)>,
) -> impl IntoResponse {
    let page_mode = kind == "page";
    ws.on_upgrade(move |socket| async move {
        // The dispatch loop is blocking (it parks V8 on one thread), so hand
        // the socket to the blocking pool rather than pinning a Tokio worker
        // for the connection's lifetime.
        let _ = tokio::task::spawn_blocking(move || run_connection(socket, page_mode));
    })
}

/// Build a current-thread runtime + `LocalSet` on the calling (blocking) thread
/// and drive the connection loop there. Every `Page` and `CdpContext` lives and
/// dies on this one thread, which is what deno_core requires.
fn run_connection(socket: WebSocket, page_mode: bool) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("CDP: failed to build runtime: {e}");
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, connection_loop(socket, page_mode));
}

/// Serialize a command response plus the events dispatch produced onto the
/// socket. The `Target.attach*` family emits events before the response —
/// Chrome emits Target.attachedToTarget BEFORE the createTarget /
/// attachToTarget response, and Playwright's doCreateNewPage looks up the new
/// page in its internal _crPages map synchronously right after the response
/// resolves, so flush the event first or the lookup finds nothing and
/// newPage() throws. Returns false once the socket is gone.
async fn send_out(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    response: &CdpResponse,
    method: &str,
    ctx: &mut CdpContext,
) -> bool {
    let events_first = matches!(
        method,
        "Target.createTarget" | "Target.attachToTarget" | "Target.attachToBrowserTarget"
    );
    let mut out = Vec::new();
    let mut events = std::mem::take(&mut ctx.pending_events);
    let push_events = |out: &mut Vec<String>, events: &mut Vec<crate::cdp::types::CdpEvent>| {
        for ev in events.drain(..) {
            if let Ok(line) = serde_json::to_string(&ev) {
                out.push(line);
            }
        }
    };
    if events_first {
        push_events(&mut out, &mut events);
        if let Ok(line) = serde_json::to_string(response) {
            out.push(line);
        }
    } else {
        if let Ok(line) = serde_json::to_string(response) {
            out.push(line);
        }
        push_events(&mut out, &mut events);
    }
    for line in out {
        if sender.send(Message::Text(line)).await.is_err() {
            return false;
        }
    }
    true
}

/// Flush whatever events dispatch (or the navigation tail) queued onto the
/// socket. Returns false once the socket is gone.
async fn send_events(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ctx: &mut CdpContext,
) -> bool {
    let events = std::mem::take(&mut ctx.pending_events);
    for ev in events {
        if let Ok(line) = serde_json::to_string(&ev) {
            if sender.send(Message::Text(line)).await.is_err() {
                return false;
            }
        }
    }
    true
}

async fn connection_loop(socket: WebSocket, page_mode: bool) {
    let (mut sender, mut receiver) = socket.split();
    // Each connection is a fresh browser: own cookie jar, own HTTP client,
    // own page set. Stealth/proxy follow the process env like the HTTP API.
    let proxy = crate::config::proxy_from_env();
    let stealth = !matches!(
        std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(),
        Some("0")
    );
    let mut ctx = CdpContext::new_with_options(proxy, stealth);

    if page_mode {
        // The page the client attached to: created up front, registered under
        // its session id, and installed as the sessionless-command fallback.
        let page_id = ctx
            .create_page_in_context(None)
            .expect("default browser context must exist");
        if let Some(page) = ctx.get_page_mut(&page_id) {
            page.navigate_blank();
        }
        ctx.sessions
            .insert(format!("{page_id}-session"), page_id.clone());
        ctx.default_page = Some(page_id);
    }

    // Spawned-navigation reporting: `Page.navigate` on this connection hands
    // the page to a `spawn_local` task and resolves at commit; the task
    // returns the page here when the load settles. futures' unbounded channel
    // (not tokio's) because `Page` is `!Send`.
    let (load_tx, mut load_rx) = futures::channel::mpsc::unbounded::<LoadDone>();
    ctx.load_tx = Some(load_tx);

    // Screencast frame pump: 33 ms cadence (~30fps ceiling), skipped while
    // no page has an armed subscription (the guard keeps idle connections at
    // zero timer wakeups) and `Skip` keeps a slow frame from stampeding
    // ticks. Frames are produced on this thread — Page is !Send.
    let mut pump = tokio::time::interval(std::time::Duration::from_millis(33));
    pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    #[cfg(feature = "screenshot")]
    let mut has_screencast;
    #[cfg(not(feature = "screenshot"))]
    let has_screencast = false;

    // Idle event-loop pump: nothing else polls page JS between commands, so
    // timers and promise continuations would never fire while the client is
    // quiet — an SPA that self-navigates on a timer would wedge forever
    // (obscura#866). Same pump the session face runs in its own loop; 200 ms
    // cadence with `Skip` so a long pump doesn't stampede ticks.
    let mut idle_pump = tokio::time::interval(std::time::Duration::from_millis(200));
    idle_pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        #[cfg(feature = "screenshot")]
        {
            has_screencast = !ctx.screencast.is_empty();
        }
        tokio::select! {
            msg = receiver.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::debug!("CDP: socket error: {e}");
                        break;
                    }
                    None => break,
                };
                match msg {
                    Message::Text(text) => {
                        let req: CdpRequest = match serde_json::from_str(text.as_str()) {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!("CDP: unparseable request: {e}");
                                continue;
                            }
                        };
                        // A navigation is in flight for this page: park the
                        // command and replay it (in order) when the load
                        // lands. enable/disable always pass through.
                        if ctx.should_park(&req) {
                            ctx.deferred.push(req);
                            continue;
                        }
                        let response = dispatch::dispatch(&req, &mut ctx).await;
                        if !send_out(&mut sender, &response, &req.method, &mut ctx).await {
                            return;
                        }
                    }
                    Message::Ping(payload) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Pong(_) => {}
                }
            }
            _ = pump.tick(), if has_screencast => {
                #[cfg(feature = "screenshot")]
                {
                    crate::cdp::domains::page::pump_screencast_frames(&mut ctx).await;
                    if !send_events(&mut sender, &mut ctx).await {
                        return;
                    }
                }
            }
            // A spawned navigation settled: put the page back, emit its
            // lifecycle events (or Chrome's failed-navigation shape), then
            // replay whatever parked on the load — parking-aware, since
            // another page's navigation may still be in flight.
            done = load_rx.next(), if !ctx.pending_loads.is_empty() => {
                let Some(done) = done else { break };
                let Some(pending) = ctx.pending_loads.remove(&done.page_id) else {
                    continue;
                };
                ctx.pages.push(done.page);
                let error = done.result.as_ref().err().cloned();
                emit_navigation_tail(&mut ctx, &pending, &done.page_id, error.as_deref());
                if !send_events(&mut sender, &mut ctx).await {
                    return;
                }
                let deferred = std::mem::take(&mut ctx.deferred);
                for req in deferred {
                    if ctx.should_park(&req) {
                        ctx.deferred.push(req);
                        continue;
                    }
                    let response = dispatch::dispatch(&req, &mut ctx).await;
                    if !send_out(&mut sender, &response, &req.method, &mut ctx).await {
                        return;
                    }
                }
            }
            // No command in flight: still give page JS its slices and adopt
            // any self-navigation, so timers fire while the client is quiet.
            _ = idle_pump.tick() => {
                pump_idle_pages(&mut ctx).await;
                if !send_events(&mut sender, &mut ctx).await {
                    return;
                }
            }
        }
    }
}

/// One idle pass over the connection's pages (obscura#866): advance each
/// page's JS event loop by a slice so timers, promise continuations and
/// answered-fetch continuations run even while no command arrives, then
/// adopt whatever navigation the page performed for itself — a real reload
/// queued by a timer/click, or SPA history adoption — and emit its
/// lifecycle events per attached session (sessionless channel when none).
/// Returns the ids of pages that navigated. Cancellation-safe at pump-slice
/// boundaries: the select drops this future when a command lands, and any
/// navigation queued but not yet adopted is picked up by the next
/// dispatch's post-eval nav check (pending-nav state lives in the Page).
pub(crate) async fn pump_idle_pages(ctx: &mut CdpContext) -> Vec<String> {
    let mut navigated = Vec::new();
    for i in 0..ctx.pages.len() {
        ctx.pages[i].pump_event_loop_slice(150).await;
        match ctx.pages[i].process_pending_navigation().await {
            Ok(true) => {
                let page_id = ctx.pages[i].id.clone();
                let sessions: Vec<String> = ctx
                    .sessions
                    .iter()
                    .filter(|(_, pid)| pid.as_str() == page_id)
                    .map(|(sid, _)| sid.clone())
                    .collect();
                if sessions.is_empty() {
                    emit_navigation_for_page(ctx, &None, &[], &page_id);
                } else {
                    // Per-session enumeration: each session already gets the
                    // full sequence here, so no extra Network fan-out targets
                    // (double delivery otherwise).
                    for sid in sessions {
                        emit_navigation_for_page(ctx, &Some(sid), &[], &page_id);
                    }
                }
                navigated.push(page_id);
            }
            Ok(false) => {}
            Err(e) => tracing::debug!("CDP: idle-pump navigation failed: {e}"),
        }
    }
    dispatch::drain_and_settle(ctx).await;
    navigated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::types::CdpRequest;

    /// Provision a page-mode connection's default page and load a real
    /// document, mirroring what connection_loop does before any command.
    async fn setup(ctx: &mut CdpContext, body: &str) -> String {
        let page_id = ctx
            .create_page_in_context(None)
            .expect("default browser context must exist");
        let session_id = format!("{page_id}-session");
        ctx.sessions.insert(session_id.clone(), page_id.clone());
        ctx.default_page = Some(page_id.clone());
        let nav = CdpRequest {
            id: 1,
            method: "Page.navigate".to_string(),
            params: json!({ "url": format!("data:text/html,<html><body>{body}</body></html>") }),
            session_id: Some(session_id.clone()),
        };
        let resp = dispatch::dispatch(&nav, ctx).await;
        assert!(resp.error.is_none(), "navigate failed: {:?}", resp.error);
        session_id
    }

    async fn evaluate(
        ctx: &mut CdpContext,
        session_id: &str,
        expression: &str,
    ) -> serde_json::Value {
        let req = CdpRequest {
            id: 2,
            method: "Runtime.evaluate".to_string(),
            params: json!({ "expression": expression, "returnByValue": true }),
            session_id: Some(session_id.to_string()),
        };
        let resp = dispatch::dispatch(&req, ctx).await;
        assert!(resp.error.is_none(), "evaluate failed: {:?}", resp.error);
        resp.result.expect("result")["result"]["value"].clone()
    }

    /// obscura#866: a timer scheduled by page JS must fire while no CDP
    /// command arrives — the idle pump advances the event loop between
    /// commands. Before the pump existed, `__t` stayed 0 forever on a quiet
    /// connection and any SPA parked on a timer wedged.
    #[tokio::test(flavor = "current_thread")]
    async fn idle_pump_fires_timers_between_commands() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let sid = setup(&mut ctx, "<p>probe</p>").await;

        evaluate(
            &mut ctx,
            &sid,
            "globalThis.__t = 0; setTimeout(() => { globalThis.__t = 1; }, 30);",
        )
        .await;
        let before = evaluate(&mut ctx, &sid, "globalThis.__t").await;
        assert_eq!(before, json!(0), "sanity: commands alone never pump timers");
        pump_idle_pages(&mut ctx).await;
        let after = evaluate(&mut ctx, &sid, "globalThis.__t").await;
        assert_eq!(after, json!(1), "timer must fire via the idle pump");
    }

    /// The Maps symptom from obscura#866: a page that navigates itself from a
    /// timer gets the navigation adopted by the idle pump — real reload,
    /// lifecycle events scoped to the attached session.
    #[tokio::test(flavor = "current_thread")]
    async fn idle_pump_adopts_timer_driven_self_navigation() {
        let mut ctx = CdpContext::new_with_options(None, false);
        let sid = setup(&mut ctx, "<p>origin</p>").await;
        let target = "data:text/html,<html><body><h1>landed</h1></body></html>";
        evaluate(
            &mut ctx,
            &sid,
            &format!("setTimeout(() => {{ location.href = {:?}; }}, 30);", target),
        )
        .await;

        let page_id = ctx.default_page.clone().expect("provisioned page");
        let navigated = pump_idle_pages(&mut ctx).await;
        assert_eq!(navigated, vec![page_id.clone()], "self-navigation adopted");

        let page = ctx.get_page(&page_id).expect("page");
        assert_eq!(page.url_string(), target);

        let methods: Vec<&str> = ctx
            .pending_events
            .iter()
            .map(|e| e.method.as_str())
            .collect();
        assert!(
            methods.contains(&"Page.frameNavigated"),
            "lifecycle events emitted: {methods:?}"
        );
        assert!(
            methods.contains(&"Page.loadEventFired"),
            "load lifecycle emitted: {methods:?}"
        );
        let nav_ev = ctx
            .pending_events
            .iter()
            .find(|e| e.method == "Page.frameNavigated")
            .expect("frameNavigated");
        assert_eq!(
            nav_ev.session_id.as_deref(),
            Some(sid.as_str()),
            "scoped to the attached session"
        );
    }
}
