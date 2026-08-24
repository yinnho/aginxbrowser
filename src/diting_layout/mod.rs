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
    Run { text: String, font_size: f32, bold: bool, color: [u8; 4] },
    /// One word/glyph of a MIXED run (text around inline elements): the
    /// batch-2b word-leaf fallback, now carrying paint context (batch 4d).
    /// Layout is still style-driven — the measure closure passes Word
    /// leaves straight through to taffy's own style sizing.
    Word { text: String, font_size: f32, bold: bool, color: [u8; 4] },
}

/// Approximate used line height for text leaves. Matches blitz exactly:
/// blitz-dom maps CSS `line-height: normal` to `font_size * 1.2`
/// (src/layout/mod.rs:76) rather than deriving from font metrics, and the
/// cross-check asserts text-derived heights against it.
fn line_height(font_size: f32) -> f32 {
    font_size * 1.2
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
        CssDisplay::Inline => Display::Flex,
        CssDisplay::None => Display::None,
    };
    if promote {
        s.flex_direction = FlexDirection::Column;
        s.align_items = match style.text_align {
            Some(TextAlign::Center) => Some(AlignItems::CENTER),
            Some(TextAlign::Right) => Some(AlignItems::FLEX_END),
            _ => None,
        };
    } else if display == CssDisplay::Inline {
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
        None => LengthPercentage::length(0.0),
    };
    // Margin has an auto variant; unset margins are CSS `0`, not auto.
    let lpa_zero = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentageAuto::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
        None => LengthPercentageAuto::length(0.0),
    };
    // Inset/clamp unset values are CSS `auto`.
    let lpa_auto = |v: Option<crate::diting_css::Length>| match v {
        Some(crate::diting_css::Length::Px(px)) => LengthPercentageAuto::length(px),
        Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
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
            None => auto(),
        },
        height: match style.height {
            Some(crate::diting_css::Length::Px(h)) => Dimension::length(
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
            ),
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            None => auto(),
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
            None => LengthPercentageAuto::auto(),
        },
        height: match style.min_height {
            Some(crate::diting_css::Length::Px(h)) => LengthPercentageAuto::length(
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            None => LengthPercentageAuto::auto(),
        },
    };
    s.max_size = Size {
        width: match style.max_width {
            Some(crate::diting_css::Length::Px(w)) => LengthPercentageAuto::length(
                w + side_px(style.padding.left) + side_px(style.padding.right) + bl + br,
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            None => LengthPercentageAuto::auto(),
        },
        height: match style.max_height {
            Some(crate::diting_css::Length::Px(h)) => LengthPercentageAuto::length(
                h + side_px(style.padding.top) + side_px(style.padding.bottom) + bt + bb,
            ),
            Some(crate::diting_css::Length::Percent(p)) => LengthPercentageAuto::percent(p / 100.0),
            None => LengthPercentageAuto::auto(),
        },
    };
    if let Some(ar) = style.aspect_ratio {
        if ar.is_finite() && ar > 0.0 {
            s.aspect_ratio = Some(ar);
        }
    }
    s
}

