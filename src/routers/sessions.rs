//! Stateful session routes (ARCHITECTURE.md P2): the /session/:id/... face
//! over the session actor, plus /sessions, /flow/run, /import/curl and the
//! named-account endpoints. Split from the crate root; behavior unchanged.
use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::session;
use crate::{account, curl_import, flow, AppError};

// ---------------------------------------------------------------------------
// Session API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SessionCreateRequest {
    /// Initial URL: creation navigates here before the session id is usable
    /// (the wizard pattern). `start_url` is honored as an alias (#115) — a
    /// caller guessing that name must not land on about:blank in silence.
    #[serde(default, alias = "start_url")]
    pub url: Option<String>,
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation. Entries are `"name=value"`
    /// strings or CDP-style objects `{"name","value","domain",...}` —
    /// browser-exported login state arrives in the object shape.
    /// Round-trips with GET /session/:id/cookies.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
    /// Web Storage to inject after the initial navigation lands:
    /// `{"local_storage": {"k":"v"}, "session_storage": {"k":"v"}}`.
    /// Round-trips with GET /session/:id/storage.
    pub storage: Option<serde_json::Value>,
    /// Idle time-to-live in seconds (default: 480, clamped 60..3600).
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// Initial viewport width in CSS pixels — pinned for the session's life.
    #[serde(default)]
    pub width: Option<u32>,
    /// Initial viewport height in CSS pixels.
    #[serde(default)]
    pub height: Option<u32>,
    /// Mobile device emulation for the initial viewport.
    #[serde(default)]
    pub mobile: bool,
    /// Exempt the session from the idle reaper (lives until close/exit).
    #[serde(default)]
    pub keepalive: bool,
    /// Persist the login state to the local store after every action; the
    /// same session id revives logged-in after idle expiry or a server
    /// restart. An explicit DELETE /session/:id drops the snapshot.
    #[serde(default)]
    pub persistent: bool,
    /// Run as a named login identity (the multi-account layer): a private
    /// cookie jar seeded from the account record, write-back to the account
    /// store after every action. `taobao-scraper` vs `taobao-publisher` —
    /// concurrent logins that never clobber each other. The account record
    /// survives the session; a later create with the same name picks up the
    /// warm jar. 1-64 chars of [a-zA-Z0-9_-].
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionCreateResponse {
    pub session_id: String,
    pub url: Option<String>,
    /// Idle budget left before auto-eviction; absent for keepalive sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    /// Echoed only for persistent sessions (the snapshot is live).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub persistent: bool,
    /// Echoed only for account sessions (named identity, private jar).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionClickRequest {
    pub index: usize,
}

