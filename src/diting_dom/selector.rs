use cssparser::{CowRcStr, ToCss};
use html5ever::{namespace_url, ns, LocalName, Namespace};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::context::QuirksMode;
use selectors::matching::{
    ElementSelectorFlags, MatchingContext, MatchingForInvalidation, MatchingMode,
    NeedsSelectorFlags,
};
use selectors::parser::{self, ParseRelative, SelectorParseErrorKind};
use selectors::{Element, OpaqueElement, SelectorList};
use selectors::visitor::SelectorVisitor;
use std::collections::HashMap;

use crate::diting_dom::tree::{DomTree, NodeData, NodeId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DitingSelector;

impl parser::SelectorImpl for DitingSelector {
    type ExtraMatchingData<'a> = ();
    type AttrValue = CssString;
    type Identifier = CssString;
    type LocalName = CssLocalName;
    type NamespaceUrl = CssNamespace;
    type NamespacePrefix = CssString;
    type BorrowedLocalName = CssLocalName;
    type BorrowedNamespaceUrl = CssNamespace;
    type NonTSPseudoClass = PseudoClass;
    type PseudoElement = PseudoElement;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CssString(pub String);

impl<'a> From<&'a str> for CssString {
    fn from(s: &'a str) -> Self {
        CssString(s.to_string())
    }
}

impl AsRef<str> for CssString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl ToCss for CssString {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        cssparser::serialize_string(&self.0, dest)
    }
}

impl PrecomputedHash for CssString {
    fn precomputed_hash(&self) -> u32 {
        let mut h: u32 = 5381;
        for b in self.0.as_bytes() {
            h = h.wrapping_mul(33).wrapping_add(*b as u32);
        }
        h
    }
}

impl Default for CssString {
    fn default() -> Self {
        CssString(String::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CssLocalName(pub LocalName);

impl<'a> From<&'a str> for CssLocalName {
    fn from(s: &'a str) -> Self {
        CssLocalName(LocalName::from(s))
    }
}

impl ToCss for CssLocalName {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        dest.write_str(&self.0)
    }
}

impl PrecomputedHash for CssLocalName {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CssNamespace(pub Namespace);

impl PrecomputedHash for CssNamespace {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoClass {
    Hover,
    Active,
    Focus,
    FocusVisible,
    FocusWithin,
    Enabled,
    Disabled,
    Checked,
    Link,
    Visited,
}

impl parser::NonTSPseudoClass for PseudoClass {
    type Impl = DitingSelector;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, PseudoClass::Hover | PseudoClass::Active)
    }

    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            PseudoClass::Hover
                | PseudoClass::Active
                | PseudoClass::Focus
                | PseudoClass::FocusVisible
                | PseudoClass::FocusWithin
        )
    }

    fn visit<V>(&self, _visitor: &mut V) -> bool
    where
        V: SelectorVisitor<Impl = Self::Impl>,
    {
        true
    }
}

impl ToCss for PseudoClass {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        match self {
            PseudoClass::Hover => dest.write_str(":hover"),
            PseudoClass::Active => dest.write_str(":active"),
            PseudoClass::Focus => dest.write_str(":focus"),
            PseudoClass::FocusVisible => dest.write_str(":focus-visible"),
            PseudoClass::FocusWithin => dest.write_str(":focus-within"),
            PseudoClass::Enabled => dest.write_str(":enabled"),
            PseudoClass::Disabled => dest.write_str(":disabled"),
            PseudoClass::Checked => dest.write_str(":checked"),
            PseudoClass::Link => dest.write_str(":link"),
            PseudoClass::Visited => dest.write_str(":visited"),
        }
    }
}

/// Placeholder so `DitingSelector` can name a pseudo-element type for the
/// selectors crate; nothing generates pseudo-elements, so it is uninhabited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {}

impl parser::PseudoElement for PseudoElement {
    type Impl = DitingSelector;
}

impl ToCss for PseudoElement {
    fn to_css<W: std::fmt::Write>(&self, _dest: &mut W) -> std::fmt::Result {
        match *self {}
    }
}

pub struct DitingSelectorParser;

impl<'i> parser::Parser<'i> for DitingSelectorParser {
    type Impl = DitingSelector;
    type Error = SelectorParseErrorKind<'i>;

    // Allow `:has()`. The selectors crate gates relative-selector parsing on
    // this (default false), so without it `a:has(p)` failed to parse and the
    // error was swallowed into an empty match set. Matching already passes a
    // SelectorCaches (which holds the relative-selector cache), so enabling
    // parsing is sufficient.
    fn parse_has(&self) -> bool {
        true
    }

    // Allow `:is()` and `:where()`. The selectors crate gates these on this hook
    // (default false), so without it every `:where(...)`/`:is(...)` selector
    // failed to parse and was dropped, discarding those rules. Tailwind's
    // preflight and most modern resets wrap their rules in `:where(...)` for
    // zero specificity, so this blanked large parts of many sites. Matching
    // (Component::Is/Where) is built into the crate, so enabling parsing is
    // sufficient.
    fn parse_is_and_where(&self) -> bool {
        true
    }

    fn parse_non_ts_pseudo_class(
        &self,
        _location: cssparser::SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoClass, cssparser::ParseError<'i, Self::Error>> {
        match name.as_ref() {
            "hover" => Ok(PseudoClass::Hover),
            "active" => Ok(PseudoClass::Active),
            "focus" => Ok(PseudoClass::Focus),
            "focus-visible" => Ok(PseudoClass::FocusVisible),
            "focus-within" => Ok(PseudoClass::FocusWithin),
            "enabled" => Ok(PseudoClass::Enabled),
            "disabled" => Ok(PseudoClass::Disabled),
            "checked" => Ok(PseudoClass::Checked),
            "link" | "any-link" => Ok(PseudoClass::Link),
            "visited" => Ok(PseudoClass::Visited),
            _ => Err(cssparser::ParseError {
                kind: cssparser::ParseErrorKind::Custom(
                    SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
                ),
                location: _location,
            }),
        }
    }
}

#[derive(Clone, Copy)]
pub struct DomElement<'a> {
    pub tree: &'a DomTree,
    pub node_id: NodeId,
}

impl<'a> DomElement<'a> {
    pub fn new(tree: &'a DomTree, node_id: NodeId) -> Self {
        DomElement { tree, node_id }
    }

