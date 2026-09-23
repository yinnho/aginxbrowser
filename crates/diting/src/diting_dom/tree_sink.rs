use std::borrow::Cow;
use std::cell::Ref;
use std::fmt;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{local_name, namespace_url, ns, Attribute as HtmlAttribute, LocalName, Namespace, QualName};

use crate::diting_dom::tree::{Attribute, DomTree, NodeData, NodeId, ShadowRootMode};

pub struct DitingElemName<'a> {
    _ref: Ref<'a, ()>,
    name: *const QualName,
}

impl<'a> fmt::Debug for DitingElemName<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = unsafe { &*self.name };
        write!(f, "{:?}", name)
    }
}

impl<'a> ElemName for DitingElemName<'a> {
    fn ns(&self) -> &Namespace {
        unsafe { &(*self.name).ns }
    }

    fn local_name(&self) -> &LocalName {
        unsafe { &(*self.name).local }
    }
}

impl TreeSink for DomTree {
    type Handle = NodeId;
    type Output = Self;
    type ElemName<'a> = DitingElemName<'a>;

    fn finish(self) -> Self::Output {
        self
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        self.document()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> DitingElemName<'a> {
        let borrow = self.borrow_inner();
        let node = borrow.nodes.get(target.index())
            .and_then(|n| n.as_ref())
            .expect("elem_name called on invalid node");
        let name_ptr: *const QualName = match &node.data {
            NodeData::Element { name, .. } => name as *const QualName,
            _ => panic!("elem_name called on non-element"),
        };
        let ref_guard = Ref::map(borrow, |_| &());
        DitingElemName {
            _ref: ref_guard,
            name: name_ptr,
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<HtmlAttribute>,
        flags: ElementFlags,
    ) -> NodeId {
        let converted_attrs: Vec<Attribute> = attrs
            .into_iter()
            .map(|a| Attribute {
                name: a.name,
                value: a.value.to_string(),
            })
            .collect();

        let id = self.new_node(NodeData::Element {
            name: name.clone(),
            attrs: converted_attrs,
            template_contents: None,
            mathml_annotation_xml_integration_point: flags.mathml_annotation_xml_integration_point,
            live_value: None,
            live_checked: None,
        });

        if flags.template {
            let template_doc = self.new_node(NodeData::Document);
            self.with_node_mut(id, |node| {
                if let NodeData::Element { template_contents, .. } = &mut node.data {
                    *template_contents = Some(template_doc);
                }
            });
        }

        id
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.new_node(NodeData::Comment {
            contents: text.to_string(),
        })
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.new_node(NodeData::ProcessingInstruction {
            target: target.to_string(),
            data: data.to_string(),
        })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node_id) => {
                self.append_child(*parent, node_id);
            }
            NodeOrText::AppendText(text) => {
                self.append_text(*parent, &text);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.with_node(*element, |n| n.parent.is_some()).unwrap_or(false);
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let doctype = self.new_node(NodeData::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
        let doc = self.document();
        self.append_child(doc, doctype);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<HtmlAttribute>) {
        self.with_node_mut(*target, |node| {
            if let NodeData::Element { attrs: existing, .. } = &mut node.data {
                for attr in attrs {
                    let dominated = existing.iter().any(|a| a.name == attr.name);
                    if !dominated {
                        existing.push(Attribute {
                            name: attr.name,
                            value: attr.value.to_string(),
                        });
                    }
                }
            }
        });
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let children = self.children(*node);
        for child_id in children {
            self.append_child(*new_parent, child_id);
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node_id) => {
                self.insert_before(*sibling, node_id);
            }
            NodeOrText::AppendText(text) => {
                let prev_text_id = {
                    let node = self.get_node(*sibling);
                    node.and_then(|n| n.prev_sibling).and_then(|prev_id| {
                        let prev = self.get_node(prev_id);
                        prev.and_then(|p| if p.is_text() { Some(prev_id) } else { None })
                    })
                };

                if let Some(prev_text_id) = prev_text_id {
                    self.with_node_mut(prev_text_id, |node| {
                        if let NodeData::Text { contents } = &mut node.data {
                            contents.push_str(&text);
                        }
                    });
                    return;
                }

                let text_id = self.new_node(NodeData::Text {
                    contents: text.to_string(),
                });
                self.insert_before(*sibling, text_id);
            }
        }
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.with_node(*target, |n| match &n.data {
            NodeData::Element { template_contents, .. } => *template_contents,
            _ => None,
        })
        .flatten()
        .expect("get_template_contents called on non-template element")
    }

    fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
        // html5ever 0.29.1's declarative-shadow path is a skeleton that
        // cannot be completed sink-side: on success nothing routes the
        // template's children into the shadow root, and closed mode is
        // never even detected (a "close" typo in its tree builder). Refuse
        // here so the template parses normally — content properly
        // contained in `template_contents` — and the post-parse walk
        // (`upgrade_declarative_shadow_roots`) performs the upgrade.
        false
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        // Only full quirks mode makes CSS class/id selectors case-insensitive;
        // limited-quirks behaves like no-quirks for selector matching.
        self.set_quirks(mode == QuirksMode::Quirks);
    }

