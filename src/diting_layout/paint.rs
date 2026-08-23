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
//! Not in this slice (tracked in docs/engine/render.md §25): border-radius,
//! per-side border colors/styles, network-loaded images (data: PNG only),
//! gradients, z-index/stacking contexts.

use super::text::TextRaster;
use super::{FontBook, PaintItem};

/// A straight-alpha RGBA8 image, row-major — our paint target.
#[derive(Debug)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
    /// Active clip stack (x0, y0, x1, y1) — exclusive right/bottom, in
    /// canvas px. An empty intersection is the degenerate (0,0,0,0): it
    /// clips everything beneath it.
    clip: Vec<(i64, i64, i64, i64)>,
}

impl Canvas {
    /// A canvas pre-filled with an opaque color (the page background stand-in
    /// — this slice models no html/body propagation yet).
    pub fn new_filled(width: usize, height: usize, color: [u8; 4]) -> Self {
        let mut data = vec![0u8; width * height * 4];
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        Self { width, height, data, clip: Vec::new() }
    }

    /// Rows [y0, y1) × cols [x0, x1) the next primitive may touch: the
    /// canvas bounds intersected with every active clip.
    fn allowed(&self) -> (i64, i64, i64, i64) {
        let mut r = (0, 0, self.width as i64, self.height as i64);
        for c in &self.clip {
            r = (r.0.max(c.0), r.1.max(c.1), r.2.min(c.2), r.3.min(c.3));
        }
        if r.2 < r.0 || r.3 < r.1 {
            (0, 0, 0, 0)
        } else {
            r
        }
    }

    /// Pop the innermost clip.
    fn pop_clip(&mut self) {
        self.clip.pop();
    }

    /// Push a clip rect (intersected with the current one).
    fn push_clip(&mut self, x0: i64, y0: i64, x1: i64, y1: i64) {
        let (cx0, cy0, cx1, cy1) = self.allowed();
        let r = (cx0.max(x0), cy0.max(y0), cx1.min(x1), cy1.min(y1));
        self.clip
            .push(if r.2 < r.0 || r.3 < r.1 { (0, 0, 0, 0) } else { r });
    }

