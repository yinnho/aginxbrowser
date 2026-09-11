//! Native (editable) PPTX — element-level DrawingML export.
//!
//! The image-based writer in `ooxml.rs` is pixel-faithful but frozen: one
//! JPEG per slide. This module maps the live tree to real Office shapes —
//! text becomes `<p:sp>` runs (`<a:t>`, real font size/weight/color),
//! background boxes become shapes (solid or CSS-gradient → `gradFill`),
//! `<img>` becomes `<p:pic>` with the fetched bytes as a media part. diting
//! is the layout oracle: the walker reads gBCR + computed style off the
//! live tree (the same layout cache the band paints ride), so geometry
//! comes from the engine, never a re-parse. Slides mode only — every
//! `selector` match is one slide; `format=pptx` stays the pixel-faithful
//! tier.
//!
//! v1 fidelity contract, same posture as deckhtml: position/size/exact
//! text/size/weight/color/fill are real; what a text box can't express
//! (per-glyph inline styling, z-index reordering, transforms, borders)
//! degrades by omission — the element still lands as an editable shape.
//! Text alignment is mapped, not eyeballed: `<br>` lines become paragraphs
//! (a joined re-wraps wherever the consumer's fonts land), padding becomes
//! bodyPr insets, line heights become exact points, and the text anchors
//! MIDDLE in the content box the way CSS half-leading centers glyphs.
//! The CSS `background` shorthand doesn't expand into background-image in
//! this engine yet, so gradient decks must use the `background-image`
//! longhand.

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;

use crate::diting_browser::Page;
use crate::diting_css::{parse_color, parse_linear_gradient, Color};
use crate::ooxml::{pptx_package, PPTX_SP_TREE_HEAD, A_NS, P_NS, R_NS, XML_DECL};
use crate::pages::PageError;

/// Per-image body cap, matching the other pump fetch paths.
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Walker — the JS side. Runs once, returns one record per selector match.
// ---------------------------------------------------------------------------

/// The selector rides in as a JSON-encoded string literal — it is page
/// input, never trusted JS source.
const WALKER_JS: &str = r#"function(sel) {
  const roots = Array.from(document.querySelectorAll(sel));
  const skipTags = new Set(['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','LINK','META','HEAD','TITLE']);
  return roots.map(root => {
    const rr = root.getBoundingClientRect();
    const rs = getComputedStyle(root);
    const slide = {
      w: rr.width, h: rr.height,
      bg: rs.backgroundColor, bgImage: rs.backgroundImage,
      elements: []
    };
    const walk = (el) => {
      for (const c of el.children) {
        if (skipTags.has(c.tagName)) continue;
        if (c.tagName.toLowerCase() === 'svg') continue;
        const st = getComputedStyle(c);
        if (st.display === 'none') continue;
        const r = c.getBoundingClientRect();
        if (r.width < 0.5 || r.height < 0.5) continue;
        // Direct text, split on <br>: each visual line rides as its own
        // DrawingML paragraph later. A joined single run re-wraps wherever
        // the consumer's own font metrics allow, which reads as the line
        // breaking in a different place than the deck authored.
        const lines = [[]];
        for (const n of c.childNodes) {
          if (n.nodeType === 3) lines[lines.length - 1].push(n.nodeValue);
          else if (n.nodeType === 1 && n.tagName && n.tagName.toLowerCase() === 'br') lines.push([]);
        }
        const text = lines
          .map(l => l.join(' ').replace(/\s+/g, ' ').trim())
          .filter(l => l.length > 0)
          .join('\n');
        slide.elements.push({
          tag: c.tagName,
          x: r.left - rr.left, y: r.top - rr.top, w: r.width, h: r.height,
          color: st.color, bg: st.backgroundColor, bgImage: st.backgroundImage,
          fontFamily: st.fontFamily, fontSize: st.fontSize, fontWeight: st.fontWeight,
          textAlign: st.textAlign, opacity: st.opacity,
          borderRadius: st.borderRadius, lineHeight: st.lineHeight,
          padTop: st.paddingTop, padLeft: st.paddingLeft,
          padRight: st.paddingRight, padBottom: st.paddingBottom,
          text: text || null,
          img: c.tagName === 'IMG' ? c.getAttribute('src') : null
        });
        walk(c);
      }
    };
    walk(root);
    return slide;
  });
}"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WalkedSlide {
    w: f64,
    h: f64,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    bg_image: Option<String>,
    #[serde(default)]
    elements: Vec<WalkedElement>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WalkedElement {
    tag: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    bg_image: Option<String>,
    #[serde(default)]
    font_family: Option<String>,
    #[serde(default)]
    font_size: Option<String>,
    #[serde(default)]
    font_weight: Option<String>,
    #[serde(default)]
    text_align: Option<String>,
    #[serde(default)]
    opacity: Option<String>,
    #[serde(default)]
    border_radius: Option<String>,
    #[serde(default)]
    line_height: Option<String>,
    #[serde(default)]
    pad_top: Option<String>,
    #[serde(default)]
    pad_left: Option<String>,
    #[serde(default)]
    pad_right: Option<String>,
    #[serde(default)]
    pad_bottom: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    img: Option<String>,
}

