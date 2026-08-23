//! Minimal raster paint (render claim batch 4a).
//!
//! The diting stack's first pixels: [`execute`] replays the document-order
//! [`PaintItem`] list that [`layout_dom_with_paint`](super::layout_dom_with_paint)
//! produced — solid background fills and wrapped text tiles — onto an RGBA
//! [`Canvas`]. Upstream obscura-render draws the same two primitive kinds
//! through vello_cpu scene encoding; we compare against exactly that in the
//! cross-check tests (background bbox exact, text ink extents and per-line
//! band structure within the batch-3b ±2px ink tolerance).
//!
//! Not in this slice (tracked in docs/engine/render.md §20): border-radius,
//! patterned border styles (dashed/dotted/double paint as solid), per-side
//! border colors/styles, images, gradients, z-index/stacking contexts, and
//! text from mixed inline runs.

use super::text::TextRaster;
use super::{FontBook, PaintItem};

/// A straight-alpha RGBA8 image, row-major — our paint target.
#[derive(Debug)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Canvas {
    /// A canvas pre-filled with an opaque color (the page background stand-in
    /// — this slice models no html/body propagation yet).
    pub fn new_filled(width: usize, height: usize, color: [u8; 4]) -> Self {
        let mut data = vec![0u8; width * height * 4];
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        Self { width, height, data }
    }

    /// Source-over fill of an axis-aligned integer rect, clipped to the
    /// canvas. Opaque colors overwrite, matching how vello_cpu paints a
    /// solid blitz background over whatever is beneath.
    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: [u8; 4]) {
        for gy in y.max(0)..(y + h).min(self.height as i64) {
            for gx in x.max(0)..(x + w).min(self.width as i64) {
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Source-over composite of a text tile's straight-alpha pixels at
    /// integer offset `(x, y)`, clipped.
    pub fn blit_text(&mut self, r: &TextRaster, x: i64, y: i64) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        for gy in 0..r.height as i64 {
            let ty = y + gy;
            if ty < 0 || ty >= self.height as i64 {
                continue;
            }
            for gx in 0..r.width as i64 {
                let tx = x + gx;
                if tx < 0 || tx >= self.width as i64 {
                    continue;
                }
                let a = r.data[(gy as usize * r.width + gx as usize) * 4 + 3];
                if a == 0 {
                    continue;
                }
                let src = [
                    r.data[(gy as usize * r.width + gx as usize) * 4],
                    r.data[(gy as usize * r.width + gx as usize) * 4 + 1],
                    r.data[(gy as usize * r.width + gx as usize) * 4 + 2],
                    a,
                ];
                let i = (ty as usize * self.width + tx as usize) * 4;
                over(&mut self.data[i..i + 4], src);
            }
        }
    }
}

/// Straight-alpha source-over onto one destination pixel.
fn over(dst: &mut [u8], src: [u8; 4]) {
    let a = src[3] as u32;
    if a == 255 {
        dst[..4].copy_from_slice(&src);
        return;
    }
    for c in 0..3 {
        dst[c] = ((src[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
    }
    dst[3] = 255.min(dst[3] as u32 + a * (255 - dst[3] as u32) / 255) as u8;
}

/// Replay the paint items onto `out`. `Bg` rects come from taffy's rounded
/// layout so the fill lands on whole pixels; each `Text` re-rasterizes
/// wrapped at the width its containing block offered at measure time, so
/// the tile's line structure is the measure function's own.
pub fn execute(items: &[PaintItem], fonts: &FontBook, out: &mut Canvas) {
    for item in items {
        match item {
            PaintItem::Bg { rect, color, .. } => out.fill_rect(
                rect.x.round() as i64,
                rect.y.round() as i64,
                rect.width.round() as i64,
                rect.height.round() as i64,
                *color,
            ),
            PaintItem::Border { rect, widths, color, .. } => {
                // Four bands, square corners: top/bottom span the full
                // border-box width (they own the corners), left/right inset
                // by the top/bottom widths — the classic rectangular-border
                // paint browsers produce with radius 0.
                let [t, r, b, l] = *widths;
                let (x, y) = (rect.x.round() as i64, rect.y.round() as i64);
                let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                out.fill_rect(x, y, w, t as i64, *color);
                out.fill_rect(x, y + h - b as i64, w, b as i64, *color);
                out.fill_rect(x, y + t as i64, l as i64, h - t as i64 - b as i64, *color);
                out.fill_rect(x + w - r as i64, y + t as i64, r as i64, h - t as i64 - b as i64, *color);
            }
            PaintItem::Text { text, font_size, bold, color, x, y, wrap_at } => {
                let r = fonts.rasterize_wrapped(text, *font_size, *bold, *color, *wrap_at);
                // Tile row 0 sits `top` px above the leaf's line-box top.
                out.blit_text(&r, x.round() as i64, (*y + r.top).round() as i64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(c: &Canvas, x: usize, y: usize) -> [u8; 4] {
        let i = (y * c.width + x) * 4;
        c.data[i..i + 4].try_into().unwrap()
    }

    /// Fill clips at the canvas edge and an opaque fill overwrites.
    #[test]
    fn fill_rect_clips_and_overwrites() {
        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.fill_rect(8, -2, 4, 4, [200, 40, 40, 255]);
        assert_eq!(px(&c, 9, 0), [200, 40, 40, 255], "clipped fill still paints");
        assert_eq!(px(&c, 7, 0), [255, 255, 255, 255], "outside the rect untouched");
        assert_eq!(px(&c, 0, 9), [255, 255, 255, 255], "below the rect untouched");
    }

    /// Text blits source-over: 50% black over white is mid-gray, and the
    /// alpha ramp composites linearly in premultiplied space.
    #[test]
    fn text_blit_blends_source_over() {
        let mut c = Canvas::new_filled(4, 4, [255, 255, 255, 255]);
        let raster = TextRaster {
            width: 2,
            height: 1,
            baseline: 0.0,
            top: 0.0,
            data: vec![0, 0, 0, 128, 0, 0, 0, 255],
        };
        c.blit_text(&raster, 1, 1);
        assert_eq!(px(&c, 1, 1), [127, 127, 127, 255], "half-alpha black over white");
        assert_eq!(px(&c, 2, 1), [0, 0, 0, 255], "opaque black covers");
        assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "outside untouched");
    }
}
