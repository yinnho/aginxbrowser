use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService,
};
use serde::{Deserialize, Serialize};

mod account;
mod browser;
// CDP bridge (faces layer): /json discovery, the /devtools WebSocket face
// and the domain dispatch. Rides the engine's Page API; a product concern
// since the workspace split (ARCHITECTURE.md §4 — "CDP is a face riding
// core/engine").
mod cdp;
mod captcha;
mod config;
mod cookie;
mod curl_import;
mod docgen;
mod doctor_cli;
mod download;
mod error;
mod firecrawl_compat;
mod flow;
mod har;
mod mcp;
mod page;
mod panel;
mod panel_drm;
mod rate;
// HTTP route handlers split by surface (ARCHITECTURE.md P2 batch 2):
// acquisition (fetch/search/click/eval/download), sessions (the stateful
// actor face + flows + accounts), outputs (screenshot/video/pdf). The
// re-exports below are exactly the names other modules consume at
// `crate::` paths.
mod routers;
mod render;
mod robots;
mod sanitize;
#[cfg(feature = "screenshot")]
mod screenshot;
mod search;
mod server;
mod session;
mod store;
// Session verdict (决策层 v1): the code-only fact sheet that answers
// "where did this session land" — challenge/captcha/login/empty/landed/
// unknown from signals the engine already holds. No evals, no model.
mod verdict;
// Timeline video pump (切片层): seek `window.__timelines` frame by frame,
// paint viewport bands, pipe raw RGBA into ffmpeg — MP4 bytes out.
#[cfg(feature = "screenshot")]
mod video;
// Page pump (切片层): cut the live page into pages — print pagination at
// block boundaries, or one page per selector match — and package as PDF/PNG.
#[cfg(feature = "screenshot")]
mod pages;
// OOXML containers (容器层): image-based PPTX/DOCX packaging of the page
// set — stored-ZIP writer, zero new dependencies.
#[cfg(feature = "screenshot")]
mod ooxml;
// Native (editable) PPTX (容器层): element-level DrawingML — text runs,
// gradient shapes, image parts, with diting as the layout oracle.
#[cfg(feature = "screenshot")]
mod pptx_native;
// The Blitz reference pipeline — cross-check oracle for diting, opt-in via
// `blitz-reference`. Not compiled in production/device builds.
#[cfg(feature = "blitz-reference")]
mod screenshot_reference;
// Dual-engine cross-check tests (diting vs blitz). Lives in the product crate
// so the diting engine carries zero product/blitz references (ARCHITECTURE.md
// §2 rule R2; moved with the workspace split).
#[cfg(all(test, feature = "blitz-reference"))]
mod bridge_cross_check;
// Env-knob test guards shared by this crate's single test binary (see the
// module docs for why the engine's own guards can't be reused).
#[cfg(test)]
mod test_support;
mod tmpl;

pub use routers::acquisition::{
    ClickRequest, ClickResponse, EvalRequest, EvalResponse, FetchRequest, FetchResponse,
    JsExtractConfig, OutputFormat, RenderTier, SearchRequest, SearchResponse, SearchResultItem,
};
#[cfg(feature = "screenshot")]
pub use routers::outputs::{
    PdfRequest, PdfResponse, ScreenshotRequest, ScreenshotResponse, VideoAudioRequest,
    VideoNarrationClip, VideoRequest, VideoResponse,
};
pub use routers::sessions::SessionCreateRequest;

use routers::acquisition::{
    click_handler, download_handler, engines_handler, eval_handler, fetch_handler, search_handler,
    search_engine_rows,
};
#[cfg(feature = "screenshot")]
use routers::outputs::{pdf_handler, screenshot_handler, video_handler};
use routers::sessions::{
    account_delete_handler, account_login_handler, account_verify_handler, accounts_handler,
    flow_run_handler, import_curl_handler, max_body_bytes, session_challenges_handler, session_click_handler,
    session_click_xy_handler, session_clone_handler, session_close_handler,
    session_console_handler, session_cookies_handler, session_create_handler,
    session_dialog_handler, session_drag_handler, session_eval_handler, session_export_handler,
    session_har_handler, session_input_handler, session_list_handler, session_navigate_handler,
    session_network_handler, session_preload_handler, session_screenshot_handler, session_scroll_handler,
    session_set_files_handler, session_state_handler, session_storage_handler,
    session_verdict_handler, session_viewport_handler, session_wait_handler, sessions_handler,
};


