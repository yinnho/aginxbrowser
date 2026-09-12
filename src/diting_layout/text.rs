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
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use swash::proxy::MetricsProxy;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::{FontRef, GlyphId};

/// Which face a text segment shapes in: the primary bundle pair (weight
/// selected by `bold`) or one of the fallback faces — single-weight tails
/// like the emoji font, where bold emoji is the same face (as in browsers).
#[derive(Clone, Copy, PartialEq)]
enum FaceSel {
    Primary,
    Fallback(usize),
}

/// A regular/bold face pair loaded from raw TTF/OTF bytes.
///
/// Deliberately minimal: one family, two weights, index-0 face. Weight
/// matching beyond the pair (500, 800, …) snaps to the nearer face, which is
/// also all the cross-check fixtures exercise. Fallback faces (emoji batch)
/// carry codepoints the pair lacks — see [`FaceSel`].
pub struct FontBook {
    regular: Vec<u8>,
    bold: Vec<u8>,
    fallbacks: Vec<Vec<u8>>,
    /// Content hash of all face bytes — the raster cache's book identity.
    /// Rasters are pure functions of (faces, text, size, bold, color, wrap,
    /// line height), so two books with identical bytes may share cache
    /// entries and books with different bytes must not; the hash is computed
    /// once at construction, which is once per process for the production
    /// book ([`crate::diting_fonts::font_book`] caches its instance).
    fingerprint: u64,
}

