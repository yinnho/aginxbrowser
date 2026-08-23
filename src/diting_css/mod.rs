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
        "width", "height", "flex-direction", "gap", "overflow",
        "object-fit", "object-position", "z-index", "border-radius",
        "float", "clear",
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
        "object-fit" => matches!(
            value,
            "fill" | "contain" | "cover" | "none" | "scale-down"
        ),
        "object-position" => {
            let part = |s: &str| {
                s.ends_with('%') && s[..s.len() - 1].parse::<f32>().is_ok()
                    || s.ends_with("px") && s[..s.len() - 2].parse::<f32>().is_ok()
            };
            value
                .split_whitespace()
                .all(|s| matches!(s, "left" | "top" | "center" | "right" | "bottom") || part(s))
        }
        "z-index" => value == "auto" || value.parse::<i32>().is_ok(),
        "float" => matches!(value, "left" | "right" | "none"),
        "clear" => matches!(value, "left" | "right" | "both" | "none"),
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
    /// Uniform border (batch 4b): per-side widths, ONE color and ONE style
    /// for all sides — per-side colors/styles are later batches. A border
    /// lays out (content inset) and paints only when `border_style` is set
    /// to a line style; `none`/`hidden` (or unset, the CSS initial) mean no
    /// border. Widths accept the keywords thin/medium/thick (1/3/5px).
    pub border_width: Sides,
    pub border_color: Option<Color>,
    pub border_style: Option<BorderStyle>,
    /// Authored box size (non-inherited): px (em/rem resolved) or %.
    pub width: Option<Length>,
    pub height: Option<Length>,
    /// Font size in px (absolute keywords/units resolved by the caller's sheet
    /// context; here we accept px/em/% where em resolves against parent).
    pub font_size: Option<f32>,
    pub font_weight: Option<u16>,
    pub text_align: Option<TextAlign>,
    /// Overflow clipping (batch 4c), uniform for both axes. Any non-visible
    /// value clips descendants' paint to the padding box; per-axis
    /// overflow-x/y is a later batch.
    pub overflow: Option<Overflow>,
    // --- flex/grid pass-through (batch 2c): px/fr-only, non-inherited ---
    pub flex_direction: Option<FlexDirection>,
    pub flex_wrap: Option<FlexWrapMode>,
    /// Real flex/grid alignment — separate from `text_align`, which only
    /// addresses inline content (upstream keeps them distinct too).
    pub justify_content: Option<JustifyMode>,
    pub align_items: Option<AlignMode>,
    pub flex_grow: Option<f32>,
    pub flex_shrink: Option<f32>,
    /// px only (`auto` stays None — the initial value).
    pub flex_basis: Option<f32>,
    pub column_gap: Option<f32>,
    pub row_gap: Option<f32>,
    /// Track list (px / fr / auto). `None` = not declared.
    pub grid_template_columns: Option<Vec<GridTrack>>,
    pub grid_template_rows: Option<Vec<GridTrack>>,
    // --- positioning + size clamps (batch 2d), non-inherited ---
    pub position: Option<PositionMode>,
    /// Inset offsets (top/right/bottom/left): px or %.
    pub top: Option<Length>,
    pub right: Option<Length>,
    pub bottom: Option<Length>,
    pub left: Option<Length>,
    pub min_width: Option<Length>,
    pub max_width: Option<Length>,
    pub min_height: Option<Length>,
    pub max_height: Option<Length>,
    /// Declared aspect ratio (width/height); `auto` stays None.
    pub aspect_ratio: Option<f32>,
    /// Replaced-content fit (batch 5c), non-inherited; initial Fill.
    pub object_fit: Option<ObjectFit>,
    /// (x, y) object-position parts, non-inherited; initial 50%/50%.
    pub object_position: Option<(ObjectPositionPart, ObjectPositionPart)>,
    /// `z-index` (batch 6a), non-inherited; None = auto. Only meaningful on
    /// positioned elements (flex/grid-item support is a later batch).
    pub z_index: Option<i32>,
    /// Uniform circular `border-radius` (batch 6b): ONE length/percentage
    /// applied to all four corners (the 1-value syntax — by far the most
    /// common form). Percentages resolve against the box width. Per-corner
    /// and elliptical (`rx ry`) radii are a later batch.
    pub border_radius: Option<Length>,
    /// Per-corner radii (batch 7c), CSS corner order (top-left, top-right,
    /// bottom-right, bottom-left), each an (rx, ry) pair — rx resolves
    /// against the box width, ry against its height (the elliptical form).
    /// `None` when no border-radius is declared; the uniform 1-value case
    /// fills all four pairs identically.
    pub corner_radii: Option<[(Length, Length); 4]>,
    /// `float` (batch 8a), non-inherited; None = none (the initial value).
    pub float_side: Option<FloatSide>,
    /// `clear`, non-inherited; None = none.
    pub clear_side: Option<ClearSide>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    Static,
    Relative,
    Absolute,
    Fixed,
}

/// `float` keyword (batch 8a). `None` (the initial value) stays None on
/// [`ComputedStyle`] — only floated boxes carry a side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSide {
    Left,
    Right,
}

