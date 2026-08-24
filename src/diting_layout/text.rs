//! Real glyph measurement (render claim batch 3a).
//!
//! The word-leaf model (batch 2b) measured text with a deterministic
//! approximation (0.55em per ASCII char, 1.0em per CJK char, bold ×1.08).
//! This module replaces that with real advances read from actual font bytes
//! through `swash` — the same stack upstream obscura-render builds its text
//! engine on (cosmic-text fork + swash 0.2) — so text-derived rects become a
//! function of the fixture glyphs, not a guess.
//!
//! Both sides of the blitz cross-check load the SAME fixture bytes: ours
//! through this module, blitz's through `DocumentConfig.font_ctx` with
//! `system_fonts: false` and the fixture registered under a pinned family
//! name. Advances therefore differ only by shaping: parley 0.10 shapes with
//! harfrust while we shape with swash — identical by construction for CJK
//! (no kerning, one glyph per char, advance = full-width) and within
//! tolerance for the kerned Latin our fixtures use.
//!
//! Regenerate the fixtures with `scripts/make_font_fixture.py`.
//!
//! Batch 3b adds the paint half: [`FontBook::rasterize`] turns a run into an
//! RGBA tile (swash outline raster) with the baseline placed per parley 0.10's
//! quantized Chrome-style metrics — see [`baseline_offset`].
//!
//! Batch 4a adds the wrapped painter: [`FontBook::rasterize_wrapped`] reuses
//! the SAME greedy line breaker as the measure path ([`greedy_wrap`]) and
//! composes the lines into one tile whose box is exactly the box the measure
//! function reported — measure and paint share one wrap truth.

use std::cell::RefCell;

use swash::proxy::MetricsProxy;
use swash::scale::{Render, ScaleContext, Source};
use swash::shape::ShapeContext;
use swash::{FontRef, GlyphId};

/// A regular/bold face pair loaded from raw TTF/OTF bytes.
///
/// Deliberately minimal: one family, two weights, index-0 face. Weight
/// matching beyond the pair (500, 800, …) snaps to the nearer face, which is
/// also all the cross-check fixtures exercise.
pub struct FontBook {
    regular: Vec<u8>,
    bold: Vec<u8>,
}

thread_local! {
    /// Reused across calls as swash's docs recommend; shaping state is not
    /// thread-safe so it lives in a thread-local.
    static SHAPE_CTX: RefCell<ShapeContext> = RefCell::new(ShapeContext::new());
    /// Same for the scaler (glyph outline raster state, batch 3b).
    static SCALE_CTX: RefCell<ScaleContext> = RefCell::new(ScaleContext::new());
}

impl FontBook {
    /// Load a book from TTF/OTF bytes. Returns `None` if either face fails
    /// to parse (truncated file, wrong magic, …).
    pub fn from_pairs(regular: Vec<u8>, bold: Vec<u8>) -> Option<Self> {
        if FontRef::from_index(&regular, 0).is_none() || FontRef::from_index(&bold, 0).is_none() {
            return None;
        }
        Some(Self { regular, bold })
    }

    fn face(&self, bold: bool) -> &Vec<u8> {
        if bold { &self.bold } else { &self.regular }
    }

    /// Shaped advance of `text` at `font_size`, in px. Kerning (GPOS) and
    /// ligatures apply; CJK comes out at one full-width advance per glyph.
    pub fn advance_width(&self, text: &str, font_size: f32, bold: bool) -> f32 {
        let bytes = self.face(bold);
        let Some(font) = FontRef::from_index(bytes, 0) else {
            return 0.0;
        };
        SHAPE_CTX.with_borrow_mut(|ctx| {
            let mut shaper = ctx.builder(font).size(font_size).build();
            shaper.add_str(text);
            let mut width = 0.0f32;
            shaper.shape_with(|cluster| width += cluster.advance());
            width
        })
    }

