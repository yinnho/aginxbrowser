//! Screenshot rendering: feed a JS-rendered HTML string to Blitz (Stylo +
//! Taffy layout, vello_cpu paint) and return a PNG.
//!
//! This module is the "paint what the agent already sees" layer - aginxbrowser's
//! V8 path has already run the page's JS and produced the final DOM; we hand
//! that DOM to Blitz purely for layout + rasterization. No sub-resource fetches
//! happen here (Blitz's DummyNetProvider is a no-op, and upstream #636's
//! `is_noop()` gating stops `<head>` stylesheets from blocking paint forever -
//! see the `screenshot` feature note in Cargo.toml).
//!
//! Element regions: after layout, every node carries a Taffy `final_layout`
//! whose `location` is parent-relative. The absolute (page-relative) origin is
//! the sum of locations up the `layout_parent` chain (which includes anonymous
//! block boxes, matching how `blitz-paint` positions nodes). `paint_scene`'s
//! x/y offsets are in scaled physical pixels, so a CSS-pixel rect is passed in
//! as `rect * scale`.

use anyhow::Result;
use blitz_dom::{BaseDocument, DocumentConfig, Point, util::Color};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;

/// Bounding box of a laid-out element in CSS pixels, relative to the page
/// origin (top-left of the root element's content).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ElementRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Output of a render pass: the PNG plus metadata about what was rendered.
#[derive(Debug)]
pub struct RenderedScreenshot {
    /// Encoded PNG bytes.
    pub png: Vec<u8>,
    /// Actual rendered pixel dimensions of the PNG (may differ from the
    /// request: full_page tracks content height, selector crops track the
    /// element's size).
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// CSS-pixel rects for the selector match(es). Empty when no selector was
    /// given. With `selector_all`, one entry per match (image stays uncropped);
    /// otherwise the single match, which is also the cropped region.
    pub rects: Vec<ElementRect>,
}

