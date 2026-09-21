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
use std::collections::HashMap;
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
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
            "input" => steps.push(step(
                "input",
                json!({ "index": v["index"], "text": v["text"] }),
            )),
            // Names-only skeleton: the recorded log never carries file bytes,
            // so the export marks where an upload belongs and the author fills
            // in content (or a {{var}}) during curation — replay fails loudly
            // on a spec without content_base64 rather than uploading 0 bytes.
            "set_files" => steps.push(step(
                "set_files",
                json!({
                    "selector": v["selector"],
                    "files": v["names"].as_array().map(|ns| {
                        ns.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>()
                    }).unwrap_or_default(),
                }),
            )),
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

/// Dotted-path lookup into the vars map: `{{resp.json.access_token}}` walks
/// object keys (and array indices) so a step can read one field off an
/// earlier step's saved response. Any miss is `None` — the caller reports it
/// as an unknown var, keeping the fail-loudly contract.
fn lookup_path<'a>(vars: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let root = segments.next()?;
    let mut v = vars.get(root)?;
    for seg in segments {
        v = match v {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

/// Replace `{{name}}` placeholders in string values, recursively through
/// objects and arrays. A placeholder may be the whole string or sit inside
/// surrounding text, and the name may be a dotted path into saved results
/// (see [`lookup_path`]). Unknown names are an error, not empty output — a
/// flow referencing a missing var must fail loudly, at the step that needs it.
///
/// A placeholder that IS the whole string keeps the referenced value's type:
/// `"value": "{{max_tries}}"` splices the number 3, not the string "3" —
/// branch conditions compare values, and a text-forced "3" makes `at_most`
/// error while `equals` silently reads false. Placeholders with text around
/// them stay text semantics (same rule as [`json_interpolate`]).
pub fn substitute(v: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            if let Some(path) = s.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
                let path = path.trim();
                if !path.is_empty() && !path.contains("{{") && !path.contains("}}") {
                    return match lookup_path(vars, path) {
                        Some(val) => Ok(val.clone()),
                        None => Err(format!("unknown var '{path}' — pass it via vars")),
                    };
                }
            }
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
                let Some(val) = lookup_path(vars, name) else {
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

/// Whole-leaf interpolation for the http step's `json` arg: a string that is
/// EXACTLY `{{path}}` is replaced by the referenced value — objects and
/// arrays embed structurally, strings re-serialize with proper JSON escaping
/// (an HTML article body full of quotes splices in as a legal string
/// literal, where text substitution would corrupt the document). Partial
/// placeholders inside longer strings are deliberately NOT expanded here —
/// compose those in `body`, where plain text semantics are what you want.
/// A whole-leaf placeholder whose path misses is an error, keeping the
/// fail-loudly contract (sending the literal `{{...}}` text instead would
/// be a silent-corruption bug).
fn json_interpolate(v: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            if let Some(path) = s.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
                let path = path.trim();
                if let Some(val) = lookup_path(vars, path) {
                    return Ok(val.clone());
                }
                // A string like "{{a}} and {{b}}" is not a whole-leaf
                // placeholder (strip leaves "{{" debris inside the path) —
                // leave it alone; only a clean single path errors.
                if !path.is_empty() && !path.contains("{{") && !path.contains("}}") {
                    return Err(format!("unknown var '{path}' in json arg"));
                }
            }
            Ok(v.clone())
        }
        Value::Object(m) => {
            let mut out = Map::with_capacity(m.len());
            for (k, x) in m {
                out.insert(k.clone(), json_interpolate(x, vars)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => a
            .iter()
            .map(|x| json_interpolate(x, vars))
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
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
        return Err(format!(
            "invalid workflow name {name:?} (lowercase/digits/dashes only)"
        ));
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

// ---------------------------------------------------------------------------
// Branch steps (issue #72)
// ---------------------------------------------------------------------------

/// The condition operators a `when` may use.
const CONDITION_OPS: &[&str] = &[
    "equals", "not_equals", "in", "not_in", "contains", "at_least", "at_most", "truthy",
    "falsy",
];

/// JSON equality with number looseness: 200 == 200.0 (serde_json's Value
/// PartialEq is representation-sensitive; a flow author comparing an
/// integer fact against a float literal shouldn't care).
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// The branch step's condition — structured JSON, deliberately no
/// expression parser: `{"var": "v.verdict", "is": "equals", "value":
/// "landed"}`. `var` is a dotted path resolved by the same
/// [`lookup_path`] `{{placeholders}}` use, so a branch reads one field
/// off an earlier step's saved result. Unknown var and unknown `is` are
/// loud step errors — the same fail-loudly contract as substitution; a
/// condition that silently evaluated false on a typo would be a flow
/// that skips its own retry loop.
fn eval_condition(when: &Value, vars: &Map<String, Value>) -> Result<bool, String> {
    let path = when
        .get("var")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "when.var (a dotted var path) is required".to_string())?;
    let is = when
        .get("is")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("when.is is required (one of {})", CONDITION_OPS.join(" | ")))?;
    let value = when.get("value");
    let val = lookup_path(vars, path).ok_or_else(|| {
        format!("unknown var '{path}' — no step saved it and no var declared it")
    })?;
    match is {
        "equals" => Ok(loose_eq(val, value.ok_or("equals needs a value")?)),
        "not_equals" => Ok(!loose_eq(val, value.ok_or("not_equals needs a value")?)),
        "in" => match value.ok_or("`in` needs a value")? {
            Value::Array(a) => Ok(a.iter().any(|x| loose_eq(val, x))),
            _ => Err("`in` requires an array value".into()),
        },
        "not_in" => match value.ok_or("`not_in` needs a value")? {
            Value::Array(a) => Ok(!a.iter().any(|x| loose_eq(val, x))),
            _ => Err("`not_in` requires an array value".into()),
        },
        "contains" => match val {
            Value::String(hay) => {
                let needle = value
                    .and_then(Value::as_str)
                    .ok_or("contains on a string var needs a string value")?;
                Ok(hay.contains(needle))
            }
            Value::Array(a) => Ok(value.is_some_and(|v| a.iter().any(|x| loose_eq(x, v)))),
            _ => Err("contains applies to string and array vars".into()),
        },
        "at_least" | "at_most" => {
            let x = val
                .as_f64()
                .ok_or("at_least/at_most need a numeric var")?;
            let y = value
                .and_then(Value::as_f64)
                .ok_or("at_least/at_most need a numeric value")?;
            Ok(if is == "at_least" { x >= y } else { x <= y })
        }
        "truthy" => Ok(js_truthy(val)),
        "falsy" => Ok(!js_truthy(val)),
        other => Err(format!(
            "unknown when.is {other:?} (one of {})",
            CONDITION_OPS.join(" | ")
        )),
    }
}

/// Pre-flight for branch steps: collect step ids, then check every
/// branch's goto target and condition shape BEFORE any step runs — an
/// unknown goto must fail the run at the door, not halfway through with
/// a half-executed session behind it. Duplicate ids are rejected for the
/// same reason: a goto must mean exactly one step.
fn validate_control_flow(steps: &[Value]) -> Result<HashMap<String, usize>, String> {
    let mut ids: HashMap<String, usize> = HashMap::new();
    for (i, s) in steps.iter().enumerate() {
        let Some(id) = s.get("id").and_then(Value::as_str) else { continue };
        if id.is_empty() {
            return Err(format!("step {i}: id must be a non-empty string"));
        }
        if ids.insert(id.to_string(), i).is_some() {
            return Err(format!(
                "duplicate step id {id:?} (step {i}) — a goto must mean exactly one step"
            ));
        }
    }
    for (i, s) in steps.iter().enumerate() {
        if s.get("op").and_then(Value::as_str) != Some("branch") {
            continue;
        }
        let args = s.get("args").cloned().unwrap_or(json!({}));
        let goto = args
            .get("goto")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("branch step {i}: missing args.goto (a step id)"))?;
        if !ids.contains_key(goto) {
            let known: Vec<&str> = steps
                .iter()
                .filter_map(|s| s.get("id").and_then(Value::as_str))
                .collect();
            return Err(format!(
                "branch step {i}: goto {goto:?} matches no step id (ids: {})",
                known.join(", ")
            ));
        }
        let when = args
            .get("when")
            .ok_or_else(|| format!("branch step {i}: missing args.when"))?;
        // Shape-check only — the var resolves at runtime, after earlier
        // steps have saved it. This catches the typos a flow author can
        // make before any session exists.
        if when
            .get("var")
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
        {
            return Err(format!("branch step {i}: when.var (a dotted var path) is required"));
        }
        let is = when
            .get("is")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("branch step {i}: when.is is required"))?;
        if !CONDITION_OPS.contains(&is) {
            return Err(format!(
                "branch step {i}: unknown when.is {is:?} (one of {})",
                CONDITION_OPS.join(" | ")
            ));
        }
    }
    Ok(ids)
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

/// Args contract: a flow that consumes `args_json` may declare the keys it
/// accepts as a top-level `"args"` array. Unknown keys are rejected loudly
/// before any step runs — a mistyped key used to splice in as nothing and the
/// flow silently degraded (a reply posted as a root tweet). None = ok (no
/// declaration, no args_json, or every key known); Some(reason) = reject.
fn validate_flow_args(flow: &Value, vars: &Map<String, Value>) -> Option<String> {
    let allowed = flow.get("args")?.as_array()?;
    let aj = vars.get("args_json")?;
    let parsed = match aj {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(e) => return Some(format!("args_json is not valid JSON: {e}")),
        },
        other => other.clone(),
    };
    let obj = match parsed {
        Value::Object(m) => m,
        _ => return Some("args_json must be a JSON object".into()),
    };
    let allowed: Vec<&str> = allowed.iter().filter_map(|k| k.as_str()).collect();
    let unknown: Vec<&str> = obj
        .keys()
        .map(|k| k.as_str())
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    Some(format!(
        "unknown args_json key(s): {} — this flow accepts: {}",
        unknown.join(", "),
        allowed.join(", ")
    ))
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
            let humanize = a.get("humanize").and_then(|v| v.as_bool()).unwrap_or(true);
            let r = mgr
                .send(sid, |reply| C::Drag {
                    from_x: f["x"].as_f64().unwrap_or(0.0),
                    from_y: f["y"].as_f64().unwrap_or(0.0),
                    to_x: t["x"].as_f64().unwrap_or(0.0),
                    to_y: t["y"].as_f64().unwrap_or(0.0),
                    steps: a
                        .get("steps")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(if humanize { 24 } else { 10 }) as u32,
                    delay_ms: a
                        .get("delay_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(if humanize { 18 } else { 30 }),
                    humanize,
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
        "set_files" => {
            let selector = str_arg(a, "selector", op)?;
            let files = a
                .get("files")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "set_files: pass files as a non-empty array".to_string())?;
            if files.is_empty() {
                return Err("set_files: pass files as a non-empty array".into());
            }
            for (i, f) in files.iter().enumerate() {
                let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                // Key present (a string) is the requirement — an empty
                // content_base64 is a legitimate deliberate 0-byte clear, the
                // same carve-out session_set_files documents. Key absent is
                // the names-only skeleton a recorded export emits; replaying
                // it would silently upload 0-byte files, so fail loudly and
                // point at {{var}} as the fix.
                match f.get("content_base64") {
                    Some(Value::String(_)) => {}
                    _ => {
                        return Err(format!(
                            "set_files: file {i} ({name:?}) has no content_base64 — supply it, e.g. via a {{{{var}}}} (the recorded export carries names only)"
                        ))
                    }
                }
            }
            mgr.send(sid, |reply| C::SetFiles {
                selector: selector.clone(),
                files: files.clone(),
                reply,
            })
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
            // Flow eval steps ride the default await budget; a flow needing a
            // longer one passes `timeout_ms` (same clamp as the HTTP face).
            let timeout_ms = a.get("timeout_ms").and_then(|v| v.as_u64());
            mgr.send(sid, |reply| C::Eval { script: script.clone(), timeout_ms, reply })
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
        // The decision-layer fact sheet (issue #73) as a step: pair it
        // with `save` and the next branch can test {{v.verdict}} /
        // {{v.facts.doc_status}} — the verdict-then-branch shape every
        // wall-aware flow wants. Read-only, eval-free.
        "verdict" => {
            let text = mgr
                .send(sid, |reply| C::Verdict { reply })
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "close" => {
            mgr.close(sid);
            Ok(json!({ "closed": true }))
        }
        other => Err(format!(
            "unknown op {other:?} (navigate set_content click click_xy drag input set_files scroll eval wait viewport screenshot state cookies verdict close http — plus executor-level branch, see run_flow)"
        )),
    }
}

/// Engine-side HTTP request step — the CORS-free escape hatch for
/// API-driven flows (WeChat OA publish, any server-to-server endpoint). The
/// request never runs in a page, so credentials never touch page context or
/// a session network log. It rides the same posture as `/fetch`: the
/// per-domain quota gate, the shared SSRF deny-set, env proxy, and a fresh
/// cookie-less client per step — no ambient session state leaks in, none
/// accumulates across steps. Redirects are NOT followed (Policy::none, like
/// every engine client): a 3xx surfaces as its status for the flow author
/// to handle. The step takes RAW (pre-substitution) args: everything except
/// `json` goes through text substitution, while `json` gets whole-leaf
/// value interpolation (see [`json_interpolate`]).
async fn exec_http_step(a: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    use base64::Engine as _;
    use diting::diting_net::client::validate_url;
    use diting::diting_net::{self, CookieJar, HttpClient};
    use std::str::FromStr;

    let mut raw = a.clone();
    let json_arg = raw
        .as_object_mut()
        .and_then(|m| m.remove("json"));
    let text = substitute(&raw, vars).map_err(|e| format!("http: {e}"))?;

    let url_str = str_arg(&text, "url", "http")?;
    let method_str = text
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let method = reqwest::Method::from_str(&method_str)
        .map_err(|_| format!("http: unsupported method {method_str:?}"))?;
    crate::rate::check_domain(&url_str).map_err(|e| format!("http: {e}"))?;
    let url: url::Url = url_str
        .parse()
        .map_err(|e| format!("http: bad url {url_str:?}: {e}"))?;
    validate_url(&url, diting_net::env_allows_private_network())
        .map_err(|e| format!("http: {e}"))?;

    let client = HttpClient::with_full_options(
        std::sync::Arc::new(CookieJar::new()),
        crate::config::proxy_from_env().as_deref(),
        false,
    );
    let rc = client.request_client(&url_str).await;
    let mut req = rc.request(method, url);
    if let Some(ms) = a.get("timeout_ms").and_then(|v| v.as_u64()) {
        req = req.timeout(std::time::Duration::from_millis(ms.clamp(1_000, 120_000)));
    }

    let mut content_type: Option<String> = None;
    if let Some(jv) = json_arg {
        let body_val = json_interpolate(&jv, vars).map_err(|e| format!("http: {e}"))?;
        let body_text =
            serde_json::to_string(&body_val).map_err(|e| format!("http: json body: {e}"))?;
        content_type = Some("application/json".into());
        req = req.body(body_text);
    } else if let Some(parts) = text.get("multipart").and_then(|v| v.as_array()) {
        if parts.is_empty() {
            return Err("http: multipart must be a non-empty array".into());
        }
        let boundary = format!(
            "aginxbrowser-flow-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut body: Vec<u8> = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let name = p
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("http: multipart part {i} has no name"))?;
            let bytes: Vec<u8> = if let Some(b64) = p.get("content_base64").and_then(|v| v.as_str())
            {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("http: multipart part {i} ({name:?}): bad base64: {e}"))?
            } else if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                t.as_bytes().to_vec()
            } else {
                return Err(format!(
                    "http: multipart part {i} ({name:?}) has neither content_base64 nor text"
                ));
            };
            let part_ct = p
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            let mut head = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"");
            if let Some(f) = p.get("filename").and_then(|v| v.as_str()) {
                head.push_str(&format!("; filename=\"{f}\""));
            }
            head.push_str(&format!("\r\nContent-Type: {part_ct}\r\n\r\n"));
            body.extend_from_slice(head.as_bytes());
            body.extend_from_slice(&bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        content_type = Some(format!("multipart/form-data; boundary={boundary}"));
        req = req.body(body);
    } else if let Some(b) = text.get("body").and_then(|v| v.as_str()) {
        req = req.body(b.to_string());
    }
    if let Some(ct) = content_type {
        req = req.header(reqwest::header::CONTENT_TYPE, ct);
    }
    if let Some(hs) = text.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in hs {
            let val = v.as_str().unwrap_or_default();
            req = req.header(k, val);
        }
    }

    let resp = req.send().await.map_err(|e| format!("http: send: {e}"))?;
    let status = resp.status().as_u16();
    // Multi-value headers fold with ", " (the same folding rule the CDP face
    // picked up for obscura#913); keys lowercase as HTTP/2 sends them.
    let mut headers = Map::new();
    for (k, v) in resp.headers() {
        let key = k.as_str().to_string();
        let val = v.to_str().unwrap_or("").to_string();
        if let Some(Value::String(prev)) = headers.get_mut(&key) {
            prev.push_str(", ");
            prev.push_str(&val);
        } else {
            headers.insert(key, json!(val));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("http: read body: {e}"))?;
    // Response cap (obscura#581 family discipline): an API step must not
    // OOM the engine on a runaway body.
    const HTTP_STEP_MAX_BODY: usize = 8 * 1024 * 1024;
    if bytes.len() > HTTP_STEP_MAX_BODY {
        return Err(format!(
            "http: response body {} bytes exceeds the 8MB step cap",
            bytes.len()
        ));
    }
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_str::<Value>(&body).ok();
    Ok(json!({
        "status": status,
        "headers": Value::Object(headers),
        "body": body,
        "json": json,
    }))
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
        .send(sid, |reply| SessionCommand::Eval {
            script,
            timeout_ms: None,
            reply,
        })
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
#[allow(clippy::too_many_arguments)]
async fn fail_receipt(
    mgr: &mut SessionManager,
    sid: &str,
    step_index: usize,
    step: &Value,
    reason: String,
    mut saved: Map<String, Value>,
    last_url: &Value,
    executed: u64,
) -> Value {
    let url = if last_url.is_null() {
        mgr.send(sid, |reply| SessionCommand::Eval {
            script: "location.href".into(),
            timeout_ms: None,
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
        "steps_done": executed,
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
    let mut vars = effective_vars(flow, call_vars);
    if let Some(err) = validate_flow_args(flow, &vars) {
        // Same shape as a pre-session create-block failure: no session was
        // touched, so there is nothing to take over.
        return json!({
            "status": "failed",
            "session_id": Value::Null,
            "failed_step": 0,
            "reason": err,
            "steps_done": 0,
            "saved": {},
            "hint": "fix the args_json keys — see the flow's args declaration",
        });
    }
    // args_json is also exposed as an `args` OBJECT (validated above, so the
    // parse can't fail here in the declared case): steps then reference
    // {{args.title}} in text or — the http step's whole-leaf json form —
    // "{{args.content_html}}" embedding a quote-heavy body as a legal JSON
    // string. An explicit `args` var from the caller wins.
    if !vars.contains_key("args") {
        let parsed = match vars.get("args_json") {
            Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
            Some(obj @ Value::Object(_)) => Some(obj.clone()),
            _ => None,
        };
        if let Some(Value::Object(m)) = parsed {
            vars.insert("args".into(), Value::Object(m));
        }
    }
    let steps = flow
        .get("steps")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    // Control-flow preflight (issue #72): every branch's goto must name a
    // real step id and every when must be well-shaped BEFORE any session
    // exists — an unknown goto failing at step 40 leaves 39 half-executed
    // steps and a session behind it; failing at the door leaves neither.
    let ids = match validate_control_flow(&steps) {
        Ok(ids) => ids,
        Err(e) => {
            return json!({
                "status": "failed",
                "session_id": Value::Null,
                "failed_step": 0,
                "reason": e,
                "steps_done": 0,
                "saved": {},
                "hint": "fix the control flow (step ids / branch gotos / when shapes) — no step ran",
            })
        }
    };
    // Execution budget: a branch loop re-runs steps, so steps.len() no longer
    // bounds the work. Default 1000 — a linear flow never notices, a runaway
    // loop fails with a receipt instead of spinning forever.
    let max_steps = flow
        .get("max_steps")
        .and_then(|v| v.as_u64())
        .unwrap_or(1000)
        .clamp(1, 100_000);

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
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
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
                c.get("use_proxy")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                cookies,
                c.get("storage").filter(|v| v.is_object()).cloned(),
                None,
                pin,
                false,
                false,
                None,
            )
        }
    };

    let mut saved = Map::new();
    let mut last_url = Value::Null;

    // The pc walk: a linear flow still runs 0..len in order; a branch step
    // moves the pc (forward or back). `executed` counts actual step
    // executions — a loop body re-runs, so this is not "steps visited" — and
    // enforces the max_steps budget.
    let mut pc: usize = 0;
    let mut executed: u64 = 0;
    while pc < steps.len() {
        if executed >= max_steps {
            let raw = &steps[pc];
            return fail_receipt(
                mgr,
                &sid,
                pc,
                raw,
                format!(
                    "step budget exhausted: {executed} executions reached max_steps {max_steps} — a branch loop that never converges?"
                ),
                saved,
                &last_url,
                executed,
            )
            .await;
        }
        executed += 1;
        // Reserved var: the execution counter, maintained by the EXECUTOR —
        // a loop budget that survives page-context resets (a wall-page
        // navigation can wipe window.* state, but the executor keeps
        // counting). Branch on `steps_done` for context-free retry limits.
        vars.insert("steps_done".into(), json!(executed));
        let raw = &steps[pc];
        let op = raw
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // branch is executor-level — it redirects the pc without touching
        // the session. Args substitute first ({{var}} inside `when.value` is
        // legitimate); a taken branch jumps to the goto id, a false one falls
        // through. No result, no expect, no save: the branch observes, the
        // steps it guards do the asserting (the candidate never rewrites the
        // reward — JevHarness discipline, applied to control flow).
        if op == "branch" {
            let args = match substitute(raw.get("args").unwrap_or(&json!({})), &vars) {
                Ok(a) => a,
                Err(e) => {
                    return fail_receipt(
                        mgr,
                        &sid,
                        pc,
                        raw,
                        format!("var substitution: {e}"),
                        saved,
                        &last_url,
                        executed,
                    )
                    .await
                }
            };
            let goto = args
                .get("goto")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_default();
            match eval_condition(args.get("when").unwrap_or(&Value::Null), &vars) {
                Ok(true) => {
                    let Some(&target) = ids.get(&goto) else {
                        // Unreachable via preflight (goto must be a literal
                        // id, and every literal was checked) — kept as a
                        // loud failure rather than a panicking executor.
                        return fail_receipt(
                            mgr,
                            &sid,
                            pc,
                            raw,
                            format!("branch: goto {goto:?} matches no step id"),
                            saved,
                            &last_url,
                            executed,
                        )
                        .await;
                    };
                    pc = target;
                    continue;
                }
                Ok(false) => {
                    pc += 1;
                    continue;
                }
                Err(e) => {
                    return fail_receipt(
                        mgr,
                        &sid,
                        pc,
                        raw,
                        format!("branch: {e}"),
                        saved,
                        &last_url,
                        executed,
                    )
                    .await
                }
            }
        }
        // http is engine-side (no page, no session command) and takes RAW
        // args — its `json` body must bypass text substitution so whole-leaf
        // values embed structurally. Every page-bound op substitutes first.
        let result = if op == "http" {
            exec_http_step(raw.get("args").unwrap_or(&json!({})), &vars).await
        } else {
            let args = match substitute(raw.get("args").unwrap_or(&json!({})), &vars) {
                Ok(a) => a,
                Err(e) => {
                    return fail_receipt(
                        mgr,
                        &sid,
                        pc,
                        raw,
                        format!("var substitution: {e}"),
                        saved,
                        &last_url,
                        executed,
                    )
                    .await
                }
            };
            exec_step(mgr, &sid, &op, &args).await
        };
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                return fail_receipt(mgr, &sid, pc, raw, e, saved, &last_url, executed).await
            }
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
                        pc,
                        raw,
                        format!("expect {check}: {e}"),
                        saved,
                        &last_url,
                        executed,
                    )
                    .await;
                }
            }
        }
        if let Some(name) = raw.get("save").and_then(|v| v.as_str()) {
            // Saved results are also substitutable — the API-chain shape
            // (token -> upload -> draft -> publish) is exactly "step N reads
            // one field off step N-1's response" via {{name.json.field}}.
            // A save may shadow a declared var of the same name: the flow
            // author owns both namespaces and the later write wins.
            vars.insert(name.to_string(), result.clone());
            saved.insert(name.to_string(), result);
        }
        pc += 1;
    }

    json!({
        "status": "ok",
        "session_id": sid,
        "steps_done": executed,
        "url": last_url,
        "saved": saved,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
