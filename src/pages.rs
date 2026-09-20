//! Page pump — the logical page-slicing layer above the engine.
//!
//! Slicing a rendered page two ways: **print** paginates the document into
//! fixed-height pages, breaking at top-level block boundaries (greedy: the
//! deepest block bottom that fits, with a half-page floor so pages never
//! collapse to slivers); **slides** turns every CSS-selector match into one
//! page sized to that element. Each page is painted as a viewport band off
//! the live tree's memoized layout — same primitive the timeline pump and
//! the screencast ride, no outerHTML re-parse, no Chromium.
//!
//! The final band of a print run uses a deliberately short viewport
//! (`vh = content_h - y`): band paint clamps `dy` to `content_h - vh`, so a
//! short last viewport makes the clamp land exactly on the page's start
//! offset instead of pulling it back into the previous page's content.

use diting::diting_browser::Page;

/// How the page set is cut.
#[derive(Debug, Clone)]
pub enum PageMode {
    /// Fixed-height pages over the whole document, breaking at top-level
    /// block boundaries where possible.
    Print,
    /// One page per CSS-selector match, sized to the element.
    Slides(String),
}

pub struct PagePumpOptions {
    pub mode: PageMode,
    /// Page size in CSS px — width always applies; height applies to print
    /// pagination (slides size each page to its element). Default A4 @96dpi.
    pub page_size: (f32, f32),
    /// Safety cap on emitted pages.
    pub max_pages: usize,
    /// Collect the vector text layer (PDF path): vectorizable text drops out
    /// of the band raster and comes back as font ops in `PageSet::text_ops`.
    /// Raster-only formats must leave this off or their pages lose text.
    pub collect_text: bool,
}

impl Default for PagePumpOptions {
    fn default() -> Self {
        Self {
            mode: PageMode::Print,
            page_size: (794.0, 1123.0),
            max_pages: 50,
            collect_text: false,
        }
    }
}

/// One painted page: raw RGBA at `width`×`height`, cut from `origin_y` in
/// document space.
#[derive(Debug)]
pub struct PageImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub origin_y: f32,
}

#[derive(Debug)]
pub struct PageSet {
    pub pages: Vec<PageImage>,
    /// Per-page PDF text ops (band-local), parallel to `pages`; empty vecs
    /// when the pump ran without `collect_text`.
    pub text_ops: Vec<Vec<diting::diting_layout::paint::PdfOp>>,
    /// Document content extent the set was cut from (CSS px).
    pub content_size: (f32, f32),
}

#[derive(Debug)]
pub enum PageError {
    /// No live document behind the page (pre-navigation or torn down).
    NoLiveDocument,
    /// Layout produced no positive content height — nothing to paginate.
    EmptyDocument,
    /// Slides mode: the selector matched nothing.
    NoSelectorMatches { selector: String },
    /// More pages than the cap; the caller should narrow the input.
    PageCapExceeded { asked: usize, cap: usize },
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::NoLiveDocument => write!(f, "no live document to cut pages from"),
            PageError::EmptyDocument => write!(f, "document layout has no content height"),
            PageError::NoSelectorMatches { selector } => {
                write!(f, "selector `{selector}` matched no elements")
            }
            PageError::PageCapExceeded { asked, cap } => {
                write!(f, "{asked} pages exceeds the cap of {cap}")
            }
        }
    }
}

impl std::error::Error for PageError {}

/// Print remainders below this (CSS px) merge into the previous page rather
/// than emitting a near-blank tail page.
const MIN_TAIL: f32 = 64.0;

/// A page break's document-space origin and size. Print bands always start
/// at `x = 0` (documents flow down); slide bands carry the element's own
/// origin — which for the standard horizontal flex-row deck means every
/// band shares `y = 0` and differs only in `x` (#60).
struct Band {
    x: f32,
    y: f32,
    vw: f32,
    vh: f32,
}

/// Render the live page into a set of page images. The page must already be
/// navigated; a probe band warms layout and fetches missing images before
/// breaks are read, so geometry comes from the fully-laid-out tree.
pub async fn render_page_set(
    page: &mut Page,
    opts: &PagePumpOptions,
) -> Result<PageSet, PageError> {
    let (w, page_h) = opts.page_size;

    // Probe band: forces a layout run and reports every img the tree is
    // missing (the collection walks the whole content extent, not just the
    // band). Fetch those through the page's client so later geometry sees
    // intrinsic sizes instead of placeholders.
    let (probe, missing) = paint(page, 0.0, 0.0, (w, page_h)).ok_or(PageError::NoLiveDocument)?;
    page.fetch_band_images(missing).await;
    let content_size = probe.content_size;

    let bands = match &opts.mode {
        PageMode::Print => print_bands(page, w, page_h)?,
        PageMode::Slides(selector) => slide_bands(page, selector)?,
    };
    if bands.len() > opts.max_pages {
        return Err(PageError::PageCapExceeded { asked: bands.len(), cap: opts.max_pages });
    }

    let mut pages = Vec::with_capacity(bands.len());
    let mut text_ops = Vec::with_capacity(bands.len());
    for band in bands {
        // Missing images that surface per-page (fetch failures stay missing;
        // a painted placeholder beats a stall). Cut semantics: (x, y) is a
        // document-space origin, not a scroll offset (#60).
        let (_, missing) = paint(page, band.x, band.y, (band.vw, band.vh))
            .ok_or(PageError::NoLiveDocument)?;
        if !missing.is_empty() {
            page.fetch_band_images(missing).await;
        }
        let (frame, _) = if opts.collect_text {
            paint_with_text(page, band.x, band.y, (band.vw, band.vh))
        } else {
            paint(page, band.x, band.y, (band.vw, band.vh))
        }
        .ok_or(PageError::NoLiveDocument)?;
        text_ops.push(frame.text_ops);
        pages.push(PageImage {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
            origin_y: band.y,
        });
    }
    Ok(PageSet { pages, text_ops, content_size })
}

