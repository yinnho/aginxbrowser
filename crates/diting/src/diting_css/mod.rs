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

/// Emulated media features pushed via CDP `Emulation.setEmulatedMedia`
/// (Playwright's `page.emulateMedia`): (name, value) pairs such as
/// `("prefers-reduced-motion", "reduce")`. A feature present here answers
/// every query for that name from its emulated value; the rest keep the
/// persona defaults `media_pref_default` holds — the same table the JS
/// `matchMedia` publishes, so the cascade and the scripts agree (#29).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaOverrides {
    pub features: Vec<(String, String)>,
}

/// The un-emulated value a preference feature holds — mirrors the
/// `_MQ_PERSONA_BOOL` desktop-light persona in the bootstrap. Features not
/// listed have no default and stay false in both faces until emulated.
fn media_pref_default(name: &str) -> Option<&'static str> {
    match name {
        "prefers-color-scheme" => Some("light"),
        "prefers-reduced-motion" => Some("no-preference"),
        "prefers-reduced-transparency" => Some("no-preference"),
        _ => None,
    }
}

/// One flattened stylesheet rule: selector text plus its declaration block.
/// Selectors are compiled lazily by the cascade via diting_dom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRule {
    pub selector: String,
    pub declarations: String,
}

/// Attribute names referenced by attribute selectors in a rule pool
/// (`[tabindex]`, `[data-x="1"]`, `[lang|=en]`), lowercased. Feeds the
/// layout-cache invalidation decision for attribute writes: a write to a
/// layout-inert attribute is only safe to skip when no rule selects on it
/// (obscura#983).
pub fn collect_selector_attr_names(rules: &[ParsedRule]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for rule in rules {
        let s = rule.selector.as_bytes();
        let mut i = 0;
        while i < s.len() {
            let Some(open) = s[i..].iter().position(|&b| b == b'[').map(|p| p + i) else { break };
            let mut j = open + 1;
            while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'-' || s[j] == b'_') {
                j += 1;
            }
            // A `|` right after the ident followed by another ident is a
            // namespace prefix, not the attribute name — skip conservatively.
            // `|=` is the dash-match operator and keeps the name.
            let namespaced = s.get(j) == Some(&b'|') && s.get(j + 1) != Some(&b'=');
            if j > open + 1 && !namespaced {
                out.insert(String::from_utf8_lossy(&s[open + 1..j]).to_ascii_lowercase());
            }
            i = j.max(open + 1);
        }
    }
    out
}

/// One `@keyframes` stop (`from` = 0, `to` = 1, `NN%` = NN/100) with raw
/// declarations. Values stay raw strings because `var()` in stop values
/// (e.g. `to { opacity: var(--o, 1) }`) substitutes against the custom
/// properties of whichever element declares `animation:` — resolved per
/// element at sample time, not at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyframeStop {
    pub offset: f32,
    pub decls: Vec<(String, String)>,
}

/// All stops of one `@keyframes` name, sorted by offset ascending.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframes {
    pub stops: Vec<KeyframeStop>,
}

pub type KeyframesMap = std::collections::HashMap<String, Keyframes>;

/// `container-type` — marks an element as a queryable container ancestor
/// (css-conditional-5). v1 is a marker only: no size containment behavior
/// is modeled, the property just feeds `@container` target lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContainerType {
    #[default]
    Normal,
    Size,
    InlineSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAxis {
    Width,
    Height,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContainerCmp {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
}

/// One size feature of a `@container` condition, value pre-resolved to px
/// (same px-only limitation as `@media` features here).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContainerFeature {
    pub axis: ContainerAxis,
    pub cmp: ContainerCmp,
    pub px: f32,
}

/// AND-combination of size features. An absent condition (bare
/// `@container name {}`) matches any qualifying container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerCondition(pub Vec<ContainerFeature>);

impl ContainerCondition {
    pub fn matches(&self, width: f32, height: f32) -> bool {
        self.0.iter().all(|f| {
            let v = match f.axis {
                ContainerAxis::Width => width,
                ContainerAxis::Height => height,
            };
            match f.cmp {
                ContainerCmp::Ge => v >= f.px,
                ContainerCmp::Le => v <= f.px,
                ContainerCmp::Gt => v > f.px,
                ContainerCmp::Lt => v < f.px,
                ContainerCmp::Eq => (v - f.px).abs() < f32::EPSILON,
            }
        })
    }
}

/// One `@container` rule kept unflattened: the condition answers against the
/// element's nearest qualifying container ancestor, which only exists after
/// a layout pass — so unlike `@media` this cannot resolve at parse time
/// (moli#282: conditions that parse but never match).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerRule {
    pub name: Option<String>,
    pub condition: Option<ContainerCondition>,
    pub rules: Vec<ParsedRule>,
}

/// Parse a stylesheet into flattened rules. Handles nested braces, comments,
/// and the at-rules whose bodies contain ordinary rules (`@media`,
/// `@supports`, `@layer`). Other at-rules (`@font-face`, `@import`, ...)
/// are dropped: they contribute no layout-relevant rule here. `@keyframes`
/// blocks are dropped as rules but their stops land in the animation table
/// when parsed via [`parse_stylesheet_timed`].
///
/// Error recovery mirrors browsers (and upstream): an unbalanced stray `}`
/// at top level resynchronizes instead of scrambling every following rule.
pub fn parse_stylesheet(css: &str) -> Vec<ParsedRule> {
    parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Screen)
}

pub fn parse_stylesheet_for(css: &str, viewport: (f32, f32), media_type: CssMediaType) -> Vec<ParsedRule> {
    parse_stylesheet_timed(css, viewport, media_type).0
}

/// Full stylesheet parse: flattened rules PLUS the `@keyframes` table the
/// animation sampler samples from. Rules-only callers keep
/// [`parse_stylesheet_for`]; this is the entry the layout run uses so a
/// declarative SVG's `<style>` keyframes reach style resolution.
pub fn parse_stylesheet_timed(
    css: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
) -> (Vec<ParsedRule>, KeyframesMap) {
    parse_stylesheet_timed_with(css, viewport, media_type, &MediaOverrides::default())
}

/// [`parse_stylesheet_timed`] with emulated media in play: the layout run
/// passes the page's `Emulation.setEmulatedMedia` state so `@media
/// (prefers-*)` arms follow what `matchMedia` already answers (#29).
pub fn parse_stylesheet_timed_with(
    css: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
) -> (Vec<ParsedRule>, KeyframesMap) {
    parse_stylesheet_with_containers(css, viewport, media_type, overrides, &mut Vec::new())
}

/// [`parse_stylesheet_timed_with`] plus the `@container` table. Container
/// rules never flatten into the ordinary rule list — their conditions need
/// per-element layout answers — so callers that can evaluate them (the
/// layout run) take this entry; everyone else keeps dropping them.
pub fn parse_stylesheet_full(
    css: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
) -> (Vec<ParsedRule>, KeyframesMap, Vec<ContainerRule>) {
    let mut containers = Vec::new();
    let (rules, keyframes) =
        parse_stylesheet_with_containers(css, viewport, media_type, overrides, &mut containers);
    (rules, keyframes, containers)
}

fn parse_stylesheet_with_containers(
    css: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
    containers: &mut Vec<ContainerRule>,
) -> (Vec<ParsedRule>, KeyframesMap) {
    let mut rules = Vec::new();
    let mut keyframes = KeyframesMap::new();
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
                    flush_at_rule(
                        at,
                        decls,
                        &mut rules,
                        &mut keyframes,
                        viewport,
                        media_type,
                        overrides,
                        containers,
                    );
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
    (rules, keyframes)
}

/// Handle the at-rules whose bodies contain ordinary rules. `@media` applies
/// inner rules when the query holds; `@supports` when the condition evaluates
/// true; `@layer` recurses with the named layer tracked (ordering only — we
/// have no layer-priority cascade yet, so layers flatten). `@keyframes`
/// parses its stops into the animation table.
#[allow(clippy::too_many_arguments)]
fn flush_at_rule(
    at: &str,
    inner: &str,
    rules: &mut Vec<ParsedRule>,
    keyframes: &mut KeyframesMap,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
    containers: &mut Vec<ContainerRule>,
) {
    let recurse = |rules: &mut Vec<ParsedRule>,
                   keyframes: &mut KeyframesMap,
                   containers: &mut Vec<ContainerRule>| {
        // Nested @container inside @media/@supports/@layer must surface in
        // the caller's container table, not a throwaway vec (the failing
        // branch simply never recurses — drop semantics hold there).
        let (inner_rules, inner_kf) =
            parse_stylesheet_with_containers(inner, viewport, media_type, overrides, containers);
        rules.extend(inner_rules);
        for (k, v) in inner_kf {
            keyframes.entry(k).or_insert(v);
        }
    };
    if let Some(prelude) = at_rule_prelude(at, "media") {
        if media_query_applies_with(prelude, viewport, media_type, overrides) {
            recurse(rules, keyframes, containers);
        }
    } else if let Some(prelude) = at_rule_prelude(at, "supports") {
        if supports_condition_applies(prelude) {
            recurse(rules, keyframes, containers);
        }
    } else if let Some(_prelude) = at_rule_prelude(at, "layer") {
        recurse(rules, keyframes, containers);
    } else if let Some(prelude) = at_rule_prelude(at, "keyframes") {
        // `from`/`to`/`NN%` stops; a stop that fails to parse is skipped,
        // not fatal — the sampler works with however many stops survive.
        if let Some(kf) = parse_keyframes_body(inner) {
            let name = prelude.trim();
            if !name.is_empty() {
                keyframes.insert(name.to_string(), kf);
            }
        }
    } else if let Some(prelude) = at_rule_prelude(at, "container") {
        parse_container_rule(prelude, inner, viewport, media_type, overrides, containers);
    }
    // Other at-rules carry no layout-relevant rules for us; drop them.
}

/// Parse one `@container` prelude + body into the container table. The body
/// recurses through the ordinary parser: plain rules land on this rule,
/// nested `@container` rules AND-compose their conditions with ours (v1
/// approximation of nearest-container semantics for the nested spelling),
/// and `@media` inside the body resolves at parse time as usual. A prelude
/// whose condition fails to parse (style queries, range triples) drops the
/// whole rule — the conservative "never matches", not "always matches".
fn parse_container_rule(
    prelude: &str,
    inner: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
    containers: &mut Vec<ContainerRule>,
) {
    let prelude = prelude.trim();
    let (name, cond_text) = split_container_prelude(prelude);
    if let Some(text) = cond_text {
        if parse_container_condition(text).is_none() {
            return;
        }
    }
    let mut nested = Vec::new();
    // v1: @keyframes inside a container body is dropped (rare in practice;
    // the plain rules are what the gated cascade needs).
    let (plain, _) =
        parse_stylesheet_with_containers(inner, viewport, media_type, overrides, &mut nested);
    let own = ContainerRule {
        name: name.clone(),
        condition: cond_text.and_then(parse_container_condition),
        rules: plain,
    };
    for mut n in nested {
        n.name = n.name.take().or_else(|| name.clone());
        n.condition = merge_conditions(own.condition.as_ref(), n.condition.take());
        containers.push(n);
    }
    // A container whose body holds ONLY nested @container rules contributes
    // nothing of its own — pushing the empty shell would sit in the plan as
    // a no-op entry.
    if !own.rules.is_empty() {
        containers.push(own);
    }
}

fn merge_conditions(outer: Option<&ContainerCondition>, inner: Option<ContainerCondition>) -> Option<ContainerCondition> {
    match (outer, inner) {
        (None, i) => i,
        (Some(o), None) => Some(o.clone()),
        (Some(o), Some(mut i)) => {
            i.0.extend_from_slice(&o.0);
            Some(i)
        }
    }
}

/// `@container` prelude = `[<name>]? <condition>?`. A leading `(` means no
/// name; otherwise the first identifier is the name and the rest (if any)
/// is the condition.
fn split_container_prelude(prelude: &str) -> (Option<String>, Option<&str>) {
    let p = prelude.trim();
    if p.is_empty() {
        return (None, None);
    }
    if p.starts_with('(') {
        return (None, Some(p));
    }
    match p.find(char::is_whitespace) {
        Some(i) => {
            let name = &p[..i];
            let rest = p[i..].trim();
            let ident = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
            if ident && (rest.is_empty() || rest.starts_with('(')) {
                (Some(name.to_string()), if rest.is_empty() { None } else { Some(rest) })
            } else {
                (None, Some(p))
            }
        }
        None => (Some(p.to_string()), None),
    }
}

/// AND-split a condition on top-level `and` (paren-depth aware), then parse
/// each feature. OR-spelling and style queries parse-fail → the rule drops.
fn parse_container_condition(text: &str) -> Option<ContainerCondition> {
    let mut feats = Vec::new();
    for part in split_top_level_and(text) {
        feats.push(parse_container_feature(part)?);
    }
    if feats.is_empty() {
        return None;
    }
    Some(ContainerCondition(feats))
}

