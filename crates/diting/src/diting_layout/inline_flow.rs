//! Inline content beside floats (#300).
//!
//! A paragraph with inline elements is a wrapping flex row of word leaves and
//! atoms. A stock flex row is an independent box: it cannot start its lines
//! after a float and run under it further down. This lays the same items out
//! line by line into the free slots taffy's float engine reports for the
//! wrapper's position, with the flex row's own rules (items top-aligned, the
//! line as tall as its tallest item, no growing).

use taffy::util::ResolveOrZero;
use taffy::{
    AvailableSpace, BlockContext, BoxGenerationMode, Clear, CoreStyle, Layout, LayoutInput, LayoutOutput, LayoutPartialTree,
    JustifyContent, Line, NodeId, Point, RequestedAxis, Size, SizingMode,
};

struct Item {
    id: NodeId,
    size: Size<f32>,
    margin: taffy::Rect<f32>,
    border: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
}

fn sized(width: f32, known: Size<Option<f32>>, mode: taffy::RunMode) -> LayoutInput {
    LayoutInput {
        run_mode: mode,
        sizing_mode: SizingMode::InherentSize,
        axis: RequestedAxis::Both,
        known_dimensions: known,
        known_dimensions_are_definite: Size { width: true, height: true },
        parent_size: Size { width: Some(width), height: None },
        available_space: Size { width: AvailableSpace::MaxContent, height: AvailableSpace::MaxContent },
        vertical_margins_are_collapsible: Line::FALSE,
    }
}

/// Lay the wrapper's children out into the float slots. `None` when the
/// wrapper has no definite width to wrap against (the caller runs flex).
/// `justify` carries the host's center/right text-align (#313): beside a
/// float the wrapper cannot lean on the host's flex promote, so each line
/// shifts inside its own slot here.
pub(super) fn layout<T: LayoutPartialTree>(
    tree: &mut T,
    id: NodeId,
    inputs: LayoutInput,
    ctx: &BlockContext<'_>,
    justify: JustifyContent,
) -> Option<LayoutOutput> {
    let width = match (inputs.known_dimensions.width, inputs.available_space.width) {
        (Some(w), _) => w,
        (None, AvailableSpace::Definite(w)) => w,
        _ => return None,
    };
    let kids: Vec<NodeId> = tree.child_ids(id).collect();
    let mut items: Vec<Item> = Vec::with_capacity(kids.len());
    for k in kids {
        let (display, margin, border, padding) = {
            let st = tree.get_core_container_style(k);
            (
                st.box_generation_mode() == BoxGenerationMode::None,
                st.margin().resolve_or_zero(Some(width), |_, _| 0.0),
                st.border().resolve_or_zero(Some(width), |_, _| 0.0),
                st.padding().resolve_or_zero(Some(width), |_, _| 0.0),
            )
        };
        if display {
            let _ = tree.compute_child_layout(k, LayoutInput::HIDDEN);
            tree.set_unrounded_layout(k, &Layout::with_order(0));
            continue;
        }
        let size = tree.compute_child_layout(k, sized(width, Size::NONE, taffy::RunMode::ComputeSize)).size;
        items.push(Item { id: k, size, margin, border, padding });
    }

    // Place items line by line, remembering each line's band (x0, room)
    // so it can shift inside it afterwards.
    struct Line {
        x0: f32,
        room: f32,
        y: f32,
        h: f32,
        w: f32,
        items: Vec<(usize, f32, f32)>,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Option<Line> = None;
    let mut y = 0.0f32;
    let mut slot = ctx.find_content_slot(y, Clear::None, None);
    for (i, it) in items.iter().enumerate() {
        let outer_w = it.size.width + it.margin.left + it.margin.right;
        let outer_h = it.size.height + it.margin.top + it.margin.bottom;
        let fits = cur.as_ref().is_none_or(|l| l.w + outer_w <= slot.width.min(width) + 0.001);
        if let Some(l) = cur.take() {
            if fits {
                cur = Some(l);
            } else {
                y = l.y + l.h;
                lines.push(l);
                slot = ctx.find_content_slot(y, Clear::None, None);
            }
        }
        if cur.is_none() {
            // A first item wider than the free room drops to the next slot
            // that is wide enough (past the float), like a line box would.
            let mut guard = 0;
            while outer_w > slot.width.min(width) + 0.001 && slot.segment_id.is_some() && guard < 64 {
                slot = ctx.find_content_slot(y, Clear::None, slot.segment_id);
                y = y.max(slot.y);
                guard += 1;
            }
            // The walk's fallback slot answers with the QUERY y, not the
            // float bottoms — an item that fit in no band sits below the
            // floats, and only clear reports that y (the float_lines
            // atomic-walk fix, #313).
            if guard > 0 && slot.segment_id.is_none() {
                y = y.max(ctx.find_content_slot(y, Clear::Both, None).y);
            }
            let x0 = slot.x.max(0.0).min(width);
            cur = Some(Line { x0, room: (width - x0).min(slot.width).max(0.0), y, h: 0.0, w: 0.0, items: Vec::new() });
        }
        let l = cur.as_mut().expect("a line opens with its first item");
        l.items.push((i, l.w + it.margin.left, y + it.margin.top));
        l.w += outer_w;
        l.h = l.h.max(outer_h);
    }
    if let Some(l) = cur.take() {
        y = l.y + l.h;
        lines.push(l);
    }
    let total_h = y;
    let size = Size { width, height: inputs.known_dimensions.height.unwrap_or(total_h) };

    if inputs.run_mode == taffy::RunMode::PerformLayout {
        for l in &lines {
            // A line wider than its band (unbreakable items) keeps the
            // band start — Chrome overflows rather than un-centering.
            // Trailing spaces hang (the space leaves are the zero-height
            // items): the shift measures the line's box, which Chrome
            // takes without them.
            let line_w = l
                .items
                .iter()
                .rev()
                .find(|(i, _, _)| items[*i].size.height > 0.0)
                .map(|(i, cx, _)| cx + items[*i].size.width + items[*i].margin.right)
                .unwrap_or(l.w);
            let shift = match justify {
                JustifyContent::CENTER => (l.room - line_w).max(0.0) / 2.0,
                JustifyContent::FLEX_END => (l.room - line_w).max(0.0),
                _ => 0.0,
            };
            for (i, cx, py) in &l.items {
                let it = &items[*i];
                let out = tree.compute_child_layout(
                    it.id,
                    sized(width, Size { width: Some(it.size.width), height: Some(it.size.height) }, taffy::RunMode::PerformLayout),
                );
                let mut lo = Layout::with_order(*i as u32);
                lo.location = Point { x: l.x0 + shift + cx, y: *py };
                lo.size = it.size;
                lo.scrollable_overflow_rect = out.scrollable_overflow_rect;
                lo.margin = it.margin;
                lo.border = it.border;
                lo.padding = it.padding;
                tree.set_unrounded_layout(it.id, &lo);
            }
        }
    }
    Some(LayoutOutput::from_outer_size(size))
}
