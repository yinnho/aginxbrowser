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
    /// Active clip stack — exclusive right/bottom, in canvas px. A plain
    /// rect clips by bounds; a rounded entry (batch 7d) additionally cuts
    /// its corner zones with per-corner elliptical radii, so overflow
    /// content inside a `border-radius` + non-visible overflow box follows
    /// the curve like upstream's padding_box_path BezPath clip. An empty
    /// intersection is the degenerate (0,0,0,0): it clips everything.
    clip: Vec<ClipShape>,
}

/// One clip stack entry: an axis-aligned rect, optionally with per-corner
/// radii (CSS order TL TR BR BL) resolved to px.
#[derive(Clone, Copy, Debug)]
enum ClipShape {
    Rect(i64, i64, i64, i64),
    Rounded {
        x0: i64,
        y0: i64,
        x1: i64,
        y1: i64,
        radii: [(f32, f32); 4],
    },
}

impl ClipShape {
    fn bounds(&self) -> (i64, i64, i64, i64) {
        match *self {
            ClipShape::Rect(x0, y0, x1, y1)
            | ClipShape::Rounded { x0, y0, x1, y1, .. } => (x0, y0, x1, y1),
        }
    }

    /// Whether pixel center `(cx, cy)` passes this clip shape.
    fn accepts(&self, cx: f64, cy: f64) -> bool {
        match *self {
            ClipShape::Rect(x0, y0, x1, y1) => cx >= x0 as f64 && cx < x1 as f64
                && cy >= y0 as f64 && cy < y1 as f64,
            ClipShape::Rounded { x0, y0, x1, y1, radii } => {
                if !(cx >= x0 as f64 && cx < x1 as f64 && cy >= y0 as f64 && cy < y1 as f64) {
                    return false;
                }
                let clamp = |r: (f32, f32), w: i64, h: i64| {
                    (
                        r.0.clamp(0.0, w as f32 / 2.0),
                        r.1.clamp(0.0, h as f32 / 2.0),
                    )
                };
                let (rx0, ry0) = clamp(radii[0], x1 - x0, y1 - y0);
                let (rx1, ry1) = clamp(radii[1], x1 - x0, y1 - y0);
                let (rx2, ry2) = clamp(radii[2], x1 - x0, y1 - y0);
                let (rx3, ry3) = clamp(radii[3], x1 - x0, y1 - y0);
                let centers = [
                    (x0 as f64 + rx0 as f64, y0 as f64 + ry0 as f64),
                    (x1 as f64 - rx1 as f64, y0 as f64 + ry1 as f64),
                    (x1 as f64 - rx2 as f64, y1 as f64 - ry2 as f64),
                    (x0 as f64 + rx3 as f64, y1 as f64 - ry3 as f64),
                ];
                let zone = if cx < centers[0].0 && cy < centers[0].1 {
                    Some(0usize)
                } else if cx > centers[1].0 && cy < centers[1].1 {
                    Some(1)
                } else if cx > centers[2].0 && cy > centers[2].1 {
                    Some(2)
                } else if cx < centers[3].0 && cy > centers[3].1 {
                    Some(3)
                } else {
                    None
                };
                match zone {
                    None => true,
                    Some(i) => {
                        let rxs = [rx0, rx1, rx2, rx3];
                        let rys = [ry0, ry1, ry2, ry3];
                        if rxs[i] <= 0.0 || rys[i] <= 0.0 {
                            true
                        } else {
                            let dx = (cx - centers[i].0) / f64::from(rxs[i]);
                            let dy = (cy - centers[i].1) / f64::from(rys[i]);
                            dx * dx + dy * dy <= 1.0
                        }
                    }
                }
            }
        }
    }
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
    /// canvas bounds intersected with the BOUNDS of every active clip
    /// (rounded clips additionally cut pixels per-shape at draw time via
    /// [`Canvas::clip_accepts`]).
    fn allowed(&self) -> (i64, i64, i64, i64) {
        let mut r = (0, 0, self.width as i64, self.height as i64);
        for c in &self.clip {
            let b = c.bounds();
            r = (r.0.max(b.0), r.1.max(b.1), r.2.min(b.2), r.3.min(b.3));
        }
        if r.2 < r.0 || r.3 < r.1 {
            (0, 0, 0, 0)
        } else {
            r
        }
    }

