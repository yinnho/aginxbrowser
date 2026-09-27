use super::*;

/// Serialize budget-touching tests and restore the budget on scope exit —
/// the env-knob guard precedent (1f7486c): the cache is process-global,
/// a leaked tiny budget would silently neuter every later test. The guard
/// OWNS the lock: releasing it at shrink() return would leave the tiny
/// budget exposed to a concurrent test's inserts.
pub(crate) struct BudgetGuard {
    _held: std::sync::MutexGuard<'static, ()>,
    prev: usize,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        RASTER_CACHE_MAX.store(self.prev, Ordering::Relaxed);
    }
}

static BUDGET_LOCK: Mutex<()> = Mutex::new(());

/// A clean slate under the serialization lock — the starting posture of
/// every cache test (parallel tests in other modules only ever INSERT;
/// only these tests reset or shrink).
pub(crate) fn isolated() -> std::sync::MutexGuard<'static, ()> {
    let held = BUDGET_LOCK.lock().unwrap();
    RasterCache::reset();
    held
}

pub(crate) fn shrink_budget_for_test(bytes: usize) -> BudgetGuard {
    let _held = BUDGET_LOCK.lock().unwrap();
    RasterCache::reset();
    let prev = RASTER_CACHE_MAX.swap(bytes, Ordering::Relaxed);
    BudgetGuard { _held, prev }
}

/// The production face bytes, for building sibling `FontBook`s in the
/// fingerprint test (same bytes = another instance; swapped = a different
/// face set).
fn production_pair() -> (Vec<u8>, Vec<u8>) {
    crate::diting_fonts::bundled_pair_for_tests()
}

/// The cache contract's happy path: a repeated rasterize returns the SAME
/// tile (`Arc::ptr_eq` — no re-shaping, no re-raster), while a different
/// color bakes different pixels into a different tile.
#[test]
fn repeated_rasterize_shares_the_cached_tile() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    let _held = isolated();
    let black = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    let again = book.rasterize_wrapped("缓存命中", 16.0, false, [0, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    assert!(Arc::ptr_eq(&black, &again), "repeat must hand back the cached Arc");
    assert!(black.ink_bbox().is_some(), "the tile has real ink");

    let red = book.rasterize_wrapped("缓存命中", 16.0, false, [255, 0, 0, 255], 20.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    assert!(!Arc::ptr_eq(&black, &red), "color rides the key — a new tile");
    let ink = |r: &TextRaster| {
        r.data
            .chunks_exact(4)
            .filter(|p| p[3] > 200)
            .map(|p| (p[0], p[1], p[2]))
            .max()
    };
    assert_eq!(ink(&black), Some((0, 0, 0)), "black tile's opaque ink is black");
    assert_eq!(ink(&red), Some((255, 0, 0)), "red tile's opaque ink is red");
}

/// `rasterize` (single line) and `rasterize_wrapped` are different
/// rasterizers with different tiles — the kind rides the key, so the two
/// must not collide even at identical text/params.
#[test]
fn line_and_wrapped_rasters_cache_separately() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    let _held = isolated();
    let text = "一行两行三行四行";
    let line = book.rasterize(text, 16.0, false, [0, 0, 0, 255], 24.0, false, None);
    // Narrow wrap: the same text breaks across 4+ lines, so the wrapped
    // tile is much taller than the single-line one.
    let wrapped = book.rasterize_wrapped(text, 16.0, false, [0, 0, 0, 255], 40.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    assert!(!Arc::ptr_eq(&line, &wrapped), "kinds must not collide");
    assert!(
        wrapped.height > line.height * 2,
        "wrapped tile stacks lines ({} vs {})",
        wrapped.height,
        line.height
    );
}

/// Budget overflow clears everything: with the budget at exactly one
/// entry's weight, the second insert wipes the map — the first entry's
/// next rasterize comes back as a fresh tile.
#[test]
fn budget_overflow_clears_all() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    // Measure entry A's weight under a normal-budget lock, then shrink —
    // two lock windows (std Mutex is not reentrant), which is safe: other
    // budget tests serialize the same way and insert-only tests can't
    // shrink anything.
    let a_weight = {
        let _held = isolated();
        let probe = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false, None);
        probe.data.len() + 2 + 64
    };
    let _budget = shrink_budget_for_test(a_weight + 1);
    let a1 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false, None);
    let same = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false, None);
    assert!(Arc::ptr_eq(&a1, &same), "A alone fits the budget exactly");
    // A longer text is a strictly heavier entry (wider tile, longer key):
    // inserting it overflows and clears — A's tile is gone.
    let b = book.rasterize("iiiiiiiiiiii", 8.0, false, [0, 0, 0, 255], 10.0, false, None);
    assert!(!Arc::ptr_eq(&a1, &b));
    let a2 = book.rasterize("ii", 8.0, false, [0, 0, 0, 255], 10.0, false, None);
    assert!(!Arc::ptr_eq(&a1, &a2), "clear-all must evict the earlier entry");
}

