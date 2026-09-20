//! The flow module's tests, split from the module root (ARCHITECTURE.md
//! P2 god-file ratchet).
use super::*;

#[test]
fn recorded_to_flow_maps_strips_and_drops_failures() {
    let jsonl = [
        r#"{"action":"create","url":"https://x.com/","use_proxy":true,"cookies":["auth_token=SECRET"]}"#,
        r#"{"action":"navigate","url":"https://x.com/login","ok":false}"#,
        r#"{"action":"navigate","url":"https://x.com/home","ok":true}"#,
        r#"{"action":"click","index":3,"ok":true}"#,
        r#"{"action":"input","index":2,"text":"hello","ok":false}"#,
        r#"{"action":"scroll","direction":"down","amount":2}"#,
        r#"{"action":"eval","script":"1+1"}"#,
    ]
    .join("\n");
    let doc = recorded_to_flow(&jsonl);
    // Secrets stripped from the create block; url + use_proxy survive.
    assert_eq!(doc["create"]["url"], "https://x.com/");
    assert_eq!(doc["create"]["use_proxy"], true);
    assert!(doc["create"].get("cookies").is_none());
    let steps = doc["steps"].as_array().unwrap();
    // ok:false navigate and input dropped, ok:true kept.
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[0]["op"], "navigate");
    assert_eq!(steps[0]["args"]["url"], "https://x.com/home");
    assert_eq!(steps[1]["op"], "click");
    assert_eq!(steps[1]["args"]["index"], 3);
    assert_eq!(steps[2]["op"], "scroll");
    assert_eq!(steps[3]["op"], "eval");
}

#[test]
fn recorded_to_flow_emits_set_files_names_only_skeleton() {
    let jsonl = r#"{"action":"set_files","selector":"input[type=file]","names":["a.png","b.png"],"ok":true}"#;
    let doc = recorded_to_flow(jsonl);
    let step = &doc["steps"][0];
    assert_eq!(step["op"], "set_files");
    assert_eq!(step["args"]["selector"], "input[type=file]");
    // Names carried through; no content_base64 key anywhere — the skeleton
    // the author fills in during curation.
    assert_eq!(
        step["args"]["files"],
        json!([{ "name": "a.png" }, { "name": "b.png" }])
    );
}

#[test]
fn substitute_handles_whole_embedded_nested_and_errors() {
    let mut vars = Map::new();
    vars.insert("q".into(), json!("rust engine"));
    vars.insert("n".into(), json!(3));
    // Whole-string replacement keeps type via string splice.
    assert_eq!(
        substitute(&json!("{{q}}"), &vars).unwrap(),
        json!("rust engine")
    );
    // Embedded + multiple + non-string vars stringified in place.
    assert_eq!(
        substitute(&json!("search {{q}} page={{n}}!"), &vars).unwrap(),
        json!("search rust engine page=3!")
    );
    // Nested objects and arrays are walked.
    assert_eq!(
        substitute(
            &json!({ "url": "https://s/?q={{q}}", "meta": ["{{n}}"] }),
            &vars
        )
        .unwrap(),
        json!({ "url": "https://s/?q=rust engine", "meta": ["3"] })
    );
    // Missing var and unterminated placeholder fail loudly.
    assert!(substitute(&json!("{{missing}}"), &vars).is_err());
    assert!(substitute(&json!("{{oops"), &vars).is_err());
}

#[test]
fn workflow_name_rejects_traversal() {
    assert!(is_workflow_name("taobao-seller"));
    assert!(!is_workflow_name("../etc"));
    assert!(!is_workflow_name("a/b"));
    assert!(!is_workflow_name("UPPER"));
    assert!(!is_workflow_name(""));
}

#[test]
fn resolve_flow_doc_requires_source() {
    // Neither flow nor name.
    assert!(resolve_flow_doc(None, None).is_err());
    // Inline without steps.
    assert!(resolve_flow_doc(Some(json!({ "create": {} })), None).is_err());
    // Traversal-shaped name rejected before touching the filesystem.
    assert!(resolve_flow_doc(None, Some("../secrets")).is_err());
}

