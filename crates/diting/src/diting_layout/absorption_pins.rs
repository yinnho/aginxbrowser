//! Layout pins from competitor-absorption batches (moli/obscura/blitz).
//! fork_deltas.rs is grandfathered-shrink-only, so absorption pins land here.
/// moli#804 / #158: `white-space: nowrap` must suppress the soft break
/// BETWEEN atomic inlines too, not just inside text measure — two 100px
/// inline-blocks in a 150px nowrap container stay on one line and
/// overflow (Chrome overflows; it never shrinks the atoms to fit).
#[test]
fn nowrap_pins_atomic_inlines_to_one_line() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    let html = |ws: &str| {
        format!(r#"<html><head><style>
            body {{ margin: 0; }}
            #box {{ width: 150px; white-space: {ws}; }}
            .b {{ display: inline-block; width: 100px; height: 10px; background: #333; }}
        </style></head><body><div id="box"><span class="b" id="a"></span><span class="b" id="b"></span></div></body></html>"#)
    };
    let rect = |html: String, sel: &str| -> (f32, f32) {
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(
            &tree.query_selector_all("style").map(|els| {
                els.iter().map(|&el| tree.text_content(el)).collect::<Vec<_>>().join("\n")
            }).unwrap_or_default(),
            (400.0, 300.0),
            CssMediaType::Screen,
        );
        let styles = crate::diting_layout::compute_styles(&tree, &rules, (400.0, 300.0));
        let rects = crate::diting_layout::layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), 400.0, 300.0);
        let id = tree.query_selector_all(sel).unwrap()[0];
        let r = rects.get(&id).unwrap();
        (r.x, r.y)
    };

    let (ax, ay) = rect(html("nowrap"), "#a");
    let (bx, by) = rect(html("nowrap"), "#b");
    assert!(
        (by - ay).abs() < 1.0,
        "nowrap keeps both inline-blocks on one line: a=({ax},{ay}) b=({bx},{by})"
    );
    assert!((bx - (ax + 100.0)).abs() < 1.0, "second atom follows the first, not shrunk");

    let (_, ny) = rect(html("normal"), "#b");
    assert!(ny > ay + 5.0, "control: normal DOES break between the atoms: y={ny}");
}

/// obscura#1087 / #159: a bare `<button>`'s intrinsic (shrink-to-fit) width
/// must carry the UA horizontal chrome — label advance + 12px padding +
/// 2×2px border. Chrome renders `<button>OK</button>` (UA font 13.3333px
/// sans) ≈ 35px wide × 25px tall; before the UA layer landed, the button
/// measured at the bare label advance (~19px) and the ink clipped.
#[test]
fn button_intrinsic_width_carries_ua_chrome() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><head><style>body { margin: 0; }</style></head><body>
        <button id="btn">OK</button>
        <button id="btn2">OK Submitting</button>
    </body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("body { margin: 0; }", (400.0, 300.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (400.0, 300.0));
    let rects = crate::diting_layout::layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), 400.0, 300.0);

    let id = tree.query_selector_all("#btn").unwrap()[0];
    let r = rects.get(&id).unwrap();
    assert!(
        (r.width - 35.0).abs() <= 2.0 && (r.height - 25.0).abs() <= 2.0,
        "button ≈ Chrome's 35×25 (label + 12px padding + 2×2px border), got {}×{}",
        r.width, r.height
    );

    // The chrome is additive: a longer label widens the button by the added
    // text advance — " Submitting" at 13.3333px is far more than the 16px
    // of fixed chrome, so a measure/paint font mismatch can't fake this.
    let id2 = tree.query_selector_all("#btn2").unwrap()[0];
    let r2 = rects.get(&id2).unwrap();
    assert!(r2.width - r.width > 60.0, "width tracks the label advance: {} → {}", r.width, r2.width);
}

