//! Page interaction: the injected state/media scripts, the indexed-state
//! extractor, and the click/input/drag executors (humanized trajectories
//! included). Split from the session module root (ARCHITECTURE.md P2);
//! behavior unchanged.
use std::collections::HashMap;

use serde_json::Value;

use super::commands::SessionClickResponse;
use crate::page::Page;

/// Collect playback-relevant sources the engine never fetches (media
/// elements, player iframes), as a JSON array of `{url, tag}`. Relative URLs
/// resolve against the page location; duplicates collapse.
pub(super) const DOM_MEDIA_SCRIPT: &str = r#"(function(){
    var out = [];
    var seen = {};
    function add(u, tag) {
        if (!u) return;
        try { u = new URL(String(u), location.href).href; } catch (e) { return; }
        if (!seen[u]) { seen[u] = 1; out.push({ url: u, tag: tag }); }
    }
    var els = document.querySelectorAll('video,audio,source,iframe');
    for (var i = 0; i < els.length; i++) {
        var e = els[i];
        add(e.getAttribute('src'), e.tagName.toLowerCase());
    }
    return JSON.stringify(out);
})()"#;

/// Merge DOM-observed candidates into the network-derived media list. A
/// candidate the network log already confirms (same URL ignoring query —
/// players append auth/expiry tokens the markup never carries) is dropped;
/// iframes surface as kind "iframe" (player pages to navigate or sniff
/// inside, not playable URLs themselves); the rest must classify as media
/// or they are dropped. These are candidates, not confirmations — `via`
/// says which side produced each entry.
pub(super) fn merge_dom_candidates(media: &mut Vec<Value>, dom_json: &str) {
    let dom: Vec<Value> = match serde_json::from_str(dom_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    let bare = |u: &str| u.split(['?', '#']).next().unwrap_or(u).to_ascii_lowercase();
    let confirmed: std::collections::HashSet<String> = media
        .iter()
        .filter_map(|m| m["url"].as_str())
        .map(bare)
        .collect();
    for cand in dom {
        let Some(url) = cand["url"].as_str().map(str::to_string) else {
            continue;
        };
        let tag = cand["tag"].as_str().unwrap_or("").to_string();
        if confirmed.contains(&bare(&url)) {
            continue;
        }
        let kind = if tag == "iframe" {
            "iframe".to_string()
        } else {
            match crate::har::media_kind(&url, None) {
                Some(k) => k.to_string(),
                None => continue,
            }
        };
        media.push(serde_json::json!({
            "url": url,
            "kind": kind,
            "via": "dom",
            "tag": tag,
        }));
    }
}

// ---------------------------------------------------------------------------
// Indexed state extraction
// ---------------------------------------------------------------------------

/// JS script that queries all interactive elements, assigns sequential indexes,
/// stores the `_nid` mapping in `window.__session_element_map`, and returns a
/// JSON array of element descriptors.
const STATE_SCRIPT: &str = r#"
(function() {
    var interactive = Array.prototype.slice.call(document.querySelectorAll(
        'a, button, input, select, textarea, [role="button"], [role="link"], [onclick], [tabindex]'
    ));
    // Elements with a JS-bound click listener (jQuery .click(), addEventListener)
    // are invisible to the selector above — a div-classed login button leaves the
    // agent with no indexed way to click it. The engine's own event registry
    // already knows every listener target, so union those nids in and restore
    // document order.
    try {
        var reg = (typeof _eventRegistry === 'undefined') ? null : _eventRegistry;
        if (reg) {
            var seenNids = {};
            for (var s = 0; s < interactive.length; s++) {
                var n0 = interactive[s]._nid;
                if (n0 !== undefined) seenNids[n0] = true;
            }
            for (var nid in reg) {
                if (seenNids[nid]) continue;
                var rec = reg[nid];
                if (!rec || !rec.click || !rec.click.length) continue;
                var bound = globalThis._wrap && globalThis._wrap(parseInt(nid, 10));
                if (bound && bound.tagName) {
                    interactive.push(bound);
                    seenNids[nid] = true;
                }
            }
            interactive.sort(function(a, b) {
                if (!a.compareDocumentPosition || !b.compareDocumentPosition) return 0;
                var p = a.compareDocumentPosition(b);
                return (p & 4) ? -1 : ((p & 2) ? 1 : 0);
            });
        }
    } catch (e) {}
    var elements = [];
    var indexMap = {};
    var idx = 0;
    for (var i = 0; i < interactive.length; i++) {
        var el = interactive[i];
        var style = el.offsetWidth === 0 && el.offsetHeight === 0;
        if (style) continue;
        var box = el.getBoundingClientRect();
        var info = {
            index: idx,
            tag: el.tagName.toLowerCase(),
            text: (el.innerText || '').trim().substring(0, 100),
            x: Math.round(box.x), y: Math.round(box.y),
            w: Math.round(box.width), h: Math.round(box.height),
            attrs: {}
        };
        var attrNames = ['id', 'class', 'href', 'type', 'name', 'value',
                         'placeholder', 'aria-label', 'title', 'src', 'alt', 'role'];
        for (var j = 0; j < attrNames.length; j++) {
            var v = el.getAttribute(attrNames[j]);
            if (v !== null) info.attrs[attrNames[j]] = v;
        }
        // Control state the agent would otherwise need a follow-up eval for:
        // checked (checkbox/radio), disabled, and the select's current option.
        try {
            var elType = (el.getAttribute('type') || '').toLowerCase();
            if (el.tagName === 'INPUT' && (elType === 'checkbox' || elType === 'radio') && el.checked) {
                info.attrs.checked = 'checked';
            }
            if (el.disabled) info.attrs.disabled = 'disabled';
            if (el.tagName === 'SELECT' && el.selectedIndex >= 0 && el.options[el.selectedIndex]) {
                var opt = el.options[el.selectedIndex];
                var optVal = opt.getAttribute('value') !== null ? opt.getAttribute('value') : (opt.textContent || '');
                info.attrs.selected = optVal.trim().substring(0, 40);
            }
        } catch (e) {}
        if (el._nid !== undefined) {
            indexMap[idx] = el._nid;
        }
        elements.push(info);
        idx++;
    }
    window.__session_element_map = indexMap;
    return JSON.stringify({url: location.href, title: document.title,
                           viewport: {w: window.innerWidth, h: window.innerHeight},
                           elements: elements});
})()
"#;

pub(super) fn extract_indexed_state(
    page: &mut Page,
    element_map: &mut HashMap<usize, u64>,
) -> Result<String, String> {
    let val = page.evaluate(STATE_SCRIPT);
    let json_str = match val.as_str() {
        Some(s) => s.to_string(),
        None => return Err("state extraction returned non-string".into()),
    };

    // Parse the JSON to extract element_map, then format as compact text.
    let parsed: Value =
        serde_json::from_str(&json_str).map_err(|e| format!("state parse error: {}", e))?;

    // Build element_map from the JS-side indexMap.
    let map_val = page.evaluate("JSON.stringify(window.__session_element_map)");
    if let Some(map_str) = map_val.as_str() {
        if let Ok(map_obj) = serde_json::from_str::<HashMap<String, u64>>(map_str) {
            for (k, v) in map_obj {
                if let Ok(idx) = k.parse::<usize>() {
                    element_map.insert(idx, v);
                }
            }
        }
    }

    // Format compact text output.
    let url = parsed.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = parsed.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    out.push_str(&format!("url={}\n", url));
    out.push_str(&format!("title={}\n", title));
    // Viewport size so the agent can tell which rects are on-screen
    // (scroll down / scroll to element before clicking off-viewport ones).
    if let Some(vp) = parsed.get("viewport") {
        out.push_str(&format!(
            "viewport={}x{}\n\n",
            vp.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
            vp.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
        ));
    } else {
        out.push('\n');
    }

    if let Some(elements) = parsed.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            let idx = el.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let tag = el.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            let text = el.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let attrs = el.get("attrs").and_then(|v| v.as_object());

            let mut attr_parts = Vec::new();
            if let Some(attrs) = attrs {
                for (k, v) in attrs {
                    let vs = v.as_str().unwrap_or("");
                    // Truncate long class values. Slice by chars, not bytes —
                    // multi-byte UTF-8 (—, CJK) panics on byte indexing.
                    if k == "class" && vs.chars().count() > 50 {
                        let t: String = vs.chars().take(50).collect();
                        attr_parts.push(format!("{}=\"{}…\"", k, t));
                    } else {
                        attr_parts.push(format!("{}=\"{}\"", k, vs));
                    }
                }
            }
            let attr_str = if attr_parts.is_empty() {
                String::new()
            } else {
                format!(" {}", attr_parts.join(" "))
            };
            // Page-relative rect (viewport coords: y is relative to the
            // current scroll position) — lets the agent "see where it is"
            // before clicking or scrolling.
            let rect = format!(
                " rect=[{},{},{}x{}]",
                el.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
            );

            if text.is_empty() {
                out.push_str(&format!("[{}] <{}{}{} />\n", idx, tag, attr_str, rect));
            } else {
                let display_text = if text.chars().count() > 80 {
                    let t: String = text.chars().take(80).collect();
                    format!("{}…", t)
                } else {
                    text.to_string()
                };
                out.push_str(&format!(
                    "[{}] <{}{}{}>{}</{}>\n",
                    idx, tag, attr_str, rect, display_text, tag
                ));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Click / Input by index
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Mouse-event synthesis — the shared builders (moved down from the CDP
// Input face, issue #47): the CDP bridge and session click_xy/drag must
// synthesize identical event chains, so this JS lives in core and the
// face imports it downward (ARCHITECTURE.md §2 R1's allowed direction).
// ---------------------------------------------------------------------------

/// Input-synthesis helpers injected idempotently before the first synthesized
/// event. Upstream obscura-js defines these in bootstrap.rs (#303 isTrusted
/// WeakSet, #324 React value-tracker bypass); diting's bootstrap has not
/// absorbed those yet, so the CDP bridge ships them as a self-contained
/// preload until they land in the engine proper.
///
/// `markTrusted` is a no-op today because diting's Event.isTrusted currently
/// returns true unconditionally; once bootstrap absorbs the #303 WeakSet, this
/// same helper name becomes the real marker and the Input domain needs no
/// change.
/// Wrapped in an IIFE: `Page.evaluate` treats its argument as a single
/// expression unless it starts with var/let/const/if/for/while/return, so the
/// bare multi-statement form parsed as `return (a; b; c)` — a SyntaxError —
/// and every helper stayed undefined, silently killing all mouse input.
pub(crate) const INPUT_HELPERS: &str = r#"(function() {
globalThis.__diting_markTrusted = globalThis.__diting_markTrusted || function(ev) { return ev; };
globalThis.__diting_setFieldValue = globalThis.__diting_setFieldValue || function(el, field, value) {
  try {
    let proto = Object.getPrototypeOf(el);
    let desc;
    while (proto && !((desc = Object.getOwnPropertyDescriptor(proto, field)) && desc.set)) {
      proto = Object.getPrototypeOf(proto);
    }
    if (desc && desc.set) { desc.set.call(el, value); return; }
  } catch (_e) {}
  el[field] = value;
};
// FileList-like object: an array with the DOM's `item(i)` accessor.
function __diting_makeFileList(files) {
  const list = files.slice();
  Object.defineProperty(list, "item", { value: (i) => list[i] || null, enumerable: false });
  return list;
}
// Populate an <input type=file>'s files from the CDP DOM.setFileInputFiles call
// (Puppeteer uploadFile / Playwright setInputFiles, upstream issue #359).
// `specs` is an array of { name, type, b64 } where b64 is the base64-encoded
// file bytes read on the Rust side. Real File objects are created so page code
// can read them via FileReader or upload them via fetch/FormData; then
// input+change fire as a genuine selection would.
globalThis.__diting_setInputFiles = function(el, specs) {
  const files = (specs || []).map((s) => {
    let bytes;
    try {
      const bin = globalThis.atob(s.b64 || "");
      bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    } catch (_e) { bytes = new Uint8Array(0); }
    return new File([bytes], s.name || "", { type: s.type || "" });
  });
  el._files = __diting_makeFileList(files);
  try { el.dispatchEvent(new Event("input", { bubbles: true })); } catch (_e) {}
  try { el.dispatchEvent(new Event("change", { bubbles: true })); } catch (_e) {}
};
// Chrome's mousedown default action for text-entry controls: focus and
// place the caret. A real click hit-tests the text and puts the caret at
// the click point; the engine has no glyph hit-testing, so the caret
// lands at the end of the value — exactly where a click past the last
// glyph would put it, and where typing then appends. Scoped to
// text-entry controls on purpose: the engine historically focused
// nothing on mousedown, and widening this to buttons/links would light
// :focus styles on every navigation click with no focus-ring heuristic
// to gate them.
globalThis.__diting_focusTextEntry = globalThis.__diting_focusTextEntry || function(t) {
  try {
    if (!t || !t.localName) return;
    if (t.localName !== "input" && t.localName !== "textarea") return;
    if (t.localName === "input") {
      const ty = String(t.getAttribute("type") || "text").toLowerCase();
      if (["button","submit","reset","image","checkbox","radio","file","hidden","range","color"].indexOf(ty) >= 0) return;
    }
    if (typeof t.focus === "function") t.focus();
    const v = t.value;
    if (typeof v === "string" && typeof t.setSelectionRange === "function") {
      t.setSelectionRange(v.length, v.length);
    }
  } catch (_e) {}
};
// Chrome's mousedown default action for a range input (blitz#456): the
// thumb snaps to the click position — mapped over the same thumb-inset
// track span the paint uses, stepped per the step attribute, committed
// through the value setter (React tracker + live mirror) — and `input`
// fires. The recorded pre-click value gates `change` on release: a
// gesture that never moved the value commits nothing. `el.click()` never
// reaches this (no coordinates → no snap), which is Chrome-faithful.
globalThis.__diting_rangeMouseDown = globalThis.__diting_rangeMouseDown || function(t, x, y) {
  try {
    if (!t || t.localName !== "input" || t.disabled) return;
    if (String(t.getAttribute("type") || "").toLowerCase() !== "range") return;
    const r = (t.getBoundingClientRect && t.getBoundingClientRect()) || null;
    if (!r || r.width <= 0) return;
    let min = parseFloat(t.min); if (isNaN(min)) min = 0;
    let max = parseFloat(t.max); if (isNaN(max) || max < min) max = min;
    let step = parseFloat(t.step); if (isNaN(step) || step <= 0) step = 1;
    const thumb = 7;
    const span = Math.max(r.width - thumb * 2, 1);
    const frac = Math.max(0, Math.min(1, (x - r.left - thumb) / span));
    const raw = min + frac * (max - min);
    let v = min + Math.round((raw - min) / step) * step;
    v = Math.max(min, Math.min(max, v));
    const old = String(t.value ?? "");
    globalThis.__diting_setFieldValue(t, "value", String(v));
    t.dispatchEvent(new Event("input", { bubbles: true }));
    globalThis.__diting_range_down = { el: t, old: old };
  } catch (_e) {}
};
// Real input hit-testing descends into child frames; elementFromPoint stops
// at the iframe element (its spec contract). Returns {el, x, y} with the
// coordinates translated into the deepest frame's own viewport (iframe-doc
// gBCR is iframe-local, no stitching — obscura #976 sub-run semantics).
globalThis.__diting_hitTarget = globalThis.__diting_hitTarget || function(x, y) {
  var doc = document, ox = 0, oy = 0, el = null;
  for (var depth = 0; depth < 8; depth++) {
    el = (doc && doc.elementFromPoint) ? doc.elementFromPoint(x - ox, y - oy) : null;
    if (!el || el.localName !== "iframe") break;
    var idoc = el._iframeDoc || null;
    if (!idoc || typeof idoc.elementFromPoint !== "function") break;
    var r = el.getBoundingClientRect();
    ox += r.left; oy += r.top;
    doc = idoc;
  }
  if (!el) return null;
  return { el: el, x: x - ox, y: y - oy };
};
// The frame scope an element lives in: [iframeDoc, iframeWin] or null for
// main-document elements. Input events dispatched on a frame element must
// run with the frame's window/document globals — an inline handler doing
// `window.__clicked = true` writes the FRAME's window, which is exactly
// what a frame-scoped Runtime.evaluate reads back (webtop/cua-bench gym).
globalThis.__diting_frameScopeOf = globalThis.__diting_frameScopeOf || function(el) {
  for (var n = el; n; n = n.parentNode) {
    if (n._isIframeDocRoot) {
      var doc = n._ownerDoc || null;
      var win = doc && doc._iframeEl && doc._iframeEl._iframeWin;
      return (doc && win) ? [doc, win] : null;
    }
  }
  return null;
};
globalThis.__diting_inFrameScope = globalThis.__diting_inFrameScope || function(scope, fn) {
  if (!scope) return fn(globalThis);
  var saved = [globalThis.document, globalThis.window, globalThis.self, globalThis.frames];
  try {
    globalThis.document = scope[0];
    globalThis.window = scope[1];
    globalThis.self = scope[1];
    globalThis.frames = scope[1];
    return fn(scope[1]);
  } finally {
    globalThis.document = saved[0];
    globalThis.window = saved[1];
    globalThis.self = saved[2];
    globalThis.frames = saved[3];
  }
};
})();
"#;

pub(crate) fn mouse_button_code(button: &str) -> u8 {
    match button {
        "middle" => 1,
        "right" => 2,
        "back" => 3,
        "forward" => 4,
        _ => 0,
    }
}

pub(crate) fn mouse_button_mask(button: &str) -> u64 {
    match button {
        "right" => 2,
        "middle" => 4,
        "back" => 8,
        "forward" => 16,
        "none" => 0,
        _ => 1,
    }
}

pub(crate) fn modifier_flags(modifiers: u64) -> (bool, bool, bool, bool) {
    // CDP Input.Modifier: Alt=1, Ctrl=2, Meta=4, Shift=8.
    (
        modifiers & 1 != 0,
        modifiers & 2 != 0,
        modifiers & 4 != 0,
        modifiers & 8 != 0,
    )
}

// Shared mouse-event builders: the CDP bridge and the session commands
// (session_click_xy / session_drag) must synthesize identical event chains,
// so the JS lives here rather than duplicated per layer. `modifiers` is the
// CDP Input.Modifier bitfield (Alt=1, Ctrl=2, Meta=4, Shift=8).

pub(crate) fn mouse_down_js(
    x: f64,
    y: f64,
    button_code: u8,
    buttons: u64,
    click_count: u64,
    modifiers: u64,
) -> String {
    let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);
    format!(
        "(function() {{\
            var hit = globalThis.__diting_hitTarget ? globalThis.__diting_hitTarget({x},{y}) : null;\
            var target = (hit && hit.el) || globalThis.__diting_click_target || document.activeElement || document.body;\
            if (!target) return;\
            var ex = hit ? hit.x : {x}, ey = hit ? hit.y : {y};\
            globalThis.__diting_click_target = target;\
            globalThis.__diting_mouse_down = {{target:target,button:{button_code},clickCount:{click_count}}};\
            globalThis.__diting_inFrameScope(globalThis.__diting_frameScopeOf(target), function(win) {{\
                var pd = globalThis.__diting_markTrusted(new PointerEvent('pointerdown', {{bubbles:true,cancelable:true,composed:true,view:win,clientX:ex,clientY:ey,button:{button_code},buttons:{buttons},pointerId:1,pointerType:'mouse',isPrimary:true,pressure:{buttons}!==0?0.5:0,width:1,height:1}}));\
                if (target.dispatchEvent(pd)) {{\
                    var evt = globalThis.__diting_markTrusted(new MouseEvent('mousedown', {{bubbles:true,cancelable:true,view:win,clientX:ex,clientY:ey,button:{button_code},buttons:{buttons},detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                    if (target.dispatchEvent(evt)) {{ globalThis.__diting_focusTextEntry(target); if ({button_code} === 0) globalThis.__diting_rangeMouseDown(target, ex, ey); }}\
                }}\
            }});\
        }})()",
        x = x, y = y, button_code = button_code, buttons = buttons,
        click_count = click_count, alt_key = alt_key, ctrl_key = ctrl_key,
        meta_key = meta_key, shift_key = shift_key,
    )
}

pub(crate) fn mouse_move_js(x: f64, y: f64, buttons: u64, modifiers: u64) -> String {
    let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);
    format!(
        "(function() {{\
            var hit = globalThis.__diting_hitTarget ? globalThis.__diting_hitTarget({x},{y}) : null;\
            var target = (hit && hit.el) || document.body;\
            if (!target) return;\
            var ex = hit ? hit.x : {x}, ey = hit ? hit.y : {y};\
            globalThis.__diting_inFrameScope(globalThis.__diting_frameScopeOf(target), function(win) {{\
                var pm = globalThis.__diting_markTrusted(new PointerEvent('pointermove', {{bubbles:true,cancelable:true,composed:true,view:win,clientX:ex,clientY:ey,button:0,buttons:{buttons},pointerId:1,pointerType:'mouse',isPrimary:true,pressure:{buttons}!==0?0.5:0,width:1,height:1}}));\
                target.dispatchEvent(pm);\
                var evt = globalThis.__diting_markTrusted(new MouseEvent('mousemove', {{bubbles:true,cancelable:true,view:win,clientX:ex,clientY:ey,button:0,buttons:{buttons},detail:0,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                target.dispatchEvent(evt);\
            }});\
        }})()",
        x = x, y = y, buttons = buttons,
        alt_key = alt_key, ctrl_key = ctrl_key, meta_key = meta_key,
        shift_key = shift_key,
    )
}

