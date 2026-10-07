//! CSS computed style → taffy Style translation (split from the layout
//! root to pay the god-file ratchet): [`to_taffy_style`], the intrinsic
//! sizing-keyword resolver, grid track/area mapping, and the padding
//! carry-over helpers. Everything here is a pure translation of one
//! element's ComputedStyle — no DOM walk, no tree mutation.
use super::*;

/// Map a computed style onto a taffy style. Mirrors upstream `to_taffy_style`
/// for the modeled subset, including the block→flex-column promotion that
/// stands in for text alignment in a block formatting context.
/// `pct_h_resolves` is whether a percent/calc height on this element may
/// resolve (computed by the caller via [`pct_height_resolves`] — this fn has
/// no DOM access). Percent heights are folded to auto per §10.5 when the
/// containing block is content-sized.
pub(super) fn to_taffy_style(style: &ComputedStyle, pct_h_resolves: bool) -> Style {
    let mut s = Style::default();
    let display = style.display.unwrap_or(CssDisplay::Block);
    // A block box (or table cell) with centered/right inline content needs a
    // flex-column stand-in because taffy's native block algorithm has no
    // line alignment (upstream to_taffy_style's promote_for_alignment).
    // Cells join the promote: `td { text-align: center }` is everywhere in
    // HTML-email-era markup. So do inline-blocks: text-align inside them
    // aligns the interior's inline content the same way (§9.4.2).
    // Logical alignment resolves physical here (#188-3, blitz#981) — an rtl
    // block with no text-align right-aligns its inline content like Chrome;
    // ltr pages resolve to Left and keep the no-promote fast path.
    let text_align = style.physical_text_align();
    let promote = matches!(display, CssDisplay::Block | CssDisplay::TableCell | CssDisplay::InlineBlock)
        && matches!(text_align, Some(TextAlign::Center) | Some(TextAlign::Right));
    s.display = match display {
        CssDisplay::Block if promote => Display::Flex,
        CssDisplay::TableCell if promote => Display::Flex,
        CssDisplay::Block | CssDisplay::TableCell => Display::Block,
        CssDisplay::Flex => Display::Flex,
        CssDisplay::Grid => Display::Grid,
        // The inline/IFC stand-in is a wrapping flex row (upstream model).
        CssDisplay::Inline => Display::Flex,
        // An inline-block's INTERIOR is its own block formatting context
        // (§9.4.1: an inline-block establishes an independent BFC): children
        // stack vertically and block width:auto fills the box, same as any
        // block container's children. Routing it through the flex-row IFC
        // stand-in made block children ROW items — content-sized
        // shrink-to-fit instead of fill (#166: Fusion Loading's
        // `.next-loading-wrap` collapsed the cascade columns to min-content,
        // 21px-wide category trees). The box's ATOMIC shrink-to-fit against
        // the parent run lives in the run machinery (RunSeg::Nodes), not
        // here.
        CssDisplay::InlineBlock if promote => Display::Flex,
        CssDisplay::InlineBlock => Display::Block,
        // Table stand-ins (table layout batch): a table is a column of row
        // wrappers, a row a non-wrapping row of cell items; cells already
        // mapped to Block above.
        CssDisplay::Table => Display::Flex,
        CssDisplay::TableRow => Display::Flex,
        CssDisplay::None => Display::None,
    };
    if promote {
        s.flex_direction = FlexDirection::Column;
        s.align_items = match text_align {
            Some(TextAlign::Center) => AlignItems::CENTER,
            Some(TextAlign::Right) => AlignItems::FLEX_END,
            _ => AlignItems::NORMAL,
        };
    } else if display == CssDisplay::Table {
        s.flex_direction = FlexDirection::Column;
        s.align_items = AlignItems::STRETCH;
        s.flex_wrap = FlexWrap::NoWrap;
    } else if display == CssDisplay::TableRow {
        s.flex_direction = FlexDirection::Row;
        s.align_items = AlignItems::STRETCH;
        s.flex_wrap = FlexWrap::NoWrap;
    } else if display == CssDisplay::Inline {
        s.flex_direction = FlexDirection::Row;
        s.flex_wrap = FlexWrap::Wrap;
        s.align_items = AlignItems::FLEX_START;
    }
    // Length mapping (batch 2e): px → resolved length, % → taffy percent
    // (0..1 fraction). taffy's percent semantics match CSS — margins and
    // paddings resolve against the containing-block width, insets per-axis
    // — so percents pass straight through and resolve at layout time.
    let lp = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentage::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentage::percent(p / 100.0),
        // Mixed calc keeps its percent part here; the px part is only
        // repaired back for width/min/max (see the calc repair pass) —
        // padding/border slots keep the percent-only approximation.
        Some(crate::diting_css::Length::Calc { percent, .. }) => {
            LengthPercentage::percent(percent / 100.0)
        }
        // Auto can only reach margins (parser rejects it elsewhere) - a
        // padding/border slot never holds it, but treat it as 0 defensively.
        // Sizing keywords are width-family only: unreachable here, same 0.
        Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) => LengthPercentage::length(0.0),
        None => LengthPercentage::length(0.0),
    };
    // Margin has an auto variant; unset margins are CSS `0`, not auto.
    // `Length::Auto` (margin:auto) maps to taffy's auto — its block
    // algorithm implements the in-flow expansion (§10.3.3 centering) and
    // the abspos resolution (§abs-non-replaced-width).
    let lpa_zero = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentageAuto::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
        // Mixed calc margins keep the percent part; the px part is not
        // repaired in this slot (see the calc repair pass).
        Some(crate::diting_css::Length::Calc { percent, .. }) => {
            LengthPercentageAuto::percent(percent / 100.0)
        }
        Some(crate::diting_css::Length::Auto) => LengthPercentageAuto::auto(),
        // Sizing keywords are width-family only: unreachable in a margin.
        Some(crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) => LengthPercentageAuto::length(0.0),
        None => LengthPercentageAuto::length(0.0),
    };
    // Inset/clamp unset values are CSS `auto`.
    let lpa_auto = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentageAuto::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
        Some(crate::diting_css::Length::Calc { percent, .. }) => {
            LengthPercentageAuto::percent(percent / 100.0)
        }
        Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) => LengthPercentageAuto::auto(),
        None => LengthPercentageAuto::auto(),
    };
    // taffy::geometry::Rect spelled in full — this module's own Rect shadows
    // the prelude name.
    s.margin = taffy::geometry::Rect {
        top: lpa_zero(style.margin.top),
        right: lpa_zero(style.margin.right),
        bottom: lpa_zero(style.margin.bottom),
        left: lpa_zero(style.margin.left),
    };
    s.padding = taffy::geometry::Rect {
        top: lp(style.padding.top),
        right: lp(style.padding.right),
        bottom: lp(style.padding.bottom),
        left: lp(style.padding.left),
    };
    // Border widths (batch 4b): taffy insets the content box by them, like
    // blitz does. A `none`/unset style computes the widths to 0.
    let bline = style.border_style.is_some();
    let (bt, br, bb, bl) = (
        if bline { side_px(style.border_width.top) } else { 0.0 },
        if bline { side_px(style.border_width.right) } else { 0.0 },
        if bline { side_px(style.border_width.bottom) } else { 0.0 },
        if bline { side_px(style.border_width.left) } else { 0.0 },
    );
    s.border = taffy::geometry::Rect {
        top: LengthPercentage::length(bt),
        right: LengthPercentage::length(br),
        bottom: LengthPercentage::length(bb),
        left: LengthPercentage::length(bl),
    };
    // CSS's initial box-sizing is content-box while taffy sizes are
    // border-box; authored `box-sizing: border-box` (the universal `*` reset
    // idiom) measures width/min/max to the border edge instead, so only the
    // content-box case maps authored px over by the padding + border widths.
    // Percent sizes pass through as percent in both modes — "percent +
    // padding px" has no taffy Dimension shape, so a % size keeps its padding
    // inside (border-box behavior) even under authored content-box.
    let border_box = matches!(
        style.box_sizing,
        Some(crate::diting_css::BoxSizing::BorderBox)
    );
    s.size = Size {
        width: match style.width {
            Some(crate::diting_css::Length::Px(w)) => Dimension::length(
                if border_box { w } else { w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br },
            ),
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            // Mixed calc rides in as a percent-only placeholder; the px part
            // is resolved against the real containing block in the post-
            // layout calc repair pass (height keeps the placeholder).
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                Dimension::percent(percent / 100.0)
            }
            // width/min/max: auto is the CSS initial value = unset. Sizing
            // keywords ride in as auto — resolve_sizing_keywords replaces
            // them with the measured intrinsic width right after the node
            // is built (before the global layout pass).
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => auto(),
        },
        height: match style.height {
            Some(crate::diting_css::Length::Px(h)) => Dimension::length(
                if border_box { h } else { h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb },
            ),
            Some(crate::diting_css::Length::Percent(p)) if pct_h_resolves => {
                Dimension::percent(p / 100.0)
            }
            Some(crate::diting_css::Length::Calc { percent, .. }) if pct_h_resolves => {
                Dimension::percent(percent / 100.0)
            }
            // §10.5 fold: a percent height against a content-sized
            // containing block computes to auto. taffy would otherwise
            // resolve it against the viewport-derived available space,
            // pinning e.g. a print deck's `height:100%` at one viewport
            // tall while its un-stacked slides run past it — the deck's own
            // overflow clip then blanks every slide but the first (#65).
            Some(crate::diting_css::Length::Percent(_)) | Some(crate::diting_css::Length::Calc { .. })
                | Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent)
                | None => auto(),
        },
    };

    // --- flex/grid pass-through (batch 2c), mirroring upstream to_taffy_style ---
    if let Some(fd) = style.flex_direction {
        s.flex_direction = match fd {
            CssFlexDirection::Row => FlexDirection::Row,
            CssFlexDirection::RowReverse => FlexDirection::RowReverse,
            CssFlexDirection::Column => FlexDirection::Column,
            CssFlexDirection::ColumnReverse => FlexDirection::ColumnReverse,
        };
    }
    if let Some(fw) = style.flex_wrap {
        s.flex_wrap = match fw {
            FlexWrapMode::NoWrap => FlexWrap::NoWrap,
            FlexWrapMode::Wrap => FlexWrap::Wrap,
        };
    }
    // Real alignment only reaches flex/grid containers; on a block formatting
    // context align-items has no effect (and text_align promotion above is a
    // separate concern, like upstream keeps them).
    if matches!(display, CssDisplay::Flex | CssDisplay::Grid) {
        if let Some(ai) = style.align_items {
            s.align_items = match ai {
                AlignMode::Stretch => AlignItems::STRETCH,
                AlignMode::FlexStart => AlignItems::FLEX_START,
                AlignMode::Center => AlignItems::CENTER,
                AlignMode::FlexEnd => AlignItems::FLEX_END,
            };
        }
        if let Some(jc) = style.justify_content {
            s.justify_content = match jc {
                JustifyMode::FlexStart => JustifyContent::FLEX_START,
                JustifyMode::Center => JustifyContent::CENTER,
                JustifyMode::FlexEnd => JustifyContent::FLEX_END,
                JustifyMode::SpaceBetween => JustifyContent::SPACE_BETWEEN,
                JustifyMode::SpaceAround => JustifyContent::SPACE_AROUND,
                JustifyMode::SpaceEvenly => JustifyContent::SPACE_EVENLY,
            };
        }
        // Cross-axis line/track distribution (blitz#1059). Single-line
        // containers have nothing to distribute; taffy's align-content is a
        // no-op there, so no wrap guard is needed.
        if let Some(ac) = style.align_content {
            s.align_content = match ac {
                AlignContentMode::FlexStart => AlignContent::FLEX_START,
                AlignContentMode::Center => AlignContent::CENTER,
                AlignContentMode::FlexEnd => AlignContent::FLEX_END,
                AlignContentMode::SpaceBetween => AlignContent::SPACE_BETWEEN,
                AlignContentMode::SpaceAround => AlignContent::SPACE_AROUND,
                AlignContentMode::SpaceEvenly => AlignContent::SPACE_EVENLY,
                AlignContentMode::Stretch => AlignContent::STRETCH,
            };
        }
    }
    if let Some(fg) = style.flex_grow {
        s.flex_grow = fg;
    }
    if let Some(fs) = style.flex_shrink {
        s.flex_shrink = fs;
    }
    // #176: a float never shrinks. Floats are reified as items of synthetic
    // flex rows (the float zone machinery); with the flow column's
    // content-based flex-basis, wide flow content would proportionally
    // squeeze the float — CSS floats keep their computed width and the
    // wrapping content adjusts instead. Deliberately overrides an author-set
    // flex-shrink: inside the synthetic row the float semantics win.
    if style.float_side.is_some() {
        s.flex_shrink = 0.0;
    }
    // flex-basis/gap carry %: taffy's percent matches CSS —
    // flex-basis resolves against the container main-axis inner size, gap
    // against the per-axis container size — so it passes through and
    // resolves at layout time, same posture as the slots above.
    if let Some(fb) = style.flex_basis {
        s.flex_basis = match fb {
            // The px carry-over (basis + main-axis padding/border to the
            // border-box measure, #216) lives in fix_flex_basis_carry at the
            // flex PARENT's tail — to_taffy_style only sees the item's own
            // style, and flex-basis measures along the parent's main axis
            // (#220). % parts can't ride (same "no Dimension shape" limit as
            // the size slots), and under authored border-box the basis
            // already measures border-edge.
            crate::diting_css::Length::Px(px) => Dimension::length(px),
            crate::diting_css::Length::Percent(p) => Dimension::percent(p / 100.0),
            crate::diting_css::Length::Calc { percent, .. } => Dimension::percent(percent / 100.0),
            // Gap's grammar never stores auto/sizing keywords here; auto is
            // flex-basis's initial anyway.
            crate::diting_css::Length::Auto
            | crate::diting_css::Length::MinContent
            | crate::diting_css::Length::MaxContent
            | crate::diting_css::Length::FitContent => Dimension::auto(),
        };
    }
    let gap_len = |v: Option<crate::diting_css::Length>| match v {
        None => LengthPercentage::length(0.0),
        Some(crate::diting_css::Length::Px(px)) => LengthPercentage::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentage::percent(p / 100.0),
        Some(crate::diting_css::Length::Calc { percent, .. }) => {
            LengthPercentage::percent(percent / 100.0)
        }
        Some(
            crate::diting_css::Length::Auto
            | crate::diting_css::Length::MinContent
            | crate::diting_css::Length::MaxContent
            | crate::diting_css::Length::FitContent,
        ) => LengthPercentage::length(0.0),
    };
    s.gap = Size {
        width: gap_len(style.column_gap),
        height: gap_len(style.row_gap),
    };
    if display == CssDisplay::Table {
        // `border-collapse: collapse` means shared borders — realized as
        // zero gaps between rows/cells; `separate` (the UA initial) gets
        // Chrome's default 2px border-spacing.
        let gap = match style.border_collapse {
            Some(crate::diting_css::BorderCollapse::Collapse) => 0.0,
            _ => 2.0,
        };
        s.gap = Size {
            width: LengthPercentage::length(gap),
            height: LengthPercentage::length(gap),
        };
    }
    if display == CssDisplay::Grid {
        if let Some(cols) = &style.grid_template_columns {
            s.grid_template_columns = cols.iter().map(|t| to_grid_track(*t)).collect();
        }
        if let Some(rows) = &style.grid_template_rows {
            s.grid_template_rows = rows.iter().map(|t| to_grid_track(*t)).collect();
        }
        if let Some(matrix) = &style.grid_template_areas {
            if let Some(areas) = template_areas_from_matrix(matrix) {
                s.grid_template_areas = Some(areas);
            }
        }
    }
    // Named-area placement: `grid-area: <name>` expands to the area's
    // implicit `<name>-start`/`<name>-end` lines on both axes (CSS §8,
    // exactly the convention taffy's NamedLineResolver implements).
    if let Some(name) = &style.grid_area {
        let start = taffy::style::GridPlacement::NamedLine(format!("{}-start", name), 1);
        let end = taffy::style::GridPlacement::NamedLine(format!("{}-end", name), 1);
        s.grid_row = taffy::geometry::Line { start: start.clone(), end: end.clone() };
        s.grid_column = taffy::geometry::Line { start, end };
    }

    // --- positioning + clamps (batch 2d) ---
    if let Some(p) = style.position {
        s.position = match p {
            // taffy has no static: in-flow boxes are Relative (its default).
            PositionMode::Static | PositionMode::Relative => Position::Relative,
            // Fixed pins to the viewport; the nearest slice stands is the
            // root as containing block (see the reparent pass in layout_dom).
            PositionMode::Absolute | PositionMode::Fixed => Position::Absolute,
            // Sticky lays out exactly in flow — the insets are stick
            // thresholds against the scrollport, NOT relative offsets, so
            // they must never reach taffy as insets. Readers compute the
            // scroll-dependent shift (see sticky_axis_shift).
            PositionMode::Sticky => Position::Relative,
        };
    }
    if style.position != Some(PositionMode::Sticky)
        && (style.top.is_some()
            || style.right.is_some()
            || style.bottom.is_some()
            || style.left.is_some())
    {
        s.inset = taffy::geometry::Rect {
            top: lpa_auto(style.top),
            right: lpa_auto(style.right),
            bottom: lpa_auto(style.bottom),
            left: lpa_auto(style.left),
        };
    }
    // Clamps follow the same box-sizing edge as the main sizes: border-box
    // min/max measure to the border edge (no carry-over), content-box adds
    // padding + border (px only; % passes through in both modes).
    s.min_size = Size {
        width: match style.min_width {
            Some(crate::diting_css::Length::Px(w)) => LengthPercentageAuto::length(
                if border_box { w } else { w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br },
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            // Percent-only placeholder; repaired post-layout like width.
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                LengthPercentageAuto::percent(percent / 100.0)
            }
            // Sizing keywords ride in as auto here too; fit-content replaces
            // this slot with the measured min-content floor at build time.
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
        },
        height: match style.min_height {
            Some(crate::diting_css::Length::Px(h)) => LengthPercentageAuto::length(
                if border_box { h } else { h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb },
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                LengthPercentageAuto::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
        },
    };
    s.max_size = Size {
        width: match style.max_width {
            Some(crate::diting_css::Length::Px(w)) => LengthPercentageAuto::length(
                if border_box { w } else { w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br },
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                LengthPercentageAuto::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
        },
        height: match style.max_height {
            Some(crate::diting_css::Length::Px(h)) => LengthPercentageAuto::length(
                if border_box { h } else { h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb },
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                LengthPercentageAuto::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
        },
    };
    if let Some(ar) = style.aspect_ratio {
        if ar.is_finite() && ar > 0.0 {
            s.aspect_ratio = Some(ar);
        }
    }
    s
}

/// CSS sizing keywords (`width: min-content` / `max-content` /
/// `fit-content`): taffy's Dimension has no intrinsic keyword, so the width
/// resolves at build time with measure passes over the just-built subtree —
/// taffy's own intrinsic sizing produces both widths (the text measure
/// closure already handles AvailableSpace::MinContent/MaxContent).
/// `fit-content` ships as the max-content width with a min-content
/// min-size floor and a 100%-of-containing-block max-size clamp, so taffy
/// applies the CSS `fit-content(stretch, max-content)` clamp inside its own
/// size resolution (indefinite containing block → plain max-content).
pub(super) fn resolve_sizing_keywords(
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node: taffy::tree::NodeId,
    style: &ComputedStyle,
    fonts: &FontBook,
) {
    let Some(crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) = style.width else {
        return;
    };
    // Same measured-leaf dispatch as the root pass: plain compute_layout
    // would zero the TextLeaf runs (no measure fn attached).
    let measured = |taffy_tree: &mut TaffyTree<TextLeaf>, w: AvailableSpace| -> f32 {
        let space = taffy::geometry::Size { width: w, height: AvailableSpace::MaxContent };
        let _ = taffy_tree.compute_layout_with_measure(node, space, |inputs, _id, ctx, style| {
            match ctx {
                Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, han, .. }) => {
                    let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps, *han);
                    measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                }
                Some(TextLeaf::Word { .. }) | Some(TextLeaf::Replaced { .. }) | Some(TextLeaf::Pseudo { .. }) | None => {
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
                }
            }
        });
        taffy_tree.layout(node).map(|l| l.size.width).unwrap_or(0.0)
    };
    let w_min = measured(taffy_tree, AvailableSpace::MinContent);
    let w_max = if matches!(style.width, Some(crate::diting_css::Length::MinContent)) {
        w_min
    } else {
        measured(taffy_tree, AvailableSpace::MaxContent)
    };
    if let Ok(mut st) = taffy_tree.style(node).cloned() {
        st.size.width = Dimension::length(w_max);
        if matches!(style.width, Some(crate::diting_css::Length::FitContent)) {
            st.min_size.width = LengthPercentageAuto::length(w_min);
            st.max_size.width = LengthPercentageAuto::percent(1.0);
        }
        let _ = taffy_tree.set_style(node, st);
    }
}

