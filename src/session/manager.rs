//! The session actor: [`SessionManager`] owns the id -> handle map
//! (create/send/close/evict/revive) and spawns [`session_thread`], the
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
    BrowserSession, SessionError, SnapshotStore, SESSION_COUNTER, SNAPSHOT_MAX_AGE_SECS,
    SESSION_TIMEOUT, drain_console,
};

// Session manager
// ---------------------------------------------------------------------------

pub struct SessionManager {
    pub(super) sessions: HashMap<String, BrowserSession>,
    pub(super) snapshots: SnapshotStore,
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
                session_thread(
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
            // keep the snapshot that revival reads.
            let revive = session.persistent;
            self.close_inner(session_id, false);
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
        result
    }

    /// Read the full login state back from the live session and hand it to
    /// the snapshot store. Best-effort: any read failure just skips the
    /// save (the next successful command retries).
    async fn capture_snapshot(&mut self, session_id: &str) {
        let Some(state) = self.read_login_state(session_id).await else {
            return;
        };
        self.snapshots.save(session_id, &state.to_string());
    }

    /// Same read, but into the account store — the write-back target for
    /// account sessions. The learned verify spec and drawn persona from the
    /// previous record survive the refresh (a state write-back must not
    /// erase what account_verify taught or re-draw the identity's device).
    async fn capture_account(&mut self, owner: &str, name: &str, session_id: &str) {
        let Some(mut record) = self.read_login_state(session_id).await else {
            return;
        };
        if let Some((text, _)) = crate::account::load(owner, name) {
            if let Ok(prev) = serde_json::from_str::<Value>(&text) {
                if prev.get("verify").is_some_and(|v| v.is_object()) {
                    record["verify"] = prev["verify"].clone();
                }
                if prev.get("persona").is_some_and(|v| v.is_object()) {
                    record["persona"] = prev["persona"].clone();
                }
            }
        }
        if let Err(e) = crate::account::save(owner, name, &record.to_string()) {
            tracing::debug!("account save failed for {name}: {e}");
        }
    }

