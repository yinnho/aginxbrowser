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
    FlexWrapMode, GridTrack, JustifyMode, TextAlign,
};
use crate::diting_dom::tree::{DomTree, NodeId};

/// Absolute (page-relative) border-box rect of a DOM element after layout.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Deterministic text metrics for word leaves — upstream's no-paint fallback
/// model (0.55em average per ASCII char, bold ×1.08), extended with a 1.0em
/// CJK class because our pages are CJK-heavy and CJK glyph advance is one em.
pub fn text_width(text: &str, font_size: f32, bold: bool) -> f32 {
    let em: f32 = text
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if is_cjk(c) { 1.0 } else { 0.55 })
        .sum();
    let w = em * font_size;
    if bold { w * 1.08 } else { w }
}

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

/// Approximate used line height for text leaves (upstream resolves a full
/// LineHeight model; the slice takes the common `normal` ≈ 1.2 fallback).
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
    let side = |v: Option<u32>| v.map(|px| LengthPercentage::length(px as f32)).unwrap_or_else(|| LengthPercentage::length(0.0));
    // taffy::geometry::Rect spelled in full — this module's own Rect shadows
    // the prelude name.
    s.margin = taffy::geometry::Rect {
        top: LengthPercentageAuto::length(side_px(style.margin.top)),
        right: LengthPercentageAuto::length(side_px(style.margin.right)),
        bottom: LengthPercentageAuto::length(side_px(style.margin.bottom)),
        left: LengthPercentageAuto::length(side_px(style.margin.left)),
    };
    s.padding = taffy::geometry::Rect {
        top: side(style.padding.top),
        right: side(style.padding.right),
        bottom: side(style.padding.bottom),
        left: side(style.padding.left),
    };
    // CSS's initial box-sizing is content-box while taffy sizes are
    // border-box; the subset has no authored box-sizing yet, so map authored
    // sizes over by the padding (border widths aren't modeled at all).
    s.size = Size {
        width: style
            .width
            .map(|w| Dimension::length(w + side_px(style.padding.left) + side_px(style.padding.right)))
            .unwrap_or_else(auto),
        height: style
            .height
            .map(|h| Dimension::length(h + side_px(style.padding.top) + side_px(style.padding.bottom)))
            .unwrap_or_else(auto),
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

fn side_px(v: Option<u32>) -> f32 {
    v.map(|px| px as f32).unwrap_or(0.0)
}

/// Effective font context for a text leaf: nearest ancestor's declared
/// font-size / weight (defaults 16px / 400).
fn font_context(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, ComputedStyle>) -> (f32, bool) {
    let mut font_size = 16.0;
    let mut bold = false;
    let mut current = Some(id);
    while let Some(nid) = current {
        if let Some(style) = styles.get(&nid) {
            if let Some(fs) = style.font_size {
                font_size = fs;
            }
            if let Some(fw) = style.font_weight {
                bold = fw >= 600;
            }
        }
        current = tree.with_node(nid, |n| n.parent).flatten();
    }
    (font_size, bold)
}

/// Split text into layout tokens. Whitespace runs collapse to a single space
/// token (CSS text processing) that keeps its width but contributes no
/// height; CJK chars break per-glyph — UAX#14 allows a break after every
/// ideograph, and without this a CJK paragraph would be one unbreakable
/// "word".
fn tokenize(text: &str) -> Vec<String> {
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

/// One taffy leaf per word (upstream's per-word leaf model). Fixed intrinsic
/// size from the deterministic metrics; wrapping happens in the enclosing
/// flex-wrap run container.
fn build_word_leaves(
    text: &str,
    font_size: f32,
    bold: bool,
    taffy_tree: &mut TaffyTree<()>,
) -> Vec<taffy::tree::NodeId> {
    tokenize(text)
        .into_iter()
        .filter_map(|token| {
            let width = text_width(&token, font_size, bold);
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
            taffy_tree.new_leaf(style).ok()
        })
        .collect()
}

/// The inline-formatting-context stand-in around a run of inline content:
/// a wrapping flex row (upstream run_wrapper_style / outer_style model).
fn run_wrapper_style() -> Style {
    Style {
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

/// Build the taffy leaf for a replaced element. Natural size comes from the
/// HTML width/height attributes (upstream's no-network fallback); the CSS
/// authored size overrides per-axis, and the missing axis derives from the
/// natural ratio. No attributes and no CSS → the CSS default replaced size
/// 300×150 (ratio 2:1).
fn build_replaced_leaf(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    taffy_tree: &mut TaffyTree<()>,
    node_map: &mut HashMap<taffy::tree::NodeId, NodeId>,
) -> Option<taffy::tree::NodeId> {
    let style = styles.get(&id).cloned().unwrap_or_default();
    let attr = |name: &str| {
        tree.with_node(id, |n| n.get_attribute(name).map(|v| v.to_string()))
            .flatten()
            .and_then(|v| v.parse::<f32>().ok())
    };
    let (aw, ah) = (attr("width"), attr("height"));
    let (nat_w, nat_h) = match (aw, ah) {
        (Some(w), Some(h)) if h > 0.0 => (w, h),
        (Some(w), None) => (w, w / 2.0),
        (None, Some(h)) => (h * 2.0, h),
        _ => (300.0, 150.0),
    };

    let mut s = Style::default();
    s.item_is_replaced = true;
    s.aspect_ratio = Some(nat_w / nat_h);
    // CSS width/height win per axis; missing axis derives from the ratio.
    s.size = Size {
        width: style.width.map(|w| Dimension::length(w)).unwrap_or_else(|| Dimension::length(nat_w)),
        height: style.height.map(|h| Dimension::length(h)).unwrap_or_else(|| Dimension::length(nat_h)),
    };

    let node = taffy_tree.new_leaf(s).ok()?;
    node_map.insert(node, id);
    Some(node)
}

/// Build the taffy subtree for one element. Returns None for display:none
/// (subtree skipped) and for the document node's non-element parts.
fn build_element(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, ComputedStyle>,
    taffy_tree: &mut TaffyTree<()>,
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
        return build_replaced_leaf(tree, id, styles, taffy_tree, node_map);
    }

    let child_ids: Vec<NodeId> = tree.children(id);

    // In a flex/grid container every element child is blockified into its own
    // item (CSS flex-item blockification); runs only form in block/inline
    // formatting contexts.
    let atomic_container = matches!(style.display, Some(CssDisplay::Flex) | Some(CssDisplay::Grid));

    // Partition children into block-level elements (direct taffy children)
    // and inline runs (text + inline elements, flattened into one wrapping
    // flex row — upstream's anonymous-run model).
    let mut direct: Vec<taffy::tree::NodeId> = Vec::new();
    let mut run: Vec<taffy::tree::NodeId> = Vec::new();
    let mut flush_run = |run: &mut Vec<taffy::tree::NodeId>, direct: &mut Vec<taffy::tree::NodeId>, taffy_tree: &mut TaffyTree<()>| {
        if run.is_empty() {
            return;
        }
        let leaves = std::mem::take(run);
        if let Ok(wrapper) = taffy_tree.new_with_children(run_wrapper_style(), &leaves) {
            direct.push(wrapper);
        }
    };

    let (font_size, _bold) = font_context(tree, id, styles);
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
        if !is_text && is_replaced_tag(&child_tag) {
            // Replaced elements are atomic: an inline-level box inside a run
            // (like a fat word), a direct item inside flex/grid or when the
            // UA/author made it block-level (our ua_display keeps img block).
            let leaf = build_replaced_leaf(tree, child, styles, taffy_tree, node_map);
            if let Some(leaf) = leaf {
                if inline_level && !atomic_container {
                    run.push(leaf);
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
            run.extend(build_word_leaves(&text, fs, b, taffy_tree));
        } else if inline_level && !atomic_container {
            // A plain inline wrapper flattens into the enclosing run (upstream
            // is_flattenable_inline): the words wrap at the real block level.
            let sub = build_element(tree, child, styles, taffy_tree, node_map);
            if let Some(sub) = sub {
                let sub_children: Vec<_> = taffy_tree.children(sub).unwrap_or_default().to_vec();
                run.extend(sub_children);
                let _ = taffy_tree.remove(sub);
            }
        } else {
            flush_run(&mut run, &mut direct, taffy_tree);
            if let Some(node) = build_element(tree, child, styles, taffy_tree, node_map) {
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

/// Lay a DOM tree out at a fixed viewport width and return each element's
/// absolute border-box rect. Elements are keyed by diting_dom NodeId; the
/// root's containing block is the viewport.
pub fn layout_dom(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    viewport_width: f32,
) -> HashMap<NodeId, Rect> {
    let mut taffy_tree = TaffyTree::new();
    let mut node_map: HashMap<taffy::tree::NodeId, NodeId> = HashMap::new();

    // The document node is not an element; lay out from the first element
    // descendant (the <html> root), like upstream.
    let root = tree
        .children(tree.document())
        .into_iter()
        .find(|id| tree.with_node(*id, |n| n.is_element()).unwrap_or(false));

    let mut rects = HashMap::new();
    let Some(root_id) = root else { return rects };
    let Some(root_node) = build_element(tree, root_id, styles, &mut taffy_tree, &mut node_map) else {
        return rects;
    };

    let available = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::MaxContent,
    };
    if taffy_tree.compute_layout(root_node, available).is_err() {
        return rects;
    }

    // Accumulate locations down the taffy tree: child location already
    // includes the parent's border+padding offset, so a plain sum is the
    // absolute border-box origin (same accumulation blitz-paint performs).
    fn collect(
        taffy_tree: &TaffyTree<()>,
        node_map: &HashMap<taffy::tree::NodeId, NodeId>,
        rects: &mut HashMap<NodeId, Rect>,
        node: taffy::tree::NodeId,
        offset: (f32, f32),
    ) {
        let Ok(layout) = taffy_tree.layout(node) else { return };
        let abs = (offset.0 + layout.location.x, offset.1 + layout.location.y);
        if let Some(dom_id) = node_map.get(&node) {
            rects.insert(
                *dom_id,
                Rect { x: abs.0, y: abs.1, width: layout.size.width, height: layout.size.height },
            );
        }
        for child in taffy_tree.children(node).unwrap_or_default().iter().copied() {
            collect(taffy_tree, node_map, rects, child, abs);
        }
    }
    collect(&taffy_tree, &node_map, &mut rects, root_node, (0.0, 0.0));
    rects
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
    fn our_styles(tree: &DomTree, rules: &[ParsedRule]) -> HashMap<NodeId, ComputedStyle> {
        fn visit(
            tree: &DomTree,
            rules: &[ParsedRule],
            nid: NodeId,
            parent: Option<&ComputedStyle>,
            out: &mut HashMap<NodeId, ComputedStyle>,
        ) {
            let Some(tag) = tree
                .with_node(nid, |n| n.as_element().map(|e| e.local.to_string()))
                .flatten()
            else {
                return;
            };
            let matched: Vec<(&ParsedRule, u32)> = rules
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
            let cs = diting_css::cascade_element(&tag, tree, nid, &matched, parent, inline.as_deref());
            for child in tree.children(nid) {
                visit(tree, rules, child, Some(&cs), out);
            }
            out.insert(nid, cs);
        }
        let mut out = HashMap::new();
        for child in tree.children(tree.document()) {
            visit(tree, rules, child, None, &mut out);
        }
        out
    }

    /// Blitz side of the dual run: parse + style + layout at the test
    /// viewport, then hand back the base document for element_rect.
    fn blitz_doc(html: &str, stylesheet: &str) -> blitz_dom::BaseDocument {
        use blitz_dom::{DocumentConfig, util::Color};
        use blitz_traits::shell::{ColorScheme, Viewport};

        let css_doc = format!("<style>{stylesheet}</style>{html}");
        let mut doc = blitz_html::HtmlDocument::from_html(
            &css_doc,
            DocumentConfig {
                base_url: Some("https://example.com/".to_string()),
                net_provider: None,
                viewport: Some(Viewport::new(VW as u32, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            },
        );
        for _ in 0..4 {
            doc.resolve(0.0);
        }
        let _ = Color::WHITE;
        doc.into_inner()
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
        let rects = layout_dom(&tree, &styles, VW);
        (doc, tree, styles, rects)
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
        // "hi" = 2 × 0.55 × 16 = 17.6 → 18 after taffy's integer rounding;
        // centered in 200 → x = (200 − 18) / 2.
        assert!((mi.width - 17.6).abs() < EPS as f32, "run width: {}", mi.width);
        assert!((mi.x - (200.0 - 18.0) / 2.0).abs() < EPS as f32, "centered x: {}", mi.x);
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
}
