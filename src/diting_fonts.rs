//! Bundled CJK font supply (render claim batch 3c).
//!
//! `/screenshot` used to depend on the server having `fonts-noto-cjk`
//! installed — same environment dependency that leaves upstream
//! obscura-render unable to draw Chinese at all. This module embeds a
//! Noto Sans SC subset (OFL — see diting_fonts/OFL.txt) at wght 400/700
//! covering full GB2312 (6763 Han) + ASCII + the common non-GB2312 symbol
//! blocks (Latin-1, General Punctuation, Arrows, Geometric/Misc Symbols,
//! Dingbats — the ·/—/✓ tail GB2312's codec omits), and feeds BOTH engines:
//!
//! - the blitz pipeline via [`font_ctx`] — registered under the real family
//!   name, hoisted to the head of fontique's Han script fallback so CJK text
//!   resolves to these bytes on every machine, with system fonts kept as a
//!   tail for chars outside GB2312 (and other scripts);
//! - the diting layout/paint stack via [`font_book`].
//!
//! Deterministic where covered, graceful where not — the compromise between
//! the fixture approach (bytes pinned both sides) and real-world pages.
//! Regenerate with `scripts/make_font_bundle.py`.

use std::sync::Arc;

use crate::diting_layout::text::FontBook;

/// Real family name, on purpose: pages that style `font-family: "Noto Sans
/// SC"` (common on Chinese sites) hit the bundled bytes directly, and the
/// name truthfully reflects the OFL-licensed source.
#[cfg(feature = "blitz-reference")]
pub const FAMILY: &str = "Noto Sans SC";

const REGULAR: &[u8] = include_bytes!("diting_fonts/diting-cjk-regular.ttf");
const BOLD: &[u8] = include_bytes!("diting_fonts/diting-cjk-bold.ttf");

/// Build the parley FontContext for a blitz render: bundled faces registered,
/// then hoisted to the head of the Han fallback chain ahead of whatever the
/// built-in registry would pick, and appended to the generic sans/serif
/// families so even a completely font-less machine resolves unstyled text.
/// `system_fonts` stays on — the tail fallback for non-GB2312 codepoints.
///
/// Only the blitz reference pipeline consumes a parley FontContext (the
/// diting stack goes through [`font_book`]), hence the feature gate.
#[cfg(feature = "blitz-reference")]
pub fn font_ctx() -> parley::FontContext {
    build_ctx(true)
}

#[cfg(feature = "blitz-reference")]
fn build_ctx(system_fonts: bool) -> parley::FontContext {
    use parley::fontique::{
        Collection, CollectionOptions, FontInfoOverride, FontStyle, FallbackKey, GenericFamily,
        Script, SourceCache,
    };

    let mut ctx = parley::FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions { shared: false, system_fonts }),
    };
    let mut families = Vec::new();
    for (bytes, weight) in [(REGULAR, 400.0), (BOLD, 700.0)] {
        let blob = parley::fontique::Blob::new(std::sync::Arc::new(bytes.to_vec()));
        let info = FontInfoOverride {
            family_name: Some(FAMILY),
            weight: Some(parley::fontique::FontWeight::new(weight)),
            style: Some(FontStyle::Normal),
            ..Default::default()
        };
        for (family, _) in ctx.collection.register_fonts(blob, Some(info)) {
            families.push(family);
        }
    }
    if !families.is_empty() {
        // Ours first, then whatever the built-in chain had (system Noto etc.)
        // — `set_fallbacks` replaces, so chain the previous list back on.
        let key = FallbackKey::new(Script::from_bytes(*b"Hani"), None);
        let existing: Vec<_> = ctx.collection.fallback_families(key).collect();
        let _ = ctx.collection.set_fallbacks(key, families.iter().copied().chain(existing));
        // A machine with no fonts at all must still resolve generic text.
        for generic in [GenericFamily::SansSerif, GenericFamily::Serif, GenericFamily::Monospace] {
            ctx.collection.append_generic_families(generic, families.iter().copied());
        }
    }
    ctx
}

