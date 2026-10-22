//! `@layer` cascade layers (CSS Cascade 5).
//!
//! The stylesheet parser records, for every rule, the id of the layer it was
//! written in (0 = unlayered); the cascade turns that id into a rank:
//! later layers beat earlier ones, unlayered author rules beat every layer,
//! and `!important` inverts the whole order (earlier layer wins, unlayered
//! loses). Layer order is the order of first appearance (`@layer a, b;`
//! statements establish it before any block); nested layers sort before their
//! parent's own rules, which are the parent's implicit last sub-layer.
//!
//! Thread-local like the `@property` registry: one document's sheets are
//! parsed in one pass on the session thread, and `reset` opens that pass.

use std::cell::RefCell;

struct Node {
    name: String,
    children: Vec<usize>,
}

struct Table {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    anon: usize,
    ranks: Option<Vec<u32>>,
}

impl Table {
    fn new() -> Self {
        Table {
            nodes: vec![Node { name: String::new(), children: Vec::new() }],
            stack: Vec::new(),
            anon: 0,
            ranks: None,
        }
    }
    fn child(&mut self, parent: usize, name: &str) -> usize {
        if let Some(&c) = self.nodes[parent].children.iter().find(|&&c| self.nodes[c].name == name) {
            return c;
        }
        let id = self.nodes.len();
        self.nodes.push(Node { name: name.to_string(), children: Vec::new() });
        self.nodes[parent].children.push(id);
        self.ranks = None;
        id
    }
    fn top(&self) -> usize {
        self.stack.last().copied().unwrap_or(0)
    }
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::new());
}

/// A new stylesheet parse starts.
pub(crate) fn reset() {
    TABLE.with(|t| *t.borrow_mut() = Table::new());
}

/// `@layer a, b.c;` — declare names (relative to the enclosing layer) so
/// their order is fixed before any block of the same name appears.
pub(crate) fn declare_list(prelude: &str) {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        for name in prelude.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let mut cur = t.top();
            for seg in name.split('.') {
                cur = t.child(cur, seg.trim());
            }
        }
    });
}

/// Enter the layer a `@layer name { … }` block writes into (an empty name is
/// an anonymous layer). Returns the node count to hand back to `leave`.
pub(crate) fn enter(name: &str) {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let mut cur = t.top();
        if name.trim().is_empty() {
            t.anon += 1;
            let anon = format!("\u{0}anon{}", t.anon);
            cur = t.child(cur, &anon);
        } else {
            for seg in name.trim().split('.') {
                cur = t.child(cur, seg.trim());
            }
        }
        t.stack.push(cur);
    });
}

pub(crate) fn leave() {
    TABLE.with(|t| {
        t.borrow_mut().stack.pop();
    });
}

/// The layer id a rule written right now belongs to (0 = unlayered).
pub(crate) fn current() -> u32 {
    TABLE.with(|t| t.borrow().top() as u32)
}

/// Cascade rank of a layer id: unlayered is the top (`u32::MAX`); otherwise
/// the node's position in a post-order walk (children before the parent's own
/// rules, siblings in declaration order).
pub(crate) fn rank(id: u32) -> u32 {
    if id == 0 {
        return u32::MAX;
    }
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        if t.ranks.is_none() {
            let mut ranks = vec![0u32; t.nodes.len()];
            let mut next = 0u32;
            // Iterative post-order from the root's children.
            let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
            while let Some((n, i)) = stack.pop() {
                if i < t.nodes[n].children.len() {
                    stack.push((n, i + 1));
                    stack.push((t.nodes[n].children[i], 0));
                } else if n != 0 {
                    ranks[n] = next;
                    next += 1;
                }
            }
            t.ranks = Some(ranks);
        }
        t.ranks.as_ref().map(|r| r.get(id as usize).copied().unwrap_or(u32::MAX)).unwrap_or(u32::MAX)
    })
}