fn split_top_level_and(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let rest = &text[i..];
                if rest.len() >= 3 {
                    let (word, tail) = rest.split_at(3);
                    if word.eq_ignore_ascii_case("and")
                        && tail.starts_with(|c: char| c.is_ascii_whitespace())
                        && text[..i].ends_with(|c: char| c.is_ascii_whitespace())
                    {
                        parts.push(&text[start..i]);
                        i += 3;
                        start = i;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(text[start..].trim());
    parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

fn parse_container_feature(f: &str) -> Option<ContainerFeature> {
    let f = f.trim();
    let f = if f.starts_with('(') && f.ends_with(')') && f.len() > 1 {
        f[1..f.len() - 1].trim()
    } else {
        f
    };
    if let Some((name, value)) = f.split_once(':') {
        let px = parse_px(value.trim())?;
        let feat = |axis, cmp| Some(ContainerFeature { axis, cmp, px });
        return match name.trim() {
            "min-width" | "min-inline-size" => feat(ContainerAxis::Width, ContainerCmp::Ge),
            "max-width" | "max-inline-size" => feat(ContainerAxis::Width, ContainerCmp::Le),
            "width" | "inline-size" => feat(ContainerAxis::Width, ContainerCmp::Eq),
            "min-height" | "min-block-size" => feat(ContainerAxis::Height, ContainerCmp::Ge),
            "max-height" | "max-block-size" => feat(ContainerAxis::Height, ContainerCmp::Le),
            "height" | "block-size" => feat(ContainerAxis::Height, ContainerCmp::Eq),
            _ => None,
        };
    }
    // Range syntax: (width >= 300px) or (300px <= width). Multi-value
    // triples (`300px <= width <= 500px`) fail the single-split parse.
    for (sym, cmp) in [
        (">=", ContainerCmp::Ge),
        ("<=", ContainerCmp::Le),
        (">", ContainerCmp::Gt),
        ("<", ContainerCmp::Lt),
        ("=", ContainerCmp::Eq),
    ] {
        if let Some((l, r)) = f.split_once(sym) {
            let (l, r) = (l.trim(), r.trim());
            if let Some(axis) = container_axis_of(l) {
                return Some(ContainerFeature { axis, cmp, px: parse_px(r)? });
            }
            if let Some(axis) = container_axis_of(r) {
                let flipped = match cmp {
                    ContainerCmp::Ge => ContainerCmp::Le,
                    ContainerCmp::Le => ContainerCmp::Ge,
                    ContainerCmp::Gt => ContainerCmp::Lt,
                    ContainerCmp::Lt => ContainerCmp::Gt,
                    ContainerCmp::Eq => ContainerCmp::Eq,
                };
                return Some(ContainerFeature { axis, cmp: flipped, px: parse_px(l)? });
            }
        }
    }
    None
}

fn container_axis_of(s: &str) -> Option<ContainerAxis> {
    match s.trim() {
        "width" | "inline-size" => Some(ContainerAxis::Width),
        "height" | "block-size" => Some(ContainerAxis::Height),
        _ => None,
    }
}

/// Parse the body of one `@keyframes` rule: top-level `{ ... }` blocks
/// whose selector is `from`, `to`, or a percentage. Stops sort by offset;
/// a body with no surviving stop yields None.
fn parse_keyframes_body(inner: &str) -> Option<Keyframes> {
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
            s => s.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok().map(|p| p / 100.0)),
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
    media_query_applies_with(query, viewport, media_type, &MediaOverrides::default())
}

/// Same, with emulated media features (`Emulation.setEmulatedMedia`) consulted
/// before the persona defaults — the cascade face of `matchMedia`'s truth.
pub fn media_query_applies_with(
    query: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
) -> bool {
    split_media_query_list(query)
        .into_iter()
        .any(|q| single_media_query_applies(q, viewport, media_type, overrides))
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

fn single_media_query_applies(
    query: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
) -> bool {
    let query = query.trim().strip_prefix("@media").unwrap_or(query).trim();
    let compact: String = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();

    // Leading `not` negates the whole arm. Only strip the `not` token itself;
    // the media type after it must stay intact (`not print` → evaluate
    // `print`, not an empty string that would wrongly imply `all`).
    if let Some(rest) = compact.strip_prefix("not") {
        return !single_media_query_applies_compact(rest, viewport, media_type, overrides);
    }
    single_media_query_applies_compact(&compact, viewport, media_type, overrides)
}

fn single_media_query_applies_compact(
    compact: &str,
    viewport: (f32, f32),
    media_type: CssMediaType,
    overrides: &MediaOverrides,
) -> bool {
    // A bare feature list (`(min-width: 768px)`) implies media type `all`;
    // evaluate its features directly instead of walking the type-token path.
    if compact.starts_with('(') || compact.is_empty() {
        return compact
            .split("and")
            .all(|feature| media_feature_applies(feature, viewport, overrides));
    }

    // Media type check first; features then refine. An unknown type fails the arm.
    let (type_matches, rest) = if let Some(r) = compact.strip_prefix("print") {
        (media_type == CssMediaType::Print, Some(r))
    } else if let Some(r) = compact.strip_prefix("screen") {
        (media_type == CssMediaType::Screen, Some(r))
    } else if let Some(r) = compact.strip_prefix("all") {
        // `all` applies under every media type — including emulated print.
        (true, Some(r))
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
    feature_expr
        .split("and")
        .all(|feature| media_feature_applies(feature, viewport, overrides))
}

/// One `(min-width: 768px)`-style expression. Width/height bounds compare
/// against the viewport; `prefers-*` preferences answer from the emulated
/// overrides first, then the persona defaults — evaluated before the px parse
/// so a non-numeric value like `reduce` isn't discarded as unparseable.
fn media_feature_applies(feature: &str, viewport: (f32, f32), overrides: &MediaOverrides) -> bool {
    let feature = feature.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let Some((name, value)) = feature.split_once(':') else {
        // Boolean feature without a value: unsupported → conservative false.
        return false;
    };
    let name = name.trim();
    let value = value.trim();
    if let Some(answer) = media_pref_answer(name, value, overrides) {
        return answer;
    }
    let value_px = parse_px(value);
    let value_px = match value_px {
        Some(v) => v,
        None => return false,
    };
    match name {
        "min-width" => viewport.0 >= value_px,
        "max-width" => viewport.0 <= value_px,
        "min-height" => viewport.1 >= value_px,
        "max-height" => viewport.1 <= value_px,
        "width" => (viewport.0 - value_px).abs() < f32::EPSILON,
        "height" => (viewport.1 - value_px).abs() < f32::EPSILON,
        _ => false,
    }
}

/// Resolve a `prefers-*` (or any keyword-valued preference) expression:
/// emulated value wins, persona default second. `None` = not a preference
/// feature we model — the caller falls through to the numeric path.
fn media_pref_answer(name: &str, value: &str, overrides: &MediaOverrides) -> Option<bool> {
    let emulated = overrides
        .features
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v);
    let truth = match emulated {
        Some(v) => Some(v.as_str()),
        None => media_pref_default(name),
    }?;
    Some(truth.eq_ignore_ascii_case(value))
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
        "line-height",
        "text-align", "border", "border-top", "border-right", "border-bottom", "border-left",
        "border-color", "border-width", "border-style",
        "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
        "width", "height", "flex-direction", "gap", "overflow", "overflow-x", "overflow-y",
        "white-space", "text-overflow",
        "object-fit", "object-position", "z-index", "border-radius",
        "float", "clear", "border-collapse", "vertical-align", "opacity", "transform",
    ];
    if !SUPPORTED.contains(&name.to_ascii_lowercase().as_str()) {
        return false;
    }
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
        "color" | "background-color" => parse_color(value).is_some() || value.starts_with("rgb") || value.starts_with("hsl"),
        "background" => parse_color(value.split_whitespace().next().unwrap_or("")).is_some()
            || value.starts_with("url(")
            || value.contains("gradient"),
        "text-align" => matches!(value, "left" | "start" | "center" | "right" | "end" | "justify"),
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
                s.ends_with('%') && s[..s.len() - 1].parse::<f32>().is_ok()
                    || s.ends_with("px") && s[..s.len() - 2].parse::<f32>().is_ok()
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

// ---------------------------------------------------------------------------
// Computed style: minimal property subset + inheritance
// ---------------------------------------------------------------------------

/// One parsed `box-shadow` layer (blitz#349 family, v1): offsets/blur/
/// spread in px (em/rem folded at cascade time), color resolved at parse
/// time (`currentColor` folds against the color value cascaded so far).
/// `inset` parses and reports through the CSSOM but the paint half is
/// outer-only for v1 — a documented divergence, not a parse failure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

/// One `text-shadow` layer (blitz#271 family): offset + optional blur
/// around a color. No spread, no `inset` — both are parse errors here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub color: Color,
}

/// The computed values this slice models. Deliberately tiny: enough to lock
/// cascade ordering, specificity, inline override, and inheritance semantics.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ComputedStyle {
    /// Outer display role. `None` means "not declared anywhere".
    pub display: Option<Display>,
    /// True while `display` still holds the cascade's UA-default fill
    /// (`Some(ua_display(tag))`). The CSSOM layer reports Chrome's per-tag
    /// UA display for those tags (table→table, li→list-item, ...) without
    /// the layout engine needing table/list box types (obscura #771).
    pub display_from_ua: bool,
    pub color: Option<Color>,
    /// SVG paint properties (svg v1). Deliberately NON-inherited here: the
    /// svg subtree compiler does paint inheritance itself, because CSS
    /// presentation-attribute priority (own CSS > own attr > inherited CSS)
    /// needs to distinguish "declared on this element" from "inherited",
    /// which a merged ComputedStyle can't. `None` = nothing declared on this
    /// element; the svg compiler seeds unset chains with the SVG initial
    /// values (fill black, stroke none, width 1).
    pub svg_fill: Option<SvgPaint>,
    pub svg_stroke: Option<SvgPaint>,
    pub svg_stroke_width: Option<f32>,
    pub svg_dasharray: Option<Vec<f32>>,
    /// `stroke-dashoffset` (animation input): the dash phase shift, in the
    /// same user units as the dasharray. The svg compiler folds it into the
    /// stroke so `stroke-dashoffset` keyframes produce the self-draw effect.
    pub svg_dashoffset: Option<f32>,
    pub background_color: Option<Color>,
    /// `box-shadow` layers (blitz#349 family, v1), non-inherited; first
    /// layer paints on top (CSS paint order). `None` = `none`.
    pub box_shadow: Option<Vec<BoxShadow>>,
    /// `text-shadow` layers (blitz#271 family), INHERITED (copied down in
    /// `cascade_element` like `color`); first layer paints on top.
    /// `None` = `none`.
    pub text_shadow: Option<Vec<TextShadow>>,
    /// `backdrop-filter: blur(<length>)` (blitz#901 family), v1 blur-only,
    /// NON-inherited: the px blur radius applied to whatever painted
    /// beneath the element's border box before its own background. `None`
    /// = `none`.
    pub backdrop_blur: Option<f32>,
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
    /// Interpretation (content-box vs border-box edges) is `box_sizing`'s
    /// call — the layout layer does the edge mapping at taffy hand-off.
    pub width: Option<Length>,
    pub height: Option<Length>,
    /// `box-sizing` (non-inherited; the `*` reset idiom sets it per element).
    /// `None` = the CSS initial `content-box`.
    pub box_sizing: Option<BoxSizing>,
    /// `background-clip: text` (non-inherited; real pages almost always ship
    /// the `-webkit-background-clip` alias, both land here). The background
    /// paints only inside the foreground glyphs — with a gradient this is the
    /// gradient-text idiom. `false` (the initial box fill) covers every box
    /// keyword; our background layers paint the border box regardless.
    pub background_clip_text: bool,
    /// Font size in px (absolute keywords/units resolved by the caller's sheet
    /// context; here we accept px/em/% where em resolves against parent).
    pub font_size: Option<f32>,
    pub font_weight: Option<u16>,
    /// Inherited: nearest ancestor's declared value, `None` = `normal`.
    pub line_height: Option<LineHeightSpec>,
    /// Inherited: extra space added to each rendered word-separator (the
    /// collapsed " " token's advance, CSS Text §7.1). `None` = `normal`
    /// (no extra); negative values tighten.
    pub word_spacing: Option<f32>,
    /// Inherited: `font-variant-caps` synthesis (`small-caps`). `None` =
    /// unset (inherits as `normal`). Synthesized small caps render lowercase
    /// runs as uppercase glyphs at 70% of the font size (Blink's synthesis
    /// ratio); true capitals and caseless chars keep the full size.
    pub font_variant_caps: Option<bool>,
    /// Queryable-container marker + name (css-conditional-5). Marker only —
    /// no containment behavior is modeled.
    pub container_type: ContainerType,
    pub container_name: Option<String>,
    /// Inherited: `nowrap` disables wrapping for inline runs inside this box.
    /// `None` = `normal`.
    pub white_space: Option<WhiteSpace>,
    /// `text-overflow` (non-inherited): the marker a clipping box draws over
    /// overflowing inline content. `None` = `clip`.
    pub text_overflow: Option<TextOverflow>,
    pub text_align: Option<TextAlign>,
    /// Per-axis overflow (css-overflow-3). `None` = not declared (visible
    /// initial); the `overflow` shorthand writes both axes, so it clobbers
    /// prior longhands the way a real shorthand does. The §3.1 pair rule
    /// (visible + non-visible coerces the visible side to auto) applies on
    /// read in [`ComputedStyle::resolved_overflow`].
    pub overflow_x: Option<Overflow>,
    pub overflow_y: Option<Overflow>,
    // --- flex/grid pass-through (batch 2c): px/fr-only, non-inherited ---
    pub flex_direction: Option<FlexDirection>,
    pub flex_wrap: Option<FlexWrapMode>,
    /// Real flex/grid alignment — separate from `text_align`, which only
    /// addresses inline content (upstream keeps them distinct too).
    pub justify_content: Option<JustifyMode>,
    pub align_items: Option<AlignMode>,
    pub flex_grow: Option<f32>,
    pub flex_shrink: Option<f32>,
    /// Length-percentage (`auto` stays None — the initial value).
    pub flex_basis: Option<Length>,
    pub column_gap: Option<Length>,
    pub row_gap: Option<Length>,
    /// Track list (px / rem / fr / auto / minmax). `None` = not declared.
    pub grid_template_columns: Option<Vec<GridTrack>>,
    pub grid_template_rows: Option<Vec<GridTrack>>,
    /// `grid-template-areas` cell matrix, rows of names (`.` = null cell).
    /// Non-rectangular declarations are dropped — the spec makes them
    /// invalid — so the layout side can trust the shape.
    pub grid_template_areas: Option<Vec<Vec<String>>>,
    /// Named area an item is placed into (`grid-area: <name>`); resolves to
    /// the area's implicit `<name>-start`/`<name>-end` grid lines.
    pub grid_area: Option<String>,
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
    /// `opacity` (animation batch A), non-inherited; None = 1 (the initial
    /// value) so untouched subtrees skip alpha work entirely. The paint pass
    /// multiplies it into everything the element's subtree emits — color
    /// alphas for Bg/Border/Text, an explicit item alpha for Image/Svg/
    /// Replaced — which matches Chrome's group compositing for fades.
    pub opacity: Option<f32>,
    /// `transform` function list (animation batch B; obscura #740 lineage,
    /// widened in the affine batch): translate/translate3d/translateX/
    /// translateY, scale/scaleX/scaleY, rotate/skewX/skewY and matrix
    /// composed into one 2D affine. Percentages resolve against the
    /// element's own border-box size at collect time; the map moves paint,
    /// gBCR and hit testing but NOT the layout box (Chrome semantics:
    /// transforms never affect layout).
    pub transform: Option<Transform2D>,
    /// Parsed `animation` shorthand (CSS animations batch). The cascade
    /// only stores it; `sample_css_animation` resolves it against the
    /// stylesheet's `@keyframes` table at a time the caller supplies, then
    /// overrides opacity/transform/stroke-dashoffset in place.
    pub animation: Option<AnimationSpec>,
    /// Parsed `transition` shorthand (CSS transitions batch). Like the
    /// animation shorthand the cascade only stores it; the JS face diffs a
    /// tracked property and registers a `CssTransition` with the engine,
    /// and `sample_css_transitions` resolves registered entries against the
    /// virtual clock. Comma lists collapse to the first entry.
    pub transition: Option<TransitionSpec>,
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
    /// `background-image` longhand, kept as the raw (already var()-substituted)
    /// token stream — gradients/urls don't feed layout, so there is no parsed
    /// form; this exists so the CSSOM reports author-set images instead of
    /// the `none` initial. The `background` shorthand does NOT fill it (v1).
    pub background_image: Option<String>,
    /// `font-family`, raw author token stream (same posture as
    /// `background_image`): this slice resolves fonts at raster time from
    /// the default stack, so there is no parsed form — the field exists so
    /// the CSSOM reports the author's stack (inherited, like the spec) and
    /// downstream exporters can name real typefaces.
    pub font_family: Option<String>,
    /// `border-collapse` (table layout): collapse = adjacent cell borders
    /// merge (we realize this as zero cell gaps), separate = the HTML
    /// default 2px `border-spacing`. `None` = not declared (separate).
    pub border_collapse: Option<BorderCollapse>,
    /// `table-layout` (table layout): `fixed` = column widths come from
    /// authored sources only (colgroup/col attributes, first-row cell
    /// widths) and later-row content never widens a column; `auto` = the
    /// initial content-measured layout. `None` = not declared (auto).
    pub table_layout: Option<TableLayout>,
    /// `vertical-align` on table cells (blitz#508); None = not declared
    /// (the UA middle default applies at the cell alignment site). The
    /// valign attribute feeds the same slot as a presentational hint, so
    /// an author declaration outranks the attribute by construction.
    pub vertical_align: Option<VerticalAlign>,
    /// `text-decoration-line` declared on THIS element (never inherited —
    /// propagation to inline descendants happens at paint-collect time).
    /// `Some(empty set)` is meaningful: `text-decoration: none` on a u/s/a
    /// clears the UA decoration.
    pub text_decoration_line: Option<TextDecorations>,
    /// Custom properties (`--*`), which DO inherit: the var() substitution
    /// source. Values are stored raw (author tokens) — !important stripped at
    /// insertion; resolution to colors/lengths happens at use sites. The map
    /// is Arc-shared: inheritance bumps the refcount instead of deep-copying
    /// (a 500-var design system made that copy the dominant cost of a
    /// whole-document style pass), and a re-declaring element clones-on-write
    /// via `Arc::make_mut` at the declaration site.
    pub custom: std::sync::Arc<std::collections::HashMap<String, String>>,
    /// `content` (generated content batch): only pseudo boxes read it.
    /// `attr()` stays unresolved here — the host's attributes live outside
    /// this module — and resolves at the cascade's visit site.
    pub content: Option<ContentValue>,
    /// `counter-reset: <name> [<int>]? ...` — non-inherited; empty vector is
    /// also what `none` parses to.
    pub counter_reset: Vec<(String, i32)>,
    /// `counter-increment: <name> [<int>]? ...` — defaults to +1 per name.
    pub counter_increment: Vec<(String, i32)>,
    /// `quotes: <pair>+` flattened `[open, close, open, close, ...]` —
    /// inherited. `None` = the UA default pair; `Some(empty)` = `quotes:
    /// none` (open-quote/close-quote render nothing).
    pub quotes: Option<Vec<String>>,
    /// Generated-content pseudos keyed on the HOST element (the cascade's
    /// visit fills them; layout synthesizes the boxes). `None` = no matching
    /// pseudo rule — the overwhelmingly common case, zero cost.
    pub pseudos: Option<Box<PseudoPair>>,
}

/// Counter style names accepted in `counter(name, style)`/`counters(...)`;
/// unknown names fail closed to `Decimal` (css-counter-styles fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CounterStyle {
    #[default]
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

/// Render one counter value. Roman is only defined for 1..=3999 and alpha
/// for >= 1; everything else (zero, negative, overflow) falls back to the
/// decimal rendering, matching Chrome's fallback posture.
pub fn format_counter_value(v: i64, style: CounterStyle) -> String {
    match style {
        CounterStyle::Decimal | CounterStyle::DecimalLeadingZero => {
            let s = v.to_string();
            if matches!(style, CounterStyle::DecimalLeadingZero)
                && (0..=9).contains(&v)
            {
                format!("0{}", s)
            } else {
                s
            }
        }
        CounterStyle::LowerAlpha | CounterStyle::UpperAlpha if v < 1 => {
            v.to_string()
        }
        CounterStyle::LowerAlpha | CounterStyle::UpperAlpha => {
            let mut s = String::new();
            let mut n = v;
            while n > 0 {
                let rem = ((n - 1) % 26) as u8;
                let c = (b'a' + rem) as char;
                s.insert(0, if matches!(style, CounterStyle::UpperAlpha) {
                    c.to_ascii_uppercase()
                } else {
                    c
                });
                n = (n - 1) / 26;
            }
            s
        }
        CounterStyle::LowerRoman | CounterStyle::UpperRoman
            if !(1..=3999).contains(&v) =>
        {
            v.to_string()
        }
        CounterStyle::LowerRoman | CounterStyle::UpperRoman => {
            const PAIRS: [(u16, &str); 13] = [
                (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"),
                (100, "c"), (90, "xc"), (50, "l"), (40, "xl"),
                (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
            ];
            let mut s = String::new();
            let mut n = v as u16;
            for (val, sym) in PAIRS {
                while n >= val {
                    s.push_str(sym);
                    n -= val;
                }
            }
            if matches!(style, CounterStyle::UpperRoman) {
                s.to_ascii_uppercase()
            } else {
                s
            }
        }
    }
}

/// `content` value on a generated-content pseudo, string forms only this
/// batch (quoted strings with CSS escapes; `attr(name)` as the unresolved
/// name; `none`/`normal` never reach here — they mean "no box").
#[derive(Debug, Clone, PartialEq)]
pub enum ContentValue {
    Str(String),
    Attr(String),
    /// `counter(name)` / `counter(name, style)`
    Counter { name: String, style: CounterStyle },
    /// `counters(name, "sep")` / `counters(name, "sep", style)`
    Counters {
        name: String,
        sep: String,
        style: CounterStyle,
    },
    OpenQuote,
    CloseQuote,
    /// `no-open-quote`/`no-close-quote`: shifts the quote depth, renders "".
    NoQuote { close: bool },
    /// A space-separated content list (`content: "§ " counter(x) ":"`).
    List(Vec<ContentValue>),
}

/// The two box-generating pseudos, each cascaded from its host element.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PseudoPair {
    pub before: Option<ComputedStyle>,
    pub after: Option<ComputedStyle>,
}

/// Parse a `content` value: a single part or a space-separated list of
/// quoted strings (CSS `\` escapes incl. `\e116` hex codepoints),
/// `attr(name)`, `counter(name[, style])`, `counters(name, "sep"[, style])`,
/// open-quote/close-quote/no-open-quote/no-close-quote, or none/normal
/// (None — no box). Unsupported function forms fail closed.
pub fn parse_content_value(value: &str) -> Option<ContentValue> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    if lower == "none" || lower == "normal" || v.is_empty() {
        return None;
    }
    let parts = parse_content_list(v)?;
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(ContentValue::List(parts)),
    }
}

/// Decode CSS string escapes: 1-6 hex digits (optionally ONE whitespace
/// terminator) → codepoint; any other char escapes itself (`\x` → x).
pub(crate) fn unescape_css_string(inner: &str) -> String {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        let mut term: Option<char> = None;
        for h in chars.by_ref() {
            if hex.len() < 6 && h.is_ascii_hexdigit() {
                hex.push(h);
            } else {
                term = Some(h);
                break;
            }
        }
        if hex.is_empty() {
            if let Some(t) = term {
                out.push(t);
            }
            continue;
        }
        if let Some(cp) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
            out.push(cp);
        }
        if let Some(t) = term {
            if !t.is_ascii_whitespace() {
                out.push(t);
            }
        }
    }
    out
}

fn parse_content_list(v: &str) -> Option<Vec<ContentValue>> {
    let ch: Vec<char> = v.chars().collect();
    let mut i = 0usize;
    let mut out: Vec<ContentValue> = Vec::new();
    while i < ch.len() {
        let c = ch[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let mut j = i + 1;
            let mut inner = String::new();
            loop {
                let d = *ch.get(j)?;
                if d == '\\' {
                    inner.push('\\');
                    inner.push(*ch.get(j + 1)?);
                    j += 2;
                    continue;
                }
                if d == quote {
                    break;
                }
                inner.push(d);
                j += 1;
            }
            out.push(ContentValue::Str(unescape_css_string(&inner)));
            i = j + 1;
            continue;
        }
        if c == '(' || c == ')' {
            return None;
        }
        // Identifier token: a bare keyword or the head of a function().
        let start = i;
        while i < ch.len() && !ch[i].is_whitespace() && ch[i] != '(' && ch[i] != ')' {
            i += 1;
        }
        let word: String = ch[start..i].iter().collect();
        if i < ch.len() && ch[i] == '(' {
            let arg_start = i + 1;
            let mut depth = 1i32;
            let mut j = arg_start;
            while j < ch.len() {
                if ch[j] == '(' {
                    depth += 1;
                } else if ch[j] == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if depth != 0 {
                return None;
            }
            let arg: String = ch[arg_start..j].iter().collect();
            out.push(parse_content_function(&word, &arg)?);
            i = j + 1;
            continue;
        }
        match word.to_ascii_lowercase().as_str() {
            "open-quote" => out.push(ContentValue::OpenQuote),
            "close-quote" => out.push(ContentValue::CloseQuote),
            "no-open-quote" => out.push(ContentValue::NoQuote { close: false }),
            "no-close-quote" => out.push(ContentValue::NoQuote { close: true }),
            _ => return None,
        }
    }
    Some(out)
}

fn parse_content_function(word: &str, arg: &str) -> Option<ContentValue> {
    let lw = word.to_ascii_lowercase();
    if lw == "attr" {
        let name = arg.trim();
        if !is_valid_counter_name(name) {
            return None;
        }
        return Some(ContentValue::Attr(name.to_string()));
    }
    if lw != "counter" && lw != "counters" {
        return None;
    }
    let parts = split_top_level_commas(arg);
    let name = parts.first()?.trim().to_string();
    if !is_valid_counter_name(&name) {
        return None;
    }
    let is_counter = lw == "counter";
    match parts.len() {
        1 if is_counter => Some(ContentValue::Counter {
            name,
            style: CounterStyle::Decimal,
        }),
        2 if is_counter => Some(ContentValue::Counter {
            name,
            style: parse_counter_style(parts[1].trim()),
        }),
        2 => Some(ContentValue::Counters {
            name,
            sep: parse_counter_separator(parts[1].trim())?,
            style: CounterStyle::Decimal,
        }),
        3 => Some(ContentValue::Counters {
            name,
            sep: parse_counter_separator(parts[1].trim())?,
            style: parse_counter_style(parts[2].trim()),
        }),
        _ => None,
    }
}

/// Quoted separator (`counters(x, ".")`) or a bare word (`counters(x, -)`).
fn parse_counter_separator(tok: &str) -> Option<String> {
    let bytes = tok.as_bytes();
    if bytes.first() == Some(&b'"') || bytes.first() == Some(&b'\'') {
        let quote = tok.chars().next()?;
        let inner = tok.strip_prefix(quote)?.strip_suffix(quote)?;
        return Some(unescape_css_string(inner));
    }
    if tok.is_empty() || tok.chars().any(char::is_whitespace) {
        return None;
    }
    Some(tok.to_string())
}

/// Known names map to their styles; anything else is a valid declaration
/// whose used style falls back to decimal (css-counter-styles posture).
fn parse_counter_style(tok: &str) -> CounterStyle {
    match tok.to_ascii_lowercase().as_str() {
        "decimal-leading-zero" => CounterStyle::DecimalLeadingZero,
        "lower-alpha" => CounterStyle::LowerAlpha,
        "upper-alpha" => CounterStyle::UpperAlpha,
        "lower-roman" => CounterStyle::LowerRoman,
        "upper-roman" => CounterStyle::UpperRoman,
        _ => CounterStyle::Decimal,
    }
}

/// <custom-ident> shape for counter and attr() names: non-empty, no
/// leading digit, alphanumeric/-/_ only.
fn is_valid_counter_name(s: &str) -> bool {
    !s.is_empty()
        && !s.chars().next().unwrap().is_ascii_digit()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// `counter-reset`/`counter-increment`: `none` → empty; a sequence of
/// `<name> [<int>]?` pairs (increment defaults to +1). Malformed → None
/// (the declaration is dropped).
pub fn parse_counter_modifiers(value: &str, is_increment: bool) -> Option<Vec<(String, i32)>> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let toks: Vec<&str> = v.split_whitespace().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let name = toks[i];
        if !is_valid_counter_name(name) {
            return None;
        }
        let delta = match toks.get(i + 1) {
            Some(t) if t.parse::<i32>().is_ok() => {
                i += 1;
                t.parse::<i32>().ok()?
            }
            _ if is_increment => 1,
            _ => 0,
        };
        out.push((name.to_string(), delta));
        i += 1;
    }
    Some(out)
}

