//! Our own layout tree: node storage plus the dispatch that picks a taffy
//! algorithm per node.
//!
//! It exposes the same methods `diting_layout` used on `taffy::TaffyTree`
//! (so it is aliased in as `TaffyTree` there), and runs the same taffy
//! block / flex / grid algorithms. What it owns is the dispatch, which is the
//! seam float wrapping needs: the stock tree never hands a text leaf the
//! block formatting context it sits in, so a leaf cannot ask where the
//! floats are (#299 / #300).
//!
//! Storage is what keeps a big page affordable (#307). A full node costs
//! 1.3 KB (a taffy `Style`, a layout `Cache`, two `Layout`s), and a mixed
//! paragraph makes one leaf per word - 50k nodes, 67 MB, on a Wikipedia
//! article. A word leaf has a fixed size and nothing else to style or cache,
//! so it is a *word node*: size, context and layout results only, laid out as
//! the fixed-size flex item it is. Both kinds live in chunked arrays (no
//! single reallocation of tens of MB).

use super::inline_flow;
use std::collections::HashSet;
use taffy::style::{
    AlignSelf, BoxGenerationMode, BoxSizing, ContainingBlockClaims, Contain, Dimension, Direction,
    LengthPercentageAuto, Overflow, Position,
};
use taffy::{
    compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout, compute_hidden_layout,
    compute_leaf_layout, compute_oof_layout, compute_root_layout, round_layout, BlockContext, Cache, CacheTree,
    CoreStyle, DetailedGridInfo, DetailedLayoutInfo, Display, FlexboxItemStyle, Layout, LayoutBlockContainer,
    LayoutContainingBlock, LayoutFlexboxContainer, LayoutGridContainer, LayoutInput, LayoutOutput,
    LayoutPartialTree, NodeId, Point, Rect, RoundTree, RunMode, Size, Style, TaffyError, TraversePartialTree,
    TraverseTree,
};
use taffy::{AvailableSpace, FlexWrap};

type Res<T> = Result<T, TaffyError>;

/// Word nodes carry this bit in their id.
const WORD: usize = 1 << 62;
const CHUNK: usize = 1024;

/// Append-only storage in fixed chunks: growing never moves what is stored.
struct Chunked<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T> Chunked<T> {
    fn new() -> Self {
        Chunked { chunks: Vec::new(), len: 0 }
    }
    fn push(&mut self, t: T) -> usize {
        if self.chunks.last().is_none_or(|c| c.len() == CHUNK) {
            self.chunks.push(Vec::with_capacity(CHUNK));
        }
        self.chunks.last_mut().expect("a chunk").push(t);
        self.len += 1;
        self.len - 1
    }
    fn get(&self, i: usize) -> Option<&T> {
        self.chunks.get(i / CHUNK)?.get(i % CHUNK)
    }
    fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        self.chunks.get_mut(i / CHUNK)?.get_mut(i % CHUNK)
    }
}

struct Node<C> {
    style: Style,
    ctx: Option<C>,
    children: Vec<NodeId>,
    parent: Option<NodeId>,
    cache: Cache,
    /// Always unrounded; rounding reads this so it never rounds twice.
    unrounded: Layout,
    final_layout: Layout,
    hoisted: Vec<NodeId>,
    detailed: DetailedLayoutInfo,
}

/// A fixed-size text leaf: all a word has is its box.
struct Word<C> {
    ctx: C,
    parent: Option<NodeId>,
    size: Size<f32>,
    /// Never shrinks below its width (a nowrap run).
    pinned: bool,
    unrounded: Layout,
    final_layout: Layout,
}

impl<C> Word<C> {
    /// The style the word would have had as a full node.
    fn style(&self) -> Style {
        Style {
            size: Size { width: Dimension::length(self.size.width), height: Dimension::length(self.size.height) },
            flex_shrink: if self.pinned { 0.0 } else { 1.0 },
            ..Style::default()
        }
    }
}

pub struct FlowTree<C = ()> {
    /// Slots are never reused: a stale id must not alias a newer node.
    nodes: Chunked<Option<Node<C>>>,
    words: Chunked<Option<Word<C>>>,
    rounding: bool,
    /// Boxes whose max-content width is their min-content width (table
    /// captions: they widen a table to their longest word, never to a line).
    min_as_max: HashSet<NodeId>,
    /// What the item-style traits return for a word node (Style is not Sync,
    /// so no static).
    blank: Style,
}