/// dfrg/swash#139 (own issue #19): a fallback face carrying an EBLC/EBDT
/// strike whose glyph declares width 0 (SimSun's blank-bitmap shape)
/// makes swash's `Bitmap::decode` call `chunks(0)` — a panic that fires
/// in release builds too, at the `Source::Bitmap` step of
/// `fallback_sources`, aborting the whole line raster before the
/// outline tail can run. [`render_guarded`] must absorb it and re-raster
/// the glyph outline-only. The fixture is OURS
/// (`scripts/make_zero_width_ebdt_font.py`), no license tail.
#[test]
fn zero_width_ebdt_strike_does_not_panic_the_raster() {
    let (reg, bold) = production_pair();
    let zw = include_bytes!("../fixtures/zero-width-ebdt.ttf").to_vec();
    let book = FontBook::from_pairs(reg, bold).unwrap().with_fallbacks(vec![zw]);
    let _held = isolated();
    let before = SWASH_PANIC_FALLBACKS.load(Ordering::Relaxed);
    // U+E000: outside the bundled pair's coverage (test-fallback-face
    // precedent), so the char routes to a fallback segment — the only
    // segment whose source list visits Source::Bitmap. Size 12 hits the
    // fixture strike's ppem exactly.
    let tile = book.rasterize("\u{E000}", 12.0, false, [0, 0, 0, 255], 14.4, false, None);
    let after = SWASH_PANIC_FALLBACKS.load(Ordering::Relaxed);
    assert!(
        after > before,
        "guard must actually fire (counter {before} -> {after}); \
         without it this line raster aborts the whole render"
    );
    // The glyph's outline is empty (numberOfContours 0), so the outline
    // retry paints no ink — but the run still yields a well-formed tile
    // instead of a poisoned/aborted raster.
    assert_eq!(tile.data.len(), tile.width * tile.height * 4);
    assert_eq!(tile.ink_bbox(), None, "empty glyph outline paints no ink");
}

/// The book's face fingerprint is the cache's book identity: two books
/// built from the same bytes share entries (either instance's tile serves
/// both), while a book with different face bytes gets its own.
#[test]
fn face_fingerprint_isolates_books() {
    let (reg, bold) = production_pair();
    let a = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
    let b = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
    // Swapped faces: a genuinely different byte set (regular slot carries
    // the bold face and vice versa), so a different fingerprint.
    let swapped = FontBook::from_pairs(bold, reg).unwrap();
    let _held = isolated();
    let r1 = a.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false, None);
    let r2 = b.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false, None);
    assert!(Arc::ptr_eq(&r1, &r2), "same face bytes share entries across instances");
    let r3 = swapped.rasterize("指纹隔离", 12.0, false, [0, 0, 0, 255], 15.0, false, None);
    assert!(!Arc::ptr_eq(&r1, &r3), "different face bytes must not share entries");
}