    fn is_mathml_annotation_xml_integration_point(&self, target: &NodeId) -> bool {
        self.with_node(*target, |n| match &n.data {
            NodeData::Element { mathml_annotation_xml_integration_point, .. } => {
                *mathml_annotation_xml_integration_point
            }
            _ => false,
        })
        .unwrap_or(false)
    }
}

pub fn parse_html(html: &str) -> DomTree {
    use html5ever::tendril::TendrilSink;
    use html5ever::{parse_document, ParseOpts};

    let tree = DomTree::new();
    let tree = parse_document(tree, ParseOpts::default())
        .from_utf8()
        .one(html.as_bytes());
    upgrade_declarative_shadow_roots(&tree, tree.document());
    tree
}

pub fn parse_fragment(html: &str) -> DomTree {
    let context_name = QualName::new(None, ns!(html), local_name!("body"));
    parse_fragment_with_context(html, context_name)
}

/// Parse a fragment using the actual insertion element as html5ever's
/// context. Table/select content (`<tr>`, `<td>`, `<option>`…) only survives
/// parsing inside its proper ancestor context; a fixed `<body>` context drops
/// it. Parsing in an `<html>` context runs the "before head" insertion mode
/// and synthesizes both `<head>` and `<body>` (see `DomTree::fragment_root`).
pub fn parse_fragment_with_context(html: &str, context_name: QualName) -> DomTree {
    use html5ever::tendril::TendrilSink;
    use html5ever::{parse_fragment, ParseOpts};

    let tree = DomTree::new();
    let tree = parse_fragment(tree, ParseOpts::default(), context_name, vec![])
        .from_utf8()
        .one(html.as_bytes());
    upgrade_declarative_shadow_roots(&tree, tree.document());
    tree
}