    /// Vertical metrics of the face, normalized to px at `font_size` — for
    /// the paint batch (baseline placement, 3b). Layout line height does NOT
    /// use these: blitz pins CSS `normal` to `font_size * 1.2`
    /// (blitz-dom/src/layout/mod.rs:76), and we match that.
    ///
    /// `descent` is the positive distance BELOW the baseline (swash reports
    /// the descender magnitude; we normalize with `abs` so the sign
    /// convention can't leak).
    pub fn metrics(&self, font_size: f32, bold: bool) -> Option<ScaledMetrics> {
        let bytes = self.face(bold);
        let font = FontRef::from_index(bytes, 0)?;
        let m = MetricsProxy::from_font(&font).materialize_metrics(&font, &[]);
        let scale = font_size / m.units_per_em as f32;
        Some(ScaledMetrics {
            ascent: m.ascent * scale,
            descent: m.descent.abs() * scale,
            line_gap: m.leading.abs() * scale,
        })
    }

    /// Rasterize one line of `text` into an RGBA tile (batch 3b): shape →
    /// per-glyph pen positions → swash outline raster (alpha) → composite
    /// with `color` (straight alpha, `max` blend so overlapping glyph
    /// coverage never double-darkens).
    ///
    /// The baseline sits per [`baseline_offset`] inside the tile; pens are
    /// rounded to whole pixels (no subpixel placement — ink-extent
    /// cross-checks against blitz stay within tolerance because both
    /// rasterizers cover the same outlines to within ~a pixel).
    pub fn rasterize(&self, text: &str, font_size: f32, bold: bool, color: [u8; 4], line_height: f32) -> TextRaster {
        let m = self.metrics(font_size, bold).unwrap_or(ScaledMetrics {
            ascent: font_size,
            descent: font_size * 0.2,
            line_gap: 0.0,
        });
        let baseline = baseline_offset(m.ascent, m.descent, line_height);

        // Tile bounds: one px slack around the font's natural extent. With
        // the fixture's negative leading the baseline-anchored extent starts
        // above the line box, so anchor on baseline ± metrics, not the box.
        let top = (baseline - m.ascent).floor() - 1.0;
        let bottom = (baseline + m.descent).ceil() + 1.0;
        let height = (bottom - top).max(1.0) as usize;
        let width = self.advance_width(text, font_size, bold).ceil() as usize + 2;

        let mut alpha = vec![0u8; width * height];
        self.blit_line(&mut alpha, width, height, text, font_size, bold, 0.0, baseline - top);
        let data = colorize(&alpha, color);
        TextRaster { width, height, baseline: baseline - top, top, data }
    }

    /// A wrapped, multi-line raster of a run (batch 4a) — the paint
    /// counterpart of `measure_text_leaf`: the SAME [`greedy_wrap`] decides
    /// the lines, each baseline sits at `round(i × lh) + baseline_offset`,
    /// and the box height is `lines × lh` — the exact box the measure
    /// function reported, so a compositor places this tile at the leaf's
    /// layout origin and the geometry lines up with the layout tree.
    pub fn rasterize_wrapped(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        wrap_at: f32,
        line_height: f32,
    ) -> TextRaster {
        let empty = || TextRaster {
            width: 0,
            height: 0,
            baseline: 0.0,
            top: 0.0,
            data: Vec::new(),
        };
        if text.trim().is_empty() {
            return empty();
        }
        let tokens = tokens_of(text, font_size, bold, self);
        let lines = greedy_wrap(&tokens, Some(wrap_at.max(0.0)));
        if lines.iter().all(|l| l.width <= 0.0) {
            return empty();
        }

        let m = self.metrics(font_size, bold).unwrap_or(ScaledMetrics {
            ascent: font_size,
            descent: font_size * 0.2,
            line_gap: 0.0,
        });
        let b0 = baseline_offset(m.ascent, m.descent, line_height);
        let baselines: Vec<f32> = (0..lines.len() as u32)
            .map(|i| (i as f32 * line_height).round() + b0)
            .collect();
        let top = (baselines[0] - m.ascent).floor() - 1.0;
        let bottom = (baselines[baselines.len() - 1] + m.descent).ceil() + 1.0;
        let height = (bottom - top).max(1.0) as usize;
        let width = lines.iter().map(|l| l.width).fold(0.0, f32::max).ceil() as usize + 2;

        let mut alpha = vec![0u8; width * height];
        for (line, baseline) in lines.iter().zip(&baselines) {
            if line.token_idx.is_empty() {
                continue;
            }
            let s: String =
                line.token_idx.iter().map(|&i| tokens[i].text.as_str()).collect();
            self.blit_line(&mut alpha, width, height, &s, font_size, bold, 0.0, baseline - top);
        }
        let data = colorize(&alpha, color);
        TextRaster { width, height, baseline: baselines[0] - top, top, data }
    }

