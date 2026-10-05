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

/// #218: clip:text on a wrapping INLINE span — the span owns no taffy box,
/// so the box-walk capture can't see it; the fill must reach the run leaf
/// via the inline-chain walk and sample over the run's own line geometry.
/// Also pins the band-path suppression: no opaque BgGradient covers the
/// glyphs (that was the pre-fix page).
#[test]
fn clip_text_inline_span_wraps_with_gradient() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body style="margin:0">
<div style="margin:0;width:130px;font-size:28px;background:#eee">
<span id="w" style="background-image:linear-gradient(90deg,red,blue);-webkit-background-clip:text;color:rgb(0,128,0)">MMMMM MMMMM MMMMM</span>
</div>
</body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (400.0, 300.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (400.0, 300.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree, &styles, &crate::diting_fonts::font_book(), 400.0, 300.0,
    );

    let g = items
        .iter()
        .find_map(|it| match it {
            PaintItem::Text { text, gradient: Some(g), .. } if text.contains('M') => Some(g.clone()),
            _ => None,
        })
        .expect("wrapping inline span's run carries the clip:text fill");
    assert!(g.clone_lines.is_empty(), "slice mode (initial) keeps one continuous area");
    assert!((g.area.width - 113.68).abs() < 1.0, "area = the WIDEST wrapped line, not the full text: {:?}", g.area);
    assert!(g.area.height > 80.0, "area spans all wrapped lines: {:?}", g.area);
    assert!(
        !items.iter().any(|it| matches!(it, PaintItem::BgGradient { .. })),
        "the span's own band gradient is suppressed — nothing covers the glyphs"
    );

    // Pixel: line 1 runs red→blue across its own extent.
    let mut c = crate::diting_layout::paint::Canvas::new_filled(200, 200, [255, 255, 255, 255]);
    crate::diting_layout::paint::execute(&items, &crate::diting_fonts::font_book(), &mut c);
    let p = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * c.width + x) * 4;
        c.data[i..i + 4].try_into().unwrap()
    };
    let (mut red_max, mut blue_min) = (0usize, usize::MAX);
    for y in 0..40 {
        for x in 0..130 {
            let [r, g_, b, a] = p(x, y);
            if a > 200 && r > 120 && b < 120 && g_ < 120 {
                red_max = red_max.max(x);
            } else if a > 200 && b > 120 && r < 120 && g_ < 120 {
                blue_min = blue_min.min(x);
            }
        }
    }
    assert!(red_max > 5, "line 1 starts red (red_max={red_max})");
    assert!(blue_min < 110 && blue_min > red_max, "line 1 ends blue right of the red (blue_min={blue_min})");
}

/// #218: `box-decoration-break: clone` restarts the gradient at EVERY
/// wrapped line. The visible knife needs a SHORT last line: slice mode
/// samples it at the strip's left end (all red ink — its ink never reaches
/// the blue end), clone mode gives the short line its own box and it runs
/// red→blue across its own extent.
#[test]
fn box_decoration_clone_restarts_gradient_per_line() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = |bdb: &str| {
        format!(r#"<html><body style="margin:0"><div style="margin:0;width:130px;font-size:28px">
<span id="w" style="background-image:linear-gradient(90deg,red,blue);-webkit-background-clip:text;color:rgb(0,128,0);box-decoration-break:{bdb}">MMMMM MMMMM MM</span>
</div></body></html>"#)
    };
    let paint = |html: String| {
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for("", (400.0, 300.0), CssMediaType::Screen);
        let styles = crate::diting_layout::compute_styles(&tree, &rules, (400.0, 300.0));
        crate::diting_layout::layout_dom_with_paint(&tree, &styles, &crate::diting_fonts::font_book(), 400.0, 300.0).1
    };
    let clone_items = paint(html("clone"));
    let slice_items = paint(html("slice"));

    let text_fill = |items: &Vec<PaintItem>| {
        items
            .iter()
            .find_map(|it| match it {
                PaintItem::Text { text, gradient: Some(g), .. } if text.contains('M') => Some(g.clone()),
                _ => None,
            })
            .expect("run carries the clip:text fill")
    };
    let g = text_fill(&clone_items);
    assert!(g.clone_per_line && g.clone_lines.len() == 3, "one fragment box per wrapped line: {:?}", g.clone_lines);

    // Ink-class counters over a canvas, per line band and x window.
    let scan = |items: &Vec<PaintItem>| {
        let mut c = crate::diting_layout::paint::Canvas::new_filled(200, 200, [255, 255, 255, 255]);
        crate::diting_layout::paint::execute(items, &crate::diting_fonts::font_book(), &mut c);
        move |y0: usize, y1: usize, x0: usize, x1: usize| -> (usize, usize) {
            let (mut reds, mut blues) = (0usize, 0usize);
            for y in y0..y1.min(200) {
                for x in x0..x1.min(200) {
                    let i = (y * c.width + x) * 4;
                    let [r, g_, b, a] = c.data[i..i + 4].try_into().unwrap();
                    if a > 200 && r > 120 && b < 120 && g_ < 120 {
                        reds += 1;
                    } else if a > 200 && b > 120 && r < 120 && g_ < 120 {
                        blues += 1;
                    }
                }
            }
            (reds, blues)
        }
    };
    let clone_scan = scan(&clone_items);
    for (line, (_, w)) in g.clone_lines.iter().enumerate() {
        let (y0, y1) = (line * 40, (line + 1) * 40);
        let w = *w as usize;
        let (reds, blues) = clone_scan(y0, y1, 0, w / 4);
        let (reds2, blues2) = clone_scan(y0, y1, w * 7 / 10, w);
        assert!(
            reds + reds2 > 3 && blues + blues2 > 3,
            "clone: line {line} restarts red and ends blue within its own {w}px box (reds={} blues={})",
            reds + reds2,
            blues + blues2
        );
    }

    // Control: the SHORT last line in slice mode never reaches the strip's
    // blue end — its whole ink sits at t < 0.5.
    let sg = text_fill(&slice_items);
    assert!(sg.clone_lines.is_empty(), "slice stays one continuous strip");
    let last_w = g.clone_lines.last().unwrap().1 as usize;
    let slice_scan = scan(&slice_items);
    let (reds, blues) = slice_scan(80, 120, last_w * 7 / 10, last_w + 40);
    assert!(
        reds > 3 && blues == 0,
        "slice: short line 3 stays at the strip's red end (reds={reds} blues={blues})"
    );
}