/// Hash a book's whole face set into the cache identity (see
/// [`FontBook::fingerprint`]).
fn face_fingerprint(regular: &[u8], bold: &[u8], fallbacks: &[Vec<u8>]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    regular.hash(&mut h);
    bold.hash(&mut h);
    for f in fallbacks {
        f.hash(&mut h);
    }
    h.finish()
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
        let fingerprint = face_fingerprint(&regular, &bold, &[]);
        Some(Self { regular, bold, fallbacks: Vec::new(), fingerprint })
    }

    /// Append single-weight fallback faces (emoji batch): unparseable bytes
    /// drop silently, parseable ones join the tail in order. A char the
    /// primary pair doesn't map resolves through the first fallback that
    /// covers it; measure and paint segment identically, so a mixed
    /// "text 🚀 text" run advances the same bytes it rasterizes.
    pub fn with_fallbacks(mut self, faces: Vec<Vec<u8>>) -> Self {
        self.fallbacks
            .extend(faces.into_iter().filter(|b| FontRef::from_index(b, 0).is_some()));
        self.fingerprint = face_fingerprint(&self.regular, &self.bold, &self.fallbacks);
        self
    }

    /// Whether any fallback face is loaded (the rasterizers' color-layer
    /// allocation gate — empty books keep the pre-emoji allocation profile).
    pub fn has_fallbacks(&self) -> bool {
        !self.fallbacks.is_empty()
    }

    /// Allocate the RGBA color layer for a run (emoji batch): only when a
    /// fallback face is loaded AND the run actually visits one — books
    /// without fallbacks, and runs the primary pair fully covers, keep the
    /// exact pre-emoji allocation profile.
    fn color_layer_for(&self, text: &str, bold: bool, width: usize, height: usize) -> Option<Vec<u8>> {
        if !self.has_fallbacks() || width == 0 || height == 0 {
            return None;
        }
        let uses_fallback = self
            .segments(text, bold)
            .iter()
            .any(|(sel, _)| matches!(sel, FaceSel::Fallback(_)));
        uses_fallback.then(|| vec![0u8; width * height * 4])
    }

    fn face(&self, bold: bool) -> &Vec<u8> {
        if bold { &self.bold } else { &self.regular }
    }

    fn face_bytes(&self, sel: FaceSel, bold: bool) -> &Vec<u8> {
        match sel {
            FaceSel::Primary => self.face(bold),
            FaceSel::Fallback(i) => &self.fallbacks[i],
        }
    }

    /// Split `text` into consecutive same-face segments (emoji batch): a
    /// char maps to the primary pair when its cmap covers it, else to the
    /// first fallback that does, else back to primary (.notdef — exactly
    /// the pre-fallback behavior for still-uncovered chars). Boundary
    /// kerning between segments is lost, but a fallback boundary only ever
    /// separates scripts — never a kerned pair.
    fn segments<'a>(&self, text: &'a str, bold: bool) -> Vec<(FaceSel, &'a str)> {
        // Parse each candidate face once per call — the cmap probes below run
        // per char, and re-parsing a face (table-directory walk) per char
        // would dominate measurement on long pages.
        let primary = FontRef::from_index(self.face(bold), 0);
        let fallbacks: Vec<Option<FontRef>> =
            self.fallbacks.iter().map(|b| FontRef::from_index(b, 0)).collect();
        let covers = |f: Option<FontRef>, ch: char| -> bool {
            // GlyphId is a plain u16 alias; 0 is .notdef.
            f.map(|f| f.charmap().map(ch) != 0).unwrap_or(false)
        };
        let pick = |ch: char| -> FaceSel {
            if covers(primary, ch) {
                return FaceSel::Primary;
            }
            for (i, f) in fallbacks.iter().enumerate() {
                if covers(*f, ch) {
                    return FaceSel::Fallback(i);
                }
            }
            FaceSel::Primary
        };
        let mut out: Vec<(FaceSel, &str)> = Vec::new();
        let mut start = 0usize;
        let mut cur: Option<FaceSel> = None;
        for (i, ch) in text.char_indices() {
            let sel = pick(ch);
            match cur {
                Some(prev) if prev == sel => continue,
                Some(prev) => {
                    out.push((prev, &text[start..i]));
                    start = i;
                    cur = Some(sel);
                }
                None => cur = Some(sel),
            }
        }
        if let Some(prev) = cur {
            out.push((prev, &text[start..]));
        }
        out
    }

    /// Shaped advance of `text` at `font_size`, in px. Kerning (GPOS) and
    /// ligatures apply; CJK comes out at one full-width advance per glyph.
    pub fn advance_width(&self, text: &str, font_size: f32, bold: bool) -> f32 {
        let mut total = 0.0f32;
        for (sel, seg) in self.segments(text, bold) {
            let bytes = self.face_bytes(sel, bold);
            let Some(font) = FontRef::from_index(bytes, 0) else { continue };
            SHAPE_CTX.with_borrow_mut(|ctx| {
                let mut shaper = ctx.builder(font).size(font_size).build();
                shaper.add_str(seg);
                shaper.shape_with(|cluster| total += cluster.advance());
            });
        }
        total
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
    pub fn rasterize(&self, text: &str, font_size: f32, bold: bool, color: [u8; 4], line_height: f32) -> Arc<TextRaster> {
        let key = RasterKey {
            fingerprint: self.fingerprint,
            kind: RasterKind::Line,
            text: text.into(),
            font_size_bits: font_size.to_bits(),
            bold,
            color,
            line_height_bits: line_height.to_bits(),
        };
        RasterCache::get_or_insert(key, || self.rasterize_line_uncached(text, font_size, bold, color, line_height))
    }

    fn rasterize_line_uncached(&self, text: &str, font_size: f32, bold: bool, color: [u8; 4], line_height: f32) -> TextRaster {
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
        let mut layer = self.color_layer_for(text, bold, width, height);
        self.blit_line(
            &mut alpha,
            layer.as_deref_mut(),
            width,
            height,
            text,
            font_size,
            bold,
            0.0,
            baseline - top,
        );
        let data = colorize_layered(&alpha, layer.as_deref(), color);
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
    ) -> Arc<TextRaster> {
        let key = RasterKey {
            fingerprint: self.fingerprint,
            kind: RasterKind::Wrapped { wrap_at_bits: wrap_at.to_bits() },
            text: text.into(),
            font_size_bits: font_size.to_bits(),
            bold,
            color,
            line_height_bits: line_height.to_bits(),
        };
        RasterCache::get_or_insert(key, || {
            self.rasterize_wrapped_uncached(text, font_size, bold, color, wrap_at, line_height)
        })
    }

    fn rasterize_wrapped_uncached(
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
        // One color layer for the whole tile: every line's fallback glyphs
        // max-blend into the same RGBA surface, then colorize merges it over
        // the mono coverage once.
        let mut layer = self.color_layer_for(&tokens.iter().map(|t| t.text.as_str()).collect::<String>(), bold, width, height);
        for (line, baseline) in lines.iter().zip(&baselines) {
            if line.token_idx.is_empty() {
                continue;
            }
            let s: String =
                line.token_idx.iter().map(|&i| tokens[i].text.as_str()).collect();
            self.blit_line(&mut alpha, layer.as_deref_mut(), width, height, &s, font_size, bold, 0.0, baseline - top);
        }
        let data = colorize_layered(&alpha, layer.as_deref(), color);
        TextRaster { width, height, baseline: baselines[0] - top, top, data }
    }

    /// Shape `text` and blit its glyphs into an A8 `alpha` buffer (max
    /// blend) at pen origin `x0` with the baseline `baseline` rows from the
    /// tile top — the shared raster core behind [`Self::rasterize`] and
    /// [`Self::rasterize_wrapped`]. Emoji batch: the run segments by face;
    /// primary segments keep the outline/Alpha path bit-for-bit, fallback
    /// segments additionally try embedded color bitmaps (Apple sbix / CBDT
    /// strikes, best fit) and land their RGBA pixels in `color_layer` —
    /// same tile geometry, own colors, max-alpha blend like the mono path.
    fn blit_line(
        &self,
        alpha: &mut [u8],
        mut color_layer: Option<&mut [u8]>,
        width: usize,
        height: usize,
        text: &str,
        font_size: f32,
        bold: bool,
        x0: f32,
        baseline: f32,
    ) {
        let mono_sources = [Source::Outline];
        let fallback_sources = [
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Bitmap(StrikeWith::BestFit),
            Source::Outline,
        ];
        // The pen carries ACROSS segments (emoji batch follow-up): a run
        // like "汉字🚀" segments into [primary CJK, fallback emoji], and the
        // fallback segment must continue at the primary's final advance —
        // a per-segment pen restart stacked the emoji on the run's first
        // glyphs (advance_width accumulated correctly, so measure agreed
        // while paint overlapped).
        let mut pen = x0;
        for (sel, seg) in self.segments(text, bold) {
            let bytes = self.face_bytes(sel, bold);
            let Some(font) = FontRef::from_index(bytes, 0) else { continue };

            // Shape once: absolute x per glyph + y offset from the baseline.
            let mut glyphs: Vec<(f32, f32, GlyphId)> = Vec::new();
            SHAPE_CTX.with_borrow_mut(|ctx| {
                let mut shaper = ctx.builder(font).size(font_size).build();
                shaper.add_str(seg);
                shaper.shape_with(|cluster| {
                    for g in cluster.glyphs {
                        glyphs.push((pen + g.x, g.y, g.id));
                        pen += g.advance;
                    }
                });
            });

            SCALE_CTX.with_borrow_mut(|sctx| {
                let mut scaler = sctx.builder(font).size(font_size).build();
                let sources = match sel {
                    FaceSel::Primary => &mono_sources[..],
                    FaceSel::Fallback(_) => &fallback_sources[..],
                };
                let render = Render::new(sources);
                for (pen_x, dy, gid) in glyphs {
                    // swash rasterizes outlines with zeno Origin::BottomLeft,
                    // so `placement.top` is the image's top edge ABOVE the
                    // pen: blit y = pen_y - top (data rows are ordinary
                    // top-down). Bitmap strikes carry the same contract.
                    let Some(img) = render.render(&mut scaler, gid) else { continue };
                    let ox = pen_x.round() as i64 + img.placement.left as i64;
                    let oy = (baseline + dy).round() as i64 - img.placement.top as i64;
                    let color_px = matches!(img.content, Content::Color)
                        && color_layer.as_ref().is_some_and(|l| !l.is_empty());
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
                            if color_px {
                                let si = ((gy * img.placement.width as i64 + gx) * 4) as usize;
                                let a = img.data[si + 3];
                                if a == 0 {
                                    continue;
                                }
                                let di = (ty * width + tx) * 4;
                                let layer = color_layer.as_mut().unwrap();
                                if a >= layer[di + 3] {
                                    layer[di..di + 4].copy_from_slice(&[
                                        img.data[si],
                                        img.data[si + 1],
                                        img.data[si + 2],
                                        a,
                                    ]);
                                }
                            } else {
                                let cov =
                                    img.data[(gy * img.placement.width as i64 + gx) as usize];
                                let slot = &mut alpha[ty * width + tx];
                                *slot = (*slot).max(cov);
                            }
                        }
                    }
                }
            });
        }
    }
}