pub(crate) fn mouse_up_js(
    x: f64,
    y: f64,
    button_code: u8,
    click_count: u64,
    modifiers: u64,
) -> String {
    let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);
    format!(
        "(function() {{\
            var hit = globalThis.__diting_hitTarget ? globalThis.__diting_hitTarget({x},{y}) : null;\
            var target = (hit && hit.el) || globalThis.__diting_click_target || document.activeElement || document.body;\
            if (!target) return;\
            var ex = hit ? hit.x : {x}, ey = hit ? hit.y : {y};\
            var down = globalThis.__diting_mouse_down;\
            globalThis.__diting_mouse_down = null;\
            globalThis.__diting_inFrameScope(globalThis.__diting_frameScopeOf(target), function(win) {{\
                var pu = globalThis.__diting_markTrusted(new PointerEvent('pointerup', {{bubbles:true,cancelable:true,composed:true,view:win,clientX:ex,clientY:ey,button:{button_code},buttons:0,pointerId:1,pointerType:'mouse',isPrimary:true,pressure:0,width:1,height:1}}));\
                target.dispatchEvent(pu);\
                var evt = globalThis.__diting_markTrusted(new MouseEvent('mouseup', {{bubbles:true,cancelable:true,view:win,clientX:ex,clientY:ey,button:{button_code},buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                target.dispatchEvent(evt);\
                var rd = globalThis.__diting_range_down; globalThis.__diting_range_down = null;\
                if (rd && rd.el && String(rd.el.value) !== rd.old) rd.el.dispatchEvent(new Event('change', {{bubbles:true}}));\
                if (!down || down.button !== {button_code} || {button_code} !== 0) return;\
                var clickTarget = down.target;\
                while (clickTarget && clickTarget !== target && !(clickTarget.contains && clickTarget.contains(target))) {{\
                    clickTarget = clickTarget.parentElement;\
                }}\
                if (!clickTarget) return;\
                var click = globalThis.__diting_markTrusted(new MouseEvent('click', {{bubbles:true,cancelable:true,view:win,clientX:ex,clientY:ey,button:0,buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                globalThis.__diting_dispatchTrustedClick(clickTarget, click, {click_count} >= 3);\
                if ({click_count} === 2) {{\
                    var dbl = globalThis.__diting_markTrusted(new MouseEvent('dblclick', {{bubbles:true,cancelable:true,view:win,clientX:ex,clientY:ey,button:0,buttons:0,detail:2,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                    clickTarget.dispatchEvent(dbl);\
                }}\
            }});\
        }})()",
        x = x, y = y, button_code = button_code,
        click_count = click_count, alt_key = alt_key, ctrl_key = ctrl_key,
        meta_key = meta_key, shift_key = shift_key,
    )
}

