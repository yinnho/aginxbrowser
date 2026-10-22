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
use taffy::{Clear, Float};

use crate::diting_css::{ClearSide, ComputedStyle, ContentValue, Display as CssDisplay, FloatSide, Length, PositionMode, TextAlign};
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
    // that is not wired yet. Generated content must be clearfix boxes.
    let parent_is_block = tree
        .with_node(id, |n| n.parent)
        .flatten()
        .and_then(|p| styles.get(&p))
        .is_none_or(|ps| matches!(ps.display.unwrap_or(CssDisplay::Block), CssDisplay::Block));
    if container.float_side.is_some()
        || matches!(container.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        || !parent_is_block
        || !container.pseudos.as_deref().is_none_or(|p| [&p.before, &p.after].into_iter().flatten().all(clearfix_box))
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
        if matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed)) {
            return false;
        }
        if s.float_side.is_some() {
            if !matches!(s.width, Some(Length::Px(_)) | Some(Length::Percent(_))) {
                return false;
            }
            floats += 1;
        } else if !matches!(display, CssDisplay::Block | CssDisplay::Inline | CssDisplay::InlineBlock) {
            return false;
        }
    }
    floats > 0
}

/// An empty generated box that exists to clear floats (`content:""; clear:both`,
/// the Bootstrap clearfix), or one that is not rendered.
fn clearfix_box(p: &ComputedStyle) -> bool {
    p.display == Some(CssDisplay::None)
        || (p.clear_side.is_some() && matches!(&p.content, Some(ContentValue::Str(t)) if t.is_empty()))
}

/// taffy's `clear` for a CSS one (logical sides are left/right in this LTR engine).
pub(super) fn clear_of(side: Option<ClearSide>) -> Clear {
    match side {
        Some(ClearSide::Left) | Some(ClearSide::InlineStart) => Clear::Left,
        Some(ClearSide::Right) | Some(ClearSide::InlineEnd) => Clear::Right,
        Some(ClearSide::Both) => Clear::Both,
        None => Clear::None,
    }
}

/// Give a built child taffy's `float` / `clear`.
pub(super) fn mark(taffy_tree: &mut super::flow_tree::FlowTree<super::TextLeaf>, node: taffy::tree::NodeId, style: Option<&ComputedStyle>) {
    let Some(style) = style else { return };
    if style.float_side.is_none() && style.clear_side.is_none() {
        return;
    }
    if let Ok(mut st) = taffy_tree.style(node).cloned() {
        st.float = match style.float_side {
            Some(FloatSide::Left) => Float::Left,
            Some(FloatSide::Right) => Float::Right,
            None => Float::None,
        };
        st.clear = clear_of(style.clear_side);
        let _ = taffy_tree.set_style(node, st);
    }
}
