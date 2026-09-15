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

use super::text::{
    baseline_offset, greedy_wrap, tokens_of, truncate_tokens, PdfGlyph, ScaledMetrics, TextRaster,
    Token,
};
use super::{FontBook, PaintItem, Rect, TextGradient};
use crate::diting_css::{TextDecorations, TextShadow};
use std::collections::HashSet;

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
    /// Open transform-bracket stack (affine batch): entries are TOTAL
    /// local→canvas maps in the same CSS matrix order PaintItem::SetXf
    /// carries, composed at push time — an inner bracket multiplies onto
    /// the current top, an empty stack is the identity. Diagonal transforms
    /// never open a bracket (their geometry pre-bakes at collect time), so
    /// untouched pages keep the exact pre-batch path.
    xf_stack: Vec<[f64; 6]>,
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
    /// A clip whose LOCAL rect maps through an open transform bracket
    /// (affine batch): `inv` is the inverse of the canvas-space total map,
    /// `(x, y, w, h)` + `radii` the local rounded rect, and `(bx0..by1)`
    /// the canvas-space bounds (mapped bbox ∩ prior clips) that
    /// [`Canvas::allowed`] intersects on. A pixel passes when its inverse
    /// image falls inside the local rounded rect — so a rotated
    /// overflow:hidden window cuts child ink along the rotation.
    Affine {
        inv: [f64; 6],
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radii: [(f32, f32); 4],
        bx0: i64,
        by0: i64,
        bx1: i64,
        by1: i64,
    },
}

impl ClipShape {
    fn bounds(&self) -> (i64, i64, i64, i64) {
        match *self {
            ClipShape::Rect(x0, y0, x1, y1)
            | ClipShape::Rounded { x0, y0, x1, y1, .. } => (x0, y0, x1, y1),
            ClipShape::Affine { bx0, by0, bx1, by1, .. } => (bx0, by0, bx1, by1),
        }
    }