/// #218 pierce knives: clip:text on a BOX reaches every descendant run —
/// direct text, nested inline spans, a nested block, and underlined text —
/// and the decoration strokes stay SOLID (Chrome paints decorations in the
/// text's own color, not the gradient).
#[test]
fn clip_text_pierces_nested_runs_decoration_stays_solid() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body style="margin:0">
<div id="c" style="margin:0;width:420px;font-size:28px;background-image:linear-gradient(90deg,red,blue);-webkit-background-clip:text;color:rgb(0,128,0)">
L1-direct <span id="s1">S1-span <span id="s2">S2-deep</span></span>
<div id="nb" style="font-size:28px">NESTEDBOX</div>
<span id="dec" style="text-decoration:underline">UNDERLINED</span>
</div>
</body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (400.0, 300.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (400.0, 300.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree, &styles, &crate::diting_fonts::font_book(), 400.0, 300.0,
    );
    for needle in ["L1-direct", "S1-span", "S2-deep", "NESTEDBOX", "UNDERLINED"] {
        assert!(
            items.iter().any(|it| matches!(it, PaintItem::Text { text, gradient: Some(_), .. } if text.contains(needle))),
            "{needle} carries the ancestor's clip:text fill"
        );
    }

    // The underline strokes solid green — the only pure-green ink on the
    // page (the glyph fills are the red→blue gradient).
    let mut c = crate::diting_layout::paint::Canvas::new_filled(500, 240, [255, 255, 255, 255]);
    crate::diting_layout::paint::execute(&items, &crate::diting_fonts::font_book(), &mut c);
    let mut green = 0usize;
    for y in 0..240 {
        for x in 0..500 {
            let i = (y * c.width + x) * 4;
            let [r, g_, b, a] = c.data[i..i + 4].try_into().unwrap();
            if a > 200 && g_ > 90 && r < 90 && b < 90 {
                green += 1;
            }
        }
    }
    assert!(green > 100, "underline paints solid green, outside the gradient: {green}px");
}

/// #218 (takumi #1802): the element that DECLARES the decoration owns its
/// fill. When it also clips a gradient through its text, Chrome strokes
/// the lines WITH the gradient — and an explicit `text-decoration-color`
/// loses (Chrome probes 2026-10-05: `text-decoration-color:green` still
/// painted red→blue; only without clip does `color:transparent` hide the
/// stroke). The nested pin above is the complementary shape: a plain
/// inner span declaring the underline under an outer clipping box stays
/// solid currentcolor.
#[test]
fn clip_text_decoration_pierces_with_gradient() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body style="margin:0">
<div id="a" style="width:340px;font-size:24px;background-image:linear-gradient(90deg,#ff0000,#0000ff);-webkit-background-clip:text;color:transparent;text-decoration:underline">underlined gradient text</div>
<div id="b" style="width:340px;font-size:24px;background-image:linear-gradient(90deg,#ff0000,#0000ff);-webkit-background-clip:text;color:transparent;text-decoration:underline;text-decoration-color:#00ff00">explicit green ignored</div>
</body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (500.0, 300.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (500.0, 300.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree, &styles, &crate::diting_fonts::font_book(), 500.0, 300.0,
    );
    for needle in ["underlined gradient text", "explicit green ignored"] {
        assert!(
            items.iter().any(|it| matches!(it, PaintItem::Text { text, decorations, gradient: Some(_), .. }
                if text.contains(needle) && decorations.pierce)),
            "{needle} carries both the clip fill and the pierce flag"
        );
    }
    let mut c = crate::diting_layout::paint::Canvas::new_filled(500, 120, [255, 255, 255, 255]);
    crate::diting_layout::paint::execute(&items, &crate::diting_fonts::font_book(), &mut c);
    // Each band's underline: the row with the longest saturated run. Its
    // left end must be red and right end blue — the gradient, not the
    // (transparent) currentcolor, and not the explicit green longhand.
    for (y0, y1) in [(0, 48), (48, 96)] {
        let mut best = (0usize, 0usize, 0usize); // (len, y, x_end)
        for y in y0..y1 {
            let (mut run, mut mx, mut end) = (0usize, 0usize, 0usize);
            for x in 0..500 {
                let i = (y * c.width + x) * 4;
                let [r, g, b, _] = c.data[i..i + 4].try_into().unwrap();
                if r.max(g).max(b) - r.min(g).min(b) > 40 {
                    run += 1;
                    if run > mx {
                        mx = run;
                        end = x;
                    }
                } else {
                    run = 0;
                }
            }
            if mx > best.0 {
                best = (mx, y, end);
            }
        }
        let (len, y, xend) = best;
        assert!(len > 200, "underline row exists in band {y0}: len={len}");
        let at = |x: usize| -> [u8; 4] {
            let i = (y * c.width + x) * 4;
            c.data[i..i + 4].try_into().unwrap()
        };
        let l = at(xend + 1 - len);
        let r = at(xend);
        assert!(l[0] > 200 && l[1] < 60 && l[2] < 60, "left end red: {l:?}");
        assert!(r[2] > 150 && r[0] < 120 && r[1] < 120, "right end blue: {r:?}");
    }
}