/// diting_css track → taffy track: `1fr` maps to minmax(auto, 1fr), a px
/// track is fixed, `auto` sizes to content.
fn to_grid_track(track: GridTrack) -> taffy::style::GridTemplateComponent<String> {
    use taffy::style::TrackSizingFunction;
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
    }
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
/// weight (defaults 16px / 400). Since batch 2e every cascaded element
/// carries a resolved font-size, so the walk stops at the NEAREST value
/// instead of relying on outer ancestors being None.
fn font_context(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, ComputedStyle>) -> (f32, bool) {
    let mut font_size: Option<f32> = None;
    let mut bold: Option<bool> = None;
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
            if font_size.is_some() && bold.is_some() {
                break;
            }
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    (font_size.unwrap_or(16.0), bold.unwrap_or(false))
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
            tokens.push(" ".to_string());
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

/// One taffy leaf per word (upstream's per-word leaf model). Intrinsic size
/// from real shaped advances (batch 3a); wrapping happens in the enclosing
/// flex-wrap run container. Since batch 4d the leaf carries paint context —
/// mixed runs paint their words.
fn build_word_leaves(
    text: &str,
    font_size: f32,
    bold: bool,
    color: [u8; 4],
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
) -> Vec<taffy::tree::NodeId> {
    tokenize(text)
        .into_iter()
        .filter_map(|token| {
            let width = fonts.advance_width(&token, font_size, bold);
            // Pure-whitespace tokens contribute no height (they sit between
            // block siblings without adding a spurious blank row).
            let height = if token.trim().is_empty() { 0.0 } else { line_height(font_size) };
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
            };
            taffy_tree.new_leaf_with_context(style, leaf).ok()
        })
        .collect()
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
/// - height = line count × 1.2×fs (blitz's pinned `normal`, layout/mod.rs:76).
fn measure_text_leaf(
    text: &str,
    font_size: f32,
    bold: bool,
    fonts: &FontBook,
    inputs: &taffy::tree::LayoutInput,
) -> taffy::tree::LayoutOutput {
    let known = inputs.known_dimensions;
    let lh = line_height(font_size);
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
    taffy::tree::LayoutOutput::from_sizes(size, size)
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
/// no children in the layout tree.
fn is_replaced_tag(tag: &str) -> bool {
    matches!(tag, "img" | "video" | "iframe" | "canvas" | "object" | "embed")
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
        t if t == "canvas" => (
            aw.unwrap_or(300.0),
            ah.unwrap_or(150.0),
            true,
        ),
        t if t == "video" || t == "iframe" || t == "embed" || t == "object" => {
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
            None => Dimension::length(nat_w + if bline { bl + br } else { 0.0 }),
        },
        height: match style.height {
            Some(crate::diting_css::Length::Px(h)) => {
                Dimension::length(h + if bline { bt + bb } else { 0.0 })
            }
            Some(crate::diting_css::Length::Percent(p)) => Dimension::percent(p / 100.0),
            None => Dimension::length(nat_h + if bline { bt + bb } else { 0.0 }),
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
    atomic_container: bool,
    font_size: f32,
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
        if inline_level && !atomic_container && !out_of_flow {
            if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map) {
                // A lone inline atom still gets the wrapping-run stand-in so
                // it lays out on the text baseline path like the main loop.
                if let Ok(wrapper) =
                    taffy_tree.new_with_children(run_wrapper_style(), &[leaf])
                {
                    direct.push(wrapper);
                }
            }
        } else if let Some(leaf) = build_replaced_leaf(tree, child, styles, images, taffy_tree, node_map) {
            direct.push(leaf);
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
            let (fs, b) = font_context(tree, child, styles);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            let col = color_context(tree, child, styles);
            leaves.extend(build_word_leaves(&text, fs, b, col, fonts, taffy_tree));
        } else {
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                leaves.extend(sub_children);
                // Flattening removes the sub's taffy node (invalidating its
                // SlotMap key) — drop the stale node_map entry with it.
                node_map.remove(&sub);
                let _ = taffy_tree.remove(sub);
            }
        }
        if !leaves.is_empty() {
            if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
                direct.push(wrapper);
            }
        }
        return;
    }
    if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map) {
        direct.push(node);
    }
}

