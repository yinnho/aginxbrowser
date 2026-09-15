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
/// selected by `bold`), the bundled monospace face (single-weight, like the
/// emoji fallback), or one of the fallback tails — single-weight faces
/// where bold is the same face (as in browsers).
#[derive(Clone, Copy, PartialEq, Debug)]
enum FaceSel {
    Primary,
    Mono,
    Fallback(usize),
}

/// The embedded PDF face a shaped glyph belongs to (vector-text batch): the
/// primary pair by weight plus the monospace face. Fallback faces (emoji
/// color bitmaps) never get one — a run visiting a fallback stays raster.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub(crate) enum PdfFace {
    Regular,
    Bold,
    Mono,
}

/// One shaped glyph positioned for the PDF text layer: glyph id in the
/// embedded face's own space (the writer embeds the exact same face bytes,
/// so ids need no remap), absolute x / baseline-relative y in px, the
/// shaped advance in px (feeds the CIDFont /W table), and the cluster's
/// source text on the cluster's first glyph only (ToUnicode CMap).
#[derive(Clone, Debug)]
pub(crate) struct PdfGlyph {
    pub gid: u16,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    pub face: PdfFace,
    pub unicode: String,
}

/// A regular/bold face pair loaded from raw TTF/OTF bytes.
///
/// Deliberately minimal: one family, two weights, index-0 face. Weight
/// matching beyond the pair (500, 800, …) snaps to the nearer face, which is
/// also all the cross-check fixtures exercise. The optional monospace face
/// serves `font-family: monospace` runs (see [`FontBook::with_mono`]);
/// fallback faces (emoji batch) carry codepoints the pair lacks — see
/// [`FaceSel`].
pub struct FontBook {
    regular: Vec<u8>,
    bold: Vec<u8>,
    mono: Option<Vec<u8>>,
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
fn face_fingerprint(regular: &[u8], bold: &[u8], mono: Option<&[u8]>, fallbacks: &[Vec<u8>]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    regular.hash(&mut h);
    bold.hash(&mut h);
    mono.hash(&mut h);
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
        let fingerprint = face_fingerprint(&regular, &bold, None, &[]);
        Some(Self { regular, bold, mono: None, fallbacks: Vec::new(), fingerprint })
    }

    /// Append single-weight fallback faces (emoji batch): unparseable bytes
    /// drop silently, parseable ones join the tail in order. A char the
    /// primary pair doesn't map resolves through the first fallback that
    /// covers it; measure and paint segment identically, so a mixed
    /// "text 🚀 text" run advances the same bytes it rasterizes.
    pub fn with_fallbacks(mut self, faces: Vec<Vec<u8>>) -> Self {
        self.fallbacks
            .extend(faces.into_iter().filter(|b| FontRef::from_index(b, 0).is_some()));
        self.fingerprint =
            face_fingerprint(&self.regular, &self.bold, self.mono.as_deref(), &self.fallbacks);
        self
    }

    /// Install the bundled monospace face (mono batch): single-weight, like
    /// the fallbacks — a `bold: true` mono run shapes the same face. Books
    /// without a usable mono face (cross-check fixture books; unparseable
    /// bytes drop silently, the `with_fallbacks` posture) render `monospace`
    /// runs with the primary pair: the exact pre-batch behavior, so the
    /// routing is invisible wherever no mono face is loaded.
    pub fn with_mono(mut self, bytes: Vec<u8>) -> Self {
        if FontRef::from_index(&bytes, 0).is_some() {
            self.mono = Some(bytes);
        }
        self.fingerprint =
            face_fingerprint(&self.regular, &self.bold, self.mono.as_deref(), &self.fallbacks);
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
    fn color_layer_for(
        &self,
        text: &str,
        bold: bool,
        mono: bool,
        width: usize,
        height: usize,
    ) -> Option<Vec<u8>> {
        if !self.has_fallbacks() || width == 0 || height == 0 {
            return None;
        }
        let uses_fallback = self
            .segments(text, bold, mono)
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
            // Minted only when the mono face parsed (see `segments`); bold
            // resolves to the same single-weight face.
            FaceSel::Mono => self.mono.as_ref().expect("mono sel without a mono face"),
            FaceSel::Fallback(i) => &self.fallbacks[i],
        }
    }

