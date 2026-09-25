    // Table-layout fork deltas (tmall publish P0 #107/#113 + the table
// background-paint family), split from fork_deltas.rs at the god-file
// ratchet's demand. Self-contained tests, crate-pathed imports only.

/// The Fusion `.next-input` shape (tmall publish P0, #107 follow-up): the
/// root span declares `display: inline-table; width: 200px` and puts
/// `display: table-cell` spans DIRECTLY inside — no tr anywhere. Before
/// inline-table parsed, the declaration dropped and the span collapsed to
/// inline (inputs went zero-width); after it parsed but before anonymous
/// ROW synthesis, the cells matched no row arm and vanished. Both halves
/// must hold: the span keeps its authored 200px and the input inside the
/// cell gets real width.
#[test]
fn inline_table_with_bare_cells_lays_out_fusion_input() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><body><div>
        <span class="next-input"><span class="inner"><input id="q"></span></span>
        </div></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for(
        ".next-input { display: inline-table; width: 200px; }\
         .inner { display: table-cell; width: 1px; }\
         .next-input input { width: 100%; }",
        (1280.0, 800.0),
        CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let rects = crate::diting_layout::layout_dom(
        &tree, &styles, &crate::diting_fonts::font_book(), 1280.0, 800.0,
    );
    let span_id = tree.query_selector_all(".next-input").unwrap()[0];
    let span = rects
        .get(&span_id)
        .expect("inline-table span owns a box");
    assert!(
        (span.width - 200.0).abs() < 2.0,
        "authored table width holds; got {span:?}"
    );
    let inp_id = tree.query_selector_all("#q").unwrap()[0];
    let inp = rects.get(&inp_id).expect("input owns a box");
    assert!(
        inp.width > 150.0,
        "width:100% input resolves against the cell, not nothing; got {inp:?}"
    );
    assert!(inp.height > 15.0, "input keeps its default height; got {inp:?}");
}

/// CSS2.2 §17.2.1's anonymous-row half on a real table: a cell directly
/// inside `<table>` (no tr) used to match no row arm and produce no box —
/// the content was invisible. It must wrap into one anonymous row and paint.
#[test]
fn table_bare_cell_wraps_into_anonymous_row() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body><table><td>loose</td></table></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (1280.0, 800.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree,
        &styles,
        &crate::diting_fonts::font_book(),
        1280.0,
        800.0,
    );
    assert!(
        items
            .iter()
            .any(|it| matches!(it, PaintItem::Text { text, .. } if text == "loose")),
        "bare td text must paint via its anonymous row; got {items:?}"
    );
}

/// Row-group backgrounds (blitz#346): thead/tbody/tfoot now get their own
/// wrapper boxes (the sticky-group batch keyed them into node_map), so a
/// background-color on the group paints on the group's band — under the
/// rows, which are its taffy children (CSS2.1's cell > row > row-group
/// order). The earlier paint-time DOM climb is gone with the boxless group.
#[test]
fn row_group_background_paints_on_row_band() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body><table><tbody><tr><td>x</td></tr></tbody></table></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for(
        "tbody { background-color: #ff0000; }",
        (1280.0, 800.0),
        CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree,
        &styles,
        &crate::diting_fonts::font_book(),
        1280.0,
        800.0,
    );

    let bands: Vec<crate::diting_layout::Rect> = items
        .iter()
        .filter_map(|it| match it {
            PaintItem::Bg { rect, color, .. } if *color == [0xff, 0x00, 0x00, 0xff] => {
                Some(*rect)
            }
            _ => None,
        })
        .collect();
    assert_eq!(bands.len(), 1, "tbody bg paints exactly one band; got {bands:?}");
    let band = bands[0];

    // The band is the row's, not the cell's: it underlaps the cell text and
    // spans the full table width.
    let (tx, ty) = items
        .iter()
        .find_map(|it| match it {
            PaintItem::Text { text, x, y, .. } if text == "x" => Some((*x, *y)),
            _ => None,
        })
        .expect("cell text paints");
    assert!(
        band.x <= tx
            && tx <= band.x + band.width
            && band.y <= ty
            && ty <= band.y + band.height,
        "band must underlap the cell text; band={band:?} text=({tx},{ty})"
    );
}

/// Same coverage through thead — the tag match covers all three row-group
/// elements, thead is the one a typo'd match arm would silently drop.
#[test]
fn thead_background_paints_on_row_band() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body><table><thead><tr><th>h</th></tr></thead></table></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for(
        "thead { background-color: #00ff00; }",
        (1280.0, 800.0),
        CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree,
        &styles,
        &crate::diting_fonts::font_book(),
        1280.0,
        800.0,
    );

    let bands = items
        .iter()
        .filter(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0x00, 0xff, 0x00, 0xff]))
        .count();
    assert_eq!(bands, 1, "thead bg paints exactly one band; got {bands}");
}

/// Cell background over row-group background: with both set, the cell's own
/// green paints ON TOP of the climbed red band (CSS2.1 cell > row-group),
/// and the red band stays under it.
#[test]
fn cell_background_paints_over_row_group_band() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body><table><tbody><tr><td class="hl">x</td></tr></tbody></table></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for(
        "tbody { background-color: #ff0000; } td.hl { background-color: #00cc00; }",
        (1280.0, 800.0),
        CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree,
        &styles,
        &crate::diting_fonts::font_book(),
        1280.0,
        800.0,
    );

    let red_idx = items
        .iter()
        .position(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0xff, 0x00, 0x00, 0xff]))
        .expect("climbed tbody band missing");
    let green_idx = items
        .iter()
        .position(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0x00, 0xcc, 0x00, 0xff]))
        .expect("cell's own bg missing");
    assert!(
        red_idx < green_idx,
        "cell bg ({green_idx}) must paint over the row-group band ({red_idx})"
    );
}

/// Row's own background wins over the row-group's: with both set, the
/// group's red band paints on the group box UNDER the row's own blue
/// (CSS2.1 row > row-group) — the red is the group's own box, not a leak
/// onto the row band.
#[test]
fn row_background_beats_row_group_background() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::PaintItem;

    let html = r#"<html><body><table><tbody><tr class="r"><td>x</td></tr></tbody></table></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for(
        "tbody { background-color: #ff0000; } tr.r { background-color: #0000ff; }",
        (1280.0, 800.0),
        CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    let (_, items) = crate::diting_layout::layout_dom_with_paint(
        &tree,
        &styles,
        &crate::diting_fonts::font_book(),
        1280.0,
        800.0,
    );

    let reds: Vec<_> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it, PaintItem::Bg { color, .. } if *color == [0xff, 0x00, 0x00, 0xff]))
        .map(|(i, _)| i)
        .collect();
    let blues: Vec<_> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it, PaintItem::Bg { color, .. } if *color == [0x00, 0x00, 0xff, 0xff]))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(reds.len(), 1, "the group's own red box paints once; got {reds:?}");
    assert_eq!(blues.len(), 1, "the row's own blue paints once; got {blues:?}");
    assert!(
        reds[0] < blues[0],
        "group red ({reds:?}) must paint under the row's blue ({blues:?})"
    );
}