/// The mono flag rides the key: a `mono: true` rasterize must not collide
/// with the `mono: false` twin even on a mono-less book (identical
/// pixels, distinct entry), and a book WITH the bundled mono face gets
/// its own tiles. Plus the advance contract that motivated the face: ten
/// ASCII digits at 20px shape to exactly 120px (0.6em each — Chrome's
/// Courier advance), while CJK in the same mono run keeps the CJK face.
#[test]
fn mono_rides_the_key_and_shapes_fixed_advance() {
    let (reg, bold) = production_pair();
    let sans = FontBook::from_pairs(reg.clone(), bold.clone()).unwrap();
    let mono_book =
        FontBook::from_pairs(reg, bold).unwrap().with_mono(crate::diting_fonts::bundled_mono_for_tests());
    let _held = isolated();
    let plain = sans.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, false, None);
    let flagged = sans.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, true, None);
    assert!(!Arc::ptr_eq(&plain, &flagged), "mono flag rides the key");
    let faced = mono_book.rasterize("mono key", 14.0, false, [0, 0, 0, 255], 18.0, true, None);
    assert!(!Arc::ptr_eq(&plain, &faced), "a mono face is different pixels");
    let adv = mono_book.advance_width("0000000000", 20.0, false, true, None);
    assert!((adv - 120.0).abs() < 0.01, "10 × 0.6em = 120px at 20px, got {adv}");
    let cjk_mono = mono_book.advance_width("汉", 20.0, false, true, None);
    let cjk_sans = sans.advance_width("汉", 20.0, false, false, None);
    assert!(
        (cjk_mono - cjk_sans).abs() < 0.01,
        "CJK keeps the CJK face in a mono run ({cjk_mono} vs {cjk_sans})"
    );
}

/// A book without a mono face degrades: `mono: true` segments exactly as
/// `mono: false` — every char on the primary pair. The blitz cross-check
/// fixture books never see the routing.
#[test]
fn mono_without_face_degrades_to_primary() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    let flagged = book.segments("code 汉 x", false, true, None);
    let plain = book.segments("code 汉 x", false, false, None);
    assert_eq!(flagged, plain, "no mono face: the flag is invisible");
    assert!(flagged.iter().all(|(sel, _)| *sel == FaceSel::Primary));
}

/// `word-spacing` rides the token model (CSS Text §7.1): exactly the
/// rendered word-separator tokens — the collapsed " " tokens — grow by
/// the extra advance; word tokens keep their natural width so wrap,
/// measure and paint all see the same shifted columns.
#[test]
fn tokens_of_applies_word_spacing_to_space_tokens_only() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    let plain = tokens_of("ab cd ef", 16.0, false, &book, false, 0.0, crate::diting_css::WhiteSpace::Normal, false, None);
    let spaced = tokens_of("ab cd ef", 16.0, false, &book, false, 9.0, crate::diting_css::WhiteSpace::Normal, false, None);
    assert_eq!(plain.len(), spaced.len(), "spacing never changes the token count");
    for (p, s) in plain.iter().zip(&spaced) {
        assert_eq!(p.text, s.text);
        if p.is_space {
            assert!((s.width - (p.width + 9.0)).abs() < 0.01,
                "space token grows by exactly the extra advance ({} → {})", p.width, s.width);
        } else {
            assert_eq!(p.width, s.width, "word token width is spacing-invariant");
        }
    }
    // `normal` is modeled as 0.0 — identical tokens.
    let normal = tokens_of("ab cd", 16.0, false, &book, false, 0.0, crate::diting_css::WhiteSpace::Normal, false, None);
    assert_eq!(normal[1].is_space, true);
}