/// `quotes: none` → Some(empty); `<open> <close> [...]` pairs → flattened
/// list. Unpaired trailing value → None (declaration dropped).
pub fn parse_quotes(value: &str) -> Option<Vec<String>> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let ch: Vec<char> = v.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < ch.len() {
        if ch[i].is_whitespace() {
            i += 1;
            continue;
        }
        let quote = ch[i];
        if quote != '"' && quote != '\'' {
            return None;
        }
        let mut j = i + 1;
        let mut inner = String::new();
        loop {
            let d = *ch.get(j)?;
            if d == '\\' {
                inner.push('\\');
                inner.push(*ch.get(j + 1)?);
                j += 2;
                continue;
            }
            if d == quote {
                break;
            }
            inner.push(d);
            j += 1;
        }
        out.push(unescape_css_string(&inner));
        i = j + 1;
    }
    if out.len() % 2 != 0 || out.is_empty() {
        return None;
    }
    Some(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    Static,
    Relative,
    Absolute,
    Fixed,
    /// In-flow like relative, but the insets are STICK THRESHOLDS against
    /// the scrollport, not offsets: layout places the box in flow (taffy
    /// Relative, insets forced AUTO at the taffy boundary), and readers
    /// paint/measure the box shifted by a scroll-dependent amount clamped
    /// to the containing block (CSS Position 3; root-scroller v1).
    Sticky,
}

/// `box-sizing`: which box edge an authored width/height/min/max measures to.
/// CSS's initial is ContentBox; the near-universal `*, *::before, *::after
/// { box-sizing: border-box }` reset is why real pages declare BorderBox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSizing {
    ContentBox,
    BorderBox,
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
    /// Logical keywords (CSS Logical Properties): inline-start/end resolve to
    /// left/right in this engine's LTR-only writing mode.
    InlineStart,
    InlineEnd,
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

/// `border-collapse` (table layout). The layout engine maps Collapse to
/// zero cell gaps and Separate to the 2px UA `border-spacing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderCollapse {
    Collapse,
    Separate,
}

/// `table-layout` (table layout). Auto is the initial content-measured
/// algorithm; Fixed pins columns from authored widths only — a column never
/// grows past its authored width for content, and the remaining width
/// splits equally over the auto columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLayout {
    Auto,
    Fixed,
}

/// `vertical-align` modeled subset. On table cells (blitz#508)
/// top/middle/bottom move the cell's content within a taller cell; baseline
/// behaves like top for the flex-column cell model. On inline content
/// sub/super are baseline shifts the layout side resolves per text leaf.
/// Lengths resolve to px at declaration time (em against the element's own
/// font-size), percentages keep their % — the spec raises/lowers by a
/// percentage of the element's own line-height, only known at layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
    Baseline,
    Sub,
    Super,
    /// Authored length in px, up-positive (CSS: positive raises the box).
    Length(f32),
    /// Authored percentage, up-positive (CSS: % of the element's line-height).
    Percent(f32),
}

/// `text-decoration-line` keyword set (CSS Text Decoration 3): which lines
/// paint. The property PROPAGATES to inline descendants rather than
/// inheriting (CSS 2.1 §16.3.1) — the layout side resolves each text leaf's
/// used value by walking its ancestor chain and unioning these sets, so the
/// cascade here only records what the element itself declared. Color/style/
/// thickness legs of the shorthand parse-and-drop: lines always paint solid
/// in the text's own color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDecorations {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

impl TextDecorations {
    pub fn is_empty(&self) -> bool {
        !self.underline && !self.overline && !self.line_through
    }
    pub fn union(self, other: Self) -> Self {
        Self {
            underline: self.underline || other.underline,
            overline: self.overline || other.overline,
            line_through: self.line_through || other.line_through,
        }
    }
}

/// Shared grammar for `text-decoration-line` (shorthand=false) and the
/// `text-decoration` shorthand (true: style keywords, colors and thickness
/// tokens are accepted and dropped). `none` clears the set; an unknown token
/// invalidates the whole declaration (None) like any CSS parse error, so a
/// bogus value never strips a UA decoration by accident.
fn parse_text_decoration(v: &str, shorthand: bool) -> Option<TextDecorations> {
    let mut out = TextDecorations::default();
    let mut saw_line = false;
    for tok in v.split_whitespace() {
        let t = tok.to_ascii_lowercase();
        match t.as_str() {
            "underline" => {
                out.underline = true;
                saw_line = true;
            }
            "overline" => {
                out.overline = true;
                saw_line = true;
            }
            "line-through" => {
                out.line_through = true;
                saw_line = true;
            }
            "none" => {
                if !saw_line {
                    out = TextDecorations::default();
                    saw_line = true;
                }
            }
            "solid" | "double" | "dotted" | "dashed" | "wavy" if shorthand => {}
            _ if shorthand
                && (parse_color(&t).is_some()
                    || parse_css_length(&t).is_some()
                    || t == "auto"
                    || t == "from-font") => {}
            _ => return None,
        }
    }
    if saw_line { Some(out) } else { None }
}

/// What a border-style token means: not a style keyword at all, an explicit
/// no-border, or a line style.
enum BorderStyleKw {
    NotAStyle,
    NoBorder,
    Line(BorderStyle),
}

impl ComputedStyle {
    /// css-overflow-3 §3.1 pair rule: when one axis is `visible` and the
    /// other is not, the visible side computes to `auto`. Chrome reports the
    /// coerced pair (body{overflow-x:hidden} → y reads "auto"), so the
    /// getComputedStyle face and the internal consumers both read through
    /// this.
    pub fn resolved_overflow(&self) -> (Overflow, Overflow) {
        let x = self.overflow_x.unwrap_or(Overflow::Visible);
        let y = self.overflow_y.unwrap_or(Overflow::Visible);
        match (x, y) {
            (Overflow::Visible, o) if o != Overflow::Visible => (Overflow::Auto, o),
            (o, Overflow::Visible) if o != Overflow::Visible => (o, Overflow::Auto),
            pair => pair,
        }
    }

    /// The single-value merge the coarse internal consumers classify with
    /// (any-axis clip, viewport propagation): the strongest axis wins
    /// (scroll > auto > hidden > clip > visible).
    pub fn effective_overflow(&self) -> Overflow {
        let (x, y) = self.resolved_overflow();
        let rank = |o: Overflow| match o {
            Overflow::Scroll => 4,
            Overflow::Auto => 3,
            Overflow::Hidden => 2,
            Overflow::Clip => 1,
            Overflow::Visible => 0,
        };
        if rank(x) >= rank(y) {
            x
        } else {
            y
        }
    }

    /// True when either axis clips descendants' paint (the paint clip gates
    /// read through this).
    pub fn clips_descendants(&self) -> bool {
        let (x, y) = self.resolved_overflow();
        x != Overflow::Visible || y != Overflow::Visible
    }
}

/// Keyword serialization for [`Overflow`] (the getComputedStyle face).
pub fn overflow_name(o: Overflow) -> &'static str {
    match o {
        Overflow::Visible => "visible",
        Overflow::Hidden => "hidden",
        Overflow::Clip => "clip",
        Overflow::Scroll => "scroll",
        Overflow::Auto => "auto",
    }
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

/// `white-space`. `normal` collapses whitespace runs and wraps; `nowrap`
/// collapses but never wraps. The `pre` family preserves: `pre` never wraps
/// at all, `pre-wrap` wraps with hanging spaces, `break-spaces` lets every
/// preserved space end a line (spaces never hang), `pre-line` collapses
/// spaces but keeps newlines as hard breaks. Honored end to end for
/// pure-text runs — mixed inline runs keep normal collapsing, the same
/// limitation `nowrap` already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Nowrap,
    Pre,
    PreWrap,
    PreLine,
    BreakSpaces,
}

impl WhiteSpace {
    /// Modes with no soft wrap opportunities at all: the run measures and
    /// paints as one line per hard break only.
    pub fn no_soft_wrap(self) -> bool {
        matches!(self, WhiteSpace::Nowrap | WhiteSpace::Pre)
    }

    /// Modes that keep whitespace characters verbatim (no trim, no run
    /// collapsing). `pre-line` is NOT here: it collapses spaces.
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces)
    }

    /// Modes that render newline characters as line breaks.
    pub fn preserves_newlines(self) -> bool {
        !matches!(self, WhiteSpace::Normal | WhiteSpace::Nowrap)
    }
}

/// `text-overflow` (non-inherited). Only meaningful on a box that clips
/// (`overflow` non-visible); the marker itself is applied at paint/raster
/// time — layout rects, scroll extents and selection keep the full text,
/// per spec. The `<string>` form is a later batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOverflow {
    Clip,
    Ellipsis,
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

/// One grid track sizing. `1fr` / `100px` / `25%` / `auto` / `minmax(a, b)`
/// — repeat() expands away at parse time, so it never reaches this enum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridTrack {
    Fr(f32),
    Px(f32),
    Percent(f32),
    Auto,
    /// minmax(min, max); the two arguments are shallow (nested minmax is
    /// invalid CSS), which also keeps the enum non-recursive.
    MinMax { min: TrackSize, max: TrackSize },
}

/// A minmax() argument — one of the simple sizings, never another minmax.
/// Percent resolves against the grid container's content box at layout
/// time, so the cascade stores the raw number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackSize {
    Fr(f32),
    Px(f32),
    Percent(f32),
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
    InlineBlock,
    Flex,
    Grid,
    /// Table family (batch: table layout). The three inner roles drive the
    /// layout engine's table branch; `Display::Table` on the element means
    /// "shrink-to-fit grid of rows", not a block.
    Table,
    TableRow,
    TableCell,
    None,
}

impl Default for Display {
    fn default() -> Self {
        Display::Block
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);

/// An SVG paint value (svg v1): a resolved color, or `currentColor` which
/// stays symbolic through the cascade and resolves against the element's
/// inherited `color` when the svg compiler flattens the subtree. "none" is
/// stored as a fully transparent color — both mean "draw nothing".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SvgPaint {
    Color(Color),
    CurrentColor,
}

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
    /// A calc() mixing percent and px (`calc(100% - 30px)`), kept symbolic:
    /// percent in CSS 0-100 scale resolves against the containing block in
    /// the layout engine's post-layout repair pass (width/min/max only —
    /// other slots keep the percent-only placeholder or drop, see
    /// to_taffy_style). The px part is always exact here.
    Calc { percent: f32, px: f32 },
    /// CSS `auto` - legal ONLY for margins (the horizontal centering idiom
    /// and the abspos §10.3.3 centering pattern). Padding and borders reject
    /// it at parse time; layout maps it to taffy's
    /// `LengthPercentageAuto::auto()`, which implements both the in-flow
    /// auto-margin expansion and abspos auto-margin resolution.
    Auto,
    /// CSS sizing keywords, accepted only in the width/min-width/max-width
    /// slots. taffy's Dimension has no intrinsic keyword, so layout resolves
    /// them against the subtree's intrinsic width with measure passes at
    /// build time (`resolve_sizing_keywords`); every other slot keeps them
    /// unreachable and maps them like `Auto` defensively.
    MinContent,
    MaxContent,
    FitContent,
}

/// The 2D affine `transform` this pipeline computes (CSS matrix
/// convention): x' = a·x + c·y + tx, y' = b·x + d·y + ty. The translate
/// stays a symbolic Length — percentages resolve against the element's own
/// border box at collect time. Function lists accumulate in CSS order (the
/// first-listed function is the outermost map).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: Length,
    pub ty: Length,
}

impl Transform2D {
    /// Whether the linear part is diagonal (translate/scale only, no
    /// rotate/skew): the layout collect walk keeps pre-baking geometry into
    /// item fields on that path; anything else flips the element's paint
    /// items into local coordinates bracketed by SetXf/ClearXf.
    pub fn is_axis_aligned(&self) -> bool {
        self.b == 0.0 && self.c == 0.0
    }

    /// Concrete [a b c d e f] affine. Percentage translates resolve against
    /// the reference size the caller supplies (0 when unknown — SVG subtree
    /// consumers pass the viewBox extent). Non-px/percent translates can't
    /// appear in a valid transform, so they resolve to 0.
    pub fn to_matrix_with(self, ref_w: f32, ref_h: f32) -> [f32; 6] {
        let px = |l: Length, r: f32| match l {
            Length::Px(v) => v,
            Length::Percent(p) => p * r / 100.0,
            _ => 0.0,
        };
        [self.a, self.b, self.c, self.d, px(self.tx, ref_w), px(self.ty, ref_h)]
    }
}

/// Parsed `animation` shorthand, v1 subset: ONE animation (the first of a
/// comma list), direction normal, play-state running. That covers the
/// declarative-SVG motion grammar (fade / rise / self-draw); the sampler
/// cycles through `iterations` iterations and holds the element at the final
/// iteration's state once past `delay + duration * iterations` when fill-mode
/// is `forwards`/`both`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimationSpec {
    pub name: String,
    /// Seconds; 0 means the animation snaps (only end states ever visible).
    pub duration: f32,
    /// Seconds before the animation starts; before it the element shows its
    /// underlying cascade value (fill-mode none semantics).
    pub delay: f32,
    pub easing: Easing,
    pub fill_forwards: bool,
    /// Iteration count; `f32::INFINITY` for `infinite`. Non-integer counts
    /// are legal (the active duration ends mid-iteration). The sampler loops
    /// per-iteration (easing restarts each cycle) and the after-phase fill
    /// holds the fractional-final-iteration state.
    pub iterations: f32,
}

/// Timing functions the sampler evaluates. Keywords map to their canonical
/// cubic-bezier control points; `steps()` invalidates the declaration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Easing {
    Linear,
    CubicBezier(f32, f32, f32, f32),
}

/// One comma entry of the `transition` shorthand, shared shape with
/// AnimationSpec: `transition-property` (`None` = all), duration and delay
/// in seconds, timing function. Like animation lists, a comma-separated
/// transition list is tracked by its first entry only.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub property: Option<String>,
    pub duration: f32,
    pub delay: f32,
    pub easing: Easing,
}

/// `linear` / `ease` family keywords and `cubic-bezier(...)`. `steps()` and
/// other timing functions we don't evaluate return None (the caller
/// invalidates the whole declaration rather than animating with the wrong
/// curve). Shared by the animation and transition shorthands, and by the
/// `add_css_transition` op (the JS face passes the easing token through).
pub(crate) fn parse_easing_token(tok: &str) -> Option<Easing> {
    match tok {
        "linear" => Some(Easing::Linear),
        "ease" => Some(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        "ease-in" => Some(Easing::CubicBezier(0.42, 0.0, 1.0, 1.0)),
        "ease-out" => Some(Easing::CubicBezier(0.0, 0.0, 0.58, 1.0)),
        "ease-in-out" => Some(Easing::CubicBezier(0.42, 0.0, 0.58, 1.0)),
        _ => {
            let rest = tok.strip_prefix("cubic-bezier(")?;
            let args: Vec<Option<f32>> = rest
                .strip_suffix(')')?
                .split(',')
                .map(|a| a.trim().parse::<f32>().ok())
                .collect();
            let pts: Option<Vec<f32>> = args.into_iter().collect();
            let pts = pts?;
            if pts.len() == 4 && pts.iter().all(|p| p.is_finite()) {
                Some(Easing::CubicBezier(pts[0], pts[1], pts[2], pts[3]))
            } else {
                None
            }
        }
    }
}

/// `transition` shorthand: `<property> <duration> <easing> <delay>` in any
/// order, times by position (first time = duration, second = delay).
/// `transition-property: none` disables the entry (returns None); `steps()`
/// invalidates the whole shorthand. Comma lists collapse to the first entry.
fn parse_transition_shorthand(v: &str) -> Option<TransitionSpec> {
    let mut duration: Option<f32> = None;
    let mut delay = 0.0f32;
    let mut easing: Option<Easing> = None;
    let mut property: Option<String> = None;
    for tok in paren_aware_tokens(v) {
        let secs = tok
            .strip_suffix("ms")
            .and_then(|n| n.parse::<f32>().ok().map(|n| n / 1000.0))
            .or_else(|| tok.strip_suffix('s').and_then(|n| n.parse::<f32>().ok()));
        if let Some(s) = secs.filter(|s| s.is_finite() && *s >= 0.0) {
            if duration.is_none() {
                duration = Some(s);
            } else if delay == 0.0 {
                delay = s;
            }
            continue;
        }
        if let Some(e) = parse_easing_token(&tok) {
            easing = Some(e);
            continue;
        }
        if tok.contains('(') {
            return None;
        }
        if tok.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            && !tok.is_empty()
        {
            match property {
                None if tok == "none" => return None,
                None => property = Some(tok),
                _ => {}
            }
        }
    }
    Some(TransitionSpec {
        property,
        duration: duration.unwrap_or(0.0),
        delay,
        easing: easing.unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
    })
}

/// The interpolable value set: opacity scalars, RGBA colors, and 2D
/// affines (batch 93; the affine lerps per component via `lerp_transform`,
/// the same path CSS keyframe animations use).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransitionValue {
    Opacity(f32),
    Color([f32; 4]),
    Transform(Option<Transform2D>),
}

impl TransitionValue {
    /// Pairwise lerp. Mismatched kinds (never produced by the register
    /// op, which snapshots one property) yield None and the caller snaps
    /// to the end value.
    pub fn lerp(a: TransitionValue, b: TransitionValue, p: f32) -> Option<TransitionValue> {
        match (a, b) {
            (TransitionValue::Opacity(x), TransitionValue::Opacity(y)) => {
                Some(TransitionValue::Opacity(x + (y - x) * p))
            }
            (TransitionValue::Color(x), TransitionValue::Color(y)) => {
                let mix = |i: usize| x[i] + (y[i] - x[i]) * p;
                Some(TransitionValue::Color([mix(0), mix(1), mix(2), mix(3)]))
            }
            (TransitionValue::Transform(x), TransitionValue::Transform(y)) => {
                lerp_transform(x, y, p).map(|t| TransitionValue::Transform(Some(t)))
            }
            _ => None,
        }
    }
}

/// One registered property transition (CSS transitions batch): the JS
/// face detects a tracked-property style write, snapshots both computed
/// values, and registers one of these through the `add_css_transition`
/// op. `start` is page-timeline seconds; the /video path collapses it to
/// 0 so clips play transitions from their outset.
#[derive(Debug, Clone)]
pub struct CssTransition {
    pub nid: usize,
    pub property: String,
    pub from: TransitionValue,
    pub to: TransitionValue,
    pub start: f64,
    pub duration: f32,
    pub delay: f32,
    pub easing: Easing,
}

/// Exit-phase transition sampling (CSS transitions batch). Runs once per
/// style pass after the cascade visit: registered entries override the
/// resolved values. Per-node sampling inside the visit would instead let
/// a transitioning parent drag its whole subtree through the interpolated
/// value (children cascade the parent) — v1 collapses that corner and
/// keeps children on the pre-transition parent value. `t = None` is the
/// live path and no-ops: the style write already carries the end state,
/// so the cascade shows it and a registered entry would only re-derive
/// the same numbers.
pub fn sample_css_transitions(
    transitions: &[CssTransition],
    t: Option<f64>,
    styles: &mut std::collections::HashMap<crate::diting_dom::NodeId, ComputedStyle>,
) {
    let Some(t) = t else { return };
    for tr in transitions {
        let Some(cs) = styles.get_mut(&crate::diting_dom::NodeId(tr.nid as u32)) else {
            continue;
        };
        let elapsed = t - tr.start - tr.delay as f64;
        let p = if tr.duration <= 0.0 {
            if elapsed >= 0.0 {
                1.0
            } else {
                0.0
            }
        } else {
            ((elapsed / tr.duration as f64).clamp(0.0, 1.0)) as f32
        };
        let eased = tr.easing.map(p);
        let Some(v) = TransitionValue::lerp(tr.from, tr.to, eased) else {
            continue;
        };
        match (tr.property.as_str(), v) {
            ("opacity", TransitionValue::Opacity(o)) => cs.opacity = Some(o),
            ("color", TransitionValue::Color(c)) => {
                cs.color = Some(to_color(c));
            }
            ("background-color", TransitionValue::Color(c)) => {
                cs.background_color = Some(to_color(c));
            }
            ("transform", TransitionValue::Transform(m)) => {
                cs.transform = m;
            }
            _ => {}
        }
    }
}