use server::do_fetch;

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub enum AppError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    PayloadTooLarge(String),
    TooManyRequests(String),
    BadGateway(String),
    GatewayTimeout(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg),
            AppError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
            AppError::GatewayTimeout(msg) => (StatusCode::GATEWAY_TIMEOUT, msg),
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(ErrorResponse { error: message })).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        let e = err.into();
        let msg = e.to_string();
        if msg.starts_with("rate limit:") {
            // Stance gate (crate::rate) must surface as 429, not the generic
            // catch-all — the status IS part of the message.
            AppError::TooManyRequests(msg)
        } else if msg.contains("timeout") || msg.contains("timed out") {
            AppError::GatewayTimeout(msg)
        } else if msg.contains("resolve") || msg.contains("connect") || msg.contains("dns") {
            AppError::BadGateway(msg)
        } else if msg.contains("selector") || msg.contains("parse") {
            AppError::BadRequest(msg)
        } else {
            AppError::Internal(msg)
        }
    }
}

/// `--cdp-port N`: bind the whole surface on loopback at N — the local
/// agent-tooling entry (agent-browser `--cdp`, Playwright `connectOverCDP`).
/// Wins over `AGINXBROWSER_BIND` (an explicit launch flag beats ambient env);
/// wider or non-loopback binds still go through the env.
fn cdp_port_from_args(args: &[String]) -> Result<Option<u16>, String> {
    let Some(pos) = args.iter().position(|a| a == "--cdp-port") else {
        return Ok(None);
    };
    let raw = args
        .get(pos + 1)
        .ok_or_else(|| "--cdp-port requires a port number".to_string())?;
    let port: u16 = raw
        .parse()
        .map_err(|_| format!("--cdp-port: {raw:?} is not a valid port"))?;
    if port == 0 {
        // OS-assigned ports are useless here — the caller must already know
        // the port to connect, so refuse instead of starting unreachable.
        return Err("--cdp-port: port 0 is OS-assigned; pick a fixed port".to_string());
    }
    Ok(Some(port))
}

/// Flags that consume the following token as their value — the token after
/// one of these is never a flag position.
const VALUE_FLAGS: &[&str] = &["--cdp-port", "--allow-network", "--font-dir"];

/// First unrecognized flag-shaped argument, if any. `--port` and friends
/// must refuse instead of silently starting a misconfigured server.
fn first_unknown_flag(args: &[String]) -> Option<String> {
    const KNOWN: &[&str] = &[
        "--mcp",
        "--allow-file-access",
        "--allow-private-network",
        "--panel",
        "--version",
        "-V",
        "--help",
        "-h",
    ];
    let mut skip_next = false;
    for a in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if VALUE_FLAGS.contains(&a.as_str()) {
            skip_next = true;
            continue;
        }
        if KNOWN.contains(&a.as_str()) {
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            return Some(a.clone());
        }
    }
    None
}

const CLI_HELP: &str = "\
aginxbrowser — agent-owned browser face (HTTP + MCP stdio)

USAGE:
    aginxbrowser [FLAGS]
    aginxbrowser doctor

SUBCOMMAND:
    doctor                    diagnose this box (env knobs, net, fonts), then exit

FLAGS:
    --mcp                     serve MCP over stdio instead of HTTP
    --cdp-port <PORT>         bind the whole surface on loopback at PORT
    --allow-private-network   permit fetches to private/loopback ranges
    --allow-file-access       permit file:// fetches
    --allow-network <CIDRS>   comma-separated allowed network ranges
    --font-dir <DIR>          extra font fallback directory (screenshot builds)
    --panel                   paint straight to the phone's DRM glass (screenshot builds)
    --version, -V             print version and exit
    --help, -h                print this help and exit

