//! `@scope (root) [to (limit)] { … }` (CSS Cascade 6).
//!
//! The inner rules are parsed as ordinary rules, then each selector is
//! re-rooted: `S` becomes `:where(root) S` (the root adds no specificity),
//! `:scope` becomes `:is(root)`, and a `to (limit)` lower bound excludes the
//! limit elements and everything under them inside the root.

use super::nesting::split_top_commas;

/// The first balanced `( … )` group of `s` and what follows it.
fn paren_group(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let body = s.strip_prefix('(')?;
    let mut depth = 1;
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&body[..i], &body[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// `(root) [to (limit)]` → (root selector list, optional limit list). A
/// prelude-less `@scope` has no ancestor to root at: `None`.
pub(super) fn parse_prelude(prelude: &str) -> Option<(String, Option<String>)> {
    let (root, rest) = paren_group(prelude)?;
    let limit = rest.trim().strip_prefix("to").and_then(paren_group).map(|(l, _)| l.trim().to_string());
    Some((root.trim().to_string(), limit))
}

/// Re-root one selector list under the scope.
pub(super) fn rewrite(selector: &str, root: &str, limit: Option<&str>) -> String {
    let bound = limit
        .map(|l| format!(":not(:is({root}) :is({l}), :is({root}) :is({l}) *)"))
        .unwrap_or_default();
    split_top_commas(selector)
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains(":scope") {
                format!("{}{bound}", s.replace(":scope", &format!(":is({root})")))
            } else {
                format!(":where({root}) {s}{bound}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