fn to_color(c: [f32; 4]) -> Color {
    let ch = |x: f32| -> u8 { (x.round().clamp(0.0, 255.0)) as u8 };
    Color(ch(c[0]), ch(c[1]), ch(c[2]), ch(c[3]))
}

impl Easing {
    /// Map linear progress [0, 1] through the curve. `ease`-family curves
    /// stay monotone in x, so one Newton pass with a bisection fallback
    /// solves the parameter; a few iterations suffice at f32 precision.
    pub fn map(&self, p: f32) -> f32 {
        match *self {
            Easing::Linear => p,
            Easing::CubicBezier(x1, y1, x2, y2) => {
                let x = p.clamp(0.0, 1.0);
                let bezier = |t: f32, a: f32, b: f32| {
                    // Bernstein form of 3(1-t)^2 t a + 3(1-t) t^2 b + t^3
                    let u = 1.0 - t;
                    3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
                };
                let mut t = x;
                let mut solved = false;
                for _ in 0..8 {
                    let xt = bezier(t, x1, x2) - x;
                    if xt.abs() < 1e-5 {
                        solved = true;
                        break;
                    }
                    // derivative of the x component
                    let u = 1.0 - t;
                    let d = 3.0 * u * u * x1 + 6.0 * u * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2);
                    if d.abs() < 1e-6 {
                        break;
                    }
                    t -= xt / d;
                }
                if !solved && !(0.0..=1.0).contains(&t) {
                    // Newton diverged (degenerate control points): bisect
                    let (mut lo, mut hi) = (0.0f32, 1.0f32);
                    for _ in 0..24 {
                        t = 0.5 * (lo + hi);
                        let xt = bezier(t, x1, x2);
                        if xt < x {
                            lo = t;
                        } else {
                            hi = t;
                        }
                    }
                }
                bezier(t.clamp(0.0, 1.0), y1, y2)
            }
        }
    }
}

/// Parse the `animation` shorthand (v1 single-animation subset). Time tokens
/// fill duration then delay in order; keyword/cubic-bezier tokens set the
/// easing; `forwards`/`both` set fill; the remaining identifier is the name.
/// `none` as the first non-keyword token means no animation at all.
fn parse_animation_shorthand(v: &str) -> Option<AnimationSpec> {
    let mut duration: Option<f32> = None;
    let mut delay = 0.0f32;
    let mut easing: Option<Easing> = None;
    let mut fill_forwards = false;
    let mut iterations = 1.0f32;
    let mut name: Option<String> = None;
    for tok in paren_aware_tokens(v) {
        let secs = tok
            .strip_suffix("ms")
            .and_then(|n| n.parse::<f32>().ok().map(|n| n / 1000.0))
            .or_else(|| tok.strip_suffix('s').and_then(|n| n.parse::<f32>().ok()));
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
                        .map(|a| a.trim().parse::<f32>().ok())
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

/// Split on whitespace at paren depth 0 so `cubic-bezier(.33, 1, .68, 1)`
/// stays one token.
fn paren_aware_tokens(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in v.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            _ if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Length arithmetic for transform accumulation: same-unit adds fold, a
/// zero px collapses into the other side; mixed units without a box to
/// resolve against can't compose and invalidate the whole list.
fn add_len(a: Length, b: Length) -> Option<Length> {
    match (a, b) {
        (Length::Px(x), Length::Px(y)) => Some(Length::Px(x + y)),
        (Length::Percent(x), Length::Percent(y)) => Some(Length::Percent(x + y)),
        (Length::Px(0.0), b) => Some(b),
        (a, Length::Px(0.0)) => Some(a),
        _ => None,
    }
}

/// Scale a Length by a factor: px scales numerically, and a percent of the
/// box times the factor is the same percent scaled (w·f·s == Percent(f·s)).
fn mul_len(a: Length, s: f32) -> Length {
    match a {
        Length::Px(x) => Length::Px(x * s),
        Length::Percent(f) => Length::Percent(f * s),
        other => other,
    }
}

/// Parse a `transform` declaration into one 2D affine [`Transform2D`]
/// (transform batch; obscura #740 lineage, widened twice). The translate
/// family (translate/translate3d — z parses and drops, 3D flattens to 2D
/// here — /translateX/translateY) and the scale family accumulate exactly;
/// rotate/skewX/skewY/matrix join the same composition. The GSAP tween
/// output this exists for writes deg/rad angles and full `matrix(…)` forms.
/// `none` and anything unparsable yield None — spec: one unknown function
/// invalidates the whole declaration, so the element renders untransformed.
/// component keeps symbolic lengths; the transition register op reuses this
/// so a before/after snapshot matches the sampler's affine.
/// Standalone callers (svg presentation attributes, @supports probes, the
/// transition register) parse without a viewport — they fold against the
/// default pair. The cascade threads the page's real viewport through
/// `parse_transform_with_vp` so `translateX(100vw)` — the deck/carousel
/// idiom — resolves to actual pixels.
pub(crate) fn parse_transform(v: &str) -> Option<Transform2D> {
    parse_transform_with_vp(v, DEFAULT_VIEWPORT.0, DEFAULT_VIEWPORT.1)
}

pub(crate) fn parse_transform_with_vp(v: &str, vw: f32, vh: f32) -> Option<Transform2D> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return None; // `none` is the initial value → no transform
    }
    let one = |s: &str| -> Option<Length> {
        let s = s.trim();
        if let Some(num) = s.strip_suffix('%') {
            num.trim().parse::<f32>().ok().map(Length::Percent)
        } else if let Some(num) = s.strip_suffix("px") {
            num.trim().parse::<f32>().ok().map(Length::Px)
        } else if let Some(num) = s.strip_suffix("vw") {
            // Viewport units fold to px at parse time (batch 162 resolve_len
            // posture) — the translate slots are px/percent only.
            num.trim().parse::<f32>().ok().map(|n| Length::Px(n * vw / 100.0))
        } else if let Some(num) = s.strip_suffix("vh") {
            num.trim().parse::<f32>().ok().map(|n| Length::Px(n * vh / 100.0))
        } else {
            // `0` is a legal bare length; other unitless values are not.
            (s == "0").then_some(Length::Px(0.0))
        }
    };
    // Percentage/calc translate slots can't ride a rotated linear part (the
    // vector map mixes axes) — only a fully resolved px composes there.
    let px_only = |l: Length| match l {
        Length::Px(v) => Some(v),
        _ => None,
    };
    let num = |s: &str| -> Option<f32> {
        let n = s.trim().parse::<f32>().ok()?;
        n.is_finite().then_some(n)
    };
    // Angle in CSS units: deg (the default suffix real sheets write), rad,
    // turn, grad. Bare nonzero numbers are invalid CSS (Chrome rejects).
    let angle = |s: &str| -> Option<f32> {
        let s = s.trim();
        let (n, scale) = if let Some(n) = s.strip_suffix("deg") {
            (n, 1.0f32)
        } else if let Some(n) = s.strip_suffix("rad") {
            (n, 180.0 / std::f32::consts::PI)
        } else if let Some(n) = s.strip_suffix("turn") {
            (n, 360.0)
        } else if let Some(n) = s.strip_suffix("grad") {
            (n, 0.9)
        } else if s == "0" {
            ("0", 1.0)
        } else {
            return None;
        };
        let deg = n.trim().parse::<f32>().ok()? * scale;
        deg.is_finite().then_some(deg)
    };
    let mut t = Transform2D {
        a: 1.0, b: 0.0, c: 0.0, d: 1.0,
        tx: Length::Px(0.0),
        ty: Length::Px(0.0),
    };
    // Walking left to right, each new function sits INNERMOST (CSS composes
    // the list first-to-last as outermost-to-innermost): M_new = M_old ∘ F.
    // Linear parts multiply; a translate maps through the linear part
    // accumulated so far (exactly the translate-scaled-by-prior-scale rule
    // the axis-aligned slice pinned).
    let compose_linear = |t: &mut Transform2D, fa: f32, fb: f32, fc: f32, fd: f32| {
        let (a, b, c, d) = (t.a, t.b, t.c, t.d);
        t.a = a * fa + c * fb;
        t.b = b * fa + d * fb;
        t.c = a * fc + c * fd;
        t.d = b * fc + d * fd;
    };
    let compose_translate = |t: &mut Transform2D, x: Length, y: Length| -> Option<()> {
        if t.is_axis_aligned() {
            // Diagonal keeps the slots independent (the historical
            // mul_len/add_len semantics, percent included).
            t.tx = add_len(mul_len(x, t.a), t.tx)?;
            t.ty = add_len(mul_len(y, t.d), t.ty)?;
        } else {
            // Full vector map: both slots must be px.
            let (px, py) = (px_only(x)?, px_only(y)?);
            t.tx = add_len(Length::Px(t.a * px + t.c * py), t.tx)?;
            t.ty = add_len(Length::Px(t.b * px + t.d * py), t.ty)?;
        }
        Some(())
    };
    let mut rest = v;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (name, after) = rest.split_once('(')?;
        let (args, tail) = after.split_once(')')?;
        let name = name.trim();
        let parts: Vec<&str> = args.split(',').collect();
        match name {
            "translate" | "translate3d" => {
                let (a, b) = match parts.as_slice() {
                    [x] => (one(x)?, Length::Px(0.0)), // translate(x) == translate(x, 0)
                    [x, y] => (one(x)?, one(y)?),
                    [x, y, z] => {
                        // 3-arg form is translate3d; z must parse as a
                        // length (GSAP writes "0px") and is then dropped —
                        // this engine has no z axis.
                        one(z)?;
                        (one(x)?, one(y)?)
                    }
                    _ => return None,
                };
                compose_translate(&mut t, a, b)?;
            }
            "translateX" => {
                let [x] = parts.as_slice() else { return None };
                let a = one(x)?;
                if t.is_axis_aligned() {
                    compose_translate(&mut t, a, Length::Px(0.0))?;
                } else {
                    let v = px_only(a)?;
                    t.tx = add_len(Length::Px(t.a * v), t.tx)?;
                    t.ty = add_len(Length::Px(t.b * v), t.ty)?;
                }
            }
            "translateY" => {
                let [y] = parts.as_slice() else { return None };
                let b = one(y)?;
                if t.is_axis_aligned() {
                    compose_translate(&mut t, Length::Px(0.0), b)?;
                } else {
                    let v = px_only(b)?;
                    t.tx = add_len(Length::Px(t.c * v), t.tx)?;
                    t.ty = add_len(Length::Px(t.d * v), t.ty)?;
                }
            }
            "scale" => {
                let (fsx, fsy) = match parts.as_slice() {
                    [x] => {
                        let s = num(x)?;
                        (s, s) // scale(s) == scale(s, s)
                    }
                    [x, y] => (num(x)?, num(y)?),
                    _ => return None,
                };
                compose_linear(&mut t, fsx, 0.0, 0.0, fsy);
            }
            "scaleX" | "scaleY" => {
                let [x] = parts.as_slice() else { return None };
                let s = num(x)?;
                if name == "scaleX" {
                    compose_linear(&mut t, s, 0.0, 0.0, 1.0);
                } else {
                    compose_linear(&mut t, 1.0, 0.0, 0.0, s);
                }
            }
            "rotate" => {
                let [arg] = parts.as_slice() else { return None };
                let rad = angle(arg)?.to_radians();
                let (s, c) = (rad.sin(), rad.cos());
                compose_linear(&mut t, c, s, -s, c);
            }
            "skewX" => {
                let [arg] = parts.as_slice() else { return None };
                compose_linear(&mut t, 1.0, 0.0, angle(arg)?.to_radians().tan(), 1.0);
            }
            "skewY" => {
                let [arg] = parts.as_slice() else { return None };
                compose_linear(&mut t, 1.0, angle(arg)?.to_radians().tan(), 0.0, 1.0);
            }
            "matrix" => {
                let [fa, fb, fc, fd, fe, ff] = parts.as_slice() else { return None };
                let (fa, fb, fc, fd) = (num(fa)?, num(fb)?, num(fc)?, num(fd)?);
                let (fe, ff) = (num(fe)?, num(ff)?);
                // e/f are plain numbers (px in matrix units) that map through
                // the OLD accumulated linear — F rides innermost, so
                // (M∘F).t = L_M·t_F + t_M — which means the translate must
                // compose BEFORE the linear update, while t still holds L_M.
                compose_translate(&mut t, Length::Px(fe), Length::Px(ff));
                compose_linear(&mut t, fa, fb, fc, fd);
            }
            _ => return None,
        }
        rest = tail;
    }
    Some(t)
}

/// Declaration-level length: em/rem can't resolve until the font context is
/// known, so parsing keeps them symbolic for the cascade to fold in. vw/vh
/// keep the same treatment against the viewport pair on [`FontCtx`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CssLength {
    Px(f32),
    Em(f32),
    Rem(f32),
    Percent(f32),
    Vw(f32),
    Vh(f32),
}

/// `line-height` computed value. Inherited as-is: the number form keeps its
/// multiplier (spec: computed value of a unitless line-height IS the number,
/// resolved against each descendant's own font-size), lengths are already
/// absolute px. `Normal` is the initial value (layout maps it to the same
/// `font-size * 1.2` blitz pins for `normal`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeightSpec {
    Normal,
    /// Unitless multiplier (and `%`, folded at parse time — a percentage
    /// resolves against the element's own font-size, exactly like a number).
    Number(f32),
    Px(f32),
}

/// Shape-level parse for @supports probing and cascade application; em/rem
/// fold to px later against the element's FontCtx.
enum LineHeightRaw {
    Normal,
    Number(f32),
    Len(CssLength),
}

fn parse_line_height(v: &str) -> Option<LineHeightRaw> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("normal") {
        return Some(LineHeightRaw::Normal);
    }
    // Unitless numbers are legal ONLY for line-height (line-height: 1.6).
    if let Ok(n) = v.parse::<f32>() {
        let numeric_shape = v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-');
        return (n.is_finite() && n > 0.0 && numeric_shape).then_some(LineHeightRaw::Number(n));
    }
    parse_css_length(v).map(LineHeightRaw::Len)
}

/// The initial / default root font size (CSS `medium`).
pub const DEFAULT_ROOT_FONT_SIZE: f32 = 16.0;

/// Font context a length resolves against. `own` is this element's computed
/// font-size (font-size itself resolves em/% against the PARENT, per spec).
/// `viewport_w/h` back the viewport units (vw/vh); the default pair is the
/// 1280×720 ICB convention, and every real cascade threads the page's actual
/// viewport through [`crate::diting_layout`] seam functions.
#[derive(Debug, Clone, Copy)]
pub struct FontCtx {
    pub own: f32,
    pub root: f32,
    pub viewport_w: f32,
    pub viewport_h: f32,
}

impl Default for FontCtx {
    fn default() -> Self {
        FontCtx {
            own: DEFAULT_ROOT_FONT_SIZE,
            root: DEFAULT_ROOT_FONT_SIZE,
            viewport_w: DEFAULT_VIEWPORT.0,
            viewport_h: DEFAULT_VIEWPORT.1,
        }
    }
}

/// Standalone parse/apply contexts without a threaded viewport resolve
/// against this pair (matches the screenshot default width).
pub const DEFAULT_VIEWPORT: (f32, f32) = (1280.0, 720.0);

fn resolve_len(l: CssLength, fonts: &FontCtx) -> Length {
    match l {
        CssLength::Px(x) => Length::Px(x),
        CssLength::Em(n) => Length::Px(n * fonts.own),
        CssLength::Rem(n) => Length::Px(n * fonts.root),
        CssLength::Percent(p) => Length::Percent(p),
        CssLength::Vw(n) => Length::Px(n * fonts.viewport_w / 100.0),
        CssLength::Vh(n) => Length::Px(n * fonts.viewport_h / 100.0),
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
    // "vw" before "vh" is arbitrary (no suffix overlap like rem/em), but
    // both must precede nothing else — no other unit ends in w/h.
    if let Some(n) = v.strip_suffix("vw") {
        return n.parse::<f32>().ok().map(CssLength::Vw);
    }
    if let Some(n) = v.strip_suffix("vh") {
        return n.parse::<f32>().ok().map(CssLength::Vh);
    }
    if let Some(e) = v.strip_suffix("em") {
        return e.parse::<f32>().ok().map(CssLength::Em);
    }
    if let Some(px) = v.strip_suffix("px") {
        return px.parse::<f32>().ok().map(CssLength::Px);
    }
    None
}

/// A calc() intermediate value: a plain number (only legal as a `*`/`/`
/// operand) or a length carrying its px and percent parts accumulated
/// independently (percent in CSS 0-100 scale).
#[derive(Debug, Clone, Copy)]
enum CalcVal {
    Number(f32),
    Len { px: f32, percent: f32 },
}

impl CalcVal {
    fn parts(self) -> (f32, f32) {
        match self {
            CalcVal::Len { px, percent } => (px, percent),
            CalcVal::Number(_) => (0.0, 0.0),
        }
    }
}

/// Evaluate a calc() expression body (the text BETWEEN the parentheses) into
/// a [`Length`]: pure px arithmetic folds to [`Length::Px`], percent-only to
/// [`Length::Percent`], and a mix to [`Length::Calc`]. em/rem fold against
/// the font context like bare declarations. The grammar is the CSS one —
/// `+`/`-` need whitespace on both sides, `*`/`/` take one number operand,
/// parentheses and nested calc() group.
pub fn eval_calc(expr: &str, fonts: &FontCtx) -> Option<Length> {
    let mut p = CalcParser { s: expr.as_bytes(), i: 0, fonts };
    let v = p.expr_value()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return None;
    }
    calcval_to_length(v)
}

/// Fold a fully-evaluated calc intermediate into a [`Length`].
fn calcval_to_length(v: CalcVal) -> Option<Length> {
    match v {
        // A bare number is not a length; `calc(0)` has no unit context here.
        CalcVal::Number(_) | CalcVal::Len { px: 0.0, percent: 0.0 } => None,
        CalcVal::Len { px: 0.0, percent } => Some(Length::Percent(percent)),
        CalcVal::Len { px, percent: 0.0 } => Some(Length::Px(px)),
        CalcVal::Len { px, percent } => Some(Length::Calc { percent, px }),
    }
}

/// Which math function a `min(`/`max(`/`clamp(` call reduces as. clamp keeps
/// the CSS form `max(MIN, min(VAL, MAX))`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MathFn {
    Min,
    Max,
    Clamp,
}

/// Evaluate a property-level value that is one math call: `calc(…)`,
/// `min(…)`, `max(…)` or `clamp(…)`. min/max take one or more
/// comma-separated sums; clamp takes exactly three. Comparing operands
/// needs one unit family — all-px or all-percent (percents share the
/// property's reference). A call mixing px with percent, or a number with
/// either, is unorderable at parse time and drops (declaration falls out,
/// same posture as calc's mixed-fold staying symbolic).
pub fn eval_math_call(v: &str, fonts: &FontCtx) -> Option<Length> {
    let t = v.trim();
    if t.len() >= 6 && t[..5].eq_ignore_ascii_case("calc(") && t.ends_with(')') {
        return eval_calc(&t[5..t.len() - 1], fonts);
    }
    for (prefix, f) in [("min(", MathFn::Min), ("max(", MathFn::Max), ("clamp(", MathFn::Clamp)] {
        let pl = prefix.len();
        if t.len() > pl && t[..pl].eq_ignore_ascii_case(prefix) && t.ends_with(')') {
            let mut p = CalcParser { s: &t.as_bytes()[pl..], i: 0, fonts };
            let v = p.math_args_value(f)?;
            p.skip_ws();
            if p.i != p.s.len() {
                return None;
            }
            return calcval_to_length(v);
        }
    }
    None
}

struct CalcParser<'a> {
    s: &'a [u8],
    i: usize,
    fonts: &'a FontCtx,
}

impl CalcParser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.i < self.s.len() && self.s[self.i] == c {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn starts_with_ignore_case(&self, prefix: &str) -> bool {
        let end = self.i + prefix.len();
        end <= self.s.len() && self.s[self.i..end].eq_ignore_ascii_case(prefix.as_bytes())
    }