#[derive(Debug, Deserialize)]
pub struct SessionInputRequest {
    pub index: usize,
    pub text: String,
    /// "full" types per-character with keydown/keypress/input/keyup cycles.
    #[serde(default)]
    pub events: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionSetFilesRequest {
    /// CSS selector for the file input — file inputs are often hidden, so the
    /// interactive index from GET /session/:id/state may not include them.
    pub selector: String,
    #[serde(default)]
    pub files: Vec<SessionFileSpec>,
}

#[derive(Debug, Deserialize)]
pub struct SessionFileSpec {
    pub name: String,
    /// File content, standard base64 (padding allowed).
    pub content_base64: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// ECMAScript time, defaults to now.
    #[serde(default)]
    pub last_modified: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct SessionScrollRequest {
    #[serde(default = "default_scroll_direction")]
    pub direction: session::ScrollDirection,
    #[serde(default = "default_scroll_amount")]
    pub amount: u32,
}

/// Request-body ceiling for the JSON-heavy routes (session eval, /screenshot,
/// /video, /pdf). Default 64 MiB, raised via AGINXBROWSER_MAX_BODY_BYTES —
/// the axum default of 2 MiB rejected base64 image payloads before the
/// script even ran (0.4.1 taobao report, problem 1). Read once at router
/// construction; eval's 413 classification re-reads it so the message always
/// names the limit actually in force.
pub(crate) fn max_body_bytes() -> usize {
    std::env::var("AGINXBROWSER_MAX_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64 * 1024 * 1024)
}

fn default_scroll_direction() -> session::ScrollDirection {
    session::ScrollDirection::Down
}

fn default_scroll_amount() -> u32 {
    3
}

#[derive(Debug, Deserialize)]
pub struct SessionEvalRequest {
    pub script: String,
    /// Await budget for the script's promise in ms (default 5000, clamped
    /// 100..120000). Slow page-side work — uploads through the page's own
    /// fetch — legitimately outlives the default; on expiry the call
    /// returns HTTP 504 with an EVAL_TIMEOUT error (the script may still
    /// be running) instead of a silent `result: null` (0.4.1 taobao
    /// report).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct SessionViewportRequest {
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub mobile: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct SessionScreenshotRequest {
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub full_page: bool,
    pub selector: Option<String>,
    #[serde(default)]
    pub selector_all: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct SessionWaitRequest {
    pub selector: Option<String>,
    pub predicate: Option<String>,
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wait_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Deserialize)]
pub struct SessionNavigateRequest {
    pub url: String,
}

// ---------------------------------------------------------------------------
// Session handlers
// ---------------------------------------------------------------------------

pub(crate) async fn session_create_handler(
    Json(req): Json<SessionCreateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account = match req.account.as_deref() {
        None => None,
        Some(name) => {
            account::validate_name(name).map_err(AppError::BadRequest)?;
            Some((crate::store::REST_OWNER.to_string(), name.to_string()))
        }
    };
    let mut mgr = session::SESSIONS.lock().await;
    mgr.evict_expired();
    let pin = match (req.width, req.height) {
        (None, None) => None,
        (w, h) => Some((w, h, req.mobile)),
    };
    let id = mgr.create(
        req.url.as_deref(),
        req.use_proxy,
        req.cookies,
        req.storage,
        req.ttl_secs,
        pin,
        req.keepalive,
        req.persistent,
        account,
    );
    let expires_in_secs = mgr.expires_in_secs(&id);
    Ok((
        StatusCode::OK,
        Json(SessionCreateResponse {
            expires_in_secs,
            session_id: id,
            url: req.url,
            persistent: req.persistent,
            account: req.account,
        }),
    ))
}

/// Credential transfer from a real browser: paste a DevTools "Copy as cURL"
/// command (any authenticated request from the Network panel) and get back a
/// session already carrying that login state. The human solves the CAPTCHA /
/// SMS once in Chrome; the agent picks up from there.
#[derive(Debug, Deserialize)]
pub struct ImportCurlRequest {
    /// The full copied cURL command (bash, PowerShell or cmd flavor).
    pub curl: String,
    /// Route the session's traffic through the engine proxy.
    #[serde(default)]
    pub use_proxy: bool,
    /// Attach the session to a named account: the imported login lands in
    /// the account's private jar and is written back under its name after
    /// every action — one import per identity, no clobbering.
    #[serde(default)]
    pub account: Option<String>,
}

pub(crate) async fn import_curl_handler(
    Json(req): Json<ImportCurlRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account = match req.account.as_deref() {
        None => None,
        Some(name) => {
            account::validate_name(name).map_err(AppError::BadRequest)?;
            Some((crate::store::REST_OWNER.to_string(), name.to_string()))
        }
    };
    let v = curl_import::create_session_from_curl(&req.curl, req.use_proxy, account)
        .await
        .map_err(AppError::BadRequest)?;
    Ok((StatusCode::OK, Json(v)))
}

// ---------------------------------------------------------------------------
// Accounts (named login identities — the multi-account layer)
// ---------------------------------------------------------------------------

/// List the caller's accounts: metadata only (name, domains, cookie count,
/// last verify verdict) — cookie values are credentials and never leave.
pub(crate) async fn accounts_handler() -> impl IntoResponse {
    let accounts = account::list(crate::store::REST_OWNER);
    axum::Json(serde_json::json!({ "count": accounts.len(), "accounts": accounts }))
}

/// Delete an account: stored record AND live jar. Sessions currently running
/// as the account keep their in-process jar handle, but with the record gone
/// their write-backs recreate nothing.
pub(crate) async fn account_delete_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !account::delete(crate::store::REST_OWNER, &name) {
        return Err(AppError::NotFound(format!("no account named {name:?}")));
    }
    Ok((StatusCode::OK, Json(serde_json::json!({ "deleted": name }))))
}

#[derive(Debug, Deserialize)]
pub struct AccountVerifyRequest {
    /// The account to check.
    pub name: String,
    /// Teach-once verify spec: the page that shows login state, and a JS
    /// expression truthy when logged in (e.g.
    /// `!!document.querySelector('.user-nick')`). Remembered after the first
    /// call; later calls can pass neither and rerun the remembered spec.
    pub url: Option<String>,
    pub predicate: Option<String>,
}

/// Check whether an account is still logged in. Runs in a scratch session AS
/// the account (private jar, same egress), so the probe doubles as a cookie
/// refresh. Errors are actionable: a missing spec tells the caller to teach
/// one; a probe failure is not a logout verdict.
pub(crate) async fn account_verify_handler(
    Json(req): Json<AccountVerifyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let v = account::verify(
        crate::store::REST_OWNER,
        &req.name,
        req.url.as_deref(),
        req.predicate.as_deref(),
    )
    .await
    .map_err(AppError::BadRequest)?;
    Ok((StatusCode::OK, Json(v)))
}

#[derive(Debug, Deserialize)]
pub struct AccountLoginRequest {
    /// The account to log in as (created implicitly on first use; 1-64
    /// chars of [a-zA-Z0-9_-]).
    pub name: String,
    /// The login page URL to open as this account.
    pub url: String,
    /// A JS expression truthy on the page the site lands on AFTER login.
    /// With it and no human step detected, the call waits for the automatic
    /// bounce and stamps the verify spec on success.
    pub predicate: Option<String>,
    /// Route through the engine proxy. Seeds a fresh account; an account
    /// with an existing record reuses its recorded egress.
    #[serde(default)]
    pub use_proxy: bool,
    /// Wait budget in ms for the automatic-login bounce (default 60000,
    /// clamped 1000..120000). Never spent while a human step is outstanding.
    #[serde(default = "default_login_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_login_timeout_ms() -> u64 {
    60_000
}

/// Open a site's login page AS a named account: probes the generic login
/// gates the page shows (password/sms/qr/slider), and with a predicate and
/// no gates, waits once for the automatic bounce then stamps the verify
/// spec. Human steps return immediately with session_id + /live handoff;
/// cookies write back after every action, so re-calling with the same
/// arguments after the human finishes closes the loop (shared jar).
pub(crate) async fn account_login_handler(
    Json(req): Json<AccountLoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    let v = account::login(
        crate::store::REST_OWNER,
        &req.name,
        &req.url,
        req.predicate.as_deref(),
        req.use_proxy,
        req.timeout_ms,
    )
    .await
    .map_err(AppError::BadRequest)?;
    Ok((StatusCode::OK, Json(v)))
}

/// Derive a session carrying the source's login state (cookies + storage +
/// viewport + dialog policy); the source stays untouched.
pub(crate) async fn session_clone_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr.clone_session(&id).await.map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

/// Live sessions with idle age and time left before auto-eviction — the
/// discovery twin of /session/create (reuse instead of spawning a fresh V8
/// thread per step).
pub(crate) async fn session_list_handler() -> impl IntoResponse {
    let mut mgr = session::SESSIONS.lock().await;
    mgr.evict_expired();
    let sessions = mgr.list();
    axum::Json(serde_json::json!({ "count": sessions.len(), "sessions": sessions }))
}

/// Replace the session's document-start preload group (empty array clears).
/// Sources run before each new document's own scripts — including inline
/// `<script>` tags — the only hook that beats pages whose signing layer
/// captures `window.fetch`/XHR natives at parse time (issue #96, xhs's
/// inline jsvmp). Set before the first navigate of a fresh session and it
/// applies to every navigation from then on.
#[derive(Deserialize)]
pub(crate) struct SessionPreloadBody {
    /// Full JS sources, in order. `[]` clears the group.
    scripts: Vec<String>,
}

pub(crate) async fn session_preload_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<SessionPreloadBody>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr
        .send(&id, |reply| session::SessionCommand::SetPreload {
            scripts: body.scripts,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

pub(crate) async fn session_navigate_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionNavigateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr
        .send(&id, |reply| session::SessionCommand::Navigate {
            url: req.url.clone(),
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

/// Session-command errors → HTTP semantics: a dead session is 404, a crashed
/// thread 503, the stance gate 429, everything else 500.
fn session_err(e: session::SessionError) -> AppError {
    if let session::SessionError::Command(msg) = &e {
        if msg.starts_with("rate limit:") {
            return AppError::TooManyRequests(msg.clone());
        }
    }
    match e {
        session::SessionError::NotFound(msg) | session::SessionError::Expired(msg) => {
            AppError::NotFound(msg)
        }
        // An await-budget expiry is a timeout, not a bad script — 504 lets
        // callers branch on "retry slower / raise timeout_ms" separately
        // from "my script threw" (0.4.1 taobao report, problem 2).
        session::SessionError::Eval(msg)
            if msg.contains("timed out") || msg.contains("EVAL_TIMEOUT") =>
        {
            AppError::GatewayTimeout(msg)
        }
        session::SessionError::Eval(msg) => AppError::BadRequest(msg),
        session::SessionError::ThreadDied(msg) => AppError::ServiceUnavailable(msg),
        session::SessionError::Command(msg) => AppError::Internal(msg),
    }
}

pub(crate) async fn session_state_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let compact_text = mgr
        .send(&id, |reply| session::SessionCommand::State { reply })
        .await
        .map_err(session_err)?;
    // Return as plain text for token efficiency.
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        compact_text,
    ))
}

/// Snapshot the session's localStorage/sessionStorage (round-trips with
/// session/create's `storage` field).
pub(crate) async fn session_storage_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Storage { reply })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("storage parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

/// Recent page console output (ring buffer of 500). Optional query filters:
/// `?level=error&since_ts=<epoch_ms>&url_contains=<substr>&limit=<n>`.
#[derive(Deserialize)]
pub(crate) struct SessionConsoleQuery {
    level: Option<String>,
    since_ts: Option<u64>,
    url_contains: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn session_console_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<SessionConsoleQuery>,
) -> Result<impl IntoResponse, AppError> {
    let filter = session::ConsoleFilter {
        level: q.level,
        since_ts: q.since_ts,
        url_contains: q.url_contains,
        limit: q.limit,
    };
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Console {
            filter,
            reply,
        })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("console parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

#[derive(Deserialize)]
pub(crate) struct SessionDialogBody {
    action: String,
    #[serde(default)]
    prompt_text: Option<String>,
}

/// Inspect or flip the session's dialog policy (window.alert/confirm/prompt
/// are auto-answered, never blocking; entries land in /console at level
/// "dialog"). POST {"action":"list"|"accept"|"dismiss", "prompt_text"?}.
pub(crate) async fn session_dialog_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<SessionDialogBody>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Dialog {
            action: body.action,
            prompt_text: body.prompt_text,
            reply,
        })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("dialog parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

#[derive(Deserialize)]
pub(crate) struct SessionExportQuery {
    /// `bash` (default) emits a runnable curl replay script;
    /// `jsonl` emits the raw action log;
    /// `json` emits a flow.json document (recorded steps as editable,
    /// replayable ops — cookies/storage stripped; replay via /flow/run).
    #[serde(default)]
    format: Option<String>,
}

/// Export the session's recorded actions: as a bash+curl replay script
/// (default — replay with zero model tokens), as raw JSONL, or as a
/// flow.json document for POST /flow/run.
pub(crate) async fn session_export_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<SessionExportQuery>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let jsonl = mgr
        .send(&id, |reply| session::SessionCommand::Export { reply })
        .await
        .map_err(session_err)?;
    match q.format.as_deref() {
        Some("jsonl") => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
            jsonl,
        )),
        Some("json") => {
            let doc = flow::recorded_to_flow(&jsonl);
            Ok((
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                doc.to_string(),
            ))
        }
        _ => {
            let script = session::replay_bash(&jsonl, "http://127.0.0.1:8089");
            Ok((
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/x-shellscript; charset=utf-8",
                )],
                script,
            ))
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct FlowRunBody {
    /// Inline flow document ({create?, vars?, steps:[{op, args, expect?, save?}]}).
    #[serde(default)]
    flow: Option<serde_json::Value>,
    /// Or run a server-side workflow/<name>/flow.json asset (unknown name →
    /// error lists what's installed).
    #[serde(default)]
    name: Option<String>,
    /// Values for {{placeholders}}; wins over the flow's own vars defaults.
    #[serde(default)]
    vars: Option<serde_json::Value>,
    /// Reuse a live session (e.g. from /import/curl) instead of creating a
    /// fresh one — how login state and flows compose.
    #[serde(default)]
    session_id: Option<String>,
    /// Override the run's step-execution budget (branch loops re-run steps,
    /// so every revisit counts). Default 1000, clamped 1..=100000; wins over
    /// any max_steps the flow document declares.
    #[serde(default)]
    max_steps: Option<u64>,
}

/// Run a flow — a recorded/edited JSON session script — to completion with
/// zero model tokens. Returns the receipt: `status:ok` + `saved` outputs, or
/// `status:failed` + the failing step, reason, and a diagnostic screenshot
/// (the session stays alive for manual takeover).
pub(crate) async fn flow_run_handler(Json(body): Json<FlowRunBody>) -> Result<impl IntoResponse, AppError> {
    let mut doc =
        flow::resolve_flow_doc(body.flow, body.name.as_deref()).map_err(AppError::BadRequest)?;
    // Caller budget wins over the document's own max_steps — one read path
    // inside run_flow.
    if let (Some(ms), Some(obj)) = (body.max_steps, doc.as_object_mut()) {
        obj.insert("max_steps".into(), serde_json::json!(ms));
    }
    let vars = body
        .vars
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut mgr = session::SESSIONS.lock().await;
    let receipt = flow::run_flow(&mut mgr, &doc, &vars, body.session_id).await;
    Ok((StatusCode::OK, Json(receipt)))
}

#[derive(Deserialize)]
pub(crate) struct SessionNetworkQuery {
    /// `media` extracts playback/stream links (m3u8, mp4, ...) from the
    /// requests the page actually issued; anything else lists all traffic.
    #[serde(default)]
    filter: Option<String>,
    /// Add an `xhr` array of background API responses (the page's own fetch/
    /// XHR traffic with retained bodies) alongside the request rows.
    #[serde(default)]
    include_bodies: Option<bool>,
    /// Narrow the `xhr` array to URLs containing this substring.
    #[serde(default)]
    url_contains: Option<String>,
    /// Per-body character cap for the `xhr` array (default 4000).
    #[serde(default)]
    body_max_chars: Option<usize>,
}

/// The session's network request log: `?filter=media` is the playback-link
/// sniffer; the default returns compact rows for every request, plus an
/// `xhr` array of background API responses when `include_bodies=true`.
pub(crate) async fn session_network_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<SessionNetworkQuery>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Network {
            media_only: q.filter.as_deref() == Some("media"),
            include_bodies: q.include_bodies.unwrap_or(false),
            url_contains: q.url_contains,
            body_max_chars: q.body_max_chars.unwrap_or(4000),
            reply,
        })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("network parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

/// Full HAR 1.2 export of the session's current-page traffic (retained
/// response bodies included) — opens in Chrome DevTools / har viewers.
pub(crate) async fn session_har_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Har { reply })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("har parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

/// One-call risk-control report: every anti-bot challenge the session's
/// traffic hit — walls navigated into (punish URLs) and walls swallowed by
/// 200-status MTop JSON bodies (`FAIL_SYS_USER_VALIDATE` / `RGV587` /
/// `x5secdata`). With hits, carries the account name (which identity got
/// walled) and the human-handoff instruction; the engine detects and
/// surfaces, it does not auto-bypass.
pub(crate) async fn session_challenges_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Challenges { reply })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("challenges parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

/// The decision-layer fact sheet (issue #73): one call classifies where
/// the session landed — `challenge` (risk control engaged, human
/// handoff), `captcha`, `login`, `empty`, `landed`, or `unknown` (a
/// non-2xx document lands here with its status in `facts`). Pure code
/// over signals the engine already holds; no screenshots, no page
/// evals, no model.
pub(crate) async fn session_verdict_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Verdict { reply })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("verdict parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

pub(crate) async fn session_click_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionClickRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let resp = mgr
        .send(&id, |reply| session::SessionCommand::Click {
            index: req.index,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((StatusCode::OK, Json(resp)))
}

#[derive(Deserialize)]
pub(crate) struct SessionClickXyRequest {
    x: f64,
    y: f64,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
}

/// Click at viewport coordinates via the real mouse chain
/// (pointerdown/mousedown → pointerup/mouseup → click on the hit element).
pub(crate) async fn session_click_xy_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionClickXyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::ClickXY {
            x: req.x,
            y: req.y,
            button: req.button.unwrap_or_else(|| "left".to_string()),
            click_count: req.click_count.unwrap_or(1),
            reply,
        })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("click_xy parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

#[derive(Deserialize)]
pub(crate) struct SessionXyBody {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
pub(crate) struct SessionDragRequest {
    from: SessionXyBody,
    to: SessionXyBody,
    #[serde(default)]
    steps: Option<u32>,
    #[serde(default)]
    delay_ms: Option<u64>,
    /// Humanized trajectory (default true); false = exact linear interpolation
    #[serde(default)]
    humanize: Option<bool>,
}

/// Drag the mouse from `from` to `to` through mousemove events — the
/// trajectory is humanized by default (eased velocity, wobble, jittered
/// timing); `humanize:false` keeps exact linear interpolation.
pub(crate) async fn session_drag_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionDragRequest>,
) -> Result<impl IntoResponse, AppError> {
    let humanize = req.humanize.unwrap_or(true);
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Drag {
            from_x: req.from.x,
            from_y: req.from.y,
            to_x: req.to.x,
            to_y: req.to.y,
            steps: req.steps.unwrap_or(if humanize { 24 } else { 10 }),
            delay_ms: req.delay_ms.unwrap_or(if humanize { 18 } else { 30 }),
            humanize,
            reply,
        })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("drag parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

pub(crate) async fn session_input_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionInputRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let filled = mgr
        .send(&id, |reply| session::SessionCommand::Input {
            index: req.index,
            text: req.text,
            full_events: req.events.as_deref() == Some("full"),
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((StatusCode::OK, Json(filled)))
}

/// Programmatic file selection: build File objects from base64 content and
/// assign them through the page's `input.files` (Playwright setInputFiles
/// semantics), then dispatch input+change so framework onChange handlers fire.
pub(crate) async fn session_set_files_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionSetFilesRequest>,
) -> Result<impl IntoResponse, AppError> {
    if req.files.is_empty() {
        return Err(AppError::BadRequest(
            "files must not be empty — to clear a selection, send one 0-byte file or clear via eval".to_string(),
        ));
    }
    let specs: Vec<serde_json::Value> = req
        .files
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "content_base64": f.content_base64,
                "mime_type": f.mime_type,
                "last_modified": f.last_modified,
            })
        })
        .collect();
    let mut mgr = session::SESSIONS.lock().await;
    let result = mgr
        .send(&id, |reply| session::SessionCommand::SetFiles {
            selector: req.selector,
            files: specs,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((StatusCode::OK, Json(result)))
}

pub(crate) async fn session_scroll_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionScrollRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let scrolled = mgr
        .send(&id, |reply| session::SessionCommand::Scroll {
            direction: req.direction,
            amount: req.amount,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "scrolled": scrolled })),
    ))
}

