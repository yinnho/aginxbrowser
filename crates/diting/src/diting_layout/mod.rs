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
    TextAlign, TextDecorations, TextOverflow, TextShadow, WhiteSpace,
};
use crate::diting_dom::tree::{DomTree, NodeId};

mod forms;
pub mod image;
pub mod paint;
pub mod svg;
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
    Run {
        text: String,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        line_height: f32,
        decorations: TextDecorations,
        baseline_shift: f32,
        mono: bool,
        word_spacing: f32,
        /// The run's white-space mode (nowrap plus the pre family):
        /// `no_soft_wrap()` drops the wrap width (one line, max-content —
        /// the box overflows its container and scrollWidth grows for
        /// free), paint wraps at +inf; `pre`/`pre-line` add hard breaks,
        /// `pre*` preserve space runs.
        ws: WhiteSpace,
        /// `text-overflow: ellipsis` on the owning element (non-inherited,
        /// so captured from the block that owns the run). Only acted on
        /// together with `nowrap` at paint time — layout rects, scroll
        /// extents and selection keep the full text, per spec.
        ellipsis: bool,
        /// Shaped wrap tokens, memoized per leaf (obscura#983's measure
        /// half): taffy probes a run leaf several times per solve
        /// (min/max-content, definite widths, repair passes) and shaping
        /// used to re-run on every probe. Tokens are a pure function of
        /// text+style, which the leaf owns for its whole lifetime — shape
        /// once via [`run_tokens`], clone the Rc on every later probe.
        tokens: std::cell::RefCell<Option<std::rc::Rc<[text::Token]>>>,
        /// `font-variant-caps: small-caps` synthesis: lowercase runs shape
        /// as uppercase glyphs at 70% size (case-run token segmentation).
        small_caps: bool,
    },
    /// One word/glyph of a MIXED run (text around inline elements): the
    /// batch-2b word-leaf fallback, now carrying paint context (batch 4d).
    /// Layout is still style-driven — the measure closure passes Word
    /// leaves straight through to taffy's own style sizing.
    Word { text: String, font_size: f32, bold: bool, color: [u8; 4], line_height: f32, decorations: TextDecorations, baseline_shift: f32, mono: bool },
    /// An inline-level replaced atom rides the strut (CSS: the strut is a
    /// zero-width glyph of the container's font, so the LINE box spans its
    /// full ascent+descent): the leaf grows by the surrounding font's
    /// descent, and collect shrinks the recorded rect back to the element
    /// box — gBCR/offsetHeight stay element-sized while the container line
    /// gains the descent below the atom.
    Replaced { strut_descent: f32 },
}

/// Used line height for a text run: the element's declared `line-height`
/// (unitless multiplier against its own font-size, or absolute px).
/// `normal`/unset derives from the face's vertical metrics —
/// ascent+descent+line gap — like a real browser, not a flat 1.2×
/// (#16, blitz#878; the ratio is memoized inside the FontBook).
fn effective_line_height(
    fonts: &FontBook,
    spec: Option<&crate::diting_css::LineHeightSpec>,
    font_size: f32,
    bold: bool,
) -> f32 {
    match spec {
        Some(crate::diting_css::LineHeightSpec::Number(n)) => font_size * n,
        Some(crate::diting_css::LineHeightSpec::Px(px)) => *px,
        Some(crate::diting_css::LineHeightSpec::Normal) | None => {
            fonts.normal_line_height(font_size, bold)
        }
    }
}

