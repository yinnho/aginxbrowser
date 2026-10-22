//! Float wrapping through taffy's float engine (#300).
//!
//! A container qualifies when it is a plain left-aligned block whose children
//! are explicit-width floats, block-level boxes and bare text. It then skips
//! the reified float zone: the floats become real taffy floats, the block
//! children overlap them like Chrome's, and text runs wrap into the free slots
//! beside them (`float_lines`). Everything else keeps the zone path.
//!
//! Off unless `AGINXBROWSER_FLOAT_WRAP=1` (or a test forces it).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

use taffy::prelude::*;
use taffy::Float;

use crate::diting_css::{ComputedStyle, Display as CssDisplay, FloatSide, Length, PositionMode, TextAlign};
use crate::diting_dom::tree::{DomTree, NodeId};

thread_local! {
    static FORCED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Test hook: force the gate on or off for this thread.
#[cfg(test)]
pub(crate) fn force(on: Option<bool>) {
    FORCED.with(|f| f.set(on));
}

pub(super) fn enabled() -> bool {
    if let Some(v) = FORCED.with(|f| f.get()) {
        return v;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("AGINXBROWSER_FLOAT_WRAP").is_ok_and(|v| v == "1"))
}

/// Style of a pure-text run leaf: a block item when the gate is on, so it
/// joins the block formatting context and can see the floats.
pub(super) fn run_style() -> Style {
    if enabled() {
        Style { display: Display::Block, ..Style::default() }
    } else {
        Style::default()
    }
}

pub(super) fn eligible(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    id: NodeId,
    container: &ComputedStyle,
    child_ids: &[NodeId],
) -> bool {
    if !enabled() {
        return false;
    }
    let block = matches!(container.display.unwrap_or(CssDisplay::Block), CssDisplay::Block);
    if !block || matches!(container.physical_text_align(), Some(TextAlign::Center) | Some(TextAlign::Right)) {
        return false;
    }
    // A box whose width comes from its content (floated, absolutely
    // positioned, or a flex/grid item) needs the float-aware intrinsic sizing
    // that is not wired yet; so does generated content (a `::after` clearfix
    // is a `clear`).
    let parent_is_block = tree
        .with_node(id, |n| n.parent)
        .flatten()
        .and_then(|p| styles.get(&p))
        .is_none_or(|ps| matches!(ps.display.unwrap_or(CssDisplay::Block), CssDisplay::Block));
    if container.float_side.is_some()
        || matches!(container.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        || !parent_is_block
        || container.pseudos.is_some()
    {
        return false;
    }
    let mut floats = 0;
    for cid in child_ids {
        if tree.with_node(*cid, |n| n.is_text()).unwrap_or(false) {
            continue;
        }
        let Some(s) = styles.get(cid) else { return false };
        let display = s.display.unwrap_or(CssDisplay::Block);
        if display == CssDisplay::None {
            continue;
        }
        if s.clear_side.is_some() || matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed)) {
            return false;
        }
        if s.float_side.is_some() {
            if !matches!(s.width, Some(Length::Px(_)) | Some(Length::Percent(_))) {
                return false;
            }
            floats += 1;
        } else if display != CssDisplay::Block {
            // Inline content beside a float needs the inline-run machinery.
            return false;
        }
    }
    floats > 0
}

/// Give a built float child taffy's `float`.
pub(super) fn mark(taffy_tree: &mut super::flow_tree::FlowTree<super::TextLeaf>, node: taffy::tree::NodeId, style: Option<&ComputedStyle>) {
    let Some(side) = style.and_then(|s| s.float_side) else { return };
    if let Ok(mut st) = taffy_tree.style(node).cloned() {
        st.float = match side {
            FloatSide::Left => Float::Left,
            FloatSide::Right => Float::Right,
        };
        let _ = taffy_tree.set_style(node, st);
    }
}
