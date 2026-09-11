//! Screenshot rendering via the inlined diting engine: feed a JS-rendered
//! HTML string to diting's own CSS cascade + Taffy box layout + paint stack
//! and return a PNG. The Blitz reference pipeline lives in
//! [`crate::screenshot_reference`] behind the opt-in `blitz-reference` feature —
//! this module is the whole renderer in a production build.
//!
//! This is the "paint what the agent already sees" layer — aginxbrowser's
//! V8 path has already run the page's JS and produced the final DOM; we
//! render that DOM, with no networking during layout: the caller pre-fetches
//! the visible sub-resources (images, head stylesheets) over HTTP via
//! [`prefetch_render_resources`] and hands them in as
//! [`PrefetchedResources`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

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

// ---------------------------------------------------------------------------
// Pre-fetched sub-resources
// ---------------------------------------------------------------------------

/// Bodies of sub-resources (images, stylesheets) fetched over HTTP before
/// rendering, keyed by absolute URL string. The key is the normalized
/// `Url::as_str()` form — the same string Blitz produces by resolving
/// `img src` / `link href` against `base_url` before calling the provider.
pub type PrefetchedResources = HashMap<String, Arc<Vec<u8>>>;

/// Absolute URLs of the `<link rel=stylesheet>` elements in `html`, resolved
/// against `base_url` — the same set [`prefetch_render_resources`] collects.
///
/// Prefetched bodies keyed by these URLs are stylesheets regardless of file
/// extension: MediaWiki serves its skin CSS from `/w/load.php?...`, which no
/// `.css` suffix test ever matches. Vector 2022's entire grid scaffold lives
/// in such a sheet, so the suffix test silently dropped it and the page
/// rendered as one stacked column.
pub fn stylesheet_hrefs(html: &str, base_url: &str) -> std::collections::HashSet<String> {
    use scraper::{Html, Selector};

    let mut out = std::collections::HashSet::new();
    let Ok(base) = url::Url::parse(base_url) else { return out };
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("link[href]") else { return out };
    for el in doc.select(&sel) {
        let is_css = el
            .value()
            .attr("rel")
            .map(|r| r.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("stylesheet")))
            .unwrap_or(false);
        if is_css {
            if let Some(href) = el.value().attr("href") {
                if let Ok(u) = base.join(href.trim()) {
                    out.insert(u.as_str().to_string());
                }
            }
        }
    }
    out
}

/// Cap on pre-fetched sub-resource URLs: this is fidelity polish for the
/// screenshot, not a scrape.
const MAX_RESOURCES: usize = 32;

/// Absolute sub-resource URLs the render paths request for `html`, resolved
/// against `base_url`, deduped, capped at [`MAX_RESOURCES`].
///
/// `<link rel=stylesheet>` first: a dropped head stylesheet blanks layout,
/// images are only fidelity polish. Then ONE winner per `<img>` — a
/// `srcset` is a priority list, not a set of resources (obscura#667 class):
/// fetching every candidate multiplies state-changing GETs (#662's damage)
/// and burns the cap so stylesheets stop fitting.
fn collect_resource_urls(html: &str, base: &url::Url, viewport_width: f32) -> Vec<url::Url> {
    use scraper::{ElementRef, Html, Selector};

    fn push_resolved(base: &url::Url, raw: &str, out: &mut Vec<url::Url>) {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("data:") || raw.starts_with("blob:") {
            return;
        }
        if out.len() >= MAX_RESOURCES {
            return;
        }
        if let Ok(u) = base.join(raw) {
            // A file:// page's subresources (requirements-aginxos P2): same
            // scheme only — the cross-scheme matrix pins file refs to file
            // pages — and only while the net-layer gate is open, so a closed
            // gate doesn't even queue reads that would be refused.
            let scheme_ok = match u.scheme() {
                "http" | "https" => true,
                "file" => base.scheme() == "file" && crate::diting_net::client::allow_file_access(),
                _ => false,
            };
            if scheme_ok && !out.contains(&u) {
                out.push(u);
            }
        }
    }

    // The single URL the diting layout path resolves for this img — a
    // scraper-DOM mirror of `diting_layout::resolve_img_source`: the
    // `<picture>` parent's first `<source>` whose media gate matches the
    // viewport, else the img's own srcset selection. Plain `src` is NOT
    // included (the caller pushes it separately for the Blitz path).
    fn selection_winner(el: ElementRef, vw: f32) -> Option<String> {
        let picture = ElementRef::wrap(el.parent()?).filter(|p| p.value().name() == "picture");
        if let Some(pic) = picture {
            for sib in pic.child_elements() {
                if sib == el {
                    break; // the img terminates the source scan
                }
                if sib.value().name() != "source" {
                    continue;
                }
                if !crate::diting_layout::media_matches_width(sib.value().attr("media"), vw) {
                    continue;
                }
                if let Some(srcset) = sib.value().attr("srcset") {
                    let cands = crate::diting_layout::image::parse_srcset(srcset);
                    if let Some(c) =
                        crate::diting_layout::image::select_srcset_candidate(&cands, vw)
                    {
                        return Some(c.url.clone());
                    }
                }
            }
        }
        let srcset = el.value().attr("srcset")?;
        let cands = crate::diting_layout::image::parse_srcset(srcset);
        crate::diting_layout::image::select_srcset_candidate(&cands, vw).map(|c| c.url.clone())
    }

    let mut urls: Vec<url::Url> = Vec::new();
    let doc = Html::parse_document(html);
    // Stylesheets first so image alternates can't starve them under the cap.
    if let Ok(sel) = Selector::parse("link[href]") {
        for el in doc.select(&sel) {
            let is_css = el
                .value()
                .attr("rel")
                .map(|r| r.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("stylesheet")))
                .unwrap_or(false);
            if is_css {
                if let Some(href) = el.value().attr("href") {
                    push_resolved(base, href, &mut urls);
                }
            }
        }
    }
    if let Ok(sel) = Selector::parse("img") {
        for el in doc.select(&sel) {
            // The diting path requests the selection winner; the Blitz path
            // requests the bare `src` (it has no srcset support) — collect
            // exactly that pair, not every candidate. Non-picture <source>
            // elements (video/audio) are requested by neither path.
            if let Some(winner) = selection_winner(el, viewport_width) {
                push_resolved(base, &winner, &mut urls);
            }
            if let Some(src) = el.value().attr("src") {
                push_resolved(base, src, &mut urls);
            }
        }
    }
    urls
}

