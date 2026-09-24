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
    Root,
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
            PseudoClass::Root => dest.write_str(":root"),
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
            "root" => Ok(PseudoClass::Root),
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
            // :root = the document element — the one element with no element
            // parent. Automation frameworks probe `:root` as an
            // is-this-document-alive sentinel (Playwright's waitForSelector
            // in a frame), so parse-fail here reads as a dead document.
            PseudoClass::Root => self.parent_element().is_none(),
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
        let index = self.compile_rule_index(rule_selectors);
        let mut hits = HashMap::new();
        let mut pseudo_hits = HashMap::new();
        self.probe_elements_into(&index, probe_ids, &mut hits, &mut pseudo_hits);
        RuleMatchSets {
            hits,
            specificity: index.specificity,
            pseudo_kinds: index.pseudo_kinds,
            pseudo_hits,
        }
    }

    /// Selector-side compilation shared by the full and incremental faces:
    /// parsed selector lists, specificities, pseudo routing, and the
    /// rightmost-compound buckets. Cheap to rebuild (one parse per rule) but
    /// far from free on real sheets (~7ms for 1.7MB), so the incremental
    /// path parks it in [`MatchCacheBox`] keyed by the css bytes.
    fn compile_rule_index(&self, rule_selectors: &[&str]) -> RuleIndex {
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
        RuleIndex {
            entries,
            specificity,
            by_id,
            by_class,
            by_tag,
            unkeyed,
            pseudo_kinds,
            quirks,
        }
    }

    /// Probe elements against the compiled index, appending matches to the
    /// (possibly pre-populated, possibly purge-filtered) hit maps. Shared by
    /// the full build and the incremental sync so the two cannot drift.
    fn probe_elements_into(
        &self,
        index: &RuleIndex,
        probe_ids: impl IntoIterator<Item = NodeId>,
        hits: &mut HashMap<usize, Vec<usize>>,
        pseudo_hits: &mut HashMap<usize, Vec<usize>>,
    ) {
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
            candidates.extend_from_slice(&index.unkeyed);
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
            probe(&index.by_tag, &local.to_ascii_lowercase());
            if let Some(id) = &id {
                probe(&index.by_id, id);
                if index.quirks {
                    probe(&index.by_id, &id.to_ascii_lowercase());
                }
            }
            if let Some(class) = &class {
                for c in class.split_whitespace() {
                    probe(&index.by_class, c);
                    if index.quirks {
                        probe(&index.by_class, &c.to_ascii_lowercase());
                    }
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
            let element = DomElement::new(self, desc_id);
            for ri in candidates.drain(..) {
                let Some(list) = index.entries[ri].as_ref() else { continue };
                if selectors::matching::matches_selector_list(list, &element, &mut context) {
                    if index.pseudo_kinds.contains_key(&ri) {
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
    }

    /// The full probe set: document descendants plus every shadow tree's,
    /// the same population `rule_match_sets` covers.
    fn match_probe_all_ids(&self) -> Vec<NodeId> {
        let mut probe_ids: Vec<NodeId> = self.descendants(self.document());
        for root in self.shadow_roots() {
            probe_ids.extend(self.descendants(root));
        }
        probe_ids
    }

    /// Whether `nid` is reachable from the document or a shadow root — the
    /// population a fresh full match would probe. Detached subtrees (removed
    /// but not freed, or awaiting re-insertion) must not contribute hits a
    /// full rebuild would not produce.
    fn match_probe_attached(&self, nid: NodeId) -> bool {
        let inner = self.borrow_inner();
        let mut current = Some(nid);
        for _ in 0..=inner.nodes.len() {
            let Some(c) = current else { return false };
            if c == inner.document || inner.shadow_roots.contains_key(&c) {
                return true;
            }
            current = inner
                .nodes
                .get(c.index())
                .and_then(|n| n.as_ref())
                .and_then(|n| n.parent);
        }
        false
    }

    /// Incremental face for repeated layout runs against the same stylesheet
    /// (#111): the hit sets persist in the tree's [`MatchCacheBox`] keyed by
    /// the css bytes, and DOM mutations recorded in [`MatchDirty`] since the
    /// last sync only re-probe the elements they could have affected. The
    /// result is bit-for-bit what a fresh full match produces — the
    /// correctness argument is stamp completeness: an insert/re-parent only
    /// changes matches inside the moved subtree (ancestor chains) plus the
    /// two child lists it touches (sibling combinators, :nth-child) plus the
    /// parent chain (:has anchoring); an attribute write only changes the
    /// element, its subtree, its siblings, and its ancestor chain. Anything
    /// the registry cannot express falls back to a full rebuild.
    pub fn rule_match_sets_incremental(&self, rule_selectors: &[&str], css_key: u64) -> RuleMatchSets {
        let dirty = self.take_match_dirty();
        let mut cache = match self.take_match_cache(css_key) {
            Some(cache) => cache,
            None => {
                // First run, or the css changed: full build against fresh
                // buckets, then park the result for the next sync.
                let index = self.compile_rule_index(rule_selectors);
                let mut hits = HashMap::new();
                let mut pseudo_hits = HashMap::new();
                self.probe_elements_into(
                    &index,
                    self.match_probe_all_ids(),
                    &mut hits,
                    &mut pseudo_hits,
                );
                let sets = RuleMatchSets {
                    hits,
                    specificity: index.specificity.clone(),
                    pseudo_kinds: index.pseudo_kinds.clone(),
                    pseudo_hits,
                };
                self.set_match_cache(MatchCacheBox {
                    key: css_key,
                    index,
                    sets: sets.clone(),
                });
                return sets;
            }
        };

        let rebuild = |tree: &Self, cache: MatchCacheBox| -> RuleMatchSets {
            let mut hits = HashMap::new();
            let mut pseudo_hits = HashMap::new();
            tree.probe_elements_into(
                &cache.index,
                tree.match_probe_all_ids(),
                &mut hits,
                &mut pseudo_hits,
            );
            let sets = RuleMatchSets {
                hits,
                specificity: cache.index.specificity.clone(),
                pseudo_kinds: cache.index.pseudo_kinds.clone(),
                pseudo_hits,
            };
            tree.set_match_cache(MatchCacheBox { sets: sets.clone(), ..cache });
            sets
        };

        if dirty.full {
            return rebuild(self, cache);
        }

        let slots = self.node_slot_count();
        let mut seen = vec![false; slots];
        let mut mark = |nid: NodeId| {
            let i = nid.index();
            if i < slots && !seen[i] {
                seen[i] = true;
                true
            } else {
                false
            }
        };
        // Fast gate: the stamp count is a lower bound on the candidate
        // count (every root contributes at least itself), so a mass
        // mutation (initial parse, big innerHTML) skips candidate
        // resolution entirely and rebuilds — the pre-incremental cost.
        let stamp_count = dirty.roots.len()
            + dirty.purge_roots.len()
            + dirty.sibling_scopes.len();
        if stamp_count.saturating_mul(4) > slots {
            return rebuild(self, cache);
        }

        // Resolve the dirty stamps into a deduped candidate set. Slots is a
        // safe index ceiling: the arena never shrinks, and freed slots that
        // come back carry their own insert stamps.
        let mut candidates: Vec<NodeId> = Vec::new();
        for root in dirty.roots.iter().copied().chain(dirty.purge_roots.iter().copied()) {
            if mark(root) {
                candidates.push(root);
            }
            for d in self.descendants(root) {
                if mark(d) {
                    candidates.push(d);
                }
            }
        }
        for parent in &dirty.sibling_scopes {
            // The children re-probe wholesale; the parent chain re-probes
            // one element at a time (:has() anchoring).
            let mut chain = Some(*parent);
            for _ in 0..=slots {
                let Some(c) = chain else { break };
                if mark(c) {
                    candidates.push(c);
                }
                chain = self
                    .with_node(c, |n| n.parent)
                    .flatten();
            }
            for c in self.children(*parent) {
                if mark(c) {
                    candidates.push(c);
                }
            }
        }
        // A mass mutation (initial parse, big innerHTML) costs more to
        // resolve than a full rebuild — same as the pre-incremental world.
        if candidates.len() * 4 > slots {
            return rebuild(self, cache);
        }

        // Purge every candidate index from the hit sets, then re-probe the
        // attached ones. Detached subtree members stay purged (a full
        // rebuild probes only document+shadow descendants), and a freed
        // slot's stale hits die here too.
        for rule_hits in cache.sets.hits.values_mut().chain(cache.sets.pseudo_hits.values_mut()) {
            rule_hits.retain(|&i| !(i < slots && seen[i]));
        }
        // A fresh build only keys rules that matched at least one element;
        // drop the vectors the purge emptied so the shapes stay identical.
        cache.sets.hits.retain(|_, v| !v.is_empty());
        cache.sets.pseudo_hits.retain(|_, v| !v.is_empty());
        let attached: Vec<NodeId> = candidates
            .into_iter()
            .filter(|&c| self.match_probe_attached(c))
            .collect();
        self.probe_elements_into(
            &cache.index,
            attached,
            &mut cache.sets.hits,
            &mut cache.sets.pseudo_hits,
        );
        let sets = cache.sets.clone();
        self.set_match_cache(cache);
        sets
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// The compiled selector-side state a match run probes against: parsed
/// lists, specificities, pseudo routing, and the rightmost-compound
/// buckets (see [`DomTree::rule_match_sets`]). Lives in
/// [`MatchCacheBox`] across incremental runs keyed by the css bytes.
pub(crate) struct RuleIndex {
    entries: Vec<Option<SelectorList<DitingSelector>>>,
    specificity: Vec<Option<u32>>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    unkeyed: Vec<usize>,
    pseudo_kinds: HashMap<usize, PseudoKind>,
    quirks: bool,
}

/// The persisted match state for [`DomTree::rule_match_sets_incremental`]:
/// the compiled index and the live hit sets, valid as long as the css bytes
/// behind `key` are unchanged and every tree mutation since the last sync
/// carried a [`MatchDirty`] stamp.
pub(crate) struct MatchCacheBox {
    pub(crate) key: u64,
    index: RuleIndex,
    sets: RuleMatchSets,
}

/// Mutations the incremental matcher needs to know about, stamped by the
/// tree at every structural change and by `note_restyle` at attribute
/// writes. Consumed (emptied) by the next
/// [`DomTree::rule_match_sets_incremental`] sync.
#[derive(Default)]
pub(crate) struct MatchDirty {
    /// Roots of inserted, re-parented, or restyled subtrees: every element
    /// at and below re-probes.
    pub(crate) roots: Vec<NodeId>,
    /// Parents whose child list changed: their element children re-probe
    /// (sibling combinators and :nth-child shift), and each parent's
    /// ancestor chain re-probes individually (`:has()` anchoring — a
    /// subtree entering or leaving these descendants can flip a `:has()`
    /// compound up the chain). The chain walk happens at sync time, deduped
    /// across stamps, so a mutation stays O(1).
    pub(crate) sibling_scopes: Vec<NodeId>,
    /// Roots of detached subtrees: same treatment as `roots` — if the
    /// subtree was re-inserted elsewhere it also carries a `roots` stamp
    /// and re-probes; if it stayed detached the purge drops its hits; if
    /// the slot was freed and reused the new occupant carries its own
    /// insert stamp, so the purge never outlives its meaning.
    pub(crate) purge_roots: Vec<NodeId>,
    /// Set when a mutation path the registry cannot describe ran — the
    /// next sync rebuilds from scratch.
    pub(crate) full: bool,
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