fn build_element(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    images: &HashMap<NodeId, DecodedImage>,
    fonts: &FontBook,
    taffy_tree: &mut TaffyTree<TextLeaf>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
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
        Text(String, f32, bool, [u8; 4]),
        Nodes(Vec<taffy::tree::NodeId>),
    }
    let mut direct: Vec<taffy::tree::NodeId> = Vec::new();
    let mut run: Vec<RunSeg> = Vec::new();
    let flush_run = |run: &mut Vec<RunSeg>, direct: &mut Vec<taffy::tree::NodeId>, taffy_tree: &mut TaffyTree<TextLeaf>| {
        if run.is_empty() {
            return;
        }
        let segs = std::mem::take(run);
        // All-text run → one measured leaf (adjacent DOM text nodes
        // concatenate, which is also how CSS joins them).
        if segs.iter().all(|s| matches!(s, RunSeg::Text(..))) {
            let text = segs
                .iter()
                .map(|s| match s { RunSeg::Text(t, ..) => t.as_str(), _ => "" })
                .collect::<String>();
            let RunSeg::Text(_, fs, bold, color) = &segs[0] else { unreachable!() };
            if let Ok(leaf) = taffy_tree.new_leaf_with_context(
                Style::default(),
                TextLeaf::Run { text, font_size: *fs, bold: *bold, color: *color },
            ) {
                direct.push(leaf);
                return;
            }
        }
        let mut leaves: Vec<taffy::tree::NodeId> = Vec::new();
        for seg in segs {
            match seg {
                RunSeg::Text(text, fs, bold, color) => {
                    leaves.extend(build_word_leaves(&text, fs, bold, color, fonts, taffy_tree))
                }
                RunSeg::Nodes(nodes) => leaves.extend(nodes),
            }
        }
        if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
            direct.push(wrapper);
        }
    };

    let (font_size, _bold) = font_context(tree, id, styles);

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
    //   row. Siblings before the first float and after the zone recurse
    //   normally.
    let is_float_child = |cid: &NodeId| -> bool {
        styles
            .get(cid)
            .is_some_and(|s| s.float_side.is_some() && s.display != Some(CssDisplay::None))
    };
    if let Some(float_idx) = child_ids.iter().position(is_float_child) {
        let run_side = styles.get(&child_ids[float_idx]).and_then(|s| s.float_side);
        let clears_this = |cid: &NodeId| -> bool {
            styles.get(cid).and_then(|s| s.clear_side).is_some_and(|c| match c {
                crate::diting_css::ClearSide::Both => true,
                crate::diting_css::ClearSide::Left => run_side == Some(crate::diting_css::FloatSide::Left),
                crate::diting_css::ClearSide::Right => run_side == Some(crate::diting_css::FloatSide::Right),
            })
        };
        let is_whitespace_text =
            |cid: &NodeId| -> bool { tree.with_node(*cid, |n| !n.is_element() && n.text_content_of_text_node().map_or(false, |t| t.trim().is_empty())).unwrap_or(false) };
        // An empty bridge sibling (upstream is_empty_bridge): whitespace
        // text OR an element with no authored size/margin/padding/border
        // and no text content — the legacy compatibility boxes real pages
        // park between the two header floats.
        let is_empty_bridge = |cid: &NodeId| -> bool {
            if is_whitespace_text(cid) {
                return true;
            }
            let has_text = tree
                .with_node(*cid, |n| {
                    n.text_content_of_text_node().map(|t| !t.trim().is_empty()).unwrap_or(false)
                        || n.first_child.is_some()
                })
                .unwrap_or(true);
            if has_text {
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

        // Extend the run across consecutive same-side floats, skipping
        // formatting whitespace between them.
        let mut run_end = float_idx + 1;
        while run_end < child_ids.len() {
            let cid = child_ids[run_end];
            if is_float_child(&cid)
                && styles.get(&cid).and_then(|s| s.float_side) == run_side
            {
                run_end += 1;
            } else if is_whitespace_text(&cid) {
                run_end += 1;
            } else {
                break;
            }
        }
        let run_len = (float_idx..run_end).filter(|&i| is_float_child(&child_ids[i])).count();

        // --- 8e: the right-float navigation bar --------------------------
        // A container of inline-ish flow content plus >=2 RIGHT floats and
        // no left float: right floats place from the inline-end inward, so
        // their visual order is the REVERSE of source order, while ordinary
        // content fills from the start of the same band. Serializing each
        // float into its own row reverses the two groups and shrink-wraps
        // the bar. Reified as [flow items | reversed right-float group],
        // with an anonymous wrapping row at definite width (upstream
        // strategy 4).
        let all_right = run_side == Some(crate::diting_css::FloatSide::Right)
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
                    atomic_container,
                    font_size,
                    &mut row_children,
                );
            }
            // The right-float group in REVERSED source order (CSS places
            // right floats inline-end first).
            let mut right_children: Vec<taffy::tree::NodeId> = Vec::new();
            for cid in right_floats.iter().rev() {
                if let Some(f) = build_element(tree, *cid, styles, images, fonts, taffy_tree, node_map) {
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
            let taffy_style = to_taffy_style(&style);
            let node = if direct.is_empty() {
                taffy_tree.new_leaf(taffy_style).ok()?
            } else {
                taffy_tree.new_with_children(taffy_style, &direct).ok()?
            };
            node_map.insert(node, id);
            return Some(node);
        }
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
                        build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map)
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
                build_element(tree, child_ids[float_idx], styles, images, fonts, taffy_tree, node_map)
            {
                pair_children.push(f);
            }
            if let Some(o) =
                build_element(tree, child_ids[opp_idx], styles, images, fonts, taffy_tree, node_map)
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
            // Pre-float and post-pair siblings take normal flow; another
            // float after the pair recurses into this branch again.
            for child in child_ids[..float_idx]
                .iter()
                .chain(child_ids[opp_idx + 1..].iter())
            {
                build_normal_sibling(
                    *child,
                    tree,
                    styles,
                    images,
                    fonts,
                    taffy_tree,
                    node_map,
                    atomic_container,
                    font_size,
                    &mut direct,
                );
            }
            flush_run(&mut run, &mut direct, taffy_tree);
            let taffy_style = to_taffy_style(&style);
            let node = if direct.is_empty() {
                taffy_tree.new_leaf(taffy_style).ok()?
            } else {
                taffy_tree.new_with_children(taffy_style, &direct).ok()?
            };
            node_map.insert(node, id);
            return Some(node);
        }

        if run_len >= 2 {
            // --- 8c: the wrapping float-grid row -------------------------
            let mut row_children: Vec<taffy::tree::NodeId> = Vec::new();
            for i in float_idx..run_end {
                if !is_float_child(&child_ids[i]) {
                    continue;
                }
                if let Some(f) = build_element(tree, child_ids[i], styles, images, fonts, taffy_tree, node_map) {
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
            // Everything after the run recurses through this same branch:
            // another float starts its own zone/run, cleared or plain
            // siblings take normal flow.
            for child in child_ids[..float_idx].iter().chain(child_ids[run_end..].iter()) {
                build_normal_sibling(
                    *child,
                    tree,
                    styles,
                    images,
                    fonts,
                    taffy_tree,
                    node_map,
                    atomic_container,
                    font_size,
                    &mut direct,
                );
            }
            let taffy_style = to_taffy_style(&style);
            let node = if direct.is_empty() {
                taffy_tree.new_leaf(taffy_style).ok()?
            } else {
                taffy_tree.new_with_children(taffy_style, &direct).ok()?
            };
            node_map.insert(node, id);
            return Some(node);
        }

        // --- 8b: single float + flow column ------------------------------
        // Zone end: the first sibling that clears this float's side (the
        // clearfix idiom), or the next float, or the point where the flow
        // siblings have already filled an ESTIMATE of the float's height —
        // real float reflow ends when normal flow passes the float's bottom
        // edge (8g; upstream estimate_float_height). Without the budget a
        // short float drags every following section into the narrow column.
        // The estimate is deliberately rough: explicit px heights when
        // present, else one line per structural row (p/li/tr/hN) plus a
        // character-count text estimate.
        let float_height_budget = {
            let fs = styles.get(&child_ids[float_idx]).and_then(|s| s.font_size).unwrap_or(16.0);
            let mut est: f32 = styles
                .get(&child_ids[float_idx])
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
            for child in tree.children(child_ids[float_idx]) {
                estimate_into(tree, child, styles, &mut est);
            }
            // The 200px default only covers the no-information case (an
            // empty/icon float); an explicit or content-derived estimate is
            // almost always better than a generous floor.
            if est <= 0.0 { est = 200.0; }
            est
        };
        let mut zone_end = child_ids.len();
        {
            let mut flow_estimate = 0.0f32;
            const ASSUMED_FLOW_WIDTH: f32 = 500.0;
            for (i, cid) in child_ids.iter().enumerate().skip(float_idx + 1) {
                if clears_this(cid) || is_float_child(cid) {
                    zone_end = i;
                    break;
                }
                // Rough per-sibling height contribution at the assumed width:
                // explicit px height wins, else structural-row lines plus a
                // character-count wrap estimate (same heuristic as the float
                // side).
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
                if flow_estimate >= float_height_budget {
                    zone_end = i + 1;
                    break;
                }
            }
        }
        // Build the float itself (blockified into the row's first item).
        let float_dom = child_ids[float_idx];
        let float_taffy =
            build_element(tree, float_dom, styles, images, fonts, taffy_tree, node_map);
        // The flow column: an ANONYMOUS block wrapper around every in-zone
        // sibling built normally inside it. Not in node_map — it has no DOM
        // identity, so collect skips it and paints walk straight through to
        // the real children.
        let flow_dom = &child_ids[float_idx + 1..zone_end];
        let mut flow_children: Vec<taffy::tree::NodeId> = Vec::new();
        let mut run: Vec<RunSeg> = Vec::new();
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
                    if inline_level && !out_of_flow {
                        run.push(RunSeg::Nodes(vec![leaf]));
                    } else {
                        flush_run(&mut run, &mut flow_children, taffy_tree);
                        flow_children.push(leaf);
                    }
                }
                continue;
            }
            if is_text {
                let text = tree.with_node(child, |n| n.text_content_of_text_node().unwrap_or("").to_string()).unwrap_or_default();
                let (fs, b) = font_context(tree, child, styles);
                let fs = if styles.get(&child).is_some() { fs } else { font_size };
                let col = color_context(tree, child, styles);
                run.push(RunSeg::Text(text, fs, b, col));
            } else if inline_level && !out_of_flow {
                let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map);
                if let Some(sub) = sub {
                    let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                    run.push(RunSeg::Nodes(sub_children));
                    node_map.remove(&sub);
                    let _ = taffy_tree.remove(sub);
                }
            } else {
                flush_run(&mut run, &mut flow_children, taffy_tree);
                if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map) {
                    flow_children.push(node);
                }
            }
        }
        flush_run(&mut run, &mut flow_children, taffy_tree);
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
            size: Size { width: percent(1.0), height: auto() },
            ..Default::default()
        };
        if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
            direct.push(row);
        }
        // Siblings before the float keep their normal-flow positions;
        // zone-external siblings (clear, later floats) recurse normally.
        for child in child_ids[..float_idx]
            .iter()
            .chain(child_ids[zone_end..].iter())
        {
            build_normal_sibling(
                *child,
                tree,
                styles,
                images,
                fonts,
                taffy_tree,
                node_map,
                atomic_container,
                font_size,
                &mut direct,
            );
        }
        flush_run(&mut run, &mut direct, taffy_tree);

        let taffy_style = to_taffy_style(&style);
        let node = if direct.is_empty() {
            taffy_tree.new_leaf(taffy_style).ok()?
        } else {
            taffy_tree.new_with_children(taffy_style, &direct).ok()?
        };
        node_map.insert(node, id);
        return Some(node);
    }

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
                if inline_level && !atomic_container && !out_of_flow {
                    run.push(RunSeg::Nodes(vec![leaf]));
                } else {
                    flush_run(&mut run, &mut direct, taffy_tree);
                    direct.push(leaf);
                }
            }
            continue;
        }
        if is_text {
            let text = tree.with_node(child, |n| n.text_content_of_text_node().unwrap_or("").to_string()).unwrap_or_default();
            let (fs, b) = font_context(tree, child, styles);
            let fs = if styles.get(&child).is_some() { fs } else { font_size };
            // First segment's node donates the whole run's color — the same
            // first-segment approximation fs/bold already use.
            let col = color_context(tree, child, styles);
            run.push(RunSeg::Text(text, fs, b, col));
        } else if inline_level && !atomic_container && !out_of_flow {
            // A plain inline wrapper flattens into the enclosing run (upstream
            // is_flattenable_inline): the words wrap at the real block level.
            let sub = build_element(tree, child, styles, images, fonts, taffy_tree, node_map);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                run.push(RunSeg::Nodes(sub_children));
                node_map.remove(&sub);
                let _ = taffy_tree.remove(sub);
            }
        } else {
            flush_run(&mut run, &mut direct, taffy_tree);
            if let Some(node) = build_element(tree, child, styles, images, fonts, taffy_tree, node_map) {
                direct.push(node);
            }
        }
    }
    flush_run(&mut run, &mut direct, taffy_tree);

    let taffy_style = to_taffy_style(&style);
    let node = if direct.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &direct).ok()?
    };
    node_map.insert(node, id);
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
    Bg { dom_id: NodeId, rect: Rect, color: [u8; 4], radius: f32 },
    /// Per-corner radii variant of `Bg` (batch 7c): CSS corner order
    /// (TL TR BR BL), each (rx, ry) already resolved to px.
    BgCorner { dom_id: NodeId, rect: Rect, color: [u8; 4], radii: [(f32, f32); 4] },
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
        /// (text, font_size, bold, color) of the alt run; None without an
        /// alt attribute (present-but-empty paints box-only, like alt="").
        alt: Option<(String, f32, bool, [u8; 4])>,
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
        dom_id: NodeId,
        rect: Rect,
        widths: [f32; 4],
        color: [u8; 4],
    },
    Text {
        text: String,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
        /// Leaf origin (line-box top-left), page px.
        x: f32,
        y: f32,
        /// The wrap width the containing block offered at measure time.
        wrap_at: f32,
    },
}

