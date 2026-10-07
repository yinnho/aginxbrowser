// Colocated contract suite — split out of the god file (ratchet).
#[cfg(test)]
mod style_text_leak_tests {
    // #187: the baidu homepage's <style> raw CSS rendered as body text at
    // the top-left (browser86 M0 sighting, twice on different days). The
    // render gate matched ONLY Some(Display::None); an element missing from
    // the styles map fell to ComputedStyle::default() — display None ≠
    // Some(None) — and built, laying its raw text as a visible run. The
    // gate now hides uncovered elements outright; these pin both halves.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::*;

    // A <style> under a VISIBLE parent (body) — the coverage-gap hazard
    // shape. A head-hosted style is masked by head's own display:none entry,
    // so the dropped-entry divergence only shows on this placement.
    const HTML: &str = r#"<html><body><style id="leak">body { margin: 0 }</style><p id="p">hello</p></body></html>"#;

    fn text_items(drop_selector: Option<&str>) -> Vec<String> {
        let tree = parse_html(HTML);
        let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
        let mut styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        if let Some(sel) = drop_selector {
            let id = tree.query_selector(sel).unwrap().unwrap();
            styles.remove(&id);
        }
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_coverage_never_paints_style_text() {
        let texts = text_items(None);
        assert!(texts.iter().any(|t| t.contains("hello")), "body text renders: {texts:?}");
        assert!(!texts.iter().any(|t| t.contains("margin")), "UA display:none keeps style text out of paint");
    }

    #[test]
    fn styles_map_miss_hides_the_element_instead_of_leaking_its_text() {
        // The exact leak shape: every element cascaded except the style
        // one (any coverage gap — walk root, epoch edge, future element
        // kind). The missing element must paint nothing while the covered
        // rest of the page keeps rendering.
        let texts = text_items(Some("#leak"));
        assert!(texts.iter().any(|t| t.contains("hello")), "covered elements keep rendering: {texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("margin") || t.contains("body")),
            "a styles-map miss must not paint the raw CSS text"
        );
    }
}

#[cfg(test)]
mod hidden_replaced_tests {
    // #196: replaced elements (is_replaced_tag family) attach from run-build
    // call sites that bypass build_element_inner's display gate, so a
    // display:none textarea — baidu/sina ship page templates as hidden
    // textareas — laid out as a real control and painted its VALUE (the raw
    // `<style …>` template text) as visible content. The gate now lives in
    // build_replaced_leaf itself, closing every attach site at once. These
    // pin the baidu shape (parse-time inline attribute), the author-rule
    // shape, a second family member (input), and the visible control that
    // must keep painting.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::*;

