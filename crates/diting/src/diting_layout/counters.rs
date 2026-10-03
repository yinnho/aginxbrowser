//! CSS counters, quotes nesting, and `q`-element UA marks — the
//! generated-content family split from mod.rs at the god-file ratchet's
//! demand. Behavior-neutral move; callers stay in mod.rs.

use std::collections::HashMap;

use taffy::prelude::*;

use crate::diting_css::ComputedStyle;
use crate::diting_dom::tree::{DomTree, NodeId};

use super::{
    build_word_leaves, color_context, decoration_context, font_context, han_context,
    mono_context, small_caps_context, valign_shift, word_spacing_context, FontBook, TextLeaf,
};

/// Chrome's UA sheet gives `q::before/::after` the `open-quote`/`close-quote`
/// keywords, which resolve through auto quote nesting: even depth uses double
/// curly quotes, odd depth flips to singles.
pub(crate) fn q_quote_pair(depth: usize) -> (&'static str, &'static str) {
    if depth.is_multiple_of(2) {
        ("\u{201C}", "\u{201D}")
    } else {
        ("\u{2018}", "\u{2019}")
    }
}

fn q_ancestor_quote_depth(tree: &DomTree, mut id: NodeId) -> usize {
    let mut depth = 0usize;
    while let Some(parent) = tree.with_node(id, |n| n.parent).flatten() {
        let is_q = tree
            .with_node(parent, |n| {
                n.as_element().map(|e| e.local.to_string() == "q")
            })
            .flatten()
            .unwrap_or(false);
        if is_q {
            depth += 1;
        }
        id = parent;
    }
    depth
}

/// CSS counter state for one compute_styles pass: per-name stacks of
/// (value, walk depth of the element that created it) plus the
/// generated-quote nesting depth. A counter created at depth d stays
/// visible to the creator's following siblings and their subtrees, and
/// pops when the walk leaves its creating parent — the css-lists-3 scope
/// rule that makes `ol{counter-reset} li::before{counter-increment}`
/// number nested lists as "1.1" and the next outer item as plain "2".
#[derive(Default)]
pub(super) struct CounterState {
    map: HashMap<String, Vec<(i64, usize)>>,
    quote_depth: usize,
}

pub(super) fn apply_counter_modifiers(
    state: &mut CounterState,
    reset: &[(String, i32)],
    increment: &[(String, i32)],
    depth: usize,
) {
    // Reset pushes a counter nested inside any ancestor-origin same-name
    // counter, but shadows every same-name counter created at this depth
    // or deeper (the previous-sibling removal of css-lists-3 §4.4.2 —
    // sibling <ol>s each restart at 1). Increment then bumps the
    // innermost; both compose on one element.
    for (name, v) in reset {
        let entry = state.map.entry(name.clone()).or_default();
        while matches!(entry.last(), Some(e) if e.1 >= depth) {
            entry.pop();
        }
        entry.push((*v as i64, depth));
    }
    for (name, d) in increment {
        let entry = state.map.entry(name.clone()).or_default();
        if entry.is_empty() {
            entry.push((0, depth));
        }
        if let Some(last) = entry.last_mut() {
            last.0 += *d as i64;
        }
    }
}

/// End-of-subtree scope pop: counters created strictly inside the exiting
/// element die with their creating parent; ones created at this depth or
/// shallower survive for the following siblings.
pub(super) fn pop_out_of_scope(state: &mut CounterState, depth: usize) {
    for stack in state.map.values_mut() {
        while matches!(stack.last(), Some(e) if e.1 > depth) {
            stack.pop();
        }
    }
}

/// The depth'th declared pair, repeating the last pair once the depth runs
/// past it; `quotes: none` renders nothing and no declared `quotes` falls
/// back to the same curly pair the UA `q` marks use.
fn quote_pair(quotes: Option<&[String]>, depth: usize) -> (&str, &str) {
    match quotes {
        Some([]) => ("", ""),
        Some(q) => {
            let n = q.len() / 2;
            let i = depth.min(n - 1) * 2;
            (q[i].as_str(), q[i + 1].as_str())
        }
        None if depth.is_multiple_of(2) => ("\u{201C}", "\u{201D}"),
        None => ("\u{2018}", "\u{2019}"),
    }
}

pub(super) fn resolve_content_into(
    cv: &crate::diting_css::ContentValue,
    tree: &DomTree,
    nid: NodeId,
    state: &mut CounterState,
    quotes: Option<&[String]>,
    out: &mut String,
) {
    use crate::diting_css::ContentValue;
    match cv {
        ContentValue::Str(s) => out.push_str(s),
        ContentValue::Attr(name) => {
            let v = tree
                .with_node(nid, |n| n.get_attribute(name).map(|s| s.to_string()))
                .flatten();
            out.push_str(&v.unwrap_or_default());
        }
        ContentValue::Counter { name, style } => {
            let v = state
                .map
                .get(name)
                .and_then(|s| s.last())
                .map(|e| e.0)
                .unwrap_or(0);
            out.push_str(&crate::diting_css::format_counter_value(v, *style));
        }
        ContentValue::Counters { name, sep, style } => {
            let stack = state.map.get(name);
            match stack {
                Some(s) if !s.is_empty() => {
                    let parts: Vec<String> = s
                        .iter()
                        .map(|e| crate::diting_css::format_counter_value(e.0, *style))
                        .collect();
                    out.push_str(&parts.join(sep));
                }
                _ => out.push_str(&crate::diting_css::format_counter_value(0, *style)),
            }
        }
        ContentValue::OpenQuote => {
            let (open, _) = quote_pair(quotes, state.quote_depth);
            out.push_str(open);
            state.quote_depth += 1;
        }
        ContentValue::CloseQuote => {
            // Unbalanced close-quote (nothing open) generates nothing.
            if state.quote_depth > 0 {
                state.quote_depth -= 1;
                let (_, close) = quote_pair(quotes, state.quote_depth);
                out.push_str(close);
            }
        }
        ContentValue::NoQuote { close } => {
            if *close {
                state.quote_depth = state.quote_depth.saturating_sub(1);
            } else {
                state.quote_depth += 1;
            }
        }
        ContentValue::List(parts) => {
            for part in parts {
                resolve_content_into(part, tree, nid, state, quotes, out);
            }
        }
    }
}

/// diting has no ::before/::after generated content; the `q` marks are the
/// one piece of UA-generated text real pages rely on, so synthesize the
/// open/close leaves directly around the flattened q's children, in the q's
/// own font context.
#[allow(clippy::too_many_arguments)]
pub(super) fn wrap_q_quotes(
    tree: &DomTree,
    child: NodeId,
    child_tag: &str,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    sub_children: &mut Vec<taffy::tree::NodeId>,
) {
    if child_tag != "q" {
        return;
    }
    let (open, close) = q_quote_pair(q_ancestor_quote_depth(tree, child));
    let (fs, b, lh) = font_context(tree, child, styles, fonts);
    let col = color_context(tree, child, styles);
    let deco = decoration_context(tree, child, styles);
    let vs = valign_shift(tree, child, styles, fonts);
    let mono = mono_context(tree, child, styles);
    let ws = word_spacing_context(tree, child, styles);
    let sc = small_caps_context(tree, child, styles);
    let han = han_context(tree, child, styles);
    let mut wrapped = build_word_leaves(open, fs, b, col, lh, deco, vs, mono, ws, sc, han, fonts, taffy_tree);
    wrapped.append(sub_children);
    wrapped.extend(build_word_leaves(close, fs, b, col, lh, deco, vs, mono, ws, sc, han, fonts, taffy_tree));
    *sub_children = wrapped;
}