pub(super) fn eval_interaction(page: &mut Page, js: &str) {
    page.evaluate_with_timeout(js, crate::page::INTERACTION_EVAL_TIMEOUT);
}

/// Click at viewport coordinates through the real mouse chain — same JS the
/// CDP bridge dispatches, so pages can't tell the two apart.
pub(super) async fn click_xy(page: &mut Page, x: f64, y: f64, button: &str, click_count: u32) {
    let code = mouse_button_code(button);
    let mask = mouse_button_mask(button);
    eval_interaction(page, INPUT_HELPERS);
    eval_interaction(
        page,
        &mouse_down_js(x, y, code, mask, click_count as u64, 0),
    );
    eval_interaction(page, &mouse_up_js(x, y, code, click_count as u64, 0));
}

/// Press → mousemoves → release. Linear path (`humanize: false`) is exact
/// interpolation for tests/tools that need precise geometry; the humanized
/// path (default) feeds the moves through [`humanized_drag_plan`] so the
/// trajectory reads as a real hand (anti-bot heuristics score linear
/// constant-velocity glides as synthetic).
pub(super) async fn drag_xy(
    page: &mut Page,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    steps: u32,
    delay_ms: u64,
    humanize: bool,
) {
    eval_interaction(page, INPUT_HELPERS);
    eval_interaction(page, &mouse_down_js(from_x, from_y, 0, 1, 1, 0));
    if humanize {
        let plan = humanized_drag_plan(
            from_x,
            from_y,
            to_x,
            to_y,
            steps,
            delay_ms,
            &mut XorShift64::from_entropy(),
        );
        for pt in &plan.points {
            if pt.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(pt.delay_ms)).await;
            }
            eval_interaction(page, &mouse_move_js(pt.x, pt.y, 1, 0));
        }
        if plan.settle_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(plan.settle_ms)).await;
        }
    } else {
        let n = steps as f64;
        for i in 1..=steps {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            let k = i as f64 / n;
            let x = from_x + (to_x - from_x) * k;
            let y = from_y + (to_y - from_y) * k;
            eval_interaction(page, &mouse_move_js(x, y, 1, 0));
        }
    }
    eval_interaction(page, &mouse_up_js(to_x, to_y, 0, 1, 0));
}

