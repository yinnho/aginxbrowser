//! OOXML containers — the packaging layer above the page pump.
//!
//! Two image-based writers, PPTX (one page image per slide) and DOCX (one
//! page image per page-sized section), built on a hand-rolled stored-entry
//! ZIP writer — same posture as the PDF writer in `pages.rs`: zero new
//! dependencies, page JPEGs ride in as-is. Fixed DOS timestamps (the ZIP
//! epoch, 1980-01-01) keep packages byte-deterministic: same page set in,
//! same container bytes out.
//!
//! Variable page heights (print's short last band, slides' per-element
//! sizes) land differently in the two formats: a PPTX deck carries one
//! slide size, so it takes the max page height and anchors each image at
//! the top-left; a DOCX can size every section independently, so each page
//! keeps its own height exactly.

// ---------------------------------------------------------------------------
// ZIP writer — stored entries only.
// ---------------------------------------------------------------------------

/// IEEE CRC-32 (the ZIP check polynomial), reflected form.
fn crc32(data: &[u8]) -> u32 {
    fn table() -> [u32; 256] {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    }
    static TABLE: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(table);
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// DOS date/time for 1980-01-01 00:00 — the ZIP epoch. Fixed (never the
/// wall clock) so identical page sets package to identical bytes.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x0021;

struct ZipEntry {
    name: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
}

/// A minimal ZIP writer emitting stored (uncompressed) entries. The page
/// images are already-compressed JPEG payloads and the XML parts are tiny,
/// so deflate would buy nothing for the dependency it costs.
struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<ZipEntry>,
}

impl ZipWriter {
    fn new() -> Self {
        Self { out: Vec::new(), entries: Vec::new() }
    }

    fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        self.out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed (2.0)
        self.out.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: UTF-8 names
        self.out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        self.out.extend_from_slice(&DOS_TIME.to_le_bytes());
        self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&size.to_le_bytes()); // compressed
        self.out.extend_from_slice(&size.to_le_bytes()); // uncompressed
        self.out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(data);
        self.entries.push(ZipEntry { name: name.as_bytes().to_vec(), crc, size, offset });
    }

    fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u32;
        for e in &self.entries {
            self.out.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central directory
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.out.extend_from_slice(&0x0800u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            self.out.extend_from_slice(&DOS_TIME.to_le_bytes());
            self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
            self.out.extend_from_slice(&e.crc.to_le_bytes());
            self.out.extend_from_slice(&e.size.to_le_bytes());
            self.out.extend_from_slice(&e.size.to_le_bytes());
            self.out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment len
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk start
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.out.extend_from_slice(&e.offset.to_le_bytes());
            self.out.extend_from_slice(&e.name);
        }
        let cd_size = self.out.len() as u32 - cd_offset;
        let count = self.entries.len() as u16;
        self.out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of central dir
        self.out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        self.out.extend_from_slice(&count.to_le_bytes()); // entries this disk
        self.out.extend_from_slice(&count.to_le_bytes()); // total entries
        self.out.extend_from_slice(&cd_size.to_le_bytes());
        self.out.extend_from_slice(&cd_offset.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        self.out
    }
}

// ---------------------------------------------------------------------------
// PPTX
// ---------------------------------------------------------------------------

/// CSS px → EMU (English Metric Units; 914400 per inch at 96 dpi).
const PX_TO_EMU: u64 = 914_400 / 96;

/// CSS px → twips (1440 per inch at 96 dpi) — the DOCX page unit.
const PX_TO_TWIP: u64 = 1440 / 96;

const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PIC_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
const WP_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

const PPTX_THEME: &str = r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="aginxbrowser"><a:themeElements><a:clrScheme name="aginxbrowser"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="aginxbrowser"><a:majorFont><a:latin typeface=""/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface=""/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="aginxbrowser"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;

const PPTX_MASTER: &str = r#"<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#;

const PPTX_LAYOUT: &str = r#"<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;

/// Empty spTree skeleton shared by slides (the pic is appended inside).
const PPTX_SP_TREE_HEAD: &str = r#"<p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#;