#[test]
fn js_truthy_matches_js_semantics() {
    assert!(!js_truthy(&json!(0)));
    assert!(!js_truthy(&json!(0.0)));
    assert!(!js_truthy(&json!("")));
    assert!(!js_truthy(&json!(null)));
    assert!(!js_truthy(&json!(false)));
    assert!(js_truthy(&json!(1)));
    assert!(js_truthy(&json!(-1)));
    assert!(js_truthy(&json!("0")));
    assert!(js_truthy(&json!([0])));
    assert!(js_truthy(&json!({})));
}

/// Dotted paths walk objects and array indices; a miss anywhere along
/// the path is the same loud unknown-var error as a missing root.
#[test]
fn substitute_walks_dotted_paths() {
    let mut vars = Map::new();
    vars.insert(
        "resp".into(),
        json!({ "json": { "access_token": "abc123", "scopes": ["snsapi", "base"] } }),
    );
    assert_eq!(
        substitute(&json!("t={{resp.json.access_token}}"), &vars).unwrap(),
        json!("t=abc123")
    );
    // Array index leg.
    assert_eq!(
        substitute(&json!("{{resp.json.scopes.1}}"), &vars).unwrap(),
        json!("base")
    );
    // Non-index into an array, deep miss, and missing root all error.
    assert!(substitute(&json!("{{resp.json.scopes.x}}"), &vars).is_err());
    assert!(substitute(&json!("{{resp.json.nope}}"), &vars).is_err());
    assert!(substitute(&json!("{{root.nope}}"), &vars).is_err());
}

/// json arg interpolation: a whole-leaf placeholder embeds the VALUE
/// (strings with quotes re-serialize legally, objects embed
/// structurally), partial placeholders stay literal, and a clean
/// whole-leaf miss is a loud error.
#[test]
fn json_interpolate_replaces_whole_leaves_only() {
    let mut vars = Map::new();
    vars.insert("html".into(), json!("He said \"hi\" <b>&</b>"));
    vars.insert("meta".into(), json!({ "n": 3, "tags": ["a"] }));
    let out = json_interpolate(
        &json!({ "title": "{{html}}", "meta": "{{meta}}", "mixed": "x {{html}} y" }),
        &vars,
    )
    .unwrap();
    assert_eq!(out["title"], json!("He said \"hi\" <b>&</b>"));
    assert_eq!(out["meta"]["n"], json!(3));
    assert_eq!(out["meta"]["tags"], json!(["a"]));
    // Partial placeholder untouched (compose those in `body` instead).
    assert_eq!(out["mixed"], json!("x {{html}} y"));
    // Clean whole-leaf miss errors loudly.
    assert!(json_interpolate(&json!("{{missing}}"), &vars).is_err());
    // A multi-placeholder string is not a whole leaf — no error.
    assert_eq!(
        json_interpolate(&json!("{{html}} {{html}}"), &vars).unwrap(),
        json!("{{html}} {{html}}")
    );
}

