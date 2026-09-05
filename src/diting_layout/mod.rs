//! Taffy fork-delta classification + minimal DOM→taffy layout bridge
//! (render claim batch 2a / 2b).
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
//!
//! The bridge below (batch 2b) is the minimal vertical slice of upstream
//! dom.rs's DOM→taffy mapping: display roles, box-model px, text-align
//! promotion, and deterministic word-leaf text. Not modeled yet (upstream
//! has, we absorb in later batches): float/table/multicol, replaced elements,
//! position:absolute, em/rem/% lengths, flex/grid property pass-through.
//! Same engine both sides of the cross-check (stock taffy 0.13.0), so the
//! rect comparison isolates the BRIDGE, not the layout algorithm.

use std::collections::HashMap;

use taffy::prelude::*;

use crate::diting_css::{
    AlignMode, ComputedStyle, Display as CssDisplay, FlexDirection as CssFlexDirection,
    FlexWrapMode, GridTrack, JustifyMode, ObjectFit, ObjectPositionPart, Overflow, PositionMode,
    TextAlign,
};
use crate::diting_dom::tree::{DomTree, NodeId};

pub mod image;
pub mod paint;
pub mod text;
pub use image::DecodedImage;
pub use text::FontBook;

/// Absolute (page-relative) border-box rect of a DOM element after layout.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// CJK classification for the tokenizer (per-glyph line breaks) — kept from
/// the pre-3a deterministic metrics era; `is_cjk` no longer feeds widths.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x9FFF      // CJK Radicals .. CJK Unified
        | 0xA960..=0xA97F      // Hangul Extended-A
        | 0xAC00..=0xD7FF      // Hangul Syllables + Compatibility
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0xFE30..=0xFE4F      // CJK Compatibility Forms
        | 0xFF00..=0xFF60      // Fullwidth Forms
        | 0xFFE0..=0xFFE6      // Fullwidth Signs
    )
}

/// Node context for measured taffy leaves (batch 3a). A pure-text run is ONE
/// leaf measured the way blitz measures its parley text nodes — see
/// [`measure_text_leaf`]; mixed runs (text + inline elements) keep the
/// flex-row-of-word-leaves fallback from batch 2b.
#[derive(Clone)]
enum TextLeaf {
    Run { text: String, font_size: f32, bold: bool, color: [u8; 4], line_height: f32 },
    /// One word/glyph of a MIXED run (text around inline elements): the
    /// batch-2b word-leaf fallback, now carrying paint context (batch 4d).
    /// Layout is still style-driven — the measure closure passes Word
    /// leaves straight through to taffy's own style sizing.
    Word { text: String, font_size: f32, bold: bool, color: [u8; 4], line_height: f32 },
}

/// Approximate used line height for text leaves. Matches blitz exactly:
/// blitz-dom maps CSS `line-height: normal` to `font_size * 1.2`
/// (src/layout/mod.rs:76) rather than deriving from font metrics, and the
/// cross-check asserts text-derived heights against it.
fn line_height(font_size: f32) -> f32 {
    font_size * 1.2
}

/// Used line height for a text run: the element's declared `line-height`
/// (unitless multiplier against its own font-size, or absolute px) with
/// `normal`/unset falling back to the same `font_size * 1.2` blitz pins.
fn effective_line_height(spec: Option<&crate::diting_css::LineHeightSpec>, font_size: f32) -> f32 {
    match spec {
        Some(crate::diting_css::LineHeightSpec::Number(n)) => font_size * n,
        Some(crate::diting_css::LineHeightSpec::Px(px)) => *px,
        Some(crate::diting_css::LineHeightSpec::Normal) | None => line_height(font_size),
    }
}

