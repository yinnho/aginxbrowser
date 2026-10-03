//! Preorder (document) order helpers for the DOM ops — split from mod.rs
//! at the god-file ratchet's demand. Behavior-neutral move.

use crate::diting_dom::{DomTree, NodeId};

/// Index of `n` among its parent's children (0-based).
pub(super) fn node_child_index(dom: &DomTree, n: NodeId) -> usize {
    let mut i = 0usize;
    let mut cur = dom.get_node(n).and_then(|x| x.prev_sibling);
    while let Some(p) = cur {
        i += 1;
        cur = dom.get_node(p).and_then(|x| x.prev_sibling);
    }
    i
}

/// Ancestor chain of `n` from the root down to `n` (root first).
pub(super) fn node_ancestors_root_first(dom: &DomTree, n: NodeId) -> Vec<NodeId> {
    let mut v = vec![n];
    let mut cur = n;
    while let Some(p) = dom.get_node(cur).and_then(|x| x.parent) {
        v.push(p);
        cur = p;
    }
    v.reverse();
    v
}

/// Preorder (document) order comparison of two nodes: -1 before, 1 after, 0 same.
pub(super) fn compare_node_order(dom: &DomTree, a: NodeId, b: NodeId) -> i32 {
    if a == b {
        return 0;
    }
    let aa = node_ancestors_root_first(dom, a);
    let bb = node_ancestors_root_first(dom, b);
    // Different roots: order is undefined per spec; keep it stable by node id.
    if aa[0] != bb[0] {
        return if a.index() < b.index() { -1 } else { 1 };
    }
    let mut i = 0usize;
    while i < aa.len() && i < bb.len() && aa[i] == bb[i] {
        i += 1;
    }
    if i >= aa.len() {
        return -1; // a is an ancestor of b -> a precedes
    }
    if i >= bb.len() {
        return 1; // b is an ancestor of a -> a follows
    }
    if node_child_index(dom, aa[i]) < node_child_index(dom, bb[i]) {
        -1
    } else {
        1
    }
}
