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
