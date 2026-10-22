//! CSS nesting (CSS Nesting 1): flatten a style rule whose block holds nested
//! rules into equivalent top-level CSS text, which then goes through the
//! ordinary parser. `&` becomes `:is(<parent>)` (so specificity is the
//! parent's, as in the spec), a nested selector without `&` is a descendant of
//! the parent (a leading `> + ~` combinator is kept), and conditional group
//! rules (`@media`, `@supports`, `@layer`, `@container`, `@scope`) nested in a
//! style rule wrap a copy of the parent rule around their declarations.

/// Split `body` into its top-level pieces: `(declaration, None)` for a
/// `;`-terminated or trailing declaration, `(prelude, Some(inner))` for a
/// `prelude { inner }` block. Quotes and parentheses (url(), calc()) are
/// opaque. `None` when `body` has no nested block.
fn pieces(body: &str) -> Option<Vec<(String, Option<String>)>> {
    let b: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut paren = 0i32;
    let mut quote: Option<char> = None;
    let mut nested = false;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' => quote = Some(c),
                '(' => paren += 1,
                ')' => paren -= 1,
                ';' if paren <= 0 => {
                    out.push((b[start..i].iter().collect::<String>(), None));
                    start = i + 1;
                }
                '{' if paren <= 0 => {
                    let mut depth = 1;
                    let mut j = i + 1;
                    let mut q2: Option<char> = None;
                    while j < b.len() && depth > 0 {
                        let d = b[j];
                        if let Some(q) = q2 {
                            if d == '\\' {
                                j += 1;
                            } else if d == q {
                                q2 = None;
                            }
                        } else if d == '"' || d == '\'' {
                            q2 = Some(d);
                        } else if d == '{' {
                            depth += 1;
                        } else if d == '}' {
                            depth -= 1;
                        }
                        j += 1;
                    }
                    let prelude: String = b[start..i].iter().collect();
                    let inner: String = b[i + 1..j.saturating_sub(1).max(i + 1)].iter().collect();
                    out.push((prelude, Some(inner)));
                    nested = true;
                    start = j;
                    i = j;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if start < b.len() {
        let rest: String = b[start..].iter().collect();
        if !rest.trim().is_empty() {
            out.push((rest, None));
        }
    }
    nested.then_some(out)
}

pub(crate) fn split_top_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut paren, mut bracket) = (0i32, 0i32);
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' => {
                paren += 1;
                cur.push(c);
            }
            ')' => {
                paren -= 1;
                cur.push(c);
            }
            '[' => {
                bracket += 1;
                cur.push(c);
            }
            ']' => {
                bracket -= 1;
                cur.push(c);
            }
            ',' if paren == 0 && bracket == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

/// Resolve a nested selector list against its parent.
fn resolve(parent: &str, nested: &str) -> String {
    let wrapped = format!(":is({parent})");
    split_top_commas(nested)
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p.contains('&') {
                p.replace('&', &wrapped)
            } else {
                // Bare selector = descendant; a leading `> + ~` combinator
                // reads the same way once the parent is prefixed.
                format!("{wrapped} {p}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

const GROUP_AT_RULES: &[&str] = &["media", "supports", "layer", "container", "scope"];

/// Flatten `sel { body }` when `body` nests rules; `None` otherwise.
pub(crate) fn flatten(sel: &str, body: &str) -> Option<String> {
    let parts = pieces(body)?;
    let mut own = String::new();
    let mut rest = String::new();
    for (head, inner) in parts {
        match inner {
            None => {
                let d = head.trim();
                if !d.is_empty() {
                    own.push_str(d);
                    own.push(';');
                }
            }
            Some(inner) => {
                let head = head.trim();
                if let Some(at) = head.strip_prefix('@') {
                    let name: String = at.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
                    if GROUP_AT_RULES.contains(&name.to_ascii_lowercase().as_str()) {
                        let body_css = flatten(sel, &inner).unwrap_or_else(|| format!("{sel}{{{inner}}}"));
                        rest.push_str(&format!("{head}{{{body_css}}}"));
                    }
                } else if head.starts_with("--") {
                    // A custom-property value that happens to hold braces.
                    own.push_str(&format!("{head}{{{inner}}};"));
                } else {
                    let resolved = resolve(sel, head);
                    let css = flatten(&resolved, &inner).unwrap_or_else(|| format!("{resolved}{{{inner}}}"));
                    rest.push_str(&css);
                }
            }
        }
    }
    let mut css = String::new();
    if !own.is_empty() {
        css.push_str(&format!("{sel}{{{own}}}"));
    }
    css.push_str(&rest);
    Some(css)
}