Bind address comes from AGINXBROWSER_BIND (default 0.0.0.0:8089); --cdp-port
pins loopback:PORT and wins over the env. Run `aginxbrowser doctor` for the
full environment-knob report.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // In --mcp (stdio transport) mode, stdout IS the JSON-RPC channel —
    // tracing lines there break strict clients on the first non-JSON
    // output. Route logs to stderr for that mode only; the HTTP server
    // keeps stdout (container conventions expect logs there).
    let mcp_stdio_mode = std::env::args().any(|a| a == "--mcp");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "aginxbrowser=info,diting::diting_browser::page=warn,diting::diting_net::wreq_client=warn,diting::console=error",
                    )
                }),
        );
    if mcp_stdio_mode {
        subscriber.with_writer(std::io::stderr).init();
    } else {
        subscriber.init();
    }

    // CLI subcommands exit before the server boots — doctor especially must
    // not pay the V8 warmup below (self-hosters run it to debug a box that
    // may not even reach the network).
    let args: Vec<String> = std::env::args().collect();

    // Metadata flags exit before anything boots — before #98 these were
    // silently ignored and `aginxbrowser --version` left a listening engine
    // behind (found live on port 8089).
    if args.iter().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("aginxbrowser {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        println!("{CLI_HELP}");
        return Ok(());
    }

    if args.get(1).map(String::as_str) == Some("doctor") {
        std::process::exit(doctor_cli::run().await);
    }

    // A typo'd or stale recipe (the historical `--port` never bound anything)
    // must not start a silently-misconfigured server.
    if let Some(bad) = first_unknown_flag(&args) {
        eprintln!("aginxbrowser: unrecognized argument {bad:?} (see --help)");
        std::process::exit(2);
    }

    // UA/TLS coherence (taobao compat report ⑤): AGINXBROWSER_UA overrides
    // the UA while the stealth transport keeps its default Chrome145
    // handshake — say so loudly instead of shipping a WAF tell silently.
    #[cfg(feature = "stealth")]
    if let Ok(ua) = std::env::var("AGINXBROWSER_UA") {
        diting::diting_net::warn_on_ua_tls_mismatch(&ua, None);
    }

    // Warm up V8 on the main thread before any session/blocking thread creates
    // an isolate: the first isolate's JSDispatchTable init is not safe to race
    // from several threads (upstream obscura #430; construction itself is
    // serialized inside the runtime).
    std::mem::drop(diting::diting_js::runtime::JsRuntime::new());

    // Opt-in relaxations, parsed before any mode branches so the HTTP server
    // and MCP stdio both see them. Both were previously documented as CLI
    // flags without wiring (only the env vars worked) — issue #33 and
    // requirements-aginxos P2.
    let cdp_port = cdp_port_from_args(&args).map_err(|e| anyhow::anyhow!(e))?;
    if args.contains(&"--allow-file-access".to_string()) {
        diting::diting_net::client::set_allow_file_access(true);
        tracing::info!("file:// access enabled (--allow-file-access)");
    }
    if args.contains(&"--allow-private-network".to_string()) {
        diting::diting_net::client::set_allow_private_network(true);
        tracing::info!("private-network fetch enabled (--allow-private-network)");
    }
    if let Some(pos) = args.iter().position(|a| a == "--allow-network") {
        let spec = args.get(pos + 1).map(|s| s.as_str()).unwrap_or("");
        diting::diting_net::client::set_allow_network(Some(spec));
        tracing::info!(
            "scoped allow-network list set (--allow-network): {} parsed entries",
            diting::diting_net::client::parse_scoped_cidrs(spec).len()
        );
    }
    // The font supply itself is screenshot-gated (diting_layout renders with
    // it); a bare server build has no faces to point at.
    #[cfg(feature = "screenshot")]
    if let Some(pos) = args.iter().position(|a| a == "--font-dir") {
        let dir = args.get(pos + 1).map(|s| s.as_str());
        diting::diting_fonts::set_font_dir(dir);
        tracing::info!("font dir fallbacks (--font-dir): {:?}", dir.unwrap_or("(none)"));
    }

    // The phone's glass is this browser's viewport. --panel, or the
    // marker file the device drops, paints straight to DRM instead of
    // handing JPEG frames to another process to paste.
    #[cfg(feature = "screenshot")]
    if args.iter().any(|a| a == "--panel")
        || std::path::Path::new("/etc/aginx/panel.on").exists()
    {
        panel::start();
    }

    // Check if running in MCP mode
    if args.contains(&"--mcp".to_string()) {
        tracing::info!("Starting in MCP mode");
        mcp::run_mcp_stdio()
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;
        return Ok(());
    }

    let app = Router::new()
        .route("/", get(status_handler))
        .route("/status", get(status_handler))
        .route("/health", get(health_handler))
        .route("/doctor", get(doctor_handler))
        // Human-takeover live view — embedded so a bare local binary serves it
        // (the hosted deployment also fronts it via nginx; same file). The
        // challenge handoff in session punish detection points here.
        .route("/live", get(live_handler))
        .route("/live.html", get(live_handler))
        .route("/open", post(open_handler))
        .route("/fetch", post(fetch_handler))
        .route("/click", post(click_handler))
        .route("/eval", post(eval_handler))
        .route("/search", post(search_handler))
        .route("/engines", get(engines_handler))
        .route("/download", post(download_handler))
        .route("/v1/scrape", post(firecrawl_compat::scrape_handler))
        .route("/session/create", post(session_create_handler))
        .route("/flow/run", post(flow_run_handler))
        .route("/session/:id/clone", post(session_clone_handler))
        .route("/import/curl", post(import_curl_handler))
        .route("/session/list", get(session_list_handler))
        .route("/session/:id/navigate", post(session_navigate_handler))
        .route("/session/:id/preload", post(session_preload_handler))
        .route("/session/:id/state", post(session_state_handler))
        .route("/session/:id/cookies", get(session_cookies_handler))
        .route("/session/:id/storage", get(session_storage_handler))
        .route("/session/:id/console", get(session_console_handler))
        .route("/session/:id/dialog", post(session_dialog_handler))
        .route("/session/:id/export", get(session_export_handler))
        .route("/session/:id/network", get(session_network_handler))
        .route("/session/:id/challenges", get(session_challenges_handler))
        .route("/session/:id/verdict", get(session_verdict_handler))
        .route("/session/:id/har", get(session_har_handler))
        .route("/session/:id/click", post(session_click_handler))
        .route("/session/:id/click_xy", post(session_click_xy_handler))
        .route("/session/:id/drag", post(session_drag_handler))
        .route("/session/:id/input", post(session_input_handler))
        // File content rides in the JSON body as base64 — the axum default
        // 2 MiB cap would reject real product images outright.
        .route(
            "/session/:id/files",
            post(session_set_files_handler).layer(axum::extract::DefaultBodyLimit::max(max_body_bytes())),
        )
        .route("/session/:id/scroll", post(session_scroll_handler))
        .route("/session/:id/viewport", post(session_viewport_handler))
        .route("/session/:id/screenshot", post(session_screenshot_handler))
        .route("/session/:id/wait", post(session_wait_handler))
        .route(
            "/session/:id/eval",
            post(session_eval_handler)
                .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes())),
        )
        .route("/sessions", get(sessions_handler))
        .route("/session/:id/close", post(session_close_handler))
        .route("/accounts", get(accounts_handler))
        .route("/accounts/:name", delete(account_delete_handler))
        .route("/account/verify", post(account_verify_handler))
        .route("/account/login", post(account_login_handler))
        .route("/mcp", get(mcp_handler).post(mcp_handler))
        // CDP bridge — Playwright connectOverCDP / Puppeteer connect surface.
        .route("/json/version", get(crate::cdp::http::json_version))
        .route("/json/version/", get(crate::cdp::http::json_version))
        .route("/json", get(crate::cdp::http::json_list))
        .route("/json/", get(crate::cdp::http::json_list))
        .route("/json/list", get(crate::cdp::http::json_list))
        .route("/json/list/", get(crate::cdp::http::json_list))
        .route("/json/unimplemented", get(crate::cdp::http::json_unimplemented))
        .route("/devtools/:kind/:id", get(crate::cdp::http::devtools_ws));

    #[cfg(feature = "screenshot")]
    let app = app
        .route(
            "/screenshot",
            post(screenshot_handler).layer(axum::extract::DefaultBodyLimit::max(max_body_bytes())),
        )
        .route(
            "/video",
            post(video_handler).layer(axum::extract::DefaultBodyLimit::max(max_body_bytes())),
        )
        .route(
            "/pdf",
            post(pdf_handler).layer(axum::extract::DefaultBodyLimit::max(max_body_bytes())),
        );

    let bind_addr = if let Some(port) = cdp_port {
        format!("127.0.0.1:{port}")
    } else {
        std::env::var("AGINXBROWSER_BIND").unwrap_or_else(|_| "0.0.0.0:8089".to_string())
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("aginxbrowser listening on {}", listener.local_addr()?);
    if let Some(port) = cdp_port {
        tracing::info!(
            "CDP on loopback — agent-browser: `agent-browser --cdp {port}` · \
             Playwright: connectOverCDP(\"http://127.0.0.1:{port}\")"
        );
    }

    // Standard proxy env vars do NOT configure this engine (reqwest/wreq's
    // implicit env matcher is pinned off on every engine client); make that
    // visible once at startup instead of letting a shell proxy look
    // "configured" while fetches go direct (obscura#491).
    if config::proxy_from_env().is_none() {
        if let Some(env) = config::standard_proxy_env() {
            tracing::warn!(
                "{env} is set but ignored — set AGINXBROWSER_PROXY to route engine traffic through a proxy"
            );
        }
    }

    // Periodically persist the process-global shared cookie jar (stateless
    // handlers mutate it in place; a crash shouldn't cost returning-client
    // cookies that keep anti-bot CAPTCHA rates down).
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            server::persist_shared_cookies();
        }
    });

    axum::serve(listener, app.with_state(mcp::mcp_http_service())).await?;
    Ok(())
}