/// Map a computed style onto a taffy style. Mirrors upstream `to_taffy_style`
/// for the modeled subset, including the block→flex-column promotion that
/// stands in for text alignment in a block formatting context.
fn to_taffy_style(style: &ComputedStyle) -> Style {
    let mut s = Style::default();
    let display = style.display.unwrap_or(CssDisplay::Block);
    // A block box with centered/right inline content needs a flex-column
    // stand-in because taffy's native block algorithm has no line alignment
    // (upstream to_taffy_style's promote_for_alignment).
    let promote = display == CssDisplay::Block
        && matches!(style.text_align, Some(TextAlign::Center) | Some(TextAlign::Right));
    s.display = match display {
        CssDisplay::Block if promote => Display::Flex,
        CssDisplay::Block => Display::Block,
        CssDisplay::Flex => Display::Flex,
        CssDisplay::Grid => Display::Grid,
        // The inline/IFC stand-in is a wrapping flex row (upstream model).
        CssDisplay::Inline | CssDisplay::InlineBlock => Display::Flex,
        CssDisplay::None => Display::None,
    };
    if promote {
        s.flex_direction = FlexDirection::Column;
        s.align_items = match style.text_align {
            Some(TextAlign::Center) => Some(AlignItems::CENTER),
            Some(TextAlign::Right) => Some(AlignItems::FLEX_END),
            _ => None,
        };
    } else if display == CssDisplay::Inline || display == CssDisplay::InlineBlock {
        s.flex_direction = FlexDirection::Row;
        s.flex_wrap = FlexWrap::Wrap;
        s.align_items = Some(AlignItems::FLEX_START);
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
    // `Length::Auto` (margin:auto) maps to taffy's auto - taffy's block
    // algorithm implements both the in-flow horizontal auto-margin
    // expansion (CSS §10.3.3 centering) and the abspos auto-margin
    // resolution (§abs-non-replaced-width).
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
    // border-box; the subset has no authored box-sizing yet, so map authored
    // sizes over by the padding + border widths.
    // Percent sizes pass through as percent — "percent + padding px" has no
    // taffy Dimension shape, so a % size keeps its padding inside (border-box
    // behavior); authored box-sizing is a later batch anyway.
    s.size = Size {
        width: match style.width {
            Some(crate::diting_css::Length::Px(w)) => Dimension::length(
                w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br,
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
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
            ),
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                Dimension::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => auto(),
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
            s.align_items = Some(match ai {
                AlignMode::Stretch => AlignItems::STRETCH,
                AlignMode::FlexStart => AlignItems::FLEX_START,
                AlignMode::Center => AlignItems::CENTER,
                AlignMode::FlexEnd => AlignItems::FLEX_END,
            });
        }
        if let Some(jc) = style.justify_content {
            s.justify_content = Some(match jc {
                JustifyMode::FlexStart => JustifyContent::FLEX_START,
                JustifyMode::Center => JustifyContent::CENTER,
                JustifyMode::FlexEnd => JustifyContent::FLEX_END,
                JustifyMode::SpaceBetween => JustifyContent::SPACE_BETWEEN,
                JustifyMode::SpaceAround => JustifyContent::SPACE_AROUND,
                JustifyMode::SpaceEvenly => JustifyContent::SPACE_EVENLY,
            });
        }
    }
    if let Some(fg) = style.flex_grow {
        s.flex_grow = fg;
    }
    if let Some(fs) = style.flex_shrink {
        s.flex_shrink = fs;
    }
    if let Some(fb) = style.flex_basis {
        s.flex_basis = Dimension::length(fb);
    }
    s.gap = Size {
        width: LengthPercentage::length(style.column_gap.unwrap_or(0.0)),
        height: LengthPercentage::length(style.row_gap.unwrap_or(0.0)),
    };
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
        };
    }
    if style.top.is_some() || style.right.is_some() || style.bottom.is_some() || style.left.is_some() {
        s.inset = taffy::geometry::Rect {
            top: lpa_auto(style.top),
            right: lpa_auto(style.right),
            bottom: lpa_auto(style.bottom),
            left: lpa_auto(style.left),
        };
    }
    // Clamps are content-box in CSS's initial box-sizing — same padding +
    // border carry-over as the main sizes above (px only; % passes through).
    s.min_size = Size {
        width: match style.min_width {
            Some(crate::diting_css::Length::Px(w)) => LengthPercentageAuto::length(
                w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br,
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
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
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
                w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br,
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                LengthPercentageAuto::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
        },
        height: match style.max_height {
            Some(crate::diting_css::Length::Px(h)) => LengthPercentageAuto::length(
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
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
fn resolve_sizing_keywords(
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
                Some(TextLeaf::Run { text, font_size, bold, line_height, .. }) => {
                    measure_text_leaf(text, *font_size, *bold, *line_height, fonts, &inputs)
                }
                Some(TextLeaf::Word { .. }) | None => {
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
/// track is fixed, `auto` sizes to content.
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
        GridTrack::Auto => TrackSizingFunction::AUTO.into(),
        GridTrack::MinMax { min, max } => {
            // min-side fr is invalid CSS; treat it as auto like the spec's
            // clamping does for the min track sizing function.
            let t_min: MinTrackSizingFunction = match min {
                crate::diting_css::TrackSize::Px(px) => length(px),
                _ => MinTrackSizingFunction::AUTO,
            };
            let t_max: MaxTrackSizingFunction = match max {
                crate::diting_css::TrackSize::Px(px) => length(px),
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
fn side_px(v: Option<crate::diting_css::Length>) -> f32 {
    match v {
        Some(crate::diting_css::Length::Px(px)) => px,
        _ => 0.0,
    }
}

/// Effective font context for a text leaf: nearest ancestor's font-size /
/// weight / line-height spec (defaults 16px / 400 / normal). Since batch 2e
/// every cascaded element carries a resolved font-size, so the walk stops at
/// the NEAREST value instead of relying on outer ancestors being None.
/// Returns (font_size, bold, used_line_height): a Number spec multiplies the
/// RUN's font-size (CSS computed-value semantics — the multiplier applies to
/// whichever font-size the text actually uses), px passes through absolute.
fn font_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> (f32, bool, f32) {
    let mut font_size: Option<f32> = None;
    let mut bold: Option<bool> = None;
    let mut lh_spec: Option<crate::diting_css::LineHeightSpec> = None;
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(style) = styles.get(&nid) {
            if font_size.is_none() {
                if let Some(fs) = style.font_size {
                    font_size = Some(fs);
                }
            }
            if bold.is_none() {
                if let Some(fw) = style.font_weight {
                    bold = Some(fw >= 600);
                }
            }
            if lh_spec.is_none() {
                lh_spec = style.line_height;
            }
            if font_size.is_some() && bold.is_some() && lh_spec.is_some() {
                break;
            }
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    let fs = font_size.unwrap_or(16.0);
    (fs, bold.unwrap_or(false), effective_line_height(lh_spec.as_ref(), fs))
}

/// Inherited text color for a node — the same nearest-set ancestor walk as
/// [`font_context`] (the cascade inherits `color`, but text NODES carry no
/// ComputedStyle, so the paint side resolves it here). Defaults to black.
fn color_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> [u8; 4] {
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(c) = styles.get(&nid).and_then(|s| s.color) {
            return [c.0, c.1, c.2, c.3];
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    [0, 0, 0, 255]
}

/// Split text into layout tokens. Whitespace runs collapse to a single space
/// token (CSS text processing) that keeps its width but contributes no
/// height; CJK chars break per-glyph — UAX#14 allows a break after every
/// ideograph, and without this a CJK paragraph would be one unbreakable
/// "word".
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            // CSS §16.6.1: a whitespace run collapses to ONE space, not one
            // leaf per char — the newline+indent node between inline
            // siblings used to measure as a dozen-plus spaces.
            if !tokens.last().is_some_and(|t| t.trim().is_empty()) {
                tokens.push(" ".to_string());
            }
        } else if is_cjk(ch) {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            tokens.push(ch.to_string());
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}

/// Adjacent-sibling margin collapse (CSS 2.1 §8.3.1). Two regimes coexist in
/// the bridge:
///
/// - Taffy's native BLOCK layout already collapses sibling margins to max —
///   but unconditionally, WITHOUT the "border/padding on the touching edges
///   separates the boxes" rule (its `has_styles_preventing_being_collapsed_
///   through` only gates a node collapsing through ITSELF, not the pair).
/// - Flex/grid containers SUM margins.
///
/// This pass runs over the built child list and rewrites the pair so BOTH
/// engines land on the CSS geometry: touching edges clean → encode the
/// collapsed max (zeroing prev.bottom, inflating next.top) for flex/grid
/// parents, and leave block parents' native collapse alone; separated edges
/// → encode the SUM (prev.bottom=0, next.top=a+b), which makes taffy's
/// unconditional block-side max a no-op (max(a+b applied once)) while giving
/// flex/grid the correct sum.
///
/// Out of scope (documented approximations): %-margin pairs (taffy resolves
/// them against the CB width at layout time; the max is unknowable at build
/// time — such pairs keep whatever the engine does), collapse-through of
/// empty self-collapsing blocks, and first/last-child collapse with the
/// parent.
fn collapse_adjacent_sibling_margins(
    taffy_tree: &mut TaffyTree<TextLeaf>,
    styles: &HashMap<NodeId, ComputedStyle>,
    node_map: &HashMap<taffy::tree::NodeId, NodeId>,
    direct: &[taffy::tree::NodeId],
) {
    let px = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => Some(px),
        _ => None,
    };
    for pair in direct.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        let (Some(&dprev), Some(&dnext)) = (node_map.get(&prev), node_map.get(&next)) else {
            continue; // run wrapper / synthetic node — not an element pair
        };
        let (Some(sp), Some(sn)) = (styles.get(&dprev), styles.get(&dnext)) else { continue };
        // Both in-flow block-level: flex/grid containers, floats, and
        // positioned boxes never collapse (CSS §8.3.1).
        if !matches!(sp.display, Some(CssDisplay::Block) | None)
            || !matches!(sn.display, Some(CssDisplay::Block) | None)
        {
            continue;
        }
        let out_of_flow = |s: &ComputedStyle| {
            matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        };
        if out_of_flow(sp) || out_of_flow(sn) || sp.float_side.is_some() || sn.float_side.is_some() {
            continue;
        }
        let (Some(mp), Some(mn)) = (px(sp.margin.bottom), px(sn.margin.top)) else {
            continue; // % margins: keep the summed gap (approximation above)
        };
        if mp == 0.0 && mn == 0.0 {
            continue;
        }
        // Border/padding on the touching edges separates the boxes: the
        // margins stop adjoining and SUM instead of collapsing to max
        // (CSS §8.3.1). Either way the pair is re-encoded as
        // [prev.bottom=0 | next.top=gap] — see the doc comment for why
        // both engine regimes land on `gap`.
        let separated = (sp.border_style.is_some() && side_px(sp.border_width.bottom) > 0.0)
            || side_px(sp.padding.bottom) > 0.0
            || (sn.border_style.is_some() && side_px(sn.border_width.top) > 0.0)
            || side_px(sn.padding.top) > 0.0;
        let gap = if separated { mp + mn } else { mp.max(mn) };
        if let (Ok(mut prev_style), Ok(mut next_style)) =
            (taffy_tree.style(prev).cloned(), taffy_tree.style(next).cloned())
        {
            prev_style.margin.bottom = taffy::style::LengthPercentageAuto::length(0.0);
            next_style.margin.top = taffy::style::LengthPercentageAuto::length(gap);
            let _ = taffy_tree.set_style(prev, prev_style);
            let _ = taffy_tree.set_style(next, next_style);
        }
    }
}

/// One taffy leaf per word (upstream's per-word leaf model). Intrinsic size
/// from real shaped advances (batch 3a); wrapping happens in the enclosing
/// flex-wrap run container. Since batch 4d the leaf carries paint context —
/// mixed runs paint their words.
fn build_word_leaves(
    text: &str,
    font_size: f32,
    bold: bool,
    color: [u8; 4],
    line_height: f32,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
) -> Vec<taffy::tree::NodeId> {
    tokenize(text)
        .into_iter()
        .filter_map(|token| {
            let width = fonts.advance_width(&token, font_size, bold);
            // Pure-whitespace tokens contribute no height (they sit between
            // block siblings without adding a spurious blank row).
            let height = if token.trim().is_empty() { 0.0 } else { line_height };
            let style = Style {
                size: Size {
                    width: Dimension::length(width.max(0.0)),
                    height: Dimension::length(height.max(0.0)),
                },
                ..Style::default()
            };
            let leaf = TextLeaf::Word {
                text: token.clone(),
                font_size,
                bold,
                color,
                line_height,
            };
            taffy_tree.new_leaf_with_context(style, leaf).ok()
        })
        .collect()
}

/// CSS §16.6.1: collapsible whitespace at the edges of an inline formatting
/// context contributes no width. The pure-text path already trims edges
/// (measure_text_leaf); the mixed-run path used to keep whitespace-only word
/// leaves, and each tokenized space carried its full advance into
/// shrink-to-fit/max-content widths — newline+indent text nodes between
/// inline siblings added phantom space runs (obscura#764's +15px). Detached
/// leaves are removed from the tree, like the flatten-path cleanup.
fn trim_run_edge_whitespace(
    taffy_tree: &mut TaffyTree<TextLeaf>,
    leaves: &mut Vec<taffy::tree::NodeId>,
) {
    let is_ws = |tree: &TaffyTree<TextLeaf>, node: taffy::tree::NodeId| {
        matches!(
            tree.get_node_context(node),
            Some(TextLeaf::Word { text, .. }) if text.trim().is_empty()
        )
    };
    while leaves.first().copied().is_some_and(|n| is_ws(taffy_tree, n)) {
        let _ = taffy_tree.remove(leaves.remove(0));
    }
    while leaves.last().copied().is_some_and(|n| is_ws(taffy_tree, n)) {
        if let Some(n) = leaves.pop() {
            let _ = taffy_tree.remove(n);
        }
    }
}

/// Taffy measure function for a pure-text run leaf (batch 3a). Reproduces
/// the observable behavior of blitz's parley-measured text nodes:
///
/// - collapsible whitespace at run EDGES contributes nothing (probe:
///   `"hello "` and `" hello"` both measure as `"hello"`, `" "` as zero);
/// - greedy line breaking over exact shaped advances; a space before a
///   break point is dropped (CSS trailing-whitespace removal);
/// - the block's width is `ceil(max line advance)` — parley rounds the text
///   run's size UP so nothing overflows the box, which taffy's own
///   round-to-nearest would not reproduce (probe: "hello" 37.36 → 38);
/// - height = line count × used line-height (blitz pins `normal` at
///   1.2×fs; declared values arrive with the leaf).
fn measure_text_leaf(
    text: &str,
    font_size: f32,
    bold: bool,
    line_height: f32,
    fonts: &FontBook,
    inputs: &taffy::tree::LayoutInput,
) -> taffy::tree::LayoutOutput {
    let known = inputs.known_dimensions;
    let lh = line_height;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return taffy::tree::LayoutOutput::HIDDEN;
    }
    // Token widths (word / single space / per-glyph CJK) with real advances.
    let tokens = text::tokens_of(text, font_size, bold, fonts);
    let widest_token = tokens.iter().map(|t| t.width).fold(0.0, f32::max);

    let wrap_at = match inputs.available_space.width {
        taffy::AvailableSpace::Definite(w) => Some(w),
        _ => None,
    };
    // Greedy wrap over the shared breaker (batch 4a): measure and paint see
    // the same lines by construction.
    let lines = text::greedy_wrap(&tokens, wrap_at);
    let min_content = matches!(inputs.available_space.width, taffy::AvailableSpace::MinContent);
    let max_line = if min_content { widest_token } else { lines.iter().map(|l| l.width).fold(0.0, f32::max) };
    let size = taffy::geometry::Size {
        width: known.width.unwrap_or(max_line.ceil()),
        height: known.height.unwrap_or(lines.len() as f32 * lh),
    };
    // taffy >= 1b918ba replaced the `content_size: Size` second argument
    // with a scrollable-overflow `Rect`; a text leaf's content is exactly
    // its own box, so the rect's extent equals the measured size.
    taffy::tree::LayoutOutput::from_sizes(
        size,
        taffy::geometry::Rect {
            left: 0.0,
            top: 0.0,
            right: size.width,
            bottom: size.height,
        },
    )
}

/// The inline-formatting-context stand-in around a run of inline content:
/// a wrapping flex row (upstream run_wrapper_style / outer_style model).
fn run_wrapper_style() -> Style {    Style {
        display: Display::Flex,
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::Wrap,
        align_items: Some(AlignItems::FLEX_START),
        ..Style::default()
    }
}

/// Tags whose layout box is a replaced leaf: intrinsic size + aspect ratio,
/// no children in the layout tree. `input`/`textarea` join the img/canvas
/// family: an unstyled control used to fall through to the plain-block path
/// and measure 0×N (no content to lay out), which reads as "invisible" to
/// every rect-based consumer — Playwright's actionability loop waits forever
/// on a zero-height input (obscura#807 class).
fn is_replaced_tag(tag: &str) -> bool {
    matches!(
        tag,
        "img" | "video" | "iframe" | "canvas" | "object" | "embed" | "input" | "textarea"
    )
}

/// The URL an `<img>` should load: its `<picture>` parent's first matching
/// `<source>` candidate when present (media query evaluated at the layout
/// viewport), else the img's own srcset selection, else plain `src`
/// (HTML §4.8.4.3.9, product-simplified). None → the placeholder path.
pub fn resolve_img_source(
    tree: &DomTree,
    img: NodeId,
    viewport_width: f32,
) -> Option<String> {
    let attr = |id: NodeId, name: &str| {
        tree.with_node(id, |n| n.get_attribute(name).map(|v| v.to_string()))
            .flatten()
    };

    // A <picture> parent contributes candidates only from <source> children
    // that precede the img in document order; the img itself terminates the
    // scan.
    if let Some(parent) = tree.with_node(img, |n| n.parent).flatten() {
        let parent_is_picture = tree
            .with_node(parent, |n| {
                n.as_element().map(|e| e.local.to_string() == "picture")
            })
            .flatten()
            .unwrap_or(false);
        if parent_is_picture {
            for child in tree.children(parent) {
                let tag = tree
                    .with_node(child, |n| n.as_element().map(|e| e.local.to_string()))
                    .flatten()
                    .unwrap_or_default();
                if child == img {
                    break;
                }
                if tag != "source" {
                    continue;
                }
                // media="(min-width: …)" / absent media both evaluate here;
                // a non-matching source is skipped to the next sibling.
                if !media_matches_width(attr(child, "media").as_deref(), viewport_width) {
                    continue;
                }
                if let Some(srcset) = attr(child, "srcset") {
                    let cands = image::parse_srcset(&srcset);
                    if let Some(c) = image::select_srcset_candidate(&cands, viewport_width) {
                        return normalize_candidate_url(tree, child, &c.url);
                    }
                }
                // type="image/webp" etc.: without the candidate set matching
                // we fall through — a source with no usable srcset never wins.
            }
        }
    }

    if let Some(srcset) = attr(img, "srcset") {
        let cands = image::parse_srcset(&srcset);
        if let Some(c) = image::select_srcset_candidate(&cands, viewport_width) {
            return normalize_candidate_url(tree, img, &c.url);
        }
    }

    attr(img, "src")
}

/// Relative candidate URLs resolve against the document base (the img's
/// owner document URL lives outside the tree, so we approximate with the
/// page URL recorded on nothing here — callers pass absolute URLs for
/// network fetches; data: URLs are self-contained and pass through).
fn normalize_candidate_url(_tree: &DomTree, _source: NodeId, url: &str) -> Option<String> {
    Some(url.to_string())
}

/// Minimal media-query width gate for `<source media>`: supports the two
/// real-world forms `(min-width: Npx)` / `(max-width: Npx)` joined by
/// `and`, plus absent/`all`. Anything else declines (conservative: falls
/// through to the next source or the fallback img).
pub(crate) fn media_matches_width(media: Option<&str>, vw: f32) -> bool {
    let Some(media) = media.map(str::trim).filter(|m| !m.is_empty()) else {
        return true;
    };
    if media.eq_ignore_ascii_case("all") {
        return true;
    }
    // NOT-prefixed or print media queries: decline rather than guess.
    if media.contains("not ") || media.contains("print") {
        return false;
    }
    let mut ok = true;
    for clause in media.split(" and ") {
        let clause = clause.trim().trim_start_matches('(').trim_end_matches(')');
        if let Some(w) = clause
            .strip_prefix("min-width:")
            .and_then(|v| v.trim().strip_suffix("px"))
            .and_then(|v| v.trim().parse::<f32>().ok())
        {
            ok &= vw >= w;
        } else if let Some(w) = clause
            .strip_prefix("max-width:")
            .and_then(|v| v.trim().strip_suffix("px"))
            .and_then(|v| v.trim().parse::<f32>().ok())
        {
            ok &= vw <= w;
        }
        // Unknown feature inside a conjunction: leave `ok` untouched (the
        // common case is a single min/max-width clause anyway).
    }
    ok
}

/// The rect replaced content paints into, per object-fit/object-position
/// (batch 5c) — the port of blitz-paint's sizing.rs + render.rs draw_image
/// offset math. `box_rect` is the content box, (nw, nh) the natural size:
///
/// - Fill: the whole box (the pre-5c behavior).
/// - Contain / Cover: scale by min/max of the two axis ratios; blitz's
///   four-arm `(x<1, y<1)` match reduces to exactly that.
/// - None: natural size. ScaleDown: Contain unless natural is already
///   smaller than the contain result on width (then natural).
/// - Offset: each object-position part resolves against the free space
///   `box − paint` — percentages scale it, px lengths use it directly;
///   the initial 50%/50% centers. Cover offsets go negative (overflow).
pub fn object_paint_rect(
    box_rect: Rect,
    nw: f32,
    nh: f32,
    fit: ObjectFit,
    pos: (ObjectPositionPart, ObjectPositionPart),
) -> Rect {
    let (bw, bh) = (box_rect.width, box_rect.height);
    let paint = |w: f32, h: f32| Rect { x: 0.0, y: 0.0, width: w, height: h };
    let size = if nw <= 0.0 || nh <= 0.0 || bw <= 0.0 || bh <= 0.0 {
        match fit {
            ObjectFit::None => paint(nw, nh),
            _ => paint(bw, bh),
        }
    } else {
        let (xr, yr) = (bw / nw, bh / nh);
        match fit {
            ObjectFit::Fill => paint(bw, bh),
            ObjectFit::Contain => paint(nw * xr.min(yr), nh * xr.min(yr)),
            ObjectFit::Cover => paint(nw * xr.max(yr), nh * xr.max(yr)),
            ObjectFit::None => paint(nw, nh),
            ObjectFit::ScaleDown => {
                let (cw, ch) = (nw * xr.min(yr), nh * xr.min(yr));
                if nw < cw {
                    paint(nw, nh)
                } else {
                    paint(cw, ch)
                }
            }
        }
    };
    let resolve = |part: ObjectPositionPart, free: f32| match part {
        ObjectPositionPart::Percent(p) => free * p / 100.0,
        ObjectPositionPart::Px(x) => x,
    };
    Rect {
        x: box_rect.x + resolve(pos.0, bw - size.width),
        y: box_rect.y + resolve(pos.1, bh - size.height),
        width: size.width,
        height: size.height,
    }
}

/// Build the taffy leaf for a replaced element. Per-tag natural-size
/// semantics (batch 7a), mirroring blitz-dom layout/mod.rs:
///
/// - `img`: a decoded image gives the natural size/ratio; attributes are
///   presentational hints overriding per-axis with the image ratio
///   back-filling; nothing at all → the CSS default replaced box 300×150.
/// - `canvas`: its width/height attributes ARE the intrinsic size
///   (defaulting 300×150) and it carries an aspect RATIO — a missing axis
///   transfers through it.
/// - `video`/`iframe`/`embed`: attribute-or-300×150 per axis, NO ratio —
///   `<video width=600>` lays out 600×150, height does not transfer.
fn build_replaced_leaf(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
) -> Option<taffy::tree::NodeId> {
    let style = styles.get(&id).cloned().unwrap_or_default();
    let tag = tree
        .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    let attr = |name: &str| {
        tree.with_node(id, |n| n.get_attribute(name).map(|v| v.to_string()))
            .flatten()
            .and_then(|v| v.parse::<f32>().ok())
    };
    let (aw, ah) = (attr("width"), attr("height"));
    // Natural size AND whether a missing axis derives from the ratio.
    let (nat_w, nat_h, ratio_transfer) = match tag.as_str() {
        // Form controls (obscura#807 class): Chrome-shaped default boxes.
        // Text-like inputs ≈ the size=20 default column; checkbox/radio are
        // square 13px widgets; button-like inputs approximate label width
        // from the value attribute (input has no laid-out text of its own);
        // textarea is the cols=20/rows=2 default box. All three axes are
        // overridable per-axis by CSS/attribute width & height below.
        "input" => {
            let ty = tree
                .with_node(id, |n| {
                    n.get_attribute("type").map(|v| v.to_ascii_lowercase())
                })
                .flatten()
                .unwrap_or_default();
            match ty.as_str() {
                "checkbox" | "radio" => (13.0, 13.0, false),
                "button" | "submit" | "reset" => {
                    let label = tree
                        .with_node(id, |n| n.get_attribute("value").map(|v| v.to_string()))
                        .flatten()
                        .unwrap_or_default();
                    let label: String = if label.is_empty() && ty == "submit" {
                        "Submit".to_string()
                    } else {
                        label
                    };
                    (label.chars().count() as f32 * 8.8 + 20.0, 22.0, false)
                }
                _ => (177.0, 22.0, false),
            }
        }
        "textarea" => (177.0, 38.0, false),
        "canvas" => (
            aw.unwrap_or(300.0),
            ah.unwrap_or(150.0),
            true,
        ),
        "video" | "iframe" | "embed" | "object" => {
            (aw.unwrap_or(300.0), ah.unwrap_or(150.0), false)
        }
        // img: attrs override the decoded image per-axis, the image ratio
        // back-fills a free axis; with nothing at all, 300×150.
        _ => {
            let img_ratio = images.get(&id).map(|i| i.width as f32 / i.height as f32);
            let (w, h) = match (aw, ah) {
                (Some(w), Some(h)) if h > 0.0 => (w, h),
                (Some(w), None) => (w, w / img_ratio.unwrap_or(2.0)),
                (None, Some(h)) => (h * img_ratio.unwrap_or(2.0), h),
                (None, None) => images
                    .get(&id)
                    .map(|i| (i.width as f32, i.height as f32))
                    .unwrap_or((300.0, 150.0)),
                // Degenerate attrs (e.g. height="0"): the CSS default box.
                _ => (300.0, 150.0),
            };
            (w, h, true)
        }
    };

    // Attrs block ratio transfer only for img: width/height attrs are
    // presentational-hint DECLARATIONS, so a CSS override of one axis does
    // not re-derive the other (#cssw stays 100×200, not 100×50). Canvas
    // attrs ARE the intrinsic size and never block.
    let (attr_dw, attr_dh) = if tag == "img" { (aw.is_some(), ah.is_some()) } else { (false, false) };
    let ratio = ratio_transfer.then(|| nat_w / nat_h);
    let css_w_px = match style.width {
        Some(crate::diting_css::Length::Px(w)) => Some(w),
        _ => None,
    };
    let css_h_px = match style.height {
        Some(crate::diting_css::Length::Px(h)) => Some(h),
        _ => None,
    };
    // Build-time ratio transfer: with exactly one authored CSS px axis and
    // nothing declaring the other, the free axis derives through the
    // natural ratio. taffy's aspect_ratio only re-derives in block/column
    // flows — not the flex-row run wrapper inline atoms now join — while
    // Chrome transfers in every container.
    let derived_h = match (ratio, css_w_px) {
        (Some(r), Some(w)) if !attr_dh => Some(w / r),
        _ => None,
    };
    let derived_w = match (ratio, css_h_px) {
        (Some(r), Some(h)) if !attr_dw => Some(h * r),
        _ => None,
    };

    let mut s = Style::default();
    s.item_is_replaced = true;
    // UA/author border lays out on replaced boxes too (batch 7a): the
    // iframe's UA `2px inset` makes a width=600 attr box come out 604.
    // taffy sizes are border-box, so the attr/CSS size gets the widths
    // added (content-box semantics of the HTML attributes).
    let bline = style.border_style.is_some();
    let bw = |which: f32| -> LengthPercentage {
        LengthPercentage::length(if bline { which } else { 0.0 })
    };
    let (bt, br, bb, bl) = (
        side_px(style.border_width.top),
        side_px(style.border_width.right),
        side_px(style.border_width.bottom),
        side_px(style.border_width.left),
    );
    s.border = taffy::geometry::Rect {
        top: bw(bt),
        right: bw(br),
        bottom: bw(bb),
        left: bw(bl),
    };
    s.aspect_ratio = ratio_transfer.then(|| nat_w / nat_h);
    // CSS width/height win per axis; missing axis derives from the ratio.
    // Percent CSS sizes pass through (the CB resolves them; the natural
    // ratio only backfills auto axes). Px sizes are content-box per the
    // attribute semantics, so border widths ride on top.
    s.size = Size {
        width: match style.width {
            Some(crate::diting_css::Length::Px(w)) => {
                Dimension::length(w + if bline { bl + br } else { 0.0 })
            }
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            // Percent-only placeholder (same convention as to_taffy_style).
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                Dimension::percent(percent / 100.0)
            }
            // Sizing keywords on replaced boxes: intrinsic = natural size
            // (css-sizing-3), same fallback as auto.
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => Dimension::length(derived_w.unwrap_or(nat_w) + if bline { bl + br } else { 0.0 }),
        },
        height: match style.height {
            Some(crate::diting_css::Length::Px(h)) => {
                Dimension::length(h + if bline { bt + bb } else { 0.0 })
            }
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            Some(crate::diting_css::Length::Calc { percent, .. }) => {
                Dimension::percent(percent / 100.0)
            }
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => Dimension::length(derived_h.unwrap_or(nat_h) + if bline { bt + bb } else { 0.0 }),
        },
    };

    let node = taffy_tree.new_leaf(s).ok()?;
    node_map.insert(node, id);
    Some(node)
}

/// Build the taffy subtree for one element. Returns None for display:none
/// (subtree skipped) and for the document node's non-element parts.
/// Build one child into its parent's normal-flow child list — the same
/// replaced/text/inline-run/block dispatch as build_element's main loop,
/// factored out so the float-zone branches (8b/8c) can append zone-external
/// siblings without duplicating the body. Runs are flushed internally (the
/// float branches interleave zone rows between siblings, so a shared pending
/// run across calls would misorder).
#[allow(clippy::too_many_arguments)]
fn build_normal_sibling(
    child: NodeId,
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
    flattened: &mut HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    run_wrappers: &mut Vec<taffy::tree::NodeId>,
    atomic_container: bool,
    font_size: f32,
    line_height: f32,
    direct: &mut Vec<taffy::tree::NodeId>,
) {
    let is_text = tree.with_node(child, |n| n.is_text()).unwrap_or(false);
    let child_tag = tree
        .with_node(child, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    let child_display = styles
        .get(&child)
        .and_then(|s| s.display)
        .or(if is_text { Some(CssDisplay::Inline) } else { Some(CssDisplay::Block) });
    let inline_level = matches!(child_display, Some(CssDisplay::Inline));
    let out_of_flow = styles.get(&child).is_some_and(|s| {
        matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
    });
    if !is_text && is_replaced_tag(&child_tag) {
        // A replaced element whose display resolves to any inline flavor
        // (UA-sheet inline-block for form controls included) is an inline
        // atom: it joins the text run instead of becoming a block sibling.
        let inline_atom =
            inline_level || child_display == Some(CssDisplay::InlineBlock);
        if inline_atom && !atomic_container && !out_of_flow {
            if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map) {
                // A lone inline atom still gets the wrapping-run stand-in so
                // it lays out on the text baseline path like the main loop.
                if let Ok(wrapper) =
                    taffy_tree.new_with_children(run_wrapper_style(), &[leaf])
                {
                    run_wrappers.push(wrapper);
                    direct.push(wrapper);
                }
            }
        } else if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map) {
            direct.push(leaf);
        }
        return;
    }
    if !is_text
        && child_display == Some(CssDisplay::InlineBlock)
        && !atomic_container
        && !out_of_flow
    {
        // Atomic inline-level box (obscura#750 family): keeps its own subtree
        // box and gets the wrapping-run stand-in so it sits on the text line
        // path like a replaced atom, sized shrink-to-fit by the parent run.
        if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
            if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &[sub]) {
                run_wrappers.push(wrapper);
                direct.push(wrapper);
            }
        }
        return;
    }
    if is_text || (inline_level && !atomic_container && !out_of_flow) {
        // Text and flattenable inlines become their own measured run here:
        // single-segment runs are the overwhelmingly common case at
        // zone boundaries, and build_word_leaves + the run wrapper reproduce
        // the mixed-run path exactly.
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        if is_text {
            let text = tree
                .with_node(child, |n| n.text_content_of_text_node().unwrap_or("").to_string())
                .unwrap_or_default();
            // Formatting-whitespace-only text generates no box (CSS white-
            // space processing — keeps adjoining block margins adjacent).
            if text.trim().is_empty() {
                return;
            }
            let (fs, b, lh) = font_context(tree, child, styles);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { line_height };
            let col = color_context(tree, child, styles);
            leaves.extend(build_word_leaves(&text, fs, b, col, lh, fonts, taffy_tree));
        } else {
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                leaves.extend(sub_children.clone());
                // Flattening removes the sub's taffy node (invalidating its
                // SlotMap key) — drop the stale node_map entry with it.
                node_map.remove(&sub);
                let _ = taffy_tree.remove(sub);
                // The element keeps its DOM identity boxless — the union
                // pass after the collect walk rebuilds a rect from the kids.
                if !sub_children.is_empty() {
                    flattened.insert(child, sub_children);
                }
            }
        }
        if !leaves.is_empty() {
            if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
                run_wrappers.push(wrapper);
                direct.push(wrapper);
            }
        }
        return;
    }
    if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
        direct.push(node);
    }
}

