    use taffy::geometry::Point;
    use taffy::prelude::*;
    use taffy::tree::{LayoutInput, LayoutOutput};

    /// Intrinsic (min-content, max-content) inline size of a text-ish leaf.
    /// The Definite branch itself implements fit-content clamping, the way a
    /// real text engine would: max(min, min(available, max)).
    #[derive(Clone, Copy)]
    struct IntrinsicInlineSize {
        min: f32,
        max: f32,
    }

    /// Natural size of a replaced element (image), width/height.
    #[derive(Clone, Copy)]
    struct ReplacedIntrinsicSize {
        width: f32,
        height: f32,
    }

    fn measure_intrinsic(
        input: LayoutInput,
        _node: NodeId,
        context: Option<&mut IntrinsicInlineSize>,
        _style: &Style,
    ) -> LayoutOutput {
        let intrinsic = context.copied().unwrap_or(IntrinsicInlineSize { min: 0.0, max: 0.0 });
        let width = input.known_dimensions.width.unwrap_or_else(|| match input.available_space.width {
            AvailableSpace::MinContent => intrinsic.min,
            AvailableSpace::MaxContent => intrinsic.max,
            AvailableSpace::Definite(available) => {
                intrinsic.min.max(available.min(intrinsic.max))
            }
        });
        LayoutOutput::from_outer_size(Size {
            width,
            height: input.known_dimensions.height.unwrap_or(10.0),
        })
    }

    fn measure_replaced(
        input: LayoutInput,
        _node: NodeId,
        context: Option<&mut ReplacedIntrinsicSize>,
        _style: &Style,
    ) -> LayoutOutput {
        let intrinsic = context.copied().unwrap_or(ReplacedIntrinsicSize { width: 0.0, height: 0.0 });
        let ratio = intrinsic.width / intrinsic.height;
        let size = match input.known_dimensions {
            Size { width: Some(width), height: Some(height) } => Size { width, height },
            Size { width: Some(width), height: None } => Size { width, height: width / ratio },
            Size { width: None, height: Some(height) } => Size { width: height * ratio, height },
            Size { width: None, height: None } => Size { width: intrinsic.width, height: intrinsic.height },
        };
        LayoutOutput::from_outer_size(size)
    }

    fn inline_margins(left_auto: bool, right_auto: bool) -> Rect<LengthPercentageAuto> {
        Rect {
            left: if left_auto { auto() } else { zero() },
            right: if right_auto { auto() } else { zero() },
            top: zero(),
            bottom: zero(),
        }
    }

    /// Single grid item (fixed 300x200 area) with `aspect_ratio: 2` and an
    /// image-like measure function returning a natural 100x50.
    fn layout_replaced_grid_item(item_style: Style, root_style: Style) -> (Point<f32>, Size<f32>) {
        let mut tree = TaffyTree::new();
        tree.disable_rounding();
        let item_style = Style { aspect_ratio: Some(2.0), ..item_style };
        let item = tree
            .new_leaf_with_context(item_style, ReplacedIntrinsicSize { width: 100.0, height: 50.0 })
            .unwrap();
        let root_style = Style {
            display: Display::Grid,
            size: Size { width: length(300.0), height: length(200.0) },
            grid_template_columns: vec![length(300.0)],
            grid_template_rows: vec![length(200.0)],
            ..root_style
        };
        let root = tree.new_with_children(root_style, &[item]).unwrap();
        tree.compute_layout_with_measure(root, Size::MAX_CONTENT, measure_replaced).unwrap();
        let layout = tree.layout(item).unwrap();
        (layout.location, layout.size)
    }

    /// Grid-in-grid: outer 300px fixed track, inner item with `fr(1.0)` track
    /// holding a text leaf of the given intrinsic size. Returns the inner
    /// item's resolved width.
    fn layout_nested_grid_item(
        intrinsic: IntrinsicInlineSize,
        margin: Rect<LengthPercentageAuto>,
        justify_self: Option<AlignSelf>,
        min_width: LengthPercentageAuto,
        max_width: LengthPercentageAuto,
        padding: Rect<LengthPercentage>,
        border: Rect<LengthPercentage>,
        box_sizing: BoxSizing,
    ) -> f32 {
        let mut tree = TaffyTree::new();
        tree.disable_rounding();

        let text = tree.new_leaf_with_context(Style::default(), intrinsic).unwrap();
        let item = tree
            .new_with_children(
                Style {
                    display: Display::Grid,
                    grid_template_columns: vec![fr(1.0)],
                    margin,
                    justify_self,
                    min_size: Size { width: min_width, height: auto() },
                    max_size: Size { width: max_width, height: auto() },
                    padding,
                    border,
                    box_sizing,
                    ..Style::default()
                },
                &[text],
            )
            .unwrap();
        let root = tree
            .new_with_children(
                Style {
                    display: Display::Grid,
                    size: Size { width: length(300.0), height: auto() },
                    grid_template_columns: vec![length(300.0)],
                    ..Style::default()
                },
                &[item],
            )
            .unwrap();

        tree.compute_layout_with_measure(root, Size::MAX_CONTENT, measure_intrinsic).unwrap();
        tree.layout(item).unwrap().size.width
    }

    fn unconstrained_item_width(
        intrinsic: IntrinsicInlineSize,
        margin: Rect<LengthPercentageAuto>,
        justify_self: Option<AlignSelf>,
    ) -> f32 {
        layout_nested_grid_item(
            intrinsic,
            margin,
            justify_self,
            auto(),
            auto(),
            Rect::zero(),
            Rect::zero(),
            BoxSizing::BorderBox,
        )
    }

    // ── Theme 4: `normal` alignment provenance ─────────────────────────────

    /// upstream fork expects the replaced item at its natural 100x50 (normal
    /// resolves to start for compressible replaced elements) while the
    /// ordinary item stretches to 300x150 (normal + aspect-ratio matrix).
    /// Stock 0.13.0 has no `normal` keyword: the legacy default logic treats
    /// both alike.
    #[test]
    fn grid_normal_alignment_replaced_vs_ordinary_aspect_ratio() {
        let (_, replaced) = layout_replaced_grid_item(
            Style { item_is_replaced: true, ..Style::default() },
            Style::default(),
        );
        // fork: Size { width: 100.0, height: 50.0 } — natural size for a
        // compressible replaced element. Stock stretches the inline axis
        // (300) and ratio-derives the block axis (150): the replaced-ness
        // distinction does not exist without the `normal` keyword.
        assert_eq!(replaced, Size { width: 300.0, height: 150.0 });

        let (_, ordinary) = layout_replaced_grid_item(Style::default(), Style::default());
        // fork: Size { width: 300.0, height: 150.0 } (agrees)
        assert_eq!(ordinary, Size { width: 300.0, height: 150.0 });
    }

    /// The fork's `resolve_item_alignment` matrix (8 cases). Case-by-case
    /// fork expectations are in the table; divergent stock results are locked
    /// here. Tuple is (justify_self, align_self, root align_items,
    /// root justify_items).
    #[test]
    fn grid_ordinary_aspect_ratio_alignment_matrix() {
        // Each entry's Size is the STOCK 0.13.0 result (locked); the comment
        // above it is the fork's expectation where it differs.
        let cases: [(Option<AlignSelf>, Option<AlignSelf>, Option<AlignItems>, Option<AlignItems>, Size<f32>); 8] = [
            // fork: 300x150 (agrees) — both normal: inline stretch, block start
            (None, None, None, None, Size { width: 300.0, height: 150.0 }),
            // fork: 400x200 — explicit block stretch drives ratio-derived width;
            // stock ignores the block stretch (no normal provenance)
            (None, Some(AlignSelf::STRETCH), None, None, Size { width: 300.0, height: 150.0 }),
            // fork: 300x150 (agrees)
            (Some(AlignSelf::STRETCH), None, None, None, Size { width: 300.0, height: 150.0 }),
            // fork: 300x200 — both axes definite, ratio must not overwrite;
            // stock re-derives height 150 from the ratio anyway
            (
                Some(AlignSelf::STRETCH),
                Some(AlignSelf::STRETCH),
                None,
                None,
                Size { width: 300.0, height: 150.0 },
            ),
            // fork: 300x150 (agrees)
            (None, Some(AlignSelf::START), None, None, Size { width: 300.0, height: 150.0 }),
            // fork: 400x200 — inline start frees block normal to stretch;
            // stock collapses to the natural 100x50 instead
            (Some(AlignSelf::START), None, None, None, Size { width: 100.0, height: 50.0 }),
            // fork: 400x200 — container-level block stretch; stock ignores it
            (None, None, Some(AlignItems::STRETCH), None, Size { width: 300.0, height: 150.0 }),
            // fork: 300x150 (agrees)
            (None, None, None, Some(AlignItems::STRETCH), Size { width: 300.0, height: 150.0 }),
        ];
        for (justify_self, align_self, align_items, justify_items, expected) in cases {
            let (_, actual) = layout_replaced_grid_item(
                Style { justify_self, align_self, ..Style::default() },
                Style { align_items, justify_items, ..Style::default() },
            );
            assert_eq!(actual, expected, "justify={justify_self:?}, align={align_self:?}");
        }
    }

    // ── Theme 3: aspect-ratio transfer vs stretch ordering ─────────────────

    /// Replaced item, explicit stretch in one axis, ratio 2.0, natural
    /// 100x50. Fork expectations: inline-stretch 300x150, block-stretch
    /// 400x200, both-stretch 300x200, definite inline 120x200, definite
    /// block 300x80. Stock results locked below.
    #[test]
    fn grid_replaced_explicit_stretch_and_definite_sizes() {
        let base = Style { item_is_replaced: true, ..Style::default() };

        let (_, inline_stretch) =
            layout_replaced_grid_item(Style { justify_self: Some(AlignSelf::STRETCH), ..base.clone() }, Style::default());
        // fork: 300x150 (agrees)
        assert_eq!(inline_stretch, Size { width: 300.0, height: 150.0 });

        let (_, block_stretch) =
            layout_replaced_grid_item(Style { align_self: Some(AlignSelf::STRETCH), ..base.clone() }, Style::default());
        // fork: 400x200 — block stretch supplies height, ratio derives width;
        // stock ignores explicit block stretch for a replaced item
        assert_eq!(block_stretch, Size { width: 300.0, height: 150.0 });

        let (_, both_stretch) = layout_replaced_grid_item(
            Style { justify_self: Some(AlignSelf::STRETCH), align_self: Some(AlignSelf::STRETCH), ..base.clone() },
            Style::default(),
        );
        // fork: 300x200 — both axes definite, ratio never overwrites;
        // stock re-derives height from the ratio regardless
        assert_eq!(both_stretch, Size { width: 300.0, height: 150.0 });

        let (_, definite_inline) = layout_replaced_grid_item(
            Style {
                size: Size { width: length(120.0), height: Dimension::auto() },
                align_self: Some(AlignSelf::STRETCH),
                ..base.clone()
            },
            Style::default(),
        );
        // fork: 120x200 — definite width + block stretch; stock keeps the
        // ratio-derived 60 instead of honoring the stretch height
        assert_eq!(definite_inline, Size { width: 120.0, height: 60.0 });

        let (_, definite_block) = layout_replaced_grid_item(
            Style {
                size: Size { width: Dimension::auto(), height: length(80.0) },
                justify_self: Some(AlignSelf::STRETCH),
                ..base
            },
            Style::default(),
        );
        // fork: 300x80 — inline stretch + definite height; stock derives
        // width 160 from the ratio instead of stretching to 300
        assert_eq!(definite_block, Size { width: 160.0, height: 80.0 });
    }

    // ── Theme 5: grid auto-margin fit-content sizing ───────────────────────

    /// Breakable text (min 100 / max 600) in a 300px track. The fork
    /// fit-contents every auto-margin / non-stretch combination to the 300px
    /// area. Stock 0.13.0 sizes all seven to the raw 600px max-content —
    /// auto margins only disable stretch, nothing clamps the inherent
    /// measurement back to the available area. This is the sharpest single
    /// demonstration of the fork's fit-content fix.
    #[test]
    fn grid_breakable_fit_content_margin_matrix() {
        let breakable = IntrinsicInlineSize { min: 100.0, max: 600.0 };
        let cases = [
            (inline_margins(true, true), None),
            (inline_margins(true, false), None),
            (inline_margins(false, true), None),
            (inline_margins(true, true), Some(AlignSelf::STRETCH)),
            (inline_margins(false, false), Some(AlignSelf::START)),
            (inline_margins(false, false), Some(AlignSelf::CENTER)),
            (inline_margins(false, false), Some(AlignSelf::END)),
        ];
        for (margin, justify_self) in cases {
            // fork: 300.0 in all seven cases
            assert_eq!(unconstrained_item_width(breakable, margin, justify_self), 600.0);
        }
    }

    /// Unbreakable text (min = max = 600) must overflow the 300px track
    /// rather than being squashed — the min-content floor of fit-content.
    /// With neither inline margin auto, stretch stays active and yields 300.
    #[test]
    fn grid_unbreakable_min_content_floor() {
        let unbreakable = IntrinsicInlineSize { min: 600.0, max: 600.0 };
        let cases = [
            (inline_margins(true, true), None),
            (inline_margins(true, false), None),
            (inline_margins(false, true), None),
            (inline_margins(true, true), Some(AlignSelf::STRETCH)),
            (inline_margins(false, false), Some(AlignSelf::START)),
            (inline_margins(false, false), Some(AlignSelf::CENTER)),
            (inline_margins(false, false), Some(AlignSelf::END)),
        ];
        for (margin, justify_self) in cases {
            assert_eq!(unconstrained_item_width(unbreakable, margin, justify_self), 600.0);
        }
        assert_eq!(
            unconstrained_item_width(unbreakable, inline_margins(false, false), None),
            300.0,
            "stretch remains active when neither inline margin is auto"
        );
    }

    /// Author min/max clamps and content-box box-sizing edges compose with
    /// fit-content sizing.
    #[test]
    fn grid_fit_content_author_min_max_box_edges() {
        let breakable = IntrinsicInlineSize { min: 100.0, max: 600.0 };
        let unbreakable = IntrinsicInlineSize { min: 600.0, max: 600.0 };
        let auto_margins = inline_margins(true, true);

        let author_min = layout_nested_grid_item(
            breakable,
            auto_margins,
            None,
            length(350.0),
            auto(),
            Rect::zero(),
            Rect::zero(),
            BoxSizing::BorderBox,
        );
        // fork: 350.0 — fit-content (300) raised to the author minimum;
        // stock never clamps to the area, so the raw 600 stands
        assert_eq!(author_min, 600.0);

        let author_max = layout_nested_grid_item(
            unbreakable,
            auto_margins,
            None,
            auto(),
            length(250.0),
            Rect::zero(),
            Rect::zero(),
            BoxSizing::BorderBox,
        );
        assert_eq!(author_max, 250.0);

        let edges = Rect { left: length(10.0), right: length(10.0), top: zero(), bottom: zero() };
        let content_box_max = layout_nested_grid_item(
            unbreakable,
            auto_margins,
            None,
            auto(),
            length(250.0),
            edges,
            edges,
            BoxSizing::ContentBox,
        );
        // fork: 290.0 (250 content + 2*10 padding + 2*10 border)
        assert_eq!(content_box_max, 290.0);
    }

    // ── Theme 3 (flex side): final-main-size ratio transfer ────────────────

    /// A flex item with `aspect-ratio: 2` and auto cross size must get its
    /// cross size from the FINAL resolved main size (Flexbox 9.4), not from
    /// a pre-flex provisional transfer and not from the measure function.
    /// The measure here deliberately ignores the ratio (fixed 10px height) so
    /// the only way to 150px is taffy's own transfer.
    #[test]
    fn flex_auto_cross_uses_final_main_ratio_transfer() {
        #[derive(Clone, Copy)]
        struct Flat;
        fn measure_flat(
            input: LayoutInput,
            _node: NodeId,
            _ctx: Option<&mut Flat>,
            _style: &Style,
        ) -> LayoutOutput {
            LayoutOutput::from_outer_size(Size {
                width: input.known_dimensions.width.unwrap_or(10.0),
                height: input.known_dimensions.height.unwrap_or(10.0),
            })
        }

        let mut tree = TaffyTree::new();
        tree.disable_rounding();
        let item = tree
            .new_leaf_with_context(
                Style { aspect_ratio: Some(2.0), flex_grow: 1.0, ..Style::default() },
                Flat,
            )
            .unwrap();
        let root = tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    size: Size { width: length(300.0), height: Dimension::auto() },
                    ..Style::default()
                },
                &[item],
            )
            .unwrap();
        tree.compute_layout_with_measure(root, Size::MAX_CONTENT, measure_flat).unwrap();
        let size = tree.layout(item).unwrap().size;
        // fork: width 300 (flex-grow fills), height 150 (ratio transfer from
        // the final main size). Stock 0.13.0 never transfers: the flat
        // measure's 10px stands.
        assert_eq!(size, Size { width: 300.0, height: 10.0 });
    }

