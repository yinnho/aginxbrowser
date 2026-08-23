//! Taffy fork-delta classification (render claim batch 2a).
//!
//! Upstream obscura vendors `taffy 0.12.1 + 11 fork commits` (~1.8k net lines:
//! geometric float clearance, margin-collapse metadata in the measure cache,
//! preferred-aspect-ratio transfer, the `normal` alignment keyword, grid
//! auto-margin fit-content sizing, track distribution limits, intrinsic-size
//! containment, and a calc() resolver injection hook). Our product pipeline
//! pins stock taffy 0.13.0 through blitz, and none of that fork work is in
//! 0.13.0 (verified marker-by-marker; only float `clear_bottoms` partially
//! landed, the segment-index high-water-marks remain).
//!
//! This module ports the fork's own regression scenarios to stock 0.13.0 and
//! locks the observed outputs. Where an assertion differs from the fork's
//! expectation (noted in each comment), that behavior is fork-only: the day
//! stock taffy absorbs the fix, the locked assertion here fails and names
//! exactly what changed. See docs/engine/render.md §11 for the full
//! eight-theme classification.

#[cfg(test)]
mod fork_deltas {
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

    /// obscura fork expects the replaced item at its natural 100x50 (normal
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
}