    /// Shape `text` and blit its glyphs into an A8 `alpha` buffer (max
    /// blend) at pen origin `x0` with the baseline `baseline` rows from the
    /// tile top — the shared raster core behind [`Self::rasterize`] and
    /// [`Self::rasterize_wrapped`].
    fn blit_line(
        &self,
        alpha: &mut [u8],
        width: usize,
        height: usize,
        text: &str,
        font_size: f32,
        bold: bool,
        x0: f32,
        baseline: f32,
    ) {
        let bytes = self.face(bold);
        let Some(font) = FontRef::from_index(bytes, 0) else { return };

        // Shape once: absolute x per glyph + y offset from the baseline.
        let mut glyphs: Vec<(f32, f32, GlyphId)> = Vec::new();
        SHAPE_CTX.with_borrow_mut(|ctx| {
            let mut shaper = ctx.builder(font).size(font_size).build();
            shaper.add_str(text);
            let mut pen = 0.0f32;
            shaper.shape_with(|cluster| {
                for g in cluster.glyphs {
                    glyphs.push((x0 + pen + g.x, g.y, g.id));
                    pen += g.advance;
                }
            });
        });

        SCALE_CTX.with_borrow_mut(|sctx| {
            let mut scaler = sctx.builder(font).size(font_size).build();
            let render = Render::new(&[Source::Outline]);
            for (pen_x, dy, gid) in glyphs {
                // swash rasterizes outlines with zeno Origin::BottomLeft, so
                // `placement.top` is the image's top edge ABOVE the pen:
                // blit y = pen_y - top (data rows are ordinary top-down).
                let Some(img) = render.render(&mut scaler, gid) else { continue };
                let ox = pen_x.round() as i64 + img.placement.left as i64;
                let oy = (baseline + dy).round() as i64 - img.placement.top as i64;
                for gy in 0..img.placement.height as i64 {
                    let Some(ty) = (oy + gy).checked_sub(0).and_then(|v| usize::try_from(v).ok())
                    else { continue };
                    if ty >= height {
                        continue;
                    }
                    for gx in 0..img.placement.width as i64 {
                        let Some(tx) = usize::try_from(ox + gx).ok() else { continue };
                        if tx >= width {
                            continue;
                        }
                        let cov = img.data[(gy * img.placement.width as i64 + gx) as usize];
                        let slot = &mut alpha[ty * width + tx];
                        *slot = (*slot).max(cov);
                    }
                }
            }
        });
    }
}

/// Colorize an A8 coverage buffer into straight-alpha RGBA8.
fn colorize(alpha: &[u8], color: [u8; 4]) -> Vec<u8> {
    let mut data = vec![0u8; alpha.len() * 4];
    for (i, a) in alpha.iter().enumerate() {
        if *a == 0 {
            continue;
        }
        data[i * 4..i * 4 + 4].copy_from_slice(&[color[0], color[1], color[2], *a]);
    }
    data
}

/// One wrap token — a word, a single space, or a per-glyph CJK char — with
/// its real shaped advance. The measure path reads `width`/`is_space`; the
/// paint path (batch 4a) additionally reads `text` to rebuild each line.
pub(crate) struct Token {
    pub text: String,
    pub width: f32,
    pub is_space: bool,
}

