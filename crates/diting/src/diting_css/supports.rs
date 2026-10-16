//! @supports declaration probe — split out of mod.rs (god-file ratchet, pure move).

use super::*;

/// Declaration support probe for @supports. Modeled on the properties this
/// cascade understands; everything else reports unsupported (conservative,
/// matches upstream's property-coverage-driven behavior). A supported
/// property with an invalid value still fails the probe, per spec.
pub fn supports_declaration(name: &str, value: &str) -> bool {
    const SUPPORTED: &[&str] = &[
        "display", "color", "background", "background-color", "margin", "margin-top",
        "margin-right", "margin-bottom", "margin-left", "padding", "padding-top",
        "padding-right", "padding-bottom", "padding-left", "font-size", "font-weight",
        "line-height",
        "text-align", "border", "border-top", "border-right", "border-bottom", "border-left",
        "border-color", "border-width", "border-style",
        "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
        "width", "height", "flex-direction", "gap", "overflow", "overflow-x", "overflow-y",
        "white-space", "text-overflow",
        "object-fit", "object-position", "z-index", "border-radius",
        "float", "clear", "border-collapse", "vertical-align", "opacity", "transform",
        "list-style-type", "list-style", "text-align-last",
        // Feature-detection staples (doudian's SSR shell probes these;
        // answering false flipped its render branch and broke hydration #418,
        // #203). position:sticky renders since the sticky batch (#434).
        "position", "inset", "top", "right", "bottom", "left",
        "aspect-ratio", "visibility", "cursor", "user-select", "pointer-events",
        "flex-wrap", "flex-grow", "flex-shrink", "align-items", "justify-content",
        "align-content",
        "min-width", "max-width", "min-height", "max-height",
        "letter-spacing", "text-decoration", "box-sizing",
        // Logical box properties + transform-origin (#244): feature-detection
        // staples of modern component CSS (@logical stubs probe these).
        "margin-block", "margin-block-start", "margin-block-end",
        "margin-inline", "margin-inline-start", "margin-inline-end",
        "padding-block", "padding-block-start", "padding-block-end",
        "padding-inline", "padding-inline-start", "padding-inline-end",
        "transform-origin",
        // fp-diff-1014 P1: Chrome-114+ feature-detection staples — answering
        // false while the UA claims 145 is a version-lag tell. Paint support
        // is degraded (values fall back at apply time); the supports() truth
        // is what fingerprinters read.
        "text-wrap", "accent-color",
    ];
    let name_l = name.to_ascii_lowercase();
    // Custom-property declarations always probe true when the value parses
    // (css-variables-1); every other unknown property stays false.
    if name_l.starts_with("--") {
        return !value.is_empty();
    }
    if !SUPPORTED.contains(&name_l.as_str()) {
        return false;
    }
    let name = name_l.as_str();
    // Unitless nonzero lengths are invalid everywhere (upstream 2c12b5a) —
    // EXCEPT line-height, where a bare number is the canonical form, and
    // opacity, which IS a bare number.
    if !name.eq_ignore_ascii_case("line-height") && !name.eq_ignore_ascii_case("opacity") {
        if let Ok(num) = value.parse::<f64>() {
            return num == 0.0;
        }
    }
    if value.is_empty() {
        return false;
    }
    // Validate values against the same grammar apply_one accepts: a bogus
    // `display: nonsense` must fail the probe like a real browser.
    match name.to_ascii_lowercase().as_str() {
        "display" => matches!(
            value,
            "block" | "inline" | "flex" | "grid" | "none" | "inline-block" | "inline-flex"
                | "inline-grid" | "table" | "inline-table" | "table-row" | "table-cell"
        ),
        "color" | "background-color" => parse_color(value).is_some() || value.starts_with("rgb") || value.starts_with("hsl")
            // CSS Color 4 functions: supported since Chrome 111; a 145 UA
            // probing lab() must hear yes (paint falls back if unparseable).
            || value.starts_with("lab(") || value.starts_with("oklab(")
            || value.starts_with("lch(") || value.starts_with("oklch(")
            || value.starts_with("color("),
        "text-wrap" => matches!(value, "wrap" | "balance" | "pretty" | "stable" | "nowrap"),
        "accent-color" => value == "auto" || parse_color(value).is_some(),
        "background" => parse_color(value.split_whitespace().next().unwrap_or("")).is_some()
            || value.starts_with("url(")
            || value.contains("gradient"),
        "text-align" => matches!(value, "left" | "start" | "center" | "right" | "end" | "justify"),
        "text-align-last" => {
            matches!(value, "auto" | "left" | "start" | "center" | "right" | "end" | "justify")
        }
        "white-space" => matches!(
            value,
            "normal" | "nowrap" | "pre" | "pre-wrap" | "pre-line" | "break-spaces"
        ),
        "text-overflow" => matches!(value, "clip" | "ellipsis"),
        "line-height" => parse_line_height(value).is_some(),
        "object-fit" => matches!(
            value,
            "fill" | "contain" | "cover" | "none" | "scale-down"
        ),
        "object-position" => {
            let part = |s: &str| {
                s.ends_with('%') && s[..s.len() - 1].css_f32().is_some()
                    || s.ends_with("px") && s[..s.len() - 2].css_f32().is_some()
            };
            value
                .split_whitespace()
                .all(|s| matches!(s, "left" | "top" | "center" | "right" | "bottom") || part(s))
        }
        "z-index" => value == "auto" || value.parse::<i32>().is_ok(),
        "float" => matches!(value, "left" | "right" | "none"),
        "clear" => matches!(value, "left" | "right" | "both" | "inline-start" | "inline-end" | "none"),
        "content" => {
            // Generated content: strings (quoted), attr()/counter()/quotes,
            // none/normal. Deeper grammars (counter styles) pass the probe
            // and parse at apply time.
            let t = value.trim();
            let lower = t.to_ascii_lowercase();
            matches!(lower.as_str(), "none" | "normal")
                || lower.starts_with("attr(")
                || lower.starts_with("counter")
                || lower.starts_with("open-quote")
                || lower.starts_with("close-quote")
                || lower.starts_with("no-open-quote")
                || lower.starts_with("no-close-quote")
                || t.starts_with('"')
                || t.starts_with('\'')
        }
        "border-collapse" => matches!(value, "collapse" | "separate"),
        "list-style-type" => matches!(
            value,
            "none" | "disc" | "circle" | "square" | "decimal" | "decimal-leading-zero"
                | "lower-roman" | "upper-roman" | "lower-alpha" | "lower-latin"
                | "upper-alpha" | "upper-latin"
        ),
        "list-style" => value.split_whitespace().any(|t| {
            matches!(
                t,
                "none" | "disc" | "circle" | "square" | "decimal" | "decimal-leading-zero"
                    | "lower-roman" | "upper-roman" | "lower-alpha" | "lower-latin"
                    | "upper-alpha" | "upper-latin" | "inside" | "outside"
            ) || t.starts_with("url(")
        }),
        "vertical-align" => matches!(
            value,
            "top" | "middle" | "bottom" | "baseline" | "sub" | "super"
        ) || parse_css_length(value).is_some(),
        "opacity" => match value.parse::<f32>() {
            Ok(n) => n.is_finite() && (0.0..=1.0).contains(&n),
            Err(_) => false,
        },
        "transform" => parse_transform(value).is_some(),
        "border-radius" => {
            // 1-4 radii, optionally `/` plus 1-4 vertical radii.
            let (horiz, vert) = match value.split_once('/') {
                Some((h, s)) => (h, Some(s)),
                None => (value, None),
            };
            let ok_list =
                |s: &str| -> bool {
                    let vals: Vec<&str> = s.split_whitespace().collect();
                    !vals.is_empty() && vals.len() <= 4 && vals.iter().all(|t| parse_css_length(t).is_some())
                };
            ok_list(horiz)
                && vert.map(ok_list).unwrap_or(true)
        }
        "font-weight" => parse_font_weight(value).is_some(),
        "font-size" => parse_font_size_len(value).is_some(),
        "overflow" => {
            let toks: Vec<&str> = value.split_whitespace().collect();
            (1..=2).contains(&toks.len())
                && toks
                    .iter()
                    .all(|t| matches!(*t, "visible" | "hidden" | "clip" | "scroll" | "auto"))
        }
        "overflow-x" | "overflow-y" => {
            matches!(value, "visible" | "hidden" | "clip" | "scroll" | "auto")
        }
        _ => true, // remaining modeled properties accept any non-empty value here
    }
}
