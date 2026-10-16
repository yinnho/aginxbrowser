//! animation shorthand + @keyframes body parsing (god-file ratchet split from mod.rs, pure move).

use super::*;

/// Parse the body of one `@keyframes` rule: top-level `{ ... }` blocks
/// whose selector is `from`, `to`, or a percentage. Stops sort by offset;
/// a body with no surviving stop yields None.
pub(super) fn parse_keyframes_body(inner: &str) -> Option<Keyframes> {
    let mut stops = Vec::new();
    let mut sel = String::new();
    let mut body = String::new();
    let mut depth = 0usize;
    let mut in_comment = false;
    let mut chars = inner.chars().peekable();
    let flush = |sel: &mut String, body: &mut String, stops: &mut Vec<KeyframeStop>| {
        let offset = match sel.trim() {
            "from" => Some(0.0),
            "to" => Some(1.0),
            s => s.strip_suffix('%').and_then(|n| n.trim().css_f32().map(|p| p / 100.0)),
        };
        if let Some(offset) = offset.filter(|o| o.is_finite() && (0.0..=1.0).contains(o)) {
            let decls = split_declarations(body);
            if !decls.is_empty() {
                stops.push(KeyframeStop { offset, decls });
            }
        }
        sel.clear();
        body.clear();
    };
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
            continue;
        }
        if c == '{' {
            depth += 1;
            if depth > 1 {
                body.push(c);
            }
        } else if c == '}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                flush(&mut sel, &mut body, &mut stops);
            } else {
                body.push(c);
            }
        } else if depth == 0 {
            sel.push(c);
        } else {
            body.push(c);
        }
    }
    if !sel.trim().is_empty() {
        flush(&mut sel, &mut body, &mut stops);
    }
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.offset.partial_cmp(&b.offset).unwrap_or(std::cmp::Ordering::Equal));
    Some(Keyframes { stops })
}

/// Parse the `animation` shorthand (v1 single-animation subset). Time tokens
/// fill duration then delay in order; keyword/cubic-bezier tokens set the
/// easing; `forwards`/`both` set fill; the remaining identifier is the name.
/// `none` as the first non-keyword token means no animation at all.
pub(super) fn parse_animation_shorthand(v: &str) -> Option<AnimationSpec> {
    let mut duration: Option<f32> = None;
    let mut delay = 0.0f32;
    let mut easing: Option<Easing> = None;
    let mut fill_forwards = false;
    let mut iterations = 1.0f32;
    let mut name: Option<String> = None;
    for tok in paren_aware_tokens(v) {
        let secs = tok
            .strip_suffix("ms")
            .and_then(|n| n.css_f32().map(|n| n / 1000.0))
            .or_else(|| tok.strip_suffix('s').and_then(|n| n.css_f32()));
        if let Some(s) = secs.filter(|s| s.is_finite() && *s >= 0.0) {
            if duration.is_none() {
                duration = Some(s);
            } else if delay == 0.0 {
                delay = s;
            }
            continue;
        }
        match tok.as_str() {
            "linear" => easing = Some(Easing::Linear),
            "ease" => easing = Some(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
            "ease-in" => easing = Some(Easing::CubicBezier(0.42, 0.0, 1.0, 1.0)),
            "ease-out" => easing = Some(Easing::CubicBezier(0.0, 0.0, 0.58, 1.0)),
            "ease-in-out" => easing = Some(Easing::CubicBezier(0.42, 0.0, 0.58, 1.0)),
            "forwards" | "both" => fill_forwards = true,
            "infinite" => iterations = f32::INFINITY,
            "backwards" | "none" | "alternate" | "reverse"
            | "alternate-reverse" | "running" | "paused" => {}
            _ => {
                if let Some(rest) = tok.strip_prefix("cubic-bezier(") {
                    let args: Vec<Option<f32>> = rest
                        .strip_suffix(')')?
                        .split(',')
                        .map(|a| a.trim().css_f32())
                        .collect();
                    let pts: Option<Vec<f32>> = args.into_iter().collect();
                    let pts = pts?;
                    if pts.len() == 4 && pts.iter().all(|p| p.is_finite()) {
                        easing = Some(Easing::CubicBezier(pts[0], pts[1], pts[2], pts[3]));
                    } else {
                        return None;
                    }
                } else if tok.contains('(') {
                    // `steps()` and other timing functions we don't evaluate
                    // invalidate the whole shorthand — silently dropping the
                    // token would animate with the wrong easing.
                    return None;
                } else if tok
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
                    && !tok.is_empty()
                {
                    // A bare number token is the iteration count (<number>,
                    // fractional legal); anything else identifier-shaped is
                    // the animation name.
                    if tok.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.') {
                        if let Ok(n) = tok.parse::<f32>() {
                            if n.is_finite() && n >= 0.0 {
                                iterations = n;
                            }
                        }
                        continue;
                    }
                    if name.is_none() {
                        name = Some(tok);
                    }
                }
            }
        }
    }
    let name = name?;
    Some(AnimationSpec {
        name,
        duration: duration.unwrap_or(0.0),
        delay,
        easing: easing.unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        fill_forwards,
        iterations,
    })
}