/// Colorize an A8 coverage buffer into straight-alpha RGBA8.
fn colorize(alpha: &[u8], color: [u8; 4]) -> Vec<u8> {
    let mut data = vec![0u8; alpha.len() * 4];
    // Element-subtree opacity rides color[3] (the layout walk's `with_alpha`
    // folds it there like it does for Bg/Border/Image) — scale the glyph
    // coverage by it so a faded element fades its text too. Dropping it made
    // `opacity: 0` hide backgrounds but leave text fully inked.
    let ca = color[3] as u16;
    for (i, a) in alpha.iter().enumerate() {
        if *a == 0 {
            continue;
        }
        let a = (*a as u16 * ca / 255) as u8;
        if a == 0 {
            continue;
        }
        data[i * 4..i * 4 + 4].copy_from_slice(&[color[0], color[1], color[2], a]);
    }
    data
}

/// [`colorize`] with the emoji batch's color layer merged over the mono
/// fill: a layer pixel with alpha ≥ the mono coverage wins outright (color
/// glyphs bring their own RGB — the fill color must not tint them), with
/// the same color[3] opacity fold `colorize` applies, so a faded element
/// fades its emoji too. Where no layer ink exists the mono path is
/// byte-for-byte `colorize`.
fn colorize_layered(alpha: &[u8], layer: Option<&[u8]>, color: [u8; 4]) -> Vec<u8> {
    let mut data = colorize(alpha, color);
    let Some(layer) = layer else { return data };
    let ca = color[3] as u16;
    for (i, px) in layer.chunks_exact(4).enumerate() {
        let a = px[3];
        if a == 0 || i * 4 + 4 > data.len() {
            continue;
        }
        let a = (a as u16 * ca / 255) as u8;
        if a == 0 || a < data[i * 4 + 3] {
            continue;
        }
        data[i * 4..i * 4 + 4].copy_from_slice(&[px[0], px[1], px[2], a]);
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
/// The pixel-level raster cache (#399): rasterizing a run is the paint
/// floor (~44ms/frame on text-heavy pages — every text leaf re-shapes and
/// re-rasters every frame even when its pixels can't have changed), yet a
/// raster is a pure function of (book faces, text, size, bold, color,
/// wrap, line height). The diting stack has no dynamic webfont loading at
/// all — `@font-face` at-rules drop in the CSS parser and the JS `FontFace`
/// class is an inert stub — so the face set is constant per process and
/// keying on the book's content fingerprint is sound with no
/// fonts-settling gate (the swap-fallback-glyphs-pinned-by-cache hazard
/// html-video hit cannot arise here; if webfont loading ever lands, the
/// gate becomes a prerequisite of this cache, not of this module).
///
/// Budget: fixed 48 MB, clear-all on overflow. A page's working set is a
/// few MB of tiles; the clear is a full re-raster of the next pass, rare
/// enough in practice that LRU bookkeeping isn't worth its complexity.
struct RasterCache;
static RASTER_CACHE: std::sync::LazyLock<Mutex<HashMap<RasterKey, Arc<TextRaster>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static RASTER_CACHE_BYTES: AtomicUsize = AtomicUsize::new(0);
static RASTER_CACHE_MAX: AtomicUsize = AtomicUsize::new(48 * 1024 * 1024);
static RASTER_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static RASTER_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Which rasterizer a key belongs to — the single-line [`FontBook::rasterize`]
/// and the wrapping [`FontBook::rasterize_wrapped`] produce different tiles
/// from the same text, so the kind rides in the key.
#[derive(PartialEq, Eq, Hash)]
enum RasterKind {
    Line,
    Wrapped { wrap_at_bits: u32 },
}

#[derive(PartialEq, Eq, Hash)]
struct RasterKey {
    fingerprint: u64,
    kind: RasterKind,
    text: Box<str>,
    font_size_bits: u32,
    bold: bool,
    color: [u8; 4],
    line_height_bits: u32,
}

impl RasterCache {
    fn get_or_insert(key: RasterKey, make: impl FnOnce() -> TextRaster) -> Arc<TextRaster> {
        if let Some(hit) = RASTER_CACHE.lock().unwrap().get(&key) {
            RASTER_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return Arc::clone(hit);
        }
        RASTER_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        let raster = Arc::new(make());
        // Rough entry weight: tile bytes plus the key's text. Tiles dominate;
        // exactness would buy nothing the clear-all policy can spend.
        let bytes = raster.data.len() + key.text.len() + 64;
        let max = RASTER_CACHE_MAX.load(Ordering::Relaxed);
        let mut map = RASTER_CACHE.lock().unwrap();
        if bytes >= max || RASTER_CACHE_BYTES.load(Ordering::Relaxed) + bytes > max {
            map.clear();
            RASTER_CACHE_BYTES.store(0, Ordering::Relaxed);
        }
        RASTER_CACHE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        map.insert(key, Arc::clone(&raster));
        raster
    }

    /// Test isolation: every cache test starts from a clean slate (parallel
    /// tests share the process-wide map).
    #[cfg(test)]
    fn reset() {
        RASTER_CACHE.lock().unwrap().clear();
        RASTER_CACHE_BYTES.store(0, Ordering::Relaxed);
        RASTER_CACHE_HITS.store(0, Ordering::Relaxed);
        RASTER_CACHE_MISSES.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod raster_cache_tests {
    use super::*;

    /// Serialize budget-touching tests and restore the budget on scope exit —
    /// the env-knob guard precedent (1f7486c): the cache is process-global,
    /// a leaked tiny budget would silently neuter every later test. The guard
    /// OWNS the lock: releasing it at shrink() return would leave the tiny
    /// budget exposed to a concurrent test's inserts.
    pub(crate) struct BudgetGuard {
        _held: std::sync::MutexGuard<'static, ()>,
        prev: usize,
    }

    impl Drop for BudgetGuard {
        fn drop(&mut self) {
            RASTER_CACHE_MAX.store(self.prev, Ordering::Relaxed);
        }
    }

    static BUDGET_LOCK: Mutex<()> = Mutex::new(());

    /// A clean slate under the serialization lock — the starting posture of
    /// every cache test (parallel tests in other modules only ever INSERT;
    /// only these tests reset or shrink).
    pub(crate) fn isolated() -> std::sync::MutexGuard<'static, ()> {
        let held = BUDGET_LOCK.lock().unwrap();
        RasterCache::reset();
        held
    }

    pub(crate) fn shrink_budget_for_test(bytes: usize) -> BudgetGuard {
        let _held = BUDGET_LOCK.lock().unwrap();
        RasterCache::reset();
        let prev = RASTER_CACHE_MAX.swap(bytes, Ordering::Relaxed);
        BudgetGuard { _held, prev }
    }

    /// The production face bytes, for building sibling `FontBook`s in the
    /// fingerprint test (same bytes = another instance; swapped = a different
    /// face set).
    fn production_pair() -> (Vec<u8>, Vec<u8>) {
        crate::diting_fonts::bundled_pair_for_tests()
    }

    /// The cache contract's happy path: a repeated rasterize returns the SAME
    /// tile (`Arc::ptr_eq` — no re-shaping, no re-raster), while a different
    /// color bakes different pixels into a different tile.
    #[test]
    fn repeated_rasterize_shares_the_cached_tile() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        let _held = isolated();
        let black = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0);
        let again = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0);
        assert!(Arc::ptr_eq(&black, &again), "repeat must hand back the cached Arc");
        assert!(black.ink_bbox().is_some(), "the tile has real ink");

        let red = book.rasterize_wrapped("缓存命中", 16.0, false, [255, 0, 0, 255], 20.0, 24.0);
        assert!(!Arc::ptr_eq(&black, &red), "color rides the key — a new tile");
        let ink = |r: &TextRaster| {
            r.data
                .chunks_exact(4)
                .filter(|p| p[3] > 200)
                .map(|p| (p[0], p[1], p[2]))
                .max()
        };
        assert_eq!(ink(&black), Some((0, 0, 0)), "black tile's opaque ink is black");
        assert_eq!(ink(&red), Some((255, 0, 0)), "red tile's opaque ink is red");
    }

    /// `rasterize` (single line) and `rasterize_wrapped` are different
    /// rasterizers with different tiles — the kind rides the key, so the two
    /// must not collide even at identical text/params.
    #[test]
    fn line_and_wrapped_rasters_cache_separately() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        let _held = isolated();
        let text = "一行两行三行四行";
        let line = book.rasterize(text, 16.0, false, [0, 0, 0, 255], 24.0);
        // Narrow wrap: the same text breaks across 4+ lines, so the wrapped
        // tile is much taller than the single-line one.
        let wrapped = book.rasterize_wrapped(text, 16.0, false, [0, 0, 0, 255], 40.0, 24.0);
        assert!(!Arc::ptr_eq(&line, &wrapped), "kinds must not collide");
        assert!(
            wrapped.height > line.height * 2,
            "wrapped tile stacks lines ({} vs {})",
            wrapped.height,
            line.height
        );
    }

    /// Budget overflow clears everything: with the budget at exactly one
    /// entry's weight, the second insert wipes the map — the first entry's
    /// next rasterize comes back as a fresh tile.
    #[test]
    fn budget_overflow_clears_all() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        // Measure entry A's weight under a normal-budget lock, then shrink —
        // two lock windows (std Mutex is not reentrant), which is safe: other
        // budget tests serialize the same way and insert-only tests can't
        // shrink anything.
        let a_weight = {
            let _held = isolated();
            let probe = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0);
            probe.data.len() + 2 + 64
        };
        let _budget = shrink_budget_for_test(a_weight + 1);
        let a1 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0);
        let same = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0);
        assert!(Arc::ptr_eq(&a1, &same), "A alone fits the budget exactly");
        // A longer text is a strictly heavier entry (wider tile, longer key):
        // inserting it overflows and clears — A's tile is gone.
        let b = book.rasterize("iiiiiiiiiiii", 8.0, false, [0, 0, 0, 255], 10.0);
        assert!(!Arc::ptr_eq(&a1, &b));
        let a2 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0);
        assert!(!Arc::ptr_eq(&a1, &a2), "clear-all must evict the earlier entry");
    }

    /// The book's face fingerprint is the cache's book identity: two books
    /// built from the same bytes share entries (either instance's tile serves
    /// both), while a book with different face bytes gets its own.
    #[test]
    fn face_fingerprint_isolates_books() {
        let (reg, bold) = production_pair();
        let a = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
        let b = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
        // Swapped faces: a genuinely different byte set (regular slot carries
        // the bold face and vice versa), so a different fingerprint.
        let swapped = FontBook::from_pairs(bold, reg).unwrap();
        let _held = isolated();
        let r1 = a.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0);
        let r2 = b.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0);
        assert!(Arc::ptr_eq(&r1, &r2), "same face bytes share entries across instances");
        let r3 = swapped.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0);
        assert!(!Arc::ptr_eq(&r1, &r3), "different face bytes must not share entries");
    }
}

