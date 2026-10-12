//! Stateless output entrypoints: screenshot / video / pdf.
//!
//! Thin glue over live-page paint (`screenshot` / `video` / `pages`). Gated on
//! the `screenshot` feature — same as the HTTP routes that call these.

#![cfg(feature = "screenshot")]

use super::browser::{build_browser, inject_cookies, run_on_local_runtime};
use crate::{ScreenshotRequest, ScreenshotResponse};
use anyhow::Result;

/// /screenshot: render the JS-rendered DOM of a page to a PNG via inlined Blitz.
///
/// Unlike /fetch (which can short-circuit to raw HTTP for static pages), this
/// always drives the diting browser so SPA/JS-rendered content is captured.
/// The page's `document.documentElement.outerHTML` is then fed to Blitz for
/// layout + paint — no Chromium. Sub-resources (images, head stylesheets) are
/// pre-fetched through the page's own HTTP client (same cookies/UA/proxy, plus
/// stealth TLS when enabled) and served to Blitz synchronously by
/// PrefetchedNetProvider; misses answer empty so nothing blocks paint.
#[cfg(feature = "screenshot")]
pub fn do_screenshot(req: ScreenshotRequest) -> Result<ScreenshotResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport before navigation (same as /video and /pdf):
            // JS-time layout (media queries) — and a bare SVG document's
            // root intrinsic — must agree with the requested render size.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            page.goto(&req.url).await?;

            if let Some(wait) = req.wait_secs {
                page.settle(wait * 1000).await;
            }

            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            // JS-rendered DOM — the same source /fetch uses for OutputFormat::Html.
            let html = page.content();
            // Pre-fetch while the page (its cookie'd HTTP client) is still alive.
            let resources = crate::screenshot::prefetch_render_resources(
                &page,
                &final_url,
                &html,
                req.width as f32,
            )
            .await;
            drop(page);
            drop(browser);

            // Render off-thread-ish: layout/paint is sync and CPU-bound.
            // We're already on a blocking runtime thread, so just call it directly.
            // Default engine is diting — our own css+layout+paint stack (no
            // Stylo/vello/parley in the path). engine=blitz opts back into
            // the Blitz reference pipeline for comparison renders — only
            // available when compiled with the `blitz-reference` feature.
            let rendered = if req.engine.as_deref() == Some("blitz") {
                #[cfg(feature = "blitz-reference")]
                {
                    crate::screenshot_reference::render_html_to_png(
                        &html,
                        &final_url,
                        req.width,
                        req.height,
                        req.scale,
                        req.full_page,
                        req.selector.as_deref(),
                        req.selector_all,
                        Some(&resources),
                    )?
                }
                #[cfg(not(feature = "blitz-reference"))]
                {
                    anyhow::bail!(
                        "engine=\"blitz\" requires the blitz-reference feature (this build renders with diting only)"
                    );
                }
            } else {
                crate::screenshot::render_html_to_png_diting(
                    &html,
                    &final_url,
                    req.width,
                    req.height,
                    req.scale,
                    req.full_page,
                    req.selector.as_deref(),
                    req.selector_all,
                    &[],
                    Some(&resources),
                )?
            };

            // Computed before the response struct moves `final_url`.
            let css_urls = crate::screenshot::stylesheet_hrefs(&html, &final_url);
            Ok(ScreenshotResponse {
                url: final_url,
                title,
                width: rendered.pixel_width,
                height: rendered.pixel_height,
                image_base64: base64_png(&rendered.png),
                format: "png".to_string(),
                selector_rects: if req.selector.is_some() {
                    Some(rendered.rects)
                } else {
                    None
                },
                selector_rects_diting: match req.selector.as_deref().filter(|_| req.diting_rects) {
                    Some(sel) => {
                        // css_urls computed above the response struct (before
                        // `final_url` moved) — link hrefs resolved absolute,
                        // not a `.css` suffix guess (MediaWiki's load.php
                        // sheets have no extension).
                        crate::screenshot::element_rects_diting(
                            &html,
                            sel,
                            req.selector_all,
                            req.width as f32,
                            req.height as f32,
                            // External <link> sheet bodies the prefetch pass already
                            // fetched — feed them to diting so its cascade sees what
                            // Blitz saw. Inline <style> blocks come from the HTML.
                            Some(
                                &resources
                                    .iter()
                                    .filter(|(k, v)| {
                                        !v.is_empty()
                                            && (css_urls.contains(k.as_str())
                                                || k.ends_with(".css"))
                                    })
                                    .map(|(_, v)| String::from_utf8_lossy(v.as_ref()).into_owned())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                        )
                        .ok()
                    }
                    None => None,
                },
            })
        })
    })
}

