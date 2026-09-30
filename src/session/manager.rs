//! The session actor: [`SessionManager`] owns the id -> handle map
//! (create/send/close/evict/revive) and spawns [`actor::session_thread`], the
//! per-session loop that owns the Browser + Page and executes commands.
//! Split from the session module root (ARCHITECTURE.md P2); behavior
//! unchanged.
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::commands::{
    ScrollDirection, SessionCommand, SessionListEntry, SessionNavResponse,
};
use super::interact::{
    DOM_MEDIA_SCRIPT, click_by_index, click_xy, drag_xy, extract_indexed_state, input_by_index,
    merge_dom_candidates, set_files_by_selector,
};
use super::record::{RecordedAction, inject_storage_js};
use super::state::{
    BrowserSession, SessionError, SnapshotStore, SESSION_COUNTER, PER_SESSION_FLUSH_BUDGET,
    SESSION_TIMEOUT, SHUTDOWN_FLUSH_BUDGET, SNAPSHOT_MAX_AGE_SECS, drain_console,
};

// Session manager
// ---------------------------------------------------------------------------

pub struct SessionManager {
    pub(super) sessions: HashMap<String, BrowserSession>,
    pub(super) snapshots: SnapshotStore,
}

mod actor;
mod persist;

/// Global session manager, shared between HTTP handlers and MCP tools.
pub static SESSIONS: std::sync::LazyLock<tokio::sync::Mutex<SessionManager>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(SessionManager::new()));

