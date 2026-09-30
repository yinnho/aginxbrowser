//! The session Screenshot command's body. Split from the manager loop —
//! ARCHITECTURE.md §6 P2 god-file ratchet (#86). Closes over only the
//! page and its six request parameters.
use crate::page::{BandFrameCache, Page};

/// #185: clamp a requested device-pixel ratio. 1.0..=3.0 covers every
/// shipping display (1x desktop, 1.5/2x laptop, 3x phone); outside that or
/// non-finite falls back to 1× rather than allocating a 10× bitmap.
pub(crate) fn resolve_dpr(dpr: Option<f32>) -> f32 {
    dpr.filter(|d| d.is_finite())
        .map(|d| d.clamp(1.0, 3.0))
        .unwrap_or(1.0)
}

/// A cached band frame as the HTTP JSON response — shared by the poll
/// cache-hit path and the mid-navigation serve (#193), so both faces
/// return byte-identical shapes.
pub(crate) fn cached_frame_json(url: &str, width: u32, height: u32, png: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    serde_json::json!({
        "url": url,
        "width": width,
        "height": height,
        "image_base64": STANDARD.encode(png),
        "format": "png",
    })
    .to_string()
}

/// The no-argument default is the hosted live page's frame poll: it paints
/// the LIVE tree's viewport band. The serialized re-parse below drops
/// everything Chrome's outerHTML drops — dirty form values first of all:
/// the live page exists to watch the agent type, and typed text lives in
/// NodeData::Element::live_value, which a re-parse of page.content()
/// cannot see. Any explicit size/full_page/selector request keeps the
/// re-parse path unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) async fn screenshot(
    page: &mut Page,
    width: Option<u32>,
    height: Option<u32>,
    full_page: bool,
    selector: Option<&str>,
    selector_all: bool,
    dpr: Option<f32>,
) -> Result<String, String> {
    let url = page.url();
    let scale = resolve_dpr(dpr);

    // Default path: the live band, with the CDP screencast pump's damage
    // signature (realm gen, dom epoch, layout rev, scroll, viewport) plus
    // the requested dpr checked FIRST (#189). A poll on a static page —
    // browser86's display path, the hosted live view — used to pay a full
    // band re-raster + PNG encode every call (4-13s on a settled baidu);
    // an unchanged signature now returns the cached PNG at base64 cost.
    // Viewport and scroll come from the engine-side mirrors the screencast
    // path uses, so HTTP frames and screencast frames key identically (and
    // the poll no longer needs the four JS evaluates to size itself).
    if !full_page && selector.is_none() && width.is_none() && height.is_none() {
        let (sx, sy) = page.inner.scroll_offset();
        let (vw, vh) = page.inner.effective_viewport();
        let sig = (
            page.inner.realm_gen(),
            page.inner.dom_epoch(),
            page.inner.layout_rev(),
            sx,
            sy,
            vw,
            vh,
            scale,
        );
        if let Some(hit) = page.band_frame_cache.as_ref() {
            if hit.sig == sig {
                return Ok(cached_frame_json(&url, hit.width, hit.height, &hit.png));
            }
        }
        let vp = (vw.max(1.0), vh.max(1.0));
        let grab = |scale: f32| {
            if (scale - 1.0).abs() < f32::EPSILON {
                page.inner.viewport_band_frame(sx, sy, vp)
            } else {
                page.inner.viewport_band_frame_dpr(sx, sy, vp, scale)
            }
        };
        if let Some((frame, missing)) = grab(scale) {
            // Lazily fetch the images the band found missing and repaint
            // once — a frame with placeholders beats a stall (same deal as
            // the video pump).
            let frame = if missing.is_empty() {
                frame
            } else {
                page.inner.fetch_band_images(missing).await;
                grab(scale).map(|(f, _)| f).unwrap_or(frame)
            };
            if let Ok(png) = crate::pages::png_of(frame.width, frame.height, &frame.rgba) {
                page.band_frame_cache = Some(BandFrameCache {
                    sig,
                    width: frame.width,
                    height: frame.height,
                    png: png.clone(),
                });
                return Ok(cached_frame_json(&url, frame.width, frame.height, &png));
            }
        }
        // No live band (pre-navigation) or encode failure: fall through to
        // the re-parse path below.
    }

    let vw = page.evaluate_with_timeout("innerWidth", crate::page::INTERACTION_EVAL_TIMEOUT);
    let vh = page.evaluate_with_timeout("innerHeight", crate::page::INTERACTION_EVAL_TIMEOUT);
    let w = width.unwrap_or_else(|| vw.as_f64().unwrap_or(1280.0) as u32).max(1);
    let h = height.unwrap_or_else(|| vh.as_f64().unwrap_or(800.0) as u32).max(1);

    let html = page.content();
    let resources =
        crate::screenshot::prefetch_render_resources(page, &url, &html, w as f32).await;
    crate::screenshot::render_html_to_png_diting(
        &html,
        &url,
        w,
        h,
        scale,
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

#[cfg(test)]
mod tests {
    use super::screenshot;
    use crate::page::Page;
    use diting::diting_browser::lifecycle::WaitUntil;
    use diting::diting_browser::{BrowserContext, Page as EnginePage};
    use std::net::TcpListener;
    use std::sync::Arc;

    // Two structurally identical static documents that differ only in
    // paint color — the exact shape under which per-realm (epoch, rev)
    // collide across a navigation (#189).
    const PAGE_A: &str = "<html><body style=\"margin:0\"><div style=\"width:200px;height:200px;background:#ff0000\"></div></body></html>";
    const PAGE_B: &str = "<html><body style=\"margin:0\"><div style=\"width:200px;height:200px;background:#0000ff\"></div></body></html>";

    fn spawn_two_page_server(a: &'static str, b: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let req = String::from_utf8_lossy(&buf);
                let body = if req.contains("GET /b.html") { b } else { a };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, resp.as_bytes());
            }
        });
        port
    }

    fn test_page() -> Page {
        let context = Arc::new(BrowserContext::with_storage_and_network(
            "shot-test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: the fixture server is 127.0.0.1
            None,
        ));
        Page {
            inner: EnginePage::new("shot-page".into(), context.clone()),
            context,
            band_frame_cache: None,
        }
    }

    fn dims(resp: &str) -> (u64, u64) {
        let v: serde_json::Value = serde_json::from_str(resp).unwrap();
        (v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap())
    }

    /// Compare the pixel payload only — the `url` field differs across
    /// navigations, which would mask a stale-cache hit.
    fn pixels(resp: &str) -> String {
        serde_json::from_str::<serde_json::Value>(resp).unwrap()["image_base64"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn static_page_second_poll_hits_cache() {
        let port = spawn_two_page_server(PAGE_A, PAGE_B);
        let mut page = test_page();
        page.inner
            .navigate_with_wait(&format!("http://127.0.0.1:{port}/a.html"), WaitUntil::Load)
            .await
            .expect("navigate fixture");
        page.inner.settle_until_idle(2000).await;

        let first = screenshot(&mut page, None, None, false, None, false, None)
            .await
            .expect("first frame");
        let paints = page.inner.band_paint_count();
        let (w1, h1) = dims(&first);

        let second = screenshot(&mut page, None, None, false, None, false, None)
            .await
            .expect("second frame");
        assert_eq!(pixels(&second), pixels(&first), "static page: second poll returns the cached frame");
        assert_eq!(
            page.inner.band_paint_count(),
            paints,
            "cached poll must not re-raster"
        );

        // A different dpr is a different cache key — never served the 1x
        // bitmap (#185 interplay).
        let hidp = screenshot(&mut page, None, None, false, None, false, Some(2.0))
            .await
            .expect("dpr=2 frame");
        let (w2, h2) = dims(&hidp);
        assert_eq!((w2, h2), (w1 * 2, h1 * 2), "dpr=2 rasterizes at device resolution");

        // Damage: a style mutation must invalidate the cache.
        page.inner
            .evaluate("document.querySelector('div').style.background = '#00ff00'");
        let third = screenshot(&mut page, None, None, false, None, false, None)
            .await
            .expect("third frame");
        assert_ne!(third, first, "a style mutation must re-raster");
        assert!(page.inner.band_paint_count() > paints);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn navigation_between_static_pages_invalidates_cache() {
        let port = spawn_two_page_server(PAGE_A, PAGE_B);
        let mut page = test_page();
        page.inner
            .navigate_with_wait(&format!("http://127.0.0.1:{port}/a.html"), WaitUntil::Load)
            .await
            .expect("navigate a");
        page.inner.settle_until_idle(2000).await;
        let a = screenshot(&mut page, None, None, false, None, false, None)
            .await
            .expect("frame a");

        // Structurally identical document: same epoch, same layout rev,
        // same scroll, same viewport — only the realm generation differs.
        page.inner
            .navigate_with_wait(&format!("http://127.0.0.1:{port}/b.html"), WaitUntil::Load)
            .await
            .expect("navigate b");
        page.inner.settle_until_idle(2000).await;
        let b = screenshot(&mut page, None, None, false, None, false, None)
            .await
            .expect("frame b");
        assert_ne!(
            pixels(&b),
            pixels(&a),
            "two static pages collide on (epoch, rev, scroll, viewport); realm gen must break the tie"
        );
    }
}