/// Map a computed style onto a taffy style. Mirrors upstream `to_taffy_style`
/// for the modeled subset, including the block→flex-column promotion that
/// stands in for text alignment in a block formatting context.
/// `pct_h_resolves` is whether a percent/calc height on this element may
/// resolve (computed by the caller via [`pct_height_resolves`] — this fn has
/// no DOM access). Percent heights are folded to auto per §10.5 when the
/// containing block is content-sized.
fn to_taffy_style(style: &ComputedStyle, pct_h_resolves: bool) -> Style {
    let mut s = Style::default();
    let display = style.display.unwrap_or(CssDisplay::Block);
    // A block box (or table cell) with centered/right inline content needs a
    // flex-column stand-in because taffy's native block algorithm has no
    // line alignment (upstream to_taffy_style's promote_for_alignment).
    // Cells join the promote: `td { text-align: center }` is everywhere in
    // HTML-email-era markup.
    let promote = matches!(display, CssDisplay::Block | CssDisplay::TableCell)
        && matches!(style.text_align, Some(TextAlign::Center) | Some(TextAlign::Right));
    s.display = match display {
        CssDisplay::Block if promote => Display::Flex,
        CssDisplay::TableCell if promote => Display::Flex,
        CssDisplay::Block | CssDisplay::TableCell => Display::Block,
        CssDisplay::Flex => Display::Flex,
        CssDisplay::Grid => Display::Grid,
        // The inline/IFC stand-in is a wrapping flex row (upstream model).
        CssDisplay::Inline | CssDisplay::InlineBlock => Display::Flex,
        // Table stand-ins (table layout batch): a table is a column of row
        // wrappers, a row a non-wrapping row of cell items; cells already
        // mapped to Block above.
        CssDisplay::Table => Display::Flex,
        CssDisplay::TableRow => Display::Flex,
        CssDisplay::None => Display::None,
    };
    if promote {
        s.flex_direction = FlexDirection::Column;
        s.align_items = match style.text_align {
            Some(TextAlign::Center) => Some(AlignItems::CENTER),
            Some(TextAlign::Right) => Some(AlignItems::FLEX_END),
            _ => None,
        };
    } else if display == CssDisplay::Table {
        s.flex_direction = FlexDirection::Column;
        s.align_items = Some(AlignItems::STRETCH);
        s.flex_wrap = FlexWrap::NoWrap;
    } else if display == CssDisplay::TableRow {
        s.flex_direction = FlexDirection::Row;
        s.align_items = Some(AlignItems::STRETCH);
        s.flex_wrap = FlexWrap::NoWrap;
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
    // flex-basis/gap carry %: taffy's percent matches CSS —
    // flex-basis resolves against the container main-axis inner size, gap
    // against the per-axis container size — so it passes through and
    // resolves at layout time, same posture as the slots above.
    if let Some(fb) = style.flex_basis {
        s.flex_basis = match fb {
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
                Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                    let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                    measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                }
                Some(TextLeaf::Word { .. }) | Some(TextLeaf::Replaced { .. }) | None => {
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
fn side_px(v: Option<crate::diting_css::Length>) -> f32 {
    match v {
        Some(crate::diting_css::Length::Px(px)) => px,
        _ => 0.0,
    }
}

/// CSS BG3 §5.4 corner-overlap rule: when adjacent corner radii sum past
/// their shared edge, ALL radii scale down by the tightest ratio (Chrome's
/// behavior) — an unclamped 999px pill radius otherwise paints a stray arc
/// far above the box (slides-deck probe). Corner order is [TL, TR, BR, BL]
/// (diting_css border-radius expand). The ratio is invariant under diagonal
/// affine scaling, so clamping pre- or post-prebake is equivalent.
fn clamp_corner_radii(radii: [(f32, f32); 4], w: f32, h: f32) -> [(f32, f32); 4] {
    let mut f = 1.0f32;
    let edges = [
        (w, radii[0].0 + radii[1].0), // top: TL.x + TR.x
        (w, radii[3].0 + radii[2].0), // bottom: BL.x + BR.x
        (h, radii[0].1 + radii[3].1), // left: TL.y + BL.y
        (h, radii[1].1 + radii[2].1), // right: TR.y + BR.y
    ];
    for (edge, sum) in edges {
        if sum > 0.0 {
            f = f.min(edge / sum);
        }
    }
    if f >= 1.0 {
        return radii;
    }
    radii.map(|(rx, ry)| (rx * f, ry * f))
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
    fonts: &FontBook,
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
    let b = bold.unwrap_or(false);
    (fs, b, effective_line_height(fonts, lh_spec.as_ref(), fs, b))
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

/// Whether a text node rides the monospace face (mono batch): the same
/// nearest-set ancestor walk as [`color_context`] over `font-family` — the
/// cascade inherits it, but text nodes carry no ComputedStyle. Any family
/// list member naming monospace/ui-monospace selects the face (per-char
/// fallback keeps CJK on the primary pair). Defaults to false.
fn mono_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> bool {
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(family) = styles.get(&nid).and_then(|s| s.font_family.as_deref()) {
            return crate::diting_css::wants_monospace(family);
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    false
}

/// Inherited `word-spacing` for a text node (px): the same nearest-set
/// ancestor walk as [`mono_context`] — the cascade inherits it, but text
/// nodes carry no ComputedStyle of their own. Defaults to 0.0 (`normal`).
fn word_spacing_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> f32 {
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(ws) = styles.get(&nid).and_then(|s| s.word_spacing) {
            return ws;
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    0.0
}

/// `font-variant-caps` inherits, so a text node's synthesis flag walks the
/// ancestor chain like word-spacing does (None = unset → no synthesis).
fn small_caps_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> bool {
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(sc) = styles.get(&nid).and_then(|s| s.font_variant_caps) {
            return sc;
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    false
}

/// text-decoration resolution for a text node: the property PROPAGATES
/// (CSS 2.1 §16.3.1) rather than inheriting, so every ancestor's declared
/// set unions in — `<u><s>` paints both lines. Propagation stops at an
/// atomic inline (inline-block) or out-of-flow (absolute/fixed/float)
/// boundary: the boundary element's own text still carries its own
/// declaration, but nothing crosses such an element inward OR outward.
fn decoration_context(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
) -> TextDecorations {
    let mut out = TextDecorations::default();
    let mut current = Some(id);
    while let Some(nid) = current {
        let boundary = if let Some(s) = styles.get(&nid) {
            if let Some(d) = s.text_decoration_line {
                out = out.union(d);
            }
            nid != id
                && (s.display == Some(CssDisplay::InlineBlock)
                    || matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
                    || s.float_side.is_some())
        } else {
            false
        };
        if boundary {
            break;
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    out
}

/// `vertical-align: sub|super` as a baseline shift on inline text (CSS 2.1
/// §10.8.1 keyword model, Chromium constants): sub drops 20% of the PARENT's
/// font-size below the baseline, super lifts 40% above it — reproduces the
/// Chrome probes (a 20px-parent `<sup>` and an explicit 20px `super` both
/// grow the line by exactly 8px). Shifts compose up the ancestor chain
/// (nested sup compounds). The walk stops at an atomic-inline (inline-block)
/// or out-of-flow boundary WITHOUT applying the boundary's own declaration:
/// vertical-align on an inline-block moves the BOX, not the text inside.
/// Down-positive: sub → +0.2×parent_fs, super → −0.4×parent_fs; an authored
/// length (up-positive, CSS: positive raises) flips sign; a percentage
/// resolves against the element's own line-height before the same flip;
/// baseline/top/middle/bottom are 0 (the line-baseline machinery handles
/// those at the alignment site).
fn valign_shift(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
) -> f32 {
    let mut shift = 0.0f32;
    let mut current = Some(id);
    while let Some(nid) = current {
        let boundary = if let Some(s) = styles.get(&nid) {
            if s.display == Some(CssDisplay::Inline) {
                match s.vertical_align {
                    Some(crate::diting_css::VerticalAlign::Sub)
                    | Some(crate::diting_css::VerticalAlign::Super) => {
                        let parent = tree.with_node(nid, |n| n.parent).flatten();
                        let parent_fs = parent
                            .map(|p| font_context(tree, p, styles, fonts).0)
                            .unwrap_or(16.0);
                        shift += if s.vertical_align == Some(crate::diting_css::VerticalAlign::Sub) {
                            0.2 * parent_fs
                        } else {
                            -0.4 * parent_fs
                        };
                    }
                    Some(crate::diting_css::VerticalAlign::Length(px)) => {
                        shift -= px;
                    }
                    Some(crate::diting_css::VerticalAlign::Percent(p)) => {
                        let own_lh = font_context(tree, nid, styles, fonts).2;
                        shift -= p / 100.0 * own_lh;
                    }
                    _ => {}
                }
            }
            nid != id
                && (s.display == Some(CssDisplay::InlineBlock)
                    || matches!(s.position, Some(PositionMode::Absolute) | Some(PositionMode::Fixed))
                    || s.float_side.is_some())
        } else {
            false
        };
        if boundary {
            break;
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    shift
}

/// Font descent at a size/weight, FontBook fallback fs×0.2 — the same
/// fallback text_baseline rides.
fn leaf_descent(fonts: &FontBook, fs: f32, bold: bool) -> f32 {
    fonts.metrics(fs, bold).map(|m| m.descent).unwrap_or(fs * 0.2)
}

/// Extra leaf height a baseline shift needs so the line box contains the
/// shifted glyph: a lift (super) pads by the lift itself; a drop (sub) pads
/// by the drop plus the glyph's descent, which would otherwise poke out the
/// line's bottom. Whole-pixel: taffy's round_layout pass sizes nodes on the
/// cumulative pixel grid, so a fractional pad would be unrecoverable from
/// the rounded height and the error would land on the element's own box.
fn shift_pad(shift: f32, descent: f32) -> f32 {
    let raw = if shift < 0.0 {
        -shift
    } else if shift > 0.0 {
        shift + descent
    } else {
        0.0
    };
    raw.ceil()
}

/// The label a select displays: the first option whose selectedness is on
/// — the parsed `selected` attribute, since JS-side dirtiness rides the
/// live_value mirror instead — else the first option, the same precedence
/// the bootstrap's select.value getter resolves with (`opts[i].selected`
/// falls back to the attribute there). An option's display text is its
/// `label` attribute when present, else its content, trimmed.
fn selected_option_label(tree: &DomTree, root: NodeId) -> Option<String> {
    // Document order over the subtree: options are children of the select
    // or of optgroups, so an explicit LIFO stack of reversed child lists
    // walks exactly the order querySelectorAll('option') would match.
    let children_rev = |id: NodeId| -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut cur = tree.with_node(id, |n| n.first_child).flatten();
        while let Some(c) = cur {
            out.push(c);
            cur = tree.with_node(c, |n| n.next_sibling).flatten();
        }
        out.reverse();
        out
    };
    let label_of = |id: NodeId| -> String {
        tree.with_node(id, |n| n.get_attribute("label").map(|v| v.to_string()))
            .flatten()
            .unwrap_or_else(|| tree.text_content(id))
            .trim()
            .to_string()
    };
    let mut first: Option<String> = None;
    let mut stack = children_rev(root);
    while let Some(nid) = stack.pop() {
        let is_option = tree
            .with_node(nid, |n| n.as_element().map(|e| e.local.as_ref() == "option"))
            .flatten()
            .unwrap_or(false);
        if is_option {
            let selected = tree
                .with_node(nid, |n| n.get_attribute("selected").is_some())
                .unwrap_or(false);
            if selected {
                return Some(label_of(nid));
            }
            // The first option stays the fallback even when its label is
            // empty (Chrome shows an empty box then, not option #2).
            if first.is_none() {
                first = Some(label_of(nid));
            }
        } else {
            // Only optgroup (and, in weird markup, anything else) can wrap
            // options; text nodes carry no options.
            stack.extend(children_rev(nid));
        }
    }
    first.filter(|s| !s.is_empty())
}

/// The text run a form control paints inside its replaced box: the dirty
/// value (live_value, mirrored from `el.value = x` by the bootstrap) wins,
/// then the parsed default — value attribute for input, textContent for
/// textarea (the same precedence the JS value getter uses, so what paints
/// is what `el.value` reads). A select paints the label of the option its
/// value getter would report (the bootstrap's value/selectedIndex writes
/// land that label as live_value on the select). An empty text-like
/// control paints its placeholder in Chrome's gray; button-ish inputs
/// label from their value like their sizing does. Checkable inputs draw
/// no run — the widget itself carries the state (see [`FormWidget`]).
/// None for everything else.
fn form_control_run(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
) -> Option<(String, f32, bool, f32, [u8; 4])> {
    let tag = tree
        .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    if tag != "input" && tag != "textarea" && tag != "select" {
        return None;
    }
    let (font_size, bold, lh) = font_context(tree, id, styles, fonts);
    let ink = color_context(tree, id, styles);
    let attr = |name: &str| {
        tree.with_node(id, |n| n.get_attribute(name).map(|v| v.to_string()))
            .flatten()
    };
    let live = tree
        .with_node(id, |n| n.live_value().map(|v| v.to_string()))
        .flatten();
    let run = |text: String, color: [u8; 4]| Some((text, font_size, bold, lh, color));
    if tag == "select" {
        let label = live
            .filter(|s| !s.trim().is_empty())
            .or_else(|| selected_option_label(tree, id))?;
        return run(label.trim().to_string(), ink);
    }
    if tag == "textarea" {
        let text = live.unwrap_or_else(|| tree.text_content(id));
        return if text.is_empty() {
            attr("placeholder").and_then(|p| run(p, [117, 117, 117, 255]))
        } else {
            run(text, ink)
        };
    }
    let ty = attr("type").unwrap_or_default().to_ascii_lowercase();
    match ty.as_str() {
        // Checkbox/radio are drawn as widgets; range's value is the thumb
        // position, not text (blitz#456).
        "checkbox" | "radio" | "range" => None,
        "button" | "submit" | "reset" => {
            let label = live
                .or_else(|| attr("value"))
                .filter(|v| !v.is_empty())
                .or_else(|| (ty == "submit").then(|| "Submit".to_string()));
            label.and_then(|t| run(t, ink))
        }
        _ => {
            let text = live.or_else(|| attr("value")).unwrap_or_default();
            if text.is_empty() {
                attr("placeholder").and_then(|p| run(p, [117, 117, 117, 255]))
            } else {
                run(text, ink)
            }
        }
    }
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

/// white-space-aware tokenizer for the pre family. Three regimes:
/// - collapse (normal/nowrap): the plain [`tokenize`] path, edges trimmed.
/// - pre-line: collapse spaces per newline-separated segment but keep the
///   newlines themselves as hard-break tokens.
/// - preserve (pre/pre-wrap/break-spaces): keep every space and newline; CRLF
///   normalizes to LF first (UA behaviour); tab becomes an 8-space run
///   (default tab-size).
pub(crate) fn tokenize_ws(text: &str, ws: crate::diting_css::WhiteSpace) -> Vec<String> {
    use crate::diting_css::WhiteSpace as Ws;
    match ws {
        Ws::Normal | Ws::Nowrap => tokenize(text.trim()),
        Ws::PreLine => {
            let mut acc = Vec::new();
            let segs: Vec<&str> = text.split('\n').collect();
            for (i, seg) in segs.iter().enumerate() {
                let seg = seg.strip_suffix('\r').unwrap_or(seg);
                acc.append(&mut tokenize(seg.trim()));
                if i + 1 < segs.len() {
                    acc.push("\n".to_string());
                }
            }
            acc
        }
        Ws::Pre | Ws::PreWrap | Ws::BreakSpaces => {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let mut tokens = Vec::new();
            let mut word = String::new();
            for ch in text.chars() {
                match ch {
                    '\n' => {
                        if !word.is_empty() {
                            tokens.push(std::mem::take(&mut word));
                        }
                        tokens.push("\n".to_string());
                    }
                    '\t' => {
                        if !word.is_empty() {
                            tokens.push(std::mem::take(&mut word));
                        }
                        tokens.push("        ".to_string());
                    }
                    _ if ch.is_whitespace() => {
                        if !word.is_empty() {
                            tokens.push(std::mem::take(&mut word));
                        }
                        tokens.push(ch.to_string());
                    }
                    _ if is_cjk(ch) => {
                        if !word.is_empty() {
                            tokens.push(std::mem::take(&mut word));
                        }
                        tokens.push(ch.to_string());
                    }
                    _ => word.push(ch),
                }
            }
            if !word.is_empty() {
                tokens.push(word);
            }
            tokens
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn build_word_leaves(
    text: &str,
    font_size: f32,
    bold: bool,
    color: [u8; 4],
    line_height: f32,
    decorations: TextDecorations,
    shift: f32,
    mono: bool,
    word_spacing: f32,
    small_caps: bool,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
) -> Vec<taffy::tree::NodeId> {
    let mut word_leaf = |seg_text: &str, seg_size: f32, is_space: bool| {
        // The separator token's advance carries the inherited
        // word-spacing (same token model measurement uses), so
        // taffy positions the following word leaves accordingly.
        let width = fonts.advance_width(seg_text, seg_size, bold, mono)
            + if is_space { word_spacing } else { 0.0 };
        // Pure-whitespace tokens contribute no height (they sit between
        // block siblings without adding a spurious blank row).
        let height = if is_space {
            0.0
        } else {
            line_height + shift_pad(shift, leaf_descent(fonts, seg_size, bold))
        };
        let style = Style {
            size: Size {
                width: Dimension::length(width.max(0.0)),
                height: Dimension::length(height.max(0.0)),
            },
            ..Style::default()
        };
        let leaf = TextLeaf::Word {
            text: seg_text.to_string(),
            font_size: seg_size,
            bold,
            color,
            line_height,
            decorations,
            baseline_shift: shift,
            mono,
        };
        taffy_tree.new_leaf_with_context(style, leaf).ok()
    };
    tokenize(text)
        .into_iter()
        .flat_map(|token| {
            let is_space = token.trim().is_empty();
            if small_caps && !is_space && token.chars().any(|c| c.is_lowercase()) {
                // Synthesized small-caps: lowercase runs render as
                // uppercase glyphs at 70% size, mixed case stays
                // full-size (Blink's model). Each segment is its own
                // leaf so taffy positions them by shaped width.
                text::caps_case_runs(&token)
                    .into_iter()
                    .filter_map(|(seg, lower)| {
                        let seg_size = if lower {
                            font_size * text::SMALL_CAPS_RATIO
                        } else {
                            font_size
                        };
                        // Synthesis uppercases the glyphs (`ß` → "SS");
                        // already-uppercase segments are unaffected.
                        word_leaf(&seg.to_uppercase(), seg_size, false)
                    })
                    .collect::<Vec<_>>()
            } else {
                word_leaf(&token, font_size, is_space).into_iter().collect::<Vec<_>>()
            }
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

/// Shaped-token memo accessor for a run leaf: shape once, then hand every
/// later probe a cheap Rc clone (obscura#983's measure half). The leaf owns
/// its text+style for its whole lifetime, so the memo needs no key — unlike
/// upstream, which keys a 16-entry per-item cache by (width, Wrap).
#[allow(clippy::too_many_arguments)]
fn run_tokens(
    text: &String,
    font_size: f32,
    bold: bool,
    fonts: &FontBook,
    mono: bool,
    word_spacing: f32,
    ws: WhiteSpace,
    memo: &std::cell::RefCell<Option<std::rc::Rc<[text::Token]>>>,
    small_caps: bool,
) -> std::rc::Rc<[text::Token]> {
    memo.borrow_mut()
        .get_or_insert_with(|| text::tokens_of(text, font_size, bold, fonts, mono, word_spacing, ws, small_caps).into())
        .clone()
}

/// Preserve-wins merge for a mixed white-space run: white-space inherits, so
/// one `pre` segment makes the whole run preserve (the same "any nowrap
/// pins the run" precedent, ranked across the family).
fn ws_rank(ws: WhiteSpace) -> u8 {
    match ws {
        WhiteSpace::Pre => 5,
        WhiteSpace::BreakSpaces => 4,
        WhiteSpace::PreWrap => 3,
        WhiteSpace::PreLine => 2,
        WhiteSpace::Nowrap => 1,
        WhiteSpace::Normal => 0,
    }
}

/// Taffy measure function for a pure-text run leaf (batch 3a). Reproduces
/// the observable behavior of blitz's parley-measured text nodes:
///
/// - `tokens` arrive pre-shaped (once per leaf, via [`run_tokens`]); an
///   empty slice means the run trims to nothing and measures HIDDEN;
/// - collapsible whitespace at run EDGES contributes nothing (probe:
///   `"hello "` and `" hello"` both measure as `"hello"`, `" "` as zero);
/// - greedy line breaking over exact shaped advances; a space before a
///   break point is dropped (CSS trailing-whitespace removal);
/// - the block's width is `ceil(max line advance)` — parley rounds the text
///   run's size UP so nothing overflows the box, which taffy's own
///   round-to-nearest would not reproduce (probe: "hello" 37.36 → 38);
/// - height = line count × used line-height (blitz pins `normal` at
///   1.2×fs; declared values arrive with the leaf) plus the baseline-shift
///   pad, so a shifted (sub/sup) run grows its line box by exactly the
///   shift's extent.
fn measure_text_leaf(
    tokens: &[text::Token],
    line_height: f32,
    pad: f32,
    inputs: &taffy::tree::LayoutInput,
    ws: WhiteSpace,
) -> taffy::tree::LayoutOutput {
    let known = inputs.known_dimensions;
    let lh = line_height;
    if tokens.is_empty() {
        return taffy::tree::LayoutOutput::HIDDEN;
    }
    let widest_token = tokens.iter().map(|t| t.width).fold(0.0, f32::max);

    let wrap_at = if ws.no_soft_wrap() {
        None
    } else {
        match inputs.available_space.width {
            taffy::AvailableSpace::Definite(w) => Some(w),
            _ => None,
        }
    };
    // Greedy wrap over the shared breaker (batch 4a): measure and paint see
    // the same lines by construction.
    let lines = text::greedy_wrap(&tokens, wrap_at, ws);
    let min_content = matches!(inputs.available_space.width, taffy::AvailableSpace::MinContent);
    // no-soft-wrap modes remove every soft wrap opportunity (CSS Text §5):
    // min-content rises to the full single line, same as max-content.
    let max_line =
        if min_content && !ws.no_soft_wrap() { widest_token } else { lines.iter().map(|l| l.width).fold(0.0, f32::max) };
    let size = taffy::geometry::Size {
        width: known.width.unwrap_or(max_line.ceil()),
        height: known.height.unwrap_or(lines.len() as f32 * lh + pad),
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
/// on a zero-height input (obscura#807 class). `svg` joins for the same
/// reason plus a stronger one: its children are shapes/text in viewBox
/// user units, not flow content — flowing them is the text-leak bug the
/// AginxOS svg batch starts from. The subtree compiles to paint ops at
/// collect time instead (see [`svg::compile_svg`]).
fn is_replaced_tag(tag: &str) -> bool {
    matches!(
        tag,
        "img" | "video" | "iframe" | "canvas" | "object" | "embed" | "input" | "textarea"
            | "select"
            | "svg"
    )
}

/// Canonicalize an `<img>`-shaped URL against the document base. The byte
/// tables both fetch paths key (`ImageCache::network_bytes`, the screenshot
/// prefetch, the band-pump's `image_bytes`) hold ABSOLUTE URLs, so a
/// relative `src="dot.png"` that passes through raw never matches its own
/// fetched body — the img paints a placeholder while the bytes sit in the
/// table. Resolution follows the fetchers' own join (`base.join(raw)`), so
/// both sides produce the same canonical `Url` string. data:/blob: are
/// self-contained and pass through; an already-absolute src canonicalizes
/// through `Url::parse` (same normalizer `join` applies); without a base —
/// or against an unparseable base — the raw string survives unchanged,
/// preserving the pre-base pass-through (miss → placeholder).
fn absolutize_img_src(raw: &str, base_url: Option<&str>) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("data:") || raw.starts_with("blob:") {
        return raw.to_string();
    }
    if let Ok(u) = url::Url::parse(raw) {
        return u.to_string();
    }
    match base_url
        .and_then(|b| url::Url::parse(b).ok())
        .and_then(|b| b.join(raw).ok())
    {
        Some(joined) => joined.to_string(),
        None => raw.to_string(),
    }
}

/// The URL an `<img>` should load: its `<picture>` parent's first matching
/// `<source>` candidate when present (media query evaluated at the layout
/// viewport), else the img's own srcset selection, else plain `src`
/// (HTML §4.8.4.3.9, product-simplified). `base_url` is the owner
/// document's URL — relative sources absolutize against it (see
/// [`absolutize_img_src`]) so the returned string is the key the fetched
/// byte tables use. None → the placeholder path.
pub fn resolve_img_source(
    tree: &DomTree,
    img: NodeId,
    viewport_width: f32,
    base_url: Option<&str>,
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
                        return Some(absolutize_img_src(&c.url, base_url));
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
            return Some(absolutize_img_src(&c.url, base_url));
        }
    }

    attr(img, "src").map(|s| absolutize_img_src(&s, base_url))
}

/// Minimal media-query width gate for `<source media>`: supports the two
/// real-world forms `(min-width: Npx)` / `(max-width: Npx)` joined by
/// `and`, plus absent/`all`. Anything else declines (conservative: falls
/// through to the next source or the fallback img).
pub fn media_matches_width(media: Option<&str>, vw: f32) -> bool {
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

/// CSS 2.1 §10.5: a percentage height resolves only when the containing
/// block's height is specified EXPLICITLY; against a content-sized
/// (indefinite) ancestor the percentage behaves as `auto`. Walk the DOM
/// ancestors of `id`: Px (or a viewport unit already folded to px at
/// cascade) is definite; Percent/Calc defers the question one level up;
/// auto/keywords/nothing stops the walk as indefinite. The document root
/// counts as definite — its percentage resolves against the viewport.
fn cb_height_definite(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    id: NodeId,
) -> bool {
    let mut cur = tree.with_node(id, |n| n.parent).flatten();
    while let Some(nid) = cur {
        if tree.with_node(nid, |n| n.parent).flatten().is_none() {
            return true;
        }
        match styles.get(&nid).and_then(|s| s.height) {
            Some(crate::diting_css::Length::Px(_)) => return true,
            Some(crate::diting_css::Length::Percent(_))
            | Some(crate::diting_css::Length::Calc { .. }) => {
                cur = tree.with_node(nid, |n| n.parent).flatten();
            }
            _ => return false,
        }
    }
    false
}

/// Whether `height: <percent>` on element `id` resolves, or folds to `auto`
/// per CSS 2.1 §10.5: it resolves when the containing-block chain is definite
/// (see [`cb_height_definite`]) OR the element is absolutely positioned (the
/// §10.5 exception — its containing block is the nearest positioned
/// ancestor's padding box, definite by construction once that ancestor has
/// laid out).
fn pct_height_resolves(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    id: NodeId,
) -> bool {
    let abspos = matches!(
        styles.get(&id).and_then(|s| s.position),
        Some(PositionMode::Absolute) | Some(PositionMode::Fixed)
    );
    abspos || cb_height_definite(tree, styles, id)
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
    strut_descent: f32,
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
                // Chrome's default slider box (blitz#456).
                "range" => (129.0, 16.0, false),
                "button" | "submit" | "reset" => {
                    // Same precedence as the paint run: dirty value, then the
                    // parsed value attribute, then Chrome's default "Submit".
                    let label = tree
                        .with_node(id, |n| {
                            n.live_value().map(|v| v.to_string()).or_else(|| {
                                n.get_attribute("value").map(|v| v.to_string())
                            })
                        })
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
        // An atomic replaced box like the others: options never lay out as
        // page text (a closed select paints one label). Width tracks the
        // label it will paint — the same precedence form_control_run
        // resolves — with control padding and dropdown-arrow room added,
        // matching how the button arm sizes from its label.
        "select" => {
            let label = tree
                .with_node(id, |n| n.live_value().map(|v| v.to_string()))
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| selected_option_label(tree, id))
                .unwrap_or_default();
            (label.chars().count() as f32 * 8.8 + 36.0, 22.0, false)
        }
        "canvas" => (
            aw.unwrap_or(300.0),
            ah.unwrap_or(150.0),
            true,
        ),
        "video" | "iframe" | "embed" | "object" => {
            (aw.unwrap_or(300.0), ah.unwrap_or(150.0), false)
        }
        // svg: the viewBox IS the intrinsic size/ratio — the element box
        // maps onto it at paint (that map is the zoom semantics of this
        // batch). Attr w/h override per-axis and back-fill through the
        // viewBox ratio; no viewBox: attr-or-300×150 like the others.
        "svg" => {
            let vb = tree
                .with_node(id, |n| n.get_attribute("viewBox").map(|v| v.to_string()))
                .flatten()
                .and_then(|v| svg::parse_view_box(&v));
            match vb {
                Some((_, _, vw, vh)) => {
                    let r = vw / vh;
                    let (w, h) = match (aw, ah) {
                        (Some(w), Some(h)) if h > 0.0 => (w, h),
                        (Some(w), None) => (w, w / r),
                        (None, Some(h)) => (h * r, h),
                        _ => (vw, vh),
                    };
                    (w, h, true)
                }
                None => (aw.unwrap_or(300.0), ah.unwrap_or(150.0), false),
            }
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

    // Attrs block ratio transfer for img and svg: width/height attrs are
    // presentational-hint DECLARATIONS, so a CSS override of one axis does
    // not re-derive the other (#cssw stays 100×200, not 100×50). Canvas
    // attrs ARE the intrinsic size and never block.
    let (attr_dw, attr_dh) =
        if tag == "img" || tag == "svg" { (aw.is_some(), ah.is_some()) } else { (false, false) };
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
    // Positioning rides into replaced leaves too (to_taffy_style does this
    // for every boxed element): without it an absolutely-positioned
    // input/img/select kept its taffy-relative style, the reparent pass
    // still moved it to the containing block, and it then stacked in flow
    // there ignoring its insets.
    s.position = match style.position {
        Some(crate::diting_css::PositionMode::Absolute)
        | Some(crate::diting_css::PositionMode::Fixed) => Position::Absolute,
        _ => Position::Relative,
    };
    // Inset/clamp unset values are CSS `auto` (same mapping as
    // to_taffy_style's local lpa_auto).
    let lpa_auto = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentageAuto::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
        Some(crate::diting_css::Length::Calc { percent, .. }) => {
            LengthPercentageAuto::percent(percent / 100.0)
        }
        Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent) | None => LengthPercentageAuto::auto(),
    };
    if style.top.is_some()
        || style.right.is_some()
        || style.bottom.is_some()
        || style.left.is_some()
    {
        s.inset = taffy::geometry::Rect {
            top: lpa_auto(style.top),
            right: lpa_auto(style.right),
            bottom: lpa_auto(style.bottom),
            left: lpa_auto(style.left),
        };
    }
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
    // The strut descent bakes into the leaf's DEFINITE height (line-box
    // bookkeeping; collect subtracts it back from the rect). taffy's ratio
    // transfer off that height — the cyclic-percent case, where a percent
    // width against a content-sized ancestor can't resolve and the width
    // derives from the height — must land on nat_w, not nat_w + strut×ratio
    // (the +9px hero width). Fold the strut into the denominator instead.
    // Both-definite leaves never transfer (the ratio is inert there) and the
    // width→height direction can't fire for inline atoms — their height arm
    // is always the baked definite length — so the compensated ratio only
    // ever feeds the height→width transfer. Block-level callers pass strut
    // 0 and keep the raw natural ratio.
    s.aspect_ratio = ratio_transfer.then(|| nat_w / (nat_h + strut_descent));
    // CSS width/height win per axis; missing axis derives from the ratio.
    // Percent CSS sizes pass through (the CB resolves them; the natural
    // ratio only backfills auto axes). Px sizes are content-box per the
    // attribute semantics, so border widths ride on top — except under
    // authored `box-sizing: border-box`, where the px already measures to
    // the border edge.
    let border_box = matches!(
        style.box_sizing,
        Some(crate::diting_css::BoxSizing::BorderBox)
    );
    // The strut descent (inline atoms only — block-level callers pass 0)
    // extends the LEAF below the element box so the line box spans the
    // strut's full extent; collect subtracts it back from the recorded
    // rect, so percent/calc arms (CB-resolved approximations) skip the pad
    // rather than grow an axis the rect shrink could not undo exactly.
    s.size = Size {
        width: match style.width {
            Some(crate::diting_css::Length::Px(w)) => {
                Dimension::length(if border_box { w } else { w + if bline { bl + br } else { 0.0 } })
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
                Dimension::length((if border_box { h } else { h + if bline { bt + bb } else { 0.0 } }) + strut_descent)
            }
            Some(crate::diting_css::Length::Percent(p))
                if pct_height_resolves(tree, styles, id) =>
            {
                Dimension::percent(p / 100.0)
            }
            Some(crate::diting_css::Length::Calc { percent, .. })
                if pct_height_resolves(tree, styles, id) =>
            {
                Dimension::percent(percent / 100.0)
            }
            // Percent (or calc with a percent part) against a content-sized
            // ancestor behaves as `auto` (§10.5) — for a replaced box its
            // natural height. Taffy would resolve the percent against an
            // indefinite CB to 0, collapsing `height:100%` iframes inside
            // auto-height wrappers (the webtop .window-body case) to zero.
            Some(crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent | crate::diting_css::Length::Percent(_) | crate::diting_css::Length::Calc { .. }) | None => Dimension::length(derived_h.unwrap_or(nat_h) + if bline { bt + bb } else { 0.0 } + strut_descent),
        },
    };

    let node = taffy_tree
        .new_leaf_with_context(s, TextLeaf::Replaced { strut_descent })
        .ok()?;
    node_map.insert(node, id);
    Some(node)
}

/// Build the taffy subtree for one element. Returns None for display:none
/// (subtree skipped) and for the document node's non-element parts.
/// Strut descent for an inline-level replaced atom: the descent of the
/// element's inherited font context (the strut IS a zero-width glyph of the
/// line's font). Block-level replaced boxes pass 0.
fn strut_descent_for(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
) -> f32 {
    let (fs, bold, _) = font_context(tree, id, styles, fonts);
    // Whole-pixel pad so the padded leaf height survives taffy's integer
    // rounding exactly and collect's subtract-back lands on the true box.
    leaf_descent(fonts, fs, bold).ceil()
}

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
    meta: &mut TableBuildMeta,
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
            let sd = strut_descent_for(tree, child, styles, fonts);
            if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map, sd) {
                // A lone inline atom still gets the wrapping-run stand-in so
                // it lays out on the text baseline path like the main loop.
                if let Ok(wrapper) =
                    taffy_tree.new_with_children(run_wrapper_style(), &[leaf])
                {
                    run_wrappers.push(wrapper);
                    direct.push(wrapper);
                }
            }
        } else if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map, 0.0) {
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
        if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
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
            let (fs, b, lh) = font_context(tree, child, styles, fonts);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { line_height };
            let col = color_context(tree, child, styles);
            let deco = decoration_context(tree, child, styles);
            let vs = valign_shift(tree, child, styles, fonts);
            let mono = mono_context(tree, child, styles);
            let ws = word_spacing_context(tree, child, styles);
            let sc = small_caps_context(tree, child, styles);
            leaves.extend(build_word_leaves(&text, fs, b, col, lh, deco, vs, mono, ws, sc, fonts, taffy_tree));
        } else {
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta);
            if let Some(sub) = sub {
                let mut sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                wrap_q_quotes(tree, child, &child_tag, styles, fonts, taffy_tree, &mut sub_children);
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
    if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
        direct.push(node);
    }
}

/// Chrome's UA sheet gives `q::before/::after` the `open-quote`/`close-quote`
/// keywords, which resolve through auto quote nesting: even depth uses double
/// curly quotes, odd depth flips to singles.
fn q_quote_pair(depth: usize) -> (&'static str, &'static str) {
    if depth % 2 == 0 {
        ("\u{201C}", "\u{201D}")
    } else {
        ("\u{2018}", "\u{2019}")
    }
}

fn q_ancestor_quote_depth(tree: &DomTree, mut id: NodeId) -> usize {
    let mut depth = 0usize;
    while let Some(parent) = tree.with_node(id, |n| n.parent).flatten() {
        let is_q = tree
            .with_node(parent, |n| {
                n.as_element().map(|e| e.local.to_string() == "q")
            })
            .flatten()
            .unwrap_or(false);
        if is_q {
            depth += 1;
        }
        id = parent;
    }
    depth
}

/// CSS counter state for one compute_styles pass: per-name stacks of
/// (value, walk depth of the element that created it) plus the
/// generated-quote nesting depth. A counter created at depth d stays
/// visible to the creator's following siblings and their subtrees, and
/// pops when the walk leaves its creating parent — the css-lists-3 scope
/// rule that makes `ol{counter-reset} li::before{counter-increment}`
/// number nested lists as "1.1" and the next outer item as plain "2".
#[derive(Default)]
struct CounterState {
    map: HashMap<String, Vec<(i64, usize)>>,
    quote_depth: usize,
}

fn apply_counter_modifiers(
    state: &mut CounterState,
    reset: &[(String, i32)],
    increment: &[(String, i32)],
    depth: usize,
) {
    // Reset pushes a counter nested inside any ancestor-origin same-name
    // counter, but shadows every same-name counter created at this depth
    // or deeper (the previous-sibling removal of css-lists-3 §4.4.2 —
    // sibling <ol>s each restart at 1). Increment then bumps the
    // innermost; both compose on one element.
    for (name, v) in reset {
        let entry = state.map.entry(name.clone()).or_default();
        while matches!(entry.last(), Some(e) if e.1 >= depth) {
            entry.pop();
        }
        entry.push((*v as i64, depth));
    }
    for (name, d) in increment {
        let entry = state.map.entry(name.clone()).or_default();
        if entry.is_empty() {
            entry.push((0, depth));
        }
        if let Some(last) = entry.last_mut() {
            last.0 += *d as i64;
        }
    }
}

/// End-of-subtree scope pop: counters created strictly inside the exiting
/// element die with their creating parent; ones created at this depth or
/// shallower survive for the following siblings.
fn pop_out_of_scope(state: &mut CounterState, depth: usize) {
    for stack in state.map.values_mut() {
        while matches!(stack.last(), Some(e) if e.1 > depth) {
            stack.pop();
        }
    }
}

/// The depth'th declared pair, repeating the last pair once the depth runs
/// past it; `quotes: none` renders nothing and no declared `quotes` falls
/// back to the same curly pair the UA `q` marks use.
fn quote_pair(quotes: Option<&[String]>, depth: usize) -> (&str, &str) {
    match quotes {
        Some([]) => ("", ""),
        Some(q) => {
            let n = q.len() / 2;
            let i = depth.min(n - 1) * 2;
            (q[i].as_str(), q[i + 1].as_str())
        }
        None if depth.is_multiple_of(2) => ("\u{201C}", "\u{201D}"),
        None => ("\u{2018}", "\u{2019}"),
    }
}

fn resolve_content_into(
    cv: &crate::diting_css::ContentValue,
    tree: &DomTree,
    nid: NodeId,
    state: &mut CounterState,
    quotes: Option<&[String]>,
    out: &mut String,
) {
    use crate::diting_css::ContentValue;
    match cv {
        ContentValue::Str(s) => out.push_str(s),
        ContentValue::Attr(name) => {
            let v = tree
                .with_node(nid, |n| n.get_attribute(name).map(|s| s.to_string()))
                .flatten();
            out.push_str(&v.unwrap_or_default());
        }
        ContentValue::Counter { name, style } => {
            let v = state
                .map
                .get(name)
                .and_then(|s| s.last())
                .map(|e| e.0)
                .unwrap_or(0);
            out.push_str(&crate::diting_css::format_counter_value(v, *style));
        }
        ContentValue::Counters { name, sep, style } => {
            let stack = state.map.get(name);
            match stack {
                Some(s) if !s.is_empty() => {
                    let parts: Vec<String> = s
                        .iter()
                        .map(|e| crate::diting_css::format_counter_value(e.0, *style))
                        .collect();
                    out.push_str(&parts.join(sep));
                }
                _ => out.push_str(&crate::diting_css::format_counter_value(0, *style)),
            }
        }
        ContentValue::OpenQuote => {
            let (open, _) = quote_pair(quotes, state.quote_depth);
            out.push_str(open);
            state.quote_depth += 1;
        }
        ContentValue::CloseQuote => {
            // Unbalanced close-quote (nothing open) generates nothing.
            if state.quote_depth > 0 {
                state.quote_depth -= 1;
                let (_, close) = quote_pair(quotes, state.quote_depth);
                out.push_str(close);
            }
        }
        ContentValue::NoQuote { close } => {
            if *close {
                state.quote_depth = state.quote_depth.saturating_sub(1);
            } else {
                state.quote_depth += 1;
            }
        }
        ContentValue::List(parts) => {
            for part in parts {
                resolve_content_into(part, tree, nid, state, quotes, out);
            }
        }
    }
}

/// diting has no ::before/::after generated content; the `q` marks are the
/// one piece of UA-generated text real pages rely on, so synthesize the
/// open/close leaves directly around the flattened q's children, in the q's
/// own font context.
#[allow(clippy::too_many_arguments)]
fn wrap_q_quotes(
    tree: &DomTree,
    child: NodeId,
    child_tag: &str,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    sub_children: &mut Vec<taffy::tree::NodeId>,
) {
    if child_tag != "q" {
        return;
    }
    let (open, close) = q_quote_pair(q_ancestor_quote_depth(tree, child));
    let (fs, b, lh) = font_context(tree, child, styles, fonts);
    let col = color_context(tree, child, styles);
    let deco = decoration_context(tree, child, styles);
    let vs = valign_shift(tree, child, styles, fonts);
    let mono = mono_context(tree, child, styles);
    let ws = word_spacing_context(tree, child, styles);
    let sc = small_caps_context(tree, child, styles);
    let mut wrapped = build_word_leaves(open, fs, b, col, lh, deco, vs, mono, ws, sc, fonts, taffy_tree);
    wrapped.append(sub_children);
    wrapped.extend(build_word_leaves(close, fs, b, col, lh, deco, vs, mono, ws, sc, fonts, taffy_tree));
    *sub_children = wrapped;
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
        for child in render_children(tree, id) {
            estimate_into(tree, child, styles, est);
        }
    }
    for child in render_children(tree, id) {
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
            for c in render_children(tree, id) {
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
    meta: &mut TableBuildMeta,
    font_size: f32,
    lh_elem: f32,
) -> Vec<taffy::tree::NodeId> {
    enum RunSeg {
        Text(String, f32, bool, [u8; 4], f32, TextDecorations, f32, bool, f32, WhiteSpace, bool),
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
            if !text.trim().is_empty() || {
                let any_preserve = segs.iter().any(|s| matches!(s, RunSeg::Text(.., w, _) if w.preserves_spaces() || w.preserves_newlines()));
                any_preserve
            } {
                let RunSeg::Text(_, fs, bold, color, lh, deco, vs, mono, wsp, _, sc) = &segs[0] else { unreachable!() };
                // white-space inherits, so the preserve-wins winner across
                // segments rides the whole run (the "any nowrap pins the
                // run" precedent, ranked across the family).
                let run_ws = segs.iter().fold(WhiteSpace::Normal, |acc, s| match s {
                    RunSeg::Text(.., w, _) if ws_rank(*w) > ws_rank(acc) => *w,
                    _ => acc,
                });
                // Anonymous flow columns own no element style; text-overflow is
                // non-inherited, so its owner is unknown here — v1-off.
                let run_ellipsis = false;
                if let Ok(leaf) = taffy_tree.new_leaf_with_context(
                    Style::default(),
                    TextLeaf::Run { text, font_size: *fs, bold: *bold, color: *color, line_height: *lh, decorations: *deco, baseline_shift: *vs, mono: *mono, word_spacing: *wsp, ws: run_ws, ellipsis: run_ellipsis, tokens: std::cell::RefCell::new(None), small_caps: *sc },
                ) {
                    flow_children.push(leaf);
                }
            }
            return;
        }
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        for seg in segs {
            match seg {
                RunSeg::Text(text, fs, bold, color, lh, deco, vs, mono, wsp, _, sc) => {
                    leaves.extend(build_word_leaves(&text, fs, bold, color, lh, deco, vs, mono, wsp, sc, fonts, taffy_tree))
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
            // Inline-flavored replaced elements (UA inline-block form
            // controls included) join the text run; the rest stay block
            // siblings (current shipped behavior for img/video).
            let inline_atom =
                inline_level || child_display == Some(CssDisplay::InlineBlock);
            let sd = if inline_atom && !out_of_flow {
                strut_descent_for(tree, child, styles, fonts)
            } else {
                0.0
            };
            let leaf = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map, sd);
            if let Some(leaf) = leaf {
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
            let (fs, b, lh) = font_context(tree, child, styles, fonts);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { lh_elem };
            let col = color_context(tree, child, styles);
            let deco = decoration_context(tree, child, styles);
            let vs = valign_shift(tree, child, styles, fonts);
            let mono = mono_context(tree, child, styles);
            let ws = word_spacing_context(tree, child, styles);
            let nws = tree.with_node(child, |n| n.parent).flatten().and_then(|p| styles.get(&p)).and_then(|s| s.white_space).unwrap_or(WhiteSpace::Normal);
            let sc = small_caps_context(tree, child, styles);
            run.push(RunSeg::Text(text, fs, b, col, lh, deco, vs, mono, ws, nws, sc));
        } else if child_display == Some(CssDisplay::InlineBlock) && !out_of_flow {
            // Atomic inline-level box (obscura#750 family): keeps its own
            // subtree box, joins the run as one shrink-to-fit unit.
            if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
                run.push(RunSeg::Nodes(vec![sub]));
            }
        } else if inline_level && !out_of_flow {
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta);
            if let Some(sub) = sub {
                let mut sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                wrap_q_quotes(tree, child, &child_tag, styles, fonts, taffy_tree, &mut sub_children);
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
            if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
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
                for child in render_children(tree, cur) {
                    stack.push(child);
                }
            }
        }
    }
    true
}

/// A `rowspan > 1` cell lifted out of its row wrapper. Its grid slot stays
/// occupied by an invisible placeholder leaf (no `node_map` entry, so paint
/// and hit-testing skip it) that keeps the column-group geometry, while the
/// cell itself becomes an absolutely-positioned child of the table node —
/// the post-layout fixup pass resolves its insets from the final row and
/// placeholder boxes.
struct SpanCell {
    taffy: taffy::tree::NodeId,
    placeholder: taffy::tree::NodeId,
    dom: NodeId,
    col: usize,
    col_span: usize,
    row: usize,
    row_span: usize,
}

/// One table's build-side record for the post-layout span fixup: the row
/// wrappers tagged with their GRID row index (rows without cells generate
/// no wrapper) and the border inset the absolute offsets resolve against.
struct TableSpans {
    rows: Vec<(usize, taffy::tree::NodeId)>,
    cells: Vec<SpanCell>,
    origin: (f32, f32),
}

/// Build-time accumulators threaded through `build_element` for the table
/// engine: the fixup jobs (consumed after the main layout) and the
/// collapsed-border edge marks (consumed by the paint walk).
#[derive(Default)]
pub(crate) struct TableBuildMeta {
    span_jobs: Vec<TableSpans>,
    /// dom id → [top, right, bottom, left]: that border side sits on a
    /// shared grid line under `border-collapse` → paint it at half width
    /// so two adjacent 2px borders read as one 2px line (Chrome centers
    /// the resolved border on the grid line; halves reproduce that for
    /// equal widths. Width conflicts keep proportional halves, not
    /// winner-takes-all — a documented v2 approximation).
    collapsed_edges: HashMap<NodeId, [bool; 4]>,
}

/// The `vertical-align` slot (declaration or legacy `valign` attribute,
/// already merged in the cascade) as the cell-content justify rule.
fn cell_valign_justify(styles: &HashMap<NodeId, ComputedStyle>, dom: &NodeId) -> JustifyContent {
    match styles.get(dom).and_then(|s| s.vertical_align) {
        Some(crate::diting_css::VerticalAlign::Top)
        | Some(crate::diting_css::VerticalAlign::Baseline) => JustifyContent::FLEX_START,
        Some(crate::diting_css::VerticalAlign::Bottom) => JustifyContent::FLEX_END,
        // Absent/unknown = Chrome's UA middle default.
        _ => JustifyContent::CENTER,
    }
}

/// Table layout (display:table family), v1 model:
///
/// The table becomes a taffy flex COLUMN of row wrappers; each row wrapper
/// is a non-wrapping flex ROW of its cells (cells themselves build through
/// the normal element paths, so every inline/block behavior inside a cell
/// keeps working); thead/tbody/tfoot flatten into their tr children.
///
/// Column alignment comes from a pre-measure pass — each row wrapper is laid
/// out at max-content width, per-column maxima are collected, then every
/// cell's flex-basis is pinned to its column max (grow 0 / shrink 1). With
/// a uniform basis per column, taffy's proportional shrink/grow preserves
/// alignment under ANY final table width (authored or shrink-to-fit), so
/// columns line up across rows the way a real table algorithm produces.
///
/// Width: authored table width passes through; otherwise the table
/// shrink-to-fits the summed column maxima with a 100%-of-containing-block
/// max clamp (the same fit-content idiom `resolve_sizing_keywords` uses).
/// `border-collapse: collapse` realizes as zero gaps between rows/cells;
/// the separate initial gets Chrome's default 2px border-spacing.
///
/// v1 known limits: caption renders top-only
/// (caption-side:bottom unstyled); colgroup/col contribute widths only;
/// `table-layout: fixed` requires an authored table width (an auto-width
/// fixed table falls back to the auto algorithm).
#[allow(clippy::too_many_arguments)]
fn build_table(
    tree: &DomTree,
    id: NodeId,
    style: &ComputedStyle,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
    flattened: &mut HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    run_wrappers: &mut Vec<taffy::tree::NodeId>,
    meta: &mut TableBuildMeta,
) -> Option<taffy::tree::NodeId> {
    let tag_of = |nid: NodeId| -> String {
        tree.with_node(nid, |n| n.as_element().map(|e| e.local.to_string()))
            .flatten()
            .unwrap_or_default()
    };
    // Rows in document order, row groups flattened; CSS `display: table-row`
    // children count as rows too (the CSS-authored minimum). The caption and
    // the colgroup/col width sources ride the same scan. `row_group` keeps
    // each row's thead/tbody/tfoot ancestor (parallel to `row_ids`) so the
    // wrapper assembly can re-group what this scan flattens — a group with
    // a real (stand-in) box gets rects/gBCR/sticky for free, and the box is
    // what `position: sticky` on a row group keys off in Chrome.
    let mut row_ids: Vec<NodeId> = Vec::new();
    let mut row_group: Vec<Option<NodeId>> = Vec::new();
    let mut caption_id: Option<NodeId> = None;
    let mut col_nodes: Vec<NodeId> = Vec::new();
    for child in tree.children(id) {
        match tag_of(child).as_str() {
            "tr" => {
                row_ids.push(child);
                row_group.push(None);
            }
            "caption" => caption_id = caption_id.or(Some(child)),
            "colgroup" | "col" => col_nodes.push(child),
            "thead" | "tbody" | "tfoot" => {
                for gc in tree.children(child).into_iter().filter(|gc| tag_of(*gc) == "tr") {
                    row_ids.push(gc);
                    row_group.push(Some(child));
                }
            }
            _ => {
                if styles.get(&child).and_then(|s| s.display) == Some(CssDisplay::TableRow) {
                    row_ids.push(child);
                    row_group.push(None);
                }
            }
        }
    }

    // `collapse` = shared borders → zero gaps; separate (the UA initial)
    // gets Chrome's default 2px border-spacing. Same value on both axes:
    // vertical between row wrappers, horizontal between cells.
    let gap = match style.border_collapse {
        Some(crate::diting_css::BorderCollapse::Collapse) => 0.0,
        _ => 2.0,
    };

    // Grid placement (spans batch): each cell claims the leftmost run of
    // free column slots wide enough for its colspan; a rowspan cell also
    // occupies those slots in the FOLLOWING rows, so later rows skip under
    // it — HTML table formatting. Ragged rows keep the leftmost-placement
    // behavior of the pre-span engine (no spans ⇒ column == cell index,
    // byte-identical attribution to the old path).
    struct CellSlot {
        taffy: taffy::tree::NodeId,
        dom: NodeId,
        col: usize,
        col_span: usize,
        row: usize,
        row_span: usize,
        /// An invisible slot-holder for a lifted rowspan cell — carries the
        /// column basis but paints nothing and takes no valign treatment.
        phantom: bool,
        /// A synthesized anonymous cell (CSS2.2 §17.2.1): no DOM box of its
        /// own, so it takes no collapsed-edge marks and never pins a fixed
        /// column from a member's style.
        anon: bool,
    }
    let n_rows = row_ids.len();
    let mut occupied: Vec<Vec<bool>> = vec![Vec::new(); n_rows];
    let span_attr = |cid: NodeId, name: &str| -> usize {
        tree.with_node(cid, |n| n.get_attribute(name).map(|v| v.to_string()))
            .flatten()
            .and_then(|v: String| v.trim().parse::<usize>().ok())
            .filter(|v| *v >= 1)
            .unwrap_or(1)
    };
    // `table-layout: fixed` needs an authored table width (an auto-width
    // fixed table falls back to the auto algorithm — the browser guidance
    // posture for "fixed needs width"). Under fixed, column widths come
    // from authored sources only: colgroup/col `width` attributes and
    // first-row cells; later-row content never widens a column. The
    // content-measurement passes still run (row heights under rowspan read
    // them), but the pinning below ignores their column maxima.
    let fixed = matches!(style.table_layout, Some(crate::diting_css::TableLayout::Fixed))
        && style.width.is_some();
    // Per-column authored widths for the fixed layout. None = auto column
    // (its share of the leftover width comes from flex-grow at layout).
    let mut col_authored: Vec<Option<f32>> = Vec::new();
    if fixed && !col_nodes.is_empty() {
        let px_attr = |nid: NodeId| -> Option<f32> {
            tree.with_node(nid, |n| n.get_attribute("width").map(|v| v.trim().to_string()))
                .flatten()
                .and_then(|v| {
                    // HTML bare number; tolerate a px suffix (pages write
                    // width="100px" against the spec). Percentages stay
                    // unparsed — no resolved table width to fold against.
                    let t = v.trim();
                    let t = t.strip_suffix("px").map(str::trim).unwrap_or(t);
                    t.parse::<f32>().ok().filter(|w| *w > 0.0)
                })
        };
        for cn in &col_nodes {
            // A colgroup's own width covers its span when it carries no col
            // children; each col child carries its own (span spreads one
            // width over a run of columns).
            let cols: Vec<NodeId> = tree
                .children(*cn)
                .into_iter()
                .filter(|gc| tag_of(*gc) == "col")
                .collect();
            let sources: Vec<(NodeId, usize)> = if cols.is_empty() {
                vec![(*cn, span_attr(*cn, "span"))]
            } else {
                cols.into_iter().map(|c| (c, span_attr(c, "span"))).collect()
            };
            for (c, span) in sources {
                let authored = px_attr(c);
                let start = col_authored.len();
                col_authored.resize(start + span, None);
                if let Some(w) = authored {
                    for slot in &mut col_authored[start..start + span] {
                        *slot = Some(w);
                    }
                }
            }
        }
    }
    let mut span_cells: Vec<SpanCell> = Vec::new();
    // Placeholders owed to rows below by rowspan cells processed above: each
    // continuation row needs its own slot-holder leaf (a taffy node has one
    // parent), else its own cells collapse to x=0.
    let mut pend_ph: Vec<Vec<(taffy::tree::NodeId, NodeId, usize, usize)>> = vec![Vec::new(); n_rows];
    // (grid row, wrapper node, cells) — wrappers only exist for rows that
    // produced at least one cell box.
    let mut row_wrappers: Vec<(usize, taffy::tree::NodeId, Vec<CellSlot>)> = Vec::new();
    // CSS2.2 §17.2.1 anonymous cell synthesis: a run of consecutive row
    // children that are not table-cells — bare text most of all (a real
    // HTML parser foster-parents it out of the table; ours keeps it in
    // place, and skipping it here rendered the content invisible) or stray
    // elements — wraps into ONE anonymous cell box that claims a column
    // slot like any td. Whitespace-only text, comments, and display:none
    // elements generate no box.
    enum RowItem {
        Cell(NodeId),
        Anon(Vec<NodeId>),
    }
    for (row_idx, rid) in row_ids.iter().enumerate() {
        let mut items: Vec<RowItem> = Vec::new();
        for cid in tree.children(*rid) {
            if styles.get(&cid).and_then(|s| s.display) == Some(CssDisplay::TableCell) {
                items.push(RowItem::Cell(cid));
                continue;
            }
            if tree.with_node(cid, |n| n.is_text()).unwrap_or(false) {
                let blank = tree
                    .with_node(cid, |n| {
                        n.text_content_of_text_node().unwrap_or("").trim().is_empty()
                    })
                    .unwrap_or(true);
                if blank {
                    continue;
                }
            } else if tree.with_node(cid, |n| n.as_element().map(|_| ())).flatten().is_none()
                || styles.get(&cid).and_then(|s| s.display) == Some(CssDisplay::None)
            {
                continue;
            }
            match items.last_mut() {
                Some(RowItem::Anon(group)) => group.push(cid),
                _ => items.push(RowItem::Anon(vec![cid])),
            }
        }
        let (row_fs, _, row_lh) = font_context(tree, *rid, styles, fonts);
        let mut cells: Vec<CellSlot> = Vec::new();
        for item in items {
            let (cell_dom, col_span, anon) = match &item {
                RowItem::Cell(cid) => (*cid, span_attr(*cid, "colspan").min(1000), false),
                RowItem::Anon(group) => (group[0], 1, true),
            };
            let row_span = if anon {
                1
            } else {
                span_attr(cell_dom, "rowspan").min(65534).min(n_rows - row_idx).max(1)
            };
            // Leftmost run of col_span free slots in this row.
            let mut col = 0usize;
            loop {
                while occupied[row_idx].len() < col + col_span {
                    occupied[row_idx].push(false);
                }
                if (col..col + col_span).all(|i| !occupied[row_idx][i]) {
                    break;
                }
                col += 1;
            }
            let Some(cell_node) = (match &item {
                RowItem::Cell(cid) => build_element(
                    tree, *cid, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta,
                ),
                RowItem::Anon(group) => {
                    // Members build through the normal sibling path (text
                    // runs, inline flattening, replaced atoms included).
                    // An all-inline group sits on one shared line (the same
                    // flex row wrap a text run rides); a block-level member
                    // stacks the group vertically — the block-in-inline
                    // split reads top to bottom.
                    let mut kids: Vec<taffy::tree::NodeId> = Vec::new();
                    for &mid in group {
                        build_normal_sibling(
                            mid, tree, styles, images, fonts, taffy_tree, node_map,
                            flattened, run_wrappers, meta, false, row_fs, row_lh, &mut kids,
                        );
                    }
                    if kids.is_empty() {
                        None
                    } else {
                        let any_block = group.iter().any(|&mid| {
                            if tree.with_node(mid, |n| n.is_text()).unwrap_or(false) {
                                return false;
                            }
                            // Same default as build_normal_sibling: an element
                            // with no computed display is block-level.
                            !matches!(
                                styles.get(&mid).and_then(|s| s.display),
                                Some(CssDisplay::Inline) | Some(CssDisplay::InlineBlock)
                            )
                        });
                        // Two-level shape, mirroring a real cell: the OUTER
                        // node is the cell the pin pass restyles into a
                        // valign flex-COLUMN (a single-level row-wrap container
                        // would get clobbered into a column and stack its run
                        // wrappers); the inner node carries the content.
                        let inner = if any_block {
                            // Block-level member: the group stacks vertically
                            // — the block-in-inline split reads top to bottom.
                            taffy_tree
                                .new_with_children(Style { display: Display::Block, ..Default::default() }, &kids)
                                .ok()
                        } else {
                            // All-inline: merge the per-member run wrappers
                            // into ONE wrapper, the same shape a real cell's
                            // mixed run builds — nested wrappers inside a
                            // wrap container wrap at exact-fit widths. Kids
                            // are reparented before the empty shells drop
                            // (the span-flatten order at the sibling path).
                            let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
                            for w in &kids {
                                leaves.extend(taffy_tree.children(*w).unwrap_or_default().to_vec());
                            }
                            for w in kids {
                                run_wrappers.retain(|r| r != &w);
                                let _ = taffy_tree.remove(w);
                            }
                            match taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
                                Ok(wrapper) => {
                                    run_wrappers.push(wrapper);
                                    Some(wrapper)
                                }
                                Err(_) => None,
                            }
                        };
                        match inner {
                            Some(inner) => taffy_tree
                                .new_with_children(Style::default(), &[inner])
                                .ok(),
                            None => None,
                        }
                    }
                }
            }) else {
                continue; // display:none builds no box and claims no slot
            };
            // Fixed layout: a first-row single-column cell pins its column
            // (the computed width already folds the td/th `width` attribute
            // hint in below every author declaration). A `<col>` width
            // outranks it — CSS2.2 §17.5.2.1: the col element sets the
            // column, the first-row cell only fills a column the cols left
            // auto.
            if fixed && !anon && row_idx == 0 && col_span == 1 {
                let w = styles
                    .get(&cell_dom)
                    .and_then(|s| s.width)
                    .and_then(|l| match l {
                        crate::diting_css::Length::Px(px) => Some(px),
                        _ => None,
                    })
                    .filter(|w| *w > 0.0);
                if let Some(px) = w {
                    if col_authored.len() < col + 1 {
                        col_authored.resize(col + 1, None);
                    }
                    if col_authored[col].is_none() {
                        col_authored[col] = Some(px);
                    }
                }
            }
            // Claim the slots in this row and (for a rowspan) the rows below.
            for occ in &mut occupied[row_idx..row_idx + row_span] {
                occ.resize(col + col_span, false);
                occ[col..col + col_span].fill(true);
            }
            if row_span > 1 {
                // Lift the cell out of the flow: an invisible placeholder
                // holds its slot in this row (sized later to the column
                // group, so the row keeps its geometry), the real box joins
                // the table node as an absolute child once the fixup pass
                // knows the final bands. Every continuation row gets its own
                // placeholder too — same slot, same column basis.
                if let Ok(placeholder) = taffy_tree.new_leaf(Style::default()) {
                    span_cells.push(SpanCell {
                        taffy: cell_node,
                        placeholder,
                        dom: cell_dom,
                        col,
                        col_span,
                        row: row_idx,
                        row_span,
                    });
                    cells.push(CellSlot {
                        taffy: placeholder,
                        dom: cell_dom,
                        col,
                        col_span,
                        row: row_idx,
                        row_span,
                        phantom: true,
                        anon,
                    });
                    for pend in &mut pend_ph[row_idx + 1..row_idx + row_span] {
                        if let Ok(ph) = taffy_tree.new_leaf(Style::default()) {
                            pend.push((ph, cell_dom, col, col_span));
                        }
                    }
                } else {
                    cells.push(CellSlot {
                        taffy: cell_node,
                        dom: cell_dom,
                        col,
                        col_span,
                        row: row_idx,
                        row_span,
                        phantom: false,
                        anon,
                    });
                }
            } else {
                cells.push(CellSlot {
                    taffy: cell_node,
                    dom: cell_dom,
                    col,
                    col_span,
                    row: row_idx,
                    row_span: 1,
                    phantom: false,
                    anon,
                });
            }
        }
        // Slot order is column order: with rowspans above, a later DOM cell
        // can land LEFT of an earlier one (the earlier one's leftmost free
        // run jumped past occupied slots). Stable sort keeps DOM order
        // within a column (impossible — slots are exclusive — but cheap).
        cells.extend(pend_ph[row_idx].drain(..).map(|(ph, dom, col, col_span)| CellSlot {
            taffy: ph,
            dom,
            col,
            col_span,
            row: row_idx,
            row_span: 1,
            phantom: true,
            anon: false,
        }));
        cells.sort_by_key(|c| c.col);
        if cells.is_empty() {
            continue;
        }
        // The `height` attribute on tr (blitz#507) landed in the tr's
        // computed style as a px presentational hint; the wrapper is
        // synthetic, so carry it across here. Spec: it's a MINIMUM row
        // height — the row still grows for taller cells (STRETCH below).
        let row_height = styles
            .get(rid)
            .and_then(|s| s.height)
            .and_then(|l| match l {
                crate::diting_css::Length::Px(px) => Some(px),
                _ => None,
            });
        let row_style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: Some(AlignItems::STRETCH),
            flex_wrap: FlexWrap::NoWrap,
            gap: taffy::geometry::Size {
                width: LengthPercentage::length(gap),
                height: LengthPercentage::length(0.0),
            },
            size: taffy::geometry::Size {
                width: auto(),
                height: row_height.map(Dimension::length).unwrap_or_else(auto),
            },
            ..Default::default()
        };
        let cell_nodes: Vec<taffy::tree::NodeId> = cells.iter().map(|c| c.taffy).collect();
        if let Ok(row_node) = taffy_tree.new_with_children(row_style, &cell_nodes) {
            node_map.insert(row_node, *rid);
            row_wrappers.push((row_idx, row_node, cells));
        }
    }

    // The caption box rides first: a block-flow first child of the table,
    // spanning its width (caption-side top is the UA default; bottom is out
    // of scope for v1). Previously the element was dropped entirely — no box
    // built — so wikipedia infobox captions were invisible.
    let mut table_children: Vec<taffy::tree::NodeId> = Vec::with_capacity(row_wrappers.len() + 1);
    if let Some(cap) = caption_id {
        if let Some(cap_node) = build_element(
            tree, cap, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta,
        ) {
            table_children.push(cap_node);
        }
    }
    // Re-group consecutive rows that share a thead/tbody/tfoot ancestor
    // into a group wrapper (flex column, same border-spacing gap, keyed to
    // the group's DOM id). Geometry is unchanged: the table's own gap still
    // spaces the top-level children, and each group's internal gap spaces
    // its rows, so the total inter-row gaps are exactly the pre-grouping
    // N-1. What the wrapper BUYS: the group lands in node_map, so the
    // harvest gives it a rect — gBCR, backgrounds, and `position: sticky`
    // on row groups (Chrome sticks the whole group) all inherit the box.
    {
        let mut gi = 0usize;
        while gi < row_wrappers.len() {
            let (row_idx, row_node, _) = &row_wrappers[gi];
            match row_group.get(*row_idx).copied().flatten() {
                None => {
                    table_children.push(*row_node);
                    gi += 1;
                }
                Some(group_dom) => {
                    let mut members = vec![*row_node];
                    gi += 1;
                    while gi < row_wrappers.len()
                        && row_group.get(row_wrappers[gi].0).copied().flatten() == Some(group_dom)
                    {
                        members.push(row_wrappers[gi].1);
                        gi += 1;
                    }
                    let group_style = Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Column,
                        align_items: Some(AlignItems::STRETCH),
                        flex_wrap: FlexWrap::NoWrap,
                        gap: taffy::geometry::Size {
                            width: LengthPercentage::length(0.0),
                            height: LengthPercentage::length(gap),
                        },
                        ..Default::default()
                    };
                    if let Ok(group_node) = taffy_tree.new_with_children(group_style, &members) {
                        node_map.insert(group_node, group_dom);
                        table_children.push(group_node);
                    } else {
                        table_children.extend(members);
                    }
                }
            }
        }
    }
    let table_node = if table_children.is_empty() {
        taffy_tree
            .new_leaf(to_taffy_style(style, pct_height_resolves(tree, styles, id)))
            .ok()?
    } else {
        taffy_tree
            .new_with_children(
                to_taffy_style(style, pct_height_resolves(tree, styles, id)),
                &table_children,
            )
            .ok()?
    };
    node_map.insert(table_node, id);

    if row_wrappers.is_empty() {
        return Some(table_node);
    }

    // Pre-measure each row wrapper at max-content and harvest per-column
    // maxima. The measured closure is the same TextLeaf dispatch as the
    // root pass — a plain compute_layout would zero the text runs.
    let measure_pass = |taffy_tree: &mut TaffyTree<TextLeaf>, node: taffy::tree::NodeId| -> Option<(f32, f32)> {
        let space = taffy::geometry::Size {
            width: AvailableSpace::MaxContent,
            height: AvailableSpace::MaxContent,
        };
        // Rounding OFF for the pre-measure: word-leaf widths are fractional
        // advances, and taffy's round-to-nearest would drop the fractional
        // residue of a run's sum (61.4 -> 61) — the pinned column then sits
        // BELOW the content width and the final layout wraps a word
        // (batch-58 probe: `AA<span>BB</span>CC` needed 61.4, measured 61,
        // wrapped to two lines). Heights keep the old rounded behavior via
        // .round() at the harvest sites. The layout MUST be read before
        // re-enabling: with rounding off `layout()` reads unrounded_layout
        // (fresh), otherwise it reads final_layout, which the skipped
        // round_layout never wrote — i.e. stale values.
        taffy_tree.disable_rounding();
        let _ = taffy_tree.compute_layout_with_measure(node, space, |inputs, _id, ctx, style| {
            match ctx {
                Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                    let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                    measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                }
                Some(TextLeaf::Word { .. }) | Some(TextLeaf::Replaced { .. }) | None => {
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
                }
            }
        });
        let out = taffy_tree.layout(node).ok().map(|l| (l.size.width, l.size.height));
        taffy_tree.enable_rounding();
        out
    };
    // A lifted (rowspan) cell sits in no row wrapper: measure it standalone
    // at the given width availability.
    let measure_cell = |taffy_tree: &mut TaffyTree<TextLeaf>, node: taffy::tree::NodeId, width: AvailableSpace| -> (f32, f32) {
        let space = taffy::geometry::Size { width, height: AvailableSpace::MaxContent };
        taffy_tree.disable_rounding();
        let _ = taffy_tree.compute_layout_with_measure(node, space, |inputs, _id, ctx, style| {
            match ctx {
                Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                    let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                    let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                    measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                }
                Some(TextLeaf::Word { .. }) | Some(TextLeaf::Replaced { .. }) | None => {
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
                }
            }
        });
        let out = taffy_tree.layout(node).map(|l| (l.size.width, l.size.height)).unwrap_or((0.0, 0.0));
        taffy_tree.enable_rounding();
        out
    };
    // Column attribution in two passes: single-column cells land wholly in
    // their column; spanning cells then distribute their DEFICIT equally
    // across the spanned columns (the simple spec algorithm — a group
    // already wide enough takes nothing). Document order within each pass.
    let lifted: HashMap<(usize, usize), taffy::tree::NodeId> =
        span_cells.iter().map(|c| ((c.row, c.col), c.taffy)).collect();
    let mut col_max: Vec<f32> = Vec::new();
    let mut pass_b: Vec<(usize, usize, f32)> = Vec::new();
    for (row_idx, row_node, cells) in &row_wrappers {
        if measure_pass(taffy_tree, *row_node).is_none() {
            continue;
        }
        for cell in cells {
            let w = if let Some(real) = lifted.get(&(*row_idx, cell.col)) {
                measure_cell(taffy_tree, *real, AvailableSpace::MaxContent).0.ceil()
            } else {
                taffy_tree
                    .unrounded_layout(cell.taffy)
                    .size
                    .width
                    .ceil()
            };
            if cell.col_span > 1 {
                pass_b.push((cell.col, cell.col_span, w));
            } else {
                if cell.col >= col_max.len() {
                    col_max.resize(cell.col + 1, 0.0);
                }
                col_max[cell.col] = col_max[cell.col].max(w);
            }
        }
    }
    for (col, span, w) in pass_b {
        if col + span > col_max.len() {
            col_max.resize(col + span, 0.0);
        }
        let group = col_max[col..col + span].iter().sum::<f32>() + gap * (span - 1) as f32;
        if w > group {
            let share = (w - group) / span as f32;
            for m in &mut col_max[col..col + span] {
                *m += share;
            }
        }
    }

    // Pin every cell to its column(-group) max: uniform bases per column
    // keep the columns aligned under any final width; a spanning cell's
    // basis is the summed group so surplus and deficit move the group as a
    // unit. grow = basis makes surplus table width distribute across
    // columns proportional to their content width (the auto-layout
    // behavior); a zero-content column gets the minimal grow of 1 so it
    // still absorbs its share of an authored table width (Chrome: a single
    // empty column in `width:100px` is 100px wide). Shrink 1 takes the
    // deficit back proportionally. Both preserve alignment because the
    // ratios are uniform per column.
    let group_width = |col: usize, span: usize| -> f32 {
        let end = (col + span).min(col_max.len());
        let mut w = if col < end { col_max[col..end].iter().sum::<f32>() } else { 0.0 };
        if span > 1 {
            w += gap * (span - 1) as f32;
        }
        w
    };
    // Fixed-layout authored group: the summed spanned authored widths plus
    // interior gaps; None when any spanned column is auto (that group then
    // grows for its equal per-column share of the leftover instead).
    let authored_group = |col: usize, span: usize| -> Option<f32> {
        let end = (col + span).min(col_authored.len());
        (col < end && (col..end).all(|i| col_authored[i].is_some())).then(|| {
            col_authored[col..end].iter().map(|w| w.unwrap_or(0.0)).sum::<f32>()
                + if span > 1 { gap * (span - 1) as f32 } else { 0.0 }
        })
    };
    for (_, _, cells) in &row_wrappers {
        for cell in cells {
            // Fixed: an authored group is exact and frozen (no grow, no
            // shrink); an auto group takes zero basis and grows by its
            // spanned-column count — every row then distributes the same
            // leftover over the same per-column grow total, keeping columns
            // aligned (authored-ness is a property of the column, so rows
            // that cover all columns freeze the same total). Auto keeps the
            // content-measured basis with proportional grow/shrink.
            let (basis, grow, shrink) = if fixed {
                match authored_group(cell.col, cell.col_span) {
                    Some(w) => (w, 0.0, 0.0),
                    None => (0.0, cell.col_span as f32, 0.0),
                }
            } else {
                let basis = group_width(cell.col, cell.col_span);
                (basis, if basis > 0.0 { basis } else { 1.0 }, 1.0)
            };
            if let Ok(mut st) = taffy_tree.style(cell.taffy).cloned() {
                st.flex_basis = Dimension::length(basis);
                st.flex_grow = grow;
                st.flex_shrink = shrink;
                if fixed {
                    // Fixed columns don't clamp to their content: a long cell
                    // wraps or overflows its column instead of widening it.
                    st.min_size.width = LengthPercentageAuto::length(0.0);
                }
                // vertical-align (blitz#508): the declaration or the legacy
                // valign attribute (same computed slot) moves cell CONTENT,
                // not the box — the cell still fills the row via the
                // wrapper's STRETCH. A flex-column cell plus justify_content
                // models it; absent/unknown values take Chrome's UA middle
                // default (every browser's vertical-align:middle on cells).
                // Placeholders only carry the basis — no content to move.
                if !cell.phantom {
                    st.display = Display::Flex;
                    st.flex_direction = FlexDirection::Column;
                    st.justify_content = Some(cell_valign_justify(styles, &cell.dom));
                }
                let _ = taffy_tree.set_style(cell.taffy, st);
            }
        }
    }

    // --- row heights under rowspan (spans batch) -------------------------
    // Natural heights at the pinned bases (the lifted cells are out of the
    // rows, so rows size to their remaining content); each lifted cell's
    // height need then spreads across its spanned rows — a deficit over
    // their natural sum distributes equally, applied as row min-heights so
    // taller content can still grow a row.
    if !span_cells.is_empty() {
        let mut row_h: Vec<f32> = vec![0.0; n_rows];
        let mut raised: Vec<bool> = vec![false; n_rows];
        for (grid_row, row_node, _) in &row_wrappers {
            if let Some((_, h)) = measure_pass(taffy_tree, *row_node) {
                row_h[*grid_row] = h.round();
            }
        }
        for cell in &span_cells {
            // Under fixed the lifted cell's real width is its authored group;
            // the content-measured group is the fallback for auto groups (the
            // final auto width only exists post-layout — the estimate only
            // shapes this row-height pre-pass).
            let group_w = if fixed {
                authored_group(cell.col, cell.col_span)
                    .unwrap_or_else(|| group_width(cell.col, cell.col_span))
            } else {
                group_width(cell.col, cell.col_span)
            };
            let (_, ch) = measure_cell(taffy_tree, cell.taffy, AvailableSpace::Definite(group_w));
            let ch = ch.round();
            let spanned: Vec<usize> = (cell.row..(cell.row + cell.row_span).min(n_rows)).collect();
            if spanned.is_empty() {
                continue;
            }
            let cur: f32 = spanned.iter().map(|r| row_h[*r]).sum();
            if ch > cur {
                let share = (ch - cur) / spanned.len() as f32;
                for r in spanned {
                    row_h[r] += share;
                    raised[r] = true;
                }
            }
        }
        for (grid_row, row_node, _) in &row_wrappers {
            if raised[*grid_row] {
                if let Ok(mut st) = taffy_tree.style(*row_node).cloned() {
                    st.min_size.height = LengthPercentageAuto::length(row_h[*grid_row]);
                    let _ = taffy_tree.set_style(*row_node, st);
                }
            }
        }
    }

    // --- collapsed-border edge marks (spans batch) -----------------------
    // Under collapse an edge on a shared grid line paints at half width:
    // two adjacent 2px borders then read as one 2px line centered on the
    // line (Chrome's resolved-border rendering for equal widths). Marks are
    // index-based (neighbor-by-position, not neighbor-by-existence).
    if matches!(style.border_collapse, Some(crate::diting_css::BorderCollapse::Collapse)) {
        let total_cols = col_max.len();
        for (_, _, cells) in &row_wrappers {
            for cell in cells {
                // Phantoms paint nothing (no node_map entry); the lifted cell
                // itself carries its true grid extent from its origin row.
                // Anonymous cells own no DOM box either — the mark would land
                // on a member element and halve its authored border.
                if cell.phantom || cell.anon {
                    continue;
                }
                meta.collapsed_edges.insert(
                    cell.dom,
                    [
                        cell.row > 0,
                        cell.col + cell.col_span < total_cols,
                        cell.row + cell.row_span < n_rows,
                        cell.col > 0,
                    ],
                );
            }
        }
    }

    // Lift the rowspan cells onto the table node as absolute children; the
    // post-layout fixup pass resolves their insets from the final row and
    // placeholder boxes. Taffy resolves a `left/top` inset against the
    // parent's border-box origin plus its border width (flexbox.rs
    // `offset_main = start + border`), so carry the effective border — the
    // row wrappers' own locations already include the table's padding.
    if !span_cells.is_empty() {
        for cell in &span_cells {
            if let Ok(mut st) = taffy_tree.style(cell.taffy).cloned() {
                st.position = Position::Absolute;
                st.flex_grow = 0.0;
                st.flex_shrink = 0.0;
                st.display = Display::Flex;
                st.flex_direction = FlexDirection::Column;
                st.justify_content = Some(cell_valign_justify(styles, &cell.dom));
                let _ = taffy_tree.set_style(cell.taffy, st);
            }
            let _ = taffy_tree.add_child(table_node, cell.taffy);
        }
        meta.span_jobs.push(TableSpans {
            rows: row_wrappers.iter().map(|(g, n, _)| (*g, *n)).collect(),
            cells: std::mem::take(&mut span_cells),
            origin: (
                if style.border_style.is_some() { side_px(style.border_width.left) } else { 0.0 },
                if style.border_style.is_some() { side_px(style.border_width.top) } else { 0.0 },
            ),
        });
    }

    // Shrink-to-fit: without an authored width (an explicit `width: auto`
    // parses to None too), the table sizes to the summed column maxima
    // plus gaps, capped at the containing block width by a 100% max clamp
    // (the same fit-content idiom resolve_sizing_keywords uses).
    if style.width.is_none() && !col_max.is_empty() {
        let gaps = gap * col_max.len().saturating_sub(1) as f32;
        let shrink = col_max.iter().sum::<f32>() + gaps;
        if shrink > 0.0 {
            if let Ok(mut st) = taffy_tree.style(table_node).cloned() {
                st.size.width = Dimension::length(shrink);
                st.max_size.width = LengthPercentageAuto::percent(1.0);
                let _ = taffy_tree.set_style(table_node, st);
            }
        }
    }

    Some(table_node)
}

/// The composed-tree children that render under `id` (shadow DOM phase 2):
/// a shadow host renders its shadow tree's children instead of its light
/// children, and a `<slot>` renders its assigned light children — or, with
/// nothing assigned, its own fallback children. The slot element itself
/// never builds a box (display:contents equivalent). Everything else passes
/// through unchanged; light-tree walks elsewhere must stay tree-scoped.
fn render_children(tree: &DomTree, id: NodeId) -> Vec<NodeId> {
    let raw: Vec<NodeId> = match tree.shadow_root(id) {
        Some(root) => tree.children(root),
        None => tree.children(id),
    };
    let mut out = Vec::with_capacity(raw.len());
    for child in raw {
        if tree.is_html_slot_element(child) {
            match tree.assigned_nodes(child) {
                Some(assigned) if !assigned.is_empty() => out.extend(assigned),
                _ => out.extend(render_children(tree, child)),
            }
        } else {
            out.push(child);
        }
    }
    out
}

/// Text of every `<style>` element inside a shadow tree. Serialized HTML
/// never contains shadow trees, so only the live-tree CSS collection
/// (ops.rs) calls this. Hosts are visited in document order so
/// equal-specificity shadow rules stay deterministic; a HashMap iteration
/// over the registry would reshuffle them per run.
pub fn shadow_style_texts(tree: &crate::diting_dom::DomTree) -> Vec<String> {
    fn collect_into(
        tree: &crate::diting_dom::DomTree,
        id: crate::diting_dom::NodeId,
        out: &mut Vec<String>,
    ) {
        if let Some(root) = tree.shadow_root(id) {
            for desc in tree.descendants(root) {
                let is_style = tree
                    .with_node(desc, |n| {
                        n.as_element().is_some_and(|e| e.local.as_ref() == "style")
                    })
                    .unwrap_or(false);
                if is_style {
                    out.push(tree.text_content(desc));
                }
                // Nested hosts inside this shadow tree.
                if tree.shadow_root(desc).is_some() {
                    collect_into(tree, desc, out);
                }
            }
        }
        for child in tree.children(id) {
            collect_into(tree, child, out);
        }
    }
    let mut out = Vec::new();
    collect_into(tree, tree.document(), &mut out);
    out
}

#[allow(clippy::too_many_arguments)]
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
    meta: &mut TableBuildMeta,
) -> Option<taffy::tree::NodeId> {
    let node = build_element_inner(
        tree, id, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta,
    )?;
    append_pseudo_leaves(tree, id, styles, node, fonts, taffy_tree);
    Some(node)
}

/// ::before/::after boxes for a host whose taffy node was just built
/// (generated-content v1). The pseudo computed styles ride the host's entry
/// in `styles` (cascade side: pseudo_styles); here they become leaves spliced
/// into the host's child list. Replaced and table hosts are skipped in v1:
/// replaced boxes own no layout children, and table hosts reify their own
/// row structure through build_table.
fn append_pseudo_leaves(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    node: taffy::tree::NodeId,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
) {
    let Some(style) = styles.get(&id) else { return };
    let Some(pair) = style.pseudos.as_deref() else { return };
    let tag = tree
        .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    if is_replaced_tag(&tag) || style.display == Some(CssDisplay::Table) {
        return;
    }
    let mut children: Vec<taffy::tree::NodeId> =
        taffy_tree.children(node).unwrap_or_default().to_vec();
    let mut changed = false;
    if let Some(before) = &pair.before {
        changed |= pseudo_leaf(before, taffy_tree, &mut children, fonts, true);
    }
    if let Some(after) = &pair.after {
        changed |= pseudo_leaf(after, taffy_tree, &mut children, fonts, false);
    }
    if changed {
        let _ = taffy_tree.set_children(node, &children);
    }
}

/// Build one pseudo box and splice it into the host's child list; returns
/// whether the list changed. Block-ish pseudos become leaves carrying the
/// pseudo's taffy style — an EMPTY content box stays a plain style leaf so
/// padding/border/background/height survive (the clearfix shape). Inline
/// pseudos join the host's text: when the adjacent leaf is a pure-text run
/// their texts merge onto one leaf, so `li:before { content: "• " }` shares
/// the line with the label (Chrome's inline generated boxes take part in the
/// host's first/last line box). The merged text takes the run's
/// metrics/color — the v1 divergence.
fn pseudo_leaf(
    p: &ComputedStyle,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    children: &mut Vec<taffy::tree::NodeId>,
    fonts: &FontBook,
    is_before: bool,
) -> bool {
    if p.display == Some(CssDisplay::None) {
        return false;
    }
    let Some(crate::diting_css::ContentValue::Str(content)) = &p.content else {
        return false;
    };
    let insert = |leaf: taffy::tree::NodeId, children: &mut Vec<taffy::tree::NodeId>| {
        if is_before {
            children.insert(0, leaf);
        } else {
            children.push(leaf);
        }
    };
    match p.display {
        None | Some(CssDisplay::Inline) => {
            if content.trim().is_empty() {
                return false;
            }
            // Merge into the adjacent pure-text run when there is one
            // (first child for ::before, last for ::after).
            let adj_idx = if is_before { 0 } else { children.len().saturating_sub(1) };
            if let Some(&adj) = children.get(adj_idx) {
                if let Some(TextLeaf::Run {
                    text,
                    font_size,
                    bold,
                    color,
                    line_height,
                    decorations,
                    baseline_shift,
                    mono,
                    word_spacing,
                    ws,
                    ellipsis,
                    small_caps,
                    ..
                }) = taffy_tree.get_node_context(adj)
                {
                    let merged = if is_before {
                        format!("{}{}", content, text)
                    } else {
                        format!("{}{}", text, content)
                    };
                    let leaf = taffy_tree
                        .new_leaf_with_context(
                            Style::default(),
                            TextLeaf::Run {
                                text: merged,
                                font_size: *font_size,
                                bold: *bold,
                                color: *color,
                                line_height: *line_height,
                                decorations: *decorations,
                                baseline_shift: *baseline_shift,
                                mono: *mono,
                                word_spacing: *word_spacing,
                                ws: *ws,
                                ellipsis: *ellipsis,
                                tokens: std::cell::RefCell::new(None),
                                small_caps: *small_caps,
                            },
                        )
                        .ok();
                    if let Some(leaf) = leaf {
                        let _ = taffy_tree.remove(adj);
                        children[adj_idx] = leaf;
                        return true;
                    }
                    return false;
                }
            }
            match taffy_tree.new_leaf_with_context(Style::default(), {
                let fs = p.font_size.unwrap_or(crate::diting_css::DEFAULT_ROOT_FONT_SIZE);
                let bold = p.font_weight.is_some_and(|w| w >= 600);
                TextLeaf::Run {
                    text: content.clone(),
                    font_size: fs,
                    bold,
                    color: p.color.map_or([0, 0, 0, 255], |c| [c.0, c.1, c.2, c.3]),
                    line_height: effective_line_height(fonts, p.line_height.as_ref(), fs, bold),
                    decorations: p.text_decoration_line.unwrap_or_default(),
                    baseline_shift: 0.0,
                    mono: p.font_family.as_deref().is_some_and(crate::diting_css::wants_monospace),
                    word_spacing: p.word_spacing.unwrap_or(0.0),
                    ws: p.white_space.unwrap_or(WhiteSpace::Normal),
                    ellipsis: false,
                    tokens: std::cell::RefCell::new(None),
                    small_caps: p.font_variant_caps.unwrap_or(false),
                }
            }) {
                Ok(leaf) => {
                    insert(leaf, children);
                    true
                }
                Err(_) => false,
            }
        }
        _ => {
            // Pseudo-element content leaf: no DOM id to walk a containing-
            // block chain from, and content strings aren't height-sized —
            // keep percent passthrough (the pre-fold behavior).
            let taffy_style = to_taffy_style(p, true);
            let leaf = if content.trim().is_empty() {
                taffy_tree.new_leaf(taffy_style).ok()
            } else {
                taffy_tree
                    .new_leaf_with_context(
                        taffy_style,
                        {
                            let fs = p
                                .font_size
                                .unwrap_or(crate::diting_css::DEFAULT_ROOT_FONT_SIZE);
                            let bold = p.font_weight.is_some_and(|w| w >= 600);
                            TextLeaf::Run {
                                text: content.clone(),
                                font_size: fs,
                                bold,
                                color: p.color.map_or([0, 0, 0, 255], |c| [c.0, c.1, c.2, c.3]),
                                line_height: effective_line_height(fonts, p.line_height.as_ref(), fs, bold),
                                decorations: p.text_decoration_line.unwrap_or_default(),
                                baseline_shift: 0.0,
                                mono: p
                                    .font_family
                                    .as_deref()
                                    .is_some_and(crate::diting_css::wants_monospace),
                                word_spacing: p.word_spacing.unwrap_or(0.0),
                                ws: p.white_space.unwrap_or(WhiteSpace::Normal),
                                ellipsis: false,
                                tokens: std::cell::RefCell::new(None),
                                small_caps: p.font_variant_caps.unwrap_or(false),
                            }
                        },
                    )
                    .ok()
            };
            match leaf {
                Some(leaf) => {
                    insert(leaf, children);
                    true
                }
                None => false,
            }
        }
    }
}

fn build_element_inner(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
    flattened: &mut HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    run_wrappers: &mut Vec<taffy::tree::NodeId>,
    meta: &mut TableBuildMeta,
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
        return build_replaced_leaf(tree, id, styles, images, taffy_tree, node_map, 0.0);
    }

    // --- table layout (display:table family) ------------------------------
    // A table dispatches here BEFORE the block/inline partition: its rows
    // are reified as flex rows of cells, never run through the flow logic.
    if style.display == Some(CssDisplay::Table) {
        return build_table(
            tree, id, &style, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta,
        );
    }

    // Shadow hosts render their shadow tree here (composed-tree children;
    // unassigned light children and slot boxes never build).
    let child_ids: Vec<NodeId> = render_children(tree, id);

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
        Text(String, f32, bool, [u8; 4], f32, TextDecorations, f32, bool, f32, WhiteSpace, bool),
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
            if text.trim().is_empty() && !segs.iter().any(|s| matches!(s, RunSeg::Text(.., w, _) if w.preserves_spaces() || w.preserves_newlines())) {
                return;
            }
            let RunSeg::Text(_, fs, bold, color, lh, deco, vs, mono, wsp, _, sc) = &segs[0] else { unreachable!() };
            // white-space inherits, so the preserve-wins winner across
            // segments rides the whole run (the "any nowrap pins the run"
            // precedent, ranked across the family).
            let run_ws = segs.iter().fold(WhiteSpace::Normal, |acc, s| match s {
                RunSeg::Text(.., w, _) if ws_rank(*w) > ws_rank(acc) => *w,
                _ => acc,
            });
            // text-overflow is non-inherited and acts on the OWNING block's
            // inline overflow: the run truncates only when its owner also
            // clips — the classic nowrap + hidden + ellipsis pattern.
            let run_ellipsis = styles.get(&id).is_some_and(|s| {
                s.clips_descendants() && s.text_overflow == Some(TextOverflow::Ellipsis)
            });
            if let Ok(leaf) = taffy_tree.new_leaf_with_context(
                Style::default(),
                TextLeaf::Run { text, font_size: *fs, bold: *bold, color: *color, line_height: *lh, decorations: *deco, baseline_shift: *vs, mono: *mono, word_spacing: *wsp, ws: run_ws, ellipsis: run_ellipsis, tokens: std::cell::RefCell::new(None), small_caps: *sc },
            ) {
                direct.push(leaf);
                return;
            }
        }
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        for seg in segs {
            match seg {
                RunSeg::Text(text, fs, bold, color, lh, deco, vs, mono, wsp, _, sc) => {
                    leaves.extend(build_word_leaves(&text, fs, bold, color, lh, deco, vs, mono, wsp, sc, fonts, taffy_tree))
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

    let (font_size, _bold, lh_elem) = font_context(tree, id, styles, fonts);

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
                    meta,
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
                if let Some(f) = build_element(tree, *cid, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
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
            let taffy_style = to_taffy_style(&style, pct_height_resolves(tree, styles, id));
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
                    meta,
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
                        build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta)
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
                build_element(tree, child_ids[float_idx], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta)
            {
                pair_children.push(f);
            }
            if let Some(o) =
                build_element(tree, child_ids[opp_idx], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta)
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
                    meta,
                    font_size,
                    lh_elem,
                );
                let float_right = run_side == Some(crate::diting_css::FloatSide::Right);
                let mut rail_children: Vec<taffy::tree::NodeId> = Vec::new();
                for &i in &rail_idx {
                    if let Some(f) =
                        build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta)
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
                if let Some(f) = build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
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
            build_element(tree, float_dom, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta);
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
            meta,
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
            let inline_atom =
                inline_level || child_display == Some(CssDisplay::InlineBlock);
            let sd = if inline_atom && !atomic_container && !out_of_flow {
                strut_descent_for(tree, child, styles, fonts)
            } else {
                0.0
            };
            let leaf = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map, sd);
            if let Some(leaf) = leaf {
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
            let (fs, b, lh) = font_context(tree, child, styles, fonts);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let lh = if styles.get(&child).is_some() { lh } else { lh_elem };
            // First segment's node donates the whole run's color — the same
            // first-segment approximation fs/bold already use.
            let col = color_context(tree, child, styles);
            let deco = decoration_context(tree, child, styles);
            let vs = valign_shift(tree, child, styles, fonts);
            let mono = mono_context(tree, child, styles);
            let ws = word_spacing_context(tree, child, styles);
            let nws = tree.with_node(child, |n| n.parent).flatten().and_then(|p| styles.get(&p)).and_then(|s| s.white_space).unwrap_or(WhiteSpace::Normal);
            let sc = small_caps_context(tree, child, styles);
            run.push(RunSeg::Text(text, fs, b, col, lh, deco, vs, mono, ws, nws, sc));
        } else if child_display == Some(CssDisplay::InlineBlock) && !atomic_container && !out_of_flow {
            // An inline-level block is ATOMIC (Chrome line-box model): it keeps
            // its own subtree box and joins the run as one unit, like a replaced
            // leaf. Flattening it would let its content wrap across the parent's
            // line; keeping the box makes taffy size it shrink-to-fit
            // (min(max-content, available)) against the run — and our wrapping
            // runs can't build the overflow bomb upstream's NoWrap rows did
            // (obscura#750).
            if let Some(sub) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
                run.push(RunSeg::Nodes(vec![sub]));
            }
        } else if inline_level && !atomic_container && !out_of_flow {
            // A plain inline wrapper flattens into the enclosing run (upstream
            // is_flattenable_inline): the words wrap at the real block level.
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta);
            if let Some(sub) = sub {
                let mut sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                wrap_q_quotes(tree, child, &child_tag, styles, fonts, taffy_tree, &mut sub_children);
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
            if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map, flattened, run_wrappers, meta) {
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

    let taffy_style = to_taffy_style(&style, pct_height_resolves(tree, styles, id));
    let node = if direct.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &direct).ok()?
    };
    node_map.insert(node, id);
    resolve_sizing_keywords(taffy_tree, node, &style, fonts);
    Some(node)
}

/// How a form control lays its text run out inside the replaced box (form
/// paint polish batch): Chrome's control padding plus vertical centering
/// for the single-line controls, top-anchored for textarea, and the
/// select's reserved dropdown-arrow zone. Resolved at collect time so
/// paint stays tree-free, the same posture as [`FormWidget`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormRun {
    /// Text-like input: 2px side padding, vertically centered run.
    Input,
    /// Button-like input: light button-face field, label centered both axes.
    Button,
    /// Textarea: 2px padding, top-anchored — a multiline run centered
    /// vertically would jump as it grows.
    Textarea,
    /// Select: like Input plus a 16px arrow zone reserved at the right,
    /// where the executor paints the closed control's ▼ mark.
    Select,
}

/// A checkable input's native widget (form paint batch): the replaced box
/// draws the control itself — a bordered square with a ✓ for checkboxes, a
/// ring with an inner dot for radios. `checked` is resolved at collect time
/// (the live_checked mirror, else the parsed `checked` attribute) so paint
/// stays tree-free, the same posture as the alt run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormWidget {
    Checkbox { checked: bool },
    Radio { checked: bool },
    /// A range slider's static geometry (blitz#456): the thumb's position
    /// along the track as a 0..1 fraction of the inset track span, read
    /// from the live value at widget-construction time.
    Range { fraction: f32 },
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
    /// A `box-shadow` layer (blitz#349 family): `rect` is the element's own
    /// box, `radii` the same clamped per-corner values a `Bg` on this box
    /// gets, and dx/dy/blur/spread are the layer's px lengths. Outer layers
    /// paint BEFORE the element's `Bg` so the background covers the shadow
    /// inside the box edge; inset layers (`inset: true`) paint AFTER the
    /// background (and gradient) but BEFORE the `Border`, hard-clipped to
    /// the box — Chrome's inner-shadow phase.
    BoxShadow {
        rect: Rect,
        color: [u8; 4],
        radii: [(f32, f32); 4],
        dx: f32,
        dy: f32,
        blur: f32,
        spread: f32,
        inset: bool,
    },
    /// A `backdrop-filter: blur()` region (blitz#901 family): the first
    /// item a glass element emits, so it filters everything painted
    /// beneath it so far. `rect` is the element's border box and `radii`
    /// the same clamped per-corner values a `Bg` on this box gets. The
    /// blur may SAMPLE outside the box (a blur needs input) but the
    /// composited output clips to the ROUNDED border shape, not the
    /// axis-aligned rect — the exact bug upstream blitz#901 hit.
    BackdropFilter {
        rect: Rect,
        radii: [(f32, f32); 4],
        blur: f32,
    },
    /// A `background-image: linear-gradient(...)` fill (gradient batch).
    /// `stops` are (0..1 position, straight RGBA) ascending, the CSS angle
    /// is in degrees (0 = to top, clockwise), and `radii` are the same
    /// per-corner values a solid `Bg` would clip to — the fill follows the
    /// rounded box. Paints AFTER the element's `Bg` (background-color is
    /// the bottom layer in CSS); element-subtree opacity is already folded
    /// into the stop alphas at emission.
    BgGradient {
        rect: Rect,
        stops: Vec<(f32, [u8; 4])>,
        css_deg: f32,
        radii: [(f32, f32); 4],
    },
    /// A decoded raster image blitted into the replaced box (batch 5b):
    /// sized per object-fit and offset per object-position (batch 5c) —
    /// see [`object_paint_rect`]. `rect` is the element box and doubles as
    /// the clip (replaced content never escapes it); `paint_rect` is the
    /// computed blit destination.
    Image {
        rect: Rect,
        paint_rect: Rect,
        image: DecodedImage,
        /// Element-subtree opacity (animation batch A) multiplied into the
        /// source pixels at blit time — raster colors can't fold a group
        /// alpha any other way.
        alpha: f32,
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
        /// Element-subtree opacity (animation batch A) applied to the
        /// placeholder fill and the alt run's color at paint time.
        alpha: f32,
        /// A checkable input's native widget (form paint batch): when set,
        /// paint draws the control itself instead of text — the checkedness
        /// resolved here at collect time (live_checked mirror, else the
        /// parsed attribute), exactly the JS getter's precedence.
        widget: Option<FormWidget>,
        /// The control's run layout (form paint polish batch): when set,
        /// paint draws Chrome's default control shell (1px gray ring, white
        /// field — only while the author styled no background of their own)
        /// and insets/centers the text run per kind. Mutually exclusive with
        /// `widget` in practice: checkables resolve None here.
        form: Option<FormRun>,
        /// Caret (typing-cursor batch): (char offset, ink) in the focused
        /// text-entry control, resolved at collect time — the offset is the
        /// recorded selection's anchor (min(start, end)) clamped to the
        /// value's char count, the ink the control's color context (never
        /// the placeholder gray). Some only while this node holds focus AND
        /// a selection was recorded for it; paint keeps no tree access, so
        /// the whole condition is evaluated here. Deterministic always-on
        /// (no blink) — a screenshot must show it.
        caret: Option<(usize, [u8; 4])>,
    },
    /// A compiled svg subtree painted into its replaced box (svg v1): the
    /// op list is in viewBox user units with group transforms pre-flattened
    /// at collect time; paint maps it through the box (element sizing IS
    /// the zoom). Arc because the item list is cloned per band paint and
    /// the op list is immutable from here on.
    Svg {
        rect: Rect,
        render: std::sync::Arc<svg::SvgRender>,
        /// Element-subtree opacity (animation batch A) multiplied into every
        /// op color at paint time — the compiled op list is shared/immutable.
        alpha: f32,
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
    /// Begin a non-diagonal transform bracket (affine batch): everything
    /// between here and the matching [`PaintItem::ClearXf`] was emitted in
    /// LOCAL coordinates and maps through `xf` (CSS matrix order
    /// a b c d e f: x' = a·x + c·y + e, y' = b·x + d·y + f). Diagonal
    /// transforms (translate/scale) never use this — they pre-bake their
    /// geometry into item fields at collect time, so untouched pages keep
    /// the exact pre-batch path. CSS makes a transform a stacking context,
    /// so the bracket wraps the element's whole subtree by construction.
    SetXf { xf: [f32; 6] },
    /// End the nearest open [`PaintItem::SetXf`] bracket.
    ClearXf,
    /// Paint in CANVAS coordinates until the matching [`PaintItem::ClearXf`]
    /// (affine residuals batch): the pushed map REPLACES the open bracket's
    /// total instead of composing onto it, cancelling the enclosing
    /// transform. Emitted by the inline-band splice — its band rects come
    /// from `abs_by_node`, which already maps through ancestor transforms,
    /// so splicing them raw inside a bracket would double-map. The pair is
    /// self-contained; paint after the ClearXf returns to the bracket.
    SetXfCanvas,
    /// A uniform solid border: four bands on the border-box edges (rounded
    /// ring when `radii` are set), painted AFTER the element's Bg
    /// (background-clip: border-box draws the bg beneath the border) and
    /// before the subtree. Widths in CSS order (top right bottom left), px;
    /// `radii` are the same per-corner values a `BgCorner` fill would use —
    /// the ring is the outer rounded box minus the widths-inset inner box
    /// (inner radii shrink by the adjacent border widths). All-zero radii
    /// keep the historical four-band fast path.
    Border {
        rect: Rect,
        widths: [f32; 4],
        color: [u8; 4],
        radii: [(f32, f32); 4],
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
        /// `background-clip: text` fill (gradient-text batch): when the
        /// nearest boxed ancestor established one, glyph coverage samples
        /// this gradient across `area` instead of filling with `color` —
        /// the run's color is ignored entirely (CSS paints the background
        /// through the glyphs; `-webkit-text-fill-color: transparent` hides
        /// the normal fill, which we get by construction).
        gradient: Option<TextGradient>,
        /// Propagated `text-decoration-line` set (CSS 2.1 §16.3.1 — the
        /// decoration an ancestor declared paints across this inline's text
        /// in the TEXT's own color, so it rides the leaf, not the ancestor).
        decorations: TextDecorations,
        /// The run rides the monospace face (mono batch): a `monospace`
        /// member anywhere in the nearest ancestor's font-family list.
        mono: bool,
        /// Inherited `word-spacing` (CSS Text §7.1), px — folded into each
        /// space token's advance at measure AND paint so wrap, glyphs and
        /// decorations all separate by the same amount.
        word_spacing: f32,
        /// `text-overflow: ellipsis` limit (nowrap+ellipsis batch): when
        /// Some, paint drops trailing tokens and appends "…" so the ink fits
        /// this width; layout rects, scroll extents and selection keep the
        /// full text (the marker is a rendering effect, CSS UI §5.2).
        truncate_at: Option<f32>,
        /// The run's wrap tokens, pre-shaped from the leaf's measure-time
        /// memo (obscura#983's paint half): the glyph-pixel RasterCache
        /// holds no wrap geometry, so the decorations painter and a
        /// first-raster cache miss would re-shape the run on every repaint
        /// without this. Some only when the item's font params are the
        /// leaf's UNSCALED ones (identity d — a diagonal scale folds d into
        /// font_size/word_spacing and the memo no longer matches); word
        /// leaves carry None.
        tokens: Option<std::rc::Rc<[text::Token]>>,
        /// The run's white-space mode (pre family): selects the tokenizer
        /// and wrap regime at raster time — wrap_at=+inf alone can't tell
        /// `nowrap` (collapse, one line) from `pre` (preserve, hard breaks).
        ws: WhiteSpace,
        /// `text-shadow` layers (blitz#271 family, inherited), element
        /// opacity already folded into each color: painted UNDER the glyphs
        /// (and decorations), first-declared layer on top.
        text_shadow: Option<Vec<TextShadow>>,
        /// Synthesized small-caps (small-caps batch, inherited): lowercase
        /// runs render as uppercase glyphs at 70% size. Word leaves carry
        /// false — their segments are pre-uppercased at build time.
        small_caps: bool,
    },
}

/// A `background-clip: text` fill (gradient-text batch): the gradient a
/// clip:text element suppressed from its own box, re-aimed at the glyphs of
/// its subtree. Coverage samples the gradient at each pixel's own position
/// in the text item's coordinate space, so the fill tracks the gradient's
/// angle across the whole element, not per glyph.
#[derive(Debug, Clone)]
pub struct TextGradient {
    /// The clip element's background box (exactly what a `BgGradient` would
    /// have filled) in the text item's coordinate space — page space on the
    /// prebaked diagonal path, local inside a transform bracket, where the
    /// affine then carries the gradient through the rotation.
    pub area: Rect,
    /// Raw stops — the text item folds its own inherited alpha at attach
    /// time, so opacity between the clip element and the text composes.
    pub stops: Vec<(f32, [u8; 4])>,
    /// linear-gradient angle in CSS degrees (0 = to top, clockwise), same
    /// convention `BgGradient` carries.
    pub css_deg: f32,
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
    layout_dom_with_paint_and_images(
        tree,
        styles,
        fonts,
        viewport_width,
        viewport_height,
        None,
        None,
    )
}

/// [`layout_dom_with_paint`] with an injected table of fetched image bodies
/// (batch 6c): absolute `http(s)`/`file` URL → response body. `<img src>`
/// entries pointing at those URLs decode like data: URLs (PNG only); misses
/// keep the placeholder. The fetch itself is the caller's job — the screenshot
/// prefetch pass fills this from diting_net. `base_url` is the document URL
/// relative srcs resolve against to reach the table's absolute keys (same
/// join the fetchers used).
#[allow(clippy::too_many_arguments)]
pub fn layout_dom_with_paint_and_images(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
    network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>>,
    base_url: Option<&str>,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>) {
    let (rects, items, _order, _local_geom, _sticky_spans, _scroller_spans) = layout_dom_with_paint_order_and_images(
        tree,
        styles,
        fonts,
        viewport_width,
        viewport_height,
        network_bytes,
        base_url,
    );
    (rects, items)
}

/// Map a rect through a CSS-order affine array [a,b,c,d,e,f]: the diagonal
/// fast path keeps the historical two-corner normalize (negative scales
/// mirror); anything else maps all four corners and unions them — Chrome's
/// gBCR bounding-box behavior under rotation. Free-standing so
/// [`expand_wrapped_leaves`] can map through a recorded array — the walk's
/// `Xf` type is fn-local.
fn map_rect_arr(m: [f32; 6], r: Rect) -> Rect {
    if m[1] == 0.0 && m[2] == 0.0 {
        let (x1, x2) = (r.x * m[0] + m[4], (r.x + r.width) * m[0] + m[4]);
        let (y1, y2) = (r.y * m[3] + m[5], (r.y + r.height) * m[3] + m[5]);
        Rect {
            x: x1.min(x2),
            y: y1.min(y2),
            width: (x2 - x1).abs(),
            height: (y2 - y1).abs(),
        }
    } else {
        let pt = |x: f32, y: f32| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        let (x0, y0) = pt(r.x, r.y);
        let (x1, y1) = pt(r.x + r.width, r.y);
        let (x2, y2) = pt(r.x, r.y + r.height);
        let (x3, y3) = pt(r.x + r.width, r.y + r.height);
        let xs = [x0, x1, x2, x3];
        let ys = [y0, y1, y2, y3];
        let min_x = xs.iter().cloned().fold(f32::MAX, f32::min);
        let max_x = xs.iter().cloned().fold(f32::MIN, f32::max);
        let min_y = ys.iter().cloned().fold(f32::MAX, f32::min);
        let max_y = ys.iter().cloned().fold(f32::MIN, f32::max);
        Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y }
    }
}

/// Collect the canvas-space rects of everything a flattened inline hoisted:
/// descend through run wrappers (nested flattening wraps wrapper into
/// wrapper) and stop at leaves / boxed nodes, which carry their LOCAL rect
/// plus the accumulated map in `local_by_node` and a first-item index in
/// `node_first_item`. A wrapped run leaf is one taffy box covering every
/// line it broke into — split it with the same greedy-wrap truth the paint
/// path uses (in LOCAL space, where the wrap width still means what the
/// paint item's `wrap_at` means), then map each line band to canvas —
/// under rotation a horizontal line band is a vertical stripe, and the
/// mapped bbox of each band is the closest an axis-aligned Bg can get.
fn expand_wrapped_leaves(
    node: taffy::tree::NodeId,
    taffy_tree: &TaffyTree<TextLeaf>,
    wrapper_set: &std::collections::HashSet<taffy::tree::NodeId>,
    local_by_node: &HashMap<taffy::tree::NodeId, (Rect, [f32; 6])>,
    fonts: &FontBook,
    pieces: &mut Vec<Rect>,
    owners: &mut Vec<taffy::tree::NodeId>,
) {
    if wrapper_set.contains(&node) {
        for c in taffy_tree.children(node).unwrap_or_default() {
            expand_wrapped_leaves(c, taffy_tree, wrapper_set, local_by_node, fonts, pieces, owners);
        }
        return;
    }
    let Some((r, m)) = local_by_node.get(&node) else { return };
    if let Some(TextLeaf::Run { text, font_size, bold, line_height, mono, word_spacing, ws, tokens, small_caps, .. }) =
        taffy_tree.get_node_context(node)
    {
        let tokens = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
        // no-soft-wrap modes keep their single line here too: the affine
        // path expands a leaf into per-line tiles, and wrapping such a run
        // at the box width would split ink the rasterizer lays on one line.
        let lines = text::greedy_wrap(&tokens, if ws.no_soft_wrap() { None } else { Some(r.width.max(0.0)) }, *ws);
        for (i, line) in lines.iter().enumerate() {
            if line.width <= 0.0 {
                continue;
            }
            pieces.push(map_rect_arr(
                *m,
                Rect {
                    x: r.x,
                    y: r.y + i as f32 * line_height,
                    width: line.width,
                    height: *line_height,
                },
            ));
            owners.push(node);
        }
        return;
    }
    pieces.push(map_rect_arr(*m, *r));
    owners.push(node);
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
        Some(TextLeaf::Word { font_size, bold, line_height, baseline_shift, .. }) => {
            if layout.size.height > 0.0 {
                let b = rel_y + text_baseline(fonts, *font_size, *bold, *line_height) - baseline_shift;
                *best = Some(best.map_or(b, |x: f32| x.max(b)));
            }
            return;
        }
        Some(TextLeaf::Run { font_size, bold, line_height, baseline_shift, .. }) => {
            // A wrapped run's last line sits one line box above its bottom;
            // the shift pad is empty reservation, not a text line.
            let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
            let b = rel_y + layout.size.height - pad - line_height
                + text_baseline(fonts, *font_size, *bold, *line_height)
                - baseline_shift;
            *best = Some(best.map_or(b, |x: f32| x.max(b)));
            return;
        }
        Some(TextLeaf::Replaced { strut_descent }) => {
            // Baseline = the element box's bottom edge (the pad below it is
            // strut reservation, not element box).
            let b = rel_y + layout.size.height - strut_descent;
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
        Some(TextLeaf::Word { font_size, bold, line_height, baseline_shift, .. }) => {
            text_baseline(fonts, *font_size, *bold, *line_height) - baseline_shift
        }
        Some(TextLeaf::Run { font_size, bold, line_height, baseline_shift, .. }) => {
            let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
            height - pad - line_height + text_baseline(fonts, *font_size, *bold, *line_height)
                - baseline_shift
        }
        Some(TextLeaf::Replaced { strut_descent }) => height - strut_descent,
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
            // Textless box: bottom edge (CSS bottom margin-edge fallback —
            // run items carry no vertical margins in this model).
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
    network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>>,
    base_url: Option<&str>,
) -> LayoutCollect {
    let solved = layout_solve(
        tree,
        styles,
        fonts,
        viewport_width,
        viewport_height,
        network_bytes,
        base_url,
    );
    layout_collect(tree, styles, fonts, &solved, viewport_width)
}

/// The solved half of a layout run, split out for #395: the taffy tree after
/// every pass (build, static-position harvest, reparent, main solve, float
/// continuation, calc repair, table span placement) plus the auxiliary maps
/// the collect walk consumes. Paint-only property writes — transform and
/// opacity, everything a GSAP-style timeline seek lands — are invisible to
/// all of it: `to_taffy_style` maps only geometry families, the solve passes
/// consume only those, and baseline shifts read pure layout. A fresh
/// [`layout_collect`] over a cached [`SolvedGeometry`] is therefore
/// bit-identical to re-running the whole pipeline — which is what lets the
/// video pump repaint per-seek frames without the full-page relayout it used
/// to pay (43 solves × 114ms on the probe page).
pub struct SolvedGeometry {
    taffy_tree: TaffyTree<TextLeaf>,
    node_map: HashMap<taffy::tree::NodeId, NodeId>,
    images: HashMap<NodeId, DecodedImage>,
    static_pos: HashMap<NodeId, (f32, f32)>,
    baseline_shifts: HashMap<taffy::tree::NodeId, f32>,
    run_wrappers: Vec<taffy::tree::NodeId>,
    collapsed_edges: HashMap<NodeId, [bool; 4]>,
    flattened: HashMap<NodeId, Vec<taffy::tree::NodeId>>,
    /// The synthetic ICB the walk roots at; `None` marks an aborted solve
    /// (no element root, or a taffy error) — collect returns the same empty
    /// outputs the undivided pipeline returned on those paths.
    icb_node: Option<taffy::tree::NodeId>,
}

impl SolvedGeometry {
    /// DOM-side solved border boxes (DOM id → (w, h)): the container-query
    /// pass reads candidate container widths from here without a full
    /// collect. Display:none nodes have no taffy box and are absent — a
    /// missing box means "not queryable", matching the containment contract.
    pub(crate) fn dom_boxes(&self) -> HashMap<NodeId, (f32, f32)> {
        self.node_map
            .iter()
            .filter_map(|(t, d)| {
                self.taffy_tree
                    .layout(*t)
                    .ok()
                    .map(|l| (*d, (l.size.width, l.size.height)))
            })
            .collect()
    }

    /// The empty solve the early-return paths hand back: nothing walked,
    /// nothing collected.
    fn aborted() -> Self {
        SolvedGeometry {
            taffy_tree: TaffyTree::new(),
            node_map: HashMap::new(),
            images: HashMap::new(),
            static_pos: HashMap::new(),
            baseline_shifts: HashMap::new(),
            run_wrappers: Vec::new(),
            collapsed_edges: HashMap::new(),
            flattened: HashMap::new(),
            icb_node: None,
        }
    }
}

/// Solve the taffy tree for the live DOM (the first half of what
/// [`layout_dom_with_paint_order_and_images`] used to do in one body): build
/// the node tree, harvest static positions, reparent out-of-flow boxes onto
/// their containing blocks, run the block/flex/grid solve, then the
/// float-continuation, calc-repair and table-span fixups that re-solve when
/// they adjust geometry. See [`SolvedGeometry`] for why the result is worth
/// caching separately from the collected items.
pub fn layout_solve(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
    network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>>,
    base_url: Option<&str>,
) -> SolvedGeometry {
    // The document node is not an element; lay out from the first element
    // descendant (the <html> root), like upstream.
    let root = tree
        .children(tree.document())
        .into_iter()
        .find(|id| tree.with_node(*id, |n| n.is_element()).unwrap_or(false));
    layout_solve_rooted(
        tree,
        styles,
        fonts,
        viewport_width,
        viewport_height,
        network_bytes,
        base_url,
        root,
    )
}

/// Explicit-root variant (fabricated iframe documents): `root` is the
/// orphan subtree's topmost element — the whole downstream build
/// (build_element walk, layout_collect) runs from it, and the ICB is that
/// subtree's own viewport.
pub fn layout_solve_rooted(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
    viewport_height: f32,
    network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>>,
    base_url: Option<&str>,
    root: Option<NodeId>,
) -> SolvedGeometry {
    let mut taffy_tree = TaffyTree::new();
    let mut node_map: HashMap<taffy::tree::NodeId, NodeId> = HashMap::new();

    // Pre-pass (batches 5b + 6c): resolve every img src through the
    // ImageCache — data: URLs decode inline, http(s)/file URLs consult the
    // fetched-byte table; results are cached so repeated srcs and layout
    // re-runs decode once. Unresolvable imgs keep the batch-5a placeholder.
    let empty: HashMap<String, std::sync::Arc<Vec<u8>>> = HashMap::new();
    let cache = image::ImageCache::with_network(network_bytes.unwrap_or(&empty));
    let mut images: HashMap<NodeId, DecodedImage> = HashMap::new();
    fn scan_images(
        tree: &DomTree,
        id: NodeId,
        cache: &image::ImageCache,
        images: &mut HashMap<NodeId, DecodedImage>,
        viewport_width: f32,
        base_url: Option<&str>,
    ) {
        let is_img = tree
            .with_node(id, |n| n.as_element().map(|e| e.local.to_string() == "img"))
            .flatten()
            .unwrap_or(false);
        if is_img {
            let src = resolve_img_source(tree, id, viewport_width, base_url);
            if let Some(img) = src.as_deref().and_then(|s| cache.resolve(s)) {
                images.insert(id, (*img).clone());
            }
        }
        for child in render_children(tree, id) {
            scan_images(tree, child, cache, images, viewport_width, base_url);
        }
    }
    if let Some(root_id) = &root {
        scan_images(tree, *root_id, &cache, &mut images, viewport_width, base_url);
    }

    // Flattened inline wrappers (see build_element) recorded as
    // dom id → hoisted taffy children, for the union pass after the walk.
    let mut flattened: HashMap<NodeId, Vec<taffy::tree::NodeId>> = HashMap::new();
    // Run wrappers (the IFC stand-in flex rows) recorded at assembly for the
    // post-layout baseline-alignment pass (blitz#750 family) — they can't be
    // re-identified structurally (inline-block boxes share the same taffy
    // style), so the builders log them as they create them.
    let mut run_wrappers: Vec<taffy::tree::NodeId> = Vec::new();
    // Table span bookkeeping (rowspan/colspan): build_table logs each spanned
    // table here; the post-layout fixup reads the placeholder geometry and
    // places the lifted absolute cells over their spanned band.
    let mut table_meta = TableBuildMeta::default();
    let Some(root_id) = root else { return SolvedGeometry::aborted() };
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
        &mut table_meta,
    ) else {
        return SolvedGeometry::aborted();
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
                        Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                            let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                            let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                            measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
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
    // ancestor with position != static OR a non-none transform (CSS
    // Transforms §: a transformed ancestor is a containing block for both
    // absolute and fixed descendants). Fixed with no such ancestor —
    // positioned ancestors don't pin it — resolves to the root = the
    // initial containing block stand-in.
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
                let target_dom = {
                    let mut cur = tree.with_node(*dom_id, |n| n.parent).flatten();
                    while let Some(nid) = cur {
                        let ancestor = styles.get(&nid);
                        // CSS Transforms: a non-none transform makes an
                        // ancestor the containing block for BOTH absolute
                        // and fixed descendants. For fixed that's the only
                        // ancestor kind that overrides the viewport — a
                        // merely positioned ancestor doesn't pin it.
                        let transformed =
                            ancestor.is_some_and(|s| s.transform.is_some());
                        if transformed
                            || (!fixed
                                && ancestor.is_some_and(|s| {
                                    matches!(
                                        s.position,
                                        Some(PositionMode::Relative)
                                            | Some(PositionMode::Absolute)
                                            | Some(PositionMode::Fixed)
                                    )
                                }))
                        {
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
            for child in render_children(tree, id) {
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
            Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
            }
            // Word leaves keep their style-driven sizing (batch 4d only
            // added paint context — zero layout change).
            Some(TextLeaf::Word { .. }) | Some(TextLeaf::Replaced { .. }) | None => {
                taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
            }
        }
    });
    if measured.is_err() {
        return SolvedGeometry::aborted();
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
                        Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                            let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                            let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                            measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                        }
                        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                    },
                );
                if re.is_err() {
                    return SolvedGeometry::aborted();
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
                // to_taffy_style's px arms — skipped under authored
                // `box-sizing: border-box`, where the calc result already
                // measures to the border edge.
                let pb = side_px(st.padding.left)
                    + side_px(st.padding.right)
                    + if st.border_style.is_some() {
                        side_px(st.border_width.left) + side_px(st.border_width.right)
                    } else {
                        0.0
                    };
                let border_box = matches!(st.box_sizing, Some(crate::diting_css::BoxSizing::BorderBox));
                let infl = if border_box { 0.0 } else { pb };
                let Some(mut ts) = taffy_tree.style(tnid).ok().cloned() else { continue };
                if let Some(crate::diting_css::Length::Calc { percent, px }) = st.width {
                    let content = (cbw * percent / 100.0 + px).max(0.0);
                    ts.size.width = Dimension::length(content + infl);
                    // Children resolve percents against this element's CONTENT
                    // box — under border-box the calc result is the border
                    // box, so strip padding+border for the child basis.
                    repaired.insert(tnid, if border_box { (content - pb).max(0.0) } else { content });
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
                    Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                        let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                        let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                        measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                    }
                    _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                },
            );
            if re.is_err() {
                return SolvedGeometry::aborted();
            }
        }
    }
    // --- table span placement (spans batch) --------------------------------
    // The lifted rowspan cells sit on their table node as absolute children
    // with auto insets; now that the final layout exists, read each
    // placeholder's slot geometry and the spanned row wrappers' bands, and
    // pin the real cell over them (width = slot, height = first-row top to
    // last-row bottom). Registry order is inner-table-first (children build
    // before their parent table finishes), so walk it in REVERSE: an outer
    // table's lift re-runs layout before an inner table inside the lifted
    // cell reads its own rows, keeping nested spanned tables honest.
    if !table_meta.span_jobs.is_empty() {
        for job in table_meta.span_jobs.iter().rev() {
            let mut dirty = false;
            for cell in &job.cells {
                let Ok(ph) = taffy_tree.layout(cell.placeholder) else { continue };
                let Some(row_layout) = job
                    .rows
                    .iter()
                    .find(|(g, _)| *g == cell.row)
                    .and_then(|(_, n)| taffy_tree.layout(*n).ok())
                else {
                    continue;
                };
                // Band bottom: the last row wrapper inside the span (clamped
                // when the rowspan ran past the table's last row at build).
                let last = job
                    .rows
                    .iter()
                    .rfind(|(g, _)| *g < cell.row + cell.row_span)
                    .or_else(|| job.rows.last());
                let Some(last_layout) = last.and_then(|(_, n)| taffy_tree.layout(*n).ok()) else {
                    continue;
                };
                // Slot x and band y are table-border-box coordinates: the row
                // wrapper's location already folds the table's border+padding
                // in, the placeholder's location the row wrapper's.
                let x = row_layout.location.x + ph.location.x;
                let y_top = row_layout.location.y;
                let y_bot = last_layout.location.y + last_layout.size.height;
                if let Ok(mut st) = taffy_tree.style(cell.taffy).cloned() {
                    st.position = Position::Absolute;
                    st.inset = taffy::geometry::Rect {
                        left: LengthPercentageAuto::length((x - job.origin.0).max(0.0)),
                        top: LengthPercentageAuto::length((y_top - job.origin.1).max(0.0)),
                        right: LengthPercentageAuto::AUTO,
                        bottom: LengthPercentageAuto::AUTO,
                    };
                    st.size = taffy::geometry::Size {
                        width: Dimension::length(ph.size.width.max(0.0)),
                        height: Dimension::length((y_bot - y_top).max(0.0)),
                    };
                    let _ = taffy_tree.set_style(cell.taffy, st);
                    dirty = true;
                }
            }
            if dirty {
                let re = taffy_tree.compute_layout_with_measure(
                    icb_node,
                    available,
                    |inputs, _id, ctx, style| match ctx {
                        Some(TextLeaf::Run { text, font_size, bold, line_height, baseline_shift, mono, word_spacing, ws, tokens, small_caps, .. }) => {
                            let pad = shift_pad(*baseline_shift, leaf_descent(fonts, *font_size, *bold));
                            let shaped = run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps);
                            measure_text_leaf(&shaped, *line_height, pad, &inputs, *ws)
                        }
                        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                    },
                );
                if re.is_err() {
                    return SolvedGeometry::aborted();
                }
            }
        }
    }
    // Both trees round to the pixel grid inside compute_layout (taffy's
    // use_rounding defaults on; blitz rounds via the same path), so the
    // rect comparisons assume integer edges on both sides.

    // Baseline alignment of inline runs (blitz#750 family): per-line shifts
    // computed against the FINAL layout, applied during the collect walk.
    let baseline_shifts = compute_baseline_shifts(&taffy_tree, &run_wrappers, fonts);

    SolvedGeometry {
        taffy_tree,
        node_map,
        images,
        static_pos,
        baseline_shifts,
        run_wrappers,
        collapsed_edges: table_meta.collapsed_edges,
        flattened,
        icb_node: Some(icb_node),
    }
}