    /// Is this a form control element that `:enabled`/`:disabled` apply to?
    fn is_form_control(&self) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.as_element()
                    .map(|name| {
                        matches!(
                            name.local.as_ref(),
                            "input"
                                | "button"
                                | "select"
                                | "textarea"
                                | "optgroup"
                                | "option"
                                | "fieldset"
                        )
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// A boolean HTML attribute's presence (its value does not matter: per
    /// HTML, `disabled=""`, `disabled="disabled"`, and bare `disabled` are
    /// all equally "set").
    fn has_boolean_attr(&self, name: &str) -> bool {
        self.tree
            .with_node(self.node_id, |n| n.get_attribute(name).is_some())
            .unwrap_or(false)
    }

    /// Chrome's :focus-visible text-entry family: textarea, select, a text
    /// flavor of input (absent/unknown type defaults to text), or anything
    /// contenteditable. Buttons, links, checkboxes stay mouse-ish.
    fn is_text_entry_control(&self) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                let e = n.as_element()?;
                let local = e.local.to_ascii_lowercase();
                if matches!(local.as_ref(), "textarea" | "select") {
                    return Some(true);
                }
                if local.as_ref() == "input" {
                    let t = n
                        .get_attribute("type")
                        .map(|s| s.to_ascii_lowercase())
                        .unwrap_or_else(|| "text".into());
                    return Some(!matches!(
                        t.as_str(),
                        "button"
                            | "submit"
                            | "reset"
                            | "image"
                            | "checkbox"
                            | "radio"
                            | "file"
                            | "hidden"
                    ));
                }
                if n.get_attribute("contenteditable").is_some() {
                    return Some(true);
                }
                Some(false)
            })
            .flatten()
            .unwrap_or(false)
    }
}

impl<'a> std::fmt::Debug for DomElement<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DomElement({:?})", self.node_id)
    }
}

impl<'a> PartialEq for DomElement<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
    }
}

impl<'a> Eq for DomElement<'a> {}

impl<'a> Element for DomElement<'a> {
    type Impl = DitingSelector;

    fn opaque(&self) -> OpaqueElement {
        // Must be stable per node. DomElement is Copy and gets a fresh stack
        // address on every traversal step, so OpaqueElement::new(self) returns a
        // different id for the same node each call. That breaks `:has` anchor
        // matching, which compares the anchor's opaque against the element
        // reached by walking up the tree. Key off the node's stable slot in the
        // tree's Vec instead (OpaqueElement only compares the address, never
        // dereferences it, and the Vec is not mutated during a query).
        let inner = self.tree.borrow_inner();
        match inner.nodes.get(self.node_id.index()) {
            Some(slot) => OpaqueElement::new(slot),
            None => OpaqueElement::new(self),
        }
    }

