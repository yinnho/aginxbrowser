//! Which DOM text nodes (and which character ranges of them) each text leaf
//! of the solved tree carries, so range / client-rect queries can turn the
//! painted text back into per-node line fragments.
//!
//! The builders know the DOM node when they create a leaf but the leaf types
//! do not; threading an id through every constructor would touch dozens of
//! call sites, so the knowledge travels in thread-locals while one solve
//! builds, is packed into the `SolvedGeometry`, and is re-armed for the
//! collect walk that stamps it onto the paint items.

use super::NodeId;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// (DOM text node, start char in the leaf's text, char length).
/// (text node, char index in the item's text where the node's chars begin, char
/// count). The start is signed: a line item cut from the middle of a run
/// begins after some node's first chars.
pub(crate) type Segs = Rc<[(NodeId, i32, u32)]>;
pub(crate) type SrcMap = Rc<HashMap<taffy::tree::NodeId, Segs>>;

thread_local! {
    static PENDING: RefCell<Vec<(u32, NodeId)>> = const { RefCell::new(Vec::new()) };
    static FRAMES: Cell<u32> = const { Cell::new(0) };
    static CUR: Cell<Option<NodeId>> = const { Cell::new(None) };
    static BUILDING: RefCell<HashMap<taffy::tree::NodeId, Segs>> = RefCell::new(HashMap::new());
    static ACTIVE: RefCell<Option<SrcMap>> = const { RefCell::new(None) };
}

/// A solve starts: forget the previous tree's leaves.
pub(crate) fn reset() {
    PENDING.with(|p| p.borrow_mut().clear());
    FRAMES.with(|f| f.set(0));
    CUR.with(|c| c.set(None));
    BUILDING.with(|b| b.borrow_mut().clear());
}

/// The solve finished: hand its map to the geometry.
pub(crate) fn take() -> SrcMap {
    Rc::new(BUILDING.with(|b| std::mem::take(&mut *b.borrow_mut())))
}

/// A `RunSeg::Text` for this DOM text node entered the pending run.
pub(crate) fn note_seg(frame: u32, node: NodeId) {
    PENDING.with(|p| p.borrow_mut().push((frame, node)));
}

/// A run-building context starts (runs nest: an inline element builds its own
/// run inside its parent's), so pending nodes are tagged by their context.
pub(crate) fn new_frame() -> u32 {
    FRAMES.with(|f| {
        f.set(f.get() + 1);
        f.get()
    })
}

/// The run is being flushed: take its text nodes in order.
pub(crate) fn drain_pending(frame: u32) -> Vec<NodeId> {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let mine = p.iter().filter(|(f, _)| *f == frame).map(|(_, n)| *n).collect();
        p.retain(|(f, _)| *f != frame);
        mine
    })
}

/// The word leaves built next belong to this text node.
pub(crate) fn set_cur(node: Option<NodeId>) {
    CUR.with(|c| c.set(node));
}

pub(crate) fn cur() -> Option<NodeId> {
    CUR.with(|c| c.get())
}

pub(crate) fn register(leaf: taffy::tree::NodeId, segs: Vec<(NodeId, u32, u32)>) {
    if !segs.is_empty() {
        let segs: Vec<(NodeId, i32, u32)> = segs.into_iter().map(|(n, s, l)| (n, s as i32, l)).collect();
        BUILDING.with(|b| b.borrow_mut().insert(leaf, segs.into()));
    }
}

/// Char ranges (start, end) in `text` of each token `tokenize` yields: words
/// advance by their length, whitespace tokens swallow the whole collapsed run.
pub(crate) fn token_ranges(text: &str, tokens: &[String]) -> Vec<(u32, u32)> {
    let chars: Vec<char> = text.chars().collect();
    let mut cur = 0usize;
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        // Leading whitespace the tokenizer folded away (tokenize_ws trims).
        if !t.trim().is_empty() {
            while cur < chars.len() && chars[cur].is_whitespace() {
                cur += 1;
            }
        }
        let start = cur;
        if t.trim().is_empty() {
            while cur < chars.len() && chars[cur].is_whitespace() {
                cur += 1;
            }
        } else {
            cur = (cur + t.chars().count()).min(chars.len());
        }
        out.push((start as u32, cur as u32));
    }
    out
}

/// Arm the map for one collect walk; dropping the guard disarms it.
pub(crate) struct Armed;
pub(crate) fn arm(map: SrcMap) -> Armed {
    ACTIVE.with(|a| *a.borrow_mut() = Some(map));
    Armed
}
impl Drop for Armed {
    fn drop(&mut self) {
        ACTIVE.with(|a| *a.borrow_mut() = None);
    }
}

pub(crate) fn lookup(leaf: taffy::tree::NodeId) -> Option<Segs> {
    ACTIVE.with(|a| a.borrow().as_ref().and_then(|m| m.get(&leaf).cloned()))
}

/// The part of `segs` (run coordinates) a line item covering run chars
/// `[from, to)` needs, re-based to the line's own text.
pub(crate) fn slice(segs: &Segs, from: u32, to: u32) -> Segs {
    let (from, to) = (from as i64, to as i64);
    segs.iter()
        .filter(|(_, s, l)| (*s as i64) < to && *s as i64 + *l as i64 > from)
        .map(|(n, s, l)| (*n, (*s as i64 - from) as i32, *l))
        .collect::<Vec<_>>()
        .into()
}

/// Pair a flushed run's text nodes with the char lengths of their segments.
pub(crate) fn seg_ranges(nodes: &[NodeId], lens: &[u32]) -> Vec<(NodeId, u32, u32)> {
    let mut start = 0u32;
    nodes
        .iter()
        .zip(lens.iter())
        .map(|(n, l)| {
            let r = (*n, start, *l);
            start += *l;
            r
        })
        .collect()
}

// Moved out of mod.rs (god-file ratchet): inline-run edge whitespace trimming.
use super::TextLeaf;
use super::flow_tree::FlowTree as TaffyTree;

/// CSS §16.6.1: collapsible whitespace at the edges of an inline formatting
/// context contributes no width. The pure-text path already trims edges
/// (measure_text_leaf); the mixed-run path used to keep whitespace-only word
/// leaves, and each tokenized space carried its full advance into
/// shrink-to-fit/max-content widths — newline+indent text nodes between
/// inline siblings added phantom space runs (obscura#764's +15px). Detached
/// leaves are removed from the tree, like the flatten-path cleanup.
pub(super) fn trim_run_edge_whitespace(
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
