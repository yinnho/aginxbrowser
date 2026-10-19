//! Agent-friendly page snapshot: structured JSON (and compact text) with
//! actionable `ref=eN` ids that map onto the same interactive index
//! `/session/{id}/click` and `/input` already use. HTTP-first Obscura /
//! Playwright-a11y style surface — no MCP. Built on
//! [`super::interact::collect_interactive_raw`].
use serde_json::{json, Value};

use super::interact::collect_interactive_raw;
use crate::page::Page;
use std::collections::HashMap;

/// Build the JSON snapshot body and (re)bind the session's element_map so
/// subsequent click/input by `ref` or `index` hit the same nodes.
pub(super) fn extract_snapshot(
    page: &mut Page,
    element_map: &mut HashMap<usize, u64>,
) -> Result<Value, String> {
    let parsed = collect_interactive_raw(page, element_map)?;
    Ok(format_snapshot_json(&parsed))
}

/// Compact a11y-ish text (one line per element) for token-cheap agents.
pub(crate) fn format_snapshot_text(snap: &Value) -> String {
    let url = snap.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = snap.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    out.push_str(&format!("url={}\n", url));
    out.push_str(&format!("title={}\n", title));
    if let Some(vp) = snap.get("viewport") {
        out.push_str(&format!(
            "viewport={}x{}\n\n",
            vp.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
            vp.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
        ));
    } else {
        out.push('\n');
    }
    if let Some(elements) = snap.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            let role = el.get("role").and_then(|v| v.as_str()).unwrap_or("generic");
            let name = el.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let rf = el.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            let mut line = format!("- {} \"{}\" [ref={}]", role, escape_name(name), rf);
            if el.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
                line.push_str(" disabled");
            }
            if let Some(v) = el.get("value").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    line.push_str(&format!(" value=\"{}\"", escape_name(v)));
                }
            }
            if let Some(ph) = el.get("placeholder").and_then(|v| v.as_str()) {
                if !ph.is_empty() {
                    line.push_str(&format!(" placeholder=\"{}\"", escape_name(ph)));
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn escape_name(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn format_snapshot_json(parsed: &Value) -> Value {
    let url = parsed.get("url").cloned().unwrap_or(json!(""));
    let title = parsed.get("title").cloned().unwrap_or(json!(""));
    let viewport = parsed
        .get("viewport")
        .cloned()
        .unwrap_or(json!({"w": 0, "h": 0}));

    let mut elements = Vec::new();
    if let Some(arr) = parsed.get("elements").and_then(|v| v.as_array()) {
        for el in arr {
            let idx = el.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let attrs = el.get("attrs").and_then(|v| v.as_object());
            let mut obj = json!({
                "ref": format!("e{idx}"),
                "index": idx,
                "role": el.get("role").and_then(|v| v.as_str()).unwrap_or("generic"),
                "name": el.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "tag": el.get("tag").and_then(|v| v.as_str()).unwrap_or(""),
                "bbox": {
                    "x": el.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                    "y": el.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                    "w": el.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                    "h": el.get("h").and_then(|v| v.as_i64()).unwrap_or(0),
                },
            });
            if el.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
                obj["disabled"] = json!(true);
            }
            if let Some(v) = el.get("value") {
                if !v.as_str().unwrap_or("").is_empty() {
                    obj["value"] = v.clone();
                }
            }
            if let Some(attrs) = attrs {
                for key in ["href", "type", "placeholder", "name", "id"] {
                    if let Some(v) = attrs.get(key) {
                        if let Some(s) = v.as_str() {
                            if !s.is_empty() {
                                obj[key] = json!(s);
                            }
                        }
                    }
                }
                if attrs.contains_key("checked") {
                    obj["checked"] = json!(true);
                }
                if let Some(sel) = attrs.get("selected").and_then(|v| v.as_str()) {
                    obj["selected"] = json!(sel);
                }
            }
            elements.push(obj);
        }
    }

    json!({
        "url": url,
        "title": title,
        "viewport": viewport,
        "elements": elements,
    })
}

/// Parse an actionable ref (`e12` / `E12`) into the interactive index.
/// Bare decimal digits are also accepted for convenience.
pub(crate) fn parse_element_ref(raw: &str) -> Result<usize, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty ref".into());
    }
    let digits = if let Some(rest) = s.strip_prefix('e').or_else(|| s.strip_prefix('E')) {
        rest
    } else {
        s
    };
    if !digits.chars().all(|c| c.is_ascii_digit()) || digits.is_empty() {
        return Err(format!(
            "invalid ref {raw:?} — expected eN (from /snapshot) or a decimal index"
        ));
    }
    digits
        .parse::<usize>()
        .map_err(|_| format!("invalid ref {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_accepts_en_and_digits() {
        assert_eq!(parse_element_ref("e0").unwrap(), 0);
        assert_eq!(parse_element_ref("E12").unwrap(), 12);
        assert_eq!(parse_element_ref("3").unwrap(), 3);
        assert!(parse_element_ref("").is_err());
        assert!(parse_element_ref("ex").is_err());
        assert!(parse_element_ref("ref=e1").is_err());
    }

    #[test]
    fn format_snapshot_json_assigns_refs() {
        let raw = json!({
            "url": "https://example.com/login",
            "title": "Login",
            "viewport": {"w": 1280, "h": 800},
            "elements": [{
                "index": 0,
                "tag": "a",
                "text": "Home",
                "role": "link",
                "name": "Home",
                "x": 24, "y": 16, "w": 52, "h": 19,
                "attrs": {"href": "/home"}
            }, {
                "index": 1,
                "tag": "input",
                "text": "",
                "role": "textbox",
                "name": "Email",
                "x": 24, "y": 60, "w": 232, "h": 22,
                "value": "",
                "attrs": {"type": "email", "placeholder": "Email"}
            }, {
                "index": 2,
                "tag": "button",
                "text": "Sign In",
                "role": "button",
                "name": "Sign In",
                "x": 24, "y": 140, "w": 88, "h": 28,
                "disabled": true,
                "attrs": {"disabled": "disabled"}
            }]
        });
        let snap = format_snapshot_json(&raw);
        assert_eq!(snap["elements"][0]["ref"], "e0");
        assert_eq!(snap["elements"][0]["role"], "link");
        assert_eq!(snap["elements"][0]["href"], "/home");
        assert_eq!(snap["elements"][1]["ref"], "e1");
        assert_eq!(snap["elements"][1]["placeholder"], "Email");
        assert_eq!(snap["elements"][2]["disabled"], true);

        let text = format_snapshot_text(&snap);
        assert!(text.contains("- link \"Home\" [ref=e0]"), "{text}");
        assert!(text.contains("- button \"Sign In\" [ref=e2] disabled"), "{text}");
    }
}
