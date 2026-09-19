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

use crate::cdp::domains::input::{
    mouse_button_code, mouse_button_mask, mouse_down_js, mouse_move_js, mouse_up_js, INPUT_HELPERS,
};

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

pub(super) async fn click_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
) -> Result<SessionClickResponse, String> {
    let nid = *element_map
        .get(&index)
        .ok_or_else(|| format!("invalid index: {}", index))?;
    let js = format!(
        "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el) {{ el.scrollIntoView({{block:'center'}}); el.click(); return true; }} return false; }})()",
        nid
    );
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    let clicked = result.as_bool().unwrap_or(false);
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
    // React/Vue controlled inputs: assigning `el.value` directly goes through
    // React's _valueTracker own-property setter, which records the new value -
    // the following `input` event then compares equal and React swallows it
    // (onChange never fires). Reset the tracker and use the prototype setter
    // so the dispatched event registers as a real change. The response carries
    // the filled element's identity + value readback so a stale element_map
    // (page re-rendered between state and input) is visible in the reply
    // instead of silently typing into the wrong field.
    let set_value = "if (el._valueTracker) el._valueTracker.setValue(''); var p = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value') || Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value');";
    let js_body: String = if full_events {
        // Strict listeners key on keyboard events (keypress-to-submit login
        // forms, masked inputs). Build the value one character at a time with
        // the full keydown/keypress/input/keyup cycle per character, then a
        // single trailing change.
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); {set_value} var text = '{text}'; var cur = ''; for (var i = 0; i < text.length; i++) {{ var ch = text[i]; var kc = ch.charCodeAt(0); var kev = function(t) {{ return new KeyboardEvent(t, {{key: ch, keyCode: kc, which: kc, bubbles: true}}); }}; el.dispatchEvent(kev('keydown')); if (p && p.set) p.set.call(el, cur + ch); else el.value = cur + ch; cur = cur + ch; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(kev('keypress')); el.dispatchEvent(kev('keyup')); }} el.dispatchEvent(new Event('change', {{bubbles: true}})); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
            set_value = set_value,
            text = escaped,
        )
    } else {
        format!(
            r#"(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); {set_value} if (p && p.set) p.set.call(el, '{text}'); else el.value = '{text}'; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return JSON.stringify({{filled: true, tag: el.tagName.toLowerCase(), id: el.id || '', name: el.getAttribute('name') || '', value: el.value}}); }} return '{{"filled":false}}'; }})()"#,
            nid = nid,
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