/// Pre-fetch the sub-resources the render paths request for `html`
/// (see [`collect_resource_urls`]), resolved against `base_url`.
///
/// Uses the page's own HTTP client — same UA, cookie jar (session cookies
/// from the page load) and proxy the navigation used, plus the stealth TLS
/// fingerprint when enabled. Bounded: ≤[`MAX_RESOURCES`] URLs, ≤2 MiB per
/// body, 3s per request — this is fidelity polish for the screenshot, not a
/// scrape.
pub async fn prefetch_render_resources(
    page: &crate::page::Page,
    base_url: &str,
    html: &str,
    viewport_width: f32,
) -> PrefetchedResources {
    const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
    const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

    // Route through the same client the page navigated with: stealth wreq
    // when enabled, plain reqwest otherwise. The plain path carries the
    // document as Referer (domain-whitelist image CDNs reject bare requests);
    // the stealth path takes only a URL.
    async fn fetch_via(
        page: &crate::page::Page,
        u: &url::Url,
        doc: Option<&str>,
    ) -> Option<crate::diting_net::Response> {
        // `Network.setBlockedURLs` holds for render-path fetches too (same
        // hard block as the static loaders): a match is never prefetched.
        if page.inner.url_blocked(u.as_str()) {
            tracing::info!("Blocked prefetch by Network.setBlockedURLs: {}", u);
            return None;
        }
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = page.inner.stealth_client {
            return stealth.fetch(u).await.ok();
        }
        page.inner
            .http_client
            .fetch_subresource(u, doc)
            .await
            .ok()
    }

    let Ok(base) = url::Url::parse(base_url) else {
        return PrefetchedResources::new();
    };
    let urls = collect_resource_urls(html, &base, viewport_width);
    if urls.is_empty() {
        return PrefetchedResources::new();
    }
    let requested = urls.len();
    let doc_referrer = base.as_str().to_string();

    let futs = urls.into_iter().map(|u| {
        let doc_referrer = doc_referrer.clone();
        async move {
            let resp = tokio::time::timeout(
                PER_REQUEST_TIMEOUT,
                fetch_via(page, &u, Some(doc_referrer.as_str())),
            )
            .await
            .ok()
            .flatten()?;
            if resp.status != 200 || resp.body.is_empty() || resp.body.len() > MAX_BODY_BYTES {
                return None;
            }
            Some((u.as_str().to_string(), Arc::new(resp.body)))
        }
    });
    let map: PrefetchedResources = futures::future::join_all(futs).await.into_iter().flatten().collect();
    tracing::debug!(
        "screenshot prefetch: {}/{} sub-resources fetched",
        map.len(),
        requested
    );
    map
}

/// `scale` renders 1:1 CSS px (retina resample lands with the canvas
/// upscale batch). `full_page` tracks content height from the laid-out
/// rects, capped at 16000 like the Blitz path; a `selector` crop is an
/// RGBA window copy out of the full-page canvas (no PNG round-trip).
#[allow(clippy::too_many_arguments)]
pub fn render_html_to_png_diting(
    html: &str,
    base_url: &str, // stylesheet hrefs AND relative img srcs resolve against it
    width: u32,
    height: u32,
    _scale: f32,
    full_page: bool,
    selector: Option<&str>,
    selector_all: bool,
    resources: Option<&PrefetchedResources>,
) -> Result<RenderedScreenshot> {
    use crate::diting_layout::{paint, Rect as DitingRect};

    if html.is_empty() {
        anyhow::bail!("render_html_to_png_diting: empty HTML (page content() returned nothing - navigation may have failed)");
    }

    let tree = crate::diting_dom::tree_sink::parse_html(html);

    // Cascade input: inline <style> blocks, then the external sheet bodies
    // the prefetch pass already fetched (same join order as
    // element_rects_diting, so both engines see the same rules). A fetched
    // body counts as CSS when its URL is a <link rel=stylesheet> href — the
    // suffix alone misses extensionless sheet URLs like MediaWiki's load.php.
    let css_urls = stylesheet_hrefs(html, base_url);
    let mut css = String::new();
    if let Ok(style_els) = tree.query_selector_all("style") {
        for el in style_els {
            css.push_str(&tree.text_content(el));
            css.push('\n');
        }
    }
    if let Some(res) = resources {
        for (k, v) in res {
            if !v.is_empty() && (css_urls.contains(k.as_str()) || k.ends_with(".css")) {
                css.push_str(&String::from_utf8_lossy(v));
                css.push('\n');
            }
        }
    }
    let rules = crate::diting_css::parse_stylesheet_for(
        &css,
        (width as f32, height as f32),
        crate::diting_css::CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules);
    let fonts = crate::diting_fonts::font_book();

    // Image bytes: everything non-stylesheet the prefetch pass fetched,
    // keyed by absolute URL — the same key `resolve_img_source` produces by
    // joining relative srcs against `base_url`.
    let network_bytes: HashMap<String, std::sync::Arc<Vec<u8>>> = resources
        .map(|res| {
            res.iter()
                .filter(|(k, v)| (!css_urls.contains(k.as_str()) && !k.ends_with(".css")) && !v.is_empty())
                .map(|(k, v)| (k.clone(), std::sync::Arc::clone(v)))
                .collect()
        })
        .unwrap_or_default();
    let net_ref = (!network_bytes.is_empty()).then_some(&network_bytes);
    let (rects, items) = crate::diting_layout::layout_dom_with_paint_and_images(
        &tree,
        &styles,
        &fonts,
        width as f32,
        height as f32,
        net_ref,
        Some(base_url),
    );

    // Content height for full_page: the deepest laid-out bottom edge.
    let content_h = rects.values().map(|r| r.y + r.height).fold(0.0_f32, f32::max);

    // Selector resolution. Inline elements carry no box of their own (their
    // content belongs to the host block's inline layout) — fall back to the
    // union of descendant boxes, mirroring the Blitz path's element_rect.
    fn diting_rect(
        tree: &crate::diting_dom::DomTree,
        rects: &HashMap<crate::diting_dom::NodeId, DitingRect>,
        id: crate::diting_dom::NodeId,
    ) -> Option<ElementRect> {
        if let Some(r) = rects.get(&id) {
            return Some(ElementRect { x: r.x as f64, y: r.y as f64, width: r.width as f64, height: r.height as f64 });
        }
        fn union_into(
            tree: &crate::diting_dom::DomTree,
            rects: &HashMap<crate::diting_dom::NodeId, DitingRect>,
            id: crate::diting_dom::NodeId,
            acc: &mut Option<ElementRect>,
        ) {
            if let Some(r) = rects.get(&id) {
                let b = ElementRect { x: r.x as f64, y: r.y as f64, width: r.width as f64, height: r.height as f64 };
                *acc = Some(match acc.take() {
                    None => b,
                    Some(a) => ElementRect {
                        x: a.x.min(b.x),
                        y: a.y.min(b.y),
                        width: (a.x + a.width).max(b.x + b.width) - a.x.min(b.x),
                        height: (a.y + a.height).max(b.y + b.height) - a.y.min(b.y),
                    },
                });
            }
            for c in tree.children(id) {
                union_into(tree, rects, c, acc);
            }
        }
        let mut acc = None;
        union_into(tree, rects, id, &mut acc);
        acc
    }

    let mut out_rects: Vec<ElementRect> = Vec::new();
    let mut crop: Option<(f32, f32, f32, f32)> = None; // CSS px x, y, w, h
    if let Some(sel) = selector {
        let matched = tree
            .query_selector_all(sel)
            .map_err(|e| anyhow::anyhow!("invalid selector {sel:?}: {e}"))?;
        if selector_all {
            out_rects = matched.iter().filter_map(|&id| diting_rect(&tree, &rects, id)).collect();
        } else {
            let id = matched
                .first()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("selector {sel:?} matched no element"))?;
            let r = diting_rect(&tree, &rects, id).ok_or_else(|| {
                anyhow::anyhow!("selector {sel:?} matched an element with no layout box")
            })?;
            if r.width < 0.5 || r.height < 0.5 {
                anyhow::bail!(
                    "selector {sel:?} matched an element with no layout box ({}x{}). \
                     Inline elements (bare <a>/<span> with text) carry no box - \
                     target a block ancestor instead",
                    r.width, r.height
                );
            }
            let (w, h) = (r.width.min(16000.0), r.height.min(16000.0));
            crop = Some((r.x.max(0.0) as f32, r.y.max(0.0) as f32, w as f32, h as f32));
            out_rects.push(r);
        }
    }

    // One full-page canvas, then an RGBA window copy when cropping.
    let page_h = if full_page {
        content_h.max(height as f32).min(16000.0)
    } else {
        height as f32
    };
    let canvas_h = match crop {
        Some((_, cy, _, ch)) => page_h.max(cy + ch),
        None => page_h,
    }
    .max(1.0) as usize;
    let canvas_w = width.max(1) as usize;
    let mut canvas = paint::Canvas::new_filled(canvas_w, canvas_h, [255, 255, 255, 255]);
    paint::execute(&items, &fonts, &mut canvas);

    let (out_w, out_h, buffer): (u32, u32, Vec<u8>) = match crop {
        Some((cx, cy, cw, ch)) => {
            let (x0, y0) = (cx.max(0.0) as usize, cy.max(0.0) as usize);
            let (x1, y1) = (
                (x0 + cw as usize).min(canvas.width),
                (y0 + ch as usize).min(canvas.height),
            );
            let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
            let mut out = Vec::with_capacity(w * h * 4);
            for row in y0..y1 {
                let start = (row * canvas.width + x0) * 4;
                out.extend_from_slice(&canvas.data[start..start + w * 4]);
            }
            (w as u32, h as u32, out)
        }
        None => (canvas.width as u32, canvas.height as u32, canvas.data),
    };

    if out_w == 0 || out_h == 0 {
        anyhow::bail!(
            "render_html_to_png_diting: zero-sized output ({}x{}; content_height={})",
            out_w, out_h, content_h
        );
    }

    let mut png_bytes = Vec::with_capacity((out_w * out_h) as usize);
    {
        use std::io::Cursor;
        let mut encoder = png::Encoder::new(Cursor::new(&mut png_bytes), out_w, out_h);
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
        "screenshot(diting): {}x{} -> {} PNG bytes (content_height={}, rects={})",
        out_w, out_h, png_bytes.len(), content_h, out_rects.len()
    );

    Ok(RenderedScreenshot {
        png: png_bytes,
        pixel_width: out_w,
        pixel_height: out_h,
        rects: out_rects,
    })
}

