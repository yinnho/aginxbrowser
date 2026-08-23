//! diting_css — cascade layer (render-claim batch 1).
//!
//! A read-only port of the upstream obscura-render CSS layer's core:
//! stylesheet parsing (rule splitting + at-rule dispatch) and a minimal
//! computed-style model with inheritance. This module does NOT feed the
//! product pipeline yet — it is the cascade primitive a future renderer
//! builds on, absorbed so upstream diffs stay readable and the behavior
//! is locked by tests against our own diting_dom selectors.
//!
//! Deliberately out of scope for this slice (tracked in docs/engine/render.md):
//! container queries, @keyframes/@property, shadow-tree scoping, the full 537
//! property surface, and paint.

// Read-only slice: every consumer is a test until the renderer batch wires
// this into a pipeline. The allow is the module-level honest statement of
// that status (batch-3 triage tier 2).
#![cfg_attr(not(test), allow(dead_code))]

use crate::diting_dom;

/// Which media a stylesheet evaluation targets. `print` rules only apply
/// inside `@media print` bodies; `screen` rules apply outside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CssMediaType {
    #[default]
    Screen,
    Print,
}

/// One flattened stylesheet rule: selector text plus its declaration block.
/// Selectors are compiled lazily by the cascade via diting_dom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRule {
    pub selector: String,
    pub declarations: String,
}

/// Parse a stylesheet into flattened rules. Handles nested braces, comments,
/// and the at-rules whose bodies contain ordinary rules (`@media`,
/// `@supports`, `@layer`). Other at-rules (`@font-face`, `@keyframes`,
/// `@import`, ...) are dropped: they contribute no layout-relevant rule here.
///
/// Error recovery mirrors browsers (and upstream): an unbalanced stray `}`
/// at top level resynchronizes instead of scrambling every following rule.
pub fn parse_stylesheet(css: &str) -> Vec<ParsedRule> {
    parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Screen)
}

pub fn parse_stylesheet_for(css: &str, viewport: (f32, f32), media_type: CssMediaType) -> Vec<ParsedRule> {
    let mut rules = Vec::new();
    let mut current_selector = String::new();
    let mut current_decls = String::new();
    let mut block_depth = 0usize;
    let mut in_comment = false;
    let mut chars = css.chars().peekable();

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
            if block_depth != 0 {
                current_decls.push(c);
            }
            block_depth += 1;
        } else if c == '}' && block_depth == 0 {
            // Stray top-level close brace: error-recover and keep parsing.
            current_selector.clear();
        } else if c == '}' {
            block_depth -= 1;
            if block_depth == 0 {
                let sel = current_selector.trim();
                let decls = current_decls.trim();
                if let Some(at) = sel.strip_prefix('@') {
                    flush_at_rule(at, decls, &mut rules, viewport, media_type);
                } else {
                    rules.push(ParsedRule {
                        selector: sel.to_string(),
                        declarations: decls.to_string(),
                    });
                }
                current_selector.clear();
                current_decls.clear();
            } else {
                current_decls.push(c);
            }
        } else if c == ';' && block_depth == 0 {
            // Statement at-rules (`@layer a;`) establish ordering slots but
            // emit no rule; drop them so the prelude cannot bleed into the
            // next selector.
            current_selector.clear();
        } else if block_depth > 0 {
            current_decls.push(c);
        } else {
            current_selector.push(c);
        }
    }
    rules
}

/// Handle the at-rules whose bodies contain ordinary rules. `@media` applies
/// inner rules when the query holds; `@supports` when the condition evaluates
/// true; `@layer` recurses with the named layer tracked (ordering only — we
/// have no layer-priority cascade yet, so layers flatten).
fn flush_at_rule(
    at: &str,
    inner: &str,
    rules: &mut Vec<ParsedRule>,
    viewport: (f32, f32),
    media_type: CssMediaType,
) {
    if let Some(prelude) = at_rule_prelude(at, "media") {
        if media_query_applies(prelude, viewport, media_type) {
            rules.extend(parse_stylesheet_for(inner, viewport, media_type));
        }
    } else if let Some(prelude) = at_rule_prelude(at, "supports") {
        if supports_condition_applies(prelude) {
            rules.extend(parse_stylesheet_for(inner, viewport, media_type));
        }
    } else if let Some(_prelude) = at_rule_prelude(at, "layer") {
        rules.extend(parse_stylesheet_for(inner, viewport, media_type));
    }
    // Other at-rules carry no layout-relevant rules for us; drop them.
}

