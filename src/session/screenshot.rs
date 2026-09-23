//! The session Screenshot command's body. Split from the manager loop —
//! ARCHITECTURE.md §6 P2 god-file ratchet (#86). Closes over only the
//! page and its five request parameters.
use crate::page::Page;

/// The no-argument default is the hosted live page's frame poll: it paints
/// the LIVE tree's viewport band. The serialized re-parse below drops
/// everything Chrome's outerHTML drops — dirty form values first of all:
/// the live page exists to watch the agent type, and typed text lives in
/// NodeData::Element::live_value, which a re-parse of page.content()
/// cannot see. Any explicit size/full_page/selector request keeps the
/// re-parse path unchanged.
pub(super) async fn screenshot(
    page: &mut Page,
    width: Option<u32>,
    height: Option<u32>,
    full_page: bool,
    selector: Option<&str>,
    selector_all: bool,
) -> Result<String, String> {
    let url = page.url();
    // Default to the live viewport so a session_viewport override is what
    // the pixels show, not the render default.
    let vw = page.evaluate_with_timeout("innerWidth", crate::page::INTERACTION_EVAL_TIMEOUT);
    let vh = page.evaluate_with_timeout("innerHeight", crate::page::INTERACTION_EVAL_TIMEOUT);
    let w = width.unwrap_or_else(|| vw.as_f64().unwrap_or(1280.0) as u32).max(1);
    let h = height.unwrap_or_else(|| vh.as_f64().unwrap_or(800.0) as u32).max(1);

    let mut band_png: Option<(u32, u32, Vec<u8>)> = None;
    if !full_page && selector.is_none() && width.is_none() && height.is_none() {
        let mut num = |expr: &str| {
            page.evaluate_with_timeout(expr, crate::page::INTERACTION_EVAL_TIMEOUT)
                .as_f64()
                .unwrap_or(0.0) as f32
        };
        let (sx, sy) = (num("scrollX"), num("scrollY"));
        let vp = (w as f32, h as f32);
        if let Some((frame, missing)) = page.inner.viewport_band_frame(sx, sy, vp) {
            // Lazily fetch the images the band found missing and repaint
            // once — a frame with placeholders beats a stall (same deal as
            // the video pump).
            let frame = if missing.is_empty() {
                frame
            } else {
                page.inner.fetch_band_images(missing).await;
                page.inner
                    .viewport_band_frame(sx, sy, vp)
                    .map(|(f, _)| f)
                    .unwrap_or(frame)
            };
            band_png = crate::pages::png_of(frame.width, frame.height, &frame.rgba)
                .ok()
                .map(|png| (frame.width, frame.height, png));
        }
    }

    if let Some((pw, ph, png)) = band_png {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        Ok(serde_json::json!({
            "url": url,
            "width": pw,
            "height": ph,
            "image_base64": STANDARD.encode(&png),
            "format": "png",
        })
        .to_string())
    } else {
        let html = page.content();
        let resources =
            crate::screenshot::prefetch_render_resources(page, &url, &html, w as f32).await;
        crate::screenshot::render_html_to_png_diting(
            &html,
            &url,
            w,
            h,
            1.0,
            full_page,
            selector,
            selector_all,
            Some(&resources),
        )
        .map_err(|e| format!("screenshot failed: {e}"))
        .map(|rendered| {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            serde_json::json!({
                "url": url,
                "width": rendered.pixel_width,
                "height": rendered.pixel_height,
                "image_base64": STANDARD.encode(&rendered.png),
                "format": "png",
            })
            .to_string()
        })
    }
}