/// css-overflow-3 §3.3 overflow propagation (blitz#880): overflow on the
/// root element applies to the VIEWPORT itself; when that value is
/// `visible`, the first `body` child that generates a box hands ITS overflow
/// to the viewport instead (a `display:none` body generates no box and
/// propagates nothing). `hidden`/`clip` arriving there makes the page
/// unscrollable — the scrolling area collapses to exactly the viewport.
/// Every reader of the root scroll range (`scroll_extent`, `band_frame`)
/// must consult this or they disagree on the scrollable range.
pub(crate) fn effective_viewport_overflow(
    dom: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    root: NodeId,
    has_box: impl Fn(NodeId) -> bool,
) -> Overflow {
    let ov =
        |id: NodeId| styles.get(&id).map(|s| s.effective_overflow()).unwrap_or(Overflow::Visible);
    let local = |id: NodeId| -> Option<String> {
        dom.get_node(id).and_then(|n| {
            n.as_element()
                .map(|e| e.local.to_ascii_lowercase().as_ref().to_string())
        })
    };
    // The viewport-carrying element: a body query climbs to its parent (the
    // root element in a normal document); anything else is taken as-is.
    let root_elem = if local(root).as_deref() == Some("body") {
        dom.get_node(root).and_then(|n| n.parent).unwrap_or(root)
    } else {
        root
    };
    let root_ov = ov(root_elem);
    if root_ov != Overflow::Visible {
        return root_ov;
    }
    for child in dom.children(root_elem) {
        if local(child).as_deref() == Some("body") {
            // First body child only: no box → no propagation, and no
            // fallback to a later body (WPT overflow-body-propagation-016).
            return if has_box(child) { ov(child) } else { root_ov };
        }
    }
    root_ov
}

