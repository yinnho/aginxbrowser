// Colocated contract suite — split out of the god file (ratchet).
#[cfg(test)]
mod q_quote_tests {
    use crate::diting_layout::q_quote_pair;

    #[test]
    fn q_quotes_alternate_by_nesting_depth() {
        let outer = q_quote_pair(0);
        let inner = q_quote_pair(1);
        let deep = q_quote_pair(2);
        assert_eq!(outer, ("\u{201C}", "\u{201D}"));
        assert_eq!(inner, ("\u{2018}", "\u{2019}"));
        assert_eq!(deep, outer);
    }
}

#[cfg(test)]
mod incremental_match_tests {
    // #111 wiring: repeated css-keyed compute_styles_timed runs sync the
    // rule match sets incrementally against the tree's dirty registry. The
    // selector-layer differential test pins the hit sets; this pins the
    // observable cascade — a class flip and a structural insert must both
    // land in the returned ComputedStyles when the key is unchanged.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::compute_styles_timed;
    use html5ever::{namespace_url, ns};

    #[test]
    fn css_keyed_rerun_reflects_mutations() {
        let sheet = ".hot { color: rgb(1, 2, 3) } .warm { color: rgb(4, 5, 6) }";
        let tree = parse_html(
            r#"<html><body><div id="wrap"><p class="hot" id="t">a</p></div></body></html>"#,
        );
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let keyframes = Default::default();
        let key = 11u64;

        let run = || {
            compute_styles_timed(&tree, &rules, &keyframes, None, &[], (800.0, 600.0), Some(key))
        };
        let t = tree.get_element_by_id("t").unwrap();
        assert_eq!(
            run().get(&t).unwrap().color,
            Some(crate::diting_css::Color(1, 2, 3, 255))
        );

        // Class flip through the ops-path shape (attr write + stamp).
        tree.with_node_mut(t, |n| n.set_attribute("class", "warm".into()));
        tree.note_restyle(t);
        assert_eq!(
            run().get(&t).unwrap().color,
            Some(crate::diting_css::Color(4, 5, 6, 255))
        );

        // Structural insert: a fresh element picks up a rule it was not
        // present for.
        let p = tree.new_node(crate::diting_dom::tree::NodeData::Text { contents: "b".into() });
        let hot2 = tree.new_node(parse_element("p", "hot"));
        tree.append_child(hot2, p);
        let wrap = tree.get_element_by_id("wrap").unwrap();
        tree.append_child(wrap, hot2);
        let styles = run();
        assert_eq!(
            styles.get(&t).unwrap().color,
            Some(crate::diting_css::Color(4, 5, 6, 255))
        );
        assert_eq!(
            styles.get(&hot2).unwrap().color,
            Some(crate::diting_css::Color(1, 2, 3, 255))
        );
    }

    fn parse_element(tag: &str, class: &str) -> crate::diting_dom::tree::NodeData {
        use crate::diting_dom::tree::{Attribute, NodeData};
        let name = html5ever::QualName {
            prefix: None,
            ns: ns!(html),
            local: html5ever::LocalName::from(tag),
        };
        NodeData::Element {
            name,
            attrs: vec![Attribute {
                name: html5ever::QualName {
                    prefix: None,
                    ns: ns!(),
                    local: html5ever::LocalName::from("class"),
                },
                value: class.into(),
            }],
            live_value: None,
            live_checked: Some(false),
            mathml_annotation_xml_integration_point: false,
            template_contents: None,
        }
    }
}

#[cfg(test)]
mod grid_percent_track_tests {
    // `grid-template-columns: 25% 1fr` used to lose its % token at parse
    // time, which dropped the whole declaration and collapsed the grid to
    // one auto column. Pin the resolved geometry: a % track sizes against
    // the grid container's content box, and 1fr eats the remainder.
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn layout(sheet: &str, body: &str) -> HashMap<NodeId, Rect> {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        rects
    }

    #[test]
    fn percent_track_and_fr_split_the_container() {
        let sheet = "body { margin: 0 } #g { display: grid; width: 400px; grid-template-columns: 25% 1fr }";
        let body = r#"<div id="g"><div>aaa</div><div>bbbb</div></div>"#;
        let rects = layout(sheet, body);
        let x_at_width = |w: f32| -> f32 {
            let hits: Vec<f32> = rects.values().filter(|r| r.width == w).map(|r| r.x).collect();
            assert_eq!(hits.len(), 1, "width {w} matched {hits:?}");
            hits[0]
        };
        // 25% of the 400px content box; the fr track takes the remainder.
        assert_eq!(x_at_width(100.0), 0.0);
        assert_eq!(x_at_width(300.0), 100.0);
    }

    #[test]
    fn repeat_percent_tracks() {
        let sheet = "body { margin: 0 } #g { display: grid; width: 400px; grid-template-columns: repeat(2, 50%) }";
        let body = r#"<div id="g"><div>aaa</div><div>bbbb</div></div>"#;
        let rects = layout(sheet, body);
        let mut xs: Vec<f32> = rects.values().filter(|r| r.width == 200.0).map(|r| r.x).collect();
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(xs, vec![0.0, 200.0]);
    }
}

#[cfg(test)]
mod reparent_anchoring_tests {
    // blitz#764's repro matrix, pinned against the reparent pass: fixed
    // anchors to the viewport (immune to UA body margin), absolute anchors
    // to the nearest positioned ancestor's padding box (not the DOM
    // parent's flow position), a collapsed top margin displaces the CB and
    // the abs box follows its final position, and an abs box with no
    // positioned ancestor falls back to the ICB.
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn layout(sheet: &str, body: &str) -> HashMap<NodeId, Rect> {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        rects
    }

    fn find(rects: &HashMap<NodeId, Rect>, w: f32, h: f32) -> (f32, f32) {
        let hits: Vec<(f32, f32)> = rects
            .values()
            .filter(|r| r.width == w && r.height == h)
            .map(|r| (r.x, r.y))
            .collect();
        assert_eq!(hits.len(), 1, "signature {w}x{h} matched {hits:?}");
        hits[0]
    }

    fn assert_at(rects: &HashMap<NodeId, Rect>, w: f32, h: f32, x: f32, y: f32) {
        assert_eq!(find(rects, w, h), (x, y), "{w}x{h} misplaced");
    }

