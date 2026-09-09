//! Interactive browser sessions for agent-style browsing.
//!
//! Each session lives on its own OS thread with a persistent Browser + Page.
//! Commands are dispatched via channels, results returned via oneshot.
//! Sessions auto-expire after 8 minutes of inactivity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::page::Page;

/// Monotonic counter for unique session IDs within a process.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default maximum idle time before a session is evicted. Overridable per
/// session at create time (ttl_secs, clamped 60..3600) — a long workflow
/// shouldn't lose its login state to the idle reaper mid-run.
const SESSION_TIMEOUT: Duration = Duration::from_secs(480); // 8 minutes

/// Ring buffer size for a session's console log (see the Console command).
const CONSOLE_RING_CAP: usize = 500;

/// Snapshots older than this are dropped instead of revived — a login
/// re-injected days later is more surprise than service. Purged lazily on
/// revival attempts and in the eviction sweep.
const SNAPSHOT_MAX_AGE_SECS: i64 = 24 * 3600;

/// Why a session command failed. An agent must be able to tell "the session
/// is gone — recreate it" from "this step failed — retry it" without parsing
/// prose, so every variant carries a machine-readable code when serialized.
#[derive(Debug, Clone)]
pub enum SessionError {
    /// Unknown id — never existed or already closed.
    NotFound(String),
    /// Idle past its TTL; the session was closed on this access.
    Expired(String),
    /// The session thread is gone (crashed or panicked).
    ThreadDied(String),
    /// The command itself failed inside a live session (selector miss,
    /// navigation failure, stance gate, ...). The session is still usable.
    Command(String),
    /// The evaluated script threw or a returned promise rejected. Distinct
    /// from [`SessionError::Command`] so agents can tell "my script is
    /// wrong" from "the tool refused"; the message carries the error name,
    /// message, throw position and first stack frame.
    Eval(String),
}