/// 调用方交 JSON 和模板名。浏览器用自己文件夹里的模板生成 HTML，并交给面板显示。
/// 失败是结构化的：unknown_template 返 404 并带 known 清单——母体据此走
/// 「安排模型写一次模板并登记」，不用解析自由文本猜原因。
async fn open_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let template = body
        .get("template")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if template.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "missing_template"})),
        );
    }
    let data = body.get("data").cloned().unwrap_or(serde_json::json!({}));
    match tmpl::open(&template, &data) {
        Ok(n) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "template": template,
                "bytes": n,
            })),
        ),
        Err(e) => {
            let status = match &e {
                tmpl::OpenError::UnknownTemplate { .. } => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            let mut body = serde_json::json!({
                "ok": false,
                "error": e.code(),
                "why": e.to_string(),
            });
            if let tmpl::OpenError::UnknownTemplate { id, known } = &e {
                body["template"] = serde_json::json!(id);
                body["known"] = serde_json::json!(known);
            }
            (status, Json(body))
        }
    }
}

async fn health_handler() -> impl IntoResponse {
    Json(health_body())
}

/// The /health body, split out so tests can assert on it directly.
///
/// Beyond liveness this carries the build identity — version, source commit,
/// the UA browser traffic presents, the default TLS fingerprint — so "which
/// binary am I talking to and what is it showing sites" is one cheap call
/// instead of a traffic sniff (the 0.3.0 tmall report hit exactly that gap:
/// the UA a session carried didn't match the docs and nothing on the box
/// could say why — it was an imported session keeping the copied request's
/// own Chrome UA, by design, but /health couldn't answer either way).
fn health_body() -> serde_json::Value {
    // The UA the browser paths carry: the AGINXBROWSER_UA override, else the
    // pinned persona (see BrowserContext's resolution chain — search-engine
    // transports keep their own defaults).
    let ua = std::env::var("AGINXBROWSER_UA").unwrap_or_else(|_| {
        diting::diting_browser::profiles::select_profile()
            .user_agent
            .to_string()
    });
    #[cfg(feature = "stealth")]
    let tls = diting::diting_net::DEFAULT_TLS_FINGERPRINT;
    #[cfg(not(feature = "stealth"))]
    let tls = "off";
    serde_json::json!({
        "status": "ok",
        "engine": "diting",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": option_env!("AGINXBROWSER_BUILD_COMMIT").unwrap_or("unknown"),
        "ua": ua,
        "tls": tls,
        "capabilities": {
            "screenshot": cfg!(feature = "screenshot"),
            "stealth": cfg!(feature = "stealth"),
            "captcha_solver": std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        }
    })
}

