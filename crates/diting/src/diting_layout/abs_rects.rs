//! Absolute page positions of every layout node, for the static-position
//! harvest.
//!
//! An absolute box's `location` is relative to its containing block (the
//! nearest positioned ancestor, else the root), not to its tree parent:
//! adding the parent chain on top doubled the offset of every auto-inset
//! absolute box deep in the page (#306).
//!
//! An ancestor's translate moves the whole subtree visually, and a static
//! position is the box's VISUAL flow spot, so the walk folds each
//! transform's translate into the offsets it hands down (#223: a fixed box
//! under a transformed ancestor takes its auto-inset coordinate from the
//! transformed origin, Chrome-truth). The box's own translate does not
//! enter its OWN record — paint applies that later, and the static
//! position is the pre-transform placement.

use std::collections::HashMap;

use taffy::style::Position;

use super::{Rect, TaffyTree, TextLeaf};
use crate::diting_css::{ComputedStyle, Length};
use crate::diting_dom::tree::NodeId;

fn translate_px(l: &Length, basis: f32) -> f32 {
    match l {
        Length::Px(v) => *v,
        Length::Percent(p) => p / 100.0 * basis,
        // Percent-only approximation, same as to_taffy_style.
        Length::Calc { percent, .. } => percent / 100.0 * basis,
        _ => 0.0,
    }
}

pub(super) fn collect(
    taffy_tree: &TaffyTree<TextLeaf>,
    styles: &HashMap<NodeId, ComputedStyle>,
    node_map: &HashMap<taffy::tree::NodeId, NodeId>,
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
    let mut child_base = abs;
    if let Some(t) = node_map.get(&node).and_then(|d| styles.get(d)).and_then(|s| s.effective_transform()) {
        child_base.0 += translate_px(&t.tx, layout.size.width);
        child_base.1 += translate_px(&t.ty, layout.size.height);
    }
    let child_cb = if pos.is_positioned() { child_base } else { cb };
    for child in taffy_tree.children(node).unwrap_or_default() {
        collect(taffy_tree, styles, node_map, child, child_base, child_cb, out);
    }
}
