//! Session state primitives: the id counter and idle/TTL constants, the
//! [`SessionError`] contract, the console ring drain, the console read
//! filter, and the session handle + snapshot persistence backends.
//! Split from the session module root (ARCHITECTURE.md P2); behavior
//! unchanged.
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::page::Page;

use super::commands::SessionCommand;

/// Monotonic counter for unique session IDs within a process.
pub(super) static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default maximum idle time before a session is evicted. Overridable per
/// session at create time (ttl_secs, clamped 60..3600) — a long workflow
/// shouldn't lose its login state to the idle reaper mid-run.
pub(super) const SESSION_TIMEOUT: Duration = Duration::from_secs(480); // 8 minutes

/// Ring buffer size for a session's console log (see the Console command).
const CONSOLE_RING_CAP: usize = 500;

/// Snapshots older than this are dropped instead of revived — a login
/// re-injected days later is more surprise than service. Purged lazily on
/// revival attempts and in the eviction sweep.
pub(super) const SNAPSHOT_MAX_AGE_SECS: i64 = 24 * 3600;

/// #88: budgets for the shutdown flush. Every live session is persisted
/// before the process exits, but a session stuck mid-command (navigation
/// deadline) must not hold the process past the supervisor's kill timeout —
/// it keeps whatever its last per-command snapshot held.
pub(super) const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_secs(60);
pub(super) const PER_SESSION_FLUSH_BUDGET: Duration = Duration::from_secs(10);

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
pub(super) fn drain_console(page: &mut Page, ring: &mut std::collections::VecDeque<Value>) {
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
    pub(super) fn matches(&self, entry: &Value) -> bool {
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
// Session handle
// ---------------------------------------------------------------------------

/// #195: the channel-layer old-frame answer's shared state. Parse-time
/// synchronous JS can hold the session's V8 thread for seconds — the actor
/// cannot poll the command channel while a script runs, so even the #193
/// in-arm serve (which polls inside the goto) waits for the script to
/// yield. The session thread publishes its pre-navigation state here (old
/// URL + last band frame) before starting a load and lowers `in_flight`
/// after; the sender side (`SessionManager::nav_frame_answer`) answers
/// default-shape screenshot polls from this snapshot without ever entering
/// the channel. Chrome's frame contract is unchanged: the old frame is
/// what the browser shows until the new document commits.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
#[derive(Default)]
pub(super) struct NavFrameShared {
    /// True while a navigation owns the page (raised before the load
    /// starts, lowered after it lands or fails).
    pub(super) in_flight: std::sync::atomic::AtomicBool,
    /// The pre-navigation URL + band frame, replaced on every navigation.
    /// Senders clone the Arc out under a short lock — no PNG copy per poll.
    #[cfg(feature = "screenshot")]
    pub(super) state: std::sync::Mutex<std::sync::Arc<NavFrameSnapshot>>,
}

/// One published pre-navigation frame: the page's URL at the moment the
/// navigation started plus the last band-frame cache entry (#189) — the
/// same pair the #193 in-arm serve reads from the page directly.
#[cfg(feature = "screenshot")]
#[derive(Default)]
pub(super) struct NavFrameSnapshot {
    pub(super) url: String,
    pub(super) frame: Option<crate::page::BandFrameCache>,
}

impl NavFrameShared {
    /// Publish the pre-navigation state and mark the navigation in flight.
    /// Must run before the load starts — from that point the V8 thread may
    /// sit inside synchronous page JS, beyond the actor's reach.
    #[cfg(feature = "screenshot")]
    pub(super) fn begin(&self, url: String, frame: Option<crate::page::BandFrameCache>) {
        *self.state.lock().expect("nav frame lock") =
            std::sync::Arc::new(NavFrameSnapshot { url, frame });
        self.in_flight.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The load is over (landed or failed) — sender-side old-frame answers
    /// stand down and polls go through the channel again.
    pub(super) fn end(&self) {
        self.in_flight.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(super) struct BrowserSession {
    pub(super) cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub(super) last_active: Instant,
    pub(super) timeout: Duration,
    /// keepalive sessions are exempt from the idle reaper — a long workflow
    /// with SSH/DB queries between browser steps must not lose its login
    /// state (real-device feedback ③). Lives until session_close or exit.
    pub(super) keepalive: bool,
    /// Remembered so session_clone can reproduce the egress path (a login
    /// behind a proxy breaks when the clone egresses directly).
    pub(super) use_proxy: bool,
    /// Login state (cookies + web storage + viewport + dialog policy) is
    /// snapshotted to the local store after every command, and the session
    /// revives under the same id from that snapshot after an idle eviction
    /// or a process restart (feedback ③). `session_close` drops the snapshot.
    pub(super) persistent: bool,
    /// Named login identity this session runs as (account.rs): `(owner,
    /// name)`. An account session builds its browser on the account's
    /// private jar and writes its state back to the account store after
    /// every command — the per-session snapshot/revive path is for
    /// anonymous sessions; for account sessions the account IS the
    /// persistence (a new session_create {account} picks up the warm jar).
    pub(super) account: Option<(String, String)>,
    /// #195: shared with this session's thread — see [`NavFrameShared`].
    /// The manager side reads it to answer default-shape screenshot polls
    /// while a navigation holds the V8 thread.
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub(super) nav_frame: std::sync::Arc<NavFrameShared>,
}

impl BrowserSession {
    pub(super) fn is_expired(&self) -> bool {
        !self.keepalive && self.last_active.elapsed() > self.timeout
    }
}

/// Where persistent-session snapshots live. Production delegates to the
/// local SQLite store (store.rs); tests inject a shared in-memory map — two
/// managers holding one Arc simulate a process restart.
#[cfg_attr(not(test), allow(dead_code))] // Memory is only constructed in tests
pub(super) enum SnapshotStore {
    Global,
    Memory(std::sync::Arc<std::sync::Mutex<HashMap<String, (String, i64)>>>),
}

impl SnapshotStore {
    pub(super) fn save(&self, id: &str, snapshot: &str) {
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
                m.lock()
                    .expect("snapshot map poisoned")
                    .insert(id.to_string(), (snapshot.to_string(), now));
            }
        }
    }

    pub(super) fn load(&self, id: &str) -> Option<(String, i64)> {
        match self {
            SnapshotStore::Global => crate::store::load_session_snapshot(id),
            SnapshotStore::Memory(m) => m.lock().expect("snapshot map poisoned").get(id).cloned(),
        }
    }

    pub(super) fn delete(&self, id: &str) {
        match self {
            SnapshotStore::Global => {
                crate::store::delete_session_snapshot(id);
            }
            SnapshotStore::Memory(m) => {
                m.lock().expect("snapshot map poisoned").remove(id);
            }
        }
    }

    pub(super) fn purge(&self, max_age_secs: i64) {
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

    /// Every snapshot id in the store, most recently saved first — startup
    /// restore (#88) enumerates the fleet the previous process flushed.
    pub(super) fn ids(&self) -> Vec<String> {
        match self {
            SnapshotStore::Global => crate::store::list_session_snapshot_ids(),
            SnapshotStore::Memory(m) => {
                let mut ids: Vec<String> = m
                    .lock()
                    .expect("snapshot map poisoned")
                    .keys()
                    .cloned()
                    .collect();
                ids.sort();
                ids
            }
        }
    }
}
