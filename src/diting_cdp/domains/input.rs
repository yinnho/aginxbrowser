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

const BACKSPACE_JS: &str = "(function() {\
    var t = document.activeElement;\
    if (!t || (t.localName !== 'input' && t.localName !== 'textarea')) return;\
    var v = t.value || '';\
    var s = t.selectionStart, e = t.selectionEnd;\
    if (s == null) {\
        globalThis.__diting_setFieldValue(t, 'value', v.slice(0, -1));\
    } else {\
        s = Math.max(0, Math.min(s, v.length));\
        e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
        if (s !== e) {\
            var lo = Math.min(s, e), hi = Math.max(s, e);\
            globalThis.__diting_setFieldValue(t, 'value', v.slice(0, lo) + v.slice(hi));\
            t.setSelectionRange(lo, lo);\
        } else if (s > 0) {\
            globalThis.__diting_setFieldValue(t, 'value', v.slice(0, s - 1) + v.slice(s));\
            t.setSelectionRange(s - 1, s - 1);\
        }\
    }\
    t.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {bubbles:true})));\
})()";

fn mouse_button_code(button: &str) -> u8 {
    match button {
        "middle" => 1,
        "right" => 2,
        "back" => 3,
        "forward" => 4,
        _ => 0,
    }
}

fn mouse_button_mask(button: &str) -> u64 {
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
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__diting_click_target || document.activeElement || document.body;\
                            if (!target) return;\
                            globalThis.__diting_click_target = target;\
                            globalThis.__diting_mouse_down = {{target:target,button:{button_code},clickCount:{click_count}}};\
                            var evt = globalThis.__diting_markTrusted(new MouseEvent('mousedown', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:{buttons},detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            target.dispatchEvent(evt);\
                        }})()",
                        x = x, y = y, button_code = button_code, buttons = buttons,
                        click_count = click_count, alt_key = alt_key, ctrl_key = ctrl_key,
                        meta_key = meta_key, shift_key = shift_key,
                    );
                    page.evaluate(&code);
                }
            } else if event_type == "mouseReleased" {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.evaluate(INPUT_HELPERS);
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__diting_click_target || document.activeElement || document.body;\
                            if (!target) return;\
                            var down = globalThis.__diting_mouse_down;\
                            globalThis.__diting_mouse_down = null;\
                            var evt = globalThis.__diting_markTrusted(new MouseEvent('mouseup', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            target.dispatchEvent(evt);\
                            if (!down || down.button !== {button_code} || {button_code} !== 0) return;\
                            var clickTarget = down.target;\
                            while (clickTarget && clickTarget !== target && !(clickTarget.contains && clickTarget.contains(target))) {{\
                                clickTarget = clickTarget.parentElement;\
                            }}\
                            if (!clickTarget) return;\
                            var tag = clickTarget.tagName;\
                            var type = (clickTarget.getAttribute && clickTarget.getAttribute('type') || '').toLowerCase();\
                            var checkable = tag === 'INPUT' && (type === 'checkbox' || type === 'radio');\
                            var oldChecked = checkable ? !!clickTarget.checked : false;\
                            var radioStates = null;\
                            if (checkable && type === 'radio') {{\
                                var radioName = clickTarget.getAttribute('name') || '';\
                                if (radioName) {{\
                                    var candidates = document.querySelectorAll('input');\
                                    radioStates = [];\
                                    for (var ri = 0; ri < candidates.length; ri++) {{\
                                        var radio = candidates[ri];\
                                        if ((radio.getAttribute('type') || '').toLowerCase() !== 'radio' || (radio.getAttribute('name') || '') !== radioName || radio.form !== clickTarget.form) continue;\
                                        radioStates.push([radio, !!radio.checked]);\
                                        if (radio !== clickTarget) radio.checked = false;\
                                    }}\
                                }}\
                                clickTarget.checked = true;\
                            }} else if (checkable) {{\
                                clickTarget.checked = !oldChecked;\
                            }}\
                            var click = globalThis.__diting_markTrusted(new MouseEvent('click', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            var cancelled = !clickTarget.dispatchEvent(click);\
                            if (cancelled) {{\
                                if (radioStates) {{\
                                    for (var rr = 0; rr < radioStates.length; rr++) radioStates[rr][0].checked = radioStates[rr][1];\
                                }} else if (checkable) clickTarget.checked = oldChecked;\
                                return;\
                            }}\
                            if (checkable && clickTarget.checked !== oldChecked) {{\
                                try {{ clickTarget.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {{bubbles:true}}))); }} catch(e) {{}}\
                                try {{ clickTarget.dispatchEvent(globalThis.__diting_markTrusted(new Event('change', {{bubbles:true}}))); }} catch(e) {{}}\
                                return;\
                            }}\
                            var labelHost = (clickTarget.matches && !clickTarget.matches('button,input:not([type=hidden]),meter,output,progress,select,textarea,a')) ? (tag === 'LABEL' ? clickTarget : (clickTarget.closest ? clickTarget.closest('label') : null)) : null;\
                            if (labelHost) {{\
                                var lblFor = labelHost.getAttribute('for');\
                                var ctl = null;\
                                if (lblFor !== null && lblFor !== undefined) ctl = lblFor === '' ? null : document.getElementById(lblFor);\
                                else if (labelHost.querySelector) ctl = labelHost.querySelector('button,input:not([type=hidden]),meter,output,progress,select,textarea');\
                                if (ctl && !(ctl.matches && ctl.matches('button,input:not([type=hidden]),meter,output,progress,select,textarea'))) ctl = null;\
                                if (ctl && ctl !== clickTarget && !ctl.disabled && !ctl.hasAttribute('disabled')) {{ ctl.click(); return; }}\
                            }}\
                            var link = clickTarget.closest ? clickTarget.closest('a[href]') : null;\
                            if (!link && tag === 'A' && clickTarget.getAttribute('href')) link = clickTarget;\
                            if (link) {{\
                                var href = link.getAttribute('href');\
                                if (href && !href.startsWith('#') && !href.startsWith('javascript:')) location.assign(href);\
                            }} else if (tag === 'BUTTON' && type !== 'button' && type !== 'reset') {{\
                                var form = clickTarget.closest ? clickTarget.closest('form') : null;\
                                if (form) {{ try {{ if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(clickTarget); }} else {{ form.submit(clickTarget); }} }} catch(e) {{}} }}\
                            }} else if (tag === 'INPUT' && (type === 'submit' || type === 'image')) {{\
                                var form2 = clickTarget.closest ? clickTarget.closest('form') : null;\
                                if (form2) {{ try {{ if (typeof form2.requestSubmit === 'function') {{ form2.requestSubmit(clickTarget); }} else {{ form2.submit(clickTarget); }} }} catch(e) {{}} }}\
                            }} else if ({click_count} >= 3 && (tag === 'INPUT' || tag === 'TEXTAREA')) {{\
                                var len = clickTarget.value ? clickTarget.value.length : 0;\
                                if (clickTarget.setSelectionRange) clickTarget.setSelectionRange(0, len);\
                                else {{ clickTarget.selectionStart = 0; clickTarget.selectionEnd = len; }}\
                            }}\
                        }})()",
                        x = x, y = y, button_code = button_code,
                        click_count = click_count, alt_key = alt_key, ctrl_key = ctrl_key,
                        meta_key = meta_key, shift_key = shift_key,
                    );
                    page.evaluate(&code);
                    let moved = page.process_pending_navigation().await.map_err(|e| e.to_string())?;
                    if moved {
                        let url = page.url_string();
                        let frame_id = page.frame_id.clone();
                        let page_id = page.id.clone();
                        let loader_id = ctx
                            .current_loader_ids
                            .get(&page_id)
                            .cloned()
                            .unwrap_or_else(|| format!("loader-{}", page_id));
                        ctx.pending_events.push(crate::diting_cdp::types::CdpEvent {
                            method: "Page.frameNavigated".into(),
                            params: json!({
                                "frame": crate::diting_cdp::domains::page::frame_json(
                                    &frame_id,
                                    &loader_id,
                                    &url,
                                ),
                                "type": "Navigation",
                            }),
                            session_id: Some(session_id.clone().unwrap_or_default()),
                        });
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

            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.evaluate(INPUT_HELPERS);
                match event_type {
                    "keyDown" | "rawKeyDown" => {
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

                        if !text.is_empty() && text != "\r" && text != "\n" {
                            page.evaluate(&insert_text_js(text));
                        }

                        if key == "Enter" {
                            let js = "(function() {\
                                var target = document.activeElement;\
                                if (!target) return;\
                                target.dispatchEvent(globalThis.__diting_markTrusted(new KeyboardEvent('keypress', {bubbles:true,key:'Enter',code:'Enter'})));\
                                if (target.localName === 'textarea') {\
                                    globalThis.__diting_setFieldValue(target, 'value', (target.value || '') + '\\n');\
                                    target.dispatchEvent(globalThis.__diting_markTrusted(new Event('input', {bubbles:true})));\
                                } else {\
                                    var form = target.form || (target.closest && target.closest('form'));\
                                    if (form) {{ try {{ if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(); }} else {{ form.submit(); }} }} catch(e) {{}} }}\
                                }\
                            })()";
                            page.evaluate(js);
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

            Ok(json!({}))
        }
        "dispatchTouchEvent" => Ok(json!({})),
        "setIgnoreInputEvents" => Ok(json!({})),
        _ => Err(format!("Unknown Input method: {}", method)),
    }
}
