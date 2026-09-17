use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;

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
})();
"#;

// Insert `text` at the caret, replacing any non-collapsed selection the way a
// real browser does when you type over selected text (for example after a
// triple-click select-all).
fn insert_text_js(text: &str) -> String {
    let literal = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function() {{\
            var t = document.activeElement;\
            if (!t || (t.localName !== 'input' && t.localName !== 'textarea')) return;\
            if (t.localName === 'input') {{\
                var ty = String(t.type || 'text').toLowerCase();\
                if (['button','submit','reset','image','checkbox','radio','file','hidden','range','color'].indexOf(ty) >= 0) return;\
            }}\
            var ins = {text};\
            var v = t.value || '';\
            var s = t.selectionStart, e = t.selectionEnd;\
            if (s == null) {{\
                globalThis.__diting_setFieldValue(t, 'value', v + ins);\
            }} else {{\
                s = Math.max(0, Math.min(s, v.length));\
                e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
                var lo = Math.min(s, e), hi = Math.max(s, e);\
                globalThis.__diting_setFieldValue(t, 'value', v.slice(0, lo) + ins + v.slice(hi));\
                var caret = lo + ins.length;\
                t.setSelectionRange(caret, caret);\
            }}\
            t.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {{bubbles:true}})));\
        }})()",
        text = literal,
    )
}

// Backspace deletes a whole code point: when the unit at the cut is a
// trail surrogate, its lead partner goes with it — slicing a single
// UTF-16 unit after an astral character would strand a lone surrogate in
// the value (obscura#1005 same lineage).
const BACKSPACE_JS: &str = "(function() {\
    function cut(v, s) {\
        var k = Math.max(0, s - 1);\
        if (k > 0) {\
            var del = v.charCodeAt(k), prev = v.charCodeAt(k - 1);\
            if (del >= 0xDC00 && del <= 0xDFFF && prev >= 0xD800 && prev <= 0xDBFF) k = k - 1;\
        }\
        return k;\
    }\
    var t = document.activeElement;\
    if (!t || (t.localName !== 'input' && t.localName !== 'textarea')) return;\
    var v = t.value || '';\
    var s = t.selectionStart, e = t.selectionEnd;\
    if (s == null) {\
        globalThis.__diting_setFieldValue(t, 'value', v.slice(0, cut(v, v.length)));\
    } else {\
        s = Math.max(0, Math.min(s, v.length));\
        e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
        if (s !== e) {\
            var lo = Math.min(s, e), hi = Math.max(s, e);\
            globalThis.__diting_setFieldValue(t, 'value', v.slice(0, lo) + v.slice(hi));\
            t.setSelectionRange(lo, lo);\
        } else if (s > 0) {\
            var k = cut(v, s);\
            globalThis.__diting_setFieldValue(t, 'value', v.slice(0, k) + v.slice(s));\
            t.setSelectionRange(k, k);\
        }\
    }\
    t.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {bubbles:true})));\
})()";

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

