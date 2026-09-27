//! Han-unification slot selection (#139, blitz#932 absorption).
//!
//! One codepoint, four glyph systems: 直 renders with a different stroke
//! shape in a Simplified, Traditional, Japanese or Korean font, and which
//! one a page wants is carried by the nearest ancestor's `lang`/`xml:lang`
//! attribute — the same signal upstream feeds Parley as the text locale.
//! The engine's bundled pair is the SC slot (Noto Sans SC); vendor Han
//! variants (ja/tc/ko faces) join through `--font-dir` as coverage tails.
//! Before this module the fallback chain never looked at lang, so a
//! `lang="ja"` page rendered every shared codepoint as the SC glyph —
//! 薬/直/骨 reading Chinese on a Japanese page.
//!
//! The fix is slot-aware routing in [`super::FontBook::segments`]: a run
//! whose lang maps to a slot sends unified ideographs to the first
//! same-slot face that covers the char (the primary pair counts — it IS
//! the SC slot). Everything else keeps the plain cascade: non-Han chars
//! never re-font (a ja page's Latin stays in the primary face), and a
//! slot with no matching face falls back to today's exact order.

use swash::FontRef;

/// Which unified-ideograph system a run's glyphs should come from —
/// derived from the run's language tag ([`han_slot_for_lang`]) and matched
/// against each face's classification ([`face_han_slot`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HanSlot {
    Simplified,
    Traditional,
    Japanese,
    Korean,
}

/// Map a BCP-47-ish `lang` value to the Han slot it wants. `ja` →
/// Japanese, `ko` → Korean, `zh` → Traditional when a region/script subtag
/// says so (tw/hk/mo/hant) else Simplified (cn/sg/hans and the bare `zh`
/// default — this engine's home turf). Anything else is `None`: no slot
/// preference, the plain per-char cascade, exactly the pre-#139 order.
pub fn han_slot_for_lang(lang: &str) -> Option<HanSlot> {
    let subs: Vec<String> = lang
        .split('-')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    match subs.first().map(String::as_str) {
        Some("ja") => Some(HanSlot::Japanese),
        Some("ko") => Some(HanSlot::Korean),
        Some("zh") => {
            let trad = subs[1..].iter().any(|s| {
                matches!(s.as_str(), "tw" | "hk" | "mo") || s.starts_with("hant")
            });
            if trad {
                Some(HanSlot::Traditional)
            } else {
                Some(HanSlot::Simplified)
            }
        }
        _ => None,
    }
}