/// Rough content-height estimate for a float (8g; upstream
/// estimate_float_height): explicit px heights when present, else one line
/// per structural row (p/li/tr/hN) plus a character-count text estimate.
/// Deliberately rough — it only decides how much flow shares the float's
/// band. The 200px floor covers the no-information case (an empty/icon
/// float); an explicit or content-derived estimate is almost always better
/// than a generous floor.
fn estimate_float_height(tree: &DomTree, styles: &HashMap<NodeId, ComputedStyle>, id: NodeId) -> f32 {
    let mut est: f32 = styles
        .get(&id)
        .and_then(|s| s.height)
        .map(|h| match h {
            crate::diting_css::Length::Px(v) => v,
            _ => 0.0,
        })
        .unwrap_or(0.0);
    fn estimate_into(
        tree: &DomTree,
        id: NodeId,
        styles: &HashMap<NodeId, ComputedStyle>,
        est: &mut f32,
    ) {
        let tag = tree
            .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
            .flatten()
            .unwrap_or_default();
        let st = styles.get(&id);
        if let Some(h) = st.and_then(|s| s.height) {
            if let crate::diting_css::Length::Px(v) = h {
                *est += v;
                return;
            }
        }
        if matches!(tag.as_str(), "li" | "tr" | "dt" | "dd" | "p" | "figcaption" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
            *est += st.and_then(|s| s.font_size).unwrap_or(16.0) * 1.2;
        }
        let text_len = tree
            .with_node(id, |n| n.text_content_of_text_node().map(|t| t.chars().filter(|c| !c.is_whitespace()).count()).unwrap_or(0))
            .unwrap_or(0) as f32;
        if text_len > 0.0 {
            let fsize = st.and_then(|s| s.font_size).unwrap_or(16.0);
            // ~0.55em average glyph advance at an assumed 280px band.
            let chars_per_line = (280.0 / (fsize * 0.55)).max(1.0);
            *est += (text_len / chars_per_line).ceil() * fsize * 1.2 + 16.0;
        }
        for child in tree.children(id) {
            estimate_into(tree, child, styles, est);
        }
    }
    for child in tree.children(id) {
        estimate_into(tree, child, styles, &mut est);
    }
    if est <= 0.0 {
        est = 200.0;
    }
    est
}

/// Where a float's zone ends (8b/8i): the first sibling from `start` that
/// clears `run_side` (the clearfix idiom), the next float, or the sibling
/// whose flow estimate completes the float's height budget (8g) — real float
/// reflow ends when normal flow passes the float's bottom edge. Without the
/// budget a short float drags every following section into the narrow
/// column.
fn zone_end_at_budget(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    child_ids: &[NodeId],
    start: usize,
    budget: f32,
    run_side: Option<crate::diting_css::FloatSide>,
) -> usize {
    let is_float = |cid: &NodeId| -> bool {
        styles
            .get(cid)
            .is_some_and(|s| s.float_side.is_some() && s.display != Some(CssDisplay::None))
    };
    let mut zone_end = child_ids.len();
    let mut flow_estimate = 0.0f32;
    const ASSUMED_FLOW_WIDTH: f32 = 500.0;
    for (i, cid) in child_ids.iter().enumerate().skip(start) {
        let clears_this = styles.get(cid).and_then(|s| s.clear_side).is_some_and(|c| match c {
            crate::diting_css::ClearSide::Both => true,
            // Logical keywords resolve against this engine's LTR-only mode.
            crate::diting_css::ClearSide::InlineStart => run_side == Some(crate::diting_css::FloatSide::Left),
            crate::diting_css::ClearSide::InlineEnd => run_side == Some(crate::diting_css::FloatSide::Right),
            crate::diting_css::ClearSide::Left => run_side == Some(crate::diting_css::FloatSide::Left),
            crate::diting_css::ClearSide::Right => run_side == Some(crate::diting_css::FloatSide::Right),
        });
        if clears_this || is_float(cid) {
            zone_end = i;
            break;
        }
        // Rough per-sibling height contribution at the assumed width:
        // explicit px height wins, else structural-row lines plus a
        // character-count wrap estimate (same heuristic as the float side).
        let st = styles.get(cid);
        // Whitespace-only text between blocks contributes nothing.
        let ws_text = tree.with_node(*cid, |n| !n.is_element() && n.text_content_of_text_node().map_or(false, |t| t.trim().is_empty())).unwrap_or(false);
        if ws_text {
            continue;
        }
        let mut contrib = st
            .and_then(|s| s.height)
            .map(|h| match h {
                crate::diting_css::Length::Px(v) => v.max(0.0),
                _ => 0.0,
            })
            .unwrap_or(0.0);
        let fsize = st.and_then(|s| s.font_size).unwrap_or(16.0);
        let mut chars = 0.0f32;
        fn count_text(tree: &DomTree, id: NodeId, chars: &mut f32) {
            *chars += tree
                .with_node(id, |n| {
                    n.text_content_of_text_node()
                        .map(|t| t.chars().filter(|c| !c.is_whitespace()).count() as f32)
                        .unwrap_or(0.0)
                })
                .unwrap_or(0.0);
            for c in tree.children(id) {
                count_text(tree, c, chars);
            }
        }
        count_text(tree, *cid, &mut chars);
        if chars > 0.0 {
            let chars_per_line = (ASSUMED_FLOW_WIDTH / (fsize * 0.55)).max(1.0);
            contrib += (chars / chars_per_line).ceil() * fsize * 1.2;
        }
        if contrib <= 0.0 {
            contrib = fsize * 1.2;
        }
        flow_estimate += contrib;
        if flow_estimate >= budget {
            zone_end = i + 1;
            break;
        }
    }
    zone_end
}

/// The anonymous flow column of a float zone (8b/8i): every in-zone sibling
/// built normally inside one block wrapper. Not in node_map — it has no DOM
/// identity, so collect skips it and paints walk straight through to the
/// real children.
#[allow(clippy::too_many_arguments)]
fn build_flow_column(
    flow_dom: &[NodeId],
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
    flattened: &mut HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    run_wrappers: &mut Vec<taffy::tree::NodeId>,
    font_size: f32,
    lh_elem: f32,
) -> Vec<taffy::tree::NodeId> {
    enum RunSeg {
        Text(String, f32, bool, [u8; 4], f32),
        Nodes(Vec<taffy::tree::NodeId>),
    }
    let mut flow_children: Vec<taffy::tree::NodeId> = Vec::new();
    let mut run: Vec<RunSeg> = Vec::new();
    let flush_run = |run: &mut Vec<RunSeg>, flow_children: &mut Vec<taffy::tree::NodeId>, taffy_tree: &mut TaffyTree<TextLeaf>, run_wrappers: &mut Vec<taffy::tree::NodeId>| {
        if run.is_empty() {
            return;
        }
        let segs = std::mem::take(run);
        if segs.iter().all(|s| matches!(s, RunSeg::Text(..))) {
            let text = segs
                .iter()
                .map(|s| match s { RunSeg::Text(t, ..) => t.as_str(), _ => "" })
                .collect::<String>();
            if !text.trim().is_empty() {
                let RunSeg::Text(_, fs, bold, color, lh) = &segs[0] else { unreachable!() };
                if let Ok(leaf) = taffy_tree.new_leaf_with_context(
                    Style::default(),
                    TextLeaf::Run { text, font_size: *fs, bold: *bold, color: *color, line_height: *lh },
                ) {
                    flow_children.push(leaf);
                }
            }
            return;
        }
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        for seg in segs {
            match seg {
                RunSeg::Text(text, fs, bold, color, lh) => {
                    leaves.extend(build_word_leaves(&text, fs, bold, color, lh, fonts, taffy_tree))
                }
                RunSeg::Nodes(nodes) => leaves.extend(nodes),
            }
        }
        trim_run_edge_whitespace(taffy_tree, &mut leaves);
        if !leaves.is_empty() {
            if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
                run_wrappers.push(wrapper);
                flow_children.push(wrapper);
            }
        }
    };
    for child in flow_dom.iter().copied() {
        let is_text = tree.with_node(child, |n| n.is_text()).unwrap_or(false);
        let child_tag = tree
            .with_node(child, |n| n.as_element().map(|e| e.local.to_string()))
            .flatten()
            .unwrap_or_default();
        let child_display = styles
            .get(&child)
            .and_then(|s| s.display)
            .or(if is_text { Some(CssDisplay::Inline) } else { Some(CssDisplay::Block) });
        let inline_level = matches!(child_display, Some(CssDisplay::Inline));
        let out_of_flow = styles.get(&child).is_some_and(|s| {
            matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        });
        if !is_text && is_replaced_tag(&child_tag) {
            let leaf = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map);
            if let Some(leaf) = leaf {
                // Inline-flavored replaced elements (UA inline-block form
                // controls included) join the text run; the rest stay block
                // siblings (current shipped behavior for img/video).
                let inline_atom =
                    inline_level || child_display == Some(CssDisplay::InlineBlock);
                if inline_atom && !out_of_flow {
                    run.push(RunSeg::Nodes(vec![leaf]));
                } else {
                    flush_run(&mut run, &mut flow_children, taffy_tree, run_wrappers);
                    flow_children.push(leaf);
                }
            }
            continue;
        }
        if is_text {
            let text = tree.with_node(child, |n| n.text_content_of_text_node().unwrap_or("").to_string()).unwrap_or_default();
            let (fs, b, lh) = font_context(tree, child, styles);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { lh_elem };
            let col = color_context(tree, child, styles);
            run.push(RunSeg::Text(text, fs, b, col, lh));
        } else if child_display == Some(CssDisplay::InlineBlock) && !out_of_flow {
            // Atomic inline-level box (obscura#750 family): keeps its own
            // subtree box, joins the run as one shrink-to-fit unit.
            if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                run.push(RunSeg::Nodes(vec![sub]));
            }
        } else if inline_level && !out_of_flow {
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                run.push(RunSeg::Nodes(sub_children.clone()));
                node_map.remove(&sub);
                let _ = taffy_tree.remove(sub);
                // Boxless after hoisting — union pass rebuilds the rect.
                if !sub_children.is_empty() {
                    flattened.insert(child, sub_children);
                }
            }
        } else {
            flush_run(&mut run, &mut flow_children, taffy_tree, run_wrappers);
            if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                flow_children.push(node);
            }
        }
    }
    flush_run(&mut run, &mut flow_children, taffy_tree, run_wrappers);
    flow_children
}