#[cfg(feature = "screenshot")]
fn base64_png(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(bytes)
}

/// /video: render the page's registered timelines (window.__timelines —
/// GSAP-style objects with `duration()` + `pause(t)`) to an MP4.
///
/// Unlike /screenshot, which extracts outerHTML and renders offline, this
/// needs the LIVE page: the timelines live in the page's own V8 isolate, so
/// the seek loop (pause(t) → viewport band paint → ffmpeg stdin) runs while
/// the browser is still up. Requires ffmpeg on PATH. See src/video.rs for
/// the protocol.
#[cfg(feature = "screenshot")]
pub fn do_video(req: crate::VideoRequest) -> Result<crate::VideoResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport so JS-time layout (media queries) and the band
            // paint below agree on the requested size.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            page.goto(&req.url).await?;

            let opts = crate::video::TimelineVideoOptions {
                fps: req.fps,
                viewport: (req.width as f32, req.height as f32),
                hold_tail_secs: req.hold_tail_secs,
                max_duration_secs: req.max_duration_secs,
                wait_timelines: std::time::Duration::from_millis(req.wait_timelines_ms),
                audio: req.audio.map(|a| crate::video::AudioTrack {
                    url: a.url,
                    volume: a.volume,
                    fade_out_secs: a.fade_out_secs,
                    loop_audio: a.loop_audio,
                }),
                narration: req
                    .narration
                    .into_iter()
                    .map(|c| crate::video::NarrationClip {
                        url: c.url,
                        start_secs: c.start_secs,
                        volume: c.volume,
                    })
                    .collect(),
                subtitles_srt: req.subtitles_srt,
                subtitles_language: req.subtitles_language,
                burn_subtitles: req.burn_subtitles,
            };
            let video = crate::video::render_timeline_video(&mut page.inner, &opts).await?;
            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            Ok(crate::VideoResponse {
                url: final_url,
                title,
                frames: video.frames,
                timeline_secs: video.timeline_secs,
                duration_secs: video.duration_secs,
                width: video.width,
                height: video.height,
                video_base64: base64_png(&video.mp4),
                has_audio: video.has_audio,
                has_subtitles: video.has_subtitles,
                burned_subtitles: video.burned_subtitles,
                format: "mp4".to_string(),
            })
        })
    })
}

