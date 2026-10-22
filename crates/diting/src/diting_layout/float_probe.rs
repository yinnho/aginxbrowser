//! Probe (#299): can taffy's own float engine reproduce Chrome's float wrap?
//! Chrome truth (headless Chrome 2026-10-22, container 200px, `float:left`
//! 50x50, `p{margin:0;line-height:20px}` of 20 short words): the p BOX starts
//! at x=0 and overlaps the float; its line boxes sit at x = 50,50,50 (lines
//! whose top is above the float's bottom) then 0.

use taffy::prelude::*;
use taffy::{Clear, Float, FloatContext, FloatDirection};

/// Words of `word_w` px separated by `space_w` px, as (width, is_space).
fn words(n: usize, word_w: f32, space_w: f32) -> Vec<(f32, bool)> {
    let mut v = Vec::new();
    for i in 0..n {
        if i > 0 {
            v.push((space_w, true));
        }
        v.push((word_w, false));
    }
    v
}

/// Greedy fill where each line asks `slot(y)` for its (x, width).
fn wrap_in_slots(toks: &[(f32, bool)], line_h: f32, mut slot: impl FnMut(f32) -> (f32, f32)) -> Vec<(f32, f32)> {
    let mut lines = Vec::new();
    let (mut y, mut used, mut have) = (0.0f32, 0.0f32, false);
    let (mut x0, mut w) = slot(y);
    for &(tw, is_space) in toks {
        if is_space {
            if have {
                used += tw;
            }
            continue;
        }
        if have && used + tw > w {
            lines.push((x0, y));
            y += line_h;
            (x0, w) = slot(y);
            used = 0.0;
        }
        used += tw;
        have = true;
    }
    if have {
        lines.push((x0, y));
    }
    lines
}

#[test]
fn taffy_places_the_float_and_leaves_the_block_box_overlapping() {
    let mut tree: TaffyTree<()> = TaffyTree::new();
    let float = tree
        .new_leaf(Style { float: Float::Left, size: Size { width: length(50.0), height: length(50.0) }, ..Default::default() })
        .unwrap();
    let p = tree.new_leaf_with_context(Style { display: Display::Block, ..Default::default() }, ()).unwrap();
    let root = tree
        .new_with_children(
            Style { display: Display::Block, size: Size { width: length(200.0), height: auto() }, ..Default::default() },
            &[float, p],
        )
        .unwrap();
    let mut widths = Vec::new();
    tree.compute_layout_with_measure(
        root,
        Size { width: AvailableSpace::Definite(200.0), height: AvailableSpace::MaxContent },
        |inputs, _, _, style| {
            taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, avail| {
                widths.push((known.width, avail.width));
                Size { width: known.width.unwrap_or(0.0), height: 100.0 }
            })
        },
    )
    .unwrap();
    let pl = *tree.layout(p).unwrap();
    let fl = *tree.layout(float).unwrap();
    eprintln!("float at ({},{}) size {}x{}", fl.location.x, fl.location.y, fl.size.width, fl.size.height);
    eprintln!("p box at ({},{}) size {}x{}; measure saw {:?}", pl.location.x, pl.location.y, pl.size.width, pl.size.height, widths);
    assert_eq!(fl.location.x, 0.0);
    assert_eq!(fl.size.width, 50.0);
}

#[test]
fn float_context_slots_give_chromes_line_offsets() {
    let mut ctx = FloatContext::new();
    ctx.set_width(200.0);
    let at = ctx.place_floated_box(Size { width: 50.0, height: 50.0 }, 0.0, [0.0, 0.0], FloatDirection::Left, Clear::None);
    assert_eq!((at.x, at.y), (0.0, 0.0));
    let toks = words(20, 36.0, 4.0);
    let lines = wrap_in_slots(&toks, 20.0, |y| {
        let s = ctx.find_content_slot(y, [0.0, 0.0], Clear::None, None);
        (s.x, s.width)
    });
    eprintln!("lines (x, y): {lines:?}");
    let xs: Vec<f32> = lines.iter().map(|l| l.0).collect();
    // Chrome: 50, 50, 50, 0, 0 ... (lines whose top is above y=50 are narrowed).
    assert_eq!(&xs[..5], &[50.0, 50.0, 50.0, 0.0, 0.0]);
}

// --- Probe 3: our own tree, so the text node receives taffy's BlockContext ---

use taffy::{
    compute_block_layout, compute_cached_layout, compute_leaf_layout, compute_root_layout, Cache, CacheTree, Layout,
    LayoutBlockContainer, LayoutInput, LayoutOutput, LayoutPartialTree, NodeId, TraversePartialTree,
};

enum Kind {
    Block,
    Float,
    /// Inline content: tokens (width, is_space) wrapped at a line height.
    Text(Vec<(f32, bool)>, f32),
}

struct N {
    kind: Kind,
    style: Style,
    cache: Cache,
    layout: Layout,
    kids: Vec<N>,
    /// (x, y) of each line the text node laid out (relative to its own box).
    lines: Vec<(f32, f32)>,
}

