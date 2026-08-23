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

use std::cell::RefCell;

use swash::proxy::MetricsProxy;
use swash::shape::ShapeContext;
use swash::FontRef;

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
    pub fn metrics(&self, font_size: f32, bold: bool) -> Option<ScaledMetrics> {
        let bytes = self.face(bold);
        let font = FontRef::from_index(bytes, 0)?;
        let m = MetricsProxy::from_font(&font).materialize_metrics(&font, &[]);
        let scale = font_size / m.units_per_em as f32;
        Some(ScaledMetrics {
            ascent: m.ascent * scale,
            descent: m.descent * scale,
            line_gap: m.leading * scale,
        })
    }
}

/// Font vertical metrics scaled to a given size (all in px; signs as the
/// font reports them — ascent up-positive, descent negative).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaledMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}
