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

use crate::diting_browser::Page;

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
}

impl Default for PagePumpOptions {
    fn default() -> Self {
        Self {
            mode: PageMode::Print,
            page_size: (794.0, 1123.0),
            max_pages: 50,
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

/// A page break's start offset and height.
struct Band {
    y: f32,
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
        PageMode::Print => print_bands(page, page_h)?,
        PageMode::Slides(selector) => slide_bands(page, selector)?,
    };
    if bands.len() > opts.max_pages {
        return Err(PageError::PageCapExceeded { asked: bands.len(), cap: opts.max_pages });
    }

    let mut pages = Vec::with_capacity(bands.len());
    for band in bands {
        // Missing images that surface per-page (fetch failures stay missing;
        // a painted placeholder beats a stall).
        let (_, missing) = paint(page, 0.0, band.y, (w, band.vh))
            .ok_or(PageError::NoLiveDocument)?;
        if !missing.is_empty() {
            page.fetch_band_images(missing).await;
        }
        let (frame, _) = paint(page, 0.0, band.y, (w, band.vh)).ok_or(PageError::NoLiveDocument)?;
        pages.push(PageImage {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
            origin_y: band.y,
        });
    }
    Ok(PageSet { pages, content_size })
}

/// Print pagination: greedy breaks at top-level block bottoms. For each
/// page, the candidate break is the largest `body`-child bottom inside
/// `(y + 0.55·page_h, y + page_h]` — blocks taller than a page guillotine at
/// the nominal boundary (v1 accepts cutting nested content inside an
/// oversized top-level block).
fn print_bands(page: &mut Page, page_h: f32) -> Result<Vec<Band>, PageError> {
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
    // block bottom is.
    let content_h = if sh.is_finite() && sh > 0.0 && sh > ih {
        sh as f32
    } else {
        bottoms.last().copied().unwrap_or(sh as f32)
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
            bands.push(Band { y, vh: remaining.max(1.0) });
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
        bands.push(Band { y, vh: (next - y).max(1.0) });
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
/// own origin and height.
fn slide_bands(page: &mut Page, selector: &str) -> Result<Vec<Band>, PageError> {
    // The selector rides in as a JSON-encoded string literal — it is page
    // input, never trusted JS source.
    let lit = serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string());
    let rects_json = page.evaluate(&format!(
        "(() => {{ return Array.from(document.querySelectorAll({lit})) \
         .map(e => {{ const r = e.getBoundingClientRect(); return [r.y, r.height]; }}); }})()"
    ));
    let rects: Vec<(f32, f32)> = rects_json
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let pair = v.as_array()?;
                    let y = pair.first()?.as_f64()? as f32;
                    let h = pair.get(1)?.as_f64()? as f32;
                    Some((y, h))
                })
                .collect()
        })
        .unwrap_or_default();
    if rects.is_empty() {
        return Err(PageError::NoSelectorMatches { selector: selector.to_string() });
    }
    Ok(rects
        .into_iter()
        .filter(|(_, h)| *h >= 1.0)
        .map(|(y, h)| Band { y, vh: h })
        .collect())
}

/// One band paint, the shared primitive.
fn paint(
    page: &Page,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
) -> Option<(crate::diting_js::ops::BandFrame, Vec<String>)> {
    page.viewport_band_frame(scroll_x, scroll_y, viewport)
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

/// Pack per-page JPEGs as an image-based PDF 1.4: one page object, one
/// DCTDecode XObject (`/Filter /DCTDecode` embeds the JPEG stream verbatim —
/// no re-encode), and one `q W 0 0 H 0 0 cm /Im0 Do Q` content stream per
/// page. MediaBoxes are in points; every page carries its own box, so
/// variable-height print bands need no normalization. Hand-rolled writer —
/// zero new dependencies.
pub fn pdf_of_pages(pages: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // The binary marker comment line flags the file as containing binary
    // streams (the JPEG data), so transport that peeks at the head doesn't
    // treat the file as text.
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    // offsets[i] is the byte offset of object (i + 1).
    let mut offsets: Vec<usize> = Vec::with_capacity(2 + 3 * pages.len());

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

    for (i, &(w, h, jpeg)) in pages.iter().enumerate() {
        let wpt = w as f64 * PX_TO_PT;
        let hpt = h as f64 * PX_TO_PT;
        let page_num = 3 + 3 * i;
        let img_num = page_num + 1;
        let content_num = page_num + 2;

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{page_num} 0 obj\n<< /Type /Page /Parent 2 0 R \
                 /MediaBox [0 0 {wpt:.2} {hpt:.2}] \
                 /Resources << /XObject << /Im0 {img_num} 0 R >> >> \
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

        let content = format!("q {wpt:.2} 0 0 {hpt:.2} 0 0 cm /Im0 Do Q\n");
        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{content_num} 0 obj\n<< /Length {} >>\nstream\n{content}endstream\nendobj\n",
                content.len()
            )
            .as_bytes(),
        );
    }

    let xref_pos = out.len();
    let total_objs = 2 + 3 * pages.len();
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
    use crate::diting_browser::lifecycle::WaitUntil;
    use crate::diting_browser::{BrowserContext, Page as EnginePage};
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
    fn has_ink(frame: &crate::diting_js::ops::BandFrame) -> bool {
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
    fn p_rgba_frame(p: &PageImage) -> crate::diting_js::ops::BandFrame {
        crate::diting_js::ops::BandFrame {
            rgba: p.rgba.clone(),
            width: p.width,
            height: p.height,
            dx: 0.0,
            dy: 0.0,
            content_size: (p.width as f32, p.height as f32),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slides_one_page_per_match() {
        let mut page = navigated(SLIDES_HTML, "slides.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".slide".to_string()),
            page_size: (400.0, 300.0),
            max_pages: 10,
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

    #[tokio::test(flavor = "current_thread")]
    async fn selector_without_matches_errors() {
        let mut page = navigated(PRINT_HTML, "nomatch.html").await;
        let opts = PagePumpOptions {
            mode: PageMode::Slides(".missing".to_string()),
            page_size: (400.0, 300.0),
            max_pages: 10,
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
        };
        let err = render_page_set(&mut page, &opts).await.unwrap_err();
        assert!(matches!(err, PageError::PageCapExceeded { asked: 4, cap: 2 }), "{err}");
    }

    /// The PDF writer's shape: header with binary marker, per-page objects,
    /// a well-formed xref + trailer, and verbatim JPEG bytes inside streams.
    #[test]
    fn pdf_packaging_shape() {
        // Minimal real JPEGs (SOI + EOI is enough for the writer — it never
        // parses the stream).
        let j1 = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 0xFF, 0xD9];
        let j2 = vec![0xFF, 0xD8, 9, 9, 0xFF, 0xD9];
        let pdf = pdf_of_pages(&[(794, 1123, &j1), (794, 200, &j2)]);
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
