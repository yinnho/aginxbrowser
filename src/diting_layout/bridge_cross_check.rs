    use super::*;
    use crate::diting_css::{self, ParsedRule};
    use crate::screenshot::element_rect;

    const VW: f32 = 800.0;
    const VH: f32 = 600.0;
    /// Half a device pixel: absorbs taffy's rounding-to-integer snap, nothing
    /// else. A modeling error of ≥1px fails; float dust does not.
    const EPS: f64 = 0.51;

    /// Full cascade for every element, chaining parents (the same path
    /// screenshot::cross_check uses, inlined because that module is private).
    /// The root element's computed font-size becomes the rem base for its
    /// whole subtree (CSS root font-size semantics).
    fn our_styles(tree: &DomTree, rules: &[ParsedRule]) -> HashMap<NodeId, ComputedStyle> {
        compute_styles(tree, rules)
    }

    /// Fixture bytes BOTH sides measure with: Noto Sans SC subset (OFL —
    /// see fixtures/OFL.txt), instanced at wght 400/700. Regenerate via
    /// scripts/make_font_fixture.py.
    const FIXTURE_REGULAR: &[u8] = include_bytes!("fixtures/diting-fixture-regular.ttf");
    const FIXTURE_BOLD: &[u8] = include_bytes!("fixtures/diting-fixture-bold.ttf");
    const FIXTURE_FAMILY: &str = "DitingFixture";

    fn fixture_fonts() -> FontBook {
        FontBook::from_pairs(FIXTURE_REGULAR.to_vec(), FIXTURE_BOLD.to_vec())
            .expect("fixture fonts parse")
    }

    /// parley FontContext holding ONLY the fixture fonts: system fonts off,
    /// both weights registered under the pinned family name. Every blitz
    /// text run resolves through this collection, so the cross-check's
    /// text-derived rects are a function of the fixture glyphs — same bytes
    /// our swash side shapes, no @font-face or network plumbing.
    fn fixture_font_ctx() -> parley::FontContext {
        use parley::fontique::{
            Collection, CollectionOptions, FontInfoOverride, FontStyle, SourceCache,
        };
        let mut ctx = parley::FontContext {
            source_cache: SourceCache::new_shared(),
            collection: Collection::new(CollectionOptions {
                shared: false,
                system_fonts: false,
            }),
        };
        for (bytes, weight) in [(FIXTURE_REGULAR, 400.0), (FIXTURE_BOLD, 700.0)] {
            let blob = parley::fontique::Blob::new(std::sync::Arc::new(bytes.to_vec()));
            let info = FontInfoOverride {
                family_name: Some(FIXTURE_FAMILY),
                weight: Some(parley::fontique::FontWeight::new(weight)),
                style: Some(FontStyle::Normal),
                ..Default::default()
            };
            ctx.collection.register_fonts(blob, Some(info));
        }
        ctx
    }

    /// Blitz side of the dual run: parse + style + layout at the test
    /// viewport, then hand back the base document for element_rect.
    fn blitz_doc_unresolved(html: &str, stylesheet: &str) -> blitz_dom::BaseDocument {
        use blitz_dom::{DocumentConfig, util::Color};
        use blitz_traits::shell::{ColorScheme, Viewport};

        // Pin the family so every text run hits the fixture collection.
        let sheet = format!("body {{ font-family: {FIXTURE_FAMILY}; }}\n{stylesheet}");
        let css_doc = format!("<style>{sheet}</style>{html}");
        let mut doc = blitz_html::HtmlDocument::from_html(
            &css_doc,
            DocumentConfig {
                base_url: Some("https://example.com/".to_string()),
                net_provider: None,
                font_ctx: Some(fixture_font_ctx()),
                viewport: Some(Viewport::new(VW as u32, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            },
        );
        let _ = Color::WHITE;
        doc.into_inner()
    }

    fn blitz_doc(html: &str, stylesheet: &str) -> blitz_dom::BaseDocument {
        let mut doc = blitz_doc_unresolved(html, stylesheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        doc
    }

    fn assert_close(what: &str, ours: f32, theirs: f64) {
        assert!(
            (ours as f64 - theirs).abs() <= EPS,
            "{what}: bridge={ours} blitz={theirs} (diff {})",
            (ours as f64 - theirs).abs()
        );
    }

    fn both_engines(
        html: &str,
        stylesheet: &str,
    ) -> (
        blitz_dom::BaseDocument,
        DomTree,
        HashMap<NodeId, ComputedStyle>,
        HashMap<NodeId, Rect>,
    ) {
        let doc = blitz_doc(html, stylesheet);
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(stylesheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        (doc, tree, styles, rects)
    }

    // ── Batch 8b: float zone reified as [float | flow-column] flex row ──────
    //
    // Blitz (pinned rev, default features) has NO float support — its
    // `floats` feature is off and enabling it here would poison the product
    // taffy via cargo feature unification. So this series cross-checks
    // against hand-computed CSS expectations instead; the reference strategy
    // is upstream obscura-render's build_children_with_float_zone.

    /// The canonical shape: a floated box at the container's left edge with
    /// following siblings wrapping alongside it. Hand-computed contract:
    /// float at (0, 0) keeping its authored size/margins; the flow column's
    /// left edge = float's right margin edge; column width = container minus
    /// the float's margin box; a cleared sibling drops BELOW both and runs
    /// full width again.
    #[test]
    fn float_left_wraps_following_siblings_into_flow_column() {
        let html = r#"<body>
            <div id="fl"></div>
            <p id="p1">旁流文本</p>
            <div id="after" style="clear: both"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #fl { float: left; width: 200px; height: 300px; }
            #p1 { margin: 0; font-size: 16px; height: 40px; }
            #after { width: 500px; height: 20px; margin: 0; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let fl = tree.query_selector("#fl").unwrap().unwrap();
        let p1 = tree.query_selector("#p1").unwrap().unwrap();
        let after = tree.query_selector("#after").unwrap().unwrap();

        let f = rects[&fl];
        assert!((f.x - 0.0).abs() < EPS as f32 && (f.y - 0.0).abs() < EPS as f32,
            "float sits at the containing block's top-left: {f:?}");
        assert!((f.width - 200.0).abs() < EPS as f32, "float keeps authored width: {f:?}");

        let p = rects[&p1];
        assert!((p.x - 200.0).abs() < EPS as f32,
            "flow sibling starts at the float's right edge: {p:?}");
        assert!((p.y - 0.0).abs() < EPS as f32, "first flow sibling is beside the float, not below");
        let col_width = VW - 200.0;
        assert!((p.width - col_width).abs() <= 2.0 * EPS as f32,
            "column width = container − float: {} vs {col_width}", p.width);

        let a = rects[&after];
        assert!((a.x - 0.0).abs() < EPS as f32, "cleared sibling returns to full width: {a:?}");
        assert!(a.y >= f.y + f.height - EPS as f32,
            "clear:both moves below the float bottom: a.y={} float.bottom={}", a.y, f.y + f.height);
    }

    /// Logical clear keywords resolve against LTR: inline-start clears a
    /// left float, inline-end a right float.
    #[test]
    fn logical_clear_keywords_clear_matching_float_side() {
        for (keyword, float_sel) in [("inline-start", "#fl"), ("inline-end", "#fr")] {
            let html = format!(
                r#"<body>
                    <div id="fl"></div>
                    <div id="fr" style="float: right; width: 120px; height: 80px;"></div>
                    <div id="after" style="clear: {keyword}"></div>
                </body>"#
            );
            let sheet = r#"
                body { margin: 0; }
                #fl { float: left; width: 200px; height: 300px; }
                #after { width: 500px; height: 20px; margin: 0; }
            "#;
            let tree = crate::diting_dom::tree_sink::parse_html(&html);
            let rules = diting_css::parse_stylesheet(sheet);
            let styles = our_styles(&tree, &rules);
            let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
            let after = tree.query_selector("#after").unwrap().unwrap();
            let a = rects[&after];
            let fl = rects[&tree.query_selector("#fl").unwrap().unwrap()];
            let fr = rects[&tree.query_selector("#fr").unwrap().unwrap()];
            if float_sel == "#fl" {
                assert!(a.y >= fl.y + fl.height - EPS as f32,
                    "{keyword} clears the left float: a.y={} left.bottom={}", a.y, fl.y + fl.height);
            } else {
                assert!(a.y >= fr.y + fr.height - EPS as f32,
                    "{keyword} clears the right float: a.y={} right.bottom={}", a.y, fr.y + fr.height);
            }
        }
    }

    /// The right-float variant of the same zone: the float hugs the
    /// container's RIGHT edge and the flow column takes the left remainder.
    #[test]
    fn float_right_hugs_container_right_edge() {
        let html = r#"<body>
            <div id="fr"></div>
            <p id="p1">旁流文本</p>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #fr { float: right; width: 150px; height: 200px; }
            #p1 { margin: 0; font-size: 16px; height: 40px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let fr = rects[&tree.query_selector("#fr").unwrap().unwrap()];
        let p1 = rects[&tree.query_selector("#p1").unwrap().unwrap()];

        assert!((fr.x - (VW - 150.0)).abs() < EPS as f32,
            "right float at container right edge: x={} want {}", fr.x, VW - 150.0);
        assert!((p1.x - 0.0).abs() < EPS as f32, "flow column starts at the left edge");
        assert!((p1.width - (VW - 150.0)).abs() <= 2.0 * EPS as f32,
            "column width excludes the right float: {} vs {}", p1.width, VW - 150.0);
    }

    /// The float-grid idiom (8c): consecutive same-side floats sit SIDE BY
    /// SIDE on one band, wrapping to a new band when the row fills —
    /// craigslist's `.box{float:left;width:23%}` directory shape. Hand
    /// contract: three 25%-wide floats at x = 0 / 200 / 400 on the same y;
    /// floats four and five wrap to the next band below the tallest
    /// band-one sibling.
    #[test]
    fn float_run_wraps_side_by_side_into_bands() {
        let html = r#"<body>
            <div class="cell" id="c1"></div><div class="cell" id="c2"></div>
            <div class="cell" id="c3"></div><div class="cell" id="c4" style="height: 30px"></div>
            <div class="cell" id="c5"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            .cell { float: left; width: 25%; height: 50px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (c1, c2, c3, c4, c5) = (r("#c1"), r("#c2"), r("#c3"), r("#c4"), r("#c5"));

        // Our engine ships no UA body margin (every test sheet sets it
        // explicitly; blitz's default.css does too but our subset starts
        // at zero) — the band is the full viewport: four 200px cells fill
        // band one exactly.
        let cell_w = VW * 0.25;
        for (name, cell, x) in
            [("c1", c1, 0.0), ("c2", c2, cell_w), ("c3", c3, 2.0 * cell_w), ("c4", c4, 3.0 * cell_w)]
        {
            assert!((cell.x - x).abs() < EPS as f32, "{name} x: {} want {x}", cell.x);
            assert!((cell.y - 0.0).abs() < EPS as f32, "{name} shares band one");
            assert!((cell.width - cell_w).abs() < EPS as f32, "{name} is 25% of the band");
        }
        // Band two: below the tallest band-one sibling.
        assert!((c5.y - 50.0).abs() < EPS as f32,
            "fifth float wraps to band two: y={} want 50", c5.y);
        assert!((c5.x - 0.0).abs() < EPS as f32, "wrapped float restarts at the left edge");
    }

    /// Zone ordering (8b/8h/8i): zone rows interleave with normal siblings
    /// in DOCUMENT order, and bridge-separated same-side floats form ONE
    /// rail. The wikipedia lead shape — a block, then a floated infobox,
    /// then across an empty bridge a SECOND float (the sidebar tables,
    /// clear-stacked), then lead prose. Hand contract: the pre-float block
    /// renders ABOVE the float band; the two floats form a right rail (each
    /// hugging the right edge, stacked); the prose starts beside the FIRST
    /// float's band in a column as wide as the container minus the rail.
    #[test]
    fn float_zone_rows_follow_document_order() {
        // MediaWiki's exact lead-section shape: the empty bridge between the
        // infobox and the sidebar is a mw-empty-elt span whose only children
        // are ResourceLoader <link> hints — no visible content, but a bare
        // has-children check reads it as flow and breaks the float rail.
        let html = r#"<body>
            <div id="desc"></div>
            <table id="ib"><tr><td>info</td></tr></table>
            <span id="bridge" class="mw-empty-elt"><link rel="stylesheet" href="/load.php"><meta property="mw:x"></span>
            <table id="sb"></table>
            <p id="lead1">第一段导语</p>
            <p id="lead2">第二段导语</p>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #desc { height: 20px; margin: 0; }
            #ib { float: right; width: 200px; height: 150px; margin: 0; }
            .mw-empty-elt { display: none; }
            #sb { float: right; clear: right; width: 150px; height: 100px; margin: 0; }
            p { margin: 0; font-size: 16px; height: 40px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (desc, ib, sb, l1, l2) = (r("#desc"), r("#ib"), r("#sb"), r("#lead1"), r("#lead2"));

        // Pre-float content owns the band ABOVE the float: CSS floats only
        // displace content that follows them in document order.
        assert!((desc.y + desc.height) <= ib.y + EPS as f32,
            "pre-float block renders above the float band: desc.bottom={} ib.y={}",
            desc.y + desc.height, ib.y);
        // First float: right edge, authored size.
        assert!((ib.x - (VW - 200.0)).abs() < EPS as f32,
            "infobox hugs the right edge: x={} want {}", ib.x, VW - 200.0);
        assert!((ib.width - 200.0).abs() < EPS as f32, "infobox width: {ib:?}");
        // The SECOND float joins the rail: right edge, stacked below the
        // first (clear:right — the wikipedia sidebar idiom).
        assert!((sb.x - (VW - 150.0)).abs() < EPS as f32,
            "sidebar hugs the right edge too: x={} want {}", sb.x, VW - 150.0);
        assert!(sb.y >= ib.y + ib.height - EPS as f32,
            "sidebar stacks below the infobox: sb.y={} ib.bottom={}", sb.y, ib.y + ib.height);
        // Lead prose shares the WHOLE rail's band: starts beside the first
        // float, column = container − widest rail float.
        assert!((l1.y - ib.y).abs() < EPS as f32,
            "lead starts beside the first float's band: l1.y={} ib.y={}", l1.y, ib.y);
        assert!((l1.x - 0.0).abs() < EPS as f32, "lead starts at the left edge: {l1:?}");
        assert!((l1.width - (VW - 200.0)).abs() <= 2.0 * EPS as f32,
            "lead column = container − rail width: {} vs {}", l1.width, VW - 200.0);
        assert!(l2.y > l1.y, "second paragraph follows the first");
    }

    /// The opposing-float header (8d): a left float and a right float
    /// (across an empty bridge sibling) share ONE band — left hugs the
    /// container's left edge, right hugs its right edge, both at band top.
    /// Hand contract: logo at x=0, tagline at VW−150, both y=0; a following
    /// sibling stacks below the taller float at full width.
    #[test]
    fn opposing_float_pair_shares_band_space_between() {
        let html = r#"<body>
            <div id="logo"></div><span></span><div id="tag"></div>
            <div id="below"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #logo { float: left; width: 200px; height: 60px; }
            #tag { float: right; width: 150px; height: 40px; }
            #below { width: 500px; height: 20px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (logo, tag, below) = (r("#logo"), r("#tag"), r("#below"));

        assert!((logo.x - 0.0).abs() < EPS as f32 && (logo.y - 0.0).abs() < EPS as f32,
            "left float at top-left: {logo:?}");
        assert!((tag.x - (VW - 150.0)).abs() < EPS as f32,
            "right float hugs the right edge: x={} want {}", tag.x, VW - 150.0);
        assert!((tag.y - 0.0).abs() < EPS as f32, "same band: both floats start at y=0");
        // The pair row's height is the max child height (60); the sibling
        // below starts after it, full width again.
        assert!((below.x - 0.0).abs() < EPS as f32, "following sibling back to full width");
        assert!((below.y - 60.0).abs() <= 2.0 * EPS as f32,
            "sibling below the band: y={} want 60", below.y);
    }

    /// The right-float navigation bar (8e): inline flow content plus two
    /// right floats on one band. Right floats place from the inline-end
    /// INWARD, so the FIRST in source order hugs the right edge and later
    /// ones stack leftward — visual order is source order reversed.
    #[test]
    fn right_float_nav_bar_reverses_source_order() {
        let html = r#"<body>
            <span id="brand">站点名</span>
            <div id="nav1"></div>
            <div id="nav2"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #brand { font-size: 16px; }
            #nav1 { float: right; width: 120px; height: 30px; }
            #nav2 { float: right; width: 100px; height: 30px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (n1, n2) = (r("#nav1"), r("#nav2"));

        // Source-first float at the far RIGHT edge; second sits to its
        // LEFT (inline-end placement).
        assert!((n1.x - (VW - 120.0)).abs() < EPS as f32,
            "first right float hugs the right edge: x={} want {}", n1.x, VW - 120.0);
        assert!((n2.x - (VW - 220.0)).abs() < EPS as f32,
            "second right float stacks leftward: x={} want {}", n2.x, VW - 220.0);
        assert!((n1.y - 0.0).abs() < EPS as f32 && (n2.y - 0.0).abs() < EPS as f32,
            "both share band one");
    }


    /// Authored box geometry + block stacking: the part of layout both
    /// engines must agree on exactly.
    #[test]
    fn authored_boxes_match_blitz() {
        let html = r#"<body><div id="a">你好世界</div><div id="b">hello world</div><div id="c"></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #a { width: 200px; height: 40px; font-size: 20px; }
            #b { width: 300px; height: 50px; padding: 10px; }
            #c { width: 120px; height: 30px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);

        for selector in ["html", "body", "#a", "#b", "#c"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }

        // Content-box → border-box mapping spot check: #b is authored
        // 300×50 content + 10px padding all around = 320×70 border box.
        let our_b = tree.query_selector("#b").unwrap().unwrap();
        let b = rects[&our_b];
        assert!((b.width - 320.0).abs() < EPS as f32, "#b border-box width: {}", b.width);
        assert!((b.height - 70.0).abs() < EPS as f32, "#b border-box height: {}", b.height);
        // Block stacking: c starts exactly at b's bottom edge.
        let our_c = tree.query_selector("#c").unwrap().unwrap();
        let c = rects[&our_c];
        assert!((c.y - (b.y + b.height)).abs() < EPS as f32, "stacking: c.y={} b.bottom={}", c.y, b.y + b.height);
    }

    /// display:none drops the subtree entirely; display:block on a default
    /// inline (span) makes it a real box.
    #[test]
    fn display_none_skips_subtree() {
        let html = r#"<body><div id="gone" style="display: none"><p id="inner">x</p></div><div id="kept">y</div></body>"#;
        let sheet = "body { margin: 0; }";
        let (doc, tree, _styles, rects) = both_engines(html, sheet);

        let gone = tree.query_selector("#gone").unwrap().unwrap();
        let inner = tree.query_selector("#inner").unwrap().unwrap();
        let kept = tree.query_selector("#kept").unwrap().unwrap();
        assert!(!rects.contains_key(&gone), "#gone must be absent");
        assert!(!rects.contains_key(&inner), "#inner (inside display:none) must be absent");

        // #kept stacks at the top: the removed box leaves no gap.
        let ours = rects[&kept];
        let blitz_nid = doc.query_selector("#kept").unwrap().unwrap();
        let theirs = element_rect(&doc, blitz_nid);
        assert_close("#kept x", ours.x, theirs.x);
        assert_close("#kept y", ours.y, theirs.y);
    }

    /// CJK wrapping (our metrics model — blitz measures real glyphs, so this
    /// is a structural lock, not a cross-engine compare): 12 ideographs at
    /// 20px in a 200px box = 10 per line, 2 lines × 24px line height.
    #[test]
    fn cjk_wraps_per_glyph() {
        let html = r#"<body><div id="zh">一二三四五六七八九十甲乙</div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #zh { width: 200px; font-size: 20px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let zh = rects[&tree.query_selector("#zh").unwrap().unwrap()];
        assert!((zh.width - 200.0).abs() < EPS as f32, "width: {}", zh.width);
        // 2 lines × (20 × 1.2)
        assert!((zh.height - 48.0).abs() < EPS as f32, "2 CJK lines: {}", zh.height);
    }

    /// Inline content of one block shares a single wrapping run: words from
    /// text + flattened span children wrap together (one line), and since the
    /// obscura#722 fix the span keeps a synthesized bounding-box rect from
    /// its hoisted children (it owns no taffy box of its own).
    #[test]
    fn inline_run_is_one_wrapper() {
        let html = r#"<body><p id="p">alpha <span id="s">beta gamma</span> delta</p></body>"#;
        let sheet = "body { margin: 0; }";
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let p = rects[&tree.query_selector("#p").unwrap().unwrap()];
        let s_id = tree.query_selector("#s").unwrap().unwrap();
        let s = rects
            .get(&s_id)
            .expect("flattened span keeps a synthesized rect");
        // One line of 16px text = 19.2 → 19 after taffy's rounding.
        assert!((p.height - 19.2).abs() < EPS as f32, "one line: {}", p.height);
        // The span's rect hugs "beta gamma": starts after "alpha ", same
        // line box height as p, inside p's bounds.
        assert!(s.x > 0.0, "span starts after leading text: {:?}", s);
        assert!((s.height - p.height).abs() <= 1.0, "span is one line: {:?}", s);
        assert!(s.y >= p.y - EPS as f32 && s.y + s.height <= p.y + p.height + EPS as f32, "span inside p: s={:?} p={:?}", s, p);
    }

    /// text-align promotion (upstream stand-in): the block becomes a
    /// flex-column so its runs center. KNOWN DIVERGENCE vs real CSS (and
    /// blitz): a BLOCK child of a centered block still stretches full width
    /// in real CSS; in the flex-column stand-in it shrink-wraps and centers.
    /// Locked here as our model's contract; inline content (the thing
    /// text-align actually addresses) centers correctly.
    #[test]
    fn text_align_center_promotes() {
        let html = r#"<body><div id="m"><div id="mi">hi</div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #m { width: 200px; height: 60px; text-align: center; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let m = rects[&tree.query_selector("#m").unwrap().unwrap()];
        let mi = rects[&tree.query_selector("#mi").unwrap().unwrap()];
        assert!((m.width - 200.0).abs() < EPS as f32, "container width: {}", m.width);
        // Run width = ceil(real shaped advance of "hi" at 16px in the fixture
        // face) (batch 3a: 14.112 → 15 ceiled — the old deterministic model
        // guessed 17.6).
        let hi = fixture_fonts().advance_width("hi", 16.0, false).ceil();
        assert!((mi.width - hi).abs() < EPS as f32, "run width: {} want {hi}", mi.width);
        assert!((mi.x - (200.0 - mi.width) / 2.0).abs() < EPS as f32, "centered x: {}", mi.x);
    }

    /// Flex pass-through: row layout with gap, column layout, and
    /// justify-content distribution — all authored sizes, so both engines'
    /// rects must agree exactly.
    #[test]
    fn flex_passthrough_matches_blitz() {
        let html = r#"<body>
            <div id="row"><div id="r1"></div><div id="r2"></div><div id="r3"></div></div>
            <div id="col"><div id="c1"></div><div id="c2"></div></div>
            <div id="sp"><div id="s1"></div><div id="s2"></div><div id="s3"></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #row { display: flex; gap: 10px; width: 320px; height: 40px; }
            #row > div { width: 100px; height: 30px; }
            #col { display: flex; flex-direction: column; gap: 6px; width: 200px; height: 106px; margin-top: 20px; }
            #col > div { width: 50px; height: 50px; }
            #sp { display: flex; justify-content: space-between; width: 300px; height: 20px; margin-top: 20px; }
            #sp > div { width: 60px; height: 10px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);

        for selector in ["#row", "#r1", "#r2", "#r3", "#col", "#c1", "#c2", "#sp", "#s1", "#s2", "#s3"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }

        // Structural spot checks of the flex math itself: row items at
        // 0 / 110 / 220 (100px + 10px gap), space-between edges flush and the
        // middle item centered (300−3×60)/2 = 60 apart.
        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        assert!((r("#r2").x - 110.0).abs() < EPS as f32, "row gap: {}", r("#r2").x);
        assert!((r("#c2").y - r("#c1").y - 56.0).abs() < EPS as f32, "column gap");
        assert!((r("#s2").x - 120.0).abs() < EPS as f32, "space-between middle: {}", r("#s2").x);
        assert!((r("#s3").x - 240.0).abs() < EPS as f32, "space-between end: {}", r("#s3").x);
    }

    /// flex-grow distribution and cross-axis align-items centering.
    #[test]
    fn flex_grow_and_align_match_blitz() {
        let html = r#"<body>
            <div id="fx"><div id="g1"></div><div id="g2"></div></div>
            <div id="ac"><div id="a1"></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #fx { display: flex; width: 300px; height: 40px; }
            #g1 { flex-grow: 1; height: 20px; }
            #g2 { width: 100px; height: 20px; }
            #ac { display: flex; align-items: center; width: 200px; height: 50px; margin-top: 20px; }
            #a1 { width: 40px; height: 10px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#fx", "#g1", "#g2", "#ac", "#a1"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        // g1 grows into 300−100 = 200 wide; a1 centers vertically in its
        // container: y offset = (50−10)/2 = 20.
        let g1 = rects[&tree.query_selector("#g1").unwrap().unwrap()];
        let ac = rects[&tree.query_selector("#ac").unwrap().unwrap()];
        let a1 = rects[&tree.query_selector("#a1").unwrap().unwrap()];
        assert!((g1.width - 200.0).abs() < EPS as f32, "grow width: {}", g1.width);
        assert!((a1.y - ac.y - 20.0).abs() < EPS as f32, "align center y: {}", a1.y - ac.y);
    }

    /// Grid tracks: fr distribution, px track, gap between tracks.
    #[test]
    fn grid_tracks_match_blitz() {
        let html = r#"<body>
            <div id="gr"><div id="t1"></div><div id="t2"></div><div id="t3"></div><div id="t4"></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gr { display: grid; grid-template-columns: 1fr 2fr; grid-template-rows: 30px 40px; width: 300px; }
            #gr > div { }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#gr", "#t1", "#t2", "#t3", "#t4"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        // 1fr 2fr of 300 → 100/200 columns; rows 30/40. Item 3 wraps to row 2.
        let t3 = rects[&tree.query_selector("#t3").unwrap().unwrap()];
        assert!((t3.x - 0.0).abs() < EPS as f32 && (t3.y - 30.0).abs() < EPS as f32, "t3 position: {:?}", t3);
        let t2 = rects[&tree.query_selector("#t2").unwrap().unwrap()];
        assert!((t2.x - 100.0).abs() < EPS as f32 && (t2.width - 200.0).abs() < EPS as f32, "t2 track: {:?}", t2);
    }

    /// Grid template areas (the Vector 2022 page scaffold): named cells,
    /// `grid-template` rows/columns shorthand, `grid-area: <name>` item
    /// placement, and a minmax(0, 1fr) track. Wikipedia's chrome is built
    /// exactly from these three pieces — before this batch all three fell
    /// out of the parser and the whole scaffold collapsed into one implicit
    /// column.
    #[test]
    fn grid_template_areas_place_named_items() {
        let html = r#"<body>
            <div id="wrap"><nav id="nav"></nav><main id="main"></main><footer id="ft"></footer></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #wrap { display: grid; width: 320px;
                grid-template: auto 1fr auto / 100px minmax(0, 1fr);
                grid-template-areas: 'nav main' 'nav main' 'ft ft'; }
            #nav { grid-area: nav; }
            #main { grid-area: main; }
            #ft { grid-area: ft; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#wrap", "#nav", "#main", "#ft"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let nav = rects[&tree.query_selector("#nav").unwrap().unwrap()];
        let main = rects[&tree.query_selector("#main").unwrap().unwrap()];
        let ft = rects[&tree.query_selector("#ft").unwrap().unwrap()];
        // The collapse symptom: nav and main must SHARE a row (side-by-side
        // columns), not stack. ft spans both columns at full width.
        assert!((nav.x - 0.0).abs() < EPS as f32 && (nav.width - 100.0).abs() < EPS as f32,
            "nav takes the fixed first column: {:?}", nav);
        assert!((main.x - 100.0).abs() < EPS as f32 && (main.width - 220.0).abs() < EPS as f32,
            "main takes the minmax(0, 1fr) remainder: {:?}", main);
        assert!((main.y - nav.y).abs() < EPS as f32, "nav and main share the first grid row");
        assert!((ft.y - (nav.y + nav.height)).abs() < EPS as f32 && (ft.width - 320.0).abs() < EPS as f32,
            "ft starts below nav and spans both columns: {:?}", ft);
    }

    /// Vector 2022 collapse (the wikipedia TOC bug): a `float:left` buried
    /// deep inside a LATER grid item must never narrow the grid's EARLIER
    /// items. The 8f displacement climb used to reach the grid container and
    /// clamp the earlier area's max-width to `float right edge − column x`
    /// (negative → 0), collapsing the whole TOC column to width 0. Two CSS
    /// rules restore it: floats only displace content AFTER them in document
    /// order, and the climb stops at the grid container — its items are not
    /// in-flow content a float can push.
    #[test]
    fn float_deep_in_grid_item_keeps_earlier_areas_intact() {
        let html = r#"<body>
            <div id="wrap">
                <nav id="col"><p>toc line one</p><p>toc line two</p></nav>
                <main id="page">
                    <div id="article"><div id="box"><div id="fl">F</div><p id="after">after the float</p></div></div>
                </main>
            </div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #wrap { display: grid; width: 320px;
                grid-template: auto / 100px minmax(0, 1fr);
                grid-template-areas: 'col page'; }
            #col { grid-area: col; }
            #col p { margin: 0; height: 30px; }
            #page { grid-area: page; }
            #fl { float: left; width: 36px; height: 60px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#col", "#page"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            // Height left out: the two engines model default <p> margins
            // differently and the bug under test is horizontal collapse.
        }
        let col = rects[&tree.query_selector("#col").unwrap().unwrap()];
        let page = rects[&tree.query_selector("#page").unwrap().unwrap()];
        let fl = rects[&tree.query_selector("#fl").unwrap().unwrap()];
        let after = rects[&tree.query_selector("#after").unwrap().unwrap()];
        // The collapse symptom: the earlier item keeps its full track.
        assert!((col.width - 100.0).abs() < EPS as f32,
            "earlier grid item keeps the fixed track width: {:?}", col);
        // The float stays inside its own (later) grid item.
        assert!(fl.x >= page.x && fl.x + fl.width <= page.x + page.width + EPS as f32,
            "float stays within its grid item: fl={:?} page={:?}", fl, page);
        // Legit displacement survives: content after the float narrows.
        assert!(after.width < page.width - 30.0,
            "later content is still displaced by the float: after={:?} page={:?}", after, page);
    }

    /// Replaced img: HTML width/height attributes map to definite sizes both
    /// engines honor; a CSS axis override wins per-axis with NO ratio
    /// derivation (attrs are presentational-hint declarations, so both axes
    /// are declared → 100×200, not 100×50). Position is NOT cross-asserted:
    /// our UA models img block-level while real CSS (and blitz) makes it an
    /// inline-level box on a text baseline (~2px strut offsets).
    #[test]
    fn replaced_img_matches_blitz() {
        let html = r#"<body>
            <img id="nat" width="200" height="100">
            <img id="cssw" width="400" height="200" style="width: 100px">
            <img id="none">
        </body>"#;
        let sheet = "body { margin: 0; }";
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#nat", "#cssw"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }

        // No dimensions at all: our model uses the CSS default object size
        // 300×150. KNOWN DIVERGENCE (recorded in render.md §13): blitz gives
        // 0×0 for an unloaded img (net_provider is None → no intrinsic size;
        // a real page with the image loaded would have one).
        let none = rects[&tree.query_selector("#none").unwrap().unwrap()];
        assert!((none.width - 300.0).abs() < EPS as f32, "default width: {}", none.width);
        assert!((none.height - 150.0).abs() < EPS as f32, "default height: {}", none.height);

        // Block stacking of our model: none starts at nat.bottom + cssw.height
        // (cssw is 100×200 — both axes declared, no ratio derivation).
        let nat = rects[&tree.query_selector("#nat").unwrap().unwrap()];
        assert!((none.y - (nat.y + nat.height + 200.0)).abs() < EPS as f32, "stacking");
    }

    /// obscura #698: `<img width=1024 height=572 style="width:100%">` inside a
    /// content-sized flex item. width:100% against an auto inline size is a
    /// cyclic percentage; obscura's taffy fork defers it to 0 (collapsing the
    /// box so it's never painted) and STOCK taffy (blitz, our pinned ref)
    /// collapses it to 0×572 too. Our replaced-leaf model sets a definite
    /// height (from the attr) + aspect ratio, so the cyclic width transfers
    /// through the ratio to the natural 1024 — matching Chrome, not blitz.
    /// This is a known divergence: assert the immune side, don't cross-check.
    #[test]
    fn cyclic_percentage_image_resolves_to_natural_size() {
        let html = r#"<body>
            <div id="row" style="display:flex">
                <div id="item">
                    <img id="hero" width="1024" height="572" style="width:100%">
                </div>
            </div>
        </body>"#;
        let sheet = "body { margin: 0; }";
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let hero = rects[&tree.query_selector("#hero").unwrap().unwrap()];
        assert!(
            (hero.width - 1024.0).abs() < EPS as f32 && (hero.height - 572.0).abs() < EPS as f32,
            "cyclic % image should resolve to natural size, got {hero:?}"
        );
    }

    /// position:relative offsets: in-flow box shifted by top/left; siblings
    /// occupy the STATIC position (relative keeps its space).
    #[test]
    fn relative_offset_matches_blitz() {
        let html = r#"<body><div id="wrap"><div id="rel"></div><div id="after"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #rel { position: relative; top: 10px; left: 20px; width: 50px; height: 30px; }
            #after { width: 50px; height: 10px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#wrap", "#rel", "#after"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let rel = rects[&tree.query_selector("#rel").unwrap().unwrap()];
        assert!((rel.x - 20.0).abs() < EPS as f32 && (rel.y - 10.0).abs() < EPS as f32, "rel offset: {:?}", rel);
        // #after sits at rel's STATIC bottom (y=30), not the shifted one.
        let after = rects[&tree.query_selector("#after").unwrap().unwrap()];
        assert!((after.y - 30.0).abs() < EPS as f32, "after static y: {}", after.y);
    }

    /// position:absolute with a positioned grandparent: the containing block
    /// is the nearest positioned ancestor (the reparent pass), NOT the DOM
    /// parent. No positioned ancestor → initial containing block (root).
    #[test]
    fn absolute_reparent_matches_blitz() {
        let html = r#"<body>
            <div id="gp"><div id="mid"><div id="abs"></div></div></div>
            <div id="orphan" style="position: absolute; top: 5px; left: 10px; width: 30px; height: 20px;"></div>
            <div id="fx" style="position: fixed; top: 8px; left: 12px; width: 26px; height: 14px;"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #mid { width: 200px; height: 100px; margin-top: 40px; margin-left: 30px; }
            #abs { position: absolute; top: 5px; left: 15px; width: 40px; height: 20px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        // KNOWN MODELING DIVERGENCE (render.md §14): blitz anchors absolute
        // and fixed boxes to the STATIC parent (its taffy bridge keeps the
        // DOM parent, so #abs lands at mid.margin-left + left = 45 and the
        // fixed box picks up the collapsed strut). CSS — and this bridge —
        // resolve against the nearest positioned ancestor / the viewport.
        // Cross-asserting positions against blitz would lock THEIR bug; the
        // in-flow elements (#gp, #mid) do agree and are checked in other
        // tests.
        let abs = rects[&tree.query_selector("#abs").unwrap().unwrap()];
        assert!((abs.x - 15.0).abs() < EPS as f32 && (abs.y - 45.0).abs() < EPS as f32, "abs in gp: {:?}", abs);
        // No positioned ancestor → initial containing block (the root).
        let orphan = rects[&tree.query_selector("#orphan").unwrap().unwrap()];
        assert!((orphan.x - 10.0).abs() < EPS as f32 && (orphan.y - 5.0).abs() < EPS as f32, "orphan at ICB: {:?}", orphan);
        // Fixed pins to the viewport regardless of the collapsed strut.
        let fx = rects[&tree.query_selector("#fx").unwrap().unwrap()];
        assert!((fx.x - 12.0).abs() < EPS as f32 && (fx.y - 8.0).abs() < EPS as f32, "fixed at viewport: {:?}", fx);
    }

    /// nicoburns' blitz#764 review point: after reparenting, the static-
    /// position fallback (auto insets) must come from the ORIGINAL flow
    /// parent. An inset-less absolute box inside a margin-offset static
    /// parent sits at its would-be flow spot (#mid's content origin), not
    /// at the containing block's padding edge (#gp's).
    #[test]
    fn absolute_auto_insets_use_original_flow_static_position() {
        let html = r#"<body>
            <div id="gp"><div id="mid"><div id="abs">x</div></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #mid { width: 200px; height: 100px; margin-top: 40px; margin-left: 30px; }
            #abs { position: absolute; width: 40px; height: 20px; }
        "#;
        // Hand-computed (no blitz cross-assert — see the divergence note in
        // absolute_reparent_matches_blitz): #mid's margin-top:40 escapes
        // through #gp, so #mid's border-box origin is (30, 40) and the abs
        // box's static spot is #mid's content origin = (30, 40). Anchoring
        // at the CB's padding edge instead would give (0, 40).
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let abs = rects[&tree.query_selector("#abs").unwrap().unwrap()];
        assert!(
            (abs.x - 30.0).abs() < EPS as f32 && (abs.y - 40.0).abs() < EPS as f32,
            "inset-less abs keeps its ORIGINAL flow position: {:?}",
            abs
        );
    }

    /// Mixed axes: top inset set, left/right auto — y comes from the inset
    /// against the CB (taffy), x from the harvested static position. And a
    /// fixed box with both axes auto also resolves at its flow spot.
    #[test]
    fn absolute_one_axis_inset_takes_static_only_on_auto_axis() {
        let html = r#"<body>
            <div id="gp"><div id="mid"><div id="mix"></div><div id="fx"></div></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #mid { width: 200px; height: 100px; margin-top: 40px; margin-left: 30px; }
            #mix { position: absolute; top: 5px; width: 40px; height: 20px; }
            #fx { position: fixed; width: 26px; height: 14px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        // Margin note: #mid's margin-top:40 escapes through #gp (position:
        // relative doesn't establish a BFC), so #gp's own border box lands
        // at (0, 40) and #mid's content origin at (30, 40).
        // #mix: y = CB (#gp) padding edge 40 + top inset 5; x = static
        // (#mid's content origin 30) — NOT taffy's CB-flow fallback x=0.
        let mix = rects[&tree.query_selector("#mix").unwrap().unwrap()];
        assert!(
            (mix.x - 30.0).abs() < EPS as f32 && (mix.y - 45.0).abs() < EPS as f32,
            "top inset + auto left: y from CB, x from static: {:?}",
            mix
        );
        // #fx's hypothetical flow spot: #mix is out of flow, so #fx would be
        // the FIRST in-flow child of #mid — its content origin, not one line
        // down.
        let fx = rects[&tree.query_selector("#fx").unwrap().unwrap()];
        assert!(
            (fx.x - 30.0).abs() < EPS as f32 && (fx.y - 40.0).abs() < EPS as f32,
            "inset-less fixed at its flow spot: {:?}",
            fx
        );
    }

    /// `margin: 0 auto` (in-flow block horizontal centering, CSS §8.3 /
    /// §10.3.3): the widest idiom on the web - every centered page layout.
    /// Length gained an `Auto` variant; taffy's block algorithm expands
    /// auto margins against the containing width. Pre-fix the auto token
    /// silently parsed to None -> 0, so the box pinned left.
    #[test]
    fn block_margin_auto_centers_against_containing_width() {
        let html = r#"<body>
            <div id="narrow"></div>
            <div id="wide"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #narrow { width: 200px; height: 40px; margin: 0 auto; }
            #wide { width: 700px; height: 10px; margin-left: auto; margin-right: auto; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let narrow = rects[&tree.query_selector("#narrow").unwrap().unwrap()];
        assert!(
            (narrow.x - 300.0).abs() < EPS as f32,
            "200px block centered in 800px viewport: {:?}",
            narrow
        );
        let wide = rects[&tree.query_selector("#wide").unwrap().unwrap()];
        assert!(
            (wide.x - 50.0).abs() < EPS as f32 && (wide.y - 40.0).abs() < EPS as f32,
            "700px block centered, stacked below: {:?}",
            wide
        );
    }

    /// The abspos auto-margin centering idiom (taffy#923's exact repro,
    /// CSS §10.3.3 over-constrained resolution): `left:0; right:0;
    /// margin-left/right: auto` centers against the containing block's
    /// padding box. Nested two deep - each box centers inside its own CB.
    #[test]
    fn absolute_auto_margin_centers_in_containing_block() {
        let html = r#"<body>
            <div id="root"><div id="outer"><div id="inner"></div></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #root { position: relative; width: 300px; height: 200px; }
            #outer {
                position: absolute; left: 0; right: 0; top: 20px;
                width: 120px; height: 80px; margin-left: auto; margin-right: auto;
            }
            #inner {
                position: absolute; top: 0; bottom: 0; left: 0; right: 0;
                width: 40px; height: 30px; margin: auto;
            }
        "#;
        // Hand-computed: #root border box 300x200 at (0,0) (both_engines
        // body margin 0). #outer's CB = root padding box = (0,0) 300x200;
        // 120 wide centered -> x=90, y=20 (top inset). #inner's CB = outer
        // padding box = (90,20) 120x80; 40x30 centered -> (130, 45).
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let outer = rects[&tree.query_selector("#outer").unwrap().unwrap()];
        assert!(
            (outer.x - 90.0).abs() < EPS as f32 && (outer.y - 20.0).abs() < EPS as f32,
            "outer centered in root CB: {:?}",
            outer
        );
        let inner = rects[&tree.query_selector("#inner").unwrap().unwrap()];
        assert!(
            (inner.x - 130.0).abs() < EPS as f32 && (inner.y - 45.0).abs() < EPS as f32,
            "inner centered in outer CB (both axes, margin:auto + inset:0): {:?}",
            inner
        );
    }

    /// `padding: auto` is illegal CSS: the declaration drops entirely
    /// (invalid-declaration recovery) - padding stays unset, never a
    /// half-applied state.
    #[test]
    fn padding_auto_drops_declaration() {
        let html = r#"<body><div id="p" style="padding: auto">x</div></body>"#;
        let (_doc, tree, styles, _rects) = both_engines(html, "");
        let st = styles.get(&tree.query_selector("#p").unwrap().unwrap()).unwrap();
        assert!(st.padding.top.is_none(), "padding:auto dropped: {:?}", st.padding);
    }

    /// obscura#675 lineage: a fixed box's insets resolve against the initial
    /// containing block — the VIEWPORT — not the root element's
    /// content-driven height. Before the fix, `bottom:0` anchored against
    /// the root's ~content height so every fixed overlay collapsed to the
    /// top of the page (hosted-instance probe: bottom:0 → y=47 on a 902px
    /// viewport, 50% → 34, 10% → 40). The root taffy node now carries the
    /// definite viewport size, so bottom and percentage insets have a real
    /// ICB to resolve against.
    #[test]
    fn declared_line_height_scales_paragraph_pitch() {
        // §49 parity fix 2: `line-height: 1.6` must move the used line
        // height (was pinned to normal = 1.2×fs). Two paragraphs of the
        // same wrapping text differ only in line-height → the 1.6 box is
        // exactly (1.6 − 1.2) × fs × lines taller per line of wrap.
        let html = r#"<body>
            <p id="lorem">谛听引擎中文渲染测试文本一行</p>
            <p id="wide" style="line-height: 1.6">谛听引擎中文渲染测试文本一行</p>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            p { margin: 0; width: 160px; font-size: 16px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let fonts = fixture_fonts();
        let text = "谛听引擎中文渲染测试文本一行";
        let tokens = text::tokens_of(text, 16.0, false, &fonts);
        let lines = text::greedy_wrap(&tokens, Some(160.0)).len() as f32;
        assert!(lines >= 2.0, "the fixture must wrap to ≥2 lines");

        let normal = rects[&tree.query_selector("#lorem").unwrap().unwrap()];
        let wide = rects[&tree.query_selector("#wide").unwrap().unwrap()];
        let expect_normal = lines * 16.0 * 1.2;
        let expect_wide = lines * 16.0 * 1.6;
        assert!(
            (normal.height - expect_normal).abs() < 1.5,
            "normal line-height 1.2×fs × {lines} lines: {:?}",
            normal
        );
        assert!(
            (wide.height - expect_wide).abs() < 1.5,
            "declared 1.6 × fs × {lines} lines: {:?}",
            wide
        );
    }

    #[test]
    fn adjacent_block_margins_collapse_to_max() {
        // CSS 2.1 §8.3.1: h1's .67em-bottom (of its 2em size → 21.44) meets
        // p's 1em-top (16) → one collapsed 21.44 gap, NOT the 37.44 sum
        // taffy would produce. A border on the touching edge re-separates
        // the boxes and the margins sum again (the border itself lives
        // INSIDE p2's border-box, so it adds nothing to the rect gap).
        let html = r#"<body>
            <h1 id="h">T</h1><p id="p1">x</p><h1 id="h2">T</h1><p id="p2">x</p>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            h1 { margin: .67em; }
            p { margin: 1em; }
            #p2 { border-top: 2px solid black; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let h = rects[&tree.query_selector("#h").unwrap().unwrap()];
        let p1 = rects[&tree.query_selector("#p1").unwrap().unwrap()];
        let h2 = rects[&tree.query_selector("#h2").unwrap().unwrap()];
        let p2 = rects[&tree.query_selector("#p2").unwrap().unwrap()];
        let collapsed_gap = p1.y - (h.y + h.height);
        assert!(
            (collapsed_gap - 21.44).abs() < EPS as f32,
            "h1-bottom + p-top collapse to max: {collapsed_gap}"
        );
        let summed_gap = p2.y - (h2.y + h2.height);
        // Rects are pixel-grid-rounded at both ends of the gap; the sum
        // crosses two rounding boundaries so the tolerance is a full pixel.
        assert!(
            (summed_gap - (21.44 + 16.0)).abs() < 1.01,
            "bordered edge separates the boxes: margins sum again: {summed_gap}"
        );
    }

    #[test]
    fn fixed_bottom_and_percent_insets_anchor_viewport() {
        let html = r#"<body>
            <div id="b0"></div><div id="t50"></div><div id="b10"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #b0 { position: fixed; bottom: 0; left: 0; width: 50px; height: 20px; }
            #t50 { position: fixed; top: 50%; left: 0; width: 50px; height: 20px; }
            #b10 { position: fixed; bottom: 10%; left: 0; width: 50px; height: 20px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        // VH = 600.
        let b0 = rects[&tree.query_selector("#b0").unwrap().unwrap()];
        assert!(
            (b0.y - 580.0).abs() < EPS as f32,
            "bottom:0 pins to the viewport bottom edge: {:?}",
            b0
        );
        let t50 = rects[&tree.query_selector("#t50").unwrap().unwrap()];
        assert!(
            (t50.y - 300.0).abs() < EPS as f32,
            "top:50% is half the VIEWPORT height: {:?}",
            t50
        );
        let b10 = rects[&tree.query_selector("#b10").unwrap().unwrap()];
        assert!(
            (b10.y - 520.0).abs() < EPS as f32,
            "bottom:10% measures from the viewport bottom: {:?}",
            b10
        );
    }

    /// bottom-only inset (top auto) anchors the box's bottom margin edge to
    /// the CB's bottom padding edge — single-axis counterpart of the
    /// opposing-inset stretch test.
    #[test]
    fn absolute_bottom_only_inset_anchors_cb_height() {
        let html = r#"<body><div id="gp"><div id="ab"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #ab { position: absolute; bottom: 10px; left: 0; width: 40px; height: 20px; }
        "#;
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let ab = rects[&tree.query_selector("#ab").unwrap().unwrap()];
        assert!(
            (ab.y - 170.0).abs() < EPS as f32,
            "bottom:10 on a 200px CB → y = 200 - 10 - 20: {:?}",
            ab
        );
    }

    /// Absolute stretch between opposing insets.
    #[test]
    fn absolute_stretch_matches_blitz() {        let html = r#"<body><div id="gp"><div id="st"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #st { position: absolute; top: 10px; bottom: 30px; left: 20px; right: 40px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#st"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let st = rects[&tree.query_selector("#st").unwrap().unwrap()];
        assert!((st.width - 240.0).abs() < EPS as f32, "stretch width: {}", st.width);
        assert!((st.height - 160.0).abs() < EPS as f32, "stretch height: {}", st.height);
    }

    /// min/max-width clamps and the CSS aspect-ratio property.
    #[test]
    fn clamps_and_aspect_ratio_match_blitz() {
        let html = r#"<body><div id="host" style="width: 200px"><div id="mn"></div></div><div id="mx"></div><div id="ar"></div><div id="arn"></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #mn { min-width: 300px; height: 10px; }
            #mx { max-width: 100px; height: 10px; }
            #ar { width: 100px; aspect-ratio: 2 / 1; }
            #arn { height: 60px; aspect-ratio: 0.5; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#host", "#mn", "#mx", "#ar", "#arn"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        assert!((r("#mn").width - 300.0).abs() < EPS as f32, "min-width floor");
        assert!((r("#mx").width - 100.0).abs() < EPS as f32, "max-width clamp");
        assert!((r("#ar").height - 50.0).abs() < EPS as f32, "ratio from width");
        assert!((r("#arn").width - 30.0).abs() < EPS as f32, "ratio from height");
    }

    /// em lengths fold against the element's own font-size — including the
    /// font-size declared in the same rule (CSS computes font-size first).
    #[test]
    fn em_lengths_match_blitz() {
        let html = r#"<body>
            <div id="outer"><div id="inner"></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #outer { font-size: 24px; }
            #inner { font-size: 2em; width: 5em; height: 1em; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#inner"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let inner = rects[&tree.query_selector("#inner").unwrap().unwrap()];
        // inner fs = 2em × 24 = 48 → width 5em = 240, height 1em = 48.
        assert!((inner.width - 240.0).abs() < EPS as f32, "5em of 48: {}", inner.width);
        assert!((inner.height - 48.0).abs() < EPS as f32, "1em of 48: {}", inner.height);
    }

    /// rem folds against the root font-size — both the implicit 16px default
    /// and an authored `html { font-size }` (our_styles threads the root
    /// element's computed size as the rem base).
    #[test]
    fn rem_against_default_and_authored_root() {
        let html = r#"<body><div id="plain"></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #plain { width: 12.5rem; height: 10px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        {
            let blitz_nid = doc.query_selector("#plain").unwrap().expect("#plain");
            let our_nid = tree.query_selector("#plain").unwrap().unwrap();
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close("default-root width", ours.width, theirs.width);
        }
        assert!((rects[&tree.query_selector("#plain").unwrap().unwrap()].width - 200.0).abs() < EPS as f32);

        let html2 = r#"<html><body><div id="scaled"></div></body></html>"#;
        let sheet2 = r#"
            body { margin: 0; }
            html { font-size: 20px; }
            #scaled { width: 10rem; height: 10px; }
        "#;
        let (_doc2, tree2, _styles2, rects2) = both_engines(html2, sheet2);
        let scaled = rects2[&tree2.query_selector("#scaled").unwrap().unwrap()];
        assert!((scaled.width - 200.0).abs() < EPS as f32, "10rem of authored root 20: {}", scaled.width);
    }

    /// Percent widths/margins resolve against the containing block at layout
    /// time (taffy percent on both sides — cross-asserting checks our
    /// pass-through mapping, taffy does the arithmetic identically).
    #[test]
    fn percent_box_model_match_blitz() {
        let html = r#"<body><div id="host"><div id="kid"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #host { width: 400px; height: 100px; }
            #kid { width: 50%; margin-left: 10%; height: 50%; margin-top: 5%; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#kid"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let kid = rects[&tree.query_selector("#kid").unwrap().unwrap()];
        // CSS: margin % both axes against CB WIDTH: left 40, top 20; 50% sizes.
        assert!((kid.x - 40.0).abs() < EPS as f32, "10% margin-left of 400: {}", kid.x);
        assert!((kid.y - 20.0).abs() < EPS as f32, "5% margin-top of CB width 400: {}", kid.y);
        assert!((kid.width - 200.0).abs() < EPS as f32, "50% width: {}", kid.width);
        assert!((kid.height - 50.0).abs() < EPS as f32, "50% height: {}", kid.height);
    }

    /// Percent insets (left/top against CB width/height) and % clamps, on a
    /// direct child of the positioned ancestor — the one abspos shape where
    /// blitz's DOM-parent anchor coincides with the CSS containing block.
    #[test]
    fn percent_insets_and_clamps_match_blitz() {
        let html = r#"<body>
            <div id="gp"><div id="pin"></div></div>
            <div id="host"><div id="clamp"></div></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #gp { position: relative; width: 300px; height: 200px; }
            #pin { position: absolute; left: 10%; top: 25%; width: 40px; height: 20px; }
            #host { width: 400px; }
            #clamp { width: 100px; min-width: 60%; height: 10px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        for selector in ["#pin", "#clamp"] {
            let blitz_nid = doc.query_selector(selector).unwrap().expect(selector);
            let our_nid = tree.query_selector(selector).unwrap().expect(selector);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = *rects.get(&our_nid).unwrap_or(&Rect::default());
            assert_close(&format!("{selector} x"), ours.x, theirs.x);
            assert_close(&format!("{selector} y"), ours.y, theirs.y);
            assert_close(&format!("{selector} width"), ours.width, theirs.width);
            assert_close(&format!("{selector} height"), ours.height, theirs.height);
        }
        let pin = rects[&tree.query_selector("#pin").unwrap().unwrap()];
        assert!((pin.x - 30.0).abs() < EPS as f32, "10% left of 300: {}", pin.x);
        assert!((pin.y - 50.0).abs() < EPS as f32, "25% top of 200: {}", pin.y);
        let clamp = rects[&tree.query_selector("#clamp").unwrap().unwrap()];
        assert!((clamp.width - 240.0).abs() < EPS as f32, "60% min-width of 400: {}", clamp.width);
    }

    // ---- batch 3a: real glyph measurement --------------------------------
    //
    // Both sides shape the SAME fixture bytes (Noto Sans SC subset): ours
    // through swash, blitz's through parley/harfrust via the injected
    // system-fonts-off FontContext. The old deterministic model guessed
    // 0.55em/ASCII char and ×1.08 for bold — these tests pin the real thing.

    /// CJK is the degenerate case that makes the per-word-leaf model exact:
    /// one glyph per token, advance = exactly one em, no kerning.
    #[test]
    fn cjk_advance_is_one_em() {
        let fonts = fixture_fonts();
        for fs in [12.0, 16.0, 20.0, 24.0] {
            for ch in ["你", "界", "测", "渲"] {
                let w = fonts.advance_width(ch, fs, false);
                assert!((w - fs).abs() < 0.01, "{ch} at {fs}px: {w} (want one em)");
            }
        }
    }

    /// ASCII shrink-wrap: a flex item sizes to its text's real proportional
    /// advances — not the 0.55em guess, and identical to blitz's parley run.
    #[test]
    fn ascii_proportional_width_matches_blitz() {
        let html = r#"<body><div id="row"><div id="w">hello world WebKit</div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #row { display: flex; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        let w = rects[&tree.query_selector("#w").unwrap().unwrap()];
        // The model contract (batch 3a): the run's intrinsic width is
        // ceil(shaped advance sum) — blitz rounds text runs UP so the box
        // never under-fits its glyphs; taffy's round-to-nearest would.
        let fonts = fixture_fonts();
        let want = fonts.advance_width("hello world WebKit", 16.0, false).ceil();
        assert!((w.width - want).abs() < EPS as f32, "ascii run: {} want {want}", w.width);
        // Cross-assert against blitz's parley/harfrust shaping of the same
        // bytes. Kerning differences between shapers stay inside EPS.
        let blitz_nid = doc.query_selector("#w").unwrap().expect("#w");
        let theirs = element_rect(&doc, blitz_nid);
        assert_close("ascii run width vs blitz", w.width, theirs.width);
        // And the guess is demonstrably wrong: the old model would say
        // 16 chars × 0.55em × 16px = 140.8.
        assert!((want - 140.8).abs() > 1.0, "model must beat the 0.55em guess (want={want})");
    }

    /// Bold resolves to the real wght=700 instance on BOTH sides (fontique
    /// picks the registered 700 face; we pick the bold half of the FontBook)
    /// — not a synthetic ×1.08 widening. Mixed text on purpose: CJK advance
    /// is one em in BOTH faces, so only the Latin run makes the width
    /// face-sensitive — a wrong face on either side breaks the cross-assert.
    #[test]
    fn bold_face_real_weight_matches_blitz() {
        let html = r#"<body><div id="row"><div id="b">加粗Bold文本</div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #row { display: flex; }
            #b { font-weight: 700; font-size: 20px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        let b = rects[&tree.query_selector("#b").unwrap().unwrap()];
        let fonts = fixture_fonts();
        let bold_w = fonts.advance_width("加粗Bold文本", 20.0, true);
        let reg_w = fonts.advance_width("加粗Bold文本", 20.0, false);
        assert!(bold_w > reg_w + 1.0, "fixture faces must differ: bold={bold_w} reg={reg_w}");
        assert!((b.width - bold_w.ceil()).abs() < EPS as f32, "bold run: {} want {}", b.width, bold_w.ceil());
        assert!((b.width - reg_w).abs() > EPS as f32, "bold must not measure with the regular face");
        let blitz_nid = doc.query_selector("#b").unwrap().expect("#b");
        let theirs = element_rect(&doc, blitz_nid);
        assert_close("bold run width vs blitz", b.width, theirs.width);
    }

    /// Mixed CJK+Latin wrapping in a fixed-width block: line breaks come
    /// from real advances (UAX#14 classes only pick the candidates), so the
    /// wrapped height — line count × 1.2×fs — matches blitz.
    #[test]
    fn mixed_cjk_latin_wrap_matches_blitz() {
        let html = r#"<body><div id="t">你好world测试engine渲染真实</div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #t { width: 200px; font-size: 20px; }
        "#;
        let (doc, tree, _styles, rects) = both_engines(html, sheet);
        let t = rects[&tree.query_selector("#t").unwrap().unwrap()];
        // Expected lines from real advances: 你好 (40) + world (~60) fills
        // line 1; 测试 (40) + engine (~62) line 2; 渲染真实 (80) line 3 —
        // the assert is the cross-check, this comment just documents the
        // arithmetic that makes 3 lines plausible.
        let blitz_nid = doc.query_selector("#t").unwrap().expect("#t");
        let theirs = element_rect(&doc, blitz_nid);
        assert_close("mixed wrap height vs blitz", t.height, theirs.height);
        // One line of 20px text is 24px; height must be a whole multiple.
        let lines = (t.height / 24.0).round();
        assert!(lines >= 2.0 && lines <= 4.0, "plausible line count: {lines} (h={})", t.height);
        assert!((t.height - lines * 24.0).abs() < EPS as f32, "height = lines × 1.2×fs: {}", t.height);
    }

    /// Baseline placement model (batch 3b): parley 0.10 quantized Chrome-style
    /// metrics — round(ascent)/round(descent) separately, below keeps the
    /// larger leading half. The Noto SC fixture's natural extent (1.448em)
    /// exceeds the 1.2em line box, so leading is NEGATIVE and the baseline
    /// lands at exactly fs below the line top for 12/16/20/24px — the cramped
    /// CJK look, locked numerically.
    #[test]
    fn baseline_model_is_parley_quantized() {
        use crate::diting_layout::text::baseline_offset;

        let fonts = fixture_fonts();
        for fs in [12.0f32, 16.0, 20.0, 24.0] {
            let m = fonts.metrics(fs, false).expect("metrics");
            // Noto Sans SC: ascender 1.16em, descender 0.288em (positive,
            // below-baseline), line gap 0.
            assert!((m.ascent - fs * 1.16).abs() < 0.05, "ascent@{fs}: {}", m.ascent);
            assert!((m.descent - fs * 0.288).abs() < 0.05, "descent@{fs}: {}", m.descent);
            assert!(m.line_gap.abs() < 0.05, "line_gap@{fs}: {}", m.line_gap);
            let b = baseline_offset(m.ascent, m.descent, line_height(fs));
            assert!((b - fs).abs() < 1e-6, "baseline@{fs}: {b} (want exactly fs)");
        }
    }

    /// The paint half (batch 3b): our swash-rasterized tile vs blitz's
    /// parley+vello_cpu painting of the same fixture run. Rasterizers differ
    /// (zeno alpha raster vs vello path AA) so the contract is INK EXTENTS,
    /// not pixels: the ink box above/below the baseline and its x extent
    /// must agree within 2px — enough to catch a wrong baseline model,
    /// wrong metric source, or wrong glyph advances; tight enough to be a
    /// real parity claim.
    #[test]
    fn raster_ink_extents_match_blitz() {
        use crate::diting_layout::text::baseline_offset;

        let html = r#"<body><div id="t">你好gapa渲染</div></body>"#;
        for fs in [16.0f32, 24.0] {
            let sheet = format!("body {{ margin: 0; }} #t {{ font-size: {fs}px; }}");
            let doc = blitz_doc(html, &sheet);

            // Paint the blitz doc: white fill then paint_scene (same path as
            // screenshot::render_html_to_png).
            let (w, h) = (400u32, 80u32);
            let mut doc = doc;
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

            // Ink bbox in the blitz image (black text on white, 50% coverage).
            let ink = |px: &[u8]| px[0] < 128 && px[1] < 128 && px[2] < 128;
            let mut bbox: Option<(u32, u32, u32, u32)> = None;
            for y in 0..h {
                for x in 0..w {
                    if !ink(&buffer[((y * w + x) * 4) as usize..]) {
                        continue;
                    }
                    bbox = Some(match bbox {
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                        None => (x, y, x, y),
                    });
                }
            }
            let (bx0, by0, bx1, by1) = bbox.expect("blitz painted some ink");

            // Our tile: same text, same fixture bytes, baseline per the model.
            let raster = fixture_fonts().rasterize("你好gapa渲染", fs, false, [0, 0, 0, 255], line_height(fs));
            let (ox0, oy0, ox1, oy1) = raster.ink_bbox().expect("our raster has ink");
            let fonts = fixture_fonts();
            let m = fonts.metrics(fs, false).unwrap();
            let baseline = baseline_offset(m.ascent, m.descent, line_height(fs));

            // Line 1's box top is y=0 in the page (body margin 0), so blitz's
            // ink distances decode against `baseline` directly.
            let d = |what: &str, ours: f32, theirs: f32| {
                assert!(
                    (ours - theirs).abs() <= 2.0,
                    "{what}@{fs}px: ours={ours} blitz={theirs} (diff {})",
                    (ours - theirs).abs()
                );
            };
            d("ink top above baseline", raster.baseline - oy0 as f32, baseline - by0 as f32);
            d("ink bottom below baseline", oy1 as f32 - raster.baseline, by1 as f32 - baseline);
            d("ink left", ox0 as f32, bx0 as f32);
            d("ink right", ox1 as f32, bx1 as f32);
        }
    }

    /// Measure/paint wrap parity (batch 4a): the greedy breaker that sizes
    /// the leaf must be the same one the wrapped raster paints. Locked via
    /// the tile's ink-band structure, not internal counts — a band per
    /// line, and the box the tile represents is exactly `lines × lh` tall.
    #[test]
    fn wrapped_raster_matches_measure_lines() {
        use crate::diting_layout::text::baseline_offset;

        let fonts = fixture_fonts();
        let (text, fs, wrap_at) = ("谛听引擎渲染测试文本行", 20.0f32, 105.0f32);
        let lh = line_height(fs);
        let tokens = text::tokens_of(text, fs, false, &fonts);
        let lines = text::greedy_wrap(&tokens, Some(wrap_at));
        assert_eq!(lines.len(), 3, "5 glyphs per 105px line, 11 glyphs → 3 lines");
        assert!((lines[0].width - 100.0).abs() < 0.05, "5 × 1em");

        let r = fonts.rasterize_wrapped(text, fs, false, [0, 0, 0, 255], wrap_at, lh);
        // One ink band per wrapped line (band = maximal run of ink rows).
        let band_tops: Vec<usize> = {
            let mut tops = Vec::new();
            let mut last_row: Option<usize> = None;
            for y in 0..r.height {
                let has_ink = (0..r.width).any(|x| r.data[(y * r.width + x) * 4 + 3] >= 128);
                if has_ink && last_row.is_none_or(|p| y > p + 1) {
                    tops.push(y);
                }
                if has_ink {
                    last_row = Some(y);
                }
            }
            tops
        };
        assert_eq!(band_tops.len(), 3, "ink bands: one per line, got {band_tops:?}");

        // Tile geometry: first baseline at the model's offset from the box
        // top, height spanning every line's metrics extent (with the ±1px
        // slack), and the 3-line box itself (3 × lh) fits inside.
        let m = fonts.metrics(fs, false).unwrap();
        let b0 = baseline_offset(m.ascent, m.descent, lh);
        assert!((r.baseline + r.top - b0).abs() < 0.51);
        let last_baseline = (2.0 * lh).round() + b0;
        let want_h =
            ((last_baseline + m.descent).ceil() - (b0 - m.ascent).floor()) as usize + 2;
        assert_eq!(r.height, want_h);
        assert!(r.top <= 0.0 && r.top > -fs, "tile starts above/at the box top");
    }

    /// First pixels of the diting paint stack vs blitz (batch 4a): a solid
    /// background block and a 3-line wrapped CJK run, both engines painting
    /// the SAME fixture glyphs. Contract: background bbox exact (both fill
    /// taffy-rounded integer rects), one ink band per line with tops within
    /// the batch-3b ink tolerance, ink bbox edges within ±2px.
    #[test]
    fn paint_bg_and_wrapped_text_match_blitz() {
        let html = r#"<body><div id="t">谛听引擎渲染测试文本行</div></body>"#;
        let sheet = "body { margin: 0; } #t { width: 105px; background: rgb(198,40,40); font-size: 20px; }";

        // Blitz side: white fill, then paint_scene (the screenshot path).
        let doc = blitz_doc(html, sheet);
        let (w, h) = (200u32, 160u32);
        let mut doc = doc;
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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

        // Our side: the real production path — parse, cascade, layout with
        // paint items, execute onto a white canvas.
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut ours);
        assert!(
            items.iter().any(|i| matches!(i, PaintItem::Bg { .. })),
            "the bg block must emit a Bg item"
        );
        assert_eq!(items.iter().filter(|i| matches!(i, PaintItem::Text { .. })).count(), 1);

        // Same scans on both buffers: bg = red-ish, ink = dark (a 50/50 AA
        // blend of black on red is (99,20,20) — still "dark" by this rule,
        // and identically so on both sides).
        let is_bg = |p: &[u8]| (p[0] as i32 - 198).abs() < 30 && (p[1] as i32 - 40).abs() < 30 && (p[2] as i32 - 40).abs() < 30;
        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;

        fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> Option<(usize, usize, usize, usize)> {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if !hit(&buf[(y * w + x) * 4..]) {
                        continue;
                    }
                    b = Some(match b {
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                        None => (x, y, x, y),
                    });
                }
            }
            b
        }
        /// Maximal runs of rows containing any hit pixel → their top rows.
        fn band_tops(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> Vec<usize> {
            let mut tops = Vec::new();
            let mut last_row: Option<usize> = None;
            for y in 0..h {
                let has = (0..w).any(|x| hit(&buf[(y * w + x) * 4..]));
                if has && last_row.is_none_or(|p| y > p + 1) {
                    tops.push(y);
                }
                if has {
                    last_row = Some(y);
                }
            }
            tops
        }

        // Background: exact bbox through pixels (rects were exact in the
        // batch-2 tests; paint must not lose that).
        let (obx0, oby0, obx1, oby1) = bbox(&ours.data, w as usize, h as usize, is_bg).expect("our bg painted");
        let (bbx0, bby0, bbx1, bby1) = bbox(&blitz, w as usize, h as usize, is_bg).expect("blitz bg painted");
        for (what, o, b) in [
            ("bg left", obx0, bbx0),
            ("bg top", oby0, bby0),
            ("bg right", obx1, bbx1),
            ("bg bottom", oby1, bby1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }

        // Ink bands: one per wrapped line, tops within tolerance.
        let o_tops = band_tops(&ours.data, w as usize, h as usize, is_ink);
        let b_tops = band_tops(&blitz, w as usize, h as usize, is_ink);
        assert_eq!(o_tops.len(), 3, "our ink bands: {o_tops:?}");
        assert_eq!(b_tops.len(), 3, "blitz ink bands: {b_tops:?}");
        for (i, (o, b)) in o_tops.iter().zip(&b_tops).enumerate() {
            assert!(
                (*o as i64 - *b as i64).abs() <= 2,
                "ink band {i} top: ours={o} blitz={b}"
            );
        }

        // Ink bbox within the batch-3b ink tolerance.
        let (oix0, oiy0, oix1, oiy1) = bbox(&ours.data, w as usize, h as usize, is_ink).expect("our ink painted");
        let (bix0, biy0, bix1, biy1) = bbox(&blitz, w as usize, h as usize, is_ink).expect("blitz ink painted");
        for (what, o, b) in [
            ("ink left", oix0, bix0),
            ("ink top", oiy0, biy0),
            ("ink right", oix1, bix1),
            ("ink bottom", oiy1, biy1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 2, "{what}: ours={o} blitz={b}");
        }
    }

    /// Border + padding through pixels (batch 4b): a 6px solid border and
    /// 8px padding around the same 3-line run. The border eats into the
    /// layout (authored width stays content-box, wrap width unchanged at
    /// 105px, border-box grows to 133×100) and paints as four bands over
    /// the background. Contract: blue band bbox == the element rect exactly,
    /// visible red interior inset by the border, ink bands unchanged in
    /// count and shifted by border+padding.
    #[test]
    fn paint_border_and_padding_match_blitz() {
        let html = r#"<body><div id="t">谛听引擎渲染测试文本行</div></body>"#;
        let sheet = "body { margin: 0; } #t { width: 105px; padding: 8px; border: 6px solid rgb(20,60,200); background: rgb(198,40,40); font-size: 20px; }";

        let doc = blitz_doc(html, sheet);
        let (w, h) = (200u32, 200u32);
        let mut doc = doc;
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut ours);
        assert!(
            items.iter().any(|i| matches!(i, PaintItem::Border { .. })),
            "the border must emit a Border item"
        );

        let is_border =
            |p: &[u8]| (p[0] as i32 - 20).abs() < 30 && (p[1] as i32 - 60).abs() < 30 && (p[2] as i32 - 200).abs() < 30;
        let is_bg = |p: &[u8]| {
            (p[0] as i32 - 198).abs() < 30 && (p[1] as i32 - 40).abs() < 30 && (p[2] as i32 - 40).abs() < 30
        };
        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;

        fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> (usize, usize, usize, usize) {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if !hit(&buf[(y * w + x) * 4..]) {
                        continue;
                    }
                    b = Some(match b {
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                        None => (x, y, x, y),
                    });
                }
            }
            b.expect("bbox: no hit pixels")
        }
        fn band_tops(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> Vec<usize> {
            let mut tops = Vec::new();
            let mut last_row: Option<usize> = None;
            for y in 0..h {
                let has = (0..w).any(|x| hit(&buf[(y * w + x) * 4..]));
                if has && last_row.is_none_or(|p| y > p + 1) {
                    tops.push(y);
                }
                if has {
                    last_row = Some(y);
                }
            }
            tops
        }

        // Border bands span the border-box: exact outer edges.
        let (obx0, oby0, obx1, oby1) = bbox(&ours.data, w as usize, h as usize, is_border);
        let (bbx0, bby0, bbx1, bby1) = bbox(&blitz, w as usize, h as usize, is_border);
        for (what, o, b) in [
            ("border left", obx0, bbx0),
            ("border top", oby0, bby0),
            ("border right", obx1, bbx1),
            ("border bottom", oby1, bby1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }
        // And the border-box itself is 133×100: content 105 + 2×(6+8) wide,
        // 3×24 + 2×(6+8) tall.
        assert_eq!((obx1 - obx0 + 1, oby1 - oby0 + 1), (133, 100), "our border box");

        // The visible red interior is the border-box inset by the 6px bands.
        let (orx0, ory0, orx1, ory1) = bbox(&ours.data, w as usize, h as usize, is_bg);
        let (brx0, bry0, brx1, bry1) = bbox(&blitz, w as usize, h as usize, is_bg);
        for (what, o, b) in [
            ("red left", orx0, brx0),
            ("red top", ory0, bry0),
            ("red right", orx1, brx1),
            ("red bottom", ory1, bry1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }
        assert_eq!((orx0, ory0), (6, 6), "our red interior inset by the border");

        // Ink: still 3 bands (wrap width unchanged), tops shifted by
        // border+padding, matching blitz within the ink tolerance.
        let o_tops = band_tops(&ours.data, w as usize, h as usize, is_ink);
        let b_tops = band_tops(&blitz, w as usize, h as usize, is_ink);
        assert_eq!(o_tops.len(), 3, "our ink bands: {o_tops:?}");
        assert_eq!(b_tops.len(), 3, "blitz ink bands: {b_tops:?}");
        for (i, (o, b)) in o_tops.iter().zip(&b_tops).enumerate() {
            assert!(
                (*o as i64 - *b as i64).abs() <= 2,
                "ink band {i} top: ours={o} blitz={b}"
            );
        }
    }

    /// Overflow clipping through pixels (batch 4c): a fixed-height
    /// (2-line) bordered box holding a 3-line run. The padding box is the
    /// clip rect — lines 1-2 visible, line 3 entirely beneath the clip,
    /// on both engines.
    #[test]
    fn paint_overflow_hidden_clips_match_blitz() {
        let html = r#"<body><div id="t">谛听引擎渲染测试文本行</div></body>"#;
        let sheet = "body { margin: 0; } #t { width: 105px; height: 48px; overflow: hidden; background: rgb(198,40,40); border: 6px solid rgb(20,60,200); font-size: 20px; }";
        // Border box 117×60 (content 105×48 + 2×6 border); clip = padding
        // box (6,6)→(111,54); baselines 26/50/74 → line 3's ink starts
        // ~57, fully under the clip.

        let doc = blitz_doc(html, sheet);
        let (w, h) = (200u32, 200u32);
        let mut doc = doc;
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut ours);
        assert!(
            items.iter().any(|i| matches!(i, PaintItem::Clip { .. })),
            "overflow: hidden must emit a Clip item"
        );

        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;
        fn band_tops(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> Vec<usize> {
            let mut tops = Vec::new();
            let mut last_row: Option<usize> = None;
            for y in 0..h {
                let has = (0..w).any(|x| hit(&buf[(y * w + x) * 4..]));
                if has && last_row.is_none_or(|p| y > p + 1) {
                    tops.push(y);
                }
                if has {
                    last_row = Some(y);
                }
            }
            tops
        }

        // Line 3 is gone on both sides: two bands, tops aligned, and no ink
        // anywhere at or below the clip's bottom edge (row 54).
        let o_tops = band_tops(&ours.data, w as usize, h as usize, is_ink);
        let b_tops = band_tops(&blitz, w as usize, h as usize, is_ink);
        assert_eq!(o_tops.len(), 2, "our visible bands: {o_tops:?}");
        assert_eq!(b_tops.len(), 2, "blitz visible bands: {b_tops:?}");
        for (i, (o, b)) in o_tops.iter().zip(&b_tops).enumerate() {
            assert!(
                (*o as i64 - *b as i64).abs() <= 2,
                "ink band {i} top: ours={o} blitz={b}"
            );
        }
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            for y in 54..h as usize {
                for x in 0..w as usize {
                    assert!(
                        !is_ink(&buf[(y * w as usize + x) * 4..]),
                        "{name}: ink leaked below the clip at ({x},{y})"
                    );
                }
            }
        }

        // The overflow must NOT clip the element's own box paint: the
        // border and background still span the full 117×60 border box.
        let is_border =
            |p: &[u8]| (p[0] as i32 - 20).abs() < 30 && (p[1] as i32 - 60).abs() < 30 && (p[2] as i32 - 200).abs() < 30;
        for (buf, name) in [(&ours.data, "ours"), (&blitz, "blitz")] {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    if is_border(&buf[(y * w as usize + x) * 4..]) {
                        b = Some(match b {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            let b = b.unwrap_or_else(|| panic!("{name}: no border pixels"));
            assert_eq!((b.2 - b.0 + 1, b.3 - b.1 + 1), (117, 60), "{name} border box");
        }
    }

    /// Mixed-run text paints (batch 4d): words around an inline element —
    /// `汉字<b>加粗</b>混合` — the most common real-page text shape. The
    /// div's own words paint from their word-leaf boxes, the <b>'s pure run
    /// paints at its leaf; whole lines wrap in the flex row. Contract: one
    /// line at full width, two bands when the wrapper forces a wrap, ink
    /// extents within tolerance both times.
    #[test]
    fn paint_mixed_run_text_matches_blitz() {
        let html = r#"<body><div id="t">汉字<b id="b">加粗</b>混合</div></body>"#;
        for (width, want_bands) in [(800.0f32, 1usize), (80.0, 2)] {
            let sheet = format!(
                "body {{ margin: 0; }} #t {{ width: {width}px; font-size: 20px; }}"
            );
            let doc = blitz_doc(html, &sheet);
            let (w, h) = (200u32, 200u32);
            let mut doc = doc;
            let blitz =
                anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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

            let tree = crate::diting_dom::tree_sink::parse_html(html);
            let rules = diting_css::parse_stylesheet(&sheet);
            let styles = our_styles(&tree, &rules);
            let fonts = fixture_fonts();
            let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
            let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
            paint::execute(&items, &fonts, &mut ours);
            // 4 word leaves (汉字混合) + 1 run leaf (the <b>'s 加粗).
            let text_items = items
                .iter()
                .filter(|i| matches!(i, PaintItem::Text { .. }))
                .count();
            assert_eq!(text_items, 5, "4 CJK word leaves + 1 bold run leaf");

            let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;
            fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> (usize, usize, usize, usize) {
                let mut b: Option<(usize, usize, usize, usize)> = None;
                for y in 0..h {
                    for x in 0..w {
                        if hit(&buf[(y * w + x) * 4..]) {
                            b = Some(match b {
                                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                                None => (x, y, x, y),
                            });
                        }
                    }
                }
                b.expect("ink bbox: no hit pixels")
            }
            fn band_tops(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> Vec<usize> {
                let mut tops = Vec::new();
                let mut last_row: Option<usize> = None;
                for y in 0..h {
                    let has = (0..w).any(|x| hit(&buf[(y * w + x) * 4..]));
                    if has && last_row.is_none_or(|p| y > p + 1) {
                        tops.push(y);
                    }
                    if has {
                        last_row = Some(y);
                    }
                }
                tops
            }

            let o_tops = band_tops(&ours.data, w as usize, h as usize, is_ink);
            let b_tops = band_tops(&blitz, w as usize, h as usize, is_ink);
            assert_eq!(o_tops.len(), want_bands, "our bands @w{width}: {o_tops:?}");
            assert_eq!(b_tops.len(), want_bands, "blitz bands @w{width}: {b_tops:?}");
            for (i, (o, b)) in o_tops.iter().zip(&b_tops).enumerate() {
                assert!(
                    (*o as i64 - *b as i64).abs() <= 2,
                    "band {i} top @w{width}: ours={o} blitz={b}"
                );
            }

            let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_ink);
            let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_ink);
            for (what, o, b) in [
                ("ink left", ox0, bx0),
                ("ink top", oy0, by0),
                ("ink right", ox1, bx1),
                ("ink bottom", oy1, by1),
            ] {
                assert!(
                    (o as i64 - b as i64).abs() <= 2,
                    "{what} @w{width}: ours={o} blitz={b}"
                );
            }
        }
    }

    /// Replaced-box paint, cross-checkable half (batch 5a): an unloaded
    /// img's own CSS background IS a shared paint item — blitz paints it
    /// for the element just like any box, we paint it via the same Bg
    /// path (the img taffy leaf is in node_map). With no alt there is no
    /// ink on either side: blitz's draw_image is a no-op without raster
    /// data, and we paint neither placeholder (author bg present) nor
    /// text. Background bbox exact ±1.
    #[test]
    fn paint_img_background_match_blitz() {
        let html = r#"<body><img id="t" src="https://example.invalid/x.png" width="100" height="50"></body>"#;
        let sheet = "body { margin: 0; } #t { background: rgb(198,40,40); }";

        let doc = blitz_doc(html, sheet);
        let (w, h) = (200u32, 200u32);
        let mut doc = doc;
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut ours);

        // Policy bookkeeping: the Replaced item is there but suppressed to
        // box-less (author background already reads as the box).
        let replaced = items
            .iter()
            .find_map(|i| match i {
                PaintItem::Replaced { fill_placeholder, .. } => Some(*fill_placeholder),
                _ => None,
            })
            .expect("img must emit a Replaced item");
        assert!(!replaced, "authored background suppresses the gray placeholder");

        let is_bg = |p: &[u8]| p[0] > 130 && p[1] < 110 && p[2] < 110;
        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;
        fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> (usize, usize, usize, usize) {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if hit(&buf[(y * w + x) * 4..]) {
                        b = Some(match b {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            b.expect("bbox: no hit pixels")
        }
        let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_bg);
        let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_bg);
        for (what, o, b) in [
            ("bg left", ox0, bx0),
            ("bg top", oy0, by0),
            ("bg right", ox1, bx1),
            ("bg bottom", oy1, by1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            assert!(
                !(0..w as usize * h as usize).any(|i| is_ink(&buf[i * 4..])),
                "{name}: no ink without an alt on an unloaded img"
            );
        }
    }

    /// Replaced-box placeholder, OUR policy half (batch 5a): upstream blitz
    /// paints nothing for an unloaded img, so the gray box + alt run are
    /// locked structurally — placeholder bbox == the img rect exactly
    /// (geometry itself is the batch-2 cross-checked part), alt ink lives
    /// inside the box, alt="" degrades to box-only, and the box honors the
    /// attribute-derived aspect ratio.
    #[test]
    fn paint_replaced_placeholder_structural() {
        let html = r#"<body><img id="t" src="https://example.invalid/x.png" width="100" alt="谛听图"></body>"#;
        let sheet = "body { margin: 0; }";
        // width=100 with no height → ratio 2:1 → 100×50 at (0,0).

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let img_rect = rects[&tree.query_selector("#t").unwrap().unwrap()];
        assert_eq!((img_rect.width, img_rect.height), (100.0, 50.0), "aspect-ratio derived box");

        let (replaced_rect, alt, fill) = items
            .iter()
            .find_map(|i| match i {
                PaintItem::Replaced { rect, alt, fill_placeholder } => {
                    Some((*rect, alt.clone(), *fill_placeholder))
                }
                _ => None,
            })
            .expect("img must emit a Replaced item");
        assert!(fill, "no authored background → gray placeholder shows");
        assert!(alt.is_some(), "alt attribute resolves to a run");
        assert_eq!(
            (
                replaced_rect.x.round(),
                replaced_rect.y.round(),
                replaced_rect.width.round(),
                replaced_rect.height.round()
            ),
            (
                img_rect.x.round(),
                img_rect.y.round(),
                img_rect.width.round(),
                img_rect.height.round()
            ),
            "placeholder covers exactly the img border box"
        );

        // Pixels: gray box exactly on the rect, dark alt ink inside it.
        let (w, h) = (200usize, 200usize);
        let mut ours = paint::Canvas::new_filled(w, h, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut ours);
        let is_gray = |p: &[u8]| {
            (p[0] as i32 - 224).abs() < 6
                && (p[1] as i32 - 224).abs() < 6
                && (p[2] as i32 - 224).abs() < 6
        };
        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;
        let bbox = |hit: &dyn Fn(&[u8]) -> bool| -> (usize, usize, usize, usize) {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if hit(&ours.data[(y * w + x) * 4..]) {
                        b = Some(match b {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            b.expect("bbox: no hit pixels")
        };
        assert_eq!(bbox(&is_gray), (0, 0, 99, 49), "gray box covers the 100×50 rect");
        let (ix0, iy0, ix1, iy1) = bbox(&is_ink);
        assert!(
            ix1 <= 99 && iy1 <= 49,
            "alt ink inside the box: ({ix0},{iy0})-({ix1},{iy1})"
        );

        // alt="" is decorative: box only, zero ink.
        let html = r#"<body><img id="t" src="https://example.invalid/x.png" width="100" alt=""></body>"#;
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let styles = our_styles(&tree, &rules);
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut canvas = paint::Canvas::new_filled(w, h, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut canvas);
        assert_eq!(bbox(&is_gray), (0, 0, 99, 49), "empty alt still boxes");
        assert!(
            !(0..w * h).any(|i| is_ink(&canvas.data[i * 4..])),
            "alt=\"\" paints no text"
        );
    }

    /// Alt overflow clips at the box (batch 6e): a long alt on a SHORT box
    /// wraps past the bottom and the excess ink is cut there — no ink below
    /// the box edge, ink still present above it (the clip is the only thing
    /// that changed vs pre-6e overflow).
    #[test]
    fn paint_replaced_alt_clips_to_box() {
        let html = r#"<body><img id="t" src="https://example.invalid/x.png" width="120" height="30" alt="谛听辨真假 一行又一行 超出盒底"></body>"#;
        let sheet = "body { margin: 0; }";

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let fonts = fixture_fonts();
        let styles = our_styles(&tree, &rules);
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);

        let (w, h) = (200usize, 300usize);
        let mut canvas = paint::Canvas::new_filled(w, h, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut canvas);

        let is_gray = |p: &[u8]| {
            (p[0] as i32 - 224).abs() < 6
                && (p[1] as i32 - 224).abs() < 6
                && (p[2] as i32 - 224).abs() < 6
        };
        let is_ink = |p: &[u8]| p[0] < 110 && p[1] < 110 && p[2] < 110;

        // Ink exists inside the box…
        assert!(
            (0..50 * w).any(|i| is_ink(&canvas.data[i * 4..])),
            "clipped alt still shows its first lines inside the box"
        );
        // …and nowhere below the box bottom (y=30).
        assert!(
            !(50 * w..h * w).any(|i| is_ink(&canvas.data[i * 4..])),
            "alt ink cut at the box bottom edge"
        );
        // The placeholder gray also stops at the box.
        let mut last_gray_row = 0usize;
        for y in (0..h).rev() {
            if (0..w).any(|x| is_gray(&canvas.data[(y * w + x) * 4..])) {
                last_gray_row = y;
                break;
            }
        }
        assert!(last_gray_row <= 29 + 1, "gray ends at the box bottom");
    }

    /// Per-corner + elliptical radii (batch 7c): `border-radius: 0 40px`
    /// sharpens the left corners and rounds only the right pair; the
    /// `rx ry` slash form makes true ellipses. Contract: pixel classes
    /// (fill vs background) sampled well clear of the AA curves must match
    /// blitz on every probe.
    #[test]
    fn paint_per_corner_radius_matches_blitz() {
        let html = r#"<body><div id="t"></div></body>"#;
        // Two-value expansion: TL/BR = 0, TR/BL = 40 → right side pill,
        // left side square. Plus a separate elliptical case below.
        let sheet = "body { margin: 0; } #t { width: 100px; height: 100px; background: rgb(200,40,40); border-radius: 0 40px; }";
        let (w, h) = (200u32, 200u32);
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);

        let px = |buf: &[u8], x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        let is_red = |c: (u8, u8, u8)| c.0 > 150 && c.1 < 110 && c.2 < 110;

        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            // Sharp left corners: filled right up to near the corner.
            assert!(is_red(px(buf, 2, 2)), "{name} sharp TL corner filled: {:?}", px(buf, 2, 2));
            // Rounded right corners: deep inside the 40px cut is background.
            assert!(
                !is_red(px(buf, 97, 3)),
                "{name} rounded TR corner cut: {:?}",
                px(buf, 97, 3)
            );
            // Right edge midpoint is on the straight part: filled.
            assert!(is_red(px(buf, 97, 50)), "{name} right mid-edge filled");
            // Center filled.
            assert!(is_red(px(buf, 50, 50)), "{name} center filled");
        }

        // Elliptical: all four corners rx=30 ry=10 via the slash syntax.
        let sheet = "body { margin: 0; } #t { width: 100px; height: 100px; background: rgb(200,40,40); border-radius: 30px / 10px; }";
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            // Inside the shallow ellipse near the top-left: point (25, 5)
            // sits ((25-30)/30)²+((5-10)/10)² ≈ 0.03+0.25 < 1 → filled.
            assert!(
                is_red(px(buf, 25, 5)),
                "{name} inside TL ellipse filled: {:?}",
                px(buf, 25, 5)
            );
            // Outside it: (5,9): ((5-30)/30)²+((9-10)/10)² ≈ 0.7+0.01 —
            // still inside; take (2,2): ≈0.756+0.64 > 1 → cut.
            assert!(
                !is_red(px(buf, 2, 2)),
                "{name} outside TL ellipse cut: {:?}",
                px(buf, 2, 2)
            );
        }
    }

    /// Rounded overflow clipping (batch 7d): an `overflow: hidden` +
    /// `border-radius` box clips its DESCENDANTS along the curve — the
    /// child's background fills the straight edges but is cut inside the
    /// corner radius zone on both engines (upstream clips through the
    /// rounded padding_box_path).
    #[test]
    fn paint_rounded_overflow_clip_matches_blitz() {
        let html = r#"<body><div id="t"><div id="child"></div></div></body>"#;
        let sheet = "body { margin: 0; }
            #t { width: 100px; height: 100px; overflow: hidden; border-radius: 30px; position: relative; height: 100px; }
            #child { position: absolute; left: 0px; top: 0px; width: 100px; height: 100px; background: rgb(40,140,200); }";
        let (w, h) = (200u32, 200u32);
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);

        let is_blue = |c: (u8, u8, u8)| c.2 > 130 && c.0 < 110 && c.1 < 190;

        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            let px = |x: usize, y: usize| {
                let i = (y * w as usize + x) * 4;
                (buf[i], buf[i + 1], buf[i + 2])
            };
            // Straight-edge midpoints survive the clip.
            assert!(is_blue(px(50, 3)), "{name} top mid-edge blue: {:?}", px(50, 3));
            assert!(is_blue(px(50, 96)), "{name} bottom mid-edge blue");
            // Deep corner zones are clipped to white on BOTH engines.
            assert!(
                !is_blue(px(3, 3)),
                "{name} TL corner cut by the curve: {:?}",
                px(3, 3)
            );
            assert!(!is_blue(px(96, 3)), "{name} TR corner cut");
            assert!(!is_blue(px(3, 96)), "{name} BL corner cut");
            assert!(!is_blue(px(96, 96)), "{name} BR corner cut");
        }
    }

    /// Per-tag replaced sizing (batch 7a), cross-checked against blitz's
    /// rects. video/iframe carry NO intrinsic ratio while canvas DOES
    /// (ratio computed from the attribute-or-default size). The ratio only
    /// diverges when ONE axis is set and the other transfers: a CSS-width
    /// canvas derives its height at the attr ratio (2:1 default), a CSS-
    /// width video keeps the 150px default height. iframe adds its UA
    /// `2px inset` border to every authored dimension.
    #[test]
    fn replaced_per_tag_sizes_match_blitz() {
        let html = r#"<body>
            <video id="v" width="600"></video>
            <iframe id="f" width="600"></iframe>
            <canvas id="c" width="600"></canvas>
            <video id="vd"></video>
            <canvas id="c2" style="width: 800px;"></canvas>
            <video id="v2" style="width: 800px;"></video>
        </body>"#;
        let sheet = "body { margin: 0; }";
        let (doc, tree, _styles, rects) = both_engines(html, sheet);

        let expect: &[(&str, f32, f32)] = &[
            // video: no ratio → 600×150.
            ("#v", 600.0, 150.0),
            // iframe: UA `2px inset` border wraps the attr size → 604×154
            // (blitz default.css carries the same rule; browsers do too).
            ("#f", 604.0, 154.0),
            // canvas ratio = 600/150 from the attrs themselves, so the
            // "missing" height transfers right back to 150.
            ("#c", 600.0, 150.0),
            ("#vd", 300.0, 150.0),
            // Ratio transfer through a CSS axis: canvas 800 wide → 400 tall;
            // video has no ratio → stays at the 150 default height.
            ("#c2", 800.0, 400.0),
            ("#v2", 800.0, 150.0),
        ];
        for (sel, ew, eh) in expect {
            let blitz_nid = doc.query_selector(sel).unwrap().expect(sel);
            let our_nid = tree.query_selector(sel).unwrap().expect(sel);
            let theirs = element_rect(&doc, blitz_nid);
            let ours = rects[&our_nid];
            assert_close(&format!("{sel} w"), ours.width, theirs.width as f64);
            assert_close(&format!("{sel} h"), ours.height, theirs.height as f64);
            assert_eq!(ours.width, *ew, "{sel} expected width");
            assert_eq!(ours.height, *eh, "{sel} expected height");
        }

        // Policy half: a bare <iframe> placeholder paints the gray box and
        // NO alt ink even with an alt attribute (alt is img-only).
        let html =
            r#"<body><iframe id="f" width="100" height="50" alt="not-an-img"></iframe></body>"#;
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let replaced = items.iter().find_map(|i| match i {
            PaintItem::Replaced { alt, fill_placeholder, .. } => {
                Some((*fill_placeholder, alt.clone()))
            }
            _ => None,
        });
        let (fill, alt) = replaced.expect("iframe emits a Replaced item");
        assert!(fill, "unstyled iframe paints the gray placeholder");
        assert!(alt.is_none(), "alt is img-only — iframe ignores the attribute");
    }

    /// A two-quadrant RGBA test image (red top-left/bottom-right, blue the
    /// others) as both a data: URL (our pipeline decodes the src attribute)
    /// and a decoded [`image::DecodedImage`] (the blitz side gets the same
    /// bytes injected straight into its img node — upstream loads images
    /// through the net layer, which the harness does not wire).
    fn fixture_image_data_url(w: u32, h: u32) -> (String, image::DecodedImage) {
        use base64::Engine as _;
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let red = (x < w / 2) ^ (y < h / 2);
                rgba.extend_from_slice(if red { &[200, 40, 40, 255] } else { &[40, 40, 200, 255] });
            }
        }
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD
                .encode(&png_bytes)
        );
        let img = image::DecodedImage::new(w, h, rgba);
        (url, img)
    }

    /// Render the blitz side with `image` injected into the element matching
    /// `selector` (before its layout pass, so natural size derives from the
    /// same decoded dims ours does).
    fn blitz_render_with_image(
        html: &str,
        sheet: &str,
        selector: &str,
        image: &image::DecodedImage,
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        use blitz_dom::node::{ImageData, RasterImageData, SpecialElementData};
        // Inject BEFORE the first resolve: the element's natural size
        // derives from the image during layout (blitz-dom/src/layout/mod.rs
        // reads SpecialElementData::Image), so the box comes out the same
        // as ours. (The product path — image arriving after a first layout
        // — needs damage-driven relayout, not modeled by the harness.)
        let mut doc = blitz_doc_unresolved(html, sheet);
        let img_id = doc
            .query_selector(selector)
            .expect("selector parses")
            .expect("img node exists");
        let node = doc.get_node_mut(img_id).expect("node");
        node.element_data_mut()
            .expect("img is an element")
            .special_data = SpecialElementData::Image(Box::new(ImageData::Raster(
            RasterImageData::new(
                image.width,
                image.height,
                std::sync::Arc::new(image.rgba.as_ref().clone()),
            ),
        )));
        for _ in 0..2 {
            doc.resolve(0.0);
        }

        let mut doc = doc;
        anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        )
    }

    fn our_render(html: &str, sheet: &str, w: usize, h: usize) -> paint::Canvas {
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let fonts = fixture_fonts();
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW, VH);
        let mut canvas = paint::Canvas::new_filled(w, h, [255, 255, 255, 255]);
        paint::execute(&items, &fonts, &mut canvas);
        canvas
    }

    /// Float paint level (8f): a float paints ABOVE the in-flow content of
    /// its band and BELOW positioned z-auto boxes (CSS 2.1 App. E — the
    /// same damage.rs paint-level model as batch 6a, float = level 1).
    /// The positioned probe is an absolutely-positioned box pinned over
    /// the float's area (a relative box with no insets would take its
    /// static position in the flow column and never overlap).
    #[test]
    fn float_paint_level_between_flow_and_positioned() {
        let px = |buf: &[u8], w: u32, x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        let html = r#"<body>
            <div id="fl"></div>
            <div id="flow"></div>
            <div id="pos"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #fl { float: left; width: 100px; height: 60px; background: rgb(200,40,40); }
            #flow { width: 150px; height: 40px; background: rgb(40,40,200); }
            #pos { position: absolute; left: 0px; top: 0px; width: 60px; height: 80px; background: rgb(40,180,40); }
        "#;
        let (w, h) = (250u32, 120u32);
        let ours = our_render(html, sheet, w as usize, h as usize);

        // Green positioned box pinned at (0,0) covers the red float:
        // green there proves floats sit below positioned z-auto.
        let (r0, g0, b0) = px(&ours.data, w, 30, 30);
        assert!(g0 > 150 && r0 < 100 && b0 < 100,
            "positioned green paints over the float: got {r0},{g0},{b0}");

        // Inside the flow column (beside the float), the wrapped #flow's
        // blue stands where neither float nor positioned ink reaches.
        let (r1, g1, b1) = px(&ours.data, w, 150, 20);
        assert!(b1 > 150 && r1 < 100,
            "in-flow column shows blue beside the float: got {r1},{g1},{b1}");

        // Float area outside the green overlay shows RED — the float's
        // own ink stands where the higher-level boxes don't reach.
        let (r2, g2, b2) = px(&ours.data, w, 80, 30);
        assert!(r2 > 150 && g2 < 100,
            "float bg visible below the green overlay: got {r2},{g2},{b2}");
    }

    /// Float continuation (8g): a block DEEPER in the DOM — not a direct
    /// sibling of the float — still shortens where it intersects the
    /// float's band, and returns to full width below it. The Wikipedia
    /// shape: floated infobox beside an article whose sections are nested
    /// divs. Sections use AUTO width (the common case) so they fill
    /// whatever their containing band allows.
    #[test]
    fn float_continuation_narrows_nested_blocks() {
        let html = r#"<body>
            <div id="box"></div>
            <div id="section1"><p>第一段落文本</p></div>
            <div id="section2"><p>第二段落</p></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #box { float: left; width: 200px; height: 100px; }
            #section1 { height: 50px; }
            #section2 { height: 30px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let s1 = r("#section1");
        let s2 = r("#section2");

        // Both sections are zone content (direct siblings of the float):
        // the 8b flow column places them beside the float at reduced
        // width — the float's exclusion is by construction here.
        assert!((s1.x - 200.0).abs() < EPS as f32,
            "intersecting nested block starts at the float's right edge: x={}", s1.x);
        assert!((s1.width - (VW - 200.0)).abs() <= 2.0 * EPS as f32,
            "and takes the remaining width: {} vs {}", s1.width, VW - 200.0);
        assert!((s2.x - 200.0).abs() < EPS as f32,
            "second section also beside the float: x={}", s2.x);

        // A block fully BELOW the float keeps full width. The float's zone
        // ends at the spacer (a block sibling): everything after it is
        // zone-external normal flow.
        let html2 = r#"<body>
            <div id="box"></div>
            <div id="spacer"></div>
            <div id="deep"><p>深处的全宽内容</p></div>
        </body>"#;
        let sheet2 = r#"
            body { margin: 0; }
            #box { float: left; width: 200px; height: 100px; }
            #spacer { height: 120px; }
            #deep { height: 20px; }
        "#;
        let tree2 = crate::diting_dom::tree_sink::parse_html(html2);
        let rules2 = diting_css::parse_stylesheet(sheet2);
        let styles2 = our_styles(&tree2, &rules2);
        let rects2 = layout_dom(&tree2, &styles2, &fixture_fonts(), VW, VH);
        let deep = rects2[&tree2.query_selector("#deep").unwrap().unwrap()];
        assert!(deep.y >= 100.0 - EPS as f32,
            "deep block starts after the spacer (below the band): y={}", deep.y);
        assert!((deep.x - 0.0).abs() < EPS as f32 && (deep.width - VW).abs() <= 2.0 * EPS as f32,
            "content below the band runs full width: {deep:?}");
    }

    /// Real-site shape validation (8 series smoke): zh.wikipedia's CSS
    /// article infobox — `float:right; clear:right; width:22em`, an 8-row
    /// table with caption/th/td rows, followed by two lead paragraphs and a
    /// later section. Hand contract at VW=800: the infobox hugs the right
    /// edge (22em = 352px → x=448), lead text fills the flow column beside
    /// it; nothing panics and every box stays inside the viewport.
    #[test]
    fn wikipedia_infobox_shape_lays_out_without_panicking() {
        // Structure-faithful excerpt of
        // https://zh.wikipedia.org/wiki/CSS (2026-08-24): infobox table +
        // lead paragraphs + one later section.
        let html = r#"<body>
            <table class="infobox"><caption>CSS</caption>
                <tr><th colspan="2">层叠样式表</th></tr>
                <tr><td colspan="2">2024年全新启用之官方标识</td></tr>
                <tr><th>扩展名</th><td>.css</td></tr>
                <tr><th>互联网媒体类型</th><td>text/css</td></tr>
                <tr><th>开发者</th><td>哈肯·維姆·萊、伯特·波斯、全球資訊網協會</td></tr>
                <tr><th>首次发布</th><td>1996年12月17日，29年前</td></tr>
                <tr><th>格式类型</th><td>样式表语言</td></tr>
                <tr><th>标准</th><td>第一版、第二版、第三版各模組規格</td></tr>
            </table>
            <p id="lead1">階層式樣式表（英語：Cascading Style Sheets，缩写CSS）是一种用来为结构化文档添加样式的计算机语言，由W3C定义和维护。CSS3現在已被大部分現代瀏覽器支援。</p>
            <p id="lead2">CSS不仅可以静态地修饰网页，还可以配合各种脚本语言动态地对网页各元素进行格式化。CSS 能够对网页中元素位置的排版进行像素级精确控制。</p>
            <h2 id="hist">歷史</h2>
            <p id="later">CSS最早的提案是在1994年提出的。</p>
        </body>"#;
        let sheet = r#"
            body { margin: 0; font-size: 16px; }
            .infobox { float: right; clear: right; width: 352px; border: 1px solid rgb(200,200,200); background: rgb(248,249,250); margin: 0 0 16px 16px; }
            p { margin: 0 0 12px 0; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        // The real product entry: paint items + rects must come out without
        // panicking on this shape.
        let (rects, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW, VH);

        assert!(!rects.is_empty(), "rects produced");
        assert!(items.iter().any(|i| matches!(i, PaintItem::Bg { .. })), "infobox bg emitted");

        let ib = rects[&tree.query_selector(".infobox").unwrap().unwrap()];
        // CSS float semantics: the float's MARGIN box hugs the container's
        // right edge. Margin box = left-margin(16) + border-box(354) = 370,
        // so the border box starts at 800 − 370 + 16 = 446.
        assert!((ib.x - 446.0).abs() <= 2.0 * EPS as f32,
            "infobox border box at the margin-box right-edge stop: x={} want 446", ib.x);
        assert!((ib.width - 354.0).abs() <= 2.0 * EPS as f32,
            "width:22em content + 1px border per side: {}", ib.width);

        let l1 = rects[&tree.query_selector("#lead1").unwrap().unwrap()];
        assert!((l1.x - 0.0).abs() < EPS as f32,
            "first paragraph starts at the container's left edge");
        assert!(l1.width < VW - 368.0 + 2.0,
            "paragraph wraps in the flow column beside the infobox: {}", l1.width);

        // Every reported rect stays inside the viewport bounds.
        for (dom_id, r) in rects.iter() {
            assert!(
                r.x >= -EPS as f32 && r.x + r.width <= VW as f32 + 2.0,
                "rect for {dom_id:?} escapes the viewport horizontally: {r:?}"
            );
        }
    }

    /// Real-site shape validation (8c with MARGINS): sfbay.craigslist.org's
    /// homepage directory grid — `.box { float:left; width:23%;
    /// margin-right:2% }` tiles four boxes per band exactly (each outer
    /// step is 25% of the band), and the fifth wraps. The live page blocks
    /// scripted fetches, so this is a structure-faithful excerpt of the
    /// documented grid shape; the hand contract at VW=800 exercises what
    /// the no-margin run test cannot: per-float margin participating in
    /// the wrap arithmetic (width 184 + gap 16 = 200 pitch).
    #[test]
    fn craigslist_float_grid_margins_tile_four_per_band() {
        let html = r#"<body>
            <div class="box" id="b1"></div><div class="box" id="b2"></div>
            <div class="box" id="b3"></div><div class="box" id="b4"></div>
            <div class="box" id="b5"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            .box { float: left; width: 23%; margin-right: 2%; height: 80px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (b1, b2, b3, b4, b5) = (r("#b1"), r("#b2"), r("#b3"), r("#b4"), r("#b5"));

        let cell_w = VW * 0.23;
        let gap = VW * 0.02;
        let pitch = cell_w + gap;
        for (name, cell, x) in
            [("b1", b1, 0.0), ("b2", b2, pitch), ("b3", b3, 2.0 * pitch), ("b4", b4, 3.0 * pitch)]
        {
            assert!((cell.x - x).abs() < EPS as f32,
                "{name} sits one 23%-plus-gap step over: x={} want {x}", cell.x);
            assert!((cell.y - 0.0).abs() < EPS as f32, "{name} shares band one");
            assert!((cell.width - cell_w).abs() < EPS as f32,
                "{name} is 23% of the band (the 2% is margin): {}", cell.width);
        }
        // Four outer steps of exactly 25% fill the band; the fifth wraps.
        assert!(pitch * 4.0 - VW <= EPS as f32 && pitch * 5.0 > VW as f32,
            "four boxes tile exactly, five overflow");
        assert!((b5.y - 80.0).abs() < EPS as f32,
            "fifth box wraps below band one: y={} want 80", b5.y);
        assert!((b5.x - 0.0).abs() < EPS as f32, "wrapped box restarts at the left edge");
    }

    /// Real-site shape validation (8e with MARGINS): a top navigation bar —
    /// inline brand text plus two right-floated link boxes, each carrying a
    /// left margin (the spacing idiom between floated links). Hand contract
    /// at VW=800: source-first link hugs the RIGHT edge, second stacks to
    /// its LEFT separated by its own 10px margin, and both margins push the
    /// pair's total footprint to 180 so nothing overlaps the brand.
    #[test]
    fn nav_bar_right_floated_links_with_margins_stack_inward() {
        let html = r#"<body>
            <span id="brand">站点名</span>
            <div class="lnk" id="l1"></div>
            <div class="lnk" id="l2"></div>
        </body>"#;
        let sheet = r#"
            body { margin: 0; }
            #brand { font-size: 16px; }
            .lnk { float: right; width: 80px; height: 30px; margin-left: 10px; }
        "#;

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        let r = |sel: &str| rects[&tree.query_selector(sel).unwrap().unwrap()];
        let (l1, l2) = (r("#l1"), r("#l2"));

        // Group footprint: two links × (80 wide + 10 left margin) = 180,
        // pushed flush right by the group's auto margin. Within it, source-
        // first l1 lands at the far right edge (620 + 10 + 80 + 10 = 720),
        // l2 one full link-plus-margin inboard (620 + 10 = 630).
        assert!((l1.x - (VW - 80.0)).abs() < EPS as f32,
            "first right float still hugs the right edge through its margin: x={} want {}",
            l1.x, VW - 80.0);
        assert!((l2.x - (VW - 170.0)).abs() < EPS as f32,
            "second link stacks leftward past l1's margin: x={} want {}",
            l2.x, VW - 170.0);
        assert!((l1.y - 0.0).abs() < EPS as f32 && (l2.y - 0.0).abs() < EPS as f32,
            "both share band one");
        // The inline brand is flattened into a text run (no own rect); the
        // link stack starting at 630 leaves the full left side to it.
    }

    /// 1:1 image paint (batch 5b): the img box equals the image size, so
    /// both engines blit the same texels at the same positions — every
    /// pixel over the box must match exactly.
    #[test]
    fn paint_img_data_url_1to1_matches_blitz() {
        let (url, img) = fixture_image_data_url(100, 50);
        let html = format!(r#"<body><img id="t" src="{url}" width="100" height="50"></body>"#);
        let sheet = "body { margin: 0; }";
        let (w, h) = (200u32, 200u32);

        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);

        // Structural: the Image item (not the placeholder) at the img box.
        let tree = crate::diting_dom::tree_sink::parse_html(&html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let (_r, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW, VH);
        let image_rect = items.iter().find_map(|i| match i {
            PaintItem::Image { paint_rect, .. } => Some(*paint_rect),
            _ => None,
        });
        assert!(
            image_rect.is_some_and(|r| (r.width, r.height) == (100.0, 50.0)),
            "Image item at the 100×50 box: {image_rect:?}"
        );

        for y in 0..50usize {
            for x in 0..100usize {
                let i = (y * w as usize + x) * 4;
                assert_eq!(
                    ours.data[i..i + 4],
                    blitz[i..i + 4],
                    "pixel ({x},{y}) differs"
                );
            }
        }
        // Outside the box: plain white on both.
        let i = (60 * w as usize + 150) * 4;
        assert_eq!(ours.data[i..i + 4], [255, 255, 255, 255]);
    }

    /// Scaled image (×2 stretch, object-fit: fill) and image-derived natural
    /// size (no attrs/CSS → the box IS the image). Nearest-neighbor vs
    /// vello's sampler diverge on edge rows, so the contract is bbox ±1
    /// plus exact interior quadrant samples.
    #[test]
    fn paint_img_scaled_and_natural_size_match_blitz() {
        let (url, img) = fixture_image_data_url(100, 50);

        // Case A: CSS box 200×100, image 100×50 → ×2 stretch.
        let html = format!(r#"<body><img id="t" src="{url}"></body>"#);
        let sheet = "body { margin: 0; } #t { width: 200px; height: 100px; }";
        let (w, h) = (300u32, 200u32);
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);

        let is_img_px = |p: &[u8]| {
            (p[0] > 130 && p[1] < 110 && p[2] < 110) || (p[0] < 110 && p[1] < 110 && p[2] > 130)
        };
        fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> (usize, usize, usize, usize) {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if hit(&buf[(y * w + x) * 4..]) {
                        b = Some(match b {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            b.expect("bbox: no hit pixels")
        }
        let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_img_px);
        let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_img_px);
        for (what, o, b) in [
            ("img left", ox0, bx0),
            ("img top", oy0, by0),
            ("img right", ox1, bx1),
            ("img bottom", oy1, by1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }
        // Interior quadrant centers: quadrant-exact colors on both engines.
        // Pattern is `(x < w/2) ^ (y < h/2)` = red, so the anti-diagonal
        // (right-top, left-bottom) quadrants are red.
        for (x, y, red) in [(50, 25, false), (150, 25, true), (50, 75, true), (150, 75, false)] {
            let i = (y * w as usize + x) * 4;
            for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
                let (r, g, b) = (buf[i], buf[i + 1], buf[i + 2]);
                assert_eq!(
                    (r > 130, b > 130),
                    (red, !red),
                    "{name} quadrant at ({x},{y}): rgb({r},{g},{b})"
                );
            }
        }

        // Case B: no attrs, no CSS → the box is the image's natural size.
        let sheet = "body { margin: 0; }";
        let (w, h) = (200u32, 200u32);
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);
        let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_img_px);
        let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_img_px);
        for (what, o, b) in [
            ("natural left", ox0, bx0),
            ("natural top", oy0, by0),
            ("natural right", ox1, bx1),
            ("natural bottom", oy1, by1),
        ] {
            assert!((o as i64 - b as i64).abs() <= 1, "{what}: ours={o} blitz={b}");
        }
        assert_eq!((ox1 - ox0 + 1, oy1 - oy0 + 1), (100, 50), "natural box 100×50");
    }

    /// object-fit: none + scale-down (batch 5c): both paint the image at
    /// NATURAL size (no resampling), so the only new variable vs the 5b 1:1
    /// test is the object-position offset — every pixel must match exactly.
    #[test]
    fn paint_object_fit_none_and_scale_down_pixel_exact() {
        let (url, img) = fixture_image_data_url(100, 50);
        let (w, h) = (300u32, 200u32);

        // none, default position (50% 50%): paints at ((300-100)/2,
        // (200-50)/2) = (100, 75), natural 100×50.
        let html = format!(r#"<body><img id="t" src="{url}"></body>"#);
        let sheet = "body { margin: 0; } #t { width: 300px; height: 200px; object-fit: none; }";
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);
        for i in 0..w as usize * h as usize {
            assert_eq!(
                ours.data[i * 4..i * 4 + 4],
                blitz[i * 4..i * 4 + 4],
                "none@center pixel ({},{})",
                i % w as usize,
                i / w as usize
            );
        }

        // scale-down on a box BIGGER than the image: contain would be
        // 300×150 but natural 100×50 is smaller → natural size, centered.
        let sheet = "body { margin: 0; } #t { width: 300px; height: 200px; object-fit: scale-down; }";
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);
        for i in 0..w as usize * h as usize {
            assert_eq!(
                ours.data[i * 4..i * 4 + 4],
                blitz[i * 4..i * 4 + 4],
                "scale-down@center pixel ({},{})",
                i % w as usize,
                i / w as usize
            );
        }

        // px offsets: object-position 10px 20px pins the top-left corner —
        // offset math independent of centering.
        let sheet =
            "body { margin: 0; } #t { width: 300px; height: 200px; object-fit: none; object-position: 10px 20px; }";
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);
        for i in 0..w as usize * h as usize {
            assert_eq!(
                ours.data[i * 4..i * 4 + 4],
                blitz[i * 4..i * 4 + 4],
                "none@10,20 pixel ({},{})",
                i % w as usize,
                i / w as usize
            );
        }
    }

    /// contain/cover sizing (batch 5c). Both resample (scale ≠ 1) so the
    /// contract is bbox ±1 against blitz plus quadrant-center samples:
    ///
    /// - contain in a square box letterboxes vertically (min ratio),
    /// - cover in a square box OVERFLOWS horizontally (max ratio) and —
    ///   verified here against upstream itself — is NOT clipped under the
    ///   default overflow: visible; the painted ink extends past the img
    ///   box symmetrically (offset goes negative).
    #[test]
    fn paint_object_fit_contain_cover_match_blitz() {
        let (url, img) = fixture_image_data_url(100, 50);

        // contain: box 200×200, image 100×50 → min(2, 4) = 2 → 200×100,
        // centered vertically at y=(200-100)/2=50.
        let html = format!(r#"<body><img id="t" src="{url}"></body>"#);
        let sheet = "body { margin: 0; } #t { width: 200px; height: 200px; object-fit: contain; }";
        let (w, h) = (300u32, 300u32);
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);

        let is_img_px = |p: &[u8]| {
            (p[0] > 130 && p[1] < 110 && p[2] < 110) || (p[0] < 110 && p[1] < 110 && p[2] > 130)
        };
        fn bbox(buf: &[u8], w: usize, h: usize, hit: impl Fn(&[u8]) -> bool) -> (usize, usize, usize, usize) {
            let mut b: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if hit(&buf[(y * w + x) * 4..]) {
                        b = Some(match b {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            b.expect("bbox: no hit pixels")
        }
        let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_img_px);
        let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_img_px);
        for (what, o, b, expect) in [
            ("contain left", ox0, bx0, 0.0),
            ("contain top", oy0, by0, 50.0),
            ("contain right", ox1, bx1, 199.0),
            ("contain bottom", oy1, by1, 149.0),
        ] {
            assert!(
                (o as f64 - expect).abs() <= 1.0 && (b as f64 - expect).abs() <= 1.0,
                "{what}: ours={o} blitz={b} expected≈{expect}"
            );
        }

        // cover: square box 100×100, wide image 100×50 → max(1, 2) = 2 →
        // 200×100 centered at x=-50 relative to the box. Upstream ALWAYS
        // clips image elements to the padding box (`is_image ||` in
        // should_clip — independent of overflow), so the ink is cut at the
        // box edges: visible spans x [0, 100], y [0, 100] on BOTH engines.
        let sheet = "body { margin: 0; } #t { width: 100px; height: 100px; object-fit: cover; }";
        let (w, h) = (300u32, 300u32);
        let blitz = blitz_render_with_image(&html, sheet, "#t", &img, w, h);
        let ours = our_render(&html, sheet, w as usize, h as usize);
        let (ox0, oy0, ox1, oy1) = bbox(&ours.data, w as usize, h as usize, is_img_px);
        let (bx0, by0, bx1, by1) = bbox(&blitz, w as usize, h as usize, is_img_px);
        for (what, o, b, expect) in [
            ("cover left", ox0, bx0, 0.0),
            ("cover top", oy0, by0, 0.0),
            ("cover right", ox1, bx1, 99.0),
            ("cover bottom", oy1, by1, 99.0),
        ] {
            assert!(
                (o as f64 - expect).abs() <= 1.0 && (b as f64 - expect).abs() <= 1.0,
                "{what}: ours={o} blitz={b} expected≈{expect}"
            );
        }
        // Interior solid samples inside the box: left half of the VISIBLE
        // region maps to image-space x∈[25,75) (paint starts at image x=25
        // after the -50px offset + ×2 scale)... concretely the pattern's
        // red/blue class must AGREE across engines at two off-boundary
        // points (one per half).
        for (x, y) in [(20usize, 25usize), (80usize, 75usize)] {
            let i = (y * w as usize + x) * 4;
            let o = &ours.data[i..i + 4];
            let b = &blitz[i..i + 4];
            assert_eq!(o[0] > 130, b[0] > 130, "cover class at ({x},{y}): ours={o:?} blitz={b:?}");
            assert_eq!(o[2] > 130, b[2] > 130, "cover class at ({x},{y}): ours={o:?} blitz={b:?}");
        }
    }

    /// Stacking order (batch 6a): overlapping solid blocks, the top color
    /// at each overlap read off both engines. Four scenarios:
    /// positive z-index hoists above later siblings; negative sinks below
    /// earlier ones; positioned z-auto paints above in-flow content even
    /// when it comes first in the document; z-index on a STATIC element is
    /// ignored (document order wins).
    #[test]
    fn paint_stacking_order_matches_blitz() {
        let px = |buf: &[u8], w: u32, x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        let is_red = |c: (u8, u8, u8)| c.0 > 150 && c.1 < 100 && c.2 < 100;
        let is_green = |c: (u8, u8, u8)| c.1 > 150 && c.0 < 100 && c.2 < 100;
        let is_blue = |c: (u8, u8, u8)| c.2 > 150 && c.0 < 100 && c.1 < 100;

        // Case A: #r (red) → #b blue z=2 relative → #g green. Document order
        // would put green last; z=2 hoists BLUE above both.
        let html = r#"<body>
            <div id="r"></div><div id="b"></div><div id="g"></div>
        </body>"#;
        let sheet = "body { margin: 0; }
            div { position: absolute; width: 100px; height: 100px; }
            #r { left: 0px; top: 0px; background: rgb(200,40,40); }
            #b { left: 50px; top: 0px; background: rgb(40,40,200); z-index: 2; }
            #g { left: 0px; top: 0px; background: rgb(40,180,40); }";
        let (w, h) = (200u32, 120u32);
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);
        // (25,25): only red+green stack (green later in doc order → on top).
        // (75,25): red/blue/green all overlap; blue's z=2 wins on BOTH.
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            let c1 = px(buf, w, 25, 25);
            let c2 = px(buf, w, 75, 25);
            assert!(is_green(c1), "{name} green-on-top at (25,25): {c1:?}");
            assert!(is_blue(c2), "{name} z=2 blue on top at (75,25): {c2:?}");
            let _ = is_red;
        }

        // Case B: negative z-index sinks below an EARLIER sibling; and a
        // positioned z-AUTO element still paints above in-flow content.
        let html_b = r#"<body>
            <div id="top"></div><div id="neg"></div><div id="auto"></div>
        </body>"#;
        let sheet_b = "body { margin: 0; }
            div { position: absolute; width: 100px; height: 100px; }
            #top { left: 0px; top: 0px; background: rgb(40,180,40); }
            #neg { left: 0px; top: 0px; background: rgb(200,40,40); z-index: -1; }
            #auto { left: 0px; top: 0px; background: rgb(40,40,200); }";
        let mut doc = blitz_doc_unresolved(html_b, sheet_b);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html_b, sheet_b, w as usize, h as usize);
        // All three fully overlap at (25,25): neg(red) sinks below its
        // earlier sibling green; auto(blue, LAST in document) covers green.
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            let c = px(buf, w, 25, 25);
            assert!(
                is_blue(c),
                "{name} doc-last z-auto blue on top at (25,25): {c:?}"
            );
        }

        // Case C: z-index on a STATIC element is ignored — document order
        // decides (later green over "z=9" red).
        let html_c = r#"<body><div id="s"></div><div id="t"></div></body>"#;
        let sheet_c = "body { margin: 0; }
            div { width: 100px; height: 100px; margin-bottom: -50px; }
            #s { background: rgb(200,40,40); z-index: 9; }
            #t { background: rgb(40,180,40); }";
        let (w2, h2) = (200u32, 160u32);
        let mut doc = blitz_doc_unresolved(html_c, sheet_c);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
            |scene| {
                use anyrender::PaintScene as _;
                use peniko::kurbo::Rect;
                scene.fill(
                    peniko::Fill::NonZero,
                    Default::default(),
                    peniko::Color::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, w2 as f64, h2 as f64),
                );
                blitz_paint::paint_scene(scene, &mut doc, 1.0, w2, h2, 0, 0);
            },
            w2,
            h2,
        );
        let ours = our_render(html_c, sheet_c, w2 as usize, h2 as usize);
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            // Overlap zone is y∈[50,100) (the -50px pull-up); there the
            // later green must cover the "z=9" static red.
            let c = px(buf, w2, 25, 75);
            assert!(
                is_green(c),
                "{name} static z-index ignored, later green on top: {c:?}"
            );
        }
    }

    /// border-radius on a solid background (batch 6b): the bg clips to the
    /// rounded border box. Rasterizers differ at the arc itself (vello AA
    /// vs our hard edge), so the contract is: bbox == the full box (the
    /// straight edges survive the corners), corner zones are background,
    /// center and edge midpoints are fill — sampled well away from the arc.
    #[test]
    fn paint_border_radius_bg_matches_blitz() {
        let html = r#"<body><div id="t"></div></body>"#;
        let sheet = "body { margin: 0; } #t { width: 100px; height: 100px; background: rgb(200,40,40); border-radius: 20px; }";
        let (w, h) = (200u32, 200u32);
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);

        let px = |buf: &[u8], w: u32, x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        let is_red = |c: (u8, u8, u8)| c.0 > 150 && c.1 < 110 && c.2 < 110;
        let is_white = |c: (u8, u8, u8)| c.0 > 240 && c.1 > 240 && c.2 > 240;

        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            // Center and edge midpoints: well inside the shape.
            for (x, y) in [(50usize, 50usize), (50usize, 2usize), (2usize, 50usize)] {
                assert!(
                    is_red(px(buf, w, x, y)),
                    "{name} rounded-bg fills ({x},{y}): {:?}",
                    px(buf, w, x, y)
                );
            }
            // Corner zone (deep inside the cut): background shows through.
            assert!(
                is_white(px(buf, w, 3, 3)),
                "{name} corner (3,3) clipped to white: {:?}",
                px(buf, w, 3, 3)
            );            // Just outside the arc along the diagonal: for r=20 the arc
            // crosses the diagonal at 20−20/√2 ≈ 5.86px from the corner,
            // so pixel (4,4) (center 4.5) is outside and (7,7) inside.
            let d = px(buf, w, 4, 4);
            assert!(
                !is_red(d),
                "{name} just outside arc at (4,4) not filled: {d:?}"
            );

            // Structural: the Bg item carries the parsed radius.
            let tree = crate::diting_dom::tree_sink::parse_html(html);
            let rules = diting_css::parse_stylesheet(sheet);
            let styles = our_styles(&tree, &rules);
            let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW, VH);
            let radius = items.iter().find_map(|i| match i {
                PaintItem::Bg { radius, .. } => Some(*radius),
                _ => None,
            });
            assert_eq!(radius, Some(20.0), "{name}: radius flows into the Bg item");
        }

        // Percentage radius: `border-radius: 50%` on a square box = circle
        // (r=50). Sampled points must classify identically across engines.
        let sheet = "body { margin: 0; } #t { width: 100px; height: 100px; background: rgb(200,40,40); border-radius: 50%; }";
        let mut doc = blitz_doc_unresolved(html, sheet);
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let blitz = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
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
        let ours = our_render(html, sheet, w as usize, h as usize);
        for (name, buf) in [("ours", &ours.data), ("blitz", &blitz)] {
            assert!(is_red(px(buf, w, 50, 50)), "{name} circle center filled");
            assert!(is_red(px(buf, w, 10, 50)), "{name} circle left of center filled");
            // Corner far outside the incircle.
            assert!(is_white(px(buf, w, 4, 4)), "{name} square corner clipped");
            // Diagonal point outside the incircle (distance from center
            // ≈65 > r=50).
            assert!(!is_red(px(buf, w, 4, 4)), "{name} corner not red");
            let diag = px(buf, w, 12, 12);
            assert!(
                !is_red(diag),
                "{name} diagonal outside incircle not filled: {diag:?}"
            );
        }
    }

    /// Network-image path (batch 6c): an `<img src="https://…">` whose body
    /// sits in the injected byte table paints exactly like the blitz side
    /// fed the same decoded bytes — pixel-exact at 1:1. The table is keyed
    /// by absolute URL (the caller resolves relative srcs before fetching),
    /// so this exercises the exact lookup the product prefetch would use.
    #[test]
    fn paint_img_from_network_bytes_matches_blitz() {
        use base64::Engine as _;
        let (w, h) = (200u32, 200u32);
        let mut rgba = Vec::new();
        for y in 0..50u32 {
            for x in 0..100u32 {
                let red = (x < 50) ^ (y < 25);
                rgba.extend_from_slice(if red { &[200, 40, 40, 255] } else { &[40, 40, 200, 255] });
            }
        }
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 100, 50);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }

        // Ours: http(s) src resolved through the injected byte table.
        let html = r#"<body><img id="t" src="https://cdn.example.com/hero.png" width="100" height="50"></body>"#;
        let sheet = "body { margin: 0; }";
        let mut net: HashMap<String, Vec<u8>> = HashMap::new();
        net.insert("https://cdn.example.com/hero.png".to_string(), png_bytes);

        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let (_rects, items) =
            layout_dom_with_paint_and_images(&tree, &styles, &fixture_fonts(), VW, VH, Some(&net));
        assert!(
            items.iter().any(|i| matches!(i, PaintItem::Image { .. })),
            "network img resolves to an Image item"
        );
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fixture_fonts(), &mut ours);

        // Blitz side: same RGBA injected directly (its harness has no net
        // layer) via the data: URL path — identical bytes, pixel-exact.
        let mut png2 = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png2, 100, 50);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png2)
        );
        let html_blitz =
            format!(r#"<body><img id="t" src="{data_url}" width="100" height="50"></body>"#);
        let img = image::DecodedImage::new(100, 50, rgba.clone());
        let blitz = blitz_render_with_image(&html_blitz, sheet, "#t", &img, w, h);
        for y in 0..50usize {
            for x in 0..100usize {
                let i = (y * w as usize + x) * 4;
                assert_eq!(
                    ours.data[i..i + 4],
                    blitz[i..i + 4],
                    "network-img pixel ({x},{y}) differs"
                );
            }
        }
    }

    /// JPEG network image (batch 6d): the byte table carries a real JPEG
    /// body; our ImageCache sniffs the magic and decodes via the same
    /// `image` crate blitz uses, so the painted pixels match blitz's
    /// injected decode exactly. Solid color keeps the comparison exact
    /// (no resampling anywhere at 1:1).
    #[test]
    fn paint_img_from_network_jpeg_matches_blitz() {
        use base64::Engine as _;
        let (w, h) = (200u32, 200u32);

        // Encode a 100×50 solid-red JPEG.
        let mut jpeg_bytes = Vec::new();
        let px_count = 100usize * 50usize * 3usize;
        ::image::DynamicImage::from(
            ::image::RgbImage::from_raw(100, 50, vec![200u8, 40, 40].repeat(px_count / 3))
                .unwrap(),
        )
        .write_to(&mut std::io::Cursor::new(&mut jpeg_bytes), ::image::ImageFormat::Jpeg)
        .expect("encodes");

        // Ours: https src + JPEG body in the table.
        let html = r#"<body><img id="t" src="https://cdn.example.com/photo.jpg" width="100" height="50"></body>"#;
        let sheet = "body { margin: 0; }";
        let mut net: HashMap<String, Vec<u8>> = HashMap::new();
        net.insert("https://cdn.example.com/photo.jpg".to_string(), jpeg_bytes);
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let (_rects, items) =
            layout_dom_with_paint_and_images(&tree, &styles, &fixture_fonts(), VW, VH, Some(&net));
        let mut ours = paint::Canvas::new_filled(w as usize, h as usize, [255, 255, 255, 255]);
        paint::execute(&items, &fixture_fonts(), &mut ours);

        // Blitz side: decode the SAME bytes with its own decoder path
        // semantics — inject the RGBA our decode produced is circular, so
        // instead re-decode here through `image` exactly like blitz-dom's
        // ImageHandler does and assert our decode matches it bit-for-bit,
        // then paint-blitz from that.
        let decoded_again = ::image::ImageReader::new(std::io::Cursor::new(
            net["https://cdn.example.com/photo.jpg"].as_slice(),
        ))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap()
        .to_rgba8();
        let img = DecodedImage::new(100, 50, decoded_again.to_vec());
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode({
                let mut png2 = Vec::new();
                {
                    let mut enc = png::Encoder::new(&mut png2, 100, 50);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    let mut writer = enc.write_header().unwrap();
                    writer.write_image_data(decoded_again.as_ref()).unwrap();
                }
                png2
            })
        );
        let html_blitz =
            format!(r#"<body><img id="t" src="{data_url}" width="100" height="50"></body>"#);
        let blitz = blitz_render_with_image(&html_blitz, sheet, "#t", &img, w, h);
        for y in 0..50usize {
            for x in 0..100usize {
                let i = (y * w as usize + x) * 4;
                assert_eq!(
                    ours.data[i..i + 4],
                    blitz[i..i + 4],
                    "network-jpeg pixel ({x},{y}) differs"
                );
            }
        }
    }

    // --- srcset/picture source selection (batch: image pipeline) ---

    fn img_source(html: &str) -> Option<String> {
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let img = tree.query_selector("img").unwrap().unwrap();
        resolve_img_source(&tree, img, 1280.0)
    }

    #[test]
    fn srcset_density_picks_nearest_1x() {
        let src = img_source(
            r#"<img src="fallback.png" srcset="a.png 1x, b.png 2x, c.png 3x">"#,
        );
        assert_eq!(src.as_deref(), Some("a.png"), "smallest density >= 1x wins at dpr 1");

        // Only hi-dpi candidates: the smallest of them (nothing fits 1x).
        let hi = img_source(r#"<img src="fallback.png" srcset="b.png 2x, c.png 3x">"#);
        assert_eq!(hi.as_deref(), Some("b.png"));
    }

    #[test]
    fn srcset_width_picks_largest_fitting_viewport() {
        let src = img_source(
            r#"<img src="fb.png" srcset="s.png 320w, m.png 640w, l.png 2000w">"#,
        );
        assert_eq!(src.as_deref(), Some("m.png"), "640w is the largest <= 1280 viewport");

        // Nothing fits → smallest keeps the box bounded.
        let tiny = img_source(r#"<img src="fb.png" srcset="xl.png 4000w, xxl.png 8000w">"#);
        assert_eq!(tiny.as_deref(), Some("xl.png"));
    }

    #[test]
    fn picture_media_selects_first_matching_source() {
        let html = r#"<picture>
            <source media="(min-width: 900px)" srcset="desktop.png">
            <source media="(min-width: 400px)" srcset="tablet.png">
            <img src="phone.png">
        </picture>"#;
        assert_eq!(img_source(html).as_deref(), Some("desktop.png"));

        // Narrow viewport skips the desktop source.
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let img = tree.query_selector("img").unwrap().unwrap();
        let narrow = resolve_img_source(&tree, img, 600.0);
        assert_eq!(narrow.as_deref(), Some("tablet.png"));

        // No media attr → matches everything; first source still wins.
        let nomedia = img_source(
            r#"<picture><source srcset="any.png"><img src="fb.png"></picture>"#,
        );
        assert_eq!(nomedia.as_deref(), Some("any.png"));

        // Non-matching sources fall through to the plain img fallback.
        let none_match = img_source(
            r#"<picture><source media="(min-width: 99999px)" srcset="big.png"><img src="fb.png"></picture>"#,
        );
        assert_eq!(none_match.as_deref(), Some("fb.png"));
    }

    #[test]
    fn srcset_parse_rejects_bad_entries_and_keeps_good_neighbors() {
        use crate::diting_layout::image::{parse_srcset, select_srcset_candidate};

        let cands = parse_srcset("good.png 2x, bad.png 5q, , tail.png");
        assert_eq!(cands.len(), 2, "unknown descriptor and empty entry skipped");
        assert_eq!(cands[0].url, "good.png");
        assert_eq!(cands[1].url, "tail.png");
        assert_eq!(cands[1].density, None, "descriptor-less entry has no explicit density");

        // Descriptor-less entries participate as 1x in selection.
        let pick = select_srcset_candidate(&cands, 1280.0).map(|c| c.url.clone());
        assert_eq!(pick.as_deref(), Some("tail.png"), "1x < 2x at dpr 1");

        assert!(parse_srcset("").is_empty());
    }

    /// Flattened inline wrappers (span/label/a — obscura#722 lineage) must
    /// still report a rect: the union pass rebuilds it from the hoisted
    /// run children. Before the fix these elements owned no taffy box at
    /// all, so getBoundingClientRect fell back to a synthetic grid cell —
    /// breaking coordinate-based clicking on inline content.
    #[test]
    fn inline_elements_keep_real_rects_after_flatten() {
        let html = r##"<body>
            <p id="p">before <span id="s">spanned</span> after</p>
            <label id="l">toggle</label>
            <a id="a" href="#">link</a>
        </body>"##;
        let sheet = "body { margin: 0; } #p { margin: 0; font-size: 16px; }";
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);

        for sel in ["#p", "#s", "#l", "#a"] {
            let id = tree.query_selector(sel).unwrap().unwrap();
            eprintln!("{} -> {:?}", sel, rects.get(&id));
        }

        let s = tree.query_selector("#s").unwrap().unwrap();
        let l = tree.query_selector("#l").unwrap().unwrap();
        let a = tree.query_selector("#a").unwrap().unwrap();
        let s_rect = rects.get(&s).expect("span keeps a rect after flatten");
        let l_rect = rects.get(&l).expect("label keeps a rect after flatten");
        let a_rect = rects.get(&a).expect("anchor keeps a rect after flatten");
        // Real geometry, not a synthetic grid cell: nonzero size, inside the
        // viewport, on the first text line (~16px font, ~19px line box).
        for r in [s_rect, l_rect, a_rect] {
            assert!(r.width > 0.0 && r.height > 0.0, "inline rect {r:?} has size");
            assert!(r.x >= 0.0 && r.y >= 0.0 && r.y < VH, "inline rect {r:?} is in-flow");
            assert!(r.height < 60.0, "inline rect {r:?} is a line box, not a grid cell");
        }
        // The span sits after the "before " text: not at the line's start.
        assert!(s_rect.x > 0.0, "span starts after leading text, got {:?}", s_rect);
    }


    // ── calc() width resolution (obscura#767) ──────────────────────────────
    //
    // A mixed percent+px calc parses to Length::Calc and rides the first
    // layout as a percent-only placeholder; the post-layout repair pass
    // resolves it ONCE against the settled containing block content box.

    #[test]
    fn calc_width_resolves_against_real_containing_block() {
        let html = r#"<body><div id="wrap"><div id="box"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #wrap { width: 600px; }
            #box { width: calc(50% + 10px); height: 20px; }
        "#;
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        let box_id = tree.query_selector("#box").unwrap().unwrap();
        let r = rects[&box_id];
        assert!(
            (r.width - 310.0).abs() < EPS as f32,
            "calc(50% + 10px) of a 600px CB = 310, got {r:?}"
        );
    }

    #[test]
    fn calc_min_max_width_clamp_the_box() {
        let html = r#"<body><div id="wide"></div><div id="small"></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #wide { width: calc(100% - 100px); max-width: calc(50% + 20px); height: 10px; }
            #small { width: 10px; min-width: calc(50% - 380px); height: 10px; }
        "#;
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        // max-width wins over the authored calc width: 700 clamps to 420.
        let wide = rects[&tree.query_selector("#wide").unwrap().unwrap()];
        assert!((wide.width - 420.0).abs() < EPS as f32, "max-width calc clamps: {wide:?}");
        // min-width lifts the 10px box to the calc result (20px).
        let small = rects[&tree.query_selector("#small").unwrap().unwrap()];
        assert!((small.width - 20.0).abs() < EPS as f32, "min-width calc lifts: {small:?}");
    }

    #[test]
    fn calc_width_inside_calc_width_parent_cascades() {
        let html = r#"<body><div id="outer"><div id="inner"></div></div></body>"#;
        let sheet = r#"
            body { margin: 0; }
            #outer { width: calc(50% + 100px); height: 20px; }
            #inner { width: calc(50% + 10px); height: 10px; }
        "#;
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        let outer = rects[&tree.query_selector("#outer").unwrap().unwrap()];
        let inner = rects[&tree.query_selector("#inner").unwrap().unwrap()];
        assert!((outer.width - 500.0).abs() < EPS as f32, "outer in 800px body: {outer:?}");
        // 50% of the PARENT'S REPAIRED width (500), not of the body (800):
        // shallow-first repair + arithmetic cascade, no second percent pass.
        assert!((inner.width - 260.0).abs() < EPS as f32, "inner = 50%·500 + 10: {inner:?}");
    }

    // ── run-edge whitespace trim (obscura#764) ─────────────────────────────

    #[test]
    fn float_shrink_to_fit_ignores_run_edge_whitespace() {
        // obscura#764's shape: a floated (shrink-to-fit) box whose mixed run
        // carries formatting whitespace at its edges. Each tokenized space
        // used to keep its full advance as a word leaf, inflating the float.
        let html = r#"<body>
            <div id="ws" style="float:left"><span>x</span>
            </div>
            <div id="compact" style="float:left"><span>x</span></div>
        </body>"#;
        let sheet = "body { margin: 0; font-size: 16px; } span { font-size: 16px; }";
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        let a = rects[&tree.query_selector("#ws").unwrap().unwrap()];
        let b = rects[&tree.query_selector("#compact").unwrap().unwrap()];
        assert!(
            (a.width - b.width).abs() < EPS as f32,
            "edge whitespace adds nothing to shrink-to-fit: ws={} compact={}",
            a.width, b.width
        );
        assert!(a.width > 0.0, "both floats still size to content: {a:?}");
        assert!(
            (b.x - a.x - a.width).abs() < 1.5,
            "the two floats sit side by side in the float row: {a:?} {b:?}"
        );
    }

    #[test]
    fn interior_whitespace_between_inline_siblings_still_counts() {
        // Over-trim guard: only run EDGES collapse; a space BETWEEN two
        // inline elements separates the words (CSS §16.6.1 interior rule).
        let html = r#"<body>
            <div id="spaced" style="float:left"><span>x</span> <span>y</span></div>
            <div id="tight" style="float:left"><span>x</span><span>y</span></div>
        </body>"#;
        let sheet = "body { margin: 0; font-size: 16px; } span { font-size: 16px; }";
        let tree = crate::diting_dom::tree_sink::parse_html(html);
        let rules = diting_css::parse_stylesheet(sheet);
        let styles = our_styles(&tree, &rules);
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW, VH);
        let a = rects[&tree.query_selector("#spaced").unwrap().unwrap()];
        let b = rects[&tree.query_selector("#tight").unwrap().unwrap()];
        assert!(
            a.width > b.width + 1.0,
            "the interior space keeps its advance: spaced={} tight={}",
            a.width, b.width
        );
    }
