// Viewport-band capture family — split out of the god file (ratchet).
// Pure move: the functions see the same ops::* namespace they grew up in.
use super::*;

/// One viewport-band frame: RGBA pixels plus the scroll offset actually
/// painted and the document's scrollable extent.
#[cfg(feature = "screenshot")]
pub struct BandFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The scroll offset actually painted — clamped into
    /// `[0, content − viewport]`, so `metadata.scrollOffsetX/Y` report this,
    /// not the raw request.
    pub dx: f32,
    pub dy: f32,
    /// CSS-pixel content size (root scrollWidth×scrollHeight).
    pub content_size: (f32, f32),
    /// PDF text-layer glyph ops for this band, band-local (vector-text
    /// batch). Empty unless the band was painted via `band_frame_with_text`.
    pub text_ops: Vec<crate::diting_layout::paint::PdfOp>,
}

/// Paint the viewport band `[dx, dx+vw) × [dy, dy+vh)` of the live tree's
/// cached layout into a viewport-sized canvas — the CDP viewport-capture /
/// screencast frame path. No outerHTML re-parse and no full-page raster:
/// layout is scroll-blind and memoized per tree epoch, so scrolling is a
/// pure re-blit and per-frame cost is independent of page height (the root
/// fix for the ~1.4s/frame full-page re-render the AginxOS report measured).
///
/// Returns the frame plus the absolute img URLs the page references but
/// [`JsState::image_bytes`] lacks — the async caller fetches them (same
/// per-URL policy as `prefetch_render_resources`: page client, SSRF gate,
/// size/timeout caps) via [`store_image_bytes`] and calls again for the
/// image-complete frame. Band paint itself never touches the network.
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, scroll_x, scroll_y, viewport, false, false)
}

/// Same band paint but ALSO collects the PDF text layer (vector-text batch):
/// vectorizable `Text` items leave the raster — the PDF layer redraws them as
/// glyphs, and painting twice would double the antialiasing — and come back
/// as band-local glyph ops on the frame.
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame_with_text(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, scroll_x, scroll_y, viewport, true, false)
}

/// Page-cut variant for the print/slides pumps (#60): `(x, y)` is a
/// document-space band ORIGIN, not a scroll offset. The only behavioral
/// difference from [`band_frame`] is that the blitz#880 root-overflow
/// collapse must not apply — it exists so `window.scrollY` and the scroll
/// pump agree on the scrollable range, but a page cut is not a scroll.
/// Without this, a slide deck with the standard `html { overflow: hidden }`
/// (translateX carousels all carry it) collapses the extent to the viewport
/// and every emitted page renders the first viewport's frame.
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame_cut(
    gs: &JsState,
    x: f32,
    y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, x, y, viewport, false, true)
}

/// [`band_frame_cut`] with the PDF text layer collected (see
/// [`band_frame_with_text`]).
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame_cut_with_text(
    gs: &JsState,
    x: f32,
    y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, x, y, viewport, true, true)
}

