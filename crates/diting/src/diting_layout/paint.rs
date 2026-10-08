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

use super::forms::{paint_form_control, paint_form_widget};
use super::text::{
    baseline_offset, greedy_wrap, last_line_offset, tokens_of, truncate_tokens, HanSlot, PdfGlyph,
    ScaledMetrics, TextRaster, Token,
};
use super::{FontBook, PaintItem, Rect, TextGradient};
use crate::diting_css::{TextDecorations, TextShadow, WhiteSpace};
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
    #[allow(clippy::too_many_arguments)] // paint plumbing — see run_tokens
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
    #[allow(clippy::too_many_arguments)] // paint plumbing — see run_tokens
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

    /// backdrop-filter: blur() (blitz#901 family). Snapshot the canvas
    /// region under the box (padded by the blur's support so the kernel
    /// has input OUTSIDE the box), separable box-blur it in premultiplied
    /// RGBA, and write the result back ONLY inside the rounded border
    /// shape — the blur may sample outside the box, the output may not.
    fn blur_backdrop(&mut self, x: f32, y: f32, w: f32, h: f32, radii: [(f32, f32); 4], blur: f32) {
        if w <= 0.0 || h <= 0.0 || self.width == 0 || self.height == 0 {
            return;
        }
        let pad = blur.ceil() as i64;
        let radius = ((blur * 0.5).round() as usize).max(1);
        let x0 = (x.floor() as i64 - pad).max(0);
        let y0 = (y.floor() as i64 - pad).max(0);
        let x1 = ((x + w).ceil() as i64 + pad).min(self.width as i64);
        let y1 = ((y + h).ceil() as i64 + pad).min(self.height as i64);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let rw = (x1 - x0) as usize;
        let rh = (y1 - y0) as usize;
        // Deinterleave the snapshot, then premultiply RGB by A so bright
        // pixels can't bleed across transparent ones.
        let mut planes: [Vec<u8>; 4] =
            [vec![0u8; rw * rh], vec![0u8; rw * rh], vec![0u8; rw * rh], vec![0u8; rw * rh]];
        for (row, plane_rows) in planes.iter_mut().enumerate() {
            for gy in 0..rh {
                let src = ((y0 + gy as i64) as usize * self.width + x0 as usize) * 4;
                let dst = gy * rw;
                for gx in 0..rw {
                    plane_rows[dst + gx] = self.data[src + gx * 4 + row];
                }
            }
        }
        let alphas = planes[3].clone();
        for plane in planes.iter_mut().take(3) {
            for (v, a) in plane.iter_mut().zip(&alphas) {
                *v = ((*v as u32 * *a as u32 + 127) / 255) as u8;
            }
        }
        // One separable pass each way — the same two-round box kernel the
        // text-shadow feather uses (support ≈ blur).
        for plane in planes.iter_mut() {
            *plane = box_blur_alpha(plane, rw, rh, radius, true);
            *plane = box_blur_alpha(plane, rw, rh, radius, false);
        }
        // Composite: backdrop pixels inside the rounded shape are REPLACED
        // with their blur (blending blurred-over-sharp would double-count);
        // outside the shape the original stays crisp.
        let (bx, by) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
        let (hw, hh) = (w as f64 / 2.0, h as f64 / 2.0);
        let rx0 = (x.floor() as i64).max(x0).max(0);
        let ry0 = (y.floor() as i64).max(y0).max(0);
        let ry1 = ((y + h).ceil() as i64).min(y1);
        let rx1 = ((x + w).ceil() as i64).min(x1);
        for gy in ry0..ry1 {
            for gx in rx0..rx1 {
                let (cx, cy) = (gx as f64 + 0.5, gy as f64 + 0.5);
                if !self.clip_accepts(cx, cy) {
                    continue;
                }
                let (px, py) = (cx - bx, cy - by);
                let r = shadow_corner_radius(radii, px, py).min(hw).min(hh);
                if sd_rounded_box(px, py, hw - r, hh - r, r) > 0.0 {
                    continue;
                }
                let (col, row) = ((gx - x0) as usize, (gy - y0) as usize);
                let av = planes[3][row * rw + col] as u32;
                let i = (gy as usize * self.width + gx as usize) * 4;
                for (c, plane) in planes.iter().take(3).enumerate() {
                    self.data[i + c] = if av == 0 {
                        0
                    } else {
                        (((plane[row * rw + col] as u32) * 255 + av / 2) / av).min(255) as u8
                    };
                }
                self.data[i + 3] = av as u8;
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
/// `box-decoration-break: clone` (#218) swaps the one `area` for the
/// pixel's own wrapped-line fragment box, so every line restarts the
/// gradient — the line index comes from the row's y against the item's
/// line boxes, exactly where the raster laid them.
fn recolor_gradient_text(r: &mut TextRaster, x: f32, y: f32, line_height: f32, g: &TextGradient) {
    if r.width == 0 || r.height == 0 || g.stops.len() < 2 {
        return;
    }
    let rad = g.css_deg.to_radians() as f64;
    let (dux, duy) = (rad.sin(), -rad.cos());
    // Pixel row 0's y in the item space (the compositor blits at
    // (x, y + top)) — row_base per row, fused multiply-add per pixel.
    let tile_top = y as f64 + r.top as f64;
    for gy in 0..r.height {
        let (bx, by, bw, bh) = if g.clone_lines.is_empty() {
            (g.area.x, g.area.y, g.area.width, g.area.height)
        } else {
            let idx = (((r.top + gy as f32) / line_height).floor().max(0.0) as usize)
                .min(g.clone_lines.len() - 1);
            let (lox, lw) = g.clone_lines[idx];
            (x + lox, y + idx as f32 * line_height, lw, line_height)
        };
        let len = (bw as f64 * dux.abs() + bh as f64 * duy.abs()).max(1.0);
        let (ccx, ccy) = (
            bx as f64 + bw as f64 / 2.0,
            by as f64 + bh as f64 / 2.0,
        );
        let col_a = dux / len;
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
/// scales with font size at 1px per 16px. #218 (takumi #1802): when the
/// DECORATING element itself clips a gradient through its text, Chrome
/// strokes with the gradient (`text-decoration-color` loses) — `gradient`
/// carries that fill and each column samples it in page space.
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
    gradient: Option<&TextGradient>,
    mono: bool,
    word_spacing: f32,
    truncate_at: Option<f32>,
    ws: WhiteSpace,
    small_caps: bool,
    han: Option<HanSlot>,
    // The item's wrap tokens, pre-shaped when the item carries the run
    // leaf's memo (unscaled font params): the decorations painter needs the
    // wrap LINES, which the glyph-pixel RasterCache doesn't hold, so
    // without this every repaint re-shaped the run here.
    pre_shaped: Option<&[Token]>,
    dx: f32,
    dy: f32,
    // #214: the item's text-align-last — the last line's stroke follows the
    // glyphs' offset, else an underlined last line would keep its stroke on
    // the start edge while the ink moved.
    last_line_align: Option<crate::diting_css::TextAlign>,
) {
    if decorations.is_empty() || text.trim().is_empty() {
        return;
    }
    let owned;
    let tokens = match pre_shaped {
        Some(t) => t,
        None => {
            owned = tokens_of(text, font_size, bold, fonts, mono, word_spacing, ws, small_caps, han);
            &owned
        }
    };
    // The ellipsis marker is undecorated (Chrome): strokes span the kept
    // tokens only, so underline/line-through end at the truncation cut.
    let kept;
    let tokens: &[Token] = match truncate_at.and_then(|limit| truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing, ws, han)) {
        Some((t, _marker)) => {
            kept = t;
            &kept
        }
        None => tokens,
    };
    let lines = greedy_wrap(tokens, Some(wrap_at.max(0.0)), ws);
    let m = fonts.metrics(font_size, bold).unwrap_or(ScaledMetrics {
        ascent: font_size,
        descent: font_size * 0.2,
        line_gap: 0.0,
    });
    let b0 = baseline_offset(m.ascent, m.descent, line_height);
    let thickness = (font_size / 16.0).round().max(1.0);
    let mut stroke = |lx: f32, ly: f32, w: f32, li: usize| {
        if w <= 0.0 {
            return;
        }
        if let Some(g) = gradient.filter(|g| g.stops.len() >= 2) {
            // Same sampling math as `recolor_gradient_text`: the gradient
            // area is the whole run's (or the clone line's) box, and the
            // stroke's page-space x drives the color while the fill itself
            // still lands at the band-shifted canvas x.
            let (bx, by, bw, bh) = if g.clone_lines.is_empty() {
                (g.area.x, g.area.y, g.area.width, g.area.height)
            } else {
                let (lox, lw) = g.clone_lines[li.min(g.clone_lines.len() - 1)];
                (x + lox, y + li as f32 * line_height, lw, line_height)
            };
            let rad = g.css_deg.to_radians() as f64;
            let (dux, duy) = (rad.sin(), -rad.cos());
            let len = (bw as f64 * dux.abs() + bh as f64 * duy.abs()).max(1.0);
            let (ccx, ccy) = (bx as f64 + bw as f64 / 2.0, by as f64 + bh as f64 / 2.0);
            let row = (ly as f64 + thickness as f64 / 2.0 - ccy) * duy / len + 0.5;
            let tw = w.round().max(1.0) as usize;
            let th = thickness as usize;
            let mut tile = vec![0u8; tw * th * 4];
            for c in 0..tw {
                let t = (((lx + c as f32) as f64 - ccx) * dux / len + row).clamp(0.0, 1.0);
                let col = gradient_stop_color(&g.stops, t as f32);
                for r in 0..th {
                    tile[(r * tw + c) * 4..(r * tw + c) * 4 + 4].copy_from_slice(&col);
                }
            }
            if out.xf().is_some() {
                out.blit_rgba_affine(&tile, tw, th, lx as f64, ly as f64);
            } else {
                let (x0, y0) = ((lx - dx).round() as i64, (ly - dy).round() as i64);
                for c in 0..tw {
                    out.fill_rect(x0 + c as i64, y0, 1, th as i64, tile[c * 4..c * 4 + 4].try_into().unwrap());
                }
            }
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
        let lx = x + last_line_offset(&lines, i, last_line_align);
        if decorations.underline {
            stroke(lx, baseline + (m.descent * 0.5).max(1.0), line.width, i);
        }
        if decorations.overline {
            stroke(lx, baseline - m.ascent, line.width, i);
        }
        if decorations.line_through {
            stroke(lx, baseline - font_size * 0.28, line.width, i);
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
pub struct PdfLine {
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
pub enum PdfOp {
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
                ws,
                text_shadow,
                small_caps,
                han,
                last_line_align,
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
                    *decorations, *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), fonts,
                    *last_line_align,
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
    ws: WhiteSpace,
    small_caps: bool,
    han: Option<HanSlot>,
    pre_shaped: Option<&[Token]>,
    fonts: &FontBook,
    last_line_align: Option<crate::diting_css::TextAlign>,
) -> Option<PdfLine> {
    let owned;
    let tokens: &[Token] = match pre_shaped {
        Some(t) => t,
        None => {
            owned = tokens_of(text, font_size, bold, fonts, mono, word_spacing, ws, small_caps, han);
            &owned
        }
    };
    // Painted tokens: kept + the U+2026 marker appended, exactly the model
    // `rasterize_wrapped_uncached` paints; decorations wrap the kept set
    // only (the marker is undecorated, Chrome).
    let truncated;
    let painted: &[Token] = match truncate_at.and_then(|limit| {
        truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing, ws, han)
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
        truncate_tokens(tokens, limit, font_size, bold, fonts, mono, word_spacing, ws, han)
    }) {
        Some((k, _)) => {
            decorated = k;
            &decorated
        }
        None => tokens,
    };
    let lines = greedy_wrap(painted, Some(wrap_at.max(0.0)), ws);
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
        let Some(gs) = fonts.pdf_shape(run, font_size, bold, mono, han) else {
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
            if !shape_run(&s, last_line_offset(&lines, li, last_line_align), baseline, &mut glyphs) {
                return None;
            }
        }
    } else {
        for (li, line) in lines.iter().enumerate() {
            let mut pen = last_line_offset(&lines, li, last_line_align);
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
        let dlines = greedy_wrap(kept, Some(wrap_at.max(0.0)), ws);
        for (i, line) in dlines.iter().enumerate() {
            if line.width <= 0.0 {
                continue;
            }
            let baseline = y + (i as f32 * line_height).round() + b0;
            let sx = x + last_line_offset(&dlines, i, last_line_align);
            if decorations.underline {
                strokes.push((sx, baseline + (m.descent * 0.5).max(1.0), line.width));
            }
            if decorations.overline {
                strokes.push((sx, baseline - m.ascent, line.width));
            }
            if decorations.line_through {
                strokes.push((sx, baseline - font_size * 0.28, line.width));
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


/// Replay the paint items onto `out`. `Bg` rects come from taffy's rounded
/// layout so the fill lands on whole pixels; each `Text` re-rasterizes
/// wrapped at the width its containing block offered at measure time, so
/// the tile's line structure is the measure function's own.
pub fn execute(items: &[PaintItem], fonts: &FontBook, out: &mut Canvas) {
    execute_band(items, fonts, out, 0.0, 0.0);
}

/// Fold a device-pixel-ratio scale into the item stream (#185): the caller
/// allocates the canvas at `vw·s × vh·s` and passes `dx·s, dy·s` to
/// [`execute_band`]; this pass multiplies every page-space geometry field by
/// `s`, so the pixel loops resolve edges and gradients at device resolution
/// and text re-shapes at the scaled font params instead of nearest-neighboring
/// a 1× bitmap up (the DPR bug's blurry face). `Text` DROPS its wrap-token
/// memo — the memo only matches unscaled shaping, the same rule the
/// collect-time diagonal fold follows. Items inside a `SetXf` bracket are
/// LOCAL coordinates: the bracket matrix alone takes the scale (S·M — execute
/// then folds the band shift into e/f as usual), scaling the locals too would
/// double-apply. `SetXfCanvas` regions are canvas-space and scale normally.
pub fn scale_items(items: &[PaintItem], s: f32) -> Vec<PaintItem> {
    if (s - 1.0).abs() < f32::EPSILON {
        return items.to_vec();
    }
    // Suffixed `_s` because the pattern-bound field names (`rect`,
    // `radii`) shadow any plain-named helper inside the arms.
    fn rect_s(r: &mut Rect, s: f32) {
        r.x *= s;
        r.y *= s;
        r.width *= s;
        r.height *= s;
    }
    fn radii_s(rs: &mut [(f32, f32); 4], s: f32) {
        for (rx, ry) in rs.iter_mut() {
            *rx *= s;
            *ry *= s;
        }
    }
    let mut out = Vec::with_capacity(items.len());
    // One entry per open bracket: true = local-coordinate span (scale rides
    // the matrix, item fields stay raw). SetXfCanvas hijacks the top bracket
    // into a canvas-space span.
    let mut stack: Vec<bool> = Vec::new();
    for item in items {
        let mut it = item.clone();
        match &mut it {
            PaintItem::SetXf { xf } => {
                for v in xf.iter_mut() {
                    *v *= s;
                }
                stack.push(true);
            }
            PaintItem::SetXfCanvas => {
                if let Some(top) = stack.last_mut() {
                    *top = false;
                } else {
                    stack.push(false);
                }
            }
            PaintItem::ClearXf => {
                stack.pop();
            }
            _ if stack.last().copied().unwrap_or(false) => {}
            PaintItem::Bg { rect, radius, .. } => {
                rect_s(rect, s);
                *radius *= s;
            }
            PaintItem::BgCorner { rect, radii, .. } => {
                rect_s(rect, s);
                radii_s(radii, s);
            }
            PaintItem::BoxShadow { rect, radii, dx, dy, blur, spread, .. } => {
                rect_s(rect, s);
                radii_s(radii, s);
                *dx *= s;
                *dy *= s;
                *blur *= s;
                *spread *= s;
            }
            PaintItem::BackdropFilter { rect, radii, blur } => {
                rect_s(rect, s);
                radii_s(radii, s);
                *blur *= s;
            }
            PaintItem::BgGradient { rect, radii, .. } => {
                rect_s(rect, s);
                radii_s(radii, s);
            }
            PaintItem::Image { rect, paint_rect, .. } => {
                rect_s(rect, s);
                rect_s(paint_rect, s);
            }
            PaintItem::Replaced { rect, alt, .. } => {
                rect_s(rect, s);
                if let Some((_, font_size, _, line_height, _)) = alt {
                    *font_size *= s;
                    *line_height *= s;
                }
            }
            PaintItem::Svg { rect, .. } => rect_s(rect, s),
            PaintItem::Clip { rect } => rect_s(rect, s),
            PaintItem::ClipRounded { rect, radii } => {
                rect_s(rect, s);
                radii_s(radii, s);
            }
            PaintItem::PopClip => {}
            PaintItem::Border { rect, widths, radii, .. } => {
                rect_s(rect, s);
                for w in widths.iter_mut() {
                    *w *= s;
                }
                radii_s(radii, s);
            }
            PaintItem::Text {
                font_size,
                line_height,
                x,
                y,
                wrap_at,
                gradient,
                word_spacing,
                truncate_at,
                tokens,
                text_shadow,
                ..
            } => {
                *font_size *= s;
                *line_height *= s;
                *x *= s;
                *y *= s;
                *wrap_at *= s;
                *word_spacing *= s;
                if let Some(limit) = truncate_at {
                    *limit *= s;
                }
                // Scaled font params no longer match the leaf's measure-time
                // shaping — the paint re-shapes (same posture as the
                // collect-time diagonal fold).
                *tokens = None;
                if let Some(g) = gradient.as_mut() {
                    rect_s(&mut g.area, s);
                }
                if let Some(shadows) = text_shadow.as_mut() {
                    for sh in shadows.iter_mut() {
                        sh.dx *= s;
                        sh.dy *= s;
                        sh.blur *= s;
                    }
                }
            }
        }
        out.push(it);
    }
    out
}

/// Scale a straight-alpha color's alpha channel (animation batch A): the
/// pipeline composites straight-alpha source-over, so a group opacity folds
/// into item colors directly.
pub(crate) fn alpha_color(c: [u8; 4], a: f32) -> [u8; 4] {
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
            PaintItem::BackdropFilter { rect, radii, blur } => {
                // v1: skipped under an open transform bracket (no
                // canvas-space snapshot on the affine path); axis-aligned
                // glass is the blitz#901 shape.
                if out.xf().is_none() && *blur > 0.0 {
                    out.blur_backdrop(rect.x - dx, rect.y - dy, rect.width, rect.height, *radii, *blur);
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
                    // #38: widths arrive fractional in the wild (border:
                    // 0.8px) and the old `as i64` cast truncated every side
                    // below 1px to zero — the border vanished entirely. Snap
                    // each side to the nearest device pixel and scale that
                    // side's alpha by the coverage (0.8px -> 1px at 0.8
                    // alpha), the integer-canvas stand-in for Chrome's
                    // antialiasing. Exact-zero sides still paint nothing;
                    // integer widths stay bit-for-bit the old bands.
                    let band = |cw: f32| -> (i64, [u8; 4]) {
                        if cw <= 0.0 {
                            return (0, *color);
                        }
                        let pix = (cw.round() as i64).max(1);
                        let cov = (cw / pix as f32).clamp(0.0, 1.0);
                        let mut c = *color;
                        c[3] = ((c[3] as f32) * cov).round() as u8;
                        (pix, c)
                    };
                    let (tp, tc) = band(t);
                    let (bp, bc) = band(b);
                    let (lp, lc) = band(l);
                    let (rp, rc) = band(r);
                    out.fill_rect(x, y, w, tp, tc);
                    out.fill_rect(x, y + h - bp, w, bp, bc);
                    out.fill_rect(x, y + tp, lp, h - tp - bp, lc);
                    out.fill_rect(x + w - rp, y + tp, rp, h - tp - bp, rc);
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
            PaintItem::Replaced { rect, alt, fill_placeholder, widget, form, alpha, caret, form_rtl } => {
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
                            *fill_placeholder, fonts, *alpha, *caret, *form_rtl,
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
                                    WhiteSpace::Normal,
                                    false,
                                    None,
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
                            *caret, *form_rtl,
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
                                    WhiteSpace::Normal,
                                    false,
                                    None,
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
            PaintItem::Text { text, font_size, bold, color, line_height, x, y, wrap_at, gradient, decorations, mono, word_spacing, truncate_at, tokens, ws, text_shadow, small_caps, han, last_line_align } => {
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
                            stamp_text_shadow(out, fonts, text, *font_size, *bold, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), sh, true, (*x + sh.dx) as f64, (*y + sh.dy) as f64, *last_line_align);
                            // Chrome shadows the decorations with the text
                            // (blitz#984): each hard layer restamps the
                            // strokes in the shadow color at the same offset.
                            // Blurred layers skip the stroke copy — a hard
                            // line beside a feathered glyph reads as a bug,
                            // not a shadow.
                            if sh.blur <= 0.0 {
                                paint_text_decorations(out, fonts, text, *font_size, *bold, [sh.color.0, sh.color.1, sh.color.2, sh.color.3], *line_height, *x + sh.dx, *y + sh.dy, *wrap_at, *decorations, None, *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), 0.0, 0.0, *last_line_align);
                            }
                        }
                    }
                    let r = fonts.rasterize_wrapped_with(text, *font_size, *bold, fill, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, *ws, tokens.clone(), *small_caps, *han, *last_line_align);
                    // Gradient recolor rewrites pixels in place — the cache
                    // hands out Arcs, so that path clones first (#399).
                    let mut owned;
                    let r = if let Some(g) = gradient {
                        owned = (*r).clone();
                        recolor_gradient_text(&mut owned, *x, *y, *line_height, g);
                        &owned
                    } else {
                        &r
                    };
                    out.blit_rgba_affine(&r.data, r.width, r.height, *x as f64, (*y + r.top) as f64);
                    paint_text_decorations(out, fonts, text, *font_size, *bold, *color, *line_height, *x, *y, *wrap_at, *decorations, decorations.pierce.then_some(gradient.as_ref()).flatten(), *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), 0.0, 0.0, *last_line_align);
                } else {
                    let pad = shadow_pad.unwrap_or(0.0);
                    if !text_reaches_band(*y, text, *font_size, *wrap_at, *line_height, dy, out.height as i64, pad) {
                        continue;
                    }
                    if let Some(shadows) = text_shadow {
                        for sh in shadows.iter().rev() {
                            stamp_text_shadow(out, fonts, text, *font_size, *bold, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), sh, false, (*x + sh.dx - dx) as f64, (*y + sh.dy - dy) as f64, *last_line_align);
                            if sh.blur <= 0.0 {
                                paint_text_decorations(out, fonts, text, *font_size, *bold, [sh.color.0, sh.color.1, sh.color.2, sh.color.3], *line_height, *x + sh.dx, *y + sh.dy, *wrap_at, *decorations, None, *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), dx, dy, *last_line_align);
                            }
                        }
                    }
                    let r = fonts.rasterize_wrapped_with(text, *font_size, *bold, fill, *wrap_at, *line_height, *mono, *word_spacing, *truncate_at, *ws, tokens.clone(), *small_caps, *han, *last_line_align);
                    let mut owned;
                    let r = if let Some(g) = gradient {
                        owned = (*r).clone();
                        recolor_gradient_text(&mut owned, *x, *y, *line_height, g);
                        &owned
                    } else {
                        &r
                    };
                    // Tile row 0 sits `top` px above the leaf's line-box top.
                    out.blit_text(r, (x - dx).round() as i64, (y - dy + r.top).round() as i64);
                    paint_text_decorations(out, fonts, text, *font_size, *bold, *color, *line_height, *x, *y, *wrap_at, *decorations, decorations.pierce.then_some(gradient.as_ref()).flatten(), *mono, *word_spacing, *truncate_at, *ws, *small_caps, *han, tokens.as_deref(), dx, dy, *last_line_align);
                }
            }
        }
    }
}

/// Effective text-shadow blur ceiling. Past this a shadow is visually
/// indistinguishable wash — the clamp bounds the scratch tile without
/// costing any real rendering (#76).
const MAX_SHADOW_BLUR: f32 = 256.0;
/// Padded-tile ceiling in pixels (~16 MP: a 4000×4000 tile; an 8000-px
/// text line at the clamped 256-px radius still fits). Alpha plane + RGBA
/// copy at this size stay under ~80 MB; anything bigger paints the
/// unfeathered shadow instead of allocating.
const MAX_SHADOW_SCRATCH_PX: usize = 16 * 1024 * 1024;

/// One text-shadow layer stamped UNDER the glyphs (blitz#271 family):
/// re-rasterize the run in the layer color (the RasterCache keys on color,
/// so shadow layers memo independently), box-blur its alpha when the layer
/// asks for a blur, and blit at the offset. `affine` selects the
/// transformed blit (page space; the open bracket folds dx/dy) vs the
/// band-space rounded blit.
///
/// The blur radius feeds the padded scratch tile's size straight from CSS —
/// `text-shadow: 0 0 100000px` on a short run asks for tens of GB (#76,
/// obscura #1059 family). Two guards below: clamp the effective radius
/// (past 256 px a shadow is visually indistinguishable wash) and cap the
/// padded tile at a scratch budget; an oversized tile paints the
/// unfeathered shadow instead of allocating. Small-blur rendering is
/// byte-identical to before (blur well under budget ⇒ same path).
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
    ws: WhiteSpace,
    small_caps: bool,
    han: Option<HanSlot>,
    tokens: Option<&[Token]>,
    sh: &TextShadow,
    affine: bool,
    ox: f64,
    oy: f64,
    last_line_align: Option<crate::diting_css::TextAlign>,
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
        ws,
        tokens.map(std::rc::Rc::from),
        small_caps,
        han,
        last_line_align,
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
    let blur = sh.blur.min(MAX_SHADOW_BLUR);
    let pad = blur.ceil() as usize;
    let radius = ((blur * 0.5).round() as usize).max(1);
    let pw = r.width.saturating_add(pad).saturating_add(pad);
    let ph = r.height.saturating_add(pad).saturating_add(pad);
    if pw.checked_mul(ph).is_none_or(|px| px > MAX_SHADOW_SCRATCH_PX) {
        // Even clamped, the tile overshoots the scratch budget (enormous
        // run + full-radius blur): paint the unfeathered shadow — same
        // shape the `blur <= 0` path draws — instead of a giant allocation.
        if affine {
            out.blit_rgba_affine(&r.data, r.width, r.height, ox, oy + r.top as f64);
        } else {
            out.blit_text(&r, ox.round() as i64, (oy + r.top as f64).round() as i64);
        }
        return;
    }
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

// The colocated contract suite (batch 219: moved out with the file riding
// the god-file ratchet cap — the layering audit exempts tests.rs).
#[cfg(test)]
mod tests;
