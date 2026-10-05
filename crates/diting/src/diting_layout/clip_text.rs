//! `background-clip: text` on flattened INLINE elements (#218).
//!
//! The box-walk capture only fires where a taffy box exists — flattened
//! inline spans have none, so a `<span style="background-clip:text">` around
//! wrapping text used to lose the fill entirely (while the inline-band pass
//! painted the raw gradient OVER the glyphs). The run leaf now carries its
//! DOM text node; at emission this module walks the inline ancestor chain,
//! re-parses the innermost clipping span's gradient, and rebuilds the fill
//! from the run's own wrapped-line geometry.
//!
//! It also owns `box-decoration-break: clone` for BOTH fill sources (inline
//! spans and boxed ancestors): the gradient restarts at every wrapped line.

use std::collections::HashMap;

use crate::diting_css::{parse_linear_gradient, ComputedStyle, Display as CssDisplay, TextAlign, WhiteSpace};
use crate::diting_dom::tree::{DomTree, NodeId};

use super::text::{greedy_wrap, last_line_offset, Token};
use super::{Rect, TextGradient};

/// Innermost INLINE ancestor of `text_node` that declares its own clip:text
/// gradient. The walk stops at the first non-inline ancestor: anything
/// box-shaped (block, inline-block, flex item…) is the box-walk capture's
/// business, and `run_fill` prefers this walk's result so a nested clipping
/// span overrides an outer box fill (Chrome's innermost-element rule).
fn inline_spec(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    text_node: NodeId,
) -> Option<(Vec<(f32, [u8; 4])>, f32, bool)> {
    let mut cur = tree.with_node(text_node, |n| n.parent).flatten()?;
    loop {
        if let Some(s) = styles.get(&cur) {
            if s.display != Some(CssDisplay::Inline) {
                return None;
            }
            if s.background_clip_text {
                if let Some(g) = s.background_image.as_deref().and_then(parse_linear_gradient) {
                    return Some((
                        g.stops.iter().map(|(p, c)| (*p, [c.0, c.1, c.2, c.3])).collect(),
                        g.css_deg,
                        s.box_decoration_clone,
                    ));
                }
            }
        } else {
            return None;
        }
        cur = tree.with_node(cur, |n| n.parent).flatten()?;
    }
}

/// The fill a run leaf paints with, uniting both sources and the
/// box-decoration-break geometry (#218):
///
/// - an inline clipping span (walked from `clip_src`) overrides the
///   box-captured `box_fill`, whose stops already carry the inherited alpha;
/// - `clone` fills get per-line fragment boxes `(x offset, advance)` — each
///   wrapped line samples the gradient over its OWN extent, restarting at 0;
/// - an inline span in `slice` mode samples over the union of its line
///   boxes (the widest line × line count), the approximation of Chrome's
///   fragment-union strip when the span owns no box to read.
///
/// `x/y/wrap_at/line_height` are FINAL-space (the item's paint values);
/// `tokens` are shaped at the leaf's unscaled metrics, so `scale` (a, d)
/// maps line geometry into final space — (1.0, 1.0) unscaled. Line
/// geometry replays the raster's `greedy_wrap`, so the fill tracks exactly
/// where each line will paint.
pub(super) fn run_fill(
    box_fill: Option<TextGradient>,
    clip_src: Option<NodeId>,
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    x: f32,
    y: f32,
    wrap_at: f32,
    line_height: f32,
    tokens: &[Token],
    ws: WhiteSpace,
    last_line_align: Option<TextAlign>,
    alpha: f32,
    scale: (f32, f32),
) -> Option<TextGradient> {
    let inline = clip_src.and_then(|n| inline_spec(tree, styles, n));
    let mut g = match (&inline, box_fill) {
        (Some((stops, deg, clone)), _) => TextGradient {
            area: Rect::default(),
            stops: stops
                .iter()
                .map(|(p, c)| (*p, [c[0], c[1], c[2], (c[3] as f32 * alpha).round() as u8]))
                .collect(),
            css_deg: *deg,
            clone_per_line: *clone,
            clone_lines: Vec::new(),
        },
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let (sa, _sd) = (scale.0.max(0.01), scale.1.max(0.01));
    let wrap = (wrap_at / sa).is_finite().then_some(wrap_at / sa);
    let lines = greedy_wrap(tokens, wrap, ws);
    if g.clone_per_line {
        g.clone_lines = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                (
                    last_line_offset(&lines, i, last_line_align) * sa,
                    l.width.max(1.0) * sa,
                )
            })
            .collect();
    } else if inline.is_some() {
        let widest = lines.iter().map(|l| l.width).fold(0.0f32, f32::max).max(1.0) * sa;
        g.area = Rect {
            x,
            y,
            width: widest,
            height: lines.len().max(1) as f32 * line_height.max(1.0),
        };
    }
    Some(g)
}