// ---------------------------------------------------------------------------
// Humanized drag trajectory
// ---------------------------------------------------------------------------

pub(super) struct XorShift64(pub(super) u64);

impl XorShift64 {
    fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let probe = &nanos as *const u64 as u64;
        let seed = nanos ^ probe.rotate_left(17) ^ 0x9E37_79B9_7F4A_7C15;
        XorShift64(if seed == 0 { 0x853C_49E6_748F_EA9B } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub(super) struct DragPoint {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) delay_ms: u64,
}

pub(super) struct DragPlan {
    pub(super) points: Vec<DragPoint>,
    pub(super) settle_ms: u64,
}

/// Minimum-jerk glide plus perpendicular wobble, per-step timing jitter, a
/// grip pause after mousedown, a settle pause before mouseup, occasional
/// hesitation micro-pauses, and (for long drags) sometimes an overshoot with
/// a corrective pull-back — the irregularities anti-bot trajectory models
/// score for. The wobble envelope dies at both ends, so the first point sits
/// on the start line and the last move lands exactly on `to` either way.
/// Deterministic given the seed.
pub(super) fn humanized_drag_plan(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    steps: u32,
    mean_ms: u64,
    rng: &mut XorShift64,
) -> DragPlan {
    let dx = to_x - from_x;
    let dy = to_y - from_y;
    let dist = (dx * dx + dy * dy).sqrt();
    let (ux, uy) = if dist > f64::EPSILON {
        (dx / dist, dy / dist)
    } else {
        (1.0, 0.0)
    };
    let (px, py) = (-uy, ux);
    let mean = mean_ms as f64;
    let n = steps.max(2) as usize;

    let amp = 0.8 + rng.next_f64() * 1.7;
    let freq = 1.0 + rng.next_f64() * 1.5;
    let phase = rng.next_f64() * std::f64::consts::TAU;
    let overshoot = if dist > 60.0 && rng.next_f64() < 0.35 {
        (2.0 + rng.next_f64() * 4.0).min(dist * 0.04)
    } else {
        0.0
    };

    let mut points = Vec::with_capacity(n + 2);
    for i in 1..=n {
        let t = i as f64 / n as f64;
        let s = t * t * t * (10.0 + t * (-15.0 + 6.0 * t));
        let along = (dist + overshoot) * s;
        let env = (std::f64::consts::PI * t).sin();
        let wobble = amp * env * (0.55 + 0.45 * (phase + t * freq * std::f64::consts::TAU).sin());
        let x = from_x + ux * along + px * wobble;
        let y = from_y + uy * along + py * wobble;
        let mut delay = mean * (0.55 + rng.next_f64() * 0.95);
        if rng.next_f64() < 0.12 {
            delay += 30.0 + rng.next_f64() * 60.0;
        }
        if i == 1 {
            delay = mean * (3.0 + rng.next_f64() * 3.0);
        }
        points.push(DragPoint {
            x,
            y,
            delay_ms: delay.round() as u64,
        });
    }
    if overshoot > 0.0 {
        for frac in [0.6, 1.0] {
            let along = dist + overshoot * (1.0 - frac);
            points.push(DragPoint {
                x: from_x + ux * along,
                y: from_y + uy * along,
                delay_ms: (mean * (1.5 + rng.next_f64() * 1.5)).round() as u64,
            });
        }
    }
    let settle_ms = (mean * (5.0 + rng.next_f64() * 5.0)).round() as u64;
    DragPlan { points, settle_ms }
}

/// Act-side re-check script (ARCHITECTURE.md §5, issue #46): verify
/// identity/enabled/visible/occlusion in the same frame as the click and
/// answer with a structured verdict instead of dispatching into a dead
/// target. The observe half (session_state rects) goes stale the moment the
/// page re-renders between the state call and the click; this closes that
/// gap. Failures are named — `gone`/`detached`/`disabled`/`not_visible`/
/// `covered_by` — and `covered_by` describes the element that would eat the
/// real click, so the agent can dismiss it or click it instead of guessing.
const CLICK_RECHECK_SCRIPT: &str = r#"(function() {
    var el = globalThis._wrap && globalThis._wrap(NID);
    if (!el || !el.tagName) return JSON.stringify({clicked: false, reason: 'gone'});
    if (!el.isConnected) return JSON.stringify({clicked: false, reason: 'detached'});
    // :disabled matches form controls only; ARIA-disabled and inert gate the
    // rest (a div-button the page marks unclickable mid-session).
    if (el.matches(':disabled') || el.closest('[aria-disabled="true"],[inert]'))
        return JSON.stringify({clicked: false, reason: 'disabled'});
    if (typeof el.checkVisibility === 'function' &&
        !el.checkVisibility({checkOpacity: true, checkVisibilityCSS: true}))
        return JSON.stringify({clicked: false, reason: 'not_visible'});
    el.scrollIntoView({block: 'center'});
    // Post-scroll geometry, then hit-test the center point. elementFromPoint
    // is z-index aware, so a hit that is neither the target nor its
    // descendant nor an ancestor wrapping it means something is covering the
    // target. (An ancestor counts as a pass: a label wrapping its input
    // forwards activation, so a real click through it still lands.)
    var r = el.getBoundingClientRect();
    if (r.width <= 0 && r.height <= 0)
        return JSON.stringify({clicked: false, reason: 'not_visible'});
    var hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    if (!hit || (hit !== el && !el.contains(hit) && !hit.contains(el))) {
        function desc(e) {
            if (!e || !e.tagName) return 'unknown';
            var s = '<' + String(e.tagName).toLowerCase();
            if (e.id) s += ' id="' + e.id + '"';
            var cls = String(e.className || '');
            if (cls) s += ' class="' + cls.substring(0, 60) + '"';
            return s + '>';
        }
        return JSON.stringify({clicked: false, reason: 'covered_by',
            covered_by: hit ? desc(hit) : 'nothing (center point outside the viewport)'});
    }
    el.click();
    return JSON.stringify({clicked: true});
})()"#;