// ---------------------------------------------------------------------------
// Collect: live page → package bytes.
// ---------------------------------------------------------------------------

/// Walk every `selector` match on the live page and package it as an
/// editable PPTX. One slide per match, element-level DrawingML inside.
/// `selector` must match at least one element; more than `max_pages`
/// matches is an error (same cap semantics as the page pump). Returns the
/// package bytes and the slide count.
pub async fn pptx_native_deck(
    page: &mut Page,
    selector: &str,
    max_pages: usize,
) -> Result<(Vec<u8>, usize), PageError> {
    let lit = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let val = page.evaluate(&format!("({WALKER_JS})({lit})"));
    let slides: Vec<WalkedSlide> = serde_json::from_value(val)
        .map_err(|_| PageError::NoSelectorMatches { selector: selector.to_string() })?;
    if slides.is_empty() {
        return Err(PageError::NoSelectorMatches { selector: selector.to_string() });
    }
    if slides.len() > max_pages {
        return Err(PageError::PageCapExceeded { asked: slides.len(), cap: max_pages });
    }
    let media = resolve_media(page, &slides).await;
    Ok((pack(&slides, &media), slides.len()))
}

// ---------------------------------------------------------------------------
// Media: unique img sources → fetched/sniffed bytes.
// ---------------------------------------------------------------------------

/// Resolved media parts in first-use order plus the src→part index. Sources
/// that fail to resolve are simply absent — the element gets no `p:pic`
/// (a skipped image beats a stalled export).
struct Media {
    parts: Vec<(String, Vec<u8>)>,
    index: HashMap<String, String>,
}

async fn resolve_media(page: &Page, slides: &[WalkedSlide]) -> Media {
    // Unique srcs in first-use order (DOM order keeps the package
    // deterministic). Relative srcs resolve against the document URL.
    let base = page.url_string();
    let mut urls: Vec<String> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for s in slides {
        for e in &s.elements {
            if let Some(src) = &e.img {
                let abs = absolutize(src, &base);
                if seen.insert(abs.clone(), ()).is_none() {
                    urls.push(abs);
                }
            }
        }
    }

    let mut parts = Vec::new();
    let mut index = HashMap::new();
    for (i, u) in urls.iter().enumerate() {
        if let Some(bytes) = fetch_image(page, u).await {
            if let Some(ext) = sniff_ext(&bytes) {
                let path = format!("ppt/media/image{}.{ext}", i + 1);
                index.insert(u.clone(), path.clone());
                parts.push((path, bytes));
            }
        }
    }
    Media { parts, index }
}