pub(crate) async fn session_viewport_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionViewportRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let viewport = mgr
        .send(&id, |reply| session::SessionCommand::Viewport {
            width: req.width,
            height: req.height,
            mobile: req.mobile,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "viewport": viewport })),
    ))
}

/// Screenshot the session's current DOM state. The reply mirrors
/// POST /screenshot's shape (image_base64 PNG) so existing consumers work
/// against either; empty body is allowed (viewport-sized capture).
pub(crate) async fn session_screenshot_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    req: Option<Json<SessionScreenshotRequest>>,
) -> Result<impl IntoResponse, AppError> {
    let req = req.map(|Json(r)| r).unwrap_or_default();
    let mut mgr = session::SESSIONS.lock().await;
    let shot = mgr
        .send(&id, |reply| session::SessionCommand::Screenshot {
            width: req.width,
            height: req.height,
            full_page: req.full_page,
            selector: req.selector.clone(),
            selector_all: req.selector_all,
            reply,
        })
        .await
        .map_err(session_err)?;
    let body: serde_json::Value =
        serde_json::from_str(&shot).map_err(|_| AppError::Internal(shot.clone()))?;
    Ok((StatusCode::OK, Json(body)))
}

/// Classify an eval-body rejection into a structured AppError. The default
/// axum rejection for an over-limit body is a bare-text 413 ("Failed to
/// buffer the request body: length limit exceeded") — callers pushing
/// base64 payloads through eval need the code and the limit in a shape
/// they can branch on (0.4.1 taobao report, problem 1).
fn eval_body_rejection(body_text: String, limit_bytes: usize) -> AppError {
    if body_text.contains("length limit") {
        AppError::PayloadTooLarge(format!(
            "EVAL_BODY_TOO_LARGE: request body exceeded the {}-byte limit \
             (raise AGINXBROWSER_MAX_BODY_BYTES or move binary payloads off eval): {}",
            limit_bytes, body_text
        ))
    } else {
        AppError::BadRequest(format!("invalid JSON body: {}", body_text))
    }
}