pub(super) async fn click_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
) -> Result<SessionClickResponse, String> {
    let nid = *element_map
        .get(&index)
        .ok_or_else(|| format!("invalid index: {}", index))?;
    let js = CLICK_RECHECK_SCRIPT.replacen("NID", &nid.to_string(), 1);
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    // A non-string/non-JSON result is an engine hiccup, not a click — read
    // it as unclicked with a raw reason rather than guessing success.
    let verdict = result
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({"clicked": false, "reason": "no verdict"}));
    let clicked = verdict
        .get("clicked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Drain any JS-initiated navigation the click started (location.href /
    // form.submit) so the returned URL reflects the post-click page — matches
    // the firecrawl /v1/scrape click handling.
    let _ = page.process_pending_navigation().await;
    if clicked {
        // Wait for quiescence, not a fixed slice: a client-side route
        // transition (RSC fetch → flight parse → render → pushState) only
        // counts as done when the loop drains. Capped so interval-heavy
        // pages can't pin the command.
        page.settle_until_idle(5000).await;
    }
    let url = page.url();
    let text_after = page
        .evaluate("document.body.innerText")
        .as_str()
        .map(|s| s.chars().take(2000).collect::<String>());
    Ok(SessionClickResponse {
        url,
        clicked,
        reason: verdict
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        covered_by: verdict
            .get("covered_by")
            .and_then(Value::as_str)
            .map(str::to_string),
        text_after,
    })
}