    /// Whether pixel center `(cx, cy)` passes every active clip.
    fn clip_accepts(&self, cx: f64, cy: f64) -> bool {
        self.clip.iter().all(|c| c.accepts(cx, cy))
    }

    /// Pop the innermost clip.
    pub(crate) fn pop_clip(&mut self) {
        self.clip.pop();
    }

    /// Push a clip rect (intersected with the current one).
    pub(crate) fn push_clip(&mut self, x0: i64, y0: i64, x1: i64, y1: i64) {
        let (cx0, cy0, cx1, cy1) = self.allowed();
        let r = (cx0.max(x0), cy0.max(y0), cx1.min(x1), cy1.min(y1));
        self.clip.push(if r.2 < r.0 || r.3 < r.1 {
            ClipShape::Rect(0, 0, 0, 0)
        } else {
            ClipShape::Rect(r.0, r.1, r.2, r.3)
        });
    }

    /// Push a rounded clip: bounds intersect like a rect; the per-corner
    /// radii cut the corner zones at draw time (batch 7d).
    fn push_rounded_clip(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, radii: [(f32, f32); 4]) {
        let (cx0, cy0, cx1, cy1) = self.allowed();
        let r = (cx0.max(x0), cy0.max(y0), cx1.min(x1), cy1.min(y1));
        self.clip.push(if r.2 < r.0 || r.3 < r.1 {
            ClipShape::Rect(0, 0, 0, 0)
        } else {
            ClipShape::Rounded { x0: r.0, y0: r.1, x1: r.2, y1: r.3, radii }
        });
    }