/// diting_css track → taffy track: `1fr` maps to minmax(auto, 1fr), a px
/// track is fixed, a % track is fixed percent (taffy resolves it against
/// the grid container's content box), `auto` sizes to content.
fn to_grid_track(track: GridTrack) -> taffy::style::GridTemplateComponent<String> {
    use taffy::style::{MaxTrackSizingFunction, MinTrackSizingFunction, TrackSizingFunction};
    match track {
        GridTrack::Fr(f) => {
            let tsf: TrackSizingFunction = fr(f);
            tsf.into()
        }
        GridTrack::Px(px) => {
            let tsf: TrackSizingFunction = length(px);
            tsf.into()
        }
        GridTrack::Percent(p) => {
            let tsf: TrackSizingFunction = percent(p / 100.0);
            tsf.into()
        }
        GridTrack::Auto => TrackSizingFunction::AUTO.into(),
        GridTrack::MinMax { min, max } => {
            // min-side fr is invalid CSS; treat it as auto like the spec's
            // clamping does for the min track sizing function.
            let t_min: MinTrackSizingFunction = match min {
                crate::diting_css::TrackSize::Px(px) => length(px),
                crate::diting_css::TrackSize::Percent(p) => percent(p / 100.0),
                _ => MinTrackSizingFunction::AUTO,
            };
            let t_max: MaxTrackSizingFunction = match max {
                crate::diting_css::TrackSize::Px(px) => length(px),
                crate::diting_css::TrackSize::Percent(p) => percent(p / 100.0),
                crate::diting_css::TrackSize::Fr(f) => fr(f),
                _ => MaxTrackSizingFunction::AUTO,
            };
            let tsf: TrackSizingFunction = minmax(t_min, t_max);
            tsf.into()
        }
    }
}