pub(super) fn input_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
    text: &str,
    full_events: bool,
) -> Result<Value, String> {
    let nid = *element_map
        .get(&index)
        .ok_or_else(|| format!("invalid index: {}", index))?;
    // Escape single quotes in text.
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    // Act-side re-check (issue #46), same frame as the fill: a detached,
    // disabled, or readonly control answers `filled:false` with a named
    // reason instead of silently writing a value nobody will read back.
    // No visibility gate here on purpose: hidden inputs (display:none plus a
    // custom-drawn widget) are legitimate fill targets, and Chrome accepts
    // script value assignment on them too.
    // React/Vue controlled inputs: assigning `el.value` directly goes through
    // React's _valueTracker own-property setter, which records the new value -
    // the following `input` event then compares equal and React swallows it
    // (onChange never fires). Reset the tracker and use the prototype setter
    // so the dispatched event registers as a real change. The response carries
    // the filled element's identity + value readback so a stale element_map
    // (page re-rendered between state and input) is visible in the reply
    // instead of silently typing into the wrong field.
    let guard = "if (!el.isConnected) return '{\"filled\":false,\"reason\":\"detached\"}'; if (el.disabled) return '{\"filled\":false,\"reason\":\"disabled\"}'; if (el.readOnly || el.closest('[aria-readonly=\"true\"]')) return '{\"filled\":false,\"reason\":\"readonly\"}';";
    let set_value = "if (el._valueTracker) el._valueTracker.setValue(''); var p = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value') || Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value');";
    let js_body: String = if full_events {
        // Strict listeners key on keyboard events (keypress-to-submit login
        // forms, masked inputs). Build the value one character at a time with
        // the full keydown/keypress/input/keyup cycle per character, then a
        // single trailing change. The tail blurs like a human leaving the
        // field — React pages that commit in onBlur (tmall's SKU suggest,
        // #100) never see their value land without it.
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ {guard} el.focus(); {set_value} var text = '{text}'; var cur = ''; for (var i = 0; i < text.length; i++) {{ var ch = text[i]; var kc = ch.charCodeAt(0); var kev = function(t) {{ return new KeyboardEvent(t, {{key: ch, keyCode: kc, which: kc, bubbles: true}}); }}; el.dispatchEvent(kev('keydown')); if (p && p.set) p.set.call(el, cur + ch); else el.value = cur + ch; cur = cur + ch; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(kev('keypress')); el.dispatchEvent(kev('keyup')); }} el.dispatchEvent(new Event('change', {{bubbles: true}})); el.blur(); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
            guard = guard,
            set_value = set_value,
            text = escaped,
        )
    } else {
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ {guard} el.focus(); {set_value} if (p && p.set) p.set.call(el, '{text}'); else el.value = '{text}'; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
            guard = guard,
            set_value = set_value,
            text = escaped,
        )
    };
    let result = page.evaluate_with_timeout(&js_body, crate::page::INTERACTION_EVAL_TIMEOUT);
    let parsed = result
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({"filled": false}));
    Ok(parsed)
}

