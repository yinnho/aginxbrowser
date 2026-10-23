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
use crate::diting_css::{TextAlign, WhiteSpace};

/// One laid-out line's slot, relative to the leaf's top-left. `y` is the
/// line's stack position — `i * line_height` for a plain stack, further
/// down when an element box's atomic walk dropped a line below the float.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub x: f32,
    pub y: f32,
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
    align: TextAlign,
    line_clamp: Option<u32>,
    atomic: bool,
    ctx: &BlockContext<'_>,
) -> Option<LayoutOutput> {
    if tokens.is_empty() {
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
    // Where the hard lines end vertically — the default one-line-per-
    // line_height stack; the atomic walk below extends it past a float.
    let mut bottom = 0.0f32;
    // nowrap/pre lines cannot break into the band (#313): they break at
    // their hard rules only (nowrap: one line, pre: per newline) and each
    // line starts at its float slot, overflowing to the right — Chrome
    // keeps a nowrap line on the float's line instead of dropping below,
    // and a pre line below the float returns to the edge. An inline
    // ELEMENT box is the exception: too wide for its band it drops below
    // the float (Chrome moves the unbreakable box), and the hard lines
    // after it stay below — bare text overflows in place.
    let mut lines = if ws.no_soft_wrap() || ws.preserves_newlines() {
        let lines = text::greedy_wrap_by(tokens, ws, |_| None);
        if atomic {
            let mut y = 0.0f32;
            for l in &lines {
                let mut s = ctx.find_content_slot(y, Clear::None, None);
                let mut guard = 0;
                while l.width > s.width.min(full).max(0.0) + 0.001 && s.segment_id.is_some() && guard < 64 {
                    s = ctx.find_content_slot(y, Clear::None, s.segment_id);
                    y = y.max(s.y);
                    guard += 1;
                }
                // The walk's fallback slot answers with the QUERY y, not
                // the float bottoms — a line that fit in no band sits
                // below the floats, and only clear reports that y.
                if guard > 0 && s.segment_id.is_none() {
                    y = y.max(ctx.find_content_slot(y, Clear::Both, None).y);
                }
                slots.push(Slot { x: s.x.max(0.0), y, width: s.width.min(full).max(0.0) });
                y += line_height;
            }
            bottom = y;
        } else {
            for i in 0..lines.len() {
                let y = i as f32 * line_height;
                let s = ctx.find_content_slot(y, Clear::None, None);
                slots.push(Slot { x: s.x.max(0.0), y, width: s.width.min(full).max(0.0) });
            }
            bottom = lines.len() as f32 * line_height;
        }
        lines
    } else {
        text::greedy_wrap_by(tokens, ws, |i| {
            while slots.len() <= i {
                let y = slots.len() as f32 * line_height;
                let s = ctx.find_content_slot(y, Clear::None, None);
                slots.push(Slot { x: s.x.max(0.0), y, width: s.width.min(full).max(0.0) });
            }
            Some(slots[i].width)
        })
    };
    if bottom == 0.0 {
        bottom = lines.len() as f32 * line_height;
    }
    if let Some(n) = line_clamp.filter(|n| lines.len() > *n as usize) {
        lines.truncate(n as usize);
        bottom = bottom.min(lines.len() as f32 * line_height);
    }
    slots.truncate(lines.len());
    while slots.len() < lines.len() {
        slots.push(Slot { x: 0.0, y: slots.len() as f32 * line_height, width: full });
    }
    // Band-relative line alignment (#313): a centered/right host on the
    // wrap path has no flex promote to lean on, so each line shifts inside
    // its own slot. The shift is baked into the recorded slot x — split
    // and the painter replay it without knowing the align.
    let shift = |s: &mut Slot, l: f32| match align {
        TextAlign::Center => s.x += (s.width - l).max(0.0) / 2.0,
        TextAlign::Right => s.x += (s.width - l).max(0.0),
        _ => {}
    };
    for (s, l) in slots.iter_mut().zip(lines.iter()) {
        shift(s, l.width);
    }
    let widest = lines.iter().map(|l| l.width).fold(0.0, f32::max);
    let size = taffy::geometry::Size {
        width: inputs.known_dimensions.width.unwrap_or(widest.ceil()),
        height: inputs.known_dimensions.height.unwrap_or(bottom + pad),
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
pub(super) fn split(text: &str, toks: &[Token], ws: WhiteSpace, line_height: f32, slots: &[Slot]) -> Vec<Line> {
    // A slot width is not a wrap opportunity for nowrap/pre — they replay
    // their hard breaks and overflow the band (measure records one slot per
    // hard line for exactly this).
    let soft = !ws.no_soft_wrap() && !ws.preserves_newlines();
    let lines = text::greedy_wrap_by(toks, ws, |i| {
        soft.then(|| slots.get(i).map(|s| s.width)).flatten()
    });
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
                slot: slots.get(i).copied().unwrap_or(Slot { x: 0.0, y: i as f32 * line_height, width: l.width }),
                from,
                to,
            })
        })
        .collect()
}

/// Measure one layout leaf: text runs through the shared breaker (beside a
/// float, into the float's free slots), everything else by its style.
pub(super) fn measure_node(
    fonts: &super::FontBook,
    inputs: taffy::tree::LayoutInput,
    id: taffy::tree::NodeId,
    ctx: Option<&mut super::TextLeaf>,
    style: &taffy::Style,
    block: Option<&mut taffy::BlockContext<'_>>,
) -> taffy::tree::LayoutOutput {
    match ctx {
        Some(super::TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, align, tokens, small_caps, han, line_clamp, atomic, .. }) => {
            let pad = super::shift_pad(*baseline_shift, super::leaf_descent(fonts, *font_size, *bold));
            let shaped = super::run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps, *han);
            if let Some(b) = block.as_deref() {
                if let Some(out) = measure(id, &shaped, *line_height, pad, &inputs, *ws, *align, *line_clamp, *atomic, b) {
                    return out;
                }
            }
            super::measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws, *line_clamp)
        }
        // Word leaves keep their style-driven sizing.
        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| taffy::geometry::Size::ZERO),
    }
}
