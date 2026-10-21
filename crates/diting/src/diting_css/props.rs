//! `@property` / `CSS.registerProperty` registry (CSS Properties & Values).
//!
//! What it models: the registered initial value as the fallback for an unset
//! custom property (cascade `var()` and the computed-style snapshot),
//! `inherits: false` (a registered property is not copied down from its
//! parent), and syntax validation (a declaration whose value does not match a
//! non-`*` syntax is ignored, so the property falls back to its initial
//! value). Not modelled: typed animation, computed-value normalisation
//! (`1in` -> `96px`).
//!
//! The registry is thread-local: style resolution is single-threaded per
//! session. `reset_for_parse` is called when a document's stylesheets are
//! (re)parsed; the rules found there land via `define_from_rule`, and
//! `CSS.registerProperty` calls ride in through `register_js`, which survive
//! reparses until the runtime is rebuilt (`clear_js`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
enum Alt {
    Literal(String),
    Typed { ty: String, mult: char },
}

#[derive(Clone, Debug)]
struct RegProp {
    /// `None` = universal syntax (`*`).
    alts: Option<Vec<Alt>>,
    inherits: bool,
    initial: Option<String>,
}

#[derive(Default)]
struct Registry {
    map: HashMap<String, RegProp>,
    non_inherit: Vec<String>,
}

thread_local! {
    static ACTIVE: RefCell<Registry> = RefCell::new(Registry::default());
    static JS: RefCell<HashMap<String, RegProp>> = RefCell::new(HashMap::new());
}

fn rebuild(r: &mut Registry) {
    r.non_inherit = r.map.iter().filter(|(_, p)| !p.inherits).map(|(n, _)| n.clone()).collect();
}

fn is_css_wide(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    )
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    let first_ok = first.is_alphabetic() || first == '_' || first as u32 >= 0x80 || first == '-';
    if !first_ok {
        return false;
    }
    if first == '-' && !s.starts_with("--") && !s.chars().nth(1).is_some_and(|c| c.is_alphabetic() || c == '_' || c as u32 >= 0x80) {
        return false;
    }
    s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c as u32 >= 0x80)
}

/// Split `1.5e3px` into (number, unit). `None` when it does not start with
/// a CSS number.
fn split_num(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut seen = i > digits_start;
    if i < b.len() && b[i] == b'.' {
        let frac = i + 1;
        let mut j = frac;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > frac {
            seen = true;
            i = j;
        } else if seen {
            // "1." is not a CSS number; stop before the dot.
        }
    }
    if !seen {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp {
            i = j;
        }
    }
    Some((&s[..i], &s[i..]))
}

const ABS_LEN: &[&str] = &["px", "cm", "mm", "q", "in", "pt", "pc"];
const REL_LEN: &[&str] = &[
    "em", "rem", "ex", "rex", "cap", "rcap", "ch", "rch", "ic", "ric", "lh", "rlh", "vw", "vh", "vi", "vb", "vmin",
    "vmax", "svw", "svh", "lvw", "lvh", "dvw", "dvh", "cqw", "cqh", "cqi", "cqb", "cqmin", "cqmax",
];

fn is_math_fn(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    (l.starts_with("calc(") || l.starts_with("min(") || l.starts_with("max(") || l.starts_with("clamp(")) && l.ends_with(')')
}