/// /pdf: cut the page into a set of pages and package as PDF (default),
/// per-page PNGs, image-based PPTX/DOCX, or native (editable) PPTX — the
/// logical page-slicing layer (print pagination at block boundaries, or one
/// page per `selector` match in slides mode). Same live-page shape as
/// /video: the geometry comes from the live tree's layout, so the whole run
/// happens while the browser is up. See src/pages.rs, src/ooxml.rs, and
/// src/pptx_native.rs.
#[cfg(feature = "screenshot")]
pub fn do_pdf(req: crate::PdfRequest) -> Result<crate::PdfResponse> {
    run_on_local_runtime(move |_rt| {
        Box::pin(async move {
            crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
            let browser = build_browser(req.use_proxy, &req.url, req.tls_fingerprint.as_deref())?;
            inject_cookies(&browser, &req.cookies, &req.url);
            let mut page = browser.new_page().await?;
            // Pin the viewport so JS-time layout and the band paints agree
            // on the requested page width.
            page.set_viewport_override(req.width as f32, req.height as f32, false, None);
            // Print-media export semantics (what Chrome's save-as-PDF runs
            // under): decks that park their page-per-slide layout in an
            // `@media print` block un-stack here — the flex-row +
            // translateX carousel renders one slide per page instead of the
            // overflow-clipped screen frame. No-op for pages without a
            // print block. Realm replay carries it past the navigation.
            page.inner
                .set_emulated_media(None, Some(Some("print".into())));
            page.goto(&req.url).await?;

            // Native PPTX branches BEFORE the page-set render: its walker
            // collects element rects itself — painting page bands first
            // would be wasted work. Slides semantics only (one slide per
            // selector match), so a missing selector is an explicit error.
            if req.format.eq_ignore_ascii_case("pptx-native") {
                let selector = req.selector.clone().ok_or_else(|| {
                    anyhow::anyhow!("format=pptx-native requires `selector` (one slide per match)")
                })?;
                let (bytes, slides) =
                    crate::pptx_native::pptx_native_deck(&mut page.inner, &selector, req.max_pages)
                        .await?;
                return Ok(crate::PdfResponse {
                    url: page.url(),
                    title: {
                        let v = page.evaluate("document.title");
                        v.as_str()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    },
                    pages: slides,
                    width: req.width,
                    height: req.height,
                    pdf_base64: None,
                    pages_base64: Vec::new(),
                    pptx_base64: Some(base64_png(&bytes)),
                    docx_base64: None,
                    format: "pptx-native".to_string(),
                });
            }

            let mode = match req.selector.as_deref() {
                Some(sel) => crate::pages::PageMode::Slides(sel.to_string()),
                None => crate::pages::PageMode::Print,
            };
            // Only the PDF path consumes the vector text layer — collecting
            // it strips vectorizable text from the band raster, so the
            // raster-only formats must not turn it on.
            let format = if req.format.eq_ignore_ascii_case("png") {
                "png"
            } else if req.format.eq_ignore_ascii_case("pptx") {
                "pptx"
            } else if req.format.eq_ignore_ascii_case("docx") {
                "docx"
            } else {
                "pdf"
            };
            let opts = crate::pages::PagePumpOptions {
                mode,
                page_size: (req.width as f32, req.height as f32),
                max_pages: req.max_pages,
                collect_text: format == "pdf",
            };
            let set = crate::pages::render_page_set(&mut page.inner, &opts).await?;
            tracing::debug!(
                "page set: {} pages, content {}x{}, break origins {:?}",
                set.pages.len(),
                set.content_size.0 as u32,
                set.content_size.1 as u32,
                set.pages
                    .iter()
                    .map(|p| p.origin_y as u32)
                    .collect::<Vec<_>>()
            );
            let final_url = page.url();
            let title: Option<String> = {
                let v = page.evaluate("document.title");
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };

            // PNG wants per-page PNGs; the other three formats all embed
            // per-page JPEGs, so they share one encode pass.
            let mut pngs: Vec<String> = Vec::new();
            let mut jpegs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            if format == "png" {
                for p in &set.pages {
                    let png = crate::pages::png_of(p.width, p.height, &p.rgba)
                        .map_err(anyhow::Error::msg)?;
                    pngs.push(base64_png(&png));
                }
            } else {
                for p in &set.pages {
                    let jpeg = crate::pages::jpeg_of(p.width, p.height, &p.rgba, req.jpeg_quality)
                        .map_err(anyhow::Error::msg)?;
                    jpegs.push((p.width, p.height, jpeg));
                }
            }
            let refs: Vec<(u32, u32, &[u8])> = jpegs
                .iter()
                .map(|(w, h, j)| (*w, *h, j.as_slice()))
                .collect();
            let pdf_refs: Vec<(u32, u32, &[u8], &[diting::diting_layout::paint::PdfOp])> = jpegs
                .iter()
                .zip(set.text_ops.iter())
                .map(|((w, h, j), ops)| (*w, *h, j.as_slice(), ops.as_slice()))
                .collect();
            let (pdf_base64, pptx_base64, docx_base64) = match format {
                "pptx" => (
                    None,
                    Some(base64_png(&crate::ooxml::pptx_of_pages(&refs))),
                    None,
                ),
                "docx" => (
                    None,
                    None,
                    Some(base64_png(&crate::ooxml::docx_of_pages(&refs))),
                ),
                "pdf" => (
                    Some(base64_png(&crate::pages::pdf_of_pages(&pdf_refs))),
                    None,
                    None,
                ),
                _ => (None, None, None),
            };

            Ok(crate::PdfResponse {
                url: final_url,
                title,
                pages: set.pages.len(),
                width: req.width,
                height: req.height,
                pdf_base64,
                pages_base64: pngs,
                pptx_base64,
                docx_base64,
                format: format.to_string(),
            })
        })
    })
}