/// Process start, for the status page's uptime readout.
static STARTED: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn fmt_uptime(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (days, rest) = (secs / 86400, secs % 86400);
    let (hours, mins) = (rest / 3600, (rest % 3600) / 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) => format!("{hours}h {mins}m"),
        _ => format!("{days}d {hours}h {mins}m"),
    }
}

/// Human-takeover live view, embedded at compile time — a bare local binary
/// serves it without nginx (the hosted deployment fronts the same file).
/// `/session/:id/challenges` punish detection points a human here: clicks,
/// drags and typing land on the real session page, so a wall (taobao slider
/// etc.) can be solved in the very session the agent owns.
async fn live_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../web/live.html"))
}

/// Human-facing status page at `/`. Umbrel (and any self-hoster poking the
/// port) needs a page the browser can open after install; agents keep using
/// /health and /doctor. Everything is server-rendered — no client JS, no
/// external assets, works on an offline LAN box.
async fn status_handler() -> axum::response::Html<String> {
    let (sessions, uptime) = {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        (mgr.session_count(), STARTED.elapsed())
    };
    let version = env!("CARGO_PKG_VERSION");
    let caps = [
        ("screenshot", cfg!(feature = "screenshot")),
        ("stealth", cfg!(feature = "stealth")),
        (
            "captcha-solver",
            std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        ),
    ]
    .map(|(name, on)| {
        if on {
            format!(r#"<span class="cap on">{name}</span>"#)
        } else {
            format!(r#"<span class="cap">{name}</span>"#)
        }
    })
    .join("");

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aginxbrowser</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font: 16px/1.6 ui-sans-serif, system-ui, sans-serif; margin: 0;
         display: flex; justify-content: center; min-height: 100vh;
         background: Canvas; color: CanvasText; }}
  main {{ max-width: 40rem; padding: 3rem 1.5rem 4rem; }}
  h1 {{ font-size: 1.4rem; margin: 0 0 .25rem; }}
  p.tag {{ margin: 0 0 2rem; opacity: .7; }}
  .ok {{ color: #1a7f37; }}
  dl {{ display: grid; grid-template-columns: max-content 1fr; gap: .4rem 1.5rem;
        margin: 0 0 2rem; }}
  dt {{ opacity: .7; }}
  dd {{ margin: 0; font-variant-numeric: tabular-nums; }}
  .cap {{ display: inline-block; border: 1px solid; border-radius: 1em;
          padding: .05rem .7rem; margin: 0 .35rem .35rem 0; opacity: .55; }}
  .cap.on {{ opacity: 1; border-color: #1a7f37; color: #1a7f37; }}
  table {{ border-collapse: collapse; width: 100%; font: .9rem/1.5 ui-monospace, monospace; }}
  td {{ padding: .3rem .6rem .3rem 0; vertical-align: top; }}
  td:first-child {{ opacity: .7; white-space: nowrap; }}
  footer {{ margin-top: 2.5rem; font-size: .85rem; opacity: .7; }}
  a {{ color: inherit; }}
</style>
</head>
<body>
<main>
  <h1>aginxbrowser <small style="font-weight:400">v{version}</small></h1>
  <p class="tag">server-side browser for AI agents &mdash; one Rust binary, no Chromium</p>

  <dl>
    <dt>status</dt><dd><span class="ok">&#9679; running</span></dd>
    <dt>uptime</dt><dd>{}</dd>
    <dt>active sessions</dt><dd>{sessions}</dd>
    <dt>engine</dt><dd>diting</dd>
  </dl>

  <p style="margin-bottom:.5rem">capabilities</p>
  <p style="margin:0 0 2rem">{caps}</p>

  <table>
    <tr><td>GET&nbsp;&nbsp;/health</td><td>liveness + capabilities (JSON)</td></tr>
    <tr><td>GET&nbsp;&nbsp;/doctor</td><td>deep self-report, <code>?probe=true</code> for a live fetch</td></tr>
    <tr><td>GET&nbsp;&nbsp;/engines</td><td>search engines: names, categories, suspension state</td></tr>
    <tr><td>POST&nbsp;/fetch</td><td>fetch a URL, render JS, return markdown/HTML</td></tr>
    <tr><td>POST&nbsp;/search</td><td>multi-engine meta-search</td></tr>
    <tr><td>POST&nbsp;/screenshot</td><td>render a page to PNG (CPU)</td></tr>
    <tr><td>POST&nbsp;/video</td><td>page timelines &rarr; MP4 (needs ffmpeg)</td></tr>
    <tr><td>POST&nbsp;/pdf</td><td>page &rarr; paginated PDF / PNGs</td></tr>
    <tr><td>POST&nbsp;/download</td><td>streaming file download</td></tr>
    <tr><td>POST&nbsp;/session/:id/&hellip;</td><td>stateful browser session (navigate/click/eval/&hellip;)</td></tr>
    <tr><td>GET&nbsp;&nbsp;/mcp</td><td>MCP endpoint (streamable HTTP)</td></tr>
  </table>

  <footer>
    <a href="https://github.com/yinnho/aginxbrowser">github.com/yinnho/aginxbrowser</a>
    &middot; <a href="https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md">API reference</a>
    &middot; Apache-2.0
  </footer>
</main>
</body>
</html>"#,
        fmt_uptime(uptime)
    );

    axum::response::Html(html)
}

/// Query params for /doctor.
#[derive(Deserialize)]
struct DoctorParams {
    /// `?probe=true` runs a live micro-fetch (proves the fetch pipeline +
    /// network egress actually work, not just that the binary is up). Off by
    /// default - a probe spins up a browser and hits the network, so it's
    /// opt-in (borrowing agent-reach's lesson: "shutil.which() alone is NOT
    /// proof of health - really execute a lightweight command").
    #[serde(default)]
    probe: Option<bool>,
}

/// Deep capability self-report + optional live probe. Agents should call this
/// (not /health) when they want to know which features are usable before
/// relying on them.
async fn doctor_handler(Query(params): Query<DoctorParams>) -> impl IntoResponse {
    let capabilities = serde_json::json!({
        "screenshot": cfg!(feature = "screenshot"),
        "stealth": cfg!(feature = "stealth"),
        "captcha_solver": std::env::var("CAPTCHA_SOLVER_API_KEY").is_ok(),
        // The product stance, visible where agents and operators look first.
        "robots_honored": std::env::var("AGINXBROWSER_HONOR_ROBOTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    });

    // Live engine health, not a static list: an agent deciding between
    // engines wants to know who is benched by a CAPTCHA right now.
    let search_engines = search_engine_rows().await;

    let probe = if params.probe.unwrap_or(false) {
        let probe_url = std::env::var("AGINXBROWSER_DOCTOR_URL")
            .unwrap_or_else(|_| "https://example.com".to_string());
        let req = FetchRequest {
            url: probe_url.clone(),
            format: OutputFormat::Markdown,
            selector: None,
            wait_secs: None,
            use_proxy: false,
            cookies: vec![],
            max_chars: 500,
            auto_bypass_challenge: true,
            render_tier: RenderTier::Auto,
            tls_fingerprint: None,
            js_extract: None,
            sanitize: true,
            capture_xhr: None,
        };
        // do_fetch drives the real fetch pipeline (build_browser -> goto ->
        // extract) on a local runtime; spawn_blocking because it is !Send and
        // cannot run inside the tokio runtime - same pattern as the MCP tools.
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || do_fetch(req)).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(Ok(resp)) => serde_json::json!({
                "url": probe_url,
                "ok": true,
                "latency_ms": latency_ms,
                "title": resp.title,
                "content_chars": resp.content.chars().count(),
            }),
            Ok(Err(e)) => serde_json::json!({
                "url": probe_url,
                "ok": false,
                "latency_ms": latency_ms,
                "error": format!("{:?}", e),
            }),
            Err(e) => serde_json::json!({
                "url": probe_url,
                "ok": false,
                "latency_ms": latency_ms,
                "error": format!("task panicked: {}", e),
            }),
        }
    } else {
        serde_json::Value::Null
    };

    Json(serde_json::json!({
        "status": "ok",
        "engine": "diting",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": capabilities,
        "search_engines": search_engines,
        "endpoints": [
            "/health", "/doctor", "/engines", "/fetch", "/click", "/eval",
            "/search", "/download", "/v1/scrape", "/session/create",
            "/session/list", "/import/curl", "/mcp"
        ],
        "probe": probe,
    }))
}

async fn mcp_handler(
    State(service): State<StreamableHttpService<mcp::AginxBrowserMcp, LocalSessionManager>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = service.handle(req).await.into_parts();
    // rmcp's body error is Infallible (never produced); coerce to an Error type.
    use http_body_util::BodyExt;
    let body = axum::body::Body::new(body.map_err(|never| -> std::io::Error { match never {} }));
    Response::from_parts(parts, body)
}

/// Lazy-initialized TTL read from env (parsed once, then cached). Shared by
/// the /fetch and /search caches.
pub(crate) fn cache_ttl_secs() -> u64 {
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("AGINXBROWSER_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600)
    })
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --cdp-port: the local agent-tooling entry. Valid port wins, garbage
    // and port 0 refuse loudly instead of silently starting unreachable.
    #[test]
    fn cdp_port_flag_parses_and_validates() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(cdp_port_from_args(&a(&["aginxbrowser"])).unwrap(), None);
        assert_eq!(
            cdp_port_from_args(&a(&["aginxbrowser", "--cdp-port", "9223"])).unwrap(),
            Some(9223)
        );
        assert!(cdp_port_from_args(&a(&["aginxbrowser", "--cdp-port"])).is_err());
        assert!(cdp_port_from_args(&a(&["aginxbrowser", "--cdp-port", "abc"])).is_err());
        assert!(cdp_port_from_args(&a(&["aginxbrowser", "--cdp-port", "0"])).is_err());
        assert!(cdp_port_from_args(&a(&["aginxbrowser", "--cdp-port", "99999"])).is_err());
    }

    // #98: unknown flags are named, value flags shield their argument, and
    // bare "-" (stdin convention) passes through. --version/--help exit
    // paths are too trivial to pin; the guard logic is where bugs would live.
    #[test]
    fn unknown_flag_guard_names_offenders_and_shields_values() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(first_unknown_flag(&a(&["aginxbrowser"])), None);
        assert_eq!(first_unknown_flag(&a(&["aginxbrowser", "--mcp", "--panel"])), None);
        assert_eq!(
            first_unknown_flag(&a(&["aginxbrowser", "--cdp-port", "9223"])),
            None
        );
        assert_eq!(
            first_unknown_flag(&a(&["aginxbrowser", "--port", "8089"])),
            Some("--port".to_string())
        );
        assert_eq!(
            first_unknown_flag(&a(&["aginxbrowser", "--allow-network", "10.0.0.0/8", "-x"])),
            Some("-x".to_string())
        );
        assert_eq!(first_unknown_flag(&a(&["aginxbrowser", "-"])), None);
    }

    // 0.3.0 tmall report P2: /health must answer "which build am I talking
    // to and what is it presenting" — commit, UA, TLS — in one cheap call,
    // no network probe.
    #[test]
    fn health_body_reports_build_identity_and_fingerprint() {
        let body = health_body();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        let commit = body["commit"].as_str().expect("commit present");
        assert!(
            !commit.is_empty(),
            "commit is the git short hash or 'unknown'"
        );
        let ua = body["ua"].as_str().expect("ua present");
        assert!(ua.contains("Chrome/"), "persona UA expected, got: {ua}");
        #[cfg(feature = "stealth")]
        assert_eq!(body["tls"], diting::diting_net::DEFAULT_TLS_FINGERPRINT);
        #[cfg(not(feature = "stealth"))]
        assert_eq!(body["tls"], "off");
    }
}