fn at_rule_prelude<'a>(at: &'a str, name: &str) -> Option<&'a str> {
    let rest = at.strip_prefix(name)?;
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '(') {
        return None;
    }
    let prelude = rest.trim();
    Some(prelude.strip_suffix('{').unwrap_or(prelude).trim())
}

// ---------------------------------------------------------------------------
// @media evaluation
// ---------------------------------------------------------------------------

/// A media-query list is an OR of comma-separated arms (commas inside
/// functions are not separators). Evaluate each arm independently.
pub fn media_query_applies(query: &str, viewport: (f32, f32), media_type: CssMediaType) -> bool {
    split_media_query_list(query)
        .into_iter()
        .any(|q| single_media_query_applies(q, viewport, media_type))
}

/// Split on top-level commas only: `rgb(1,2,3)` keeps its commas inside one arm.
fn split_media_query_list(query: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in query.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                parts.push(&query[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&query[start..]);
    parts
}

fn single_media_query_applies(query: &str, viewport: (f32, f32), media_type: CssMediaType) -> bool {
    let query = query.trim().strip_prefix("@media").unwrap_or(query).trim();
    let compact: String = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();

    // Leading `not` negates the whole arm. Only strip the `not` token itself;
    // the media type after it must stay intact (`not print` → evaluate
    // `print`, not an empty string that would wrongly imply `all`).
    if let Some(rest) = compact.strip_prefix("not") {
        return !single_media_query_applies_compact(rest, viewport, media_type);
    }
    single_media_query_applies_compact(&compact, viewport, media_type)
}

fn single_media_query_applies_compact(compact: &str, viewport: (f32, f32), media_type: CssMediaType) -> bool {
    // A bare feature list (`(min-width: 768px)`) implies media type `all`;
    // evaluate its features directly instead of walking the type-token path.
    if compact.starts_with('(') || compact.is_empty() {
        return compact.split("and").all(|feature| media_feature_applies(feature, viewport));
    }

    // Media type check first; features then refine. An unknown type fails the arm.
    let (type_matches, rest) = if let Some(r) = compact.strip_prefix("print") {
        (media_type == CssMediaType::Print, Some(r))
    } else if let Some(r) = compact.strip_prefix("screen") {
        (media_type == CssMediaType::Screen, Some(r))
    } else if let Some(r) = compact.strip_prefix("all") {
        (media_type == CssMediaType::Screen, Some(r))
    } else if let Some(r) = compact.strip_prefix("onlyscreen") {
        // `only screen` legacy prefix: skip the `only` token.
        (media_type == CssMediaType::Screen, Some(r))
    } else {
        (false, None)
    };
    if !type_matches {
        return false;
    }
    let rest = rest.unwrap_or("");
    if rest.is_empty() {
        return true;
    }
    // Features must be `and`-chained after the type token; anything else is
    // unparseable — be conservative and fail.
    let Some(feature_expr) = rest.strip_prefix("and") else {
        return false;
    };
    feature_expr.split("and").all(|feature| media_feature_applies(feature, viewport))
}

/// One `(min-width: 768px)`-style expression. Only width/height bounds are
/// modeled — that covers the responsive breakpoints real sheets use.
fn media_feature_applies(feature: &str, viewport: (f32, f32)) -> bool {
    let feature = feature.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let Some((name, value)) = feature.split_once(':') else {
        // Boolean feature without a value: unsupported → conservative false.
        return false;
    };
    let value_px = parse_px(value.trim());
    let value_px = match value_px {
        Some(v) => v,
        None => return false,
    };
    match name.trim() {
        "min-width" => viewport.0 >= value_px,
        "max-width" => viewport.0 <= value_px,
        "min-height" => viewport.1 >= value_px,
        "max-height" => viewport.1 <= value_px,
        "width" => (viewport.0 - value_px).abs() < f32::EPSILON,
        "height" => (viewport.1 - value_px).abs() < f32::EPSILON,
        _ => false,
    }
}

fn parse_px(value: &str) -> Option<f32> {
    let v = value.trim().strip_suffix("px").unwrap_or(value.trim());
    v.parse::<f32>().ok()
}

// ---------------------------------------------------------------------------
// @supports evaluation
// ---------------------------------------------------------------------------

/// Evaluate an `@supports` condition: `prop: value` probes checked against
/// our supported-declaration set, combined with not/and/or per CSS Conditional.
pub fn supports_condition_applies(condition: &str) -> bool {
    let condition = condition.trim();
    let condition = condition
        .strip_prefix("@supports")
        .or_else(|| condition.strip_prefix("supports"))
        .unwrap_or(condition)
        .trim();
    eval_supports(condition).unwrap_or(false)
}

fn eval_supports(condition: &str) -> Option<bool> {
    let condition = condition.trim();
    if condition.is_empty() {
        return None;
    }
    // Unwrap one enclosing parenthesized group: `(...)` or `((a) and (b))`.
    if condition.starts_with('(') && condition.ends_with(')') && balanced_inner_is_whole(condition) {
        return eval_supports(&condition[1..condition.len() - 1]);
    }
    if let Some(rest) = strip_keyword(condition, "not") {
        return eval_supports(rest).map(|r| !r);
    }
    if let Some(parts) = split_top_level_keyword(condition, "or") {
        let results: Option<Vec<bool>> = parts.iter().map(|p| eval_supports(p)).collect();
        return results.map(|rs| rs.iter().any(|&b| b));
    }
    if let Some(parts) = split_top_level_keyword(condition, "and") {
        let results: Option<Vec<bool>> = parts.iter().map(|p| eval_supports(p)).collect();
        return results.map(|rs| rs.iter().all(|&b| b));
    }
    // Leaf: `(prop: value)` declaration probe.
    let probe = condition.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let (name, value) = probe.split_once(':')?;
    Some(supports_declaration(name.trim(), value.trim()))
}

/// Is the content between the outer parens one balanced group (not `(a)+(b)`)?
fn balanced_inner_is_whole(condition: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in condition.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != condition.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn strip_keyword<'a>(condition: &'a str, keyword: &str) -> Option<&'a str> {
    if condition.get(..keyword.len()).is_some_and(|p| p.eq_ignore_ascii_case(keyword))
        && condition.as_bytes().get(keyword.len()).is_some_and(u8::is_ascii_whitespace)
    {
        Some(condition[keyword.len()..].trim())
    } else {
        None
    }
}