    /// sum: term (( '+' | '-' ) term)* — both sides must carry units.
    fn expr_value(&mut self) -> Option<CalcVal> {
        let mut acc = self.term_value()?;
        loop {
            self.skip_ws();
            let plus = match self.s.get(self.i) {
                Some(b'+') => true,
                Some(b'-') => false,
                _ => break,
            };
            self.i += 1;
            let rhs = self.term_value()?;
            if matches!(acc, CalcVal::Number(_)) || matches!(rhs, CalcVal::Number(_)) {
                return None;
            }
            let (a, ap) = acc.parts();
            let (b, bp) = rhs.parts();
            acc = CalcVal::Len {
                px: if plus { a + b } else { a - b },
                percent: if plus { ap + bp } else { ap - bp },
            };
        }
        Some(acc)
    }

    /// product: factor (( '*' | '/' ) factor)*, one side a number.
    fn term_value(&mut self) -> Option<CalcVal> {
        let mut acc = self.factor_value()?;
        loop {
            self.skip_ws();
            let div = match self.s.get(self.i) {
                Some(b'*') => false,
                Some(b'/') => true,
                _ => break,
            };
            self.i += 1;
            let rhs = self.factor_value()?;
            let (n, val) = match (acc, rhs) {
                (CalcVal::Number(n), v) => (n, v),
                (v, CalcVal::Number(n)) => (n, v),
                _ => return None,
            };
            acc = match val {
                CalcVal::Len { px, percent } if !div => CalcVal::Len { px: px * n, percent: percent * n },
                CalcVal::Len { px, percent } if div && n != 0.0 => CalcVal::Len { px: px / n, percent: percent / n },
                _ => return None,
            };
        }
        Some(acc)
    }

    /// Parse and reduce the argument list of a math call (`i` sits just
    /// past the function name's `(`): sum (`,` sum)* `)`, one or more for
    /// min/max, exactly three for clamp.
    fn math_args_value(&mut self, f: MathFn) -> Option<CalcVal> {
        let mut args = Vec::new();
        loop {
            args.push(self.expr_value()?);
            if !self.eat(b',') {
                break;
            }
        }
        if !self.eat(b')') {
            return None;
        }
        if args.is_empty() || (f == MathFn::Clamp && args.len() != 3) {
            return None;
        }
        // One unit family only (see eval_math_call): all-px or all-percent.
        let all_px = args.iter().all(|a| matches!(a, CalcVal::Len { percent: 0.0, .. }));
        let all_pct = args.iter().all(|a| matches!(a, CalcVal::Len { px: 0.0, .. }));
        if !(all_px || all_pct) {
            return None;
        }
        let key = |a: &CalcVal| -> f32 {
            match a {
                CalcVal::Number(n) => *n,
                CalcVal::Len { px, percent } => {
                    if all_px {
                        *px
                    } else {
                        *percent
                    }
                }
            }
        };
        let pick = |lo: bool, a: &CalcVal, b: &CalcVal| {
            let (x, y) = (key(a), key(b));
            let take_b = if lo { y < x } else { y > x };
            if take_b { *b } else { *a }
        };
        let mut acc = args[0];
        match f {
            MathFn::Min | MathFn::Max => {
                let lo = f == MathFn::Min;
                for a in &args[1..] {
                    acc = pick(lo, &acc, a);
                }
            }
            MathFn::Clamp => {
                let (min, val, max) = (&args[0], &args[1], &args[2]);
                // max(MIN, min(VAL, MAX))
                let inner = pick(true, val, max);
                acc = pick(false, min, &inner);
            }
        }
        Some(acc)
    }

    fn factor_value(&mut self) -> Option<CalcVal> {
        self.skip_ws();
        if self.eat(b'(') {
            let v = self.expr_value()?;
            return self.eat(b')').then_some(v);
        }
        if self.starts_with_ignore_case("calc(") {
            self.i += 5;
            let v = self.expr_value()?;
            return self.eat(b')').then_some(v);
        }
        for (prefix, f) in [("min(", MathFn::Min), ("max(", MathFn::Max), ("clamp(", MathFn::Clamp)] {
            if self.starts_with_ignore_case(prefix) {
                self.i += prefix.len();
                return self.math_args_value(f);
            }
        }
        // number | length token. '+/-/"/("/")/',' end a token but may START
        // one (a signed number), so only delimit when past the first byte.
        let start = self.i;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if self.i > start
                && (c.is_ascii_whitespace() || matches!(c, b'+' | b'-' | b'*' | b'/' | b'(' | b')' | b','))
            {
                break;
            }
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if let Ok(n) = tok.parse::<f32>() {
            return n.is_finite().then_some(CalcVal::Number(n));
        }
        let resolved = parse_css_length(tok).map(|l| resolve_len(l, self.fonts))?;
        match resolved {
            Length::Px(px) => Some(CalcVal::Len { px, percent: 0.0 }),
            Length::Percent(p) => Some(CalcVal::Len { px: 0.0, percent: p }),
            _ => None,
        }
    }
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
        | "label" | "time" | "abbr" | "q"
        // The phrasing-content rest (obscura#936): mark/ins/del/big/s/u/
        // strike/tt/samp/kbd/dfn/var/cite/bdi/bdo are inline in every
        // browser UA sheet — falling to Block made a <mark> paint a
        // full-width yellow band and put each <ins>/<del>/<big> on its own
        // line. (pre/xmp/listing/plaintext stay Block.)
        | "mark" | "ins" | "del" | "big" | "s" | "u" | "strike" | "tt" | "samp" | "kbd"
        | "dfn" | "var" | "cite" | "bdi" | "bdo" => Display::Inline,
        // Form controls are inline-level per every browser UA sheet, and the
        // inline-flavor matters twice: computed-style fidelity (CSSOM) and
        // the run dispatch — inline-block replaced elements join the text
        // run as atomic boxes, which is what keeps unstyled inputs on the
        // same line as their labels (obscura#807 class). button
        // shrink-to-fits its label content.
        "input" | "textarea" | "button" => Display::InlineBlock,
        // Table family: the layout engine's table branch dispatches on these
        // roles (rows become flex rows of cells inside a column-of-rows
        // table node); the CSSOM already reports the same values.
        "table" => Display::Table,
        "tr" | "thead" | "tbody" | "tfoot" => Display::TableRow,
        "td" | "th" => Display::TableCell,
        // Replaced media is inline-level in every browser UA sheet: an
        // unstyled <img> sits ON the text line (baseline = bottom margin
        // edge), not stacked as a block between the text lines. Same run
        // dispatch as form controls — the replaced leaf joins the run.
        "img" | "video" | "iframe" | "canvas" | "object" | "embed" => Display::Inline,
        // Non-visual metadata elements: every browser's UA stylesheet sets
        // display:none on these (blitz assets/default.css included). Without
        // it a page's <head>/<style> blocks lay out as empty boxes and push
        // the whole body down the page.
        "head" | "style" | "script" | "meta" | "link" | "title" | "noscript"
        | "template" | "base" => Display::None,
        _ => Display::Block,
    }
}

pub fn ua_font_weight(tag: &str) -> Option<u16> {
    match tag {
        "b" | "strong" | "th" => Some(700),
        // Every browser UA sheet (blitz default.css included) bolds the
        // heading levels; our ua_font_weight only fed b/strong before,
        // so unstyled <h1> painted regular weight.
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some(700),
        _ => None,
    }
}

/// UA text-align: th centers every browser UA sheet's header cells, caption
/// carries CSS2.1's `caption { text-align: center }`. The element's own UA
/// declaration beats an inherited value (cascadeElement merges with `.or`),
/// so a th stays centered inside a text-align:right ancestor, matching
/// Chrome.
pub fn ua_text_align(tag: &str) -> Option<TextAlign> {
    match tag {
        "th" | "caption" => Some(TextAlign::Center),
        _ => None,
    }
}

/// UA text decorations (every browser UA sheet's block): u/ins underline,
/// s/del/strike line-through, and links underline — the `a` entry stands for
/// `a:-webkit-any-link`, so the cascade site gates it on an href attribute.
/// Author declarations override (they apply later in cascade_element).
pub fn ua_text_decoration(tag: &str) -> Option<TextDecorations> {
    match tag {
        "u" | "ins" | "a" => Some(TextDecorations { underline: true, ..Default::default() }),
        "s" | "del" | "strike" => Some(TextDecorations { line_through: true, ..Default::default() }),
        _ => None,
    }
}

/// UA heading font-sizes (blitz default.css / HTML rendering spec). Expressed
/// as em so the cascade's font-size pre-pass folds them against the PARENT
/// font-size, where author declarations override. small/big/sub/sup mirror
/// every browser UA sheet's relative keywords as em factors (smaller = 13.33
/// at the 16px base, larger = 1.2 × parent).
pub fn ua_font_size(tag: &str) -> Option<CssLength> {
    match tag {
        "h1" => Some(CssLength::Em(2.0)),
        "h2" => Some(CssLength::Em(1.5)),
        "h3" => Some(CssLength::Em(1.17)),
        "h4" => Some(CssLength::Em(1.0)),
        "h5" => Some(CssLength::Em(0.83)),
        "h6" => Some(CssLength::Em(0.67)),
        "small" | "sub" | "sup" => Some(CssLength::Em(0.8333)),
        "big" => Some(CssLength::Em(1.2)),
        _ => None,
    }
}

/// UA monospace families: code/kbd/samp/tt and the preformatted group are
/// monospace in every browser UA sheet. Beats the inherited family (`.or`
/// at the merge site), author declarations override it.
pub fn ua_font_family(tag: &str) -> Option<&'static str> {
    match tag {
        "code" | "kbd" | "samp" | "tt" | "pre" | "xmp" | "listing" | "plaintext" => {
            Some("monospace")
        }
        _ => None,
    }
}

/// Whether a computed font-family list selects a monospace face for the
/// run: any top-level list member named `monospace`/`ui-monospace` (case-
/// insensitive, unquoted — quoted names are literal families). The layout
/// stack renders that run's ASCII through the bundled mono face; chars it
/// lacks (CJK) fall through to the CJK pair per-character, like a browser
/// per-char cascade.
pub fn wants_monospace(family: &str) -> bool {
    family.split(',').any(|f| {
        let f = f.trim();
        let unquoted = f
            .strip_prefix('"')
            .and_then(|f| f.strip_suffix('"'))
            .or_else(|| f.strip_prefix('\'').and_then(|f| f.strip_suffix('\'')))
            .unwrap_or(f);
        unquoted.eq_ignore_ascii_case("monospace") || unquoted.eq_ignore_ascii_case("ui-monospace")
    })
}

/// UA margins in CSS order (top right bottom left), from the same blitz
/// default.css block every other browser UA sheet mirrors. Em folds against
/// the element's OWN font-size at cascade time (h1's .67em × its 2em size).
/// UA white-space defaults: the preformatted group keeps whitespace and
/// newlines in every browser UA sheet. Fills after inheritance so an author
/// declaration (`pre { white-space: normal }`) still wins.
pub fn ua_white_space(tag: &str) -> Option<WhiteSpace> {
    match tag {
        "pre" | "xmp" | "listing" | "plaintext" => Some(WhiteSpace::Pre),
        _ => None,
    }
}

pub fn ua_margin(tag: &str) -> Option<[CssLength; 4]> {    let em = |n: f32| CssLength::Em(n);
    let px = |n: f32| CssLength::Px(n);
    match tag {
        "body" => Some([px(8.0), px(8.0), px(8.0), px(8.0)]),
        "p" | "dl" | "multicol" | "search" => Some([em(1.0), px(0.0), em(1.0), px(0.0)]),
        "blockquote" | "figure" => Some([em(1.0), px(40.0), em(1.0), px(40.0)]),
        "dd" => Some([px(0.0), px(0.0), px(0.0), px(40.0)]),
        "ul" | "ol" | "menu" | "dir" => Some([em(1.0), px(0.0), em(1.0), px(0.0)]),
        "pre" | "xmp" | "listing" | "plaintext" => Some([em(1.0), px(0.0), em(1.0), px(0.0)]),
        "h1" => Some([em(0.67), px(0.0), em(0.67), px(0.0)]),
        "h2" => Some([em(0.83), px(0.0), em(0.83), px(0.0)]),
        "h3" => Some([em(1.0), px(0.0), em(1.0), px(0.0)]),
        "h4" => Some([em(1.33), px(0.0), em(1.33), px(0.0)]),
        "h5" => Some([em(1.67), px(0.0), em(1.67), px(0.0)]),
        "h6" => Some([em(2.33), px(0.0), em(2.33), px(0.0)]),
        _ => None,
    }
}

/// UA paddings (list indentation): the classic 40px inline-start of nested
/// list containers, same source as the margin table.
pub fn ua_padding(tag: &str) -> Option<[CssLength; 4]> {
    match tag {
        "ul" | "ol" | "menu" | "dir" => {
            Some([CssLength::Px(0.0), CssLength::Px(0.0), CssLength::Px(0.0), CssLength::Px(40.0)])
        }
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
/// Resolve one element's CSS animation against the stylesheet's keyframes
/// table at time `t` (seconds since document start), mutating the computed
/// style in place. `t: None` means "no clock" — static renders (screenshot,
/// PDF) sample the END state, so an animated SVG's poster frame is the
/// finished diagram rather than the blank t=0 one; the engine has no wall
/// clock and the end state is the only deterministic poster.
///
/// v1 interpolates the paint-channel properties (opacity, transform,
/// stroke-dashoffset — the fade/rise/self-draw grammar); other properties
/// inside keyframes are ignored. Before the delay the element keeps its
/// underlying cascade value (fill-mode none semantics); after the end it
/// holds the final stop when fill-mode is `forwards`/`both`.
pub fn sample_css_animation(style: &mut ComputedStyle, keyframes: &KeyframesMap, t: Option<f64>) {
    let Some(anim) = style.animation.clone() else { return };
    let Some(kf) = keyframes.get(&anim.name) else { return };
    let progress = match t {
        None => 1.0,
        Some(t) => {
            let elapsed = t as f32 - anim.delay;
            // Strict: t == delay is the first instant of the active phase
            // (progress 0, from-stop applies), not the pre-delay hold.
            if elapsed < 0.0 {
                return;
            }
            // Active duration spans iteration-count cycles (CSS Animations 1).
            // `infinite` multiplies out to +inf, so the animation never ends;
            // a zero count never enters the active phase at all.
            let active = anim.duration * anim.iterations;
            if anim.duration <= 0.0 || elapsed >= active {
                if !anim.fill_forwards {
                    return;
                }
                // After-phase fill holds the final iteration's state (Web
                // Animations after-phase iteration progress): the fractional
                // part of the count, or the 100% keyframe when it lands on a
                // whole iteration (a zero count holds the 0% one).
                if anim.iterations == 0.0 {
                    0.0
                } else if anim.iterations.fract() == 0.0 {
                    1.0
                } else {
                    anim.iterations.fract()
                }
            } else {
                // Easing transforms progress within each iteration, so it
                // restarts every cycle.
                anim.easing.map((elapsed / anim.duration) % 1.0)
            }
        }
    };
    apply_stops(style, kf, progress.clamp(0.0, 1.0));
}

fn lerp(a: f32, b: f32, l: f32) -> f32 {
    a + (b - a) * l
}

/// A keyframe transform value, keeping "stop lacks the property" (use the
/// underlying transform) apart from `transform: none` (identity).
enum StopTransform {
    Absent,
    Identity,
    T(Transform2D),
}

fn stop_transform(
    stop: &KeyframeStop,
    custom: &std::collections::HashMap<String, String>,
) -> StopTransform {
    let Some((_, raw)) = stop.decls.iter().rev().find(|(k, _)| k == "transform") else {
        return StopTransform::Absent;
    };
    let sub = substitute_vars(raw, custom, 0).unwrap_or_else(|| raw.clone());
    if sub.trim() == "none" {
        return StopTransform::Identity;
    }
    parse_transform(&sub).map(StopTransform::T).unwrap_or(StopTransform::Absent)
}

fn stop_num(stop: &KeyframeStop, prop: &str, custom: &std::collections::HashMap<String, String>) -> Option<f32> {
    let (_, raw) = stop.decls.iter().rev().find(|(k, _)| k == prop)?;
    let sub = substitute_vars(raw, custom, 0).unwrap_or_else(|| raw.clone());
    sub.trim().parse::<f32>().ok().filter(|n| n.is_finite())
}

fn lerp_len(a: Length, b: Length, l: f32) -> Length {
    match (a, b) {
        (Length::Px(x), Length::Px(y)) => Length::Px(lerp(x, y, l)),
        (Length::Percent(x), Length::Percent(y)) => Length::Percent(lerp(x, y, l)),
        // Mixed units have no box to resolve against mid-flight: snap to
        // the nearer side (discrete interpolation).
        _ => {
            if l < 0.5 {
                a
            } else {
                b
            }
        }
    }
}

/// Interpolate two transforms. `None` on a side is identity for this
/// purpose (`transform: none` in a `to` stop); the mixed-unit lengths snap
/// discretely, everything else lerps componentwise.
fn lerp_transform(a: Option<Transform2D>, b: Option<Transform2D>, l: f32) -> Option<Transform2D> {
    let mix_to_identity = |x: &Transform2D, l: f32| Transform2D {
        a: lerp(x.a, 1.0, l),
        b: lerp(x.b, 0.0, l),
        c: lerp(x.c, 0.0, l),
        d: lerp(x.d, 1.0, l),
        tx: lerp_len(x.tx, Length::Px(0.0), l),
        ty: lerp_len(x.ty, Length::Px(0.0), l),
    };
    match (a, b) {
        (None, None) => None,
        (Some(x), None) => Some(mix_to_identity(&x, l)),
        (None, Some(y)) => Some(mix_to_identity(&y, 1.0 - l)),
        (Some(x), Some(y)) => Some(Transform2D {
            a: lerp(x.a, y.a, l),
            b: lerp(x.b, y.b, l),
            c: lerp(x.c, y.c, l),
            d: lerp(x.d, y.d, l),
            tx: lerp_len(x.tx, y.tx, l),
            ty: lerp_len(x.ty, y.ty, l),
        }),
    }
}

/// Apply the keyframe bracket around `p` to the three animated properties,
/// with the element's underlying cascade values as the fallback for stops
/// that omit them (CSS missing-keyframe semantics).
fn apply_stops(style: &mut ComputedStyle, kf: &Keyframes, p: f32) {
    let stops = &kf.stops;
    if stops.is_empty() {
        return;
    }
    let mut i0 = 0;
    for (i, s) in stops.iter().enumerate() {
        if s.offset <= p {
            i0 = i;
        } else {
            break;
        }
    }
    let (local, s1) = match stops.get(i0 + 1) {
        Some(next) => {
            let span = next.offset - stops[i0].offset;
            let local = if span <= 0.0 { 1.0 } else { ((p - stops[i0].offset) / span).clamp(0.0, 1.0) };
            (local, next)
        }
        None => (1.0, &stops[i0]),
    };
    let s0 = &stops[i0];

    let base_opacity = style.opacity.unwrap_or(1.0);
    let v0 = stop_num(s0, "opacity", &style.custom).unwrap_or(base_opacity);
    let v1 = stop_num(s1, "opacity", &style.custom).unwrap_or(base_opacity);
    style.opacity = Some(lerp(v0, v1, local).clamp(0.0, 1.0));

    let base_dash = style.svg_dashoffset.unwrap_or(0.0);
    let d0 = stop_num(s0, "stroke-dashoffset", &style.custom).unwrap_or(base_dash);
    let d1 = stop_num(s1, "stroke-dashoffset", &style.custom).unwrap_or(base_dash);
    style.svg_dashoffset = Some(lerp(d0, d1, local));

    let underlying = style.transform;
    let resolve = |st: StopTransform| match st {
        StopTransform::Absent => underlying,
        StopTransform::Identity => None,
        StopTransform::T(t) => Some(t),
    };
    let t0 = resolve(stop_transform(s0, &style.custom));
    let t1 = resolve(stop_transform(s1, &style.custom));
    style.transform = lerp_transform(t0, t1, local);
}

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
            // Custom property names are case-SENSITIVE (--Main ≠ --main);
            // everything else lowercases.
            let name = if name.starts_with("--") {
                name.to_string()
            } else {
                name.to_ascii_lowercase()
            };
            Some((name, value.trim().to_string()))
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
    apply_declarations_importance(style, declarations, fonts, Importance::Any)
}

/// Which declarations an apply pass admits. The cascade runs the normal
/// pass (stylesheet rules in specificity+order sequence, then inline at the
/// top of normal) before the !important pass — author important beats every
/// author-normal declaration, inline style included (CSS 2.1 §6.4.1). That
/// ordering is what lets a stylesheet's `@media print { transform: none
/// !important }` override the inline `translateX(...)` a carousel script
/// parks on each slide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Importance {
    /// Only declarations WITHOUT `!important`.
    Normal,
    /// Only declarations WITH `!important` (suffix stripped).
    Important,
    /// Everything — the single-pass posture for standalone callers
    /// (@supports probes, tests) where importance never reorders anything.
    Any,
}