/// Render an HTML string to a PNG (RGBA bytes) plus element rects.
///
/// `base_url` is used to resolve relative URLs in the document (mostly moot
/// since we don't fetch sub-resources, but Blitz uses it for `<base>` etc.).
/// When `full_page` is true, the render height tracks the document's computed
/// content height (capped at 16000px to bound memory); otherwise the requested
/// `height` is used (viewport-sized).
///
/// `selector`: when given (and `selector_all` is false), the image is cropped
/// to the first matching element instead of the whole page - Blitz re-paints
/// with the rect's origin as the viewport offset, so no PNG decoding/cropping
/// is needed. With `selector_all`, the image renders normally and `rects`
/// carries every match (empty if none match; invalid selectors are errors).
#[allow(clippy::too_many_arguments)]
pub fn render_html_to_png(
    html: &str,
    base_url: &str,
    width: u32,
    height: u32,
    scale: f32,
    full_page: bool,
    selector: Option<&str>,
    selector_all: bool,
) -> Result<RenderedScreenshot> {
    if html.is_empty() {
        anyhow::bail!("render_html_to_png: empty HTML (page content() returned nothing - navigation may have failed)");
    }
    let scale_f = scale.max(0.1) as f64;

    // No net provider -> DummyNetProvider (no-op). Upstream #636's is_noop()
    // gating ensures head stylesheets don't permanently block paint. We never
    // fetch sub-resources here; the DOM is already JS-rendered by the V8 path.
    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some(base_url.to_string()),
            net_provider: None,
            viewport: Some(Viewport::new(
                width * (scale as u32),
                height * (scale as u32),
                scale,
                ColorScheme::Light,
            )),
            ..Default::default()
        },
    );

    // Drive Stylo style resolution + Taffy layout to completion. Without a net
    // provider there are no deferred resource waits, so a few rounds suffice.
    for _ in 0..4 {
        document.resolve(0.0);
    }

    // Guard: if critical resources are somehow still pending, paint_scene would
    // no-op the whole frame. Surface this explicitly rather than returning blank.
    if document.as_ref().has_pending_critical_resources() {
        anyhow::bail!("render_html_to_png: Blitz still reports pending critical resources (is_noop gating from upstream #636 not active?)");
    }

    let root_layout = document.as_ref().root_element().final_layout().size;
    let computed_height = root_layout.height as f64;

    // Resolve the selector (if any) against the laid-out document.
    let mut rects = Vec::new();
    let mut crop: Option<(f64, f64, f64, f64)> = None; // CSS px x, y, w, h
    if let Some(sel) = selector {
        if selector_all {
            let ids = document
                .as_ref()
                .query_selector_all(sel)
                .map_err(|e| anyhow::anyhow!("invalid selector {sel:?}: {e:?}"))?;
            rects = ids
                .iter()
                .map(|&id| element_rect(document.as_ref(), id))
                .collect();
        } else {
            let id = document
                .as_ref()
                .query_selector(sel)
                .map_err(|e| anyhow::anyhow!("invalid selector {sel:?}: {e:?}"))?
                .ok_or_else(|| anyhow::anyhow!("selector {sel:?} matched no element"))?;
            let r = element_rect(document.as_ref(), id);
            if r.width < 0.5 || r.height < 0.5 {
                anyhow::bail!(
                    "selector {sel:?} matched an element with no layout box ({}x{}). \
                     Inline elements (bare <a>/<span> with text) carry no Taffy box - \
                     target a block ancestor instead",
                    r.width, r.height
                );
            }
            // Clamp the crop into the page and to sane minimums; cap like full_page.
            let (w, h) = (r.width.min(16000.0), r.height.min(16000.0));
            crop = Some((r.x.max(0.0), r.y.max(0.0), w, h));
            rects.push(r);
        }
    }

    let (render_width, render_height, viewport_scroll) = match crop {
        // Element crop: paint a viewport-sized window with the element's
        // origin as the scroll position. paint_scene's own x/y offset params
        // shift painted positions and viewport culling inconsistently
        // (upstream only ever passes 0,0), whereas viewport_scroll shifts
        // both coherently - so we use that and keep the offsets at zero.
        Some((cx, cy, cw, ch)) => (
            (cw * scale_f) as u32,
            (ch * scale_f) as u32,
            Some((cx, cy)),
        ),
        None => (
            (width as f64 * scale_f) as u32,
            if full_page {
                // Track real content height, clamped to bound the RGBA buffer (~width*16000*4 bytes).
                ((computed_height.max(height as f64)).min(16000.0) * scale_f) as u32
            } else {
                (height as f64 * scale_f) as u32
            },
            None,
        ),
    };

    if render_width == 0 || render_height == 0 {
        anyhow::bail!(
            "render_html_to_png: zero-sized output ({}x{}; computed_height={})",
            render_width, render_height, computed_height
        );
    }

    if let Some((sx, sy)) = viewport_scroll {
        document.as_mut().set_viewport_scroll(Point { x: sx, y: sy });
    }

    // Paint white background, then the document. vello_cpu is pure CPU - no GPU/wgpu.
    let buffer = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
        |scene| {
            scene.fill(
                Fill::NonZero,
                Default::default(),
                Color::WHITE,
                Default::default(),
                &Rect::new(0.0, 0.0, render_width as f64, render_height as f64),
            );
            paint_scene(
                scene,
                document.as_mut(),
                scale_f,
                render_width,
                render_height,
                0,
                0,
            );
        },
        render_width,
        render_height,
    );

    // Encode RGBA buffer -> PNG bytes.
    let mut png_bytes = Vec::with_capacity((render_width * render_height * 4) as usize / 4);
    {
        use std::io::Cursor;
        let mut encoder = png::Encoder::new(Cursor::new(&mut png_bytes), render_width, render_height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| anyhow::anyhow!("png encode header: {e}"))?;
        writer
            .write_image_data(&buffer)
            .map_err(|e| anyhow::anyhow!("png encode data: {e}"))?;
        writer
            .finish()
            .map_err(|e| anyhow::anyhow!("png encode finish: {e}"))?;
    }

    tracing::debug!(
        "screenshot: {}x{} @ scale {} (scroll {:?}) -> {} PNG bytes (computed_height={}, rects={})",
        render_width, render_height, scale_f, viewport_scroll, png_bytes.len(), computed_height, rects.len()
    );

    Ok(RenderedScreenshot {
        png: png_bytes,
        pixel_width: render_width,
        pixel_height: render_height,
        rects,
    })
}