/// The wrapped rasterizer must actually MOVE the second word, not just
/// grow the tile: with ws≠0 the glyphs blit per token at the cumulative
/// pen, so the ink's right edge widens by the extra advance too (the
/// whole-line shape would have left the ink put). ws=0 keeps the exact
/// existing path — byte-identical tile against a re-rasterize.
#[test]
fn rasterize_wrapped_word_spacing_shifts_ink() {
    let (reg, bold) = production_pair();
    let book = FontBook::from_pairs(reg, bold).unwrap();
    let _held = isolated();
    let tight = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    let loose = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 12.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    assert_eq!(tight.height, loose.height, "one line either way");
    let ink_right = |r: &TextRaster| r.ink_bbox().map(|b| b.2).unwrap_or(0);
    assert!(
        (ink_right(&loose) - ink_right(&tight)) as f32 >= 11.0,
        "second word's ink shifts right by the extra advance (tight={}, loose={})",
        ink_right(&tight), ink_right(&loose)
    );
    // Negative spacing tightens toward overlap — still deterministic.
    let tight2 = book.rasterize_wrapped("ab cd", 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, -8.0, None, crate::diting_css::WhiteSpace::Normal, false, None);
    assert!(ink_right(&tight2) < ink_right(&tight), "negative ws pulls the second word left");
}

/// Small-caps synthesis segments a token at case boundaries (`ß` is a
/// lowercase char whose uppercase is "SS"); caseless chars ride the
/// current segment (digits after a capital stay full-size).
#[test]
fn caps_case_runs_segments_at_case_boundaries() {
    assert_eq!(
        caps_case_runs("hello"),
        vec![("hello".to_string(), true)]
    );
    assert_eq!(
        caps_case_runs("ßaB1c"),
        vec![("ßa".to_string(), true), ("B1".to_string(), false), ("c".to_string(), true)]
    );
    // Caseless-only text stays one full-size segment.
    assert_eq!(caps_case_runs("123"), vec![("123".to_string(), false)]);
    assert_eq!(caps_case_runs(""), Vec::new());
}

/// tokens_of with `small_caps` emits pre-uppercased tokens carrying
/// SMALL_CAPS_RATIO on lowercase runs; already-uppercase text keeps
/// scale 1.0 and the plain tokenizer passes `false` never rescales.
#[test]
fn tokens_of_small_caps_uppercases_lower_runs_at_ratio() {
    let book = crate::diting_fonts::font_book();
    let toks = tokens_of(
        "hello AB", 16.0, false, &book, false, 0.0,
        crate::diting_css::WhiteSpace::Normal, true, None,
    );
    let words: Vec<&Token> = toks.iter().filter(|t| !t.is_space).collect();
    assert_eq!(words.len(), 2, "hello + AB");
    assert_eq!(words[0].text, "HELLO");
    assert!((words[0].scale - SMALL_CAPS_RATIO).abs() < 1e-6);
    assert_eq!(words[1].text, "AB");
    assert_eq!(words[1].scale, 1.0, "already-uppercase stays full-size");

    let plain = tokens_of(
        "hello AB", 16.0, false, &book, false, 0.0,
        crate::diting_css::WhiteSpace::Normal, false, None,
    );
    assert!(plain.iter().all(|t| t.scale == 1.0));
    assert_eq!(plain.iter().find(|t| !t.is_space).unwrap().text, "hello");
}

/// Element opacity rides the text color's alpha channel (the layout
/// walk's `with_alpha`); `colorize` must multiply it into the glyph
/// coverage — not paste RGB at full coverage, which made `opacity: 0`
/// hide backgrounds while leaving text fully inked.
#[test]
fn colorize_folds_color_alpha_into_coverage() {
    let tile = colorize(&[255, 128, 0], [10, 20, 30, 128]);
    // 255 * 128/255 = 128, 128 * 128/255 = 64.
    assert_eq!(&tile[0..4], &[10, 20, 30, 128]);
    assert_eq!(&tile[4..8], &[10, 20, 30, 64]);
    // Zero coverage stays empty; zero color alpha kills any coverage.
    assert_eq!(&tile[8..12], &[0, 0, 0, 0]);
    let gone = colorize(&[255, 200], [10, 20, 30, 0]);
    assert_eq!(gone.iter().filter(|&&b| b != 0).count(), 0);
    // Fully opaque color is the old behavior, byte for byte.
    let solid = colorize(&[255, 128, 0], [10, 20, 30, 255]);
    assert_eq!(&solid[0..4], &[10, 20, 30, 255]);
    assert_eq!(&solid[4..8], &[10, 20, 30, 128]);
}