impl N {
    fn new(kind: Kind, style: Style) -> N {
        N { kind, style, cache: Cache::new(), layout: Layout::with_order(0), kids: Vec::new(), lines: Vec::new() }
    }
    fn at(&self, id: NodeId) -> &N {
        let i = usize::from(id);
        if i == usize::MAX { self } else { &self.kids[i] }
    }
    fn at_mut(&mut self, id: NodeId) -> &mut N {
        let i = usize::from(id);
        if i == usize::MAX { self } else { &mut self.kids[i] }
    }
}

struct Iter(std::ops::Range<usize>);
impl Iterator for Iter {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        self.0.next().map(NodeId::from)
    }
}

impl TraversePartialTree for N {
    type ChildIter<'a> = Iter;
    fn child_ids(&self, _: NodeId) -> Iter {
        Iter(0..self.kids.len())
    }
    fn child_count(&self, _: NodeId) -> usize {
        self.kids.len()
    }
    fn get_child_id(&self, _: NodeId, i: usize) -> NodeId {
        NodeId::from(i)
    }
}

impl LayoutPartialTree for N {
    type CoreContainerStyle<'a> = &'a Style;
    type CustomIdent = String;
    fn get_core_container_style(&self, id: NodeId) -> &Style {
        &self.at(id).style
    }
    fn set_unrounded_layout(&mut self, id: NodeId, l: &Layout) {
        self.at_mut(id).layout = *l;
    }
    fn resolve_calc_value(&self, _: *const (), _: f32) -> f32 {
        0.0
    }
    fn compute_child_layout(&mut self, id: NodeId, inputs: LayoutInput) -> LayoutOutput {
        compute_cached_layout(self, id, inputs, |tree, id, inputs| {
            if matches!(tree.at(id).kind, Kind::Block) {
                compute_block_layout(tree, id, inputs, None)
            } else {
                // Float boxes (sized by style) and text without a context.
                compute_leaf_layout(inputs, &tree.at(id).style, |_, _| 0.0, |known, _| Size {
                    width: known.width.unwrap_or(0.0),
                    height: known.height.unwrap_or(0.0),
                })
            }
        })
    }
}

impl taffy::LayoutContainingBlock for N {
    type OofItemStyle<'a> = &'a Style;
    fn get_oof_item_style(&self, id: NodeId) -> &Style {
        &self.at(id).style
    }
    fn clear_hoisted_children(&mut self, _: NodeId) {}
    fn add_hoisted_children(&mut self, _: NodeId, _: &[NodeId]) {}
}

impl CacheTree for N {
    fn cache_get(&mut self, id: NodeId, i: &LayoutInput) -> Option<LayoutOutput> {
        self.at_mut(id).cache.get(i)
    }
    fn cache_store(&mut self, id: NodeId, i: &LayoutInput, o: LayoutOutput) {
        self.at_mut(id).cache.store(i, o)
    }
    fn cache_clear(&mut self, id: NodeId) {
        self.at_mut(id).cache.clear();
    }
}

impl LayoutBlockContainer for N {
    type BlockContainerStyle<'a> = &'a Style;
    type BlockItemStyle<'a> = &'a Style;
    fn get_block_container_style(&self, id: NodeId) -> &Style {
        &self.at(id).style
    }
    fn get_block_child_style(&self, id: NodeId) -> &Style {
        &self.at(id).style
    }
    fn compute_block_child_layout(
        &mut self,
        id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut taffy::BlockContext<'_>>,
    ) -> LayoutOutput {
        let Kind::Text(toks, line_h) = &self.at(id).kind else {
            return self.compute_child_layout(id, inputs);
        };
        let (toks, line_h) = (toks.clone(), *line_h);
        let Some(ctx) = block_ctx else { return self.compute_child_layout(id, inputs) };
        let width = inputs.known_dimensions.width.unwrap_or(0.0);
        // Slots are asked relative to this block's own top (the sub-context is
        // positioned at the child's estimated y).
        let lines = wrap_in_slots(&toks, line_h, |y| {
            let s = ctx.find_content_slot(y, Clear::None, None);
            (s.x, s.width)
        });
        let height = lines.len() as f32 * line_h;
        self.at_mut(id).lines = lines;
        LayoutOutput::from_outer_size(Size { width, height })
    }
}

#[test]
fn own_tree_text_node_gets_the_block_context_and_wraps_around_the_float() {
    let mut root = N::new(Kind::Block, Style { display: Display::Block, size: Size { width: length(200.0), height: auto() }, ..Default::default() });
    root.kids.push(N::new(Kind::Float, Style { float: Float::Left, size: Size { width: length(50.0), height: length(50.0) }, ..Default::default() }));
    root.kids.push(N::new(Kind::Text(words(20, 36.0, 4.0), 20.0), Style { display: Display::Block, ..Default::default() }));
    compute_root_layout(&mut root, NodeId::from(usize::MAX), Size { width: AvailableSpace::Definite(200.0), height: AvailableSpace::MaxContent });
    let p = &root.kids[1];
    eprintln!("p box ({},{}) {}x{}; lines {:?}", p.layout.location.x, p.layout.location.y, p.layout.size.width, p.layout.size.height, p.lines);
    assert_eq!(p.layout.location.x, 0.0, "the block box overlaps the float (Chrome boxLeft 0)");
    let xs: Vec<f32> = p.lines.iter().map(|l| l.0).collect();
    assert_eq!(&xs[..5], &[50.0, 50.0, 50.0, 0.0, 0.0], "line offsets follow the float (Chrome)");
}