/// Upgrade `<template shadowrootmode>` declarations into native shadow
/// roots (#87, the blitz#923 class). Runs after every parse: for each
/// template whose `shadowrootmode` names a real mode, attach its
/// `template_contents` fragment to the parent element as a shadow root
/// and drop the now-empty template shell — the template never exists in
/// the finished tree, exactly like the platform's parse-time behavior.
/// Failure cases mirror the spec: an invalid mode value or a host that
/// already has a shadow root leaves the template inert, and inert
/// template content is never itself walked.
fn upgrade_declarative_shadow_roots(tree: &DomTree, scope: NodeId) {
    // Iterative on purpose: a recursive walk overflows on the 20k-deep
    // nesting the parser accepts — the same trap text_content/outer_html
    // once hit (see tree's test_deep_nesting_does_not_overflow).
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        for child in tree.children(node) {
            let declarative = tree.with_node(child, |n| {
                let NodeData::Element { name, attrs, template_contents, .. } = &n.data else {
                    return None;
                };
                if name.local != local_name!("template") {
                    return None;
                }
                let mode = attrs
                    .iter()
                    .find(|a| a.name.local == local_name!("shadowrootmode"))?
                    .value
                    .trim();
                if mode.eq_ignore_ascii_case("open") {
                    Some((ShadowRootMode::Open, n.parent, *template_contents))
                } else if mode.eq_ignore_ascii_case("closed") {
                    Some((ShadowRootMode::Closed, n.parent, *template_contents))
                } else {
                    None
                }
            });
            if let Some((mode, Some(host), Some(contents))) = declarative.flatten() {
                let host_is_element =
                    tree.with_node(host, |n| n.is_element()).unwrap_or(false);
                if host_is_element
                    && tree.attach_shadow_root_node(host, contents, mode).is_ok()
                {
                    // The contents fragment is now a registered shadow root,
                    // so removing the shell cannot free it
                    // (inclusive_owned_subtrees only follows light children
                    // and registered host edges).
                    tree.remove(child);
                    // Shadow content can carry its own declarative
                    // templates.
                    stack.push(contents);
                    continue;
                }
            }
            stack.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let tree = parse_html("<html><head></head><body><h1>Hello</h1></body></html>");
        assert!(tree.len() > 3);
        let text = tree.text_content(tree.document());
        assert!(text.contains("Hello"));
    }

    #[test]
    fn test_parse_with_attributes() {
        let tree = parse_html(r#"<div id="main" class="container">Text</div>"#);
        let main = tree.get_element_by_id("main");
        assert!(main.is_some());
        let node = tree.get_node(main.unwrap()).unwrap();
        assert_eq!(node.get_attribute("class"), Some("container"));
    }

    #[test]
    fn test_parse_nested_structure() {
        let tree = parse_html(
            r#"<html><body>
                <div id="outer">
                    <p id="para">Hello <strong>World</strong></p>
                    <ul>
                        <li>Item 1</li>
                        <li>Item 2</li>
                    </ul>
                </div>
            </body></html>"#,
        );

        let outer = tree.get_element_by_id("outer").unwrap();
        let text = tree.text_content(outer);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(text.contains("Item 1"));
        assert!(text.contains("Item 2"));
    }

    #[test]
    fn test_parse_malformed_html() {
        let tree = parse_html("<div><p>Unclosed paragraph<p>Another<div>Nested wrong</div>");
        assert!(tree.len() > 3);
        let text = tree.text_content(tree.document());
        assert!(text.contains("Unclosed paragraph"));
        assert!(text.contains("Another"));
    }

    #[test]
    fn test_parse_doctype() {
        let tree = parse_html("<!DOCTYPE html><html><body>Hello</body></html>");
        let first_child = tree.children(tree.document())[0];
        let node = tree.get_node(first_child).unwrap();
        assert!(matches!(node.data, NodeData::Doctype { .. }));
    }

    #[test]
    fn test_parse_fragment() {
        let tree = parse_fragment("<p>Hello</p><p>World</p>");
        let text = tree.text_content(tree.document());
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    fn tag_of(tree: &DomTree, id: NodeId) -> String {
        tree.with_node(id, |n| match &n.data {
            NodeData::Element { name, .. } => name.local.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
    }

    #[test]
    fn declarative_shadow_open_moves_content_into_shadow_root() {
        let tree = parse_html(
            r#"<body><div id="host"><template shadowrootmode="open"><span>inner</span></template></div></body>"#,
        );
        let host = tree.get_element_by_id("host").unwrap();
        // The shell is gone: the host's light tree is empty.
        assert!(tree.children(host).is_empty());
        let root = tree.shadow_root(host).expect("shadow root attached");
        assert_eq!(tree.shadow_root_info(root).unwrap().mode, ShadowRootMode::Open);
        let kids = tree.children(root);
        assert_eq!(kids.len(), 1);
        assert_eq!(tag_of(&tree, kids[0]), "span");
        assert_eq!(tree.text_content(kids[0]), "inner");
    }

    #[test]
    fn declarative_shadow_closed_mode() {
        // Closed mode is the one html5ever's own detector cannot see (its
        // "close" typo) — the fixup must catch it regardless.
        let tree = parse_html(
            r#"<body><div id="h"><template shadowrootmode="closed"><p>x</p></template></div></body>"#,
        );
        let host = tree.get_element_by_id("h").unwrap();
        assert!(tree.children(host).is_empty());
        let root = tree.shadow_root(host).expect("closed root attached");
        assert_eq!(tree.shadow_root_info(root).unwrap().mode, ShadowRootMode::Closed);
        assert_eq!(tree.text_content(root), "x");
    }

    #[test]
    fn declarative_shadow_invalid_mode_stays_inert() {
        let tree = parse_html(
            r#"<body><div id="h"><template shadowrootmode="oopen"><span>no</span></template></div></body>"#,
        );
        let host = tree.get_element_by_id("h").unwrap();
        assert!(tree.shadow_root(host).is_none());
        let kids = tree.children(host);
        assert_eq!(kids.len(), 1);
        assert_eq!(tag_of(&tree, kids[0]), "template");
        // Inert means invisible: the text stays in the contents fragment,
        // out of the light DOM a render walk (or textContent) would see.
        assert_eq!(tree.text_content(host), "");
    }

    #[test]
    fn declarative_shadow_nested_inside_shadow_content() {
        let tree = parse_html(
            r#"<body><div id="outer"><template shadowrootmode="open"><section><template shadowrootmode="closed"><b>deep</b></template></section></template></div></body>"#,
        );
        let outer = tree.get_element_by_id("outer").unwrap();
        let outer_root = tree.shadow_root(outer).expect("outer root");
        let kids = tree.children(outer_root);
        assert_eq!(tag_of(&tree, kids[0]), "section");
        let inner_root = tree.shadow_root(kids[0]).expect("nested root");
        assert_eq!(tree.shadow_root_info(inner_root).unwrap().mode, ShadowRootMode::Closed);
        assert_eq!(tree.text_content(inner_root), "deep");
    }

    #[test]
    fn declarative_shadow_fragment_parse() {
        let tree = parse_fragment(r#"<div id="f"><template shadowrootmode="open"><span>frag</span></template></div>"#);
        let host = tree.get_element_by_id("f").unwrap();
        assert!(tree.children(host).is_empty());
        let root = tree.shadow_root(host).expect("fragment shadow root");
        assert_eq!(tree.text_content(root), "frag");
    }
}
