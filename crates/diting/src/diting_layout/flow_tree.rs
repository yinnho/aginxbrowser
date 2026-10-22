//! Our own layout tree: node storage plus the dispatch that picks a taffy
//! algorithm per node.
//!
//! It exposes the same methods `diting_layout` used on `taffy::TaffyTree`
//! (so it is aliased in as `TaffyTree` there), and runs the same taffy
//! block / flex / grid algorithms. What it owns is the dispatch, which is the
//! seam float wrapping needs: the stock tree never hands a text leaf the
//! block formatting context it sits in, so a leaf cannot ask where the
//! floats are (#299 / #300).

use taffy::{
    compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout, compute_hidden_layout,
    compute_oof_layout, compute_root_layout, round_layout, BlockContext, Cache, CacheTree, DetailedGridInfo,
    DetailedLayoutInfo, Display, Layout, LayoutBlockContainer, LayoutContainingBlock, LayoutFlexboxContainer,
    LayoutGridContainer, LayoutInput, LayoutOutput, LayoutPartialTree, NodeId, RoundTree, RunMode, Size, Style,
    TaffyError, TraversePartialTree, TraverseTree,
};
use super::inline_flow;
use taffy::{AvailableSpace, FlexWrap};

type Res<T> = Result<T, TaffyError>;

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

pub struct FlowTree<C = ()> {
    /// Slots are never reused: a stale id must not alias a newer node.
    nodes: Vec<Option<Node<C>>>,
    rounding: bool,
    /// Boxes whose max-content width is their min-content width (table
    /// captions: they widen a table to their longest word, never to a line).
    min_as_max: std::collections::HashSet<NodeId>,
}

