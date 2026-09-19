//! Recording and replay: [`RecordedAction`] is the action log a session
//! appends to, and [`replay_bash`] turns that log into a runnable curl
//! script. Split from the session module root (ARCHITECTURE.md P2);
//! behavior unchanged.
use serde::Serialize;
use serde_json::Value;

/// One recorded session action — the replay log. Only actions that change
/// the page are recorded; reads (State, Cookies) would just add noise to a
/// replay script.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecordedAction {
    Create {
        url: Option<String>,
        use_proxy: bool,
        cookies: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        storage: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    Navigate {
        url: String,
        ok: bool,
    },
    SetContent {
        html: String,
        ok: bool,
    },
    Click {
        index: usize,
        ok: bool,
    },
    #[serde(rename = "click_xy")]
    ClickXY {
        x: f64,
        y: f64,
        ok: bool,
    },
    Drag {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        steps: u32,
    },
    Input {
        index: usize,
        text: String,
        ok: bool,
    },
    SetFiles {
        selector: String,
        /// File names only — content base64 never enters the action log:
        /// recordings and exported flows would balloon, and blob content has
        /// no business sitting in a replay script.
        names: Vec<String>,
        ok: bool,
    },
    Scroll {
        direction: String,
        amount: u32,
    },
    Eval {
        script: String,
    },
    Viewport {
        width: Option<u32>,
        height: Option<u32>,
        mobile: bool,
    },
}

/// Embed a string in single quotes for bash: `'` → `'\''` (the standard
/// close-quote-escaped-quote-reopen idiom). Everything else is literal.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render a recorded action log (JSONL, as returned by the Export command)
/// as a runnable bash script that replays the session against the HTTP API
/// with plain curl — replay with zero model tokens and nothing to install.
///
/// Index-based actions replay against indexes from the ORIGINAL run's
/// `/state` output; if the page's element order changed, indexes may point
/// elsewhere. That's inherent to index-based replay — the script is a
/// starting point, auditable and editable.
/// Build the evaluate that writes a `{"local_storage":{..},"session_storage":{..}}`
/// map into the page's Web Storage. The JSON payloads are embedded as JS
/// string literals via serde (JSON escape rules are JS-compatible), so keys
/// or values containing quotes/newlines/CJK survive verbatim. Returns None
/// for a storage object with nothing in it.
pub(super) fn inject_storage_js(storage: &Value) -> Option<String> {
    let ls = storage.get("local_storage").filter(|v| v.is_object())?;
    let ss = storage
        .get("session_storage")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let ls_lit = serde_json::to_string(&serde_json::to_string(ls).ok()?).ok()?;
    let ss_lit = serde_json::to_string(&serde_json::to_string(&ss).ok()?).ok()?;
    Some(format!(
        "(function(){{ var n=0; var ls=JSON.parse({ls_lit}); \
         for (var k in ls) {{ try {{ localStorage.setItem(k, String(ls[k])); n++; }} catch(e) {{}} }} \
         var ss=JSON.parse({ss_lit}); \
         for (var k in ss) {{ try {{ sessionStorage.setItem(k, String(ss[k])); n++; }} catch(e) {{}} }} \
         return n; }})()"
    ))
}

pub fn replay_bash(jsonl: &str, default_base: &str) -> String {
    let mut out = String::new();
    out.push_str("#!/usr/bin/env bash\n");
    out.push_str("# aginxbrowser session replay — recorded actions re-run as plain curl.\n");
    out.push_str("# No LLM in the loop: replay costs zero model tokens.\n");
    out.push_str("# Treat this file like credentials — it contains any cookies injected\n");
    out.push_str("# at session create.\n");
    out.push_str("set -eu\n");
    out.push_str(&format!("BASE=\"${{AGINXBROWSER_URL:-{default_base}}}\"\n"));
    out.push_str("POST() { curl -sS -X POST \"$BASE/$1\" -H 'Content-Type: application/json' -d \"$2\"; }\n\n");

    let mut sid_bound = false;
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Payload = JSON body, shell-single-quoted for the POST helper's -d "$2".
        let payload = |body: Value| shell_quote(&body.to_string());
        match v["action"].as_str().unwrap_or_default() {
            "create" => {
                let mut body = serde_json::json!({
                    "url": v["url"].clone(),
                    "cookies": v["cookies"].clone(),
                    "use_proxy": v["use_proxy"].as_bool().unwrap_or(false),
                });
                if v.get("storage").map(|s| s.is_object()).unwrap_or(false) {
                    body["storage"] = v["storage"].clone();
                }
                if v.get("account")
                    .and_then(|a| a.as_str())
                    .is_some_and(|a| !a.is_empty())
                {
                    body["account"] = v["account"].clone();
                }
                let body = payload(body);
                out.push_str(&format!(
                    "SID=$(POST session/create {body} | sed -n 's/.*\"session_id\":\"\\([^\"]*\\)\".*/\\1/p')\n"
                ));
                out.push_str(
                    "[ -n \"$SID\" ] || { echo \"session create failed\" >&2; exit 1; }\n",
                );
                sid_bound = true;
            }
            "navigate" if sid_bound => {
                let body = payload(serde_json::json!({"url": v["url"].clone()}));
                out.push_str(&format!(
                    "POST \"session/$SID/navigate\" {body} > /dev/null\n"
                ));
            }
            "click" if sid_bound => {
                let body = payload(serde_json::json!({"index": v["index"].clone()}));
                out.push_str(&format!("POST \"session/$SID/click\" {body} > /dev/null\n"));
            }
            "click_xy" if sid_bound => {
                let body = payload(serde_json::json!({"x": v["x"].clone(), "y": v["y"].clone()}));
                out.push_str(&format!(
                    "POST \"session/$SID/click_xy\" {body} > /dev/null\n"
                ));
            }
            "drag" if sid_bound => {
                let body = payload(serde_json::json!({
                    "from": {"x": v["from_x"].clone(), "y": v["from_y"].clone()},
                    "to": {"x": v["to_x"].clone(), "y": v["to_y"].clone()},
                    "steps": v["steps"].clone(),
                }));
                out.push_str(&format!("POST \"session/$SID/drag\" {body} > /dev/null\n"));
            }
            "input" if sid_bound => {
                let body = payload(
                    serde_json::json!({"index": v["index"].clone(), "text": v["text"].clone()}),
                );
                out.push_str(&format!("POST \"session/$SID/input\" {body} > /dev/null\n"));
            }
            "scroll" if sid_bound => {
                let body = payload(serde_json::json!({
                    "direction": v["direction"].clone(),
                    "amount": v["amount"].clone(),
                }));
                out.push_str(&format!(
                    "POST \"session/$SID/scroll\" {body} > /dev/null\n"
                ));
            }
            "viewport" if sid_bound => {
                let body = payload(serde_json::json!({
                    "width": v["width"].clone(),
                    "height": v["height"].clone(),
                    "mobile": v["mobile"].as_bool().unwrap_or(false),
                }));
                out.push_str(&format!(
                    "POST \"session/$SID/viewport\" {body} > /dev/null\n"
                ));
            }
            "eval" if sid_bound => {
                let body = payload(serde_json::json!({"script": v["script"].clone()}));
                out.push_str(&format!("POST \"session/$SID/eval\" {body} > /dev/null\n"));
            }
            _ => {}
        }
    }
    if sid_bound {
        out.push_str("\necho '--- final state ---'\n");
        out.push_str("POST \"session/$SID/state\" '{}'\n");
        out.push_str("echo\n");
    }
    out
}