impl SessionError {
    pub fn code(&self) -> &'static str {
        match self {
            SessionError::NotFound(_) => "SESSION_NOT_FOUND",
            SessionError::Expired(_) => "SESSION_EXPIRED",
            SessionError::ThreadDied(_) => "SESSION_CRASHED",
            SessionError::Command(_) => "COMMAND_FAILED",
            SessionError::Eval(_) => "EVAL_ERROR",
        }
    }

    pub fn to_value(&self) -> Value {
        let mut v = serde_json::json!({
            "code": self.code(),
            "message": self.to_string(),
        });
        if matches!(self, SessionError::NotFound(_) | SessionError::Expired(_)) {
            v["hint"] = Value::String("call session_create to recreate".to_string());
        }
        v
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotFound(m)
            | SessionError::Expired(m)
            | SessionError::ThreadDied(m)
            | SessionError::Command(m)
            | SessionError::Eval(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<String> for SessionError {
    fn from(m: String) -> Self {
        SessionError::Command(m)
    }
}

// Serialized into tool responses via `json!({"error": e})` — the whole
// object, not a bare string.
impl Serialize for SessionError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

/// Move the engine's pending console calls into the session's ring buffer.
/// Called after every command so the queue drains regularly (a long
/// non-CDP session would otherwise grow it unboundedly) and by the Console
/// command itself.
fn drain_console(page: &Page, ring: &mut std::collections::VecDeque<Value>) {
    let calls = page.inner.take_pending_console_calls();
    if calls.is_empty() {
        return;
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for (level, msg, log_url) in calls {
        ring.push_back(serde_json::json!({
            "ts_ms": ts_ms, "level": level, "text": msg, "url": log_url,
        }));
    }
    while ring.len() > CONSOLE_RING_CAP {
        ring.pop_front();
    }
}

/// Optional narrowing for a Console read (session tool + HTTP query share
/// this). All fields absent → the full ring comes back, newest last. `limit`
/// keeps the most recent N matches — with a rolling buffer that is the
/// useful end of the log.
#[derive(Clone, Debug, Default)]
pub struct ConsoleFilter {
    pub level: Option<String>,
    pub since_ts: Option<u64>,
    pub url_contains: Option<String>,
    pub limit: Option<usize>,
}

impl ConsoleFilter {
    fn matches(&self, entry: &Value) -> bool {
        if let Some(level) = &self.level {
            let hit = entry
                .get("level")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case(level))
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        if let Some(since) = self.since_ts {
            let hit = entry
                .get("ts_ms")
                .and_then(|v| v.as_u64())
                .map(|t| t >= since)
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        if let Some(needle) = &self.url_contains {
            let hit = entry
                .get("url")
                .and_then(|v| v.as_str())
                .map(|u| u.contains(needle.as_str()))
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Command protocol
// ---------------------------------------------------------------------------

/// GetViewport's read-back: the pinned (width, height, mobile), or None.
type ViewportOverrideReply = oneshot::Sender<Result<Option<(f32, f32, bool)>, String>>;

pub enum SessionCommand {
    Navigate {
        url: String,
        reply: oneshot::Sender<Result<SessionNavResponse, String>>,
    },
    /// Load literal HTML as the page's content (local, free — no network,
    /// no page budget, no rate gate). Same load path as a navigation so
    /// DOM/JS state machinery treats it as a real page.
    SetContent {
        html: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    State {
        reply: oneshot::Sender<Result<String, String>>,
    },
    Click {
        index: usize,
        reply: oneshot::Sender<Result<SessionClickResponse, String>>,
    },
    Input {
        index: usize,
        text: String,
        /// events:"full" — per-character keydown/keypress/input/keyup cycle
        /// for strict keyboard listeners (keypress-submit login forms).
        full_events: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Scroll {
        direction: ScrollDirection,
        amount: u32,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Eval {
        script: String,
        reply: oneshot::Sender<Result<Value, SessionError>>,
    },
    /// Pin the viewport (width/height/mobile). None width/height keeps the
    /// current dimension (Chrome's setDeviceMetricsOverride semantics),
    /// both-None with mobile flips only the pointer persona.
    Viewport {
        width: Option<u32>,
        height: Option<u32>,
        mobile: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Read back the pinned viewport (session_clone's source side); None
    /// when the session never called Viewport.
    GetViewport {
        reply: ViewportOverrideReply,
    },
    /// Export the session's cookies as a JSON string
    /// `{"url":...,"cookies":["name=value; Domain=...; Path=...; ...", ...]}`.
    /// Full Set-Cookie form so a clone opened on a sibling domain keeps its
    /// login state. Round-trips with `session_create`'s `cookies` field.
    Cookies {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Snapshot localStorage/sessionStorage for the page's current origin as
    /// `{"url","origin","local_storage":{k:v},"session_storage":{k:v}}`.
    /// Round-trips with `session_create`'s `storage` field to replay a
    /// logged-in session when cookies alone aren't enough.
    Storage {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Recent console output (log/warn/error/...) from the page, ring-buffered
    /// (500 entries). Drained from the engine's console queue after every
    /// command, so output from clicks/evals/navigation alike shows up here.
    /// Read-only, so not recorded in the action log. The filter narrows what
    /// comes back; entries themselves are kept intact in the ring.
    Console {
        filter: ConsoleFilter,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Inspect or flip the dialog policy for window.alert/confirm/prompt.
    /// action "list" reports the policy plus every level-"dialog" ring
    /// entry; "accept"/"dismiss" set it for subsequent dialogs (dialogs
    /// never block — they are auto-answered and logged). prompt_text sets
    /// what window.prompt returns once accepted; omitted keeps the current.
    Dialog {
        action: String,
        prompt_text: Option<String>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Click at viewport coordinates via the real mouse chain
    /// (pointerdown/mousedown → pointerup/mouseup → click on whatever
    /// elementFromPoint hits there). click_count 2 adds dblclick, 3+ sets
    /// detail — the shape canvas/map pages listen for.
    ClickXY {
        x: f64,
        y: f64,
        button: String,
        click_count: u32,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Press at `from`, glide through `steps` interpolated mousemove events
    /// (delay_ms between each), release at `to` — drags a marker/canvas
    /// selection the way a real pointer would, so mousemove-driven widgets
    /// (AMap markers, drag handles) track every intermediate position.
    Drag {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        steps: u32,
        delay_ms: u64,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Acknowledged by the session thread right before it exits. The closer
    /// waits on this to learn the thread actually stopped - without it, close
    /// replies ok while the thread is still pinned inside V8 (a runaway eval)
    /// and keeps burning CPU.
    Close {
        reply: oneshot::Sender<()>,
    },
    /// Export the session's recorded action log as JSONL (one
    /// RecordedAction per line) — the raw material for replay scripts.
    Export {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Snapshot the page's network request log (the sniffer surface). With
    /// `media_only`, the reply is `{"url","media":[{url,kind,status,mime,via}]}`
    /// — playback links extracted from requests the page actually issued
    /// (via "network"), merged with DOM-observed media-element and player
    /// iframe sources the engine never fetches (via "dom" candidates);
    /// otherwise `{"url","total","requests":[...]}` compact rows, plus an
    /// `xhr` array of background API responses when `include_bodies` is set
    /// (Scrapling's capture_xhr insight: the page's own API face is the
    /// clean structured read).
    Network {
        media_only: bool,
        include_bodies: bool,
        url_contains: Option<String>,
        body_max_chars: usize,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Export the current page's traffic as a HAR 1.2 JSON document
    /// (retained response bodies included).
    Har {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Render the session's CURRENT DOM state — mutations from clicks,
    /// inputs, evals included — to a PNG through the diting pipeline.
    /// Read-only, so not recorded in the action log. Width/height default
    /// to the session's live viewport (a session_viewport override shows up
    /// in the pixels). Reply is a JSON string with image_base64.
    Screenshot {
        width: Option<u32>,
        height: Option<u32>,
        full_page: bool,
        selector: Option<String>,
        selector_all: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Poll until a CSS selector matches or a JS predicate turns truthy,
    /// driving the page's event loop between checks so async work (fetches,
    /// timers, promise chains) actually progresses — a plain sleep does not
    /// run page JS. Read-only, so not recorded in the action log. Reply is
    /// `{"matched":true,"elapsed_ms":N,"detail":{...}}`; timeout replies Err.
    Wait {
        selector: Option<String>,
        predicate: Option<String>,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SessionNavResponse {
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionClickResponse {
    pub url: String,
    pub clicked: bool,
    /// Post-click landed page text (body.innerText, capped), so the client
    /// can diff before/after in one response — same evidence contract as the
    /// stateless /click.
    pub text_after: Option<String>,
}

/// One live session, as reported by [`SessionManager::list`].
#[derive(Debug, Serialize)]
pub struct SessionListEntry {
    pub session_id: String,
    /// Seconds since the session last answered a command.
    pub idle_secs: u64,
    /// Idle budget left before auto-eviction. Absent for `keepalive`
    /// sessions, which never auto-evict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    /// True when the session is exempt from the idle reaper.
    pub keepalive: bool,
    /// True when the login state is snapshotted and the session revives by
    /// the same id after eviction or restart.
    pub persistent: bool,
}

/// One recorded session action — the replay log. Only actions that change
/// the page are recorded; reads (State, Cookies) would just add noise to a
/// replay script.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecordedAction {
    Create { url: Option<String>, use_proxy: bool, cookies: Vec<String>, #[serde(skip_serializing_if = "Option::is_none")] storage: Option<Value> },
    Navigate { url: String, ok: bool },
    SetContent { html: String, ok: bool },
    Click { index: usize, ok: bool },
    #[serde(rename = "click_xy")]
    ClickXY { x: f64, y: f64, ok: bool },
    Drag { from_x: f64, from_y: f64, to_x: f64, to_y: f64, steps: u32 },
    Input { index: usize, text: String, ok: bool },
    Scroll { direction: String, amount: u32 },
    Eval { script: String },
    Viewport { width: Option<u32>, height: Option<u32>, mobile: bool },
}

// ---------------------------------------------------------------------------
// Session handle
// ---------------------------------------------------------------------------

struct BrowserSession {
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    last_active: Instant,
    timeout: Duration,
    /// keepalive sessions are exempt from the idle reaper — a long workflow
    /// with SSH/DB queries between browser steps must not lose its login
    /// state (real-device feedback ③). Lives until session_close or exit.
    keepalive: bool,
    /// Remembered so session_clone can reproduce the egress path (a login
    /// behind a proxy breaks when the clone egresses directly).
    use_proxy: bool,
    /// Login state (cookies + web storage + viewport + dialog policy) is
    /// snapshotted to the local store after every command, and the session
    /// revives under the same id from that snapshot after an idle eviction
    /// or a process restart (feedback ③). `session_close` drops the snapshot.
    persistent: bool,
}

impl BrowserSession {
    fn is_expired(&self) -> bool {
        !self.keepalive && self.last_active.elapsed() > self.timeout
    }
}

/// Where persistent-session snapshots live. Production delegates to the
/// local SQLite store (store.rs); tests inject a shared in-memory map — two
/// managers holding one Arc simulate a process restart.
enum SnapshotStore {
    Global,
    Memory(std::sync::Arc<std::sync::Mutex<HashMap<String, (String, i64)>>>),
}

impl SnapshotStore {
    fn save(&self, id: &str, snapshot: &str) {
        match self {
            SnapshotStore::Global => {
                if let Err(e) = crate::store::save_session_snapshot(id, snapshot) {
                    tracing::debug!("session snapshot save failed: {e}");
                }
            }
            SnapshotStore::Memory(m) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                m.lock().expect("snapshot map poisoned").insert(id.to_string(), (snapshot.to_string(), now));
            }
        }
    }

    fn load(&self, id: &str) -> Option<(String, i64)> {
        match self {
            SnapshotStore::Global => crate::store::load_session_snapshot(id),
            SnapshotStore::Memory(m) => m.lock().expect("snapshot map poisoned").get(id).cloned(),
        }
    }

    fn delete(&self, id: &str) {
        match self {
            SnapshotStore::Global => {
                crate::store::delete_session_snapshot(id);
            }
            SnapshotStore::Memory(m) => {
                m.lock().expect("snapshot map poisoned").remove(id);
            }
        }
    }

    fn purge(&self, max_age_secs: i64) {
        match self {
            SnapshotStore::Global => crate::store::purge_session_snapshots(max_age_secs),
            SnapshotStore::Memory(m) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                m.lock()
                    .expect("snapshot map poisoned")
                    .retain(|_, (_, at)| now - *at <= max_age_secs);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

pub struct SessionManager {
    sessions: HashMap<String, BrowserSession>,
    snapshots: SnapshotStore,
}

/// Global session manager, shared between HTTP handlers and MCP tools.
pub static SESSIONS: std::sync::LazyLock<tokio::sync::Mutex<SessionManager>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(SessionManager::new()));

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
            snapshots: SnapshotStore::Global,
        }
    }

    /// Create a new browser session. Returns the session ID. `pin_viewport`
    /// sets the initial device emulation (feedback ⑤: the viewport belongs to
    /// the session's identity, so `session_create {width, height, mobile}`
    /// must produce the same layout a prior session_viewport call would).
    /// Dimensions are forwarded as given — the Viewport command keeps the
    /// live value for any unspecified axis. With `persistent`, the login
    /// state is snapshotted to the local store after every command and the
    /// session revives under the same id after idle eviction or restart.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        start_url: Option<&str>,
        use_proxy: bool,
        cookies: Vec<String>,
        storage: Option<Value>,
        ttl_secs: Option<u64>,
        pin_viewport: Option<(Option<u32>, Option<u32>, bool)>,
        keepalive: bool,
        persistent: bool,
    ) -> String {
        // A snapshot with the same id may survive from a previous process —
        // a fresh session must never squat on a revivable id.
        let session_id = loop {
            let id = format!("s_{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed));
            if !self.sessions.contains_key(&id) && self.snapshots.load(&id).is_none() {
                break id;
            }
        };
        self.spawn_session(
            &session_id,
            start_url,
            use_proxy,
            cookies,
            storage,
            ttl_secs,
            pin_viewport,
            keepalive,
            persistent,
        );
        session_id
    }

    /// Bring a session (fresh or revived) to life under an explicit id:
    /// spawn the session thread, register it, and queue the viewport pin.
    #[allow(clippy::too_many_arguments)]
    fn spawn_session(
        &mut self,
        session_id: &str,
        start_url: Option<&str>,
        use_proxy: bool,
        cookies: Vec<String>,
        storage: Option<Value>,
        ttl_secs: Option<u64>,
        pin_viewport: Option<(Option<u32>, Option<u32>, bool)>,
        keepalive: bool,
        persistent: bool,
    ) {
        let timeout = Duration::from_secs(ttl_secs.unwrap_or(SESSION_TIMEOUT.as_secs()).clamp(60, 3600));

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let thread_id = session_id.to_string();
        let thread_url = start_url.map(|s| s.to_string());
        std::thread::Builder::new()
            .name(format!("session-{}", &thread_id[..8.min(thread_id.len())]))
            // Deep stack for the V8 isolate — see server::v8_stack_size.
            // A default 2 MB thread dies on minified SPA recursion
            // (juejin.cn class) before the page renders.
            .stack_size(crate::server::v8_stack_size())
            .spawn(move || {
                session_thread(thread_id, thread_url, use_proxy, cookies, storage, cmd_rx);
            })
            .expect("failed to spawn session thread");

        self.sessions.insert(
            session_id.to_string(),
            BrowserSession {
                cmd_tx: cmd_tx.clone(),
                last_active: Instant::now(),
                timeout,
                keepalive,
                use_proxy,
                persistent,
            },
        );
        // Fire-and-forget viewport pin: it queues ahead of anything the
        // caller can send (the id is returned only after this enqueue), and
        // the thread applies it right after its initial navigation. The
        // override lives on the Page, so it survives every later navigation.
        if let Some((w, h, mobile)) = pin_viewport {
            let (tx, _rx) = oneshot::channel();
            let _ = cmd_tx.send(SessionCommand::Viewport {
                width: w,
                height: h,
                mobile,
                reply: tx,
            });
        }
    }

    /// Seconds left before this session is evicted for idleness; None for
    /// keepalive sessions (no idle expiry). Feedback ③: the agent should see
    /// the remaining lifetime instead of discovering an expired session
    /// mid-workflow.
    pub fn expires_in_secs(&self, session_id: &str) -> Option<u64> {
        let s = self.sessions.get(session_id)?;
        if s.keepalive {
            return None;
        }
        Some(s.timeout.saturating_sub(s.last_active.elapsed()).as_secs())
    }

    /// Derive a new session carrying the source's login state: cookies,
    /// localStorage/sessionStorage, viewport pin, dialog policy, proxy and
    /// keepalive flags, remaining timeout. The source is untouched — this
    /// replaces the manual cookies→create round-trip where a hand-edited
    /// cookie string could clobber a working login (real-device feedback ⑩).
    pub async fn clone_session(&mut self, session_id: &str) -> Result<Value, SessionError> {
        let cookies_text = self.send(session_id, |reply| SessionCommand::Cookies { reply }).await?;
        let storage_text = self.send(session_id, |reply| SessionCommand::Storage { reply }).await?;
        let dialog_text = self
            .send(session_id, |reply| SessionCommand::Dialog {
                action: "list".to_string(),
                prompt_text: None,
                reply,
            })
            .await?;
        let viewport = self
            .send(session_id, |reply| SessionCommand::GetViewport { reply })
            .await?;

        let parse = |text: String, what: &str| -> Result<Value, SessionError> {
            serde_json::from_str(&text)
                .map_err(|e| SessionError::Command(format!("{} snapshot parse error: {}", what, e)))
        };
        let cookies = parse(cookies_text, "cookies")?;
        let storage = parse(storage_text, "storage")?;
        let dialog = parse(dialog_text, "dialog")?;

        let (use_proxy, keepalive, persistent, timeout) = {
            let s = self
                .sessions
                .get(session_id)
                .ok_or_else(|| SessionError::NotFound(format!("session not found: {}", session_id)))?;
            (s.use_proxy, s.keepalive, s.persistent, s.timeout)
        };

        let url = cookies["url"].as_str().unwrap_or("about:blank").to_string();
        let cookie_list: Vec<String> = cookies["cookies"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let ls = storage["local_storage"].clone();
        let ss = storage["session_storage"].clone();
        let injected = if ls.as_object().is_none_or(|m| m.is_empty())
            && ss.as_object().is_none_or(|m| m.is_empty())
        {
            None
        } else {
            Some(serde_json::json!({ "local_storage": ls, "session_storage": ss }))
        };
        let pin = viewport.map(|(w, h, mobile)| (Some(w as u32), Some(h as u32), mobile));

        let new_id = self.create(
            Some(&url),
            use_proxy,
            cookie_list,
            injected,
            Some(timeout.as_secs()),
            pin,
            keepalive,
            persistent,
        );

        // Copy a non-default dialog policy over (fire-and-forget, same
        // enqueue-ahead pattern as the viewport pin inside create).
        let accept = dialog["policy"].as_str() == Some("accept");
        let prompt_text = dialog["prompt_text"].as_str().map(str::to_string);
        if accept || prompt_text.is_some() {
            if let Some(s) = self.sessions.get(&new_id) {
                let (tx, _rx) = oneshot::channel();
                let _ = s.cmd_tx.send(SessionCommand::Dialog {
                    action: if accept { "accept" } else { "dismiss" }.to_string(),
                    prompt_text,
                    reply: tx,
                });
            }
        }

        let mut resp = serde_json::json!({
            "session_id": new_id,
            "cloned_from": session_id,
            "url": url,
            "viewport": viewport
                .map(|(w, h, mobile)| serde_json::json!({"width": w as u32, "height": h as u32, "mobile": mobile}))
                .unwrap_or(Value::Null),
        });
        if let Some(s) = self.expires_in_secs(resp["session_id"].as_str().unwrap_or("")) {
            resp["expires_in_secs"] = serde_json::json!(s);
        }
        Ok(resp)
    }

    /// Send a command to a session and await the result. Most commands
    /// reply `Result<T, String>` (→ [`SessionError::Command`]); a few carry
    /// a richer error (e.g. Eval's [`SessionError::Eval`]) — both work via
    /// `E: Into<SessionError>`, inferred from the command's reply channel.
    ///
    /// An unknown id gets one second chance: a persistent session's
    /// snapshot revives it under the same id, so an idle eviction — or a
    /// whole server restart — costs the agent no re-login (feedback ③).
    pub async fn send<T: Send + 'static, E: Into<SessionError> + Send + 'static>(
        &mut self,
        session_id: &str,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, E>>) -> SessionCommand,
    ) -> Result<T, SessionError> {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if !session.is_expired() {
                session.last_active = Instant::now();
                return self.dispatch(session_id, make_cmd).await;
            }
            // Idle-expired on access. A persistent session falls through to
            // snapshot revival below instead of erroring; the eviction must
            // keep the snapshot that revival reads.
            let revive = session.persistent;
            self.close_inner(session_id, false);
            if !revive {
                return Err(SessionError::Expired(format!("session expired: {}", session_id)));
            }
        }
        if self.revive_session(session_id) {
            return self.dispatch(session_id, make_cmd).await;
        }
        Err(SessionError::NotFound(format!("session not found: {}", session_id)))
    }

    /// Channel round-trip against a live session — no expiry check, no
    /// revival, no snapshot capture (the capture collector itself runs
    /// through here, so it must not recurse).
    async fn raw_send<T: Send + 'static, E: Into<SessionError> + Send + 'static>(
        &self,
        session_id: &str,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, E>>) -> SessionCommand,
    ) -> Option<Result<T, E>> {
        let session = self.sessions.get(session_id)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        session.cmd_tx.send(make_cmd(reply_tx)).ok()?;
        reply_rx.await.ok()
    }

    /// Dispatch one command to a live session and, for persistent sessions,
    /// refresh the on-disk snapshot after every successful command so the
    /// snapshot never trails the live login state by more than one action.
    async fn dispatch<T: Send + 'static, E: Into<SessionError> + Send + 'static>(
        &mut self,
        session_id: &str,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, E>>) -> SessionCommand,
    ) -> Result<T, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::NotFound(format!("session not found: {}", session_id)))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .cmd_tx
            .send(make_cmd(reply_tx))
            .map_err(|_| SessionError::ThreadDied("session thread died".to_string()))?;

        let result: Result<T, SessionError> = reply_rx
            .await
            .map_err(|_| SessionError::ThreadDied("session thread died".to_string()))?
            .map_err(Into::into);
        if result.is_ok() && self.sessions.get(session_id).is_some_and(|s| s.persistent) {
            self.capture_snapshot(session_id).await;
        }
        result
    }

    /// Read the full login state back from the live session and hand it to
    /// the snapshot store. Best-effort: any read failure just skips the
    /// save (the next successful command retries).
    async fn capture_snapshot(&mut self, session_id: &str) {
        let (use_proxy, keepalive, ttl) = match self.sessions.get(session_id) {
            Some(s) => (s.use_proxy, s.keepalive, s.timeout.as_secs()),
            None => return,
        };
        let Some(cookies) = self
            .raw_send(session_id, |reply| SessionCommand::Cookies { reply })
            .await
            .and_then(Result::ok)
        else {
            return;
        };
        let Some(storage) = self
            .raw_send(session_id, |reply| SessionCommand::Storage { reply })
            .await
            .and_then(Result::ok)
        else {
            return;
        };
        let Some(dialog) = self
            .raw_send(session_id, |reply| SessionCommand::Dialog {
                action: "list".to_string(),
                prompt_text: None,
                reply,
            })
            .await
            .and_then(Result::ok)
        else {
            return;
        };
        let viewport: Option<Option<(f32, f32, bool)>> = self
            .raw_send(session_id, |reply| SessionCommand::GetViewport { reply })
            .await
            .and_then(Result::ok);

        let parse = |text: String| serde_json::from_str::<Value>(&text).ok();
        let (Some(cookies), Some(storage), Some(dialog)) = (parse(cookies), parse(storage), parse(dialog))
        else {
            return;
        };
        let cookie_list: Vec<String> = cookies["cookies"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let snapshot = serde_json::json!({
            "version": 1,
            "url": cookies["url"].as_str().unwrap_or("about:blank"),
            "cookies": cookie_list,
            "local_storage": storage["local_storage"],
            "session_storage": storage["session_storage"],
            "viewport": viewport.flatten().map(|(w, h, mobile)| serde_json::json!(
                {"width": w as u32, "height": h as u32, "mobile": mobile}
            )).unwrap_or(Value::Null),
            "dialog": {
                "policy": dialog["policy"].as_str().unwrap_or("dismiss"),
                "prompt_text": dialog["prompt_text"],
            },
            "use_proxy": use_proxy,
            "keepalive": keepalive,
            "ttl_secs": ttl,
        });
        self.snapshots.save(session_id, &snapshot.to_string());
    }

    /// Bring a snapshotted session back under the same id: false when there
    /// is no live snapshot (stale ones are dropped on sight).
    fn revive_session(&mut self, session_id: &str) -> bool {
        let Some((text, saved_at)) = self.snapshots.load(session_id) else {
            return false;
        };
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - saved_at;
        if age > SNAPSHOT_MAX_AGE_SECS {
            self.snapshots.delete(session_id);
            return false;
        }
        let Ok(snap) = serde_json::from_str::<Value>(&text) else {
            self.snapshots.delete(session_id);
            return false;
        };
        if snap["version"].as_u64() != Some(1) {
            self.snapshots.delete(session_id);
            return false;
        }
        let cookies: Vec<String> = snap["cookies"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let ls = snap["local_storage"].clone();
        let ss = snap["session_storage"].clone();
        let injected = if ls.as_object().is_none_or(|m| m.is_empty())
            && ss.as_object().is_none_or(|m| m.is_empty())
        {
            None
        } else {
            Some(serde_json::json!({ "local_storage": ls, "session_storage": ss }))
        };
        let pin = snap["viewport"].as_object().map(|v| {
            (
                v.get("width").and_then(Value::as_u64).map(|w| w as u32),
                v.get("height").and_then(Value::as_u64).map(|h| h as u32),
                v.get("mobile").and_then(Value::as_bool).unwrap_or(false),
            )
        });
        let dialog = snap["dialog"].as_object();
        let accept = dialog
            .and_then(|d| d.get("policy"))
            .and_then(Value::as_str)
            == Some("accept");
        let prompt_text = dialog
            .and_then(|d| d.get("prompt_text"))
            .and_then(Value::as_str)
            .map(str::to_string);

        self.spawn_session(
            session_id,
            snap["url"].as_str(),
            snap["use_proxy"].as_bool().unwrap_or(false),
            cookies,
            injected,
            snap["ttl_secs"].as_u64(),
            pin,
            snap["keepalive"].as_bool().unwrap_or(false),
            true,
        );

        // Non-default dialog policy rides along (enqueue-ahead, same as the
        // viewport pin inside spawn_session).
        if accept || prompt_text.is_some() {
            if let Some(s) = self.sessions.get(session_id) {
                let (tx, _rx) = oneshot::channel();
                let _ = s.cmd_tx.send(SessionCommand::Dialog {
                    action: if accept { "accept" } else { "dismiss" }.to_string(),
                    prompt_text,
                    reply: tx,
                });
            }
        }
        tracing::info!(session = session_id, "persistent session revived from snapshot");
        true
    }

    /// Close and remove a session. Fire-and-forget; use [`Self::close_and_wait`]
    /// when the caller needs to know the session thread actually stopped.
    /// An explicit close also drops the persistent snapshot — "done" means
    /// done (idle eviction keeps it; that is the revive path).
    pub fn close(&mut self, session_id: &str) {
        self.close_inner(session_id, true);
    }

    fn close_inner(&mut self, session_id: &str, drop_snapshot: bool) {
        if let Some(session) = self.sessions.remove(session_id) {
            let (tx, _rx) = oneshot::channel();
            let _ = session.cmd_tx.send(SessionCommand::Close { reply: tx });
        }
        if drop_snapshot {
            self.snapshots.delete(session_id);
        }
    }

    /// Close and wait (bounded) for the session thread to acknowledge. Returns
    /// true if the thread exited (or was already dead); false means it did not
    /// ack within the budget - the command stays queued and the thread will
    /// exit when its current (watchdog-bounded) command finishes.
    pub async fn close_and_wait(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.remove(session_id) {
            self.snapshots.delete(session_id);
            let (tx, rx) = oneshot::channel();
            if session.cmd_tx.send(SessionCommand::Close { reply: tx }).is_err() {
                return true; // thread already gone
            }
            match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
                Ok(Ok(())) => true,
                Ok(Err(_)) => true, // dropped sender = thread exiting
                Err(_) => false,
            }
        } else {
            false
        }
    }

    /// Evict expired sessions. Persistent sessions keep their snapshot: the
    /// eviction is invisible to the agent, the next command revives the id.
    /// Opportunistically ages out snapshots nobody revived.
    pub fn evict_expired(&mut self) {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.close_inner(&id, false);
        }
        self.snapshots.purge(SNAPSHOT_MAX_AGE_SECS);
    }

    /// Snapshot of live sessions — id + idle age only, most recently active
    /// first. The page URL lives inside the session thread; fetching it per
    /// entry would round-trip a State command per session, too heavy for a
    /// listing. Call [`Self::evict_expired`] first if the list must not
    /// include idle-but-not-yet-evicted sessions.
    pub fn list(&self) -> Vec<SessionListEntry> {
        let mut out: Vec<SessionListEntry> = self
            .sessions
            .iter()
            .map(|(id, s)| SessionListEntry {
                session_id: id.clone(),
                idle_secs: s.last_active.elapsed().as_secs(),
                expires_in_secs: if s.keepalive {
                    None
                } else {
                    Some(s.timeout.saturating_sub(s.last_active.elapsed()).as_secs())
                },
                keepalive: s.keepalive,
                persistent: s.persistent,
            })
            .collect();
        out.sort_by_key(|e| e.idle_secs);
        out
    }

    /// Live session count (call [`Self::evict_expired`] first if the number
    /// must not include idle-but-not-yet-evicted sessions).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

// ---------------------------------------------------------------------------
// Replay script generation
// ---------------------------------------------------------------------------

/// Embed a string in single quotes for bash: `'` → `'\''` (the standard
/// close-quote-escaped-quote-reopen idiom). Everything else is literal.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render a recorded action log (JSONL, as returned by the Export command)
/// as a runnable bash script that replays the session against the HTTP API
/// with plain curl — replay with zero model tokens and nothing to install.
///
/// Index-based actions replay against indexes from the ORIGINAL run's
/// `/state` output; if the page's element order changed, indexes may point
/// elsewhere. That's inherent to index-based replay — the script is a
/// starting point, auditable and editable.
/// Build the evaluate that writes a `{"local_storage":{..},"session_storage":{..}}`
/// map into the page's Web Storage. The JSON payloads are embedded as JS
/// string literals via serde (JSON escape rules are JS-compatible), so keys
/// or values containing quotes/newlines/CJK survive verbatim. Returns None
/// for a storage object with nothing in it.
fn inject_storage_js(storage: &Value) -> Option<String> {
    let ls = storage.get("local_storage").filter(|v| v.is_object())?;
    let ss = storage.get("session_storage").cloned().unwrap_or(serde_json::json!({}));
    let ls_lit = serde_json::to_string(&serde_json::to_string(ls).ok()?).ok()?;
    let ss_lit = serde_json::to_string(&serde_json::to_string(&ss).ok()?).ok()?;
    Some(format!(
        "(function(){{ var n=0; var ls=JSON.parse({ls_lit}); \
         for (var k in ls) {{ try {{ localStorage.setItem(k, String(ls[k])); n++; }} catch(e) {{}} }} \
         var ss=JSON.parse({ss_lit}); \
         for (var k in ss) {{ try {{ sessionStorage.setItem(k, String(ss[k])); n++; }} catch(e) {{}} }} \
         return n; }})()"
    ))
}

pub fn replay_bash(jsonl: &str, default_base: &str) -> String {
    let mut out = String::new();
    out.push_str("#!/usr/bin/env bash\n");
    out.push_str("# aginxbrowser session replay — recorded actions re-run as plain curl.\n");
    out.push_str("# No LLM in the loop: replay costs zero model tokens.\n");
    out.push_str("# Treat this file like credentials — it contains any cookies injected\n");
    out.push_str("# at session create.\n");
    out.push_str("set -eu\n");
    out.push_str(&format!("BASE=\"${{AGINXBROWSER_URL:-{default_base}}}\"\n"));
    out.push_str("POST() { curl -sS -X POST \"$BASE/$1\" -H 'Content-Type: application/json' -d \"$2\"; }\n\n");

    let mut sid_bound = false;
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        // Payload = JSON body, shell-single-quoted for the POST helper's -d "$2".
        let payload = |body: Value| shell_quote(&body.to_string());
        match v["action"].as_str().unwrap_or_default() {
            "create" => {
                let mut body = serde_json::json!({
                    "url": v["url"].clone(),
                    "cookies": v["cookies"].clone(),
                    "use_proxy": v["use_proxy"].as_bool().unwrap_or(false),
                });
                if v.get("storage").map(|s| s.is_object()).unwrap_or(false) {
                    body["storage"] = v["storage"].clone();
                }
                let body = payload(body);
                out.push_str(&format!(
                    "SID=$(POST session/create {body} | sed -n 's/.*\"session_id\":\"\\([^\"]*\\)\".*/\\1/p')\n"
                ));
                out.push_str("[ -n \"$SID\" ] || { echo \"session create failed\" >&2; exit 1; }\n");
                sid_bound = true;
            }
            "navigate" if sid_bound => {
                let body = payload(serde_json::json!({"url": v["url"].clone()}));
                out.push_str(&format!("POST \"session/$SID/navigate\" {body} > /dev/null\n"));
            }
            "click" if sid_bound => {
                let body = payload(serde_json::json!({"index": v["index"].clone()}));
                out.push_str(&format!("POST \"session/$SID/click\" {body} > /dev/null\n"));
            }
            "click_xy" if sid_bound => {
                let body = payload(serde_json::json!({"x": v["x"].clone(), "y": v["y"].clone()}));
                out.push_str(&format!("POST \"session/$SID/click_xy\" {body} > /dev/null\n"));
            }
            "drag" if sid_bound => {
                let body = payload(serde_json::json!({
                    "from": {"x": v["from_x"].clone(), "y": v["from_y"].clone()},
                    "to": {"x": v["to_x"].clone(), "y": v["to_y"].clone()},
                    "steps": v["steps"].clone(),
                }));
                out.push_str(&format!("POST \"session/$SID/drag\" {body} > /dev/null\n"));
            }
            "input" if sid_bound => {
                let body = payload(serde_json::json!({"index": v["index"].clone(), "text": v["text"].clone()}));
                out.push_str(&format!("POST \"session/$SID/input\" {body} > /dev/null\n"));
            }
            "scroll" if sid_bound => {
                let body = payload(serde_json::json!({
                    "direction": v["direction"].clone(),
                    "amount": v["amount"].clone(),
                }));
                out.push_str(&format!("POST \"session/$SID/scroll\" {body} > /dev/null\n"));
            }
            "viewport" if sid_bound => {
                let body = payload(serde_json::json!({
                    "width": v["width"].clone(),
                    "height": v["height"].clone(),
                    "mobile": v["mobile"].as_bool().unwrap_or(false),
                }));
                out.push_str(&format!("POST \"session/$SID/viewport\" {body} > /dev/null\n"));
            }
            "eval" if sid_bound => {
                let body = payload(serde_json::json!({"script": v["script"].clone()}));
                out.push_str(&format!("POST \"session/$SID/eval\" {body} > /dev/null\n"));
            }
            _ => {}
        }
    }
    if sid_bound {
        out.push_str("\necho '--- final state ---'\n");
        out.push_str("POST \"session/$SID/state\" '{}'\n");
        out.push_str("echo\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Session thread — owns Browser + Page
// ---------------------------------------------------------------------------

fn session_thread(
    _session_id: String,
    start_url: Option<String>,
    use_proxy: bool,
    cookies: Vec<String>,
    storage: Option<Value>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build session runtime");

    rt.block_on(async {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let browser = crate::server::build_browser(use_proxy, "", None)
                    .expect("failed to build session browser");
                // Inject cookies before navigation so a session can start
                // already logged-in (cookies gathered from a prior session via
                // the Cookies command, or hand-exported). Mirrors /fetch.
                if !cookies.is_empty() {
                    let target = start_url.as_deref().unwrap_or("");
                    crate::server::inject_cookies(&browser, &cookies, target);
                }
                let mut page = browser.new_page().await.expect("failed to create session page");

                // Replay log: every page-changing action this session took,
                // in order. In-memory only, dies with the session; exported
                // explicitly via the Export command. Declared before the
                // initial navigation below (which moves start_url).
                let mut recorder: Vec<RecordedAction> = vec![RecordedAction::Create {
                    url: start_url.clone(),
                    use_proxy,
                    cookies: cookies.clone(),
                    storage: storage.clone(),
                }];

                // Ring buffer of recent page console output, fed by
                // drain_console after every command (see Console command).
                let mut console_ring: std::collections::VecDeque<Value> =
                    std::collections::VecDeque::new();

                // Navigate to start URL if provided.
                // Page budget: every page this session walks counts (initial
                // navigation included). Bounds bulk walking; working one page
                // (state/scroll/eval/typing) stays free. See rate.rs.
                let mut pages_loaded: u32 = 0;
                if let Some(url) = start_url {
                    match page.goto(&url).await {
                        Ok(()) => pages_loaded += 1,
                        Err(e) => {
                            // session_create is fire-and-forget by design (the
                            // id returns before the thread starts navigating),
                            // so a failed initial navigation must not be
                            // silent: the agent sees it in session_console as
                            // an error entry — same observability contract as
                            // page console output (feedback ⑦ family). The
                            // session stays alive; navigating elsewhere works.
                            tracing::warn!("session: initial navigation failed: {}", e);
                            let ts_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            console_ring.push_back(serde_json::json!({
                                "ts_ms": ts_ms,
                                "level": "error",
                                "text": format!("initial navigation failed: {}", e),
                                "url": url,
                            }));
                        }
                    }
                }

                // Inject storage after landing so the entries are scoped to
                // the page's origin — this is what cookies can't carry (the
                // xinzao-class login token lives in localStorage, not the
                // cookie jar). Injection failure is non-fatal: an entry key
                // collision or a storage write error just means the session
                // starts logged out, like any fresh visit.
                if let Some(storage) = &storage {
                    if let Some(js) = inject_storage_js(storage) {
                        let v = page.evaluate(&js);
                        if let Some(n) = v.as_i64() {
                            tracing::info!("session: restored {n} storage entries");
                        }
                    }
                }

                // Element index → _nid mapping (rebuilt on each /state call).
                let mut element_map: HashMap<usize, u64> = HashMap::new();

                // Command loop. Between commands the JS event loop keeps
                // running (200ms slices) instead of freezing on a blocking
                // recv(): timers, fetch callbacks and promise chains must
                // progress while the session idles, exactly like a real
                // browser's main thread. Without the pump, async work
                // started by page scripts stalled until the next command -
                // WorkOS Radar's 5s worker-response window expired with its
                // timer un-pumped (measured 31s frozen).
                loop {
                    let cmd = tokio::select! {
                        biased;
                        cmd = cmd_rx.recv() => match cmd {
                            Some(c) => c,
                            None => break,
                        },
                        _ = page.pump_event_loop_slice(200) => continue,
                    };
                    match cmd {
                        SessionCommand::Navigate { url, reply } => {
                            // Budget first (local, free), then the per-domain
                            // rate gate — same stance as the stateless paths.
                            let result = match crate::rate::check_page_budget(pages_loaded)
                                .and_then(|_| crate::rate::check_domain(&url))
                            {
                                Err(reason) => Err(reason),
                                Ok(()) => match page.goto(&url).await {
                                    Ok(()) => {
                                        let final_url = page.url();
                                        let title = page
                                            .evaluate("document.title")
                                            .as_str()
                                            .filter(|s| !s.is_empty())
                                            .map(|s| s.to_string());
                                        element_map.clear();
                                        pages_loaded += 1;
                                        Ok(SessionNavResponse { url: final_url, title })
                                    }
                                    Err(e) => Err(format!("navigation failed: {}", e)),
                                },
                            };
                            recorder.push(RecordedAction::Navigate {
                                ok: result.is_ok(),
                                url,
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::SetContent { html, reply } => {
                            // Local bytes on the same load path as a
                            // navigation: base64 data URL (no escaping
                            // games with # , % in the HTML), decoded by
                            // the nav layer. pages_loaded stays put —
                            // nothing walked the network, so the page
                            // budget doesn't spend here.
                            use base64::{engine::general_purpose::STANDARD, Engine as _};
                            let url = format!(
                                "data:text/html;base64,{}",
                                STANDARD.encode(html.as_bytes())
                            );
                            let result = match page.goto(&url).await {
                                Ok(()) => {
                                    let title = page
                                        .evaluate("document.title")
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    element_map.clear();
                                    Ok(serde_json::json!({
                                        "bytes": html.len(),
                                        "title": title,
                                    }))
                                }
                                Err(e) => Err(format!("setContent failed: {}", e)),
                            };
                            recorder.push(RecordedAction::SetContent {
                                ok: result.is_ok(),
                                html,
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::State { reply } => {
                            element_map.clear();
                            let result = extract_indexed_state(&mut page, &mut element_map);
                            let _ = reply.send(result);
                        }

                        SessionCommand::Click { index, reply } => {
                            // A click that changes the page is a page walk and
                            // spends the budget; one that only toggles state
                            // (a checkbox, a menu) is free.
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    click_by_index(&mut page, &element_map, index)
                                        .await
                                        .map(|resp| {
                                            if resp.url != before {
                                                pages_loaded += 1;
                                            }
                                            resp
                                        })
                                }
                            };
                            recorder.push(RecordedAction::Click { index, ok: result.is_ok() });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Input { index, text, full_events, reply } => {
                            let result = input_by_index(&mut page, &element_map, index, &text, full_events);
                            recorder.push(RecordedAction::Input {
                                index,
                                text,
                                ok: result.as_ref().map(|v| v.get("filled").and_then(Value::as_bool).unwrap_or(false)).unwrap_or(false),
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::ClickXY { x, y, button, click_count, reply } => {
                            // A coordinate click can navigate exactly like an
                            // indexed one, so it spends the same page budget.
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    click_xy(&mut page, x, y, &button, click_count).await;
                                    let _ = page.process_pending_navigation().await;
                                    if page.url() != before {
                                        pages_loaded += 1;
                                    }
                                    Ok(serde_json::json!({
                                        "url": page.url(),
                                        "x": x,
                                        "y": y,
                                    }).to_string())
                                }
                            };
                            recorder.push(RecordedAction::ClickXY { x, y, ok: result.is_ok() });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Drag { from_x, from_y, to_x, to_y, steps, delay_ms, reply } => {
                            let steps = steps.clamp(1, 200);
                            let delay_ms = delay_ms.min(1000);
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    drag_xy(&mut page, from_x, from_y, to_x, to_y, steps, delay_ms).await;
                                    let _ = page.process_pending_navigation().await;
                                    if page.url() != before {
                                        pages_loaded += 1;
                                    }
                                    Ok(serde_json::json!({
                                        "url": page.url(),
                                        "from": {"x": from_x, "y": from_y},
                                        "to": {"x": to_x, "y": to_y},
                                        "steps": steps,
                                    }).to_string())
                                }
                            };
                            recorder.push(RecordedAction::Drag { from_x, from_y, to_x, to_y, steps });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Scroll { direction, amount, reply } => {
                            let dy = match direction {
                                ScrollDirection::Up => -(amount as i32) * 100,
                                ScrollDirection::Down => (amount as i32) * 100,
                            };
                            let js = format!("window.scrollBy(0, {})", dy);
                            page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
                            recorder.push(RecordedAction::Scroll {
                                direction: match direction {
                                    ScrollDirection::Up => "up".to_string(),
                                    ScrollDirection::Down => "down".to_string(),
                                },
                                amount,
                            });
                            let _ = reply.send(Ok(true));
                        }

                        SessionCommand::GetViewport { reply } => {
                            let _ = reply.send(Ok(page.viewport_override()));
                        }

                        SessionCommand::Viewport { width, height, mobile, reply } => {
                            // Unspecified dimensions keep the current ones
                            // (Chrome's setDeviceMetricsOverride rule), so
                            // read the live values before overriding.
                            let probe = "(function(){return [innerWidth, innerHeight]})()";
                            let current = {
                                let v = page.evaluate_with_timeout(probe, crate::page::INTERACTION_EVAL_TIMEOUT);
                                match (v.get(0).and_then(Value::as_f64), v.get(1).and_then(Value::as_f64)) {
                                    (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w as f32, h as f32),
                                    _ => (1920.0, 1000.0),
                                }
                            };
                            let w = width.map(|v| v as f32).unwrap_or(current.0);
                            let h = height.map(|v| v as f32).unwrap_or(current.1);
                            page.set_viewport_override(w, h, mobile, None);
                            let val = serde_json::json!({
                                "width": w as u32,
                                "height": h as u32,
                                "mobile": mobile,
                            });
                            recorder.push(RecordedAction::Viewport { width: Some(w as u32), height: Some(h as u32), mobile });
                            let _ = reply.send(Ok(val));
                        }

                        SessionCommand::Eval { script, reply } => {
                            let outcome = page.evaluate_async_checked(&script).await;
                            recorder.push(RecordedAction::Eval { script });
                            // Drain any JS-initiated navigation the script
                            // started (location.href / form submit) so the
                            // session's current URL moves with it — same
                            // policy as click_by_index below.
                            let _ = page.process_pending_navigation().await;
                            let _ = reply.send(outcome.map_err(SessionError::Eval));
                        }

                        SessionCommand::Storage { reply } => {
                            let collect = "(function(){ var ls={}, ss={}; \
                                try { for (var i=0;i<localStorage.length;i++){ var k=localStorage.key(i); ls[k]=localStorage.getItem(k); } } catch(e) {} \
                                try { for (var i=0;i<sessionStorage.length;i++){ var k=sessionStorage.key(i); ss[k]=sessionStorage.getItem(k); } } catch(e) {} \
                                return {local_storage: ls, session_storage: ss}; })()";
                            let v = page.evaluate(collect);
                            let payload = serde_json::json!({
                                "url": page.url(),
                                "local_storage": v.get("local_storage").cloned().unwrap_or(serde_json::json!({})),
                                "session_storage": v.get("session_storage").cloned().unwrap_or(serde_json::json!({})),
                            });
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Console { filter, reply } => {
                            drain_console(&page, &mut console_ring);
                            let total = console_ring.len();
                            let mut messages: Vec<Value> = console_ring
                                .iter()
                                .filter(|e| filter.matches(e))
                                .cloned()
                                .collect();
                            let matched = messages.len();
                            if let Some(cap) = filter.limit {
                                if messages.len() > cap {
                                    messages = messages.split_off(messages.len() - cap);
                                }
                            }
                            let payload = serde_json::json!({
                                "url": page.url(),
                                "total": total,
                                "matched": matched,
                                "messages": messages,
                            });
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Dialog { action, prompt_text, reply } => {
                            let out = match action.as_str() {
                                "list" => {
                                    drain_console(&page, &mut console_ring);
                                    let (accept, prompt_text) = page.inner.dialog_policy();
                                    let dialogs: Vec<Value> = console_ring
                                        .iter()
                                        .filter(|e| e["level"] == "dialog")
                                        .cloned()
                                        .collect();
                                    Ok(serde_json::json!({
                                        "policy": if accept { "accept" } else { "dismiss" },
                                        "prompt_text": prompt_text,
                                        "dialogs": dialogs,
                                    }).to_string())
                                }
                                "accept" | "dismiss" => {
                                    page.inner.set_dialog_policy(action == "accept", prompt_text);
                                    let (accept, prompt_text) = page.inner.dialog_policy();
                                    Ok(serde_json::json!({
                                        "policy": if accept { "accept" } else { "dismiss" },
                                        "prompt_text": prompt_text,
                                    }).to_string())
                                }
                                other => Err(format!(
                                    "unknown dialog action {:?} (expected list | accept | dismiss)",
                                    other
                                )),
                            };
                            let _ = reply.send(out);
                        }

                        SessionCommand::Cookies { reply } => {
                            let url_str = page.url();
                            // Full Set-Cookie form (Domain/flags included) so
                            // snapshot/clone restores cross-subdomain login
                            // state: bare name=value pairs re-anchor at
                            // whatever URL the new session opens, losing
                            // every sibling-domain cookie.
                            let cookies: Vec<String> = page
                                .context
                                .cookie_jar
                                .get_all_cookies()
                                .into_iter()
                                .map(|c| {
                                    let mut s = format!(
                                        "{}={}; Domain={}; Path={}",
                                        c.name, c.value, c.domain, c.path
                                    );
                                    if c.secure {
                                        s.push_str("; Secure");
                                    }
                                    if c.http_only {
                                        s.push_str("; HttpOnly");
                                    }
                                    if !c.same_site.is_empty() {
                                        s.push_str(&format!("; SameSite={}", c.same_site));
                                    }
                                    s
                                })
                                .collect();
                            let resp = serde_json::json!({ "url": url_str, "cookies": cookies });
                            let _ = reply.send(Ok(resp.to_string()));
                        }

                        SessionCommand::Export { reply } => {
                            let jsonl = recorder
                                .iter()
                                .filter_map(|a| serde_json::to_string(a).ok())
                                .collect::<Vec<_>>()
                                .join("\n");
                            let _ = reply.send(Ok(jsonl));
                        }

                        SessionCommand::Network { media_only, include_bodies, url_contains, body_max_chars, reply } => {
                            page.inner.sync_js_network_events();
                            let payload = if media_only {
                                // Media elements and player iframes are never
                                // fetched by the engine (no media/frame
                                // loading), so they cannot surface in the
                                // network log — collect their DOM sources and
                                // merge them in as candidates.
                                let dom = page
                                    .evaluate(DOM_MEDIA_SCRIPT)
                                    .as_str()
                                    .unwrap_or("[]")
                                    .to_string();
                                let events = &page.inner.network_events;
                                let mut media = crate::har::media_entries(events);
                                merge_dom_candidates(&mut media, &dom);
                                let body_of = |rid: &str| page.inner.get_response_body(rid);
                                media.extend(crate::har::media_from_bodies(events, &body_of));
                                serde_json::json!({
                                    "url": page.url(),
                                    "media": media,
                                })
                            } else {
                                let events = &page.inner.network_events;
                                let mut payload = serde_json::json!({
                                    "url": page.url(),
                                    "total": events.len(),
                                    "requests": crate::har::compact_events(events),
                                });
                                // Anti-bot challenges answer 200, so they
                                // hide among successful rows — surface the
                                // count at the top level so an agent that
                                // just asks "did we get punished" doesn't
                                // have to scan every URL.
                                let challenges = events
                                    .iter()
                                    .filter(|e| crate::har::challenge_kind(&e.url).is_some())
                                    .count();
                                if challenges > 0 {
                                    payload["challenges"] = json!(challenges);
                                }
                                if include_bodies {
                                    let body_of = |rid: &str| page.inner.get_response_body(rid);
                                    let filters = url_contains
                                        .as_deref()
                                        .map(|s| vec![s.to_string()])
                                        .unwrap_or_default();
                                    payload["xhr"] = json!(crate::har::xhr_bodies(
                                        events, &filters, body_max_chars, &body_of
                                    ));
                                }
                                payload
                            };
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Har { reply } => {
                            page.inner.sync_js_network_events();
                            let title = page
                                .evaluate("document.title")
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let events = &page.inner.network_events;
                            let body_of = |rid: &str| page.inner.get_response_body(rid);
                            let har = crate::har::har_log(&title, events, &body_of);
                            let _ = reply.send(Ok(har.to_string()));
                        }

                        SessionCommand::Screenshot { width, height, full_page, selector, selector_all, reply } => {
                            #[cfg(feature = "screenshot")]
                            {
                                let html = page.content();
                                let url = page.url();
                                // Default to the live viewport so a
                                // session_viewport override is what the
                                // pixels show, not the render default.
                                let vw = page.evaluate_with_timeout(
                                    "innerWidth",
                                    crate::page::INTERACTION_EVAL_TIMEOUT,
                                );
                                let vh = page.evaluate_with_timeout(
                                    "innerHeight",
                                    crate::page::INTERACTION_EVAL_TIMEOUT,
                                );
                                let w = width
                                    .unwrap_or_else(|| vw.as_f64().unwrap_or(1280.0) as u32)
                                    .max(1);
                                let h = height
                                    .unwrap_or_else(|| vh.as_f64().unwrap_or(800.0) as u32)
                                    .max(1);
                                let resources = crate::screenshot::prefetch_render_resources(
                                    &page, &url, &html, w as f32,
                                )
                                .await;
                                let result = crate::screenshot::render_html_to_png_diting(
                                    &html,
                                    &url,
                                    w,
                                    h,
                                    1.0,
                                    full_page,
                                    selector.as_deref(),
                                    selector_all,
                                    Some(&resources),
                                )
                                .map_err(|e| format!("screenshot failed: {e}"))
                                .map(|rendered| {
                                    use base64::{engine::general_purpose::STANDARD, Engine as _};
                                    serde_json::json!({
                                        "url": url,
                                        "width": rendered.pixel_width,
                                        "height": rendered.pixel_height,
                                        "image_base64": STANDARD.encode(&rendered.png),
                                        "format": "png",
                                    })
                                    .to_string()
                                });
                                let _ = reply.send(result);
                            }
                            #[cfg(not(feature = "screenshot"))]
                            {
                                let _ = (
                                    &width, &height, &full_page, &selector, &selector_all,
                                );
                                let _ = reply.send(Err(
                                    "session screenshot requires the `screenshot` feature"
                                        .to_string(),
                                ));
                            }
                        }

                        SessionCommand::Wait { selector, predicate, timeout_ms, reply } => {
                            let result = if selector.is_some() == predicate.is_some() {
                                Err("wait takes exactly one of `selector` or `predicate`"
                                    .to_string())
                            } else {
                                // Clamp so a stray request can't pin the
                                // session thread (Close included) for long.
                                let timeout_ms = timeout_ms.clamp(1, 120_000);
                                let started = std::time::Instant::now();
                                let deadline = started + Duration::from_millis(timeout_ms);
                                let mut last_error: Option<String> = None;
                                loop {
                                    let detail = if let Some(sel) = &selector {
                                        let escaped =
                                            sel.replace('\\', "\\\\").replace('\'', "\\'");
                                        let js = format!(
                                            "(function(){{ var el = document.querySelector('{}'); \
                                             if (!el) return null; \
                                             return {{tag: el.tagName, \
                                             text: (el.textContent || '').trim().slice(0, 200)}}; }})()",
                                            escaped
                                        );
                                        let v = page.evaluate_with_timeout(
                                            &js,
                                            crate::page::INTERACTION_EVAL_TIMEOUT,
                                        );
                                        if v.is_null() { None } else { Some(v) }
                                    } else {
                                        let pred = predicate.as_deref().unwrap_or("false");
                                        let js = format!(
                                            "(function(){{ try {{ var v = ({pred}); \
                                             if (!v) return {{truthy: false}}; \
                                             return {{truthy: true, value: \
                                             (typeof v === 'object' && v !== null \
                                             ? JSON.stringify(v) : String(v)).slice(0, 200)}}; \
                                             }} catch (e) {{ return {{truthy: false, \
                                             error: String(e).slice(0, 200)}}; }} }})()"
                                        );
                                        let v = page.evaluate_with_timeout(
                                            &js,
                                            crate::page::INTERACTION_EVAL_TIMEOUT,
                                        );
                                        if v.get("truthy").and_then(|t| t.as_bool()) == Some(true) {
                                            v.get("value").cloned()
                                        } else {
                                            last_error = v
                                                .get("error")
                                                .and_then(|e| e.as_str())
                                                .map(str::to_string)
                                                .or(last_error);
                                            None
                                        }
                                    };
                                    if let Some(detail) = detail {
                                        break Ok(serde_json::json!({
                                            "matched": true,
                                            "elapsed_ms": started.elapsed().as_millis() as u64,
                                            "detail": detail,
                                        })
                                        .to_string());
                                    }
                                    if std::time::Instant::now() >= deadline {
                                        let what = selector
                                            .as_deref()
                                            .map(|s| format!("selector \"{s}\""))
                                            .unwrap_or_else(|| {
                                                format!("predicate \"{}\"",
                                                        predicate.as_deref().unwrap_or(""))
                                            });
                                        let mut msg =
                                            format!("timeout after {timeout_ms}ms waiting for {what}");
                                        if let Some(e) = last_error {
                                            msg.push_str(&format!(" (last error: {e})"));
                                        }
                                        break Err(msg);
                                    }
                                    // Drive the page's loop while waiting —
                                    // fetches/timers only advance when pumped,
                                    // and the slice parks when quiescent so
                                    // idle pages don't spin hot.
                                    page.pump_event_loop_slice(150).await;
                                }
                            };
                            let _ = reply.send(result);
                        }

                        SessionCommand::Close { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }

                    // Pump the JS event loop briefly after every command.
                    // Commands that only evaluate synchronously (Eval
                    // returning a non-Promise, Scroll, State) can still have
                    // started async work — a fetch fired from a submit/click
                    // handler, promise chains, timers — which needs
                    // event-loop turns to progress. Without this the work
                    // stranded until the next navigation (React server-action
                    // fetches never resolved). Returns immediately when the
                    // loop is idle, so quiescent pages pay nothing; busy pages
                    // get up to 1.5s of drain per command.
                    page.settle_until_idle(1500).await;
                    drain_console(&page, &mut console_ring);
                }
            })
            .await;
    });
}

// ---------------------------------------------------------------------------
// DOM media candidates
// ---------------------------------------------------------------------------

/// Collect playback-relevant sources the engine never fetches (media
/// elements, player iframes), as a JSON array of `{url, tag}`. Relative URLs
/// resolve against the page location; duplicates collapse.
const DOM_MEDIA_SCRIPT: &str = r#"(function(){
    var out = [];
    var seen = {};
    function add(u, tag) {
        if (!u) return;
        try { u = new URL(String(u), location.href).href; } catch (e) { return; }
        if (!seen[u]) { seen[u] = 1; out.push({ url: u, tag: tag }); }
    }
    var els = document.querySelectorAll('video,audio,source,iframe');
    for (var i = 0; i < els.length; i++) {
        var e = els[i];
        add(e.getAttribute('src'), e.tagName.toLowerCase());
    }
    return JSON.stringify(out);
})()"#;

/// Merge DOM-observed candidates into the network-derived media list. A
/// candidate the network log already confirms (same URL ignoring query —
/// players append auth/expiry tokens the markup never carries) is dropped;
/// iframes surface as kind "iframe" (player pages to navigate or sniff
/// inside, not playable URLs themselves); the rest must classify as media
/// or they are dropped. These are candidates, not confirmations — `via`
/// says which side produced each entry.
fn merge_dom_candidates(media: &mut Vec<Value>, dom_json: &str) {
    let dom: Vec<Value> = match serde_json::from_str(dom_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    let bare = |u: &str| {
        u.split(['?', '#'])
            .next()
            .unwrap_or(u)
            .to_ascii_lowercase()
    };
    let confirmed: std::collections::HashSet<String> = media
        .iter()
        .filter_map(|m| m["url"].as_str())
        .map(bare)
        .collect();
    for cand in dom {
        let Some(url) = cand["url"].as_str().map(str::to_string) else {
            continue;
        };
        let tag = cand["tag"].as_str().unwrap_or("").to_string();
        if confirmed.contains(&bare(&url)) {
            continue;
        }
        let kind = if tag == "iframe" {
            "iframe".to_string()
        } else {
            match crate::har::media_kind(&url, None) {
                Some(k) => k.to_string(),
                None => continue,
            }
        };
        media.push(serde_json::json!({
            "url": url,
            "kind": kind,
            "via": "dom",
            "tag": tag,
        }));
    }
}

// ---------------------------------------------------------------------------
// Indexed state extraction
// ---------------------------------------------------------------------------

/// JS script that queries all interactive elements, assigns sequential indexes,
/// stores the `_nid` mapping in `window.__session_element_map`, and returns a
/// JSON array of element descriptors.
const STATE_SCRIPT: &str = r#"
(function() {
    var interactive = Array.prototype.slice.call(document.querySelectorAll(
        'a, button, input, select, textarea, [role="button"], [role="link"], [onclick], [tabindex]'
    ));
    // Elements with a JS-bound click listener (jQuery .click(), addEventListener)
    // are invisible to the selector above — a div-classed login button leaves the
    // agent with no indexed way to click it. The engine's own event registry
    // already knows every listener target, so union those nids in and restore
    // document order.
    try {
        var reg = (typeof _eventRegistry === 'undefined') ? null : _eventRegistry;
        if (reg) {
            var seenNids = {};
            for (var s = 0; s < interactive.length; s++) {
                var n0 = interactive[s]._nid;
                if (n0 !== undefined) seenNids[n0] = true;
            }
            for (var nid in reg) {
                if (seenNids[nid]) continue;
                var rec = reg[nid];
                if (!rec || !rec.click || !rec.click.length) continue;
                var bound = globalThis._wrap && globalThis._wrap(parseInt(nid, 10));
                if (bound && bound.tagName) {
                    interactive.push(bound);
                    seenNids[nid] = true;
                }
            }
            interactive.sort(function(a, b) {
                if (!a.compareDocumentPosition || !b.compareDocumentPosition) return 0;
                var p = a.compareDocumentPosition(b);
                return (p & 4) ? -1 : ((p & 2) ? 1 : 0);
            });
        }
    } catch (e) {}
    var elements = [];
    var indexMap = {};
    var idx = 0;
    for (var i = 0; i < interactive.length; i++) {
        var el = interactive[i];
        var style = el.offsetWidth === 0 && el.offsetHeight === 0;
        if (style) continue;
        var box = el.getBoundingClientRect();
        var info = {
            index: idx,
            tag: el.tagName.toLowerCase(),
            text: (el.innerText || '').trim().substring(0, 100),
            x: Math.round(box.x), y: Math.round(box.y),
            w: Math.round(box.width), h: Math.round(box.height),
            attrs: {}
        };
        var attrNames = ['id', 'class', 'href', 'type', 'name', 'value',
                         'placeholder', 'aria-label', 'title', 'src', 'alt', 'role'];
        for (var j = 0; j < attrNames.length; j++) {
            var v = el.getAttribute(attrNames[j]);
            if (v !== null) info.attrs[attrNames[j]] = v;
        }
        // Control state the agent would otherwise need a follow-up eval for:
        // checked (checkbox/radio), disabled, and the select's current option.
        try {
            var elType = (el.getAttribute('type') || '').toLowerCase();
            if (el.tagName === 'INPUT' && (elType === 'checkbox' || elType === 'radio') && el.checked) {
                info.attrs.checked = 'checked';
            }
            if (el.disabled) info.attrs.disabled = 'disabled';
            if (el.tagName === 'SELECT' && el.selectedIndex >= 0 && el.options[el.selectedIndex]) {
                var opt = el.options[el.selectedIndex];
                var optVal = opt.getAttribute('value') !== null ? opt.getAttribute('value') : (opt.textContent || '');
                info.attrs.selected = optVal.trim().substring(0, 40);
            }
        } catch (e) {}
        if (el._nid !== undefined) {
            indexMap[idx] = el._nid;
        }
        elements.push(info);
        idx++;
    }
    window.__session_element_map = indexMap;
    return JSON.stringify({url: location.href, title: document.title,
                           viewport: {w: window.innerWidth, h: window.innerHeight},
                           elements: elements});
})()
"#;

fn extract_indexed_state(
    page: &mut Page,
    element_map: &mut HashMap<usize, u64>,
) -> Result<String, String> {
    let val = page.evaluate(STATE_SCRIPT);
    let json_str = match val.as_str() {
        Some(s) => s.to_string(),
        None => return Err("state extraction returned non-string".into()),
    };

    // Parse the JSON to extract element_map, then format as compact text.
    let parsed: Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("state parse error: {}", e))?;

    // Build element_map from the JS-side indexMap.
    let map_val = page.evaluate("JSON.stringify(window.__session_element_map)");
    if let Some(map_str) = map_val.as_str() {
        if let Ok(map_obj) = serde_json::from_str::<HashMap<String, u64>>(map_str) {
            for (k, v) in map_obj {
                if let Ok(idx) = k.parse::<usize>() {
                    element_map.insert(idx, v);
                }
            }
        }
    }

    // Format compact text output.
    let url = parsed.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = parsed.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    out.push_str(&format!("url={}\n", url));
    out.push_str(&format!("title={}\n", title));
    // Viewport size so the agent can tell which rects are on-screen
    // (scroll down / scroll to element before clicking off-viewport ones).
    if let Some(vp) = parsed.get("viewport") {
        out.push_str(&format!(
            "viewport={}x{}\n\n",
            vp.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
            vp.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
        ));
    } else {
        out.push('\n');
    }

    if let Some(elements) = parsed.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            let idx = el.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let tag = el.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            let text = el.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let attrs = el.get("attrs").and_then(|v| v.as_object());

            let mut attr_parts = Vec::new();
            if let Some(attrs) = attrs {
                for (k, v) in attrs {
                    let vs = v.as_str().unwrap_or("");
                    // Truncate long class values. Slice by chars, not bytes —
                    // multi-byte UTF-8 (—, CJK) panics on byte indexing.
                    if k == "class" && vs.chars().count() > 50 {
                        let t: String = vs.chars().take(50).collect();
                        attr_parts.push(format!("{}=\"{}…\"", k, t));
                    } else {
                        attr_parts.push(format!("{}=\"{}\"", k, vs));
                    }
                }
            }
            let attr_str = if attr_parts.is_empty() {
                String::new()
            } else {
                format!(" {}", attr_parts.join(" "))
            };
            // Page-relative rect (viewport coords: y is relative to the
            // current scroll position) — lets the agent "see where it is"
            // before clicking or scrolling.
            let rect = format!(
                " rect=[{},{},{}x{}]",
                el.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
            );

            if text.is_empty() {
                out.push_str(&format!("[{}] <{}{}{} />\n", idx, tag, attr_str, rect));
            } else {
                let display_text = if text.chars().count() > 80 {
                    let t: String = text.chars().take(80).collect();
                    format!("{}…", t)
                } else {
                    text.to_string()
                };
                out.push_str(&format!("[{}] <{}{}{}>{}</{}>\n", idx, tag, attr_str, rect, display_text, tag));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Click / Input by index
// ---------------------------------------------------------------------------

use crate::diting_cdp::domains::input::{
    mouse_down_js, mouse_move_js, mouse_up_js, mouse_button_code, mouse_button_mask,
    INPUT_HELPERS,
};

fn eval_interaction(page: &mut Page, js: &str) {
    page.evaluate_with_timeout(js, crate::page::INTERACTION_EVAL_TIMEOUT);
}

/// Click at viewport coordinates through the real mouse chain — same JS the
/// CDP bridge dispatches, so pages can't tell the two apart.
async fn click_xy(page: &mut Page, x: f64, y: f64, button: &str, click_count: u32) {
    let code = mouse_button_code(button);
    let mask = mouse_button_mask(button);
    eval_interaction(page, INPUT_HELPERS);
    eval_interaction(
        page,
        &mouse_down_js(x, y, code, mask, click_count as u64, 0),
    );
    eval_interaction(page, &mouse_up_js(x, y, code, click_count as u64, 0));
}

/// Press → interpolated mousemoves (buttons=1 held, delay between steps so
/// mousemove-driven widgets can keep up) → release. The intermediate moves
/// are the whole point: a marker drag only tracks when the page sees the
/// pointer travel, not a teleporting cursor.
async fn drag_xy(
    page: &mut Page,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    steps: u32,
    delay_ms: u64,
) {
    eval_interaction(page, INPUT_HELPERS);
    eval_interaction(page, &mouse_down_js(from_x, from_y, 0, 1, 1, 0));
    let n = steps as f64;
    for i in 1..=steps {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let k = i as f64 / n;
        let x = from_x + (to_x - from_x) * k;
        let y = from_y + (to_y - from_y) * k;
        eval_interaction(page, &mouse_move_js(x, y, 1, 0));
    }
    eval_interaction(page, &mouse_up_js(to_x, to_y, 0, 1, 0));
}

async fn click_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
) -> Result<SessionClickResponse, String> {
    let nid = *element_map.get(&index).ok_or_else(|| format!("invalid index: {}", index))?;
    let js = format!(
        "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el) {{ el.scrollIntoView({{block:'center'}}); el.click(); return true; }} return false; }})()",
        nid
    );
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    let clicked = result.as_bool().unwrap_or(false);
    // Drain any JS-initiated navigation the click started (location.href /
    // form.submit) so the returned URL reflects the post-click page — matches
    // the firecrawl /v1/scrape click handling.
    let _ = page.process_pending_navigation().await;
    if clicked {
        // Wait for quiescence, not a fixed slice: a client-side route
        // transition (RSC fetch → flight parse → render → pushState) only
        // counts as done when the loop drains. Capped so interval-heavy
        // pages can't pin the command.
        page.settle_until_idle(5000).await;
    }
    let url = page.url();
    let text_after = page
        .evaluate("document.body.innerText")
        .as_str()
        .map(|s| s.chars().take(2000).collect::<String>());
    Ok(SessionClickResponse { url, clicked, text_after })
}

fn input_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
    text: &str,
    full_events: bool,
) -> Result<Value, String> {
    let nid = *element_map.get(&index).ok_or_else(|| format!("invalid index: {}", index))?;
    // Escape single quotes in text.
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    // React/Vue controlled inputs: assigning `el.value` directly goes through
    // React's _valueTracker own-property setter, which records the new value -
    // the following `input` event then compares equal and React swallows it
    // (onChange never fires). Reset the tracker and use the prototype setter
    // so the dispatched event registers as a real change. The response carries
    // the filled element's identity + value readback so a stale element_map
    // (page re-rendered between state and input) is visible in the reply
    // instead of silently typing into the wrong field.
    let set_value = "if (el._valueTracker) el._valueTracker.setValue(''); var p = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value') || Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value');";
    let js_body: String = if full_events {
        // Strict listeners key on keyboard events (keypress-to-submit login
        // forms, masked inputs). Build the value one character at a time with
        // the full keydown/keypress/input/keyup cycle per character, then a
        // single trailing change.
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); {set_value} var text = '{text}'; var cur = ''; for (var i = 0; i < text.length; i++) {{ var ch = text[i]; var kc = ch.charCodeAt(0); var kev = function(t) {{ return new KeyboardEvent(t, {{key: ch, keyCode: kc, which: kc, bubbles: true}}); }}; el.dispatchEvent(kev('keydown')); if (p && p.set) p.set.call(el, cur + ch); else el.value = cur + ch; cur = cur + ch; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(kev('keypress')); el.dispatchEvent(kev('keyup')); }} el.dispatchEvent(new Event('change', {{bubbles: true}})); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
            set_value = set_value,
            text = escaped,
        )
    } else {
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); {set_value} if (p && p.set) p.set.call(el, '{text}'); else el.value = '{text}'; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
            set_value = set_value,
            text = escaped,
        )
    };
    let result = page.evaluate_with_timeout(&js_body, crate::page::INTERACTION_EVAL_TIMEOUT);
    let parsed = result
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({"filled": false}));
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (WorkOS Radar collector frozen 31s; the same bug class the
    /// upstream engine fixed as v0.2.1 "MCP pumps the page task queue between
    /// tool calls"): while a session sits idle between commands, its page's
    /// timers, microtasks and interval callbacks must keep firing like a real
    /// browser's main thread. Arms a timer, a promise chain and an interval,
    /// stays idle well past their deadlines with no command in flight, then
    /// reads the markers back. A session that parks on a blocking recv()
    /// instead of pumping 200ms slices leaves all three markers unset.
    #[tokio::test]
    async fn idle_session_keeps_timers_and_microtasks_firing() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);

        let armed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: r#"(function() {
                    window.__marks = {timeout: false, micro: false, ticks: 0};
                    setTimeout(function() { window.__marks.timeout = true; }, 400);
                    Promise.resolve().then(function() { window.__marks.micro = true; });
                    setInterval(function() { window.__marks.ticks++; }, 200);
                    return 'armed';
                })()"#
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(armed.as_str().unwrap_or(""), "armed");

        // Idle gap: no command sent for well past the 400ms timer deadline.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let state = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "JSON.stringify(window.__marks)".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");

        let json: Value = serde_json::from_str(state.as_str().unwrap_or("null")).unwrap_or(Value::Null);
        let timeout = json.get("timeout").and_then(|v| v.as_bool()).unwrap_or(false);
        let micro = json.get("micro").and_then(|v| v.as_bool()).unwrap_or(false);
        let ticks = json.get("ticks").and_then(|v| v.as_i64()).unwrap_or(0);
        // 1.5s idle at a 200ms interval ≈ 7 ticks; 3 is a safe floor that
        // still proves sustained pumping (one post-command drain gives 0-1).
        assert!(timeout, "setTimeout must fire during the idle gap");
        assert!(micro, "promise chain must settle during the idle gap");
        assert!(ticks >= 3, "interval must keep firing while idle, got {ticks} ticks");
    }

    /// Session errors must be machine-readable: an agent has to tell "the
    /// session is gone — recreate it" from "this step failed — retry it"
    /// without parsing prose (real-device feedback ①). Also pins the
    /// acceptance rule from the same report: once an expiry error fires, the
    /// id stays consistently dead — the next call answers SESSION_NOT_FOUND,
    /// never a half-alive success.
    #[tokio::test]
    async fn session_errors_carry_codes_and_expiry_closes_consistently() {
        let mut mgr = SessionManager::new();

        // Unknown id: SESSION_NOT_FOUND with the recreate hint.
        let err = mgr
            .send(&"s_missing".to_string(), |reply| SessionCommand::State { reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
        assert_eq!(err.to_value()["hint"], "call session_create to recreate");

        // Command failure inside a LIVE session: COMMAND_FAILED, no recreate
        // hint, and the session survives (a retry is safe).
        let sid = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
        let err = mgr
            .send(&sid, |reply| SessionCommand::Click { index: 99_999, reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "COMMAND_FAILED");
        assert!(err.to_value().get("hint").is_none());
        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");

        // Expired: SESSION_EXPIRED + destructive close, then the same id
        // answers SESSION_NOT_FOUND — the two conclusions agree.
        let sid = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
        mgr.sessions.get_mut(&sid).unwrap().last_active = Instant::now() - Duration::from_secs(3600);
        let err = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_EXPIRED");
        assert_eq!(err.to_value()["hint"], "call session_create to recreate");
        assert!(!mgr.sessions.contains_key(&sid), "expiry must close the session");
        let err = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    /// Regression (obscura #618 class): an eval whose script clicks a submit
    /// button must leave the session on the form's action URL — the click
    /// stores a pending JS navigation that the Eval command drains (same
    /// policy as click_by_index).
    #[tokio::test]
    async fn eval_submit_click_navigates_the_session() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /form",
                "<html><body><form method='POST' action='/done'>\
                 <input name='q' value='hello'>\
                 <button type='submit' id='go'>Go</button></form></body></html>",
            ),
            ("POST /done", "<html><body>submitted ok</body></html>"),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/form")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        let clicked = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.querySelector('#go').click(); 'clicked'".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(clicked.as_str().unwrap_or(""), "clicked");

        let href = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "location.href".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert!(
            href.as_str().unwrap_or("").ends_with("/done"),
            "session must follow the submit navigation, got {}",
            href
        );
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
    }

    #[tokio::test]
    async fn viewport_command_pins_viewport_across_navigation() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /m",
                "<html><head><style>@media (max-width:600px){body{color:#010203}}</style>\
                 </head><body><p>m</p></body></html>",
            ),
            (
                "GET /n",
                "<html><head><style>@media (max-width:600px){body{color:#010203}}</style>\
                 </head><body><p>n</p></body></html>",
            ),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/m")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        let vp = mgr
            .send(&sid, |reply| SessionCommand::Viewport {
                width: Some(375),
                height: Some(667),
                mobile: true,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(vp["width"].as_f64(), Some(375.0));
        assert_eq!(vp["height"].as_f64(), Some(667.0));
        assert_eq!(vp["mobile"].as_bool(), Some(true));

        let matches = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "matchMedia('(max-width:600px)').matches && matchMedia('(pointer:coarse)').matches".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(matches, serde_json::json!(true));

        // Omitted dimensions keep the current ones (Chrome's
        // setDeviceMetricsOverride rule): only the flag moves here.
        let vp = mgr
            .send(&sid, |reply| SessionCommand::Viewport {
                width: None,
                height: None,
                mobile: false,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(vp["width"].as_f64(), Some(375.0));
        let pointer = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "matchMedia('(pointer:coarse)').matches".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(pointer, serde_json::json!(false));

        // The override must survive a later navigation.
        mgr.send(&sid, |reply| SessionCommand::Navigate {
            url: format!("http://127.0.0.1:{port}/n"),
            reply,
        })
        .await
        .unwrap();
        let w = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "innerWidth".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(w.as_f64(), Some(375.0));

        // Recorded so replay scripts re-pin the viewport.
        let exported = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(
            exported.contains("\"viewport\""),
            "export must record the viewport action, got: {exported}"
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    #[tokio::test]
    async fn set_content_loads_local_html_as_a_real_page() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);

        let html = "<html><head><title>Doc</title></head>\
             <body><button id=\"b\">hello</button><script>window.__ran=1</script></body></html>"
            .to_string();
        let r = mgr
            .send(&sid, |reply| SessionCommand::SetContent {
                html: html.clone(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(r["bytes"].as_u64(), Some(html.len() as u64));
        assert_eq!(r["title"].as_str(), Some("Doc"));

        // It is a real page: scripts ran, DOM state is extractable.
        let ran = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__ran".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(ran.as_i64(), Some(1));
        let state = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap();
        assert!(state.contains("hello"), "state must see the DOM, got: {state}");

        // A later SetContent replaces the page (old DOM gone).
        mgr.send(&sid, |reply| SessionCommand::SetContent {
            html: "<html><head><title>Two</title></head><body><p>second</p></body></html>"
                .to_string(),
            reply,
        })
        .await
        .unwrap();
        let title = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.title".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(title.as_str(), Some("Two"));

        // Recorded so replay scripts restore the content.
        let exported = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(
            exported.contains("set_content"),
            "export must record the setContent action, got: {exported}"
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    #[tokio::test]
    async fn clone_carries_login_state_to_a_new_session() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /app",
            "<html><body><script>localStorage.setItem('user','miccim')</script>\
             <p>app</p></body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let src = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/app")),
            false,
            vec!["sid=abc".to_string()],
            None,
            None,
            None,
            false,
            false,
        );
        mgr.send(&src, |reply| SessionCommand::Viewport {
            width: Some(375),
            height: Some(667),
            mobile: false,
            reply,
        })
        .await
        .unwrap();
        mgr.send(&src, |reply| SessionCommand::Dialog {
            action: "accept".to_string(),
            prompt_text: None,
            reply,
        })
        .await
        .unwrap();

        let resp = mgr.clone_session(&src).await.unwrap();
        assert_eq!(resp["cloned_from"].as_str(), Some(src.as_str()));
        assert_eq!(resp["viewport"]["width"].as_f64(), Some(375.0));
        let dup = resp["session_id"].as_str().unwrap().to_string();
        assert_ne!(dup, src);

        // Login state arrived in the derived session: cookie, storage,
        // viewport pin, dialog policy.
        let cookies = mgr
            .send(&dup, |reply| SessionCommand::Cookies { reply })
            .await
            .unwrap();
        let cookies: Value = serde_json::from_str(&cookies).unwrap();
        assert!(
            cookies["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().is_some_and(|s| s.starts_with("sid=abc;"))),
            "cookie must carry over: {cookies}"
        );

        let user = mgr
            .send(&dup, |reply| SessionCommand::Eval {
                script: "localStorage.getItem('user')".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(user.as_str(), Some("miccim"));

        let w = mgr
            .send(&dup, |reply| SessionCommand::Eval {
                script: "innerWidth".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(w.as_f64(), Some(375.0));

        let policy = mgr
            .send(&dup, |reply| SessionCommand::Dialog {
                action: "list".to_string(),
                prompt_text: None,
                reply,
            })
            .await
            .unwrap();
        let policy: Value = serde_json::from_str(&policy).unwrap();
        assert_eq!(policy["policy"].as_str(), Some("accept"));

        // The source keeps serving untouched.
        let still = mgr
            .send(&src, |reply| SessionCommand::Eval {
                script: "localStorage.getItem('user')".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(still.as_str(), Some("miccim"));

        assert!(mgr.close_and_wait(&dup).await);
        assert!(mgr.close_and_wait(&src).await);
    }

    #[cfg(feature = "screenshot")]
    #[tokio::test]
    async fn screenshot_captures_current_dom_state() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /shot",
            "<html><head><style>body{background-color:#336699}</style>\
             </head><body><h1>before</h1></body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some(&format!("http://127.0.0.1:{port}/shot")), false, vec![], None, None, None, false, false);

        // Mutate the DOM after load — the capture renders page.content(),
        // so the pixels must reflect the mutation, not the HTTP response.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.querySelector('h1').textContent = 'after'".to_string(),
            reply,
        })
        .await
        .unwrap();

        let shot = mgr
            .send(&sid, |reply| SessionCommand::Screenshot {
                width: Some(400),
                height: Some(300),
                full_page: false,
                selector: None,
                selector_all: false,
                reply,
            })
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&shot).expect("screenshot JSON");
        assert_eq!(v["width"].as_f64(), Some(400.0));
        assert_eq!(v["format"], serde_json::json!("png"));
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let png = STANDARD
            .decode(v["image_base64"].as_str().expect("base64 body"))
            .expect("decodable base64");
        assert_eq!(&png[0..4], b"\x89PNG", "PNG magic bytes");
        assert!(png.len() > 1000, "non-trivial image, got {} bytes", png.len());

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// The async-content case straight from real usage: a page whose cards
    /// arrive via setTimeout can't be probed with a bare eval (racy), so the
    /// agent sleeps blindly. Wait polls with the event loop driven in
    /// between, so the timer actually fires and the selector matches.
    #[tokio::test]
    async fn wait_selector_matches_async_inserted_element() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /wait",
            "<html><body><div id=\"root\"></div><script>\
             setTimeout(function() { \
               document.getElementById('root').innerHTML = \
                 '<div class=\"card\">Plan A</div><div class=\"card\">Plan B</div>'; \
             }, 600); \
             setTimeout(function() { window.__ready = true; }, 1200); \
             </script></body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some(&format!("http://127.0.0.1:{port}/wait")), false, vec![], None, None, None, false, false);

        // Elapsed must cover the 600ms timer — proof the loop was driven,
        // not just polled.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Wait {
                selector: Some(".card".to_string()),
                predicate: None,
                timeout_ms: 8_000,
                reply,
            })
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).expect("wait JSON");
        assert_eq!(v["matched"], serde_json::json!(true));
        assert_eq!(v["detail"]["tag"], serde_json::json!("DIV"));
        assert!(
            v["detail"]["text"].as_str().unwrap_or("").contains("Plan A"),
            "detail carries the matched element's text"
        );

        // Predicate form against a later timer.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Wait {
                selector: None,
                predicate: Some("window.__ready === true".to_string()),
                timeout_ms: 8_000,
                reply,
            })
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).expect("wait JSON");
        assert_eq!(v["matched"], serde_json::json!(true));

        // Timeout path: never-matching selector errors with the selector named.
        let err = mgr
            .send(&sid, |reply| SessionCommand::Wait {
                selector: Some(".never".to_string()),
                predicate: None,
                timeout_ms: 300,
                reply,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("timeout") && err.to_string().contains(".never"),
            "timeout error names the selector, got: {err}"
        );

        // Exactly one of selector/predicate.
        let err = mgr
            .send(&sid, |reply| SessionCommand::Wait {
                selector: Some("a".to_string()),
                predicate: Some("true".to_string()),
                timeout_ms: 1_000,
                reply,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exactly one"), "got: {err}");

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// The failure mode from real usage: a session idled out and the login
    /// state in localStorage was gone. Export from session A, restore into
    /// session B — keys/values with quotes and CJK must survive the trip.
    #[tokio::test]
    async fn storage_exports_and_restores_across_sessions() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /store",
            "<html><body>store</body></html>",
        )]);
        let url = format!("http://127.0.0.1:{port}/store");

        let mut mgr = SessionManager::new();
        let a = mgr.create(Some(&url), false, vec![], None, None, None, false, false);

        mgr.send(&a, |reply| SessionCommand::Eval {
            script: "localStorage.setItem('token','abc\"123'); \
                     localStorage.setItem('user','小张'); \
                     sessionStorage.setItem('cart','2 items'); 'ok'"
                .to_string(),
            reply,
        })
        .await
        .unwrap();

        let exported = mgr
            .send(&a, |reply| SessionCommand::Storage { reply })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&exported).expect("storage JSON");
        assert_eq!(v["local_storage"]["token"], serde_json::json!("abc\"123"));
        assert_eq!(v["local_storage"]["user"], serde_json::json!("小张"));
        assert_eq!(v["session_storage"]["cart"], serde_json::json!("2 items"));
        assert!(mgr.close_and_wait(&a).await);

        // Fresh session on the same origin, restored from A's snapshot.
        let b = mgr.create(Some(&url), false, vec![], Some(v), None, None, false, false);
        let token = mgr
            .send(&b, |reply| SessionCommand::Eval {
                script: "localStorage.getItem('token') + '|' + localStorage.getItem('user') \
                         + '|' + sessionStorage.getItem('cart')"
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            token.as_str().unwrap_or(""),
            "abc\"123|小张|2 items",
            "restored storage must carry quotes and CJK verbatim"
        );
        assert!(mgr.close_and_wait(&b).await);
    }

    /// ttl_secs widens (or narrows) a session's idle budget; the default
    /// stays at the 8-minute reaper.
    #[tokio::test]
    async fn ttl_secs_overrides_the_idle_budget() {
        let mut mgr = SessionManager::new();
        let long = mgr.create(Some("about:blank"), false, vec![], None, Some(3600), None, false, false);
        let short = mgr.create(Some("about:blank"), false, vec![], None, Some(60), None, false, false);
        let def = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);

        let entries = mgr.list();
        let by_id = |list: &[SessionListEntry], id: &str| {
            list.iter()
                .find(|e| e.session_id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .expires_in_secs
                .expect("non-keepalive session carries a countdown")
        };
        assert!(by_id(&entries, &long) > 3500, "hour-long TTL, got {}", by_id(&entries, &long));
        assert!(by_id(&entries, &short) <= 60, "60s TTL, got {}", by_id(&entries, &short));
        assert!(
            by_id(&entries, &def) <= 480,
            "default stays at 8 minutes, got {}",
            by_id(&entries, &def)
        );

        // Clamp: out-of-range asks land on the rails.
        let clamped = mgr.create(Some("about:blank"), false, vec![], None, Some(999_999), None, false, false);
        let entries = mgr.list();
        assert!(by_id(&entries, &clamped) <= 3600, "clamp at one hour");

        for id in [long, short, def, clamped] {
            assert!(mgr.close_and_wait(&id).await);
        }
    }

    /// Feedback ⑤: session_create {width,height,mobile} pins the viewport at
    /// session birth — the first page already renders under the override, so
    /// the very first probe (and a later navigation) sees it without a
    /// separate session_viewport call.
    #[tokio::test]
    async fn create_pins_viewport_before_first_command() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /m",
                "<html><head><style>@media (max-width:600px){body{color:#010203}}</style>\
                 </head><body><p>m</p></body></html>",
            ),
            ("GET /n", "<html><body>n</body></html>"),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/m")),
            false,
            vec![],
            None,
            None,
            Some((Some(375), Some(667), true)),
            false,
            false,
        );

        // First command the caller sends is a probe — the queued pin must
        // have been applied ahead of it (FIFO on the command channel). The
        // probe echoes current metrics without moving the mobile flag.
        let vp = mgr
            .send(&sid, |reply| SessionCommand::Viewport {
                width: None,
                height: None,
                mobile: true,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(vp["width"].as_f64(), Some(375.0), "pin applied before first command");
        assert_eq!(vp["mobile"].as_bool(), Some(true));

        let matches = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "innerWidth + 'x' + innerHeight + '|' + matchMedia('(pointer:coarse)').matches"
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(matches, serde_json::json!("375x667|true"));

        // The pin is a real override: it rides through a navigation too.
        mgr.send(&sid, |reply| SessionCommand::Navigate {
            url: format!("http://127.0.0.1:{port}/n"),
            reply,
        })
        .await
        .unwrap();
        let after = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "innerWidth".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(after, serde_json::json!(375));

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// Feedback ③: keepalive sessions skip the idle reaper — backdate the
    /// clock past any TTL and they still answer, expires_in_secs() goes
    /// quiet, and list() flags them.
    #[tokio::test]
    async fn keepalive_sessions_never_expire() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /ka",
            "<html><body>ka</body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/ka")),
            false,
            vec![],
            None,
            None,
            None,
            true,
            false,
        );

        {
            let s = mgr.sessions.get_mut(&sid).expect("session");
            s.last_active = std::time::Instant::now() - std::time::Duration::from_secs(99_999);
        }

        assert!(mgr.expires_in_secs(&sid).is_none(), "keepalive has no countdown");
        let entry = mgr
            .list()
            .into_iter()
            .find(|e| e.session_id == sid)
            .expect("listed");
        assert!(entry.keepalive, "list() flags keepalive");
        assert!(
            !mgr.sessions.get(&sid).expect("session").is_expired(),
            "past any TTL, keepalive still lives"
        );

        let v = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1 + 1".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(2));

        assert!(mgr.close_and_wait(&sid).await);
    }

    type SharedSnapshots = std::sync::Arc<std::sync::Mutex<HashMap<String, (String, i64)>>>;

    /// A manager whose snapshots live in a shared map: two managers holding
    /// the same Arc simulate a process restart (the sqlite backend behind
    /// SessionManager::new is exercised by store.rs's own tests).
    fn manager_on(shared: SharedSnapshots) -> SessionManager {
        SessionManager {
            sessions: HashMap::new(),
            snapshots: SnapshotStore::Memory(shared),
        }
    }

    /// Feedback ③: a persistent session's login state survives eviction and
    /// a full manager restart — the same session_id comes back with the
    /// cookie, the injected storage and the viewport pin, no re-login.
    #[tokio::test]
    async fn persistent_session_revives_by_the_same_id_across_managers() {
        let _net = crate::server::test_util::net_env_guard();
        // Plain page: no script of its own, so anything found after the
        // revival can only have come from the snapshot.
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /app",
            "<html><body><p>app</p></body></html>",
        )]);
        let url = format!("http://127.0.0.1:{port}/app");
        let shared: SharedSnapshots = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        let mut mgr = manager_on(shared.clone());
        let sid = mgr.create(
            Some(&url),
            false,
            vec!["sid=abc".to_string()],
            None,
            None,
            Some((Some(375), Some(667), false)),
            false,
            true,
        );
        // The mutation is what the snapshot must capture — the page never
        // sets this key, so only injection can restore it.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "localStorage.setItem('user','miccim'); 'ok'".to_string(),
            reply,
        })
        .await
        .unwrap();
        assert!(
            shared.lock().unwrap().contains_key(&sid),
            "a successful command must refresh the snapshot"
        );

        // "Restart": a brand-new manager over the same snapshot store.
        let mut mgr2 = manager_on(shared.clone());
        let user = mgr2
            .send(&sid, |reply| SessionCommand::Eval {
                script: "localStorage.getItem('user')".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(user.as_str(), Some("miccim"));

        let cookies = mgr2
            .send(&sid, |reply| SessionCommand::Cookies { reply })
            .await
            .unwrap();
        let cookies: Value = serde_json::from_str(&cookies).unwrap();
        assert!(
            cookies["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().is_some_and(|s| s.starts_with("sid=abc;"))),
            "cookie must survive the restart: {cookies}"
        );

        let w = mgr2
            .send(&sid, |reply| SessionCommand::Eval {
                script: "innerWidth".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(w.as_f64(), Some(375.0));

        assert!(mgr2.close_and_wait(&sid).await);
        assert!(
            !mgr2.sessions.contains_key(&sid),
            "close removed the revived session"
        );
        assert!(
            !shared.lock().unwrap().contains_key(&sid),
            "explicit close on the revived session drops the snapshot"
        );
        assert!(
            mgr.sessions.contains_key(&sid),
            "the pre-restart manager is unaffected by the other manager's close"
        );
    }

    /// Feedback ③: an idle-expired persistent session revives instead of
    /// erroring; a plain session still gets the structured Expired error.
    #[tokio::test]
    async fn expired_persistent_session_revives_instead_of_erroring() {
        let _net = crate::server::test_util::net_env_guard();
        let shared: SharedSnapshots = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mut mgr = manager_on(shared);

        let p = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, true);
        mgr.send(&p, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            reply,
        })
        .await
        .unwrap();
        {
            let s = mgr.sessions.get_mut(&p).expect("session");
            s.last_active = std::time::Instant::now() - std::time::Duration::from_secs(481);
        }
        let v = mgr
            .send(&p, |reply| SessionCommand::Eval {
                script: "2 + 1".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(3), "expired persistent session revives");
        assert!(mgr.sessions.contains_key(&p), "revived session is live again");

        // Control: a plain session expires with the structured error.
        let plain = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
        mgr.send(&plain, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            reply,
        })
        .await
        .unwrap();
        {
            let s = mgr.sessions.get_mut(&plain).expect("session");
            s.last_active = std::time::Instant::now() - std::time::Duration::from_secs(481);
        }
        let err = mgr
            .send(&plain, |reply| SessionCommand::Eval {
                script: "1".to_string(),
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_EXPIRED");

        mgr.close_inner(&p, true);
        mgr.close_inner(&plain, true);
    }

    /// An explicit close drops the snapshot (done means done); an idle
    /// eviction keeps it (that is the revive path).
    #[tokio::test]
    async fn close_drops_the_snapshot_but_idle_eviction_keeps_it() {
        let _net = crate::server::test_util::net_env_guard();
        let shared: SharedSnapshots = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mut mgr = manager_on(shared.clone());

        let a = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, true);
        mgr.send(&a, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            reply,
        })
        .await
        .unwrap();
        assert!(shared.lock().unwrap().contains_key(&a));
        assert!(mgr.close_and_wait(&a).await);
        assert!(
            !shared.lock().unwrap().contains_key(&a),
            "explicit close drops the snapshot"
        );

        let b = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, true);
        mgr.send(&b, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            reply,
        })
        .await
        .unwrap();
        {
            let s = mgr.sessions.get_mut(&b).expect("session");
            s.last_active = std::time::Instant::now() - std::time::Duration::from_secs(481);
        }
        mgr.evict_expired();
        assert!(
            shared.lock().unwrap().contains_key(&b),
            "idle eviction must keep the snapshot"
        );
        let v = mgr
            .send(&b, |reply| SessionCommand::Eval {
                script: "40 + 2".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(42), "evicted persistent session revives");
        mgr.close_inner(&b, true);
    }

    /// A fresh process must not allocate a session id that a surviving
    /// snapshot still owns.
    #[tokio::test]
    async fn create_skips_ids_owned_by_snapshots() {
        let shared: SharedSnapshots = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let next = format!("s_{}", SESSION_COUNTER.load(Ordering::Relaxed));
        shared
            .lock()
            .unwrap()
            .insert(next.clone(), (r#"{"version":1}"#.to_string(), 0));
        let mut mgr = manager_on(shared);
        let id = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
        assert_ne!(id, next, "create must skip ids owned by a snapshot");
        mgr.close_inner(&id, true);
    }

    /// Feedback ⑨: a throwing eval is an EVAL_ERROR with the error name,
    /// message, throw position and first stack frame — not a silent null.
    /// A rejected promise surfaces the same way.
    #[tokio::test]
    async fn eval_exceptions_carry_name_position_and_stack() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /err",
            "<html><body>err</body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/err")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        let err = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1 + 1; throw new TypeError('boom at runtime')".to_string(),
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "EVAL_ERROR");
        let text = err.to_string();
        assert!(text.contains("TypeError: boom at runtime"), "got: {text}");
        assert!(text.contains("(line 1, col"), "throw position in message, got: {text}");
        assert!(
            text.contains("<anonymous>:1:14"),
            "first stack frame carries the throw site, got: {text}"
        );

        // The session survives a failed eval and still evaluates fine.
        let ok = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "2 + 2".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(ok, serde_json::json!(4));

        let err = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "Promise.reject(new Error('async boom'))".to_string(),
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "EVAL_ERROR");
        assert!(
            err.to_string().contains("Error: async boom"),
            "got: {}",
            err.to_string()
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// session_click_xy / session_drag synthesize the real mouse chain at
    /// viewport coordinates: elementFromPoint hit-testing picks the pad (not
    /// body), a drag delivers every interpolated mousemove so mousemove-driven
    /// widgets see the pointer travel, and click_count 2 adds dblclick.
    #[tokio::test]
    async fn click_xy_and_drag_fire_mouse_chain() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /mousepad",
            "<html><body style=\"margin:0\">\
             <div id=\"pad\" style=\"position:absolute;left:10px;top:10px;width:200px;height:200px;\"></div>\
             <script>\
             var log = [];\
             var pad = document.getElementById('pad');\
             pad.addEventListener('mousedown', function(e){ log.push('down@'+e.clientX+','+e.clientY+' on '+e.target.id); });\
             document.addEventListener('mousemove', function(e){ log.push('move@'+Math.round(e.clientX)+','+Math.round(e.clientY)); });\
             document.addEventListener('mouseup', function(e){ log.push('up@'+e.clientX+','+e.clientY); });\
             document.addEventListener('click', function(e){ log.push('click@'+e.clientX+','+e.clientY); });\
             document.addEventListener('dblclick', function(e){ log.push('dblclick@'+e.clientX+','+e.clientY); });\
             window.__log = log;\
             </script>\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/mousepad")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        async fn read_log(mgr: &mut SessionManager, sid: &str) -> Vec<String> {
            let out = mgr
                .send(sid, |reply| SessionCommand::Eval {
                    script: "JSON.stringify(window.__log)".to_string(),
                    reply,
                })
                .await
                .unwrap();
            let raw = out.as_str().unwrap_or("[]");
            serde_json::from_str::<Vec<String>>(raw).expect("log array")
        }

        // Single click at (50,50) — inside the pad. The chain lands on the
        // hit element: mousedown with the coordinates, mouseup, then click.
        let out = mgr
            .send(&sid, |reply| SessionCommand::ClickXY {
                x: 50.0,
                y: 50.0,
                button: "left".to_string(),
                click_count: 1,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("click_xy JSON");
        assert_eq!(v["x"], 50.0);
        assert_eq!(v["y"], 50.0);

        let log = read_log(&mut mgr, &sid).await;
        assert!(
            log.iter().any(|e| e == "down@50,50 on pad"),
            "mousedown hits the pad, got {log:?}"
        );
        assert!(log.iter().any(|e| e == "up@50,50"), "mouseup, got {log:?}");
        assert!(log.iter().any(|e| e == "click@50,50"), "click, got {log:?}");
        assert!(
            !log.iter().any(|e| e.starts_with("dblclick")),
            "single click must not dblclick, got {log:?}"
        );

        // click_count 2 adds the dblclick synthesis Chrome does.
        mgr.send(&sid, |reply| SessionCommand::ClickXY {
            x: 50.0,
            y: 50.0,
            button: "left".to_string(),
            click_count: 2,
            reply,
        })
        .await
        .unwrap();
        let log = read_log(&mut mgr, &sid).await;
        assert!(
            log.iter().any(|e| e == "dblclick@50,50"),
            "dblclick after count 2, got {log:?}"
        );

        // Drag across the pad: 1 press, N interpolated moves ending at the
        // release point, 1 release. The moves are what mousemove-driven
        // widgets (map markers) track.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Drag {
                from_x: 60.0,
                from_y: 60.0,
                to_x: 150.0,
                to_y: 90.0,
                steps: 10,
                delay_ms: 0,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("drag JSON");
        assert_eq!(v["steps"], 10);

        let log = read_log(&mut mgr, &sid).await;
        let downs = log.iter().filter(|e| e.starts_with("down@60,60")).count();
        assert_eq!(downs, 1, "one press, got {log:?}");
        let moves: Vec<&String> = log.iter().filter(|e| e.starts_with("move@")).collect();
        assert_eq!(moves.len(), 10, "every interpolated move delivered, got {log:?}");
        assert_eq!(moves[0].as_str(), "move@69,63", "first step interpolated");
        assert_eq!(
            moves.last().map(|m| m.as_str()),
            Some("move@150,90"),
            "last move lands on the release point, got {log:?}"
        );
        assert!(
            log.iter().any(|e| e == "up@150,90"),
            "release at the destination, got {log:?}"
        );

        // Both actions join the replay log.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(out.contains("\"click_xy\""), "click_xy recorded, got {out}");
        assert!(out.contains("\"drag\""), "drag recorded, got {out}");

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// Page console output lands in the session ring: messages from the
    /// page's own script and from a later eval both show up, with levels
    /// intact, and the second read doesn't duplicate (queue drained).
    #[tokio::test]
    async fn console_ring_captures_page_and_eval_output() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /console",
            "<html><body><script>\
             console.log('boot ok'); \
             console.warn('legacy api'); \
             console.error('load failed: x'); \
             </script></body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/console")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        // Output produced by an agent-driven eval joins the same ring.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "console.error('eval-time boom'); 'done'".to_string(),
            reply,
        })
        .await
        .unwrap();

        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter::default(),
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("console JSON");
        let msgs = v["messages"].as_array().expect("messages array");
        let texts: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m["text"].as_str())
            .collect();
        assert!(texts.contains(&"boot ok"), "log from page script, got {texts:?}");
        assert!(texts.contains(&"legacy api"), "warn level, got {texts:?}");
        assert!(
            texts.contains(&"load failed: x") && texts.contains(&"eval-time boom"),
            "errors from page and eval, got {texts:?}"
        );
        let err_levels: Vec<&str> = msgs
            .iter()
            .filter(|m| m["text"].as_str().unwrap_or("").contains("load failed"))
            .filter_map(|m| m["level"].as_str())
            .collect();
        assert_eq!(err_levels, vec!["error"]);

        // Reads are non-destructive on the ring: the repeat read still
        // carries everything (agents re-read / diff by ts_ms as needed).
        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter::default(),
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("console JSON 2");
        assert_eq!(
            v["messages"].as_array().map(Vec::len),
            Some(msgs.len()),
            "ring must be stable across reads"
        );

        // Filters: level narrows (2 errors here), limit keeps the most
        // recent match, total/matched report both sides of the funnel.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter {
                    level: Some("error".into()),
                    limit: Some(1),
                    ..Default::default()
                },
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("filtered JSON");
        let filtered = v["messages"].as_array().expect("filtered array");
        assert_eq!(v["total"].as_u64(), Some(4), "ring size, not filter size");
        assert_eq!(v["matched"].as_u64(), Some(2), "errors in ring");
        assert_eq!(filtered.len(), 1, "limit keeps the most recent");
        assert_eq!(filtered[0]["text"].as_str(), Some("eval-time boom"));
        assert_eq!(filtered[0]["level"].as_str(), Some("error"));

        // Every entry carries the page URL it was logged on, so
        // url_contains can slice a multi-page session.
        let port_str = port.to_string();
        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter {
                    url_contains: Some(port_str.clone()),
                    ..Default::default()
                },
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("url-filtered JSON");
        assert_eq!(v["matched"].as_u64(), Some(4), "all logged on this page");
        for m in v["messages"].as_array().unwrap() {
            assert!(
                m["url"].as_str().unwrap_or("").contains(&port_str),
                "entry url stamped at log time: {m}"
            );
        }

        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter {
                    url_contains: Some("no-such-page.example".to_string()),
                    since_ts: Some(u64::MAX / 2),
                    ..Default::default()
                },
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("empty-filter JSON");
        assert_eq!(v["matched"].as_u64(), Some(0), "non-matching filters");
        assert_eq!(
            v["messages"].as_array().map(Vec::len),
            Some(0),
            "empty result is an empty array, not an error"
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// Dialogs never block: default policy auto-dismisses (confirm false,
    /// prompt null) while every call is logged at level "dialog"; the
    /// session_dialog command flips the policy and prompt_text, and the
    /// next dialog answers accordingly.
    #[tokio::test]
    async fn dialog_policy_answers_and_logs() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /dialogs",
            "<html><body>dialogs</body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/dialogs")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
        );

        // Default policy: auto-dismiss. confirm → false, prompt → null.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "[String(confirm('delete it?')), String(prompt('your name'))].join('|')"
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(out, "false|null", "dismiss is the default policy");

        // The three calls are observable in the ring via session_console.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Console {
                filter: ConsoleFilter {
                    level: Some("dialog".into()),
                    ..Default::default()
                },
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("dialog ring JSON");
        let dialogs = v["messages"].as_array().expect("dialog entries");
        assert_eq!(dialogs.len(), 2, "confirm + prompt logged");
        assert!(dialogs.iter().all(|m| m["level"] == "dialog"));
        let payloads: Vec<Value> = dialogs
            .iter()
            .map(|m| serde_json::from_str(m["text"].as_str().unwrap_or("")).unwrap())
            .collect();
        assert_eq!(payloads[0]["dialog"], "confirm", "kind tagged: {payloads:?}");
        assert_eq!(payloads[0]["message"], "delete it?");
        assert_eq!(payloads[0]["answer"], false, "dismissed");
        assert_eq!(payloads[1]["dialog"], "prompt");
        assert_eq!(payloads[1]["value"], Value::Null);

        // Flip to accept with prompt text; the next dialogs answer on-policy.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Dialog {
                action: "accept".into(),
                prompt_text: Some("ada".into()),
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("policy JSON");
        assert_eq!(v["policy"], "accept");
        assert_eq!(v["prompt_text"], "ada");

        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "[String(confirm('sure?')), String(prompt('your name')), String(prompt('fallback'))].join('|')"
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(out, "true|ada|ada", "accept + prompt_text applied");

        // list() reports the policy and the accumulated dialog history.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Dialog {
                action: "list".into(),
                prompt_text: None,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("list JSON");
        assert_eq!(v["policy"], "accept");
        assert_eq!(v["dialogs"].as_array().map(Vec::len), Some(5), "2 + 3 logged");

        // Back to dismiss: prompts answer null again.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Dialog {
                action: "dismiss".into(),
                prompt_text: None,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("dismiss JSON");
        assert_eq!(v["policy"], "dismiss");

        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "String(confirm('again?'))".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(out, "false", "policy flip is not sticky");

        // Unknown actions are errors, not silent no-ops.
        let err = mgr
            .send(&sid, |reply| SessionCommand::Dialog {
                action: "sudo".into(),
                prompt_text: None,
                reply,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown dialog action"));

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// list() reports every live session with an expiry budget, most recently
    /// active first, and drops to empty once sessions close.
    #[tokio::test]
    async fn list_reports_live_sessions_and_goes_empty_after_close() {
        let mut mgr = SessionManager::new();
        let a = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);

        let entries = mgr.list();
        assert_eq!(entries.len(), 2);
        let ids: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
        assert!(entries[0].idle_secs <= entries[1].idle_secs, "most recent first");
        for e in &entries {
            assert!(
                e.expires_in_secs.expect("countdown") <= 480,
                "expiry budget caps at the 8 min timeout"
            );
        }

        assert!(mgr.close_and_wait(&a).await);
        assert!(mgr.close_and_wait(&b).await);
        assert!(mgr.list().is_empty());
    }

    /// A recorded session replays as a bash+curl script: every action type
    /// becomes a POST against $BASE, and single quotes in payloads survive
    /// shell quoting (the '\'' idiom) instead of terminating the argument.
    #[test]
    fn replay_bash_renders_all_actions_and_quotes_singles() {
        let jsonl = [
            r#"{"action":"create","url":"https://example.com/","use_proxy":false,"cookies":["sid=it's"]}"#,
            r#"{"action":"navigate","url":"https://example.com/page","ok":true}"#,
            r#"{"action":"click","index":3,"ok":true}"#,
            r#"{"action":"input","index":1,"text":"it's a test","ok":true}"#,
            r#"{"action":"scroll","direction":"down","amount":3}"#,
            r#"{"action":"eval","script":"document.querySelector('#q').value"}"#,
        ]
        .join("\n");

        let script = replay_bash(&jsonl, "http://127.0.0.1:8089");
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains(r#"BASE="${AGINXBROWSER_URL:-http://127.0.0.1:8089}""#));
        assert!(script.contains("POST session/create"));
        assert!(script.contains(r#"POST "session/$SID/navigate""#));
        assert!(script.contains(r#"POST "session/$SID/click""#));
        assert!(script.contains(r#"POST "session/$SID/input""#));
        assert!(script.contains(r#"POST "session/$SID/scroll""#));
        assert!(script.contains(r#"POST "session/$SID/eval""#));
        assert!(script.contains(r#"POST "session/$SID/state" '{}'"#), "ends by printing final state");
        // Single-quote escaping: it's → 'it'\''s — an unescaped quote would
        // terminate the argument and execute the rest as shell.
        assert!(script.contains(r#""sid=it'\''s"]"#));
        assert!(script.contains(r#""text":"it'\''s a test""#));
        // Command substitution appears exactly once — capturing SID from the
        // create call. Payloads are plain single-quoted strings, never
        // $(...)-wrapped (that would execute JSON as shell).
        assert_eq!(script.matches("$(").count(), 1);
    }

    /// Actions before a create record (or a log with no create at all) must
    /// not emit $SID-referencing POSTs — the script would die on an unbound
    /// variable under set -eu.
    #[test]
    fn replay_bash_without_create_skips_sid_actions() {
        let jsonl = r#"{"action":"navigate","url":"https://example.com/","ok":true}"#;
        let script = replay_bash(jsonl, "http://127.0.0.1:8089");
        assert!(!script.contains("$SID"), "no SID may be referenced without a create");
    }

    /// A live session records what it did: create params + one entry per
    /// navigate/scroll/eval command, exportable as JSONL. Reads (State,
    /// Cookies) stay out of the log — replaying them is meaningless.
    #[tokio::test]
    async fn session_records_actions_and_exports_jsonl() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some("about:blank"), false, vec!["k=v".to_string()], None, None, None, false, false);

        let _ = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap(); // read: not recorded
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Scroll {
                direction: ScrollDirection::Down,
                amount: 2,
                reply,
            })
            .await
            .unwrap();
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1 + 1".to_string(),
                reply,
            })
            .await
            .unwrap();
        let jsonl = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");

        let actions: Vec<Value> = jsonl.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(actions.len(), 3, "create + scroll + eval, state excluded");

        assert_eq!(actions[0]["action"], "create");
        assert_eq!(actions[0]["url"], "about:blank");
        assert_eq!(actions[0]["cookies"][0], "k=v");
        assert_eq!(actions[0]["use_proxy"], false);

        assert_eq!(actions[1]["action"], "scroll");
        assert_eq!(actions[1]["direction"], "down");
        assert_eq!(actions[1]["amount"], 2);

        assert_eq!(actions[2]["action"], "eval");
        assert_eq!(actions[2]["script"], "1 + 1");

        // The exported log must render through replay_bash without losing
        // actions (create present → SID-bound POSTs emitted).
        let script = replay_bash(&jsonl, "http://127.0.0.1:8089");
        assert!(script.contains("POST session/create"));
        assert!(script.contains(r#"POST "session/$SID/scroll""#));
        assert!(script.contains(r#"POST "session/$SID/eval""#));
    }

    /// The session sniffer: a page-side fetch() of a media URL surfaces in
    /// the Network command (media filter extracts the playback link), and
    /// Har returns a parseable HAR 1.2 document whose document entry carries
    /// its retained text body. Media elements and player iframes the engine
    /// never fetches surface as DOM candidates (via "dom"); the token-less
    /// <source> URL dedupes against its token-carrying network twin.
    #[tokio::test]
    async fn network_sniffer_and_har_surfaces() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /watch",
                "<html><body>\
                 <video src='/v/clip.mp4' controls><source src='/v/master.m3u8'></video>\
                 <iframe src='/embed'></iframe>\
                 <script>\
                 fetch('/v/master.m3u8?token=1').then(function(r){return r.text()});\
                 </script></body></html>",
            ),
            ("GET /v/master.m3u8?token=1", "#EXTM3U"),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some(&format!("http://127.0.0.1:{port}/watch")), false, vec![], None, None, None, false, false);

        // One pump cycle so the page's fetch() settles into the event queue.
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1".to_string(),
                reply,
            })
            .await
            .unwrap();

        let media = mgr
            .send(&sid, |reply| SessionCommand::Network { media_only: true, include_bodies: false, url_contains: None, body_max_chars: 0, reply })
            .await
            .unwrap();
        let media: Value = serde_json::from_str(&media).unwrap();
        let items = media["media"].as_array().unwrap();
        assert_eq!(
            items.len(),
            3,
            "network m3u8 + dom video + dom iframe (source deduped): {media}"
        );
        assert_eq!(items[0]["kind"], "hls");
        assert_eq!(items[0]["via"], "network");
        assert!(
            items[0]["url"].as_str().unwrap().ends_with("/v/master.m3u8?token=1"),
            "playback link carries its query: {media}"
        );
        // Native video src the engine never fetched: a dom candidate.
        assert_eq!(items[1]["via"], "dom");
        assert_eq!(items[1]["tag"], "video");
        assert_eq!(items[1]["kind"], "mp4");
        assert!(
            items[1]["url"].as_str().unwrap().ends_with("/v/clip.mp4"),
            "native video src is a dom candidate: {media}"
        );
        // Player iframe: a candidate to navigate into, not a playable URL.
        assert_eq!(items[2]["via"], "dom");
        assert_eq!(items[2]["tag"], "iframe");
        assert_eq!(items[2]["kind"], "iframe");
        assert!(
            items[2]["url"].as_str().unwrap().ends_with("/embed"),
            "player iframe surfaces as a navigation candidate: {media}"
        );

        let all = mgr
            .send(&sid, |reply| SessionCommand::Network { media_only: false, include_bodies: false, url_contains: None, body_max_chars: 0, reply })
            .await
            .unwrap();
        let all: Value = serde_json::from_str(&all).unwrap();
        assert!(
            all["total"].as_u64().unwrap() >= 2,
            "document + script fetch: {all}"
        );
        assert!(
            all.get("xhr").is_none(),
            "no xhr array unless include_bodies asks for it: {all}"
        );

        // include_bodies: the page's own API responses ride along as a
        // sibling `xhr` array, narrowed by url_contains.
        let xhrs = mgr
            .send(&sid, |reply| SessionCommand::Network {
                media_only: false,
                include_bodies: true,
                url_contains: Some("/v/".to_string()),
                body_max_chars: 100,
                reply,
            })
            .await
            .unwrap();
        let xhrs: Value = serde_json::from_str(&xhrs).unwrap();
        let arr = xhrs["xhr"].as_array().expect("xhr array");
        assert_eq!(arr.len(), 1, "the script-initiated fetch only: {xhrs}");
        assert!(
            arr[0]["url"].as_str().unwrap().ends_with("/v/master.m3u8?token=1"),
            "entry carries the request URL: {xhrs}"
        );
        assert_eq!(arr[0]["status"], 200);
        assert_eq!(arr[0]["body"], "#EXTM3U");
        assert_eq!(arr[0]["body_truncated"], false);

        let har = mgr
            .send(&sid, |reply| SessionCommand::Har { reply })
            .await
            .unwrap();
        let har: Value = serde_json::from_str(&har).unwrap();
        assert_eq!(har["log"]["version"], "1.2");
        let entries = har["log"]["entries"].as_array().unwrap();
        assert!(entries.len() >= 2, "document + fetch entry: {har}");
        let doc = entries
            .iter()
            .find(|e| e["request"]["url"].as_str().unwrap().ends_with("/watch"))
            .unwrap();
        assert_eq!(doc["response"]["status"], 200);
        assert!(
            doc["response"]["content"]["text"].as_str().unwrap().contains("master.m3u8"),
            "document body retained as text"
        );

        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");
    }
}
