//! Flow JSON — record a browser session once, replay it with zero model
//! tokens. Two halves:
//!
//! 1. `recorded_to_flow` — the session_export `format=json` converter. Turns
//!    the recorded action log into a flow.json document: steps of
//!    `{op, args, expect?, save?}` plus a `create` block. Cookies and web
//!    storage are deliberately stripped here — a flow is a shareable asset
//!    (committed, rsynced, read by any agent), while the secret-bearing twin
//!    remains the local `format=bash` replay script.
//! 2. `run_flow` — the executor. Linear steps, `{{var}}` substitution,
//!    `expect` assertions, fail-with-receipt abort: the failing step, the
//!    reason, and a best-effort diagnostic screenshot come back so the
//!    calling agent can repair the flow or take the session over manually.
//!    The engine itself stays LLM-free — the repair loop is the caller's.
//!
//! Flows live as data assets in `workflow/<name>/flow.json` (override with
//! `AGINXBROWSER_WORKFLOW_DIR`), deliberately NOT baked into the binary: a
//! deployed instance gains a workflow by dropping a file, no rebuild.

use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::session::{ScrollDirection, SessionCommand, SessionManager};

// ---------------------------------------------------------------------------
// Recording → flow.json
// ---------------------------------------------------------------------------

fn step(op: &str, args: Value) -> Value {
    json!({ "op": op, "args": args })
}

/// Convert a recorded action log (JSONL, as returned by the Export command)
/// into a flow.json document. Actions recorded as failed (`ok:false`) were
/// probing attempts — dead weight in a replay — and are dropped; the flow is
/// the curated path.
pub fn recorded_to_flow(jsonl: &str) -> Value {
    let mut create = Map::new();
    let mut steps = Vec::new();
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("ok") == Some(&Value::Bool(false)) {
            continue;
        }
        match v["action"].as_str().unwrap_or_default() {
            "create" => {
                if let Some(url) = v["url"].as_str().filter(|s| !s.is_empty()) {
                    create.insert("url".into(), json!(url));
                }
                if v["use_proxy"].as_bool().unwrap_or(false) {
                    create.insert("use_proxy".into(), json!(true));
                }
            }
            "navigate" => steps.push(step("navigate", json!({ "url": v["url"] }))),
            "set_content" => steps.push(step("set_content", json!({ "html": v["html"] }))),
            "click" => steps.push(step("click", json!({ "index": v["index"] }))),
            "click_xy" => steps.push(step("click_xy", json!({ "x": v["x"], "y": v["y"] }))),
            "drag" => steps.push(step(
                "drag",
                json!({
                    "from": { "x": v["from_x"], "y": v["from_y"] },
                    "to": { "x": v["to_x"], "y": v["to_y"] },
                    "steps": v["steps"],
                }),
            )),
            "input" => steps.push(step("input", json!({ "index": v["index"], "text": v["text"] }))),
            "scroll" => steps.push(step(
                "scroll",
                json!({ "direction": v["direction"], "amount": v["amount"] }),
            )),
            "eval" => steps.push(step("eval", json!({ "script": v["script"] }))),
            "viewport" => steps.push(step(
                "viewport",
                json!({
                    "width": v["width"],
                    "height": v["height"],
                    "mobile": v["mobile"].as_bool().unwrap_or(false),
                }),
            )),
            _ => {}
        }
    }
    let mut doc = Map::new();
    if !create.is_empty() {
        doc.insert("create".into(), Value::Object(create));
    }
    doc.insert("steps".into(), Value::Array(steps));
    Value::Object(doc)
}

// ---------------------------------------------------------------------------
// {{var}} substitution
// ---------------------------------------------------------------------------

