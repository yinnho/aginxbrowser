use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;
use crate::diting_dom::{DomTree, NodeData, NodeId};
use crate::diting_browser::Page;

/// Escape a client-supplied objectId for interpolation into a single-quoted JS
/// string literal (`__diting_objects['<here>']`). Backslashes must be escaped
/// before single quotes, otherwise an id ending in `\` turns the closing quote
/// into `\'` and produces a syntax error. All objectId lookup sites route
/// through this so they cannot diverge; not an injection vector (every `'`
/// stays escaped, so the worst case is an unterminated string -> clean
/// resolution failure), but a robustness fix.
fn escape_object_id(oid: &str) -> String {
    oid.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Resolve a DOM `nodeId` from CDP params. Honors `nodeId`, `backendNodeId`,
/// and `objectId` in that order. Playwright commonly passes only `objectId`
/// (returned by a prior `DOM.resolveNode`); without this fallback those
/// requests silently default to node 0 and click the wrong element.
fn resolve_node_id(page: &mut Page, params: &Value) -> Result<u64, String> {
    if let Some(nid) = params.get("nodeId").and_then(|v| v.as_u64()) {
        return Ok(nid);
    }
    if let Some(nid) = params.get("backendNodeId").and_then(|v| v.as_u64()) {
        return Ok(nid);
    }
    if let Some(oid) = params.get("objectId").and_then(|v| v.as_str()) {
        let code = format!(
            "(function() {{ var o = globalThis.__diting_objects && globalThis.__diting_objects['{}']; \
             return (o && typeof o._nid === 'number') ? o._nid : -1; }})()",
            escape_object_id(oid)
        );
        let result = page.evaluate(&code);
        let nid = result.as_f64().map(|n| n as i64).unwrap_or(-1);
        if nid < 0 {
            return Err(format!("objectId {oid} could not be resolved to a node"));
        }
        return Ok(nid as u64);
    }
    Err("nodeId, backendNodeId, or objectId required".to_string())
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" => Ok(json!({})),
        "getDocument" => {
            let page = ctx.get_session_page(session_id).ok_or("No page")?;
            let depth = params.get("depth").and_then(|v| v.as_i64()).unwrap_or(2);
            page.with_dom(|dom| {
                let node = serialize_node(dom, dom.document(), depth as u32, 0);
                json!({ "root": node })
            })
            .ok_or_else(|| "No DOM loaded".to_string())
        }
        "querySelector" => {
            let page = ctx.get_session_page(session_id).ok_or("No page")?;
            let selector =
                params.get("selector").and_then(|v| v.as_str()).ok_or("selector required")?;
            let result = page
                .with_dom(|dom| {
                    dom.query_selector(selector).ok().flatten().map(|id| id.index() as u64)
                })
                .unwrap_or(Some(0));
            Ok(json!({ "nodeId": result.unwrap_or(0) }))
        }
        "querySelectorAll" => {
            let page = ctx.get_session_page(session_id).ok_or("No page")?;
            let selector =
                params.get("selector").and_then(|v| v.as_str()).ok_or("selector required")?;
            let ids = page
                .with_dom(|dom| {
                    dom.query_selector_all(selector).ok().map(|ids| {
                        ids.iter().map(|id| id.index() as u64).collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            Ok(json!({ "nodeIds": ids.unwrap_or_default() }))
        }
        "getOuterHTML" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = resolve_node_id(page, params)?;
            let html = page
                .with_dom(|dom| dom.outer_html(NodeId::new(node_id as u32)))
                .unwrap_or_default();
            Ok(json!({ "outerHTML": html }))
        }
        "describeNode" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let depth = params.get("depth").and_then(|v| v.as_i64()).unwrap_or(0);

            let node_id = if let Some(nid) = params
                .get("nodeId")
                .and_then(|v| v.as_u64())
                .or_else(|| params.get("backendNodeId").and_then(|v| v.as_u64()))
            {
                nid
            } else if let Some(oid) = params.get("objectId").and_then(|v| v.as_str()) {
                let code = format!(
                    "(function() {{ var o = globalThis.__diting_objects['{}']; \
                     if (!o) return -1; return (typeof o._nid === 'number') ? o._nid : -1; }})()",
                    escape_object_id(oid)
                );
                let result = page.evaluate(&code);
                result.as_f64().map(|n| n as u64).unwrap_or(0)
            } else {
                return Err("nodeId or objectId required".to_string());
            };

            let node = page
                .with_dom(|dom| serialize_node(dom, NodeId::new(node_id as u32), depth as u32, 0))
                .unwrap_or(json!(null));
            Ok(json!({ "node": node }))
        }
        "resolveNode" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = if let Some(nid) = params
                .get("nodeId")
                .and_then(|v| v.as_u64())
                .or_else(|| params.get("backendNodeId").and_then(|v| v.as_u64()))
            {
                nid
            } else if let Some(oid) = params.get("objectId").and_then(|v| v.as_str()) {
                let code = format!(
                    "(function() {{ var o = globalThis.__diting_objects['{}']; \
                     return (o && typeof o._nid === 'number') ? o._nid : -1; }})()",
                    escape_object_id(oid)
                );
                let result = page.evaluate(&code);
                result.as_f64().map(|n| n as u64).unwrap_or(0)
            } else {
                return Err("nodeId or objectId required".to_string());
            };

            // _wrap resolves the canonical JS wrapper (cached per nid), which
            // is what Runtime.callFunctionOn and the input domains target.
            let js_code = format!(
                "(function() {{\
                    var nid = {nid};\
                    return globalThis._wrap ? globalThis._wrap(nid) : null;\
                 }})()",
                nid = node_id,
            );

            let info = match page.js.as_mut() {
                Some(js) => match js.store_object_with_meta(&js_code) {
                    Ok(info) => info,
                    Err(_) => {
                        return Ok(json!({
                            "object": {
                                "type": "object",
                                "subtype": "node",
                                "className": "HTMLElement",
                                "objectId": format!("node-{node_id}"),
                            }
                        }));
                    }
                },
                None => return Err("No JS runtime".to_string()),
            };

            Ok(json!({
                "object": {
                    "type": "object",
                    "subtype": "node",
                    "className": if info.class_name.is_empty() {
                        "HTMLElement".to_string()
                    } else {
                        info.class_name.clone()
                    },
                    "description": info.description,
                    "objectId": info.object_id.unwrap_or_else(|| format!("node-{node_id}")),
                }
            }))
        }
        "setAttributeValue" | "removeNode" => Ok(json!({})),
        "focus" => {
            // No layout engine, but diting's JS focus() sets document.activeElement,
            // which Input.dispatchKeyEvent targets. CDP clients (browser-use) focus an
            // input via DOM.focus before typing; without this their keystrokes land on
            // nothing and the field stays empty.
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = resolve_node_id(page, params)?;
            let code = format!(
                "(function() {{ var el = globalThis._wrap && globalThis._wrap({0}); \
                 if (el && typeof el.focus === 'function') {{ el.focus(); return true; }} return false; }})()",
                node_id
            );
            let _ = page.evaluate(&code);
            Ok(json!({}))
        }
        "scrollIntoViewIfNeeded" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = resolve_node_id(page, params)?;
            // No layout viewport to move, but the JS shim records this element
            // for the hit testing used by subsequent input events.
            let code = format!(
                "(function() {{ var el = globalThis._wrap && globalThis._wrap({0}); \
                 if (!el || typeof el.scrollIntoView !== 'function') return false; \
                 el.scrollIntoView(); return true; }})()",
                node_id
            );
            let did_scroll = page.evaluate(&code).as_bool().unwrap_or(false);
            if !did_scroll {
                return Err(format!(
                    "node {node_id} could not be resolved to a scrollable element"
                ));
            }
            Ok(json!({}))
        }
        "setFileInputFiles" => {
            // Puppeteer's ElementHandle.uploadFile / Playwright's setInputFiles
            // drive an <input type=file> through this CDP call (upstream #359).
            // Reads each local file, then hands its bytes (base64) to the JS
            // layer, which builds real File objects and fires input+change like
            // a real selection so page code can read/upload them.
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            // setFileInputFiles reads local files and hands their bytes to page
            // JS. Anyone who can reach the CDP port (default localhost, but a
            // hosted deploy may bind wider) could otherwise read any file the
            // process can read — the same threat as Page.navigate to file://,
            // so it honours the same opt-in and is off by default.
            if !page.context.allow_file_access {
                return Err(
                    "DOM.setFileInputFiles is disabled. Restart with --allow-file-access to enable local file uploads."
                        .to_string(),
                );
            }
            let node_id = resolve_node_id(page, params)?;
            let paths: Vec<String> = params
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
                })
                .unwrap_or_default();

            let mut specs = Vec::with_capacity(paths.len());
            for p in &paths {
                let bytes =
                    std::fs::read(p).map_err(|e| format!("setFileInputFiles: cannot read '{p}': {e}"))?;
                let name = std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();
                specs.push(json!({
                    "name": name,
                    "type": guess_mime(p),
                    "b64": encode_base64(&bytes)
                }));
            }

            let specs_json = serde_json::to_string(&specs).unwrap_or_else(|_| "[]".to_string());
            let code = format!(
                "(function() {{ var el = globalThis._wrap && globalThis._wrap({0}); \
                 if (el && globalThis.__diting_setInputFiles) {{ globalThis.__diting_setInputFiles(el, {1}); return true; }} return false; }})()",
                node_id, specs_json
            );
            let _ = page.evaluate(&code);
            Ok(json!({}))
        }
        "getBoxModel" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = match resolve_node_id(page, params) {
                Ok(nid) => nid,
                Err(_) => return Ok(json!(null)),
            };
            let code = format!(
                "(function() {{\
                    var el = globalThis._wrap && globalThis._wrap({0});\
                    if (!el || typeof el.getBoundingClientRect !== 'function') return null;\
                    var r = el.getBoundingClientRect();\
                    return [r.left, r.top, r.right, r.top, r.right, r.bottom, r.left, r.bottom,\
                            r.width, r.height];\
                }})()",
                node_id
            );
            let val = page.evaluate(&code);
            let (quad, w, h) = box_from_value(&val);
            Ok(json!({
                "model": {
                    "content": quad.clone(),
                    "padding": quad.clone(),
                    "border": quad.clone(),
                    "margin": quad,
                    "width": w,
                    "height": h,
                }
            }))
        }
        "getContentQuads" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let node_id = match resolve_node_id(page, params) {
                Ok(nid) => nid,
                Err(_) => return Ok(json!(null)),
            };
            let code = format!(
                "(function() {{\
                    var el = globalThis._wrap && globalThis._wrap({0});\
                    if (!el || typeof el.getBoundingClientRect !== 'function') return null;\
                    var r = el.getBoundingClientRect();\
                    return [r.left, r.top, r.right, r.top, r.right, r.bottom, r.left, r.bottom];\
                }})()",
                node_id
            );
            let val = page.evaluate(&code);
            let quad = quad_from_value(&val);
            Ok(json!({ "quads": [quad] }))
        }
        _ => Err(format!("Unknown DOM method: {method}")),
    }
}