    #[test]
    fn fixed_under_body_ignores_ua_margin() {
        let rects = layout("", r#"<p>hello</p><div style="position: fixed; top: 0; left: 0; width: 26px; height: 14px"></div>"#);
        assert_at(&rects, 26.0, 14.0, 0.0, 0.0);
    }

    #[test]
    fn abs_anchors_to_positioned_ancestor_padding_box() {
        let sheet = "#gp { position: relative; width: 300px; height: 200px; padding-top: 1px } #mid { width: 200px; height: 100px; margin-left: 30px } #abs { position: absolute; top: 5px; left: 15px; width: 40px; height: 20px }";
        let rects = layout(sheet, r#"<div id="gp"><div id="mid"><div id="abs"></div></div></div>"#);
        assert_at(&rects, 200.0, 100.0, 38.0, 9.0);
        // gp padding box (8,8) + insets, NOT #mid border box + insets (53,14).
        assert_at(&rects, 40.0, 20.0, 23.0, 13.0);
    }

    #[test]
    fn collapsed_top_margin_does_not_leak_into_abs_anchor() {
        let sheet = "#gp { position: relative; width: 300px; height: 200px; padding-top: 1px } #mid { width: 200px; height: 100px; margin-left: 30px; margin-top: 40px } #abs { position: absolute; top: 5px; left: 15px; width: 40px; height: 20px }";
        let rects = layout(sheet, r#"<div id="gp"><div id="mid"><div id="abs"></div></div></div><div style="position: absolute; top: 5px; left: 10px; width: 42px; height: 22px"></div><div style="position: fixed; top: 8px; left: 12px; width: 44px; height: 24px"></div>"#);
        // #mid's 40px margin stays in flow (gp padding blocks the collapse).
        assert_at(&rects, 300.0, 201.0, 8.0, 8.0);
        assert_at(&rects, 200.0, 100.0, 38.0, 49.0);
        assert_at(&rects, 40.0, 20.0, 23.0, 13.0);
        // No positioned ancestor → ICB; fixed → viewport.
        assert_at(&rects, 42.0, 22.0, 10.0, 5.0);
        assert_at(&rects, 44.0, 24.0, 12.0, 8.0);
    }

    #[test]
    fn collapse_through_displaces_cb_and_abs_follows() {
        let sheet = "#gp { position: relative; width: 300px; height: 200px } #mid { width: 200px; height: 100px; margin-left: 30px; margin-top: 40px } #abs { position: absolute; top: 5px; left: 15px; width: 40px; height: 20px }";
        let rects = layout(sheet, r#"<div id="gp"><div id="mid"><div id="abs"></div></div></div>"#);
        // mid's margin collapses through gp and past the 8px body margin
        // (max(8,40)); gp and mid both start at y=40.
        assert_at(&rects, 300.0, 200.0, 8.0, 40.0);
        assert_at(&rects, 200.0, 100.0, 38.0, 40.0);
        assert_at(&rects, 40.0, 20.0, 23.0, 45.0);
    }
}

#[cfg(test)]
mod paint_token_memo_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    /// The collection walk hands each Run leaf's measure-time wrap tokens to
    /// its Text paint item (obscura#983's paint half): identity maps carry
    /// Some (the memo matches only unscaled font params), while a diagonal
    /// scale(2) folds d into font_size/word_spacing and must fall back to
    /// None so paint re-shapes with the scaled params.
    #[test]
    fn run_items_carry_wrap_tokens_unscaled_only() {
        let html = r#"<html><body><p>淘宝商品列表页的一段中文文本需要折行处理，再长一点保证折行。</p></body></html>"#;
        let collect_items = |sheet: &str| {
            let tree = parse_html(html);
            let rules = parse_stylesheet_for(sheet, (1280.0, 800.0), CssMediaType::Screen);
            let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
            let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
                &tree, &styles, &crate::diting_fonts::font_book(), 1280.0, 800.0, None, None,
            );
            items
        };

        let runs = |items: &[PaintItem]| -> Vec<bool> {
            items
                .iter()
                .filter_map(|it| match it {
                    PaintItem::Text { text, tokens, .. } if text.contains("淘宝") => Some(tokens.is_some()),
                    _ => None,
                })
                .collect()
        };
        let got = runs(&collect_items("p { font-size: 16px; text-decoration: underline; }"));
        assert!(!got.is_empty() && got.iter().all(|&t| t), "unscaled run carries tokens: {got:?}");
        let scaled = runs(&collect_items("p { font-size: 16px; text-decoration: underline; transform: scale(2); }"));
        assert!(!scaled.is_empty() && scaled.iter().all(|&t| !t), "scaled run drops tokens: {scaled:?}");
    }
}

/// obscura#983's measure half: taffy probes a run leaf repeatedly per solve
/// (min-content, max-content, definite widths, deferred/repair passes) and
/// every probe used to re-run full shaping of the same text. This module
/// pins the timing shape of a text-heavy solve and the memo mechanism.
#[cfg(test)]
mod run_token_memo_tests {
    use crate::diting_layout::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn paragraph(tree: &mut TaffyTree<TextLeaf>, seed: usize) -> taffy::tree::NodeId {
        // CJK-heavy tokens: shaping + font-book fallback walk is the real
        // cost upstream measured on COLMAP, not 4-char ASCII words.
        let text = (0..8)
            .map(|i| format!("页面布局引擎第{}段第{}节点", seed, i))
            .collect::<Vec<_>>()
            .join(" ");
        tree.new_leaf_with_context(
            Style::default(),
            TextLeaf::Run {
                text,
                font_size: 16.0,
                bold: false,
                color: [0, 0, 0, 255],
                line_height: 19.2,
                decorations: crate::diting_css::TextDecorations::default(),
                baseline_shift: 0.0,
                mono: false,
                word_spacing: 0.0,
                ws: WhiteSpace::Normal,
                ellipsis: false,
                tokens: std::cell::RefCell::new(None),
                small_caps: false,
            },
        )
        .unwrap()
    }

    fn text_page() -> (TaffyTree<TextLeaf>, taffy::tree::NodeId, Vec<taffy::tree::NodeId>) {
        let mut tree = TaffyTree::new();
        let kids: Vec<taffy::tree::NodeId> = (0..40)
            .map(|s| paragraph(&mut tree, s))
            .collect();
        let root = tree
            .new_with_children(
                Style {
                    display: taffy::style::Display::Flex,
                    flex_direction: taffy::style::FlexDirection::Column,
                    size: taffy::geometry::Size {
                        width: Dimension::length(800.0),
                        height: Dimension::auto(),
                    },
                    ..Style::default()
                },
                &kids,
            )
            .unwrap();
        (tree, root, kids)
    }

    fn solve(
        tree: &mut TaffyTree<TextLeaf>,
        root: taffy::tree::NodeId,
        fonts: &FontBook,
        width: AvailableSpace,
        calls: &AtomicUsize,
    ) {
        let space = taffy::geometry::Size { width, height: AvailableSpace::MaxContent };
        tree.compute_layout_with_measure(root, space, |inputs, _id, ctx, style| {
            calls.fetch_add(1, Ordering::Relaxed);
            match ctx {
                Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                    let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                    measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                }
                _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
            }
        })
        .unwrap();
    }

    #[test]
    fn text_heavy_solve_timing() {
        let t_font = Instant::now();
        let fonts = crate::diting_fonts::font_book();
        eprintln!("font book ready: {:?}", t_font.elapsed());
        let (mut tree, root, leaves) = text_page();
        let calls = AtomicUsize::new(0);
        let t0 = Instant::now();
        solve(&mut tree, root, &fonts, AvailableSpace::Definite(800.0), &calls);
        eprintln!("first solve of 40x8-token CJK runs ({} measure calls): {:?}",
            calls.load(Ordering::Relaxed), t0.elapsed());
        // A plain re-solve is served from taffy's cache (zero measure calls,
        // ~1ms) — the memo's win is only visible on a dirty tree, the shape
        // of every real style/attr mutation re-layout.
        for id in &leaves {
            let _ = tree.mark_dirty(*id);
        }
        let before = calls.load(Ordering::Relaxed);
        let t1 = Instant::now();
        solve(&mut tree, root, &fonts, AvailableSpace::Definite(800.0), &calls);
        eprintln!("forced re-solve ({} measure calls, each re-shaped every token before the memo): {:?}",
            calls.load(Ordering::Relaxed) - before, t1.elapsed());
        let h = tree.layout(root).map(|l| l.size.height).unwrap_or(0.0);
        assert!(h > 0.0, "text page must lay out to a positive height");
    }

    #[test]
    fn memo_populated_after_solve_and_survives_dirty_resolve() {
        let (mut tree, root, leaves) = text_page();
        let fonts = crate::diting_fonts::font_book();
        let calls = AtomicUsize::new(0);
        solve(&mut tree, root, &fonts, AvailableSpace::Definite(800.0), &calls);
        // A dirty re-solve re-enters the measure closure for every leaf —
        // the memo must be populated and stay populated (every probe after
        // the first clones the Rc instead of re-shaping).
        for id in &leaves {
            let _ = tree.mark_dirty(*id);
        }
        solve(&mut tree, root, &fonts, AvailableSpace::Definite(800.0), &calls);
        for id in &leaves {
            match tree.get_node_context(*id) {
                Some(TextLeaf::Run { tokens, .. }) => assert!(tokens.borrow().is_some()),
                _ => panic!("expected a Run leaf"),
            }
        }
    }
}

#[cfg(test)]
mod box_shadow_paint_tests {
    // blitz#349 family: outer shadows emit one BoxShadow item per layer
    // UNDER the element's own background, inset layers one per layer
    // ABOVE it (still under the border) — both emit orders reversed
    // (first-declared layer paints on top).
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn items(sheet: &str, body: &str) -> Vec<PaintItem> {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
    }