/// Whether `id`'s subtree can never paint anything: no non-whitespace text
/// outside metadata elements. MediaWiki parks
/// `<span class="mw-empty-elt"><link/><link/></span>` wrappers between its
/// floated infobox and the sidebar tables — the links are ResourceLoader
/// hints, not content — so a bare has-children check misreads the wrappers
/// as flow content and stops the same-side float rail from forming.
fn subtree_paints_nothing(tree: &DomTree, id: NodeId) -> bool {
    enum Step {
        Paints,
        Skip,
        Descend,
    }
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        let Some(step) = tree.with_node(cur, |n| match &n.data {
            crate::diting_dom::NodeData::Text { contents } => {
                if contents.trim().is_empty() { Step::Skip } else { Step::Paints }
            }
            crate::diting_dom::NodeData::Element { name, .. } => {
                if matches!(name.local.as_ref(), "link" | "meta" | "style" | "script" | "template") {
                    Step::Skip
                } else {
                    Step::Descend
                }
            }
            _ => Step::Skip,
        }) else {
            return false;
        };
        match step {
            Step::Paints => return false,
            Step::Skip => {}
            Step::Descend => {
                let mut c = tree.with_node(cur, |n| n.first_child).flatten();
                while let Some(x) = c {
                    stack.push(x);
                    c = tree.with_node(x, |n| n.next_sibling).flatten();
                }
            }
        }
    }
    true
}

fn build_element(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
    flattened: &mut HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    run_wrappers: &mut Vec<taffy::tree::NodeId>,
) -> Option<taffy::tree::NodeId> {
    let style = styles.get(&id).cloned().unwrap_or_default();
    if style.display == Some(CssDisplay::None) {
        return None;
    }
    let tag = tree
        .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    // Reached directly (root, or a replaced element someone recursed into):
    // replaced boxes own no layout children.
    if is_replaced_tag(&tag) {
        return build_replaced_leaf(tree, id, styles, images, taffy_tree, node_map);
    }

    let child_ids: Vec<NodeId> = tree.children(id);

    // In a flex/grid container every element child is blockified into its own
    // item (CSS flex-item blockification); runs only form in block/inline
    // formatting contexts.
    let atomic_container = matches!(style.display, Some(CssDisplay::Flex) | Some(CssDisplay::Grid));

    // Partition children into block-level elements (direct taffy children)
    // and inline runs (text + inline elements). A PURE-text run (only text
    // nodes) becomes ONE measured leaf — the same shape blitz gives its
    // parley text nodes, whose observable behavior (edge-whitespace collapse,
    // greedy wrap, ceiled width) we reproduce in measure_text_leaf. Mixed
    // runs fall back to the batch-2b wrapping flex row of word leaves.
    enum RunSeg {
        Text(String, f32, bool, [u8; 4], f32),
        Nodes(Vec<taffy::tree::NodeId>),
    }
    let mut direct: Vec<taffy::tree::NodeId> = Vec::new();
    let mut run: Vec<RunSeg> = Vec::new();
    let flush_run = |run: &mut Vec<RunSeg>, direct: &mut Vec<taffy::tree::NodeId>, taffy_tree: &mut TaffyTree<TextLeaf>, run_wrappers: &mut Vec<taffy::tree::NodeId>| {
        if run.is_empty() {
            return;
        }
        let segs = std::mem::take(run);
        // All-text run → one measured leaf (adjacent DOM text nodes
        // concatenate, which is also how CSS joins them). A run that is
        // ONLY formatting whitespace generates no box at all (CSS white-
        // space processing: it would otherwise sit between two blocks and
        // physically separate their adjoining margins, breaking collapse).
        if segs.iter().all(|s| matches!(s, RunSeg::Text(..))) {
            let text = segs
                .iter()
                .map(|s| match s { RunSeg::Text(t, ..) => t.as_str(), _ => "" })
                .collect::<String>();
            if text.trim().is_empty() {
                return;
            }
            let RunSeg::Text(_, fs, bold, color, lh) = &segs[0] else { unreachable!() };
            if let Ok(leaf) = taffy_tree.new_leaf_with_context(
                Style::default(),
                TextLeaf::Run { text, font_size: *fs, bold: *bold, color: *color, line_height: *lh },
            ) {
                direct.push(leaf);
                return;
            }
        }
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        for seg in segs {
            match seg {
                RunSeg::Text(text, fs, bold, color, lh) => {
                    leaves.extend(build_word_leaves(&text, fs, bold, color, lh, fonts, taffy_tree))
                }
                RunSeg::Nodes(nodes) => leaves.extend(nodes),
            }
        }
        trim_run_edge_whitespace(taffy_tree, &mut leaves);
        if !leaves.is_empty() {
            if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
                run_wrappers.push(wrapper);
                direct.push(wrapper);
            }
        }
    };

    let (font_size, _bold, lh_elem) = font_context(tree, id, styles);

    // --- float zone (batch 8b/8c): floats reified as synthetic flex rows ---
    // taffy (as configured — float_layout is a non-default feature that must
    // stay off) has no floats, so float shapes are REIFIED at tree-build
    // time, following upstream obscura-render's
    // build_children_with_float_zone:
    //
    // - A RUN of ≥2 consecutive same-side floats (the classic float-grid
    //   idiom, whitespace between them allowed) becomes ONE wrapping flex
    //   row — CSS places same-side floats side by side, wrapping to a new
    //   band when the row fills (8c).
    // - A single float plus following siblings becomes [float | anonymous
    //   flow column]: the column takes the remaining width; the float keeps
    //   its authored margins/size. Right floats sit at the row's inline-end.
    //   (8b)
    // - A `clear` sibling ends a zone and stays in normal flow after the
    //   row.
    // - Zones interleave with normal siblings in DOCUMENT order (8h): a
    //   float's zone row lands at the float's document position — content
    //   before it keeps its band ABOVE the zone — and a second float after
    //   the zone opens its own zone instead of demoting to a plain block.
    //   Wikipedia's lead section floats an infobox, then two sidebar tables
    //   across empty bridges, then runs the lead paragraphs.
    let is_float_child = |cid: &NodeId| -> bool {
        styles
            .get(cid)
            .is_some_and(|s| s.float_side.is_some() && s.display != Some(CssDisplay::None))
    };
    if child_ids.iter().any(|cid| is_float_child(cid)) {
        let is_whitespace_text =
            |cid: &NodeId| -> bool { tree.with_node(*cid, |n| !n.is_element() && n.text_content_of_text_node().map_or(false, |t| t.trim().is_empty())).unwrap_or(false) };
        // An empty bridge sibling (upstream is_empty_bridge): whitespace
        // text OR an element with no authored size/margin/padding/border
        // whose subtree paints nothing — the legacy compatibility boxes real
        // pages park between the two header floats, and MediaWiki's
        // mw-empty-elt wrappers (link/meta cargo included) between float
        // runs.
        let is_empty_bridge = |cid: &NodeId| -> bool {
            if is_whitespace_text(cid) {
                return true;
            }
            if !subtree_paints_nothing(tree, *cid) {
                return false;
            }
            styles.get(cid).is_some_and(|s| {
                s.width.is_none()
                    && s.height.is_none()
                    && s.margin.top.is_none()
                    && s.margin.right.is_none()
                    && s.margin.bottom.is_none()
                    && s.margin.left.is_none()
                    && s.padding.top.is_none()
                    && s.padding.right.is_none()
                    && s.padding.bottom.is_none()
                    && s.padding.left.is_none()
                    && s.border_style.is_none()
            })
        };

        // --- 8e: the right-float navigation bar --------------------------
        // A container of inline-ish flow content plus >=2 RIGHT floats and
        // no left float: right floats place from the inline-end inward, so
        // their visual order is the REVERSE of source order, while ordinary
        // content fills from the start of the same band. Serializing each
        // float into its own row reverses the two groups and shrink-wraps
        // the bar. Reified as [flow items | reversed right-float group],
        // with an anonymous wrapping row at definite width (upstream
        // strategy 4).
        let first_float_side = child_ids
            .iter()
            .find(|cid| is_float_child(cid))
            .and_then(|cid| styles.get(cid).and_then(|s| s.float_side));
        let all_right = first_float_side == Some(crate::diting_css::FloatSide::Right)
            && !child_ids.iter().any(|cid| {
                styles.get(cid).and_then(|s| s.float_side) == Some(crate::diting_css::FloatSide::Left)
            });
        let flow_is_inline = child_ids.iter().all(|cid| {
            if is_float_child(cid) || is_empty_bridge(cid) {
                return true;
            }
            styles
                .get(cid)
                .map_or(true, |s| s.display != Some(CssDisplay::Block))
        });
        let right_floats: Vec<NodeId> = child_ids
            .iter()
            .copied()
            .filter(|cid| {
                styles.get(cid).is_some_and(|s| {
                    s.float_side == Some(crate::diting_css::FloatSide::Right) && s.display != Some(CssDisplay::None)
                })
            })
            .collect();
        if all_right && flow_is_inline && right_floats.len() >= 2 {
            // Flow items in source order; runs of formatting whitespace
            // collapse to one representative node and drop at band edges.
            let mut flow_dom: Vec<NodeId> = Vec::new();
            let mut pending_ws: Option<NodeId> = None;
            let mut has_flow_content = false;
            for &cid in &child_ids {
                if is_float_child(&cid) {
                    continue;
                }
                if is_whitespace_text(&cid) {
                    if has_flow_content && pending_ws.is_none() {
                        pending_ws = Some(cid);
                    }
                    continue;
                }
                if has_flow_content {
                    if let Some(ws) = pending_ws.take() {
                        flow_dom.push(ws);
                    }
                } else {
                    pending_ws = None;
                }
                flow_dom.push(cid);
                has_flow_content = true;
            }
            let mut row_children: Vec<taffy::tree::NodeId> = Vec::new();
            for cid in flow_dom {
                build_normal_sibling(
                    cid,
                    tree,
                    styles,
                    images,
                    fonts,
                    taffy_tree,
                    node_map,
                    flattened,
                    run_wrappers,
                    atomic_container,
                    font_size,
                    lh_elem,
                    &mut row_children,
                );
            }
            // The right-float group in REVERSED source order (CSS places
            // right floats inline-end first).
            let mut right_children: Vec<taffy::tree::NodeId> = Vec::new();
            for cid in right_floats.iter().rev() {
                if let Some(f) = build_element(tree, *cid, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                    right_children.push(f);
                }
            }
            if !row_children.is_empty() && !right_children.is_empty() {
                let group_style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    margin: taffy::geometry::Rect {
                        top: LengthPercentageAuto::length(0.0),
                        right: LengthPercentageAuto::length(0.0),
                        bottom: LengthPercentageAuto::length(0.0),
                        left: LengthPercentageAuto::auto(),
                    },
                    ..Default::default()
                };
                if let Ok(group) = taffy_tree.new_with_children(group_style, &right_children) {
                    row_children.push(group);
                }
                let bar_style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    align_items: Some(AlignItems::FLEX_START),
                    size: Size { width: percent(1.0), height: auto() },
                    ..Default::default()
                };
                if let Ok(bar) = taffy_tree.new_with_children(bar_style, &row_children) {
                    direct.push(bar);
                }
            } else {
                // Degenerate side empty: fall back to plain normal flow.
                for n in row_children.drain(..) {
                    direct.push(n);
                }
                for f in right_children.drain(..) {
                    direct.push(f);
                }
            }
            collapse_adjacent_sibling_margins(taffy_tree, styles, node_map, &direct);
            let taffy_style = to_taffy_style(&style);
            let node = if direct.is_empty() {
                taffy_tree.new_leaf(taffy_style).ok()?
            } else {
                taffy_tree.new_with_children(taffy_style, &direct).ok()?
            };
            node_map.insert(node, id);
            return Some(node);
        }
        // --- 8h: the document-order zone walk --------------------------------
        // The cursor walks child_ids once. Normal siblings append to direct
        // where they stand; a float opens its zone AT its document position
        // and the walk resumes after the zone. This is what keeps multiple
        // zones (infobox → sidebars → prose) in source order, and stops
        // later floats from falling into build_normal_sibling's plain-block
        // path (the bug that parked wikipedia's sidebar tables above the
        // lead prose).
        let mut cursor = 0usize;
        while cursor < child_ids.len() {
            let child = child_ids[cursor];
            if !is_float_child(&child) {
                build_normal_sibling(
                    child,
                    tree,
                    styles,
                    images,
                    fonts,
                    taffy_tree,
                    node_map,
                    flattened,
                    run_wrappers,
                    atomic_container,
                    font_size,
                    lh_elem,
                    &mut direct,
                );
                cursor += 1;
                continue;
            }
            let float_idx = cursor;
            let run_side = styles.get(&child_ids[float_idx]).and_then(|s| s.float_side);
            // Extend the run across consecutive same-side floats, skipping
            // formatting whitespace between them.
            let mut run_end = float_idx + 1;
            while run_end < child_ids.len() {
                let rc = child_ids[run_end];
                let extends_run = is_float_child(&rc)
                    && styles.get(&rc).and_then(|s| s.float_side) == run_side;
                if extends_run || is_whitespace_text(&rc) {
                    run_end += 1;
                } else {
                    break;
                }
            }
            let run_len = (float_idx..run_end).filter(|&i| is_float_child(&child_ids[i])).count();

            // --- 8d: opposing float pair on one band -------------------------
            // The classic left-logo / right-tagline header: a float followed
            // (possibly across empty bridge siblings) by an OPPOSITE-side float
            // shares one band — the left float hugs the left edge, the right
            // float the right edge. Reified as a space-between row; a multi-
            // float run packs into an inner wrapping row first.
            let opposite_side = match run_side {
                Some(crate::diting_css::FloatSide::Left) => Some(crate::diting_css::FloatSide::Right),
                Some(crate::diting_css::FloatSide::Right) => Some(crate::diting_css::FloatSide::Left),
                None => None,
            };
            let mut bridge_end = run_end;
            while bridge_end < child_ids.len() && is_empty_bridge(&child_ids[bridge_end]) {
                bridge_end += 1;
            }
            let opposite_at = (bridge_end < child_ids.len()
                && is_float_child(&child_ids[bridge_end])
                && styles.get(&child_ids[bridge_end]).and_then(|s| s.float_side) == opposite_side)
                .then_some(bridge_end);
            if let Some(opp_idx) = opposite_at {
            let mut pair_children: Vec<taffy::tree::NodeId> = Vec::new();
            if run_len >= 2 {
                let mut inner: Vec<taffy::tree::NodeId> = Vec::new();
                for i in float_idx..run_end {
                    if !is_float_child(&child_ids[i]) {
                        continue;
                    }
                    if let Some(f) =
                        build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers)
                    {
                        inner.push(f);
                    }
                }
                let inner_style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    align_items: Some(AlignItems::FLEX_START),
                    ..Default::default()
                };
                if let Some(row) = taffy_tree.new_with_children(inner_style, &inner).ok() {
                    pair_children.push(row);
                }
            } else if let Some(f) =
                build_element(tree, child_ids[float_idx], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers)
            {
                pair_children.push(f);
            }
            if let Some(o) =
                build_element(tree, child_ids[opp_idx], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers)
            {
                pair_children.push(o);
            }
            let pair_style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                justify_content: Some(JustifyContent::SPACE_BETWEEN),
                align_items: Some(AlignItems::FLEX_START),
                size: Size { width: percent(1.0), height: auto() },
                ..Default::default()
            };
            if let Ok(row) = taffy_tree.new_with_children(pair_style, &pair_children) {
                direct.push(row);
            }
            // The walk resumes after the pair: earlier siblings already
            // appended above, later content (another float included) takes
            // the next loop turn.
            cursor = opp_idx + 1;
            continue;
        }

        // --- 8i: the same-side float rail ---------------------------------
        // Same-side floats separated ONLY by empty bridges — wikipedia
        // parks mw-empty-elt spans between its infobox and the sidebar
        // tables, whose CSS stacks them with clear. ONE vertical rail at the
        // inline end plus ONE flow column spanning the whole rail: the lead
        // text starts beside the FIRST float's band, like a real browser's
        // float rail, instead of stacking each float in its own zone row
        // above the text.
        if opposite_at.is_none()
            && bridge_end < child_ids.len()
            && is_float_child(&child_ids[bridge_end])
            && styles.get(&child_ids[bridge_end]).and_then(|s| s.float_side) == run_side
        {
            // Keep absorbing bridge-separated same-side floats into the rail.
            let mut rail_end = bridge_end + 1;
            loop {
                let mut b = rail_end;
                while b < child_ids.len() && is_empty_bridge(&child_ids[b]) {
                    b += 1;
                }
                if b < child_ids.len()
                    && is_float_child(&child_ids[b])
                    && styles.get(&child_ids[b]).and_then(|s| s.float_side) == run_side
                {
                    rail_end = b + 1;
                } else {
                    break;
                }
            }
            let rail_idx: Vec<usize> = (float_idx..rail_end)
                .filter(|&i| is_float_child(&child_ids[i]))
                .collect();
            if rail_idx.len() >= 2 {
                // The budget spans the WHOLE rail: the flow column shares
                // the band with every rail float combined.
                let budget: f32 = rail_idx
                    .iter()
                    .map(|&i| estimate_float_height(tree, styles, child_ids[i]))
                    .sum();
                let zone_end = zone_end_at_budget(
                    tree,
                    styles,
                    &child_ids,
                    rail_idx[rail_idx.len() - 1] + 1,
                    budget,
                    run_side,
                );
                let flow_children = build_flow_column(
                    &child_ids[rail_end..zone_end],
                    tree,
                    styles,
                    images,
                    fonts,
                    taffy_tree,
                    node_map,
                    flattened,
                    run_wrappers,
                    font_size,
                    lh_elem,
                );
                let float_right = run_side == Some(crate::diting_css::FloatSide::Right);
                let mut rail_children: Vec<taffy::tree::NodeId> = Vec::new();
                for &i in &rail_idx {
                    if let Some(f) =
                        build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers)
                    {
                        rail_children.push(f);
                    }
                }
                let column_style = Style {
                    display: Display::Block,
                    flex_grow: 1.0,
                    flex_shrink: 1.0,
                    flex_basis: Dimension::length(0.0),
                    min_size: Size { width: LengthPercentageAuto::length(0.0), height: LengthPercentageAuto::auto() },
                    ..Default::default()
                };
                let column = (!flow_children.is_empty())
                    .then(|| taffy_tree.new_with_children(column_style, &flow_children).ok())
                    .flatten();
                if let Ok(rail) = taffy_tree.new_with_children(
                    Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Column,
                        // Inline-end alignment: every float hugs the rail's
                        // outer edge (right floats hug right).
                        align_items: Some(if float_right { AlignItems::FLEX_END } else { AlignItems::FLEX_START }),
                        ..Default::default()
                    },
                    &rail_children,
                ) {
                    let row_children: Vec<taffy::tree::NodeId> = match (float_right, column) {
                        (true, Some(col)) => vec![col, rail],
                        (true, None) => vec![rail],
                        (false, Some(col)) => vec![rail, col],
                        (false, None) => vec![rail],
                    };
                    let row_style = Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Row,
                        align_items: Some(AlignItems::FLEX_START),
                        size: Size { width: percent(1.0), height: auto() },
                        ..Default::default()
                    };
                    if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
                        direct.push(row);
                    } else {
                        for n in row_children {
                            direct.push(n);
                        }
                    }
                } else {
                    // Degenerate rail failure: never lose content.
                    for n in rail_children {
                        direct.push(n);
                    }
                    for n in flow_children {
                        direct.push(n);
                    }
                }
                cursor = zone_end;
                continue;
            }
        }

        if run_len >= 2 {
            // --- 8c: the wrapping float-grid row -------------------------
            let mut row_children: Vec<taffy::tree::NodeId> = Vec::new();
            for i in float_idx..run_end {
                if !is_float_child(&child_ids[i]) {
                    continue;
                }
                if let Some(f) = build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                    row_children.push(f);
                }
            }
            let row_style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                align_items: Some(AlignItems::FLEX_START),
                // The row IS the band's inline size: definite width so
                // percentage-width floats wrap against the real container,
                // not an intrinsic pre-stretch guess.
                size: Size { width: percent(1.0), height: auto() },
                ..Default::default()
            };
            if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
                direct.push(row);
            }
            // The walk resumes after the run: another float starts its own
            // zone/run, cleared or plain siblings take normal flow.
            cursor = run_end;
            continue;
        }

        // --- 8b: single float + flow column ------------------------------
        // Zone end: the first sibling that clears this float's side (the
        // clearfix idiom), or the next float, or the point where the flow
        // siblings have already filled an ESTIMATE of the float's height
        // (8g) — see zone_end_at_budget.
        let float_height_budget = estimate_float_height(tree, styles, child_ids[float_idx]);
        let zone_end = zone_end_at_budget(tree, styles, &child_ids, float_idx + 1, float_height_budget, run_side);
        // Build the float itself (blockified into the row's first item).
        let float_dom = child_ids[float_idx];
        let float_taffy =
            build_element(tree, float_dom, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers);
        // The flow column: an ANONYMOUS block wrapper around every in-zone
        // sibling built normally inside it (see build_flow_column).
        let flow_children = build_flow_column(
            &child_ids[float_idx + 1..zone_end],
            tree,
            styles,
            images,
            fonts,
            taffy_tree,
            node_map,
            flattened,
            run_wrappers,
            font_size,
            lh_elem,
        );
        let float_right = styles.get(&float_dom)
            .and_then(|s| s.float_side)
            == Some(crate::diting_css::FloatSide::Right);
        let mut row_children: Vec<taffy::tree::NodeId> = Vec::new();
        if !flow_children.is_empty() {
            if let Ok(column) = taffy_tree.new_with_children(
                Style {
                    display: Display::Block,
                    flex_grow: 1.0,
                    flex_shrink: 1.0,
                    flex_basis: Dimension::length(0.0),
                    min_size: Size { width: LengthPercentageAuto::length(0.0), height: LengthPercentageAuto::auto() },
                    ..Default::default()
                },
                &flow_children,
            ) {
                row_children.push(column);
            }
        }
        // A right float hugs the container's right edge — the row's LAST
        // item (CSS places right floats at the inline-end).
        if float_right {
            row_children.extend(float_taffy);
        } else {
            for f in float_taffy.into_iter().rev() {
                row_children.insert(0, f);
            }
        }
        let row_style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: Some(AlignItems::FLEX_START),
            // With an empty flow column the float is the row's ONLY item —
            // flex-end keeps a lone right float at the inline-end instead of
            // parking it at flex-start (column-present rows have no free
            // space, so this is a no-op there).
            justify_content: float_right.then_some(JustifyContent::FLEX_END),
            size: Size { width: percent(1.0), height: auto() },
            ..Default::default()
        };
        if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
            direct.push(row);
        }
        // The walk resumes after the zone: zone-external siblings (clear,
        // later floats) take the next loop turn at their document position.
        cursor = zone_end;
        }
    } else {
    for child in child_ids {
        let is_text = tree.with_node(child, |n| n.is_text()).unwrap_or(false);
        let child_tag = tree
            .with_node(child, |n| n.as_element().map(|e| e.local.to_string()))
            .flatten()
            .unwrap_or_default();
        let child_display = styles
            .get(&child)
            .and_then(|s| s.display)
            .or(if is_text { Some(CssDisplay::Inline) } else { Some(CssDisplay::Block) });
        let inline_level = matches!(child_display, Some(CssDisplay::Inline));
        // Out-of-flow children (CSS blockification of abspos) never flatten
        // into an inline run — the reparent pass in layout_dom will move them
        // to their containing block anyway.
        let out_of_flow = styles.get(&child).is_some_and(|s| {
            matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
        });
        if !is_text && is_replaced_tag(&child_tag) {
            // Replaced elements are atomic: an inline-level box inside a run
            // (like a fat word), a direct item inside flex/grid or when the
            // UA/author made it block-level (our ua_display keeps img block).
            let leaf = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map);
            if let Some(leaf) = leaf {
                let inline_atom =
                    inline_level || child_display == Some(CssDisplay::InlineBlock);
                if inline_atom && !atomic_container && !out_of_flow {
                    run.push(RunSeg::Nodes(vec![leaf]));
                } else {
                    flush_run(&mut run, &mut direct, taffy_tree, run_wrappers);
                    direct.push(leaf);
                }
            }
            continue;
        }
        if is_text {
            let text = tree.with_node(child, |n| n.text_content_of_text_node().unwrap_or("").to_string()).unwrap_or_default();
            let (fs, b, lh) = font_context(tree, child, styles);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { lh_elem };
            // First segment's node donates the whole run's color — the same
            // first-segment approximation fs/bold already use.
            let col = color_context(tree, child, styles);
            run.push(RunSeg::Text(text, fs, b, col, lh));
        } else if child_display == Some(CssDisplay::InlineBlock) && !atomic_container && !out_of_flow {
            // An inline-level block is ATOMIC (Chrome line-box model): it keeps
            // its own subtree box and joins the run as one unit, like a replaced
            // leaf. Flattening it would let its content wrap across the parent's
            // line; keeping the box makes taffy size it shrink-to-fit
            // (min(max-content, available)) against the run — and our wrapping
            // runs can't build the overflow bomb upstream's NoWrap rows did
            // (obscura#750).
            if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                run.push(RunSeg::Nodes(vec![sub]));
            }
        } else if inline_level && !atomic_container && !out_of_flow {
            // A plain inline wrapper flattens into the enclosing run (upstream
            // is_flattenable_inline): the words wrap at the real block level.
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                run.push(RunSeg::Nodes(sub_children.clone()));
                node_map.remove(&sub);
                let _ = taffy_tree.remove(sub);
                // Boxless after hoisting — union pass rebuilds the rect.
                if !sub_children.is_empty() {
                    flattened.insert(child, sub_children);
                }
            }
        } else {
            flush_run(&mut run, &mut direct, taffy_tree, run_wrappers);
            if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers) {
                direct.push(node);
            }
        }
    }
    }
    flush_run(&mut run, &mut direct, taffy_tree, run_wrappers);
    // Sibling margin normalization runs over the FINAL child list of every
    // container (see collapse_adjacent_sibling_margins: fixes the
    // border/padding separation rule taffy's native block collapse lacks,
    // and supplies collapsing for flex stand-in parents).
    collapse_adjacent_sibling_margins(taffy_tree, styles, node_map, &direct);

    let taffy_style = to_taffy_style(&style);
    let node = if direct.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &direct).ok()?
    };
    node_map.insert(node, id);
    resolve_sizing_keywords(taffy_tree, node, &style, fonts);
    Some(node)
}