/// text-overflow: ellipsis (blitz#888): fitting text returns None (the
/// marker only renders on overflow, Chrome parity); overflowing text
/// keeps the longest prefix whose ink plus the marker fits the limit,
/// drops the trailing space, and the marker uses the run's own font
/// params (U+2026, covered by the fallback chain).
#[test]
fn truncate_tokens_clips_to_limit_and_appends_marker() {
    let fonts = crate::diting_fonts::font_book();
    let tokens = tokens_of("alpha beta gamma delta", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal, false, None);
    let total: f32 = tokens.iter().map(|t| t.width).sum();

    assert!(
        truncate_tokens(&tokens, total + 1.0, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal, None).is_none(),
        "fitting text is untouched"
    );

    let limit = total / 2.0;
    let (kept, marker) = truncate_tokens(&tokens, limit, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal, None)
        .expect("overflowing text truncates");
    let kept_w: f32 = kept.iter().map(|t| t.width).sum();
    let marker_w: f32 = marker.iter().map(|t| t.width).sum();
    assert!(marker_w > 0.0 && marker.iter().any(|t| t.text.contains('\u{2026}')));
    assert!(
        kept_w + marker_w <= limit + f32::EPSILON,
        "kept {kept_w} + marker {marker_w} within {limit}"
    );
    assert!(kept.len() < tokens.len(), "prefix is strictly shorter");
    assert!(
        !kept.last().map(|t| t.is_space).unwrap_or(false),
        "trailing space is popped"
    );

    // A bold run's marker must measure wider than the normal run's —
    // font params flow through.
    let (_, bold_marker) = truncate_tokens(&tokens, limit, 16.0, true, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal, None).unwrap();
    let bold_w: f32 = bold_marker.iter().map(|t| t.width).sum();
    assert!(bold_w > marker_w, "bold marker measures wider ({bold_w} > {marker_w})");
}

/// Paint half of obscura#983: a pre-shaped token slice must rasterize
/// byte-identically to re-shaping from scratch — same wrap lines, same
/// ellipsis truncation, same tile.
#[test]
fn rasterize_wrapped_pre_shaped_matches_reshaped() {
    let fonts = crate::diting_fonts::font_book();
    let text = "淘宝商品列表页的一段中文文本需要折行处理".repeat(4);
    let tokens = tokens_of(&text, 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Normal, false, None);

    let plain = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, None, false, None);
    let pre = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, None, crate::diting_css::WhiteSpace::Normal, Some(&tokens), false, None);
    assert_eq!((plain.width, plain.height, plain.baseline), (pre.width, pre.height, pre.baseline));
    assert_eq!(plain.data, pre.data);

    let plain_t = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, Some(320.0), crate::diting_css::WhiteSpace::Normal, None, false, None);
    let pre_t = fonts.rasterize_wrapped_uncached(&text, 16.0, false, [0, 0, 0, 255], 200.0, 24.0, false, 0.0, Some(320.0), crate::diting_css::WhiteSpace::Normal, Some(&tokens), false, None);
    assert_eq!((plain_t.width, plain_t.height, plain_t.baseline), (pre_t.width, pre_t.height, pre_t.baseline));
    assert_eq!(plain_t.data, pre_t.data);
}

// ---- white-space pre family (batch 106) ----

fn ws_lines(text: &str, ws: crate::diting_css::WhiteSpace, wrap_at: Option<f32>) -> Vec<WrapLine> {
    let fonts = crate::diting_fonts::font_book();
    let tokens = tokens_of(text, 16.0, false, &fonts, false, 0.0, ws, false, None);
    greedy_wrap(&tokens, wrap_at, ws)
}