/// Pack per-page JPEGs as an image-based PPTX: one slide per page, the
/// page image anchored at the slide's top-left at its own size. The deck
/// carries a single slide size — max page width × max page height — because
/// presentation.xml has exactly one `sldSz`.
pub fn pptx_of_pages(pages: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let max_w = pages.iter().map(|&(w, _, _)| w).max().unwrap_or(794);
    let max_h = pages.iter().map(|&(_, h, _)| h).max().unwrap_or(1123);

    let mut content_types = format!(
        r#"{XML_DECL}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpeg" ContentType="image/jpeg"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>"#
    );
    let mut sld_ids = String::new();
    let mut pres_rels = format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#
    );
    for (i, &(_, _, _)) in pages.iter().enumerate() {
        let n = i + 1;
        let rid = n + 1; // rId1 is the master; slides start at rId2
        content_types.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
        sld_ids.push_str(&format!(r#"<p:sldId id="{}" r:id="rId{rid}"/>"#, 256 + i));
        pres_rels.push_str(&format!(
            r#"<Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{n}.xml"/>"#
        ));
    }
    content_types.push_str(
        r#"<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/></Types>"#,
    );
    pres_rels.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
        pages.len() + 2
    ));

    let presentation = format!(
        r#"{XML_DECL}<p:presentation xmlns:a="{A_NS}" xmlns:r="{R_NS}" xmlns:p="{P_NS}"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>{sld_ids}</p:sldIdLst><p:sldSz cx="{}" cy="{}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#,
        max_w as u64 * PX_TO_EMU,
        max_h as u64 * PX_TO_EMU,
    );

    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes());
    zip.add(
        "_rels/.rels",
        format!(
            r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#
        )
        .as_bytes(),
    );
    zip.add(
        "docProps/core.xml",
        format!(
            r#"{XML_DECL}<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:creator>aginxbrowser</dc:creator></cp:coreProperties>"#
        )
        .as_bytes(),
    );
    zip.add(
        "docProps/app.xml",
        format!(
            r#"{XML_DECL}<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>aginxbrowser</Application></Properties>"#
        )
        .as_bytes(),
    );
    zip.add("ppt/presentation.xml", presentation.as_bytes());
    zip.add("ppt/_rels/presentation.xml.rels", pres_rels.as_bytes());
    zip.add("ppt/theme/theme1.xml", format!("{XML_DECL}{PPTX_THEME}").as_bytes());
    zip.add("ppt/slideMasters/slideMaster1.xml", format!("{XML_DECL}{PPTX_MASTER}").as_bytes());
    zip.add(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        format!(
            r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#
        )
        .as_bytes(),
    );
    zip.add("ppt/slideLayouts/slideLayout1.xml", format!("{XML_DECL}{PPTX_LAYOUT}").as_bytes());
    zip.add(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        format!(
            r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#
        )
        .as_bytes(),
    );
    for (i, &(w, h, jpeg)) in pages.iter().enumerate() {
        let n = i + 1;
        let slide = format!(
            r#"{XML_DECL}<p:sld xmlns:a="{A_NS}" xmlns:r="{R_NS}" xmlns:p="{P_NS}"><p:cSld>{}<p:pic><p:nvPicPr><p:cNvPr id="2" name="Page {n}"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{}" cy="{}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#,
            PPTX_SP_TREE_HEAD,
            w as u64 * PX_TO_EMU,
            h as u64 * PX_TO_EMU,
        );
        let slide_rels = format!(
            r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image{n}.jpeg"/></Relationships>"#
        );
        zip.add(&format!("ppt/slides/slide{n}.xml"), slide.as_bytes());
        zip.add(&format!("ppt/slides/_rels/slide{n}.xml.rels"), slide_rels.as_bytes());
        zip.add(&format!("ppt/media/image{n}.jpeg"), jpeg);
    }
    zip.finish()
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