/// Lay a DOM tree out at a fixed viewport width and return each element's
/// absolute border-box rect. Elements are keyed by diting_dom NodeId; the
/// root's containing block is the viewport.
pub fn layout_dom(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
) -> HashMap<NodeId, Rect> {
    layout_dom_with_paint(tree, styles, fonts, viewport_width).0
}

/// [`layout_dom`] plus the paint item list in document order (an element's
/// `Bg` precedes the items of its subtree, so a solid background lands
/// under its descendants' ink).
pub fn layout_dom_with_paint(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    fonts: &FontBook,
    viewport_width: f32,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>) {
    layout_dom_with_paint_and_images(tree, styles, fonts, viewport_width, None)
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
    network_bytes: Option<&HashMap<String, Vec<u8>>>,
) -> (HashMap<NodeId, Rect>, Vec<PaintItem>) {
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
    ) {
        let is_img = tree
            .with_node(id, |n| n.as_element().map(|e| e.local.to_string() == "img"))
            .flatten()
            .unwrap_or(false);
        if is_img {
            let src = tree
                .with_node(id, |n| n.get_attribute("src").map(|v| v.to_string()))
                .flatten();
            if let Some(img) = src.as_deref().and_then(|s| cache.resolve(s)) {
                images.insert(id, (*img).clone());
            }
        }
        for child in tree.children(id) {
            scan_images(tree, child, cache, images);
        }
    }
    if let Some(root_id) = &root {
        scan_images(tree, *root_id, &cache, &mut images);
    }

    let mut rects = HashMap::new();
    let mut items: Vec<PaintItem> = Vec::new();
    let Some(root_id) = root else { return (rects, items) };
    let Some(root_node) =
        build_element(tree, root_id, styles, &images, fonts, &mut taffy_tree, &mut node_map)
    else {
        return (rects, items);
    };

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
                let target = target_dom.and_then(|d| dom_of.get(&d)).copied().unwrap_or(root_node);
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
    let available = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::MaxContent,
    };
    // Measured-leaf dispatch (batch 3a): pure-text runs carry a TextLeaf
    // context and measure through the FontBook; every other leaf must fall
    // through to taffy's own style-based sizing — the closure fires for ALL
    // childless nodes, so returning HIDDEN here would zero plain leaves
    // (that's exactly what stock compute_layout does below via the same fn).
    let measured = taffy_tree.compute_layout_with_measure(root_node, available, |inputs, _id, ctx, style| {
        match ctx {
            Some(TextLeaf::Run { text, font_size, bold, .. }) => {
                measure_text_leaf(text, *font_size, *bold, fonts, &inputs)
            }
            // Word leaves keep their style-driven sizing (batch 4d only
            // added paint context — zero layout change).
            Some(TextLeaf::Word { .. }) | None => {
                taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO)
            }
        }
    });
    if measured.is_err() {
        return (rects, items);
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
        abs_rects(&taffy_tree, root_node, (0.0, 0.0), &mut rects_now);

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
                let mut cur = parent_dom;
                loop {
                    let Some(grand) = tree.with_node(cur, |n| n.parent).flatten() else { break };
                    // cur's taffy box shares the float's taffy parent — both
                    // sit inside the same synthetic 8b/8c/8d row, whose
                    // contents the float already excludes. Stop climbing.
                    if let Some(&cnid) = dom_of.get(&cur) {
                        if taffy_tree.parent(cnid) == float_taffy_parent {
                            break;
                        }
                    }
                    let siblings = tree.children(cur);
                    for sib in siblings {
                        if sib == *float_dom {
                            continue;
                        }
                        // Ancestors of the float (its parent chain up to the
                        // BFC) are not "later content" — narrowing the body
                        // itself would shrink the whole zone row.
                        let mut is_ancestor = false;
                        {
                            let mut a = Some(*float_dom);
                            while let Some(x) = a {
                                if x == sib {
                                    is_ancestor = true;
                                    break;
                                }
                                a = tree.with_node(x, |n| n.parent).flatten();
                            }
                        }
                        if is_ancestor {
                            continue;
                        }
                        let Some(&snid) = dom_of.get(&sib) else { continue };
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
                    cur = grand;
                }
            }
            if changed {
                let re = taffy_tree.compute_layout_with_measure(
                    root_node,
                    available,
                    |inputs, _id, ctx, style| match ctx {
                        Some(TextLeaf::Run { text, font_size, bold, .. }) => {
                            measure_text_leaf(text, *font_size, *bold, fonts, &inputs)
                        }
                        _ => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
                    },
                );
                if re.is_err() {
                    return (rects, items);
                }
            }
        }
    }
    // Both trees round to the pixel grid inside compute_layout (taffy's
    // use_rounding defaults on; blitz rounds via the same path), so the
    // rect comparisons assume integer edges on both sides.

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
        rects: &mut HashMap<NodeId, Rect>,
        items: &mut Vec<PaintItem>,
        node: taffy::tree::NodeId,
        offset: (f32, f32),
        viewport_width: f32,
    ) {
        let Ok(layout) = taffy_tree.layout(node) else { return };
        let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
        let mut clips = false;
        if let Some(dom_id) = node_map.get(&node) {
            let rect = Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height };
            rects.insert(*dom_id, rect);
            if let Some(c) = styles.get(dom_id).and_then(|s| s.background_color) {
                if c.3 != 0 && rect.width > 0.0 && rect.height > 0.0 {
                    let color = [c.0, c.1, c.2, c.3];
                    // Radii resolve per-axis: rx against the box width,
                    // ry against its height (the elliptical form).
                    let res = |l: &crate::diting_css::Length, basis: f32| match l {
                        crate::diting_css::Length::Px(v) => *v,
                        crate::diting_css::Length::Percent(p) => p * basis / 100.0,
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
                                    dom_id: *dom_id,
                                    rect,
                                    color,
                                    radius: radii[0].0,
                                });
                            } else {
                                items.push(PaintItem::BgCorner {
                                    dom_id: *dom_id,
                                    rect,
                                    color,
                                    radii: [
                                        radii[0], radii[1], radii[2], radii[3],
                                    ],
                                });
                            }
                        }
                        None => items.push(PaintItem::Bg {
                            dom_id: *dom_id,
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
                    items.push(PaintItem::Border { dom_id: *dom_id, rect, widths, color });
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
                                let (font_size, bold) = font_context(tree, *dom_id, styles);
                                (text, font_size, bold, color_context(tree, *dom_id, styles))
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
        if let Some(TextLeaf::Run { text, font_size, bold, color }) = taffy_tree.get_node_context(node) {
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
                x: abs.0,
                y: abs.1,
                wrap_at,
            });
        }
        if let Some(TextLeaf::Word { text, font_size, bold, color }) = taffy_tree.get_node_context(node) {
            // A word leaf paints at its own box — the enclosing flex row
            // already did the line breaking (leaf-level wrap). Single-token
            // text can never break, so wrap_at just equals the leaf width.
            items.push(PaintItem::Text {
                text: text.clone(),
                font_size: *font_size,
                bold: *bold,
                color: *color,
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
                collect(tree, taffy_tree, node_map, styles, images, rects, items, children[i], abs, viewport_width);
            }
        }
        if clips {
            items.push(PaintItem::PopClip);
        }
    }
    collect(
        tree,
        &taffy_tree,
        &node_map,
        styles,
        &images,
        &mut rects,
        &mut items,
        root_node,
        (0.0, 0.0),
        viewport_width,
    );
    (rects, items)
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
            .filter_map(|rule| {
                let hits = tree.query_selector_all_from(tree.document(), &rule.selector).ok()?;
                if !hits.contains(&nid) {
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
            visit(tree, rules, child, Some(&cs), child_root_fs, out);
        }
        out.insert(nid, cs);
    }
    let mut out = HashMap::new();
    for child in tree.children(tree.document()) {
        visit(
            tree,
            rules,
            child,
            None,
            crate::diting_css::DEFAULT_ROOT_FONT_SIZE,
            &mut out,
        );
    }
    out
}

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
}