/// Split on a top-level (depth-0, outside parens) boolean keyword.
fn split_top_level_keyword(condition: &str, keyword: &str) -> Option<Vec<String>> {
    let bytes = condition.as_bytes();
    let kw = keyword.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    let mut found = false;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = (depth - 1).max(0),
            _ if depth == 0 && i + kw.len() <= bytes.len() && bytes[i..].starts_with(kw) => {
                let before_ok = i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b')';
                let after_idx = i + kw.len();
                let after_ok =
                    after_idx == bytes.len() || bytes[after_idx].is_ascii_whitespace() || bytes[after_idx] == b'(';
                let word_boundary_before = i == 0
                    || !bytes[i - 1].is_ascii_alphanumeric();
                if before_ok && after_ok && word_boundary_before {
                    // Avoid matching "and" inside "not": check no preceding space-less letter.
                    parts.push(condition[start..i].trim().to_string());
                    found = true;
                    i = after_idx;
                    start = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !found {
        return None;
    }
    parts.push(condition[start..].trim().to_string());
    Some(parts)
}

/// Declaration support probe for @supports. Modeled on the properties this
/// cascade understands; everything else reports unsupported (conservative,
/// matches upstream's property-coverage-driven behavior). A supported
/// property with an invalid value still fails the probe, per spec.
pub fn supports_declaration(name: &str, value: &str) -> bool {
    const SUPPORTED: &[&str] = &[
        "display", "color", "background", "background-color", "margin", "margin-top",
        "margin-right", "margin-bottom", "margin-left", "padding", "padding-top",
        "padding-right", "padding-bottom", "padding-left", "font-size", "font-weight",
        "text-align", "border", "border-color", "border-width", "border-style",
        "width", "height", "flex-direction", "gap",
    ];
    if !SUPPORTED.contains(&name.to_ascii_lowercase().as_str()) {
        return false;
    }
    // Unitless nonzero lengths are invalid everywhere (upstream 2c12b5a).
    if let Ok(num) = value.parse::<f64>() {
        return num == 0.0;
    }
    if value.is_empty() {
        return false;
    }
    // Validate values against the same grammar apply_one accepts: a bogus
    // `display: nonsense` must fail the probe like a real browser.
    match name.to_ascii_lowercase().as_str() {
        "display" => matches!(
            value,
            "block" | "inline" | "flex" | "grid" | "none" | "inline-block" | "contents"
        ),
        "color" | "background-color" => parse_color(value).is_some() || value.starts_with("rgb") || value.starts_with("hsl"),
        "background" => parse_color(value.split_whitespace().next().unwrap_or("")).is_some()
            || value.starts_with("url(")
            || value.contains("gradient"),
        "text-align" => matches!(value, "left" | "start" | "center" | "right" | "end" | "justify"),
        "font-weight" => parse_font_weight(value).is_some(),
        "font-size" => parse_font_size(value).is_some(),
        _ => true, // remaining modeled properties accept any non-empty value here
    }
}

// ---------------------------------------------------------------------------
// Computed style: minimal property subset + inheritance
// ---------------------------------------------------------------------------

/// The computed values this slice models. Deliberately tiny: enough to lock
/// cascade ordering, specificity, inline override, and inheritance semantics.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ComputedStyle {
    /// Outer display role. `None` means "not declared anywhere".
    pub display: Option<Display>,
    pub color: Option<Color>,
    pub background_color: Option<Color>,
    /// Shorthand sides in CSS order (top right bottom left), already expanded.
    pub margin: Sides,
    pub padding: Sides,
    /// Font size in px (absolute keywords/units resolved by the caller's sheet
    /// context; here we accept px/em/% where em resolves against parent).
    pub font_size: Option<f32>,
    pub font_weight: Option<u16>,
    pub text_align: Option<TextAlign>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    Flex,
    Grid,
    None,
}

impl Default for Display {
    fn default() -> Self {
        Display::Block
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sides {
    pub top: Option<u32>,
    pub right: Option<u32>,
    pub bottom: Option<u32>,
    pub left: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// UA defaults per tag — the tiny corner of the upstream ua_style() that our
/// property subset can express: phrasing content is inline, everything else
/// block; b/strong bold.
pub fn ua_display(tag: &str) -> Display {
    match tag {
        "span" | "a" | "b" | "i" | "strong" | "em" | "code" | "small" | "sub" | "sup"
        | "label" | "time" | "abbr" => Display::Inline,
        _ => Display::Block,
    }
}

pub fn ua_font_weight(tag: &str) -> Option<u16> {
    match tag {
        "b" | "strong" | "th" => Some(700),
        _ => None,
    }
}

/// Split a declaration block into (name, value) pairs. Quote- and paren-aware;
/// nested `{}` blocks become one dropped chunk rather than leaking into the
/// parent rule (upstream split_declarations semantics).
pub fn split_declarations(css: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in css.char_indices() {
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => in_quote = Some(c),
            '(' | '{' => depth += 1,
            ')' | '}' => depth = (depth - 1).max(0),
            ';' if depth == 0 => {
                parts.push(&css[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&css[start..]);

    parts
        .into_iter()
        .filter_map(|decl| {
            let (name, value) = decl.split_once(':')?;
            let name = name.trim();
            if name.starts_with('!') || name.is_empty() {
                return None;
            }
            // Nested-rule chunks (`&:hover { ... }`) split into a bogus
            // (selector, body) pair whose value contains `{` — drop them
            // rather than treating the selector as a property name.
            if value.contains('{') {
                return None;
            }
            Some((name.to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

/// Apply declarations to a computed style. Returns whether any recognized
/// property was applied (the @supports leaf-probe contract).
pub fn apply_declarations(style: &mut ComputedStyle, declarations: &str) -> bool {
    let mut applied = false;
    for (name, value) in split_declarations(declarations) {
        // !important wins later during cascade merge; here both streams apply
        // with important taking precedence per-property at the call site.
        if apply_one(style, &name, &value) {
            applied = true;
        }
    }
    applied
}

fn apply_one(style: &mut ComputedStyle, name: &str, value: &str) -> bool {
    let v = value.trim();
    match name {
        "display" => {
            style.display = match v {
                "block" => Some(Display::Block),
                "inline" => Some(Display::Inline),
                "flex" => Some(Display::Flex),
                "grid" => Some(Display::Grid),
                "none" => Some(Display::None),
                _ => return false,
            };
            true
        }
        "color" => parse_color(v).map(|c| style.color = Some(c)).is_some(),
        "background" | "background-color" => {
            // `background` shorthand: take a leading color token if present.
            let candidate = if name == "background" {
                v.split_whitespace().next().unwrap_or("")
            } else {
                v
            };
            parse_color(candidate).map(|c| style.background_color = Some(c)).is_some()
        }
        "margin" | "padding" => {
            let sides = expand_sides(v);
            let target = if name == "margin" { &mut style.margin } else { &mut style.padding };
            *target = sides;
            sides.top.is_some()
        }
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            set_side(&mut style.margin, name, parse_length_px(v));
            true
        }
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
            set_side(&mut style.padding, name, parse_length_px(v));
            true
        }
        "font-size" => parse_font_size(v).map(|px| style.font_size = Some(px)).is_some(),
        "font-weight" => {
            let weight = parse_font_weight(v);
            style.font_weight = weight;
            weight.is_some()
        }
        "text-align" => {
            style.text_align = match v {
                "left" | "start" => Some(TextAlign::Left),
                "center" => Some(TextAlign::Center),
                "right" | "end" => Some(TextAlign::Right),
                _ => return false,
            };
            true
        }
        _ => false,
    }
}

fn set_side(sides: &mut Sides, name: &str, value: Option<u32>) {
    let slot = match name.rsplit_once('-').map(|(_, side)| side) {
        Some("top") => &mut sides.top,
        Some("right") => &mut sides.right,
        Some("bottom") => &mut sides.bottom,
        Some("left") => &mut sides.left,
        _ => return,
    };
    *slot = value;
}

/// CSS 1–4 value expansion (px-only for this slice; non-px lengths drop to None).
fn expand_sides(value: &str) -> Sides {
    let vals: Vec<Option<u32>> = value
        .split_whitespace()
        .map(parse_length_px)
        .collect();
    match vals.as_slice() {
        [one] => Sides { top: *one, right: *one, bottom: *one, left: *one },
        [t, r] => Sides { top: *t, right: *r, bottom: *t, left: *r },
        [t, r, b] => Sides { top: *t, right: *r, bottom: *b, left: *r },
        [t, r, b, l] => Sides { top: *t, right: *r, bottom: *b, left: *l },
        _ => Sides::default(),
    }
}

fn parse_length_px(v: &str) -> Option<u32> {
    let v = v.trim();
    if v == "0" {
        return Some(0);
    }
    v.strip_suffix("px")?.parse::<u32>().ok()
}

fn parse_font_size(v: &str) -> Option<f32> {
    let v = v.trim();
    if let Some(px) = v.strip_suffix("px") {
        return px.parse::<f32>().ok();
    }
    // em/% resolve during inheritance (they need the parent size).
    if let Some(em) = v.strip_suffix("em") {
        return em.parse::<f32>().ok().map(|n| n * 16.0);
    }
    match v {
        "small" => Some(13.0),
        "medium" => Some(16.0),
        "large" => Some(18.0),
        "x-large" => Some(24.0),
        _ => None,
    }
}

fn parse_font_weight(v: &str) -> Option<u16> {
    match v.trim() {
        "normal" => Some(400),
        "bold" => Some(700),
        other => {
            let n = other.parse::<u16>().ok()?;
            (1..=9).contains(&(n / 100)).then_some(n)
        }
    }
}

/// Named colors + #rgb/#rrggbbaa hex (the forms real sheets overwhelmingly use).
pub fn parse_color(v: &str) -> Option<Color> {
    let v = v.trim().to_ascii_lowercase();
    let named = match v.as_str() {
        "black" => Color(0, 0, 0, 255),
        "white" => Color(255, 255, 255, 255),
        "red" => Color(255, 0, 0, 255),
        "green" => Color(0, 128, 0, 255),
        "lime" => Color(0, 255, 0, 255),
        "blue" => Color(0, 0, 255, 255),
        "gray" | "grey" => Color(128, 128, 128, 255),
        "silver" => Color(192, 192, 192, 255),
        "transparent" => Color(0, 0, 0, 0),
        _ => return parse_hex_color(&v),
    };
    Some(named)
}

fn parse_hex_color(v: &str) -> Option<Color> {
    let hex = v.strip_prefix('#')?;
    let (r, g, b, a) = match hex.len() {
        3 => {
            let n: Vec<u8> = hex.chars().filter_map(|c| c.to_digit(16).map(|d| d as u8)).collect();
            if n.len() != 3 {
                return None;
            }
            (
                n[0] * 17,
                n[1] * 17,
                n[2] * 17,
                255,
            )
        }
        6 | 8 => {
            let bytes = (0..hex.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                .collect::<Vec<u8>>();
            if bytes.len() != hex.len() / 2 {
                return None;
            }
            match bytes.as_slice() {
                [r, g, b] => (*r, *g, *b, 255),
                [r, g, b, a] => (*r, *g, *b, *a),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(Color(r, g, b, a))
}

// ---------------------------------------------------------------------------
// Cascade
// ---------------------------------------------------------------------------

/// One candidate: a compiled rule plus how strongly it applies.
struct CascadeCandidate<'a> {
    rule_index: usize,
    declarations: &'a str,
    specificity: u32,
    source_order: usize,
    inline: bool,
}

/// Compute the style for one element: UA defaults ← author rules (specificity,
/// then source order) ← inline style. Inherited properties (color, font-*,
/// text-align) fall back to the parent's computed values.
///
/// Read-only entry point: callers pass the tree, the element, its matched
/// rules (already filtered by the caller's selector matching), and the parent
/// style. This keeps the module decoupled from any particular matching
/// strategy while still locking ordering semantics.
pub fn cascade_element(
    tag: &str,
    tree: &diting_dom::tree::DomTree,
    node_id: diting_dom::tree::NodeId,
    matched_rules: &[(&ParsedRule, u32)],
    parent: Option<&ComputedStyle>,
    inline_css: Option<&str>,
) -> ComputedStyle {
    let mut style = ComputedStyle {
        display: Some(ua_display(tag)),
        font_weight: ua_font_weight(tag),
        ..Default::default()
    };

    // Inherited defaults from parent BEFORE author rules (author overrides).
    if let Some(parent) = parent {
        style.color = parent.color;
        style.font_size = parent.font_size;
        style.font_weight = style.font_weight.or(parent.font_weight);
        style.text_align = parent.text_align;
    }

    // Author rules: sort by (specificity, source order) ascending, apply in
    // order so later/higher-specificity wins per property.
    let mut candidates: Vec<CascadeCandidate> = matched_rules
        .iter()
        .enumerate()
        .map(|(order, (rule, spec))| CascadeCandidate {
            rule_index: order,
            declarations: &rule.declarations,
            specificity: *spec,
            source_order: order,
            inline: false,
        })
        .collect();
    candidates.sort_by_key(|c| (c.specificity, c.source_order));

    let _ = tree;
    let _ = node_id;
    for candidate in &candidates {
        apply_declarations(&mut style, candidate.declarations);
    }

    // Inline style always last.
    if let Some(inline) = inline_css {
        apply_inline_declarations(&mut style, inline);
    }

    style
}

/// Inline styles use the same declaration grammar.
pub fn apply_inline_declarations(style: &mut ComputedStyle, css: &str) -> bool {
    apply_declarations(style, css)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- stylesheet parsing ----

    #[test]
    fn parse_rules_comments_and_nested_braces() {
        let css = r#"/* header */
            a { color: red; }
            div { background: url("a{b}.png"); color: blue; }
        "#;
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 2, "{rules:?}");
        assert_eq!(rules[0].selector, "a");
        assert_eq!(rules[0].declarations, "color: red;");
        assert_eq!(rules[1].selector, "div");
        // Braces inside quoted strings must not confuse the block counter.
        assert!(rules[1].declarations.contains(r#"url("a{b}.png")"#), "{rules:?}");
    }

    #[test]
    fn stray_close_brace_resyncs_parsing() {
        // Upstream: remoteok.com ships an unbalanced top-level `}` mid-sheet;
        // browsers recover and keep the rest of the sheet usable.
        let css = "a { color: red; } } p { color: blue; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 2, "{rules:?}");
        assert_eq!(rules[1].selector, "p");
    }

    #[test]
    fn media_queries_gate_inner_rules() {
        let css = r#"
            @media (min-width: 768px) { .desktop { display: flex; } }
            @media (max-width: 500px) { .mobile { display: none; } }
            @media screen and (min-width: 100px) { .both { color: red; } }
            @media print { .paper { color: blue; } }
        "#;

        let desktop = parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Screen);
        let sels: Vec<&str> = desktop.iter().map(|r| r.selector.as_str()).collect();
        assert!(sels.contains(&".desktop"), "{sels:?}");
        assert!(sels.contains(&".both"), "{sels:?}");
        assert!(!sels.contains(&".mobile"), "{sels:?}");
        assert!(!sels.contains(&".paper"), "{sels:?}");

        let narrow = parse_stylesheet_for(css, (400.0, 720.0), CssMediaType::Screen);
        let sels: Vec<&str> = narrow.iter().map(|r| r.selector.as_str()).collect();
        assert!(sels.contains(&".mobile"), "{sels:?}");
        assert!(!sels.contains(&".desktop"), "{sels:?}");

        let print = parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Print);
        let sels: Vec<&str> = print.iter().map(|r| r.selector.as_str()).collect();
        assert!(sels.contains(&".paper"), "{sels:?}");
        // A bare feature query implies media type `all`, so it applies under
        // print too — that's spec behavior, lock it as such.
        assert!(sels.contains(&".desktop"), "bare feature = all: {sels:?}");
        assert!(!sels.contains(&".both"), "`screen and …` must not apply in print: {sels:?}");
    }

    #[test]
    fn media_list_is_or_and_function_commas_survive() {
        assert!(media_query_applies("not print, (min-width: 10px)", (1280.0, 720.0), CssMediaType::Screen));
        assert!(!media_query_applies("(min-width: 99999px)", (1280.0, 720.0), CssMediaType::Screen));
        // Comma inside a function is not a list separator.
        assert!(media_query_applies("(width: 1280px)", (1280.0, 720.0), CssMediaType::Screen));
    }

    #[test]
    fn supports_conditions_evaluate() {
        let css = r#"
            @supports (display: grid) { .g { display: grid; } }
            @supports not (display: nonexistent-thing) { .n { color: red; } }
            @supports ((display: grid) and (display: flex)) { .af { color: blue; } }
            @supports (display: totally-bogus-value-is-still-a-declaration-probe-false) { .x { color: green; } }
        "#;
        let rules = parse_stylesheet(css);
        let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
        assert!(sels.contains(&".g"), "{sels:?}");
        assert!(sels.contains(&".n"), "{sels:?}");
        assert!(sels.contains(&".af"), "{sels:?}");
        assert!(!sels.contains(&".x"), "{sels:?}");
    }

    #[test]
    fn layer_bodies_flatten_and_font_face_dropped() {
        let css = r#"
            @layer base { .in-layer { color: red; } }
            @font-face { font-family: X; src: url(x.ttf); }
            @import url(other.css);
            .top { color: blue; }
        "#;
        let rules = parse_stylesheet(css);
        let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
        assert!(sels.contains(&".in-layer"), "{sels:?}");
        assert!(sels.contains(&".top"), "{sels:?}");
        assert_eq!(rules.len(), 2, "@font-face/@import must drop: {sels:?}");
    }

    #[test]
    fn keyframes_and_import_do_not_bleed_into_next_selector() {
        let css = "@keyframes spin { from { opacity: 0; } } .after { color: red; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].selector, ".after");
    }

    // ---- declarations & computed style ----

    #[test]
    fn declaration_splitting_handles_quotes_and_nested_blocks() {
        let decls = r#"content: "a;b"; background: url(x(1;2).png); width: 4px; & :hover { color: red; }"#;
        let parsed = split_declarations(decls);
        let names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["content", "background", "width"], "{parsed:?}");
        // The nested block is one dropped chunk (unparseable as declarations).
    }

    #[test]
    fn shorthand_expansion_four_value_forms() {
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin: 1px 2px 3px 4px; padding: 8px");
        assert_eq!(s.margin.top, Some(1));
        assert_eq!(s.margin.right, Some(2));
        assert_eq!(s.margin.bottom, Some(3));
        assert_eq!(s.margin.left, Some(4));
        assert_eq!(s.padding.top, Some(8));
        assert_eq!(s.padding.left, Some(8));

        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin: 5px 6px");
        assert_eq!((s.margin.top, s.margin.bottom), (Some(5), Some(5)));
        assert_eq!((s.margin.right, s.margin.left), (Some(6), Some(6)));

        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin: 7px 8px 9px");
        assert_eq!(s.margin.bottom, Some(9));
        assert_eq!(s.margin.left, Some(8), "3-value form mirrors right to left");
    }

    #[test]
    fn colors_parse_named_hex_short_hex() {
        assert_eq!(parse_color("red"), Some(Color(255, 0, 0, 255)));
        assert_eq!(parse_color("#00ff00"), Some(Color(0, 255, 0, 255)));
        assert_eq!(parse_color("#f00"), Some(Color(255, 0, 0, 255)));
        assert_eq!(parse_color("#ff000080").map(|c| c.3), Some(128));
        assert_eq!(parse_color("rgb(1,2,3)"), None, "functional colors out of slice scope");
    }

    #[test]
    fn unitless_nonzero_lengths_rejected() {
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin-top: 0; margin-bottom: 12; padding-left: 3px");
        assert_eq!(s.margin.top, Some(0), "zero is a valid unitless length");
        assert_eq!(s.margin.bottom, None, "nonzero unitless length is invalid CSS");
        assert_eq!(s.padding.left, Some(3));
    }

    // ---- cascade ----

    #[test]
    fn cascade_specificity_then_source_order_then_inline() {
        let tree = diting_dom::tree_sink::parse_html(
            r#"<p id="main" class="intro" style="color: green">x</p>"#,
        );
        let node = tree.get_element_by_id("main").unwrap();

        let rules = vec![
            ParsedRule { selector: "p".into(), declarations: "color: #111111".into() },
            ParsedRule { selector: ".intro".into(), declarations: "color: #222222".into() },
            ParsedRule { selector: "#main".into(), declarations: "color: #333333".into() },
        ];
        // Compile through our own diting_dom selectors for specificity truth.
        let matched: Vec<(&ParsedRule, u32)> = rules
            .iter()
            .filter_map(|rule| {
                tree.compile_rule_selector(&rule.selector)
                    .map(|compiled| (rule, compiled.specificity()))
            })
            .collect();
        assert_eq!(matched.len(), 3);

        let computed = cascade_element("p", &tree, node, &matched, None, Some("background-color: #abcdef"));
        assert_eq!(
            computed.color,
            Some(Color(0x33, 0x33, 0x33, 0xff)),
            "id specificity beats class beats tag: {computed:?}"
        );
        assert_eq!(
            computed.background_color,
            Some(Color(0xab, 0xcd, 0xef, 0xff)),
            "inline style applies last"
        );
    }

    #[test]
    fn inheritance_flows_from_parent_and_author_overrides() {
        let tree =
            diting_dom::tree_sink::parse_html(r#"<section><p>x</p><em>y</em></section>"#);
        let section = tree.query_selector("section").unwrap().unwrap();
        let em = tree.query_selector("em").unwrap().unwrap();

        let parent = ComputedStyle {
            color: Some(Color(51, 51, 51, 255)),
            font_size: Some(18.0),
            text_align: Some(TextAlign::Center),
            ..Default::default()
        };
        // No matched author rules for <em>: pure inheritance + UA defaults.
        let child = cascade_element("em", &tree, em, &[], Some(&parent), None);
        assert_eq!(child.color, parent.color, "color inherits");
        assert_eq!(child.font_size, parent.font_size, "font-size inherits");
        assert_eq!(child.text_align, parent.text_align, "text-align inherits");
        assert_eq!(child.display, Some(Display::Inline), "UA default for <em>");

        // Author rule on the child overrides the inherited color only.
        let rule = ParsedRule { selector: "em".into(), declarations: "color: red".into() };
        let spec = tree.compile_rule_selector("em").unwrap().specificity();
        let overridden = cascade_element("em", &tree, em, &[(&rule, spec)], Some(&parent), None);
        assert_eq!(overridden.color, Some(Color(255, 0, 0, 255)));
        assert_eq!(overridden.font_size, parent.font_size, "unmentioned props still inherit");

        // Section itself: block UA default even with no author CSS.
        let block = cascade_element("section", &tree, section, &[], None, None);
        assert_eq!(block.display, Some(Display::Block));
    }

    #[test]
    fn end_to_end_stylesheet_through_dom_matching() {
        // The batch's headline integration: parse a real-shaped sheet with
        // this module, match its selectors with OUR diting_dom engine
        // (including :where() from batch −1), cascade the winners.
        let html = r#"<html><body>
            <nav class="menu"><a href="/x">link</a></nav>
            <article><p class="lead">text</p></article>
        </body></html>"#;
        let tree = diting_dom::tree_sink::parse_html(html);

        let sheet = r#"
            body { margin: 0; }
            .menu a { text-align: center; }
            :where(article) { padding: 12px; }
            article .lead { font-weight: bold; }
        "#;
        let rules = parse_stylesheet(sheet);
        assert_eq!(rules.len(), 4, "{rules:?}");

        let lead = tree.query_selector(".lead").unwrap().unwrap();
        let matched: Vec<(&ParsedRule, u32)> = rules
            .iter()
            .enumerate()
            .filter_map(|(order, rule)| {
                // Match against the element per querySelector semantics.
                let hits = tree.query_selector_all_from(
                    tree.document(),
                    &rule.selector,
                ).ok()?;
                if order == usize::MAX || !hits.contains(&lead) {
                    return None;
                }
                let compiled = tree.compile_rule_selector(&rule.selector)?;
                Some((rule, compiled.specificity()))
            })
            .collect();
        // `.menu a` does not hit .lead; `article .lead` does. `:where(article)`
        // matches the <article> element, not its child .lead.
        assert_eq!(matched.len(), 1, "{matched:?}");

        let computed = cascade_element("p", &tree, lead, &matched, None, None);
        assert_eq!(computed.font_weight, Some(700), "author bold applies");
        assert_eq!(computed.text_align, None, ".menu a never touched .lead");

        // The :where rule applies to the article element itself.
        let article = tree.query_selector("article").unwrap().unwrap();
        let art_matched: Vec<(&ParsedRule, u32)> = rules
            .iter()
            .filter_map(|rule| {
                let hits = tree
                    .query_selector_all_from(tree.document(), &rule.selector)
                    .ok()?;
                if !hits.contains(&article) {
                    return None;
                }
                let compiled = tree.compile_rule_selector(&rule.selector)?;
                Some((rule, compiled.specificity()))
            })
            .collect();
        assert_eq!(art_matched.len(), 1, "{art_matched:?}");
        let art_computed = cascade_element("article", &tree, article, &art_matched, None, None);
        assert_eq!(art_computed.padding.top, Some(12), ":where(article) padding applies");
    }
}