/// The absolute (page-relative) border-box rect of a laid-out element.
///
/// Taffy only sizes block-level boxes; pure inline elements (`<a>text</a>`)
/// report 0x0 because their content belongs to the containing block's inline
/// layout. As a fallback, union the element's element-descendant boxes (which
/// covers mixed content like `<a><img></a>`); if that is still empty the
/// caller gets the honest 0x0.
fn element_rect(doc: &BaseDocument, node_id: NodeId) -> ElementRect {
    let (x, y) = absolute_origin(doc, node_id);
    let size = doc
        .get_node(node_id)
        .map(|n| n.final_layout().size)
        .unwrap_or_default();
    let mut rect = ElementRect {
        x,
        y,
        width: size.width as f64,
        height: size.height as f64,
    };
    if rect.width < 0.5 || rect.height < 0.5 {
        if let Some(bbox) = descendants_bbox(doc, node_id) {
            rect = bbox;
        }
    }
    rect
}

/// Union of the absolute rects of every element descendant of `node_id`
/// (skipping text/comment nodes, which carry no layout). Bounded to 2000
/// visits so pathological DOMs can't stall the render.
fn descendants_bbox(doc: &BaseDocument, node_id: NodeId) -> Option<ElementRect> {
    let root = doc.get_node(node_id)?;
    let mut stack: Vec<NodeId> = root.children.iter().copied().collect();
    let mut bbox: Option<ElementRect> = None;
    let mut visited = 0usize;
    while let Some(id) = stack.pop() {
        if visited > 2000 {
            break;
        }
        let Some(node) = doc.get_node(id) else { continue };
        if node.element_data().is_none() {
            // Text/comment nodes have no final_layout (accessing it panics);
            // their extent belongs to the nearest block ancestor anyway.
            continue;
        }
        visited += 1;
        let size = node.final_layout().size;
        if size.width > 0.0 && size.height > 0.0 {
            let (ox, oy) = absolute_origin(doc, id);
            let r = ElementRect {
                x: ox,
                y: oy,
                width: size.width as f64,
                height: size.height as f64,
            };
            bbox = Some(match bbox {
                None => r,
                Some(b) => ElementRect {
                    x: b.x.min(r.x),
                    y: b.y.min(r.y),
                    width: (b.x + b.width).max(r.x + r.width) - b.x.min(r.x),
                    height: (b.y + b.height).max(r.y + r.height) - b.y.min(r.y),
                },
            });
        }
        stack.extend(node.children.iter().copied());
    }
    bbox
}

/// Sum of `final_layout().location` from `node_id` up the `layout_parent`
/// chain. Taffy locations are parent-relative, and blitz-paint composes the
/// same accumulation into its transform when positioning nodes, so this sum
/// matches painted positions. Anonymous block boxes (layout-only nodes) are on
/// the `layout_parent` chain and carry layouts too.
fn absolute_origin(doc: &BaseDocument, node_id: NodeId) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    let mut current = Some(node_id);
    while let Some(id) = current {
        let Some(node) = doc.get_node(id) else { break };
        let location = node.final_layout().location;
        x += location.x as f64;
        y += location.y as f64;
        current = node.layout_parent.get().or(node.parent);
    }
    (x, y)
}

