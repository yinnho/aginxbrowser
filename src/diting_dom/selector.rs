use cssparser::{CowRcStr, ToCss};
use html5ever::{namespace_url, ns, LocalName, Namespace};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::context::QuirksMode;
use selectors::matching::{
    ElementSelectorFlags, MatchingContext, MatchingForInvalidation, MatchingMode,
    NeedsSelectorFlags,
};
use selectors::parser::{self, ParseRelative, SelectorParseErrorKind};
use selectors::{Element, OpaqueElement, SelectorList};
use selectors::visitor::SelectorVisitor;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
}

impl parser::PseudoElement for PseudoElement {
    type Impl = DitingSelector;
}

impl ToCss for PseudoElement {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        match self {
            PseudoElement::Before => dest.write_str("::before"),
            PseudoElement::After => dest.write_str("::after"),
        }
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
            // Dynamic user-interaction pseudo-classes have no meaning against
            // a static DOM snapshot with no live user input.
            PseudoClass::Hover
            | PseudoClass::Active
            | PseudoClass::Focus
            | PseudoClass::FocusVisible
            | PseudoClass::FocusWithin => false,
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

    fn add_element_unique_hashes(&self, _filter: &mut BloomFilter) -> bool {
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
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        Ok(selectors::matching::matches_selector_list(
            &selector_list,
            &DomElement::new(self, nid),
            &mut context,
        ))
    }

    /// Parse a single selector once for repeated single-element matching, and
    /// precompute its specificity, ancestor hashes, and "subject key" (the
    /// rightmost id/class/attribute/tag used to bucket the rule for fast candidate
    /// lookup). Returns `None` if the selector fails to parse.
    ///
    /// This is the primitive a stylesheet cascade builds on: compile every rule
    /// once, index by key, then only test the handful of candidate rules against
    /// each element instead of scanning the whole tree per rule.
    pub fn compile_rule_selector(&self, selector: &str) -> Option<CompiledSelector> {
        let list = parse_selector(selector).ok()?;
        let sel = list.slice().first()?.clone();
        let specificity = sel.specificity();
        let keys = SubjectKeys::from_vec(subject_keys(&sel));
        let hashes = parser::AncestorHashes::new(&sel, QuirksMode::NoQuirks);
        Some(CompiledSelector {
            sel,
            specificity,
            keys,
            hashes,
        })
    }

    /// Does a single element match a precompiled selector? Allocates a fresh
    /// match cache each call; for many matches in a row use [`DomTree::matcher`].
    pub fn element_matches(&self, nid: NodeId, compiled: &CompiledSelector) -> bool {
        self.matcher().matches(self, nid, compiled)
    }

    /// A reusable matcher that holds the selector caches and an ancestor bloom
    /// filter, so a cascade can fast-reject most (element, rule) pairs without
    /// walking the selector's combinators. Reuse one across a whole cascade
    /// pass and drive [`Matcher::push_ancestor`] / [`Matcher::pop_ancestor`] as
    /// you descend/ascend the tree so the filter tracks the current path.
    pub fn matcher(&self) -> Matcher {
        Matcher {
            caches: selectors::context::SelectorCaches::default(),
            ancestors: AncestorFilter::new(),
            candidate_generation: 0,
            candidate_seen: Vec::new(),
        }
    }
}

/// Holds the servo selector match caches plus an incremental ancestor bloom
/// filter so a treewalk cascade can test thousands of (element, rule) pairs
/// cheaply: most rules are fast-rejected by the filter before any combinator
/// walking happens.
pub struct Matcher {
    caches: selectors::context::SelectorCaches,
    ancestors: AncestorFilter,
    candidate_generation: u32,
    candidate_seen: Vec<u32>,
}

impl Matcher {
    /// Start one indexed-candidate collection without clearing the reusable
    /// seen table. Stylesheet rules that live in several selector buckets use
    /// this to avoid matching and cascading the same rule twice.
    #[doc(hidden)]
    pub fn begin_candidate_collection(&mut self, candidate_count: usize) {
        self.candidate_generation = self.candidate_generation.wrapping_add(1);
        if self.candidate_generation == 0 {
            self.candidate_seen.fill(0);
            self.candidate_generation = 1;
        }
        self.candidate_seen.resize(candidate_count, 0);
    }

    /// Return true once per candidate index in the current collection.
    #[doc(hidden)]
    pub fn mark_candidate(&mut self, index: usize) -> bool {
        debug_assert!(index < self.candidate_seen.len());
        let Some(seen) = self.candidate_seen.get_mut(index) else {
            // Candidate collection is an optimization. If a future caller
            // supplies inconsistent bounds, fail open to an extra match rather
            // than silently dropping authored CSS.
            return true;
        };
        if *seen == self.candidate_generation {
            return false;
        }
        *seen = self.candidate_generation;
        true
    }