pub(crate) async fn session_eval_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    payload: Result<Json<SessionEvalRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(req) =
        payload.map_err(|rej| eval_body_rejection(rej.body_text(), max_body_bytes()))?;
    let mut mgr = session::SESSIONS.lock().await;
    let result = mgr
        .send(&id, |reply| session::SessionCommand::Eval {
            script: req.script,
            timeout_ms: req.timeout_ms,
            reply,
        })
        .await
        .map_err(session_err)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "result": result })),
    ))
}

/// Live sessions with their identity — the P2 ask from the 0.4.1 taobao
/// report: the homepage shows a count, agents need "which session is on
/// which page". The URL rides a cheap per-session Url command with a short
/// timeout so a session pinned inside a long eval can't wedge the listing
/// (it reports `url: null` instead).
pub(crate) async fn sessions_handler() -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    mgr.evict_expired();
    let entries = mgr.list();
    let mut out = Vec::with_capacity(entries.len());
    for e in &entries {
        let url = match tokio::time::timeout(
            std::time::Duration::from_millis(250),
            mgr.send(&e.session_id, |reply| session::SessionCommand::Url {
                reply,
            }),
        )
        .await
        {
            Ok(Ok(u)) => Some(u),
            // Busy (mid-eval), expired, or gone — identity probe is best
            // effort; the listing must never block on one session.
            _ => None,
        };
        out.push(serde_json::json!({
            "session_id": e.session_id,
            "url": url,
            "idle_secs": e.idle_secs,
            "expires_in_secs": e.expires_in_secs,
            "keepalive": e.keepalive,
            "persistent": e.persistent,
        }));
    }
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "count": out.len(),
            "sessions": out,
        })),
    ))
}