const FALLBACK_QUAD: [f64; 8] = [8.0, 8.0, 108.0, 8.0, 108.0, 28.0, 8.0, 28.0];

fn nums_from_value(val: &Value) -> Option<Vec<f64>> {
    val.as_array().map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
}

// Chrome's CDP wire emits integral doubles without the ".0" (base::Value JSON
// writer), so a box at x=256 arrives as `256` — and strict clients (Hermes
// Agent browser tools) deserialize quads as i64, failing on `256.0` with
// "invalid type: floating point, expected i64" (obscura#576). Fractional
// coordinates stay fractional; the 2^53 bound keeps absurd magnitudes from
// saturating the i64 cast instead of serializing.
fn coord_json(n: &f64) -> Value {
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 {
        json!(*n as i64)
    } else {
        json!(n)
    }
}

fn quad_from_value(val: &Value) -> Vec<Value> {
    match nums_from_value(val) {
        Some(nums) if nums.len() == 8 => nums.iter().map(coord_json).collect(),
        _ => FALLBACK_QUAD.iter().map(coord_json).collect(),
    }
}

fn box_from_value(val: &Value) -> (Vec<Value>, Value, Value) {
    match nums_from_value(val) {
        Some(nums) if nums.len() >= 10 => {
            let q: Vec<Value> = nums[..8].iter().map(coord_json).collect();
            (q, coord_json(&nums[8]), coord_json(&nums[9]))
        }
        _ => (
            FALLBACK_QUAD.iter().map(coord_json).collect(),
            json!(100),
            json!(20),
        ),
    }
}