/// End-to-end for the engine-side http step against the recording
/// server: JSON body interpolation (quotes survive), URL chaining off a
/// saved response's field, and the receipt carrying parsed json.
#[tokio::test]
async fn run_flow_http_step_posts_json_and_chains_saved_fields() {
    let _net = crate::server::test_util::net_env_guard();
    let (port, hits) = crate::server::test_util::recording_server(&[
        ("POST /token", r#"{"access_token":"abc123","expires_in":7200}"#),
        ("POST /draft", r#"{"media_id":"M1"}"#),
    ]);
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "http", "args": {
                "url": format!("http://127.0.0.1:{port}/token"),
                "method": "POST",
                "json": { "grant_type": "{{g}}" }
            }, "save": "t" },
            { "op": "http", "args": {
                "url": "http://127.0.0.1:{{port}}/draft?access_token={{t.json.access_token}}",
                "method": "POST",
                "json": { "title": "{{args.title}}" }
            }, "save": "d" },
        ]
    });
    let mut vars = Map::new();
    vars.insert("g".into(), json!("client_credential"));
    vars.insert("port".into(), json!(port.to_string()));
    // args as the JSON-string form — run_flow exposes it as an `args`
    // object, the same shape the wechat-oa-post flow consumes.
    vars.insert(
        "args_json".into(),
        json!("{\"title\": \"He said \\\"hi\\\" <b>&</b>\"}"),
    );
    let receipt = run_flow(&mut mgr, &flow, &vars, None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    assert_eq!(receipt["saved"]["t"]["json"]["access_token"], "abc123");
    assert_eq!(receipt["saved"]["d"]["json"]["media_id"], "M1");

    let hits = hits.lock().unwrap();
    let second = hits.iter().find(|h| h.contains("/draft")).expect("second request recorded");
    // URL carries the token spliced from step 1's saved response.
    assert!(second.contains(&format!("access_token=abc123")), "hit: {second}");
    // Hit format is "METHOD path PROTO body" (the recording server
    // extracts the body past the first header blank line) — the JSON
    // body stayed legal through interpolation, quotes and all.
    let body = second.splitn(4, ' ').nth(3).unwrap_or("");
    let parsed: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("body {body:?}: {e}"));
    assert_eq!(parsed["title"], "He said \"hi\" <b>&</b>");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// The multipart variant: base64 content lands as a well-formed
/// multipart/form-data body (boundary markers, disposition, filename).
#[tokio::test]
async fn run_flow_http_step_multipart_uploads() {
    let _net = crate::server::test_util::net_env_guard();
    let (port, hits) = crate::server::test_util::recording_server(&[(
        "POST /upload",
        r#"{"media_id":"THUMB"}"#,
    )]);
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "http", "args": {
                "url": format!("http://127.0.0.1:{port}/upload"),
                "method": "POST",
                "multipart": [
                    { "name": "media", "filename": "cover.png",
                      "content_type": "image/png", "content_base64": "aGVsbG8=" }
                ]
            }, "save": "up" },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    assert_eq!(receipt["saved"]["up"]["json"]["media_id"], "THUMB");

    let hits = hits.lock().unwrap();
    let hit = hits.iter().find(|h| h.contains("/upload")).expect("upload recorded");
    // The recording server logs "METHOD path PROTO body" — header lines
    // (the multipart content-type) aren't captured, so the contract is
    // pinned on the body itself: boundary markers wrap a well-formed part.
    let body = hit.splitn(4, ' ').nth(3).unwrap_or("");
    assert!(body.starts_with("--aginxbrowser-flow-"), "body: {body:?}");
    assert!(body.contains("Content-Disposition: form-data; name=\"media\"; filename=\"cover.png\""));
    assert!(body.contains("Content-Type: image/png"));
    // Decoded part content ("aGVsbG8=" = "hello") and the closing
    // boundary marker (hex suffix is time-derived).
    assert!(body.contains("hello"));
    assert!(body.trim_end().ends_with("--"));
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// An http step referencing a path that no earlier step saved fails at
/// THAT step with the unknown var named — not a silent literal splice.
#[tokio::test]
async fn run_flow_http_step_unknown_json_var_fails_loudly() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "http", "args": {
                "url": "https://api.example.invalid/x",
                "json": { "t": "{{never_saved.field}}" }
            } },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "failed");
    let reason = receipt["reason"].as_str().unwrap();
    assert!(reason.contains("never_saved.field"), "reason: {reason}");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// End-to-end on the no-network path: set_content loads a local page,
