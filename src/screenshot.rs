//! Screenshot rendering: feed a JS-rendered HTML string to Blitz (Stylo +
//! Taffy layout, vello_cpu paint) and return a PNG.
//!
//! This module is the "paint what the agent already sees" layer — aginxbrowser's
//! V8 path has already run the page's JS and produced the final DOM; we hand
//! that DOM to Blitz purely for layout + rasterization. No sub-resource fetches
//! happen here (Blitz's DummyNetProvider is a no-op, and upstream #636's
//! `is_noop()` gating stops `<head>` stylesheets from blocking paint forever —
//! see the `screenshot` feature note in Cargo.toml).

use anyhow::Result;
use blitz_dom::{DocumentConfig, util::Color};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;

/// Render an HTML string to a PNG (RGBA bytes).
///
/// `base_url` is used to resolve relative URLs in the document (mostly moot
/// since we don't fetch sub-resources, but Blitz uses it for `<base>` etc.).
/// When `full_page` is true, the render height tracks the document's computed
/// content height (capped at 16000px to bound memory); otherwise the requested
/// `height` is used (viewport-sized).
pub fn render_html_to_png(
    html: &str,
    base_url: &str,
    width: u32,
    height: u32,
    scale: f32,
    full_page: bool,
) -> Result<Vec<u8>> {
    if html.is_empty() {
        anyhow::bail!("render_html_to_png: empty HTML (page content() returned nothing — navigation may have failed)");
    }
    let scale_f = scale.max(0.1) as f64;

    // No net provider → DummyNetProvider (no-op). Upstream #636's is_noop()
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

    let render_width = (width as f64 * scale_f) as u32;
    let render_height = if full_page {
        // Track real content height, clamped to bound the RGBA buffer (~width*16000*4 bytes).
        ((computed_height.max(height as f64)).min(16000.0) * scale_f) as u32
    } else {
        (height as f64 * scale_f) as u32
    };

    if render_width == 0 || render_height == 0 {
        anyhow::bail!(
            "render_html_to_png: zero-sized output ({}x{}; computed_height={})",
            render_width, render_height, computed_height
        );
    }

    // Paint white background, then the document. vello_cpu is pure CPU — no GPU/wgpu.
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

    // Encode RGBA buffer → PNG bytes.
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
        "screenshot: {}x{} @ scale {} → {} PNG bytes (computed_height={})",
        render_width, render_height, scale_f, png_bytes.len(), computed_height
    );

    Ok(png_bytes)
}

// Suppress unused-import warning for anyrender::PaintScene when the trait is
// only brought into scope for paint_scene's generic bound.
#[allow(unused_imports)]
use anyrender::PaintScene as _;