/// The unified-ideograph ranges — the only chars slot routing applies to.
/// Han-unification is about ideographs: kana/Hangul glyph shapes are not
/// unified, and CJK punctuation differences are too subtle to justify
/// re-routing, so neither rides the slot.
pub fn is_han_ideograph(ch: char) -> bool {
    matches!(ch, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// Classify which Han slot a face serves, from its bytes.
///
/// The name table is the only signal that separates SC from TC (both cover
/// the same codepoints — the difference is glyph shapes), and it also
/// outranks every other hint: the bundled SC pair itself carries kana
/// coverage AND a GSUB `kana` script (Noto Sans SC genuinely has both), so
/// any script/coverage heuristic would misfile the primary face as
/// Japanese. Name tokens are matched as whole words (case-insensitive)
/// across family / typographic family / full name, with a CJK-substring
/// pass for 繁/简/日本/한 names. A face with no name signal at all falls
/// back to its GSUB/GPOS script tags (a name-stripped subset whose layout
/// tables still say `kana`/`hang`); `None` means "no slot claim" — the
/// face is never preferred over the primary pair.
pub fn face_han_slot(bytes: &[u8]) -> Option<HanSlot> {
    let font = FontRef::from_index(bytes, 0)?;
    let mut name = String::new();
    for id in [
        swash::StringId::Family,
        swash::StringId::TypographicFamily,
        swash::StringId::Full,
    ] {
        if let Some(s) = font.localized_strings().find_by_id(id, None) {
            s.chars().for_each(|c| name.push(c));
            name.push(' ');
        }
    }
    if !name.trim().is_empty() {
        // to_ascii_lowercase leaves the CJK markers untouched.
        let lowered = name.to_ascii_lowercase();
        let by_token = lowered.split_whitespace()
            .flat_map(|w| w.split(|c: char| !c.is_alphanumeric()))
            .find_map(|tok| match tok {
                "jp" | "jpn" | "japan" | "japanese" => Some(HanSlot::Japanese),
                "kr" | "kor" | "korea" | "korean" => Some(HanSlot::Korean),
                "tc" | "tw" | "hk" | "mo" | "cns" | "traditional" => Some(HanSlot::Traditional),
                "sc" | "cn" | "sg" | "prc" | "simplified" => Some(HanSlot::Simplified),
                _ => None,
            });
        if let Some(slot) = by_token {
            return Some(slot);
        }
        if lowered.contains('繁') {
            return Some(HanSlot::Traditional);
        }
        if lowered.contains('简') {
            return Some(HanSlot::Simplified);
        }
        if lowered.contains("日本") || lowered.contains("ゴシック") || lowered.contains("明朝") {
            return Some(HanSlot::Japanese);
        }
        if lowered.contains('한') {
            return Some(HanSlot::Korean);
        }
    }
    for ws in font.writing_systems() {
        if ws.script_tag() == swash::tag_from_bytes(b"kana") {
            return Some(HanSlot::Japanese);
        }
        if ws.script_tag() == swash::tag_from_bytes(b"hang") {
            return Some(HanSlot::Korean);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diting_fonts::bundled_pair_for_tests;

    #[test]
    fn lang_tags_map_to_slots() {
        assert_eq!(han_slot_for_lang("ja"), Some(HanSlot::Japanese));
        assert_eq!(han_slot_for_lang("ja-JP"), Some(HanSlot::Japanese));
        assert_eq!(han_slot_for_lang("JA-jp"), Some(HanSlot::Japanese));
        assert_eq!(han_slot_for_lang("ko"), Some(HanSlot::Korean));
        assert_eq!(han_slot_for_lang("ko-KR"), Some(HanSlot::Korean));
        assert_eq!(han_slot_for_lang("zh"), Some(HanSlot::Simplified));
        assert_eq!(han_slot_for_lang("zh-CN"), Some(HanSlot::Simplified));
        assert_eq!(han_slot_for_lang("zh-Hans"), Some(HanSlot::Simplified));
        assert_eq!(han_slot_for_lang("zh-SG"), Some(HanSlot::Simplified));
        assert_eq!(han_slot_for_lang("zh-TW"), Some(HanSlot::Traditional));
        assert_eq!(han_slot_for_lang("zh-HK"), Some(HanSlot::Traditional));
        assert_eq!(han_slot_for_lang("zh-Hant-TW"), Some(HanSlot::Traditional));
        assert_eq!(han_slot_for_lang("en"), None);
        assert_eq!(han_slot_for_lang("en-US"), None);
        assert_eq!(han_slot_for_lang(""), None);
        assert_eq!(han_slot_for_lang("  "), None);
    }

    #[test]
    fn ideograph_ranges() {
        assert!(is_han_ideograph('直'));
        assert!(is_han_ideograph('薬'));
        assert!(is_han_ideograph('\u{3400}'));
        assert!(is_han_ideograph('\u{FA30}'));
        // Not unified: Latin, kana, Hangul, CJK punctuation.
        assert!(!is_han_ideograph('a'));
        assert!(!is_han_ideograph('あ'));
        assert!(!is_han_ideograph('가'));
        assert!(!is_han_ideograph('、'));
    }

    /// The bundled pair must classify Simplified — by NAME, not by its
    /// kana coverage or its GSUB `kana` script (Noto Sans SC carries both,
    /// the trap that disqualifies coverage as a signal).
    #[test]
    fn bundled_pair_is_the_simplified_slot() {
        let (reg, bold) = bundled_pair_for_tests();
        assert_eq!(face_han_slot(&reg), Some(HanSlot::Simplified));
        assert_eq!(face_han_slot(&bold), Some(HanSlot::Simplified));
    }

    /// The ja fixture: family name "Diting Test JP" with the SC pair's own
    /// GSUB underneath — the name token wins the classification.
    #[test]
    fn ja_fixture_classifies_japanese() {
        let bytes = include_bytes!("../../diting_fonts/test-han-ja-face.ttf");
        assert_eq!(face_han_slot(bytes), Some(HanSlot::Japanese));
    }

    #[test]
    fn nameless_and_garbage_faces_claim_nothing() {
        assert_eq!(face_han_slot(b"not a font"), None);
        assert_eq!(face_han_slot(b""), None);
        // The bundled mono face: "Noto Sans Mono" carries no slot token.
        let mono = crate::diting_fonts::bundled_mono_for_tests();
        assert_eq!(face_han_slot(&mono), None);
    }
}