/// Programmatic file selection (Playwright setInputFiles semantics): locate a
/// file input by CSS selector, build File objects from base64 content
/// specs, assign through `el.files`, then dispatch input+change so framework
/// onChange handlers fire. Selector-addressed on purpose — file inputs are
/// routinely hidden, so the interactive index the other commands use may not
/// include them at all.
pub(super) fn set_files_by_selector(page: &mut Page, selector: &str, files: &[Value]) -> Result<Value, String> {
    if selector.trim().is_empty() {
        return Err("selector must not be empty".to_string());
    }
    // JSON-encode both interpolations so selector quotes and spec strings
    // can't break out of the script literal.
    let selector_json = serde_json::to_string(selector).map_err(|e| e.to_string())?;
    let specs = serde_json::to_string(files).map_err(|e| e.to_string())?;
    let js = format!(
        r#"(function() {{
    var el = document.querySelector({selector_json});
    if (!el) return JSON.stringify({{set: false, error: "selector matched no element"}});
    if (el.tagName !== 'INPUT' || (el.getAttribute('type') || '').toLowerCase() !== 'file')
        return JSON.stringify({{set: false, error: "not a file input"}});
    var specs = {specs};
    var out = [];
    for (var i = 0; i < specs.length; i++) {{
        var s = specs[i] || {{}};
        var bin;
        try {{ bin = atob(s.content_base64 || ''); }} catch (e) {{
            return JSON.stringify({{set: false, error: "invalid base64 for " + (s.name || ("file " + i))}});
        }}
        var bytes = new Uint8Array(bin.length);
        for (var j = 0; j < bin.length; j++) bytes[j] = bin.charCodeAt(j);
        var opts = {{ type: s.mime_type || 'application/octet-stream' }};
        if (s.last_modified != null) opts.lastModified = Number(s.last_modified);
        out.push(new File([bytes], String(s.name || 'blob'), opts));
    }}
    el.files = out;
    el.dispatchEvent(new Event('input', {{bubbles: true}}));
    el.dispatchEvent(new Event('change', {{bubbles: true}}));
    return JSON.stringify({{
        set: true,
        selector: {selector_json},
        count: out.length,
        value: el.value,
        files: out.map(function(f) {{ return {{name: f.name, size: f.size, type: f.type}}; }})
    }});
}})()"#,
        selector_json = selector_json,
        specs = specs,
    );
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    let parsed = result
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .ok_or_else(|| "files assignment did not return a JSON string".to_string())?;
    Ok(parsed)
}

