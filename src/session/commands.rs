//! The session command protocol: [`SessionCommand`] (what the async side
//! sends down each session's channel) and the reply/response types that
//! cross back. Split from the session module root (ARCHITECTURE.md P2);
//! behavior unchanged.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use super::state::{ConsoleFilter, SessionError};

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
    /// Programmatic file selection (Playwright setInputFiles semantics).
    /// Selector-addressed because file inputs are routinely hidden — the
    /// interactive index from State may not include them at all.
    SetFiles {
        selector: String,
        files: Vec<Value>,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Scroll {
        direction: ScrollDirection,
        amount: u32,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Eval {
        script: String,
        /// Await budget for the script's promise, ms (default 5000, clamped
        /// 100..120000). Slow page-side work — uploads through the page's
        /// own fetch — legitimately outlives the default; without this the
        /// caller hit a silent `result: null` at the 5s mark (0.4.1 taobao
        /// report) instead of a truthable EVAL_TIMEOUT error.
        timeout_ms: Option<u64>,
        reply: oneshot::Sender<Result<Value, SessionError>>,
    },
    /// The session's current page URL — the cheap identity probe behind
    /// GET /sessions (State is a full indexed page walk, too heavy to
    /// fan out per listing entry).
    Url {
        reply: oneshot::Sender<Result<String, String>>,
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
    GetViewport { reply: ViewportOverrideReply },
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
        /// Humanized trajectory (easing + wobble + timing jitter). `false`
        /// keeps the exact linear interpolation legacy callers/tests assert on.
        humanize: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Acknowledged by the session thread right before it exits. The closer
    /// waits on this to learn the thread actually stopped - without it, close
    /// replies ok while the thread is still pinned inside V8 (a runaway eval)
    /// and keeps burning CPU.
    Close { reply: oneshot::Sender<()> },
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
    /// One-call risk-control report: every anti-bot challenge the session's
    /// traffic hit, as structured rows (URL-shaped walls plus the
    /// 200-status MTop JSON bodies taobao's x5 answers with). Read-only, so
    /// not recorded in the action log. Reply is
    /// `{"url", "total", "events":[{url,method,status,kind,via}], "handoff"}`;
    /// `handoff` is present only when there is something to hand off.
    Challenges {
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
    /// Short challenge tag ("punish") when the navigation itself landed on
    /// an anti-bot wall — risk-control pages answer like ordinary pages, so
    /// the flag is the machine-readable verdict. Absent when it didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
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
    /// Account name when this session runs as a named login identity
    /// (see account.rs); absent for anonymous sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// True when the login state is snapshotted and the session revives by
    /// the same id after eviction or restart.
    pub persistent: bool,
}