/// eval saves a value, expect selector holds, receipt is ok and the
/// session survives the run.
#[tokio::test]
async fn run_flow_ok_saves_and_keeps_session() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "set_content", "args": { "html": "<html><body><a id='t' href='/next'>hi {{who}}</a></body></html>" } },
            { "op": "eval", "args": { "script": "document.querySelector('#t').textContent" }, "save": "title" },
            { "op": "eval", "args": { "script": "window.scrollTo(0, 50)" }, "expect": { "selector": "#t" } },
        ]
    });
    let mut vars = Map::new();
    vars.insert("who".into(), json!("flow"));
    let receipt = run_flow(&mut mgr, &flow, &vars, None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    assert_eq!(receipt["steps_done"], 3);
    assert_eq!(receipt["saved"]["title"], "hi flow");

    // Session stays alive and queryable.
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    let state = mgr
        .send(&sid, |reply| SessionCommand::State { reply })
        .await
        .unwrap();
    assert!(state.contains("hi flow"));
    assert!(mgr.close_and_wait(&sid).await);
}

/// set_files in a flow: the upload lands on the page's file input and the
/// File metadata is readable back from JS — the same machinery
/// session_set_files drives.
#[tokio::test]
async fn run_flow_set_files_uploads() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "set_content", "args": { "html": "<html><body><input id='f' type='file'></body></html>" } },
            { "op": "set_files", "args": {
                "selector": "#f",
                "files": [
                    { "name": "a.txt", "content_base64": "aGVsbG8=" },
                    { "name": "empty.bin", "content_base64": "" }
                ]
            } },
            { "op": "eval", "args": {
                "script": "(() => { const fs = document.getElementById('f').files; return [fs.length, fs[0].name, fs[0].size, fs[1].name, fs[1].size].join('|'); })()"
            }, "save": "files" },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    assert_eq!(receipt["steps_done"], 3);
    // "hello" is 5 bytes; the deliberate 0-byte clear file is 0.
    assert_eq!(receipt["saved"]["files"], "2|a.txt|5|empty.bin|0");

    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// The names-only skeleton a recorded export emits must fail loudly at
/// the set_files step — not silently upload 0-byte files.
#[tokio::test]
async fn run_flow_set_files_names_only_skeleton_fails_loudly() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "set_content", "args": { "html": "<html><body><input id='f' type='file'></body></html>" } },
            { "op": "set_files", "args": { "selector": "#f", "files": [{ "name": "a.txt" }] } },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["failed_step"], 1);
    let reason = receipt["reason"].as_str().unwrap();
    assert!(reason.contains("content_base64"), "reason: {reason}");
    assert!(reason.contains("{{var}}"), "reason: {reason}");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// A failed expect aborts with the evidence bundle: failing step index,
/// reason naming the check, saved-so-far preserved.
#[tokio::test]
async fn run_flow_failure_receipt() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [
            { "op": "set_content", "args": { "html": "<html><body>hello</body></html>" } },
            { "op": "eval", "args": { "script": "document.body.innerText" }, "save": "seen" },
            { "op": "eval", "args": { "script": "1" }, "expect": { "text_contains": "definitely-absent" } },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["failed_step"], 2);
    let reason = receipt["reason"].as_str().unwrap();
    assert!(reason.contains("expect text_contains"), "reason: {reason}");
    assert_eq!(receipt["saved"]["seen"], "hello");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}

