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
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith};
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
pub enum PdfFace {
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
pub struct PdfGlyph {
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
    /// Han-slot classification of the faces (#139): the primary pair's and,
    /// parallel to `fallbacks`, each tail's. Pure functions of the bytes, so
    /// they ride construction like the fingerprint. `None` = the face claims
    /// no slot and never outranks the plain per-char cascade.
    primary_slot: Option<han::HanSlot>,
    fallback_slots: Vec<Option<han::HanSlot>>,
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
    /// `line-height: normal` ratios (ascent+descent+line_gap at size 1) for
    /// [regular, bold], keyed by book fingerprint — the same identity
    /// contract as the raster cache (#16).
    static NORMAL_LH_RATIOS: RefCell<HashMap<u64, [f32; 2]>> = RefCell::new(HashMap::new());
}

/// Glyphs whose swash raster panicked and was absorbed by
/// [`render_guarded`] — observability for the strike-decode panic family
/// (dfrg/swash#139: a width-0 EBDT bitmap makes `chunks(0)` panic in
/// release too; #123-126 are the debug-only arithmetic siblings). The
/// zero-width-strike test flips this to prove the guard actually ran.
static SWASH_PANIC_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Rasterize one glyph behind a panic guard. Malformed embedded bitmap
/// tables (the SimSun blank-glyph shape: an EBLC/EBDT strike whose small
/// metrics declare width 0) panic inside swash's `Bitmap::decode` — and
/// the panic fires at the `Source::Bitmap` step of the fallback source
/// list, aborting the whole line raster before the outline tail can run.
/// Catching it here lets the caller retry the same glyph through a pure
/// outline `Render`, the one source set that cannot touch the strike
/// decoder. Blank glyphs raster to no ink either way, so the retry is
/// pixel-identical for the shapes that actually trigger this.
fn render_guarded(render: &Render, scaler: &mut Scaler, gid: GlyphId) -> Option<Image> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render.render(scaler, gid)
    })) {
        Ok(img) => img,
        Err(_) => {
            SWASH_PANIC_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

impl FontBook {
    /// Load a book from TTF/OTF bytes. Returns `None` if either face fails
    /// to parse (truncated file, wrong magic, …).
    pub fn from_pairs(regular: Vec<u8>, bold: Vec<u8>) -> Option<Self> {
        if FontRef::from_index(&regular, 0).is_none() || FontRef::from_index(&bold, 0).is_none() {
            return None;
        }
        let fingerprint = face_fingerprint(&regular, &bold, None, &[]);
        let primary_slot = han::face_han_slot(&regular);
        Some(Self {
            regular,
            bold,
            mono: None,
            fallbacks: Vec::new(),
            primary_slot,
            fallback_slots: Vec::new(),
            fingerprint,
        })
    }

    /// Append single-weight fallback faces (emoji batch): unparseable bytes
    /// drop silently, parseable ones join the tail in order. A char the
    /// primary pair doesn't map resolves through the first fallback that
    /// covers it; measure and paint segment identically, so a mixed
    /// "text 🚀 text" run advances the same bytes it rasterizes.
    pub fn with_fallbacks(mut self, faces: Vec<Vec<u8>>) -> Self {
        let kept: Vec<Vec<u8>> = faces
            .into_iter()
            .filter(|b| FontRef::from_index(b, 0).is_some())
            .collect();
        // Slots stay parallel to the faces (#139) — classify exactly the
        // bytes that joined.
        self.fallback_slots.extend(kept.iter().map(|b| han::face_han_slot(b)));
        self.fallbacks.extend(kept);
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
        han: Option<han::HanSlot>,
        width: usize,
        height: usize,
    ) -> Option<Vec<u8>> {
        if !self.has_fallbacks() || width == 0 || height == 0 {
            return None;
        }
        let uses_fallback = self
            .segments(text, bold, mono, han)
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
    ///
    /// #139 (Han unification): with `han`, unified ideographs route to the
    /// first same-slot face that covers them BEFORE the plain cascade — the
    /// primary pair counts as the SC slot it is. No slot preference, no
    /// matching face, or a non-ideograph char: the order below is exactly
    /// the pre-#139 cascade, so zh pages and lang-less runs are unchanged
    /// glyph-for-glyph.
    fn segments<'a>(
        &self,
        text: &'a str,
        bold: bool,
        mono: bool,
        han: Option<han::HanSlot>,
    ) -> Vec<(FaceSel, &'a str)> {
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
            if let Some(slot) = han {
                if han::is_han_ideograph(ch) {
                    if self.primary_slot == Some(slot) && covers(primary, ch) {
                        return FaceSel::Primary;
                    }
                    for (i, f) in fallbacks.iter().enumerate() {
                        if self.fallback_slots.get(i) == Some(&Some(slot)) && covers(*f, ch) {
                            return FaceSel::Fallback(i);
                        }
                    }
                    // No same-slot face covers this ideograph: fall through
                    // to the plain cascade (a ja page without a ja face
                    // renders SC glyphs — today's behavior).
                }
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
        let covers_sel = |sel: FaceSel, ch: char| -> bool {
            match sel {
                FaceSel::Primary => covers(primary, ch),
                FaceSel::Mono => covers(mono_face, ch),
                FaceSel::Fallback(i) => covers(fallbacks[i], ch),
            }
        };
        for (i, ch) in text.char_indices() {
            // A variation selector (U+FE00..=U+FE0F) is default-ignorable:
            // it only modifies the presentation of the base before it. It
            // rides that base's face when the face maps it — a covered VS
            // is conventionally zero-width and can drive GSUB presentation
            // choice (Mongolian FVS). Otherwise it is dropped: picked on
            // its own it fell to another face's .notdef and measured a
            // full em of phantom advance (archify #91, the engine twin),
            // and even riding, an unmapped selector shapes as .notdef.
            if matches!(ch, '\u{FE00}'..='\u{FE0F}') {
                if let Some(sel) = cur {
                    if covers_sel(sel, ch) {
                        continue;
                    }
                    out.push((sel, &text[start..i]));
                }
                start = i + ch.len_utf8();
                cur = None;
                continue;
            }
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
    /// `han` is the run's Han slot from its lang (#139) — `None` shapes the
    /// plain cascade.
    pub fn advance_width(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        mono: bool,
        han: Option<han::HanSlot>,
    ) -> f32 {
        // #120: font-size 0 is the whitespace-killing idiom (Fusion sets it
        // on `.next-input` and its icon `<i>`s inherit) and must measure
        // nothing. rustybuzz treats size 0 as "unset" and shapes at the raw
        // upem scale instead, so a single 'x' came back ~500 px and PUA
        // icon glyphs ~1000+ — every select arrow on the Tmall publish page
        // blew its 1px table-cell trick out to 2178 px. Same guard in
        // `pdf_shape` and `blit_line`.
        if font_size <= 0.0 {
            return 0.0;
        }
        let mut total = 0.0f32;
        for (sel, seg) in self.segments(text, bold, mono, han) {
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
        han: Option<han::HanSlot>,
    ) -> Option<Vec<PdfGlyph>> {
        // #120 companion guard: zero-size text embeds no glyphs.
        if font_size <= 0.0 {
            return Some(Vec::new());
        }
        let mut out: Vec<PdfGlyph> = Vec::new();
        let mut pen = 0.0f32;
        for (sel, seg) in self.segments(text, bold, mono, han) {
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
    pub fn pdf_face_metrics(&self, face: PdfFace) -> Option<(f32, f32, f32)> {
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
    pub fn pdf_face_bytes(&self, face: PdfFace) -> &[u8] {
        match face {
            PdfFace::Regular => &self.regular,
            PdfFace::Bold => &self.bold,
            PdfFace::Mono => self.mono.as_deref().unwrap_or(&self.regular),
        }
    }

    /// Vertical metrics of the face, normalized to px at `font_size` — for
    /// the paint batch (baseline placement, 3b). Layout line height does NOT
    /// hard-pin CSS `normal` to `font_size * 1.2` anymore: it derives from
    /// these metrics (see [`FontBook::normal_line_height`], #16/blitz#878).
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

    /// Used line height for `line-height: normal`: the face's own vertical
    /// extent (ascent + descent + line gap) like a real browser, not a flat
    /// 1.2× (#16, blitz#878). Layout asks once per text leaf, so the ratio
    /// is memoized per book fingerprint — the same book-identity contract
    /// as the raster cache. Falls back to the legacy 1.2 when the face
    /// won't parse.
    pub fn normal_line_height(&self, font_size: f32, bold: bool) -> f32 {
        let fp = self.fingerprint;
        let ratios = NORMAL_LH_RATIOS.with(|memo| {
            if let Some(r) = memo.borrow().get(&fp) {
                return *r;
            }
            let r = [
                self.metrics(1.0, false)
                    .map(|m| m.ascent + m.descent + m.line_gap)
                    .unwrap_or(1.2),
                self.metrics(1.0, true)
                    .map(|m| m.ascent + m.descent + m.line_gap)
                    .unwrap_or(1.2),
            ];
            memo.borrow_mut().insert(fp, r);
            r
        });
        font_size * ratios[usize::from(bold)]
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
        han: Option<han::HanSlot>,
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
            small_caps: false, // single-line path renders verbatim (no caps synthesis)
            han,
        };
        RasterCache::get_or_insert(key, || {
            self.rasterize_line_uncached(text, font_size, bold, color, line_height, mono, han)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn rasterize_line_uncached(
        &self,
        text: &str,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        line_height: f32,
        mono: bool,
        han: Option<han::HanSlot>,
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
        let width = self.advance_width(text, font_size, bold, mono, han).ceil() as usize + 2;

        let mut alpha = vec![0u8; width * height];
        let mut layer = self.color_layer_for(text, bold, mono, han, width, height);
        self.blit_line(
            &mut alpha,
            layer.as_deref_mut(),
            width,
            height,
            text,
            font_size,
            bold,
            mono,
            han,
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
        small_caps: bool,
        han: Option<han::HanSlot>,
    ) -> Arc<TextRaster> {
        self.rasterize_wrapped_with(
            text, font_size, bold, color, wrap_at, line_height, mono, word_spacing, truncate_at,
            ws, None, small_caps, han,
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
        small_caps: bool,
        han: Option<han::HanSlot>,
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
            small_caps,
            han,
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
                small_caps,
                han,
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
        small_caps: bool,
        han: Option<han::HanSlot>,
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
                owned = tokens_of(text, font_size, bold, self, mono, word_spacing, ws, small_caps, han);
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
            truncate_tokens(tokens, limit, font_size, bold, self, mono, word_spacing, ws, han)
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
        let mut layer = self.color_layer_for(&tokens.iter().map(|t| t.text.as_str()).collect::<String>(), bold, mono, han, width, height);
        if word_spacing == 0.0 && tokens.iter().all(|t| t.scale == 1.0) {
            for (line, baseline) in lines.iter().zip(&baselines) {
                if line.token_idx.is_empty() {
                    continue;
                }
                let s: String =
                    line.token_idx.iter().map(|&i| tokens[i].text.as_str()).collect();
                self.blit_line(&mut alpha, layer.as_deref_mut(), width, height, &s, font_size, bold, mono, han, 0.0, baseline - top);
            }
        } else {
            // Word-spacing or small-caps run: blit token by token at the
            // cumulative pen so the widened space advances actually separate
            // the words and reduced-size caps segments shape at their own
            // scale (the whole-line shape above would draw them uniform).
            // Same token model measurement uses — one shape per token.
            for (line, baseline) in lines.iter().zip(&baselines) {
                let mut pen = 0.0f32;
                for &i in &line.token_idx {
                    let t = &tokens[i];
                    if !t.is_space {
                        self.blit_line(&mut alpha, layer.as_deref_mut(), width, height, &t.text, font_size * t.scale, bold, mono, han, pen, baseline - top);
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
        han: Option<han::HanSlot>,
        x0: f32,
        baseline: f32,
    ) {
        // #120 companion guard: zero-size text paints nothing (and without
        // this, the scaler would rasterize upem-sized glyphs).
        if font_size <= 0.0 {
            return;
        }
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
        for (sel, seg) in self.segments(text, bold, mono, han) {
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
                // Outline-only retry behind the panic guard: a malformed
                // embedded strike (zero-width EBDT, dfrg/swash#139) panics
                // `Bitmap::decode` mid-list — before `fallback_sources`'
                // own outline tail — so the retry needs a bare outline
                // `Render` that cannot reach the strike decoder at all.
                let outline_render = Render::new(&mono_sources[..]);
                for (pen_x, dy, gid) in glyphs {
                    // swash rasterizes outlines with zeno Origin::BottomLeft,
                    // so `placement.top` is the image's top edge ABOVE the
                    // pen: blit y = pen_y - top (data rows are ordinary
                    // top-down). Bitmap strikes carry the same contract.
                    let Some(img) = render_guarded(&render, &mut scaler, gid)
                        .or_else(|| render_guarded(&outline_render, &mut scaler, gid))
                    else { continue };
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

// The token/wrap family lives in `text/wrap.rs` — batch 202 split this
// pure-function block out so the file rides back under the layering
// audit's god-file ratchet cap. Re-exports keep every existing
// `text::Token` / `super::text::greedy_wrap` path working unchanged.
mod wrap;

pub use wrap::{greedy_wrap, tokens_of, Token, WrapLine};
pub(crate) use wrap::{caps_case_runs, truncate_tokens, SMALL_CAPS_RATIO};

// The Han-unification slot model (#139): lang → slot, slot → face routing.
mod han;

pub use han::{face_han_slot, han_slot_for_lang, is_han_ideograph, HanSlot};

// The colocated contract suite (batch 219: moved out with the file riding
// the god-file ratchet cap — the layering audit exempts tests.rs).
#[cfg(test)]
mod tests;

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
    small_caps: bool,
    /// The run's Han slot (#139): it changes which face each ideograph
    /// segments into — same text, same size, different glyphs — so it must
    /// ride the identity like `bold` does. `None` (the lang-less callers)
    /// vs `Some(Simplified)` produce identical pixels today but stay
    /// separate entries: correctness over entry count.
    han: Option<HanSlot>,
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