// Hard cap on how deep a getDocument/describeNode response may nest, independent
// of the requested `depth`. DOM.getDocument{depth:-1} arrives here as u32::MAX,
// which on a pathologically deep DOM (trivially scriptable, unbounded by
// html5ever) produces a Value nested that far. Even built without recursion,
// serde_json's own serialization and the Value's Drop recurse over that nesting
// and overflow the stack — on tokio's ~2 MiB worker stacks especially. Bounding
// the depth keeps the response safe to serialize and drop. Real DOMs are shallow
// (deep React trees are a few hundred), so this only truncates pathological
// nesting, which beats crashing the worker. Mirrors DOMSnapshot's MAX_NODES
// guard (upstream issue #341).
//
// Sized for the ~2 MiB stack of a tokio worker thread (where the CDP processor
// runs): each DOM level becomes two nested JSON containers
// (object -> "children" array -> object ...), so serde_json's recursive
// serialization and the Value's recursive Drop descend ~2x this depth. 256 keeps
// that a few hundred frames deep with wide margin, while still far exceeding any
// real page. Clients needing a deeper subtree re-request it with
// DOM.requestChildNodes / describeNode on a specific node.
const MAX_SERIALIZE_DEPTH: u32 = 256;

/// Build the CDP Node object for a single node (without its `children` array),
/// returning it together with that node's child ids. `None` for a missing node.
fn node_value(dom: &DomTree, node_id: NodeId) -> Option<(Value, Vec<NodeId>)> {
    let node = dom.get_node(node_id)?;
    let children_ids = dom.children(node_id);
    let child_count = children_ids.len();
    let mut result = json!({
        "nodeId": node_id.index(),
        "backendNodeId": node_id.index(),
        "childNodeCount": child_count,
    });

    match &node.data {
        NodeData::Document => {
            result["nodeType"] = json!(9);
            result["nodeName"] = json!("#document");
            result["localName"] = json!("");
            result["nodeValue"] = json!("");
            result["xmlVersion"] = json!("");
        }
        NodeData::Doctype { name, public_id, system_id } => {
            result["nodeType"] = json!(10);
            result["nodeName"] = json!(name);
            result["localName"] = json!("");
            result["nodeValue"] = json!("");
            result["publicId"] = json!(public_id);
            result["systemId"] = json!(system_id);
        }
        NodeData::Element { name, attrs, .. } => {
            result["nodeType"] = json!(1);
            result["nodeName"] = json!(name.local.as_ref().to_ascii_uppercase());
            result["localName"] = json!(name.local.as_ref());
            result["nodeValue"] = json!("");
            let cdp_attrs: Vec<String> = attrs
                .iter()
                .flat_map(|a| vec![a.name.local.to_string(), a.value.clone()])
                .collect();
            result["attributes"] = json!(cdp_attrs);
        }
        NodeData::Text { contents } => {
            result["nodeType"] = json!(3);
            result["nodeName"] = json!("#text");
            result["localName"] = json!("");
            result["nodeValue"] = json!(contents);
        }
        NodeData::Comment { contents } => {
            result["nodeType"] = json!(8);
            result["nodeName"] = json!("#comment");
            result["localName"] = json!("");
            result["nodeValue"] = json!(contents);
        }
        NodeData::ProcessingInstruction { target, data } => {
            result["nodeType"] = json!(7);
            result["nodeName"] = json!(target);
            result["localName"] = json!("");
            result["nodeValue"] = json!(data);
        }
    }

    Some((result, children_ids))
}