/// The same bundle as a [`FontBook`] for the diting layout/paint stack,
/// with the platform's color-emoji face (when present) appended as a
/// single-weight fallback — the same posture browsers take: the emoji font
/// has no bold variant, and chars the primary pair lacks resolve through it.
///
/// Cached in a `OnceLock`: the book used to be re-parsed per call (the
/// video pump carried its own copy for exactly that reason), and since the
/// raster cache (#399) keys on the book's face fingerprint, constructing
/// the book once per process also computes that hash once.
pub fn font_book() -> Arc<FontBook> {
    static BOOK: std::sync::OnceLock<Arc<FontBook>> = std::sync::OnceLock::new();
    BOOK.get_or_init(|| {
        let book = FontBook::from_pairs(REGULAR.to_vec(), BOLD.to_vec())
            .expect("bundled CJK fonts parse (regenerate via scripts/make_font_bundle.py)");
        match platform_emoji_font() {
            Some(bytes) => Arc::new(book.with_fallbacks(vec![bytes])),
            None => Arc::new(book),
        }
    })
    .clone()
}

/// Best-effort read of the host's color-emoji font (emoji batch): the
/// bundled pair carries no emoji glyphs, and shipping Apple/Noto emoji
/// bytes in-binary is both a size and a licensing question we don't need
/// to answer — every platform a screenshot server runs on already ships
/// one. `None` (file absent / unreadable) is the graceful pre-emoji
/// behavior: runs stay .notdef exactly as before, no feature regression.
///
/// `pub(crate)` for the gated raster tests, which skip when it returns
/// `None` (a font-less CI container must stay green).
pub(crate) fn platform_emoji_font() -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &["/System/Library/Fonts/Apple Color Emoji.ttc"];
    #[cfg(all(unix, not(target_os = "macos")))]
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/noto-color-emoji/NotoColorEmoji.ttf",
        "/usr/share/fonts/truetype/google-noto-color-emoji/NotoColorEmoji.ttf",
        "/usr/share/fonts/google-noto-color-emoji/NotoColorEmoji.ttf",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\seguiemj.ttf",
    ];

    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    None
}

