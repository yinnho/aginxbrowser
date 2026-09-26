//! The session module's tests, split from the module root
//! (ARCHITECTURE.md P2). Indentation is one level deeper than the
//! original `mod tests` block — moving the lines was out of scope for a
//! pure move batch.
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::*;
    use super::commands::SessionListEntry;
    use super::interact::{humanized_drag_plan, XorShift64};
    use super::state::{SnapshotStore, SESSION_COUNTER};

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
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

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
                timeout_ms: None,
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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );

        let json: Value =
            serde_json::from_str(state.as_str().unwrap_or("null")).unwrap_or(Value::Null);
        let timeout = json
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let micro = json.get("micro").and_then(|v| v.as_bool()).unwrap_or(false);
        let ticks = json.get("ticks").and_then(|v| v.as_i64()).unwrap_or(0);
        // 1.5s idle at a 200ms interval ≈ 7 ticks; 3 is a safe floor that
        // still proves sustained pumping (one post-command drain gives 0-1).
        assert!(timeout, "setTimeout must fire during the idle gap");
        assert!(micro, "promise chain must settle during the idle gap");
        assert!(
            ticks >= 3,
            "interval must keep firing while idle, got {ticks} ticks"
        );
    }

    /// File-upload leg (the taobao product-publishing chain's missing piece):
    /// SetFiles assigns File objects through input.files, value reads back
    /// Chrome's fakepath string, FormData(form) carries the selection as real
    /// File entries, and a wrong selector / non-file target reports set:false
    /// instead of throwing.
    #[tokio::test]
    async fn set_files_assigns_selection_and_formdata_reads_it() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let armed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: r#"document.body.innerHTML = '<form id="f"><input type="file" name="up"><input type="text" name="t"></form>'; 'ok'"#.to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(armed.as_str().unwrap_or(""), "ok");

        let specs = vec![
            serde_json::json!({"name": "a.png", "content_base64": "aGk=", "mime_type": "image/png"}),
            serde_json::json!({"name": "b.jpg", "content_base64": "Qg=="}),
        ];
        let result = mgr
            .send(&sid, |reply| SessionCommand::SetFiles {
                selector: "input[type=file]".to_string(),
                files: specs,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            result.get("set").and_then(Value::as_bool),
            Some(true),
            "assignment must succeed: {result}"
        );
        assert_eq!(result.get("count").and_then(Value::as_i64), Some(2));
        assert_eq!(
            result.get("value").and_then(Value::as_str),
            Some("C:\\fakepath\\a.png")
        );

        // The selection is live page state: FormData(form) sees real File
        // entries and the bytes round-trip through the File objects.
        let check = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: r#"(function() {
                    var el = document.querySelector('input[type=file]');
                    var entries = [];
                    for (var e of new FormData(document.getElementById('f')).entries())
                        entries.push(e[0] + ':' + (typeof File === 'function' && e[1] instanceof File ? e[1].name + '/' + e[1].size : e[1]));
                    return entries.join('|') + '#' + el.files.item(1).name + '#' + atob('aGk=');
                })()"#
                    .to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(check.as_str().unwrap_or(""), "up:a.png/2|up:b.jpg/1|t:#b.jpg#hi");

        // Wrong selector and non-file target report set:false, never throw.
        let miss = mgr
            .send(&sid, |reply| SessionCommand::SetFiles {
                selector: "input#nope".to_string(),
                files: vec![serde_json::json!({"name": "x", "content_base64": ""})],
                reply,
            })
            .await
            .unwrap();
        assert_eq!(miss.get("set").and_then(Value::as_bool), Some(false));
        assert!(
            miss.get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("no element"),
            "miss should name the failure: {miss}"
        );

        let not_file = mgr
            .send(&sid, |reply| SessionCommand::SetFiles {
                selector: "input[type=text]".to_string(),
                files: vec![serde_json::json!({"name": "x", "content_base64": ""})],
                reply,
            })
            .await
            .unwrap();
        assert_eq!(not_file.get("set").and_then(Value::as_bool), Some(false));
        assert!(
            not_file
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("not a file input"),
            "non-file target should be named: {not_file}"
        );

        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");
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
            .send(&"s_missing".to_string(), |reply| SessionCommand::State {
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
        assert_eq!(err.to_value()["hint"], "call session_create to recreate");

        // Command failure inside a LIVE session: COMMAND_FAILED, no recreate
        // hint, and the session survives (a retry is safe).
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
        let err = mgr
            .send(&sid, |reply| SessionCommand::Click {
                index: 99_999,
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "COMMAND_FAILED");
        assert!(err.to_value().get("hint").is_none());
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );

        // Expired: SESSION_EXPIRED + destructive close, then the same id
        // answers SESSION_NOT_FOUND — the two conclusions agree.
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
        mgr.sessions.get_mut(&sid).unwrap().last_active =
            Instant::now() - Duration::from_secs(3600);
        let err = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_EXPIRED");
        assert_eq!(err.to_value()["hint"], "call session_create to recreate");
        assert!(
            !mgr.sessions.contains_key(&sid),
            "expiry must close the session"
        );
        let err = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    /// GET /sessions identity probe (0.4.1 taobao report problem 7): the Url
    /// command must answer cheaply with the session's current URL — it is
    /// fanned out per listing entry, so anything heavier than a read would
    /// make the listing itself expensive.
    #[tokio::test]
    async fn url_command_round_trips_the_current_page() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
        let url = mgr
            .send(&sid, |reply| SessionCommand::Url { reply })
            .await
            .unwrap();
        assert_eq!(url, "about:blank");
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
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
            None,
        );

        let clicked = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.querySelector('#go').click(); 'clicked'".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(clicked.as_str().unwrap_or(""), "clicked");

        let href = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "location.href".to_string(),
                timeout_ms: None,
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

    /// Index of the element carrying `id="…"` in a session_state listing —
    /// the observe half of the interaction contract hands back formatted
    /// text, so the act-side tests parse their target's index from it.
    fn state_index_of(state: &str, id: &str) -> usize {
        let needle = format!("id=\"{}\"", id);
        for line in state.lines() {
            if line.contains(&needle) {
                if let Some(n) = line
                    .trim_start_matches('[')
                    .split(']')
                    .next()
                    .and_then(|s| s.parse().ok())
                {
                    return n;
                }
            }
        }
        panic!("no indexed element with {} in state:\n{}", needle, state);
    }

    /// Act-side re-check contract (issue #46): between session_state and
    /// session_click the page can re-render — the click must re-verify
    /// visibility/occlusion in the same frame and fail with a structured
    /// reason instead of dispatching into a dead target. A full-cover overlay
    /// answers `covered_by` naming the veil; once the veil is gone the same
    /// click succeeds.
    #[tokio::test]
    async fn click_refuses_covered_button_with_structured_reason() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /p",
            "<html><body><button id='go' onclick=\"window.__hit = 1\">Go</button>\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/p")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let state = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap();
        let idx = state_index_of(&state, "go");

        // Drop a z-index overlay over the whole viewport after the snapshot.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "(function(){ var d = document.createElement('div');\
                     d.id = 'veil';\
                     d.style.cssText = 'position:absolute;left:0;top:0;width:1600px;height:900px;z-index:9999';\
                     document.body.appendChild(d); return 'veiled'; })()"
                .to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();

        let resp = mgr
            .send(&sid, |reply| SessionCommand::Click { index: idx, reply })
            .await
            .unwrap();
        assert!(!resp.clicked, "a covered button must not report clicked");
        assert_eq!(resp.reason.as_deref(), Some("covered_by"));
        let covered = resp.covered_by.unwrap_or_default();
        assert!(
            covered.contains("veil"),
            "covered_by must name the element eating the click, got {}",
            covered
        );

        // Remove the veil: the identical click now dispatches.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.getElementById('veil').remove(); 'gone'".to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();
        let resp = mgr
            .send(&sid, |reply| SessionCommand::Click { index: idx, reply })
            .await
            .unwrap();
        assert!(resp.clicked, "uncovered button must click");
        assert!(resp.reason.is_none(), "success carries no reason");
        let hit = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__hit === 1 ? 'hit' : 'miss'".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(hit.as_str().unwrap_or(""), "hit");

        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
    }

    /// Same contract, disabled/hidden half: a control the page disabled or
    /// hid after the snapshot answers `disabled` / `not_visible` — never a
    /// silent success.
    #[tokio::test]
    async fn click_refuses_disabled_and_hidden_buttons() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /p",
            "<html><body><button id='b1'>One</button><button id='b2'>Two</button>\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/p")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let state = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap();
        let b1 = state_index_of(&state, "b1");
        let b2 = state_index_of(&state, "b2");

        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.getElementById('b1').disabled = true;\
                     document.getElementById('b2').style.display = 'none';\
                     'set'"
                .to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();

        let resp = mgr
            .send(&sid, |reply| SessionCommand::Click { index: b1, reply })
            .await
            .unwrap();
        assert!(!resp.clicked);
        assert_eq!(resp.reason.as_deref(), Some("disabled"));

        let resp = mgr
            .send(&sid, |reply| SessionCommand::Click { index: b2, reply })
            .await
            .unwrap();
        assert!(!resp.clicked);
        assert_eq!(resp.reason.as_deref(), Some("not_visible"));

        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
    }

    /// Input half of the contract: readonly and disabled controls answer
    /// `filled:false` with a named reason. Hidden inputs are deliberately
    /// NOT refused — a display:none input paired with a custom widget is a
    /// legitimate fill target (documented on the guard).
    #[tokio::test]
    async fn input_refuses_readonly_and_disabled_fields() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /p",
            "<html><body><input id='ok' name='q'><input id='dis' name='d' disabled>\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/p")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let state = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap();
        let ok = state_index_of(&state, "ok");
        let dis = state_index_of(&state, "dis");

        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.getElementById('ok').readOnly = true; 'set'".to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();

        let resp = mgr
            .send(&sid, |reply| SessionCommand::Input {
                index: ok,
                text: "hello".to_string(),
                full_events: false,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(resp["filled"], false, "readonly field must refuse");
        assert_eq!(resp["reason"], "readonly");

        let resp = mgr
            .send(&sid, |reply| SessionCommand::Input {
                index: dis,
                text: "hello".to_string(),
                full_events: false,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(resp["filled"], false, "disabled field must refuse");
        assert_eq!(resp["reason"], "disabled");

        // Lift the readonly flag: the same fill goes through with a readback.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.getElementById('ok').readOnly = false; 'ok'".to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();
        let resp = mgr
            .send(&sid, |reply| SessionCommand::Input {
                index: ok,
                text: "hello".to_string(),
                full_events: false,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(resp["filled"], true);
        assert_eq!(resp["value"], "hello");

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
            None,
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
                timeout_ms: None,
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
                timeout_ms: None,
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
                timeout_ms: None,
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
    async fn preload_runs_before_inline_scripts_and_persists_across_navigations() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // The document-start contract (#96): the preload wraps window.fetch
        // and plants a marker BEFORE the document's own scripts run — the
        // inline script below records what it saw. Eval-based patching can
        // never win this race; only preload can.
        let r = mgr
            .send(&sid, |reply| SessionCommand::SetPreload {
                scripts: vec![
                    "window.__pre = 'ran'; window.__pre_fetch = window.fetch;".to_string(),
                ],
                reply,
            })
            .await
            .unwrap();
        assert_eq!(r["count"].as_u64(), Some(1));

        let html = "<html><body><script>\
             window.__inline_saw = window.__pre || 'none';\
             window.__inline_fetch_wrapped = window.__pre_fetch === window.fetch;\
             </script></body></html>"
            .to_string();
        mgr.send(&sid, |reply| SessionCommand::SetContent {
            html,
            reply,
        })
        .await
        .unwrap();
        let saw = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__inline_saw".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(saw.as_str(), Some("ran"));
        let wrapped = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__inline_fetch_wrapped".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            wrapped.as_bool(),
            Some(true),
            "inline script must see the SAME fetch the preload captured"
        );

        // The group survives a navigation: a second document without any
        // inline reader still gets the preload (marker present).
        mgr.send(&sid, |reply| SessionCommand::SetContent {
            html: "<html><body><p>two</p></body></html>".to_string(),
            reply,
        })
        .await
        .unwrap();
        let persisted = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__pre || 'gone'".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(persisted.as_str(), Some("ran"));

        // Clearing works: an empty group leaves the next document untouched.
        mgr.send(&sid, |reply| SessionCommand::SetPreload {
            scripts: vec![],
            reply,
        })
        .await
        .unwrap();
        mgr.send(&sid, |reply| SessionCommand::SetContent {
            html: "<html><body><p>three</p></body></html>".to_string(),
            reply,
        })
        .await
        .unwrap();
        let cleared = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__pre || 'gone'".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(cleared.as_str(), Some("gone"));

        assert!(mgr.close_and_wait(&sid).await);
    }

    #[tokio::test]
    async fn set_content_loads_local_html_as_a_real_page() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(ran.as_i64(), Some(1));
        let state = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap();
        assert!(
            state.contains("hello"),
            "state must see the DOM, got: {state}"
        );

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
                timeout_ms: None,
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

    // A session created without a url (and one whose initial navigation
    // fails) must still own a live JS context, like a real browser's blank
    // tab. Until 2026-09-15 a never-navigated Page kept `js: None` and every
    // eval silently returned null — only literal `document.title`/`document.
    // URL` were served from Rust state, so agents saw "the page exists but no
    // script runs" (the 0.4.1 report's intermittent `result: null`).
    #[tokio::test]
    async fn no_url_session_has_a_live_js_context_from_birth() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        let sid = mgr.create(None, false, vec![], None, None, None, false, false, None);

        let two = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1+1".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(two.as_i64(), Some(2), "eval must run, got: {two}");

        // Side effects persist — the script really executed.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "window.__born = 42; 0".to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();
        let born = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__born".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(born.as_i64(), Some(42));

        // The exception contract survives: a throw is an Err, not a null.
        let threw = mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "throw new Error('born-live')".to_string(),
            timeout_ms: None,
            reply,
        })
        .await;
        assert!(threw.is_err(), "throw must surface as Err, got: {threw:?}");

        assert!(mgr.close_and_wait(&sid).await);
    }

    // Same guarantee when the initial navigation itself fails: Chrome keeps a
    // working JS context on its error page, so the agent can eval, read the
    // console entry, and navigate on from the dead URL.
    #[tokio::test]
    async fn failed_initial_navigation_still_leaves_a_live_js_context() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        // Port 1 on loopback: nothing listens, connection refused immediately.
        let sid = mgr.create(
            Some("http://127.0.0.1:1/"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let two = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1+1".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            two.as_i64(),
            Some(2),
            "eval after failed initial nav must run, got: {two}"
        );

        // And the session stays usable: navigating elsewhere works.
        let _ = mgr
            .send(&sid, |reply| SessionCommand::SetContent {
                html: "<html><body><p id='ok'>landed</p></body></html>".to_string(),
                reply,
            })
            .await
            .unwrap();

        let landed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.getElementById('ok').textContent".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(landed.as_str(), Some("landed"));

        assert!(mgr.close_and_wait(&sid).await);
    }

    // #80: a failed navigation must surface through eval/state — a null eval
    // result answers with the navigation error, real values pass through,
    // state never degrades to a bare non-string complaint, and the overlay
    // clears once a later navigation succeeds.
    #[tokio::test]
    async fn eval_and_state_report_the_failed_navigation() {
        let _net = crate::server::test_util::net_env_guard();
        let mut mgr = SessionManager::new();
        // Port 1 on loopback: nothing listens, connection refused immediately.
        let sid = mgr.create(
            Some("http://127.0.0.1:1/"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // A null result on the fallback page reads as "page broken"; the
        // navigation failure is the actionable truth.
        let probed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.querySelector('#missing')".to_string(),
                timeout_ms: None,
                reply,
            })
            .await;
        let err = probed.unwrap_err().to_string();
        assert!(
            err.contains("navigation failed") && err.contains("did not complete"),
            "null eval must name the failed navigation, got: {err}"
        );

        // Real values pass through — the fallback page is alive.
        let href = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "location.href".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(href.as_str(), Some("about:blank"));

        // State may legitimately succeed on the live fallback page, but an
        // error must name the navigation, not just the stringness complaint.
        let state = mgr.send(&sid, |reply| SessionCommand::State { reply }).await;
        match state {
            Ok(text) => assert!(
                !text.contains("non-string"),
                "state must not leak the bare extraction complaint: {text}"
            ),
            Err(e) => assert!(
                e.to_string().contains("navigation failed"),
                "state error must name the failed navigation, got: {e}"
            ),
        }

        // A later successful navigation clears the overlay: null is null again.
        mgr.send(&sid, |reply| SessionCommand::SetContent {
            html: "<html><body></body></html>".to_string(),
            reply,
        })
        .await
        .unwrap();
        let probed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.querySelector('#missing')".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert!(
            probed.is_null(),
            "cleared overlay must pass null through, got: {probed}"
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
            None,
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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(user.as_str(), Some("miccim"));

        let w = mgr
            .send(&dup, |reply| SessionCommand::Eval {
                script: "innerWidth".to_string(),
                timeout_ms: None,
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
                timeout_ms: None,
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
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/shot")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // Mutate the DOM after load — the capture renders page.content(),
        // so the pixels must reflect the mutation, not the HTTP response.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.querySelector('h1').textContent = 'after'".to_string(),
            timeout_ms: None,
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
        assert!(
            png.len() > 1000,
            "non-trivial image, got {} bytes",
            png.len()
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// The hosted live page's frame poll is the no-argument screenshot, and
    /// it must paint the LIVE tree. Dirty form values live in
    /// NodeData::Element::live_value (mirrored from the JS value setter),
    /// which a re-parse of serialized HTML cannot see — Chrome's outerHTML
    /// drops them too. Pinned differentially: count dark text-ink pixels
    /// inside the input's rect before and after typing. The re-parse path
    /// renders the value ATTRIBUTE (empty) and leaves the count unchanged.
    #[cfg(feature = "screenshot")]
    #[tokio::test]
    async fn screenshot_default_frame_paints_typed_input_value() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /live",
            "<html><body style=\"margin:0\">\
             <input id=q style=\"position:absolute;left:8px;top:8px;width:240px;height:32px\">\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/live")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        async fn ink_in_input(mgr: &mut SessionManager, sid: &str) -> usize {
            let shot = mgr
                .send(sid, |reply| SessionCommand::Screenshot {
                    width: None,
                    height: None,
                    full_page: false,
                    selector: None,
                    selector_all: false,
                    reply,
                })
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&shot).expect("screenshot JSON");
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let png = STANDARD
                .decode(v["image_base64"].as_str().expect("base64 body"))
                .expect("png bytes");
            let img = image::load_from_memory(&png).expect("decode png").to_rgb8();
            // Dark ink inside the input rect (8,8,240x32) — the box fill and
            // any border are constant across both shots, so only the typed
            // text moves the count.
            img.enumerate_pixels()
                .filter(|(x, y, p)| {
                    *x >= 10
                        && *x < 246
                        && *y >= 10
                        && *y < 38
                        && p.0[0] < 100
                        && p.0[1] < 100
                        && p.0[2] < 100
                })
                .count()
        }

        let before = ink_in_input(&mut mgr, &sid).await;
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "document.getElementById('q').value = 'typed hello world'".to_string(),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();
        let after = ink_in_input(&mut mgr, &sid).await;

        assert!(
            after > before + 40,
            "typed value must paint inside the input box (ink {before} → {after})"
        );
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
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/wait")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

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
            v["detail"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("Plan A"),
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
        let a = mgr.create(
            Some(&url),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        mgr.send(&a, |reply| SessionCommand::Eval {
            script: "localStorage.setItem('token','abc\"123'); \
                     localStorage.setItem('user','小张'); \
                     sessionStorage.setItem('cart','2 items'); 'ok'"
                .to_string(),
            timeout_ms: None,
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
        let b = mgr.create(
            Some(&url),
            false,
            vec![],
            Some(v),
            None,
            None,
            false,
            false,
            None,
        );
        let token = mgr
            .send(&b, |reply| SessionCommand::Eval {
                script: "localStorage.getItem('token') + '|' + localStorage.getItem('user') \
                         + '|' + sessionStorage.getItem('cart')"
                    .to_string(),
                timeout_ms: None,
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
        let long = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            Some(3600),
            None,
            false,
            false,
            None,
        );
        let short = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            Some(60),
            None,
            false,
            false,
            None,
        );
        let def = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let entries = mgr.list();
        let by_id = |list: &[SessionListEntry], id: &str| {
            list.iter()
                .find(|e| e.session_id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .expires_in_secs
                .expect("non-keepalive session carries a countdown")
        };
        assert!(
            by_id(&entries, &long) > 3500,
            "hour-long TTL, got {}",
            by_id(&entries, &long)
        );
        assert!(
            by_id(&entries, &short) <= 60,
            "60s TTL, got {}",
            by_id(&entries, &short)
        );
        assert!(
            by_id(&entries, &def) <= 480,
            "default stays at 8 minutes, got {}",
            by_id(&entries, &def)
        );

        // Clamp: out-of-range asks land on the rails.
        let clamped = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            Some(999_999),
            None,
            false,
            false,
            None,
        );
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
            None,
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
        assert_eq!(
            vp["width"].as_f64(),
            Some(375.0),
            "pin applied before first command"
        );
        assert_eq!(vp["mobile"].as_bool(), Some(true));

        let matches = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script:
                    "innerWidth + 'x' + innerHeight + '|' + matchMedia('(pointer:coarse)').matches"
                        .to_string(),
                timeout_ms: None,
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
                timeout_ms: None,
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
            None,
        );

        {
            let s = mgr.sessions.get_mut(&sid).expect("session");
            s.last_active = std::time::Instant::now() - std::time::Duration::from_secs(99_999);
        }

        assert!(
            mgr.expires_in_secs(&sid).is_none(),
            "keepalive has no countdown"
        );
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
                timeout_ms: None,
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
            None,
        );
        // The mutation is what the snapshot must capture — the page never
        // sets this key, so only injection can restore it.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "localStorage.setItem('user','miccim'); 'ok'".to_string(),
            timeout_ms: None,
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
                timeout_ms: None,
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
                timeout_ms: None,
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

        let p = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            true,
            None,
        );
        mgr.send(&p, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            timeout_ms: None,
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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(3),
            "expired persistent session revives"
        );
        assert!(
            mgr.sessions.contains_key(&p),
            "revived session is live again"
        );

        // Control: a plain session expires with the structured error.
        let plain = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
        mgr.send(&plain, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            timeout_ms: None,
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
                timeout_ms: None,
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

        let a = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            true,
            None,
        );
        mgr.send(&a, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            timeout_ms: None,
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

        let b = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            true,
            None,
        );
        mgr.send(&b, |reply| SessionCommand::Eval {
            script: "1".to_string(),
            timeout_ms: None,
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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(42),
            "evicted persistent session revives"
        );
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
        let id = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
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
            None,
        );

        let err = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1 + 1; throw new TypeError('boom at runtime')".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "EVAL_ERROR");
        let text = err.to_string();
        assert!(text.contains("TypeError: boom at runtime"), "got: {text}");
        assert!(
            text.contains("(line 1, col"),
            "throw position in message, got: {text}"
        );
        assert!(
            text.contains("<anonymous>:1:14"),
            "first stack frame carries the throw site, got: {text}"
        );

        // The session survives a failed eval and still evaluates fine.
        let ok = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "2 + 2".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(ok, serde_json::json!(4));

        let err = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "Promise.reject(new Error('async boom'))".to_string(),
                timeout_ms: None,
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
            None,
        );

        async fn read_log(mgr: &mut SessionManager, sid: &str) -> Vec<String> {
            let out = mgr
                .send(sid, |reply| SessionCommand::Eval {
                    script: "JSON.stringify(window.__log)".to_string(),
                    timeout_ms: None,
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
        // widgets (map markers) track. humanize:false pins the exact linear
        // interpolation; the humanized path has its own test below.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Drag {
                from_x: 60.0,
                from_y: 60.0,
                to_x: 150.0,
                to_y: 90.0,
                steps: 10,
                delay_ms: 0,
                humanize: false,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("drag JSON");
        assert_eq!(v["steps"], 10);
        assert_eq!(v["humanized"], false);

        let log = read_log(&mut mgr, &sid).await;
        let downs = log.iter().filter(|e| e.starts_with("down@60,60")).count();
        assert_eq!(downs, 1, "one press, got {log:?}");
        let moves: Vec<&String> = log.iter().filter(|e| e.starts_with("move@")).collect();
        assert_eq!(
            moves.len(),
            10,
            "every interpolated move delivered, got {log:?}"
        );
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

    /// #95: a coordinate click must reach the button inside a CLOSED shadow
    /// root — the xhs publish-button shape. Chrome's hit test pierces closed
    /// roots (elementFromPoint returns the composed target, which is why a
    /// human can press it in a real browser), so the engine's walk must too
    /// — while `host.shadowRoot` keeps reading null from page JS.
    #[tokio::test]
    async fn click_xy_pierces_closed_shadow_dom() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /closed",
            "<html><body style=\"margin:0\">\
             <xhs-btn id=\"host\" style=\"position:absolute;left:10px;top:10px;width:100px;height:20px;\"></xhs-btn>\
             <script>\
             var host = document.getElementById('host');\
             var root = host.attachShadow({mode:'closed'});\
             root.innerHTML = '<button id=\"inner\" style=\"width:100px;height:20px\">publish</button>';\
             root.getElementById('inner').addEventListener('click', function(e){\
                 window.__hit = e.target.id;\
             });\
             window.__hit = 'none';\
             </script>\
             </body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/closed")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // elementFromPoint at the host's center names the shadow-inner
        // button; the closed root stays invisible to page JS and the light
        // DOM stays empty.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "JSON.stringify({\
                    hit: document.elementFromPoint(60, 20).id,\
                    closed: document.getElementById('host').shadowRoot === null,\
                    light: document.getElementById('host').querySelector('button') === null\
                 })"
                .to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["hit"], "inner", "elementFromPoint must pierce the closed root");
        assert_eq!(v["closed"], true, "shadowRoot must stay null for a closed root");
        assert_eq!(v["light"], true, "the button must live in shadow, not light DOM");

        // The coordinate click path delivers the whole chain to the button.
        mgr.send(&sid, |reply| SessionCommand::ClickXY {
            x: 60.0,
            y: 20.0,
            button: "left".to_string(),
            click_count: 1,
            reply,
        })
        .await
        .unwrap();
        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "window.__hit".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            out, serde_json::json!("inner"),
            "the closed-shadow button must receive the click"
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// The humanized drag path (the default): eased velocity, perpendicular
    /// wobble off the straight line, and a landing exactly on the release
    /// point — one decimal place in the page log so sub-pixel shape is
    /// visible.
    #[tokio::test]
    async fn humanized_drag_lands_exact_and_wobbles() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /mousepad",
            "<html><body style=\"margin:0\">\
             <div id=\"pad\" style=\"position:absolute;left:10px;top:10px;width:300px;height:300px;\"></div>\
             <script>\
             var log = [];\
             var pad = document.getElementById('pad');\
             pad.addEventListener('mousedown', function(e){ log.push('down@'+e.clientX+','+e.clientY+' on '+e.target.id); });\
             document.addEventListener('mousemove', function(e){ log.push('move@'+Math.round(e.clientX*10)/10+','+Math.round(e.clientY*10)/10); });\
             document.addEventListener('mouseup', function(e){ log.push('up@'+Math.round(e.clientX*10)/10+','+Math.round(e.clientY*10)/10); });\
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
            None,
        );

        async fn read_log(mgr: &mut SessionManager, sid: &str) -> Vec<String> {
            let out = mgr
                .send(sid, |reply| SessionCommand::Eval {
                    script: "JSON.stringify(window.__log)".to_string(),
                    timeout_ms: None,
                    reply,
                })
                .await
                .unwrap();
            let raw = out.as_str().unwrap_or("[]");
            serde_json::from_str::<Vec<String>>(raw).expect("log array")
        }

        let out = mgr
            .send(&sid, |reply| SessionCommand::Drag {
                from_x: 60.0,
                from_y: 60.0,
                to_x: 150.0,
                to_y: 90.0,
                steps: 24,
                delay_ms: 0,
                humanize: true,
                reply,
            })
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).expect("drag JSON");
        assert_eq!(v["humanized"], true);
        assert_eq!(v["steps"], 24);

        let log = read_log(&mut mgr, &sid).await;
        assert!(
            log.iter().any(|e| e == "down@60,60 on pad"),
            "press at the start point, got {log:?}"
        );
        assert!(
            log.iter().any(|e| e == "up@150,90"),
            "release at the destination, got {log:?}"
        );

        let parse_move = |e: &String| -> Option<(f64, f64)> {
            let rest = e.strip_prefix("move@")?;
            let (x, y) = rest.split_once(',')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        };
        let moves: Vec<(f64, f64)> = log.iter().filter_map(parse_move).collect();
        assert!(
            (24..=26).contains(&moves.len()),
            "24 glide moves (+2 when the overshoot correction engages), got {}",
            moves.len()
        );

        // Minimum-jerk starts with near-zero velocity: the first move must
        // hug the start point (linear 1/24 would already be at x≈63.75).
        assert!(
            moves[0].0 < 62.0,
            "eased slow start, first move at {:?}, all: {moves:?}",
            moves[0]
        );

        // Wobble: at least one move deviates from the start→to line by more
        // than 0.3px (the envelope guarantees ≥0.7·amp ≥ 0.56px mid-drag).
        let dev = |p: (f64, f64)| {
            (90.0 * (p.1 - 60.0) - 30.0 * (p.0 - 60.0)) / 94.868
        };
        let max_dev = moves.iter().map(|p| dev(*p).abs()).fold(0.0, f64::max);
        assert!(
            max_dev > 0.3,
            "perpendicular wobble visible off the straight line, max {max_dev:.3}px"
        );

        // Landing: the final move sits on the release point (wobble envelope
        // and easing both die at t=1; overshoot corrects back).
        let last = *moves.last().unwrap();
        assert!(
            (last.0 - 150.0).abs() < 0.05 && (last.1 - 90.0).abs() < 0.05,
            "final move lands exactly on the release point, got {last:?}"
        );

        assert!(mgr.close_and_wait(&sid).await);
    }

    /// The generator itself, seeded: count bounds, exact landing, grip pause
    /// before the first move, settle pause in the plan, jittered timing, and
    /// the wobble never exceeding its amplitude budget.
    #[test]
    fn humanized_drag_plan_is_bounded_and_lands_exact() {
        let mut rng = XorShift64(42);
        let (fx, fy, tx, ty) = (60.0f64, 60.0f64, 250.0f64, 60.0f64);
        let plan = humanized_drag_plan(fx, fy, tx, ty, 24, 18, &mut rng);

        assert!(
            plan.points.len() == 24 || plan.points.len() == 26,
            "24 glide points (+2 overshoot correction), got {}",
            plan.points.len()
        );
        let last = plan.points.last().unwrap();
        assert!(
            (last.x - tx).abs() < 1e-9 && (last.y - ty).abs() < 1e-9,
            "final point is exactly the release point"
        );

        // Grip pause after mousedown, settle pause before mouseup.
        assert!(
            plan.points[0].delay_ms >= 3 * 18,
            "first move waits out the grip pause, got {}",
            plan.points[0].delay_ms
        );
        assert!(
            plan.settle_ms >= 5 * 18,
            "settle pause present, got {}",
            plan.settle_ms
        );

        // Timing jitter: delays vary, none runs away, none below the
        // 0.55×mean floor (except the first, which is the grip pause).
        let delays: std::collections::HashSet<u64> =
            plan.points.iter().map(|p| p.delay_ms).collect();
        assert!(delays.len() > 1, "per-step timing jittered, {delays:?}");
        for p in &plan.points[1..] {
            assert!(
                (9..=120).contains(&p.delay_ms),
                "delay within mean±hesitation budget, got {}",
                p.delay_ms
            );
        }

        // Geometry: progress stays within [start, target+overshoot cap]; the
        // only backward motion allowed is the small overshoot pull-back
        // (≤ 6px overshoot ⇒ ≤ 3.6px per corrective step).
        let dx = tx - fx;
        let dist = dx.hypot(ty - fy);
        let mut prev_along = -1.0f64;
        for p in &plan.points {
            let along = ((p.x - fx) * dx + (p.y - fy) * (ty - fy)) / dist;
            assert!(
                along >= prev_along - dist * 0.05 && along <= dist + 8.0,
                "progress stays within [start, target+overshoot cap], got {along}"
            );
            prev_along = along;
            let wobble = (90.0 * (p.y - fy) - (ty - fy) * (p.x - fx)) / dist;
            assert!(
                wobble.abs() <= 2.6,
                "wobble within amplitude budget, got {wobble}"
            );
        }
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
            None,
        );

        // Output produced by an agent-driven eval joins the same ring.
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: "console.error('eval-time boom'); 'done'".to_string(),
            timeout_ms: None,
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
        let texts: Vec<&str> = msgs.iter().filter_map(|m| m["text"].as_str()).collect();
        assert!(
            texts.contains(&"boot ok"),
            "log from page script, got {texts:?}"
        );
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
            None,
        );

        // Default policy: auto-dismiss. confirm → false, prompt → null.
        let out = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "[String(confirm('delete it?')), String(prompt('your name'))].join('|')"
                    .to_string(),
                timeout_ms: None,
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
        assert_eq!(
            payloads[0]["dialog"], "confirm",
            "kind tagged: {payloads:?}"
        );
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
                timeout_ms: None,
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
        assert_eq!(
            v["dialogs"].as_array().map(Vec::len),
            Some(5),
            "2 + 3 logged"
        );

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
                timeout_ms: None,
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
        let a = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = mgr.create(
            Some("about:blank"),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let entries = mgr.list();
        assert_eq!(entries.len(), 2);
        let ids: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
        assert!(
            entries[0].idle_secs <= entries[1].idle_secs,
            "most recent first"
        );
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
        assert!(
            script.contains(r#"POST "session/$SID/state" '{}'"#),
            "ends by printing final state"
        );
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
        assert!(
            !script.contains("$SID"),
            "no SID may be referenced without a create"
        );
    }

    /// A live session records what it did: create params + one entry per
    /// navigate/scroll/eval command, exportable as JSONL. Reads (State,
    /// Cookies) stay out of the log — replaying them is meaningless.
    #[tokio::test]
    async fn session_records_actions_and_exports_jsonl() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some("about:blank"),
            false,
            vec!["k=v".to_string()],
            None,
            None,
            None,
            false,
            false,
            None,
        );

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
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();
        let jsonl = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );

        let actions: Vec<Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
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

    /// (#116) A hung script-initiated fetch is invisible in the request log
    /// (entries land at completion) — the tmall publish report's "still
    /// executing?" question. The Network command must surface it as
    /// `in_flight` while the transport hangs, with url/method/age.
    #[tokio::test]
    async fn network_lists_hung_scripted_fetch_in_flight() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /watch",
            "<html><body></body></html>",
        )]);

        // Accept the TCP connection, never answer — the fetch hangs until
        // its 30s read timeout, far beyond this test.
        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hang_port = hang.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut parked = Vec::new();
            for s in hang.incoming() {
                match s {
                    Ok(s) => parked.push(s),
                    Err(_) => break,
                }
            }
        });

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/watch")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // Fire-and-forget: the expression's completion value must be a
        // non-thenable so the eval returns synchronously (#114 wrapper).
        mgr.send(&sid, |reply| SessionCommand::Eval {
            script: format!(
                "(function(){{ fetch('http://127.0.0.1:{hang_port}/hang').catch(function(){{}}); return 'fired'; }})()"
            ),
            timeout_ms: None,
            reply,
        })
        .await
        .unwrap();

        // The async op dispatches on the first loop turn after the eval —
        // poll the Network command briefly rather than assuming ordering.
        let mut listed = None;
        for _ in 0..40 {
            let text = mgr
                .send(&sid, |reply| SessionCommand::Network {
                    media_only: false,
                    include_bodies: false,
                    url_contains: None,
                    body_max_chars: 0,
                    reply,
                })
                .await
                .unwrap();
            let val: Value = serde_json::from_str(&text).unwrap();
            if let Some(rows) = val["in_flight"].as_array() {
                if let Some(row) = rows
                    .iter()
                    .find(|r| r["url"].as_str().unwrap_or("").contains("/hang"))
                {
                    listed = Some(row.clone());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let row = listed.expect("hung fetch must surface in in_flight within ~2s");
        assert_eq!(row["method"], "GET");
        assert!(
            row["age_ms"].as_u64().unwrap_or(u64::MAX) < 10_000,
            "age must be wall-clock sane: {row}"
        );
    }

    /// (#130) Both Network keys are always present — `in_flight: []` and
    /// `challenges: 0` on a quiet page. An absent field can't be told apart
    /// from a wrong endpoint or a stale version by the reading agent, which
    /// is exactly the ambiguity the #119 retester hit ("can't treat a
    /// missing field as zero or as proof of death").
    #[tokio::test]
    async fn network_payload_keeps_in_flight_and_challenges_keys_when_empty() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[(
            "GET /quiet",
            "<html><body>nothing flying</body></html>",
        )]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/quiet")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        let text = mgr
            .send(&sid, |reply| SessionCommand::Network {
                media_only: false,
                include_bodies: false,
                url_contains: None,
                body_max_chars: 0,
                reply,
            })
            .await
            .unwrap();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(
            val["in_flight"].is_array(),
            "in_flight must be present as an array, got: {val}"
        );
        assert!(
            val["in_flight"].as_array().unwrap().is_empty(),
            "quiet page must answer zero in flight: {val}"
        );
        assert_eq!(
            val["challenges"], 0,
            "challenges must be present as a count, got: {val}"
        );
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
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/watch")),
            false,
            vec![],
            None,
            None,
            None,
            false,
            false,
            None,
        );

        // One pump cycle so the page's fetch() settles into the event queue.
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1".to_string(),
                timeout_ms: None,
                reply,
            })
            .await
            .unwrap();

        let media = mgr
            .send(&sid, |reply| SessionCommand::Network {
                media_only: true,
                include_bodies: false,
                url_contains: None,
                body_max_chars: 0,
                reply,
            })
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
            items[0]["url"]
                .as_str()
                .unwrap()
                .ends_with("/v/master.m3u8?token=1"),
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
            .send(&sid, |reply| SessionCommand::Network {
                media_only: false,
                include_bodies: false,
                url_contains: None,
                body_max_chars: 0,
                reply,
            })
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
            arr[0]["url"]
                .as_str()
                .unwrap()
                .ends_with("/v/master.m3u8?token=1"),
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
            doc["response"]["content"]["text"]
                .as_str()
                .unwrap()
                .contains("master.m3u8"),
            "document body retained as text"
        );

        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
    }