/// `clear` keyword: which side(s)' floats an element must move below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearSide {
    Left,
    Right,
    Both,
}

/// Border line style. Only `Solid` paints faithfully in this slice; the
/// patterned styles occupy the same layout space but paint as solid
/// (documented approximation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
    Solid,
    Dashed,
    Dotted,
    Double,
}

/// What a border-style token means: not a style keyword at all, an explicit
/// no-border, or a line style.
enum BorderStyleKw {
    NotAStyle,
    NoBorder,
    Line(BorderStyle),
}

/// Overflow behavior. Paint-side only in this slice: non-visible values
/// clip descendants to the padding box. (CSS also makes clipping boxes
/// establish a BFC — margin-collapse/float containment is a later batch.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

/// `object-fit` (batch 5c): how replaced content maps into its box. The
/// math mirrors blitz-paint/src/sizing.rs `compute_object_fit` — Fill
/// stretches, Contain/Cover pick min/max of the per-axis scale ratios,
/// None uses the natural size, ScaleDown is Contain unless the natural
/// size is already smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFit {
    Fill,
    Contain,
    Cover,
    None,
    ScaleDown,
}

impl Default for ObjectFit {
    fn default() -> Self {
        ObjectFit::Fill
    }
}

/// One axis of `object-position` (batch 5c): a percentage of the free
/// space (box − painted object) or an absolute px offset. The initial
/// value is 50% both axes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObjectPositionPart {
    Percent(f32),
    Px(f32),
}

fn border_style_kw(v: &str) -> BorderStyleKw {
    match v {
        "solid" => BorderStyleKw::Line(BorderStyle::Solid),
        "dashed" => BorderStyleKw::Line(BorderStyle::Dashed),
        "dotted" => BorderStyleKw::Line(BorderStyle::Dotted),
        "double" => BorderStyleKw::Line(BorderStyle::Double),
        "none" | "hidden" => BorderStyleKw::NoBorder,
        _ => BorderStyleKw::NotAStyle,
    }
}