/// Tokenize a run's trimmed text and shape every token (shared by
/// `measure_text_leaf` and `rasterize_wrapped`).
pub(crate) fn tokens_of(text: &str, font_size: f32, bold: bool, fonts: &FontBook) -> Vec<Token> {
    super::tokenize(text.trim())
        .into_iter()
        .map(|t| Token {
            is_space: t.trim().is_empty(),
            width: fonts.advance_width(&t, font_size, bold),
            text: t,
        })
        .collect()
}

/// One greedy-wrapped line: which tokens committed to it and the total
/// advance. A space only commits together with the word that follows it;
/// pending spaces at a break point (or at the run's end) are dropped.
pub(crate) struct WrapLine {
    pub token_idx: Vec<usize>,
    pub width: f32,
}

/// The greedy line breaker — the single wrap truth shared by the measure
/// path (`measure_text_leaf`) and the paint path (`rasterize_wrapped`),
/// locked by the batch-3a probes: break before a token that would overflow
/// `wrap_at`, drop the whitespace before every break.
pub(crate) fn greedy_wrap(tokens: &[Token], wrap_at: Option<f32>) -> Vec<WrapLine> {
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

/// Font vertical metrics scaled to a given size, all in px. `ascent` is the
/// distance above the baseline; `descent` the POSITIVE distance below it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaledMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

/// Baseline offset below a line box's top edge, reproducing parley 0.10's
/// Chrome-style quantized metrics — the exact path blitz exercises
/// (parley src/layout/line_break.rs:1103-1129 with `quantize = true`):
///
/// - ascent and descent are rounded separately, THEN leading is derived:
///   `leading = line_height - (round(ascent) + round(descent))`;
/// - the leading is split with `above = floor(leading / 2)`, below gets the
///   rest (Chrome gives 'below' the larger half);
/// - `baseline = round(line_y) + round(ascent) + above`.
///
/// For the Noto Sans SC fixture (ascender 1.16em, descender 0.288em) the
/// natural extent (1.448em) EXCEEDS blitz's pinned `normal` line box
/// (1.2em): leading is negative, and the baseline lands at ~1.0em below the
/// line top (exactly fs at 12/16/20/24px) with glyph ink overflowing the
/// box top — the familiar cramped-CJK look, reproduced bit-for-bit.
pub fn baseline_offset(ascent: f32, descent: f32, line_height: f32) -> f32 {
    let a = ascent.round();
    let d = descent.round();
    let leading = line_height - (a + d);
    let above = (leading * 0.5).floor();
    a + above
}

/// A raster of a text run (batch 3b): our minimal paint output.
/// Straight-alpha RGBA8, row-major. Single-line runs keep the baseline at
/// [`baseline_offset`]; wrapped runs (batch 4a) place line i's baseline at
/// `round(i × lh) + baseline_offset`. `top` is tile row 0's y in LINE-BOX
/// coordinates (≤ 0 when ink overflows the cramped CJK box top): a
/// compositor blits at `(box_x, box_y + top)`.
#[derive(Debug)]
pub struct TextRaster {
    pub width: usize,
    pub height: usize,
    /// Distance from the tile's top edge to the FIRST line's baseline, px.
    pub baseline: f32,
    /// Tile row 0 relative to the line box top (usually ≤ 0), px.
    pub top: f32,
    /// RGBA8, row-major, straight alpha.
    pub data: Vec<u8>,
}

impl TextRaster {
    /// Bounding box of ink at ≥50% coverage: `(x0, y0, x1, y1)` pixel
    /// indices, inclusive; `None` if the tile is empty.
    pub fn ink_bbox(&self) -> Option<(usize, usize, usize, usize)> {
        let mut bbox: Option<(usize, usize, usize, usize)> = None;
        for y in 0..self.height {
            for x in 0..self.width {
                let a = self.data[(y * self.width + x) * 4 + 3];
                if a < 128 {
                    continue;
                }
                bbox = Some(match bbox {
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    None => (x, y, x, y),
                });
            }
        }
        bbox
    }
}