    /// Source-over fill of an axis-aligned integer rect, clipped to the
    /// canvas and the clip stack. Opaque colors overwrite, matching how
    /// vello_cpu paints a solid blitz background over whatever is beneath.
    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: [u8; 4]) {
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in (y.max(0)).max(ay0)..(y + h).min(self.height as i64).min(ay1) {
            for gx in (x.max(0)).max(ax0)..(x + w).min(self.width as i64).min(ax1) {
                if !self.clip_accepts(gx as f64 + 0.5, gy as f64 + 0.5) {
                    continue;
                }
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
    /// per-pixel). `alpha` (animation batch A) scales the source alpha —
    /// raster pixels can't fold a group opacity any other way.
    pub fn blit_image(
        &mut self,
        image: &super::image::DecodedImage,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        alpha: f32,
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
                if !self.clip_accepts(tx as f64 + 0.5, ty as f64 + 0.5) {
                    continue;
                }
                let sx = (((gx as f64 + 0.5) * sw as f64 / w as f64) as i64).min(sw - 1);
                let i = ((sy * sw + sx) * 4) as usize;
                let a = if alpha >= 1.0 {
                    src[i + 3]
                } else {
                    (src[i + 3] as f32 * alpha).round() as u8
                };
                let src_px = [src[i], src[i + 1], src[i + 2], a];
                let d = (ty as usize * self.width + tx as usize) * 4;
                over(&mut self.data[d..d + 4], src_px);
            }
        }
    }

    /// Source-over fill of a rectangle with per-corner elliptical radii
    /// (batch 7c), clipped to the canvas and the clip stack. Radii are in
    /// CSS corner order (top-left, top-right, bottom-right, bottom-left),
    /// each `(rx, ry)` in px — rx already resolved against the box width
    /// and ry against its height. Each corner radius clamps to half its
    /// own box dimension (the CSS scale-down rule; upstream blitz skips it,
    /// so cross-check cases stay within the legal range). Hard-edged
    /// rasterization: vello antialiases the arc and we don't — cross-checks
    /// sample well inside/outside, never on the curve.
    pub fn fill_corner_rect(
        &mut self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        radii: [(f32, f32); 4],
        color: [u8; 4],
    ) {
        if w <= 0 || h <= 0 {
            return;
        }
        let uniform = |r: (f32, f32)| r.0 == radii[0].0 && r.1 == radii[0].1;
        let degenerate = |r: (f32, f32)| r.0 <= 0.0 || r.1 <= 0.0;
        if radii.iter().all(|&r| degenerate(r)) {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        // A fully uniform circular radius takes the batch-6b fast path.
        if radii.iter().all(|&r| uniform(r)) && !degenerate(radii[0]) {
            self.fill_rounded_rect(x, y, w, h, radii[0].0, color);
            return;
        }
        let (ax0, ay0, ax1, ay1) = self.allowed();
        let (fx, fy) = (x as f64, y as f64);
        let (fw, fh) = (w as f64, h as f64);
        // Clamp per-corner: rx ≤ half width, ry ≤ half height.
        let clamp_r = |r: (f32, f32)| {
            (
                r.0.clamp(0.0, w as f32 / 2.0) as f64,
                r.1.clamp(0.0, h as f32 / 2.0) as f64,
            )
        };
        // Corner circle/ellipse centers.
        let centers: [(f64, f64); 4] = [
            (fx + clamp_r(radii[0]).0, fy + clamp_r(radii[0]).1),
            (fx + fw - clamp_r(radii[1]).0, fy + clamp_r(radii[1]).1),
            (fx + fw - clamp_r(radii[2]).0, fy + fh - clamp_r(radii[2]).1),
            (fx + clamp_r(radii[3]).0, fy + fh - clamp_r(radii[3]).1),
        ];
        for gy in y.max(0).max(ay0)..(y + h).min(self.height as i64).min(ay1) {
            for gx in x.max(0).max(ax0)..(x + w).min(self.width as i64).min(ax1) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                // Which corner zone does the pixel sit in (edge band × edge
                // band)? The cross-shaped middle is always inside; a corner
                // zone tests the (dx/rx)² + (dy/ry)² ≤ 1 ellipse equation.
                let zone = if cx < centers[0].0 && cy < centers[0].1 {
                    Some(0usize)
                } else if cx > centers[1].0 && cy < centers[1].1 {
                    Some(1)
                } else if cx > centers[2].0 && cy > centers[2].1 {
                    Some(2)
                } else if cx < centers[3].0 && cy > centers[3].1 {
                    Some(3)
                } else {
                    None
                };
                let inside = match zone {
                    None => true,
                    Some(i) => {
                        let (rx, ry) = clamp_r(radii[i]);
                        if rx <= 0.0 || ry <= 0.0 {
                            true
                        } else {
                            let (ccx, ccy) = centers[i];
                            let dx = (cx - ccx) / rx;
                            let dy = (cy - ccy) / ry;
                            dx * dx + dy * dy <= 1.0
                        }
                    }
                };
                if !inside {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Source-over fill of a rounded rectangle (batch 6b), clipped to the
    /// canvas and the clip stack. `radius` is the uniform circular corner
    /// radius in px (clamped to half the shorter side — the CSS scale-down
    /// rule; upstream blitz skips it and distorts past half, so cross-check
    /// cases stay within the legal range). Hard-edged rasterization: the
    /// corner boundary is a step function, while vello antialiases it —
    /// cross-checks sample well inside/outside, never on the arc.
    pub fn fill_rounded_rect(
        &mut self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        radius: f32,
        color: [u8; 4],
    ) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = radius
            .clamp(0.0, (w.min(h) as f32) / 2.0);
        if r <= 0.0 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        let (ax0, ay0, ax1, ay1) = self.allowed();
        // Pixel centers (px+0.5) test against the quarter-circle arcs.
        let (fx, fy) = (x as f64, y as f64);
        let (fw, fh) = (w as f64, h as f64);
        let fr = r as f64;
        for gy in y.max(0).max(ay0)..(y + h).min(self.height as i64).min(ay1) {
            for gx in x.max(0).max(ax0)..(x + w).min(self.width as i64).min(ax1) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                // A pixel is outside only when it sits in a corner zone
                // (edge band × edge band) beyond its quarter-circle arc;
                // the cross-shaped middle zone is always inside.
                let (x_left, x_right) = (cx < fx + fr, cx >= fx + fw - fr);
                let (y_top, y_bottom) = (cy < fy + fr, cy >= fy + fh - fr);
                let corner = if x_left && y_top {
                    Some((fx + fr, fy + fr))
                } else if x_right && y_top {
                    Some((fx + fw - fr, fy + fr))
                } else if x_right && y_bottom {
                    Some((fx + fw - fr, fy + fh - fr))
                } else if x_left && y_bottom {
                    Some((fx + fr, fy + fh - fr))
                } else {
                    None
                };
                let inside = match corner {
                    Some((ccx, ccy)) => {
                        let (dx, dy) = (cx - ccx, cy - ccy);
                        dx * dx + dy * dy <= fr * fr
                    }
                    None => true,
                };
                if !inside {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
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
                if !self.clip_accepts(tx as f64 + 0.5, ty as f64 + 0.5) {
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
    execute_band(items, fonts, out, 0.0, 0.0);
}

/// Scale a straight-alpha color's alpha channel (animation batch A): the
/// pipeline composites straight-alpha source-over, so a group opacity folds
/// into item colors directly.
fn alpha_color(c: [u8; 4], a: f32) -> [u8; 4] {
    if a >= 1.0 {
        c
    } else {
        [c[0], c[1], c[2], (c[3] as f32 * a).round() as u8]
    }
}

/// Replay the paint items shifted by `(-dx, -dy)` — the viewport-band paint:
/// with `dy` at the band's page-space top, only the band's rows land on the
/// canvas and everything else falls outside the bounds (every primitive's
/// pixel loop intersects with `allowed()`, so off-band rects/borders/images
/// cost an empty range). `Text`/`Replaced` rasterize at paint time, so they
/// get a cheap bounds estimate first — a skip can only ever drop ink that
/// the estimate put outside the band, and the estimate errs high.
pub fn execute_band(items: &[PaintItem], fonts: &FontBook, out: &mut Canvas, dx: f32, dy: f32) {
    // Rough advance width: CJK/fullwidth ≈ 1em, everything else ≈ 0.6em.
    fn est_width(text: &str, font_size: f32) -> f32 {
        text.chars()
            .map(|c| if c > '\u{2E80}' { 1.0 } else { 0.6 })
            .sum::<f32>()
            * font_size
    }
    // Whether a text tile's ink can reach the band. Line count comes from
    // the same wrap model rasterize_wrapped uses; `top` can lift ink above
    // the line-box top by up to a line's leading, so the top edge gets a
    // full line of slack.
    fn text_reaches_band(
        y: f32,
        text: &str,
        font_size: f32,
        wrap_at: f32,
        line_height: f32,
        dy: f32,
        band_h: i64,
    ) -> bool {
        let wrap = wrap_at.max(1.0);
        let lines = (est_width(text, font_size) / wrap).ceil().max(1.0);
        let top = y - line_height;
        let bottom = y + lines * line_height;
        bottom > dy && top < dy + band_h as f32
    }

    for item in items {
        match item {
            PaintItem::Clip { rect } => out.push_clip(
                (rect.x - dx).round() as i64,
                (rect.y - dy).round() as i64,
                (rect.x + rect.width - dx).round() as i64,
                (rect.y + rect.height - dy).round() as i64,
            ),
            PaintItem::ClipRounded { rect, radii } => out.push_rounded_clip(
                (rect.x - dx).round() as i64,
                (rect.y - dy).round() as i64,
                (rect.x + rect.width - dx).round() as i64,
                (rect.y + rect.height - dy).round() as i64,
                *radii,
            ),
            PaintItem::PopClip => {
                out.pop_clip();
            }
            PaintItem::BgCorner { rect, color, radii, .. } => out.fill_corner_rect(
                (rect.x - dx).round() as i64,
                (rect.y - dy).round() as i64,
                rect.width.round() as i64,
                rect.height.round() as i64,
                *radii,
                *color,
            ),
            PaintItem::Bg { rect, color, radius, .. } => out.fill_rounded_rect(
                (rect.x - dx).round() as i64,
                (rect.y - dy).round() as i64,
                rect.width.round() as i64,
                rect.height.round() as i64,
                *radius,
                *color,
            ),
            PaintItem::Border { rect, widths, color, .. } => {
                // Four bands, square corners: top/bottom span the full
                // border-box width (they own the corners), left/right inset
                // by the top/bottom widths — the classic rectangular-border
                // paint browsers produce with radius 0.
                let [t, r, b, l] = *widths;
                let (x, y) = (
                    (rect.x - dx).round() as i64,
                    (rect.y - dy).round() as i64,
                );
                let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                out.fill_rect(x, y, w, t as i64, *color);
                out.fill_rect(x, y + h - b as i64, w, b as i64, *color);
                out.fill_rect(x, y + t as i64, l as i64, h - t as i64 - b as i64, *color);
                out.fill_rect(x + w - r as i64, y + t as i64, r as i64, h - t as i64 - b as i64, *color);
            }
            PaintItem::Image { rect, paint_rect, image, alpha } => {
                // Replaced content is always clipped to the element box
                // (upstream clips image elements regardless of overflow);
                // object-fit cover/object-position can push paint_rect past
                // `rect`, so clip the blit to the box.
                out.push_clip(
                    (rect.x - dx).round() as i64,
                    (rect.y - dy).round() as i64,
                    (rect.x + rect.width - dx).round() as i64,
                    (rect.y + rect.height - dy).round() as i64,
                );
                out.blit_image(
                    image,
                    (paint_rect.x - dx).round() as i64,
                    (paint_rect.y - dy).round() as i64,
                    paint_rect.width.round() as i64,
                    paint_rect.height.round() as i64,
                    *alpha,
                );
                out.pop_clip();
            }
            PaintItem::Replaced { rect, alt, fill_placeholder, alpha } => {
                if rect.y + rect.height <= dy || rect.y >= dy + out.height as f32 {
                    continue;
                }
                let (x, y) = (
                    (rect.x - dx).round() as i64,
                    (rect.y - dy).round() as i64,
                );
                let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                if *fill_placeholder && w > 0 && h > 0 {
                    out.fill_rect(x, y, w, h, alpha_color([224, 224, 224, 255], *alpha));
                }
                if let Some((text, font_size, bold, line_height, color)) = alt {
                    if !text.trim().is_empty() {
                        // The alt run wraps at the box width; the tile's
                        // `top` offsets ink above the box top exactly like
                        // any other text tile (cramped-CJK leading). The
                        // ink clips to the box (batch 6e): a broken-image
                        // alt that wraps past the bottom is cut there,
                        // like every browser.
                        out.push_clip(x, y, x + w, y + h);
                        let r = fonts.rasterize_wrapped(
                            text,
                            *font_size,
                            *bold,
                            alpha_color(*color, *alpha),
                            w.max(0) as f32,
                            *line_height,
                        );
                        out.blit_text(&r, x, (y as f32 + r.top).round() as i64);
                        out.pop_clip();
                    }
                }
            }
            PaintItem::Svg { rect, render, alpha } => {
                // Same band prefilter as Replaced: the svg painter clips to
                // the element box anyway, this just skips rasterizing an
                // off-band subtree.
                if rect.y + rect.height <= dy || rect.y >= dy + out.height as f32 {
                    continue;
                }
                super::svg::paint_svg(render, rect, fonts, out, dx, dy, *alpha);
            }
            PaintItem::Text { text, font_size, bold, color, line_height, x, y, wrap_at } => {
                if !text_reaches_band(*y, text, *font_size, *wrap_at, *line_height, dy, out.height as i64) {
                    continue;
                }
                let r = fonts.rasterize_wrapped(text, *font_size, *bold, *color, *wrap_at, *line_height);
                // Tile row 0 sits `top` px above the leaf's line-box top.
                out.blit_text(&r, (x - dx).round() as i64, (y - dy + r.top).round() as i64);
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

    /// Band painting with no shift is pixel-identical to plain execute —
    /// the viewport path's dy=0 degenerate case must not perturb the
    /// established renderer.
    #[test]
    fn band_dy_zero_matches_execute() {
        use super::super::image::DecodedImage;
        let items = vec![
            PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 60.0 }, color: [30, 60, 90, 255], radius: 0.0 },
            PaintItem::Border { rect: super::super::Rect { x: 5.0, y: 5.0, width: 30.0, height: 20.0 }, widths: [2.0, 3.0, 4.0, 1.0], color: [200, 40, 40, 255] },
            PaintItem::Clip { rect: super::super::Rect { x: 8.0, y: 8.0, width: 24.0, height: 14.0 } },
            PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 60.0 }, color: [240, 240, 0, 255], radius: 0.0 },
            PaintItem::PopClip,
            PaintItem::Image {
                rect: super::super::Rect { x: 10.0, y: 30.0, width: 8.0, height: 8.0 },
                paint_rect: super::super::Rect { x: 10.0, y: 30.0, width: 8.0, height: 8.0 },
                image: DecodedImage::new(2, 2, vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]),
                alpha: 1.0,
            },
            PaintItem::Replaced {
                rect: super::super::Rect { x: 25.0, y: 40.0, width: 10.0, height: 12.0 },
                alt: Some(("alt".into(), 16.0, false, 20.0, [0, 0, 0, 255])),
                fill_placeholder: true,
                alpha: 1.0,
            },
            PaintItem::Text { text: "hello".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 4.0, wrap_at: 36.0 },
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut full = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut full);
        let mut band = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
        execute_band(&items, &fonts, &mut band, 0.0, 0.0);
        assert_eq!(full.data, band.data, "dy=0 band paint equals execute");
    }

    /// A band at dy=100 reproduces exactly rows [100, 180) of the full
    /// render — the viewport-frame contract AginxOS's screencast builds on.
    #[test]
    fn band_capture_equals_window_of_full_render() {
        let items = vec![
            PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 300.0 }, color: [30, 60, 90, 255], radius: 0.0 },
            PaintItem::Bg { rect: super::super::Rect { x: 4.0, y: 120.0, width: 32.0, height: 40.0 }, color: [200, 40, 40, 255], radius: 0.0 },
            PaintItem::Border { rect: super::super::Rect { x: 6.0, y: 240.0, width: 28.0, height: 30.0 }, widths: [3.0, 3.0, 3.0, 3.0], color: [0, 200, 0, 255] },
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut full = Canvas::new_filled(40, 300, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut full);
        let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
        execute_band(&items, &fonts, &mut band, 0.0, 100.0);
        for y in 0..80 {
            for x in 0..40 {
                let f = &full.data[((y + 100) * 40 + x) * 4..((y + 100) * 40 + x) * 4 + 4];
                let b = &band.data[(y * 40 + x) * 4..(y * 40 + x) * 4 + 4];
                assert_eq!(f, b, "band row {y} must equal full row {}", y + 100);
            }
        }
    }

    /// A clip opened above the band and closed inside it stays paired and
    /// still cuts: the clip rect translates with the band, so content
    /// beyond the clip's page-space edge stays out even though the clip's
    /// own bounds are far above the canvas.
    #[test]
    fn clip_spanning_band_stays_paired_and_cuts() {
        let items = vec![
            PaintItem::Clip { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 130.0 } },
            PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 50.0, width: 40.0, height: 100.0 }, color: [0, 200, 0, 255], radius: 0.0 },
            PaintItem::PopClip,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
        execute_band(&items, &fonts, &mut band, 0.0, 100.0);
        // Clip page [0,130) → band [-100,30): green bg page [50,150) → band
        // [-50,50), clipped to rows [0,30).
        assert_eq!(px(&band, 20, 29), [0, 200, 0, 255], "clipped green inside the band");
        assert_eq!(px(&band, 20, 30), [255, 255, 255, 255], "clip's page-space edge still cuts");
        // The stack must have drained: a later fill fills the whole canvas.
        band.fill_rect(0, 0, 40, 80, [0, 0, 255, 255]);
        assert_eq!(px(&band, 0, 0), [0, 0, 255, 255], "clip popped, stack drained");
    }

    /// Text whose line box straddles the band's top edge keeps its ink in
    /// the band (the estimate's one-line top slack), and text far below is
    /// skipped without polluting the canvas.
    #[test]
    fn text_band_edges() {
        let fonts = crate::diting_fonts::font_book();
        // A tall low-content page: only two text leaves, one near the band.
        let items = vec![
            PaintItem::Text { text: "edge".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 96.0, wrap_at: 36.0 },
            PaintItem::Text { text: "far".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 500.0, wrap_at: 36.0 },
        ];
        let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
        execute_band(&items, &fonts, &mut band, 0.0, 100.0);
        let ink = band.data.chunks_exact(4).any(|p| p[0] < 128);
        assert!(ink, "straddling text must paint into the band");
        // The far text must not have painted anything (only "edge" ink).
        let dark_rows: Vec<usize> = (0..80)
            .filter(|&y| (0..40).any(|x| band.data[(y * 40 + x) * 4] < 128))
            .collect();
        assert!(dark_rows.iter().all(|&y| y < 25), "no ink from the far-below text: {dark_rows:?}");
    }
}