/// One paint primitive in document order (batch 4a) — the minimal output
/// contract between layout and paint. `Bg` is an element's solid
/// background-color over its border-box (rounded per border-radius, batch
/// 6b); `Text` is a text run at its leaf origin, re-wrapped at the width
/// its containing block gave it at measure time (mixed runs paint
/// per-word leaves at their own boxes, batch 4d). Shadows and elliptical/
/// per-corner radii don't exist yet in this slice.
#[derive(Debug, Clone)]
pub enum PaintItem {
    Bg { rect: Rect, color: [u8; 4], radius: f32 },
    /// Per-corner radii variant of `Bg` (batch 7c): CSS corner order
    /// (TL TR BR BL), each (rx, ry) already resolved to px.
    BgCorner { rect: Rect, color: [u8; 4], radii: [(f32, f32); 4] },
    /// A decoded raster image blitted into the replaced box (batch 5b):
    /// sized per object-fit and offset per object-position (batch 5c) —
    /// see [`object_paint_rect`]. `rect` is the element box and doubles as
    /// the clip (replaced content never escapes it); `paint_rect` is the
    /// computed blit destination.
    Image {
        rect: Rect,
        paint_rect: Rect,
        image: DecodedImage,
    },
    /// A replaced element's placeholder (batch 5a): an optional gray box
    /// (skipped when the author styled a background — that already shows)
    /// plus the alt text run, already resolved to font/color context and
    /// wrapped inside the box at paint time. Upstream blitz paints
    /// nothing for an unloaded img (`draw_image` is a no-op without
    /// raster data), so the placeholder is OUR no-network product policy,
    /// locked by structural tests; the cross-checked part is the shared
    /// box geometry (batch 2 rects) and the img's own CSS background.
    Replaced {
        rect: Rect,
        /// (text, font_size, bold, line_height, color) of the alt run; None
        /// without an alt attribute (present-but-empty paints box-only, like
        /// alt="").
        alt: Option<(String, f32, bool, f32, [u8; 4])>,
        fill_placeholder: bool,
    },
    /// Begin clipping descendants to `rect` (the padding box of an
    /// overflow-clipping element) until the matching [`PaintItem::PopClip`].
    /// The flat item list carries the tree's clip structure in document
    /// order; nesting is an intersection.
    Clip { rect: Rect },
    /// Rounded variant of `Clip` (batch 7d): the clipping box's own radii
    /// cut descendants' ink along the curve, like upstream's rounded
    /// padding-box BezPath clip.
    ClipRounded { rect: Rect, radii: [(f32, f32); 4] },
    PopClip,
    /// A uniform solid border: four bands on the border-box edges, painted
    /// AFTER the element's Bg (background-clip: border-box draws the bg
    /// beneath the border) and before the subtree. Widths in CSS order
    /// (top right bottom left), px.
    Border {
        rect: Rect,
        widths: [f32; 4],
        color: [u8; 4],
    },
    Text {
        text: String,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        /// Used line-height (px) — the leaf's own measure-time value, so the
        /// paint baselines land exactly where layout placed the line boxes.
        line_height: f32,
        /// Leaf origin (line-box top-left), page px.
        x: f32,
        y: f32,
        /// The wrap width the containing block offered at measure time.
        wrap_at: f32,
    },
}

/// Lay a DOM tree out at a fixed viewport size and return each element's
/// absolute border-box rect. Elements are keyed by diting_dom NodeId; the
/// root's containing block is the viewport (a definite-sized root taffy
/// node, so bottom/percentage insets on fixed boxes resolve against it —
/// obscura#675 lineage fix).
pub fn layout_dom(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
) -> HashMap<NodeId, Rect> {
    layout_dom_with_paint(tree, styles, fonts, viewport_width, viewport_height).0
}

/// [`layout_dom`] plus the paint item list in document order (an element's
/// `Bg` precedes the items of its subtree, so a solid background lands
/// under its descendants' ink).
pub fn layout_dom_with_paint(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>) {
    layout_dom_with_paint_and_images(tree, styles, fonts, viewport_width, viewport_height, None)
}

/// [`layout_dom_with_paint`] with an injected table of fetched image bodies
/// (batch 6c): absolute `http(s)` URL → response body. `<img src>` entries
/// pointing at those URLs decode like data: URLs (PNG only); misses keep
/// the placeholder. The fetch itself is the caller's job — the screenshot
/// prefetch pass fills this from diting_net.
pub fn layout_dom_with_paint_and_images(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
    network_bytes: Option<&HashMap<String, Vec<u8>>>,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>) {
    let (rects, items, _order) = layout_dom_with_paint_order_and_images(
        tree,
        styles,
        fonts,
        viewport_width,
        viewport_height,
        network_bytes,
    );
    (rects, items)
}

/// Baseline of a text line box below its top edge, from real font metrics
/// (the same quantized model the paint path rasterizes against).
fn text_baseline(fonts: &FontBook, font_size: f32, bold: bool, line_height: f32) -> f32 {
    let m = fonts.metrics(font_size, bold).unwrap_or(text::ScaledMetrics {
        ascent: font_size,
        descent: font_size * 0.2,
        line_gap: 0.0,
    });
    text::baseline_offset(m.ascent, m.descent, line_height)
}