/// Whether `body`'s overflow propagated to the viewport (css-overflow-3
/// §3.3, blitz#880): its used value then flips back to `visible`, so the
/// body must not clip its own descendants in paint.
pub(crate) fn body_overflow_propagates(
    dom: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    body: NodeId,
    has_box: impl Fn(NodeId) -> bool,
) -> bool {
    let Some(html) = dom.get_node(body).and_then(|n| n.parent) else {
        return false;
    };
    let html_ov =
        styles.get(&html).map(|s| s.effective_overflow()).unwrap_or(Overflow::Visible);
    if html_ov != Overflow::Visible {
        return false;
    }
    // Propagation only ever happens for the first body child of the root
    // element, and only when it generates a box and carries a non-visible
    // value (visible hands up nothing).
    let first_body = dom.children(html).into_iter().find(|c| {
        dom.get_node(*c)
            .and_then(|n| {
                n.as_element()
                    .map(|e| e.local.to_ascii_lowercase().as_ref() == "body")
            })
            .unwrap_or(false)
    });
    first_body == Some(body)
        && has_box(body)
        && styles
            .get(&body)
            .map(|s| s.effective_overflow())
            .unwrap_or(Overflow::Visible)
            != Overflow::Visible
}

/// Resolve a sticky inset (`top`/`bottom`/`left`/`right`) to px against the
/// scrollport dimension it sticks to — per Blink, sticky insets resolve
/// percentages against the SCROLLPORT (the viewport for the root scroller),
/// not the containing block.
pub fn sticky_inset_px(l: &crate::diting_css::Length, port_dim: f32) -> f32 {
    match l {
        crate::diting_css::Length::Px(v) => *v,
        crate::diting_css::Length::Percent(p) => port_dim * p / 100.0,
        crate::diting_css::Length::Calc { percent, px } => port_dim * percent / 100.0 + px,
        // Keyword sizing lengths (auto/min-content/max-content/…) are not
        // valid inset values — the inset parse rejects them, but the enum
        // allows them, so compute as "no inset" rather than panicking.
        _ => 0.0,
    }
}