    fn parent_element(&self) -> Option<Self> {
        let node = self.tree.get_node(self.node_id)?;
        let parent_id = node.parent?;
        let parent = self.tree.get_node(parent_id)?;
        if parent.is_element() {
            Some(DomElement::new(self.tree, parent_id))
        } else {
            None
        }
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn pseudo_element_originating_element(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let node = self.tree.get_node(self.node_id)?;
        let mut current = node.prev_sibling;
        while let Some(sibling_id) = current {
            let sibling = self.tree.get_node(sibling_id)?;
            if sibling.is_element() {
                return Some(DomElement::new(self.tree, sibling_id));
            }
            current = sibling.prev_sibling;
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let node = self.tree.get_node(self.node_id)?;
        let mut current = node.next_sibling;
        while let Some(sibling_id) = current {
            let sibling = self.tree.get_node(sibling_id)?;
            if sibling.is_element() {
                return Some(DomElement::new(self.tree, sibling_id));
            }
            current = sibling.next_sibling;
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        let node = self.tree.get_node(self.node_id)?;
        let mut current = node.first_child;
        while let Some(child_id) = current {
            let child = self.tree.get_node(child_id)?;
            if child.is_element() {
                return Some(DomElement::new(self.tree, child_id));
            }
            current = child.next_sibling;
        }
        None
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.as_element()
                    .map(|name| name.ns == ns!(html))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn has_local_name(&self, local_name: &CssLocalName) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.as_element()
                    .map(|name| name.local == local_name.0)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn has_namespace(&self, ns: &CssNamespace) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.as_element()
                    .map(|name| name.ns == ns.0)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn is_same_type(&self, other: &Self) -> bool {
        let self_name = self.tree.with_node(self.node_id, |n| {
            n.as_element().map(|name| (name.local.clone(), name.ns.clone()))
        }).flatten();
        let other_name = self.tree.with_node(other.node_id, |n| {
            n.as_element().map(|name| (name.local.clone(), name.ns.clone()))
        }).flatten();
        match (self_name, other_name) {
            (Some((al, ans)), Some((bl, bns))) => al == bl && ans == bns,
            _ => false,
        }
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&CssNamespace>,
        local_name: &CssLocalName,
        operation: &AttrSelectorOperation<&CssString>,
    ) -> bool {
        self.tree
            .with_node(self.node_id, |node| {
                let attrs = match node.attrs() {
                    Some(a) => a,
                    None => return false,
                };
                attrs.iter().any(|attr| {
                    let ns_match = match ns {
                        NamespaceConstraint::Any => true,
                        NamespaceConstraint::Specific(expected_ns) => attr.name.ns == expected_ns.0,
                    };
                    if !ns_match || attr.name.local != local_name.0 {
                        return false;
                    }
                    operation.eval_str(&attr.value)
                })
            })
            .unwrap_or(false)
    }

    fn has_attr_in_no_namespace(&self, local_name: &CssLocalName) -> bool {
        self.tree
            .with_node(self.node_id, |node| {
                node.attrs()
                    .map(|attrs| {
                        attrs.iter().any(|a| {
                            a.name.ns == html5ever::ns!()
                                && a.name.local == local_name.0
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        match pc {
            PseudoClass::Link => self.is_link(),
            PseudoClass::Visited => false,
            // :enabled/:disabled/:checked reflect real, static DOM state (the
            // disabled/checked/selected attributes), not live user interaction,
            // so they resolve the same way against a static snapshot as they
            // would in a browser that never received an input event. Component
            // systems (Material, Bootstrap, ...) lean on :enabled for base
            // styling, so treating it as unconditionally false made every such
            // rule silently inert.
            PseudoClass::Enabled => self.is_form_control() && !self.has_boolean_attr("disabled"),
            PseudoClass::Disabled => self.is_form_control() && self.has_boolean_attr("disabled"),
            PseudoClass::Checked => {
                self.has_boolean_attr("checked") || self.has_boolean_attr("selected")
            }
            // Focus tracks the tree's live focused node (blitz#839): agents
            // focus() a control and then read :focus styles, and the old
            // unconditional false froze every focus-dependent rule inert.
            PseudoClass::Focus => self.tree.focused_node() == Some(self.node_id),
            PseudoClass::FocusWithin => {
                // :focus-within = the subject itself is focused OR contains
                // the focused node — so climb from the focused element and
                // see whether the subject is on that ancestor chain.
                let mut cur = self.tree.focused_node();
                while let Some(id) = cur {
                    if id == self.node_id {
                        return true;
                    }
                    cur = self.tree.get_node(id).and_then(|n| n.parent);
                }
                false
            }
            // Chrome's :focus-visible heuristic narrowed to what a
            // script-driven engine knows: text-entry controls show the ring
            // on programmatic focus, mouse-ish targets (button/link) don't.
            // Keyboard Tab would light those too, but Tab traversal isn't
            // implemented — this matches Chrome for every focus() probe an
            // agent actually runs.
            PseudoClass::FocusVisible => {
                self.tree.focused_node() == Some(self.node_id) && self.is_text_entry_control()
            }
            // Hover/active stay snapshot-false: nothing in the engine holds
            // a live hover/active target.
            PseudoClass::Hover | PseudoClass::Active => false,
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.as_element()
                    .map(|name| {
                        matches!(name.local.as_ref(), "a" | "area" | "link")
                            && n.get_attribute("href").is_some()
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn assigned_slot(&self) -> Option<Self> {
        None
    }

    fn has_id(&self, id: &CssString, case_sensitivity: CaseSensitivity) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.get_attribute("id")
                    .map(|value| case_sensitivity.eq(value.as_bytes(), id.0.as_bytes()))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn has_class(&self, name: &CssString, case_sensitivity: CaseSensitivity) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.get_attribute("class")
                    .map(|class_attr| {
                        class_attr
                            .split_whitespace()
                            .any(|c| case_sensitivity.eq(c.as_bytes(), name.0.as_bytes()))
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn has_custom_state(&self, _name: &CssString) -> bool {
        false
    }

    fn imported_part(&self, _name: &CssString) -> Option<CssString> {
        None
    }

    fn is_part(&self, _name: &CssString) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.tree
            .with_node(self.node_id, |node| {
                let mut child = node.first_child;
                while let Some(child_id) = child {
                    if let Some(child_node) = self.tree.get_node(child_id) {
                        match &child_node.data {
                            NodeData::Element { .. } => return false,
                            NodeData::Text { contents } if !contents.is_empty() => return false,
                            _ => {}
                        }
                        child = child_node.next_sibling;
                    } else {
                        break;
                    }
                }
                true
            })
            .unwrap_or(true)
    }

    fn is_root(&self) -> bool {
        self.tree
            .with_node(self.node_id, |n| {
                n.parent
                    .map(|parent_id| {
                        self.tree
                            .with_node(parent_id, |p| p.is_document())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn ignores_nth_child_selectors(&self) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, _filter: &mut selectors::bloom::BloomFilter) -> bool {
        false
    }
}

// Thread-local LRU cache of parsed selectors. Without this every
// querySelector / querySelectorAll re-parses the selector string;
// for batch-heavy DOM access (agent scraping a table, framework
// repeatedly polling for elements) the parse cost adds up to tens
// of ms per page. Cap of 256 entries fits a typical page's distinct
// selectors without unbounded memory growth.
thread_local! {
    static SELECTOR_CACHE: std::cell::RefCell<
        std::collections::HashMap<String, std::sync::Arc<SelectorList<DitingSelector>>>,
    > = std::cell::RefCell::new(std::collections::HashMap::with_capacity(64));
}
const SELECTOR_CACHE_CAP: usize = 256;

fn parse_selector_uncached(selector: &str) -> Result<SelectorList<DitingSelector>, String> {
    let mut parser_input = cssparser::ParserInput::new(selector);
    let mut parser = cssparser::Parser::new(&mut parser_input);
    SelectorList::parse(&DitingSelectorParser, &mut parser, ParseRelative::No)
        .map_err(|e| format!("Failed to parse selector '{}': {:?}", selector, e))
}

pub fn parse_selector(selector: &str) -> Result<SelectorList<DitingSelector>, String> {
    // Hot path: cached. Cold path: parse + insert.
    if let Some(cached) = SELECTOR_CACHE.with(|c| c.borrow().get(selector).cloned()) {
        return Ok((*cached).clone());
    }
    let parsed = parse_selector_uncached(selector)?;
    let cached = std::sync::Arc::new(parsed.clone());
    SELECTOR_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        // Crude eviction: if at cap, dump the whole table. A real LRU
        // would be more memory-friendly but selectors are small and 256
        // is comfortably above a single page's distinct-selector count.
        if cache.len() >= SELECTOR_CACHE_CAP {
            cache.clear();
        }
        cache.insert(selector.to_string(), cached);
    });
    Ok(parsed)
}

/// If `selector` is a bare ASCII id selector like `#main`, return the id (without
/// the `#`). Conservative: escapes, combinators, commas, whitespace, non-ASCII, or
/// a non-letter/underscore first character fall through to the full selector engine.
fn simple_id_selector(selector: &str) -> Option<&str> {
    let id = selector.trim().strip_prefix('#')?;
    let first = id.as_bytes().first().copied()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    if id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Some(id)
    } else {
        None
    }
}

impl DomTree {
    pub fn query_selector(&self, selector: &str) -> Result<Option<NodeId>, String> {
        self.query_selector_from(self.document(), selector)
    }

    pub fn query_selector_all(&self, selector: &str) -> Result<Vec<NodeId>, String> {
        self.query_selector_all_from(self.document(), selector)
    }

    pub fn query_selector_from(&self, root: NodeId, selector: &str) -> Result<Option<NodeId>, String> {
        // Fast path: a bare "#id" selector resolves through the id index in O(1)
        // instead of scanning every descendant. The index holds the first element
        // in tree order per id, which is exactly what the full scan would return.
        // In quirks mode `#id` matches ASCII-case-insensitively, but the id index
        // is keyed on the exact-case id, so skip the fast path and let the
        // selector engine do the case-insensitive match.
        if !self.is_quirks() {
            if let Some(id) = simple_id_selector(selector) {
                match self.get_element_by_id(id) {
                    // querySelector matches strict descendants of root only, so the
                    // indexed element must have root among its ancestors.
                    Some(nid) if self.ancestors(nid).contains(&root) => return Ok(Some(nid)),
                    // No element has this id at all: a bare id selector cannot match.
                    None => return Ok(None),
                    // Indexed (first) element is not under a scoped root; a later
                    // duplicate could still be a descendant, so fall through to scan.
                    Some(_) => {}
                }
            }
        }
        let selector_list = parse_selector(selector)?;
        let mut caches = selectors::context::SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.selector_quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        self.bind_query_scope(&mut context, root);

        for desc_id in self.descendants(root) {
            let is_element = self.with_node(desc_id, |n| n.is_element()).unwrap_or(false);
            if is_element {
                let element = DomElement::new(self, desc_id);
                if selectors::matching::matches_selector_list(
                    &selector_list,
                    &element,
                    &mut context,
                ) {
                    return Ok(Some(desc_id));
                }
            }
        }
        Ok(None)
    }

    // Map the document's quirks flag onto the selector crate's QuirksMode. In
    // quirks mode the crate matches class/id selectors ASCII-case-insensitively.
    fn selector_quirks_mode(&self) -> QuirksMode {
        if self.is_quirks() {
            QuirksMode::Quirks
        } else {
            QuirksMode::NoQuirks
        }
    }

    // `:scope` in a query rooted at an element is that element (the DOM spec's
    // scoping node for querySelector/querySelectorAll). A document-rooted
    // query leaves the scope unset, where the selectors crate falls back to
    // the root element — which is exactly how browsers resolve
    // `document.querySelector(':scope ...')` (it means html).
    fn bind_query_scope(
        &self,
        context: &mut MatchingContext<'_, DitingSelector>,
        root: NodeId,
    ) {
        let is_element = self.with_node(root, |n| n.is_element()).unwrap_or(false);
        if is_element {
            context.scope_element = Some(DomElement::new(self, root).opaque());
        }
    }

    pub fn query_selector_all_from(&self, root: NodeId, selector: &str) -> Result<Vec<NodeId>, String> {
        let selector_list = parse_selector(selector)?;
        let mut caches = selectors::context::SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.selector_quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        self.bind_query_scope(&mut context, root);
        let mut results = Vec::new();

        for desc_id in self.descendants(root) {
            let is_element = self.with_node(desc_id, |n| n.is_element()).unwrap_or(false);
            if is_element {
                let element = DomElement::new(self, desc_id);
                if selectors::matching::matches_selector_list(
                    &selector_list,
                    &element,
                    &mut context,
                ) {
                    results.push(desc_id);
                }
            }
        }
        Ok(results)
    }

    /// Test one element as the selector subject, including when it is
    /// detached from the document (query_selector only walks the tree, so a
    /// detached subject was untestable before this entry point).
    pub fn matches_selector(&self, nid: NodeId, selector: &str) -> Result<bool, String> {
        if !self
            .with_node(nid, |node| node.is_element())
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let selector_list = parse_selector(selector)?;
        let mut caches = selectors::context::SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.selector_quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        // Element.matches() scopes `:scope` to the element being tested —
        // `el.matches(':scope')` is true and is the idiom libraries use to
        // feature-detect :scope support.
        context.scope_element = Some(DomElement::new(self, nid).opaque());
        Ok(selectors::matching::matches_selector_list(
            &selector_list,
            &DomElement::new(self, nid),
            &mut context,
        ))
    }

    /// Parse a single selector once and precompute its specificity — the
    /// only fact the cascade asks of a compiled rule today. Returns `None`
    /// if the selector fails to parse.
    pub fn compile_rule_selector(&self, selector: &str) -> Option<CompiledSelector> {
        let list = parse_selector(selector).ok()?;
        Some(CompiledSelector {
            specificity: list.slice().first()?.specificity(),
        })
    }

    /// Build [`RuleMatchSets`] for one stylesheet's rule selectors: the
    /// per-rule document match sets `compute_styles` needs, without one
    /// full-document querySelectorAll per rule.
    ///
    /// A rule selector can only match an element that carries the
    /// rightmost compound's id/class/tag, so rules are bucketed by that
    /// key and each element's keys probe only the plausible buckets; the
    /// servo matcher then confirms candidates exactly as the
    /// querySelector path would. A selector that fails to parse yields no
    /// bucket and no hits, the "never matches" outcome the old per-rule
    /// qSA error path produced.
    pub fn rule_match_sets(&self, rule_selectors: &[&str]) -> RuleMatchSets {
        // Rule matching is document-rooted (the same scope posture as a
        // document-rooted querySelectorAll): no :scope binding here. Shadow
        // descendants join the probe set so shadow `<style>` rules can match
        // shadow elements — the cascade's global-rule-pool approximation
        // (spec-scoped styles are a v3 concern). Combinators still stop at
        // the shadow root: the matcher climbs ordinary parent links, and a
        // shadow root has none.
        let mut probe_ids: Vec<NodeId> = self.descendants(self.document());
        for root in self.shadow_roots() {
            probe_ids.extend(self.descendants(root));
        }
        self.rule_match_sets_probing(rule_selectors, probe_ids)
    }

    /// Subtree-scoped variant (fabricated iframe documents): probe only the
    /// descendants of `roots`. The document-rooted probe never reaches an
    /// orphan tree, so reusing it there would silently match no rule.
    pub fn rule_match_sets_within(&self, rule_selectors: &[&str], roots: &[NodeId]) -> RuleMatchSets {
        let mut probe_ids: Vec<NodeId> = Vec::new();
        for root in roots {
            probe_ids.extend(self.descendants(*root));
        }
        self.rule_match_sets_probing(rule_selectors, probe_ids)
    }

    fn rule_match_sets_probing(
        &self,
        rule_selectors: &[&str],
        probe_ids: Vec<NodeId>,
    ) -> RuleMatchSets {
        let mut entries: Vec<Option<SelectorList<DitingSelector>>> =
            Vec::with_capacity(rule_selectors.len());
        let mut specificity: Vec<Option<u32>> = Vec::with_capacity(rule_selectors.len());
        let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_class: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_tag: HashMap<String, Vec<usize>> = HashMap::new();
        // Rules where no sub-selector has an id/class/tag key (universal,
        // attr-only, pseudo-only compounds) stay candidates for every
        // element.
        let mut unkeyed: Vec<usize> = Vec::new();
        // In quirks mode class/id matching is ASCII-case-insensitive, so a
        // probe must reach rules keyed in the other case: bucket and probe
        // both spellings there. Over-bucketing is always safe (the matcher
        // has the final word); a bucket the probe cannot reach never is.
        let quirks = self.selector_quirks_mode() == QuirksMode::Quirks;
        let mut pseudo_kinds: HashMap<usize, PseudoKind> = HashMap::new();

        for (ri, selector) in rule_selectors.iter().enumerate() {
            // A trailing pseudo-element suffix routes the rule to the
            // pseudo cascade instead of the normal one. Only a suffix at
            // the very end of a comma-free selector is recognized; anything
            // else (`a::before b`, `a::before:hover`, comma lists) parses
            // with the suffix attached and stays dead — exactly what
            // MatchingMode::Normal did to it before pseudo support.
            let mut base_selector = *selector;
            let sel_trim = selector.trim();
            if !sel_trim.contains(',') {
                let lower = sel_trim.to_ascii_lowercase();
                let stripped = lower
                    .strip_suffix("::before")
                    .map(|rest| (rest.len(), PseudoKind::Before))
                    .or_else(|| {
                        lower
                            .strip_suffix("::after")
                            .map(|rest| (rest.len(), PseudoKind::After))
                    })
                    .or_else(|| {
                        lower
                            .strip_suffix(":before")
                            .map(|rest| (rest.len(), PseudoKind::Before))
                    })
                    .or_else(|| {
                        lower
                            .strip_suffix(":after")
                            .map(|rest| (rest.len(), PseudoKind::After))
                    });
                // to_ascii_lowercase is byte-for-byte, so slicing the
                // original by the lowered length keeps the author's case.
                if let Some((base_len, kind)) = stripped {
                    if let Some(base) = sel_trim.get(..base_len) {
                        base_selector = base;
                        pseudo_kinds.insert(ri, kind);
                    }
                }
            }
            let Ok(list) = parse_selector(base_selector) else {
                entries.push(None);
                specificity.push(None);
                continue;
            };
            specificity.push(list.slice().first().map(|s| s.specificity()));
            // A comma list matches if ANY sub-selector matches, so every
            // sub-selector contributes its own bucket entry.
            let mut bucketed = false;
            for sel in list.slice() {
                match rightmost_key(sel) {
                    Some(RuleKey::Id(id)) => {
                        by_id.entry(id.clone()).or_default().push(ri);
                        if quirks {
                            by_id.entry(id.to_ascii_lowercase()).or_default().push(ri);
                        }
                        bucketed = true;
                    }
                    Some(RuleKey::Class(class)) => {
                        by_class.entry(class.clone()).or_default().push(ri);
                        if quirks {
                            by_class
                                .entry(class.to_ascii_lowercase())
                                .or_default()
                                .push(ri);
                        }
                        bucketed = true;
                    }
                    Some(RuleKey::Tag(tag)) => {
                        by_tag.entry(tag).or_default().push(ri);
                        bucketed = true;
                    }
                    None => {}
                }
            }
            if !bucketed {
                unkeyed.push(ri);
            }
            entries.push(Some(list));
        }

        let mut hits: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut pseudo_hits: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut caches = selectors::context::SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.selector_quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        let mut candidates: Vec<usize> = Vec::new();
        for desc_id in probe_ids {
            let Some((local, id, class)) = self
                .with_node(desc_id, |n| {
                    let name = n.as_element()?;
                    Some((
                        name.local.as_ref().to_string(),
                        n.get_attribute("id").map(|s| s.to_string()),
                        n.get_attribute("class").map(|s| s.to_string()),
                    ))
                })
                .flatten()
            else {
                continue;
            };
            candidates.clear();
            candidates.extend_from_slice(&unkeyed);
            let mut probe = |bucket: &HashMap<String, Vec<usize>>, key: &str| {
                if let Some(rules) = bucket.get(key) {
                    candidates.extend_from_slice(rules);
                }
            };
            // Tag selectors bucket under the parser's lower_name; probing
            // the element's lowercased local name is the shared spelling.
            // Foreign-element tags keep camelCase local names in the DOM
            // (SVG clipPath and friends), and lowercasing the probe covers
            // them too.
            probe(&by_tag, &local.to_ascii_lowercase());
            if let Some(id) = &id {
                probe(&by_id, id);
                if quirks {
                    probe(&by_id, &id.to_ascii_lowercase());
                }
            }
            if let Some(class) = &class {
                for c in class.split_whitespace() {
                    probe(&by_class, c);
                    if quirks {
                        probe(&by_class, &c.to_ascii_lowercase());
                    }
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
            let element = DomElement::new(self, desc_id);
            for ri in candidates.drain(..) {
                let Some(list) = entries[ri].as_ref() else { continue };
                if selectors::matching::matches_selector_list(list, &element, &mut context) {
                    if pseudo_kinds.contains_key(&ri) {
                        pseudo_hits.entry(ri).or_default().push(desc_id.index());
                    } else {
                        hits.entry(ri).or_default().push(desc_id.index());
                    }
                }
            }
        }
        // The walk is document order, not node-index order; the cascade
        // binary-searches these, so each rule's hit set ends ascending.
        for rule_hits in hits.values_mut().chain(pseudo_hits.values_mut()) {
            rule_hits.sort_unstable();
        }
        RuleMatchSets {
            hits,
            specificity,
            pseudo_kinds,
            pseudo_hits,
        }
    }
}

/// A parsed rule selector reduced to its specificity — the only fact the
/// cascade asks of a compiled rule today.
pub struct CompiledSelector {
    specificity: u32,
}

impl CompiledSelector {
    pub fn specificity(&self) -> u32 {
        self.specificity
    }
}

/// The rightmost-compound key a rule selector's subject element must
/// carry, ranked by selectivity (id > class > tag). Extracted per
/// sub-selector for [`DomTree::rule_match_sets`]'s buckets; a compound
/// with none of the three keys nothing, so its rule is tested against
/// every element.
enum RuleKey {
    Tag(String),
    Class(String),
    Id(String),
}

impl RuleKey {
    fn rank(&self) -> u8 {
        match self {
            RuleKey::Tag(_) => 0,
            RuleKey::Class(_) => 1,
            RuleKey::Id(_) => 2,
        }
    }
}

/// Extract the key from one parsed selector's rightmost compound. Servo's
/// `Selector::iter()` yields components right-to-left starting at the
/// rightmost compound; the iterator stops at the compound boundary, which
/// is exactly the span that constrains the subject element (anything left
/// of a combinator describes ancestors, not the subject).
fn rightmost_key(selector: &parser::Selector<DitingSelector>) -> Option<RuleKey> {
    let mut best: Option<RuleKey> = None;
    for component in selector.iter() {
        let key = match component {
            parser::Component::ID(id) => Some(RuleKey::Id(id.0.clone())),
            parser::Component::Class(class) => Some(RuleKey::Class(class.0.clone())),
            // The DOM-side probe reaches this bucket through the element's
            // lowercased local name, so the parser's lower_name is the
            // spelling both sides agree on.
            parser::Component::LocalName(name) => Some(RuleKey::Tag(name.lower_name.0.to_string())),
            _ => None,
        };
        if let Some(key) = key {
            if best.as_ref().is_none_or(|b| key.rank() > b.rank()) {
                best = Some(key);
            }
        }
    }
    best
}

/// Chrome-style rule-hash match sets for stylesheet application, built by
/// [`DomTree::rule_match_sets`].
///
/// `compute_styles` used to precompute each rule's document match set
/// with one full-document `querySelectorAll` per rule: O(rules x docsize)
/// selector matches per layout run, re-paid on every epoch bump. On the
/// WeChat article pages (6912 rules, 483 elements, 3 MB of CSS) that
/// phase alone was ~3.7s of EVERY layout run, and page scripts'
/// write-then-read layout thrashing re-triggered it several times per
/// navigation, the engine-side root cause behind the appmsg.js
/// synchronous "dead spin" that blew the nav deadline (a V8 terminate
/// cannot land inside a long Rust phase).
pub struct RuleMatchSets {
    /// Rule index to matching element node indices, ascending: ready for
    /// the cascade's per-element binary-search membership test (which the
    /// old code documented but then did linearly).
    pub hits: HashMap<usize, Vec<usize>>,
    /// Rule index to the rule selector's specificity, parsed once here
    /// instead of re-parsed per matched element (the old cascade called
    /// `compile_rule_selector` per element x matched rule).
    pub specificity: Vec<Option<u32>>,
    /// Pseudo-element rules (`div::before { ... }`), keyed by rule index.
    /// A rule carries at most one kind (the suffix sits on the single
    /// selector), and its hits live in [`RuleMatchSets::pseudo_hits`] so
    /// the normal cascade never applies the base compound to real elements.
    pub pseudo_kinds: HashMap<usize, PseudoKind>,
    /// Pseudo-element rule hit sets, same shape as `hits` but keyed by the
    /// rule indexes present in `pseudo_kinds`.
    pub pseudo_hits: HashMap<usize, Vec<usize>>,
}

/// The pseudo-element suffix a rule selector may end with. Single-colon
/// legacy spellings (`:before`/`:after`) fold into the same kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoKind {
    Before,
    After,
}

impl PseudoKind {
    pub fn css_name(self) -> &'static str {
        match self {
            PseudoKind::Before => "::before",
            PseudoKind::After => "::after",
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::diting_dom::tree_sink::parse_html;

    #[test]
    fn test_query_selector_tag() {
        let tree = parse_html("<html><body><h1>Title</h1><p>Text</p></body></html>");
        let result = tree.query_selector("h1").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.as_element().unwrap().local.as_ref(), "h1");
    }

    #[test]
    fn test_query_selector_class() {
        let tree =
            parse_html(r#"<div class="foo bar">Content</div><div class="baz">Other</div>"#);
        let result = tree.query_selector(".foo").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.get_attribute("class"), Some("foo bar"));
    }

    #[test]
    fn test_query_selector_id() {
        let tree = parse_html(r#"<div id="main">Content</div>"#);
        let result = tree.query_selector("#main").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_query_selector_all() {
        let tree = parse_html("<ul><li>1</li><li>2</li><li>3</li></ul>");
        let results = tree.query_selector_all("li").unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_query_selector_descendant() {
        let tree =
            parse_html(r#"<div id="outer"><div id="inner"><span>Target</span></div></div>"#);
        let result = tree.query_selector("#outer span").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.as_element().unwrap().local.as_ref(), "span");
    }

    #[test]
    fn test_query_selector_attribute() {
        let tree = parse_html(
            r#"<input type="text" name="user"><input type="password" name="pass">"#,
        );
        let result = tree.query_selector(r#"input[type="password"]"#).unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.get_attribute("name"), Some("pass"));
    }

    #[test]
    fn test_query_selector_no_match() {
        let tree = parse_html("<div>Hello</div>");
        let result = tree.query_selector("span").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_query_selector_complex() {
        let tree = parse_html(
            r#"<div class="container">
                <ul class="list">
                    <li class="item active">First</li>
                    <li class="item">Second</li>
                    <li class="item active">Third</li>
                </ul>
            </div>"#,
        );
        let results = tree.query_selector_all(".list .item.active").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_selector_all_from_scopes_to_subtree() {
        let tree = parse_html(
            r#"<div id="a"><span class="x">in a</span></div><div id="b"><span class="x">in b</span></div>"#,
        );
        let a = tree.get_element_by_id("a").expect("div#a");
        let b = tree.get_element_by_id("b").expect("div#b");

        // Document-rooted: sees both spans.
        assert_eq!(tree.query_selector_all(".x").unwrap().len(), 2);
        // Scoped to #a: only its descendant span.
        let in_a = tree.query_selector_all_from(a, ".x").unwrap();
        assert_eq!(in_a.len(), 1);
        let in_b = tree.query_selector_all_from(b, ".x").unwrap();
        assert_eq!(in_b.len(), 1);
        assert_ne!(in_a[0], in_b[0]);
    }

    #[test]
    fn test_query_selector_from_returns_first_in_subtree_only() {
        let tree = parse_html(
            r#"<section id="s"><p>first</p><p>second</p></section><p>outside</p>"#,
        );
        let s = tree.get_element_by_id("s").expect("section#s");

        // Scoped to #s: skip the outside paragraph; return the first inside.
        let first_in_s = tree.query_selector_from(s, "p").unwrap().expect("a p inside");
        assert_eq!(tree.text_content(first_in_s), "first");
    }

    #[test]
    fn test_query_selector_from_excludes_self() {
        // The root element itself must not match its own scoped query, per
        // the spec: querySelector matches descendants only.
        let tree = parse_html(r#"<div id="root" class="x"><span>child</span></div>"#);
        let root = tree.get_element_by_id("root").expect("div#root");

        // Only descendants are candidates: `.x` on root finds nothing.
        assert!(tree.query_selector_from(root, ".x").unwrap().is_none());
        // `span` finds the child.
        assert!(tree.query_selector_from(root, "span").unwrap().is_some());
    }

    #[test]
    fn test_scope_child_combinator_scoped_to_query_root() {
        // The archify viewer pattern: container.querySelector(':scope > svg')
        // must find the container's direct svg child — not svgs in a sibling
        // container, not svgs deeper inside.
        let tree = parse_html(
            r#"<html><body>
                <div id="a"><svg id="direct"></svg><div><svg id="deep"></svg></div></div>
                <div id="b"><svg id="other"></svg></div>
            </body></html>"#,
        );
        let a = tree.get_element_by_id("a").unwrap();
        let hit = tree
            .query_selector_from(a, ":scope > svg")
            .unwrap()
            .expect("direct svg child of #a");
        assert_eq!(
            tree.get_element_by_id("direct"),
            Some(hit),
            "must be the direct child, not the deep or sibling svg"
        );

        let direct_only = tree.query_selector_all_from(a, ":scope > svg").unwrap();
        assert_eq!(direct_only.len(), 1);
    }

    #[test]
    fn test_scope_descendant_combinator_and_bare_form() {
        let tree = parse_html(
            r#"<html><body>
                <div id="a"><span class="x"></span><section><span class="x"></span></section></div>
                <span class="x"></span>
            </body></html>"#,
        );
        let a = tree.get_element_by_id("a").unwrap();
        // `:scope .x` = every .x in the subtree (both depths)...
        assert_eq!(tree.query_selector_all_from(a, ":scope .x").unwrap().len(), 2);
        // ...while a document-rooted query sees all three.
        assert_eq!(tree.query_selector_all(":scope .x").unwrap().len(), 3);

        // Bare `:scope` from an element root matches nothing: the scope
        // element itself is not among its own descendants.
        assert!(tree.query_selector_from(a, ":scope").unwrap().is_none());
        assert!(tree.query_selector_all_from(a, ":scope").unwrap().is_empty());
    }

    #[test]
    fn test_scope_from_document_root_means_html() {
        // Document-rooted queries leave the scope unset, where :scope falls
        // back to the root element — the browser behavior for
        // document.querySelector(':scope ...').
        let tree = parse_html(r#"<html><body><svg id="top"></svg><div><svg id="nested"></svg></div></body></html>"#);
        assert_eq!(
            tree.query_selector(":scope").unwrap(),
            tree.query_selector("html").unwrap(),
            "bare :scope from the document is the document element"
        );
        let hit = tree
            .query_selector(":scope > body")
            .unwrap()
            .expect("body is the document element's direct child");
        assert_eq!(
            tree.query_selector("body").unwrap(),
            Some(hit),
            ":scope from the document means html, so :scope > body resolves"
        );
    }

    #[test]
    fn test_matches_selector_binds_scope_to_element() {
        // Element.matches semantics: :scope is the tested element.
        let tree = parse_html(r#"<html><body><div id="hit" class="panel"><span></span></div></body></html>"#);
        let hit = tree.get_element_by_id("hit").unwrap();
        assert!(tree.matches_selector(hit, ":scope").unwrap());
        assert!(tree.matches_selector(hit, ":scope.panel").unwrap());
        assert!(!tree.matches_selector(hit, ":scope.missing").unwrap());
        // With :scope bound to the tested element, a child combinator off it
        // can only be false: nothing is its own parent. `span.matches
        // (':scope > span')` asks whether span's parent is span.
        let span = tree.query_selector("span").unwrap().expect("the span");
        assert!(!tree.matches_selector(span, ":scope > span").unwrap());
    }

    #[test]
    fn test_quirks_mode_class_id_case_insensitive() {
        // No doctype => full quirks mode: class and id selectors match
        // ASCII-case-insensitively (legacy pages depend on this).
        let quirks = parse_html(r#"<div class="Foo" id="Main">x</div>"#);
        assert!(quirks.is_quirks(), "no-doctype document must parse as quirks");
        assert!(quirks.query_selector(".foo").unwrap().is_some());
        assert!(quirks.query_selector(".FOO").unwrap().is_some());
        assert!(quirks.query_selector("#main").unwrap().is_some());

        // With a doctype => no-quirks: class/id match case-sensitively.
        let strict = parse_html(
            r#"<!DOCTYPE html><html><body><div class="Foo" id="Main">x</div></body></html>"#,
        );
        assert!(!strict.is_quirks());
        assert!(strict.query_selector(".Foo").unwrap().is_some());
        assert!(strict.query_selector(".foo").unwrap().is_none());
        assert!(strict.query_selector("#main").unwrap().is_none());
    }

    #[test]
    fn test_enabled_disabled_checked_match_dom_state() {
        let tree = parse_html(
            r#"<form>
                <input type="text" name="a">
                <input type="text" name="b" disabled>
                <input type="checkbox" name="c" checked>
                <button name="d">go</button>
                <select name="e"><option selected>x</option><option>y</option></select>
            </form>"#,
        );

        let enabled = tree.query_selector_all("input:enabled").unwrap();
        assert_eq!(enabled.len(), 2, "two non-disabled inputs: {enabled:?}");
        let disabled = tree.query_selector_all("input:disabled").unwrap();
        assert_eq!(disabled.len(), 1);
        let checked = tree.query_selector_all(":checked").unwrap();
        assert_eq!(checked.len(), 2, "checkbox[checked] + option[selected]: {checked:?}");

        // :enabled on a non-form-control never matches.
        let tree2 = parse_html(r#"<div>plain</div>"#);
        assert!(tree2.query_selector("div:enabled").unwrap().is_none());
    }

    #[test]
    fn test_has_parses() {
        // Isolate parse from match: :has must parse, not error.
        assert!(
            super::parse_selector("a:has(p.bt)").is_ok(),
            ":has failed to parse"
        );
    }

    #[test]
    fn test_query_selector_has() {
        let tree = parse_html(r#"<a><p class="bt">x</p></a>"#);
        let all = tree.query_selector_all("a:has(p.bt)").unwrap();
        assert_eq!(all.len(), 1, "a:has(p.bt) should match the <a>");
        let none = tree.query_selector_all("a:has(span.bt)").unwrap();
        assert_eq!(none.len(), 0, "a:has(span.bt) should match nothing");
    }

    #[test]
    fn test_query_selector_has_deep_and_bare_relative() {
        // :has with a descendant combinator several levels deep, and the bare
        // relative form (no leading combinator == descendant). Nested :has is
        // spec-invalid (:has(:has())) and the selectors crate rejects it.
        let tree = parse_html(
            r#"<section><article><div><ul><li class="hit">x</li></ul></div></article></section>"#,
        );
        assert_eq!(tree.query_selector_all("section:has(.hit)").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all("article:has(li)").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all("div:has(> ul > .hit)").unwrap().len(), 1);
        assert!(super::parse_selector("section:has(article:has(li))").is_err());
    }

    #[test]
    fn test_is_and_where_parse_and_match() {
        // Tailwind preflight / modern resets wrap rules in :where(...).
        let tree = parse_html(
            r#"<button class="btn">go</button><input class="field"><div class="btn-like">x</div>"#,
        );
        assert_eq!(
            tree.query_selector_all(":where(button, input)").unwrap().len(),
            2
        );
        assert_eq!(
            tree.query_selector_all(":is(button, input)").unwrap().len(),
            2
        );
        // Compound subject: .control:is(...) narrows by both parts.
        assert_eq!(
            tree.query_selector_all(r#"input:is([class="field"])"#.trim_end_matches('"')).is_err(),
            false
        );
        assert_eq!(tree.query_selector_all("button:where(.btn)").unwrap().len(), 1);
    }

    #[test]
    fn focus_state_pseudo_classes_parse_and_match_static_snapshot() {
        // Bootstrap's .visually-hidden-focusable pattern: :not(:focus):not(:focus-within)
        // must MATCH when nothing holds focus (both pseudos are false).
        let tree = parse_html(r#"<div class="visually-hidden-focusable">Skip</div>"#);
        let hidden = tree
            .query_selector_all(".visually-hidden-focusable:not(:focus):not(:focus-within)")
            .unwrap();
        assert_eq!(hidden.len(), 1);
        assert_eq!(tree.query_selector_all(":focus-visible").unwrap().len(), 0);
    }

    /// The live side of the focus pseudos (blitz#839): the tree's focused
    /// node drives :focus, :focus-within climbs ancestors, and
    /// :focus-visible narrows to text-entry controls (input type=text
    /// yes, type=checkbox no).
    #[test]
    fn focus_pseudo_classes_match_live_focused_node() {
        let tree = parse_html(
            r#"<form><input id="q" type="text"><input id="c" type="checkbox"><button id="b">Go</button></form>"#,
        );
        let q = tree.get_element_by_id("q").unwrap();
        tree.set_focused_node(Some(q));
        assert_eq!(tree.query_selector_all(":focus").unwrap().len(), 1);
        assert_eq!(
            tree.query_selector_all("#q:focus-visible").unwrap().len(),
            1
        );
        // :focus-within reaches the form even though the form itself isn't focused.
        assert_eq!(
            tree.query_selector_all("form:focus-within").unwrap().len(),
            1
        );

        // A checkbox holds focus → :focus matches, but :focus-visible doesn't
        // (mouse-ish control per the heuristic).
        let c = tree.get_element_by_id("c").unwrap();
        tree.set_focused_node(Some(c));
        assert_eq!(tree.query_selector_all("#c:focus").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all(":focus-visible").unwrap().len(), 0);

        // Blur → every focus pseudo is false again.
        tree.set_focused_node(None);
        assert_eq!(tree.query_selector_all(":focus").unwrap().len(), 0);
        assert_eq!(
            tree.query_selector_all("form:focus-within").unwrap().len(),
            0
        );
    }

    #[test]
    fn link_pseudo_class_matches_anchor_with_href() {
        let tree = parse_html(
            r#"<a href="https://x/">real</a><a name="anchor-only">not</a><a>bare</a>"#,
        );
        let links = tree.query_selector_all(":link").unwrap();
        assert_eq!(links.len(), 1, ":link needs an href: {links:?}");
        assert_eq!(tree.query_selector_all("a:any-link").unwrap().len(), 1);
        // :visited is always false on a static snapshot.
        assert_eq!(tree.query_selector_all(":visited").unwrap().len(), 0);
    }

    #[test]
    fn matches_selector_works_on_detached_subjects() {
        // query_selector only walks the tree, so a detached subject was
        // untestable before matches_selector existed.
        let detached = parse_html("<em id='d'>d</em>");
        let d = detached.get_element_by_id("d").unwrap();
        assert!(detached.matches_selector(d, "em#d").unwrap());
        assert!(!detached.matches_selector(d, "div em").unwrap());
    }

    /// The rule-hash must be indistinguishable from the old per-rule
    /// querySelectorAll on every selector shape compute_styles feeds it:
    /// per-rule hit sets AND per-rule specificity, rule index by rule
    /// index. parse_html fixtures without a doctype parse in quirks mode,
    /// so the case-folded selectors in the list also pin the quirks
    /// bucket/probe spellings against the matcher's ground truth.
    #[test]
    fn rule_match_sets_agree_with_per_rule_qsa() {
        let tree = parse_html(
            r#"<div id="wrap" class="container">
                <h1 class="title main">T</h1>
                <p data-kind="lead">L</p>
                <p>plain</p>
                <ul><li class="item">1</li><li>2</li></ul>
                <svg><clipPath id="cp"><rect/></clipPath></svg>
                <span>s</span>
            </div>"#,
        );
        let selectors = [
            "div",
            ".container",
            "#wrap",
            "p",
            ".title.main",
            "div .item",
            "ul li",
            "h1, .item",
            "li, rect, #cp",
            "*",
            "ul *",
            "[data-kind]",
            "p[data-kind='lead']",
            "li:not(.item)",
            "li:first-child",
            "clipPath",
            "#nope",
            ".container, .missing",
            "p[lang]",
            // Quirks-mode case folds: whichever way the matcher answers,
            // the hash must answer identically.
            ".ITEM",
            "#WRAP",
            // Dangling combinator: fails to parse, so no bucket, no hits,
            // no specificity — same as the old qSA error path skipping it.
            "div >",
        ];
        let sets = tree.rule_match_sets(&selectors);
        for (ri, sel) in selectors.iter().enumerate() {
            let expected: Vec<usize> = tree
                .query_selector_all_from(tree.document(), sel)
                .map(|v| v.into_iter().map(|n| n.index()).collect())
                .unwrap_or_default();
            let got = sets.hits.get(&ri).cloned().unwrap_or_default();
            assert_eq!(got, expected, "hit set diverged for rule {ri} `{sel}`");
            // Ascending order is the cascade's binary-search contract.
            assert!(
                got.windows(2).all(|w| w[0] < w[1]),
                "hits not ascending for rule {ri} `{sel}`: {got:?}"
            );
            assert_eq!(
                sets.specificity[ri],
                tree.compile_rule_selector(sel).map(|c| c.specificity()),
                "specificity diverged for rule {ri} `{sel}`"
            );
        }
    }

    /// Rules whose rightmost compound has no id/class/tag key (universal,
    /// attr-only, pseudo-only) fall into the always-tested bucket — the
    /// one place the hash could silently drop matches if the fallback
    /// regressed.
    #[test]
    fn rule_match_sets_unkeyed_rules_still_match() {
        let tree = parse_html(r#"<div><p data-x="1">a</p><p>b</p><span>c</span></div>"#);
        let selectors = ["[data-x]", "*:not(span)", "[hidden]", "*"];
        let sets = tree.rule_match_sets(&selectors);
        let expected: Vec<usize> = tree
            .query_selector_all_from(tree.document(), "[data-x]")
            .unwrap()
            .into_iter()
            .map(|n| n.index())
            .collect();
        let got = sets.hits.get(&0).cloned().unwrap_or_default();
        assert_eq!(got, expected, "attr-only rule lost its match");
        // `*` matches every element: html, head, body included.
        let star = sets.hits.get(&3).cloned().unwrap_or_default();
        assert!(star.len() >= 5, "universal rule matched only {star:?}");
        // Never-matching unkeyed rule contributes nothing.
        assert!(sets.hits.get(&2).is_none());
    }

    // ---- pseudo-element rule routing (::before/::after v1) ----
    use super::PseudoKind;

    #[test]
    fn pseudo_element_rules_route_out_of_normal_hits() {
        let tree = parse_html(r#"<div class="row">x</div><p class="row">y</p>"#);
        let sets = tree.rule_match_sets(&[".row:after"]);
        assert_eq!(sets.pseudo_kinds.get(&0), Some(&PseudoKind::After));
        assert_eq!(sets.pseudo_hits.get(&0).map(|v| v.len()), Some(2));
        assert!(
            !sets.hits.contains_key(&0),
            "pseudo rule must never reach the normal cascade"
        );
    }

    #[test]
    fn pseudo_suffix_spellings_and_case() {
        let tree = parse_html(r#"<ul><li id="a">one</li><li id="b">two</li></ul>"#);
        let sets = tree.rule_match_sets(&["li::before", "li:after", ".LI:BEFORE"]);
        assert_eq!(sets.pseudo_kinds.get(&0), Some(&PseudoKind::Before));
        assert_eq!(sets.pseudo_kinds.get(&1), Some(&PseudoKind::After));
        assert_eq!(sets.pseudo_hits.get(&0).map(|v| v.len()), Some(2));
        // The suffix matches case-insensitively but the base keeps the
        // author's case, so class matching stays case-sensitive: routing
        // is orthogonal to matching.
        assert_eq!(sets.pseudo_kinds.get(&2), Some(&PseudoKind::Before));
        assert!(sets.pseudo_hits.get(&2).is_none(), ".LI matches nothing");
    }

    #[test]
    fn comma_lists_and_non_trailing_suffixes_stay_dead() {
        let tree = parse_html(r##"<a id="x" href="#">l</a><b class="b">t</b>"##);
        let sets = tree.rule_match_sets(&["a::before, b", ".b a::before:hover", "a::before .b"]);
        assert!(
            sets.pseudo_kinds.is_empty(),
            "comma list / non-trailing suffix must not route"
        );
        assert!(sets.pseudo_hits.is_empty());
        assert!(!sets.hits.contains_key(&0), "comma'd pseudo list never matches real elements");
        assert!(!sets.hits.contains_key(&2), "descendant-of-pseudo never matches real elements");
    }
}