/// The Wait command's poll loop: exactly one of `selector`/`predicate`
/// (the arm rejects anything else). Clamp keeps a stray request from
/// pinning the session thread (Close included) for long. While waiting
/// the page's loop keeps running — fetches/timers only advance when
/// pumped, and the slice parks when quiescent so idle pages don't spin
/// hot.
pub(super) async fn wait(
    page: &mut Page,
    selector: Option<&str>,
    predicate: Option<&str>,
    timeout_ms: u64,
) -> Result<String, String> {
    let timeout_ms = timeout_ms.clamp(1, 120_000);
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_millis(timeout_ms);
    let mut last_error: Option<String> = None;
    loop {
        let detail = if let Some(sel) = selector {
            let escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
            let js = format!(
                "(function(){{ var el = document.querySelector('{}'); \
                 if (!el) return null; \
                 return {{tag: el.tagName, \
                 text: (el.textContent || '').trim().slice(0, 200)}}; }})()",
                escaped
            );
            let v = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
            if v.is_null() { None } else { Some(v) }
        } else {
            let pred = predicate.unwrap_or("false");
            let js = format!(
                "(function(){{ try {{ var v = ({pred}); \
                 if (!v) return {{truthy: false}}; \
                 return {{truthy: true, value: \
                 (typeof v === 'object' && v !== null \
                 ? JSON.stringify(v) : String(v)).slice(0, 200)}}; \
                 }} catch (e) {{ return {{truthy: false, \
                 error: String(e).slice(0, 200)}}; }} }})()"
            );
            let v = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
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
            return Ok(serde_json::json!({
                "matched": true,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "detail": detail,
            })
            .to_string());
        }
        if std::time::Instant::now() >= deadline {
            let what = selector
                .map(|s| format!("selector \"{s}\""))
                .unwrap_or_else(|| format!("predicate \"{}\"", predicate.unwrap_or("")));
            let mut msg = format!("timeout after {timeout_ms}ms waiting for {what}");
            if let Some(e) = last_error {
                msg.push_str(&format!(" (last error: {e})"));
            }
            return Err(msg);
        }
        page.pump_event_loop_slice(150).await;
    }
}
