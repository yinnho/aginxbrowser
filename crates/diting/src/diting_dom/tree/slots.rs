//! Slot assignment and flat-tree walks over shadow boundaries: which
//! `<slot>` a light child renders through (`assigned_slot`/
//! `assigned_nodes`) and the shadow-including successor/predecessor walks
//! the frame pump iterates with (`next_in_subtree` family). Split out of
//! `tree.rs` in batch 202 to ride under the layering audit's god-file
//! cap — read-only queries; the structural mutations stay in the parent.

use super::{DomTree, DomTreeInner, NodeId};

impl DomTree {
    /// Whether `node` is an HTML `<slot>` element. Slot assignment is defined
    /// only for HTML slots; same-local-name elements in other namespaces do
    /// not participate in the flattened tree.
    pub fn is_html_slot_element(&self, node: NodeId) -> bool {
        self.get_node(node).is_some_and(|node| {
            node.as_element().is_some_and(|name| {
                name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
                    && name.local.as_ref() == "slot"
            })
        })
    }

    /// Return the first slot to which `node` is assigned.
    ///
    /// The node must be a direct light child of a shadow host. Element slot
    /// names and slot `name` values compare as exact strings; text nodes use
    /// the empty/default name. The first same-name slot in shadow-tree order
    /// wins, matching the HTML slot assignment algorithm.
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        let node_ref = self.get_node(node)?;
        let parent = node_ref.parent?;
        let name = if node_ref.is_element() {
            node_ref.get_attribute("slot").unwrap_or("").to_owned()
        } else if node_ref.text_content_of_text_node().is_some() {
            String::new()
        } else {
            return None;
        };
        drop(node_ref);

        let root = self.shadow_root(parent)?;
        self.descendants(root).into_iter().find(|candidate| {
            self.is_html_slot_element(*candidate)
                && self
                    .get_node(*candidate)
                    .and_then(|slot| slot.get_attribute("name").map(str::to_owned))
                    .unwrap_or_default()
                    == name
        })
    }

    /// Nodes directly assigned to an HTML slot. The first same-name slot wins;
    /// later duplicate slots and slots with no matching light children return
    /// an empty list. `None` means `slot` is not a slot in a shadow tree.
    pub fn assigned_nodes(&self, slot: NodeId) -> Option<Vec<NodeId>> {
        if !self.is_html_slot_element(slot) {
            return None;
        }
        let root = self.containing_shadow_root(slot)?;
        let host = self.shadow_root_info(root)?.host;
        let name = self
            .get_node(slot)
            .and_then(|slot| slot.get_attribute("name").map(str::to_owned))
            .unwrap_or_default();
        let is_same_name_slot = |candidate: NodeId| {
            self.is_html_slot_element(candidate)
                && self
                    .get_node(candidate)
                    .and_then(|slot| slot.get_attribute("name").map(str::to_owned))
                    .unwrap_or_default()
                    == name
        };
        if self
            .descendants(root)
            .into_iter()
            .take_while(|candidate| *candidate != slot)
            .any(is_same_name_slot)
        {
            return Some(Vec::new());
        }
        Some(
            self.children(host)
                .into_iter()
                .filter(|candidate| {
                    let Some(node) = self.get_node(*candidate) else {
                        return false;
                    };
                    let candidate_name = if node.is_element() {
                        node.get_attribute("slot").unwrap_or("")
                    } else if node.text_content_of_text_node().is_some() {
                        ""
                    } else {
                        return false;
                    };
                    candidate_name == name
                })
                .collect(),
        )
    }

    /// Returns the node after `current` in document order, without leaving the
    /// subtree rooted at `root`.
    ///
    /// Keeping the ancestor climb inside the DOM avoids one JS/native crossing
    /// per ancestor when a TreeWalker reaches a deep leaf.
    pub fn next_in_subtree(&self, root: NodeId, current: NodeId) -> Option<NodeId> {
        let inner = self.inner.borrow();
        let current_node = inner.nodes.get(current.index())?.as_ref()?;
        if let Some(child) = current_node.first_child {
            return Some(child);
        }
        Self::climb_to_next_sibling(&inner, root, current)
    }

    /// Returns the node after the whole subtree rooted at `current`, in document
    /// order, without leaving the subtree rooted at `root`.
    ///
    /// This is `next_in_subtree` minus the descend-into-children step, which is
    /// what `NodeFilter.FILTER_REJECT` needs: it rejects a node *and* its
    /// descendants, unlike `FILTER_SKIP`, which only skips the node itself and
    /// is served by `next_in_subtree`.
    pub fn next_after_subtree(&self, root: NodeId, current: NodeId) -> Option<NodeId> {
        let inner = self.inner.borrow();
        Self::climb_to_next_sibling(&inner, root, current)
    }

    /// Returns the node before `current` in document order, without leaving the
    /// subtree rooted at `root`. `root` has no predecessor within its own
    /// subtree, but it is itself reachable as one — a NodeIterator can return
    /// its root, unlike a TreeWalker.
    ///
    /// A NodeIterator applies no subtree pruning (DOM 6.2: FILTER_REJECT
    /// behaves as FILTER_SKIP), so unlike the TreeWalker's backward walk the
    /// whole step fits here instead of being interleaved with filter calls.
    pub fn prev_in_subtree(&self, root: NodeId, current: NodeId) -> Option<NodeId> {
        let inner = self.inner.borrow();
        if current == root {
            return None;
        }
        let current_node = inner.nodes.get(current.index())?.as_ref()?;

        let Some(prev) = current_node.prev_sibling else {
            // No previous sibling: the parent immediately precedes `current`.
            return current_node.parent;
        };

        // Otherwise it is the previous sibling's deepest last descendant.
        let mut node_id = prev;
        for _ in 0..=inner.nodes.len() {
            let node = inner.nodes.get(node_id.index())?.as_ref()?;
            match node.last_child {
                Some(child) => node_id = child,
                None => return Some(node_id),
            }
        }

        // Same defense in depth as the forward walk: a malformed tree must not
        // spin here.
        None
    }

    /// Follow `current`'s next sibling, climbing ancestors until one has a next
    /// sibling — without stepping outside `root`.
    fn climb_to_next_sibling(
        inner: &DomTreeInner,
        root: NodeId,
        current: NodeId,
    ) -> Option<NodeId> {
        let mut node_id = current;
        for _ in 0..=inner.nodes.len() {
            if node_id == root {
                return None;
            }
            let node = inner.nodes.get(node_id.index())?.as_ref()?;
            if let Some(sibling) = node.next_sibling {
                return Some(sibling);
            }
            node_id = node.parent?;
        }

        // Parent cycles are prevented by the mutation APIs. Keep a hard bound
        // here as defense in depth for a malformed tree.
        None
    }
}