/// One rasterized text tile: what the rasterizers below return and what the
/// paint stack blits. `Clone` exists for the mutating consumers (gradient
/// recolor rewrites pixels in place) — the cache hands out `Arc`s, those
/// callers clone before they mutate.
#[derive(Debug, Clone)]
pub struct TextRaster {
    pub width: usize,
    pub height: usize,
    /// Distance from the tile's top edge to the FIRST line's baseline, px.
    #[allow(dead_code)] // raster metadata: the compositor blits by `top` today; line-box baseline assembly (text batch) is the pending reader
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Element opacity rides the text color's alpha channel (the layout
    /// walk's `with_alpha`); `colorize` must multiply it into the glyph
    /// coverage — not paste RGB at full coverage, which made `opacity: 0`
    /// hide backgrounds while leaving text fully inked.
    #[test]
    fn colorize_folds_color_alpha_into_coverage() {
        let tile = colorize(&[255, 128, 0], [10, 20, 30, 128]);
        // 255 * 128/255 = 128, 128 * 128/255 = 64.
        assert_eq!(&tile[0..4], &[10, 20, 30, 128]);
        assert_eq!(&tile[4..8], &[10, 20, 30, 64]);
        // Zero coverage stays empty; zero color alpha kills any coverage.
        assert_eq!(&tile[8..12], &[0, 0, 0, 0]);
        let gone = colorize(&[255, 200], [10, 20, 30, 0]);
        assert_eq!(gone.iter().filter(|&&b| b != 0).count(), 0);
        // Fully opaque color is the old behavior, byte for byte.
        let solid = colorize(&[255, 128, 0], [10, 20, 30, 255]);
        assert_eq!(&solid[0..4], &[10, 20, 30, 255]);
        assert_eq!(&solid[4..8], &[10, 20, 30, 128]);
    }
}