/// Print pagination: greedy breaks at top-level block bottoms. For each
/// page, the candidate break is the largest `body`-child bottom inside
/// `(y + 0.55·page_h, y + page_h]` — blocks taller than a page guillotine at
/// the nominal boundary (v1 accepts cutting nested content inside an
/// oversized top-level block).
fn print_bands(page: &mut Page, page_w: f32, page_h: f32) -> Result<Vec<Band>, PageError> {
    // Bottoms of body's direct children (document coords — gBCR reads the
    // same Rust layout cache the band paint uses), plus the scroll extent
    // and viewport height in one probe. Script/style and friends have no
    // visual box — their gBCR would fall back to synthetic grid cells and
    // poison the break search, so they're skipped in JS.
    let probe = page.evaluate(
        "(() => { const skip = new Set(['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','LINK','META']); \
         const kids = (document.body && document.body.children) || []; \
         const out = []; \
         for (const e of kids) { if (skip.has(e.tagName)) continue; out.push(e.getBoundingClientRect().bottom); } \
         const d = document.documentElement; \
         return { b: out, sh: d.scrollHeight, ih: window.innerHeight }; })()",
    );
    let obj = probe.as_object();
    let mut bottoms: Vec<f32> = obj
        .and_then(|o| o.get("b"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_f64().map(|n| n as f32)).collect())
        .unwrap_or_default();
    bottoms.retain(|b| b.is_finite() && *b > 0.0);
    bottoms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let (sh, ih) = obj
        .map(|o| {
            (
                o.get("sh").and_then(|v| v.as_f64()).unwrap_or(0.0),
                o.get("ih").and_then(|v| v.as_f64()).unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0));
    // scrollHeight is viewport-clamped (Chrome semantics: at least
    // innerHeight) — right for scrolling, wrong for pagination, where a
    // document shorter than the viewport must not grow blank tail pages.
    // Overflowing scrollHeight IS the content extent; otherwise the deepest
    // block bottom is. A bare-text body has no block bottoms at all, so the
    // text-ink extent off the same layout the band paint rides takes over —
    // never scrollHeight, which for it is just the viewport wearing a hat.
    // (The test viewport is persona-random, so the old numbers were flaky
    // by whole pages.)
    let content_h = if sh.is_finite() && sh > 0.0 && sh > ih {
        sh as f32
    } else {
        let block_h = bottoms.last().copied().unwrap_or(0.0);
        let ink_h = page.text_ink_extent().map(|(_, h)| h).unwrap_or(0.0);
        block_h.max(ink_h)
    };
    if !(content_h.is_finite() && content_h > 0.0) {
        return Err(PageError::EmptyDocument);
    }

    let mut bands: Vec<Band> = Vec::new();
    let mut y = 0.0f32;
    while y < content_h {
        let remaining = content_h - y;
        if remaining <= page_h {
            // Last page: short viewport makes the dy clamp land exactly on y.
            bands.push(Band { x: 0.0, y, vw: page_w, vh: remaining.max(1.0) });
            break;
        }
        let floor = y + page_h * 0.55;
        let ceiling = y + page_h;
        // Largest block bottom that fits with the half-page floor behind it.
        let next = bottoms
            .iter()
            .rev()
            .find(|b| **b > floor && **b <= ceiling)
            .copied()
            .unwrap_or(ceiling);
        bands.push(Band { x: 0.0, y, vw: page_w, vh: (next - y).max(1.0) });
        y = next;
    }
    // Merge a sub-MIN_TAIL remainder into the previous page instead of a
    // near-blank tail (per-page MediaBoxes make variable heights free).
    if bands.len() >= 2 {
        let last = bands.last().expect("len checked").vh;
        if last < MIN_TAIL {
            let tail = bands.pop().expect("len checked");
            bands.last_mut().expect("len checked").vh += tail.vh;
        }
    }
    Ok(bands)
}

/// Slides pagination: every selector match becomes one band at the element's
/// own origin and size. Horizontal decks (the flex-row + translateX layout)
/// share `y = 0` and differ only in `x` — the band must carry both axes or
/// every match renders the first viewport's frame (#60).
fn slide_bands(page: &mut Page, selector: &str) -> Result<Vec<Band>, PageError> {
    // The selector rides in as a JSON-encoded string literal — it is page
    // input, never trusted JS source.
    let lit = serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string());
    let rects_json = page.evaluate(&format!(
        "(() => {{ return Array.from(document.querySelectorAll({lit})) \
         .map(e => {{ const r = e.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; }}); }})()"
    ));
    let rects: Vec<(f32, f32, f32, f32)> = rects_json
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let quad = v.as_array()?;
                    let x = quad.first()?.as_f64()? as f32;
                    let y = quad.get(1)?.as_f64()? as f32;
                    let w = quad.get(2)?.as_f64()? as f32;
                    let h = quad.get(3)?.as_f64()? as f32;
                    Some((x, y, w, h))
                })
                .collect()
        })
        .unwrap_or_default();
    if rects.is_empty() {
        return Err(PageError::NoSelectorMatches { selector: selector.to_string() });
    }
    Ok(rects
        .into_iter()
        .filter(|(_, _, w, h)| *w >= 1.0 && *h >= 1.0)
        .map(|(x, y, w, h)| Band { x, y, vw: w, vh: h })
        .collect())
}

/// One band paint, the shared primitive. Cut semantics (#60): `(x, y)` is
/// the page's document-space origin, not a scroll offset — the root-overflow
/// extent collapse must not eat it.
fn paint(
    page: &Page,
    x: f32,
    y: f32,
    viewport: (f32, f32),
) -> Option<(diting::diting_js::ops::BandFrame, Vec<String>)> {
    page.viewport_band_cut(x, y, viewport)
}

/// One band paint with the vector text layer collected (the PDF path): the
/// band raster comes back without vectorizable text; `frame.text_ops` carries
/// the glyph lines to re-emit as font objects.
fn paint_with_text(
    page: &Page,
    x: f32,
    y: f32,
    viewport: (f32, f32),
) -> Option<(diting::diting_js::ops::BandFrame, Vec<String>)> {
    page.viewport_band_cut_with_text(x, y, viewport)
}

// ---------------------------------------------------------------------------
// Packaging encoders — bytes out of the painted pages.
// ---------------------------------------------------------------------------