    #[test]
    fn shadow_items_reversed_under_background() {
        let items = items(
            "#card { width: 40px; height: 20px; background: blue; box-shadow: 1px 1px red, 2px 2px green }",
            r#"<div id="card"></div>"#,
        );
        let at: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| matches!(it, PaintItem::BoxShadow { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(at.len(), 2);
        assert!(matches!(&items[at[0]], PaintItem::BoxShadow { dx, .. } if *dx == 2.0), "reversed: green first");
        assert!(matches!(&items[at[1]], PaintItem::BoxShadow { dx, .. } if *dx == 1.0), "red second");
        let bg = items
            .iter()
            .position(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0, 0, 255, 255]));
        assert!(bg.is_some_and(|b| at.iter().all(|s| *s < b)), "shadows under the background");
    }

    #[test]
    fn inset_layers_above_background_below_border() {
        let items = items(
            "#card { width: 40px; height: 20px; background: blue; border: 1px solid black; box-shadow: 2px 2px red, inset 2px 2px green }",
            r#"<div id="card"></div>"#,
        );
        let outer = items
            .iter()
            .position(|it| matches!(it, PaintItem::BoxShadow { inset: false, .. }));
        let inset = items
            .iter()
            .position(|it| matches!(it, PaintItem::BoxShadow { inset: true, .. }));
        let bg = items
            .iter()
            .position(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0, 0, 255, 255]));
        let border = items.iter().position(|it| matches!(it, PaintItem::Border { .. }));
        let (outer, inset, bg, border) = (outer.unwrap(), inset.unwrap(), bg.unwrap(), border.unwrap());
        assert!(outer < bg, "outer shadow under the background");
        assert!(bg < inset, "inset shadow above the background");
        assert!(inset < border, "inset shadow under the border");
    }

    /// blitz#901 family: the backdrop filter is the element's FIRST item —
    /// it must see everything painted beneath the element and none of the
    /// element's own ink.
    #[test]
    fn backdrop_filter_is_first_item_of_its_element() {
        let items = items(
            "#card { width: 40px; height: 20px; background: blue; border-radius: 8px; box-shadow: 2px 2px red; backdrop-filter: blur(6px) }",
            r#"<div id="card"></div>"#,
        );
        let bd = items
            .iter()
            .position(|it| matches!(it, PaintItem::BackdropFilter { blur, .. } if *blur == 6.0));
        let outer = items
            .iter()
            .position(|it| matches!(it, PaintItem::BoxShadow { inset: false, .. }));
        let bg = items
            .iter()
            .position(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [0, 0, 255, 255]));
        let (bd, outer, bg) = (bd.unwrap(), outer.unwrap(), bg.unwrap());
        assert!(bd < outer, "backdrop filter beneath the element's own shadow");
        assert!(bd < bg, "backdrop filter beneath its own background");
        let radii = match &items[bd] {
            PaintItem::BackdropFilter { radii, .. } => *radii,
            _ => unreachable!(),
        };
        assert!(radii.iter().all(|r| r.0 > 0.0), "carries the clamped corner radii: {radii:?}");
    }
}