/// The LAST in-flow line baseline inside an atomic inline-level box (CSS2:
/// an inline-block baseline-aligns to the baseline of its last in-flow line
/// box), as a y offset from the box's border-box top. `rel_y` accumulates
/// taffy locations plus already-computed baseline shifts (inner runs are
/// processed before outer ones). Returns None when the subtree carries no
/// text — the caller then falls back to the bottom edge, which is the CSS
/// behavior for a box with no in-flow line boxes.
fn subtree_last_baseline(
    taffy_tree: &TaffyTree<TextLeaf>,
    node: taffy::tree::NodeId,
    rel_y: f32,
    shifts: &HashMap<taffy::tree::NodeId, f32>,
    fonts: &FontBook,
    best: &mut Option<f32>,
) {
    let Ok(layout) = taffy_tree.layout(node) else { return };
    let rel_y = rel_y + shifts.get(&node).copied().unwrap_or(0.0);
    match taffy_tree.get_node_context(node) {
        Some(TextLeaf::Word { font_size, bold, line_height, .. }) => {
            if layout.size.height > 0.0 {
                let b = rel_y + text_baseline(fonts, *font_size, *bold, *line_height);
                *best = Some(best.map_or(b, |x: f32| x.max(b)));
            }
            return;
        }
        Some(TextLeaf::Run { font_size, bold, line_height, .. }) => {
            // A wrapped run's last line sits one line box above its bottom.
            let b = rel_y + layout.size.height - line_height
                + text_baseline(fonts, *font_size, *bold, *line_height);
            *best = Some(best.map_or(b, |x: f32| x.max(b)));
            return;
        }
        None => {}
    }
    for child in taffy_tree.children(node).unwrap_or_default() {
        let Ok(cl) = taffy_tree.layout(child) else { continue };
        subtree_last_baseline(taffy_tree, child, rel_y + cl.location.y, shifts, fonts, best);
    }
}

/// Baseline alignment for inline runs (blitz#750 family): CSS aligns the
/// boxes of one line by their baselines, but taffy leaves cannot report
/// baselines (the measure closure returns Size only — still true at the
/// taffy rev blitz's own baseline PR pins), so run wrappers lay out
/// FLEX_START and we shift afterwards. Wrapping is width-driven, so the
/// flex lines taffy already formed ARE the CSS line boxes; per line every
/// item drops by (line max baseline − its own baseline). For uniform-font
/// runs every dy is zero, leaving existing geometry bit-identical — only
/// runs mixing font metrics or atomic boxes move.
fn compute_baseline_shifts(
    taffy_tree: &TaffyTree<TextLeaf>,
    run_wrappers: &[taffy::tree::NodeId],
    fonts: &FontBook,
) -> HashMap<taffy::tree::NodeId, f32> {
    let mut shifts: HashMap<taffy::tree::NodeId, f32> = HashMap::new();
    // First-line baselines of processed wrappers: a nested wrapper (flattened
    // inline content) joins an outer run as ONE item and, as a flex
    // container, contributes its FIRST baseline (css-align, blitz#750's
    // Display::Flex arm).
    let mut first_baselines: HashMap<taffy::tree::NodeId, f32> = HashMap::new();
    // Inner wrappers first: outer runs read their shifts and first baselines.
    let depth = |mut n: taffy::tree::NodeId| {
        let mut d = 0usize;
        while let Some(p) = taffy_tree.parent(n) {
            d += 1;
            n = p;
        }
        d
    };
    let mut ordered = run_wrappers.to_vec();
    ordered.sort_by_key(|w| std::cmp::Reverse(depth(*w)));

    for wrapper in ordered {
        let children = taffy_tree.children(wrapper).unwrap_or_default().to_vec();
        if children.len() < 2 {
            // A lone item aligns to itself; nothing to shift. Still record
            // the first baseline for an enclosing run.
            if let Some(&only) = children.first() {
                if let Ok(l) = taffy_tree.layout(only) {
                    if l.size.height > 0.0 {
                        let b = item_baseline(taffy_tree, only, &shifts, &first_baselines, fonts, l.size.height);
                        first_baselines.insert(wrapper, b);
                    }
                }
            }
            continue;
        }
        // Group into flex lines by y (taffy rounds locations to the pixel
        // grid, so same-line items share a y).
        let mut lines: Vec<(f32, Vec<(taffy::tree::NodeId, f32)>)> = Vec::new();
        for child in children {
            let Ok(l) = taffy_tree.layout(child) else { continue };
            // Whitespace-only word leaves carry no height and no ink.
            if l.size.height <= 0.0 {
                continue;
            }
            let b = item_baseline(taffy_tree, child, &shifts, &first_baselines, fonts, l.size.height);
            match lines.iter_mut().find(|(y, _)| (*y - l.location.y).abs() < 0.5) {
                Some((_, items)) => items.push((child, b)),
                None => lines.push((l.location.y, vec![(child, b)])),
            }
        }
        lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((_, items)) = lines.first() {
            let b = items.iter().fold(0.0f32, |m, (_, b)| m.max(*b));
            first_baselines.insert(wrapper, b);
        }
        for (_, items) in lines {
            let line_b = items.iter().fold(0.0f32, |m, (_, b)| m.max(*b));
            for (child, b) in items {
                let dy = line_b - b;
                if dy > 0.01 {
                    shifts.insert(child, dy);
                }
            }
        }
    }
    shifts
}

/// Baseline of one run item below its border-box top.
fn item_baseline(
    taffy_tree: &TaffyTree<TextLeaf>,
    child: taffy::tree::NodeId,
    shifts: &HashMap<taffy::tree::NodeId, f32>,
    first_baselines: &HashMap<taffy::tree::NodeId, f32>,
    fonts: &FontBook,
    height: f32,
) -> f32 {
    match taffy_tree.get_node_context(child) {
        Some(TextLeaf::Word { font_size, bold, line_height, .. }) => {
            text_baseline(fonts, *font_size, *bold, *line_height)
        }
        Some(TextLeaf::Run { font_size, bold, line_height, .. }) => {
            height - line_height + text_baseline(fonts, *font_size, *bold, *line_height)
        }
        None => {
            if let Some(b) = first_baselines.get(&child) {
                return *b;
            }
            if taffy_tree.children(child).map_or(0, |c| c.len()) > 0 {
                // Atomic inline-level box: last in-flow line baseline.
                let mut best = None;
                subtree_last_baseline(taffy_tree, child, 0.0, shifts, fonts, &mut best);
                if let Some(b) = best {
                    return b;
                }
            }
            // Replaced leaf or textless box: bottom edge (CSS bottom
            // margin-edge fallback — run items carry no vertical margins in
            // this model).
            height
        }
    }
}

