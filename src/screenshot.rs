//! Screenshot rendering: feed a JS-rendered HTML string to Blitz (Stylo +
//! Taffy layout, vello_cpu paint) and return a PNG.
//!
//! This module is the "paint what the agent already sees" layer - aginxbrowser's
//! V8 path has already run the page's JS and produced the final DOM; we hand
//! that DOM to Blitz purely for layout + rasterization. Blitz itself does no
//! networking: the caller pre-fetches the visible sub-resources (images, head
//! stylesheets) over HTTP and hands them to [`PrefetchedNetProvider`], which
//! serves them synchronously. Without a pre-fetch, the no-op DummyNetProvider
//! applies and upstream #636's `is_noop()` gating stops `<head>` stylesheets
//! from blocking paint forever (see the `screenshot` feature note in
//! Cargo.toml).
//!
//! Element regions: after layout, every node carries a Taffy `final_layout`
//! whose `location` is parent-relative. The absolute (page-relative) origin is
//! the sum of locations up the `layout_parent` chain (which includes anonymous
//! block boxes, matching how `blitz-paint` positions nodes). `paint_scene`'s
//! x/y offsets are in scaled physical pixels, so a CSS-pixel rect is passed in
//! as `rect * scale`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocumentConfig, Point, util::Color};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request as NetRequest};
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

// ---------------------------------------------------------------------------
// Pre-fetched sub-resources
// ---------------------------------------------------------------------------

/// Bodies of sub-resources (images, stylesheets) fetched over HTTP before
/// rendering, keyed by absolute URL string. The key is the normalized
/// `Url::as_str()` form — the same string Blitz produces by resolving
/// `img src` / `link href` against `base_url` before calling the provider.
pub type PrefetchedResources = HashMap<String, Arc<Vec<u8>>>;

/// A [`NetProvider`] that answers synchronously from a pre-fetched map.
///
/// Requests are never left unanswered: a miss (unseen URL, too-big body,
/// fetch failure) gets empty bytes. An unanswered `<head>` stylesheet would
/// sit in `pending_critical_resources` forever and blank the paint; an
/// answered-empty one parses to an empty sheet and clears the pending set —
/// blitz's `load_resource` removes the request id before inspecting the
/// result, so even decode failures clear it.
struct PrefetchedNetProvider {
    resources: PrefetchedResources,
}

impl NetProvider for PrefetchedNetProvider {
    fn fetch(&self, _doc_id: usize, request: NetRequest, handler: Box<dyn NetHandler>) {
        let url = request.url.as_str().to_string();
        let bytes = match self.resources.get(&url) {
            Some(body) => Bytes::from(body.as_ref().clone()),
            None => Bytes::new(),
        };
        handler.bytes(url, bytes);
    }
    // is_noop() keeps its false default: we DO deliver (possibly empty)
    // resources, so head stylesheets must go through resolve-and-clear
    // rather than being skipped by the #636 gating.
}

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
            if matches!(u.scheme(), "http" | "https") && !out.contains(&u) {
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
    // when enabled, plain reqwest otherwise.
    async fn fetch_via(
        page: &crate::page::Page,
        u: &url::Url,
    ) -> Option<crate::diting_net::Response> {
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = page.inner.stealth_client {
            return stealth.fetch(u).await.ok();
        }
        page.inner.http_client.fetch(u).await.ok()
    }

    let Ok(base) = url::Url::parse(base_url) else {
        return PrefetchedResources::new();
    };
    let urls = collect_resource_urls(html, &base, viewport_width);
    if urls.is_empty() {
        return PrefetchedResources::new();
    }
    let requested = urls.len();

    let futs = urls.into_iter().map(|u| async move {
        let resp = tokio::time::timeout(PER_REQUEST_TIMEOUT, fetch_via(page, &u))
            .await
            .ok()
            .flatten()?;
        if resp.status != 200 || resp.body.is_empty() || resp.body.len() > MAX_BODY_BYTES {
            return None;
        }
        Some((u.as_str().to_string(), Arc::new(resp.body)))
    });
    let map: PrefetchedResources = futures::future::join_all(futs).await.into_iter().flatten().collect();
    tracing::debug!(
        "screenshot prefetch: {}/{} sub-resources fetched",
        map.len(),
        requested
    );
    map
}

