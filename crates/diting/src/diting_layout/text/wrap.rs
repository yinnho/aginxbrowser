//! Tokenization and greedy line wrapping for the text layer — pure
//! functions over `FontBook` advances, split out of `text.rs` (batch 202)
//! to keep that file under the layering audit's god-file ratchet. No
//! raster state, no metrics cache: everything here is callable from both
//! the paint path and the PDF path.

use super::FontBook;

/// One wrap token — a word, a single space, or a per-glyph CJK char — with
/// its real shaped advance. The measure path reads `width`/`is_space`; the
/// paint path (batch 4a) additionally reads `text` to rebuild each line.
#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub width: f32,
    pub is_space: bool,
    /// A preserved newline (white-space pre family): greedy_wrap ends the
    /// current line on it; it never joins a WrapLine itself.
    pub is_break: bool,
    /// Glyph-size ratio for `font-variant-caps: small-caps` synthesis
    /// (1.0 = normal). Small-caps segments carry 0.7 (Blink's ratio) and
    /// already-uppercased text; the paint path multiplies the font size.
    pub scale: f32,
}

/// The small-caps synthesis ratio (Blink's kSmallCapsFontSizeMultiplier).
pub(crate) const SMALL_CAPS_RATIO: f32 = 0.7;

/// Split `text` into case-runs for small-caps synthesis: (run, lowercase?)
/// pairs. Lowercase runs synthesize as uppercase glyphs at 0.7× size;
/// capitals and caseless chars (digits, punctuation, CJK, emoji) keep the
/// full size.
pub(crate) fn caps_case_runs(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut seg = String::new();
    let mut seg_lower = false;
    for c in text.chars() {
        let lower = c.is_lowercase();
        if seg.is_empty() {
            seg_lower = lower;
            seg.push(c);
        } else if lower == seg_lower {
            seg.push(c);
        } else {
            out.push((std::mem::take(&mut seg), seg_lower));
            seg_lower = lower;
            seg.push(c);
        }
    }
    if !seg.is_empty() {
        out.push((seg, seg_lower));
    }
    out
}

/// Append one case-run of a small-caps token: lowercase runs are uppercased
/// and shaped at the reduced size; other runs keep the full size. `ß`
/// expands to "SS" per Unicode case mapping.
fn push_caps_segment(
    seg: &str,
    lower: bool,
    fonts: &FontBook,
    font_size: f32,
    bold: bool,
    mono: bool,
    out: &mut Vec<Token>,
) {
    if seg.is_empty() {
        return;
    }
    if lower {
        let up: String = seg.chars().flat_map(char::to_uppercase).collect();
        let width = fonts.advance_width(&up, font_size * SMALL_CAPS_RATIO, bold, mono);
        out.push(Token {
            is_space: false,
            is_break: false,
            width,
            text: up,
            scale: SMALL_CAPS_RATIO,
        });
    } else {
        let width = fonts.advance_width(seg, font_size, bold, mono);
        out.push(Token {
            is_space: false,
            is_break: false,
            width,
            text: seg.to_string(),
            scale: 1.0,
        });
    }
}

/// Tokenize a run's text under its computed `white-space` mode and shape
/// every token (shared by `measure_text_leaf` and `rasterize_wrapped`).
/// `word_spacing` (px) adds to each rendered space token's advance
/// (CSS Text §7.1) so measure, paint and the shared wrap breaker all see
/// the same widened widths. With `small_caps`, lowercase runs inside a
/// token become separate uppercase tokens at the reduced scale (case-run
/// segmentation: "Hello" → "H"@1.0 + "ELLO"@0.7). Public for the product
/// crate's dual-engine cross-check, which shapes fixture runs through the
/// engine's own path so both sides measure identical tokens.
pub fn tokens_of(
    text: &str,
    font_size: f32,
    bold: bool,
    fonts: &FontBook,
    mono: bool,
    word_spacing: f32,
    ws: crate::diting_css::WhiteSpace,
    small_caps: bool,
) -> Vec<Token> {
    crate::diting_layout::tokenize_ws(text, ws)
        .into_iter()
        .flat_map(|t| {
            let is_break = t == "\n";
            let is_space = !is_break && t.trim().is_empty();
            if is_break || is_space || !small_caps {
                vec![Token {
                    is_space,
                    is_break,
                    width: if is_break {
                        0.0
                    } else {
                        fonts.advance_width(&t, font_size, bold, mono)
                            + if is_space { word_spacing } else { 0.0 }
                    },
                    text: t,
                    scale: 1.0,
                }]
            } else {
                let mut out: Vec<Token> = Vec::new();
                for (seg, lower) in caps_case_runs(&t) {
                    push_caps_segment(&seg, lower, fonts, font_size, bold, mono, &mut out);
                }
                out
            }
        })
        .collect()
}

/// One greedy-wrapped line: which tokens committed to it and the total
/// advance. A space only commits together with the word that follows it;
/// pending spaces at a break point (or at the run's end) are dropped.
/// One wrapped line out of [`greedy_wrap`]: the token slice (indices into
/// the input) and its measured width. Public with `greedy_wrap` — the
/// product crate's cross-check asserts line counts/widths against it.
pub struct WrapLine {
    pub token_idx: Vec<usize>,
    pub width: f32,
}