    /// Whether pixel center `(cx, cy)` passes this clip shape.
    fn accepts(&self, cx: f64, cy: f64) -> bool {
        match *self {
            ClipShape::Rect(x0, y0, x1, y1) => cx >= x0 as f64 && cx < x1 as f64
                && cy >= y0 as f64 && cy < y1 as f64,
            ClipShape::Affine { inv, x, y, w, h, radii, .. } => {
                let (lx, ly) = mat_apply(inv, cx, cy);
                rounded_contains(x, y, w, h, radii, lx, ly)
            }
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
        Self { width, height, data, clip: Vec::new(), xf_stack: Vec::new() }
    }

    /// A fully transparent canvas — the LOCAL scratch surface an affine
    /// bracket rasterizes a subtree tile into before blitting it through
    /// the transform (text/svg/replaced ink keeps its own rasterizer, which
    /// only knows how to write axis-aligned integer pixels).
    pub fn new_transparent(width: usize, height: usize) -> Self {
        Self { width, height, data: vec![0u8; width * height * 4], clip: Vec::new(), xf_stack: Vec::new() }
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

    // --- affine bracket (rotate/skew/matrix paint) ---

    /// The current total local→canvas map, or None while no bracket is
    /// open (the identity — callers route to the axis-aligned primitives).
    fn xf(&self) -> Option<[f64; 6]> {
        self.xf_stack.last().copied()
    }

    /// Open a transform bracket: the incoming map composes ONTO the current
    /// top (an empty stack is the identity), so nested brackets multiply —
    /// exactly the collect walk's child_xf chain.
    pub(crate) fn push_xf(&mut self, m: [f64; 6]) {
        let top = match self.xf() {
            Some(t) => mat_mul(t, m),
            None => m,
        };
        self.xf_stack.push(top);
    }

    /// Close the innermost transform bracket.
    pub(crate) fn pop_xf(&mut self) {
        self.xf_stack.pop();
    }

    /// Canvas-space bbox of a LOCAL rect through the current bracket
    /// (identity when none is open) — the affine items' band prefilter.
    fn mapped_xf_bounds(&self, x: f64, y: f64, w: f64, h: f64) -> (i64, i64, i64, i64) {
        match self.xf() {
            Some(m) => mapped_bounds(m, x, y, w, h),
            None => (x.floor() as i64, y.floor() as i64, (x + w).ceil() as i64, (y + h).ceil() as i64),
        }
    }

    /// Push a clip whose LOCAL rect maps through the current bracket: the
    /// stack entry stores the inverse map plus the local rounded rect, and
    /// its bounds are the mapped bbox intersected with the prior clips. A
    /// singular map (degenerate scale) clips everything.
    fn push_clip_affine(&mut self, x: f64, y: f64, w: f64, h: f64, radii: [(f32, f32); 4]) {
        let Some(m) = self.xf() else { return };
        let (bx0, by0, bx1, by1) = mapped_bounds(m, x, y, w, h);
        let (cx0, cy0, cx1, cy1) = self.allowed();
        let r = (bx0.max(cx0), by0.max(cy0), bx1.min(cx1), by1.min(cy1));
        match (r.2 > r.0 && r.3 > r.1, mat_inv(m)) {
            (true, Some(inv)) => self.clip.push(ClipShape::Affine {
                inv,
                x,
                y,
                w,
                h,
                radii,
                bx0: r.0,
                by0: r.1,
                bx1: r.2,
                by1: r.3,
            }),
            _ => self.clip.push(ClipShape::Rect(0, 0, 0, 0)),
        }
    }

    /// Source-over fill of a rounded LOCAL rect through the current
    /// bracket: each canvas pixel in the mapped bbox inverse-maps into
    /// local space and takes the same zone/ellipse corner test the
    /// axis-aligned fills use — hard edges, the batch-6b/7c posture
    /// (no antialiased arcs). Integer multiples of 90° are pixel-exact.
    fn fill_shape_affine(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radii: [(f32, f32); 4],
        color: [u8; 4],
    ) {
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let (bx0, by0, bx1, by1) = mapped_bounds(m, x, y, w, h);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                if !rounded_contains(x, y, w, h, radii, lx, ly) {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Gradient fill through the current bracket: stop positions project in
    /// LOCAL space (the gradient rides the element's box through the
    /// rotation), colors sample per inverse-mapped pixel center, and the
    /// shape clips by the same local rounded test a solid fill uses.
    fn fill_gradient_affine(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        stops: &[(f32, [u8; 4])],
        css_deg: f32,
        radii: [(f32, f32); 4],
    ) {
        if w <= 0.0 || h <= 0.0 || stops.len() < 2 {
            return;
        }
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let rad = css_deg.to_radians() as f64;
        let (dux, duy) = (rad.sin(), -rad.cos());
        let len = (w * dux.abs() + h * duy.abs()).max(1.0);
        let (ccx, ccy) = (x + w / 2.0, y + h / 2.0);
        let (bx0, by0, bx1, by1) = mapped_bounds(m, x, y, w, h);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                if !rounded_contains(x, y, w, h, radii, lx, ly) {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let t = (((lx - ccx) * dux + (ly - ccy) * duy) / len + 0.5).clamp(0.0, 1.0) as f32;
                let color = gradient_stop_color(stops, t);
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Border through the current bracket: pixels inside the outer local
    /// rounded box and NOT inside the widths-inset inner rounded box — the
    /// rotated twin of the axis-aligned ring paint (affine residuals batch;
    /// radii ride the bracket like every other rounded shape).
    #[allow(clippy::too_many_arguments)]
    fn border_affine(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        widths: [f32; 4],
        radii: [(f32, f32); 4],
        color: [u8; 4],
    ) {
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let (bx0, by0, bx1, by1) = mapped_bounds(m, x, y, w, h);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                if !border_ring_contains(x, y, w, h, widths, radii, lx, ly) {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Axis-aligned rounded border ring: the same outer-minus-inner test
    /// [`border_affine`](Self::border_affine) inverse-maps for, evaluated
    /// directly on canvas pixel centers. Only reached with nonzero radii —
    /// square borders keep the four-band fast path (bit-for-bit the
    /// historical paint).
    #[allow(clippy::too_many_arguments)]
    fn fill_border_ring(
        &mut self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        widths: [f32; 4],
        radii: [(f32, f32); 4],
        color: [u8; 4],
    ) {
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in y.max(ay0).max(0)..(y + h).min(ay1).min(self.height as i64) {
            for gx in x.max(ax0).max(0)..(x + w).min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                if !border_ring_contains(x as f64, y as f64, w as f64, h as f64, widths, radii, cx, cy) {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], color);
            }
        }
    }

    /// Open a canvas-coordinate bracket ([`PaintItem::SetXfCanvas`]): the
    /// map REPLACES the current top instead of composing onto it, so items
    /// until the matching ClearXf paint in canvas space (the band shift in
    /// a viewport-band paint, the identity in full-page execute), cancelling
    /// any enclosing transform bracket.
    pub(crate) fn push_xf_canvas(&mut self, m: [f64; 6]) {
        self.xf_stack.push(m);
    }

    /// Nearest-neighbor image blit through the current bracket: the
    /// element's LOCAL paint rect maps through the map; each canvas pixel
    /// in the mapped bbox inverse-maps to a local point, which scales into
    /// source texel space exactly like the axis-aligned `blit_image`
    /// (destination-pixel-center sampling).
    fn blit_image_affine(
        &mut self,
        image: &super::image::DecodedImage,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        alpha: f32,
    ) {
        if w <= 0.0 || h <= 0.0 || image.width == 0 || image.height == 0 {
            return;
        }
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let src = &image.rgba;
        let (sw, sh) = (image.width as f64, image.height as f64);
        let (bx0, by0, bx1, by1) = mapped_bounds(m, x, y, w, h);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                if !(lx >= x && lx < x + w && ly >= y && ly < y + h) {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let sx = (((lx - x) * sw / w) as i64).clamp(0, image.width as i64 - 1);
                let sy = (((ly - y) * sh / h) as i64).clamp(0, image.height as i64 - 1);
                let i = ((sy as usize * image.width as usize) + sx as usize) * 4;
                let a = if alpha >= 1.0 {
                    src[i + 3]
                } else {
                    (src[i + 3] as f32 * alpha).round() as u8
                };
                let src_px = [src[i], src[i + 1], src[i + 2], a];
                let d = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[d..d + 4], src_px);
            }
        }
    }

    /// Blit a straight-alpha RGBA tile rasterized in LOCAL coordinates
    /// through the current bracket: tile pixel (0, 0) lands at local point
    /// `(ox, oy)`. Nearest sampling; transparent pixels skip — the affine
    /// twin of `blit_text`.
    fn blit_rgba_affine(&mut self, src: &[u8], sw: usize, sh: usize, ox: f64, oy: f64) {
        if sw == 0 || sh == 0 {
            return;
        }
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let (bx0, by0, bx1, by1) = mapped_bounds(m, ox, oy, sw as f64, sh as f64);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                let (tx, ty) = ((lx - ox) as i64, (ly - oy) as i64);
                if tx < 0 || ty < 0 || tx >= sw as i64 || ty >= sh as i64 {
                    continue;
                }
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let i = (ty as usize * sw + tx as usize) * 4;
                let a = src[i + 3];
                if a == 0 {
                    continue;
                }
                let src_px = [src[i], src[i + 1], src[i + 2], a];
                let d = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[d..d + 4], src_px);
            }
        }
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

    /// Source-over fill of a `linear-gradient(...)` box (gradient batch):
    /// each pixel's stop position is its projection onto the CSS gradient
    /// line — direction `(sin θ, −cos θ)` in screen space (y grows down,
    /// css_deg 0 = to top, clockwise), line length `|w·sinθ| + |h·cosθ|`,
    /// position `dot(p − center, dir)/len + 0.5` clamped to [0,1]. Stop
    /// colors interpolate in premultiplied space; the shape clips through
    /// the same per-corner elliptical test a solid `BgCorner` uses (the
    /// fill follows the rounded box).
    pub fn fill_gradient(
        &mut self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        stops: &[(f32, [u8; 4])],
        css_deg: f32,
        radii: [(f32, f32); 4],
    ) {
        if w <= 0 || h <= 0 || stops.len() < 2 {
            return;
        }
        let (ax0, ay0, ax1, ay1) = self.allowed();
        let rad = css_deg.to_radians() as f64;
        let (dux, duy) = (rad.sin(), -rad.cos());
        let len = (w as f64 * dux.abs() + h as f64 * duy.abs()).max(1.0);
        let (ccx, ccy) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
        // Row-invariant part of the position: hoisted so the inner loop is
        // one fused multiply-add per pixel.
        let col_a = dux / len;
        // Degenerate corners (either radius ≤ 0) don't round; only build the
        // clip shape when at least one corner is live.
        let rounded = radii.iter().any(|r| r.0 > 0.0 && r.1 > 0.0);
        let shape = rounded.then_some(ClipShape::Rounded { x0: x, y0: y, x1: x + w, y1: y + h, radii });
        for gy in y.max(0).max(ay0)..(y + h).min(self.height as i64).min(ay1) {
            let row_base = (gy as f64 + 0.5 - ccy) * duy / len + 0.5;
            for gx in x.max(0).max(ax0)..(x + w).min(self.width as i64).min(ax1) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                if let Some(s) = &shape {
                    if !s.accepts(cx, cy) {
                        continue;
                    }
                }
                let t = ((cx - ccx) * col_a + row_base).clamp(0.0, 1.0) as f32;
                let color = gradient_stop_color(stops, t);
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

    /// Outer box-shadow (blitz#349 family, v1): per-pixel SDF around the
    /// offset/spread-inflated shadow box, the element's own border box
    /// knocked out (a transparent background must not show the shadow
    /// inside it — Chrome semantics). The feather is a linear falloff over
    /// [0, blur]: the same visible extent as Chrome's gaussian σ=blur/2,
    /// cheaper per pixel, no blur taps.
    ///
    /// The inset twin (`inset: true`, v2): the shadow box is the element
    /// box offset by (dx, dy) and SHRUNK by a positive spread; ink fills
    /// from its edge outward, fading to nothing `blur` px INTO the box
    /// (`alpha = 1 + d/blur` for the signed distance d), everything is
    /// hard-clipped to the element box, and a fully collapsed shadow box
    /// (spread ≥ half-dim) legally shadows the whole element.
    #[allow(clippy::too_many_arguments)]
    fn fill_box_shadow(
        &mut self,
        rect: &Rect,
        color: [u8; 4],
        radii: [(f32, f32); 4],
        dx: f32,
        dy: f32,
        blur: f32,
        spread: f32,
        inset: bool,
    ) {
        if rect.width <= 0.0 || rect.height <= 0.0 || color[3] == 0 {
            return;
        }
        let ehw = rect.width as f64 / 2.0;
        let ehh = rect.height as f64 / 2.0;
        let (ecx, ecy) = (rect.x as f64 + ehw, rect.y as f64 + ehh);
        let (scx, scy) = (ecx + dx as f64, ecy + dy as f64);
        let feather = blur.max(0.0) as f64;
        let (shw, shh) = if inset {
            // A collapsed shadow box is legal: clamp to a point and the
            // whole element ends up outside it (fully shadowed).
            ((ehw - spread as f64).max(0.0), (ehh - spread as f64).max(0.0))
        } else {
            (ehw + spread as f64, ehh + spread as f64)
        };
        if !inset && (shw <= 0.0 || shh <= 0.0) {
            return;
        }
        // Outer bounds hug the feather-inflated shadow box; inset ink is
        // hard-clipped to the element box (its feather lives inside it),
        // so the loop never leaves the element.
        let (bx0, by0, bx1, by1) = if inset {
            (ecx - ehw, ecy - ehh, ecx + ehw, ecy + ehh)
        } else {
            let pad = feather + 1.0;
            (scx - shw - pad, scy - shh - pad, scx + shw + pad, scy + shh + pad)
        };
        let (ax0, ay0, ax1, ay1) = self.allowed();
        let x0 = (bx0.floor() as i64).max(ax0).max(0);
        let y0 = (by0.floor() as i64).max(ay0).max(0);
        let x1 = (bx1.ceil() as i64).min(ax1).min(self.width as i64);
        let y1 = (by1.ceil() as i64).min(ay1).min(self.height as i64);
        for gy in y0..y1 {
            for gx in x0..x1 {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (qx, qy) = (cx - ecx, cy - ecy);
                let er = shadow_corner_radius(radii, qx, qy).min(ehw.min(ehh));
                let outside = sd_rounded_box(qx, qy, ehw - er, ehh - er, er) >= 0.0;
                // Outer shadows knock the element interior out; inset ink
                // never crosses the element edge — the two skip opposite
                // sides of the same test.
                if outside == inset {
                    continue;
                }
                let (px, py) = (cx - scx, cy - scy);
                let sr = (shadow_corner_radius(radii, px, py)
                    + if inset { -spread as f64 } else { spread as f64 })
                .clamp(0.0, shw.min(shh));
                let d = sd_rounded_box(px, py, shw - sr, shh - sr, sr);
                let a = if feather > 0.0 {
                    if inset {
                        (1.0 + d / feather).clamp(0.0, 1.0)
                    } else {
                        (1.0 - d / feather).clamp(0.0, 1.0)
                    }
                } else if inset == (d >= 0.0) {
                    1.0
                } else {
                    0.0
                };
                if a <= 0.0 {
                    continue;
                }
                let mut src = color;
                src[3] = ((color[3] as f64 * a).round() as i64).clamp(0, 255) as u8;
                if src[3] == 0 {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], src);
            }
        }
    }

    /// The affine twin: the shadow box lives in LOCAL space (the offset
    /// rides the element's transform), the loop bounds are the
    /// feather-inflated local box mapped through the bracket, and every
    /// canvas pixel inverse-maps into local space for the same SDF test.
    /// Inset mirrors the canvas twin: shadow box shrunk by spread, ink
    /// hard-clipped to the local element box, feather falling inward.
    #[allow(clippy::too_many_arguments)]
    fn fill_shadow_affine(
        &mut self,
        rect: &Rect,
        color: [u8; 4],
        radii: [(f32, f32); 4],
        dx: f32,
        dy: f32,
        blur: f32,
        spread: f32,
        inset: bool,
    ) {
        if rect.width <= 0.0 || rect.height <= 0.0 || color[3] == 0 {
            return;
        }
        let Some(m) = self.xf() else { return };
        let Some(inv) = mat_inv(m) else { return };
        let ehw = rect.width as f64 / 2.0;
        let ehh = rect.height as f64 / 2.0;
        let (ecx, ecy) = (rect.x as f64 + ehw, rect.y as f64 + ehh);
        let (scx, scy) = (ecx + dx as f64, ecy + dy as f64);
        let feather = blur.max(0.0) as f64;
        let (shw, shh) = if inset {
            ((ehw - spread as f64).max(0.0), (ehh - spread as f64).max(0.0))
        } else {
            (ehw + spread as f64, ehh + spread as f64)
        };
        if !inset && (shw <= 0.0 || shh <= 0.0) {
            return;
        }
        let (lx0, ly0, lx1, ly1) = if inset {
            (ecx - ehw, ecy - ehh, ecx + ehw, ecy + ehh)
        } else {
            let pad = feather + 1.0;
            (scx - shw - pad, scy - shh - pad, scx + shw + pad, scy + shh + pad)
        };
        let (bx0, by0, bx1, by1) = mapped_bounds(m, lx0, ly0, lx1 - lx0, ly1 - ly0);
        let (ax0, ay0, ax1, ay1) = self.allowed();
        for gy in by0.max(ay0).max(0)..by1.min(ay1).min(self.height as i64) {
            for gx in bx0.max(ax0).max(0)..bx1.min(ax1).min(self.width as i64) {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                let (lx, ly) = mat_apply(inv, cx, cy);
                let (qx, qy) = (lx - ecx, ly - ecy);
                let er = shadow_corner_radius(radii, qx, qy).min(ehw.min(ehh));
                let outside = sd_rounded_box(qx, qy, ehw - er, ehh - er, er) >= 0.0;
                if outside == inset {
                    continue;
                }
                let (px, py) = (lx - scx, ly - scy);
                let sr = (shadow_corner_radius(radii, px, py)
                    + if inset { -spread as f64 } else { spread as f64 })
                .clamp(0.0, shw.min(shh));
                let d = sd_rounded_box(px, py, shw - sr, shh - sr, sr);
                let a = if feather > 0.0 {
                    if inset {
                        (1.0 + d / feather).clamp(0.0, 1.0)
                    } else {
                        (1.0 - d / feather).clamp(0.0, 1.0)
                    }
                } else if inset == (d >= 0.0) {
                    1.0
                } else {
                    0.0
                };
                if a <= 0.0 || !self.clip_accepts(cx, cy) {
                    continue;
                }
                let mut src = color;
                src[3] = ((color[3] as f64 * a).round() as i64).clamp(0, 255) as u8;
                if src[3] == 0 {
                    continue;
                }
                let i = (gy as usize * self.width + gx as usize) * 4;
                over(&mut self.data[i..i + 4], src);
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

// --- 2D affine math (affine batch) — CSS matrix order throughout:
// [a, b, c, d, e, f] maps x' = a·x + c·y + e, y' = b·x + d·y + f. ---

/// Matrix product m∘n (apply n first, then m) in the CSS column convention —
/// the same composition the collect walk's Xf::compose and the parse-time
/// function-list accumulation use, in f64 for raster-side exactness.
fn mat_mul(m: [f64; 6], n: [f64; 6]) -> [f64; 6] {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

/// Apply the map to a point.
fn mat_apply(m: [f64; 6], x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// Inverse of the map, or None when the linear part is singular (a
/// degenerate scale collapses the plane to a line — nothing to rasterize).
fn mat_inv(m: [f64; 6]) -> Option<[f64; 6]> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-9 {
        return None;
    }
    let ia = m[3] / det;
    let ib = -m[1] / det;
    let ic = -m[2] / det;
    let id = m[0] / det;
    Some([
        ia,
        ib,
        ic,
        id,
        -(ia * m[4] + ic * m[5]),
        -(ib * m[4] + id * m[5]),
    ])
}

/// Canvas-space integer bbox of a local rect mapped through `m`: the four
/// mapped corners' union, floored/ceiled outward — a conservative superset
/// of the true mapped area (fill loops re-test every pixel anyway).
fn mapped_bounds(m: [f64; 6], x: f64, y: f64, w: f64, h: f64) -> (i64, i64, i64, i64) {
    let (x0, y0) = mat_apply(m, x, y);
    let (x1, y1) = mat_apply(m, x + w, y);
    let (x2, y2) = mat_apply(m, x, y + h);
    let (x3, y3) = mat_apply(m, x + w, y + h);
    let xs = [x0, x1, x2, x3];
    let ys = [y0, y1, y2, y3];
    (
        xs.iter().copied().fold(f64::MAX, f64::min).floor() as i64,
        ys.iter().copied().fold(f64::MAX, f64::min).floor() as i64,
        xs.iter().copied().fold(f64::MIN, f64::max).ceil() as i64,
        ys.iter().copied().fold(f64::MIN, f64::max).ceil() as i64,
    )
}

/// LOCAL-space rounded-rect containment (the affine twin of
/// [`ClipShape::Rounded::accepts`]): a pixel's inverse image lands in local
/// coordinates, where the same zone/ellipse corner test runs against the
/// rect's own radii (clamped to half the box per the CSS scale-down rule).
/// Zero radii degenerate to the plain rect test.
fn rounded_contains(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    radii: [(f32, f32); 4],
    lx: f64,
    ly: f64,
) -> bool {
    if !(lx >= x && lx < x + w && ly >= y && ly < y + h) {
        return false;
    }
    let clamp = |r: (f32, f32)| {
        (
            r.0.clamp(0.0, w as f32 / 2.0) as f64,
            r.1.clamp(0.0, h as f32 / 2.0) as f64,
        )
    };
    let (rx0, ry0) = clamp(radii[0]);
    let (rx1, ry1) = clamp(radii[1]);
    let (rx2, ry2) = clamp(radii[2]);
    let (rx3, ry3) = clamp(radii[3]);
    let centers = [
        (x + rx0, y + ry0),
        (x + w - rx1, y + ry1),
        (x + w - rx2, y + h - ry2),
        (x + rx3, y + h - ry3),
    ];
    let zone = if lx < centers[0].0 && ly < centers[0].1 {
        Some(0usize)
    } else if lx > centers[1].0 && ly < centers[1].1 {
        Some(1)
    } else if lx > centers[2].0 && ly > centers[2].1 {
        Some(2)
    } else if lx < centers[3].0 && ly > centers[3].1 {
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
                let dx = (lx - centers[i].0) / rxs[i];
                let dy = (ly - centers[i].1) / rys[i];
                dx * dx + dy * dy <= 1.0
            }
        }
    }
}

/// Rounded-box signed distance (iq's formula, box-shadow v1): negative
/// inside, zero on the edge, the distance in px outside. `b` is the
/// half-extent MINUS the corner radius, `r` the radius.
fn sd_rounded_box(px: f64, py: f64, bx: f64, by: f64, r: f64) -> f64 {
    let qx = px.abs() - bx;
    let qy = py.abs() - by;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Per-quadrant corner radius (CSS order TL TR BR BL) as the mean of the
/// corner's (rx, ry): the SDF is circular, so an elliptical corner
/// approximates to its mean — uniform radii (the overwhelming case) are
/// exact. Quadrants read off the point's sign relative to the box center.
fn shadow_corner_radius(radii: [(f32, f32); 4], qx: f64, qy: f64) -> f64 {
    let i = if qx < 0.0 && qy < 0.0 {
        0
    } else if qx >= 0.0 && qy < 0.0 {
        1
    } else if qx >= 0.0 && qy >= 0.0 {
        2
    } else {
        3
    };
    (radii[i].0 + radii[i].1) as f64 / 2.0
}

/// LOCAL-space border-ring containment (affine residuals batch): inside the
/// outer rounded box AND outside the widths-inset inner rounded box — the
/// CSS border shape. Inner radii shrink by the adjacent border widths per
/// corner (TL: left/top, TR: right/top, …), floored at 0 — the corner-box
/// approximation every raster browser uses. A degenerate inner box (the
/// border thicker than the box on an axis) leaves a solid fill. Zero radii
/// degenerate to the plain square ring.
#[allow(clippy::too_many_arguments)]
fn border_ring_contains(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    widths: [f32; 4],
    radii: [(f32, f32); 4],
    lx: f64,
    ly: f64,
) -> bool {
    let [t, r, b, l] = widths;
    if !rounded_contains(x, y, w, h, radii, lx, ly) {
        return false;
    }
    let (ix, iy) = (x + l as f64, y + t as f64);
    let (iw, ih) = ((w - l as f64 - r as f64).max(0.0), (h - t as f64 - b as f64).max(0.0));
    if iw <= 0.0 || ih <= 0.0 {
        return true;
    }
    let inner_radii = [
        ((radii[0].0 - l).max(0.0), (radii[0].1 - t).max(0.0)),
        ((radii[1].0 - r).max(0.0), (radii[1].1 - t).max(0.0)),
        ((radii[2].0 - r).max(0.0), (radii[2].1 - b).max(0.0)),
        ((radii[3].0 - l).max(0.0), (radii[3].1 - b).max(0.0)),
    ];
    !rounded_contains(ix, iy, iw, ih, inner_radii, lx, ly)
}

/// Gradient stop color at position `t` (0..1). Stops are ascending; `t`
/// outside the list clamps to the end stops. The ramp scan uses a strict
/// upper bound, so a shared position (hard line, `blue 50%, green 50%`)
/// falls through to the next ramp and the LATER stop's color owns the
/// line itself — Chrome's behavior at the discontinuity.
fn gradient_stop_color(stops: &[(f32, [u8; 4])], t: f32) -> [u8; 4] {
    if t <= stops[0].0 {
        return stops[0].1;
    }
    let last = stops.len() - 1;
    if t >= stops[last].0 {
        return stops[last].1;
    }
    for w in stops.windows(2) {
        let (p0, c0) = w[0];
        let (p1, c1) = w[1];
        if t < p1 {
            // Windows tile [p0, last) and the outer clamps pin t >= p0,
            // so the selected window always has span > 0.
            let k = (t - p0) / (p1 - p0);
            return lerp_premultiplied(c0, c1, k);
        }
    }
    stops[last].1
}

/// Interpolate two straight-alpha colors at `k` in premultiplied space:
/// premultiply both endpoints, lerp, un-premultiply by the lerped alpha.
/// A straight lerp of rgba() midpoints drags transparent colors toward
/// gray (rgba(255,0,0,0)→rgba(0,0,255,255) at 0.5 would come out purple).
fn lerp_premultiplied(a: [u8; 4], b: [u8; 4], k: f32) -> [u8; 4] {
    let out_a = a[3] as f32 + (b[3] as f32 - a[3] as f32) * k;
    if out_a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let ch = |x: u8, y: u8| {
        let px = x as f32 * a[3] as f32 / 255.0;
        let py = y as f32 * b[3] as f32 / 255.0;
        ((px + (py - px) * k) * 255.0 / out_a).round().clamp(0.0, 255.0) as u8
    };
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2]), out_a.round().clamp(0.0, 255.0) as u8]
}

/// background-clip: text fill (gradient-text batch): rewrite an
/// opaque-white text raster in place into the gradient — each covered
/// pixel samples at its own position `(x + gx, y + top + gy)` in the item's
/// coordinate space, final alpha = glyph coverage × stop alpha. The
/// projection math mirrors [`Canvas::fill_gradient`] (same CSS gradient
/// line convention), minus the shape test: coverage IS the shape.
fn recolor_gradient_text(r: &mut TextRaster, x: f32, y: f32, g: &TextGradient) {
    if r.width == 0 || r.height == 0 || g.stops.len() < 2 {
        return;
    }
    let rad = g.css_deg.to_radians() as f64;
    let (dux, duy) = (rad.sin(), -rad.cos());
    let len = (g.area.width as f64 * dux.abs() + g.area.height as f64 * duy.abs()).max(1.0);
    let (ccx, ccy) = (
        g.area.x as f64 + g.area.width as f64 / 2.0,
        g.area.y as f64 + g.area.height as f64 / 2.0,
    );
    let col_a = dux / len;
    // Pixel row 0's y in the item space (the compositor blits at
    // (x, y + top)) — row_base per row, fused multiply-add per pixel.
    let tile_top = y as f64 + r.top as f64;
    for gy in 0..r.height {
        let row_base = (tile_top + gy as f64 - ccy) * duy / len + 0.5;
        for gx in 0..r.width {
            let i = (gy * r.width + gx) * 4;
            let coverage = r.data[i + 3];
            if coverage == 0 {
                continue;
            }
            let t = (((x as f64 + gx as f64) - ccx) * col_a + row_base).clamp(0.0, 1.0) as f32;
            let c = gradient_stop_color(&g.stops, t);
            r.data[i] = c[0];
            r.data[i + 1] = c[1];
            r.data[i + 2] = c[2];
            r.data[i + 3] = ((c[3] as u32 * coverage as u32 + 127) / 255) as u8;
        }
    }
}

/// Rough advance width: CJK/fullwidth ≈ 1em, everything else ≈ 0.6em.
pub(crate) fn est_width(text: &str, font_size: f32) -> f32 {
    text.chars()
        .map(|c| if c > '\u{2E80}' { 1.0 } else { 0.6 })
        .sum::<f32>()
        * font_size
}

/// Paint a text item's `text-decoration-line` set — one rect per wrapped
/// line, in the text's own color. Line breaks and baselines replay the
/// exact `rasterize_wrapped` math (same tokens, same `baseline_offset`), so
/// the strokes sit where the glyphs are regardless of wrap width. Geometry
/// is font-metric derived (CSS Text Decoration 4's `auto` position/thickness
/// family): underline rides half the descent under the baseline,
/// line-through sits at ~x-height, overline caps the ascent; thickness
/// scales with font size at 1px per 16px.
#[allow(clippy::too_many_arguments)]
fn paint_text_decorations(
    out: &mut Canvas,
    fonts: &FontBook,
    text: &str,
    font_size: f32,
    bold: bool,
    color: [u8; 4],
    line_height: f32,
    x: f32,
    y: f32,
    wrap_at: f32,
    decorations: TextDecorations,
    mono: bool,
    word_spacing: f32,
    truncate_at: Option<f32>,
    // The item's wrap tokens, pre-shaped when the item carries the run
    // leaf's memo (unscaled font params): the decorations painter needs the
    // wrap LINES, which the glyph-pixel RasterCache doesn't hold, so
    // without this every repaint re-shaped the run here.
    pre_shaped: Option<&[Token]>,
    dx: f32,
    dy: f32,
) {
    if decorations.is_empty() || text.trim().is_empty() {
        return;
    }
    let owned;
    let tokens = match pre_shaped {
        Some(t) => t,
        None => {
            owned = tokens_of(text, font_size, bold, fonts, mono, word_spacing);
            &owned
        }
    };
    // The ellipsis marker is undecorated (Chrome): strokes span the kept
    // tokens only, so underline/line-through end at the truncation cut.
    let kept;
    let tokens: &[Token] = match truncate_at.and_then(|limit| truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing)) {
        Some((t, _marker)) => {
            kept = t;
            &kept
        }
        None => tokens,
    };
    let lines = greedy_wrap(tokens, Some(wrap_at.max(0.0)));
    let m = fonts.metrics(font_size, bold).unwrap_or(ScaledMetrics {
        ascent: font_size,
        descent: font_size * 0.2,
        line_gap: 0.0,
    });
    let b0 = baseline_offset(m.ascent, m.descent, line_height);
    let thickness = (font_size / 16.0).round().max(1.0);
    let mut stroke = |lx: f32, ly: f32, w: f32| {
        if w <= 0.0 {
            return;
        }
        if out.xf().is_some() {
            // fill_rect paints raw canvas coords — under a transform bracket
            // the stroke needs the affine too, so blit a solid tile instead.
            let tw = w.ceil().max(1.0) as usize;
            let th = thickness as usize;
            let mut tile = vec![0u8; tw * th * 4];
            for px in tile.chunks_exact_mut(4) {
                px.copy_from_slice(&color);
            }
            out.blit_rgba_affine(&tile, tw, th, lx as f64, ly as f64);
        } else {
            out.fill_rect(
                (lx - dx).round() as i64,
                (ly - dy).round() as i64,
                w.round().max(1.0) as i64,
                thickness as i64,
                color,
            );
        }
    };
    for (i, line) in lines.iter().enumerate() {
        if line.width <= 0.0 {
            continue;
        }
        let baseline = y + (i as f32 * line_height).round() + b0;
        if decorations.underline {
            stroke(x, baseline + (m.descent * 0.5).max(1.0), line.width);
        }
        if decorations.overline {
            stroke(x, baseline - m.ascent, line.width);
        }
        if decorations.line_through {
            stroke(x, baseline - font_size * 0.28, line.width);
        }
    }
}

/// The page-space extent of the `Text` items alone, from the same wrap model
/// the band pre-filter and `rasterize_wrapped` use (height = y + lines ×
/// line-height, width = one wrapped line). Bare text owns no element box —
/// html/body stretch to the viewport — so this is the only place its true
/// extent exists: the scroll-union in `band_frame` and the print pump's page
/// count both read it. Errs high, never low.
/// Fold the wrap-model ink extent of every Text item, in CANVAS space
/// (blitz#841 transform half). Under a non-diagonal transform the collect
/// walk brackets the subtree's items in LOCAL coordinates (`SetXf`); a
/// naive union of raw x/y then overestimates the scrollable region — a
/// rotated long line reports its unrotated width as horizontal ink.
/// Track the bracket exactly like the paint pass (SetXf composes on top of
/// the open map, SetXfCanvas cancels it, ClearXf pops) and map each item's
/// ink box through the active map before it joins the union. Prebaked
/// (diagonal) chains carry no bracket, so their items — already in canvas
/// space — pass through an identity map and the fold is bit-identical to
/// the pre-transform behavior.
pub fn text_ink_extent(items: &[PaintItem]) -> (f32, f32) {
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    let mut cur = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut open: Vec<[f32; 6]> = Vec::new();
    for item in items {
        match item {
            PaintItem::SetXf { xf } => {
                open.push(cur);
                cur = compose_arr(cur, *xf);
            }
            PaintItem::SetXfCanvas => {
                open.push(cur);
                cur = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
            }
            PaintItem::ClearXf => {
                if let Some(prev) = open.pop() {
                    cur = prev;
                }
            }
            PaintItem::Text { text, font_size, line_height, x, y, wrap_at, .. } => {
                let wrap = wrap_at.max(1.0);
                let est = est_width(text, *font_size);
                let lines = (est / wrap).ceil().max(1.0);
                let local = Rect {
                    x: *x,
                    y: *y,
                    width: est.min(wrap),
                    height: lines * line_height,
                };
                let r = super::map_rect_arr(cur, local);
                w = w.max(r.x + r.width);
                h = h.max(r.y + r.height);
            }
            _ => {}
        }
    }
    (w, h)
}

/// Compose two CSS-order affine arrays, `m` outer and `n` inner (m·n) —
/// array twin of the collect walk's fn-local `Xf::compose`.
fn compose_arr(m: [f32; 6], n: [f32; 6]) -> [f32; 6] {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

/// One vectorizable text run for the PDF text layer (vector-text batch):
/// shaped glyphs in absolute page px plus the decoration strokes, in the
/// item's own color. The writer embeds the same face bytes the shaper used,
/// so glyph ids pass through with no remap.
#[derive(Debug)]
pub(crate) struct PdfLine {
    pub font_size: f32,
    pub color: [u8; 4],
    pub glyphs: Vec<PdfGlyph>,
    /// (x, y_top, width) rects in page px; height is the decoration
    /// thickness derived from font_size at write time.
    pub strokes: Vec<(f32, f32, f32)>,
}

/// A clip (rect only) or text run, in document order — the PDF text layer's
/// mini-stream. Rounded clips don't appear here: a run inside one stays
/// raster (the vector layer can't express the corner cut), so only rect
/// clips need to bracket the runs.
#[derive(Debug)]
pub(crate) enum PdfOp {
    Clip { x: f32, y: f32, w: f32, h: f32 },
    PopClip,
    Line(PdfLine),
}

/// The text-layer half of a paint list: every `Text` item that can be
/// re-expressed as positioned glyphs, plus the rect-clip brackets it sits
/// under. Returns the ops in document order and the indexes of items the
/// caller must DROP from its raster pass (vectorized text painted twice
/// would double the antialiasing). Mirrors the raster walk exactly —
/// same token/pre-shaped path, same truncate-then-marker model, same
/// greedy wrap and per-line baselines — so vector and raster ink land on
/// the same pixels.
pub(crate) fn pdf_text_ops(items: &[PaintItem], fonts: &FontBook) -> (Vec<PdfOp>, HashSet<usize>) {
    let mut ops: Vec<PdfOp> = Vec::new();
    let mut vectorized: HashSet<usize> = HashSet::new();
    let mut clip_stack: Vec<bool> = Vec::new();
    let mut xf_depth = 0usize;
    for (idx, item) in items.iter().enumerate() {
        match item {
            PaintItem::Clip { rect } => {
                ops.push(PdfOp::Clip { x: rect.x, y: rect.y, w: rect.width, h: rect.height });
                clip_stack.push(true);
            }
            PaintItem::ClipRounded { .. } => clip_stack.push(false),
            PaintItem::PopClip => {
                if clip_stack.pop() == Some(true) {
                    ops.push(PdfOp::PopClip);
                }
            }
            PaintItem::SetXf { .. } | PaintItem::SetXfCanvas => xf_depth += 1,
            PaintItem::ClearXf => xf_depth = xf_depth.saturating_sub(1),
            PaintItem::Text {
                text,
                font_size,
                bold,
                color,
                line_height,
                x,
                y,
                wrap_at,
                gradient,
                decorations,
                mono,
                word_spacing,
                truncate_at,
                tokens,
                text_shadow,
            } => {
                // Vector gate: the shaper must cover every segment (fallback
                // faces are emoji color bitmaps with no outlines), the fill
                // must be plain opaque (gradients paint per-pixel through
                // the glyphs, shadows layer under them, subtree opacity rides
                // color[3]), and the item must sit in page space — under a
                // transform bracket the coordinates are LOCAL, and under a
                // rounded clip the ink is corner-cut. Any miss keeps the item
                // raster, where the exact painting already exists.
                if text.trim().is_empty()
                    || gradient.is_some()
                    || text_shadow.is_some()
                    || color[3] != 255
                    || xf_depth != 0
                    || clip_stack.contains(&false)
                {
                    continue;
                }
                let Some(line) = pdf_vectorize_line(
                    text, *font_size, *bold, *color, *line_height, *x, *y, *wrap_at,
                    *decorations, *mono, *word_spacing, *truncate_at, tokens.as_deref(), fonts,
                ) else {
                    continue;
                };
                vectorized.insert(idx);
                ops.push(PdfOp::Line(line));
            }
            _ => {}
        }
    }
    (ops, vectorized)
}

/// Vectorize one `Text` item: shape its wrapped lines into positioned
/// glyphs (page px, absolute ink y) and collect the decoration strokes.
/// None = "can't express this as vector" (a fallback face somewhere, or
/// nothing would paint at all) — the caller keeps the item raster.
#[allow(clippy::too_many_arguments)]
fn pdf_vectorize_line(
    text: &str,
    font_size: f32,
    bold: bool,
    color: [u8; 4],
    line_height: f32,
    x: f32,
    y: f32,
    wrap_at: f32,
    decorations: TextDecorations,
    mono: bool,
    word_spacing: f32,
    truncate_at: Option<f32>,
    pre_shaped: Option<&[Token]>,
    fonts: &FontBook,
) -> Option<PdfLine> {
    let owned;
    let tokens: &[Token] = match pre_shaped {
        Some(t) => t,
        None => {
            owned = tokens_of(text, font_size, bold, fonts, mono, word_spacing);
            &owned
        }
    };
    // Painted tokens: kept + the U+2026 marker appended, exactly the model
    // `rasterize_wrapped_uncached` paints; decorations wrap the kept set
    // only (the marker is undecorated, Chrome).
    let truncated;
    let painted: &[Token] = match truncate_at.and_then(|limit| {
        truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing)
    }) {
        Some((mut kept, marker)) => {
            kept.extend(marker);
            truncated = kept;
            &truncated
        }
        None => tokens,
    };
    let decorated;
    let kept: &[Token] = match truncate_at.and_then(|limit| {
        truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing)
    }) {
        Some((k, _)) => {
            decorated = k;
            &decorated
        }
        None => tokens,
    };
    let lines = greedy_wrap(painted, Some(wrap_at.max(0.0)));
    if lines.iter().all(|l| l.width <= 0.0) {
        return None;
    }
    let m = fonts.metrics(font_size, bold).unwrap_or(ScaledMetrics {
        ascent: font_size,
        descent: font_size * 0.2,
        line_gap: 0.0,
    });
    let b0 = baseline_offset(m.ascent, m.descent, line_height);

    let mut glyphs: Vec<PdfGlyph> = Vec::new();
    let shape_run = |run: &str, pen: f32, baseline: f32, out: &mut Vec<PdfGlyph>| -> bool {
        // Shaped advances/glyph ids, pen-relative → page space. The whole-line
        // string at pen 0 is the word-spacing==0 paint model; per non-space
        // token at the cumulative pen is the word-spacing>0 one (spaces only
        // advance, mirroring the raster token walk).
        let Some(gs) = fonts.pdf_shape(run, font_size, bold, mono) else {
            return false;
        };
        out.extend(gs.into_iter().map(|mut g| {
            g.x += x + pen;
            g.y += baseline;
            g
        }));
        true
    };
    if word_spacing == 0.0 {
        for (li, line) in lines.iter().enumerate() {
            if line.token_idx.is_empty() {
                continue;
            }
            let s: String = line.token_idx.iter().map(|&i| painted[i].text.as_str()).collect();
            let baseline = y + (li as f32 * line_height).round() + b0;
            if !shape_run(&s, 0.0, baseline, &mut glyphs) {
                return None;
            }
        }
    } else {
        for (li, line) in lines.iter().enumerate() {
            let mut pen = 0.0f32;
            let baseline = y + (li as f32 * line_height).round() + b0;
            for &i in &line.token_idx {
                let t = &painted[i];
                if !t.is_space && !shape_run(&t.text, pen, baseline, &mut glyphs) {
                    return None;
                }
                pen += t.width;
            }
        }
    }
    if glyphs.is_empty() && decorations.is_empty() {
        return None;
    }

    let mut strokes: Vec<(f32, f32, f32)> = Vec::new();
    if !decorations.is_empty() {
        // Mirror paint_text_decorations to the pixel: kept-token wrap, same
        // baseline steps. Stroke height (1px per 16px font) is the writer's
        // business — it lives in the PDF op stream, not here.
        let dlines = greedy_wrap(kept, Some(wrap_at.max(0.0)));
        for (i, line) in dlines.iter().enumerate() {
            if line.width <= 0.0 {
                continue;
            }
            let baseline = y + (i as f32 * line_height).round() + b0;
            if decorations.underline {
                strokes.push((x, baseline + (m.descent * 0.5).max(1.0), line.width));
            }
            if decorations.overline {
                strokes.push((x, baseline - m.ascent, line.width));
            }
            if decorations.line_through {
                strokes.push((x, baseline - font_size * 0.28, line.width));
            }
        }
    }
    Some(PdfLine { font_size, color, glyphs, strokes })
}

/// Bake page-space pdf ops into a band's local coordinates (band origin
/// subtraction) — the writer then needs no per-band offset.
pub(crate) fn pdf_ops_translate(ops: &mut [PdfOp], dx: f32, dy: f32) {
    for op in ops.iter_mut() {
        match op {
            PdfOp::Clip { x, y, .. } => {
                *x -= dx;
                *y -= dy;
            }
            PdfOp::Line(l) => {
                for g in l.glyphs.iter_mut() {
                    g.x -= dx;
                    g.y -= dy;
                }
                for (sx, sy, _) in l.strokes.iter_mut() {
                    *sx -= dx;
                    *sy -= dy;
                }
            }
            PdfOp::PopClip => {}
        }
    }
}

/// Draw a checkable input's native widget (form paint batch): a bordered
/// box — square with a ✓ for checkboxes, circular ring for radios — with
/// the checked state carried in ink. The look is Chrome's neutral light
/// default (white field, gray border, dark mark) rather than any platform
/// accent, so it reads on both light and dark pages. Works in whatever
/// coordinate space `out` is in: the direct path passes page-band coords,
/// the affine path a local scratch at (0, 0).
#[allow(clippy::too_many_arguments)]
fn paint_form_widget(
    out: &mut Canvas,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    widget: super::FormWidget,
    fonts: &FontBook,
    alpha: f32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (radio, checked, range) = match widget {
        super::FormWidget::Checkbox { checked } => (false, checked, None),
        super::FormWidget::Radio { checked } => (true, checked, None),
        super::FormWidget::Range { fraction } => (false, false, Some(fraction)),
    };
    // A slider (blitz#456) has no outer shell: a 4px track spanning the box
    // inset by the thumb radius, a filled leading segment and a 14px thumb
    // — the same neutral gray ramp as the checkables (light track, gray
    // fill, ink thumb) reads on both light and dark pages.
    if let Some(fraction) = range {
        let track = alpha_color([203, 203, 203, 255], alpha);
        let border = alpha_color([118, 118, 118, 255], alpha);
        let ink = alpha_color([26, 26, 26, 255], alpha);
        let thumb_r = 7i64;
        let cy = y + h / 2;
        let tx = x + thumb_r;
        let tw = (w - 2 * thumb_r).max(1);
        let thumb_cx = tx + ((tw as f32 * fraction.clamp(0.0, 1.0)).round() as i64);
        out.fill_rounded_rect(tx, cy - 2, tw, 4, 2.0, track);
        out.fill_rounded_rect(tx, cy - 2, (thumb_cx - tx).max(0), 4, 2.0, border);
        out.fill_rounded_rect(
            thumb_cx - thumb_r,
            cy - thumb_r,
            thumb_r * 2,
            thumb_r * 2,
            thumb_r as f32,
            ink,
        );
        return;
    }
    let border = alpha_color([118, 118, 118, 255], alpha);
    let fill = alpha_color([255, 255, 255, 255], alpha);
    let ink = alpha_color([26, 26, 26, 255], alpha);
    // fill_rounded_rect clamps the radius to half the shorter side, so a
    // radio's w/2 is the full circle; the inset re-fill keeps a 1px ring.
    let radius = if radio { w.min(h) as f32 / 2.0 } else { 2.0 };
    out.fill_rounded_rect(x, y, w, h, radius, border);
    out.fill_rounded_rect(x + 1, y + 1, w - 2, h - 2, (radius - 1.0).max(0.0), fill);
    if !checked {
        return;
    }
    if radio {
        let d = (w.min(h) - 6).max(2);
        out.fill_rounded_rect(
            x + (w - d) / 2,
            y + (h - d) / 2,
            d,
            d,
            d as f32 / 2.0,
            ink,
        );
    } else {
        // The bundled symbol set carries ✓ (the font-fallback batch's
        // coverage tail), and the raster cache (#399) makes the repeat free.
        // Centering goes by the tile's ink bbox, not its full line box —
        // the cramped CJK metrics leave the glyph off-center inside the box.
        let fs = (h as f32 * 0.82).max(6.0);
        let r = fonts.rasterize("✓", fs, false, ink, fs * 1.45, false);
        if let Some((bx0, by0, bx1, by1)) = r.ink_bbox() {
            let iw = (bx1 - bx0 + 1) as i64;
            let ih = (by1 - by0 + 1) as i64;
            let tx = x + (w - iw) / 2 - bx0 as i64;
            let ty = y + (h - ih) / 2 - by0 as i64;
            out.blit_text(&r, tx, ty);
        }
    }
}

/// The closed select's dropdown mark (form paint polish batch): a 7×4
/// solid ▼ centered in the 16px zone at the box's right, in the border
/// gray — hard-edged rows like every other primitive here.
fn paint_select_arrow(out: &mut Canvas, x: i64, y: i64, w: i64, h: i64, alpha: f32) {
    let cx = x + w - 9;
    let cy = y + h / 2;
    let color = alpha_color([118, 118, 118, 255], alpha);
    for (row, width) in [7i64, 5, 3, 1].into_iter().enumerate() {
        out.fill_rect(cx - width / 2, cy - 2 + row as i64, width, 1, color);
    }
}

/// A text-carrying form control's default shell + run layout (form paint
/// polish batch): Chrome's 1px gray ring with a white field (the light
/// button face for buttons), the run inset by the control's 2px padding —
/// vertically centered for single-line controls, top-anchored for textarea
/// — and a select's arrow in its reserved right zone. The shell only
/// paints while `fill` is set (the author styled no background of their
/// own — their bg/border already read as the box). Works in whatever
/// coordinate space `out` is in, like [`paint_form_widget`].
#[allow(clippy::too_many_arguments)]
fn paint_form_control(
    out: &mut Canvas,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    run: Option<&(String, f32, bool, f32, [u8; 4])>,
    form: super::FormRun,
    fill: bool,
    fonts: &FontBook,
    alpha: f32,
    caret: Option<(usize, [u8; 4])>,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    if fill {
        let border = alpha_color([118, 118, 118, 255], alpha);
        let field = alpha_color(
            match form {
                super::FormRun::Button => [239, 239, 239, 255],
                _ => [255, 255, 255, 255],
            },
            alpha,
        );
        out.fill_rounded_rect(x, y, w, h, 2.0, border);
        out.fill_rounded_rect(x + 1, y + 1, w - 2, h - 2, 1.0, field);
    }
    if form == super::FormRun::Select {
        paint_select_arrow(out, x, y, w, h, alpha);
    }
    // Metrics come from the run; the fallback pair only carries a caret
    // whose collect site failed to synthesize a run (the collect path
    // guarantees one whenever a caret is present, so the empty text below
    // keeps the fallback from ever painting).
    let (text, font_size, bold, line_height, color) = match run {
        Some((t, fs, b, lh, c)) => (t.as_str(), *fs, *b, *lh, *c),
        None => ("", 16.0, false, 19.0, [0, 0, 0, 255]),
    };
    // Ink stays inside the field: wrap at the box minus the padding, and
    // the select additionally reserves its arrow zone.
    let wrap_at = match form {
        super::FormRun::Select => (w - 20).max(0),
        super::FormRun::Button => w,
        _ => (w - 4).max(0),
    };
    // The line-box top the tile hangs from: centered for the single-line
    // controls ((h − lh)/2, symmetric overflow when the box runs shorter
    // than the line), 2px below the top edge for textarea. Button labels
    // also center horizontally, at the estimator width the band prefilter
    // uses — close enough to the ink the rasterizer will lay down.
    let (tx, ty) = match form {
        super::FormRun::Button => {
            let est = est_width(text, font_size).min(w as f32);
            (x as f32 + ((w as f32 - est) / 2.0).max(2.0), y as f32 + (h as f32 - line_height) / 2.0)
        }
        super::FormRun::Textarea => (x as f32 + 2.0, y as f32 + 2.0),
        _ => (x as f32 + 2.0, y as f32 + (h as f32 - line_height) / 2.0),
    };
    if !text.trim().is_empty() {
        let r = fonts.rasterize_wrapped(
            text,
            font_size,
            bold,
            alpha_color(color, alpha),
            wrap_at.max(1) as f32,
            line_height,
            false,
            0.0, // control labels carry no inherited word-spacing (v1 boundary)
            None, // control labels never truncate (input text-overflow is a v2 face)
        );
        out.push_clip(x + 1, y + 1, x + w - 1, y + h - 1);
        out.blit_text(&r, tx.round() as i64, (ty + r.top).round() as i64);
        out.pop_clip();
    }
    // Caret (typing-cursor batch): a 1px bar the line height tall, riding
    // the same token/wrap walk the rasterizer lays the run down with, so
    // it stands exactly between the glyphs the offset names — including
    // on an empty value, where the run above paints nothing. Always on
    // (no blink): a screenshot is one instant, and Chrome's 50%-duty
    // blink would make the caret randomly vanish from captures. Chrome
    // colors it caret-color (default: the used color); collect passes
    // the ink so the placeholder gray can never leak into the bar.
    if let Some((off, ink)) = caret {
        if matches!(form, super::FormRun::Input | super::FormRun::Textarea) {
            let tokens = tokens_of(text, font_size, bold, fonts, false, 0.0);
            let lines = greedy_wrap(&tokens, Some(wrap_at.max(1) as f32));
            // tokens_of tokenizes the TRIMMED text: map the offset into
            // trimmed coordinates. A caret inside the leading whitespace
            // pins to the line start — accepted v1 imprecision.
            let leading = text.chars().count() - text.trim_start().chars().count();
            let off = off.saturating_sub(leading);
            // Which wrapped line holds the offset, and the ink width
            // before it on that line: consume whole tokens while their
            // cumulative chars stay at/below the offset; a mid-token
            // landing takes the proportional slice of that token's
            // advance. Walking painted tokens only (greedy_wrap drops
            // whitespace before breaks) keeps the caret glued to the
            // glyphs actually on screen.
            let mut line = 0usize;
            let mut x_before = 0.0f32;
            let mut cum = 0usize;
            'lines: for (li, l) in lines.iter().enumerate() {
                line = li;
                x_before = 0.0;
                for &ti in &l.token_idx {
                    let tok = &tokens[ti];
                    let n = tok.text.chars().count();
                    if cum + n <= off {
                        x_before += tok.width;
                        cum += n;
                    } else {
                        if off > cum {
                            x_before += tok.width * (off - cum) as f32 / n as f32;
                        }
                        break 'lines;
                    }
                }
            }
            // Input never wraps in Chrome (the text scrolls); the engine
            // has no text scroll, so a caret that wrapped past the first
            // line clamps to the right padding — the deterministic
            // stand-in. Every bar clamps into the field regardless.
            let bar_x = if form == super::FormRun::Input && line > 0 {
                x as f32 + w as f32 - 3.0
            } else {
                tx + x_before
            }
            .round();
            let lo = x as f32 + 2.0;
            let hi = (x as f32 + w as f32 - 3.0).max(lo);
            let bar_x = bar_x.max(lo).min(hi) as i64;
            let bar_y = (ty + line as f32 * line_height).round() as i64;
            let bar_h = line_height.ceil().max(1.0) as i64;
            out.push_clip(x + 1, y + 1, x + w - 1, y + h - 1);
            out.fill_rect(bar_x, bar_y, 1, bar_h, alpha_color(ink, alpha));
            out.pop_clip();
        }
    }
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
    // Whether a text tile's ink can reach the band. Line count comes from
    // the same wrap model rasterize_wrapped uses; `top` can lift ink above
    // the line-box top by up to a line's leading, so the top edge gets a
    // full line of slack.
    #[allow(clippy::too_many_arguments)]
    fn text_reaches_band(
        y: f32,
        text: &str,
        font_size: f32,
        wrap_at: f32,
        line_height: f32,
        dy: f32,
        band_h: i64,
        pad: f32,
    ) -> bool {
        let wrap = wrap_at.max(1.0);
        let lines = (est_width(text, font_size) / wrap).ceil().max(1.0);
        let top = y - line_height;
        let bottom = y + lines * line_height;
        bottom + pad > dy && top - pad < dy + band_h as f32
    }

    for item in items {
        match item {
            PaintItem::SetXf { xf } => {
                // The stored map is in page space; the band shift folds in
                // here — the canvas-space total is T(−dx,−dy)∘M, whose e/f
                // are exactly M.e − dx / M.f − dy.
                out.push_xf([
                    xf[0] as f64,
                    xf[1] as f64,
                    xf[2] as f64,
                    xf[3] as f64,
                    xf[4] as f64 - dx as f64,
                    xf[5] as f64 - dy as f64,
                ]);
            }
            PaintItem::SetXfCanvas => {
                // Canvas-space emission (the inline-band splice): replace
                // the open map with the band shift alone — identity in the
                // full-page execute — so the bracket's transform cancels.
                out.push_xf_canvas([1.0, 0.0, 0.0, 1.0, -dx as f64, -dy as f64]);
            }
            PaintItem::ClearXf => {
                out.pop_xf();
            }
            PaintItem::Clip { rect } => {
                if out.xf().is_some() {
                    out.push_clip_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        [(0.0, 0.0); 4],
                    );
                } else {
                    out.push_clip(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        (rect.x + rect.width - dx).round() as i64,
                        (rect.y + rect.height - dy).round() as i64,
                    );
                }
            }
            PaintItem::ClipRounded { rect, radii } => {
                if out.xf().is_some() {
                    out.push_clip_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        *radii,
                    );
                } else {
                    out.push_rounded_clip(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        (rect.x + rect.width - dx).round() as i64,
                        (rect.y + rect.height - dy).round() as i64,
                        *radii,
                    );
                }
            }
            PaintItem::PopClip => {
                out.pop_clip();
            }
            PaintItem::BgCorner { rect, color, radii, .. } => {
                if out.xf().is_some() {
                    out.fill_shape_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        *radii,
                        *color,
                    );
                } else {
                    out.fill_corner_rect(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        rect.width.round() as i64,
                        rect.height.round() as i64,
                        *radii,
                        *color,
                    );
                }
            }
            PaintItem::BoxShadow { rect, color, radii, dx: sdx, dy: sdy, blur, spread, inset } => {
                if out.xf().is_some() {
                    out.fill_shadow_affine(rect, *color, *radii, *sdx, *sdy, *blur, *spread, *inset);
                } else {
                    // No bracket: the collect walk's paint translation still
                    // applies (the affine path folds it into the matrix).
                    let moved = Rect {
                        x: rect.x - dx,
                        y: rect.y - dy,
                        width: rect.width,
                        height: rect.height,
                    };
                    out.fill_box_shadow(&moved, *color, *radii, *sdx, *sdy, *blur, *spread, *inset);
                }
            }
            PaintItem::Bg { rect, color, radius, .. } => {
                if out.xf().is_some() {
                    out.fill_shape_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        [(*radius, *radius); 4],
                        *color,
                    );
                } else {
                    out.fill_rounded_rect(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        rect.width.round() as i64,
                        rect.height.round() as i64,
                        *radius,
                        *color,
                    );
                }
            }
            PaintItem::BgGradient { rect, stops, css_deg, radii } => {
                if out.xf().is_some() {
                    out.fill_gradient_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        stops,
                        *css_deg,
                        *radii,
                    );
                } else {
                    out.fill_gradient(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        rect.width.round() as i64,
                        rect.height.round() as i64,
                        stops,
                        *css_deg,
                        *radii,
                    );
                }
            }
            PaintItem::Border { rect, widths, color, radii } => {
                if out.xf().is_some() {
                    out.border_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        *widths,
                        *radii,
                        *color,
                    );
                } else if radii.iter().all(|r| r.0 <= 0.0 && r.1 <= 0.0) {
                    // Four bands, square corners: top/bottom span the full
                    // border-box width (they own the corners), left/right inset
                    // by the top/bottom widths — the classic rectangular-border
                    // paint browsers produce with radius 0 (the historical
                    // fast path, bit-for-bit).
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
                } else {
                    // Rounded ring: outer rounded box minus the widths-inset
                    // inner rounded box — Chrome's border shape when
                    // border-radius meets a border.
                    out.fill_border_ring(
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                        rect.width.round() as i64,
                        rect.height.round() as i64,
                        *widths,
                        *radii,
                        *color,
                    );
                }
            }
            PaintItem::Image { rect, paint_rect, image, alpha } => {
                if out.xf().is_some() {
                    // Replaced content still clips to the element box — the
                    // clip rides the same bracket.
                    out.push_clip_affine(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                        [(0.0, 0.0); 4],
                    );
                    out.blit_image_affine(
                        image,
                        paint_rect.x as f64,
                        paint_rect.y as f64,
                        paint_rect.width as f64,
                        paint_rect.height as f64,
                        *alpha,
                    );
                    out.pop_clip();
                } else {
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
            }
            PaintItem::Replaced { rect, alt, fill_placeholder, widget, form, alpha, caret } => {
                if out.xf().is_some() {
                    // Rasterize the placeholder + alt into a transparent
                    // LOCAL scratch at raw metrics (the bracket maps the
                    // tile; alt text must not bake the scale in), then blit
                    // it through the map.
                    let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                    if w <= 0 || h <= 0 {
                        continue;
                    }
                    let (bx0, by0, bx1, by1) = out.mapped_xf_bounds(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                    );
                    if bx1 <= 0 || by1 <= 0 || bx0 >= out.width as i64 || by0 >= out.height as i64 {
                        continue;
                    }
                    let (w, h) = (w as usize, h as usize);
                    let mut scratch = Canvas::new_transparent(w, h);
                    if let Some(widget) = widget {
                        paint_form_widget(
                            &mut scratch, 0, 0, w as i64, h as i64, *widget, fonts, *alpha,
                        );
                    } else if let Some(form) = form {
                        paint_form_control(
                            &mut scratch, 0, 0, w as i64, h as i64, alt.as_ref(), *form,
                            *fill_placeholder, fonts, *alpha, *caret,
                        );
                    } else {
                        if *fill_placeholder {
                            scratch.fill_rect(0, 0, w as i64, h as i64, alpha_color([224, 224, 224, 255], *alpha));
                        }
                        if let Some((text, font_size, bold, line_height, color)) = alt {
                            if !text.trim().is_empty() {
                                // Ink clips to the box (batch 6e), now in the
                                // scratch's own coordinates.
                                scratch.push_clip(0, 0, w as i64, h as i64);
                                let r = fonts.rasterize_wrapped(
                                    text,
                                    *font_size,
                                    *bold,
                                    alpha_color(*color, *alpha),
                                    w as f32,
                                    *line_height,
                                    false,
                                    0.0,
                                    None,
                                );
                                scratch.blit_text(&r, 0, r.top.round() as i64);
                                scratch.pop_clip();
                            }
                        }
                    }
                    out.blit_rgba_affine(&scratch.data, w, h, rect.x as f64, rect.y as f64);
                } else {
                    if rect.y + rect.height <= dy || rect.y >= dy + out.height as f32 {
                        continue;
                    }
                    let (x, y) = (
                        (rect.x - dx).round() as i64,
                        (rect.y - dy).round() as i64,
                    );
                    let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                    if let Some(widget) = widget {
                        if w > 0 && h > 0 {
                            paint_form_widget(out, x, y, w, h, *widget, fonts, *alpha);
                        }
                    } else if let Some(form) = form {
                        paint_form_control(
                            out, x, y, w, h, alt.as_ref(), *form, *fill_placeholder, fonts, *alpha,
                            *caret,
                        );
                    } else {
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
                                    false,
                                    0.0,
                                    None,
                                );
                                out.blit_text(&r, x, (y as f32 + r.top).round() as i64);
                                out.pop_clip();
                            }
                        }
                    }
                }
            }
            PaintItem::Svg { rect, render, alpha } => {
                if out.xf().is_some() {
                    let (w, h) = (rect.width.round() as i64, rect.height.round() as i64);
                    if w <= 0 || h <= 0 {
                        continue;
                    }
                    let (bx0, by0, bx1, by1) = out.mapped_xf_bounds(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                    );
                    if bx1 <= 0 || by1 <= 0 || bx0 >= out.width as i64 || by0 >= out.height as i64 {
                        continue;
                    }
                    // Rasterize the subtree into a transparent LOCAL scratch
                    // (dx/dy = the local origin lands it at (0, 0)), then
                    // blit through the bracket.
                    let (w, h) = (w as usize, h as usize);
                    let mut scratch = Canvas::new_transparent(w, h);
                    super::svg::paint_svg(render, rect, fonts, &mut scratch, rect.x, rect.y, *alpha);
                    out.blit_rgba_affine(&scratch.data, w, h, rect.x as f64, rect.y as f64);
                } else {
                    // Same band prefilter as Replaced: the svg painter clips to
                    // the element box anyway, this just skips rasterizing an
                    // off-band subtree.
                    if rect.y + rect.height <= dy || rect.y >= dy + out.height as f32 {
                        continue;
                    }
                    super::svg::paint_svg(render, rect, fonts, out, dx, dy, *alpha);
                }
            }
            PaintItem::Text { text, font_size, bold, color, line_height, x, y, wrap_at, gradient, decorations, mono, word_spacing, truncate_at, tokens, text_shadow } => {
                // background-clip: text: the fill color is ignored entirely
                // (CSS paints the background through the glyphs; the
                // transparent-text-fill half of the idiom is free by
                // construction) — rasterize an opaque-white mask and rewrite
                // each covered pixel with the gradient sampled at its own
                // position. Both blit paths stay untouched: the band path's
                // page coords and the bracket's local coords are exactly the
                // space `area` was captured in, so the gradient rides the
                // affine through rotations too.
                let fill = if gradient.is_some() { [255, 255, 255, 255] } else { *color };
                // Shadow extent (blur feather + offset) for prefilter
                // widening; shadows ride under the glyphs, first-declared
                // layer on top (blitz#271 family).
                let shadow_pad = text_shadow.as_ref().and_then(|s| {
                    s.iter()
                        .map(|sh| sh.blur.ceil().max(sh.dx.abs()).max(sh.dy.abs()) + 1.0)
                        .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |m| m.max(v))))
                });
                if out.xf().is_some() {
                    // Rasterize at RAW local metrics — the bracket maps the
                    // tile, so no scale folds into font metrics — prefiltered
                    // by the mapped bbox of the same estimated tile box the
                    // band check below uses, widened by the shadow extent so
                    // an offset/blur reaching the canvas isn't skipped.
                    let pad = shadow_pad.unwrap_or(0.0) as i64;
                    let est_w = est_width(text, *font_size).max(*wrap_at);
                    let lines = (est_width(text, *font_size) / wrap_at.max(1.0)).ceil().max(1.0);
                    let (bx0, by0, bx1, by1) = out.mapped_xf_bounds(
                        *x as f64,
                        (*y - *line_height) as f64,
                        est_w as f64,
                        ((lines + 1.0) * *line_height) as f64,
                    );
                    if bx1 + pad <= 0 || by1 + pad <= 0 || bx0 - pad >= out.width as i64 || by0 - pad >= out.height as i64 {
                        continue;
                    }
                    if let Some(shadows) = text_shadow {
                        for sh in shadows.iter().rev() {
                            stamp_text_shadow(out, fonts, text, *font_size, *bold, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, tokens.as_deref(), sh, true, (*x + sh.dx) as f64, (*y + sh.dy) as f64);
                        }
                    }
                    let r = fonts.rasterize_wrapped_with(text, *font_size, *bold, fill, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, tokens.clone());
                    // Gradient recolor rewrites pixels in place — the cache
                    // hands out Arcs, so that path clones first (#399).
                    let mut owned;
                    let r = if let Some(g) = gradient {
                        owned = (*r).clone();
                        recolor_gradient_text(&mut owned, *x, *y, g);
                        &owned
                    } else {
                        &r
                    };
                    out.blit_rgba_affine(&r.data, r.width, r.height, *x as f64, (*y + r.top) as f64);
                    paint_text_decorations(out, fonts, text, *font_size, *bold, *color, *line_height, *x, *y, *wrap_at, *decorations, *mono, *word_spacing, *truncate_at, tokens.as_deref(), 0.0, 0.0);
                } else {
                    let pad = shadow_pad.unwrap_or(0.0);
                    if !text_reaches_band(*y, text, *font_size, *wrap_at, *line_height, dy, out.height as i64, pad) {
                        continue;
                    }
                    if let Some(shadows) = text_shadow {
                        for sh in shadows.iter().rev() {
                            stamp_text_shadow(out, fonts, text, *font_size, *bold, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, tokens.as_deref(), sh, false, (*x + sh.dx - dx) as f64, (*y + sh.dy - dy) as f64);
                        }
                    }
                    let r = fonts.rasterize_wrapped_with(text, *font_size, *bold, fill, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, tokens.clone());
                    let mut owned;
                    let r = if let Some(g) = gradient {
                        owned = (*r).clone();
                        recolor_gradient_text(&mut owned, *x, *y, g);
                        &owned
                    } else {
                        &r
                    };
                    // Tile row 0 sits `top` px above the leaf's line-box top.
                    out.blit_text(r, (x - dx).round() as i64, (y - dy + r.top).round() as i64);
                    paint_text_decorations(out, fonts, text, *font_size, *bold, *color, *line_height, *x, *y, *wrap_at, *decorations, *mono, *word_spacing, *truncate_at, tokens.as_deref(), dx, dy);
                }
            }
        }
    }
}

