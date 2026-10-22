//! Per-line geometry of DOM text nodes, rebuilt from the painted text items.
//!
//! Every text `PaintItem` knows the wrap width and tokens the painter used and
//! (`src`) which DOM text node(s) its characters came from. Re-running the
//! shared greedy breaker over them yields the same lines the painter draws, so
//! `Range.getClientRects()` / `getBoundingClientRect()` and the per-line boxes
//! of a wrapped inline element come out of the exact geometry on screen — no
//! second layout model.

use super::text::{self, FontBook, Token};
use super::{NodeId, PaintItem};
use crate::diting_css::WhiteSpace;

/// One token of a line fragment: node-local char range, x offset from the
/// fragment's left edge, advance, and the text (to measure partial tokens).
#[derive(Clone, Debug)]
struct Span {
    cs: u32,
    ce: u32,
    x: f32,
    w: f32,
    text: String,
}

/// A line of one text node: node-local char range [start, end), the box
/// (page coordinates) and the token spans needed to cut sub-ranges.
#[derive(Clone, Debug)]
pub struct Frag {
    pub start: u32,
    pub end: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    spans: Vec<Span>,
    font_size: f32,
    bold: bool,
    mono: bool,
    han: Option<text::HanSlot>,
}

impl Frag {
    /// x offset (from the fragment's left edge) of node-local char `p`.
    fn x_at(&self, fonts: &FontBook, p: u32) -> f32 {
        let p = p.clamp(self.start, self.end);
        for s in &self.spans {
            if p < s.cs {
                return s.x;
            }
            if p <= s.ce {
                if p == s.ce || s.ce == s.cs {
                    return s.x + s.w;
                }
                let into = (p - s.cs) as usize;
                let prefix: String = s.text.chars().take(into).collect();
                let part = fonts.advance_width(&prefix, self.font_size, self.bold, self.mono, self.han);
                let whole = fonts.advance_width(&s.text, self.font_size, self.bold, self.mono, self.han).max(0.001);
                // Scale the measured prefix onto the laid-out advance (word
                // spacing and rounding ride the token width, not the shaper).
                return s.x + part * (s.w / whole);
            }
        }
        self.spans.last().map(|s| s.x + s.w).unwrap_or(0.0)
    }

    /// The box of node-local chars [from, to) on this line.
    pub fn sub_rect(&self, fonts: &FontBook, from: u32, to: u32) -> [f32; 4] {
        let a = from.max(self.start);
        let b = to.min(self.end).max(a);
        let x0 = self.x_at(fonts, a);
        let x1 = self.x_at(fonts, b);
        [self.x + x0, self.y, (x1 - x0).max(0.0), self.h]
    }

    pub fn rect(&self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }
}

/// Char range in `text` of each token the painter's tokenizer produced:
/// words advance by their length (after the whitespace the tokenizer
/// trimmed), whitespace tokens swallow the collapsed run (or their own
/// length when spaces are preserved), a preserved break takes one char.
fn token_chars(text: &[char], toks: &[Token], ws: WhiteSpace) -> Vec<(u32, u32)> {
    let mut cur = 0usize;
    let mut out = Vec::with_capacity(toks.len());
    for t in toks {
        let n = t.text.chars().count();
        if t.is_break {
            let s = cur;
            if cur < text.len() {
                cur += 1;
            }
            out.push((s as u32, cur as u32));
        } else if t.is_space {
            let s = cur;
            if ws.preserves_spaces() {
                cur = (cur + n).min(text.len());
            } else {
                while cur < text.len() && text[cur].is_whitespace() {
                    cur += 1;
                }
            }
            out.push((s as u32, cur as u32));
        } else {
            while cur < text.len() && text[cur].is_whitespace() && !ws.preserves_spaces() {
                cur += 1;
            }
            let s = cur;
            cur = (cur + n).min(text.len());
            out.push((s as u32, cur as u32));
        }
    }
    out
}

/// The line fragments of DOM text node `target`, in paint order.
pub fn node_frags(items: &[PaintItem], fonts: &FontBook, target: NodeId) -> Vec<Frag> {
    let mut out = Vec::new();
    for item in items {
        let PaintItem::Text {
            text: run_text,
            font_size,
            bold,
            line_height,
            x,
            y,
            wrap_at,
            mono,
            word_spacing,
            tokens,
            ws,
            small_caps,
            han,
            last_line_align,
            src: Some(segs),
            ..
        } = item
        else {
            continue;
        };
        let Some(&(_, seg_start, seg_len)) = segs.iter().find(|(n, _, _)| *n == target) else { continue };
        let toks: std::rc::Rc<[Token]> = match tokens {
            Some(t) => t.clone(),
            None => text::tokens_of(run_text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, *small_caps, *han).into(),
        };
        if toks.is_empty() {
            continue;
        }
        let wrap = if wrap_at.is_finite() && !ws.no_soft_wrap() { Some(*wrap_at) } else { None };
        let lines = text::greedy_wrap(&toks, wrap, *ws);
        let chars: Vec<char> = run_text.chars().collect();
        let ranges = token_chars(&chars, &toks, *ws);
        let (asc, desc) = fonts.metrics(*font_size, *bold).map(|m| (m.ascent, m.descent)).unwrap_or((*font_size * 0.8, *font_size * 0.2));
        let content_h = asc + desc;
        let seg_end = seg_start + seg_len;
        for (li, line) in lines.iter().enumerate() {
            let (Some(&first), Some(&last)) = (line.token_idx.first(), line.token_idx.last()) else { continue };
            let line_x = *x + text::last_line_offset(&lines, li, *last_line_align);
            let line_y = *y + li as f32 * *line_height;
            // x of each token inside the line, in order.
            let mut cursor = 0.0f32;
            let mut spans_run: Vec<(u32, u32, f32, f32, String)> = Vec::new();
            for &ti in &line.token_idx {
                let t = &toks[ti];
                let (cs, ce) = ranges[ti];
                spans_run.push((cs, ce, cursor, t.width, t.text.clone()));
                cursor += t.width;
            }
            let run_cs = ranges[first].0;
            let run_ce = ranges[last].1;
            let lo = run_cs.max(seg_start);
            let hi = run_ce.min(seg_end);
            if hi <= lo {
                continue;
            }
            // Node-local spans clipped to the node's slice of the run.
            let spans: Vec<Span> = spans_run
                .iter()
                .filter(|(cs, ce, ..)| *ce > seg_start && *cs < seg_end)
                .map(|(cs, ce, sx, w, t)| Span { cs: cs.max(&seg_start) - seg_start, ce: ce.min(&seg_end) - seg_start, x: *sx, w: *w, text: t.clone() })
                .collect();
            let mut f = Frag {
                start: lo - seg_start,
                end: hi - seg_start,
                x: line_x,
                y: line_y + (*line_height - content_h) / 2.0,
                w: line.width,
                h: content_h,
                spans,
                font_size: *font_size,
                bold: *bold,
                mono: *mono,
                han: *han,
            };
            // Cut the line to the node's chars when siblings share it.
            if lo > run_cs || hi < run_ce {
                let x0 = f.x_at(fonts, 0).max(f.x_at(fonts, f.start));
                let x1 = f.x_at(fonts, f.end);
                f.x += x0;
                f.w = (x1 - x0).max(0.0);
                for s in &mut f.spans {
                    s.x -= x0;
                }
            }
            out.push(f);
        }
    }
    out
}
