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
    if CHROME_EXTRA_PROPS.contains(&name_l.as_str()) && !SUPPORTED.contains(&name_l.as_str()) {
        // Parsed-and-ignored properties: true for any sane value (never
        // `subgrid`, whose layout this engine does not do).
        return !value.is_empty() && !value.eq_ignore_ascii_case("subgrid") && !value.contains("subgrid");
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

/// Pseudo-class / pseudo-element names `CSS.supports('selector(...)')` answers
/// true for — Chrome's answer for a UA claiming Chrome 145, minus the ones
/// whose behaviour is invisible-by-design here are kept true (feature
/// detection reads the answer, not the rendering).
pub(super) fn pseudo_known(head: &str) -> bool {
    matches!(
        head,
        "hover" | "focus" | "focus-visible" | "focus-within" | "active" | "visited" | "link" | "any-link"
            | "checked" | "indeterminate" | "disabled" | "enabled" | "placeholder-shown" | "read-only"
            | "read-write" | "required" | "optional" | "default" | "valid" | "invalid" | "in-range"
            | "out-of-range" | "user-valid" | "user-invalid" | "autofill" | "target" | "target-within"
            | "first-child" | "last-child" | "only-child" | "first-of-type" | "last-of-type" | "only-of-type"
            | "root" | "empty" | "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type"
            | "not" | "is" | "where" | "has" | "lang" | "dir" | "scope" | "defined" | "popover-open" | "modal"
            | "fullscreen" | "host" | "host-context" | "slotted" | "part" | "before" | "after" | "first-line"
            | "first-letter" | "selection" | "placeholder" | "marker" | "backdrop" | "file-selector-button"
            | "cue" | "highlight" | "spelling-error" | "grammar-error" | "target-text" | "view-transition"
    )
}

/// Properties Chrome 145 supports that this cascade does not model (they
/// parse and are ignored). `CSS.supports` and `@supports` answer true for a
/// non-empty value so feature detection takes the same branch as Chrome —
/// except the ones whose JS/CSS fallback matters for layout: anchor
/// positioning, scroll/view timelines, which stay false so pages use their
/// fallbacks.
pub(super) const CHROME_EXTRA_PROPS: &[&str] = &[
    "all", "animation", "animation-delay", "animation-direction", "animation-duration", "animation-fill-mode",
    "animation-iteration-count", "animation-name", "animation-play-state", "animation-timing-function",
    "animation-composition", "appearance", "backdrop-filter", "backface-visibility", "background-attachment",
    "background-blend-mode", "background-clip", "background-image", "background-origin", "background-position",
    "background-position-x", "background-position-y", "background-repeat", "background-size", "block-size",
    "border-bottom-color", "border-bottom-left-radius", "border-bottom-right-radius", "border-bottom-style",
    "border-image", "border-image-outset", "border-image-repeat", "border-image-slice", "border-image-source",
    "border-image-width", "border-left-color", "border-left-style", "border-right-color", "border-right-style",
    "border-spacing", "border-top-color", "border-top-left-radius", "border-top-right-radius", "border-top-style",
    "border-block", "border-inline", "border-start-start-radius", "border-start-end-radius",
    "border-end-start-radius", "border-end-end-radius", "box-decoration-break", "box-shadow", "break-after",
    "break-before", "break-inside", "caption-side", "caret-color", "clip", "clip-path", "color-scheme",
    "column-count", "column-fill", "column-gap", "column-rule", "column-rule-color", "column-rule-style",
    "column-rule-width", "column-span", "column-width", "columns", "contain", "contain-intrinsic-size",
    "contain-intrinsic-width", "contain-intrinsic-height", "container", "container-name", "container-type",
    "content-visibility", "counter-increment", "counter-reset", "counter-set", "direction", "empty-cells",
    "field-sizing", "fill", "fill-opacity", "fill-rule", "filter", "flex", "flex-basis", "flex-flow", "font",
    "font-family", "font-feature-settings", "font-kerning", "font-language-override", "font-optical-sizing",
    "font-palette", "font-size-adjust", "font-stretch", "font-style", "font-synthesis", "font-variant",
    "font-variant-alternates", "font-variant-caps", "font-variant-east-asian", "font-variant-ligatures",
    "font-variant-numeric", "font-variant-position", "font-variation-settings", "forced-color-adjust",
    "grid", "grid-area", "grid-auto-columns", "grid-auto-flow", "grid-auto-rows", "grid-column",
    "grid-column-end", "grid-column-start", "grid-row", "grid-row-end", "grid-row-start", "grid-template",
    "grid-template-areas", "grid-template-columns", "grid-template-rows", "column-gap", "row-gap",
    "hyphens", "hyphenate-character", "image-rendering", "initial-letter", "inline-size", "inset-block",
    "inset-block-end", "inset-block-start", "inset-inline", "inset-inline-end", "inset-inline-start",
    "interpolate-size", "isolation", "justify-items", "justify-self", "align-self", "align-tracks",
    "letter-spacing", "line-break", "line-clamp", "list-style-image", "list-style-position",
    "mask", "mask-clip", "mask-composite", "mask-image", "mask-mode", "mask-origin", "mask-position",
    "mask-repeat", "mask-size", "mask-type", "max-block-size", "max-inline-size", "min-block-size",
    "min-inline-size", "mix-blend-mode", "offset", "offset-anchor", "offset-distance", "offset-path",
    "offset-position", "offset-rotate", "order", "orphans", "outline", "outline-color", "outline-offset",
    "outline-style", "outline-width", "overflow-anchor", "overflow-block", "overflow-clip-margin",
    "overflow-inline", "overflow-wrap", "overscroll-behavior", "overscroll-behavior-block",
    "overscroll-behavior-inline", "overscroll-behavior-x", "overscroll-behavior-y", "padding-inline",
    "page", "page-break-after", "page-break-before", "page-break-inside", "paint-order", "perspective",
    "perspective-origin", "place-content", "place-items", "place-self", "print-color-adjust", "quotes",
    "resize", "rotate", "ruby-position", "scale", "scroll-behavior", "scroll-margin", "scroll-margin-block",
    "scroll-margin-bottom", "scroll-margin-inline", "scroll-margin-left", "scroll-margin-right",
    "scroll-margin-top", "scroll-padding", "scroll-padding-block", "scroll-padding-bottom",
    "scroll-padding-inline", "scroll-padding-left", "scroll-padding-right", "scroll-padding-top",
    "scroll-snap-align", "scroll-snap-stop", "scroll-snap-type", "scrollbar-color", "scrollbar-gutter",
    "scrollbar-width", "shape-image-threshold", "shape-margin", "shape-outside", "stroke", "stroke-dasharray",
    "stroke-dashoffset", "stroke-linecap", "stroke-linejoin", "stroke-miterlimit", "stroke-opacity",
    "stroke-width", "tab-size", "table-layout", "text-decoration-color", "text-decoration-line",
    "text-decoration-skip-ink", "text-decoration-style", "text-decoration-thickness", "text-emphasis",
    "text-emphasis-color", "text-emphasis-position", "text-emphasis-style", "text-indent", "text-justify",
    "text-orientation", "text-rendering", "text-shadow", "text-size-adjust", "text-transform",
    "text-underline-offset", "text-underline-position", "text-wrap-mode", "text-wrap-style", "touch-action",
    "transform-box", "transform-style", "transition", "transition-behavior", "transition-delay",
    "transition-duration", "transition-property", "transition-timing-function", "translate",
    "unicode-bidi", "view-transition-name", "will-change", "word-break", "word-spacing", "word-wrap",
    "writing-mode", "zoom", "align-content", "align-items", "justify-content", "flex-direction",
];