impl<C> Default for FlowTree<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> FlowTree<C> {
    pub fn new() -> Self {
        FlowTree { nodes: Vec::new(), rounding: true, min_as_max: std::collections::HashSet::new() }
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
        self.nodes.push(Some(Node {
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
        NodeId::from(self.nodes.len() - 1)
    }

    fn get(&self, id: NodeId) -> Res<&Node<C>> {
        self.nodes.get(usize::from(id)).and_then(|n| n.as_ref()).ok_or(TaffyError::InvalidInputNode(id))
    }

    fn get_mut(&mut self, id: NodeId) -> Res<&mut Node<C>> {
        self.nodes.get_mut(usize::from(id)).and_then(|n| n.as_mut()).ok_or(TaffyError::InvalidInputNode(id))
    }

    pub fn new_leaf(&mut self, style: Style) -> Res<NodeId> {
        Ok(self.push(style, None, Vec::new()))
    }

    pub fn new_leaf_with_context(&mut self, style: Style, ctx: C) -> Res<NodeId> {
        Ok(self.push(style, Some(ctx), Vec::new()))
    }

    pub fn new_with_children(&mut self, style: Style, children: &[NodeId]) -> Res<NodeId> {
        for c in children {
            self.get(*c).map_err(|_| TaffyError::InvalidChildNode(*c))?;
        }
        let id = self.push(style, None, children.to_vec());
        for c in children {
            self.get_mut(*c)?.parent = Some(id);
        }
        Ok(id)
    }

    /// Drop a node; its children stay in the tree, detached.
    pub fn remove(&mut self, node: NodeId) -> Res<NodeId> {
        let (parent, kids) = {
            let n = self.get(node)?;
            (n.parent, n.children.clone())
        };
        if let Some(p) = parent {
            if let Ok(pn) = self.get_mut(p) {
                pn.children.retain(|c| *c != node);
            }
            self.mark_dirty(p)?;
        }
        for k in kids {
            if let Ok(kn) = self.get_mut(k) {
                kn.parent = None;
            }
        }
        self.nodes[usize::from(node)] = None;
        Ok(node)
    }

    pub fn get_node_context(&self, node: NodeId) -> Option<&C> {
        self.get(node).ok()?.ctx.as_ref()
    }

    pub fn add_child(&mut self, parent: NodeId, child: NodeId) -> Res<()> {
        self.get_mut(child).map_err(|_| TaffyError::InvalidChildNode(child))?.parent = Some(parent);
        self.get_mut(parent).map_err(|_| TaffyError::InvalidParentNode(parent))?.children.push(child);
        self.mark_dirty(parent)
    }

    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) -> Res<()> {
        let old = self.get(parent).map_err(|_| TaffyError::InvalidParentNode(parent))?.children.clone();
        for c in old {
            if let Ok(n) = self.get_mut(c) {
                n.parent = None;
            }
        }
        for &c in children {
            if let Some(prev) = self.get(c).map_err(|_| TaffyError::InvalidChildNode(c))?.parent {
                self.remove_child(prev, c)?;
            }
            self.get_mut(c)?.parent = Some(parent);
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
        if let Ok(cn) = self.get_mut(child) {
            cn.parent = None;
        }
        self.mark_dirty(parent)?;
        Ok(child)
    }

    pub fn parent(&self, child: NodeId) -> Option<NodeId> {
        self.get(child).ok()?.parent
    }

    pub fn children(&self, parent: NodeId) -> Res<Vec<NodeId>> {
        Ok(self.get(parent)?.children.clone())
    }

    pub fn set_style(&mut self, node: NodeId, style: Style) -> Res<()> {
        self.get_mut(node)?.style = style;
        self.mark_dirty(node)
    }

    pub fn style(&self, node: NodeId) -> Res<&Style> {
        Ok(&self.get(node)?.style)
    }

    /// Layout relative to the parent (rounded when rounding is on).
    pub fn layout(&self, node: NodeId) -> Res<&Layout> {
        let n = self.get(node)?;
        Ok(if self.rounding { &n.final_layout } else { &n.unrounded })
    }

    pub fn unrounded_layout(&self, node: NodeId) -> &Layout {
        &self.nodes[usize::from(node)].as_ref().expect("live node").unrounded
    }

    /// Clear the cache of `node` and of every ancestor above it that still
    /// holds one.
    pub fn mark_dirty(&mut self, node: NodeId) -> Res<()> {
        let mut cur = Some(node);
        while let Some(id) = cur {
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
        self.get(node)?;
        let rounding = self.rounding;
        let mut view = View { tree: self, measure };
        compute_root_layout(&mut view, node, space);
        if rounding {
            round_layout(&mut view, node);
        }
        Ok(())
    }
}

struct View<'a, C, M> {
    tree: &'a mut FlowTree<C>,
    measure: M,
}

impl<C, M> View<'_, C, M> {
    fn n(&self, id: NodeId) -> &Node<C> {
        self.tree.nodes[usize::from(id)].as_ref().expect("live node")
    }
    fn n_mut(&mut self, id: NodeId) -> &mut Node<C> {
        self.tree.nodes[usize::from(id)].as_mut().expect("live node")
    }
}

impl<C, M> View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    fn compute(&mut self, id: NodeId, mut inputs: LayoutInput, block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput {
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
                    let n = tree.nodes[usize::from(id)].as_mut().expect("live node");
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
        ChildIter(self.n(parent).children.clone().into_iter())
    }
    fn child_count(&self, parent: NodeId) -> usize {
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
        = &'a Style
    where
        Self: 'a;
    type CustomIdent = String;
    fn get_core_container_style(&self, id: NodeId) -> &Style {
        &self.n(id).style
    }
    fn set_unrounded_layout(&mut self, id: NodeId, layout: &Layout) {
        self.n_mut(id).unrounded = *layout;
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
        &self.n(id).style
    }
    fn clear_hoisted_children(&mut self, id: NodeId) {
        self.n_mut(id).hoisted.clear();
    }
    fn add_hoisted_children(&mut self, id: NodeId, hoisted: &[NodeId]) {
        self.n_mut(id).hoisted.extend_from_slice(hoisted);
    }
    fn get_detailed_layout_info(&self, id: NodeId) -> &DetailedLayoutInfo {
        &self.n(id).detailed
    }
}

impl<C, M> CacheTree for View<'_, C, M>
where
    M: FnMut(LayoutInput, NodeId, Option<&mut C>, &Style, Option<&mut BlockContext<'_>>) -> LayoutOutput,
{
    fn cache_get(&mut self, id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.n_mut(id).cache.get(input)
    }
    fn cache_store(&mut self, id: NodeId, input: &LayoutInput, out: LayoutOutput) {
        self.n_mut(id).cache.store(input, out)
    }
    fn cache_clear(&mut self, id: NodeId) {
        self.n_mut(id).cache.clear();
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
        &self.n(id).style
    }
    fn get_block_child_style(&self, id: NodeId) -> &Style {
        &self.n(id).style
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
        = &'a Style
    where
        Self: 'a;
    fn get_flexbox_container_style(&self, id: NodeId) -> &Style {
        &self.n(id).style
    }
    fn get_flexbox_child_style(&self, id: NodeId) -> &Style {
        &self.n(id).style
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
        &self.n(id).style
    }
    fn get_grid_child_style(&self, id: NodeId) -> &Style {
        &self.n(id).style
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
        self.n(id).unrounded
    }
    fn set_final_layout(&mut self, id: NodeId, layout: &Layout) {
        self.n_mut(id).final_layout = *layout;
    }
    fn is_out_of_flow(&self, id: NodeId) -> bool {
        let n = self.n(id);
        n.style.position.is_out_of_flow() && n.style.display != Display::None
    }
    fn hoisted_child_count(&self, id: NodeId) -> usize {
        self.n(id).hoisted.len()
    }
    fn get_hoisted_child_id(&self, id: NodeId, index: usize) -> NodeId {
        self.n(id).hoisted[index]
    }
}