/// Serialize a node and its descendants into the CDP Node tree, iteratively.
/// The requested `max_depth` is clamped to `current_depth + MAX_SERIALIZE_DEPTH`
/// so a `depth:-1` (u32::MAX) request on a very deep DOM cannot produce a Value
/// that overflows the stack when serde_json later serializes or drops it. An
/// explicit heap worklist keeps the builder itself off the call stack.
fn serialize_node(dom: &DomTree, node_id: NodeId, max_depth: u32, current_depth: u32) -> Value {
    let max_depth = max_depth.min(current_depth.saturating_add(MAX_SERIALIZE_DEPTH));

    struct Frame {
        value: Value,
        children: Vec<NodeId>,
        next: usize,
        built: Vec<Value>,
        depth: u32,
        expand: bool,
    }

    let (root_value, root_children) = match node_value(dom, node_id) {
        Some(v) => v,
        None => return json!(null),
    };
    let root_expand = current_depth < max_depth && !root_children.is_empty();
    let mut stack = vec![Frame {
        value: root_value,
        children: root_children,
        next: 0,
        built: Vec::new(),
        depth: current_depth,
        expand: root_expand,
    }];

    loop {
        // Decide the next step without holding a borrow across a push.
        let next_child = {
            let top = stack.last_mut().expect("stack is non-empty in loop");
            if top.expand && top.next < top.children.len() {
                let cid = top.children[top.next];
                top.next += 1;
                Some(cid)
            } else {
                None
            }
        };

        match next_child {
            Some(cid) => {
                let child_depth = stack.last().unwrap().depth + 1;
                match node_value(dom, cid) {
                    Some((cval, cchildren)) => {
                        let cexpand = child_depth < max_depth && !cchildren.is_empty();
                        stack.push(Frame {
                            value: cval,
                            children: cchildren,
                            next: 0,
                            built: Vec::new(),
                            depth: child_depth,
                            expand: cexpand,
                        });
                    }
                    // Missing child: match the old recursive behavior of emitting null.
                    None => stack.last_mut().unwrap().built.push(json!(null)),
                }
            }
            None => {
                // This node's children are all built; finalize and fold into parent.
                let mut frame = stack.pop().unwrap();
                if !frame.built.is_empty() {
                    frame.value["children"] = json!(frame.built);
                }
                match stack.last_mut() {
                    Some(parent) => parent.built.push(frame.value),
                    None => return frame.value,
                }
            }
        }
    }
}