/// Batch 2b: the bridge against blitz's real layout. Same taffy on both
/// sides, so every difference the assertions catch is bridge modeling, not
/// layout math. Authored-geometry fixtures only for cross-engine asserts
/// (text metrics are heuristic here, glyph-measured in blitz — those are
/// locked as our-side structural tests instead).
#[cfg(test)]
mod bridge_cross_check {
    use super::*;
    use crate::diting_css::{self, ParsedRule};
    use crate::screenshot::element_rect;

    const VW: f32 = 800.0;
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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);
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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
    /// text + flattened span children wrap together, and the span itself
    /// owns no box.
    #[test]
    fn inline_run_is_one_wrapper() {
        let html = r#"<body><p id="p">alpha <span id="s">beta gamma</span> delta</p></body>"#;
        let sheet = "body { margin: 0; }";
        let (_doc, tree, _styles, rects) = both_engines(html, sheet);
        let p = rects[&tree.query_selector("#p").unwrap().unwrap()];
        let s = tree.query_selector("#s").unwrap().unwrap();
        assert!(!rects.contains_key(&s), "flattened span owns no box");
        // One line of 16px text = 19.2 → 19 after taffy's rounding.
        assert!((p.height - 19.2).abs() < EPS as f32, "one line: {}", p.height);
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

    /// Absolute stretch between opposing insets.
    #[test]
    fn absolute_stretch_matches_blitz() {
        let html = r#"<body><div id="gp"><div id="st"></div></div></body>"#;
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
            let raster = fixture_fonts().rasterize("你好gapa渲染", fs, false, [0, 0, 0, 255]);
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

        let r = fonts.rasterize_wrapped(text, fs, false, [0, 0, 0, 255], wrap_at);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
            let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);

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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fonts, VW);
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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects2 = layout_dom(&tree2, &styles2, &fixture_fonts(), VW);
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
        let (rects, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let rects = layout_dom(&tree, &styles, &fixture_fonts(), VW);

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
        let (_r, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW);
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
            let (_rects, items) = layout_dom_with_paint(&tree, &styles, &fixture_fonts(), VW);
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
            layout_dom_with_paint_and_images(&tree, &styles, &fixture_fonts(), VW, Some(&net));
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
            layout_dom_with_paint_and_images(&tree, &styles, &fixture_fonts(), VW, Some(&net));
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

}
