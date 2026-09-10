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

/// Rough advance width: CJK/fullwidth ≈ 1em, everything else ≈ 0.6em.
pub(crate) fn est_width(text: &str, font_size: f32) -> f32 {
    text.chars()
        .map(|c| if c > '\u{2E80}' { 1.0 } else { 0.6 })
        .sum::<f32>()
        * font_size
}

/// The page-space extent of the `Text` items alone, from the same wrap model
/// the band pre-filter and `rasterize_wrapped` use (height = y + lines ×
/// line-height, width = one wrapped line). Bare text owns no element box —
/// html/body stretch to the viewport — so this is the only place its true
/// extent exists: the scroll-union in `band_frame` and the print pump's page
/// count both read it. Errs high, never low.
pub fn text_ink_extent(items: &[PaintItem]) -> (f32, f32) {
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    for item in items {
        if let PaintItem::Text { text, font_size, line_height, x, y, wrap_at, .. } = item {
            let wrap = wrap_at.max(1.0);
            let est = est_width(text, *font_size);
            let lines = (est / wrap).ceil().max(1.0);
            w = w.max(x + est.min(wrap));
            h = h.max(y + lines * line_height);
        }
    }
    (w, h)
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
            PaintItem::Replaced { rect, alt, fill_placeholder, alpha } => {
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
                            );
                            scratch.blit_text(&r, 0, r.top.round() as i64);
                            scratch.pop_clip();
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
            PaintItem::Text { text, font_size, bold, color, line_height, x, y, wrap_at } => {
                if out.xf().is_some() {
                    // Rasterize at RAW local metrics — the bracket maps the
                    // tile, so no scale folds into font metrics — prefiltered
                    // by the mapped bbox of the same estimated tile box the
                    // band check below uses.
                    let est_w = est_width(text, *font_size).max(*wrap_at);
                    let lines = (est_width(text, *font_size) / wrap_at.max(1.0)).ceil().max(1.0);
                    let (bx0, by0, bx1, by1) = out.mapped_xf_bounds(
                        *x as f64,
                        (*y - *line_height) as f64,
                        est_w as f64,
                        ((lines + 1.0) * *line_height) as f64,
                    );
                    if bx1 <= 0 || by1 <= 0 || bx0 >= out.width as i64 || by0 >= out.height as i64 {
                        continue;
                    }
                    let r = fonts.rasterize_wrapped(text, *font_size, *bold, *color, *wrap_at, *line_height);
                    out.blit_rgba_affine(&r.data, r.width, r.height, *x as f64, (*y + r.top) as f64);
                } else {
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
}