fn type_matches(ty: &str, v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() {
        return false;
    }
    match ty {
        "number" => is_math_fn(v) || matches!(split_num(v), Some((_, u)) if u.is_empty()),
        "integer" => {
            is_math_fn(v) || matches!(split_num(v), Some((n, u)) if u.is_empty() && !n.contains(['.', 'e', 'E']))
        }
        "percentage" => is_math_fn(v) || matches!(split_num(v), Some((_, "%"))),
        "length" | "length-percentage" => {
            if is_math_fn(v) {
                return true;
            }
            match split_num(v) {
                Some((n, u)) => {
                    let ul = u.to_ascii_lowercase();
                    ABS_LEN.contains(&ul.as_str())
                        || REL_LEN.contains(&ul.as_str())
                        || (u.is_empty() && n.trim_start_matches(['+', '-']).chars().all(|c| c == '0' || c == '.'))
                        || (ty == "length-percentage" && u == "%")
                }
                None => false,
            }
        }
        "angle" => {
            is_math_fn(v)
                || matches!(split_num(v), Some((n, u)) if ["deg", "grad", "rad", "turn"].contains(&u.to_ascii_lowercase().as_str())
                    || (u.is_empty() && n.trim_start_matches(['+', '-']).chars().all(|c| c == '0' || c == '.')))
        }
        "time" => is_math_fn(v) || matches!(split_num(v), Some((_, u)) if ["s", "ms"].contains(&u.to_ascii_lowercase().as_str())),
        "resolution" => {
            is_math_fn(v)
                || matches!(split_num(v), Some((_, u)) if ["dpi", "dpcm", "dppx", "x"].contains(&u.to_ascii_lowercase().as_str()))
        }
        "color" => super::parse_color(v).is_some() || v.eq_ignore_ascii_case("currentcolor") || v.eq_ignore_ascii_case("transparent"),
        "custom-ident" => is_ident(v) && !is_css_wide(v),
        "url" => v.to_ascii_lowercase().starts_with("url(") && v.ends_with(')'),
        "image" => {
            let l = v.to_ascii_lowercase();
            v.ends_with(')')
                && (l.starts_with("url(")
                    || l.starts_with("image-set(")
                    || l.contains("-gradient(")
                    || l.starts_with("cross-fade("))
        }
        "transform-function" => v.ends_with(')') && v.find('(').is_some_and(|i| i > 0 && is_ident(&v[..i])),
        "transform-list" => {
            // A run of transform functions: every top-level token is `name(...)`.
            let mut depth = 0i32;
            let mut ok = true;
            let mut last_close = true;
            for c in v.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth < 0 {
                            ok = false;
                        }
                        last_close = depth == 0;
                    }
                    c if depth == 0 && !c.is_whitespace() => last_close = false,
                    _ => {}
                }
            }
            ok && depth == 0 && last_close && v.contains('(')
        }
        _ => false,
    }
}

const TYPES: &[&str] = &[
    "length", "number", "percentage", "length-percentage", "color", "image", "url", "integer", "angle", "time",
    "resolution", "transform-function", "transform-list", "custom-ident",
];

/// `Some(None)` = universal `*`; `Some(Some(alts))` = a data-type grammar;
/// `None` = not a valid syntax string.
fn parse_syntax(s: &str) -> Option<Option<Vec<Alt>>> {
    let s = s.trim();
    if s == "*" {
        return Some(None);
    }
    if s.is_empty() {
        return None;
    }
    let mut alts = Vec::new();
    for part in s.split('|') {
        let p = part.trim();
        if let Some(body) = p.strip_prefix('<') {
            let (ty, mult) = match body.strip_suffix(">+") {
                Some(t) => (t, '+'),
                None => match body.strip_suffix(">#") {
                    Some(t) => (t, '#'),
                    None => (body.strip_suffix('>')?, ' '),
                },
            };
            if !TYPES.contains(&ty) || (mult != ' ' && ty == "transform-list") {
                return None;
            }
            alts.push(Alt::Typed { ty: ty.to_string(), mult });
        } else if is_ident(p) && !is_css_wide(p) {
            alts.push(Alt::Literal(p.to_string()));
        } else {
            return None;
        }
    }
    Some(Some(alts))
}

fn alts_match(alts: &[Alt], value: &str) -> bool {
    alts.iter().any(|a| match a {
        Alt::Literal(l) => value == l,
        Alt::Typed { ty, mult: ' ' } => type_matches(ty, value),
        Alt::Typed { ty, mult: '+' } => {
            let items: Vec<&str> = value.split_whitespace().collect();
            !items.is_empty() && items.iter().all(|i| type_matches(ty, i))
        }
        Alt::Typed { ty, .. } => value.split(',').all(|i| type_matches(ty, i)),
    })
}

/// An initial value must parse for the syntax and not depend on its
/// environment (relative lengths, `var()`).
fn initial_ok(alts: &[Alt], v: &str) -> bool {
    if is_css_wide(v) || v.to_ascii_lowercase().contains("var(") || !alts_match(alts, v) {
        return false;
    }
    !v.split(|c: char| c.is_whitespace() || c == ',').any(|tok| {
        split_num(tok).is_some_and(|(_, u)| REL_LEN.contains(&u.to_ascii_lowercase().as_str()))
    })
}

