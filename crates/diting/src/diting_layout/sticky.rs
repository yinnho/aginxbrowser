// #434 sticky v1: the pure math half — inset resolution against the
// scrollport, the per-axis shift solver, and the paint-item translation
// pass (bracket fold + nested-delta). Extracted from the layout god file
// to pay its shrink-only ratchet. The layout-run halves (span recording,
// read-time shifts) live with the collect walk in mod.rs.
use std::collections::HashMap;

use super::{PaintItem, Rect};
use crate::diting_css::{ComputedStyle, PositionMode};
use crate::diting_dom::tree::NodeId;

/// Resolve a sticky inset (`top`/`bottom`/`left`/`right`) to px against the
/// scrollport dimension it sticks to — per Blink, sticky insets resolve
/// percentages against the SCROLLPORT (the viewport for the root scroller),
/// not the containing block.
pub fn sticky_inset_px(l: &crate::diting_css::Length, port_dim: f32) -> f32 {
    match l {
        crate::diting_css::Length::Px(v) => *v,
        crate::diting_css::Length::Percent(p) => port_dim * p / 100.0,
        crate::diting_css::Length::Calc { percent, px } => port_dim * percent / 100.0 + px,
        // Keyword sizing lengths (auto/min-content/max-content/…) are not
        // valid inset values — the inset parse rejects them, but the enum
        // allows them, so compute as "no inset" rather than panicking.
        _ => 0.0,
    }
}

/// One axis of the sticky constraint math (CSS Position 3, horizontal-tb
/// v1 — root scroller only): how far the box travels to stay visible in the
/// scrollport, given its in-flow position/size, the two stick insets
/// (start = top/left, end = bottom/right, already px), the scrollport's
/// origin/size, and the containing-block span the shift clamps to.
/// End applies first, start overrides on overconstraint (Blink's
/// sticky_constraining_rect order — start wins in LTR horizontal-tb).
#[allow(clippy::too_many_arguments)]
pub fn sticky_axis_shift(
    pos: f32,
    size: f32,
    start: Option<f32>,
    end: Option<f32>,
    port_start: f32,
    port_size: f32,
    cb_start: f32,
    cb_end: f32,
) -> f32 {
    let mut shift = 0.0f32;
    if let Some(end) = end {
        // Keep the box's end edge at least `end` inside the port's end
        // edge — pushes the box back (negative shift) as the port scrolls
        // past; never pulls it forward.
        shift = ((port_start + port_size - end) - (pos + size)).min(0.0);
    }
    if let Some(start) = start {
        // Pin to the port's start edge + inset once the port scrolls past
        // the in-flow spot; start wins over the end inset when both fire.
        shift = ((port_start + start) - pos).max(shift).max(0.0);
    }
    // The box never escapes its containing block: full down-travel stops
    // where the box's end edge meets the CB's end, and up-travel stops at
    // the CB's start (v1 uses the CB border box; spec says margin box).
    shift.max(cb_start - pos).min((cb_end - size) - pos)
}

/// Translate one rect-carrying paint item by (tx, ty) in its own coordinate
/// space. Shadow dx/dy and gradient angles are offsets/directions, not
/// positions — untouched.
fn translate_item(it: &mut PaintItem, tx: f32, ty: f32) {
    fn tr(r: &mut Rect, tx: f32, ty: f32) {
        r.x += tx;
        r.y += ty;
    }
    match it {
        PaintItem::Bg { rect, .. }
        | PaintItem::BgCorner { rect, .. }
        | PaintItem::BoxShadow { rect, .. }
        | PaintItem::BackdropFilter { rect, .. }
        | PaintItem::BgGradient { rect, .. }
        | PaintItem::Replaced { rect, .. }
        | PaintItem::Svg { rect, .. }
        | PaintItem::Clip { rect }
        | PaintItem::ClipRounded { rect, .. }
        | PaintItem::Border { rect, .. } => tr(rect, tx, ty),
        PaintItem::Image { rect, paint_rect, .. } => {
            tr(rect, tx, ty);
            tr(paint_rect, tx, ty);
        }
        PaintItem::Text { x, y, .. } => {
            *x += tx;
            *y += ty;
        }
        PaintItem::SetXf { .. } | PaintItem::SetXfCanvas | PaintItem::ClearXf | PaintItem::PopClip => {}
    }
}

/// One shifted subtree's item span: (source node, start, end) into a
/// paint run's items vec, recorded by the collect walk while both ends
/// are exact — sticky spans open before the node's own first item,
/// scroller spans after the node's clip (its own box stays fixed). See
/// [`apply_sticky_to_items`].
pub type StickySpan = (NodeId, usize, usize);

/// Stacking-context predicates for the cross-parent z walk: positioned
/// with an integer z-index (z:0 included), any transform, opacity<1,
/// backdrop-filter. The PAINT-order view; `collect` adds two walk-internal
/// barriers on top — clipping (a hoisted range would escape its Clip pair)
/// and sticky subtrees (their shift span must stay contiguous).
pub(super) fn establishes_stacking_context(s: &ComputedStyle) -> bool {
    let positioned = matches!(
        s.position,
        Some(PositionMode::Relative)
            | Some(PositionMode::Absolute)
            | Some(PositionMode::Fixed)
            | Some(PositionMode::Sticky)
    );
    (positioned && s.z_index.is_some())
        || s.effective_transform().is_some()
        || s.opacity.is_some_and(|o| o < 1.0)
        || s.backdrop_blur.is_some_and(|b| b > 0.0)
}