/// Strut on flattened inlines (obscura#722 residual, our side): the union
/// pass rebuilds a rect from hoisted kids, and that union must grow to the
/// element's own effective line-height — a smaller-font descendant (or a
/// replaced-only inline) otherwise reports a box shorter than the line box
/// Chrome hands out, and click targeting by rect lands short. Structural
/// assertion: no Chrome oracle, we lock our own contract.
#[test]
fn flattened_inline_union_carries_strut_height() {
    use crate::diting_css::{parse_stylesheet_for, CssMediaType};
    use crate::diting_dom::tree_sink::parse_html;

    let html = r#"<html><body><p>before <a style="line-height:32px"><span style="font-size:8px">x</span></a> after</p></body></html>"#;
    let tree = parse_html(html);
    let rules = parse_stylesheet_for("", (1280.0, 800.0), CssMediaType::Screen);
    let styles = crate::diting_layout::compute_styles(&tree, &rules);
    let rects = crate::diting_layout::layout_dom(
        &tree, &styles, &crate::diting_fonts::font_book(), 1280.0, 800.0,
    );

    let a_id = tree.query_selector_all("a").unwrap()[0];
    let r = rects.get(&a_id).expect("flattened <a> must own a union rect");
    assert!(
        r.height >= 31.0,
        "own line-height 32px must strut the union; got {r:?}"
    );
    assert!(r.height <= 40.0, "strut must not overshoot the line box; got {r:?}");
    assert!(r.width > 0.0, "text content must give the inline width");

    // Regression guard: a plain text inline keeps a sane line box (no strut
    // needed — its word leaves already carry the line height).
    let html2 = r#"<html><body><p>plain <b>bold word</b> tail</p></body></html>"#;
    let tree2 = parse_html(html2);
    let styles2 = crate::diting_layout::compute_styles(&tree2, &rules);
    let rects2 = crate::diting_layout::layout_dom(
        &tree2, &styles2, &crate::diting_fonts::font_book(), 1280.0, 800.0,
    );
    let b_id = tree2.query_selector_all("b").unwrap()[0];
    let rb = rects2.get(&b_id).expect("flattened <b> must own a union rect");
    let (_, _, lh) = super::font_context(&tree2, b_id, &styles2);
    assert!(
        rb.height >= lh - 1.0 && rb.height <= lh + 8.0,
        "text inline should sit at its line height {lh}; got {rb:?}"
    );
}