/// `'nav main' 'nav footer'` cell matrix → taffy `GridTemplateAreas`: each
/// distinct name becomes the bounding rectangle of its cells (CSS requires
/// named areas to be rectangular; `.` cells are null). Taffy's resolver then
/// derives the implicit `<name>-start`/`<name>-end` grid lines that
/// `grid-area: <name>` items reference.
fn template_areas_from_matrix(matrix: &[Vec<String>]) -> Option<taffy::style::GridTemplateAreas<String>> {
    use taffy::style::GridTemplateArea;
    let cols = matrix.first()?.len();
    if cols == 0 || matrix.iter().any(|r| r.len() != cols) {
        return None;
    }
    let mut areas: Vec<GridTemplateArea<String>> = Vec::new();
    for (r, row) in matrix.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell == "." {
                continue;
            }
            match areas.iter_mut().find(|a| a.name == *cell) {
                Some(a) => {
                    a.row_start = a.row_start.min((r + 1) as u16);
                    a.row_end = a.row_end.max((r + 2) as u16);
                    a.column_start = a.column_start.min((c + 1) as u16);
                    a.column_end = a.column_end.max((c + 2) as u16);
                }
                None => areas.push(GridTemplateArea {
                    name: cell.clone(),
                    // GridTemplateArea fields are 1-based LINE indices
                    // (row 1 = before the first track), matching how CSS
                    // numbers grid lines. Feeding 0-based track indices
                    // lands items on line 0, which the spec makes invalid
                    // — placement silently degrades to auto.
                    row_start: (r + 1) as u16,
                    row_end: (r + 2) as u16,
                    column_start: (c + 1) as u16,
                    column_end: (c + 2) as u16,
                }),
            }
        }
    }
    Some(taffy::style::GridTemplateAreas {
        areas,
        row_count: matrix.len() as u16,
        column_count: cols as u16,
    })
}

/// Padding contribution in px for the content-box→border-box carry-over
/// (percent padding contributes nothing addable — see the % note in
/// to_taffy_style).
pub(super) fn side_px(v: Option<crate::diting_css::Length>) -> f32 {
    match v {
        Some(crate::diting_css::Length::Px(px)) => px,
        _ => 0.0,
    }
}