/// The collect half's full output: per-element border-box rects, the flat
/// paint-item list in paint order, the boxed-element paint ranking
/// (obscura #738), the event-coordinate local geometry map (blitz #663
/// family), the sticky subtree item spans (root-scroller v1), and the
/// scroller subtree item spans (sticky v2) — same span shape, recorded for
/// every real scroll container at layout time so the read-time shift walk
/// can translate a scrolled subtree without re-laying-out.
pub type LayoutCollect = (
    HashMap<NodeId, Rect>,
    Vec<PaintItem>,
    Vec<NodeId>,
    HashMap<NodeId, (Rect, [f32; 6])>,
    Vec<StickySpan>,
    Vec<StickySpan>,
);

/// Apply sticky shifts to a paint run: for every span whose value is
/// non-zero, translate that span's items by the shift in document space.
/// The input (cached) run is never mutated — callers get a shifted copy
/// only when some span has a shift.
///
/// Spans arrive PRE-RESOLVED as (start, end, value): `value` is the total
/// document-space translation the items inside the span must carry. The
/// caller pairs a layout-recorded span with its value — a sticky span's
/// is the node's read total, a scroller span's is that total minus the
/// scroller's own offset (sticky v2: the scroller's own box stays fixed,
/// so a node that is BOTH carries two different values over its two
/// nested spans — exactly why the value rides the span, not a node-keyed
/// map).
///
/// Nested spans: an ancestor span strictly contains a descendant's (the
/// collect walk records both in document order; a both-node's sticky span
/// opens before its clip, its scroller span after it), and values are
/// CUMULATIVE totals — so a nested span must translate by its DELTA over
/// the nearest enclosing live span; the ancestor's pass already moved
/// these items by its own total. Spans nest or are disjoint by tree
/// construction, so range containment alone identifies the enclosing span
/// — no DOM walk needed here. Liveness is a DELTA question, not a value
/// question: a pinned sticky inside a scrolled container reads total ZERO
/// (its own +shift cancels the scroller's base) yet must still move its
/// items by +shift over that base — dropping zero-valued spans was v1's
/// (root-only) shortcut, where value and delta were always equal. A
/// zero-DELTA span is a pure no-op (its total equals its enclosing base,
/// so skipping it leaves even the stack unchanged) and is skipped.
///
/// Bracket discipline: items inside a SetXf bracket are in local coords,
/// so translating them raw would double-map the shift (M·(p+t) ≠ M·p+t).
/// Instead the translation composes into the bracket matrix (T·M: e/f
/// shift, linear part untouched), moving the whole transformed subtree
/// uniformly. SetXfCanvas content is already canvas (document) space, so
/// its items translate directly — tracked with a small bracket stack of
/// (is_local) flags. A span can only contain brackets from its own
/// subtree: a source under any transformed ancestor is gated to zero
/// shift, so an ancestor's bracket never reaches into a live nested span.
pub fn apply_sticky_to_items(
    items: &[PaintItem],
    spans: &[(usize, usize, [f32; 2])],
) -> Vec<PaintItem> {
    let mut out = items.to_vec();
    let mut live: Vec<(usize, usize, [f32; 2])> = spans.to_vec();
    // Pre-order (start asc, end desc) so enclosing spans precede the spans
    // they contain; the open-span stack then answers "nearest enclosing".
    live.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut open: Vec<(usize, [f32; 2])> = Vec::new(); // (end, total shift)
    for &(start, end, sh) in &live {
        while open.last().is_some_and(|e| e.0 <= start) {
            open.pop();
        }
        let base = open.last().map(|&(_, b)| b).unwrap_or([0.0, 0.0]);
        let d = [sh[0] - base[0], sh[1] - base[1]];
        // Zero DELTA over the nearest enclosing span: a no-op for both the
        // items and the stack (the total equals the base it would push).
        if d[0] == 0.0 && d[1] == 0.0 {
            continue;
        }
        open.push((end, sh));
        let mut brackets: Vec<bool> = Vec::new(); // true = SetXf (local)
        for it in &mut out[start..end] {
            match it {
                PaintItem::SetXf { xf } => {
                    if !brackets.last().is_some_and(|&local| local) {
                        // Compose T·M — the shift is document-space, applied
                        // AFTER the bracket maps local content: e' = e + d0,
                        // f' = f + d1, linear part untouched. (M·T would
                        // rotate/scale the shift by the element's own map.)
                        xf[4] += d[0];
                        xf[5] += d[1];
                    }
                    brackets.push(true);
                }
                PaintItem::SetXfCanvas => brackets.push(false),
                PaintItem::ClearXf => {
                    brackets.pop();
                }
                _ => {
                    if !brackets.last().is_some_and(|&local| local) {
                        translate_item(it, d[0], d[1]);
                    }
                }
            }
        }
    }
    out
}