fn modifier_flags(modifiers: u64) -> (bool, bool, bool, bool) {
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
            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__diting_click_target || document.activeElement || document.body;\
            if (!target) return;\
            globalThis.__diting_click_target = target;\
            globalThis.__diting_mouse_down = {{target:target,button:{button_code},clickCount:{click_count}}};\
            var pd = globalThis.__diting_markTrusted(new PointerEvent('pointerdown', {{bubbles:true,cancelable:true,composed:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:{buttons},pointerId:1,pointerType:'mouse',isPrimary:true,pressure:{buttons}!==0?0.5:0,width:1,height:1}}));\
            if (target.dispatchEvent(pd)) {{\
                var evt = globalThis.__diting_markTrusted(new MouseEvent('mousedown', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:{buttons},detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                if (target.dispatchEvent(evt)) {{ globalThis.__diting_focusTextEntry(target); if ({button_code} === 0) globalThis.__diting_rangeMouseDown(target, {x}, {y}); }}\
            }}\
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
            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || document.body;\
            if (!target) return;\
            var pm = globalThis.__diting_markTrusted(new PointerEvent('pointermove', {{bubbles:true,cancelable:true,composed:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:{buttons},pointerId:1,pointerType:'mouse',isPrimary:true,pressure:{buttons}!==0?0.5:0,width:1,height:1}}));\
            target.dispatchEvent(pm);\
            var evt = globalThis.__diting_markTrusted(new MouseEvent('mousemove', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:{buttons},detail:0,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
            target.dispatchEvent(evt);\
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
            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__diting_click_target || document.activeElement || document.body;\
            if (!target) return;\
            var down = globalThis.__diting_mouse_down;\
            globalThis.__diting_mouse_down = null;\
            var pu = globalThis.__diting_markTrusted(new PointerEvent('pointerup', {{bubbles:true,cancelable:true,composed:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:0,pointerId:1,pointerType:'mouse',isPrimary:true,pressure:0,width:1,height:1}}));\
            target.dispatchEvent(pu);\
            var evt = globalThis.__diting_markTrusted(new MouseEvent('mouseup', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
            target.dispatchEvent(evt);\
            var rd = globalThis.__diting_range_down; globalThis.__diting_range_down = null;\
            if (rd && rd.el && String(rd.el.value) !== rd.old) rd.el.dispatchEvent(new Event('change', {{bubbles:true}}));\
            if (!down || down.button !== {button_code} || {button_code} !== 0) return;\
            var clickTarget = down.target;\
            while (clickTarget && clickTarget !== target && !(clickTarget.contains && clickTarget.contains(target))) {{\
                clickTarget = clickTarget.parentElement;\
            }}\
            if (!clickTarget) return;\
            var click = globalThis.__diting_markTrusted(new MouseEvent('click', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
            globalThis.__diting_dispatchTrustedClick(clickTarget, click, {click_count} >= 3);\
            if ({click_count} === 2) {{\
                var dbl = globalThis.__diting_markTrusted(new MouseEvent('dblclick', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:0,detail:2,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                clickTarget.dispatchEvent(dbl);\
            }}\
        }})()",
        x = x, y = y, button_code = button_code,
        click_count = click_count, alt_key = alt_key, ctrl_key = ctrl_key,
        meta_key = meta_key, shift_key = shift_key,
    )
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "dispatchMouseEvent" => {
            let event_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let x = params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let button = params.get("button").and_then(|v| v.as_str()).unwrap_or("left");
            let button_code = mouse_button_code(button);
            let buttons = params
                .get("buttons")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| mouse_button_mask(button));
            let click_count = params.get("clickCount").and_then(|v| v.as_u64()).unwrap_or(1);
            let modifiers = params.get("modifiers").and_then(|v| v.as_u64()).unwrap_or(0);
            let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);

            if event_type == "mousePressed" {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    let code = mouse_down_js(x, y, button_code, buttons, click_count, modifiers);
                    page.evaluate(&code);
                }
            } else if event_type == "mouseMoved" {
                // A plain move carries no pressed button unless the client
                // says so — the shared `buttons` default above (mask of
                // `button`) would report a held left button and turn hover
                // tracking into a phantom drag.
                let move_buttons = params.get("buttons").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    let code = mouse_move_js(x, y, move_buttons, modifiers);
                    page.evaluate(&code);
                }
            } else if event_type == "mouseReleased" {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    let code = mouse_up_js(x, y, button_code, click_count, modifiers);
                    page.evaluate(&code);
                    let moved = page.process_pending_navigation().await.map_err(|e| e.to_string())?;
                    if moved {
                        // Full navigation event sequence (frameNavigated +
                        // lifecycle + the document's network events), the same
                        // tail Page.navigate uses. A click-triggered form
                        // submit previously pushed frameNavigated alone, so
                        // Playwright's click() waiting for load never resolved
                        // (obscura#886 shape).
                        let page_id = page.id.clone();
                        let extra_network =
                            ctx.other_network_sessions(session_id, &page_id);
                        super::page::emit_navigation_for_page(
                            ctx,
                            session_id,
                            &extra_network,
                            &page_id,
                        );
                    }
                }
            } else if event_type == "mouseWheel" {
                let delta_x = params.get("deltaX").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let delta_y = params.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || document.body || document.documentElement;\
                            if (!target) return;\
                            var wheel = globalThis.__diting_markTrusted(new WheelEvent('wheel', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},deltaX:{delta_x},deltaY:{delta_y},deltaMode:0,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            if (!target.dispatchEvent(wheel)) return;\
                            var dx = {delta_x}, dy = {delta_y};\
                            var root = document.scrollingElement || document.documentElement || document.body;\
                            var scrollTarget = null;\
                            var el = target;\
                            while (el && el.nodeType === 1 && el !== root && el !== document.body && el !== document.documentElement) {{\
                                var maxX = Math.max(0, (el.scrollWidth || 0) - (el.clientWidth || 0));\
                                var maxY = Math.max(0, (el.scrollHeight || 0) - (el.clientHeight || 0));\
                                var style = null;\
                                try {{ style = getComputedStyle(el); }} catch (_e) {{}}\
                                var ox = style ? (style.overflowX || style.overflow || '') : '';\
                                var oy = style ? (style.overflowY || style.overflow || '') : '';\
                                var allowX = ox === 'auto' || ox === 'scroll' || ox === 'overlay';\
                                var allowY = oy === 'auto' || oy === 'scroll' || oy === 'overlay';\
                                var consumesX = allowX && ((dx > 0 && el.scrollLeft < maxX) || (dx < 0 && el.scrollLeft > 0));\
                                var consumesY = allowY && ((dy > 0 && el.scrollTop < maxY) || (dy < 0 && el.scrollTop > 0));\
                                if (consumesX || consumesY) {{ scrollTarget = el; break; }}\
                                el = el.parentElement;\
                            }}\
                            if (!scrollTarget) scrollTarget = root;\
                            if (scrollTarget === root && root && typeof root.scrollBy === 'function') {{\
                                var beforeX = root.scrollLeft, beforeY = root.scrollTop;\
                                root.scrollBy(dx, dy);\
                                if (root.scrollLeft !== beforeX || root.scrollTop !== beforeY) setTimeout(function() {{\
                                    try {{ document.dispatchEvent(new Event('scroll', {{bubbles:false}})); }} catch (_e) {{}}\
                                    try {{ globalThis.dispatchEvent(new Event('scroll', {{bubbles:false}})); }} catch (_e) {{}}\
                                }}, 0);\
                            }} else if (scrollTarget && typeof scrollTarget.scrollBy === 'function') scrollTarget.scrollBy(dx, dy);\
                        }})()",
                        x = x, y = y, delta_x = delta_x, delta_y = delta_y,
                        alt_key = alt_key, ctrl_key = ctrl_key, meta_key = meta_key,
                        shift_key = shift_key,
                    );
                    page.evaluate(&code);
                }
            }

            Ok(json!({}))
        }
        "dispatchKeyEvent" => {
            let event_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let code = params.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");

            // The navigation a key press may queue is loaded AFTER the
            // `page` borrow ends (emit re-borrows ctx) — Backspace below
            // still needs the page handle.
            let mut nav_page_id: Option<String> = None;
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.evaluate(INPUT_HELPERS);
                match event_type {
                    "keyDown" | "rawKeyDown" => {
                        if key == "Escape" {
                            // obscura#952: Escape on a modal dialog runs the
                            // close request (cancelable `cancel`, then close)
                            // as the keydown's default action. The page sees
                            // keydown first; a preventDefault leaves the
                            // dialog open, like Chrome. The bootstrap helper
                            // no-ops when no modal dialog is in scope.
                            let js = "(function() {\
                                var target = document.activeElement || document.body;\
                                var evt = globalThis.__diting_markTrusted(new KeyboardEvent('keydown', {bubbles:true,cancelable:true,key:'Escape',code:'Escape'}));\
                                target.dispatchEvent(evt);\
                                if (!evt.defaultPrevented && globalThis.__diting_dialogEscapeClose) {\
                                    try { globalThis.__diting_dialogEscapeClose(); } catch (e) {}\
                                }\
                            })()";
                            page.evaluate(js);
                        } else if key == "Tab" {
                            // #14 (blitz#899): Tab's default action is
                            // sequential focus navigation. The page sees the
                            // keydown first; a preventDefault leaves focus
                            // where it is, like Chrome. Shift comes from the
                            // modifiers bitmask (bit 3).
                            let shift = params
                                .get("modifiers")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                & 8
                                != 0;
                            let js = format!(
                                "(function() {{\
                                    var target = document.activeElement || document.body;\
                                    var evt = globalThis.__diting_markTrusted(new KeyboardEvent('keydown', {{bubbles:true,cancelable:true,key:'Tab',code:'Tab',shiftKey:{shift}}}));\
                                    target.dispatchEvent(evt);\
                                    if (!evt.defaultPrevented && globalThis.__diting_tabNavigate) {{\
                                        try {{ globalThis.__diting_tabNavigate({shift}); }} catch (e) {{}}\
                                    }}\
                                }})()",
                                shift = shift,
                            );
                            page.evaluate(&js);
                        } else {
                        let js = format!(
                            "(function() {{\
                                var target = document.activeElement || document.body;\
                                var evt = globalThis.__diting_markTrusted(new KeyboardEvent('keydown', {{bubbles:true,cancelable:true,key:'{key}',code:'{code}'}}));\
                                target.dispatchEvent(evt);\
                            }})()",
                            key = key.replace('\\', "\\\\").replace('\'', "\\'"),
                            code = code.replace('\\', "\\\\").replace('\'', "\\'"),
                        );
                        page.evaluate(&js);
                        }

                        // Tab's default action is focus navigation, not text
                        // insertion: a client passing text:"\t" (the CDP
                        // convention) must not splice a tab into the control.
                        if !text.is_empty() && text != "\r" && text != "\n" && key != "Tab" {
                            page.evaluate(&insert_text_js(text));
                        }

                        if key == "Enter" {
                            let js = "(function() {\
                                var target = document.activeElement;\
                                if (!target) return;\
                                target.dispatchEvent(globalThis.__diting_markTrusted(new KeyboardEvent('keypress', {bubbles:true,key:'Enter',code:'Enter'})));\
                                if (target.localName === 'textarea') {\
                                    // Newline splices at the caret like insert_text_js
                                    // does (obscura#577 follow-up): appending at the
                                    // end put the newline after a mid-text caret and
                                    // left the selection pointing before it.
                                    var v = target.value || '';\
                                    var s = target.selectionStart, e = target.selectionEnd;\
                                    var lo, hi;\
                                    if (s == null) { lo = hi = v.length; }\
                                    else {\
                                        s = Math.max(0, Math.min(s, v.length));\
                                        e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
                                        lo = Math.min(s, e); hi = Math.max(s, e);\
                                    }\
                                    globalThis.__diting_setFieldValue(target, 'value', v.slice(0, lo) + '\\n' + v.slice(hi));\
                                    target.setSelectionRange(lo + 1, lo + 1);\
                                    target.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {bubbles:true})));\
                                } else {
                                    // Enter on an activation-behavior element synthesizes
                                    // a click on it (blitz#839): Chrome runs the focused
                                    // button/link/checkbox activation, NOT implicit form
                                    // submission — a focused type=button must not submit.
                                    var ln = target.localName;\
                                    var ty = (ln === 'input') ? String(target.type || 'text').toLowerCase() : '';\
                                    var activatable = ln === 'button' || ln === 'a' || ln === 'area'\
                                        || (ln === 'input' && ['button','submit','reset','image','checkbox','radio','file'].indexOf(ty) >= 0);\
                                    if (activatable) {\
                                        try { target.click(); } catch (e) {}\
                                    } else {\
                                        var form = target.form || (target.closest && target.closest('form'));\
                                        if (form) { try { if (typeof form.requestSubmit === 'function') { form.requestSubmit(); } else { form.submit(); } } catch(e) {} }\
                                    }\
                                }\
                            })()";
                            page.evaluate(js);

                            // Enter's implicit submission queued a navigation
                            // (requestSubmit → op_navigate). Load it after the
                            // page borrow ends — same inline contract as
                            // mouseReleased below; nothing else reads the
                            // queue until the next evaluate, so a
                            // waitForNavigation would hang forever
                            // (obscura#921 keyboard shape).
                            if page
                                .process_pending_navigation()
                                .await
                                .map_err(|e| e.to_string())?
                            {
                                nav_page_id = Some(page.id.clone());
                            }
                        }

                        if key == "Backspace" {
                            page.evaluate(BACKSPACE_JS);
                        }
                    }
                    "keyUp" => {
                        let js = format!(
                            "(function() {{\
                                var target = document.activeElement || document.body;\
                                var evt = globalThis.__diting_markTrusted(new KeyboardEvent('keyup', {{bubbles:true,key:'{key}',code:'{code}'}}));\
                                target.dispatchEvent(evt);\
                            }})()",
                            key = key.replace('\\', "\\\\").replace('\'', "\\'"),
                            code = code.replace('\\', "\\\\").replace('\'', "\\'"),
                        );
                        page.evaluate(&js);

                        // Space activates on keyUP in Chrome (blitz#839): the
                        // focused button/checkbox/radio family fires its click
                        // when the key releases. Links are not Space-
                        // activatable and stay out of the list.
                        if key == " " {
                            let js = "(function() {\
                                var target = document.activeElement;\
                                if (!target) return;\
                                var ln = target.localName;\
                                var ty = (ln === 'input') ? String(target.type || '').toLowerCase() : '';\
                                if (ln === 'button'\
                                    || (ln === 'input' && ['button','submit','reset','image','checkbox','radio'].indexOf(ty) >= 0)) {\
                                    try { target.click(); } catch (e) {}\
                                }\
                            })()";
                            page.evaluate(js);
                        }
                    }
                    "char" => {
                        if !text.is_empty() {
                            page.evaluate(&insert_text_js(text));
                            page.settle(50).await;
                        }
                    }
                    _ => {}
                }
            }

            if let Some(page_id) = nav_page_id {
                let extra_network = ctx.other_network_sessions(session_id, &page_id);
                super::page::emit_navigation_for_page(ctx, session_id, &extra_network, &page_id);
            }

            Ok(json!({}))
        }
        // Text that doesn't come from a key press — IME commits (CJK),
        // emoji pickers, paste-style agent drivers (Hermes browser_type) —
        // has no dispatchKeyEvent representation, so without this arm such
        // clients cannot type at all (obscura#577). Reuses the same
        // insertion path as the char branch: the helper embeds text as a
        // serde_json string literal, which also survives the control-
        // character escaping trap upstream's first arm hit (#688).
        "insertText" => {
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    page.evaluate(&insert_text_js(text));
                    // Pump the event loop so framework change detection
                    // picks up the mutation (same as the char branch above).
                    page.settle(50).await;
                }
            }
            Ok(json!({}))
        }
        "dispatchTouchEvent" => Ok(json!({})),
        "setIgnoreInputEvents" => Ok(json!({})),
        _ => Err(format!("Unknown Input method: {}", method)),
    }
}
