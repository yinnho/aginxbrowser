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
        let Some(state) = self.read_login_state(session_id).await else {
            return;
        };
        self.snapshots.save(session_id, &state.to_string());
    }

    /// Same read, but into the account store — the write-back target for
    /// account sessions. The learned verify spec and drawn persona from the
    /// previous record survive the refresh (a state write-back must not
    /// erase what account_verify taught or re-draw the identity's device).
    pub(super) async fn capture_account(&mut self, owner: &str, name: &str, session_id: &str) {
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
}