/// #218 (takumi #1795): `initial`/`inherit`/`unset` (and the `all`
/// shorthand) copy fields instead of parse-and-drop. Matrix pinned to
/// Chrome headless ground truth (2026-10-05, getComputedStyle dump):
/// inherited keywords take the parent's value, non-inherited keywords take
/// the initial default (`None` here), and `all:inherit` even copies the
/// parent's non-inherited `display: flex` down.
#[test]
fn css_wide_keyword_matrix_matches_chrome() {
    use crate::diting_css::{parse_stylesheet_for, Color, CssMediaType, Display};
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><head><style>
#p1{color:rgb(255,0,0);font-size:32px;font-weight:700;text-decoration:underline}
#c1{color:initial}
#c2{color:inherit}
#c3{color:unset}
#f1{display:flex;color:green;font-size:20px}
#f1i{display:initial}
#f2{all:initial}
#f3{all:unset}
#f4{all:inherit}
#b1{border-style:solid;border-width:2px;border-color:red}
#b1u{border-style:unset}
#w1{width:123px}
#w1i{width:initial}
#s1{font-size:inherit}
</style></head><body>
<div id="p1">P <span id="c1"></span><span id="c2"></span><span id="c3"></span><span id="s1"></span></div>
<div id="f1"><div id="f1i"></div><div id="f2"></div><div id="f3"></div><div id="f4"></div></div>
<div id="b1"><span id="b1u"></span></div>
<div id="w1"><span id="w1i"></span></div>
</body></html>"#;
    let tree = parse_html(html);
    let css = tree.query_selector_all("style").map(|els| {
        els.iter().map(|&el| tree.text_content(el)).collect::<Vec<_>>().join("\n")
    }).unwrap_or_default();
    let rules = parse_stylesheet_for(&css, (500.0, 600.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (500.0, 600.0));
    let s = |sel: &str| styles.get(&tree.query_selector_all(sel).unwrap()[0]).unwrap();

    // Chrome: initial color rgb(0,0,0) — None here paints the same black.
    assert_eq!(s("#c1").color, None, "color:initial drops the inherited red");
    assert_eq!(s("#c2").color, Some(Color(255, 0, 0, 255)), "color:inherit");
    assert_eq!(s("#c3").color, Some(Color(255, 0, 0, 255)), "color:unset inherits");
    assert_eq!(s("#s1").font_size, Some(32.0), "font-size:inherit");
    // Chrome: display:initial on a flex item computes block (None here).
    assert_eq!(s("#f1").display, Some(Display::Flex));
    assert_eq!(s("#f1i").display, None, "display:initial → block");
    assert_eq!(s("#f1i").color, Some(Color(0, 128, 0, 255)), "flex line inherits green");
    assert_eq!(s("#f2").color, None, "all:initial resets color to black");
    assert_eq!(s("#f2").font_size, None, "all:initial resets font-size to 16px");
    assert_eq!(s("#f2").text_decoration_line, None, "all:initial clears decorations");
    assert_eq!(s("#f3").color, Some(Color(0, 128, 0, 255)), "all:unset keeps inherited green");
    assert_eq!(s("#f3").display, None, "all:unset's display initial → block");
    // Chrome: all:inherit copies the parent's NON-inherited display:flex too.
    assert_eq!(s("#f4").display, Some(Display::Flex), "all:inherit carries display:flex down");
    assert_eq!(s("#f4").font_size, Some(20.0), "all:inherit font-size");
    assert!(s("#b1").border_style.is_some(), "solid border stays");
    assert_eq!(s("#b1u").border_style, None, "border-style:unset → none (not inherited)");
    assert!(s("#w1").width.is_some(), "width:123px parses");
    assert_eq!(s("#w1i").width, None, "width:initial → auto");
}