/// Pack per-page JPEGs as an image-based DOCX: one page-sized section per
/// page image, zero margins. Word lets every section carry its own page
/// size, so variable-height print bands and slide pages keep exact heights.
pub fn docx_of_pages(pages: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut document = format!(
        r#"{XML_DECL}<w:document xmlns:w="{W_NS}" xmlns:wp="{WP_NS}" xmlns:a="{A_NS}" xmlns:pic="{PIC_NS}" xmlns:r="{R_NS}"><w:body>"#
    );
    let mut doc_rels = format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#
    );
    let sect = |w: u32, h: u32| {
        format!(
            r#"<w:sectPr><w:pgSz w="{}" h="{}"/><w:pgMar w:top="0" w:right="0" w:bottom="0" w:left="0" w:header="0" w:footer="0" w:gutter="0"/></w:sectPr>"#,
            w as u64 * PX_TO_TWIP,
            h as u64 * PX_TO_TWIP,
        )
    };
    for (i, &(w, h, _)) in pages.iter().enumerate() {
        let n = i + 1;
        let last = i + 1 == pages.len();
        // Section breaks ride in the pPr of the section's last paragraph;
        // the final section's sectPr sits at the body level.
        let sect_xml = if last { String::new() } else { sect(w, h) };
        document.push_str(&format!(
            r#"<w:p><w:pPr><w:spacing w:before="0" w:after="0" w:line="240" w:lineRule="auto"/>{sect_xml}</w:pPr><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{}" cy="{}"/><wp:docPr id="{n}" name="Page {n}"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri="{PIC_NS}"><pic:pic><pic:nvPicPr><pic:cNvPr id="{n}" name="Page {n}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rId{n}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{}" cy="{}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#,
            w as u64 * PX_TO_EMU,
            h as u64 * PX_TO_EMU,
            w as u64 * PX_TO_EMU,
            h as u64 * PX_TO_EMU,
        ));
        doc_rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image{n}.jpeg"/>"#
        ));
    }
    // Body-level sectPr for the final section (also the only section when
    // there is a single page).
    let (lw, lh) = pages
        .last()
        .map(|&(w, h, _)| (w, h))
        .unwrap_or((794, 1123));
    document.push_str(&sect(lw, lh));
    document.push_str("</w:body></w:document>");
    doc_rels.push_str("</Relationships>");

    let content_types = format!(
        r#"{XML_DECL}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpeg" ContentType="image/jpeg"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
    );

    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes());
    zip.add(
        "_rels/.rels",
        format!(
            r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#
        )
        .as_bytes(),
    );
    zip.add("word/document.xml", document.as_bytes());
    zip.add("word/_rels/document.xml.rels", doc_rels.as_bytes());
    for (i, &(_, _, jpeg)) in pages.iter().enumerate() {
        zip.add(&format!("word/media/image{}.jpeg", i + 1), jpeg);
    }
    zip.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the central directory and return (name, local-data-slice) pairs.
    fn entries_of(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let eocd = zip
            .windows(4)
            .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])
            .expect("EOCD");
        let rd16 = |p: usize| u16::from_le_bytes([zip[p], zip[p + 1]]) as usize;
        let rd32 = |p: usize| u32::from_le_bytes([zip[p], zip[p + 1], zip[p + 2], zip[p + 3]]) as usize;
        let count = rd16(eocd + 10);
        let mut cd = rd32(eocd + 16);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            assert_eq!(&zip[cd..cd + 4], &[0x50, 0x4b, 0x01, 0x02], "central dir sig");
            let csize = rd32(cd + 20);
            let name_len = rd16(cd + 28);
            let local = rd32(cd + 42);
            let name = String::from_utf8(zip[cd + 46..cd + 46 + name_len].to_vec()).expect("name");
            // Local header: 30 fixed bytes + name; then the stored payload.
            let l_name_len = rd16(local + 26);
            let data_at = local + 30 + l_name_len;
            out.push((name, zip[data_at..data_at + csize].to_vec()));
            cd += 46 + name_len;
        }
        out
    }

    #[test]
    fn crc32_known_check_value() {
        // The canonical CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn zip_roundtrips_stored_entries() {
        let mut zip = ZipWriter::new();
        zip.add("[Content_Types].xml", b"<Types/>");
        zip.add("media/img.jpeg", &[0xFF, 0xD8, 0x00, 0xFF, 0xD9]);
        let out = zip.finish();
        assert_eq!(&out[..4], &[0x50, 0x4b, 0x03, 0x04], "local header magic");
        let entries = entries_of(&out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "[Content_Types].xml");
        assert_eq!(entries[0].1, b"<Types/>".to_vec());
        assert_eq!(entries[1].1, vec![0xFF, 0xD8, 0x00, 0xFF, 0xD9]);
        // Deterministic: same inputs, same bytes (fixed DOS timestamp).
        let mut again = ZipWriter::new();
        again.add("[Content_Types].xml", b"<Types/>");
        again.add("media/img.jpeg", &[0xFF, 0xD8, 0x00, 0xFF, 0xD9]);
        assert_eq!(out, again.finish(), "byte-identical rebuild");
    }

    #[test]
    fn pptx_package_shape() {
        let j1 = vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9];
        let j2 = vec![0xFF, 0xD8, 0x02, 0xFF, 0xD9];
        let pptx = pptx_of_pages(&[(794, 1123, &j1), (794, 200, &j2)]);
        let entries = entries_of(&pptx);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        for needed in [
            "[Content_Types].xml",
            "_rels/.rels",
            "ppt/presentation.xml",
            "ppt/_rels/presentation.xml.rels",
            "ppt/theme/theme1.xml",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            "ppt/slides/slide1.xml",
            "ppt/slides/slide2.xml",
            "ppt/slides/_rels/slide1.xml.rels",
            "ppt/slides/_rels/slide2.xml.rels",
            "ppt/media/image1.jpeg",
            "ppt/media/image2.jpeg",
        ] {
            assert!(names.contains(&needed), "missing {needed}");
        }
        let ct = String::from_utf8(entries[0].1.clone()).expect("ct utf8");
        assert_eq!(ct.matches("slide+xml").count(), 2, "two slide overrides");
        let pres = entries
            .iter()
            .find(|(n, _)| n == "ppt/presentation.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("presentation");
        assert_eq!(pres.matches("<p:sldId ").count(), 2);
        // Deck slide size = max page height (1123px), in EMU.
        assert!(pres.contains(&format!("cy=\"{}\"", 1123 * 9525)));
        // Media bytes verbatim.
        assert_eq!(
            entries.iter().find(|(n, _)| n == "ppt/media/image1.jpeg").expect("m1").1,
            j1
        );
        // Determinism.
        assert_eq!(pptx, pptx_of_pages(&[(794, 1123, &j1), (794, 200, &j2)]));
    }

    #[test]
    fn docx_package_shape() {
        let j1 = vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9];
        let j2 = vec![0xFF, 0xD8, 0x02, 0xFF, 0xD9];
        let docx = docx_of_pages(&[(794, 1123, &j1), (794, 200, &j2)]);
        let entries = entries_of(&docx);
        let doc = entries
            .iter()
            .find(|(n, _)| n == "word/document.xml")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("document");
        // Two inline drawings (one per page).
        assert_eq!(doc.matches("<w:drawing>").count(), 2);
        // Page 1's section break rides in its paragraph; the final section
        // is the body-level sectPr sized to the LAST page (200px).
        assert_eq!(doc.matches("<w:sectPr>").count(), 2);
        assert!(doc.contains(&format!(r#"h="{}""#, 1123 * 15)), "mid sectPr = page 1 height");
        assert!(doc.contains(&format!(r#"h="{}""#, 200 * 15)), "final sectPr = page 2 height");
        // Images referenced per page.
        let rels = entries
            .iter()
            .find(|(n, _)| n == "word/_rels/document.xml.rels")
            .map(|(_, d)| String::from_utf8_lossy(d).to_string())
            .expect("rels");
        assert_eq!(rels.matches("image1.jpeg").count(), 1);
        assert_eq!(rels.matches("image2.jpeg").count(), 1);
        assert_eq!(
            entries.iter().find(|(n, _)| n == "word/media/image2.jpeg").expect("m2").1,
            j2
        );
        assert_eq!(docx, docx_of_pages(&[(794, 1123, &j1), (794, 200, &j2)]), "deterministic");
    }
}