/// A passed session_id wins over the flow's create block — login state
/// rides in from import_curl/persistent sessions, the flow never re-creates.
#[tokio::test]
async fn run_flow_reuses_session_id() {
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
    let flow = json!({
        "create": { "url": "https://example.com/" },
        "steps": [ { "op": "set_content", "args": { "html": "<html><body>x</body></html>" } } ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), Some(sid.clone())).await;
    assert_eq!(receipt["status"], "ok");
    assert_eq!(receipt["session_id"], sid);
    assert!(mgr.close_and_wait(&sid).await);
}

/// The create block goes through {{var}} substitution too (its url is the
/// most common placeholder host — found live on x.com, where a raw
/// {{handle}} navigated literally and the flow died in a login redirect).
/// A create-block substitution failure fails before any session exists.
#[tokio::test]
async fn run_flow_substitutes_create_block() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "vars": { "q": "zz" },
        "create": { "url": "data:text/plain,hello-{{q}}" },
        "steps": [
            { "op": "eval", "args": { "script": "document.body.innerText" }, "save": "seen" },
        ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    assert_eq!(receipt["saved"]["seen"], "hello-zz");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);

    // Missing var: fails loudly, no session leaked into the manager.
    let bad = json!({
        "create": { "url": "data:text/plain,hello-{{missing}}" },
        "steps": []
    });
    let before = mgr.list().len();
    let r2 = run_flow(&mut mgr, &bad, &Map::new(), None).await;
    assert_eq!(r2["status"], "failed");
    let reason = r2["reason"].as_str().unwrap();
    assert!(
        reason.contains("create block") && reason.contains("missing"),
        "reason: {reason}"
    );
    assert!(r2["session_id"].is_null());
    assert_eq!(mgr.list().len(), before);
}

/// An args-declaring flow rejects unknown args_json keys before any step
/// runs — the mistyped-key reply-becomes-root-tweet incident, as a
/// contract. No session is created, the reason names both the unknown
/// key and the accepted set.
#[tokio::test]
async fn run_flow_args_validation_rejects_unknown_keys() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "args": ["text", "reply_to"],
        "steps": [ { "op": "set_content", "args": { "html": "<html><body>x</body></html>" } } ]
    });
    let mut vars = Map::new();
    vars.insert(
        "args_json".into(),
        json!({ "tweet_id": "2098960519113642269", "text": "hi" }),
    );
    let before = mgr.list().len();
    let receipt = run_flow(&mut mgr, &flow, &vars, None).await;
    assert_eq!(receipt["status"], "failed");
    let reason = receipt["reason"].as_str().unwrap();
    assert!(reason.contains("tweet_id"), "reason: {reason}");
    assert!(reason.contains("reply_to"), "reason: {reason}");
    assert_eq!(receipt["failed_step"], 0);
    assert_eq!(receipt["steps_done"], 0);
    assert!(receipt["session_id"].is_null());
    assert_eq!(mgr.list().len(), before);
}

/// All-known keys pass and the flow runs; a bad args_json JSON string is
/// rejected with the parse error rather than splicing into nothing.
#[tokio::test]
async fn run_flow_args_validation_allows_known_keys() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "args": ["text", "reply_to"],
        "vars": { "args_json": { "text": "hi", "reply_to": "" } },
        "steps": [ { "op": "set_content", "args": { "html": "<html><body>x</body></html>" } } ]
    });
    let receipt = run_flow(&mut mgr, &flow, &Map::new(), None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);

    // Non-JSON string as args_json: parse error, pre-session.
    let before = mgr.list().len();
    let mut vars = Map::new();
    vars.insert("args_json".into(), json!("not json at all"));
    let r2 = run_flow(&mut mgr, &flow, &vars, None).await;
    assert_eq!(r2["status"], "failed");
    let reason = r2["reason"].as_str().unwrap();
    assert!(reason.contains("not valid JSON"), "reason: {reason}");
    assert!(r2["session_id"].is_null());
    assert_eq!(mgr.list().len(), before);
}

/// No args declaration = no validation — flows that don't consume
/// args_json (recorder exports, the playbook flows) stay untouched.
#[tokio::test]
async fn run_flow_args_validation_skips_undeclared_flows() {
    let mut mgr = SessionManager::new();
    let flow = json!({
        "steps": [ { "op": "set_content", "args": { "html": "<html><body>x</body></html>" } } ]
    });
    let mut vars = Map::new();
    vars.insert("args_json".into(), json!({ "whatever": 1 }));
    let receipt = run_flow(&mut mgr, &flow, &vars, None).await;
    assert_eq!(receipt["status"], "ok", "receipt: {receipt}");
    let sid = receipt["session_id"].as_str().unwrap().to_string();
    assert!(mgr.close_and_wait(&sid).await);
}