/// One text-shadow layer stamped UNDER the glyphs (blitz#271 family):
/// re-rasterize the run in the layer color (the RasterCache keys on color,
/// so shadow layers memo independently), box-blur its alpha when the layer
/// asks for a blur, and blit at the offset. `affine` selects the
/// transformed blit (page space; the open bracket folds dx/dy) vs the
/// band-space rounded blit.
#[allow(clippy::too_many_arguments)]
fn stamp_text_shadow(
    out: &mut Canvas,
    fonts: &FontBook,
    text: &str,
    font_size: f32,
    bold: bool,
    wrap_at: f32,
    line_height: f32,
    mono: bool,
    word_spacing: f32,
    truncate_at: Option<f32>,
    tokens: Option<&[Token]>,
    sh: &TextShadow,
    affine: bool,
    ox: f64,
    oy: f64,
) {
    let r = fonts.rasterize_wrapped_with(
        text,
        font_size,
        bold,
        [sh.color.0, sh.color.1, sh.color.2, sh.color.3],
        wrap_at,
        line_height,
        mono,
        word_spacing,
        truncate_at,
        tokens.map(std::rc::Rc::from),
    );
    if sh.blur <= 0.0 {
        if affine {
            out.blit_rgba_affine(&r.data, r.width, r.height, ox, oy + r.top as f64);
        } else {
            out.blit_text(&r, ox.round() as i64, (oy + r.top as f64).round() as i64);
        }
        return;
    }
    // Soft layer: blur the raster's ALPHA channel in place of a renderer
    // blur filter (upstream blitz#271 blocks blur on renderer support; the
    // per-pixel path here just does it). The padded buffer gives the
    // feather room so the run tile's edge can't clip it.
    let pad = sh.blur.ceil() as usize;
    let radius = ((sh.blur * 0.5).round() as usize).max(1);
    let pw = r.width + pad * 2;
    let ph = r.height + pad * 2;
    let mut alpha = vec![0u8; pw * ph];
    for (row, src_row) in r.data.chunks_exact(r.width * 4).enumerate() {
        for (col, p) in src_row.chunks_exact(4).enumerate() {
            alpha[(row + pad) * pw + col + pad] = p[3];
        }
    }
    for round in 0..2 {
        alpha = box_blur_alpha(&alpha, pw, ph, radius, round == 0);
    }
    let mut data = Vec::with_capacity(pw * ph * 4);
    for a in &alpha {
        data.extend_from_slice(&[sh.color.0, sh.color.1, sh.color.2, *a]);
    }
    let blurred = TextRaster {
        width: pw,
        height: ph,
        baseline: 0.0,
        top: 0.0,
        data,
    };
    // Row 0 of the padded tile is `pad` px above the run tile's row 0,
    // which itself sits `r.top` px above the line box top.
    if affine {
        out.blit_rgba_affine(&blurred.data, blurred.width, blurred.height, ox - pad as f64, oy + r.top as f64 - pad as f64);
    } else {
        out.blit_text(&blurred, (ox - pad as f64).round() as i64, (oy + r.top as f64 - pad as f64).round() as i64);
    }
}