#[cfg(test)]
mod white_space_pre_family_tests {
    // Batch 106: the preserve modes end-to-end through real layout — a
    // <pre>'s hard breaks stack line boxes, the author can collapse it back
    // with white-space: normal, blank source lines own a line box, and
    // whitespace-only pre content still keeps its line (the flush_run
    // preserve gate).
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn pre_rect(body: &str) -> Rect {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        let pre = tree.query_selector("pre").expect("selector parse").expect("pre in body");
        rects[&pre]
    }

    #[test]
    fn pre_element_stacks_hard_breaks() {
        let one = pre_rect(r#"<pre>abc</pre>"#);
        let three = pre_rect(r#"<pre>abc
def
ghi</pre>"#);
        assert!(three.height > one.height + 30.0, "two hard breaks must add two line boxes: one={} three={}", one.height, three.height);
        assert!(one.height < three.height / 2.5, "one line is well under a third of three");
    }

    #[test]
    fn author_normal_collapses_pre_back_to_one_line() {
        let one = pre_rect(r#"<pre>abc</pre>"#);
        let collapsed = pre_rect(r#"<pre style="white-space: normal">abc
def</pre>"#);
        assert_eq!(collapsed.height, one.height, "collapsed whitespace = one line box");
    }

    #[test]
    fn blank_lines_inside_pre_own_a_line_box() {
        let two = pre_rect(r#"<pre>abc
def</pre>"#);
        let blank = pre_rect(r#"<pre>abc

def</pre>"#);
        assert!(blank.height > two.height + 14.0, "the empty middle line takes a full line box: two={} blank={}", two.height, blank.height);
    }

    #[test]
    fn whitespace_only_pre_content_keeps_a_line() {
        let r = pre_rect(r#"<pre>   </pre>"#);
        assert!(r.height >= 16.0, "preserved spaces still paint a line box, h={}", r.height);
    }
}

/// #434 sticky v1 (root scroller): the pure math halves. The layout-run
/// halves (span recording, read-time shifts) are covered end-to-end in
/// diting_js runtime tests — these pin the inset/shift arithmetic and the
/// paint-run translation, where the bracket-fold and nested-delta logic
/// live.
#[cfg(test)]
mod sticky_tests {
    use crate::diting_layout::{apply_sticky_to_items, sticky_axis_shift, sticky_inset_px, PaintItem, Rect};
    use crate::diting_css::Length;

    fn bg(x: f32, y: f32) -> PaintItem {
        PaintItem::Bg {
            rect: Rect { x, y, width: 10.0, height: 10.0 },
            color: [0, 0, 0, 255],
            radius: 0.0,
        }
    }

    fn bg_xy(it: &PaintItem) -> (f32, f32) {
        let PaintItem::Bg { rect, .. } = it else { panic!("not a Bg") };
        (rect.x, rect.y)
    }

    #[test]
    fn inset_px_resolves_against_the_scrollport() {
        assert_eq!(sticky_inset_px(&Length::Px(10.0), 1000.0), 10.0);
        assert_eq!(sticky_inset_px(&Length::Percent(50.0), 1000.0), 500.0);
        // calc(10% + 5px) against an 800px scrollport
        assert_eq!(
            sticky_inset_px(&Length::Calc { percent: 10.0, px: 5.0 }, 800.0),
            85.0
        );
    }

    #[test]
    fn shift_rests_until_scrolled_past() {
        // In-flow at 100, top:0, scroll 0 — the port start has not reached it.
        assert_eq!(
            sticky_axis_shift(100.0, 40.0, Some(0.0), None, 0.0, 1000.0, 8.0, 4008.0),
            0.0
        );
    }

    #[test]
    fn shift_pins_to_the_inset_at_scroll() {
        // Scrolled to 300: the box at 100 travels 200 to sit at the port top.
        assert_eq!(
            sticky_axis_shift(100.0, 40.0, Some(0.0), None, 300.0, 1000.0, 8.0, 4008.0),
            200.0
        );
        assert_eq!(
            sticky_axis_shift(100.0, 40.0, Some(10.0), None, 300.0, 1000.0, 8.0, 4008.0),
            210.0
        );
    }

    #[test]
    fn end_inset_pulls_back_only() {
        // bottom:0 with the port's end edge 40px past the box's end edge:
        // the box is dragged back to stay inside — negative shift, never
        // pulled forward by the end inset.
        assert_eq!(
            sticky_axis_shift(1600.0, 40.0, None, Some(0.0), 600.0, 1000.0, 8.0, 4008.0),
            -40.0
        );
    }

    #[test]
    fn start_wins_the_overconstraint() {
        // Both insets constrain in opposite directions (box taller than the
        // port): the computed end shift is -50, the start inset overrides
        // to 0 — Blink's constraining-rect order.
        assert_eq!(
            sticky_axis_shift(50.0, 100.0, Some(0.0), Some(0.0), 0.0, 100.0, 0.0, 5000.0),
            0.0
        );
    }

    #[test]
    fn shift_clamps_to_the_containing_block() {
        // A 40px box in an 8..208 CB: the raw pin shift is 140 but travel
        // stops where the box's end edge meets the CB's end — 8.
        assert_eq!(
            sticky_axis_shift(160.0, 40.0, Some(0.0), None, 300.0, 1000.0, 8.0, 208.0),
            8.0
        );
    }

    #[test]
    fn apply_translates_raw_and_folds_brackets() {
        let items = vec![
            bg(0.0, 0.0),
            PaintItem::SetXf { xf: [2.0, 0.0, 0.0, 2.0, 50.0, 60.0] },
            bg(10.0, 20.0), // inside the local bracket — the matrix carries it
            PaintItem::ClearXf,
            bg(30.0, 30.0),
        ];
        let spans = vec![(0usize, 5usize, [30.0f32, 40.0f32])];
        let out = apply_sticky_to_items(&items, &spans);
        assert_eq!(bg_xy(&out[0]), (30.0, 40.0), "outside any bracket: raw translate");
        let PaintItem::SetXf { xf } = &out[1] else { panic!("not a SetXf") };
        assert_eq!(
            *xf,
            [2.0, 0.0, 0.0, 2.0, 80.0, 100.0],
            "T·M fold: e/f take the shift, linear part untouched"
        );
        assert_eq!(bg_xy(&out[2]), (10.0, 20.0), "inside the local bracket: untouched");
        assert_eq!(bg_xy(&out[4]), (60.0, 70.0), "after ClearXf: raw translate again");
        // The input run is the cached one — never mutated.
        assert_eq!(bg_xy(&items[0]), (0.0, 0.0));
    }

    #[test]
    fn apply_nested_span_adds_only_its_delta() {
        // Parent span 0..2 total [0,200]; child span 1..2 CUMULATIVE
        // [0,260] (its own 60 on top of the parent's 200). The child's
        // items must land at +260, not +460 — the child pass adds only its
        // delta over the enclosing span's already-applied translation.
        let items = vec![bg(0.0, 8.0), bg(0.0, 48.0)];
        let spans = vec![
            (0usize, 2usize, [0.0f32, 200.0f32]),
            (1usize, 2usize, [0.0f32, 260.0f32]),
        ];
        let out = apply_sticky_to_items(&items, &spans);
        assert_eq!(bg_xy(&out[0]).1, 208.0, "parent's own item: parent total");
        assert_eq!(bg_xy(&out[1]).1, 308.0, "child item: 48 + cumulative 260");
    }

    #[test]
    fn apply_zero_shift_span_is_a_noop() {
        let items = vec![bg(0.0, 8.0)];
        let spans = vec![(0usize, 1usize, [0.0f32, 0.0f32])];
        let out = apply_sticky_to_items(&items, &spans);
        assert_eq!(bg_xy(&out[0]), (0.0, 8.0));
    }

    #[test]
    fn apply_pinned_sticky_inside_scroller_keeps_its_delta() {
        // The sticky-v2 composition case: a scroller span total [0,-400]
        // enclosing a PINNED sticky span total [0,0] — the sticky's own
        // +400 pin shift exactly cancels the scroller's -400 base, so the
        // sticky span's VALUE is zero while its DELTA over the base is
        // +400. Liveness must be judged per-delta (zero-value filtering
        // was v1's root-only shortcut; here it would drop the pin and
        // scroll the head away with the content).
        let items = vec![bg(0.0, 8.0), bg(0.0, 48.0), bg(0.0, 2048.0)];
        let spans = vec![
            (0usize, 3usize, [0.0f32, -400.0f32]),
            (1usize, 2usize, [0.0f32, 0.0f32]),
        ];
        let out = apply_sticky_to_items(&items, &spans);
        assert_eq!(bg_xy(&out[1]).1, 48.0, "pinned head: NET ZERO total, it keeps its layout position while the port rides over it");
        assert_eq!(bg_xy(&out[2]).1, 1648.0, "tall content travels with the scroller base");
        assert_eq!(bg_xy(&out[0]).1, -392.0, "pre-clip item takes the scroller base only");
    }
}

/// Batch 124 (takumi#1489 probe, learn-only channel): leading collapsible
/// whitespace. Browsers trim a space sequence at the start of a line —
/// the IFC start (css-text-3 §4.1.3) and after a forced break (§4.1.2) —
/// so an indented `<p>\\n  Due <span>x</span></p>` renders flush, and a
/// space right after `<br>` never indents the next line. The opposite
/// edge is pinned too: separators BETWEEN inline siblings must survive.
#[cfg(test)]
mod batch_124_leading_ws_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn text_x(body: &str, needle: &str) -> f32 {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
            .iter()
            .find_map(|it| match it {
                // Run leaves carry the RAW node text (trimming lives in the
                // tokens); Word leaves carry the token itself.
                PaintItem::Text { text, x, .. } if text.trim_start().starts_with(needle) => Some(*x),
                _ => None,
            })
            .unwrap_or_else(|| {
                let all: Vec<String> = items
                    .iter()
                    .filter_map(|it| match it {
                        PaintItem::Text { text, .. } => Some(format!("{text:?}")),
                        _ => None,
                    })
                    .collect();
                panic!("no text item starting with {needle:?}; texts: {all:?}")
            })
    }

    #[test]
    fn leading_ws_at_ifc_start_is_trimmed_mixed_run() {
        // takumi#1489's exact shape: newline+indent text node, then a span.
        let flush = text_x(r#"<p id="f">Due <span>x</span></p>"#, "Due");
        let indented = text_x(r#"<p id="i">
  Due <span>x</span></p>"#, "Due");
        assert_eq!(indented, flush, "newline+indent at IFC start must not shift the paragraph");
    }

    #[test]
    fn leading_ws_at_ifc_start_is_trimmed_pure_text() {
        let flush = text_x(r#"<p>Due</p>"#, "Due");
        let indented = text_x(r#"<p>
  Due</p>"#, "Due");
        assert_eq!(indented, flush);
    }

    #[test]
    fn mid_run_space_between_inline_siblings_survives() {
        // The opposite edge: the separator between inline siblings is
        // mid-line whitespace — per-node trimming would glue these.
        let glued = text_x(r#"<p><span>a</span>b<span>c</span></p>"#, "b");
        let spaced = text_x(r#"<p><span>a</span> b <span>c</span></p>"#, "b");
        assert!(spaced > glued + 2.0, "separator space must advance: glued={glued} spaced={spaced}");
    }

    #[test]
    fn leading_ws_after_forced_break_is_trimmed() {
        // css-text-3 §4.1.2: a collapsible space sequence at the beginning
        // of a line is removed — including the line opened by a <br>.
        let base = text_x(r#"<p>abc<br>def</p>"#, "def");
        let spaced = text_x(r#"<p>abc<br> def</p>"#, "def");
        assert_eq!(spaced, base, "space after <br> opens the next line and must be removed");
    }
}

/// Batch 125 (#21, takumi#1490 same face, learn-only channel): a wrapping
/// inline span paints per line FRAGMENT. Solid background-color bands
/// predate this pass; the new halves are the gradient — every fragment
/// samples one continuous strip over the union of the bands (Blink
/// PaintRectForImageStrip: a to-right gradient never restarts per line) —
/// and border under box-decoration-break: slice: top/bottom edges on every
/// fragment, left only the first, right only the last, strips outside the
/// text bands so glyphs stay uncovered.
#[cfg(test)]
mod batch_125_inline_fragment_deco_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn items(sheet: &str, body: &str) -> Vec<PaintItem> {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
    }

    const WRAP_BODY: &str = r#"<p id="p"><span id="s">alpha beta gamma delta epsilon zeta eta theta</span></p>"#;

    #[test]
    fn solid_bg_wrapping_span_paints_per_line_bands() {
        let items = items("#p { width: 200px } #s { background: rgb(255,0,0) }", WRAP_BODY);
        let bands: Vec<&Rect> = items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Bg { rect, color, .. } if *color == [255, 0, 0, 255] => Some(rect),
                _ => None,
            })
            .collect();
        assert!(bands.len() >= 2, "one Bg per line fragment, got {}", bands.len());
        let ys: Vec<f32> = bands.iter().map(|r| r.y).collect();
        ys.windows(2).for_each(|w| assert!(w[1] > w[0], "bands stack line by line: {ys:?}"));
    }

    #[test]
    fn gradient_wrapping_span_samples_continuous_union_strip() {
        let items = items(
            "#p { width: 200px } #s { background: linear-gradient(to right, red, blue) }",
            WRAP_BODY,
        );
        let grads: Vec<Rect> = items
            .iter()
            .filter_map(|it| match it {
                PaintItem::BgGradient { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert!(
            grads.len() >= 2,
            "one clipped gradient per line fragment, got {} (gradient-only span used to paint nothing)",
            grads.len()
        );
        assert!(
            grads.windows(2).all(|w| w[0].x == w[1].x && w[0].width == w[1].width),
            "all fragments sample ONE union rect (continuous strip): {grads:?}"
        );
        // Each gradient is bracketed Clip(band) … PopClip, and the clip
        // rects are the per-line bands — narrower than the union strip.
        let band_rects: Vec<Rect> = items
            .windows(3)
            .filter_map(|w| match (&w[0], &w[1], &w[2]) {
                (PaintItem::Clip { rect: c }, PaintItem::BgGradient { rect, .. }, PaintItem::PopClip) => {
                    Some((*c, *rect))
                }
                _ => None,
            })
            .map(|(c, _)| c)
            .collect();
        assert_eq!(band_rects.len(), grads.len(), "every gradient rides a Clip bracket");
        assert!(
            band_rects.iter().any(|b| b.width < grads[0].width),
            "at least one fragment is narrower than the union: {band_rects:?} vs {:?}",
            grads[0]
        );
    }

    /// Small-caps word leaves are PRE-uppercased at build time (`ß` →
    /// "SS") and carry the reduced size on lowercase segments — the leaf
    /// must never paint lowercase glyphs, only smaller uppercase ones.
    #[test]
    fn small_caps_word_leaves_pre_uppercase_at_ratio() {
        let tree = parse_html("<html><body>x</body></html>");
        let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
        let _styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let mut tt = TaffyTree::<TextLeaf>::new();
        let leaves = build_word_leaves(
            "hello AB", 16.0, false, [0, 0, 0, 255], 20.0, TextDecorations::default(),
            0.0, false, 0.0, true, &crate::diting_fonts::font_book(), &mut tt,
        );
        let got: Vec<(String, f32)> = leaves
            .iter()
            .filter_map(|id| match tt.get_node_context(*id) {
                Some(TextLeaf::Word { text, font_size, .. }) => Some((text.clone(), *font_size)),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("HELLO".to_string(), 16.0 * text::SMALL_CAPS_RATIO),
                // The whitespace token stays its own full-size leaf.
                (" ".to_string(), 16.0),
                ("AB".to_string(), 16.0),
            ],
            "lowercase run uppercases at 70%, uppercase run stays full size"
        );
    }

    #[test]
    fn border_wrapping_span_slice_edges() {
        let items = items("#p { width: 200px } #s { border: 4px solid rgb(255,0,0) }", WRAP_BODY);
        let strips: Vec<Rect> = items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Bg { rect, color, .. } if *color == [255, 0, 0, 255] => Some(*rect),
                _ => None,
            })
            .collect();
        assert!(
            strips.len() >= 6,
            "border-only span used to paint nothing; need >= 2 bands x (top+bottom) + left + right, got {}",
            strips.len()
        );
        let horizontal: Vec<&Rect> = strips.iter().filter(|r| r.width > r.height).collect();
        let vertical: Vec<&Rect> = strips.iter().filter(|r| r.width <= r.height).collect();
        // Slice: top+bottom per fragment (so an even count >= 4 = 2 bands),
        // exactly ONE left and ONE right edge.
        assert!(horizontal.len() >= 4 && horizontal.len().is_multiple_of(2), "top+bottom per band: {:?}", horizontal);
        assert_eq!(vertical.len(), 2, "slice = left only on first band, right only on last: {strips:?}");
        let xs: Vec<f32> = vertical.iter().map(|r| r.x).collect();
        let min_x = strips.iter().map(|r| r.x).fold(f32::INFINITY, f32::min);
        assert!(xs.contains(&min_x), "left edge opens the inline: {xs:?} vs min {min_x}");
        // The inline CLOSES at the LAST fragment's right edge (later lines
        // are typically shorter — the right edge is not the union's max).
        let last_bottom = horizontal.iter().max_by(|a, b| a.y.total_cmp(&b.y)).unwrap();
        assert!(
            vertical
                .iter()
                .any(|r| (r.x + r.width - (last_bottom.x + last_bottom.width)).abs() < 0.01),
            "right edge closes the last fragment: {strips:?}"
        );
    }

    // #26: a span with ONLY box-shadow used to paint nothing at all — the
    // band guard checked bg/gradient/border but not shadows.
    #[test]
    fn shadow_only_wrapping_span_paints_per_fragment() {
        let items = items("#p { width: 200px } #s { box-shadow: 6px 6px 0 rgb(255,0,0) }", WRAP_BODY);
        let shadows: Vec<&PaintItem> = items
            .iter()
            .filter(|it| matches!(it, PaintItem::BoxShadow { inset: false, .. }))
            .collect();
        assert!(
            shadows.len() >= 2,
            "one outer shadow per line fragment (shadow-only span used to paint nothing), got {}",
            shadows.len()
        );
        let rects: Vec<Rect> = shadows
            .iter()
            .filter_map(|it| match it {
                PaintItem::BoxShadow { rect, dx, dy, color, .. } => {
                    assert_eq!(*color, [255, 0, 0, 255]);
                    assert!((*dx - 6.0).abs() < 0.01 && (*dy - 6.0).abs() < 0.01);
                    Some(*rect)
                }
                _ => None,
            })
            .collect();
        let ys: Vec<f32> = rects.iter().map(|r| r.y).collect();
        ys.windows(2).for_each(|w| assert!(w[1] > w[0], "shadow fragments stack line by line: {ys:?}"));
    }

    // #26: border-radius on a wrapping span — box-decoration-break: slice.
    // First fragment keeps the LEFT corners, last the RIGHT, middles square
    // (Chrome headless evidence on the issue).
    #[test]
    fn rounded_wrapping_span_slices_corner_radii() {
        // A single-line span keeps all four corners (uniform → the Bg
        // shortcut, not BgCorner) — computed before the wrapping `items`
        // binding shadows the helper.
        let single_line = items(
            "#s { background: rgb(250,204,21); border-radius: 8px }",
            r#"<p id="p"><span id="s">tiny</span></p>"#,
        );
        let rounded: Vec<f32> = single_line
            .iter()
            .filter_map(|it| match it {
                PaintItem::Bg { radius, color, .. } if *color == [250, 204, 21, 255] => Some(*radius),
                _ => None,
            })
            .collect();
        assert!(
            rounded.iter().any(|r| *r > 0.0),
            "single-line span rounds all four corners: {rounded:?}"
        );
        let items = items(
            "#p { width: 200px } #s { background: rgb(250,204,21); border-radius: 12px }",
            WRAP_BODY,
        );
        let corners: Vec<[(f32, f32); 4]> = items
            .iter()
            .filter_map(|it| match it {
                PaintItem::BgCorner { radii, .. } => Some(*radii),
                _ => None,
            })
            .collect();
        assert_eq!(
            corners.len(),
            2,
            "exactly the first and last fragment carry rounded corners: {:?}",
            corners
        );
        // First fragment: TL/BL rounded (left), TR/BR square.
        assert!(corners[0][0].0 > 0.0 && corners[0][3].0 > 0.0, "first band left corners: {:?}", corners[0]);
        assert!(corners[0][1].0 == 0.0 && corners[0][2].0 == 0.0, "first band right corners square: {:?}", corners[0]);
        // Last fragment: TR/BR rounded (right), TL/BL square.
        let last = corners[corners.len() - 1];
        assert!(last[1].0 > 0.0 && last[2].0 > 0.0, "last band right corners: {last:?}");
        assert!(last[0].0 == 0.0 && last[3].0 == 0.0, "last band left corners square: {last:?}");
        // Oversized radii clamp per fragment (Chrome's proportional
        // reduction): 12px on a ~18px line never paints as a full 12.
        assert!(
            corners[0][0].0 < 12.0,
            "radius clamped against the fragment box: {:?}",
            corners[0]
        );
        // Middle fragments stay square Bg bands.
        let middle: Vec<f32> = items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Bg { radius, color, .. } if *color == [250, 204, 21, 255] => Some(*radius),
                _ => None,
            })
            .collect();
        if !middle.is_empty() {
            assert!(
                middle.iter().all(|r| *r == 0.0),
                "middle fragments are square bands: {middle:?}"
            );
        }
    }

    // #26: shadow + radius together — the shadow rides the fragment box and
    // the outer shadow layer lands BELOW the span's own background.
    #[test]
    fn shadow_and_radius_stack_in_block_layer_order() {
        let items = items(
            "#p { width: 200px } #s { background: rgb(165,243,252); border-radius: 10px; box-shadow: 2px 3px 4px rgb(0,0,200) }",
            WRAP_BODY,
        );
        let first_shadow = items.iter().position(|it| matches!(it, PaintItem::BoxShadow { inset: false, .. }));
        let first_bg = items
            .iter()
            .position(|it| matches!(it, PaintItem::BgCorner { color, .. } if *color == [165, 243, 252, 255]));
        let (Some(s), Some(b)) = (first_shadow, first_bg) else {
            panic!("expected shadow items and rounded bg items, got {items:?}");
        };
        assert!(s < b, "outer shadow paints below the background (block layer order)");
        let n_shadow = items
            .iter()
            .filter(|it| matches!(it, PaintItem::BoxShadow { inset: false, .. }))
            .count();
        let n_bg = items
            .iter()
            .filter(|it| matches!(it, PaintItem::BgCorner { color, .. } if *color == [165, 243, 252, 255]))
            .count();
        assert_eq!(n_shadow, n_bg + items.iter().filter(|it| matches!(it, PaintItem::Bg { color, .. } if *color == [165, 243, 252, 255])).count(),
            "one shadow per painted fragment");
    }
}

#[cfg(test)]
mod container_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_full, CssMediaType, MediaOverrides};
    use crate::diting_dom::tree_sink::parse_html;

    fn by_id(tree: &DomTree, id: &str) -> NodeId {
        tree.query_selector_all(&format!("#{id}")).unwrap()[0]
    }

    /// Pass 1 styles → solve → container plan, mirroring layout_run_all's
    /// probe stage (no geometry cache here, so the probe always solves).
    fn plan_for(
        sheet: &str,
        body: &str,
    ) -> (DomTree, Vec<crate::diting_css::ParsedRule>, ContainerPlan) {
        let html = format!("<html><head><style>{sheet}</style></head><body>{body}</body></html>");
        let tree = parse_html(&html);
        let (rules, _kf, containers) = parse_stylesheet_full(
            sheet,
            (800.0, 600.0),
            CssMediaType::Screen,
            &MediaOverrides::default(),
        );
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let solved = layout_solve(
            &tree,
            &styles,
            &crate::diting_fonts::font_book(),
            800.0,
            600.0,
            None,
            None,
        );
        let boxes = solved.dom_boxes();
        let plan = container_plan(&tree, &styles, &boxes, &containers, rules.len());
        (tree, rules, plan)
    }

    #[test]
    fn container_plan_gates_by_container_width() {
        // The moli#282 repro shape: one 300px container (passes
        // min-width:200px), one 100px container (fails), one element with no
        // container ancestor (no answer at all).
        let (tree, rules, plan) = plan_for(
            "#cqbox { container-type: inline-size; width: 300px } \
             .inner { color: rgb(0, 0, 255) } \
             @container (min-width: 200px) { .inner { color: rgb(255, 0, 0) } }",
            "<div id='cqbox'><div class='inner' id='i1'>x</div></div>\
             <div style='width: 100px; container-type: inline-size'><div class='inner' id='i2'>y</div></div>\
             <div class='inner' id='i3'>z</div>",
        );
        assert_eq!(plan.extra_rules.len(), 1);
        let gates = &plan.gates[&rules.len()];
        let (i1, i2, i3) = (by_id(&tree, "i1"), by_id(&tree, "i2"), by_id(&tree, "i3"));
        assert!(gates.binary_search(&i1.index()).is_ok(), "300px container passes min-width:200px");
        assert!(gates.binary_search(&i2.index()).is_err(), "100px container fails");
        assert!(gates.binary_search(&i3.index()).is_err(), "no container ancestor, no gate");

        // The gated cascade is what makes that plan visible in styles.
        let mut full_rules = rules.clone();
        full_rules.extend(plan.extra_rules.iter().cloned());
        let styles = compute_styles_gated(
            &tree,
            &full_rules,
            &crate::diting_css::KeyframesMap::new(),
            None,
            &[],
            rules.len(),
            &plan.gates,
            (1280.0, 720.0),
        );
        assert_eq!(styles[&i1].color, Some(crate::diting_css::Color(255, 0, 0, 255)));
        assert_eq!(styles[&i2].color, Some(crate::diting_css::Color(0, 0, 255, 255)));
        assert_eq!(styles[&i3].color, Some(crate::diting_css::Color(0, 0, 255, 255)));
    }

    #[test]
    fn container_plan_nearest_and_named_wins() {
        let (tree, rules, plan) = plan_for(
            ".item { color: rgb(0, 0, 255) } \
             @container side (min-width: 200px) { .item { color: rgb(255, 0, 0) } }",
            "<div style='container-type: inline-size; container-name: side; width: 400px'>\
               <div style='container-type: inline-size; width: 100px'><div class='item' id='a'>x</div></div>\
             </div>\
             <div style='container-type: inline-size; width: 400px'><div class='item' id='b'>y</div></div>",
        );
        let gates = &plan.gates[&rules.len()];
        let (a, b) = (by_id(&tree, "a"), by_id(&tree, "b"));
        assert!(
            gates.binary_search(&a.index()).is_ok(),
            "named lookup walks PAST the nearer anonymous 100px container to the 400px 'side' one"
        );
        assert!(gates.binary_search(&b.index()).is_err(), "no 'side' ancestor at all");
    }

    #[test]
    fn container_plan_element_cannot_query_itself() {
        // #self is BOTH the container and the only .inner: a query against
        // its own width must not fire.
        let (tree, rules, plan) = plan_for(
            "#self { container-type: inline-size; width: 300px } \
             @container (min-width: 100px) { .inner { color: rgb(255, 0, 0) } }",
            "<div id='self' class='inner'>x</div>",
        );
        assert!(
            plan.extra_rules.is_empty(),
            "the container itself is never inside its own query scope; gates={:?}",
            plan.gates
        );
        let _ = (tree, rules);
    }

    #[test]
    fn bake_container_units_exact_strings() {
        assert_eq!(
            bake_container_units(".a { width: 10cqw; height: 50cqh }", 300.0, 200.0),
            ".a { width: 30px; height: 100px }",
        );
        assert_eq!(
            bake_container_units("padding: 2.5cqi", 300.0, 200.0),
            "padding: 7.500px",
        );
        assert_eq!(bake_container_units("margin: -10cqb", 300.0, 200.0), "margin: -20px");
        // Non-container units and digit+letter runs (hex colors, asset names)
        // pass through byte-identical.
        assert_eq!(
            bake_container_units(".b { width: 30px; color: #a1b2c3; background: url(logo-2x.png) }", 300.0, 200.0),
            ".b { width: 30px; color: #a1b2c3; background: url(logo-2x.png) }",
        );
    }
}

#[cfg(test)]
mod float_continuation_tests {
    // #127: the post-layout float continuation pass (batch 8g) climbed out
    // of a plain overflow:visible block and clamped later PLAIN blocks'
    // max-width to the float's leftover sliver. CSS 2.1 §9.5 says a float's
    // band moves the border box only of a later in-flow box that establishes
    // an independent formatting context (or a table); a plain block keeps
    // its full width and only its inner line boxes shorten. On the tmall
    // publish form this collapsed the width:auto/margin:auto cards to ~95px
    // because a feedback float's band stuck out of its shrunk parent.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom};

    const VW: f32 = 1280.0;

    fn card_rect(html: &str, sheet: &str, sel: &str) -> (f32, f32, f32, f32) {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector(sel).unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        (r.x, r.y, r.width, r.height)
    }

    #[test]
    fn plain_block_keeps_full_width_beside_escaped_float() {
        // The float lives inside a shrunk earlier block (height 20px), so
        // its 60px-tall band sticks out below it and vertically overlaps
        // the later card — the exact #127 shape. Chrome: the card's BOX
        // stays full width (its line boxes would wrap, not its border box).
        let (x, _y, w, _h) = card_rect(
            r#"<html><body>
                <div id="msg"><div id="fcell">feedback</div></div>
                <div id="card"><div id="inner">card</div></div>
            </body></html>"#,
            r#"
                body { margin: 0; }
                #msg { height: 20px; }
                #fcell { float: left; width: 700px; height: 60px; }
                #card { width: auto; margin: 0 auto; }
                #inner { height: 40px; }
            "#,
            "#card",
        );
        assert!(
            (x - 0.0).abs() <= 1.0 && (w - VW).abs() <= 1.0,
            "plain block after an escaped float keeps full width; got x={x} w={w}"
        );
    }

    #[test]
    fn bfc_block_still_narrows_beside_float() {
        // §9.5 keeps its teeth: a later sibling that establishes an
        // independent formatting context (overflow: hidden) must have its
        // border box dodge the float's band.
        let (_x, _y, w, _h) = card_rect(
            r#"<html><body>
                <div id="msg"><div id="fcell">feedback</div></div>
                <div id="bfc"><div id="inner">section</div></div>
            </body></html>"#,
            r#"
                body { margin: 0; }
                #msg { height: 20px; }
                #fcell { float: left; width: 700px; height: 60px; }
                #bfc { overflow: hidden; }
                #inner { height: 40px; }
            "#,
            "#bfc",
        );
        assert!(
            (w - (VW - 700.0)).abs() <= 1.0,
            "overflow:hidden block narrows past the float's right edge; got w={w}"
        );
    }

    #[test]
    fn float_contained_by_bfc_ancestor_reaches_no_outer_blocks() {
        // The climb must stop at an ancestor that establishes a formatting
        // context: the float is contained there and can never stick out to
        // displace that ancestor's own later siblings — even when those
        // siblings would themselves dodge floats.
        let (x, _y, w, _h) = card_rect(
            r#"<html><body>
                <div id="msg"><div id="fcell">feedback</div></div>
                <div id="bfc"><div id="inner">section</div></div>
            </body></html>"#,
            r#"
                body { margin: 0; }
                #msg { height: 20px; overflow: hidden; }
                #fcell { float: left; width: 700px; height: 60px; }
                #bfc { overflow: hidden; }
                #inner { height: 40px; }
            "#,
            "#bfc",
        );
        assert!(
            (x - 0.0).abs() <= 1.0 && (w - VW).abs() <= 1.0,
            "float inside an overflow:hidden block never narrows outer siblings; got x={x} w={w}"
        );
    }
}