/// Ground truth for the named-area feed (the Vector 2022 page scaffold
/// shape): a GridTemplateAreas matrix on the container plus
/// `<name>-start`/`<name>-end` NamedLine placements on the items must lay
/// each item into its named rectangle — nav in the fixed first column, main
/// beside it in the minmax(0, 1fr) remainder, footer spanning both columns
/// below. If stock taffy (the same rev the bridge pins) ever disagrees with
/// how diting_layout feeds it, this is where the divergence surfaces first.
#[test]
fn taffy_named_grid_area_placement() {
    use taffy::style::{GridPlacement, GridTemplateArea, GridTemplateAreas};

    let area = |name: &str, rs: u16, re: u16, cs: u16, ce: u16| GridTemplateArea {
        name: name.into(),
        row_start: rs,
        row_end: re,
        column_start: cs,
        column_end: ce,
    };
    // Each axis spans the area's implicit lines: start → `<n>-start`,
    // end → `<n>-end` (CSS §8; taffy's NamedLineResolver implements the same
    // convention).
    let place = |name: &str| Style {
        grid_row: taffy::geometry::Line {
            start: GridPlacement::NamedLine(format!("{}-start", name), 1),
            end: GridPlacement::NamedLine(format!("{}-end", name), 1),
        },
        grid_column: taffy::geometry::Line {
            start: GridPlacement::NamedLine(format!("{}-start", name), 1),
            end: GridPlacement::NamedLine(format!("{}-end", name), 1),
        },
        ..Default::default()
    };

    let mut tree: TaffyTree<()> = TaffyTree::new();
    tree.disable_rounding();
    let nav = tree.new_leaf(place("nav")).unwrap();
    let main = tree.new_leaf(place("main")).unwrap();
    let ft = tree.new_leaf(place("ft")).unwrap();
    let root = tree
        .new_with_children(
            Style {
                display: Display::Grid,
                size: Size { width: length(320.0), height: auto() },
                grid_template_columns: vec![length(100.0), minmax(length(0.0), fr(1.0))],
                grid_template_areas: Some(GridTemplateAreas {
                    areas: vec![
                        area("nav", 1, 3, 1, 2),
                        area("main", 1, 3, 2, 3),
                        area("ft", 3, 4, 1, 3),
                    ],
                    row_count: 3,
                    column_count: 2,
                }),
                ..Default::default()
            },
            &[nav, main, ft],
        )
        .unwrap();
    tree.compute_layout(root, Size { width: AvailableSpace::Definite(320.0), height: AvailableSpace::MaxContent }).unwrap();

    let nav_layout = tree.layout(nav).unwrap();
    let main_layout = tree.layout(main).unwrap();
    let ft_layout = tree.layout(ft).unwrap();
    assert_eq!(nav_layout.size.width, 100.0, "nav fills the fixed first column");
    assert_eq!(main_layout.location.x, 100.0, "main starts at the second column");
    assert_eq!(main_layout.size.width, 220.0, "main takes the minmax(0, 1fr) remainder");
    assert_eq!(main_layout.location.y, nav_layout.location.y, "nav and main share the first row");
    assert_eq!(ft_layout.size.width, 320.0, "footer spans both columns");
    assert_eq!(ft_layout.location.y, nav_layout.location.y + nav_layout.size.height, "footer below nav");
}

