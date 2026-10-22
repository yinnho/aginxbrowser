//! Absolute page positions of every layout node, for the static-position
//! harvest.
//!
//! An absolute box's `location` is relative to its containing block (the
//! nearest positioned ancestor, else the root), not to its tree parent:
//! adding the parent chain on top doubled the offset of every auto-inset
//! absolute box deep in the page (#306).

use std::collections::HashMap;

use taffy::style::Position;

use super::{Rect, TaffyTree, TextLeaf};

pub(super) fn collect(
    taffy_tree: &TaffyTree<TextLeaf>,
    node: taffy::tree::NodeId,
    offset: (f32, f32),
    cb: (f32, f32),
    out: &mut HashMap<taffy::tree::NodeId, Rect>,
) {
    let Ok(layout) = taffy_tree.layout(node) else { return };
    let pos = taffy_tree.style(node).map(|s| s.position).unwrap_or_default();
    let base = if pos == Position::Absolute { cb } else { offset };
    let abs = (base.0 + layout.location.x, base.1 + layout.location.y);
    out.insert(node, Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height });
    let child_cb = if pos.is_positioned() { abs } else { cb };
    for child in taffy_tree.children(node).unwrap_or_default() {
        collect(taffy_tree, child, abs, child_cb, out);
    }
}