pub(crate) fn apply_declarations_importance(
    style: &mut ComputedStyle,
    declarations: &str,
    fonts: &FontCtx,
    imp: Importance,
) -> bool {
    let mut applied = false;
    for (name, value) in split_declarations(declarations) {
        // Strip a trailing !important before anything sees the value: the
        // flag reorders the cascade passes, and the suffix would otherwise
        // poison every parser (`none !important` != `none`).
        let (value, important) = strip_important(&value);
        match imp {
            Importance::Normal if important => continue,
            Importance::Important if !important => continue,
            _ => {}
        }
        if name.starts_with("--") {
            // Custom property: store the token stream raw. Values may hold
            // anything (including semicolon-free junk). Empty value =
            // guaranteed-invalid → unset. The map is Arc-shared down the
            // inheritance chain (custom props inherit computed wholesale);
            // make_mut clones only when THIS element re-declares (#109: a
            // 500-var design system made the per-element inheritance clone
            // the dominant cost of a whole-document style pass).
            let v = value.trim();
            if v.is_empty() {
                std::sync::Arc::make_mut(&mut style.custom).remove(&name);
            } else {
                std::sync::Arc::make_mut(&mut style.custom)
                    .insert(name.clone(), v.to_string());
            }
            applied = true;
            continue;
        }
        // Normal property: substitute var() references against the custom
        // map (inherited + already-applied declarations). Unresolved without
        // fallback ⇒ treat the declaration as invalid-at-computed-value-time
        // and drop it — inherited/earlier values survive, which matches the
        // common case of IACVT on non-inherited properties closely enough.
        let value = if value.to_ascii_lowercase().contains("var(") {
            match substitute_vars(&value, &style.custom, 0) {
                Some(v) => v,
                None => continue,
            }
        } else {
            value.to_string()
        };
        if apply_one(style, &name, &value, fonts) {
            applied = true;
        }
    }
    applied
}

/// Split a trailing `!important` off a declaration value (case-insensitive,
/// whitespace-tolerant). Returns the stripped value plus whether the flag
/// was present.
fn strip_important(value: &str) -> (String, bool) {
    let v = value.trim_end();
    if v.to_ascii_lowercase().ends_with("!important") {
        (v[..v.len() - "!important".len()].trim_end().to_string(), true)
    } else {
        (value.to_string(), false)
    }
}