/// The greedy line breaker — the single wrap truth shared by the measure
/// path (`measure_text_leaf`) and the paint path (`rasterize_wrapped`),
/// locked by the batch-3a probes: break before a token that would overflow
/// `wrap_at`, drop the whitespace before every break. Public for the
/// product crate's dual-engine cross-check (same reason as `tokens_of`).
pub fn greedy_wrap(
    tokens: &[Token],
    wrap_at: Option<f32>,
    ws: crate::diting_css::WhiteSpace,
) -> Vec<WrapLine> {
    if ws.preserves_newlines() {
        return greedy_wrap_preserved(tokens, wrap_at, ws);
    }
    let mut lines = vec![WrapLine { token_idx: Vec::new(), width: 0.0 }];
    let mut pending_space = 0.0f32;
    let mut pending_idx: Vec<usize> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.is_space {
            pending_space += t.width;
            pending_idx.push(i);
            continue;
        }
        let cur = lines.last_mut().expect("always one line");
        if let Some(avail) = wrap_at {
            if cur.width > 0.0 && cur.width + pending_space + t.width > avail {
                lines.push(WrapLine { token_idx: vec![i], width: t.width });
                pending_space = 0.0;
                pending_idx.clear();
                continue;
            }
        }
        cur.width += pending_space + t.width;
        cur.token_idx.extend(pending_idx.drain(..));
        cur.token_idx.push(i);
        pending_space = 0.0;
    }
    lines
}

/// The wrap breaker for the `pre` family (white-space batch): newlines end
/// lines unconditionally (each break token opens the next line, so blank
/// source lines stay blank), preserved spaces belong to their line.
///
/// - `pre` never gets here with a `wrap_at` (no soft wrap opportunities);
/// - `pre-wrap` lets spaces HANG: a space that overflows stays on the line,
///   only a word breaks to the next line;
/// - `break-spaces` never hangs: any token that overflows — space included —
///   starts the next line, so every preserved space can end a line.
fn greedy_wrap_preserved(
    tokens: &[Token],
    wrap_at: Option<f32>,
    ws: crate::diting_css::WhiteSpace,
) -> Vec<WrapLine> {
    let mut lines = vec![WrapLine { token_idx: Vec::new(), width: 0.0 }];
    let mut break_opened_last = false;
    for (i, t) in tokens.iter().enumerate() {
        if t.is_break {
            lines.push(WrapLine { token_idx: Vec::new(), width: 0.0 });
            break_opened_last = true;
            continue;
        }
        break_opened_last = false;
        let cur = lines.last_mut().expect("always one line");
        if let Some(avail) = wrap_at {
            if cur.width > 0.0 && cur.width + t.width > avail {
                if ws == crate::diting_css::WhiteSpace::BreakSpaces {
                    lines.push(WrapLine { token_idx: vec![i], width: t.width });
                    continue;
                }
                // pre-wrap: the overflowing space hangs; a word breaks.
                if !t.is_space {
                    lines.push(WrapLine { token_idx: vec![i], width: t.width });
                    continue;
                }
            }
        }
        cur.width += t.width;
        cur.token_idx.push(i);
    }
    // A trailing newline ends the last line — it doesn't open an empty one
    // (Chrome drops the final newline of a block). At most one: "a\n\n"
    // still keeps its middle blank line.
    if break_opened_last {
        lines.pop();
    }
    lines
}

/// `text-overflow: ellipsis` truncation (nowrap+ellipsis batch): drop whole
/// trailing tokens until the kept run plus the U+2026 marker fits `limit`,
/// then return `(kept, marker)` — the marker is tokenized with the run's own
/// font params so the same fallback chain shapes it. Returns `None` when the
/// run already fits (Chrome only renders an ellipsis for content that
/// actually overflows). Decoration strokes take `kept` only — Chrome leaves
/// the ellipsis itself undecorated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn truncate_tokens(
    tokens: &[Token],
    limit: f32,
    font_size: f32,
    bold: bool,
    fonts: &FontBook,
    mono: bool,
    word_spacing: f32,
    ws: crate::diting_css::WhiteSpace,
) -> Option<(Vec<Token>, Vec<Token>)> {
    let total: f32 = tokens.iter().map(|t| t.width).sum();
    if total <= limit {
        return None;
    }
    let marker = tokens_of("\u{2026}", font_size, bold, fonts, mono, word_spacing, ws, false);
    let marker_w = marker.first().map(|t| t.width).unwrap_or(0.0);
    let mut kept: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut w = 0.0f32;
    for t in tokens {
        if w + t.width + marker_w > limit {
            // Chrome cuts at glyph boundaries: an unbreakable word that
            // doesn't fit whole still fills the remaining space char by
            // char. Per-char widths skip intra-run kerning (v1 posture).
            // Small-caps tokens are already uppercased — shape their chars
            // at the token's own scale, no re-segmentation.
            if !t.is_space {
                let mut acc = String::new();
                let mut acc_w = w;
                for ch in t.text.chars() {
                    let cw = tokens_of(
                        &ch.to_string(),
                        font_size * t.scale,
                        bold,
                        fonts,
                        mono,
                        0.0,
                        ws,
                        false,
                    )
                    .first()
                    .map(|t| t.width)
                    .unwrap_or(0.0);
                    if acc_w + cw + marker_w > limit {
                        break;
                    }
                    acc_w += cw;
                    acc.push(ch);
                }
                if !acc.is_empty() {
                    kept.push(Token {
                        text: acc,
                        width: acc_w - w,
                        is_space: false,
                        is_break: false,
                        scale: t.scale,
                    });
                }
            }
            break;
        }
        w += t.width;
        kept.push(Token {
            text: t.text.clone(),
            width: t.width,
            is_space: t.is_space,
            is_break: t.is_break,
            scale: t.scale,
        });
    }
    // Whitespace left at the cut carries no ink and would only sit between
    // the kept glyphs and the marker.
    while kept.last().is_some_and(|t| t.is_space) {
        kept.pop();
    }
    Some((kept, marker))
}