/// One axis of the sticky constraint math (CSS Position 3, horizontal-tb
/// v1 — root scroller only): how far the box travels to stay visible in the
/// scrollport, given its in-flow position/size, the two stick insets
/// (start = top/left, end = bottom/right, already px), the scrollport's
/// origin/size, and the containing-block span the shift clamps to.
/// End applies first, start overrides on overconstraint (Blink's
/// sticky_constraining_rect order — start wins in LTR horizontal-tb).
#[allow(clippy::too_many_arguments)]
pub fn sticky_axis_shift(
    pos: f32,
    size: f32,
    start: Option<f32>,
    end: Option<f32>,
    port_start: f32,
    port_size: f32,
    cb_start: f32,
    cb_end: f32,
) -> f32 {
    let mut shift = 0.0f32;
    if let Some(end) = end {
        // Keep the box's end edge at least `end` inside the port's end
        // edge — pushes the box back (negative shift) as the port scrolls
        // past; never pulls it forward.
        shift = ((port_start + port_size - end) - (pos + size)).min(0.0);
    }
    if let Some(start) = start {
        // Pin to the port's start edge + inset once the port scrolls past
        // the in-flow spot; start wins over the end inset when both fire.
        shift = ((port_start + start) - pos).max(shift).max(0.0);
    }
    // The box never escapes its containing block: full down-travel stops
    // where the box's end edge meets the CB's end, and up-travel stops at
    // the CB's start (v1 uses the CB border box; spec says margin box).
    shift.max(cb_start - pos).min((cb_end - size) - pos)
}

/// Translate one rect-carrying paint item by (tx, ty) in its own coordinate
/// space. Shadow dx/dy and gradient angles are offsets/directions, not
/// positions — untouched.
fn translate_item(it: &mut PaintItem, tx: f32, ty: f32) {
    fn tr(r: &mut Rect, tx: f32, ty: f32) {
        r.x += tx;
        r.y += ty;
    }
    match it {
        PaintItem::Bg { rect, .. }
        | PaintItem::BgCorner { rect, .. }
        | PaintItem::BoxShadow { rect, .. }
        | PaintItem::BackdropFilter { rect, .. }
        | PaintItem::BgGradient { rect, .. }
        | PaintItem::Replaced { rect, .. }
        | PaintItem::Svg { rect, .. }
        | PaintItem::Clip { rect }
        | PaintItem::ClipRounded { rect, .. }
        | PaintItem::Border { rect, .. } => tr(rect, tx, ty),
        PaintItem::Image { rect, paint_rect, .. } => {
            tr(rect, tx, ty);
            tr(paint_rect, tx, ty);
        }
        PaintItem::Text { x, y, .. } => {
            *x += tx;
            *y += ty;
        }
        PaintItem::SetXf { .. } | PaintItem::SetXfCanvas | PaintItem::ClearXf | PaintItem::PopClip => {}
    }
}

/// One shifted subtree's item span: (source node, start, end) into a
/// paint run's items vec, recorded by the collect walk while both ends
/// are exact — sticky spans open before the node's own first item,
/// scroller spans after the node's clip (its own box stays fixed). See
/// [`apply_sticky_to_items`].
pub type StickySpan = (NodeId, usize, usize);

/// Stacking-context predicates for the cross-parent z walk: positioned
/// with an integer z-index (z:0 included), any transform, opacity<1,
/// backdrop-filter. The PAINT-order view; `collect` adds two walk-internal
/// barriers on top — clipping (a hoisted range would escape its Clip pair)
/// and sticky subtrees (their shift span must stay contiguous).
fn establishes_stacking_context(s: &ComputedStyle) -> bool {
    let positioned = matches!(
        s.position,
        Some(PositionMode::Relative)
            | Some(PositionMode::Absolute)
            | Some(PositionMode::Fixed)
            | Some(PositionMode::Sticky)
    );
    (positioned && s.z_index.is_some())
        || s.transform.is_some()
        || s.opacity.is_some_and(|o| o < 1.0)
        || s.backdrop_blur.is_some_and(|b| b > 0.0)
}

/// The collect half's full output: per-element border-box rects, the flat
/// paint-item list in paint order, the boxed-element paint ranking
/// (obscura #738), the event-coordinate local geometry map (blitz #663
/// family), the sticky subtree item spans (root-scroller v1), and the
/// scroller subtree item spans (sticky v2) — same span shape, recorded for
/// every real scroll container at layout time so the read-time shift walk
/// can translate a scrolled subtree without re-laying-out.
pub type LayoutCollect = (
    HashMap<NodeId, Rect>,
    Vec<PaintItem>,
    Vec<NodeId>,
    HashMap<NodeId, (Rect, [f32; 6])>,
    Vec<StickySpan>,
    Vec<StickySpan>,
);