/// Send one command through the global [`SESSIONS`] manager without holding
/// its lock while the reply is in flight (#193). HTTP handlers and MCP tools
/// used to lock SESSIONS across the whole `mgr.send(...).await`, so one long
/// command — a navigate loading for seconds, a Wait parked for up to 120s —
/// froze every session's calls, not just its own: the mid-load screenshot
/// poll queued 1.8s behind a 2.1s navigate even though the session loop
/// itself answered it in ~20ms. Three phases: validate/revive and clone the
/// command channel under the lock, await the reply lock-free, then
/// re-acquire the lock for the post-success persistence (the same tail
/// [`SessionManager::dispatch`] runs inline). Multi-step orchestrators
/// (create/clone, the account wizard, listings) still hold the lock — they
/// interleave several calls against one consistent map.
pub(crate) async fn send_command<T, E, F>(
    session_id: &str,
    make_cmd: F,
) -> Result<T, SessionError>
where
    T: Send + 'static,
    E: Into<SessionError> + Send + 'static,
    F: FnOnce(oneshot::Sender<Result<T, E>>) -> SessionCommand,
{
    let cmd_tx = {
        let mut mgr = SESSIONS.lock().await;
        mgr.prepare_send(session_id)?
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    cmd_tx
        .send(make_cmd(reply_tx))
        .map_err(|_| SessionError::ThreadDied("session thread died".to_string()))?;
    let result: Result<T, SessionError> = reply_rx
        .await
        .map_err(|_| SessionError::ThreadDied("session thread died".to_string()))?
        .map_err(Into::into);
    if result.is_ok() {
        let mut mgr = SESSIONS.lock().await;
        mgr.persist_after_success(session_id).await;
    }
    result
}

/// Read a session's idle budget through the global manager — reply stamping
/// for handlers that released the lock during the round trip
/// ([`send_command`]).
pub(crate) async fn expires_in_secs(session_id: &str) -> Option<u64> {
    SESSIONS.lock().await.expires_in_secs(session_id)
}

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
    /// With `account` (owner, name), the session runs as that named login
    /// identity: private cookie jar, state written back to the account
    /// store instead of a per-session snapshot (account.rs).
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
        account: Option<(String, String)>,
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
            account,
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
        account: Option<(String, String)>,
    ) {
        let timeout = Duration::from_secs(
            ttl_secs
                .unwrap_or(SESSION_TIMEOUT.as_secs())
                .clamp(60, 3600),
        );

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let thread_id = session_id.to_string();
        let thread_url = start_url.map(|s| s.to_string());
        let thread_account = account.clone();
        std::thread::Builder::new()
            .name(format!("session-{}", &thread_id[..8.min(thread_id.len())]))
            // Deep stack for the V8 isolate — see server::v8_stack_size.
            // A default 2 MB thread dies on minified SPA recursion
            // (juejin.cn class) before the page renders.
            .stack_size(crate::server::v8_stack_size())
            .spawn(move || {
                actor::session_thread(
                    thread_id,
                    thread_url,
                    use_proxy,
                    cookies,
                    storage,
                    thread_account,
                    cmd_rx,
                );
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
                account,
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
        let cookies_text = self
            .send(session_id, |reply| SessionCommand::Cookies { reply })
            .await?;
        let storage_text = self
            .send(session_id, |reply| SessionCommand::Storage { reply })
            .await?;
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

        let (use_proxy, keepalive, persistent, timeout, account) = {
            let s = self.sessions.get(session_id).ok_or_else(|| {
                SessionError::NotFound(format!("session not found: {}", session_id))
            })?;
            (
                s.use_proxy,
                s.keepalive,
                s.persistent,
                s.timeout,
                s.account.clone(),
            )
        };

        let url = cookies["url"].as_str().unwrap_or("about:blank").to_string();
        let cookie_list: Vec<String> = cookies["cookies"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let ls = storage["local_storage"].clone();
        let ss = storage["session_storage"].clone();
        let injected = if ls.as_object().is_none_or(|m| m.is_empty())
            && ss.as_object().is_none_or(|m| m.is_empty())
        {
            None
        } else {
            // Gate the clone on where the storage was actually read: the
            // same pending-injection contract as record replay (issue #141).
            // An about:blank capture keeps today's unconditional behavior.
            let mut blob = serde_json::json!({ "local_storage": ls, "session_storage": ss });
            if let Some(o) = crate::account::url_origin(&url) {
                blob["origin"] = Value::String(o);
            }
            Some(blob)
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
            // The clone runs as the same identity: same account, same live
            // jar (two tabs, one profile). An anonymous source stays None.
            // Cloned: the name is echoed into the response below.
            account.clone(),
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
        if let Some((_, name)) = &account {
            resp["account"] = serde_json::json!(name);
        }
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
            // keep the snapshot that revival reads. A non-persistent one
            // only carries a snapshot as a #88 restart bridge — expiry ends
            // the bridge too, so a later touch can't zombie-revive stale
            // login cookies.
            let revive = session.persistent;
            self.close_inner(session_id, !revive);
            if !revive {
                return Err(SessionError::Expired(format!(
                    "session expired: {}",
                    session_id
                )));
            }
        }
        if self.revive_session(session_id) {
            return self.dispatch(session_id, make_cmd).await;
        }
        Err(SessionError::NotFound(format!(
            "session not found: {}",
            session_id
        )))
    }

    /// Phase 1 of the lock-free round trip ([`send_command`]): the prologue
    /// of [`Self::send`] — liveness check, idle-expiry policy, snapshot
    /// revival — ending in the session's command channel instead of a
    /// dispatch, so the caller can await the reply without holding the
    /// manager lock.
    fn prepare_send(
        &mut self,
        session_id: &str,
    ) -> Result<mpsc::UnboundedSender<SessionCommand>, SessionError> {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if !session.is_expired() {
                session.last_active = Instant::now();
                return Ok(session.cmd_tx.clone());
            }
            // Idle-expired on access — same policy as send(): a persistent
            // session falls through to revival, anything else is Expired.
            let revive = session.persistent;
            self.close_inner(session_id, !revive);
            if !revive {
                return Err(SessionError::Expired(format!(
                    "session expired: {}",
                    session_id
                )));
            }
        }
        if self.revive_session(session_id) {
            if let Some(session) = self.sessions.get(session_id) {
                return Ok(session.cmd_tx.clone());
            }
        }
        Err(SessionError::NotFound(format!(
            "session not found: {}",
            session_id
        )))
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

    /// Dispatch one command to a live session and refresh the persisted
    /// login state after every successful command: persistent anonymous
    /// sessions snapshot to their session-id slot; account sessions write
    /// back to the account store (the account IS the persistence — no
    /// per-session snapshot, so nothing to revive from, by design).
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
        if result.is_ok() {
            self.persist_after_success(session_id).await;
        }
        result
    }

    /// Phase 3 of the lock-free round trip ([`send_command`]): the
    /// post-success tail of [`Self::dispatch`] — an account session's login
    /// state writes back to the account store, a persistent anonymous one
    /// snapshots to its session-id slot.
    async fn persist_after_success(&mut self, session_id: &str) {
        match self
            .sessions
            .get(session_id)
            .map(|s| (s.account.clone(), s.persistent))
        {
            Some((Some((owner, name)), _)) => {
                self.capture_account(&owner, &name, session_id).await;
            }
            Some((None, true)) => {
                self.capture_snapshot(session_id).await;
            }
            _ => {}
        }
    }


    /// Close and remove a session. Fire-and-forget; use [`Self::close_and_wait`]
    /// when the caller needs to know the session thread actually stopped.
    /// An explicit close also drops the persistent snapshot — "done" means
    /// done (idle eviction keeps it; that is the revive path).
    pub fn close(&mut self, session_id: &str) {
        self.close_inner(session_id, true);
    }

    pub(super) fn close_inner(&mut self, session_id: &str, drop_snapshot: bool) {
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
            if session
                .cmd_tx
                .send(SessionCommand::Close { reply: tx })
                .is_err()
            {
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
    /// A non-persistent session's snapshot (#88 restart bridge) goes with
    /// it — no zombie revival of an expired session. Opportunistically ages
    /// out snapshots nobody revived.
    pub fn evict_expired(&mut self) {
        let expired: Vec<(String, bool)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, s)| (id.clone(), s.persistent))
            .collect();
        for (id, persistent) in expired {
            self.close_inner(&id, !persistent);
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
                account: s.account.as_ref().map(|(_, name)| name.clone()),
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
