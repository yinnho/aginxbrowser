//! Float wrapping through taffy's float engine (#300, widening #313).
//!
//! A container qualifies when it is a plain block whose children are
//! explicit-width floats (px/%), block-level boxes — including flex/grid,
//! which Chrome narrows as independent formatting contexts beside the
//! float — abspos/fixed boxes, generated content of any shape, and bare
//! text. It then skips the reified float zone: the floats become real
//! taffy floats, the block children overlap them like Chrome's, and text
//! runs wrap into the free slots beside them (`float_lines`).
//!
//! Still on the zone path (#313 remaining): hosts whose width comes from
//! their content (float/abspos/flex-item hosts need float-aware intrinsic
//! sizing), and a nowrap/pre inline element in a center/right host — it
//! rides the run wrapper as an unwrappable atom that has no band-relative
//! alignment hook yet. Float widths of every authored shape ride the wrap
//! path: px/% as-is, em/rem folded at cascade, calc repaired between two
//! solves (cache-busted when the fixup is a float), sizing keywords
//! measured at build. Center/right hosts wrap in every mounting shape:
//! bare text and one plain inline element of pure text (#314) mount their
//! run leaf directly, and mixed runs — several/nested inline elements, an
//! element among text, an inline-block (#314 tail) — ride the run wrapper,
//! whose inline_flow placement shifts whole lines from the wrapper's
//! justify (the host's text-align is baked there at flush_run; the
//! promote is suppressed for the wrap host). nowrap/pre runs take the
//! slots too: each hard line starts at its band and overflows right
//! (Chrome keeps a nowrap line on the float's line instead of dropping
//! below; a pre line below the float returns to the edge). A separate
//! face: a center/right host's float-free block child (a p) keeps the
//! promote and so never reaches inline_flow — its lines sit at the edge
//! instead of the band; that needs an alignment fallback for the no-float
//! promote, not this gate.
//!
//! Default on since the real-site gray round came back clean (bs3 docs,
//! a float-free portal, our own blog): opt out with
//! `AGINXBROWSER_FLOAT_WRAP=0` (or a test forcing it off).
//! `AGINXBROWSER_FLOAT_WRAP_ALL` is the #313 damage-experiment posture:
//! every float container takes the wrap path regardless of the gate, for
//! probing the next face before it's eaten.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

use taffy::prelude::*;
use taffy::{Clear, Float};

use crate::diting_css::{ClearSide, ComputedStyle, Display as CssDisplay, FloatSide, Length, PositionMode, TextAlign, WhiteSpace};
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
    *ON.get_or_init(|| !std::env::var("AGINXBROWSER_FLOAT_WRAP").is_ok_and(|v| v == "0"))
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
    // #313 damage-experiment posture: every float container takes the wrap
    // path. Diagnostic only — probe the next face before eating it.
    if std::env::var("AGINXBROWSER_FLOAT_WRAP_ALL").is_ok() {
        return true;
    }
    let block = matches!(container.display.unwrap_or(CssDisplay::Block), CssDisplay::Block);
    if !block {
        return false;
    }
    if matches!(container.physical_text_align(), Some(TextAlign::Center) | Some(TextAlign::Right)) {
        // Center/right hosts wrap (#313): bare text and single inline runs
        // mount their run leaf beside the float (float_lines shifts each
        // line inside its slot, #314), and mixed runs — several/nested
        // inline elements, an element among text, an inline-block — ride
        // the run wrapper, whose inline_flow placement now shifts whole
        // lines from the wrapper's justify. A nowrap/pre inline element
        // stays zone: its wrapper is an unwrappable atom with no
        // band-relative alignment.
        if container.pseudos.is_some() {
            return false;
        }
        for cid in child_ids {
            if tree.with_node(*cid, |n| n.is_text()).unwrap_or(false) {
                continue;
            }
            let Some(s) = styles.get(cid) else { return false };
            let display = s.display.unwrap_or(CssDisplay::Block);
            if display == CssDisplay::Inline {
                let ws = s.white_space.unwrap_or(WhiteSpace::Normal);
                if ws.no_soft_wrap() || ws.preserves_newlines() {
                    return false;
                }
            } else if !matches!(display, CssDisplay::Block | CssDisplay::InlineBlock | CssDisplay::Flex | CssDisplay::Grid) {
                return false;
            }
        }
    }
    // A box whose width comes from its content (floated, absolutely
    // positioned, or a flex/grid item) needs the float-aware intrinsic sizing
    // that is not wired yet.
    let parent_is_block = tree
        .with_node(id, |n| n.parent)
        .flatten()
        .and_then(|p| styles.get(&p))
        .is_none_or(|ps| matches!(ps.display.unwrap_or(CssDisplay::Block), CssDisplay::Block));
    if container.float_side.is_some()
        || matches!(container.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        || !parent_is_block
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
        // Abspos/fixed children ride the wrap path fine: they are out of
        // flow, the reparent pass moves them to their containing block, and
        // Chrome-truth probes (with and without insets) pin their placement
        // (#313). Flex/grid children are block-level siblings that Chrome
        // narrows as independent formatting contexts beside the float.
        if s.float_side.is_some() {
            // em/rem fold to Px at cascade; calc keeps its symbolic
            // percent+px mix (the calc repair pass fixes the true width
            // between two solves, and busts the layout cache when the fixup
            // was a float so its siblings' float_lines slots re-wrap); the
            // sizing keywords resolve to measured px at build time
            // (resolve_sizing_keywords).
            if !matches!(
                s.width,
                None
                    | Some(Length::Px(_))
                    | Some(Length::Percent(_))
                    | Some(Length::Calc { .. })
                    | Some(Length::MinContent)
                    | Some(Length::MaxContent)
                    | Some(Length::FitContent)
            ) {
                return false;
            }
            floats += 1;
        } else if !matches!(display, CssDisplay::Block | CssDisplay::Inline | CssDisplay::InlineBlock | CssDisplay::Flex | CssDisplay::Grid) {
            return false;
        }
    }
    floats > 0
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