/// `pre` tokenization preserves every space as its own token, expands a
/// tab to 8 columns, and models CRLF (and lone CR) as one break.
#[test]
fn pre_tokens_preserve_whitespace_and_breaks() {
    let fonts = crate::diting_fonts::font_book();
    let toks = tokens_of("a  b\tc\r\nd", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::Pre, false, None);
    let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, vec!["a", " ", " ", "b", "        ", "c", "\n", "d"]);
    assert!(toks[6].is_break && toks[6].width == 0.0);
    assert!(toks[4].is_space && toks[4].text.chars().all(|c| c == ' '));
}

/// `pre` wraps only on hard breaks; blank source lines stay blank lines.
#[test]
fn pre_wraps_on_newlines_and_keeps_blank_lines() {
    let lines = ws_lines("a\n\nb", crate::diting_css::WhiteSpace::Pre, None);
    assert_eq!(lines.len(), 3, "a, blank, b");
    assert_eq!(lines[1].width, 0.0, "the middle line is empty, not merged");
    // No soft wrap at all: an arbitrarily long run stays one line even
    // with a tiny wrap_at handed in (defensive — measure passes None).
    let lines = ws_lines(&"x".repeat(200), crate::diting_css::WhiteSpace::Pre, Some(10.0));
    assert_eq!(lines.len(), 1, "pre has no soft wrap opportunities");
}

/// `pre-wrap`: an overflowing space HANGS at the line end; only a word
/// breaks down.
#[test]
fn pre_wrap_spaces_hang_at_line_end() {
    let fonts = crate::diting_fonts::font_book();
    let toks = tokens_of("aa   bb", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::PreWrap, false, None);
    let (w_aa, sp) = (toks[0].width, toks[1].width);
    let wrap_at = w_aa + 2.5 * sp; // "aa  " fits, the 3rd space hangs, "bb" breaks
    let lines = ws_lines("aa   bb", crate::diting_css::WhiteSpace::PreWrap, Some(wrap_at));
    assert_eq!(lines.len(), 2, "one break, before bb");
    assert_eq!(lines[0].token_idx, vec![0, 1, 2, 3], "aa + all three spaces hang on line 1");
    assert_eq!(lines[1].token_idx, vec![4], "bb alone on line 2");
}

/// `break-spaces`: nothing hangs — the overflowing space itself starts
/// the next line, so preserved spaces can end a line.
#[test]
fn break_spaces_wraps_space_tokens_down() {
    let fonts = crate::diting_fonts::font_book();
    let toks = tokens_of("aa   bb", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::BreakSpaces, false, None);
    let w_aa = toks[0].width;
    let lines = ws_lines("aa   bb", crate::diting_css::WhiteSpace::BreakSpaces, Some(w_aa + 0.5));
    assert_eq!(lines[0].token_idx, vec![0], "aa alone — the first space already overflows");
    let spaced: Vec<usize> = lines[1..lines.len() - 1].iter().flat_map(|l| l.token_idx.iter().copied()).collect();
    assert_eq!(spaced, vec![1, 2, 3], "the three spaces wrap down together");
    assert_eq!(lines.last().unwrap().token_idx, vec![4], "bb on the last line");
}

/// `pre-line`: spaces collapse within each newline-separated segment and
/// newlines are hard breaks (the tokenizer keeps a trailing break; the
/// breaker is what drops its line box).
#[test]
fn pre_line_collapses_spaces_but_breaks_on_newlines() {
    let fonts = crate::diting_fonts::font_book();
    let toks = tokens_of("  a  b  \n c\nd  \n", 16.0, false, &fonts, false, 0.0, crate::diting_css::WhiteSpace::PreLine, false, None);
    let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, vec!["a", " ", "b", "\n", "c", "\n", "d", "\n"], "collapsed segments, breaks between, trailing break kept for the breaker");
    let lines = ws_lines("  a  b  \n c\nd  \n", crate::diting_css::WhiteSpace::PreLine, None);
    let line_texts: Vec<Vec<&str>> = lines
        .iter()
        .map(|l| l.token_idx.iter().map(|&i| texts[i]).collect())
        .collect();
    assert_eq!(line_texts, vec![vec!["a", " ", "b"], vec!["c"], vec!["d"]], "trailing newline opens no empty line");
}