impl<C> Default for FlowTree<C> {
    fn default() -> Self {
        Self::new()
    }
}

fn word_index(id: NodeId) -> Option<usize> {
    let u = usize::from(id);
    (u & WORD != 0).then_some(u & !WORD)
}

impl<C> FlowTree<C> {
    pub fn new() -> Self {
        FlowTree { nodes: Chunked::new(), words: Chunked::new(), rounding: true, min_as_max: HashSet::new(), blank: Style::default() }
    }

    /// Size `node`'s max-content like its min-content (a table caption).
    pub fn mark_min_as_max(&mut self, node: NodeId) {
        self.min_as_max.insert(node);
    }

    pub fn enable_rounding(&mut self) {
        self.rounding = true;
    }

    pub fn disable_rounding(&mut self) {
        self.rounding = false;
    }

    fn push(&mut self, style: Style, ctx: Option<C>, children: Vec<NodeId>) -> NodeId {
        let i = self.nodes.push(Some(Node {
            style,
            ctx,
            children,
            parent: None,
            cache: Cache::new(),
            unrounded: Layout::new(),
            final_layout: Layout::new(),
            hoisted: Vec::new(),
            detailed: DetailedLayoutInfo::None,
        }));
        NodeId::from(i)
    }

    /// A fixed-size text leaf (see the module doc).
    pub fn new_word(&mut self, width: f32, height: f32, ctx: C) -> NodeId {
        let i = self.words.push(Some(Word {
            ctx,
            parent: None,
            size: Size { width, height },
            pinned: false,
            unrounded: Layout::new(),
            final_layout: Layout::new(),
        }));
        NodeId::from(WORD | i)
    }

    /// Keep a leaf from shrinking below its width (a nowrap run).
    pub fn pin_shrink(&mut self, node: NodeId) {
        if let Some(i) = word_index(node) {
            if let Some(Some(w)) = self.words.get_mut(i) {
                w.pinned = true;
            }
        } else if let Ok(mut st) = self.style(node).cloned() {
            st.flex_shrink = 0.0;
            let _ = self.set_style(node, st);
        }
    }

    fn exists(&self, id: NodeId) -> bool {
        match word_index(id) {
            Some(i) => matches!(self.words.get(i), Some(Some(_))),
            None => matches!(self.nodes.get(usize::from(id)), Some(Some(_))),
        }
    }

    /// Full nodes only; word nodes have no style, children or cache.
    fn get(&self, id: NodeId) -> Res<&Node<C>> {
        if word_index(id).is_some() {
            return Err(TaffyError::InvalidInputNode(id));
        }
        self.nodes.get(usize::from(id)).and_then(|n| n.as_ref()).ok_or(TaffyError::InvalidInputNode(id))
    }

    fn get_mut(&mut self, id: NodeId) -> Res<&mut Node<C>> {
        if word_index(id).is_some() {
            return Err(TaffyError::InvalidInputNode(id));
        }
        self.nodes.get_mut(usize::from(id)).and_then(|n| n.as_mut()).ok_or(TaffyError::InvalidInputNode(id))
    }

    fn set_parent(&mut self, id: NodeId, parent: Option<NodeId>) -> Res<()> {
        match word_index(id) {
            Some(i) => match self.words.get_mut(i) {
                Some(Some(w)) => w.parent = parent,
                _ => return Err(TaffyError::InvalidInputNode(id)),
            },
            None => self.get_mut(id)?.parent = parent,
        }
        Ok(())
    }

    pub fn new_leaf(&mut self, style: Style) -> Res<NodeId> {
        Ok(self.push(style, None, Vec::new()))
    }

    pub fn new_leaf_with_context(&mut self, style: Style, ctx: C) -> Res<NodeId> {
        Ok(self.push(style, Some(ctx), Vec::new()))
    }

    pub fn new_with_children(&mut self, style: Style, children: &[NodeId]) -> Res<NodeId> {
        for c in children {
            if !self.exists(*c) {
                return Err(TaffyError::InvalidChildNode(*c));
            }
        }
        let id = self.push(style, None, children.to_vec());
        for c in children {
            self.set_parent(*c, Some(id))?;
        }
        Ok(id)
    }