/// Apply sticky shifts to a paint run: for every span whose value is
/// non-zero, translate that span's items by the shift in document space.
/// The input (cached) run is never mutated — callers get a shifted copy
/// only when some span has a shift.
///
/// Spans arrive PRE-RESOLVED as (start, end, value): `value` is the total
/// document-space translation the items inside the span must carry. The
/// caller pairs a layout-recorded span with its value — a sticky span's
/// is the node's read total, a scroller span's is that total minus the
/// scroller's own offset (sticky v2: the scroller's own box stays fixed,
/// so a node that is BOTH carries two different values over its two
/// nested spans — exactly why the value rides the span, not a node-keyed
/// map).
///
/// Nested spans: an ancestor span strictly contains a descendant's (the
/// collect walk records both in document order; a both-node's sticky span
/// opens before its clip, its scroller span after it), and values are
/// CUMULATIVE totals — so a nested span must translate by its DELTA over
/// the nearest enclosing live span; the ancestor's pass already moved
/// these items by its own total. Spans nest or are disjoint by tree
/// construction, so range containment alone identifies the enclosing span
/// — no DOM walk needed here. Liveness is a DELTA question, not a value
/// question: a pinned sticky inside a scrolled container reads total ZERO
/// (its own +shift cancels the scroller's base) yet must still move its
/// items by +shift over that base — dropping zero-valued spans was v1's
/// (root-only) shortcut, where value and delta were always equal. A
/// zero-DELTA span is a pure no-op (its total equals its enclosing base,
/// so skipping it leaves even the stack unchanged) and is skipped.
///
/// Bracket discipline: items inside a SetXf bracket are in local coords,
/// so translating them raw would double-map the shift (M·(p+t) ≠ M·p+t).
/// Instead the translation composes into the bracket matrix (T·M: e/f
/// shift, linear part untouched), moving the whole transformed subtree
/// uniformly. SetXfCanvas content is already canvas (document) space, so
/// its items translate directly — tracked with a small bracket stack of
/// (is_local) flags. A span can only contain brackets from its own
/// subtree: a source under any transformed ancestor is gated to zero
/// shift, so an ancestor's bracket never reaches into a live nested span.
pub fn apply_sticky_to_items(
    items: &[PaintItem],
    spans: &[(usize, usize, [f32; 2])],
) -> Vec<PaintItem> {
    let mut out = items.to_vec();
    let mut live: Vec<(usize, usize, [f32; 2])> = spans.to_vec();
    // Pre-order (start asc, end desc) so enclosing spans precede the spans
    // they contain; the open-span stack then answers "nearest enclosing".
    live.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut open: Vec<(usize, [f32; 2])> = Vec::new(); // (end, total shift)
    for &(start, end, sh) in &live {
        while open.last().is_some_and(|e| e.0 <= start) {
            open.pop();
        }
        let base = open.last().map(|&(_, b)| b).unwrap_or([0.0, 0.0]);
        let d = [sh[0] - base[0], sh[1] - base[1]];
        // Zero DELTA over the nearest enclosing span: a no-op for both the
        // items and the stack (the total equals the base it would push).
        if d[0] == 0.0 && d[1] == 0.0 {
            continue;
        }
        open.push((end, sh));
        let mut brackets: Vec<bool> = Vec::new(); // true = SetXf (local)
        for it in &mut out[start..end] {
            match it {
                PaintItem::SetXf { xf } => {
                    if !brackets.last().is_some_and(|&local| local) {
                        // Compose T·M — the shift is document-space, applied
                        // AFTER the bracket maps local content: e' = e + d0,
                        // f' = f + d1, linear part untouched. (M·T would
                        // rotate/scale the shift by the element's own map.)
                        xf[4] += d[0];
                        xf[5] += d[1];
                    }
                    brackets.push(true);
                }
                PaintItem::SetXfCanvas => brackets.push(false),
                PaintItem::ClearXf => {
                    brackets.pop();
                }
                _ => {
                    if !brackets.last().is_some_and(|&local| local) {
                        translate_item(it, d[0], d[1]);
                    }
                }
            }
        }
    }
    out
}