    // A form control's value is NOT a PaintItem::Text — the replaced paint
    // walk wraps it in PaintItem::Replaced { alt } (with form: Some(_)), so
    // the harness collects both faces: Text for real runs, Replaced alt for
    // control values.
    fn texts(html: &str, sheet: &str) -> Vec<String> {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Text { text, .. } => Some(text.clone()),
                PaintItem::Replaced { alt: Some((text, ..)), .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    const TEMPLATE: &str =
        "&lt;style data-for=\"result\" id=\"css_result\"&gt;#ftCon{display:none}&lt;/style&gt;";

    #[test]
    fn inline_attr_hidden_textarea_does_not_paint_its_value() {
        // The baidu homepage shape: template as textarea content, hidden by
        // a parse-time inline style attribute.
        let html = format!(
            r#"<html><body><textarea style="display:none;">{TEMPLATE}</textarea><p id="p">hello</p></body></html>"#
        );
        let ts = texts(&html, "");
        assert!(ts.iter().any(|t| t.contains("hello")), "rest of page renders: {ts:?}");
        assert!(
            !ts.iter().any(|t| t.contains("css_result") || t.contains("ftCon")),
            "hidden textarea must not paint its template value: {ts:?}"
        );
    }

    #[test]
    fn stylesheet_rule_hidden_textarea_does_not_paint_its_value() {
        let html = format!(
            r#"<html><body><textarea class="tmpl">{TEMPLATE}</textarea><p id="p">hello</p></body></html>"#
        );
        let ts = texts(&html, ".tmpl { display: none }");
        assert!(ts.iter().any(|t| t.contains("hello")), "rest of page renders: {ts:?}");
        assert!(
            !ts.iter().any(|t| t.contains("css_result") || t.contains("ftCon")),
            "rule-hidden textarea must not paint its template value: {ts:?}"
        );
    }

    #[test]
    fn hidden_input_does_not_paint_its_value() {
        // Second family member: the same gate must cover the whole
        // is_replaced_tag set, not just textarea.
        let html = r#"<html><body><input value="SECRETV" style="display:none;"><p id="p">hello</p></body></html>"#;
        let ts = texts(html, "");
        assert!(ts.iter().any(|t| t.contains("hello")), "rest of page renders: {ts:?}");
        assert!(!ts.iter().any(|t| t.contains("SECRETV")), "hidden input must not paint: {ts:?}");
    }

    #[test]
    fn hidden_img_does_not_paint_its_alt() {
        // The issue's explicit ask: pin a display:none img too — the whole
        // is_replaced_tag family (img/video/iframe/input/select/svg…)
        // attaches through the same call sites. An undecoded img paints its
        // alt run inside the Replaced placeholder; hidden, it must vanish.
        let html = r#"<html><body><img src="/missing.png" alt="SECRETALT" style="display:none;"><p id="p">hello</p></body></html>"#;
        let ts = texts(html, "");
        assert!(ts.iter().any(|t| t.contains("hello")), "rest of page renders: {ts:?}");
        assert!(!ts.iter().any(|t| t.contains("SECRETALT")), "hidden img must not paint its alt: {ts:?}");
    }

    #[test]
    fn visible_textarea_still_paints_its_value() {
        // The gate must not over-hide: a visible control keeps painting its
        // value on the form-control path.
        let html = r#"<html><body><textarea>shown value</textarea><p id="p">hello</p></body></html>"#;
        let ts = texts(html, "");
        assert!(ts.iter().any(|t| t.contains("shown value")), "visible textarea paints: {ts:?}");
        assert!(ts.iter().any(|t| t.contains("hello")), "rest of page renders: {ts:?}");
    }
}

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
    use html5ever::ns;

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

    #[test]
    fn flex_wrap_align_content_distributes_wrapped_lines() {
        // blitz#1059 absorption: align-content moves the wrapped-line block
        // in the cross axis. 300px-tall wrap container, two 50px lines →
        // 100px of lines, 200px free; center puts 100px above the block.
        let sheet = "body { margin: 0 } #c { display: flex; flex-wrap: wrap; width: 100px; height: 300px; align-content: center } \
                     #a { width: 100px; height: 50px } #b { width: 60px; height: 50px }";
        let rects = layout(sheet, r#"<div id="c"><div id="a"></div><div id="b"></div></div>"#);
        assert_at(&rects, 100.0, 50.0, 0.0, 100.0);
        assert_at(&rects, 60.0, 50.0, 0.0, 150.0);
        // Unset (normal→stretch): the LINE boxes stretch to share the free
        // space (150px each), children sit at their line starts — the
        // pre-fix world was identical here because taffy's default was
        // already stretch.
        let sheet = "body { margin: 0 } #c { display: flex; flex-wrap: wrap; width: 100px; height: 300px } \
                     #a { width: 100px; height: 50px } #b { width: 60px; height: 50px }";
        let rects = layout(sheet, r#"<div id="c"><div id="a"></div><div id="b"></div></div>"#);
        assert_at(&rects, 100.0, 50.0, 0.0, 0.0);
        assert_at(&rects, 60.0, 50.0, 0.0, 150.0);
        // flex-start packs the lines flush: pre-fix pages that asked for it
        // got the stretch default instead.
        let sheet = "body { margin: 0 } #c { display: flex; flex-wrap: wrap; width: 100px; height: 300px; align-content: flex-start } \
                     #a { width: 100px; height: 50px } #b { width: 60px; height: 50px }";
        let rects = layout(sheet, r#"<div id="c"><div id="a"></div><div id="b"></div></div>"#);
        assert_at(&rects, 100.0, 50.0, 0.0, 0.0);
        assert_at(&rects, 60.0, 50.0, 0.0, 50.0);
        // flex-end pins the other edge: 200px free all above the block.
        let sheet = "body { margin: 0 } #c { display: flex; flex-wrap: wrap; width: 100px; height: 300px; align-content: flex-end } \
                     #a { width: 100px; height: 50px } #b { width: 60px; height: 50px }";
        let rects = layout(sheet, r#"<div id="c"><div id="a"></div><div id="b"></div></div>"#);
        assert_at(&rects, 100.0, 50.0, 0.0, 200.0);
        assert_at(&rects, 60.0, 50.0, 0.0, 250.0);
    }

    // (the probe that diagnosed #188-2 lived here print-only; its matrix is
    // now pinned in abspos_self_align_tests below)
}

/// #188-2 (blitz#977): an out-of-flow box's own self-alignment at its
/// static position. CSS resolves the static position of an abspos child as
/// "the sole item of its flow parent" (css-flexbox §4.1; css-grid §9.2: a
/// grid area spanning the container's content edges), so the child's
/// align-self/justify-self must resolve against the parent's content box —
/// taffy (pre-OofItemStyle) never consulted them: flex kept the container's
/// align-items only, grid placed abspos children at (0,0) on every axis.
/// The correction pass runs after the static-position harvest; these pin
/// the whole Chrome-parity matrix.
#[cfg(test)]
mod abspos_self_align_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn abs_at(sheet: &str) -> (f32, f32) {
        let html = r#"<html><body><div id="c"><div id="abs"></div></div></body></html>"#;
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        let hits: Vec<(f32, f32)> = rects
            .values()
            .filter(|r| r.width == 60.0 && r.height == 40.0)
            .map(|r| (r.x, r.y))
            .collect();
        assert_eq!(hits.len(), 1, "60x40 abs box matched {hits:?}");
        hits[0]
    }

    // 400×200 container, 60×40 abspos child, body margin 0 — the same
    // fixture the Chrome ground-truth matrix was measured on.
    fn flex_sheet(container_extra: &str, child_extra: &str) -> String {
        format!(
            "body {{ margin: 0 }} #c {{ position: relative; display: flex; width: 400px; height: 200px; {container_extra} }} \
             #abs {{ position: absolute; width: 60px; height: 40px; {child_extra} }}"
        )
    }

    fn grid_sheet(extra_child: &str, extra_container: &str) -> String {
        format!(
            "body {{ margin: 0 }} #c {{ position: relative; display: grid; width: 400px; height: 200px; {extra_container} }} \
             #abs {{ position: absolute; width: 60px; height: 40px; {extra_child} }}"
        )
    }

    #[test]
    fn flex_align_self_overrides_container_align_items() {
        // ai=flex-start on the container; the child's own align-self must
        // win the cross axis. Before: all four landed at (170, 0).
        assert_eq!(abs_at(&flex_sheet("justify-content: center; align-items: flex-start", "align-self: flex-start")), (170.0, 0.0));
        assert_eq!(abs_at(&flex_sheet("justify-content: center; align-items: flex-start", "align-self: center")), (170.0, 80.0));
        assert_eq!(abs_at(&flex_sheet("justify-content: center; align-items: flex-start", "align-self: flex-end")), (170.0, 160.0));
        // Stretch degenerates to start: auto insets + definite size never
        // stretch the box (CSS2 §10.3.7) — height stays 40 too.
        assert_eq!(abs_at(&flex_sheet("justify-content: center; align-items: flex-start", "align-self: stretch")), (170.0, 0.0));
    }

    #[test]
    fn flex_justify_self_is_ignored_per_spec() {
        // css-flexbox §4: justify-self does not apply to flex items — the
        // main axis carries only the container's justify-content (flex-start
        // here → x=0). Pinned so a future "fix" can't quietly apply it.
        for js in ["start", "center", "end"] {
            assert_eq!(
                abs_at(&flex_sheet("justify-content: flex-start", &format!("justify-self: {js}"))),
                (0.0, 0.0),
                "justify-self={js} must not move a flex item"
            );
        }
    }

    #[test]
    fn flex_container_alignment_stays_correct() {
        // Regression guard for the harvest path the correction rides on:
        // container-level justify-content/align-items keep working when the
        // child states no align-self.
        assert_eq!(abs_at(&flex_sheet("justify-content: center; align-items: center", "")), (170.0, 80.0));
        assert_eq!(abs_at(&flex_sheet("justify-content: flex-end; align-items: flex-end", "")), (340.0, 160.0));
    }

    #[test]
    fn flex_column_resolves_align_self_on_x() {
        // The cross axis of a column flex container is x — the correction
        // must follow flex_direction, not assume row.
        assert_eq!(
            abs_at(&flex_sheet("flex-direction: column", "align-self: center")),
            (170.0, 0.0),
            "column flex: align-self moves x, y stays at content start"
        );
    }

    #[test]
    fn flex_align_self_respects_margin() {
        // Alignment subjects resolve inside the margin box: flex-start puts
        // the margin edge at the content edge.
        assert_eq!(
            abs_at(&flex_sheet("justify-content: center", "align-self: flex-start; margin-top: 20px")),
            (170.0, 20.0)
        );
    }

    #[test]
    fn grid_self_alignment_matrix() {
        // The full 4×4 justify-self × align-self matrix against the grid
        // container's content box. Before: every combo sat at (0, 0).
        let x_of = |js: &str| match js { "center" => 170.0, "end" => 340.0, _ => 0.0 };
        let y_of = |aslf: &str| match aslf { "center" => 80.0, "end" => 160.0, _ => 0.0 };
        for js in ["start", "center", "end", "stretch"] {
            for aslf in ["start", "center", "end", "stretch"] {
                assert_eq!(
                    abs_at(&grid_sheet(&format!("justify-self: {js}; align-self: {aslf}"), "")),
                    (x_of(js), y_of(aslf)),
                    "grid js={js} as={aslf}"
                );
            }
        }
    }

    #[test]
    fn grid_falls_back_to_items_level_defaults() {
        // justify-self/align-self unset → the container's justify-items /
        // align-items answer; no align-items either → stretch ≈ start.
        assert_eq!(
            abs_at(&grid_sheet("", "justify-items: end; align-items: center")),
            (340.0, 80.0)
        );
        assert_eq!(abs_at(&grid_sheet("", "")), (0.0, 0.0));
    }

    #[test]
    fn grid_inset_pinned_axis_is_untouched() {
        // left: 10px pins x through the inset path (anchored to the CB
        // padding box by the reparent pass); only the auto-inset axis takes
        // the static-position alignment.
        assert_eq!(
            abs_at(&grid_sheet("left: 10px; align-self: center", "")),
            (10.0, 80.0)
        );
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
                han: None,
                clip_src: None,
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
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps, None);
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

    /// #146: Chrome's trailing-`<br>` line-box accounting, pinned against
    /// the headless-Chrome ground-truth matrix measured for the issue. A
    /// forced break ENDS the current line; it contributes a line box iff
    /// the line it ends is empty (block start / consecutive br / after a
    /// block sibling). Trailing break after content: no height. Content
    /// after a break: that content's own line. So `lines = breaks +
    /// (content after the last break ? 1 : 0)`, floored at `breaks`.
    #[test]
    fn br_line_boxes_match_the_chrome_matrix() {
        use crate::diting_layout::{compute_styles, layout_dom};
        fn div_height(body: &str) -> f32 {
            let html = format!(r#"<html><body style="margin:0"><div id="d">{body}</div></body></html>"#);
            let tree = parse_html(&html);
            let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
            let styles = compute_styles(&tree, &rules, (800.0, 600.0));
            let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0);
            rects[&tree.query_selector("#d").unwrap().unwrap()].height
        }
        let line = div_height("a");
        assert!(line > 0.0);
        // Trailing break after content adds NO line (Chrome: 20 == 20).
        assert_eq!(div_height("a<br>"), line, "trailing br must not add a line");
        // Content after the break is the break's own line (Chrome: 40 = 2×20).
        assert_eq!(div_height("a<br>b"), line * 2.0, "one break, two lines");
        // The EMPTY line between two breaks owns a line box (Chrome: 40).
        assert_eq!(div_height("a<br><br>"), line * 2.0, "consecutive brs: the middle empty line exists");
        // A br-only block is one line tall (Chrome: 20) — the rich-text
        // empty-paragraph shape <p><br></p>.
        assert_eq!(div_height("<br>"), line, "br-only block keeps one line box");
        // Collapsible whitespace after the br is not content (Chrome: 20).
        assert_eq!(div_height("<br> "), line, "ws after br does not open a second line");
        // All-break content: every br owns its line (Chrome <br><br> = 40).
        assert_eq!(div_height("<br><br>"), line * 2.0, "two standalone brs stack two empty lines");
        // The empty line's height rides the BR's own inherited line-height
        // (lh:3 → the strut is 3em, not the 1.2 default). Block-level p so
        // the strut stacks in p's own flow instead of hoisting into a run.
        let p3 = div_height(r#"<p style="line-height: 3; margin: 0">x</p>"#);
        let p3_break = div_height(r#"<p style="line-height: 3; margin: 0">x<br><br></p>"#);
        assert!(
            (p3_break - p3 * 2.0).abs() < 1.0,
            "strut follows the br's inherited line-height: break={p3_break} one-line={p3}"
        );
    }

    /// The #146 inverse edge, straight from blitz#939: a `display:none` br
    /// is the classic "hide the trailing spacer" idiom and must break
    /// nothing and add no line.
    #[test]
    fn display_none_br_contributes_no_line() {
        use crate::diting_layout::{compute_styles, layout_dom};
        fn div_height(sheet: &str, body: &str) -> f32 {
            let html = format!(r#"<html><head><style>{sheet}</style></head><body style="margin:0"><div id="d">{body}</div></body></html>"#);
            let tree = parse_html(&html);
            let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
            let styles = compute_styles(&tree, &rules, (800.0, 600.0));
            let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0);
            rects[&tree.query_selector("#d").unwrap().unwrap()].height
        }
        let one = div_height("", "a");
        assert_eq!(div_height("", "a<br style='display:none'>"), one, "hidden br adds no line");
        // A standalone hidden br owns no line box either.
        assert_eq!(div_height("", "<br style='display:none'>"), 0.0, "hidden br-only block is 0-height");
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
            0.0, false, 0.0, true, None, &crate::diting_fonts::font_book(), &mut tt,
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

/// blitz#941 immunity pin: giant-but-finite lengths (3e38px boxes,
/// 3e37 line-heights — their one-attribute 100%-CPU hang) must resolve to
/// FINITE geometry. Our greedy wrap is single-pass over tokens (no parley
/// loop) and css_f32 gates NaN, so what's left to pin is that sums of
/// giant values (height + margin, both 3e38) never overflow to inf.
#[cfg(test)]
mod finite_giant_values_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    #[test]
    fn giant_finite_lengths_resolve_to_finite_geometry() {
        let html = r#"<html><body style="margin:0">
            <p id="p1">a <span style="display:inline-block;height:3e38px;margin-top:3e38px">x</span> b</p>
            <p id="p2" style="line-height:3e37">ab</p>
            <div id="d3"><input id="i3" value="ab" style="line-height:3e37"></div>
        </body></html>"#;
        let tree = parse_html(html);
        let rules = parse_stylesheet_for("", (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (800.0, 600.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        assert!(
            rects.values().all(|r| r.width.is_finite() && r.height.is_finite()),
            "sums of giant values must clamp, not overflow: {:?}",
            rects.values().filter(|r| !r.height.is_finite()).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod anon_cell_stale_key_tests {
    // #119: an anonymous table cell whose members are all inline-level used
    // to unwrap-and-free EVERY kid as if it were a run-wrapper shell. But an
    // out-of-flow replaced atom (the tmall publish page's absolute-positioned
    // textarea) is pushed to `direct` BARE — it owns its node_map entry, so
    // freeing it stranded a stale SlotMap key that panicked the reparent
    // pass's parent() on every layout rebuild. The merge must only unwrap
    // kids that are actually registered run wrappers.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_layout::compute_styles;
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::layout_dom_with_paint_order_and_images;

    fn layout(sheet: &str, body: &str) -> std::collections::HashMap<crate::diting_dom::tree::NodeId, (f32, f32, f32, f32)> {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (1280.0, 720.0));
        let (rects, _, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        rects
            .into_iter()
            .map(|(k, r)| (k, (r.x, r.y, r.width, r.height)))
            .collect()
    }

    // The fixture table is authored with `display:table`/`display:table-row`
    // DIVS on purpose: html5ever foster-parents stray content out of a real
    // `<tr>`, so an HTML-source `<tr>hello<textarea/></tr>` never forms the
    // anonymous cell at all (that is why an earlier draft of these tests
    // stayed green on the buggy code). The tmall tree is built by JS
    // appendChild — no parser rescue — and the div shape reproduces it.
    #[test]
    fn abs_textarea_in_anon_cell_survives_wrapper_merge() {
        // Before the fix this PANICKED in taffy parent() via the reparent
        // pass (invalid SlotMap key). Now the bare atom rides the merged
        // wrapper and the solve completes with the textarea's box present.
        let rects = layout(
            "body { margin: 0 }",
            r#"<div style="display: table"><div style="display: table-row">hello<textarea style="position: absolute; top: 5px; left: 7px; width: 40px; height: 20px"></textarea></div></div>"#,
        );
        let ta: Vec<_> = rects.values().filter(|(_, _, w, h)| *w == 40.0 && *h == 20.0).collect();
        assert_eq!(ta.len(), 1, "textarea box missing from layout: {rects:?}");
        let (x, y, _, _) = ta[0];
        assert!((x - 7.0).abs() <= 1.0 && (y - 5.0).abs() <= 1.0, "abs insets not honored: {x},{y}");
    }

    #[test]
    fn abs_inline_box_in_anon_cell_survives_wrapper_merge() {
        // Same hole for a non-replaced out-of-flow inline: its element box
        // also lands bare in `direct` (the inline-flatten path excludes
        // out-of-flow members).
        let rects = layout(
            "body { margin: 0 }",
            r#"<div style="display: table"><div style="display: table-row">hi<span style="position: absolute; top: 3px; left: 4px; width: 30px; height: 12px"></span></div></div>"#,
        );
        let sp: Vec<_> = rects.values().filter(|(_, _, w, h)| *w == 30.0 && *h == 12.0).collect();
        assert_eq!(sp.len(), 1, "abs inline box missing from layout: {rects:?}");
    }
}

mod inline_block_interior_tests {
    // #166: an inline-block's interior is its own BFC (§9.4.1) — block
    // children stack and width:auto fills the box. The flex-row IFC
    // stand-in used to make them row items: content-sized shrink-to-fit
    // (the tmall cascade columns collapsed to 21px min-content).
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom};

    const VW: f32 = 1280.0;

    fn rect(html: &str, sheet: &str, sel: &str) -> (f32, f32, f32, f32) {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector(sel).unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        (r.x, r.y, r.width, r.height)
    }

    #[test]
    fn block_child_fills_inline_block() {
        // The real-page shape: fixed-width inline-block shell (Fusion
        // Loading), block width:auto inside, percent-width grandchild.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="row">
                <span id="shell" style="display: inline-block; width: 250px"><div id="wrap">
                    <div id="pct" style="width: 100%">仅百分比宽内容</div>
                </div></span>
            </div></body></html>"#,
            "body { margin: 0 } #row { width: 750px }",
            "#wrap",
        );
        assert!((w - 250.0).abs() <= 1.0, "block width:auto must fill the inline-block; got w={w}");
    }

    #[test]
    fn empty_block_child_does_not_collapse_to_zero() {
        // Variant D of the repro: no children at all — the block still
        // fills; the old row mapping measured it to 0.
        let (_x, _y, w, _h) = rect(
            r#"<html><body>
                <span id="shell" style="display: inline-block; width: 250px"><div id="wrap"></div></span>
            </body></html>"#,
            "body { margin: 0 }",
            "#wrap",
        );
        assert!((w - 250.0).abs() <= 1.0, "childless block must not collapse; got w={w}");
    }

    #[test]
    fn block_children_stack_vertically() {
        // Two blocks inside one inline-block stack (BFC block flow), they
        // must not end up side-by-side row items.
        let tree = parse_html(r#"<html><body>
            <span id="shell" style="display: inline-block; width: 250px">
                <div id="a" style="height: 30px">a</div>
                <div id="b" style="height: 30px">b</div>
            </span>
        </body></html>"#);
        let rules = parse_stylesheet_for("body { margin: 0 }", (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let mut got = [None, None];
        for (sel, slot) in [("#a", 0), ("#b", 1)] {
            let id = tree.query_selector(sel).unwrap().unwrap();
            got[slot] = rects.get(&id).map(|r| (r.x, r.y, r.width, r.height));
        }
        let (Some((_ax, ay, aw, _ah)), Some((_bx, by, bw, _bh))) = (got[0], got[1]) else {
            panic!("missing boxes: {got:?}");
        };
        assert!(
            (aw - 250.0).abs() <= 1.0 && (bw - 250.0).abs() <= 1.0 && by >= ay + 29.0,
            "siblings stack and fill; got a(y={ay},w={aw}) b(y={by},w={bw})"
        );
    }

    #[test]
    fn auto_inline_block_still_shrinks_to_fit_in_run() {
        // Regression guard for the OUTER side (#166 fix must not un-atomic
        // the box): an auto-width inline-block in a text run keeps its
        // shrink-to-fit content width instead of filling the line.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="row">word <span id="atom" style="display: inline-block">短</span></div></body></html>"#,
            "body { margin: 0 } #row { width: 750px }",
            "#atom",
        );
        assert!(w < 60.0, "auto inline-block stays shrink-to-fit; got w={w}");
    }

    #[test]
    fn cascade_columns_all_fill() {
        // The shipped symptom: three fixed-width inline-block columns; the
        // 2nd/3rd collapsed because their content was narrower than the
        // shell (col 1 masked the bug — its preferred width overflowed).
        let sheet = "body { margin: 0 } #row { width: 750px } .col { display: inline-block; width: 250px } .wrap { height: 100px }";
        let html = r#"<html><body><div id="row">
            <span class="col"><div class="wrap" id="c1"><div style="width: 100%">很长很长很长很长很长很长很长很长</div></div></span>
            <span class="col"><div class="wrap" id="c2"><div style="width: 100%">短</div></div></span>
            <span class="col"><div class="wrap" id="c3"><input id="inp" style="width: 100%"></div></span>
        </div></body></html>"#;
        for sel in ["#c1", "#c2", "#c3"] {
            let (_x, _y, w, _h) = rect(html, sheet, sel);
            assert!((w - 250.0).abs() <= 1.0, "{sel} must be 250 wide; got w={w}");
        }
    }
}

mod block_in_inline_tests {
    // #186: an inline element with an in-flow block-level child is
    // blockified whole — the approximation of CSS2.1 §9.2.1.1's anonymous
    // split. Left on the IFC flex-row stand-in the block child became a
    // shrink-to-fit row item: shobserver export pages had ARTICLE inside
    // inline A#source self-measure 1092px inside a 740px .container,
    // clipping the article's right edge off-viewport.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom};

    const VW: f32 = 1280.0;

    fn rect(html: &str, sheet: &str, sel: &str) -> (f32, f32, f32, f32) {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector(sel).unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        (r.x, r.y, r.width, r.height)
    }

    #[test]
    fn block_child_of_inline_fills_container() {
        // The #186 shape: fixed-width container, default-inline anchor
        // wrapper, block article inside. The article must take the
        // container's content width, not its own shrink-to-fit measure.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="container" style="width: 740px">
                <a id="source"><article id="art"><p id="p">很长很长很长很长很长很长很长很长很长很长</p></article></a>
            </div></body></html>"#,
            "body { margin: 0 }",
            "#art",
        );
        assert!((w - 740.0).abs() <= 1.0, "article must fill the 740px container; got w={w}");
    }

    #[test]
    fn nested_inline_chain_propagates() {
        // The split propagates outward through nested inlines (§9.2.1.1):
        // the intermediate inline also blockifies, so the block still fills.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="w" style="width: 600px">
                <span id="outer"><a id="inner"><div id="blk">内容</div></a></span>
            </div></body></html>"#,
            "body { margin: 0 }",
            "#blk",
        );
        assert!((w - 600.0).abs() <= 1.0, "nested inline chain must not shrink the block; got w={w}");
    }

    #[test]
    fn text_only_inline_stays_flattened() {
        // Regression guard: a plain inline with only text keeps the old
        // flatten-into-run behavior — it must NOT become a 750px block.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="row" style="width: 750px">word <span id="s">短文本</span> tail</div></body></html>"#,
            "body { margin: 0 }",
            "#s",
        );
        assert!(w < 200.0, "text-only inline stays inline; got w={w}");
    }

    #[test]
    fn inline_img_link_does_not_trigger() {
        // A replaced child never triggers the split (Chrome keeps inline
        // img atomic in the line): the classic image link stays a run
        // member instead of turning into a full-width block.
        let (_ax, _ay, aw, _ah) = rect(
            r#"<html><body><div id="row" style="width: 750px">go <a id="lnk"><img id="im" style="width: 40px; height: 20px" src="about:blank"></a> next</div></body></html>"#,
            "body { margin: 0 }",
            "#lnk",
        );
        assert!(aw < 100.0, "image link must stay inline; got w={aw}");
        let (_ix, _iy, iw, ih) = rect(
            r#"<html><body><div id="row" style="width: 750px">go <a id="lnk"><img id="im" style="width: 40px; height: 20px" src="about:blank"></a> next</div></body></html>"#,
            "body { margin: 0 }",
            "#im",
        );
        assert!((iw - 40.0).abs() <= 1.0 && (ih - 20.0).abs() <= 1.0, "img keeps its authored box; got {iw}x{ih}");
    }
}

mod text_align_block_children_tests {
    // #169: `text-align: center` aligns the INLINE content only — block-level
    // children still fill the container width (§10.3.3). The flex-column
    // alignment stand-in used to shrink EVERY item to content width: on the
    // 1688 punish page `body, p { text-align: center }` collapsed the whole
    // nc-slider chain (`#nocaptcha` … `.nc_scale`) to 0-width tracks.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom};

    const VW: f32 = 1280.0;

    fn rect(html: &str, sheet: &str, sel: &str) -> (f32, f32, f32, f32) {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector(sel).unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        (r.x, r.y, r.width, r.height)
    }

    #[test]
    fn block_child_fills_centered_body() {
        // The punish-page shape: `body, p { text-align: center }` — a bare
        // block child of body must fill, not shrink to its text.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="track" style="height: 34px">向右滑动验证</div></body></html>"#,
            "body { margin: 0 } body, p { text-align: center }",
            "#track",
        );
        assert!((w - VW).abs() <= 1.0, "block under centered body must fill; got w={w}");
    }

    #[test]
    fn deep_chain_of_blocks_fills() {
        // The actual collapse cascade: three nested auto blocks inside the
        // centered container all fill, so a fixed-width track at the bottom
        // has room (and itself stretches only if width:auto).
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="a"><div id="b"><div id="c" style="height: 8px">x</div></div></div></body></html>"#,
            "body { margin: 0; text-align: center }",
            "#c",
        );
        assert!((w - VW).abs() <= 1.0, "nested auto blocks fill; got w={w}");
    }

    #[test]
    fn run_text_stays_centered() {
        // Alignment of the inline content itself is the promote's whole
        // point — keep it: a short text run in the centered body sits at the
        // horizontal middle, not at the left edge.
        let rules = parse_stylesheet_for("body { text-align: center }", (VW, 800.0), CssMediaType::Screen);
        let tree = parse_html(r#"<html><body><i id="m">.</i></body></html>"#);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector("#m").unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        assert!(
            r.x > VW * 0.4 && r.x < VW * 0.6,
            "inline run still centers; marker x={}",
            r.x
        );
    }

    #[test]
    fn table_child_keeps_shrink_to_fit_center() {
        // Tables under text-align:center shrink-to-fit and center in Chrome —
        // the STRETCH patch must not widen them.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><table id=t><tr><td>仅一行</td></tr></table></body></html>"#,
            "body { margin: 0; text-align: center }",
            "#t",
        );
        assert!(w < VW * 0.5, "table stays shrink-to-fit; got w={w}");
    }
}

mod pseudo_inline_style_tests {
    // #172 (REQ template-pseudo verdict): an inline ::before/::after whose
    // content merges into an adjacent pure-text run used to paint with the
    // RUN's color/size — `.row::before { content: "▸ "; color: accent }`
    // lost its accent entirely (0 accent pixels vs the real-element
    // control). The merge is now conditional on the pseudo's own text style
    // matching the run's; a diverging pseudo becomes its own leaf and keeps
    // its declared style.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom_with_paint_order_and_images, PaintItem};

    const ACCENT: [u8; 4] = [255, 51, 102, 255];

    fn text_items(sheet: &str, body: &str) -> Vec<PaintItem> {
        let html = format!("<html><head><style>{sheet}</style></head><body style=\"margin:0\">{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (800.0, 600.0));
        let (_, items, _, _, _, _) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
    }

    fn runs(items: &[PaintItem]) -> Vec<(String, [u8; 4], f32, f32)> {
        items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Text { text, color, x, y, .. } => Some((text.clone(), *color, *x, *y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn accent_pseudo_before_text_keeps_its_color() {
        let items = text_items(
            "#m::before { content: \"MARK \"; color: rgb(255,51,102) }",
            r#"<div id="m">tail</div>"#,
        );
        let rs = runs(&items);
        let mark = rs.iter().find(|(t, ..)| t.starts_with("MARK"));
        let Some((_, color, _, _)) = mark else {
            panic!("pseudo text not painted: {rs:?}");
        };
        assert_eq!(*color, ACCENT, "diverging pseudo must paint in its own color");
        let tail = rs.iter().find(|(t, ..)| t.contains("tail"));
        assert!(tail.is_some_and(|(_, c, _, _)| *c != ACCENT), "host text keeps the host color: {rs:?}");
    }

    #[test]
    fn diverging_pseudo_shares_the_line_with_the_run() {
        // The unmerged leaf must still sit on the host's first line — the
        // chrome-shape `.q::before "> "` prefix hugs the following text.
        let items = text_items(
            "#m::before { content: \"MARK \"; color: rgb(255,51,102) }",
            r#"<div id="m">tail</div>"#,
        );
        let rs = runs(&items);
        let (mx, my) = rs.iter().find(|(t, ..)| t.starts_with("MARK")).map(|(_, _, x, y)| (*x, *y)).unwrap();
        let (tx, ty) = rs.iter().find(|(t, ..)| t.contains("tail")).map(|(_, _, x, y)| (*x, *y)).unwrap();
        assert!((my - ty).abs() < 1.0, "same line: mark y={my} tail y={ty}");
        assert!(tx >= mx, "prefix precedes the run: mark x={mx} tail x={tx}");
    }

    #[test]
    fn matching_pseudo_still_merges_into_the_run() {
        // No declared text style → inherited → merge stays (one item, host
        // color): the `li:before { content: "• " }` optimization survives.
        let items = text_items(
            "#m::before { content: \"• \" }",
            r#"<div id="m">label</div>"#,
        );
        let rs = runs(&items);
        assert!(
            rs.iter().any(|(t, ..)| t.contains("• ") && t.contains("label")),
            "undeclared pseudo still merges with the run: {rs:?}"
        );
    }

    #[test]
    fn accent_pseudo_after_text_keeps_its_color() {
        let items = text_items(
            "#m::after { content: \" TAIL\"; color: rgb(255,51,102) }",
            r#"<div id="m">head</div>"#,
        );
        let rs = runs(&items);
        assert!(
            rs.iter().any(|(t, c, ..)| t.trim_start().starts_with("TAIL") && *c == ACCENT),
            "::after must keep its accent too: {rs:?}"
        );
    }
}

mod pseudo_empty_box_tests {
    // #172: an EMPTY-content pseudo (content:"" carrying only background/
    // gradient decoration — the overlay idiom) generated a layout leaf but
    // nothing in paint claimed it: no DOM id → the walk's background
    // emission never ran → zero pixels. The leaf is now tagged
    // TextLeaf::Pseudo (host id + side) and paint claims the pseudo's own
    // decoration layers; abspos/fixed position blockifies the display so
    // the undeclared-display overlay shape survives at all.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom_with_paint_order_and_images, paint, PaintItem, Rect};

    fn render(sheet: &str, body: &str) -> (Vec<PaintItem>, paint::Canvas) {
        let html = format!(
            "<html><head><style>{sheet}</style></head><body style=\"margin:0\">{body}</body></html>"
        );
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (800.0, 600.0));
        let (_, items, ..) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        let mut canvas = paint::Canvas::new_transparent(800, 600);
        paint::execute(&items, &crate::diting_fonts::font_book(), &mut canvas);
        (items, canvas)
    }

    fn solid_bgs(items: &[PaintItem]) -> Vec<(Rect, [u8; 4])> {
        items
            .iter()
            .filter_map(|it| match it {
                PaintItem::Bg { rect, color, .. } => Some((*rect, *color)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_block_pseudo_background_paints() {
        // The classic in-flow shape: a display:block empty ::before bar
        // (underline/divider idiom) — layout box existed, paint ignored it.
        let (items, _) = render(
            "#m::before { content: \"\"; display: block; height: 8px; background: rgb(10,20,30) }",
            r#"<div id="m" style="height:40px">x</div>"#,
        );
        let bars = solid_bgs(&items);
        let Some((r, c)) = bars.iter().find(|(r, _)| r.height == 8.0) else {
            panic!("empty block pseudo background not painted: {bars:?}");
        };
        assert_eq!(*c, [10, 20, 30, 255], "bar keeps the pseudo's own color");
        assert!(r.width > 700.0, "block bar fills the container: {r:?}");
    }

    #[test]
    fn absolute_overlay_pseudo_stretches_to_containing_block() {
        // The overlay idiom from the issue: content:"" + position:absolute
        // + inset:0 + gradient, NO display declared — CSS blockifies, the
        // box stretches to the containing block (the positioned .wrap).
        let (items, _) = render(
            ".wrap { position: relative; width: 400px; height: 300px; background: rgb(5,5,5) }\
             .wrap::before { content: \"\"; position: absolute; inset: 0;\
                             background: linear-gradient(rgba(255,0,0,0.5), rgba(0,0,255,0.5)) }",
            r#"<div class="wrap">t</div>"#,
        );
        let grads: Vec<&PaintItem> = items
            .iter()
            .filter(|it| matches!(it, PaintItem::BgGradient { .. }))
            .collect();
        let Some(&PaintItem::BgGradient { rect, .. }) = grads.iter().find(|it| match it {
            PaintItem::BgGradient { rect, .. } => {
                (rect.width - 400.0).abs() < 1.0 && (rect.height - 300.0).abs() < 1.0
            }
            _ => false,
        }) else {
            panic!("absolute overlay gradient not painted: {items:?}");
        };
        assert!(
            (rect.width - 400.0).abs() < 1.0 && (rect.height - 300.0).abs() < 1.0,
            "overlay stretches to the containing block: {rect:?}"
        );
    }

    #[test]
    fn overlay_pseudo_inks_canvas_pixels() {
        // End-to-end through the rasterizer: a solid absolute overlay over
        // half the wrap must ink those pixels (the issue's 0px symptom).
        let (_, canvas) = render(
            ".wrap { position: relative; width: 200px; height: 100px }\
             .wrap::before { content: \"\"; position: absolute; left: 100px; top: 0;\
                             width: 100px; height: 100px; background: rgb(200,0,0) }",
            r#"<div class="wrap"></div>"#,
        );
        let px = |x: usize, y: usize| {
            let i = (y * canvas.width + x) * 4;
            canvas.data[i..i + 4].to_vec()
        };
        assert_eq!(px(150, 50), vec![200, 0, 0, 255], "right half inked by the overlay");
        assert_eq!(px(50, 50), vec![0, 0, 0, 0], "left half untouched");
    }

    #[test]
    fn inline_empty_pseudo_still_paints_nothing() {
        // Chrome-faithful: a truly inline empty pseudo generates an empty
        // inline box — no line box, no ink. The empty-content gate in the
        // inline branch keeps returning false.
        let (items, _) = render(
            "#m::before { content: \"\"; background: rgb(9,9,9) }",
            r#"<div id="m">label</div>"#,
        );
        assert!(
            !items.iter().any(|it| matches!(it, PaintItem::Bg { .. } | PaintItem::BgGradient { .. })),
            "inline empty pseudo paints nothing: {:?}",
            solid_bgs(&items)
        );
    }
}

mod abspos_float_shrink_tests {
    // #176: a shrink-to-fit container (abspos auto-width, a floated box)
    // holding a float + text must size to float + text on one line, not to
    // the float alone. The synthetic float row's flow column carried
    // flex-basis 0 — a zero hypothetical main size — so every intrinsic
    // width read measured the row to the float's width (150 vs Chrome's
    // 188 = float 150 + "hello" 38). The float:left container shape was
    // already correct (it rides the run machinery's own sizing); these
    // tests pin both so the fix can't drift either way.
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;
    use crate::diting_layout::{compute_styles, layout_dom};

    const VW: f32 = 1280.0;

    fn rect(html: &str, sheet: &str, sel: &str) -> (f32, f32, f32, f32) {
        let tree = parse_html(html);
        let rules = parse_stylesheet_for(sheet, (VW, 800.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (VW, 800.0));
        let rects = layout_dom(&tree, &styles, &crate::diting_fonts::font_book(), VW, 800.0);
        let id = tree.query_selector(sel).unwrap().unwrap();
        let r = rects.get(&id).unwrap();
        (r.x, r.y, r.width, r.height)
    }

    const BODY: &str = r#"<html><body><div id="stage"><div id="box" class="fit">
        <div id="f" style="float: left; width: 150px; height: 40px"></div>hello
    </div></div></body></html>"#;

    #[test]
    fn abspos_sizes_float_plus_text() {
        // Chrome: the abs box shrink-to-fits to 188 (float 150 + text 38 on
        // one line). The bug measured the synthetic row to 150 — the flow
        // column's flex-basis 0 contributed nothing to the intrinsic read.
        let (_x, _y, w, _h) = rect(
            BODY,
            "body { margin: 0 } #stage { position: relative; width: 400px; height: 200px }\
             #box { position: absolute; left: 0; top: 0 }",
            "#box",
        );
        assert!(
            w > 160.0 && w < 220.0,
            "abspos shrink-to-fit must cover float + text on one line (~188); got w={w}"
        );
    }

    #[test]
    fn floated_container_keeps_float_plus_text() {
        // The sibling shape that already worked (the floated container rides
        // the run machinery's own shrink-to-fit): same band, pinned so the
        // fix can't regress it.
        let (_x, _y, w, _h) = rect(
            BODY,
            "body { margin: 0 } #stage { width: 400px }\
             #box { float: left }",
            "#box",
        );
        assert!(
            w > 160.0 && w < 220.0,
            "floated shrink-to-fit must cover float + text on one line (~188); got w={w}"
        );
    }

    #[test]
    fn abspos_float_only_stays_float_width() {
        // No text: the shrink-to-fit box is exactly the float — guards
        // against an over-widening fix (e.g. a stray stretch to the stage).
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="stage"><div id="box">
                <div id="f" style="float: left; width: 150px; height: 40px"></div>
            </div></div></body></html>"#,
            "body { margin: 0 } #stage { position: relative; width: 400px; height: 200px }\
             #box { position: absolute; left: 0; top: 0 }",
            "#box",
        );
        assert!(
            (w - 150.0).abs() <= 1.0,
            "float-only shrink-to-fit is the float's width; got w={w}"
        );
    }

    #[test]
    fn static_wide_content_never_squeezes_float() {
        // Definite-width parent (400px): the flow column wraps beside the
        // float; the float NEVER shrinks no matter how wide the column's
        // content is (CSS floats keep their computed width). With the fix's
        // content-based flex-basis on the column this is exactly the
        // regression the float/rail flex-shrink 0 rules hold shut.
        let (_x, _y, w, _h) = rect(
            r#"<html><body><div id="host">
                <div id="f" style="float: left; width: 150px; height: 40px"></div>
                hello world this line is deliberately wide enough to overflow the
                two hundred fifty pixels left beside the float so the flex row
                runs out of free space and shrink would engage if allowed
            </div></body></html>"#,
            "body { margin: 0 } #host { width: 400px }",
            "#f",
        );
        assert!(
            (w - 150.0).abs() <= 0.5,
            "a float keeps its computed width beside wide flow content; got w={w}"
        );
    }
}

/// #185 (DPR): [`paint::scale_items`] folds a device-pixel ratio into the
/// item stream. Page-space geometry scales, text drops its wrap-token memo
/// (the memo only matches unscaled shaping), `SetXf` brackets take the
/// scale on the MATRIX while their local-span items stay raw, and a scaled
/// execute inks the right device pixels.
#[cfg(test)]
mod dpr_scale_items_tests {
    use crate::diting_layout::{paint, PaintItem};
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    fn collect(sheet: &str, body: &str) -> Vec<PaintItem> {
        let html = format!(
            "<html><head><style>{sheet}</style></head><body style=\"margin:0\">{body}</body></html>"
        );
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (200.0, 100.0), CssMediaType::Screen);
        let styles = crate::diting_layout::compute_styles(&tree, &rules, (200.0, 100.0));
        let (_, items, ..) = crate::diting_layout::layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 200.0, 100.0, None, None,
        );
        items
    }

    #[test]
    fn page_space_geometry_scales_and_text_drops_tokens() {
        // A real collected stream: bg box + an underlined run (underline
        // forces the run leaf so it carries the token memo unscaled).
        let items = collect(
            "#b { width: 50px; height: 10px; background: rgb(1,2,3) }\
             p { font-size: 16px; text-decoration: underline; width: 180px }",
            r#"<div id="b"></div><p>一段需要折行的中文文本内容比较长一些</p>"#,
        );
        let scaled = paint::scale_items(&items, 2.0);
        let bg = scaled.iter().find_map(|it| match it {
            PaintItem::Bg { rect, color, .. } if *color == [1, 2, 3, 255] => Some(*rect),
            _ => None,
        });
        let Some(r) = bg else { panic!("bg missing: {scaled:?}") };
        assert_eq!((r.width, r.height), (100.0, 20.0), "box doubles");
        let (_text, fs, lh, wrap, tokens) = scaled
            .iter()
            .find_map(|it| match it {
                PaintItem::Text { text, font_size, line_height, wrap_at, tokens, .. }
                    if text.contains("折行") =>
                {
                    Some((text.clone(), *font_size, *line_height, *wrap_at, tokens.is_some()))
                }
                _ => None,
            })
            .unwrap();
        // The run's own unscaled params, read from the same stream, are the
        // expected values — the test must not hardcode the font metrics.
        let (fs0, lh0, wrap0) = items
            .iter()
            .find_map(|it| match it {
                PaintItem::Text { text, font_size, line_height, wrap_at, .. }
                    if text.contains("折行") =>
                {
                    Some((*font_size, *line_height, *wrap_at))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(fs, fs0 * 2.0, "font_size doubles");
        assert_eq!(lh, lh0 * 2.0, "line_height doubles");
        assert_eq!(wrap, wrap0 * 2.0, "wrap_at doubles");
        assert!(!tokens, "scaled run drops the wrap-token memo");
        // Identity scale is a faithful copy.
        assert_eq!(
            paint::scale_items(&items, 1.0).len(),
            items.len(),
            "identity scale returns the same stream"
        );
    }

    #[test]
    fn setxf_bracket_scales_matrix_not_locals() {
        // Synthetic stream mirroring sticky_tests: a local Bg inside a
        // rotation bracket. S·M means all six matrix components double and
        // the LOCAL geometry stays raw — scaling both would double-apply.
        let items = vec![
            PaintItem::Bg {
                rect: crate::diting_layout::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
                color: [0, 0, 0, 255],
                radius: 0.0,
            },
            PaintItem::SetXf { xf: [2.0, 0.3, 0.0, 2.0, 50.0, 60.0] },
            PaintItem::Bg {
                rect: crate::diting_layout::Rect { x: 10.0, y: 20.0, width: 5.0, height: 5.0 },
                color: [0, 0, 0, 255],
                radius: 0.0,
            },
            PaintItem::ClearXf,
            PaintItem::Bg {
                rect: crate::diting_layout::Rect { x: 30.0, y: 30.0, width: 10.0, height: 10.0 },
                color: [0, 0, 0, 255],
                radius: 0.0,
            },
        ];
        let out = paint::scale_items(&items, 2.0);
        let PaintItem::SetXf { xf } = &out[1] else { panic!("bracket survived") };
        assert_eq!(*xf, [4.0, 0.6, 0.0, 4.0, 100.0, 120.0], "matrix takes the scale");
        let PaintItem::Bg { rect, .. } = &out[2] else { panic!() };
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (10.0, 20.0, 5.0, 5.0), "locals raw");
        let PaintItem::Bg { rect, .. } = &out[4] else { panic!() };
        assert_eq!((rect.x, rect.width), (60.0, 20.0), "after ClearXf page space scales again");
    }

    #[test]
    fn scaled_execute_inks_device_pixels() {
        // End-to-end: a 50px box at x=100 on a 200px page, executed at 2×
        // onto a 400px canvas inks the doubled window and nothing else.
        let items = collect(
            "#b { position: absolute; left: 100px; top: 0; width: 50px; height: 40px; background: rgb(200,0,0) }",
            r#"<div id="b"></div>"#,
        );
        let scaled = paint::scale_items(&items, 2.0);
        let mut canvas = paint::Canvas::new_transparent(400, 200);
        paint::execute(&scaled, &crate::diting_fonts::font_book(), &mut canvas);
        let px = |x: usize, y: usize| {
            let i = (y * canvas.width + x) * 4;
            canvas.data[i..i + 4].to_vec()
        };
        // left:100 × 50 wide → device window [200, 300).
        assert_eq!(px(250, 20), vec![200, 0, 0, 255], "box center at device 2x");
        assert_eq!(px(199, 20), vec![0, 0, 0, 0], "just left of the doubled edge");
        assert_eq!(px(300, 20), vec![0, 0, 0, 0], "just right of it");
    }
}

/// #188-3 (blitz#981): the `direction` property + `dir` attribute face.
/// `direction: rtl` now parses and inherits; `dir` fills the same slot as a
/// presentational hint BELOW author declarations (author `direction: ltr`
/// beats `dir="rtl"`); `dir="auto"` takes HTML's first-strong rule with the
/// control's value for input/textarea; and at consume time a start/undeclared
/// alignment on an rtl block right-aligns its inline content like Chrome.
/// No UAX #9 reordering — same-direction content lands exactly, mixed runs
/// keep logical order.
#[cfg(test)]
mod direction_rtl_tests {
    use crate::diting_layout::*;
    use crate::diting_css::{parse_stylesheet_for, CssMediaType, TextAlign, TextDirection};
    use crate::diting_dom::tree_sink::parse_html;

    fn styles_for(
        sheet: &str,
        body: &str,
    ) -> (
        crate::diting_dom::tree::DomTree,
        std::collections::HashMap<crate::diting_dom::tree::NodeId, crate::diting_css::ComputedStyle>,
    ) {
        let html = format!("<html><body>{body}</body></html>");
        let tree = parse_html(&html);
        let rules = parse_stylesheet_for(sheet, (800.0, 600.0), CssMediaType::Screen);
        let styles = compute_styles(&tree, &rules, (800.0, 600.0));
        (tree, styles)
    }

    #[test]
    fn direction_parses_and_inherits() {
        let (tree, styles) = styles_for(
            "#p { direction: rtl }",
            r#"<div id="p"><div id="kid">text</div></div>"#,
        );
        let p = tree.query_selector("#p").unwrap().unwrap();
        let kid = tree.query_selector("#kid").unwrap().unwrap();
        assert_eq!(styles[&p].direction, Some(TextDirection::Rtl));
        // Inherited down the subtree as Some(Rtl) — never "unset".
        assert_eq!(styles[&kid].direction, Some(TextDirection::Rtl));
        // An ltr declaration parses too, and undeclared stays None (= ltr).
        let (tree, styles) = styles_for(
            "#e { direction: ltr }",
            r#"<div id="e">x</div><div id="u">y</div>"#,
        );
        assert_eq!(styles[&tree.query_selector("#e").unwrap().unwrap()].direction, Some(TextDirection::Ltr));
        assert_eq!(styles[&tree.query_selector("#u").unwrap().unwrap()].direction, None);
    }

    #[test]
    fn dir_attribute_hint_sits_below_author_css() {
        // The hint fills the slot; an author declaration beats it.
        let (tree, styles) = styles_for(
            "#w { direction: ltr }",
            r#"<div id="h" dir="rtl">x</div><div id="w" dir="rtl">y</div>"#,
        );
        assert_eq!(styles[&tree.query_selector("#h").unwrap().unwrap()].direction, Some(TextDirection::Rtl));
        assert_eq!(styles[&tree.query_selector("#w").unwrap().unwrap()].direction, Some(TextDirection::Ltr));
    }

    #[test]
    fn dir_auto_takes_first_strong() {
        // input's value attribute is the candidate text for the control's
        // own dir=auto (HTML §4.10.5); a plain element uses its content.
        let (tree, styles) = styles_for(
            "",
            r#"<input id="ar" dir="auto" value="مرحبا">
               <input id="la" dir="auto" value="hello">
               <div id="t" dir="auto">سلام</div>
               <div id="n" dir="auto">123</div>"#,
        );
        assert_eq!(styles[&tree.query_selector("#ar").unwrap().unwrap()].direction, Some(TextDirection::Rtl));
        assert_eq!(
            styles[&tree.query_selector("#la").unwrap().unwrap()].direction,
            Some(TextDirection::Ltr),
            "first strong Latin resolves ltr (Chrome's computed direction for dir=auto)"
        );
        assert_eq!(styles[&tree.query_selector("#t").unwrap().unwrap()].direction, Some(TextDirection::Rtl));
        assert_eq!(
            styles[&tree.query_selector("#n").unwrap().unwrap()].direction,
            None,
            "no strong character leaves the slot unset (= ltr initial)"
        );
    }

    fn text_x(sheet: &str, body: &str, needle: &str) -> f32 {
        let (tree, styles) = styles_for(sheet, body);
        let (_, items, ..) = layout_dom_with_paint_order_and_images(
            &tree, &styles, &crate::diting_fonts::font_book(), 800.0, 600.0, None, None,
        );
        items
            .iter()
            .find_map(|it| match it {
                PaintItem::Text { text, x, .. } if text.contains(needle) => Some(*x),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no text item for {needle:?}"))
    }

    #[test]
    fn rtl_block_right_aligns_its_runs() {
        // An rtl block with NO text-align right-aligns its inline content —
        // the undeclared initial IS start (css-text-3), and start flips
        // against the element's own base direction, exactly like Chrome.
        let right = text_x("#p { direction: rtl; width: 400px }", r#"<div id="p">hi</div>"#, "hi");
        assert!(right > 300.0, "rtl run must hug the right edge; x={right}");
        let left = text_x("#p { width: 400px }", r#"<div id="p">hi</div>"#, "hi");
        assert!(left < 50.0, "ltr undeclared stays flush left (no-promote fast path); x={left}");
    }

    #[test]
    fn logical_start_end_flip_by_direction() {
        // text-align: end under rtl lands LEFT; start under rtl lands RIGHT.
        // Computed style keeps the logical keyword (Chrome reports start/end
        // verbatim) — resolution happens at consume time only.
        let end_left = text_x(
            "#p { direction: rtl; text-align: end; width: 400px }",
            r#"<div id="p">hi</div>"#, "hi",
        );
        assert!(end_left < 50.0, "end under rtl = physical left; x={end_left}");
        let start_right = text_x(
            "#p { direction: rtl; text-align: start; width: 400px }",
            r#"<div id="p">hi</div>"#, "hi",
        );
        assert!(start_right > 300.0, "start under rtl = physical right; x={start_right}");
        let (tree, styles) = styles_for("#p { text-align: end }", r#"<div id="p">x</div>"#);
        let p = tree.query_selector("#p").unwrap().unwrap();
        assert_eq!(styles[&p].text_align, Some(TextAlign::End), "computed style keeps the logical keyword");
    }

    /// Paint-level: an RTL input anchors its value at the RIGHT edge of the
    /// field — the mirror of the LTR left inset. Ink column positions are
    /// measured on the executed canvas (dark pixels only; the field's
    /// 118-gray border and white fill are excluded by the threshold).
    #[test]
    fn rtl_input_anchors_value_at_right_edge() {
        let ink_span = |dir: &str| -> (f32, f32) {
            let body = format!(r#"<input id="i" dir="{dir}" value="hello" style="width: 150px">"#);
            let html = format!("<html><body style=\"margin:0\">{body}</body></html>");
            let tree = parse_html(&html);
            let rules = parse_stylesheet_for("", (200.0, 100.0), CssMediaType::Screen);
            let styles = compute_styles(&tree, &rules, (200.0, 100.0));
            let (_, items, ..) = layout_dom_with_paint_order_and_images(
                &tree, &styles, &crate::diting_fonts::font_book(), 200.0, 100.0, None, None,
            );
            let mut canvas = paint::Canvas::new_transparent(200, 100);
            paint::execute(&items, &crate::diting_fonts::font_book(), &mut canvas);
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for y in 0..canvas.height {
                for x in 0..canvas.width {
                    let i = (y * canvas.width + x) * 4;
                    let (r, g, b, a) = (canvas.data[i], canvas.data[i + 1], canvas.data[i + 2], canvas.data[i + 3]);
                    if a == 255 && r < 100 && g < 100 && b < 100 {
                        lo = lo.min(x as f32);
                        hi = hi.max(x as f32);
                    }
                }
            }
            assert!(lo <= hi, "no ink at all for dir={dir}");
            (lo, hi)
        };
        let (lo_ltr, _) = ink_span("ltr");
        let (lo_rtl, hi_rtl) = ink_span("rtl");
        assert!(lo_ltr < 15.0, "ltr value hugs the left inset; lo={lo_ltr}");
        assert!(lo_rtl > 60.0, "rtl value must be right-anchored, not left; lo={lo_rtl}");
        assert!(hi_rtl > 120.0, "rtl ink reaches near the right edge; hi={hi_rtl}");
    }
}