    /// Drop a node; its children stay in the tree, detached.
    pub fn remove(&mut self, node: NodeId) -> Res<NodeId> {
        if !self.exists(node) {
            return Err(TaffyError::InvalidInputNode(node));
        }
        let parent = self.parent(node);
        let kids = self.children(node).unwrap_or_default();
        if let Some(p) = parent {
            if let Ok(pn) = self.get_mut(p) {
                pn.children.retain(|c| *c != node);
            }
            self.mark_dirty(p)?;
        }
        for k in kids {
            let _ = self.set_parent(k, None);
        }
        match word_index(node) {
            Some(i) => *self.words.get_mut(i).expect("exists") = None,
            None => *self.nodes.get_mut(usize::from(node)).expect("exists") = None,
        }
        Ok(node)
    }

    pub fn get_node_context(&self, node: NodeId) -> Option<&C> {
        match word_index(node) {
            Some(i) => self.words.get(i)?.as_ref().map(|w| &w.ctx),
            None => self.get(node).ok()?.ctx.as_ref(),
        }
    }

    pub fn add_child(&mut self, parent: NodeId, child: NodeId) -> Res<()> {
        if !self.exists(child) {
            return Err(TaffyError::InvalidChildNode(child));
        }
        self.get(parent).map_err(|_| TaffyError::InvalidParentNode(parent))?;
        self.set_parent(child, Some(parent))?;
        self.get_mut(parent)?.children.push(child);
        self.mark_dirty(parent)
    }

    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) -> Res<()> {
        let old = self.get(parent).map_err(|_| TaffyError::InvalidParentNode(parent))?.children.clone();
        for c in old {
            let _ = self.set_parent(c, None);
        }
        for &c in children {
            if !self.exists(c) {
                return Err(TaffyError::InvalidChildNode(c));
            }
            if let Some(prev) = self.parent(c) {
                self.remove_child(prev, c)?;
            }
            self.set_parent(c, Some(parent))?;
        }
        self.get_mut(parent)?.children = children.to_vec();
        self.mark_dirty(parent)
    }

    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Res<NodeId> {
        let pn = self.get_mut(parent).map_err(|_| TaffyError::InvalidParentNode(parent))?;
        let idx = pn.children.iter().position(|c| *c == child);
        let Some(idx) = idx else {
            return Err(TaffyError::ChildIndexOutOfBounds { parent, child_index: usize::MAX, child_count: pn.children.len() });
        };
        pn.children.remove(idx);
        let _ = self.set_parent(child, None);
        self.mark_dirty(parent)?;
        Ok(child)
    }

    pub fn parent(&self, child: NodeId) -> Option<NodeId> {
        match word_index(child) {
            Some(i) => self.words.get(i)?.as_ref()?.parent,
            None => self.get(child).ok()?.parent,
        }
    }

    pub fn children(&self, parent: NodeId) -> Res<Vec<NodeId>> {
        if word_index(parent).is_some() && self.exists(parent) {
            return Ok(Vec::new());
        }
        Ok(self.get(parent)?.children.clone())
    }

    pub fn set_style(&mut self, node: NodeId, style: Style) -> Res<()> {
        self.get_mut(node)?.style = style;
        self.mark_dirty(node)
    }

    /// The style of a full node. A word node has none (its size is its box).
    pub fn style(&self, node: NodeId) -> Res<&Style> {
        Ok(&self.get(node)?.style)
    }

    /// Layout relative to the parent (rounded when rounding is on).
    pub fn layout(&self, node: NodeId) -> Res<&Layout> {
        let rounding = self.rounding;
        match word_index(node) {
            Some(i) => match self.words.get(i) {
                Some(Some(w)) => Ok(if rounding { &w.final_layout } else { &w.unrounded }),
                _ => Err(TaffyError::InvalidInputNode(node)),
            },
            None => {
                let n = self.get(node)?;
                Ok(if rounding { &n.final_layout } else { &n.unrounded })
            }
        }
    }

    pub fn unrounded_layout(&self, node: NodeId) -> &Layout {
        match word_index(node) {
            Some(i) => &self.words.get(i).and_then(|w| w.as_ref()).expect("live node").unrounded,
            None => &self.get(node).expect("live node").unrounded,
        }
    }

    /// Clear the cache of `node` and of every ancestor above it that still
    /// holds one.
    pub fn mark_dirty(&mut self, node: NodeId) -> Res<()> {
        let mut cur = Some(node);
        while let Some(id) = cur {
            if word_index(id).is_some() {
                // Words are never cached: only what is above them can be stale.
                cur = self.parent(id);
                continue;
            }
            let n = self.get_mut(id)?;
            if matches!(n.cache.clear(), taffy::ClearState::AlreadyEmpty) {
                break;
            }
            cur = n.parent;
        }
        Ok(())
    }

    pub fn compute_layout_with_measure<M>(&mut self, node: NodeId, space: Size<AvailableSpace>, mut measure: M) -> Res<()>
    where
        M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style) -> LayoutOutput,
    {
        self.compute_layout_with_block_measure(node, space, move |inputs, id, ctx, style, _| measure(inputs, id, ctx, style))
    }

    /// Like [`Self::compute_layout_with_measure`], but a leaf laid out as an
    /// in-flow block child also receives the block formatting context it sits
    /// in, so it can ask where the floats are (`find_content_slot`).
    pub fn compute_layout_with_block_measure<M>(&mut self, node: NodeId, space: Size<AvailableSpace>, measure: M) -> Res<()>
    where
        M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
    {
        if !self.exists(node) {
            return Err(TaffyError::InvalidInputNode(node));
        }
        let rounding = self.rounding;
        let mut view = View { tree: self, measure };
        compute_root_layout(&mut view, node, space);
        if rounding {
            round_layout(&mut view, node);
        }
        Ok(())
    }
}