/// Re-run ONLY the collect half against a cached solve. Layout splits in
/// two halves for #395: the taffy solve (build, static-position harvest,
/// reparent, main solve, float continuation, calc repair, table placement)
/// is blind to paint-only properties — transform and opacity never reach
/// `to_taffy_style` — so this is the half a paint-only style write must
/// re-run, against the cached solve, instead of paying for a fresh taffy
/// pass (43 solves × 114ms on the probe page).
pub fn layout_collect(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    solved: &SolvedGeometry,
    viewport_width: f32,
) -> LayoutCollect {
    let mut rects = HashMap::new();
    let mut items: Vec<PaintItem> = Vec::new();
    // Paint sequence of the boxed elements (obscura #738): filled by the
    // `collect` walk, sibling bands already z-sorted. See the doc on
    // [`layout_dom_with_paint_order_and_images`].
    let mut paint_order: Vec<NodeId> = Vec::new();
    // Per-element LOCAL geometry for the event-coordinate surface (blitz
    // #663 family): the PRE-map border box plus the element's TOTAL
    // accumulated map (own linear part composed over ancestors — the same
    // map gBCR's `rects` entry was produced with). offsetX/Y inverse-maps
    // the hit point through it into the element's local space; hit testing
    // uses it to keep rotate/skew elements from swallowing corners of their
    // mapped bounding box. Keyed by DOM nid directly; the inline-band pass's
    // taffy-keyed `local_by_node` keeps its own shape untouched.
    let mut local_geom: HashMap<NodeId, (Rect, [f32; 6])> = HashMap::new();
    let mut sticky_spans: Vec<StickySpan> = Vec::new();
    // Scroller spans (sticky v2): one per real scroll container, recorded
    // unconditionally at layout time — scroll offsets are read-time paint
    // state that changes without a layout epoch bump, so the spans cannot
    // be gated on the current offset.
    let mut scroller_spans: Vec<StickySpan> = Vec::new();
    let Some(icb_node) = solved.icb_node else {
        return (rects, items, paint_order, local_geom, sticky_spans, scroller_spans);
    };
    let SolvedGeometry {
        taffy_tree,
        node_map,
        images,
        static_pos,
        baseline_shifts,
        run_wrappers,
        flattened,
        collapsed_edges,
        ..
    } = solved;

    // Accumulate locations down the taffy tree: child location already
    // includes the parent's border+padding offset, so a plain sum is the
    // absolute border-box origin (same accumulation blitz-paint performs).
    // The same pre-order walk emits paint items: an element's Bg when its
    // box is recorded, a run's Text at its leaf — document order, parents
    // before children.
    /// Resolved 2D affine threaded down the collect walk (animation batch B,
    /// widened in the affine batch): x' = a·x + c·y + e, y' = b·x + d·y + f,
    /// CSS matrix convention. Identity unless some ancestor declared a
    /// transform with a part beyond translate — pure translates fold into
    /// `offset` and never appear here, so untouched trees take the exact
    /// pre-batch path. Diagonal maps pre-bake into item fields at collect
    /// time; a non-diagonal map (rotate/skew) flips descendants into the
    /// SetXf/ClearXf local-coordinate paint path.
    #[derive(Clone, Copy)]
    struct Xf {
        a: f32,
        b: f32,
        c: f32,
        d: f32,
        e: f32,
        f: f32,
    }

    impl Xf {
        const IDENTITY: Xf = Xf { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

        /// Whether the linear part is diagonal (translate/scale lineage
        /// only) — the collect walk pre-bakes geometry on that path.
        fn is_diagonal(&self) -> bool {
            self.b == 0.0 && self.c == 0.0
        }

        /// Compose two maps, `m` outer and `n` inner (matrix product m·n).
        fn compose(m: Xf, n: Xf) -> Xf {
            Xf {
                a: m.a * n.a + m.c * n.b,
                b: m.b * n.a + m.d * n.b,
                c: m.a * n.c + m.c * n.d,
                d: m.b * n.c + m.d * n.d,
                e: m.a * n.e + m.c * n.f + m.e,
                f: m.b * n.e + m.d * n.f + m.f,
            }
        }

        /// Map a rect: the diagonal fast path keeps the historical two-corner
        /// normalize (negative scales mirror); anything else maps all four
        /// corners and unions them — Chrome's gBCR bounding-box behavior
        /// under rotation.
        fn map_rect(&self, r: Rect) -> Rect {
            map_rect_arr(self.to_array(), r)
        }

        fn to_array(self) -> [f32; 6] {
            [self.a, self.b, self.c, self.d, self.e, self.f]
        }
    }

    /// A positioned z>0 child deferred past a non-context parent: emitted
    /// at the nearest stacking-context ancestor instead (cross-parent z,
    /// the narrow slice). Carries the deferring frame's walk env — geometry
    /// is recursion-order-free, so the subtree paints pixel-identical at
    /// its z slot; only the sequence in the flat item list moves. Lives
    /// beside `Xf` because that map type is fn-scoped.
    struct ZEscape {
        z: i32,
        child: taffy::tree::NodeId,
        offset: (f32, f32),
        xf: Xf,
        alpha: f32,
        gradient: Option<(TextGradient, [f32; 6])>,
    }

    /// Multiply a straight-alpha color's alpha channel (animation batch A):
    /// the paint pipeline is straight-alpha source-over throughout, so
    /// folding element opacity into item colors composites the subtree.
    fn with_alpha(c: [u8; 4], a: f32) -> [u8; 4] {
        if a >= 1.0 {
            c
        } else {
            [c[0], c[1], c[2], (c[3] as f32 * a).round() as u8]
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect(
        tree: &DomTree,
        taffy_tree: &TaffyTree<TextLeaf>,
        node_map: &HashMap<taffy::tree::NodeId, NodeId>,
        styles: &HashMap<NodeId, ComputedStyle>,
        fonts: &FontBook,
        images: &HashMap<NodeId, DecodedImage>,
        static_pos: &HashMap<NodeId, (f32, f32)>,
        baseline_shifts: &HashMap<taffy::tree::NodeId, f32>,
        collapsed_edges: &HashMap<NodeId, [bool; 4]>,
        rects: &mut HashMap<NodeId, Rect>,
        local_geom: &mut HashMap<NodeId, (Rect, [f32; 6])>,
        abs_by_node: &mut HashMap<taffy::tree::NodeId, Rect>,
        local_by_node: &mut HashMap<taffy::tree::NodeId, (Rect, [f32; 6])>,
        node_first_item: &mut HashMap<taffy::tree::NodeId, usize>,
        sticky_spans: &mut Vec<StickySpan>,
        scroller_spans: &mut Vec<StickySpan>,
        items: &mut Vec<PaintItem>,
        paint_order: &mut Vec<NodeId>,
        escaped: &mut Vec<ZEscape>,
        node: taffy::tree::NodeId,
        offset: (f32, f32),
        viewport_width: f32,
        xf: Xf,
        alpha: f32,
        text_gradient: Option<&(TextGradient, [f32; 6])>,
    ) {
        let Ok(layout) = taffy_tree.layout(node) else { return };
        let item0 = items.len();
        // background-clip: text (gradient-text batch): inherited from the
        // nearest boxed clip:text ancestor, shadowed locally so a nested
        // clip:text element replaces it for its own subtree only — siblings
        // never see each other's gradient.
        let mut text_gradient = text_gradient.cloned();
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
        // transform (obscura #740 lineage, widened twice): the translate
        // part resolves against the element's own border box (percent
        // included) and folds into the offset — the element's rect, paint
        // items and whole subtree move together, layout never sees it
        // (Chrome: transforms don't affect layout). The linear part waits
        // for the box below: it pivots on the box center.
        let mut offset = offset;
        let mut t_lin = (1.0f32, 0.0f32, 0.0f32, 1.0f32);
        if let Some(dom_id) = node_map.get(&node) {
            if let Some(t) = styles.get(dom_id).and_then(|s| s.transform) {
                let resolve = |l: crate::diting_css::Length, basis: f32| match l {
                    crate::diting_css::Length::Px(v) => v,
                    crate::diting_css::Length::Percent(p) => p / 100.0 * basis,
                    // Percent-only approximation, same as to_taffy_style.
                    crate::diting_css::Length::Calc { percent, .. } => percent / 100.0 * basis,
                    crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
                };
                offset.0 += resolve(t.tx, layout.size.width);
                offset.1 += resolve(t.ty, layout.size.height);
                t_lin = (t.a, t.b, t.c, t.d);
            }
        }
        // opacity (animation batch A): non-inherited, multiplies down the
        // subtree — a faded parent fades everything inside it, matching
        // Chrome's group compositing for fades.
        let alpha = alpha
            * node_map
                .get(&node)
                .and_then(|d| styles.get(d))
                .and_then(|s| s.opacity)
                .unwrap_or(1.0);
        // Baseline alignment (blitz#750 family): run items drop onto their
        // line's baseline. Folded into the offset like the translate above,
        // so the item's rect, paint and whole subtree move together.
        if let Some(dy) = baseline_shifts.get(&node) {
            offset.1 += dy;
        }
        // Strut descent (replaced inline atoms): the taffy leaf is taller
        // than the element box by the strut's descent so the line box spans
        // it; every recorded box (rect/local/abs — gBCR, paint, hit test)
        // reports the element box.
        let strut_pad = match taffy_tree.get_node_context(node) {
            Some(TextLeaf::Replaced { strut_descent }) => *strut_descent,
            _ => 0.0,
        };
        let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
        // The map handed to descendants (animation batch B): upgraded below
        // once this element's own box is final, since the scale part pivots
        // on the box center.
        let mut child_xf = xf;
        // Every visited node's absolute border box — the union pass after
        // the walk rebuilds rects for flattened inline wrappers from kids.
        // The local-space twin (affine residuals batch ②) records the
        // PRE-map box plus the incoming map: the inline-band pass splits
        // text runs into line bands where the wrap width still means what
        // the paint item's `wrap_at` means, then maps each band.
        local_by_node.insert(
            node,
            (
                Rect {
                    x: abs.0,
                    y: abs.1,
                    width: layout.size.width,
                    height: layout.size.height - strut_pad,
                },
                xf.to_array(),
            ),
        );
        abs_by_node.insert(
            node,
            xf.map_rect(Rect {
                x: abs.0,
                y: abs.1,
                width: layout.size.width,
                height: layout.size.height - strut_pad,
            }),
        );
        let mut clips = false;
        let mut xf_bracket = false;
        // Sticky v2: while `clips` is true and the used overflow is
        // scrollable (anything but `clip` — `hidden` IS a scroll container
        // per css-overflow), this element is a scroll container. Its subtree
        // items form a span recorded AFTER the Clip push (the clip rect
        // itself must not translate when the subtree is scrolled) and closed
        // BEFORE the PopClip push.
        let mut scroll_span: Option<(NodeId, usize)> = None;
        if let Some(dom_id) = node_map.get(&node) {
            let mut rect = Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height - strut_pad };
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
            // The transform's linear part pivots on the element's own
            // (already translate-folded) box center: pivoting the
            // accumulated function list there equals CSS transform-origin
            // composition (the translate rode the same absolute axes the
            // offset fold used, so both list orders come out exact).
            // Ancestors wrap outside via compose. Diagonal linear parts
            // keep pre-baking geometry into item fields (the historical
            // path); a rotate/skew flips this element's whole item range
            // into local coordinates bracketed by SetXf/ClearXf, and paint
            // inverse-maps per pixel.
            let prebake = if t_lin != (1.0f32, 0.0, 0.0, 1.0) {
                let (rcx, rcy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
                let own = Xf {
                    a: t_lin.0,
                    b: t_lin.1,
                    c: t_lin.2,
                    d: t_lin.3,
                    e: rcx - (t_lin.0 * rcx + t_lin.2 * rcy),
                    f: rcy - (t_lin.1 * rcx + t_lin.3 * rcy),
                };
                child_xf = Xf::compose(xf, own);
                if !xf.is_diagonal() {
                    // Inside an open bracket the canvas top already equals
                    // xf, so only the own map composes on top of it.
                    items.push(PaintItem::SetXf { xf: own.to_array() });
                    xf_bracket = true;
                    false
                } else if !child_xf.is_diagonal() {
                    // First non-diagonal map on a prebaked chain: the canvas
                    // top is identity here, so the bracket carries the FULL
                    // accumulated map.
                    items.push(PaintItem::SetXf { xf: child_xf.to_array() });
                    xf_bracket = true;
                    false
                } else {
                    true
                }
            } else {
                xf.is_diagonal()
            };
            // gBCR/hit-testing rect: the TRANSFORMED box (Chrome reports the
            // mapped bounds — a 4-corner bounding box under rotation;
            // translate is already in `rect` via the offset fold).
            let mrect = child_xf.map_rect(rect);
            rects.insert(*dom_id, mrect);
            // The un-mapped twin plus the total map (event-coordinate
            // surface, blitz #663 family): offsetX/Y inverse-maps the hit
            // point through the map and subtracts this box's padding edge.
            local_geom.insert(*dom_id, (rect, child_xf.to_array()));
            // Items under a SetXf bracket paint in LOCAL coordinates (raw
            // `rect`); the prebaked path uses the mapped box.
            let bg_rect = if prebake { mrect } else { rect };
            // CSS2.1 paints cell > row > row-group backgrounds. Row groups
            // have their own wrapper boxes since the sticky-group batch, so
            // the group's bg paints on its own band (under the rows, which
            // are its taffy children) — the old paint-time DOM climb that
            // faked a group band onto the row is gone with the boxless
            // group. (If the group wrapper fails to build, the group's bg
            // is dropped — error fallback only.)
            let bg = styles.get(dom_id).and_then(|s| s.background_color);
            // Radii resolve per-axis against the LOCAL box (rx width, ry
            // height — the elliptical form), then ride the affine. Hoisted
            // above the fills so the color and gradient layers share them.
            let res = |l: &crate::diting_css::Length, basis: f32| match l {
                crate::diting_css::Length::Px(v) => *v,
                crate::diting_css::Length::Percent(p) => p * basis / 100.0,
                crate::diting_css::Length::Calc { percent, .. } => percent * basis / 100.0,
                crate::diting_css::Length::Auto | crate::diting_css::Length::MinContent | crate::diting_css::Length::MaxContent | crate::diting_css::Length::FitContent => 0.0,
            };
            let radii: [(f32, f32); 4] = styles
                .get(dom_id)
                .and_then(|s| s.corner_radii.as_ref())
                .map(|corners| {
                    let rs = std::array::from_fn(|i| {
                        // Radii ride the affine only on the prebaked path
                        // (each axis scales by its diagonal entry); under a
                        // SetXf bracket they stay local and the affine
                        // rasterizer curves them.
                        if prebake {
                            (res(&corners[i].0, rect.width) * child_xf.a, res(&corners[i].1, rect.height) * child_xf.d)
                        } else {
                            (res(&corners[i].0, rect.width), res(&corners[i].1, rect.height))
                        }
                    });
                    clamp_corner_radii(rs, bg_rect.width, bg_rect.height)
                })
                .unwrap_or([(0.0, 0.0); 4]);
            if alpha > 0.0 && rect.width > 0.0 && rect.height > 0.0 {
                // backdrop-filter filters everything beneath this element,
                // so it must emit BEFORE any of the element's own ink
                // (shadows, background, border) — first item in the block.
                if let Some(blur) = styles
                    .get(dom_id)
                    .and_then(|s| s.backdrop_blur)
                    .filter(|b| *b > 0.0)
                {
                    items.push(PaintItem::BackdropFilter {
                        rect: bg_rect,
                        radii,
                        blur,
                    });
                }
                // box-shadow below everything: CSS stacks shadows under the
                // background, first-declared layer on top, so emit reversed.
                if let Some(shadows) = styles.get(dom_id).and_then(|s| s.box_shadow.as_ref()) {
                    for sh in shadows.iter().rev().filter(|sh| !sh.inset) {
                        items.push(PaintItem::BoxShadow {
                            rect: bg_rect,
                            color: with_alpha([sh.color.0, sh.color.1, sh.color.2, sh.color.3], alpha),
                            radii,
                            dx: sh.dx,
                            dy: sh.dy,
                            blur: sh.blur,
                            spread: sh.spread,
                            inset: false,
                        });
                    }
                }
                // background-color: the bottom CSS layer.
                if let Some(c) = bg.filter(|c| c.3 != 0) {
                    let color = with_alpha([c.0, c.1, c.2, c.3], alpha);
                    let uniform = radii.iter().all(|r| *r == radii[0]);
                    if uniform {
                        items.push(PaintItem::Bg {
                            rect: bg_rect,
                            color,
                            radius: radii[0].0,
                        });
                    } else {
                        items.push(PaintItem::BgCorner {
                            rect: bg_rect,
                            color,
                            radii,
                        });
                    }
                }
                // background-image above it: v1 takes a parseable
                // linear-gradient for the whole box (url() images stay
                // unpainted — they are the Image item's job, not a fill).
                if let Some(g) = styles
                    .get(dom_id)
                    .and_then(|s| s.background_image.as_deref())
                    .and_then(crate::diting_css::parse_linear_gradient)
                {
                    if styles.get(dom_id).is_some_and(|s| s.background_clip_text) {
                        // background-clip: text: the gradient never fills
                        // the box (that was the opaque-block-covering-the-
                        // text bug) — it becomes the glyph fill for this
                        // subtree, threaded down like alpha. Stops stay raw;
                        // each text folds its own inherited alpha at attach.
                        // The captured map guards the space: a text leaf
                        // only attaches while its accumulated xf still
                        // equals this one, so an intervening transform
                        // degrades those glyphs to solid color instead of
                        // sampling a mismatched space.
                        text_gradient = Some((
                            TextGradient {
                                area: bg_rect,
                                stops: g.stops.iter().map(|(p, c)| (*p, [c.0, c.1, c.2, c.3])).collect(),
                                css_deg: g.css_deg,
                            },
                            child_xf.to_array(),
                        ));
                    } else {
                        let stops = g
                            .stops
                            .iter()
                            .map(|(p, c)| (*p, with_alpha([c.0, c.1, c.2, c.3], alpha)))
                            .collect();
                        items.push(PaintItem::BgGradient { rect: bg_rect, stops, css_deg: g.css_deg, radii });
                    }
                }
                // inset layers above the background (and gradient) but
                // under the border, same reversed first-on-top order —
                // Chrome's inner-shadow phase sits between the two.
                if let Some(shadows) = styles.get(dom_id).and_then(|s| s.box_shadow.as_ref()) {
                    for sh in shadows.iter().rev().filter(|sh| sh.inset) {
                        items.push(PaintItem::BoxShadow {
                            rect: bg_rect,
                            color: with_alpha([sh.color.0, sh.color.1, sh.color.2, sh.color.3], alpha),
                            radii,
                            dx: sh.dx,
                            dy: sh.dy,
                            blur: sh.blur,
                            spread: sh.spread,
                            inset: true,
                        });
                    }
                }
            }
            // A border exists only with a line style; its color defaults to
            // currentColor = the element's own computed (inherited) color.
            let st = styles.get(dom_id);
            if let Some(style) = st.filter(|s| s.border_style.is_some()) {
                // Top/bottom are horizontal strips (vertical thickness rides
                // d), left/right vertical strips (a) — the affine maps each
                // edge's thickness with its own axis; a bracket keeps the
                // widths local.
                let (kx, ky) = if prebake { (child_xf.a, child_xf.d) } else { (1.0, 1.0) };
                // Collapsed table cell borders (spans batch): an edge on a
                // shared grid line paints at HALF width so two adjacent 2px
                // borders read as one 2px line centered on the line —
                // Chrome's resolved-border rendering for equal widths.
                // Index-based marks from build_table (top/right/bottom/left).
                let halve = collapsed_edges.get(dom_id);
                let mut widths = [
                    side_px(style.border_width.top) * ky,
                    side_px(style.border_width.right) * kx,
                    side_px(style.border_width.bottom) * ky,
                    side_px(style.border_width.left) * kx,
                ];
                if let Some(h) = halve {
                    for (i, w) in widths.iter_mut().enumerate() {
                        if h[i] {
                            *w /= 2.0;
                        }
                    }
                }
                if widths.iter().any(|w| *w > 0.0) {
                    let color = style
                        .border_color
                        .or(style.color)
                        .map(|c| with_alpha([c.0, c.1, c.2, c.3], alpha))
                        .unwrap_or([0, 0, 0, 255]);
                    items.push(PaintItem::Border { rect: bg_rect, widths, color, radii });
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
                // svg: the subtree compiles to paint ops here — never the
                // image path, never the placeholder (an empty svg still
                // renders its element box, matching Chrome).
                let is_svg = tree
                    .with_node(*dom_id, |n| {
                        n.as_element().map(|e| e.local.to_string() == "svg")
                    })
                    .flatten()
                    .unwrap_or(false);
                if is_svg {
                    let render = std::sync::Arc::new(svg::compile_svg(tree, styles, *dom_id));
                    items.push(PaintItem::Svg { rect: bg_rect, render, alpha });
                } else if let Some(img) = images.get(dom_id) {
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
                    // want it — mapped through the same affine as mrect on
                    // the prebaked path, raw under a bracket.
                    let op_rect = object_paint_rect(rect, img.width as f32, img.height as f32, fit, pos);
                    let paint_rect = if prebake { child_xf.map_rect(op_rect) } else { op_rect };
                    items.push(PaintItem::Image { rect: bg_rect, paint_rect, image: img.clone(), alpha });
                } else {
                    // Alt text is an <img> concept only (batch 7a): video/
                    // iframe/canvas placeholders are the bare box. Form
                    // controls (input/textarea) paint the text they show —
                    // the dirty value, parsed default, or gray placeholder.
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
                                let (font_size, bold, lh) = font_context(tree, *dom_id, styles, fonts);
                                (text, font_size, bold, lh, color_context(tree, *dom_id, styles))
                            })
                    } else {
                        form_control_run(tree, *dom_id, styles, fonts)
                    };
                    // The gray box only when the author gave no visible
                    // background — an authored bg already reads as "box here".
                    let fill_placeholder = styles
                        .get(dom_id)
                        .and_then(|s| s.background_color)
                        .is_none_or(|c| c.3 == 0);
                    // Checkable inputs resolve their widget here (dirty
                    // mirror beats the parsed attribute — the JS getter's
                    // precedence) so the paint executor stays tree-free.
                    let widget = tree
                        .with_node(*dom_id, |n| {
                            let is_input =
                                n.as_element().map(|e| e.local.as_ref() == "input").unwrap_or(false);
                            if !is_input {
                                return None;
                            }
                            let ty = n
                                .get_attribute("type")
                                .map(|v| v.to_ascii_lowercase())
                                .unwrap_or_default();
                            let checked = n
                                .live_checked()
                                .unwrap_or_else(|| n.get_attribute("checked").is_some());
                            match ty.as_str() {
                                "checkbox" => Some(FormWidget::Checkbox { checked }),
                                "radio" => Some(FormWidget::Radio { checked }),
                                "range" => {
                                    // Fraction of the inset track the thumb
                                    // sits at: live value (dirty mirror of a
                                    // click) over the value attribute, over
                                    // the spec default (min+max)/2. min/max
                                    // fall back to 0/100; max<=min pins the
                                    // thumb at the start (the spec clamps
                                    // value to min).
                                    let num = |name: &str, def: f32| {
                                        n.get_attribute(name)
                                            .and_then(|v| v.parse::<f32>().ok())
                                            .unwrap_or(def)
                                    };
                                    let (min, max) = (num("min", 0.0), num("max", 100.0));
                                    let value = n
                                        .live_value()
                                        .or_else(|| n.get_attribute("value"))
                                        .and_then(|v| v.parse::<f32>().ok())
                                        .unwrap_or((min + max) / 2.0);
                                    let fraction = if max > min {
                                        ((value - min) / (max - min)).clamp(0.0, 1.0)
                                    } else {
                                        0.0
                                    };
                                    Some(FormWidget::Range { fraction })
                                }
                                _ => None,
                            }
                        })
                        .flatten();
                    // The control's run layout kind: same tag/type split the
                    // widget arm uses, but for the text-carrying controls —
                    // checkables take the widget path instead.
                    let form = if widget.is_none() {
                        tree.with_node(*dom_id, |n| {
                            let local = n
                                .as_element()
                                .map(|e| e.local.to_string())
                                .unwrap_or_default();
                            match local.as_str() {
                                "textarea" => Some(FormRun::Textarea),
                                "select" => Some(FormRun::Select),
                                "input" => {
                                    let ty = n
                                        .get_attribute("type")
                                        .map(|v| v.to_ascii_lowercase())
                                        .unwrap_or_default();
                                    match ty.as_str() {
                                        "button" | "submit" | "reset" => Some(FormRun::Button),
                                        // checkbox/radio never reach here
                                        // (widget took them); every other type
                                        // is a text-like single-line control.
                                        _ => Some(FormRun::Input),
                                    }
                                }
                                _ => None,
                            }
                        })
                        .flatten()
                    } else {
                        None
                    };
                    // Caret: the focused text-entry control's typing
                    // cursor, at the recorded selection's anchor. Resolved
                    // here — collect time — because paint keeps no tree
                    // access; same posture as checkedness and the run kind
                    // above. min(start,end): a non-collapsed range paints
                    // the anchor side (Chrome paints the focus node; the
                    // direction mirror is not recorded, so the anchor is
                    // the deterministic stand-in).
                    let caret = if matches!(form, Some(FormRun::Input) | Some(FormRun::Textarea))
                        && tree.focused_node() == Some(*dom_id)
                        && tree.selection().is_some_and(|(nid, _, _)| nid == *dom_id)
                    {
                        tree.selection().map(|(_, start, end)| {
                            let live = tree
                                .with_node(*dom_id, |n| n.live_value().map(|v| v.to_string()))
                                .flatten();
                            // The value the JS getter would report: dirty
                            // mirror first, then the parsed default
                            // (attribute for input, child text for
                            // textarea).
                            let value = if matches!(form, Some(FormRun::Textarea)) {
                                live.unwrap_or_else(|| tree.text_content(*dom_id))
                            } else {
                                live.or_else(|| {
                                    tree.with_node(*dom_id, |n| {
                                        n.get_attribute("value").map(|v| v.to_string())
                                    })
                                    .flatten()
                                })
                                .unwrap_or_default()
                            };
                            let ink = color_context(tree, *dom_id, styles);
                            (start.min(end).min(value.chars().count()), ink)
                        })
                    } else {
                        None
                    };
                    // An empty value with no placeholder leaves the run
                    // None — but a caret still needs the control's font
                    // metrics to stand in a line box, so synthesize an
                    // empty run for it (the rasterizer blits nothing for
                    // empty text; only the caret consumes the metrics).
                    let alt = if alt.is_none() && caret.is_some() {
                        let (font_size, bold, lh) = font_context(tree, *dom_id, styles, fonts);
                        Some((
                            String::new(),
                            font_size,
                            bold,
                            lh,
                            caret.map(|(_, ink)| ink).unwrap_or_default(),
                        ))
                    } else {
                        alt
                    };
                    items.push(PaintItem::Replaced {
                        rect: bg_rect,
                        alt,
                        fill_placeholder,
                        alpha,
                        widget,
                        form,
                        caret,
                    });
                }
            }
            // A clipping element constrains its DESCENDANTS' paint (its own
            // bg/border above are not clipped) to the padding box — the
            // border box inset by the border widths. Text runs are taffy
            // children here, so they land inside the clip pair too.
            let style = styles.get(dom_id);
            clips = style.is_some_and(|s| s.clips_descendants());
            if clips {
                // blitz#880 (css-overflow-3 §3.3): html's overflow belongs to
                // the viewport — the canvas bounds are the clip, never an
                // html clip push. A body whose overflow propagated to the
                // viewport has its used value flipped back to `visible` and
                // must not clip either.
                let tag = tree.get_node(*dom_id).and_then(|n| {
                    n.as_element()
                        .map(|e| e.local.to_ascii_lowercase().as_ref().to_string())
                });
                if tag.as_deref() == Some("html")
                    || (tag.as_deref() == Some("body")
                        && body_overflow_propagates(tree, styles, *dom_id, |id| {
                            rects.contains_key(&id)
                        }))
                {
                    clips = false;
                }
            }
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
                    })
                    .map(|rs| clamp_corner_radii(rs, pad_w, pad_h))
                    .map(|rs| {
                        // Descendants paint through child_xf; on the prebaked
                        // path the clip (and its radii) rides the same
                        // affine, under a bracket it stays local.
                        if prebake {
                            rs.map(|(rx, ry)| (rx * child_xf.a, ry * child_xf.d))
                        } else {
                            rs
                        }
                    });
                // The clip lives in child space (it bounds descendants) —
                // mapped through child_xf on the prebaked path, raw local
                // geometry under a bracket. The element's own bg/border
                // above are already painted and stay unclipped either way.
                let pad_local = Rect { x: rect.x + bl, y: rect.y + bt, width: pad_w, height: pad_h };
                let pad_rect = if prebake { child_xf.map_rect(pad_local) } else { pad_local };
                let clip_item = match radii {
                    Some(radii) if radii.iter().any(|r| r.0 > 0.0 && r.1 > 0.0) => {
                        PaintItem::ClipRounded {
                            rect: pad_rect,
                            radii,
                        }
                    }
                    _ => PaintItem::Clip {
                        rect: pad_rect,
                    },
                };
                items.push(clip_item);
                // Sticky v2: a scroll container's span opens after its clip
                // — the clip rect (and the element's own bg/border above)
                // stay fixed while the scrolled subtree translates under
                // them. html/body never get here (viewport owns their
                // overflow, `clips` was flipped back to false above).
                if st.effective_overflow() != Overflow::Clip {
                    scroll_span = Some((*dom_id, items.len()));
                }
            }
        }
        // background-clip: text attach: the inherited fill lands only while
        // this leaf still paints in the space the gradient was captured in
        // (an intervening transform opened a different one — degrade to the
        // solid run color), with this leaf's inherited opacity folded into
        // the stops.
        let text_gradient_fill = text_gradient
            .as_ref()
            .filter(|(_, at)| *at == xf.to_array())
            .map(|(g, _)| TextGradient {
                area: g.area,
                stops: g.stops.iter().map(|(p, c)| (*p, with_alpha(*c, alpha))).collect(),
                css_deg: g.css_deg,
            });
        // Inherited text-shadow layers with this element's opacity folded
        // into each color (the item colors carry alpha, shadows ride along).
        // Text leaves sit outside `node_map` — walk up to the nearest boxed
        // ancestor, whose style already carries the inherited layer list.
        let mut shadow_dom = node_map.get(&node).copied();
        let mut shadow_up = taffy_tree.parent(node);
        while shadow_dom.is_none() {
            match shadow_up {
                Some(p) => {
                    shadow_dom = node_map.get(&p).copied();
                    shadow_up = taffy_tree.parent(p);
                }
                None => break,
            }
        }
        let run_text_shadow = shadow_dom
            .and_then(|dom_id| styles.get(&dom_id))
            .and_then(|s| s.text_shadow.clone())
            .map(|layers| {
                layers
                    .into_iter()
                    .map(|mut sh| {
                        let c = with_alpha([sh.color.0, sh.color.1, sh.color.2, sh.color.3], alpha);
                        sh.color = crate::diting_css::Color(c[0], c[1], c[2], c[3]);
                        sh
                    })
                    .collect()
            });
        if let Some(TextLeaf::Run { text, font_size, bold, color, line_height, decorations, mono, word_spacing, ws, ellipsis, tokens, small_caps, .. }) = taffy_tree.get_node_context(node) {
            // The wrap width the containing block offered at measure time:
            // the direct taffy parent's content box (the run wrapper for
            // mixed runs, the block itself for pure runs — same width).
            // Scaled geometry (animation batch B): position maps through
            // the affine, font metrics ride d, the wrap width a — exact
            // for uniform scales, the documented approximation otherwise.
            // Under a non-diagonal map (affine batch) the leaf emits RAW
            // local geometry and the canvas-side SetXf bracket maps the
            // rasterized tile per pixel.
            if xf.is_diagonal() {
                let wrap_at = taffy_tree
                    .parent(node)
                    .and_then(|p| taffy_tree.layout(p).ok())
                    .map(|l| l.content_box_width())
                    .unwrap_or(viewport_width)
                    * xf.a;
                // no-soft-wrap modes measure/paint one line: wrap at +inf (a
                // distinct RasterKey), ellipsis truncates at the real box
                // width.
                let truncate_at = if *ellipsis && ws.no_soft_wrap() { Some(wrap_at) } else { None };
                let wrap_at = if ws.no_soft_wrap() { f32::INFINITY } else { wrap_at };
                items.push(PaintItem::Text {
                    text: text.clone(),
                    font_size: font_size * xf.d,
                    bold: *bold,
                    color: with_alpha(*color, alpha),
                    line_height: line_height * xf.d,
                    x: abs.0 * xf.a + xf.e,
                    y: abs.1 * xf.d + xf.f,
                    wrap_at,
                    gradient: text_gradient_fill.clone(),
                    decorations: *decorations,
                    mono: *mono,
                    word_spacing: word_spacing * xf.d,
                    truncate_at,
                    // Paint half of obscura#983: hand the leaf's measure-time
                    // wrap tokens to the paint item so repaints stop re-shaping.
                    // Only valid unscaled — a diagonal scale folds d into
                    // font_size/word_spacing and the memo no longer matches.
                    tokens: if xf.d == 1.0 {
                        Some(run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps))
                    } else {
                        None
                    },
                    ws: *ws,
                    text_shadow: run_text_shadow.clone(),
                    small_caps: *small_caps,
                });
            } else {
                let wrap_at = taffy_tree
                    .parent(node)
                    .and_then(|p| taffy_tree.layout(p).ok())
                    .map(|l| l.content_box_width())
                    .unwrap_or(viewport_width);
                let truncate_at = if *ellipsis && ws.no_soft_wrap() { Some(wrap_at) } else { None };
                let wrap_at = if ws.no_soft_wrap() { f32::INFINITY } else { wrap_at };
                items.push(PaintItem::Text {
                    text: text.clone(),
                    font_size: *font_size,
                    bold: *bold,
                    color: with_alpha(*color, alpha),
                    line_height: *line_height,
                    x: abs.0,
                    y: abs.1,
                    wrap_at,
                    gradient: text_gradient_fill.clone(),
                    decorations: *decorations,
                    mono: *mono,
                    word_spacing: *word_spacing,
                    truncate_at,
                    tokens: Some(run_tokens(text, *font_size, *bold, fonts, *mono, *word_spacing, *ws, tokens, *small_caps)),
                    ws: *ws,
                    text_shadow: run_text_shadow.clone(),
                    small_caps: *small_caps,
                });
            }
        }
        if let Some(TextLeaf::Word { text, font_size, bold, color, line_height, decorations, mono, .. }) = taffy_tree.get_node_context(node) {
            // A word leaf paints at its own box — the enclosing flex row
            // already did the line breaking (leaf-level wrap). Single-token
            // text can never break, so wrap_at just equals the leaf width.
            // Same diagonal/non-diagonal split as the Run leaf above.
            if xf.is_diagonal() {
                items.push(PaintItem::Text {
                    text: text.clone(),
                    font_size: font_size * xf.d,
                    bold: *bold,
                    color: with_alpha(*color, alpha),
                    line_height: line_height * xf.d,
                    x: abs.0 * xf.a + xf.e,
                    y: abs.1 * xf.d + xf.f,
                    wrap_at: layout.size.width * xf.a,
                    gradient: text_gradient_fill.clone(),
                    decorations: *decorations,
                    mono: *mono,
                    word_spacing: 0.0,
                    truncate_at: None,
                    tokens: None,
                    ws: WhiteSpace::Normal,
                    text_shadow: run_text_shadow.clone(),
                    // Word leaves are pre-uppercased at build time.
                    small_caps: false,
                });
            } else {
                items.push(PaintItem::Text {
                    text: text.clone(),
                    font_size: *font_size,
                    bold: *bold,
                    color: with_alpha(*color, alpha),
                    line_height: *line_height,
                    x: abs.0,
                    y: abs.1,
                    wrap_at: layout.size.width,
                    gradient: text_gradient_fill.clone(),
                    decorations: *decorations,
                    mono: *mono,
                    word_spacing: 0.0,
                    truncate_at: None,
                    tokens: None,
                    ws: WhiteSpace::Normal,
                    text_shadow: run_text_shadow.clone(),
                    // Word leaves are pre-uppercased at build time.
                    small_caps: false,
                });
            }
        }
        // Inline background bands (blitz#340 family) splice at the first
        // paint item of the band owner's content — record this node's first
        // item index while the walk is here. +1 past a SetXf pushed at
        // item0, so splices land inside the bracket (and are skipped there
        // — see the splice site).
        let rec = item0 + usize::from(xf_bracket);
        if items.len() > rec {
            node_first_item.entry(node).or_insert(rec);
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
        // Cross-parent z (the narrow slice): a positioned z>0 child of a
        // NON-stacking-context parent must hoist past it — in Chrome,
        // A{relative}>B{absolute z:5} out-paints A's later sibling
        // C{relative z:1}. This frame is a BARRIER when it consumes such
        // children locally: a stacking context, a clipper (a hoisted range
        // would tear its Clip/PopClip pair), or a sticky subtree (its
        // shift span must stay contiguous). Every bracket producer implies
        // a barrier condition — xf_bracket needs a transform, clips needs
        // clips_descendants — so a hoisted subtree can never land inside
        // someone else's brackets. Negative z stays per-parent: the linear
        // walk cannot emit before this node's own background, and a neg-z
        // child already paints above it today (pre-existing inaccuracy,
        // out of this slice). The synthetic ICB is not in node_map →
        // always a barrier.
        // Row-group wrappers hoisted into a z band by their member rows
        // (look-through below) carry positioned subtrees without being
        // stacking contexts themselves — they must stop escapes at the
        // band slot, or their recursion would append back into the
        // consuming barrier's `below` after it drained.
        let self_is_group = node_map
            .get(&node)
            .and_then(|d| {
                tree.with_node(*d, |n| {
                    n.as_element()
                        .map(|e| {
                            matches!(
                                e.local.to_ascii_lowercase().as_ref(),
                                "thead" | "tbody" | "tfoot"
                            )
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        let barrier = self_is_group
            || match node_map.get(&node).and_then(|d| styles.get(d)) {
                None => true,
                Some(s) => {
                    establishes_stacking_context(s)
                        || s.clips_descendants()
                        || s.position == Some(PositionMode::Sticky)
                }
            };
        let mut neg: Vec<(i32, usize)> = Vec::new();
        let mut mid: Vec<(i32, usize)> = Vec::new();
        let mut pos: Vec<(i32, usize)> = Vec::new();
        // z>0 children this non-barrier frame defers up to the barrier.
        let mut pending: Vec<ZEscape> = Vec::new();
        for (i, &child) in children.iter().enumerate() {
            let child_style = node_map.get(&child).and_then(|d| styles.get(d));
            // Sticky counts as positioned here (CSS App. E step 8): a stuck
            // overlay must out-paint — and so out-rank in elementFromPoint,
            // which sorts by this order — any later static sibling whose tall
            // box covers the stuck band (uv-docs' .md-header case, #434).
            let mut positioned = child_style.is_some_and(|s| {
                matches!(
                    s.position,
                    Some(PositionMode::Relative)
                        | Some(PositionMode::Absolute)
                        | Some(PositionMode::Fixed)
                        | Some(PositionMode::Sticky)
                )
            });
            let mut member_z_override: Option<i32> = None;
            // Row-group wrappers (thead/tbody/tfoot stand-ins) establish no
            // box in CSS, so a sticky ROW inside one must still join the
            // table's positioned band — confining it to the group frame lets
            // a later tbody's tall static box cover the stuck header. The
            // group's own sticky style already classifies it directly; this
            // look-through only fires for a static group with positioned
            // member rows (z≠0 members stay per-parent, pre-existing).
            if !positioned {
                let is_group = node_map
                    .get(&child)
                    .and_then(|d| {
                        tree.with_node(*d, |n| {
                            n.as_element()
                                .map(|e| {
                                    matches!(
                                        e.local.to_ascii_lowercase().as_ref(),
                                        "thead" | "tbody" | "tfoot"
                                    )
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if is_group {
                    let mut any_pos = false;
                    let mut member_max_z: i32 = 0;
                    let mut member_min_z: i32 = 0;
                    for m in taffy_tree.children(child).unwrap_or_default() {
                        if let Some(s) = node_map.get(&m).and_then(|d| styles.get(d)) {
                            if matches!(
                                s.position,
                                Some(PositionMode::Relative)
                                    | Some(PositionMode::Absolute)
                                    | Some(PositionMode::Fixed)
                                    | Some(PositionMode::Sticky)
                            ) {
                                any_pos = true;
                                let mz = s.z_index.unwrap_or(0);
                                member_max_z = member_max_z.max(mz);
                                member_min_z = member_min_z.min(mz);
                            }
                        }
                    }
                    if any_pos {
                        positioned = true;
                        // A member carrying a nonzero z drags the whole
                        // group into its hoisted band (a sticky row with
                        // z-index must still out-rank a positioned z:2
                        // sibling of the table); signless members keep the
                        // plain positioned band below.
                        if member_max_z > 0 {
                            member_z_override = Some(member_max_z);
                        } else if member_min_z < 0 {
                            member_z_override = Some(member_min_z);
                        }
                    }
                }
            }
            let floated = child_style.is_some_and(|s| s.float_side.is_some());
            let z = member_z_override
                .unwrap_or_else(|| child_style.and_then(|s| s.z_index).unwrap_or(0));
            if z != 0 && positioned {
                // Hoisted band: painted before (z<0) / after (z>0) the
                // middle band, ascending within the band.
                if z < 0 {
                    neg.push((z, i));
                } else if barrier {
                    pos.push((z, i));
                } else {
                    // Defer past this non-context parent, carrying the walk
                    // env the direct call would have received — geometry is
                    // recursion-order-free, so the subtree paints identical
                    // wherever its slot lands; the barrier emits it in ITS
                    // positive band below.
                    pending.push(ZEscape {
                        z,
                        child,
                        offset: abs,
                        xf: child_xf,
                        alpha,
                        gradient: text_gradient.clone(),
                    });
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
        // Emission, neg → mid → pos as before. `below` catches escapes
        // bubbled up from non-context mid children — neg/pos children carry
        // z≠0, are themselves contexts, and consume locally, so only the
        // mid band can hand escapes up.
        let mut below: Vec<ZEscape> = Vec::new();
        macro_rules! emit {
            ($n:expr, $o:expr, $x:expr, $a:expr, $g:expr) => {
                collect(
                    tree, taffy_tree, node_map, styles, fonts, images, static_pos,
                    baseline_shifts, collapsed_edges, rects, local_geom, abs_by_node,
                    local_by_node, node_first_item, sticky_spans, scroller_spans, items,
                    paint_order, &mut below, $n, $o, viewport_width, $x, $a, $g,
                )
            };
        }
        for (_, i) in neg {
            emit!(children[i], abs, child_xf, alpha, text_gradient.as_ref());
        }
        for (_, i) in mid {
            emit!(children[i], abs, child_xf, alpha, text_gradient.as_ref());
        }
        // Positive band: own z>0 children merged with the bubbled escapes,
        // stable-sorted by z — own children precede escapes on equal z
        // (direct children ahead of hoisted descendants). ONLY a barrier
        // may consume escapes here; a non-barrier frame hands `below` up
        // untouched with its own pending, or a nephew would settle at this
        // frame's level instead of the real stacking context.
        enum PosSlot {
            Own(usize),
            Up(ZEscape),
        }
        let mut seq: Vec<(i32, PosSlot)> =
            pos.into_iter().map(|(z, i)| (z, PosSlot::Own(i))).collect();
        if barrier {
            seq.extend(below.drain(..).map(|e| (e.z, PosSlot::Up(e))));
        }
        seq.sort_by_key(|(z, _)| *z);
        for (_, slot) in seq {
            match slot {
                PosSlot::Own(i) => {
                    emit!(children[i], abs, child_xf, alpha, text_gradient.as_ref())
                }
                PosSlot::Up(e) => emit!(e.child, e.offset, e.xf, e.alpha, e.gradient.as_ref()),
            }
        }
        if barrier {
            // Pos-band emission targets are all contexts, so `below` is
            // empty again here — asserted rather than assumed.
            debug_assert!(below.is_empty());
        } else {
            // Hand the deferred band to the parent frame's walk: own
            // pending children first, then bubbled nephews, matching the
            // own-before-escapes tie rule above.
            escaped.append(&mut pending);
            escaped.append(&mut below);
        }
        // Sticky v2: the scroller's span closes before the PopClip — the
        // closing bracket itself must not translate. Same span shape as the
        // sticky spans above, consumed by the same apply_sticky_to_items.
        if let Some((sid, s0)) = scroll_span {
            scroller_spans.push((sid, s0, items.len()));
        }
        if clips {
            items.push(PaintItem::PopClip);
        }
        if xf_bracket {
            items.push(PaintItem::ClearXf);
        }
        // Sticky spans (root-scroller v1): a sticky subtree's items are a
        // contiguous run — the walk emits the node's own band, then its
        // children (z-sorted), then the closing brackets — so the span is
        // exactly [item0, len) recorded here, pre-splice. Item0 was
        // captured at entry, before this node pushed anything.
        if let Some(dom_id) = node_map.get(&node) {
            if styles
                .get(dom_id)
                .is_some_and(|s| s.position == Some(PositionMode::Sticky))
            {
                sticky_spans.push((*dom_id, item0, items.len()));
            }
        }
    }
    let mut abs_by_node: HashMap<taffy::tree::NodeId, Rect> = HashMap::new();
    let mut local_by_node: HashMap<taffy::tree::NodeId, (Rect, [f32; 6])> = HashMap::new();
    let mut node_first_item: HashMap<taffy::tree::NodeId, usize> = HashMap::new();
    // The ICB is not in node_map → always a barrier, so the deferred band
    // never survives the walk — asserted rather than assumed.
    let mut root_escapes: Vec<ZEscape> = Vec::new();
    collect(
        tree,
        taffy_tree,
        node_map,
        styles,
        fonts,
        images,
        static_pos,
        baseline_shifts,
        collapsed_edges,
        &mut rects,
        &mut local_geom,
        &mut abs_by_node,
        &mut local_by_node,
        &mut node_first_item,
        &mut sticky_spans,
        &mut scroller_spans,
        &mut items,
        &mut paint_order,
        &mut root_escapes,
        icb_node,
        (0.0, 0.0),
        viewport_width,
        Xf::IDENTITY,
        1.0,
        None,
    );
    debug_assert!(root_escapes.is_empty());
    // Flattened inline wrappers (span/label/a/… — obscura#722 lineage) own no
    // taffy box: the run hoisted their children. getBoundingClientRect still
    // owes them a rect, so union the hoisted kids' absolute boxes into one
    // bounding box. CSS unions the element's own fragments; kids approximate
    // that closely for text content (exact for the common cases).
    for (dom, kids) in flattened {
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
            let (_, _, lh) = font_context(tree, *dom, styles, fonts);
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
    // Inline backgrounds (blitz#340 family): a flattened inline element has
    // no taffy box, so the collect walk never reaches the boxed-Bg emission
    // for it and the background-color silently vanished (nicoburns'
    // diagnosis in that issue is the same shape). Paint it as per-line bands
    // over the hoisted content's absolute rects, spliced at the content's
    // first paint item so the color lands under the element's own ink.
    // Nested inlines record a later first item, so their bands splice after
    // the outer's and overpaint it — outer bg under inner bg under ink, the
    // CSS inline paint order.
    if !flattened.is_empty() {
        // A rotated ancestor's SetXf bracket maps everything inside it, and
        // the band rects below are CANVAS-space (each line band maps through
        // the leaf's recorded map) — splicing them raw there would
        // double-map. Bands falling inside a bracket get wrapped in a
        // SetXfCanvas/ClearXf pair that cancels the enclosing map (see the
        // enum doc), so the rotated inline background paints at its canvas
        // rect, still under the leaf's own ink.
        let mut inside_xf = vec![false; items.len()];
        {
            let mut depth = 0usize;
            for (i, it) in items.iter().enumerate() {
                inside_xf[i] = depth > 0;
                match it {
                    PaintItem::SetXf { .. } => depth += 1,
                    PaintItem::ClearXf => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        let wrapper_set: std::collections::HashSet<_> = run_wrappers.iter().copied().collect();
        let mut inserts: Vec<(usize, usize, Vec<PaintItem>)> = Vec::new();
        for (dom, kids) in flattened {
            // Paint-contributing decoration on a flattened inline (#21):
            // the solid per-line bands this pass always had, a
            // background-image gradient, or a border. The old guard skipped
            // on missing bg-COLOR, silently dropping gradient-only and
            // border-only spans entirely.
            let st = styles.get(dom);
            let bg = st.and_then(|s| s.background_color).filter(|c| c.3 != 0);
            let grad = st
                .and_then(|s| s.background_image.as_deref())
                .and_then(crate::diting_css::parse_linear_gradient);
            // (#26): box-shadow layers paint even on a span with no
            // background, gradient, or border at all — shadow-only spans
            // previously fell straight through this guard.
            let shadows = st
                .and_then(|s| s.box_shadow.clone())
                .filter(|v| !v.is_empty());
            let border = st.filter(|s| s.border_style.is_some()).and_then(|s| {
                let widths = [
                    side_px(s.border_width.top),
                    side_px(s.border_width.right),
                    side_px(s.border_width.bottom),
                    side_px(s.border_width.left),
                ];
                widths.iter().any(|w| *w > 0.0).then(|| {
                    let color = s
                        .border_color
                        .or(s.color)
                        .map(|c| [c.0, c.1, c.2, c.3])
                        .unwrap_or([0, 0, 0, 255]);
                    (widths, color)
                })
            });
            if bg.is_none() && grad.is_none() && border.is_none() && shadows.is_none() {
                continue;
            }
            // The recorded kids can be leaves directly or run wrappers
            // holding them at any depth (nested inlines flatten wrapper
            // into wrapper); descend through wrappers so bands measure
            // text geometry, not wrapper spans.
            let mut pieces: Vec<Rect> = Vec::new();
            let mut owners: Vec<taffy::tree::NodeId> = Vec::new();
            for k in kids {
                expand_wrapped_leaves(
                    *k,
                    taffy_tree,
                    &wrapper_set,
                    &local_by_node,
                    fonts,
                    &mut pieces,
                    &mut owners,
                );
            }
            let Some(&idx) = owners.iter().filter_map(|n| node_first_item.get(n)).min() else {
                continue;
            };
            let inside = inside_xf.get(idx).copied().unwrap_or(false);
            // Group pieces into line bands by vertical overlap: same-line
            // leaves share the line box even where baseline shifts split
            // their tops; different lines never overlap.
            pieces.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
            let mut bands: Vec<Rect> = Vec::new();
            for r in pieces {
                match bands.last_mut() {
                    Some(last) if r.y < last.y + last.height => {
                        let bottom = (last.y + last.height).max(r.y + r.height);
                        let right = (last.x + last.width).max(r.x + r.width);
                        last.y = last.y.min(r.y);
                        last.x = last.x.min(r.x);
                        last.width = right - last.x;
                        last.height = bottom - last.y;
                    }
                    _ => bands.push(r),
                }
            }
            let mut band_items: Vec<PaintItem> = Vec::new();
            // (#26) slice corner radii: resolve the span's corner_radii
            // against each band box, then box-decoration-break: slice — the
            // FIRST fragment keeps the left corners (TL/BL), the LAST the
            // right (TR/BR), middles are square (Chrome headless evidence on
            // the issue). Per-band clamping shrinks oversized radii the way
            // Chrome's proportional reduction turns a too-tall 16px radius
            // into the elliptical arc on a short line fragment.
            let corners = st.and_then(|s| s.corner_radii);
            let res = |l: &crate::diting_css::Length, basis: f32| match l {
                crate::diting_css::Length::Px(v) => *v,
                crate::diting_css::Length::Percent(p) => p * basis / 100.0,
                crate::diting_css::Length::Calc { percent, .. } => percent * basis / 100.0,
                crate::diting_css::Length::Auto
                | crate::diting_css::Length::MinContent
                | crate::diting_css::Length::MaxContent
                | crate::diting_css::Length::FitContent => 0.0,
            };
            let n_bands = bands.len();
            let frag_radii = |i: usize, band: &Rect| -> [(f32, f32); 4] {
                corners
                    .map(|cs| {
                        let mut rs = std::array::from_fn(|k| {
                            (res(&cs[k].0, band.width), res(&cs[k].1, band.height))
                        });
                        if n_bands > 1 {
                            if i == 0 {
                                rs[1] = (0.0, 0.0);
                                rs[2] = (0.0, 0.0);
                            } else if i + 1 == n_bands {
                                rs[0] = (0.0, 0.0);
                                rs[3] = (0.0, 0.0);
                            } else {
                                rs = [(0.0, 0.0); 4];
                            }
                        }
                        clamp_corner_radii(rs, band.width, band.height)
                    })
                    .unwrap_or([(0.0, 0.0); 4])
            };
            // Shadow geometry rides the fragment border box: the band grown
            // outward by the border widths (left edge only on the first
            // fragment, right only on the last — same slice rule the border
            // strips below follow).
            let [bt, br_, bb, bl] = border.as_ref().map(|(w, _)| *w).unwrap_or([0.0; 4]);
            let frag_rect = |i: usize, band: &Rect| Rect {
                x: if i == 0 { band.x - bl } else { band.x },
                y: band.y - bt,
                width: (if i + 1 == n_bands { band.x + band.width + br_ } else { band.x + band.width })
                    - (if i == 0 { band.x - bl } else { band.x }),
                height: band.height + bt + bb,
            };
            // Layer order mirrors the block walk: outer shadows below the
            // background, bg-color, bg-image gradient, inset shadows above
            // the background, border strips on top.
            if let Some(ref list) = shadows {
                for sh in list.iter().rev().filter(|sh| !sh.inset) {
                    for (i, band) in bands.iter().enumerate() {
                        band_items.push(PaintItem::BoxShadow {
                            rect: frag_rect(i, band),
                            color: [sh.color.0, sh.color.1, sh.color.2, sh.color.3],
                            radii: frag_radii(i, band),
                            dx: sh.dx,
                            dy: sh.dy,
                            blur: sh.blur,
                            spread: sh.spread,
                            inset: false,
                        });
                    }
                }
            }
            if let Some(color) = bg {
                let color = [color.0, color.1, color.2, color.3];
                for (i, rect) in bands.iter().enumerate() {
                    let radii = frag_radii(i, rect);
                    if radii.iter().all(|r| *r == radii[0]) {
                        band_items.push(PaintItem::Bg { rect: *rect, color, radius: radii[0].0 });
                    } else {
                        band_items.push(PaintItem::BgCorner { rect: *rect, color, radii });
                    }
                }
            }
            if let Some(g) = grad {
                // Continuous strip (Blink PaintRectForImageStrip): every
                // fragment samples the gradient defined over the UNION of
                // the bands, clipped to its own band — a to-right gradient
                // never restarts on later lines.
                let ux = bands.iter().map(|r| r.x).fold(f32::INFINITY, f32::min);
                let uy = bands.iter().map(|r| r.y).fold(f32::INFINITY, f32::min);
                let ur = bands.iter().map(|r| r.x + r.width).fold(f32::NEG_INFINITY, f32::max);
                let ub = bands.iter().map(|r| r.y + r.height).fold(f32::NEG_INFINITY, f32::max);
                if ur > ux && ub > uy {
                    let union = Rect { x: ux, y: uy, width: ur - ux, height: ub - uy };
                    let stops: Vec<(f32, [u8; 4])> =
                        g.stops.iter().map(|(p, c)| (*p, [c.0, c.1, c.2, c.3])).collect();
                    for (i, band) in bands.iter().enumerate() {
                        band_items.push(PaintItem::Clip { rect: *band });
                        band_items.push(PaintItem::BgGradient {
                            rect: union,
                            stops: stops.clone(),
                            css_deg: g.css_deg,
                            radii: frag_radii(i, band),
                        });
                        band_items.push(PaintItem::PopClip);
                    }
                }
            }
            if let Some(ref list) = shadows {
                for sh in list.iter().rev().filter(|sh| sh.inset) {
                    for (i, band) in bands.iter().enumerate() {
                        band_items.push(PaintItem::BoxShadow {
                            rect: frag_rect(i, band),
                            color: [sh.color.0, sh.color.1, sh.color.2, sh.color.3],
                            radii: frag_radii(i, band),
                            dx: sh.dx,
                            dy: sh.dy,
                            blur: sh.blur,
                            spread: sh.spread,
                            inset: true,
                        });
                    }
                }
            }
            if let Some((widths, color)) = border {
                // box-decoration-break: slice — top/bottom edges ride every
                // fragment; left only the first, right only the last. The
                // strips sit OUTSIDE the text bands (Chrome grows the
                // fragment border-box outward), so they never cover glyphs.
                let [t, r, b, l] = widths;
                for (i, band) in bands.iter().enumerate() {
                    let x0 = if i == 0 { band.x - l } else { band.x };
                    let x1 = if i + 1 == bands.len() { band.x + band.width + r } else { band.x + band.width };
                    if t > 0.0 {
                        band_items.push(PaintItem::Bg {
                            rect: Rect { x: x0, y: band.y - t, width: x1 - x0, height: t },
                            color,
                            radius: 0.0,
                        });
                    }
                    if b > 0.0 {
                        band_items.push(PaintItem::Bg {
                            rect: Rect { x: x0, y: band.y + band.height, width: x1 - x0, height: b },
                            color,
                            radius: 0.0,
                        });
                    }
                    if i == 0 && l > 0.0 {
                        band_items.push(PaintItem::Bg {
                            rect: Rect { x: band.x - l, y: band.y - t, width: l, height: band.height + t + b },
                            color,
                            radius: 0.0,
                        });
                    }
                    if i + 1 == bands.len() && r > 0.0 {
                        band_items.push(PaintItem::Bg {
                            rect: Rect { x: band.x + band.width, y: band.y - t, width: r, height: band.height + t + b },
                            color,
                            radius: 0.0,
                        });
                    }
                }
            }
            if inside {
                band_items.insert(0, PaintItem::SetXfCanvas);
                band_items.push(PaintItem::ClearXf);
            }
            inserts.push((idx, tree.ancestors(*dom).len(), band_items));
        }
        // Later splice points first, deeper element first on ties, so each
        // batch lands under everything recorded after it.
        inserts.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        // By reference: the sticky-span adjustment below re-reads the
        // pre-splice indices and band sizes after this loop.
        for (idx, _, band_items) in &inserts {
            items.splice(*idx..*idx, band_items.iter().cloned());
        }
        // Sticky spans were recorded pre-splice, and the insert indices are
        // pre-splice too: shift each endpoint by every insert landing inside
        // it. A start counts an insert AT the boundary (a band spliced at
        // the sticky node's own first item is its inline background); an
        // end is exclusive, so only strictly-inside inserts count.
        if !sticky_spans.is_empty() {
            for (_, start, end) in sticky_spans.iter_mut() {
                let (s0, e0) = (*start, *end);
                *start = s0
                    + inserts
                        .iter()
                        .map(|(idx, _, band)| usize::from(*idx <= s0) * band.len())
                        .sum::<usize>();
                *end = e0
                    + inserts
                        .iter()
                        .map(|(idx, _, band)| usize::from(*idx < e0) * band.len())
                        .sum::<usize>();
            }
        }
    }
    (rects, items, paint_order, local_geom, sticky_spans, scroller_spans)
}


/// Compute every element's cascade result for a parsed tree + stylesheet:
/// the production style-resolution pass feeding [`layout_dom`]. Walks the
/// tree once; each element matches the full rule set (O(rules × elements) —
/// fine for page-sized inputs, a matching index is future work), chains
/// inherited properties from the parent's computed style, and applies the
/// inline `style` attribute last.
/// Per-element gating for `@container` arms (moli#282): the container
/// conditions answer against the element's nearest qualifying container
/// ancestor, which only exists after a layout pass — so the plan is built
/// between a probe solve and the final cascade. `gates` is keyed by
/// ABSOLUTE rule index in the extended rule slice (`container arms sit at
/// index >= base_len`); values are ascending DOM node indexes ready for
/// the cascade's binary-search membership test.
pub(crate) struct ContainerPlan {
    pub extra_rules: Vec<crate::diting_css::ParsedRule>,
    pub gates: HashMap<usize, Vec<usize>>,
}

struct ContainerStackEntry {
    name: Option<String>,
    w: f32,
    h: f32,
}

/// Evaluate every container rule's inner selectors against the tree and
/// record which elements they reach through a passing container. Rules with
/// container-query units (cqw/cqh/cqi/cqb) in their declarations are split
/// per distinct container size so the units bake to concrete px per group.
pub(crate) fn container_plan(
    tree: &DomTree,
    styles: &HashMap<NodeId, crate::diting_css::ComputedStyle>,
    boxes: &HashMap<NodeId, (f32, f32)>,
    container_rules: &[crate::diting_css::ContainerRule],
    base_len: usize,
) -> ContainerPlan {
    use crate::diting_css::ContainerType;
    let mut entries: Vec<(crate::diting_css::ParsedRule, Option<String>, Option<crate::diting_css::ContainerCondition>)> =
        Vec::new();
    for cr in container_rules {
        for r in &cr.rules {
            entries.push((r.clone(), cr.name.clone(), cr.condition.clone()));
        }
    }
    if entries.is_empty() {
        return ContainerPlan { extra_rules: Vec::new(), gates: HashMap::new() };
    }
    let selectors: Vec<&str> = entries.iter().map(|(r, _, _)| r.selector.as_str()).collect();
    let sets = tree.rule_match_sets(&selectors);
    // Per inner rule: (node index, container w, container h) in document order.
    let mut acc: Vec<Vec<(usize, f32, f32)>> = vec![Vec::new(); entries.len()];
    #[allow(clippy::too_many_arguments)]
    fn dfs(
        tree: &DomTree,
        styles: &HashMap<NodeId, crate::diting_css::ComputedStyle>,
        boxes: &HashMap<NodeId, (f32, f32)>,
        sets: &crate::diting_dom::selector::RuleMatchSets,
        entries: &[(crate::diting_css::ParsedRule, Option<String>, Option<crate::diting_css::ContainerCondition>)],
        acc: &mut [Vec<(usize, f32, f32)>],
        nid: NodeId,
        stack: &mut Vec<ContainerStackEntry>,
    ) {
        let is_element = tree.with_node(nid, |n| n.is_element()).unwrap_or(false);
        if is_element {
            // The element's own container-type cannot satisfy its own query:
            // gate evaluation runs before the self-push below.
            if !stack.is_empty() {
                for (ri, (_, name, condition)) in entries.iter().enumerate() {
                    if sets
                        .hits
                        .get(&ri)
                        .is_none_or(|h| h.binary_search(&nid.index()).is_err())
                    {
                        continue;
                    }
                    let target = stack.iter().rev().find(|c| {
                        name.as_ref().is_none_or(|n| c.name.as_deref() == Some(n.as_str()))
                    });
                    if let Some(c) = target {
                        if condition.as_ref().is_none_or(|cond| cond.matches(c.w, c.h)) {
                            acc[ri].push((nid.index(), c.w, c.h));
                        }
                    }
                }
            }
            if let Some(cs) = styles.get(&nid) {
                if cs.container_type != ContainerType::Normal {
                    if let Some(&(w, h)) = boxes.get(&nid) {
                        stack.push(ContainerStackEntry { name: cs.container_name.clone(), w, h });
                    }
                }
            }
        }
        for child in tree.children(nid) {
            dfs(tree, styles, boxes, sets, entries, acc, child, stack);
        }
        if is_element {
            if let Some(cs) = styles.get(&nid) {
                if cs.container_type != ContainerType::Normal && boxes.contains_key(&nid) {
                    stack.pop();
                }
            }
        }
    }
    let mut stack = Vec::new();
    dfs(tree, styles, boxes, &sets, &entries, &mut acc, tree.document(), &mut stack);

    let mut extra_rules = Vec::new();
    let mut gates: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ri, (rule, _, _)) in entries.iter().enumerate() {
        let hits = &acc[ri];
        if hits.is_empty() {
            continue;
        }
        let has_cq_units = ["cqw", "cqh", "cqi", "cqb"]
            .iter()
            .any(|u| rule.declarations.contains(u));
        if !has_cq_units {
            let idx = base_len + extra_rules.len();
            let mut nodes: Vec<usize> = hits.iter().map(|(n, _, _)| *n).collect();
            nodes.sort_unstable();
            nodes.dedup();
            extra_rules.push(rule.clone());
            gates.insert(idx, nodes);
        } else {
            // Same selector may answer through different containers → the
            // unit bake differs per container. Group by container size.
            let mut groups: std::collections::HashMap<(i64, i64), (f32, f32, Vec<usize>)> =
                std::collections::HashMap::new();
            for (n, w, h) in hits {
                let key = ((w * 100.0) as i64, (h * 100.0) as i64);
                groups.entry(key).or_insert((*w, *h, Vec::new())).2.push(*n);
            }
            for (_, (w, h, mut nodes)) in groups {
                let idx = base_len + extra_rules.len();
                nodes.sort_unstable();
                nodes.dedup();
                extra_rules.push(crate::diting_css::ParsedRule {
                    selector: rule.selector.clone(),
                    declarations: bake_container_units(&rule.declarations, w, h),
                });
                gates.insert(idx, nodes);
            }
        }
    }
    ContainerPlan { extra_rules, gates }
}

/// Rewrite container-query lengths (`10cqw`, `2.5cqh`, ...) against the
/// answering container's border box into px, so the ordinary declaration
/// parser never sees the units. Non-container tokens (including hex colors
/// and URLs containing digit+letter runs) pass through byte-identical.
fn bake_container_units(text: &str, w: f32, h: f32) -> String {
    fn push_px(out: &mut String, px: f32) {
        if px.fract() == 0.0 && px.abs() < 1e15 {
            out.push_str(&format!("{}px", px as i64));
        } else {
            out.push_str(&format!("{:.3}", px));
            out.push_str("px");
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let num_start = c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
            || (c == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()));
        if !num_start {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        if chars[i] == '-' {
            i += 1;
        }
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        let ustart = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit: String = chars[ustart..i].iter().collect();
        match unit.as_str() {
            "cqw" | "cqi" => {
                let v: f32 = chars[start..ustart].iter().collect::<String>().parse().unwrap_or(0.0);
                push_px(&mut out, v * w / 100.0);
            }
            "cqh" | "cqb" => {
                let v: f32 = chars[start..ustart].iter().collect::<String>().parse().unwrap_or(0.0);
                push_px(&mut out, v * h / 100.0);
            }
            _ => out.extend(chars[start..i].iter()),
        }
    }
    out
}

pub fn compute_styles(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
    viewport: (f32, f32),
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    // Static renders (screenshot/svg paths): no registered transitions —
    // the cascade values stand.
    compute_styles_timed(
        tree,
        rules,
        &crate::diting_css::KeyframesMap::new(),
        None,
        &[],
        viewport,
    )
}

/// The animated face of [`compute_styles`]: after the cascade resolves each
/// element, its `animation` shorthand is sampled against the stylesheet's
/// `@keyframes` table at `css_time`. `None` samples the end state (the
/// poster posture for static renders); the video pump feeds real seconds
/// here. Everything the sampler touches is paint-channel only
/// (opacity/transform/stroke-dashoffset), so this runs inside the
/// collect-cache (#395) invalidation domain — `set_css_time` drops the
/// paint-only caches and the next read re-cascades fresh.
#[allow(clippy::too_many_arguments)]
fn compute_styles_impl(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
    keyframes: &crate::diting_css::KeyframesMap,
    css_time: Option<f64>,
    within_root: Option<NodeId>,
    transitions: &[crate::diting_css::CssTransition],
    gated: Option<(usize, &HashMap<usize, Vec<usize>>)>,
    viewport: (f32, f32),
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        tree: &DomTree,
        rules: &[crate::diting_css::ParsedRule],
        sets: &crate::diting_dom::selector::RuleMatchSets,
        keyframes: &crate::diting_css::KeyframesMap,
        css_time: Option<f64>,
        nid: NodeId,
        parent: Option<&crate::diting_css::ComputedStyle>,
        root_fs: f32,
        out: &mut HashMap<NodeId, crate::diting_css::ComputedStyle>,
        counters: &mut CounterState,
        depth: usize,
        gated: Option<(usize, &HashMap<usize, Vec<usize>>)>,
        viewport: (f32, f32),
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
                let hits = sets.hits.get(&ri)?;
                if hits.binary_search(&nid.index()).is_err() {
                    return None;
                }
                // @container arms (index >= base) cascade only on elements
                // the container plan gated through a passing container.
                if let Some((base, gates)) = gated {
                    if ri >= base
                        && gates
                            .get(&ri)
                            .is_none_or(|v| v.binary_search(&nid.index()).is_err())
                    {
                        return None;
                    }
                }
                // Specificity was parsed once with the match sets above;
                // the old per-element compile_rule_selector re-parse cost
                // one selector parse per (element x matched rule).
                Some((rule, sets.specificity.get(ri).copied().flatten()?))
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
            viewport,
        );
        let mut cs = cs;
        crate::diting_css::sample_css_animation(&mut cs, keyframes, css_time);
        // CSS counters: apply this element's own reset/increment before its
        // pseudos resolve. The reset pushes a shadowing counter whose scope
        // lasts until this element's parent's subtree ends (see
        // pop_out_of_scope) — css-lists-3 scoping, not a flat clobber.
        // display:none elements generate no boxes and no counters.
        if cs.display != Some(crate::diting_css::Display::None)
            && (!cs.counter_reset.is_empty() || !cs.counter_increment.is_empty())
        {
            apply_counter_modifiers(counters, &cs.counter_reset, &cs.counter_increment, depth);
        }
        // Generated content: pseudo-element rules cascade a synthetic span
        // against the host's own matched set and hang off the host's
        // computed style; build_element synthesizes their boxes. ::before
        // resolves before the children and ::after after — quote depth and
        // counter mutations interleave with the subtree walk (css-content-3
        // box order), so one-shot resolution would close quotes opened
        // before the children ever ran.
        let mut pseudo_pair = crate::diting_css::PseudoPair::default();
        if !sets.pseudo_kinds.is_empty() {
            pseudo_pair.before = pseudo_styles(
                tree,
                rules,
                sets,
                nid,
                &cs,
                root_fs,
                counters,
                depth,
                crate::diting_dom::selector::PseudoKind::Before,
                gated,
                viewport,
            );
        }
        let child_root_fs = if parent.is_none() {
            cs.font_size.unwrap_or(crate::diting_css::DEFAULT_ROOT_FONT_SIZE)
        } else {
            root_fs
        };
        // Composed-tree children, each paired with its flat-tree
        // inheritance parent. A shadow host's shadow children inherit from
        // the host; a slot's assigned light children inherit from the SLOT
        // (pinned to Chrome 2026-09-16: `color` on the slot colors the
        // slotted child, and with it undeclared the host's value walks
        // through) — so the slot is walked before its assigned nodes and
        // its computed style is already in `out` when they cascade. A slot
        // with nothing assigned composes nothing here; its own visit
        // enumerates the fallback children against it. Unassigned light
        // children of a host are never visited — they render nothing, so
        // no computed style either.
        let slot_consumed = tree.is_html_slot_element(nid)
            && tree.assigned_nodes(nid).is_some_and(|a| !a.is_empty());
        let walk: Vec<(NodeId, NodeId)> = if slot_consumed {
            Vec::new()
        } else {
            let raw: Vec<NodeId> = match tree.shadow_root(nid) {
                Some(root) => tree.children(root),
                None => tree.children(nid),
            };
            let mut w = Vec::with_capacity(raw.len());
            for child in raw {
                w.push((child, nid));
                if tree.is_html_slot_element(child) {
                    if let Some(assigned) = tree.assigned_nodes(child) {
                        for node in assigned {
                            w.push((node, child));
                        }
                    }
                }
            }
            w
        };
        for (child, inherit_from) in walk {
            let parent = if inherit_from == nid {
                cs.clone()
            } else {
                out.get(&inherit_from)
                    .cloned()
                    .unwrap_or_else(|| cs.clone())
            };
            visit(
                tree,
                rules,
                sets,
                keyframes,
                css_time,
                child,
                Some(&parent),
                child_root_fs,
                out,
                counters,
                depth + 1,
                gated,
                viewport,
            );
        }
        if !sets.pseudo_kinds.is_empty() {
            pseudo_pair.after = pseudo_styles(
                tree,
                rules,
                sets,
                nid,
                &cs,
                root_fs,
                counters,
                depth,
                crate::diting_dom::selector::PseudoKind::After,
                gated,
                viewport,
            );
        }
        if pseudo_pair.before.is_some() || pseudo_pair.after.is_some() {
            cs.pseudos = Some(Box::new(pseudo_pair));
        }
        pop_out_of_scope(counters, depth);
        out.insert(nid, cs);
    }
    // Pseudo-element rules (::before/::after) for one host: their matched
    // sets live in pseudo_hits (the selector layer kept them OUT of hits so
    // the normal cascade above never sees them), and each kind cascades a
    // synthetic span whose parent is the host — undeclared properties
    // inherit exactly like a real child's would.
    #[allow(clippy::too_many_arguments)]
    fn pseudo_styles(
        tree: &DomTree,
        rules: &[crate::diting_css::ParsedRule],
        sets: &crate::diting_dom::selector::RuleMatchSets,
        nid: NodeId,
        host: &crate::diting_css::ComputedStyle,
        root_fs: f32,
        counters: &mut CounterState,
        depth: usize,
        wanted: crate::diting_dom::selector::PseudoKind,
        gated: Option<(usize, &HashMap<usize, Vec<usize>>)>,
        viewport: (f32, f32),
    ) -> Option<crate::diting_css::ComputedStyle> {
        use crate::diting_css::ContentValue;
        use crate::diting_dom::selector::PseudoKind;
        let mut pair = crate::diting_css::PseudoPair::default();
        let slot = match wanted {
            PseudoKind::Before => &mut pair.before,
            _ => &mut pair.after,
        };
        {
            let matched: Vec<(&crate::diting_css::ParsedRule, u32)> = rules
                .iter()
                .enumerate()
                .filter_map(|(ri, rule)| {
                    if sets.pseudo_kinds.get(&ri) != Some(&wanted) {
                        return None;
                    }
                    let hits = sets.pseudo_hits.get(&ri)?;
                    if hits.binary_search(&nid.index()).is_err() {
                        return None;
                    }
                    if let Some((base, gates)) = gated {
                        if ri >= base
                            && gates
                                .get(&ri)
                                .is_none_or(|v| v.binary_search(&nid.index()).is_err())
                        {
                            return None;
                        }
                    }
                    Some((rule, sets.specificity.get(ri).copied().flatten()?))
                })
                .collect();
            if matched.is_empty() {
                return None;
            }
            // Tag "span": the UA sheet's inline display and none of the
            // tag-gated UA branches (their attribute reads fire only on
            // a/td/th/tr hosts).
            let mut p = crate::diting_css::cascade_element(
                "span", tree, nid, &matched, Some(host), None, root_fs, viewport,
            );
            // attr() resolves against the HOST's attributes; a missing
            // attribute yields the empty string. Counters and quotes resolve
            // against the walk state: the pseudo's OWN reset/increment apply
            // to the persistent state (css-lists-3 — `li::before
            // {counter-increment: item}` is the canonical pattern; the entry
            // lives at the host's depth, so it survives the host's own
            // subtree exit and dies with the host's parent), then the whole
            // content flattens to a plain string for the box builder.
            apply_counter_modifiers(counters, &p.counter_reset, &p.counter_increment, depth);
            let quotes = p.quotes.clone();
            let cv = p.content.take();
            if let Some(cv) = cv {
                let mut out = String::new();
                resolve_content_into(&cv, tree, nid, counters, quotes.as_deref(), &mut out);
                p.content = Some(ContentValue::Str(out));
            }
            // Clearfix box: display:table coerces to block — the generated
            // box needs flow presence (it becomes a taffy child of the host),
            // not the table walk.
            if p.display == Some(crate::diting_css::Display::Table) {
                p.display = Some(crate::diting_css::Display::Block);
            }
            *slot = Some(p);
        }
        slot.clone()
    }
    // One querySelectorAll per RULE over the whole document, sorted for the
    // binary search in visit. This replaces the per-element-per-rule full-doc
    // scan that made style resolution quadratic-cubic on real pages.
    // One bucketed tree walk replaces one full-document querySelectorAll
    // per rule: the rightmost-compound rule hash (see RuleMatchSets for
    // the WeChat numbers behind this) builds every rule's document match
    // set AND each selector's specificity in a single pass, both sorted
    // once for the binary searches in visit.
    let rule_selectors: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
    // The document run probes document(+shadow) descendants; a within-run
    // probes only the orphan root's subtree (rule_match_sets_within).
    let sets = match within_root {
        Some(root) => tree.rule_match_sets_within(&rule_selectors, &[root]),
        None => tree.rule_match_sets(&rule_selectors),
    };

    let mut out = HashMap::new();
    if std::env::var("AGINXBROWSER_LAYOUT_TRACE").is_ok() {
        let elements = out_capacity_hint(tree);
        eprintln!(
            "[styles-trace] rules={} elements={} rule_match_sets={}",
            rules.len(),
            elements,
            sets.hits.len()
        );
    }
    let mut counters = CounterState::default();
    match within_root {
        Some(root) => visit(
            tree,
            rules,
            &sets,
            keyframes,
            css_time,
            root,
            None,
            crate::diting_css::DEFAULT_ROOT_FONT_SIZE,
            &mut out,
            &mut counters,
            0,
            gated,
            viewport,
        ),
        None => {
            for child in tree.children(tree.document()) {
                visit(
                    tree,
                    rules,
                    &sets,
                    keyframes,
                    css_time,
                    child,
                    None,
                    crate::diting_css::DEFAULT_ROOT_FONT_SIZE,
                    &mut out,
                    &mut counters,
                    0,
                    gated,
                    viewport,
                );
            }
        }
    }
    // Exit-phase transition sampling (CSS transitions batch): registered
    // entries override cascade values once per pass, after the visit.
    if !transitions.is_empty() {
        crate::diting_css::sample_css_transitions(transitions, css_time, &mut out);
    }
    out
}

/// Document-rooted style resolution (main path).
pub fn compute_styles_timed(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
    keyframes: &crate::diting_css::KeyframesMap,
    css_time: Option<f64>,
    transitions: &[crate::diting_css::CssTransition],
    viewport: (f32, f32),
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    compute_styles_impl(tree, rules, keyframes, css_time, None, transitions, None, viewport)
}

/// Second-pass face for `@container` arms (moli#282): `gates` maps ABSOLUTE
/// rule indexes (>= `base_len`, the container arms appended past the base
/// rules) to the elements that reached them through a passing container.
/// Callers build the plan from a probe pass's geometry, extend `rules` with
/// the plan's extra rules, and re-cascade through here — the gated arms
/// then match exactly those elements.
#[allow(clippy::too_many_arguments)]
pub fn compute_styles_gated(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
    keyframes: &crate::diting_css::KeyframesMap,
    css_time: Option<f64>,
    transitions: &[crate::diting_css::CssTransition],
    base_len: usize,
    gates: &HashMap<usize, Vec<usize>>,
    viewport: (f32, f32),
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    compute_styles_impl(
        tree,
        rules,
        keyframes,
        css_time,
        None,
        transitions,
        Some((base_len, gates)),
        viewport,
    )
}

/// Subtree variant (fabricated iframe documents, obscura #976 family): the
/// orphan root plays the root-element role — no parent cascade, default
/// root font size — and the rule match sets probe only its descendants.
pub fn compute_styles_timed_within(
    tree: &DomTree,
    rules: &[crate::diting_css::ParsedRule],
    keyframes: &crate::diting_css::KeyframesMap,
    css_time: Option<f64>,
    root: NodeId,
    transitions: &[crate::diting_css::CssTransition],
    viewport: (f32, f32),
) -> HashMap<NodeId, crate::diting_css::ComputedStyle> {
    compute_styles_impl(tree, rules, keyframes, css_time, Some(root), transitions, None, viewport)
}

/// Trace-only element count (a full walk just for the debug knob; keep out of
/// the hot path when the knob is off).
fn out_capacity_hint(tree: &DomTree) -> usize {
    fn count(tree: &DomTree, nid: NodeId, acc: &mut usize) {
        if tree.with_node(nid, |n| n.as_element().is_some()) == Some(true) {
            *acc += 1;
        }
        for child in tree.children(nid) {
            count(tree, child, acc);
        }
    }
    let mut acc = 0;
    count(tree, tree.document(), &mut acc);
    acc
}

#[cfg(test)]
mod fork_deltas;

// (The dual-engine bridge_cross_check tests moved to the product crate with
// the workspace split — src/bridge_cross_check.rs — so diting carries zero
// blitz/product references; ARCHITECTURE.md §2 rule R2.)

#[cfg(test)]
mod tests;
