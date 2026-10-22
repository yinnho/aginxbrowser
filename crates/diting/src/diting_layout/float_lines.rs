//! Text lines beside floats (#300).
//!
//! A paragraph next to a float keeps its box full-width; only the line boxes
//! that touch the float are shorter and start further in. taffy's float
//! engine knows where the floats are, and hands a text leaf the block
//! formatting context it sits in; this asks it for the free slot of each
//! line, wraps into those slots, and remembers each line's (x, width) so the
//! painter and the range geometry draw the same lines.

use std::cell::RefCell;
use std::collections::HashMap;

use taffy::tree::{LayoutInput, LayoutOutput, NodeId};
use taffy::{BlockContext, Clear};

use super::text::{self, Token};
use crate::diting_css::WhiteSpace;

/// One laid-out line's horizontal slot, relative to the leaf's left edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub x: f32,
    pub width: f32,
}

thread_local! {
    static SLOTS: RefCell<HashMap<NodeId, Vec<Slot>>> = RefCell::new(HashMap::new());
}

pub(super) fn reset() {
    SLOTS.with(|s| s.borrow_mut().clear());
}

/// The per-line slots recorded for a leaf by the last full layout pass; `None`
/// when no float touched it (uniform lines).
pub(super) fn slots_of(id: NodeId) -> Option<Vec<Slot>> {
    SLOTS.with(|s| s.borrow().get(&id).cloned())
}

/// Measure a text leaf whose lines may sit beside floats. `None` when the
/// leaf is not in a context that has floats near it (the caller measures as
/// before).
#[allow(clippy::too_many_arguments)]
pub(super) fn measure(
    id: NodeId,
    tokens: &[Token],
    line_height: f32,
    pad: f32,
    inputs: &LayoutInput,
    ws: WhiteSpace,
    line_clamp: Option<u32>,
    ctx: &BlockContext<'_>,
) -> Option<LayoutOutput> {
    if tokens.is_empty() || ws.no_soft_wrap() || ws.preserves_newlines() {
        return None;
    }
    let full = match (inputs.known_dimensions.width, inputs.available_space.width) {
        (Some(w), _) => w,
        (None, taffy::AvailableSpace::Definite(w)) => w,
        _ => return None,
    };
    if !ctx.has_active_floats(0.0) {
        return None;
    }
    let mut slots: Vec<Slot> = Vec::new();
    let mut lines = text::greedy_wrap_by(tokens, ws, |i| {
        while slots.len() <= i {
            let y = slots.len() as f32 * line_height;
            let s = ctx.find_content_slot(y, Clear::None, None);
            slots.push(Slot { x: s.x.max(0.0), width: s.width.min(full).max(0.0) });
        }
        Some(slots[i].width)
    });
    if let Some(n) = line_clamp.filter(|n| lines.len() > *n as usize) {
        lines.truncate(n as usize);
    }
    slots.truncate(lines.len());
    while slots.len() < lines.len() {
        slots.push(Slot { x: 0.0, width: full });
    }
    let widest = lines.iter().map(|l| l.width).fold(0.0, f32::max);
    let size = taffy::geometry::Size {
        width: inputs.known_dimensions.width.unwrap_or(widest.ceil()),
        height: inputs.known_dimensions.height.unwrap_or(lines.len() as f32 * line_height + pad),
    };
    if inputs.run_mode == taffy::RunMode::PerformLayout {
        SLOTS.with(|s| s.borrow_mut().insert(id, slots));
    }
    Some(LayoutOutput::from_sizes(
        size,
        taffy::geometry::Rect { left: 0.0, top: 0.0, right: size.width, bottom: size.height },
    ))
}

/// One painted line of a run that sits beside a float.
pub(super) struct Line {
    /// The run's raw text for this line (the painter re-tokenizes it).
    pub text: String,
    pub tokens: std::rc::Rc<[Token]>,
    pub slot: Slot,
    /// Char range of the line in the run's text, for the range geometry.
    pub from: u32,
    pub to: u32,
}

/// Cut a run into the lines `measure` laid out, using the slots it recorded.
pub(super) fn split(text: &str, toks: &[Token], ws: WhiteSpace, slots: &[Slot]) -> Vec<Line> {
    let lines = text::greedy_wrap_by(toks, ws, |i| slots.get(i).map(|s| s.width));
    let chars: Vec<char> = text.chars().collect();
    let ranges = super::text_geom::token_chars(&chars, toks, ws);
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let (first, last) = (*l.token_idx.first()?, *l.token_idx.last()?);
            let (from, to) = (ranges[first].0, ranges[last].1);
            Some(Line {
                text: chars[from as usize..to as usize].iter().collect(),
                tokens: l.token_idx.iter().map(|&t| toks[t].clone()).collect::<Vec<_>>().into(),
                slot: slots.get(i).copied().unwrap_or(Slot { x: 0.0, width: l.width }),
                from,
                to,
            })
        })
        .collect()
}