/// A node's style as the algorithms read it: a full node's own, or the one a
/// word node would have had.
#[allow(clippy::large_enum_variant)]
pub enum StyleRef<'a> {
    Full(&'a Style),
    Word(Style),
}

impl StyleRef<'_> {
    fn s(&self) -> &Style {
        match self {
            StyleRef::Full(s) => s,
            StyleRef::Word(s) => s,
        }
    }
}

impl CoreStyle for StyleRef<'_> {
    type CustomIdent = <Style as CoreStyle>::CustomIdent;
    fn box_generation_mode(&self) -> BoxGenerationMode {
        CoreStyle::box_generation_mode(self.s())
    }
    fn is_block(&self) -> bool {
        CoreStyle::is_block(self.s())
    }
    fn is_compressible_replaced(&self) -> bool {
        CoreStyle::is_compressible_replaced(self.s())
    }
    fn is_replaced(&self) -> bool {
        CoreStyle::is_replaced(self.s())
    }
    fn box_sizing(&self) -> BoxSizing {
        CoreStyle::box_sizing(self.s())
    }
    fn direction(&self) -> Direction {
        CoreStyle::direction(self.s())
    }
    fn overflow(&self) -> Point<Overflow> {
        CoreStyle::overflow(self.s())
    }
    fn scrollbar_width(&self) -> f32 {
        CoreStyle::scrollbar_width(self.s())
    }
    fn position(&self) -> Position {
        CoreStyle::position(self.s())
    }
    fn inset(&self) -> Rect<LengthPercentageAuto> {
        CoreStyle::inset(self.s())
    }
    fn is_containing_block(&self) -> ContainingBlockClaims {
        CoreStyle::is_containing_block(self.s())
    }
    fn size(&self) -> Size<Dimension> {
        CoreStyle::size(self.s())
    }
    fn min_size(&self) -> Size<LengthPercentageAuto> {
        CoreStyle::min_size(self.s())
    }
    fn max_size(&self) -> Size<LengthPercentageAuto> {
        CoreStyle::max_size(self.s())
    }
    fn aspect_ratio(&self) -> Option<f32> {
        CoreStyle::aspect_ratio(self.s())
    }
    fn margin(&self) -> Rect<LengthPercentageAuto> {
        CoreStyle::margin(self.s())
    }
    fn padding(&self) -> Rect<taffy::LengthPercentage> {
        CoreStyle::padding(self.s())
    }
    fn border(&self) -> Rect<taffy::LengthPercentage> {
        CoreStyle::border(self.s())
    }
    fn contain(&self) -> Contain {
        CoreStyle::contain(self.s())
    }
}

impl FlexboxItemStyle for StyleRef<'_> {
    fn flex_basis(&self) -> Dimension {
        FlexboxItemStyle::flex_basis(self.s())
    }
    fn flex_grow(&self) -> f32 {
        FlexboxItemStyle::flex_grow(self.s())
    }
    fn flex_shrink(&self) -> f32 {
        FlexboxItemStyle::flex_shrink(self.s())
    }
    fn align_self(&self) -> Option<AlignSelf> {
        FlexboxItemStyle::align_self(self.s())
    }
}

struct View<'a, C, M> {
    tree: &'a mut FlowTree<C>,
    measure: M,
}