    /// Source-over fill of an axis-aligned integer rect, clipped to the
    /// canvas and the clip stack. Opaque colors overwrite, matching how
    /// vello_cpu paints a solid blitz background over whatever is beneath.
    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: [u8; 4]) {
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in (y.max(0)).max(ay0)..(y + h).min(self.height as i64).min(ay1) {
            for gx in (x.max(0)).max(ax0)..(x + w).min(self.width as i64).min(ax1) {
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Nearest-neighbor blit of an RGBA image scaled into the rect
    /// `(x, y, w, h)` (object-fit: fill), source-over per pixel, clipped to
    /// the canvas and the clip stack. 1:1 sizes are exact texel copies;
    /// scaled output is nearest-neighbor (vello samples bilinearly upstream,
    /// so scaled-image cross-checks compare bbox + sampled interior, not
    /// per-pixel).
    pub fn blit_image(
        &mut self,
        image: &super::image::DecodedImage,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
    ) {
        if w <= 0 || h <= 0 || image.width == 0 || image.height == 0 {
            return;
        }
        let (ax0, ay0, ax1, ay1) = self.allowed();
        let src = &image.rgba;
        let (sw, sh) = (image.width as i64, image.height as i64);
        for gy in 0..h {
            let ty = y + gy;
            if ty < ay0 || ty >= ay1 || ty < 0 || ty >= self.height as i64 {
                continue;
            }
            // Sample the texel covering this destination row/column's center.
            let sy = (((gy as f64 + 0.5) * sh as f64 / h as f64) as i64).min(sh - 1);
            for gx in 0..w {
                let tx = x + gx;
                if tx < ax0 || tx >= ax1 || tx < 0 || tx >= self.width as i64 {
                    continue;
                }
                let sx = (((gx as f64 + 0.5) * sw as f64 / w as f64) as i64).min(sw - 1);
                let i = ((sy * sw + sx) * 4) as usize;
                let src_px = [src[i], src[i + 1], src[i + 2], src[i + 3]];
                let d = (ty as usize * self.width + tx as usize) * 4;
                over(&mut self.data[d..d + 4], src_px);
            }
        }
    }

    /// Source-over composite of a text tile's straight-alpha pixels at
    /// integer offset `(x, y)`, clipped to the canvas and the clip stack.
    pub fn blit_text(&mut self, r: &TextRaster, x: i64, y: i64) {
        if r.width == 0 || r.height == 0 {
            return;
        }
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in 0..r.height as i64 {
            let ty = y + gy;
            if ty < ay0 || ty >= ay1 || ty < 0 || ty >= self.height as i64 {
                continue;
            }
            for gx in 0..r.width as i64 {
                let tx = x + gx;
                if tx < ax0 || tx >= ax1 || tx < 0 || tx >= self.width as i64 {
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
            PaintItem::Clip { rect } => out.push_clip(
                rect.x.round() as i64,
                rect.y.round() as i64,
                (rect.x + rect.width).round() as i64,
                (rect.y + rect.height).round() as i64,
            ),
            PaintItem::PopClip => {
                out.pop_clip();
            }
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
            PaintItem::Image { rect, paint_rect, image } => {
                // Replaced content is always clipped to the element box
                // (upstream clips image elements regardless of overflow);
                // object-fit cover/object-position can push paint_rect past
                // `rect`, so clip the blit to the box.
                out.push_clip(
                    rect.x.round() as i64,
                    rect.y.round() as i64,
                    (rect.x + rect.width).round() as i64,
                    (rect.y + rect.height).round() as i64,
                );
                out.blit_image(
                    image,
                    paint_rect.x.round() as i64,
                    paint_rect.y.round() as i64,
                    paint_rect.width.round() as i64,
                    paint_rect.height.round() as i64,
                );
                out.pop_clip();
            }
            PaintItem::Replaced { rect, alt, fill_placeholder } => {
                let (x, y) = (rect.x.round() as i64, rect.y.round() as i64);
                let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                if *fill_placeholder && w > 0 && h > 0 {
                    out.fill_rect(x, y, w, h, [224, 224, 224, 255]);
                }
                if let Some((text, font_size, bold, color)) = alt {
                    if !text.trim().is_empty() {
                        // The alt run wraps at the box width; the tile's
                        // `top` offsets ink above the box top exactly like
                        // any other text tile (cramped-CJK leading).
                        let r = fonts.rasterize_wrapped(
                            text,
                            *font_size,
                            *bold,
                            *color,
                            w.max(0) as f32,
                        );
                        out.blit_text(&r, x, (y as f32 + r.top).round() as i64);
                    }
                }
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

    /// The clip stack constrains fills, nesting intersects, and popping
    /// restores — a degenerate intersection clips everything.
    #[test]
    fn clip_stack_constrains_and_pops() {
        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.push_clip(2, 2, 6, 6);
        c.fill_rect(0, 0, 10, 10, [200, 40, 40, 255]);
        assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "outside clip untouched");
        assert_eq!(px(&c, 5, 5), [200, 40, 40, 255], "inside clip painted");
        assert_eq!(px(&c, 6, 5), [255, 255, 255, 255], "x1 exclusive");

        // Nested clip intersects.
        c.push_clip(4, 4, 8, 8);
        c.fill_rect(0, 0, 10, 10, [0, 0, 0, 255]);
        assert_eq!(px(&c, 3, 5), [200, 40, 40, 255], "inner clip keeps only [4,6)");
        assert_eq!(px(&c, 5, 5), [0, 0, 0, 255]);
        c.pop_clip();
        c.pop_clip();
        c.fill_rect(0, 0, 10, 10, [0, 255, 0, 255]);
        assert_eq!(px(&c, 0, 0), [0, 255, 0, 255], "popped clips restore");

        // Degenerate intersection clips everything beneath.
        c.push_clip(8, 8, 2, 2);
        c.fill_rect(0, 0, 10, 10, [255, 0, 0, 255]);
        assert_eq!(px(&c, 9, 9), [0, 255, 0, 255], "degenerate clip paints nothing");
    }
}