    /// Match `nid` (matched as if it were the subject element; the ancestor
    /// filter reflects `nid`'s ancestors, not `nid` itself) against `compiled`.
    pub fn matches(&mut self, tree: &DomTree, nid: NodeId, compiled: &CompiledSelector) -> bool {
        if !tree.with_node(nid, |n| n.is_element()).unwrap_or(false) {
            return false;
        }
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            Some(self.ancestors.filter()),
            &mut self.caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        let element = DomElement::new(tree, nid);
        selectors::matching::matches_selector(
            &compiled.sel,
            0,
            Some(&compiled.hashes),
            &element,
            &mut context,
        )
    }

    /// Push `nid`'s hashes onto the ancestor filter before descending into its
    /// children. Must be paired with a matching [`Matcher::pop_ancestor`].
    pub fn push_ancestor(&mut self, tree: &DomTree, nid: NodeId) {
        self.ancestors.push(tree, nid);
    }

    /// Pop the hashes pushed by the most recent unmatched [`Matcher::push_ancestor`].
    pub fn pop_ancestor(&mut self) {
        self.ancestors.pop();
    }
}

/// An incremental ancestor bloom filter (the same structure browsers use to
/// fast-reject selector matches during a treewalk). `push` is called with each
/// element on the way down the tree, `pop` on the way back up, so at any point
/// the filter holds exactly the hashes of the current node's ancestor chain.
struct AncestorFilter {
    filter: Box<selectors::bloom::BloomFilter>,
    /// Hashes pushed per tree-depth level, so `pop` knows what to remove.
    levels: Vec<Vec<u32>>,
}

impl AncestorFilter {
    fn new() -> Self {
        AncestorFilter {
            filter: Box::default(),
            levels: Vec::new(),
        }
    }

    fn filter(&self) -> &selectors::bloom::BloomFilter {
        &self.filter
    }

    fn push(&mut self, tree: &DomTree, nid: NodeId) {
        let mut hashes = Vec::new();
        tree.with_node(nid, |node| {
            if let Some(elem) = node.as_element() {
                push_hash(
                    &mut hashes,
                    &mut self.filter,
                    CssLocalName(elem.local.clone()).precomputed_hash(),
                );
                if let Some(id) = node.get_attribute("id") {
                    push_hash(
                        &mut hashes,
                        &mut self.filter,
                        CssString(id.to_string()).precomputed_hash(),
                    );
                }
                if let Some(class) = node.get_attribute("class") {
                    for c in class.split_whitespace() {
                        push_hash(
                            &mut hashes,
                            &mut self.filter,
                            CssString(c.to_string()).precomputed_hash(),
                        );
                    }
                }
            }
        });
        self.levels.push(hashes);
    }

    fn pop(&mut self) {
        if let Some(hashes) = self.levels.pop() {
            for h in hashes {
                self.filter.remove_hash(h);
            }
        }
    }
}

fn push_hash(hashes: &mut Vec<u32>, filter: &mut selectors::bloom::BloomFilter, hash: u32) {
    let masked = hash & selectors::bloom::BLOOM_HASH_MASK;
    filter.insert_hash(masked);
    hashes.push(masked);
}

/// The rightmost simple selector used to index a rule for fast lookup. A rule is
/// only tested against elements that carry its key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SelectorKey {
    /// The document element. `:root` can match only this one subject, so it is
    /// substantially more selective than the universal bucket despite having
    /// no id/class/tag token.
    Root,
    Id(String),
    Class(String),
    Attribute(String),
    Local(String),
    Universal,
}

/// A parsed selector plus its cached specificity and subject key.
pub struct CompiledSelector {
    sel: parser::Selector<DitingSelector>,
    specificity: u32,
    keys: SubjectKeys,
    hashes: parser::AncestorHashes,
}

impl CompiledSelector {
    /// The one bucket usable by legacy selector indexes. Selectors whose
    /// subject has multiple disjoint alternatives deliberately report
    /// `Universal`; use [`Self::candidate_keys`] to opt into multi-bucket
    /// indexing.
    pub fn key(&self) -> &SelectorKey {
        match &self.keys {
            SubjectKeys::One(key) => key,
            // Existing single-bucket consumers must keep treating disjoint
            // alternatives as universal until they opt into `candidate_keys`.
            SubjectKeys::Many(_) => &SelectorKey::Universal,
        }
    }