impl<C, M> View<'_, C, M> {
    fn n(&self, id: NodeId) -> &Node<C> {
        self.tree.nodes.get(usize::from(id)).and_then(|n| n.as_ref()).expect("live full node")
    }
    fn n_mut(&mut self, id: NodeId) -> &mut Node<C> {
        self.tree.nodes.get_mut(usize::from(id)).and_then(|n| n.as_mut()).expect("live full node")
    }
    fn word(&self, id: NodeId) -> Option<&Word<C>> {
        self.tree.words.get(word_index(id)?)?.as_ref()
    }
    fn word_mut(&mut self, id: NodeId) -> Option<&mut Word<C>> {
        self.tree.words.get_mut(word_index(id)?)?.as_mut()
    }
    /// A full node's style; the default for a word (item-style traits that
    /// have no use for a word's own).
    fn style_or_default(&self, id: NodeId) -> &Style {
        if word_index(id).is_some() {
            &self.tree.blank
        } else {
            &self.n(id).style
        }
    }
}

impl<C, M> View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    fn compute(&mut self, id: NodeId, mut inputs: LayoutInput, block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput {
        // A word is a fixed-size leaf: no cache, no children, no hoisting.
        if let Some(w) = self.word(id) {
            if inputs.run_mode == RunMode::PerformHiddenLayout {
                return compute_hidden_layout(self, id);
            }
            let style = w.style();
            return compute_leaf_layout(inputs, &style, |_, _| 0.0, |_, _| Size::ZERO);
        }
        if inputs.available_space.width == AvailableSpace::MaxContent && self.tree.min_as_max.contains(&id) {
            inputs.available_space.width = AvailableSpace::MinContent;
        }
        // An ancestor is `display:none`: lay this subtree out hidden.
        if inputs.run_mode == RunMode::PerformHiddenLayout {
            return compute_hidden_layout(self, id);
        }
        compute_cached_layout(self, id, inputs, |view, id, inputs| {
            let display = view.n(id).style.display;
            let has_children = !view.n(id).children.is_empty();
            // The inline-flow wrapper (`run_wrapper_style` with float wrap on):
            // a block-level box whose items wrap like a flex row. Beside a
            // float its lines take the free slots; elsewhere it is the flex row.
            let flow = has_children && display == Display::Block && view.n(id).style.flex_wrap == FlexWrap::Wrap;
            let mut out = match (display, has_children) {
                (Display::None, _) => compute_hidden_layout(view, id),
                (Display::Block, true) if flow => block_ctx
                    .as_deref()
                    .filter(|c| c.has_active_floats(0.0))
                    .and_then(|c| inline_flow::layout(view, id, inputs, c))
                    .unwrap_or_else(|| compute_flexbox_layout(view, id, inputs)),
                (Display::Block, true) => compute_block_layout(view, id, inputs, block_ctx),
                (Display::FlowRoot, true) => compute_block_layout(view, id, inputs, None),
                (Display::Flex, true) => compute_flexbox_layout(view, id, inputs),
                (Display::Grid, true) => compute_grid_layout(view, id, inputs),
                (_, false) => {
                    let View { tree, measure } = view;
                    let n = tree.nodes.get_mut(usize::from(id)).and_then(|n| n.as_mut()).expect("live full node");
                    measure(inputs, id, n.ctx.as_mut(), &n.style, block_ctx)
                }
            };
            // Absolute / fixed boxes whose containing block is this node. Only
            // the full pass runs it: size-only passes must not write hoisted
            // layouts (a later cache hit would not rewrite them).
            if inputs.run_mode == RunMode::PerformLayout {
                compute_oof_layout(view, id, &mut out);
            }
            out
        })
    }
}

pub struct ChildIter(std::vec::IntoIter<NodeId>);
impl Iterator for ChildIter {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        self.0.next()
    }
}

impl<C, M> TraversePartialTree for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type ChildIter<'a>
        = ChildIter
    where
        Self: 'a;
    fn child_ids(&self, parent: NodeId) -> ChildIter {
        if word_index(parent).is_some() {
            return ChildIter(Vec::new().into_iter());
        }
        ChildIter(self.n(parent).children.clone().into_iter())
    }
    fn child_count(&self, parent: NodeId) -> usize {
        if word_index(parent).is_some() {
            return 0;
        }
        self.n(parent).children.len()
    }
    fn get_child_id(&self, parent: NodeId, index: usize) -> NodeId {
        self.n(parent).children[index]
    }
}

impl<C, M> TraverseTree for View<'_, C, M> where M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput {}