/// #206 (blitz #1003 absorption): UA list markers did not exist at all —
/// every bullet/number on unstyled lists was missing ink. v1 paints the
/// marker as text OUTSIDE the li's border box (hanging off the outer left
/// edge), sharing the first line's typography; `list-style: none` (and
/// `list-style-type: none`) suppresses it; `<ol>` numbers 1., 2. by
/// same-parent sibling count.
#[test]
fn outside_list_markers_paint_outside_the_border_box() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_layout::PaintItem;
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><body style="margin:0">
        <ul style="padding-left:0"><li id="a" style="border:2px solid red;width:120px">item</li></ul>
        <ul style="padding-left:0"><li id="b">one</li><li id="c">two</li></ul>
        <ol style="padding-left:0"><li id="d">first</li><li id="e">second</li></ol>
        <ul style="padding-left:0;list-style:none"><li id="f">plain</li></ul>
        </body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (1280.0, 800.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (rects, items) = crate::diting_layout::layout_dom_with_paint(
        &tree, &styles, &crate::diting_fonts::font_book(), 1280.0, 800.0,
    );
    let li_rect = |sel: &str| {
        let id = tree.query_selector_all(sel).unwrap()[0];
        rects.get(&id).copied().unwrap_or_else(|| panic!("{sel} owns a box"))
    };
    let marker_left_edge = |item_text: &str| -> Option<f32> {
        items.iter().find_map(|it| match it {
            PaintItem::Text { text, x, .. } if text == item_text => Some(*x),
            _ => None,
        })
    };
    // The bullet hangs OUTSIDE the bordered li: its whole advance starts
    // left of the border box (Chrome: ink at border.x-12..-7).
    let a = li_rect("#a");
    let bullet_x = marker_left_edge("\u{2022}").expect("disc bullet paints");
    assert!(
        bullet_x < a.x - 6.0,
        "outside marker starts left of the border box (got x={bullet_x}, li.x={}): {a:?}",
        a.x
    );
    // Two-lis list: exactly two bullets.
    let bullets = items.iter().filter(|it| matches!(it, PaintItem::Text { text, .. } if text == "\u{2022}")).count();
    assert_eq!(bullets, 3, "three disc items across #a/#b/#c: {bullets}");
    // ol numbers by sibling position.
    let one = marker_left_edge("1.").expect("ol first marker");
    let two = marker_left_edge("2.").expect("ol second marker");
    let d = li_rect("#d");
    assert!(one < d.x, "decimal marker outside: one={one} li.x={}", d.x);
    assert_eq!(one, two, "tabular digits: 1. and 2. share the advance, so the same edge");
    // list-style:none kills the marker: the bullets total stays at three
    // (#a/#b/#c; the reset list adds none), and the reset list's own text
    // run still paints.
    assert!(items.iter().any(|it| matches!(it, PaintItem::Text { text, .. } if text == "plain")),
        "the reset list's text still paints");
}