/// Cookie read-back for one session — the HTTP-face mirror of the MCP
/// `session_cookies` tool (the Cookies command already existed; only the
/// route was missing). Values are the full Set-Cookie form so the output
/// round-trips with `POST /session/create`'s `cookies` field.
pub(crate) async fn session_cookies_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let text = mgr
        .send(&id, |reply| session::SessionCommand::Cookies { reply })
        .await
        .map_err(session_err)?;
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("cookies parse error: {}", e)))?;
    Ok((StatusCode::OK, Json(val)))
}

/// Wait for a selector/predicate with the page's event loop driven between
/// polls. Errors (timeout) surface as 500 with the message, matching the
/// other session error paths.
pub(crate) async fn session_wait_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SessionWaitRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    let out = mgr
        .send(&id, |reply| session::SessionCommand::Wait {
            selector: req.selector.clone(),
            predicate: req.predicate.clone(),
            timeout_ms: req.timeout_ms,
            reply,
        })
        .await
        .map_err(session_err)?;
    let body: serde_json::Value =
        serde_json::from_str(&out).map_err(|_| AppError::Internal(out.clone()))?;
    Ok((StatusCode::OK, Json(body)))
}

pub(crate) async fn session_close_handler(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mut mgr = session::SESSIONS.lock().await;
    // Wait for the session thread's ack so `ok` is truthful - a runaway eval
    // can pin the thread inside V8 for up to its watchdog budget.
    let closed = mgr.close_and_wait(&id).await;
    Ok((StatusCode::OK, Json(serde_json::json!({ "ok": closed }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0.4.1 taobao report problem 1: an over-limit body must classify as a
    // structured 413 carrying the code and the limit in force, not axum's
    // bare-text page. The length-limit text below is axum 0.7's verbatim
    // rejection body — pinned so an upstream wording change fails loudly here
    // instead of silently downgrading 413s to 400s.
    #[test]
    fn eval_body_rejection_classifies_length_limit_as_payload_too_large() {
        match eval_body_rejection(
            "Failed to buffer the request body: length limit exceeded".into(),
            67_108_864,
        ) {
            AppError::PayloadTooLarge(msg) => {
                assert!(msg.contains("EVAL_BODY_TOO_LARGE"), "got: {msg}");
                assert!(msg.contains("67108864"), "message names the limit: {msg}");
            }
            _ => panic!("expected PayloadTooLarge"),
        }
        // Any other rejection stays a malformed-JSON 400.
        match eval_body_rejection("expected value at line 1 column 1".into(), 67_108_864) {
            AppError::BadRequest(msg) => assert!(msg.contains("invalid JSON body"), "got: {msg}"),
            _ => panic!("expected BadRequest"),
        }
    }
}