impl<C, M> LayoutPartialTree for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type CoreContainerStyle<'a>
        = StyleRef<'a>
    where
        Self: 'a;
    type CustomIdent = String;
    fn get_core_container_style(&self, id: NodeId) -> StyleRef<'_> {
        match self.word(id) {
            Some(w) => StyleRef::Word(w.style()),
            None => StyleRef::Full(&self.n(id).style),
        }
    }
    fn set_unrounded_layout(&mut self, id: NodeId, layout: &Layout) {
        match self.word_mut(id) {
            Some(w) => w.unrounded = *layout,
            None => self.n_mut(id).unrounded = *layout,
        }
    }
    fn resolve_calc_value(&self, _val: *const (), _basis: f32) -> f32 {
        0.0
    }
    fn compute_child_layout(&mut self, id: NodeId, inputs: LayoutInput) -> LayoutOutput {
        self.compute(id, inputs, None)
    }
}

impl<C, M> LayoutContainingBlock for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type OofItemStyle<'a>
        = &'a Style
    where
        Self: 'a;
    fn get_oof_item_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn clear_hoisted_children(&mut self, id: NodeId) {
        if word_index(id).is_none() {
            self.n_mut(id).hoisted.clear();
        }
    }
    fn add_hoisted_children(&mut self, id: NodeId, hoisted: &[NodeId]) {
        if word_index(id).is_none() {
            self.n_mut(id).hoisted.extend_from_slice(hoisted);
        }
    }
    fn get_detailed_layout_info(&self, id: NodeId) -> &DetailedLayoutInfo {
        static NONE: DetailedLayoutInfo = DetailedLayoutInfo::None;
        if word_index(id).is_some() {
            &NONE
        } else {
            &self.n(id).detailed
        }
    }
}

impl<C, M> CacheTree for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    fn cache_get(&mut self, id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        if word_index(id).is_some() {
            return None;
        }
        self.n_mut(id).cache.get(input)
    }
    fn cache_store(&mut self, id: NodeId, input: &LayoutInput, out: LayoutOutput) {
        if word_index(id).is_none() {
            self.n_mut(id).cache.store(input, out)
        }
    }
    fn cache_clear(&mut self, id: NodeId) {
        if word_index(id).is_none() {
            self.n_mut(id).cache.clear();
        }
    }
}

impl<C, M> LayoutBlockContainer for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type BlockContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = &'a Style
    where
        Self: 'a;
    fn get_block_container_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn get_block_child_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn compute_block_child_layout(
        &mut self,
        id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        self.compute(id, inputs, block_ctx)
    }
}

impl<C, M> LayoutFlexboxContainer for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type FlexboxContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = StyleRef<'a>
    where
        Self: 'a;
    fn get_flexbox_container_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn get_flexbox_child_style(&self, id: NodeId) -> StyleRef<'_> {
        match self.word(id) {
            Some(w) => StyleRef::Word(w.style()),
            None => StyleRef::Full(&self.n(id).style),
        }
    }
}

impl<C, M> LayoutGridContainer for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    type GridContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type GridItemStyle<'a>
        = &'a Style
    where
        Self: 'a;
    fn get_grid_container_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn get_grid_child_style(&self, id: NodeId) -> &Style {
        self.style_or_default(id)
    }
    fn set_detailed_grid_info(&mut self, id: NodeId, info: DetailedGridInfo) {
        self.n_mut(id).detailed = DetailedLayoutInfo::Grid(Box::new(info));
    }
}

impl<C, M> RoundTree for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    fn get_unrounded_layout(&self, id: NodeId) -> Layout {
        match self.word(id) {
            Some(w) => w.unrounded,
            None => self.n(id).unrounded,
        }
    }
    fn set_final_layout(&mut self, id: NodeId, layout: &Layout) {
        match self.word_mut(id) {
            Some(w) => w.final_layout = *layout,
            None => self.n_mut(id).final_layout = *layout,
        }
    }
    fn is_out_of_flow(&self, id: NodeId) -> bool {
        if word_index(id).is_some() {
            return false;
        }
        let n = self.n(id);
        n.style.position.is_out_of_flow() && n.style.display != Display::None
    }
    fn hoisted_child_count(&self, id: NodeId) -> usize {
        if word_index(id).is_some() {
            0
        } else {
            self.n(id).hoisted.len()
        }
    }
    fn get_hoisted_child_id(&self, id: NodeId, index: usize) -> NodeId {
        self.n(id).hoisted[index]
    }
}