/// [`layout_dom_with_paint_and_images`] plus the paint order: every boxed
/// element in the sequence the flat item list paints it, so the LAST entry
/// whose rect contains a point is the element a pixel there shows. This is
/// the ranking `elementFromPoint` needs — document order is not paint order
/// once positioned siblings with z-index hoist out of it (obscura #738).
/// Boxless flattened inline wrappers (span/a/label — obscura#722 lineage)
/// are absent; hit testing ranks them with their nearest boxed ancestor,
/// which paints their hoisted ink in-flow anyway.
pub fn layout_dom_with_paint_order_and_images(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
    network_bytes: Option<&HashMap<String, Vec<u8>>>,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>, Vec<NodeId>) {
    let mut taffy_tree = TaffyTree::new();
    let mut node_map: HashMap<taffy::tree::NodeId, NodeId> = HashMap::new();

    // The document node is not an element; lay out from the first element
    // descendant (the <html> root), like upstream.
    let root = tree
        .children(tree.document())
        .into_iter()
        .find(|id| tree.with_node(*id, |n| n.is_element()).unwrap_or(false));

    // Pre-pass (batches 5b + 6c): resolve every img src through the
    // ImageCache — data: URLs decode inline, http(s) URLs consult the
    // fetched-byte table; results are cached so repeated srcs and layout
    // re-runs decode once. Unresolvable imgs keep the batch-5a placeholder.
    let empty: HashMap<String, Vec<u8>> = HashMap::new();
    let cache = image::ImageCache::with_network(network_bytes.unwrap_or(&empty));
    let mut images: HashMap<NodeId, DecodedImage> = HashMap::new();
    fn scan_images(
        tree: &DomTree,
        id: NodeId,
        cache: &image::ImageCache,
        images: &mut HashMap<NodeId, DecodedImage>,
        viewport_width: f32,
    ) {
        let is_img = tree
            .with_node(id, |n| n.as_element().map(|e| e.local.to_string() == "img"))
            .flatten()
            .unwrap_or(false);
        if is_img {
            let src = resolve_img_source(tree, id, viewport_width);
            if let Some(img) = src.as_deref().and_then(|s| cache.resolve(s)) {
                images.insert(id, (*img).clone());
            }
        }
        for child in tree.children(id) {
            scan_images(tree, child, cache, images, viewport_width);
        }
    }
    if let Some(root_id) = &root {
        scan_images(tree, *root_id, &cache, &mut images, viewport_width);
    }

    let mut rects = HashMap::new();
    let mut items: Vec<PaintItem> = Vec::new();
    // Paint sequence of the boxed elements (obscura #738): filled by the
    // `collect` walk, sibling bands already z-sorted. See the doc on
    // [`layout_dom_with_paint_order_and_images`].
    let mut paint_order: Vec<NodeId> = Vec::new();
    // Flattened inline wrappers (see build_element) recorded as
    // dom id → hoisted taffy children, for the union pass after the walk.
    let mut flattened: HashMap<NodeId, Vec<taffy::tree::NodeId>> = HashMap::new();
    // Run wrappers (the IFC stand-in flex rows) recorded at assembly for the
    // post-layout baseline-alignment pass (blitz#750 family) — they can't be
    // re-identified structurally (inline-block boxes share the same taffy
    // style), so the builders log them as they create them.
    let mut run_wrappers: Vec<taffy::tree::NodeId> = Vec::new();
    let Some(root_id) = root else { return (rects, items, paint_order) };
    let Some(root_node) = build_element(
        tree,
        root_id,
        styles,
        &images,
        fonts,
        &mut taffy_tree,
        &mut node_map,
        &mut flattened,
        &mut run_wrappers,
    ) else {
        return (rects, items, paint_order);
    };

    // The initial containing block (obscura#675 lineage fix): CSS anchors a
    // fixed box's insets — and every percentage that bottoms out at the root
    // — to the VIEWPORT, a notional box DISTINCT from the root element's own
    // content-driven box (blitz keeps html at content height too; conflating
    // the two made the cross-check read html=600 vs blitz=140). Wrap the
    // root element in a synthetic definite viewport-sized taffy node; the
    // reparent pass below lands fixed boxes — and ancestor-less absolutes,
    // whose containing block is also the ICB — on it, so taffy's inset math
    // has a real viewport to resolve against. The wrapper has no DOM node,
    // so collect/paint walks keyed by node_map skip it. Children taller
    // than the viewport overflow visibly; taffy doesn't clip.
    let icb_node = taffy_tree
        .new_leaf(Style {
            // Block, not taffy's Flex default: the root element must stay a
            // stretching BLOCK child (a flex item would shrink html to
            // max-content — probe: 500px child → html=500 instead of VW).
            display: Display::Block,
            size: Size {
                width: taffy::style::Dimension::length(viewport_width),
                height: taffy::style::Dimension::length(viewport_height),
            },
            ..Default::default()
        })
        .expect("synthetic ICB node");
    let _ = taffy_tree.add_child(icb_node, root_node);

    let available = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::MaxContent,
    };

    // --- static-position harvest (blitz#764 review point) ----------------
    // An out-of-flow box with BOTH insets auto on an axis resolves at its
    // static position: where it would have been in its original flow. The
    // reparent pass below moves the box to its containing block first, and
    // taffy's auto-inset fallback then uses the flow position inside its
    // CURRENT taffy parent - the CB - so the box lands after the CB's last
    // in-flow child instead of at its DOM-parent flow spot. Harvest by
    // laying the tree out once BEFORE reparenting: with the boxes still
    // absolute children of their DOM parents, taffy's auto-inset fallback
    // IS the CSS static position (its flow spot among the original
    // siblings, which per CSS is computed with every OTHER out-of-flow box
    // still out of flow - exactly what an absolute child sees). No style
    // mutation, so the main layout below starts from a clean cache.
    let mut static_pos: HashMap<NodeId, (f32, f32)> = HashMap::new();
    {
        let needs_static: Vec<(taffy::tree::NodeId, NodeId)> = node_map
            .iter()
            .filter(|(_, dom_id)| {
                styles.get(*dom_id).is_some_and(|s| {
                    matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
                        && ((s.left.is_none() && s.right.is_none())
                            || (s.top.is_none() && s.bottom.is_none()))
                })
            })
            .map(|(t, d)| (*t, *d))
            .collect();
        if !needs_static.is_empty() {
            let laid_out = taffy_tree
                .compute_layout_with_measure(icb_node, available, |inputs, _id, ctx, style| {
                    match ctx {
                        Some(TextLeaf::Run { text, font_size, bold, line_height, .. }) => {
                            measure_text_leaf(text, *font_size, *bold, *line_height, fonts, &inputs)
                        }
                        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                    }
                })
                .is_ok();
            if laid_out {
                fn abs_rects(
                    taffy_tree: &TaffyTree<TextLeaf>,
                    node: taffy::tree::NodeId,
                    offset: (f32, f32),
                    out: &mut HashMap<taffy::tree::NodeId, Rect>,
                ) {
                    let Ok(layout) = taffy_tree.layout(node) else { return };
                    let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
                    out.insert(node, Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height });
                    for child in taffy_tree.children(node).unwrap_or_default() {
                        abs_rects(taffy_tree, child, abs, out);
                    }
                }
                let mut now: HashMap<taffy::tree::NodeId, Rect> = HashMap::new();
                abs_rects(&taffy_tree, icb_node, (0.0, 0.0), &mut now);
                for (tnid, dom_id) in &needs_static {
                    if let Some(r) = now.get(tnid) {
                        static_pos.insert(*dom_id, (r.x, r.y));
                    }
                }
            }
        }
    }

    // Absolute/fixed reparent pass (upstream's containing-block fix-up):
    // taffy resolves an absolute child against its DIRECT taffy parent, so
    // move each out-of-flow box to its CSS containing block — the nearest
    // ancestor with position != static; fixed (and no positioned ancestor)
    // resolves to the root = the initial containing block stand-in.
    {
        let dom_of: HashMap<NodeId, taffy::tree::NodeId> =
            node_map.iter().map(|(k, v)| (*v, *k)).collect();
        let mut reparents: Vec<(taffy::tree::NodeId, taffy::tree::NodeId)> = node_map
            .iter()
            .filter_map(|(taffy_nid, dom_id)| {
                let style = styles.get(dom_id)?;
                let fixed = style.position == Some(PositionMode::Fixed);
                if style.position != Some(PositionMode::Absolute) && !fixed {
                    return None;
                }
                let target_dom = if fixed {
                    None
                } else {
                    let mut cur = tree.with_node(*dom_id, |n| n.parent).flatten();
                    while let Some(nid) = cur {
                        let positioned = styles.get(&nid).is_some_and(|s| {
                            matches!(
                                s.position,
                                Some(PositionMode::Relative)
                                    | Some(PositionMode::Absolute)
                                    | Some(PositionMode::Fixed)
                            )
                        });
                        if positioned {
                            break;
                        }
                        cur = tree.with_node(nid, |n| n.parent).flatten();
                    }
                    cur
                };
                let target = target_dom.and_then(|d| dom_of.get(&d)).copied().unwrap_or(icb_node);
                let current = taffy_tree.parent(*taffy_nid)?;
                if current != target {
                    Some((*taffy_nid, target))
                } else {
                    None
                }
            })
            .collect();
        // Append order must be DOCUMENT order: node_map is a HashMap, and
        // add_child appends — iterating it directly scrambles sibling order
        // after reparenting (paint order then diverges from both blitz and
        // the DOM). Sort the reparented nodes by their pre-order rank.
        let mut rank: HashMap<NodeId, usize> = HashMap::new();
        fn preorder(tree: &DomTree, id: NodeId, rank: &mut HashMap<NodeId, usize>, ctr: &mut usize) {
            rank.insert(id, *ctr);
            *ctr += 1;
            for child in tree.children(id) {
                preorder(tree, child, rank, ctr);
            }
        }
        {
            let mut ctr = 0usize;
            if let Some(root_id) = root {
                preorder(tree, root_id, &mut rank, &mut ctr);
            }
        }
        reparents.sort_by_key(|(taffy_nid, _)| {
            node_map
                .get(taffy_nid)
                .and_then(|d| rank.get(d))
                .copied()
                .unwrap_or(usize::MAX)
        });
        for (node, target) in reparents {
            if let Some(old) = taffy_tree.parent(node) {
                let _ = taffy_tree.remove_child(old, node);
            }
            let _ = taffy_tree.add_child(target, node);
        }
    }
    // Measured-leaf dispatch (batch 3a): pure-text runs carry a TextLeaf
    // context and measure through the FontBook; every other leaf must fall
    // through to taffy's own style-based sizing — the closure fires for ALL
    // childless nodes, so returning HIDDEN here would zero plain leaves
    // (that's exactly what stock compute_layout does below via the same fn).
    let measured = taffy_tree.compute_layout_with_measure(icb_node, available, |inputs, _id, ctx, style| {
        match ctx {
            Some(TextLeaf::Run { text, font_size, bold, line_height, .. }) => {
                measure_text_leaf(text, *font_size, *bold, *line_height, fonts, &inputs)
            }
            // Word leaves keep their style-driven sizing (batch 4d only
            // added paint context — zero layout change).
            Some(TextLeaf::Word { .. }) | None => {
                taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
            }
        }
    });
    if measured.is_err() {
        return (rects, items, paint_order);
    }

    // --- float continuation (batch 8g) -----------------------------------
    // A float excludes in-flow content not only among its DIRECT siblings:
    // blocks further down the DOM (later siblings of its parent, and of
    // grand-parents, up to the nearest block formatting context) must also
    // shorten their lines where they intersect the float's band. The tree
    // build can't know those rectangles yet, so — like upstream
    // apply_float_continuations — this runs after a first layout: collect
    // every float's real band, narrow the intersecting LATER blocks by
    // clamping their max-width (left float: shrink the right side; right
    // float: push the left edge past the float), then lay out again.
    {
        let dom_of: HashMap<NodeId, taffy::tree::NodeId> =
            node_map.iter().map(|(k, v)| (*v, *k)).collect();
        let mut rects_now: HashMap<taffy::tree::NodeId, Rect> = HashMap::new();
        fn abs_rects(
            taffy_tree: &TaffyTree<TextLeaf>,
            node: taffy::tree::NodeId,
            offset: (f32, f32),
            out: &mut HashMap<taffy::tree::NodeId, Rect>,
        ) {
            let Ok(layout) = taffy_tree.layout(node) else { return };
            let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
            out.insert(node, Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height });
            for child in taffy_tree.children(node).unwrap_or_default() {
                abs_rects(taffy_tree, child, abs, out);
            }
        }
        abs_rects(&taffy_tree, icb_node, (0.0, 0.0), &mut rects_now);

        // Float bands with the DOM node that owns each float.
        let mut bands: Vec<(NodeId, taffy::tree::NodeId, Rect)> = Vec::new();
        for (tnid, dom_id) in node_map.iter() {
            let is_float = styles.get(dom_id).is_some_and(|s| s.float_side.is_some());
            if !is_float {
                continue;
            }
            if let Some(r) = rects_now.get(tnid) {
                bands.push((*dom_id, *tnid, *r));
            }
        }
        if !bands.is_empty() {
            let mut changed = false;
            for (float_dom, fnode, fr) in &bands {
                let side = styles.get(float_dom).and_then(|s| s.float_side).unwrap_or(crate::diting_css::FloatSide::Left);
                let Some(parent_dom) = tree.with_node(*float_dom, |n| n.parent).flatten() else { continue };
                let float_taffy_parent = taffy_tree.parent(*fnode);
                // Walk from the float's parent upward; narrow every LATER
                // sibling block until we hit a BFC stand-in. Blocks inside
                // the float's OWN zone row (the 8b flow column) share the
                // float's taffy parent — they are the content the float
                // already excludes by construction, so skip them.
                let mut below = *float_dom;
                let mut cur = parent_dom;
                loop {
                    // A grid/flex container establishes an independent
                    // formatting context: its children are grid/flex items
                    // that CSS floats can never displace. Vector 2022 parks
                    // floated navboxes deep inside the article's mw-body
                    // grid — without this stop the climb escapes the grid
                    // and narrows the page's TOC column to max-width 0.
                    if styles.get(&cur).is_some_and(|s| {
                        matches!(s.display, Some(CssDisplay::Grid) | Some(CssDisplay::Flex))
                    }) {
                        break;
                    }
                    // cur's taffy box shares the float's taffy parent — both
                    // sit inside the same synthetic 8b/8c/8d row, whose
                    // contents the float already excludes. Stop climbing.
                    if let Some(&cnid) = dom_of.get(&cur) {
                        if taffy_tree.parent(cnid) == float_taffy_parent {
                            break;
                        }
                    }
                    let siblings = tree.children(cur);
                    // Floats displace only content that FOLLOWS them in
                    // document order: siblings BEFORE the float's branch
                    // keep their full width (their boxes predate the float's
                    // band). Narrowing them would, e.g., shrink every
                    // section above an article-bottom navbox float.
                    let after = match siblings.iter().position(|s| *s == below) {
                        Some(i) => &siblings[i + 1..],
                        None => &siblings[..],
                    };
                    for sib in after {
                        let Some(&snid) = dom_of.get(sib) else { continue };
                        // Anything whose taffy ANCESTRY runs through the
                        // float's own synthetic row (the 8b flow column
                        // wraps the zone's blocks) is content the float
                        // already excludes — never narrow it.
                        let mut anc = taffy_tree.parent(snid);
                        let mut inside_zone_row = false;
                        while let Some(a) = anc {
                            if Some(a) == float_taffy_parent {
                                inside_zone_row = true;
                                break;
                            }
                            anc = taffy_tree.parent(a);
                        }
                        if inside_zone_row {
                            continue;
                        }
                        let Some(sr) = rects_now.get(&snid) else { continue };
                        // Intersect test against this float's vertical band.
                        let overlaps_y = sr.y < fr.y + fr.height && sr.y + sr.height > fr.y;
                        if !overlaps_y || sr.width <= 0.0 {
                            continue;
                        }
                        let is_float_sib = styles.get(&sib).is_some_and(|s| s.float_side.is_some());
                        if is_float_sib {
                            continue;
                        }
                        // Out-of-flow boxes are never pushed by a float
                        // (CSS: floats only displace in-flow content).
                        let out_of_flow = styles.get(&sib).is_some_and(|s| {
                            matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
                        });
                        if out_of_flow {
                            continue;
                        }
                        match side {
                            crate::diting_css::FloatSide::Left => {
                                let want_right = fr.x + fr.width;
                                let inset = (want_right - sr.x).max(0.0);
                                let avail = (sr.width - inset).max(0.0);
                                if avail < sr.width - 0.5 {
                                    if let Ok(mut st) = taffy_tree.style(snid).cloned() {
                                        st.max_size.width =
                                            LengthPercentageAuto::length(avail.max(0.0));
                                        let _ = taffy_tree.set_style(snid, st);
                                        changed = true;
                                    }
                                }
                            }
                            crate::diting_css::FloatSide::Right => {
                                let float_left = fr.x;
                                let inset = (sr.x + sr.width - float_left).max(0.0);
                                let avail = (sr.width - inset).max(0.0);
                                if avail < sr.width - 0.5 {
                                    if let Ok(mut st) = taffy_tree.style(snid).cloned() {
                                        st.margin.left =
                                            LengthPercentageAuto::length(fr.x - sr.x);
                                        st.max_size.width =
                                            LengthPercentageAuto::length(avail.max(0.0));
                                        let _ = taffy_tree.set_style(snid, st);
                                        changed = true;
                                    }
                                }
                            }
                        }
                    }
                    below = cur;
                    let Some(grand) = tree.with_node(cur, |n| n.parent).flatten() else { break };
                    cur = grand;
                }
            }
            if changed {
                let re = taffy_tree.compute_layout_with_measure(
                    icb_node,
                    available,
                    |inputs, _id, ctx, style| match ctx {
                        Some(TextLeaf::Run { text, font_size, bold, line_height, .. }) => {
                            measure_text_leaf(text, *font_size, *bold, *line_height, fonts, &inputs)
                        }
                        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                    },
                );
                if re.is_err() {
                    return (rects, items, paint_order);
                }
            }
        }
    }
    // calc() repair pass (obscura#767 family): a mixed percent+px calc on
    // width/min/max rode in as a percent-only placeholder (taffy has no
    // "percent + px" Dimension shape). Now that the layout has settled,
    // resolve each placeholder exactly ONCE against its containing block's
    // content-box width — CSS never re-samples a resolved value against a
    // re-sampled basis, so this single deterministic pass replaces the
    // value instead of feeding it back through another percent resolution.
    // Sorted shallow-first, a calc width inside a calc-width parent
    // cascades through the arithmetic `repaired` map without another layout
    // round-trip. height and the margin/padding/inset slots keep the
    // percent-only placeholder (documented approximation).
    {
        let has_calc_width = |s: &crate::diting_css::ComputedStyle| {
            matches!(s.width, Some(crate::diting_css::Length::Calc { .. }))
                || matches!(s.min_width, Some(crate::diting_css::Length::Calc { .. }))
                || matches!(s.max_width, Some(crate::diting_css::Length::Calc { .. }))
        };
        let mut fixups: Vec<(taffy::tree::NodeId, NodeId)> = node_map
            .iter()
            .filter(|(_, d)| styles.get(*d).is_some_and(has_calc_width))
            .map(|(t, d)| (*t, *d))
            .collect();
        if !fixups.is_empty() {
            fn taffy_depth(
                taffy_tree: &TaffyTree<TextLeaf>,
                node: taffy::tree::NodeId,
                icb: taffy::tree::NodeId,
            ) -> usize {
                let (mut d, mut cur) = (0usize, node);
                while cur != icb {
                    match taffy_tree.parent(cur) {
                        Some(p) => {
                            cur = p;
                            d += 1;
                        }
                        None => break,
                    }
                }
                d
            }
            fixups.sort_by_key(|(t, _)| taffy_depth(&taffy_tree, *t, icb_node));
            let mut repaired: HashMap<taffy::tree::NodeId, f32> = HashMap::new();
            for (tnid, dom_id) in fixups {
                let Some(st) = styles.get(&dom_id) else { continue };
                // Containing block = the taffy parent's settled content box,
                // or the arithmetic repair when the parent itself was a
                // calc-width fixup.
                let Some(cbw) = taffy_tree.parent(tnid).and_then(|p| {
                    if let Some(w) = repaired.get(&p) {
                        return Some(*w);
                    }
                    let l = taffy_tree.layout(p).ok()?;
                    Some(
                        l.size.width
                            - l.padding.left
                            - l.padding.right
                            - l.border.left
                            - l.border.right,
                    )
                }) else {
                    continue;
                };
                // content-box → taffy border-box carry-over, same as
                // to_taffy_style's px arms.
                let infl = side_px(st.padding.left)
                    + side_px(st.padding.right)
                    + if st.border_style.is_some() {
                        side_px(st.border_width.left) + side_px(st.border_width.right)
                    } else {
                        0.0
                    };
                let Some(mut ts) = taffy_tree.style(tnid).ok().cloned() else { continue };
                if let Some(crate::diting_css::Length::Calc { percent, px }) = st.width {
                    let content = (cbw * percent / 100.0 + px).max(0.0);
                    ts.size.width = Dimension::length(content + infl);
                    repaired.insert(tnid, content);
                }
                if let Some(crate::diting_css::Length::Calc { percent, px }) = st.min_width {
                    let content = (cbw * percent / 100.0 + px).max(0.0);
                    ts.min_size.width = LengthPercentageAuto::length(content + infl);
                }
                if let Some(crate::diting_css::Length::Calc { percent, px }) = st.max_width {
                    let content = (cbw * percent / 100.0 + px).max(0.0);
                    ts.max_size.width = LengthPercentageAuto::length(content + infl);
                }
                let _ = taffy_tree.set_style(tnid, ts);
            }
            let re = taffy_tree.compute_layout_with_measure(
                icb_node,
                available,
                |inputs, _id, ctx, style| match ctx {
                    Some(TextLeaf::Run { text, font_size, bold, line_height, .. }) => {
                        measure_text_leaf(text, *font_size, *bold, *line_height, fonts, &inputs)
                    }
                    _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                },
            );
            if re.is_err() {
                return (rects, items, paint_order);
            }
        }
    }
    // Both trees round to the pixel grid inside compute_layout (taffy's
    // use_rounding defaults on; blitz rounds via the same path), so the
    // rect comparisons assume integer edges on both sides.

    // Baseline alignment of inline runs (blitz#750 family): per-line shifts
    // computed against the FINAL layout, applied during the collect walk.
    let baseline_shifts = compute_baseline_shifts(&taffy_tree, &run_wrappers, fonts);

    // Accumulate locations down the taffy tree: child location already
    // includes the parent's border+padding offset, so a plain sum is the
    // absolute border-box origin (same accumulation blitz-paint performs).
    // The same pre-order walk emits paint items: an element's Bg when its
    // box is recorded, a run's Text at its leaf — document order, parents
    // before children.
    fn collect(
        tree: &DomTree,
        taffy_tree: &TaffyTree<TextLeaf>,
        node_map: &HashMap<taffy::tree::NodeId, NodeId>,
        styles: &HashMap<NodeId, ComputedStyle>,
        images: &HashMap<NodeId, DecodedImage>,
        static_pos: &HashMap<NodeId, (f32, f32)>,
        baseline_shifts: &HashMap<taffy::tree::NodeId, f32>,
        rects: &mut HashMap<NodeId, Rect>,
        abs_by_node: &mut HashMap<taffy::tree::NodeId, Rect>,
        items: &mut Vec<PaintItem>,
        paint_order: &mut Vec<NodeId>,
        node: taffy::tree::NodeId,
        offset: (f32, f32),
        viewport_width: f32,
    ) {
        let Ok(layout) = taffy_tree.layout(node) else { return };
        // Hit testing (obscura #738): record this element's slot in the flat
        // paint sequence as the walk reaches it. Because `collect` pushes a
        // node's own items before recursing and sorts children into the
        // z-index bands below, the resulting vector IS the paint order —
        // descendants after ancestors, hoisted z>0 siblings after the flow.
        // Text leaves and the synthetic ICB root never enter `node_map`, so
        // only boxed elements land here.
        if let Some(dom_id) = node_map.get(&node) {
            paint_order.push(*dom_id);
        }
        // transform: translate shifts geometry — the element's own rect,
        // its paint items and its whole subtree — but never layout itself
        // (Chrome: transforms don't affect layout; taffy never sees this).
        // Percentages resolve against the element's own border box
        // (obscura #740). Folded into the offset so every consumer of
        // `abs` below (rects, items, clips, the paint slots above) moves
        // together.
        let mut offset = offset;
        if let Some(dom_id) = node_map.get(&node) {
            if let Some((tx, ty)) = styles.get(dom_id).and_then(|s| s.transform_translate) {
                let resolve = |l: crate::diting_css::Length, basis: f32| match l {
                    crate::diting_css::Length::Px(v) => v,
                    crate::diting_css::Length::Percent(p) => p / 100.0 * basis,
                    // Percent-only approximation, same as to_taffy_style.
                    crate::diting_css::Length::Calc { percent, .. } => percent / 100.0 * basis,
                    crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
                };
                offset.0 += resolve(tx, layout.size.width);
                offset.1 += resolve(ty, layout.size.height);
            }
        }
        // Baseline alignment (blitz#750 family): run items drop onto their
        // line's baseline. Folded into the offset like the translate above,
        // so the item's rect, paint and whole subtree move together.
        if let Some(dy) = baseline_shifts.get(&node) {
            offset.1 += dy;
        }
        let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
        // Every visited node's absolute border box — the union pass after
        // the walk rebuilds rects for flattened inline wrappers from kids.
        abs_by_node.insert(
            node,
            Rect {
                x: abs.0,
                y: abs.1,
                width: layout.size.width,
                height: layout.size.height,
            },
        );
        let mut clips = false;
        if let Some(dom_id) = node_map.get(&node) {
            let mut rect = Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height };
            // Static-position override (the harvest pass above): a
            // both-auto axis of an out-of-flow box takes its ORIGINAL flow
            // coordinate, not the post-reparent CB flow tail taffy fell
            // back to. Sizes stay taffy's (shrink-to-fit against the CB is
            // correct per CSS).
            if let Some((sx, sy)) = static_pos.get(dom_id) {
                let s = styles.get(dom_id);
                let x_auto = s.is_none_or(|s| s.left.is_none() && s.right.is_none());
                let y_auto = s.is_none_or(|s| s.top.is_none() && s.bottom.is_none());
                if x_auto {
                    rect.x = *sx;
                }
                if y_auto {
                    rect.y = *sy;
                }
            }
            rects.insert(*dom_id, rect);
            if let Some(c) = styles.get(dom_id).and_then(|s| s.background_color) {
                if c.3 != 0 && rect.width > 0.0 && rect.height > 0.0 {
                    let color = [c.0, c.1, c.2, c.3];
                    // Radii resolve per-axis: rx against the box width,
                    // ry against its height (the elliptical form).
                    let res = |l: &crate::diting_css::Length, basis: f32| match l {
                        crate::diting_css::Length::Px(v) => *v,
                        crate::diting_css::Length::Percent(p) => p * basis / 100.0,
                        crate::diting_css::Length::Calc { percent, .. } => percent * basis / 100.0,
                        crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
                    };
                    match &styles.get(dom_id).and_then(|s| s.corner_radii.clone()) {
                        Some(corners) => {
                            let radii = corners
                                .iter()
                                .map(|(rx, ry)| {
                                    (res(rx, rect.width), res(ry, rect.height))
                                })
                                .collect::<Vec<_>>();
                            let uniform = radii.iter().all(|r| *r == radii[0]);
                            if uniform {
                                items.push(PaintItem::Bg {
                                    rect,
                                    color,
                                    radius: radii[0].0,
                                });
                            } else {
                                items.push(PaintItem::BgCorner {
                                    rect,
                                    color,
                                    radii: [
                                        radii[0], radii[1], radii[2], radii[3],
                                    ],
                                });
                            }
                        }
                        None => items.push(PaintItem::Bg {
                            rect,
                            color,
                            radius: 0.0,
                        }),
                    }
                }
            }
            // A border exists only with a line style; its color defaults to
            // currentColor = the element's own computed (inherited) color.
            let st = styles.get(dom_id);
            if let Some(style) = st.filter(|s| s.border_style.is_some()) {
                let widths = [
                    side_px(style.border_width.top),
                    side_px(style.border_width.right),
                    side_px(style.border_width.bottom),
                    side_px(style.border_width.left),
                ];
                if widths.iter().any(|w| *w > 0.0) {
                    let color = style
                        .border_color
                        .or(style.color)
                        .map(|c| [c.0, c.1, c.2, c.3])
                        .unwrap_or([0, 0, 0, 255]);
                    items.push(PaintItem::Border { rect, widths, color });
                }
            }
            // A replaced box paints either its decoded image (batch 5b,
            // object-fit: fill over the content box — equals the border box
            // until replaced elements model border/padding) or the batch-5a
            // placeholder over its bg/border. The alt run resolves against
            // the img's inherited font/color context here so paint stays
            // style-free.
            let replaced = tree
                .with_node(*dom_id, |n| n.as_element().map(|e| is_replaced_tag(&e.local)))
                .flatten()
                .unwrap_or(false);
            if replaced {
                if let Some(img) = images.get(dom_id) {
                    let st = styles.get(dom_id);
                    let fit = st.and_then(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
                    let pos = st
                        .and_then(|s| s.object_position)
                        .unwrap_or((
                            ObjectPositionPart::Percent(50.0),
                            ObjectPositionPart::Percent(50.0),
                        ));
                    // The blit destination per object-fit/position (batch
                    // 5c); `rect` stays the element box for callers that
                    // want it.
                    let paint_rect = object_paint_rect(rect, img.width as f32, img.height as f32, fit, pos);
                    items.push(PaintItem::Image { rect, paint_rect, image: img.clone() });
                } else {
                    // Alt text is an <img> concept only (batch 7a): video/
                    // iframe/canvas placeholders are the bare box.
                    let is_img = tree
                        .with_node(*dom_id, |n| {
                            n.as_element().map(|e| e.local.to_string() == "img")
                        })
                        .flatten()
                        .unwrap_or(false);
                    let alt = if is_img {
                        tree.with_node(*dom_id, |n| n.get_attribute("alt").map(|v| v.to_string()))
                            .flatten()
                            .map(|text| {
                                let (font_size, bold, lh) = font_context(tree, *dom_id, styles);
                                (text, font_size, bold, lh, color_context(tree, *dom_id, styles))
                            })
                    } else {
                        None
                    };
                    // The gray box only when the author gave no visible
                    // background — an authored bg already reads as "box here".
                    let fill_placeholder = styles
                        .get(dom_id)
                        .and_then(|s| s.background_color)
                        .is_none_or(|c| c.3 == 0);
                    items.push(PaintItem::Replaced { rect, alt, fill_placeholder });
                }
            }
            // A clipping element constrains its DESCENDANTS' paint (its own
            // bg/border above are not clipped) to the padding box — the
            // border box inset by the border widths. Text runs are taffy
            // children here, so they land inside the clip pair too.
            let style = styles.get(dom_id);
            clips = style.is_some_and(|s| {
                s.overflow.is_some_and(|o| o != Overflow::Visible)
            });
            if clips {
                let st = style.expect("checked above");
                let bline = st.border_style.is_some();
                let (bt, br, bb, bl) = (
                    if bline { side_px(st.border_width.top) } else { 0.0 },
                    if bline { side_px(st.border_width.right) } else { 0.0 },
                    if bline { side_px(st.border_width.bottom) } else { 0.0 },
                    if bline { side_px(st.border_width.left) } else { 0.0 },
                );
                // The clip rect is the padding box; the element's own radii
                // ride along (batch 7d) so descendants cut at the curve —
                // upstream clips through the rounded padding_box_path.
                let res = |l: &crate::diting_css::Length, basis: f32| match l {
                    crate::diting_css::Length::Px(v) => *v,
                    crate::diting_css::Length::Percent(p) => p * basis / 100.0,
                    crate::diting_css::Length::Calc { percent, .. } => percent * basis / 100.0,
                    crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
                };
                let pad_w = (rect.width - bl - br).max(0.0);
                let pad_h = (rect.height - bt - bb).max(0.0);
                let radii: Option<[(f32, f32); 4]> = st
                    .corner_radii
                    .clone()
                    .map(|cs| {
                        let mut out = [(0.0f32, 0.0f32); 4];
                        for (slot, (rx, ry)) in out.iter_mut().zip(cs.iter()) {
                            *slot = (res(rx, rect.width), res(ry, rect.height));
                        }
                        out
                    })
                    .or_else(|| {
                        // Legacy uniform shortcut only when no per-corner
                        // form was parsed at all.
                        st.border_radius.map(|r| {
                            let v = match r {
                                crate::diting_css::Length::Px(v) => v,
                                crate::diting_css::Length::Percent(p) => p * rect.width / 100.0,
                                crate::diting_css::Length::Calc { percent, .. } => {
                                    percent * rect.width / 100.0
                                }
                                crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
                            };
                            [(v, v); 4]
                        })
                    });
                let clip_item = match radii {
                    Some(radii) if radii.iter().any(|r| r.0 > 0.0 && r.1 > 0.0) => {
                        PaintItem::ClipRounded {
                            rect: Rect { x: rect.x + bl, y: rect.y + bt, width: pad_w, height: pad_h },
                            radii,
                        }
                    }
                    _ => PaintItem::Clip {
                        rect: Rect { x: rect.x + bl, y: rect.y + bt, width: pad_w, height: pad_h },
                    },
                };
                items.push(clip_item);
            }
        }
        if let Some(TextLeaf::Run { text, font_size, bold, color, line_height }) = taffy_tree.get_node_context(node) {
            // The wrap width the containing block offered at measure time:
            // the direct taffy parent's content box (the run wrapper for
            // mixed runs, the block itself for pure runs — same width).
            let wrap_at = taffy_tree
                .parent(node)
                .and_then(|p| taffy_tree.layout(p).ok())
                .map(|l| l.content_box_width())
                .unwrap_or(viewport_width);
            items.push(PaintItem::Text {
                text: text.clone(),
                font_size: *font_size,
                bold: *bold,
                color: *color,
                line_height: *line_height,
                x: abs.0,
                y: abs.1,
                wrap_at,
            });
        }
        if let Some(TextLeaf::Word { text, font_size, bold, color, line_height }) = taffy_tree.get_node_context(node) {
            // A word leaf paints at its own box — the enclosing flex row
            // already did the line breaking (leaf-level wrap). Single-token
            // text can never break, so wrap_at just equals the leaf width.
            items.push(PaintItem::Text {
                text: text.clone(),
                font_size: *font_size,
                bold: *bold,
                color: *color,
                line_height: *line_height,
                x: abs.0,
                y: abs.1,
                wrap_at: layout.size.width,
            });
        }
        // Stacking order (batch 6a, float level in 8f), the blitz-dom
        // damage.rs model per parent: children with z-index ≠ 0 that are
        // positioned hoist out of document order into the negative band
        // (painted first, sorted by z ascending) and the positive band
        // (last, ascending); the rest paint between them in a stable sort
        // by paint level — in-flow (static) 0 first, floats 1 (CSS 2.1
        // App. E step 5: a float paints above the in-flow blocks and text
        // of its own band), positioned z-auto 2 above. Same-z ties keep
        // tree order; text leaves are always in-flow.
        let children = taffy_tree.children(node).unwrap_or_default().to_vec();
        let mut neg: Vec<(i32, usize)> = Vec::new();
        let mut mid: Vec<(i32, usize)> = Vec::new();
        let mut pos: Vec<(i32, usize)> = Vec::new();
        for (i, &child) in children.iter().enumerate() {
            let child_style = node_map.get(&child).and_then(|d| styles.get(d));
            let positioned = child_style.is_some_and(|s| {
                matches!(
                    s.position,
                    Some(PositionMode::Relative)
                        | Some(PositionMode::Absolute)
                        | Some(PositionMode::Fixed)
                )
            });
            let floated = child_style.is_some_and(|s| s.float_side.is_some());
            let z = child_style.and_then(|s| s.z_index).unwrap_or(0);
            if z != 0 && positioned {
                // Hoisted band: painted before (z<0) / after (z>0) the
                // middle band, ascending within the band.
                if z < 0 {
                    neg.push((z, i));
                } else {
                    pos.push((z, i));
                }
            } else if positioned {
                mid.push((2, i)); // paint level 2: above in-flow content
            } else if floated {
                mid.push((1, i)); // paint level 1: above static, below positioned
            } else {
                mid.push((0, i));
            }
        }
        neg.sort_by_key(|(z, _)| *z);
        mid.sort_by_key(|(lvl, _)| *lvl);
        pos.sort_by_key(|(z, _)| *z);
        for list in [neg, mid, pos] {
            for (_, i) in list {
                collect(tree, taffy_tree, node_map, styles, images, static_pos, baseline_shifts, rects, abs_by_node, items, paint_order, children[i], abs, viewport_width);
            }
        }
        if clips {
            items.push(PaintItem::PopClip);
        }
    }
    let mut abs_by_node: HashMap<taffy::tree::NodeId, Rect> = HashMap::new();
    collect(
        tree,
        &taffy_tree,
        &node_map,
        styles,
        &images,
        &static_pos,
        &baseline_shifts,
        &mut rects,
        &mut abs_by_node,
        &mut items,
        &mut paint_order,
        icb_node,
        (0.0, 0.0),
        viewport_width,
    );
    // Flattened inline wrappers (span/label/a/… — obscura#722 lineage) own no
    // taffy box: the run hoisted their children. getBoundingClientRect still
    // owes them a rect, so union the hoisted kids' absolute boxes into one
    // bounding box. CSS unions the element's own fragments; kids approximate
    // that closely for text content (exact for the common cases).
    for (dom, kids) in &flattened {
        if rects.contains_key(dom) {
            continue;
        }
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        let mut any = false;
        for k in kids {
            let Some(r) = abs_by_node.get(k) else { continue };
            any = true;
            min_x = min_x.min(r.x);
            min_y = min_y.min(r.y);
            max_x = max_x.max(r.x + r.width);
            max_y = max_y.max(r.y + r.height);
        }
        if any {
            // Strut (obscura#722 residual): the inline's own box carries its
            // line-height even when its content is shorter — a replaced-only
            // inline (`<a><img 8x8></a>`) or a smaller-font descendant unions
            // below the line box Chrome reports. Grow the union vertically to
            // the element's effective line-height, split like half-leading.
            // Sub-pixel shortfalls (< 1px) are run-metric rounding, not a
            // missing strut — growing there just pushes the first line's
            // inline above y=0, which Chrome never reports.
            let (_, _, lh) = font_context(tree, *dom, styles);
            let h = max_y - min_y;
            if lh - h > 1.0 {
                let grow = (lh - h) / 2.0;
                min_y -= grow;
                max_y += grow;
            }
            rects.insert(
                *dom,
                Rect {
                    x: min_x,
                    y: min_y,
                    width: max_x - min_x,
                    height: max_y - min_y,
                },
            );
        }
    }
    (rects, items, paint_order)
}