pub fn element_rects_diting(
    html: &str,
    selector: &str,
    selector_all: bool,
    viewport_width: f32,
    viewport_height: f32,
    extra_css: Option<&str>,
) -> Result<Vec<ElementRect>> {
    use crate::diting_dom::tree_sink::parse_html;

    // Concatenate every <style> block's text in document order.
    let tree = parse_html(html);
    let mut css = String::new();
    if let Ok(style_els) = tree.query_selector_all("style") {
        for el in style_els {
            let text = tree.text_content(el);
            css.push_str(&text);
            css.push('\n');
        }
    }
    if let Some(extra) = extra_css {
        css.push_str(extra);
    }

    let rules = crate::diting_css::parse_stylesheet_for(
        &css,
        (viewport_width, viewport_height),
        crate::diting_css::CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules);

    let matched = tree
        .query_selector_all(selector)
        .map_err(|e| anyhow::anyhow!("invalid selector {selector:?}: {e}"))?;
    let ids: Vec<_> = if selector_all {
        matched
    } else {
        matched.into_iter().take(1).collect()
    };

    let rects = crate::diting_layout::layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), viewport_width, viewport_height);
    Ok(ids
        .iter()
        .filter_map(|id| rects.get(id).map(|r| ElementRect {
            x: r.x as f64,
            y: r.y as f64,
            width: r.width as f64,
            height: r.height as f64,
        }))
        .collect())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a small PNG and count pixels matching `pred` (test helper,
    /// mirrored in screenshot_reference's tests so both modules stay standalone).
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

    #[test]
    fn stylesheet_hrefs_resolve_extensionless_sheet_urls() {
        // MediaWiki shape: the skin CSS comes from a query-string URL with no
        // `.css` suffix; plain hosted sheets keep working; rel values that
        // merely mention stylesheets (preload/alternate) stay out.
        let html = r#"<html><head>
            <link rel="stylesheet" href="/w/load.php?lang=en&amp;modules=skins.vector&amp;only=styles">
            <link rel="stylesheet" href="https://cdn.example.com/skin.css">
            <link rel="stylesheet preload" as="style" href="/both.css">
            <link rel="alternate stylesheet" href="/alt.css">
            <link rel="preload" as="style" href="/pre.css">
            <link rel="icon" href="/favicon.ico">
        </head></html>"#;
        let set = stylesheet_hrefs(html, "https://en.wikipedia.org/wiki/HTTP");
        assert!(set.contains(
            "https://en.wikipedia.org/w/load.php?lang=en&modules=skins.vector&only=styles"
        ), "{set:?}");
        assert!(set.contains("https://cdn.example.com/skin.css"), "{set:?}");
        assert!(set.contains("https://en.wikipedia.org/both.css"), "{set:?}");
        assert!(!set.contains("https://en.wikipedia.org/pre.css"), "{set:?}");
        assert!(!set.contains("https://en.wikipedia.org/favicon.ico"), "{set:?}");
    }

    /// requirements-aginxos P2 residual, found by eyeballing the ON-probe
    /// screenshot: a file:// page rendered with default styles (black h1,
    /// gray img placeholder) while the live DOM showed the sheet applied —
    /// the collector had an http|https whitelist, so local subresources
    /// never entered the prefetch list. File refs are admitted for file
    /// pages with the gate open, never across schemes (the cross-scheme
    /// matrix), and not queued at all while the gate is closed.
    #[test]
    fn collect_resource_urls_admits_file_subresources_for_file_pages() {
        use crate::diting_net::client::file_access_test::file_access_guard;
        let html = concat!(
            r#"<html><head><link rel="stylesheet" href="style.css"></head>"#,
            r#"<body><img src="dot.png" width="16" height="16"></body></html>"#,
        );
        let file_base = url::Url::parse("file:///tmp/agx-shot/page.html").unwrap();
        let http_base = url::Url::parse("https://example.com/page.html").unwrap();

        {
            let _g = file_access_guard(true);
            let urls = collect_resource_urls(html, &file_base, 1280.0);
            assert!(urls.iter().any(|u| u.as_str().ends_with("/style.css")), "{urls:?}");
            assert!(urls.iter().any(|u| u.as_str().ends_with("/dot.png")), "{urls:?}");
            // Cross-scheme stays pinned: an http page never pulls file:// refs,
            // open gate or not.
            let urls = collect_resource_urls(
                r#"<html><link rel="stylesheet" href="file:///etc/passwd"></html>"#,
                &http_base,
                1280.0,
            );
            assert!(urls.is_empty(), "{urls:?}");
        }
        {
            // Gate closed: local refs are not even queued for fetching.
            let _g = file_access_guard(false);
            let urls = collect_resource_urls(html, &file_base, 1280.0);
            assert!(urls.is_empty(), "{urls:?}");
        }
    }

    /// End-to-end render on the diting stack (no Stylo/vello/parley): a
    /// simple SSR page must come back as a decodable PNG with the right
    /// dimensions, real ink (text + backgrounds), a full_page height that
    /// tracks content, and a selector crop that is exactly the element's
    /// window of the same canvas.
    #[test]
    fn diting_engine_renders_png_end_to_end() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            #banner { width: 300px; height: 60px; background: #2a5fd0; }
            #target { width: 120px; height: 50px; background: #ff0000; }
            p { margin: 0; font-size: 16px; color: #111111; }
        </style></head><body>
            <div id="banner"></div>
            <div id="target"></div>
            <p>谛听渲染第一图</p>
        </body></html>"##;

        let full = render_html_to_png_diting(html, "https://example.com/", 300, 200, 1.0, true, None, false, None)
            .expect("diting full render");
        assert_eq!(full.pixel_width, 300, "viewport width");
        // banner 60 + target 50 + one 16px line ≈ 78-80px of content: full_page
        // must track past the 200px floor? No — content < viewport floor keeps
        // the floor (same max() semantics as the blitz path).
        assert_eq!(full.pixel_height, 200, "full_page floors at viewport height");

        let banner = count_color(&full.png, |(r, g, b)| r < 100 && g > 60 && g < 140 && b > 150);
        assert!(banner > 300 * 55, "banner blue dominates its band: {banner}");
        let red = count_color(&full.png, |(r, g, b)| r > 200 && g < 80 && b < 80);
        assert!(red > 110 * 45, "target red fills its box: {red}");
        let ink = count_color(&full.png, |(r, g, b)| r < 80 && g < 80 && b < 80);
        assert!(ink > 20, "CJK text paints visible ink: {ink}");

        // Selector crop: a 120x50 window whose pixels are all red (the
        // #target box), proving the crop is the element's own canvas region.
        let cropped = render_html_to_png_diting(html, "https://example.com/", 300, 200, 1.0, false, Some("#target"), false, None)
            .expect("diting crop");
        assert_eq!((cropped.pixel_width, cropped.pixel_height), (120, 50), "crop = element box");
        assert_eq!(cropped.rects.len(), 1);
        let all_red = count_color(&cropped.png, |(r, g, b)| r > 200 && g < 80 && b < 80);
        assert_eq!(all_red, 120 * 50, "every cropped pixel is the target's red");

        // selector_all: no crop, rects for every match.
        let rects = render_html_to_png_diting(html, "https://example.com/", 300, 200, 1.0, false, Some("div"), true, None)
            .expect("diting selector_all");
        assert_eq!(rects.pixel_width, 300, "selector_all does not crop");
        assert_eq!(rects.rects.len(), 2, "both divs match in document order");
    }

    /// The diting element-coordinate entry answers the same wire shape as
    /// Blitz's selector_rects: a `<style>`-driven absolute box lands at its
    /// authored rect, `selector_all=false` returns just the first match, and
    /// an invalid selector is an Err. Extra CSS (external sheet bodies)
    /// participates in the cascade.
    #[test]
    fn diting_element_rects_from_html() {
        let html = r##"<html><head><style>
            #target { position: absolute; left: 100px; top: 150px; width: 60px; height: 40px; }
            .it { position: absolute; width: 30px; height: 30px; }
        </style></head><body style="margin:0">
            <div id="target"></div>
            <div class="it" id="a" style="left:10px;top:20px"></div>
            <div class="it" id="b" style="left:50px;top:60px"></div>
        </body></html>"##;

        // First match only.
        let one = element_rects_diting(html, "#target", false, 800.0, 600.0, None).expect("rects");
        assert_eq!(one.len(), 1);
        assert!((one[0].x - 100.0).abs() <= 1.0 && (one[0].y - 150.0).abs() <= 1.0,
            "absolute box at its authored position: {:?}", one[0]);
        assert!((one[0].width - 60.0).abs() <= 1.0 && (one[0].height - 40.0).abs() <= 1.0,
            "authored size: {:?}", one[0]);

        // selector_all: document order.
        let all = element_rects_diting(html, ".it", true, 800.0, 600.0, None).expect("all");
        assert_eq!(all.len(), 2);
        assert!((all[0].x - 10.0).abs() <= 1.0 && (all[1].x - 50.0).abs() <= 1.0);

        // No match is empty; an invalid selector errors.
        assert!(element_rects_diting(html, "#nope", true, 800.0, 600.0, None)
            .expect("empty ok").is_empty());
        assert!(element_rects_diting(html, "###", true, 800.0, 600.0, None).is_err());

        // extra_css participates in the cascade with correct specificity:
        // an id rule (from the extra sheet) overrides the class rule's
        // height — while #a's inline style keeps winning on left, exactly
        // the CSS cascade order (inline > id > class).
        let overridden =
            element_rects_diting(html, "#a", false, 800.0, 600.0, Some("#a { height: 90px; }"))
                .expect("extra css");
        assert_eq!(overridden.len(), 1);
        assert!((overridden[0].x - 10.0).abs() <= 1.0,
            "inline left survives: {:?}", overridden[0]);
        assert!((overridden[0].height - 90.0).abs() <= 1.0,
            "id rule beats class rule for height: {:?}", overridden[0]);
    }

    /// obscura#667 class, DOM side: a `srcset` is a priority list, not a set
    /// of resources — the collector must take the one candidate selection
    /// picks (plus the bare `src` the Blitz path requests, it has no srcset
    /// support), never every candidate.
    #[test]
    fn srcset_collects_the_selection_winner_not_every_candidate() {
        let html = r#"<img src="/a.jpg" srcset="/a1.jpg 480w, /a2.jpg 1024w, /a3.jpg 2048w">"#;
        let base = url::Url::parse("https://x.test/").unwrap();
        let urls = collect_resource_urls(html, &base, 1280.0);
        let strs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
        assert_eq!(
            strs,
            vec!["https://x.test/a2.jpg", "https://x.test/a.jpg"],
            "1024w is the largest fitting a 1280 viewport; got {strs:?}"
        );
    }

    /// `<picture>`: the first `<source>` whose media gate matches wins, one
    /// candidate from it, plus the fallback img's `src`. A `<source>` outside
    /// a picture (video/audio) is never fetched by either render path.
    #[test]
    fn picture_media_gate_picks_one_and_video_sources_are_skipped() {
        let html = r#"
            <picture>
              <source media="(min-width: 800px)" srcset="/wide1.jpg 480w, /wide2.jpg 1600w">
              <source srcset="/narrow.jpg 1x">
              <img src="/fallback.jpg">
            </picture>
            <video><source srcset="/v1.jpg 1x, /v2.jpg 2x"></video>
        "#;
        let base = url::Url::parse("https://x.test/").unwrap();
        let urls = collect_resource_urls(html, &base, 1280.0);
        let strs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
        assert_eq!(
            strs,
            vec!["https://x.test/wide1.jpg", "https://x.test/fallback.jpg"],
            "first source matches at 1280, its 480w candidate fits; got {strs:?}"
        );
    }

    // --- table layout (display:table family) ------------------------------

    /// Cells sit side by side, rows stack, and columns line up across rows
    /// with uniform per-column widths (the pre-measured maxima).
    #[test]
    fn table_cells_sit_side_by_side_with_columns_aligned_across_rows() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; font-size: 16px; }
        </style></head><body>
            <table>
                <tr><td id="a1" style="width:120px">alpha</td><td id="a2">b</td></tr>
                <tr><td id="b1">x</td><td id="b2">y</td></tr>
            </table>
        </body></html>"##;
        let rects = element_rects_diting(html, "td", true, 800.0, 600.0, None).expect("rects");
        assert_eq!(rects.len(), 4, "all four cells: {rects:?}");
        // Row 1: a1 then a2 on one line.
        assert!(rects[0].x < rects[1].x, "cells share a line: {rects:?}");
        assert!((rects[0].y - rects[1].y).abs() <= 1.0, "same band: {rects:?}");
        // Row 2 below, columns aligned with row 1.
        assert!(
            rects[2].y > rects[0].y + rects[0].height - 1.0,
            "row 2 starts below row 1: {rects:?}"
        );
        assert!(
            (rects[2].x - rects[0].x).abs() <= 1.0,
            "column 1 aligned: {:?} vs {:?}",
            rects[2],
            rects[0]
        );
        assert!(
            (rects[3].x - rects[1].x).abs() <= 1.0,
            "column 2 aligned: {:?} vs {:?}",
            rects[3],
            rects[1]
        );
        assert!(
            (rects[2].width - rects[0].width).abs() <= 1.0,
            "column 1 width uniform: {rects:?}"
        );
        assert!(
            (rects[3].width - rects[1].width).abs() <= 1.0,
            "column 2 width uniform: {rects:?}"
        );
    }

    /// An unstyled table shrink-to-fits its content instead of eating the
    /// whole body width (the biggest visible symptom of no table layout).
    #[test]
    fn table_shrink_to_fits_its_content_instead_of_full_width() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            td { padding: 0; }
        </style></head><body>
            <table><tr><td><div style="width:100px;height:40px"></div></td></tr></table>
        </body></html>"##;
        let table = element_rects_diting(html, "table", false, 800.0, 600.0, None).expect("table");
        assert_eq!(table.len(), 1);
        assert!(
            (table[0].width - 100.0).abs() <= 1.0,
            "table shrink-wraps to the 100px cell, not the 800px body: {:?}",
            table[0]
        );
    }

    /// An authored table width distributes its slack across columns
    /// proportionally to their content bases, keeping rows aligned.
    #[test]
    fn authored_table_width_distributes_over_columns() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { width: 360px; border-collapse: collapse; }
            td { padding: 0; height: 20px; }
        </style></head><body>
            <table>
                <tr><td id="l1" style="width:120px">a</td><td id="r1" style="width:60px">b</td></tr>
                <tr><td id="l2">c</td><td id="r2">d</td></tr>
            </table>
        </body></html>"##;
        let l1 = element_rects_diting(html, "#l1", false, 800.0, 600.0, None).expect("l1")[0];
        let r1 = element_rects_diting(html, "#r1", false, 800.0, 600.0, None).expect("r1")[0];
        let l2 = element_rects_diting(html, "#l2", false, 800.0, 600.0, None).expect("l2")[0];
        let r2 = element_rects_diting(html, "#r2", false, 800.0, 600.0, None).expect("r2")[0];
        assert!((l1.x + l1.width - r1.x).abs() <= 1.0, "row 1 columns adjacent: {l1:?} {r1:?}");
        assert!(
            (l1.width + r1.width - 360.0).abs() <= 1.0,
            "cells fill the 360px table: {l1:?} {r1:?}"
        );
        assert!((l2.x - l1.x).abs() <= 1.0 && (r2.x - r1.x).abs() <= 1.0, "rows aligned: {l1:?} {r1:?} {l2:?} {r2:?}");
        assert!((l2.width - l1.width).abs() <= 1.0 && (r2.width - r1.width).abs() <= 1.0, "column widths uniform: {l1:?} {r1:?} {l2:?} {r2:?}");
    }

    /// `border-collapse: collapse` = zero gaps between cells; the separate
    /// initial keeps Chrome's default 2px border-spacing.
    #[test]
    fn border_collapse_zero_gap_and_separate_two_px_gap() {
        let mk = |mode: &str| {
            format!(
                r##"<html><head><style>
            body {{ margin: 0; }}
            table {{ border-collapse: {mode}; }}
            td {{ width: 100px; height: 40px; padding: 0; }}
        </style></head><body>
            <table><tr><td id="l">a</td><td id="r">b</td></tr></table>
        </body></html>"##
            )
        };
        let gap = |mode: &str| {
            let html = mk(mode);
            let l = element_rects_diting(&html, "#l", false, 800.0, 600.0, None).expect("l")[0];
            let r = element_rects_diting(&html, "#r", false, 800.0, 600.0, None).expect("r")[0];
            r.x - (l.x + l.width)
        };
        let collapsed = gap("collapse");
        assert!(collapsed.abs() <= 0.5, "collapse = shared border, no gap: {collapsed}");
        let separate = gap("separate");
        assert!(
            (separate - 2.0).abs() <= 0.5,
            "separate = Chrome's default 2px border-spacing: {separate}"
        );
    }

    /// thead/tbody flatten into their tr children; the header row sits
    /// above the body row and its cells align with it.
    #[test]
    fn thead_tbody_flatten_into_rows() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td, th { padding: 0; height: 20px; }
        </style></head><body>
            <table>
                <thead><tr><th id="h">head</th></tr></thead>
                <tbody><tr><td id="b">body</td></tr></tbody>
            </table>
        </body></html>"##;
        let h = element_rects_diting(html, "#h", false, 800.0, 600.0, None).expect("h")[0];
        let b = element_rects_diting(html, "#b", false, 800.0, 600.0, None).expect("b")[0];
        assert!(b.y > h.y + 1.0, "tbody row below thead row: {h:?} {b:?}");
        assert!((h.x - b.x).abs() <= 1.0, "cells left-aligned: {h:?} {b:?}");
        assert!((h.width - b.width).abs() <= 1.0, "single column, uniform width: {h:?} {b:?}");
    }

    /// colspan: the spanning cell covers both columns exactly — same x as the
    /// first column cell below it, width = sum of the two column widths.
    #[test]
    fn colspan_cell_spans_the_columns_it_covers() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; font-size: 0; line-height: 0; }
            div { display: inline-block; }
        </style></head><body>
            <table>
                <tr><td colspan="2" id="wide"><div style="width:150px;height:20px"></div></td></tr>
                <tr><td id="c1"><div style="width:100px;height:20px"></div></td><td id="c2"><div style="width:50px;height:20px"></div></td></tr>
            </table>
        </body></html>"##;
        let wide = element_rects_diting(html, "#wide", false, 800.0, 600.0, None).expect("wide")[0];
        let c1 = element_rects_diting(html, "#c1", false, 800.0, 600.0, None).expect("c1")[0];
        let c2 = element_rects_diting(html, "#c2", false, 800.0, 600.0, None).expect("c2")[0];
        assert!((wide.x - c1.x).abs() <= 1.0, "spanning cell starts at col 1: {wide:?} {c1:?}");
        assert!(
            (wide.width - (c1.width + c2.width)).abs() <= 2.0,
            "spanning cell width = col1 + col2: {wide:?} {c1:?} {c2:?}"
        );
        assert!(
            (wide.x + wide.width - (c2.x + c2.width)).abs() <= 1.0,
            "spanning cell ends with col 2: {wide:?} {c2:?}"
        );
    }

    /// rowspan: the lifted cell spans both rows — top at row 1's top, bottom at
    /// row 2's bottom — and the cells to its right skip the occupied slot.
    #[test]
    fn rowspan_cell_spans_the_rows_it_covers() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; font-size: 0; line-height: 0; }
            div { display: inline-block; }
        </style></head><body>
            <table>
                <tr><td rowspan="2" id="tall"><div style="width:40px;height:80px"></div></td><td id="r1"><div style="width:40px;height:30px"></div></td></tr>
                <tr><td id="r2"><div style="width:40px;height:30px"></div></td></tr>
            </table>
        </body></html>"##;
        let tall = element_rects_diting(html, "#tall", false, 800.0, 600.0, None).expect("tall")[0];
        let r1 = element_rects_diting(html, "#r1", false, 800.0, 600.0, None).expect("r1")[0];
        let r2 = element_rects_diting(html, "#r2", false, 800.0, 600.0, None).expect("r2")[0];
        // both rows are 30px of content each; the 80px rowspan cell forces the
        // band to 80px, split evenly → 40px per row.
        assert!((tall.y - r1.y).abs() <= 1.0, "rowspan cell top = row 1 top: {tall:?} {r1:?}");
        assert!(
            (tall.height - (r1.height + r2.height)).abs() <= 2.0,
            "rowspan cell height = row1 + row2: {tall:?} {r1:?} {r2:?}"
        );
        assert!(
            (tall.y + tall.height - (r2.y + r2.height)).abs() <= 1.0,
            "rowspan cell bottom = row 2 bottom: {tall:?} {r2:?}"
        );
        assert!(
            (r1.x - r2.x).abs() <= 1.0 && r2.x >= tall.x + tall.width - 1.0,
            "row 2 cell right of the occupied slot: {tall:?} {r1:?} {r2:?}"
        );
    }

    /// rowspan + colspan together: a 2×2 spanning cell covers the full 2×2
    /// grid of normal cells that define the columns and rows.
    #[test]
    fn rowspan_and_colspan_combined() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; font-size: 0; line-height: 0; }
            div { display: inline-block; }
        </style></head><body>
            <table>
                <tr><td rowspan="2" colspan="2" id="big"><div style="width:90px;height:90px"></div></td><td id="a"><div style="width:30px;height:30px"></div></td></tr>
                <tr><td id="b"><div style="width:30px;height:30px"></div></td></tr>
                <tr><td id="c"><div style="width:30px;height:30px"></div></td><td id="d"><div style="width:60px;height:30px"></div></td><td id="e"><div style="width:30px;height:30px"></div></td></tr>
            </table>
        </body></html>"##;
        let big = element_rects_diting(html, "#big", false, 800.0, 600.0, None).expect("big")[0];
        let a = element_rects_diting(html, "#a", false, 800.0, 600.0, None).expect("a")[0];
        let c = element_rects_diting(html, "#c", false, 800.0, 600.0, None).expect("c")[0];
        let d = element_rects_diting(html, "#d", false, 800.0, 600.0, None).expect("d")[0];
        let e = element_rects_diting(html, "#e", false, 800.0, 600.0, None).expect("e")[0];
        // columns: col1=30, col2=60, col3=30 (from the third row); big covers
        // col1+col2 = 90.
        assert!((big.x - c.x).abs() <= 1.0, "big starts at col 1: {big:?} {c:?}");
        assert!(
            (big.width - (c.width + d.width)).abs() <= 2.0,
            "big width = col1 + col2: {big:?} {c:?} {d:?}"
        );
        assert!((a.x - e.x).abs() <= 1.0, "col 3 aligned across rows 1/3: {a:?} {e:?}");
        // big holds a 90px div and spans rows 1+2 (30px content each) → the
        // 30px deficit splits evenly, 45px per row, band = 90.
        assert!((big.height - 90.0).abs() <= 2.0, "big spans the full 2-row band: {big:?}");
        assert!(
            (c.y - (big.y + big.height)).abs() <= 1.0,
            "row 3 starts at big's bottom: {big:?} {c:?}"
        );
    }

    /// caption: a real box above the first row, spanning the table width.
    /// The element used to be dropped entirely (no box built), so wikipedia
    /// infobox `<caption>` titles were invisible.
    #[test]
    fn caption_renders_above_the_table() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; height: 20px; width: 100px; }
        </style></head><body>
            <table>
                <caption id="cap">Quarterly figures</caption>
                <tr><td id="c1">a</td><td id="c2">b</td></tr>
            </table>
        </body></html>"##;
        let cap = element_rects_diting(html, "#cap", false, 800.0, 600.0, None).expect("caption box")[0];
        let c1 = element_rects_diting(html, "#c1", false, 800.0, 600.0, None).expect("c1")[0];
        let c2 = element_rects_diting(html, "#c2", false, 800.0, 600.0, None).expect("c2")[0];
        assert!(cap.height >= 10.0, "caption carries its text: {cap:?}");
        assert!(
            c1.y >= cap.y + cap.height - 1.0,
            "first row starts below the caption: {cap:?} {c1:?}"
        );
        assert!(
            (cap.width - (c2.x + c2.width - c1.x)).abs() <= 2.0,
            "caption spans the table width: {cap:?} {c1:?} {c2:?}"
        );
    }

    /// table-layout: fixed — column widths come from the first row only;
    /// long content in later rows never widens a column. Auto (the initial)
    /// widens column 1 for the row-2 text; fixed holds the authored 100px
    /// and gives the auto column the leftover.
    #[test]
    fn table_layout_fixed_pins_first_row_column_widths() {
        let mk = |layout: &str| {
            format!(
                r##"<html><head><style>
            body {{ margin: 0; }}
            table {{ width: 300px; table-layout: {layout}; border-collapse: collapse; }}
            td {{ padding: 0; height: 20px; }}
        </style></head><body>
            <table>
                <tr><td id="a1" style="width:100px">a</td><td id="b1">b</td></tr>
                <tr><td id="a2">much wider content in row two</td><td id="b2">y</td></tr>
            </table>
        </body></html>"##
            )
        };
        let rect = |layout: &str, id: &str| {
            let html = mk(layout);
            element_rects_diting(&html, id, false, 800.0, 600.0, None).expect(id)[0]
        };
        let (a1, b1, a2) = (rect("fixed", "#a1"), rect("fixed", "#b1"), rect("fixed", "#a2"));
        assert!((a1.width - 100.0).abs() <= 1.0, "fixed: authored 100px holds: {a1:?}");
        assert!((a2.width - a1.width).abs() <= 1.0, "fixed: col 1 uniform across rows: {a1:?} {a2:?}");
        assert!((b1.width - 200.0).abs() <= 2.0, "fixed: auto column takes the 200px leftover: {b1:?}");
        let a1_auto = rect("auto", "#a1");
        assert!(
            a1_auto.width > 140.0,
            "auto: row-2 content widens col 1 past 100px: {a1_auto:?}"
        );
    }

    /// Fixed columns from <colgroup>/<col width>: the col element pins its
    /// column, and CSS2.2 §17.5.2.1 makes it outrank a first-row cell width.
    #[test]
    fn fixed_layout_honors_colgroup_col_widths() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { width: 300px; table-layout: fixed; border-collapse: collapse; }
            td { padding: 0; height: 20px; }
        </style></head><body>
            <table>
                <colgroup><col width="60"><col></colgroup>
                <tr><td id="a1" style="width:120px">a</td><td id="b1">b</td></tr>
                <tr><td id="a2">long content that wants a wide column</td><td id="b2">y</td></tr>
            </table>
        </body></html>"##;
        let a1 = element_rects_diting(html, "#a1", false, 800.0, 600.0, None).expect("a1")[0];
        let a2 = element_rects_diting(html, "#a2", false, 800.0, 600.0, None).expect("a2")[0];
        let b1 = element_rects_diting(html, "#b1", false, 800.0, 600.0, None).expect("b1")[0];
        assert!((a1.width - 60.0).abs() <= 1.0, "col width outranks the first-row cell: {a1:?}");
        assert!((a2.width - 60.0).abs() <= 1.0, "col 1 pinned despite row-2 content: {a2:?}");
        assert!((b1.width - 240.0).abs() <= 2.0, "second col is auto: 240px leftover: {b1:?}");
    }

    /// The td `width` attribute — the height attribute's (blitz#507)
    /// sibling: a px hint below every author declaration, so on its own it
    /// pins a fixed-layout column, and a CSS width on the same element
    /// outranks it.
    #[test]
    fn td_width_attribute_hints_the_column() {
        let mk = |cell: &str| {
            format!(
                r##"<html><head><style>
            body {{ margin: 0; }}
            table {{ width: 300px; table-layout: fixed; border-collapse: collapse; }}
            td {{ padding: 0; height: 20px; }}
        </style></head><body>
            <table><tr><td {cell} id="a">a</td><td id="b">b</td></tr></table>
        </body></html>"##
            )
        };
        let width = |cell: &str, id: &str| {
            let html = mk(cell);
            element_rects_diting(&html, id, false, 800.0, 600.0, None).expect(id)[0].width
        };
        let attr = width(r#"width="80""#, "#a");
        assert!((attr - 80.0).abs() <= 1.0, "bare width attribute pins the column: {attr}");
        let css = width(r#"width="80" style="width:120px""#, "#a");
        assert!((css - 120.0).abs() <= 1.0, "CSS width outranks the attribute: {css}");
    }

    /// collapsed borders between adjacent cells halve: two 4px borders meet at
    /// one shared 4px line, not 8px of doubled paint.
    #[test]
    fn collapsed_borders_halve_where_adjacent_cells_meet() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { width: 100px; height: 40px; padding: 0; border: 4px solid black; }
        </style></head><body>
            <table><tr><td id="l">a</td><td id="r">b</td></tr></table>
        </body></html>"##;
        let l = element_rects_diting(html, "#l", false, 800.0, 600.0, None).expect("l")[0];
        let r = element_rects_diting(html, "#r", false, 800.0, 600.0, None).expect("r")[0];
        assert!(
            (r.x - (l.x + l.width)).abs() <= 1.0,
            "halved borders meet exactly — no gap, no overlap: {l:?} {r:?}"
        );
    }

    /// The `height` attribute on td is a px presentational hint (blitz#507):
    /// an empty `<td height="55">` is a 55px-tall box — the bare bones of
    /// HTML-email bar charts.
    #[test]
    fn td_height_attribute_sizes_the_cell() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { width: 100px; border-collapse: collapse; }
            td { padding: 0; font-size: 0; line-height: 0; }
        </style></head><body>
            <table><tr><td id="c" height="55">&nbsp;</td></tr></table>
        </body></html>"##;
        let c = element_rects_diting(html, "#c", false, 800.0, 600.0, None).expect("c")[0];
        assert!((c.height - 55.0).abs() <= 1.0, "attr height 55: {c:?}");
        assert!((c.width - 100.0).abs() <= 1.0, "fills the authored table width: {c:?}");
    }

    /// Same for tr: the attribute is the row height, cells stretch to it.
    #[test]
    fn tr_height_attribute_sets_the_row_height() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; }
        </style></head><body>
            <table><tr height="70"><td id="c">x</td></tr></table>
        </body></html>"##;
        let c = element_rects_diting(html, "#c", false, 800.0, 600.0, None).expect("c")[0];
        assert!((c.height - 70.0).abs() <= 1.0, "cell stretches to the tr attr height: {c:?}");
    }

    /// valign moves cell content within a taller cell (blitz#508); the
    /// attribute's default is Chrome's UA vertical-align: middle.
    #[test]
    fn valign_attr_positions_cell_content_vertically() {
        let mk = |valign: &str| {
            format!(
                r##"<html><head><style>
            body {{ margin: 0; }}
            table {{ border-collapse: collapse; }}
            td {{ padding: 0; height: 110px; }}
        </style></head><body>
            <table><tr><td {valign}><div id="d" style="width:50px;height:20px"></div></td></tr></table>
        </body></html>"##
            )
        };
        let top = element_rects_diting(&mk("valign=\"top\""), "div", false, 800.0, 600.0, None)
            .expect("top")[0];
        let middle = element_rects_diting(&mk(""), "div", false, 800.0, 600.0, None).expect("mid")[0];
        let bottom = element_rects_diting(&mk("valign=\"bottom\""), "div", false, 800.0, 600.0, None)
            .expect("bottom")[0];
        assert!(top.y <= 1.0, "valign=top pins content to the cell top: {top:?}");
        assert!(
            (middle.y - 45.0).abs() <= 1.5,
            "no attr = UA middle default: 110 cell, 20 content → y=45: {middle:?}"
        );
        assert!(
            (bottom.y - 90.0).abs() <= 1.5,
            "valign=bottom pins content to the cell bottom: {bottom:?}"
        );
    }

    /// The CSS property mirrors the attribute (blitz#508): top/bottom move
    /// the content within the taller cell; baseline behaves like top for
    /// the flex-column cell model.
    #[test]
    fn css_vertical_align_positions_cell_content() {
        let mk = |decl: &str| {
            format!(
                r##"<html><head><style>
            body {{ margin: 0; }}
            table {{ border-collapse: collapse; }}
            td {{ padding: 0; height: 110px; {decl} }}
        </style></head><body>
            <table><tr><td><div id="d" style="width:50px;height:20px"></div></td></tr></table>
        </body></html>"##
            )
        };
        for (decl, want, label) in [
            ("vertical-align: top", 0.0, "top"),
            ("vertical-align: baseline", 0.0, "baseline folds to top"),
            ("vertical-align: bottom", 90.0, "bottom"),
        ] {
            let r = element_rects_diting(&mk(decl), "#d", false, 800.0, 600.0, None)
                .unwrap_or_else(|e| panic!("{label}: {e}"))[0];
            assert!((r.y - want).abs() <= 1.5, "{label} → y≈{want}, got {r:?}");
        }
    }

    /// Presentational hints sit below every author declaration: a CSS
    /// vertical-align outranks a competing valign attribute on the cell.
    #[test]
    fn css_vertical_align_outranks_valign_attribute() {
        let html = r##"<html><head><style>
            body { margin: 0; }
            table { border-collapse: collapse; }
            td { padding: 0; height: 110px; vertical-align: top; }
        </style></head><body>
            <table><tr><td valign="bottom"><div id="d" style="width:50px;height:20px"></div></td></tr></table>
        </body></html>"##;
        let d = element_rects_diting(html, "#d", false, 800.0, 600.0, None).expect("d")[0];
        assert!(d.y <= 1.0, "CSS top beats valign=bottom: {d:?}");
    }

    /// Stylesheets come first: img alternates must not burn the fetch cap and
    /// starve the sheet (a dropped head stylesheet blanks layout, images are
    /// only fidelity polish).
    #[test]
    fn stylesheets_are_collected_before_image_urls() {
        let mut html = String::from(r##"<link rel="stylesheet" href="/style.css">"##);
        for i in 0..40 {
            html.push_str(&format!(r##"<img src="/i{i}.jpg">"##));
        }
        let base = url::Url::parse("https://x.test/").unwrap();
        let urls = collect_resource_urls(&html, &base, 1280.0);
        assert_eq!(urls.len(), MAX_RESOURCES, "capped at {MAX_RESOURCES}");
        assert_eq!(urls[0].as_str(), "https://x.test/style.css");
    }
}

#[cfg(test)]
mod opacity_pipeline_tests {
    use super::*;

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

    /// CSS `opacity` must hide TEXT, not just backgrounds: the layout walk
    /// folds element opacity into the text color's alpha and `colorize`
    /// multiplies it into glyph coverage. This locks the whole static-paint
    /// chain (a page whose hidden-by-opacity captions painted at full ink
    /// made every "hidden until animated" scene visible from frame 0).
    /// Rotated inline background (affine residuals batch ②): a span with a
    /// background inside a rotate(90°) div must paint its band — as a
    /// VERTICAL stripe (the rotation applied to the band rect), not the
    /// horizontal unrotated band, and not nothing (the pre-fix skip).
    #[test]
    fn rotated_inline_background_paints_its_band() {
        let html = r#"<html><head><style>
            body { margin: 0; }
            #rot { transform: rotate(90deg); width: 200px; height: 40px; }
            #hl { background: #ffcc00; font-size: 20px; color: #000; }
        </style></head><body>
            <div id="rot"><span id="hl">MMMMMMMMMM</span></div>
        </body></html>"#;
        let shot = render_html_to_png_diting(html, "http://probe.local/", 220, 180, 1.0, false, None, false, None)
            .expect("render");
        // rotate(90°) about the div center (100,20) is x' = 120 − y,
        // y' = x − 80: the first line's horizontal band (x ∈ [0,165),
        // y ∈ [0,24)) maps to a vertical stripe at columns ≈ [96, 121) and
        // rows up to ≈ 85 — every other column holding yellow means the
        // band painted unrotated; zero yellow means the old skip.
        let is_yellow = |(r, g, b): (u8, u8, u8)| r > 230 && g > 180 && b < 80;
        let decoder = png::Decoder::new(std::io::Cursor::new(&shot.png));
        let mut reader = decoder.read_info().expect("png read_info");
        let mut buf = vec![0; reader.output_buffer_size().expect("png buffer size")];
        let info = reader.next_frame(&mut buf).expect("png decode");
        let px = &buf[..info.buffer_size()];
        let mut yellow_cols: Vec<u32> = Vec::new();
        let mut max_row = 0u32;
        for y in 0..info.height {
            for x in 0..info.width {
                let i = ((y * info.width + x) * 4) as usize;
                if is_yellow((px[i], px[i + 1], px[i + 2])) {
                    yellow_cols.push(x);
                    max_row = max_row.max(y);
                }
            }
        }
        assert!(yellow_cols.len() > 500, "band paints: {} yellow px", yellow_cols.len());
        assert!(
            yellow_cols.iter().all(|&x| (90..122).contains(&x)),
            "stripe is vertical (rotation applied to the band): cols {:?}..{:?}",
            yellow_cols.iter().min(),
            yellow_cols.iter().max()
        );
        assert!(max_row > 60, "band extends below the original line box: max row {max_row}");
    }

    #[test]
    fn css_opacity_zero_hides_text_in_static_screenshot() {
        // Force the body box to the full canvas: diting paints body bg over
        // its own box only (no propagation to the root canvas), so a
        // content-height body would leave white bands that read as "bright
        // ink" and the probe would prove nothing (hit three times now).
        let html = r#"<html><head><style>
            html, body { margin: 0; width: 300px; height: 200px; background: #000000; }
            .h { color: #FFFFFF; font-size: 28px; opacity: 0; }
            .half { color: #FFFFFF; font-size: 28px; opacity: 0.5; }
        </style></head><body><div class="h">HIDDEN</div><div class="half">HALF</div></body></html>"#;
        let shot = render_html_to_png_diting(html, "http://probe.local/", 300, 200, 1.0, false, None, false, None)
            .expect("render");
        // Nothing fully bright: the opacity-0 line leaves zero ink and the
        // half-opacity line is mid-gray at worst.
        assert_eq!(count_color(&shot.png, |(r, g, b)| r as u32 + g as u32 + b as u32 > 600), 0, "opacity:0 text must not paint");
        // The half-opacity line DOES land — blended, not full-white.
        let gray = count_color(&shot.png, |(r, g, b)| r > 80 && g > 80 && b > 80 && r as u32 + g as u32 + b as u32 <= 600);
        assert!(gray > 50, "opacity:0.5 text should paint blended gray, got {gray} px");
    }
}