    /// Split `text` into consecutive same-face segments (emoji batch): a
    /// char maps to the mono face first when the run is mono and it's
    /// covered, then to the primary pair when its cmap covers it, else to
    /// the first fallback that does, else back to primary (.notdef — exactly
    /// the pre-fallback behavior for still-uncovered chars). Boundary
    /// kerning between segments is lost, but a fallback boundary only ever
    /// separates scripts — never a kerned pair.
    fn segments<'a>(&self, text: &'a str, bold: bool, mono: bool) -> Vec<(FaceSel, &'a str)> {
        // Parse each candidate face once per call — the cmap probes below run
        // per char, and re-parsing a face (table-directory walk) per char
        // would dominate measurement on long pages.
        let primary = FontRef::from_index(self.face(bold), 0);
        let mono_face = if mono {
            self.mono.as_deref().and_then(|b| FontRef::from_index(b, 0))
        } else {
            None
        };
        let fallbacks: Vec<Option<FontRef>> =
            self.fallbacks.iter().map(|b| FontRef::from_index(b, 0)).collect();
        let covers = |f: Option<FontRef>, ch: char| -> bool {
            // GlyphId is a plain u16 alias; 0 is .notdef.
            f.map(|f| f.charmap().map(ch) != 0).unwrap_or(false)
        };
        let pick = |ch: char| -> FaceSel {
            // A mono run sends every char the mono face covers to it (ASCII —
            // the point of the face); the rest falls through to the primary
            // pair, then the fallback tails — the browser per-char cascade
            // (code blocks with Chinese comments render CJK in the CJK face).
            // No mono face (or mono=false): this arm never fires.
            if covers(mono_face, ch) {
                return FaceSel::Mono;
            }
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
    pub fn advance_width(&self, text: &str, font_size: f32, bold: bool, mono: bool) -> f32 {
        let mut total = 0.0f32;
        for (sel, seg) in self.segments(text, bold, mono) {
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

    /// Shape `text` into per-glyph records for the PDF text layer
    /// (vector-text batch) — the SAME face segmentation and swash shaping
    /// `blit_line` runs, so glyph ids and pen positions match the raster
    /// bit-for-bit. Returns `None` when any segment routes to a fallback
    /// face: those glyphs (emoji color bitmaps) have no outline in the
    /// embedded faces and stay in the raster; the caller keeps the whole
    /// item raster-only.
    pub(crate) fn pdf_shape(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        mono: bool,
    ) -> Option<Vec<PdfGlyph>> {
        let mut out: Vec<PdfGlyph> = Vec::new();
        let mut pen = 0.0f32;
        for (sel, seg) in self.segments(text, bold, mono) {
            let face = match sel {
                FaceSel::Primary if bold => PdfFace::Bold,
                FaceSel::Primary => PdfFace::Regular,
                FaceSel::Mono => PdfFace::Mono,
                FaceSel::Fallback(_) => return None,
            };
            let bytes = self.face_bytes(sel, bold);
            let Some(font) = FontRef::from_index(bytes, 0) else { continue };
            SHAPE_CTX.with_borrow_mut(|ctx| {
                let mut shaper = ctx.builder(font).size(font_size).build();
                shaper.add_str(seg);
                shaper.shape_with(|cluster| {
                    // Cluster source offsets are byte indices into the
                    // segment (add_str feeds char_indices offsets).
                    let cluster_text = &seg[cluster.source.to_range()];
                    for (gi, g) in cluster.glyphs.iter().enumerate() {
                        out.push(PdfGlyph {
                            gid: g.id,
                            x: pen + g.x,
                            y: g.y,
                            advance: g.advance,
                            face,
                            // Ligature/mark clusters: the first glyph
                            // carries the cluster's text for ToUnicode;
                            // the rest map to nothing.
                            unicode: if gi == 0 { cluster_text.to_string() } else { String::new() },
                        });
                        pen += g.advance;
                    }
                });
            });
        }
        Some(out)
    }

    /// Vertical metrics of a [`PdfFace`] in FONT UNITS (the
    /// FontDescriptor coordinate space — not the px-scaled `metrics`).
    /// `(ascent, descent, units_per_em)` with descent negative per PDF.
    pub(crate) fn pdf_face_metrics(&self, face: PdfFace) -> Option<(f32, f32, f32)> {
        let sel = match face {
            PdfFace::Regular => FaceSel::Primary,
            PdfFace::Bold => FaceSel::Primary,
            PdfFace::Mono => FaceSel::Mono,
        };
        let bytes = self.face_bytes(sel, face == PdfFace::Bold);
        let font = FontRef::from_index(bytes, 0)?;
        let m = MetricsProxy::from_font(&font).materialize_metrics(&font, &[]);
        // swash reports the descender as a positive magnitude (see
        // `metrics`); PDF /Descent is negative below the baseline.
        Some((m.ascent, -m.descent.abs(), m.units_per_em as f32))
    }

    /// Raw TTF bytes of a [`PdfFace`] — the FontFile2 payload for the PDF
    /// text layer. `PdfFace::Mono` only ever appears when the mono face
    /// parsed (see `pdf_shape`), so the fallback arm is unreachable.
    pub(crate) fn pdf_face_bytes(&self, face: PdfFace) -> &[u8] {
        match face {
            PdfFace::Regular => &self.regular,
            PdfFace::Bold => &self.bold,
            PdfFace::Mono => self.mono.as_deref().unwrap_or(&self.regular),
        }
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
    pub fn rasterize(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        line_height: f32,
        mono: bool,
    ) -> Arc<TextRaster> {
        let key = RasterKey {
            fingerprint: self.fingerprint,
            kind: RasterKind::Line,
            text: text.into(),
            font_size_bits: font_size.to_bits(),
            bold,
            mono,
            color,
            line_height_bits: line_height.to_bits(),
            word_spacing_bits: 0.0f32.to_bits(), // single-line path applies no word-spacing
        };
        RasterCache::get_or_insert(key, || {
            self.rasterize_line_uncached(text, font_size, bold, color, line_height, mono)
        })
    }

    fn rasterize_line_uncached(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        line_height: f32,
        mono: bool,
    ) -> TextRaster {
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
        let width = self.advance_width(text, font_size, bold, mono).ceil() as usize + 2;

        let mut alpha = vec![0u8; width * height];
        let mut layer = self.color_layer_for(text, bold, mono, width, height);
        self.blit_line(
            &mut alpha,
            layer.as_deref_mut(),
            width,
            height,
            text,
            font_size,
            bold,
            mono,
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
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_wrapped(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        wrap_at: f32,
        line_height: f32,
        mono: bool,
        word_spacing: f32,
        truncate_at: Option<f32>,
        ws: crate::diting_css::WhiteSpace,
    ) -> Arc<TextRaster> {
        self.rasterize_wrapped_with(
            text, font_size, bold, color, wrap_at, line_height, mono, word_spacing, truncate_at,
            ws, None,
        )
    }

    /// Same raster with the run's wrap tokens handed in pre-shaped
    /// (obscura#983's paint half): the leaf memo from the measure half is
    /// still warm at first-raster time, so a cache miss reuses it instead of
    /// shaping the run a second time. Tokens must have been shaped with the
    /// same (text, font_size, bold, mono, word_spacing, ws) — the paint arm
    /// only forwards them when the item's font params are the leaf's
    /// unscaled ones.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_wrapped_with(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        wrap_at: f32,
        line_height: f32,
        mono: bool,
        word_spacing: f32,
        truncate_at: Option<f32>,
        ws: crate::diting_css::WhiteSpace,
        tokens: Option<std::rc::Rc<[Token]>>,
    ) -> Arc<TextRaster> {
        let key = RasterKey {
            fingerprint: self.fingerprint,
            kind: RasterKind::Wrapped {
                wrap_at_bits: wrap_at.to_bits(),
                truncate_at_bits: truncate_at.unwrap_or(0.0).to_bits(),
                ws_bits: ws as u32,
            },
            text: text.into(),
            font_size_bits: font_size.to_bits(),
            bold,
            mono,
            color,
            line_height_bits: line_height.to_bits(),
            word_spacing_bits: word_spacing.to_bits(),
        };
        RasterCache::get_or_insert(key, || {
            self.rasterize_wrapped_uncached(
                text,
                font_size,
                bold,
                color,
                wrap_at,
                line_height,
                mono,
                word_spacing,
                truncate_at,
                ws,
                tokens.as_deref(),
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn rasterize_wrapped_uncached(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        wrap_at: f32,
        line_height: f32,
        mono: bool,
        word_spacing: f32,
        truncate_at: Option<f32>,
        ws: crate::diting_css::WhiteSpace,
        pre_shaped: Option<&[Token]>,
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
        let owned;
        let tokens = match pre_shaped {
            Some(t) => t,
            None => {
                owned = tokens_of(text, font_size, bold, self, mono, word_spacing, ws);
                &owned
            }
        };
        // text-overflow: ellipsis (nowrap+ellipsis batch): when the run
        // overflows `truncate_at`, drop whole trailing tokens and append the
        // U+2026 marker (tokenized with the run's own font params, so the
        // fallback chain covers it). The marker is part of the painted
        // tokens; decorations take the kept tokens only (Chrome leaves the
        // ellipsis undecorated).
        let truncated;
        let tokens: &[Token] = match truncate_at.and_then(|limit| {
            truncate_tokens(tokens, limit, font_size, bold, self, mono, word_spacing, ws)
        }) {
            Some((mut kept, marker)) => {
                kept.extend(marker);
                truncated = kept;
                &truncated
            }
            None => tokens,
        };
        let lines = greedy_wrap(tokens, Some(wrap_at.max(0.0)), ws);
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
        let mut layer = self.color_layer_for(&tokens.iter().map(|t| t.text.as_str()).collect::<String>(), bold, mono, width, height);
        if word_spacing == 0.0 {
            for (line, baseline) in lines.iter().zip(&baselines) {
                if line.token_idx.is_empty() {
                    continue;
                }
                let s: String =
                    line.token_idx.iter().map(|&i| tokens[i].text.as_str()).collect();
                self.blit_line(&mut alpha, layer.as_deref_mut(), width, height, &s, font_size, bold, mono, 0.0, baseline - top);
            }
        } else {
            // Word-spacing run: blit token by token at the cumulative pen so
            // the widened space advances actually separate the words (the
            // whole-line shape above would draw them at natural spacing).
            // Same token model measurement uses — one shape per token.
            for (line, baseline) in lines.iter().zip(&baselines) {
                let mut pen = 0.0f32;
                for &i in &line.token_idx {
                    let t = &tokens[i];
                    if !t.is_space {
                        self.blit_line(&mut alpha, layer.as_deref_mut(), width, height, &t.text, font_size, bold, mono, pen, baseline - top);
                    }
                    pen += t.width;
                }
            }
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
        mono: bool,
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
        for (sel, seg) in self.segments(text, bold, mono) {
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
                    FaceSel::Primary | FaceSel::Mono => &mono_sources[..],
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
#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub text: String,
    pub width: f32,
    pub is_space: bool,
    /// A preserved newline (white-space pre family): greedy_wrap ends the
    /// current line on it; it never joins a WrapLine itself.
    pub is_break: bool,
}

/// Tokenize a run's text under its computed `white-space` mode and shape
/// every token (shared by `measure_text_leaf` and `rasterize_wrapped`).
/// `word_spacing` (px) adds to each rendered space token's advance
/// (CSS Text §7.1) so measure, paint and the shared wrap breaker all see
/// the same widened widths.
pub(crate) fn tokens_of(
    text: &str,
    font_size: f32,
    bold: bool,
    fonts: &FontBook,
    mono: bool,
    word_spacing: f32,
    ws: crate::diting_css::WhiteSpace,
) -> Vec<Token> {
    super::tokenize_ws(text, ws)
        .into_iter()
        .map(|t| {
            let is_break = t == "\n";
            let is_space = !is_break && t.trim().is_empty();
            Token {
                is_space,
                is_break,
                width: if is_break {
                    0.0
                } else {
                    fonts.advance_width(&t, font_size, bold, mono)
                        + if is_space { word_spacing } else { 0.0 }
                },
                text: t,
            }
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
pub(crate) fn greedy_wrap(
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
    let marker = tokens_of("\u{2026}", font_size, bold, fonts, mono, word_spacing, ws);
    let marker_w = marker.first().map(|t| t.width).unwrap_or(0.0);
    let mut kept: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut w = 0.0f32;
    for t in tokens {
        if w + t.width + marker_w > limit {
            // Chrome cuts at glyph boundaries: an unbreakable word that
            // doesn't fit whole still fills the remaining space char by
            // char. Per-char widths skip intra-run kerning (v1 posture).
            if !t.is_space {
                let mut acc = String::new();
                let mut acc_w = w;
                for ch in t.text.chars() {
                    let cw = tokens_of(&ch.to_string(), font_size, bold, fonts, mono, 0.0, ws)
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
        });
    }
    // Whitespace left at the cut carries no ink and would only sit between
    // the kept glyphs and the marker.
    while kept.last().is_some_and(|t| t.is_space) {
        kept.pop();
    }
    Some((kept, marker))
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
/// raster is a pure function of (book faces, text, size, bold, mono,
/// color, wrap, line height). The diting stack has no dynamic webfont loading at
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
    Wrapped {
        wrap_at_bits: u32,
        /// `text-overflow: ellipsis` truncation limit, bits of the limit px
        /// (`truncate_at_bits == 0.0f32.to_bits()` = no truncation). Part of
        /// the key: a nowrap+ellipsis run and the same run untruncated share
        /// the +inf wrap width but must rasterize differently.
        truncate_at_bits: u32,
        /// `white-space` mode (pre family): wrap_at=+inf alone can't tell
        /// `nowrap` (collapse, one line) from `pre` (preserve, hard breaks),
        /// and the token shapes themselves diverge.
        ws_bits: u32,
    },
}

#[derive(PartialEq, Eq, Hash)]
struct RasterKey {
    fingerprint: u64,
    kind: RasterKind,
    text: Box<str>,
    font_size_bits: u32,
    bold: bool,
    mono: bool,
    color: [u8; 4],
    line_height_bits: u32,
    word_spacing_bits: u32,
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
        let black = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal);
        let again = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal);
        assert!(Arc::ptr_eq(&black, &again), "repeat must hand back the cached Arc");
        assert!(black.ink_bbox().is_some(), "the tile has real ink");

        let red = book.rasterize_wrapped("缓存命中", 16.0, false, [255, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal);
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
        let line = book.rasterize(text, 16.0, false, [0, 0, 0, 255], 24.0, false);
        // Narrow wrap: the same text breaks across 4+ lines, so the wrapped
        // tile is much taller than the single-line one.
        let wrapped = book.rasterize_wrapped(text, 16.0, false, [0, 0, 0, 255], 40.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal);
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
            let probe = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false);
            probe.data.len() + 2 + 64
        };
        let _budget = shrink_budget_for_test(a_weight + 1);
        let a1 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false);
        let same = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false);
        assert!(Arc::ptr_eq(&a1, &same), "A alone fits the budget exactly");
        // A longer text is a strictly heavier entry (wider tile, longer key):
        // inserting it overflows and clears — A's tile is gone.
        let b = book.rasterize("iiiiiiiiiiii", 8.0, false, [0, 0, 0, 255], 10.0, false);
        assert!(!Arc::ptr_eq(&a1, &b));
        let a2 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false);
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
        let r1 = a.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false);
        let r2 = b.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false);
        assert!(Arc::ptr_eq(&r1, &r2), "same face bytes share entries across instances");
        let r3 = swapped.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false);
        assert!(!Arc::ptr_eq(&r1, &r3), "different face bytes must not share entries");
    }

    /// The mono flag rides the key: a `mono: true` rasterize must not collide
    /// with the `mono: false` twin even on a mono-less book (identical
    /// pixels, distinct entry), and a book WITH the bundled mono face gets
    /// its own tiles. Plus the advance contract that motivated the face: ten
    /// ASCII digits at 20px shape to exactly 120px (0.6em each — Chrome's
    /// Courier advance), while CJK in the same mono run keeps the CJK face.
    #[test]
    fn mono_rides_the_key_and_shapes_fixed_advance() {
        let (reg, bold) = production_pair();
        let sans = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
        let mono_book =
            FontBook::from_pairs(reg, bold).unwrap().with_mono(crate::diting_fonts::bundled_mono_for_tests());
        let _held = isolated();
        let plain = sans.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, false);
        let flagged = sans.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, true);
        assert!(!Arc::ptr_eq(&plain, &flagged), "mono flag rides the key");
        let faced = mono_book.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, true);
        assert!(!Arc::ptr_eq(&plain, &faced), "a mono face is different pixels");
        let adv = mono_book.advance_width("0000000000", 20.0, false, true);
        assert!((adv - 120.0).abs() < 0.01, "10 × 0.6em = 120px at 20px, got {adv}");
        let cjk_mono = mono_book.advance_width("汉", 20.0, false, true);
        let cjk_sans = sans.advance_width("汉", 20.0, false, false);
        assert!(
            (cjk_mono - cjk_sans).abs() < 0.01,
            "CJK keeps the CJK face in a mono run ({cjk_mono} vs {cjk_sans})"
        );
    }

    /// A book without a mono face degrades: `mono: true` segments exactly as
    /// `mono: false` — every char on the primary pair. The blitz cross-check
    /// fixture books never see the routing.
    #[test]
    fn mono_without_face_degrades_to_primary() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        let flagged = book.segments("code 汉 x", false, true);
        let plain = book.segments("code 汉 x", false, false);
        assert_eq!(flagged, plain, "no mono face: the flag is invisible");
        assert!(flagged.iter().all(|(sel, _)| *sel == FaceSel::Primary));
    }

    /// `word-spacing` rides the token model (CSS Text §7.1): exactly the
    /// rendered word-separator tokens — the collapsed " " tokens — grow by
    /// the extra advance; word tokens keep their natural width so wrap,
    /// measure and paint all see the same shifted columns.
    #[test]
    fn tokens_of_applies_word_spacing_to_space_tokens_only() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        let plain = tokens_of("ab cd ef", 16.0, false, &book, false, 0.0, crate::diting_css::WhiteSpace::Normal);
        let spaced = tokens_of("ab cd ef", 16.0, false, &book, false, 9.0, crate::diting_css::WhiteSpace::Normal);
        assert_eq!(plain.len(), spaced.len(), "spacing never changes the token count");
        for (p, s) in plain.iter().zip(&spaced) {
            assert_eq!(p.text, s.text);
            if p.is_space {
                assert!((s.width - (p.width + 9.0)).abs() < 0.01,
                    "space token grows by exactly the extra advance ({} → {})", p.width, s.width);
            } else {
                assert_eq!(p.width, s.width, "word token width is spacing-invariant");
            }
        }
        // `normal` is modeled as 0.0 — identical tokens.
        let normal = tokens_of("ab cd", 16.0, false, &book, false, 0.0, crate::diting_css::WhiteSpace::Normal);
        assert_eq!(normal[1].is_space, true);
    }

    /// The wrapped rasterizer must actually MOVE the second word, not just
    /// grow the tile: with ws≠0 the glyphs blit per token at the cumulative
    /// pen, so the ink's right edge widens by the extra advance too (the
    /// whole-line shape would have left the ink put). ws=0 keeps the exact
    /// existing path — byte-identical tile against a re-rasterize.
    #[test]
    fn rasterize_wrapped_word_spacing_shifts_ink() {
        let (reg, bold) = production_pair();
        let book = FontBook::from_pairs(reg, bold).unwrap();
        let _held = isolated();
        let tight = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal);
        let loose = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 12.0, None, crate::diting_css::WhiteSpace::Normal);
        assert_eq!(tight.height, loose.height, "one line either way");
        let ink_right = |r: &TextRaster| r.ink_bbox().map(|b| b.2).unwrap_or(0);
        assert!(
            (ink_right(&loose) - ink_right(&tight)) as f32 >= 11.0,
            "second word's ink shifts right by the extra advance (tight={}, loose={})",
            ink_right(&tight), ink_right(&loose)
        );
        // Negative spacing tightens toward overlap — still deterministic.
        let tight2 = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, -8.0, None, crate::diting_css::WhiteSpace::Normal);
        assert!(ink_right(&tight2) < ink_right(&tight), "negative ws pulls the second word left");
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

    /// text-overflow: ellipsis (blitz#888): fitting text returns None (the
    /// marker only renders on overflow, Chrome parity); overflowing text
    /// keeps the longest prefix whose ink plus the marker fits the limit,
    /// drops the trailing space, and the marker uses the run's own font
    /// params (U+2026, covered by the fallback chain).
    #[test]
    fn truncate_tokens_clips_to_limit_and_appends_marker() {
        let fonts = crate::diting_fonts::font_book();
        let tokens = tokens_of("alpha beta gamma delta", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal);
        let total: f32 = tokens.iter().map(|t| t.width).sum();

        assert!(
            truncate_tokens(&tokens, total + 1.0, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal).is_none(),
            "fitting text is untouched"
        );

        let limit = total / 2.0;
        let (kept, marker) = truncate_tokens(&tokens, limit, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal)
            .expect("overflowing text truncates");
        let kept_w: f32 = kept.iter().map(|t| t.width).sum();
        let marker_w: f32 = marker.iter().map(|t| t.width).sum();
        assert!(marker_w > 0.0 && marker.iter().any(|t| t.text.contains('\u{2026}')));
        assert!(
            kept_w + marker_w <= limit + f32::EPSILON,
            "kept {kept_w} + marker {marker_w} within {limit}"
        );
        assert!(kept.len() < tokens.len(), "prefix is strictly shorter");
        assert!(
            !kept.last().map(|t| t.is_space).unwrap_or(false),
            "trailing space is popped"
        );

        // A bold run's marker must measure wider than the normal run's —
        // font params flow through.
        let (_, bold_marker) = truncate_tokens(&tokens, limit, 16.0, true, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal).unwrap();
        let bold_w: f32 = bold_marker.iter().map(|t| t.width).sum();
        assert!(bold_w > marker_w, "bold marker measures wider ({bold_w} > {marker_w})");
    }

    /// Paint half of obscura#983: a pre-shaped token slice must rasterize
    /// byte-identically to re-shaping from scratch — same wrap lines, same
    /// ellipsis truncation, same tile.
    #[test]
    fn rasterize_wrapped_pre_shaped_matches_reshaped() {
        let fonts = crate::diting_fonts::font_book();
        let text = "淘宝商品列表页的一段中文文本需要折行处理".repeat(4);
        let tokens = tokens_of(&text, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal);

        let plain = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, None);
        let pre = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, Some(&tokens));
        assert_eq!((plain.width, plain.height, plain.baseline), (pre.width, pre.height, pre.baseline));
        assert_eq!(plain.data, pre.data);

        let plain_t = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, Some(320.0), crate::diting_css::WhiteSpace::Normal, None);
        let pre_t = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, Some(320.0), crate::diting_css::WhiteSpace::Normal, Some(&tokens));
        assert_eq!((plain_t.width, plain_t.height, plain_t.baseline), (pre_t.width, pre_t.height, pre_t.baseline));
        assert_eq!(plain_t.data, pre_t.data);
    }

    // ---- white-space pre family (batch 106) ----

    fn ws_lines(text: &str, ws: crate::diting_css::WhiteSpace, wrap_at: Option<f32>) -> Vec<WrapLine> {
        let fonts = crate::diting_fonts::font_book();
        let tokens = tokens_of(text, 16.0, false, &fonts, false, 0.0, ws);
        greedy_wrap(&tokens, wrap_at, ws)
    }

    /// `pre` tokenization preserves every space as its own token, expands a
    /// tab to 8 columns, and models CRLF (and lone CR) as one break.
    #[test]
    fn pre_tokens_preserve_whitespace_and_breaks() {
        let fonts = crate::diting_fonts::font_book();
        let toks = tokens_of("a  b\tc\r\nd", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Pre);
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["a", " ", " ", "b", "        ", "c", "\n", "d"]);
        assert!(toks[6].is_break && toks[6].width == 0.0);
        assert!(toks[4].is_space && toks[4].text.chars().all(|c| c == ' '));
    }

    /// `pre` wraps only on hard breaks; blank source lines stay blank lines.
    #[test]
    fn pre_wraps_on_newlines_and_keeps_blank_lines() {
        let lines = ws_lines("a\n\nb", crate::diting_css::WhiteSpace::Pre, None);
        assert_eq!(lines.len(), 3, "a, blank, b");
        assert_eq!(lines[1].width, 0.0, "the middle line is empty, not merged");
        // No soft wrap at all: an arbitrarily long run stays one line even
        // with a tiny wrap_at handed in (defensive — measure passes None).
        let lines = ws_lines(&"x".repeat(200), crate::diting_css::WhiteSpace::Pre, Some(10.0));
        assert_eq!(lines.len(), 1, "pre has no soft wrap opportunities");
    }

    /// `pre-wrap`: an overflowing space HANGS at the line end; only a word
    /// breaks down.
    #[test]
    fn pre_wrap_spaces_hang_at_line_end() {
        let fonts = crate::diting_fonts::font_book();
        let toks = tokens_of("aa   bb", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::PreWrap);
        let (w_aa, sp) = (toks[0].width, toks[1].width);
        let wrap_at = w_aa + 2.5 * sp; // "aa  " fits, the 3rd space hangs, "bb" breaks
        let lines = ws_lines("aa   bb", crate::diting_css::WhiteSpace::PreWrap, Some(wrap_at));
        assert_eq!(lines.len(), 2, "one break, before bb");
        assert_eq!(lines[0].token_idx, vec![0, 1, 2, 3], "aa + all three spaces hang on line 1");
        assert_eq!(lines[1].token_idx, vec![4], "bb alone on line 2");
    }

    /// `break-spaces`: nothing hangs — the overflowing space itself starts
    /// the next line, so preserved spaces can end a line.
    #[test]
    fn break_spaces_wraps_space_tokens_down() {
        let fonts = crate::diting_fonts::font_book();
        let toks = tokens_of("aa   bb", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::BreakSpaces);
        let w_aa = toks[0].width;
        let lines = ws_lines("aa   bb", crate::diting_css::WhiteSpace::BreakSpaces, Some(w_aa + 0.5));
        assert_eq!(lines[0].token_idx, vec![0], "aa alone — the first space already overflows");
        let spaced: Vec<usize> = lines[1..lines.len() - 1].iter().flat_map(|l| l.token_idx.iter().copied()).collect();
        assert_eq!(spaced, vec![1, 2, 3], "the three spaces wrap down together");
        assert_eq!(lines.last().unwrap().token_idx, vec![4], "bb on the last line");
    }

    /// `pre-line`: spaces collapse within each newline-separated segment and
    /// newlines are hard breaks (the tokenizer keeps a trailing break; the
    /// breaker is what drops its line box).
    #[test]
    fn pre_line_collapses_spaces_but_breaks_on_newlines() {
        let fonts = crate::diting_fonts::font_book();
        let toks = tokens_of("  a  b  \n c\nd  \n", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::PreLine);
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["a", " ", "b", "\n", "c", "\n", "d", "\n"], "collapsed segments, breaks between, trailing break kept for the breaker");
        let lines = ws_lines("  a  b  \n c\nd  \n", crate::diting_css::WhiteSpace::PreLine, None);
        let line_texts: Vec<Vec<&str>> = lines
            .iter()
            .map(|l| l.token_idx.iter().map(|&i| texts[i]).collect())
            .collect();
        assert_eq!(line_texts, vec![vec!["a", " ", "b"], vec!["c"], vec!["d"]], "trailing newline opens no empty line");
    }

    /// A trailing newline in any preserved mode ends the last line instead
    /// of adding a phantom empty one; a doubled trailing newline still keeps
    /// exactly one blank line.
    #[test]
    fn trailing_newline_drops_not_doubles() {
        assert_eq!(ws_lines("a\n", crate::diting_css::WhiteSpace::Pre, None).len(), 1);
        assert_eq!(ws_lines("a\n\n", crate::diting_css::WhiteSpace::Pre, None).len(), 2);
        assert_eq!(ws_lines("a\n", crate::diting_css::WhiteSpace::BreakSpaces, None).len(), 1);
        // Un-touched content (no break) never pops its only line.
        assert_eq!(ws_lines("", crate::diting_css::WhiteSpace::Pre, None).len(), 1);
    }
}