/// Standard base64 (with padding). Used to ferry file bytes to the JS layer for
/// DOM.setFileInputFiles without pulling in a dependency.
fn encode_base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Best-effort MIME type from a file extension, for the File objects created by
/// DOM.setFileInputFiles. Defaults to application/octet-stream.
fn guess_mime(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_object_id_escapes_backslash_before_quote() {
        assert_eq!(escape_object_id(r"x\"), r"x\\");
        assert_eq!(escape_object_id("a'b"), r"a\'b");
        assert_eq!(escape_object_id(r"a\'b"), r"a\\\'b");
        assert_eq!(escape_object_id("plain"), "plain");
    }

    #[test]
    fn encode_base64_matches_standard_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn quad_coords_serialize_integral_values_as_integers() {
        // obscura#576: Chrome's CDP wire emits integral doubles without the
        // ".0" (base::Value JSON writer), and strict clients (Hermes Agent
        // browser tools) deserialize quads as i64 — a "256.0" fails with
        // "invalid type: floating point `256.0`, expected i64". Fractional
        // coordinates must stay fractional. Payload is the issue's quad.
        let quad = quad_from_value(&json!([
            256.0,
            206.0390625,
            347.25,
            206.0390625,
            347.25,
            225.0390625,
            256.0,
            225.0390625
        ]));
        let wire = serde_json::to_string(&quad).expect("serialize");
        assert!(
            wire.starts_with("[256,206.0390625,347.25,"),
            "integral coords must serialize without .0, got {wire}"
        );

        // Fallback quad is all-integral too, and width/height ride the same rule.
        let (quad, w, h) = box_from_value(&json!([
            8.0, 8.0, 108.0, 8.0, 108.0, 28.0, 8.0, 28.0, 100.0, 20.0
        ]));
        let wire = serde_json::to_string(&quad).expect("serialize");
        assert_eq!(
            wire, "[8,8,108,8,108,28,8,28]",
            "fallback quad integral: {wire}"
        );
        assert_eq!(serde_json::to_string(&w).unwrap(), "100");
        assert_eq!(serde_json::to_string(&h).unwrap(), "20");
    }

    #[test]
    fn get_document_deep_tree_does_not_overflow() {
        use crate::diting_dom::DomTree;

        // Build the deep chain directly (no parser) so setup is O(n) and fast.
        let dom = DomTree::new();
        let mut parent = dom.document();
        let depth = 50_000usize;
        for _ in 0..depth {
            let n = dom.new_node(NodeData::Text { contents: String::new() });
            dom.append_child(parent, n);
            parent = n;
        }

        // Mirror getDocument {"depth": -1}: as_i64() == -1, then `depth as u32`.
        let node = serialize_node(&dom, dom.document(), (-1i64) as u32, 0);

        // serde_json's own serialization recurses over the Value nesting; this
        // must not overflow either — that is why depth is bounded, not just the
        // builder made iterative.
        let s = serde_json::to_string(&node).expect("serialize");
        assert!(!s.is_empty());

        // Output nesting is bounded well below the tree's true depth (truncated,
        // not crashed) yet still serializes a meaningful prefix.
        let mut cur = &node;
        let mut levels = 0usize;
        while let Some(children) = cur.get("children").and_then(|c| c.as_array()) {
            let Some(first) = children.first() else { break };
            cur = first;
            levels += 1;
        }
        assert!(levels >= 100, "should serialize a deep prefix, got {levels}");
        assert!(levels < depth, "nesting must be bounded below true depth, got {levels}");
    }
}
