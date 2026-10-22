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
    Line, NodeId, Point, RequestedAxis, Size, SizingMode,
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
pub(super) fn layout<T: LayoutPartialTree>(
    tree: &mut T,
    id: NodeId,
    inputs: LayoutInput,
    ctx: &BlockContext<'_>,
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

    // Place items line by line.
    let mut placed: Vec<(usize, f32, f32)> = Vec::with_capacity(items.len());
    let (mut y, mut line_h, mut cursor) = (0.0f32, 0.0f32, 0.0f32);
    let mut slot = ctx.find_content_slot(y, Clear::None, None);
    let mut on_line = false;
    let mut max_right = 0.0f32;
    for (i, it) in items.iter().enumerate() {
        let outer_w = it.size.width + it.margin.left + it.margin.right;
        let outer_h = it.size.height + it.margin.top + it.margin.bottom;
        let fits = cursor + outer_w <= slot.width.min(width) + 0.001;
        if on_line && !fits {
            y += line_h;
            line_h = 0.0;
            cursor = 0.0;
            on_line = false;
            slot = ctx.find_content_slot(y, Clear::None, None);
        }
        if !on_line {
            // A first item wider than the free room drops to the next slot
            // that is wide enough (past the float), like a line box would.
            let mut guard = 0;
            while outer_w > slot.width.min(width) + 0.001 && slot.segment_id.is_some() && guard < 64 {
                slot = ctx.find_content_slot(y, Clear::None, slot.segment_id);
                y = y.max(slot.y);
                guard += 1;
            }
        }
        placed.push((i, slot.x.max(0.0) + cursor + it.margin.left, y + it.margin.top));
        cursor += outer_w;
        line_h = line_h.max(outer_h);
        on_line = true;
        max_right = max_right.max(slot.x.max(0.0) + cursor);
    }
    let total_h = if on_line { y + line_h } else { 0.0 };
    let size = Size { width, height: inputs.known_dimensions.height.unwrap_or(total_h) };

    if inputs.run_mode == taffy::RunMode::PerformLayout {
        for (i, x, py) in placed {
            let it = &items[i];
            let out = tree.compute_child_layout(
                it.id,
                sized(width, Size { width: Some(it.size.width), height: Some(it.size.height) }, taffy::RunMode::PerformLayout),
            );
            let mut l = Layout::with_order(i as u32);
            l.location = Point { x, y: py };
            l.size = it.size;
            l.scrollable_overflow_rect = out.scrollable_overflow_rect;
            l.margin = it.margin;
            l.border = it.border;
            l.padding = it.padding;
            tree.set_unrounded_layout(it.id, &l);
        }
    }
    Some(LayoutOutput::from_outer_size(size))
}