/// One grid track sizing. `1fr` / `100px` / `auto` — minmax() and repeat()
/// are later batches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridTrack {
    Fr(f32),
    Px(f32),
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrapMode {
    NoWrap,
    Wrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyMode {
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignMode {
    Stretch,
    FlexStart,
    Center,
    FlexEnd,
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

/// A computed length (batch 2e). `Px` is fully resolved — em/rem were folded
/// in during the cascade (em against the element's own font-size, rem
/// against the root font-size, matching CSS computed-value semantics).
/// `Percent` stays symbolic here and resolves against the containing block
/// in the layout engine (taffy's percent semantics match CSS: margins and
/// paddings against CB width, insets per-axis).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Percent(f32),
}

/// Declaration-level length: em/rem can't resolve until the font context is
/// known, so parsing keeps them symbolic for the cascade to fold in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CssLength {
    Px(f32),
    Em(f32),
    Rem(f32),
    Percent(f32),
}

/// The initial / default root font size (CSS `medium`).
pub const DEFAULT_ROOT_FONT_SIZE: f32 = 16.0;

/// Font context a length resolves against. `own` is this element's computed
/// font-size (font-size itself resolves em/% against the PARENT, per spec).
#[derive(Debug, Clone, Copy)]
pub struct FontCtx {
    pub own: f32,
    pub root: f32,
}

impl Default for FontCtx {
    fn default() -> Self {
        FontCtx { own: DEFAULT_ROOT_FONT_SIZE, root: DEFAULT_ROOT_FONT_SIZE }
    }
}

fn resolve_len(l: CssLength, fonts: &FontCtx) -> Length {
    match l {
        CssLength::Px(x) => Length::Px(x),
        CssLength::Em(n) => Length::Px(n * fonts.own),
        CssLength::Rem(n) => Length::Px(n * fonts.root),
        CssLength::Percent(p) => Length::Percent(p),
    }
}

/// px / em / rem / % (unitless `0` is a legal length; `rem` before `em`
/// because "rem" also ends in "em").
fn parse_css_length(v: &str) -> Option<CssLength> {
    let v = v.trim();
    if v == "0" {
        return Some(CssLength::Px(0.0));
    }
    if let Some(p) = v.strip_suffix('%') {
        return p.parse::<f32>().ok().map(CssLength::Percent);
    }
    if let Some(r) = v.strip_suffix("rem") {
        return r.parse::<f32>().ok().map(CssLength::Rem);
    }
    if let Some(e) = v.strip_suffix("em") {
        return e.parse::<f32>().ok().map(CssLength::Em);
    }
    if let Some(px) = v.strip_suffix("px") {
        return px.parse::<f32>().ok().map(CssLength::Px);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sides {
    pub top: Option<Length>,
    pub right: Option<Length>,
    pub bottom: Option<Length>,
    pub left: Option<Length>,
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

/// UA `border: 2px inset` on iframes (batch 7a) — the classic embedded
/// document frame, same rule blitz's assets/default.css carries (and every
/// real browser). The border lays out (600 attr width → 604 border box)
/// and paints as our uniform solid band; the `inset` style distinction is
/// a later batch.
pub fn ua_border(tag: &str) -> Option<(f32, BorderStyle)> {
    match tag {
        "iframe" => Some((2.0, BorderStyle::Solid)),
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
/// property was applied (the @supports leaf-probe contract). Uses the
/// default font context — callers that know the element's font-size (the
/// cascade) use `apply_declarations_with` so em/rem resolve correctly.
pub fn apply_declarations(style: &mut ComputedStyle, declarations: &str) -> bool {
    apply_declarations_with(style, declarations, &FontCtx::default())
}

pub fn apply_declarations_with(
    style: &mut ComputedStyle,
    declarations: &str,
    fonts: &FontCtx,
) -> bool {
    let mut applied = false;
    for (name, value) in split_declarations(declarations) {
        // !important wins later during cascade merge; here both streams apply
        // with important taking precedence per-property at the call site.
        if apply_one(style, &name, &value, fonts) {
            applied = true;
        }
    }
    applied
}

fn apply_one(style: &mut ComputedStyle, name: &str, value: &str, fonts: &FontCtx) -> bool {
    let v = value.trim();
    // Length arm helper: parse + fold em/rem against the font context.
    let len = |val: &str| parse_css_length(val).map(|l| resolve_len(l, fonts));
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
            let sides = expand_sides(v, fonts);
            let target = if name == "margin" { &mut style.margin } else { &mut style.padding };
            *target = sides;
            sides.top.is_some()
        }
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            set_side(&mut style.margin, name, len(v));
            true
        }
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
            set_side(&mut style.padding, name, len(v));
            true
        }
        "border" => {
            // <border-width> || <border-style> || <color>, any order. Parse
            // everything first and only then apply: an unparseable token
            // drops the WHOLE declaration (CSS invalid-declaration recovery),
            // with no partial side effects.
            let mut width: Option<Length> = None;
            let mut line: Option<Option<BorderStyle>> = None; // Some(None) = explicit none
            let mut color: Option<Color> = None;
            for tok in v.split_whitespace() {
                let w = len(tok).or_else(|| match tok {
                    "thin" => Some(Length::Px(1.0)),
                    "medium" => Some(Length::Px(3.0)),
                    "thick" => Some(Length::Px(5.0)),
                    _ => None,
                });
                if let Some(l) = w {
                    width = Some(l);
                    continue;
                }
                match border_style_kw(tok) {
                    BorderStyleKw::Line(bs) => line = Some(Some(bs)),
                    BorderStyleKw::NoBorder => line = Some(None),
                    BorderStyleKw::NotAStyle => match parse_color(tok) {
                        Some(c) => color = Some(c),
                        None => return false,
                    },
                }
            }
            if let Some(l) = width {
                style.border_width =
                    Sides { top: Some(l), right: Some(l), bottom: Some(l), left: Some(l) };
            }
            match line {
                Some(Some(bs)) => style.border_style = Some(bs),
                Some(None) => style.border_style = None,
                None => {}
            }
            if let Some(c) = color {
                style.border_color = Some(c);
            }
            width.is_some() || line.is_some() || color.is_some()
        }
        "border-width" => {
            let sides = expand_sides(v, fonts);
            style.border_width = sides;
            sides.top.is_some()
        }
        "border-style" => {
            // Uniform keyword only (per-side style lists are later batches).
            match border_style_kw(v) {
                BorderStyleKw::Line(bs) => {
                    style.border_style = Some(bs);
                    true
                }
                BorderStyleKw::NoBorder => {
                    style.border_style = None;
                    true
                }
                BorderStyleKw::NotAStyle => false,
            }
        }
        "border-color" => parse_color(v).map(|c| style.border_color = Some(c)).is_some(),
        "font-size" => {
            // Resolved by the cascade's font-size pre-pass (em/% need the
            // PARENT font-size); here only px/keywords can apply directly.
            parse_font_size_len(v).map(|l| match l {
                CssLength::Px(px) => style.font_size = Some(px),
                _ => {} // em/rem/% handled by the pre-pass, not this arm
            }).is_some()
        }
        "width" => len(v).map(|l| style.width = Some(l)).is_some(),
        "height" => len(v).map(|l| style.height = Some(l)).is_some(),
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
        "overflow" => {
            style.overflow = match v {
                "visible" => Some(Overflow::Visible),
                "hidden" => Some(Overflow::Hidden),
                "clip" => Some(Overflow::Clip),
                "scroll" => Some(Overflow::Scroll),
                "auto" => Some(Overflow::Auto),
                _ => return false,
            };
            true
        }
        "object-fit" => {
            style.object_fit = match v {
                "fill" => Some(ObjectFit::Fill),
                "contain" => Some(ObjectFit::Contain),
                "cover" => Some(ObjectFit::Cover),
                "none" => Some(ObjectFit::None),
                "scale-down" => Some(ObjectFit::ScaleDown),
                _ => return false,
            };
            true
        }
        "z-index" => {
            // `auto` (the initial value) stays None.
            style.z_index = match v {
                "auto" => None,
                _ => match v.parse::<i32>() {
                    n @ Ok(_) => n.ok(),
                    Err(_) => return false,
                },
            };
            true
        }
        "border-radius" => {
            // CSS syntax: 1-4 horizontal radii, optionally `/` plus 1-4
            // vertical radii (the elliptical form). Corners fill in CSS
            // order (TL TR BR BL) from however many values are given.
            let (horiz, vert) = match v.split_once('/') {
                Some((h, s)) => (h, Some(s)),
                None => (v, None),
            };
            let parse_list = |s: &str| -> Option<Vec<Length>> {
                let vals: Vec<Option<Length>> =
                    s.split_whitespace().map(|t| len(t)).collect();
                if vals.iter().any(|v| v.is_none()) || vals.is_empty() || vals.len() > 4 {
                    return None;
                }
                Some(vals.into_iter().flatten().collect())
            };
            let Some(h) = parse_list(horiz) else { return false };
            let vlist = match vert {
                Some(s) => match parse_list(s) {
                    Some(v) if v.len() == h.len() => v,
                    _ => return false,
                },
                None => h.clone(),
            };
            // Expand n values to four corners per the CSS mirror rule.
            let expand = |vals: &[Length]| -> [Length; 4] {
                match vals {
                    [a] => [*a, *a, *a, *a],
                    [a, b] => [*a, *b, *a, *b],
                    [a, b, c] => [*a, *b, *c, *b],
                    vals => [vals[0], vals[1], vals[2], vals[3]],
                }
            };
            let hc = expand(&h);
            let vc = expand(&vlist);
            style.corner_radii = Some([
                (hc[0], vc[0]),
                (hc[1], vc[1]),
                (hc[2], vc[2]),
                (hc[3], vc[3]),
            ]);
            // Keep the uniform shortcut in sync for the common 1-value case.
            if matches!(&hc, [a, b, c, d] if a == b && b == c && c == d)
                && hc[0] == vc[0]
            {
                style.border_radius = Some(hc[0]);
            } else {
                style.border_radius = None;
            }
            true
        }
        "object-position" => {
            let part = |s: &str| -> Option<ObjectPositionPart> {
                match s {
                    // Keyword positions are their percentage equivalents.
                    "left" | "top" => Some(ObjectPositionPart::Percent(0.0)),
                    "center" => Some(ObjectPositionPart::Percent(50.0)),
                    "right" | "bottom" => Some(ObjectPositionPart::Percent(100.0)),
                    _ => {
                        let num = s.strip_suffix('%').map(|n| n.parse::<f32>().ok()).flatten();
                        if let Some(p) = num {
                            return Some(ObjectPositionPart::Percent(p));
                        }
                        parse_px_f32(s).map(ObjectPositionPart::Px)
                    }
                }
            };
            let vals: Vec<ObjectPositionPart> = v.split_whitespace().filter_map(part).collect();
            match vals.as_slice() {
                [x] => {
                    style.object_position = Some((*x, ObjectPositionPart::Percent(50.0)));
                }
                [x, y] => {
                    style.object_position = Some((*x, *y));
                }
                _ => return false,
            }
            true
        }
        "flex-direction" => {
            style.flex_direction = match v {
                "row" => Some(FlexDirection::Row),
                "row-reverse" => Some(FlexDirection::RowReverse),
                "column" => Some(FlexDirection::Column),
                "column-reverse" => Some(FlexDirection::ColumnReverse),
                _ => return false,
            };
            true
        }
        "flex-wrap" => {
            style.flex_wrap = match v {
                "nowrap" => Some(FlexWrapMode::NoWrap),
                "wrap" => Some(FlexWrapMode::Wrap),
                _ => return false,
            };
            true
        }
        "justify-content" => {
            style.justify_content = match v {
                "flex-start" | "start" => Some(JustifyMode::FlexStart),
                "center" => Some(JustifyMode::Center),
                "flex-end" | "end" => Some(JustifyMode::FlexEnd),
                "space-between" => Some(JustifyMode::SpaceBetween),
                "space-around" => Some(JustifyMode::SpaceAround),
                "space-evenly" => Some(JustifyMode::SpaceEvenly),
                _ => return false,
            };
            true
        }
        "align-items" => {
            style.align_items = match v {
                "stretch" => Some(AlignMode::Stretch),
                "flex-start" | "start" => Some(AlignMode::FlexStart),
                "center" => Some(AlignMode::Center),
                "flex-end" | "end" => Some(AlignMode::FlexEnd),
                _ => return false,
            };
            true
        }
        "flex-grow" => {
            style.flex_grow = parse_num_f32(v);
            style.flex_grow.is_some()
        }
        "flex-shrink" => {
            style.flex_shrink = parse_num_f32(v);
            style.flex_shrink.is_some()
        }
        "flex-basis" => {
            // px only; `auto` (the initial value) stays None.
            style.flex_basis = parse_px_f32(v);
            style.flex_basis.is_some()
        }
        "gap" => {
            let vals: Vec<Option<f32>> = v.split_whitespace().map(parse_px_f32).collect();
            match vals.as_slice() {
                [one] => {
                    style.column_gap = *one;
                    style.row_gap = *one;
                }
                [c, r] => {
                    style.column_gap = *c;
                    style.row_gap = *r;
                }
                _ => return false,
            };
            true
        }
        "column-gap" => {
            style.column_gap = parse_px_f32(v);
            style.column_gap.is_some()
        }
        "row-gap" => {
            style.row_gap = parse_px_f32(v);
            style.row_gap.is_some()
        }
        "grid-template-columns" => {
            style.grid_template_columns = parse_grid_tracks(v);
            style.grid_template_columns.is_some()
        }
        "grid-template-rows" => {
            style.grid_template_rows = parse_grid_tracks(v);
            style.grid_template_rows.is_some()
        }
        "position" => {
            style.position = match v {
                "static" => Some(PositionMode::Static),
                "relative" => Some(PositionMode::Relative),
                "absolute" => Some(PositionMode::Absolute),
                "fixed" => Some(PositionMode::Fixed),
                _ => return false,
            };
            true
        }
        "float" => {
            style.float_side = match v {
                "left" => Some(FloatSide::Left),
                "right" => Some(FloatSide::Right),
                "none" => None,
                _ => return false,
            };
            true
        }
        "clear" => {
            style.clear_side = match v {
                "left" => Some(ClearSide::Left),
                "right" => Some(ClearSide::Right),
                "both" => Some(ClearSide::Both),
                "none" => None,
                _ => return false,
            };
            true
        }
        "top" => {
            style.top = len(v);
            style.top.is_some()
        }
        "right" => {
            style.right = len(v);
            style.right.is_some()
        }
        "bottom" => {
            style.bottom = len(v);
            style.bottom.is_some()
        }
        "left" => {
            style.left = len(v);
            style.left.is_some()
        }
        "min-width" => {
            style.min_width = len(v);
            style.min_width.is_some()
        }
        "max-width" => {
            style.max_width = len(v);
            style.max_width.is_some()
        }
        "min-height" => {
            style.min_height = len(v);
            style.min_height.is_some()
        }
        "max-height" => {
            style.max_height = len(v);
            style.max_height.is_some()
        }
        "aspect-ratio" => {
            // `1.5` or `16 / 9`; `auto` and invalid ratios stay None.
            style.aspect_ratio = parse_aspect_ratio(v);
            style.aspect_ratio.is_some()
        }
        _ => false,
    }
}

/// `aspect-ratio: 1.5` or `16 / 9` (width / height).
fn parse_aspect_ratio(v: &str) -> Option<f32> {
    let v = v.trim();
    if v == "auto" {
        return None;
    }
    let (w, h) = match v.split_once('/') {
        Some((w, h)) => (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?),
        None => (v.parse::<f32>().ok()?, 1.0),
    };
    if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
        Some(w / h)
    } else {
        None
    }
}

/// Plain number (flex-grow: 1).
fn parse_num_f32(v: &str) -> Option<f32> {
    v.parse::<f32>().ok()
}

/// Whitespace-separated track list: `1fr 2fr 100px auto`. Unknown tokens
/// abort the whole declaration (browsers drop it entirely).
fn parse_grid_tracks(v: &str) -> Option<Vec<GridTrack>> {
    let mut tracks = Vec::new();
    for token in v.split_whitespace() {
        if token == "auto" {
            tracks.push(GridTrack::Auto);
        } else if let Some(fr) = token.strip_suffix("fr") {
            tracks.push(GridTrack::Fr(fr.parse::<f32>().ok()?));
        } else if let Some(px) = parse_px_f32(token) {
            tracks.push(GridTrack::Px(px));
        } else {
            return None;
        }
    }
    if tracks.is_empty() {
        None
    } else {
        Some(tracks)
    }
}

fn set_side(sides: &mut Sides, name: &str, value: Option<Length>) {
    let slot = match name.rsplit_once('-').map(|(_, side)| side) {
        Some("top") => &mut sides.top,
        Some("right") => &mut sides.right,
        Some("bottom") => &mut sides.bottom,
        Some("left") => &mut sides.left,
        _ => return,
    };
    *slot = value;
}

/// CSS 1–4 value expansion (px/em/rem/%; unknown units drop to None).
fn expand_sides(value: &str, fonts: &FontCtx) -> Sides {
    let vals: Vec<Option<Length>> = value
        .split_whitespace()
        .map(|tok| parse_css_length(tok).map(|l| resolve_len(l, fonts)))
        .collect();
    match vals.as_slice() {
        [one] => Sides { top: *one, right: *one, bottom: *one, left: *one },
        [t, r] => Sides { top: *t, right: *r, bottom: *t, left: *r },
        [t, r, b] => Sides { top: *t, right: *r, bottom: *b, left: *r },
        [t, r, b, l] => Sides { top: *t, right: *r, bottom: *b, left: *l },
        _ => Sides::default(),
    }
}

/// px-only float length (gaps, flex-basis, grid tracks: fractional px legal,
/// units beyond px are later batches).
fn parse_px_f32(v: &str) -> Option<f32> {
    let v = v.trim();
    if v == "0" {
        return Some(0.0);
    }
    v.strip_suffix("px")?.parse::<f32>().ok()
}

/// font-size accepts px/em/rem/% and the common absolute keywords. em/%
/// resolve against the PARENT font-size, rem against the root — done by the
/// cascade's pre-pass via `font_size_px`.
fn parse_font_size_len(v: &str) -> Option<CssLength> {
    let v = v.trim();
    if let Some(l) = parse_css_length(v) {
        return Some(l);
    }
    match v {
        "small" => Some(CssLength::Px(13.0)),
        "medium" => Some(CssLength::Px(16.0)),
        "large" => Some(CssLength::Px(18.0)),
        "x-large" => Some(CssLength::Px(24.0)),
        _ => None,
    }
}

/// Fold a parsed font-size against its resolution bases.
fn font_size_px(l: CssLength, parent_fs: f32, root_fs: f32) -> f32 {
    match l {
        CssLength::Px(x) => x,
        CssLength::Em(n) => n * parent_fs,
        CssLength::Rem(n) => n * root_fs,
        CssLength::Percent(p) => p / 100.0 * parent_fs,
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
    // rgb()/rgba() — channels as 0-255 or %, alpha 0-1 (batch 4a: the most
    // common authored background format on real pages, and the one the
    // paint cross-checks style backgrounds with).
    if let Some(rest) = v
        .strip_prefix("rgb(")
        .or_else(|| v.strip_prefix("rgba("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let nums: Option<Vec<f32>> = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|tok| {
                if let Some(pct) = tok.strip_suffix('%') {
                    pct.parse::<f32>().ok().map(|p| p * 255.0 / 100.0)
                } else {
                    tok.parse::<f32>().ok()
                }
            })
            .collect();
        let nums = nums?;
        if nums.len() != 3 && nums.len() != 4 {
            return None;
        }
        let chan = |x: f32| x.round().clamp(0.0, 255.0) as u8;
        let a = if nums.len() == 4 { (nums[3].clamp(0.0, 1.0) * 255.0).round() as u8 } else { 255 };
        return Some(Color(chan(nums[0]), chan(nums[1]), chan(nums[2]), a));
    }
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
    root_font_size: f32,
) -> ComputedStyle {
    let mut style = ComputedStyle {
        display: Some(ua_display(tag)),
        font_weight: ua_font_weight(tag),
        ..Default::default()
    };
    if let Some((px, line)) = ua_border(tag) {
        style.border_width.top = Some(Length::Px(px));
        style.border_width.right = Some(Length::Px(px));
        style.border_width.bottom = Some(Length::Px(px));
        style.border_width.left = Some(Length::Px(px));
        style.border_style = Some(line);
    }

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

    // Font-size pre-pass: CSS computes font-size before every other
    // property (regardless of declaration order within a block), because
    // em lengths elsewhere resolve against it. Walk the same winning order
    // (sorted candidates, then inline) and keep the last parseable
    // declaration, then fold em/% against the PARENT size and rem against
    // the root size.
    let parent_fs = parent
        .and_then(|p| p.font_size)
        .unwrap_or(DEFAULT_ROOT_FONT_SIZE);
    let mut fs_decl: Option<CssLength> = None;
    for candidate in &candidates {
        if let Some(d) = last_font_size_decl(candidate.declarations) {
            fs_decl = Some(d);
        }
    }
    if let Some(inline) = inline_css {
        if let Some(d) = last_font_size_decl(inline) {
            fs_decl = Some(d);
        }
    }
    let own_fs = fs_decl
        .map(|d| font_size_px(d, parent_fs, root_font_size))
        .unwrap_or(parent_fs);
    style.font_size = Some(own_fs);
    let fonts = FontCtx { own: own_fs, root: root_font_size };

    let _ = tree;
    let _ = node_id;
    for candidate in &candidates {
        apply_declarations_with(&mut style, candidate.declarations, &fonts);
    }

    // Inline style always last (same font context: inline em resolves
    // against the element's own font-size too).
    if let Some(inline) = inline_css {
        apply_declarations_with(&mut style, inline, &fonts);
    }

    style
}

/// The winning font-size declaration in one declaration block (last
/// parseable wins, matching apply order). Caller folds candidate+inline in
/// order to find the overall winner.
fn last_font_size_decl(declarations: &str) -> Option<CssLength> {
    let mut found = None;
    for (name, value) in split_declarations(declarations) {
        if name == "font-size" {
            if let Some(d) = parse_font_size_len(&value) {
                found = Some(d);
            }
        }
    }
    found
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
    fn float_and_clear_parse_into_style() {
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "float: left");
        assert_eq!(s.float_side, Some(FloatSide::Left));
        assert_eq!(s.clear_side, None);

        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "float: right; clear: both");
        assert_eq!(s.float_side, Some(FloatSide::Right));
        assert_eq!(s.clear_side, Some(ClearSide::Both));

        // Explicit initial values compute to None.
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "float: none; clear: none");
        assert_eq!(s.float_side, None);
        assert_eq!(s.clear_side, None);

        // Invalid values drop the declaration (style stays default).
        let mut s = ComputedStyle::default();
        assert!(!apply_declarations(&mut s, "float: top"));
        assert!(!apply_declarations(&mut s, "clear: all"));
        assert!(!apply_declarations(&mut s, "clear: inline-start"));
        assert_eq!(s, ComputedStyle::default());
    }

    #[test]
    fn float_clear_probe_matches_grammar() {
        assert!(supports_declaration("float", "left"));
        assert!(supports_declaration("float", "right"));
        assert!(supports_declaration("float", "none"));
        assert!(!supports_declaration("float", "top"));
        assert!(supports_declaration("clear", "both"));
        assert!(!supports_declaration("clear", "inline-start"), "not in our grammar yet");
        // `0` passes the probe's shared unitless-zero gate (the same wildcard
        // every keyword property gets); the value parser itself declines it.
    }

    #[test]
    fn parse_color_rgb_function_forms() {
        assert_eq!(parse_color("rgb(198, 40, 40)"), Some(Color(198, 40, 40, 255)));
        assert_eq!(parse_color("rgb(198 40 40)"), Some(Color(198, 40, 40, 255)), "space-separated");
        assert_eq!(parse_color("rgba(198,40,40,0.5)"), Some(Color(198, 40, 40, 128)));
        assert_eq!(parse_color("RGB(50%, 0%, 0%)"), Some(Color(128, 0, 0, 255)), "percent + case-insensitive");
        assert_eq!(parse_color("rgb(1,2)"), None, "wrong arity");
    }

    #[test]
    fn border_shorthand_and_longhands_parse() {
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "border: 6px solid rgb(20,60,200)");
        assert_eq!(s.border_width.top, Some(Length::Px(6.0)));
        assert_eq!(s.border_width.left, Some(Length::Px(6.0)));
        assert_eq!(s.border_style, Some(BorderStyle::Solid));
        assert_eq!(s.border_color, Some(Color(20, 60, 200, 255)));

        // Order-free shorthand + width keywords.
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "border: red thin dashed");
        assert_eq!(s.border_width.left, Some(Length::Px(1.0)));
        assert_eq!(s.border_color, Some(Color(255, 0, 0, 255)));
        assert_eq!(s.border_style, Some(BorderStyle::Dashed));

        // Longhands, incl. the 2-value side expansion.
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "border-width: 2px 4px; border-style: solid; border-color: blue");
        assert_eq!(s.border_width.top, Some(Length::Px(2.0)));
        assert_eq!(s.border_width.left, Some(Length::Px(4.0)));
        assert_eq!(s.border_style, Some(BorderStyle::Solid));
        assert_eq!(s.border_color, Some(Color(0, 0, 255, 255)));

        // `none` computes any widths away (CSS initial style).
        apply_declarations(&mut s, "border-style: none");
        assert_eq!(s.border_style, None);

        // Garbage token drops the whole shorthand declaration.
        let mut s = ComputedStyle::default();
        assert!(!apply_declarations(&mut s, "border: solid wat"));
        assert_eq!(s.border_style, None);
    }

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
        assert_eq!(s.margin.top, Some(Length::Px(1.0)));
        assert_eq!(s.margin.right, Some(Length::Px(2.0)));
        assert_eq!(s.margin.bottom, Some(Length::Px(3.0)));
        assert_eq!(s.margin.left, Some(Length::Px(4.0)));
        assert_eq!(s.padding.top, Some(Length::Px(8.0)));
        assert_eq!(s.padding.left, Some(Length::Px(8.0)));

        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin: 5px 6px");
        assert_eq!((s.margin.top, s.margin.bottom), (Some(Length::Px(5.0)), Some(Length::Px(5.0))));
        assert_eq!((s.margin.right, s.margin.left), (Some(Length::Px(6.0)), Some(Length::Px(6.0))));

        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin: 7px 8px 9px");
        assert_eq!(s.margin.bottom, Some(Length::Px(9.0)));
        assert_eq!(s.margin.left, Some(Length::Px(8.0)), "3-value form mirrors right to left");
    }

    #[test]
    fn colors_parse_named_hex_short_hex() {
        assert_eq!(parse_color("red"), Some(Color(255, 0, 0, 255)));
        assert_eq!(parse_color("#00ff00"), Some(Color(0, 255, 0, 255)));
        assert_eq!(parse_color("#f00"), Some(Color(255, 0, 0, 255)));
        assert_eq!(parse_color("#ff000080").map(|c| c.3), Some(128));
        // rgb()/rgba() absorbed in batch 4a (was "out of slice scope").
        assert_eq!(parse_color("rgb(1,2,3)"), Some(Color(1, 2, 3, 255)));
    }

    #[test]
    fn unitless_nonzero_lengths_rejected() {
        let mut s = ComputedStyle::default();
        apply_declarations(&mut s, "margin-top: 0; margin-bottom: 12; padding-left: 3px");
        assert_eq!(s.margin.top, Some(Length::Px(0.0)), "zero is a valid unitless length");
        assert_eq!(s.margin.bottom, None, "nonzero unitless length is invalid CSS");
        assert_eq!(s.padding.left, Some(Length::Px(3.0)));
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

        let computed = cascade_element("p", &tree, node, &matched, None, Some("background-color: #abcdef"), DEFAULT_ROOT_FONT_SIZE);
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
        let child = cascade_element("em", &tree, em, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE);
        assert_eq!(child.color, parent.color, "color inherits");
        assert_eq!(child.font_size, parent.font_size, "font-size inherits");
        assert_eq!(child.text_align, parent.text_align, "text-align inherits");
        assert_eq!(child.display, Some(Display::Inline), "UA default for <em>");

        // Author rule on the child overrides the inherited color only.
        let rule = ParsedRule { selector: "em".into(), declarations: "color: red".into() };
        let spec = tree.compile_rule_selector("em").unwrap().specificity();
        let overridden = cascade_element("em", &tree, em, &[(&rule, spec)], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE);
        assert_eq!(overridden.color, Some(Color(255, 0, 0, 255)));
        assert_eq!(overridden.font_size, parent.font_size, "unmentioned props still inherit");

        // Section itself: block UA default even with no author CSS.
        let block = cascade_element("section", &tree, section, &[], None, None, DEFAULT_ROOT_FONT_SIZE);
        assert_eq!(block.display, Some(Display::Block));
    }

    // ---- batch 2e: em/rem/% lengths ----

    #[test]
    fn em_rem_and_percent_parse_and_resolve() {
        let mut s = ComputedStyle::default();
        let fonts = FontCtx { own: 20.0, root: 32.0 };
        apply_declarations_with(&mut s, "width: 10em; height: 2.5rem; margin: 5% 1px", &fonts);
        assert_eq!(s.width, Some(Length::Px(200.0)), "em folds against own fs");
        assert_eq!(s.height, Some(Length::Px(80.0)), "rem folds against root fs");
        assert_eq!(s.margin.top, Some(Length::Percent(5.0)), "% stays symbolic");
        assert_eq!(s.margin.left, Some(Length::Px(1.0)));
    }

    #[test]
    fn font_size_em_percent_against_parent_rem_against_root() {
        let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
        let p = tree.query_selector("p").unwrap().unwrap();
        let parent = ComputedStyle { font_size: Some(20.0), ..Default::default() };

        // 1.5em of 20 → 30; 150% of 20 → 30; 1.25rem of root 24 → 30.
        let mk = |decl: &str, root: f32| {
            cascade_element(
                "p", &tree, p,
                &[(&ParsedRule { selector: "p".into(), declarations: decl.into() }, 1)],
                Some(&parent), None, root,
            )
        };
        assert_eq!(mk("font-size: 1.5em", 16.0).font_size, Some(30.0), "em against parent");
        assert_eq!(mk("font-size: 150%", 16.0).font_size, Some(30.0), "% against parent");
        assert_eq!(mk("font-size: 1.25rem", 24.0).font_size, Some(30.0), "rem against root");
        assert_eq!(mk("color: red", 16.0).font_size, Some(20.0), "inherits parent");
    }

    #[test]
    fn font_size_computes_before_em_lengths_same_block() {
        // Declaration order inside one block must not matter: font-size is
        // computed first, then width's em folds against it (CSS spec order).
        let tree = diting_dom::tree_sink::parse_html("<div><p>x</p></div>");
        let p = tree.query_selector("p").unwrap().unwrap();
        let cs = cascade_element(
            "p", &tree, p,
            &[(&ParsedRule { selector: "p".into(), declarations: "width: 10em; font-size: 24px".into() }, 1)],
            None, None, DEFAULT_ROOT_FONT_SIZE,
        );
        assert_eq!(cs.font_size, Some(24.0));
        assert_eq!(cs.width, Some(Length::Px(240.0)), "10em of the same block's font-size");
    }

    #[test]
    fn cascade_font_size_wins_by_specificity_not_prepass_order() {
        // The pre-pass must respect cascade order: the id rule's font-size
        // beats the later-parsed class rule, and width's em folds against
        // the WINNER.
        let tree = diting_dom::tree_sink::parse_html(r#"<p id="m" class="c">x</p>"#);
        let p = tree.get_element_by_id("m").unwrap();
        let rules = vec![
            ParsedRule { selector: "p.c".into(), declarations: "font-size: 10px; width: 2em".into() },
            ParsedRule { selector: "#m".into(), declarations: "font-size: 30px".into() },
        ];
        let matched: Vec<(&ParsedRule, u32)> = rules
            .iter()
            .filter_map(|r| tree.compile_rule_selector(&r.selector).map(|c| (r, c.specificity())))
            .collect();
        let cs = cascade_element("p", &tree, p, &matched, None, None, DEFAULT_ROOT_FONT_SIZE);
        assert_eq!(cs.font_size, Some(30.0), "id specificity wins font-size");
        assert_eq!(cs.width, Some(Length::Px(60.0)), "2em of 30, not of 10");
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

        let computed = cascade_element("p", &tree, lead, &matched, None, None, DEFAULT_ROOT_FONT_SIZE);
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
        let art_computed = cascade_element("article", &tree, article, &art_matched, None, None, DEFAULT_ROOT_FONT_SIZE);
        assert_eq!(art_computed.padding.top, Some(Length::Px(12.0)), ":where(article) padding applies");
    }
}