    /// The login-state read shared by both persistence paths: cookies,
    /// web storage, dialog policy and viewport pin, plus the session's
    /// proxy/keepalive/ttl flags. None when any read failed.
    async fn read_login_state(&self, session_id: &str) -> Option<Value> {
        let (use_proxy, keepalive, ttl) = match self.sessions.get(session_id) {
            Some(s) => (s.use_proxy, s.keepalive, s.timeout.as_secs()),
            None => return None,
        };
        let cookies = self
            .raw_send(session_id, |reply| SessionCommand::Cookies { reply })
            .await
            .and_then(Result::ok)?;
        let storage = self
            .raw_send(session_id, |reply| SessionCommand::Storage { reply })
            .await
            .and_then(Result::ok)?;
        let dialog = self
            .raw_send(session_id, |reply| SessionCommand::Dialog {
                action: "list".to_string(),
                prompt_text: None,
                reply,
            })
            .await
            .and_then(Result::ok)?;
        let viewport: Option<Option<(f32, f32, bool)>> = self
            .raw_send(session_id, |reply| SessionCommand::GetViewport { reply })
            .await
            .and_then(Result::ok);

        let parse = |text: String| serde_json::from_str::<Value>(&text).ok();
        let (cookies, storage, dialog) = (parse(cookies)?, parse(storage)?, parse(dialog)?);
        let cookie_list: Vec<String> = cookies["cookies"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Some(serde_json::json!({
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
        }))
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
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
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
        let accept = dialog.and_then(|d| d.get("policy")).and_then(Value::as_str) == Some("accept");
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
            // Snapshots are never from account sessions (they write to the
            // account store instead), so a revived session is anonymous.
            None,
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
        tracing::info!(
            session = session_id,
            "persistent session revived from snapshot"
        );
        true
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

// ---------------------------------------------------------------------------
// Replay script generation

// Session thread — owns Browser + Page
// ---------------------------------------------------------------------------

fn session_thread(
    _session_id: String,
    start_url: Option<String>,
    use_proxy: bool,
    cookies: Vec<String>,
    storage: Option<Value>,
    account: Option<(String, String)>,
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
                // Account sessions build on the account's private jar
                // (account.rs): same-account sessions share it (two tabs,
                // one profile), other accounts and the anonymous shared jar
                // never see this session's cookies. The jar is seeded from
                // the stored record, so a post-restart session_create
                // {account} starts where the last one left off. The
                // account's device persona rides along too (teach-once in
                // the record): its UA goes to the browser, its fp_seed pins
                // the page's JS hardware identity below — one stable device
                // per identity, not one shared fingerprint with two logins.
                let (browser, persona) = match &account {
                    Some((owner, name)) => {
                        let persona = crate::account::persona_for(owner, name, None);
                        (
                            crate::server::build_browser_for_account(
                                use_proxy,
                                "",
                                None,
                                crate::account::jar_for(owner, name),
                                Some(persona.user_agent.as_str()),
                            )
                            .expect("failed to build session browser"),
                            Some(persona),
                        )
                    }
                    None => (
                        crate::server::build_browser(use_proxy, "", None)
                            .expect("failed to build session browser"),
                        None,
                    ),
                };
                // Inject cookies before navigation so a session can start
                // already logged-in (cookies gathered from a prior session via
                // the Cookies command, or hand-exported). Mirrors /fetch. For
                // account sessions the browser's cookie store IS the account
                // jar, so first-time login imports land in the account too.
                if !cookies.is_empty() {
                    let target = start_url.as_deref().unwrap_or("");
                    crate::server::inject_cookies(&browser, &cookies, target);
                }
                let mut page = browser.new_page().await.expect("failed to create session page");

                // Pin the persona's hardware seed before the first
                // navigation: init_js re-bakes the seed into the JS runtime
                // on every navigation (#269), so a pre-goto pin holds for
                // the session's whole life — screen/dpr/GPU/canvas draw from
                // the account's identity, not a fresh random one per page.
                if let Some(persona) = &persona {
                    page.inner.set_fingerprint_seed(persona.fp_seed);
                }

                // Replay log: every page-changing action this session took,
                // in order. In-memory only, dies with the session; exported
                // explicitly via the Export command. Declared before the
                // initial navigation below (which moves start_url).
                let mut recorder: Vec<RecordedAction> = vec![RecordedAction::Create {
                    url: start_url.clone(),
                    use_proxy,
                    cookies: cookies.clone(),
                    storage: storage.clone(),
                    account: account.as_ref().map(|(_, name)| name.clone()),
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
                            // A failed navigation leaves the page with no JS
                            // runtime (js: None) — every eval would silently
                            // return null while document.URL still serves the
                            // stub string, which reads as "logged-in but
                            // scriptless". Chrome keeps a working JS context
                            // on its error page; land on about:blank for the
                            // same guarantee (live-reproven 2026-09-15: an
                            // x.com session behind an unreachable network
                            // returned Null for every flow script).
                            let _ = page.goto("about:blank").await;
                        }
                    }
                } else {
                    // A real browser tab is never runtime-less: about:blank
                    // carries a full JS context. A never-navigated Page has
                    // `js: None`, and eval fell back to a stub that returned
                    // Null for every script (only literal `document.title`
                    // and `document.URL` were served from Rust state) — an
                    // agent evaluating right after session_create saw silent
                    // nulls (the 0.4.1 report's intermittent `result: null`).
                    // Land on about:blank so the runtime exists from birth.
                    let _ = page.goto("about:blank").await;
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
                //
                // After a command the slice stretches to 1.5s: async work the
                // command started (a fetch fired from a submit/click handler,
                // promise chains, timers) needs event-loop turns to progress,
                // and only evaluating synchronously would strand it. The
                // drain lives in the pump arm so it stays cancellation-safe:
                // a queued command preempts it at once. Draining inside the
                // command handler instead blocked the loop for the full 1.5s
                // on every never-idle page — back-to-back commands each paid
                // ~1505ms (measured), which is the "eval takes seconds" class
                // of report.
                let mut drain_budget: u64 = 0;
                loop {
                    let cmd = tokio::select! {
                        biased;
                        cmd = cmd_rx.recv() => match cmd {
                            Some(c) => c,
                            None => break,
                        },
                        _ = page.pump_event_loop_slice(if drain_budget > 0 { 1500 } else { 200 }) => {
                            drain_console(&mut page, &mut console_ring);
                            drain_budget = 0;
                            continue;
                        }
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
                                        let challenge =
                                            crate::har::challenge_kind(&final_url).map(|s| s.to_string());
                                        element_map.clear();
                                        pages_loaded += 1;
                                        Ok(SessionNavResponse { url: final_url, title, challenge })
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

                        SessionCommand::SetFiles { selector, files, reply } => {
                            let result = set_files_by_selector(&mut page, &selector, &files);
                            recorder.push(RecordedAction::SetFiles {
                                selector,
                                names: files
                                    .iter()
                                    .filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_owned))
                                    .collect(),
                                ok: result
                                    .as_ref()
                                    .map(|v| v.get("set").and_then(Value::as_bool).unwrap_or(false))
                                    .unwrap_or(false),
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

                        SessionCommand::Drag { from_x, from_y, to_x, to_y, steps, delay_ms, humanize, reply } => {
                            let steps = steps.clamp(1, 200);
                            let delay_ms = delay_ms.min(1000);
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    drag_xy(&mut page, from_x, from_y, to_x, to_y, steps, delay_ms, humanize).await;
                                    let _ = page.process_pending_navigation().await;
                                    if page.url() != before {
                                        pages_loaded += 1;
                                    }
                                    Ok(serde_json::json!({
                                        "url": page.url(),
                                        "from": {"x": from_x, "y": from_y},
                                        "to": {"x": to_x, "y": to_y},
                                        "steps": steps,
                                        "humanized": humanize,
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

                        SessionCommand::Eval { script, timeout_ms, reply } => {
                            let outcome = page.evaluate_async_checked(&script, timeout_ms).await;
                            recorder.push(RecordedAction::Eval { script });
                            // Drain any JS-initiated navigation the script
                            // started (location.href / form submit) so the
                            // session's current URL moves with it — same
                            // policy as click_by_index below.
                            let _ = page.process_pending_navigation().await;
                            let _ = reply.send(outcome.map_err(SessionError::Eval));
                        }

                        SessionCommand::Url { reply } => {
                            let _ = reply.send(Ok(page.url()));
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
                            drain_console(&mut page, &mut console_ring);
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
                                    drain_console(&mut page, &mut console_ring);
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
                                let body_of = |rid: &str| page.inner.get_response_body(rid);
                                let mut payload = serde_json::json!({
                                    "url": page.url(),
                                    "total": events.len(),
                                    "requests": crate::har::compact_events(events),
                                });
                                // Anti-bot challenges answer 200, so they
                                // hide among successful rows — surface the
                                // count at the top level so an agent that
                                // just asks "did we get punished" doesn't
                                // have to scan every URL. Same detection as
                                // the Challenges command (URL shape + the
                                // MTop risk-control bodies), so the numbers
                                // agree.
                                let challenges =
                                    crate::har::challenge_rows(events, &body_of).len();
                                if challenges > 0 {
                                    payload["challenges"] = json!(challenges);
                                }
                                if include_bodies {
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

                                // The no-argument default is the hosted live
                                // page's frame poll, and it paints the LIVE
                                // tree's viewport band. The serialized
                                // re-parse below drops everything Chrome's
                                // outerHTML drops — dirty form values first
                                // of all: the live page exists to watch the
                                // agent type, and typed text lives in
                                // NodeData::Element::live_value, which a
                                // re-parse of page.content() cannot see. Any
                                // explicit size/full_page/selector request
                                // keeps the re-parse path unchanged.
                                let mut band_png: Option<(u32, u32, Vec<u8>)> = None;
                                if !full_page && selector.is_none() && width.is_none() && height.is_none() {
                                    let mut num = |expr: &str| {
                                        page.evaluate_with_timeout(
                                            expr,
                                            crate::page::INTERACTION_EVAL_TIMEOUT,
                                        )
                                        .as_f64()
                                        .unwrap_or(0.0) as f32
                                    };
                                    let (sx, sy) = (num("scrollX"), num("scrollY"));
                                    let vp = (w as f32, h as f32);
                                    if let Some((frame, missing)) =
                                        page.inner.viewport_band_frame(sx, sy, vp)
                                    {
                                        // Lazily fetch the images the band
                                        // found missing and repaint once — a
                                        // frame with placeholders beats a
                                        // stall (same deal as the video pump).
                                        let frame = if missing.is_empty() {
                                            frame
                                        } else {
                                            page.inner.fetch_band_images(missing).await;
                                            page.inner.viewport_band_frame(sx, sy, vp)
                                                .map(|(f, _)| f)
                                                .unwrap_or(frame)
                                        };
                                        band_png = crate::pages::png_of(
                                            frame.width,
                                            frame.height,
                                            &frame.rgba,
                                        )
                                        .ok()
                                        .map(|png| (frame.width, frame.height, png));
                                    }
                                }

                                let result = if let Some((pw, ph, png)) = band_png {
                                    use base64::{engine::general_purpose::STANDARD, Engine as _};
                                    Ok(serde_json::json!({
                                        "url": url,
                                        "width": pw,
                                        "height": ph,
                                        "image_base64": STANDARD.encode(&png),
                                        "format": "png",
                                    })
                                    .to_string())
                                } else {
                                    let html = page.content();
                                    let resources = crate::screenshot::prefetch_render_resources(
                                        &page, &url, &html, w as f32,
                                    )
                                    .await;
                                    crate::screenshot::render_html_to_png_diting(
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
                                    })
                                };
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

                        SessionCommand::Challenges { reply } => {
                            page.inner.sync_js_network_events();
                            let events = &page.inner.network_events;
                            let body_of = |rid: &str| page.inner.get_response_body(rid);
                            let rows = crate::har::challenge_rows(events, &body_of);
                            let mut payload = serde_json::json!({
                                "url": page.url(),
                                "total": rows.len(),
                                "events": rows,
                            });
                            if !rows.is_empty() {
                                // Which identity got walled matters as much as
                                // the wall itself — a multi-account run wants
                                // to know whether the scraper or the publisher
                                // hit risk control.
                                if let Some((_, name)) = &account {
                                    payload["account"] = json!(name);
                                }
                                // Human handoff (taobao 0.4.1 report P1-6):
                                // the engine detects and surfaces, it does not
                                // auto-bypass. A person opens the live view,
                                // solves the slider in this session, and the
                                // retry below rides the x5sec cookie that
                                // solving sets — same cookies, same persona,
                                // session continues.
                                payload["handoff"] = json!(
                                    "anti-bot wall detected — hand this session to a human: \
                                     open the live view (web/live.html), solve the challenge there, \
                                     then retry the same request in this session"
                                );
                            }
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Close { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }

                    // The post-command drain happens in the pump arm above
                    // (stretched slice, preemptible by the next command).
                    drain_console(&mut page, &mut console_ring);
                    drain_budget = 1500;
                }
            })
            .await;
    });
}