/// #214 (blitz#998): `text-align-last` moves ONLY the run's last line.
/// The offset lives inside the rasterized tile (nothing changes at the
/// item level), so the probe reads ink columns per line band out of two
/// rasters of the same two-line run — plain vs `last_line_align: Right`:
/// the first line's ink must stay byte-identical while the last line's
/// ink shifts right by the line's slack.
#[test]
fn text_align_last_moves_only_the_last_line_ink() {
    let fonts = crate::diting_fonts::font_book();
    // Two lines at wrap 100: "aaaa aaaa aaaa" breaks after the second word
    // group — whatever the exact break, ink lands in two distinct bands.
    let text = "aaaa aaaa aaaa";
    let lh = 24.0f32;
    let plain = fonts.rasterize_wrapped(
        text, 16.0, false, [0, 0, 0, 255], 100.0, lh, false, 0.0, None,
        crate::diting_css::WhiteSpace::Normal, false, None, None,
    );
    let right = fonts.rasterize_wrapped(
        text, 16.0, false, [0, 0, 0, 255], 100.0, lh, false, 0.0, None,
        crate::diting_css::WhiteSpace::Normal, false, None,
        Some(crate::diting_css::TextAlign::Right),
    );
    // (min_x, max_x) ink columns of the rows in [y0, y1).
    let band_cols = |r: &crate::diting_layout::text::TextRaster, y0: usize, y1: usize| -> (usize, usize) {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for y in y0..y1.min(r.height) {
            for x in 0..r.width {
                if r.data[(y * r.width + x) * 4 + 3] > 0 {
                    lo = lo.min(x);
                    hi = hi.max(x);
                }
            }
        }
        (lo, hi)
    };
    assert!(plain.height > lh as usize, "the run must wrap to two lines (h={})", plain.height);
    let mid = lh as usize;
    let (p0, p1) = (band_cols(&plain, 0, mid), band_cols(&plain, mid, mid * 2));
    let (r0, r1) = (band_cols(&right, 0, mid), band_cols(&right, mid, mid * 2));
    assert_eq!((p0.0, p0.1), (r0.0, r0.1), "the FIRST line's ink is untouched");
    assert!(
        r1.0 > p1.0 && r1.1 > p1.1,
        "the LAST line moves right (plain {p1:?} → right {r1:?})"
    );
    // Center halves the slack: its offset sits strictly between plain and
    // right, and the tile never widens (the offset anchors inside it).
    let center = fonts.rasterize_wrapped(
        text, 16.0, false, [0, 0, 0, 255], 100.0, lh, false, 0.0, None,
        crate::diting_css::WhiteSpace::Normal, false, None,
        Some(crate::diting_css::TextAlign::Center),
    );
    let (_, c1) = (band_cols(&center, 0, mid), band_cols(&center, mid, mid * 2));
    assert!(c1.0 > p1.0 && c1.0 < r1.0, "center sits between start and right (plain {} center {} right {})", p1.0, c1.0, r1.0);
    assert_eq!(plain.width, right.width, "alignment changes no tile geometry");
}

/// #214 collect wiring: the run's PaintItem carries the owning block's
/// resolved `text-align-last` (inherited down, physical fold included),
/// and `text-align: justify` parses (consumed as start — no promote, no
/// last-line override) without breaking the block's own run.
#[test]
fn text_align_last_resolves_onto_the_run_item() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType, TextAlign};
    use crate::diting_layout::PaintItem;
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><body style="margin:0">
        <div id="r" style="width:120px;text-align:justify;text-align-last:right">aaaa aaaa aaaa</div>
        <div id="c" style="width:120px">head <span style="text-align-last:center">tail</span></div>
        </body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (1280.0, 800.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    // Declared keyword round-trips in the cascade; justify parses on
    // text-align itself.
    let rid = tree.query_selector_all("#r").unwrap()[0];
    let rs = styles.get(&rid).expect("#r styles");
    assert_eq!(rs.text_align, Some(TextAlign::Justify), "text-align: justify parses");
    assert_eq!(rs.text_align_last, Some(crate::diting_css::TextAlignLast::Right));
    // Inheritance: the span declares its own value; #c's block runs carry
    // none (the property targets the run's owning block — the span's
    // inline style rides its own run machinery, word-leaf v1 boundary).
    let sid = tree.query_selector_all("#c span").unwrap()[0];
    assert_eq!(
        styles.get(&sid).unwrap().text_align_last,
        Some(crate::diting_css::TextAlignLast::Center)
    );
    // The justify paragraph's run item resolves last:right; an undeclared
    // block's run resolves None (auto).
    let (_, items, _, _, _, _) = crate::diting_layout::layout_dom_with_paint_order_and_images(
        &tree, &styles, &crate::diting_fonts::font_book(), 1280.0, 800.0, None, None,
    );
    let run_align = |needle: &str| -> Option<Option<TextAlign>> {
        items.iter().find_map(|it| match it {
            PaintItem::Text { text, last_line_align, .. } if text.contains(needle) => Some(*last_line_align),
            _ => None,
        })
    };
    assert_eq!(
        run_align("aaaa aaaa aaaa"),
        Some(Some(TextAlign::Right)),
        "the justify+last:right paragraph's run carries the physical override"
    );
    assert_eq!(run_align("head").flatten(), None, "an undeclared block stays auto (no override)");
}