fn build(syntax: &str, inherits: bool, initial: Option<&str>) -> Option<RegProp> {
    let alts = parse_syntax(syntax)?;
    let initial = initial.map(|s| s.trim().to_string());
    match &alts {
        None => {}
        Some(a) => {
            if !initial_ok(a, initial.as_deref()?) {
                return None;
            }
        }
    }
    Some(RegProp { alts, inherits, initial })
}

fn insert(name: &str, p: RegProp) {
    ACTIVE.with(|r| {
        let mut r = r.borrow_mut();
        r.map.insert(name.to_string(), p);
        rebuild(&mut r);
    });
}

/// A new stylesheet parse starts: drop the previous document's `@property`
/// rules, keep the `CSS.registerProperty` entries.
pub(crate) fn reset_for_parse() {
    let js = JS.with(|j| j.borrow().clone());
    ACTIVE.with(|r| {
        let mut r = r.borrow_mut();
        r.map = js;
        rebuild(&mut r);
    });
}

/// `@property --name { syntax; inherits; initial-value }`. An invalid rule is
/// ignored whole (as in Chrome).
pub(crate) fn define_from_rule(prelude: &str, inner: &str) {
    let name = prelude.trim();
    if !name.starts_with("--") || name.len() < 3 {
        return;
    }
    let (mut syntax, mut inherits, mut initial) = (None, None, None);
    for (k, v) in super::split_declarations(inner) {
        let v = v.trim();
        match k.as_str() {
            "syntax" => syntax = Some(v.trim_matches(['"', '\'']).to_string()),
            "inherits" => {
                inherits = match v.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            "initial-value" => initial = Some(v.to_string()),
            _ => {}
        }
    }
    let (Some(syntax), Some(inherits)) = (syntax, inherits) else { return };
    if let Some(p) = build(&syntax, inherits, initial.as_deref()) {
        insert(name, p);
    }
}

/// `CSS.registerProperty` (already validated on the JS side).
pub(crate) fn register_js(name: &str, syntax: &str, inherits: bool, initial: Option<&str>) {
    if let Some(p) = build(syntax, inherits, initial) {
        JS.with(|j| j.borrow_mut().insert(name.to_string(), p.clone()));
        insert(name, p);
    }
}

/// The runtime was rebuilt: forget script registrations.
pub(crate) fn clear_js() {
    JS.with(|j| j.borrow_mut().clear());
    ACTIVE.with(|r| {
        let mut r = r.borrow_mut();
        r.map.clear();
        rebuild(&mut r);
    });
}

/// Declaring `name: value`: false when `name` is registered with a data-type
/// syntax and the value does not match (the declaration is then ignored).
pub(crate) fn value_acceptable(name: &str, value: &str) -> bool {
    ACTIVE.with(|r| {
        let r = r.borrow();
        let Some(Some(alts)) = r.map.get(name).map(|p| p.alts.as_ref()) else { return true };
        is_css_wide(value) || value.to_ascii_lowercase().contains("var(") || alts_match(alts, value.trim())
    })
}

/// The value `var(name)` sees: the element's own/inherited value, else the
/// registered initial value.
pub(crate) fn lookup(custom: &HashMap<String, String>, name: &str) -> Option<String> {
    if let Some(v) = custom.get(name) {
        return Some(v.clone());
    }
    ACTIVE.with(|r| r.borrow().map.get(name).and_then(|p| p.initial.clone()))
}

/// The custom-property map a child starts from: the parent's, minus the
/// registered `inherits: false` names.
pub(crate) fn inherit_custom(parent: &Arc<HashMap<String, String>>) -> Arc<HashMap<String, String>> {
    ACTIVE.with(|r| {
        let r = r.borrow();
        if !r.non_inherit.iter().any(|n| parent.contains_key(n)) {
            return parent.clone();
        }
        let mut m = (**parent).clone();
        for n in &r.non_inherit {
            m.remove(n);
        }
        Arc::new(m)
    })
}

/// Registered initial values for names the element's map does not carry —
/// the computed-style snapshot adds them so `getComputedStyle(el)
/// .getPropertyValue('--x')` sees what `var(--x)` sees.
pub(crate) fn missing_initials(custom: &HashMap<String, String>) -> Vec<(String, String)> {
    ACTIVE.with(|r| {
        r.borrow()
            .map
            .iter()
            .filter(|(n, _)| !custom.contains_key(*n))
            .filter_map(|(n, p)| p.initial.clone().map(|i| (n.clone(), i)))
            .collect()
    })
}