    /// Deduplicated buckets that together cover every element this selector
    /// can match. A consumer must insert the rule in every returned bucket and
    /// deduplicate rule candidates before matching, since one element can
    /// carry keys from more than one alternative.
    pub fn candidate_keys(&self) -> &[SelectorKey] {
        match &self.keys {
            SubjectKeys::One(key) => std::slice::from_ref(key),
            SubjectKeys::Many(keys) => keys,
        }
    }
    pub fn specificity(&self) -> u32 {
        self.specificity
    }
}

enum SubjectKeys {
    One(SelectorKey),
    Many(Box<[SelectorKey]>),
}

impl SubjectKeys {
    fn from_vec(mut keys: Vec<SelectorKey>) -> Self {
        if keys.len() == 1 {
            Self::One(keys.pop().unwrap())
        } else {
            Self::Many(keys.into_boxed_slice())
        }
    }
}

fn key_rank(key: &SelectorKey) -> u8 {
    match key {
        SelectorKey::Universal => 0,
        SelectorKey::Local(_) => 1,
        SelectorKey::Attribute(_) => 2,
        SelectorKey::Class(_) => 3,
        SelectorKey::Id(_) => 4,
        SelectorKey::Root => 5,
    }
}

fn push_unique_key(keys: &mut Vec<SelectorKey>, key: SelectorKey) {
    if !keys.contains(&key) {
        keys.push(key);
    }
}

/// Candidate buckets for a selector's rightmost compound. A single ordinary
/// id/class/local selector keeps the allocation-free common path. A pure
/// `:is()`/`:where()` subject returns the deduplicated union of its arms, but
/// only when every arm has a concrete key; one universal/pseudo-only
/// arm makes the whole set universal because that arm can match an element
/// carrying none of the other keys.
///
/// When an outer compound key exists, disjoint functional-pseudo keys replace
/// it only if every arm is strictly more selective. This is the same
/// conservative contract as Gecko's `SelectorMap::find_bucket`: for
/// `.control:is(button, #save)` keep `.control`, while
/// `.control:is(#save, #cancel)` may use the two id buckets.
fn subject_keys(sel: &parser::Selector<DitingSelector>) -> Vec<SelectorKey> {
    use parser::Component;
    let mut local: Option<String> = None;
    let mut attribute: Option<String> = None;
    let mut class: Option<String> = None;
    let mut id: Option<String> = None;
    let mut root = false;
    let mut disjoint_sets: Vec<Vec<SelectorKey>> = Vec::new();
    for comp in sel.iter_raw_match_order() {
        match comp {
            Component::Combinator(_) => break,
            Component::ID(v) => id = Some(v.0.clone()),
            Component::Root => root = true,
            Component::Class(v) => class = Some(v.0.clone()),
            Component::LocalName(l) => local = Some(l.lower_name.0.to_string()),
            Component::AttributeInNoNamespaceExists {
                local_name_lower, ..
            } => attribute = Some(local_name_lower.0.to_string()),
            Component::AttributeInNoNamespace { local_name, .. } => {
                attribute = Some(local_name.0.to_string())
            }
            Component::AttributeOther(selector) => {
                attribute = Some(selector.local_name_lower.0.to_string())
            }
            Component::Is(list) | Component::Where(list) => {
                let mut keys = Vec::new();
                let mut universal = false;
                for alternative in list.slice() {
                    let alternative_keys = subject_keys(alternative);
                    if alternative_keys
                        .iter()
                        .any(|key| matches!(key, SelectorKey::Universal))
                    {
                        universal = true;
                        break;
                    }
                    for key in alternative_keys {
                        push_unique_key(&mut keys, key);
                    }
                }
                if universal || keys.is_empty() {
                    keys.clear();
                    keys.push(SelectorKey::Universal);
                }
                disjoint_sets.push(keys);
            }
            _ => {}
        }
    }
    let direct = if root {
        SelectorKey::Root
    } else if let Some(i) = id {
        SelectorKey::Id(i)
    } else if let Some(c) = class {
        SelectorKey::Class(c)
    } else if let Some(a) = attribute {
        SelectorKey::Attribute(a)
    } else if let Some(l) = local {
        SelectorKey::Local(l)
    } else {
        SelectorKey::Universal
    };

    let direct_rank = key_rank(&direct);
    let mut best: Option<Vec<SelectorKey>> = None;
    for keys in disjoint_sets {
        if keys.iter().any(|key| key_rank(key) <= direct_rank) {
            continue;
        }
        let min_rank = keys.iter().map(key_rank).min().unwrap_or(0);
        let replaces = best.as_ref().is_none_or(|current| {
            let current_min = current.iter().map(key_rank).min().unwrap_or(0);
            min_rank > current_min || (min_rank == current_min && keys.len() < current.len())
        });
        if replaces {
            best = Some(keys);
        }
    }
    best.unwrap_or_else(|| vec![direct])
}