/// Encode an RGBA frame as JPEG (the PDF embedding path; quality 1-100).
pub fn jpeg_of(width: u32, height: u32, rgba: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "frame buffer size mismatch".to_string())?;
    let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    rgb.write_with_encoder(encoder).map_err(|e| format!("jpeg encode: {e}"))?;
    Ok(out.into_inner())
}

/// Encode an RGBA frame as PNG (the standalone-pages path).
pub fn png_of(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder =
            png::Encoder::new(std::io::Cursor::new(&mut out), width.max(1), height.max(1));
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| format!("png header: {e}"))?;
        writer.write_image_data(rgba).map_err(|e| format!("png encode: {e}"))?;
    }
    Ok(out)
}

/// CSS px → PDF points (96dpi assumption, the same one CSS `px` carries).
const PX_TO_PT: f64 = 72.0 / 96.0;

/// Pack per-page JPEGs + vector text layers as a PDF 1.4: each page embeds
/// the painted band as a DCTDecode XObject (`/Filter /DCTDecode` — the JPEG
/// stream goes in verbatim, no re-encode) and redraws vectorizable text on
/// top as a real text layer — Type0/CIDFontType2 fonts with the bundled
/// TTFs as FontFile2, per-glyph /W widths, and a ToUnicode CMap so text
/// extraction gets real Unicode back. Text ops are band-local px; the
/// writer flips y to the PDF bottom-up point space itself. Hand-rolled
/// writer — zero new dependencies.
pub fn pdf_of_pages(
    pages: &[(u32, u32, &[u8], &[diting::diting_layout::paint::PdfOp])],
) -> Vec<u8> {
    use diting::diting_layout::paint::PdfOp;
    use diting::diting_layout::text::PdfFace;
    use std::collections::BTreeMap;

    let fonts = diting::diting_fonts::font_book();

    // Face usage scan: widths stored as px/em ratio (scaled to font units
    // once the face's upem is known) and first-text-per-gid for ToUnicode.
    let mut widths: BTreeMap<PdfFace, BTreeMap<u16, f32>> = BTreeMap::new();
    let mut to_unicode: BTreeMap<PdfFace, BTreeMap<u16, String>> = BTreeMap::new();
    for (_, _, _, ops) in pages {
        for op in *ops {
            let PdfOp::Line(l) = op else { continue };
            for g in &l.glyphs {
                widths
                    .entry(g.face)
                    .or_default()
                    .entry(g.gid)
                    .or_insert(g.advance / l.font_size.max(1.0));
                if !g.unicode.is_empty() {
                    to_unicode
                        .entry(g.face)
                        .or_default()
                        .entry(g.gid)
                        .or_insert_with(|| g.unicode.clone());
                }
            }
        }
    }
    let used_faces: Vec<PdfFace> = [PdfFace::Regular, PdfFace::Bold, PdfFace::Mono]
        .into_iter()
        .filter(|f| widths.contains_key(f))
        .collect();
    let mut metrics: BTreeMap<PdfFace, (f32, f32, f32)> = BTreeMap::new();
    for f in &used_faces {
        let m = fonts.pdf_face_metrics(*f).unwrap_or((800.0, -200.0, 1000.0));
        metrics.insert(*f, m);
    }
    for (f, ws) in widths.iter_mut() {
        let upem = metrics[f].2;
        for w in ws.values_mut() {
            *w = (*w * upem).round();
        }
    }

    let base_font = |f: &PdfFace| -> &'static str {
        match f {
            PdfFace::Regular => "DitingCJK-Regular",
            PdfFace::Bold => "DitingCJK-Bold",
            PdfFace::Mono => "DitingMono-Regular",
        }
    };
    let res_name = |f: &PdfFace| -> &'static str {
        match f {
            PdfFace::Regular => "F1",
            PdfFace::Bold => "F2",
            PdfFace::Mono => "F3",
        }
    };
    let w_array = |f: &PdfFace| -> String {
        let mut s = String::from("[");
        for (gid, w) in &widths[f] {
            s.push_str(&format!(" {gid} [{w:.0}]"));
        }
        s.push_str(" ]");
        s
    };
    let to_unicode_stream = |f: &PdfFace| -> String {
        let mut s = String::from(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
             /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
             1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
        );
        let entries: Vec<(&u16, &String)> = to_unicode[f].iter().collect();
        for chunk in entries.chunks(100) {
            s.push_str(&format!("{} beginbfchar\n", chunk.len()));
            for (gid, text) in chunk {
                let mut hex = String::new();
                for u in text.encode_utf16() {
                    hex.push_str(&format!("{u:04X}"));
                }
                s.push_str(&format!("<{gid:04X}> <{hex}>\n"));
            }
            s.push_str("endbfchar\n");
        }
        s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
        s
    };

    let face_base = 3 + 3 * pages.len();
    let face_ids: BTreeMap<PdfFace, usize> = used_faces
        .iter()
        .enumerate()
        .map(|(j, f)| (*f, face_base + 5 * j))
        .collect();

    let mut out: Vec<u8> = Vec::new();
    // The binary marker comment line flags the file as containing binary
    // streams (the JPEG data), so transport that peeks at the head doesn't
    // treat the file as text.
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    // offsets[i] is the byte offset of object (i + 1).
    let mut offsets: Vec<usize> = Vec::with_capacity(2 + 3 * pages.len() + 5 * used_faces.len());

    offsets.push(out.len());
    out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let kids: Vec<String> =
        (0..pages.len()).map(|i| format!("{} 0 R", 3 + 3 * i)).collect();
    offsets.push(out.len());
    out.extend_from_slice(
        format!(
            "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {} >>\nendobj\n",
            kids.join(" "),
            pages.len()
        )
        .as_bytes(),
    );

    for (i, &(w, h, jpeg, text_ops)) in pages.iter().enumerate() {
        let wpt = w as f64 * PX_TO_PT;
        let hpt = h as f64 * PX_TO_PT;
        let page_num = 3 + 3 * i;
        let img_num = page_num + 1;
        let content_num = page_num + 2;
        let font_res = if used_faces.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = used_faces
                .iter()
                .map(|f| format!("/{} {} 0 R", res_name(f), face_ids[f]))
                .collect();
            format!(" /Font << {} >>", list.join(" "))
        };

        // Text layer over the flattened raster: clip brackets re-expressed
        // as `q re W n` pairs, then BT/Tf/Tm/Tj runs with decoration fills.
        let mut content = format!("q {wpt:.2} 0 0 {hpt:.2} 0 0 cm /Im0 Do Q\n");
        let mut clip_depth = 0usize;
        for op in text_ops {
            match op {
                PdfOp::Clip { x, y, w: cw, h: ch } => {
                    content.push_str(&format!(
                        "q {:.2} {:.2} {:.2} {:.2} re W n\n",
                        *x as f64 * PX_TO_PT,
                        (h as f64 - (*y + *ch) as f64) * PX_TO_PT,
                        *cw as f64 * PX_TO_PT,
                        *ch as f64 * PX_TO_PT,
                    ));
                    clip_depth += 1;
                }
                PdfOp::PopClip => {
                    if clip_depth > 0 {
                        content.push_str("Q\n");
                        clip_depth -= 1;
                    }
                }
                PdfOp::Line(l) => {
                    let [r, g, b, _] = l.color;
                    content.push_str(&format!(
                        "{:.3} {:.3} {:.3} rg\nBT\n",
                        r as f64 / 255.0,
                        g as f64 / 255.0,
                        b as f64 / 255.0,
                    ));
                    let mut cur: Option<PdfFace> = None;
                    for glyph in &l.glyphs {
                        if cur != Some(glyph.face) {
                            content.push_str(&format!(
                                "/{} {:.3} Tf\n",
                                res_name(&glyph.face),
                                l.font_size as f64 * PX_TO_PT,
                            ));
                            cur = Some(glyph.face);
                        }
                        content.push_str(&format!(
                            "1 0 0 1 {:.2} {:.2} Tm <{:04X}> Tj\n",
                            glyph.x as f64 * PX_TO_PT,
                            (h as f64 - glyph.y as f64) * PX_TO_PT,
                            glyph.gid,
                        ));
                    }
                    content.push_str("ET\n");
                    let th = (l.font_size / 16.0).round().max(1.0) as f64;
                    for &(sx, sy, sw) in &l.strokes {
                        content.push_str(&format!(
                            "{:.2} {:.2} {:.2} {:.2} re f\n",
                            sx as f64 * PX_TO_PT,
                            (h as f64 - (sy + th as f32) as f64) * PX_TO_PT,
                            sw as f64 * PX_TO_PT,
                            th * PX_TO_PT,
                        ));
                    }
                }
            }
        }

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{page_num} 0 obj\n<< /Type /Page /Parent 2 0 R \
                 /MediaBox [0 0 {wpt:.2} {hpt:.2}] \
                 /Resources <<{font_res} /XObject << /Im0 {img_num} 0 R >> >> \
                 /Contents {content_num} 0 R >>\nendobj\n"
            )
            .as_bytes(),
        );
        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{img_num} 0 obj\n<< /Type /XObject /Subtype /Image \
                 /Width {w} /Height {h} /ColorSpace /DeviceRGB \
                 /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
                jpeg.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(jpeg);
        out.extend_from_slice(b"\nendstream\nendobj\n");

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{content_num} 0 obj\n<< /Length {} >>\nstream\n{content}endstream\nendobj\n",
                content.len()
            )
            .as_bytes(),
        );
    }

    // Embedded font programs: five objects per used face — Type0 wrapper,
    // CIDFontType2, FontDescriptor, FontFile2 (raw TTF), ToUnicode CMap.
    for f in &used_faces {
        let fid = face_ids[f];
        let (cid, desc, file, cmap) = (fid + 1, fid + 2, fid + 3, fid + 4);
        let name = base_font(f);
        let (asc, dsc, upem) = metrics[f];
        let bytes = fonts.pdf_face_bytes(*f);

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{fid} 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /{name} \
                 /Encoding /Identity-H /DescendantFonts [{cid} 0 R] \
                 /ToUnicode {cmap} 0 R >>\nendobj\n"
            )
            .as_bytes(),
        );

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{cid} 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /{name} \
                 /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                 /FontDescriptor {desc} 0 R /DW {:.0} /W {} /CIDToGIDMap /Identity >>\nendobj\n",
                upem * 0.6,
                w_array(f),
            )
            .as_bytes(),
        );

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{desc} 0 obj\n<< /Type /FontDescriptor /FontName /{name} /Flags 4 \
                 /FontBBox [-1000 -300 2100 1100] /ItalicAngle 0 \
                 /Ascent {asc:.0} /Descent {dsc:.0} /CapHeight 700 /StemV 80 \
                 /FontFile2 {file} 0 R >>\nendobj\n"
            )
            .as_bytes(),
        );

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{file} 0 obj\n<< /Length {} /Length1 {} >>\nstream\n",
                bytes.len(),
                bytes.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\nendstream\nendobj\n");

        let tounicode = to_unicode_stream(f);
        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{cmap} 0 obj\n<< /Length {} >>\nstream\n{tounicode}endstream\nendobj\n",
                tounicode.len()
            )
            .as_bytes(),
        );
    }

    let xref_pos = out.len();
    let total_objs = 2 + 3 * pages.len() + 5 * used_faces.len();
    out.extend_from_slice(format!("xref\n0 {}\n", total_objs + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            total_objs + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(all(test, feature = "screenshot"))]
mod tests {
    use super::*;
    use diting::diting_browser::lifecycle::WaitUntil;
    use diting::diting_browser::{BrowserContext, Page as EnginePage};
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// Four 200px blocks at width 400; paginated at page_h=300 every block
    /// is its own page (each 200 fits, none shares a page under the 0.55
    /// floor: two blocks = 400 > 300).
    const PRINT_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0}
.blk{width:400px;height:200px}
</style></head><body>
<div class="blk" style="background:#c0392b"></div>
<div class="blk" style="background:#27ae60"></div>
<div class="blk" style="background:#2980b9"></div>
<div class="blk" style="background:#8e44ad"></div>
</body></html>"#;

    /// Three .slide elements of different heights stacked with gaps.
    const SLIDES_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0}