/// The bundled pair as owned bytes — the raster-cache tests in
/// `diting_layout::text` build sibling `FontBook`s from them (fingerprint
/// isolation: same bytes = another instance, swapped = a different set).
#[cfg(test)]
pub(crate) fn bundled_pair_for_tests() -> (Vec<u8>, Vec<u8>) {
    (REGULAR.to_vec(), BOLD.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GB2312 coverage spot check through the REAL consumption path
    /// (advance + raster, not a cmap table dump): every full-width Han
    /// glyph must advance exactly one em and actually paint ink. A char
    /// missing from the subset would .notdef — whose 1em advance passes
    /// width asserts while the raster stays empty (the batch-3b lesson),
    /// so the ink check is the one that bites.
    #[test]
    fn bundle_cjk_coverage_paints() {
        let book = font_book();
        let sample = "国庆节快乐浏览器引擎字体渲染简体汉字上海北京深圳杭州";
        for ch in sample.chars() {
            let w = book.advance_width(&ch.to_string(), 20.0, false);
            assert!((w - 20.0).abs() < 0.05, "{ch}: advance {w} (want one em)");
        }
        let raster = book.rasterize("汉字渲染", 24.0, false, [0, 0, 0, 255], 24.0 * 1.2);
        assert!(raster.ink_bbox().is_some(), "bundle raster must have ink");
    }

    /// Symbol-coverage counterpart of [`bundle_cjk_coverage_paints`]: the
    /// non-GB2312 punctuation/symbol tail (·/—/✓/→/●/★/…) that GB2312's codec
    /// omits — the diting-font-fallback-gap. Each must paint real ink, not
    /// .notdef (whose empty raster is what a coverage miss would produce; the
    /// 1em-advance check is blind to it, so ink is the test that bites).
    #[test]
    fn bundle_symbol_coverage_paints() {
        let book = font_book();
        let sample = "·—–…✓→←↑↓●○◆◇■□▲△▼▽★☆";
        for ch in sample.chars() {
            let raster = book.rasterize(&ch.to_string(), 24.0, false, [0, 0, 0, 255], 24.0 * 1.2);
            assert!(
                raster.ink_bbox().is_some(),
                "{ch} (U+{:04X}) must have ink",
                ch as u32
            );
        }
    }

    /// Emoji fallback (emoji batch): with a platform emoji font present, a
    /// char the primary pair lacks must resolve through the fallback face —
    /// colored RGBA ink from the embedded bitmap strike, not a mono .notdef
    /// box — and measure/paint must agree on the mixed run's advance (the
    /// segmentation is shared by construction). Skipped when the host ships
    /// no emoji font: a font-less CI container keeps the graceful posture.
    #[test]
    fn emoji_fallback_paints_color_ink() {
        if platform_emoji_font().is_none() {
            eprintln!("skipping: no platform emoji font on this host");
            return;
        }
        let book = font_book();
        assert!(book.has_fallbacks(), "the emoji face must load as a fallback");

        let adv_cjk = book.advance_width("字字", 24.0, false);
        let adv_mixed = book.advance_width("字🚀字", 24.0, false);
        assert!(
            adv_mixed > adv_cjk + 4.0,
            "the emoji must contribute its own advance: {adv_mixed} vs {adv_cjk}"
        );

        let raster = book.rasterize("🚀", 24.0, false, [0, 0, 0, 255], 24.0 * 1.2);
        assert!(raster.ink_bbox().is_some(), "the emoji must have ink");
        let colored = raster
            .data
            .chunks_exact(4)
            .filter(|p| {
                p[3] > 128
                    && (p[0] as i32 - p[1] as i32).abs().max((p[1] as i32 - p[2] as i32).abs()) > 32
            })
            .count();
        assert!(
            colored > 20,
            "emoji ink must come from the colored bitmap strike, not a mono glyph ({colored} px)"
        );

        // The wrapped painter shares the segmentation: a mixed line wraps and
        // both the CJK and the emoji halves paint.
        let wrapped = book.rasterize_wrapped(
            "文字🚀文字",
            24.0,
            false,
            [0, 0, 0, 255],
            60.0,
            24.0 * 1.2,
        );
        assert!(wrapped.ink_bbox().is_some(), "wrapped mixed run must have ink");
    }

    /// The product claim: CJK text renders with the bundled collection and
    /// system fonts DISABLED — no fonts-noto-cjk, no PingFang, nothing.
    /// Uses the real production wiring (build_ctx), only with the system
    /// tail turned off. Renders through the Blitz pipeline (the heavier
    /// engine-level variant of the swash coverage tests above) — hence
    /// `blitz-reference`; in a screenshot-only build the bundle claim is
    /// carried by bundle_cjk_coverage_paints / bundle_symbol_coverage_paints.
    #[cfg(feature = "blitz-reference")]
    #[test]
    fn cjk_renders_without_system_fonts() {
        let ctx = build_ctx(false);

        let html = r#"<style>body { margin: 0; font-family: serif; }</style>
            <body><div id="t">谛听引擎汉字渲染</div></body>"#;
        let mut doc = blitz_html::HtmlDocument::from_html(
            html,
            blitz_dom::DocumentConfig {
                base_url: Some("https://example.com/".to_string()),
                net_provider: None,
                font_ctx: Some(ctx),
                viewport: Some(blitz_traits::shell::Viewport::new(400, 80, 1.0, Default::default())),
                ..Default::default()
            },
        );
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let mut doc = doc;
        let (w, h) = (400u32, 80u32);
        let buffer = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
            |scene| {
                use anyrender::PaintScene as _;
                use peniko::kurbo::Rect;
                scene.fill(
                    peniko::Fill::NonZero,
                    Default::default(),
                    peniko::Color::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, w as f64, h as f64),
                );
                blitz_paint::paint_scene(scene, &mut doc, 1.0, w, h, 0, 0);
            },
            w,
            h,
        );
        // Black ink on the white fill = the bundle carried the glyphs.
        let ink = buffer
            .chunks(4)
            .filter(|p| p[0] < 128 && p[1] < 128 && p[2] < 128)
            .count();
        assert!(ink > 100, "CJK must paint from the bundle alone (ink px={ink})");
    }
}
