//! Login-state persistence — the capture/revive half of `SessionManager`,
//! split from manager.rs (batch 220) to keep the command-loop file under
//! the layering audit's god-file cap. Capture writes a live session's
//! cookies + storage back to the session-snapshot store or the account
//! record after every successful command; revive brings a persistent
//! session back from its snapshot after idle eviction or a restart.

use super::*;
use serde_json::Value;

impl SessionManager {
    /// Read the full login state back from the live session and hand it to
    /// the snapshot store. Best-effort: any read failure just skips the
    /// save (the next successful command retries).
    pub(super) async fn capture_snapshot(&mut self, session_id: &str) {
        let Some(mut state) = self.read_login_state(session_id).await else {
            return;
        };
        // #88: a shutdown flush snapshots non-persistent sessions too. The
        // flag rides along so revival restores the session the operator
        // had — a plain ttl session must not come back immortal.
        if let Some(s) = self.sessions.get(session_id) {
            state["persistent"] = serde_json::json!(s.persistent);
        }
        self.snapshots.save(session_id, &state.to_string());
    }

    /// Same read, but into the account store — the write-back target for
    /// account sessions. The learned verify spec and drawn persona from the
    /// previous record survive the refresh (a state write-back must not
    /// erase what account_verify taught or re-draw the identity's device).
    /// The preserve-merge runs inside the store's lock (mutate_account), so
    /// a persona draw or verify teach landing mid-read cannot be clobbered
    /// by this save — the plain load→merge→save it replaced had that gap.
    pub(super) async fn capture_account(&mut self, owner: &str, name: &str, session_id: &str) {
        let Some(mut record) = self.read_login_state(session_id).await else {
            return;
        };
        if let Err(e) = crate::store::mutate_account(owner, name, |rec| {
            if rec.get("verify").is_some_and(|v| v.is_object()) {
                record["verify"] = rec["verify"].clone();
            }
            if rec.get("persona").is_some_and(|v| v.is_object()) {
                record["persona"] = rec["persona"].clone();
            }
            *rec = record;
        }) {
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
    pub(super) fn revive_session(&mut self, session_id: &str) -> bool {
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
            // Same origin gate as the clone path (issue #141): replay only
            // onto the site the snapshot was taken from.
            let mut blob = serde_json::json!({ "local_storage": ls, "session_storage": ss });
            if let Some(o) = crate::account::url_origin(snap["url"].as_str().unwrap_or("about:blank")) {
                blob["origin"] = Value::String(o);
            }
            Some(blob)
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
            // #88: a shutdown flush may snapshot a non-persistent session —
            // revive it as what it was, not immortal. Absent key = a
            // per-command snapshot (all persistent, the pre-#88 writers).
            snap["persistent"].as_bool().unwrap_or(true),
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
            "session revived from snapshot"
        );
        true
    }

    /// #88, shutdown half: an upgrade must not cost live sessions. Every
    /// live session — persistent or not — is persisted before the process
    /// exits: account sessions get one final account-store write-back,
    /// anonymous ones a snapshot under their id. Bounded per session and
    /// overall, so a session stuck mid-command (navigation deadline) keeps
    /// whatever its last per-command snapshot held instead of holding the
    /// process past the supervisor's kill timeout. Returns how many
    /// sessions were persisted.
    pub(crate) async fn flush_for_shutdown(&mut self) -> usize {
        self.evict_expired();
        let deadline = std::time::Instant::now() + SHUTDOWN_FLUSH_BUDGET;
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        let mut flushed = 0usize;
        for id in ids {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    flushed,
                    total = self.session_count(),
                    "shutdown flush budget spent — remaining sessions keep their last snapshot"
                );
                break;
            }
            let Some(account) = self.sessions.get(&id).map(|s| s.account.clone()) else {
                continue;
            };
            let one = tokio::time::timeout(PER_SESSION_FLUSH_BUDGET, async {
                match &account {
                    Some((owner, name)) => self.capture_account(owner, name, &id).await,
                    None => self.capture_snapshot(&id).await,
                }
            })
            .await;
            match one {
                Ok(()) => flushed += 1,
                Err(_elapsed) => tracing::warn!(
                    session = %id,
                    "shutdown flush timed out — keeping the last per-command snapshot"
                ),
            }
        }
        flushed
    }

    /// #88, startup half: the fleet the previous process flushed at shutdown
    /// revives eagerly under its own ids, before the listener opens — the
    /// first session_list after a binary swap shows the sessions the
    /// operator left running, login state intact. Stale snapshots age out
    /// inside revival instead of coming back. Returns how many sessions
    /// were restored.
    ///
    /// Gated by [`eager_restore_mode`] (`AGINXBROWSER_RESTORE_SESSIONS`):
    /// parallel dogfood ports sharing `~/.aginxbrowser/cache.db` otherwise
    /// revive every leftover keepalive SPA and wedge the fresh process
    /// (8129 / #224 class). Prefer per-instance `AGINXBROWSER_STORE_PATH`
    /// or `AGINXBROWSER_COOKIE_STORE_DIR` isolation; use the env when the
    /// store must stay shared. Lazy revive-on-first-touch is unchanged.
    pub(crate) fn restore_all(&mut self) -> usize {
        match eager_restore_mode() {
            EagerRestoreMode::Off => {
                tracing::info!(
                    "eager session restore disabled (AGINXBROWSER_RESTORE_SESSIONS=0) —                      snapshots stay on disk for lazy revive-on-touch;                      isolate with AGINXBROWSER_STORE_PATH / AGINXBROWSER_COOKIE_STORE_DIR                      when running parallel serves"
                );
                0
            }
            mode => {
                let ids = self.snapshots.ids();
                let mut restored = 0usize;
                let mut skipped_keepalive = 0usize;
                for id in ids {
                    if mode == EagerRestoreMode::SkipKeepalive {
                        let is_keepalive = self
                            .snapshots
                            .load(&id)
                            .and_then(|(text, _)| serde_json::from_str::<Value>(&text).ok())
                            .and_then(|v| v.get("keepalive").and_then(Value::as_bool))
                            .unwrap_or(false);
                        if is_keepalive {
                            skipped_keepalive += 1;
                            tracing::info!(
                                session = %id,
                                "skipping eager restore of keepalive snapshot                                  (AGINXBROWSER_RESTORE_SESSIONS=skip-keepalive) —                                  still revivable on first touch"
                            );
                            continue;
                        }
                    }
                    if self.revive_session(&id) {
                        restored += 1;
                    }
                }
                if skipped_keepalive > 0 {
                    tracing::info!(
                        restored,
                        skipped_keepalive,
                        "eager restore finished (keepalive snapshots deferred)"
                    );
                }
                restored
            }
        }
    }
}