/// Resolve one image source to bytes: `data:` URIs decode locally,
/// http(s) fetches go through the page's own client (SSRF gate, 3 s
/// timeout, 200-only, ≤2 MiB) with the document as Referer — the same
/// per-URL policy as the page pump's missing-image pass.
async fn fetch_image(page: &Page, url: &str) -> Option<Vec<u8>> {
    let parsed = url::Url::parse(url).ok()?;
    match parsed.scheme() {
        "data" => decode_data_uri(url),
        "http" | "https" => {
            if crate::diting_js::ops::validate_fetch_url(&parsed).is_err() {
                return None;
            }
            // `Network.setBlockedURLs` holds for render-path fetches too
            // (same hard block as the static loaders): a match keeps the
            // placeholder instead of reaching the wire.
            if page.url_blocked(url) {
                tracing::info!("Blocked pptx image by Network.setBlockedURLs: {}", url);
                return None;
            }
            let base = page.url_string();
            let fetched = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                page.context.http_client.fetch_subresource(&parsed, Some(base.as_str())),
            )
            .await;
            match fetched {
                Ok(Ok(resp)) if resp.status == 200
                    && !resp.body.is_empty()
                    && resp.body.len() <= MAX_IMAGE_BYTES =>
                {
                    Some(resp.body)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `data:[<mime>][;base64],<payload>` — only base64 payloads carry real
/// image bytes worth embedding.
fn decode_data_uri(url: &str) -> Option<Vec<u8>> {
    let comma = url.find(',')?;
    let (meta, payload) = url.split_at(comma);
    let payload = &payload[1..];
    if !meta.to_ascii_lowercase().contains(";base64") {
        return None;
    }
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.decode(payload).ok()
}

/// Magic-byte sniffing — the declared mime can lie; the bytes can't.
fn sniff_ext(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        Some("jpeg")
    } else if b.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

fn absolutize(src: &str, base: &str) -> String {
    if url::Url::parse(src).is_ok() {
        return src.to_string();
    }
    url::Url::parse(base)
        .and_then(|b| b.join(src))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| src.to_string())
}

// ---------------------------------------------------------------------------
// Mapping: walked records → DrawingML. Pure functions, DOM order = z-order.
// ---------------------------------------------------------------------------

/// CSS px → EMU (914400 per inch at 96 dpi).
fn emu(px: f32) -> u64 {
    (px.max(0.0) * (crate::ooxml::PX_TO_EMU as f32)).round() as u64
}

/// A shape fill: the three states an element's background can land in.
enum Fill {
    Solid(Color),
    Grad { stops: Vec<(f32, Color)>, css_deg: f32 },
}

/// Parse an element's background into a fill: gradient (longhand
/// `background-image`) wins, then a non-transparent `background-color`.
/// The gradient grammar itself is `diting_css::parse_linear_gradient`
/// (shared with the paint layer since the gradient-paint batch).
fn element_fill(bg: Option<&str>, bg_image: Option<&str>) -> Option<Fill> {
    if let Some(g) = bg_image.and_then(parse_linear_gradient) {
        return Some(Fill::Grad { stops: g.stops, css_deg: g.css_deg });
    }
    let c = bg.and_then(parse_color)?;
    (c.3 > 0).then_some(Fill::Solid(c))
}

/// CSS gradient angle → DrawingML `ang` (60000ths of a degree, clockwise
/// from 3 o'clock). CSS 0deg points up; the conversion is
/// `(90 - css) mod 360` — 135deg CSS → 315° → 18900000.
fn drawingml_angle(css_deg: f32) -> u64 {
    ((90.0 - css_deg).rem_euclid(360.0) * 60000.0).round() as u64
}

fn hex_of(c: Color) -> String {
    format!("{:02X}{:02X}{:02X}", c.0, c.1, c.2)
}

/// `<a:srgbClr>` with an `<a:alpha>` child when combined alpha < 1.
fn srgb_with_alpha(c: Color, alpha: f32) -> String {
    let a = (c.3 as f32 / 255.0 * alpha).clamp(0.0, 1.0);
    if a >= 0.999 {
        format!(r#"<a:srgbClr val="{}"/>"#, hex_of(c))
    } else {
        format!(
            r#"<a:srgbClr val="{}"><a:alpha val="{}"/></a:srgbClr>"#,
            hex_of(c),
            (a * 100000.0).round() as u32
        )
    }
}

fn fill_xml(fill: &Fill, opacity: f32) -> String {
    match fill {
        Fill::Solid(c) => format!("<a:solidFill>{}</a:solidFill>", srgb_with_alpha(*c, opacity)),
        Fill::Grad { stops, css_deg } => {
            let gs: String = stops
                .iter()
                .map(|(p, c)| {
                    format!(
                        r#"<a:gs pos="{}">{}</a:gs>"#,
                        (p.clamp(0.0, 1.0) * 100000.0).round() as u32,
                        srgb_with_alpha(*c, opacity)
                    )
                })
                .collect();
            format!(
                r#"<a:gradFill rotWithShape="1"><a:gsLst>{gs}</a:gsLst><a:lin ang="{}" scaled="1"/></a:gradFill>"#,
                drawingml_angle(*css_deg)
            )
        }
    }
}

// ---------------------------------------------------------------------------
// XML builders.
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Geometry: `rect`, or `roundRect` with the DrawingML `adj` (0..50000)
/// when the element carries a uniform corner radius.
fn geom_xml(border_radius: Option<&str>, w: f32, h: f32) -> String {
    let r = border_radius
        .and_then(|s| s.split_whitespace().next())
        .and_then(|tok| tok.strip_suffix("px"))
        .and_then(|n| n.parse::<f32>().ok())
        .unwrap_or(0.0);
    if r > 0.0 && w > 0.0 && h > 0.0 {
        let adj = (r / w.min(h) * 100000.0).round().clamp(0.0, 50000.0) as u32;
        format!(
            r#"<a:prstGeom prst="roundRect"><a:avLst><a:gd name="adj" fmla="val {adj}"/></a:avLst></a:prstGeom>"#
        )
    } else {
        r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>"#.to_string()
    }
}

fn xfrm(x: f32, y: f32, w: f32, h: f32) -> String {
    format!(
        r#"<a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm>"#,
        emu(x),
        emu(y),
        emu(w),
        emu(h)
    )
}

/// A background-only shape: fill, no text body. The rect is (x, y, w, h).
fn plain_shape(
    id: u64,
    name: &str,
    rect: (f32, f32, f32, f32),
    fill: &Fill,
    opacity: f32,
    radius: Option<&str>,
) -> String {
    let (x, y, w, h) = rect;
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}{}{}<a:ln><a:noFill/></a:ln></p:spPr></p:sp>"#,
        xml_escape(name),
        xfrm(x, y, w, h),
        geom_xml(radius, w, h),
        fill_xml(fill, opacity),
    )
}

/// A text-bearing shape: optional fill + a text body whose runs carry the
/// element's font size (hundredths of a point), weight, family, and color.
/// Vertical alignment is the part CSS hands us for free and DrawingML
/// doesn't: half-leading centers the glyphs in each line box, so the shape
/// anchors its text MIDDLE in the content box, padding maps to insets, and
/// line heights convert to exact points — the DrawingML defaults (top
/// anchor, "single" spacing ≈ 1.2 em) sit visibly high and drift per line.
fn text_shape(id: u64, e: &WalkedElement, fill: Option<&Fill>) -> String {
    let opacity: f32 = e.opacity.as_deref().and_then(|o| o.parse().ok()).unwrap_or(1.0);

    let font_px = e
        .font_size
        .as_deref()
        .and_then(|s| s.trim_end_matches("px").trim().parse::<f32>().ok())
        .unwrap_or(16.0);
    let sz = font_px * 75.0; // px → pt in hundredths
    let bold = e
        .font_weight
        .as_deref()
        .map(|w| {
            w.trim().eq_ignore_ascii_case("bold")
                || w.trim().parse::<u16>().map(|n| n >= 600).unwrap_or(false)
        })
        .unwrap_or(false);
    let algn = match e.text_align.as_deref() {
        Some("center") => "ctr",
        Some("right") | Some("end") => "r",
        _ => "l",
    };

    // Line spacing: unitless and px lengths both become EXACT points
    // (spcPts). Percentages (spcPct) are relative to the renderer's own
    // single-line height (~1.2 em in Office) — CSS 1.35 mapped to 135%
    // renders ~1.62 em and every line lands further down the box.
    // "normal" (and anything unparseable) inherits the shape default.
    let mut ln_spc = String::new();
    if let Some(lh) = e.line_height.as_deref() {
        let lh = lh.trim();
        if lh != "normal" {
            if let Ok(n) = lh.parse::<f32>() {
                ln_spc = format!(
                    r#"<a:lnSpc><a:spcPts val="{}"/></a:lnSpc>"#,
                    (n * font_px * 75.0).round() as u32
                );
            } else if let Some(px) = lh.strip_suffix("px").and_then(|p| p.trim().parse::<f32>().ok()) {
                ln_spc = format!(r#"<a:lnSpc><a:spcPts val="{}"/></a:lnSpc>"#, (px * 75.0).round() as u32);
            }
        }
    }

    // Both scripts of a mixed zh/en deck carry the same family; see
    // `portable_family`.
    let typeface = e
        .font_family
        .as_deref()
        .and_then(portable_family)
        .map(|f| {
            format!(
                r#"<a:latin typeface="{}"/><a:ea typeface="{}"/>"#,
                xml_escape(&f),
                xml_escape(&f)
            )
        })
        .unwrap_or_default();

    // Run color: the computed color, alpha-composited with opacity.
    let color_xml = e
        .color
        .as_deref()
        .and_then(parse_color)
        .map(|c| format!("<a:solidFill>{}</a:solidFill>", srgb_with_alpha(c, opacity)))
        .unwrap_or_default();

    let pad = |v: Option<&str>| {
        v.and_then(|s| s.trim_end_matches("px").trim().parse::<f32>().ok())
            .unwrap_or(0.0)
    };
    let (pt, pl, pr, pb) = (
        pad(e.pad_top.as_deref()),
        pad(e.pad_left.as_deref()),
        pad(e.pad_right.as_deref()),
        pad(e.pad_bottom.as_deref()),
    );

    // <br>-split lines ride as one DrawingML paragraph each, so the deck's
    // authored breaks survive instead of re-wrapping wherever the consumer's
    // own font metrics land.
    let paragraphs: Vec<&str> = e
        .text
        .as_deref()
        .map(|t| t.split('\n').filter(|l| !l.trim().is_empty()).collect())
        .unwrap_or_default();

    // A box only one line tall turns wrapping off: its width is the
    // fit-content width under diting's fonts, and a substituted face in
    // the consumer is wider — wrapping would fold the line. Taller boxes
    // keep wrapping (they hold more than one line by construction).
    let single_line = paragraphs.len() <= 1 && (e.h as f32 - pt - pb) < 2.2 * font_px;
    let wrap = if single_line { "none" } else { "square" };

    let bold_attr = if bold { r#" b="1""# } else { "" };

    let paras: String = paragraphs
        .iter()
        .map(|line| {
            format!(
                r#"<a:p><a:pPr algn="{algn}">{ln_spc}</a:pPr><a:r><a:rPr lang="en-US" sz="{sz}" dirty="0"{bold_attr}>{color_xml}{typeface}</a:rPr><a:t>{}</a:t></a:r></a:p>"#,
                xml_escape(line),
            )
        })
        .collect();

    let fill_xml_str = fill.map(|f| fill_xml(f, opacity)).unwrap_or_else(|| "<a:noFill/>".to_string());

    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr>{}{}{}<a:ln><a:noFill/></a:ln></p:spPr><p:txBody><a:bodyPr lIns="{}" tIns="{}" rIns="{}" bIns="{}" wrap="{wrap}" anchor="ctr"/><a:lstStyle/>{paras}</p:txBody></p:sp>"#,
        xml_escape(&e.tag),
        xfrm(e.x as f32, e.y as f32, e.w as f32, e.h as f32),
        geom_xml(e.border_radius.as_deref(), e.w as f32, e.h as f32),
        fill_xml_str,
        emu(pl),
        emu(pt),
        emu(pr),
        emu(pb),
    )
}

/// The typeface to declare for both the latin and east-asian runs. Stacks
/// open with vendor aliases ("-apple-system", "BlinkMacSystemFont",
/// "system-ui") and CSS generics that resolve to nothing outside the
/// authoring OS — the consumer substitutes its default and the metric
/// drift re-wraps text that fit its box. Skip those, then prefer a
/// CJK-capable family the author listed (Office and WPS both resolve
/// "Microsoft YaHei" on Windows and macOS; one CJK face covers the mixed
/// zh/en runs this exporter emits); otherwise the first real family.
fn portable_family(stack: &str) -> Option<String> {
    const SKIPPED: [&str; 7] = [
        "-apple-system", "BlinkMacSystemFont", "system-ui", "sans-serif", "serif", "monospace", "cursive",
    ];
    let families: Vec<&str> = stack
        .split(',')
        .map(|f| f.trim().trim_matches(|c| c == '"' || c == '\'').trim())
        .filter(|f| !f.is_empty() && !SKIPPED.iter().any(|s| s.eq_ignore_ascii_case(f)))
        .collect();
    const CJK: [&str; 6] = [
        "Microsoft YaHei", "PingFang SC", "Hiragino Sans GB", "Noto Sans SC", "Source Han Sans SC", "SimHei",
    ];
    CJK.iter()
        .find(|cjk| families.iter().any(|f| f.eq_ignore_ascii_case(cjk)))
        .or_else(|| families.first())
        .map(|f| f.to_string())
}

/// An image shape with its own relationship id (slide-local, images only).
fn pic_xml(id: u64, rid: usize, e: &WalkedElement) -> String {
    format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="{id}" name="{}"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId{rid}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
        xml_escape(&e.tag),
        xfrm(e.x as f32, e.y as f32, e.w as f32, e.h as f32),
    )
}

/// Map one walked slide to (slide XML, slide .rels XML).
fn map_slide(s: &WalkedSlide, media: &Media) -> (String, String) {
    let mut shapes = String::new();
    let mut rels = String::new();
    let mut rid = 0usize;
    let mut id = 1u64; // 1 is the spTree group; shapes start at 2

    // The slide root's own background leads, so it paints behind everything.
    if let Some(fill) = element_fill(s.bg.as_deref(), s.bg_image.as_deref()) {
        id += 1;
        shapes.push_str(&plain_shape(
            id,
            "Slide Background",
            (0.0, 0.0, s.w as f32, s.h as f32),
            &fill,
            1.0,
            None,
        ));
    }

    for e in &s.elements {
        // Off-slide boxes never show; skip instead of exporting invisible
        // shapes (elements can hang past the root via negative margins).
        if e.x + e.w <= 0.0 || e.y + e.h <= 0.0 || e.x >= s.w || e.y >= s.h {
            continue;
        }
        if let Some(src) = &e.img {
            if let Some(path) = media.index.get(src) {
                rid += 1;
                id += 1;
                shapes.push_str(&pic_xml(id, rid, e));
                let file = path.rsplit('/').next().unwrap_or(path.as_str());
                rels.push_str(&format!(
                    r#"<Relationship Id="rId{rid}" Type="{R_NS}/image" Target="../media/{file}"/>"#
                ));
            }
            continue;
        }
        let has_text = e.text.as_deref().is_some_and(|t| !t.is_empty());
        let fill = element_fill(e.bg.as_deref(), e.bg_image.as_deref());
        if !has_text && fill.is_none() {
            continue; // no-op container: nothing visual of its own
        }
        id += 1;
        if has_text {
            shapes.push_str(&text_shape(id, e, fill.as_ref()));
        } else {
            shapes.push_str(&plain_shape(
                id,
                &e.tag,
                (e.x as f32, e.y as f32, e.w as f32, e.h as f32),
                fill.as_ref().expect("checked above"),
                e.opacity.as_deref().and_then(|o| o.parse().ok()).unwrap_or(1.0),
                e.border_radius.as_deref(),
            ));
        }
    }

    let slide = format!(
        r#"{XML_DECL}<p:sld xmlns:a="{A_NS}" xmlns:r="{R_NS}" xmlns:p="{P_NS}"><p:cSld>{PPTX_SP_TREE_HEAD}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
    );
    let rels_xml = format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    (slide, rels_xml)
}

/// Pure packaging: walked slides + resolved media → PPTX bytes. The deck
/// carries one slide size (max width × max height); smaller slides anchor
/// at the top-left, same posture as the image-based writer.
fn pack(slides: &[WalkedSlide], media: &Media) -> Vec<u8> {
    let deck_w = slides.iter().map(|s| s.w as f32).fold(0.0_f32, f32::max).max(1.0);
    let deck_h = slides.iter().map(|s| s.h as f32).fold(0.0_f32, f32::max).max(1.0);
    let out: Vec<(String, String)> = slides.iter().map(|s| map_slide(s, media)).collect();
    let mut exts: BTreeSet<&str> = BTreeSet::new();
    for (path, _) in &media.parts {
        if let Some(ext) = path.rsplit('.').next() {
            exts.insert(ext);
        }
    }
    let exts: Vec<&str> = exts.into_iter().collect();
    pptx_package(&out, &media.parts, emu(deck_w), emu(deck_h), &exts)
}

#[cfg(all(test, feature = "screenshot"))]
mod tests {
    use super::*;
    use crate::diting_browser::lifecycle::WaitUntil;
    use crate::diting_browser::{BrowserContext, Page as EnginePage};
    use crate::ooxml::entries_of;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    // --- gradient angle export --------------------------------------------
    // (The gradient grammar itself is tested in diting_css next to
    // parse_linear_gradient; only the DrawingML conversion is ours.)

    #[test]
    fn drawingml_angle_converts_css_degrees() {
        // CSS 0deg points up; DrawingML 0° points right (3 o'clock):
        // (90 - css) mod 360, in 60000ths of a degree.
        assert_eq!(drawingml_angle(135.0), 18900000, "135deg CSS → 315° DrawingML");
        assert_eq!(drawingml_angle(90.0), 0, "to right → 0° (pointing right)");
    }

    // --- pure mapping ------------------------------------------------------

    fn el(tag: &str, x: f64, y: f64, w: f64, h: f64) -> WalkedElement {
        WalkedElement {
            tag: tag.to_string(),
            x, y, w, h,
            color: None, bg: None, bg_image: None, font_family: None,
            font_size: None, font_weight: None, text_align: None, opacity: None,
            border_radius: None, line_height: None,
            pad_top: None, pad_left: None, pad_right: None, pad_bottom: None,
            text: None, img: None,
        }
    }

    fn slide(w: f64, h: f64, elements: Vec<WalkedElement>) -> WalkedSlide {
        WalkedSlide { w, h, bg: None, bg_image: None, elements }
    }

    #[test]
    fn text_element_becomes_native_run() {
        let mut e = el("H1", 40.0, 30.0, 600.0, 80.0);
        e.text = Some("Hello deck".to_string());
        e.font_size = Some("32px".to_string());
        e.font_weight = Some("700".to_string());
        e.text_align = Some("center".to_string());
        e.font_family = Some("Arial, sans-serif".to_string());
        e.color = Some("rgb(34, 34, 34)".to_string());
        let pptx = pack(&[slide(1280.0, 720.0, vec![e])], &Media { parts: vec![], index: HashMap::new() });
        let entries = entries_of(&pptx);
        let slide_xml = entries
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains("<a:t>Hello deck</a:t>"), "real text run, not an image");
        assert!(slide_xml.contains("sz=\"2400\""), "32px → 24pt → sz=2400");
        assert!(slide_xml.contains("b=\"1\""), "weight 700 → bold");
        assert!(slide_xml.contains("algn=\"ctr\""));
        assert!(slide_xml.contains(r#"typeface="Arial""#));
        assert!(slide_xml.contains(r#"<a:srgbClr val="222222"/>"#));
        assert!(slide_xml.contains("lIns=\"0\""), "zero insets so CSS geometry holds");
        // No media parts at all — nothing image-based in the package.
        assert!(entries.iter().all(|(n, _)| !n.contains("/media/")), "no image parts");
    }

    #[test]
    fn text_alignment_contract_maps_css_to_drawingml() {
        // The WPS misalignment family, one shape each: <br> lines →
        // paragraphs, unitless line-height → exact points, vendor font
        // aliases → a portable family, single-line box → no re-wrap.
        let mut e = el("DIV", 0.0, 0.0, 900.0, 194.4);
        e.text = Some("用国产模型跑\nClaude Code / Codex".to_string());
        e.font_size = Some("72px".to_string());
        e.line_height = Some("1.35".to_string());
        e.font_family = Some(
            "-apple-system, BlinkMacSystemFont, \"PingFang SC\", \"Microsoft YaHei\", sans-serif".to_string(),
        );
        let pptx = pack(&[slide(1280.0, 720.0, vec![e])], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        // Two lines, two real paragraphs — not one run left to re-wrap.
        assert_eq!(slide_xml.matches("<a:p>").count(), 2, "one paragraph per <br> line");
        assert!(slide_xml.contains("<a:t>用国产模型跑</a:t>"));
        assert!(slide_xml.contains("<a:t>Claude Code / Codex</a:t>"));
        // 1.35 × 72px = 97.2px = 72.9pt → spcPts 7290, not spcPct 135000.
        assert!(slide_xml.contains(r#"<a:spcPts val="7290"/>"#));
        assert!(!slide_xml.contains("spcPct"), "percent line spacing drifts in Office");
        // Vendor aliases skipped, the CJK family the author listed pins for
        // both scripts.
        assert!(slide_xml.contains(r#"<a:latin typeface="Microsoft YaHei"/>"#));
        assert!(slide_xml.contains(r#"<a:ea typeface="Microsoft YaHei"/>"#));
        assert!(!slide_xml.contains("-apple-system"), "vendor alias never exported");
        // Two lines tall → keeps wrapping.
        assert!(slide_xml.contains(r#"wrap="square""#), "multi-line box wraps");

        // Same element one line tall: wrapping off — the box width is the
        // fit-content width under diting's fonts and a substituted face in
        // WPS is wider, which would fold the line.
        let mut single = el("H1", 0.0, 0.0, 600.0, 97.2);
        single.text = Some("Agent 的 AI 大脑".to_string());
        single.font_size = Some("72px".to_string());
        let pptx = pack(&[slide(1280.0, 720.0, vec![single])], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains(r#"wrap="none""#), "single-line box does not re-wrap");
        assert!(slide_xml.contains(r#"anchor="ctr""#), "text centers like CSS half-leading");
    }

    #[test]
    fn padding_maps_to_bodypr_insets() {
        // A chip: 10px 26px of padding around its text. CSS draws the
        // glyphs inside the content box; insets carry that into DrawingML.
        let mut e = el("SPAN", 0.0, 0.0, 152.0, 44.0);
        e.text = Some("brain.aginx.net".to_string());
        e.font_size = Some("24px".to_string());
        e.pad_top = Some("10px".to_string());
        e.pad_left = Some("26px".to_string());
        e.pad_right = Some("26px".to_string());
        e.pad_bottom = Some("10px".to_string());
        let pptx = pack(&[slide(400.0, 300.0, vec![e])], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains(r#"tIns="95250""#), "10px top padding");
        assert!(slide_xml.contains(r#"lIns="247650""#), "26px left padding");
        assert!(slide_xml.contains(r#"rIns="247650""#), "26px right padding");
        assert!(slide_xml.contains(r#"bIns="95250""#), "10px bottom padding");
    }

    #[test]
    fn gradient_background_becomes_gradfill() {
        let mut e = el("DIV", 0.0, 0.0, 1280.0, 720.0);
        e.bg_image = Some("linear-gradient(135deg, #2c3e50 0%, #fd79a8 100%)".to_string());
        let s = WalkedSlide { w: 1280.0, h: 720.0, bg: None, bg_image: None, elements: vec![e] };
        let pptx = pack(&[s], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains("gradFill"));
        assert!(slide_xml.contains(r#"ang="18900000""#), "135deg CSS → DrawingML angle");
        assert!(slide_xml.contains(r#"<a:gs pos="0">"#));
        assert!(slide_xml.contains(r#"<a:gs pos="100000">"#));
    }

    #[test]
    fn radius_becomes_roundrect_and_root_bg_leads() {
        let mut e = el("DIV", 10.0, 10.0, 100.0, 50.0);
        e.bg = Some("rgb(39, 174, 96)".to_string());
        e.border_radius = Some("12px".to_string());
        let s = WalkedSlide {
            w: 400.0,
            h: 300.0,
            bg: Some("rgb(192, 57, 43)".to_string()),
            bg_image: None,
            elements: vec![e],
        };
        let pptx = pack(&[s], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains(r#"prst="roundRect""#));
        // adj = 12/min(100,50)=0.24 → 24000.
        assert!(slide_xml.contains(r#"fmla="val 24000""#));
        // The root background shape precedes the element shape.
        let bg_at = slide_xml.find("Slide Background").expect("root bg shape");
        let el_at = slide_xml.find(r#"val="27AE60""#).expect("element bg");
        assert!(bg_at < el_at, "root bg paints first");
        // Deck size in EMU.
        let pres = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/presentation.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("presentation");
        assert!(pres.contains(&format!(r#"cx="{}""#, 400 * 9525)));
    }

    #[test]
    fn opacity_applies_alpha_to_fills() {
        let mut e = el("DIV", 0.0, 0.0, 100.0, 100.0);
        e.bg = Some("rgb(255, 0, 0)".to_string());
        e.opacity = Some("0.5".to_string());
        let pptx = pack(&[slide(200.0, 200.0, vec![e])], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert!(slide_xml.contains(r#"<a:alpha val="50000"/>"#));
    }

    #[test]
    fn noop_containers_are_skipped() {
        let e = el("DIV", 0.0, 0.0, 100.0, 100.0); // no text, no bg
        let s = slide(200.0, 200.0, vec![e]);
        let pptx = pack(&[s], &Media { parts: vec![], index: HashMap::new() });
        let slide_xml = entries_of(&pptx)
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        assert_eq!(slide_xml.matches("<p:sp>").count(), 0);
    }

    // --- integration: live page → package ---------------------------------

    const DECK_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0}
.slide{width:400px;height:300px;background:#202030;margin:0 auto}
.slide h1{font-family:Arial,sans-serif;font-size:32px;font-weight:700;color:#f0f0f0;text-align:center}
.card{width:120px;height:80px;background-color:#27ae60;border-radius:12px}
.chip{font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei",sans-serif;font-size:24px;line-height:1.35;padding:10px 26px;color:#ffffff;background-color:#574b90}
</style></head><body>
<div class="slide">
  <h1>Hello native</h1>
  <div class="card"></div>
  <div class="chip">每年省下<br>几千块</div>
  <img src="IMG_PLACEHOLDER" width="60" height="60">
</div>
</body></html>"#;

    fn spawn_html_server(html: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = Read::read(&mut stream, &mut buf);
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
            "pptx-native-test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: the fixture server is 127.0.0.1
            None,
        ));
        EnginePage::new("pptx-native-page".into(), context)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deck_from_live_page() {
        // A real 1x1 PNG as a data: URI so the image path runs end-to-end.
        let png = crate::pages::png_of(1, 1, &[255, 0, 0, 255]).expect("png encode");
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let html: &'static str = Box::leak(
            DECK_HTML.replace("IMG_PLACEHOLDER", &format!("data:image/png;base64,{}", STANDARD.encode(&png)))
                .into_boxed_str(),
        );
        let port = spawn_html_server(html);
        let mut page = test_page();
        page.navigate_with_wait(&format!("http://127.0.0.1:{port}/deck.html"), WaitUntil::Load)
            .await
            .expect("navigate fixture");
        page.settle_until_idle(5000).await;

        let (pptx, _) = pptx_native_deck(&mut page, ".slide", 10).await.expect("deck");
        let entries = entries_of(&pptx);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"ppt/slides/slide1.xml"));
        assert!(names.contains(&"ppt/slides/_rels/slide1.xml.rels"));

        let slide_xml = entries
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide1");
        // Real text run with engine-measured styling.
        assert!(slide_xml.contains("<a:t>Hello native</a:t>"));
        assert!(slide_xml.contains("sz=\"2400\""), "32px → 24pt");
        assert!(slide_xml.contains("b=\"1\""), "weight 700 → bold");
        assert!(slide_xml.contains("algn=\"ctr\""), "center → algn ctr");
        assert!(slide_xml.contains(r#"typeface="Arial""#), "font-family from the engine surface");
        assert!(slide_xml.contains(r#"anchor="ctr""#), "text centers like CSS half-leading");
        // The chip: <br> → two paragraphs, portable family for both scripts,
        // exact line spacing, padding → insets — the full alignment contract
        // read off the live engine.
        assert!(slide_xml.contains(r#"typeface="Microsoft YaHei""#));
        assert!(slide_xml.contains(r#"<a:ea typeface="Microsoft YaHei"/>"#));
        assert!(slide_xml.contains(r#"<a:spcPts val="2430"/>"#), "1.35 × 24px = 32.4px = 24.3pt");
        assert!(slide_xml.contains(r#"tIns="95250""#), "10px top padding");
        assert!(slide_xml.contains(r#"lIns="247650""#), "26px left padding");
        assert!(slide_xml.contains("<a:t>每年省下</a:t>"));
        assert!(slide_xml.contains("<a:t>几千块</a:t>"));
        assert_eq!(
            slide_xml.matches("<a:p>").count(),
            3,
            "h1 + two chip paragraphs"
        );
        // The card: rounded green shape.
        assert!(slide_xml.contains(r#"prst="roundRect""#));
        assert!(slide_xml.contains(r#"val="27AE60""#));
        // The image: a real media part, referenced from the slide rels.
        let media = names.iter().find(|n| n.starts_with("ppt/media/")).copied();
        let media = media.expect("a media part exists");
        assert!(media.ends_with(".png"));
        assert_eq!(
            entries.iter().find(|(n, _)| n == media).map(|(_, d)| d.len()),
            Some(png.len()),
            "media bytes verbatim"
        );
        let rels = entries
            .iter()
            .find(|(n, _)| n == "ppt/slides/_rels/slide1.xml.rels")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("slide rels");
        let file = media.rsplit('/').next().unwrap();
        assert!(rels.contains(&format!("Target=\"../media/{file}\"")));
        // Determinism.
        let (again, _) = pptx_native_deck(&mut page, ".slide", 10).await.expect("deck again");
        assert_eq!(pptx, again, "byte-identical rebuild");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn selector_must_match_and_cap_holds() {
        let port = spawn_html_server(DECK_HTML.replace("IMG_PLACEHOLDER", "x").leak());
        let mut page = test_page();
        page.navigate_with_wait(&format!("http://127.0.0.1:{port}/deck.html"), WaitUntil::Load)
            .await
            .expect("navigate fixture");
        page.settle_until_idle(5000).await;
        assert!(matches!(
            pptx_native_deck(&mut page, ".nope", 10).await,
            Err(PageError::NoSelectorMatches { .. })
        ));
        assert!(matches!(
            pptx_native_deck(&mut page, ".slide", 0).await,
            Err(PageError::PageCapExceeded { .. })
        ));
    }
}