/// Vector 2022's `.vector-column-start` wears real skin margins (margin-top
/// 2.85rem, margin-left -0.75rem) inside a fixed 12.25rem first track. The
/// margin box must still stretch across the track, leaving the content box
/// WIDER than the track (196 + 12), never zero.
#[test]
fn taffy_grid_item_negative_margin_still_stretches() {
    use taffy::style::{GridPlacement, GridTemplateArea, GridTemplateAreas};

    let place = |name: &str| Style {
        grid_row: taffy::geometry::Line {
            start: GridPlacement::NamedLine(format!("{}-start", name), 1),
            end: GridPlacement::NamedLine(format!("{}-end", name), 1),
        },
        grid_column: taffy::geometry::Line {
            start: GridPlacement::NamedLine(format!("{}-start", name), 1),
            end: GridPlacement::NamedLine(format!("{}-end", name), 1),
        },
        ..Default::default()
    };
    let mut tree: TaffyTree<()> = TaffyTree::new();
    tree.disable_rounding();
    let toc = tree
        .new_leaf(Style {
            margin: taffy::geometry::Rect {
                top: LengthPercentageAuto::length(45.6),
                right: LengthPercentageAuto::length(0.0),
                bottom: LengthPercentageAuto::length(0.0),
                left: LengthPercentageAuto::length(-12.0),
            },
            ..place("columnStart")
        })
        .unwrap();
    let content = tree.new_leaf(place("pageContent")).unwrap();
    let notice = tree.new_leaf(place("siteNotice")).unwrap();
    let footer = tree.new_leaf(place("footer")).unwrap();
    let root = tree
        .new_with_children(
            Style {
                display: Display::Grid,
                gap: Size { width: length(24.0), height: length(0.0) },
                size: Size { width: length(1352.0), height: auto() },
                grid_template_columns: vec![length(196.0), minmax(length(0.0), fr(1.0))],
                grid_template_rows: vec![min_content(), fr(1.0), min_content()],
                grid_template_areas: Some(GridTemplateAreas {
                    areas: vec![
                        GridTemplateArea { name: "siteNotice".into(), row_start: 1, row_end: 2, column_start: 1, column_end: 3 },
                        GridTemplateArea { name: "columnStart".into(), row_start: 2, row_end: 3, column_start: 1, column_end: 2 },
                        GridTemplateArea { name: "pageContent".into(), row_start: 2, row_end: 3, column_start: 2, column_end: 3 },
                        GridTemplateArea { name: "footer".into(), row_start: 3, row_end: 4, column_start: 1, column_end: 3 },
                    ],
                    row_count: 3,
                    column_count: 2,
                }),
                ..Default::default()
            },
            &[notice, toc, content, footer],
        )
        .unwrap();
    tree.compute_layout(root, Size { width: AvailableSpace::Definite(1352.0), height: AvailableSpace::MaxContent }).unwrap();

    let toc_l = tree.layout(toc).unwrap();
    let content_l = tree.layout(content).unwrap();
    println!("toc: x={} y={} w={} h={}", toc_l.location.x, toc_l.location.y, toc_l.size.width, toc_l.size.height);
    println!("content: x={} y={} w={} h={}", content_l.location.x, content_l.location.y, content_l.size.width, content_l.size.height);
    assert_eq!(toc_l.size.width, 208.0, "fixed 196 track minus a -12 margin leaves a 208-wide content box");
    assert_eq!(toc_l.location.x, -12.0, "negative margin shifts the box out of the track origin");
    assert_eq!(content_l.size.width, 1132.0, "pageContent takes the fr remainder");
}