/// How serve-start [`SessionManager::restore_all`] behaves.
///
/// Parsed from `AGINXBROWSER_RESTORE_SESSIONS` (case-insensitive, trimmed):
/// - unset / `1` / `true` / `on` / `all` → [`EagerRestoreMode::All`] (#88 default)
/// - `0` / `false` / `off` / `no` → [`EagerRestoreMode::Off`]
/// - `skip-keepalive` / `non-keepalive` / `no-keepalive` → [`EagerRestoreMode::SkipKeepalive`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EagerRestoreMode {
    All,
    Off,
    SkipKeepalive,
}

/// Test lock — parallel tests that flip `AGINXBROWSER_RESTORE_SESSIONS`
/// hold this so they do not observe each other's env writes.
#[cfg(test)]
pub(crate) static RESTORE_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn eager_restore_mode() -> EagerRestoreMode {
    match std::env::var("AGINXBROWSER_RESTORE_SESSIONS")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("") | Some("1") | Some("true") | Some("yes") | Some("on") | Some("all") => {
            EagerRestoreMode::All
        }
        Some("0") | Some("false") | Some("no") | Some("off") => EagerRestoreMode::Off,
        Some("skip-keepalive") | Some("non-keepalive") | Some("no-keepalive") => {
            EagerRestoreMode::SkipKeepalive
        }
        Some(other) => {
            tracing::warn!(
                value = other,
                "unknown AGINXBROWSER_RESTORE_SESSIONS — treating as all                  (want 0|off|skip-keepalive|all)"
            );
            EagerRestoreMode::All
        }
    }
}

#[cfg(test)]
mod restore_mode_tests {
    use super::*;

    #[test]
    fn eager_restore_mode_parses_env() {
        let _lock = RESTORE_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AGINXBROWSER_RESTORE_SESSIONS");
        assert_eq!(eager_restore_mode(), EagerRestoreMode::All);
        for v in ["1", "true", "ON", "all", ""] {
            std::env::set_var("AGINXBROWSER_RESTORE_SESSIONS", v);
            assert_eq!(eager_restore_mode(), EagerRestoreMode::All, "{v:?}");
        }
        for v in ["0", "false", "OFF", "no"] {
            std::env::set_var("AGINXBROWSER_RESTORE_SESSIONS", v);
            assert_eq!(eager_restore_mode(), EagerRestoreMode::Off, "{v:?}");
        }
        for v in ["skip-keepalive", "non-keepalive", "NO-KEEPALIVE"] {
            std::env::set_var("AGINXBROWSER_RESTORE_SESSIONS", v);
            assert_eq!(eager_restore_mode(), EagerRestoreMode::SkipKeepalive, "{v:?}");
        }
        std::env::remove_var("AGINXBROWSER_RESTORE_SESSIONS");
    }
}
