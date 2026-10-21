//! `var()` substitution (god-file ratchet split from mod.rs, pure move).

/// Replace every `var(--name[, fallback])` in `value` using `custom`.
/// `None` when any reference is unresolvable (no such property AND no
/// usable fallback) — the caller then drops the declaration (IACVT).
/// Depth-capped to break `--a: var(--b); --b: var(--a)` cycles.
pub(super) fn substitute_vars(value: &str, custom: &std::collections::HashMap<String, String>, depth: usize) -> Option<String> {
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
    let replacement = match super::props::lookup(custom, name) {
        Some(v) => substitute_vars(&v, custom, depth + 1)?,
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