/// Replace `{{name}}` placeholders in string values, recursively through
/// objects and arrays. A placeholder may be the whole string or sit inside
/// surrounding text. Unknown names are an error, not empty output — a flow
/// referencing a missing var must fail loudly, at the step that needs it.
pub fn substitute(v: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            let mut out = String::with_capacity(s.len());
            let mut rest: &str = s;
            loop {
                let Some(start) = rest.find("{{") else {
                    out.push_str(rest);
                    break;
                };
                out.push_str(&rest[..start]);
                let after = &rest[start + 2..];
                let Some(end) = after.find("}}") else {
                    return Err(format!("unterminated placeholder in {s:?}"));
                };
                let name = after[..end].trim();
                let Some(val) = vars.get(name) else {
                    return Err(format!("unknown var '{name}' — pass it via vars"));
                };
                match val {
                    Value::String(t) => out.push_str(t),
                    other => out.push_str(&other.to_string()),
                }
                rest = &after[end + 2..];
            }
            Ok(Value::String(out))
        }
        Value::Object(m) => {
            let mut out = Map::with_capacity(m.len());
            for (k, val) in m {
                out.insert(k.clone(), substitute(val, vars)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => a
            .iter()
            .map(|x| substitute(x, vars))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

// ---------------------------------------------------------------------------
// Workflow directory
// ---------------------------------------------------------------------------

/// Where `name`-addressed flows live. Server-side assets, sibling of the
/// binary's working directory by default.
pub fn workflow_dir() -> PathBuf {
    std::env::var_os("AGINXBROWSER_WORKFLOW_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("workflow"))
}

/// A workflow name is a single path segment of lowercase/digits/dashes —
/// anything else (dots, slashes, ..) is rejected before it ever reaches the
/// filesystem.
fn is_workflow_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Names of flows installed under the workflow dir (discovery-by-error: an
/// unknown name in runFlow lists these).
pub fn available_workflows() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(workflow_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("flow.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_workflow_name(n))
        .collect();
    names.sort();
    names
}

/// Resolve the flow document to run: inline, or by name from the workflow
/// directory.
pub fn resolve_flow_doc(flow: Option<Value>, name: Option<&str>) -> Result<Value, String> {
    if let Some(doc) = flow {
        if doc.get("steps").and_then(|s| s.as_array()).is_some() {
            return Ok(doc);
        }
        return Err("flow document has no steps array".into());
    }
    let Some(name) = name else {
        return Err(
            "pass either \"flow\" (inline document) or \"name\" (server-side workflow/<name>/flow.json)"
                .into(),
        );
    };
    if !is_workflow_name(name) {
        return Err(format!("invalid workflow name {name:?} (lowercase/digits/dashes only)"));
    }
    let path = workflow_dir().join(name).join("flow.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "workflow {name:?} not found at {} (available: {})",
            path.display(),
            available_workflows().join(", ")
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("workflow {name:?} is not valid JSON: {e}"))
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

fn str_arg(a: &Value, key: &str, op: &str) -> Result<String, String> {
    a.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("{op}: missing string arg {key:?}"))
}

fn u64_arg(a: &Value, key: &str, op: &str) -> Result<u64, String> {
    a.get(key)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("{op}: missing integer arg {key:?}"))
}

fn opt_u32(a: &Value, key: &str) -> Option<u32> {
    a.get(key).and_then(|v| v.as_u64()).map(|n| n as u32)
}

/// JS truthiness for expect results (0, "", null, false, NaN are falsy —
/// everything else truthy, arrays/objects included).
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Defaults a flow declares for its vars; caller-passed vars win.
fn effective_vars(flow: &Value, call_vars: &Map<String, Value>) -> Map<String, Value> {
    let mut vars = Map::new();
    if let Some(defaults) = flow.get("vars").and_then(|v| v.as_object()) {
        vars.extend(defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    for (k, v) in call_vars {
        vars.insert(k.clone(), v.clone());
    }
    vars
}

/// Execute one step against the session — the same SessionCommand path the
/// HTTP/MCP handlers drive, so quotas, stealth egress, and recording
/// behavior are identical whether a human, an agent, or a flow does it.
async fn exec_step(
    mgr: &mut SessionManager,
    sid: &str,
    op: &str,
    a: &Value,
) -> Result<Value, String> {
    use SessionCommand as C;
    match op {
        "navigate" => {
            let url = str_arg(a, "url", op)?;
            let r = mgr
                .send(sid, |reply| C::Navigate { url: url.clone(), reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(r).map_err(|e| e.to_string())
        }
        "set_content" => {
            let html = str_arg(a, "html", op)?;
            mgr.send(sid, |reply| C::SetContent { html: html.clone(), reply })
                .await
                .map_err(|e| e.to_string())
        }
        "click" => {
            let index = u64_arg(a, "index", op)? as usize;
            let r = mgr
                .send(sid, |reply| C::Click { index, reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(r).map_err(|e| e.to_string())
        }
        "click_xy" => {
            let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let button = a.get("button").and_then(|v| v.as_str()).unwrap_or("left").to_string();
            let click_count = a.get("click_count").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let text = mgr
                .send(sid, |reply| C::ClickXY { x, y, button, click_count, reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "drag" => {
            let f = a.get("from").cloned().unwrap_or(json!({}));
            let t = a.get("to").cloned().unwrap_or(json!({}));
            let r = mgr
                .send(sid, |reply| C::Drag {
                    from_x: f["x"].as_f64().unwrap_or(0.0),
                    from_y: f["y"].as_f64().unwrap_or(0.0),
                    to_x: t["x"].as_f64().unwrap_or(0.0),
                    to_y: t["y"].as_f64().unwrap_or(0.0),
                    steps: a.get("steps").and_then(|v| v.as_u64()).unwrap_or(10) as u32,
                    delay_ms: a.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(30),
                    reply,
                })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&r).map_err(|e| e.to_string())
        }
        "input" => {
            let index = u64_arg(a, "index", op)? as usize;
            let text = str_arg(a, "text", op)?;
            let full_events = a.get("events").and_then(|v| v.as_str()) == Some("full");
            mgr.send(sid, |reply| C::Input { index, text: text.clone(), full_events, reply })
                .await
                .map_err(|e| e.to_string())
        }
        "scroll" => {
            let direction = match a.get("direction").and_then(|v| v.as_str()) {
                Some("up") => ScrollDirection::Up,
                _ => ScrollDirection::Down,
            };
            let amount = a.get("amount").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
            let ok = mgr
                .send(sid, |reply| C::Scroll { direction, amount, reply })
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "scrolled": ok }))
        }
        "eval" => {
            let script = str_arg(a, "script", op)?;
            mgr.send(sid, |reply| C::Eval { script: script.clone(), reply })
                .await
                .map_err(|e| e.to_string())
        }
        "wait" => {
            let selector = a.get("selector").and_then(|v| v.as_str()).map(String::from);
            let predicate = a.get("predicate").and_then(|v| v.as_str()).map(String::from);
            let timeout_ms = a.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(10_000);
            if selector.is_none() && predicate.is_none() {
                return Err("wait: pass selector or predicate".into());
            }
            let text = mgr
                .send(sid, |reply| C::Wait { selector, predicate, timeout_ms, reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "viewport" => {
            let mobile = a.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false);
            mgr.send(sid, |reply| C::Viewport {
                width: opt_u32(a, "width"),
                height: opt_u32(a, "height"),
                mobile,
                reply,
            })
            .await
            .map_err(|e| e.to_string())
        }
        "screenshot" => {
            let text = mgr
                .send(sid, |reply| C::Screenshot {
                    width: opt_u32(a, "width"),
                    height: opt_u32(a, "height"),
                    full_page: a.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false),
                    selector: a.get("selector").and_then(|v| v.as_str()).map(String::from),
                    selector_all: a.get("selector_all").and_then(|v| v.as_bool()).unwrap_or(false),
                    reply,
                })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "state" => {
            let text = mgr
                .send(sid, |reply| C::State { reply })
                .await
                .map_err(|e| e.to_string())?;
            Ok(Value::String(text))
        }
        "cookies" => {
            let text = mgr
                .send(sid, |reply| C::Cookies { reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "close" => {
            mgr.close(sid);
            Ok(json!({ "closed": true }))
        }
        other => Err(format!(
            "unknown op {other:?} (navigate set_content click click_xy drag input scroll eval wait viewport screenshot state cookies close)"
        )),
    }
}

/// One expect check = one eval driving the page's event loop. All declared
/// checks must hold (assert semantics: first failure aborts the flow —
/// Selenium IDE's soft "verify" is deliberately not offered, a replay must
/// not limp on).
async fn run_expect(
    mgr: &mut SessionManager,
    sid: &str,
    check: &str,
    spec: &Value,
) -> Result<(), String> {
    let literal = |s: &Value| -> Result<String, String> {
        s.as_str()
            .and_then(|v| serde_json::to_string(v).ok())
            .ok_or_else(|| format!("{check} expects a string"))
    };
    let script = match check {
        "url_contains" => format!("location.href.includes({})", literal(spec)?),
        "selector" => format!("!!document.querySelector({})", literal(spec)?),
        "text_contains" => format!(
            "((document.body && document.body.innerText) || '').includes({})",
            literal(spec)?
        ),
        "eval_truthy" => literal(spec)?,
        other => {
            return Err(format!(
                "unknown check {other:?} (url_contains | selector | text_contains | eval_truthy)"
            ))
        }
    };
    let v = mgr
        .send(sid, |reply| SessionCommand::Eval { script, reply })
        .await
        .map_err(|e| e.to_string())?;
    if js_truthy(&v) {
        Ok(())
    } else {
        Err(format!("returned {v}"))
    }
}

/// The failure receipt: enough evidence for the calling agent to repair the
/// flow or take the session over — failing step, reason, current URL, a
/// best-effort viewport screenshot, and everything saved so far. The session
/// is left alive on purpose (manual takeover beats forced cleanup).
async fn fail_receipt(
    mgr: &mut SessionManager,
    sid: &str,
    step_index: usize,
    step: &Value,
    reason: String,
    mut saved: Map<String, Value>,
    last_url: &Value,
) -> Value {
    let url = if last_url.is_null() {
        mgr.send(sid, |reply| SessionCommand::Eval {
            script: "location.href".into(),
            reply,
        })
        .await
        .ok()
        .unwrap_or_else(|| json!("about:blank"))
    } else {
        last_url.clone()
    };
    let screenshot = mgr
        .send(sid, |reply| SessionCommand::Screenshot {
            width: None,
            height: None,
            full_page: false,
            selector: None,
            selector_all: false,
            reply,
        })
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    json!({
        "status": "failed",
        "session_id": sid,
        "failed_step": step_index,
        "step": step,
        "reason": reason,
        "url": url,
        "screenshot": screenshot,
        "steps_done": step_index,
        "saved": Value::Object(std::mem::take(&mut saved)),
        "hint": "fix the flow and re-run, or take the session over manually (session_state / session_click / ...) — it stays alive",
    })
}

/// Run a flow to completion (or first failure). Returns the receipt —
/// `status: ok` with `saved` outputs, or `status: failed` with the evidence
/// bundle from [`fail_receipt`]. Either way the session stays alive and its
/// id rides on the receipt.
pub async fn run_flow(
    mgr: &mut SessionManager,
    flow: &Value,
    call_vars: &Map<String, Value>,
    session_id: Option<String>,
) -> Value {
    let vars = effective_vars(flow, call_vars);
    let steps = flow
        .get("steps")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    // Session: reuse the given one — that's how login state (import_curl, a
    // persistent session, a prior run) and flows compose — or create a fresh
    // one from the flow's create block. The create block goes through the
    // same {{var}} substitution as step args (its url is the most common
    // placeholder host); failing it fails before any session exists, so the
    // receipt carries no session to take over. The create block MAY carry
    // cookies for private hand-authored flows; the recorder never writes
    // them there.
    let sid = match session_id {
        Some(id) => id,
        None => {
            let raw_c = flow.get("create").cloned().unwrap_or(json!({}));
            let c = match substitute(&raw_c, &vars) {
                Ok(c) => c,
                Err(e) => {
                    return json!({
                        "status": "failed",
                        "session_id": Value::Null,
                        "failed_step": 0,
                        "reason": format!("create block var substitution: {e}"),
                        "steps_done": 0,
                        "saved": {},
                        "hint": "pass the missing var via vars",
                    })
                }
            };
            let cookies = c
                .get("cookies")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let pin = match (opt_u32(&c, "width"), opt_u32(&c, "height")) {
                (Some(w), Some(h)) => Some((
                    Some(w),
                    Some(h),
                    c.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false),
                )),
                _ => None,
            };
            mgr.create(
                c.get("url").and_then(|v| v.as_str()),
                c.get("use_proxy").and_then(|v| v.as_bool()).unwrap_or(false),
                cookies,
                c.get("storage").filter(|v| v.is_object()).cloned(),
                None,
                pin,
                false,
                false,
            )
        }
    };

    let mut saved = Map::new();
    let mut last_url = Value::Null;

    for (i, raw) in steps.iter().enumerate() {
        let op = raw.get("op").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let args = match substitute(raw.get("args").unwrap_or(&json!({})), &vars) {
            Ok(a) => a,
            Err(e) => {
                return fail_receipt(
                    mgr,
                    &sid,
                    i,
                    raw,
                    format!("var substitution: {e}"),
                    saved,
                    &last_url,
                )
                .await
            }
        };
        let result = match exec_step(mgr, &sid, &op, &args).await {
            Ok(v) => v,
            Err(e) => return fail_receipt(mgr, &sid, i, raw, e, saved, &last_url).await,
        };
        if let Some(u) = result.get("url").and_then(|v| v.as_str()) {
            if !u.is_empty() {
                last_url = json!(u);
            }
        }
        if let Some(expect) = raw.get("expect") {
            for (check, spec) in expect.as_object().into_iter().flatten() {
                if let Err(e) = run_expect(mgr, &sid, check, spec).await {
                    return fail_receipt(
                        mgr,
                        &sid,
                        i,
                        raw,
                        format!("expect {check}: {e}"),
                        saved,
                        &last_url,
                    )
                    .await;
                }
            }
        }
        if let Some(name) = raw.get("save").and_then(|v| v.as_str()) {
            saved.insert(name.to_string(), result);
        }
    }

    json!({
        "status": "ok",
        "session_id": sid,
        "steps_done": steps.len(),
        "url": last_url,
        "saved": saved,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
            substitute(&json!({ "url": "https://s/?q={{q}}", "meta": ["{{n}}"] }), &vars).unwrap(),
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
        let sid = mgr.create(Some("about:blank"), false, vec![], None, None, None, false, false);
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
        assert!(reason.contains("create block") && reason.contains("missing"), "reason: {reason}");
        assert!(r2["session_id"].is_null());
        assert_eq!(mgr.list().len(), before);
    }
}