.slide{width:400px}
#s1{height:120px;background:#c0392b}
.gap{width:400px;height:80px;background:#dddddd}
#s2{height:200px;background:#27ae60}
#s3{height:160px;background:#2980b9}
</style></head><body>
<div class="slide" id="s1"></div>
<div class="gap"></div>
<div class="slide" id="s2"></div>
<div class="slide" id="s3"></div>
</body></html>"#;

    /// The standard horizontal deck: flex row, every slide 100% of the row,
    /// `overflow: hidden` on the root (translateX carousels all carry it).
    /// Every match shares y=0 and differs only in x — pins both layers of
    /// #60: the y-only band collection AND the root-overflow extent collapse
    /// that would clamp a naive scroll_x back to 0.
    const HSLIDES_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0;overflow:hidden}
.deck{display:flex;width:800px;height:300px}
.slide{flex:0 0 400px;height:300px}
#h1{background:#c0392b}
#h2{background:#2980b9}
</style></head><body>
<div class="deck"><div class="slide" id="h1"></div><div class="slide" id="h2"></div></div>
</body></html>"#;

    /// The export idiom the 智能体手机 deck rides: screen layout is the flex
    /// carousel (script writes inline viewport-unit transforms), and the
    /// author parks the one-slide-per-page layout in `@media print` —
    /// `display:block` un-stack plus `transform:none !important` to cancel
    /// the inline offsets, with the deck's `height:100%` folding to auto
    /// against the now-auto body (§10.5) so it grows to stack every slide —
    /// the fixed-height deck would keep clipping slides 2+ via its own
    /// `overflow:hidden` (#65). Print slides are 1500px so the stacked
    /// content (3000) exceeds every persona viewport (≤2452): the deck
    /// height probe below is then deterministic — folded it's 3000, unfixed
    /// it sticks at the viewport. Chrome's save-as-PDF runs under print
    /// media, and `/pdf` now does the same. Discriminators: block-stretched
    /// slide width (800, not the flex 400) catches a dead print arm; the
    /// stacked y origins (0/1500) catch a surviving inline transform; the
    /// deck height catches the §10.5 fold; distinct inked pages catch the
    /// rest.
    const DECK_PRINT_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0;overflow:hidden;height:100%}