/// A trailing newline in any preserved mode ends the last line instead
/// of adding a phantom empty one; a doubled trailing newline still keeps
/// exactly one blank line.
#[test]
fn trailing_newline_drops_not_doubles() {
    assert_eq!(ws_lines("a\n", crate::diting_css::WhiteSpace::Pre, None).len(), 1);
    assert_eq!(ws_lines("a\n\n", crate::diting_css::WhiteSpace::Pre, None).len(), 2);
    assert_eq!(ws_lines("a\n", crate::diting_css::WhiteSpace::BreakSpaces, None).len(), 1);
    // Un-touched content (no break) never pops its only line.
    assert_eq!(ws_lines("", crate::diting_css::WhiteSpace::Pre, None).len(), 1);
}

// ---- #139: Han-unification slot routing ----

/// A ja-slot run routes unified ideographs to the first same-slot face (the
/// ja fallback here), Latin keeps the primary pair, and every non-ja slot —
/// or no slot at all — keeps the exact pre-#139 cascade (primary covers 直,
/// so zh/tc/None all stay Primary). The fixture's 直 glyph is the SC pair's
/// own outline (same-source subset), so the assertion is at the FaceSel
/// routing level — the honest contract per the issue ("unit-test the
/// font-selection branch").
#[test]
fn han_slot_routes_ideographs_to_same_slot_face() {
    let (reg, bold) = production_pair();
    let ja = include_bytes!("../../diting_fonts/test-han-ja-face.ttf").to_vec();
    let book = FontBook::from_pairs(reg, bold).unwrap().with_fallbacks(vec![ja]);

    let ja_run = book.segments("直A", false, false, Some(HanSlot::Japanese));
    assert_eq!(ja_run[0], (FaceSel::Fallback(0), "直"), "ja ideograph routes to the ja face");
    assert_eq!(ja_run[1], (FaceSel::Primary, "A"), "Latin never re-fonts");

    let plain = book.segments("直A", false, false, None);
    assert!(plain.iter().all(|(sel, _)| *sel == FaceSel::Primary), "no lang: plain cascade");
    let zh = book.segments("直A", false, false, Some(HanSlot::Simplified));
    assert!(zh.iter().all(|(sel, _)| *sel == FaceSel::Primary), "primary IS the SC slot");
    let tc = book.segments("直A", false, false, Some(HanSlot::Traditional));
    assert!(tc.iter().all(|(sel, _)| *sel == FaceSel::Primary), "no tc face: fall through");
}

/// The slot rides the raster-cache identity (#139): same text, same size,
/// different slot = different glyphs = different tile; the same slot still
/// hits the cached Arc.
#[test]
fn han_slot_rides_the_raster_key() {
    let (reg, bold) = production_pair();
    let ja = include_bytes!("../../diting_fonts/test-han-ja-face.ttf").to_vec();
    let book = FontBook::from_pairs(reg, bold).unwrap().with_fallbacks(vec![ja]);
    let _held = isolated();
    let zh = book.rasterize("直", 16.0, false, [0, 0, 0, 255], 24.0, false, Some(HanSlot::Simplified));
    let ja_r = book.rasterize("直", 16.0, false, [0, 0, 0, 255], 24.0, false, Some(HanSlot::Japanese));
    assert!(!Arc::ptr_eq(&zh, &ja_r), "slot must ride the key");
    let zh2 = book.rasterize("直", 16.0, false, [0, 0, 0, 255], 24.0, false, Some(HanSlot::Simplified));
    assert!(Arc::ptr_eq(&zh, &zh2), "same slot still caches");
}