/// Render an HTML string to a PNG (RGBA bytes) plus element rects.
///
/// `base_url` resolves relative URLs in the document (`<base>`, sub-resource
/// hrefs when `resources` is given). When `full_page` is true, the render
/// height tracks the document's computed content height (capped at 16000px to
/// bound memory); otherwise the requested `height` is used (viewport-sized).
///
/// `resources`: pre-fetched sub-resource bodies (see
/// [`prefetch_render_resources`]). `None` or empty renders resource-less via
/// the no-op DummyNetProvider (#636 gating); a non-empty map activates
/// [`PrefetchedNetProvider`] so images and head stylesheets actually paint.
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
    resources: Option<&PrefetchedResources>,
) -> Result<RenderedScreenshot> {
    if html.is_empty() {
        anyhow::bail!("render_html_to_png: empty HTML (page content() returned nothing - navigation may have failed)");
    }
    let scale_f = scale.max(0.1) as f64;

    // Sub-resource delivery: a non-empty pre-fetch map activates the sync
    // provider (misses answer empty, so nothing pends forever). Without one,
    // DummyNetProvider (no-op) + upstream #636's is_noop() gating keeps head
    // stylesheets from blocking paint.
    let has_resources = resources.is_some_and(|m| !m.is_empty());
    let net_provider: Option<Arc<dyn NetProvider>> = if has_resources {
        Some(Arc::new(PrefetchedNetProvider {
            resources: resources.unwrap().clone(),
        }))
    } else {
        None
    };

    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some(base_url.to_string()),
            net_provider,
            // Bundled CJK fonts (batch 3c): hoisted to the head of the Han
            // fallback chain, system fonts kept as tail — CJK renders the
            // same on every machine, no fonts-noto-cjk dependency.
            font_ctx: Some(crate::diting_fonts::font_ctx()),
            viewport: Some(Viewport::new(
                width * (scale as u32),
                height * (scale as u32),
                scale,
                ColorScheme::Light,
            )),
            ..Default::default()
        },
    );

    // Drive Stylo style resolution + Taffy layout to completion. A delivered
    // stylesheet is applied on the NEXT round (resolve drains the resource
    // event queue first), so the provider path gets extra rounds.
    let rounds = if has_resources { 8 } else { 4 };
    for _ in 0..rounds {
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

// ---------------------------------------------------------------------------
// diting engine render: our own stack paints the PNG (no Stylo/vello/parley)
// ---------------------------------------------------------------------------

/// Full-page screenshot on the diting stack: diting_css cascade →
/// diting_layout (taffy + swash) → diting paint Canvas → PNG.
///
/// The engine-claim endpoint of the render line: unlike
/// [`render_html_to_png`] there is no Stylo, no vello_cpu and no parley in
/// the path, so the parley 0.11 CJK hang (linebender/parley#752, which pins
/// our blitz dependency at 2fa6434d) cannot bite here. Coverage is narrower
/// than Blitz — tables, shadows, transforms and mixed-run line layout are
/// still being claimed (docs/engine/render.md); `/screenshot` picks the
/// engine per request via its `engine` field, so the gap is measured on
/// real traffic rather than guessed.
///
/// `scale` renders 1:1 CSS px (retina resample lands with the canvas
/// upscale batch). `full_page` tracks content height from the laid-out
/// rects, capped at 16000 like the Blitz path; a `selector` crop is an
/// RGBA window copy out of the full-page canvas (no PNG round-trip).
#[allow(clippy::too_many_arguments)]
pub fn render_html_to_png_diting(
    html: &str,
    base_url: &str, // stylesheet hrefs resolve against it; img-URL normalization is still 挂账 in ImageCache
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

    // Image bytes: everything non-stylesheet the prefetch pass fetched
    // (already keyed by absolute URL, which is what ImageCache looks up).
    let network_bytes: HashMap<String, Vec<u8>> = resources
        .map(|res| {
            res.iter()
                .filter(|(k, v)| (!css_urls.contains(k.as_str()) && !k.ends_with(".css")) && !v.is_empty())
                .map(|(k, v)| (k.clone(), v.as_ref().clone()))
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

/// The absolute (page-relative) border-box rect of a laid-out element.
///
/// Taffy only sizes block-level boxes; pure inline elements (`<a>text</a>`)
/// report 0x0 because their content belongs to the containing block's inline
/// layout. As a fallback, union the element's element-descendant boxes (which
/// covers mixed content like `<a><img></a>`); if that is still empty the
/// caller gets the honest 0x0.
pub(crate) fn element_rect(doc: &BaseDocument, node_id: NodeId) -> ElementRect {
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

// ---------------------------------------------------------------------------
// diting engine rects: our own layout pipeline answering selector_rects
// ---------------------------------------------------------------------------

/// Element rects computed by the DITING pipeline (diting_dom + diting_css +
/// diting_layout), independent of Blitz/Stylo. The page HTML carries its CSS
/// in `<style>` blocks and `style` attributes; external `<link>` sheets are
/// not consulted here — the caller can pass their bodies via `extra_css`
/// (joined after the inline blocks, so link rules win ties).
///
/// Returns one [`ElementRect`] per match of `selector` (document order),
/// or just the first when `selector_all` is false. An invalid selector or a
/// parse failure is an Err; an empty match list is Ok(vec![]).
///
/// This is the Phase-2 claim of the element-coordinate API on our own stack:
/// same wire shape as Blitz's `selector_rects`, keyed by the same CSS-pixel
/// page-relative contract, so callers can compare engines or migrate.
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

    /// The diting element-coordinate entry answers the same wire shape as
    /// Blitz's selector_rects: a `<style>`-driven absolute box lands at its
    /// authored rect, `selector_all=false` returns just the first match, and
    /// an invalid selector is an Err. Extra CSS (external sheet bodies)
    /// participates in the cascade.
    #[test]
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

    /// Deterministic layout check: an absolutely-positioned element must report
    /// its CSS rect, and a selector crop must contain (only) that element.
    #[test]
    fn selector_rect_and_crop() {
        let html = r##"<html><body style="margin:0">
            <div id="target" style="position:absolute;left:100px;top:150px;width:60px;height:40px;background:#ff0000"></div>
        </body></html>"##;

        // 1. Rect math: uncropped render (selector_all), selector reports the element rect.
        let full = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#target"), true, None)
            .expect("full render");
        assert_eq!(full.rects.len(), 1);
        let r = full.rects[0];
        assert!((r.x - 100.0).abs() <= 1.0, "x: {r:?}");
        assert!((r.y - 150.0).abs() <= 1.0, "y: {r:?}");
        assert!((r.width - 60.0).abs() <= 1.0, "width: {r:?}");
        assert!((r.height - 40.0).abs() <= 1.0, "height: {r:?}");

        // 2. Crop: rendered image is exactly the element, mostly red.
        let crop = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#target"), false, None)
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
        let all = render_html_to_png(html2, "https://example.com/", 200, 200, 1.0, false, Some(".it"), true, None)
            .expect("all render");
        assert_eq!(all.rects.len(), 2);
        assert!((all.rects[0].x - 10.0).abs() <= 1.0);
        assert!((all.rects[1].x - 50.0).abs() <= 1.0);
        assert_eq!((all.pixel_width, all.pixel_height), (200, 200));

        // 4. No match is an error for crop mode, empty for all-mode.
        assert!(render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#nope"), false, None).is_err());
        let none = render_html_to_png(html, "https://example.com/", 800, 600, 1.0, false, Some("#nope"), true, None)
            .expect("all-mode returns empty");
        assert!(none.rects.is_empty());

        // 5. Inline element with box children: rect falls back to the union of
        //    descendant boxes (the <a> itself carries no Taffy box).
        let html3 = r##"<html><body style="margin:0">
            <a id="link" href="#"><div style="position:absolute;left:200px;top:250px;width:50px;height:70px;background:#ff0000"></div></a>
        </body></html>"##;
        let inline = render_html_to_png(html3, "https://example.com/", 800, 600, 1.0, false, Some("#link"), true, None)
            .expect("inline render");
        assert_eq!(inline.rects.len(), 1);
        assert!((inline.rects[0].x - 200.0).abs() <= 1.0, "{:?}", inline.rects[0]);
        assert!((inline.rects[0].width - 50.0).abs() <= 1.0, "{:?}", inline.rects[0]);
        assert!((inline.rects[0].height - 70.0).abs() <= 1.0, "{:?}", inline.rects[0]);

        // 6. Pure-inline (text-only) element crops are rejected with a clear error.
        let html4 = r##"<html><body style="margin:0"><p><a id="textonly" href="#">just text</a></p></body></html>"##;
        assert!(render_html_to_png(html4, "https://example.com/", 800, 600, 1.0, false, Some("#textonly"), false, None)
            .is_err());
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

    /// PrefetchedNetProvider: served stylesheets and images actually paint;
    /// a miss answers empty bytes and must NOT trip the pending-critical
    /// guard (an unanswered head stylesheet would blank the paint).
    #[test]
    fn prefetched_resources_paint() {
        // 1. Stylesheet hit: body background turns green via the served CSS.
        let css_html = r##"<html><head><link rel="stylesheet" href="/s.css"></head><body style="margin:0"><p>hi</p></body></html>"##;
        let mut hit = PrefetchedResources::new();
        hit.insert(
            "https://example.com/s.css".to_string(),
            Arc::new(b"body { background: #00ff00 }".to_vec()),
        );
        let served =
            render_html_to_png(css_html, "https://example.com/", 200, 150, 1.0, false, None, false, Some(&hit))
                .expect("served render");
        let green = count_color(&served.png, |(r, g, b)| g > 200 && r < 80 && b < 80);
        assert!(
            green > 200 * 150 * 9 / 10,
            "expected green page, got {green}/{} green pixels",
            200 * 150
        );

        // 2. Miss (unrelated key): the empty-bytes answer must clear the head
        //    stylesheet's pending-critical entry instead of bailing on render.
        let img_html = r##"<html><head><link rel="stylesheet" href="/s.css"></head><body style="margin:0">
            <img src="https://cdn.example.com/dot.png" style="position:absolute;left:0;top:0;width:80px;height:60px">
        </body></html>"##;
        let mut miss = PrefetchedResources::new();
        miss.insert(
            "https://example.com/other.css".to_string(),
            Arc::new(b"body{}".to_vec()),
        );
        let blank =
            render_html_to_png(img_html, "https://example.com/", 200, 150, 1.0, false, None, false, Some(&miss))
                .expect("miss render must not trip the pending-critical guard");
        let green = count_color(&blank.png, |(r, g, b)| g > 200 && r < 80 && b < 80);
        assert_eq!(green, 0, "unserved stylesheet must not apply");

        // 3. Image hit: a solid-red 8x8 PNG served at the img's absolute URL
        //    paints the 80x60 element red.
        let mut img_hit = PrefetchedResources::new();
        img_hit.insert(
            "https://cdn.example.com/dot.png".to_string(),
            Arc::new(solid_png(8, 8, 255, 0, 0)),
        );
        let painted =
            render_html_to_png(img_html, "https://example.com/", 200, 150, 1.0, false, None, false, Some(&img_hit))
                .expect("image render");
        let red = count_color(&painted.png, |(r, g, b)| r > 200 && g < 80 && b < 80);
        assert!(
            red > 80 * 60 * 8 / 10,
            "expected mostly red img, got {red}/{} red pixels",
            80 * 60
        );
    }

    /// Encode a solid-color RGBA PNG (w x h).
    fn solid_png(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        use std::io::Cursor;
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(Cursor::new(&mut out), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let row: Vec<u8> = std::iter::repeat([r, g, b, 255]).take(w as usize).flatten().collect();
        let data: Vec<u8> = std::iter::repeat(row).take(h as usize).flatten().collect();
        writer.write_image_data(&data).expect("png data");
        writer.finish().expect("png finish");
        out
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

    // ------------------------------------------------------------------
    // Baseline atlas (render-claim batch 0): deterministic local pages whose
    // Blitz-pipeline outputs are locked here as the parity yardstick a future
    // in-house renderer must match page-for-page. Metrics per page: distinct
    // color count, key-region pixel counts, and layout rects. All fixtures
    // are network-free so the atlas runs in CI unchanged.
    //
    // When the renderer under test changes, these are THE numbers to compare:
    // a page whose color count or region counts drift has a layout/paint
    // regression (or improvement) that must be explained before switching.
    // ------------------------------------------------------------------

    /// Distinct RGBA colors in a rendered PNG.
    fn distinct_colors(png_bytes: &[u8]) -> usize {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("png read_info");
        let mut buf = vec![0; reader.output_buffer_size().expect("png output buffer size")];
        let info = reader.next_frame(&mut buf).expect("png decode");
        use std::collections::HashSet;
        buf[..info.buffer_size()]
            .chunks(4)
            .map(|px| [px[0], px[1], px[2], px[3]])
            .collect::<HashSet<_>>()
            .len()
    }

    #[test]
    fn baseline_ssr_text_and_chinese() {
        // Pure SSR page with CJK text — the case curl-fed Blitz failed at
        // first (all-white) and the case any successor must keep green.
        // System fonts provide the CJK glyphs on dev hosts; CI needs
        // fonts-noto-cjk like the server.
        let html = r#"<html><head><style>
            body { margin: 0; font-family: sans-serif; background: #ffffff; }
            h1 { color: #1a0dab; font-size: 28px; margin: 16px; }
            p.item { color: #333333; font-size: 14px; margin: 16px; }
        </style></head><body>
            <h1>搜索引擎优化测试</h1>
            <p class="item">第一段中文内容，用于验证 CJK 字形光栅与断行。</p>
            <p class="item">Second paragraph mixing English with 中文内容.</p>
        </body></html>"#;

        let shot = render_html_to_png(html, "https://example.com/", 800, 400, 1.0, false, None, false, None)
            .expect("SSR render");
        assert_eq!((shot.pixel_width, shot.pixel_height), (800, 400));

        let colors = distinct_colors(&shot.png);
        assert!(colors > 50, "text+antialiasing should yield many colors, got {colors}");
        // Blue heading glyphs present (#1a0dab ± antialiasing).
        let blue = count_color(&shot.png, |(r, g, b)| b > 120 && r < 90 && g < 80);
        assert!(blue > 100, "CJK heading glyphs missing: {blue} blue px");
        // Dark body text present.
        let dark = count_color(&shot.png, |(r, g, b)| r < 90 && g < 90 && b < 90);
        assert!(dark > 200, "body text missing: {dark} dark px");

        // Layout truth: h1 sits near the top of the document.
        let h1 = render_html_to_png(html, "https://example.com/", 800, 400, 1.0, false, Some("h1"), true, None)
            .expect("h1 rect");
        assert_eq!(h1.rects.len(), 1);
        assert!(h1.rects[0].y < 100.0, "h1 near top: {:?}", h1.rects[0]);
    }

    #[test]
    fn baseline_flex_grid_layout() {
        // Flexbox row + CSS grid: the two workhorse layouts of real sites.
        let html = r#"<html><head><style>
            body { margin: 0; }
            .row { display: flex; gap: 10px; padding: 10px; }
            .cell { width: 100px; height: 50px; }
            .grid { display: grid; grid-template-columns: repeat(3, 60px); gap: 8px; padding: 10px; }
            .g { height: 40px; background: #0066cc; }
        </style></head><body>
            <div class="row">
                <div class="cell" style="background:#ff0000"></div>
                <div class="cell" style="background:#00aa00"></div>
                <div class="cell" style="background:#0000ff"></div>
            </div>
            <div class="grid">
                <div class="g"></div><div class="g"></div><div class="g"></div>
                <div class="g"></div><div class="g"></div><div class="g"></div>
            </div>
        </body></html>"#;

        let shot = render_html_to_png(html, "https://example.com/", 600, 300, 1.0, false, None, false, None)
            .expect("flex/grid render");
        let red = count_color(&shot.png, |(r, g, b)| r > 200 && g < 80 && b < 80);
        let green = count_color(&shot.png, |(r, g, b)| g > 140 && r < 70 && b < 70);
        let blue = count_color(&shot.png, |(r, g, b)| b > 180 && r < 70 && g < 130);
        // Each flex cell is 100x50 = 5000 px; allow rounding slack.
        assert!(red > 4000, "flex red cell: {red}");
        assert!(green > 4000, "flex green cell: {green}");
        assert!(blue > 4000, "flex blue cell: {blue}");

        // Grid: two rows of three 60x40 cells with 8px gaps — verify geometry
        // via rects (gap math is exact) rather than pixel counts.
        let all = render_html_to_png(html, "https://example.com/", 600, 300, 1.0, false, Some(".g"), true, None)
            .expect("grid rects");
        assert_eq!(all.rects.len(), 6, "grid must place all six cells");
        let row1_max_y = all.rects.iter().take(3).map(|r| r.y).fold(0.0f64, f64::max);
        let row2_min_y = all.rects.iter().skip(3).map(|r| r.y).fold(f64::MAX, f64::min);
        assert!(
            row2_min_y >= row1_max_y + 40.0,
            "grid rows must not overlap: row1 maxY {row1_max_y}, row2 minY {row2_min_y}"
        );
        for r in &all.rects {
            assert!((r.width - 60.0).abs() <= 1.5, "grid cell width: {r:?}");
            assert!((r.height - 40.0).abs() <= 1.5, "grid cell height: {r:?}");
        }
    }

    #[test]
    fn baseline_table_and_display_none() {
        // Authored table + display:none hiding: the exact pair that made
        // curl-fed renders blank back in the Phase 0 spike.
        let html = r#"<html><head><style>
            table { border-collapse: collapse; }
            td { border: 1px solid #999999; padding: 4px 12px; color: #222222; }
            .hidden { display: none; }
        </style></head><body>
            <table><tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr></table>
            <div class="hidden">must not paint</div>
        </body></html>"#;

        let shot = render_html_to_png(html, "https://example.com/", 400, 300, 1.0, false, None, false, None)
            .expect("table render");
        // Gray borders from collapsed td edges.
        let gray = count_color(&shot.png, |(r, g, b)| {
            (r as i32 - g as i32).abs() < 12 && (g as i32 - b as i32).abs() < 12 && r > 110 && r < 190
        });
        assert!(gray > 150, "table borders missing: {gray} gray px");
        // Text ink present.
        let ink = count_color(&shot.png, |(r, g, b)| r < 80 && g < 80 && b < 80);
        assert!(ink > 30, "table text missing: {ink}");

        // display:none content paints nothing. selector_all still reports a
        // match entry (blitz keeps the node), but its box is 0x0 — lock that
        // honest behavior as the baseline.
        let hidden = render_html_to_png(html, "https://example.com/", 400, 300, 1.0, false, Some(".hidden"), true, None)
            .expect("hidden query");
        for r in &hidden.rects {
            assert!(
                r.width < 0.5 && r.height < 0.5,
                "display:none must have no visible box: {r:?}"
            );
        }

        let colors = distinct_colors(&shot.png);
        assert!(colors > 30, "border+text+antialias palette: {colors}");
    }

    #[test]
    fn baseline_full_page_tracks_content_height() {
        // full_page must grow the canvas to the content, not clip at viewport.
        let html = r#"<html><head><style>body{margin:0}</style></head><body>
            <div style="height:1800px;background:#123456"></div>
        </body></html>"#;
        let shot = render_html_to_png(html, "https://example.com/", 400, 300, 1.0, true, None, false, None)
            .expect("full-page render");
        assert!(
            shot.pixel_height >= 1800,
            "full_page must track content height, got {}",
            shot.pixel_height
        );

        // Viewport-only render clips at 300px.
        let vp = render_html_to_png(html, "https://example.com/", 400, 300, 1.0, false, None, false, None)
            .expect("viewport render");
        assert_eq!(vp.pixel_height, 300);
    }
}

// ---------------------------------------------------------------------------
// Dual-engine computed-style cross-check (render-claim follow-up): run the
// same deterministic pages through diting_css's cascade AND Blitz's Stylo
// cascade, and compare the properties diting_css models. Divergences here are
// exactly the semantic gaps a future renderer switch must close.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod cross_check {
    use super::*;
    use crate::diting_css::{self, ComputedStyle, Display, TextAlign};
    use stylo_alias::values::computed::LengthPercentage;
    use stylo_alias::values::specified::box_::{DisplayInside, DisplayOutside};
    use stylo_alias::values::specified::text::TextAlignKeyword;

    /// Read the Stylo-computed values we care about for one node of a resolved
    /// blitz document, projected into diting_css's property subset.
    fn stylo_view(doc: &BaseDocument, node_id: NodeId) -> Option<ComputedStyle> {
        let node = doc.get_node(node_id)?;
        let data = node.stylo_element_data_opt()?.get()?;
        let style = data.styles.get_primary()?;

        let mut cs = ComputedStyle::default();

        // Display projection: outside=Inline + inside=Flow is CSS inline;
        // everything else collapses to Block in our subset.
        let display = style.get_box().display;
        cs.display = if display.is_none() {
            Some(Display::None)
        } else {
            match (display.outside(), display.inside()) {
                (DisplayOutside::Inline, DisplayInside::Flow) => Some(Display::Inline),
                (DisplayOutside::Inline, DisplayInside::FlowRoot) => Some(Display::Inline),
                (_, DisplayInside::Flex) => Some(Display::Flex),
                (_, DisplayInside::Grid) => Some(Display::Grid),
                _ => Some(Display::Block),
            }
        };

        // Colors: computed colors are already absolute; components are sRGB
        // 0..1 f32 (+ alpha) for the named/hex colors our fixtures use.
        fn rgba(c: &stylo_alias::color::AbsoluteColor) -> diting_css::Color {
            diting_css::Color(
                (c.components.0 * 255.0) as u8,
                (c.components.1 * 255.0) as u8,
                (c.components.2 * 255.0) as u8,
                (c.alpha * 255.0) as u8,
            )
        }
        cs.color = Some(rgba(&style.get_inherited_text().color));
        cs.background_color = Some(rgba(
            style.get_background().background_color.as_absolute()?,
        ));

        let margin = style.get_margin();
        cs.margin.top = margin_enum_len(&margin.margin_top);
        cs.margin.right = margin_enum_len(&margin.margin_right);
        cs.margin.bottom = margin_enum_len(&margin.margin_bottom);
        cs.margin.left = margin_enum_len(&margin.margin_left);

        let padding = style.get_padding();
        cs.padding.top = nonnegative_len(&padding.padding_top);
        cs.padding.right = nonnegative_len(&padding.padding_right);
        cs.padding.bottom = nonnegative_len(&padding.padding_bottom);
        cs.padding.left = nonnegative_len(&padding.padding_left);

        cs.font_size = Some(style.get_font().font_size.used_size.0.px());
        cs.font_weight = Some(style.get_font().font_weight.value() as u16);

        cs.text_align = match style.clone_text_align() {
            TextAlignKeyword::Start | TextAlignKeyword::Left | TextAlignKeyword::MozLeft => {
                Some(TextAlign::Left)
            }
            TextAlignKeyword::Center | TextAlignKeyword::MozCenter => Some(TextAlign::Center),
            TextAlignKeyword::End | TextAlignKeyword::Right | TextAlignKeyword::MozRight => {
                Some(TextAlign::Right)
            }
            _ => None,
        };
        Some(cs)
    }

    /// Map a computed LengthPercentage into our subset: px lengths come
    /// through as `Length::Px`, percentages as `Length::Percent` (stylo keeps
    /// them symbolic until used-value time, like us). calc() has no single
    /// shape, so it reads as None.
    fn px_len(lp: &LengthPercentage) -> Option<diting_css::Length> {
        match lp.unpack() {
            stylo_alias::values::computed::length_percentage::Unpacked::Length(l) => {
                Some(diting_css::Length::Px(l.px() as f32))
            }
            stylo_alias::values::computed::length_percentage::Unpacked::Percentage(p) => {
                Some(diting_css::Length::Percent(p.0 * 100.0))
            }
            _ => None,
        }
    }

    /// Margin/padding longhands are `GenericMargin<LengthPercentage>` enums
    /// (LengthPercentage / Auto / anchor variants). Only the plain length
    /// case maps into our subset.
    fn margin_enum_len(m: &stylo_alias::values::computed::Margin) -> Option<diting_css::Length> {
        use stylo_alias::values::generics::length::GenericMargin;
        match m {
            GenericMargin::LengthPercentage(lp) => px_len(lp),
            _ => None,
        }
    }

    /// Padding longhands are `NonNegative<LengthPercentage>` (no auto).
    fn nonnegative_len(
        p: &stylo_alias::values::generics::NonNegative<LengthPercentage>,
    ) -> Option<diting_css::Length> {
        px_len(&p.0)
    }

    /// Build both engines' view of one page: parse+resolve with blitz, then
    /// compute with diting_css against our own DOM tree.
    fn dual_compute(
        html: &str,
        stylesheet: &str,
    ) -> (
        BaseDocument,
        crate::diting_dom::tree::DomTree,
        Vec<diting_css::ParsedRule>,
    ) {
        // Blitz side.
        let css_doc = format!("<style>{stylesheet}</style>{html}");
        let mut doc = HtmlDocument::from_html(
            &css_doc,
            DocumentConfig {
                base_url: Some("https://example.com/".to_string()),
                net_provider: None,
                viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            },
        );
        for _ in 0..4 {
            doc.resolve(0.0);
        }

        // diting side: same HTML + sheet through our engine.
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(stylesheet);

        (doc.into_inner(), tree, rules)
    }

    #[test]
    fn dual_engine_display_and_colors_agree() {
        let html = r#"<body><h1 id="t">标题</h1><p class="lead">x</p><span class="inl">y</span></body>"#;
        let sheet = r#"
            h1 { color: #1a0dab; background-color: #ff0000; font-weight: bold; font-size: 28px; margin-top: 16px; }
            .lead { color: red; }
            span.inl { display: inline-block; }
        "#;
        let (doc, tree, rules) = dual_compute(html, sheet);

        let cases: &[( &str, NodeId)] = &[
            ("h1", doc.query_selector("#t").unwrap().expect("h1 in blitz doc")),
            ("p", doc.query_selector(".lead").unwrap().expect(".lead in blitz doc")),
        ];
        for (tag, blitz_nid) in cases {
            let ours_selector = if *tag == "h1" { "h1" } else { ".lead" };
            let our_nid = tree.query_selector(ours_selector).unwrap().unwrap();

            let parent = tree.with_node(our_nid, |n| n.parent).flatten();
            let parent_style = parent.and_then(|pid| {
                let tag_name = tree.with_node(pid, |n| {
                    n.as_element().map(|e| e.local.to_string())
                }).flatten()?;
                Some(cascade_simple(&tree, pid, &tag_name, &rules))
            });
            let inline = tree.with_node(our_nid, |n| n.get_attribute("style").map(|s| s.to_string())).flatten();
            let our_view = diting_css_cascade_full(&tree, our_nid, *tag, &rules, parent_style.as_ref(), inline.as_deref());

            let theirs = stylo_view(&doc, *blitz_nid).expect("stylo primary style");

            assert_eq!(our_view.color, theirs.color, "{tag} color: ours={:?} theirs={:?}", our_view.color, theirs.color);
            // KNOWN MODELING DIVERGENCE (recorded in render.md §10): Stylo
            // always materializes an initial value (transparent) for every
            // property; diting_css uses None to mean "declared nowhere".
            // Compare the set-or-transparent flag instead of raw values.
            let ours_bg_set = our_view.background_color.map(|c| c != diting_css::Color(0, 0, 0, 0)).unwrap_or(false);
            let theirs_bg = theirs.background_color.expect("stylo always has a bg");
            let theirs_bg_set = theirs_bg != diting_css::Color(0, 0, 0, 0);
            assert_eq!(ours_bg_set, theirs_bg_set, "{tag} background set-flag");
            // Same divergence for font-weight: Stylo's initial value is 400;
            // our None means "no author/UA declaration reached this element".
            assert_eq!(
                Some(our_view.font_weight.unwrap_or(400)),
                theirs.font_weight,
                "{tag} font-weight"
            );
            // KNOWN GAP (render.md §10): Stylo's UA stylesheet gives h1/p a
            // default margin (1em = 16px); our minimal UA layer models only
            // display + bold, no UA margins. Compare only explicitly authored
            // margins: when ours is None (nothing authored), skip.
            if our_view.margin.top.is_some() {
                assert_eq!(our_view.margin.top, theirs.margin.top, "{tag} margin-top");
            }
        }
    }

    #[test]
    fn dual_engine_media_queries_agree() {
        // A rule gated by min-width that applies at 1280 but not at 320:
        // verify each engine honors the gate identically at blitz's fixed
        // 800px test viewport by choosing thresholds around it.
        let html = r#"<body><div class="resp">x</div></body>"#;
        let sheet = r#"
            @media (min-width: 700px) { .resp { color: blue; } }
            @media (max-width: 500px) { .resp { display: none; } }
        "#;
        let (doc, tree, rules) = dual_compute(html, sheet);

        let blitz_nid = doc.query_selector(".resp").unwrap().expect(".resp in blitz doc");
        let our_nid = tree.query_selector(".resp").unwrap().unwrap();

        let our_view = diting_css_cascade_full(&tree, our_nid, "div", &rules, None, None);
        let theirs = stylo_view(&doc, blitz_nid).expect("stylo");

        assert_eq!(our_view.color, theirs.color, "@media-gated color");
        assert_eq!(our_view.display, theirs.display, "@media-gated display");
    }

    #[test]
    fn dual_engine_inheritance_agrees() {
        // color inherits from body → p; border-like props do not exist in the
        // subset so only inherited ones are compared.
        let html = r#"<body style="color: #333333"><p>plain</p></body>"#;
        let sheet = "body { font-size: 15px; }";
        let (doc, tree, rules) = dual_compute(html, sheet);

        let blitz_p = doc.query_selector("p").unwrap().expect("p in blitz doc");
        let our_p = tree.query_selector("p").unwrap().unwrap();

        // The inline style lives on <body>; our cascade must walk the parent
        // chain (body → p) for inheritance, exactly like Stylo does.
        let our_body = tree.query_selector("body").unwrap().unwrap();
        let body_style = diting_css_cascade_full(&tree, our_body, "body", &rules, None, Some("color: #333333"));
        let our_view = diting_css_cascade_full(&tree, our_p, "p", &rules, Some(&body_style), None);
        let theirs = stylo_view(&doc, blitz_p).expect("stylo");

        assert_eq!(our_view.color, theirs.color, "inline-inherited color on body reaches p");
        assert_eq!(our_view.font_size, theirs.font_size, "font-size from body sheet inherits to p");
    }

    // -- helpers shared by the cross-check tests --

    fn cascade_simple(
        tree: &crate::diting_dom::tree::DomTree,
        nid: crate::diting_dom::tree::NodeId,
        tag: &str,
        rules: &[diting_css::ParsedRule],
    ) -> ComputedStyle {
        diting_css_cascade_full(tree, nid, tag, rules, None, None)
    }

    /// Full-fidelity entry into diting_css's cascade: match rules against the
    /// element, chain parent styles up the ancestor list, apply inline last.
    fn diting_css_cascade_full(
        tree: &crate::diting_dom::tree::DomTree,
        nid: crate::diting_dom::tree::NodeId,
        tag: &str,
        rules: &[diting_css::ParsedRule],
        parent: Option<&ComputedStyle>,
        inline: Option<&str>,
    ) -> ComputedStyle {
        let matched: Vec<(&diting_css::ParsedRule, u32)> = rules
            .iter()
            .filter_map(|rule| {
                let hits = tree.query_selector_all_from(tree.document(), &rule.selector).ok()?;
                if !hits.contains(&nid) {
                    return None;
                }
                let compiled = tree.compile_rule_selector(&rule.selector)?;
                Some((rule, compiled.specificity()))
            })
            .collect();
        diting_css::cascade_element(tag, tree, nid, &matched, parent, inline, diting_css::DEFAULT_ROOT_FONT_SIZE)
    }
}