.deck{display:flex;width:800px;height:100%;overflow:hidden}
.slide{flex:0 0 400px;height:300px}
#h1{background:#c0392b}
#h2{background:#2980b9}
@media print {
  html,body{overflow:visible;height:auto}
  .deck{display:block}
  .slide{page-break-after:always;height:1500px;transform:none !important}
}
</style></head><body>
<div class="deck"><div class="slide" id="h1"></div><div class="slide" id="h2"></div></div>
<script>
document.querySelectorAll('.slide').forEach(function(el, i) {
  el.style.transform = 'translateY(' + (100 * i) + 'vh)';
});
</script>
</body></html>"#;

    /// Text nodes only — body has no element children, so the probe's child
    /// bottoms list is empty and body's own box stretches to the viewport:
    /// the extent must come from the Text paint items. ~24 wrapped lines at
    /// line-height 20px ≈ 480px: paginates at page_h 300 but is far shorter
    /// than the 1000px test viewport — the old fallback (viewport-clamped
    /// scrollHeight) turned this into 4 mostly-blank pages.
    fn bare_text_html() -> &'static str {
        let text = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
             eiusmod tempor incididunt ut labore et dolore magna aliqua ut enim ad \
             minim veniam quis nostrud exercitation ullamco laboris nisi ut aliquip \
             ex ea commodo consequat duis aute irure dolor in reprehenderit in \
             voluptate velit esse cillum dolore eu fugiat nulla pariatur ";
        let html = format!(
            r#"<!doctype html><html><head><style>
html,body{{margin:0;padding:0;width:400px;font-size:16px;line-height:20px;color:#000}}
</style></head><body>{}</body></html>"#,
            text.repeat(3)
        );
        Box::leak(html.into_boxed_str())
    }

    fn spawn_html_server(html: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let body = html.as_bytes();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_page() -> EnginePage {
        let context = Arc::new(BrowserContext::with_storage_and_network(
            "pages-test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: the fixture server is 127.0.0.1
            None,
        ));
        EnginePage::new("pages-page".into(), context)
    }

    async fn navigated(html: &'static str, path: &str) -> EnginePage {
        let port = spawn_html_server(html);
        let mut page = test_page();
        page.navigate_with_wait(&format!("http://127.0.0.1:{port}/{path}"), WaitUntil::Load)
            .await
            .expect("navigate fixture");
        page.settle_until_idle(5000).await;
        page
    }

    /// Every page must carry real content — a blank (all-white) band means
    /// the break logic walked past the content.
    fn has_ink(frame: &diting::diting_js::ops::BandFrame) -> bool {
        frame
            .rgba
            .chunks_exact(4)
            .any(|px| px[0] != 255 || px[1] != 255 || px[2] != 255)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn print_breaks_on_block_boundaries() {
        let mut page = navigated(PRINT_HTML, "print.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Print,
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        let set = render_page_set(&mut page, &opts).await.expect("page set renders");
        assert_eq!(set.pages.len(), 4, "one page per 200px block at page_h 300: {:?}", set.pages.iter().map(|p| (p.origin_y, p.height)).collect::<Vec<_>>());
        for (i, p) in set.pages.iter().enumerate() {
            assert_eq!((p.width, p.height), (400, 200), "page {i} size");
            assert_eq!(p.origin_y, (i as f32) * 200.0, "page {i} origin");
            assert!(has_ink(&p_rgba_frame(p)), "page {i} is not blank");
        }
    }

    // Helper: build a BandFrame view of a PageImage for has_ink (origin_y
    // and content size don't matter for the ink check).
    fn p_rgba_frame(p: &PageImage) -> diting::diting_js::ops::BandFrame {
        diting::diting_js::ops::BandFrame {
            rgba: p.rgba.clone(),
            width: p.width,
            height: p.height,
            dx: 0.0,
            dy: 0.0,
            content_size: (p.width as f32, p.height as f32),
            text_ops: Vec::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slides_one_page_per_match() {
        let mut page = navigated(SLIDES_HTML, "slides.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".slide".to_string()),
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        let set = render_page_set(&mut page, &opts).await.expect("slides render");
        assert_eq!(set.pages.len(), 3, "gap div is not a .slide: {:?}", set.pages.iter().map(|p| (p.origin_y, p.height)).collect::<Vec<_>>());
        // Heights match the elements: 120 / 200 / 160 (±1px layout rounding).
        let heights: Vec<u32> = set.pages.iter().map(|p| p.height).collect();
        assert_eq!(heights[0], 120);
        assert_eq!(heights[1], 200);
        assert_eq!(heights[2], 160);
        // Origins are the document offsets: 0, 200 (120 + 80 gap), 400.
        assert_eq!(set.pages[0].origin_y, 0.0);
        assert_eq!(set.pages[1].origin_y, 200.0);
        assert_eq!(set.pages[2].origin_y, 400.0);
        for (i, p) in set.pages.iter().enumerate() {
            assert!(has_ink(&p_rgba_frame(p)), "slide {i} is not blank");
        }
    }

    /// #60: a horizontal flex-row deck with the root `overflow: hidden` —
    /// every match shares y=0, so the y-only band collection used to emit
    /// the first viewport's frame for every slide (and the scroll-semantics
    /// extent collapse would have eaten a naive scroll_x too). The two pages
    /// must be distinct pixels, each sized to its element.
    #[tokio::test(flavor = "current_thread")]
    async fn slides_horizontal_deck_pages_differ() {
        let mut page = navigated(HSLIDES_HTML, "hslides.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".slide".to_string()),
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        let set = render_page_set(&mut page, &opts).await.expect("horizontal slides render");
        assert_eq!(set.pages.len(), 2);
        for (i, p) in set.pages.iter().enumerate() {
            assert_eq!((p.width, p.height), (400, 300), "slide {i} sized to its element");
            assert!(has_ink(&p_rgba_frame(p)), "slide {i} is not blank");
        }
        assert_ne!(
            set.pages[0].rgba, set.pages[1].rgba,
            "horizontal deck must not emit the same frame for every match (#60)"
        );
    }

    /// The `/pdf` export contract for deck pages (the 智能体手机 PPT incident):
    /// print-media emulation must activate the `@media print` un-stack (block
    /// layout, slides stacked vertically), the block's
    /// `transform:none !important` must cancel the inline viewport-unit
    /// transforms the carousel script wrote, and the deck's `height:100%`
    /// must fold to auto against the auto-height body (§10.5, #65) so the
    /// deck grows to its stacked content instead of sticking at one viewport
    /// and clipping slides 2+ through its own `overflow:hidden`.
    #[tokio::test(flavor = "current_thread")]
    async fn slides_print_media_unstacks_deck_over_inline_transform() {
        let mut page = navigated(DECK_PRINT_HTML, "deckprint.html").await;
        // What do_pdf pins before navigation: print media (Chrome's
        // save-as-PDF semantics).
        page.set_emulated_media(None, Some(Some("print".into())));
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".slide".to_string()),
            page_size: (800.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        // The §10.5 fold, directly: the deck's own box must span the stacked
        // slides (2 × 1500). Unfolded it sticks at the persona viewport
        // (902..2452 observed) — 3000 clears every persona, so the assert is
        // deterministic both ways.
        let deck_h = page
            .evaluate("document.querySelector('.deck').getBoundingClientRect().height")
            .as_f64()
            .unwrap_or(0.0);
        assert!(
            deck_h >= 2999.0,
            "deck height:100% must fold to auto against the auto body and span the stacked slides (#65); got {deck_h}"
        );
        let set = render_page_set(&mut page, &opts).await.expect("print deck renders");
        assert_eq!(set.pages.len(), 2);
        for (i, p) in set.pages.iter().enumerate() {
            assert_eq!((p.width, p.height), (800, 1500), "slide {i} block-stretched, print height honored");
            assert!(has_ink(&p_rgba_frame(p)), "slide {i} is not blank");
        }
        assert_eq!(set.pages[0].origin_y, 0.0, "slide 1 at the top");
        assert_eq!(
            set.pages[1].origin_y, 1500.0,
            "slide 2 stacked below slide 1 — the print un-stack with the inline transform canceled"
        );
        assert_ne!(set.pages[0].rgba, set.pages[1].rgba, "the two slides carry distinct backgrounds");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn selector_without_matches_errors() {
        let mut page = navigated(PRINT_HTML, "nomatch.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".missing".to_string()),
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        let err = render_page_set(&mut page, &opts).await.unwrap_err();
        assert!(matches!(err, PageError::NoSelectorMatches { .. }), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn page_cap_is_enforced() {
        let mut page = navigated(PRINT_HTML, "cap.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Print,
            page_size: (400.0, 300.0),
            max_pages: 2,
            collect_text: false,
        };
        let err = render_page_set(&mut page, &opts).await.unwrap_err();
        assert!(matches!(err, PageError::PageCapExceeded { asked: 4, cap: 2 }), "{err}");
    }

    /// Bare-text body: no element children → the extent must come from the
    /// text paint items (html/body stretch to the viewport, so even body's
    /// own box lies). The text's ink extent (est. 480px) paginates into
    /// exactly 2 pages, every page carries ink. The old fallback was
    /// viewport-clamped scrollHeight = innerHeight — and the test viewport is
    /// persona-random (902..2452 observed), so the bug paginated into a
    /// persona-dependent 3-8 mostly-blank pages.
    #[tokio::test(flavor = "current_thread")]
    async fn bare_text_body_paginates_without_blank_tail() {
        let mut page = navigated(bare_text_html(), "bare.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Print,
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: false,
        };
        let set = render_page_set(&mut page, &opts).await.expect("bare text renders");
        assert_eq!(set.pages.len(), 2, "ink extent 480px at page_h 300 is 2 pages: {:?}", set.pages.iter().map(|p| (p.origin_y, p.height)).collect::<Vec<_>>());
        for (i, p) in set.pages.iter().enumerate() {
            assert!(has_ink(&p_rgba_frame(p)), "page {i} is blank — blank-tail bug");
        }
    }

    /// The PDF writer's shape: header with binary marker, per-page objects,
    /// a well-formed xref + trailer, and verbatim JPEG bytes inside streams.
    #[test]
    fn pdf_packaging_shape() {
        // Minimal real JPEGs (SOI + EOI is enough for the writer — it never
        // parses the stream).
        let j1 = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 0xFF, 0xD9];
        let j2 = vec![0xFF, 0xD8, 9, 9, 0xFF, 0xD9];
        let pdf = pdf_of_pages(&[(794, 1123, &j1, &[]), (794, 200, &j2, &[])]);
        assert!(pdf.starts_with(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3"), "header + binary marker");
        assert!(pdf.ends_with(b"%%EOF\n"), "trailer");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Count 2"), "pages count");
        assert!(text.contains("/Filter /DCTDecode"), "jpeg embedding");
        assert!(text.contains("startxref"), "xref pointer");
        // xref offsets must point at real objects: the first listed offset
        // is object 1's "1 0 obj".
        let start = text.rfind("startxref").expect("marker");
        let xref_at: usize = text[start..]
            .trim_start_matches("startxref\n")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .expect("numeric startxref");
        assert_eq!(&pdf[xref_at..xref_at + 4], b"xref", "startxref points at the xref table");
        // Every "n" entry's offset should land on "<n> 0 obj". The walk runs
        // on raw bytes, not the lossy `text`: the binary marker and the JPEG
        // streams contain invalid UTF-8 that from_utf8_lossy widens to
        // U+FFFD, shifting every later byte offset in the string. skip(3)
        // walks past "xref", the count line, and the free entry (…65535 f)
        // that precedes them; entries carry the 20-byte trailing space, so
        // the line is trimmed before the n-check.
        let entries: Vec<&[u8]> = pdf[xref_at..]
            .split(|&b| b == b'\n')
            .skip(3)
            .take_while(|l| l.trim_ascii().ends_with(b"n"))
            .collect();
        assert_eq!(entries.len(), 8, "2 fixed + 3×2 objects");
        for (i, e) in entries.iter().enumerate() {
            let off: usize = std::str::from_utf8(&e[..10])
                .ok()
                .and_then(|s| s.parse().ok())
                .expect("10-digit offset");
            let expect = format!("{} 0 obj", i + 1);
            assert!(pdf[off..].starts_with(expect.as_bytes()), "obj {} offset", i + 1);
        }
    }

    /// The vector text layer end-to-end at the writer level: hand-built
    /// glyph ops in, Type0/CIDFontType2/FontFile2/ToUnicode out, BT/Tf/Tm/Tj
    /// runs with clip brackets and decoration fills, and an xref that still
    /// points at every object once the 5-per-face block is appended.
    #[test]
    fn pdf_text_layer_embeds_fonts_and_glyph_ops() {
        use diting::diting_layout::paint::{PdfLine, PdfOp};
        use diting::diting_layout::text::{PdfFace, PdfGlyph};

        let j1 = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xD9];
        let ops = vec![
            PdfOp::Clip { x: 0.0, y: 0.0, w: 400.0, h: 100.0 },
            PdfOp::Line(PdfLine {
                font_size: 16.0,
                color: [0, 0, 0, 255],
                glyphs: vec![
                    PdfGlyph {
                        gid: 68,
                        x: 10.0,
                        y: 20.0,
                        advance: 9.0,
                        face: PdfFace::Regular,
                        unicode: "A".into(),
                    },
                    PdfGlyph {
                        gid: 120,
                        x: 25.0,
                        y: 20.0,
                        advance: 8.0,
                        face: PdfFace::Regular,
                        unicode: String::new(),
                    },
                    PdfGlyph {
                        gid: 1234,
                        x: 50.0,
                        y: 40.0,
                        advance: 16.0,
                        face: PdfFace::Regular,
                        unicode: "中".into(),
                    },
                ],
                strokes: vec![(10.0, 24.0, 9.0)],
            }),
            PdfOp::PopClip,
            PdfOp::Line(PdfLine {
                font_size: 16.0,
                color: [0, 0, 0, 255],
                glyphs: vec![PdfGlyph {
                    gid: 70,
                    x: 10.0,
                    y: 60.0,
                    advance: 9.0,
                    face: PdfFace::Bold,
                    unicode: "B".into(),
                }],
                strokes: Vec::new(),
            }),
        ];
        let pdf = pdf_of_pages(&[(794, 1123, &j1, &ops)]);
        // contains() is position-free, so the lossy whole-file string is
        // safe even though the JPEG bytes sit mid-file; the xref walk below
        // stays on raw bytes.
        let text = String::from_utf8_lossy(&pdf);

        // Font stack: one 5-object block per used face (Regular + Bold, no
        // Mono since nothing routes to it).
        assert!(text.contains("/Subtype /Type0"), "Type0 wrapper");
        assert!(text.contains("/Subtype /CIDFontType2"), "CID font");
        assert!(text.contains("/Encoding /Identity-H"));
        assert!(text.contains("/FontFile2"), "embedded TTF");
        assert!(
            text.contains("/ToUnicode 10 0 R") && text.contains("/ToUnicode 15 0 R"),
            "Type0 dict links its CMap (what extractors follow)"
        );
        assert!(text.contains("DitingCJK-Regular"), "regular base font");
        assert!(text.contains("DitingCJK-Bold"), "bold base font");
        assert!(!text.contains("DitingMono-Regular"), "unused mono face not embedded");

        // The FontFile2 stream is the shaper's own bytes, verbatim.
        let fonts = diting::diting_fonts::font_book();
        let regular = fonts.pdf_face_bytes(PdfFace::Regular);
        assert!(
            text.contains(&format!("/Length1 {}", regular.len())),
            "FontFile2 carries the full TTF"
        );

        // Content stream: image draw, clip bracket (y flipped), color +
        // BT/ET, face-switched Tf, per-glyph Tm/Tj, underline fill.
        assert!(text.contains("q 595.50 0 0 842.25 0 0 cm /Im0 Do Q"), "image draw");
        assert!(text.contains("/MediaBox [0 0 595.50 842.25]"), "px → pt");
        assert!(text.contains("q 0.00 767.25 300.00 75.00 re W n"), "clip, y flipped");
        assert!(text.contains("0.000 0.000 0.000 rg\nBT\n"), "color then BT");
        assert!(text.contains("/F1 12.000 Tf"), "16px @96dpi → 12pt");
        assert!(text.contains("1 0 0 1 7.50 827.25 Tm <0044> Tj"), "glyph 'A'");
        assert!(text.contains("<0078> Tj"), "continuation glyph, no extra Tf");
        assert!(text.contains("<04D2> Tj"), "CJK glyph '中'");
        assert!(text.contains("/F2 12.000 Tf"), "bold face switch");
        assert!(text.contains("<0046> Tj"), "bold glyph 'B'");
        assert!(text.contains("ET\n"));
        assert!(text.contains("7.50 823.50 6.75 0.75 re f"), "underline fill");
        assert!(text.contains("Q\n"), "pop clip");

        // Page resources reference exactly the used faces.
        assert!(text.contains("/Font << /F1 6 0 R /F2 11 0 R >>"), "F1@6 F2@11");

        // CID widths: px advances scaled to font units per em.
        let (_, _, upem) = fonts.pdf_face_metrics(PdfFace::Regular).unwrap();
        assert!(
            text.contains(&format!("68 [{}", (9.0 / 16.0 * upem).round())),
            "/W entry for gid 68"
        );
        assert!(text.contains(&format!("/DW {:.0}", upem * 0.6)), "default width");

        // ToUnicode CMap: first text per gid as UTF-16BE, decoded back the
        // way a PDF text extractor reads it.
        assert!(text.contains("beginbfchar"));
        fn decode_cmap_entry(text: &str, gid_hex: &str) -> Option<String> {
            let marker = format!("<{gid_hex}> <");
            let at = text.find(&marker)? + marker.len();
            let hex: String = text[at..].chars().take_while(|c| *c != '>').collect();
            let units: Vec<u16> = (0..hex.len())
                .step_by(4)
                .filter_map(|i| u16::from_str_radix(&hex[i..i + 4], 16).ok())
                .collect();
            String::from_utf16(&units).ok()
        }
        assert_eq!(decode_cmap_entry(&text, "0044").as_deref(), Some("A"));
        assert_eq!(decode_cmap_entry(&text, "04D2").as_deref(), Some("中"));
        assert_eq!(decode_cmap_entry(&text, "0046").as_deref(), Some("B"));

        // xref integrity with the face block appended.
        let start = text.rfind("startxref").expect("marker");
        let xref_at: usize = text[start..]
            .trim_start_matches("startxref\n")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .expect("numeric startxref");
        let entries: Vec<&[u8]> = pdf[xref_at..]
            .split(|&b| b == b'\n')
            .skip(3)
            .take_while(|l| l.trim_ascii().ends_with(b"n"))
            .collect();
        assert_eq!(entries.len(), 15, "2 fixed + 3×1 page objs + 5×2 face objs");
        for (i, e) in entries.iter().enumerate() {
            let off: usize = std::str::from_utf8(&e[..10])
                .ok()
                .and_then(|s| s.parse().ok())
                .expect("10-digit offset");
            let expect = format!("{} 0 obj", i + 1);
            assert!(pdf[off..].starts_with(expect.as_bytes()), "obj {} offset", i + 1);
        }
    }

    /// The collection half of the flag: ordinary text comes back as glyph
    /// ops while a text-shadow run stays raster-only and never reaches the
    /// text layer (it would paint twice).
    #[tokio::test(flavor = "current_thread")]
    async fn collect_text_vectors_plain_and_skips_shadow() {
        use diting::diting_layout::paint::PdfOp;

        const HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0}
</style></head><body>
<div style="font-size:16px;color:#111">PLAINTEXT</div>
<div style="font-size:16px;text-shadow:2px 2px 2px #888">zzshadowzz</div>
</body></html>"#;
        let mut page = navigated(HTML, "collect.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Print,
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: true,
        };
        let set = render_page_set(&mut page, &opts).await.expect("render");
        let has_glyph = |needle: char| {
            set.text_ops.iter().flatten().any(|op| match op {
                PdfOp::Line(l) => l.glyphs.iter().any(|g| g.unicode.contains(needle)),
                _ => false,
            })
        };
        assert!(has_glyph('P'), "plain text run vectorizes");
        assert!(!has_glyph('z'), "text-shadow run stays raster");
    }

    /// Emoji glyphs come from the fallback face — the shaper can't route
    /// them to an embedded face, so an emoji-only page yields no glyph lines.
    #[tokio::test(flavor = "current_thread")]
    async fn emoji_only_page_stays_raster() {
        use diting::diting_layout::paint::PdfOp;

        const HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0}
</style></head><body><div style="font-size:32px">🚀🔥</div></body></html>"#;
        let mut page = navigated(HTML, "emoji.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Print,
            page_size: (400.0, 300.0),
            max_pages: 10,
            collect_text: true,
        };
        let set = render_page_set(&mut page, &opts).await.expect("render");
        assert!(
            set.text_ops
                .iter()
                .flatten()
                .all(|op| !matches!(op, PdfOp::Line(_))),
            "no glyph lines for fallback-face glyphs: {:?}",
            set.text_ops
        );
    }

    /// The encoders produce their format magics from an RGBA frame.
    #[test]
    fn encoders_emit_format_magics() {
        // 2×2 frame, one red pixel — enough for a valid encode.
        let rgba = [
            255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        let jpeg = jpeg_of(2, 2, &rgba, 90).expect("jpeg");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "jpeg SOI magic");
        let png = png_of(2, 2, &rgba).expect("png");
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G'], "png magic");
    }
}