#[cfg(test)]
mod tests {
    use crate::diting_dom::tree_sink::parse_html;

    /// Test bridge to the private subject_keys computation.
    fn subject_keys_for_test(
        tree: &crate::diting_dom::tree::DomTree,
        selector: &str,
    ) -> Vec<super::SelectorKey> {
        tree.compile_rule_selector(selector)
            .expect("selector must compile")
            .candidate_keys()
            .to_vec()
    }

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
        // must MATCH on a snapshot with no live input (both pseudos are false).
        let tree = parse_html(r#"<div class="visually-hidden-focusable">Skip</div>"#);
        let hidden = tree
            .query_selector_all(".visually-hidden-focusable:not(:focus):not(:focus-within)")
            .unwrap();
        assert_eq!(hidden.len(), 1);
        assert_eq!(tree.query_selector_all(":focus-visible").unwrap().len(), 0);
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
    fn compiled_selector_keys_cover_matches() {
        use super::SelectorKey;
        let tree = parse_html(
            r#"<section class="scope">
                <div class="alpha control" id="save" data-kind="x"></div>
                <button class="beta control"></button>
                <input class="control" id="cancel">
                <div class="a"><span class="x"></span></div>
            </section>"#,
        );
        // :root keys to Root; disjoint :is arms expand to multiple buckets;
        // a less-selective outer compound keeps its single bucket.
        let root_keys: Vec<_> = tree.compile_rule_selector(":root").unwrap().candidate_keys().to_vec();
        assert_eq!(root_keys, vec![SelectorKey::Root]);
        assert_eq!(
            subject_keys_for_test(&tree, ":is(.IssueLabel, .Label)"),
            vec![SelectorKey::Class("IssueLabel".into()), SelectorKey::Class("Label".into())]
        );
        assert_eq!(
            subject_keys_for_test(&tree, ".control:is(button, #save)"),
            vec![SelectorKey::Class("control".into())]
        );

        // Every actual match carries at least one candidate bucket key.
        for selector in [":is(.alpha, .beta)", ":where(button, input)", ".scope span.x"] {
            let compiled = tree.compile_rule_selector(selector).unwrap();
            for matched in tree.query_selector_all(selector).unwrap() {
                let node = tree.get_node(matched).unwrap();
                let covered = compiled.candidate_keys().iter().any(|key| match key {
                    SelectorKey::Universal | SelectorKey::Root => true,
                    SelectorKey::Id(id) => node.get_attribute("id") == Some(id.as_str()),
                    SelectorKey::Class(c) => node
                        .get_attribute("class")
                        .is_some_and(|cls| cls.split_whitespace().any(|v| v == c)),
                    SelectorKey::Attribute(a) => node.get_attribute(a).is_some(),
                    SelectorKey::Local(l) => {
                        node.as_element().is_some_and(|e| e.local.as_ref() == l)
                    }
                });
                assert!(covered, "{selector} matched outside all buckets");
            }
        }
    }

    #[test]
    fn matcher_with_ancestor_filter_matches_compiled_selectors() {
        let tree = parse_html(
            r#"<div class="wrap"><code><span id="target" class="line">x</span></code></div>"#,
        );
        let target = tree.get_element_by_id("target").unwrap();
        let compiled = tree.compile_rule_selector(".wrap code .line").unwrap();
        let mut matcher = tree.matcher();
        // Push ancestors root→down so the bloom tracks the current path.
        for ancestor in tree.ancestors(target).into_iter().rev() {
            matcher.push_ancestor(&tree, ancestor);
        }
        assert!(matcher.matches(&tree, target, &compiled));

        // A sibling subtree must fail through the same matcher.
        let other = tree.compile_rule_selector(".nomatch code .line");
        assert!(other.is_some());

        // Single-element API works on detached subjects too.
        let detached = parse_html("<em id='d'>d</em>");
        let d = detached.get_element_by_id("d").unwrap();
        assert!(detached.matches_selector(d, "em#d").unwrap());
        assert!(!detached.matches_selector(d, "div em").unwrap());
    }

    #[test]
    fn candidate_collection_dedupes_across_generations() {
        let tree = parse_html("<main></main>");
        let mut matcher = tree.matcher();

        matcher.begin_candidate_collection(4);
        assert!(matcher.mark_candidate(2));
        assert!(!matcher.mark_candidate(2));

        matcher.begin_candidate_collection(4);
        assert!(matcher.mark_candidate(2), "new generation resets seen");
        assert!(!matcher.mark_candidate(2));
    }
}