// Suppress unused-import warning for anyrender::PaintScene when the trait is
// only brought into scope for paint_scene's generic bound.
#[allow(unused_imports)]
use anyrender::PaintScene as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic layout check: an absolutely-positioned element must report
    /// its CSS rect, and a selector crop must contain (only) that element.
    #[test]
    fn selector_rect_and_crop() {
        let html = r##"<html><body style="margin:0">
            <div id="target" style="position:absolute;left:100px;top:150px;width:60px;height:40px;background:#ff0000"></div>
        </body></html>"##;

        // 1. Rect math: uncropped render (selector_all), selector reports the element rect.
        let full = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#target"), true)
            .expect("full render");
        assert_eq!(full.rects.len(), 1);
        let r = full.rects[0];
        assert!((r.x - 100.0).abs() <= 1.0, "x: {r:?}");
        assert!((r.y - 150.0).abs() <= 1.0, "y: {r:?}");
        assert!((r.width - 60.0).abs() <= 1.0, "width: {r:?}");
        assert!((r.height - 40.0).abs() <= 1.0, "height: {r:?}");

        // 2. Crop: rendered image is exactly the element, mostly red.
        let crop = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#target"), false)
            .expect("crop render");
        assert_eq!((crop.pixel_width, crop.pixel_height), (60, 40));
        assert_eq!(crop.rects[0], r);
        let red = count_color(&crop.png, |(r, g, b)| r > 200 && g < 80 && b < 80);
        // Layout rounding may shave edge pixels; the interior must be red.
        assert!(
            red > 60 * 40 * 9 / 10,
            "expected mostly red crop, got {red}/{} red pixels",
            60 * 40
        );

        // 3. selector_all: every match reported, no crop (image is viewport-sized).
        let html2 = r##"<html><body style="margin:0">
            <div class="it" style="position:absolute;left:10px;top:20px;width:30px;height:30px;background:#00ff00"></div>
            <div class="it" style="position:absolute;left:50px;top:60px;width:30px;height:30px;background:#0000ff"></div>
        </body></html>"##;
        let all = render_html_to_png(html2, "https://example.com/", 200, 200, 1.0, false, Some(".it"), true)
            .expect("all render");
        assert_eq!(all.rects.len(), 2);
        assert!((all.rects[0].x - 10.0).abs() <= 1.0);
        assert!((all.rects[1].x - 50.0).abs() <= 1.0);
        assert_eq!((all.pixel_width, all.pixel_height), (200, 200));

        // 4. No match is an error for crop mode, empty for all-mode.
        assert!(render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#nope"), false).is_err());
        let none = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#nope"), true)
            .expect("all-mode returns empty");
        assert!(none.rects.is_empty());

        // 5. Inline element with box children: rect falls back to the union of
        //    descendant boxes (the <a> itself carries no Taffy box).
        let html3 = r##"<html><body style="margin:0">
            <a id="link" href="#"><div style="position:absolute;left:200px;top:250px;width:50px;height:70px;background:#ff0000"></div></a>
        </body></html>"##;
        let inline = render_html_to_png(html3, "https://example.com/", 800, 600, 1.0, false, Some("#link"), true)
            .expect("inline render");
        assert_eq!(inline.rects.len(), 1);
        assert!((inline.rects[0].x - 200.0).abs() <= 1.0, "{:?}", inline.rects[0]);
        assert!((inline.rects[0].width - 50.0).abs() <= 1.0, "{:?}", inline.rects[0]);
        assert!((inline.rects[0].height - 70.0).abs() <= 1.0, "{:?}", inline.rects[0]);

        // 6. Pure-inline (text-only) element crops are rejected with a clear error.
        let html4 = r##"<html><body style="margin:0"><p><a id="textonly" href="#">just text</a></p></body></html>"##;
        assert!(render_html_to_png(html4, "https://example.com/", 800, 600, 1.0, false, Some("#textonly"), false)
            .is_err());
    }

    /// Decode a small PNG and count pixels matching `pred`.
    fn count_color(png_bytes: &[u8], pred: impl Fn((u8, u8, u8)) -> bool) -> usize {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("png read_info");
        let mut buf = vec![0; reader.output_buffer_size().expect("png output buffer size")];
        let info = reader.next_frame(&mut buf).expect("png decode");
        buf[..info.buffer_size()]
            .chunks(4)
            .filter(|px| pred((px[0], px[1], px[2])))
            .count()
    }
}