/// Compute every element's cascade result for a parsed tree + stylesheet:
/// the production style-resolution pass feeding [`layout_dom`]. Walks the
/// tree once; each element matches the full rule set (O(rules × elements) —
/// fine for page-sized inputs, a matching index is future work), chains
/// inherited properties from the parent's computed style, and applies the
/// inline `style` attribute last.
pub fn compute_styles(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    fn visit(
        tree: &DomTree,
        rules: &[crate::diting_css::ParsedRule],
        rule_matches: &HashMap<usize, Vec<usize>>,
        nid: NodeId,
        parent: Option<&crate::diting_css::ComputedStyle>,
        root_fs: f32,
        out: &mut HashMap<NodeId, crate::diting_css::ComputedStyle>,
    ) {
        let Some(tag) = tree
            .with_node(nid, |n| n.as_element().map(|e| e.local.to_string()))
            .flatten()
        else {
            return;
        };
        let matched: Vec<(&crate::diting_css::ParsedRule, u32)> = rules
            .iter()
            .enumerate()
            .filter_map(|(ri, rule)| {
                // Per-rule document match sets are precomputed once (below);
                // matching here PER ELEMENT made one compute_styles pass
                // O(elements x rules x docsize) — the baidu SERP hang: a page
                // script's first geometry read re-resolved every rule against
                // every element for minutes.
                let hits = rule_matches.get(&ri)?;
                if !hits.contains(&nid.index()) {
                    return None;
                }
                let compiled = tree.compile_rule_selector(&rule.selector)?;
                Some((rule, compiled.specificity()))
            })
            .collect();
        let inline = tree
            .with_node(nid, |n| n.get_attribute("style").map(|s| s.to_string()))
            .flatten();
        let cs = crate::diting_css::cascade_element(
            &tag,
            tree,
            nid,
            &matched,
            parent,
            inline.as_deref(),
            root_fs,
        );
        let child_root_fs = if parent.is_none() {
            cs.font_size.unwrap_or(crate::diting_css::DEFAULT_ROOT_FONT_SIZE)
        } else {
            root_fs
        };
        for child in tree.children(nid) {
            visit(tree, rules, rule_matches, child, Some(&cs), child_root_fs, out);
        }
        out.insert(nid, cs);
    }
    // One querySelectorAll per RULE over the whole document, sorted for the
    // binary search in visit. This replaces the per-element-per-rule full-doc
    // scan that made style resolution quadratic-cubic on real pages.
    let rule_matches: HashMap<usize, Vec<usize>> = rules
        .iter()
        .enumerate()
        .filter_map(|(ri, rule)| {
            let hits = tree.query_selector_all_from(tree.document(), &rule.selector).ok()?;
            Some((ri, hits.into_iter().map(|n| n.index()).collect()))
        })
        .collect();

    let mut out = HashMap::new();
    for child in tree.children(tree.document()) {
        visit(
            tree,
            rules,
            &rule_matches,
            child,
            None,
            crate::diting_css::DEFAULT_ROOT_FONT_SIZE,
            &mut out,
        );
    }
    out
}

#[cfg(test)]
mod fork_deltas;

/// Batch 2b: the bridge against blitz's real layout. Same taffy on both
/// sides, so every difference the assertions catch is bridge modeling, not
/// layout math. Authored-geometry fixtures only for cross-engine asserts
/// (text metrics are heuristic here, glyph-measured in blitz — those are
/// locked as our-side structural tests instead).
#[cfg(test)]
mod bridge_cross_check;