/// Replace every `var(--name[, fallback])` in `value` using `custom`.
/// `None` when any reference is unresolvable (no such property AND no
/// usable fallback) — the caller then drops the declaration (IACVT).
/// Depth-capped to break `--a: var(--b); --b: var(--a)` cycles.
fn substitute_vars(value: &str, custom: &std::collections::HashMap<String, String>, depth: usize) -> Option<String> {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let Some(start) = lower.find("var(") else {
        return Some(value.to_string());
    };
    // Walk to the matching close paren. `start`/`end` are byte offsets of
    // ASCII chars, so slicing between them is char-boundary safe; interior
    // UTF-8 bytes (>= 0x80) never match the ASCII delimiters.
    let bytes = value.as_bytes();
    let mut level = 0usize;
    let mut end = None;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => level += 1,
            b')' => {
                level -= 1;
                if level == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let end = end?;
    // Inside the parens: `--name` + optional `, fallback`.
    let inner = &value[start + 4..end];
    let (name, fallback) = match split_top_comma(inner) {
        Some((n, f)) => (n.trim(), Some(f)),
        None => (inner.trim(), None),
    };
    let head = &value[..start];
    let tail = &value[end + 1..];
    let replacement = match custom.get(name) {
        Some(v) => substitute_vars(v, custom, depth + 1)?,
        None => match fallback {
            Some(f) => substitute_vars(f.trim(), custom, depth + 1)?,
            None => return None,
        },
    };
    // The tail may contain further var() references.
    let tail = substitute_vars(tail, custom, depth)?;
    Some(format!("{head}{replacement}{tail}"))
}

/// Split at the first top-level comma (outside parens/quotes) — used to
/// separate a var() fallback, which may itself contain commas and nested
/// functions like `var(--x, 1px, 2px)` / `var(--y, rgb(0, 0, 0))`.
fn split_top_comma(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut paren = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren -= 1,
            b',' if paren == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

fn apply_one(style: &mut ComputedStyle, name: &str, value: &str, fonts: &FontCtx) -> bool {
    let v = value.trim();
    // Length arm helper: parse + fold em/rem against the font context;
    // calc() bodies route through the evaluator (same folding per term).
    let len = |val: &str| -> Option<Length> {
        let t = val.trim();
        if let Some(l) = eval_math_call(t, fonts) {
            return Some(l);
        }
        parse_css_length(t).map(|l| resolve_len(l, fonts))
    };
    // Sizing keywords first: only the width family accepts them (the match
    // arms below decide legality); everything else is a regular length.
    let len_sizing = |val: &str| -> Option<Length> {
        match val.trim().to_ascii_lowercase().as_str() {
            "min-content" => Some(Length::MinContent),
            "max-content" => Some(Length::MaxContent),
            "fit-content" => Some(Length::FitContent),
            _ => len(val),
        }
    };
    match name {
        "display" => {
            // inline-flex/inline-grid (#107) and inline-table (its follow-up):
            // Fusion's .next-input family is `display: inline-table;
            // width: 200px` with `width: 100%` inputs inside — dropping the
            // declaration (the pre-#107 behavior for each missing value)
            // left 32 inputs at width 0 on the Tmall publish page. taffy
            // only needs the container mode, so they map onto plain
            // Flex/Grid/Table; the outer inline participation is a
            // refinement, not a correctness requirement.
            style.display = match v {
                "block" => Some(Display::Block),
                "inline" => Some(Display::Inline),
                "inline-block" => Some(Display::InlineBlock),
                "flex" | "inline-flex" => Some(Display::Flex),
                "grid" | "inline-grid" => Some(Display::Grid),
                "table" | "inline-table" => Some(Display::Table),
                "table-row" => Some(Display::TableRow),
                "table-cell" => Some(Display::TableCell),
                "none" => Some(Display::None),
                _ => return false,
            };
            style.display_from_ua = false;
            true
        }
        "border-collapse" => {
            style.border_collapse = match v {
                "collapse" => Some(BorderCollapse::Collapse),
                "separate" => Some(BorderCollapse::Separate),
                _ => return false,
            };
            true
        }
        "table-layout" => {
            style.table_layout = match v {
                "auto" => Some(TableLayout::Auto),
                "fixed" => Some(TableLayout::Fixed),
                _ => return false,
            };
            true
        }
        "vertical-align" => {
            match v {
                "top" => style.vertical_align = Some(VerticalAlign::Top),
                "baseline" => style.vertical_align = Some(VerticalAlign::Baseline),
                "middle" => style.vertical_align = Some(VerticalAlign::Middle),
                "bottom" => style.vertical_align = Some(VerticalAlign::Bottom),
                "sub" => style.vertical_align = Some(VerticalAlign::Sub),
                "super" => style.vertical_align = Some(VerticalAlign::Super),
                _ => match parse_css_length(v).map(|l| resolve_len(l, fonts)) {
                    // Lengths resolve to px now (em against the element's own
                    // font-size); the % keeps its shape for the layout side,
                    // which resolves it against the element's line-height.
                    Some(Length::Px(px)) => style.vertical_align = Some(VerticalAlign::Length(px)),
                    Some(Length::Percent(p)) => style.vertical_align = Some(VerticalAlign::Percent(p)),
                    _ => return false,
                },
            }
            true
        }
        "text-decoration-line" => match parse_text_decoration(v, false) {
            Some(d) => {
                style.text_decoration_line = Some(d);
                true
            }
            None => false,
        },
        "text-decoration" => match parse_text_decoration(v, true) {
            Some(d) => {
                style.text_decoration_line = Some(d);
                true
            }
            None => false,
        },
        "color" => parse_color(v).map(|c| style.color = Some(c)).is_some(),
        // SVG presentation properties (svg v1): they cascade like any other
        // property — archify-class diagrams color shapes through classes +
        // custom properties (`fill: var(--frontend-fill)`), so the values
        // arrive here var()-substituted. `none` maps to a fully transparent
        // paint (both mean "draw nothing"); `currentColor` stays symbolic
        // and resolves against the element's inherited `color` at svg
        // compile time. url(#paint) references (gradients/patterns) drop
        // the declaration, falling back to inherited/attribute/default.
        "fill" | "stroke" => {
            let paint = if v.eq_ignore_ascii_case("none") {
                SvgPaint::Color(Color(0, 0, 0, 0))
            } else if v.eq_ignore_ascii_case("currentColor") {
                SvgPaint::CurrentColor
            } else {
                match parse_color(v) {
                    Some(c) => SvgPaint::Color(c),
                    None => return false,
                }
            };
            if name == "fill" {
                style.svg_fill = Some(paint);
            } else {
                style.svg_stroke = Some(paint);
            }
            true
        }
        "stroke-width" => match v.parse::<f32>() {
            Ok(w) if w.is_finite() && w >= 0.0 => {
                style.svg_stroke_width = Some(w);
                true
            }
            _ => false,
        },
        "stroke-dasharray" => {
            let list: Option<Vec<f32>> = v
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|t| !t.is_empty())
                .map(|t| t.parse::<f32>().ok().filter(|n| n.is_finite() && *n >= 0.0))
                .collect();
            match list {
                Some(l) if !l.is_empty() => {
                    style.svg_dasharray = Some(l);
                    true
                }
                _ => false,
            }
        }
        "stroke-dashoffset" => match v.parse::<f32>() {
            Ok(n) if n.is_finite() => {
                style.svg_dashoffset = Some(n);
                true
            }
            _ => false,
        },
        // `font` shorthand: `[ <style> || <variant> || <weight> || <stretch> ]?
        // <size> [ / <line-height> ]? <family>`. font-style/variant/stretch
        // tokens are accepted and skipped (the style struct models none of
        // them); weight/size/line-height/family apply through the same parse
        // paths as the longhands. Per shorthand reset semantics the modeled
        // longhands reset to their initial first, so `font: 20px serif` on a
        // <b> clears the bold like Chrome (the expanded author-initial
        // declaration beats the UA bolder).
        "font" => match parse_font_shorthand(v, fonts) {
            Some(p) => {
                style.font_weight = Some(p.weight);
                style.line_height = Some(p.line_height.unwrap_or(LineHeightSpec::Normal));
                if let Some(px) = p.size_px {
                    style.font_size = Some(px);
                }
                style.font_family = Some(p.family);
                // font-variant-caps is modeled now, so the shorthand reset
                // covers it too: `font: 20px serif` after small-caps drops
                // the caps like Chrome; `font: small-caps 20px serif` sets it.
                style.font_variant_caps = Some(p.small_caps);
                true
            }
            None => false,
        },
        // Raw passthrough: no parsed form, no layout effect — the CSSOM
        // layer reports it verbatim (var() already substituted upstream).
        "background-image" => {
            style.background_image = Some(v.to_string());
            true
        }
        // Same posture as background-image: the author's stack, verbatim.
        "font-family" => {
            style.font_family = Some(v.to_string());
            true
        }
        "background-color" => parse_color(v).map(|c| style.background_color = Some(c)).is_some(),
        // `background` shorthand: paren-aware token split, then classify —
        // a *-gradient(...) function feeds background_image (the paint
        // layer parses it there), a color token feeds background_color;
        // repeat/position/attachment tokens are accepted and ignored (v1
        // models no other layer longhand). Like every CSS shorthand it
        // RESETS the sub-longhands first — both modeled ones — so
        // `background: #fff` after a gradient clears the image and
        // `background: none` after `background-color: red` clears the
        // color (css-backgrounds-3 §3).
        "background" => {
            style.background_image = None;
            style.background_color = None;
            let mut applied = false;
            for tok in split_sides(v) {
                let t = tok.trim();
                if let Some(c) = parse_color(t) {
                    style.background_color = Some(c);
                    applied = true;
                } else if t.to_ascii_lowercase().contains("gradient(") {
                    style.background_image = Some(t.to_string());
                    applied = true;
                } else if t.eq_ignore_ascii_case("none") || t.starts_with("url(") {
                    // Recognized layer values this slice doesn't model.
                    applied = true;
                }
            }
            applied
        }
        "margin" => {
            let sides = expand_sides(v, fonts);
            style.margin = sides;
            true
        }
        "padding" => {
            // `auto` is illegal for padding: drop the declaration entirely
            // (CSS invalid-declaration recovery).
            let sides = expand_sides(v, fonts);
            match sides_no_auto(&sides) {
                Some(clean) => {
                    style.padding = clean;
                    true
                }
                None => false,
            }
        }
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            let len = if v.eq_ignore_ascii_case("auto") {
                Some(Length::Auto)
            } else {
                len(v)
            };
            set_side(&mut style.margin, name, len);
            true
        }
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
            // padding: auto is illegal - drop the declaration.
            if v.eq_ignore_ascii_case("auto") {
                return false;
            }
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
        // Per-side shorthands (`border-top: 2px solid red`, …): same
        // triple grammar as `border`, width applied to the named side only.
        // border_style/border_color stay uniform-for-all-sides in this model
        // (documented approximation), so a side shorthand's style/color
        // applies globally.
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let mut width: Option<Length> = None;
            let mut line: Option<Option<BorderStyle>> = None;
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
                match name {
                    "border-top" => style.border_width.top = Some(l),
                    "border-right" => style.border_width.right = Some(l),
                    "border-bottom" => style.border_width.bottom = Some(l),
                    "border-left" => style.border_width.left = Some(l),
                    _ => unreachable!(),
                }
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
        // Per-side width longhands: `border: 1px solid; border-top-width: 0`
        // must leave the other three sides intact (Chrome shows a three-sided
        // outline there — blitz#837's repro shape). The property used to be
        // unknown here, so the side kept the shorthand's width.
        "border-top-width" | "border-right-width" | "border-bottom-width" | "border-left-width" => {
            let l = match v {
                "thin" => Some(Length::Px(1.0)),
                "medium" => Some(Length::Px(3.0)),
                "thick" => Some(Length::Px(5.0)),
                _ => len(v),
            };
            let Some(l) = l else { return false };
            match name {
                "border-top-width" => style.border_width.top = Some(l),
                "border-right-width" => style.border_width.right = Some(l),
                "border-bottom-width" => style.border_width.bottom = Some(l),
                _ => style.border_width.left = Some(l),
            }
            true
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
        "width" => {
            style.width = len_sizing(v);
            style.width.is_some()
        }
        // `height: auto` must APPLY as Length::Auto, not drop: `auto` is a
        // legal value, and dropping the declaration leaves an earlier
        // `height: 100%` standing — a print block's `html,body{height:auto}`
        // then never clears the screen value, so the §10.5 fold (#65) has
        // nothing to fold (the computed height stays a viewport percent and
        // the deck stays pinned at one screen tall).
        "height" => {
            let l = if v.eq_ignore_ascii_case("auto") {
                Some(Length::Auto)
            } else {
                len(v)
            };
            l.map(|l| style.height = Some(l)).is_some()
        }
        "font-weight" => {
            let weight = parse_font_weight(v);
            style.font_weight = weight;
            weight.is_some()
        }
        "line-height" => parse_line_height(v)
            .map(|raw| {
                style.line_height = Some(match raw {
                    LineHeightRaw::Normal => LineHeightSpec::Normal,
                    LineHeightRaw::Number(n) => LineHeightSpec::Number(n),
                    LineHeightRaw::Len(CssLength::Px(px)) => LineHeightSpec::Px(px),
                    // em/rem fold against the element's OWN font-size (the
                    // cascade's FontCtx carries it); % behaves like a number.
                    LineHeightRaw::Len(CssLength::Em(n)) => LineHeightSpec::Px(n * fonts.own),
                    LineHeightRaw::Len(CssLength::Rem(n)) => LineHeightSpec::Px(n * fonts.root),
                    LineHeightRaw::Len(CssLength::Percent(p)) => LineHeightSpec::Number(p / 100.0),
                    LineHeightRaw::Len(CssLength::Vw(n)) => LineHeightSpec::Px(n * fonts.viewport_w / 100.0),
                    LineHeightRaw::Len(CssLength::Vh(n)) => LineHeightSpec::Px(n * fonts.viewport_h / 100.0),
                })
            })
            .is_some(),
        "text-align" => {
            style.text_align = match v {
                "left" | "start" => Some(TextAlign::Left),
                "center" => Some(TextAlign::Center),
                "right" | "end" => Some(TextAlign::Right),
                _ => return false,
            };
            true
        }
        "word-spacing" => {
            // `normal` is the explicit initial; px lengths (negative allowed)
            // apply as-is, em/rem fold against the font context. Percent is
            // relative to the containing block's inline advance (needs
            // geometry this layer lacks) — rejected like other %-lengths.
            if v.eq_ignore_ascii_case("normal") {
                style.word_spacing = None;
                return true;
            }
            match len(v) {
                Some(Length::Px(px)) => {
                    style.word_spacing = Some(px);
                    true
                }
                _ => false,
            }
        }
        "font-variant-caps" => {
            style.font_variant_caps = match v {
                "small-caps" => Some(true),
                "normal" => Some(false),
                _ => return false,
            };
            true
        }
        "container-type" => {
            style.container_type = match v {
                "size" => ContainerType::Size,
                "inline-size" => ContainerType::InlineSize,
                "normal" => ContainerType::Normal,
                _ => return false,
            };
            true
        }
        "container-name" => {
            style.container_name = match v {
                "none" | "" => None,
                ident => Some(ident.to_string()),
            };
            true
        }
        // `font-variant` shorthand: v1 models only the caps subset the
        // engine can synthesize; other feature keywords (ligatures,
        // numeric, east-asian...) reject like other unmodeled values.
        "font-variant" => match v {
            "small-caps" => {
                style.font_variant_caps = Some(true);
                true
            }
            "normal" => {
                style.font_variant_caps = Some(false);
                true
            }
            _ => false,
        },
        "overflow" => {
            let (a, b) = match v.split_whitespace().collect::<Vec<_>>().as_slice() {
                [one] => (*one, *one),
                [x, y] => (*x, *y),
                _ => return false,
            };
            let kw = |t: &str| match t {
                "visible" => Some(Overflow::Visible),
                "hidden" => Some(Overflow::Hidden),
                "clip" => Some(Overflow::Clip),
                "scroll" => Some(Overflow::Scroll),
                "auto" => Some(Overflow::Auto),
                _ => None,
            };
            let (Some(x), Some(y)) = (kw(a), kw(b)) else {
                return false;
            };
            style.overflow_x = Some(x);
            style.overflow_y = Some(y);
            true
        }
        "overflow-x" | "overflow-y" => {
            let Some(o) = (match v.trim() {
                "visible" => Some(Overflow::Visible),
                "hidden" => Some(Overflow::Hidden),
                "clip" => Some(Overflow::Clip),
                "scroll" => Some(Overflow::Scroll),
                "auto" => Some(Overflow::Auto),
                _ => None,
            }) else {
                return false;
            };
            if name.ends_with("-x") {
                style.overflow_x = Some(o);
            } else {
                style.overflow_y = Some(o);
            }
            true
        }
        "box-shadow" => {
            if v.eq_ignore_ascii_case("none") {
                style.box_shadow = None;
                return true;
            }
            match parse_box_shadow(v, style.color, fonts) {
                Some(layers) if !layers.is_empty() => {
                    style.box_shadow = Some(layers);
                    true
                }
                _ => false,
            }
        }
        "backdrop-filter" => {
            if v.eq_ignore_ascii_case("none") {
                style.backdrop_blur = None;
                return true;
            }
            match parse_backdrop_blur(v, fonts) {
                Some(px) => {
                    style.backdrop_blur = Some(px);
                    true
                }
                _ => false,
            }
        }
        "text-shadow" => {
            if v.eq_ignore_ascii_case("none") {
                style.text_shadow = None;
                return true;
            }
            match parse_text_shadow(v, style.color, fonts) {
                Some(layers) if !layers.is_empty() => {
                    style.text_shadow = Some(layers);
                    true
                }
                _ => false,
            }
        }
        "white-space" => {
            style.white_space = match v {
                "normal" => Some(WhiteSpace::Normal),
                "nowrap" => Some(WhiteSpace::Nowrap),
                "pre" => Some(WhiteSpace::Pre),
                "pre-wrap" => Some(WhiteSpace::PreWrap),
                "pre-line" => Some(WhiteSpace::PreLine),
                "break-spaces" => Some(WhiteSpace::BreakSpaces),
                _ => return false,
            };
            true
        }
        "text-overflow" => {
            style.text_overflow = match v {
                "clip" => Some(TextOverflow::Clip),
                "ellipsis" => Some(TextOverflow::Ellipsis),
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
        "transform" => {
            // Full 2D function list (obscura #740 lineage, widened twice):
            // translate/scale/rotate/skew/matrix compose into one affine.
            // One unknown function still invalidates the whole declaration
            // (spec) — parse_transform returns None and the element paints
            // where layout put it. Viewport-threaded so translateX(100vw)
            // — the deck/carousel idiom — folds against the page viewport.
            style.transform = parse_transform_with_vp(v, fonts.viewport_w, fonts.viewport_h);
            style.transform.is_some()
        }
        "opacity" => {
            // A number in [0, 1] (animation batch A). The initial value 1
            // stores like any other — paint skips alpha work when the
            // accumulated alpha is exactly 1.
            match v.trim().parse::<f32>() {
                Ok(n) if n.is_finite() => {
                    style.opacity = Some(n.clamp(0.0, 1.0));
                    true
                }
                _ => false,
            }
        }
        "animation" => {
            // Stored, not applied here: the sampler resolves it against
            // the stylesheet's @keyframes table at sample time.
            style.animation = parse_animation_shorthand(v);
            style.animation.is_some()
        }
        "transition" => {
            // Same storage-only contract as `animation`: the cascade
            // records the spec; trigger detection lives in the JS face
            // and interpolation in the exit sampler. Comma lists collapse
            // to the first entry.
            style.transition = parse_transition_shorthand(v);
            style.transition.is_some()
        }
        "transition-property" => {
            // First comma entry only. `none` disables transitions for the
            // element (the spec is dropped); a later duration/delay
            // longhand would re-create it with the initial value "all".
            let tok = v.split(',').next().unwrap_or("").trim();
            if tok.eq_ignore_ascii_case("none") {
                style.transition = None;
                true
            } else if !tok.is_empty()
                && tok
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                let spec = style.transition.get_or_insert(TransitionSpec {
                    property: None,
                    duration: 0.0,
                    delay: 0.0,
                    easing: Easing::CubicBezier(0.25, 0.1, 0.25, 1.0),
                });
                spec.property = Some(tok.to_ascii_lowercase());
                true
            } else {
                false
            }
        }
        "transition-duration" | "transition-delay" => {
            // First comma entry; ms folds to seconds. Negative times are
            // invalid here (the sampler never sees a phase offset).
            let t = v.split(',').next().unwrap_or("").trim();
            let secs = t
                .strip_suffix("ms")
                .and_then(|n| n.parse::<f32>().ok().map(|n| n / 1000.0))
                .or_else(|| t.strip_suffix('s').and_then(|n| n.parse::<f32>().ok()));
            match secs {
                Some(s) if s.is_finite() && s >= 0.0 => {
                    let spec = style.transition.get_or_insert(TransitionSpec {
                        property: None,
                        duration: 0.0,
                        delay: 0.0,
                        easing: Easing::CubicBezier(0.25, 0.1, 0.25, 1.0),
                    });
                    if name == "transition-duration" {
                        spec.duration = s;
                    } else {
                        spec.delay = s;
                    }
                    true
                }
                _ => false,
            }
        }
        "transition-timing-function" => {
            // Keyword or cubic-bezier(); steps() and other curves we do
            // not evaluate invalidate the declaration, same rule as the
            // shorthand.
            match parse_easing_token(v.trim()) {
                Some(e) => {
                    style.transition.get_or_insert(TransitionSpec {
                        property: None,
                        duration: 0.0,
                        delay: 0.0,
                        easing: e,
                    });
                    true
                }
                None => false,
            }
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
        // `flex` shorthand (css-flexbox-1 §7.1.1):
        // `none | [ <'flex-grow'> <'flex-shrink'>? || <'flex-basis'> ]`.
        // The gotcha: omitted components take the SHORTHAND's initial values
        // (1 / 1 / 0%), not the longhands' (0 / 1 / auto) — `flex: 1` is
        // `1 1 0%`, so equal-grow cards align even with unequal content,
        // and `flex: 0 0 100vw` (the deck/carousel idiom, #61) sizes empty
        // items. Tokens classify positionally: numbers fill grow then
        // shrink, a length/percent/`auto` fills basis, either order (`||`
        // admits `flex: 100px 0`). Like every shorthand it sets all three
        // sub-longhands, so it resets earlier longhand writes.
        "flex" => {
            if v.eq_ignore_ascii_case("none") {
                style.flex_grow = Some(0.0);
                style.flex_shrink = Some(0.0);
                style.flex_basis = None; // auto
                true
            } else {
                let mut grow: Option<f32> = None;
                let mut shrink: Option<f32> = None;
                // Some(None) = explicit `auto`; bare None = omitted → 0%.
                let mut basis: Option<Option<Length>> = None;
                for tok in split_sides(v) {
                    let t = tok.trim();
                    if t.eq_ignore_ascii_case("auto") {
                        if basis.is_some() {
                            return false;
                        }
                        basis = Some(None);
                    } else if let Some(n) = parse_num_f32(t) {
                        if grow.is_none() {
                            grow = Some(n);
                        } else if shrink.is_none() {
                            shrink = Some(n);
                        } else {
                            return false;
                        }
                    } else if let Some(l) = len(t) {
                        if basis.is_some() {
                            return false;
                        }
                        basis = Some(Some(l));
                    } else {
                        return false;
                    }
                }
                match grow {
                    Some(g) => {
                        style.flex_grow = Some(g);
                        style.flex_shrink = Some(shrink.unwrap_or(1.0));
                        // Explicit `auto` (Some(None)) must survive — only an
                        // omitted basis becomes the shorthand's 0% initial.
                        style.flex_basis =
                            match basis {
                                Some(b) => b,
                                None => Some(Length::Percent(0.0)),
                            };
                        true
                    }
                    // No number anywhere (`flex: auto`-less forms like
                    // `flex: 10px`) are invalid — drop the declaration.
                    None => false,
                }
            }
        }
        "flex-basis" => {
            // <length-percentage> | auto (the initial value, stays None).
            style.flex_basis = len(v);
            style.flex_basis.is_some()
        }
        "gap" => {
            let vals: Vec<Option<Length>> = split_sides(v).into_iter().map(&len).collect();
            match vals.as_slice() {
                [Some(one)] => {
                    style.column_gap = Some(*one);
                    style.row_gap = Some(*one);
                    true
                }
                [Some(c), Some(r)] => {
                    style.column_gap = Some(*c);
                    style.row_gap = Some(*r);
                    true
                }
                // One invalid component invalidates the whole declaration
                // (CSS syntax) — no partial application. `auto` is not in
                // gap's grammar: len() rejects it, same outcome.
                _ => false,
            }
        }
        "column-gap" => {
            style.column_gap = len(v);
            style.column_gap.is_some()
        }
        "row-gap" => {
            style.row_gap = len(v);
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
        "grid-template-areas" => {
            style.grid_template_areas = parse_grid_template_areas(v);
            style.grid_template_areas.is_some()
        }
        // Shorthand `rows / columns`. The string-literal-areas form
        // (`grid-template: 'a b' 1fr / 2rem`) is not supported — both halves
        // fall through to None rather than half-parse.
        "grid-template" => {
            if let Some((rows, cols)) = v.split_once('/') {
                if !v.contains('\'') {
                    style.grid_template_rows = parse_grid_tracks(rows);
                    style.grid_template_columns = parse_grid_tracks(cols);
                }
            }
            true
        }
        // Single-ident named form only (`grid-area: main`); the 4-value
        // numeric form is a later batch.
        "grid-area" => {
            let name = v.trim();
            if !name.contains(' ') && !name.contains('/') && !name.is_empty() {
                style.grid_area = Some(name.to_string());
            }
            true
        }
        "position" => {
            style.position = match v {
                "static" => Some(PositionMode::Static),
                "relative" => Some(PositionMode::Relative),
                "absolute" => Some(PositionMode::Absolute),
                "fixed" => Some(PositionMode::Fixed),
                "sticky" => Some(PositionMode::Sticky),
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
                "inline-start" => Some(ClearSide::InlineStart),
                "inline-end" => Some(ClearSide::InlineEnd),
                "none" => None,
                _ => return false,
            };
            true
        }
        "content" => match parse_content_value(value) {
            // none/normal (or unparseable) leave the field unset: no pseudo
            // box. `content` only ever matters on a generated-content pseudo.
            Some(cv) => {
                style.content = Some(cv);
                true
            }
            None => false,
        },
        "counter-reset" => match parse_counter_modifiers(value, false) {
            Some(m) => {
                style.counter_reset = m;
                true
            }
            None => false,
        },
        "counter-increment" => match parse_counter_modifiers(value, true) {
            Some(m) => {
                style.counter_increment = m;
                true
            }
            None => false,
        },
        "quotes" => match parse_quotes(value) {
            Some(q) => {
                style.quotes = Some(q);
                true
            }
            None => false,
        },
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
        // inset shorthand: <length-percentage>{1,4} | auto. Same expansion
        // family as margin, but auto is the property initial value here
        // (None slots, not Length::Auto) and `inset: auto` resets all four.
        "inset" => {
            if v.eq_ignore_ascii_case("auto") {
                style.top = None;
                style.right = None;
                style.bottom = None;
                style.left = None;
                true
            } else {
                let sides = expand_sides(v, fonts);
                let conv = |x: Option<Length>| match x {
                    Some(Length::Auto) => None,
                    other => other,
                };
                style.top = conv(sides.top);
                style.right = conv(sides.right);
                style.bottom = conv(sides.bottom);
                style.left = conv(sides.left);
                style.top.is_some() || style.right.is_some() || style.bottom.is_some() || style.left.is_some()
            }
        }
        // min/max-width sizing keywords are valid CSS but not resolved by the
        // layout layer yet — they stay unparsed (drop to auto) rather than
        // parse-into-nothing.
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
        "box-sizing" => {
            style.box_sizing = match v {
                "content-box" => Some(BoxSizing::ContentBox),
                "border-box" => Some(BoxSizing::BorderBox),
                _ => return false,
            };
            true
        }
        "background-clip" | "-webkit-background-clip" => {
            style.background_clip_text = match v {
                "text" => true,
                "border-box" | "padding-box" | "content-box" => false,
                _ => return false,
            };
            true
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
    let mut rest = v.trim();
    while !rest.is_empty() {
        let (token, tail) = next_track_token(rest)?;
        // repeat(N, <track-list>) expands inline to N copies of the list.
        // auto-fill/auto-fit counts need the container's size, which the
        // cascade doesn't have — those reject like any unknown token.
        if let Some(inner) = token.strip_prefix("repeat(").and_then(|t| t.strip_suffix(')')) {
            let (count, list) = inner.split_once(',')?;
            let count: usize = count.trim().parse().ok()?;
            // Chrome caps explicit tracks at 1000; repeat expansion must not
            // let a stylesheet allocate unboundedly.
            if count == 0 || count > 1000 {
                return None;
            }
            let expanded = parse_grid_tracks(list.trim())?;
            for _ in 0..count {
                tracks.extend_from_slice(&expanded);
            }
        } else {
            tracks.push(parse_grid_track_token(token)?);
        }
        rest = tail.trim_start();
    }
    if tracks.is_empty() {
        None
    } else {
        Some(tracks)
    }
}

/// Split off one track token: `minmax(a, b)` and `repeat(n, …)` contain
/// spaces, so they can't go through plain whitespace splitting — a token
/// whose first `(` precedes any whitespace runs to its balanced close
/// (repeat lists can nest a minmax, so the scan counts depth).
fn next_track_token(v: &str) -> Option<(&str, &str)> {
    let ws = v.find(char::is_whitespace).unwrap_or(v.len());
    let paren = v.find('(').unwrap_or(v.len());
    if paren < ws {
        let mut depth = 0usize;
        for (i, c) in v.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(v.split_at(i + 1));
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    if ws == 0 {
        None
    } else {
        Some(v.split_at(ws))
    }
}

/// One sizing token: `auto`, `<len>` (px; rem resolves against the 16px
/// browser root — grid tracks live at the top of the page where the root
/// size is what authors mean), `<percent>` (of the grid container's content
/// box, resolved at layout time), `Nfr`, `0`, or a `minmax(a, b)` pair.
/// `min-content`/`max-content` approximate to `auto` until the track
/// machinery learns them.
fn parse_grid_track_token(tok: &str) -> Option<GridTrack> {
    if let Some(inner) = tok.strip_prefix("minmax(").and_then(|t| t.strip_suffix(')')) {
        let (a, b) = inner.split_once(',')?;
        return Some(GridTrack::MinMax {
            min: parse_track_size(a.trim())?,
            max: parse_track_size(b.trim())?,
        });
    }
    parse_track_size(tok).map(|sz| match sz {
        TrackSize::Fr(f) => GridTrack::Fr(f),
        TrackSize::Px(px) => GridTrack::Px(px),
        TrackSize::Percent(p) => GridTrack::Percent(p),
        TrackSize::Auto => GridTrack::Auto,
    })
}

/// One minmax() argument (or a bare track token): same sizings as
/// [`parse_grid_track_token`], minus minmax itself.
fn parse_track_size(tok: &str) -> Option<TrackSize> {
    if tok.eq_ignore_ascii_case("auto")
        || tok.eq_ignore_ascii_case("min-content")
        || tok.eq_ignore_ascii_case("max-content")
    {
        return Some(TrackSize::Auto);
    }
    if let Some(fr) = tok.strip_suffix("fr") {
        return fr.parse::<f32>().ok().map(TrackSize::Fr);
    }
    if let Some(px) = parse_px_f32(tok) {
        return Some(TrackSize::Px(px));
    }
    if let Some(r) = tok.strip_suffix("rem") {
        return r.parse::<f32>().ok().map(|n| TrackSize::Px(n * 16.0));
    }
    if let Some(p) = tok.strip_suffix('%') {
        return p.parse::<f32>().ok().map(TrackSize::Percent);
    }
    if tok == "0" {
        return Some(TrackSize::Px(0.0));
    }
    None
}

/// `'a b' 'c d'` → `[[a, b], [c, d]]`. Every string row must have the same
/// cell count (CSS §7.3) or the whole declaration is invalid → None.
fn parse_grid_template_areas(v: &str) -> Option<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    for raw in v.split('\'') {
        let row = raw.trim();
        if row.is_empty() {
            continue;
        }
        let cells: Vec<String> = row.split_whitespace().map(str::to_string).collect();
        if cells.is_empty() {
            return None;
        }
        rows.push(cells);
    }
    if rows.is_empty() {
        return None;
    }
    let width = rows[0].len();
    if rows.iter().any(|r| r.len() != width) {
        return None;
    }
    Some(rows)
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

/// CSS 1–4 value expansion (px/em/rem/%/calc(); unknown units drop to None).
/// `auto` tokens pass through as `Length::Auto` (margin centering) - the
/// padding/border call sites reject them by dropping the declaration.
fn expand_sides(value: &str, fonts: &FontCtx) -> Sides {
    let vals: Vec<Option<Length>> = split_sides(value)
        .into_iter()
        .map(|tok| {
            if tok.eq_ignore_ascii_case("auto") {
                Some(Length::Auto)
            } else {
                eval_math_call(tok, fonts).or_else(|| parse_css_length(tok).map(|l| resolve_len(l, fonts)))
            }
        })
        .collect();
    match vals.as_slice() {
        [one] => Sides { top: *one, right: *one, bottom: *one, left: *one },
        [t, r] => Sides { top: *t, right: *r, bottom: *t, left: *r },
        [t, r, b] => Sides { top: *t, right: *r, bottom: *b, left: *r },
        [t, r, b, l] => Sides { top: *t, right: *r, bottom: *b, left: *l },
        _ => Sides::default(),
    }
}

/// Split a shorthand value into its top-level whitespace-separated tokens,
/// keeping whitespace INSIDE balanced parentheses together —
/// `calc(100% - 30px) 5px` is two tokens, not four.
fn split_sides(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for (idx, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && ch.is_whitespace() {
            if let Some(s) = start.take() {
                out.push(&value[s..idx]);
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(s) = start {
        out.push(&value[s..]);
    }
    out
}

/// Reject `Length::Auto` entries (padding/border grammar: auto is illegal
/// there, CSS drops the whole declaration). Returns None if any side is auto.
fn sides_no_auto(s: &Sides) -> Option<Sides> {
    let ok = |v: Option<Length>| match v {
        Some(Length::Auto) => None,
        v => Some(v),
    };
    Some(Sides { top: ok(s.top)?, right: ok(s.right)?, bottom: ok(s.bottom)?, left: ok(s.left)? })
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
/// cascade's pre-pass via `font_size_px`. The relative keywords fold to em
/// factors (CSS 2.1 leaves the exact ratio UA-defined; 1.2 is the Chrome
/// ladder, so smaller == ÷1.2, larger == ×1.2).
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
        "larger" => Some(CssLength::Em(1.2)),
        "smaller" => Some(CssLength::Em(1.0 / 1.2)),
        _ => None,
    }
}

/// Fold a parsed font-size against its resolution bases.
fn font_size_px(l: CssLength, parent_fs: f32, root_fs: f32, viewport: (f32, f32)) -> f32 {
    match l {
        CssLength::Px(x) => x,
        CssLength::Em(n) => n * parent_fs,
        CssLength::Rem(n) => n * root_fs,
        CssLength::Percent(p) => p / 100.0 * parent_fs,
        CssLength::Vw(n) => n * viewport.0 / 100.0,
        CssLength::Vh(n) => n * viewport.1 / 100.0,
    }
}

/// Quote- and paren-aware token split for the `font` shorthand: quoted
/// family names ("Helvetica Neue", Arial) and calc() bodies stay one token.
fn split_font_tokens(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start: Option<usize> = None;
    for (idx, ch) in value.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                if start.is_none() {
                    start = Some(idx);
                }
            }
            '(' => {
                depth += 1;
                if start.is_none() {
                    start = Some(idx);
                }
            }
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && c.is_whitespace() => {
                if let Some(s) = start.take() {
                    out.push(&value[s..idx]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(idx);
                }
            }
        }
    }
    if let Some(s) = start {
        out.push(&value[s..]);
    }
    out
}

struct FontShorthand {
    weight: u16,
    size_px: Option<f32>,
    line_height: Option<LineHeightSpec>,
    family: String,
    small_caps: bool,
}

/// Parse the `font` shorthand grammar (system font keywords are not
/// modeled): leading style/variant/weight/stretch tokens in any order, then
/// the mandatory size (which may carry a `/line-height` with or without
/// surrounding whitespace), then the mandatory family as the verbatim
/// remainder. em font-size folds against the current context size — exact
/// when the shorthand is the only font-size declaration (own == inherited),
/// an approximation when an earlier declaration already set a size.
fn parse_font_shorthand(v: &str, fonts: &FontCtx) -> Option<FontShorthand> {
    let v = v.trim();
    let first = v.split_whitespace().next()?.to_ascii_lowercase();
    if matches!(
        first.as_str(),
        "caption" | "icon" | "menu" | "message-box" | "small-caption" | "status-bar"
            | "inherit" | "initial" | "unset" | "revert"
    ) {
        return None;
    }
    let toks = split_font_tokens(v);
    let mut weight = 400u16;
    let mut small_caps = false;
    let mut idx = 0usize;
    while idx < toks.len() {
        let t = toks[idx];
        if let Some(w) = parse_font_weight(t) {
            weight = w;
            idx += 1;
            continue;
        }
        match t.to_ascii_lowercase().as_str() {
            "small-caps" => {
                small_caps = true;
                idx += 1;
            }
            "italic" | "oblique" | "normal" | "semi-condensed" | "condensed"
            | "extra-condensed" | "ultra-condensed" | "semi-expanded" | "expanded"
            | "extra-expanded" | "ultra-expanded" | "bolder" | "lighter" => idx += 1,
            _ => break,
        }
    }
    // Mandatory size — may carry an attached `/line-height` (`20px/1.5`).
    // An unparseable size makes the whole shorthand invalid.
    let size_tok = toks.get(idx)?;
    let (size_str, mut next) = match size_tok.split_once('/') {
        Some((s, lh)) if !s.is_empty() => (s, Some(lh)),
        _ => (*size_tok, None),
    };
    let size = size_px(size_str, fonts)?;
    idx += 1;
    // Detached `/line-height`: `20px / 1.5` or `20px /1.5`.
    if next.is_none() {
        if let Some(t) = toks.get(idx) {
            if *t == "/" {
                next = Some(*toks.get(idx + 1)?);
                idx += 2;
            } else if let Some(rest) = t.strip_prefix('/') {
                next = Some(rest);
                idx += 1;
            }
        }
    }
    let line_height = next.and_then(parse_line_height).map(|raw| match raw {
        LineHeightRaw::Normal => LineHeightSpec::Normal,
        LineHeightRaw::Number(n) => LineHeightSpec::Number(n),
        LineHeightRaw::Len(CssLength::Px(px)) => LineHeightSpec::Px(px),
        // em folds against the shorthand's NEW size (spec: line-height
        // computes after font-size within the shorthand); % behaves like a
        // number multiplier, matching the line-height longhand arm.
        LineHeightRaw::Len(CssLength::Em(n)) => LineHeightSpec::Px(n * size),
        LineHeightRaw::Len(CssLength::Rem(n)) => LineHeightSpec::Px(n * fonts.root),
        LineHeightRaw::Len(CssLength::Percent(p)) => LineHeightSpec::Number(p / 100.0),
        LineHeightRaw::Len(CssLength::Vw(n)) => LineHeightSpec::Px(n * fonts.viewport_w / 100.0),
        LineHeightRaw::Len(CssLength::Vh(n)) => LineHeightSpec::Px(n * fonts.viewport_h / 100.0),
    });
    // Mandatory family: the verbatim remainder (quoted names included).
    if idx >= toks.len() {
        return None;
    }
    let family = toks[idx..].join(" ");
    Some(FontShorthand {
        weight,
        size_px: Some(size),
        line_height,
        family,
        small_caps,
    })
}

/// Fold a font-size token from the shorthand against the context. `None`
/// when the token is not a parseable size (the whole shorthand is invalid
/// without one).
fn size_px(size_str: &str, fonts: &FontCtx) -> Option<f32> {
    parse_font_size_len(size_str).map(|l| font_size_px(l, fonts.own, fonts.root, (fonts.viewport_w, fonts.viewport_h)))
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

/// `box-shadow` v1 parser (blitz#349 family): comma-separated `<shadow>`
/// layers, each `<color>? && <length>{2,4} && inset?` in any order. Two
/// lengths minimum (dx, dy, then blur, then spread); a negative blur
/// invalidates the layer, a negative spread is legal. A missing color folds
/// against the `color` value cascaded so far (the spec's order-dependent
/// currentColor rule). `inset` layers parse and report through the CSSOM
/// but never reach the paint half — documented v1 divergence.
pub fn parse_box_shadow(
    value: &str,
    current_color: Option<Color>,
    fonts: &FontCtx,
) -> Option<Vec<BoxShadow>> {
    let mut layers = Vec::new();
    for part in split_top_level_commas(value) {
        let mut inset = false;
        let mut color: Option<Color> = None;
        let mut lengths: Vec<f32> = Vec::new();
        for tok in split_top_level_units(&part) {
            if tok.eq_ignore_ascii_case("inset") {
                if inset {
                    return None;
                }
                inset = true;
            } else if let Some(c) = parse_color(tok) {
                if color.is_some() {
                    return None;
                }
                color = Some(c);
            } else {
                if lengths.len() >= 4 {
                    return None;
                }
                lengths.push(resolve_shadow_len(tok, fonts)?);
            }
        }
        if lengths.len() < 2 {
            return None;
        }
        let blur = lengths.get(2).copied().unwrap_or(0.0);
        if blur < 0.0 {
            return None;
        }
        layers.push(BoxShadow {
            dx: lengths[0],
            dy: lengths[1],
            blur,
            spread: lengths.get(3).copied().unwrap_or(0.0),
            color: color.unwrap_or_else(|| current_color.unwrap_or(Color(0, 0, 0, 255))),
            inset,
        });
    }
    Some(layers)
}

/// `text-shadow` layers (blitz#271 family): `<color>? && <length>{2,3}` per
/// comma-separated layer — no 4th length, no `inset` (either is an invalid
/// declaration, not a parse-through), negative blur invalid, missing color
/// folds to currentColor like box-shadow.
pub fn parse_text_shadow(
    value: &str,
    current_color: Option<Color>,
    fonts: &FontCtx,
) -> Option<Vec<TextShadow>> {
    let mut layers = Vec::new();
    for part in split_top_level_commas(value) {
        let mut color: Option<Color> = None;
        let mut lengths: Vec<f32> = Vec::new();
        for tok in split_top_level_units(&part) {
            if tok.eq_ignore_ascii_case("inset") {
                return None;
            } else if let Some(c) = parse_color(tok) {
                if color.is_some() {
                    return None;
                }
                color = Some(c);
            } else {
                if lengths.len() >= 3 {
                    return None;
                }
                lengths.push(resolve_shadow_len(tok, fonts)?);
            }
        }
        if lengths.len() < 2 {
            return None;
        }
        let blur = lengths.get(2).copied().unwrap_or(0.0);
        if blur < 0.0 {
            return None;
        }
        layers.push(TextShadow {
            dx: lengths[0],
            dy: lengths[1],
            blur,
            color: color.unwrap_or_else(|| current_color.unwrap_or(Color(0, 0, 0, 255))),
        });
    }
    Some(layers)
}

/// `backdrop-filter` v1: exactly one `blur(<length>)` function or `none`
/// (handled at the declaration arm). A filter-function list or any other
/// filter function is an invalid declaration, not a parse-through — the
/// prior computed value survives, matching the other shadow parsers.
pub fn parse_backdrop_blur(value: &str, fonts: &FontCtx) -> Option<f32> {
    let v = value.trim();
    let open = v.find('(')?;
    if !v[..open].eq_ignore_ascii_case("blur") || !v.ends_with(')') {
        return None;
    }
    let inner = &v[open + 1..v.len() - 1];
    if inner.trim() != inner || inner.trim().is_empty() {
        return None;
    }
    let px = resolve_shadow_len(inner, fonts)?;
    if px < 0.0 {
        return None;
    }
    Some(px)
}

/// Shadow lengths resolve em/rem against the cascade fonts and fold calc();
/// % is rejected (it would need the shadow receiver's box).
fn resolve_shadow_len(val: &str, fonts: &FontCtx) -> Option<f32> {
    if let Some(Length::Px(px)) = eval_math_call(val, fonts) {
        return Some(px);
    }
    match parse_css_length(val.trim()).map(|l| resolve_len(l, fonts)) {
        Some(Length::Px(px)) => Some(px),
        _ => None,
    }
}

/// Split on commas outside any parenthesis or quoted string — `rgb(1, 2, 3)`
/// stays one piece, `counters(x, ", ")` keeps its separator whole.
fn split_top_level_commas(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote: Option<u8> = None;
    let bytes = value.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(value[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(value[start..].trim().to_string());
    parts.retain(|p| !p.is_empty());
    parts
}

/// Whitespace split at parenthesis depth 0 — the functional token
/// (`rgb(0 0 0 / 50%)`) stays whole, its inner spaces unsplit.
fn split_top_level_units(value: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for (i, b) in value.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ if b.is_ascii_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    units.push(&value[s..i]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(s) = start {
        units.push(&value[s..]);
    }
    units
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

/// One parsed CSS `linear-gradient(...)`: stops as (0..1 position, color),
/// ascending by position, plus the CSS angle in degrees (0 = to top,
/// clockwise; 180 = the `linear-gradient(a, b)` default, to bottom).
/// Authored for the native PPTX exporter (`gradFill`), promoted to a
/// shared parse when the diting paint layer grew its own background-image
/// consumer — one grammar, two backends.
pub struct LinearGradient {
    pub stops: Vec<(f32, Color)>,
    pub css_deg: f32,
}

/// Parse a `linear-gradient(<angle>, <color> [pos%], ...)` value. Angle
/// forms: bare `135deg` or `to top/right/bottom/left` (corners rejected,
/// v1). Missing stop positions distribute evenly (all missing) or
/// interpolate between the nearest known neighbors (some missing), per the
/// CSS rule. Anything else — `none`, `url(...)`, radial/repeating — is
/// None so callers keep their fallback fill.
pub fn parse_linear_gradient(raw: &str) -> Option<LinearGradient> {
    let v = raw.trim().to_ascii_lowercase();
    let rest = v.strip_prefix("linear-gradient(")?.strip_suffix(')')?;
    // Top-level comma split (rgb()/rgba() carry commas of their own).
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in rest.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    // Two parts is the floor (`red, blue` with no angle spec); an angle
    // that starves the stop list falls through to the colors.len() gate.
    if parts.len() < 2 {
        return None;
    }

    // First part: angle spec or the first stop.
    let mut css_deg = 180.0; // CSS default: to bottom
    let mut stop_parts: &[String] = &parts;
    let first = parts[0].trim();
    if let Some(deg) = first.strip_suffix("deg") {
        css_deg = deg.trim().parse().ok()?;
        stop_parts = &parts[1..];
    } else if let Some(dir) = first.strip_prefix("to ") {
        css_deg = match dir.trim() {
            "top" => 0.0,
            "right" => 90.0,
            "bottom" => 180.0,
            "left" => 270.0,
            _ => return None, // corners: v1 doesn't map diagonal keywords
        };
        stop_parts = &parts[1..];
    }

    // Stops: "<color> [pos%]" — color first (may contain spaces inside
    // parens), an optional trailing percent position.
    let mut colors: Vec<Color> = Vec::new();
    let mut positions: Vec<Option<f32>> = Vec::new();
    for p in stop_parts {
        let p = p.trim();
        if p.is_empty() {
            return None;
        }
        // Whole part is a bare color (no position).
        if let Some(c) = parse_color(p) {
            colors.push(c);
            positions.push(None);
            continue;
        }
        let (head, tail) = p.rsplit_once(' ')?;
        let c = parse_color(head.trim())?;
        let pos = tail.trim().strip_suffix('%')?.trim().parse::<f32>().ok()? / 100.0;
        colors.push(c);
        positions.push(Some(pos));
    }
    if colors.len() < 2 {
        return None;
    }

    // Missing positions distribute evenly (all missing), or interpolate
    // between the nearest known neighbors (some missing) — the CSS
    // interpolation rule, to slide fidelity.
    let n = colors.len();
    let mut pos: Vec<f32> = vec![0.0; n];
    if positions.iter().all(|p| p.is_none()) {
        for (i, slot) in pos.iter_mut().enumerate() {
            *slot = i as f32 / (n - 1) as f32;
        }
    } else {
        positions[0] = positions[0].or(Some(0.0));
        positions[n - 1] = positions[n - 1].or(Some(1.0));
        for i in 0..n {
            if positions[i].is_none() {
                let (mut lo, mut hi) = (i, i);
                while lo > 0 && positions[lo].is_none() {
                    lo -= 1;
                }
                while hi + 1 < n && positions[hi].is_none() {
                    hi += 1;
                }
                let a = positions[lo].unwrap_or(0.0);
                let b = positions[hi].unwrap_or(1.0);
                pos[i] = a + (b - a) * (i - lo) as f32 / (hi - lo).max(1) as f32;
            } else {
                pos[i] = positions[i].unwrap_or(0.0);
            }
        }
    }

    let mut stops: Vec<(f32, Color)> = pos.into_iter().zip(colors).collect();
    // Out-of-order authored stops clamp up to their predecessor at raster
    // time per CSS; a stable sort is the equivalent here (equal positions
    // keep the hard-line transition).
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(LinearGradient { stops, css_deg })
}

// ---------------------------------------------------------------------------
// Cascade
// ---------------------------------------------------------------------------

/// One candidate: a compiled rule plus how strongly it applies.
struct CascadeCandidate<'a> {
    declarations: &'a str,
    specificity: u32,
    source_order: usize,
}

/// Compute the style for one element: UA defaults ← author rules (specificity,
/// then source order) ← inline style. Inherited properties (color, font-*,
/// text-align) fall back to the parent's computed values.
///
/// Read-only entry point: callers pass the tree, the element, its matched
/// rules (already filtered by the caller's selector matching), and the parent
/// style. This keeps the module decoupled from any particular matching
/// strategy while still locking ordering semantics.
#[allow(clippy::too_many_arguments)]
pub fn cascade_element(
    tag: &str,
    tree: &diting_dom::tree::DomTree,
    node_id: diting_dom::tree::NodeId,
    matched_rules: &[(&ParsedRule, u32)],
    parent: Option<&ComputedStyle>,
    inline_css: Option<&str>,
    root_font_size: f32,
    viewport: (f32, f32),
) -> ComputedStyle {
    let mut style = ComputedStyle {
        display: Some(ua_display(tag)),
        display_from_ua: true,
        font_weight: ua_font_weight(tag),
        text_align: ua_text_align(tag),
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
        // font-family inherits like the spec: the closest ancestor that
        // names a stack wins. `or` is enough because a child without its
        // own declaration carries None here.
        style.font_family = parent.font_family.clone();
        style.font_weight = style.font_weight.or(parent.font_weight);
        // `.or` (not overwrite): the element's own UA declaration beats an
        // inherited value — a th stays centered inside a text-align:right
        // ancestor, matching every browser UA sheet.
        style.text_align = style.text_align.or(parent.text_align);
        // Number keeps its multiplier for descendants (spec computed value);
        // Px inherits as absolute px — both copy straight through.
        style.line_height = parent.line_height;
        // Same inherited posture as text_align above: the element's own UA
        // declaration beats an inherited value, author rules below re-declare.
        style.word_spacing = style.word_spacing.or(parent.word_spacing);
        // white-space inherits; the element's own declaration wins.
        style.white_space = style.white_space.or(parent.white_space);
        // font-variant-caps inherits (small-caps synthesis flows down).
        style.font_variant_caps = style.font_variant_caps.or(parent.font_variant_caps);
        // `quotes` inherits so a pseudo's open-quote picks up an ancestor's
        // declared pairs.
        style.quotes = style.quotes.or(parent.quotes.clone());
        // text-shadow inherits the whole layer list (blitz#271 family) —
        // like color, author rules below re-declare per element.
        style.text_shadow = parent.text_shadow.clone();
        // Custom properties inherit computed (already-substituted-where-
        // possible) values; author rules below may re-declare per element.
        style.custom = parent.custom.clone();
    }
    // UA per-tag family/colors AFTER inherited defaults (an element's own
    // UA declaration beats an inherited value — same posture as text_align
    // above) and BEFORE author rules (author declarations override). mark is
    // yellow-on-black in every browser UA sheet; code/kbd/samp/tt are
    // monospace (obscura#936 table).
    if tag == "mark" {
        style.background_color = Some(Color(255, 255, 0, 255));
        style.color = Some(Color(0, 0, 0, 255));
    }
    if let Some(family) = ua_font_family(tag) {
        style.font_family = Some(family.to_string());
    }
    // UA decorations ride the same slot. The link rule is `a:-webkit-any-link`
    // upstream — an <a> without href is a named anchor and stays plain.
    if let Some(d) = ua_text_decoration(tag) {
        let hrefless_anchor = tag == "a"
            && !tree
                .with_node(node_id, |n| n.get_attribute("href").is_some())
                .unwrap_or(false);
        if !hrefless_anchor {
            style.text_decoration_line = Some(d);
        }
    }
    // UA baseline shifts (Chromium html.css: `sub { vertical-align: sub }`,
    // `sup { vertical-align: super }`); font-size: smaller already rides the
    // ua_font_size table. Author declarations below override.
    match tag {
        "sub" => style.vertical_align = Some(VerticalAlign::Sub),
        "sup" => style.vertical_align = Some(VerticalAlign::Super),
        _ => {}
    }
    // UA white-space (the preformatted group). Runs after inheritance, so a
    // plain `<pre>` inherits normal, then the UA default fills None — author
    // declarations still win.
    if let Some(ws) = ua_white_space(tag) {
        style.white_space = style.white_space.or(Some(ws));
    }

    // Author rules: sort by (specificity, source order) ascending, apply in
    // order so later/higher-specificity wins per property.
    let mut candidates: Vec<CascadeCandidate> = matched_rules
        .iter()
        .enumerate()
        .map(|(order, (rule, spec))| CascadeCandidate {
            declarations: &rule.declarations,
            specificity: *spec,
            source_order: order,
        })
        .collect();
    candidates.sort_by_key(|c| (c.specificity, c.source_order));

    // Font-size pre-pass: CSS computes font-size before every other
    // property (regardless of declaration order within a block), because
    // em lengths elsewhere resolve against it. Walk the same winning order
    // (sorted candidates, then inline) and keep the last parseable
    // declaration, then fold em/% against the PARENT size and rem against
    // the root size. The UA heading size (h1 2em …) is the lowest-priority
    // candidate — author declarations win over it.
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
    let fs_decl = fs_decl.or_else(|| ua_font_size(tag));
    let own_fs = fs_decl
        .map(|d| font_size_px(d, parent_fs, root_font_size, viewport))
        .unwrap_or(parent_fs);
    style.font_size = Some(own_fs);
    let fonts = FontCtx { own: own_fs, root: root_font_size, viewport_w: viewport.0, viewport_h: viewport.1 };

    // UA box defaults AFTER the fs pre-pass (em margins resolve against the
    // element's OWN font-size — h1's .67em × its 2em size) and BEFORE author
    // rules, so authored margin/padding overrides per side.
    if let Some(m) = ua_margin(tag) {
        let [t, r, b, l] = m.map(|side| resolve_len(side, &fonts));
        style.margin.top = style.margin.top.or(Some(t));
        style.margin.right = style.margin.right.or(Some(r));
        style.margin.bottom = style.margin.bottom.or(Some(b));
        style.margin.left = style.margin.left.or(Some(l));
    }
    if let Some(p) = ua_padding(tag) {
        let [t, r, b, l] = p.map(|side| resolve_len(side, &fonts));
        style.padding.top = style.padding.top.or(Some(t));
        style.padding.right = style.padding.right.or(Some(r));
        style.padding.bottom = style.padding.bottom.or(Some(b));
        style.padding.left = style.padding.left.or(Some(l));
    }

    // Presentational attribute hint (blitz#507): the `height` attribute on
    // td/th/tr is a px height per the HTML spec (bare number). It slots
    // below every author declaration — the author candidates below apply
    // after this and win unconditionally — but fills the slot an author
    // rule never touched. HTML-email bar charts are bare
    // `<td height="55">` columns; without this they collapse to 0.
    if matches!(tag, "td" | "th" | "tr") {
        let attr_h = tree
            .with_node(node_id, |n| {
                n.get_attribute("height")
                    .and_then(|v| v.trim().parse::<f32>().ok())
            })
            .flatten();
        if let Some(h) = attr_h {
            style.height = Some(Length::Px(h));
        }
    }
    // The `width` attribute, td/th arm of the same hint family: a px width
    // (bare number) slotted below every author declaration. Fixed table
    // layout and the auto column maxima both read it off the computed
    // style, so a bare `<td width="80">` pins its column with no CSS.
    if matches!(tag, "td" | "th") {
        let attr_w = tree
            .with_node(node_id, |n| {
                n.get_attribute("width")
                    .and_then(|v| v.trim().parse::<f32>().ok())
            })
            .flatten();
        if let Some(w) = attr_w {
            style.width = Some(Length::Px(w));
        }
    }
    // valign attribute (blitz#508), same hint slot: fills vertical_align
    // below every author declaration, so `td { vertical-align: top }`
    // outranks valign="middle" like in a real browser. middle (and unknown
    // values) stay None — the alignment site's UA default is middle anyway.
    if matches!(tag, "td" | "th") {
        let attr_va = tree
            .with_node(node_id, |n| {
                n.get_attribute("valign")
                    .map(|v| v.trim().to_ascii_lowercase())
            })
            .flatten()
            .and_then(|v| match v.as_str() {
                "top" => Some(VerticalAlign::Top),
                "bottom" => Some(VerticalAlign::Bottom),
                _ => None,
            });
        if attr_va.is_some() {
            style.vertical_align = attr_va;
        }
    }
    // Author declarations in two passes (CSS 2.1 §6.4.1). Normal pass:
    // stylesheet rules in (specificity, source order) sequence — inline
    // style last, the top of normal. Important pass on top of everything:
    // author !important beats every author-normal declaration INCLUDING
    // inline style, and inline !important beats stylesheet !important
    // (Chrome order). Without the second pass a print override like
    // `transform: none !important` would lose to the inline
    // translateX(...) a carousel script parks on every slide.
    for candidate in &candidates {
        apply_declarations_importance(
            &mut style,
            candidate.declarations,
            &fonts,
            Importance::Normal,
        );
    }
    // Inline style (same font context: inline em resolves against the
    // element's own font-size too).
    if let Some(inline) = inline_css {
        apply_declarations_importance(&mut style, inline, &fonts, Importance::Normal);
    }
    for candidate in &candidates {
        apply_declarations_importance(
            &mut style,
            candidate.declarations,
            &fonts,
            Importance::Important,
        );
    }
    if let Some(inline) = inline_css {
        apply_declarations_importance(&mut style, inline, &fonts, Importance::Important);
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
mod tests;
