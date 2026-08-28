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

use crate::diting_cdp::dispatch::{self, CdpContext};
use crate::diting_cdp::types::CdpRequest;

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

/// `/json/list` — targets are per-connection, so there is no persistent target
/// registry to enumerate. Empty list; clients create targets over the browser
/// WebSocket (`Target.createTarget`).
pub async fn json_list() -> impl IntoResponse {
    axum::Json(json!([]))
}

/// WebSocket upgrade handler for `/devtools/{kind}/{id}` (`kind` = `browser`
/// or `page`). Both are treated identically: a fresh isolated context whose
/// pages are created on demand through `Target.createTarget`.
pub async fn devtools_ws(
    ws: WebSocketUpgrade,
    Path((_kind, _id)): Path<(String, String)>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        // The dispatch loop is blocking (it parks V8 on one thread), so hand
        // the socket to the blocking pool rather than pinning a Tokio worker
        // for the connection's lifetime.
        let _ = tokio::task::spawn_blocking(move || run_connection(socket));
    })
}

/// Build a current-thread runtime + `LocalSet` on the calling (blocking) thread
/// and drive the connection loop there. Every `Page` and `CdpContext` lives and
/// dies on this one thread, which is what deno_core requires.
fn run_connection(socket: WebSocket) {
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
    local.block_on(&rt, connection_loop(socket));
}

async fn connection_loop(socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    // Each connection is a fresh browser: own cookie jar, own HTTP client,
    // own page set. Stealth/proxy follow the process env like the HTTP API.
    let proxy = crate::config::proxy_from_env();
    let stealth = !matches!(
        std::env::var("AGINXBROWSER_STEALTH").ok().as_deref(),
        Some("0")
    );
    let mut ctx = CdpContext::new_with_options(proxy, stealth);

    while let Some(msg) = receiver.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("CDP: socket error: {e}");
                break;
            }
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
                let response = dispatch::dispatch(&req, &mut ctx).await;

                // Chrome emits Target.attachedToTarget BEFORE the createTarget /
                // attachToTarget response. Playwright's doCreateNewPage looks up
                // the new page in its internal _crPages map synchronously right
                // after the response resolves, and that map is only populated by
                // the attachedToTarget event — so flush the event first or the
                // lookup finds nothing and newPage() throws.
                let events_first = matches!(
                    req.method.as_str(),
                    "Target.createTarget"
                        | "Target.attachToTarget"
                        | "Target.attachToBrowserTarget"
                );

                let mut out = Vec::new();
                let mut events = std::mem::take(&mut ctx.pending_events);
                let push_events = |out: &mut Vec<String>, events: &mut Vec<crate::diting_cdp::types::CdpEvent>| {
                    for ev in events.drain(..) {
                        if let Ok(line) = serde_json::to_string(&ev) {
                            out.push(line);
                        }
                    }
                };
                if events_first {
                    push_events(&mut out, &mut events);
                    if let Ok(line) = serde_json::to_string(&response) {
                        out.push(line);
                    }
                } else {
                    if let Ok(line) = serde_json::to_string(&response) {
                        out.push(line);
                    }
                    push_events(&mut out, &mut events);
                }
                for line in out {
                    if sender.send(Message::Text(line.into())).await.is_err() {
                        return;
                    }
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
}