#[cfg(feature = "screenshot")]
fn band_frame_inner(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
    collect_text: bool,
    cut: bool,
) -> Option<(BandFrame, Vec<String>)> {
    gs.band_paints.set(gs.band_paints.get() + 1);
    let dom = gs.dom.as_ref()?;
    // Same page-height cap the full-page render uses — a malicious client
    // requesting a 1e9-viewport must not allocate for it.
    const MAX_BAND: f32 = 16000.0;
    let vw = if viewport.0.is_finite() { viewport.0.max(1.0) } else { 1.0 }.min(MAX_BAND);
    let vh = if viewport.1.is_finite() { viewport.1.max(1.0) } else { 1.0 }.min(MAX_BAND);

    let epoch = dom.epoch();
    ensure_layout_run(gs, dom, epoch);
    let guard = gs.layout_cache.borrow();
    let (_, (rects, _, styles, items, _, sticky_spans, scroller_spans)) =
        guard.as_ref().filter(|(e, _)| *e == epoch)?;

    // Scrollable content extent: the root scroller's box unioned with every
    // laid-out descendant, clamped up to the viewport — the same union the
    // `scroll_extent` op serves the JS side, so the pump's clamp and
    // window.scrollY agree on the range.
    let root = dom
        .query_selector_all("html")
        .ok()
        .and_then(|v| v.into_iter().next())
        .or_else(|| dom.query_selector_all("body").ok().and_then(|v| v.into_iter().next()));
    let mut content_w = vw;
    let mut content_h = vh;
    if let Some(root) = root.filter(|r| rects.contains_key(r)) {
        if let Some(&[ox, oy, ow, oh]) = rects.get(&root) {
            content_w = content_w.max(ow);
            content_h = content_h.max(oh);
            let mut stack = dom.children(root);
            while let Some(cur) = stack.pop() {
                // Same viewport-fixed skip as the scroll_extent op (blitz#841)
                // — the two walks must agree or JS scrollHeight and the
                // pump's clamp disagree on the scroll range.
                if is_viewport_fixed(dom, styles, cur, root) {
                    continue;
                }
                stack.extend(dom.children(cur));
                if let Some(&[x, y, w, h]) = rects.get(&cur) {
                    content_w = content_w.max((x + w - ox).max(0.0));
                    content_h = content_h.max((y + h - oy).max(0.0));
                }
            }
        }
    }
    // The rect union is element-only and the html/body boxes stretch to the
    // viewport, so a bare-text body (no element children) reports no extent
    // past the viewport — its ink lives only in the Text paint items. Folding
    // their wrap-model extent in fixes both readers of content_h: the scroll
    // clamp (blitz#444 fixed overflowing elements but text-only bodies still
    // couldn't scroll past the first screenful) and the print pump's page
    // count (which saw viewport-clamped scrollHeight and cut blank tail
    // pages).
    let (ink_w, ink_h) = crate::diting_layout::paint::text_ink_extent(items);
    content_w = content_w.max(ink_w);
    content_h = content_h.max(ink_h);
    // Viewport overflow propagation (blitz#880, css-overflow-3 §3.3):
    // hidden/clip carried by the root element — its own or handed up by the
    // first body child — makes the viewport itself unscrollable, so the
    // scrolling area collapses to exactly the viewport, overriding the
    // unions above. Same clamp the `scroll_extent` op serves the JS side:
    // the two walks must agree or window.scrollY and the pump's clamp
    // disagree on the scroll range. Page cuts (`cut`, #60) skip this: their
    // origin is not a scroll position, and slide decks carry
    // `html { overflow: hidden }` as a matter of course.
    if !cut {
        if let Some(root) = root.filter(|r| rects.contains_key(r)) {
            let eff = crate::diting_layout::effective_viewport_overflow(
                dom,
                styles,
                root,
                |id| rects.contains_key(&id),
            );
            if matches!(
                eff,
                crate::diting_css::Overflow::Hidden | crate::diting_css::Overflow::Clip
            ) {
                content_w = vw;
                content_h = vh;
            }
        }
    }
    let dx = (if scroll_x.is_finite() { scroll_x.max(0.0) } else { 0.0 })
        .min((content_w - vw).max(0.0));
    let dy = (if scroll_y.is_finite() { scroll_y.max(0.0) } else { 0.0 })
        .min((content_h - vh).max(0.0));

    // Sticky v2 paint half: resolve each layout-recorded span to the
    // total its items must carry — sticky spans take the node's read
    // total (box and all), scroller spans subtract the scroller's own
    // offset (its box and clip stay fixed while its content translates).
    // Sticky spans first so a both-node's sticky span precedes its
    // scroller span in the nesting order. Nothing shifted on the page →
    // paint the cached run untouched, zero copy. Per-frame translate, not
    // memoized: paint-only writes re-collect items without bumping the
    // tree epoch, so a cached shifted copy could go stale mid-animation.
    let sticky_map = sticky_shifts(gs, dom);
    let mut live: Vec<(usize, usize, [f32; 2])> = sticky_spans
        .iter()
        .filter_map(|&(n, s, e)| sticky_map.get(&n).map(|&v| (s, e, v)))
        .collect();
    live.extend(scroller_spans.iter().map(|&(n, s, e)| {
        let [ox, oy] = dom.node_scroll(n);
        let base = sticky_map.get(&n).copied().unwrap_or([0.0, 0.0]);
        (s, e, [base[0] - ox, base[1] - oy])
    }));
    // No zero-VALUE filter here: liveness is per-delta inside
    // apply_sticky_to_items (a pinned sticky inside a scroller has total
    // zero yet a nonzero delta over the scroller's base).
    let shifted_items: Option<Vec<crate::diting_layout::PaintItem>> = if live.is_empty() {
        None
    } else {
        Some(crate::diting_layout::apply_sticky_to_items(items, &live))
    };
    let items: &[crate::diting_layout::PaintItem] =
        shifted_items.as_deref().unwrap_or(items.as_slice());

    // Images the page references but the byte table lacks. Sources come
    // back absolutized against the document URL (resolve_img_source's base
    // join), so the missing entries are directly fetchable and match the
    // table's absolute keys on the re-blit.
    let mut missing: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(imgs) = dom.query_selector_all("img") {
        let table = gs.image_bytes.borrow();
        for nid in imgs {
            if let Some(src) =
                crate::diting_layout::resolve_img_source(dom, nid, vw, Some(gs.url.as_str()))
            {
                if !table.contains_key(&src) && seen.insert(src.clone()) {
                    missing.push(src);
                }
            }
        }
    }

    let mut canvas =
        crate::diting_layout::paint::Canvas::new_filled(vw as usize, vh as usize, [255, 255, 255, 255]);
    let fonts = crate::diting_fonts::font_book();
    // Vector-text collect: the items the PDF layer will redraw as glyphs drop
    // out of the raster pass (painting both would double the antialiasing).
    // Their ops come back band-local — the writer needs no per-band offset.
    let mut text_ops: Vec<crate::diting_layout::paint::PdfOp> = Vec::new();
    if collect_text {
        let (ops, vectorized) = crate::diting_layout::paint::pdf_text_ops(items, &fonts);
        let background: Vec<crate::diting_layout::PaintItem> = items
            .iter()
            .enumerate()
            .filter(|(i, _)| !vectorized.contains(i))
            .map(|(_, it)| it.clone())
            .collect();
        crate::diting_layout::paint::execute_band(&background, &fonts, &mut canvas, dx, dy);
        let mut ops = ops;
        // Band-window filter: ops are still document-space here (translate
        // below). Drop glyph lines whose baselines fall outside the band
        // (with a font-size margin) so each PDF page carries only its own
        // text instead of the whole document shifted off-page.
        let band_top = dy;
        let band_bottom = dy + canvas.height as f32;
        ops.retain(|op| match op {
            crate::diting_layout::paint::PdfOp::Line(l) => l.glyphs.iter().any(|g| {
                g.y > band_top - l.font_size * 1.5 && g.y < band_bottom + l.font_size
            }),
            _ => true,
        });
        crate::diting_layout::paint::pdf_ops_translate(&mut ops, dx, dy);
        text_ops = ops;
    } else {
        crate::diting_layout::paint::execute_band(items, &fonts, &mut canvas, dx, dy);
    }
    drop(guard);
    Some((
        BandFrame {
            width: canvas.width as u32,
            height: canvas.height as u32,
            rgba: canvas.data,
            dx,
            dy,
            content_size: (content_w, content_h),
            text_ops,
        },
        missing,
    ))
}