/// One separable box-blur pass (H or V) over an alpha plane with
/// edge-clamped windows. Two rounds ≈ a triangular kernel — the same
/// extent convention as the box-shadow linear feather (support ≈ blur).
fn box_blur_alpha(src: &[u8], w: usize, h: usize, r: usize, horizontal: bool) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    if horizontal {
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let lo = x.saturating_sub(r);
                let hi = (x + r + 1).min(w);
                let mut sum = 0u32;
                for k in lo..hi {
                    sum += src[row + k] as u32;
                }
                out[row + x] = (sum / (hi - lo) as u32) as u8;
            }
        }
    } else {
        for x in 0..w {
            for y in 0..h {
                let lo = y.saturating_sub(r);
                let hi = (y + r + 1).min(h);
                let mut sum = 0u32;
                for k in lo..hi {
                    sum += src[k * w + x] as u32;
                }
                out[y * w + x] = (sum / (hi - lo) as u32) as u8;
            }
        }
    }
    out
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

    /// blitz#841 transform half: a Text item inside a non-diagonal SetXf
    /// bracket carries LOCAL coordinates, so the ink extent fold must map
    /// its ink box through the active map — a rotate(90deg) long line
    /// contributes its length to the VERTICAL extent, not the horizontal
    /// one. `字`×50 at 10px = 500px single-line ink; under [0,1,-1,0,200,0]
    /// (p → (200−y, x)) the (10,20,500,12) box maps to x'∈[168,180],
    /// y'∈[10,510].
    #[test]
    fn ink_extent_maps_bracketed_text_through_the_transform() {
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
            PaintItem::Text {
                text: "字".repeat(50),
                font_size: 10.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 12.0,
                x: 10.0,
                y: 20.0,
                wrap_at: 500.0,
                gradient: None,
                decorations: TextDecorations::default(),
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            },
            PaintItem::ClearXf,
        ];
        let (w, h) = text_ink_extent(&items);
        assert!((w - 180.0).abs() < 0.01, "rotated line must not report its unrotated 510px width, got {w}");
        assert!((h - 510.0).abs() < 0.01, "rotated line's length lands vertically, got {h}");
    }

    /// ClearXf pops the bracket: a text after the close folds through the
    /// identity again (raw x + ink width, the historical behavior).
    #[test]
    fn ink_extent_clearxf_restores_identity() {
        let plain = |x: f32, y: f32| PaintItem::Text {
            text: "字".repeat(50),
            font_size: 10.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 12.0,
            x,
            y,
            wrap_at: 500.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            text_shadow: None,
        };
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
            plain(10.0, 20.0),
            PaintItem::ClearXf,
            plain(10.0, 20.0),
        ];
        let (w, h) = text_ink_extent(&items);
        assert!((w - 510.0).abs() < 0.01, "post-bracket text is unrotated, got {w}");
        assert!((h - 510.0).abs() < 0.01, "the rotated text still owns the vertical max, got {h}");
    }

    /// SetXfCanvas cancels the open map (inline-band splice): text inside it
    /// folds through the identity, like the paint pass treats it.
    #[test]
    fn ink_extent_canvas_bracket_resets_the_map() {
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
            PaintItem::SetXfCanvas,
            PaintItem::Text {
                text: "字".repeat(50),
                font_size: 10.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 12.0,
                x: 10.0,
                y: 20.0,
                wrap_at: 500.0,
                gradient: None,
                decorations: TextDecorations::default(),
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            },
            PaintItem::ClearXf,
            PaintItem::ClearXf,
        ];
        let (w, _h) = text_ink_extent(&items);
        assert!((w - 510.0).abs() < 0.01, "canvas-spliced text ignores the open rotation, got {w}");
    }

    /// Nested brackets compose outer∘inner (the walk's relative-own maps):
    /// translate(100,0) outside rotate(90deg) maps p → (100−y, x).
    #[test]
    fn ink_extent_nested_brackets_compose() {
        let items = vec![
            PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 100.0, 0.0] },
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 0.0, 0.0] },
            PaintItem::Text {
                text: "字".repeat(50),
                font_size: 10.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 12.0,
                x: 10.0,
                y: 20.0,
                wrap_at: 500.0,
                gradient: None,
                decorations: TextDecorations::default(),
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            },
            PaintItem::ClearXf,
            PaintItem::ClearXf,
        ];
        let (w, h) = text_ink_extent(&items);
        assert!((w - 80.0).abs() < 0.01, "T∘R maps the box to x'∈[68,80], got {w}");
        assert!((h - 510.0).abs() < 0.01, "T∘R keeps the length vertical, got {h}");
    }

    /// background-clip: text (gradient-text batch): a Text item carrying a
    /// TextGradient samples the gradient at each glyph pixel's own position —
    /// a to-right gradient over the glyph band runs red on the left half of
    /// the INK and blue on the right — and ignores its fill color entirely.
    #[test]
    fn gradient_text_samples_gradient_at_glyph_positions() {
        let items = vec![PaintItem::Text {
            text: "mmmmm".into(),
            font_size: 20.0,
            bold: true,
            // Would be black ink without the gradient: proves the fill color
            // steps aside when the gradient rides the item.
            color: [0, 0, 0, 255],
            line_height: 24.0,
            x: 10.0,
            y: 4.0,
            wrap_at: 400.0,
            gradient: Some(TextGradient {
                // 90deg = to right across [10, 110): t < 0.5 red, t > 0.5
                // blue, hard switchover at the box center x = 60.
                area: crate::diting_layout::Rect { x: 10.0, y: 0.0, width: 100.0, height: 32.0 },
                stops: vec![(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])],
                css_deg: 90.0,
            }),
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            text_shadow: None,
        }];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 40, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);

        let mut reds: Vec<usize> = Vec::new();
        let mut blues: Vec<usize> = Vec::new();
        for y in 0..c.height {
            for x in 0..c.width {
                let [r, g, b, a] = px(&c, x, y);
                if a < 64 {
                    continue;
                }
                if r > 120 && b < 120 && g < 120 {
                    reds.push(x);
                } else if b > 120 && r < 120 && g < 120 {
                    blues.push(x);
                }
            }
        }
        assert!(!reds.is_empty() && !blues.is_empty(), "both gradient halves must paint through the glyphs");
        assert!(
            *reds.iter().max().unwrap() < 60,
            "strong red must stay left of the gradient's midpoint; max red x = {}",
            reds.iter().max().unwrap()
        );
        assert!(
            *blues.iter().min().unwrap() >= 60,
            "strong blue must start right of the midpoint; min blue x = {}",
            blues.iter().min().unwrap()
        );
    }

    /// Axis-aligned gradients land on the right walls: 180° (the CSS
    /// default, to bottom) paints red top / blue bottom, 0° flips, 90°
    /// (to right) runs left→right. Plateau stops keep the sampled pixels
    /// on the exact end colors.
    #[test]
    fn gradient_axis_directions() {
        let stops = vec![
            (0.0, [255, 0, 0, 255]),
            (0.2, [255, 0, 0, 255]),
            (0.8, [0, 0, 255, 255]),
            (1.0, [0, 0, 255, 255]),
        ];
        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.fill_gradient(0, 0, 10, 10, &stops, 180.0, [(0.0, 0.0); 4]);
        assert_eq!(px(&c, 5, 0), [255, 0, 0, 255], "180°: top row at t=0.05 is red");
        assert_eq!(px(&c, 5, 9), [0, 0, 255, 255], "180°: bottom row at t=0.95 is blue");
        assert_eq!(px(&c, 0, 0), px(&c, 9, 0), "180°: rows are column-invariant");

        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.fill_gradient(0, 0, 10, 10, &stops, 0.0, [(0.0, 0.0); 4]);
        assert_eq!(px(&c, 5, 0), [0, 0, 255, 255], "0° (to top): top is blue");
        assert_eq!(px(&c, 5, 9), [255, 0, 0, 255], "0°: bottom is red");

        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.fill_gradient(0, 0, 10, 10, &stops, 90.0, [(0.0, 0.0); 4]);
        assert_eq!(px(&c, 0, 5), [255, 0, 0, 255], "90° (to right): left is red");
        assert_eq!(px(&c, 9, 5), [0, 0, 255, 255], "90°: right is blue");
    }

    /// 135° runs corner to corner: TL at t≈0, BR at t≈1, and the two
    /// off-diagonal corners project to the gradient-line center (t=0.5,
    /// the mid lerp of the plateau ramp).
    #[test]
    fn gradient_135deg_diagonal() {
        let stops = vec![
            (0.0, [255, 0, 0, 255]),
            (0.2, [255, 0, 0, 255]),
            (0.8, [0, 0, 255, 255]),
            (1.0, [0, 0, 255, 255]),
        ];
        let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
        c.fill_gradient(0, 0, 10, 10, &stops, 135.0, [(0.0, 0.0); 4]);
        assert_eq!(px(&c, 0, 0), [255, 0, 0, 255], "TL projects to t=0.05");
        assert_eq!(px(&c, 9, 9), [0, 0, 255, 255], "BR projects to t=0.95");
        assert_eq!(px(&c, 9, 0), [128, 0, 128, 255], "TR projects to t=0.5");
        assert_eq!(px(&c, 0, 9), [128, 0, 128, 255], "BL projects to t=0.5");
    }

    /// The gradient fill follows the rounded box: a full-circle radius
    /// leaves the corner pixels untouched while the middle paints.
    #[test]
    fn gradient_rounded_clip_follows_box() {
        let stops = vec![(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])];
        let mut c = Canvas::new_filled(12, 12, [255, 255, 255, 255]);
        c.fill_gradient(1, 1, 10, 10, &stops, 180.0, [(5.0, 5.0); 4]);
        assert_eq!(px(&c, 1, 1), [255, 255, 255, 255], "corner pixel stays canvas bg");
        assert_eq!(px(&c, 10, 1), [255, 255, 255, 255], "opposite corner too");
        assert_ne!(px(&c, 6, 6), [255, 255, 255, 255], "middle paints");
    }

    /// Stop interpolation runs premultiplied: a transparent-red → opaque-blue
    /// midpoint keeps blue at full saturation instead of the purple a
    /// straight rgba lerp produces; positions clamp outside the stop list
    /// and equal positions keep the later stop past the hard line.
    #[test]
    fn gradient_stop_interpolation_is_premultiplied() {
        assert_eq!(lerp_premultiplied([255, 0, 0, 0], [0, 0, 255, 255], 0.5), [0, 0, 255, 128]);
        assert_eq!(lerp_premultiplied([10, 20, 30, 255], [10, 20, 30, 255], 0.5), [10, 20, 30, 255]);
        let stops = vec![(0.25, [255, 0, 0, 0]), (0.75, [0, 0, 255, 255])];
        assert_eq!(gradient_stop_color(&stops, 0.0), [255, 0, 0, 0], "below clamps to first");
        assert_eq!(gradient_stop_color(&stops, 1.0), [0, 0, 255, 255], "above clamps to last");
        // t=0.5 sits mid-ramp: k=0.5, the premultiplied midpoint again.
        assert_eq!(gradient_stop_color(&stops, 0.5), [0, 0, 255, 128]);
        let hard = vec![(0.0, [1, 2, 3, 255]), (0.5, [4, 5, 6, 255]), (0.5, [7, 8, 9, 255]), (1.0, [1, 1, 1, 255])];
        assert_eq!(gradient_stop_color(&hard, 0.5), [7, 8, 9, 255], "equal positions: later stop wins past the hard line");
        assert_eq!(gradient_stop_color(&hard, 0.49), [4, 5, 6, 255], "just before the hard line is the earlier stop");
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
            PaintItem::Border { rect: super::super::Rect { x: 5.0, y: 5.0, width: 30.0, height: 20.0 }, widths: [2.0, 3.0, 4.0, 1.0], color: [200, 40, 40, 255], radii: [(0.0, 0.0); 4] },
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
                widget: None,
                form: None,
                caret: None,
            },
            PaintItem::Text { text: "hello".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 4.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, text_shadow: None },
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut full = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut full);
        let mut band = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
        execute_band(&items, &fonts, &mut band, 0.0, 0.0);
        assert_eq!(full.data, band.data, "dy=0 band paint equals execute");
    }

    /// text-decoration paint (#419): an underlined run adds a stroke strictly
    /// below the glyph ink, line-through/overline add their own bands, and an
    /// empty decoration set paints nothing extra.
    #[test]
    fn text_decorations_paint_line_bands() {
        let fonts = crate::diting_fonts::font_book();
        let paint = |decorations| {
            let items = vec![PaintItem::Text {
                text: "mmmm".into(),
                font_size: 16.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 20.0,
                x: 2.0,
                y: 4.0,
                wrap_at: 400.0,
                gradient: None,
                decorations,
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            }];
            let mut c = Canvas::new_filled(80, 32, [255, 255, 255, 255]);
            execute(&items, &fonts, &mut c);
            c
        };
        let ink_rows = |c: &Canvas| -> Vec<usize> {
            (0..c.height)
                .map(|y| (0..c.width).filter(|&x| px(c, x, y)[3] > 0 && px(c, x, y)[0] < 128).count())
                .collect()
        };
        let base = ink_rows(&paint(TextDecorations::default()));
        let base_total: usize = base.iter().sum();
        let base_bottom = (0..32).rev().find(|&y| base[y] > 0).unwrap();
        let under = ink_rows(&paint(TextDecorations { underline: true, ..Default::default() }));
        assert!(under.iter().sum::<usize>() > base_total, "underline adds ink");
        // "mmmm" has no descenders: the underline sits strictly below the
        // glyph ink's bottom row.
        let under_bottom = (0..32).rev().find(|&y| under[y] > base[y]).unwrap();
        assert!(under_bottom > base_bottom, "underline ink below glyph bottom {base_bottom}, got {under_bottom}");
        for d in [
            TextDecorations { line_through: true, ..Default::default() },
            TextDecorations { overline: true, ..Default::default() },
            TextDecorations { underline: true, line_through: true, ..Default::default() },
        ] {
            let rows = ink_rows(&paint(d));
            assert!(rows.iter().sum::<usize>() > base_total, "decoration {d:?} adds ink");
        }
        // The xf-bracket path paints the same strokes through the affine.
        let items = vec![
            PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] },
            PaintItem::Text {
                text: "mmmm".into(),
                font_size: 16.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 20.0,
                x: 2.0,
                y: 4.0,
                wrap_at: 400.0,
                gradient: None,
                decorations: TextDecorations { underline: true, ..Default::default() },
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            },
            PaintItem::ClearXf,
        ];
        let mut c = Canvas::new_filled(80, 32, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        let rows = ink_rows(&c);
        assert!(rows.iter().sum::<usize>() > base_total, "underline paints under identity xf");
    }

    /// Native form widgets (form paint batch): a checked checkbox draws the
    /// ✓ ink inside a white field ringed by the gray border; unchecked
    /// leaves the interior empty; a checked radio carries the center dot.
    /// The direct band path and the transform-bracket scratch path paint
    /// the same widget (the bracket just maps the same local tile).
    #[test]
    fn form_widgets_paint_checked_state() {
        let fonts = crate::diting_fonts::font_book();
        let box_at = |widget| PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 16.0, height: 16.0 },
            alt: None,
            fill_placeholder: false,
            widget,
            alpha: 1.0,
            form: None,
            caret: None,
        };
        // Interior pixel classes: field is white, border gray, ink near-black.
        let field = [255, 255, 255, 255];
        let border = [118, 118, 118, 255];

        // Checked checkbox: ink in the middle, white field around it.
        let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
        execute(&[box_at(Some(super::super::FormWidget::Checkbox { checked: true }))], &fonts, &mut c);
        assert_eq!(px(&c, 12, 4), border, "top border band");
        assert_eq!(px(&c, 6, 6), field, "field interior inside the border ring");
        // Ink = all channels dark (the green canvas bg would sneak past a
        // red-channel-only probe; the gray border sits above 80).
        let ink = c.data.chunks_exact(4).filter(|p| p[0] < 80 && p[1] < 80 && p[2] < 80 && p[3] > 200).count();
        assert!(ink > 4, "the ✓ must leave dark ink; got {ink} px");

        // Unchecked: same box, no ink anywhere.
        let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
        execute(&[box_at(Some(super::super::FormWidget::Checkbox { checked: false }))], &fonts, &mut c);
        assert_eq!(px(&c, 6, 6), field, "field still paints");
        let ink = c.data.chunks_exact(4).filter(|p| p[0] < 80 && p[1] < 80 && p[2] < 80 && p[3] > 200).count();
        assert_eq!(ink, 0, "unchecked must carry no ink");

        // Checked radio: center dot, white ring field between dot and border.
        let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
        execute(&[box_at(Some(super::super::FormWidget::Radio { checked: true }))], &fonts, &mut c);
        assert_eq!(px(&c, 12, 4), border, "circle's top border pixel");
        assert_eq!(px(&c, 12, 12), [26, 26, 26, 255], "center dot");
        // Between the dot and the ring: the white field (dot r=5, white
        // circle r=7, border r=8 — all centered (12,12) — so (6,12), at
        // distance 5.5, is the 2px field band).
        assert_eq!(px(&c, 6, 12), field, "field band between dot and ring");

        // Through a transform bracket: the widget rasterizes into the local
        // scratch and blits through the map — a 180° flip keeps every pixel
        // class, just mirrored, so the same probes hold after flipping x/y.
        let items = vec![
            PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 24.0, 24.0] },
            box_at(Some(super::super::FormWidget::Radio { checked: true })),
            PaintItem::ClearXf,
        ];
        let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
        execute(&items, &fonts, &mut c);
        assert_eq!(px(&c, 12, 12), [26, 26, 26, 255], "center dot survives the bracket (invariant point)");
        assert_eq!(px(&c, 6, 12), field, "field band rides the bracket");
    }

    /// A range slider (blitz#456) paints a 4px track spanning the
    /// thumb-inset box, the leading segment in the fill gray, and a round
    /// thumb parked at the value's fraction — no outer shell, no text.
    #[test]
    fn form_widget_paint_range_slider() {
        let fonts = crate::diting_fonts::font_book();
        let track = [203, 203, 203, 255];
        let fill = [118, 118, 118, 255];
        let ink = [26, 26, 26, 255];
        let range = |fraction| PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 16.0 },
            alt: None,
            fill_placeholder: false,
            widget: Some(super::super::FormWidget::Range { fraction }),
            alpha: 1.0,
            form: None,
            caret: None,
        };
        // Track x ∈ [11, 117), cy = 12; fraction 0.25 parks the thumb
        // center at 11 + round(106 × 0.25) = 38.
        let mut c = Canvas::new_filled(128, 24, [0, 255, 0, 255]);
        execute(&[range(0.25)], &fonts, &mut c);
        assert_eq!(px(&c, 20, 12), fill, "leading segment behind the thumb");
        assert_eq!(px(&c, 38, 12), ink, "thumb center");
        assert_eq!(px(&c, 100, 12), track, "trailing track");

        // Fraction 0: thumb parked at the start, nothing filled.
        let mut c = Canvas::new_filled(128, 24, [0, 255, 0, 255]);
        execute(&[range(0.0)], &fonts, &mut c);
        assert_eq!(px(&c, 11, 12), ink, "thumb at the track start");
        assert_eq!(px(&c, 38, 12), track, "no fill segment ahead");
        assert_eq!(px(&c, 100, 12), track, "trailing track");
    }

    /// Text-run layout for the text-carrying form controls (form paint
    /// polish batch): the default shell is a 1px gray ring with a white
    /// field (the light button face on buttons), the run insets 2px and
    /// centers vertically for single-line controls, textarea stays
    /// top-anchored, and a select reserves+pains its dropdown arrow at the
    /// right. An authored background (fill=false) drops the shell but keeps
    /// the run layout, and an empty control paints the bare shell.
    #[test]
    fn form_controls_pad_center_and_arrow() {
        let fonts = crate::diting_fonts::font_book();
        let run = |text: &str| Some((text.to_string(), 16.0, false, 19.0, [0u8, 0, 0, 255]));
        let ctrl = |form, alt, fill| PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 24.0 },
            alt,
            fill_placeholder: fill,
            widget: None,
            alpha: 1.0,
            form,
            caret: None,
        };
        // Ink bbox over the whole canvas, three channels dark (the green bg
        // and the gray ring/arrow both sit at or above 80).
        let ink_bbox = |c: &Canvas| {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..c.height {
                for x in 0..c.width {
                    let [r, g, bl, _] = px(c, x, y);
                    if r < 80 && g < 80 && bl < 80 {
                        b = Some(match b {
                            None => (x, y, x, y),
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                        });
                    }
                }
            }
            b
        };

        // Text input: ring + white field, run padded in and centered.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[ctrl(Some(super::super::FormRun::Input), run("abcd"), true)], &fonts, &mut c);
        assert_eq!(px(&c, 4, 15), [118, 118, 118, 255], "left ring band");
        assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "white field inside the ring");
        let (x0, y0, _x1, y1) = ink_bbox(&c).expect("input run ink");
        assert!(x0 >= 6, "run starts at least 2px inside the box (x0={x0})");
        assert!(y0 > 5 && y1 < 27, "run clear of the ring bands (y={y0}..{y1})");
        let cy = (y0 + y1) as f32 / 2.0;
        assert!((13.0..=19.0).contains(&cy), "run vertically centered on 16 (cy={cy})");

        // Authored background: no shell (the canvas shows through), run keeps
        // its layout.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[ctrl(Some(super::super::FormRun::Input), run("abcd"), false)], &fonts, &mut c);
        assert_eq!(px(&c, 10, 10), [0, 255, 0, 255], "no shell without the default look");
        assert!(ink_bbox(&c).is_some(), "the run still paints");

        // Empty control: bare shell, no ink.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[ctrl(Some(super::super::FormRun::Input), None, true)], &fonts, &mut c);
        assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "empty input keeps its field");
        assert!(ink_bbox(&c).is_none(), "no run, no ink");

        // Button: light button-face field, label centered horizontally.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[ctrl(Some(super::super::FormRun::Button), run("Go"), true)], &fonts, &mut c);
        assert_eq!(px(&c, 10, 10), [239, 239, 239, 255], "button face");
        let (x0, _y0, x1, _y1) = ink_bbox(&c).expect("button label ink");
        let cx = (x0 + x1) as f32 / 2.0;
        assert!((60.0..=68.0).contains(&cx), "label centered on the box center 64 (cx={cx})");

        // Textarea: top-anchored — the whole run sits in the top half of a
        // 40px box (a taller box than the others for the claim to bite).
        let mut c = Canvas::new_filled(132, 50, [0, 255, 0, 255]);
        let tall = PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 40.0 },
            alt: run("line"),
            fill_placeholder: true,
            widget: None,
            alpha: 1.0,
            form: Some(super::super::FormRun::Textarea),
            caret: None,
        };
        execute(&[tall], &fonts, &mut c);
        let (_x0, y0, _x1, y1) = ink_bbox(&c).expect("textarea run ink");
        assert!(y1 < 24, "top-anchored run stays in the top half (y={y0}..{y1})");

        // Select: the dropdown arrow in the right zone, the label clear of it.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[ctrl(Some(super::super::FormRun::Select), run("Alpha"), true)], &fonts, &mut c);
        // cx = 4+120−9 = 115, rows 14..17 (widths 7/5/3/1) — (115,15) is the
        // second row's center pixel.
        assert_eq!(px(&c, 115, 15), [118, 118, 118, 255], "dropdown arrow ink");
        assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "select field");
        let (x0, _y0, x1, _y1) = ink_bbox(&c).expect("select label ink");
        assert!(x0 >= 6, "label padded 2px in (x0={x0})");
        assert!(x1 < 110, "label stays clear of the arrow zone (x1={x1})");

        // Through a transform bracket: the same shell rasterizes into the
        // local scratch and blits through the map — 180° flip about the
        // canvas center mirrors every probe.
        let items = vec![
            PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 132.0, 34.0] },
            ctrl(Some(super::super::FormRun::Select), run("Alpha"), true),
            PaintItem::ClearXf,
        ];
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&items, &fonts, &mut c);
        assert_eq!(px(&c, 132 - 115, 34 - 15), [118, 118, 118, 255], "arrow rides the bracket");
        assert_eq!(px(&c, 132 - 10, 34 - 10), [255, 255, 255, 255], "field rides the bracket");
    }

    /// Caret paint (typing-cursor batch): a 1px bar the line height tall at
    /// the offset's token-walk position — offset 0 stands before the first
    /// glyph, an offset past the text clears the glyphs, an empty field with
    /// a caret still paints the bar (collect synthesizes the run), no caret
    /// means no bar, and a textarea offset that wrapped lands on the wrapped
    /// line. The run is painted white so its glyphs vanish into the white
    /// field — every dark pixel below is the bar itself.
    #[test]
    fn caret_paints_bar_at_offset() {
        let fonts = crate::diting_fonts::font_book();
        let white_run =
            |text: &str| Some((text.to_string(), 16.0, false, 19.0, [255u8, 255, 255, 255]));
        let input = |alt, caret| PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 24.0 },
            alt,
            fill_placeholder: true,
            widget: None,
            alpha: 1.0,
            form: Some(super::super::FormRun::Input),
            caret,
        };
        let ink = [0u8, 0, 0, 255];
        // The single dark column and its y extent: the white run keeps the
        // glyphs invisible, the gray ring sits at 118, and the green canvas
        // bg fails the all-channels probe — so only the bar qualifies.
        let bar = |c: &Canvas| -> Option<(usize, usize, usize)> {
            let mut found = None;
            for x in 0..c.width {
                let ys: Vec<usize> = (0..c.height)
                    .filter(|&y| {
                        let [r, g, b, _] = px(c, x, y);
                        r < 80 && g < 80 && b < 80
                    })
                    .collect();
                if !ys.is_empty() {
                    assert!(found.is_none(), "caret bar must be one 1px column (second at x={x})");
                    found = Some((x, *ys.first().unwrap(), *ys.last().unwrap()));
                }
            }
            found
        };

        // Offset 0: the bar stands at the run start, one line tall and
        // vertically centered with the box.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[input(white_run("abcd"), Some((0, ink)))], &fonts, &mut c);
        let (bx, y0, y1) = bar(&c).expect("caret bar at offset 0");
        assert_eq!(bx, 6, "offset 0 stands at the 2px-padded run start");
        assert_eq!(y1 - y0 + 1, 19, "bar is the line height tall");
        let cy = (y0 + y1) as f32 / 2.0;
        assert!((13.0..=19.0).contains(&cy), "bar centered on the box center 16 (cy={cy})");

        // Offset 4 (end of "abcd"): single column well past the glyphs.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[input(white_run("abcd"), Some((4, ink)))], &fonts, &mut c);
        let (bx, _y0, _y1) = bar(&c).expect("caret bar at offset 4");
        assert!(bx > 6 + 16, "offset 4 clears the glyphs (bx={bx})");

        // Empty value with a caret: the bar still paints (collect handed the
        // synthesized run) at the field start.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[input(None, Some((0, ink)))], &fonts, &mut c);
        let (bx, y0, y1) = bar(&c).expect("caret bar on the empty field");
        assert_eq!(bx, 6, "empty field caret at the run start");
        assert_eq!(y1 - y0 + 1, 19, "empty field bar keeps the line height");

        // No caret, white run: nothing dark anywhere.
        let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
        execute(&[input(white_run("abcd"), None)], &fonts, &mut c);
        assert!(bar(&c).is_none(), "no caret, no bar");

        // Textarea whose value wraps: offset 5 (end of "bb") lands on the
        // second line — the bar's top sits a full line height below the
        // first line's.
        let tall = PaintItem::Replaced {
            rect: super::super::Rect { x: 4.0, y: 4.0, width: 40.0, height: 52.0 },
            alt: white_run("aa bb"),
            fill_placeholder: true,
            widget: None,
            alpha: 1.0,
            form: Some(super::super::FormRun::Textarea),
            caret: Some((5, ink)),
        };
        let mut c = Canvas::new_filled(60, 62, [0, 255, 0, 255]);
        execute(&[tall], &fonts, &mut c);
        let (bx, y0, y1) = bar(&c).expect("wrapped caret bar");
        assert_eq!(y1 - y0 + 1, 19, "wrapped bar keeps the line height");
        assert!(y0 >= 24, "offset 5 lands on the wrapped second line (y0={y0})");
        assert!(bx > 6, "the bar sits after the wrapped token (bx={bx})");
    }

    /// A band at dy=100 reproduces exactly rows [100, 180) of the full
    /// render — the viewport-frame contract AginxOS's screencast builds on.
    #[test]
    fn band_capture_equals_window_of_full_render() {
        let items = vec![
            PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 300.0 }, color: [30, 60, 90, 255], radius: 0.0 },
            PaintItem::Bg { rect: super::super::Rect { x: 4.0, y: 120.0, width: 32.0, height: 40.0 }, color: [200, 40, 40, 255], radius: 0.0 },
            PaintItem::Border { rect: super::super::Rect { x: 6.0, y: 240.0, width: 28.0, height: 30.0 }, widths: [3.0, 3.0, 3.0, 3.0], color: [0, 200, 0, 255], radii: [(0.0, 0.0); 4] },
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
            PaintItem::Text { text: "edge".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 96.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, text_shadow: None },
            PaintItem::Text { text: "far".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 500.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, text_shadow: None },
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

    // ---- affine brackets (rotate/skew/matrix paint) ----

    /// rotate(90°) about the 120×60 box's center paints pixel-exactly: the
    /// map x' = 90 − y, y' = x − 30 (pivot (60, 30)) turns the box into a
    /// 60×120 canvas region — integer multiples of 90° must have zero
    /// rasterization slop, exactly like the axis-aligned path.
    #[test]
    fn rotate_90_bracket_pixel_exact() {
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
            PaintItem::Bg {
                rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 60.0 },
                color: [200, 40, 40, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Mapped box: x' ∈ [30, 90), y' ∈ [−30, 90) → on-canvas [30,90)×[0,90).
        assert_eq!(px(&c, 30, 0), [200, 40, 40, 255], "mapped TL corner");
        assert_eq!(px(&c, 89, 89), [200, 40, 40, 255], "mapped BR corner");
        assert_eq!(px(&c, 90, 10), [255, 255, 255, 255], "right of the mapped box");
        assert_eq!(px(&c, 29, 10), [255, 255, 255, 255], "left of the mapped box");
        assert_eq!(px(&c, 60, 90), [255, 255, 255, 255], "below the mapped box");
        // The stack drained: a post-ClearXf fill paints unrotated.
        c.fill_rect(0, 0, 5, 5, [0, 0, 255, 255]);
        assert_eq!(px(&c, 0, 0), [0, 0, 255, 255]);
    }

    /// Nested brackets compose multiplicatively: an inner translate rides
    /// the outer rotation (the canvas total is outer∘inner), and both
    /// clears return to the identity.
    #[test]
    fn nested_brackets_compose() {
        // Outer: rotate(90°) about (60, 30) — x' = 90 − y, y' = x − 30.
        // Inner: translate(10, 0). Total: x' = 90 − y, y' = x − 20.
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
            PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 10.0, 0.0] },
            PaintItem::Bg {
                rect: super::super::Rect { x: 30.0, y: 60.0, width: 10.0, height: 10.0 },
                color: [0, 200, 0, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Total maps (x, y) → (90 − y, x − 20): the local (30,60,10,10) box
        // lands at x' ∈ [20,30), y' ∈ [10,20).
        assert_eq!(px(&c, 20, 10), [0, 200, 0, 255], "nested compose TL");
        assert_eq!(px(&c, 29, 19), [0, 200, 0, 255], "nested compose BR");
        assert_eq!(px(&c, 30, 10), [255, 255, 255, 255], "right edge exclusive");
        assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "bottom edge exclusive");
    }

    /// An affine clip cuts child ink along the rotation: a clip at local
    /// y < 30 under the 90° bracket trims the child background to its
    /// inverse image — and the clip pops cleanly afterwards.
    #[test]
    fn affine_clip_cuts_child_ink() {
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
            PaintItem::Clip {
                rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 30.0 },
            },
            PaintItem::Bg {
                rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 60.0 },
                color: [200, 40, 40, 255],
                radius: 0.0,
            },
            PaintItem::PopClip,
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Clip local y ∈ [0,30) ⇔ canvas x' = 90 − y ∈ (60, 90]: pixel
        // centers pass at columns 60..=89; the bg box caps at x' < 90.
        assert_eq!(px(&c, 60, 40), [200, 40, 40, 255], "inside the rotated window");
        assert_eq!(px(&c, 59, 40), [255, 255, 255, 255], "outside the rotated window");
        assert_eq!(px(&c, 89, 40), [200, 40, 40, 255], "window's far edge");
        assert_eq!(px(&c, 90, 40), [255, 255, 255, 255], "past the bg box");
        // The affine clip drained: a later fill covers the whole canvas.
        c.fill_rect(0, 0, 120, 120, [0, 0, 255, 255]);
        assert_eq!(px(&c, 0, 0), [0, 0, 255, 255]);
    }

    /// Text under a bracket rasterizes at raw local metrics and blits
    /// through the map: a 180° flip moves the ink to the mirror half of
    /// the canvas and none survives at the un-mapped position.
    #[test]
    fn affine_text_paints_through_bracket() {
        // rotate(180°) about (60, 25): x' = 120 − x, y' = 50 − y.
        let items = vec![
            PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 120.0, 50.0] },
            PaintItem::Text {
                text: "flip".into(),
                font_size: 16.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 20.0,
                x: 10.0,
                y: 10.0,
                wrap_at: 200.0,
                gradient: None,
                decorations: TextDecorations::default(),
                mono: false,
                word_spacing: 0.0,
                truncate_at: None,
                tokens: None,
                text_shadow: None,
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 50, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Un-mapped ink would sit at x < 30 (the leaf is at x=10, ~30px wide);
        // the 180° flip about x=60 mirrors it to the right half (x > 60).
        let ink_x: Vec<usize> = (0..c.width)
            .filter(|&x| (0..c.height).any(|y| c.data[(y * c.width + x) * 4] < 128))
            .collect();
        assert!(!ink_x.is_empty(), "text must paint through the bracket");
        assert!(
            ink_x.iter().all(|&x| x > 60),
            "ink must land in the mirrored half: {ink_x:?}"
        );
    }

    /// skewX(45°) — x' = x + y, y' = y — paints pixel-exactly: the sheared
    /// parallelogram puts ink at columns the unskewed box can't reach and
    /// cuts the ones only it occupied (affine residuals batch).
    #[test]
    fn skew_x_bracket_pixel_exact() {
        let items = vec![
            PaintItem::SetXf { xf: [1.0, 0.0, 1.0, 1.0, 0.0, 0.0] },
            PaintItem::Bg {
                rect: super::super::Rect { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
                color: [200, 40, 40, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 60, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Inverse: lx = cx − cy, ly = cy. Local box [10,50)×[10,30).
        assert_eq!(px(&c, 20, 10), [200, 40, 40, 255], "sheared TL edge");
        assert_eq!(px(&c, 55, 20), [200, 40, 40, 255], "right of the unskewed box — only skew reaches");
        assert_eq!(px(&c, 69, 29), [200, 40, 40, 255], "far bottom-right of the parallelogram");
        assert_eq!(px(&c, 30, 29), [255, 255, 255, 255], "lower-left cut away by the shear");
        assert_eq!(px(&c, 60, 10), [255, 255, 255, 255], "top right edge exclusive (lx = 50)");
        assert_eq!(px(&c, 79, 29), [255, 255, 255, 255], "bottom right edge exclusive");
    }

    /// matrix(1, 0.5, 0, 1, 5, 0) — x' = x + 5, y' = 0.5x + y — the general
    /// affine form, pixel-exact on integer-friendly entries.
    #[test]
    fn matrix_bracket_pixel_exact() {
        let items = vec![
            PaintItem::SetXf { xf: [1.0, 0.5, 0.0, 1.0, 5.0, 0.0] },
            PaintItem::Bg {
                rect: super::super::Rect { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
                color: [200, 40, 40, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(60, 50, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Inverse: lx = cx − 5, ly = cy − 0.5·lx. Local box [10,50)×[10,30).
        assert_eq!(px(&c, 16, 16), [200, 40, 40, 255], "mapped TL region");
        assert_eq!(px(&c, 30, 26), [200, 40, 40, 255], "mid box");
        assert_eq!(px(&c, 30, 33), [200, 40, 40, 255], "sheared down — only the matrix reaches");
        assert_eq!(px(&c, 30, 8), [255, 255, 255, 255], "above the sheared top");
        assert_eq!(px(&c, 55, 40), [255, 255, 255, 255], "past the sheared right");
    }

    /// A rounded border through a bracket is the ROUNDED ring rotated, not
    /// the square-cornered one: corners stay cut along the curve, the
    /// widths-inset hole stays open (affine residuals batch ①).
    #[test]
    fn rotated_rounded_border_pixel_exact() {
        // rotate(90°) about (20, 20): (x, y) → (40 − y, x).
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 40.0, 0.0] },
            PaintItem::Border {
                rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 },
                widths: [6.0, 6.0, 6.0, 6.0],
                color: [200, 40, 40, 255],
                radii: [(12.0, 12.0); 4],
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Inverse: lx = 40 − cy, ly = cx.
        assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "the inner hole stays open");
        assert_eq!(px(&c, 20, 5), [200, 40, 40, 255], "straight edge band");
        assert_eq!(px(&c, 33, 6), [200, 40, 40, 255], "corner arc band (outer in, inner out)");
        assert_eq!(px(&c, 37, 2), [255, 255, 255, 255], "outside the rounded corner — square paint would hit");
    }

    /// The axis-aligned twin: nonzero radii switch the border to the rounded
    /// ring — corners cut along the arc, hole open, edges solid. Zero radii
    /// keep the historical four-band paint (covered by the older tests).
    #[test]
    fn rounded_border_ring_axis_aligned() {
        let items = vec![PaintItem::Border {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 },
            widths: [4.0, 4.0, 4.0, 4.0],
            color: [200, 40, 40, 255],
            radii: [(10.0, 10.0); 4],
        }];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "corner cut by the 10px arc");
        assert_eq!(px(&c, 5, 5), [200, 40, 40, 255], "arc band mid-corner");
        assert_eq!(px(&c, 20, 0), [200, 40, 40, 255], "top band");
        assert_eq!(px(&c, 0, 20), [200, 40, 40, 255], "left band");
        assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "hole open");
    }

    /// SetXfCanvas cancels the enclosing bracket: the canvas-space Bg paints
    /// UNROTATED (its rotated image would be off-canvas entirely), and the
    /// matching ClearXf restores the bracket for subsequent local items
    /// (affine residuals batch ②, the inline-band splice's engine).
    #[test]
    fn canvas_bracket_cancels_enclosing_map() {
        // rotate(90°) about (60, 30): x' = 90 − y, y' = x − 30.
        let items = vec![
            PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
            PaintItem::SetXfCanvas,
            PaintItem::Bg {
                rect: super::super::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
                color: [200, 40, 40, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
            PaintItem::Bg {
                rect: super::super::Rect { x: 40.0, y: 40.0, width: 10.0, height: 10.0 },
                color: [0, 0, 200, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
        ];
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        // Rotated, the red box maps to y' ∈ [−30,−20) — off-canvas; painting
        // at (0,0) proves the bracket was cancelled for it.
        assert_eq!(px(&c, 0, 0), [200, 40, 40, 255], "canvas-space bg paints unrotated");
        assert_eq!(px(&c, 9, 9), [200, 40, 40, 255], "canvas-space bg far corner");
        // After the ClearXf the rotation is back: local (40,40,10,10) maps
        // to canvas x' = 90 − y ∈ (40,50], y' = x − 30 ∈ [10,20).
        assert_eq!(px(&c, 41, 11), [0, 0, 200, 255], "bracket restored after ClearXf");
        assert_eq!(px(&c, 45, 15), [0, 0, 200, 255], "restored map far corner");
        assert_eq!(px(&c, 50, 15), [255, 255, 255, 255], "restored map edge exclusive");
    }

    /// text-overflow: ellipsis (blitz#888): the marker is a raster-time
    /// rendering effect. Ink stops at the truncate limit (the overflow is
    /// never painted) while the untruncated run inks well past it, and the
    /// truncated run still carries marker ink near the limit.
    #[test]
    fn ellipsis_truncates_paint_ink_at_the_limit() {
        let fonts = crate::diting_fonts::font_book();
        let paint = |truncate_at: Option<f32>| {
            let items = vec![PaintItem::Text {
                text: "mmmmmmmmmmmmmmmmmmmm".into(),
                font_size: 16.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 20.0,
                x: 2.0,
                y: 4.0,
                wrap_at: 400.0,
                gradient: None,
                decorations: TextDecorations::default(),
                mono: false,
                word_spacing: 0.0,
                truncate_at,
                tokens: None,
                text_shadow: None,
            }];
            let mut c = Canvas::new_filled(400, 32, [255, 255, 255, 255]);
            execute(&items, &fonts, &mut c);
            c
        };
        let right_edge = |c: &Canvas| -> usize {
            (0..c.width)
                .rev()
                .find(|&x| (0..c.height).any(|y| px(c, x, y)[3] > 0 && px(c, x, y)[0] < 128))
                .unwrap_or(0)
        };
        let clipped = right_edge(&paint(Some(60.0)));
        let full = right_edge(&paint(None));
        assert!(
            clipped <= 2 + 62,
            "ink stops at the limit (x=2 + 60), got {clipped}"
        );
        assert!(full > 80, "untruncated run inks past the limit, got {full}");
        assert!(clipped > 30, "marker ink near the limit, got {clipped}");
    }

    /// Paint half of obscura#983: handing the decorations painter the item's
    /// pre-shaped wrap tokens must stroke byte-identically to re-shaping —
    /// the multi-line wrap and the ellipsis truncation cut included.
    #[test]
    fn decorations_pre_shaped_matches_reshaped() {
        let fonts = crate::diting_fonts::font_book();
        let text = "淘宝商品列表页的一段中文文本需要折行处理".repeat(3);
        let deco = TextDecorations { underline: true, line_through: true, ..Default::default() };
        let tokens = tokens_of(&text, 16.0, false, &fonts, false, 0.0);
        let stroke = |pre: Option<&[Token]>, truncate_at: Option<f32>| {
            let mut c = Canvas::new_filled(320, 200, [255, 255, 255, 255]);
            paint_text_decorations(&mut c, &fonts, &text, 16.0, false, [0, 0, 0, 255], 24.0, 2.0, 4.0, 300.0, deco, false, 0.0, truncate_at, pre, 0.0, 0.0);
            c.data
        };
        assert_eq!(stroke(None, None), stroke(Some(&tokens), None), "wrapped: pre-shaped == re-shaped");
        assert_eq!(stroke(None, Some(200.0)), stroke(Some(&tokens), Some(200.0)), "truncated: pre-shaped == re-shaped");
    }

    // ---- box-shadow (blitz#349 family, v1) ----

    /// A zero-blur offset shadow paints hard: visible in the offset band
    /// outside the element, knocked out inside the element's own box even
    /// though the element paints no background of its own.
    #[test]
    fn box_shadow_offset_band_knocked_out_inside() {
        let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
        let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
        c.fill_box_shadow(&rect, [255, 0, 0, 255], [(0.0, 0.0); 4], 5.0, 5.0, 0.0, 0.0, false);
        assert_eq!(px(&c, 20, 20), [255, 0, 0, 255], "offset band right/below");
        assert_eq!(px(&c, 19, 19), [255, 255, 255, 255], "inside the element: knocked out");
        assert_eq!(px(&c, 5, 5), [255, 255, 255, 255], "opposite corner: no shadow");
        assert_eq!(px(&c, 30, 30), [255, 255, 255, 255], "past the offset box: none");
    }

    /// The feather falls off monotonically with distance from the shadow
    /// box edge; under the feather the element interior stays knocked out,
    /// and past blur px nothing paints at all.
    #[test]
    fn box_shadow_blur_falloff_monotonic() {
        let mut c = Canvas::new_filled(60, 30, [255, 255, 255, 255]);
        let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
        c.fill_box_shadow(&rect, [0, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 8.0, 0.0, false);
        let ink = |x: usize, y: usize| 255 - px(&c, x, y)[0];
        assert_eq!(ink(19, 14), 0, "knocked out even under the feather");
        let near = ink(21, 14); // 1.5px past the right edge (edge at 20)
        let far = ink(26, 14); // 6.5px past
        assert!(near > far, "monotonic: {near} > {far}");
        assert!(near < 255 && near > 0, "feather band is partial: {near}");
        assert_eq!(ink(0, 14), 0, "past the feather extent: none");
    }

    /// Inset v2: ink fills between the shadow box edge and the element
    /// edge, hard-clipped to the element box. A 10px element with spread 2
    /// gets a full-ink 2px ring around the shadow box (12..18) and a
    /// hollow center; nothing lands outside the element.
    #[test]
    fn box_shadow_inset_spread_ring_clipped_to_element() {
        let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
        let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
        c.fill_box_shadow(&rect, [255, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 0.0, 2.0, true);
        assert_eq!(px(&c, 10, 14), [255, 0, 0, 255], "element edge: full ink");
        assert_eq!(px(&c, 11, 14), [255, 0, 0, 255], "ring band (0.5 outside the shadow box)");
        assert_eq!(px(&c, 13, 14), [255, 255, 255, 255], "inside the shadow box: hollow");
        assert_eq!(px(&c, 9, 14), [255, 255, 255, 255], "outside the element: hard clip");
        assert_eq!(px(&c, 30, 14), [255, 255, 255, 255], "nowhere past the element");
    }

    /// The inset feather falls inward from the shadow box edge: partial at
    /// the element edge, monotonic down to nothing `blur` px inside, and
    /// the element interior deep in the hollow stays clean.
    #[test]
    fn box_shadow_inset_blur_falloff_monotonic() {
        let mut c = Canvas::new_filled(60, 60, [255, 255, 255, 255]);
        let rect = Rect { x: 10.0, y: 10.0, width: 40.0, height: 40.0 };
        c.fill_box_shadow(&rect, [0, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 8.0, 0.0, true);
        let ink = |x: usize, y: usize| 255 - px(&c, x, y)[0];
        let near = ink(10, 30); // 0.5 inside the element edge (edge at 10)
        let far = ink(16, 30); // 6.5 inside
        assert!(near > far, "monotonic inward: {near} > {far}");
        assert!(near < 255 && near > 0, "feather band is partial: {near}");
        assert_eq!(ink(19, 30), 0, "past the feather extent: none");
    }

    /// The SDF is exact on the straight edges: -5 at the center of a
    /// 10x10 box (r=2), 0 on the right edge, +3 three px out.
    #[test]
    fn sd_rounded_box_exact_on_edges() {
        assert_eq!(sd_rounded_box(0.0, 0.0, 3.0, 3.0, 2.0), -5.0);
        assert!(sd_rounded_box(5.0, 0.0, 3.0, 3.0, 2.0).abs() < 1e-9, "edge distance 0");
        assert!((sd_rounded_box(8.0, 0.0, 3.0, 3.0, 2.0) - 3.0).abs() < 1e-9, "+3 outside");
    }

    // ---- text-shadow (blitz#271 family) ----

    fn shadow_item(layers: Vec<TextShadow>) -> Vec<PaintItem> {
        vec![PaintItem::Text {
            text: "mmmmm".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 400.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            text_shadow: if layers.is_empty() { None } else { Some(layers) },
        }]
    }

    fn reds(c: &Canvas) -> Vec<(usize, usize)> {
        // Red-over-white composites keep r > g exactly when a red layer
        // contributed — pure white and pure black both have r == g, so any
        // nonzero red coverage (a 1/255 feather sliver included) matches.
        let mut out = Vec::new();
        for y in 0..c.height {
            for x in 0..c.width {
                if px(c, x, y)[0] > px(c, x, y)[1] {
                    out.push((x, y));
                }
            }
        }
        out
    }

    fn darks(c: &Canvas) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for y in 0..c.height {
            for x in 0..c.width {
                let p = px(c, x, y);
                if p[0] < 80 && p[1] < 80 && p[2] < 80 {
                    out.push((x, y));
                }
            }
        }
        out
    }

    /// A hard (blur 0) layer is an exact recolored copy of the glyph raster
    /// at the offset: every red pixel has glyph ink `(-dx, -dy)` from it in
    /// a shadow-less render, glyphs stay black, and the offset makes the
    /// shadow stick out where the glyphs aren't.
    #[test]
    fn text_shadow_hard_offset_recolor() {
        let fonts = crate::diting_fonts::font_book();
        let plain = {
            let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
            execute(&shadow_item(vec![]), &fonts, &mut c);
            c
        };
        let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(&shadow_item(vec![TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) }]), &fonts, &mut c);
        let red = reds(&c);
        assert!(!red.is_empty(), "shadow ink exists");
        // The shadow is a byte-identical recolored copy: every red pixel
        // (any nonzero coverage) mirrors glyph coverage at (-8,0).
        let plain_ink: std::collections::HashSet<(usize, usize)> = (0..plain.height)
            .flat_map(|y| (0..plain.width).map(move |x| (x, y)))
            .filter(|(x, y)| px(&plain, *x, *y)[0] < 255)
            .collect();
        for (x, y) in &red {
            assert!(x >= &8 && plain_ink.contains(&(x - 8, *y)), "red pixel ({x},{y}) mirrors glyph ink at (-8,0)");
        }
        // Glyphs paint OVER the shadow: solid glyph pixels stay pure black.
        for (x, y) in darks(&plain) {
            assert!(px(&c, x, y)[0] < 80, "solid glyph ({x},{y}) wins over the shadow");
        }
    }

    /// A co-located layer (dx=dy=0) sits entirely UNDER the glyphs: solid
    /// glyph pixels stay pure glyph ink — none of the shadow color leaks
    /// through full coverage.
    #[test]
    fn text_shadow_under_glyphs() {
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(&shadow_item(vec![TextShadow { dx: 0.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) }]), &fonts, &mut c);
        let plain = {
            let mut c2 = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
            execute(&shadow_item(vec![]), &fonts, &mut c2);
            c2
        };
        for (x, y) in darks(&plain) {
            assert!(px(&c, x, y)[0] < 80, "solid glyph ({x},{y}) covers the shadow");
        }
    }

    /// A blurred layer spreads past the hard extent (feather reaches
    /// `pad` px beyond the raster on both sides) and falls off toward the
    /// edge — center-row alpha above the feather-tip alpha.
    #[test]
    fn text_shadow_blur_spreads_and_falls_off() {
        let fonts = crate::diting_fonts::font_book();
        let span = |blur: f32| {
            let mut c = Canvas::new_filled(200, 80, [255, 255, 255, 255]);
            execute(
                &shadow_item(vec![TextShadow { dx: 0.0, dy: 14.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
                &fonts,
                &mut c,
            );
            let rows: Vec<usize> = reds(&c).iter().map(|(_, y)| *y).collect();
            (*rows.iter().min().unwrap(), *rows.iter().max().unwrap())
        };
        let (hard_top, hard_bot) = span(0.0);
        let (soft_top, soft_bot) = span(6.0);
        assert!(soft_top < hard_top, "feather reaches above the hard extent: {} < {hard_top}", soft_top);
        assert!(soft_bot > hard_bot, "feather reaches below the hard extent: {} > {hard_bot}", soft_bot);
        // Falloff: at the hard span's center row, alpha (redness) exceeds
        // the feather tip rows of the soft render.
        let mid_alpha = |c_row: usize, blur: f32| {
            let mut c = Canvas::new_filled(200, 80, [255, 255, 255, 255]);
            execute(
                &shadow_item(vec![TextShadow { dx: 0.0, dy: 14.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
                &fonts,
                &mut c,
            );
            255 - px(&c, 30, c_row)[1]
        };
        let center = (hard_top + hard_bot) / 2;
        assert!(mid_alpha(center, 6.0) > mid_alpha(soft_top, 6.0), "center ink above feather tip");
        assert!(mid_alpha(center, 6.0) > mid_alpha(soft_bot, 6.0), "center ink above feather tip (bottom)");
    }

    /// Layer order: first-declared paints ON TOP. Two layers at the SAME
    /// offset — the later (second-declared, painted first) blue must be
    /// fully covered by the first-declared red.
    #[test]
    fn text_shadow_first_layer_on_top() {
        let fonts = crate::diting_fonts::font_book();
        let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(
            &shadow_item(vec![
                TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) },
                TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(0, 0, 255, 255) },
            ]),
            &fonts,
            &mut c,
        );
        assert!(!reds(&c).is_empty(), "first-declared red shows");
        let mut blue = 0;
        for y in 0..c.height {
            for x in 0..c.width {
                let p = px(&c, x, y);
                if p[2] > 150 && p[0] < 80 {
                    blue += 1;
                }
            }
        }
        assert_eq!(blue, 0, "second-declared blue never surfaces under an identical red");
    }

    /// The separable box blur preserves the plane's total mass up to edge
    /// clamping and is idempotent-flat on a constant plane.
    #[test]
    fn box_blur_alpha_preserves_mass_and_flat() {
        let src = vec![0u8; 100];
        let flat = vec![200u8; 100];
        assert_eq!(box_blur_alpha(&flat, 10, 10, 2, true), flat, "constant plane stays constant");
        // Non-square plane: the vertical pass smears down the lit column
        // only. Guards the transposed-stride bug (a square buffer hides it
        // because w == h makes row and column strides coincide).
        let mut col = vec![0u8; 21]; // 7 wide, 3 tall
        col[1 * 7 + 2] = 255; // row 1, col 2
        let out = box_blur_alpha(&col, 7, 3, 1, false);
        for x in 0..7 {
            for y in 0..3 {
                let expect = if x == 2 { [127u8, 85, 127][y] } else { 0 };
                assert_eq!(out[y * 7 + x], expect, "vertical blur at ({x},{y})");
            }
        }
        let mut one = vec![0u8; 100];
        one[45] = 255;
        let out = box_blur_alpha(&one, 10, 10, 2, true);
        let total: u32 = out.iter().map(|&v| v as u32).sum();
        assert!(total > 200 && total <= 255 * 5, "mass spreads into the window, got {total}");
        assert!(out[45] < 255, "peak diluted");
        let empty: Vec<u8> = box_blur_alpha(&src, 10, 10, 1, false).to_vec();
        assert!(empty.iter().all(|&v| v == 0), "zero plane stays zero");
    }
}
