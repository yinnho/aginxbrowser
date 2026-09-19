use serde_json::{json, Value};

use crate::cdp::dispatch::CdpContext;

// Mouse-event synthesis moved down to core (issue #47): the CDP Input
// face and session_click_xy/session_drag must synthesize identical event
// chains, so the shared builders live in core and this face imports them
// downward (ARCHITECTURE.md §2 R1).
use crate::session::interact::{
    modifier_flags, mouse_button_code, mouse_button_mask, mouse_down_js, mouse_move_js,
    mouse_up_js, INPUT_HELPERS,
};

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
