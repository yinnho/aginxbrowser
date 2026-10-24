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
//! #315's non-summable-child face is on the wrap path too: a content-sized
//! host with an inline-block/flex/grid beside the float sums that box's
//! preferred width through the intrinsic repair; a plain block sibling
//! adds nothing (Chrome's preferred width stays the float alone) and still
//! wraps. Auto-width flex/grid ITEM hosts with a summable shape wrap via
//! the same intrinsic repair as floated/abspos hosts (definite-width items
//! already wrap). Float
//! widths of every authored shape ride the wrap
//! path: px/% as-is, em/rem folded at cascade, calc repaired between two
//! solves (cache-busted when the fixup is a float), sizing keywords
//! measured at build. Center/right hosts wrap in every mounting shape:
//! bare text and one plain inline element of pure text (#314) mount their
//! run leaf directly, and mixed runs — several/nested inline elements, an
//! element among text, an inline-block (#314 tail) — ride the run wrapper,
//! whose inline_flow placement shifts whole lines from the wrapper's
//! justify (the host's text-align is baked there at flush_run; the
//! promote is suppressed for the wrap host). nowrap/pre runs take the
//! slots too, and Chrome splits them by who owns the box: BARE text
//! (the host's own, a nowrap p) starts each hard line at its band and
//! overflows right — Chrome keeps the line on the float's line — while a
//! nowrap/pre ELEMENT box (a lone span, `atomic` on its run leaf) is
//! unbreakable and drops below the float when it fits no band, its
//! later hard lines continuing down (float_lines' atomic walk; the
//! walk's fallback slot answers with the query y, so clear reports the
//! real drop depth). A nowrap element among OTHER inline content rides
//! the run wrapper and drops the same way — inline_flow's first-item
//! walk moves a box no band fits past the floats (its own clear
//! rescue), the content after it continuing on the dropped line. The
//! same drop serves SOFT lines (#315's probe
//! round): §9.5 moves a line whose first word cannot fit its band below
//! the floats, the fitting words above staying beside. Content-sized
//! hosts (#315) ride the wrap path too — floated and abs/fixed boxes,
//! and auto-width flex/grid ITEM hosts — through the intrinsic repair
//! pass in mod.rs, which sums what taffy's max()-over-children
//! intrinsic drops: the float margin boxes and the caption text on one
//! line for max-content (a pre run its widest hard line), the widest
//! float against the longest word for min-content, avail against the
//! containing block (flex/grid item: the parent's content box). Those
//! hosts keep a summable shape (floats + inline runs + inline-block/flex/grid
//! preferred boxes); a plain block child adds nothing to the sum. A plain
//! block inside a wrap host (a centered p beside the host's float) inherits the
//! environment too: in CSS only a BFC boundary stops line boxes from
//! owing the ancestor floats a band, so mod.rs demotes the flex-column
//! promote inside the host's block flow — the block chain is the only
//! road taffy hands the BlockContext down — and the block's alignment
//! rides the run wrapper's justify like every other wrap host (#314
//! tail).
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

use crate::diting_css::{ClearSide, ComputedStyle, Display as CssDisplay, FloatSide, Length, TextAlign};
use crate::diting_dom::tree::{DomTree, NodeId};

thread_local! {
    static FORCED: Cell<Option<bool>> = const { Cell::new(None) };
    /// Building descendants of a wrap host right now. Plain blocks inside
    /// it stay plain: their lines owe the ancestor floats a band, and the
    /// flex-column promote would cut them out of it (mod.rs demotes).
    static ENV: Cell<bool> = const { Cell::new(false) };
}

/// Whether the element about to be built sits in a wrap host's block flow.
pub(super) fn in_env() -> bool {
    ENV.with(Cell::get)
}

/// What the container being built exposes to ITS children — its own floats
/// (wrap_v2) or the environment it inherited and passes on as a plain
/// block. BFC boundaries (flex/grid/table/abspos/floats) break the chain
/// by never setting this from inside.
pub(super) fn set_env(on: bool) {
    ENV.with(|c| c.set(on));
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
        // lines from the wrapper's justify. A nowrap inline element rides
        // the wrapper too: an element box too wide for its band drops
        // below the float (inline_flow's first-item walk). A pre element
        // among other content also rides the wrapper — its newlines are
        // exploded into parent line breaks at hoist (br shape), so lead
        // shares the first hard line and tail continues on the next.
        if container.pseudos.is_some() {
            return false;
        }
    }
    // Auto-width hosts that shrink-to-fit — floated, abs/fixed, or a
    // flex/grid ITEM — ride the wrap path when their children are a
    // summable shape (floats + inline). The intrinsic repair pass in
    // mod.rs sums what taffy's max()-over-children drops. Definite-width
    // flex/grid items wrap like any block host. Other non-block parents
    // with an auto-width host stay zone (no repair wired).
    let parent_display = tree
        .with_node(id, |n| n.parent)
        .flatten()
        .and_then(|p| styles.get(&p))
        .map(|ps| ps.display.unwrap_or(CssDisplay::Block));
    let parent_is_block = parent_display.is_none_or(|d| matches!(d, CssDisplay::Block));
    let parent_is_flex_grid = matches!(parent_display, Some(CssDisplay::Flex) | Some(CssDisplay::Grid));
    if !parent_is_block && container.width.is_none() && !parent_is_flex_grid {
        return false;
    }
    // Content-sized hosts (float/abspos/flex-item) admit the same child
    // shapes as definite-width hosts. The #315 repair sums floats + inline
    // runs + inline-block/flex/grid preferred boxes; a plain block child
    // contributes nothing (Chrome's host preferred width stays the float).
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
        } else if !matches!(
            display,
            CssDisplay::Block
                | CssDisplay::Inline
                | CssDisplay::InlineBlock
                | CssDisplay::Flex
                | CssDisplay::Grid
        ) {
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
